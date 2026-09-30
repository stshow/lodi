//! M-0.5 T-4: `<config>/home.lock` uses the project lock schema, pins only `[tools]`, and a
//! locked stale request changes nothing. All roots are below `CARGO_TARGET_TMPDIR`.

mod support;

use std::ffi::OsString;
use std::fs;
use std::sync::Mutex;

use lodi::fetch::{FetchError, Fetcher};
use lodi::home::tools;
use lodi::lock::{self, HOME_LOCK_FILE, LOCK_VERSION};
use lodi::roots::Roots;
use lodi::util::sha256_hex;

#[derive(Default)]
struct MapFetcher {
    requests: Mutex<Vec<String>>,
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.requests.lock().unwrap().push(url.to_string());
        Err(FetchError::NotFound(url.to_string()))
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn roots(env: &support::HomeEnv) -> Roots {
    let home = OsString::from(env.home());
    let config = OsString::from(env.config());
    let store = OsString::from(env.data().join("lodi"));
    Roots::from_vars(
        Some(home.as_os_str()),
        Some(config.as_os_str()),
        None,
        Some(store.as_os_str()),
    )
    .unwrap()
}

fn inline(version: &str, body: &[u8]) -> String {
    format!(
        "[home]\nversion = \"1\"\n\n[tools.demo]\nversion = \"{version}\"\n\
         url = \"https://fixtures.test/demo-{version}\"\nsha256 = \"{}\"\nformat = \"binary\"\n",
        sha256_hex(body)
    )
}

fn write_manifest(roots: &Roots, text: &str) {
    fs::create_dir_all(roots.config()).unwrap();
    fs::write(roots.config().join(lock::HOME_MANIFEST_FILE), text).unwrap();
}

#[test]
fn resolution_writes_the_shared_canonical_schema_and_reuses_it_byte_for_byte() {
    let env = support::home_env("home-lock-canonical");
    let roots = roots(&env);
    let fetcher = MapFetcher::default();
    write_manifest(&roots, &inline("1.0.0", b"first\n"));

    let first = tools::resolve(&roots, &fetcher, 0, false)
        .unwrap()
        .expect("a tool set resolves");
    assert!(first.wrote_lock);
    assert_eq!(first.resolved, ["demo"]);
    let path = roots.config().join(HOME_LOCK_FILE);
    let bytes = fs::read(&path).unwrap();
    let parsed = lock::parse_lock(&bytes).unwrap();
    assert_eq!(parsed.version, LOCK_VERSION);
    assert_eq!(bytes, parsed.to_canonical_json().as_bytes());
    assert!(bytes.ends_with(b"\n"));

    let second = tools::resolve(&roots, &fetcher, 1, false)
        .unwrap()
        .expect("the same tool set stays resolved");
    assert!(!second.wrote_lock);
    assert!(second.resolved.is_empty());
    assert_eq!(fs::read(&path).unwrap(), bytes);
    assert!(
        fetcher.requests().is_empty(),
        "inline pins need no discovery"
    );
}

#[test]
fn files_do_not_participate_in_the_home_lock_identity() {
    let env = support::home_env("home-lock-files");
    let roots = roots(&env);
    let fetcher = MapFetcher::default();
    let tool = inline("1.0.0", b"first\n");
    write_manifest(
        &roots,
        &format!("{tool}\n[files.\"note\"]\ncontent = \"one\"\n"),
    );
    tools::resolve(&roots, &fetcher, 0, false).unwrap();
    let path = roots.config().join(HOME_LOCK_FILE);
    let before = fs::read(&path).unwrap();

    write_manifest(
        &roots,
        &format!("{tool}\n[files.\"note\"]\ncontent = \"two\"\n"),
    );
    let outcome = tools::resolve(&roots, &fetcher, 1, false).unwrap().unwrap();
    assert!(!outcome.wrote_lock);
    assert_eq!(fs::read(path).unwrap(), before);
}

#[test]
fn locked_stale_tools_are_exit_10_with_the_required_hint_and_no_write() {
    let gate = support::home_gate();
    let env = support::home_env("home-lock-stale");
    let roots = roots(&env);
    let fetcher = MapFetcher::default();
    write_manifest(&roots, &inline("1.0.0", b"first\n"));
    tools::resolve(&roots, &fetcher, 0, false).unwrap();

    write_manifest(&roots, &inline("2.0.0", b"second\n"));
    let lock_path = roots.config().join(HOME_LOCK_FILE);
    let lock_before = fs::read(&lock_path).unwrap();
    let listing_before = support::listing(env.root());
    let ledger_before: Vec<_> = gate
        .ledger_lines()
        .into_iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect();

    let failure = tools::resolve(&roots, &fetcher, 1, true).unwrap_err();
    assert_eq!(failure.exit_status, 10);
    assert_eq!(failure.codes(), ["E_LOCK_STALE"]);
    let text = failure.to_string();
    assert!(
        text.contains("hint: run lodi home apply without --locked to re-resolve"),
        "{text}"
    );
    assert_eq!(fs::read(lock_path).unwrap(), lock_before);
    assert_eq!(support::listing(env.root()), listing_before);
    assert_eq!(
        gate.ledger_lines()
            .into_iter()
            .filter(|(_, path)| path.starts_with(env.root()))
            .collect::<Vec<_>>(),
        ledger_before
    );
    assert!(fetcher.requests().is_empty());
}

#[test]
fn a_newer_home_lock_uses_the_shared_lock_version_refusal() {
    let env = support::home_env("home-lock-version");
    let roots = roots(&env);
    let fetcher = MapFetcher::default();
    write_manifest(&roots, &inline("1.0.0", b"first\n"));
    let lock_path = roots.config().join(HOME_LOCK_FILE);
    let bytes = format!("{{\"version\":{}}}\n", LOCK_VERSION + 1);
    fs::write(&lock_path, &bytes).unwrap();

    let failure = tools::resolve(&roots, &fetcher, 0, false).unwrap_err();
    assert_eq!(
        failure.exit_status,
        lodi::diag::exit_status("E_LOCK_VERSION")
    );
    assert_eq!(failure.codes(), ["E_LOCK_VERSION"]);
    assert!(failure.to_string().contains(HOME_LOCK_FILE));
    assert_eq!(fs::read_to_string(lock_path).unwrap(), bytes);
    assert!(fetcher.requests().is_empty());
}

#[test]
fn a_manifest_without_tools_needs_and_produces_no_lock() {
    let env = support::home_env("home-lock-none");
    let roots = roots(&env);
    let fetcher = MapFetcher::default();
    write_manifest(
        &roots,
        "[home]\nversion = \"1\"\n\n[files.\"note\"]\ncontent = \"only files\"\n",
    );

    assert!(
        tools::resolve(&roots, &fetcher, 0, false)
            .unwrap()
            .is_none()
    );
    assert!(!roots.config().join(HOME_LOCK_FILE).exists());
    assert!(
        !roots.data().exists(),
        "resolution created a store for no tools"
    );
    assert!(fetcher.requests().is_empty());
}
