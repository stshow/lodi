//! `lodi` — the command-line entry point.
//!
//! Only the CLI surface that works is exposed: `--version`, `--help` — and, since LD-360, the
//! part of it about one command, reached by `lodi help` and by `--help` or `-h` anywhere among
//! lodi's own arguments — `lodi init` (M-0.3 T-1),
//! `lodi shell` and `lodi develop` with or without `-- <command>` (M-0.3 T-2), `lodi run <task>`
//! (host-tool environments since S-3, rootless Podman container environments since S-4), both
//! locking and asking for trust by themselves (LD-496), `lodi gc`, the store collector
//! (M-0.3 T-3), `lodi search`, the offline browse command (M-0.4 T-6), and the
//! config's `import`, `switch`, `update`, `pin` and `unpin` (#693 to #708). A 1.x name 2.0
//! removed stops with its replacement (#699, `OLD_NAMES`). Every other argument is refused with
//! exit status 2. Diagnostics use the design `spec/11` format and exit statuses.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use lodi::diag::EXIT_USAGE as USAGE_ERROR;
use lodi::fetch::HttpFetcher;
use lodi::host::{self, Action, Options};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What one invocation asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Version,
    /// The help text, or with a subject (`switch`, `init`) the part of it about that
    /// command or scope (LD-360).
    Help(Vec<String>),
    /// `lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]`: write ./lodi.toml.
    Init(lodi::init::Options),
    /// `lodi shell [--no-nest] [--base DISTRO[:RELEASE]] ITEM…`: an ad-hoc interactive
    /// environment resolved from these arguments alone (M-0.3 T-2, LD-49).
    Shell {
        no_nest: bool,
        request: lodi::shell::Request,
    },
    /// `lodi develop [--no-nest] [--trust] [-- <program> <args…>]`; an empty argv is the
    /// interactive entry of `spec/10-cli` §3 (LD-48).
    Develop {
        flags: Entry,
        argv: Vec<OsString>,
    },
    /// `lodi run [--no-nest] [--trust] <task> <args…>`.
    Run {
        flags: Entry,
        task: String,
        args: Vec<OsString>,
    },
    /// `lodi gc [--dry-run] [--keep-days N] [--images] [-v]` (M-0.3 T-3).
    Gc(lodi::gc::Options),
    /// `lodi search [--registry | --distro] QUERY` (M-0.4 T-6, design calls D12, D13, D14).
    Search {
        query: String,
        scope: lodi::search::Scope,
    },
    /// `lodi update [PATH] [--host NAME] [--root DIR] [-v]`, `lodi pin (PKG | --all) [--to V]
    /// [PATH] [--host NAME] [--root DIR]` and `lodi unpin (PKG | --all) [PATH] [--host NAME]
    /// [--root DIR]` on the config discovery finds (#696).
    Pins(&'static str, Pins),
    /// `lodi import [PATH] [--home] [--yes] [--dry-run] [--root DIR] [-v]` (#693, #707, #708):
    /// this host and your home into the config; `--home`, `--yes` and `--dry-run`.
    Import(Import),
    /// `lodi switch [PATH-OR-URL] [--ask | --dry-run] [--host | --home] [--update]
    /// [--overwrite-drift] [--resolved ID] [--ref R] [--rev C] [--refresh] [-v] [--root DIR]`
    /// (#695): the host part, then your home part.
    Switch(Switch),
    /// A 1.x name 2.0 removed (#699): the old command as typed, and what replaces it.
    Removed(String, &'static str),
    /// No arguments at all: print the scope lines of the help's opening and exit 0 (LD-380).
    Missing,
    Unsupported(String),
}

/// The flags `develop` and `run` take before their command or task, in any order.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Entry {
    no_nest: bool,
    /// `--trust`: allow the project's text for this run only (LD-496).
    trust: bool,
}

/// Read `develop`'s and `run`'s own flags off the front of `rest`: the flags and where the rest
/// begins. A flag given twice ends them, and is the usage error it then is.
fn parse_entry_flags(rest: &[OsString]) -> (Entry, usize) {
    let mut flags = Entry::default();
    let mut at = 0;
    while let Some(arg) = rest.get(at).map(|a| a.to_string_lossy()) {
        match arg.as_ref() {
            "--no-nest" if !flags.no_nest => flags.no_nest = true,
            "--trust" if !flags.trust => flags.trust = true,
            _ => break,
        }
        at += 1;
    }
    (flags, at)
}

/// What `update`, `pin` and `unpin` were given (#696).
#[derive(Debug, Default, PartialEq, Eq)]
struct Pins {
    target: Option<lodi::config::verbs::Target>,
    to: Option<String>,
    path: Option<OsString>,
    host: Option<String>,
    root: Option<std::path::PathBuf>,
    verbose: bool,
}

/// What `import` was given (#693, #707, #708).
#[derive(Debug, Default, PartialEq, Eq)]
struct Import {
    path: Option<OsString>,
    root: Option<std::path::PathBuf>,
    verbose: bool,
    home: bool,
    yes: bool,
    dry_run: bool,
}

/// What `switch` was given (#695): its root is the flags' own.
#[derive(Debug, Default, PartialEq, Eq)]
struct Switch {
    path: Option<OsString>,
    verbose: bool,
    flags: lodi::config::switch::Flags,
}

/// `update`, `pin` and `unpin`'s own arguments, in any order: the first positional of `pin` and
/// `unpin` is the package unless `--all` is given, the next the config's path. A second
/// package, `--all` with a package, neither, or `--to` on `unpin` are usage errors.
fn parse_pins(verb: &'static str, rest: &[OsString]) -> Request {
    let mut pins = Pins::default();
    let mut all = false;
    let mut positionals: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = rest
            .get(i + 1)
            .filter(|value| !value.to_string_lossy().starts_with('-'));
        let text = value.map(|v| v.to_string_lossy().into_owned());
        let ok = match arg.as_str() {
            "--root" if pins.root.is_none() && value.is_some() => {
                pins.root = value.map(std::path::PathBuf::from);
                i += 2;
                true
            }
            "--host" if pins.host.is_none() && value.is_some() => {
                pins.host = text;
                i += 2;
                true
            }
            "--to" if verb == "pin" && pins.to.is_none() && value.is_some() => {
                pins.to = text;
                i += 2;
                true
            }
            "--all" if verb != "update" && !all => {
                all = true;
                i += 1;
                true
            }
            "-v" if verb == "update" && !pins.verbose => {
                pins.verbose = true;
                i += 1;
                true
            }
            positional if !positional.starts_with('-') => {
                positionals.push(rest[i].clone());
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("{verb} {arg}"));
        }
    }
    let mut positionals = positionals.into_iter();
    if verb != "update" {
        pins.target = Some(if all {
            lodi::config::verbs::Target::All
        } else {
            let Some(name) = positionals.next() else {
                return Request::Unsupported(format!("{verb} (no package and no --all)"));
            };
            let name = name.to_string_lossy().into_owned();
            if !lodi::hostscope::manifest::is_package_name(&name) {
                return Request::Unsupported(format!("{verb} {name}"));
            }
            lodi::config::verbs::Target::Package(name)
        });
    }
    // Beside `--all`, a package name that names no folder is a package, not the config's path.
    if all
        && let Some(first) = positionals.as_slice().first()
        && lodi::hostscope::manifest::is_package_name(&first.to_string_lossy())
        && !Path::new(first).is_dir()
    {
        return Request::Unsupported(format!("{verb} --all {}", first.to_string_lossy()));
    }
    pins.path = positionals.next();
    if let Some(extra) = positionals.next() {
        // `--all` with a package is the usage error this is.
        return Request::Unsupported(format!("{verb} {}", extra.to_string_lossy()));
    }
    Request::Pins(verb, pins)
}

/// `lodi import`'s own arguments, in any order: one path, `--home`, `--yes`, `--dry-run`,
/// `--root DIR`, `-v` (#693, #707, #708).
fn parse_import(rest: &[OsString]) -> Request {
    let mut import = Import::default();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = rest
            .get(i + 1)
            .filter(|value| !value.to_string_lossy().starts_with('-'));
        let ok = match arg.as_str() {
            "--root" if import.root.is_none() && value.is_some() => {
                import.root = value.map(std::path::PathBuf::from);
                i += 2;
                true
            }
            "--home" if !import.home => {
                import.home = true;
                i += 1;
                true
            }
            "--yes" if !import.yes => {
                import.yes = true;
                i += 1;
                true
            }
            "--dry-run" if !import.dry_run => {
                import.dry_run = true;
                i += 1;
                true
            }
            "-v" if !import.verbose => {
                import.verbose = true;
                i += 1;
                true
            }
            positional if import.path.is_none() && !positional.starts_with('-') => {
                import.path = Some(rest[i].clone());
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("import {arg}"));
        }
    }
    Request::Import(import)
}

/// `lodi switch`'s own arguments, in any order (#695). `--host` with `--home`, and `--ask` with
/// `--dry-run`, are usage errors; so are 1.x's `--no-update` and `--unsupported-partial-upgrade`.
fn parse_switch(rest: &[OsString]) -> Request {
    let mut switch = Switch::default();
    let flags = &mut switch.flags;
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = rest
            .get(i + 1)
            .filter(|value| !value.to_string_lossy().starts_with('-'));
        let text = value.map(|v| v.to_string_lossy().into_owned());
        let mut takes = 1;
        let ok = match arg.as_str() {
            "--root" if flags.root.is_none() && value.is_some() => {
                flags.root = value.map(std::path::PathBuf::from);
                takes = 2;
                true
            }
            "--resolved" if flags.resolved.is_none() && value.is_some() => {
                flags.resolved = text;
                takes = 2;
                true
            }
            // A ref or a commit, never both.
            "--ref" if flags.git_ref.is_none() && flags.rev.is_none() && value.is_some() => {
                flags.git_ref = text;
                takes = 2;
                true
            }
            // A commit is not refreshed.
            "--rev"
                if flags.rev.is_none()
                    && flags.git_ref.is_none()
                    && !flags.refresh
                    && value.is_some() =>
            {
                flags.rev = text;
                takes = 2;
                true
            }
            "--ask" if !flags.ask && !flags.dry_run => {
                flags.ask = true;
                true
            }
            "--dry-run" if !flags.dry_run && !flags.ask => {
                flags.dry_run = true;
                true
            }
            "--host" if !flags.host_only && !flags.home_only => {
                flags.host_only = true;
                true
            }
            "--home" if !flags.home_only && !flags.host_only => {
                flags.home_only = true;
                true
            }
            "--update" if !flags.update => {
                flags.update = true;
                true
            }
            "--overwrite-drift" if !flags.overwrite_drift => {
                flags.overwrite_drift = true;
                true
            }
            "--refresh" if !flags.refresh && flags.rev.is_none() => {
                flags.refresh = true;
                true
            }
            "-v" if !switch.verbose => {
                switch.verbose = true;
                true
            }
            positional if switch.path.is_none() && !positional.starts_with('-') => {
                switch.path = Some(rest[i].clone());
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("switch {arg}"));
        }
        i += takes;
    }
    Request::Switch(switch)
}

const BOOT: &str =
    "boot changes take effect at once with `lodi switch`; there is nothing to confirm";

/// What replaces `home plan` and `home status`: the one preview, of the whole config or the home.
const HOME_PREVIEW: &str =
    "use `lodi switch --dry-run`, or `lodi switch --home --dry-run` for your home only";

/// The 1.x names 2.0 removed (LD-491, #699), and what each error says replaces it. A two-word
/// name is matched before its first word alone; there are no aliases.
const OLD_NAMES: &[(&str, &str)] = &[
    ("plan", "use `lodi switch --dry-run`"),
    ("apply", "use `lodi switch`"),
    (
        "host arm",
        "use `lodi import`, which asks once whether lodi may manage this host",
    ),
    (
        "host import",
        "use `lodi import`, which copies this machine and your home",
    ),
    ("host plan", "use `lodi switch --dry-run`"),
    ("host apply", "use `lodi switch`"),
    ("host versions", "use `lodi pin PKG`"),
    ("host pin", "use `lodi pin PKG --to V`"),
    ("host unpin", "use `lodi unpin PKG`"),
    (
        "host",
        "use `lodi import`, `lodi switch`, `lodi pin PKG` or `lodi unpin PKG`",
    ),
    ("home init", "use `lodi import --home`"),
    ("home import", "use `lodi import --home`"),
    ("home plan", HOME_PREVIEW),
    ("home apply", "use `lodi switch --home`"),
    ("home status", HOME_PREVIEW),
    (
        "home",
        "use `lodi import --home`, `lodi switch --home` or `lodi switch --home --dry-run`",
    ),
    ("boot confirm", BOOT),
    ("boot", BOOT),
    (
        "lock",
        "`lodi develop` and `lodi run` lock by themselves, and `lodi update` moves a project's \
         pins",
    ),
    (
        "trust",
        "`lodi develop` and `lodi run` ask on first run; without a terminal pass `--trust` or \
         set `LODI_TRUST=1`",
    ),
    ("info", "use `lodi search TOOL`"),
];

/// The old name `args` start with, as typed, and its replacement (#699). Its first one or two
/// words decide, after a leading `help` and with `--help` and `-h` set aside, so that every
/// argument after it, and asking for help on it, gives the same error. Nothing after a `--` is
/// lodi's own.
fn removed(args: &[OsString]) -> Option<Request> {
    let words: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .take_while(|a| a != "--")
        .filter(|a| a != "--help" && a != "-h")
        .collect();
    let words = match words.first().map(String::as_str) {
        Some("help") => &words[1..],
        _ => &words[..],
    };
    let first = words.first()?;
    let pair = words.get(1).map(|second| format!("{first} {second}"));
    [pair, Some(first.clone())]
        .into_iter()
        .flatten()
        .find_map(|name| {
            let (_, replacement) = OLD_NAMES.iter().find(|(old, _)| *old == name)?;
            Some(Request::Removed(name, replacement))
        })
}

fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Request {
    let args: Vec<OsString> = args.into_iter().collect();
    if let Some(request) = removed(&args) {
        return request;
    }
    if let Some(request) = parse_help(&args) {
        return request;
    }
    let mut args = args.into_iter();
    let Some(first) = args.next() else {
        return Request::Missing;
    };
    let first = first.to_string_lossy().into_owned();
    let rest: Vec<OsString> = args.collect();
    let text = |i: usize| rest.get(i).map(|a| a.to_string_lossy().into_owned());
    match first.as_str() {
        "init" => parse_init(&rest),
        "import" => parse_import(&rest),
        "switch" => parse_switch(&rest),
        "update" => parse_pins("update", &rest),
        "pin" => parse_pins("pin", &rest),
        "unpin" => parse_pins("unpin", &rest),
        "gc" => parse_gc(&rest),
        "search" => parse_search(&rest),
        "shell" => parse_shell(&rest),
        "develop" => {
            let (flags, at) = parse_entry_flags(&rest);
            match text(at).as_deref() {
                Some("--") if rest.len() > at + 1 => Request::Develop {
                    flags,
                    argv: rest[at + 1..].to_vec(),
                },
                Some("--") => Request::Unsupported("develop -- (no command)".into()),
                Some(other) => Request::Unsupported(format!("develop {other}")),
                // No `-- COMMAND` at all is the interactive entry, not a usage error (LD-48).
                None => Request::Develop {
                    flags,
                    argv: Vec::new(),
                },
            }
        }
        "run" => {
            let (flags, at) = parse_entry_flags(&rest);
            match text(at) {
                Some(task) if !task.starts_with('-') => Request::Run {
                    flags,
                    task,
                    args: rest[at + 1..].to_vec(),
                },
                Some(other) => Request::Unsupported(format!("run {other}")),
                None => Request::Unsupported("run (no task)".into()),
            }
        }
        _ if !rest.is_empty() => Request::Unsupported(first),
        "--version" => Request::Version,
        _ => Request::Unsupported(first),
    }
}

/// A request for help (LD-360): `--help` or `-h` anywhere among lodi's own arguments, or `help`
/// as the first argument. What lodi hands on to a child is not its own:
/// nothing after a `--`, and nothing from `run`'s task name on, so `lodi develop -- ls -h` and
/// `lodi run build --help` do exactly what they did. The subject is the command or scope the
/// other words name; any further word is ignored. `None` is not a help request — or a `--help`
/// beside a command that does not exist, which stays the usage error that command already was.
fn parse_help(args: &[OsString]) -> Option<Request> {
    let text: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut own: Vec<&str> = text
        .iter()
        .map(String::as_str)
        .take_while(|a| *a != "--")
        .collect();
    if own.first() == Some(&"run") {
        if let Some(task) = own[1..].iter().position(|a| !a.starts_with('-')) {
            own.truncate(task + 1);
        }
    }
    let flag = own.iter().any(|a| *a == "--help" || *a == "-h");
    let named = own.first() == Some(&"help");
    if !flag && !named {
        return None;
    }
    let words: Vec<&str> = own
        .iter()
        .skip(usize::from(named))
        .filter(|a| **a != "--help" && **a != "-h")
        .copied()
        .collect();
    match help_subject(&words) {
        Some(subject) => Some(Request::Help(subject)),
        None if named => Some(Request::Unsupported(format!("help {}", words.join(" ")))),
        None => None,
    }
}

/// `lodi init`'s own arguments. Flags may come in any order; an unknown flag, a flag without
/// its value and any positional argument are usage errors (exit 2). `init --help` never gets
/// here: it is the help request `parse_help` answers first (LD-360).
fn parse_init(rest: &[OsString]) -> Request {
    let mut options = lodi::init::Options::default();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = |i: &mut usize, slot: &mut Option<String>| -> bool {
            match rest.get(*i + 1) {
                Some(v) if slot.is_none() => {
                    *slot = Some(v.to_string_lossy().into_owned());
                    *i += 2;
                    true
                }
                _ => false,
            }
        };
        let ok = match arg.as_str() {
            "--name" => value(&mut i, &mut options.name),
            "--base" => value(&mut i, &mut options.base),
            "--force" if !options.force => {
                options.force = true;
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("init {arg}"));
        }
    }
    Request::Init(options)
}

/// `lodi gc`'s own arguments (design call D9, LD-54): `--dry-run`, `--keep-days N`, `--images`
/// and `-v`, each at most once. Everything else — an unknown flag, a positional argument, a
/// `--keep-days` that is not a non-negative integer, and the `spec/03` flags this build does
/// not implement (`--no-wait`, `--max-size`, `--keep-outputs`) — is a usage error (exit 2): a
/// flag that exists and does nothing would be worse than one that does not exist.
fn parse_gc(rest: &[OsString]) -> Request {
    let mut options = lodi::gc::Options::default();
    let mut keep_days_given = false;
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let ok = match arg.as_str() {
            "--dry-run" if !options.dry_run => {
                options.dry_run = true;
                i += 1;
                true
            }
            "--images" if !options.images => {
                options.images = true;
                i += 1;
                true
            }
            "-v" if !options.verbose => {
                options.verbose = true;
                i += 1;
                true
            }
            "--keep-days" if !keep_days_given => match rest.get(i + 1) {
                Some(v) => match v.to_string_lossy().parse::<u64>() {
                    Ok(days) => {
                        options.keep_days = days;
                        keep_days_given = true;
                        i += 2;
                        true
                    }
                    Err(_) => false,
                },
                None => false,
            },
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("gc {arg}"));
        }
    }
    Request::Gc(options)
}

/// `lodi search`'s own arguments: at most one of `--registry` and `--distro`, and exactly one
/// query. Both scopes together, no query at all, a second query, an unknown flag and `--json`
/// (design call D14) are usage errors at exit 2 — a flag that exists and is ignored would be
/// worse than one that does not exist.
fn parse_search(rest: &[OsString]) -> Request {
    use lodi::search::Scope;
    let mut scope = Scope::Both;
    let mut query: Option<String> = None;
    for arg in rest {
        let arg = arg.to_string_lossy().into_owned();
        let ok = match arg.as_str() {
            "--registry" if scope == Scope::Both => {
                scope = Scope::Registry;
                true
            }
            "--distro" if scope == Scope::Both => {
                scope = Scope::Distro;
                true
            }
            text if !text.starts_with('-') && query.is_none() => {
                query = Some(text.to_string());
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("search {arg}"));
        }
    }
    match query {
        Some(query) => Request::Search { query, scope },
        None => Request::Unsupported("search (no query)".into()),
    }
}

/// `lodi shell`'s own arguments: an optional `--no-nest`, an optional `--base VALUE` and at
/// least one tool or package name. A flag without its value, an unknown flag, a name that looks
/// like one, and `--` (there is no command form in 0.3) are all usage errors (exit 2).
fn parse_shell(rest: &[OsString]) -> Request {
    let mut no_nest = false;
    let mut request = lodi::shell::Request::default();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let ok = match arg.as_str() {
            "--no-nest" if !no_nest => {
                no_nest = true;
                i += 1;
                true
            }
            "--base" => match rest.get(i + 1) {
                Some(v) if request.base.is_none() && !v.to_string_lossy().starts_with('-') => {
                    request.base = Some(v.to_string_lossy().into_owned());
                    i += 2;
                    true
                }
                _ => false,
            },
            name if !name.starts_with('-') => {
                request.items.push(name.to_string());
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("shell {arg}"));
        }
    }
    if request.items.is_empty() {
        return Request::Unsupported(match &request.base {
            Some(_) => "shell --base (no package)".into(),
            None => "shell (no tool)".into(),
        });
    }
    Request::Shell { no_nest, request }
}

fn version_text() -> String {
    format!("{NAME} {VERSION}")
}

/// The top of `lodi --help`: the purpose, the opening a bare `lodi` prints (where to start:
/// `import`, `switch`, `init`), examples, the command groups and where the reference is.
fn help_text() -> String {
    let mut groups = String::new();
    for (label, words) in HELP_GROUPS {
        groups.push_str(&format!("  {label:<13}  {NAME} {}\n", words.join(", ")));
    }
    format!(
        "{NAME} {VERSION} — environment and system manager for Ubuntu, Debian, Arch and Fedora

Start here:
  {NAME} import   copy this host and your home into a config, ~/.config/lodi
  {NAME} switch   make this host and your home match the config
  {NAME} init     start a project: write ./lodi.toml in this folder

Examples:
  {NAME} import --home                copy only your home, no root needed
  {NAME} switch --dry-run             show what a switch would change
  {NAME} pin tree                     list the versions of tree you can pin
  {NAME} init --base debian:bookworm  start a project in a Debian container
  {NAME} develop -- make              run make in the project's environment
  {NAME} run build                    run the project's build task

Commands ('{NAME} help COMMAND' shows one with its examples):
{groups}  help           {NAME} help [COMMAND], --help, --version

Anything else is refused with exit status {USAGE_ERROR}.
Reference: https://github.com/stshow/lodi/blob/main/docs/CLI.md"
    )
}

/// The command groups of `lodi --help`: a label and the commands in the order they are listed.
/// Each is a key of [`COMMANDS`], and a test holds the two to the same set.
const HELP_GROUPS: [(&str, &[&str]); 2] = [
    (
        "host and home",
        &["import", "switch", "update", "pin", "unpin"],
    ),
    (
        "projects",
        &["init", "develop", "run", "shell", "search", "gc"],
    ),
];

/// One command's help (LD-463): what it does in one line, how it is typed, one to three
/// examples with what each does, every option, and the details. `lodi --help` lists the keys,
/// and `lodi help COMMAND` prints one entry, so neither can name a command the other does not.
struct CommandHelp {
    key: &'static str,
    summary: &'static str,
    usage: &'static [&'static str],
    examples: Examples,
    options: Examples,
    details: &'static str,
}

/// Examples as the user types them, each with what it does; options the same way.
type Examples = &'static [(&'static str, &'static str)];

/// The options `lodi switch`, `import`, `update`, `pin` and `unpin` share.
const ROOT: (&str, &str) = ("--root DIR", "act on the machine under DIR instead of /");
const HOST: (&str, &str) = ("--host NAME", "the host, when the config has more than one");

const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        key: "help",
        summary: "Show help for lodi or for one command.",
        usage: &["lodi help [COMMAND]"],
        examples: &[
            ("lodi help init", "the help of lodi init"),
            ("lodi help switch", "the help of lodi switch"),
        ],
        options: &[],
        details: "COMMAND is a command (init, switch). COMMAND --help and COMMAND -h print the\n\
                  same help. Help reads nothing and runs nothing.",
    },
    CommandHelp {
        key: "import",
        summary: "Copy this host and your home into your config; it installs nothing.",
        usage: &["lodi import [PATH] [--home] [--yes] [--dry-run] [--root DIR] [-v]"],
        examples: &[
            ("lodi import", "this host and your home into ~/.config/lodi"),
            ("lodi import --home", "only your home.toml, no root needed"),
            (
                "lodi import --dry-run",
                "show what a new import would write",
            ),
        ],
        options: &[
            ("--home", "only your home, and nothing read as root"),
            (
                "--yes",
                "answer yes when asked whether lodi may manage this host",
            ),
            ("--dry-run", "show what it would write, and write nothing"),
            ROOT,
            ("-v", "show the output of every step"),
        ],
        details: "It writes host.toml, home.toml, lodi.lock and files/host/ into the config\n\
                  PATH, LODI_REPO, the remembered path or ~/.config/lodi names, as you even\n\
                  under sudo. A folder it creates gets git init; nothing is committed. A config\n\
                  with another host gains this one as <hostname>/host.toml in config.toml; a\n\
                  config with this host has the machine's changes merged into its host.toml.\n\
                  On the machine it writes only lodi's mark under /etc/lodi and its record of\n\
                  what it read, and installs nothing.",
    },
    CommandHelp {
        key: "switch",
        summary: "Make this host and your home match your config, the host first.",
        usage: &[
            "lodi switch [PATH | URL] [--dry-run | --ask] [--host | --home] [--update]",
            "            [--overwrite-drift] [--resolved ID] [--root DIR] [-v]",
            "            [--ref NAME | --rev COMMIT] [--refresh]",
        ],
        examples: &[
            (
                "lodi switch --dry-run",
                "show what both parts would change, and change nothing",
            ),
            (
                "lodi switch",
                "the host part, asking for root, then your home part",
            ),
            ("lodi switch --home", "only your home, no root needed"),
        ],
        options: &[
            ("--dry-run", "show what would change, and change nothing"),
            ("--ask", "show what would change, then wait for a yes"),
            ("--host", "only the host part"),
            ("--home", "only your home part, no root needed"),
            (
                "--update",
                "look the pins up again first, as lodi update does",
            ),
            (
                "--overwrite-drift",
                "replace a home file that was edited by hand",
            ),
            (
                "--resolved ID",
                "go on after you set an interrupted switch right",
            ),
            ROOT,
            ("-v", "show the package manager's output"),
            ("--ref NAME", "with a URL: the branch or tag to fetch"),
            ("--rev COMMIT", "with a URL: the commit to fetch"),
            ("--refresh", "with a URL: fetch the branch again"),
        ],
        details: "It locks what lodi.lock is missing, shows what changes on standard error, then\n\
                  changes the host part and, if that worked, your home part. It names the\n\
                  config's commit and warns when the config is not in git or has uncommitted\n\
                  changes. The host part needs `lodi import` to have allowed lodi to manage it.",
    },
    CommandHelp {
        key: "update",
        summary: "Move your config's snapshots to the last published day and lock them.",
        usage: &["lodi update [PATH] [--host NAME] [--root DIR] [-v]"],
        examples: &[
            (
                "lodi update",
                "move the snapshot and write ~/.config/lodi/lodi.lock",
            ),
            (
                "lodi update --host work",
                "only the host named work, and every home",
            ),
        ],
        options: &[HOST, ROOT, ("-v", "show the output of every step")],
        details: "It locks the snapshot, the pins, the signed_by_url keys and the homes' tools\n\
                  in the config's lodi.lock. A host without a snapshot stays without one and a\n\
                  pin you wrote never moves. It writes all of it or nothing, then says\n\
                  `next: lodi switch`. In a folder holding ./lodi.toml, it updates that\n\
                  project's lodi.lock instead.",
    },
    CommandHelp {
        key: "pin",
        summary: "Pin a package (or, with --all, the host's snapshot) and lock it.",
        usage: &[
            "lodi pin PKG [--to V] [PATH] [--host NAME] [--root DIR]",
            "lodi pin --all [--to DATE] [PATH] [--host NAME] [--root DIR]",
        ],
        examples: &[
            (
                "lodi pin tree",
                "list tree's versions, with the command to pin each",
            ),
            (
                "lodi pin tree --to 2026-09-01",
                "pin tree to that day's archive",
            ),
            (
                "lodi pin --all",
                "set the snapshot to the last published day",
            ),
        ],
        options: &[
            ("--to V", "the version or day to pin to; with --all, a day"),
            ("--all", "pin the whole host to one day"),
            HOST,
            ROOT,
        ],
        details: "It writes the host manifest and lodi.lock together, then says\n\
                  `next: lodi switch`.",
    },
    CommandHelp {
        key: "unpin",
        summary: "Remove a package's pin (or, with --all, every pin and the snapshot).",
        usage: &[
            "lodi unpin PKG [PATH] [--host NAME] [--root DIR]",
            "lodi unpin --all [PATH] [--host NAME] [--root DIR]",
        ],
        examples: &[
            ("lodi unpin tree", "remove tree's pin"),
            ("lodi unpin --all", "remove every pin and the snapshot"),
        ],
        options: &[("--all", "remove every pin and the snapshot"), HOST, ROOT],
        details: "It writes the host manifest and lodi.lock together, then says\n\
                  `next: lodi switch`. Nothing to remove is not an error.",
    },
    CommandHelp {
        key: "init",
        summary: "Start a project: write a commented ./lodi.toml in this directory.",
        usage: &["lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]"],
        examples: &[
            ("lodi init", "a project named after this directory"),
            (
                "lodi init --base debian:bookworm",
                "a project in a Debian container",
            ),
            (
                "lodi init --name hello --force",
                "replace ./lodi.toml, naming it hello",
            ),
        ],
        options: &[
            (
                "--name NAME",
                "the project's name, instead of this folder's",
            ),
            (
                "--base DISTRO[:RELEASE]",
                "a project in a container of that distribution",
            ),
            ("--force", "replace an existing ./lodi.toml"),
        ],
        details: "It also adds .lodi/ to ./.gitignore. --base writes a [container] manifest\n\
                  instead of host tools, from one of the bases\n\
                  arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble. --force replaces\n\
                  an existing ./lodi.toml. It works offline: nothing is downloaded, installed\n\
                  or run.",
    },
    CommandHelp {
        key: "shell",
        summary: "Open a shell with some tools, without a project.",
        usage: &[
            "lodi shell [--no-nest] TOOL[@CONSTRAINT]...",
            "lodi shell [--no-nest] --base DISTRO[:RELEASE] PACKAGE...",
        ],
        examples: &[
            (
                "lodi shell python@3.12 jq",
                "python 3.12 and the latest jq on PATH",
            ),
            (
                "lodi shell --base debian:bookworm curl",
                "curl in a Debian container",
            ),
        ],
        options: &[
            ("--no-nest", "stop when already inside a lodi environment"),
            (
                "--base DISTRO[:RELEASE]",
                "distribution packages in a Podman container",
            ),
        ],
        details: "The tools come from these arguments alone: ./lodi.toml is never read and\n\
                  no file is written in the current directory. --base does the same in a\n\
                  rootless Podman container with those packages, on one of the bases\n\
                  arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble. lodi never\n\
                  installs Podman. The shell is $SHELL when it is set and executable, else\n\
                  /bin/sh. --no-nest refuses to start inside another lodi environment.",
    },
    CommandHelp {
        key: "develop",
        summary: "Run a command, or a shell, in the project's environment.",
        usage: &["lodi develop [--no-nest] [--trust] [-- COMMAND [ARGS...]]"],
        examples: &[
            ("lodi develop", "an interactive shell in the environment"),
            (
                "lodi develop -- make",
                "run make there and exit with its status",
            ),
        ],
        options: &[
            ("--no-nest", "stop when already inside a lodi environment"),
            ("--trust", "allow the project's tasks for this run only"),
        ],
        details: "The environment is ./lodi.toml and ./lodi.lock: upstream tools on this\n\
                  machine, or a rootless Podman container for a [container] manifest. It\n\
                  writes a missing lock and adds a new tool to it, and never moves a pinned\n\
                  version. It shows the project's tasks and asks before the first run and\n\
                  after they change; --trust or LODI_TRUST=1 allows them for this run only.\n\
                  An arch base comes with no keyring and no signature check: each package\n\
                  is a verified file that HTTPS and the lock's hashes vouch for,\n\
                  as it is for debian and ubuntu. COMMAND gets its arguments unchanged.\n\
                  The shell is $SHELL when it is set and executable, else\n\
                  /bin/sh. lodi never installs Podman and changes no setting of the machine.",
    },
    CommandHelp {
        key: "run",
        summary: "Run one of the project's tasks.",
        usage: &["lodi run [--no-nest] [--trust] TASK [ARGS...]"],
        examples: &[
            (
                "lodi run versions",
                "run the [tasks.versions] entry of ./lodi.toml",
            ),
            ("lodi run build", "run [tasks.build] the same way"),
        ],
        options: &[
            ("--no-nest", "stop when already inside a lodi environment"),
            ("--trust", "allow the project's tasks for this run only"),
        ],
        details: "The task runs with /bin/sh -c in the environment lodi develop uses, locked\n\
                  and allowed the same way. ARGS go to the task unchanged, --help included.",
    },
    CommandHelp {
        key: "gc",
        summary: "Free disk space: remove what no project or shell still uses.",
        usage: &["lodi gc [--dry-run] [--keep-days N] [--images] [-v]"],
        examples: &[
            (
                "lodi gc --dry-run",
                "show what would be removed, and remove nothing",
            ),
            ("lodi gc", "remove it"),
            (
                "lodi gc --images",
                "also remove the Podman images lodi built and no one uses",
            ),
        ],
        options: &[
            (
                "--dry-run",
                "show what would be removed, and remove nothing",
            ),
            (
                "--keep-days N",
                "keep downloads newer than N days, 7 by default",
            ),
            ("--images", "also remove unused Podman images lodi built"),
            ("-v", "name everything it removes"),
        ],
        details: "It removes the store entries no live session uses, the roots of sessions\n\
                  that have ended, and cached downloads no entry names that are older than\n\
                  --keep-days (default 7). --dry-run shows exactly that and changes nothing,\n\
                  and -v names every object. Nothing outside $LODI_HOME is removed unless\n\
                  --images is given, and then only unused Podman images tagged exactly\n\
                  localhost/lodi-env:<32 hex>.",
    },
    CommandHelp {
        key: "search",
        summary: "Find a tool in the catalogue, or a package in the distro indexes.",
        usage: &["lodi search [--registry | --distro] QUERY"],
        examples: &[
            (
                "lodi search grep",
                "the tools and packages that mention grep",
            ),
            (
                "lodi search --registry python",
                "the catalogue's tools only",
            ),
        ],
        options: &[
            ("--registry", "search the catalogue only"),
            ("--distro", "search the distribution's packages only"),
        ],
        details: "It lists the catalogue tools whose name or description contains QUERY,\n\
                  then the packages of the distro indexes ./lodi.lock names, read from the\n\
                  download cache. It makes no network request and writes nothing. A cold\n\
                  cache is a note on standard error and exit 0. --registry searches the\n\
                  catalogue only, and --distro the indexes only.",
    },
];

/// The help of one command.
fn command_help(command: &CommandHelp) -> String {
    let details: Vec<&str> = command.details.lines().collect();
    let options = match command.options {
        [] => String::new(),
        options => format!("\n\nOptions:\n{}", example_lines(options)),
    };
    format!(
        "{}\n\nUsage:\n{}\n\nExamples:\n{}{options}\n\nDetails:\n{}",
        command.summary,
        indent(command.usage),
        example_lines(command.examples),
        indent(&details)
    )
}

fn indent(lines: &[&str]) -> String {
    lines
        .iter()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Examples as the user types them, each with what it does beside it in one column.
fn example_lines(examples: &[(&str, &str)]) -> String {
    let width = examples.iter().map(|(c, _)| c.len()).max().unwrap_or(0);
    examples
        .iter()
        .map(|(command, what)| format!("  {command:<width$}  {what}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The command `words` names: nothing for the whole text (no words, or a first word that is a
/// flag), else the command its first word is. Everything after it is ignored — it is an
/// argument of the command asked about. `None` when the first word is no command.
fn help_subject(words: &[&str]) -> Option<Vec<String>> {
    let Some(first) = words.first().filter(|w| !w.starts_with('-')) else {
        return Some(Vec::new());
    };
    COMMANDS
        .iter()
        .any(|c| c.key == *first)
        .then(|| vec![first.to_string()])
}

/// What a bare `lodi` prints: the help's opening, from line 3 to the next blank line, cut from
/// `help_text()` so that the two can never say different things (LD-380).
fn orientation() -> String {
    help_text()
        .lines()
        .skip(2)
        .take_while(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a help request prints: the top of the help for no subject, and one command's help
/// otherwise.
fn help_for(subject: &[String]) -> String {
    let key = subject.join(" ");
    if key.is_empty() {
        return help_text();
    }
    let command = COMMANDS
        .iter()
        .find(|c| c.key == key)
        .expect("help_subject names a command of the table");
    command_help(command)
}

/// The fetcher of every command that resolves: configured from the environment, and keeping the
/// GitHub API's answers in the store's `cache/api/` for conditional requests (LD-404).
fn fetcher_from_env() -> Result<HttpFetcher, String> {
    HttpFetcher::from_env().map(|f| f.with_api_cache(lodi::fetch::ApiCache::from_env()))
}

fn report(result: Result<u8, lodi::lock::Failure>) -> ExitCode {
    match result {
        Ok(status) => ExitCode::from(status),
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::from(failure.exit_status)
        }
    }
}

fn run_entry(action: Action, flags: Entry) -> ExitCode {
    let fetcher = match fetcher_from_env() {
        Ok(f) => f,
        Err(e) => {
            let d = lodi::diag::Diagnostic::new("E_CONFIG", e);
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let options = Options {
        no_nest: flags.no_nest,
        trust_once: flags.trust || std::env::var_os("LODI_TRUST").is_some_and(|v| v == "1"),
        interactive: host::interactive(),
    };
    report(host::enter(Path::new("."), &action, &options, &fetcher))
}

fn run_shell(request: &lodi::shell::Request, no_nest: bool) -> ExitCode {
    let fetcher = match fetcher_from_env() {
        Ok(f) => f,
        Err(e) => {
            let d = lodi::diag::Diagnostic::new("E_CONFIG", e);
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let options = Options {
        no_nest,
        // An ad-hoc shell runs no task text, so there is nothing for the trust gate to ask
        // about; the flag is irrelevant here and left at its default.
        trust_once: false,
        interactive: host::interactive(),
    };
    report(lodi::shell::enter(
        Path::new("."),
        request,
        &options,
        &fetcher,
    ))
}

/// `lodi search`: the notes on standard error, the report on standard output.
/// Neither command has a non-zero success status — a cold cache is a note, not a failure
/// (M-0.4 T-6, design calls D12 and D13).
fn report_browse(result: Result<lodi::search::Report, lodi::lock::Failure>) -> ExitCode {
    match result {
        Ok(report) => {
            for note in &report.notes {
                eprintln!("{note}");
            }
            for line in &report.lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::from(failure.exit_status)
        }
    }
}

/// A config verb's refusal, on standard error with its exit status.
fn fail(d: lodi::diag::Diagnostic) -> ExitCode {
    let status = lodi::diag::exit_status(d.code);
    eprintln!("{d}");
    ExitCode::from(status)
}

/// The LD-376 guard for `verb` on `root`.
fn guard(verb: &str, root: Option<&Path>) -> Result<(), lodi::diag::Diagnostic> {
    lodi::hostscope::require_root_for(
        verb,
        root,
        std::env::var_os(lodi::hostscope::REQUIRE_ROOT_VAR).as_deref(),
    )
}

/// What a config verb runs on: the path typed (a URL or absolute as it is, else from the current
/// folder), `LODI_REPO`, and who runs lodi on `root`.
fn invocation(
    path: Option<&OsString>,
    root: Option<&Path>,
    host: Option<String>,
    verbose: bool,
) -> Result<lodi::config::verbs::Invocation, lodi::diag::Diagnostic> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    let typed = path.map(|path| {
        if lodi::hostscope::remote::is_url(path) || Path::new(path).is_absolute() {
            path.clone()
        } else {
            cwd.join(path).into_os_string()
        }
    });
    Ok(lodi::config::verbs::Invocation {
        typed,
        env: std::env::var_os("LODI_REPO"),
        identity: config_identity(root)?,
        host,
        verbose,
    })
}

/// `lodi update`, `pin` and `unpin` (#696): the project in the current folder for a bare
/// `update` beside `./lodi.toml`, else the config discovery finds, for the person behind `sudo`.
fn run_pins(verb: &str, pins: &Pins) -> ExitCode {
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("/"));
    if verb == "update" && pins.path.is_none() && cwd.join(lodi::lock::MANIFEST_FILE).is_file() {
        if pins.host.is_some() {
            eprintln!(
                "{NAME}: usage: `update --host` chooses a config's host, and this folder is a \
                 project; name the config: lodi update PATH --host NAME"
            );
            return ExitCode::from(USAGE_ERROR);
        }
        return match lodi::config::verbs::update_project(&cwd) {
            Ok(line) => {
                eprint!("{line}");
                ExitCode::SUCCESS
            }
            Err(failure) => report(Err(failure)),
        };
    }
    if let Err(d) = guard(verb, pins.root.as_deref()) {
        return fail(d);
    }
    let root = pins.root.as_deref();
    let invocation = match invocation(pins.path.as_ref(), root, pins.host.clone(), pins.verbose) {
        Ok(invocation) => invocation,
        Err(d) => return fail(d),
    };
    if verb == "update"
        && let Some(typed) = &invocation.typed
        && Path::new(typed).join(lodi::lock::MANIFEST_FILE).is_file()
    {
        return fail(
            lodi::diag::Diagnostic::new(
                "E_CONFIG",
                format!("{} is a project, not a config", Path::new(typed).display()),
            )
            .hint("run `lodi update` inside it to update its lodi.lock"),
        );
    }
    let target = pins
        .target
        .clone()
        .unwrap_or(lodi::config::verbs::Target::All);
    let done = match verb {
        "update" => lodi::config::verbs::update(&invocation),
        "pin" => lodi::config::verbs::pin(&invocation, &target, pins.to.as_deref()),
        _ => lodi::config::verbs::unpin(&invocation, &target),
    };
    match done {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(d) if d.code == lodi::config::USAGE => {
            eprintln!("{NAME}: usage: {}", d.message);
            ExitCode::from(USAGE_ERROR)
        }
        Err(d) => fail(d),
    }
}

/// Who runs lodi, for the config module (#692), on the `--root` given or `/`.
fn config_identity(root: Option<&Path>) -> Result<lodi::config::Identity, lodi::diag::Diagnostic> {
    let root = root.map_or_else(|| std::path::PathBuf::from("/"), Path::to_path_buf);
    if let Some(missing) = lodi::hostscope::safety::missing_root(&root) {
        return Err(missing);
    }
    let system_root = root == Path::new("/");
    let hostname = lodi::hostscope::source::hostname(&root, system_root)?;
    Ok(lodi::config::Identity {
        euid: lodi::hostscope::safety::current_euid(),
        sudo_uid: std::env::var_os("SUDO_UID"),
        doas_user: std::env::var_os("DOAS_USER"),
        home: lodi::roots::capture().home(),
        state_home: std::env::var_os("XDG_STATE_HOME").map(std::path::PathBuf::from),
        config_home: lodi::roots::capture().xdg_config(),
        data_home: lodi::roots::capture().named_data(),
        root,
        hostname,
    })
}

/// `lodi import` (#693): the LD-376 guard, then the config folder discovery chooses.
fn run_import(import: &Import) -> ExitCode {
    let root = import.root.as_deref();
    let done = guard("import", root)
        .and_then(|()| invocation(import.path.as_ref(), root, None, import.verbose))
        .and_then(|inv| lodi::config::import::run(&inv, import.home, import.yes, import.dry_run));
    match done {
        Ok(report) => {
            for line in report.lines() {
                if line.starts_with("W_") {
                    eprintln!("{line}");
                } else {
                    println!("{line}");
                }
            }
            ExitCode::SUCCESS
        }
        Err(d) => fail(d),
    }
}

/// `lodi switch` (#695): the LD-376 guard, then the config discovery finds, for the person
/// behind `sudo` or `doas`.
fn run_switch(switch: &Switch) -> ExitCode {
    let flags = &switch.flags;
    let root = flags.root.as_deref();
    if let Err(d) = guard("switch", root) {
        return fail(d);
    }
    // An ostree root is said for what it is before its hostname is read (LD-528).
    if !flags.home_only {
        let root = root.unwrap_or(Path::new("/"));
        if lodi::hostscope::safety::missing_root(root).is_none()
            && let Err(d) = lodi::hostscope::safety::check_ostree(root)
        {
            return fail(d);
        }
    }
    let invocation = match invocation(switch.path.as_ref(), root, None, switch.verbose) {
        Ok(invocation) => invocation,
        Err(d) => return fail(d),
    };
    match lodi::config::switch::run(&invocation, flags) {
        Ok(()) => ExitCode::SUCCESS,
        Err(lodi::config::switch::Stopped::Usage(text)) => {
            eprintln!("{NAME}: usage: {text}");
            ExitCode::from(USAGE_ERROR)
        }
        Err(lodi::config::switch::Stopped::Failed(error)) => {
            if !error.done.is_empty() {
                eprint!("{}", error.done);
            }
            eprintln!("{error}");
            ExitCode::from(error.exit_status())
        }
    }
}

/// The command a first guess meant (LD-382). `lodi setup`, `lodi init host` and `lodi init
/// home` stay usage errors — `lodi init` takes no scope word — but the refusal names the command
/// that does what was asked (#699). `arg` is what the parser
/// refused, so `lodi init --force home` is `init home` too.
fn pointer(arg: &str) -> Option<String> {
    let text = match arg {
        "setup" => format!(
            "start with what you want: this machine and your home, `{NAME} import`; only your \
             home, `{NAME} import --home`; a project here, `{NAME} init`"
        ),
        "init host" => format!(
            "`{NAME} init` starts a project and takes no scope word: for this machine run \
             `{NAME} import`"
        ),
        "init home" => format!(
            "`{NAME} init` starts a project and takes no scope word: for your home run \
             `{NAME} import --home`"
        ),
        _ => return None,
    };
    Some(text)
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // The elevator's hidden entry (LD-503): a parent lodi's child, never a command.
    if args
        .first()
        .is_some_and(|first| first == lodi::elevate::ENTRY)
    {
        return lodi::elevate::entry(&args[1..]);
    }
    // The one deprecation point of the dispatch (M-1.0 T-8). Every surface this invocation typed
    // is looked up in the committed table — empty at 1.0, so this prints nothing — and an entry
    // prints its `W_DEPRECATED` line on standard error. A warning never changes the exit status,
    // so nothing below this loop knows it ran.
    for surface in lodi::surface::typed(&args) {
        if let Some(warning) = lodi::surface::deprecation(lodi::surface::DEPRECATIONS, &surface) {
            eprintln!("{warning}");
        }
    }
    // `shell`, `develop`, `run` and `search` never look for a config (#698 story 39): they
    // resolve through the built-in catalogue alone.
    let request = parse(args);
    match request {
        Request::Shell { no_nest, request } => run_shell(&request, no_nest),
        Request::Develop { flags, argv } => run_entry(Action::Develop(argv), flags),
        Request::Run { flags, task, args } => run_entry(Action::Run { task, args }, flags),
        Request::Gc(options) => match lodi::gc::run(&options) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(d) => {
                eprintln!("{d}");
                ExitCode::from(lodi::diag::exit_status(d.code))
            }
        },
        Request::Search { query, scope } => {
            report_browse(lodi::search::search(Path::new("."), &query, scope))
        }
        Request::Pins(verb, pins) => run_pins(verb, &pins),
        Request::Import(import) => run_import(&import),
        Request::Switch(switch) => run_switch(&switch),
        Request::Init(options) => match lodi::init::init(Path::new("."), &options) {
            Ok(lines) => {
                for line in lines {
                    println!("{line}");
                }
                ExitCode::SUCCESS
            }
            Err(failure) => {
                eprintln!("{failure}");
                ExitCode::from(failure.exit_status)
            }
        },
        Request::Version => {
            println!("{}", version_text());
            ExitCode::SUCCESS
        }
        Request::Help(subject) => {
            println!("{}", help_for(&subject));
            ExitCode::SUCCESS
        }
        Request::Missing => {
            println!("{}", orientation());
            ExitCode::SUCCESS
        }
        Request::Removed(old, replacement) => {
            eprintln!("{NAME}: usage: `{NAME} {old}` is gone in {NAME} 2.0; {replacement}");
            ExitCode::from(USAGE_ERROR)
        }
        Request::Unsupported(arg) => {
            eprintln!("{NAME}: unsupported command or argument '{arg}'");
            if let Some(pointer) = pointer(&arg) {
                eprintln!("   = hint: {pointer}");
            }
            eprintln!(
                "This build implements only --version, --help, help, init, \
                 shell ITEM..., develop [-- COMMAND], run TASK, search QUERY, \
                 import, switch, update, pin, unpin and gc."
            );
            ExitCode::from(USAGE_ERROR)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_strs(args: &[&str]) -> Request {
        parse(args.iter().map(OsString::from))
    }

    #[test]
    fn version_flag() {
        assert_eq!(parse_strs(&["--version"]), Request::Version);
    }

    fn help_of(words: &[&str]) -> Request {
        Request::Help(words.iter().map(|w| w.to_string()).collect())
    }

    #[test]
    fn help_flag() {
        assert_eq!(parse_strs(&["--help"]), help_of(&[]));
    }

    /// LD-360: `-h`, `help` and `--help` anywhere among lodi's own arguments are help requests,
    /// with the subject the other words name and every further word ignored.
    #[test]
    fn help_is_asked_for_at_every_level() {
        for (args, subject) in [
            (&["-h"][..], &[][..]),
            (&["help"], &[]),
            (&["help", "--help"], &[]),
            (&["--help", "--frobnicate"], &[]),
            (&["--version", "-h"], &[]),
            (&["help", "switch"], &["switch"]),
            (&["-h", "switch", "--home"], &["switch"]),
            (&["switch", "--root", "scratch", "-h"], &["switch"]),
            (&["init", "--help"], &["init"]),
            (&["init", "--name", "x", "-h"], &["init"]),
            (&["help", "init", "extra"], &["init"]),
            (&["shell", "python", "-h"], &["shell"]),
            (&["develop", "--help", "--", "true"], &["develop"]),
            (&["run", "--no-nest", "--help"], &["run"]),
            (&["gc", "--help"], &["gc"]),
            (&["search", "ython", "--help"], &["search"]),
            (&["help", "help"], &["help"]),
        ] {
            assert_eq!(parse_strs(args), help_of(subject), "{args:?}");
        }
    }

    /// What lodi hands on to a child is not its own, so these are exactly what they were: a
    /// task's arguments and a command after `--`.
    #[test]
    fn a_childs_arguments_are_never_a_help_request() {
        assert_eq!(
            parse_strs(&["run", "build", "--help"]),
            Request::Run {
                flags: Entry::default(),
                task: "build".into(),
                args: vec!["--help".into()],
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--", "ls", "-h"]),
            Request::Develop {
                flags: Entry::default(),
                argv: vec!["ls".into(), "-h".into()],
            }
        );
        assert_eq!(
            parse_strs(&["shell", "python", "--", "--help"]),
            Request::Unsupported("shell --".into())
        );
    }

    /// A help request about nothing that exists is refused: `lodi help` names what it could not
    /// find, and a `--help` beside an unknown command is that command's usage error unchanged.
    #[test]
    fn help_about_nothing_is_refused() {
        for (args, refused) in [
            (&["help", "frobnicate"][..], "help frobnicate"),
            (&["help", "frobnicate", "init"], "help frobnicate init"),
            (&["frobnicate", "--help"], "frobnicate"),
            (&["-V"], "-V"),
        ] {
            assert_eq!(
                parse_strs(args),
                Request::Unsupported(refused.to_string()),
                "{args:?}"
            );
        }
    }

    /// One table answers every help request (LD-463): the top of the help lists each command of
    /// it exactly once in its groups, each command has its own help, and every
    /// subject the table names is one a help request resolves to.
    #[test]
    fn the_help_lists_every_command_of_the_table_once() {
        let mut listed: Vec<String> = HELP_GROUPS
            .iter()
            .flat_map(|(_, words)| words.iter().map(|w| w.to_string()))
            .collect();
        listed.push("help".into());
        listed.sort();
        let mut keys: Vec<String> = COMMANDS.iter().map(|c| c.key.to_string()).collect();
        keys.sort();
        assert_eq!(listed, keys);
        assert_eq!(help_for(&[]), help_text());
        for command in COMMANDS {
            let subject: Vec<String> = command.key.split(' ').map(str::to_string).collect();
            let words: Vec<&str> = command.key.split(' ').collect();
            assert_eq!(
                help_subject(&words),
                Some(subject.clone()),
                "{}",
                command.key
            );
            let help = help_for(&subject);
            assert!(help.starts_with(command.summary), "{}", command.key);
            let usage = format!("\n  {NAME} {}", command.key);
            assert!(help.contains(&usage), "{}", command.key);
        }
        assert_eq!(
            help_subject(&["switch", "status", "x"]),
            Some(vec!["switch".into()])
        );
        assert_eq!(help_subject(&["home"]), None);
        assert_eq!(help_subject(&["host"]), None);
        assert_eq!(help_subject(&["frobnicate"]), None);
    }

    /// U13 for 2.0 (#700): the whole text opens with where to start, `lodi import` first, a bare
    /// `lodi` prints exactly that opening, and the examples and the command groups follow.
    #[test]
    fn help_opens_with_where_to_start_and_groups_the_commands() {
        let whole = help_text();
        let lines: Vec<&str> = whole.lines().collect();
        assert!(lines[3].trim_start().starts_with("lodi import "), "{whole}");
        let opening: Vec<&str> = lines[2..]
            .iter()
            .take_while(|l| !l.is_empty())
            .copied()
            .collect();
        assert_eq!(orientation(), opening.join("\n"));
        let headers: Vec<&str> = lines[2 + opening.len()..]
            .iter()
            .filter(|l| !l.starts_with(' ') && l.ends_with(':'))
            .copied()
            .collect();
        assert_eq!(
            headers,
            [
                "Examples:",
                "Commands ('lodi help COMMAND' shows one with its examples):",
            ]
        );
    }

    #[test]
    fn nothing_is_missing() {
        assert_eq!(parse_strs(&[]), Request::Missing);
    }

    fn shell_request(base: Option<&str>, items: &[&str]) -> lodi::shell::Request {
        lodi::shell::Request {
            base: base.map(str::to_string),
            items: items.iter().map(|i| i.to_string()).collect(),
        }
    }

    #[test]
    fn search_takes_one_query_and_at_most_one_scope() {
        use lodi::search::Scope;
        assert_eq!(
            parse_strs(&["search", "ython"]),
            Request::Search {
                query: "ython".into(),
                scope: Scope::Both
            }
        );
        assert_eq!(
            parse_strs(&["search", "--registry", "ython"]),
            Request::Search {
                query: "ython".into(),
                scope: Scope::Registry
            }
        );
        // The flag may follow the query; the scopes may not be combined.
        assert_eq!(
            parse_strs(&["search", "ython", "--distro"]),
            Request::Search {
                query: "ython".into(),
                scope: Scope::Distro
            }
        );
        for args in [
            &["search"][..],
            &["search", "--registry", "--distro", "x"],
            &["search", "--json", "x"],
            &["search", "a", "b"],
        ] {
            assert!(
                matches!(parse_strs(args), Request::Unsupported(_)),
                "{args:?}"
            );
        }
    }

    #[test]
    fn shell_takes_names_a_base_and_no_command_form() {
        assert_eq!(
            parse_strs(&["shell", "python@3.12", "nodejs"]),
            Request::Shell {
                no_nest: false,
                request: shell_request(None, &["python@3.12", "nodejs"])
            }
        );
        assert_eq!(
            parse_strs(&["shell", "--no-nest", "--base", "debian:12", "make", "gcc"]),
            Request::Shell {
                no_nest: true,
                request: shell_request(Some("debian:12"), &["make", "gcc"])
            }
        );
        // The base may come after the names; the request is the same either way.
        assert_eq!(
            parse_strs(&["shell", "make", "--base", "debian:12"]),
            Request::Shell {
                no_nest: false,
                request: shell_request(Some("debian:12"), &["make"])
            }
        );
        for bad in [
            &["shell"][..],
            &["shell", "--no-nest"],
            &["shell", "--base"],
            &["shell", "--base", "debian:12"],
            &["shell", "--base", "--no-nest", "make"],
            &["shell", "--base", "a", "--base", "b", "make"],
            &["shell", "--no-nest", "--no-nest", "python"],
            &["shell", "--pure", "python"],
            // There is deliberately no `--` form: running a command stays `develop --`.
            &["shell", "python", "--", "true"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    const NO_NEST: Entry = Entry {
        no_nest: true,
        trust: false,
    };

    /// A 1.x name is removed whatever follows it, help included (#699); a child's own words,
    /// a task called `lock` and a command after `--`, are not lodi's.
    #[test]
    fn an_old_name_is_removed_whatever_follows_it() {
        for (args, old) in [
            (&["lock"][..], "lock"),
            (&["lock", "--check"], "lock"),
            (&["trust", "--revoke"], "trust"),
            (&["info", "python"], "info"),
            (&["help", "plan"], "plan"),
            (&["-h", "apply", "x"], "apply"),
            (&["host", "arm", "--root", "/r"], "host arm"),
            (&["help", "host", "plan"], "host plan"),
            (&["host", "--root", "/r", "-h"], "host"),
            (&["host", "status", "--help"], "host"),
            (&["host", "pin", "curl", "--to", "1"], "host pin"),
            (&["home", "init", "--help"], "home init"),
            (&["home", "plan", "./cfg"], "home plan"),
            (&["help", "home", "apply"], "home apply"),
            (&["home", "status", "-h"], "home status"),
            (&["home"], "home"),
            (&["home", "--"], "home"),
            (&["-h", "home", "frobnicate"], "home"),
            (&["boot", "confirm"], "boot confirm"),
            (&["boot", "--root", "/r"], "boot"),
        ] {
            assert!(
                matches!(parse_strs(args), Request::Removed(name, _) if name == old),
                "{args:?}"
            );
        }
        for args in [
            &["run", "lock"][..],
            &["develop", "--", "plan"],
            &["search", "info"],
            &["pin", "host"],
            &["switch", "--home"],
        ] {
            assert!(
                !matches!(parse_strs(args), Request::Removed(..)),
                "{args:?}"
            );
        }
    }

    /// `--trust` and `--no-nest` come before develop's `--` and run's task, in either order,
    /// each at most once (LD-496).
    #[test]
    fn develop_and_run_take_trust_and_no_nest_in_either_order() {
        let both = Entry {
            no_nest: true,
            trust: true,
        };
        let trust = Entry {
            no_nest: false,
            trust: true,
        };
        assert_eq!(
            parse_strs(&["develop", "--trust", "--no-nest", "--", "make"]),
            Request::Develop {
                flags: both,
                argv: vec!["make".into()],
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--trust"]),
            Request::Develop {
                flags: trust,
                argv: Vec::new(),
            }
        );
        assert_eq!(
            parse_strs(&["run", "--no-nest", "--trust", "build", "--trust"]),
            Request::Run {
                flags: both,
                task: "build".into(),
                args: vec!["--trust".into()],
            }
        );
        for bad in [
            &["develop", "--trust", "--trust"][..],
            &["run", "--trust"],
            &["run", "--trust", "--trust", "build"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn develop_is_interactive_without_a_command_and_runs_one_after_a_double_dash() {
        assert_eq!(
            parse_strs(&["develop", "--", "make", "-j", "4"]),
            Request::Develop {
                flags: Entry::default(),
                argv: ["make", "-j", "4"].map(OsString::from).to_vec()
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--no-nest", "--", "--", ""]),
            Request::Develop {
                flags: NO_NEST,
                argv: ["--", ""].map(OsString::from).to_vec()
            }
        );
        // No `-- COMMAND` at all is the interactive entry (LD-48), with or without --no-nest.
        assert_eq!(
            parse_strs(&["develop"]),
            Request::Develop {
                flags: Entry::default(),
                argv: Vec::new()
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--no-nest"]),
            Request::Develop {
                flags: NO_NEST,
                argv: Vec::new()
            }
        );
        for bad in [
            &["develop", "--"][..],
            &["develop", "make"],
            &["develop", "--no-nest", "--"],
            &["develop", "--pure", "--", "x"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn run_takes_a_task_and_passes_the_rest_unchanged() {
        assert_eq!(
            parse_strs(&["run", "build", "--release", "a b"]),
            Request::Run {
                flags: Entry::default(),
                task: "build".into(),
                args: ["--release", "a b"].map(OsString::from).to_vec()
            }
        );
        assert_eq!(
            parse_strs(&["run", "--no-nest", "t"]),
            Request::Run {
                flags: NO_NEST,
                task: "t".into(),
                args: Vec::new()
            }
        );
        for bad in [&["run"][..], &["run", "--list"], &["run", "--no-nest"]] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn gc_takes_its_four_flags_and_refuses_everything_else() {
        assert_eq!(
            parse_strs(&["gc"]),
            Request::Gc(lodi::gc::Options::default())
        );
        assert_eq!(
            parse_strs(&["gc", "--dry-run", "--images", "-v", "--keep-days", "0"]),
            Request::Gc(lodi::gc::Options {
                dry_run: true,
                keep_days: 0,
                images: true,
                verbose: true,
            })
        );
        for bad in [
            &["gc", "--keep-days"][..],
            &["gc", "--keep-days", "-1"],
            &["gc", "--keep-days", "week"],
            &["gc", "--keep-days", "1", "--keep-days", "2"],
            &["gc", "--dry-run", "--dry-run"],
            // spec/03 §7 also has these; this build implements neither (D9, LD-54).
            &["gc", "--no-wait"],
            &["gc", "--max-size", "1G"],
            &["gc", "--keep-outputs"],
            &["gc", "everything"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn planned_commands_are_not_implemented() {
        for command in ["build", "frobnicate"] {
            assert_eq!(
                parse_strs(&[command]),
                Request::Unsupported(command.to_string())
            );
        }
    }

    #[test]
    fn extra_arguments_are_refused() {
        assert_eq!(
            parse_strs(&["--version", "extra"]),
            Request::Unsupported("--version".to_string())
        );
    }

    #[test]
    fn version_text_is_name_and_crate_version() {
        assert_eq!(version_text(), "lodi 2.0.0");
    }

    #[test]
    fn init_takes_its_flags_in_any_order_and_refuses_everything_else() {
        assert_eq!(
            parse_strs(&["init"]),
            Request::Init(lodi::init::Options::default())
        );
        assert_eq!(
            parse_strs(&["init", "--force", "--base", "debian:12", "--name", "x"]),
            Request::Init(lodi::init::Options {
                name: Some("x".into()),
                base: Some("debian:12".into()),
                force: true,
            })
        );
        for bad in [
            &["init", "--name"][..],
            &["init", "--base"],
            &["init", "here"],
            &["init", "--force", "--force"],
            &["init", "--name", "a", "--name", "b"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }

    /// `lodi import [PATH] [--home] [--yes] [--dry-run] [--root DIR] [-v]` (#693, #707, #708),
    /// in any order; nothing else.
    #[test]
    fn import_takes_one_path_home_yes_dry_run_root_and_verbose() {
        let Request::Import(import) = parse_strs(&[
            "import",
            "-v",
            "--home",
            "cfg",
            "--yes",
            "--dry-run",
            "--root",
            "/scratch",
        ]) else {
            panic!("not an import");
        };
        assert_eq!(import.path.as_deref(), Some(std::ffi::OsStr::new("cfg")));
        assert_eq!(import.root.as_deref(), Some(Path::new("/scratch")));
        assert!(import.verbose && import.home && import.yes && import.dry_run);
        assert!(matches!(
            parse_strs(&["import"]),
            Request::Import(Import {
                home: false,
                yes: false,
                dry_run: false,
                ..
            })
        ));
        for refused in [
            &["import", "a", "b"][..],
            &["import", "--yes", "--yes"],
            &["import", "--host", "box"],
            &["import", "--home", "--home"],
            &["import", "--dry-run", "--dry-run"],
            &["import", "--root"],
        ] {
            assert!(
                matches!(parse_strs(refused), Request::Unsupported(_)),
                "{refused:?}"
            );
        }
    }

    #[test]
    fn help_names_only_implemented_commands() {
        let mut help = help_text();
        for command in COMMANDS {
            help.push_str(&command_help(command));
        }
        assert!(help.contains("--version") && help.contains("--help"));
        assert!(help.contains("lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]"));
        assert!(help.contains("lodi develop [--no-nest] [--trust] [-- COMMAND"));
        assert!(help.contains("lodi shell [--no-nest] TOOL[@CONSTRAINT]..."));
        assert!(help.contains("lodi shell [--no-nest] --base DISTRO[:RELEASE] PACKAGE..."));
        assert!(help.contains("lodi run [--no-nest] [--trust] TASK"));
        for removed in ["lodi lock", "lodi trust", "lodi info"] {
            assert!(!help.contains(removed), "{removed}");
        }
        assert!(help.contains("lodi gc [--dry-run] [--keep-days N] [--images] [-v]"));
        assert!(!help.contains("lodi host"));
        // The 1.x names 2.0 removed are in no help (#699).
        for (old, _) in OLD_NAMES {
            assert!(!help.contains(&format!("lodi {old} ")), "{old}");
        }
        assert!(help.contains("lodi import [PATH] [--home] [--yes] [--dry-run] [--root DIR] [-v]"));
        assert!(help.contains("lodi update [PATH] [--host NAME] [--root DIR] [-v]"));
        let usage = format!("{NAME} build");
        assert!(!help.contains(&usage), "help offers {usage}");
    }

    /// `update`, `pin` and `unpin` take a PATH, --host, --root and their own flags in any order
    /// (#696), and refuse a URL's selectors and `--yes`.
    #[test]
    fn the_pin_verbs_take_their_flags_and_refuse_the_rest() {
        let Request::Pins("update", pins) =
            parse_strs(&["update", "--host", "box", "repo", "--root", "r", "-v"])
        else {
            panic!("update did not parse");
        };
        assert_eq!(pins.path.as_deref(), Some(std::ffi::OsStr::new("repo")));
        assert_eq!(pins.host.as_deref(), Some("box"));
        assert!(pins.verbose);
        let Request::Pins("pin", pins) = parse_strs(&["pin", "--to", "1.2", "bc", "cfg"]) else {
            panic!("pin did not parse");
        };
        assert_eq!(
            pins.target,
            Some(lodi::config::verbs::Target::Package("bc".into()))
        );
        assert_eq!(pins.to.as_deref(), Some("1.2"));
        assert!(matches!(
            parse_strs(&["unpin", "--all"]),
            Request::Pins(
                "unpin",
                Pins {
                    target: Some(lodi::config::verbs::Target::All),
                    ..
                }
            )
        ));
        for bad in [
            &["import", "--refresh"][..],
            &["update", "github:o/r", "--ref", "main"],
            &["update", "--yes"],
            &["pin"],
            &["pin", "a", "b", "c"],
            &["unpin", "bc", "--to", "1"],
            &["unpin", "--all", "--all"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }
}
