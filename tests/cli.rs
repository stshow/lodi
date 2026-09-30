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
    assert_eq!(stdout(&output), "lodi 1.12.1\n");
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
            "lodi 1.12.1 \u{2014} environment and system manager for Ubuntu, Debian, Arch and Fedora"
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
    assert!(text.contains("lodi lock ") && text.contains("lodi lock --check"));
    assert!(text.contains("lodi develop [--no-nest] [-- COMMAND"));
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
    assert!(text.contains("lodi run [--no-nest] TASK") && text.contains("lodi trust"));
    assert!(text.contains("lodi gc [--dry-run] [--keep-days N] [--images] [-v]"));
    // M-0.5 names `plan` (T-2), and since T-3 `apply` and `status`; M-Import T-4 adds `import`,
    // and LD-382 its name `init`, which starts the scope. There is no `lodi home restore`.
    assert!(text.contains("lodi home init [--out DIR | --stdout] [--force]"));
    assert!(text.contains("lodi home import [--out DIR | --stdout] [--force]"));
    assert!(text.contains("lodi home plan"));
    assert!(text.contains("lodi home apply [--overwrite-drift] [--locked]"));
    assert!(text.contains("lodi home status"));
    assert!(
        !text.contains("lodi home restore"),
        "help advertises lodi home restore"
    );
    assert!(text.contains("lodi host plan [--root DIR]"));
    assert!(text.contains("lodi host apply [--root DIR]"));
    assert!(text.contains("--resolved JOURNAL-ID"));
    assert!(!text.contains("lodi host status"));
    // M-0.4 T-6: the two browse commands, and no `--json` and no `lodi registry` beside them.
    assert!(text.contains("lodi search [--registry | --distro] QUERY"));
    assert!(text.contains("lodi info [TOOL]"));
    for absent in ["--json", "lodi registry", "lodi doctor"] {
        assert!(!text.contains(absent), "help advertises {absent}");
    }
    assert!(text.contains("localhost/lodi-env:<32 hex>"));
    assert!(text.contains("rootless Podman container for a [container] manifest"));
    assert!(text.contains("lodi never installs Podman"));
    assert!(!text.contains("lodi check"), "help advertises lodi check");
    // The top-level verbs of a repository (DONE D2, LD-416).
    for verb in ["import", "plan", "apply", "update"] {
        assert!(
            text.contains(&format!("lodi {verb} [SOURCE]")),
            "help lacks lodi {verb}"
        );
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
        &["trust", "--list"],
        &["init", "--name"],
        // M-0.4 T-6: `lodi search` needs exactly one query and at most one scope, and there is
        // no `--json` in 0.4 (design call D14). `lodi info` takes nothing or one tool name.
        &["search"],
        &["search", "--registry"],
        &["search", "--registry", "--distro", "python"],
        &["search", "--json", "python"],
        &["search", "python", "nodejs"],
        &["search", "--frobnicate", "python"],
        &["info", "--json"],
        &["info", "python", "nodejs"],
    ] {
        let output = lodi(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(stderr(&output).contains("unsupported"), "{args:?}");
    }
}

#[test]
fn develop_run_and_trust_without_a_manifest_are_manifest_errors() {
    let empty = support::scratch("cli-nomanifest");
    for args in [
        &["develop", "--", "true"][..],
        // The interactive entry needs a manifest too, and says so before it opens a shell.
        &["develop"],
        &["run", "build"],
        &["trust"],
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

/// A refused argument after a `home` verb is named in the message, not only the verb (1.0.1):
/// `lodi home plan extra` used to say `'home plan'`. Every case is refused while parsing, before
/// anything reads a home, so none of them touches one.
#[test]
fn home_usage_errors_name_the_refused_argument() {
    for (args, named) in [
        (&["home", "plan", "extra", "more"][..], "'home plan more'"),
        (&["home", "plan", "--json", "x"], "'home plan --json'"),
        (&["home", "status", "extra", "more"], "'home status more'"),
        (
            &["home", "import", "--force", "--frobnicate"],
            "'home import --frobnicate'",
        ),
        (
            &["home", "apply", "--locked", "--locked"],
            "'home apply --locked'",
        ),
    ] {
        let output = lodi(args);
        let text = stderr(&output);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {text}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(text.contains("unsupported"), "{args:?}: {text}");
        assert!(
            text.contains(named),
            "{args:?} does not name {named}: {text}"
        );
        assert!(
            !text.contains("E_"),
            "{args:?}: a usage error carries no code: {text}"
        );
    }
}

#[test]
fn lock_refuses_unknown_options_and_a_missing_manifest() {
    for args in [&["lock", "--frozen"][..], &["lock", "--check", "extra"]] {
        let output = lodi(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(stderr(&output).contains("unsupported"), "{args:?}");
    }
    let empty = support::scratch("cli-empty");
    for args in [&["lock"][..], &["lock", "--check"]] {
        let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .current_dir(&empty)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(3), "{args:?}");
        assert!(
            stderr(&output).contains("lodi: error E_NO_MANIFEST"),
            "{args:?}"
        );
    }
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

/// A bare `lodi` orients (LD-380): the scope lines of `lodi --help`'s opening, on standard
/// output, at exit 0 — until 1.1.1 it was `no command given` at exit 2.
#[test]
fn no_arguments_prints_the_scopes() {
    let output = lodi(&[]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "{}", stderr(&output));
    let help = stdout(&lodi(&["--help"]));
    let scopes: Vec<&str> = help.lines().skip(2).take(6).collect();
    assert_eq!(stdout(&output), format!("{}\n", scopes.join("\n")));
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

/// A lock with nothing to resolve says so, with no parenthesis around an empty list of what
/// was resolved; the second run finds it fresh and says that.
#[test]
fn a_lock_with_nothing_to_resolve_names_no_empty_parenthesis() {
    let project = Project::new("cli-nothing-to-lock", EMPTY);
    let wrote = project.run(&["lock"]);
    assert_eq!(wrote.status.code(), Some(0), "{}", stderr(&wrote));
    assert_eq!(stdout(&wrote), "wrote lodi.lock: nothing to lock\n");
    assert!(project.path("lodi.lock").is_file());
    let again = project.run(&["lock"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert_eq!(stdout(&again), "lodi.lock is up to date: nothing to lock\n");
}

/// `lodi run` with a name the manifest has no task for is a usage error the moment the manifest
/// is read: before the lock is looked at and before the trust gate, in the `lodi: error` shape,
/// with the tasks there are as its note.
#[test]
fn an_unknown_task_is_reported_before_the_lock_and_the_trust_gate() {
    let unknown = "lodi: error: lodi.toml has no task `nope`\n   = its tasks: also, hi\n";
    let project = Project::new("cli-unknown-task", TASKS);
    // No lock and no trust record: the name is still what is reported.
    let first = project.run(&["run", "nope"]);
    assert_eq!(first.status.code(), Some(2), "{}", stderr(&first));
    assert_eq!(stderr(&first), unknown);
    assert!(first.stdout.is_empty());
    // A known task in the same project meets the lock first, so the order above is the rule
    // and not an accident of this project.
    let known = project.run(&["run", "hi"]);
    assert_eq!(known.status.code(), Some(10), "{}", stderr(&known));
    assert!(
        stderr(&known).contains("E_LOCK_STALE"),
        "{}",
        stderr(&known)
    );

    // With a fresh lock and still no trust record, the same.
    assert_eq!(project.run(&["lock"]).status.code(), Some(0));
    let second = project.run(&["run", "nope"]);
    assert_eq!(second.status.code(), Some(2), "{}", stderr(&second));
    assert_eq!(stderr(&second), unknown);
    let untrusted = project.run(&["run", "hi"]);
    assert_eq!(untrusted.status.code(), Some(11), "{}", stderr(&untrusted));
    assert!(
        stderr(&untrusted).contains("E_TRUST_REQUIRED"),
        "{}",
        stderr(&untrusted)
    );

    // A manifest with no tasks at all says that.
    let bare = Project::new("cli-no-tasks", EMPTY);
    let none = bare.run(&["run", "nope"]);
    assert_eq!(none.status.code(), Some(2), "{}", stderr(&none));
    assert_eq!(
        stderr(&none),
        "lodi: error: lodi.toml has no task `nope`\n   = it declares no tasks\n"
    );
}

/// A pseudo-terminal's two ends: the controlling side, kept open while the other is in use, and
/// the terminal a child reads as its standard input.
fn terminal() -> (fs::File, fs::File) {
    use std::ffi::CStr;
    use std::os::fd::FromRawFd;
    use std::os::unix::fs::OpenOptionsExt;
    // SAFETY: the POSIX pseudo-terminal calls on a descriptor this function owns; the name
    // buffer is large enough for any `/dev/pts/N` and is NUL-terminated by `ptsname_r`.
    unsafe {
        let master = libc::posix_openpt(libc::O_RDWR | libc::O_NOCTTY);
        assert!(master >= 0, "a pseudo-terminal is available");
        let controller = fs::File::from_raw_fd(master);
        assert_eq!(libc::grantpt(master), 0);
        assert_eq!(libc::unlockpt(master), 0);
        let mut name = [0 as libc::c_char; 128];
        assert_eq!(libc::ptsname_r(master, name.as_mut_ptr(), name.len()), 0);
        let path = CStr::from_ptr(name.as_ptr()).to_str().unwrap().to_owned();
        let terminal = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOCTTY)
            .open(path)
            .expect("the terminal end opens");
        (controller, terminal)
    }
}

/// `lodi trust` with standard input not a terminal records trust exactly as before and says, on
/// standard error, that it did so without a prompt; the record is real, because the task then
/// runs. With a terminal as standard input nothing is added.
#[test]
fn trust_without_a_terminal_says_it_was_recorded_without_a_prompt() {
    let project = Project::new("cli-trust-no-terminal", TASKS);
    assert_eq!(project.run(&["lock"]).status.code(), Some(0));
    let trusted = project.run(&["trust"]);
    assert_eq!(trusted.status.code(), Some(0), "{}", stderr(&trusted));
    assert_eq!(
        stderr(&trusted),
        "lodi: trust was recorded without a prompt because standard input is not a terminal\n"
    );
    assert!(
        stdout(&trusted).ends_with("trusted.\n"),
        "{}",
        stdout(&trusted)
    );
    let ran = project.run(&["run", "hi"]);
    assert_eq!(ran.status.code(), Some(0), "{}", stderr(&ran));
    assert_eq!(stdout(&ran), "hi\n");
    // Revoking records nothing, so it says nothing of a prompt.
    let revoked = project.run(&["trust", "--revoke"]);
    assert_eq!(revoked.status.code(), Some(0), "{}", stderr(&revoked));
    assert!(revoked.stderr.is_empty(), "{}", stderr(&revoked));

    // A terminal on standard input: the output is what it always was, and trust is recorded.
    let (_controller, tty) = terminal();
    let on_terminal = project
        .command(&["trust"])
        .stdin(Stdio::from(tty))
        .output()
        .expect("the lodi binary runs");
    assert_eq!(
        on_terminal.status.code(),
        Some(0),
        "{}",
        stderr(&on_terminal)
    );
    assert!(on_terminal.stderr.is_empty(), "{}", stderr(&on_terminal));
    assert!(stdout(&on_terminal).ends_with("trusted.\n"));
    let ran = project.run(&["run", "hi"]);
    assert_eq!(ran.status.code(), Some(0), "{}", stderr(&ran));
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
    assert_eq!(project.run(&["lock"]).status.code(), Some(0));

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

/// M-1.0 T-8: no command and no flag is deprecated — the one row of the committed table from 1.2
/// is the home manifest's `[files]` (D5) — so the warning the dispatch itself can print appears
/// nowhere a user reads — not in `--help`, which is the inventory, and not on either stream of an
/// ordinary invocation. `tests/surface.rs` holds the mechanism behind it.
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
/// `lodi SUBJECT --help`, `lodi SUBJECT -h`, `lodi help SUBJECT` and `lodi -h SUBJECT` — and a
/// bare `lodi home` and `lodi host` — print the same help on standard output and exit 0: since
/// LD-463 a one-line summary, then the usage lines of the subject. `lodi help` and `lodi -h`
/// print the whole text. A host form carries a scratch `--root` that nothing ever reads,
/// because help is answered before anything is.
#[test]
fn help_is_printed_at_every_level() {
    // A path in a fresh scratch directory that nothing makes: help never opens it.
    let root = support::scratch("cli-help").join("root");
    let root = root.to_string_lossy().into_owned();
    let whole = lodi(&["--help"]);
    assert!(whole.status.success());
    let whole = stdout(&whole);
    let subjects = help_subjects(&whole);
    assert_eq!(
        subjects,
        [
            "import",
            "plan",
            "apply",
            "update",
            "host",
            "host arm",
            "host import",
            "host plan",
            "host apply",
            "host versions",
            "host pin",
            "host unpin",
            "boot",
            "boot confirm",
            "home",
            "home init",
            "home import",
            "home plan",
            "home apply",
            "home status",
            "init",
            "lock",
            "develop",
            "run",
            "trust",
            "info",
            "shell",
            "search",
            "gc",
            "help",
        ],
        "every command and scope of the help text is covered here"
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
        if words == ["home"] || words == ["host"] {
            forms.push(words.clone());
        }
        if words[0] == "host" {
            for form in forms.iter_mut().filter(|f| f.len() > 1) {
                form.extend(["--root", root.as_str()]);
            }
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
            assert!(text.len() < whole.len(), "{form:?} printed the whole text");
            match &first {
                None => first = Some(text),
                Some(same) => assert_eq!(&text, same, "{form:?}"),
            }
        }
    }
    assert!(
        !std::path::Path::new(&root).exists(),
        "a help request opened its --root"
    );
}

/// A help request about nothing that exists is refused, and a `--help` beside an unknown command
/// or verb is the usage error that invocation already was (LD-360): exit 2, nothing on standard
/// output, the refused words named.
#[test]
fn help_about_nothing_that_exists_is_a_usage_error() {
    for (args, named) in [
        (&["help", "frobnicate"][..], "'help frobnicate'"),
        (&["help", "home", "frobnicate"], "'help home frobnicate'"),
        (&["frobnicate", "--help"], "'frobnicate'"),
        (&["home", "frobnicate", "-h"], "'home frobnicate'"),
        (&["home", "--"], "'home --'"),
    ] {
        let out = lodi(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        let text = stderr(&out);
        assert!(text.contains("unsupported"), "{args:?}: {text}");
        assert!(text.contains(named), "{args:?}: {text}");
    }
}

/// U13, as LD-380 left it: `lodi --help` opens with the three scopes, whose first command is
/// the README quick start's first `lodi` command, then shows examples and the command groups
/// (LD-463).
#[test]
fn help_opens_with_where_to_start() {
    let text = stdout(&lodi(&["--help"]));
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines[1], "");
    assert!(lines[2].starts_with("Three independent scopes."), "{text}");
    assert!(
        lines[2..8].iter().all(|l| !l.is_empty()) && lines[8].is_empty(),
        "{text}"
    );
    let readme =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/README.md")).unwrap();
    // A command is a quoted span with `lodi` as one of its words; the README's are its inline
    // code spans. The opening's are its aligned columns.
    let runs_lodi = |span: &&str| span.split_whitespace().any(|w| w == "lodi");
    let quick = &readme[readme.find("## Quick start").unwrap()..];
    let first = quick
        .lines()
        .find_map(|l| l.split('`').skip(1).step_by(2).find(runs_lodi))
        .unwrap();
    assert!(
        first.ends_with(" host arm"),
        "the quick start begins by arming: {first}"
    );
    // The first command the opening names is that step.
    let named = lines[3]
        .split("  ")
        .map(str::trim)
        .find(|column| runs_lodi(column));
    assert_eq!(named, Some(first), "{}", lines[3]);
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
