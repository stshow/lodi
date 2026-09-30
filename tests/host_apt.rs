//! M-0.6 T-4: the apt backend, offline, over shims whose answers were **recorded from the real
//! Debian 12 and Ubuntu 24.04 guests of this package**.
//!
//! No apt runs here and none could: this machine has no package manager and must never get one
//! (`AGENTS.md` §8). What runs is the product, unchanged, as a child process, with a directory of
//! test-owned shims first on its `PATH` — which is the whole seam by design (D3, LD-115). Each
//! shim does two things: it appends its own argv to a log, and it prints the bytes the guest
//! printed for that command. So the assertions below are of two kinds, and both are exact:
//!
//! - **what lodi would run**: every argv, byte for byte, in order, from the log;
//! - **what lodi concludes**: the plan's lines, the journal, the lock and the restart
//!   classification, derived from apt's own recorded output.
//!
//! The recordings live in `tests/fixtures/host/apt/<distro>/<scenario>/`, with
//! `PROVENANCE.json` saying which guest run produced them and from which commands. Their
//! per-state directories are the machine at one moment: `before`, `after`, and for the
//! interrupted transaction also `partial` (one of two packages there) and `dirty` (dpkg left
//! part way through, which `dpkg --audit` really does print).
//!
//! The shims read two variables of their own, `LODI_APT_STATE` and `LODI_APT_FIXTURES`, from a
//! `shim-env` file beside them, written by the test: the product clears the environment of every
//! package manager it runs (LD-357), so nothing of the test's own environment reaches a shim.
//! Nothing in the product knows either name: they are how the *test* tells *its own* shims which
//! recording to replay, and there is no test-only branch anywhere in the product's paths.
//!
//! Every manifest here says `packages = "managed"` under `[host]`: this file is the proof that
//! 1.1.0's removal rule is kept exactly under that key (LD-375). The exact mode, which an import
//! writes, is `tests/host_exact.rs`'s.

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::RwLock;
use std::time::Duration;

use fixture::Fixture;
use hostroot::{Root, feed, wait_until};
use lodi::hostscope::pm::{Origin, apt};
use lodi::hostscope::safety::Distro;

/// Held for writing while this file's `Case::new` writes its shims and for reading around every
/// spawn that might exec one (#465, the same class of defect #456 fixed for the fakehost-based
/// binaries in `tests/support/fakehost.rs`). This file defines its own `Case` and shim writer and
/// does not include that module, so it needs its own instance — one `static` per compiled test
/// binary, matching fork's per-process scope.
static SHIMS: RwLock<()> = RwLock::new(());

/// The manifest the `install` recordings were made against: `common`, a per-distro `add`, a
/// `mark_auto`, a `hold`, and one `optional` name no distribution has. The `arch` table is never
/// selected here — were the per-distro merge wrong, its name would reach the index.
const INSTALL_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"sqlite3\", \"lodi-absent-probe\"]
mark_auto = [\"tree\"]
hold = [\"sqlite3\"]
optional = [\"lodi-absent-probe\"]

[packages.debian]
add = [\"zip\"]

[packages.ubuntu]
add = [\"zip\"]

[packages.arch]
add = [\"lodi-absent-probe-arch\"]
";

/// The same manifest once the per-distro `add` has left it: `auto_remove` (the default) removes
/// what the lock records lodi as having installed, and nothing else.
const REMOVED_MANIFEST: &str = "\
[host]
version = \"1\"
packages = \"managed\"

[packages]
common = [\"tree\", \"sqlite3\"]
mark_auto = [\"tree\"]
hold = [\"sqlite3\"]
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
    /// A fresh root, armed and given the identity of `distro`, with the shims of this test
    /// first on the `PATH` every child gets and `scenario`'s recordings behind them.
    fn new(name: &str, distro: &str, scenario: &str, manifest: &str) -> Case {
        let root = Root::new(name);
        match distro {
            "debian-12" => {
                root.arm().write(
                    "etc/os-release",
                    "PRETTY_NAME=\"Debian GNU/Linux 12 (bookworm)\"\nID=debian\n\
                     VERSION_ID=\"12\"\nVERSION_CODENAME=bookworm\n",
                );
            }
            "ubuntu-24.04" => {
                root.arm().write(
                    "etc/os-release",
                    "PRETTY_NAME=\"Ubuntu 24.04.3 LTS\"\nID=ubuntu\nID_LIKE=debian\n\
                     VERSION_ID=\"24.04\"\nVERSION_CODENAME=noble\n",
                );
            }
            other => panic!("no recordings for {other}"),
        }
        root.write("etc/lodi/host.toml", manifest);
        let fixtures = fixtures_dir().join(distro).join(scenario);
        assert!(
            fixtures.is_dir(),
            "the recordings {} are not there; they are produced by \
             `docs/milestones/m-0.6/probes/hostvm.py --record-fixtures`",
            fixtures.display()
        );
        let base = support::scratch(&format!("apt-{name}"));
        let shims = base.join("bin");
        let state = base.join("state");
        fs::create_dir_all(&shims).expect("the shim directory");
        fs::create_dir_all(&state).expect("the shim state directory");
        {
            let _writing = SHIMS.write().unwrap_or_else(|e| e.into_inner());
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
        // A fresh index, so that nothing is refreshed unless a test asks for it. The product
        // reads when a file below `<root>/var/lib/apt/lists` was last touched and nothing else.
        case.root
            .write("var/lib/apt/lists/recorded_Packages", "recorded index\n");
        case
    }

    /// Which recorded state of the machine the shims replay from now on.
    fn phase(&self, phase: &str) {
        fs::write(self.state.join("phase"), format!("{phase}\n")).expect("the phase");
    }

    /// Leave the machine with an index the product must refresh. The product counts an index's
    /// age from the last time apt touched a file in its lists directory — the later of the file's
    /// mtime and its change time — and no test can set a change time into the past, so the state
    /// is reached the one way a test can: a lists directory apt never filled. The six-hour window
    /// itself is proven in `pm::apt`'s unit tests.
    fn stale_index(&self) {
        fs::remove_file(self.root.path("var/lib/apt/lists/recorded_Packages"))
            .expect("the index file");
    }

    /// Give the index file what apt gives it: an mtime set to the archive's own
    /// `Last-Modified`, `hours` ago, while the file itself was written just now.
    fn archive_dated_index(&self, hours: u64) {
        let path = self.root.path("var/lib/apt/lists/recorded_Packages");
        let when = std::time::SystemTime::now() - Duration::from_secs(hours * 3600);
        let file = fs::File::options()
            .write(true)
            .open(&path)
            .expect("the index file");
        file.set_modified(when).expect("an archive-dated index");
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
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        self.command(&args).output().expect("lodi runs")
    }

    fn plan(&self) -> Output {
        self.verb("plan", &[])
    }

    fn apply(&self, extra: &[&str]) -> Output {
        self.verb("apply", extra)
    }

    /// Every argv the shims saw, in order, exactly as the product built it.
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

    /// What every shim was told about the two variables an apt program must never be without.
    /// The product never learns the shims exist, so this is the environment a real `apt-get`
    /// would have been given, read back from the program's own side of the seam.
    fn env_lines(&self) -> Vec<String> {
        fs::read_to_string(self.state.join("env"))
            .unwrap_or_default()
            .lines()
            .map(ToString::to_string)
            .collect()
    }

    /// The root the product resolved, which is the scratch root after canonicalization: it is
    /// what the root options below carry, so an assertion has to read it the same way.
    fn resolved(&self) -> PathBuf {
        fs::canonicalize(&self.root.dir).expect("the scratch root is there")
    }

    /// Design call D2 (LD-114) puts these after the subcommand of every invocation, because
    /// every host command in this file is a `--root` one: apt's own `RootDir` for the three apt
    /// programs, and the package database below the root for `dpkg-query` and `dpkg`. On a
    /// machine — the root `/`, which the guest evidence records — there are none, and the argv
    /// is exactly the one `spec/09` §5.1 fixes.
    fn root_option(&self) -> String {
        format!("-o RootDir={}", self.resolved().display())
    }

    fn admin_option(&self) -> String {
        format!(
            "--admindir={}",
            self.resolved().join("var/lib/dpkg").display()
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
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/apt")
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
            "PATH='{path}'\nexport PATH\nLODI_APT_STATE={}\nLODI_APT_FIXTURES={}\n",
            quote(state),
            quote(fixtures)
        ),
    )
    .expect("the shim environment");
}

/// The shims: one per program the backend can reach for. Each logs its argv with `\x1f` between
/// the words and `\x1e` after the last one — a separator no argv contains, so a format string
/// with a tab and a newline in it survives the round trip — and then replays a recording.
fn write_shims(dir: &Path) {
    let bodies = [
        (
            "apt-get",
            "case \"$1\" in\n\
             update) printf 'Hit:1 the recorded index\\n' ;;\n\
             install)\n\
             \x20 if [ -p \"$state/gate\" ]; then cat \"$state/gate\" >/dev/null;\n\
             \x20 else printf '%s\\n' \"${LODI_APT_NEXT_PHASE:-after}\" > \"$state/phase\"; fi\n\
             \x20 code=$(cat \"$state/exit\" 2>/dev/null || echo 0)\n\
             \x20 if [ -f \"$state/stderr\" ]; then cat \"$state/stderr\" >&2; fi\n\
             \x20 exit \"$code\" ;;\n\
             esac\n",
        ),
        (
            "apt-cache",
            "case \"$1\" in\n\
             policy) cat \"$fixtures/policy.txt\" ;;\n\
             pkgnames)\n\
             \x20 for last in \"$@\"; do :; done\n\
             \x20 grep \"^$last\" \"$fixtures/pkgnames.txt\" 2>/dev/null || true ;;\n\
             esac\n",
        ),
        (
            "apt-mark",
            "case \"$1\" in\n\
             showauto) cat \"$fixtures/$phase/apt-mark-showauto.txt\" ;;\n\
             showhold) cat \"$fixtures/$phase/apt-mark-showhold.txt\" ;;\n\
             esac\n",
        ),
        ("dpkg-query", "cat \"$fixtures/$phase/dpkg-query.txt\"\n"),
        (
            "dpkg",
            "case \"$1\" in\n--audit) cat \"$fixtures/$phase/dpkg-audit.txt\" ;;\nesac\n",
        ),
    ];
    for (name, body) in bodies {
        let script = format!(
            "#!/bin/sh\n\
             # A test-owned shim. It replays what a real guest printed for this command.\n\
             # Everything this process inherited, read by a builtin before anything is set.\n\
             inherited=$(export -p)\n\
             . \"${{0%/*}}/shim-env\"\n\
             state=\"$LODI_APT_STATE\"\n\
             fixtures=\"$LODI_APT_FIXTURES\"\n\
             # Command substitution strips the trailing newline, so this is the bare word.\n\
             phase=$(cat \"$state/phase\" 2>/dev/null || echo before)\n\
             {{ printf '%s' '{name}'\n\
             \x20 for a in \"$@\"; do printf '\\037%s' \"$a\"; done\n\
             \x20 printf '\\036'; }} >> \"$state/log\"\n\
             {{ printf '== %s\\n' '{name}'; printf '%s\\n' \"$inherited\"; }} >> \"$state/inherited\"\n\
             # The two variables that must reach every apt program, as this program got them.\n\
             printf '%s DEBIAN_FRONTEND=%s APT_LISTCHANGES_FRONTEND=%s\\n' \\\n\
             \x20 '{name}' \"${{DEBIAN_FRONTEND-unset}}\" \
             \"${{APT_LISTCHANGES_FRONTEND-unset}}\" >> \"$state/env\"\n\
             {body}\
             exit 0\n"
        );
        let path = dir.join(name);
        fs::write(&path, script).expect("a shim");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("a shim mode");
    }
}

/// Start a real apply and stop it inside the transaction, with the package manager held at the
/// moment it was started: the `apt-get install` shim blocks on a fifo until this returns.
fn kill_inside_the_transaction(case: &Case) {
    let gate = case.state.join("gate");
    let _ = fs::remove_file(&gate);
    let status = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        Command::new("mkfifo")
            .arg(&gate)
            .status()
            .expect("mkfifo runs")
    };
    assert!(status.success(), "mkfifo {}", gate.display());
    let _ = fs::remove_file(case.state.join("log"));
    // Whatever fails below, neither the apply nor the shim it holds outlives this test.
    let mut child = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        Fixture::spawn(
            "the held apply",
            // check-host-safety: refusal — `command` appends this scratch root as `--root`.
            &mut case.command(&["host", "apply"]),
        )
    };
    wait_until("the transaction to start", || {
        case.lines()
            .iter()
            .any(|line| line.starts_with("apt-get install"))
    });
    {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        let _ = Command::new("kill")
            .arg("-KILL")
            .arg(child.id().to_string())
            .status();
    }
    let _ = child.wait();
    // Let the held shim go, without letting it record anything further: it writes no phase
    // when it was gated, so what the machine shows next is only what this test says it shows.
    feed(&gate, "");
    let _ = fs::remove_file(&gate);
}

// ------------------------------------------------------------------ (a), (h): the one argv ---

/// (a) and (h). One transaction, one `apt-get install`, and every argv the apply builds — the
/// reads that observe the machine and the writes that change it — byte for byte, in order.
#[test]
fn every_apt_invocation_of_one_apply_is_exactly_this() {
    let case = Case::new("argv", "debian-12", "install", INSTALL_MANIFEST);

    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        "+ package tree\n+ package sqlite3\n+ package zip\n\
         ~ package tree (auto)\n~ package sqlite3 (hold)\n2 action(s)\n"
    );
    assert!(
        err(&plan).contains("W_OPTIONAL_SKIPPED: `lodi-absent-probe` is optional"),
        "{}",
        err(&plan)
    );
    // A plan reads the machine and the index, and runs nothing that could change either.
    let ro = case.root_option();
    let admin = case.admin_option();
    // `${Package}`, not `${binary:Package}`: LD-312 made the installed set read under the one
    // identity the hold set, the automatic set, the candidate lookup and every declared name
    // already use (LD-305). This assertion is of the argv, so it moved with the format string;
    // what the apply concludes from the replayed recordings did not.
    let query =
        format!("dpkg-query -W {admin} -f=${{Package}}\t${{Version}}\t${{db:Status-Abbrev}}\n");
    let showauto = format!("apt-mark showauto {ro}");
    let showhold = format!("apt-mark showhold {ro}");
    let policy = format!("apt-cache policy {ro} -- tree sqlite3 lodi-absent-probe zip");
    assert_eq!(
        case.lines(),
        [
            query.clone(),
            showauto.clone(),
            showhold.clone(),
            policy.clone(),
        ]
    );

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    assert_eq!(
        case.lines(),
        [
            // the read-only pre-flight, before the apply lock exists
            query.clone(),
            showauto.clone(),
            showhold.clone(),
            policy.clone(),
            // everything read again while holding the lock, because another process may have
            // finished in between
            query.clone(),
            showauto.clone(),
            showhold.clone(),
            policy.clone(),
            // the transaction: the index is asked once more that it can still place every name
            format!("apt-cache policy {ro} -- tree sqlite3 zip"),
            format!(
                "apt-get install {ro} -y --no-install-recommends \
                 -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold \
                 -- tree sqlite3 zip"
            ),
            // the marks, after the transaction that made them meaningful
            format!("apt-mark auto {ro} -- tree"),
            format!("apt-mark hold {ro} -- sqlite3"),
            // the second dpkg-query OD-15 asks for: the versions the lock records
            query.clone(),
            showauto.clone(),
            showhold.clone(),
        ]
    );
    assert_eq!(
        case.lines()
            .iter()
            .filter(|line| line.starts_with("apt-get install"))
            .count(),
        1,
        "one transaction, always"
    );

    // Every one of them was handed the non-interactive environment, apt's changelog frontend
    // included: nothing an apply runs can stop to ask a question.
    let env = case.env_lines();
    assert_eq!(env.len(), case.lines().len(), "{env:?}");
    for line in &env {
        assert!(
            line.ends_with(" DEBIAN_FRONTEND=noninteractive APT_LISTCHANGES_FRONTEND=none"),
            "{line}"
        );
    }

    // The journal wrote that argv down, with its resolved path, before it ran.
    let journal = case.journal();
    let actions = journal["actions"].as_array().expect("actions");
    let transaction = actions
        .iter()
        .find(|entry| entry["kind"] == "packages.transaction")
        .expect("the transaction is an action");
    assert_eq!(
        transaction["commands"][0]
            .as_array()
            .expect("an argv")
            .iter()
            .map(|word| word.as_str().expect("a word").to_string())
            .collect::<Vec<String>>(),
        [
            "apt-get".to_string(),
            "install".to_string(),
            "-o".to_string(),
            format!("RootDir={}", case.resolved().display()),
            "-y".to_string(),
            "--no-install-recommends".to_string(),
            "-o".to_string(),
            "Dpkg::Options::=--force-confdef".to_string(),
            "-o".to_string(),
            "Dpkg::Options::=--force-confold".to_string(),
            "--".to_string(),
            "tree".to_string(),
            "sqlite3".to_string(),
            "zip".to_string(),
        ]
    );
    assert_eq!(
        transaction["programs"][0].as_str().expect("a path"),
        case.shims.join("apt-get").display().to_string(),
        "the journal records the path PATH resolved the bare name to"
    );
}

/// Nothing this backend can run upgrades the machine, removes what lodi did not install, or
/// repairs dpkg on the operator's behalf.
#[test]
fn no_invocation_upgrades_autoremoves_or_configures_dpkg() {
    let case = Case::new("forbidden", "debian-12", "install", INSTALL_MANIFEST);
    case.stale_index();
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let mut seen = case.lines();
    case.phase("after");
    case.manifest(REMOVED_MANIFEST);
    let removal = case.apply(&[]);
    assert!(removal.status.success(), "{}", err(&removal));
    seen.extend(case.lines());
    assert!(seen.len() > 10, "{seen:?}");
    for line in &seen {
        for forbidden in [
            "autoremove",
            "dist-upgrade",
            "upgrade",
            "--configure",
            "sudo",
            "doas",
        ] {
            assert!(!line.contains(forbidden), "{line} contains {forbidden}");
        }
    }
}

// --------------------------------------------------- (b): a second apply changes nothing ---

/// (b) A second apply against the machine the first one left plans nothing at all.
#[test]
fn a_second_apply_plans_and_changes_nothing() {
    let case = Case::new("noop", "debian-12", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    assert_eq!(case.root.read("var/lib/lodi/host/.lock"), "");

    let plan = case.plan();
    assert_eq!(out(&plan), "nothing to do\n", "{}", err(&plan));
    let before = fs::metadata(case.root.path("etc/lodi/host.lock"))
        .and_then(|meta| meta.modified())
        .expect("the lock's mtime");
    std::thread::sleep(Duration::from_millis(20));
    let apply = case.apply(&[]);
    assert_eq!(out(&apply), "nothing to do\n", "{}", err(&apply));
    let after = fs::metadata(case.root.path("etc/lodi/host.lock"))
        .and_then(|meta| meta.modified())
        .expect("the lock's mtime");
    assert_eq!(before, after, "the second apply rewrote the lock");
    assert!(
        !case
            .lines()
            .iter()
            .any(|line| line.starts_with("apt-get install")),
        "{:?}",
        case.lines()
    );
}

// ------------------------------------------- (c): the marks, the holds and `auto_remove` ---

/// (c) The lock records the marks the apply set, `auto_remove` removes what the lock records
/// lodi as having installed, and a package lodi never installed is left exactly as it is.
#[test]
fn the_lock_records_the_marks_and_bounds_auto_remove() {
    let case = Case::new("marks", "debian-12", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());

    let lock = case.lock();
    let packages = lock["packages"].as_object().expect("the lock's packages");
    assert_eq!(
        packages.keys().map(String::as_str).collect::<Vec<&str>>(),
        ["sqlite3", "tree", "zip"],
        "the lock records what lodi installed, and nothing else"
    );
    assert_eq!(packages["tree"]["mark"], "auto");
    assert_eq!(packages["sqlite3"]["held"], true);
    assert_eq!(packages["zip"]["mark"], "manual");
    // Every recorded version is the one the machine showed **after** the transaction.
    let after = fs::read_to_string(case.fixtures.join("after/dpkg-query.txt")).expect("recording");
    for (name, record) in packages {
        let version = record["version"].as_str().expect("a version");
        assert!(
            after.lines().any(|line| {
                let mut fields = line.split('\t');
                fields.next() == Some(name.as_str())
                    && fields.next() == Some(version)
                    // The middle character of dpkg's status is the one that says "installed";
                    // the first is the wanted state, which a hold changes to `h`.
                    && fields.next().and_then(|status| status.chars().nth(1)) == Some('i')
            }),
            "{name} {version} is not what dpkg-query showed"
        );
    }

    // The per-distro `add` leaves the manifest. `zip` goes, because the lock says lodi put it
    // there; nothing else does, whatever else the machine has installed.
    case.manifest(REMOVED_MANIFEST);
    let removal = case.apply(&[]);
    assert!(removal.status.success(), "{}", err(&removal));
    assert!(out(&removal).contains("- package zip"), "{}", out(&removal));
    let transaction: Vec<String> = case
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("apt-get install"))
        .collect();
    assert_eq!(transaction.len(), 1, "{transaction:?}");
    assert!(
        transaction[0].ends_with(" zip-"),
        "a removal travels in the transaction with apt's own suffix: {}",
        transaction[0]
    );
}

/// The lock records the whole **declared** set that is installed, not just what this one apply
/// put there. A declared package that was already on the machine is still one Lodi manages: if
/// it never reached the lock, `dpkg-query` and the lock would disagree, and that package could
/// never leave the manifest cleanly, because `auto_remove` is bounded by the lock.
///
/// This is not hypothetical. A guest run of the killed-transaction case produced exactly this
/// state — one of two packages installed when the apply resumed — and the lock recorded only the
/// other one.
#[test]
fn the_lock_records_a_declared_package_that_was_already_installed() {
    let case = Case::new(
        "already-there",
        "debian-12",
        "interrupted",
        INTERRUPTED_MANIFEST,
    );
    // The machine already has `unzip` and not `patch`, and nothing Lodi did put it there.
    case.phase("partial");
    let plan = case.plan();
    assert_eq!(
        out(&plan),
        "+ package patch\n1 action(s)\n",
        "{}",
        err(&plan)
    );

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let transactions: Vec<String> = case
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("apt-get install"))
        .collect();
    assert_eq!(transactions.len(), 1, "{transactions:?}");
    assert!(transactions[0].ends_with(" patch"), "{}", transactions[0]);

    let lock = case.lock();
    assert_eq!(
        lock["packages"]
            .as_object()
            .expect("packages")
            .keys()
            .map(String::as_str)
            .collect::<Vec<&str>>(),
        ["patch", "unzip"],
        "the lock is the record of the managed set, not of this apply's installs"
    );
}

// ------------------------------------------------- (d): `absent` joins the same transaction ---

/// (d) `absent` removes an installed package, in the same transaction as an install, so that
/// apt solves the whole change at once.
#[test]
fn absent_removes_in_the_same_transaction_as_an_install() {
    let case = Case::new("absent", "debian-12", "install", INSTALL_MANIFEST);
    assert!(case.apply(&[]).status.success());
    // Everything is installed now. The manifest keeps `sqlite3` and `zip` and declares `tree`
    // absent, so one transaction has to take `tree` away.
    // `hold` is kept so that this plan is only about the removal.
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"sqlite3\", \"zip\"]\n\
         hold = [\"sqlite3\"]\nabsent = [\"tree\"]\n",
    );
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        "- package tree\n1 action(s)\n",
        "{}",
        err(&plan)
    );

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let transactions: Vec<String> = case
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("apt-get install"))
        .collect();
    assert_eq!(transactions.len(), 1, "{transactions:?}");
    assert!(transactions[0].ends_with(" tree-"), "{}", transactions[0]);
}

// ------------------------------------------ (e), (f): unknown, optional and version-qualified ---

/// (e) A name the index does not offer is an error; when the index knows something close it says
/// so, and when it knows nothing close it says what `optional` is for instead. The same name in
/// `optional` is a warning and exit 0.
#[test]
fn an_unknown_name_is_an_error_and_an_optional_one_is_skipped() {
    let case = Case::new("unknown", "debian-12", "install", INSTALL_MANIFEST);

    // A typo of a package this guest's index really does offer. `logtail` is in the recorded
    // `apt-cache pkgnames lo` answer, one edit away.
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"logtai\"]\n",
    );
    let typo = case.plan();
    assert_eq!(typo.status.code(), Some(3), "{}", err(&typo));
    assert!(err(&typo).contains("E_UNKNOWN_PACKAGE"), "{}", err(&typo));
    assert!(
        err(&typo).contains("the nearest names the index knows are logtail"),
        "{}",
        err(&typo)
    );
    // The nearest names come from the index, asked for by the name's own first two characters.
    assert!(
        case.lines()
            .contains(&format!("apt-cache pkgnames {} -- lo", case.root_option())),
        "{:?}",
        case.lines()
    );

    case.manifest("[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"lodi-absent-probe\"]\n");
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3), "{}", err(&plan));
    assert!(
        err(&plan).contains("E_UNKNOWN_PACKAGE") && err(&plan).contains("lodi-absent-probe"),
        "{}",
        err(&plan)
    );
    assert!(
        err(&plan).contains("`[packages] optional`"),
        "with nothing close in the index, the hint says what optional is for: {}",
        err(&plan)
    );
    // Nothing was planned, journalled or changed by a refusal.
    assert!(!case.root.exists("var/lib/lodi"));

    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"lodi-absent-probe\"]\n\
         optional = [\"lodi-absent-probe\"]\n",
    );
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", err(&apply));
    // The warning is printed even though there was then nothing to do.
    assert!(
        err(&apply).contains("W_OPTIONAL_SKIPPED"),
        "{}",
        err(&apply)
    );
    assert_eq!(out(&apply), "nothing to do\n", "{}", err(&apply));
    assert!(
        !case
            .lines()
            .iter()
            .any(|line| line.starts_with("apt-get install")),
        "{:?}",
        case.lines()
    );
}

/// (f) A version in a package name is refused at the manifest: host packages track the
/// distribution, and this build writes no constraint, pin or snapshot (OD-15, D9/LD-121).
#[test]
fn a_version_qualified_name_is_unsupported() {
    let case = Case::new("qualified", "debian-12", "install", INSTALL_MANIFEST);
    case.manifest(
        "[host]\nversion = \"1\"\npackages = \"managed\"\n\n[packages]\ncommon = [\"tree=1.0\"]\n",
    );
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3));
    assert!(err(&plan).contains("E_UNSUPPORTED"), "{}", err(&plan));
    assert!(case.lines().is_empty(), "the index was not even read");
}

// --------------------------------------------------------------- the six-hour index refresh ---

/// An index apt has just fetched is fresh, whatever date its files carry (LD-378). apt stamps
/// each list file with the archive's own `Last-Modified`, so an index `apt-get update` fetched a
/// minute ago has an mtime hours old whenever the archive has published nothing since. The W2
/// row met exactly that on an Ubuntu guest: every plan after a refresh planned the refresh
/// again. A converged machine plans nothing, and its apply runs no `apt-get update`.
#[test]
fn an_index_dated_by_its_archive_is_as_fresh_as_its_last_update() {
    let case = Case::new("refresh-idle", "debian-12", "install", INSTALL_MANIFEST);
    let first = case.apply(&[]);
    assert!(first.status.success(), "{}", err(&first));
    case.archive_dated_index(9);
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(out(&plan), "nothing to do\n", "{}", err(&plan));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    assert_eq!(out(&apply), "nothing to do\n", "{}", err(&apply));
    assert!(
        !case
            .lines()
            .iter()
            .any(|line| line.starts_with("apt-get update")),
        "{:?}",
        case.lines()
    );
}

/// An index older than six hours, or none at all, is refreshed as an action in the journal like
/// any other, and `--no-update` skips it. Nothing else in the apply changes because of it.
#[test]
fn a_stale_index_is_refreshed_as_an_action_and_no_update_skips_it() {
    let case = Case::new("refresh", "debian-12", "install", INSTALL_MANIFEST);
    case.stale_index();
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        format!(
            "~ index (apt-get update {})\n+ package tree\n+ package sqlite3\n+ package zip\n\
             ~ package tree (auto)\n~ package sqlite3 (hold)\n3 action(s)\n",
            case.root_option()
        )
    );

    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let lines = case.lines();
    let update = lines
        .iter()
        .position(|line| line.starts_with("apt-get update"));
    let install = lines
        .iter()
        .position(|line| line.starts_with("apt-get install"));
    assert!(
        update.is_some() && update < install,
        "the refresh comes first: {lines:?}"
    );
    let journal = case.journal();
    assert_eq!(journal["actions"][0]["kind"], "packages.index");
    assert_eq!(journal["actions"][0]["commands"][0][0], "apt-get");
    assert_eq!(journal["actions"][0]["commands"][0][1], "update");

    // `--no-update` leaves the stale index alone and installs anyway.
    let case = Case::new("refresh-skipped", "debian-12", "install", INSTALL_MANIFEST);
    case.stale_index();
    let apply = case.apply(&["--no-update"]);
    assert!(apply.status.success(), "{}", err(&apply));
    assert!(
        !case
            .lines()
            .iter()
            .any(|line| line.starts_with("apt-get update")),
        "{:?}",
        case.lines()
    );
    assert!(
        case.lines()
            .iter()
            .any(|line| line.starts_with("apt-get install")),
        "{:?}",
        case.lines()
    );
}

// -------------------------------------------------------- (g): the three classifications ---

/// (g) The three classes of an apply killed inside the transaction, each produced from the same
/// real kill and a different recorded machine state.
///
/// `incomplete` — the machine still shows what it showed before: the next apply simply plans
/// again. `completed` — it shows what the transaction was to leave. `ambiguous` — it shows
/// neither, and lodi stops rather than guess, naming the journal and the operator's own repair.
#[test]
fn a_killed_transaction_is_classified_incomplete_completed_or_ambiguous() {
    // incomplete: nothing was installed before the kill, and nothing is installed after it.
    let case = Case::new(
        "kill-incomplete",
        "debian-12",
        "interrupted",
        INTERRUPTED_MANIFEST,
    );
    kill_inside_the_transaction(&case);
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(
        out(&plan).contains("is outstanding: incomplete at p1"),
        "{}",
        out(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    assert!(
        out(&apply).contains("was incomplete at p1"),
        "{}",
        out(&apply)
    );

    // completed: the transaction had in fact finished when the kill landed.
    let case = Case::new(
        "kill-completed",
        "debian-12",
        "interrupted",
        INTERRUPTED_MANIFEST,
    );
    kill_inside_the_transaction(&case);
    case.phase("after");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(
        out(&plan).contains("is outstanding: complete"),
        "{}",
        out(&plan)
    );

    // ambiguous: one of the two packages is there and the other is not, which is neither the
    // state before the transaction nor the state after it.
    let case = Case::new(
        "kill-ambiguous",
        "debian-12",
        "interrupted",
        INTERRUPTED_MANIFEST,
    );
    kill_inside_the_transaction(&case);
    case.phase("partial");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert!(
        out(&plan).contains("is outstanding: ambiguous at p1"),
        "{}",
        out(&plan)
    );
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", err(&apply));
    let message = err(&apply);
    assert!(message.contains("E_JOURNAL_AMBIGUOUS"), "{message}");
    assert!(
        message.contains("dpkg --configure -a"),
        "the ambiguous stop names the operator's own repair: {message}"
    );
    assert!(
        message.contains("--resolved"),
        "the ambiguous stop names how a human clears it: {message}"
    );
    let id = hostroot::journals(&case.root)
        .pop()
        .expect("a journal")
        .file_stem()
        .expect("a stem")
        .to_string_lossy()
        .into_owned();
    assert!(message.contains(&id), "{message}");

    // The declaration is what clears it, and it is recorded in the next journal. The machine
    // is still the one lodi refused to reason about: a human has looked, not tidied up.
    let resolved = case.apply(&["--resolved", &id]);
    assert!(resolved.status.success(), "{}", err(&resolved));
    assert_eq!(case.journal()["resolved"], serde_json::json!(id));
}

/// A package database that is not clean makes the answer ambiguous whatever the installed set
/// says, and the complaint `dpkg --audit` really printed on the guest is what it names.
#[test]
fn a_dpkg_left_part_way_through_is_ambiguous_and_lodi_does_not_repair_it() {
    let case = Case::new(
        "kill-dirty",
        "debian-12",
        "interrupted",
        INTERRUPTED_MANIFEST,
    );
    let audit =
        fs::read_to_string(case.fixtures.join("dirty/dpkg-audit.txt")).expect("the recording");
    assert!(
        !audit.trim().is_empty(),
        "the `dirty` recording is what a half-configured dpkg prints; it cannot be empty"
    );
    kill_inside_the_transaction(&case);
    case.phase("dirty");
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", err(&apply));
    assert!(
        err(&apply).contains("the package database is not clean"),
        "{}",
        err(&apply)
    );
    assert!(
        err(&apply).contains("dpkg --configure -a"),
        "{}",
        err(&apply)
    );
    // Lodi named the repair; it did not run it.
    assert!(
        !case.lines().iter().any(|line| line.contains("--configure")),
        "{:?}",
        case.lines()
    );
    // The message is a sentence, and the hint asks for something an operator can act on.
    // An unclean database is a condition of the machine, not a subject that can be put into a
    // state, so neither text may hold it where a path or a `package NAME` belongs (R-1).
    assert!(
        !err(&apply).contains("matches neither the state before it"),
        "an unclean database names no bracket to compare against: {}",
        err(&apply)
    );
    assert!(
        err(&apply).contains("put the machine into the state you want"),
        "{}",
        err(&apply)
    );
}

/// An `apt-get install` that fails stops the apply with `E_APPLY` (exit 8) carrying what apt
/// itself printed, and the journal records the action it failed at.
#[test]
fn a_failing_transaction_is_apply_error_carrying_what_apt_printed() {
    let case = Case::new("fails", "debian-12", "install", INSTALL_MANIFEST);
    fs::write(case.state.join("exit"), "100\n").expect("the shim's exit code");
    // This one string is the test's own, not a recording: a failure is what is being staged, and
    // no successful guest run of this package could have produced it.
    fs::write(
        case.state.join("stderr"),
        "E: Unable to correct problems, you have held broken packages.\n",
    )
    .expect("the shim's complaint");
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", err(&apply));
    assert!(err(&apply).contains("E_APPLY"), "{}", err(&apply));
    assert!(err(&apply).contains("exited 100"), "{}", err(&apply));
    assert!(
        err(&apply).contains("held broken packages"),
        "the diagnostic carries what apt itself said: {}",
        err(&apply)
    );
    let journal = case.journal();
    assert_eq!(journal["actions"][0]["id"], "p1");
    assert!(
        !case.root.exists("etc/lodi/host.lock"),
        "no lock was written"
    );
}

// --------------------------------------------------------------- both apt distributions ---

/// The same manifest on Ubuntu 24.04, over that guest's own recordings: one transaction, the
/// per-distro `add` selected by the distribution the root declares.
#[test]
fn ubuntu_24_04_installs_the_same_way_from_its_own_recordings() {
    let case = Case::new("ubuntu", "ubuntu-24.04", "install", INSTALL_MANIFEST);
    let plan = case.plan();
    assert!(plan.status.success(), "{}", err(&plan));
    assert_eq!(
        out(&plan),
        "+ package tree\n+ package sqlite3\n+ package zip\n\
         ~ package tree (auto)\n~ package sqlite3 (hold)\n2 action(s)\n"
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", err(&apply));
    let transactions: Vec<String> = case
        .lines()
        .into_iter()
        .filter(|line| line.starts_with("apt-get install"))
        .collect();
    assert_eq!(
        transactions,
        [format!(
            "apt-get install {} -y --no-install-recommends \
             -o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold \
             -- tree sqlite3 zip",
            case.root_option()
        )]
    );
    assert_eq!(case.journal()["distro"], "ubuntu");
    let packages = case.lock()["packages"].as_object().expect("packages").len();
    assert_eq!(packages, 3);
}

/// Every recording this suite replays came off a guest, and `PROVENANCE.json` says which run
/// and which command produced each one.
#[test]
fn the_recordings_say_where_they_came_from() {
    let provenance: serde_json::Value = serde_json::from_str(
        &fs::read_to_string(fixtures_dir().join("PROVENANCE.json"))
            .expect("tests/fixtures/host/apt/PROVENANCE.json exists"),
    )
    .expect("it parses");
    assert_eq!(provenance["task"], "M-0.6 T-4");
    let commands = provenance["commands"]
        .as_object()
        .expect("the command each reading came from");
    for name in [
        "dpkg-query.txt",
        "apt-mark-showauto.txt",
        "apt-mark-showhold.txt",
        "dpkg-audit.txt",
        "policy.txt",
        "pkgnames.txt",
    ] {
        assert!(
            commands.contains_key(name),
            "no command recorded for {name}"
        );
    }
    for distro in ["debian-12", "ubuntu-24.04"] {
        let run = &provenance["runs"][distro];
        assert!(run["guestImage"].is_string(), "{distro} names no image");
        assert!(run["recordedAt"].is_string(), "{distro} has no date");
        for state in ["before", "after"] {
            assert!(
                fixtures_dir()
                    .join(distro)
                    .join("install")
                    .join(state)
                    .join("dpkg-query.txt")
                    .is_file(),
                "{distro}/install/{state} was not recorded"
            );
        }
    }
}

// ------------------------------------------------------- R-2: where an installed version came from ---

/// `apt-cache policy NAME` states the sources of the version that is **installed**, and only of
/// that one.
///
/// This is the reading R-2 is built on, and the reason the test it replaced was wrong: apt counts
/// `/var/lib/dpkg/status` as a source at priority 100 for everything installed, so "the index
/// offers a candidate" was true of a hand-installed `.deb` as much as of a Debian package.
#[test]
fn a_policy_version_table_yields_the_installed_versions_sources_alone() {
    let sources = apt::parse_policy_sources(
        "docker-ce:\n\
         \x20 Installed: 5:27.3.1-1~deb12\n\
         \x20 Candidate: 5:28.0.0-1~deb12\n\
         \x20 Version table:\n\
         \x20    5:28.0.0-1~deb12 500\n\
         \x20       500 http://download.docker.com/linux/debian bookworm/stable amd64 Packages\n\
         \x20*** 5:27.3.1-1~deb12 100\n\
         \x20       100 /var/lib/dpkg/status\n\
         lodi-hand-built:\n\
         \x20 Installed: 0-1\n\
         \x20 Candidate: 0-1\n\
         \x20 Version table:\n\
         \x20*** 0-1 100\n\
         \x20       100 /var/lib/dpkg/status\n",
    );
    assert_eq!(
        sources.get("docker-ce").map(Vec::as_slice),
        Some(["/var/lib/dpkg/status".to_string()].as_slice()),
        "the candidate's source belongs to a version this machine does not have"
    );
    assert_eq!(
        sources.get("lodi-hand-built").map(Vec::as_slice),
        Some([apt::STATUS_SOURCE.to_string()].as_slice())
    );
}

/// The no-argument listing is read for `o=` and `l=` and never for `origin`, which is a host
/// name; and a field that could be an address is not written into the emitted file at all.
#[test]
fn the_release_table_is_read_for_labels_and_never_for_an_address() {
    let table = apt::parse_release_table(
        " 500 http://download.docker.com/linux/debian bookworm/stable amd64 Packages\n\
         \x20    release o=Docker,a=bookworm,n=bookworm,l=Docker CE,c=stable,b=amd64\n\
         \x20    origin download.docker.com\n\
         \x20500 http://deb.debian.org/debian bookworm/main amd64 Packages\n\
         \x20    release v=12.11,o=Debian,a=stable,n=bookworm,l=Debian,c=main,b=amd64\n\
         \x20    origin deb.debian.org\n",
    );
    let docker = table
        .get("http://download.docker.com/linux/debian bookworm/stable amd64 Packages")
        .expect("the Docker source");
    assert_eq!(
        (docker.origin.as_str(), docker.component.as_str()),
        ("Docker", "stable")
    );
    assert_eq!(
        table.len(),
        2,
        "the `origin` lines became no entry of their own"
    );
    for release in table.values() {
        for field in [
            &release.origin,
            &release.label,
            &release.suite,
            &release.component,
        ] {
            assert!(!field.contains('.'), "a host name reached a field: {field}");
        }
    }
    assert_eq!(apt::address_free("Docker"), Some("Docker"));
    for addressed in ["download.docker.com", "http://example", "a/b", "x@y", "a:b"] {
        assert_eq!(apt::address_free(addressed), None, "{addressed}");
    }
}

/// The four classes, from the two readings together: the distribution's own, one of its own
/// suites a fresh install does not enable, a third party by its label, and the local database
/// alone. A version several sources offer takes the best answer any of them gives.
#[test]
fn an_installed_version_is_classified_by_its_sources_release_fields() {
    let table = apt::parse_release_table(
        " 500 debian-main\n\
         \x20    release o=Debian,a=stable,l=Debian,c=main\n\
         \x20500 debian-backports\n\
         \x20    release o=Debian,a=bookworm-backports,l=Debian Backports,c=main\n\
         \x20500 docker\n\
         \x20    release o=Docker,a=bookworm,l=Docker CE,c=stable\n\
         \x20500 unlabelled\n\
         \x20    release a=now\n",
    );
    let sources =
        |names: &[&str]| -> Vec<String> { names.iter().map(ToString::to_string).collect() };
    let of = |names: &[&str]| apt::classify(Distro::Debian, &sources(names), &table);
    assert_eq!(of(&["debian-main", apt::STATUS_SOURCE]), Origin::Base);
    assert_eq!(
        of(&["debian-backports", apt::STATUS_SOURCE]),
        Origin::Suite("bookworm-backports".to_string())
    );
    assert_eq!(
        of(&["docker", apt::STATUS_SOURCE]),
        Origin::ThirdParty("Docker, bookworm/stable".to_string())
    );
    assert_eq!(
        of(&["unlabelled"]),
        Origin::ThirdParty(format!("{}, now", apt::UNLABELLED)),
        "a source that states no origin is not this distribution's own"
    );
    assert_eq!(of(&[apt::STATUS_SOURCE]), Origin::LocalOnly);
    assert_eq!(
        of(&["docker", "debian-main", apt::STATUS_SOURCE]),
        Origin::Base,
        "one source that can install it on a fresh machine is enough"
    );
    // Only the first two may be declared, which is what D4 and LD-272 say.
    assert!(Origin::Base.is_declarable() && Origin::Suite(String::new()).is_declarable());
    assert!(!Origin::LocalOnly.is_declarable());
    assert!(!Origin::ThirdParty(String::new()).is_declarable());
}

/// A machine with no index at all: `apt-cache policy` with no argument names one source, its
/// own package database. Every version on such a machine would classify `LocalOnly`, which
/// would be wrong — the machine was never asked, and [`apt::has_index`] is how the backend
/// tells "asked and told nothing" from "never asked".
#[test]
fn a_release_table_of_the_package_database_alone_is_no_index() {
    let none = apt::parse_release_table(&format!(
        " 100 {}\n\
         \x20    release a=now\n",
        apt::STATUS_SOURCE
    ));
    assert_eq!(none.len(), 1);
    assert!(
        !apt::has_index(&none),
        "the machine's own database is not an index"
    );

    let some = apt::parse_release_table(&format!(
        " 100 {}\n\
         \x20    release a=now\n\
         \x20500 debian-main\n\
         \x20    release o=Debian,a=stable,l=Debian,c=main\n",
        apt::STATUS_SOURCE
    ));
    assert!(apt::has_index(&some), "one fetched source is an index");
    assert!(apt::has_index(&apt::parse_release_table(
        " 500 debian-main\n\x20    release o=Debian,a=stable,l=Debian,c=main\n"
    )));
    assert!(
        !apt::has_index(&apt::parse_release_table("")),
        "no source at all is no index either"
    );
}

/// `Origin::Unchecked` is declarable and the other unknown answer is not: the difference
/// between a machine that could not be asked and a name no repository serves.
#[test]
fn the_unchecked_origin_is_declared_and_the_local_only_one_is_not() {
    assert!(Origin::Unchecked.is_declarable());
    assert!(!Origin::LocalOnly.is_declarable());
}

/// Rule (LD-357): a package manager the host scope starts inherits no variable of the caller's
/// environment beyond the fixed set, and its `PATH` is the fixed path. The caller here carries
/// variables that would steer apt if they reached it; the shims' own record of what they
/// inherited shows none of them.
#[test]
fn a_package_manager_inherits_nothing_but_the_fixed_environment() {
    let case = Case::new("fixed-env", "debian-12", "install", INSTALL_MANIFEST);
    let _ = fs::remove_file(case.state.join("inherited"));
    let output = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        case
            // check-host-safety: refusal — `command` appends this scratch root as `--root`.
            .command(&["host", "plan"])
            .env("APT_CONFIG", "/nonexistent/planted.conf")
            .env("LD_LIBRARY_PATH", "/nonexistent")
            .env("LODI_PLANTED", "planted")
            .output()
            .expect("lodi runs")
    };
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
            "APT_LISTCHANGES_FRONTEND",
        ],
    );
    assert!(checked > 0, "no package manager ran:\n{inherited}");
}

/// The scope document discloses what the transaction's two dpkg options do on the real machine
/// (LD-358): the options it names are the ones the argv above carries, it says they apply on the
/// real machine as under `--root`, and it says which version of a changed configuration file wins
/// and that Lodi does not report it. The document is read as it is in the tree, its line wrapping
/// folded away, so the assertion holds the text and not its layout.
#[test]
fn the_host_scope_document_discloses_force_confold_on_the_real_machine() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("docs/scopes/host.md");
    let text = fs::read_to_string(&path).expect("docs/scopes/host.md");
    let text = text.split_whitespace().collect::<Vec<&str>>().join(" ");
    for needle in [
        "That invocation carries \
         `-o Dpkg::Options::=--force-confdef -o Dpkg::Options::=--force-confold`, \
         on the real machine exactly as under `--root`",
        "dpkg **keeps the machine's version** and leaves the package's new one beside it \
         (normally as `.dpkg-dist`)",
        "Lodi does not report which files that happened to.",
        "Compare them yourself after an apply.",
    ] {
        assert!(
            text.contains(needle),
            "docs/scopes/host.md no longer discloses: {needle}"
        );
    }
}
