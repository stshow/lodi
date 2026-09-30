//! `strategy = "json_index"`: a JSON array upstream publishes, one entry per version, with an
//! optional per-platform build list that says whether a version exists for this architecture.
//!
//! The build list is an array of strings, or — when the recipe sets `file_key` — an array of
//! **tables**, compared on that key (M-0.4 T-4, go.dev's shape). A matched table's string
//! fields travel with the candidate as `index_fields`, which is where a recipe whose
//! `[asset] checksum = "index"` takes the artifact's digest from.

use std::collections::BTreeMap;

use crate::catalogue::Reader;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::upstream::json;
use crate::upstream::strategy::{AssetUrl, Candidate, Context, Discovery, Strategy};
use crate::version::Version;
use toml_edit::TableLike;

pub const STRATEGY: Strategy = Strategy {
    parse,
    discover,
    asset_url: AssetUrl::FromRecipe,
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
        &[
            "strategy",
            "url",
            "version_key",
            "strip_prefix",
            "files_key",
            "file",
            "file_key",
        ],
    )?;
    let url = r.string(versions, "url", "versions")?;
    r.check_template(&url, before_version)?;
    let file = r.opt_string(versions, "file", "versions")?;
    if let Some(file) = &file {
        // `file` is rendered once per entry, after that entry's version is known, so it may
        // name the version too -- an upstream whose build list carries whole file names
        // (go.dev) needs it (M-0.4 T-4).
        let mut known: Vec<&str> = before_version.to_vec();
        known.extend(["version", "version_major", "version_minor"]);
        r.check_template(file, &known)?;
    }
    let files_key = r.opt_string(versions, "files_key", "versions")?;
    if files_key.is_some() != file.is_some() {
        return Err(r.invalid("`files_key` and `file` in [versions] go together"));
    }
    let file_key = r.opt_string(versions, "file_key", "versions")?;
    if file_key.is_some() && files_key.is_none() {
        return Err(r.invalid("`file_key` in [versions] needs `files_key` and `file`"));
    }
    Ok(Discovery::JsonIndex {
        url,
        version_key: r.string(versions, "version_key", "versions")?,
        strip_prefix: r
            .opt_string(versions, "strip_prefix", "versions")?
            .unwrap_or_default(),
        files_key,
        file,
        file_key,
    })
}

fn discover(
    fetcher: &dyn Fetcher,
    discovery: &Discovery,
    ctx: &Context,
) -> Result<Vec<Candidate>, Diagnostic> {
    let Discovery::JsonIndex {
        url,
        version_key,
        strip_prefix,
        files_key,
        file,
        file_key,
    } = discovery
    else {
        return Err(ctx.invalid("json_index was asked to discover another strategy"));
    };
    let url = ctx.render(url)?;
    let files_key = files_key.as_deref();

    let body = fetcher.get(&url).map_err(crate::upstream::fetch_error)?;
    let list = json(&body, &url)?;
    let entries = list
        .as_array()
        .ok_or_else(|| Diagnostic::new("E_RECIPE_CTX", format!("{url}: expected a JSON array")))?;
    let mut out = Vec::new();
    for entry in entries {
        let Some(raw) = entry[version_key].as_str() else {
            continue;
        };
        let text = raw.strip_prefix(strip_prefix).unwrap_or(raw).to_string();
        let Ok(version) = Version::parse(&text) else {
            continue;
        };
        // The build list is matched against the file name of *this* entry's version.
        let major = version.parts.first().map_or(String::new(), u64::to_string);
        let minor = version.parts.get(1).map_or(String::new(), u64::to_string);
        let mut vars = ctx.vars.to_vec();
        vars.push(("version", &text));
        vars.push(("version_major", &major));
        vars.push(("version_minor", &minor));
        let file = match file {
            Some(template) => {
                Some(crate::catalogue::render(template, &vars).map_err(|e| ctx.invalid(e))?)
            }
            None => None,
        };
        let file = file.as_deref();
        let mut index_fields = BTreeMap::new();
        let for_arch = match (files_key, file) {
            (Some(key), Some(file)) => match (entry[key].as_array(), file_key.as_deref()) {
                // The upstream's build list is an array of tables: the entry whose `file_key`
                // is the rendered file name is the build, and what it says about that file --
                // its digest above all -- is kept for the resolver.
                (Some(files), Some(file_key)) => {
                    match files.iter().find(|f| f[file_key].as_str() == Some(file)) {
                        Some(found) => {
                            if let Some(table) = found.as_object() {
                                for (k, v) in table {
                                    if let Some(v) = v.as_str() {
                                        index_fields.insert(k.clone(), v.to_string());
                                    }
                                }
                            }
                            true
                        }
                        None => false,
                    }
                }
                (Some(files), None) => files.iter().any(|f| f.as_str() == Some(file)),
                (None, _) => false,
            },
            _ => true,
        };
        out.push(Candidate {
            version,
            text,
            tag: None,
            asset: None,
            size: None,
            digest: None,
            for_arch,
            index_fields,
        });
    }
    Ok(out)
}
