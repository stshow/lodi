//! A bounded, deterministic Debian dependency closure (LD-17).
//!
//! This is not a general SAT solver. It follows apt's default choices for the cases the spike
//! needs and refuses the rest explicitly:
//!
//! - Every package of the base rootfs is installed at its base version. A base package is
//!   upgraded only when a dependency needs a version the base does not have; the base's own
//!   dependencies are not re-verified (the base is a consistent installation).
//! - A dependency selects the highest version across all pinned indexes that satisfies its
//!   relation (for an unversioned dependency that is apt's default candidate); ties go to the
//!   repository listed later.
//! - `Pre-Depends` and `Depends` are followed; `Recommends` and `Suggests` are not
//!   (`install_recommends = false`).
//! - A dependency already satisfied by an installed or selected package (real, or a virtual
//!   name it provides) adds nothing. Otherwise the alternatives of `a | b` are tried in order
//!   and the first installable one is selected.
//! - A virtual name with exactly one provider selects it; several providers are an error that
//!   names them (the manifest must list one explicitly).
//! - `:any` and `:native` qualifiers are accepted (single-architecture closure); any other
//!   architecture qualifier is refused.
//! - After the closure is complete, every selected package's dependencies are checked again
//!   and `Conflicts`/`Breaks` between packages in the result are an error, never solved around.
//! - One resolution is bounded like the Arch resolver's: the number of requested names, the
//!   size of the closure and the number of dependency groups taken off the queue each have a
//!   cap, and passing one is an error that names it, never a hang (M-1.0 u-4, LD-363).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::control::Dependency;
use super::index::BinaryPackage;
use super::version::DebVersion;

/// Where a package of the closure comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Origin {
    /// Already in the base rootfs at this version.
    Base,
    /// Downloaded from the pinned repositories during realization.
    Install,
}

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Base => "base",
            Origin::Install => "install",
        }
    }
}

/// One package of the resolved closure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosurePackage {
    pub name: String,
    pub version: String,
    /// Debian architecture (`amd64`, `all`); `None` for a base package absent from the index.
    pub arch: Option<String>,
    pub origin: Origin,
    /// Index data when the exact version is in a pinned index.
    pub artifact: Option<Artifact>,
    /// An rpm build's epoch and release (LD-435); `None` for the apt and pacman families.
    pub epoch: Option<u64>,
    pub release: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Artifact {
    pub filename: String,
    pub sha256: String,
    pub size: u64,
    pub repository: String,
    pub suite: String,
}

/// Production bounds for one resolution (the Arch resolver's, `crate::arch::resolve::LIMITS`).
pub const LIMITS: Limits = Limits {
    max_requests: 10_000,
    max_closure_size: 100_000,
    max_iterations: 1_000_000,
};

/// Resource bounds exposed so each failure can be exercised without production-sized input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_requests: usize,
    pub max_closure_size: usize,
    pub max_iterations: usize,
}

/// Why a closure could not be computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolveError {
    /// One of the [`Limits`] was passed: which one, and its value.
    Limit { what: &'static str, limit: usize },
    /// A requested name is neither a package nor a virtual name in the pinned indexes.
    NotFound {
        name: String,
        suggestions: Vec<String>,
    },
    /// No package satisfies a dependency (or a version relation cannot be met).
    Unsatisfiable {
        dependency: String,
        required_by: String,
        available: Vec<String>,
    },
    /// A virtual name has several providers and nothing chose one.
    AmbiguousVirtual {
        name: String,
        required_by: String,
        providers: Vec<String>,
    },
    /// A dependency names another architecture (`foo:i386`).
    ForeignArch {
        dependency: String,
        required_by: String,
    },
    /// Two packages of the result conflict or break each other.
    Conflict {
        package: String,
        relation: String,
        other: String,
    },
}

/// The closure of `requested` on top of `base` over the pinned `index`.
pub fn resolve(
    index: &[BinaryPackage],
    base: &[(String, DebVersion)],
    requested: &[String],
) -> Result<Vec<ClosurePackage>, ResolveError> {
    resolve_with_limits(index, base, requested, LIMITS)
}

/// The same resolver with caller-supplied bounds for deterministic resource-limit tests.
pub fn resolve_with_limits(
    index: &[BinaryPackage],
    base: &[(String, DebVersion)],
    requested: &[String],
    limits: Limits,
) -> Result<Vec<ClosurePackage>, ResolveError> {
    Resolver::new(index, limits).run(base, requested)
}

struct Selected<'a> {
    version: DebVersion,
    origin: Origin,
    /// The index entry for this exact version, if there is one.
    entry: Option<&'a BinaryPackage>,
    /// For a base package absent from the index: the candidate entry, used only for what the
    /// package provides (an approximation recorded in LD-17).
    provides_from: Option<&'a BinaryPackage>,
}

struct Resolver<'a> {
    /// Real packages by name, highest version first (later repository first on ties).
    by_name: BTreeMap<&'a str, Vec<&'a BinaryPackage>>,
    /// Virtual name -> providing entries.
    providers: BTreeMap<&'a str, Vec<&'a BinaryPackage>>,
    selected: BTreeMap<String, Selected<'a>>,
    limits: Limits,
}

impl<'a> Resolver<'a> {
    fn new(index: &'a [BinaryPackage], limits: Limits) -> Resolver<'a> {
        let mut by_name: BTreeMap<&str, Vec<(usize, &BinaryPackage)>> = BTreeMap::new();
        let mut providers: BTreeMap<&str, Vec<&BinaryPackage>> = BTreeMap::new();
        for (position, p) in index.iter().enumerate() {
            by_name.entry(&p.name).or_default().push((position, p));
            for (virtual_name, _) in &p.provides {
                providers.entry(virtual_name).or_default().push(p);
            }
        }
        let by_name = by_name
            .into_iter()
            .map(|(name, mut entries)| {
                entries.sort_by(|(pa, a), (pb, b)| b.version.cmp(&a.version).then(pb.cmp(pa)));
                (name, entries.into_iter().map(|(_, p)| p).collect())
            })
            .collect();
        Resolver {
            by_name,
            providers,
            selected: BTreeMap::new(),
            limits,
        }
    }

    fn limit(what: &'static str, limit: usize) -> ResolveError {
        ResolveError::Limit { what, limit }
    }

    fn run(
        mut self,
        base: &[(String, DebVersion)],
        requested: &[String],
    ) -> Result<Vec<ClosurePackage>, ResolveError> {
        if requested.len() > self.limits.max_requests {
            return Err(Self::limit("request count", self.limits.max_requests));
        }
        for (name, version) in base {
            let candidates = self.by_name.get(name.as_str());
            let entry = candidates.and_then(|c| c.iter().find(|p| p.version == *version).copied());
            let provides_from = candidates.and_then(|c| c.first().copied());
            self.selected.insert(
                name.clone(),
                Selected {
                    version: version.clone(),
                    origin: Origin::Base,
                    entry,
                    provides_from,
                },
            );
        }

        if self.selected.len() > self.limits.max_closure_size {
            return Err(Self::limit("closure size", self.limits.max_closure_size));
        }

        let mut queue: VecDeque<(Vec<Dependency>, String)> = VecDeque::new();
        for name in requested {
            self.check_requested(name)?;
            let dep = Dependency {
                name: name.clone(),
                arch_qualifier: None,
                version: None,
            };
            queue.push_back((vec![dep], "the manifest".to_string()));
        }
        let mut iterations = 0usize;
        while let Some((group, required_by)) = queue.pop_front() {
            // A version one dependency needs can be replaced by one another needs and back
            // again; the iteration cap is what ends such a cycle.
            iterations += 1;
            if iterations > self.limits.max_iterations {
                return Err(Self::limit("iteration count", self.limits.max_iterations));
            }
            // A package of another architecture never satisfies `foo:i386`; refuse it before
            // the amd64 `foo` could be taken for it.
            for dep in &group {
                Self::check_arch(dep, &required_by)?;
            }
            if group.iter().any(|d| self.satisfied(d)) {
                continue;
            }
            let chosen = self.choose(&group, &required_by)?;
            if !self.selected.contains_key(&chosen.name)
                && self.selected.len() >= self.limits.max_closure_size
            {
                return Err(Self::limit("closure size", self.limits.max_closure_size));
            }
            let origin_required = format!("{} {}", chosen.name, chosen.version);
            self.selected.insert(
                chosen.name.clone(),
                Selected {
                    version: chosen.version.clone(),
                    origin: Origin::Install,
                    entry: Some(chosen),
                    provides_from: Some(chosen),
                },
            );
            for group in &chosen.depends {
                queue.push_back((group.clone(), origin_required.clone()));
            }
        }
        self.verify()?;
        Ok(self.closure())
    }

    fn check_requested(&self, name: &str) -> Result<(), ResolveError> {
        if self.by_name.contains_key(name) || self.providers.contains_key(name) {
            return Ok(());
        }
        let mut near: Vec<(usize, &str)> = self
            .by_name
            .keys()
            .map(|k| (crate::manifest::distance(name, k), *k))
            .filter(|(d, _)| *d <= name.len().max(3) / 2 + 1)
            .collect();
        near.sort();
        Err(ResolveError::NotFound {
            name: name.to_string(),
            suggestions: near
                .into_iter()
                .take(3)
                .map(|(_, k)| k.to_string())
                .collect(),
        })
    }

    fn check_arch(dep: &Dependency, required_by: &str) -> Result<(), ResolveError> {
        match dep.arch_qualifier.as_deref() {
            None | Some("any") | Some("native") => Ok(()),
            Some(_) => Err(ResolveError::ForeignArch {
                dependency: dep.to_string(),
                required_by: required_by.to_string(),
            }),
        }
    }

    fn version_ok(dep: &Dependency, version: &DebVersion) -> bool {
        dep.version
            .as_ref()
            .is_none_or(|(relation, bound)| relation.holds(version, bound))
    }

    /// Whether the installed or selected set already satisfies `dep`.
    fn satisfied(&self, dep: &Dependency) -> bool {
        if let Some(s) = self.selected.get(&dep.name)
            && Self::version_ok(dep, &s.version)
        {
            return true;
        }
        self.selected.values().any(|s| {
            s.provides_from.is_some_and(|p| {
                p.provides.iter().any(|(name, version)| {
                    *name == dep.name
                        && match (&dep.version, version) {
                            (None, _) => true,
                            (Some((relation, bound)), Some(v)) => relation.holds(v, bound),
                            (Some(_), None) => false,
                        }
                })
            })
        })
    }

    /// The package to install for an unsatisfied alternative group.
    fn choose(
        &self,
        group: &[Dependency],
        required_by: &str,
    ) -> Result<&'a BinaryPackage, ResolveError> {
        let mut available = Vec::new();
        for dep in group {
            Self::check_arch(dep, required_by)?;
            if let Some(entries) = self.by_name.get(dep.name.as_str()) {
                if let Some(p) = entries.iter().find(|p| Self::version_ok(dep, &p.version)) {
                    return Ok(p);
                }
                available.extend(entries.iter().map(|p| format!("{} {}", p.name, p.version)));
            }
            let providers: Vec<&BinaryPackage> = self
                .providers
                .get(dep.name.as_str())
                .map(|entries| {
                    entries
                        .iter()
                        .filter(|p| {
                            p.provides.iter().any(|(name, version)| {
                                *name == dep.name
                                    && match (&dep.version, version) {
                                        (None, _) => true,
                                        (Some((relation, bound)), Some(v)) => {
                                            relation.holds(v, bound)
                                        }
                                        (Some(_), None) => false,
                                    }
                            })
                        })
                        .copied()
                        .collect()
                })
                .unwrap_or_default();
            let names: BTreeSet<&str> = providers.iter().map(|p| p.name.as_str()).collect();
            match names.len() {
                0 => {}
                1 => {
                    let name = *names.first().unwrap_or(&"");
                    if let Some(p) = self.by_name.get(name).and_then(|e| e.first()) {
                        return Ok(p);
                    }
                }
                _ if group.len() == 1 => {
                    return Err(ResolveError::AmbiguousVirtual {
                        name: dep.name.clone(),
                        required_by: required_by.to_string(),
                        providers: names.into_iter().map(str::to_string).collect(),
                    });
                }
                _ => {}
            }
        }
        let dependency = group
            .iter()
            .map(|d| d.to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        Err(ResolveError::Unsatisfiable {
            dependency,
            required_by: required_by.to_string(),
            available,
        })
    }

    /// Re-check every installed package's dependencies and the conflicts of the result.
    fn verify(&self) -> Result<(), ResolveError> {
        for (name, s) in &self.selected {
            let Some(entry) = s.entry else { continue };
            if s.origin == Origin::Install {
                for group in &entry.depends {
                    if !group.iter().any(|d| self.satisfied(d)) {
                        return Err(ResolveError::Unsatisfiable {
                            dependency: group
                                .iter()
                                .map(|d| d.to_string())
                                .collect::<Vec<_>>()
                                .join(" | "),
                            required_by: format!("{name} {}", s.version),
                            available: Vec::new(),
                        });
                    }
                }
            }
            for (relation, groups) in [
                ("conflicts with", &entry.conflicts),
                ("breaks", &entry.breaks),
            ] {
                for dep in groups.iter().flatten() {
                    if let Some(other) = self.violates(name, dep) {
                        return Err(ResolveError::Conflict {
                            package: format!("{name} {}", s.version),
                            relation: format!("{relation} {dep}"),
                            other,
                        });
                    }
                }
            }
        }
        Ok(())
    }

    /// Another package of the result that matches the negative relation `dep` of `owner`.
    fn violates(&self, owner: &str, dep: &Dependency) -> Option<String> {
        if dep.name != owner
            && let Some(s) = self.selected.get(&dep.name)
            && Self::version_ok(dep, &s.version)
        {
            return Some(format!("{} {}", dep.name, s.version));
        }
        if dep.version.is_some() {
            return None;
        }
        self.selected.iter().find_map(|(name, s)| {
            let provides = s
                .provides_from
                .is_some_and(|p| p.provides.iter().any(|(v, _)| *v == dep.name));
            (name != owner && provides)
                .then(|| format!("{name} {} (provides {})", s.version, dep.name))
        })
    }

    fn closure(&self) -> Vec<ClosurePackage> {
        self.selected
            .iter()
            .map(|(name, s)| ClosurePackage {
                name: name.clone(),
                version: s.version.as_str().to_string(),
                arch: s.entry.map(|e| e.arch.clone()),
                origin: s.origin,
                artifact: s.entry.map(|e| Artifact {
                    filename: e.filename.clone(),
                    sha256: e.sha256.clone(),
                    size: e.size,
                    repository: e.repository.clone(),
                    suite: e.suite.clone(),
                }),
                epoch: None,
                release: None,
            })
            .collect()
    }
}
