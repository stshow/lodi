//! M-Spike S-5: frozen replay and fault cases, run as the real binary.
//!
//! Frozen operation performs no discovery and substitutes nothing; corrupt, truncated,
//! oversized, vanished and refused artifacts, vanished store entries and tampered cache files
//! fail closed (the command never runs) and leave no complete entry; failed work is recovered
//! only by re-verification. Two live projects with different locked Node versions stay
//! independent. The host-mode cases use synthetic tool archives served by a loopback server
//! (`tests/support`) that counts every request; the container case uses real rootless Podman
//! with a storage of this test (`tests/support/podman.rs`, removed and checked gone at its end)
//! and the real locked artifacts through the caching
//! mirror of S-4 (`target/tmp/spike-mirror/`, filled from the network on first use). The same
//! behaviour against the real upstreams is `scripts/spike-local.sh --case frozen|faults|concurrent`.
//! The last test runs the evidence checker (`python3` on `PATH`).

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fixture.rs"]
mod fixture;
/// A test's own Podman storage and the one checked removal of it (#334).
#[path = "support/podman.rs"]
mod podman;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fixture::{Fixture, hold};
use podman::TestStorage;

use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
const ROOT: &str = env!("CARGO_MANIFEST_DIR");

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// One isolated user: a store, a config directory, a home and an artifact server.
struct User {
    base: PathBuf,
    server: Server,
}

impl User {
    fn new(name: &str, server: Server) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        User { base, server }
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn command(&self, dir: &Path, args: &[&str], rewrite: Option<&str>) -> Command {
        let mut c = Command::new(LODI);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.lodi_home())
            .env(
                "LODI_FETCH_REWRITE",
                rewrite.map_or_else(|| self.server.rewrite(), str::to_string),
            )
            .env("LODI_TRUST", "1")
            .stdin(Stdio::null());
        // The gate's cgroup placement inside lodi-work.slice (#367, scripts/gate.sh).
        if let Some(v) = std::env::var_os("CONTAINERS_CONF_OVERRIDE") {
            c.env("CONTAINERS_CONF_OVERRIDE", v);
        }
        c
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args, None).output().unwrap()
    }

    /// Run with every download refused (a loopback port nothing listens on), and asked for once:
    /// these cases prove that a refusal fails closed, not how it is tried again (LD-386), which
    /// would wait out the whole retry budget first.
    fn offline(&self, dir: &Path, args: &[&str]) -> Output {
        let closed = ClosedPort::bind();
        self.command(dir, args, Some(&closed.rewrite()))
            .env("LODI_FETCH_ATTEMPTS", "1")
            .output()
            .unwrap()
    }

    fn spawn(&self, dir: &Path, args: &[&str]) -> Fixture {
        Fixture::spawn(
            "lodi",
            self.command(dir, args, None)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped()),
        )
    }

    /// Store entries whose sidecar says `complete: true`.
    fn complete(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.lodi_home().join("store/.meta")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let v: serde_json::Value =
                    serde_json::from_slice(&fs::read(e.path()).ok()?).ok()?;
                (v["complete"] == true).then(|| v["name"].as_str().unwrap().to_string())
            })
            .collect();
        names.sort();
        names
    }

    fn tree_hash(&self, entry: &str) -> String {
        let meta = self
            .lodi_home()
            .join("store/.meta")
            .join(format!("{entry}.json"));
        let v: serde_json::Value = serde_json::from_slice(&fs::read(meta).unwrap()).unwrap();
        v["treeHash"].as_str().unwrap().to_string()
    }

    fn cache(&self) -> Vec<String> {
        listing(&self.lodi_home().join("cache/dl"))
    }

    /// Every cached download is named by the SHA-256 of its content.
    fn assert_cache_verified(&self) {
        for name in self.cache() {
            let bytes = fs::read(self.lodi_home().join("cache/dl").join(&name)).unwrap();
            assert_eq!(
                lodi::util::sha256_hex(&bytes),
                name,
                "unverified cache file"
            );
        }
    }

    fn entry(&self, prefix: &str, tool: &str) -> String {
        listing(&self.lodi_home().join("store"))
            .into_iter()
            .find(|n| n.starts_with(prefix) && !n.contains('/') && n.contains(tool))
            .unwrap_or_else(|| panic!("no {prefix}{tool} entry"))
    }

    fn sessions(&self) -> Vec<String> {
        session_listing(&self.lodi_home().join("gcroots/sessions"))
    }
}

fn remove_entry(user: &User, name: &str) {
    let path = user.lodi_home().join("store").join(name);
    make_writable(&path);
    fs::remove_dir_all(path).unwrap();
}

fn tools() -> Vec<Tool> {
    vec![Tool::python("3.12.14"), Tool::node("22", "22.23.2")]
}

const MARK: &[&str] = &["develop", "--", "sh", "-c", ": > ran"];

#[test]
fn frozen_replay_never_discovers_or_substitutes_and_stale_locks_are_refused_without_requests() {
    let tools = tools();
    let user = User::new("faults-frozen", Server::for_tools(&tools));
    let project = user.base.join("project");
    let task = "[tasks.mark]\nrun = \": > ran\"\n";
    write_project(&project, &manifest(&tools, task), &tools);
    let lock = fs::read(project.join("lodi.lock")).unwrap();

    // A fresh lock is never resolved again: `lodi lock` keeps it byte for byte, asks nothing.
    for args in [&["lock"][..], &["lock", "--check"]] {
        let o = user.run(&project, args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", err(&o));
    }
    assert!(
        user.server.requests().is_empty(),
        "{:?}",
        user.server.requests()
    );
    assert_eq!(fs::read(project.join("lodi.lock")).unwrap(), lock);

    // A cold entry fetches exactly the locked artifacts, once each, and nothing else.
    let o = user.run(&project, &["develop", "--", "node"]);
    assert_eq!(out(&o), "v22.23.2\n", "{}", err(&o));
    let mut requested = user.server.requests();
    requested.sort();
    let mut locked: Vec<String> = tools.iter().map(|t| t.url.clone()).collect();
    locked.sort();
    assert_eq!(
        requested, locked,
        "frozen entry requested only the locked artifacts"
    );

    // Newer versions upstream change nothing: the lock decides, the store is warm.
    let newer = Tool::node("22", "22.99.0");
    user.server
        .files
        .lock()
        .unwrap()
        .insert(newer.url.clone(), newer.bytes.clone());
    user.server.clear();
    let o = user.run(&project, &["develop", "--", "node"]);
    assert_eq!(out(&o), "v22.23.2\n");
    assert!(user.server.requests().is_empty());

    // Stale (the manifest asks for another version), invalid and missing locks: exit 10 with
    // no request, the lock untouched, nothing run.
    let stale = manifest(&[tools[0].clone(), Tool::node("20", "20.20.2")], task);
    fs::write(project.join("lodi.toml"), stale).unwrap();
    for args in [MARK, &["run", "mark"][..], &["lock", "--check"]] {
        let o = user.run(&project, args);
        assert_eq!(o.status.code(), Some(10), "{args:?}: {}", err(&o));
        assert!(err(&o).contains("E_LOCK_STALE"), "{}", err(&o));
    }
    assert_eq!(fs::read(project.join("lodi.lock")).unwrap(), lock);
    fs::write(project.join("lodi.toml"), manifest(&tools, task)).unwrap();
    for bad in [&b"{\"version\": 1}\n"[..], b"not json", b""] {
        fs::write(project.join("lodi.lock"), bad).unwrap();
        let o = user.run(&project, MARK);
        assert_eq!(o.status.code(), Some(10), "{}", err(&o));
    }
    fs::remove_file(project.join("lodi.lock")).unwrap();
    let o = user.run(&project, MARK);
    assert_eq!(o.status.code(), Some(10), "{}", err(&o));
    assert!(
        user.server.requests().is_empty(),
        "{:?}",
        user.server.requests()
    );
    assert!(!project.join("ran").exists());

    // A lock whose hash was altered in place is fresh, but its artifact fails verification.
    let mut altered = tools.clone();
    altered[1].bytes = node_archive("22.23.3");
    write_project(&project, &manifest(&altered, ""), &altered);
    let other = User::new("faults-frozen-altered", Server::for_tools(&tools));
    let o = other.run(&project, MARK);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(err(&o).contains("E_HASH_MISMATCH"), "{}", err(&o));
    assert!(!project.join("ran").exists());
    assert!(
        other
            .complete()
            .iter()
            .all(|n| !n.contains("nodejs") && !n.starts_with("env-"))
    );
}

#[test]
fn corrupt_truncated_oversized_vanished_and_refused_artifacts_fail_closed() {
    let tools = tools();
    let node = &tools[1];
    type Fault = fn(&User, &Tool);
    let faults: [(&str, &str, Fault); 6] = [
        ("corrupt", "E_HASH_MISMATCH", |u, t| {
            let mut b = t.bytes.clone();
            let mid = b.len() / 2;
            b[mid] ^= 0xFF;
            u.server.files.lock().unwrap().insert(t.url.clone(), b);
        }),
        ("truncated", "E_HASH_MISMATCH", |u, t| {
            let b = t.bytes[..t.bytes.len() / 2].to_vec();
            u.server.files.lock().unwrap().insert(t.url.clone(), b);
        }),
        ("oversized", "E_FETCH", |u, t| {
            let mut b = t.bytes.clone();
            b.extend_from_slice(&[0; 4096]);
            u.server.files.lock().unwrap().insert(t.url.clone(), b);
        }),
        ("another artifact", "E_HASH_MISMATCH", |u, t| {
            let b = node_archive("22.23.3");
            u.server.files.lock().unwrap().insert(t.url.clone(), b);
        }),
        ("vanished", "E_FETCH", |u, t| {
            u.server.missing.lock().unwrap().push(t.url.clone());
        }),
        ("refused", "E_FETCH", |_, _| {}),
    ];
    for (name, code, inject) in faults {
        let user = User::new(
            &format!("faults-{}", name.replace(' ', "-")),
            Server::for_tools(&tools),
        );
        let project = user.base.join("project");
        write_project(&project, &manifest(&tools, ""), &tools);
        inject(&user, node);
        let o = if name == "refused" {
            user.offline(&project, MARK)
        } else {
            user.run(&project, MARK)
        };
        assert_eq!(o.status.code(), Some(5), "{name}: {}", err(&o));
        assert!(err(&o).contains(code), "{name}: {}", err(&o));
        assert!(!project.join("ran").exists(), "{name}: the command ran");
        let complete = user.complete();
        assert!(
            complete
                .iter()
                .all(|n| !n.contains("nodejs") && !n.starts_with("env-")),
            "{name}: complete entries after the failure: {complete:?}"
        );
        assert!(!user.cache().contains(&node.sha256_hex()), "{name}: cached");
        user.assert_cache_verified();
        assert!(user.sessions().is_empty(), "{name}: live-session root left");

        // Recovery: the real bytes, the same store; staging and partial files are gone.
        user.server
            .files
            .lock()
            .unwrap()
            .insert(node.url.clone(), node.bytes.clone());
        user.server.missing.lock().unwrap().clear();
        let o = user.run(&project, &["develop", "--", "node"]);
        assert_eq!(out(&o), "v22.23.2\n", "{name} recovery: {}", err(&o));
        assert!(listing(&user.lodi_home().join("store/tmp")).is_empty());
        assert!(
            user.cache().iter().all(|f| !f.starts_with('.')),
            "{name}: partial file left"
        );
        user.assert_cache_verified();
    }
}

#[test]
fn vanished_store_entries_and_tampered_caches_are_rebuilt_only_from_verified_bytes() {
    let tools = tools();
    let user = User::new("faults-vanished", Server::for_tools(&tools));
    let project = user.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    let o = user.run(&project, &["develop", "--", "node"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let art = user.entry("art-", "nodejs");
    let env = user.entry("env-", "");
    let tree = user.tree_hash(&art);

    // The tree vanished, its sidecar stayed: it is residue, rebuilt from the verified cache
    // with no request, to the same tree hash.
    remove_entry(&user, &art);
    user.server.clear();
    let o = user.offline(&project, &["develop", "--", "node"]);
    assert_eq!(out(&o), "v22.23.2\n", "{}", err(&o));
    assert_eq!(user.tree_hash(&art), tree);

    // The environment entry vanished: rebuilt, nothing fetched.
    remove_entry(&user, &env);
    let o = user.offline(&project, &["develop", "--", "node"]);
    assert_eq!(out(&o), "v22.23.2\n", "{}", err(&o));
    assert!(user.server.requests().is_empty());

    // A cached download altered on disk is discarded, never extracted; offline that fails.
    remove_entry(&user, &art);
    let cached = user
        .lodi_home()
        .join("cache/dl")
        .join(tools[1].sha256_hex());
    let mut bytes = fs::read(&cached).unwrap();
    let mid = bytes.len() / 2;
    bytes[mid] ^= 0xFF;
    fs::write(&cached, bytes).unwrap();
    let o = user.offline(&project, MARK);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(!project.join("ran").exists());
    assert!(!user.complete().contains(&art));
    assert!(!cached.exists(), "the tampered file is discarded");
    user.assert_cache_verified();

    // Entry and cache gone, network refused: fail closed. With the network: re-verified.
    let o = user.offline(&project, MARK);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(!project.join("ran").exists());
    let o = user.run(&project, MARK);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(project.join("ran").exists());
    assert_eq!(user.tree_hash(&art), tree);
    assert_eq!(user.server.count(&tools[1].url), 1);
}

#[test]
fn two_live_projects_with_different_node_versions_stay_independent() {
    let node22 = Tool::node("22", "22.23.2");
    let node20 = Tool::node("20", "20.20.2");
    let python = Tool::python("3.12.14");
    let all = vec![python.clone(), node22.clone(), node20.clone()];
    let user = User::new("faults-concurrent", Server::for_tools(&all));
    let hold = format!(
        "node > .lodi/node; echo \"$PATH\" > .lodi/path; \
         echo \"$LODI_ACTIVATION\" > .lodi/activation; echo \"$LODI_ENV\" > .lodi/env; \
         : > .lodi/started; {}; node",
        hold("[ ! -e \"$1\" ]")
    );
    let hold = hold.as_str();
    let a = user.base.join("a");
    let b = user.base.join("b");
    write_project(
        &a,
        &manifest(&[python.clone(), node22.clone()], ""),
        &[python.clone(), node22],
    );
    let b_tools = [node20];
    write_project(&b, &manifest(&b_tools, ""), &b_tools);
    let o = user.run(&a, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let a_entries: BTreeMap<String, String> = user
        .complete()
        .into_iter()
        .map(|n| (n.clone(), user.tree_hash(&n)))
        .collect();

    let release = user.base.join("release");
    let release_arg = release.to_str().unwrap();
    for p in [&a, &b] {
        fs::create_dir_all(p.join(".lodi")).unwrap();
    }
    let children: Vec<Fixture> = [&a, &b]
        .iter()
        .map(|p| user.spawn(p, &["develop", "--", "sh", "-c", hold, "hold", release_arg]))
        .collect();
    wait::until("both children to start", || {
        a.join(".lodi/started").exists() && b.join(".lodi/started").exists()
    });
    assert_eq!(user.sessions().len(), 2, "both live at once");
    let read = |p: &Path, f: &str| fs::read_to_string(p.join(".lodi").join(f)).unwrap();
    assert_eq!(read(&a, "node"), "v22.23.2\n");
    assert_eq!(read(&b, "node"), "v20.20.2\n");
    assert_ne!(read(&a, "activation"), read(&b, "activation"));
    assert_ne!(read(&a, "env"), read(&b, "env"));
    assert!(!read(&a, "path").contains(read(&b, "env").trim()));
    assert!(!read(&b, "path").contains(read(&a, "env").trim()));
    fs::write(&release, "").unwrap();
    for (child, version) in children.into_iter().zip(["v22.23.2\n", "v20.20.2\n"]) {
        let o = child.wait_with_output().unwrap();
        assert_eq!(
            (o.status.code(), out(&o)),
            (Some(0), version.to_string()),
            "{}",
            err(&o)
        );
    }
    assert!(user.sessions().is_empty());
    for (name, tree) in &a_entries {
        assert_eq!(&user.tree_hash(name), tree, "project B changed {name}");
    }
    // The parent (this test process) never sees an activation.
    assert!(std::env::var_os("LODI_ACTIVATION").is_none());
}

// --- container mode, real Podman -----------------------------------------------------------

#[test]
fn container_artifacts_that_are_corrupt_or_vanished_build_and_record_nothing() {
    // Removed at the end by the checked removal the suites share, which fails the test if the
    // storage is still there (#334).
    let storage = TestStorage::new("faults-container");
    let fixture = Path::new(ROOT).join("tests/fixtures/spike");
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(fixture.join("lodi.lock")).unwrap()).unwrap();
    let base = &lock["base"];
    let rootfs = base["rootfs"]["url"].as_str().unwrap().to_string();
    let repos: BTreeMap<&str, &str> = base["repositories"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["name"].as_str().unwrap(), r["url"].as_str().unwrap()))
        .collect();
    let deb = base["closure"]["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["origin"] == "install")
        .map(|p| {
            format!(
                "{}{}",
                repos[p["repository"].as_str().unwrap()],
                p["filename"].as_str().unwrap()
            )
        })
        .unwrap();
    let mirror = mirror_cache();

    for (name, url, vanish) in [("rootfs", &rootfs, false), ("deb", &deb, true)] {
        let user = User::new(&format!("faults-container-{name}"), Server::mirror(&mirror));
        if vanish {
            user.server.missing.lock().unwrap().push(url.clone());
        } else {
            user.server
                .files
                .lock()
                .unwrap()
                .insert(url.clone(), b"not the locked layer".to_vec());
        }
        let project = user.base.join("spike-openssl");
        fs::create_dir_all(&project).unwrap();
        for f in ["lodi.toml", "lodi.lock", "Makefile", "openssl-version.c"] {
            fs::copy(fixture.join(f), project.join(f)).unwrap();
        }
        let o = user
            .command(&project, MARK, None)
            .env("CONTAINERS_STORAGE_CONF", storage.conf())
            .output()
            .unwrap();
        assert_eq!(o.status.code(), Some(5), "{name}: {}", err(&o));
        let code = if vanish { "E_FETCH" } else { "E_HASH_MISMATCH" };
        assert!(err(&o).contains(code), "{name}: {}", err(&o));
        assert!(!project.join("ran").exists(), "{name}: the command ran");
        assert!(!project.join(".lodi").exists(), "{name}: the fixture wrote");
        // The verified tools and their environment entry may be complete; the image is not.
        let meta = listing(&user.lodi_home().join("store/.meta"));
        assert!(
            meta.iter().all(|n| !n.starts_with("img-")),
            "{name}: image record {meta:?}"
        );
        user.assert_cache_verified();
        let images = storage.podman(&["images", "--all", "--format", "{{.Repository}}:{{.Tag}}"]);
        assert_eq!(images.status.code(), Some(0), "{}", err(&images));
        assert!(
            out(&images).lines().all(|l| !l.contains("lodi-")),
            "{name}: images {}",
            out(&images)
        );
        assert!(user.sessions().is_empty());
    }
    // The storage is gone once the test is done with it: its drop waits for the storage's
    // processes, tries again until the removal holds and fails here with Podman's error if it
    // never does (#327, #334).
    let dir = storage.dir().to_path_buf();
    drop(storage);
    assert!(
        !dir.exists(),
        "the test storage {} is still there",
        dir.display()
    );
}
