//! M-0.5 T-4: the home tool set consumes the shared schema and project realization pipeline,
//! with the command-facing schema enabled by T-5.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use lodi::fetch::{FetchError, Fetcher, HttpFetcher};
use lodi::home::tools;
use lodi::host;
use lodi::lock;
use lodi::manifest::parse_project_manifest;
use lodi::roots::Roots;
use lodi::store::{Report, Store};
use lodi::util::sha256_hex;

struct MapFetcher {
    files: BTreeMap<String, Vec<u8>>,
    requests: Mutex<Vec<String>>,
}

impl MapFetcher {
    fn one(url: &str, bytes: &[u8]) -> MapFetcher {
        MapFetcher {
            files: BTreeMap::from([(url.to_string(), bytes.to_vec())]),
            requests: Mutex::new(Vec::new()),
        }
    }
}

impl Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.requests.lock().unwrap().push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::NotFound(url.to_string()))
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

fn tool_block(url: &str, body: &[u8]) -> String {
    format!(
        "[tools.demo]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{}\"\n\
         format = \"binary\"\n",
        sha256_hex(body)
    )
}

fn write_home(roots: &Roots, text: &str) {
    fs::create_dir_all(roots.config()).unwrap();
    fs::write(roots.config().join(lock::HOME_MANIFEST_FILE), text).unwrap();
}

fn entry_names(store: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(store.join("store"))
        .unwrap()
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            (!name.starts_with('.') && name != "tmp").then_some(name)
        })
        .collect();
    names.sort();
    names
}

#[test]
fn malformed_tools_have_byte_identical_project_and_home_diagnostics() {
    let env = support::home_env("home-tools-diagnostics");
    let roots = roots(&env);
    let text = "[tools.demo]\nversion = 7\noptional = \"yes\"\nunknown = true\n";
    let file = "same.toml";
    let project = parse_project_manifest(text, file).unwrap_err();
    let home =
        tools::parse_manifest_bytes(text.as_bytes(), file, &roots.config_root()).unwrap_err();
    assert_eq!(home.diagnostics, project.diagnostics);
    assert!(
        home.diagnostics.len() >= 2,
        "the comparison must cover several errors"
    );
}

#[test]
fn home_and_project_realization_produce_the_same_entry_names() {
    let env = support::home_env("home-tools-realize");
    let roots = roots(&env);
    let url = "https://fixtures.test/demo-1.0.0";
    let body = b"#!/bin/sh\necho home tool\n";
    let block = tool_block(url, body);
    let fetcher = MapFetcher::one(url, body);

    let project_manifest = parse_project_manifest(
        &format!("[project]\nversion = \"1\"\n\n{block}"),
        "project.toml",
    )
    .unwrap();
    let (project_lock, _) = lock::resolve_manifest(
        &project_manifest,
        lock::tool_set_hash(&project_manifest.tools),
        None,
        &fetcher,
        0,
    )
    .unwrap();
    let project_plan = host::plan_for(&project_manifest, &project_lock, "host", None).unwrap();
    let project_store_path = env.root().join("project-store");
    let project_store = Store::open(&project_store_path).unwrap();
    let _shared = project_store.shared_lock().unwrap();
    host::realize(
        &project_store,
        &fetcher,
        &project_lock,
        &project_plan,
        &mut Report::default(),
    )
    .unwrap();

    write_home(&roots, &format!("[home]\nversion = \"1\"\n\n{block}"));
    let resolution = tools::resolve(&roots, &fetcher, 0, false, None)
        .unwrap()
        .unwrap();
    let realized = tools::realize(&roots, &fetcher, &resolution).unwrap();

    assert_eq!(entry_names(&project_store_path), entry_names(roots.data()));
    let mut project_entries = vec![project_plan.env_name()];
    project_entries.extend(project_plan.arts.iter().map(|(_, name)| name.clone()));
    assert_eq!(realized.entries, project_entries);
    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(roots.data().join(tools::HOME_GC_ROOT)).unwrap()).unwrap();
    assert_eq!(root["entries"], serde_json::json!(project_entries));
}

#[test]
fn command_facing_reports_accept_tools_and_write_nothing() {
    let env = support::home_env("home-tools-cli-reports");
    let roots = roots(&env);
    let text = format!(
        "[home]\nversion = \"1\"\n\n{}",
        tool_block("https://fixtures.test/demo", b"tool\n")
    );
    write_home(&roots, &text);
    let before = support::listing(env.root());

    // `lodi switch --home --dry-run`, 1.x's `home plan` and `home status` (LD-518).
    let out = env.lodi(&["home", "plan"]).output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(0), "{stderr}");
    assert_eq!(support::listing(env.root()), before);
    assert!(!roots.config().join(lock::HOME_LOCK_FILE).exists());
    assert!(!roots.config().join("lodi.lock").exists());
    assert!(!env.share().join("lodi").join(tools::HOME_GC_ROOT).exists());
    assert!(!roots.data().join(tools::HOME_GC_ROOT).exists());
}

/// The package's real-network evidence: resolve the embedded jq recipe against its real release
/// API, download and verify the real artifact, realize it in a scratch store, and root it. The
/// package probe selects this ignored test explicitly; ordinary `cargo test` stays offline.
#[test]
#[ignore = "selected only by scripts/m05-local.sh --case home-tools"]
fn live_home_tool_resolves_realizes_and_is_rooted() {
    assert_eq!(std::env::var("LODI_M05_LIVE").as_deref(), Ok("1"));
    let env = support::home_env("home-tools-live");
    let roots = roots(&env);
    write_home(
        &roots,
        "[home]\nversion = \"1\"\n\n[tools]\njq = \"latest\"\n",
    );
    let fetcher = HttpFetcher::from_env().unwrap();
    let resolution = tools::resolve(&roots, &fetcher, 0, false, None)
        .unwrap()
        .unwrap();
    let realized = tools::realize(&roots, &fetcher, &resolution).unwrap();
    let store = Store::open(roots.data()).unwrap();
    assert!(
        realized
            .entries
            .iter()
            .all(|name| store.complete(name).is_some())
    );
    assert!(roots.data().join(tools::HOME_GC_ROOT).is_file());
    assert!(!fetcher.requests().is_empty());
}
