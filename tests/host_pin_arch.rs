//! Arch host pins over measured, trimmed core.db and extra.db served on loopback.
//! No test runs a host verb without a scratch --root or contacts the public archive.
#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
#[path = "support/wait.rs"]
mod wait;

use fakehost::{Case, Machine, Pkg, story};
use lodi::fetch::{HttpFetcher, parse_rewrites};
use lodi::hostscope::pin::{self, Context, Declared, Request};
use lodi::hostscope::safety::Distro;
use std::collections::BTreeMap;
use std::path::Path;

const DAY: &str = "2026-09-01T00:00:00Z";

fn fixture_urls() -> BTreeMap<String, Vec<u8>> {
    let mut out = BTreeMap::new();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01");
    for repo in ["core", "extra"] {
        let folder = root.join(format!("{repo}/os/x86_64"));
        let bytes = std::fs::read(folder.join(format!("{repo}.db"))).unwrap();
        out.insert(
            format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/{repo}.db"),
            bytes,
        );
        if repo == "extra" {
            for name in [
                "tree-2.3.2-1-x86_64.pkg.tar.zst",
                "bc-1.08.2-1-x86_64.pkg.tar.zst",
            ] {
                let url = format!(
                    "https://archive.archlinux.org/repos/2026/09/01/extra/os/x86_64/{name}"
                );
                out.insert(url.clone(), std::fs::read(folder.join(name)).unwrap());
                out.insert(
                    format!("{url}.sig"),
                    std::fs::read(folder.join(format!("{name}.sig"))).unwrap(),
                );
            }
        }
    }
    out
}

fn context<'a>(
    sources: &'a BTreeMap<String, lodi::hostscope::manifest::SourceEntry>,
) -> Context<'a> {
    Context {
        distro: Distro::Arch,
        codename: "rolling",
        arch: "x86_64",
        sources,
        now: lodi::util::parse_utc("2026-09-03T00:00:00Z").unwrap(),
    }
}

#[test]
fn per_entry_pin_installs_verified_archive_file_with_lodi_only_hold() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new("arch-pin-file", Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let plan = case.verb_env("plan", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    let apply = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(
        fakehost::err(&apply).contains("W_ARCH_PARTIAL"),
        "{}",
        story(&apply)
    );
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    let log = case.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("pacman -Syy --root") && line.contains(" --config ")),
        "{log:?}"
    );
    assert!(
        log.iter()
            .any(|line| line.contains("-U --root")
                && line.contains("tree-2.3.2-1-x86_64.pkg.tar.zst")),
        "{log:?}"
    );
    assert!(
        log.iter().all(|line| !line.contains("pacman-key")),
        "{log:?}"
    );
    assert!(
        server
            .requests()
            .iter()
            .any(|url| url.ends_with(".pkg.tar.zst.sig")),
        "Lodi must fetch the real detached signature beside the pinned package"
    );
    let requests = server.requests().len();
    let after = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert!(fakehost::nothing(&after), "{}", story(&after));
    assert_eq!(
        server.requests().len(),
        requests,
        "a settled Arch pin makes zero requests"
    );
}

/// A pin taken out of host.toml (a `git revert`) releases the lodi-only hold it made: the plan
/// says so, the record no longer holds it, nothing on the machine ever held it, and the next
/// switch has nothing to do (#712).
#[test]
fn removing_an_arch_pin_releases_its_lodi_only_hold_and_the_plan_says_so() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new(
        "arch-pin-release",
        Machine::arch().offering(Pkg::new("tree")),
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01");
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &fixture.join(format!("{repo}/os/x86_64")),
        );
    }
    let host = "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n";
    case.set_manifest(&format!("{host}\n[packages.pin]\ntree = \"2026-09-01\"\n"));
    let rewrite = [("LODI_FETCH_REWRITE", server.rewrite())];
    let env: Vec<(&str, &str)> = rewrite.iter().map(|(k, v)| (*k, v.as_str())).collect();
    let pinned = case.verb_env("apply", &[], &env);
    assert_eq!(pinned.status.code(), Some(0), "{}", story(&pinned));
    assert_eq!(case.lock()["packages"]["tree"]["held"], true);

    case.set_manifest(host);
    let plan = case.verb_env("plan", &[], &env);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(
        fakehost::err(&plan).contains("~ package tree (unhold"),
        "{}",
        story(&plan)
    );
    let apply = case.verb_env("apply", &[], &env);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert_eq!(case.lock()["packages"]["tree"]["held"], false);
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    assert!(!case.root.exists("etc/pacman.conf"));
    let after = case.verb_env("apply", &[], &env);
    assert!(fakehost::nothing(&after), "{}", story(&after));
}

#[test]
fn missing_arch_package_signature_refuses_pinned_apply() {
    let mut urls = fixture_urls();
    urls.retain(|url, _| !url.ends_with(".pkg.tar.zst.sig"));
    let server = support::Server::start(urls);
    let case = Case::new(
        "arch-pin-no-sig",
        Machine::arch().offering(Pkg::new("tree")),
    );
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01");
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &fixture.join(format!("{repo}/os/x86_64")),
        );
    }
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n",
    );
    let apply = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_ne!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(
        server
            .requests()
            .iter()
            .any(|url| url.ends_with(".pkg.tar.zst.sig"))
    );
    assert!(case.machine()["installed"].get("tree").is_none());
    assert!(!case.log().iter().any(|line| line.starts_with("pacman -U ")));
    assert!(!case.root.exists("var/lib/lodi/host/pin/stage"));
}

#[test]
fn dated_arch_refresh_forces_older_database() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new("arch-older-db", Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64"
            )),
        );
    }
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n",
    );
    let applied = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    let log = case.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("pacman -Syy --root") && line.contains(" --config ")),
        "a dated archive older than the live sync database must be forced: {log:?}"
    );
}

/// A pinned case whose dated `-Syy` fails first with each of `failures`, in libalpm's words.
fn dated_sync_case(name: &str, failures: &[&str]) -> Case {
    let case = Case::new(name, Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64"
            )),
        );
    }
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n\
         [packages.pin]\ntree = \"2026-09-01\"\n",
    );
    case.edit(|m| m["sync_failures"] = failures.to_vec().into());
    case
}

fn dated_syncs(case: &Case) -> usize {
    case.log()
        .iter()
        .filter(|line| line.starts_with("pacman -Syy --root") && line.contains(" --config "))
        .count()
}

/// A dated sync that timed out is tried again, like a fetch (LD-386): the 1.4.1 rehearsal's Arch
/// guest failed a whole apply on one `archive.archlinux.org` connect timeout inside `-Syy`.
#[test]
fn a_dated_arch_sync_that_timed_out_is_tried_again() {
    let server = support::Server::start(fixture_urls());
    let case = dated_sync_case(
        "arch-sync-timeout",
        &["Connection timed out after 10001 milliseconds"],
    );
    let apply = case.verb_env(
        "apply",
        &[],
        &[
            ("LODI_FETCH_REWRITE", &server.rewrite()),
            ("LODI_FETCH_ATTEMPTS", "2"),
        ],
    );
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    assert_eq!(dated_syncs(&case), 2, "{:?}", case.log());
    assert!(
        fakehost::err(&apply).contains("retrying in"),
        "{}",
        story(&apply)
    );
}

/// The retries are bounded by `LODI_FETCH_ATTEMPTS`, and an answer is not tried again: nothing is
/// installed either way.
#[test]
fn a_dated_arch_sync_gives_up_after_its_attempts_and_never_retries_an_answer() {
    let server = support::Server::start(fixture_urls());
    for (name, failures, attempts, syncs) in [
        ("arch-sync-gave-up", &["Operation too slow"; 3][..], "2", 2),
        (
            "arch-sync-404",
            &["The requested URL returned error: 404"][..],
            "10",
            1,
        ),
    ] {
        let case = dated_sync_case(name, failures);
        let apply = case.verb_env(
            "apply",
            &[],
            &[
                ("LODI_FETCH_REWRITE", &server.rewrite()),
                ("LODI_FETCH_ATTEMPTS", attempts),
            ],
        );
        assert_eq!(apply.status.code(), Some(4), "{name}: {}", story(&apply));
        assert!(
            fakehost::err(&apply).contains("E_REPO_UNREACHABLE"),
            "{name}: {}",
            story(&apply)
        );
        assert_eq!(dated_syncs(&case), syncs, "{name}: {:?}", case.log());
        assert!(case.machine()["installed"].get("tree").is_none(), "{name}");
    }
}

#[test]
fn interrupted_arch_stage_is_the_next_applies_journalled_first_action() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new("arch-pin-sweep", Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let first = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    case.root
        .write("var/lib/lodi/host/pin/stage/left-behind", "partial\n");
    let plan = case.verb_env("plan", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert!(
        fakehost::err(&plan).contains("remove the pin stage an interrupted apply left"),
        "{}",
        story(&plan)
    );
    let second = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(second.status.code(), Some(0), "{}", story(&second));
    assert!(!case.root.exists("var/lib/lodi/host/pin/stage"));
    let journal = hostroot::journals(&case.root)
        .into_iter()
        .map(|p| std::fs::read_to_string(p).unwrap())
        .find(|s| s.contains("\"s0\""))
        .expect("the sweep is journalled");
    assert!(
        journal.contains("remove the pin stage an interrupted apply left"),
        "{journal}"
    );
}

#[test]
fn arch_readback_mismatch_is_closure_drift_after_machine_record_is_written() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new(
        "arch-pin-readback",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.edit(|m| m["skew"]["tree"] = "9.9-1".into());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let applied = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(applied.status.code(), Some(6), "{}", story(&applied));
    assert!(
        fakehost::err(&applied).contains("E_CLOSURE_DRIFT"),
        "{}",
        story(&applied)
    );
    assert_eq!(
        case.lock()["packages"]["tree"]["version"],
        "9.9-1",
        "the record reports what pacman shows"
    );
}

#[test]
fn a_pin_moved_outside_lodi_is_drift_and_unpin_keeps_its_version() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new("arch-pin-drift", Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    let unpinned =
        "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n";
    case.set_manifest(&format!(
        "{unpinned}\n[packages.pin]\ntree = \"2026-09-01\"\n"
    ));
    let first = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    let mut live = Pkg::new("tree");
    live.version = "9.9-1".into();
    case.install_by_hand(live);
    let plan = case.verb_env("plan", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert!(fakehost::err(&plan).contains("W_DRIFT"), "{}", story(&plan));
    let declined = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(declined.status.code(), Some(11), "{}", story(&declined));
    let restored = case.verb_env(
        "apply",
        &["--overwrite-drift"],
        &[("LODI_FETCH_REWRITE", &server.rewrite())],
    );
    assert_eq!(restored.status.code(), Some(0), "{}", story(&restored));
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    assert!(
        case.log()
            .iter()
            .any(|line| line.contains(" --ignore tree ") || line.ends_with(" --ignore tree --"))
    );
    case.set_manifest(unpinned);
    let removed = case.apply(&[]);
    assert_eq!(removed.status.code(), Some(0), "{}", story(&removed));
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    assert_eq!(case.lock()["packages"]["tree"]["held"], false);
}

#[test]
fn an_arch_apply_never_writes_host_directory_or_pacman_configuration() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new(
        "arch-pin-census",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.root
        .write("etc/pacman.conf", "[options]\nArchitecture = auto\n");
    case.root.write(
        "etc/pacman.d/mirrorlist",
        "Server = https://mirror.example/\n",
    );
    let source = case.config();
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let census = |dir: &Path| -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                entries.extend(census_recursive(&path));
            } else {
                entries.push((
                    path.strip_prefix(&case.root.dir)
                        .unwrap()
                        .display()
                        .to_string(),
                    std::fs::read(&path).unwrap(),
                    path.metadata().unwrap().modified().unwrap(),
                ));
            }
        }
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        entries
    };
    let before_host = census(&source);
    let config = [
        case.root.path("etc/pacman.conf"),
        case.root.path("etc/pacman.d/mirrorlist"),
    ];
    let before_config: Vec<_> = config
        .iter()
        .map(|path| {
            (
                std::fs::read(path).unwrap(),
                path.metadata().unwrap().modified().unwrap(),
            )
        })
        .collect();
    let applied = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    // The switch fills the config's missing `lodi.lock` first (LD-498); nothing else changes.
    let mut after_host = census(&source);
    after_host.retain(|(path, _, _)| !path.ends_with("/lodi.lock"));
    assert_eq!(after_host, before_host, "the config is a read-only input");
    let after_config: Vec<_> = config
        .iter()
        .map(|path| {
            (
                std::fs::read(path).unwrap(),
                path.metadata().unwrap().modified().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        after_config, before_config,
        "no pacman configuration bytes or mtimes changed"
    );
    assert!(
        case.log()
            .iter()
            .all(|line| !line.starts_with("pacman-key "))
    );
}

fn census_recursive(dir: &Path) -> Vec<(String, Vec<u8>, std::time::SystemTime)> {
    let mut rows = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            rows.extend(census_recursive(&path));
        } else {
            rows.push((
                path.display().to_string(),
                std::fs::read(&path).unwrap(),
                path.metadata().unwrap().modified().unwrap(),
            ));
        }
    }
    rows
}

#[test]
fn a_package_file_with_a_changed_byte_is_not_installed_or_kept() {
    let mut files = fixture_urls();
    let key = files
        .keys()
        .find(|key| key.ends_with("tree-2.3.2-1-x86_64.pkg.tar.zst"))
        .unwrap()
        .clone();
    files.get_mut(&key).unwrap()[15] ^= 1;
    let server = support::Server::start(files);
    let case = Case::new(
        "arch-pin-package-hash",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let result = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(result.status.code(), Some(5), "{}", story(&result));
    assert!(
        fakehost::err(&result).contains("E_HASH_MISMATCH"),
        "{}",
        story(&result)
    );
    assert!(!case.installed().contains("tree"));
    assert!(!case.root.exists("var/lib/lodi/host/pin/stage"));
}

#[test]
fn recorded_arch_db_digests_are_checked_against_the_sync_before_any_install() {
    let server = support::Server::start(fixture_urls());
    let lock = resolved_arch_lock(&server);
    let case = Case::new(
        "arch-pin-recorded-db",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.edit(|m| m["pin_corrupt_db"] = true.into());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    // The pins as `lodi pin` recorded them: the host's section of the config's `lodi.lock`.
    let recorded = case.set_pins(&lock);
    let prior = server.requests().len();
    let plan = case.verb_env("plan", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert_eq!(
        server.requests().len(),
        prior,
        "a recorded lock's plan is offline"
    );
    let applied = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(applied.status.code(), Some(5), "{}", story(&applied));
    assert!(
        fakehost::err(&applied).contains("E_HASH_MISMATCH"),
        "{}",
        story(&applied)
    );
    assert!(!case.installed().contains("tree"));
    assert_eq!(
        std::fs::read_to_string(case.config().join("lodi.lock")).unwrap(),
        recorded,
        "the host pin is never rewritten"
    );
}

#[test]
fn synchronised_db_must_match_recorded_digest_before_any_package_change() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new(
        "arch-pin-db-hash",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.edit(|m| m["pin_corrupt_db"] = true.into());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let result = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(result.status.code(), Some(5), "{}", story(&result));
    assert!(
        fakehost::err(&result).contains("E_HASH_MISMATCH"),
        "{}",
        story(&result)
    );
    assert!(!case.installed().contains("tree"));
    assert!(
        case.log()
            .iter()
            .all(|line| !line.starts_with("pacman -U ") && !line.starts_with("pacman -Syu "))
    );
}

#[test]
fn pacman_dry_run_refuses_unsatisfiable_local_pin_before_transaction() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new("arch-pin-unsat", Machine::arch().offering(Pkg::new("tree")));
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.edit(|m| m["pin_unsatisfiable"] = true.into());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n");
    let result = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
    assert_eq!(result.status.code(), Some(4), "{}", story(&result));
    assert!(
        fakehost::err(&result).contains("E_PIN_UNSATISFIABLE"),
        "{}",
        story(&result)
    );
    assert!(!case.has_lock(), "nothing was installed or recorded");
    assert!(
        !case.root.exists("var/lib/lodi/host/pin/stage"),
        "staged files are removed"
    );
    assert!(
        case.log()
            .iter()
            .all(|line| !line.starts_with("pacman -U ") && !line.starts_with("pacman -Syu "))
    );
}

#[test]
fn arch_snapshot_install_reads_dated_databases_and_syncs_through_pin_conf() {
    let case = Case::new(
        "arch-pin-dated-sync",
        Machine::arch().offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(&format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64")));
    }
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n");
    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    let log = case.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("pacman -Syu --root") && line.contains(" --config ")),
        "{log:?}"
    );
    assert!(
        log.iter().all(|line| !line.starts_with("pacman -U ")),
        "snapshot is not a local-file pin: {log:?}"
    );
    let second = case.apply(&[]);
    assert!(fakehost::nothing(&second), "{}", story(&second));
}

#[test]
fn arch_snapshot_plan_uses_a_dated_conf_without_inert_note() {
    let case = Case::new(
        "arch-pin-snapshot",
        Machine::arch().offering(Pkg::new("tree")),
    );
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"arch\"\nsnapshot = \"2026-09-01T00:00:00Z\"\n\n[packages]\ncommon = [\"tree\"]\n");
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(!fakehost::err(&plan).contains("inert"), "{}", story(&plan));
    assert!(
        fakehost::err(&plan).contains("+ package tree"),
        "{}",
        story(&plan)
    );
}

#[test]
fn dated_arch_databases_keep_package_signatures_required() {
    let conf = lodi::hostscope::pin::arch::render_config(DAY).unwrap();
    assert!(
        conf.starts_with("[options]\nArchitecture = x86_64\n"),
        "{conf}"
    );
    assert!(conf.contains(
        "[core]\nServer = https://archive.archlinux.org/repos/2026/09/01/core/os/x86_64/\n"
    ));
    assert!(conf.contains(
        "[extra]\nServer = https://archive.archlinux.org/repos/2026/09/01/extra/os/x86_64/\n"
    ));
    assert!(
        conf.contains("SigLevel = Required DatabaseOptional\n"),
        "dated databases have no .db.sig, but packages must stay signed: {conf}"
    );
    assert_eq!(
        conf.lines()
            .filter(|line| line.starts_with("SigLevel = "))
            .count(),
        1
    );
    assert!(!conf.contains("SigLevel = Never"), "{conf}");
    assert!(conf.contains("LocalFileSigLevel = Required\n"), "{conf}");
    assert!(!conf.contains("/etc/"), "{conf}");
    let files = fixture_urls();
    for repo in ["core", "extra"] {
        let url =
            format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/{repo}.db");
        let bytes = files.get(&url).expect("measured archive database");
        assert!(
            bytes.starts_with(&[0x1f, 0x8b]),
            "real gzip database: {repo}"
        );
    }
}

#[test]
fn arch_manifest_accepts_common_and_distribution_pin_tables() {
    let text = "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n[packages.pin]\ntree = \"2026-09-01\"\n\n[packages.arch.pin]\ntree = \"2026-09-01T12:00:00Z\"\n";
    let context = lodi::hostscope::manifest::Context {
        arch: "x86_64".into(),
        os: "linux".into(),
        distro: "arch".into(),
        release: "rolling".into(),
        codename: "".into(),
        lodi_version: env!("CARGO_PKG_VERSION").into(),
    };
    let parsed = lodi::hostscope::manifest::parse(text, "host.toml", &context)
        .expect("Arch pin tables have the same grammar as apt's");
    assert_eq!(
        parsed.packages.pins("arch")["tree"].requested,
        "2026-09-01T12:00:00Z"
    );
}

#[test]
fn arch_bounds_and_version_without_a_digest_interface_refuse_before_a_request() {
    let server = support::Server::start(fixture_urls());
    let sources = BTreeMap::new();
    for (value, code) in [
        ("2026-09-03T00:00:00Z", "E_SNAPSHOT_FUTURE"),
        ("2024-04-30T00:00:00Z", "E_SNAPSHOT_TOO_OLD"),
    ] {
        let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
            Ok(Box::new(HttpFetcher::new(
                parse_rewrites(&server.rewrite()).unwrap(),
            )))
        };
        assert_eq!(
            pin::resolve(
                &context(&sources),
                Some(value),
                &BTreeMap::new(),
                None,
                &mut fetch
            )
            .unwrap_err()
            .code,
            code
        );
    }
    let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
        Ok(Box::new(HttpFetcher::new(
            parse_rewrites(&server.rewrite()).unwrap(),
        )))
    };
    let versions = BTreeMap::from([(
        "tree".into(),
        Declared {
            requested: "2.3.2-1".into(),
            request: Request::Version("2.3.2-1".into()),
        },
    )]);
    let refusal = pin::resolve(&context(&sources), None, &versions, None, &mut fetch).unwrap_err();
    assert_eq!(refusal.code, "E_UNSUPPORTED");
    assert!(refusal.to_string().contains("YYYY-MM-DD"), "{refusal}");
    assert!(server.requests().is_empty());
}

fn resolved_arch_lock(server: &support::Server) -> pin::PinsLock {
    let sources = BTreeMap::new();
    let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
        Ok(Box::new(HttpFetcher::new(
            parse_rewrites(&server.rewrite()).unwrap(),
        )))
    };
    let date = "2026-09-01";
    let declared = BTreeMap::from([(
        "tree".to_string(),
        Declared {
            requested: date.into(),
            request: Request::Date(lodi::util::parse_utc(DAY).unwrap()),
        },
    )]);
    let resolution =
        pin::resolve(&context(&sources), Some(DAY), &declared, None, &mut fetch).unwrap();
    let mut lock = resolution.lock("arch", "rolling", "x86_64");
    lock.snapshot.as_mut().unwrap().indexes = pin::index_digests(
        &HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap()),
        Distro::Arch,
        "rolling",
        lodi::util::parse_utc(DAY).unwrap(),
    )
    .unwrap();
    lock
}

#[test]
fn dated_core_and_extra_are_resolved_through_the_verified_index() {
    let server = support::Server::start(fixture_urls());
    let sources = BTreeMap::new();
    let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
        Ok(Box::new(HttpFetcher::new(
            parse_rewrites(&server.rewrite()).unwrap(),
        )))
    };
    let pins = ["pacman", "tree"]
        .map(|name| {
            (
                name.to_owned(),
                Declared {
                    requested: "2026-09-01".into(),
                    request: Request::Date(lodi::util::parse_utc(DAY).unwrap()),
                },
            )
        })
        .into_iter()
        .collect();
    let resolved = pin::resolve(&context(&sources), Some(DAY), &pins, None, &mut fetch)
        .expect("the measured Arch databases resolve without apt metadata");
    assert_eq!(resolved.pins["pacman"].version, "7.1.0.r9.g54d9411-2");
    assert_eq!(resolved.pins["pacman"].repository, "core");
    assert_eq!(resolved.pins["tree"].version, "2.3.2-1");
    assert_eq!(resolved.pins["tree"].repository, "extra");
    assert!(
        resolved
            .pins
            .values()
            .all(|p| p.filename.ends_with(".pkg.tar.zst") && p.sha256.starts_with("sha256:"))
    );
    let lock = resolved.lock("arch", "rolling", "x86_64");
    assert_eq!(lock.snapshot.unwrap().indexes.len(), 2);
    assert_eq!(
        server.requests().len(),
        2,
        "one verified .db for each repository"
    );
}

/// #172: with two pins, the refusal names the one pacman's own output names.
#[test]
fn unsatisfiable_second_pin_is_the_one_named() {
    let server = support::Server::start(fixture_urls());
    let case = Case::new(
        "arch-pin-unsat-second",
        Machine::arch()
            .offering(Pkg::new("bc"))
            .offering(Pkg::new("tree")),
    );
    for repo in ["core", "extra"] {
        case.archive(
            &format!("https://archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64/"),
            &archive_dir(repo),
        );
    }
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"bc\", \"tree\"]\n\n\
         [packages.pin]\nbc = \"2026-09-01\"\ntree = \"2026-09-01\"\n",
    );
    for (unmet, named, unnamed) in [("tree", "`tree`", Some("`bc`")), ("zlib", "`bc`", None)] {
        case.edit(|m| m["pin_unsatisfiable"] = unmet.into());
        let result = case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]);
        let error = fakehost::err(&result);
        assert_eq!(result.status.code(), Some(4), "{}", story(&result));
        assert!(error.contains("E_PIN_UNSATISFIABLE"), "{error}");
        assert!(error.contains(named), "{unmet}: {error}");
        match unnamed {
            Some(other) => assert!(!error.contains(other), "{unmet}: {error}"),
            // Output naming no pin lists every staged pin.
            None => assert!(error.contains("`tree`"), "{unmet}: {error}"),
        }
        assert!(
            case.log()
                .iter()
                .all(|line| !line.starts_with("pacman -U "))
        );
    }
}

fn archive_dir(repo: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01/{repo}/os/x86_64"
    ))
}

/// The real signer of the measured `tree` 2.3.2-1 signature, which the host keyring lacks here.
const TREE_SIGNER: &str = "E499C79F53C96A54E572FEE1C06086337C50773E";
/// The signer of the unit keyring package, trusted by the host keyring unless a test removes it.
const KEYRING_SIGNER: &str = "0429897DE5F3BDAC537A30696D42BDD116E0068F";
const KEYRING: &str = "archlinux-keyring-20260727-1-any.pkg.tar.zst";

/// A v4 signature packet that names its issuer the way gpg writes it. The fake checks only the
/// issuer; the guest row verifies the real packages' real signatures.
fn signature(issuer: &str) -> Vec<u8> {
    let fingerprint: Vec<u8> = (0..40)
        .step_by(2)
        .map(|i| u8::from_str_radix(&issuer[i..i + 2], 16).unwrap())
        .collect();
    let mut hashed = vec![22, 33, 4];
    hashed.extend(&fingerprint);
    let mut body = vec![4, 0, 1, 8, 0, hashed.len() as u8];
    body.extend(&hashed);
    body.extend([0, 0, 0xab, 0xcd]);
    let mut packet = vec![0x88, body.len() as u8];
    packet.extend(body);
    packet
}

struct Keyring {
    /// Keys the dated keyring package ships, and the ones it revokes.
    shipped: &'static [&'static str],
    revoked: &'static [&'static str],
    keyring_sig: bool,
    tree_sig: bool,
}

const SOUND: Keyring = Keyring {
    shipped: &[TREE_SIGNER],
    revoked: &[],
    keyring_sig: true,
    tree_sig: true,
};

/// A machine whose keyring trusts only the keyring package's signer, pinning `tree` to a day
/// whose core.db serves a signed `archlinux-keyring`.
fn keyring_case(name: &str, keyring: Keyring, opt_in: bool) -> (Case, support::Server) {
    let base = "https://archive.archlinux.org/repos/2026/09/01/core/os/x86_64/";
    let lines = |keys: &[&str]| keys.iter().map(|k| format!("{k}\n")).collect::<String>();
    let package = support::tar(&[
        support::file(
            "usr/share/pacman/keyrings/archlinux.gpg",
            &lines(keyring.shipped),
            0o644,
        ),
        support::file(
            "usr/share/pacman/keyrings/archlinux-trusted",
            &format!("{KEYRING_SIGNER}:4:\n"),
            0o644,
        ),
        support::file(
            "usr/share/pacman/keyrings/archlinux-revoked",
            &lines(keyring.revoked),
            0o644,
        ),
    ]);
    let desc = format!(
        "%FILENAME%\n{KEYRING}\n\n%NAME%\narchlinux-keyring\n\n%VERSION%\n20260727-1\n\n\
         %ARCH%\nany\n\n%SHA256SUM%\n{}\n\n",
        lodi::util::sha256_hex(&package)
    );
    let database = support::gzip(&support::tar(&[support::file(
        "archlinux-keyring-20260727-1/desc",
        &desc,
        0o644,
    )]));
    let mut urls = fixture_urls();
    urls.insert(format!("{base}core.db"), database.clone());
    urls.insert(format!("{base}{KEYRING}"), package);
    if keyring.keyring_sig {
        urls.insert(format!("{base}{KEYRING}.sig"), signature(KEYRING_SIGNER));
    }
    if !keyring.tree_sig {
        urls.retain(|url, _| !url.ends_with("tree-2.3.2-1-x86_64.pkg.tar.zst.sig"));
    }
    let server = support::Server::start(urls);
    let case = Case::new(name, Machine::arch().offering(Pkg::new("tree")));
    let core = case.root.path("dated-core");
    std::fs::create_dir_all(&core).unwrap();
    std::fs::write(core.join("core.db"), database).unwrap();
    case.archive(base, &core);
    case.archive(
        "https://archive.archlinux.org/repos/2026/09/01/extra/os/x86_64/",
        &archive_dir("extra"),
    );
    case.root.write(
        "etc/pacman.d/gnupg/pubring.kbx",
        &format!("{KEYRING_SIGNER}\n"),
    );
    case.root
        .write("etc/pacman.d/gnupg/trustdb.gpg", "trust values\n");
    case.edit(|m| m["keyring"] = true.into());
    let opt = if opt_in {
        "\n[packages.arch]\narchived_keyring = true\n"
    } else {
        ""
    };
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n{opt}\n\
         [packages.pin]\ntree = \"2026-09-01\"\n"
    ));
    (case, server)
}

/// Every file of the machine's own keyring, byte for byte.
fn host_keyring(case: &Case) -> BTreeMap<String, Vec<u8>> {
    let dir = case.root.path("etc/pacman.d/gnupg");
    std::fs::read_dir(&dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (
                entry.file_name().to_string_lossy().into_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect()
}

fn apply_pinned(case: &Case, server: &support::Server) -> std::process::Output {
    case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())])
}

/// #170, default: a pinned package whose signer the machine does not trust is refused, naming
/// the package, the pin date and the key, and offering the archived keyring; nothing changes.
#[test]
fn retired_signer_is_refused_by_default() {
    let (case, server) = keyring_case("arch-key-retired", SOUND, false);
    let trust = host_keyring(&case);
    let applied = apply_pinned(&case, &server);
    let error = fakehost::err(&applied);
    assert_eq!(applied.status.code(), Some(5), "{}", story(&applied));
    assert!(error.contains("E_PIN_UNTRUSTED"), "{error}");
    for fact in ["tree", "2026-09-01", TREE_SIGNER, "archived_keyring"] {
        assert!(error.contains(fact), "{fact}: {error}");
    }
    assert!(case.machine()["installed"].get("tree").is_none());
    assert!(
        case.log()
            .iter()
            .all(|line| !line.starts_with("pacman -U ") && !line.starts_with("pacman-key"))
    );
    assert!(
        !server
            .requests()
            .iter()
            .any(|url| url.contains("archlinux-keyring"))
    );
    assert_eq!(host_keyring(&case), trust);
    assert!(!case.root.exists("var/lib/lodi/host/pin/stage"));
}

/// #170, opt-in: the dated keyring, verified by the machine's trust, verifies the pin in a
/// private keyring that only pin.conf names; the machine's keyring is untouched.
#[test]
fn archived_keyring_opt_in_installs_through_a_private_keyring() {
    let (case, server) = keyring_case("arch-key-archived", SOUND, true);
    let trust = host_keyring(&case);
    let applied = apply_pinned(&case, &server);
    assert_eq!(applied.status.code(), Some(0), "{}", story(&applied));
    assert_eq!(case.machine()["installed"]["tree"]["version"], "2.3.2-1");
    assert_eq!(host_keyring(&case), trust);
    assert!(
        server
            .requests()
            .iter()
            .any(|url| url.ends_with(&format!("{KEYRING}.sig")))
    );
    assert!(case.log().iter().any(|line| line.starts_with("pacman -U ")));
    assert!(!case.root.exists("var/lib/lodi/host/pin/stage"));
    let conf = case.root.path("var/lib/lodi/host/pin/pin.conf");
    assert!(
        !std::fs::read_to_string(conf)
            .unwrap_or_default()
            .contains("GPGDir")
    );
}

/// #170, opt-in failing: the dated keyring revokes the signer, so the pin is still refused,
/// naming the package, date and key; nothing is installed.
#[test]
fn archived_keyring_that_distrusts_the_signer_fails_clearly() {
    let keyring = Keyring {
        revoked: &[TREE_SIGNER],
        ..SOUND
    };
    let (case, server) = keyring_case("arch-key-distrust", keyring, true);
    let trust = host_keyring(&case);
    let applied = apply_pinned(&case, &server);
    let error = fakehost::err(&applied);
    assert_eq!(applied.status.code(), Some(5), "{}", story(&applied));
    assert!(error.contains("E_PIN_UNTRUSTED"), "{error}");
    for fact in ["tree", "2026-09-01", TREE_SIGNER] {
        assert!(error.contains(fact), "{fact}: {error}");
    }
    assert!(
        case.log()
            .iter()
            .any(|line| line.starts_with("pacman-key") && line.contains("--populate"))
    );
    assert!(case.machine()["installed"].get("tree").is_none());
    assert!(
        case.log()
            .iter()
            .all(|line| !line.starts_with("pacman -U "))
    );
    assert_eq!(host_keyring(&case), trust);
}

/// A validly signed keyring package whose signer the machine does not trust enables no key.
#[test]
fn archived_keyring_from_an_untrusted_signer_enables_no_key() {
    let (case, server) = keyring_case("arch-key-untrusted", SOUND, true);
    case.root.write("etc/pacman.d/gnupg/pubring.kbx", "");
    let trust = host_keyring(&case);
    let applied = apply_pinned(&case, &server);
    let error = fakehost::err(&applied);
    assert_eq!(applied.status.code(), Some(5), "{}", story(&applied));
    assert!(
        error.contains("E_PIN_UNTRUSTED") && error.contains("archlinux-keyring"),
        "{error}"
    );
    let log = case.log();
    assert!(
        log.iter()
            .any(|line| line.starts_with("pacman-key") && line.contains("--verify"))
    );
    assert!(
        log.iter()
            .all(|line| !line.contains("--populate") && !line.contains("--init"))
    );
    assert!(log.iter().all(|line| !line.starts_with("pacman -U ")));
    assert!(case.machine()["installed"].get("tree").is_none());
    assert_eq!(host_keyring(&case), trust);
}

/// Signatures stay required under the opt-in: neither an unsigned keyring package nor an
/// unsigned pinned package is used.
#[test]
fn archived_keyring_still_requires_both_signatures() {
    for (label, keyring) in [
        (
            "keyring",
            Keyring {
                keyring_sig: false,
                ..SOUND
            },
        ),
        (
            "package",
            Keyring {
                tree_sig: false,
                ..SOUND
            },
        ),
    ] {
        let (case, server) = keyring_case(&format!("arch-key-unsigned-{label}"), keyring, true);
        let trust = host_keyring(&case);
        let applied = apply_pinned(&case, &server);
        // The signature that did not answer is a fetch refusal, not a manifest or trust one.
        assert_eq!(
            applied.status.code(),
            Some(4),
            "{label}: {}",
            story(&applied)
        );
        assert!(
            fakehost::err(&applied).contains("E_REPO_UNREACHABLE"),
            "{label}: {}",
            story(&applied)
        );
        let wanted = if label == "keyring" {
            KEYRING
        } else {
            "tree-2.3.2-1"
        };
        assert!(
            server
                .requests()
                .iter()
                .any(|url| url.contains(wanted) && url.ends_with(".sig")),
            "{label}"
        );
        assert!(case.machine()["installed"].get("tree").is_none(), "{label}");
        assert!(
            case.log()
                .iter()
                .all(|line| !line.starts_with("pacman -U ")),
            "{label}"
        );
        assert_eq!(host_keyring(&case), trust, "{label}");
        if label == "keyring" {
            assert!(
                case.log()
                    .iter()
                    .all(|line| !line.starts_with("pacman-key")),
                "{label}"
            );
        }
    }
}

/// The opt-in is an Arch key: another distribution's table refuses it.
#[test]
fn archived_keyring_is_refused_outside_the_arch_table() {
    let case = Case::new("deb-archived-keyring", Machine::debian());
    case.set_manifest(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n[packages.debian]\narchived_keyring = true\n",
    );
    let plan = case.plan();
    assert_eq!(plan.status.code(), Some(3), "{}", story(&plan));
    assert!(
        fakehost::err(&plan).contains("archived_keyring"),
        "{}",
        story(&plan)
    );
}
