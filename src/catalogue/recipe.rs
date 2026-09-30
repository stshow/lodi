//! Tool recipes: `catalogue/tools/<name>.toml`, parsed and validated (`catalogue/README.md`).
//!
//! The `[versions]` table is the one part a strategy owns: this module looks the strategy up in
//! [`crate::upstream::strategy::STRATEGIES`] and hands the table to it, so a new strategy adds a
//! module and one line there and changes nothing here.

use std::collections::BTreeMap;

use crate::catalogue::{Reader, parse_document};
use crate::diag::Diagnostic;
use crate::upstream::strategy::{self, Discovery};
use crate::util::sha256_tagged;

/// A tool recipe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recipe {
    pub name: String,
    /// Where it came from, as the lock records it: `builtin`, or `user` for a recipe of the
    /// person's own in their repository's `recipes/` folder (LD-409).
    pub input: &'static str,
    /// `[recipe] description`: the one line `lodi search` and `lodi info` show (M-0.4 T-6).
    pub description: String,
    /// `[recipe] homepage`: where the tool itself lives, shown by the same two commands.
    pub homepage: String,
    /// The recipe file as messages name it: `python.toml` for a built-in one, and its path from
    /// the repository's root, `recipes/python.toml`, for one of the person's own.
    pub file: String,
    /// `sha256:` of the recipe text as embedded, recorded in the lock.
    pub sha256: String,
    pub arch_names: BTreeMap<String, String>,
    /// The `[versions] strategy` name, as registered in [`strategy::STRATEGIES`].
    pub strategy: &'static str,
    pub versions: Discovery,
    /// The artifacts of one version, in declaration order. A recipe declares one `[asset]`
    /// table or several `[[asset]]` entries; several are extracted over one another into the
    /// **one** store entry of the tool (M-0.4 T-4, `spec/01` §3.8).
    pub assets: Vec<Asset>,
    pub bin: Vec<String>,
    pub path: Vec<String>,
    /// `[spec] env`: contributed before a declaration's environment is overlaid.
    pub env: BTreeMap<String, String>,
}

/// One artifact of a recipe: where it is, how it is unpacked, and where its digest comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asset {
    /// Download URL template. `None` when the strategy finds the asset itself.
    pub url: Option<String>,
    pub format: String,
    pub strip_components: u32,
    /// The directory inside the artifact to use as its root, after `strip_components`.
    pub subdir: String,
    /// Exact regular-file paths to omit after `strip_components`, before anything is written.
    pub exclude: Vec<String>,
    pub checksum: ChecksumSource,
}

/// Where an artifact's SHA-256 comes from. There is always exactly one source and never a
/// fallback: Lodi downloads nothing it cannot verify (`spec/05` §3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChecksumSource {
    /// `checksum_file`: a `sha256sum`-format file listing the asset, at this URL template.
    File(String),
    /// `checksum_sidecar`: `<asset url><suffix>`, whose body is a bare digest or one
    /// `sha256sum` line (M-0.4 T-4).
    Sidecar(String),
    /// `checksum = "index"`: the discovery document's own entry for this file, under
    /// `checksum_key` (M-0.4 T-4).
    Index(String),
    /// The strategy carries the digest itself (`Strategy::supplies_digest`).
    Strategy,
}

impl Recipe {
    /// The path the lock records: the file name, relative to the catalogue or to `recipes/`.
    pub fn lock_path(&self) -> &str {
        self.file.rsplit('/').next().unwrap_or(&self.file)
    }
}

/// Parse and validate a built-in tool recipe.
pub fn parse_recipe(text: &str, file: &str) -> Result<Recipe, Diagnostic> {
    parse(text, file, "builtin")
}

/// Parse and validate `recipes/<file>` of a repository (LD-409): the same reader as a built-in
/// recipe, and then the catalogue lint's rules that `scripts/check-catalogue.py` holds the
/// built-in ones to before they are embedded — a description and a homepage, every URL HTTPS,
/// and no version literal outside a comment.
pub fn parse_user_recipe(text: &str, file: &str) -> Result<Recipe, Diagnostic> {
    let shown = format!("{}/{file}", crate::catalogue::user::DIR);
    let recipe = parse(text, &shown, "user")?;
    let r = Reader::new(&shown);
    for (key, value) in [
        ("description", &recipe.description),
        ("homepage", &recipe.homepage),
    ] {
        if value.trim().is_empty() {
            return Err(r.invalid(format!("[recipe] {key} is empty")));
        }
    }
    let doc = parse_document(text, &r)?;
    let mut urls = Vec::new();
    urls_in(doc.as_item(), &mut urls);
    if let Some(url) = urls.iter().find(|url| !url.starts_with("https://")) {
        return Err(r.invalid(format!("{url} is not an HTTPS URL")));
    }
    if let Some(line) = text
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with('#') && has_version_literal(line))
    {
        return Err(r.invalid(format!(
            "looks like a version list: `{line}`; a recipe names no version, `lodi lock` \
             discovers it"
        )));
    }
    Ok(recipe)
}

/// Every string of `item` that looks like a URL, as the catalogue lint finds them.
fn urls_in(item: &toml_edit::Item, out: &mut Vec<String>) {
    use toml_edit::{Item, Value};
    fn value(v: &Value, out: &mut Vec<String>) {
        match v {
            Value::String(s) if s.value().contains("://") => out.push(s.value().clone()),
            Value::Array(array) => array.iter().for_each(|v| value(v, out)),
            Value::InlineTable(table) => table.iter().for_each(|(_, v)| value(v, out)),
            _ => {}
        }
    }
    match item {
        Item::Value(v) => value(v, out),
        Item::Table(table) => table.iter().for_each(|(_, item)| urls_in(item, out)),
        Item::ArrayOfTables(array) => array
            .iter()
            .for_each(|table| table.iter().for_each(|(_, item)| urls_in(item, out))),
        Item::None => {}
    }
}

/// Whether `line` holds digits, a dot and digits: the lint's `[0-9]+\.[0-9]+`.
fn has_version_literal(line: &str) -> bool {
    let bytes = line.as_bytes();
    bytes
        .windows(3)
        .any(|w| w[0].is_ascii_digit() && w[1] == b'.' && w[2].is_ascii_digit())
}

fn parse(text: &str, file: &str, input: &'static str) -> Result<Recipe, Diagnostic> {
    let r = Reader::new(file);
    let doc = parse_document(text, &r)?;
    let root = doc.as_table();
    r.keys(
        root,
        "root",
        &["recipe", "arch_names", "versions", "asset", "spec"],
    )?;

    let meta = r.table(root, "recipe")?;
    r.keys(meta, "recipe", &["name", "description", "homepage"])?;
    let name = r.string(meta, "name", "recipe")?;
    let description = r.string(meta, "description", "recipe")?;
    let homepage = r.string(meta, "homepage", "recipe")?;
    if file.rsplit('/').next() != Some(format!("{name}.toml").as_str()) {
        return Err(r.invalid(format!("recipe name `{name}` does not match the file name")));
    }

    let mut arch_names = BTreeMap::new();
    if let Some(table) = root
        .get("arch_names")
        .and_then(toml_edit::Item::as_table_like)
    {
        r.keys(table, "arch_names", &["x86_64", "aarch64"])?;
        for (arch, _) in table.iter() {
            arch_names.insert(arch.to_string(), r.string(table, arch, "arch_names")?);
        }
    }
    let mut known = vec![
        "version",
        "tag",
        "arch",
        "arch_alt",
        "arch_rust",
        "version_major",
        "version_minor",
    ];
    if !arch_names.is_empty() {
        known.push("arch_name");
    }
    let before_version: Vec<&str> = known
        .iter()
        .copied()
        .filter(|v| !["version", "tag", "version_major", "version_minor"].contains(v))
        .collect();

    let versions = r.table(root, "versions")?;
    let strategy_name = r.string(versions, "strategy", "versions")?;
    let Some((registered, strategy)) = strategy::find(&strategy_name) else {
        return Err(r.invalid(format!("unknown version strategy `{strategy_name}`")));
    };
    let discovery = (strategy.parse)(&r, versions, &before_version)?;

    // `[asset]` is one table, or `[[asset]]` is several in declaration order (M-0.4 T-4).
    let assets: Vec<&dyn toml_edit::TableLike> = match root.get("asset") {
        Some(toml_edit::Item::ArrayOfTables(array)) => array
            .iter()
            .map(|t| t as &dyn toml_edit::TableLike)
            .collect(),
        Some(item) => match item.as_table_like() {
            Some(table) => vec![table],
            None => return Err(r.invalid("[asset] must be a table or an array of tables")),
        },
        None => return Err(r.invalid("missing table [asset]")),
    };
    if assets.is_empty() {
        return Err(r.invalid("[[asset]] declares no artifact"));
    }
    if assets.len() > 1 && strategy.asset_url == strategy::AssetUrl::FromStrategy {
        return Err(r.invalid(format!(
            "{registered} finds one asset in the release, so a recipe using it declares one \
             [asset]"
        )));
    }
    let mut parsed = Vec::new();
    for asset in assets {
        parsed.push(parse_asset(&r, asset, &known, strategy, registered)?);
    }

    let spec = r.table(root, "spec")?;
    r.keys(spec, "spec", &["bin", "path", "env"])?;
    let bin = r.strings(spec, "bin", "spec")?;
    let path = r.strings(spec, "path", "spec")?;
    for p in bin.iter().chain(&path) {
        if p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty()) {
            return Err(r.invalid(format!("`{p}` must be a relative path inside the artifact")));
        }
    }
    let env = spec_env(&r, spec)?;

    Ok(Recipe {
        name,
        input,
        description,
        homepage,
        file: file.to_string(),
        sha256: sha256_tagged(text.as_bytes()),
        arch_names,
        strategy: registered,
        versions: discovery,
        assets: parsed,
        bin,
        path,
        env,
    })
}

/// `[spec] env`: an optional table of environment-variable names to values. The names are the
/// ones `spec/01` §1.4 allows, and the **only** substitutions a value may use are
/// `${self.path}` and `${self.version}`, which are resolved at realization — a recipe knows
/// neither the store path nor the chosen version, so nothing else could be substituted here.
fn spec_env(
    r: &Reader,
    spec: &dyn toml_edit::TableLike,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    let mut env = BTreeMap::new();
    let Some(table) = spec.get("env").and_then(toml_edit::Item::as_table_like) else {
        if spec.get("env").is_some() {
            return Err(r.invalid("`env` in [spec] must be a table"));
        }
        return Ok(env);
    };
    for (name, item) in table.iter() {
        if !crate::tools::is_env_name(name) {
            return Err(r.invalid(format!(
                "environment variable name `{name}` in [spec.env] must match [A-Za-z_][A-Za-z0-9_]*"
            )));
        }
        let value = item
            .as_str()
            .ok_or_else(|| r.invalid(format!("`{name}` in [spec.env] must be a string")))?;
        r.check_template(value, &["self.path", "self.version"])?;
        env.insert(name.to_string(), value.to_string());
    }
    Ok(env)
}

/// One `[asset]` (or one `[[asset]]` entry): its URL, its layout and its one hash source.
fn parse_asset(
    r: &Reader,
    asset: &dyn toml_edit::TableLike,
    known: &[&str],
    strategy: &strategy::Strategy,
    registered: &str,
) -> Result<Asset, Diagnostic> {
    r.keys(
        asset,
        "asset",
        &[
            "url",
            "format",
            "strip_components",
            "subdir",
            "exclude",
            "checksum_file",
            "checksum",
            "checksum_key",
            "checksum_sidecar",
        ],
    )?;
    let url = r.opt_string(asset, "url", "asset")?;
    match (strategy.asset_url, &url) {
        (strategy::AssetUrl::FromStrategy, Some(_)) => {
            return Err(r.invalid(format!(
                "`url` in [asset] is taken from the release for {registered}"
            )));
        }
        (strategy::AssetUrl::FromRecipe, None) => {
            return Err(r.invalid(format!("{registered} needs `url` in [asset]")));
        }
        (_, Some(url)) => r.check_template(url, known)?,
        _ => {}
    }
    let format = r.string(asset, "format", "asset")?;
    if !crate::tools::FORMATS.contains(&format.as_str()) {
        return Err(r.invalid(format!("unsupported asset format `{format}`")));
    }
    // A bare binary is installed as `bin/<name>` and has no archive components to strip.
    let strip_components = if format == "binary" && asset.get("strip_components").is_none() {
        0
    } else {
        r.integer(asset, "strip_components", "asset", 8)?
    };
    let subdir = r.opt_string(asset, "subdir", "asset")?.unwrap_or_default();
    if subdir.starts_with('/') || subdir.split('/').any(|c| c == "..") {
        return Err(r.invalid(format!(
            "`subdir` in [asset] must be a relative path inside the artifact, not `{subdir}`"
        )));
    }
    let exclude = match asset.get("exclude") {
        Some(_) => r.strings(asset, "exclude", "asset")?,
        None => Vec::new(),
    };
    for (nth, path) in exclude.iter().enumerate() {
        let components: Vec<&str> = path.split('/').collect();
        if path.is_empty()
            || path.starts_with('/')
            || path.ends_with('/')
            || components
                .iter()
                .any(|part| part.is_empty() || *part == "." || *part == "..")
            || path.contains(['*', '?', '[', ']', '{', '}'])
        {
            return Err(r.invalid(format!(
                "`exclude` entry `{path}` in [asset] must be an exact relative file path"
            )));
        }
        if exclude[..nth].contains(path) {
            return Err(r.invalid(format!(
                "`exclude` entry `{path}` appears more than once in [asset]"
            )));
        }
    }

    // A hash source is required, and there is exactly one: a checksum file the recipe names, a
    // sidecar beside the artifact, the discovery document's own entry, or a strategy that
    // carries the digest. Lodi never downloads an artifact it cannot verify.
    let checksum_file = r.opt_string(asset, "checksum_file", "asset")?;
    let sidecar = r.opt_string(asset, "checksum_sidecar", "asset")?;
    let checksum = r.opt_string(asset, "checksum", "asset")?;
    let checksum_key = r.opt_string(asset, "checksum_key", "asset")?;
    if let Some(value) = &checksum
        && value != "index"
    {
        return Err(r.invalid(format!(
            "`checksum` in [asset] is `index` or nothing, not `{value}`"
        )));
    }
    if checksum.is_some() != checksum_key.is_some() {
        return Err(r.invalid("`checksum = \"index\"` and `checksum_key` in [asset] go together"));
    }
    let declared: Vec<&str> = [
        checksum_file.as_ref().map(|_| "checksum_file"),
        sidecar.as_ref().map(|_| "checksum_sidecar"),
        checksum.as_ref().map(|_| "checksum"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if declared.len() > 1 {
        return Err(r.invalid(format!(
            "[asset] declares more than one hash source ({}); it has exactly one",
            declared.join(", ")
        )));
    }
    if strategy.supplies_digest && !declared.is_empty() {
        return Err(r.invalid(format!(
            "{} in [asset] is taken from the API for {registered}",
            declared.join(", ")
        )));
    }
    let source = if let Some(template) = checksum_file {
        r.check_template(&template, known)?;
        ChecksumSource::File(template)
    } else if let Some(suffix) = sidecar {
        if suffix.is_empty() {
            return Err(r.invalid("`checksum_sidecar` in [asset] must not be empty"));
        }
        ChecksumSource::Sidecar(suffix)
    } else if let Some(key) = checksum_key {
        ChecksumSource::Index(key)
    } else if strategy.supplies_digest {
        ChecksumSource::Strategy
    } else {
        return Err(r.invalid(format!(
            "no hash source: [asset] needs `checksum_file`, `checksum_sidecar` or \
             `checksum = \"index\"`, because {registered} does not supply the artifact's digest \
             itself"
        )));
    };

    Ok(Asset {
        url,
        format,
        strip_components,
        subdir,
        exclude,
        checksum: source,
    })
}
