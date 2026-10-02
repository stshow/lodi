//! The host guard of LD-376: with `LODI_HOST_REQUIRE_ROOT=1` in the environment, every command
//! that reaches the host scope (`lodi switch`, `import`, `update`, `pin` and `unpin`) given
//! without `--root` is refused before it reads anything, and with `--root DIR` it behaves exactly
//! as without the variable.
//!
//! A verb without `--root` is aimed at `/`, and no host code in this repository is ever run
//! against this machine (`AGENTS.md` §8). So every rootless invocation below runs inside a
//! `bwrap` sandbox whose `/etc` and `/var` are a scratch fixture and whose every other path is
//! bound read-only, as uid 0 of an unprivileged user namespace, with no network and a `PATH` that
//! holds no program. The fixture carries the may-manage marker, a config in its `HOME`, and says
//! `ID=loud-fixture`: a command that got past the guard would read it and say so, which is the
//! failure the refusal is checked against. `strace` then shows that the refused process opened
//! and examined nothing under the fixture, nor `/` itself.
//!
//! Every other invocation carries a scratch `--root` with a fake machine behind it
//! (`tests/support/fakehost.rs`, LD-514, LD-522).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

/// The real binary on a pseudo-terminal, for a switch run without `fakehost`'s variable.
#[allow(clippy::duplicate_mod)]
#[path = "support/terminal.rs"]
mod terminal;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use hostroot::ids;

const VAR: &str = "LODI_HOST_REQUIRE_ROOT";
const CODE: &str = "E_HOST_ROOT_REQUIRED";

/// Every command that reaches the host scope, with the flag shapes that reach a different code
/// path.
const VERBS: &[&[&str]] = &[
    &["switch"],
    &["switch", "--host"],
    &["switch", "--dry-run"],
    &["switch", "/tmp/lodi-guard-never-written"],
    &["import"],
    &["import", "--yes"],
    &["import", "--dry-run"],
    &["update"],
    &["pin", "bc"],
    &["unpin", "bc"],
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

/// The loud fixture: a Debian-shaped `/etc` with the may-manage marker that names itself, a
/// `/var`, and a `HOME` whose config declares a file.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        let dir = scratch(name);
        for (rel, body) in [
            ("etc/lodi/may-manage", ""),
            (
                "etc/os-release",
                "ID=loud-fixture\nNAME=\"read by a guarded verb\"\n",
            ),
            ("etc/hostname", "loud\n"),
            (
                "home/.config/lodi/host.toml",
                "[host]\nversion = \"1\"\n\n[system]\nhostname = \"loud\"\n\n\
                 [files.\"/etc/loud.conf\"]\ncontent = \"loud\\n\"\n",
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

    /// `lodi <args>` with no `--root`, inside the sandbox, optionally under `strace`.
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
                "--unshare-pid",
            ])
            .args(["--tmpfs", "/"])
            // lodi finds its own binary through `/proc/self/exe`.
            .args(["--proc", "/proc"]);
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
            .arg("--bind")
            .arg(self.dir.join("home"))
            .arg("/home")
            // A folder with no project in it: a bare `update` looks for `./lodi.toml` first.
            .args(["--chdir", "/empty-path"])
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
            .args(args)
            .output()
            .expect("bwrap runs")
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// G1, the refusal. Without the guard each command here reaches the fixture: a switch and an
/// import read the config and then the `os-release` that names the fixture.
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
/// on the may-manage marker or anything else in lodi's directory of the root's `etc`, on the
/// root's `var/lib`, on its `os-release`, or on the config in its `HOME`.
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
            let line = own_paths_spelled_out(line);
            for touched in [
                "\"/\"",
                "\"/etc/lodi",
                "\"/etc/os-release",
                "\"/var/lib",
                "\"/etc/passwd",
                "\"/etc/group",
                "\"/tmp/lodi-guard-never-written",
                "\"/home",
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

/// `line` with every path below the checkout or the target directory spelled `"<own>/…"`. Before
/// lodi's `main` runs, the dynamic loader looks for its libraries along the binary's own `RUNPATH`,
/// and a build in the flake's development shell puts that shell's `$out/lib`, a folder of the
/// checkout, first there. None of those is a path of the root, but a checkout below `/home` spells
/// them `"/home…` (LD-541). Every other path, `/home` itself and the root's `HOME` included, is
/// left as it was.
fn own_paths_spelled_out(line: &str) -> String {
    let exe = Path::new(env!("CARGO_BIN_EXE_lodi"));
    let target = exe
        .parent()
        .and_then(Path::parent)
        .expect("the target directory");
    let mut line = line.to_string();
    for dir in [Path::new(env!("CARGO_MANIFEST_DIR")), target] {
        for spelled in [dir.to_path_buf()]
            .into_iter()
            .chain(fs::canonicalize(dir).ok())
        {
            line = line.replace(&format!("\"{}/", spelled.display()), "\"<own>/");
        }
    }
    line
}

/// G1, "the variable unset or not `1` changes nothing": in the same sandbox, with the variable
/// absent or set to anything but `1`, a dry-run import goes on to read the fixture exactly as it
/// always did. This is also the proof that the fixture is loud.
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
        let out = fixture.run(&["import", "--dry-run"], setting, false);
        let stderr = text(&out.stderr);
        assert!(
            !stderr.contains(CODE),
            "{VAR}={setting:?} is not the guard: {stderr}"
        );
        assert!(
            stderr.contains("loud-fixture"),
            "{VAR}={setting:?}: the import read the root as before: {stderr}"
        );
    }
    let _ = fs::remove_dir_all(&fixture.dir);
}

/// What one run showed: its status, standard output, standard error and the tree it left.
type Observed = (Option<i32>, String, String, Vec<(String, u64, u32)>);

/// One command against a fresh fake machine prepared by `prepare`, with the variable set as
/// given (`fakehost::Case::command` sets it to `1`; it is taken out or replaced here), on a
/// terminal as a person runs it. Returns what the process printed, its status and the tree it
/// left, with the root's and the fake's own paths spelled `<root>` and `<fake>` so that two
/// roots compare.
fn with_root(
    name: &str,
    verb: &str,
    setting: Option<&str>,
    prepare: fn(&fakehost::Case),
) -> Observed {
    let case = fakehost::Case::new(name, fakehost::Machine::debian());
    prepare(&case);
    let mut command = case.command(verb, &[], &[]);
    command.env_remove(VAR);
    if let Some(value) = setting {
        command.env(VAR, value);
    }
    if case.root.exists(UNARMED) {
        // Unarmed: the may-manage marker `command` writes is taken away again.
        fs::remove_file(case.root.path(UNARMED)).expect("the note");
        fs::remove_file(case.root.path(lodi::marker::MARKER)).expect("the marker");
    }
    let (out, shown) = {
        let _spawning = fakehost::spawning();
        terminal::on_terminal("", move |stdin, stderr| {
            let mut command = command;
            command
                .stdin(stdin)
                .stderr(stderr)
                .output()
                .expect("lodi runs")
        })
    };
    let root = &case.root;
    // A journal is named by its moment and a random tag; that name is all that may differ.
    let spell = |s: &[u8]| {
        text(s)
            .replace("\r\n", "\n")
            .replace(&root.dir.display().to_string(), "<root>")
            .replace(&case.base().display().to_string(), "<fake>")
            .lines()
            .map(|line| match line.strip_prefix("journal ") {
                Some(_) => "journal <id>\n".to_string(),
                None => format!("{}\n", timeless(line)),
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
            } else if rel.contains("/logs/") {
                // A switch's log is named by its moment too, and says how long it took.
                ("log-entry".to_string(), 0)
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
    (out.status.code(), spell(&out.stdout), spell(&shown), tree)
}

/// `line` with every duration (`2.4s`, `13s`) spelled `<t>`: how long a step took is all that may differ.
fn timeless(line: &str) -> String {
    let mut out = String::new();
    for word in line.split(' ') {
        let (number, rest) = word.split_at(
            word.find(|c: char| !c.is_ascii_digit() && c != '.')
                .unwrap_or(word.len()),
        );
        if !number.is_empty() && rest.starts_with('s') {
            out.push_str("<t>");
            out.push_str(&rest[1..]);
        } else {
            out.push_str(word);
        }
        out.push(' ');
    }
    out.pop();
    out
}

fn files_manifest(case: &fakehost::Case) {
    let (uid, gid) = ids(&case.root);
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\n\n[files.\"/etc/guard.conf\"]\ncontent = \"ok\\n\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
}

/// The note [`unarmed`] leaves for [`with_root`], which takes the marker away again after the
/// command is built.
const UNARMED: &str = "unarmed";

fn unarmed(case: &fakehost::Case) {
    files_manifest(case);
    case.root.write(UNARMED, "");
}

fn armed_debian(_: &fakehost::Case) {}

/// G1, "with `--root DIR` the commands behave exactly as without the variable": each one,
/// against two identical scratch roots, prints the same, exits the same and leaves the same tree.
#[test]
fn with_root_every_verb_behaves_exactly_as_without_the_variable() {
    type Case = (&'static str, &'static str, fn(&fakehost::Case));
    let cases: &[Case] = &[
        ("plan", "plan", files_manifest),
        ("apply", "apply", files_manifest),
        ("unarmed-plan", "plan", unarmed),
        ("import-land", "import", armed_debian),
    ];
    for (name, verb, prepare) in cases {
        let plain = with_root(&format!("guard-{name}-plain"), verb, None, *prepare);
        for setting in ["1", "0"] {
            let guarded = with_root(
                &format!("guard-{name}-{setting}"),
                verb,
                Some(setting),
                *prepare,
            );
            assert_eq!(
                guarded,
                plain,
                "`{}` with --root under {VAR}={setting} differs from without it",
                fakehost::command_line(verb, &[]).join(" ")
            );
        }
    }
}
