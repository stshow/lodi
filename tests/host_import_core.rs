//! M-Import T-1: the import core — candidate set, baseline subtraction, emitter.
//!
//! Offline, deterministic and guestless. No package manager runs here and none could: this
//! machine has none and must never get one (`AGENTS.md` §8). What runs is the product's own
//! library over a directory of test-owned shims first on `PATH`, which is the whole seam by
//! design (D3, LD-115). Each shim appends its argv to a log and replays a recording, so the
//! assertions below are of two kinds and both are exact:
//!
//! - **what lodi would run**: every argv, from the log — and for this module that is the whole of
//!   acceptance row (f), because a read-only module is one whose log holds only read commands;
//! - **what lodi emits**: the bytes, compared against a committed expected file.
//!
//! # Where the recordings come from, and what they are not
//!
//! `tests/fixtures/host/import/<distro>/<scenario>/` is **hand-authored** from public package
//! metadata — the shapes `dpkg-query`, `apt-cache policy`, `pacman -Qi` and `pacman -Si` print —
//! and is not a capture of any machine. It could not be: `AGENTS.md` §8 forbids reading this
//! machine's package database, and this package creates no guest. `PROVENANCE.json` beside them
//! says so in the same words, and M-Import T-5 is the package that records the real readings off
//! fresh guests. Two `fresh` scenarios therefore model a fresh install from each distribution's
//! own installer, not a cloud image.
//!
//! `LODI_IMPORT_STATE` and `LODI_IMPORT_FIXTURES` are **test-harness** names. No product source
//! file reads either one; they are how this test tells its own shims which recording to replay,
//! and `nothing_this_module_builds_mutates_a_machine` reads the log they produce.
//!
//! Setting `LODI_RECORD_EXPECTED=1` rewrites the committed `expected.toml` files instead of
//! comparing against them. It is the same idiom as `LODI_RECORD_SPECIMENS` (M-1.0 T-3) and is
//! never set by a validation command: an unset variable compares, which is what a gate runs.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`,
/// LD-286): `hostroot.rs` is included by `#[path]` and takes the helper from here.
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, RwLock};

use hostroot::Root;
use lodi::hostscope::import::{self, Snapshot, baseline};
use lodi::hostscope::safety::{Gate, Operation};
use lodi::hostscope::{manifest, plan, pm};

/// The instant every scenario is emitted at: a real time of day, so that the hour flooring the
/// emitter does is visible in the committed bytes rather than assumed.
const CAPTURED_AT: &str = "2026-09-22T01:37:11Z";
/// What the emitter must write for it.
const FLOORED: &str = "2026-09-22T01:00:00Z";
/// What the import writes for that capture: the last UTC day that had ended (LD-396, LD-444).
const ENDED_DAY: &str = "2026-09-21T00:00:00Z";

/// Every scenario, as `(distro directory, scenario directory)`.
const SCENARIOS: &[(&str, &str)] = &[
    ("arch", "chosen"),
    ("arch", "fresh"),
    ("arch", "off-base"),
    ("debian-12", "ambiguous"),
    ("debian-12", "ambiguous-multi-uri"),
    ("debian-12", "ambiguous-unlisted"),
    ("debian-12", "chosen"),
    ("debian-12", "compressed"),
    ("debian-12", "fresh"),
    ("debian-12", "mirror"),
    ("debian-12", "mixed-backports"),
    ("debian-12", "multiarch"),
    ("debian-12", "off-base"),
    ("debian-12", "off-index"),
    ("debian-12", "own-suite"),
    ("debian-12", "sources"),
    ("debian-12", "unattributed"),
    ("ubuntu-24.04", "chosen"),
    ("ubuntu-24.04", "fresh"),
    ("ubuntu-24.04", "one-line-proposed"),
    ("ubuntu-24.04", "proposed"),
    ("ubuntu-26.04", "chosen"),
    ("ubuntu-26.04", "fresh"),
];

/// A host name planted in every scratch root. Acceptance row (g) is asserted against a machine
/// that really has one: the emitter must not carry it out, and an assertion about a name that was
/// never there would prove nothing.
const HOST_NAME: &str = "a-machine-that-was-read";

/// The shims are one directory shared by the whole binary, so the recording they replay is
/// selected by a file rather than by an environment variable, and the cases that use them run one
/// at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Held for writing while this file's own [`write_shims`] writes them, and for reading around
/// every spawn that might exec one (#465, the same class of defect #456 fixed for the
/// fakehost-based binaries in `tests/support/fakehost.rs`). `SERIAL` already keeps this file's own
/// cases from overlapping, but the write and every spawn still take this guard so a shim write can
/// never race a spawn from anywhere else in this process.
static SHIMS: RwLock<()> = RwLock::new(());

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/import")
}

/// The shim directory, first on this process's `PATH`, and the state directory the shims write
/// their log into. Both are set up exactly once, behind a `Once`.
///
/// `hostroot::shims()` is called first and deliberately: it prepends its own inert `apt-get` and
/// `dpkg-query`, and this directory has to sit ahead of those, not behind them.
fn shims() -> (PathBuf, PathBuf) {
    static SETUP: Once = Once::new();
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let base = BASE
        .get_or_init(|| support::scratch("import-shims"))
        .clone();
    let (bin, state) = (base.join("bin"), base.join("state"));
    SETUP.call_once(|| {
        hostroot::shims();
        fs::create_dir_all(&bin).expect("the shim directory");
        fs::create_dir_all(&state).expect("the shim state directory");
        write_shims(&bin, &state);
        let existing = std::env::var("PATH").unwrap_or_default();
        let joined = format!("{}:{existing}", bin.display());
        // SAFETY: every `PATH` access in this binary happens after this `Once` completes —
        // the shims are resolved through it, and no test here reads `PATH` any other way.
        unsafe { std::env::set_var("PATH", joined) };
    });
    (bin, state)
}

/// The shims: one per program a backend of either family can reach for. Each logs its argv with
/// `\x1f` between the words and `\x1e` after the last, a separator no argv contains, and then
/// prints what a machine would have printed. The fixture directory to read comes from a file the
/// case writes, so no environment variable moves between cases.
fn write_shims(bin: &Path, state: &Path) {
    let _writing = SHIMS.write().unwrap_or_else(|e| e.into_inner());
    let bodies = [
        // Neither of these is ever reached by the import core, and that is the point: if one
        // were, it would be in the log and `nothing_this_module_builds_mutates_a_machine` would
        // say which.
        ("apt-get", "exit 0\n"),
        ("dpkg", "exit 0\n"),
        // dpkg-query is the one shim that reads the format string it was given, because a
        // real dpkg-query does: `${binary:Package}` prints `libc6:amd64` for a Multi-Arch
        // package and `${Package}` prints `libc6` (LD-305, LD-312). A scenario that records
        // both answers proves which one the product asks for; one that records only the plain
        // answer models a machine with no Multi-Arch package, where the two are the same text.
        (
            "dpkg-query",
            "case \"$*\" in\n\
             *Priority*) base=dpkg-survey ;;\n\
             *) base=dpkg-query ;;\n\
             esac\n\
             case \"$*\" in\n\
             *binary:Package*) file=\"$fixtures/$base-binary.txt\" ;;\n\
             *) file=\"$fixtures/$base.txt\" ;;\n\
             esac\n\
             [ -f \"$file\" ] || file=\"$fixtures/$base.txt\"\n\
             cat \"$file\"\n",
        ),
        // `apt-cache policy` answers two different questions: with names it prints each one's
        // version table, and with none it prints the release fields of every enabled source.
        // A real apt-cache does both from the same subcommand, so the shim does too.
        (
            "apt-cache",
            "names=0; skip=0\n\
             for a in \"$@\"; do\n\
             \x20 if [ \"$skip\" = 1 ]; then skip=0; continue; fi\n\
             \x20 case \"$a\" in\n\
             \x20 policy) ;;\n\
             \x20 -o) skip=1 ;;\n\
             \x20 -*) ;;\n\
             \x20 *) names=1 ;;\n\
             \x20 esac\n\
             done\n\
             case \"$1\" in\n\
             policy)\n\
             \x20 if [ \"$names\" = 1 ]; then\n\
             \x20   cat \"$fixtures/policy.txt\"\n\
             \x20 else\n\
             \x20   cat \"$fixtures/policy-release.txt\"\n\
             \x20 fi ;;\n\
             esac\n",
        ),
        // `pacman-conf --repo-list`: the machine's own enabled repositories. A scenario that
        // records none has no such file, and the shim then answers as a machine with no
        // `pacman-conf` does — nothing, at a non-zero status — which is the fallback path.
        (
            "pacman-conf",
            "if [ -f \"$fixtures/pacman-conf-repo-list.txt\" ]; then\n\
             \x20 cat \"$fixtures/pacman-conf-repo-list.txt\"\n\
             else\n\
             \x20 exit 1\n\
             fi\n",
        ),
        (
            "apt-mark",
            "case \"$1\" in\n\
             showauto) cat \"$fixtures/apt-mark-showauto.txt\" ;;\n\
             showhold) cat \"$fixtures/apt-mark-showhold.txt\" ;;\n\
             esac\n",
        ),
        (
            "pacman",
            "case \"$1\" in\n\
             -Q) cat \"$fixtures/pacman-Q.txt\" ;;\n\
             -Qe) cat \"$fixtures/pacman-Qe.txt\" ;;\n\
             -Qi) cat \"$fixtures/pacman-Qi.txt\" ;;\n\
             -Qqem)\n\
             \x20 if [ -s \"$fixtures/pacman-Qqem.txt\" ]; then\n\
             \x20   cat \"$fixtures/pacman-Qqem.txt\"\n\
             \x20 else\n\
             \x20   exit 1\n\
             \x20 fi ;;\n\
             -Si)\n\
             \x20 cat \"$fixtures/pacman-Si.txt\"\n\
             \x20 if [ -f \"$fixtures/pacman-Si.err\" ]; then\n\
             \x20   cat \"$fixtures/pacman-Si.err\" >&2; exit 1\n\
             \x20 fi ;;\n\
             esac\n",
        ),
    ];
    for (name, body) in bodies {
        let script = format!(
            "#!/bin/sh\n\
             # A test-owned shim of tests/host_import_core.rs. It logs its argv and replays a\n\
             # hand-authored recording. No package manager runs on this machine.\n\
             # The product starts it with a cleared environment and a fixed PATH (LD-357), so the\n\
             # PATH its own tools are found on is baked in when the test writes it.\n\
             PATH='{path}'; export PATH\n\
             state='{state}'\n\
             fixtures=$(cat \"$state/fixtures\")\n\
             {{ printf '%s' '{name}'\n\
             \x20 for a in \"$@\"; do printf '\\037%s' \"$a\"; done\n\
             \x20 printf '\\036'; }} >> \"$state/log\"\n\
             {body}",
            state = state.display(),
            path = std::env::var("PATH").unwrap_or_default(),
        );
        let path = bin.join(name);
        fs::write(&path, script).expect("a shim");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("a shim mode");
    }
}

/// One scratch root behind the shims, with one scenario's recordings selected.
struct Case {
    root: Root,
    fixtures: PathBuf,
    state: PathBuf,
    /// Held for the life of the case: the shims are shared, so cases do not overlap.
    _serial: MutexGuard<'static, ()>,
}

impl Case {
    fn new(distro: &str, scenario: &str) -> Case {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_, state) = shims();
        let fixtures = fixtures_root().join(distro).join(scenario);
        assert!(
            fixtures.is_dir(),
            "the recordings {} are not there",
            fixtures.display()
        );
        let root = Root::new(&format!("import-{distro}-{scenario}"));
        root.arm();
        root.write(
            "etc/os-release",
            &fs::read_to_string(fixtures_root().join(distro).join("os-release"))
                .expect("the os-release recording"),
        );
        // A machine that really has a name, so that acceptance row (g) is asserted against one.
        root.write("etc/hostname", &format!("{HOST_NAME}\n"));
        // A fresh index in both families' places, so that no plan built from this root has an
        // index action to take. Import never reads either one; the round-trip plan does.
        root.write("var/lib/apt/lists/recorded_Packages", "recorded index\n");
        root.write("var/lib/pacman/sync/core.db", "recorded database\n");
        for (list, dir, suffix) in [
            ("snaps.txt", import::SNAPS, ".snap"),
            ("flatpaks.txt", import::FLATPAKS, ""),
        ] {
            let text = fs::read_to_string(fixtures.join(list)).unwrap_or_default();
            for name in text.lines().filter(|line| !line.trim().is_empty()) {
                if suffix.is_empty() {
                    fs::create_dir_all(root.path(&format!("{dir}/{name}"))).expect("a flatpak");
                } else {
                    root.write(&format!("{dir}/{name}"), "");
                }
            }
        }
        // A scenario may carry a `root/` subtree: the files under /etc/apt and
        // /var/lib/apt/lists an import reads for the repositories a machine enabled.
        if fixtures.join("root").is_dir() {
            copy_tree(&fixtures.join("root"), &root.dir);
        }
        fs::write(state.join("fixtures"), fixtures.display().to_string()).expect("the selection");
        let _ = fs::remove_file(state.join("log"));
        Case {
            root,
            fixtures,
            state,
            _serial: serial,
        }
    }

    fn gate(&self) -> Gate {
        Gate::open(&self.root.options(), Operation::Plan).expect("the scratch root is armed")
    }

    fn machine(&self) -> import::Machine {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        import::read(&self.gate()).expect("the recordings are read")
    }

    fn emitted(&self) -> String {
        import::emit::manifest(&self.machine(), &snapshot(), None)
    }

    fn expected_path(&self) -> PathBuf {
        self.fixtures.join("expected.toml")
    }

    /// Every argv the shims saw, one line per invocation, in order.
    fn log(&self) -> Vec<String> {
        let bytes = fs::read(self.state.join("log")).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .split('\u{1e}')
            .filter(|record| !record.is_empty())
            .map(|record| {
                record
                    .split('\u{1f}')
                    .collect::<Vec<&str>>()
                    .join(" ")
                    .trim()
                    .to_string()
            })
            .collect()
    }
}

/// Copy a fixture subtree into the scratch root, files and directories, nothing else.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).expect("the target directory");
    for entry in fs::read_dir(from).expect("the fixture subtree").flatten() {
        let target = to.join(entry.file_name());
        if entry.path().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).expect("a fixture file");
        }
    }
}

fn snapshot() -> Snapshot {
    Snapshot::at(lodi::util::parse_utc(CAPTURED_AT).expect("a timestamp this test spells"))
}

// ------------------------------------------------------------------ acceptance (a) ---

/// (a) Over every recorded scenario the emitted manifest equals the committed expected file byte
/// for byte, and a second call equals the first.
#[test]
fn every_scenario_emits_the_committed_bytes_and_twice_the_same_bytes() {
    for (distro, scenario) in SCENARIOS {
        let case = Case::new(distro, scenario);
        let first = case.emitted();
        let second = case.emitted();
        assert_eq!(first, second, "{distro}/{scenario} is not deterministic");
        if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
            fs::write(case.expected_path(), recorded(&first)).expect("the expected file");
            continue;
        }
        let expected = fs::read_to_string(case.expected_path()).unwrap_or_else(|e| {
            panic!(
                "{}: {e}; record it with LODI_RECORD_EXPECTED=1 cargo test --test \
                 host_import_core",
                case.expected_path().display()
            )
        });
        assert_eq!(recorded(&first), expected, "{distro}/{scenario} moved");
    }
}

/// The emitted bytes as the committed file holds them. The lodi an imported manifest names as the
/// one it needs is the release that first honours its `snapshot`, `>=1.4.0`, whatever lodi ran
/// the import (LD-398), so the recording holds it literally and a version bump moves nothing.
fn recorded(emitted: &str) -> String {
    let needs = "min_lodi_version = \">=1.4.0\"";
    assert_eq!(emitted.matches(needs).count(), 1, "{emitted}");
    emitted.to_string()
}

/// The capture is the instant floored to the hour, and it is the caller's value rather than a
/// clock this module reads — which is what makes the byte-identical re-import above possible at
/// all (LD-282). The file carries the last UTC day that had ended by then: a dated apt index of
/// a more recent instant can still change after it is read (LD-444).
#[test]
fn the_snapshot_is_the_last_day_ended_at_the_capture() {
    assert_eq!(snapshot().as_str(), FLOORED);
    let case = Case::new("debian-12", "fresh");
    let line = format!("snapshot = \"{ENDED_DAY}\"");
    assert!(
        case.emitted().contains(&line),
        "the emitted file carries the last ended day"
    );
    // A different instant in the same day gives the same bytes; the next day does not.
    let machine = case.machine();
    let same_day = Snapshot::at(lodi::util::parse_utc("2026-09-22T23:59:59Z").expect("a time"));
    let next_day = Snapshot::at(lodi::util::parse_utc("2026-09-23T00:00:00Z").expect("a time"));
    assert_eq!(
        import::emit::manifest(&machine, &same_day, None),
        import::emit::manifest(&machine, &snapshot(), None)
    );
    assert_ne!(
        import::emit::manifest(&machine, &next_day, None),
        import::emit::manifest(&machine, &snapshot(), None)
    );
}

/// A 1.3 binary reads `[host] snapshot` and ignores it, so a file that carries a live snapshot
/// names a lodi that reads it: every 1.3 release refuses the file, and 1.4.0 on applies it
/// (M-Pin, LD-398).
#[test]
fn an_imported_file_with_a_live_snapshot_is_refused_by_a_1_3_lodi() {
    use lodi::version::{Constraint, Version};
    let v = |text: &str| Version::parse(text).expect("a version this test spells");
    for (distro, scenario) in SCENARIOS {
        let emitted = Case::new(distro, scenario).emitted();
        assert!(
            emitted.contains("\nsnapshot = \""),
            "{distro}/{scenario}: {emitted}"
        );
        let needs = emitted
            .lines()
            .find_map(|line| line.strip_prefix("min_lodi_version = \""))
            .and_then(|rest| rest.strip_suffix('"'))
            .unwrap_or_else(|| panic!("{distro}/{scenario}: no min_lodi_version"));
        let needs = Constraint::parse(needs).expect("the constraint the import writes");
        for older in ["1.1.1", "1.2.0", "1.3.0", "1.3.9"] {
            assert!(
                !needs.matches(&v(older)),
                "{distro}/{scenario}: {older} accepts it"
            );
        }
        for newer in ["1.4.0", "1.4.1", "1.5.0"] {
            assert!(
                needs.matches(&v(newer)),
                "{distro}/{scenario}: {newer} refuses it"
            );
        }
    }
}

/// The one manifest key added after 1.4.0 is the opt-in `[packages.arch] archived_keyring`,
/// which a 1.4.0 binary refuses. The import never writes it, so the file it writes is one 1.4.0
/// reads, and `IMPORT_MINIMUM` stays 1.4.0 (LD-451, LD-454).
#[test]
fn an_imported_file_never_writes_the_archived_keyring_a_1_4_0_lodi_refuses() {
    use lodi::hostscope::import::emit::IMPORT_MINIMUM;
    assert_eq!(IMPORT_MINIMUM, "1.4.0");
    for (distro, scenario) in SCENARIOS {
        let emitted = Case::new(distro, scenario).emitted();
        assert!(
            !emitted.contains("archived_keyring"),
            "{distro}/{scenario}: {emitted}"
        );
    }
}

#[test]
fn arch_import_uses_previous_utc_day() {
    let case = Case::new("arch", "fresh");
    let rendered = import::emit::manifest(&case.machine(), &snapshot(), None);
    assert!(
        rendered.contains("snapshot = \"2026-09-21T00:00:00Z\""),
        "Arch imports the previous UTC day: {rendered}"
    );
}

// ------------------------------------------------------------------ acceptance (b) ---

/// (b) A fresh-install fixture declares nothing beyond the distribution's own metapackages — on
/// apt the `metapackages` sections and the kernel metapackages, on pacman `base` together with
/// the kernel and firmware `base` deliberately does not carry (this task's own behaviour text).
#[test]
fn a_fresh_install_declares_only_the_distributions_own_metapackages() {
    for (distro, scenario) in SCENARIOS.iter().filter(|(_, s)| *s == "fresh") {
        let case = Case::new(distro, scenario);
        let machine = case.machine();
        let selection = baseline::select(&machine);
        assert!(
            !selection.common.is_empty(),
            "{distro}: a fresh install still declares its own base"
        );
        for name in &selection.common {
            assert!(
                machine.survey.metapackages.contains(name) || machine.survey.keep.contains(name),
                "{distro}: `{name}` is neither a metapackage of this distribution nor its kernel"
            );
        }
        assert!(selection.not_captured() == 0, "{selection:?}");
    }
}

// ------------------------------------------------------- acceptance (c) and (d) ---

/// (c) A name whose installed version is known to the local package database and to no
/// repository is in the comment block and never in `[packages]`.
///
/// Before R-2 this was "a name the enabled repositories offer no candidate for". On apt that
/// test was true of nothing installed, because `apt-cache policy` counts `/var/lib/dpkg/status`
/// as a source; the recordings below give such a name no policy block at all, which is the one
/// shape the old test could still catch and is exactly what a name the index has never heard of
/// looks like. The `off-base` scenarios are where the classes a real machine has are proved.
#[test]
fn a_name_known_only_to_the_package_database_is_commented_and_never_declared() {
    for (distro, name) in [
        ("debian-12", "lodi-hand-built"),
        ("ubuntu-24.04", "ubuntu-hand-built"),
        ("ubuntu-26.04", "ubuntu-hand-built"),
    ] {
        let case = Case::new(distro, "chosen");
        let machine = case.machine();
        assert!(
            machine.observed.installed.contains_key(name)
                && !machine.observed.auto.contains(name)
                && machine.origins.get(name) == Some(&pm::Origin::LocalOnly),
            "{distro}: `{name}` is installed, chosen, and no repository offers its version"
        );
        let selection = baseline::select(&machine);
        assert_eq!(selection.local_only, vec![name.to_string()]);
        assert!(!selection.common.contains(&name.to_string()));
        let emitted = case.emitted();
        let (declarations, comment) = split_at_not_captured(&emitted);
        assert!(
            !declarations.contains(name),
            "{distro}: `{name}` reached the declarations"
        );
        assert!(
            comment.contains(&format!("\n#   {name}\n")),
            "{distro}: `{name}` is not named in the comment block"
        );
    }
}

/// (d) A foreign name — an AUR build, which pacman itself reports — is in the comment block only.
#[test]
fn a_foreign_name_is_in_the_comment_block_only() {
    let case = Case::new("arch", "chosen");
    let machine = case.machine();
    assert!(machine.survey.foreign.contains("yay"), "pacman said so");
    let selection = baseline::select(&machine);
    assert_eq!(selection.foreign, vec!["yay".to_string()]);
    assert!(!selection.common.contains(&"yay".to_string()));
    let emitted = case.emitted();
    let (declarations, comment) = split_at_not_captured(&emitted);
    assert!(!declarations.contains("yay"));
    assert!(comment.contains("\n#   yay\n"));
    assert!(comment.contains("\n# From no repository this package manager knows:\n"));
}

/// A machine with **no** foreign package is not a failure, which is what every stock Arch guest
/// is (M-Import T-5, LD-295).
///
/// `pacman -Qqem` exits 1 when nothing matches, and until T-5 read a real guest that status was
/// `E_APPLY` at exit 8 — so an import stopped on every fresh Arch machine before it
/// emitted a byte. The shim above answers the way pacman does whenever a scenario's recording is
/// empty, so the `fresh` recording reproduces that machine offline.
#[test]
fn a_machine_with_no_foreign_package_is_not_a_failure() {
    let case = Case::new("arch", "fresh");
    assert!(
        fs::read(case.fixtures.join("pacman-Qqem.txt"))
            .expect("the recording")
            .is_empty(),
        "this case is about the empty answer, which the shim gives as exit 1",
    );
    let machine = case.machine();
    assert!(
        machine.survey.foreign.is_empty(),
        "no foreign name was read"
    );
    let emitted = case.emitted();
    let (_, comment) = split_at_not_captured(&emitted);
    assert!(comment.contains("\n# From no repository this package manager knows: none\n"));
}

// ------------------------------------------------------- R-2, acceptance (1) and (2) ---

/// (1) On apt, what is declared is decided by the **origin of the installed version**, and the
/// four classes a real machine has are each in the right place.
///
/// The `off-base` recording is one machine with one name of each kind: `lodi-hand-built`, whose
/// installed version has no source but `/var/lib/dpkg/status`; `docker-ce` from `o=Docker`;
/// `lodi-agent` from a PPA (`o=LP-PPA-…`); `golang-1.22` from `o=Debian a=bookworm-backports`;
/// and `ripgrep` from the release's own suite. The first three are named and never declared, the
/// last two are declared, and the backports one says beside itself that a fresh machine gets
/// another version.
#[test]
fn an_apt_machine_declares_only_what_the_distributions_own_repositories_serve() {
    let case = Case::new("debian-12", "off-base");
    let machine = case.machine();
    assert_eq!(
        machine.origins.get("lodi-hand-built"),
        Some(&pm::Origin::LocalOnly),
        "its only source line is the dpkg status file"
    );
    assert_eq!(
        machine.origins.get("golang-1.22"),
        Some(&pm::Origin::Suite("bookworm-backports".to_string()))
    );
    assert_eq!(machine.origins.get("ripgrep"), Some(&pm::Origin::Base));

    let selection = baseline::select(&machine);
    assert_eq!(
        selection.common,
        vec!["golang-1.22".to_string(), "ripgrep".to_string()],
        "a third-party, a PPA and a hand-installed name are not declared"
    );
    assert_eq!(
        selection.third_party,
        vec![
            (
                "docker-ce".to_string(),
                "Docker, bookworm/stable".to_string()
            ),
            (
                "lodi-agent".to_string(),
                "LP-PPA-lodi-testing, noble/main".to_string()
            ),
        ],
        "sorted, each by its source's own label"
    );
    assert_eq!(selection.local_only, vec!["lodi-hand-built".to_string()]);
    assert_eq!(selection.not_captured(), 3);

    let emitted = case.emitted();
    let (declarations, comment) = split_at_not_captured(&emitted);
    for name in ["docker-ce", "lodi-agent", "lodi-hand-built"] {
        assert!(
            !declarations.contains(name),
            "`{name}` reached the declarations"
        );
        assert!(comment.contains(&format!("\n#   {name}")), "{comment}");
    }
    assert!(
        declarations.contains("#   golang-1.22             bookworm-backports\n"),
        "the backports name carries its class comment: {declarations}"
    );

    // The counts line at the top of the block is the number of names under the headings.
    assert!(
        comment.contains(
            "\n# 3 package(s) installed and chosen on this machine are NOT declared above,\n"
        ),
        "{comment}"
    );
    // The package classes end where the repositories part of the block begins (LD-367).
    let headings = comment
        .split_once("the host scope manages neither.\n#\n")
        .expect("the counts come before the headings")
        .1
        .split("\n#\n")
        .next()
        .expect("the classes");
    assert_eq!(
        headings
            .lines()
            .filter(|line| line.starts_with("#   "))
            .count(),
        3,
        "{headings}"
    );

    // AGENTS.md §1.2: a third-party source is free to put a host name in `Origin:`, and the
    // emitted file carries labels only. The recording's sources are all addressed.
    assert!(
        fs::read_to_string(case.fixtures.join("policy-release.txt"))
            .expect("the recording")
            .contains("http"),
        "this case is about a machine whose sources have addresses"
    );
    assert!(!emitted.contains("http"), "{emitted}");
}

/// (2) On pacman, the repository that offers the installed name decides, and a name two
/// repositories offer is declared because a fresh machine can still install it from the one
/// that is the distribution's own.
#[test]
fn a_pacman_machine_declares_what_an_enabled_base_repository_offers() {
    let case = Case::new("arch", "off-base");
    let machine = case.machine();
    assert_eq!(
        machine.origins.get("tela-icon-theme"),
        Some(&pm::Origin::ThirdParty("chaotic-aur".to_string()))
    );
    assert_eq!(machine.origins.get("ripgrep"), Some(&pm::Origin::Base));
    assert_eq!(
        machine.origins.get("lodi-tool"),
        Some(&pm::Origin::Base),
        "`chaotic-aur` and `extra` both offer it, and `extra` is enough"
    );

    let selection = baseline::select(&machine);
    assert!(
        selection.common.contains(&"lodi-tool".to_string())
            && selection.common.contains(&"ripgrep".to_string()),
        "{selection:?}"
    );
    assert_eq!(
        selection.third_party,
        vec![("tela-icon-theme".to_string(), "chaotic-aur".to_string())]
    );
    assert_eq!(selection.foreign, vec!["yay".to_string()]);
    assert_eq!(selection.not_captured(), 2);

    let emitted = case.emitted();
    let (declarations, comment) = split_at_not_captured(&emitted);
    assert!(!declarations.contains("tela-icon-theme") && !declarations.contains("yay"));
    assert!(
        comment.contains("\n#   tela-icon-theme         chaotic-aur\n"),
        "{comment}"
    );
    assert!(comment.contains("\n#   yay\n"), "{comment}");
}

/// The emitted file up to `NOT CAPTURED`, and the block from it on.
fn split_at_not_captured(emitted: &str) -> (&str, &str) {
    emitted
        .split_once("# NOT CAPTURED")
        .expect("every emitted file ends with the comment block")
}

// ------------------------------------------------ LD-305, fixed by LD-312 ---

/// The one Multi-Arch name of the `multiarch` scenario that a person chose by hand and holds.
const HELD_MULTI_ARCH: &str = "libfuse2";

/// The installed set is read by `${Package}`, so an architecture-qualified name never exists.
///
/// This is the defect LD-305 recorded and this package fixed. `dpkg-query -W
/// -f=${binary:Package}` prints `libc6:amd64` for every Multi-Arch package while `apt-mark
/// showauto` prints `libc6`, so on a real Debian 12 guest 143 of 333 installed names matched
/// nothing in the automatic set, survived the subtraction, found no candidate under a qualified
/// name and were listed in the emitted `NOT CAPTURED` block as names the repositories offer
/// nothing for. The `multiarch` recording is that shape: it holds **both** answers dpkg-query
/// gives, and the shim replays the one the product's own format string asks for — so this case
/// fails against the old format and passes against the new one.
#[test]
fn multi_arch_libraries_are_read_by_plain_name_and_reach_neither_block() {
    let case = Case::new("debian-12", "multiarch");

    // The recording is the evidence's shape, read from the recording itself rather than listed
    // here: the names `${binary:Package}` qualifies, and the plain automatic set beside them.
    let qualified: Vec<String> = fs::read_to_string(case.fixtures.join("dpkg-query-binary.txt"))
        .expect("the ${binary:Package} recording")
        .lines()
        .filter_map(|line| line.split('\t').next())
        .filter(|name| name.contains(':'))
        .map(ToString::to_string)
        .collect();
    assert!(
        qualified.len() >= 10,
        "the fixture reproduces the evidence only with ten or more qualified names, not {}",
        qualified.len()
    );
    let automatic: BTreeSet<String> =
        fs::read_to_string(case.fixtures.join("apt-mark-showauto.txt"))
            .expect("the automatic set")
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(ToString::to_string)
            .collect();

    let machine = case.machine();
    let selection = baseline::select(&machine);
    let emitted = case.emitted();
    let (declarations, comment) = split_at_not_captured(&emitted);

    for name in &qualified {
        let plain = name.split(':').next().expect("a name before the qualifier");
        // Read by `${Package}`: the qualified spelling is nowhere, and the plain one is
        // everything — the installed set and the survey the baseline subtracts by alike.
        assert!(
            !machine.observed.installed.contains_key(name),
            "`{name}` survived as a qualified name"
        );
        assert!(
            machine.observed.installed.contains_key(plain),
            "`{plain}` is not in the installed set"
        );
        assert!(
            machine.survey.priority.contains_key(plain),
            "the survey is read under a different identity than the installed set: `{plain}`"
        );
        assert!(
            !emitted.contains(name),
            "`{name}` reached the emitted file in its qualified spelling"
        );
        if plain == HELD_MULTI_ARCH {
            // The one a person chose. Its own case is below.
            assert!(!automatic.contains(plain));
            continue;
        }
        assert!(
            automatic.contains(plain),
            "the fixture reproduces the evidence only when `{plain}` is plain in the automatic \
             set"
        );
        let plain = plain.to_string();
        assert!(
            !selection.common.contains(&plain)
                && !selection.local_only.contains(&plain)
                && !selection.third_party.iter().any(|(n, _)| n == &plain)
                && !selection.foreign.contains(&plain),
            "`{plain}` is automatic and should have left the candidate set"
        );
        assert!(
            !declarations.contains(&plain),
            "`{plain}` reached the declarations"
        );
        assert!(
            !comment.contains(&format!("\n#   {plain}\n")),
            "`{plain}` is named in the NOT CAPTURED block"
        );
    }
    // Nothing at all is left out of this machine's declarations: the block that used to name a
    // hundred libraries names none.
    assert!(selection.not_captured() == 0, "{selection:?}");
}

/// A package a person really chose still lands, and a held Multi-Arch one is still held.
///
/// `apt-mark showhold` prints plain names too, so before the fix a held Multi-Arch package was
/// held under a name the installed set never carried. It is now the same name on both sides.
#[test]
fn a_chosen_package_lands_and_a_held_multi_arch_one_is_still_held() {
    let case = Case::new("debian-12", "multiarch");
    let machine = case.machine();
    assert!(
        machine.observed.held.contains(HELD_MULTI_ARCH),
        "`apt-mark showhold` named it"
    );
    let selection = baseline::select(&machine);
    assert_eq!(
        selection.common,
        [HELD_MULTI_ARCH, "linux-image-amd64", "ripgrep"],
        "the machine's own choices, and only those"
    );
    assert_eq!(selection.hold, [HELD_MULTI_ARCH]);

    let emitted = case.emitted();
    let (declarations, _) = split_at_not_captured(&emitted);
    for name in ["ripgrep", HELD_MULTI_ARCH] {
        assert!(
            declarations.contains(&format!("\n  \"{name}\",\n")),
            "`{name}` is not declared"
        );
    }
}

// ------------------------------------------------------------------ acceptance (e) ---

/// (e) The emitted text parses with `hostscope::manifest::parse` and, planned against the very
/// machine it was read from, produces **no package action at all**.
#[test]
fn the_emitted_text_round_trips_with_no_package_action() {
    for (distro, scenario) in SCENARIOS {
        let case = Case::new(distro, scenario);
        let emitted = case.emitted();
        let gate = case.gate();
        let ctx = manifest::Context::from_gate(&gate);
        let parsed = manifest::parse(&emitted, "host.toml", &ctx).unwrap_or_else(|e| {
            panic!("{distro}/{scenario}: the emitted file does not parse: {e}")
        });
        assert!(
            !parsed.packages.is_empty(),
            "{distro}/{scenario}: a manifest with no packages would prove nothing"
        );
        assert_eq!(parsed.host.distro.as_deref(), Some(gate.distro.name()));
        assert_eq!(parsed.host.snapshot.as_deref(), Some(ENDED_DAY));
        let built = {
            let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
            plan::build(
                &gate,
                &parsed,
                &std::collections::BTreeMap::new(),
                lodi::hostscope::lock::IN_PLACE_SOURCE,
                None,
                "sha256:0000000000000000000000000000000000000000000000000000000000000000",
                false,
            )
            .unwrap_or_else(|e| panic!("{distro}/{scenario}: the plan failed: {e}"))
        };
        let package_lines: Vec<String> = built
            .actions
            .iter()
            .filter(|action| matches!(action.kind, plan::Kind::Package(_)))
            .flat_map(|action| action.lines())
            .collect();
        assert!(
            package_lines.is_empty(),
            "{distro}/{scenario}: the plan still wants {package_lines:?}"
        );
        // No machine change. Given no record, the plan also says the apply would write the one
        // the import itself writes where it lands in place (LD-375).
        assert!(built.is_noop(), "{distro}/{scenario}: {}", built.render());
        assert!(
            built
                .render()
                .ends_with("no machine changes; apply would update the record\n"),
            "{distro}/{scenario}: {}",
            built.render()
        );
    }
}

// ------------------------------------------------------------------ acceptance (f) ---

/// (f) Nothing this module builds mutates anything: the shim argv log holds only the read
/// commands, and no word of any mutation appears in it.
#[test]
fn nothing_this_module_builds_mutates_a_machine() {
    /// What a read is, per family, as `program <first argument>`. Anything else in the log is a
    /// failure whatever it is.
    const READS: &[&str] = &[
        "apt-cache policy",
        "apt-mark showauto",
        "apt-mark showhold",
        "dpkg-query -W",
        "pacman -Q",
        "pacman -Qe",
        "pacman -Qi",
        "pacman -Qqem",
        "pacman -Si",
        "pacman-conf --repo-list",
        "pacman-conf --sysroot",
    ];
    /// Words that only a mutation carries. `-Si` is a read whose name begins like `-S`, which is
    /// why the check above is the exact pair and this one is the whole word.
    const MUTATIONS: &[&str] = &[
        "--asdeps",
        "--asexplicit",
        "--noconfirm",
        "-Rns",
        "-S",
        "-Syu",
        "-U",
        "auto",
        "hold",
        "install",
        "manual",
        "remove",
        "unhold",
        "update",
        "upgrade",
    ];
    for (distro, scenario) in SCENARIOS {
        let case = Case::new(distro, scenario);
        let _ = case.machine();
        let log = case.log();
        assert!(
            !log.is_empty(),
            "{distro}/{scenario}: the shims saw nothing"
        );
        for line in &log {
            let mut words = line.split(' ');
            let pair = format!(
                "{} {}",
                words.next().unwrap_or_default(),
                words.next().unwrap_or_default()
            );
            assert!(
                READS.contains(&pair.as_str()),
                "{distro}/{scenario}: `{line}` is not one of this module's reads"
            );
            for word in line.split(' ') {
                assert!(
                    !MUTATIONS.contains(&word),
                    "{distro}/{scenario}: `{line}` carries `{word}`"
                );
            }
        }
        // The two programs that can only change a machine were never reached at all.
        for program in ["apt-get", "dpkg "] {
            assert!(
                !log.iter().any(|line| line.starts_with(program)),
                "{distro}/{scenario}: {program} was run"
            );
        }
        // And the module's own root was not written into: the census before and after is equal.
        let before = census(&case.root.dir);
        let _ = case.machine();
        assert_eq!(before, census(&case.root.dir), "{distro}/{scenario}");
    }
}

/// Every path below a directory with its mode and size, so that "wrote nothing" is a comparison
/// rather than a hope.
fn census(dir: &Path) -> Vec<String> {
    fn visit(dir: &Path, base: &Path, out: &mut Vec<String>) {
        let Ok(entries) = fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            out.push(format!(
                "{} {:o} {}",
                path.strip_prefix(base).unwrap_or(&path).display(),
                meta.permissions().mode(),
                meta.len()
            ));
            if meta.is_dir() {
                visit(&path, base, out);
            }
        }
    }
    let mut out = Vec::new();
    visit(dir, dir, &mut out);
    out.sort();
    out
}

// ------------------------------------------------------------------ acceptance (g) ---

/// (g) The emitted bytes carry no address, no home path, no host name and no version string.
///
/// A *version string* is a token that starts with a digit and carries a dot: `1.07.1-3+b1` is
/// one, and the package name `python3.11` is not, because a name is not a version and refusing it
/// would be refusing the machine's own vocabulary. The versions the fixture really holds are
/// searched for by their own text as well, so the rule is proved twice.
#[test]
fn the_emitted_bytes_carry_no_address_home_path_host_name_or_version() {
    for (distro, scenario) in SCENARIOS {
        let case = Case::new(distro, scenario);
        let emitted = case.emitted();
        let where_ = format!("{distro}/{scenario}");
        assert!(!emitted.contains('@'), "{where_}: an address-shaped byte");
        assert!(!emitted.contains("/home/"), "{where_}: a home path");
        // The host name is a typed setting since si-1 (LD-421): it is written once, as the
        // `[system]` hostname, and nowhere else.
        let hostname = format!("[system]\nhostname = \"{HOST_NAME}\"\n");
        assert_eq!(emitted.matches(&hostname).count(), 1, "{where_}: {emitted}");
        let emitted = emitted.replace(&hostname, "[system]\n");
        assert!(!emitted.contains(HOST_NAME), "{where_}: the host name");
        // One version is written, on purpose, and only one: the lodi that honours the snapshot,
        // 1.4.0 (LD-398). Everything else stays true of the rest of the file.
        let needs = "min_lodi_version = \">=1.4.0\"\n";
        assert_eq!(emitted.matches(needs).count(), 1, "{where_}: {emitted}");
        let emitted = emitted.replace(needs, "");
        // And one more, commented, in each [sources] block: the lodi that reads the table
        // (LD-367). A file with no block carries none of it.
        let sources_need = "# #   min_lodi_version = \">=1.3.0\"\n";
        assert_eq!(
            emitted.matches(sources_need).count(),
            emitted.matches("\n# [sources.").count(),
            "{where_}: {emitted}"
        );
        let emitted = emitted.replace(sources_need, "");
        assert!(
            !emitted.contains(env!("CARGO_PKG_VERSION")),
            "{where_}: the lodi version"
        );
        for token in tokens(&emitted) {
            assert!(
                !(token.starts_with(|c: char| c.is_ascii_digit()) && token.contains('.')),
                "{where_}: `{token}` is a version string"
            );
        }
        let machine = case.machine();
        let versions: BTreeSet<&String> = machine.observed.installed.values().collect();
        for version in versions {
            assert!(
                !emitted.contains(version.as_str()),
                "{where_}: the version `{version}` reached the emitted bytes"
            );
        }
    }
}

/// The maximal runs of characters a package name, a version or a release may be spelled with.
/// Everything else — quotes, spaces, brackets, colons — separates one token from the next.
fn tokens(text: &str) -> Vec<String> {
    text.split(|c: char| !(c.is_ascii_alphanumeric() || "+-._".contains(c)))
        .filter(|token| !token.is_empty())
        .map(ToString::to_string)
        .collect()
}

// ------------------------------------------------------ the seam, and what it cannot do ---

/// The candidate set is a projection of readings the product already takes: installed minus
/// automatically installed, and nothing else.
#[test]
fn the_candidate_set_is_installed_minus_auto() {
    let case = Case::new("debian-12", "chosen");
    let machine = case.machine();
    let chosen: BTreeSet<String> = baseline::chosen(&machine.observed).into_iter().collect();
    let expected: BTreeSet<String> = machine
        .observed
        .installed
        .keys()
        .filter(|name| !machine.observed.auto.contains(*name))
        .cloned()
        .collect();
    assert_eq!(chosen, expected);
    assert!(chosen.contains("curl") && !chosen.contains("libcurl4"));
}

/// A metapackage is kept and the closure it reaches is dropped, so declaring the metapackage
/// declares all of it and the emitted file stays short.
#[test]
fn a_metapackage_is_kept_and_its_closure_is_dropped() {
    let case = Case::new("debian-12", "fresh");
    let machine = case.machine();
    let selection = baseline::select(&machine);
    assert!(selection.common.contains(&"task-ssh-server".to_string()));
    assert!(
        !machine.observed.auto.contains("openssh-server"),
        "the closure member was chosen too, which is what makes this a real subtraction"
    );
    assert!(!selection.common.contains(&"openssh-server".to_string()));
    // And the priority rule, on a name no metapackage reaches.
    assert_eq!(
        machine.survey.priority.get("bash").copied(),
        Some(pm::Priority::Required)
    );
    assert!(!selection.common.contains(&"bash".to_string()));
}

/// A hold is read off the machine where the family keeps one, and where it does not the emitted
/// file says why rather than omitting the section silently.
#[test]
fn a_hold_is_read_where_the_family_keeps_one_and_explained_where_it_does_not() {
    let apt = Case::new("debian-12", "chosen");
    let emitted = apt.emitted();
    assert!(emitted.contains("hold = [\n  \"ripgrep\",\n]"), "{emitted}");
    drop(apt);

    let pacman = Case::new("arch", "chosen");
    let emitted = pacman.emitted();
    assert!(
        emitted.contains("# No hold was read off this machine.")
            && emitted.contains("keeps no hold of its own"),
        "{emitted}"
    );
    assert!(!emitted.contains("\nhold = "), "{emitted}");
}

/// The counts of the two packaging systems Lodi does not manage are in the comment block, and
/// they are read off the root rather than by running `snap` or `flatpak`.
#[test]
fn the_comment_block_counts_the_snaps_and_the_flatpaks() {
    let ubuntu = Case::new("ubuntu-24.04", "chosen");
    let machine = ubuntu.machine();
    assert_eq!((machine.snaps, machine.flatpaks), (2, 0));
    assert!(ubuntu.emitted().contains(
        "# 2 snap(s) and 0 system flatpak(s) are installed; the host scope manages neither."
    ));
    drop(ubuntu);

    let arch = Case::new("arch", "chosen");
    let machine = arch.machine();
    assert_eq!((machine.snaps, machine.flatpaks), (0, 2));
    assert!(arch.emitted().contains(
        "# 0 snap(s) and 2 system flatpak(s) are installed; the host scope manages neither."
    ));
}

/// A machine with **no package index at all** — nothing fetched from any repository, which is
/// what a cloud image is until `apt-get update` has run on it — is not a machine that chose
/// nothing. Every source line `apt-cache policy` prints for it is `/var/lib/dpkg/status`, so
/// the origin of no installed version can be checked; declaring only what a base repository
/// serves would declare nothing at all, and emit a file that restores an empty machine.
///
/// So everything chosen is declared as read, nothing is left out, and the import says so
/// twice: above the list in the emitted file, and once on standard error.
#[test]
fn a_machine_with_no_package_index_declares_everything_and_says_so() {
    let case = Case::new("debian-12", "off-index");
    let machine = case.machine();
    assert!(
        machine
            .origins
            .values()
            .all(|origin| *origin == pm::Origin::Unchecked),
        "no source but the package database is no answer about any of them: {:?}",
        machine.origins
    );

    let selection = baseline::select(&machine);
    assert!(selection.unchecked, "the machine, not a name, is unchecked");
    assert_eq!(selection.not_captured(), 0, "nothing is left out");
    for name in ["docker-ce", "lodi-agent", "lodi-hand-built"] {
        assert!(
            selection.common.contains(&name.to_string()),
            "`{name}` is declared as read: {:?}",
            selection.common
        );
    }

    let warnings = selection.warnings(machine.distro);
    assert_eq!(warnings.len(), 1, "once about the machine: {warnings:?}");
    assert!(
        warnings[0].starts_with("W_UNCHECKED_ORIGIN: this machine has no package index,"),
        "{}",
        warnings[0]
    );
    assert!(
        !warnings[0].contains("W_UNCAPTURED"),
        "nothing was left out, so nothing is uncaptured: {}",
        warnings[0]
    );

    let emitted = case.emitted();
    assert!(
        emitted.contains("# This machine has no package index: nothing has been fetched from any"),
        "{emitted}"
    );
    assert!(
        emitted.contains("# 0 package(s) installed and chosen on this machine are NOT declared"),
        "{emitted}"
    );
}

/// The same machine after `apt-get update`: the very recordings above, with the sources the
/// index brings back, classify every one of those names — and three of them stop being
/// declared. The pair is the whole point of `Origin::Unchecked`: it is "not asked", never
/// "asked and told nothing".
#[test]
fn the_same_names_are_classified_once_the_index_is_there() {
    let indexed = Case::new("debian-12", "off-base");
    let selection = baseline::select(&indexed.machine());
    assert!(!selection.unchecked);
    assert_eq!(selection.not_captured(), 3);
}

// ------------------------------------------------------------- repositories ([sources]) ---

use import::sources::{Reason, Unattributed};
use lodi::hostscope::sources::Format;

/// Strip the `# ` from every line of each commented `[sources.NAME]` block, the way a person
/// takes one, and leave every other line alone. A `# # ` line stays a comment.
fn take_blocks(emitted: &str) -> String {
    let mut out = String::new();
    let mut inside = false;
    for line in emitted.lines() {
        if line.starts_with("# [sources.") {
            inside = true;
        } else if line.is_empty() {
            inside = false;
        }
        out.push_str(if inside {
            line.strip_prefix("# ").unwrap_or(line)
        } else {
            line
        });
        out.push('\n');
    }
    out
}

/// The `sources` scenario: a Debian machine with Docker's repository enabled in deb822 form and
/// installed from, an enabled repository nothing came from, a plain-http `.list`, a vendor's
/// `[trusted=yes]` line beside an unsigned one, a credential file, and the distribution's own
/// three stanzas. The one repository carried is Docker's, as a commented `[sources.docker]`
/// block naming the packages attributed to it by `apt-cache policy`; its armored keyring stays
/// `.asc` (S4); everything else is named under NOT CAPTURED with its reason, by path and never
/// by URI.
#[test]
fn a_repository_installed_from_is_emitted_as_a_commented_sources_block() {
    let case = Case::new("debian-12", "sources");
    let machine = case.machine();
    let repos = &machine.repositories;
    assert!(repos.read && repos.indexed);
    assert_eq!(repos.distribution, 3, "{repos:?}");
    assert_eq!(repos.carried.len(), 1, "{repos:?}");
    let docker = &repos.carried[0];
    assert_eq!(docker.name, "docker");
    assert_eq!(docker.path, "/etc/apt/sources.list.d/docker.sources");
    assert_eq!(
        docker.packages,
        vec!["docker-ce", "docker-ce-cli"],
        "the chosen names whose installed version it offers"
    );
    assert_eq!(docker.format, Format::Armored);
    assert_eq!(docker.source(), "files/etc/apt/keyrings/lodi-docker.asc");
    assert_eq!(
        docker.keyring,
        fs::read(case.fixtures.join("root/etc/apt/keyrings/docker.asc")).unwrap()
    );
    assert_eq!(docker.sha256, lodi::util::sha256_hex(&docker.keyring));
    assert_eq!(repos.credentials, vec!["/etc/apt/auth.conf.d/vendor.conf"]);
    let refused: Vec<(&str, &Reason)> = repos
        .refused
        .iter()
        .map(|r| (r.path.as_str(), &r.reason))
        .collect();
    assert_eq!(
        refused,
        vec![
            (
                "/etc/apt/sources.list.d/idle.sources",
                &Reason::NothingInstalled
            ),
            ("/etc/apt/sources.list.d/plain.list", &Reason::NotHttps),
            ("/etc/apt/sources.list.d/vendor.list", &Reason::Trusted),
            ("/etc/apt/sources.list.d/vendor.list", &Reason::NoSignedBy),
        ],
        "{refused:?}"
    );

    let emitted = case.emitted();
    let block = format!(
        "# [sources.docker]\n\
         # # read from /etc/apt/sources.list.d/docker.sources\n\
         # # on this machine that file already lists it: disable that stanza before you\n\
         # # take this block here, because apt refuses two Signed-By values for one repository\n\
         # # installed from it, and not declared under [packages] until you take it:\n\
         # #   docker-ce\n# #   docker-ce-cli\n\
         # # a lodi that reads [sources]; when you take this block, [host] needs\n\
         # #   min_lodi_version = \">=1.3.0\"\n\
         # # the keyring was not written (--stdout): copy /etc/apt/keyrings/docker.asc\n\
         # # to files/etc/apt/keyrings/lodi-docker.asc beside this file\n\
         # types = [\"deb\"]\n\
         # uris = [\"https://download.docker.com/linux/debian\"]\n\
         # suites = [\"${{host.codename}}\"]\n\
         # components = [\"stable\"]\n\
         # signed_by = \"files/etc/apt/keyrings/lodi-docker.asc\"\n\
         # signed_by_sha256 = \"{}\"\n\n",
        docker.sha256
    );
    assert!(emitted.contains(&block), "{emitted}");
    // Commented out: the parsed manifest has no source, and the block sits after [packages].
    let ctx = manifest::Context::from_gate(&case.gate());
    let parsed = manifest::parse(&emitted, "host.toml", &ctx).expect("parses");
    assert!(parsed.sources.is_empty());
    // The packages that came from it are off-base (R-2): not declared, named under their class.
    assert!(!parsed.packages.common.contains(&"docker-ce".to_string()));
    assert!(
        emitted.contains("#   docker-ce               Docker, bookworm/stable\n"),
        "{emitted}"
    );
    assert!(emitted.find("[packages]").unwrap() < emitted.find("# [sources.docker]").unwrap());
    // The note that says to add the source first points at the block.
    assert!(
        emitted.contains("— take its commented [sources] block\n# above, where one is written"),
        "{emitted}"
    );
    // The refusals are named, by path; no refused URI reaches the bytes.
    for line in [
        "#   /etc/apt/sources.list.d/plain.list\n#     its URI is not https://",
        "#   /etc/apt/sources.list.d/vendor.list\n#     it is marked trusted",
        "#     it names no Signed-By keyring",
        "#   /etc/apt/sources.list.d/idle.sources\n#     no installed package came from it",
        "# the distribution's own repositories, which every install has: 3",
        "# credential files apt keeps, which lodi never reads or carries:\n#   /etc/apt/auth.conf.d/vendor.conf",
    ] {
        assert!(emitted.contains(line), "missing {line:?} in:\n{emitted}");
    }
    for refused_uri in [
        "vendor.example.invalid",
        "mirror.example.invalid",
        "packages.example.invalid",
    ] {
        assert!(!emitted.contains(refused_uri), "{refused_uri}: {emitted}");
    }

    // Uncommented, the block is a manifest an apply could take.
    let taken = take_blocks(&emitted);
    let parsed = manifest::parse(&taken, "host.toml", &ctx)
        .unwrap_or_else(|e| panic!("the uncommented block does not parse: {e}\n{taken}"));
    assert_eq!(parsed.sources.len(), 1);
    assert_eq!(
        parsed.sources["docker"].suites,
        vec!["bookworm".to_string()]
    );
    assert_eq!(parsed.sources["docker"].signed_by_sha256, docker.sha256);
}

/// A country mirror of the distribution — `o=Debian` from a site outside `*.debian.org`, here
/// over plain http — is the distribution's own: counted, never refused as a third party. The
/// third-party repository beside it is still carried, its `Architectures` in §4's place.
#[test]
fn a_country_mirror_of_the_distribution_is_the_distributions_own() {
    let case = Case::new("debian-12", "mirror");
    let sources_list = case.root.read("etc/apt/sources.list");
    assert!(
        sources_list.contains("ftp.mirror.example.invalid") && !sources_list.contains("debian.org"),
        "the case is a machine whose distribution comes from a mirror: {sources_list}"
    );
    let machine = case.machine();
    let repos = &machine.repositories;
    assert_eq!(repos.distribution, 3, "{repos:?}");
    assert!(repos.refused.is_empty(), "{repos:?}");
    assert_eq!(repos.carried.len(), 1);
    assert_eq!(
        repos.carried[0].packages,
        vec!["docker-ce", "docker-ce-cli"]
    );
    let emitted = case.emitted();
    assert!(
        emitted.contains(
            "# components = [\"stable\"]\n\
             # architectures = [\"amd64\"]\n\
             # signed_by = \"files/etc/apt/keyrings/lodi-docker.asc\"\n"
        ),
        "{emitted}"
    );
    assert!(!emitted.contains("ftp.mirror.example.invalid"), "{emitted}");
}

/// A machine whose apt keeps its indexes compressed (`Acquire::GzipIndexes`) has no plain
/// `Packages` file at all. Attribution never reads one, so its third-party package is still
/// attributed and its repository still carried.
#[test]
fn a_machine_with_compressed_indexes_still_attributes_its_third_party_package() {
    let case = Case::new("debian-12", "compressed");
    let lists: Vec<String> = fs::read_dir(case.root.path("var/lib/apt/lists"))
        .expect("the lists")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name != "recorded_Packages")
        .collect();
    assert!(
        !lists.is_empty() && lists.iter().all(|name| name.ends_with(".lz4")),
        "the case is a machine whose indexes are compressed: {lists:?}"
    );
    let machine = case.machine();
    let repos = &machine.repositories;
    assert_eq!(repos.carried.len(), 1, "{repos:?}");
    assert_eq!(repos.carried[0].name, "docker");
    assert_eq!(
        repos.carried[0].packages,
        vec!["docker-ce", "docker-ce-cli"]
    );
    assert!(case.emitted().contains("# [sources.docker]\n"));
}

/// The recording says which repositories offer the installed version, not where it was installed
/// from. A version two enabled repositories offer is attributed to neither, and named as
/// ambiguous; the one that offers another name alone still carries that name, and the other,
/// with nothing attributed to it, is refused by name.
#[test]
fn a_package_more_than_one_repository_offers_is_attributed_to_no_block() {
    let case = Case::new("debian-12", "ambiguous");
    let machine = case.machine();
    let repos = &machine.repositories;
    assert!(
        repos
            .unattributed
            .contains(&("docker-ce".to_string(), Unattributed::Ambiguous(2))),
        "{repos:?}"
    );
    assert_eq!(repos.carried.len(), 1, "{repos:?}");
    assert_eq!(repos.carried[0].packages, vec!["docker-ce-cli"]);
    assert_eq!(repos.refused.len(), 1, "{repos:?}");
    assert_eq!(
        repos.refused[0].path,
        "/etc/apt/sources.list.d/docker-mirror.sources"
    );
    assert_eq!(repos.refused[0].reason, Reason::NothingInstalled);
    let emitted = case.emitted();
    assert!(!emitted.contains("# #   docker-ce\n"), "guessed: {emitted}");
    assert!(
        emitted.contains(
            "#   docker-ce\n#     ambiguous: 2 repositories offer the installed version, so none \
             is guessed\n"
        ),
        "{emitted}"
    );
}

/// Attribution counts the distinct enabled repositories of the recording that offer the installed
/// version, not the stanzas that list one of them: two repositories in one multi-URI stanza, and
/// one listed repository beside one no stanza lists, are two repositories each. In both cases
/// docker-ce is ambiguous under NOT CAPTURED and in no block, and docker-ce-cli, which Docker's
/// repository alone offers, is still carried (si-1 validator-repair-1).
#[test]
fn a_package_two_policy_repositories_offer_is_ambiguous_however_the_stanzas_list_them() {
    for scenario in ["ambiguous-multi-uri", "ambiguous-unlisted"] {
        let case = Case::new("debian-12", scenario);
        let machine = case.machine();
        let repos = &machine.repositories;
        assert!(
            repos
                .unattributed
                .contains(&("docker-ce".to_string(), Unattributed::Ambiguous(2))),
            "{scenario}: {repos:?}"
        );
        assert!(
            repos
                .carried
                .iter()
                .all(|repo| !repo.packages.contains(&"docker-ce".to_string())),
            "{scenario}: guessed: {repos:?}"
        );
        assert!(
            repos
                .refused
                .iter()
                .all(|repo| !repo.packages.contains(&"docker-ce".to_string())),
            "{scenario}: {repos:?}"
        );
        assert_eq!(repos.carried.len(), 1, "{scenario}: {repos:?}");
        assert_eq!(repos.carried[0].name, "docker");
        assert_eq!(repos.carried[0].packages, vec!["docker-ce-cli"]);
        let emitted = case.emitted();
        assert!(
            !emitted.contains("# #   docker-ce\n"),
            "{scenario}: {emitted}"
        );
        assert!(
            emitted.contains(
                "#   docker-ce\n#     ambiguous: 2 repositories offer the installed version, so \
                 none is guessed\n"
            ),
            "{scenario}: {emitted}"
        );
    }
    // The multi-URI stanza's block carries both of its URIs; the unlisted repository is in no
    // block at all.
    let multi = Case::new("debian-12", "ambiguous-multi-uri").emitted();
    assert!(
        multi.contains(
            "# uris = [\"https://download.docker.com/linux/debian\", \
             \"https://docker-mirror.example.invalid/linux/debian\"]\n"
        ),
        "{multi}"
    );
    let unlisted = Case::new("debian-12", "ambiguous-unlisted").emitted();
    assert!(
        !unlisted.contains("# uris = [\"https://docker-mirror.example.invalid"),
        "{unlisted}"
    );
}

/// A version no enabled repository offers — its repository removed, or a package file installed
/// by hand — is attributed to no block and named as unattributed. With nothing attributed to
/// any repository, no block is written.
#[test]
fn a_package_no_repository_offers_is_attributed_to_no_block() {
    let case = Case::new("debian-12", "unattributed");
    let machine = case.machine();
    let repos = &machine.repositories;
    assert!(repos.carried.is_empty(), "{repos:?}");
    assert_eq!(
        repos.unattributed,
        ["docker-ce", "docker-ce-cli", "lodi-hand-built"]
            .iter()
            .map(|name| (name.to_string(), Unattributed::Offered))
            .collect::<Vec<_>>()
    );
    let emitted = case.emitted();
    assert!(!emitted.contains("[sources."), "{emitted}");
    assert!(
        emitted.contains(
            "# installed packages attributed to no [sources] block:\n#   docker-ce\n\
             #     unattributed: no enabled repository offers the installed version"
        ),
        "{emitted}"
    );
}

/// Every refusal of the import is named under NOT CAPTURED, by path and reason, and never
/// redacted: credentials in a URI, a query, `Trusted: yes`, an inline key block, a fingerprint,
/// a directory, a link, secret key material and bytes that are no keyring. None of them is
/// carried, no refused URI and no refused keyring byte reaches the emitted text, and nothing
/// under `/etc/apt/auth.conf.d/` is opened.
#[test]
fn every_refusal_is_named_and_nothing_refused_is_carried() {
    let case = Case::new("debian-12", "sources");
    let stanza = |uri: &str, extra: &str| {
        format!("Types: deb\nURIs: {uri}\nSuites: bookworm\nComponents: main\n{extra}")
    };
    let d = "etc/apt/sources.list.d";
    case.root.write(
        &format!("{d}/r-credentials.sources"),
        // The user information is joined at run time, so that no committed line has the shape
        // of an address (AGENTS.md section 1.6).
        &stanza(
            &format!("https://user:secret-token{}creds.example.invalid/apt", '@'),
            "Signed-By: /etc/apt/keyrings/docker.asc\n",
        ),
    );
    case.root.write(
        &format!("{d}/r-query.sources"),
        &stanza(
            "https://query.example.invalid/apt?token=abc",
            "Signed-By: /etc/apt/keyrings/docker.asc\n",
        ),
    );
    case.root.write(
        &format!("{d}/r-trusted.sources"),
        &stanza(
            "https://trusted.example.invalid/apt",
            "Trusted: yes\nSigned-By: /etc/apt/keyrings/docker.asc\n",
        ),
    );
    case.root.write(
        &format!("{d}/r-inline.sources"),
        &stanza(
            "https://inline.example.invalid/apt",
            "Signed-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n -----END PGP PUBLIC KEY BLOCK-----\n",
        ),
    );
    case.root.write(
        &format!("{d}/r-fingerprint.sources"),
        &stanza(
            "https://fingerprint.example.invalid/apt",
            "Signed-By: 0123456789ABCDEF0123456789ABCDEF01234567\n",
        ),
    );
    fs::create_dir_all(case.root.path("etc/apt/keyrings/a-directory.gpg")).unwrap();
    case.root.write(
        &format!("{d}/r-directory.sources"),
        &stanza(
            "https://directory.example.invalid/apt",
            "Signed-By: /etc/apt/keyrings/a-directory.gpg\n",
        ),
    );
    std::os::unix::fs::symlink("docker.asc", case.root.path("etc/apt/keyrings/a-link.asc"))
        .unwrap();
    case.root.write(
        &format!("{d}/r-link.sources"),
        &stanza(
            "https://link.example.invalid/apt",
            "Signed-By: /etc/apt/keyrings/a-link.asc\n",
        ),
    );
    // Generated framing only: a public-key packet followed by a secret-key packet.
    let mut secret = vec![0xc6, 4, 1, 2, 3, 4];
    secret.extend_from_slice(&[0xc5, 4, 0x5e, 0xc2, 0xe7, 0x00]);
    fs::write(case.root.path("etc/apt/keyrings/secret.gpg"), &secret).unwrap();
    case.root.write(
        &format!("{d}/r-secret.sources"),
        &stanza(
            "https://secret.example.invalid/apt",
            "Signed-By: /etc/apt/keyrings/secret.gpg\n",
        ),
    );
    case.root.write(
        "etc/apt/keyrings/html.gpg",
        "<html>a download page</html>\n",
    );
    case.root.write(
        &format!("{d}/r-html.sources"),
        &stanza(
            "https://html.example.invalid/apt",
            "Signed-By: /etc/apt/keyrings/html.gpg\n",
        ),
    );
    // Made unreadable to everyone: were it opened, the read would fail the import.
    let auth = case.root.path("etc/apt/auth.conf.d/vendor.conf");
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o000)).unwrap();

    let machine = case.machine();
    let repos = &machine.repositories;
    let reason_of = |file: &str| -> Reason {
        repos
            .refused
            .iter()
            .find(|r| r.path == format!("/{d}/{file}"))
            .unwrap_or_else(|| panic!("{file} is not refused: {repos:?}"))
            .reason
            .clone()
    };
    assert_eq!(reason_of("r-credentials.sources"), Reason::Credentials);
    assert_eq!(reason_of("r-query.sources"), Reason::Query);
    assert_eq!(reason_of("r-trusted.sources"), Reason::Trusted);
    assert_eq!(
        reason_of("r-inline.sources"),
        Reason::SignedByNotAFile("an inline key block".into())
    );
    assert!(matches!(
        reason_of("r-fingerprint.sources"),
        Reason::SignedByNotAFile(why) if why.contains("fingerprint")
    ));
    assert_eq!(
        reason_of("r-directory.sources"),
        Reason::SignedByNotAFile("not a regular file".into())
    );
    assert_eq!(
        reason_of("r-link.sources"),
        Reason::SignedByNotAFile("a symbolic link".into())
    );
    assert_eq!(reason_of("r-secret.sources"), Reason::SecretKey);
    assert_eq!(reason_of("r-html.sources"), Reason::NotAKeyring);
    assert_eq!(repos.carried.len(), 1, "only Docker's: {repos:?}");
    assert_eq!(repos.credentials, vec!["/etc/apt/auth.conf.d/vendor.conf"]);

    let emitted = case.emitted();
    for (file, why) in [
        ("r-credentials.sources", "its URI carries credentials"),
        ("r-query.sources", "its URI carries a query (`?…`)"),
        ("r-trusted.sources", "it is marked trusted"),
        ("r-inline.sources", "an inline key block"),
        ("r-fingerprint.sources", "a fingerprint names no file"),
        ("r-directory.sources", "not a regular file"),
        ("r-link.sources", "a symbolic link"),
        ("r-secret.sources", "secret key material"),
        ("r-html.sources", "not an OpenPGP public keyring"),
    ] {
        let line = format!("#   /{d}/{file}\n#     ");
        let at = emitted
            .find(&line)
            .unwrap_or_else(|| panic!("{file} is not named:\n{emitted}"));
        assert!(
            emitted[at + line.len()..]
                .lines()
                .next()
                .unwrap()
                .contains(why),
            "{file}: {emitted}"
        );
    }
    for never in ["secret-token", "user:", "token=abc", ".example.invalid/apt"] {
        assert!(!emitted.contains(never), "{never} reached the bytes");
    }
    assert!(
        emitted.contains(
            "# credential files apt keeps, which lodi never reads or carries:\n\
             #   /etc/apt/auth.conf.d/vendor.conf\n"
        ),
        "{emitted}"
    );
    fs::set_permissions(&auth, fs::Permissions::from_mode(0o644)).unwrap();
}

/// Rebased onto the landed core (LD-365's validator repair names a fragment in `uris`): a
/// stanza whose URI carries a fragment is named as carrying a fragment, and one whose URI carries
/// a query as carrying a query, each in the manifest's own words (`manifest::uri_problem`) —
/// never "a query or a fragment" — and neither repeats any part of the URI, which never reaches
/// the file.
#[test]
fn a_uri_fragment_is_named_as_a_fragment_and_a_query_as_a_query() {
    let case = Case::new("debian-12", "sources");
    let d = "etc/apt/sources.list.d";
    for (file, uri) in [
        (
            "r-fragment.sources",
            "https://fragment.example.invalid/apt#part-kept-out",
        ),
        (
            "r-query.sources",
            "https://query.example.invalid/apt?token=abc",
        ),
        (
            "r-both.sources",
            "https://both.example.invalid/apt?token=def#part-kept-out-too",
        ),
    ] {
        case.root.write(
            &format!("{d}/{file}"),
            &format!(
                "Types: deb\nURIs: {uri}\nSuites: bookworm\nComponents: main\n\
                 Signed-By: /etc/apt/keyrings/docker.asc\n"
            ),
        );
    }
    let machine = case.machine();
    let why_of = |file: &str| -> String {
        machine
            .repositories
            .refused
            .iter()
            .find(|r| r.path == format!("/{d}/{file}"))
            .unwrap_or_else(|| panic!("{file} is not refused: {:?}", machine.repositories))
            .reason
            .why()
    };
    let fragment = "its URI carries a fragment (`#…`), which no source lodi arms has";
    let query = "its URI carries a query (`?…`), which no source lodi arms has";
    assert_eq!(why_of("r-fragment.sources"), fragment);
    assert_eq!(why_of("r-query.sources"), query);
    // The manifest names the fragment first (S3's `#` before S2's query), and so does the import.
    assert_eq!(why_of("r-both.sources"), fragment);
    assert_eq!(machine.repositories.carried.len(), 1, "only Docker's");

    let emitted = case.emitted();
    for (file, why) in [
        ("r-fragment.sources", fragment),
        ("r-query.sources", query),
        ("r-both.sources", fragment),
    ] {
        let line = format!("#   /{d}/{file}\n#     {why}\n");
        assert!(emitted.contains(&line), "missing {line:?} in:\n{emitted}");
    }
    for never in [
        "part-kept-out",
        "token=abc",
        "token=def",
        ".example.invalid/apt",
    ] {
        assert!(!emitted.contains(never), "{never} reached the bytes");
    }
}

/// Rebased onto the landed core, whose one apt source reader refuses a link at `/etc` or
/// `/etc/apt` without reading its target (LD-365's validator repair): nothing else the import
/// reads under `/etc/apt` is read through a link either. A keyring that `Signed-By` names behind
/// a linked directory — one pointing inside the root, and one pointing out of it — is refused by
/// name, as a link on the way to it, and never read or carried; with `/etc/apt` itself a link the
/// import names that, carries nothing, and lists no credential file behind it. No path outside
/// the root reaches the file.
#[test]
fn nothing_the_import_reads_under_etc_apt_is_read_through_a_link() {
    let case = Case::new("debian-12", "sources");
    let keyring = fs::read(case.root.path("etc/apt/keyrings/docker.asc")).unwrap();
    let elsewhere = Root::new("import-keyring-elsewhere");
    fs::create_dir_all(elsewhere.path("keyrings")).unwrap();
    fs::write(elsewhere.path("keyrings/vendor.asc"), &keyring).unwrap();
    fs::create_dir_all(case.root.path("elsewhere/keyrings")).unwrap();
    fs::write(case.root.path("elsewhere/keyrings/vendor.asc"), &keyring).unwrap();
    std::os::unix::fs::symlink(
        "../../../elsewhere/keyrings",
        case.root.path("etc/apt/keyrings/inside"),
    )
    .unwrap();
    std::os::unix::fs::symlink(
        elsewhere.path("keyrings"),
        case.root.path("etc/apt/keyrings/outside"),
    )
    .unwrap();
    let d = "etc/apt/sources.list.d";
    for (file, host, dir) in [
        ("r-inside.sources", "inside.example.invalid", "inside"),
        ("r-outside.sources", "outside.example.invalid", "outside"),
    ] {
        case.root.write(
            &format!("{d}/{file}"),
            &format!(
                "Types: deb\nURIs: https://{host}/apt\nSuites: bookworm\nComponents: main\n\
                 Signed-By: /etc/apt/keyrings/{dir}/vendor.asc\n"
            ),
        );
    }
    let machine = case.machine();
    let repos = &machine.repositories;
    for (file, dir) in [
        ("r-inside.sources", "inside"),
        ("r-outside.sources", "outside"),
    ] {
        let refused = repos
            .refused
            .iter()
            .find(|r| r.path == format!("/{d}/{file}"))
            .unwrap_or_else(|| panic!("{file} is not refused: {repos:?}"));
        assert_eq!(
            refused.reason,
            Reason::SignedByNotAFile(format!(
                "/etc/apt/keyrings/{dir} on the way to it is a symbolic link, which lodi does \
                 not follow"
            )),
            "{file}"
        );
    }
    assert_eq!(repos.carried.len(), 1, "only Docker's: {repos:?}");
    let emitted = case.emitted();
    assert!(
        emitted.contains(
            "#   /etc/apt/sources.list.d/r-outside.sources\n#     its Signed-By is not a regular \
             keyring file: /etc/apt/keyrings/outside on the way to it is a symbolic link"
        ),
        "{emitted}"
    );
    for never in [
        elsewhere.dir.display().to_string(),
        case.root.dir.display().to_string(),
    ] {
        assert!(
            !emitted.contains(&never),
            "a path of this machine: {emitted}"
        );
    }
    drop(machine);
    drop(case);

    // `/etc/apt` itself a link to a directory elsewhere below the root.
    let case = Case::new("debian-12", "sources");
    fs::rename(case.root.path("etc/apt"), case.root.path("elsewhere-apt")).unwrap();
    std::os::unix::fs::symlink("../elsewhere-apt", case.root.path("etc/apt")).unwrap();
    let machine = case.machine();
    let repos = &machine.repositories;
    assert!(repos.carried.is_empty(), "{repos:?}");
    assert!(
        repos.credentials.is_empty(),
        "a credential file was listed through the link: {repos:?}"
    );
    assert!(
        repos.refused.iter().any(|r| matches!(
            &r.reason,
            Reason::Malformed(why) if why.contains("/etc/apt on the way to it is a symbolic link")
        )),
        "{repos:?}"
    );
    let emitted = case.emitted();
    assert!(
        !emitted.contains("auth.conf.d/vendor.conf"),
        "a credential file was named through the link:\n{emitted}"
    );
}

/// A machine with no third-party repository says so in one line, and Arch says it is not read.
#[test]
fn a_machine_without_a_repository_says_none_and_arch_says_not_read() {
    let debian = Case::new("debian-12", "chosen");
    let emitted = debian.emitted();
    assert!(
        emitted.contains("# repositories read and not written as a [sources] block: none\n"),
        "{emitted}"
    );
    assert!(!emitted.contains("[sources."), "{emitted}");
    drop(debian);
    let arch = Case::new("arch", "chosen");
    let emitted = arch.emitted();
    assert!(!arch.machine().repositories.read);
    assert_eq!(
        emitted
            .matches(
                "# third-party pacman repositories: not read; lodi arms no pacman repository\n"
            )
            .count(),
        1,
        "{emitted}"
    );
    assert!(!emitted.contains("[sources."), "{emitted}");
}

/// The repositories part of NOT CAPTURED: from `# repositories read` to the end, which is what
/// the release gate's import row reads too.
fn repositories_part(emitted: &str) -> &str {
    let at = emitted
        .find("# repositories read")
        .unwrap_or_else(|| panic!("no repositories part in:\n{emitted}"));
    &emitted[at..]
}

/// Every refusal the import read, as its path and its reason in the words it prints.
fn refusals(machine: &import::Machine) -> Vec<(String, String)> {
    machine
        .repositories
        .refused
        .iter()
        .map(|r| (r.path.clone(), r.reason.why()))
        .collect()
}

/// si-1 (S5, LD-421): an enabled extra suite of the distribution's own archive — Ubuntu's
/// `-backports` and `-proposed` — is written as a commented `[sources]` block, verified by the
/// digest of the distribution's own archive key, never named under NOT CAPTURED (LD-414 named it
/// there before 1.10.0). The `proposed` scenario is an Ubuntu 24.04 machine with the image's own
/// `ubuntu.sources` (`noble`, `-updates`, `-backports`; `-security`) and `noble-proposed`
/// enabled in a file of its own. The default suites stay counted, two stanzas.
#[test]
fn an_extra_suite_of_the_distributions_own_archive_is_a_sources_block() {
    let case = Case::new("ubuntu-24.04", "proposed");
    let machine = case.machine();
    let repos = &machine.repositories;
    assert!(repos.read && repos.indexed, "{repos:?}");
    assert_eq!(repos.distribution, 2, "the image's two stanzas: {repos:?}");
    assert!(refusals(&machine).is_empty(), "{:?}", refusals(&machine));
    let blocks: Vec<(&str, &str, bool)> = repos
        .carried
        .iter()
        .map(|repo| (repo.name.as_str(), repo.suites[0].as_str(), repo.own))
        .collect();
    assert_eq!(
        blocks,
        vec![
            ("backports", "noble-backports", true),
            ("proposed", "noble-proposed", true)
        ]
    );
    for repo in &repos.carried {
        assert_eq!(
            repo.keyring_path,
            "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
        );
        assert_eq!(
            repo.uris,
            vec!["https://archive.ubuntu.com/ubuntu/".to_string()]
        );
    }
    let emitted = case.emitted();
    for line in [
        "# [sources.proposed]\n",
        "# # read from /etc/apt/sources.list.d/ubuntu-proposed.sources\n",
        "# suites = [\"${host.codename}-proposed\"]\n",
        "# suites = [\"${host.codename}-backports\"]\n",
    ] {
        assert!(emitted.contains(line), "no {line:?}:\n{emitted}");
    }
    let part = repositories_part(&emitted);
    assert!(
        part.starts_with(
            "# repositories read and not written as a [sources] block: none\n\
             # the distribution's own repositories, which every install has: 2\n"
        ),
        "{part}"
    );
    for default in [
        "noble-updates",
        "noble-security",
        "noble-proposed",
        "noble-backports",
    ] {
        assert!(!part.contains(default), "{default} is named:\n{part}");
    }
}

/// The same rule on Debian, where one stanza lists a default suite and an extra one together and
/// every suite is written by its alias (`oldstable`, as a Debian 12 machine may since Debian 13
/// became stable). An alias is the suite its `Release` names as its codename (`n=`): `oldstable`,
/// `oldstable-updates` and `oldstable-security` are `bookworm`, `-updates` and `-security`, the
/// defaults, and are counted; `oldstable-proposed-updates` is `bookworm-proposed-updates`, which
/// a fresh install does not enable, and is named by the file that lists it. The stanza that lists
/// it is still counted for the default suites beside it.
#[test]
fn an_extra_suite_beside_default_ones_is_named_and_an_alias_of_a_default_one_is_not() {
    let case = Case::new("debian-12", "own-suite");
    let machine = case.machine();
    let repos = &machine.repositories;
    assert_eq!(repos.distribution, 2, "{repos:?}");
    assert_eq!(
        refusals(&machine),
        vec![(
            "/etc/apt/sources.list.d/debian.sources".to_string(),
            "oldstable-proposed-updates is a suite of the distribution's own archive that a \
             fresh install\ndoes not enable, and lodi does not write such a suite as a [sources] \
             block yet"
                .to_string()
        )]
    );
    let emitted = case.emitted();
    let part = repositories_part(&emitted);
    assert!(
        part.contains("# the distribution's own repositories, which every install has: 2\n"),
        "{part}"
    );
    assert_eq!(
        part.matches("/etc/apt/sources.list.d/debian.sources")
            .count(),
        1,
        "{part}"
    );
    for alias in ["oldstable\n", "oldstable-updates", "oldstable-security"] {
        assert!(!part.contains(alias), "{alias:?} is named:\n{part}");
    }
}

/// R8: Debian backports has its own Release origin, but a deb822 stanza can list it beside
/// the default suite. Do not let the default half swallow the extra half.
#[test]
fn a_mixed_deb822_stanza_names_backports_beside_the_counted_default() {
    let case = Case::new("debian-12", "mixed-backports");
    let machine = case.machine();
    assert_eq!(
        machine.repositories.distribution, 2,
        "{:?}",
        machine.repositories
    );
    let refused = refusals(&machine);
    assert_eq!(refused.len(), 1, "{refused:?}");
    assert_eq!(refused[0].0, "/etc/apt/sources.list.d/debian.sources");
    assert!(refused[0].1.contains("oldstable-backports"), "{refused:?}");
    assert!(refused[0].1.contains("[sources] block"), "{refused:?}");
    let emitted = case.emitted();
    let part = repositories_part(&emitted);
    assert_eq!(
        part.matches("#   /etc/apt/sources.list.d/debian.sources")
            .count(),
        1,
        "{part}"
    );
    assert!(part.contains("oldstable-backports"), "{part}");
    assert!(!part.contains("oldstable-updates"), "{part}");
}

/// R8: a `deb` and a `deb-src` line of one suite are one block, never two.
#[test]
fn repeated_one_line_sources_make_one_block_for_an_extra_suite() {
    let case = Case::new("ubuntu-24.04", "one-line-proposed");
    let machine = case.machine();
    assert_eq!(
        machine.repositories.distribution, 2,
        "{:?}",
        machine.repositories
    );
    assert!(refusals(&machine).is_empty(), "{:?}", refusals(&machine));
    let proposed: Vec<_> = machine
        .repositories
        .carried
        .iter()
        .filter(|repo| repo.path == "/etc/apt/sources.list.d/ubuntu-proposed.list")
        .collect();
    assert_eq!(proposed.len(), 1, "{proposed:?}");
    assert_eq!(proposed[0].name, "proposed");
    assert_eq!(
        proposed[0].types,
        vec!["deb".to_string(), "deb-src".to_string()]
    );
    let emitted = case.emitted();
    assert_eq!(
        emitted.matches("# [sources.proposed").count(),
        1,
        "{emitted}"
    );
}

/// Regression for D8 (LD-414): an ordinary machine names nothing new. The Ubuntu image's own
/// `ubuntu.sources` — whose `-backports` is a block since si-1 — and a Debian machine's default
/// suites, by codename or by alias, are counted and never named.
#[test]
fn an_ordinary_ubuntu_or_debian_machine_names_no_suite_of_its_own_archive() {
    let ubuntu = Case::new("ubuntu-24.04", "proposed");
    fs::remove_file(
        ubuntu
            .root
            .path("etc/apt/sources.list.d/ubuntu-proposed.sources"),
    )
    .expect("the proposed stanza");
    let machine = ubuntu.machine();
    assert_eq!(machine.repositories.distribution, 2);
    assert!(refusals(&machine).is_empty(), "{:?}", refusals(&machine));
    assert!(repositories_part(&ubuntu.emitted()).starts_with(
        "# repositories read and not written as a [sources] block: none\n\
             # the distribution's own repositories, which every install has: 2\n"
    ));
    drop(ubuntu);

    let debian = Case::new("debian-12", "own-suite");
    let stanzas = debian.root.read("etc/apt/sources.list.d/debian.sources");
    debian.root.write(
        "etc/apt/sources.list.d/debian.sources",
        &stanzas.replace(" oldstable-proposed-updates", ""),
    );
    let machine = debian.machine();
    assert_eq!(machine.repositories.distribution, 2);
    assert!(refusals(&machine).is_empty(), "{:?}", refusals(&machine));
    drop(debian);

    for scenario in ["sources", "mirror"] {
        let debian = Case::new("debian-12", scenario);
        let machine = debian.machine();
        assert_eq!(machine.repositories.distribution, 3, "{scenario}");
        assert!(
            refusals(&machine)
                .iter()
                .all(|(_, why)| !why.contains("the distribution's own archive")),
            "{scenario}: {:?}",
            refusals(&machine)
        );
    }
}
