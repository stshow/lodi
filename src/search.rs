//! `lodi search` (M-0.4 T-6, design calls D12, D13, D14; LD-496).
//!
//! A read-only browse command. It answers "what can this build give me?", and it answers it
//! **with the network off**:
//!
//! * **It never makes a request** (D12), not even a conditional one. The catalogue is embedded
//!   in the binary, and the distro half reads whatever the download cache already holds — **no
//!   command in this build puts a `Packages.xz` there** (LD-182): the apt family's
//!   `fetch_index` verifies each index and drops the bytes, so every real project sees the
//!   cold-cache note today and the fixture is what proves the warm path. A cold cache is one
//!   line on standard error and exit **0** — a browse command that sometimes takes thirty
//!   seconds is one people stop trusting.
//! * **It writes nothing**: no file, no directory, no lock, no store and no cache entry. Nothing
//!   here opens the store through [`crate::store::Store::open`], which would create its
//!   directories; the one path it builds is read with [`std::fs::read`] and is absent as often
//!   as not.
//! * It resolves, locks and realizes nothing, so it can substitute no version.
//!
//! A query that is a recipe's name, ignoring case, shows that recipe's details first: its file
//! and hash, what it replaces, and what the current directory's lock pinned for it (LD-496).
//! The other matches follow.
//!
//! The distro half reads **only** the indexes `./lodi.lock` itself names (D13): each
//! `Packages.xz` is looked up in `<data root>/cache/dl/` by the SHA-256 the lock recorded for
//! it, so the lookup is exact, needs no new state, and cannot show packages from a distribution
//! this project does not use. A file that is there is re-hashed before it is read; a file that
//! is not is the note above.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalogue::{Recipe, builtin_recipes};
use crate::debian::base::xz_decompress;
use crate::debian::index::{IndexHit, search_index};
use crate::debian::version::DebVersion;
use crate::lock::{self, Failure, LockProblem};
use crate::store::art_name;
use crate::util::{sha256_hex, untag_sha256};

/// What `lodi search` is asked to look at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// Both halves: the catalogue first, then the project's distro indexes.
    Both,
    /// `--registry`: the embedded catalogue only.
    Registry,
    /// `--distro`: the project's distro indexes only.
    Distro,
}

/// What a browse command produced: the report, and the notes that belong on standard error.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub lines: Vec<String>,
    /// One line each, already carrying the `lodi: warning …` prefix.
    pub notes: Vec<String>,
}

/// The note a cold or absent index makes: not an error, because an index the cache does not
/// hold leaves the catalogue half answered and nothing else broken (design call D13).
fn note(reason: impl std::fmt::Display) -> String {
    format!("lodi: warning W_NO_INDEX: {reason}")
}

/// Pad `text` to `width` columns, never truncating it.
fn column(text: &str, width: usize) -> String {
    let mut out = text.to_string();
    while out.chars().count() < width {
        out.push(' ');
    }
    out
}

// --------------------------------------------------------------------------- lodi search ---

/// `lodi search QUERY` for the project in `root` (`spec/10-cli` §4, design calls D12 and D13).
///
/// The details of the recipe named exactly `query` come first, then the catalogue rows sorted
/// by name without that recipe, then the distro rows of the indexes the project's own lock
/// names. The query matches case-insensitively as a substring of a name or a description.
pub fn search(root: &Path, query: &str, scope: Scope) -> Result<Report, Failure> {
    let mut report = Report::default();
    let mut matched = false;
    if scope != Scope::Distro {
        // The built-in catalogue alone: search never looks for a config (#698 story 39).
        let builtin = builtin_recipes().map_err(Failure::one)?;
        let exact = |r: &&Recipe| r.name.eq_ignore_ascii_case(query);
        if let Some(recipe) = builtin.iter().find(exact) {
            report.lines.extend(details(root, recipe));
            matched = true;
        }
        let others: Vec<Recipe> = builtin.iter().filter(|r| !exact(r)).cloned().collect();
        matched |= recipe_rows("catalogue", &others, query, &mut report);
    }
    if scope != Scope::Registry {
        matched |= distro_rows(root, query, scope == Scope::Distro, &mut report);
    }
    if !matched {
        report.lines.push(format!("no match for \"{query}\""));
    }
    Ok(report)
}

/// The rows of the `recipes` that answer `query`, under `heading`; whether there were any.
fn recipe_rows(heading: &str, recipes: &[Recipe], query: &str, report: &mut Report) -> bool {
    let hits: Vec<&Recipe> = recipes
        .iter()
        .filter(|r| recipe_matches(r, query))
        .collect();
    if hits.is_empty() {
        return false;
    }
    let width = hits
        .iter()
        .map(|r| r.name.chars().count())
        .max()
        .unwrap_or(0);
    report.lines.push(heading.to_string());
    for recipe in hits {
        report.lines.push(format!(
            "  {}  {}  ({})",
            column(&recipe.name, width),
            recipe.description,
            recipe.homepage
        ));
    }
    true
}

/// Whether one recipe answers `query`: its name or its description, ASCII case-insensitively.
fn recipe_matches(recipe: &Recipe, query: &str) -> bool {
    let query = query.to_ascii_lowercase();
    recipe.name.to_ascii_lowercase().contains(&query)
        || recipe.description.to_ascii_lowercase().contains(&query)
}

/// The distro half: rows from the indexes `./lodi.lock` names, or one note saying which of the
/// reasons there were none. Returns whether it printed any row.
///
/// The note is printed only where it is something to act on: when the distro half was asked for
/// by name (`asked`, `--distro`), or when the lock names a container base, so that the index
/// missing is one of a distribution this project uses. Someone browsing the catalogue with no
/// lock, or in a host-tool project, gets the catalogue and no note.
fn distro_rows(root: &Path, query: &str, asked: bool, report: &mut Report) -> bool {
    let lock = match lock::read_lock(root) {
        Ok(lock) => lock,
        Err(LockProblem::Missing) => {
            if asked {
                report.notes.push(note(format!(
                    "there is no ./{}, so no distro index is named; `lodi develop` writes it",
                    lock::LOCK_FILE
                )));
            }
            return false;
        }
        Err(problem) => {
            if asked {
                let why = problem.diagnostic(lock::LOCK_FILE).message;
                report.notes.push(note(why));
            }
            return false;
        }
    };
    let Some(base) = &lock.base else {
        if asked {
            report.notes.push(note(format!(
                "./{} pins no container base, so this project has no distro index",
                lock::LOCK_FILE
            )));
        }
        return false;
    };
    // The download cache, located but never created: `search` writes nothing at all.
    let cache = match crate::roots::capture().store_root() {
        Ok(data) => data.join("cache/dl"),
        Err(d) => {
            report
                .notes
                .push(note(format!("{}; there is no download cache", d.message)));
            return false;
        }
    };
    let mut best: BTreeMap<String, IndexHit> = BTreeMap::new();
    let mut cold = Vec::new();
    for repository in &base.repositories {
        for index in &repository.indexes {
            let label = format!("{} {}/{}", repository.name, index.suite, index.component);
            match read_index(&cache, &index.packages.sha256) {
                Ok(Some(text)) => {
                    for hit in search_index(&text, query) {
                        keep_newest(&mut best, hit);
                    }
                }
                Ok(None) => cold.push(label),
                Err(why) => cold.push(format!("{label} ({why})")),
            }
        }
    }
    if !cold.is_empty() {
        // No command in this build fills `cache/dl` with a `Packages.xz` (LD-182, LD-193), so
        // the note says what is missing and stops there rather than naming a command that
        // would not help.
        report.notes.push(note(format!(
            "the package index of {} is not in the download cache, which no command of this \
             build fills yet, so `lodi search` has no distro half for this project",
            cold.join(", ")
        )));
    }
    if best.is_empty() {
        return false;
    }
    let width = best.keys().map(|n| n.chars().count()).max().unwrap_or(0);
    report
        .lines
        .push(format!("{} {}", base.distro, base.release));
    for hit in best.values() {
        report.lines.push(format!(
            "  {}  {}  {}",
            column(&hit.name, width),
            hit.version,
            hit.description
        ));
    }
    true
}

/// One `Packages.xz` out of `cache/dl`, keyed by the SHA-256 the lock recorded for it.
/// `Ok(None)` means the cache does not hold it; an `Err` is a file that is there and unusable.
fn read_index(cache: &Path, tagged: &str) -> Result<Option<String>, String> {
    let hex = untag_sha256(tagged).ok_or("the lock records a malformed index hash")?;
    let path: PathBuf = cache.join(hex);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read the cached index: {e}")),
    };
    // The cache is content-addressed, so a file whose bytes are not what the lock named is not
    // this index: browsing never shows unverified content.
    if sha256_hex(&bytes) != hex {
        return Err("the cached index does not match the hash in the lock".into());
    }
    let plain = xz_decompress(&bytes)?;
    String::from_utf8(plain)
        .map(Some)
        .map_err(|_| "the cached index is not UTF-8".to_string())
}

/// One row per package name: the highest Debian version wins, so a package the project's
/// `-updates` or security suite also carries is shown once, at the version that would install.
fn keep_newest(best: &mut BTreeMap<String, IndexHit>, hit: IndexHit) {
    match best.get(&hit.name) {
        Some(kept) => {
            let newer = match (
                DebVersion::parse(&hit.version),
                DebVersion::parse(&kept.version),
            ) {
                (Ok(new), Ok(old)) => new > old,
                _ => false,
            };
            if newer {
                best.insert(hit.name.clone(), hit);
            }
        }
        None => {
            best.insert(hit.name.clone(), hit);
        }
    }
}

/// What `lodi search` shows first for the recipe its query names exactly: what the catalogue
/// knows about it, plus what the current directory's lock pinned for that label when it has one.
fn details(root: &Path, recipe: &Recipe) -> Vec<String> {
    let mut lines = vec![recipe.name.clone()];
    let field = |key: &str, value: &str| format!("  {}  {value}", column(&format!("{key}:"), 12));
    lines.push(field("description", &recipe.description));
    lines.push(field("homepage", &recipe.homepage));
    lines.push(field("strategy", recipe.strategy));
    lines.push(field("upstream", &upstream(recipe)));
    lines.push(field("bin", &recipe.bin.join(", ")));
    lines.push(field("path", &recipe.path.join(", ")));
    lines.push(field(
        "recipe",
        &format!("{} ({})", recipe.file, recipe.sha256),
    ));
    // The lock is read only to answer "and what is this project pinned to?"; its absence, its
    // staleness and a label this recipe does not appear under are all simply silence.
    if let Ok(lock) = lock::read_lock(root)
        && let Some(entry) = lock.packages.get(&recipe.name)
    {
        lines.push(field("locked", &entry.version));
        for artifact in &entry.artifacts {
            lines.push(field("url", &artifact.url));
            lines.push(field("sha256", &artifact.sha256));
        }
        if let Ok(art) = art_name(entry) {
            lines.push(field("entry", &art));
        }
    }
    lines
}

/// Where a recipe's strategy looks, in one line the user can open.
fn upstream(recipe: &Recipe) -> String {
    use crate::upstream::strategy::Discovery;
    match &recipe.versions {
        Discovery::GithubAssets { repo, .. } | Discovery::GithubReleases { repo, .. } => {
            format!("https://github.com/{repo}/releases")
        }
        Discovery::JsonIndex { url, .. } => url.clone(),
        Discovery::TextIndex { url, .. } => url.clone(),
        // Adoptium is asked one feature-release line at a time (M-0.4 T-5). A recipe that pins
        // the line names its endpoint; one that does not takes the line from the constraint at
        // lock time, so the line to open is the vendor's own release page.
        Discovery::Adoptium {
            feature_version, ..
        } => match feature_version {
            Some(line) => format!("https://api.adoptium.net/v3/assets/feature_releases/{line}/ga"),
            None => "https://adoptium.net/temurin/releases/".to_string(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn columns_never_truncate() {
        assert_eq!(column("ab", 4), "ab  ");
        assert_eq!(column("abcdef", 2), "abcdef");
    }
}
