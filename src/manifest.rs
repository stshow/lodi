//! The project manifest (`lodi.toml`): the strict subset of design `spec/01` §3 that this build
//! supports. `docs/milestones/m-0.3/DATA_MODEL.md` lists every supported key, every refused key
//! and the diagnostic code each refusal carries (`docs/milestones/m-spike/DATA_MODEL.md` is the
//! frozen record of the spike's own subset, LD-40).
//!
//! Syntax is TOML 1.0.0 (`spec/01` §1.1). Keys that `spec/01` defines but this build does not
//! implement fail with `E_UNSUPPORTED`; keys it does not define fail with `E_UNKNOWN_ATTR` or
//! `E_UNKNOWN_BLOCK`. Every problem in a file is reported in one run (`spec/01` §7). Loading
//! never executes anything: task text is data here.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Range;
use std::path::Path;

use toml_edit::{ImDocument, Item, Key, Value};

use crate::diag::{Diagnostic, EXIT_MANIFEST, Location};
use crate::tools::{self, ToolSet, is_env_name, is_ident, is_package_name};

/// The `[tools]` schema lives in [`crate::tools`], which knows nothing about this scope
/// (design call D11): the home scope of 0.5 parses the same table with the same module.
pub use crate::tools::ToolDecl as Tool;

/// Largest accepted manifest file (`spec/01` §1.4).
pub const MAX_MANIFEST_BYTES: usize = 1024 * 1024;

/// A validated project manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectManifest {
    pub project: Project,
    /// Present only in container mode (`spec/01` §3.6).
    pub container: Option<Container>,
    /// `packages.common`, duplicates collapsed, in first-seen order.
    pub packages: Vec<String>,
    pub tools: ToolSet,
    /// `[env]` in declaration order; arrays are already joined with `:`.
    pub env: Vec<(String, String)>,
    pub tasks: BTreeMap<String, Task>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// The manifest schema version; only `"1"` exists.
    pub schema_version: String,
    /// `project.name` as written, if present.
    pub name: Option<String>,
}

impl ProjectManifest {
    /// The environment name: `project.name`, else the project directory's basename, sanitized
    /// to `[a-z0-9-]` (`spec/01` §3.2). `None` when nothing usable remains.
    pub fn environment_name(&self, project_root: &Path) -> Option<String> {
        let source = match &self.project.name {
            Some(name) => name.clone(),
            None => project_root.file_name()?.to_string_lossy().into_owned(),
        };
        let name = sanitize_name(&source);
        (!name.is_empty()).then_some(name)
    }

    /// Whether `[container]` selects container mode (otherwise host-tool mode).
    pub fn is_container_mode(&self) -> bool {
        self.container.is_some()
    }
}

/// Lower-case, map every character outside `[a-z0-9-]` to `-`, collapse and trim dashes.
pub fn sanitize_name(name: &str) -> String {
    let mut out = String::new();
    for c in name.to_lowercase().chars() {
        let c = if c.is_ascii_lowercase() || c.is_ascii_digit() {
            c
        } else {
            '-'
        };
        if !(c == '-' && (out.is_empty() || out.ends_with('-'))) {
            out.push(c);
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Distro {
    Arch,
    Debian,
    Fedora,
    Ubuntu,
}

impl Distro {
    /// The name the base definition and the lock use.
    pub fn name(self) -> &'static str {
        match self {
            Distro::Arch => "arch",
            Distro::Debian => "debian",
            Distro::Fedora => "fedora",
            Distro::Ubuntu => "ubuntu",
        }
    }

    /// The distro a name spells out, if this build supports it. The set is the one
    /// `[container] distro` accepts and the one `lodi init --base` and `lodi shell --base`
    /// accept, so there is one answer to "which distributions does this build support".
    pub fn parse(name: &str) -> Option<Distro> {
        match name {
            "arch" => Some(Distro::Arch),
            "debian" => Some(Distro::Debian),
            "fedora" => Some(Distro::Fedora),
            "ubuntu" => Some(Distro::Ubuntu),
            _ => None,
        }
    }

    /// The releases of this distro this build supports, the canonical name first (design call
    /// D10, LD-55: one release per distro, because each release is an acceptance surface). A
    /// rolling distribution has exactly one name for its one release and no version alias.
    pub fn releases(self) -> &'static [&'static str] {
        match self {
            Distro::Arch => &["rolling"],
            Distro::Debian => &["bookworm", "12"],
            Distro::Fedora => &["44"],
            Distro::Ubuntu => &["noble", "24.04"],
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
}

/// `[container]`: the closure inputs this build supports and the runtime settings, which may
/// only take the values the spike implements.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Container {
    pub distro: Distro,
    /// Normalized to the codename: `bookworm` or `noble` in this build.
    pub release: String,
    pub arch: Arch,
    /// RFC 3339 UTC, `YYYY-MM-DDTHH:MM:SSZ`; `None` means locking pins it (OD-17).
    pub snapshot: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    /// The command line, executed later with `sh -c`; never run by the loader.
    pub run: String,
}

/// Every diagnostic from one manifest; exit status 3.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestErrors {
    pub diagnostics: Vec<Diagnostic>,
}

impl ManifestErrors {
    pub const EXIT_STATUS: u8 = EXIT_MANIFEST;

    /// The diagnostic codes in report order.
    pub fn codes(&self) -> Vec<&'static str> {
        self.diagnostics.iter().map(|d| d.code).collect()
    }
}

impl fmt::Display for ManifestErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, d) in self.diagnostics.iter().enumerate() {
            if i > 0 {
                writeln!(f)?;
            }
            write!(f, "{d}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ManifestErrors {}

/// Read and validate the manifest at `path`. `path` is shown in diagnostics as given.
pub fn load_project_manifest(path: &Path) -> Result<ProjectManifest, ManifestErrors> {
    let file = path.display().to_string();
    let fail = |d: Diagnostic| ManifestErrors {
        diagnostics: vec![d],
    };
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(fail(
                Diagnostic::new("E_NO_MANIFEST", format!("no manifest at {file}"))
                    .hint("run `lodi init` to create one"),
            ));
        }
        Err(e) => {
            return Err(fail(Diagnostic::new(
                "E_NO_MANIFEST",
                format!("cannot read {file}: {e}"),
            )));
        }
    };
    parse_project_manifest_bytes(&bytes, &file)
}

/// Validate manifest bytes; `file` names the source in diagnostics.
pub fn parse_project_manifest_bytes(
    bytes: &[u8],
    file: &str,
) -> Result<ProjectManifest, ManifestErrors> {
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(ManifestErrors {
            diagnostics: vec![Diagnostic::new(
                "E_SYNTAX",
                format!(
                    "manifest is {} bytes; the limit is {MAX_MANIFEST_BYTES} bytes (1 MiB)",
                    bytes.len()
                ),
            )],
        });
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => parse_project_manifest(text, file),
        Err(e) => {
            let valid = std::str::from_utf8(&bytes[..e.valid_up_to()]).unwrap_or("");
            Err(ManifestErrors {
                diagnostics: vec![
                    Diagnostic::new("E_SYNTAX", "manifest is not valid UTF-8").at(Location::at(
                        file,
                        valid,
                        valid.len(),
                    )),
                ],
            })
        }
    }
}

/// Validate manifest text; `file` names the source in diagnostics.
pub fn parse_project_manifest(text: &str, file: &str) -> Result<ProjectManifest, ManifestErrors> {
    let mut v = Validator {
        file,
        text,
        diagnostics: Vec::new(),
    };
    if text.len() > MAX_MANIFEST_BYTES {
        v.diagnostics.push(Diagnostic::new(
            "E_SYNTAX",
            format!(
                "manifest is {} bytes; the limit is {MAX_MANIFEST_BYTES} bytes (1 MiB)",
                text.len()
            ),
        ));
        return Err(v.finish_err());
    }
    let doc = match ImDocument::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            let mut d = Diagnostic::new("E_SYNTAX", first_line(e.message()));
            if let Some(span) = e.span() {
                d = d.at(Location::at(file, text, span.start));
            }
            v.diagnostics.push(d);
            return Err(v.finish_err());
        }
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

const TOP_LEVEL: &[&str] = &[
    "path",
    "project",
    "vars",
    "inputs",
    "modules",
    "container",
    "packages",
    "tools",
    "env",
    "hooks",
    "tasks",
    "profiles",
];
const PROJECT_KEYS: &[&str] = &[
    "version",
    "name",
    "description",
    "shell",
    "prompt",
    "default_profile",
    "registries",
    "min_lodi_version",
];
const CONTAINER_KEYS: &[&str] = &[
    "distro",
    "release",
    "arch",
    "snapshot",
    "unpinned",
    "mirror",
    "rootfs",
    "install_recommends",
    "locale",
    "home",
    "share",
    "persist",
    "network",
    "hostname",
    "pid",
    "user",
    "devices",
    "gui",
    "writable",
    "extra_args",
];
const PACKAGES_KEYS: &[&str] = &["common", "debian", "ubuntu", "arch", "x86_64", "aarch64"];
const HOST_PACKAGES_KEYS: &[&str] = &["hold", "mark_auto", "optional", "absent"];
const TASK_KEYS: &[&str] = &["run", "description", "cwd", "env", "depends_on"];
const RESERVED_ENV: &[&str] = &["PATH", "HOME", "USER", "SHELL"];

/// A table-like node and the span of the key that introduced it.
struct Entry<'a> {
    key: &'a str,
    key_span: Option<Range<usize>>,
    item: &'a Item,
}

fn entries<'a>(table: &'a dyn toml_edit::TableLike) -> Vec<Entry<'a>> {
    table
        .iter()
        .map(|(key, item)| {
            let key_span = table
                .get_key_value(key)
                .and_then(|(k, _): (&Key, _)| k.span());
            Entry {
                key,
                key_span,
                item,
            }
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

/// Levenshtein distance, for "did you mean" hints.
pub(crate) fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let current = row[j + 1];
            row[j + 1] = if ca == *cb {
                previous
            } else {
                1 + previous.min(row[j]).min(row[j + 1])
            };
            previous = current;
        }
    }
    row[b.len()]
}

fn suggestion(key: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .map(|k| (distance(key, k), *k))
        .filter(|(d, k)| *d <= 2 && *d < k.len())
        .min()
        .map(|(_, k)| k.to_string())
}

struct Validator<'t> {
    file: &'t str,
    text: &'t str,
    diagnostics: Vec<Diagnostic>,
}

/// The project scope's diagnostic sink: [`crate::tools`] hands a span back and this scope —
/// which owns the file name and the text — turns it into a `spec/11` location.
impl tools::Sink for Validator<'_> {
    fn emit(&mut self, d: Diagnostic, span: Option<Range<usize>>) {
        let d = self.located(d, span);
        self.diagnostics.push(d);
    }
}

impl Validator<'_> {
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

    /// The span of an entry: its value when it has one, else its key.
    fn span_of(entry: &Entry<'_>) -> Option<Range<usize>> {
        entry.item.span().or_else(|| entry.key_span.clone())
    }

    /// Refuse a key the scope does not define (`E_UNKNOWN_BLOCK` for tables, else
    /// `E_UNKNOWN_ATTR`), with a "did you mean" hint from the names `spec/01` defines there.
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

    /// Refuse a key `spec/01` defines but this build does not implement.
    fn unsupported(&mut self, entry: &Entry<'_>, place: &str) {
        self.error_hint(
            "E_UNSUPPORTED",
            format!("`{}` in {place} is not supported by this build", entry.key),
            "remove it: docs/scopes/project.md lists the keys this lodi supports".into(),
            entry.key_span.clone(),
        );
    }

    fn type_error(&mut self, entry: &Entry<'_>, place: &str, expected: &str) {
        let message = format!(
            "`{}` in {place} must be {expected}, found {}",
            entry.key,
            kind(entry.item)
        );
        let span = Self::span_of(entry);
        match entry.item {
            Item::Value(Value::Datetime(_)) => self.error_hint(
                "E_TYPE",
                message,
                "write timestamps as quoted RFC 3339 strings".into(),
                span,
            ),
            _ => self.error("E_TYPE", message, span),
        }
    }

    /// A table-like entry, or an `E_TYPE` error.
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

    /// A string entry after the `${ }` rules of `spec/01` §1.2–1.3, or `None` after reporting.
    fn string(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        match entry.item.as_str() {
            Some(s) => self.substitute(s, entry.item.span()),
            None => {
                self.type_error(entry, place, "a string");
                None
            }
        }
    }

    /// `spec/01` §1.3 for one string, applied by [`crate::tools::substitute`] so that every
    /// scope refuses the same constructs in the same words.
    fn substitute(&mut self, s: &str, span: Option<Range<usize>>) -> Option<String> {
        let (text, problems) = tools::substitute(s);
        let ok = problems.is_empty();
        for d in problems {
            tools::Sink::emit(self, d, span.clone());
        }
        ok.then_some(text)
    }

    fn root(&mut self, doc: &toml_edit::Table) -> ProjectManifest {
        let mut manifest = ProjectManifest {
            project: Project {
                schema_version: "1".into(),
                name: None,
            },
            container: None,
            packages: Vec::new(),
            tools: BTreeMap::new(),
            env: Vec::new(),
            tasks: BTreeMap::new(),
        };
        let mut packages_span = None;
        let mut container_seen = false;
        for entry in entries(doc) {
            let place = "the project manifest";
            match entry.key {
                "project" => {
                    if let Some(t) = self.table(&entry, place) {
                        self.project(t, &mut manifest.project);
                    }
                }
                "container" => {
                    container_seen = true;
                    if let Some(t) = self.table(&entry, place) {
                        manifest.container = self.container(t, &entry);
                    }
                }
                "packages" => {
                    packages_span = entry.key_span.clone().or_else(|| entry.item.span());
                    if let Some(t) = self.table(&entry, place) {
                        manifest.packages = self.packages(t);
                    }
                }
                "tools" => {
                    if let Some(t) = self.table(&entry, place) {
                        manifest.tools = self.tools(t);
                    }
                }
                "env" => {
                    if let Some(t) = self.table(&entry, place) {
                        manifest.env = self.env(t);
                    }
                }
                "tasks" => {
                    if let Some(t) = self.table(&entry, place) {
                        manifest.tasks = self.tasks(t);
                    }
                }
                "services" => self.error_hint(
                    "E_UNSUPPORTED",
                    "long-running services are deferred; `[services]` is not supported".into(),
                    "remove `[services]`: run the service yourself, such as from a task".into(),
                    entry.key_span.clone(),
                ),
                "path" | "vars" | "inputs" | "modules" | "hooks" | "profiles" => {
                    self.unsupported(&entry, place)
                }
                _ => self.unknown(&entry, place, TOP_LEVEL),
            }
        }
        if packages_span.is_some() && !container_seen {
            self.error_hint(
                "E_MODE",
                "distro packages need a `[container]` table".into(),
                "add [container] with distro and release, or move the tools to [tools]".into(),
                packages_span,
            );
        }
        manifest
    }

    fn project(&mut self, table: &dyn toml_edit::TableLike, project: &mut Project) {
        let place = "[project]";
        for entry in entries(table) {
            match entry.key {
                "version" => {
                    if let Some(version) = self.string(&entry, place) {
                        if version == "1" {
                            project.schema_version = version;
                        } else {
                            self.error_hint(
                                "E_UNSUPPORTED",
                                format!("manifest schema version \"{version}\" is not supported"),
                                "only version \"1\" exists".into(),
                                Self::span_of(&entry),
                            );
                        }
                    }
                }
                "name" => {
                    if let Some(name) = self.string(&entry, place) {
                        if sanitize_name(&name).is_empty() {
                            self.error(
                                "E_IDENT",
                                format!(
                                    "project name \"{name}\" has no characters left after \
                                     sanitizing to [a-z0-9-]"
                                ),
                                Self::span_of(&entry),
                            );
                        } else {
                            project.name = Some(name);
                        }
                    }
                }
                "min_lodi_version" => {
                    if let Some(text) = self.string(&entry, place) {
                        self.check_min_lodi_version(&text, Self::span_of(&entry));
                    }
                }
                key if PROJECT_KEYS.contains(&key) => self.unsupported(&entry, place),
                _ => self.unknown(&entry, place, PROJECT_KEYS),
            }
        }
    }

    /// `[project] min_lodi_version` is **honoured**, through the same
    /// [`crate::version::Constraint`] check the home scope (`src/home/manifest.rs`) and the host
    /// scope (`src/hostscope/manifest.rs`) already run (M-1.0 T-1, design call D6). It is read
    /// while the manifest is parsed, so it is decided before anything is resolved, fetched or
    /// written. An empty string declares no minimum, as it does in the home scope.
    fn check_min_lodi_version(&mut self, text: &str, span: Option<Range<usize>>) {
        if text.is_empty() {
            return;
        }
        let running = crate::version::Version::parse(crate::schema::LODI_VERSION)
            .expect("this build's own version parses as a version");
        match crate::version::Constraint::parse(text) {
            Ok(constraint) if constraint.matches(&running) => {}
            Ok(_) => self.error_hint(
                "E_VERSION",
                format!(
                    "[project] min_lodi_version = \"{text}\" is not satisfied by lodi {running}"
                ),
                "install a newer lodi, or lower min_lodi_version".into(),
                span,
            ),
            Err(e) => self.error_hint(
                "E_VERSION",
                format!("[project] min_lodi_version = \"{text}\" is not a version constraint: {e}"),
                "write a constraint such as \"0.3\" or \">=0.3.0\"".into(),
                span,
            ),
        }
    }

    /// A string from a fixed set: `supported` values are accepted, `defined` ones (valid in
    /// `spec/01` but not implemented) are `E_UNSUPPORTED`, anything else is `E_TYPE`.
    fn choice(
        &mut self,
        entry: &Entry<'_>,
        place: &str,
        supported: &[&str],
        defined: &[&str],
    ) -> Option<String> {
        let value = self.string(entry, place)?;
        if supported.contains(&value.as_str()) {
            return Some(value);
        }
        let span = Self::span_of(entry);
        if defined.contains(&value.as_str()) {
            self.error_hint(
                "E_UNSUPPORTED",
                format!(
                    "{place} {} = \"{value}\" is not supported by this build",
                    entry.key
                ),
                format!("supported: {}", supported.join(", ")),
                span,
            );
        } else {
            let mut all: Vec<&str> = supported.to_vec();
            all.extend_from_slice(defined);
            self.error(
                "E_TYPE",
                format!(
                    "{place} {} must be one of {}, found \"{value}\"",
                    entry.key,
                    all.join(", ")
                ),
                span,
            );
        }
        None
    }

    fn container(
        &mut self,
        table: &dyn toml_edit::TableLike,
        header: &Entry<'_>,
    ) -> Option<Container> {
        let place = "[container]";
        let mut distro = None;
        let mut release_written = None;
        let mut arch = Some(Arch::X86_64);
        let mut snapshot = None;
        let mut ok = true;
        for entry in entries(table) {
            match entry.key {
                "distro" => {
                    distro = self
                        .choice(&entry, place, &["arch", "debian", "fedora", "ubuntu"], &[])
                        .map(|name| match name.as_str() {
                            "arch" => Distro::Arch,
                            "fedora" => Distro::Fedora,
                            "ubuntu" => Distro::Ubuntu,
                            _ => Distro::Debian,
                        });
                    ok &= distro.is_some();
                }
                "release" => {
                    // Checked below: which releases exist depends on the distro.
                    release_written = self
                        .string(&entry, place)
                        .map(|r| (r, Self::span_of(&entry)));
                    ok &= release_written.is_some();
                }
                "arch" => {
                    arch = self
                        .choice(&entry, place, &["x86_64"], &["aarch64"])
                        .map(|_| Arch::X86_64);
                    ok &= arch.is_some();
                }
                "snapshot" => match entry.item.as_str() {
                    Some(_) => {
                        snapshot = self.string(&entry, place).and_then(|s| {
                            if is_rfc3339_utc(&s) {
                                Some(s)
                            } else {
                                self.error_hint(
                                    "E_TYPE",
                                    format!("container snapshot \"{s}\" is not an RFC 3339 UTC timestamp"),
                                    "write it as YYYY-MM-DDTHH:MM:SSZ, for example \"2026-09-18T00:00:00Z\"".into(),
                                    Self::span_of(&entry),
                                );
                                None
                            }
                        });
                        ok &= snapshot.is_some();
                    }
                    None => {
                        self.type_error(&entry, place, "a string");
                        ok = false;
                    }
                },
                // Runtime settings: only the values the spike implements (S-4).
                "home" => {
                    ok &= self
                        .choice(&entry, place, &["shared"], &["isolated", "none"])
                        .is_some()
                }
                "user" => ok &= self.choice(&entry, place, &["same"], &["root"]).is_some(),
                "writable" => {
                    ok &= self
                        .choice(&entry, place, &["ephemeral"], &["persistent"])
                        .is_some()
                }
                key if CONTAINER_KEYS.contains(&key) => {
                    self.unsupported(&entry, place);
                    ok = false;
                }
                _ => {
                    self.unknown(&entry, place, CONTAINER_KEYS);
                    ok = false;
                }
            }
        }
        let mut release = None;
        if let (Some(distro), Some((r, span))) = (distro, release_written) {
            let supported = distro.releases();
            if supported.contains(&r.as_str()) {
                release = Some(supported[0].to_string());
            } else {
                // A distro with one release has no alias to offer, so the hint lists whatever
                // names there are rather than assuming there are two.
                let names: Vec<String> = supported.iter().map(|r| format!("\"{r}\"")).collect();
                self.error_hint(
                    "E_UNSUPPORTED",
                    format!("container release \"{r}\" is not supported by this build"),
                    format!(
                        "this build supports {} {}",
                        distro.name(),
                        names.join(" (or ") + &")".repeat(names.len() - 1)
                    ),
                    span,
                );
                ok = false;
            }
        }
        let span = header.key_span.clone();
        if !table.contains_key("distro") {
            self.error(
                "E_TYPE",
                "[container] requires `distro`".into(),
                span.clone(),
            );
            ok = false;
        }
        if !table.contains_key("release")
            && let Some(distro) = distro
        {
            self.error(
                "E_TYPE",
                format!("[container] requires `release` for {}", distro.name()),
                span,
            );
            ok = false;
        }
        if !ok {
            return None;
        }
        Some(Container {
            distro: distro?,
            release: release?,
            arch: arch?,
            snapshot,
        })
    }

    fn packages(&mut self, table: &dyn toml_edit::TableLike) -> Vec<String> {
        let place = "[packages]";
        let mut packages = Vec::new();
        for entry in entries(table) {
            match entry.key {
                "common" => {
                    for name in self.string_array(&entry, place) {
                        if !is_package_name(&name) {
                            self.error(
                                "E_IDENT",
                                format!("package name \"{name}\" must match ^[a-z0-9][a-z0-9._+-]{{0,127}}$"),
                                Self::span_of(&entry),
                            );
                        } else if !packages.contains(&name) {
                            packages.push(name);
                        }
                    }
                }
                key if HOST_PACKAGES_KEYS.contains(&key) => self.error_hint(
                    "E_BLOCK_NOT_ALLOWED",
                    format!(
                        "`{key}` is a host-scope package key, not allowed in a project manifest"
                    ),
                    format!("remove `{key}`: it belongs in a machine's host.toml"),
                    entry.key_span.clone(),
                ),
                key if PACKAGES_KEYS.contains(&key) => self.unsupported(&entry, place),
                _ => self.unknown(&entry, place, PACKAGES_KEYS),
            }
        }
        packages
    }

    /// An array of strings, each through the `${ }` rules; errors are reported.
    fn string_array(&mut self, entry: &Entry<'_>, place: &str) -> Vec<String> {
        let Some(array) = entry.item.as_array() else {
            self.type_error(entry, place, "an array of strings");
            return Vec::new();
        };
        let mut out = Vec::new();
        for value in array.iter() {
            match value.as_str() {
                Some(s) => {
                    if let Some(s) = self.substitute(s, value.span()) {
                        out.push(s);
                    }
                }
                None => self.error(
                    "E_TYPE",
                    format!("`{}` in {place} must contain only strings", entry.key),
                    value.span().or_else(|| Self::span_of(entry)),
                ),
            }
        }
        out
    }

    /// `[tools]` is one call into [`crate::tools`], which owns the whole `spec/01` §3.8
    /// schema and knows nothing about this scope (design call D11). There is no list of
    /// supported tool names here any more: an unknown name reaches resolution and fails
    /// `E_NO_RECIPE` at exit 4 with the catalogue's nearest names (design call D10).
    fn tools(&mut self, table: &dyn toml_edit::TableLike) -> ToolSet {
        tools::parse(table, self)
    }

    fn env(&mut self, table: &dyn toml_edit::TableLike) -> Vec<(String, String)> {
        let place = "[env]";
        let mut env = Vec::new();
        for entry in entries(table) {
            let name = entry.key;
            if !is_env_name(name) {
                self.error(
                    "E_IDENT",
                    format!("environment variable name `{name}` must match ^[A-Za-z_][A-Za-z0-9_]{{0,255}}$"),
                    entry.key_span.clone(),
                );
                continue;
            }
            if RESERVED_ENV.contains(&name) || name.starts_with("LODI_") {
                let hint = if name == "PATH" {
                    "use the top-level `path` key for PATH"
                } else {
                    "PATH, HOME, USER, SHELL and LODI_* are managed by Lodi"
                };
                self.error_hint(
                    "E_IDENT",
                    format!("environment variable `{name}` is reserved"),
                    hint.into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let value = if entry.item.as_str().is_some() {
                self.string(&entry, place)
            } else if entry.item.as_array().is_some() {
                let before = self.diagnostics.len();
                let parts = self.string_array(&entry, place);
                (self.diagnostics.len() == before).then(|| parts.join(":"))
            } else {
                self.type_error(&entry, place, "a string or an array of strings");
                None
            };
            if let Some(value) = value {
                env.push((name.to_string(), value));
            }
        }
        env
    }

    fn tasks(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, Task> {
        let mut tasks = BTreeMap::new();
        for entry in entries(table) {
            let place = format!("[tasks.{}]", entry.key);
            if !is_ident(entry.key) {
                self.error(
                    "E_IDENT",
                    format!(
                        "task name `{}` must match ^[a-zA-Z_][a-zA-Z0-9_-]{{0,63}}$",
                        entry.key
                    ),
                    entry.key_span.clone(),
                );
                continue;
            }
            let Some(t) = self.table(&entry, "[tasks]") else {
                continue;
            };
            let mut run = None;
            let mut ok = true;
            for field in entries(t) {
                match field.key {
                    "run" => {
                        run = self.string(&field, &place);
                        ok &= run.is_some();
                    }
                    key if TASK_KEYS.contains(&key) => {
                        self.unsupported(&field, &place);
                        ok = false;
                    }
                    _ => {
                        self.unknown(&field, &place, TASK_KEYS);
                        ok = false;
                    }
                }
            }
            if !t.contains_key("run") {
                self.error(
                    "E_TYPE",
                    format!("{place} requires `run` (a string)"),
                    entry.key_span.clone(),
                );
            }
            if let (true, Some(run)) = (ok, run) {
                tasks.insert(entry.key.to_string(), Task { run });
            }
        }
        tasks
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` with a real calendar date and time.
fn is_rfc3339_utc(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 20 {
        return false;
    }
    let digits = |r: Range<usize>| -> Option<u32> {
        let part = &s[r];
        part.bytes()
            .all(|c| c.is_ascii_digit())
            .then(|| part.parse().ok())
            .flatten()
    };
    let shape = b[4] == b'-' && b[7] == b'-' && b[10] == b'T' && b[13] == b':' && b[16] == b':';
    if !shape || b[19] != b'Z' {
        return false;
    }
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        digits(0..4),
        digits(5..7),
        digits(8..10),
        digits(11..13),
        digits(14..16),
        digits(17..19),
    ) else {
        return false;
    };
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    let days = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return false,
    };
    (1..=days).contains(&day) && hour < 24 && minute < 60 && second < 60
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_keeps_lowercase_digits_and_single_dashes() {
        assert_eq!(sanitize_name("My App_2"), "my-app-2");
        assert_eq!(sanitize_name("--a..b--"), "a-b");
        assert_eq!(sanitize_name("!!!"), "");
    }

    #[test]
    fn identifier_patterns() {
        assert!(is_package_name("g++") && is_package_name("python3.12"));
        assert!(!is_package_name("Python") && !is_package_name("-x") && !is_package_name(""));
        assert!(is_ident("build_all-2") && !is_ident("2x") && !is_ident(&"a".repeat(65)));
        assert!(is_env_name("_X1") && !is_env_name("1X") && !is_env_name("A-B"));
    }

    #[test]
    fn rfc3339_utc_timestamps() {
        assert!(is_rfc3339_utc("2026-09-18T00:00:00Z"));
        assert!(is_rfc3339_utc("2024-02-29T23:59:59Z"));
        for bad in [
            "2023-02-29T00:00:00Z",
            "2026-13-01T00:00:00Z",
            "2026-09-18T24:00:00Z",
            "2026-09-18 00:00:00Z",
            "2026-09-18T00:00:00+00:00",
            "2026-09-18T00:00:00.5Z",
            "2026-09-18",
        ] {
            assert!(!is_rfc3339_utc(bad), "{bad}");
        }
    }

    #[test]
    fn substitution_names() {
        use crate::tools::is_substitution_name;
        assert!(is_substitution_name("project.root") && is_substitution_name("tools.nodejs.path"));
        assert!(!is_substitution_name("project.root + 1") && !is_substitution_name("env.HOME"));
    }

    #[test]
    fn suggestions_are_close_names() {
        assert_eq!(suggestion("nmae", PROJECT_KEYS).as_deref(), Some("name"));
        assert_eq!(suggestion("zzzzzz", PROJECT_KEYS), None);
    }
}
