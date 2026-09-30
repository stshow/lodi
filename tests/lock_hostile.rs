//! A hostile `lodi.lock` — one a cloned repository ships — stops at `lock::validate`, before any
//! of its strings reaches a file name, a Containerfile line or `PATH` (M-1.0 S-1). One fixture
//! per gap; every one is the existing invalid-lock error, not a new code.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::sync::Mutex;

use lodi::fetch::{FetchError, Fetcher, Sink};
use lodi::lock::{
    LOCK_FORMAT, LOCK_VERSION, LockFile, Profile, closure_hash, parse_lock, validate,
};
use lodi::store::{MAX_ARTIFACT_BYTES, Report, Store};
use lodi::util::sha256_tagged;
use support::Tool;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/arch");

/// The hand-written Arch lock, which this build accepts as it is.
fn arch_lock() -> LockFile {
    let lock = parse_lock(
        fs::read_to_string(Path::new(FIXTURE).join("lodi.lock"))
            .unwrap()
            .as_bytes(),
    )
    .unwrap();
    validate(&lock).unwrap();
    lock
}

/// (a) `base.rootfs.subdir` is interpolated into a `COPY` line: a control character, or anything
/// outside `[A-Za-z0-9._+-]` components, is refused.
#[test]
fn a_subdir_with_a_control_character_is_an_invalid_lock() {
    for hostile in [
        "root.x86_64\nRUN touch /pwned",
        "root.x86_64 /etc",
        "root.x86_64\r",
        "root$x86_64",
    ] {
        let mut lock = arch_lock();
        lock.base.as_mut().unwrap().rootfs.subdir = Some(hostile.to_string());
        let error = validate(&lock).expect_err(&format!("{hostile:?} must be refused"));
        assert!(error.contains("subdir"), "{hostile:?}: {error}");
    }
    // The fixture's own value, and the grammar's ordinary shapes, still pass.
    for fine in ["root.x86_64", "a/b-c/d_e+f.g"] {
        let mut lock = arch_lock();
        lock.base.as_mut().unwrap().rootfs.subdir = Some(fine.to_string());
        validate(&lock).unwrap_or_else(|e| panic!("{fine:?} must pass: {e}"));
    }
}

/// (b) A closure package name becomes `<name>_<sha>.deb` in the build context: one that is not a
/// package name is refused, even when the closure hash the same lock states is consistent.
#[test]
fn a_closure_package_name_that_is_a_path_is_an_invalid_lock() {
    for hostile in ["../../evil", "a/b", "-flag", "", "nl\nRUN x"] {
        let mut lock = arch_lock();
        let base = lock.base.as_mut().unwrap();
        base.closure.packages[0].name = hostile.to_string();
        base.closure.packages.sort_by(|a, b| a.name.cmp(&b.name));
        base.closure.hash = closure_hash(&base.closure.packages);
        let error = validate(&lock).expect_err(&format!("{hostile:?} must be refused"));
        assert!(error.contains("not a package name"), "{hostile:?}: {error}");
    }
}

/// (d) `spec.bin` and `spec.path` are symlinked into `tree/bin`: a lock is held to the same
/// relative-path rule as the manifest.
#[test]
fn a_spec_path_that_leaves_the_artifact_is_an_invalid_lock() {
    for (what, hostile) in [
        ("path", "../../../../usr/bin"),
        ("path", "/usr/bin"),
        ("path", "bin//x"),
        ("bin", "../../../usr/bin/sudo"),
        ("bin", "/bin/sh"),
    ] {
        let tool = Tool::node("22", "22.1.0");
        let mut entry = tool.entry();
        match what {
            "path" => entry.spec.path = vec![hostile.to_string()],
            _ => entry.spec.bin = vec![hostile.to_string()],
        }
        let packages = BTreeMap::from([(tool.label.clone(), entry)]);
        let lock = LockFile {
            version: LOCK_VERSION,
            format: LOCK_FORMAT.into(),
            generated_by: "lodi 0.1.0".into(),
            manifest_hash: sha256_tagged(b"[tools]\n"),
            base: None,
            profiles: BTreeMap::from([(
                "default".to_string(),
                Profile {
                    packages: packages.keys().cloned().collect(),
                },
            )]),
            packages,
        };
        let error = validate(&lock).expect_err(&format!("spec.{what} {hostile:?} must be refused"));
        assert!(
            error.contains(&format!("spec.{what}")) && error.contains(hostile),
            "{what} {hostile:?}: {error}"
        );
    }
}

/// A fetcher that remembers the byte limit it was handed.
struct LimitFetcher {
    limits: Mutex<Vec<u64>>,
}

impl Fetcher for LimitFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::NotFound(url.to_string()))
    }

    fn requests(&self) -> Vec<String> {
        Vec::new()
    }

    fn download(&self, _url: &str, out: &mut dyn Sink, limit: u64) -> Result<u64, FetchError> {
        self.limits.lock().unwrap().push(limit);
        out.write_all(b"x").unwrap();
        Ok(1)
    }
}

/// (c) A lock-stated artifact size narrows the download ceiling and never raises it.
#[test]
fn a_lock_stated_size_never_raises_the_artifact_ceiling() {
    let home = support::scratch("lock-hostile").join("store");
    let store = Store::open_for_write(&home).unwrap();
    let fetcher = LimitFetcher {
        limits: Mutex::new(Vec::new()),
    };
    let mut report = Report::default();
    // The bytes will not match the hash; the limit handed to the fetcher is what is under test.
    let _ = store.fetch_verified(
        &fetcher,
        "https://artifacts.test/huge.tar.xz",
        &sha256_tagged(b"something else"),
        Some(MAX_ARTIFACT_BYTES * 64),
        &mut report,
    );
    let _ = store.fetch_verified(
        &fetcher,
        "https://artifacts.test/small.tar.xz",
        &sha256_tagged(b"something else"),
        Some(1024),
        &mut report,
    );
    let limits = fetcher.limits.lock().unwrap().clone();
    assert_eq!(
        limits,
        vec![MAX_ARTIFACT_BYTES, 1024],
        "the ceiling is a ceiling"
    );
    let _ = fs::remove_dir_all(&home);
}
