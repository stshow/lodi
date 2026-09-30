//! M-0.6 T-5: the pacman backend, offline, over shims whose answers were **recorded from the
//! real Arch guests of this package**.
//!
//! No pacman runs here and none could: this machine has no package manager and must never get
//! one (`AGENTS.md` §8). What runs is the product, unchanged, as a child process, with a
//! directory of test-owned shims first on its `PATH` — which is the whole seam by design (D3,
//! LD-115). The shim does two things: it appends its own argv to a log, and it prints the bytes
//! the guest printed for that command. So the assertions below are of two kinds, and both are
//! exact:
//!
//! - **what lodi would run**: every argv, byte for byte, in order, from the log;
//! - **what lodi concludes**: the plan's lines, the journal, the lock and the restart
//!   classification, derived from pacman's own recorded output.
//!
//! The recordings live in `tests/fixtures/host/pacman/arch/<scenario>/`, with `PROVENANCE.json`
//! saying which guest run produced them and from which commands. Their per-state directories
//! are the machine at one moment: `image` (the stock cloud image), `before`, `after`, and for
//! the interrupted transaction also `partial` (one of two packages there) and `dirty` (the
//! moment after the kill, when pacman's own `db.lck` is still on the machine).
//!
//! The shim reads two variables of its own, `LODI_PACMAN_STATE` and `LODI_PACMAN_FIXTURES`, from
//! a `shim-env` file beside it, written by the test: the product clears the environment of every
//! package manager it runs (LD-357), so nothing of the test's own environment reaches a shim.
//! Nothing in the product knows either name: they are how the *test* tells *its own* shim which
//! recording to replay, and there is no test-only branch anywhere in the product's paths.
//!
//! What Arch makes different, and what this file therefore asks that `tests/host_apt.rs` does
//! not: the refresh is inside the transaction and there is no index action, the removals are a
//! second action of their own, a `hold` is `--ignore` on lodi's own transaction and leaves
//! nothing behind on the machine (D11), and `--unsupported-partial-upgrade` is the one door to
//! the one mode Arch does not support (D10).
//!
//! Every manifest here says `packages = "managed"` under `[host]`: this file is the proof that
//! 1.1.0's removal rule is kept exactly under that key (LD-375). The exact mode, which an import
//! writes, is `tests/host_exact.rs`'s.

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fixture::Fixture;
use hostroot::{Root, feed, wait_until};
use lodi::hostscope::pm::{Origin, pacman};

/// The manifest the `install` recordings were made against: `common`, a per-distro `add`, a
/// `mark_auto`, a `hold` on a package the image already had, and one `optional` name no
/// distribution has. The `debian` table is never selected here — were the per-distro merge
/// wrong, its name would reach the index and stop the plan.
#[test]
fn local_file_pin_argv_is_exact_with_ignore_on_lodis_own_sync() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01");
    let mut files = std::collections::BTreeMap::new();
    for repo in ["core", "extra"] {
        files.insert(
            format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/{repo}.db"),
            fs::read(root.join(format!("{repo}/os/x86_64/{repo}.db"))).unwrap(),
        );
    }
    let name = "tree-2.3.2-1-x86_64.pkg.tar.zst";
    let url = format!("https://archive.archlinux.org/repos/2026/09/01/extra/os/x86_64/{name}");
    files.insert(
        url.clone(),
        fs::read(root.join(format!("extra/os/x86_64/{name}"))).unwrap(),
    );
    files.insert(
        format!("{url}.sig"),
        fs::read(root.join(format!("extra/os/x86_64/{name}.sig"))).unwrap(),
    );
    let server = support::Server::start(files);
    let mut current = fakehost::Pkg::new("tree");
    current.version = "9.9-1".into();
    let case = fakehost::Case::new(
        "pacman-arch-file-argv",
        fakehost::Machine::arch().with(current),
    );
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &root.join(format!("{repo}/os/x86_64")),
        );
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let applied = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(
        applied.status.code(),
        Some(0),
        "{}",
        fakehost::story(&applied)
    );
    let root = case.root.dir.display();
    let base = format!("--root {root} --dbpath {root}/var/lib/pacman");
    let config = format!("--config {root}/var/lib/lodi/host/pin/pin.conf");
    let staged = format!("{root}/var/lib/lodi/host/pin/stage/{name}");
    let log = case.log();
    let mutating: Vec<_> = log
        .iter()
        .filter(|line| {
            line.starts_with("pacman -Syy ")
                || line.starts_with("pacman -Syu ")
                || line.starts_with("pacman -Up ")
                || line.starts_with("pacman -U ")
        })
        .cloned()
        .collect();
    assert_eq!(
        mutating,
        vec![
            format!("pacman -Syy {base} {config} --noconfirm"),
            format!("pacman -Up {base} {config} --print-format %n -- {staged}"),
            format!("pacman -Syu {base} --noconfirm --ignore tree --"),
            format!("pacman -U {base} {config} --noconfirm -- {staged}"),
        ]
    );
}

#[test]
fn dated_snapshot_pacman_argv_is_exact_and_uses_private_pin_conf() {
    let machine = fakehost::Machine::arch().offering(fakehost::Pkg::new("tree"));
    let case = fakehost::Case::new("pacman-arch-dated-argv", machine);
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n");
    let applied = case.apply(&[]);
    assert_eq!(
        applied.status.code(),
        Some(0),
        "{}",
        fakehost::story(&applied)
    );
    let root = case.root.dir.display();
    let base = format!("--root {root} --dbpath {root}/var/lib/pacman");
    let config = format!("--config {root}/var/lib/lodi/host/pin/pin.conf");
    let log = case.log();
    let mutating: Vec<_> = log
        .iter()
        .filter(|line| line.starts_with("pacman -Syy ") || line.starts_with("pacman -Syu "))
        .cloned()
        .collect();
    assert_eq!(
        mutating,
        vec![
            format!("pacman -Syy {base} {config} --noconfirm"),
            format!("pacman -Syu {base} {config} --noconfirm -- tree"),
        ]
    );
}

const INSTALL_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"jq\", \"libsecret\", \"lodi-absent-probe\"]
mark_auto = [\"tree\"]
hold = [\"libsecret\"]
optional = [\"lodi-absent-probe\"]

[packages.debian]
add = [\"lodi-absent-probe-debian\"]

[packages.arch]
add = [\"zip\"]
";

/// The same manifest once the per-distro `add` has left it: `auto_remove` (the default) removes
/// what the lock records lodi as having installed, and nothing else.
const REMOVED_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"jq\", \"libsecret\"]
mark_auto = [\"tree\"]
hold = [\"libsecret\"]
";

/// An `absent` name removed while another name is installed: on Arch that is two actions, and
/// the removal is the second of them.
const ABSENT_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"jq\", \"libsecret\", \"bc\"]
mark_auto = [\"tree\"]
hold = [\"libsecret\"]
absent = [\"zip\"]
";

/// One name that is not installed yet, so that the partial mode has a transaction to build.
const PARTIAL_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"jq\", \"libsecret\", \"bc\"]
mark_auto = [\"tree\"]
hold = [\"libsecret\"]
";

/// The manifest the `interrupted` recordings were made against. Two packages in one transaction
/// is what makes an **ambiguous** restart producible on purpose: one there and one not is
/// neither bracket.
const INTERRUPTED_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"unzip\", \"patch\"]
";

/// One scratch root with one set of recordings behind a directory of shims.
struct Case {
    root: Root,
    shims: PathBuf,
    state: PathBuf,
    fixtures: PathBuf,
}

impl Case {
    /// A fresh root, armed and given Arch's identity, with the shim of this test first on the
    /// `PATH` every child gets and `scenario`'s recordings behind it.
    fn new(name: &str, scenario: &str, manifest: &str) -> Case {
        let root = Root::new(name);
        root.arm().write(
            "etc/os-release",
            "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n\
             BUILD_ID=rolling\nANSI_COLOR=\"38;2;23;147;209\"\n",
        );
        root.write("etc/lodi/host.toml", manifest);
        let fixtures = fixtures_dir().join("arch").join(scenario);
        assert!(
            fixtures.is_dir(),
            "the recordings {} are not there; they are produced by \
             `docs/milestones/m-0.6/probes/hostvm.py --record-fixtures`",
            fixtures.display()
        );
        let base = support::scratch(&format!("pacman-{name}"));
        let shims = base.join("bin");
        let state = base.join("state");
        fs::create_dir_all(&shims).expect("the shim directory");
        fs::create_dir_all(&state).expect("the shim state directory");
        {
            // This file's own recorded-fixture shim, not `fakehost::Case`'s, but the same guard
            // (#456): the two run in the same test binary, so a write here and a spawn from a
            // `fakehost::Case` elsewhere in this file can race the same way.
            let _writing = fakehost::writing();
            write_shims(&shims);
            write_shim_env(&shims, &state, &fixtures);
        }
        let case = Case {
            root,
            shims,
            state,
            fixtures,
        };
        case.phase("before");
        // Synchronised databases, so that `index_age` answers. Nothing acts on the answer here
        // — Arch refreshes inside the transaction — but the plan reports it, and a machine with
        // no databases at all is a different machine from this one.
        case.root
            .write("var/lib/pacman/sync/extra.db", "recorded database\n");
        case
    }

    /// Which recorded state of the machine the shim replays from now on.
    fn phase(&self, phase: &str) {
        fs::write(self.state.join("phase"), format!("{phase}\n")).expect("the phase");
    }

    /// Leave pacman's own transaction lock on the root, as a killed pacman does. Lodi reads it
    /// and never removes it, so a test that puts it there is the only thing that takes it away.
    fn db_lock(&self, present: bool) {
        let path = self.root.path("var/lib/pacman/db.lck");
        if present {
            fs::write(&path, "").expect("the database lock");
        } else {
            let _ = fs::remove_file(&path);
        }
    }

    fn manifest(&self, body: &str) {
        self.root.write("etc/lodi/host.toml", body);
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
        let path = format!(
            "{}:{}",
            self.shims.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        command
            .args(args)
            .arg("--root")
            .arg(&self.root.dir)
            .env("PATH", path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Run one host command, with the log emptied first so that it holds this run alone.
    ///
    /// The verb is assembled here rather than spelled out at each call, so that every host
    /// invocation in this file is a scratch-root one by construction: there is exactly one place
    /// the scope and the `--root` come from, and `scripts/check-host-safety.py` can see it.
    // check-host-safety: refusal — one place, and `command` appends this scratch root as --root.
    fn verb(&self, verb: &str, extra: &[&str]) -> Output {
        let mut args = vec!["host", verb];
        args.extend_from_slice(extra);
        let _ = fs::remove_file(self.state.join("log"));
        let _ = fs::remove_file(self.state.join("env"));
        let _ = fs::remove_file(self.state.join("inherited"));
        let _spawning = fakehost::spawning();
        self.command(&args).output().expect("lodi runs")
    }

    fn plan(&self) -> Output {
        self.verb("plan", &[])
    }

    fn apply(&self, extra: &[&str]) -> Output {
        self.verb("apply", extra)
    }

    /// Every argv the shim saw, in order, exactly as the product built it.
    fn log(&self) -> Vec<Vec<String>> {
        let bytes = fs::read(self.state.join("log")).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .split('\u{1e}')
            .filter(|record| !record.is_empty())
            .map(|record| record.split('\u{1f}').map(ToString::to_string).collect())
            .collect()
    }

    /// The same, as one line per invocation, which is what an assertion reads best.
    fn lines(&self) -> Vec<String> {
        self.log().iter().map(|argv| argv.join(" ")).collect()
    }

    /// What the shim was told about the environment the seam fixes for every program it runs.
    fn env_lines(&self) -> Vec<String> {
        fs::read_to_string(self.state.join("env"))
            .unwrap_or_default()
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    /// The root the product resolved, which is the scratch root after canonicalization.
    fn resolved(&self) -> PathBuf {
        fs::canonicalize(&self.root.dir).expect("the scratch root is there")
    }

    /// Design call D2 (LD-114) puts these after the operation of every invocation, because every
    /// host command in this file is a `--root` one: pacman's own `--root` and the package
    /// database below it. On a machine — the root `/`, which the guest evidence records — there
    /// are none, and the argv is exactly the one the Arch guests ran.
    fn root_options(&self) -> String {
        let root = self.resolved();
        format!(
            "--root {} --dbpath {}",
            root.display(),
            root.join("var/lib/pacman").display()
        )
    }

    fn lock(&self) -> serde_json::Value {
        serde_json::from_str(&self.root.read("etc/lodi/host.lock")).expect("the lock parses")
    }

    /// The newest journal's header, which is where the argv of every action is written down
    /// **before** it runs.
    fn journal(&self) -> serde_json::Value {
        let path = hostroot::journals(&self.root)
            .pop()
            .expect("a journal was written");
        let text = fs::read_to_string(path).expect("the journal is readable");
        serde_json::from_str(text.lines().next().expect("a header")).expect("the header parses")
    }
}

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/pacman")
}

fn out(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn err(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// What the shims read in place of an environment: the product starts every package manager
/// with the environment cleared and a fixed `PATH` (LD-357), so the shims take their own
/// variables — and the `PATH` their `cat` is found on — from this file beside them.
fn write_shim_env(shims: &Path, state: &Path, fixtures: &Path) {
    let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', "'\\''"));
    let path = std::env::var("PATH")
        .unwrap_or_default()
        .replace('\'', "'\\''");
    fs::write(
        shims.join("shim-env"),
        format!(
            "PATH='{path}'\nexport PATH\nLODI_PACMAN_STATE={}\nLODI_PACMAN_FIXTURES={}\n",
            quote(state),
            quote(fixtures)
        ),
    )
    .expect("the shim environment");
}

/// The shim. It logs its argv with `\x1f` between the words and `\x1e` after the last one — a
/// separator no argv contains — and then replays a recording.
///
/// One program, because the backend reaches for one: the gate already requires `pacman` on
/// `PATH`, and pacman answers every question this backend asks.
fn write_shims(dir: &Path) {
    let script = "\
#!/bin/sh
# A test-owned shim. It replays what a real Arch guest printed for this command.
# Everything this process inherited, read by a builtin before anything is set.
inherited=$(export -p)
. \"${0%/*}/shim-env\"
state=\"$LODI_PACMAN_STATE\"
fixtures=\"$LODI_PACMAN_FIXTURES\"
# Command substitution strips the trailing newline, so this is the bare word.
phase=$(cat \"$state/phase\" 2>/dev/null || echo before)
{ printf '%s' 'pacman'
  for a in \"$@\"; do printf '\\037%s' \"$a\"; done
  printf '\\036'; } >> \"$state/log\"
{ printf '== %s\\n' 'pacman'; printf '%s\\n' \"$inherited\"; } >> \"$state/inherited\"
# The environment the seam fixes for every program it runs, as this program got it.
printf 'pacman LC_ALL=%s LANG=%s PAGER=%s\\n' \\
  \"${LC_ALL-unset}\" \"${LANG-unset}\" \"${PAGER-unset}\" >> \"$state/env\"
case \"$1\" in
  -Q)  cat \"$fixtures/$phase/pacman-Q.txt\" ;;
  -Qe) cat \"$fixtures/$phase/pacman-Qe.txt\" ;;
  -Slq) cat \"$fixtures/pacman-Slq.txt\" ;;
  -Si)
    cat \"$fixtures/pacman-Si.txt\"
    # pacman exits non-zero when any one name is unknown, and prints the rest anyway.
    if [ -s \"$fixtures/pacman-Si.err\" ]; then cat \"$fixtures/pacman-Si.err\" >&2; exit 1; fi ;;
  -Syu|-S|-Rns)
    if [ -p \"$state/gate\" ]; then cat \"$state/gate\" >/dev/null; printf 'gone\\n' > \"$state/gated\"
    else printf '%s\\n' \"${LODI_PACMAN_NEXT_PHASE:-after}\" > \"$state/phase\"; fi
    code=$(cat \"$state/exit\" 2>/dev/null || echo 0)
    if [ -f \"$state/stderr\" ]; then cat \"$state/stderr\" >&2; fi
    exit \"$code\" ;;
esac
exit 0
";
    let path = dir.join("pacman");
    fs::write(&path, script).expect("a shim");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("a shim mode");
}

/// Start a real apply and stop it inside the transaction, with the package manager held at the
/// moment it was started: the `pacman -Syu` shim blocks on a fifo until this returns.
fn kill_inside_the_transaction(case: &Case) {
    let gate = case.state.join("gate");
    let _ = fs::remove_file(&gate);
    let gone = case.state.join("gated");
    let _ = fs::remove_file(&gone);
    let status = Command::new("mkfifo")
        .arg(&gate)
        .status()
        .expect("mkfifo runs");
    assert!(status.success(), "mkfifo {}", gate.display());
    let _ = fs::remove_file(case.state.join("log"));
    // Whatever fails below, neither the apply nor the shim it holds outlives this test.
    let mut child = Fixture::spawn(
        "the held apply",
        // check-host-safety: refusal — `command` appends this scratch root as `--root`.
        &mut case.command(&["host", "apply"]),
    );
    wait_until("the transaction to start", || {
        case.lines()
            .iter()
            .any(|line| line.starts_with("pacman -Syu"))
    });
    let _ = Command::new("kill")
        .arg("-KILL")
        .arg(child.id().to_string())
        .status();
    let _ = child.wait();
    // Let the held shim go, without letting it record anything further: it writes no phase when
    // it was gated, so what the machine shows next is only what this test says it shows. The
    // wait is the point — the shim outlived the process that started it, and a test that set
    // the next state while it was still running would be racing its own fixture.
    feed(&gate, "");
    wait_until("the held package manager to exit", || gone.is_file());
    let _ = fs::remove_file(&gate);
}

// ----------------------------------------------------------------- (a), (i): the one argv ---

/// (a) and (i). One transaction, one `pacman -Syu --noconfirm`, and every argv the apply builds
/// — the reads that observe the machine and the writes that change it — byte for byte, in order.
#[test]
fn every_pacman_invocation_of_one_apply_is_exactly_this() {
    let case = Case::new("argv", "install", INSTALL_MANIFEST);

    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        "+ package tree\n+ package jq\n+ package zip\n\
         ~ package libsecret (hold: lodi transactions only)\n\
         ~ package tree (auto)\n~ package libsecret (manual)\n2 action(s)\n"
    );
    assert!(
        err(&plan).contains("W_OPTIONAL_SKIPPED: `lodi-absent-probe` is optional"),
        "{}",
        err(&plan)
    );
    // A plan reads the machine and the index, and runs nothing that could change either.
    let ro = case.root_options();
    assert_eq!(
        case.lines(),
        vec![
            format!("pacman -Q {ro}"),
            format!("pacman -Qe {ro}"),
            format!("pacman -Si {ro} -- tree jq libsecret lodi-absent-probe zip"),
        ],
        "a plan observes and asks the index, and mutates nothing"
    );

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    assert_eq!(
        case.lines(),
        vec![
            // The machine is read twice before anything moves: once for the read-only
            // pre-flight, and again after the apply lock is held, because another process may
            // have changed the machine in between. Neither reading mutates anything.
            format!("pacman -Q {ro}"),
            format!("pacman -Qe {ro}"),
            format!("pacman -Si {ro} -- tree jq libsecret lodi-absent-probe zip"),
            format!("pacman -Q {ro}"),
            format!("pacman -Qe {ro}"),
            format!("pacman -Si {ro} -- tree jq libsecret lodi-absent-probe zip"),
            // One transaction. It refreshes and upgrades the machine in the same breath,
            // because on Arch that is the only supported way to install (D10).
            format!("pacman -Syu {ro} --noconfirm --ignore libsecret -- tree jq zip"),
            format!("pacman -D {ro} --asdeps -- tree"),
            format!("pacman -D {ro} --asexplicit -- libsecret"),
            // The second reading OD-15 asks for: the lock records the versions the machine
            // shows afterwards, not the ones the index promised.
            format!("pacman -Q {ro}"),
            format!("pacman -Qe {ro}"),
        ]
    );
    // There is no separate index action and there is no bare `-Sy` anywhere: fetching the
    // databases without upgrading is exactly the partial state Arch does not support.
    for line in case.lines() {
        assert!(
            !line.starts_with("pacman -Sy ") && !line.contains("-Syy"),
            "{line} refreshes without upgrading"
        );
    }
    // The environment the seam fixes reaches the program, every time.
    for line in case.env_lines() {
        assert_eq!(line, "pacman LC_ALL=C LANG=C PAGER=cat", "{line}");
    }
    assert!(!case.env_lines().is_empty());
}

/// The journal writes the argv down **before** it runs, which is what makes the evidence a
/// record of intent rather than a report of history.
#[test]
fn the_journal_records_the_transaction_before_it_runs() {
    let case = Case::new("journal", "install", INSTALL_MANIFEST);
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let header = case.journal();
    let actions = header["actions"].as_array().expect("actions");
    let kinds: Vec<&str> = actions
        .iter()
        .map(|a| a["kind"].as_str().expect("a kind"))
        .collect();
    assert_eq!(kinds, vec!["packages.transaction", "packages.marks"]);
    let commands = actions[0]["commands"].as_array().expect("commands");
    assert_eq!(commands.len(), 1, "one transaction, always");
    let argv: Vec<&str> = commands[0]
        .as_array()
        .expect("an argv")
        .iter()
        .map(|w| w.as_str().expect("a word"))
        .collect();
    assert_eq!(argv[0], "pacman");
    assert_eq!(argv[1], "-Syu");
    assert!(argv.contains(&"--noconfirm") && argv.contains(&"--ignore"));
    assert!(argv.ends_with(&["--", "tree", "jq", "zip"]));
}

// ------------------------------------------------------------------- (b): nothing to do ---

/// (b) A second apply plans and changes nothing: no transaction, no marks, no lock rewrite —
/// and, because Arch refreshes inside its transaction, no upgrade of the machine either.
#[test]
fn a_second_apply_plans_and_changes_nothing() {
    let case = Case::new("idempotent", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    let first = case.root.read("etc/lodi/host.lock");

    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(out(&plan), "nothing to do\n");
    let second = case.apply(&[]);
    assert!(second.status.success(), "{}", err(&second));
    assert_eq!(out(&second), "nothing to do\n");
    assert_eq!(case.root.read("etc/lodi/host.lock"), first);
    let ro = case.root_options();
    assert_eq!(
        case.lines(),
        vec![
            format!("pacman -Q {ro}"),
            format!("pacman -Qe {ro}"),
            format!("pacman -Si {ro} -- tree jq libsecret lodi-absent-probe zip"),
        ],
        "a settled machine is observed and left alone"
    );
}

// -------------------------------------------------- (c): the unsupported partial upgrade ---

/// (c) `--unsupported-partial-upgrade` builds `-S --needed`, warns with `W_ARCH_PARTIAL`, and is
/// the **only** way to reach that mode: no manifest key, no environment variable, no default,
/// and not even a plan.
#[test]
fn the_partial_upgrade_is_one_flag_one_argv_and_one_warning() {
    let case = Case::new("partial", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    case.manifest(PARTIAL_MANIFEST);

    let apply = case.apply(&["--unsupported-partial-upgrade"]);
    assert!(apply.status.success(), "{}", err(&apply));
    let warning = err(&apply);
    assert!(
        warning.contains(
            "W_ARCH_PARTIAL: --unsupported-partial-upgrade installs without \
                          upgrading this machine, which arch does not support"
        ),
        "{warning}"
    );
    let ro = case.root_options();
    assert!(
        case.lines().contains(&format!(
            "pacman -S {ro} --needed --noconfirm --ignore libsecret -- bc"
        )),
        "{:?}",
        case.lines()
    );
    assert!(
        !case.lines().iter().any(|line| line.contains("-Syu")),
        "the partial mode upgrades nothing: {:?}",
        case.lines()
    );

    // Without the flag, the same manifest is a full upgrade and there is no warning.
    let full = case.apply(&[]);
    assert!(full.status.success(), "{}", err(&full));
    assert!(!err(&full).contains("W_ARCH_PARTIAL"), "{}", err(&full));
    assert!(
        case.lines().contains(&format!(
            "pacman -Syu {ro} --noconfirm --ignore libsecret -- bc"
        )),
        "{:?}",
        case.lines()
    );

    // A plan does not take the flag: it is an apply's flag, and an unknown one is a usage error.
    let planned = case.verb("plan", &["--unsupported-partial-upgrade"]);
    assert_eq!(planned.status.code(), Some(2), "{}", err(&planned));
}

/// The flag means something on Arch and nothing anywhere else, so a root that is not Arch
/// refuses it rather than accepting a flag it would ignore.
#[test]
fn a_root_with_no_partial_mode_refuses_the_flag_rather_than_ignoring_it() {
    let root = Root::new("pacman-flag-on-debian");
    root.arm().write(
        "etc/os-release",
        "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nID=debian\nVERSION_ID=\"12\"\n",
    );
    root.write("etc/lodi/host.toml", "[host]\nversion = \"1\"\n");
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        // check-host-safety: refusal — this scratch root is the `--root` of this one call.
        .args(["host", "apply", "--unsupported-partial-upgrade", "--root"])
        .arg(&root.dir)
        .output()
        .expect("lodi runs");
    assert_eq!(output.status.code(), Some(3), "{}", err(&output));
    let message = err(&output);
    assert!(message.contains("E_UNSUPPORTED"), "{message}");
    assert!(message.contains("arch option"), "{message}");
}

// -------------------------------------------------------------------------- (d): the marks ---

/// (d) `mark_auto` is what `pacman -Qd` would list and the rest is what `pacman -Qe` lists, and
/// the lock says the same thing about each.
#[test]
fn mark_auto_is_a_dependency_and_the_rest_are_explicit() {
    let case = Case::new("marks", "install", INSTALL_MANIFEST);
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let ro = case.root_options();
    assert!(
        case.lines()
            .contains(&format!("pacman -D {ro} --asdeps -- tree"))
    );
    assert!(
        case.lines()
            .contains(&format!("pacman -D {ro} --asexplicit -- libsecret"))
    );
    let lock = case.lock();
    assert_eq!(lock["packages"]["tree"]["mark"], "auto");
    for name in ["jq", "libsecret", "zip"] {
        assert_eq!(lock["packages"][name]["mark"], "manual", "{name}");
    }
    // The recorded `-Qe` of the guest after the apply is the machine's own answer, and it is
    // the one the lock was derived from: `tree` is not in it, and the other three are.
    let explicit = fs::read_to_string(case.fixtures.join("after/pacman-Qe.txt"))
        .expect("the recorded explicit set");
    let names: Vec<&str> = explicit
        .lines()
        .filter_map(|line| line.split_whitespace().next())
        .collect();
    assert!(!names.contains(&"tree"), "{names:?}");
    for name in ["jq", "libsecret", "zip"] {
        assert!(names.contains(&name), "{name} is missing from {names:?}");
    }
}

// --------------------------------------------------------------------------- (e): the hold ---

/// (e) A hold is `--ignore` on lodi's own transaction, it keeps the package at the version the
/// machine already had, and the plan says how far it reaches: no file is written, nothing on the
/// machine changes, and no other command on that machine is bound by it (D11).
#[test]
fn a_hold_is_an_ignore_on_lodis_own_transaction_and_says_so() {
    let case = Case::new("hold", "install", INSTALL_MANIFEST);
    let plan = case.plan();
    assert!(
        out(&plan).contains("~ package libsecret (hold: lodi transactions only)"),
        "{}",
        out(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let ro = case.root_options();
    assert!(
        case.lines().contains(&format!(
            "pacman -Syu {ro} --noconfirm --ignore libsecret -- tree jq zip"
        )),
        "{:?}",
        case.lines()
    );
    // Nothing that could hold a package outside this transaction: no `pacman.conf`, no
    // drop-in beside it, and no `IgnorePkg` anywhere below the root.
    assert!(!case.root.exists("etc/pacman.conf"));
    assert!(!case.root.exists("etc/pacman.d"));
    for line in case.lines() {
        assert!(!line.contains("pacman.conf"), "{line}");
        assert!(!line.contains("IgnorePkg"), "{line}");
    }
    // The version the machine had before the transaction is the version it has after it, and
    // the lock records the hold as what this apply actually passed.
    let before = recorded_version(&case, "before", "libsecret");
    let after = recorded_version(&case, "after", "libsecret");
    assert_eq!(before, after, "the held package moved");
    let lock = case.lock();
    assert_eq!(lock["packages"]["libsecret"]["version"], after);
    assert_eq!(lock["packages"]["libsecret"]["held"], true);
    assert_eq!(lock["packages"]["tree"]["held"], false);
    // And the rest of the machine did move, so the hold was measured against a transaction that
    // really upgrades: this is the recorded guest, where `-Syu` upgraded five other packages.
    assert_ne!(
        recorded_version(&case, "before", "bash"),
        recorded_version(&case, "after", "bash"),
        "the recorded transaction upgraded nothing, so the hold proves nothing"
    );
}

/// A declared package that is held and **not yet installed** is installed. A hold says "leave
/// this where it is"; a package that is nowhere is at no version, and ignoring it would quietly
/// mean "never install this".
#[test]
fn a_hold_on_a_package_the_machine_does_not_have_still_installs_it() {
    let case = Case::new("hold-absent", "install", INSTALL_MANIFEST);
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"tree\", \"jq\"]\nhold = [\"jq\"]\n",
    );
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(out(&plan).contains("+ package jq"), "{}", out(&plan));
    assert!(
        !out(&plan).contains("~ package jq (hold"),
        "a package that is not there cannot be held at a version: {}",
        out(&plan)
    );
    assert!(case.apply(&[]).status.success());
    assert!(
        case.lines().iter().any(|line| line.contains("-Syu")
            && line.ends_with("tree jq")
            && !line.contains("--ignore")),
        "{:?}",
        case.lines()
    );
}

// ------------------------------------------------------- (f): absent, auto_remove and -Rns ---

/// (f) `auto_remove` removes what the lock records lodi as having installed, as its own second
/// action, with `-Rns` naming exactly that package and nothing else.
#[test]
fn auto_remove_is_one_rns_of_exactly_what_lodi_installed() {
    let case = Case::new("autoremove", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    case.manifest(REMOVED_MANIFEST);

    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(out(&plan), "- package zip\n1 action(s)\n");
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let ro = case.root_options();
    assert!(
        case.lines()
            .contains(&format!("pacman -Rns {ro} --noconfirm -- zip")),
        "{:?}",
        case.lines()
    );
    // One removal, of one name, and no transaction beside it: a removal-only apply installs
    // nothing and therefore upgrades nothing.
    let lines = case.lines();
    let removals: Vec<&String> = lines.iter().filter(|line| line.contains("-Rns")).collect();
    assert_eq!(removals.len(), 1, "{removals:?}");
    assert!(
        !case.lines().iter().any(|line| line.contains("-Syu")),
        "{:?}",
        case.lines()
    );
    let kinds = journal_kinds(&case);
    assert_eq!(kinds, vec!["packages.removal"]);
    // The package the operator installed for themselves is in no lock, so it is in no removal.
    assert!(
        !case.lines().iter().any(|line| line.contains("whois")),
        "auto_remove reached a package lodi never installed: {:?}",
        case.lines()
    );
}

/// (f) `absent` removes an installed package while another is installed, and on Arch those are
/// two actions in one apply: the transaction first, the removal second, each with its own
/// journal brackets.
#[test]
fn absent_is_the_second_action_of_an_apply_that_also_installs() {
    let case = Case::new("absent", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    case.manifest(ABSENT_MANIFEST);

    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        "+ package bc\n~ package libsecret (hold: lodi transactions only)\n\
         - package zip\n2 action(s)\n"
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let ro = case.root_options();
    let lines = case.lines();
    let transaction = format!("pacman -Syu {ro} --noconfirm --ignore libsecret -- bc");
    let removal = format!("pacman -Rns {ro} --noconfirm -- zip");
    let at = |needle: &String| lines.iter().position(|line| line == needle);
    assert!(
        at(&transaction).is_some() && at(&removal).is_some(),
        "{lines:?}"
    );
    assert!(
        at(&transaction) < at(&removal),
        "the removal is the second action: {lines:?}"
    );
    assert_eq!(
        journal_kinds(&case),
        vec!["packages.transaction", "packages.removal"]
    );
}

// ------------------------------------------------- (g): an unknown name, and an optional one ---

/// (g) A name the index cannot place is `E_UNKNOWN_PACKAGE` (3) and stops the command before
/// anything is written; the **same name** declared `optional` is skipped with a warning and the
/// command carries on.
#[test]
fn an_unknown_name_stops_and_the_same_name_optional_is_skipped() {
    let case = Case::new("unknown", "install", INSTALL_MANIFEST);
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"tree\", \"lodi-absent-probe\"]\n",
    );
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3), "{}", err(&plan));
    let message = err(&plan);
    assert!(message.contains("E_UNKNOWN_PACKAGE"), "{message}");
    assert!(message.contains("lodi-absent-probe"), "{message}");
    // The nearest names come from `pacman -Slq`, which takes no pattern at all: LD-80 keeps
    // regular expressions out of the product, and `-Ss` would be one.
    assert!(
        message.contains("lodi") || message.contains("did you mean"),
        "{message}"
    );
    assert!(!case.root.exists("etc/lodi/host.lock"));
    assert!(!case.root.exists("var/lib/lodi"));

    // The same name, declared optional.
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"tree\", \"lodi-absent-probe\"]\n\
         optional = [\"lodi-absent-probe\"]\n",
    );
    let skipped = case.plan();
    assert!(skipped.status.success(), "{}", err(&skipped));
    assert!(
        err(&skipped).contains("W_OPTIONAL_SKIPPED: `lodi-absent-probe` is optional"),
        "{}",
        err(&skipped)
    );
    assert!(
        out(&skipped).contains("+ package tree"),
        "{}",
        out(&skipped)
    );
    assert!(
        !out(&skipped).contains("lodi-absent-probe"),
        "{}",
        out(&skipped)
    );
}

// --------------------------------------------------------- (h): the interrupted transaction ---

/// (h) An apply killed inside its transaction is classified on restart from what the machine
/// shows: the state before it is **incomplete**, the state after it is **complete**, and one of
/// the two packages there is **ambiguous** — and the ambiguous one stops at
/// `E_JOURNAL_AMBIGUOUS` (8) until a human says it is resolved.
#[test]
fn a_killed_transaction_is_classified_from_what_the_machine_shows() {
    let case = Case::new("kill", "interrupted", INTERRUPTED_MANIFEST);
    kill_inside_the_transaction(&case);
    assert!(
        case.lines()
            .iter()
            .any(|line| line.contains("-Syu") && line.ends_with("unzip patch")),
        "{:?}",
        case.lines()
    );

    // Nothing happened: the machine is as it was before the transaction.
    case.phase("before");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(
        out(&plan).contains("is outstanding: incomplete at p1"),
        "{}",
        out(&plan)
    );

    // Everything happened: the machine is as the transaction meant to leave it.
    case.phase("after");
    let done = case.plan();
    assert!(done.status.success(), "{}", err(&done));
    assert!(
        out(&done).contains("is outstanding: complete\n"),
        "{}",
        out(&done)
    );

    // One of the two: neither bracket, so lodi refuses to guess.
    case.phase("partial");
    let ambiguous = case.apply(&[]);
    assert_eq!(ambiguous.status.code(), Some(8), "{}", err(&ambiguous));
    let message = err(&ambiguous);
    assert!(message.contains("E_JOURNAL_AMBIGUOUS"), "{message}");
    assert!(
        message.contains("matches neither the state before it"),
        "{message}"
    );
    assert!(message.contains("db.lck"), "{message}");
    assert!(message.contains("--resolved"), "{message}");

    // The human says which journal they put right, and the apply finishes the work.
    // The machine stays as the operator found it: `--resolved` is answerable only while the
    // journal is ambiguous, and the apply that follows is what carries it to the end.
    let id = journal_id(&case);
    let resolved = case.apply(&["--resolved", &id]);
    assert!(resolved.status.success(), "{}", err(&resolved));
    let lock = case.lock();
    for name in ["unzip", "patch"] {
        assert_eq!(
            lock["packages"][name]["version"],
            recorded_version(&case, "after", name),
            "{name}"
        );
    }
}

/// (h) pacman's own transaction lock, left behind by a pacman that did not finish, makes the
/// answer ambiguous whatever the installed set says — and lodi names the file and leaves it
/// exactly where it found it. Removing it is the operator's decision, not lodi's.
#[test]
fn a_database_lock_left_behind_is_ambiguous_and_lodi_never_removes_it() {
    let case = Case::new("dblock", "interrupted", INTERRUPTED_MANIFEST);
    kill_inside_the_transaction(&case);

    // The machine shows exactly the state the transaction meant to leave — which would be
    // `completed` — but pacman's lock is still there, so the answer is not that.
    case.phase("after");
    case.db_lock(true);
    let stopped = case.apply(&[]);
    assert_eq!(stopped.status.code(), Some(8), "{}", err(&stopped));
    let message = err(&stopped);
    assert!(message.contains("E_JOURNAL_AMBIGUOUS"), "{message}");
    assert!(
        message.contains("the package database is not clean"),
        "{message}"
    );
    assert!(message.contains("db.lck"), "{message}");
    assert!(
        message.contains("remove that lock file yourself"),
        "the operator is told whose job it is: {message}"
    );
    // The message is a sentence and the hint asks for something an operator can act on.
    // pacman's transaction lock is a **condition** of the machine, not a subject that can be
    // put into a state, so it may not stand where a path or a `package NAME` stands (R-1):
    // before the fix this read "... and the package database is not clean: ... is present, so a
    // pacman transaction did not finish matches neither the state before it nor the state after
    // it", and told the operator to put that whole sentence "into the state you want".
    assert!(
        !message.contains("matches neither the state before it"),
        "pacman's lock names no bracket to compare against: {message}"
    );
    assert!(
        message.contains("put the machine into the state you want"),
        "{message}"
    );
    assert!(
        !message.contains("put the package database is not clean"),
        "the hint may never ask for a sentence to be put into a state: {message}"
    );
    assert!(
        case.root.exists("var/lib/pacman/db.lck"),
        "lodi removed the lock, which is never lodi's to remove"
    );
    // No pacman was run at all: the command stopped before it planned anything to do.
    assert!(
        !case.lines().iter().any(|line| line.contains("-Syu")),
        "{:?}",
        case.lines()
    );

    // The operator removes it, and the same machine is then simply `completed`.
    case.db_lock(false);
    let clear = case.plan();
    assert!(clear.status.success(), "{}", err(&clear));
    assert!(
        out(&clear).contains("is outstanding: complete\n"),
        "{}",
        out(&clear)
    );
}

// ------------------------------------------------------------------------ the recordings ---

/// Every fixture this file replays came off a guest, and `PROVENANCE.json` says which one.
/// A fixture with no provenance is a fixture somebody could have written by hand.
#[test]
fn the_recordings_name_the_guest_run_that_produced_them() {
    let provenance: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixtures_dir().join("PROVENANCE.json")).expect("the provenance"),
    )
    .expect("the provenance parses");
    assert_eq!(provenance["kind"], "lodi-pacman-fixture-provenance");
    let runs = provenance["runs"].as_object().expect("the runs");
    for key in ["arch", "arch/interrupted"] {
        let run = runs.get(key).unwrap_or_else(|| panic!("no run for {key}"));
        for field in [
            "case",
            "distro",
            "guestImage",
            "guestImageChecksum",
            "binary",
            "driver",
            "vmTool",
            "recordedAt",
            "evidence",
        ] {
            assert!(run.get(field).is_some(), "{key} has no {field}");
        }
        // That the evidence file is committed is checked by tests/dev_records.rs.
        assert!(run["evidence"].is_string(), "{key} names no evidence file");
    }
    let commands = provenance["commands"].as_object().expect("the commands");
    for name in [
        "pacman-Q.txt",
        "pacman-Qe.txt",
        "pacman-Qd.txt",
        "pacman-Si.txt",
    ] {
        assert!(
            commands.contains_key(name),
            "no command recorded for {name}"
        );
    }
}

/// `pacman -Si` names the person who packaged each package, by name and address. That is a real
/// person's personal data, and `AGENTS.md` §1 says none of it is ever committed. The probe takes
/// the field out on the guest, before the reading is fetched; this is the check that a recording
/// which skipped that step fails here rather than landing.
#[test]
fn no_recording_carries_anybodys_address() {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("the fixture directory").flatten() {
            let path = entry.path();
            if path.is_dir() {
                visit(&path, out);
            } else {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    visit(&fixtures_dir(), &mut files);
    assert!(files.len() > 10, "the recordings are there: {files:?}");
    for path in files {
        let text = fs::read_to_string(&path).expect("a recording is text");
        for (number, line) in text.lines().enumerate() {
            assert!(
                !line.contains('@'),
                "{}:{} carries an address: {line}",
                path.display(),
                number + 1
            );
        }
    }
}

// ------------------------------------------------------------------------------- helpers ---

/// The version one recorded state of the machine shows for one package.
fn recorded_version(case: &Case, phase: &str, name: &str) -> String {
    let text = fs::read_to_string(case.fixtures.join(phase).join("pacman-Q.txt"))
        .unwrap_or_else(|_| panic!("the recorded {phase} state"));
    text.lines()
        .find_map(|line| {
            let mut fields = line.split_whitespace();
            match (fields.next(), fields.next()) {
                (Some(found), Some(version)) if found == name => Some(version.to_string()),
                _ => None,
            }
        })
        .unwrap_or_else(|| panic!("{name} is not in the recorded {phase} state"))
}

/// The kinds of the newest journal's actions, in order.
fn journal_kinds(case: &Case) -> Vec<String> {
    case.journal()["actions"]
        .as_array()
        .expect("actions")
        .iter()
        .map(|a| a["kind"].as_str().expect("a kind").to_string())
        .collect()
}

fn journal_id(case: &Case) -> String {
    case.journal()["journalId"]
        .as_str()
        .expect("a journal id")
        .to_string()
}

// --------------------------------------------- R-2: which repository the installed name is in ---

/// `pacman -Si` prints one record per repository that offers a name, and the `Repository :`
/// field of each is the answer. A name two repositories offer has two records, and the set is
/// what decides.
#[test]
fn si_records_give_every_repository_that_offers_a_name() {
    let offered = pacman::parse_si_repositories(
        "Repository      : chaotic-aur\n\
         Name            : lodi-tool\n\
         Version         : 1.3.0-1\n\
         \n\
         Repository      : extra\n\
         Name            : lodi-tool\n\
         Version         : 1.3.0-1\n\
         \n\
         Repository      : chaotic-aur\n\
         Name            : tela-icon-theme\n\
         Version         : 2025.01-1\n",
    );
    assert_eq!(
        offered
            .get("lodi-tool")
            .map(|set| set.iter().cloned().collect::<Vec<String>>()),
        Some(vec!["chaotic-aur".to_string(), "extra".to_string()])
    );
    assert_eq!(
        offered
            .get("tela-icon-theme")
            .map(|set| set.iter().cloned().collect::<Vec<String>>()),
        Some(vec!["chaotic-aur".to_string()])
    );
}

/// The classification: one of the family's own repositories, and enabled on this machine, is
/// base; anything else is that repository by name; nothing at all is the local database alone.
#[test]
fn a_repository_that_is_not_the_familys_own_is_never_declarable() {
    let set =
        |names: &[&str]| -> BTreeSet<String> { names.iter().map(ToString::to_string).collect() };
    let enabled = set(&["chaotic-aur", "core", "extra", "multilib"]);
    assert_eq!(pacman::classify(&set(&["extra"]), &enabled), Origin::Base);
    assert_eq!(
        pacman::classify(&set(&["chaotic-aur", "extra"]), &enabled),
        Origin::Base,
        "one repository a fresh machine has is enough"
    );
    assert_eq!(
        pacman::classify(&set(&["chaotic-aur"]), &enabled),
        Origin::ThirdParty("chaotic-aur".to_string())
    );
    assert_eq!(pacman::classify(&set(&[]), &enabled), Origin::LocalOnly);
    // `pacman-conf` missing is an empty set, and then the family's own names decide alone — the
    // fallback never declares a name the stricter test would have left out.
    assert_eq!(
        pacman::classify(&set(&["extra"]), &BTreeSet::new()),
        Origin::Base
    );
    assert_eq!(
        pacman::classify(&set(&["extra"]), &set(&["core"])),
        Origin::ThirdParty("extra".to_string()),
        "a repository this machine does not have enabled is not one it can install from"
    );
    assert_eq!(
        pacman::parse_repo_list("core\nextra\n\nchaotic-aur\n"),
        set(&["chaotic-aur", "core", "extra"])
    );
}

/// The mirror of the apt reading: a root whose `var/lib/pacman/sync` holds no database has
/// never been told what any repository offers, so the origin of nothing installed on it can be
/// checked. It is read off the directory and **not** inferred from an empty `--Si` answer,
/// because a machine every one of whose chosen names is an AUR build answers `-Si` with
/// nothing too, and that machine's index is perfectly good.
#[test]
fn a_root_with_no_sync_database_has_no_index() {
    let root = Root::new("pacman-has-index");
    assert!(
        !pacman::has_index(&root.path("")),
        "a root with no sync directory at all"
    );

    fs::create_dir_all(root.path("var/lib/pacman/sync")).expect("the sync directory");
    assert!(
        !pacman::has_index(&root.path("")),
        "an empty sync directory is not an index"
    );

    root.write("var/lib/pacman/sync/core.db.part", "half a download\n");
    assert!(
        !pacman::has_index(&root.path("")),
        "an interrupted download is not a database"
    );

    root.write("var/lib/pacman/sync/core.db", "recorded database\n");
    assert!(
        pacman::has_index(&root.path("")),
        "one database is an index"
    );
}

/// Rule (LD-357): a package manager the host scope starts inherits no variable of the caller's
/// environment beyond the fixed set, and its `PATH` is the fixed path. The caller here carries
/// variables that would steer pacman if they reached it; the shims' own record of what they
/// inherited shows none of them.
#[test]
fn a_package_manager_inherits_nothing_but_the_fixed_environment() {
    let case = Case::new("fixed-env", "install", INSTALL_MANIFEST);
    let _ = fs::remove_file(case.state.join("inherited"));
    let output = case
        // check-host-safety: refusal — `command` appends this scratch root as `--root`.
        .command(&["host", "plan"])
        .env("PACMAN_CONF", "/nonexistent/planted.conf")
        .env("LD_LIBRARY_PATH", "/nonexistent")
        .env("LODI_PLANTED", "planted")
        .output()
        .expect("lodi runs");
    assert!(output.status.success(), "{}", err(&output));
    let inherited = fs::read_to_string(case.state.join("inherited")).expect("the shims ran");
    assert!(!inherited.contains("planted"), "{inherited}");
    let checked = hostroot::assert_fixed_environment(
        &inherited,
        &[
            "PATH",
            "DEBIAN_FRONTEND",
            "DEBCONF_NONINTERACTIVE_SEEN",
            "LC_ALL",
            "LANG",
            "PAGER",
        ],
    );
    assert!(checked > 0, "no package manager ran:\n{inherited}");
}

/// #172: the fake pacman refuses an unsigned local package whether or not it was given
/// `--config`, so a lost `.sig` fails on its own and not only through the exact-argv test.
#[test]
fn fake_pacman_requires_a_local_package_signature_without_config() {
    let case = fakehost::Case::new(
        "fake-unsigned-local",
        fakehost::Machine::arch().offering(fakehost::Pkg::new("tree")),
    );
    let file = case.base().join("tree-2.3.2-1-x86_64.pkg.tar.zst");
    fs::write(&file, b"an unsigned package").unwrap();
    for args in [vec!["-Up"], vec!["-U", "--noconfirm"]] {
        let _spawning = fakehost::spawning();
        let ran = Command::new(case.base().join("bin/pacman"))
            .args(&args)
            .arg("--")
            .arg(&file)
            .output()
            .unwrap();
        assert_eq!(
            ran.status.code(),
            Some(1),
            "{args:?}: {}",
            fakehost::story(&ran)
        );
    }
    assert!(case.machine()["installed"].get("tree").is_none());
}
