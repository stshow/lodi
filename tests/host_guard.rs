//! The host guard of LD-376: with `LODI_HOST_REQUIRE_ROOT=1` in the environment, every `lodi
//! host` verb given without `--root` is refused before it reads anything, and with `--root DIR`
//! it behaves exactly as without the variable.
//!
//! A verb without `--root` is aimed at `/`, and no host code in this repository is ever run
//! against this machine (`AGENTS.md` §8). So every rootless invocation below runs inside a
//! `bwrap` sandbox whose `/etc` and `/var` are a scratch fixture and whose every other path is
//! bound read-only, as uid 0 of an unprivileged user namespace, with no network and a `PATH` that
//! holds no program. The fixture is armed and says `ID=loud-fixture`: a verb that got past the
//! guard would read it and say so (or, for `arm`, find it armed and succeed), which is the
//! failure the refusal is checked against. `strace` then shows that the refused process opened
//! and examined nothing under the fixture, nor `/` itself.
//!
//! Every other invocation carries a scratch `--root` (`tests/support/hostroot.rs`).

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hostroot::{Root, ids};

const VAR: &str = "LODI_HOST_REQUIRE_ROOT";
const CODE: &str = "E_HOST_ROOT_REQUIRED";

/// Every verb `lodi host` has, with the flag shapes that reach a different code path.
const VERBS: &[&[&str]] = &[
    &["plan"],
    &["apply"],
    &["apply", "--no-update"],
    &["import"],
    &["import", "--stdout"],
    &["import", "--out", "/tmp/lodi-guard-never-written"],
    &["arm"],
];

fn scratch(name: &str) -> PathBuf {
    support::scratch(&format!("guard-{name}"))
}

fn tool(name: &str) -> PathBuf {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| {
            panic!(
                "`{name}` is required by this test and is not on PATH: the rootless verbs are \
                 only ever run inside a sandbox, never against this machine"
            )
        })
}

/// The loud fixture: an armed Debian-shaped `/etc` that names itself, and a `/var`.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = scratch(name);
        for (rel, body) in [
            ("etc/lodi/host-allowed", ""),
            (
                "etc/os-release",
                "ID=loud-fixture\nNAME=\"read by a guarded verb\"\n",
            ),
            (
                "etc/lodi/host.toml",
                "[files.\"/etc/loud.conf\"]\ncontent = \"loud\\n\"\n",
            ),
            ("var/lib/dpkg/status", ""),
        ] {
            let path = dir.join(rel);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, body).unwrap();
        }
        fs::create_dir_all(dir.join("empty-path")).unwrap();
        fs::create_dir_all(dir.join("trace")).unwrap();
        Fixture { dir }
    }

    /// Every path below the fixture with its size, mode and modification time.
    fn snapshot(&self) -> BTreeMap<PathBuf, (u64, u32, i64, i64)> {
        fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (u64, u32, i64, i64)>) {
            for entry in fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let meta = fs::symlink_metadata(&path).unwrap();
                out.insert(
                    path.clone(),
                    (meta.len(), meta.mode(), meta.mtime(), meta.mtime_nsec()),
                );
                if meta.is_dir() {
                    walk(&path, out);
                }
            }
        }
        let mut out = BTreeMap::new();
        walk(&self.dir.join("etc"), &mut out);
        walk(&self.dir.join("var"), &mut out);
        out
    }

    /// `lodi host <args>` with no `--root`, inside the sandbox, optionally under `strace`.
    fn run(&self, args: &[&str], setting: Option<&str>, trace: bool) -> Output {
        // A fresh tmpfs is `/`, owned by the namespace's uid 0 as a real root is; the fixture is
        // its `/etc` and `/var`; the programs' own files are bound read-only; `/trace` is the one
        // other writable place, for the strace log.
        let mut command = Command::new(tool("bwrap"));
        command
            .args([
                "--unshare-user",
                "--uid",
                "0",
                "--gid",
                "0",
                "--unshare-net",
            ])
            .args(["--tmpfs", "/"]);
        for system in ["/nix", "/usr", "/lib", "/lib64", "/bin", "/sbin"] {
            if Path::new(system).exists() {
                command.args(["--ro-bind", system, system]);
            }
        }
        command
            .arg("--bind")
            .arg(self.dir.join("etc"))
            .arg("/etc")
            .arg("--bind")
            .arg(self.dir.join("var"))
            .arg("/var")
            .arg("--bind")
            .arg(self.dir.join("trace"))
            .arg("/trace")
            .arg("--ro-bind")
            .arg(env!("CARGO_BIN_EXE_lodi"))
            .arg("/lodi")
            .arg("--ro-bind")
            .arg(self.dir.join("empty-path"))
            .arg("/empty-path")
            .arg("--dir")
            .arg("/home")
            .arg("--die-with-parent")
            .arg("--clearenv")
            .args([
                "--setenv",
                "PATH",
                "/empty-path",
                "--setenv",
                "HOME",
                "/home",
            ]);
        if let Some(value) = setting {
            command.args(["--setenv", VAR, value]);
        }
        command.arg("--");
        if trace {
            command
                .arg(fs::canonicalize(tool("strace")).expect("strace resolves"))
                .args([
                    "-f",
                    "-qq",
                    "-e",
                    "trace=%file,%desc",
                    "-o",
                    "/trace/strace.log",
                ]);
        }
        command
            .arg("/lodi")
            .arg("host")
            .args(args)
            .output()
            .expect("bwrap runs")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// G1, the refusal. Without the guard each verb here reaches the fixture: `plan`, `apply` and
/// `import` read the marker and then the `os-release` that names the fixture, and `arm` finds the
/// marker in place and succeeds.
#[test]
fn every_host_verb_without_root_is_refused_before_it_reads_anything() {
    let fixture = Fixture::new("refused");
    let before = fixture.snapshot();
    for verb in VERBS {
        // check-host-safety: refusal — inside the sandbox, whose `/etc` and `/var` are the fixture.
        let out = fixture.run(verb, Some("1"), false);
        let (stdout, stderr) = (text(&out.stdout), text(&out.stderr));
        assert_eq!(
            out.status.code(),
            Some(9),
            "{verb:?} under {VAR}=1 without --root: stdout {stdout:?} stderr {stderr:?}"
        );
        assert!(
            stderr.starts_with(&format!("lodi: error {CODE}: ")),
            "{verb:?}: {stderr}"
        );
        assert!(
            stderr.contains("--root"),
            "{verb:?} names the flag: {stderr}"
        );
        assert!(stdout.is_empty(), "{verb:?} printed {stdout:?}");
        assert!(
            !stderr.contains("loud-fixture") && !stderr.contains("E_HOST_NOT_ARMED"),
            "{verb:?} reached the root: {stderr}"
        );
    }
    assert_eq!(fixture.snapshot(), before, "nothing in the fixture changed");
    let _ = fs::remove_dir_all(&fixture.dir);
}

/// G1, "before anything is read": the refused process makes no file system call on `/` itself,
/// on the arming marker or anything else in lodi's directory of the root's `etc`, on the root's
/// `var/lib`, or on its `os-release`.
#[test]
fn a_refused_verb_opens_no_path_of_the_root() {
    let fixture = Fixture::new("traced");
    for verb in VERBS {
        let log = fixture.dir.join("trace/strace.log");
        let _ = fs::remove_file(&log);
        // check-host-safety: refusal — inside the sandbox, whose `/etc` and `/var` are the fixture.
        let out = fixture.run(verb, Some("1"), true);
        assert_eq!(
            out.status.code(),
            Some(9),
            "{verb:?}: {}",
            text(&out.stderr)
        );
        let trace = fs::read_to_string(&log).expect("strace wrote its log");
        assert!(
            trace.contains("execve("),
            "the trace saw the process at all: {trace}"
        );
        // The one line that names the arguments is the exec that started the process.
        for line in trace.lines().filter(|line| !line.contains("execve(")) {
            for touched in [
                "\"/\"",
                "\"/etc/lodi",
                "\"/etc/os-release",
                "\"/var/lib",
                "\"/etc/passwd",
                "\"/etc/group",
                "\"/tmp/lodi-guard-never-written",
            ] {
                assert!(
                    !line.contains(touched),
                    "{verb:?} touched {touched} before it was refused: {line}"
                );
            }
        }
    }
    let _ = fs::remove_dir_all(&fixture.dir);
}

/// G1, "the variable unset or not `1` changes nothing": in the same sandbox, with the variable
/// absent or set to anything but `1`, `plan` goes on to read the fixture exactly as it always
/// did. This is also the proof that the fixture is loud.
#[test]
fn any_other_setting_is_no_guard_at_all() {
    let fixture = Fixture::new("unset");
    for setting in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("yes"),
        Some(" 1"),
        Some("1 "),
    ] {
        // check-host-safety: refusal — inside the sandbox, whose `/etc` and `/var` are the fixture.
        let out = fixture.run(&["plan"], setting, false);
        let stderr = text(&out.stderr);
        assert!(
            !stderr.contains(CODE),
            "{VAR}={setting:?} is not the guard: {stderr}"
        );
        assert!(
            stderr.contains("loud-fixture"),
            "{VAR}={setting:?}: plan read the root as before: {stderr}"
        );
    }
    let _ = fs::remove_dir_all(&fixture.dir);
}

/// What one run showed: its status, standard output, standard error and the tree it left.
type Observed = (Option<i32>, String, String, Vec<(String, u64, u32)>);

/// One verb against a fresh scratch root prepared by `prepare`, with the variable set as given.
/// Returns what the process printed, its status and the tree it left, with the root's own path
/// spelled `<root>` so that two roots compare.
fn with_root(name: &str, args: &[&str], setting: Option<&str>, prepare: fn(&Root)) -> Observed {
    let root = Root::new(name);
    prepare(&root);
    let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
    command.arg("host").args(args).arg("--root").arg(&root.dir);
    command.env_remove(VAR);
    if let Some(value) = setting {
        command.env(VAR, value);
    }
    let out = command.output().expect("lodi runs");
    // A journal is named by its moment and a random tag; that name is all that may differ.
    let spell = |s: &[u8]| {
        text(s)
            .replace(&root.dir.display().to_string(), "<root>")
            .lines()
            .map(|line| match line.strip_prefix("journal ") {
                Some(_) => "journal <id>\n".to_string(),
                None => format!("{line}\n"),
            })
            .collect::<String>()
    };
    let mut tree = Vec::new();
    fn walk(base: &Path, dir: &Path, tree: &mut Vec<(String, u64, u32)>) {
        let mut entries: Vec<_> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for path in entries {
            let meta = fs::symlink_metadata(&path).unwrap();
            let rel = path.strip_prefix(base).unwrap().display().to_string();
            // A journal entry is named by its moment and carries that name; its presence and mode
            // are what compare.
            let (rel, len) = if rel.contains("/journal/") {
                ("journal-entry".to_string(), 0)
            } else {
                (rel, if meta.is_dir() { 0 } else { meta.len() })
            };
            tree.push((rel, len, meta.mode()));
            if meta.is_dir() {
                walk(base, &path, tree);
            }
        }
    }
    walk(&root.dir, &root.dir, &mut tree);
    (
        out.status.code(),
        spell(&out.stdout),
        spell(&out.stderr),
        tree,
    )
}

fn files_manifest(root: &Root) {
    let (uid, gid) = ids(root);
    root.debian_with(&format!(
        "[files.\"/etc/guard.conf\"]\ncontent = \"ok\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
}

fn unarmed(root: &Root) {
    root.debian();
}

fn armed_debian(root: &Root) {
    root.arm().debian();
}

/// G1, "with `--root DIR` the verbs behave exactly as without the variable": each verb, against
/// two identical scratch roots, prints the same, exits the same and leaves the same tree.
#[test]
fn with_root_every_verb_behaves_exactly_as_without_the_variable() {
    type Case = (&'static str, &'static [&'static str], fn(&Root));
    let cases: &[Case] = &[
        ("plan", &["plan"], files_manifest),
        ("apply", &["apply"], files_manifest),
        ("unarmed-plan", &["plan"], unarmed),
        ("import-stdout", &["import", "--stdout"], armed_debian),
        ("import-land", &["import"], armed_debian),
        ("arm", &["arm"], unarmed),
    ];
    for (name, args, prepare) in cases {
        let plain = with_root(&format!("{name}-plain"), args, None, *prepare);
        for setting in ["1", "0"] {
            let guarded = with_root(&format!("{name}-{setting}"), args, Some(setting), *prepare);
            assert_eq!(
                guarded,
                plain,
                "`lodi host {}` with --root under {VAR}={setting} differs from without it",
                args.join(" ")
            );
        }
    }
}
