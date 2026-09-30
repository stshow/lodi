//! Version-discovery strategies (design `spec/05` §3.2, LD-80, LD-81).
//!
//! A strategy is **one module plus one line**: the module owns a variant of [`Discovery`], the
//! parser for its own `[versions]` keys and the discoverer the resolver calls, and the line is
//! its entry in [`STRATEGIES`] below. Nothing else in the tree knows a strategy's name —
//! `src/catalogue/recipe.rs` looks it up here, and `src/upstream/mod.rs` dispatches through it —
//! so a package that adds a strategy touches this file once and conflicts with no other lane.
//!
//! `catalogue/README.md` documents every registered strategy, and
//! `python3 -B scripts/check-catalogue.py` fails when a recipe names a strategy that is not in
//! both that document and the list below.

pub mod adoptium;
pub mod github_assets;
pub mod github_releases;
pub mod json_index;
pub mod text_index;

use std::collections::BTreeMap;

use toml_edit::TableLike;

use crate::catalogue::{Reader, render};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::version::{Constraint, Version};

/// How versions of a tool are discovered upstream: one variant per registered strategy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Discovery {
    /// Versions are the GA releases of one Adoptium *feature-release* line. `feature_version`
    /// names the line; when it is absent the line comes from the constraint, and a constraint
    /// that names no line (`latest`) means the maximum of the API's `available_lts_releases`.
    /// The asset URL and the artifact's SHA-256 both come from the API.
    Adoptium {
        feature_version: Option<u32>,
        image_type: String,
        jvm_impl: String,
    },
    /// Versions are encoded in the asset file names of the newest `releases` GitHub releases
    /// of `repo`; `asset` is the file-name template (`${version}` is captured, `${tag}` is the
    /// release tag). The asset's download URL comes from the release.
    GithubAssets {
        repo: String,
        releases: u32,
        asset: String,
    },
    /// Versions are the **release tags** of `repo`, matched against the `tag` template; the
    /// asset is the one whose file name is `asset` rendered for that version and tag, and its
    /// URL and `sha256:` digest come from the release. `prereleases` includes releases GitHub
    /// flags as pre-releases; drafts are never considered.
    GithubReleases {
        repo: String,
        releases: u32,
        tag: String,
        prereleases: bool,
        asset: String,
    },
    /// A JSON array at `url` whose entries carry the version under `version_key` (minus
    /// `strip_prefix`); when `files_key` is set, an entry counts only if that array holds
    /// `file` (the upstream's per-platform build list). The list is an array of strings, or —
    /// when `file_key` is set — an array of tables compared on that key, whose matched table
    /// the candidate carries in `index_fields` (M-0.4 T-4).
    JsonIndex {
        url: String,
        version_key: String,
        strip_prefix: String,
        files_key: Option<String>,
        file: Option<String>,
        file_key: Option<String>,
    },
    /// A text document at `url`, one candidate per line that matches the `line` template:
    /// its literal prefix and its literal suffix around `${version}` (M-0.4 T-4, LD-80 — there
    /// is still no regular expression anywhere in this).
    TextIndex { url: String, line: String },
}

/// One discovered version and where its asset is.
#[derive(Debug, Clone)]
pub struct Candidate {
    pub version: Version,
    pub text: String,
    pub tag: Option<String>,
    /// Asset file name and URL (the strategy found it), or `None` when the recipe renders it.
    pub asset: Option<(String, String)>,
    pub size: Option<u64>,
    /// `sha256:` digest published next to the asset, if any.
    pub digest: Option<String>,
    /// Whether upstream publishes a build for the requested architecture.
    pub for_arch: bool,
    /// What the discovery document itself said about the matched file: the string fields of the
    /// index entry, empty for a strategy that reads no such entry. A recipe whose
    /// `[asset] checksum = "index"` takes the artifact's digest from here (M-0.4 T-4).
    pub index_fields: BTreeMap<String, String>,
}

/// Where the download URL of the chosen version comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetUrl {
    /// The strategy knows it (and `url` in `[asset]` is therefore refused).
    FromStrategy,
    /// The recipe renders it from `url` in `[asset]` (which is therefore required).
    FromRecipe,
}

/// What a discoverer is given besides the fetcher: the recipe file, for diagnostics that name
/// it, and the architecture's template variables.
pub struct Context<'a> {
    pub file: &'a str,
    pub vars: &'a [(&'a str, &'a str)],
    /// The constraint the caller wrote, as written and as parsed. A strategy whose upstream is
    /// shaped one line per feature release — `adoptium` — needs it to know which line to ask
    /// for; the others ignore it and select from what discovery returned, as before.
    pub constraint_text: &'a str,
    pub constraint: &'a Constraint,
}

impl Context<'_> {
    /// `E_RECIPE_INVALID`, naming the recipe file.
    pub fn invalid(&self, message: impl Into<String>) -> Diagnostic {
        Reader::new(self.file).invalid(message)
    }

    /// Render a template with this architecture's variables.
    pub fn render(&self, template: &str) -> Result<String, Diagnostic> {
        render(template, self.vars).map_err(|e| self.invalid(e))
    }
}

/// Match `name` against a template in which `${version}` is the only unknown: the literal
/// prefix and the literal suffix must be there, and the digits and dots between them are the
/// version. **This is the whole of the matching this project does** — there is no regular
/// expression engine in the tree and no strategy may add one (LD-80, design call D5). It is
/// shared because both GitHub strategies match by it, one over asset file names and one over
/// release tags.
pub(crate) fn capture(template: &str, name: &str) -> Option<String> {
    let (before, after) = template.split_once("${version}")?;
    let rest = name.strip_prefix(before)?;
    let end = rest
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(rest.len());
    let (version, tail) = rest.split_at(end);
    (tail == after && !version.is_empty()).then(|| version.to_string())
}

/// Parse a strategy's own `[versions]` keys. `before_version` is the set of template variables
/// that are known before a version has been discovered.
pub type ParseFn =
    fn(&Reader, &dyn TableLike, before_version: &[&str]) -> Result<Discovery, Diagnostic>;

/// Discover every version upstream publishes, for the architecture in the context.
pub type DiscoverFn = fn(&dyn Fetcher, &Discovery, &Context) -> Result<Vec<Candidate>, Diagnostic>;

/// A registered strategy: the two functions the resolver calls, where the asset URL is from,
/// and whether the strategy carries the artifact's digest itself.
pub struct Strategy {
    pub parse: ParseFn,
    pub discover: DiscoverFn,
    pub asset_url: AssetUrl,
    /// `true` when discovery yields the artifact's own `sha256:` digest, so a recipe using this
    /// strategy needs no `checksum_file` (`spec/05` §3.3). `scripts/check-catalogue.py` keeps
    /// the same set in `STRATEGIES_WITH_OWN_DIGEST`, and the recipe parser refuses a recipe
    /// that declares no hash source at all.
    pub supplies_digest: bool,
}

/// The registration list: the TOML name of every strategy this build has, sorted, one line each.
/// **This list is the only shared line a new strategy adds.**
pub const STRATEGIES: &[(&str, Strategy)] = &[
    ("adoptium", adoptium::STRATEGY),
    ("github_assets", github_assets::STRATEGY),
    ("github_releases", github_releases::STRATEGY),
    ("json_index", json_index::STRATEGY),
    ("text_index", text_index::STRATEGY),
];

/// The registered strategy called `name`, with the `'static` name to quote in diagnostics.
pub fn find(name: &str) -> Option<(&'static str, &'static Strategy)> {
    STRATEGIES
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, s)| (*n, s))
}

/// The registered strategy of a parsed [`Discovery`], by the name the recipe carried.
pub fn discoverer(name: &str) -> Option<DiscoverFn> {
    find(name).map(|(_, s)| s.discover)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registration_list_is_sorted_and_unique() {
        let names: Vec<&str> = STRATEGIES.iter().map(|(n, _)| *n).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "STRATEGIES must be sorted and hold no duplicate"
        );
    }

    #[test]
    fn an_unregistered_strategy_is_not_found() {
        assert!(find("static").is_none());
        assert!(find("github_assets").is_some());
        assert!(find("adoptium").is_some());
    }
}
