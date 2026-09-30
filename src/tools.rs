//! The `[tools]` contract of design `spec/01` §3.8, implemented once and scope-independently.
//!
//! This module knows **nothing** about which scope's file a `[tools]` table came from: it reads
//! no file, names no manifest, and takes the caller's diagnostic sink instead of owning one, so
//! each scope keeps its own file name, line and column (design call D11). The project manifest
//! (`crate::manifest`) is one caller; the 0.5 home scope is meant to be the next.
//!
//! Two halves:
//!
//! - [`parse`] validates a table-like node into a [`ToolSet`]. Everything decidable by reading
//!   the declaration alone is decided here, and every refusal is an error rather than a default:
//!   an inline tool (one with `url`) whose `version` is not an exact version is
//!   `E_INLINE_NEEDS_EXACT`, an archive format this build cannot decode is `E_UNSUPPORTED`, a
//!   tool name outside `spec/01` §1.4 is `E_IDENT`.
//! - [`resolve`] (and [`resolve_one`], the per-tool form the lock's freshness check uses) turns
//!   a declaration into a [`LockedTool`], through a catalogue recipe **or** the inline form. It
//!   is the only place that decides between the two.
//!
//! Per-architecture tables (`spec/01` §6.3) override any other key of the same table for that
//! architecture. They are parsed, validated and recorded for every architecture; resolution runs
//! for the host architecture only (OD-14, `AGENTS.md` §9.3a), so a tool declared **only** for
//! another architecture is `E_UNSUPPORTED_ARCH` — or, when it is `optional`, a
//! `W_OPTIONAL_SKIPPED` warning and an entry left out of the lock.

use std::collections::BTreeMap;
use std::ops::Range;

use toml_edit::{Item, Key, Value};

use crate::catalogue::Recipe;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::upstream::{LockedArtifact, LockedTool, resolve_tool};
use crate::util::sha256_tagged;
use crate::version::{Constraint, Version};

/// The architectures a per-architecture table may name (`spec/01` §6.3).
pub const ARCHES: &[&str] = &["x86_64", "aarch64"];

/// The archive formats this build decodes (design call D7/LD-…): everything else is
/// `E_UNSUPPORTED`, because adding a decoder is adding an audited crate to a static binary.
pub const FORMATS: &[&str] = &["tar.gz", "tar.xz", "binary"];

/// Archive extensions this build recognizes but cannot decode. A URL ending in one of them is
/// refused rather than guessed at as a single binary.
const UNDECODABLE: &[&str] = &[
    ".zip", ".tar.bz2", ".tbz2", ".tar.zst", ".tzst", ".tar", ".7z", ".rar", ".gz", ".xz", ".bz2",
    ".zst",
];

/// Every key `spec/01` §3.8 defines that this build implements.
pub const TOOL_KEYS: &[&str] = &[
    "version",
    "name",
    "optional",
    "env",
    "path",
    "x86_64",
    "aarch64",
    "url",
    "sha256",
    "format",
    "strip_components",
    "subdir",
    "bin",
];

/// The largest `strip_components` this build accepts.
const MAX_STRIP: i64 = 8;

/// Tool and package names (`spec/01` §1.4).
pub fn is_package_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_lowercase() || b.is_ascii_digit())
        && s.len() <= 128
        && bytes.all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'+' | b'-')
        })
}

/// Environment variable names (`spec/01` §1.4).
pub fn is_env_name(s: &str) -> bool {
    let mut bytes = s.bytes();
    matches!(bytes.next(), Some(b) if b.is_ascii_alphabetic() || b == b'_')
        && s.len() <= 256
        && bytes.all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// Profile, task, module, input and variable names (`spec/01` §1.4).
pub fn is_ident(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic() || c == '_')
        && s.len() <= 64
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// A name that `spec/01` §2.2 makes available to `${ }`.
pub fn is_substitution_name(name: &str) -> bool {
    const FIXED: &[&str] = &[
        "host.arch",
        "host.os",
        "host.distro",
        "host.release",
        "host.codename",
        "project.root",
        "project.name",
        "lodi.version",
        "container.distro",
        "container.release",
        "container.snapshot",
        "container.arch",
        "self.path",
        "self.version",
    ];
    if FIXED.contains(&name) {
        return true;
    }
    let parts: Vec<&str> = name.split('.').collect();
    match parts.as_slice() {
        ["input", input, "path" | "rev"] => is_ident(input),
        ["tools", tool, "path" | "version"] => is_package_name(tool),
        _ => false,
    }
}

/// Apply `spec/01` §1.3 to one string: `$${` is a literal `${`; a bare `$NAME` is left for the
/// shell; `${ }` substitution is not implemented by this build, so a valid name is
/// `E_UNSUPPORTED` (`E_VAR_UNSET` for `vars.*`, since `[vars]` cannot be declared) and anything
/// else inside `${ }` is `E_EXCLUDED_CONSTRUCT`. The rule belongs to no scope, so it lives here
/// with the rest of the shared contract; the caller attaches its own location to each
/// diagnostic. A tool's `env` is the one exception and is checked against `${self.path}` and
/// `${self.version}` instead, because those are substituted at realization.
pub fn substitute(s: &str) -> (String, Vec<Diagnostic>) {
    let mut out = String::with_capacity(s.len());
    let mut problems = Vec::new();
    let mut rest = s;
    while let Some(i) = rest.find('$') {
        out.push_str(&rest[..i]);
        let tail = &rest[i..];
        if let Some(after) = tail.strip_prefix("$${") {
            out.push_str("${");
            rest = after;
        } else if let Some(after) = tail.strip_prefix("${") {
            let Some(end) = after.find('}') else {
                problems.push(Diagnostic::new(
                    "E_EXCLUDED_CONSTRUCT",
                    "unterminated `${` in a string; write `$${` for a literal `${`",
                ));
                return (out, problems);
            };
            let name = &after[..end];
            problems.push(if name.strip_prefix("vars.").is_some_and(is_ident) {
                Diagnostic::new(
                    "E_VAR_UNSET",
                    format!("`${{{name}}}` names an undeclared variable"),
                )
                .hint("[vars] is not supported by this build")
            } else if is_substitution_name(name) {
                Diagnostic::new(
                    "E_UNSUPPORTED",
                    format!("`${{{name}}}` substitution is not supported by this build"),
                )
                .hint("write the value literally, or `$${` for a literal `${`")
            } else {
                Diagnostic::new(
                    "E_EXCLUDED_CONSTRUCT",
                    format!("`${{{name}}}` is not a substitution name"),
                )
                .hint("there are no expressions: write the value, or `$${` for a literal `${`")
            });
            rest = &after[end + 1..];
        } else {
            out.push('$');
            rest = &tail[1..];
        }
    }
    out.push_str(rest);
    (out, problems)
}

/// Where a caller's diagnostics go. The caller owns the file name and the text, so it — not this
/// module — turns a span into a location; a scope that has no text at all can ignore the span.
pub trait Sink {
    /// Record `d`, which came from `span` in the caller's own document.
    fn emit(&mut self, d: Diagnostic, span: Option<Range<usize>>);
}

/// One declaration's keys, each present only when the declaration wrote it. A per-architecture
/// table is the same shape, and overrides the base table key by key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Fields {
    /// The constraint as written and as parsed.
    pub version: Option<(String, Constraint)>,
    pub name: Option<String>,
    pub optional: Option<bool>,
    pub env: Option<BTreeMap<String, String>>,
    pub path: Option<Vec<String>>,
    pub bin: Option<Vec<String>>,
    pub url: Option<String>,
    /// 64 lower-case hex, without a `sha256:` tag.
    pub sha256: Option<String>,
    pub format: Option<String>,
    pub strip_components: Option<u32>,
    pub subdir: Option<String>,
}

impl Fields {
    /// `other`'s present keys over this table's (`spec/01` §6.3).
    fn overlaid(&self, other: &Fields) -> Fields {
        macro_rules! pick {
            ($f:ident) => {
                other.$f.clone().or_else(|| self.$f.clone())
            };
        }
        Fields {
            version: pick!(version),
            name: pick!(name),
            optional: pick!(optional),
            env: pick!(env),
            path: pick!(path),
            bin: pick!(bin),
            url: pick!(url),
            sha256: pick!(sha256),
            format: pick!(format),
            strip_components: pick!(strip_components),
            subdir: pick!(subdir),
        }
    }

    /// A deterministic rendering of the keys that were written, for the declaration digest.
    fn write_into(&self, out: &mut String) {
        let mut put = |key: &str, value: String| {
            out.push_str(key);
            out.push('=');
            out.push_str(&value);
            out.push('\n');
        };
        if let Some((text, _)) = &self.version {
            put("version", text.clone());
        }
        if let Some(v) = &self.name {
            put("name", v.clone());
        }
        if let Some(v) = self.optional {
            put("optional", v.to_string());
        }
        if let Some(env) = &self.env {
            for (k, v) in env {
                put(&format!("env.{k}"), v.clone());
            }
        }
        if let Some(v) = &self.path {
            put("path", v.join(":"));
        }
        if let Some(v) = &self.bin {
            put("bin", v.join(":"));
        }
        if let Some(v) = &self.url {
            put("url", v.clone());
        }
        if let Some(v) = &self.sha256 {
            put("sha256", v.clone());
        }
        if let Some(v) = &self.format {
            put("format", v.clone());
        }
        if let Some(v) = self.strip_components {
            put("strip_components", v.to_string());
        }
        if let Some(v) = &self.subdir {
            put("subdir", v.clone());
        }
    }
}

/// One `[tools]` entry: the base table and its per-architecture overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDecl {
    /// The base table's constraint as written (`"latest"` when absent).
    pub constraint_text: String,
    pub constraint: Constraint,
    pub base: Fields,
    /// Per-architecture tables by architecture name, a subset of [`ARCHES`].
    pub arch: BTreeMap<String, Fields>,
    /// True when nothing but `version` was declared — the shape every build before M-0.4 T-2
    /// accepted. Such an entry's lock bytes are unchanged by this package.
    pub simple: bool,
}

impl ToolDecl {
    /// A declaration that is nothing but a version constraint, the form an ad-hoc request
    /// (`lodi shell TOOL[@CONSTRAINT]`) produces.
    pub fn simple(constraint_text: String, constraint: Constraint) -> ToolDecl {
        ToolDecl {
            base: Fields {
                version: Some((constraint_text.clone(), constraint.clone())),
                ..Fields::default()
            },
            constraint_text,
            constraint,
            arch: BTreeMap::new(),
            simple: true,
        }
    }

    /// The declaration as it applies to `arch`, with the defaults of `spec/01` §3.8 filled in.
    pub fn effective(&self, arch: &str) -> Effective {
        let merged = match self.arch.get(arch) {
            Some(over) => self.base.overlaid(over),
            None => self.base.clone(),
        };
        let (constraint_text, constraint) = merged
            .version
            .clone()
            .unwrap_or_else(|| ("latest".to_string(), Constraint::Latest));
        Effective {
            constraint_text,
            constraint,
            name: merged.name.clone(),
            optional: merged.optional.unwrap_or(false),
            env: merged.env.clone().unwrap_or_default(),
            path: merged
                .path
                .clone()
                .unwrap_or_else(|| vec!["bin".to_string()]),
            bin: merged.bin.clone(),
            url: merged.url.clone(),
            sha256: merged.sha256.clone(),
            format: merged.format.clone(),
            strip_components: merged.strip_components.unwrap_or(0),
            subdir: merged.subdir.clone().unwrap_or_default(),
        }
    }

    /// Whether this declaration names a per-architecture table for at least one architecture,
    /// but not for `arch`.
    pub fn declared_for_other_arch_only(&self, arch: &str) -> bool {
        !self.arch.is_empty() && !self.arch.contains_key(arch)
    }

    /// `sha256:` of a deterministic rendering of every key this declaration wrote, base table
    /// and per-architecture tables alike. It stands in for a recipe file's hash in an inline
    /// tool's lock entry, and it is what makes a changed declaration stale exactly as a changed
    /// constraint is (`spec/02` §6).
    pub fn digest(&self) -> String {
        let mut text = String::new();
        self.base.write_into(&mut text);
        for (arch, fields) in &self.arch {
            let mut inner = String::new();
            fields.write_into(&mut inner);
            for line in inner.lines() {
                text.push_str(arch);
                text.push('.');
                text.push_str(line);
                text.push('\n');
            }
        }
        sha256_tagged(text.as_bytes())
    }
}

/// A declaration with `spec/01` §3.8's defaults applied, for one architecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub constraint_text: String,
    pub constraint: Constraint,
    /// The recipe to look up; `None` means the label is the name.
    pub name: Option<String>,
    pub optional: bool,
    pub env: BTreeMap<String, String>,
    pub path: Vec<String>,
    /// `None` means "whatever the recipe promises", or nothing to check for an inline archive.
    pub bin: Option<Vec<String>>,
    pub url: Option<String>,
    pub sha256: Option<String>,
    pub format: Option<String>,
    pub strip_components: u32,
    pub subdir: String,
}

impl Effective {
    /// Whether this is the inline form (`spec/01` §3.8): a URL instead of a recipe.
    pub fn is_inline(&self) -> bool {
        self.url.is_some()
    }
}

/// A validated `[tools]` table: label -> declaration. Labels are the lock's entry names.
pub type ToolSet = BTreeMap<String, ToolDecl>;

// ------------------------------------------------------------------------------- parsing ---

/// A table-like node and the span of the key that introduced it.
struct Entry<'a> {
    key: &'a str,
    key_span: Option<Range<usize>>,
    item: &'a Item,
}

fn entries<'a>(table: &'a dyn toml_edit::TableLike) -> Vec<Entry<'a>> {
    table
        .iter()
        .map(|(key, item)| Entry {
            key,
            key_span: table
                .get_key_value(key)
                .and_then(|(k, _): (&Key, _)| k.span()),
            item,
        })
        .collect()
}

fn span_of(entry: &Entry<'_>) -> Option<Range<usize>> {
    entry.item.span().or_else(|| entry.key_span.clone())
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::None => "nothing",
        Item::Table(_) => "a table",
        Item::ArrayOfTables(_) => "an array of tables",
        Item::Value(value) => match value {
            Value::String(_) => "a string",
            Value::Integer(_) => "an integer",
            Value::Float(_) => "a float",
            Value::Boolean(_) => "a boolean",
            Value::Datetime(_) => "a datetime",
            Value::Array(_) => "an array",
            Value::InlineTable(_) => "a table",
        },
    }
}

fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if *ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

/// The nearest names to `name` among `known`, closest first; empty when nothing is near.
pub fn nearest(name: &str, known: &[String]) -> Vec<String> {
    let mut scored: Vec<(usize, &String)> = known
        .iter()
        .map(|k| (distance(name, k), k))
        .filter(|(d, k)| *d <= 3 && *d < k.len().max(name.len()))
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().map(|(_, k)| k.clone()).collect()
}

/// Whether `text` names one exact version rather than a range, a prefix family or `latest`.
/// An inline tool's URL and hash are fixed, so a constraint on it would assert nothing
/// (design call D9); this is the test `E_INLINE_NEEDS_EXACT` applies.
pub fn is_exact_version(text: &str) -> bool {
    // A version with fewer than three numeric components is a *prefix* — `"1.7"` matches every
    // 1.7.x — and a prefix over a fixed URL and a fixed hash asserts nothing (design call D9).
    Version::parse(text.trim()).is_ok_and(|v| v.parts.len() >= 3)
}

struct Parser<'s> {
    sink: &'s mut dyn Sink,
    /// How many diagnostics have been emitted. A declaration that produced one is left out of
    /// the [`ToolSet`]: the run has already failed, and half a declaration resolves nothing.
    errors: usize,
}

impl Parser<'_> {
    fn error(&mut self, code: &'static str, message: String, span: Option<Range<usize>>) {
        self.errors += 1;
        self.sink.emit(Diagnostic::new(code, message), span);
    }

    fn error_hint(
        &mut self,
        code: &'static str,
        message: String,
        hint: String,
        span: Option<Range<usize>>,
    ) {
        self.errors += 1;
        self.sink
            .emit(Diagnostic::new(code, message).hint(hint), span);
    }

    fn type_error(&mut self, entry: &Entry<'_>, place: &str, expected: &str) {
        let message = format!(
            "`{}` in {place} must be {expected}, found {}",
            entry.key,
            kind(entry.item)
        );
        self.error("E_TYPE", message, span_of(entry));
    }

    /// A string value, with `spec/01` §1.3 applied to it.
    fn string(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        let raw = self.raw_string(entry, place)?;
        let (text, problems) = substitute(&raw);
        let ok = problems.is_empty();
        for d in problems {
            self.errors += 1;
            self.sink.emit(d, span_of(entry));
        }
        ok.then_some(text)
    }

    /// A string value as written. Only a tool's `env` uses it: its `${self.…}` substitutions
    /// are checked by [`Parser::check_self_substitutions`] and kept for realization.
    fn raw_string(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        match entry.item.as_str() {
            Some(s) => Some(s.to_string()),
            None => {
                self.type_error(entry, place, "a string");
                None
            }
        }
    }

    fn string_array(&mut self, entry: &Entry<'_>, place: &str) -> Option<Vec<String>> {
        let Some(array) = entry.item.as_array() else {
            self.type_error(entry, place, "an array of strings");
            return None;
        };
        let mut out = Vec::new();
        let mut ok = true;
        for value in array.iter() {
            match value.as_str() {
                Some(s) => out.push(s.to_string()),
                None => {
                    ok = false;
                    self.error(
                        "E_TYPE",
                        format!("`{}` in {place} must contain only strings", entry.key),
                        value.span().or_else(|| span_of(entry)),
                    );
                }
            }
        }
        ok.then_some(out)
    }

    /// Relative sub-paths of an artifact: no leading `/`, no `..`, no empty component.
    fn relative_paths(&mut self, entry: &Entry<'_>, place: &str) -> Option<Vec<String>> {
        let values = self.string_array(entry, place)?;
        let mut ok = true;
        for p in &values {
            if p.starts_with('/') || p.split('/').any(|c| c == ".." || c.is_empty()) {
                ok = false;
                self.error_hint(
                    "E_TYPE",
                    format!("`{p}` in {place} is not a relative path inside the artifact"),
                    "write it relative to the artifact root, without a leading `/` or `..`".into(),
                    span_of(entry),
                );
            }
        }
        ok.then_some(values)
    }

    fn unknown_key(&mut self, entry: &Entry<'_>, place: &str) {
        let (code, what) = if entry.item.is_table_like() || entry.item.is_array_of_tables() {
            ("E_UNKNOWN_BLOCK", "table")
        } else {
            ("E_UNKNOWN_ATTR", "key")
        };
        let known: Vec<String> = TOOL_KEYS.iter().map(|k| (*k).to_string()).collect();
        let hint = match nearest(entry.key, &known).first() {
            Some(near) => format!("did you mean `{near}`?"),
            None => format!("valid names: {}", TOOL_KEYS.join(", ")),
        };
        self.error_hint(
            code,
            format!("unknown {what} `{}` in {place}", entry.key),
            hint,
            entry.key_span.clone(),
        );
    }

    /// One `[tools.<label>]` table (or per-architecture table) into [`Fields`].
    fn fields(
        &mut self,
        table: &dyn toml_edit::TableLike,
        place: &str,
        per_arch: bool,
    ) -> (Fields, BTreeMap<String, Fields>, bool) {
        let mut fields = Fields::default();
        let mut arches = BTreeMap::new();
        let mut only_version = true;
        for field in entries(table) {
            if field.key != "version" {
                only_version = false;
            }
            match field.key {
                "version" => {
                    if let Some(text) = self.string(&field, place) {
                        match Constraint::parse(&text) {
                            Ok(c) => fields.version = Some((text, c)),
                            Err(why) => self.error_hint(
                                "E_TYPE",
                                format!("invalid version constraint \"{text}\" in {place}: {why}"),
                                "use a prefix such as \"3.12\", \"latest\", or ranges such as \
                                 \">=3.11, <3.13\""
                                    .into(),
                                span_of(&field),
                            ),
                        }
                    }
                }
                "name" => {
                    if let Some(name) = self.string(&field, place) {
                        if is_package_name(&name) {
                            fields.name = Some(name);
                        } else {
                            self.error(
                                "E_IDENT",
                                format!(
                                    "`name` in {place}: `{name}` must match \
                                     ^[a-z0-9][a-z0-9._+-]{{0,127}}$"
                                ),
                                span_of(&field),
                            );
                        }
                    }
                }
                "optional" => match field.item.as_bool() {
                    Some(b) => fields.optional = Some(b),
                    None => self.type_error(&field, place, "a boolean"),
                },
                "env" => {
                    if let Some(env) = self.env(&field, place) {
                        fields.env = Some(env);
                    }
                }
                "path" => {
                    if let Some(v) = self.relative_paths(&field, place) {
                        fields.path = Some(v);
                    }
                }
                "bin" => {
                    if let Some(v) = self.relative_paths(&field, place) {
                        fields.bin = Some(v);
                    }
                }
                "url" => {
                    if let Some(url) = self.string(&field, place) {
                        if url.starts_with("https://") {
                            fields.url = Some(url);
                        } else {
                            self.error_hint(
                                "E_INSECURE_URL",
                                format!("`url` in {place} is not an HTTPS URL: {url}"),
                                "inline tools are downloaded over HTTPS only".into(),
                                span_of(&field),
                            );
                        }
                    }
                }
                "sha256" => {
                    if let Some(hex) = self.string(&field, place) {
                        if hex.len() == 64
                            && hex
                                .bytes()
                                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
                        {
                            fields.sha256 = Some(hex);
                        } else {
                            self.error_hint(
                                "E_TYPE",
                                format!("`sha256` in {place} is not 64 lower-case hex digits"),
                                "paste the digest `sha256sum` prints, without a `sha256:` prefix"
                                    .into(),
                                span_of(&field),
                            );
                        }
                    }
                }
                "format" => {
                    if let Some(format) = self.string(&field, place) {
                        if FORMATS.contains(&format.as_str()) {
                            fields.format = Some(format);
                        } else {
                            self.error_hint(
                                "E_UNSUPPORTED",
                                format!(
                                    "archive format `{format}` in {place} is not implemented \
                                         by this build"
                                ),
                                format!(
                                    "this build decodes {}; nothing is silently substituted",
                                    FORMATS.join(", ")
                                ),
                                span_of(&field),
                            );
                        }
                    }
                }
                "strip_components" => match field.item.as_integer() {
                    Some(n) if (0..=MAX_STRIP).contains(&n) => {
                        fields.strip_components = Some(n as u32);
                    }
                    Some(n) => self.error(
                        "E_TYPE",
                        format!("`strip_components` in {place} is {n}; it must be 0…{MAX_STRIP}"),
                        span_of(&field),
                    ),
                    None => self.type_error(&field, place, "an integer"),
                },
                "subdir" => {
                    if let Some(subdir) = self.string(&field, place) {
                        if subdir.starts_with('/')
                            || subdir.split('/').any(|c| c == ".." || c.is_empty())
                        {
                            self.error_hint(
                                "E_TYPE",
                                format!(
                                    "`subdir` in {place} is not a relative path inside the archive"
                                ),
                                "write it relative to the archive root, without a leading `/` or \
                                 `..`"
                                    .into(),
                                span_of(&field),
                            );
                        } else {
                            fields.subdir = Some(subdir);
                        }
                    }
                }
                arch if ARCHES.contains(&arch) => {
                    if per_arch {
                        self.error_hint(
                            "E_UNKNOWN_BLOCK",
                            format!("`{arch}` in {place} is already an architecture table"),
                            format!(
                                "architecture tables do not nest: declare `{arch}` as its own \
                                 table under the tool"
                            ),
                            field.key_span.clone(),
                        );
                        continue;
                    }
                    let Some(inner) = field.item.as_table_like() else {
                        self.type_error(&field, place, "a table");
                        continue;
                    };
                    let inner_place = format!("{}.{arch}]", place.trim_end_matches(']'));
                    let (over, _, _) = self.fields(inner, &inner_place, true);
                    arches.insert(arch.to_string(), over);
                }
                _ => self.unknown_key(&field, place),
            }
        }
        (fields, arches, only_version)
    }

    /// `[tools.<label>.env]`: names `spec/01` §1.4 allows, values in which `${self.path}` and
    /// `${self.version}` are the only substitutions (resolved at realization).
    fn env(&mut self, entry: &Entry<'_>, place: &str) -> Option<BTreeMap<String, String>> {
        let Some(table) = entry.item.as_table_like() else {
            self.type_error(entry, place, "a table of strings");
            return None;
        };
        let place = format!("{}.env]", place.trim_end_matches(']'));
        let mut env = BTreeMap::new();
        let mut ok = true;
        for field in entries(table) {
            if !is_env_name(field.key) {
                ok = false;
                self.error(
                    "E_IDENT",
                    format!(
                        "environment variable name `{}` in {place} must match \
                         ^[A-Za-z_][A-Za-z0-9_]{{0,255}}$",
                        field.key
                    ),
                    field.key_span.clone(),
                );
                continue;
            }
            let Some(value) = self.raw_string(&field, &place) else {
                ok = false;
                continue;
            };
            if !self.check_self_substitutions(&value, &place, span_of(&field)) {
                ok = false;
                continue;
            }
            env.insert(field.key.to_string(), value);
        }
        ok.then_some(env)
    }

    /// `${self.path}` and `${self.version}` are the only substitutions a tool's `env` may use
    /// (`spec/01` §3.8); anything else inside `${ }` is `E_EXCLUDED_CONSTRUCT`.
    fn check_self_substitutions(
        &mut self,
        value: &str,
        place: &str,
        span: Option<Range<usize>>,
    ) -> bool {
        let mut rest = value;
        while let Some(i) = rest.find("${") {
            rest = &rest[i + 2..];
            let Some(end) = rest.find('}') else {
                self.error_hint(
                    "E_EXCLUDED_CONSTRUCT",
                    format!("unterminated `${{` in {place}"),
                    "write `${self.path}` or `${self.version}`, the only substitutions a tool's \
                     env has"
                        .into(),
                    span,
                );
                return false;
            };
            let name = &rest[..end];
            if !matches!(name, "self.path" | "self.version") {
                self.error_hint(
                    "E_EXCLUDED_CONSTRUCT",
                    format!("`${{{name}}}` in {place} is not a substitution a tool's env has"),
                    "only `${self.path}` and `${self.version}` are substituted, at realization"
                        .into(),
                    span,
                );
                return false;
            }
            rest = &rest[end + 1..];
        }
        true
    }
}

/// Validate a `[tools]` table into a [`ToolSet`], reporting every problem through `sink`.
///
/// `table` is any table-like `toml_edit` node — this module never learns which document it came
/// out of. Every entry is validated, so one run reports every problem (`spec/01` §7); an entry
/// that produced an error is left out of the set.
pub fn parse(table: &dyn toml_edit::TableLike, sink: &mut dyn Sink) -> ToolSet {
    let mut parser = Parser { sink, errors: 0 };
    let mut set: ToolSet = BTreeMap::new();
    for entry in entries(table) {
        let before = parser.errors;
        let place = format!("[tools.{}]", entry.key);
        if !is_package_name(entry.key) {
            parser.error(
                "E_IDENT",
                format!(
                    "tool name `{}` must match ^[a-z0-9][a-z0-9._+-]{{0,127}}$",
                    entry.key
                ),
                entry.key_span.clone(),
            );
            continue;
        }
        let (base, arch, simple) = if entry.item.as_str().is_some() {
            let Some(text) = parser.string(&entry, "[tools]") else {
                continue;
            };
            match Constraint::parse(&text) {
                Ok(c) => (
                    Fields {
                        version: Some((text, c)),
                        ..Fields::default()
                    },
                    BTreeMap::new(),
                    true,
                ),
                Err(why) => {
                    parser.error_hint(
                        "E_TYPE",
                        format!(
                            "invalid version constraint \"{text}\" for tool `{}`: {why}",
                            entry.key
                        ),
                        "use a prefix such as \"3.12\", \"latest\", or ranges such as \
                         \">=3.11, <3.13\""
                            .into(),
                        span_of(&entry),
                    );
                    continue;
                }
            }
        } else if let Some(t) = entry.item.as_table_like() {
            parser.fields(t, &place, false)
        } else {
            parser.type_error(&entry, "[tools]", "a version constraint string or a table");
            continue;
        };

        let decl = ToolDecl {
            constraint_text: base
                .version
                .as_ref()
                .map_or_else(|| "latest".to_string(), |(t, _)| t.clone()),
            constraint: base
                .version
                .as_ref()
                .map_or(Constraint::Latest, |(_, c)| c.clone()),
            base,
            arch,
            simple,
        };

        // An inline tool's version is a label, not a search: a constraint on a fixed URL and a
        // fixed hash would assert nothing (design call D9). Checked for the base table and for
        // every architecture the declaration names, because a per-architecture table may be the
        // inline half of a declaration whose base table is not.
        let mut places: Vec<String> = Vec::new();
        if decl.base.url.is_some() {
            places.push("x86_64".to_string());
        }
        places.extend(decl.arch.keys().cloned());
        let mut bad = false;
        for arch in places {
            let eff = decl.effective(&arch);
            if !eff.is_inline() {
                continue;
            }
            if !is_exact_version(&eff.constraint_text) {
                bad = true;
                parser.error_hint(
                    "E_INLINE_NEEDS_EXACT",
                    format!(
                        "`{}` declares `url`, so its version must be an exact version, not \
                         `{}`",
                        entry.key, eff.constraint_text
                    ),
                    "write the version the URL points at, such as version = \"1.7.1\"".into(),
                    entry.key_span.clone(),
                );
                break;
            }
            if let Some(format) = format_of(&eff)
                && !FORMATS.contains(&format)
            {
                bad = true;
                parser.error_hint(
                    "E_UNSUPPORTED",
                    format!(
                        "the artifact of `{}` is `{format}`, which this build cannot decode",
                        entry.key
                    ),
                    format!(
                        "this build decodes {}; set `format` to one of them, or use an artifact \
                         it decodes",
                        FORMATS.join(", ")
                    ),
                    entry.key_span.clone(),
                );
                break;
            }
        }
        if bad || parser.errors > before {
            continue;
        }
        set.insert(entry.key.to_string(), decl);
    }
    set
}

/// The archive format of an inline declaration: what it declares, else what its URL's extension
/// says. `None` when there is no URL; a recognized extension this build cannot decode is
/// returned as itself, so the caller refuses it rather than guessing at a single binary.
pub fn format_of(eff: &Effective) -> Option<&'static str> {
    if let Some(declared) = &eff.format {
        return FORMATS.iter().copied().find(|f| *f == declared);
    }
    let url = eff.url.as_ref()?;
    let name = url.rsplit('/').next().unwrap_or(url);
    let name = name.split(['?', '#']).next().unwrap_or(name).to_lowercase();
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        return Some("tar.gz");
    }
    if name.ends_with(".tar.xz") || name.ends_with(".txz") {
        return Some("tar.xz");
    }
    match UNDECODABLE.iter().find(|ext| name.ends_with(**ext)) {
        Some(ext) => Some(ext),
        None => Some("binary"),
    }
}

// ----------------------------------------------------------------------------- resolution ---

/// The recipes resolution may look a tool up in. The set is data the caller supplies, so this
/// module names no recipe and holds no list of its own.
pub trait Catalogue {
    /// The recipe for `name`, or `None` when the catalogue has none.
    fn recipe(&self, name: &str) -> Option<Result<Recipe, Diagnostic>>;
    /// Every tool name the catalogue holds, sorted — the hint of `E_NO_RECIPE`.
    fn names(&self) -> Vec<String>;
    /// Where a recipe of the person's own for `name` would go — the other hint of `E_NO_RECIPE`.
    fn place(&self, name: &str) -> String {
        crate::catalogue::user::UserRecipes::none().place(name)
    }
}

/// The catalogue built into this binary.
pub struct Builtin;

impl Catalogue for Builtin {
    fn recipe(&self, name: &str) -> Option<Result<Recipe, Diagnostic>> {
        crate::catalogue::builtin_recipe(name)
    }
    fn names(&self) -> Vec<String> {
        crate::catalogue::builtin_tool_names()
    }
}

/// What resolving one declaration produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Locked(Box<LockedTool>),
    /// The tool is `optional` and has no build for this architecture: the warning line to print,
    /// and no lock entry (`W_OPTIONAL_SKIPPED`; the exit status does not change).
    Skipped(String),
}

/// Resolve one declaration for `arch`, through a catalogue recipe or the inline form.
pub fn resolve_one(
    label: &str,
    decl: &ToolDecl,
    catalogue: &dyn Catalogue,
    fetcher: &dyn Fetcher,
    arch: &str,
) -> Result<Outcome, Diagnostic> {
    let eff = decl.effective(arch);
    let result = resolve_inner(label, decl, &eff, catalogue, fetcher, arch);
    match result {
        Err(d) if eff.optional && d.code == "E_UNSUPPORTED_ARCH" => Ok(Outcome::Skipped(format!(
            "lodi: warning W_OPTIONAL_SKIPPED: `{label}` is optional and has no {arch} build; it \
             is left out of the lock ({})",
            d.message
        ))),
        other => other.map(|t| Outcome::Locked(Box::new(t))),
    }
}

fn resolve_inner(
    label: &str,
    decl: &ToolDecl,
    eff: &Effective,
    catalogue: &dyn Catalogue,
    fetcher: &dyn Fetcher,
    arch: &str,
) -> Result<LockedTool, Diagnostic> {
    if decl.declared_for_other_arch_only(arch) {
        let declared: Vec<&str> = decl.arch.keys().map(String::as_str).collect();
        return Err(Diagnostic::new(
            "E_UNSUPPORTED_ARCH",
            format!(
                "`{label}` is declared only for {}; this build resolves {arch}",
                declared.join(", ")
            ),
        )
        .hint("declare the tool for x86_64 too, or mark it `optional = true`"));
    }
    let name = eff.name.clone().unwrap_or_else(|| label.to_string());
    let mut locked = if eff.is_inline() {
        inline(label, &name, eff, arch)?
    } else {
        let Some(recipe) = catalogue.recipe(&name) else {
            let known = catalogue.names();
            let near = nearest(&name, &known);
            let listed = if near.is_empty() { &known } else { &near };
            return Err(Diagnostic::new(
                "E_NO_RECIPE",
                format!("no recipe for tool `{name}` in the built-in catalogue"),
            )
            .hint(format!(
                "nearest names: {}; there is no fallback and nothing is guessed",
                listed.join(", ")
            ))
            .hint(format!(
                "a recipe of your own goes in {}",
                catalogue.place(&name)
            )));
        };
        let recipe = recipe?;
        let mut t = resolve_tool(
            fetcher,
            &recipe,
            &name,
            &eff.constraint_text,
            &eff.constraint,
            arch,
        )?;
        // The declaration overrides what the recipe says about exposure and layout.
        if let Some(bin) = &eff.bin {
            t.bin = bin.clone();
        }
        if decl.base.path.is_some() || decl.arch.values().any(|f| f.path.is_some()) {
            t.path = eff.path.clone();
        }
        // A declaration that describes the artifact describes **every** artifact of the tool:
        // a recipe's `[[asset]]` entries are the components of one release and are unpacked
        // the same way (M-0.4 T-4).
        for artifact in &mut t.artifacts {
            if let Some(format) = &eff.format {
                artifact.format = format.clone();
            }
            if decl.base.strip_components.is_some()
                || decl.arch.values().any(|f| f.strip_components.is_some())
            {
                artifact.strip_components = eff.strip_components;
            }
            if !eff.subdir.is_empty() {
                artifact.subdir = eff.subdir.clone();
            }
        }
        t
    };
    // A recipe may contribute an environment of its own (`[spec] env`, M-0.4 T-5): the
    // declaration's `env` is overlaid on it key by key, exactly as a per-architecture table is
    // overlaid on a base table, so a manifest can add to or replace what a recipe sets and an
    // inline tool — which has no recipe — is unaffected.
    locked.env.extend(eff.env.clone());
    let digest = decl.digest();
    if locked.input == "inline" {
        // `spec/01` §3.8 through T-2: an inline tool has no recipe file, so the declaration's
        // own digest takes the recipe hash's place in the lock. A changed declaration is then
        // stale exactly as a changed constraint is.
        locked.recipe_sha256 = digest.clone();
    }
    locked.declaration = (!decl.simple).then_some(digest);
    Ok(locked)
}

/// The inline form: a URL and a hash the user wrote, with no recipe and no discovery request.
fn inline(label: &str, name: &str, eff: &Effective, arch: &str) -> Result<LockedTool, Diagnostic> {
    let url = eff.url.clone().expect("an inline tool has a url");
    let Some(sha256) = eff.sha256.clone() else {
        return Err(Diagnostic::new(
            "E_NO_CHECKSUM",
            format!("`{label}` declares {url} for {arch} with no `sha256`"),
        )
        .hint(
            "add sha256 = \"<digest>\"; this build records no hash it did not verify (no trust \
             on first use)",
        ));
    };
    let format = format_of(eff).expect("an inline tool has a url");
    if !FORMATS.contains(&format) {
        return Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!("the artifact of `{label}` is `{format}`, which this build cannot decode"),
        )
        .hint(format!("this build decodes {}", FORMATS.join(", "))));
    }
    let bin = match &eff.bin {
        Some(bin) => bin.clone(),
        // `format = "binary"` is one file, published as the tool's own name (`spec/01` §3.8).
        None if format == "binary" => vec![format!("bin/{name}")],
        None => Vec::new(),
    };
    Ok(LockedTool {
        name: name.to_string(),
        constraint: eff.constraint_text.clone(),
        version: eff.constraint_text.trim().to_string(),
        tag: None,
        input: "inline".to_string(),
        recipe_file: format!("[tools.{label}]"),
        recipe_sha256: String::new(), // filled in by the caller from the declaration digest
        arch: arch.to_string(),
        artifacts: vec![LockedArtifact {
            url,
            sha256,
            size: None,
            format: format.to_string(),
            strip_components: eff.strip_components,
            subdir: eff.subdir.clone(),
            exclude: Vec::new(),
        }],
        bin,
        path: eff.path.clone(),
        env: BTreeMap::new(),
        declaration: None,
    })
}

/// What [`resolve`] produces: each label's locked tool, in order, and the
/// `W_OPTIONAL_SKIPPED` warning lines of the declarations that were left out.
pub type Resolved = (Vec<(String, LockedTool)>, Vec<String>);

/// Resolve a whole set for `arch`: the locked tools by label, and the warning lines of every
/// optional tool that was left out.
pub fn resolve(
    set: &ToolSet,
    catalogue: &dyn Catalogue,
    fetcher: &dyn Fetcher,
    arch: &str,
) -> Result<Resolved, Diagnostic> {
    let mut locked = Vec::new();
    let mut warnings = Vec::new();
    for (label, decl) in set {
        match resolve_one(label, decl, catalogue, fetcher, arch)? {
            Outcome::Locked(tool) => locked.push((label.clone(), *tool)),
            Outcome::Skipped(warning) => warnings.push(warning),
        }
    }
    Ok((locked, warnings))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink that keeps what it was given; the shape every caller of [`parse`] provides.
    #[derive(Default)]
    struct Collect(Vec<(Diagnostic, Option<Range<usize>>)>);

    impl Sink for Collect {
        fn emit(&mut self, d: Diagnostic, span: Option<Range<usize>>) {
            self.0.push((d, span));
        }
    }

    fn parse_text(text: &str) -> (ToolSet, Vec<String>) {
        let doc = toml_edit::ImDocument::parse(text.to_string()).expect("the fixture parses");
        let table = doc
            .as_table()
            .get("tools")
            .and_then(Item::as_table_like)
            .expect("the fixture has [tools]");
        let mut sink = Collect::default();
        let set = parse(table, &mut sink);
        let codes = sink.0.iter().map(|(d, _)| d.code.to_string()).collect();
        (set, codes)
    }

    #[test]
    fn a_bare_string_is_a_constraint_and_stays_simple() {
        let (set, codes) = parse_text("[tools]\npython = \"3.12\"\n");
        assert!(codes.is_empty(), "{codes:?}");
        let decl = &set["python"];
        assert_eq!(decl.constraint_text, "3.12");
        assert!(decl.simple);
        assert_eq!(decl.effective("x86_64").path, ["bin"]);
    }

    #[test]
    fn a_per_arch_table_overrides_key_by_key() {
        let (set, codes) = parse_text(
            "[tools.jq]\nversion = \"1.7.1\"\nurl = \"https://e.test/jq-x86\"\n\
             sha256 = \"aa11\"\n[tools.jq.aarch64]\nurl = \"https://e.test/jq-arm\"\n",
        );
        assert_eq!(codes, ["E_TYPE"], "a short sha256 is refused");
        assert!(set.is_empty());

        let hex = "a".repeat(64);
        let (set, codes) = parse_text(&format!(
            "[tools.jq]\nversion = \"1.7.1\"\nurl = \"https://e.test/jq-x86\"\n\
             sha256 = \"{hex}\"\n[tools.jq.aarch64]\nurl = \"https://e.test/jq-arm\"\n"
        ));
        assert!(codes.is_empty(), "{codes:?}");
        let decl = &set["jq"];
        assert!(!decl.simple);
        assert_eq!(
            decl.effective("x86_64").url.unwrap(),
            "https://e.test/jq-x86"
        );
        assert_eq!(
            decl.effective("aarch64").url.unwrap(),
            "https://e.test/jq-arm"
        );
        // The overlay keeps the base table's other keys.
        assert_eq!(decl.effective("aarch64").sha256.unwrap(), hex);
    }

    #[test]
    fn the_digest_changes_with_any_declared_key() {
        let hex = "b".repeat(64);
        let one = parse_text(&format!(
            "[tools.jq]\nversion = \"1.7.1\"\nurl = \"https://e.test/jq\"\nsha256 = \"{hex}\"\n"
        ))
        .0;
        let two = parse_text(&format!(
            "[tools.jq]\nversion = \"1.7.1\"\nurl = \"https://e.test/jq\"\nsha256 = \"{hex}\"\n\
             strip_components = 1\n"
        ))
        .0;
        assert_ne!(one["jq"].digest(), two["jq"].digest());
        assert_eq!(one["jq"].digest(), one["jq"].digest());
    }

    #[test]
    fn a_format_by_extension_is_never_guessed_for_one_this_build_cannot_decode() {
        let eff = |url: &str| Effective {
            constraint_text: "1".into(),
            constraint: Constraint::Latest,
            name: None,
            optional: false,
            env: BTreeMap::new(),
            path: vec!["bin".into()],
            bin: None,
            url: Some(url.to_string()),
            sha256: None,
            format: None,
            strip_components: 0,
            subdir: String::new(),
        };
        assert_eq!(format_of(&eff("https://e.test/x.tar.gz")), Some("tar.gz"));
        assert_eq!(format_of(&eff("https://e.test/x.tgz")), Some("tar.gz"));
        assert_eq!(format_of(&eff("https://e.test/x.tar.xz")), Some("tar.xz"));
        assert_eq!(format_of(&eff("https://e.test/jq-linux64")), Some("binary"));
        assert_eq!(format_of(&eff("https://e.test/x.zip")), Some(".zip"));
        assert_eq!(
            format_of(&eff("https://e.test/x.tar.zst")),
            Some(".tar.zst")
        );
    }

    #[test]
    fn exact_versions_are_version_literals_only() {
        assert!(is_exact_version("1.7.1"));
        assert!(is_exact_version("3.12.14-rc1"));
        // Fewer than three components is a prefix over a fixed URL, which asserts nothing.
        assert!(!is_exact_version("22"));
        assert!(!is_exact_version("1.7"));
        assert!(!is_exact_version("latest"));
        assert!(!is_exact_version(">=1.2"));
        assert!(!is_exact_version("1.x"));
    }

    #[test]
    fn nearest_names_are_near() {
        let known: Vec<String> = ["nodejs", "python", "ripgrep"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(nearest("pyton", &known), ["python"]);
        assert!(nearest("zzzzzzzz", &known).is_empty());
    }
}
