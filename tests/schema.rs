//! M-1.0 T-1: the on-disk schema contract.
//!
//! Three things are proved here, offline, deterministically, with no guest, no network and no
//! Podman:
//!
//! 1. **The registry cannot rot.** `src/**/*.rs` is read, every type that derives `Serialize` is
//!    collected, and the set must be exactly what `lodi::schema::ARTIFACTS` names. A negative
//!    control feeds the same scanner a source file that adds an unregistered type and requires
//!    it to be reported (acceptance S01).
//! 2. **Compatibility is proved by real specimens.** `tests/fixtures/schemas/<version>/` holds
//!    artifacts written by that released version's own code (see the README there). Each is read
//!    with **this** build, value for value (acceptance S02).
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

/// The host directory's pin lock (M-Pin, LD-395): format `lodi-host-pins/1`, which records no
/// writer by design, so that the same pins give the same bytes on any machine and under any
/// build. fk-1 (LD-434): a Fedora host's lock is version 2 and only a Fedora host's; every other
/// lock is still written as version 1.
#[test]
fn the_pin_lock_row_writes_2_for_fedora_only_and_its_file_records_no_writer() {
    use lodi::hostscope::pin::{self, PinsLock};
    let row = schema::artifact(pin::ARTIFACT);
    assert_eq!((row.writes, row.reads), (2, &[1u64, 2][..]));
    let text = fs::read_to_string(repo().join("tests/fixtures/host/pin/pins.lock"))
        .expect("the recorded pin lock");
    let raw: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(schema::version_of(row.kind, &raw), Some(1));
    assert_eq!(schema::writer_of(row.kind, &raw), None);
    assert_eq!(raw["format"], pin::FORMAT);
    for (distro, version, format) in [
        ("debian", 1, pin::FORMAT),
        ("ubuntu", 1, pin::FORMAT),
        ("arch", 1, pin::FORMAT),
        ("fedora", 2, pin::FEDORA_FORMAT),
    ] {
        let lock = PinsLock::new(distro, "x", "x86_64");
        assert_eq!((lock.version, lock.format.as_str()), (version, format));
        let bytes = pin::render_lock(&lock);
        assert_eq!(pin::parse_lock(bytes.as_bytes(), "x").unwrap(), lock);
        let mut other: Value = serde_json::from_str(&bytes).unwrap();
        other["version"] = json!(3 - version);
        other["format"] = json!(if version == 1 {
            pin::FEDORA_FORMAT
        } else {
            pin::FORMAT
        });
        let refused = pin::parse_lock(other.to_string().as_bytes(), "x").unwrap_err();
        assert_eq!(refused.code, "E_LOCK_VERSION", "{distro}");
    }
}

/// fk-1 (LD-434): the repository's one lock is version 2 when it holds a Fedora host and only
/// then, so a lock with none keeps its bytes, and an earlier build refuses a Fedora one by its
/// version instead of misreading its records.
#[test]
fn a_root_lock_is_version_2_with_a_fedora_host_and_only_then() {
    use lodi::flakelock::{self, HostSection, RootLock};
    use lodi::hostscope::pin::PinsLock;
    let mut lock = RootLock::new();
    lock.hosts.insert(
        "laptop".into(),
        HostSection::from_pins(PinsLock::new("debian", "bookworm", "x86_64")),
    );
    let debian = flakelock::render(&lock);
    assert!(
        debian.contains("\"format\": \"lodi-repository-lock/1\""),
        "{debian}"
    );
    lock.hosts.insert(
        "desk".into(),
        HostSection::from_pins(PinsLock::new("fedora", "44", "x86_64")),
    );
    let fedora = flakelock::render(&lock);
    assert!(
        fedora.contains("\"format\": \"lodi-repository-lock/2\""),
        "{fedora}"
    );
    let read = flakelock::parse(fedora.as_bytes(), "lodi.lock").unwrap();
    assert_eq!(flakelock::render(&read), fedora);
    let mut raw: Value = serde_json::from_str(&fedora).unwrap();
    raw["version"] = json!(1);
    raw["format"] = json!(flakelock::FORMAT);
    let refused = flakelock::parse(raw.to_string().as_bytes(), "lodi.lock").unwrap_err();
    assert_eq!(refused.code, "E_LOCK_VERSION");
    let mut raw: Value = serde_json::from_str(&debian).unwrap();
    raw["version"] = json!(2);
    raw["format"] = json!(flakelock::FEDORA_FORMAT);
    let refused = flakelock::parse(raw.to_string().as_bytes(), "lodi.lock").unwrap_err();
    assert_eq!(refused.code, "E_LOCK_VERSION");
}

/// The recorded pin lock reads back and renders to the same bytes.
#[test]
fn the_recorded_pin_lock_reads_back_byte_for_byte() {
    let path = repo().join("tests/fixtures/host/pin/pins.lock");
    let bytes = fs::read(&path).expect("the recorded pin lock");
    let lock = lodi::hostscope::pin::parse_lock(&bytes, "pins.lock").expect("it reads");
    assert_eq!(lodi::hostscope::pin::render_lock(&lock).into_bytes(), bytes);
}

/// A pin lock of a later format is refused before it is deserialized, naming the file and the
/// versions this build reads.
#[test]
fn a_pin_lock_outside_the_read_set_is_refused_before_it_is_deserialized() {
    let d = lodi::hostscope::pin::parse_lock(
        br#"{"version": 3, "format": "lodi-host-pins/3", "whatever": true}"#,
        "hosts/box/pins.lock",
    )
    .unwrap_err();
    assert_eq!(d.code, "E_LOCK_VERSION");
    assert_eq!(lodi::diag::exit_status(d.code), 4);
    assert!(d.message.contains("hosts/box/pins.lock"), "{d}");
    assert!(d.message.contains("lodi-host-pins/1"), "{d}");
}

// ---------------------------------------------------------------------------------------------
// 2. The specimens (design call D15).
// ---------------------------------------------------------------------------------------------

fn specimen_dir() -> PathBuf {
    repo().join("tests/fixtures/schemas")
}

/// Every released version that has a specimen directory, sorted.
fn specimen_versions() -> Vec<String> {
    let mut versions: Vec<String> = fs::read_dir(specimen_dir())
        .expect("the specimen directory exists")
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    versions.sort();
    versions
}

fn specimens(version: &str) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(specimen_dir().join(version))
        .expect("a specimen directory is readable")
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| {
            let kind = e
                .path()
                .file_stem()
                .expect("a specimen has a file stem")
                .to_string_lossy()
                .into_owned();
            (kind, fs::read(e.path()).expect("a specimen is readable"))
        })
        .collect()
}

/// The keys the registry adds to a document that an earlier release wrote without them.
fn added_fields(kind: &str) -> BTreeSet<String> {
    let a = schema::artifact(kind);
    [a.version_field.to_string(), a.writer_field.to_string()]
        .into_iter()
        .collect()
}

/// Every value in `original` is still there, unchanged, in `produced`; the only keys `produced`
/// may add are the registry's own two.
fn assert_round_trip(kind: &str, original: &Value, produced: &Value) {
    let original = original.as_object().expect("a specimen is a JSON object");
    let produced = produced.as_object().expect("the round trip is an object");
    for (key, value) in original {
        assert_eq!(
            produced.get(key),
            Some(value),
            "{kind}: `{key}` did not survive the round trip"
        );
    }
    let added = added_fields(kind);
    for key in produced.keys() {
        assert!(
            original.contains_key(key) || added.contains(key),
            "{kind}: the round trip invented the key `{key}`"
        );
    }
}

#[test]
fn the_released_versions_all_have_specimens() {
    let versions = specimen_versions();
    for released in ["0.2.0", "0.3.0"] {
        assert!(
            versions.iter().any(|v| v == released),
            "no specimen directory for the released version {released}"
        );
    }
    for version in &versions {
        let found = specimens(version);
        assert!(!found.is_empty(), "{version} has no specimen");
        for kind in found.keys() {
            assert!(
                ARTIFACTS.iter().any(|a| a.kind == *kind),
                "{version}/{kind}.json is not a registered artifact"
            );
        }
    }
}

#[test]
fn a_project_lock_written_by_a_released_lodi_still_reads() {
    for version in specimen_versions() {
        let bytes = specimens(&version)
            .remove("project-lock")
            .expect("every released version wrote a project lock");
        let lock = lodi::lock::parse_lock(&bytes).unwrap_or_else(|e| {
            panic!("the {version} project lock is refused: {e:?}");
        });
        assert_eq!(lock.version, lodi::lock::LOCK_VERSION);
        assert_eq!(lock.format, lodi::lock::LOCK_FORMAT);
        assert_eq!(lock.generated_by, format!("lodi {version}"));
        // The lock is canonical JSON, so a lock this build re-serializes is byte for byte the
        // lock that release wrote: no schema version moved and no lock byte moved (S04).
        assert_eq!(
            lock.to_canonical_json().as_bytes(),
            bytes.as_slice(),
            "the {version} lock does not round-trip byte for byte"
        );
        let raw: Value = serde_json::from_slice(&bytes).expect("a specimen is JSON");
        assert_eq!(schema::version_of("project-lock", &raw), Some(1));
        assert_eq!(
            schema::writer_of("project-lock", &raw).as_deref(),
            Some(version.as_str()),
            "the specimen's writer field must name the directory it is in"
        );
    }
}

/// The home scope's state record 1.0.0 wrote (M-Home, recorded by `tests/home_compat.rs`) reads
/// with this build, value for value, although this build writes version 2.
#[test]
fn a_home_state_written_by_a_released_lodi_still_reads() {
    let mut found = 0;
    for version in specimen_versions() {
        let Some(bytes) = specimens(&version).remove("home-state") else {
            continue;
        };
        found += 1;
        let raw: Value = serde_json::from_slice(&bytes).expect("a specimen is JSON");
        assert_eq!(schema::version_of("home-state", &raw), Some(1));
        assert!(schema::artifact("home-state").reads_version(1));
        let state: lodi::home::state::HomeState =
            serde_json::from_value(raw.clone()).expect("the 1.0 state record deserializes");
        let produced = serde_json::to_value(&state).unwrap();
        assert_round_trip("home-state", &raw, &produced);
    }
    assert!(found > 0, "no released home-state specimen");
}

#[test]
fn h7_new_home_state_records_schema_three_and_its_source() {
    let dir = scratch("h7-source");
    let home = dir.join("person");
    fs::create_dir_all(&home).unwrap();
    let roots = lodi::roots::Roots::for_host(home, dir.join("config"));
    let data = roots.data_root();
    lodi::home::state::write(&data, &lodi::home::state::HomeState::default()).unwrap();
    let bytes = fs::read(data.path().join(lodi::home::state::STATE_FILE)).unwrap();
    let raw: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(raw["version"], 3);
    assert_eq!(raw["source"], "~/.config/lodi");
    assert!(schema::artifact("home-state").reads_version(2));
    assert!(schema::artifact("home-state").reads_version(3));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn a_home_state_names_services_with_version_4_and_only_then() {
    use lodi::home::state::{self, HomeState, ServiceRecord};
    let dir = scratch("home-state-services");
    let roots = lodi::roots::Roots::for_host(dir.join("person"), dir.join("config"));
    let data = roots.data_root();
    let path = data.path().join(state::STATE_FILE);
    let mut record = HomeState::default();
    state::write(&data, &record).unwrap();
    let plain = fs::read(&path).unwrap();
    record.services.insert(
        "backup.timer".into(),
        ServiceRecord {
            before: "absent".into(),
        },
    );
    state::write(&data, &record).unwrap();
    let raw: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(raw["version"], 4);
    assert_eq!(raw["services"]["backup.timer"]["before"], "absent");
    let back = state::read(&data).unwrap();
    assert_eq!((back.version, back.services), (4, record.services.clone()));

    for (version, services, linger) in [
        (4, json!({}), false),
        (3, json!({"backup.timer": {"before": "absent"}}), false),
        (4, json!({"--now": {"before": "absent"}}), false),
        (4, json!({"backup.timer": {"before": "running"}}), false),
        (3, json!({}), true),
    ] {
        let mut raw = raw.clone();
        raw["version"] = json!(version);
        raw["services"] = services;
        raw["linger"] = json!(linger);
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(state::read(&data).unwrap_err().code, "E_STORE_IO", "{raw}");
    }

    record.services.clear();
    state::write(&data, &record).unwrap();
    assert_eq!(fs::read(&path).unwrap(), plain);
    record.linger = true;
    state::write(&data, &record).unwrap();
    assert_eq!(state::read(&data).unwrap().version, 4);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn a_store_sidecar_written_by_a_released_lodi_still_reads() {
    for version in specimen_versions() {
        let bytes = specimens(&version)
            .remove("store-sidecar")
            .expect("every released version wrote a store sidecar");
        let raw: Value = serde_json::from_slice(&bytes).expect("a specimen is JSON");
        assert_eq!(
            schema::writer_of("store-sidecar", &raw).as_deref(),
            Some(version.as_str())
        );
        // The sidecar has no `version` field before 1.0: reading it is the whole point of the
        // `#[serde(default)]` of design call D5. From 1.0 on the writer stamps it.
        let expected = if version.starts_with("0.") {
            None
        } else {
            Some(1)
        };
        assert_eq!(
            schema::version_of("store-sidecar", &raw),
            expected,
            "{version}"
        );
        let meta: lodi::store::Meta = schema::parse_registered("store-sidecar", &bytes)
            .unwrap_or_else(|| {
                panic!("the {version} store sidecar is refused");
            });
        assert_eq!(meta.version, 1, "a sidecar with no version is version 1");
        assert_eq!(meta.lodi_version, version);
        assert!(meta.complete);
        let produced: Value = serde_json::to_value(&meta).expect("the sidecar re-serializes");
        assert_round_trip("store-sidecar", &raw, &produced);
    }
}

#[test]
fn an_environment_plan_written_by_a_released_lodi_still_names_the_same_environment() {
    for version in specimen_versions() {
        let found = specimens(&version);
        let raw: Value = serde_json::from_slice(&found["environment-plan"]).expect("JSON");
        assert_eq!(schema::version_of("environment-plan", &raw), Some(1));
        assert_eq!(raw["format"], json!("lodi-spike-plan/1"));
        assert_eq!(raw["mode"], json!("host"));
        assert_eq!(raw["arch"], json!("x86_64"));
        assert_eq!(raw["env"], json!([["SPECIMEN", "1"]]));
        assert_eq!(raw["tasks"]["hello"]["run"], json!("echo hello"));

        // The plan document **is** the environment's identity: this build must hash the bytes
        // that release wrote to the `envhash` the same release recorded beside them, or a store
        // written by it would realize every environment again. From 1.0 on the document also
        // carries the registry's writer field, which is added to the document and never to the
        // value the identity is hashed from (`schema::stamped_json`), so it is left out here.
        let mut hashed = raw.clone();
        hashed
            .as_object_mut()
            .expect("a plan is a JSON object")
            .remove(schema::artifact("environment-plan").writer_field);
        let identity = lodi::util::sha256_tagged(lodi::lock::canonical_json(&hashed).as_bytes());
        let sidecar: Value = serde_json::from_slice(&found["store-sidecar"]).expect("JSON");
        assert_eq!(
            sidecar["identity"],
            json!(identity),
            "the {version} plan no longer hashes to the entry that holds it"
        );
        let session: Value = serde_json::from_slice(&found["session-root"]).expect("JSON");
        assert_eq!(session["activation"]["envhash"], json!(identity));
    }
}

#[test]
fn a_session_root_written_by_a_released_lodi_still_keeps_its_entries() {
    // The real read: `lodi gc --dry-run` over a store whose only root is the committed specimen,
    // renamed to this process's pid so that the root is live. Nothing in the file is edited.
    for version in specimen_versions() {
        let found = specimens(&version);
        let dir = scratch(&format!("session-{version}"));
        let store = dir.join("lodihome");
        let session: Value = serde_json::from_slice(&found["session-root"]).expect("JSON");
        let entry = session["entries"][0]
            .as_str()
            .expect("a session root names its entries")
            .to_string();
        fs::create_dir_all(store.join("gcroots/sessions")).unwrap();
        fs::create_dir_all(store.join("store/.meta")).unwrap();
        fs::create_dir_all(store.join(format!("store/{entry}"))).unwrap();
        fs::write(
            store.join(format!("store/{entry}/env.json")),
            &found["environment-plan"],
        )
        .unwrap();
        fs::write(
            store
                .join("gcroots/sessions")
                .join(std::process::id().to_string()),
            &found["session-root"],
        )
        .unwrap();
        fs::write(
            store.join(format!("store/.meta/{entry}.json")),
            &found["store-sidecar"],
        )
        .unwrap();

        let kept = gc_dry_run(&dir, &store);
        assert!(
            kept.contains("would remove 0 entries"),
            "the {version} session root no longer keeps {entry}:\n{kept}"
        );
        assert!(
            !kept.contains(&entry),
            "a live root's entry must not be named for collection:\n{kept}"
        );

        // With the root gone the same store collects that entry, which is what proves the root
        // is what kept it rather than an accident of the report.
        fs::remove_file(
            store
                .join("gcroots/sessions")
                .join(std::process::id().to_string()),
        )
        .unwrap();
        let without = gc_dry_run(&dir, &store);
        assert!(
            without.contains(&format!("would remove entry {entry}")),
            "with no root the entry must be collectable:\n{without}"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

fn gc_dry_run(home: &Path, store: &Path) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["gc", "--dry-run", "-v"])
        .env_clear()
        .env("PATH", "")
        .env("HOME", home)
        .env("LODI_HOME", store)
        .output()
        .expect("the lodi binary runs");
    assert!(out.status.success(), "{:?}", out);
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

#[test]
fn a_trust_file_written_by_a_released_lodi_is_still_trusted() {
    // The manifest the specimens were recorded from (tests/fixtures/schemas/README.md) declares
    // exactly this one task, so the hash that release recorded must be the hash this build
    // computes for the same text.
    let text = vec![("hello".to_string(), "echo hello".to_string())];
    for version in specimen_versions() {
        let bytes = specimens(&version)
            .remove("trust-store")
            .expect("every released version wrote a trust file");
        let raw: Value = serde_json::from_slice(&bytes).expect("JSON");
        assert_eq!(schema::version_of("trust-store", &raw), Some(1));
        let manifest = raw["entries"]
            .as_object()
            .expect("the trust file records entries")
            .keys()
            .next()
            .expect("one entry")
            .clone();

        let dir = scratch(&format!("trust-{version}"));
        let path = dir.join("trust.json");
        fs::write(&path, &bytes).unwrap();
        let store = lodi::trust::TrustStore::at(path.clone());
        let status = store
            .status(
                Path::new(&manifest),
                &lodi::trust::Subject::Tasks(text.clone()),
            )
            .expect("the released trust file is read");
        assert_eq!(
            status,
            lodi::trust::Status::Trusted,
            "the {version} trust file no longer trusts the text it recorded"
        );
        // Reading it is all a trust decision does to it: the released bytes are unchanged, and
        // the task text hashes to exactly what that release recorded (LD-359).
        assert_eq!(fs::read(&path).unwrap(), bytes, "{version}: the file moved");
        assert_eq!(
            lodi::trust::Subject::Tasks(text.clone()).hash().as_deref(),
            raw["entries"][&manifest]["hash"].as_str(),
            "{version}: the task text no longer hashes as it was recorded"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}

// ---------------------------------------------------------------------------------------------
// 3. The refusals (design calls D3, D18).
// ---------------------------------------------------------------------------------------------

/// A specimen with its schema version replaced by `version`. The bytes are still a released
/// version's own document; only the one field the refusal is about is moved.
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
    let bytes = specimens("0.3.0").remove("project-lock").unwrap();
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
        assert!(rendered.contains("written by lodi 0.3.0"), "{rendered}");
    }
}

#[test]
fn a_lock_that_records_no_writer_says_so_and_names_no_version() {
    let bytes = specimens("0.2.0").remove("project-lock").unwrap();
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
        !rendered.contains("0.2.0"),
        "no version may be guessed: {rendered}"
    );
    assert!(
        !rendered.contains("0.4"),
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
    let bytes = specimens("0.3.0").remove("trust-store").unwrap();
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

    // Releases through 0.3.0 wrote no writer field into the trust file, so the specimen itself is
    // the file that records nothing: the refusal must say exactly that and name no version.
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
        !rendered.contains("0.3.0"),
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

/// LD-401: schema 3 is the record of a host read from a git URL. A git table comes with version 3
/// and only with it, and a directory's record is still written as version 2.
#[test]
fn a_host_lock_carries_a_git_table_with_version_3_and_only_then() {
    use lodi::hostscope::lock::{GitRecord, HostLock};
    let dir = scratch("host-lock-git");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-09-26T00:00:00Z".into(), "debian".into(), "12".into());
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (2, "lodi-host-lock/2")
    );
    lock.set_git(Some(GitRecord {
        url: "git+https://github.com/owner/hosts.git".into(),
        reference: "HEAD".into(),
        rev: "0".repeat(40),
        nar_hash: format!("sha256:{}", "0".repeat(64)),
    }));
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (3, "lodi-host-lock/3")
    );
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
    raw.as_object_mut().unwrap().remove("git");
    fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
    assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
    raw["version"] = json!(2);
    raw["format"] = json!("lodi-host-lock/2");
    fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
    assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");

    lock.set_git(None);
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (2, "lodi-host-lock/2")
    );
    let _ = fs::remove_dir_all(&dir);
}

/// sd-1: schema 5 is the record that holds an OS basic, with or without services, and a record
/// with none is written as before; what it keeps to restore is held to the manifest's grammar.
#[test]
fn a_host_lock_holds_basics_with_version_5_and_only_then() {
    use lodi::hostscope::lock::{BasicRecord, HostLock};
    let dir = scratch("host-lock-basics");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-09-28T00:00:00Z".into(), "debian".into(), "12".into());
    lock.set_services([("ssh.service".to_string(), "enabled".to_string())].into());
    let before = lock.to_bytes();
    let record = |value: &str, was: Option<&str>| BasicRecord {
        value: value.into(),
        was: was.map(str::to_string),
    };
    lock.set_basics(
        [
            ("timezone".to_string(), record("Europe/Berlin", Some("UTC"))),
            ("network".to_string(), record("enp0s2", None)),
        ]
        .into(),
    );
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (5, "lodi-host-lock/5")
    );
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    for (version, basics) in [
        (5, json!({})),
        (4, json!({"timezone": {"value": "UTC"}})),
        (5, json!({"timezone": {"value": "UTC", "was": "-x"}})),
        (5, json!({"hostname": {"value": "-x"}})),
        (5, json!({"network": {"value": "enp0s2 -e"}})),
        (5, json!({"sysctl": {"value": "x"}})),
    ] {
        let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw["version"] = json!(version);
        raw["format"] = json!(format!("lodi-host-lock/{version}"));
        raw["basics"] = basics;
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    }

    lock.set_basics(Default::default());
    assert_eq!(lock.to_bytes(), before);
    let _ = fs::remove_dir_all(&dir);
}

/// bc-1: version 5 also holds the loader's parameters and each sysctl value with what it was;
/// what it keeps reaches a tool's argv, so it is held to the manifest's grammar.
#[test]
fn a_host_lock_holds_kernel_parameters_and_sysctl_values_in_version_5() {
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
    assert_eq!(lock.version, 5);
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

/// bl-1: version 5 also holds the declared loader with the one it replaced, and its timeout and
/// default entry; what it keeps reaches a tool's argv or a loader file, so it is held to the
/// manifest's grammar.
#[test]
fn a_host_lock_holds_the_boot_loader_in_version_5() {
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
    assert_eq!(lock.version, 5);
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

/// sc-1: schema 4 is the record that names a declared service, with or without a git table, and
/// a record with none is written as before.
#[test]
fn a_host_lock_names_services_with_version_4_and_only_then() {
    use lodi::hostscope::lock::HostLock;
    let dir = scratch("host-lock-services");
    let path = dir.join("host.lock");
    let mut lock = HostLock::new("2026-09-28T00:00:00Z".into(), "debian".into(), "12".into());
    let plain = lock.to_bytes();
    lock.set_services([("ssh.service".to_string(), "enabled".to_string())].into());
    assert_eq!(
        (lock.version, lock.format.as_str()),
        (4, "lodi-host-lock/4")
    );
    fs::write(&path, lock.to_bytes()).unwrap();
    assert_eq!(HostLock::read(&path).unwrap(), Some(lock.clone()));

    for (version, services) in [(4, json!({})), (2, json!({"ssh.service": "enabled"}))] {
        let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
        raw["version"] = json!(version);
        raw["format"] = json!(format!("lodi-host-lock/{version}"));
        raw["services"] = services;
        fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
        assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");
    }
    let mut raw: serde_json::Value = serde_json::from_slice(&lock.to_bytes()).unwrap();
    raw["services"] = json!({"--now": "enabled"});
    fs::write(&path, lodi::lock::canonical_json(&raw)).unwrap();
    assert_eq!(HostLock::read(&path).unwrap_err().code, "E_LOCK_VERSION");

    lock.set_services(Default::default());
    assert_eq!(lock.to_bytes(), plain);
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
        "{\"version\":5,\"lodiVersion\":\"9.9.9\",\"files\":{},\"directories\":[]}\n",
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
    assert!(rendered.contains("is schema version 5"), "{rendered}");
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
    let bytes = specimens("0.3.0").remove("store-sidecar").unwrap();
    assert!(
        schema::parse_registered::<lodi::store::Meta>("store-sidecar", &bytes).is_some(),
        "the released sidecar is usable"
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
        let out = lodi_in(&dir, &["lock"]);
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
    let out = lodi_in(&dir, &["lock"]);
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
    let out = lodi_in(&dir, &["lock"]);
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
    let out = lodi_in(&dir, &["lock"]);
    assert!(out.status.success(), "{:?}", out);
    let _ = fs::remove_dir_all(&dir);
}
