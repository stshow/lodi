//! `strategy = "github_assets"`: versions are captured out of the asset file names of the newest
//! releases of a GitHub repository, and the download URL and digest come from the release.

use std::collections::BTreeMap;

use crate::catalogue::{Reader, render};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::upstream::json;
use crate::upstream::strategy::{AssetUrl, Candidate, Context, Discovery, Strategy, capture};
use crate::version::Version;
use toml_edit::TableLike;

pub const STRATEGY: Strategy = Strategy {
    parse,
    discover,
    asset_url: AssetUrl::FromStrategy,
    supplies_digest: false,
};

fn parse(
    r: &Reader,
    versions: &dyn TableLike,
    before_version: &[&str],
) -> Result<Discovery, Diagnostic> {
    r.keys(
        versions,
        "versions",
        &["strategy", "repo", "releases", "asset"],
    )?;
    let asset = r.string(versions, "asset", "versions")?;
    let mut vars = before_version.to_vec();
    vars.extend(["version", "tag"]);
    r.check_template(&asset, &vars)?;
    if !asset.contains("${version}") {
        return Err(r.invalid("`asset` in [versions] must contain ${version}"));
    }
    Ok(Discovery::GithubAssets {
        repo: r.string(versions, "repo", "versions")?,
        releases: r.integer(versions, "releases", "versions", 10)?.max(1),
        asset,
    })
}

fn discover(
    fetcher: &dyn Fetcher,
    discovery: &Discovery,
    ctx: &Context,
) -> Result<Vec<Candidate>, Diagnostic> {
    let Discovery::GithubAssets {
        repo,
        releases,
        asset: asset_template,
    } = discovery
    else {
        return Err(ctx.invalid("github_assets was asked to discover another strategy"));
    };
    let releases = *releases;
    let url = format!("https://api.github.com/repos/{repo}/releases?per_page={releases}");
    let body = fetcher.get(&url).map_err(crate::upstream::fetch_error)?;
    let list = json(&body, &url)?;
    let shape = || {
        Diagnostic::new(
            "E_RECIPE_CTX",
            format!("{url}: unexpected release list shape"),
        )
    };
    let mut out: Vec<Candidate> = Vec::new();
    for release in list
        .as_array()
        .ok_or_else(shape)?
        .iter()
        .take(releases as usize)
    {
        if release["draft"].as_bool() == Some(true) || release["prerelease"].as_bool() == Some(true)
        {
            continue;
        }
        let tag = release["tag_name"].as_str().ok_or_else(shape)?;
        // The tag is interpolated into URLs: accept only a plain name.
        let plain = !tag.is_empty()
            && !tag.starts_with('.')
            && tag
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b));
        if !plain {
            continue;
        }
        let mut with_tag = ctx.vars.to_vec();
        with_tag.push(("tag", tag));
        with_tag.push(("version", "${version}"));
        let template = render(asset_template, &with_tag).map_err(|e| ctx.invalid(e))?;
        let download_prefix = format!("https://github.com/{repo}/releases/download/");
        for asset in release["assets"].as_array().ok_or_else(shape)? {
            let (Some(name), Some(url)) = (
                asset["name"].as_str(),
                asset["browser_download_url"].as_str(),
            ) else {
                return Err(shape());
            };
            let Some(text) = capture(&template, name) else {
                continue;
            };
            let Ok(version) = Version::parse(&text) else {
                continue;
            };
            if !url.starts_with(&download_prefix) || out.iter().any(|c| c.text == text) {
                continue;
            }
            out.push(Candidate {
                version,
                text,
                tag: Some(tag.to_string()),
                asset: Some((name.to_string(), url.to_string())),
                size: asset["size"].as_u64(),
                digest: asset["digest"].as_str().map(str::to_string),
                for_arch: true,
                index_fields: BTreeMap::new(),
            });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_capture() {
        let t = "cpython-${version}+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz";
        assert_eq!(
            capture(
                t,
                "cpython-3.12.14+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz"
            ),
            Some("3.12.14".into())
        );
        for other in [
            "cpython-3.12.14+20260901-x86_64-unknown-linux-gnu-install_only_stripped.tar.gz",
            "cpython-3.15.0rc1+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz",
            "cpython-3.13.1+20260901-x86_64-unknown-linux-musl-install_only.tar.gz",
            "cpython-+20260901-x86_64-unknown-linux-gnu-install_only.tar.gz",
        ] {
            assert_eq!(capture(t, other), None, "{other}");
        }
    }
}
