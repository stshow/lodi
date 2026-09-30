//! M-Pin core (LD-395): the host directory's `pins.lock`, `[host] snapshot` made live, and exact
//! per-entry pins on apt, through the dated archives.
//!
//! Every test is offline. A dated archive is served on loopback through `LODI_FETCH_REWRITE`
//! from `tests/fixtures/host/pin/archive/`, which `scripts/mpin-local.sh --fixtures` trimmed from
//! a measured fetch (each directory's `PROVENANCE` names the public URL, the instant and the
//! SHA-256 of the untrimmed bytes); the machine is the fake package manager of
//! `tests/support/fakepm.py` under a scratch `--root`. No real package manager runs, no host
//! verb reaches `/`, and nothing sleeps on a clock.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;

use lodi::diag::exit_status;
use lodi::hostscope::manifest::{Context, HostManifest, parse as parse_host};
use lodi::hostscope::pin::Request;
use lodi::manifest::ManifestErrors;

fn context(distro: &str) -> Context {
    let (release, codename) = match distro {
        "ubuntu" => ("24.04", "noble"),
        "arch" => ("rolling", ""),
        _ => ("12", "bookworm"),
    };
    Context {
        arch: "x86_64".into(),
        os: "linux".into(),
        distro: distro.into(),
        release: release.into(),
        codename: codename.into(),
        lodi_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn parse_on(distro: &str, text: &str) -> HostManifest {
    match parse_host(text, "host.toml", &context(distro)) {
        Ok(manifest) => manifest,
        Err(errors) => panic!("expected a valid manifest, got:\n{errors}"),
    }
}

fn parse(text: &str) -> HostManifest {
    parse_on("debian", text)
}

fn errors_on(distro: &str, text: &str) -> ManifestErrors {
    match parse_host(text, "host.toml", &context(distro)) {
        Ok(_) => panic!("expected a rejection, the manifest parsed:\n{text}"),
        Err(errors) => errors,
    }
}

/// The one diagnostic of `code` a rejection carries, with its exit status and a hint.
#[track_caller]
fn rejects_on(distro: &str, text: &str, code: &str, status: u8) -> lodi::diag::Diagnostic {
    let errors = errors_on(distro, text);
    let found = errors
        .diagnostics
        .iter()
        .find(|d| d.code == code)
        .unwrap_or_else(|| panic!("expected {code}, got {:?}:\n{errors}", errors.codes()))
        .clone();
    assert_eq!(exit_status(found.code), status, "exit status of {code}");
    assert!(
        found.notes.iter().any(|note| note.starts_with("hint: ")),
        "{code} carries no hint:\n{found}"
    );
    found
}

#[track_caller]
fn rejects(text: &str, code: &str, status: u8) -> lodi::diag::Diagnostic {
    rejects_on("debian", text, code, status)
}

const HOST: &str = "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n";

// ------------------------------------------------------------------ the manifest (§4) ---

#[test]
fn a_pin_table_is_a_key_of_packages_and_of_a_distro_table() {
    let manifest = parse(&format!(
        "{HOST}[packages]\ncommon = [\"bc\", \"curl\", \"git\"]\n\n\
         [packages.pin]\nbc = \"1.07.1-3\"\ncurl = \"2026-01-04\"\n\n\
         [packages.debian.pin]\ngit = \"2026-09-18T14:59:00Z\"\n"
    ));
    let pins = manifest.packages.pins("debian");
    assert_eq!(
        pins.iter()
            .map(|(name, pin)| (name.as_str(), pin.request.clone()))
            .collect::<Vec<_>>(),
        vec![
            ("bc", Request::Version("1.07.1-3".into())),
            ("curl", Request::Date(1_767_484_800)),
            ("git", Request::Date(1_789_740_000)),
        ],
        "a version, a day and an instant floored to the hour"
    );
    assert_eq!(pins["git"].requested, "2026-09-18T14:59:00Z", "as written");
}

#[test]
fn the_distro_pin_wins_over_the_packages_pin_on_its_distribution_only() {
    let text = format!(
        "{}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\nbc = \"1.0\"\n\n\
         [packages.debian.pin]\nbc = \"2.0\"\n\n[packages.ubuntu.pin]\nbc = \"3.0\"\n",
        "[host]\nversion = \"1\"\n\n"
    );
    let on_debian = parse_on("debian", &text).packages.pins("debian");
    assert_eq!(on_debian["bc"].request, Request::Version("2.0".into()));
    let on_ubuntu = parse_on("ubuntu", &text).packages.pins("ubuntu");
    assert_eq!(on_ubuntu["bc"].request, Request::Version("3.0".into()));
    let bare = "[host]\nversion = \"1\"\n\n[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\nbc = \"1.0\"\n";
    assert_eq!(
        parse_on("ubuntu", bare).packages.pins("ubuntu")["bc"].request,
        Request::Version("1.0".into())
    );
}

#[test]
fn a_pin_outside_the_package_list_is_e_unknown_package() {
    let d = rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\ncurl = \"1.0\"\n"),
        "E_UNKNOWN_PACKAGE",
        3,
    );
    assert!(d.message.contains("`curl`"), "{d}");
    rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.debian.pin]\ncurl = \"1.0\"\n"),
        "E_UNKNOWN_PACKAGE",
        3,
    );
}

#[test]
fn latest_is_e_unsupported_with_the_hint_that_floats_an_entry() {
    let d = rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\nbc = \"latest\"\n"),
        "E_UNSUPPORTED",
        3,
    );
    let rendered = d.to_string();
    // check-host-safety: refusal — the hint's text, which nothing runs.
    assert!(rendered.contains("lodi host unpin --all"), "{rendered}");
}

#[test]
fn an_empty_or_multi_line_pin_is_e_type_at_its_file_line_and_column() {
    for (value, column) in [("\"\"", 6), ("\"1.0\\n2.0\"", 6)] {
        let text = format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\nbc = {value}\n");
        let d = rejects(&text, "E_TYPE", 3);
        let at = d.location.as_ref().expect("a location");
        assert_eq!(
            (at.file.as_str(), at.line, at.column),
            ("host.toml", 9, column),
            "{d}"
        );
    }
}

#[test]
fn a_pin_value_that_is_not_a_string_is_e_type() {
    rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\nbc = 1\n"),
        "E_TYPE",
        3,
    );
}

#[test]
fn a_name_both_pinned_and_absent_is_e_attr_conflict() {
    let d = rejects(
        &format!(
            "{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.debian]\nremove = [\"bc\"]\n\
             absent = [\"bc\"]\n\n[packages.pin]\nbc = \"1.0\"\n"
        ),
        "E_ATTR_CONFLICT",
        3,
    );
    assert!(d.message.contains("`bc`"), "{d}");
}

#[test]
fn a_pin_beside_a_hold_is_accepted() {
    let manifest = parse(&format!(
        "{HOST}[packages]\ncommon = [\"bc\"]\nhold = [\"bc\"]\n\n[packages.pin]\nbc = \"1.0\"\n"
    ));
    assert_eq!(manifest.packages.hold, ["bc"]);
    assert_eq!(manifest.packages.pins("debian").len(), 1);
}

#[test]
fn a_version_qualified_name_stays_e_unsupported_and_its_hint_names_packages_pin() {
    let d = rejects(
        &format!("{HOST}[packages]\ncommon = [\"git=1:2.39\"]\n"),
        "E_UNSUPPORTED",
        3,
    );
    assert!(d.to_string().contains("[packages.pin]"), "{d}");
}

#[test]
fn the_snapshot_keeps_its_grammar_and_its_hint_no_longer_says_nothing_reads_it() {
    let d = rejects(
        "[host]\nversion = \"1\"\nsnapshot = \"2026-09-18\"\n",
        "E_TYPE",
        3,
    );
    let rendered = d.to_string();
    assert!(
        !rendered.contains("nothing in this build reads it"),
        "{rendered}"
    );
    assert!(rendered.contains("YYYY-MM-DDTHH:MM:SSZ"), "{rendered}");
    assert!(rendered.contains("live archive"), "{rendered}");
    let manifest = parse("[host]\nversion = \"1\"\nsnapshot = \"2026-09-18T14:00:00Z\"\n");
    assert_eq!(
        manifest.host.snapshot.as_deref(),
        Some("2026-09-18T14:00:00Z")
    );
}

#[test]
fn a_pin_key_that_is_not_a_package_name_is_e_ident() {
    rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.pin]\n\"-bc\" = \"1.0\"\n"),
        "E_IDENT",
        3,
    );
}

// Arch pin parsing and materialisation are tested in host_pin_arch.rs (LD-396).

#[test]
fn a_pin_in_an_architecture_table_is_e_unsupported() {
    rejects(
        &format!("{HOST}[packages]\ncommon = [\"bc\"]\n\n[packages.x86_64.pin]\nbc = \"1.0\"\n"),
        "E_UNSUPPORTED",
        3,
    );
}

#[allow(dead_code)]
fn unused(_: BTreeMap<(), ()>) {}

// -------------------------------------------------------------- the pin lock (§4, P13) ---

use lodi::fetch::{HttpFetcher, parse_rewrites};
use lodi::hostscope::pin::{self, Declared, PinRecord, PinsLock, SnapshotRecord};
use lodi::hostscope::safety::Distro;
use std::path::{Path, PathBuf};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every recorded archive file under `tests/fixtures/host/pin/archive/<host>/<path>`, by the
/// public URL it was trimmed from, and every recorded answer of the per-package interface.
fn archive_files() -> BTreeMap<String, Vec<u8>> {
    fn walk(base: &Path, dir: &Path, out: &mut BTreeMap<String, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).expect("a fixture directory") {
            let path = entry.expect("an entry").path();
            if path.is_dir() {
                walk(base, &path, out);
            } else if path.file_name().is_some_and(|n| n != "PROVENANCE") {
                let rel = path.strip_prefix(base).unwrap().display().to_string();
                out.insert(format!("https://{rel}"), std::fs::read(&path).unwrap());
            }
        }
    }
    let fixtures = repo().join("tests/fixtures/host/pin");
    let mut out = BTreeMap::new();
    let base = fixtures.join("archive");
    walk(&base, &base, &mut out);
    let interface = fixtures.join("interface");
    let index: BTreeMap<String, String> = serde_json::from_slice(
        &std::fs::read(interface.join("index.json")).expect("the interface index"),
    )
    .expect("the interface index parses");
    for (url, file) in index {
        out.insert(url, std::fs::read(interface.join(file)).unwrap());
    }
    out
}

/// A counting loopback archive: the recorded fixtures, reached through `LODI_FETCH_REWRITE`.
fn archive() -> support::Server {
    support::Server::start(archive_files())
}

fn fetcher_of(server: &support::Server) -> HttpFetcher {
    HttpFetcher::new(parse_rewrites(&server.rewrite()).expect("the rewrite parses"))
}

const OLDER: &str = "2025-03-01T00:00:00Z";
const RECENT: &str = "2026-09-01T00:00:00Z";
/// A clock after every recorded instant, so no fixture instant is in the future.
const NOW: i64 = 1_790_000_000;

fn declared(pairs: &[(&str, &str)]) -> BTreeMap<String, Declared> {
    pairs
        .iter()
        .map(|(name, value)| {
            (
                name.to_string(),
                Declared {
                    requested: value.to_string(),
                    request: pin::parse_value(name, value).expect("a pin"),
                },
            )
        })
        .collect()
}

fn no_sources() -> BTreeMap<String, lodi::hostscope::manifest::SourceEntry> {
    BTreeMap::new()
}

/// The lock the recorded Debian archives resolve these requests to, through `server`.
fn resolved_debian_lock(server: &support::Server) -> PinsLock {
    let sources = no_sources();
    let cx = pin::Context {
        distro: Distro::Debian,
        codename: "bookworm",
        arch: "x86_64",
        sources: &sources,
        now: NOW,
    };
    let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
        Ok(Box::new(fetcher_of(server)))
    };
    let resolution = pin::resolve(
        &cx,
        Some(RECENT),
        &declared(&[("tzdata", "2025-03-01"), ("bc", RECENT)]),
        None,
        &mut fetch,
    )
    .expect("the pins resolve");
    let mut lock = resolution.lock("debian", "bookworm", "x86_64");
    let snapshot = lock.snapshot.as_mut().expect("the file-level snapshot");
    snapshot.indexes = pin::index_digests(
        &fetcher_of(server),
        Distro::Debian,
        "bookworm",
        lodi::util::parse_utc(RECENT).unwrap(),
    )
    .expect("the dated Release files");
    lock
}

/// The child half of [`pins_lock_is_byte_identical_from_two_processes`]: resolve, render, print.
#[test]
fn child_renders_a_pins_lock() {
    if !hostroot::is_child() {
        return;
    }
    let server = archive();
    let lock = resolved_debian_lock(&server);
    print!("<<<PINS\n{}PINS>>>\n", pin::render_lock(&lock));
}

#[test]
fn pins_lock_is_byte_identical_from_two_processes() {
    let rendered: Vec<String> = (0..2)
        .map(|_| hostroot::spawn("child_renders_a_pins_lock", &[]))
        .collect::<Vec<_>>()
        .into_iter()
        .map(|child| {
            let output = child.wait_with_output().expect("the child ends");
            assert!(output.status.success(), "{}", fakehost::story(&output));
            let text = String::from_utf8_lossy(&output.stdout).into_owned();
            let start = text.find("<<<PINS\n").expect("the rendered lock") + 8;
            let end = text.find("PINS>>>").expect("its end");
            text[start..end].to_string()
        })
        .collect();
    assert!(!rendered[0].is_empty(), "the child rendered nothing");
    assert!(rendered[0].contains("\"tzdata\""), "{}", rendered[0]);
    assert_eq!(rendered[0], rendered[1], "two processes, one byte sequence");
}

#[test]
fn pins_lock_bytes_are_sorted_keys_two_space_lf_and_one_trailing_newline() {
    let mut lock = PinsLock::new("debian", "bookworm", "x86_64");
    for name in ["zsh", "bc"] {
        lock.pins.insert(
            name.to_string(),
            PinRecord {
                policy: "version".into(),
                requested: "1.0".into(),
                snapshot: Some(OLDER.into()),
                version: "1.0".into(),
                sha256: format!("sha256:{}", "a".repeat(64)),
                filename: format!("pool/main/{name}.deb"),
                repository: "debian".into(),
                epoch: None,
                release: None,
                arch: None,
                source: None,
            },
        );
    }
    lock.snapshot = Some(SnapshotRecord {
        requested: RECENT.into(),
        instant: RECENT.into(),
        indexes: BTreeMap::from([(
            "debian/dists/bookworm/Release".to_string(),
            format!("sha256:{}", "b".repeat(64)),
        )]),
    });
    let text = pin::render_lock(&lock);
    let expected = format!(
        "{{\n  \"arch\": \"x86_64\",\n  \"distro\": \"debian\",\n  \"format\": \"lodi-host-pins/1\",\n  \
         \"pins\": {{\n    \"bc\": {{\n      \"filename\": \"pool/main/bc.deb\",\n      \"policy\": \
         \"version\",\n      \"repository\": \"debian\",\n      \"requested\": \"1.0\",\n      \
         \"sha256\": \"sha256:{a}\",\n      \"snapshot\": \"{OLDER}\",\n      \"version\": \"1.0\"\n    \
         }},\n    \"zsh\": {{\n      \"filename\": \"pool/main/zsh.deb\",\n      \"policy\": \
         \"version\",\n      \"repository\": \"debian\",\n      \"requested\": \"1.0\",\n      \
         \"sha256\": \"sha256:{a}\",\n      \"snapshot\": \"{OLDER}\",\n      \"version\": \"1.0\"\n    \
         }}\n  }},\n  \"release\": \"bookworm\",\n  \"snapshot\": {{\n    \"indexes\": {{\n      \
         \"debian/dists/bookworm/Release\": \"sha256:{b}\"\n    }},\n    \"instant\": \"{RECENT}\",\n    \
         \"requested\": \"{RECENT}\"\n  }},\n  \"version\": 1\n}}\n",
        a = "a".repeat(64),
        b = "b".repeat(64)
    );
    assert_eq!(text, expected);
    assert_eq!(
        pin::parse_lock(text.as_bytes(), "pins.lock").expect("it reads back"),
        lock
    );
}

#[test]
fn a_pins_lock_holds_no_machine_name_user_path_binary_version_or_wall_clock_time() {
    let server = archive();
    let text = pin::render_lock(&resolved_debian_lock(&server));
    assert!(!text.is_empty());
    assert!(!text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    assert!(
        !text.contains("generatedBy") && !text.contains("lodiVersion"),
        "{text}"
    );
    for forbidden in [
        std::env::var("HOME").unwrap_or_default(),
        std::env::var("USER").unwrap_or_default(),
        std::fs::read_to_string("/etc/hostname")
            .unwrap_or_default()
            .trim()
            .to_string(),
        repo().display().to_string(),
    ] {
        if forbidden.len() > 2 {
            assert!(!text.contains(&forbidden), "{forbidden:?} in:\n{text}");
        }
    }
    // The only instants are archive instants: the ones requested.
    let mut instants: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    for at in 0..bytes.len().saturating_sub(20) {
        if bytes[at + 4] == b'-'
            && bytes[at + 10] == b'T'
            && bytes[at + 19] == b'Z'
            && let Ok(found) = std::str::from_utf8(&bytes[at..at + 20])
            && lodi::util::parse_utc(found).is_some()
        {
            instants.push(found);
        }
    }
    assert!(!instants.is_empty());
    for instant in instants {
        assert!(
            instant == OLDER || instant == RECENT,
            "{instant} is not an archive instant it was asked for"
        );
    }
    assert!(!text.contains(" /"), "an absolute path in:\n{text}");
}

#[test]
fn a_later_pins_lock_format_is_e_lock_version_naming_the_file() {
    let later = br#"{"format": "lodi-host-pins/2", "version": 2, "distro": "debian", "release": "bookworm", "arch": "x86_64", "pins": {}}"#;
    let d = pin::parse_lock(later, "/hosts/box/pins.lock").unwrap_err();
    assert_eq!(d.code, "E_LOCK_VERSION");
    assert_eq!(exit_status(d.code), 4);
    assert!(d.message.contains("/hosts/box/pins.lock"), "{d}");
    let unknown = br#"{"format": "lodi-host-pins/1", "version": 1, "distro": "debian", "release": "bookworm", "arch": "x86_64", "pins": {}, "extra": 1}"#;
    assert_eq!(
        pin::parse_lock(unknown, "pins.lock").unwrap_err().code,
        "E_LOCK_VERSION",
        "deny_unknown_fields"
    );
}

// ----------------------------------------------------------------- resolution (P2, P5) ---

fn resolve_on(
    server: &support::Server,
    distro: Distro,
    snapshot: Option<&str>,
    pins: &[(&str, &str)],
    lock: Option<&PinsLock>,
    sources: &BTreeMap<String, lodi::hostscope::manifest::SourceEntry>,
) -> Result<pin::Resolution, lodi::diag::Diagnostic> {
    let codename = if distro == Distro::Ubuntu {
        "noble"
    } else {
        "bookworm"
    };
    let cx = pin::Context {
        distro,
        codename,
        arch: "x86_64",
        sources,
        now: NOW,
    };
    let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
        Ok(Box::new(fetcher_of(server)))
    };
    pin::resolve(&cx, snapshot, &declared(pins), lock, &mut fetch)
}

#[test]
fn a_date_pin_reads_its_version_and_digest_from_the_dated_release_and_packages() {
    let server = archive();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("tzdata", "2025-03-01")],
        None,
        &no_sources(),
    )
    .expect("it resolves");
    let tzdata = &resolution.pins["tzdata"];
    assert_eq!(tzdata.version, "2024b-0+deb12u1");
    assert_eq!(tzdata.repository, "debian");
    assert_eq!(tzdata.snapshot, lodi::util::parse_utc(OLDER));
    assert!(tzdata.sha256.starts_with("sha256:") && tzdata.sha256.len() == 71);
    assert!(
        tzdata.filename.starts_with("pool/main/t/tzdata/"),
        "{tzdata:?}"
    );
    assert!(!tzdata.recorded);
    let requests = server.requests();
    assert!(
        requests
            .iter()
            .any(|u| u.ends_with("/archive/debian/20250301T000000Z/dists/bookworm/Release")),
        "{requests:?}"
    );
    assert!(
        requests
            .iter()
            .all(|u| u.starts_with("https://snapshot.debian.org/archive/")),
        "only the catalogue's dated archive was asked: {requests:?}"
    );
}

#[test]
fn an_apt_instant_is_floored_to_the_hour() {
    let server = archive();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("tzdata", "2025-03-01T00:59:59Z")],
        None,
        &no_sources(),
    )
    .expect("it resolves");
    assert_eq!(
        resolution.pins["tzdata"].snapshot,
        lodi::util::parse_utc(OLDER),
        "the instant is the hour it falls in"
    );
    assert!(
        server.requests().iter().all(|u| !u.contains("T005959Z")),
        "{:?}",
        server.requests()
    );
}

#[test]
fn a_version_pin_on_debian_is_e_unsupported_naming_the_date_form() {
    let server = archive();
    let d = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("bc", "1.07.1-3+b1")],
        None,
        &no_sources(),
    )
    .unwrap_err();
    assert_eq!(d.code, "E_UNSUPPORTED", "{d}");
    assert!(d.to_string().contains("YYYY-MM-DD"), "{d}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
fn a_version_pin_on_ubuntu_resolves_through_the_per_package_interface() {
    let server = archive();
    let resolution = resolve_on(
        &server,
        Distro::Ubuntu,
        None,
        &[("tzdata", "2024b-0ubuntu0.24.04.1")],
        None,
        &no_sources(),
    )
    .expect("it resolves");
    let tzdata = &resolution.pins["tzdata"];
    assert_eq!(tzdata.version, "2024b-0ubuntu0.24.04.1");
    assert_eq!(tzdata.policy, "version");
    assert_eq!(tzdata.repository, "ubuntu");
    assert_eq!(
        tzdata.snapshot,
        lodi::util::parse_utc("2025-02-05T19:00:00Z")
    );
    let files: serde_json::Value = serde_json::from_slice(
        &std::fs::read(repo().join("tests/fixtures/host/pin/interface/files.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        tzdata.sha256,
        format!("sha256:{}", files[0]["sha256"].as_str().unwrap()),
        "the dated index and the interface agree"
    );
}

#[test]
fn an_unknown_version_is_e_no_match_naming_the_nearest_versions() {
    let server = archive();
    let d = resolve_on(
        &server,
        Distro::Ubuntu,
        None,
        &[("tzdata", "2024b-0ubuntu0.24.04.9")],
        None,
        &no_sources(),
    )
    .unwrap_err();
    assert_eq!(d.code, "E_NO_MATCH", "{d}");
    assert_eq!(exit_status(d.code), 4);
    assert!(d.to_string().contains("2024b-0ubuntu0.24.04.1"), "{d}");
}

/// The recorded Debian archive at `instant`, with one `Packages.xz` replaced by `packages` and
/// its `Release` rewritten to vouch for it: an attack on a recorded fixture, made in the test.
fn with_packages(
    files: &mut BTreeMap<String, Vec<u8>>,
    instant: &str,
    suite: &str,
    packages: &str,
    vouch: bool,
) {
    let id = instant.replace(['-', ':'], "");
    let base = format!("https://snapshot.debian.org/archive/debian/{id}/dists/{suite}/");
    let packed = xz(packages.as_bytes());
    let release = String::from_utf8(files[&format!("{base}Release")].clone()).unwrap();
    let mut kept: Vec<String> = release
        .lines()
        .take_while(|line| *line != "SHA256:")
        .map(str::to_string)
        .collect();
    if vouch {
        kept.push("SHA256:".into());
        kept.push(format!(
            " {} {} main/binary-amd64/Packages.xz",
            lodi::util::sha256_hex(&packed),
            packed.len()
        ));
        files.insert(
            format!("{base}Release"),
            (kept.join("\n") + "\n").into_bytes(),
        );
    }
    files.insert(format!("{base}main/binary-amd64/Packages.xz"), packed);
}

fn xz(bytes: &[u8]) -> Vec<u8> {
    let mut child = std::process::Command::new("xz")
        .args(["-c", "-z"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("xz runs");
    use std::io::Write;
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    child.wait_with_output().unwrap().stdout
}

#[test]
fn a_version_served_with_no_digest_is_e_no_checksum() {
    let mut files = archive_files();
    with_packages(
        &mut files,
        OLDER,
        "bookworm-updates",
        "Package: tzdata\nVersion: 2024b-0+deb12u9\nArchitecture: all\n\
         Filename: pool/main/t/tzdata/tzdata_2024b-0+deb12u9_all.deb\nSize: 1\n",
        true,
    );
    let server = support::Server::start(files);
    let d = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("tzdata", "2025-03-01")],
        None,
        &no_sources(),
    )
    .unwrap_err();
    assert_eq!(d.code, "E_NO_CHECKSUM", "{d}");
    assert_eq!(exit_status(d.code), 5);
}

#[test]
fn an_index_that_does_not_match_its_release_is_e_hash_mismatch_with_nothing_stored() {
    let mut files = archive_files();
    with_packages(
        &mut files,
        OLDER,
        "bookworm-updates",
        "Package: tzdata\nVersion: 9\n",
        false,
    );
    let server = support::Server::start(files);
    let d = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("tzdata", "2025-03-01")],
        None,
        &no_sources(),
    )
    .unwrap_err();
    assert_eq!(d.code, "E_HASH_MISMATCH", "{d}");
    assert_eq!(exit_status(d.code), 5);
}

#[test]
fn a_snapshot_before_snapshot_min_or_after_now_is_refused_with_zero_requests() {
    let server = archive();
    for (snapshot, pins, code) in [
        (Some("2023-06-09T00:00:00Z"), vec![], "E_SNAPSHOT_TOO_OLD"),
        (Some("2030-01-01T00:00:00Z"), vec![], "E_SNAPSHOT_FUTURE"),
        (None, vec![("tzdata", "2020-01-01")], "E_SNAPSHOT_TOO_OLD"),
        (None, vec![("tzdata", "2030-01-01")], "E_SNAPSHOT_FUTURE"),
    ] {
        let d = resolve_on(
            &server,
            Distro::Debian,
            snapshot,
            &pins,
            None,
            &no_sources(),
        )
        .unwrap_err();
        assert_eq!(d.code, code, "{snapshot:?} {pins:?}: {d}");
        assert_eq!(exit_status(d.code), 4);
    }
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
fn a_recorded_pin_makes_zero_requests() {
    let server = archive();
    let lock = resolved_debian_lock(&server);
    server.clear();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        Some(RECENT),
        &[("tzdata", "2025-03-01"), ("bc", RECENT)],
        Some(&lock),
        &no_sources(),
    )
    .expect("it resolves from the record");
    assert!(server.requests().is_empty(), "{:?}", server.requests());
    assert!(resolution.pins.values().all(|p| p.recorded));
    assert!(resolution.snapshot.as_ref().is_some_and(|s| s.recorded));
    assert_eq!(resolution.lock("debian", "bookworm", "x86_64"), lock);
}

#[test]
fn a_record_whose_requested_value_no_longer_matches_counts_as_no_record() {
    let server = archive();
    let lock = resolved_debian_lock(&server);
    server.clear();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        Some(RECENT),
        &[("tzdata", "2026-09-01"), ("bc", RECENT)],
        Some(&lock),
        &no_sources(),
    )
    .expect("it resolves again");
    assert!(!resolution.pins["tzdata"].recorded);
    assert_eq!(resolution.pins["tzdata"].version, "2026b-0+deb12u1");
    assert!(resolution.pins["bc"].recorded);
    assert!(!server.requests().is_empty());
    let edited = resolve_on(
        &server,
        Distro::Debian,
        Some(OLDER),
        &[("bc", RECENT)],
        Some(&lock),
        &no_sources(),
    )
    .expect("an edited snapshot");
    assert!(
        !edited.snapshot.as_ref().unwrap().recorded,
        "the snapshot was edited"
    );
}

#[test]
fn a_record_no_request_names_is_ignored_and_the_next_lock_drops_it() {
    let server = archive();
    let lock = resolved_debian_lock(&server);
    server.clear();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        Some(RECENT),
        &[("bc", RECENT)],
        Some(&lock),
        &no_sources(),
    )
    .expect("it resolves");
    assert!(server.requests().is_empty());
    assert!(!resolution.pins.contains_key("tzdata"));
    assert!(
        !resolution
            .lock("debian", "bookworm", "x86_64")
            .pins
            .contains_key("tzdata")
    );
}

#[test]
fn the_pin_module_holds_no_url() {
    let text = std::fs::read_to_string(repo().join("src/hostscope/pin.rs")).unwrap();
    assert!(
        !text.contains("https://"),
        "every URL of the pin module comes from the catalogue or the manifest"
    );
    assert!(!text.contains("http://"));
}

fn docker_source() -> BTreeMap<String, lodi::hostscope::manifest::SourceEntry> {
    let manifest = parse_on(
        "debian",
        "[host]\nversion = \"1\"\n\n[packages]\ncommon = [\"docker-compose-plugin\"]\n\n\
         [sources.docker]\nuris = [\"https://download.docker.com/linux/debian\"]\n\
         suites = [\"bookworm\"]\ncomponents = [\"stable\"]\nsigned_by = \"docker.gpg\"\n\
         signed_by_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n",
    );
    manifest.sources
}

#[test]
fn a_sources_package_pinned_by_version_resolves_against_that_sources_current_index() {
    let server = archive();
    let listed = String::from_utf8(
        archive_files()["https://download.docker.com/linux/debian/dists/bookworm/stable/binary-amd64/Packages"]
            .clone(),
    )
    .unwrap();
    let version = listed
        .lines()
        .find_map(|line| line.strip_prefix("Version: "))
        .expect("a recorded version")
        .to_string();
    let resolution = resolve_on(
        &server,
        Distro::Debian,
        Some(RECENT),
        &[("docker-compose-plugin", &version)],
        None,
        &docker_source(),
    )
    .expect("it resolves against the source");
    let pinned = &resolution.pins["docker-compose-plugin"];
    assert_eq!(pinned.repository, "docker");
    assert_eq!(
        pinned.snapshot, None,
        "a declared source has no dated archive"
    );
    assert_eq!(pinned.version, version);
    assert!(
        server
            .requests()
            .iter()
            .all(|u| u.starts_with("https://download.docker.com/")),
        "the file-level snapshot does not reach it: {:?}",
        server.requests()
    );
}

#[test]
fn a_date_pin_on_a_sources_package_is_e_unsupported() {
    let server = archive();
    let d = resolve_on(
        &server,
        Distro::Debian,
        None,
        &[("docker-compose-plugin", "2026-01-04")],
        None,
        &docker_source(),
    )
    .unwrap_err();
    assert_eq!(d.code, "E_UNSUPPORTED", "{d}");
    assert!(d.message.contains("docker"), "{d}");
}

// ------------------------------------------------------ plan and apply (§3, P4, P6–P9) ---

use fakehost::{Case, Machine, Pkg, err, out, story};
use lodi::hostscope::plan::{PRIVATE_NOTE, SOURCES_NOTE, SWEEP_LINE};

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// The machine's own apt sources: the distribution's two archives, with a component the
/// catalogue does not name, and a vendor's repository.
const MACHINE_SOURCES: &str = "Types: deb\nURIs: http://deb.debian.org/debian\n\
    Suites: bookworm bookworm-updates\nComponents: main non-free-firmware\n\
    Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\n\
    Types: deb\nURIs: http://deb.debian.org/debian-security\nSuites: bookworm-security\n\
    Components: main non-free-firmware\n\
    Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n";
const VENDOR_SOURCES: &str = "Types: deb\nURIs: https://download.docker.com/linux/debian\n\
    Suites: bookworm\nComponents: stable\nSigned-By: /etc/apt/keyrings/docker.gpg\n";

fn pkg(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = version.to_string();
    pkg
}

/// A Debian machine behind the fake, its own sources written, every recorded dated archive
/// served to its private updates, and the loopback archive the product fetches through.
struct PinCase {
    case: Case,
    server: support::Server,
}

impl PinCase {
    fn new(name: &str, machine: Machine) -> PinCase {
        let case = Case::new(name, machine);
        case.root
            .write("etc/apt/sources.list.d/debian.sources", MACHINE_SOURCES);
        case.root
            .write("etc/apt/sources.list.d/vendor.sources", VENDOR_SOURCES);
        let fixtures = repo().join("tests/fixtures/host/pin/archive");
        for repository in ["debian", "debian-security"] {
            let dir = fixtures
                .join("snapshot.debian.org/archive")
                .join(repository);
            for id in std::fs::read_dir(&dir).unwrap().flatten() {
                let id = id.file_name().to_string_lossy().into_owned();
                case.archive(
                    &format!("https://snapshot.debian.org/archive/{repository}/{id}/"),
                    &dir.join(&id),
                );
            }
        }
        case.archive(
            "https://download.docker.com/linux/debian",
            &fixtures.join("download.docker.com/linux/debian"),
        );
        PinCase {
            case,
            server: archive(),
        }
    }

    fn verb(&self, verb: &str, extra: &[&str]) -> std::process::Output {
        let rewrite = self.server.rewrite();
        self.case
            .verb_env(verb, extra, &[("LODI_FETCH_REWRITE", rewrite.as_str())])
    }

    fn plan(&self) -> std::process::Output {
        self.verb("plan", &[])
    }

    fn apply(&self, extra: &[&str]) -> std::process::Output {
        self.verb("apply", extra)
    }

    fn manifest(&self, text: &str) {
        self.case.set_manifest(text);
    }

    /// `pins.lock` beside the in-place manifest, rendered by the pure renderer from what these
    /// requests resolve to: what M-Pin's verbs write.
    fn record(&self, snapshot: Option<&str>, pins: &[(&str, &str)]) -> PinsLock {
        let sources = no_sources();
        let cx = pin::Context {
            distro: Distro::Debian,
            codename: "bookworm",
            arch: "x86_64",
            sources: &sources,
            now: NOW,
        };
        let mut fetch = || -> Result<Box<dyn lodi::fetch::Fetcher>, lodi::diag::Diagnostic> {
            Ok(Box::new(fetcher_of(&self.server)))
        };
        let resolution =
            pin::resolve(&cx, snapshot, &declared(pins), None, &mut fetch).expect("it resolves");
        let mut lock = resolution.lock("debian", "bookworm", "x86_64");
        if let (Some(record), Some(requested)) = (lock.snapshot.as_mut(), snapshot) {
            record.indexes = pin::index_digests(
                &fetcher_of(&self.server),
                Distro::Debian,
                "bookworm",
                lodi::util::parse_utc(requested).unwrap(),
            )
            .unwrap();
        }
        self.write_lock(&lock);
        self.server.clear();
        lock
    }

    fn write_lock(&self, lock: &PinsLock) {
        self.case
            .root
            .write("etc/lodi/pins.lock", &pin::render_lock(lock));
    }

    fn installed(&self, name: &str) -> Option<String> {
        self.case.machine()["installed"][name]["version"]
            .as_str()
            .map(str::to_string)
    }

    fn held(&self, name: &str) -> bool {
        self.case.machine()["installed"][name]["held"] == serde_json::Value::Bool(true)
    }

    /// The argv the fake saw of `program SUBCOMMAND`, whole, without the root options and
    /// without a simulation: what ran.
    fn argv(&self, program_and_verb: &str) -> Vec<String> {
        let rooted = format!(" -o RootDir={}", self.case.root.dir.display());
        self.case
            .log()
            .into_iter()
            .map(|line| line.replace(&rooted, ""))
            .filter(|line| line.starts_with(program_and_verb) && !line.contains(" -s "))
            .collect()
    }

    /// The simulations the fake saw.
    fn simulations(&self) -> Vec<String> {
        self.case
            .log()
            .into_iter()
            .filter(|line| line.starts_with("apt-get install") && line.contains(" -s "))
            .collect()
    }
}

fn host(snapshot: Option<&str>, common: &[&str], pins: &[(&str, &str)]) -> String {
    let mut text =
        String::from("[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"managed\"\n");
    if let Some(snapshot) = snapshot {
        text.push_str(&format!("snapshot = \"{snapshot}\"\n"));
    }
    text.push_str(&format!(
        "\n[packages]\ncommon = [{}]\n",
        common
            .iter()
            .map(|n| format!("\"{n}\""))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    if !pins.is_empty() {
        text.push_str("\n[packages.pin]\n");
        for (name, value) in pins {
            text.push_str(&format!("{name} = \"{value}\"\n"));
        }
    }
    text
}

/// Kind, mode, size and nanosecond mtime of everything below `dir`, by path.
fn census(dir: &Path) -> BTreeMap<PathBuf, (bool, u32, u64, i128)> {
    fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (bool, u32, u64, i128)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            use std::os::unix::fs::MetadataExt;
            out.insert(
                path.clone(),
                (
                    meta.is_dir(),
                    meta.mode(),
                    if meta.is_dir() { 0 } else { meta.len() },
                    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec()),
                ),
            );
            if meta.is_dir() {
                walk(&path, out);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, &mut out);
    out
}

fn debian_with(installed: &[(&str, &str)]) -> Machine {
    let mut machine = Machine::debian();
    for (name, version) in installed {
        machine = machine.with(pkg(name, version));
    }
    machine
}

#[test]
fn plan_prints_each_pinned_names_resolved_version_and_origin_in_one_fixed_wording() {
    let pc = PinCase::new(
        "pin-plan-wording",
        debian_with(&[("bc", "1.07.1-3+b1"), ("tzdata", "2026b-0+deb12u1")]),
    );
    pc.manifest(&host(None, &["bc", "tzdata"], &[("tzdata", "2025-03-01")]));
    let plan = pc.plan();
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    let root = pc.case.root.dir.display();
    assert_eq!(
        out(&plan),
        format!(
            "{PRIVATE_NOTE}\n\
             ~ package tzdata (pinned 2024b-0+deb12u1 from debian at 2025-03-01T00:00:00Z; \
             downgrade from 2026b-0+deb12u1; not recorded in pins.lock, record it with: \
             lodi host pin tzdata --to 2025-03-01 --root {root})\n\
             ~ package tzdata (hold)\n\
             2 action(s)\n"
        )
    );
}

#[test]
fn an_unrecorded_pin_is_resolved_in_memory_and_the_plan_names_the_command_that_records_it() {
    let pc = PinCase::new(
        "pin-unrecorded",
        debian_with(&[("tzdata", "2024b-0+deb12u1")]),
    );
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let plan = pc.plan();
    let text = out(&plan);
    let root = pc.case.root.dir.display();
    assert!(
        text.contains(&format!(
            "= package tzdata (pinned 2024b-0+deb12u1 from debian at 2025-03-01T00:00:00Z; not \
             recorded in pins.lock, record it with: \
             lodi host pin tzdata --to 2025-03-01 --root {root})"
        )),
        "{}",
        story(&plan)
    );
    assert!(
        !pc.server.requests().is_empty(),
        "resolved in memory, through the fetch"
    );
    assert!(
        !pc.case.root.exists("etc/lodi/pins.lock"),
        "and never written"
    );
    // The command it names records exactly that pin, and the next plan finds it recorded.
    let recorded = pc.verb("pin", &["tzdata", "--to", "2025-03-01"]);
    assert_eq!(recorded.status.code(), Some(0), "{}", story(&recorded));
    assert!(pc.case.root.exists("etc/lodi/pins.lock"));
    let again = out(&pc.plan());
    assert!(
        again.contains(
            "= package tzdata (pinned 2024b-0+deb12u1 from debian at 2025-03-01T00:00:00Z)"
        ),
        "{again}"
    );
}

#[test]
fn a_file_level_snapshot_does_not_reach_a_declared_sources_package_and_the_plan_says_so() {
    let pc = PinCase::new("pin-sources-note", debian_with(&[("bc", "1.07.1-3+b1")]));
    // The declared source is this machine's only stanza for that repository.
    std::fs::remove_file(pc.case.root.path("etc/apt/sources.list.d/vendor.sources")).unwrap();
    let key = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\nbm90IGEga2V5\n=AAAA\n-----END PGP PUBLIC KEY BLOCK-----\n";
    pc.case.root.write("etc/lodi/files/docker.asc", key);
    pc.manifest(&format!(
        "{}\n[sources.docker]\nuris = [\"https://download.docker.com/linux/debian\"]\n\
         suites = [\"bookworm\"]\ncomponents = [\"stable\"]\nsigned_by = \"files/docker.asc\"\n\
         signed_by_sha256 = \"{}\"\n",
        host(Some(RECENT), &["bc"], &[]),
        lodi::util::sha256_hex(key.as_bytes())
    ));
    let plan = pc.plan();
    assert!(
        out(&plan).lines().any(|line| line == SOURCES_NOTE),
        "{}",
        story(&plan)
    );
}

#[test]
fn a_settled_machine_and_a_recorded_pin_it_matches_make_zero_requests() {
    let pc = PinCase::new(
        "pin-settled",
        debian_with(&[("bc", "1.07.1-3+b1"), ("tzdata", "2024b-0+deb12u1")]),
    );
    pc.record(Some(RECENT), &[("tzdata", "2025-03-01")]);
    pc.manifest(&host(
        Some(RECENT),
        &["bc", "tzdata"],
        &[("tzdata", "2025-03-01")],
    ));
    let first = pc.apply(&[]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    assert!(pc.held("tzdata"));
    pc.server.clear();
    let plan = pc.plan();
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert!(out(&plan).ends_with("nothing to do\n"), "{}", story(&plan));
    assert!(
        out(&plan).contains(
            "= package tzdata (pinned 2024b-0+deb12u1 from debian at 2025-03-01T00:00:00Z)\n"
        ),
        "{}",
        story(&plan)
    );
    assert!(
        pc.server.requests().is_empty(),
        "{:?}",
        pc.server.requests()
    );
}

#[test]
fn plan_writes_nothing() {
    let pc = PinCase::new("pin-plan-writes", debian_with(&[("bc", "1.07.1-3+b1")]));
    pc.manifest(
        &host(Some(RECENT), &["bc", "tree"], &[("tzdata", "2025-03-01")]).replace(
            "common = [\"bc\", \"tree\"]",
            "common = [\"bc\", \"tree\", \"tzdata\"]",
        ),
    );
    let before = census(&pc.case.root.dir);
    let plan = pc.plan();
    assert_eq!(plan.status.code(), Some(0), "{}", story(&plan));
    assert_eq!(census(&pc.case.root.dir), before, "{}", story(&plan));
}

#[test]
fn apply_installs_exactly_what_plan_resolved_without_resolving_again() {
    let pc = PinCase::new("pin-once", debian_with(&[]));
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    let requests = pc.server.requests();
    let mut seen = std::collections::BTreeSet::new();
    for url in &requests {
        assert!(
            seen.insert(url),
            "{url} was fetched twice in one apply: {requests:?}"
        );
    }
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));
}

#[test]
fn the_snapshot_makes_one_transaction_read_a_private_dated_source_set() {
    let pc = PinCase::new("pin-private-set", debian_with(&[("bc", "1.07.1-3+b1")]));
    pc.manifest(&host(Some(RECENT), &["bc", "tree"], &[]));
    let lists_before = census(&pc.case.root.path("var/lib/apt/lists"));
    let apt_before = census(&pc.case.root.path("etc/apt"));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(
        out(&apply).contains("+ package tree\n"),
        "{}",
        story(&apply)
    );
    let plan_note = lodi::hostscope::plan::private_note(lodi::util::parse_utc(RECENT));
    assert!(
        plan_note.contains("installs from the dated archive at 2026-09-01T00:00:00Z"),
        "{plan_note}"
    );
    // One private set, rendered by the one deb822 writer: a dated stanza per repository of the
    // base with the machine's own suites, components and keyring, and the vendor's as it is.
    assert_eq!(
        pc.case.private_saw(),
        vec![
            "# Written by lodi for one apply of a pinned host; removed when that apply ends.\n\
             Types: deb\nURIs: https://snapshot.debian.org/archive/debian/20260901T000000Z/\n\
             Suites: bookworm bookworm-updates\nComponents: main non-free-firmware\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n\n\
             Types: deb\nURIs: https://snapshot.debian.org/archive/debian-security/20260901T000000Z/\n\
             Suites: bookworm-security\nComponents: main non-free-firmware\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n\n\
             Types: deb\nURIs: https://download.docker.com/linux/debian\nSuites: bookworm\n\
             Components: stable\nSigned-By: /etc/apt/keyrings/docker.gpg\n"
                .to_string()
        ]
    );
    let install = pc.argv("apt-get install");
    assert_eq!(install.len(), 1, "one transaction: {install:?}");
    // The stage's paths are inside the root: apt's own `RootDir` prefixes them.
    let stage = pin::STAGE;
    for option in [
        format!("-o Dir::Etc::SourceList={stage}/sources.list"),
        format!("-o Dir::Etc::SourceParts={stage}/sources.list.d"),
        format!("-o Dir::State::Lists={stage}/lists"),
    ] {
        assert!(install[0].contains(&option), "{install:?}");
    }
    assert!(install[0].ends_with("-- tree"), "{install:?}");
    assert!(
        pc.argv("apt-get update")
            .iter()
            .all(|line| line.contains(&format!("Dir::State::Lists={stage}/lists"))),
        "the machine's own index is not refreshed"
    );
    // D-A: the installed package keeps its version, the missing one comes from the dated
    // archive, and nothing is held.
    assert_eq!(pc.installed("bc").as_deref(), Some("1.07.1-3+b1"));
    assert_eq!(pc.installed("tree").as_deref(), Some("2.1.0-1"));
    assert!(!pc.held("tree") && !pc.held("bc"));
    assert!(pc.argv("apt-mark hold").is_empty());
    // The census: nothing under /etc/apt or /var/lib/apt/lists changed, and the stage is gone.
    assert_eq!(census(&pc.case.root.path("etc/apt")), apt_before);
    assert_eq!(
        census(&pc.case.root.path("var/lib/apt/lists")),
        lists_before
    );
    assert!(!pc.case.root.exists("var/lib/lodi/host/pin/stage"));
}

/// Debian 12's cloud image reaches its two archives through local mirror lists
/// (`mirror+file:`), each naming the archive's own address — the supervisor's guest run of
/// 2026-09-24, where the snapshot's apply installed the live `git`. The image lists
/// `bookworm-backports` too, which the recorded archive does not carry, so it is left out here.
const MIRROR_SOURCES: &str = "Types: deb deb-src\n\
    URIs: mirror+file:///etc/apt/mirrors/debian.list\n\
    Suites: bookworm bookworm-updates\nComponents: main\n\
    Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\n\
    Types: deb deb-src\nURIs: mirror+file:///etc/apt/mirrors/debian-security.list\n\
    Suites: bookworm-security\nComponents: main\n\
    Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n";

#[test]
fn a_snapshot_replaces_a_base_archive_the_machine_reaches_through_a_local_mirror_list() {
    // The live archive offers a newer `tree` than the dated one: a stanza carried as it is would
    // let apt install that one.
    let pc = PinCase::new(
        "pin-mirror-list",
        debian_with(&[("bc", "1.07.1-3+b1")]).offering(pkg("tree", "2.1.1-1")),
    );
    pc.case
        .root
        .write("etc/apt/sources.list.d/debian.sources", MIRROR_SOURCES);
    pc.case.root.write(
        "etc/apt/mirrors/debian.list",
        "https://deb.debian.org/debian\n",
    );
    pc.case.root.write(
        "etc/apt/mirrors/debian-security.list",
        "# the image's own mirror\nhttps://deb.debian.org/debian-security\tpriority:1\n",
    );
    pc.manifest(&host(Some(RECENT), &["bc", "tree"], &[]));
    let apt_before = census(&pc.case.root.path("etc/apt"));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    // D-A: the missing package comes from the dated archive, not the live one.
    assert_eq!(pc.installed("tree").as_deref(), Some("2.1.0-1"));
    assert_eq!(pc.installed("bc").as_deref(), Some("1.07.1-3+b1"));
    // The dated stanzas take the place of the mirror-list stanzas, with the machine's own
    // suites, components and keyring; the vendor's is carried as it is.
    assert_eq!(
        pc.case.private_saw(),
        vec![
            "# Written by lodi for one apply of a pinned host; removed when that apply ends.\n\
             Types: deb\nURIs: https://snapshot.debian.org/archive/debian/20260901T000000Z/\n\
             Suites: bookworm bookworm-updates\nComponents: main\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n\n\
             Types: deb\nURIs: https://snapshot.debian.org/archive/debian-security/20260901T000000Z/\n\
             Suites: bookworm-security\nComponents: main\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\nCheck-Valid-Until: no\n\n\
             Types: deb\nURIs: https://download.docker.com/linux/debian\nSuites: bookworm\n\
             Components: stable\nSigned-By: /etc/apt/keyrings/docker.gpg\n"
                .to_string()
        ]
    );
    assert_eq!(census(&pc.case.root.path("etc/apt")), apt_before);
}

#[test]
fn sources_still_refuses_the_check_valid_until_key() {
    let errors = errors_on(
        "debian",
        "[host]\nversion = \"1\"\n\n[sources.x]\nuris = [\"https://example.invalid/x\"]\n\
         suites = [\"bookworm\"]\ncomponents = [\"main\"]\nsigned_by = \"k.gpg\"\n\
         signed_by_sha256 = \"0000000000000000000000000000000000000000000000000000000000000000\"\n\
         check_valid_until = false\n",
    );
    assert!(errors.codes().contains(&"E_UNKNOWN_ATTR"), "{errors}");
    let rendered = lodi::hostscope::sourceset::render(&lodi::hostscope::sourceset::Stanza {
        types: vec!["deb".into()],
        uris: vec!["https://example.invalid/x".into()],
        suites: vec!["bookworm".into()],
        components: vec!["main".into()],
        architectures: vec![],
        signed_by: "/etc/apt/keyrings/lodi-x.gpg".into(),
    });
    assert!(!rendered.contains("Check-Valid-Until"), "{rendered}");
}

#[test]
fn an_exact_pin_installs_name_equals_version_holds_it_and_records_the_hold() {
    let pc = PinCase::new("pin-exact", debian_with(&[]));
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    let install = pc.argv("apt-get install");
    assert!(
        install[0].ends_with("-- tzdata=2024b-0+deb12u1"),
        "{install:?}"
    );
    assert!(!install[0].contains("--allow-downgrades"), "{install:?}");
    assert_eq!(
        pc.argv("apt-mark hold"),
        vec!["apt-mark hold -- tzdata".to_string()]
    );
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));
    assert!(pc.held("tzdata"));
    let lock = pc.case.lock();
    assert_eq!(lock["packages"]["tzdata"]["version"], "2024b-0+deb12u1");
    assert_eq!(lock["packages"]["tzdata"]["held"], true);
    assert_eq!(lock["version"], 2, "the machine record stays schema 2");
    assert!(
        !pc.case.root.read("etc/lodi/host.lock").contains("snapshot"),
        "the machine record is not the pin"
    );
}

#[test]
fn allow_downgrades_is_passed_only_when_the_plan_names_a_downgrade() {
    let down = PinCase::new("pin-down", debian_with(&[("tzdata", "2026b-0+deb12u1")]));
    down.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let plan = down.plan();
    assert!(
        out(&plan).contains("downgrade from 2026b-0+deb12u1"),
        "{}",
        story(&plan)
    );
    let apply = down.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(down.argv("apt-get install")[0].contains("--allow-downgrades"));
    assert_eq!(down.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));

    let up = PinCase::new("pin-up", debian_with(&[("tzdata", "2024b-0+deb12u1")]));
    up.manifest(&host(None, &["tzdata"], &[("tzdata", "2026-09-01")]));
    let plan = up.plan();
    assert!(
        out(&plan).contains("upgrade from 2024b-0+deb12u1"),
        "{}",
        story(&plan)
    );
    let apply = up.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    let install = up.argv("apt-get install");
    assert!(!install[0].contains("--allow-downgrades"), "{install:?}");
    assert!(
        install[0].ends_with("-- tzdata=2026b-0+deb12u1"),
        "{install:?}"
    );
}

#[test]
fn a_read_back_that_does_not_match_is_e_closure_drift_with_the_machine_record_written() {
    let pc = PinCase::new("pin-readback", debian_with(&[]));
    pc.case
        .edit(|m| m["skew"] = serde_json::json!({"tzdata": "2024b-0+deb12u1+lodi1"}));
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(6), "{}", story(&apply));
    assert!(err(&apply).contains("E_CLOSURE_DRIFT"), "{}", story(&apply));
    assert!(
        err(&apply).contains("tzdata pinned 2024b-0+deb12u1, found 2024b-0+deb12u1+lodi1"),
        "{}",
        story(&apply)
    );
    assert_eq!(
        pc.case.lock()["packages"]["tzdata"]["version"],
        "2024b-0+deb12u1+lodi1",
        "the record says what the machine has"
    );
}

#[test]
fn a_pinned_version_moved_outside_lodi_is_drift_until_overwrite_drift_restores_it() {
    let pc = PinCase::new("pin-drift", debian_with(&[]));
    pc.record(None, &[("tzdata", "2025-03-01")]);
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    assert_eq!(pc.apply(&[]).status.code(), Some(0));
    pc.case
        .edit(|m| m["installed"]["tzdata"]["version"] = serde_json::json!("2026b-0+deb12u1"));
    let plan = pc.plan();
    assert!(
        err(&plan).contains("W_DRIFT: package tzdata is pinned to 2024b-0+deb12u1 and was changed to 2026b-0+deb12u1 outside lodi")
            || out(&plan).contains("W_DRIFT: package tzdata is pinned to 2024b-0+deb12u1 and was changed to 2026b-0+deb12u1 outside lodi"),
        "{}",
        story(&plan)
    );
    let declined = pc.apply(&[]);
    assert!(
        err(&declined).contains("E_DECLINED"),
        "{}",
        story(&declined)
    );
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2026b-0+deb12u1"));
    let restored = pc.apply(&["--overwrite-drift"]);
    assert_eq!(restored.status.code(), Some(0), "{}", story(&restored));
    let install = pc.argv("apt-get install");
    assert!(install[0].contains("--allow-downgrades"), "{install:?}");
    assert!(
        install[0].contains("--allow-change-held-packages"),
        "{install:?}"
    );
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));
}

#[test]
fn removing_a_pin_releases_its_hold_keeps_its_version_and_the_plan_says_so() {
    let pc = PinCase::new("pin-unpin", debian_with(&[]));
    pc.record(None, &[("tzdata", "2025-03-01")]);
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    assert_eq!(pc.apply(&[]).status.code(), Some(0));
    assert!(pc.held("tzdata"));
    pc.manifest(&host(None, &["tzdata"], &[]));
    let plan = pc.plan();
    assert!(
        out(&plan).contains(
            "~ package tzdata (unhold: no longer pinned; 2024b-0+deb12u1 stays installed)\n"
        ),
        "{}",
        story(&plan)
    );
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(!pc.held("tzdata"));
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));
    assert!(pc.argv("apt-get install").is_empty(), "no version moved");
}

#[test]
fn a_second_apply_of_an_unchanged_pinned_directory_writes_nothing_and_makes_zero_requests() {
    let pc = PinCase::new("pin-second", debian_with(&[("bc", "1.07.1-3+b1")]));
    pc.record(Some(RECENT), &[("tzdata", "2025-03-01")]);
    pc.manifest(&host(
        Some(RECENT),
        &["bc", "tree", "tzdata"],
        &[("tzdata", "2025-03-01")],
    ));
    let first = pc.apply(&[]);
    assert_eq!(first.status.code(), Some(0), "{}", story(&first));
    let lock = pc.case.root.read("etc/lodi/host.lock");
    let before = census(&pc.case.root.dir);
    pc.server.clear();
    let second = pc.apply(&[]);
    assert_eq!(second.status.code(), Some(0), "{}", story(&second));
    assert!(
        out(&second).ends_with("nothing to do\n"),
        "{}",
        story(&second)
    );
    assert_eq!(
        census(&pc.case.root.dir),
        before,
        "no byte and no mtime moved"
    );
    assert_eq!(pc.case.root.read("etc/lodi/host.lock"), lock);
    assert!(
        pc.server.requests().is_empty(),
        "{:?}",
        pc.server.requests()
    );
}

#[test]
fn apply_never_writes_the_host_directory() {
    let pc = PinCase::new("pin-hostdir", debian_with(&[]));
    pc.case.root.write("etc/hostname", "box\n");
    let dir = pc.case.root.path("hosts/box");
    pc.case.root.write(
        "hosts/box/host.toml",
        &host(
            Some(RECENT),
            &["tree", "tzdata"],
            &[("tzdata", "2025-03-01")],
        ),
    );
    pc.record(Some(RECENT), &[("tzdata", "2025-03-01")]);
    let lock = pc.case.root.read("etc/lodi/pins.lock");
    pc.case.root.write("hosts/box/pins.lock", &lock);
    let before = census(&dir);
    let hosts = pc.case.root.path("hosts").display().to_string();
    let apply = pc.verb("apply", &[&hosts]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert_eq!(pc.installed("tzdata").as_deref(), Some("2024b-0+deb12u1"));
    assert_eq!(
        census(&dir),
        before,
        "the host directory is read, never written"
    );
    assert!(
        pc.server.requests().is_empty(),
        "recorded in the host directory's pins.lock"
    );
}

#[test]
fn a_recorded_index_digest_that_differs_from_what_apt_downloaded_is_e_hash_mismatch() {
    let pc = PinCase::new("pin-digest", debian_with(&[]));
    let mut lock = pc.record(Some(RECENT), &[]);
    let indexes = &mut lock.snapshot.as_mut().unwrap().indexes;
    let first = indexes.keys().next().unwrap().clone();
    indexes.insert(first, format!("sha256:{}", "0".repeat(64)));
    pc.write_lock(&lock);
    pc.manifest(&host(Some(RECENT), &["tree"], &[]));
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(5), "{}", story(&apply));
    assert!(err(&apply).contains("E_HASH_MISMATCH"), "{}", story(&apply));
    assert_eq!(pc.installed("tree"), None, "nothing installed");
    assert!(pc.argv("apt-get install").is_empty(), "no transaction ran");
    assert!(
        hostroot::journals(&pc.case.root).is_empty(),
        "no journal was begun"
    );
}

#[test]
fn a_pin_the_index_cannot_satisfy_is_refused_by_the_preflight_before_the_journal() {
    let pc = PinCase::new(
        "pin-unsatisfiable",
        debian_with(&[("tzdata", "2026b-0+deb12u1")]),
    );
    let mut lock = pc.record(None, &[("tzdata", "2025-03-01")]);
    lock.pins.get_mut("tzdata").unwrap().version = "2024b-0+deb12u9".into();
    pc.write_lock(&lock);
    pc.manifest(&host(None, &["tzdata"], &[("tzdata", "2025-03-01")]));
    let before = pc.case.machine();
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(4), "{}", story(&apply));
    let text = err(&apply);
    assert!(text.contains("E_PIN_UNSATISFIABLE"), "{}", story(&apply));
    assert!(
        text.contains("tzdata") && text.contains("2024b-0+deb12u9"),
        "{text}"
    );
    assert!(
        text.contains("debian at 2025-03-01T00:00:00Z"),
        "what is missing: {text}"
    );
    assert!(
        hostroot::journals(&pc.case.root).is_empty(),
        "before the journal"
    );
    assert_eq!(pc.case.machine()["installed"], before["installed"]);
    assert!(!pc.case.root.exists("var/lib/lodi/host/pin/stage"));
    assert!(pc.argv("apt-get install").is_empty(), "no transaction ran");
    assert_eq!(pc.simulations().len(), 1, "the preflight simulated it once");
}

#[test]
fn a_stage_left_by_an_interrupted_apply_is_removed_and_journalled_at_the_next_apply() {
    let pc = PinCase::new("pin-sweep", debian_with(&[]));
    pc.manifest(&host(Some(RECENT), &["tree"], &[]));
    let hold = hostroot::Hold::at_open_of(&pc.case.root, "lodi-pin.sources");
    let rewrite = pc.server.rewrite();
    let path = pc.case.fake_path();
    let mut child = hold.spawn_with(
        &pc.case.root,
        &[
            ("PATH", path.as_str()),
            ("LODI_FETCH_REWRITE", rewrite.as_str()),
        ],
    );
    hold.wait_held();
    hostroot::kill(&mut child);
    assert!(
        pc.case
            .root
            .exists("var/lib/lodi/host/pin/stage/sources.list.d")
    );
    let plan = pc.plan();
    let text = out(&plan);
    let sweep = text.find(SWEEP_LINE).expect("the sweep is planned");
    assert!(
        sweep < text.find("+ package tree").unwrap(),
        "the sweep is the first action: {text}"
    );
    let apply = pc.apply(&[]);
    assert_eq!(apply.status.code(), Some(0), "{}", story(&apply));
    assert!(out(&apply).contains(SWEEP_LINE), "{}", story(&apply));
    let journal = hostroot::journals(&pc.case.root)
        .into_iter()
        .map(|path| std::fs::read_to_string(path).unwrap())
        .find(|text| text.contains("\"s0\""))
        .expect("a journal records the sweep");
    assert!(
        journal.contains(SWEEP_LINE.trim_start_matches("~ ")),
        "{journal}"
    );
    assert!(!pc.case.root.exists("var/lib/lodi/host/pin/stage"));
    assert_eq!(pc.installed("tree").as_deref(), Some("2.1.0-1"));
}

// ------------------------------------------------------------------- the import (P11) ---

#[test]
fn the_import_says_in_two_lines_what_the_snapshot_now_does_and_writes_no_pin() {
    use lodi::hostscope::import::emit::{IMPORT_MINIMUM, SNAPSHOT_COMMENT};
    assert_eq!(SNAPSHOT_COMMENT.lines().count(), 2, "{SNAPSHOT_COMMENT}");
    assert!(
        !SNAPSHOT_COMMENT.chars().any(|c| c.is_ascii_digit()) && !SNAPSHOT_COMMENT.contains('/'),
        "no date, version or path: {SNAPSHOT_COMMENT}"
    );
    assert!(SNAPSHOT_COMMENT.contains("installs a missing package from the dated archive"));
    assert!(SNAPSHOT_COMMENT.contains("live archive"));
    assert_eq!(
        IMPORT_MINIMUM, "1.4.0",
        "the release that honours the snapshot (LD-398)"
    );
    let fixtures = repo().join("tests/fixtures/host/import");
    let mut apt = 0;
    for distro in std::fs::read_dir(&fixtures).unwrap().flatten() {
        if !distro.path().is_dir() {
            continue;
        }
        let name = distro.file_name().to_string_lossy().into_owned();
        for scenario in std::fs::read_dir(distro.path()).unwrap().flatten() {
            let Ok(text) = std::fs::read_to_string(scenario.path().join("expected.toml")) else {
                continue;
            };
            assert!(
                !text.contains("[packages.pin]"),
                "{}",
                scenario.path().display()
            );
            if name != "arch" {
                apt += 1;
            }
            assert!(
                text.contains(&format!("{SNAPSHOT_COMMENT}snapshot = \"")),
                "{}:\n{text}",
                scenario.path().display()
            );
        }
    }
    assert!(apt >= 10, "every apt import fixture was looked at");
}

// ----------------------------------------------------- P3: an unpinned file is unchanged ---

#[path = "support/compat.rs"]
mod compat;

/// The release the goldens of `tests/fixtures/host/compat-1.3/` hold this build to, and the
/// version the recording binary reports itself as (its README names the commit).
const COMPAT_1_3: &str = "tests/fixtures/host/compat-1.3";
const COMPAT_RECORDED: &str = "1.3.0";

#[test]
fn compat_1_3_every_committed_host_manifest_unpinned_plans_simulates_and_applies_as_1_3_did() {
    let fixtures = repo().join("tests/fixtures/host/import");
    let mut dirs: Vec<PathBuf> = Vec::new();
    for distro in std::fs::read_dir(&fixtures).unwrap().flatten() {
        if !distro.path().is_dir() {
            continue;
        }
        for scenario in std::fs::read_dir(distro.path()).unwrap().flatten() {
            if scenario.path().join("expected.toml").is_file() {
                dirs.push(scenario.path());
            }
        }
    }
    dirs.sort();
    let mut walked = 0;
    for dir in dirs {
        let distro = dir
            .parent()
            .unwrap()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let scenario = dir.file_name().unwrap().to_string_lossy().into_owned();
        let label = format!("{distro}-{scenario}");
        let arch = distro == "arch";
        // 1.3's imported Arch bytes are frozen from the published tag: the current import's
        // comment and day have deliberately changed, but the unpinned 1.3 input did not.
        let text = std::fs::read_to_string(if arch {
            repo().join(COMPAT_1_3).join(format!("{label}.toml"))
        } else {
            dir.join("expected.toml")
        })
        .unwrap();
        let mut machine = if arch {
            Machine::arch()
        } else {
            Machine::debian()
        };
        for name in compat::declared_names(&text) {
            if !machine.installed.iter().any(|p| p.name == name) {
                machine = machine.offering(Pkg::new(&name).repo(Some(if arch {
                    "extra"
                } else {
                    "main"
                })));
            }
        }
        let case = Case::new(&format!("compat13-{label}"), machine);
        if let Some(release) = distro.strip_prefix("ubuntu-") {
            let codename = text
                .lines()
                .find_map(|line| line.strip_prefix("# distribution: ubuntu ("))
                .and_then(|rest| rest.strip_suffix(')'))
                .expect("an ubuntu codename");
            case.root.write(
                "etc/os-release",
                &format!(
                    "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"{release}\"\n\
                     VERSION_CODENAME={codename}\n"
                ),
            );
        }
        // Neither a snapshot, a pin nor a pins.lock: the file 1.3 applies, with the minimum a 1.3
        // import wrote (from 1.4.0 the import writes `>=1.4.0`, which 1.3 refuses, LD-398).
        // Nor the [system] table the import writes since si-1, which 1.3 did not know.
        let unpinned = compat::without_system(&fakehost::without_snapshot(&text)).replace(
            "min_lodi_version = \">=1.4.0\"\n",
            "min_lodi_version = \">=1.1.1\"\n",
        );
        assert!(!unpinned.contains("snapshot = ") && !unpinned.contains("[packages.pin]"));
        case.set_manifest(&unpinned);
        assert!(!case.root.exists("etc/lodi/pins.lock"));
        compat::check_masked(
            &repo().join(COMPAT_1_3),
            &label,
            &compat::walk(&case, COMPAT_RECORDED),
            COMPAT_RECORDED,
            compat::mask_manifest_digest,
        );
        walked += 1;
    }
    assert_eq!(walked, 23, "every committed host manifest is walked");
}

#[test]
fn compat_1_3_a_files_manifest_plans_and_applies_as_1_3_did() {
    let case = Case::new(
        "compat13-files",
        Machine::debian().offering(Pkg::new("tree").repo(Some("main"))),
    );
    let (uid, gid) = hostroot::ids(&case.root);
    case.root.write("etc/app.conf", "the machine's own\n");
    case.root.write("etc/lodi/files/app.conf", "declared\n");
    case.set_manifest(&format!(
        "[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"managed\"\n\n\
         [packages]\ncommon = [\"tree\"]\n\n\
         [files.\"/etc/motd\"]\ncontent = \"hello\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/app.conf\"]\nsource = \"files/app.conf\"\nmode = \"0600\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));
    compat::check(
        &repo().join(COMPAT_1_3),
        "files",
        &compat::walk(&case, COMPAT_RECORDED),
        COMPAT_RECORDED,
    );
}

/// The recorded `pins.lock` of `tests/fixtures/host/pin/pins.lock` is what the pure renderer
/// writes for these requests against the recorded archives: recorded only by
/// `LODI_RECORD_EXPECTED=1 cargo test --locked --test host_pin pins_lock_golden`, never by hand.
/// `tests/schema.rs` reads it back as the format's specimen.
#[test]
fn pins_lock_golden_is_what_the_renderer_writes() {
    let server = archive();
    let text = pin::render_lock(&resolved_debian_lock(&server));
    let golden = repo().join("tests/fixtures/host/pin/pins.lock");
    if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
        std::fs::write(&golden, &text).unwrap();
        return;
    }
    assert_eq!(
        std::fs::read_to_string(&golden).expect("record it with LODI_RECORD_EXPECTED=1"),
        text
    );
}
