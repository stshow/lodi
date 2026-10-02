//! Runs the built `lodi` binary and checks its observable behaviour.

mod support;

/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

fn lodi(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .output()
        .expect("the lodi binary runs")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

#[test]
fn version_prints_name_and_version() {
    let output = lodi(&["--version"]);
    assert!(output.status.success());
    assert_eq!(stdout(&output), "lodi 2.0.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn help_succeeds_and_advertises_only_working_commands() {
    let output = lodi(&["--help"]);
    assert!(output.status.success());
    let whole = stdout(&output);
    // M-1.0 T-10a: the first line is the release banner, with no development-build suffix.
    assert_eq!(
        whole.lines().next(),
        Some(
            "lodi 2.0.0 \u{2014} environment and system manager for Ubuntu, Debian, Arch and Fedora"
        )
    );
    // Since LD-463 each command's usage and details are in its own help: read them all.
    let mut text = whole.clone();
    for subject in help_subjects(&whole) {
        let words: Vec<&str> = subject.split(' ').collect();
        let output = lodi(&[&["help"], &words[..]].concat());
        assert!(output.status.success(), "{subject}");
        text.push_str(&stdout(&output));
    }
    assert!(!text.contains("M-Spike"));
    assert!(text.contains("--version") && text.contains("--help"));
    assert!(text.contains("lodi init [--name NAME] [--base DISTRO[:RELEASE]] [--force]"));
    assert!(text.contains("lodi develop [--no-nest] [--trust] [-- COMMAND"));
    assert!(text.contains("lodi shell [--no-nest] TOOL[@CONSTRAINT]..."));
    assert!(text.contains("lodi shell [--no-nest] --base DISTRO[:RELEASE] PACKAGE..."));
    // M-Arch-Base T-6: both `--base` surfaces name the bases this build writes, in one order
    // (fd-1 added fedora:44), and the trust model is stated where a keyring would otherwise be
    // assumed.
    assert_eq!(
        text.matches("arch:rolling, debian:bookworm, fedora:44 or ubuntu:noble")
            .count(),
        2,
        "{text}"
    );
    assert!(text.contains("no keyring and no signature check"), "{text}");
    assert!(
        text.contains("as it is for debian and ubuntu"),
        "the trust model is one statement for the three bases: {text}"
    );
    assert!(text.contains("lodi run [--no-nest] [--trust] TASK"));
    // LD-496: develop and run lock and ask by themselves, and search shows a recipe's details.
    for removed in ["lodi lock", "lodi trust", "lodi info"] {
        assert!(!text.contains(removed), "help names {removed}");
    }
    assert!(text.contains("lodi gc [--dry-run] [--keep-days N] [--images] [-v]"));
    // The 1.x host and home verbs went in 2.0 (#699): `lodi switch --home` is the home's.
    assert!(!text.contains("lodi home"), "help names a home verb");
    assert!(!text.contains("lodi host"), "help names a host verb");
    assert!(text.contains("lodi switch --home"));
    // M-0.4 T-6: the browse command, and no `--json` and no `lodi registry` beside it.
    assert!(text.contains("lodi search [--registry | --distro] QUERY"));
    for absent in ["--json", "lodi registry", "lodi doctor"] {
        assert!(!text.contains(absent), "help advertises {absent}");
    }
    assert!(text.contains("localhost/lodi-env:<32 hex>"));
    assert!(text.contains("rootless Podman container for a [container] manifest"));
    assert!(text.contains("lodi never installs Podman"));
    assert!(!text.contains("lodi check"), "help advertises lodi check");
    // The host and home commands (#700).
    assert!(
        text.contains("lodi import [PATH] [--home]"),
        "help lacks lodi import"
    );
    // `lodi plan` and `lodi apply` went in 2.0 (#699).
    for verb in ["plan", "apply"] {
        assert!(
            !text.contains(&format!("lodi {verb} ")),
            "help names lodi {verb}"
        );
    }
    for usage in [
        "lodi update [PATH]",
        "lodi pin PKG [--to V]",
        "lodi pin --all [--to DATE]",
        "lodi unpin PKG",
        "lodi unpin --all",
    ] {
        assert!(text.contains(usage), "help lacks {usage}");
    }
}

#[test]
fn unsupported_commands_fail_with_usage_status() {
    // A truly unknown command and the planned-but-absent ones. `init` is implemented since
    // M-0.3 T-1 and `shell` since T-2, so neither is one of these any more.
    for command in ["frobnicate", "build", "--frobnicate"] {
        let output = lodi(&[command]);
        assert_eq!(output.status.code(), Some(2), "{command}");
        assert!(output.stdout.is_empty(), "{command}");
        assert!(stderr(&output).contains("unsupported"), "{command}");
    }
}

#[test]
fn unimplemented_forms_of_develop_run_and_trust_fail_with_usage_status() {
    for args in [
        // `develop` with no `-- COMMAND` is the interactive entry since M-0.3 T-2 (LD-48);
        // `develop --` with nothing after it is still a usage error.
        &["develop", "--"][..],
        &["develop", "make"],
        // `lodi shell` needs at least one name, and `--base` needs a package after it. Both
        // are refused before anything is resolved, so both are offline.
        &["shell"],
        &["shell", "--base"],
        &["shell", "--base", "debian:bookworm"],
        &["shell", "--no-nest"],
        &["shell", "python", "--", "true"],
        &["run"],
        &["run", "--list"],
        // `lodi gc` takes four flags and nothing else (design call D9, LD-54): every other
        // flag `spec/03` §7 names is refused here rather than silently ignored, and so is a
        // `--keep-days` that is not a whole number of days. None of these reaches the store.
        &["gc", "--older-than", "7d"],
        &["gc", "--aggressive"],
        &["gc", "--images=all"],
        &["gc", "--dry-run", "--dry-run"],
        &["gc", "--keep-days"],
        &["gc", "--keep-days", "-1"],
        &["gc", "--keep-days", "1.5"],
        &["gc", "--keep-days", "week"],
        &["gc", "store"],
        // `lodi lock`, `lodi trust` and `lodi info` are gone (LD-496); since #699 each stops
        // naming its replacement, which `tests/old_names.rs` holds.
        &["develop", "--trust", "--trust"],
        &["develop", "--trust", "true"],
        &["run", "--trust"],
        &["run", "--no-nest", "--trust", "--no-nest", "build"],
        &["init", "--name"],
        // M-0.4 T-6: `lodi search` needs exactly one query and at most one scope, and there is
        // no `--json` in 0.4 (design call D14).
        &["search"],
        &["search", "--registry"],
        &["search", "--registry", "--distro", "python"],
        &["search", "--json", "python"],
        &["search", "python", "nodejs"],
        &["search", "--frobnicate", "python"],
    ] {
        let output = lodi(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(stderr(&output).contains("unsupported"), "{args:?}");
    }
}

#[test]
fn develop_and_run_without_a_manifest_are_manifest_errors() {
    let empty = support::scratch("cli-nomanifest");
    for args in [
        &["develop", "--", "true"][..],
        // The interactive entry needs a manifest too, and says so before it opens a shell.
        &["develop"],
        &["run", "build"],
        &["develop", "--trust", "--", "true"],
        &["run", "--trust", "--no-nest", "build"],
        // A task's arguments and a command after `--` are the child's, never a help request
        // (LD-360): these reach the manifest exactly as they did.
        &["run", "build", "--help"],
        &["develop", "--", "true", "-h"],
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .current_dir(&empty)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        assert!(stderr(&output).contains("E_NO_MANIFEST"), "{args:?}");
    }
}

#[test]
fn undocumented_aliases_fail_with_usage_status() {
    // `-h` and `help` are help requests since LD-360; `-V` is still no alias of `--version`.
    let output = lodi(&["-V"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    let text = stderr(&output);
    assert!(
        text.contains("unsupported") && text.contains("'-V'"),
        "{text}"
    );
}

/// `lodi gc` with no store at all: the one thing it can say, exit 0, and not a byte written.
/// This is also where "never run `podman` when $LODI_HOME holds no `img-` record" is cheapest
/// to see: `PATH` is emptied, so a `podman` anywhere would fail loudly instead of being found.
#[test]
fn gc_on_a_store_that_is_not_there_collects_nothing_and_succeeds() {
    // A path in a fresh scratch directory that nothing makes.
    let home = support::scratch("cli-gc").join("home");
    for args in [&["gc"][..], &["gc", "--dry-run"], &["gc", "--images", "-v"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .env("LODI_HOME", &home)
            .env("PATH", "")
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?} {}",
            stderr(&output)
        );
        assert_eq!(stdout(&output), "nothing to collect\n", "{args:?}");
        assert!(!home.exists(), "{args:?} created {}", home.display());
    }
}

/// The opening of `lodi --help`: the lines after the banner and its blank line, up to the next
/// blank line.
fn opening(help: &str) -> Vec<&str> {
    help.lines().skip(2).take_while(|l| !l.is_empty()).collect()
}

/// A bare `lodi` orients (LD-380): the opening of `lodi --help`, on standard output, at exit 0.
#[test]
fn no_arguments_prints_the_opening_of_the_help() {
    let output = lodi(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let help = stdout(&lodi(&["--help"]));
    let opening = opening(&help);
    assert!(opening.len() >= 3, "{help}");
    assert_eq!(stdout(&output), format!("{}\n", opening.join("\n")));
}

// ------------------------------------------------------------ a project in a scratch home ---

/// A scratch project directory with its own store, trust store and home, all under the target
/// directory; nothing of the developer's home is read or written.
struct Project {
    root: PathBuf,
    dir: PathBuf,
}

impl Project {
    fn new(tag: &str, manifest: &str) -> Project {
        let root = support::scratch(tag);
        let dir = root.join("project");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("lodi.toml"), manifest).unwrap();
        Project { root, dir }
    }

    /// `lodi ARGS` in the project, standard input `stdin`.
    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
        command
            .args(args)
            .current_dir(&self.dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.root.join("home"))
            .env("XDG_CONFIG_HOME", self.root.join("config"))
            .env("XDG_DATA_HOME", self.root.join("data"))
            .env("XDG_STATE_HOME", self.root.join("state"))
            .env("XDG_CACHE_HOME", self.root.join("cache"))
            .env("LODI_HOME", self.root.join("lodi"))
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.command(args).output().expect("the lodi binary runs")
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // The store's entries are sealed read-only; their modes are opened before removal.
        support::make_writable(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

const EMPTY: &str = "[project]\nname = \"empty\"\n";
const TASKS: &str = "[project]\nname = \"tasks\"\n\n[tasks.hi]\nrun = \"echo hi\"\n\n\
                     [tasks.also]\nrun = \"echo also\"\n";

/// A command run in the environment that does nothing, on any machine.
const NOTHING: &[&str] = &["develop", "--", "/bin/sh", "-c", ":"];

/// A lock with nothing to resolve says so on standard error, with no parenthesis around an
/// empty list of what was resolved; the second run finds it fresh and says nothing of it.
#[test]
fn a_lock_with_nothing_to_resolve_names_no_empty_parenthesis() {
    let project = Project::new("cli-nothing-to-lock", EMPTY);
    let wrote = project.run(NOTHING);
    assert_eq!(wrote.status.code(), Some(0), "{}", stderr(&wrote));
    assert!(wrote.stdout.is_empty(), "{}", stdout(&wrote));
    assert!(
        stderr(&wrote).starts_with("lodi: wrote lodi.lock: nothing to lock\n"),
        "{}",
        stderr(&wrote)
    );
    assert!(project.path("lodi.lock").is_file());
    let again = project.run(NOTHING);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(!stderr(&again).contains("lodi.lock"), "{}", stderr(&again));
}

/// `lodi run` with a name the manifest has no task for is a usage error the moment the manifest
/// is read: before the trust gate and before anything is locked, in the `lodi: error` shape,
/// with the tasks there are as its note.
#[test]
fn an_unknown_task_is_reported_before_the_trust_gate_and_the_lock() {
    let unknown = "lodi: error: lodi.toml has no task `nope`\n   = its tasks: also, hi\n";
    let project = Project::new("cli-unknown-task", TASKS);
    // No lock and no trust record: the name is still what is reported.
    let first = project.run(&["run", "nope"]);
    assert_eq!(first.status.code(), Some(2), "{}", stderr(&first));
    assert_eq!(stderr(&first), unknown);
    assert!(first.stdout.is_empty());
    assert!(!project.path("lodi.lock").exists());
    // `--trust` allows the text, and the unknown name is still the error, with nothing locked.
    let allowed = project.run(&["run", "--trust", "nope"]);
    assert_eq!(allowed.status.code(), Some(2), "{}", stderr(&allowed));
    assert_eq!(stderr(&allowed), unknown);
    assert!(!project.path("lodi.lock").exists());
    // A known task in the same project meets the trust gate next, so the order above is the
    // rule and not an accident of this project; still nothing is locked.
    let untrusted = project.run(&["run", "hi"]);
    assert_eq!(untrusted.status.code(), Some(11), "{}", stderr(&untrusted));
    assert!(
        stderr(&untrusted).contains("E_TRUST_REQUIRED"),
        "{}",
        stderr(&untrusted)
    );
    assert!(!project.path("lodi.lock").exists());
    // Allowed for one run, it locks and runs.
    let ran = project.run(&["run", "--trust", "hi"]);
    assert_eq!(ran.status.code(), Some(0), "{}", stderr(&ran));
    assert_eq!(stdout(&ran), "hi\n");
    assert!(project.path("lodi.lock").is_file());

    // A manifest with no tasks at all says that.
    let bare = Project::new("cli-no-tasks", EMPTY);
    let none = bare.run(&["run", "nope"]);
    assert_eq!(none.status.code(), Some(2), "{}", stderr(&none));
    assert_eq!(
        stderr(&none),
        "lodi: error: lodi.toml has no task `nope`\n   = it declares no tasks\n"
    );
}

/// Start `lodi develop -- COMMAND…` in a process group of its own, and wait until Lodi's child
/// is `comm` — the command has been executed, so Lodi is waiting on it with its signal handling
/// in place and nothing of a shell's own interrupt rules stands between the signal and it.
fn develop_until_running(project: &Project, command: &[&str], comm: &str) -> std::process::Child {
    use std::os::unix::process::CommandExt;
    let mut args = vec!["develop", "--"];
    args.extend(command);
    let child = project
        .command(&args)
        .process_group(0)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("the lodi binary starts");
    let children = format!("/proc/{0}/task/{0}/children", child.id());
    wait::until("the command in the environment to start", || {
        fs::read_to_string(&children)
            .unwrap_or_default()
            .split_whitespace()
            .any(|pid| {
                fs::read_to_string(format!("/proc/{pid}/comm")).is_ok_and(|c| c.trim() == comm)
            })
    });
    child
}

/// Signals during `lodi develop -- COMMAND`. A terminal's interrupt goes to the whole
/// foreground process group: the command is interrupted and Lodi reports it as 130, as it
/// reports a forwarded `SIGTERM` as 143. An interrupt sent to the Lodi process alone is not
/// forwarded — the command was not interrupted, and a terminal's interrupt would otherwise
/// reach it twice — so the command runs to its end and Lodi exits with the command's own status.
#[test]
fn an_interrupt_reports_the_status_of_the_command_it_reached() {
    let project = Project::new("cli-develop-signals", EMPTY);
    assert_eq!(project.run(NOTHING).status.code(), Some(0));

    // The process group, as a terminal delivers it.
    let mut group = develop_until_running(&project, &["sleep", "60"], "sleep");
    // SAFETY: a signal to the process group this test started.
    assert_eq!(unsafe { libc::kill(-(group.id() as i32), libc::SIGINT) }, 0);
    assert_eq!(group.wait().unwrap().code(), Some(130));

    // `SIGTERM` to Lodi alone is forwarded to the command.
    let mut term = develop_until_running(&project, &["sleep", "60"], "sleep");
    // SAFETY: a signal to the process this test started.
    assert_eq!(unsafe { libc::kill(term.id() as i32, libc::SIGTERM) }, 0);
    assert_eq!(term.wait().unwrap().code(), Some(143));

    // `SIGINT` to Lodi alone: the command finishes and its own status is the one reported.
    let mut alone = develop_until_running(&project, &["sh", "-c", "sleep 3; exit 7"], "sh");
    // SAFETY: a signal to the process this test started.
    assert_eq!(unsafe { libc::kill(alone.id() as i32, libc::SIGINT) }, 0);
    assert_eq!(alone.wait().unwrap().code(), Some(7));
}

/// M-1.0 T-8: no command and no flag is deprecated — a row of the committed table can only name
/// a manifest table — so the warning the dispatch itself can print appears nowhere a user reads:
/// not in `--help`, which is the inventory, and not on either stream of an ordinary invocation.
/// `tests/surface.rs` holds the mechanism behind it.
#[test]
fn no_command_is_deprecated_and_help_never_names_the_warning() {
    for args in [&["--help"][..], &["--version"], &["frobnicate"]] {
        let output = lodi(args);
        assert!(!stdout(&output).contains("W_DEPRECATED"), "{args:?}");
        assert!(!stderr(&output).contains("W_DEPRECATED"), "{args:?}");
    }
    assert!(
        lodi::surface::DEPRECATIONS
            .iter()
            .all(|d| d.manifest().is_some())
    );
}

/// Every command and scope `lodi --help` lists in its command groups, at both depths: `home`
/// and `home plan`, `lock`, `help`. A group line is a label, `lodi`, and a comma-separated list
/// whose first item carries the scope word its followers share; flags are not subjects.
fn help_subjects(whole: &str) -> Vec<String> {
    let groups = whole
        .split("\nCommands (")
        .nth(1)
        .expect("--help has a Commands header");
    let mut subjects = Vec::new();
    for line in groups.lines().skip(1).take_while(|l| l.starts_with("  ")) {
        let list = line.split_once(" lodi ").expect("a group lists commands").1;
        let mut scope = String::new();
        for (at, item) in list.split(", ").enumerate() {
            let words: Vec<&str> = item
                .split_whitespace()
                .filter(|w| !w.starts_with('['))
                .collect();
            if words[0].starts_with('-') {
                continue;
            }
            if at == 0 && words.len() == 2 {
                scope = words[0].to_string();
                subjects.push(scope.clone());
                subjects.push(words.join(" "));
                scope.push(' ');
            } else {
                subjects.push(format!("{scope}{}", words[0]));
            }
        }
    }
    subjects
}

/// LD-360 (U1): help at every level. For every command and scope `lodi --help` lists,
/// `lodi SUBJECT --help`, `lodi SUBJECT -h`, `lodi help SUBJECT` and `lodi -h SUBJECT` print
/// the same help on standard output and exit 0: since
/// LD-463 a one-line summary, then the usage lines of the subject. `lodi help` and `lodi -h`
/// print the whole text.
#[test]
fn help_is_printed_at_every_level() {
    let whole = lodi(&["--help"]);
    assert!(whole.status.success());
    let whole = stdout(&whole);
    let subjects = help_subjects(&whole);
    assert_eq!(
        subjects,
        [
            "import", "switch", "update", "pin", "unpin", "init", "develop", "run", "shell",
            "search", "gc", "help",
        ],
        "the help lists exactly the eleven commands of 2.0, then help (#699)"
    );
    for form in [&["help"][..], &["-h"], &["help", "--help"]] {
        let out = lodi(form);
        assert_eq!(out.status.code(), Some(0), "{form:?}");
        assert_eq!(stdout(&out), whole, "{form:?}");
        assert!(out.stderr.is_empty(), "{form:?}");
    }
    for subject in &subjects {
        let words: Vec<&str> = subject.split(' ').collect();
        let mut forms: Vec<Vec<&str>> = vec![
            [&words[..], &["--help"]].concat(),
            [&words[..], &["-h"]].concat(),
            [&["help"], &words[..]].concat(),
            [&["-h"], &words[..]].concat(),
        ];
        // `lodi help --help` asks `help` for help, which is the whole text, checked above; the
        // help entry itself is `lodi help help` and `lodi --help help`.
        if words == ["help"] {
            forms = vec![
                vec!["help", "help"],
                vec!["--help", "help"],
                vec!["-h", "help"],
            ];
        }
        let mut first: Option<String> = None;
        for form in &forms {
            let out = lodi(form);
            let text = stdout(&out);
            assert_eq!(out.status.code(), Some(0), "{form:?}: {}", stderr(&out));
            assert!(out.stderr.is_empty(), "{form:?}: {}", stderr(&out));
            let summary = text.lines().next().unwrap_or_default();
            assert!(
                !summary.is_empty() && !summary.starts_with(' ') && text.lines().nth(1) == Some(""),
                "{form:?}: {text}"
            );
            let usage = format!("  lodi {subject}");
            assert!(
                text.lines()
                    .any(|l| l == usage || l.starts_with(&format!("{usage} "))),
                "{form:?}: {text}"
            );
            assert_ne!(text, whole, "{form:?} printed the whole text");
            match &first {
                None => first = Some(text),
                Some(same) => assert_eq!(&text, same, "{form:?}"),
            }
        }
    }
}

/// A help request about nothing that exists is refused, and a `--help` beside an unknown command
/// or verb is the usage error that invocation already was (LD-360): exit 2, nothing on standard
/// output, the refused words named.
#[test]
fn help_about_nothing_that_exists_is_a_usage_error() {
    for (args, named) in [
        (&["help", "frobnicate"][..], "'help frobnicate'"),
        (&["frobnicate", "--help"], "'frobnicate'"),
    ] {
        let out = lodi(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let text = stderr(&out);
        assert!(text.contains("unsupported"), "{args:?}: {text}");
        assert!(text.contains(named), "{args:?}: {text}");
    }
}

/// U13 for 2.0 (#700): `lodi --help` opens with where to start, whose commands are
/// `lodi import`, `lodi switch` and `lodi init` in that order, then shows examples and the
/// command groups, and ends with a link to the reference.
#[test]
fn help_opens_with_where_to_start() {
    let text = stdout(&lodi(&["--help"]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "");
    let named: Vec<String> = opening(&text)
        .iter()
        .filter_map(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            let at = words.iter().position(|w| *w == "lodi")?;
            Some(format!("lodi {}", words.get(at + 1)?))
        })
        .collect();
    assert_eq!(named, ["lodi import", "lodi switch", "lodi init"], "{text}");
    let headers: Vec<&str> = lines[2 + opening(&text).len()..]
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
    let last = lines.last().copied().unwrap_or_default();
    assert!(
        last.contains("https://") && last.ends_with("docs/CLI.md"),
        "--help does not end with the reference: {last}"
    );
}

/// The command groups of `lodi --help` (#700): the host and home first, then projects, then
/// help. Each group line is a label, `lodi`, and the commands.
#[test]
fn help_groups_the_host_and_home_then_projects() {
    let text = stdout(&lodi(&["--help"]));
    let groups: Vec<(String, Vec<String>)> = text
        .split("\nCommands (")
        .nth(1)
        .expect("--help has a Commands header")
        .lines()
        .skip(1)
        .take_while(|l| l.starts_with("  "))
        .map(|line| {
            let (label, list) = line.split_once(" lodi ").expect("a group lists commands");
            let words = list
                .split(", ")
                .map(|item| {
                    item.split_whitespace()
                        .next()
                        .unwrap_or_default()
                        .to_string()
                })
                .filter(|w| !w.starts_with('-'))
                .collect();
            (label.trim().to_string(), words)
        })
        .collect();
    let want = [
        (
            "host and home",
            &["import", "switch", "update", "pin", "unpin"][..],
        ),
        (
            "projects",
            &["init", "develop", "run", "shell", "search", "gc"],
        ),
        ("help", &["help"]),
    ];
    let want: Vec<(String, Vec<String>)> = want
        .iter()
        .map(|(l, w)| (l.to_string(), w.iter().map(|s| s.to_string()).collect()))
        .collect();
    assert_eq!(groups, want, "{text}");
}

/// No help text names a command the help does not list (#700): every `lodi WORD` in
/// `lodi --help` and in each command's help is a listed command or a flag, so no removed 1.x
/// name is taught. The negative control: an unknown name is caught.
#[test]
fn no_help_names_a_command_it_does_not_list() {
    let whole = stdout(&lodi(&["--help"]));
    let listed = help_subjects(&whole);
    // A command is `lodi` at the start of a line or a column, after `sudo`, or in quotes;
    // prose such as "allowed lodi to manage it" is not one.
    let named = |text: &str| -> Vec<String> {
        let mut out = Vec::new();
        for line in text.lines() {
            for (at, _) in line.match_indices("lodi ") {
                let before = &line[..at];
                let starts = before.trim().is_empty()
                    || before.ends_with("  ")
                    || before.ends_with("sudo ")
                    || before.ends_with(['`', '\'']);
                let word: String = line[at + 5..]
                    .chars()
                    .take_while(|c| c.is_ascii_lowercase())
                    .collect();
                if starts && !word.is_empty() {
                    out.push(word);
                }
            }
        }
        out
    };
    let mut texts = vec![whole.clone()];
    for subject in &listed {
        texts.push(stdout(&lodi(&["help", subject])));
    }
    for text in &texts {
        for word in named(text) {
            assert!(listed.contains(&word), "help names `lodi {word}`:\n{text}");
        }
    }
    assert_eq!(named("run `lodi frobnicate` now"), ["frobnicate"]);
}
