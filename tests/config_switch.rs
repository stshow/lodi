//! `lodi switch`, slice 1 of 2 (#695): the host part, then your home part, previewed first.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it (`LODI_HOST_REQUIRE_ROOT=1`), a scratch `HOME` below
//! the root, a decoy current folder holding another config, and the recorded dated archives of
//! `tests/fixtures/host/pin/` served on loopback. The config is the flat one `lodi import`
//! writes, at `~/.config/lodi`; the "may manage" marker of #705 is written by hand, as #707's
//! import does. Unless a case says otherwise it runs on a terminal (`TERM=dumb`, plain step
//! lines) with a `PATH` of recording stub elevators (`sudo`, `doas`, `run0`), which run the rest
//! unprivileged as the fake machine allows (#709), the fake's shims and `git`, and nothing else
//! (`tests/support/elevators.rs`, #715). Nothing reaches `/` or the network.

#[path = "support/elevators.rs"]
mod elevators;
#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/git_loopback.rs"]
mod git_loopback;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
#[path = "support/terminal.rs"]
mod terminal;
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use elevators::{ELEVATORS, Elevators};
use fakehost::{err, out, story};
use lodi::config::switch::{DIRTY, NOT_IN_GIT};
use pinverbs::*;

/// The warning of a fetched config whose `lodi.lock` lacks what a switch resolved.
const UNLOCKED: &str = "does not lock everything";

/// What a package the machine offers but has not installed is called here.
const ZIP: &str = "3.0-13";

/// What standard input is: a terminal holding typed text, or a pipe holding text.
#[derive(Clone, Copy)]
enum Input<'a> {
    Terminal(&'a str),
    Pipe(&'a str),
}

const HOME: &str =
    "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"set bell-style none\\n\"\n";

/// One machine with `bc` installed and `zip` on offer, a scratch home, a decoy current folder.
struct Sw {
    v: Verbs,
    uid: u32,
    gid: u32,
    el: Elevators,
}

fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn write(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
}

impl Sw {
    fn new(name: &str) -> Sw {
        let machine = with(fakehost::Machine::debian(), &[("bc", BC)]).offering(pkg("zip", ZIP));
        Sw::on(name, machine)
    }

    fn on(name: &str, machine: fakehost::Machine) -> Sw {
        let v = Verbs::on(name, machine, false);
        v.alias_now();
        let (uid, gid) = hostroot::ids(&v.case.root);
        let el = Elevators::new(&v.case.root.dir);
        let sw = Sw { v, uid, gid, el };
        fs::remove_file(sw.path(lodi::marker::MARKER)).unwrap();
        sw.v.case.root.write(
            "etc/passwd",
            &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n"),
        );
        sw.v.case
            .root
            .write("etc/group", &format!("root:x:0:\nsample:x:{gid}:\n"));
        mkdir(&sw.path("home"), 0o755);
        mkdir(&sw.decoy(), 0o755);
        write(&sw.decoy().join("host.toml"), "# a decoy\n");
        sw.allow();
        sw
    }

    /// The "may manage" marker of #705, as `lodi import` leaves it once the owner agreed.
    fn allow(&self) {
        mkdir(&self.path("etc/lodi"), 0o755);
        write(&self.path("etc/lodi/may-manage"), "");
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.v.case.root.path(rel)
    }

    fn config(&self) -> PathBuf {
        self.path("home/.config/lodi")
    }

    fn decoy(&self) -> PathBuf {
        self.path("decoy")
    }

    /// A flat config: `host.toml` and `home.toml` where given.
    fn write_config(&self, host: Option<&str>, home: Option<&str>) {
        mkdir(&self.path("home/.config"), 0o700);
        mkdir(&self.config(), 0o755);
        if let Some(host) = host {
            write(&self.config().join("host.toml"), host);
        }
        if let Some(home) = home {
            write(&self.config().join("home.toml"), home);
        }
    }

    fn installed(&self) -> std::collections::BTreeSet<String> {
        self.v.case.installed()
    }

    /// Only the stub elevators, the fake's shims and the tools: no elevator of the machine's own
    /// (`tests/support/elevators.rs`).
    fn path_var(&self) -> String {
        self.el.path(&[&self.v.case.base().join("bin")])
    }

    /// `lodi ARGS --root ROOT` from the decoy folder with `input` on standard input, under
    /// `sh -c` so the umask is the test's; on a terminal, standard error is what it showed.
    fn run_by(
        &self,
        mut command: Command,
        args: &[&str],
        envs: &[(&str, &str)],
        input: Input,
    ) -> Output {
        let rewrite = self.v.server.rewrite();
        let _spawning = fakehost::spawning();
        command
            .arg("-c")
            .arg("umask 022; exec \"$0\" \"$@\"")
            .arg(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .arg("--root")
            .arg(&self.v.case.root.dir)
            .current_dir(self.decoy())
            .env("PATH", self.path_var())
            .env("HOME", self.path("home"))
            .env("TERM", "dumb")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("LODI_REPO")
            .env_remove("SUDO_UID")
            .env_remove("SUDO_GID")
            .env_remove("DOAS_USER")
            .env_remove("LODI_FS_LEDGER")
            .env("LODI_HOME", self.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_REWRITE", rewrite)
            .env("GIT_CEILING_DIRECTORIES", &self.v.case.root.dir)
            .envs(envs.iter().copied())
            .stdout(Stdio::piped());
        match input {
            Input::Terminal(typed) => {
                // The command moves in, so its copies of the terminal close when it is done.
                let (mut output, shown) = terminal::on_terminal(typed, move |stdin, stderr| {
                    let mut command = command;
                    command
                        .stdin(stdin)
                        .stderr(stderr)
                        .output()
                        .expect("lodi runs")
                });
                output.stderr = String::from_utf8_lossy(&shown)
                    .replace("\r\n", "\n")
                    .into_bytes();
                output
            }
            Input::Pipe(text) => {
                let mut child = command
                    .stdin(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("lodi runs");
                let mut stdin = child.stdin.take().unwrap();
                let _ = stdin.write_all(text.as_bytes());
                drop(stdin);
                child.wait_with_output().expect("lodi ends")
            }
        }
    }

    fn run_input(&self, args: &[&str], input: Input) -> Output {
        self.run_by(Command::new("/bin/sh"), args, &[], input)
    }

    /// As the test's own user, on a terminal, with `envs` set too.
    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.run_by(Command::new("/bin/sh"), args, envs, Input::Terminal(""))
    }

    /// As the test's own user, on a terminal.
    fn run(&self, args: &[&str]) -> Output {
        self.run_input(args, Input::Terminal(""))
    }

    /// As `sudo` runs it for the test's own user: root in a user namespace of its own whose root
    /// is that user, `HOME` root's own, and `behind` (`SUDO_UID`, `DOAS_USER`).
    fn run_as_root(&self, args: &[&str], behind: &[(&str, &str)]) -> Output {
        let mut unshare = Command::new(which("unshare").expect("unshare"));
        unshare.args(["--user", "--map-root-user", "/bin/sh"]);
        let root_home = self.path("root-home").display().to_string();
        let mut envs = behind.to_vec();
        envs.push(("HOME", root_home.as_str()));
        self.run_by(unshare, args, &envs, Input::Pipe(""))
    }

    fn remembered(&self) -> Option<String> {
        fs::read_to_string(
            self.path("home/.local/state/lodi")
                .join(lodi::config::REMEMBERED),
        )
        .ok()
    }

    fn lock(&self) -> Option<serde_json::Value> {
        let text = fs::read_to_string(self.config().join("lodi.lock")).ok()?;
        Some(serde_json::from_str(&text).unwrap())
    }

    /// Everything below the root but the fake's state, for "nothing was written".
    fn tree(&self) -> std::collections::BTreeMap<PathBuf, (u32, u64, String, i128)> {
        census(&self.v.case.root.dir)
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(self.config())
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@users.noreply.github.com",
            ])
            .args(args)
            .env("GIT_CEILING_DIRECTORIES", &self.v.case.root.dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?}");
    }

    /// The config committed in a fresh repository, so that a switch names a clean commit.
    fn commit_config(&self) -> String {
        self.git(&["init", "-q"]);
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", "config"]);
        let output = Command::new("git")
            .arg("-C")
            .arg(self.config())
            .args(["rev-parse", "--short", "HEAD"])
            .env("GIT_CEILING_DIRECTORIES", &self.v.case.root.dir)
            .output()
            .unwrap();
        String::from_utf8(output.stdout).unwrap().trim().to_string()
    }
}

/// A refusal: its code, its exit status, and nothing below the root changed.
#[track_caller]
fn refused_untouched(sw: &Sw, args: &[&str], code: &str) -> Output {
    let before = sw.tree();
    let output = sw.run(args);
    refused(&output, code);
    assert_eq!(sw.tree(), before, "{}", story(&output));
    output
}

#[track_caller]
fn usage(sw: &Sw, args: &[&str]) {
    let before = sw.tree();
    let output = sw.run(args);
    assert_eq!(output.status.code(), Some(2), "{}", story(&output));
    assert_eq!(sw.tree(), before, "{}", story(&output));
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|file| file.is_file())
    })
}

fn host_with(names: &[&str]) -> String {
    host("debian", None, names, &[])
}

// ------------------------------------------------------------------------- both parts ---

#[test]
fn switch_applies_the_host_then_the_home() {
    let sw = Sw::new("sw-both");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "set bell-style none\n"
    );
    assert_eq!(out(&output), "", "nothing on standard output");
    assert!(err(&output).contains("switched"), "{}", story(&output));
}

#[test]
fn a_host_failure_stops_before_the_home_and_says_so() {
    let sw = Sw::new("sw-host-fails");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.v.case
        .edit(|m| m["fail"] = serde_json::json!({ "apt-get install": "E: the fake refuses" }));
    let output = sw.run(&["switch"]);
    refused(&output, "E_APPLY");
    assert!(!sw.installed().contains("zip"));
    assert!(
        !sw.path("home/.inputrc").exists(),
        "the home part never ran"
    );
    assert!(
        err(&output).contains("home part did not run"),
        "{}",
        story(&output)
    );
}

/// With `--host` there is no home part, so a host failure says nothing of one.
#[test]
fn a_host_only_failure_names_no_home_part() {
    let sw = Sw::new("sw-host-only-fails");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.v.case
        .edit(|m| m["fail"] = serde_json::json!({ "apt-get install": "E: the fake refuses" }));
    let output = sw.run(&["switch", "--host"]);
    refused(&output, "E_APPLY");
    assert!(!err(&output).contains("home part"), "{}", story(&output));
}

#[test]
fn a_home_failure_keeps_the_host_change() {
    let sw = Sw::new("sw-home-fails");
    let home = "[home]\nversion = \"1\"\n\n[home.file.\"locked/rc\"]\ntext = \"x\\n\"\n";
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(home));
    // A folder the person may not write: planning passes, writing fails.
    mkdir(&sw.path("home/locked"), 0o555);
    let output = sw.run(&["switch"]);
    mkdir(&sw.path("home/locked"), 0o755);
    assert_ne!(output.status.code(), Some(0), "{}", story(&output));
    assert!(
        sw.installed().contains("zip"),
        "the host part is kept: {}",
        story(&output)
    );
    assert!(
        err(&output).contains("home part was not applied"),
        "{}",
        story(&output)
    );
}

#[test]
fn a_second_switch_has_nothing_to_switch() {
    let sw = Sw::new("sw-noop");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    ok(&sw.run(&["switch"]));
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.starts_with("nothing to switch"), "{}", story(&output));
    assert_eq!(
        text.lines().filter(|l| !l.contains("warning")).count(),
        1,
        "{text}"
    );
}

/// An optional package the machine does not offer is said to be left out by every switch, the
/// one with nothing to do too (LD-515).
#[test]
fn a_switch_with_nothing_to_do_still_says_an_optional_package_is_left_out() {
    let sw = Sw::new("sw-noop-optional");
    let host = host_with(&["bc", "zip", "nosuch"]).replacen(
        "[packages]\n",
        "[packages]\noptional = [\"nosuch\"]\n",
        1,
    );
    sw.write_config(Some(&host), Some(HOME));
    let first = sw.run(&["switch"]);
    ok(&first);
    assert!(
        err(&first).contains("W_OPTIONAL_SKIPPED"),
        "{}",
        story(&first)
    );
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.starts_with("nothing to switch"), "{}", story(&output));
    let skipped: Vec<&str> = text
        .lines()
        .filter(|line| line.starts_with("W_OPTIONAL_SKIPPED") && line.contains("`nosuch`"))
        .collect();
    assert_eq!(skipped.len(), 1, "{}", story(&output));
}

// ---------------------------------------------------------------- preview and flags ---

#[test]
fn dry_run_previews_and_changes_and_writes_nothing() {
    let sw = Sw::new("sw-dry");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let before = sw.tree();
    let output = sw.run(&["switch", "--dry-run"]);
    ok(&output);
    assert_eq!(sw.tree(), before, "{}", story(&output));
    assert!(!sw.installed().contains("zip"));
    let text = err(&output);
    assert!(text.contains("+ package zip"), "{text}");
    assert!(text.contains("host: +1 -0 packages"), "{text}");
    assert!(text.contains("home: 1 file"), "{text}");
    assert!(text.contains(NOT_IN_GIT), "{text}");
    assert_eq!(out(&output), "");
}

#[test]
fn conflicting_and_dropped_flags_are_usage_errors() {
    let sw = Sw::new("sw-usage");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    for args in [
        &["switch", "--host", "--home"][..],
        &["switch", "--home", "--host"],
        &["switch", "--ask", "--dry-run"],
        &["switch", "--dry-run", "--ask"],
        &["switch", "--no-update"],
        &["switch", "--unsupported-partial-upgrade"],
        &["switch", "--ref", "main"],
        &[
            "switch",
            "--rev",
            "0123456789012345678901234567890123456789",
        ],
        &["switch", "--refresh"],
    ] {
        usage(&sw, args);
    }
}

#[test]
fn ask_without_a_terminal_stops_with_nothing_changed() {
    let sw = Sw::new("sw-ask");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run_input(&["switch", "--ask"], Input::Pipe("y\n"));
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains("drop --ask"), "{}", story(&output));
    assert!(!sw.installed().contains("zip"));
    assert!(!sw.path("home/.inputrc").exists());
}

// ------------------------------------------------------------------------ one part ---

#[test]
fn host_runs_only_the_host_part_and_home_only_the_home_part() {
    let sw = Sw::new("sw-one-part");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    ok(&sw.run(&["switch", "--home"]));
    assert!(sw.path("home/.inputrc").exists());
    assert!(!sw.installed().contains("zip"));
    ok(&sw.run(&["switch", "--host"]));
    assert!(sw.installed().contains("zip"));
}

#[test]
fn a_part_the_config_does_not_declare_stops() {
    let sw = Sw::new("sw-missing-part");
    sw.write_config(None, Some(HOME));
    let output = refused_untouched(&sw, &["switch", "--host"], "E_NO_MANIFEST");
    assert!(err(&output).contains("no host"), "{}", story(&output));
    ok(&sw.run(&["switch"]));
    assert!(
        sw.path("home/.inputrc").exists(),
        "a home-only config switches"
    );
}

#[test]
fn a_host_only_config_switches_without_a_home() {
    let sw = Sw::new("sw-host-only");
    sw.write_config(Some(&host_with(&["bc", "zip"])), None);
    let output = refused_untouched(&sw, &["switch", "--home"], "E_NO_MANIFEST");
    assert!(err(&output).contains("no home"), "{}", story(&output));
    ok(&sw.run(&["switch"]));
    assert!(sw.installed().contains("zip"));
}

#[test]
fn a_host_lodi_may_not_manage_stops_and_home_still_works() {
    let sw = Sw::new("sw-not-allowed");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    fs::remove_file(sw.path("etc/lodi/may-manage")).unwrap();
    let output = refused_untouched(&sw, &["switch"], "E_HOST_NOT_ARMED");
    assert!(err(&output).contains("lodi import"), "{}", story(&output));
    ok(&sw.run(&["switch", "--home"]));
    assert!(sw.path("home/.inputrc").exists());
}

// --------------------------------------------------------------- the commit, git ---

#[test]
fn switch_names_the_commit_and_warns_on_uncommitted_changes() {
    let sw = Sw::new("sw-git");
    sw.write_config(Some(&host_with(&["bc"])), Some(HOME));
    let head = sw.commit_config();
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.contains(&format!("({head})")), "{text}");
    assert!(!text.contains(DIRTY), "{text}");
    write(&sw.config().join("host.toml"), &host_with(&["bc", "zip"]));
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.contains(&format!("({head}+uncommitted)")), "{text}");
    assert_eq!(text.matches(DIRTY).count(), 1, "{text}");
}

// ------------------------------------------------------------- finding the config ---

#[test]
fn a_typed_path_is_remembered_after_success_only() {
    let sw = Sw::new("sw-typed");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let typed = sw.config().display().to_string();
    ok(&sw.run(&["switch", &typed, "--dry-run"]));
    assert_eq!(sw.remembered(), None, "a dry run remembers nothing");
    sw.v.case
        .edit(|m| m["fail"] = serde_json::json!({ "apt-get install": "E: the fake refuses" }));
    assert_ne!(sw.run(&["switch", &typed]).status.code(), Some(0));
    assert_eq!(sw.remembered(), None, "a failed switch remembers nothing");
    sw.v.case.edit(|m| {
        m.as_object_mut().unwrap().remove("fail");
    });
    ok(&sw.run(&["switch", &typed]));
    assert_eq!(sw.remembered(), Some(format!("{typed}\n")));
}

// ------------------------------------------------------------------------- pins ---

#[test]
fn a_first_switch_locks_the_config_as_the_person() {
    let sw = Sw::new("sw-lock");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    assert!(sw.lock().is_none());
    ok(&sw.run(&["switch"]));
    let lock = sw.lock().expect("lodi.lock written");
    assert_eq!(lock["format"], "lodi-config-lock");
    assert!(lock["hosts"]["host.toml"].is_object(), "{lock}");
}

#[test]
fn update_dry_run_writes_no_lock() {
    let sw = Sw::new("sw-update-dry");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let before = sw.tree();
    let output = sw.run(&["switch", "--update", "--dry-run"]);
    ok(&output);
    assert_eq!(sw.tree(), before, "{}", story(&output));
}

// ------------------------------------------------------------------ acting for you ---

#[test]
fn under_sudo_the_home_is_the_persons_and_their_files_are_theirs() {
    use std::os::unix::fs::MetadataExt;
    let sw = Sw::new("sw-sudo");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    mkdir(&sw.path("root-home"), 0o700);
    let (uid, gid) = (sw.uid.to_string(), sw.gid.to_string());
    let output = sw.run_as_root(&["switch"], &[("SUDO_UID", &uid), ("SUDO_GID", &gid)]);
    ok(&output);
    assert!(sw.installed().contains("zip"));
    let meta = fs::metadata(sw.path("home/.inputrc")).expect("the person's home was switched");
    assert_eq!((meta.uid(), meta.gid()), (sw.uid, sw.gid));
    assert!(!sw.path("root-home/.inputrc").exists(), "never root's home");
    assert!(sw.el.calls().is_empty(), "already root: no elevator");
    let logs = sw.path("home/.local/state/lodi/logs");
    assert!(logs.is_dir(), "the log is the person's: {}", story(&output));
}

#[test]
fn under_doas_the_home_is_the_persons() {
    let sw = Sw::new("sw-doas");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    mkdir(&sw.path("root-home"), 0o700);
    let output = sw.run_as_root(&["switch"], &[("DOAS_USER", "sample")]);
    ok(&output);
    assert!(sw.path("home/.inputrc").exists(), "{}", story(&output));
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert!(sw.el.calls().is_empty(), "already root: no elevator");
    let output = sw.run_as_root(&["switch"], &[("DOAS_USER", "nobody-here")]);
    refused(&output, "E_CONFIG");
}

// ----------------------------------------------------------------------- progress ---

/// A home step with nothing to do is left out of the list (#694 story 7), and under `sudo` the
/// home part's steps are the child's own progress, counted (#694 story 17).
#[test]
fn home_steps_with_nothing_to_do_are_left_out_and_counted_under_sudo() {
    let sw = Sw::new("sw-home-steps");
    sw.write_config(None, Some(HOME));
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.contains("[1/1] Files: done, 1 in"), "{text}");
    assert!(
        !text.contains("Tools") && !text.contains("User services"),
        "{text}"
    );
    let sudo = Sw::new("sw-home-steps-sudo");
    sudo.write_config(None, Some(HOME));
    mkdir(&sudo.path("root-home"), 0o700);
    let (uid, gid) = (sudo.uid.to_string(), sudo.gid.to_string());
    let output = sudo.run_as_root(&["switch"], &[("SUDO_UID", &uid), ("SUDO_GID", &gid)]);
    ok(&output);
    let text = err(&output);
    assert!(text.contains("[1/1] Files: done, 1 in"), "{text}");
    assert!(
        !text.contains("Tools") && !text.contains("User services"),
        "{text}"
    );
}

#[test]
fn plain_step_lines_number_host_and_home_steps_together() {
    let sw = Sw::new("sw-steps");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run(&["switch"]);
    ok(&output);
    let text = err(&output);
    assert!(text.contains("[1/3] Download packages"), "{text}");
    assert!(text.contains("[2/3] Install packages"), "{text}");
    assert!(text.contains("[3/3] Files"), "{text}");
    let summary = text.lines().last().unwrap();
    assert!(summary.starts_with("switched in "), "{text}");
    assert!(summary.contains("+1 -0 packages, home 1 file"), "{text}");
    assert_eq!(
        sw.el.calls().len(),
        1,
        "the host steps came from the root run"
    );
    let logs: Vec<_> = fs::read_dir(sw.path("home/.local/state/lodi/logs"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(logs.len(), 1);
    let log = fs::read_to_string(logs[0].path()).unwrap();
    for step in ["Install packages", "Files"] {
        assert!(log.contains(step), "{step}: {log}");
    }
}

// -------------------------------------------------------------------- more flags ---

#[test]
fn a_hand_edited_file_stops_the_home_part_unless_overwrite_drift() {
    let sw = Sw::new("sw-drift");
    sw.write_config(None, Some(HOME));
    ok(&sw.run(&["switch"]));
    fs::write(sw.path("home/.inputrc"), "mine\n").unwrap();
    write(
        &sw.config().join("home.toml"),
        &HOME.replace("none", "visible"),
    );
    let output = sw.run(&["switch"]);
    assert_ne!(output.status.code(), Some(0), "{}", story(&output));
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "mine\n"
    );
    ok(&sw.run(&["switch", "--overwrite-drift"]));
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "set bell-style visible\n"
    );
}

#[test]
fn update_refreshes_the_pins_then_switches() {
    let sw = Sw::new("sw-update");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run(&["switch", "--update"]);
    ok(&output);
    assert!(sw.installed().contains("zip"));
    let lock = sw.lock().expect("--update writes the lock");
    assert!(lock["hosts"]["host.toml"].is_object(), "{lock}");
}

#[test]
fn verbose_shows_the_tools_own_output_and_stdout_stays_empty() {
    let sw = Sw::new("sw-verbose");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run(&["switch", "-v"]);
    ok(&output);
    assert_eq!(out(&output), "");
    assert!(err(&output).contains("switched in "), "{}", story(&output));
    // The package manager's own lines, under the live step (#694, #695).
    assert!(
        err(&output).contains("    0 upgraded, 1 newly installed"),
        "{}",
        story(&output)
    );
    let plain = Sw::new("sw-not-verbose");
    plain.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = plain.run(&["switch"]);
    ok(&output);
    assert!(
        !err(&output).contains("newly installed"),
        "{}",
        story(&output)
    );
}

// ------------------------------------------------------------------- from a git URL ---

/// The URL a test types, served on loopback from a repository the test commits.
const URL: &str = "github:you/lodi";

/// A repository holding `files`, committed and served as [`URL`].
struct Remote {
    served: git_loopback::Served,
    work: PathBuf,
}

impl Remote {
    fn start(sw: &Sw, files: &[(&str, &str)]) -> (Remote, String) {
        let base = sw.v.case.base().to_path_buf();
        let work = base.join("work");
        let served = base.join("served");
        for dir in [&work, &served] {
            let _ = fs::remove_dir_all(dir);
            mkdir(dir, 0o755);
        }
        let remote = Remote {
            served: git_loopback::Served::start(&served),
            work,
        };
        let commit = remote.commit(files, "config");
        (remote, commit)
    }

    /// `files` written over the work tree, committed and pushed; the commit.
    fn commit(&self, files: &[(&str, &str)], message: &str) -> String {
        for (name, text) in files {
            write(&self.work.join(name), text);
        }
        let commit = git_loopback::commit_all(&self.work, message);
        self.served.push(&self.work, "lodi");
        commit
    }

    /// `LODI_FETCH_REWRITE` sending [`URL`] here and every other request to the pin archives.
    fn rewrite(&self, sw: &Sw) -> String {
        let git = self
            .served
            .rewrite("https://github.com/you/lodi.git", "lodi");
        format!("{git};{}", sw.v.server.rewrite())
    }

    fn run(&self, sw: &Sw, args: &[&str]) -> Output {
        let rewrite = self.rewrite(sw);
        sw.run_env(args, &[("LODI_FETCH_REWRITE", &rewrite)])
    }
}

#[test]
fn switch_from_a_git_url_applies_it_names_its_commit_and_remembers_it() {
    let sw = Sw::new("sw-url");
    let (remote, commit) = Remote::start(
        &sw,
        &[
            ("host.toml", &host_with(&["bc", "zip"])),
            ("home.toml", HOME),
        ],
    );
    let state = sw.path("home/.local/state/lodi");
    ok(&remote.run(&sw, &["switch", URL, "--dry-run"]));
    assert_eq!(sw.remembered(), None, "a dry run remembers nothing");
    assert!(
        !state.join("fetched.json").exists(),
        "a dry run records nothing"
    );
    let output = remote.run(&sw, &["switch", URL]);
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    let tree = state
        .join("git")
        .read_dir()
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let tree = tree.join(&commit);
    assert!(tree.join("host.toml").is_file(), "{}", tree.display());
    assert!(
        !tree.join("lodi.lock").exists(),
        "a fetched tree is never written"
    );
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "set bell-style none\n"
    );
    let text = err(&output);
    assert!(text.contains(&format!("({})", &commit[..7])), "{text}");
    assert!(
        !text.contains(DIRTY) && !text.contains(NOT_IN_GIT),
        "a fetched config is a commit: {text}"
    );
    assert_eq!(sw.remembered(), Some(format!("{URL}\n")));
    assert_eq!(out(&output), "");

    // The remembered URL, its commit locked and its tree kept: no path and no request.
    let asked = remote.served.requests();
    let record = || {
        use std::os::unix::fs::MetadataExt;
        let meta = fs::metadata(state.join("fetched.json")).expect("fetched.json");
        (meta.ino(), meta.mtime(), meta.mtime_nsec())
    };
    let recorded = record();
    // What the fetched tree's lock lacked was resolved once and kept beside it (LD-528).
    assert!(text.contains(UNLOCKED), "{text}");
    let again = remote.run(&sw, &["switch"]);
    ok(&again);
    assert_eq!(record(), recorded, "nothing to switch rewrote fetched.json");
    let shown = err(&again);
    let lines: Vec<&str> = shown.lines().collect();
    assert!(
        lines[0].starts_with("source ") && lines[0].ends_with(": locked, no request"),
        "{}",
        story(&again)
    );
    assert_eq!(
        lines[1],
        format!("nothing to switch ({})", &commit[..7]),
        "{}",
        story(&again)
    );
    assert!(!shown.contains(UNLOCKED), "{}", story(&again));
    assert_eq!(
        remote.served.requests(),
        asked,
        "a locked switch asked again"
    );
    let update = remote.run(&sw, &["switch", "--update"]);
    refused(&update, "E_UNSUPPORTED");
}

#[test]
fn ref_rev_and_refresh_choose_what_is_fetched() {
    let sw = Sw::new("sw-url-flags");
    let (remote, first) = Remote::start(&sw, &[("host.toml", &host_with(&["bc"]))]);
    git_loopback::git(&remote.work, &["branch", "stable"]);
    remote.served.push(&remote.work, "lodi");
    for both in [
        &["--ref", "main", "--rev", &first][..],
        &["--rev", &first, "--refresh"],
    ] {
        let both = remote.run(&sw, &[&["switch", URL][..], both].concat());
        assert_eq!(both.status.code(), Some(2), "{}", story(&both));
    }
    assert_eq!(remote.served.requests(), 0, "a usage error asked");
    ok(&remote.run(&sw, &["switch", URL]));
    let second = remote.commit(&[("host.toml", &host_with(&["bc", "zip"]))], "zip");
    let record = || -> serde_json::Value {
        let text = fs::read_to_string(sw.path("home/.local/state/lodi/fetched.json")).unwrap();
        serde_json::from_str(&text).unwrap()
    };

    let kept = remote.run(&sw, &["switch"]);
    ok(&kept);
    assert!(!sw.installed().contains("zip"), "a push moved the switch");
    let refreshed = remote.run(&sw, &["switch", "--refresh"]);
    ok(&refreshed);
    assert!(
        err(&refreshed).contains(&format!("refresh {first} -> {second}")),
        "{}",
        story(&refreshed)
    );
    assert!(sw.installed().contains("zip"));
    assert_eq!(record()["rev"], second.as_str());

    let pinned = remote.run(&sw, &["switch", "--rev", &first]);
    ok(&pinned);
    assert!(!sw.installed().contains("zip"), "{}", story(&pinned));
    ok(&remote.run(&sw, &["switch"]));
    assert_eq!(
        record()["rev"],
        first.as_str(),
        "a bare switch moved the commit"
    );

    ok(&remote.run(&sw, &["switch", "--ref", "stable"]));
    assert_eq!(
        (record()["ref"].clone(), record()["rev"].clone()),
        ("stable".into(), first.as_str().into())
    );

    // Offline, a refresh asks and fails; nothing is switched and the record stays.
    let rewrite = remote.rewrite(&sw);
    drop(remote);
    let before = record();
    let offline = sw.run_env(
        &["switch", "--refresh"],
        &[
            ("LODI_FETCH_REWRITE", &rewrite),
            ("LODI_FETCH_ATTEMPTS", "1"),
        ],
    );
    refused(&offline, "E_FETCH");
    assert_eq!(record(), before);
}

/// The definition of done's final proof (`docs/DONE.md` §4, D5), as the deleted
/// `tests/done_final_proof.rs` ran it with `apply`: a served config pins `tzdata` older and is
/// switched by URL; a newer commit pins it newer and `--refresh` switches it; `git revert` of
/// that commit and `--refresh` bring the older version back, `bc` and its undeclared
/// dependency never move, and a further switch has nothing to do (LD-535). The guest row
/// `done-final-proof-https-revert` proves the same on bare guests.
#[test]
fn a_reverted_commit_switched_by_url_brings_the_earlier_pin_back() {
    const DEP: (&str, &str) = ("libdep", "3.1-2");
    let machine = fakehost::Machine::debian()
        .with(pkg("bc", BC).depends(&[DEP.0]))
        .with(pkg("tzdata", TZ_RECENT))
        .with(pkg(DEP.0, DEP.1).dep());
    let sw = Sw::on("sw-final-proof", machine);
    let config = |day: &str| {
        host(
            "debian",
            Some(RECENT),
            &["bc", "tzdata"],
            &[("tzdata", day)],
        )
    };
    let (remote, _) = Remote::start(&sw, &[("host.toml", &config(OLDER)), ("home.toml", HOME)]);
    let version = |name: &str| sw.v.case.machine()["installed"][name].clone();

    let first = remote.run(&sw, &["switch", URL]);
    ok(&first);
    assert_eq!(version("tzdata")["version"], TZ_OLDER, "{}", story(&first));

    remote.commit(&[("host.toml", &config(RECENT))], "newer tzdata");
    let newer = remote.run(&sw, &["switch", "--refresh"]);
    ok(&newer);
    assert_eq!(version("tzdata")["version"], TZ_RECENT, "{}", story(&newer));

    git_loopback::git(&remote.work, &["revert", "--no-edit", "HEAD"]);
    remote.served.push(&remote.work, "lodi");
    let reverted = remote.run(&sw, &["switch", "--refresh"]);
    ok(&reverted);
    assert_eq!(
        version("tzdata")["version"],
        TZ_OLDER,
        "{}",
        story(&reverted)
    );
    assert_eq!(version("bc")["version"], BC, "{}", story(&reverted));
    assert_eq!(version(DEP.0)["version"], DEP.1, "{}", story(&reverted));
    assert_eq!(version(DEP.0)["explicit"], false, "{}", story(&reverted));
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "set bell-style none\n"
    );

    let again = remote.run(&sw, &["switch"]);
    ok(&again);
    assert!(
        err(&again).contains("nothing to switch"),
        "{}",
        story(&again)
    );
}

#[test]
fn under_sudo_a_url_is_fetched_and_recorded_in_the_persons_state() {
    let sw = Sw::new("sw-url-sudo");
    let (remote, commit) = Remote::start(
        &sw,
        &[
            ("host.toml", &host_with(&["bc", "zip"])),
            ("home.toml", HOME),
        ],
    );
    mkdir(&sw.path("root-home"), 0o700);
    let (uid, gid) = (sw.uid.to_string(), sw.gid.to_string());
    let rewrite = remote.rewrite(&sw);
    let output = sw.run_as_root(
        &["switch", URL],
        &[
            ("SUDO_UID", &uid),
            ("SUDO_GID", &gid),
            ("LODI_FETCH_REWRITE", &rewrite),
        ],
    );
    ok(&output);
    assert!(sw.installed().contains("zip"));
    assert!(sw.path("home/.inputrc").exists(), "{}", story(&output));
    let record = fs::read_to_string(sw.path("home/.local/state/lodi/fetched.json")).unwrap();
    assert!(record.contains(&commit), "{record}");
    let tree = sw.path("home/.local/state/lodi/git");
    assert!(tree.is_dir(), "the tree is kept in the person's state");
    assert!(!sw.path("root-home/.local").exists(), "never root's state");
    // The typed URL is remembered as the person's, owned as every file written for them is: by
    // the owner of their home, not by whatever `SUDO_UID` names (LD-499, LD-528).
    let remembered = fs::read_to_string(
        sw.path("home/.local/state/lodi")
            .join(lodi::config::REMEMBERED),
    );
    assert_eq!(
        remembered.ok().as_deref(),
        Some(&*format!("{URL}\n")),
        "{}",
        story(&output)
    );
    assert!(
        !err(&output).contains("cannot remember"),
        "{}",
        story(&output)
    );
}

// ---------------------------------------------------------------------- --resolved ---

/// An interrupted apply whose one file action left the file neither as it was nor as it was to
/// become: a journal only a person can settle.
fn ambiguous_journal(sw: &Sw) -> String {
    let id = format!("20260101T000000Z-{}", "a".repeat(32));
    let header = serde_json::json!({
        "record": "header", "version": 1, "journalId": id,
        "startedAt": "2026-01-01T00:00:00Z", "lodiVersion": "2.0.0", "root": "/",
        "manifestDigest": format!("sha256:{}", "0".repeat(64)), "distro": "debian",
        "programs": [], "euid": 0,
        "actions": [{
            "id": "f1", "kind": "file.write", "summary": "write /etc/motd",
            "pre": [{ "path": "/etc/motd", "exists": false }],
            "post": [{ "path": "/etc/motd", "exists": true, "regular": true,
                       "digest": format!("sha256:{}", "1".repeat(64)) }],
        }],
    });
    let begin =
        serde_json::json!({ "record": "begin", "action": "f1", "at": "2026-01-01T00:00:01Z" });
    for dir in ["var/lib/lodi", "var/lib/lodi/host"] {
        mkdir(&sw.path(dir), 0o755);
    }
    mkdir(&sw.path("var/lib/lodi/host/journal"), 0o700);
    fs::write(
        sw.path(&format!("var/lib/lodi/host/journal/{id}.json")),
        format!("{header}\n{begin}\n"),
    )
    .unwrap();
    write(&sw.path("etc/motd"), "by hand\n");
    id
}

#[test]
fn resolved_goes_on_past_an_interrupted_apply_a_person_settled() {
    let sw = Sw::new("sw-resolved");
    sw.write_config(Some(&host_with(&["bc", "zip"])), None);
    let id = ambiguous_journal(&sw);
    let stopped = sw.run(&["switch"]);
    refused(&stopped, "E_JOURNAL_AMBIGUOUS");
    assert!(err(&stopped).contains("--resolved"), "{}", story(&stopped));
    assert!(!sw.installed().contains("zip"));
    let wrong = sw.run(&["switch", "--resolved", "20000101T000000Z-0000"]);
    refused(&wrong, "E_JOURNAL_AMBIGUOUS");
    assert!(!sw.installed().contains("zip"));
    ok(&sw.run(&["switch", "--resolved", &id]));
    assert!(sw.installed().contains("zip"));
}
// ----------------------------------------------------------------------- elevation ---

/// The root run fetches through the person's `LODI_FETCH_REWRITE` and `LODI_FETCH_ATTEMPTS`,
/// carried in the request, though the elevator clears the environment as `sudo` does: a keyring
/// by a URL nothing can resolve is fetched from the mirror, by the person for the lock and by the
/// root run to write it (LD-515).
#[test]
fn the_root_run_fetches_through_the_person_s_mirror() {
    let sw = Sw::new("sw-mirror");
    let key = {
        let mut out = vec![0xc6, 29];
        out.extend_from_slice(b"\x04invented public key material");
        out.extend_from_slice(&[0xcd, 17]);
        out.extend_from_slice(b"invented test key");
        out
    };
    let url = "https://keys.example.invalid/vendor.gpg";
    let sha = lodi::util::sha256_hex(&key);
    sw.v.server.files.lock().unwrap().insert(url.into(), key);
    let host = format!(
        "{}\n[sources.vendor]\nuris = [\"https://packages.example.invalid/debian\"]\n\
         suites = [\"bookworm\"]\ncomponents = [\"main\"]\nsigned_by_url = \"{url}\"\n\
         signed_by_sha256 = \"{sha}\"\n",
        host_with(&["bc"])
    );
    sw.write_config(Some(&host), None);
    let output = sw.run_env(&["switch"], &[("LODI_FETCH_ATTEMPTS", "1")]);
    ok(&output);
    assert_eq!(sw.el.calls().len(), 1, "the root run: {}", story(&output));
    assert_eq!(
        sw.v.server.count(url),
        2,
        "the lock, then the root run: {}",
        story(&output)
    );
}

/// Not root, on a terminal, with host changes: the host part runs once through sudo, the
/// first of the three, as lodi's hidden entry; the home part stays this process's.
#[test]
fn a_host_change_runs_the_host_part_once_through_sudo() {
    let sw = Sw::new("sw-elevate");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run(&["switch"]);
    ok(&output);
    let calls = sw.el.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("sudo -- "), "{calls:?}");
    assert!(
        calls[0].contains(" __elevated --request switch"),
        "{calls:?}"
    );
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    let meta = fs::metadata(sw.path("home/.inputrc")).expect("the home part ran");
    use std::os::unix::fs::MetadataExt;
    assert_eq!((meta.uid(), meta.gid()), (sw.uid, sw.gid), "as the person");
    assert_eq!(out(&output), "");
}

/// Drift is settled before the prompt: refused with no elevator call, and `--overwrite-drift`,
/// handed to the child, is acted on there without a question.
#[test]
fn drift_is_settled_before_the_prompt_and_the_child_acts_on_the_answer() {
    let sw = Sw::new("sw-elevate-drift");
    let exact = |names: &[&str]| host_with(names).replace("\"managed\"", "\"exact\"");
    sw.write_config(Some(&exact(&["bc"])), None);
    ok(&sw.run(&["switch"]));
    sw.v.case.install_by_hand(pkg("htop", "3.0-1"));
    sw.write_config(Some(&exact(&["bc", "zip"])), Some(HOME));
    sw.el.forget();
    let output = sw.run(&["switch"]);
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains("htop"), "{}", story(&output));
    assert!(sw.el.calls().is_empty(), "{}", story(&output));
    assert!(!sw.path("home/.inputrc").exists());

    let output = sw.run(&["switch", "--overwrite-drift"]);
    ok(&output);
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
    assert!(!sw.installed().contains("htop"), "{}", story(&output));
    assert!(sw.installed().contains("zip"));
    assert!(!err(&output).contains("[y/N]"), "{}", story(&output));
}

/// The fake machine changes between the preview and the root run: the child stops with
/// nothing changed, and the home part does not run.
#[test]
fn a_host_that_changed_since_the_preview_stops_the_root_run() {
    let sw = Sw::new("sw-elevate-moved");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let original = sw.v.case.machine();
    sw.v.case.install_by_hand(pkg("zip", ZIP));
    let moved = sw.v.case.machine();
    sw.v.case.set_machine(&original);
    let moved_at = sw.path("moved.json");
    fs::write(&moved_at, serde_json::to_string_pretty(&moved).unwrap()).unwrap();
    let state = sw.v.case.base().join("state/machine.json");
    let cp = which("cp").expect("cp");
    let hook = format!(
        "'{}' '{}' '{}'",
        cp.display(),
        moved_at.display(),
        state.display()
    );
    sw.el.install(&ELEVATORS, false, &hook);
    let output = sw.run(&["switch"]);
    refused(&output, "E_DECLINED");
    assert!(
        err(&output).contains("changed since the preview"),
        "{}",
        story(&output)
    );
    assert_eq!(sw.v.case.machine(), moved, "nothing changed after the move");
    assert!(!sw.path("home/.inputrc").exists(), "{}", story(&output));
}

#[test]
fn the_elevator_is_the_first_of_sudo_doas_and_run0() {
    let sw = Sw::new("sw-elevate-which");
    let cases: [(&[&str], &str); 3] = [
        (&ELEVATORS, "sudo"),
        (&["doas"], "doas"),
        (&["run0"], "run0"),
    ];
    for (at, (present, used)) in cases.into_iter().enumerate() {
        sw.el.install(present, false, "");
        sw.el.forget();
        // Each run has a host change: zip in, out, in again.
        let names: &[&str] = if at == 1 { &["bc"] } else { &["bc", "zip"] };
        sw.write_config(Some(&host_with(names)), None);
        let output = sw.run(&["switch"]);
        ok(&output);
        let calls = sw.el.calls();
        assert_eq!(calls.len(), 1, "{calls:?} {}", story(&output));
        assert!(calls[0].starts_with(&format!("{used} ")), "{calls:?}");
        assert_eq!(
            sw.installed().contains("zip"),
            at != 1,
            "{}",
            story(&output)
        );
    }
}

/// An elevator of the machine's own (a program root owns, here a link to `true`) is never run
/// under `LODI_HOST_REQUIRE_ROOT=1`: refused before it runs, with nothing changed (#715).
#[test]
fn an_elevator_root_owns_is_refused_under_require_root() {
    use std::os::unix::fs::MetadataExt;
    let sw = Sw::new("sw-elevate-own");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let real = which("true").expect("true");
    assert_eq!(fs::metadata(&real).unwrap().uid(), 0, "{}", real.display());
    let own = sw.path("machine-bin");
    mkdir(&own, 0o755);
    std::os::unix::fs::symlink(&real, own.join("sudo")).unwrap();
    let path = format!(
        "{}:{}",
        own.display(),
        sw.v.case.base().join("bin").display()
    );
    let machine = sw.v.case.machine();
    let output = sw.run_env(&["switch"], &[("PATH", &path)]);
    refused(&output, "E_HOST_ROOT_REQUIRED");
    let shown = err(&output);
    assert!(
        shown.contains(&own.join("sudo").display().to_string()),
        "{shown}"
    );
    assert_eq!(sw.v.case.machine(), machine);
    assert!(!sw.path("home/.inputrc").exists(), "{}", story(&output));
}

#[test]
fn no_elevator_at_all_stops_before_the_host_changes() {
    let sw = Sw::new("sw-elevate-none");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.el.install(&[], false, "");
    let machine = sw.v.case.machine();
    let output = sw.run(&["switch"]);
    refused(&output, "E_NEED_ROOT");
    for hint in ["--home", "sudo"] {
        assert!(err(&output).contains(hint), "{hint}: {}", story(&output));
    }
    assert_eq!(sw.v.case.machine(), machine);
    assert!(!sw.path("home/.inputrc").exists());
}

/// A script with host changes stops naming `--home`; one without host changes goes ahead.
#[test]
fn without_a_terminal_only_host_changes_stop() {
    let sw = Sw::new("sw-elevate-script");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let output = sw.run_input(&["switch"], Input::Pipe(""));
    refused(&output, "E_NEED_ROOT");
    assert!(err(&output).contains("--home"), "{}", story(&output));
    assert!(sw.el.calls().is_empty());
    assert!(!sw.installed().contains("zip"));
    assert!(!sw.path("home/.inputrc").exists());

    sw.write_config(Some(&host_with(&["bc"])), Some(HOME));
    ok(&sw.run(&["switch", "--host"]));
    sw.el.forget();
    let output = sw.run_input(&["switch"], Input::Pipe(""));
    ok(&output);
    assert!(sw.path("home/.inputrc").exists(), "{}", story(&output));
    assert!(sw.el.calls().is_empty(), "no host change, no elevator");
}

#[test]
fn dry_run_and_a_declined_ask_never_run_an_elevator() {
    let sw = Sw::new("sw-elevate-dry");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    ok(&sw.run(&["switch", "--dry-run"]));
    let output = sw.run_input(&["switch", "--ask"], Input::Terminal("n\n"));
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains("switch? [y/N]"), "{}", story(&output));
    assert!(sw.el.calls().is_empty());
    assert!(!sw.installed().contains("zip"));
    let output = sw.run_input(&["switch", "--ask"], Input::Terminal("y\n"));
    ok(&output);
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
    assert!(sw.installed().contains("zip"));
}

#[test]
fn a_refused_password_changes_nothing_and_runs_no_home_part() {
    let sw = Sw::new("sw-elevate-refused");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.el.install(&ELEVATORS, true, "");
    let machine = sw.v.case.machine();
    let output = sw.run(&["switch"]);
    refused(&output, "E_NEED_ROOT");
    assert_eq!(sw.el.calls().len(), 1);
    assert_eq!(sw.v.case.machine(), machine);
    assert!(!sw.path("home/.inputrc").exists(), "{}", story(&output));
    assert!(
        sw.lock().is_none(),
        "lodi.lock was written: {}",
        story(&output)
    );

    // A lock there already is left as it was, byte for byte (LD-520).
    sw.el.install(&ELEVATORS, false, "");
    sw.write_config(Some(&host_with(&["bc"])), Some(HOME));
    ok(&sw.run(&["switch"]));
    let path = sw.config().join("lodi.lock");
    let lock = fs::read(&path).expect("the first switch wrote lodi.lock");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.el.install(&ELEVATORS, true, "");
    let output = sw.run(&["switch"]);
    refused(&output, "E_NEED_ROOT");
    assert_eq!(fs::read(&path).unwrap(), lock, "{}", story(&output));
}

/// A refusal writes nothing, the config's lock included: no terminal to ask a password on, and
/// a declined `--ask`.
#[test]
fn a_refused_switch_writes_no_lock() {
    let sw = Sw::new("sw-refused-no-lock");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let before = sw.tree();
    let output = sw.run_input(&["switch"], Input::Pipe(""));
    refused(&output, "E_NEED_ROOT");
    assert_eq!(sw.tree(), before, "{}", story(&output));
    let output = sw.run_input(&["switch", "--ask"], Input::Terminal("n\n"));
    refused(&output, "E_DECLINED");
    assert_eq!(sw.tree(), before, "{}", story(&output));
    assert!(sw.lock().is_none());
}

/// `lodi.lock` is the person's, written before the elevator runs; the root run leaves it.
#[test]
fn the_lock_is_written_before_the_prompt_and_the_root_run_leaves_it() {
    let sw = Sw::new("sw-elevate-lock");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let at_call = sw.path("lock-at-call");
    let hook = format!(
        "'{}' '{}' '{}'",
        which("cp").expect("cp").display(),
        sw.config().join("lodi.lock").display(),
        at_call.display()
    );
    sw.el.install(&ELEVATORS, false, &hook);
    let output = sw.run(&["switch"]);
    ok(&output);
    let before = fs::read(&at_call).expect("lodi.lock was there when the elevator ran");
    assert_eq!(fs::read(sw.config().join("lodi.lock")).unwrap(), before);
    use std::os::unix::fs::MetadataExt;
    let meta = fs::metadata(sw.config().join("lodi.lock")).unwrap();
    assert_eq!(meta.uid(), sw.uid);
}

// ------------------------------------------------- planning inputs readable as you ---

/// The host record and the journals a root run leaves are readable by everyone, so that the
/// next preview, as the person, reads them without a password (LD-492).
#[test]
fn the_record_and_the_journals_a_root_run_leaves_are_readable_by_everyone() {
    let sw = Sw::new("sw-readable-state");
    sw.write_config(Some(&host_with(&["bc", "zip"])), None);
    ok(&sw.run(&["switch"]));
    let mode = |rel: &str| {
        fs::symlink_metadata(sw.path(rel))
            .unwrap_or_else(|e| panic!("{rel}: {e}"))
            .permissions()
            .mode()
    };
    assert_eq!(mode("etc/lodi/host.lock") & 0o004, 0o004, "the record");
    for dir in [
        "var/lib/lodi",
        "var/lib/lodi/host",
        "var/lib/lodi/host/journal",
    ] {
        assert_eq!(mode(dir) & 0o005, 0o005, "{dir}: {:o}", mode(dir));
    }
    let journals: Vec<_> = fs::read_dir(sw.path("var/lib/lodi/host/journal"))
        .unwrap()
        .flatten()
        .collect();
    assert!(!journals.is_empty());
    for journal in journals {
        let mode = journal.metadata().unwrap().permissions().mode();
        assert_eq!(mode & 0o004, 0o004, "{:?}: {mode:o}", journal.path());
    }
}

/// A managed file only root may read (mode 0000, which its owner cannot read without the
/// capability a root run has): the preview takes it as the record says, the root run reads it
/// unchanged, and the switch goes ahead instead of stopping as changed since the preview.
#[test]
fn a_file_only_root_can_read_is_checked_by_the_root_run() {
    let sw = Sw::new("sw-readable-secret");
    sw.el.reading(sw.uid, sw.gid);
    let secret = "[files.\"/etc/secret.conf\"]\ncontent = \"s\\n\"\nmode = \"0000\"\n\
                  owner = \"sample\"\ngroup = \"sample\"\n";
    sw.write_config(Some(&format!("{}\n{secret}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    assert!(
        fs::read(sw.path("etc/secret.conf")).is_err(),
        "the person cannot read it"
    );
    sw.write_config(
        Some(&format!("{}\n{secret}", host_with(&["bc", "zip"]))),
        None,
    );
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert!(
        err(&output).contains("= file /etc/secret.conf (unchanged)"),
        "{}",
        story(&output)
    );
    assert_eq!(sw.el.calls().len(), 2);
}

/// A hand edit of a root-only file, which the preview could not see, stops the root run before
/// it changes anything.
#[test]
fn drift_on_a_file_only_root_can_read_stops_the_root_run() {
    let sw = Sw::new("sw-readable-drift");
    sw.el.reading(sw.uid, sw.gid);
    let secret = "[files.\"/etc/secret.conf\"]\ncontent = \"s\\n\"\nmode = \"0000\"\n\
                  owner = \"sample\"\ngroup = \"sample\"\n";
    sw.write_config(Some(&format!("{}\n{secret}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    let at = sw.path("etc/secret.conf");
    fs::set_permissions(&at, fs::Permissions::from_mode(0o600)).unwrap();
    fs::write(&at, "edited\n").unwrap();
    fs::set_permissions(&at, fs::Permissions::from_mode(0o000)).unwrap();
    sw.write_config(
        Some(&format!("{}\n{secret}", host_with(&["bc", "zip"]))),
        None,
    );
    let output = sw.run(&["switch"]);
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains("secret.conf"), "{}", story(&output));
    assert!(!sw.installed().contains("zip"), "{}", story(&output));
}

/// Users and groups are planned from the root's `passwd` and `group`, which the person reads:
/// the preview names a new group without root, and the root run adds it.
#[test]
fn users_and_groups_are_previewed_as_the_person() {
    let sw = Sw::new("sw-readable-groups");
    sw.write_config(
        Some(&format!(
            "{}\n[groups.devs]\ngid = 3000\n",
            host_with(&["bc"])
        )),
        None,
    );
    let output = sw.run(&["switch", "--dry-run"]);
    ok(&output);
    assert!(err(&output).contains("+ group devs"), "{}", story(&output));
    assert!(sw.el.calls().is_empty());
    let output = sw.run(&["switch"]);
    ok(&output);
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
    let group = fs::read_to_string(sw.path("etc/group")).unwrap();
    assert!(group.contains("devs:x:3000:"), "{group}");
}

/// A preview as the person, which never asks for a password (LD-492): it goes ahead, names
/// `unread` as read by the root run, and calls no elevator.
#[track_caller]
fn previewed_naming(sw: &Sw, unread: &str) -> Output {
    sw.el.forget();
    let output = sw.run(&["switch", "--dry-run"]);
    ok(&output);
    let line = format!("{unread}: not readable as you; the root run reads it");
    assert!(err(&output).contains(&line), "{line}: {}", story(&output));
    assert!(sw.el.calls().is_empty(), "{}", story(&output));
    output
}

/// The live firewall table, which only a process with CAP_NET_ADMIN may list: the preview names
/// it, and the root run lists it, finds it as declared and goes ahead.
#[test]
fn the_firewall_table_is_listed_by_the_root_run() {
    let sw = Sw::new("sw-readable-firewall");
    sw.el.reading(sw.uid, sw.gid);
    sw.v.case
        .edit(|m| m["os"]["nft_needs_caps"] = serde_json::json!(true));
    let firewall = "\n[firewall]\nallow = [\"22/tcp\"]\n";
    sw.write_config(Some(&format!("{}{firewall}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    sw.write_config(
        Some(&format!("{}{firewall}", host_with(&["bc", "zip"]))),
        None,
    );
    previewed_naming(&sw, "table inet lodi");
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
}

/// lodi's netplan file, 0600 as netplan wants it (0000 here, which the person cannot read but a
/// root run can): the preview names it, and the root run reads it as written and goes ahead.
#[test]
fn a_netplan_file_only_root_can_read_is_read_by_the_root_run() {
    let sw = Sw::new("sw-readable-netplan");
    sw.el.reading(sw.uid, sw.gid);
    sw.v.case.root.write(
        "etc/netplan/50-cloud-init.yaml",
        "network:\n  version: 2\n  ethernets:\n    enp0s2:\n      dhcp4: true\n",
    );
    let network = "\n[network]\nconfirm_within = 2\n\n[network.interfaces.enp0s2]\n\
                   dhcp = true\naddresses = [\"10.0.2.50/24\"]\ngateway = \"10.0.2.2\"\n";
    sw.write_config(Some(&format!("{}{network}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    let file = sw.path("etc/netplan/90-lodi.yaml");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    sw.write_config(
        Some(&format!("{}{network}", host_with(&["bc", "zip"]))),
        None,
    );
    previewed_naming(&sw, "/etc/netplan/90-lodi.yaml");
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
}

/// Another account's `authorized_keys`, below a `.ssh` only it (and root) may enter: the
/// preview names it, and the root run reads it, finds the key there and goes ahead.
#[test]
fn another_account_s_authorized_keys_are_read_by_the_root_run() {
    let sw = Sw::new("sw-readable-keys");
    sw.el.reading(sw.uid, sw.gid);
    let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGxvZGktc3UtMS10ZXN0LWtleS1ub3QtcmVhbC0wMQ t";
    let users = format!("\n[users.sample]\nssh_keys = [\"{key}\"]\n");
    sw.write_config(Some(&format!("{}{users}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    let ssh = sw.path("home/.ssh");
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o000)).unwrap();
    sw.write_config(Some(&format!("{}{users}", host_with(&["bc", "zip"]))), None);
    previewed_naming(&sw, "/home/.ssh/authorized_keys");
    let output = sw.run(&["switch"]);
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    let keys = fs::read_to_string(ssh.join("authorized_keys")).unwrap();
    assert_eq!(
        keys.lines().filter(|line| *line == key).count(),
        1,
        "{keys}"
    );
}

/// The originals store (0700, root's on a machine; 0000 here): an `[etc]` entry kept there at
/// adoption leaves the manifest, the preview names its copy, and the root run puts it back.
#[test]
fn an_original_in_the_store_only_root_can_read_is_restored_by_the_root_run() {
    let sw = Sw::new("sw-readable-store");
    sw.el.reading(sw.uid, sw.gid);
    sw.v.case.root.write("etc/motd", "welcome\n");
    fs::set_permissions(sw.path("etc/motd"), fs::Permissions::from_mode(0o644)).unwrap();
    let motd = "\n[etc.\"motd\"]\ntext = \"welcome\\n\"\nowner = \"sample\"\n\
                group = \"sample\"\n";
    sw.write_config(Some(&format!("{}{motd}", host_with(&["bc"]))), None);
    ok(&sw.run(&["switch"]));
    let store = sw.path("etc/lodi/originals");
    let kept = fs::read_dir(store.join("etc"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    let copy = format!(
        "/etc/lodi/originals/etc/{}",
        kept.file_name().to_string_lossy()
    );
    fs::set_permissions(&store, fs::Permissions::from_mode(0o000)).unwrap();
    sw.write_config(Some(&host_with(&["bc", "zip"])), None);
    previewed_naming(&sw, &copy);
    let output = sw.run(&["switch"]);
    fs::set_permissions(&store, fs::Permissions::from_mode(0o700)).unwrap();
    ok(&output);
    assert!(sw.installed().contains("zip"), "{}", story(&output));
    assert_eq!(
        fs::read_to_string(sw.path("etc/motd")).unwrap(),
        "welcome\n"
    );
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
}

/// The EFI system partition, mounted root-only (`umask=0077`, as Ubuntu and Fedora mount it;
/// 0000 here): the preview names it and the timeout it sets, and the root run reads it and sets
/// the loader's timeout.
#[test]
fn an_esp_only_root_can_read_is_read_by_the_root_run() {
    let sw = Sw::new("sw-readable-esp");
    sw.el.reading(sw.uid, sw.gid);
    let root = &sw.v.case.root;
    root.write("sys/firmware/efi/efivars/.keep", "");
    root.write("boot/efi/EFI/BOOT/BOOTX64.EFI", "SHIM removable");
    root.write("boot/grub/grub.cfg", "menuentry 'Linux' {\n}\n");
    root.write("etc/default/grub", "GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\n");
    let esp = sw.path("boot/efi");
    fs::set_permissions(&esp, fs::Permissions::from_mode(0o000)).unwrap();
    let boot = "\n[boot]\nloader = \"grub\"\ntimeout = 3\n";
    sw.write_config(Some(&format!("{}{boot}", host_with(&["bc"]))), None);
    let preview = previewed_naming(&sw, "/boot/efi");
    assert!(err(&preview).contains("+ timeout 3"), "{}", story(&preview));
    let output = sw.run(&["switch"]);
    fs::set_permissions(&esp, fs::Permissions::from_mode(0o755)).unwrap();
    ok(&output);
    assert!(err(&output).contains("+ timeout 3"), "{}", story(&output));
    assert!(
        root.read("etc/default/grub").contains("GRUB_TIMEOUT=3"),
        "{}",
        story(&output)
    );
    assert_eq!(sw.el.calls().len(), 1, "{}", story(&output));
}

/// A switch with no host-part changes never elevates, even where an input is root's alone: the
/// host record says what lodi last wrote, and metadata that still matches it is unchanged
/// (#695, #709 story 2). `unread` makes the input root-only after a first switch wrote it.
#[track_caller]
fn root_only_and_unchanged(sw: &Sw, extra: &str, unread: impl Fn(&Sw)) {
    sw.el.reading(sw.uid, sw.gid);
    let host = format!("{}{extra}", host_with(&["bc"]));
    sw.write_config(Some(&host), Some(HOME));
    ok(&sw.run(&["switch"]));
    unread(sw);
    sw.el.forget();
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(
        err(&output).starts_with("nothing to switch"),
        "{}",
        story(&output)
    );
    assert!(sw.el.calls().is_empty(), "{}", story(&output));
    let edited = HOME.replace("bell-style none", "bell-style visible");
    sw.write_config(Some(&host), Some(&edited));
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(
        sw.el.calls().is_empty(),
        "a home-only edit: {}",
        story(&output)
    );
    // No root run, so nothing is said of what one would read.
    assert!(!err(&output).contains("root run"), "{}", story(&output));
    assert_eq!(
        fs::read_to_string(sw.path("home/.inputrc")).unwrap(),
        "set bell-style visible\n"
    );
}

/// Mode 0000 stands in for a 0600 file of root's: the person cannot read it, a root run can.
#[test]
fn a_root_only_file_as_recorded_never_elevates() {
    let sw = Sw::new("sw-unchanged-secret");
    let secret = "\n[files.\"/etc/secret.conf\"]\ncontent = \"s\\n\"\nmode = \"0000\"\n\
                  owner = \"sample\"\ngroup = \"sample\"\n";
    root_only_and_unchanged(&sw, secret, |_| {});
}

#[test]
fn a_firewall_table_as_recorded_never_elevates() {
    let sw = Sw::new("sw-unchanged-firewall");
    sw.v.case
        .edit(|m| m["os"]["nft_needs_caps"] = serde_json::json!(true));
    root_only_and_unchanged(&sw, "\n[firewall]\nallow = [\"22/tcp\"]\n", |_| {});
}

#[test]
fn a_root_only_esp_as_recorded_never_elevates() {
    let sw = Sw::new("sw-unchanged-esp");
    let root = &sw.v.case.root;
    root.write("sys/firmware/efi/efivars/.keep", "");
    root.write("boot/efi/EFI/BOOT/BOOTX64.EFI", "SHIM removable");
    root.write("boot/grub/grub.cfg", "menuentry 'Linux' {\n}\n");
    root.write("etc/default/grub", "GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\n");
    let boot = "\n[boot]\nloader = \"grub\"\ntimeout = 3\n";
    root_only_and_unchanged(&sw, boot, |sw| {
        let esp = sw.path("boot/efi");
        fs::set_permissions(&esp, fs::Permissions::from_mode(0o000)).unwrap();
    });
    fs::set_permissions(sw.path("boot/efi"), fs::Permissions::from_mode(0o755)).unwrap();
}

/// `LODI_HOME`, else `XDG_DATA_HOME`, holds the home part's records, and `XDG_CONFIG_HOME` is
/// where `[home.xdg_config]` writes, as the environment table says (#700 story 19).
#[test]
fn the_data_and_config_variables_move_the_home_part_s_files() {
    let sw = Sw::new("sw-xdg");
    let home = "[home]\nversion = \"1\"\n\n[home.xdg_config.\"app/rc\"]\ntext = \"x\\n\"\n";
    sw.write_config(None, Some(home));
    let xdg = sw.path("home/xdg");
    mkdir(&xdg, 0o755);
    let xdg = xdg.display().to_string();
    let output = sw.run_env(&["switch"], &[("XDG_CONFIG_HOME", &xdg)]);
    ok(&output);
    assert!(sw.path("home/xdg/app/rc").exists(), "{}", story(&output));
    assert!(!sw.path("home/.config/app").exists(), "{}", story(&output));
    let state = "home-scope/state.json";
    assert!(
        sw.path("lodi-home").join(state).exists(),
        "{}",
        story(&output)
    );
    assert!(!sw.path("home/.local/share/lodi").exists());
    let data = sw.path("data").display().to_string();
    let output = sw.run_env(
        &["switch"],
        &[
            ("XDG_CONFIG_HOME", &xdg),
            ("LODI_HOME", ""),
            ("XDG_DATA_HOME", &data),
        ],
    );
    ok(&output);
    assert!(
        sw.path("data/lodi").join(state).exists(),
        "{}",
        story(&output)
    );
    assert!(!sw.path("home/.local/share/lodi").exists());
}

/// `--host NAME` on a flat config, which has one host and no `config.toml`, is a usage error
/// (exit 2).
#[test]
fn a_host_flag_on_a_flat_config_is_a_usage_error() {
    let sw = Sw::new("sw-flat-host");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    for verb in ["update", "pin", "unpin"] {
        let output = sw.run(&[verb, "--host", "box"]);
        assert_eq!(output.status.code(), Some(2), "{}", story(&output));
    }
}

/// The marker is an empty file (#705): one with text in it does not arm the host.
#[test]
fn a_marker_with_text_in_it_does_not_arm_the_host() {
    let sw = Sw::new("sw-marker-text");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    fs::write(sw.path("etc/lodi/may-manage"), "armed\n").unwrap();
    refused_untouched(&sw, &["switch"], "E_HOST_NOT_ARMED");
}

/// With no `git` program a config in git still switches: lodi warns it cannot tell the commit
/// and goes on (#695).
#[test]
fn a_machine_with_no_git_program_still_switches() {
    let sw = Sw::new("sw-no-git");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    sw.git(&["init", "-q"]);
    sw.git(&["add", "-A"]);
    sw.git(&["commit", "-q", "-m", "c"]);
    fs::remove_file(sw.v.case.root.dir.join("tools/git")).unwrap();
    let output = sw.run(&["switch"]);
    ok(&output);
    assert!(sw.installed().contains("zip"));
    assert!(
        err(&output).contains("lodi: warning: "),
        "{}",
        story(&output)
    );
}

/// A `--update` that fails switches nothing and leaves the lock as it was (#695 story 33).
#[test]
fn a_failed_update_keeps_the_lock() {
    let sw = Sw::new("sw-update-fails");
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    ok(&sw.run(&["switch"]));
    let lock = sw.config().join("lodi.lock");
    let before = fs::read(&lock).unwrap();
    sw.write_config(Some(&host_with(&["bc", "zip", "nosuch"])), Some(HOME));
    let output = sw.run(&["switch", "--update"]);
    assert!(!output.status.success(), "{}", story(&output));
    assert_eq!(fs::read(&lock).unwrap(), before, "{}", story(&output));
    assert!(!sw.installed().contains("nosuch"));
}

/// The root run plans the host part once (#702's open note): the package index it reads to
/// check the preview is the one it applies.
#[test]
fn the_root_run_plans_the_host_part_once() {
    let sw = Sw::new("sw-plans-once");
    sw.el.reading(sw.uid, sw.gid);
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(HOME));
    let _ = fs::remove_file(sw.v.case.base().join("state/log"));
    let output = sw.run(&["switch"]);
    ok(&output);
    let plans = sw.v.case.log();
    let plans = plans.iter().filter(|call| call.ends_with("-- bc zip"));
    // The person's preview, the root run's check of it, and the plan under the apply lock
    // (LD-379): no fourth.
    assert_eq!(plans.count(), 3, "{}", story(&output));
}

// ----------------------------------------------------------------------- lingering ---

/// A user manager beside the root: `systemctl` keeps unit states in `sd`, and `loginctl` acts as
/// polkit lets it on Arch: anyone may read lingering, only root may change it. Root is uid 0, or
/// the copy in `rootbin`, which only the root run's `PATH` holds (the stub elevator puts it there).
/// Every call is logged with who made it.
struct Manager {
    bin: PathBuf,
    rootbin: PathBuf,
    sd: PathBuf,
    run: PathBuf,
}

const FAKE_SYSTEMCTL: &str = r#"#!/bin/sh
sd=@SD@
echo "uid=$(@ID@ -u) systemctl $*" >> "$sd/calls"
shift
verb=$1
shift
unit=
for a in "$@"; do case $a in --now|--) ;; *) unit=$a ;; esac; done
s=disabled
[ -e "$sd/state/$unit" ] && read -r s < "$sd/state/$unit"
case $verb in
  daemon-reload) ;;
  is-enabled) echo "$s"; [ "$s" = enabled ] ;;
  is-active) [ "$s" = enabled ] && echo active || { echo inactive; exit 3; } ;;
  enable) echo enabled > "$sd/state/$unit" ;;
  disable) echo disabled > "$sd/state/$unit" ;;
  *) exit 64 ;;
esac
"#;

const FAKE_LOGINCTL: &str = r#"#!/bin/sh
sd=@SD@
by=person
{ [ "$(@ID@ -u)" = 0 ] || [ -e "${0%/loginctl}/root" ]; } && by=root
echo "by=$by loginctl $*" >> "$sd/calls"
l=no
[ -e "$sd/linger" ] && read -r l < "$sd/linger"
case $1 in
  show-user) echo "$l" ;;
  enable-linger|disable-linger)
    [ $by = root ] || { echo "Could not ${1%-linger} linger: Access denied" >&2; exit 1; }
    [ $1 = enable-linger ] && echo yes > "$sd/linger" || echo no > "$sd/linger" ;;
  *) exit 64 ;;
esac
"#;

impl Manager {
    fn new(sw: &Sw) -> Manager {
        let at = |rel: &str| sw.path(&format!("manager/{rel}"));
        let m = Manager {
            bin: at("bin"),
            rootbin: at("rootbin"),
            sd: at("sd"),
            run: at("run"),
        };
        mkdir(&m.bin, 0o755);
        mkdir(&m.rootbin, 0o755);
        // The dropped child is another uid: it writes the fake's state too.
        mkdir(&m.sd.join("state"), 0o777);
        fs::set_permissions(&m.sd, fs::Permissions::from_mode(0o777)).unwrap();
        mkdir(&m.run.join("systemd"), 0o755);
        write(&m.run.join("systemd/private"), "");
        let id = which("id").expect("id").display().to_string();
        let sd = m.sd.display().to_string();
        for (dir, name, body) in [
            (&m.bin, "systemctl", FAKE_SYSTEMCTL),
            (&m.bin, "loginctl", FAKE_LOGINCTL),
            (&m.rootbin, "loginctl", FAKE_LOGINCTL),
        ] {
            let path = dir.join(name);
            fs::write(&path, body.replace("@ID@", &id).replace("@SD@", &sd)).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
        }
        write(&m.rootbin.join("root"), "");
        write(&m.sd.join("calls"), "");
        fs::set_permissions(m.sd.join("calls"), fs::Permissions::from_mode(0o666)).unwrap();
        m
    }

    /// The `PATH` and runtime folder a switch runs with.
    fn envs(&self, sw: &Sw) -> Vec<(&'static str, String)> {
        let path = sw.el.path(&[&self.bin, &sw.v.case.base().join("bin")]);
        let run = self.run.display().to_string();
        vec![("PATH", path), ("XDG_RUNTIME_DIR", run)]
    }

    fn lingering(&self) -> bool {
        fs::read_to_string(self.sd.join("linger")).is_ok_and(|l| l.trim() == "yes")
    }

    /// The calls that change lingering.
    fn linger_calls(&self) -> Vec<String> {
        fs::read_to_string(self.sd.join("calls"))
            .unwrap()
            .lines()
            .filter(|call| call.contains("able-linger"))
            .map(str::to_string)
            .collect()
    }
}

const LINGER: &str = "[services.\"a.service\"]\nenable = true\nlinger = true\n";
const NO_LINGER: &str = "[services.\"a.service\"]\nenable = true\n";

/// `lodi switch` typed as the person, with host changes: the root run turns lingering on and
/// off for them, never their own process, which polkit refuses with no local session.
#[test]
fn self_elevated_the_root_run_changes_lingering() {
    let sw = Sw::new("sw-linger-elevated");
    let m = Manager::new(&sw);
    let before = format!("PATH='{}':\"$PATH\"", m.rootbin.display());
    sw.el.install(&ELEVATORS, false, &before);
    let envs = m.envs(&sw);
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    sw.write_config(Some(&host_with(&["bc", "zip"])), Some(LINGER));
    let output = sw.run_env(&["switch"], &envs);
    ok(&output);
    let line = format!("+ linger (loginctl enable-linger {})", sw.uid);
    assert!(err(&output).contains(&line), "{}", story(&output));
    assert!(m.lingering(), "{}", story(&output));
    let enabled = format!("by=root loginctl enable-linger {}", sw.uid);
    assert_eq!(m.linger_calls(), [enabled], "{}", story(&output));

    sw.write_config(Some(&host_with(&["bc"])), Some(NO_LINGER));
    let output = sw.run_env(&["switch"], &envs);
    ok(&output);
    assert!(!m.lingering(), "{}", story(&output));
    let disabled = format!("by=root loginctl disable-linger {}", sw.uid);
    assert_eq!(m.linger_calls()[1..], [disabled], "{}", story(&output));
}

/// The person of [`Mapped`]: a uid of the namespace's own, not root's.
const PERSON: &str = "1000";

/// A scratch root whose home belongs to [`PERSON`] in a user namespace that maps the invoking
/// user's subordinate ids (`unshare --map-auto`), so that a switch under sudo there really drops
/// to another uid for the home part. Dropped, it gives every file back to the invoking user.
struct Mapped {
    root: PathBuf,
}

impl Mapped {
    fn unshare() -> Command {
        let mut command = Command::new(which("unshare").expect("unshare"));
        command.args(["--map-auto", "--map-root-user"]);
        command
    }

    /// `/bin/sh` as root of the namespace, with `envs`' `PATH`: `unshare` itself finds
    /// `newuidmap` on the test's own.
    fn sudo(envs: &[(&str, &str)]) -> Command {
        let path = envs.iter().find(|(k, _)| *k == "PATH").expect("a PATH").1;
        let mut command = Command::new(which("env").expect("env"));
        command
            .arg(format!("PATH={}", std::env::var("PATH").unwrap()))
            .arg(which("unshare").expect("unshare"))
            .args(["--map-auto", "--map-root-user", "env"])
            .arg(format!("PATH={path}"))
            .arg("/bin/sh");
        command
    }

    fn new(sw: &Sw) -> Mapped {
        let status = Mapped::unshare()
            .args(["chown", "-R", &format!("{PERSON}:{PERSON}")])
            .arg(sw.path("home"))
            .status()
            .expect("unshare runs");
        assert!(status.success(), "this machine maps no subordinate ids");
        Mapped {
            root: sw.v.case.root.dir.clone(),
        }
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        let _ = Mapped::unshare()
            .args(["chown", "-R", "0:0"])
            .arg(&self.root)
            .status();
    }
}

/// A switch under sudo: root turns lingering on before the home part, which runs dropped to the
/// person, and off after it; the dropped child never asks.
#[test]
fn under_sudo_root_changes_lingering_not_the_dropped_child() {
    // The person is another uid: its root must be one it can reach wherever the checkout is.
    let _base = hostroot::Traversable::new();
    let sw = Sw::new("sw-linger-sudo");
    let m = Manager::new(&sw);
    sw.write_config(None, Some(LINGER));
    mkdir(&sw.path("root-home"), 0o700);
    let mapped = Mapped::new(&sw);
    let (uid, gid) = (sw.uid.to_string(), sw.gid.to_string());
    let root_home = sw.path("root-home").display().to_string();
    let mut envs = m.envs(&sw);
    envs.extend([("SUDO_UID", uid), ("SUDO_GID", gid), ("HOME", root_home)]);
    let envs: Vec<(&str, &str)> = envs.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let output = sw.run_by(Mapped::sudo(&envs), &["switch"], &envs, Input::Pipe(""));
    ok(&output);
    let line = format!("+ linger (loginctl enable-linger {PERSON})");
    assert!(err(&output).contains(&line), "{}", story(&output));
    assert!(m.lingering(), "{}", story(&output));
    let enabled = format!("by=root loginctl enable-linger {PERSON}");
    assert_eq!(m.linger_calls(), [enabled], "{}", story(&output));

    drop(mapped);
    sw.write_config(None, Some(NO_LINGER));
    let _mapped = Mapped::new(&sw);
    let output = sw.run_by(Mapped::sudo(&envs), &["switch"], &envs, Input::Pipe(""));
    ok(&output);
    assert!(!m.lingering(), "{}", story(&output));
    let disabled = format!("by=root loginctl disable-linger {PERSON}");
    assert_eq!(m.linger_calls()[1..], [disabled], "{}", story(&output));
}
