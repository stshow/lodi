//! `lodi` — the command-line entry point.
//!
//! Only the CLI surface that works is exposed: `--version`, `--help` — and, since LD-360, the
//! part of it about one command or scope, reached by `lodi help`, by `--help` or `-h` anywhere
//! among lodi's own arguments and by a bare `lodi home` or `lodi host` — `lodi init` (M-0.3 T-1),
//! `lodi lock` and `lodi lock --check` (M-Spike S-2), `lodi shell` and `lodi develop` with or
//! without `-- <command>` (M-0.3 T-2), `lodi run <task>`, `lodi trust` (host-tool
//! environments since S-3, rootless Podman container environments since S-4), `lodi gc`,
//! the store collector (M-0.3 T-3), `lodi search` and `lodi info`, the offline browse
//! commands (M-0.4 T-6), and the home scope's `lodi home plan`, `lodi home apply` and
//! `lodi home status` over the files `~/.config/lodi/home.toml` declares (M-0.5 T-2 and T-3),
//! and `lodi home import`, which writes a commented starting file (M-Import T-4, M-Home im-1).
//! The host scope exposes `lodi host plan` and `lodi host apply` for armed machines (M-0.6 T-3),
//! `lodi host import`, the capture of M-Import T-3 that lands in `/etc/lodi` (LD-325), and
//! `lodi host arm` (LD-320), and M-Pin's `lodi host versions`, `pin` and `unpin` (LD-397).
//! Every other
//! argument is refused with exit status 2. Diagnostics use the design `spec/11` format and exit
//! statuses.

use std::ffi::OsString;
use std::path::Path;
use std::process::ExitCode;

use lodi::diag::EXIT_USAGE as USAGE_ERROR;
use lodi::fetch::HttpFetcher;
use lodi::host::{self, Action, Options};
use lodi::lock::{self, LOCK_FILE, Outcome};

const NAME: &str = env!("CARGO_PKG_NAME");
const VERSION: &str = env!("CARGO_PKG_VERSION");

/// What one invocation asks for.
#[derive(Debug, PartialEq, Eq)]
enum Request {
    Version,
    /// The help text, or with a subject (`home`, `host plan`, `init`) the part of it about that
    /// command or scope (LD-360).
    Help(Vec<String>),
    /// `lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]`: write ./lodi.toml.
    Init(lodi::init::Options),
    /// `lodi lock`: resolve and write `lodi.lock` in the current directory.
    Lock,
    /// `lodi lock --check`: exit 10 unless `lodi.lock` is present, valid and fresh; never writes.
    LockCheck,
    /// `lodi shell [--no-nest] [--base DISTRO[:RELEASE]] ITEM…`: an ad-hoc interactive
    /// environment resolved from these arguments alone (M-0.3 T-2, LD-49).
    Shell {
        no_nest: bool,
        request: lodi::shell::Request,
    },
    /// `lodi develop [--no-nest] [-- <program> <args…>]`; an empty argv is the interactive
    /// entry of `spec/10-cli` §3 (LD-48).
    Develop {
        no_nest: bool,
        argv: Vec<OsString>,
    },
    /// `lodi run [--no-nest] <task> <args…>`.
    Run {
        no_nest: bool,
        task: String,
        args: Vec<OsString>,
    },
    /// `lodi trust` / `lodi trust --revoke`.
    Trust {
        revoke: bool,
    },
    /// `lodi gc [--dry-run] [--keep-days N] [--images] [-v]` (M-0.3 T-3).
    Gc(lodi::gc::Options),
    /// `lodi search [--registry | --distro] QUERY` (M-0.4 T-6, design calls D12, D13, D14).
    Search {
        query: String,
        scope: lodi::search::Scope,
    },
    /// `lodi info [TOOL]` (M-0.4 T-6, design call D17).
    Info(Option<String>),
    /// `lodi home apply [--overwrite-drift] [--locked]`: files plus the user-wide tool profile.
    HomeApply(lodi::home::apply::Options),
    HomeApplySource(lodi::home::apply::Options, lodi::home::source::Selection),
    /// `lodi home status`: report on the applied files and exit 0 whatever it finds (T-3).
    HomeStatus,
    HomeStatusSource(lodi::home::source::Selection),
    /// `lodi home plan`: print what an apply would do to the files `<config>/home.toml`
    /// declares, and write nothing at all (M-0.5 T-2, design call D12).
    HomePlan,
    HomePlanSource(lodi::home::source::Selection),
    /// `lodi home init [--out DIR | --stdout] [--force]`, and `lodi home import`, its 1.0 name
    /// (LD-382): write a commented starting `home.toml`, reading no file in the home directory
    /// (M-Import T-4, LD-325, LD-341).
    HomeImport(lodi::home::import::Options),
    HomeImportSource(lodi::home::import::Options, lodi::home::source::Selection),
    /// `lodi host plan [--root DIR] [SOURCE [--host NAME]]`: inspect an armed root and write
    /// nothing; `SOURCE` selects a host directory (LD-379).
    HostPlan(lodi::hostscope::Options),
    /// `lodi host apply ...`: apply one journalled host transaction.
    HostApply(lodi::hostscope::Options),
    /// `lodi host import [--root DIR] [--out DIR | --stdout] [--force] [--dry-run]`: read an
    /// armed root and write the manifest a fresh install could apply — by default into
    /// `<root>/etc/lodi` (LD-325) — or reconcile an existing one (LD-378). It applies nothing.
    HostImport(lodi::hostscope::Options),
    /// `lodi host arm [--root DIR]`: write the arming marker, deliberately and as root on `/`
    /// (LD-320). It is the one command that arms a root.
    HostArm(lodi::hostscope::Options),
    /// `lodi host versions NAME [SOURCE [--host NAME]] [--root DIR]`: what the archive offers of
    /// one package, newest first, and one line to copy (M-Pin, LD-397).
    HostVersions(lodi::hostscope::Options),
    /// `lodi host pin (NAME | --all) [--to VERSION|DATE] [SOURCE [--host NAME]] [--root DIR]`:
    /// pin one name, or the whole file, in the host directory (M-Pin, LD-397).
    HostPin(lodi::hostscope::Options),
    /// `lodi host unpin (NAME | --all) [SOURCE [--host NAME]] [--root DIR]` (M-Pin, LD-397).
    HostUnpin(lodi::hostscope::Options),
    /// `lodi boot confirm [--root DIR]`: in the trial boot of a kernel parameter change, make it
    /// the default (bv-1, LD-425).
    BootConfirm(lodi::hostscope::Options),
    /// `lodi plan|apply|import|update [SOURCE] [--host NAME] [--root DIR] [--yes]` (DONE D2,
    /// LD-416): the repository LD-447 resolves, its host and then every declared home.
    Top(Top, lodi::hostscope::Options, bool),
    /// No arguments at all: print the scope lines of the help's opening and exit 0 (LD-380).
    Missing,
    Unsupported(String),
}

/// The four top-level verbs of a repository (DONE D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Top {
    Plan,
    Apply,
    Import,
    Update,
}

impl Top {
    fn word(self) -> &'static str {
        match self {
            Top::Plan => "plan",
            Top::Apply => "apply",
            Top::Import => "import",
            Top::Update => "update",
        }
    }
}

fn parse<I: IntoIterator<Item = OsString>>(args: I) -> Request {
    let args: Vec<OsString> = args.into_iter().collect();
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
        "plan" => parse_top(Top::Plan, &rest),
        "apply" => parse_top(Top::Apply, &rest),
        "import" => parse_top(Top::Import, &rest),
        "update" => parse_top(Top::Update, &rest),
        "home" => parse_home(&rest),
        "host" => parse_host(&rest),
        "boot" => parse_boot(&rest),
        "gc" => parse_gc(&rest),
        "search" => parse_search(&rest),
        "info" => parse_info(&rest),
        "shell" => parse_shell(&rest),
        "lock" => match (text(0).as_deref(), rest.len()) {
            (None, _) => Request::Lock,
            (Some("--check"), 1) => Request::LockCheck,
            (Some(other), _) => Request::Unsupported(format!("lock {other}")),
        },
        "develop" => {
            let no_nest = text(0).as_deref() == Some("--no-nest");
            let at = usize::from(no_nest);
            match text(at).as_deref() {
                Some("--") if rest.len() > at + 1 => Request::Develop {
                    no_nest,
                    argv: rest[at + 1..].to_vec(),
                },
                Some("--") => Request::Unsupported("develop -- (no command)".into()),
                Some(other) => Request::Unsupported(format!("develop {other}")),
                // No `-- COMMAND` at all is the interactive entry, not a usage error (LD-48).
                None => Request::Develop {
                    no_nest,
                    argv: Vec::new(),
                },
            }
        }
        "run" => {
            let no_nest = text(0).as_deref() == Some("--no-nest");
            let at = usize::from(no_nest);
            match text(at) {
                Some(task) if !task.starts_with('-') => Request::Run {
                    no_nest,
                    task,
                    args: rest[at + 1..].to_vec(),
                },
                Some(other) => Request::Unsupported(format!("run {other}")),
                None => Request::Unsupported("run (no task)".into()),
            }
        }
        "trust" => match (text(0).as_deref(), rest.len()) {
            (None, _) => Request::Trust { revoke: false },
            (Some("--revoke"), 1) => Request::Trust { revoke: true },
            (Some(other), _) => Request::Unsupported(format!("trust {other}")),
        },
        _ if !rest.is_empty() => Request::Unsupported(first),
        "--version" => Request::Version,
        _ => Request::Unsupported(first),
    }
}

/// A request for help (LD-360): `--help` or `-h` anywhere among lodi's own arguments, `help` as
/// the first argument, or a bare `home` or `host`. What lodi hands on to a child is not its own:
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
    let bare = matches!(text.as_slice(), [scope] if scope == "home" || scope == "host");
    if !flag && !named && !bare {
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

fn parse_host(rest: &[OsString]) -> Request {
    let Some(verb) = rest.first().map(|v| v.to_string_lossy().into_owned()) else {
        return Request::Unsupported("host (no verb)".into());
    };
    if verb == "versions" || verb == "pin" || verb == "unpin" {
        return parse_host_pin(&verb, &rest[1..]);
    }
    if verb != "plan" && verb != "apply" && verb != "import" && verb != "arm" {
        return Request::Unsupported(format!("host {verb}"));
    }
    let mut options = lodi::hostscope::Options::default();
    let mut i = 1;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let ok = match arg.as_str() {
            "--root" if options.root.is_none() => match rest.get(i + 1) {
                Some(value) if !value.to_string_lossy().starts_with('-') => {
                    options.root = Some(std::path::PathBuf::from(value));
                    i += 2;
                    true
                }
                _ => false,
            },
            "--overwrite-drift" if verb == "apply" && !options.overwrite_drift => {
                options.overwrite_drift = true;
                i += 1;
                true
            }
            "--resolved" if verb == "apply" && options.resolved.is_none() => {
                match rest.get(i + 1) {
                    Some(value) if !value.to_string_lossy().starts_with('-') => {
                        options.resolved = Some(value.to_string_lossy().into_owned());
                        i += 2;
                        true
                    }
                    _ => false,
                }
            }
            // `lodi host import`'s two flags. `--out` takes a directory and `--force` replaces
            // the manifest already in it, exactly as `lodi init --force` does for a project.
            "--out" if verb == "import" && options.out.is_none() => match rest.get(i + 1) {
                Some(value) if !value.to_string_lossy().starts_with('-') => {
                    options.out = Some(std::path::PathBuf::from(value));
                    i += 2;
                    true
                }
                _ => false,
            },
            "--force" if verb == "import" && !options.force => {
                options.force = true;
                i += 1;
                true
            }
            // `--stdout` prints the manifest instead of landing it (LD-325). It names no
            // destination, so `--out` and `--force` beside it are refused below.
            "--stdout" if verb == "import" && !options.stdout => {
                options.stdout = true;
                i += 1;
                true
            }
            // `--dry-run` shows what an import would change and writes nothing (LD-378).
            "--dry-run" if verb == "import" && !options.dry_run => {
                options.dry_run = true;
                i += 1;
                true
            }
            "--no-update" if verb == "apply" && !options.no_update => {
                options.no_update = true;
                i += 1;
                true
            }
            "--no-home"
                if matches!(verb.as_str(), "plan" | "apply" | "import") && !options.no_home =>
            {
                options.no_home = true;
                i += 1;
                true
            }
            // Arch's partial-upgrade mode is reached by this flag and by nothing else: no
            // manifest key, no environment variable, no default (design call D10). A root whose
            // package manager has no such mode refuses it at the safety gate rather than
            // accepting a flag it would ignore.
            "--unsupported-partial-upgrade"
                if verb == "apply" && !options.unsupported_partial_upgrade =>
            {
                options.unsupported_partial_upgrade = true;
                i += 1;
                true
            }
            // A URL `SOURCE`'s selectors (LD-401): a ref, a commit, or a fresh resolution.
            "--ref" | "--rev"
                if (verb == "plan" || verb == "apply")
                    && options.git_ref.is_none()
                    && options.rev.is_none() =>
            {
                match rest.get(i + 1) {
                    Some(value) if !value.to_string_lossy().starts_with('-') => {
                        let value = Some(value.to_string_lossy().into_owned());
                        if arg == "--ref" {
                            options.git_ref = value;
                        } else {
                            options.rev = value;
                        }
                        i += 2;
                        true
                    }
                    _ => false,
                }
            }
            "--refresh" if (verb == "plan" || verb == "apply") && !options.refresh => {
                options.refresh = true;
                i += 1;
                true
            }
            // `--host NAME` chooses a host in a directory of hosts instead of the hostname
            // (LD-379). Its value is judged where it is used: one safe path component.
            "--host" if verb != "arm" && options.host.is_none() => match rest.get(i + 1) {
                Some(value) if !value.to_string_lossy().starts_with('-') => {
                    options.host = Some(value.to_string_lossy().into_owned());
                    i += 2;
                    true
                }
                _ => false,
            },
            // The positional `SOURCE` (LD-379): a host directory, or a directory of hosts. One,
            // and never something that looks like a flag.
            positional
                if verb != "arm" && options.source.is_none() && !positional.starts_with('-') =>
            {
                options.source = Some(std::path::PathBuf::from(&rest[i]));
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("host {verb} {arg}"));
        }
    }
    if options.stdout && options.out.is_some() {
        return Request::Unsupported("host import --stdout with --out".into());
    }
    if options.stdout && options.force {
        return Request::Unsupported("host import --stdout with --force".into());
    }
    if options.stdout && options.dry_run {
        return Request::Unsupported("host import --stdout with --dry-run".into());
    }
    // A host directory is where the import writes, so a second destination beside it is a
    // contradiction (LD-379); and `--host` chooses within a `SOURCE`, so it needs one.
    if options.source.is_some() && (options.stdout || options.out.is_some()) {
        return Request::Unsupported(format!(
            "host {verb} SOURCE with {}",
            if options.stdout { "--stdout" } else { "--out" }
        ));
    }
    if options.host.is_some() && options.source.is_none() {
        return Request::Unsupported(format!("host {verb} --host without SOURCE"));
    }
    // `--ref`, `--rev` and `--refresh` select within a URL, and a commit is not refreshed.
    let url = options
        .source
        .as_ref()
        .is_some_and(|source| lodi::hostscope::remote::is_url(source.as_os_str()));
    let selector = [
        options.git_ref.as_ref().map(|_| "--ref"),
        options.rev.as_ref().map(|_| "--rev"),
        options.refresh.then_some("--refresh"),
    ];
    if let Some(flag) = selector.iter().flatten().next()
        && !url
    {
        return Request::Unsupported(format!("host {verb} {flag} without a URL SOURCE"));
    }
    if options.rev.is_some() && options.refresh {
        return Request::Unsupported(format!("host {verb} --rev with --refresh"));
    }
    match verb.as_str() {
        "plan" => Request::HostPlan(options),
        "import" => Request::HostImport(options),
        "arm" => Request::HostArm(options),
        _ => Request::HostApply(options),
    }
}

/// Neither `--ref` nor `--rev` is given yet.
fn selects_nothing(options: &lodi::hostscope::Options) -> bool {
    options.git_ref.is_none() && options.rev.is_none()
}

/// A top-level verb's own arguments (DONE D2, LD-416): one `SOURCE`, `--host NAME`, `--root
/// DIR` and `--yes` in any order, and on `plan` and `apply` a URL `SOURCE`'s `--ref`, `--rev` and
/// `--refresh` (LD-401). The repository itself is resolved when the verb runs (LD-447).
fn parse_top(verb: Top, rest: &[OsString]) -> Request {
    let word = verb.word();
    let mut options = lodi::hostscope::Options::default();
    let mut yes = false;
    let reads_url = matches!(verb, Top::Plan | Top::Apply);
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = rest
            .get(i + 1)
            .filter(|value| !value.to_string_lossy().starts_with('-'));
        let text = value.map(|v| v.to_string_lossy().into_owned());
        let ok = match arg.as_str() {
            "--root" if options.root.is_none() && value.is_some() => {
                options.root = value.map(std::path::PathBuf::from);
                i += 2;
                true
            }
            "--host" if options.host.is_none() && value.is_some() => {
                options.host = text;
                i += 2;
                true
            }
            "--yes" if !yes => {
                yes = true;
                i += 1;
                true
            }
            // One of `--ref` and `--rev`, as on the host verbs: the second is refused (#297).
            "--ref" if reads_url && selects_nothing(&options) && value.is_some() => {
                options.git_ref = text;
                i += 2;
                true
            }
            "--rev" if reads_url && selects_nothing(&options) && value.is_some() => {
                options.rev = text;
                i += 2;
                true
            }
            "--refresh" if reads_url && !options.refresh => {
                options.refresh = true;
                i += 1;
                true
            }
            positional if options.source.is_none() && !positional.starts_with('-') => {
                options.source = Some(std::path::PathBuf::from(&rest[i]));
                i += 1;
                true
            }
            _ => false,
        };
        if !ok {
            return Request::Unsupported(format!("{word} {arg}"));
        }
    }
    let url = options
        .source
        .as_ref()
        .is_some_and(|source| lodi::hostscope::remote::is_url(source.as_os_str()));
    let selector = [
        options.git_ref.as_ref().map(|_| "--ref"),
        options.rev.as_ref().map(|_| "--rev"),
        options.refresh.then_some("--refresh"),
    ];
    if let Some(flag) = selector.iter().flatten().next()
        && !url
    {
        return Request::Unsupported(format!("{word} {flag} without a URL SOURCE"));
    }
    if options.rev.is_some() && options.refresh {
        return Request::Unsupported(format!("{word} --rev with --refresh"));
    }
    Request::Top(verb, options, yes)
}

/// M-Pin's three verbs (LD-397): `lodi host versions NAME`, `lodi host pin (NAME | --all) [--to
/// VALUE]` and `lodi host unpin (NAME | --all)`, each with `[SOURCE [--host NAME]] [--root DIR]`
/// in any order. The first positional is the package name, unless `--all` is given; the next is
/// `SOURCE`. A name that is not a package name, a second name, `--to` on anything but `pin` and
/// `--all` on `versions` are usage errors.
fn parse_host_pin(verb: &str, rest: &[OsString]) -> Request {
    let mut options = lodi::hostscope::Options::default();
    let mut positionals: Vec<OsString> = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        let arg = rest[i].to_string_lossy().into_owned();
        let value = rest
            .get(i + 1)
            .filter(|value| !value.to_string_lossy().starts_with('-'));
        let ok = match arg.as_str() {
            "--root" if options.root.is_none() && value.is_some() => {
                options.root = value.map(std::path::PathBuf::from);
                i += 2;
                true
            }
            "--host" if options.host.is_none() && value.is_some() => {
                options.host = value.map(|v| v.to_string_lossy().into_owned());
                i += 2;
                true
            }
            "--to" if verb == "pin" && options.to.is_none() && value.is_some() => {
                options.to = value.map(|v| v.to_string_lossy().into_owned());
                i += 2;
                true
            }
            "--all" if verb != "versions" && !options.all => {
                options.all = true;
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
            return Request::Unsupported(format!("host {verb} {arg}"));
        }
    }
    let mut positionals = positionals.into_iter();
    if !options.all {
        let Some(name) = positionals.next() else {
            return Request::Unsupported(format!("host {verb} (no package name)"));
        };
        let name = name.to_string_lossy().into_owned();
        if !lodi::hostscope::manifest::is_package_name(&name) {
            return Request::Unsupported(format!("host {verb} {name}"));
        }
        options.name = Some(name);
    }
    options.source = positionals.next().map(std::path::PathBuf::from);
    if let Some(extra) = positionals.next() {
        return Request::Unsupported(format!("host {verb} {}", extra.to_string_lossy()));
    }
    if options.host.is_some() && options.source.is_none() {
        return Request::Unsupported(format!("host {verb} --host without SOURCE"));
    }
    match verb {
        "versions" => Request::HostVersions(options),
        "pin" => Request::HostPin(options),
        _ => Request::HostUnpin(options),
    }
}

/// `lodi boot confirm [--root DIR]` (bv-1, LD-425): nothing else is taken.
fn parse_boot(rest: &[OsString]) -> Request {
    let words: Vec<String> = rest
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut options = lodi::hostscope::Options::default();
    match words.as_slice() {
        [verb] if verb == "confirm" => {}
        [verb, flag, dir] if verb == "confirm" && flag == "--root" && !dir.starts_with('-') => {
            options.root = Some(std::path::PathBuf::from(&rest[2]));
        }
        _ => return Request::Unsupported(format!("boot {}", words.join(" "))),
    }
    Request::BootConfirm(options)
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

/// `lodi home`'s own arguments: the first two-word command in this tool (design call D3,
/// `LD-99`). Four verbs exist — `plan`, `apply`, `status` and, since M-Import T-4, `import`,
/// which since LD-382 is also spelled `init`: the one request under either name, so a usage
/// error names the verb that was typed and nothing else differs.
/// A missing verb, an unknown verb, an argument a verb does not take and a flag given twice are
/// usage errors at exit 2 with no `E_` code, like every other usage error here.
fn parse_home(rest: &[OsString]) -> Request {
    let Some(verb) = rest.first().map(|v| v.to_string_lossy().into_owned()) else {
        return Request::Unsupported("home (no verb)".into());
    };
    // A usage error names the argument that was refused, not only the verb it followed.
    let refused =
        |arg: &OsString| Request::Unsupported(format!("home {verb} {}", arg.to_string_lossy()));
    match (verb.as_str(), rest.len()) {
        ("plan", _) | ("status", _) => match parse_home_source(&rest[1..]) {
            Ok(Some(selection)) if verb == "plan" => Request::HomePlanSource(selection),
            Ok(Some(selection)) => Request::HomeStatusSource(selection),
            Ok(None) if verb == "plan" => Request::HomePlan,
            Ok(None) => Request::HomeStatus,
            Err(arg) => Request::Unsupported(format!("home {verb} {arg}")),
        },
        ("import" | "init", _) => {
            let mut options = lodi::home::import::Options::default();
            let mut named = Vec::new();
            let mut i = 1;
            while i < rest.len() {
                match rest[i].to_string_lossy().as_ref() {
                    "--force" if !options.force => {
                        options.force = true;
                        i += 1;
                    }
                    "--out" if options.out.is_none() && i + 1 < rest.len() => {
                        options.out = Some(rest[i + 1].to_string_lossy().into_owned());
                        i += 2;
                    }
                    "--stdout" if !options.stdout => {
                        options.stdout = true;
                        i += 1;
                    }
                    "--host" if i + 1 < rest.len() => {
                        named.push(rest[i].clone());
                        named.push(rest[i + 1].clone());
                        i += 2;
                    }
                    value if !value.starts_with('-') => {
                        named.push(rest[i].clone());
                        i += 1;
                    }
                    _ => return refused(&rest[i]),
                }
            }
            // `--stdout` names no destination (LD-325): a destination directory or a replacement
            // beside it is a contradiction, and a usage error.
            if options.stdout && options.out.is_some() {
                return Request::Unsupported(format!("home {verb} --stdout with --out"));
            }
            if options.stdout && options.force {
                return Request::Unsupported(format!("home {verb} --stdout with --force"));
            }
            match parse_home_source(&named) {
                Ok(Some(selection)) if options.out.is_none() && !options.stdout => {
                    options.fixed_path = true;
                    Request::HomeImportSource(options, selection)
                }
                Ok(Some(_)) => Request::Unsupported(format!("home {verb} SOURCE with --out")),
                Ok(None) => Request::HomeImport(options),
                Err(arg) => Request::Unsupported(format!("home {verb} {arg}")),
            }
        }
        ("apply", _) => {
            let mut options = lodi::home::apply::Options::default();
            let mut source = None;
            let mut host = None;
            let mut i = 1;
            while i < rest.len() {
                let arg = &rest[i];
                match arg.to_string_lossy().as_ref() {
                    "--overwrite-drift" if !options.overwrite_drift => {
                        options.overwrite_drift = true;
                        i += 1;
                    }
                    "--locked" if !options.locked => {
                        options.locked = true;
                        i += 1;
                    }
                    "--host"
                        if host.is_none()
                            && rest
                                .get(i + 1)
                                .is_some_and(|v| !v.to_string_lossy().starts_with('-')) =>
                    {
                        host = Some(rest[i + 1].to_string_lossy().into_owned());
                        i += 2;
                    }
                    text if !text.starts_with('-') && source.is_none() => {
                        source = Some(std::path::PathBuf::from(arg));
                        i += 1;
                    }
                    _ => return refused(arg),
                }
            }
            match source {
                Some(source) => Request::HomeApplySource(
                    options,
                    lodi::home::source::Selection { source, host },
                ),
                None if host.is_some() => Request::Unsupported("home apply --host".into()),
                None => Request::HomeApply(options),
            }
        }
        _ => Request::Unsupported(format!("home {verb}")),
    }
}

fn parse_home_source(args: &[OsString]) -> Result<Option<lodi::home::source::Selection>, String> {
    let mut source = None;
    let mut host = None;
    let mut i = 0;
    while i < args.len() {
        let value = args[i].to_string_lossy();
        if value == "--host"
            && host.is_none()
            && args
                .get(i + 1)
                .is_some_and(|v| !v.to_string_lossy().starts_with('-'))
        {
            host = Some(args[i + 1].to_string_lossy().into_owned());
            i += 2;
        } else if !value.starts_with('-') && source.is_none() {
            source = Some(std::path::PathBuf::from(&args[i]));
            i += 1;
        } else {
            return Err(value.into_owned());
        }
    }
    match source {
        Some(source) => Ok(Some(lodi::home::source::Selection { source, host })),
        None if host.is_some() => Err("--host needs SOURCE".into()),
        None => Ok(None),
    }
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

/// `lodi info`'s own arguments: nothing, or one tool name. There is no `--json` (design call
/// D14), so every flag is a usage error at exit 2; `info --help` is answered before this runs
/// (LD-360).
fn parse_info(rest: &[OsString]) -> Request {
    match rest {
        [] => Request::Info(None),
        [one] if !one.to_string_lossy().starts_with('-') => {
            Request::Info(Some(one.to_string_lossy().into_owned()))
        }
        [first, ..] => Request::Unsupported(format!("info {}", first.to_string_lossy())),
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

/// The top of `lodi --help`: the purpose, the opening a bare `lodi` prints (lines 3 to 8), one
/// example of each first command, the command groups and where the reference is (LD-463).
fn help_text() -> String {
    let mut groups = String::new();
    for (label, scope, words) in HELP_GROUPS {
        groups.push_str(&format!(
            "  {label:<12}  {NAME} {scope}{}\n",
            words.join(", ")
        ));
    }
    format!(
        "{NAME} {VERSION} — environment and system manager for Ubuntu, Debian, Arch and Fedora

Three independent scopes. Start with the one you want. None needs another:
  this machine  sudo {NAME} host arm      once: allow lodi to manage this machine
                sudo {NAME} host import   copy its packages and changed /etc files
  your home     {NAME} home init          write a starting home.toml
  a project     {NAME} init               write ./lodi.toml here; then {NAME} lock
Commands without 'host' or 'home' act on the current directory.

Examples:
  sudo {NAME} host arm                once, before any other host command
  sudo {NAME} host import ~/lodi      copy this machine's packages into ~/lodi
  {NAME} home init --stdout           print a starting home.toml and write nothing
  {NAME} init --base debian:bookworm  start a project in a Debian container
  {NAME} lock --check                 check that ./lodi.lock matches ./lodi.toml
  {NAME} trust                        review the project's tasks and allow them
  {NAME} develop -- make              run make in the project's environment
  {NAME} run build                    run the project's build task

Commands ('{NAME} help COMMAND' shows one with its examples):
{groups}  help          {NAME} help [COMMAND], --help, --version

Anything else is refused with exit status {USAGE_ERROR}.
Reference: https://github.com/stshow/lodi/blob/main/docs/CLI.md"
    )
}

/// The command groups of `lodi --help`: a label, the scope word its commands share, and the
/// commands in the order they are listed. Each is a key of [`COMMANDS`], and a test holds the
/// two to the same set.
const HELP_GROUPS: [(&str, &str, &[&str]); 6] = [
    ("a repository", "", &["import", "plan", "apply", "update"]),
    (
        "this machine",
        "host ",
        &["arm", "import", "plan", "apply", "versions", "pin", "unpin"],
    ),
    ("a trial boot", "boot ", &["confirm"]),
    (
        "your home",
        "home ",
        &["init", "import", "plan", "apply", "status"],
    ),
    (
        "a project",
        "",
        &["init", "lock", "develop", "run", "trust", "info"],
    ),
    ("anywhere", "", &["shell", "search", "gc"]),
];

/// One command's help (LD-463): what it does in one line, how it is typed, one to three
/// examples with what each does, and the details. `lodi --help` lists the keys, and
/// `lodi help COMMAND` prints one entry, so neither can name a command the other does not.
struct CommandHelp {
    key: &'static str,
    summary: &'static str,
    usage: &'static [&'static str],
    examples: Examples,
    details: &'static str,
}

/// Examples as the user types them, each with what it does.
type Examples = &'static [(&'static str, &'static str)];

/// What `lodi help home` and `lodi help host` say about their scope before its commands.
const SCOPES: [(&str, &str, Examples); 3] = [
    (
        "home",
        "Manage the files and tools of your home directory from a home.toml you keep.",
        &[
            (
                "lodi home init",
                "write a starting ~/.config/lodi/home.toml",
            ),
            ("lodi home plan", "show what an apply would change"),
            (
                "lodi home apply",
                "write the files and tools home.toml declares",
            ),
        ],
    ),
    (
        "host",
        "Manage this machine's packages, sources and files from a host.toml you keep.",
        &[
            (
                "sudo lodi host import ~/lodi",
                "copy this machine's packages into ~/lodi",
            ),
            (
                "sudo lodi host plan ~/lodi",
                "show what applying ~/lodi would change",
            ),
            (
                "sudo lodi host apply ~/lodi",
                "make this machine match ~/lodi",
            ),
        ],
    ),
    (
        "boot",
        "Confirm a kernel parameter change, which boots once as a trial first.",
        &[(
            "sudo lodi boot confirm",
            "in the trial boot: make it the default",
        )],
    ),
];

const COMMANDS: &[CommandHelp] = &[
    CommandHelp {
        key: "help",
        summary: "Show help for lodi, for one scope or for one command.",
        usage: &["lodi help [COMMAND]"],
        examples: &[
            ("lodi help init", "the help of lodi init"),
            ("lodi help host", "every command of the host scope"),
            ("lodi help host plan", "one command of the host scope"),
        ],
        details: "COMMAND is a command (init, lock), a scope (home, host) or a scope's command\n\
                  (home plan, host arm). COMMAND --help, COMMAND -h and a bare lodi home or\n\
                  lodi host print the same help. Help reads nothing and runs nothing.",
    },
    CommandHelp {
        key: "import",
        summary: "Copy this machine into a repository you keep, with a home.toml for you.",
        usage: &["lodi import [SOURCE] [--host NAME] [--root DIR] [--yes]"],
        examples: &[
            (
                "sudo lodi import ~/lodi",
                "copy this machine into ~/lodi/HOSTNAME/",
            ),
            (
                "sudo lodi import --yes",
                "the same, into the current directory",
            ),
        ],
        details: "It writes <repository>/<hostname>/, or --host NAME, as the repository's owner\n\
                  even under sudo, with a commented home/<login>/home.toml for you. It applies\n\
                  nothing and runs no git. The repository is SOURCE, else the current directory\n\
                  once you confirm it (--yes confirms; with no terminal, --yes is needed), else\n\
                  LODI_REPO.",
    },
    CommandHelp {
        key: "plan",
        summary: "Show what lodi apply would change: the machine, then each home.",
        usage: &[
            "lodi plan [SOURCE] [--host NAME] [--root DIR] [--yes]",
            "          [--ref NAME | --rev COMMIT | --refresh]",
        ],
        examples: &[
            ("sudo lodi plan ~/lodi", "what applying ~/lodi would change"),
            (
                "sudo lodi plan github:OWNER/REPO --refresh",
                "its newest commit, from GitHub",
            ),
        ],
        details: "It plans the host, then every home it declares, each as its own user, and\n\
                  writes nothing. SOURCE is a path, or a git URL or alias as lodi host plan\n\
                  takes it, else the current directory once you confirm it (--yes), else\n\
                  LODI_REPO. --ref, --rev and --refresh choose the commit of a URL.",
    },
    CommandHelp {
        key: "apply",
        summary: "Make this machine and each home match your repository.",
        usage: &[
            "lodi apply [SOURCE] [--host NAME] [--root DIR] [--yes]",
            "           [--ref NAME | --rev COMMIT | --refresh]",
        ],
        examples: &[
            (
                "sudo lodi apply ~/lodi",
                "apply the machine, then each home",
            ),
            (
                "sudo lodi apply github:OWNER/REPO --refresh",
                "its newest commit, from GitHub",
            ),
        ],
        details: "It applies the host as root, then each home/<login>/ the host declares, as\n\
                  that user in a process of its own. A login this machine does not have is\n\
                  skipped with a line, and one home's failure does not stop the next: the\n\
                  exit status is the first failure's. SOURCE is chosen as for lodi plan.",
    },
    CommandHelp {
        key: "update",
        summary: "Move your repository's snapshot to the last published day and lock it.",
        usage: &["lodi update [SOURCE] [--host NAME] [--root DIR] [--yes]"],
        examples: &[(
            "lodi update ~/lodi",
            "move the snapshot and write ~/lodi/lodi.lock",
        )],
        details: "It locks the snapshot, the pins, the signed_by_url keys and the homes' tools\n\
                  in <repository>/lodi.lock, the one lock of the repository. A floating host\n\
                  stays floating and a pin you wrote never moves. It writes all of it or\n\
                  nothing. 1.4's pins.lock and home.lock are read until a writer (update,\n\
                  host pin, host unpin, a home's tools) moves them into lodi.lock.",
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
        details: "It also adds .lodi/ to ./.gitignore. --base writes a [container] manifest\n\
                  instead of host tools, from one of the bases\n\
                  arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble. --force replaces\n\
                  an existing ./lodi.toml. It works offline: nothing is downloaded, installed\n\
                  or run.",
    },
    CommandHelp {
        key: "lock",
        summary: "Pin the project's tools and packages in ./lodi.lock.",
        usage: &["lodi lock", "lodi lock --check"],
        examples: &[
            ("lodi lock", "write ./lodi.lock, or say it is up to date"),
            (
                "lodi lock --check",
                "exit 10 unless ./lodi.lock matches ./lodi.toml",
            ),
        ],
        details: "It resolves ./lodi.toml from upstream metadata only: nothing is installed,\n\
                  built or run. --check never resolves or writes, and exits 10 unless\n\
                  ./lodi.lock exists, is valid and matches ./lodi.toml. An arch base comes\n\
                  with no keyring and no signature check: each package is a verified file,\n\
                  and HTTPS and the lock's hashes vouch for it, as it is for debian and ubuntu.",
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
        usage: &["lodi develop [--no-nest] [-- COMMAND [ARGS...]]"],
        examples: &[
            ("lodi develop", "an interactive shell in the environment"),
            (
                "lodi develop -- make",
                "run make there and exit with its status",
            ),
        ],
        details: "The environment is ./lodi.toml and ./lodi.lock: upstream tools on this\n\
                  machine, or a rootless Podman container for a [container] manifest. It\n\
                  uses the lock as it is, so run lodi lock first. COMMAND gets its arguments\n\
                  unchanged. The shell is $SHELL when it is set and executable, else\n\
                  /bin/sh. lodi never installs Podman and changes no setting of the machine.",
    },
    CommandHelp {
        key: "run",
        summary: "Run one of the project's tasks.",
        usage: &["lodi run [--no-nest] TASK [ARGS...]"],
        examples: &[
            (
                "lodi run versions",
                "run the [tasks.versions] entry of ./lodi.toml",
            ),
            ("lodi run build", "run [tasks.build] the same way"),
        ],
        details: "The task runs with /bin/sh -c in the environment lodi develop uses, from\n\
                  the lock as it is. ARGS go to the task unchanged, --help included. A task\n\
                  runs only after lodi trust.",
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
        details: "It lists the catalogue tools whose name or description contains QUERY,\n\
                  then the packages of the distro indexes ./lodi.lock names, read from the\n\
                  download cache. It makes no network request and writes nothing. A cold\n\
                  cache is a note on standard error and exit 0. --registry searches the\n\
                  catalogue only, and --distro the indexes only.",
    },
    CommandHelp {
        key: "info",
        summary: "Summarize this project, or show what lodi knows about one tool.",
        usage: &["lodi info [TOOL]"],
        examples: &[
            (
                "lodi info",
                "the base, the tools and whether ./lodi.lock is fresh",
            ),
            (
                "lodi info python",
                "the python recipe and what this project locked",
            ),
        ],
        details: "With no argument it summarizes ./lodi.toml: the base, the tools with their\n\
                  constraints and locked versions, and whether ./lodi.lock is fresh, stale or\n\
                  missing. With a TOOL it prints what the catalogue knows about that recipe\n\
                  and what this project locked for it. It works offline and writes nothing.",
    },
    CommandHelp {
        key: "trust",
        summary: "Review the project's tasks and allow them to run.",
        usage: &["lodi trust [--revoke]"],
        examples: &[
            (
                "lodi trust",
                "show the task text of ./lodi.toml and record your trust",
            ),
            ("lodi trust --revoke", "take that trust back"),
        ],
        details: "Nothing in ./lodi.toml runs until it is trusted, and any change to the\n\
                  file asks again.",
    },
    CommandHelp {
        key: "home init",
        summary: "Write a starting home.toml for this home directory.",
        usage: &["lodi home init [--out DIR | --stdout] [--force] [SOURCE [--host NAME]]"],
        examples: &[
            ("lodi home init", "write ~/.config/lodi/home.toml"),
            ("lodi home init --stdout", "print it and write nothing"),
        ],
        details: "It reads no file in this home directory. It writes a commented\n\
                  [programs.<name>] stub for each program lodi has a module for whose\n\
                  executable is on PATH, and a [tools] and a [home.file] example, all\n\
                  commented out, with a note naming which paths those programs use already\n\
                  exist. With no flag it goes where home plan reads it: $XDG_CONFIG_HOME/lodi,\n\
                  else ~/.config/lodi. --out DIR writes DIR/home.toml inside this home\n\
                  directory instead. --stdout prints it and writes nothing. An existing\n\
                  home.toml needs --force. SOURCE writes in a named home or a selected\n\
                  host's home.",
    },
    CommandHelp {
        key: "home import",
        summary: "The 1.0 name of lodi home init, which does exactly the same.",
        usage: &["lodi home import [--out DIR | --stdout] [--force] [SOURCE [--host NAME]]"],
        examples: &[("lodi home import --stdout", "print a starting home.toml")],
        details: "It takes the same flags, prints the same output and exits with the same\n\
                  statuses as lodi home init, and it stays for all of 1.x.",
    },
    CommandHelp {
        key: "home plan",
        summary: "Show what lodi home apply would change. It changes nothing.",
        usage: &["lodi home plan [SOURCE [--host NAME]]"],
        examples: &[("lodi home plan", "plan ~/.config/lodi/home.toml")],
        details: "It lists what an apply would do to each file ~/.config/lodi/home.toml\n\
                  declares, in lexicographic order with the mode of each. No file is\n\
                  written, not even the configuration directory.",
    },
    CommandHelp {
        key: "home apply",
        summary: "Write the files and tools your home.toml declares.",
        usage: &["lodi home apply [--overwrite-drift] [--locked] [SOURCE [--host NAME]]"],
        examples: &[
            (
                "lodi home apply",
                "make your home directory match home.toml",
            ),
            (
                "lodi home apply --locked",
                "use home.lock as it is and resolve nothing",
            ),
        ],
        details: "It creates the declared files with their modes, keeps one copy of a file\n\
                  that was there before lodi took the path over, and restores, deletes or\n\
                  keeps a file whose entry is gone. A managed file edited by hand stops the\n\
                  whole apply with E_DRIFT, and nothing is written. --overwrite-drift takes\n\
                  such a path back and keeps the edited bytes in the backup store. [tools]\n\
                  are pinned in home.lock, downloaded and unpacked, and linked into one PATH\n\
                  directory lodi makes. --locked refuses to resolve. No directory is ever\n\
                  removed and no shell configuration file is ever touched.",
    },
    CommandHelp {
        key: "home status",
        summary: "Report whether each managed file still matches home.toml.",
        usage: &["lodi home status [SOURCE [--host NAME]]"],
        examples: &[(
            "lodi home status",
            "ok, drift, missing, unmanaged or stale-backup",
        )],
        details: "It reports each declared and managed path as ok, drift, missing, unmanaged\n\
                  or stale-backup. It exits 0 whatever it finds, and writes nothing.",
    },
    CommandHelp {
        key: "boot confirm",
        summary: "Make the trial boot of a kernel parameter change the default.",
        usage: &["lodi boot confirm [--root DIR]"],
        examples: &[(
            "sudo lodi boot confirm",
            "in the trial boot: make it the default",
        )],
        details: "An apply that changes [kernel] parameters boots them once, as a trial entry,\n\
                  and leaves the default entry as it was. Run this in that trial boot to make\n\
                  the change the default; the entry before it stays as a fallback. Without it,\n\
                  the boot after the trial is the earlier entry again, and lodi says so. It\n\
                  changes nothing in any other boot.",
    },
    CommandHelp {
        key: "host arm",
        summary: "Allow lodi to manage this machine. Run it once.",
        usage: &["lodi host arm [--root DIR]"],
        examples: &[("sudo lodi host arm", "arm this machine")],
        details: "It creates the empty marker <root>/etc/lodi/host-allowed at mode 0644. On /\n\
                  it must run as root, and under --root DIR you arm a tree of your own. A\n\
                  valid marker is left as it is, and an invalid one is refused, never\n\
                  repaired. Nothing else arms a root. Delete the file to disarm it.",
    },
    CommandHelp {
        key: "host import",
        summary: "Copy this machine's packages and changed /etc files into a host.toml.",
        usage: &[
            "lodi host import [--root DIR] [--out DIR | --stdout] [--force] [--dry-run]",
            "                 [--no-home] [SOURCE [--host NAME]]",
        ],
        examples: &[
            (
                "sudo lodi host import ~/lodi",
                "write ~/lodi/<hostname>/host.toml",
            ),
            (
                "sudo lodi host import --dry-run ~/lodi",
                "show what a new import would merge",
            ),
            ("sudo lodi host import --stdout", "print the manifest only"),
        ],
        details: "It reads an armed root's own package database and writes the host.toml a\n\
                  fresh install could apply. With no flag it writes <root>/etc/lodi/host.toml\n\
                  with the files/ bundle beside it, where host plan reads it. On / that needs\n\
                  root. --out DIR writes the bundle there instead. --stdout prints the\n\
                  manifest and copies no configuration file. Over an existing host.toml it\n\
                  merges in what changed on the machine since the last import and keeps your\n\
                  edits. --force replaces it instead, and --dry-run prints the diff and\n\
                  writes nothing. It applies nothing and takes no lock. With SOURCE it writes\n\
                  the host directory SOURCE selects, as the owner of SOURCE even under sudo,\n\
                  and a commented home/<login>/home.toml for you unless one is there or\n\
                  --no-home is given.",
    },
    CommandHelp {
        key: "host plan",
        summary: "Show what lodi host apply would change on this machine. It changes nothing.",
        usage: &["lodi host plan [--root DIR] [--no-home] [SOURCE [--host NAME]]"],
        examples: &[
            ("sudo lodi host plan", "plan <root>/etc/lodi/host.toml"),
            (
                "sudo lodi host plan ~/lodi",
                "plan from a host directory you own",
            ),
            (
                "sudo lodi host plan github:OWNER/REPO",
                "plan from a public git repository",
            ),
        ],
        details: "It shows the package and file changes the host.toml declares, and writes\n\
                  nothing. SOURCE is a host directory, which holds host.toml and files/, or\n\
                  a directory of hosts, one per hostname. --host NAME picks another host\n\
                  than this one. SOURCE must belong to root or you and be writable by nobody\n\
                  else. A selected home is shown after the host, and --no-home skips it.\n\
                  SOURCE may also be a public git+https:// URL, or github:OWNER/REPO,\n\
                  gitlab:OWNER/REPO or codeberg:OWNER/REPO. Its commit is fetched, verified\n\
                  and locked, and --ref NAME, --rev COMMIT and --refresh choose another one.",
    },
    CommandHelp {
        key: "host apply",
        summary: "Make this machine match host.toml: its packages, package sources and files.",
        usage: &[
            "lodi host apply [--root DIR] [--overwrite-drift] [--resolved JOURNAL-ID]",
            "                [--no-update] [--unsupported-partial-upgrade] [--no-home]",
            "                [SOURCE [--host NAME]]",
        ],
        examples: &[
            ("sudo lodi host apply", "apply <root>/etc/lodi/host.toml"),
            (
                "sudo lodi host apply ~/lodi",
                "apply a host directory you own",
            ),
            (
                "sudo lodi host apply --no-update ~/lodi",
                "without refreshing the package index",
            ),
        ],
        details: "It makes the changes host plan shows. lodi records each step, so an\n\
                  interrupted apply can pick up where it left off, and one it cannot sort\n\
                  out stops until --resolved names its journal. The root must be armed\n\
                  first with host arm, and lodi never runs sudo itself. On arch the apply\n\
                  upgrades the whole machine, because arch does not support a partial\n\
                  upgrade. --unsupported-partial-upgrade installs without upgrading, and is\n\
                  the only way to do that. On debian and ubuntu the package index is\n\
                  refreshed when it is over six hours old, unless --no-update is given.\n\
                  A selected home is applied after the host, as you, with root given up\n\
                  for good.",
    },
    CommandHelp {
        key: "host versions",
        summary: "List the versions of a package the archive offers.",
        usage: &["lodi host versions NAME [--root DIR] [SOURCE [--host NAME]]"],
        examples: &[(
            "lodi host versions tree ~/lodi",
            "every version of tree, newest first",
        )],
        details: "It lists the versions of NAME, newest first, with the day each arrived and\n\
                  which is installed, pinned and latest, then one line to copy. There is no\n\
                  picker.",
    },
    CommandHelp {
        key: "host pin",
        summary: "Keep a package at one version, or the whole machine at one date.",
        usage: &[
            "lodi host pin (NAME | --all) [--to VERSION|DATE] [--root DIR]",
            "              [SOURCE [--host NAME]]",
        ],
        examples: &[
            (
                "lodi host pin tree ~/lodi",
                "keep tree at the version installed now",
            ),
            (
                "lodi host pin --all ~/lodi",
                "keep every other package at today's archive",
            ),
        ],
        details: "It pins NAME in host.toml to a version or a date, the installed version by\n\
                  default, or with --all the whole file to a date, now by default. What the\n\
                  pin resolves to is recorded in the lodi.lock of the repository SOURCE\n\
                  names, or in pins.lock in /etc/lodi. It changes only those lines and that\n\
                  file, writes nothing on a failure and applies nothing. A host directory\n\
                  you own needs no root.",
    },
    CommandHelp {
        key: "host unpin",
        summary: "Remove a pin, so the package follows host.toml again.",
        usage: &["lodi host unpin (NAME | --all) [--root DIR] [SOURCE [--host NAME]]"],
        examples: &[
            ("lodi host unpin tree ~/lodi", "remove the pin of tree"),
            (
                "lodi host unpin --all ~/lodi",
                "remove the snapshot, every pin and pins.lock",
            ),
        ],
        details: "The next host apply releases the holds and changes no version.",
    },
];

/// The help of one scope: its summary, the usage of each of its commands, and its examples.
fn scope_help(scope: &str, summary: &str, examples: &[(&str, &str)]) -> String {
    let usage: Vec<&str> = COMMANDS
        .iter()
        .filter(|c| c.key.split(' ').next() == Some(scope) && c.key.contains(' '))
        .flat_map(|c| c.usage.iter().copied())
        .collect();
    format!(
        "{summary}\n\nUsage:\n{}\n\nExamples:\n{}\n\n'{NAME} help {scope} COMMAND' shows one \
         command with its examples and details.",
        indent(&usage),
        example_lines(examples)
    )
}

/// The help of one command.
fn command_help(command: &CommandHelp) -> String {
    let details: Vec<&str> = command.details.lines().collect();
    format!(
        "{}\n\nUsage:\n{}\n\nExamples:\n{}\n\nDetails:\n{}",
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

/// The command or scope `words` names: nothing for the whole text (no words, or a first word
/// that is a flag), one word for a command (`init`) or a scope (`home`), two for a scope's
/// command (`home plan`). Everything after the subject is ignored — it is an argument of the
/// command asked about, and so is a flag in the second word's place (`host --root DIR`).
/// `None` when the first word is no command, or when a scope's second word is none of its
/// commands.
fn help_subject(words: &[&str]) -> Option<Vec<String>> {
    let Some(first) = words.first().filter(|w| !w.starts_with('-')) else {
        return Some(Vec::new());
    };
    let under: Vec<Vec<&str>> = COMMANDS
        .iter()
        .map(|c| c.key.split(' ').collect::<Vec<_>>())
        .filter(|key| key.first() == Some(first))
        .collect();
    if under.is_empty() {
        return None;
    }
    if let Some(second) = words.get(1).filter(|w| !w.starts_with('-')) {
        if under.iter().any(|key| key.get(1) == Some(second)) {
            return Some(vec![first.to_string(), second.to_string()]);
        }
        // Every command of a scope has two words, so a second word that is none of them is
        // refused.
        if under.iter().all(|key| key.len() > 1) {
            return None;
        }
    }
    Some(vec![first.to_string()])
}

/// What a bare `lodi` prints: the scope lines of the help's opening, lines 3 to 8, cut from
/// `help_text()` so that the two can never say different things (LD-380).
fn orientation() -> String {
    help_text()
        .lines()
        .skip(2)
        .take(6)
        .collect::<Vec<_>>()
        .join("\n")
}

/// What a help request prints: the top of the help for no subject, a scope's help for `home`
/// or `host`, and one command's help otherwise.
fn help_for(subject: &[String]) -> String {
    let key = subject.join(" ");
    if key.is_empty() {
        return help_text();
    }
    if let Some((scope, summary, examples)) = SCOPES.iter().find(|(s, _, _)| *s == key) {
        return scope_help(scope, summary, examples);
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

fn run_lock(check: bool) -> ExitCode {
    let root = Path::new(".");
    let result = if check {
        lock::frozen(root).map(|l| format!("{LOCK_FILE} is up to date: {}", lock::summary(&l)))
    } else {
        let fetcher = match fetcher_from_env() {
            Ok(f) => f,
            Err(e) => {
                let d = lodi::diag::Diagnostic::new("E_CONFIG", e);
                eprintln!("{d}");
                return ExitCode::from(lodi::diag::exit_status(d.code));
            }
        };
        lock::lock_project(root, &fetcher, lodi::util::now_utc()).map(|outcome| match outcome {
            Outcome::UpToDate(l) => format!("{LOCK_FILE} is up to date: {}", lock::summary(&l)),
            // A lock written with nothing resolved names no parenthesis: an empty one reads as
            // something left out.
            Outcome::Written { lock: l, resolved } if resolved.is_empty() => {
                format!("wrote {LOCK_FILE}: {}", lock::summary(&l))
            }
            Outcome::Written { lock: l, resolved } => format!(
                "wrote {LOCK_FILE} (resolved {}): {}",
                resolved.join(", "),
                lock::summary(&l)
            ),
        })
    };
    match result {
        Ok(message) => {
            println!("{message}");
            ExitCode::SUCCESS
        }
        Err(failure) => {
            eprintln!("{failure}");
            ExitCode::from(failure.exit_status)
        }
    }
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

fn run_entry(action: Action, no_nest: bool) -> ExitCode {
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
        trust_once: std::env::var_os("LODI_TRUST").is_some_and(|v| v == "1"),
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

/// `lodi home plan`. It reads the roots, loads `<config>/home.toml` and prints the plan; the
/// whole path is read-only, so a failure leaves the user's files exactly as they were.
fn home_fence(roots: &lodi::roots::Roots) -> Result<(), lodi::diag::Diagnostic> {
    // A state that cannot be read is the verb's own refusal, at the verb's own status.
    let Ok(state) = lodi::home::state::read(&roots.data_root()) else {
        return Ok(());
    };
    let source = state.source();
    if source == lodi::home::state::DEFAULT_SOURCE {
        return Ok(());
    }
    // A home that came with a host from a git URL is applied again only with that host (LD-401).
    let hint = if lodi::hostscope::remote::is_url(std::ffi::OsStr::new(source)) {
        format!("it came with its host: sudo lodi host apply {source}")
    } else {
        format!("name the source: lodi home apply {source}")
    };
    Err(lodi::diag::Diagnostic::new(
        "E_DECLINED",
        format!("the last home apply read {source}; a bare home verb reads ~/.config/lodi"),
    )
    .hint(hint))
}

fn run_home_plan() -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    if let Err(d) = home_fence(&roots) {
        eprintln!("{d}");
        return ExitCode::from(lodi::diag::exit_status(d.code));
    }
    report_home_plan(lodi::home::plan::plan(&roots))
}

fn report_home_plan(
    result: Result<lodi::home::plan::Plan, lodi::manifest::ManifestErrors>,
) -> ExitCode {
    match result {
        Ok(plan) => {
            for warning in &plan.warnings {
                eprintln!("{warning}");
            }
            for line in plan.lines() {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(errors) => {
            eprintln!("{errors}");
            ExitCode::from(lodi::manifest::ManifestErrors::EXIT_STATUS)
        }
    }
}

fn run_home_plan_source(selection: &lodi::home::source::Selection) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let selected = match lodi::home::source::load(roots, selection) {
        Ok(selected) => selected,
        Err(failure) => return report_home(Err(failure)),
    };
    let plan = lodi::home::plan::plan_for(
        &selected.roots,
        &selected.manifest,
        false,
        &Default::default(),
    )
    .map(|mut plan| {
        plan.warnings = lodi::home::plan::manifest_warnings(&selected.roots, &selected.manifest);
        plan
    });
    report_home_plan(plan.map_err(|d| lodi::manifest::ManifestErrors {
        diagnostics: vec![d],
    }))
}

fn run_home_status_source(selection: &lodi::home::source::Selection) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let selected = match lodi::home::source::load(roots, selection) {
        Ok(selected) => selected,
        Err(failure) => return report_home(Err(failure)),
    };
    report_home(lodi::home::apply::status_preloaded(
        &selected.roots,
        &selected.manifest,
    ))
}

/// `lodi home apply` and `lodi home status`. Both print their `W_…` lines on standard error and
/// their report on standard output; a failure carries the status its code maps to, which is how
/// `E_DRIFT` reaches exit 8 (M-0.5 T-3, design call D6).
fn run_home(
    verb: impl FnOnce(
        &lodi::roots::Roots,
    ) -> Result<lodi::home::apply::Outcome, lodi::home::apply::Failure>,
) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    if let Err(d) = home_fence(&roots) {
        eprintln!("{d}");
        return ExitCode::from(lodi::diag::exit_status(d.code));
    }
    report_home(verb(&roots))
}

fn report_home(result: Result<lodi::home::apply::Outcome, lodi::home::apply::Failure>) -> ExitCode {
    match result {
        Ok(outcome) => {
            for warning in &outcome.warnings {
                eprintln!("{warning}");
            }
            for line in &outcome.lines {
                println!("{line}");
            }
            ExitCode::SUCCESS
        }
        Err(failure) => {
            for warning in &failure.warnings {
                eprintln!("{warning}");
            }
            eprintln!("{}", failure.text);
            ExitCode::from(failure.status)
        }
    }
}

fn run_home_apply_source(
    options: &lodi::home::apply::Options,
    selection: &lodi::home::source::Selection,
) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let selected = match lodi::home::source::load(roots, selection) {
        Ok(selected) => selected,
        Err(failure) => return report_home(Err(failure)),
    };
    let fetcher = match fetcher_from_env() {
        Ok(fetcher) => fetcher,
        Err(error) => {
            let d = lodi::diag::Diagnostic::new("E_CONFIG", error);
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    report_home(lodi::home::apply::apply_preloaded_to(
        &selected.roots,
        &selected.manifest,
        selected.lock,
        selected.target.as_ref(),
        &selected.source,
        options,
        &fetcher,
        lodi::util::now_utc(),
    ))
}

fn run_home_apply(options: &lodi::home::apply::Options) -> ExitCode {
    let fetcher = match fetcher_from_env() {
        Ok(fetcher) => fetcher,
        Err(error) => {
            let d = lodi::diag::Diagnostic::new("E_CONFIG", error);
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    run_home(|roots| lodi::home::apply::apply(roots, options, &fetcher, lodi::util::now_utc()))
}

/// `lodi home import` (M-Import T-4, LD-341). What goes to standard output is either the report
/// of a written manifest or — with `--stdout` — the manifest itself, so that
/// `lodi home import --stdout > home.toml` is the same file (LD-325). It warns about nothing.
fn run_home_import(options: &lodi::home::import::Options) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    if let Err(d) = home_fence(&roots) {
        eprintln!("{d}");
        return ExitCode::from(lodi::diag::exit_status(d.code));
    }
    report_home_import(lodi::home::import::import(&roots, options))
}

fn run_home_import_source(
    options: &lodi::home::import::Options,
    selection: &lodi::home::source::Selection,
) -> ExitCode {
    let roots = match lodi::roots::Roots::from_env() {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    let roots = match lodi::home::source::target(roots, selection) {
        Ok(roots) => roots,
        Err(d) => {
            eprintln!("{d}");
            return ExitCode::from(lodi::diag::exit_status(d.code));
        }
    };
    report_home_import(lodi::home::import::import(&roots, options))
}

fn report_home_import(
    result: Result<lodi::home::import::Import, lodi::diag::Diagnostic>,
) -> ExitCode {
    match result {
        Ok(import) => {
            match &import.report {
                Some(lines) => {
                    for line in lines {
                        println!("{line}");
                    }
                }
                None => print!("{}", import.manifest),
            }
            ExitCode::SUCCESS
        }
        Err(d) => {
            eprintln!("{d}");
            ExitCode::from(lodi::diag::exit_status(d.code))
        }
    }
}

/// `lodi search` and `lodi info`: the notes on standard error, the report on standard output.
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

/// Every host verb runs through here, so the LD-376 guard is checked before any of them starts:
/// under `LODI_HOST_REQUIRE_ROOT=1` a verb without `--root` is refused before it reads anything.
fn run_host(
    verb: &str,
    options: &lodi::hostscope::Options,
    run: fn(&lodi::hostscope::Options) -> Result<String, lodi::hostscope::HostError>,
) -> ExitCode {
    if let Err(error) = lodi::hostscope::guard(verb, options) {
        eprintln!("{error}");
        return ExitCode::from(error.exit_status());
    }
    // `--host` beside a `SOURCE` that is itself a host is a usage error (LD-379): known only by
    // looking, so it is asked after the guard and before the verb.
    if let Some(usage) = lodi::hostscope::source::usage(options) {
        eprintln!("{NAME}: {usage}");
        return ExitCode::from(USAGE_ERROR);
    }
    let print = |report: &str| {
        for line in report.lines() {
            if line.starts_with("W_") {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    };
    // A fetched tree is never written: `pin`, `unpin` and `import` refuse a URL (LD-401).
    if let Err(error) = lodi::hostscope::remote::refuse_writer(verb, options) {
        let error = lodi::hostscope::HostError::from(error);
        eprintln!("{error}");
        return ExitCode::from(error.exit_status());
    }
    match run(options) {
        Ok(report) => {
            print(&report);
            ExitCode::SUCCESS
        }
        Err(error) => {
            print(&error.done);
            eprintln!("{error}");
            ExitCode::from(error.exit_status())
        }
    }
}

/// `lodi boot confirm`: the LD-376 guard, then the confirmation (bv-1, LD-425).
fn run_boot(options: &lodi::hostscope::Options) -> ExitCode {
    let guard = lodi::hostscope::require_root_for(
        "lodi boot confirm",
        options.root.as_deref(),
        std::env::var_os(lodi::hostscope::REQUIRE_ROOT_VAR).as_deref(),
    );
    match guard
        .map_err(lodi::hostscope::HostError::from)
        .and_then(|()| lodi::hostscope::trial::confirm(options))
    {
        Ok(report) => {
            print!("{report}");
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::from(error.exit_status())
        }
    }
}

/// `lodi plan|apply|import|update` (DONE D2, LD-416): the LD-376 guard first, then the
/// repository LD-447 resolves, then the host verb on it — `plan` and `apply` over the host and
/// every declared home, `import` into `<repository>/<hostname>/`, `update` of the root lock.
fn run_top(verb: Top, mut options: lodi::hostscope::Options, yes: bool) -> ExitCode {
    let word = verb.word();
    let fail = |error: lodi::hostscope::HostError| {
        eprintln!("{error}");
        ExitCode::from(error.exit_status())
    };
    if let Err(error) = lodi::hostscope::require_root_for(
        &format!("lodi {word}"),
        options.root.as_deref(),
        std::env::var_os(lodi::hostscope::REQUIRE_ROOT_VAR).as_deref(),
    ) {
        return fail(error.into());
    }
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(e) => {
            let d = lodi::diag::Diagnostic::new(
                "E_CONFIG",
                format!("the current directory cannot be read: {e}"),
            );
            return fail(d.into());
        }
    };
    let resolved = lodi::repo::resolve(
        word,
        options.source.as_deref().map(std::path::Path::as_os_str),
        &cwd,
        std::env::var_os(lodi::repo::ENV).as_deref(),
        yes,
        &mut lodi::repo::Terminal,
    );
    match resolved {
        Ok(repository) => options.source = Some(repository.source),
        Err(d) => return fail(d.into()),
    }
    if let Some(usage) = lodi::hostscope::source::usage(&options) {
        eprintln!("{NAME}: {usage}");
        return ExitCode::from(USAGE_ERROR);
    }
    let host_verb = match verb {
        Top::Import => "import",
        Top::Update => "pin",
        Top::Plan => "plan",
        Top::Apply => "apply",
    };
    if let Err(error) = lodi::hostscope::remote::refuse_writer(host_verb, &options) {
        return fail(lodi::hostscope::HostError::from(error));
    }
    let run: fn(&lodi::hostscope::Options) -> Result<String, lodi::hostscope::HostError> =
        match verb {
            Top::Plan => lodi::hostscope::flake::plan,
            Top::Apply => lodi::hostscope::flake::apply,
            Top::Import => lodi::hostscope::run_import,
            Top::Update => lodi::hostscope::pin::verbs::update,
        };
    let print = |report: &str| {
        for line in report.lines() {
            if line.starts_with("W_") {
                eprintln!("{line}");
            } else {
                println!("{line}");
            }
        }
    };
    match run(&options) {
        Ok(report) => {
            print(&report);
            ExitCode::SUCCESS
        }
        Err(mut error) => {
            print(&error.done);
            // The host verbs' hints name `lodi host …`; this invocation was the top-level verb.
            for d in &mut error.diagnostics {
                for note in &mut d.notes {
                    *note = note.replace(&format!("lodi host {word}"), &format!("lodi {word}"));
                }
            }
            fail(error)
        }
    }
}

/// The command a first guess meant (LD-382). `lodi setup`, `lodi init host` and `lodi init
/// home` stay usage errors (`lodi import` is a command since LD-416) — the grammar is scope first, and `lodi init` takes no scope
/// word — but the refusal names the command that does what was asked. `arg` is what the parser
/// refused, so `lodi init --force home` is `init home` too.
fn pointer(arg: &str) -> Option<String> {
    let text = match arg {
        "setup" => format!(
            "start with the scope you want: this machine, `sudo {NAME} host arm` then \
             `sudo {NAME} host import`; your home, `{NAME} home init`; a project here, `{NAME} init`"
        ),
        "init host" => format!(
            "`{NAME} init` starts a project and takes no scope word: for this machine run \
             `sudo {NAME} host arm`, then `sudo {NAME} host import`"
        ),
        "init home" => format!(
            "`{NAME} init` starts a project and takes no scope word: for your home run \
             `{NAME} home init`"
        ),
        _ => return None,
    };
    Some(text)
}

fn main() -> ExitCode {
    let args: Vec<OsString> = std::env::args_os().skip(1).collect();
    // The one deprecation point of the dispatch (M-1.0 T-8). Every surface this invocation typed
    // is looked up in the committed table — empty at 1.0, so this prints nothing — and an entry
    // prints its `W_DEPRECATED` line on standard error. A warning never changes the exit status,
    // so nothing below this loop knows it ran.
    for surface in lodi::surface::typed(&args) {
        if let Some(warning) = lodi::surface::deprecation(lodi::surface::DEPRECATIONS, &surface) {
            eprintln!("{warning}");
        }
    }
    let request = parse(args);
    // A home verb under `sudo` acts on whichever home the environment names, which is not the
    // home of the person who typed it (LD-382). One line says so; nothing else changes.
    if matches!(
        request,
        Request::HomePlan
            | Request::HomePlanSource(_)
            | Request::HomeApply(_)
            | Request::HomeApplySource(_, _)
            | Request::HomeStatus
            | Request::HomeStatusSource(_)
            | Request::HomeImport(_)
            | Request::HomeImportSource(_, _)
    ) {
        if let Some(warning) = lodi::home::sudo_warning_here() {
            eprintln!("{warning}");
        }
    }
    // A verb that names no repository takes LD-447's: its `recipes/` folder, if any, is read
    // once here, before anything resolves (LD-409). A verb with a `SOURCE` reads that one's.
    let reader = match &request {
        Request::Shell { .. } => Some("shell"),
        Request::Develop { .. } => Some("develop"),
        Request::Run { .. } => Some("run"),
        Request::Search { .. } => Some("search"),
        Request::Info(_) => Some("info"),
        Request::Lock | Request::LockCheck => Some("lock"),
        Request::HomePlan | Request::HomeApply(_) | Request::HomeStatus => Some("home"),
        _ => None,
    };
    if let Some(verb) = reader {
        lodi::catalogue::user::install(lodi::catalogue::user::from_environment(verb));
    }
    match request {
        Request::Shell { no_nest, request } => run_shell(&request, no_nest),
        Request::Develop { no_nest, argv } => run_entry(Action::Develop(argv), no_nest),
        Request::Run {
            no_nest,
            task,
            args,
        } => run_entry(Action::Run { task, args }, no_nest),
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
        Request::Info(None) => report_browse(lodi::search::project(Path::new("."))),
        Request::Info(Some(name)) => report_browse(lodi::search::tool(Path::new("."), &name)),
        Request::HomePlan => run_home_plan(),
        Request::HomePlanSource(selection) => run_home_plan_source(&selection),
        Request::HomeImport(options) => run_home_import(&options),
        Request::HomeImportSource(options, selection) => {
            run_home_import_source(&options, &selection)
        }
        Request::HomeApply(options) => run_home_apply(&options),
        Request::HomeApplySource(options, selection) => run_home_apply_source(&options, &selection),
        Request::HomeStatus => run_home(lodi::home::apply::status),
        Request::HomeStatusSource(selection) => run_home_status_source(&selection),
        Request::Top(verb, options, yes) => run_top(verb, options, yes),
        Request::HostPlan(options) => run_host("plan", &options, lodi::hostscope::plan),
        Request::HostApply(options) => run_host("apply", &options, lodi::hostscope::apply),
        Request::HostImport(options) => run_host("import", &options, lodi::hostscope::run_import),
        Request::HostArm(options) => run_host("arm", &options, lodi::hostscope::arm::run),
        Request::BootConfirm(options) => run_boot(&options),
        Request::HostVersions(options) => {
            run_host("versions", &options, lodi::hostscope::pin::verbs::versions)
        }
        Request::HostPin(options) => run_host("pin", &options, lodi::hostscope::pin::verbs::pin),
        Request::HostUnpin(options) => {
            run_host("unpin", &options, lodi::hostscope::pin::verbs::unpin)
        }
        Request::Trust { revoke } => {
            report(host::trust_command(Path::new("."), revoke).map(|text| {
                println!("{text}");
                0
            }))
        }
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
        Request::Lock => run_lock(false),
        Request::LockCheck => run_lock(true),
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
        Request::Unsupported(arg) => {
            eprintln!("{NAME}: unsupported command or argument '{arg}'");
            if let Some(pointer) = pointer(&arg) {
                eprintln!("   = hint: {pointer}");
            }
            eprintln!(
                "This build implements only --version, --help, help, init, lock, lock --check, \
                 shell ITEM..., develop [-- COMMAND], run TASK, search QUERY, info [TOOL], \
                 plan, apply, import, update, home init, home plan, home apply, home status, \
                 home import, host plan, host apply, host import, host arm, gc and trust."
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

    /// LD-360: `-h`, `help`, `--help` anywhere among lodi's own arguments and a bare scope are
    /// help requests, with the subject the other words name and every further word ignored.
    #[test]
    fn help_is_asked_for_at_every_level() {
        for (args, subject) in [
            (&["-h"][..], &[][..]),
            (&["help"], &[]),
            (&["help", "--help"], &[]),
            (&["--help", "--frobnicate"], &[]),
            (&["--version", "-h"], &[]),
            (&["home"], &["home"]),
            (&["host"], &["host"]),
            (&["home", "--help"], &["home"]),
            (&["host", "-h"], &["host"]),
            (&["host", "--root", "scratch", "--help"], &["host"]),
            (&["-h", "home"], &["home"]),
            (&["help", "home"], &["home"]),
            (&["help", "host", "arm"], &["host", "arm"]),
            (&["home", "plan", "--help"], &["home", "plan"]),
            (
                &["host", "apply", "--root", "scratch", "-h"],
                &["host", "apply"],
            ),
            (
                &["home", "import", "--out", "dir", "--help"],
                &["home", "import"],
            ),
            (&["init", "--help"], &["init"]),
            (&["init", "--name", "x", "-h"], &["init"]),
            (&["help", "init", "extra"], &["init"]),
            (&["lock", "--check", "--help"], &["lock"]),
            (&["shell", "python", "-h"], &["shell"]),
            (&["develop", "--help", "--", "true"], &["develop"]),
            (&["run", "--no-nest", "--help"], &["run"]),
            (&["gc", "--help"], &["gc"]),
            (&["search", "ython", "--help"], &["search"]),
            (&["info", "--help"], &["info"]),
            (&["trust", "-h"], &["trust"]),
            (&["help", "help"], &["help"]),
        ] {
            assert_eq!(parse_strs(args), help_of(subject), "{args:?}");
        }
    }

    /// What lodi hands on to a child is not its own, so these are exactly what they were: a
    /// task's arguments, a command after `--`, and a scope with anything but a help flag.
    #[test]
    fn a_childs_arguments_are_never_a_help_request() {
        assert_eq!(
            parse_strs(&["run", "build", "--help"]),
            Request::Run {
                no_nest: false,
                task: "build".into(),
                args: vec!["--help".into()],
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--", "ls", "-h"]),
            Request::Develop {
                no_nest: false,
                argv: vec!["ls".into(), "-h".into()],
            }
        );
        assert_eq!(
            parse_strs(&["shell", "python", "--", "--help"]),
            Request::Unsupported("shell --".into())
        );
        assert_eq!(
            parse_strs(&["home", "--"]),
            Request::Unsupported("home --".into())
        );
    }

    /// A help request about nothing that exists is refused: `lodi help` names what it could not
    /// find, and a `--help` beside an unknown command is that command's usage error unchanged.
    #[test]
    fn help_about_nothing_is_refused() {
        for (args, refused) in [
            (&["help", "frobnicate"][..], "help frobnicate"),
            (&["help", "home", "frobnicate"], "help home frobnicate"),
            (&["frobnicate", "--help"], "frobnicate"),
            (&["home", "frobnicate", "-h"], "home frobnicate"),
            (&["host", "status", "--help"], "host status"),
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
    /// it exactly once in its groups, each command and scope has its own help, and every
    /// subject the table names is one a help request resolves to.
    #[test]
    fn the_help_lists_every_command_of_the_table_once() {
        let mut listed: Vec<String> = HELP_GROUPS
            .iter()
            .flat_map(|(_, scope, words)| words.iter().map(move |w| format!("{scope}{w}")))
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
        for scope in ["home", "host"] {
            let help = help_for(&[scope.to_string()]);
            let usage = help.split("\n\nExamples:").next().unwrap();
            let verbs = COMMANDS
                .iter()
                .filter(|c| c.key.starts_with(&format!("{scope} ")));
            assert_eq!(
                usage.matches(&format!("\n  {NAME} {scope} ")).count(),
                verbs.count(),
                "{scope}"
            );
        }
        assert!(!help_for(&["home".into()]).contains("lodi host"));
        assert_eq!(help_subject(&["host", "status"]), None);
        assert_eq!(help_subject(&["frobnicate"]), None);
    }

    /// U13, as LD-380 left it: the whole text opens with the three scopes, whose first command
    /// is the README quick start's (`sudo lodi host arm`), a bare `lodi` prints exactly those
    /// six lines, and the examples and the command groups follow (LD-463).
    #[test]
    fn help_opens_with_where_to_start_and_groups_the_commands() {
        let whole = help_text();
        let lines: Vec<&str> = whole.lines().collect();
        assert!(lines[2].starts_with("Three independent scopes."), "{whole}");
        assert!(lines[3].contains(" sudo lodi host arm "), "{whole}");
        assert_eq!(lines[8], "", "the opening is six lines");
        assert_eq!(orientation(), lines[2..8].join("\n"));
        let headers: Vec<&str> = lines[9..]
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

    #[test]
    fn lock_and_lock_check() {
        assert_eq!(parse_strs(&["lock"]), Request::Lock);
        assert_eq!(parse_strs(&["lock", "--check"]), Request::LockCheck);
        assert_eq!(
            parse_strs(&["lock", "--frozen"]),
            Request::Unsupported("lock --frozen".to_string())
        );
        assert_eq!(
            parse_strs(&["lock", "--check", "x"]),
            Request::Unsupported("lock --check".to_string())
        );
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
    fn info_takes_nothing_or_one_name() {
        assert_eq!(parse_strs(&["info"]), Request::Info(None));
        assert_eq!(
            parse_strs(&["info", "python"]),
            Request::Info(Some("python".into()))
        );
        for args in [&["info", "--json"][..], &["info", "a", "b"]] {
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

    #[test]
    fn develop_is_interactive_without_a_command_and_runs_one_after_a_double_dash() {
        assert_eq!(
            parse_strs(&["develop", "--", "make", "-j", "4"]),
            Request::Develop {
                no_nest: false,
                argv: ["make", "-j", "4"].map(OsString::from).to_vec()
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--no-nest", "--", "--", ""]),
            Request::Develop {
                no_nest: true,
                argv: ["--", ""].map(OsString::from).to_vec()
            }
        );
        // No `-- COMMAND` at all is the interactive entry (LD-48), with or without --no-nest.
        assert_eq!(
            parse_strs(&["develop"]),
            Request::Develop {
                no_nest: false,
                argv: Vec::new()
            }
        );
        assert_eq!(
            parse_strs(&["develop", "--no-nest"]),
            Request::Develop {
                no_nest: true,
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
                no_nest: false,
                task: "build".into(),
                args: ["--release", "a b"].map(OsString::from).to_vec()
            }
        );
        assert_eq!(
            parse_strs(&["run", "--no-nest", "t"]),
            Request::Run {
                no_nest: true,
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
    fn trust_and_revoke() {
        assert_eq!(parse_strs(&["trust"]), Request::Trust { revoke: false });
        assert_eq!(
            parse_strs(&["trust", "--revoke"]),
            Request::Trust { revoke: true }
        );
        assert!(matches!(
            parse_strs(&["trust", "--list"]),
            Request::Unsupported(_)
        ));
    }

    #[test]
    fn host_pin_verbs_take_one_name_or_all_and_the_documented_flags() {
        let with = |name: Option<&str>, to: Option<&str>, all: bool| lodi::hostscope::Options {
            root: Some("/scratch".into()),
            source: Some("/hosts".into()),
            host: Some("box".into()),
            name: name.map(str::to_string),
            to: to.map(str::to_string),
            all,
            ..Default::default()
        };
        assert_eq!(
            parse_strs(&[
                "host", "versions", "curl", "/hosts", "--host", "box", "--root", "/scratch"
            ]),
            Request::HostVersions(with(Some("curl"), None, false))
        );
        assert_eq!(
            parse_strs(&[
                "host", "pin", "--to", "8.5.0-2", "curl", "--root", "/scratch", "/hosts", "--host",
                "box"
            ]),
            Request::HostPin(with(Some("curl"), Some("8.5.0-2"), false))
        );
        assert_eq!(
            parse_strs(&[
                "host", "pin", "/hosts", "--all", "--host", "box", "--root", "/scratch"
            ]),
            Request::HostPin(with(None, None, true))
        );
        assert_eq!(
            parse_strs(&[
                "host", "unpin", "--all", "/hosts", "--host", "box", "--root", "/scratch"
            ]),
            Request::HostUnpin(with(None, None, true))
        );
        assert_eq!(
            parse_strs(&["host", "unpin", "curl"]),
            Request::HostUnpin(lodi::hostscope::Options {
                name: Some("curl".into()),
                ..Default::default()
            })
        );
        for bad in [
            &["host", "versions"][..],
            &["host", "versions", "--all"],
            &["host", "versions", "curl", "--to", "1.0"],
            &["host", "pin"],
            &["host", "pin", "--to", "2026-09-01"],
            &["host", "pin", "curl", "--to"],
            &["host", "pin", "curl", "--to", "a", "--to", "b"],
            &["host", "pin", "--all", "--all"],
            &["host", "pin", "curl", "/hosts", "extra"],
            &["host", "pin", "Not/A/Name"],
            &["host", "pin", "curl", "--host", "box"],
            &["host", "pin", "curl", "--dry-run"],
            &["host", "unpin", "curl", "--to", "2026-09-01"],
            &["host", "unpin"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?} must be refused"
            );
        }
    }

    #[test]
    fn host_has_exactly_plan_apply_import_arm_and_the_documented_flags() {
        assert_eq!(
            parse_strs(&["host", "plan", "--root", "/scratch"]),
            Request::HostPlan(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                ..Default::default()
            })
        );
        assert_eq!(
            parse_strs(&[
                "host",
                "apply",
                "--no-update",
                "--overwrite-drift",
                "--unsupported-partial-upgrade",
                "--resolved",
                "journal-id",
                "--root",
                "/scratch",
            ]),
            Request::HostApply(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                resolved: Some("journal-id".into()),
                overwrite_drift: true,
                no_update: true,
                unsupported_partial_upgrade: true,
                ..Default::default()
            })
        );
        assert_eq!(
            parse_strs(&[
                "host", "import", "--root", "/scratch", "--out", "/out", "--force"
            ]),
            Request::HostImport(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                out: Some("/out".into()),
                force: true,
                ..Default::default()
            })
        );
        assert_eq!(
            parse_strs(&["host", "import", "--root", "/scratch"]),
            Request::HostImport(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                ..Default::default()
            })
        );
        // `--stdout` prints the manifest and names no destination (LD-325).
        assert_eq!(
            parse_strs(&["host", "import", "--stdout", "--root", "/scratch"]),
            Request::HostImport(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                stdout: true,
                ..Default::default()
            })
        );
        // The positional `SOURCE` and `--host` of `plan`, `apply` and `import` (LD-379), in any
        // order among the flags.
        for verb in ["plan", "apply", "import"] {
            let options = lodi::hostscope::Options {
                root: Some("/scratch".into()),
                source: Some("/hosts".into()),
                host: Some("box".into()),
                ..Default::default()
            };
            let parsed = parse_strs(&[
                "host", verb, "/hosts", "--host", "box", "--root", "/scratch",
            ]);
            let expected = match verb {
                "plan" => Request::HostPlan(options.clone()),
                "apply" => Request::HostApply(options.clone()),
                _ => Request::HostImport(options.clone()),
            };
            assert_eq!(parsed, expected, "{verb}");
            let reordered = parse_strs(&[
                "host", verb, "--root", "/scratch", "--host", "box", "/hosts",
            ]);
            assert_eq!(reordered, expected, "{verb}");
        }
        // `arm` takes `--root` and nothing else (LD-320).
        assert_eq!(
            parse_strs(&["host", "arm", "--root", "/scratch"]),
            Request::HostArm(lodi::hostscope::Options {
                root: Some("/scratch".into()),
                ..Default::default()
            })
        );
        // A bare `host` is the scope's help since LD-360, not a usage error.
        for bad in [
            &["host", "status"][..],
            &["host", "plan", "--json"],
            &["host", "apply", "--root"],
            &["host", "apply", "--resolved"],
            &["host", "plan", "--no-update"],
            // The mode has exactly one door, and a plan is not it.
            &["host", "plan", "--unsupported-partial-upgrade"],
            &[
                "host",
                "apply",
                "--unsupported-partial-upgrade",
                "--unsupported-partial-upgrade",
            ],
            // One `SOURCE` at most (LD-379); `--host` needs one, takes a value and is given
            // once; a `SOURCE` is where an import writes, so neither `--out` nor `--stdout`
            // may stand beside it.
            &["host", "apply", "a", "b"],
            &["host", "import", "a", "b"],
            &["host", "plan", "--host", "box"],
            &["host", "plan", "/hosts", "--host"],
            &["host", "plan", "/hosts", "--host", "--root"],
            &["host", "plan", "/hosts", "--host", "a", "--host", "b"],
            &["host", "import", "/hosts", "--out", "/a"],
            &["host", "import", "/hosts", "--stdout"],
            &["host", "arm", "--host", "box", "--root", "/scratch"],
            // `--out` and `--force` belong to `import` and to nothing else, and no flag may be
            // given twice.
            &["host", "import", "--out"],
            &["host", "import", "--json"],
            &["host", "import", "--out", "/a", "--out", "/b"],
            &["host", "import", "--force", "--force"],
            &["host", "import", "--stdout", "--stdout"],
            &["host", "import", "--stdout", "--out", "/a"],
            &["host", "import", "--out", "/a", "--stdout"],
            &["host", "import", "--stdout", "--force"],
            &["host", "plan", "--stdout"],
            &["host", "arm", "--stdout", "--root", "/scratch"],
            &["host", "plan", "--out", "/a"],
            &["host", "apply", "--force"],
            &["host", "arm", "extra", "--root", "/scratch"],
            &["host", "arm", "--force", "--root", "/scratch"],
            &["host", "arm", "--out", "/a", "--root", "/scratch"],
            &["host", "arm", "--no-update", "--root", "/scratch"],
            &["host", "arm", "--root", "/a", "--root", "/b"],
            &["host", "disarm", "--root", "/scratch"],
        ] {
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
        assert_eq!(version_text(), "lodi 1.12.2");
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

    #[test]
    fn help_names_only_implemented_commands() {
        let mut help = help_text();
        for command in COMMANDS {
            help.push_str(&command_help(command));
        }
        assert!(help.contains("--version") && help.contains("--help"));
        assert!(help.contains("lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]"));
        assert!(help.contains("lodi lock ") && help.contains("lodi lock --check"));
        assert!(help.contains("lodi develop [--no-nest] [-- COMMAND"));
        assert!(help.contains("lodi shell [--no-nest] TOOL[@CONSTRAINT]..."));
        assert!(help.contains("lodi shell [--no-nest] --base DISTRO[:RELEASE] PACKAGE..."));
        assert!(help.contains("lodi run [--no-nest] TASK") && help.contains("lodi trust"));
        assert!(help.contains("lodi gc [--dry-run] [--keep-days N] [--images] [-v]"));
        assert!(help.contains("lodi host plan [--root DIR]"));
        assert!(help.contains("lodi host apply [--root DIR]"));
        assert!(help.contains(
            "lodi host import [--root DIR] [--out DIR | --stdout] [--force] [--dry-run]"
        ));
        assert!(help.contains("lodi home import [--out DIR | --stdout] [--force]"));
        assert!(!help.contains("lodi host status"));
        // The top-level verbs of a repository are commands since LD-416 (DONE D2).
        for verb in ["import", "plan", "apply", "update"] {
            assert!(help.contains(&format!(
                "lodi {verb} [SOURCE] [--host NAME] [--root DIR] [--yes]"
            )));
        }
        let usage = format!("{NAME} build");
        assert!(!help.contains(&usage), "help offers {usage}");
    }

    /// DONE D2 (LD-416): the four top-level verbs take one SOURCE, --host, --root and --yes in any
    /// order; a URL's selectors only on plan and apply, only with a URL, and never --rev with
    /// --refresh.
    #[test]
    fn the_top_level_verbs_take_their_flags_and_refuse_the_rest() {
        let Request::Top(Top::Update, options, true) =
            parse_strs(&["update", "--yes", "--host", "box", "repo", "--root", "r"])
        else {
            panic!("update did not parse");
        };
        assert_eq!(options.source.as_deref(), Some(Path::new("repo")));
        assert_eq!(options.host.as_deref(), Some("box"));
        assert!(matches!(
            parse_strs(&["apply", "github:o/r", "--refresh"]),
            Request::Top(Top::Apply, _, false)
        ));
        for bad in [
            &["apply", "a", "b"][..],
            &["import", "--refresh"],
            &["update", "github:o/r", "--ref", "main"],
            &["plan", "--ref", "main"],
            &["apply", "github:o/r", "--rev", "x", "--refresh"],
            &["apply", "--yes", "--yes"],
            &["apply", "--no-home"],
        ] {
            assert!(
                matches!(parse_strs(bad), Request::Unsupported(_)),
                "{bad:?}"
            );
        }
    }
}
