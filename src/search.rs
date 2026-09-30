//! `lodi search` and `lodi info` (M-0.4 T-6, design calls D12, D13, D14, D17).
//!
//! Two read-only browse commands. Between them they answer "what can this build give me?" and
//! "what is this project pinned to?", and they answer it **with the network off**:
//!
//! * **Neither command ever makes a request** (D12), not even a conditional one. The catalogue
//!   is embedded in the binary, and the distro half of `search` reads whatever the download
//!   cache already holds — **no command in this build puts a `Packages.xz` there** (LD-182):
//!   the apt family's `fetch_index` verifies each index and drops the bytes, so every real
//!   project sees the cold-cache note today and the fixture is what proves the warm path. A
//!   cold cache is one line on standard error and exit **0** — a browse command that sometimes
//!   takes thirty seconds is one people stop trusting.
//! * **Neither command writes anything**: no file, no directory, no lock, no store and no cache
//!   entry. Nothing here opens the store through [`crate::store::Store::open`], which would
//!   create its directories; the one path it builds is read with [`std::fs::read`] and is
//!   absent as often as not.
//! * Neither resolves, locks or realizes anything, so neither can substitute a version.
//!
//! The distro half reads **only** the indexes `./lodi.lock` itself names (D13): each
//! `Packages.xz` is looked up in `<data root>/cache/dl/` by the SHA-256 the lock recorded for
//! it, so the lookup is exact, needs no new state, and cannot show packages from a distribution
//! this project does not use. A file that is there is re-hashed before it is read; a file that
//! is not is the note above.
//!
//! `lodi info` needs `./lodi.toml` (`E_NO_MANIFEST`, exit 3) but **not** a fresh lock (D17):
//! reporting that the lock is stale is the command's job, so a stale lock is `lock: stale` with
//! the labels that changed, not `E_LOCK_STALE`. `lodi info TOOL` needs neither file — a recipe
//! is a property of the binary — and an unknown name is `E_NO_RECIPE` (exit 4) naming the
//! nearest ones.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::catalogue::user::{self, UserRecipes};
use crate::catalogue::{Recipe, builtin_recipe, builtin_recipes};
use crate::debian::base::xz_decompress;
use crate::debian::index::{IndexHit, search_index};
use crate::debian::version::DebVersion;
use crate::diag::Diagnostic;
use crate::lock::{self, Failure, LockFile, LockProblem, ToolEntry};
use crate::manifest::ProjectManifest;
use crate::store::art_name;
use crate::tools::{Catalogue, ToolDecl, nearest};
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
/// Catalogue rows first, sorted by name, then the distro rows of the indexes the project's own
/// lock names. The query matches case-insensitively as a substring of a name or a description.
pub fn search(root: &Path, query: &str, scope: Scope) -> Result<Report, Failure> {
    let mut report = Report::default();
    let mut matched = false;
    if scope != Scope::Distro {
        let own = user::active().map_err(Failure::one)?;
        report.notes.extend(own.broken());
        // A built-in recipe a recipe of your own hides is not offered (LD-409).
        let builtin: Vec<Recipe> = builtin_recipes()
            .map_err(Failure::one)?
            .into_iter()
            .filter(|r| !own.has(&r.name))
            .collect();
        matched |= recipe_rows("catalogue", &builtin, query, &mut report);
        if let Some(dir) = &own.dir {
            let heading = format!("recipes  ({})", dir.display());
            matched |= recipe_rows(&heading, &own.recipes(), query, &mut report);
        }
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
                    "there is no ./{}, so no distro index is named; run `lodi lock`",
                    lock::LOCK_FILE
                )));
            }
            return false;
        }
        Err(problem) => {
            if asked {
                let why = problem.diagnostic(lock::LOCK_FILE).message;
                report.notes.push(note(format!(
                    "{why}; run `lodi lock` to write a usable one"
                )));
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

// ----------------------------------------------------------------------------- lodi info ---

/// `lodi info` with no argument: the current project, from `./lodi.toml` and, when it is there,
/// `./lodi.lock`. A missing manifest is `E_NO_MANIFEST` (exit 3); a stale lock is **reported**,
/// never refused (design call D17).
pub fn project(root: &Path) -> Result<Report, Failure> {
    let (manifest, _) = lock::load_manifest(root)?;
    let mut report = Report::default();
    let name = manifest
        .environment_name(root)
        .unwrap_or_else(|| "(unnamed)".to_string());
    report.lines.push(format!("project: {name}"));
    report.lines.push(format!(
        "mode:    {}",
        if manifest.is_container_mode() {
            "container"
        } else {
            "host tools"
        }
    ));
    let lock = lock::read_lock(root);
    if let Some(container) = &manifest.container {
        let snapshot = match (&lock, &container.snapshot) {
            (_, Some(pinned)) => pinned.clone(),
            (Ok(l), None) => l.base.as_ref().map_or_else(
                || "(pinned by `lodi lock`)".to_string(),
                |b| b.snapshot.clone(),
            ),
            (Err(_), None) => "(pinned by `lodi lock`)".to_string(),
        };
        report.lines.push(format!(
            "base:    {} {} snapshot {snapshot}",
            container.distro.name(),
            container.release
        ));
        if !manifest.packages.is_empty() {
            report
                .lines
                .push(format!("packages: {}", manifest.packages.join(", ")));
        }
    }
    let locked = lock.as_ref().ok();
    if manifest.tools.is_empty() {
        report.lines.push("tools:   (none)".to_string());
    } else {
        report.lines.push("tools:".to_string());
        let width = manifest
            .tools
            .keys()
            .map(|l| l.chars().count())
            .max()
            .unwrap_or(0);
        for (label, tool) in &manifest.tools {
            let entry = locked.and_then(|l| l.packages.get(label));
            let mut row = format!(
                "  {}  {}  {}",
                column(label, width),
                column(&tool.constraint_text, 10),
                tool_state(entry)
            );
            if let Some(missing) = missing_recipe(label, tool) {
                row.push_str("  ");
                row.push_str(&missing);
            }
            report.lines.push(row);
        }
    }
    report.lines.extend(lock_status(&manifest, &lock));
    Ok(report)
}

/// The note on a `[tools]` row whose recipe the catalogue does not have, with the nearest names
/// exactly as `lodi lock`'s `E_NO_RECIPE` gives them — the whole catalogue when nothing is near.
/// An inline tool names a URL instead of a recipe and never gets one.
fn missing_recipe(label: &str, tool: &ToolDecl) -> Option<String> {
    let eff = tool.effective(lock::HOST_ARCH);
    let name = eff.name.clone().unwrap_or_else(|| label.to_string());
    let own = user::active().unwrap_or_default();
    if eff.is_inline() || own.recipe(&name).is_some() || builtin_recipe(&name).is_some() {
        return None;
    }
    let known = Catalogue::names(&*own);
    let near = nearest(&name, &known);
    let listed = if near.is_empty() { &known } else { &near };
    Some(format!(
        "(no recipe `{name}` in the catalogue; nearest names: {})",
        listed.join(", ")
    ))
}

/// The locked half of one `[tools]` row: the version and the store entry it would use.
fn tool_state(entry: Option<&ToolEntry>) -> String {
    match entry {
        None => "(not locked)".to_string(),
        Some(entry) => match art_name(entry) {
            Ok(name) => format!("{}  {name}", entry.version),
            Err(_) => entry.version.clone(),
        },
    }
}

/// `lock: fresh`, `lock: stale` with the labels that changed, or `lock: missing` — the whole
/// point of the command (design call D17), so none of these is an error.
fn lock_status(manifest: &ProjectManifest, lock: &Result<LockFile, LockProblem>) -> Vec<String> {
    match lock {
        Err(LockProblem::Missing) => vec![format!(
            "lock:    missing (run `lodi lock` to write ./{})",
            lock::LOCK_FILE
        )],
        Err(problem) => vec![format!(
            "lock:    unusable: {}",
            problem.diagnostic(lock::LOCK_FILE).message
        )],
        Ok(lock) => {
            let stale = lock::staleness(manifest, lock);
            if stale.is_empty() {
                return vec!["lock:    fresh".to_string()];
            }
            let mut lines = vec!["lock:    stale".to_string()];
            lines.extend(stale.into_iter().map(|label| format!("  {label}")));
            lines
        }
    }
}

/// `lodi info TOOL`: what the embedded catalogue knows about one recipe, plus what the current
/// directory's lock pinned for that label when it has one. Needs no manifest and no lock.
pub fn tool(root: &Path, name: &str) -> Result<Report, Failure> {
    let own = user::active().map_err(Failure::one)?;
    let recipe = match Catalogue::recipe(&*own, name) {
        Some(recipe) => recipe.map_err(Failure::one)?,
        None => return Err(Failure::one(no_recipe(name, &own))),
    };
    let mut report = Report::default();
    report.lines.push(recipe.name.clone());
    let field = |key: &str, value: &str| format!("  {}  {value}", column(&format!("{key}:"), 12));
    report.lines.push(field("description", &recipe.description));
    report.lines.push(field("homepage", &recipe.homepage));
    report.lines.push(field("strategy", recipe.strategy));
    report.lines.push(field("upstream", &upstream(&recipe)));
    report.lines.push(field("bin", &recipe.bin.join(", ")));
    report.lines.push(field("path", &recipe.path.join(", ")));
    // A recipe of your own is named by where it is: your repository's `recipes/` folder.
    let file = match (&own.dir, recipe.input) {
        (Some(dir), "user") => dir.join(recipe.lock_path()).display().to_string(),
        _ => recipe.file.clone(),
    };
    report
        .lines
        .push(field("recipe", &format!("{file} ({})", recipe.sha256)));
    if recipe.input == "user" && builtin_recipe(name).is_some() {
        report
            .lines
            .push(field("replaces", &format!("the built-in recipe {name}")));
    }
    // The lock is read only to answer "and what is this project pinned to?"; its absence, its
    // staleness and a label this recipe does not appear under are all simply silence.
    if let Ok(lock) = lock::read_lock(root)
        && let Some(entry) = lock.packages.get(name)
    {
        report.lines.push(field("locked", &entry.version));
        for artifact in &entry.artifacts {
            report.lines.push(field("url", &artifact.url));
            report.lines.push(field("sha256", &artifact.sha256));
        }
        if let Ok(art) = art_name(entry) {
            report.lines.push(field("entry", &art));
        }
    }
    Ok(report)
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

/// `E_NO_RECIPE` (exit 4) with the catalogue's nearest names: a typo gets the name it meant,
/// and a name that resembles nothing gets the whole (short) catalogue.
fn no_recipe(name: &str, own: &UserRecipes) -> Diagnostic {
    let names = Catalogue::names(own);
    let mut ranked: Vec<(usize, String)> = names
        .iter()
        .map(|candidate| (distance(name, candidate), candidate.clone()))
        .collect();
    ranked.sort();
    let threshold = name.chars().count().div_ceil(2).max(2);
    let nearest: Vec<String> = ranked.iter().take(3).map(|(_, n)| n.clone()).collect();
    let close = ranked.first().is_some_and(|(d, _)| *d <= threshold);
    let hint = if close {
        format!(
            "did you mean {}?",
            ranked
                .iter()
                .filter(|(d, _)| *d <= threshold)
                .take(3)
                .map(|(_, n)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )
    } else {
        format!(
            "the nearest names in the catalogue are {}; this build carries {}",
            nearest.join(", "),
            names.join(", ")
        )
    };
    Diagnostic::new(
        "E_NO_RECIPE",
        format!("no recipe for tool `{name}` in the built-in catalogue"),
    )
    .hint(hint)
    .hint(format!("a recipe of your own goes in {}", own.place(name)))
}

/// Levenshtein distance, for "did you mean" alone. Both names are short catalogue names.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let cost = usize::from(ca != cb);
            let next = (row[j] + 1).min(row[j + 1] + 1).min(previous + cost);
            previous = row[j + 1];
            row[j + 1] = next;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nearest_names_are_ranked_and_bounded() {
        assert_eq!(distance("pyhton", "python"), 2);
        let d = no_recipe("pyhton", &UserRecipes::none());
        assert_eq!(d.code, "E_NO_RECIPE");
        assert!(
            d.notes.iter().any(|n| n.contains("python")),
            "{:?}",
            d.notes
        );
        // A name that resembles nothing in the catalogue gets the catalogue instead.
        let d = no_recipe("zzzzzzzzzzzzzzzz", &UserRecipes::none());
        assert!(
            d.notes
                .iter()
                .any(|n| n.contains("the nearest names in the catalogue are")),
            "{:?}",
            d.notes
        );
    }

    #[test]
    fn columns_never_truncate() {
        assert_eq!(column("ab", 4), "ab  ");
        assert_eq!(column("abcdef", 2), "abcdef");
    }
}
