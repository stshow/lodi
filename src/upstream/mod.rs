//! Upstream tool resolution (design `spec/05` §3, OD-08): discover the versions a recipe's
//! upstream publishes, select the highest one matching the manifest's constraint, and pin its
//! download URL and SHA-256 from the upstream checksum file. Nothing is downloaded but
//! metadata: the artifact itself is fetched and verified at realization (S-3).
//!
//! What is *upstream-specific* is not here: each discovery strategy is its own module under
//! [`strategy`], registered in one sorted list, and this file dispatches through that list
//! without naming a strategy (LD-81).

pub mod strategy;

use std::collections::BTreeMap;

use serde_json::Value;

use crate::catalogue::{ChecksumSource, Recipe, arch_vars, render};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::version::Constraint;

use strategy::{Candidate, Context};

/// A tool pinned for one architecture, everything needed to realize it without discovery.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedTool {
    pub name: String,
    pub constraint: String,
    pub version: String,
    /// The upstream tag the version was found under (`github_assets`), if any.
    pub tag: Option<String>,
    /// Where the pin came from: `builtin` (a catalogue recipe) or `inline` (the declaration's
    /// own `url` and `sha256`, `spec/01` §3.8).
    pub input: String,
    /// The recipe file name, or the declaration's table path for an inline tool.
    pub recipe_file: String,
    pub recipe_sha256: String,
    pub arch: String,
    /// The artifacts of this version, in declaration order. A recipe with several `[[asset]]`
    /// entries, and only such a recipe, pins more than one; they are extracted over one another
    /// into the one store entry of the tool (M-0.4 T-4, `spec/01` §3.8).
    pub artifacts: Vec<LockedArtifact>,
    pub bin: Vec<String>,
    pub path: Vec<String>,
    /// The contributed environment the declaration asked for, with `${self.path}` and
    /// `${self.version}` still unresolved: they are substituted at realization.
    pub env: BTreeMap<String, String>,
    /// `sha256:` of the declaration, for a declaration that used a key beyond `version`. It is
    /// what makes a changed declaration stale; a declaration that is only a constraint has
    /// `None`, so its lock bytes are exactly what earlier builds wrote.
    pub declaration: Option<String>,
}

/// One pinned artifact: where it is, what it hashes to, and how it is unpacked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedArtifact {
    pub url: String,
    /// Lowercase hex.
    pub sha256: String,
    pub size: Option<u64>,
    pub format: String,
    pub strip_components: u32,
    /// The directory inside the archive to use as the root, after `strip_components`.
    pub subdir: String,
    /// Exact post-strip regular-file paths the recipe excludes from this artifact.
    pub exclude: Vec<String>,
}

pub(crate) fn fetch_error(e: crate::fetch::FetchError) -> Diagnostic {
    Diagnostic::new("E_FETCH", e.to_string())
}

/// A sidecar digest that could not be fetched: any HTTP status, tried again or not (LD-386),
/// is `E_NO_CHECKSUM` — upstream did not publish what this recipe verifies against — and any
/// other failure is `E_FETCH`.
fn sidecar_error(sidecar_url: &str, e: crate::fetch::FetchError) -> Diagnostic {
    let Some(status) = e.status() else {
        return fetch_error(e);
    };
    let tried = match e {
        crate::fetch::FetchError::Exhausted(attempts, _) => {
            format!("; gave up after {attempts} attempts")
        }
        _ => String::new(),
    };
    Diagnostic::new(
        "E_NO_CHECKSUM",
        format!("{sidecar_url} is not published (HTTP {status}{tried})"),
    )
    .hint(
        "the recipe verifies this artifact against the digest upstream \
         publishes beside it; Lodi downloads nothing it cannot verify",
    )
}

pub(crate) fn json(bytes: &[u8], url: &str) -> Result<Value, Diagnostic> {
    serde_json::from_slice(bytes).map_err(|e| {
        Diagnostic::new(
            "E_RECIPE_CTX",
            format!("{url} is not the JSON the recipe expects: {e}"),
        )
    })
}

/// Resolve `constraint` for tool `name` through `recipe` on architecture `arch` (`x86_64`).
pub fn resolve_tool(
    fetcher: &dyn Fetcher,
    recipe: &Recipe,
    name: &str,
    constraint_text: &str,
    constraint: &Constraint,
    arch: &str,
) -> Result<LockedTool, Diagnostic> {
    let vars = arch_vars(&recipe.arch_names, arch);
    let needs_arch_name = !recipe.arch_names.is_empty();
    if (needs_arch_name && !recipe.arch_names.contains_key(arch)) || arch != "x86_64" {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!("the {} recipe has no build for {arch}", recipe.name),
        ));
    }
    let var_refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let invalid = |e: String| {
        Diagnostic::new(
            "E_RECIPE_INVALID",
            format!("{}: {e}", crate::catalogue::described(&recipe.file)),
        )
    };

    let ctx = Context {
        file: &recipe.file,
        vars: &var_refs,
        constraint_text,
        constraint,
    };
    let discover = strategy::discoverer(recipe.strategy)
        .ok_or_else(|| invalid(format!("unknown version strategy `{}`", recipe.strategy)))?;
    let candidates = discover(fetcher, &recipe.versions, &ctx)?;

    let matching: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| constraint.matches(&c.version))
        .collect();
    let Some(chosen) = matching
        .iter()
        .filter(|c| c.for_arch)
        .max_by(|a, b| a.version.cmp(&b.version))
        .copied()
    else {
        if !matching.is_empty() {
            return Err(Diagnostic::new(
                "E_UNSUPPORTED_ARCH",
                format!(
                    "{name} {constraint_text}: upstream publishes {} without a {arch} build",
                    matching
                        .iter()
                        .map(|c| c.text.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            ));
        }
        return Err(no_match(name, constraint_text, &candidates));
    };

    let major = chosen
        .version
        .parts
        .first()
        .map_or(String::new(), u64::to_string);
    let minor = chosen
        .version
        .parts
        .get(1)
        .map_or(String::new(), u64::to_string);
    let mut all_vars = var_refs.clone();
    all_vars.push(("version", &chosen.text));
    all_vars.push(("version_major", &major));
    all_vars.push(("version_minor", &minor));
    if let Some(tag) = &chosen.tag {
        all_vars.push(("tag", tag));
    }
    // One artifact per `[asset]`/`[[asset]]`, in declaration order, each with its own URL and
    // its own hash source. Nothing is downloaded here: this is the pin.
    let mut artifacts = Vec::new();
    for asset in &recipe.assets {
        let (file_name, url) = match (&chosen.asset, &asset.url) {
            (Some((name, url)), _) => (name.clone(), url.clone()),
            (None, Some(template)) => {
                let url = render(template, &all_vars).map_err(invalid)?;
                let name = url.rsplit('/').next().unwrap_or("").to_string();
                (name, url)
            }
            (None, None) => return Err(invalid("no asset URL".to_string())),
        };
        if !url.starts_with("https://") {
            return Err(Diagnostic::new(
                "E_INSECURE_URL",
                format!("{url} is not an HTTPS URL"),
            ));
        }
        let sha256 = asset_sha256(fetcher, asset, chosen, &file_name, &url, &all_vars, invalid)?;
        artifacts.push(LockedArtifact {
            url,
            sha256,
            size: chosen.size,
            format: asset.format.clone(),
            strip_components: asset.strip_components,
            subdir: asset.subdir.clone(),
            exclude: asset.exclude.clone(),
        });
    }

    Ok(LockedTool {
        name: name.to_string(),
        constraint: constraint_text.to_string(),
        version: chosen.text.clone(),
        tag: chosen.tag.clone(),
        input: recipe.input.to_string(),
        recipe_file: recipe.lock_path().to_string(),
        recipe_sha256: recipe.sha256.clone(),
        arch: arch.to_string(),
        artifacts,
        bin: recipe.bin.clone(),
        path: recipe.path.clone(),
        // A declaration's environment is overlaid on the recipe's contribution later; keep
        // `${self.path}` and `${self.version}` unresolved until realization.
        env: recipe.env.clone(),
        declaration: None,
    })
}

/// The SHA-256 this artifact is pinned with, from the one hash source its `[asset]` declares
/// (`spec/05` §3.3). There is no fallback and no third branch: an artifact whose digest this
/// resolver cannot state is `E_NO_CHECKSUM`, never a download.
fn asset_sha256(
    fetcher: &dyn Fetcher,
    asset: &crate::catalogue::Asset,
    chosen: &Candidate,
    file_name: &str,
    url: &str,
    all_vars: &[(&str, &str)],
    invalid: impl Fn(String) -> Diagnostic,
) -> Result<String, Diagnostic> {
    match &asset.checksum {
        ChecksumSource::File(template) => {
            let checksum_url = render(template, all_vars).map_err(invalid)?;
            let sums = fetcher.get(&checksum_url).map_err(fetch_error)?;
            let sha256 =
                sha256sums_lookup(&String::from_utf8_lossy(&sums), file_name).ok_or_else(|| {
                    Diagnostic::new(
                        "E_NO_CHECKSUM",
                        format!("{checksum_url} lists no SHA-256 for {file_name}"),
                    )
                })?;
            cross_check(&sha256, chosen, file_name, &checksum_url)?;
            Ok(sha256)
        }
        // The digest published beside the artifact, at `<url><suffix>`: a bare digest or one
        // `sha256sum` line. A sidecar that is not there is `E_NO_CHECKSUM`, not a fetch error:
        // upstream published an artifact this recipe cannot verify.
        ChecksumSource::Sidecar(suffix) => {
            let sidecar_url = format!("{url}{suffix}");
            let body = fetcher
                .get(&sidecar_url)
                .map_err(|e| sidecar_error(&sidecar_url, e))?;
            let text = String::from_utf8_lossy(&body);
            let first = text.split_whitespace().next().unwrap_or_default();
            let sha256 = sha256sums_lookup(&text, file_name)
                .or_else(|| is_sha256_hex(&first).then(|| first.to_string()))
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E_NO_CHECKSUM",
                        format!("{sidecar_url} carries no SHA-256 for {file_name}"),
                    )
                })?;
            cross_check(&sha256, chosen, file_name, &sidecar_url)?;
            Ok(sha256)
        }
        // The discovery document's own entry for this file said what it hashes to.
        ChecksumSource::Index(key) => {
            let value = chosen.index_fields.get(key).ok_or_else(|| {
                Diagnostic::new(
                    "E_NO_CHECKSUM",
                    format!("the index entry for {file_name} carries no `{key}`"),
                )
                .hint(
                    "the recipe takes the digest from the index that lists the file; Lodi \
                     downloads nothing it cannot verify",
                )
            })?;
            if !is_sha256_hex(&value.as_str()) {
                return Err(Diagnostic::new(
                    "E_NO_CHECKSUM",
                    format!("the index lists `{value}` for {file_name}, not a SHA-256"),
                ));
            }
            Ok(value.clone())
        }
        ChecksumSource::Strategy => {
            let digest = chosen.digest.as_deref().ok_or_else(|| {
                Diagnostic::new(
                    "E_NO_CHECKSUM",
                    format!("{file_name}: upstream publishes no SHA-256 for this asset"),
                )
                .hint(
                    "the recipe takes the digest from the upstream API, which did not give one \
                     for this asset; Lodi downloads nothing it cannot verify",
                )
            })?;
            let hex = digest
                .strip_prefix("sha256:")
                .filter(is_sha256_hex)
                .ok_or_else(|| {
                    Diagnostic::new(
                        "E_NO_CHECKSUM",
                        format!("{file_name}: upstream published `{digest}`, not a sha256: digest"),
                    )
                })?;
            Ok(hex.to_string())
        }
    }
}

/// Where the strategy also carries a digest, the two must agree (`github_assets`).
fn cross_check(
    sha256: &str,
    chosen: &Candidate,
    file_name: &str,
    source_url: &str,
) -> Result<(), Diagnostic> {
    if let Some(digest) = &chosen.digest
        && digest.strip_prefix("sha256:") != Some(sha256)
    {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{file_name}: the release lists {digest} but {source_url} lists sha256:{sha256}"
            ),
        ));
    }
    Ok(())
}

fn no_match(name: &str, constraint_text: &str, candidates: &[Candidate]) -> Diagnostic {
    let mut versions: Vec<&Candidate> = candidates
        .iter()
        .filter(|c| c.version.pre.is_none() && c.for_arch)
        .collect();
    versions.sort_by(|a, b| b.version.cmp(&a.version));
    let nearest: Vec<&str> = versions.iter().take(5).map(|c| c.text.as_str()).collect();
    let d = Diagnostic::new(
        "E_NO_MATCH",
        format!("no {name} version upstream matches `{constraint_text}`"),
    );
    if nearest.is_empty() {
        d.hint("upstream lists no versions")
    } else {
        d.hint(format!("available: {}", nearest.join(", ")))
    }
}

/// Whether `hex` is a SHA-256 digest as the lock records one: 64 lower-case hex digits.
fn is_sha256_hex(hex: &&str) -> bool {
    hex.len() == 64
        && hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The hex digest for `file` in `sha256sum` output (`<hex>  <name>` or `<hex> *<name>`). The
/// name must be exactly the one the file lists: an entry for another directory's file of the
/// same name (`<hex>  sub/<name>`) is not this file's digest (M-1.0 u-4, LD-363).
pub fn sha256sums_lookup(text: &str, file: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (hash, name) = line.trim().split_once(char::is_whitespace)?;
        let name = name.trim_start();
        let name = name.strip_prefix('*').unwrap_or(name);
        (name == file && crate::debian::index::is_sha256_hex(hash)).then(|| hash.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sidecar that answers with a status is `E_NO_CHECKSUM` as before, when the status was
    /// tried again until the attempts ran out too, and says so; anything else is `E_FETCH`.
    #[test]
    fn a_sidecar_error_keeps_its_code_when_it_was_tried_again() {
        use crate::fetch::FetchError;
        let url = "https://dl.test/w.sha256";
        let d = sidecar_error(url, FetchError::Status(url.into(), 404));
        assert_eq!(d.code, "E_NO_CHECKSUM");
        assert_eq!(d.message, format!("{url} is not published (HTTP 404)"));
        let tried = FetchError::Exhausted(10, Box::new(FetchError::Status(url.into(), 503)));
        let d = sidecar_error(url, tried);
        assert_eq!(d.code, "E_NO_CHECKSUM");
        assert_eq!(
            d.message,
            format!("{url} is not published (HTTP 503; gave up after 10 attempts)")
        );
        let cut = FetchError::Transport(url.into(), "stalled".into());
        let d = sidecar_error(url, FetchError::Exhausted(10, Box::new(cut)));
        assert_eq!(d.code, "E_FETCH");
        assert_eq!(
            d.message,
            format!("{url}: stalled; gave up after 10 attempts")
        );
    }

    #[test]
    fn sha256sums() {
        let text = format!(
            "{a}  node-v22.1.0-linux-x64.tar.xz\n{b} *cpython-3.12.1+1-x.tar.gz\n{b}  sub/dir/f.tgz\n",
            a = "a".repeat(64),
            b = "b".repeat(64)
        );
        assert_eq!(
            sha256sums_lookup(&text, "node-v22.1.0-linux-x64.tar.xz"),
            Some("a".repeat(64))
        );
        assert_eq!(
            sha256sums_lookup(&text, "cpython-3.12.1+1-x.tar.gz"),
            Some("b".repeat(64))
        );
        assert_eq!(
            sha256sums_lookup(&text, "sub/dir/f.tgz"),
            Some("b".repeat(64))
        );
        assert_eq!(sha256sums_lookup(&text, "node-v22.1.0-linux-x64.tar"), None);
    }

    #[test]
    fn a_checksum_entry_matching_only_by_basename_is_not_the_files_digest() {
        let text = format!(
            "{a}  other/node-v22.1.0-linux-x64.tar.xz\n{b}  ./cpython.tar.gz\n{c} **f.tgz\n",
            a = "a".repeat(64),
            b = "b".repeat(64),
            c = "c".repeat(64)
        );
        assert_eq!(
            sha256sums_lookup(&text, "node-v22.1.0-linux-x64.tar.xz"),
            None
        );
        assert_eq!(sha256sums_lookup(&text, "cpython.tar.gz"), None);
        assert_eq!(sha256sums_lookup(&text, "f.tgz"), None);
        // The exact entry still wins when the same name appears elsewhere too.
        let both = format!(
            "{text}{d}  node-v22.1.0-linux-x64.tar.xz\n",
            d = "d".repeat(64)
        );
        assert_eq!(
            sha256sums_lookup(&both, "node-v22.1.0-linux-x64.tar.xz"),
            Some("d".repeat(64))
        );
    }
}
