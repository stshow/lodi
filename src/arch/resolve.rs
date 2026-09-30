//! Bounded, deterministic dependency resolution for pacman repository metadata.
//!
//! This is deliberately separate from the apt resolver. It implements only the behaviours fixed
//! by M-Arch-Base design call D7: versioned provides, literal soname ABI provides, replacements,
//! group expansion and conflict refusal. Optional dependencies are absent from [`Package`] and
//! therefore can never enter the work queue.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use super::db::{Dependency, DependencyVersion, Package, Relation};
use super::version::PacmanVersion;
use crate::debian::resolve::{Artifact, ClosurePackage, Origin};
use crate::diag::Diagnostic;

/// Production bounds for one resolution.
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

/// The lock-facing result. `requested` is sorted and deduplicated; a group name is replaced by
/// its package members, while an ordinary request keeps the name the caller supplied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolution {
    pub requested: Vec<String>,
    pub closure: Vec<ClosurePackage>,
}

/// Resolve `requested` on top of the packages already present in the base rootfs.
pub fn resolve(
    index: &[Package],
    base: &[(String, PacmanVersion)],
    requested: &[String],
) -> Result<Resolution, Diagnostic> {
    resolve_with_limits(index, base, requested, LIMITS)
}

/// The same resolver with caller-supplied bounds for deterministic resource-limit tests.
pub fn resolve_with_limits(
    index: &[Package],
    base: &[(String, PacmanVersion)],
    requested: &[String],
    limits: Limits,
) -> Result<Resolution, Diagnostic> {
    Resolver::new(index, limits).run(base, requested)
}

#[derive(Clone)]
struct Selected<'a> {
    version: PacmanVersion,
    origin: Origin,
    entry: Option<&'a Package>,
}

struct Resolver<'a> {
    index: &'a [Package],
    by_name: BTreeMap<&'a str, Vec<&'a Package>>,
    providers: BTreeMap<&'a str, Vec<&'a Package>>,
    replacers: BTreeMap<&'a str, Vec<&'a Package>>,
    groups: BTreeMap<&'a str, Vec<&'a Package>>,
    selected: BTreeMap<String, Selected<'a>>,
    limits: Limits,
}

impl<'a> Resolver<'a> {
    fn new(index: &'a [Package], limits: Limits) -> Self {
        let mut by_name: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
        let mut providers: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
        let mut replacers: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
        let mut groups: BTreeMap<&str, Vec<&Package>> = BTreeMap::new();
        for package in index {
            by_name.entry(&package.name).or_default().push(package);
            for provided in &package.provides {
                providers.entry(&provided.name).or_default().push(package);
            }
            for replaced in &package.replaces {
                replacers.entry(&replaced.name).or_default().push(package);
            }
            for group in &package.groups {
                groups.entry(group).or_default().push(package);
            }
        }
        for entries in by_name.values_mut() {
            entries.sort_by(|a, b| b.version.cmp(&a.version));
        }
        Self {
            index,
            by_name,
            providers,
            replacers,
            groups,
            selected: BTreeMap::new(),
            limits,
        }
    }

    fn run(
        mut self,
        base: &[(String, PacmanVersion)],
        requested: &[String],
    ) -> Result<Resolution, Diagnostic> {
        if requested.len() > self.limits.max_requests {
            return Err(self.limit("request count", self.limits.max_requests));
        }
        for (name, version) in base {
            let entry = self
                .by_name
                .get(name.as_str())
                .and_then(|entries| entries.iter().find(|p| p.version == *version).copied());
            self.selected.insert(
                name.clone(),
                Selected {
                    version: version.clone(),
                    origin: Origin::Base,
                    entry,
                },
            );
        }
        if self.selected.len() > self.limits.max_closure_size {
            return Err(self.limit("closure size", self.limits.max_closure_size));
        }

        let mut recorded = BTreeSet::new();
        let mut queue = VecDeque::new();
        let mut top_level: Vec<&str> = requested.iter().map(String::as_str).collect();
        top_level.sort_unstable();
        top_level.dedup();
        for name in top_level {
            if !self.by_name.contains_key(name)
                && let Some(members) = self.groups.get(name)
            {
                let mut names: Vec<&str> = members.iter().map(|p| p.name.as_str()).collect();
                names.sort_unstable();
                names.dedup();
                for member in names {
                    recorded.insert(member.to_string());
                    queue.push_back((unversioned(member), format!("group `{name}`")));
                }
            } else {
                self.check_requested(name)?;
                recorded.insert(name.to_string());
                queue.push_back((unversioned(name), "the manifest".to_string()));
            }
            if recorded.len() > self.limits.max_requests {
                return Err(self.limit("request count", self.limits.max_requests));
            }
        }

        let mut iterations = 0usize;
        while let Some((dependency, required_by)) = queue.pop_front() {
            iterations = iterations.saturating_add(1);
            if iterations > self.limits.max_iterations {
                return Err(self.limit("iteration count", self.limits.max_iterations));
            }
            if self.satisfied(&dependency) {
                continue;
            }
            let chosen = self.choose(&dependency, &required_by)?;
            let is_new = !self.selected.contains_key(&chosen.name);
            if is_new && self.selected.len() >= self.limits.max_closure_size {
                return Err(self.limit("closure size", self.limits.max_closure_size));
            }
            let owner = format!("{} {}", chosen.name, chosen.version);
            let changed = self
                .selected
                .get(&chosen.name)
                .is_none_or(|selected| selected.version != chosen.version);
            self.selected.insert(
                chosen.name.clone(),
                Selected {
                    version: chosen.version.clone(),
                    origin: Origin::Install,
                    entry: Some(chosen),
                },
            );
            if changed {
                for dependency in &chosen.depends {
                    queue.push_back((dependency.clone(), owner.clone()));
                }
            }
        }
        self.verify()?;
        Ok(Resolution {
            requested: recorded.into_iter().collect(),
            closure: self.closure()?,
        })
    }

    fn check_requested(&self, name: &str) -> Result<(), Diagnostic> {
        if self.by_name.contains_key(name)
            || self.providers.contains_key(name)
            || self.replacers.contains_key(name)
        {
            return Ok(());
        }
        let mut near: Vec<(usize, &str)> = self
            .index
            .iter()
            .map(|package| {
                let score = std::iter::once(package.name.as_str())
                    .chain(words(&package.description))
                    .map(|candidate| crate::manifest::distance(name, candidate))
                    .min()
                    .unwrap_or(name.len());
                (score, package.name.as_str())
            })
            .filter(|(score, _)| *score <= name.len().max(3) / 2 + 1)
            .collect();
        near.sort_unstable();
        near.dedup_by_key(|(_, candidate)| *candidate);
        let diagnostic = Diagnostic::new(
            "E_NO_MATCH",
            format!("package `{name}` is not in the pinned Arch indexes"),
        );
        if near.is_empty() {
            Err(diagnostic)
        } else {
            Err(diagnostic.hint(format!(
                "did you mean {}?",
                near.into_iter()
                    .take(3)
                    .map(|(_, candidate)| candidate)
                    .collect::<Vec<_>>()
                    .join(", ")
            )))
        }
    }

    fn satisfied(&self, dependency: &Dependency) -> bool {
        self.selected.iter().any(|(name, selected)| {
            if name == &dependency.name && dependency_holds(dependency, &selected.version) {
                return true;
            }
            selected.entry.is_some_and(|package| {
                package
                    .provides
                    .iter()
                    .any(|provided| provide_holds(dependency, provided))
                    || (dependency.constraint.is_none()
                        && package
                            .replaces
                            .iter()
                            .any(|replaced| replaced.name == dependency.name))
            })
        })
    }

    fn choose(
        &self,
        dependency: &Dependency,
        required_by: &str,
    ) -> Result<&'a Package, Diagnostic> {
        let exact = self.by_name.get(dependency.name.as_str());
        if let Some(package) = exact.and_then(|entries| {
            entries
                .iter()
                .find(|package| dependency_holds(dependency, &package.version))
                .copied()
        }) {
            return Ok(package);
        }

        let providers: Vec<&Package> = self
            .providers
            .get(dependency.name.as_str())
            .into_iter()
            .flat_map(|entries| entries.iter().copied())
            .filter(|package| {
                package
                    .provides
                    .iter()
                    .any(|provided| provide_holds(dependency, provided))
            })
            .collect();
        if let Some(package) = one_candidate(dependency, required_by, "providers", providers)? {
            return Ok(package);
        }

        if exact.is_none() && !self.providers.contains_key(dependency.name.as_str()) {
            let replacers: Vec<&Package> = self
                .replacers
                .get(dependency.name.as_str())
                .into_iter()
                .flat_map(|entries| entries.iter().copied())
                .collect();
            if let Some(package) =
                one_candidate(dependency, required_by, "replacements", replacers)?
            {
                return Ok(package);
            }
        }

        let available = exact
            .into_iter()
            .flat_map(|entries| entries.iter())
            .map(|package| format!("{} {}", package.name, package.version))
            .collect::<Vec<_>>();
        let diagnostic = Diagnostic::new(
            "E_NO_MATCH",
            format!(
                "no package satisfies `{}` (required by {required_by})",
                dependency_text(dependency)
            ),
        );
        if available.is_empty() {
            Err(diagnostic)
        } else {
            Err(diagnostic.hint(format!("available: {}", available.join(", "))))
        }
    }

    fn verify(&self) -> Result<(), Diagnostic> {
        for (name, selected) in &self.selected {
            let Some(package) = selected.entry else {
                continue;
            };
            if selected.origin == Origin::Install {
                for dependency in &package.depends {
                    if !self.satisfied(dependency) {
                        return Err(Diagnostic::new(
                            "E_NO_MATCH",
                            format!(
                                "no package satisfies `{}` (required by {name} {})",
                                dependency_text(dependency),
                                selected.version
                            ),
                        ));
                    }
                }
            }
            for conflict in &package.conflicts {
                if let Some(other) = self.conflicting(name, conflict) {
                    return Err(Diagnostic::new(
                        "E_NO_MATCH",
                        format!(
                            "{name} {} conflicts with `{}`, but the closure contains {other}",
                            selected.version,
                            dependency_text(conflict)
                        ),
                    ));
                }
            }
        }
        Ok(())
    }

    fn conflicting(&self, owner: &str, conflict: &Dependency) -> Option<String> {
        self.selected.iter().find_map(|(name, selected)| {
            if name == owner {
                return None;
            }
            let matched = (name == &conflict.name && dependency_holds(conflict, &selected.version))
                || selected.entry.is_some_and(|package| {
                    package
                        .provides
                        .iter()
                        .any(|provided| provide_holds(conflict, provided))
                });
            matched.then(|| format!("{name} {}", selected.version))
        })
    }

    fn closure(&self) -> Result<Vec<ClosurePackage>, Diagnostic> {
        self.selected
            .iter()
            .map(|(name, selected)| {
                let artifact = selected
                    .entry
                    .map(|package| {
                        let size = package.size.ok_or_else(|| {
                            Diagnostic::new(
                                "E_RECIPE_INVALID",
                                format!(
                                    "Arch package `{}` {} has no `%CSIZE%` in the pinned index",
                                    package.name, package.version
                                ),
                            )
                        })?;
                        Ok(Artifact {
                            filename: package.filename.clone(),
                            sha256: package.sha256.clone(),
                            size,
                            repository: package.repository.clone(),
                            suite: String::new(),
                        })
                    })
                    .transpose()?;
                Ok(ClosurePackage {
                    name: name.clone(),
                    version: selected.version.as_str().to_string(),
                    arch: selected.entry.map(|package| package.arch.clone()),
                    origin: selected.origin,
                    artifact,
                    epoch: None,
                    release: None,
                })
            })
            .collect()
    }

    fn limit(&self, name: &str, value: usize) -> Diagnostic {
        Diagnostic::new(
            "E_NO_MATCH",
            format!("Arch resolution exceeds the {name} limit of {value}"),
        )
    }
}

fn unversioned(name: &str) -> Dependency {
    Dependency {
        name: name.to_string(),
        constraint: None,
    }
}

fn dependency_holds(dependency: &Dependency, version: &PacmanVersion) -> bool {
    dependency.constraint.as_ref().is_none_or(|(relation, bound)| {
        matches!(bound, DependencyVersion::Package(required) if relation.holds(version.cmp(required)))
    })
}

fn provide_holds(dependency: &Dependency, provided: &Dependency) -> bool {
    if dependency.name != provided.name {
        return false;
    }
    match (&dependency.constraint, &provided.constraint) {
        (None, _) => true,
        (Some(_), None) => false,
        (
            Some((required_relation, DependencyVersion::Package(required))),
            Some((Relation::Equal, DependencyVersion::Package(version))),
        ) => required_relation.holds(version.cmp(required)),
        (
            Some((required_relation, DependencyVersion::SonameAbi(required))),
            Some((Relation::Equal, DependencyVersion::SonameAbi(version))),
        ) => required_relation.holds(version.cmp(required)),
        _ => false,
    }
}

fn one_candidate<'a>(
    dependency: &Dependency,
    required_by: &str,
    kind: &str,
    candidates: Vec<&'a Package>,
) -> Result<Option<&'a Package>, Diagnostic> {
    let mut names: BTreeSet<&str> = candidates
        .iter()
        .map(|package| package.name.as_str())
        .collect();
    match names.len() {
        0 => Ok(None),
        1 => {
            let name = names.pop_first().unwrap_or_default();
            Ok(candidates
                .into_iter()
                .filter(|package| package.name == name)
                .max_by(|left, right| left.version.cmp(&right.version)))
        }
        _ => Err(Diagnostic::new(
            "E_NO_MATCH",
            format!(
                "`{}` (required by {required_by}) has several {kind}",
                dependency_text(dependency)
            ),
        )
        .hint(format!(
            "candidates: {}",
            names.into_iter().collect::<Vec<_>>().join(", ")
        ))),
    }
}

fn dependency_text(dependency: &Dependency) -> String {
    match &dependency.constraint {
        None => dependency.name.clone(),
        Some((relation, DependencyVersion::Package(version))) => {
            format!("{}{}{}", dependency.name, relation.as_str(), version)
        }
        Some((relation, DependencyVersion::SonameAbi(abi))) => {
            format!("{}{}{}", dependency.name, relation.as_str(), abi)
        }
    }
}

fn words(description: &str) -> impl Iterator<Item = &str> {
    description
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '-')
        .filter(|word| !word.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_soname_comparison_does_not_use_pacman_version_equality() {
        let required = Dependency::parse("libexample.so=3-64").unwrap();
        let leading_zero = Dependency::parse("libexample.so=03-64").unwrap();
        assert!(!provide_holds(&required, &leading_zero));
        assert_eq!(
            PacmanVersion::parse("3-64").unwrap(),
            PacmanVersion::parse("03-64").unwrap()
        );
    }
}
