//! `strategy = "github_releases"`: the version is the **release tag**, matched against a tag
//! template, and the asset is chosen out of that release by its file name.
//!
//! This is the sibling of [`github_assets`](super::github_assets), and the difference is where
//! the version comes from. `github_assets` captures it out of an asset's file name, which is
//! what python-build-standalone needs because one release carries many versions. Most projects
//! publish one version per release and encode it in the tag, so here the tag is matched against
//! `tag` (`v${version}` by default), the asset file name is *rendered* from `asset` once the
//! version and the tag are known, and a tag the template does not match is simply not a version
//! — never a mis-parse (LD-80: no regular expressions, anywhere).
//!
//! The hash source is the API's own per-asset `sha256:` digest, so a recipe using this strategy
//! declares no `checksum_file`; an asset the API publishes without a digest is `E_NO_CHECKSUM`
//! in the resolver. `Authorization` is sent from `GITHUB_TOKEN` when that variable is set, and
//! the value is handed to the transport and never logged, recorded or put in a diagnostic. A
//! refusal (403) or a rate limit (429) is `E_FETCH` with a hint naming `GITHUB_TOKEN` and the
//! reset header: there is no fallback to `releases.atom`, to a mirror or to a retry loop.

use std::collections::BTreeMap;

use crate::catalogue::{Reader, render};
use crate::diag::Diagnostic;
use crate::fetch::{FetchError, Fetcher};
use crate::upstream::json;
use crate::upstream::strategy::{AssetUrl, Candidate, Context, Discovery, Strategy, capture};
use crate::version::Version;
use toml_edit::TableLike;

/// One page of the releases API, as `docs/milestones/m-0.4/TASKS.md` T-3 specifies it. The
/// recipe's `releases` says how many of those to *consider*; the page size is fixed so that a
/// repository whose newest entries are drafts or pre-releases still offers real ones.
const PER_PAGE: u32 = 30;
/// The default tag template: `v1.2.3`, which is what most projects publish.
const DEFAULT_TAG: &str = "v${version}";
/// The default number of newest releases to consider.
const DEFAULT_RELEASES: u32 = 3;
/// The largest number a recipe may ask for, bounded by the one page this strategy fetches.
const MAX_RELEASES: i64 = 10;

pub const STRATEGY: Strategy = Strategy {
    parse,
    discover,
    asset_url: AssetUrl::FromStrategy,
    supplies_digest: true,
};

fn opt_integer(
    r: &Reader,
    table: &dyn TableLike,
    key: &str,
    at: &str,
    range: std::ops::RangeInclusive<i64>,
    default: u32,
) -> Result<u32, Diagnostic> {
    match table.get(key) {
        None => Ok(default),
        Some(item) => match item.as_integer() {
            Some(n) if range.contains(&n) => Ok(n as u32),
            _ => Err(r.invalid(format!(
                "`{key}` in [{at}] must be an integer from {} to {}",
                range.start(),
                range.end()
            ))),
        },
    }
}

fn opt_bool(
    r: &Reader,
    table: &dyn TableLike,
    key: &str,
    at: &str,
    default: bool,
) -> Result<bool, Diagnostic> {
    match table.get(key) {
        None => Ok(default),
        Some(item) => item
            .as_bool()
            .ok_or_else(|| r.invalid(format!("`{key}` in [{at}] must be true or false"))),
    }
}

fn parse(
    r: &Reader,
    versions: &dyn TableLike,
    before_version: &[&str],
) -> Result<Discovery, Diagnostic> {
    r.keys(
        versions,
        "versions",
        &[
            "strategy",
            "repo",
            "releases",
            "tag",
            "prereleases",
            "asset",
        ],
    )?;
    let repo = r.string(versions, "repo", "versions")?;
    // The repository is interpolated into an API URL and into the download prefix the release's
    // own asset URLs are checked against: accept `owner/name` and nothing else.
    let mut parts = repo.split('/');
    let plain = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    match (parts.next(), parts.next(), parts.next()) {
        (Some(owner), Some(name), None) if plain(owner) && plain(name) => {}
        _ => {
            return Err(r.invalid(format!(
                "`repo` in [versions] must be `owner/name`, not `{repo}`"
            )));
        }
    }

    let tag = match r.opt_string(versions, "tag", "versions")? {
        Some(tag) => tag,
        None => DEFAULT_TAG.to_string(),
    };
    r.check_template(&tag, &["version"])?;
    if tag.matches("${version}").count() != 1 {
        return Err(r.invalid("`tag` in [versions] must contain ${version} exactly once"));
    }

    let asset = r.string(versions, "asset", "versions")?;
    let mut vars = before_version.to_vec();
    vars.extend(["version", "tag"]);
    r.check_template(&asset, &vars)?;

    Ok(Discovery::GithubReleases {
        repo,
        releases: opt_integer(
            r,
            versions,
            "releases",
            "versions",
            1..=MAX_RELEASES,
            DEFAULT_RELEASES,
        )?,
        tag,
        prereleases: opt_bool(r, versions, "prereleases", "versions", false)?,
        asset,
    })
}

/// `E_FETCH`, with the `GITHUB_TOKEN` hint when GitHub refused or rate-limited the request.
/// There is no other branch: Lodi never tries a second source for a version list.
fn fetch_failed(e: FetchError) -> Diagnostic {
    let rate_limited = matches!(&e, FetchError::Status(_, 403 | 429));
    let d = Diagnostic::new("E_FETCH", e.to_string());
    if rate_limited {
        d.hint(
            "GitHub refuses or rate-limits anonymous callers: set GITHUB_TOKEN to a token with \
             public read access, or retry after the time named by the response's \
             `x-ratelimit-reset` header",
        )
    } else {
        d
    }
}

fn discover(
    fetcher: &dyn Fetcher,
    discovery: &Discovery,
    ctx: &Context,
) -> Result<Vec<Candidate>, Diagnostic> {
    let Discovery::GithubReleases {
        repo,
        releases,
        tag: tag_template,
        prereleases,
        asset: asset_template,
    } = discovery
    else {
        return Err(ctx.invalid("github_releases was asked to discover another strategy"));
    };
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page={PER_PAGE}");

    // The token is read here and handed straight to the transport. It is never logged, put in a
    // diagnostic or written to the lock; an unset or blank variable simply means no header.
    let mut headers: Vec<(&str, String)> = vec![("Accept", "application/vnd.github+json".into())];
    if let Some(token) = std::env::var("GITHUB_TOKEN")
        .ok()
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
    {
        headers.push(("Authorization", format!("Bearer {token}")));
    }

    let body = fetcher
        .get_with_headers(&url, &headers)
        .map_err(fetch_failed)?;
    let list = json(&body, &url)?;
    let shape = || {
        Diagnostic::new(
            "E_RECIPE_CTX",
            format!("{url}: unexpected release list shape"),
        )
    };
    let download_prefix = format!("https://github.com/{repo}/releases/download/");

    let mut out: Vec<Candidate> = Vec::new();
    let mut considered = 0;
    for release in list.as_array().ok_or_else(shape)? {
        if release["draft"].as_bool() == Some(true) {
            continue;
        }
        if !prereleases && release["prerelease"].as_bool() == Some(true) {
            continue;
        }
        if considered >= *releases {
            break;
        }
        considered += 1;
        let tag = release["tag_name"].as_str().ok_or_else(shape)?;
        // A tag the template does not match is not a version of this tool: ignore it. This is
        // where `nightly` and `v1.2.3-rc1` are left alone instead of being mis-parsed.
        let Some(text) = capture(tag_template, tag) else {
            continue;
        };
        let Ok(version) = Version::parse(&text) else {
            continue;
        };
        if out.iter().any(|c| c.text == text) {
            continue;
        }
        let mut vars = ctx.vars.to_vec();
        vars.push(("tag", tag));
        vars.push(("version", &text));
        let wanted = render(asset_template, &vars).map_err(|e| ctx.invalid(e))?;

        let mut found = None;
        for asset in release["assets"].as_array().ok_or_else(shape)? {
            let (Some(name), Some(asset_url)) = (
                asset["name"].as_str(),
                asset["browser_download_url"].as_str(),
            ) else {
                return Err(shape());
            };
            if name != wanted || !asset_url.starts_with(&download_prefix) {
                continue;
            }
            found = Some((
                name.to_string(),
                asset_url.to_string(),
                asset["size"].as_u64(),
                asset["digest"].as_str().map(str::to_string),
            ));
            break;
        }
        match found {
            Some((name, asset_url, size, digest)) => out.push(Candidate {
                version,
                text,
                tag: Some(tag.to_string()),
                asset: Some((name, asset_url)),
                size,
                digest,
                for_arch: true,
                index_fields: BTreeMap::new(),
            }),
            // The release exists but publishes nothing under that name: the version is real and
            // this architecture's build is missing, which is `E_UNSUPPORTED_ARCH`, not silence.
            None => out.push(Candidate {
                version,
                text,
                tag: Some(tag.to_string()),
                asset: None,
                size: None,
                digest: None,
                for_arch: false,
                index_fields: BTreeMap::new(),
            }),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tag_is_a_version_only_when_the_template_matches_it() {
        assert_eq!(capture("v${version}", "v10.5.0"), Some("10.5.0".into()));
        assert_eq!(capture("${version}", "15.2.0"), Some("15.2.0".into()));
        assert_eq!(capture("jq-${version}", "jq-1.8.2"), Some("1.8.2".into()));
        for other in ["nightly", "v1.2.3-rc1", "latest", "v", "release-v1.2.3"] {
            assert_eq!(capture("v${version}", other), None, "{other}");
        }
        assert_eq!(capture("${version}", "nightly"), None);
        assert_eq!(capture("${version}", "v1.2.3-rc1"), None);
    }
}
