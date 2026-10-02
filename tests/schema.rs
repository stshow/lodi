//! M-1.0 T-1: the on-disk schema contract.
//!
//! Three things are proved here, offline, deterministically, with no guest, no network and no
//! Podman:
//!
//! 1. **The registry cannot rot.** `src/**/*.rs` is read, every type that derives `Serialize` is
//!    collected, and the set must be exactly what `lodi::schema::ARTIFACTS` names. A negative
//!    control feeds the same scanner a source file that adds an unregistered type and requires
//!    it to be reported (acceptance S01).
//! 2. **Every read set is the one version 2.0 writes** (#710), and `docs/SCHEMAS.md`'s table
//!    holds exactly the registry's rows: no 0.x or 1.x version is read back.
//! 3. **A file outside its read-set is refused** with its artifact's own code and exit status,
//!    before the rest of it is deserialized, and the message names the Lodi that wrote it — or
//!    says plainly that the file records none (acceptance S03).
//!
//! `[project] min_lodi_version` is exercised through the real binary at the end (acceptance S05).

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::schema::{self, ARTIFACTS};
use serde_json::{Value, json};

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    support::scratch(&format!("schema-{tag}"))
}

// ---------------------------------------------------------------------------------------------
// 1. The source scan (design call D4).
// ---------------------------------------------------------------------------------------------

/// Every `.rs` file under `src/`, as (module path, source text).
fn sources() -> Vec<(String, String)> {
    fn walk(dir: &Path, into: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).expect("src/ is readable").flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, into);
            } else if path.extension().is_some_and(|e| e == "rs") {
                into.push(path);
            }
        }
    }
    let root = repo().join("src");
    let mut files = Vec::new();
    walk(&root, &mut files);
    files.sort();
    files
        .into_iter()
        .map(|path| {
            let rel = path
                .strip_prefix(&root)
                .expect("every file walked is below src/")
                .with_extension("");
            let mut module = rel.to_string_lossy().replace('/', "::");
            if let Some(stripped) = module.strip_suffix("::mod") {
                module = stripped.to_string();
            }
            let text = fs::read_to_string(&path).expect("a source file is UTF-8");
            (module, text)
        })
        .collect()
}

/// Every `module::Type` in `files` that derives `Serialize`, so is a candidate for being written
/// to a file inside a Lodi root.
fn serialized_types(files: &[(String, String)]) -> BTreeSet<String> {
    let mut found = BTreeSet::new();
    for (module, text) in files {
        let lines: Vec<&str> = text.lines().collect();
        for (i, line) in lines.iter().enumerate() {
            let trimmed = line.trim();
            let Some(rest) = trimmed.strip_prefix("#[derive(") else {
                continue;
            };
            // `Serialize`, `serde::Serialize` and `::serde::Serialize` are the same derive.
            let derives: BTreeSet<&str> = rest
                .trim_end_matches(")]")
                .split(',')
                .map(|d| d.trim().rsplit("::").next().unwrap_or("").trim())
                .collect();
            if !derives.contains("Serialize") {
                continue;
            }
            // The item the derive belongs to is the next line that declares one; attributes
            // and doc comments may sit in between.
            for next in &lines[i + 1..] {
                let next = next.trim();
                if next.starts_with('#') || next.starts_with("//") || next.is_empty() {
                    continue;
                }
                let name = next
                    .trim_start_matches("pub ")
                    .trim_start_matches("pub(crate) ")
                    .strip_prefix("struct ")
                    .or_else(|| {
                        next.trim_start_matches("pub ")
                            .trim_start_matches("pub(crate) ")
                            .strip_prefix("enum ")
                    })
                    .map(|n| {
                        n.split(|c: char| !(c.is_alphanumeric() || c == '_'))
                            .next()
                            .unwrap_or("")
                            .to_string()
                    });
                if let Some(name) = name
                    && !name.is_empty()
                {
                    found.insert(format!("{module}::{name}"));
                }
                break;
            }
        }
    }
    found
}

/// Every type the registry claims.
fn registered_types() -> BTreeSet<String> {
    ARTIFACTS
        .iter()
        .flat_map(|a| a.types.iter().map(|t| (*t).to_string()))
        .collect()
}

#[test]
fn every_serialized_on_disk_type_is_named_by_the_registry() {
    let found = serialized_types(&sources());
    let registered = registered_types();
    let unregistered: Vec<&String> = found.difference(&registered).collect();
    assert!(
        unregistered.is_empty(),
        "these types derive Serialize and no row of lodi::schema::ARTIFACTS names them; add the \
         artifact they belong to (or the type to its row) before writing one to disk: \
         {unregistered:?}"
    );
    let gone: Vec<&String> = registered.difference(&found).collect();
    assert!(
        gone.is_empty(),
        "the registry names types that no longer derive Serialize in src/: {gone:?}"
    );
    // The registry is not empty in a way that would make the two assertions above vacuous.
    assert!(found.len() >= 30, "the scanner found only {}", found.len());
}

#[test]
fn the_scan_fails_when_an_unregistered_type_is_added() {
    // The negative control: the same scanner, over a source file this test supplies, which adds
    // one serialized record nobody registered.
    let fixture = vec![(
        "newscope::records".to_string(),
        "use serde::{Deserialize, Serialize};\n\n\
         /// A record a later package might write into a root.\n\
         #[derive(Debug, Clone, Serialize, Deserialize)]\n\
         #[serde(rename_all = \"camelCase\")]\n\
         pub struct LaterRecord {\n    pub version: u64,\n}\n"
            .to_string(),
    )];
    let found = serialized_types(&fixture);
    assert_eq!(
        found.iter().map(String::as_str).collect::<Vec<_>>(),
        ["newscope::records::LaterRecord"]
    );
    let registered = registered_types();
    assert!(
        found.difference(&registered).count() == 1,
        "an unregistered serialized type must be reported"
    );
}

#[test]
fn the_scan_ignores_a_type_that_only_derives_deserialize() {
    let fixture = vec![(
        "newscope::reader".to_string(),
        "#[derive(Debug, Deserialize)]\npub struct OnlyRead {\n    pub version: u64,\n}\n"
            .to_string(),
    )];
    assert!(serialized_types(&fixture).is_empty());
}

#[test]
fn the_registry_is_closed_and_self_consistent() {
    for a in ARTIFACTS {
        assert!(a.reads_version(a.writes), "{}", a.kind);
        assert!(!a.path.is_empty() && !a.root.is_empty(), "{}", a.kind);
        if let Some(shared) = a.shares {
            assert!(ARTIFACTS.iter().any(|o| o.kind == shared), "{}", a.kind);
        }
    }
    // Every row of `docs/SCHEMAS.md`'s table names a registered kind, and every registered kind
    // is in it: the documentation cannot drift from the code.
    let doc = fs::read_to_string(repo().join("docs/SCHEMAS.md")).expect("docs/SCHEMAS.md exists");
    for a in ARTIFACTS {
        assert!(
            doc.contains(&format!("`{}`", a.kind)),
            "docs/SCHEMAS.md does not name the artifact `{}`",
            a.kind
        );
    }
}

/// 2.0 restarts the read sets (#710): each artifact reads only the version it writes, a changed
/// format is numbered above any 1.x version, and the 1.x locks 2.0 no longer writes are gone.
#[test]
fn every_read_set_is_the_one_version_2_0_writes() {
    for a in ARTIFACTS {
        assert_eq!(a.reads, &[a.writes], "{} reads only what it writes", a.kind);
    }
    let kinds: Vec<&str> = ARTIFACTS.iter().map(|a| a.kind).collect();
    for gone in ["home-lock", "host-pins", "repository-lock"] {
        assert!(
            !kinds.contains(&gone),
            "{gone} is a 1.x artifact: {kinds:?}"
        );
    }
    // The host lock's last 1.x version was 5 and the home state's 4.
    assert_eq!(schema::artifact("host-lock").writes, 6);
    assert_eq!(schema::artifact("home-state").writes, 5);
}

/// The rows of `docs/SCHEMAS.md`'s table: kind, the version written, the versions read.
fn documented_rows() -> BTreeMap<String, (String, String)> {
    let doc = fs::read_to_string(repo().join("docs/SCHEMAS.md")).expect("docs/SCHEMAS.md exists");
    let mut rows = BTreeMap::new();
    for line in doc.lines().filter(|l| l.starts_with("| `")) {
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        let kind = cells[0].trim_matches('`').to_string();
        rows.insert(kind, (cells[3].to_string(), cells[4].to_string()));
    }
    rows
}

#[test]
fn the_schemas_table_is_the_registry() {
    let rows = documented_rows();
    let registered: BTreeSet<String> = ARTIFACTS.iter().map(|a| a.kind.to_string()).collect();
    assert_eq!(
        rows.keys().cloned().collect::<BTreeSet<_>>(),
        registered,
        "docs/SCHEMAS.md's table names exactly the registered artifacts"
    );
    for a in ARTIFACTS {
        let (writes, reads) = &rows[a.kind];
        assert_eq!(writes, &a.writes.to_string(), "{}: written version", a.kind);
        let listed: Vec<String> = a.reads.iter().map(u64::to_string).collect();
        assert_eq!(reads, &listed.join(", "), "{}: read set", a.kind);
    }
}

/// A document of `kind` as this build writes it, written by lodi `writer` (2.0 keeps these
/// formats, so their bytes are what 1.12.2 recorded).
fn written(kind: &str) -> Vec<u8> {
    let text = match kind {
        "project-lock" => {
            r#"{"base":null,"format":"lodi-spike-lock/1","generatedBy":"lodi 1.12.2","manifestHash":"sha256:006ebb69de9f201a947e5527801d9d83bbd6c14e54d915fc514e76e5db2104b8","packages":{},"profiles":{"default":{"packages":[]}},"version":1}"#
        }
        "trust-store" => {
            r#"{"entries":{"/tmp/work/project/lodi.toml":{"hash":"sha256:344f561fd0a19dfa46b99a34393e5cfe091b5f1b9ce42268707a41ebf5b195b1","trusted":"2026-09-30T22:58:38Z"}},"version":1}"#
        }
        "store-sidecar" => {
            r#"{"complete":true,"created":"2026-09-30T22:58:38Z","identity":"sha256:5a0c047285a3d4a97b7eb59185d170f84b7c3236161a5afc074ada74330d4a9a","lodiVersion":"1.12.2","name":"env-5a0c047285a3d4a97b7eb59185d170f8","references":[],"treeHash":"sha256:6b4f6fc582a7744058fd8652d4bc01dc092b178af7d8974442b685c0ef9b585b","type":"env","version":1}"#
        }
        other => panic!("no document for {other}"),
    };
    text.as_bytes().to_vec()
}

// ---------------------------------------------------------------------------------------------
// 3. The refusals (design calls D3, D18).
// ---------------------------------------------------------------------------------------------

/// A document with its schema version replaced by `version`; only the one field the refusal is
/// about is moved.
fn at_version(kind: &str, bytes: &[u8], version: u64) -> Vec<u8> {
    let mut raw: Value = serde_json::from_slice(bytes).expect("a specimen is JSON");
    raw[schema::artifact(kind).version_field] = json!(version);
    lodi::lock::canonical_json(&raw).into_bytes()
}

fn without_writer(kind: &str, bytes: &[u8]) -> Vec<u8> {
    let mut raw: Value = serde_json::from_slice(bytes).expect("a specimen is JSON");
    raw.as_object_mut()
        .expect("an object")
        .remove(schema::artifact(kind).writer_field);
    lodi::lock::canonical_json(&raw).into_bytes()
}

#[test]
fn a_lock_outside_the_read_set_is_refused_and_names_its_writer() {
    let bytes = written("project-lock");
    // Newer and older alike: a version outside the read-set is refused the same way, because an
    // upgrade on read would be a write (design call D18).
    for version in [0, 2, 99] {
        let problem = lodi::lock::parse_lock(&at_version("project-lock", &bytes, version))
            .expect_err("a lock outside the read-set is refused");
        let d = problem.diagnostic("lodi.lock");
        assert_eq!(d.code, "E_LOCK_VERSION");
        assert_eq!(lodi::diag::exit_status(d.code), 4);
        let rendered = d.to_string();
        assert!(
            rendered.contains(&format!("has lock version {version}")),
            "{rendered}"
        );
        assert!(rendered.contains("this lodi reads version 1"), "{rendered}");
        assert!(rendered.contains("written by lodi 1.12.2"), "{rendered}");
    }
}

#[test]
fn a_lock_that_records_no_writer_says_so_and_names_no_version() {
    let bytes = written("project-lock");
    let anonymous = without_writer("project-lock", &at_version("project-lock", &bytes, 2));
    let d = lodi::lock::parse_lock(&anonymous)
        .expect_err("still refused")
        .diagnostic("lodi.lock");
    let rendered = d.to_string();
    assert!(
        rendered.contains("this file does not record which lodi wrote it"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("1.12"),
        "no version may be guessed: {rendered}"
    );
}

#[test]
fn a_missing_lock_version_stays_the_stale_lock_error() {
    // A file with no numeric `version` is not "a schema version this build does not read": it is
    // not this document at all, and it keeps the code it has always had.
    let d = lodi::lock::parse_lock(b"{\"format\": \"lodi-spike-lock/1\"}\n")
        .expect_err("refused")
        .diagnostic("lodi.lock");
    assert_eq!(d.code, "E_LOCK_STALE");
    assert_eq!(lodi::diag::exit_status(d.code), 10);
}

#[test]
fn a_trust_file_outside_the_read_set_is_refused_with_its_own_code() {
    let bytes = written("trust-store");
    let dir = scratch("trust-refusal");
    let path = dir.join("trust.json");
    let refuse = |path: &Path| {
        lodi::trust::TrustStore::at(path.to_path_buf())
            .status(
                Path::new("/nowhere/lodi.toml"),
                &lodi::trust::Subject::Tasks(vec![("t".into(), "x".into())]),
            )
            .expect_err("a trust file outside the read-set is refused")
    };

    // This document records no writer field: the refusal must say exactly that and name no
    // version.
    fs::write(&path, at_version("trust-store", &bytes, 2)).unwrap();
    let d = refuse(&path);
    assert_eq!(d.code, "E_CONFIG");
    assert_eq!(lodi::diag::exit_status(d.code), 3);
    let rendered = d.to_string();
    assert!(rendered.contains("is schema version 2"), "{rendered}");
    assert!(
        rendered.contains("this file does not record which lodi wrote it"),
        "{rendered}"
    );
    assert!(
        !rendered.contains("1.12"),
        "no version may be guessed: {rendered}"
    );

    // A trust file this build's registry stamped does name its writer.
    let mut raw: Value = serde_json::from_slice(&bytes).expect("JSON");
    schema::stamp("trust-store", &mut raw);
    fs::write(
        &path,
        at_version(
            "trust-store",
            lodi::lock::canonical_json(&raw).as_bytes(),
            2,
        ),
    )
    .unwrap();
    let rendered = refuse(&path).to_string();
    assert!(
        rendered.contains(&format!("written by lodi {}", schema::LODI_VERSION)),
        "{rendered}"
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_host_lock_outside_the_read_set_is_refused_before_it_is_deserialized() {
    let dir = scratch("host-lock");
    let path = dir.join("host.lock");
    // Only the version moves; every other field is a host lock this build writes. A record that
    // is refused for its version is refused before the rest of it is read, so the absence of the
    // remaining fields never decides the message.
    fs::write(
        &path,
        lodi::lock::canonical_json(&json!({
            "version": 2,
            "format": "lodi-host-lock/1",
            "generatedBy": "lodi 9.9.9",
        })),
    )
    .unwrap();
    let d = lodi::hostscope::lock::HostLock::read(&path).expect_err("refused");
    assert_eq!(d.code, "E_LOCK_VERSION");
    assert_eq!(lodi::diag::exit_status(d.code), 4);
    let rendered = d.to_string();
    assert!(
        rendered.contains("is lodi-host-lock/1 version 2"),
        "{rendered}"
    );
    assert!(rendered.contains("written by lodi 9.9.9"), "{rendered}");

    fs::write(
        &path,
        "{\"version\": 2, \"format\": \"lodi-host-lock/1\"}\n",
    )
    .unwrap();
    let rendered = lodi::hostscope::lock::HostLock::read(&path)
        .expect_err("refused")
        .to_string();
    assert!(
        rendered.contains("this file does not record which lodi wrote it"),
        "{rendered}"
    );
    let _ = fs::remove_dir_all(&dir);
}

/// #710: 2.0 writes one host lock version, 6, whatever the record holds; a 1.x record (2 to 5)
/// is refused with the artifact's code and the file's path, and what it keeps for a tool's argv
/// is still held to the manifest's grammar.
#[test]
fn a_host_lock_is_version_6_whatever_it_holds() {
    use lodi::hostscope::lock::{BasicRecord, GitRecord, HostLock};
    let dir = scratch("host-lock-six");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-10-02T00:00:00Z".into(), "debian".into(), "12".into());
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (6, "lodi-host-lock/6")
    );
    lock.set_git(Some(GitRecord {
        url: "git+https://github.com/owner/hosts.git".into(),
        reference: "HEAD".into(),
        rev: "0".repeat(40),
        nar_hash: format!("sha256:{}", "0".repeat(64)),
    }));
    lock.set_services([("ssh.service".to_string(), "enabled".to_string())].into());
    lock.set_basics(
        [(
            "timezone".to_string(),
            BasicRecord {
                value: "Europe/Berlin".into(),
                was: Some("UTC".into()),
            },
        )]
        .into(),
    );
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (6, "lodi-host-lock/6")
    );
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    for version in 2..=5 {
        let mut raw: Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw["version"] = json!(version);
        raw["format"] = json!(format!("lodi-host-lock/{version}"));
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        let d = HostLock::read(&path).expect_err("a 1.x host lock is refused");
        assert_eq!(d.code, "E_LOCK_VERSION");
        assert!(d.to_string().contains(&path.display().to_string()), "{d}");
    }
    for (field, value) in [
        ("basics", json!({"timezone": {"value": "UTC", "was": "-x"}})),
        ("basics", json!({"hostname": {"value": "-x"}})),
        ("basics", json!({"network": {"value": "enp0s2 -e"}})),
        ("basics", json!({"sysctl": {"value": "x"}})),
        ("services", json!({"--now": "enabled"})),
    ] {
        let mut raw: Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw[field] = value;
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// bc-1: the host lock also holds the loader's parameters and each sysctl value with what it was;
/// what it keeps reaches a tool's argv, so it is held to the manifest's grammar.
#[test]
fn a_host_lock_holds_kernel_parameters_and_sysctl_values() {
    use lodi::hostscope::lock::{BasicRecord, HostLock};
    let dir = scratch("host-lock-kernel");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-09-29T00:00:00Z".into(), "debian".into(), "12".into());
    lock.set_basics(
        [
            (
                "kernel.parameters".to_string(),
                BasicRecord {
                    value: "quiet mitigations=off".into(),
                    was: None,
                },
            ),
            (
                "sysctl.vm.swappiness".to_string(),
                BasicRecord {
                    value: "10".into(),
                    was: Some("60".into()),
                },
            ),
        ]
        .into(),
    );
    assert_eq!(lock.version, 6);
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    for basics in [
        json!({"kernel.parameters": {"value": "quiet", "was": "x"}}),
        json!({"kernel.parameters": {"value": "a;b $(id)"}}),
        json!({"sysctl.vm.swappiness": {"value": "10"}}),
        json!({"sysctl.-a": {"value": "1", "was": "0"}}),
        json!({"sysctl.vm.swappiness": {"value": "1\n2", "was": "0"}}),
    ] {
        let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw["basics"] = basics;
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// bl-1: the host lock also holds the declared loader with the one it replaced, and its timeout and
/// default entry; what it keeps reaches a tool's argv or a loader file, so it is held to the
/// manifest's grammar.
#[test]
fn a_host_lock_holds_the_boot_loader() {
    use lodi::hostscope::lock::{BasicRecord, HostLock};
    let dir = scratch("host-lock-boot");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-09-29T00:00:00Z".into(), "debian".into(), "12".into());
    let record = |value: &str, was: Option<&str>| BasicRecord {
        value: value.into(),
        was: was.map(str::to_string),
    };
    lock.set_basics(
        [
            (
                "boot.loader".to_string(),
                record("systemd-boot", Some("grub")),
            ),
            ("boot.timeout".to_string(), record("3", None)),
            ("boot.default".to_string(), record("lodi-*", None)),
        ]
        .into(),
    );
    assert_eq!(lock.version, 6);
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    for basics in [
        json!({"boot.loader": {"value": "lilo", "was": "grub"}}),
        json!({"boot.loader": {"value": "grub"}}),
        json!({"boot.timeout": {"value": "x"}}),
        json!({"boot.timeout": {"value": "3", "was": "5"}}),
        json!({"boot.default": {"value": "a\"$(id)"}}),
        json!({"boot.other": {"value": "1"}}),
    ] {
        let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw["basics"] = basics;
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_journal_outside_the_read_set_is_refused_and_names_its_writer() {
    let dir = scratch("journal");
    let path = dir.join("2026-01-01T00-00-00Z-aaaa.json");
    fs::write(
        &path,
        "{\"record\":\"header\",\"version\":2,\"lodiVersion\":\"9.9.9\"}\n",
    )
    .unwrap();
    let d = lodi::hostscope::journal::read(&path).expect_err("refused");
    assert_eq!(d.code, "E_LOCK_VERSION");
    let rendered = d.to_string();
    assert!(rendered.contains("is journal version 2"), "{rendered}");
    assert!(rendered.contains("written by lodi 9.9.9"), "{rendered}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_home_state_and_backup_index_outside_the_read_set_are_refused() {
    let dir = scratch("home-records");
    let home = dir.join("home");
    let config = dir.join("config");
    let data = dir.join("data");
    for d in [&home, &config, &data] {
        fs::create_dir_all(d).unwrap();
    }
    // `XDG_DATA_HOME` is the parent of the data root: the root itself is `<XDG_DATA_HOME>/lodi`.
    let data_root = data.join("lodi");
    fs::create_dir_all(data_root.join("home-scope/backups")).unwrap();
    fs::write(
        data_root.join("home-scope/state.json"),
        "{\"version\":4,\"lodiVersion\":\"9.9.9\",\"files\":{},\"directories\":[]}\n",
    )
    .unwrap();
    fs::write(
        data_root.join("home-scope/backups/index.json"),
        "{\"version\":2,\"lodiVersion\":\"9.9.9\",\"backups\":{}}\n",
    )
    .unwrap();

    let (home, config, data_var) = (
        OsString::from(&home),
        OsString::from(&config),
        OsString::from(&data),
    );
    let roots = lodi::roots::Roots::from_vars(
        Some(home.as_os_str()),
        Some(config.as_os_str()),
        Some(data_var.as_os_str()),
        None,
    )
    .expect("scratch roots");
    let root = roots.data_root();

    let d = lodi::home::state::read(&root).expect_err("a state outside the read-set is refused");
    assert_eq!(d.code, "E_STORE_IO");
    assert_eq!(lodi::diag::exit_status(d.code), 6);
    let rendered = d.to_string();
    assert!(rendered.contains("is schema version 4"), "{rendered}");
    assert!(rendered.contains("home-scope/state.json"), "{rendered}");
    assert!(rendered.contains("written by lodi 9.9.9"), "{rendered}");

    let d = lodi::home::backup::read(&root).expect_err("an index outside the read-set is refused");
    assert_eq!(d.code, "E_STORE_IO");
    let rendered = d.to_string();
    assert!(rendered.contains("is schema version 2"), "{rendered}");
    assert!(rendered.contains("written by lodi 9.9.9"), "{rendered}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_sidecar_or_image_record_outside_the_read_set_is_not_a_usable_record() {
    let bytes = written("store-sidecar");
    assert!(
        schema::parse_registered::<lodi::store::Meta>("store-sidecar", &bytes).is_some(),
        "a sidecar this build writes is usable"
    );
    assert!(
        schema::parse_registered::<lodi::store::Meta>(
            "store-sidecar",
            &at_version("store-sidecar", &bytes, 2)
        )
        .is_none(),
        "a sidecar from a newer schema is never partly believed"
    );
}

// ---------------------------------------------------------------------------------------------
// 4. `[project] min_lodi_version` (design call D6).
// ---------------------------------------------------------------------------------------------

fn project(tag: &str, min: &str) -> PathBuf {
    let dir = scratch(tag);
    fs::write(
        dir.join("lodi.toml"),
        format!("[project]\nversion = \"1\"\nmin_lodi_version = \"{min}\"\n"),
    )
    .unwrap();
    dir
}

/// `lodi develop` locks the project first (LD-496), then runs a command that does nothing.
const LOCKS: &[&str] = &["develop", "--trust", "--", "/bin/sh", "-c", ":"];

fn lodi_in(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "")
        .env("HOME", dir)
        .env("LODI_HOME", dir.join("lodihome"))
        // Any request at all would fail loudly against a closed port; this command makes none.
        .env("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/")
        .output()
        .expect("the lodi binary runs")
}

#[test]
fn a_satisfied_project_min_lodi_version_resolves() {
    let running = lodi::schema::LODI_VERSION;
    let series = running
        .rsplit_once('.')
        .expect("a version has a patch component")
        .0
        .to_string();
    for constraint in [series.as_str(), ">=0.1.0", running] {
        let dir = project("min-ok", constraint);
        let out = lodi_in(&dir, LOCKS);
        assert!(
            out.status.success(),
            "`{constraint}` should be satisfied by lodi {running}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(dir.join("lodi.lock").exists());
        let _ = fs::remove_dir_all(&dir);
    }
}

#[test]
fn an_unsatisfied_project_min_lodi_version_is_e_version_before_anything_is_resolved() {
    let dir = project("min-unsatisfied", ">=99.0.0");
    let out = lodi_in(&dir, LOCKS);
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("E_VERSION"), "{stderr}");
    assert!(stderr.contains("[project] min_lodi_version"), "{stderr}");
    assert!(stderr.contains(lodi::schema::LODI_VERSION), "{stderr}");
    assert!(stderr.contains("install a newer lodi"), "{stderr}");
    // Nothing was resolved, fetched or written: not the lock, and not the store either.
    assert!(!dir.join("lodi.lock").exists());
    assert!(!dir.join("lodihome").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_project_min_lodi_version_that_is_not_a_constraint_is_e_version() {
    let dir = project("min-nonsense", "nonsense");
    let out = lodi_in(&dir, LOCKS);
    assert_eq!(out.status.code(), Some(3));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(stderr.contains("E_VERSION"), "{stderr}");
    assert!(stderr.contains("is not a version constraint"), "{stderr}");
    assert!(!dir.join("lodi.lock").exists());
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn an_empty_project_min_lodi_version_declares_no_minimum() {
    let dir = project("min-empty", "");
    let out = lodi_in(&dir, LOCKS);
    assert!(out.status.success(), "{:?}", out);
    let _ = fs::remove_dir_all(&dir);
}
