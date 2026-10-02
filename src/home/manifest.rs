//! The home manifest (`<config>/home.toml`): the subset of design `spec/01` §4 this build
//! supports (M-0.5 T-2, design calls D2, D8, `LD-98`, `LD-104`).
//!
//! This loader is a **sibling** of [`crate::manifest`], not an extension of it: a separate scope
//! with a separate root schema, which happens to fail the same way for the same input because it
//! copies that module's `Validator` idiom. Every problem in a file is reported in one run, sorted
//! by position, each with its file, line, column and a hint (`spec/01` §7). Loading writes
//! nothing and runs nothing: `text` and `source` are data here.
//!
//! ```toml
//! [home]
//! version = "1"
//!
//! [home.file.".config/git/ignore"]
//! text = """
//! .lodi/
//! """
//! mode      = "0644"       # three or four octal digits, default "0644"
//! state     = "present"    # present | absent
//! backup    = true
//! on_remove = "restore"    # restore | delete | keep
//!
//! [home.file.".inputrc"]
//! source = "./dotfiles/inputrc"
//! ```
//!
//! The key of `[home.file."<path>"]` is a [`RelPath`] below the **home** root, so a path that could
//! leave it is `E_PATH_ESCAPE` at the key's column, before anything is planned. `source` is a
//! [`RelPath`] below the directory the manifest itself lives in and is read verbatim at load
//! time, through [`crate::home::fsops`], so a symbolic link on the way to it is refused there
//! too.
//!
//! `[home.xdg_config."<path>"]` (`docs/design/HOME_PROGRAMS.md` §3.2) and `[programs.<name>]`
//! (§3.1) sit beside it. All three kinds — the two `[home.*]` tables and every file a program
//! module renders — become the one [`FileEntry`] type in one map, so a path declared twice
//! anywhere is `E_DUP_RESOURCE`. 1.x's `[files]` table is refused as an unknown table from 2.0
//! (#710). A `source` that names a directory expands at
//! load time into one entry per regular file below it. `state = "seed"` writes a file once, when
//! nothing is at the path, and never compares or rewrites it afterwards.
//!
//! `[tools]` is handed to [`crate::tools::parse`], so this scope neither copies nor narrows the
//! project schema. T-4 first used that path behind the command surface; T-5 makes it ordinary now
//! that the stable PATH surface exists.
//! `[vars]`, `[inputs]`, `[modules]` and `[home] registries` remain `E_UNSUPPORTED` with a message
//! naming what each would need (design call D8). No `${ }` substitution exists in any scope in
//! 0.5, so a well-formed `${…}` is `E_UNSUPPORTED` and a malformed one
//! `E_EXCLUDED_CONSTRUCT`, exactly as in the project manifest.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::os::unix::fs::FileTypeExt;
use std::path::{Component, PathBuf};

use toml_edit::{ImDocument, Item, Key, Value};

use crate::diag::{Diagnostic, Location};
use crate::home::fsops::{self, RelPath, Root};
use crate::home::programs::{self, Module, RenderEnv};
use crate::home::toml::{Table, Value as TomlValue};
use crate::manifest::{ManifestErrors, distance};
use crate::roots::Roots;
use crate::tools::{self, ToolSet};
use crate::version::Version;

/// The manifest's name inside the configuration root (`spec/01` §4, design call D2).
pub use crate::lock::HOME_MANIFEST_FILE;

/// Largest accepted home manifest, the project manifest's limit (`spec/01` §1.4).
pub const MAX_HOME_MANIFEST_BYTES: usize = 1024 * 1024;

/// The mode a `[files]` entry gets when it declares none.
pub const DEFAULT_MODE: &str = "0644";

/// The mode of every file a program module renders: no module has `mode` (§3.1).
const PROGRAM_MODE: u32 = 0o644;

/// Bits a plain file may not carry here: set-uid, set-gid and sticky.
const SPECIAL_BITS: u32 = 0o7000;

const TOP_LEVEL: &[&str] = &[
    "home", "programs", "vars", "inputs", "modules", "tools", "services",
];
/// The keys of one `[services."<unit>"]` table (hc-1, LD-427).
const SERVICE_KEYS: &[&str] = &["enable", "linger", "unit"];

/// Where an inline `unit` is written, relative to the home directory: the user manager's own
/// configuration directory, whatever `XDG_CONFIG_HOME` this process was given.
pub const USER_UNIT_DIR: &str = ".config/systemd/user";
const HOME_KEYS: &[&str] = &[
    "version",
    "min_lodi_version",
    "registries",
    "file",
    "xdg_config",
];
const HOME_FILE_KEYS: &[&str] = &[
    "text",
    "source",
    "mode",
    "executable",
    "state",
    "backup",
    "on_remove",
    "enable",
];

/// What a declared path must be (`spec/01` §4.3, `docs/design/HOME_PROGRAMS.md` §3.2). The
/// manifest spells [`FileState::Managed`] `present`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    /// `present`: written, recorded, and drift-checked on every apply.
    Managed,
    /// `seed`: written once when nothing is at the path, recorded as written by Lodi, and never
    /// compared or rewritten afterwards.
    Seed,
    /// `absent`: removed.
    Absent,
}

/// What an apply does with a file whose entry has disappeared (design call D5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnRemove {
    Restore,
    Delete,
    Keep,
}

impl OnRemove {
    pub fn name(self) -> &'static str {
        match self {
            OnRemove::Restore => "restore",
            OnRemove::Delete => "delete",
            OnRemove::Keep => "keep",
        }
    }
}

/// Where a present file's bytes come from. Both forms are resolved at load time, so a plan never
/// reads the manifest's inputs again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Origin {
    /// `text = "…"`, verbatim.
    Content,
    /// `source = "./…"`, relative to the manifest's own directory, read verbatim. For a file
    /// expanded from a directory `source`, the file below it.
    Source(RelPath),
    /// Rendered by the program module of that name.
    Program(String),
}

/// One `[home.file."<path>"]` entry, validated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    pub path: RelPath,
    pub origin: Origin,
    /// The bytes the file must hold; empty for `state = "absent"`, which declares no content.
    pub bytes: Vec<u8>,
    pub mode: u32,
    /// The mode as the manifest wrote it, for the plan's mode column.
    pub mode_text: String,
    pub state: FileState,
    pub backup: bool,
    pub on_remove: OnRemove,
    /// The table that declared it, as the plan and a duplicate report name it: `home.file`,
    /// `home.xdg_config` or `programs.<name>`.
    pub table: String,
}

/// A declared `[programs.<name>]` table that renders, with the files that shadow it
/// (`W_SHADOWED`, §4.6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramUse {
    pub name: String,
    /// The application's executable, looked for on `PATH` (`W_PROGRAM_NOT_FOUND`, decision D1).
    pub executable: String,
    /// The home-relative paths it renders, sorted.
    pub paths: Vec<String>,
    /// The files the application reads before, or alongside, those (§4.6).
    pub shadowed_by: Vec<programs::Shadow>,
    /// The rendered file the user sources from the shell's own start-up file, absolute: the line
    /// apply prints last (§5; bash and zsh).
    pub hook: Option<PathBuf>,
}

/// A validated home manifest. `files` is keyed by the path, so every walk of it is in
/// lexicographic order (design call D13).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HomeManifest {
    /// `[home] version`; only `"1"` exists.
    pub schema_version: String,
    /// `[home] min_lodi_version` as written, when it declares one.
    pub min_lodi_version: Option<String>,
    /// The shared `spec/01` §3.8 tool declarations.
    pub tools: ToolSet,
    pub files: BTreeMap<String, FileEntry>,
    /// The program modules that render, sorted by name.
    pub programs: Vec<ProgramUse>,
    /// `[services."<unit>"]`: each user unit and whether it is wanted enabled and running
    /// (hc-1, LD-427). An inline `unit` is one more entry of `files`.
    pub services: BTreeMap<String, bool>,
    /// Whether any service asks for `linger = true` (H1).
    pub linger: bool,
}

impl HomeManifest {
    /// Whether `[programs.<name>]` is declared and renders.
    pub fn uses_program(&self, name: &str) -> bool {
        self.programs.iter().any(|p| p.name == name)
    }

    /// The shell files the user sources from a shell's own start-up file, by module name
    /// (§5): what apply and status print the line for, last.
    pub fn hooks(&self) -> Vec<(String, PathBuf)> {
        self.programs
            .iter()
            .filter_map(|p| p.hook.clone().map(|hook| (p.name.clone(), hook)))
            .collect()
    }
}

/// Read and validate `<config>/home.toml`. `source` values are read relative to that same
/// directory, through the configuration root, so nothing outside it is opened.
pub fn load_home_manifest(roots: &Roots) -> Result<HomeManifest, ManifestErrors> {
    load_home_manifest_inner(roots, programs::registry())
}

/// [`load_home_manifest`] with an explicit program-module registry: the whole loader, which is
/// how a test drives a module this build does not ship through `plan`, `apply` and `status`.
pub fn load_home_manifest_with(
    roots: &Roots,
    registry: &[Module],
) -> Result<HomeManifest, ManifestErrors> {
    load_home_manifest_inner(roots, registry)
}

/// T-4's compatibility name for the ordinary loader, retained for its direct lock/realization
/// tests. Both paths hand `[tools]` to the shared [`crate::tools::parse`] entry point.
pub(crate) fn load_home_manifest_with_tools(roots: &Roots) -> Result<HomeManifest, ManifestErrors> {
    load_home_manifest_inner(roots, programs::registry())
}

/// Where `home.toml` lives: the configuration root, and the manifest's path inside it.
/// `lodi switch` reads it here and `lodi import --home` writes it here, through this one
/// function, so the two can never name different places (LD-325).
pub fn location(roots: &Roots) -> (Root, RelPath) {
    (
        roots.config_root(),
        RelPath::new(HOME_MANIFEST_FILE).expect("the manifest's name is a relative path"),
    )
}

fn load_home_manifest_inner(
    roots: &Roots,
    registry: &[Module],
) -> Result<HomeManifest, ManifestErrors> {
    let env = RenderEnv::of(roots);
    // §4.1: an XDG configuration directory outside the home directory is refused before the
    // manifest is read, whatever it declares — `[files]` alone included — so nothing is planned,
    // applied or reported under it. A lexical prefix alone accepts `HOME/../outside`;
    // reject parent traversal rather than reading through it to resolve the path.
    if env.xdg_config_home.strip_prefix(&env.home).is_err()
        || env
            .xdg_config_home
            .components()
            .any(|part| part == Component::ParentDir)
    {
        return Err(one(Diagnostic::new(
            "E_CONFIG",
            "XDG_CONFIG_HOME is outside the home directory or contains parent traversal; \
             the home scope writes only below HOME",
        )
        .hint(
            "set XDG_CONFIG_HOME to a directory below $HOME without `..`, or unset it",
        )));
    }
    let (root, rel) = location(roots);
    let path = root.path().join(rel.as_str());
    let file = display_path(roots, &path);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(one(Diagnostic::new(
                "E_NO_MANIFEST",
                format!("there is no home manifest at {file}"),
            )
            .hint(format!(
                "run lodi import --home to write a commented starting {file}, or write it by hand \
                 following docs/guide/manage-your-home.md"
            ))));
        }
        Err(e) => {
            return Err(one(Diagnostic::new(
                "E_NO_MANIFEST",
                format!("cannot read {file}: {e}"),
            )));
        }
    };
    parse_home_manifest_bytes_inner(&bytes, &file, &root, &env, registry, None)
}

/// The path a diagnostic shows. The default configuration root is written `~/.config/lodi`, so
/// the common message carries no absolute path at all (`AGENTS.md` §1.2).
pub fn display_path(roots: &Roots, path: &std::path::Path) -> String {
    match path.strip_prefix(roots.home()) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn one(d: Diagnostic) -> ManifestErrors {
    ManifestErrors {
        diagnostics: vec![d],
    }
}

/// Validate manifest bytes; `file` names the source in diagnostics and `base` is the directory
/// `source` values are relative to.
pub fn parse_home_manifest_bytes(
    bytes: &[u8],
    file: &str,
    base: &Root,
) -> Result<HomeManifest, ManifestErrors> {
    parse_home_manifest_bytes_inner(
        bytes,
        file,
        base,
        &RenderEnv::placeholder(),
        programs::registry(),
        None,
    )
}

#[derive(Clone, Copy)]
pub struct HostSource<'a> {
    pub host: &'a crate::hostscope::source::Host,
    pub prefix: &'a str,
}

impl HostSource<'_> {
    fn relative(self, rel: &RelPath) -> String {
        if self.prefix.is_empty() {
            rel.as_str().to_string()
        } else {
            format!("{}/{}", self.prefix, rel.as_str())
        }
    }

    fn read(self, rel: &RelPath) -> Result<Vec<u8>, Diagnostic> {
        let bytes = crate::hostscope::source::read_file(
            self.host,
            &self.relative(rel),
            crate::hostscope::MAX_SOURCE_BYTES,
        )?
        .ok_or_else(|| Diagnostic::new("E_CONFIG", format!("source {rel} is not there")))?;
        if bytes.len() > crate::hostscope::MAX_SOURCE_BYTES {
            return Err(Diagnostic::new(
                "E_SYNTAX",
                format!("source {rel} exceeds 16 MiB"),
            ));
        }
        Ok(bytes)
    }

    fn kind(self, rel: &RelPath) -> Result<Option<std::fs::FileType>, Diagnostic> {
        crate::hostscope::source::inspect(self.host, &self.relative(rel))
    }

    fn entries(
        self,
        rel: &RelPath,
    ) -> Result<Option<Vec<(std::ffi::OsString, std::fs::FileType)>>, Diagnostic> {
        crate::hostscope::source::read_dir(self.host, &self.relative(rel))
    }
}

/// `env` is where the modules render for: [`RenderEnv::of`] the person's roots for `lodi
/// switch`, so that a shadowing file and a shell's rc line name the real home.
pub fn parse_home_manifest_host_bytes(
    bytes: &[u8],
    file: &str,
    base: &Root,
    env: &RenderEnv,
    source: HostSource<'_>,
) -> Result<HomeManifest, ManifestErrors> {
    parse_home_manifest_bytes_inner(bytes, file, base, env, programs::registry(), Some(source))
}

pub(crate) fn parse_home_manifest_bytes_with_tools(
    bytes: &[u8],
    file: &str,
    base: &Root,
) -> Result<HomeManifest, ManifestErrors> {
    parse_home_manifest_bytes(bytes, file, base)
}

fn parse_home_manifest_bytes_inner(
    bytes: &[u8],
    file: &str,
    base: &Root,
    env: &RenderEnv,
    registry: &[Module],
    source: Option<HostSource<'_>>,
) -> Result<HomeManifest, ManifestErrors> {
    if bytes.len() > MAX_HOME_MANIFEST_BYTES {
        return Err(one(Diagnostic::new(
            "E_SYNTAX",
            format!(
                "home manifest is {} bytes; the limit is {MAX_HOME_MANIFEST_BYTES} bytes (1 MiB)",
                bytes.len()
            ),
        )));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => parse_home_manifest_in_with(text, file, base, env, registry, source),
        Err(e) => {
            let valid = std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or("");
            Err(one(Diagnostic::new(
                "E_SYNTAX",
                "home manifest is not valid UTF-8",
            )
            .at(Location::at(file, valid, valid.len()))))
        }
    }
}

/// Validate manifest text; `file` names the source in diagnostics.
///
/// Program modules render against [`RenderEnv::placeholder`]; [`load_home_manifest`] renders
/// against the process's own roots.
pub fn parse_home_manifest(
    text: &str,
    file: &str,
    base: &Root,
) -> Result<HomeManifest, ManifestErrors> {
    parse_home_manifest_in(
        text,
        file,
        base,
        &RenderEnv::placeholder(),
        programs::registry(),
    )
}

/// Validate manifest text against explicit directories and an explicit module registry: the
/// whole loader, which is how a test drives a module this build does not ship.
pub fn parse_home_manifest_in(
    text: &str,
    file: &str,
    base: &Root,
    env: &RenderEnv,
    registry: &[Module],
) -> Result<HomeManifest, ManifestErrors> {
    parse_home_manifest_in_with(text, file, base, env, registry, None)
}

fn parse_home_manifest_in_with(
    text: &str,
    file: &str,
    base: &Root,
    env: &RenderEnv,
    registry: &[Module],
    source: Option<HostSource<'_>>,
) -> Result<HomeManifest, ManifestErrors> {
    let doc = match ImDocument::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            let mut d = Diagnostic::new("E_SYNTAX", first_line(e.message()));
            if let Some(span) = e.span() {
                d = d.at(Location::at(file, text, span.start));
            }
            return Err(one(d));
        }
    };
    let mut v = Validator {
        file,
        text,
        base,
        env,
        registry,
        host_source: source,
        diagnostics: Vec::new(),
    };
    let manifest = v.root(doc.as_table());
    if v.diagnostics.is_empty() {
        Ok(manifest)
    } else {
        Err(v.finish_err())
    }
}

fn first_line(message: &str) -> String {
    message.lines().next().unwrap_or("").trim().to_string()
}

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
            Value::Datetime(_) => "a date-time",
            Value::Array(_) => "an array",
            Value::InlineTable(_) => "an inline table",
        },
    }
}

fn suggestion(key: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .map(|k| (distance(key, k), *k))
        .filter(|(d, k)| *d <= 2 && *d < k.len())
        .min()
        .map(|(_, k)| k.to_string())
}

struct Validator<'t, 'h> {
    file: &'t str,
    text: &'t str,
    base: &'t Root,
    env: &'t RenderEnv,
    registry: &'t [Module],
    host_source: Option<HostSource<'h>>,
    diagnostics: Vec<Diagnostic>,
}

impl tools::Sink for Validator<'_, '_> {
    fn emit(&mut self, d: Diagnostic, span: Option<Range<usize>>) {
        let d = self.located(d, span);
        self.diagnostics.push(d);
    }
}

impl Validator<'_, '_> {
    fn finish_err(&mut self) -> ManifestErrors {
        let mut diagnostics = std::mem::take(&mut self.diagnostics);
        diagnostics.sort_by_key(|d| {
            d.location
                .as_ref()
                .map_or((0, 0), |at| (at.line, at.column))
        });
        ManifestErrors { diagnostics }
    }

    fn located(&self, d: Diagnostic, span: Option<Range<usize>>) -> Diagnostic {
        match span {
            Some(span) => d.at(Location::at(self.file, self.text, span.start)),
            None => d,
        }
    }

    fn error(&mut self, code: &'static str, message: String, span: Option<Range<usize>>) {
        let d = self.located(Diagnostic::new(code, message), span);
        self.diagnostics.push(d);
    }

    fn error_hint(
        &mut self,
        code: &'static str,
        message: String,
        hint: String,
        span: Option<Range<usize>>,
    ) {
        let d = self.located(Diagnostic::new(code, message).hint(hint), span);
        self.diagnostics.push(d);
    }

    /// Push a diagnostic another module built, at `span` when it carries no location of its own.
    fn push(&mut self, d: Diagnostic, span: Option<Range<usize>>) {
        let d = match d.location {
            Some(_) => d,
            None => self.located(d, span),
        };
        self.diagnostics.push(d);
    }

    /// The span of an entry: its value when it has one, else its key.
    fn span_of(entry: &Entry<'_>) -> Option<Range<usize>> {
        entry.item.span().or_else(|| entry.key_span.clone())
    }

    fn unknown(&mut self, entry: &Entry<'_>, place: &str, known: &[&str]) {
        let (code, what) = if entry.item.is_table_like() || entry.item.is_array_of_tables() {
            ("E_UNKNOWN_BLOCK", "table")
        } else {
            ("E_UNKNOWN_ATTR", "key")
        };
        let message = format!("unknown {what} `{}` in {place}", entry.key);
        let hint = match suggestion(entry.key, known) {
            Some(near) => format!("did you mean `{near}`?"),
            None => format!("valid names: {}", known.join(", ")),
        };
        self.error_hint(code, message, hint, entry.key_span.clone());
    }

    fn type_error(&mut self, entry: &Entry<'_>, place: &str, expected: &str) {
        let message = format!(
            "`{}` in {place} must be {expected}, found {}",
            entry.key,
            kind(entry.item)
        );
        self.error("E_TYPE", message, Self::span_of(entry));
    }

    fn table<'a>(
        &mut self,
        entry: &Entry<'a>,
        place: &str,
    ) -> Option<&'a dyn toml_edit::TableLike> {
        let table = entry.item.as_table_like();
        if table.is_none() {
            self.type_error(entry, place, "a table");
        }
        table
    }

    fn string(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        match entry.item.as_str() {
            Some(s) => self.substitute(s, entry.item.span()),
            None => {
                self.type_error(entry, place, "a string");
                None
            }
        }
    }

    fn boolean(&mut self, entry: &Entry<'_>, place: &str) -> Option<bool> {
        match entry.item.as_bool() {
            Some(b) => Some(b),
            None => {
                self.type_error(entry, place, "a boolean");
                None
            }
        }
    }

    /// A string from a fixed set, or `E_TYPE` naming the set.
    fn choice(&mut self, entry: &Entry<'_>, place: &str, allowed: &[&str]) -> Option<String> {
        let value = self.string(entry, place)?;
        if allowed.contains(&value.as_str()) {
            return Some(value);
        }
        self.error_hint(
            "E_TYPE",
            format!("`{}` in {place} is \"{value}\"", entry.key),
            format!("it must be one of: {}", allowed.join(", ")),
            Self::span_of(entry),
        );
        None
    }

    /// `spec/01` §1.3, as [`crate::manifest`] applies it: `$${` is a literal `${`; a bare `$NAME`
    /// is left alone; `${ }` substitution exists in no scope in this build, so a valid name is
    /// `E_UNSUPPORTED` (`E_VAR_UNSET` for `vars.*`, which cannot be declared) and anything else
    /// inside `${ }` is `E_EXCLUDED_CONSTRUCT`.
    fn substitute(&mut self, s: &str, span: Option<Range<usize>>) -> Option<String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        let mut ok = true;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            if let Some(after) = tail.strip_prefix("$${") {
                out.push_str("${");
                rest = after;
            } else if let Some(after) = tail.strip_prefix("${") {
                ok = false;
                let Some(end) = after.find('}') else {
                    self.error(
                        "E_EXCLUDED_CONSTRUCT",
                        "unterminated `${` in a string; write `$${` for a literal `${`".into(),
                        span.clone(),
                    );
                    return None;
                };
                let name = &after[..end];
                if name.strip_prefix("vars.").is_some_and(is_ident) {
                    self.error_hint(
                        "E_VAR_UNSET",
                        format!("`${{{name}}}` names an undeclared variable"),
                        "[vars] is not supported by this build".into(),
                        span.clone(),
                    );
                } else if is_substitution_name(name) {
                    self.error_hint(
                        "E_UNSUPPORTED",
                        format!("`${{{name}}}` substitution is not supported by this build"),
                        "write the value literally, or `$${` for a literal `${`".into(),
                        span.clone(),
                    );
                } else {
                    self.error_hint(
                        "E_EXCLUDED_CONSTRUCT",
                        format!("`${{{name}}}` is not a substitution name"),
                        "there are no expressions: write the value, or `$${` for a literal `${`"
                            .into(),
                        span.clone(),
                    );
                }
                rest = &after[end + 1..];
            } else {
                out.push('$');
                rest = &tail[1..];
            }
        }
        out.push_str(rest);
        ok.then_some(out)
    }

    fn root(&mut self, doc: &toml_edit::Table) -> HomeManifest {
        let mut manifest = HomeManifest {
            schema_version: "1".into(),
            min_lodi_version: None,
            tools: BTreeMap::new(),
            files: BTreeMap::new(),
            programs: Vec::new(),
            services: BTreeMap::new(),
            linger: false,
        };
        let place = "the home manifest";
        let mut programs = None;
        for entry in entries(doc) {
            match entry.key {
                "home" => {
                    if let Some(t) = self.table(&entry, place) {
                        self.home(t, &mut manifest);
                    }
                }
                "files" => self.error_hint(
                    "E_UNKNOWN_BLOCK",
                    format!("unknown table `files` in {place}"),
                    "declare each file in [home.file] instead, with `text` for `content`".into(),
                    entry.key_span.clone(),
                ),
                // Read after the loop: a shell module includes the tools only when `[tools]`
                // declares one (§5), whatever order the manifest writes the two tables in.
                "programs" => programs = Some(entry),
                "tools" => {
                    if let Some(t) = self.table(&entry, place) {
                        manifest.tools = tools::parse(t, self);
                    }
                }
                "services" => {
                    if let Some(t) = self.table(&entry, place) {
                        self.services(t, &mut manifest);
                    }
                }
                "vars" => self.needs(&entry, "`${ }` substitution in home.toml"),
                "inputs" => self.needs(&entry, "inputs fetched from elsewhere"),
                "modules" => self.needs(&entry, "a manifest split across several files"),
                _ => self.unknown(&entry, place, TOP_LEVEL),
            }
        }
        if let Some(entry) = programs
            && let Some(t) = self.table(&entry, place)
        {
            self.programs(t, &mut manifest);
        }
        manifest
    }

    /// Refuse a name `spec/01` §4 defines and this build does not implement, naming what it
    /// would need (design call D8).
    fn needs(&mut self, entry: &Entry<'_>, what: &str) {
        self.error_hint(
            "E_UNSUPPORTED",
            format!("`[{}]` is not supported by this version of lodi", entry.key),
            format!(
                "remove `[{}]` from home.toml: lodi does not support {what}",
                entry.key
            ),
            entry.key_span.clone(),
        );
    }

    fn home(&mut self, table: &dyn toml_edit::TableLike, manifest: &mut HomeManifest) {
        let place = "[home]";
        for entry in entries(table) {
            match entry.key {
                "version" => {
                    if let Some(version) = self.string(&entry, place) {
                        if version == "1" {
                            manifest.schema_version = version;
                        } else {
                            self.error_hint(
                                "E_UNSUPPORTED",
                                format!(
                                    "home manifest schema version \"{version}\" is not supported"
                                ),
                                "only version \"1\" exists".into(),
                                Self::span_of(&entry),
                            );
                        }
                    }
                }
                "min_lodi_version" => {
                    if let Some(text) = self.string(&entry, place) {
                        self.min_lodi_version(&entry, text, manifest);
                    }
                }
                "registries" => self.error_hint(
                    "E_UNSUPPORTED",
                    "`registries` is not supported by this version of lodi".into(),
                    "remove it: lodi uses only its built-in recipes".into(),
                    entry.key_span.clone(),
                ),
                "file" => {
                    if let Some(t) = self.table(&entry, place) {
                        self.home_files(t, "home.file", None, manifest);
                    }
                }
                "xdg_config" => {
                    if let Some(t) = self.table(&entry, place)
                        && let Some(prefix) = self.xdg_prefix(&entry, "[home.xdg_config]")
                    {
                        self.home_files(t, "home.xdg_config", Some(prefix), manifest);
                    }
                }
                _ => self.unknown(&entry, place, HOME_KEYS),
            }
        }
    }

    /// `min_lodi_version` is honoured: a build older than it is `E_VERSION`. An empty string
    /// declares no minimum, which is what the documented example writes.
    fn min_lodi_version(&mut self, entry: &Entry<'_>, text: String, manifest: &mut HomeManifest) {
        if text.is_empty() {
            return;
        }
        let span = Self::span_of(entry);
        let Ok(wanted) = Version::parse(&text) else {
            self.error_hint(
                "E_TYPE",
                format!("`min_lodi_version` in [home] is \"{text}\", which is not a version"),
                "write a version like \"0.3.0\"".into(),
                span,
            );
            return;
        };
        let running = Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("this build's own version parses as a version");
        if running < wanted {
            self.error_hint(
                "E_VERSION",
                format!("the home manifest needs lodi {text} or newer, and this is lodi {running}"),
                "install a newer lodi, or lower min_lodi_version".into(),
                span,
            );
        } else {
            manifest.min_lodi_version = Some(text);
        }
    }

    /// Record one entry in the one map every kind shares. A path already there is
    /// `E_DUP_RESOURCE` naming both declarations (§3.2, §3.3).
    fn add(&mut self, manifest: &mut HomeManifest, file: FileEntry, span: Option<Range<usize>>) {
        let path = file.path.as_str().to_string();
        if let Some(first) = manifest.files.get(&path) {
            if first.table == file.table {
                self.error_hint(
                    "E_DUP_RESOURCE",
                    format!("`{path}` is declared twice in [{}]", file.table),
                    "one entry per path; a trailing `/` is not part of the path".into(),
                    span,
                );
            } else {
                self.error_hint(
                    "E_DUP_RESOURCE",
                    format!(
                        "`{path}` is declared by [{}] and again by [{}]",
                        first.table, file.table
                    ),
                    "declare each path once".into(),
                    span,
                );
            }
            return;
        }
        manifest.files.insert(path, file);
    }

    /// `[home.file]` (`prefix` is `None`) or `[home.xdg_config]` (`prefix` is the XDG
    /// configuration directory relative to the home directory, possibly empty).
    fn home_files(
        &mut self,
        table: &dyn toml_edit::TableLike,
        name: &str,
        prefix: Option<String>,
        manifest: &mut HomeManifest,
    ) {
        for entry in entries(table) {
            let key = entry.key.strip_suffix('/').unwrap_or(entry.key);
            // The key is validated on its own first, so `..` in it is refused even when the
            // prefix would absorb it.
            let path = match RelPath::new(key) {
                Ok(path) => path,
                Err(d) => {
                    self.push(d, entry.key_span.clone());
                    continue;
                }
            };
            let path = match prefix.as_deref() {
                None | Some("") => path,
                Some(prefix) => match RelPath::new(&format!("{prefix}/{}", path.as_str())) {
                    Ok(path) => path,
                    Err(d) => {
                        self.push(d, entry.key_span.clone());
                        continue;
                    }
                },
            };
            let place = format!("[{name}.\"{}\"]", entry.key);
            let Some(t) = self.table(&entry, &format!("[{name}]")) else {
                continue;
            };
            let span = entry.key_span.clone();
            for file in self.file(t, path, &place, span.clone(), name) {
                self.add(manifest, file, span.clone());
            }
        }
    }

    /// `[services."<unit>"]`: `enable` (default true), `linger` (default false) and an inline
    /// `unit`, written to [`USER_UNIT_DIR`] only when the table carries one (H3).
    fn services(&mut self, table: &dyn toml_edit::TableLike, manifest: &mut HomeManifest) {
        for entry in entries(table) {
            if !crate::hostscope::manifest::is_unit_name(entry.key) {
                self.error_hint(
                    "E_TYPE",
                    format!("`{}` in [services] is not a systemd unit name", entry.key),
                    "name the unit in full, such as `backup.service` or `backup.timer`".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let place = format!("[services.\"{}\"]", entry.key);
            let Some(t) = self.table(&entry, "[services]") else {
                continue;
            };
            let mut enable = true;
            for field in entries(t) {
                match field.key {
                    "enable" => enable = self.boolean(&field, &place).unwrap_or(enable),
                    "linger" => {
                        manifest.linger |= self.boolean(&field, &place).unwrap_or(false);
                    }
                    "unit" => {
                        let Some(text) = self.string(&field, &place) else {
                            continue;
                        };
                        let path = match RelPath::new(&format!("{USER_UNIT_DIR}/{}", entry.key)) {
                            Ok(path) => path,
                            Err(d) => {
                                self.push(d, entry.key_span.clone());
                                continue;
                            }
                        };
                        let file = FileEntry {
                            bytes: text.into_bytes(),
                            table: format!("services.{}", entry.key),
                            ..placeholder(path, "")
                        };
                        self.add(manifest, file, entry.key_span.clone());
                    }
                    _ => self.unknown(&field, &place, SERVICE_KEYS),
                }
            }
            manifest.services.insert(entry.key.to_string(), enable);
        }
    }

    /// The XDG configuration directory relative to the home directory, or `E_CONFIG` when it is
    /// outside it (§4.1): nothing is rendered or declared below a directory Lodi may not write.
    fn xdg_prefix(&mut self, entry: &Entry<'_>, what: &str) -> Option<String> {
        match self.env.xdg_config_home.strip_prefix(&self.env.home) {
            Ok(rest) => match rest.to_str() {
                Some(rest) => Some(rest.to_string()),
                None => {
                    self.error(
                        "E_CONFIG",
                        "the XDG configuration directory is not UTF-8".into(),
                        entry.key_span.clone(),
                    );
                    None
                }
            },
            Err(_) => {
                self.error_hint(
                    "E_CONFIG",
                    format!(
                        "{what} renders below the XDG configuration directory, and \
                         XDG_CONFIG_HOME is outside the home directory"
                    ),
                    "set XDG_CONFIG_HOME to a directory below $HOME, or unset it".into(),
                    entry.key_span.clone(),
                );
                None
            }
        }
    }

    /// One file table's entry: zero entries when it is disabled or wrong, one entry, or — for a
    /// `[home.file]` whose `source` is a directory — one entry per regular file below it.
    fn file(
        &mut self,
        table: &dyn toml_edit::TableLike,
        path: RelPath,
        place: &str,
        key_span: Option<Range<usize>>,
        table_name: &str,
    ) -> Vec<FileEntry> {
        let mut content: Option<(String, Option<Range<usize>>)> = None;
        let mut source: Option<(String, Option<Range<usize>>)> = None;
        // A `content` or `source` that was written and rejected is still a declaration: the
        // entry must not also be told that it declares neither.
        let mut declared = false;
        let mut mode_text = DEFAULT_MODE.to_string();
        let mut mode = 0o644;
        let mut mode_span: Option<Option<Range<usize>>> = None;
        let mut executable: Option<(bool, Option<Range<usize>>)> = None;
        let mut state = FileState::Managed;
        let mut backup = true;
        let mut on_remove: Option<OnRemove> = None;
        let mut enable = true;
        for entry in entries(table) {
            match entry.key {
                "text" => {
                    declared = true;
                    if let Some(text) = self.string(&entry, place) {
                        content = Some((text, Self::span_of(&entry)));
                    }
                }
                "source" => {
                    declared = true;
                    if let Some(text) = self.string(&entry, place) {
                        source = Some((text, Self::span_of(&entry)));
                    }
                }
                "mode" => {
                    mode_span = Some(Self::span_of(&entry));
                    if let Some(text) = self.string(&entry, place)
                        && let Some(bits) = self.mode(&entry, &text, place)
                    {
                        mode_text = text;
                        mode = bits;
                    }
                }
                "executable" => {
                    if let Some(value) = self.boolean(&entry, place) {
                        executable = Some((value, Self::span_of(&entry)));
                    }
                }
                "state" => {
                    if let Some(value) = self.choice(&entry, place, &["present", "absent", "seed"])
                    {
                        state = match value.as_str() {
                            "absent" => FileState::Absent,
                            "seed" => FileState::Seed,
                            _ => FileState::Managed,
                        };
                    }
                }
                "backup" => {
                    if let Some(value) = self.boolean(&entry, place) {
                        backup = value;
                    }
                }
                "on_remove" => on_remove = self.on_remove(&entry, place).or(on_remove),
                "enable" => {
                    if let Some(value) = self.boolean(&entry, place) {
                        enable = value;
                    }
                }
                _ => self.unknown(&entry, place, HOME_FILE_KEYS),
            }
        }
        if let Some((value, span)) = executable {
            if mode_span.is_some() {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares both `mode` and `executable`"),
                    "`executable = true` is mode \"0755\"; keep one of them".into(),
                    span,
                );
                return Vec::new();
            }
            if value {
                mode = 0o755;
                mode_text = "0755".into();
            }
        }
        let on_remove = on_remove.unwrap_or(match state {
            FileState::Seed => OnRemove::Keep,
            _ => OnRemove::Restore,
        });
        let Some(origins) = self.origin(content, source, declared, state, place, key_span.clone())
        else {
            return Vec::new();
        };
        if !enable {
            return Vec::new();
        }
        origins
            .into_iter()
            .filter_map(|(below, origin, bytes)| {
                let path = match below {
                    None => path.clone(),
                    Some(below) => match RelPath::new(&format!("{}/{below}", path.as_str())) {
                        Ok(path) => path,
                        Err(d) => {
                            self.push(d, key_span.clone());
                            return None;
                        }
                    },
                };
                Some(FileEntry {
                    path,
                    origin,
                    bytes,
                    mode,
                    mode_text: mode_text.clone(),
                    state,
                    backup,
                    on_remove,
                    table: table_name.to_string(),
                })
            })
            .collect()
    }

    fn on_remove(&mut self, entry: &Entry<'_>, place: &str) -> Option<OnRemove> {
        let value = self.choice(entry, place, &["restore", "delete", "keep"])?;
        Some(match value.as_str() {
            "delete" => OnRemove::Delete,
            "keep" => OnRemove::Keep,
            _ => OnRemove::Restore,
        })
    }

    /// Exactly one of the text key and `source` describes a present file; an absent one
    /// declares neither, because there is nothing for it to hold. Each result is the path below
    /// a directory `source` (`None` for the declared path itself), the origin and the bytes.
    #[allow(clippy::type_complexity)]
    fn origin(
        &mut self,
        content: Option<(String, Option<Range<usize>>)>,
        source: Option<(String, Option<Range<usize>>)>,
        declared: bool,
        state: FileState,
        place: &str,
        key_span: Option<Range<usize>>,
    ) -> Option<Vec<(Option<String>, Origin, Vec<u8>)>> {
        if state == FileState::Absent {
            if !declared {
                return Some(vec![(None, Origin::Content, Vec::new())]);
            }
            if let Some((_, span)) = content.or(source) {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} is `state = \"absent\"` and still declares its content"),
                    "an absent file holds nothing: drop `text` and `source`, or make the entry \
                     present"
                        .into(),
                    span,
                );
            }
            return None;
        }
        match (content, source) {
            (Some((text, _)), None) => Some(vec![(None, Origin::Content, text.into_bytes())]),
            (None, Some((text, span))) => self.source(&text, span, place),
            (Some(_), Some((_, span))) => {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares both `text` and `source`"),
                    "a file's bytes come from one of them; delete the other".into(),
                    span,
                );
                None
            }
            (None, None) if declared => None,
            (None, None) => {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares neither `text` nor `source`"),
                    "give the file its bytes with `text`, or name a file with `source`".into(),
                    key_span,
                );
                None
            }
        }
    }

    /// `source` is relative to the manifest's own directory and must stay inside it. It is read
    /// verbatim here, so a plan never opens it again. A directory expands into every regular file
    /// below it, in lexicographic order.
    #[allow(clippy::type_complexity)]
    fn source(
        &mut self,
        text: &str,
        span: Option<Range<usize>>,
        place: &str,
    ) -> Option<Vec<(Option<String>, Origin, Vec<u8>)>> {
        let cleaned = text.strip_prefix("./").unwrap_or(text);
        let cleaned = cleaned.strip_suffix('/').unwrap_or(cleaned);
        let rel = match RelPath::new(cleaned) {
            Ok(rel) => rel,
            Err(d) => {
                self.push(d, span);
                return None;
            }
        };
        if let Some(reader) = self.host_source {
            let kind = match reader.kind(&rel) {
                Ok(kind) => kind,
                Err(d) => {
                    self.push(d, span);
                    return None;
                }
            };
            return match kind {
                Some(kind) if kind.is_dir() => self.expand(&rel, text, span, place),
                Some(kind) if kind.is_file() => match reader.read(&rel) {
                    Ok(bytes) => Some(vec![(None, Origin::Source(rel), bytes)]),
                    Err(d) => {
                        self.push(d, span);
                        None
                    }
                },
                Some(_) => {
                    self.error_hint(
                        "E_PATH_ESCAPE",
                        format!("the `source` of {place}, `{text}`, is not a regular file"),
                        "name a regular file or directory".into(),
                        span,
                    );
                    None
                }
                None => {
                    self.error_hint(
                        "E_CONFIG",
                        format!("the `source` of {place}, `{text}`, is not there"),
                        "the source is relative to the manifest".into(),
                        span,
                    );
                    None
                }
            };
        }
        let path: PathBuf = self.base.path().join(rel.as_str());
        if std::fs::read_link(&path).is_ok() {
            self.error_hint(
                "E_CONFIG",
                format!("the `source` of {place}, `{text}`, is a symbolic link"),
                "point `source` at the regular file itself; lodi never follows a link here".into(),
                span,
            );
            return None;
        }
        match std::fs::metadata(&path) {
            Err(_) => {
                self.error_hint(
                    "E_CONFIG",
                    format!("the `source` of {place}, `{text}`, is not there"),
                    "`source` is relative to the directory home.toml is in".into(),
                    span,
                );
                None
            }
            Ok(meta) if meta.is_dir() => self.expand(&rel, text, span, place),
            Ok(meta) if !meta.is_file() => {
                self.error_hint(
                    "E_CONFIG",
                    format!("the `source` of {place}, `{text}`, is not a regular file"),
                    "`source` names one file, never a directory or a device".into(),
                    span,
                );
                None
            }
            Ok(_) => match fsops::read(self.base, &rel) {
                Ok(bytes) => Some(vec![(None, Origin::Source(rel), bytes)]),
                Err(d) => {
                    self.push(d, span);
                    None
                }
            },
        }
    }

    /// A directory `source`: every regular file below it, depth first in lexicographic order,
    /// each read through [`fsops`]. A symbolic link or a special file anywhere below it is
    /// `E_PATH_ESCAPE`, and so is a name that is not UTF-8; an empty tree is `E_CONFIG`.
    #[allow(clippy::type_complexity)]
    fn expand(
        &mut self,
        dir: &RelPath,
        text: &str,
        span: Option<Range<usize>>,
        place: &str,
    ) -> Option<Vec<(Option<String>, Origin, Vec<u8>)>> {
        let mut out = Vec::new();
        let mut ok = true;
        // A stack of directories still to list, relative to `dir` ("" is `dir` itself).
        let mut pending: Vec<String> = vec![String::new()];
        let mut found: Vec<String> = Vec::new();
        while let Some(below) = pending.pop() {
            let at = if below.is_empty() {
                dir.clone()
            } else {
                match RelPath::new(&format!("{}/{below}", dir.as_str())) {
                    Ok(at) => at,
                    Err(d) => {
                        self.push(d, span.clone());
                        return None;
                    }
                }
            };
            let listed = match self.host_source {
                Some(reader) => reader.entries(&at),
                None => fsops::entries(self.base, &at),
            };
            let listed = match listed {
                Ok(listed) => listed.unwrap_or_default(),
                Err(d) => {
                    self.push(d, span.clone());
                    return None;
                }
            };
            for (name, kind) in listed {
                let Some(name) = name.to_str() else {
                    self.error_hint(
                        "E_PATH_ESCAPE",
                        format!(
                            "the `source` of {place}, `{text}`, holds a name that is not UTF-8"
                        ),
                        "rename it; a home path is UTF-8".into(),
                        span.clone(),
                    );
                    ok = false;
                    continue;
                };
                let child = if below.is_empty() {
                    name.to_string()
                } else {
                    format!("{below}/{name}")
                };
                if !(kind.is_dir() || kind.is_file()) {
                    let special = kind.is_block_device()
                        || kind.is_char_device()
                        || kind.is_fifo()
                        || kind.is_socket();
                    let what = if special {
                        "a special file"
                    } else {
                        "a symbolic link"
                    };
                    self.error_hint(
                        "E_PATH_ESCAPE",
                        format!("`{child}` below the `source` of {place}, `{text}`, is {what}"),
                        "a directory `source` holds regular files and directories only; lodi \
                         never follows a link"
                            .into(),
                        span.clone(),
                    );
                    ok = false;
                } else if kind.is_dir() {
                    pending.push(child);
                } else {
                    found.push(child);
                }
            }
        }
        if !ok {
            return None;
        }
        found.sort();
        if found.is_empty() {
            self.error_hint(
                "E_CONFIG",
                format!("the `source` of {place}, `{text}`, is a directory with no file in it"),
                "a directory `source` declares one file per regular file below it".into(),
                span,
            );
            return None;
        }
        for child in found {
            let rel = match RelPath::new(&format!("{}/{child}", dir.as_str())) {
                Ok(rel) => rel,
                Err(d) => {
                    self.push(d, span.clone());
                    return None;
                }
            };
            let bytes = match self.host_source {
                Some(reader) => reader.read(&rel),
                None => fsops::read(self.base, &rel),
            };
            match bytes {
                Ok(bytes) => out.push((Some(child), Origin::Source(rel), bytes)),
                Err(d) => {
                    self.push(d, span.clone());
                    return None;
                }
            }
        }
        Some(out)
    }

    /// `[programs]`: each table names a module of the registry, is read by it through
    /// [`programs::Fields`], and — when enabled — renders into ordinary file entries, each
    /// headed by the ownership comment and carrying the table's `extra` (§3.1, §4, §6).
    fn programs(&mut self, table: &dyn toml_edit::TableLike, manifest: &mut HomeManifest) {
        let mut uses = Vec::new();
        let env = RenderEnv {
            tools: !manifest.tools.is_empty(),
            starship_init: self.starship_init(table),
            ..self.env.clone()
        };
        for entry in entries(table) {
            let Some(module) = self.registry.iter().find(|m| m.name == entry.key) else {
                self.unknown_program(&entry);
                continue;
            };
            let place = format!("[programs.{}]", entry.key);
            let Some(t) = self.table(&entry, "[programs]") else {
                continue;
            };
            let before = self.diagnostics.len();
            let mut enable = true;
            let mut backup = true;
            let mut on_remove: Option<OnRemove> = None;
            let mut extra: Option<(String, Option<Range<usize>>)> = None;
            let mut owned = Table::new();
            let mut spots = BTreeMap::new();
            for key in entries(t) {
                match key.key {
                    "enable" => {
                        if let Some(value) = self.boolean(&key, &place) {
                            enable = value;
                        }
                    }
                    "backup" => {
                        if let Some(value) = self.boolean(&key, &place) {
                            backup = value;
                        }
                    }
                    "on_remove" => on_remove = self.on_remove(&key, &place).or(on_remove),
                    "extra" => {
                        if let Some(text) = self.string(&key, &place) {
                            extra = Some((text, key.item.span()));
                        }
                    }
                    name => {
                        self.spots(&key, &mut vec![name.to_string()], &mut spots);
                        if let Some(value) = self.owned_item(key.item) {
                            owned.insert(name.to_string(), value);
                        }
                    }
                }
            }
            let at = entry.key_span.clone().map(|s| self.location(s.start));
            let base = self.base;
            let host_source = self.host_source;
            let snippet_place = place.clone();
            let reader = move |text: &str| read_snippet(base, text, &snippet_place, host_source);
            let mut fields =
                programs::Fields::new(&owned, &spots, place.clone(), at).reading(&reader);
            let output = (module.run)(&mut fields, &env);
            self.diagnostics.extend(fields.finish());
            if self.diagnostics.len() > before || !enable {
                continue;
            }
            let Some(output) = output else {
                continue;
            };
            // Every module renders below the home directory; an XDG configuration directory
            // outside it is refused here, before any module renders into it (§4.1).
            if self.xdg_prefix(&entry, &place).is_none() {
                continue;
            }
            let table_name = format!("programs.{}", module.name);
            let mut paths = Vec::new();
            let mut extra_used = false;
            for rendered in output.files {
                let Some(path) = self.rendered_path(&rendered.path, &place, &entry) else {
                    continue;
                };
                let state = match rendered.state {
                    FileState::Absent => {
                        self.error(
                            "E_CONFIG",
                            format!("{place} rendered `{path}` as absent, which no module may"),
                            entry.key_span.clone(),
                        );
                        continue;
                    }
                    state => state,
                };
                let this_extra = match (&extra, rendered.takes_extra && !extra_used) {
                    (Some((text, _)), true) => {
                        extra_used = true;
                        Some(text.as_str())
                    }
                    _ => None,
                };
                let bytes = match programs::compose(module.name, &rendered.body, this_extra) {
                    Ok(bytes) => bytes,
                    Err(problem) => {
                        let span = extra.as_ref().and_then(|(_, span)| span.clone());
                        self.extra_problem(problem, &place, span);
                        continue;
                    }
                };
                paths.push(path.as_str().to_string());
                let file = FileEntry {
                    path,
                    origin: Origin::Program(module.name.to_string()),
                    bytes,
                    mode: PROGRAM_MODE,
                    mode_text: DEFAULT_MODE.to_string(),
                    state,
                    backup,
                    on_remove: on_remove.unwrap_or(match state {
                        FileState::Seed => OnRemove::Keep,
                        _ => OnRemove::Restore,
                    }),
                    table: table_name.clone(),
                };
                self.add(manifest, file, entry.key_span.clone());
            }
            if let (Some((_, span)), false) = (&extra, extra_used) {
                self.error_hint(
                    "E_UNKNOWN_ATTR",
                    format!("{place} renders no file that takes `extra`"),
                    "remove `extra`".into(),
                    span.clone(),
                );
                continue;
            }
            paths.sort();
            uses.push(ProgramUse {
                name: module.name.to_string(),
                executable: module.executable.to_string(),
                paths,
                shadowed_by: output.shadowed_by,
                hook: output.hook,
            });
        }
        uses.sort_by(|a: &ProgramUse, b| a.name.cmp(&b.name));
        manifest.programs = uses;
    }

    /// The shells whose file carries starship's `init` line (§5, §6.10), read before any module
    /// renders: none unless the registry has `starship` and its table is declared and not
    /// disabled; then the shells its `shells` names, or every declared shell module. A malformed
    /// `shells` selects nothing here, and the module refuses it.
    fn starship_init(&self, table: &dyn toml_edit::TableLike) -> BTreeSet<String> {
        use programs::starship;
        let known = |name: &str| self.registry.iter().any(|m| m.name == name);
        let Some(t) = table
            .get("starship")
            .and_then(Item::as_table_like)
            .filter(|_| known("starship"))
        else {
            return BTreeSet::new();
        };
        if t.get("enable").and_then(Item::as_bool) == Some(false) {
            return BTreeSet::new();
        }
        let shells: Option<Vec<String>> = t.get("shells").map(|item| {
            item.as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        });
        let declared: Vec<&str> = starship::SHELLS
            .iter()
            .copied()
            .filter(|shell| known(shell) && table.contains_key(shell))
            .collect();
        starship::init_shells(shells.as_deref(), &declared)
    }

    fn location(&self, offset: usize) -> Location {
        Location::at(self.file, self.text, offset)
    }

    /// Where each key below a program table is written: the key for `E_UNKNOWN_ATTR`, its value
    /// for `E_TYPE`.
    fn spots(
        &self,
        entry: &Entry<'_>,
        path: &mut Vec<String>,
        out: &mut BTreeMap<Vec<String>, programs::Spot>,
    ) {
        let key = entry.key_span.clone().map(|s| self.location(s.start));
        let value = Self::span_of(entry).map(|s| self.location(s.start));
        out.insert(path.clone(), programs::Spot { key, value });
        if let Some(table) = entry.item.as_table_like() {
            for inner in entries(table) {
                path.push(inner.key.to_string());
                self.spots(&inner, path, out);
                path.pop();
            }
        }
    }

    /// An `extra` [`programs::compose`] refused: a TOML one that does not parse is `E_SYNTAX`
    /// pointing inside the string, and a key it shares with a typed option is `E_ATTR_CONFLICT`.
    fn extra_problem(
        &mut self,
        problem: programs::ExtraProblem,
        place: &str,
        span: Option<Range<usize>>,
    ) {
        match problem {
            programs::ExtraProblem::Syntax { message, offset } => {
                let at = span.map(|span| inside_string(self.text, span, offset));
                let d = Diagnostic::new(
                    "E_SYNTAX",
                    format!("the `extra` of {place} is not TOML: {message}"),
                )
                .hint("`extra` of a TOML format is TOML text, merged with the typed keys");
                self.diagnostics.push(match at {
                    Some(offset) => d.at(self.location(offset)),
                    None => d,
                });
            }
            programs::ExtraProblem::Conflict(key) => self.error_hint(
                "E_ATTR_CONFLICT",
                format!("`{key}` is set by {place} and again in its `extra`"),
                "declare it once".into(),
                span,
            ),
        }
    }

    /// A module's absolute path as a path relative to the home directory, or `E_CONFIG` when it
    /// is outside it (a data root outside the home directory, for the shell modules).
    fn rendered_path(
        &mut self,
        path: &std::path::Path,
        place: &str,
        entry: &Entry<'_>,
    ) -> Option<RelPath> {
        let Ok(rest) = path.strip_prefix(&self.env.home) else {
            self.error_hint(
                "E_CONFIG",
                format!("{place} renders a file outside the home directory"),
                "the XDG directories and LODI_HOME must be below $HOME for a module to render \
                 there"
                    .into(),
                entry.key_span.clone(),
            );
            return None;
        };
        let Some(rest) = rest.to_str() else {
            self.error(
                "E_CONFIG",
                format!("{place} renders a path that is not UTF-8"),
                entry.key_span.clone(),
            );
            return None;
        };
        match RelPath::new(rest) {
            Ok(rel) => Some(rel),
            Err(d) => {
                self.push(d, entry.key_span.clone());
                None
            }
        }
    }

    /// `E_UNKNOWN_BLOCK` for a module this build does not ship: the nearest name as the hint and
    /// the shipped list in the message (§3.1).
    fn unknown_program(&mut self, entry: &Entry<'_>) {
        let names: Vec<&str> = self.registry.iter().map(|m| m.name).collect();
        let shipped = if names.is_empty() {
            "this build ships no program module".to_string()
        } else {
            format!("the modules of this build are: {}", names.join(", "))
        };
        let hint = match suggestion(entry.key, &names) {
            Some(near) => format!("did you mean `{near}`?"),
            None => "declare the file with [home.file] instead".to_string(),
        };
        self.error_hint(
            "E_UNKNOWN_BLOCK",
            format!(
                "`[programs.{}]` names no program module; {shipped}",
                entry.key
            ),
            hint,
            entry.key_span.clone(),
        );
    }

    /// A program table's value, owned, with every string held to the substitution rule the rest
    /// of the manifest follows (`spec/01` §1.3).
    fn owned_item(&mut self, item: &Item) -> Option<TomlValue> {
        match item {
            Item::None => None,
            Item::Value(value) => self.owned_value(value),
            Item::Table(table) => Some(TomlValue::Table(self.owned_table(table))),
            Item::ArrayOfTables(array) => Some(TomlValue::Array(
                array
                    .iter()
                    .map(|t| TomlValue::Table(self.owned_table(t)))
                    .collect(),
            )),
        }
    }

    fn owned_table(&mut self, table: &dyn toml_edit::TableLike) -> Table {
        let mut out = Table::new();
        for (key, item) in table.iter() {
            if let Some(value) = self.owned_item(item) {
                out.insert(key.to_string(), value);
            }
        }
        out
    }

    fn owned_value(&mut self, value: &Value) -> Option<TomlValue> {
        match value {
            Value::String(s) => self
                .substitute(s.value(), value.span())
                .map(TomlValue::String),
            Value::Array(items) => Some(TomlValue::Array(
                items.iter().filter_map(|v| self.owned_value(v)).collect(),
            )),
            Value::InlineTable(table) => Some(TomlValue::Table(self.owned_table(table))),
            other => Some(crate::home::toml::from_value(other)),
        }
    }

    /// Three or four octal digits. A set-uid, set-gid or sticky bit is `E_UNSUPPORTED`: the home
    /// scope writes plain files only.
    fn mode(&mut self, entry: &Entry<'_>, text: &str, place: &str) -> Option<u32> {
        let span = Self::span_of(entry);
        let digits = text.len();
        if !(3..=4).contains(&digits) || !text.bytes().all(|b| (b'0'..=b'7').contains(&b)) {
            self.error_hint(
                "E_TYPE",
                format!("`mode` in {place} is \"{text}\""),
                "write three or four octal digits, like \"0644\"".into(),
                span,
            );
            return None;
        }
        let bits = u32::from_str_radix(text, 8).expect("four octal digits parse");
        if bits & SPECIAL_BITS != 0 {
            self.error_hint(
                "E_UNSUPPORTED",
                format!(
                    "`mode` in {place} is \"{text}\", which sets a set-uid, set-gid or sticky bit"
                ),
                "the home scope writes plain files only; use a mode below 1000".into(),
                span,
            );
            return None;
        }
        Some(bits)
    }
}

/// A stand-in entry that only carries a path and a table, for reporting a duplicate that was
/// never parsed.
/// One `snippets` file of `place` (§5): relative to the manifest's directory and inside it
/// (`E_PATH_ESCAPE`), a regular file and not a link (`E_CONFIG`), read verbatim now so that a
/// plan never opens it again — as a `source` is.
fn read_snippet(
    base: &Root,
    text: &str,
    place: &str,
    source: Option<HostSource<'_>>,
) -> Result<Vec<u8>, Diagnostic> {
    let cleaned = text.strip_prefix("./").unwrap_or(text);
    let rel = RelPath::new(cleaned)?;
    if let Some(source) = source {
        return source.read(&rel);
    }
    let path: PathBuf = base.path().join(rel.as_str());
    let refuse = |what: &str| {
        Diagnostic::new(
            "E_CONFIG",
            format!("the snippet `{text}` of {place} {what}"),
        )
        .hint("`snippets` names regular files relative to the directory home.toml is in")
    };
    if std::fs::read_link(&path).is_ok() {
        return Err(refuse("is a symbolic link"));
    }
    match std::fs::metadata(&path) {
        Err(_) => Err(refuse("is not there")),
        Ok(meta) if !meta.is_file() => Err(refuse("is not a regular file")),
        Ok(_) => fsops::read(base, &rel),
    }
}

fn placeholder(path: RelPath, table: &str) -> FileEntry {
    FileEntry {
        path,
        origin: Origin::Content,
        bytes: Vec::new(),
        mode: 0o644,
        mode_text: DEFAULT_MODE.to_string(),
        state: FileState::Managed,
        backup: true,
        on_remove: OnRemove::Restore,
        table: table.to_string(),
    }
}

/// The offset in the manifest of byte `offset` of a string value whose token spans `span`: past
/// the opening quote — and, for a multi-line string, past the newline TOML drops after it — so
/// that an `extra` written as a literal string (`'''…'''`) is pointed at exactly. In a basic
/// string an escape before the point shifts it by the escape's length; it stays inside the string.
fn inside_string(text: &str, span: Range<usize>, offset: usize) -> usize {
    let raw = text.get(span.clone()).unwrap_or("");
    let mut start = span.start;
    if raw.starts_with("'''") || raw.starts_with("\"\"\"") {
        start += 3;
        let rest = &text[start.min(text.len())..];
        if rest.starts_with("\r\n") {
            start += 2;
        } else if rest.starts_with('\n') {
            start += 1;
        }
    } else if raw.starts_with('\'') || raw.starts_with('"') {
        start += 1;
    }
    (start + offset).min(span.end.saturating_sub(1).max(span.start))
}

/// `spec/01` §2.2's identifier grammar, as [`crate::manifest`] applies it.
fn is_ident(s: &str) -> bool {
    !s.is_empty()
        && s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

/// The names `spec/01` §2.2 allows inside `${ }`. A copy of [`crate::manifest`]'s own list: the
/// two scopes must refuse the same string with the same code, and this module is a sibling of
/// that one rather than an extension of it.
fn is_substitution_name(name: &str) -> bool {
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
        ["tools", tool, "path" | "version"] => is_ident(tool),
        _ => false,
    }
}
