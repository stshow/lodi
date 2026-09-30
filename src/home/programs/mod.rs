//! Home program modules: `[programs.<name>]` rendered into ordinary managed files (M-Home,
//! `docs/design/HOME_PROGRAMS.md` §3.1, §4, §6).
//!
//! A module is a [`Program`]: a typed value read from the module's table through [`Fields`]
//! ([`Program::read`]) and a pure function from that value and the directories of [`RenderEnv`]
//! to the bodies of one or more files ([`Program::render`]). What every module shares is done
//! **here, once**, and not by the module:
//!
//! - the common keys `enable`, `backup`, `on_remove` and `extra` (§3.1, §6) are consumed by the
//!   loader, so a module never sees them;
//! - an undeclared key is `E_UNKNOWN_ATTR` with the nearest key the module reads, a wrong type
//!   is `E_TYPE`, each at its own `file:line:column` ([`Fields`]);
//! - every rendered file starts with the ownership header of §4.2 in its format's comment syntax
//!   ([`header`]), with no date, version, user, host or path in it (I3, I12);
//! - `extra` is merged in canonical key order into a TOML body, a key set both ways being
//!   `E_ATTR_CONFLICT`, and appended byte-exact after a text body (I11) ([`compose`]).
//!
//! The manifest loader ([`crate::home::manifest`]) turns every rendered file into a file entry
//! exactly like a `[home.file]` entry, so it has the same state record, drift rule, backup rule,
//! plan line and status row as any other managed file (G5), at mode `0644` (no module has
//! `mode`).
//!
//! **What a module may not do** (invariants I1–I3, held by a source lint in
//! `tests/home_programs.rs`): spawn a process, read the environment or a clock, read or write a
//! file, or put a date, a version, a user, a host or an absolute path into what it renders.
//! Everything it needs is in its table and in [`RenderEnv`].
//!
//! Adding a module is its `pub mod` here, its [`module`] row in [`REGISTRY`] and its
//! [`Program::STUB`], the commented table `lodi home import` emits for it (§7). This build
//! ships `bash`, `fish`, `git` and `zsh` (M-Home pa-1) and `alacritty`, `helix`, `kitty`,
//! `nvim`, `starship` and `tmux` (pb-1); any other `[programs.<name>]` is
//! `E_UNKNOWN_BLOCK` naming the shipped list. Tests also hand the loader a registry of their own
//! (`manifest::parse_home_manifest_in`, `plan::plan_with`, `apply::apply_with`,
//! `apply::status_with`) to reach the framework through test modules.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::RangeInclusive;
use std::path::PathBuf;

use crate::diag::{Diagnostic, Location};
use crate::home::render;
use crate::home::toml::{Table, Value};

pub use crate::home::manifest::FileState;

pub mod alacritty;
pub mod bash;
pub mod fish;
pub mod git;
pub mod helix;
pub mod kitty;
pub mod nvim;
pub mod shell;
pub mod starship;
pub mod tmux;
pub mod zsh;

/// The directories a module may render into, already resolved from the environment by
/// [`crate::roots`]. Every path a module returns must be inside `home`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderEnv {
    /// The home directory.
    pub home: PathBuf,
    /// `$XDG_CONFIG_HOME`, or `~/.config` (§4.1). Inside `home`, or the loader refuses the
    /// manifest with `E_CONFIG` before any module renders into it.
    pub xdg_config_home: PathBuf,
    /// Lodi's own data root (`$LODI_HOME`, else `$XDG_DATA_HOME/lodi`): where a shell module
    /// renders (§5).
    pub lodi_data_root: PathBuf,
    /// Whether the manifest's `[tools]` declares a tool: a shell module then includes the
    /// generated tool profile (§5). Set by the loader, which reads `[tools]` first.
    pub tools: bool,
    /// The shells (`bash`, `zsh`, `fish`) whose file carries starship's guarded `init` line
    /// (§5, §6.10). Set by the loader from `[programs.starship]` before any module renders.
    pub starship_init: BTreeSet<String>,
}

impl RenderEnv {
    /// The directories of the process's own roots.
    pub fn of(roots: &crate::roots::Roots) -> RenderEnv {
        RenderEnv {
            home: roots.home().to_path_buf(),
            xdg_config_home: roots.xdg_config().to_path_buf(),
            lodi_data_root: roots.data().to_path_buf(),
            tools: false,
            starship_init: BTreeSet::new(),
        }
    }

    /// The default layout below a home directory that does not exist, for parsing a manifest
    /// without roots (`parse_home_manifest`): what a module renders is still checked, and no
    /// real directory is named.
    pub fn placeholder() -> RenderEnv {
        let home = PathBuf::from("/nonexistent-home");
        RenderEnv {
            xdg_config_home: home.join(".config"),
            lodi_data_root: home.join(".local/share/lodi"),
            home,
            tools: false,
            starship_init: BTreeSet::new(),
        }
    }
}

/// How a format writes a comment line: what the ownership header is written in (§4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Comment {
    /// `# …`: TOML, git-config, kitty, tmux and the shells.
    Hash,
    /// `-- …`: Lua.
    DoubleDash,
}

impl Comment {
    fn prefix(self) -> &'static str {
        match self {
            Comment::Hash => "#",
            Comment::DoubleDash => "--",
        }
    }
}

/// What a module renders into one file, before the header and `extra`.
#[derive(Debug, Clone, PartialEq)]
pub enum Body {
    /// A TOML document: the typed keys as a table, written by [`render::toml_document`] after a
    /// TOML `extra` is merged into it (§6).
    Toml(Table),
    /// Text in the application's own syntax, already written by one of [`render`]'s writers; an
    /// `extra` is appended to it verbatim.
    Text { comment: Comment, text: String },
}

impl Body {
    fn comment(&self) -> Comment {
        match self {
            Body::Toml(_) => Comment::Hash,
            Body::Text { comment, .. } => *comment,
        }
    }
}

/// One file a module renders.
#[derive(Debug, Clone, PartialEq)]
pub struct Rendered {
    /// Absolute, inside [`RenderEnv::home`].
    pub path: PathBuf,
    pub body: Body,
    /// [`FileState::Managed`] or [`FileState::Seed`]; never [`FileState::Absent`].
    pub state: FileState,
    /// Whether the table's `extra` goes into this file. At most one file of a module takes it.
    pub takes_extra: bool,
}

/// A program module.
pub trait Program: Sized {
    /// The table name: `[programs.<NAME>]`.
    const NAME: &'static str;

    /// The application's executable: when it is not on `PATH`, plan, apply and status print
    /// `W_PROGRAM_NOT_FOUND` and the file is rendered all the same (decision D1). The table name
    /// unless the module says otherwise.
    const EXECUTABLE: &'static str = Self::NAME;

    /// The commented stub `lodi home import` emits for this module when its executable is on
    /// `PATH` (§7), below a `# [programs.<NAME>]` line the importer writes: one line per key the
    /// module reads, each starting `# `, with a fixed example value and never a value taken from
    /// the machine. With the `# ` of every line removed it is a table the module accepts, and
    /// running the module over that table is how the importer learns which paths the module
    /// writes and is shadowed by ([`stub_paths`]). The common keys (`enable`, `backup`,
    /// `on_remove`, `extra`) are the loader's and are not in it. Empty for a module that ships
    /// no stub; every module of [`REGISTRY`] ships one (`tests/home_import.rs`).
    const STUB: &'static str = "";

    /// Read the module's typed keys (never `enable`, `backup`, `on_remove` or `extra`). Every
    /// problem is recorded in `fields`; `None` is returned only when one was.
    fn read(fields: &mut Fields<'_>) -> Option<Self>;

    /// The files, in any order; the loader sorts them by path. A pure function of `self` and
    /// `env`: every value that could be refused was refused by [`Program::read`].
    fn render(&self, env: &RenderEnv) -> Vec<Rendered>;

    /// Files the application reads before, or alongside, what [`Program::render`] writes (§4.6):
    /// each one that exists is `W_SHADOWED`. None by default.
    fn shadowed_by(&self, env: &RenderEnv) -> Vec<Shadow> {
        let _ = env;
        Vec::new()
    }

    /// The rendered file the user sources from the shell's own start-up file, whose line apply
    /// prints last and Lodi never adds (§5: bash and zsh). None by default.
    fn hook(&self, env: &RenderEnv) -> Option<PathBuf> {
        let _ = env;
        None
    }
}

/// A file an application reads before, or alongside, one Lodi renders (§4.6). Both paths are
/// absolute. Only whether `by` exists is ever asked, never what it holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shadow {
    /// The file that shadows: `~/.gitconfig`, `~/.tmux.conf`.
    pub by: PathBuf,
    /// The rendered file it shadows.
    pub of: PathBuf,
}

/// What the loader keeps of one module's run.
#[derive(Debug, Clone, PartialEq)]
pub struct Output {
    pub files: Vec<Rendered>,
    pub shadowed_by: Vec<Shadow>,
    /// [`Program::hook`].
    pub hook: Option<PathBuf>,
}

/// One row of the registry: a module's name and its monomorphized reader and renderer.
#[derive(Clone, Copy)]
pub struct Module {
    pub name: &'static str,
    /// [`Program::EXECUTABLE`].
    pub executable: &'static str,
    /// [`Program::STUB`].
    pub stub: &'static str,
    pub run: fn(&mut Fields<'_>, &RenderEnv) -> Option<Output>,
}

impl std::fmt::Debug for Module {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Module").field("name", &self.name).finish()
    }
}

fn run_with<P: Program>(fields: &mut Fields<'_>, env: &RenderEnv) -> Option<Output> {
    let program = P::read(fields)?;
    let mut files = program.render(env);
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Some(Output {
        files,
        shadowed_by: program.shadowed_by(env),
        hook: program.hook(env),
    })
}

/// The registry row of `P`.
pub const fn module<P: Program>() -> Module {
    Module {
        name: P::NAME,
        executable: P::EXECUTABLE,
        stub: P::STUB,
        run: run_with::<P>,
    }
}

/// Every module this build ships, sorted by name. Each module adds its one row here.
pub const REGISTRY: &[Module] = &[
    module::<alacritty::Alacritty>(),
    module::<bash::Bash>(),
    module::<fish::Fish>(),
    module::<git::Git>(),
    module::<helix::Helix>(),
    module::<kitty::Kitty>(),
    module::<nvim::Nvim>(),
    module::<starship::Starship>(),
    module::<tmux::Tmux>(),
    module::<zsh::Zsh>(),
];

/// The modules this build ships, sorted by name: what maps `[programs.<name>]` to a renderer.
pub fn registry() -> &'static [Module] {
    REGISTRY
}

/// The ownership header of §4.2 in `comment`'s syntax. It names the table and nothing else: no
/// date, no version, no user, no host, no path (LD-109, I3).
pub fn header(comment: Comment, module: &str) -> String {
    let c = comment.prefix();
    format!(
        "{c} Managed by lodi from home.toml [programs.{module}]. Edit home.toml, not this file:\n\
         {c} a hand edit here is drift, and `lodi home apply` stops on it.\n"
    )
}

/// Set `value` at the dotted `key` of a TOML body (`editor.cursor-shape.insert`), creating the
/// tables on the way: how the TOML modules build [`Body::Toml`] from their typed keys.
pub fn set(table: &mut Table, key: &str, value: Value) {
    let mut parts: Vec<&str> = key.split('.').collect();
    let last = parts.pop().expect("a key has a name");
    let mut table = table;
    for part in parts {
        let entry = table
            .entry(part.to_string())
            .or_insert_with(|| Value::Table(Table::new()));
        let Value::Table(inner) = entry else {
            unreachable!("a module sets each of its keys once, below tables only")
        };
        table = inner;
    }
    table.insert(last.to_string(), value);
}

/// Refuse a value that a line-oriented format cannot carry on one line: a line break, a NUL or
/// another control character (kitty, tmux). Whether it was accepted.
pub fn one_line(fields: &mut Fields<'_>, key: &str, value: &str) -> bool {
    if value.chars().any(char::is_control) {
        fields.refuse(
            key,
            "E_TYPE",
            "holds a line break, a NUL or another control character, which this file cannot \
             carry",
            "write the value on one line, without control characters",
        );
        return false;
    }
    true
}

/// A module's [`Program::STUB`] with the `# ` of every line removed: the table it stands for.
/// `None` when a line does not start with `# ` or the text is not TOML.
pub fn stub_table(module: &Module) -> Option<Table> {
    let mut text = String::new();
    for line in module.stub.lines() {
        text.push_str(line.strip_prefix("# ")?);
        text.push('\n');
    }
    crate::home::toml::parse(&text).ok()
}

/// Every path `module` renders into, and every path it is shadowed by (§4.6), when its stub is
/// declared: sorted, each once. Nothing is read to find them — the stub names no file (a
/// `snippets` list in it is empty, and the reader handed over refuses every path) — so this is
/// the module table answering, not the machine. Empty when the stub is not a table the module
/// accepts, which the tests hold no shipped module to.
pub fn stub_paths(module: &Module, env: &RenderEnv) -> Vec<PathBuf> {
    let Some(table) = stub_table(module) else {
        return Vec::new();
    };
    let spots = BTreeMap::new();
    let refuse = |path: &str| -> Result<Vec<u8>, Diagnostic> {
        Err(Diagnostic::new(
            "E_CONFIG",
            format!("a stub names `{path}`, and a stub reads no file"),
        ))
    };
    let mut fields =
        Fields::new(&table, &spots, format!("[programs.{}]", module.name), None).reading(&refuse);
    let output = (module.run)(&mut fields, env);
    if !fields.finish().is_empty() {
        return Vec::new();
    }
    let Some(output) = output else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = output.files.into_iter().map(|file| file.path).collect();
    paths.extend(output.shadowed_by.into_iter().map(|shadow| shadow.by));
    paths.sort();
    paths.dedup();
    paths
}

/// Why [`compose`] refused an `extra`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExtraProblem {
    /// A TOML `extra` does not parse: the parser's message and the byte offset inside the
    /// string it points at.
    Syntax { message: String, offset: usize },
    /// A TOML `extra` sets a key a typed option also sets: its dotted path.
    Conflict(String),
}

/// The bytes of one rendered file: the header, the body and — when the file takes it — the
/// table's `extra`. Pure: equal arguments give byte-identical output (I1).
pub fn compose(module: &str, body: &Body, extra: Option<&str>) -> Result<Vec<u8>, ExtraProblem> {
    let mut out = header(body.comment(), module);
    let text = match body {
        Body::Toml(table) => {
            let mut table = table.clone();
            if let Some(extra) = extra {
                let more = crate::home::toml::parse(extra)
                    .map_err(|(message, offset)| ExtraProblem::Syntax { message, offset })?;
                render::merge_toml(&mut table, more).map_err(ExtraProblem::Conflict)?;
            }
            render::toml_document(&table)
        }
        Body::Text { text, .. } => {
            let mut text = text.clone();
            if let Some(extra) = extra {
                text.push_str(extra);
            }
            text
        }
    };
    if !text.is_empty() {
        out.push('\n');
        out.push_str(&text);
    }
    Ok(out.into_bytes())
}

/// Where a key of a program table is written, for a diagnostic.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Spot {
    /// The key itself: where `E_UNKNOWN_ATTR` points.
    pub key: Option<Location>,
    /// Its value: where `E_TYPE` points.
    pub value: Option<Location>,
}

/// How the loader reads a file a table names (`snippets`, §5): the path as written, relative to
/// the manifest's directory, to its bytes — or the refusal (`E_PATH_ESCAPE`, `E_CONFIG`), which
/// [`Fields::files`] places at the key's value. The module never opens a file itself (I2).
pub type Reader<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, Diagnostic>;

/// A module's view of its table: typed reads by dotted key, each problem recorded at its own
/// location, and every key the module never asked for reported as `E_UNKNOWN_ATTR` when the
/// loader calls [`Fields::finish`].
pub struct Fields<'a> {
    table: &'a Table,
    spots: &'a BTreeMap<Vec<String>, Spot>,
    /// The table's own location, for a problem no key carries.
    at: Option<Location>,
    place: String,
    /// Every key the module asked for, present or not: the names an unknown key is compared with.
    asked: BTreeSet<Vec<String>>,
    diagnostics: Vec<Diagnostic>,
    /// What [`Fields::files`] reads through; none until the loader hands one over.
    reader: Option<Reader<'a>>,
}

/// A value's type as a diagnostic names it.
fn kind(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "a string",
        Value::Integer(_) => "an integer",
        Value::Float(_) => "a float",
        Value::Boolean(_) => "a boolean",
        Value::Datetime(_) => "a date-time",
        Value::Array(_) => "an array",
        Value::Table(_) => "a table",
    }
}

fn split(key: &str) -> Vec<String> {
    key.split('.').map(str::to_string).collect()
}

impl<'a> Fields<'a> {
    /// The view of `table` (the module's keys only), `place` being how a message names it
    /// (`[programs.git]`) and `at` where the table is written.
    pub fn new(
        table: &'a Table,
        spots: &'a BTreeMap<Vec<String>, Spot>,
        place: String,
        at: Option<Location>,
    ) -> Fields<'a> {
        Fields {
            table,
            spots,
            at,
            place,
            asked: BTreeSet::new(),
            diagnostics: Vec::new(),
            reader: None,
        }
    }

    /// The same view, with the loader's [`Reader`] for the files a table names.
    pub fn reading(mut self, reader: Reader<'a>) -> Fields<'a> {
        self.reader = Some(reader);
        self
    }

    /// Whether a problem has been recorded.
    pub fn failed(&self) -> bool {
        !self.diagnostics.is_empty()
    }

    fn spot(&self, path: &[String]) -> Spot {
        self.spots.get(path).cloned().unwrap_or_default()
    }

    fn push(&mut self, d: Diagnostic, at: Option<Location>) {
        let d = match at.or_else(|| self.at.clone()) {
            Some(at) => d.at(at),
            None => d,
        };
        self.diagnostics.push(d);
    }

    /// Record a problem with the value of `key`, for a check only the module can make (a value
    /// the target format cannot hold, a name outside a grammar).
    pub fn refuse(&mut self, key: &str, code: &'static str, message: &str, hint: &str) {
        let path = split(key);
        let at = self.spot(&path).value;
        let d = Diagnostic::new(code, format!("`{key}` in {} {message}", self.place)).hint(hint);
        self.push(d, at);
    }

    /// The value at `key` (dotted: `user.name` is the key `name` of the table `user`), marking the
    /// key as one the module reads. A non-table on the way is `E_TYPE`.
    pub fn get(&mut self, key: &str) -> Option<&'a Value> {
        let path = split(key);
        self.asked.insert(path.clone());
        let mut table = self.table;
        for (i, part) in path.iter().enumerate() {
            let value = table.get(part)?;
            if i + 1 == path.len() {
                return Some(value);
            }
            match value {
                Value::Table(inner) => table = inner,
                other => {
                    let prefix = path[..=i].to_vec();
                    let at = self.spot(&prefix).value;
                    let d = Diagnostic::new(
                        "E_TYPE",
                        format!(
                            "`{}` in {} must be a table, found {}",
                            prefix.join("."),
                            self.place,
                            kind(other)
                        ),
                    );
                    self.push(d, at);
                    // Reported once: the unknown-key walk must not report it again.
                    self.asked.insert(prefix);
                    return None;
                }
            }
        }
        None
    }

    fn wrong(&mut self, key: &str, expected: &str, found: &Value) {
        let at = self.spot(&split(key)).value;
        let d = Diagnostic::new(
            "E_TYPE",
            format!(
                "`{key}` in {} must be {expected}, found {}",
                self.place,
                kind(found)
            ),
        );
        self.push(d, at);
    }

    /// A string.
    pub fn string(&mut self, key: &str) -> Option<String> {
        match self.get(key)? {
            Value::String(s) => Some(s.clone()),
            other => {
                self.wrong(key, "a string", other);
                None
            }
        }
    }

    /// A boolean.
    pub fn boolean(&mut self, key: &str) -> Option<bool> {
        match self.get(key)? {
            Value::Boolean(b) => Some(*b),
            other => {
                self.wrong(key, "a boolean", other);
                None
            }
        }
    }

    /// An integer inside `range`, which the hint states when the value is outside it.
    pub fn integer(&mut self, key: &str, range: RangeInclusive<i64>) -> Option<i64> {
        match self.get(key)? {
            Value::Integer(i) if range.contains(i) => Some(*i),
            Value::Integer(i) => {
                let i = *i;
                self.refuse(
                    key,
                    "E_TYPE",
                    &format!("is {i}, outside the range this key allows"),
                    &format!("use an integer from {} to {}", range.start(), range.end()),
                );
                None
            }
            other => {
                self.wrong(key, "an integer", other);
                None
            }
        }
    }

    /// A number: an integer or a float, as a float.
    pub fn number(&mut self, key: &str) -> Option<f64> {
        match self.get(key)? {
            Value::Integer(i) => Some(*i as f64),
            Value::Float(f) if f.is_finite() => Some(*f),
            Value::Float(_) => {
                self.refuse(key, "E_TYPE", "is not a finite number", "write a number");
                None
            }
            other => {
                self.wrong(key, "a number", other);
                None
            }
        }
    }

    /// A number (an integer or a float, as a float) above `min` — or at it, when
    /// `min_inclusive` — and at most `max`; outside that range it is `E_TYPE`, and the hint
    /// states the range.
    pub fn number_in(&mut self, key: &str, min: f64, min_inclusive: bool, max: f64) -> Option<f64> {
        let value = self.number(key)?;
        let above = if min_inclusive {
            value >= min
        } else {
            value > min
        };
        if above && value <= max {
            return Some(value);
        }
        let hint = match (min_inclusive, max.is_finite()) {
            (true, true) => format!("use a number from {min} to {max}"),
            (false, true) => format!("use a number greater than {min} and at most {max}"),
            (true, false) => format!("use a number of {min} or more"),
            (false, false) => format!("use a number greater than {min}"),
        };
        self.refuse(
            key,
            "E_TYPE",
            &format!("is {value}, outside the range this key allows"),
            &hint,
        );
        None
    }

    /// An array of integers, each inside `range`, which the hint states when one is outside it.
    pub fn integers(&mut self, key: &str, range: RangeInclusive<i64>) -> Option<Vec<i64>> {
        let value = self.get(key)?;
        let Value::Array(items) = value else {
            self.wrong(key, "an array of integers", value);
            return None;
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Value::Integer(i) if range.contains(i) => out.push(*i),
                Value::Integer(i) => {
                    let i = *i;
                    self.refuse(
                        key,
                        "E_TYPE",
                        &format!("holds {i}, outside the range this key allows"),
                        &format!("use integers from {} to {}", range.start(), range.end()),
                    );
                    return None;
                }
                other => {
                    self.wrong(key, "an array of integers", other);
                    return None;
                }
            }
        }
        Some(out)
    }

    /// An array of strings, each from a fixed set, or `E_TYPE` naming the set.
    pub fn choices(&mut self, key: &str, allowed: &[&str]) -> Option<Vec<String>> {
        let values = self.strings(key)?;
        for value in &values {
            if !allowed.contains(&value.as_str()) {
                self.refuse(
                    key,
                    "E_TYPE",
                    &format!("holds \"{value}\""),
                    &format!("each value must be one of: {}", allowed.join(", ")),
                );
                return None;
            }
        }
        Some(values)
    }

    /// A string from a fixed set, or `E_TYPE` naming the set.
    pub fn choice(&mut self, key: &str, allowed: &[&str]) -> Option<String> {
        let value = self.string(key)?;
        if allowed.contains(&value.as_str()) {
            return Some(value);
        }
        self.refuse(
            key,
            "E_TYPE",
            &format!("is \"{value}\""),
            &format!("it must be one of: {}", allowed.join(", ")),
        );
        None
    }

    /// An array of strings.
    pub fn strings(&mut self, key: &str) -> Option<Vec<String>> {
        let value = self.get(key)?;
        let Value::Array(items) = value else {
            self.wrong(key, "an array of strings", value);
            return None;
        };
        let mut out = Vec::with_capacity(items.len());
        for item in items {
            match item {
                Value::String(s) => out.push(s.clone()),
                other => {
                    self.wrong(key, "an array of strings", other);
                    return None;
                }
            }
        }
        Some(out)
    }

    /// A table of strings, sorted by key (`env`, `aliases`).
    pub fn string_map(&mut self, key: &str) -> Option<BTreeMap<String, String>> {
        let value = self.get(key)?;
        let Value::Table(table) = value else {
            self.wrong(key, "a table of strings", value);
            return None;
        };
        let mut out = BTreeMap::new();
        for (name, item) in table {
            match item {
                Value::String(s) => {
                    out.insert(name.clone(), s.clone());
                }
                other => {
                    self.wrong(&format!("{key}.{name}"), "a string", other);
                    return None;
                }
            }
        }
        Some(out)
    }

    /// An array of paths below the manifest's directory, each read through the loader's
    /// [`Reader`] at load time, in declared order (`snippets`, §5). A refusal is placed at the
    /// key's value.
    pub fn files(&mut self, key: &str) -> Option<Vec<Vec<u8>>> {
        let paths = self.strings(key)?;
        let Some(reader) = self.reader else {
            self.refuse(
                key,
                "E_CONFIG",
                "names files, and nothing here reads them",
                "declare the table in home.toml",
            );
            return None;
        };
        let at = self.spot(&split(key)).value;
        let mut out = Vec::with_capacity(paths.len());
        let mut ok = true;
        for path in &paths {
            match reader(path) {
                Ok(bytes) => out.push(bytes),
                Err(d) => {
                    self.push(d, at.clone());
                    ok = false;
                }
            }
        }
        ok.then_some(out)
    }

    /// Report every key of the table the module never asked for, and hand back what was
    /// recorded, sorted by position.
    pub fn finish(mut self) -> Vec<Diagnostic> {
        let mut unknown = Vec::new();
        walk(self.table, &mut Vec::new(), &self.asked, &mut unknown);
        let known: Vec<String> = self.asked.iter().map(|k| k.join(".")).collect();
        for path in unknown {
            let name = path.join(".");
            let hint = match nearest(&name, &known) {
                Some(near) => format!("did you mean `{near}`?"),
                None if known.is_empty() => "this module reads no such key".to_string(),
                None => format!("the keys it reads: {}", known.join(", ")),
            };
            let at = self.spot(&path).key;
            let d = Diagnostic::new(
                "E_UNKNOWN_ATTR",
                format!("unknown key `{name}` in {}", self.place),
            )
            .hint(hint);
            self.push(d, at);
        }
        self.diagnostics
    }
}

/// The keys below `table` that no asked key covers: a key is covered when it was asked for, or
/// when a key above it was (a table read whole), and a table is walked into when an asked key is
/// below it.
fn walk(
    table: &Table,
    path: &mut Vec<String>,
    asked: &BTreeSet<Vec<String>>,
    out: &mut Vec<Vec<String>>,
) {
    for (key, value) in table {
        path.push(key.clone());
        if asked.contains(path.as_slice()) {
        } else if let Value::Table(inner) = value
            && asked.iter().any(|a| a.starts_with(path.as_slice()))
        {
            walk(inner, path, asked, out);
        } else {
            out.push(path.clone());
        }
        path.pop();
    }
}

fn nearest(key: &str, known: &[String]) -> Option<String> {
    known
        .iter()
        .map(|k| (crate::manifest::distance(key, k), k))
        .filter(|(d, k)| *d <= 2 && *d < k.len())
        .min()
        .map(|(_, k)| k.clone())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_registry_is_sorted_and_names_each_module_once() {
        let names: Vec<&str> = registry().iter().map(|m| m.name).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            names, sorted,
            "the registry is not sorted or names a module twice"
        );
        for m in registry() {
            assert!(
                !m.name.is_empty()
                    && m.name
                        .bytes()
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
                "{m:?}: a module name is a lowercase table key"
            );
        }
    }

    #[test]
    fn the_header_names_the_table_in_each_comment_syntax_and_nothing_else() {
        assert_eq!(
            header(Comment::Hash, "git"),
            "# Managed by lodi from home.toml [programs.git]. Edit home.toml, not this file:\n\
             # a hand edit here is drift, and `lodi home apply` stops on it.\n"
        );
        let lua = header(Comment::DoubleDash, "nvim");
        assert!(lua.lines().all(|l| l.starts_with("-- ")), "{lua}");
    }

    #[test]
    fn compose_merges_a_toml_extra_and_appends_a_text_one() {
        let mut t = Table::new();
        t.insert("b".into(), Value::Integer(1));
        let bytes = compose("x", &Body::Toml(t.clone()), Some("a = true\n")).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.ends_with("\na = true\nb = 1\n"), "{text}");
        assert_eq!(
            compose("x", &Body::Toml(t.clone()), Some("b = 2")),
            Err(ExtraProblem::Conflict("b".into()))
        );
        assert!(matches!(
            compose("x", &Body::Toml(t), Some("a = ")),
            Err(ExtraProblem::Syntax { .. })
        ));
        let text = Body::Text {
            comment: Comment::DoubleDash,
            text: "x = 1\n".into(),
        };
        let bytes = compose("x", &text, Some("  raw ; bytes\n")).unwrap();
        assert!(
            String::from_utf8(bytes)
                .unwrap()
                .ends_with("\nx = 1\n  raw ; bytes\n")
        );
    }
}
