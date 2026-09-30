//! The rpm closure of a request, resolved the way dnf5 (libsolv) resolves it (LD-435).
//!
//! What is installed already satisfies what it provides and is never replaced. Every package the
//! closure holds — installed ones included, because a new package can switch on an installed
//! package's conditional dependency — has its `Requires` met. The rules, each measured against
//! dnf5 on the Fedora 44 base (`docs/milestones/m-fedora/records/fd-1-fedora-develop.md`):
//!
//! - a requirement is met by any package of the closure whose provides, or whose file list for a
//!   path, overlaps it by rpm's rules ([`overlaps`]); `rpmlib(...)` is rpm's own;
//! - otherwise the providers are pooled, each name keeps its best build (highest EVR, then
//!   `x86_64` before `noarch`), and the first name in byte order is chosen: libsolv's
//!   `prune_to_best_version`, which is also how `(A or B)` picks between two uninstalled names;
//! - `(A and B)` needs both, `(A with B)` one package that provides both, `(A without B)` one
//!   that provides A and not B; `(A if B [else C])` and `(A unless B [else C])` are decided
//!   against the whole closure and re-decided until nothing changes, so a condition switched on
//!   by a later choice is honoured.
//!
//! Only `x86_64` and `noarch` builds are candidates. Weak dependencies (`Recommends`,
//! `Supplements`) are never followed, as a lodi transaction keeps them off. `Conflicts` and
//! `Obsoletes` are not read: the image build's own `rpm` transaction refuses a conflicting set,
//! and the read-back refuses anything the lock does not name.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::metadata::{Dep, Evr, Flags, RpmPackage, overlaps};
use crate::diag::Diagnostic;

/// Upper bounds of one resolution.
const MAX_CLOSURE: usize = 20_000;
const MAX_ROUNDS: usize = 10_000;

/// One package of the closure, and whether the base image already holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
    pub package: &'a RpmPackage,
    pub installed: bool,
}

/// A rich dependency (rpm's boolean dependencies).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Rich {
    Atom(Dep),
    And(Vec<Rich>),
    Or(Vec<Rich>),
    With(Vec<Rich>),
    Without(Box<Rich>, Box<Rich>),
    If(Box<Rich>, Box<Rich>, Option<Box<Rich>>),
    Unless(Box<Rich>, Box<Rich>, Option<Box<Rich>>),
}

impl Rich {
    /// Parse a requirement's text: a rich dependency when it starts with `(`, else a plain one.
    pub fn parse(dep: &Dep) -> Result<Rich, String> {
        if !dep.name.starts_with('(') {
            return Ok(Rich::Atom(dep.clone()));
        }
        let tokens = tokenize(&dep.name)?;
        let mut at = 0;
        let rich = group(&tokens, &mut at)?;
        if at != tokens.len() {
            return Err(format!("trailing text in `{}`", dep.name));
        }
        Ok(rich)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Open,
    Close,
    Word(String),
}

fn tokenize(text: &str) -> Result<Vec<Token>, String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            c if c.is_whitespace() => i += 1,
            '(' => {
                out.push(Token::Open);
                i += 1;
            }
            ')' => {
                out.push(Token::Close);
                i += 1;
            }
            _ => {
                // A name may carry balanced parentheses of its own: `python3dist(sip)`.
                let start = i;
                let mut depth = 0usize;
                while i < chars.len() && !chars[i].is_whitespace() {
                    match chars[i] {
                        '(' => depth += 1,
                        ')' if depth == 0 => break,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    i += 1;
                }
                out.push(Token::Word(chars[start..i].iter().collect()));
            }
        }
    }
    Ok(out)
}

const OPERATORS: &[&str] = &["and", "or", "with", "without", "if", "unless", "else"];

fn group(tokens: &[Token], at: &mut usize) -> Result<Rich, String> {
    if tokens.get(*at) != Some(&Token::Open) {
        return Err("a rich dependency must start with `(`".into());
    }
    *at += 1;
    let mut operands = vec![operand(tokens, at)?];
    let mut operators: Vec<String> = Vec::new();
    loop {
        match tokens.get(*at) {
            Some(Token::Close) => {
                *at += 1;
                break;
            }
            Some(Token::Word(w)) if OPERATORS.contains(&w.as_str()) => {
                operators.push(w.clone());
                *at += 1;
                operands.push(operand(tokens, at)?);
            }
            other => return Err(format!("unexpected {other:?} in a rich dependency")),
        }
    }
    let ops: Vec<&str> = operators.iter().map(String::as_str).collect();
    let mut operands = operands.into_iter();
    let mut next = || Box::new(operands.next().expect("one operand per operator plus one"));
    Ok(match ops.as_slice() {
        [] => *next(),
        [first, ..]
            if matches!(*first, "and" | "or" | "with") && ops.iter().all(|o| o == first) =>
        {
            let list: Vec<Rich> = std::iter::from_fn(|| Some(*next()))
                .take(ops.len() + 1)
                .collect();
            match *first {
                "and" => Rich::And(list),
                "or" => Rich::Or(list),
                _ => Rich::With(list),
            }
        }
        ["without"] => Rich::Without(next(), next()),
        ["if"] => Rich::If(next(), next(), None),
        ["if", "else"] => Rich::If(next(), next(), Some(next())),
        ["unless"] => Rich::Unless(next(), next(), None),
        ["unless", "else"] => Rich::Unless(next(), next(), Some(next())),
        _ => {
            return Err(format!(
                "unsupported operators {ops:?} in a rich dependency"
            ));
        }
    })
}

fn operand(tokens: &[Token], at: &mut usize) -> Result<Rich, String> {
    match tokens.get(*at) {
        Some(Token::Open) => group(tokens, at),
        Some(Token::Word(name)) if !OPERATORS.contains(&name.as_str()) => {
            *at += 1;
            let mut dep = Dep {
                name: name.clone(),
                flags: None,
                evr: None,
            };
            if let Some(Token::Word(op)) = tokens.get(*at)
                && let Some(flags) = Flags::parse_operator(op)
            {
                let Some(Token::Word(evr)) = tokens.get(*at + 1) else {
                    return Err(format!("`{name} {op}` without a version"));
                };
                dep.flags = Some(flags);
                dep.evr = Some(Evr::parse(evr).ok_or_else(|| format!("bad version `{evr}`"))?);
                *at += 2;
            }
            Ok(Rich::Atom(dep))
        }
        other => Err(format!("unexpected {other:?} in a rich dependency")),
    }
}

/// Provide name (or file path) -> (package, flags, EVR).
type Provides<'a> = BTreeMap<&'a str, Vec<(usize, Option<Flags>, Option<&'a Evr>)>>;

struct Resolver<'a> {
    packages: Vec<&'a RpmPackage>,
    by_name: BTreeMap<&'a str, Vec<usize>>,
    provides: Provides<'a>,
    selected: BTreeSet<usize>,
    selected_names: BTreeMap<&'a str, usize>,
    installed: BTreeSet<usize>,
    queue: VecDeque<usize>,
    conditionals: Vec<(usize, Rich)>,
}

fn unsatisfiable(what: &str, by: &RpmPackage) -> Diagnostic {
    Diagnostic::new(
        "E_NO_MATCH",
        format!("no package satisfies `{what}` (required by {})", by.nevra()),
    )
}

fn describe(rich: &Rich) -> String {
    match rich {
        Rich::Atom(d) => match (&d.flags, &d.evr) {
            (Some(f), Some(e)) => format!(
                "{} {f:?} {}:{}{}",
                d.name,
                e.epoch,
                e.version,
                e.release
                    .as_deref()
                    .map(|r| format!("-{r}"))
                    .unwrap_or_default()
            ),
            _ => d.name.clone(),
        },
        other => format!("{other:?}"),
    }
}

impl<'a> Resolver<'a> {
    fn new(index: &'a [RpmPackage]) -> Self {
        let packages: Vec<&RpmPackage> = index
            .iter()
            .filter(|p| p.arch == "x86_64" || p.arch == "noarch")
            .collect();
        let mut by_name: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
        let mut provides: Provides = BTreeMap::new();
        for (i, p) in packages.iter().enumerate() {
            by_name.entry(p.name.as_str()).or_default().push(i);
            for d in &p.provides {
                provides
                    .entry(d.name.as_str())
                    .or_default()
                    .push((i, d.flags, d.evr.as_ref()));
            }
            for f in &p.files {
                provides
                    .entry(f.as_str())
                    .or_default()
                    .push((i, None, None));
            }
        }
        Resolver {
            packages,
            by_name,
            provides,
            selected: BTreeSet::new(),
            selected_names: BTreeMap::new(),
            installed: BTreeSet::new(),
            queue: VecDeque::new(),
            conditionals: Vec::new(),
        }
    }

    fn providers(&self, dep: &Dep) -> Vec<usize> {
        let mut out: Vec<usize> = self
            .provides
            .get(dep.name.as_str())
            .into_iter()
            .flatten()
            .filter(|(_, f, e)| overlaps((*f, *e), (dep.flags, dep.evr.as_ref())))
            .map(|(i, _, _)| *i)
            .collect();
        out.sort_unstable();
        out.dedup();
        out
    }

    fn select(&mut self, i: usize, by: &RpmPackage) -> Result<(), Diagnostic> {
        let p = self.packages[i];
        if let Some(&other) = self.selected_names.get(p.name.as_str()) {
            if other == i {
                return Ok(());
            }
            return Err(Diagnostic::new(
                "E_NO_MATCH",
                format!(
                    "the closure needs both {} and {} (required by {})",
                    self.packages[other].nevra(),
                    p.nevra(),
                    by.nevra()
                ),
            ));
        }
        if self.selected.len() >= MAX_CLOSURE {
            return Err(Diagnostic::new(
                "E_NO_MATCH",
                format!("Fedora resolution exceeds the closure limit of {MAX_CLOSURE}"),
            ));
        }
        self.selected.insert(i);
        self.selected_names.insert(p.name.as_str(), i);
        self.queue.push_back(i);
        Ok(())
    }

    /// libsolv's choice among uninstalled candidates: each name's best build, then byte order.
    fn choose(&self, candidates: &[usize]) -> Option<usize> {
        let mut best: BTreeMap<&str, usize> = BTreeMap::new();
        for &i in candidates {
            let p = self.packages[i];
            // A name already in the closure at another build cannot be chosen twice.
            if let Some(&have) = self.selected_names.get(p.name.as_str())
                && have != i
            {
                continue;
            }
            best.entry(p.name.as_str())
                .and_modify(|b| {
                    let q = self.packages[*b];
                    let better = p
                        .evr
                        .compare(&q.evr)
                        .then_with(|| (p.arch == "x86_64").cmp(&(q.arch == "x86_64")));
                    if better.is_gt() {
                        *b = i;
                    }
                })
                .or_insert(i);
        }
        best.into_values().next()
    }

    fn satisfied(&self, rich: &Rich) -> bool {
        match rich {
            Rich::Atom(d) => {
                d.name.starts_with("rpmlib(")
                    || self.providers(d).iter().any(|i| self.selected.contains(i))
            }
            Rich::And(list) => list.iter().all(|r| self.satisfied(r)),
            Rich::Or(list) => list.iter().any(|r| self.satisfied(r)),
            Rich::With(list) => self.selected.iter().any(|i| self.all_provided_by(*i, list)),
            Rich::Without(a, b) => self.selected.iter().any(|i| {
                self.all_provided_by(*i, std::slice::from_ref(a))
                    && !self.all_provided_by(*i, std::slice::from_ref(b))
            }),
            Rich::If(a, b, c) => {
                if self.satisfied(b) {
                    self.satisfied(a)
                } else {
                    c.as_ref().is_none_or(|c| self.satisfied(c))
                }
            }
            Rich::Unless(a, b, c) => {
                if self.satisfied(b) {
                    c.as_ref().is_none_or(|c| self.satisfied(c))
                } else {
                    self.satisfied(a)
                }
            }
        }
    }

    fn all_provided_by(&self, i: usize, list: &[Rich]) -> bool {
        list.iter().all(|r| match r {
            Rich::Atom(d) => self.providers(d).contains(&i),
            _ => false,
        })
    }

    fn atoms_candidates(&self, list: &[Rich]) -> Option<Vec<usize>> {
        let mut sets = list.iter().map(|r| match r {
            Rich::Atom(d) => Some(self.providers(d)),
            _ => None,
        });
        let first = sets.next()??;
        let mut out: BTreeSet<usize> = first.into_iter().collect();
        for s in sets {
            let s: BTreeSet<usize> = s?.into_iter().collect();
            out = out.intersection(&s).copied().collect();
        }
        Some(out.into_iter().collect())
    }

    fn fulfil(&mut self, rich: &Rich, by: usize) -> Result<(), Diagnostic> {
        let by_pkg = self.packages[by];
        if matches!(rich, Rich::If(..) | Rich::Unless(..)) {
            self.conditionals.push((by, rich.clone()));
            return Ok(());
        }
        if self.satisfied(rich) {
            return Ok(());
        }
        match rich {
            Rich::Atom(d) => {
                let chosen = self
                    .choose(&self.providers(d))
                    .ok_or_else(|| unsatisfiable(&describe(rich), by_pkg))?;
                self.select(chosen, by_pkg)
            }
            Rich::And(list) => {
                for r in list {
                    self.fulfil(r, by)?;
                }
                Ok(())
            }
            Rich::Or(list) => {
                if list.iter().all(|r| matches!(r, Rich::Atom(_))) {
                    let pooled: Vec<usize> = list
                        .iter()
                        .flat_map(|r| match r {
                            Rich::Atom(d) => self.providers(d),
                            _ => Vec::new(),
                        })
                        .collect();
                    let chosen = self
                        .choose(&pooled)
                        .ok_or_else(|| unsatisfiable(&describe(rich), by_pkg))?;
                    return self.select(chosen, by_pkg);
                }
                for r in list {
                    let mut trial = self.snapshot();
                    if trial.fulfil_in(r, by).is_ok() {
                        return self.fulfil(r, by);
                    }
                }
                Err(unsatisfiable(&describe(rich), by_pkg))
            }
            Rich::With(list) => {
                let chosen = self
                    .atoms_candidates(list)
                    .and_then(|c| self.choose(&c))
                    .ok_or_else(|| unsatisfiable(&describe(rich), by_pkg))?;
                self.select(chosen, by_pkg)
            }
            Rich::Without(a, b) => {
                let (Rich::Atom(a), Rich::Atom(b)) = (a.as_ref(), b.as_ref()) else {
                    return Err(unsatisfiable(&describe(rich), by_pkg));
                };
                let not: BTreeSet<usize> = self.providers(b).into_iter().collect();
                let candidates: Vec<usize> = self
                    .providers(a)
                    .into_iter()
                    .filter(|i| !not.contains(i))
                    .collect();
                let chosen = self
                    .choose(&candidates)
                    .ok_or_else(|| unsatisfiable(&describe(rich), by_pkg))?;
                self.select(chosen, by_pkg)
            }
            Rich::If(..) | Rich::Unless(..) => unreachable!("deferred above"),
        }
    }

    /// A copy of the selection, to try an alternative without committing to it.
    fn snapshot(&self) -> Trial<'_, 'a> {
        Trial {
            resolver: self,
            added: BTreeSet::new(),
        }
    }

    fn requirements(&mut self, i: usize) -> Result<(), Diagnostic> {
        let p = self.packages[i];
        for dep in &p.requires {
            if dep.name.starts_with("rpmlib(") {
                continue;
            }
            let rich = Rich::parse(dep).map_err(|why| {
                Diagnostic::new(
                    "E_REPO_UNREACHABLE",
                    format!("{}: unreadable dependency `{}`: {why}", p.nevra(), dep.name),
                )
            })?;
            self.fulfil(&rich, i)?;
        }
        Ok(())
    }

    fn run(&mut self, base: &[String], requested: &[String]) -> Result<(), Diagnostic> {
        for nevra in base {
            let i = self
                .packages
                .iter()
                .position(|p| p.nevra() == *nevra)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E_RECIPE_CTX",
                        format!(
                            "the base image's build {nevra} is not in the pinned repository \
                             metadata"
                        ),
                    )
                })?;
            self.installed.insert(i);
            let p = self.packages[i];
            self.select(i, p)?;
        }
        for name in requested {
            let candidates = self.by_name.get(name.as_str()).cloned().unwrap_or_default();
            if candidates.is_empty() {
                return Err(Diagnostic::new(
                    "E_NO_MATCH",
                    format!("package `{name}` is not in the pinned Fedora repositories"),
                ));
            }
            if self.selected_names.contains_key(name.as_str()) {
                continue;
            }
            let chosen = self.choose(&candidates).expect("a name with candidates");
            let p = self.packages[chosen];
            self.select(chosen, p)?;
        }
        for _ in 0..MAX_ROUNDS {
            while let Some(i) = self.queue.pop_front() {
                self.requirements(i)?;
            }
            let pending = std::mem::take(&mut self.conditionals);
            let mut changed = false;
            for (by, rich) in &pending {
                let active = match rich {
                    Rich::If(a, b, c) => {
                        if self.satisfied(b) {
                            Some(a.as_ref())
                        } else {
                            c.as_deref()
                        }
                    }
                    Rich::Unless(a, b, c) => {
                        if self.satisfied(b) {
                            c.as_deref()
                        } else {
                            Some(a.as_ref())
                        }
                    }
                    other => Some(other),
                };
                if let Some(need) = active
                    && !self.satisfied(need)
                {
                    self.fulfil(need, *by)?;
                    changed = true;
                }
            }
            self.conditionals = pending;
            if !changed && self.queue.is_empty() {
                return Ok(());
            }
        }
        Err(Diagnostic::new(
            "E_NO_MATCH",
            format!("Fedora resolution did not settle within {MAX_ROUNDS} rounds"),
        ))
    }
}

/// An alternative tried against the selection without changing it: whether it can be met at
/// all, one level deep.
struct Trial<'r, 'a> {
    resolver: &'r Resolver<'a>,
    added: BTreeSet<usize>,
}

impl Trial<'_, '_> {
    fn fulfil_in(&mut self, rich: &Rich, _by: usize) -> Result<(), ()> {
        let r = self.resolver;
        match rich {
            Rich::Atom(d) => {
                let chosen = r.choose(&r.providers(d)).ok_or(())?;
                self.added.insert(chosen);
                Ok(())
            }
            Rich::And(list) | Rich::With(list) | Rich::Or(list) => {
                for x in list {
                    self.fulfil_in(x, _by)?;
                }
                Ok(())
            }
            Rich::Without(a, _) => self.fulfil_in(a, _by),
            Rich::If(..) | Rich::Unless(..) => Ok(()),
        }
    }
}

/// Resolve `requested` on top of the installed `base` builds (`NAME-EPOCH:VERSION-RELEASE.ARCH`)
/// against `index`, to the whole closure, sorted by name and architecture.
pub fn resolve<'a>(
    index: &'a [RpmPackage],
    base: &[String],
    requested: &[String],
) -> Result<Vec<Resolved<'a>>, Diagnostic> {
    let mut r = Resolver::new(index);
    r.run(base, requested)?;
    let mut out: Vec<Resolved> = r
        .selected
        .iter()
        .map(|i| Resolved {
            package: r.packages[*i],
            installed: r.installed.contains(i),
        })
        .collect();
    out.sort_by(|a, b| {
        (a.package.name.as_str(), a.package.arch.as_str())
            .cmp(&(b.package.name.as_str(), b.package.arch.as_str()))
    });
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn atom(name: &str) -> Rich {
        Rich::Atom(Dep {
            name: name.into(),
            flags: None,
            evr: None,
        })
    }

    fn rich(text: &str) -> Rich {
        Rich::parse(&Dep {
            name: text.into(),
            flags: None,
            evr: None,
        })
        .unwrap()
    }

    #[test]
    fn rich_dependencies_parse_as_rpm_writes_them() {
        assert_eq!(
            rich("(ntpsec or chrony)"),
            Rich::Or(vec![atom("ntpsec"), atom("chrony")])
        );
        assert_eq!(
            rich("(avahi if systemd)"),
            Rich::If(Box::new(atom("avahi")), Box::new(atom("systemd")), None)
        );
        let Rich::With(list) = rich("(python3.14dist(sip) < 7~~ with python3.14dist(sip) >= 6.7)")
        else {
            panic!("not a with");
        };
        let Rich::Atom(first) = &list[0] else {
            panic!("not an atom")
        };
        assert_eq!(first.name, "python3.14dist(sip)");
        assert_eq!(first.flags, Some(Flags::Lt));
        assert!(matches!(
            rich("(ansible-core or (ansible < 2.10.0 with ansible >= 2.9.10))"),
            Rich::Or(ref l) if matches!(l[1], Rich::With(_))
        ));
        assert!(matches!(rich("(a if b else c)"), Rich::If(_, _, Some(_))));
        assert!(
            Rich::parse(&Dep {
                name: "(a and b or c)".into(),
                flags: None,
                evr: None
            })
            .is_err()
        );
    }
}
