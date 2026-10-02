//! gu-1 (LD-401): a switch of a public git URL, recorded and switched again offline.
//!
//! On the 2.0 commands (#699, LD-512): `plan URL` is `lodi switch --host --dry-run URL` and
//! `apply URL` is `lodi switch --host URL`; the fetched tree is kept below the scratch home's
//! state (`~/.local/state/lodi/git/`), and the commit a switch applied is recorded beside it in
//! `fetched.json`.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, and a loopback replay of real git http-backend
//! recordings (`tests/support/git_host.rs`) reached through `LODI_FETCH_REWRITE`. Nothing reaches
//! `/` or a forge, and no real package manager runs (`AGENTS.md` §8). `LODI_FETCH_ATTEMPTS=1`
//! keeps a refused connection from waiting on retries, so no case sleeps.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/git_host.rs"]
mod git_host;
#[path = "support/git_loopback.rs"]
mod git_loopback;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Output;

use fakehost::{Case, Machine, Pkg, err, nothing, story};
use git_host::{Counter, FIRST, LINK, SECOND, SUBMODULE, Server};

const URL: &str = "git+https://git.example.test/hosts.git";
const HTTPS: &str = "https://git.example.test/hosts.git";

fn case(name: &str) -> Case {
    let case = Case::new(
        name,
        Machine::debian()
            .offering(Pkg::new("jq"))
            .offering(Pkg::new("pv"))
            .offering(Pkg::new("tree")),
    );
    case.root.write("etc/hostname", "box\n");
    case
}

/// One host verb with `rewrite` as `LODI_FETCH_REWRITE`.
fn run(case: &Case, verb: &str, args: &[&str], rewrite: &str) -> Output {
    case.verb_env(
        verb,
        args,
        &[
            ("LODI_FETCH_REWRITE", rewrite),
            ("LODI_FETCH_ATTEMPTS", "1"),
        ],
    )
}

fn to(server: &Server) -> String {
    server.rewrite(HTTPS, "/hosts.git")
}

fn nowhere(counter: &Counter) -> String {
    format!("{HTTPS}={}/hosts.git", counter.url)
}

fn code(output: &Output) -> Option<i32> {
    output.status.code()
}

fn diagnosed(output: &Output, code: &str) -> bool {
    err(output).contains(code)
}

fn cache(case: &Case) -> PathBuf {
    case.root.path("home/.local/state/lodi/git")
}

fn tree(case: &Case, url: &str, commit: &str) -> PathBuf {
    cache(case)
        .join(lodi::util::sha256_hex(url.as_bytes()))
        .join(commit)
}

/// The record of the commit the last switch from a URL applied: url, ref, rev and narHash.
fn record(case: &Case) -> serde_json::Value {
    serde_json::from_str(&case.root.read(RECORD)).expect("the fetched record")
}

const RECORD: &str = "home/.local/state/lodi/fetched.json";

fn installed(case: &Case, name: &str) -> bool {
    case.installed().contains(name)
}

/// Every path below the root with its modification time and length, but the person's own
/// bookkeeping a switch rewrites in the scratch home's state: its run logs, the remembered config
/// path and the fetched commit's record (whose bytes the tests compare instead).
fn mtimes(root: &Path) -> BTreeMap<PathBuf, (i64, i64, u64)> {
    let mut seen = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if ["logs", "config-path", "fetched.json"]
                .iter()
                .any(|name| path.ends_with(Path::new("home/.local/state/lodi").join(name)))
            {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if !path.ends_with("home/.local/state/lodi") {
                seen.insert(path.clone(), (meta.mtime(), meta.mtime_nsec(), meta.len()));
            }
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------- U1 grammar ---

/// U1. Insecure forms and credentials are `E_INSECURE_URL`, malformed paths `E_UNSUPPORTED`,
/// and `--rev` with `--refresh` a usage error: each before any request, as a counting listener
/// every form points at shows.
#[test]
fn u1_insecure_and_malformed_urls_are_refused_before_any_request() {
    let case = case("u1-grammar");
    let counter = Counter::start();
    let at = counter.url.trim_start_matches("http://").to_string();
    let rewrite = nowhere(&counter);
    for insecure in [
        format!("git+http://{at}/hosts.git"),
        format!("git+ssh://{at}/hosts.git"),
        "git+file:///srv/hosts.git".to_string(),
        format!("ssh://{at}/hosts.git"),
        format!("git@{at}:hosts.git"),
        "git+https://user:pw@127.0.0.1/hosts.git".to_string(),
        "git+https://token@localhost/hosts.git".to_string(),
    ] {
        let output = run(&case, "plan", &[&insecure], &rewrite);
        assert_eq!(code(&output), Some(5), "{insecure}: {}", story(&output));
        assert!(diagnosed(&output, "E_INSECURE_URL"), "{}", story(&output));
    }
    for (malformed, named) in [
        (format!("{URL}?ref=main"), "?"),
        (format!("{URL}#box"), "#"),
        ("git+https://git.example.test".to_string(), ""),
        ("git+https://git.example.test/".to_string(), ""),
        (
            "git+https://git.example.test/a/../hosts.git".to_string(),
            "..",
        ),
        ("git+https://git.example.test/a b.git".to_string(), " "),
        ("git+https://git.example.test/a%2e.git".to_string(), "%"),
    ] {
        let output = run(&case, "plan", &[&malformed], &rewrite);
        assert_eq!(code(&output), Some(3), "{malformed}: {}", story(&output));
        assert!(diagnosed(&output, "E_UNSUPPORTED"), "{}", story(&output));
        assert!(err(&output).contains(named), "{}", story(&output));
    }
    for args in [vec![URL, "--rev", FIRST, "--refresh"]] {
        for verb in ["plan", "apply"] {
            let output = run(&case, verb, &args, &rewrite);
            assert_eq!(
                code(&output),
                Some(2),
                "{verb} {args:?}: {}",
                story(&output)
            );
        }
    }
    assert_eq!(counter.connections(), 0, "a refusal made a request");
    assert!(!case.root.exists("etc/lodi/host.lock"));
    assert!(!case.root.exists(RECORD));
    assert!(!cache(&case).exists());
}

// ------------------------------------------------------------ U2 layouts and selection ---

/// U2. A fetched tree with a top-level `host.toml` switches.
#[test]
fn u2_a_top_level_host_switches() {
    let single = case("u2-single");
    let server = Server::start("single", "first");
    let applied = run(&single, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&single, "jq"));
}

// ------------------------------------------------------------------------- U3 the cache ---

/// U3. The fetched tree lives at `~/.local/state/lodi/git/<URL sha256>/<commit>/`,
/// private (directories 0700, files 0600), with no staging sibling left; once a newer commit is
/// locked the older tree is pruned; and a tree changed on disk is refused at the next read.
#[test]
fn u3_the_cache_is_private_verified_and_pruned() {
    let case = case("u3-cache");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let first = tree(&case, URL, FIRST);
    let meta = |p: &Path| fs::metadata(p).unwrap();
    assert_eq!(meta(&first).permissions().mode() & 0o7777, 0o700);
    assert_eq!(
        meta(first.parent().unwrap()).permissions().mode() & 0o7777,
        0o700
    );
    assert_eq!(meta(&cache(&case)).permissions().mode() & 0o7777, 0o700);
    assert_eq!(
        meta(&first.join("host.toml")).permissions().mode() & 0o7777,
        0o600
    );
    let mut entries: Vec<_> = fs::read_dir(first.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    entries.sort();
    // The tree, and the lock it was switched with kept beside it (LD-528).
    let kept = [FIRST.to_string(), format!("{FIRST}.lock")];
    assert_eq!(entries, kept.map(std::ffi::OsString::from));

    server.push("second");
    let refreshed = run(&case, "apply", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refreshed), Some(0), "{}", story(&refreshed));
    assert!(installed(&case, "pv"));
    assert!(tree(&case, URL, SECOND).is_dir());
    assert!(!first.exists(), "the tree no longer locked was not pruned");
    assert!(
        !first.with_extension("lock").exists(),
        "nor the lock kept beside it"
    );

    let second = tree(&case, URL, SECOND).join("host.toml");
    fs::write(&second, "[host]\ndistro = \"debian\"\n").unwrap();
    let counter = Counter::start();
    let tampered = run(&case, "apply", &[URL], &nowhere(&counter));
    assert_eq!(code(&tampered), Some(5), "{}", story(&tampered));
    assert!(
        diagnosed(&tampered, "E_HASH_MISMATCH"),
        "{}",
        story(&tampered)
    );
    assert!(installed(&case, "pv"));
    assert_eq!(counter.connections(), 0);
}

/// U3. Under a scratch `--root` the cache may belong to root or the invoker, and nobody else may
/// write it: a group-writable tree is refused as any host directory is (LD-357, LD-379).
#[test]
fn u3_a_cache_others_could_write_is_refused() {
    let case = case("u3-trust");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let first = tree(&case, URL, FIRST);
    fs::set_permissions(&first, fs::Permissions::from_mode(0o770)).unwrap();
    let counter = Counter::start();
    let refused = run(&case, "plan", &[URL], &nowhere(&counter));
    assert_eq!(code(&refused), Some(3), "{}", story(&refused));
    assert!(diagnosed(&refused, "E_PATH_ESCAPE"), "{}", story(&refused));
    assert_eq!(counter.connections(), 0);
}

/// U3. Only git's directory and regular-file modes materialise: a symbolic link is
/// `E_PATH_ESCAPE` and a submodule `E_UNSUPPORTED` (LD-400's tree rules), with nothing cached.
#[test]
fn u3_links_and_submodules_do_not_materialise() {
    for (name, commit, expected, exit) in [
        ("link", LINK, "E_PATH_ESCAPE", 3),
        ("submodule", SUBMODULE, "E_UNSUPPORTED", 3),
    ] {
        let case = case(&format!("u3-{name}"));
        let server = Server::start(name, "only");
        let output = run(&case, "apply", &[URL], &to(&server));
        assert_eq!(code(&output), Some(exit), "{name}: {}", story(&output));
        assert!(diagnosed(&output, expected), "{}", story(&output));
        assert!(!tree(&case, URL, commit).exists());
        assert!(!case.root.exists("etc/lodi/host.lock"));
    }
}

// ----------------------------------------------------------------- U4 lock and fence ---

/// U4. A URL switch records the git table (url, ref, rev, narHash) of what it applied.
#[test]
fn u4_the_record_holds_the_git_source() {
    let case = case("u4-lock");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let git = record(&case);
    assert_eq!(git["url"], URL);
    assert_eq!(git["ref"], "HEAD");
    assert_eq!(git["rev"], FIRST);
    let nar = git["narHash"].as_str().unwrap();
    assert!(nar.starts_with("sha256:") && nar.len() == 71, "{nar}");
}

// ------------------------------------------------------------------------ U5 offline ---

/// U5, named red-first proof (2). After a recorded switch the server is stopped; a re-switch
/// works from the record and the verified cache alone, with zero connections, no mtime moved
/// below the root but the run's log, and nothing to switch.
#[test]
fn offline_reapply_from_the_lock_alone() {
    let case = case("u5-offline");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let asked = server.connections();
    drop(server);

    let recorded = case.root.read(RECORD);
    let before = mtimes(&case.root.dir);
    let counter = Counter::start();
    let again = run(&case, "apply", &[URL], &nowhere(&counter));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(nothing(&again), "{}", story(&again));
    assert_eq!(
        counter.connections(),
        0,
        "the offline re-apply made a request"
    );
    assert_eq!(
        mtimes(&case.root.dir),
        before,
        "the offline re-apply moved an mtime"
    );
    assert_eq!(case.root.read(RECORD), recorded);
    assert!(asked > 0);
}

/// U5. A deleted cache tree fetches the locked commit once and checks its NAR against the lock;
/// a lock whose NAR the fetched tree does not have stores nothing.
#[test]
fn u5_a_deleted_cache_tree_fetches_the_locked_commit_once() {
    let case = case("u5-deleted");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    server.push("second");
    fs::remove_dir_all(tree(&case, URL, FIRST)).unwrap();
    let before = server.lines().len();
    let again = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(nothing(&again), "{}", story(&again));
    let asked = &server.lines()[before..];
    assert_eq!(asked.len(), 2, "one advertisement and one fetch: {asked:?}");
    assert!(tree(&case, URL, FIRST).is_dir());
    assert!(!installed(&case, "pv"), "the pushed change was applied");

    let mut kept = record(&case);
    kept["narHash"] = serde_json::json!(format!("sha256:{}", "0".repeat(64)));
    case.root.write(
        RECORD,
        &format!("{}\n", serde_json::to_string_pretty(&kept).unwrap()),
    );
    fs::remove_dir_all(tree(&case, URL, FIRST)).unwrap();
    let mismatch = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&mismatch), Some(5), "{}", story(&mismatch));
    assert!(
        diagnosed(&mismatch, "E_HASH_MISMATCH"),
        "{}",
        story(&mismatch)
    );
    // Only the lock the last switch kept beside the commit's tree is left (LD-528).
    let left: Vec<_> = fs::read_dir(tree(&case, URL, FIRST).parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    let kept = std::ffi::OsString::from(format!("{FIRST}.lock"));
    assert_eq!(left, [kept], "a mismatched tree was stored");
}

/// U5. `--refresh` resolves again and moves the lock old -> new; `--rev` pins exactly, and a later
/// bare URL apply keeps what is locked.
#[test]
fn u5_refresh_moves_and_rev_pins() {
    let case = case("u5-refresh");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    server.push("second");
    let unchanged = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&unchanged), Some(0), "{}", story(&unchanged));
    assert_eq!(record(&case)["rev"], FIRST, "a push moved the lock");

    let refreshed = run(&case, "apply", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refreshed), Some(0), "{}", story(&refreshed));
    assert!(
        err(&refreshed).contains(&format!("refresh {FIRST} -> {SECOND}")),
        "{}",
        story(&refreshed)
    );
    assert_eq!(record(&case)["rev"], SECOND);
    assert!(installed(&case, "pv"));

    let pinned = run(&case, "apply", &[URL, "--rev", FIRST], &to(&server));
    assert_eq!(code(&pinned), Some(0), "{}", story(&pinned));
    assert_eq!(record(&case)["rev"], FIRST);
    let kept = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&kept), Some(0), "{}", story(&kept));
    assert_eq!(record(&case)["rev"], FIRST);

    let stable = run(&case, "apply", &[URL, "--ref", "stable"], &to(&server));
    assert_eq!(code(&stable), Some(0), "{}", story(&stable));
    assert_eq!(record(&case)["ref"], "stable");
    assert_eq!(record(&case)["rev"], FIRST);
}

/// U5. An offline `--refresh`, and a ref the lock does not hold offline, are `E_FETCH` with
/// nothing written and the lock unchanged.
#[test]
fn u5_offline_refresh_and_unlocked_ref_fail_with_nothing_written() {
    let case = case("u5-offline-refresh");
    let server = Server::start("single", "second");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    drop(server);
    let locked = case.root.read(RECORD);
    let before = mtimes(&case.root.dir);
    let counter = Counter::start();
    for args in [vec![URL, "--refresh"], vec![URL, "--ref", "stable"]] {
        for verb in ["plan", "apply"] {
            let output = run(&case, verb, &args, &nowhere(&counter));
            assert_eq!(
                code(&output),
                Some(5),
                "{verb} {args:?}: {}",
                story(&output)
            );
            assert!(diagnosed(&output, "E_FETCH"), "{}", story(&output));
        }
    }
    assert_eq!(case.root.read(RECORD), locked);
    assert_eq!(mtimes(&case.root.dir), before);
}

// --------------------------------------------------------------------------- U6 plan ---

/// U6. The preview's source line carries its resolved and refresh states (a recorded commit
/// with nothing to switch prints no source line); a preview records no commit; and a folder's
/// preview has no source line.
#[test]
fn u6_plan_source_line_states() {
    let case = case("u6-plan");
    let server = Server::start("single", "first");
    let first = run(&case, "plan", &[URL], &to(&server));
    assert_eq!(code(&first), Some(0), "{}", story(&first));
    let line = err(&first).lines().next().unwrap_or("").to_string();
    for part in [URL, "HEAD", FIRST, "resolved, not locked yet"] {
        assert!(line.contains(part), "{part}: {}", story(&first));
    }
    assert!(!case.root.exists("etc/lodi/host.lock"));
    assert!(!case.root.exists(RECORD), "a preview recorded the commit");

    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let locked = run(&case, "plan", &[URL], &to(&server));
    assert_eq!(code(&locked), Some(0), "{}", story(&locked));
    assert!(nothing(&locked), "{}", story(&locked));
    server.push("second");
    let lock_before = case.root.read(RECORD);
    let refresh = run(&case, "plan", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refresh), Some(0), "{}", story(&refresh));
    assert!(
        err(&refresh).contains(&format!("refresh {FIRST} -> {SECOND}")),
        "{}",
        story(&refresh)
    );
    assert_eq!(case.root.read(RECORD), lock_before);

    let folder = case.root.path("folder");
    fs::create_dir_all(&folder).unwrap();
    fs::write(folder.join("host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
    let directory = run(&case, "plan", &[&folder.display().to_string()], "");
    assert_eq!(code(&directory), Some(0), "{}", story(&directory));
    assert!(!err(&directory).contains(URL), "{}", story(&directory));
}

// ---------------------------------------------------------------- U7 the fetched home ---

/// U7 on Fedora (ff-1): a Fedora config with a home, committed and served live on loopback,
/// switches the host with dnf and then the home; a recorded re-switch asks nothing, and the
/// fetched tree is never written.
#[test]
fn u7_a_fetched_home_applies_after_the_host_on_fedora() {
    let case = Case::new("u7-home-fedora", Machine::fedora().offering(Pkg::new("jq")));
    case.root.write("etc/hostname", "box\n");
    let (uid, gid) = hostroot::ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n"),
    );
    let work = case.base().join("work");
    fs::create_dir_all(&work).unwrap();
    fs::write(
        work.join("host.toml"),
        "[host]\ndistro = \"fedora\"\n\n[packages.fedora]\nadd = [\"jq\"]\n",
    )
    .unwrap();
    fs::write(
        work.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\\n\"\n",
    )
    .unwrap();
    let commit = git_loopback::commit_all(&work, "fedora");
    let served = case.base().join("served");
    fs::create_dir_all(&served).unwrap();
    let server = git_loopback::Served::start(&served);
    server.push(&work, "hosts");
    let rewrite = server.rewrite(HTTPS, "hosts");
    let planned = run(&case, "switch", &[URL, "--dry-run"], &rewrite);
    assert_eq!(code(&planned), Some(0), "{}", story(&planned));
    assert!(err(&planned).contains("note"), "{}", story(&planned));
    let applied = run(&case, "switch", &[URL], &rewrite);
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&case, "jq"), "{}", story(&applied));
    assert_eq!(case.root.read("home/note"), "home\n");
    assert_eq!(record(&case)["rev"], commit.as_str());
    let fetched = tree(&case, URL, &commit);
    let before = mtimes(&fetched);
    let counter = Counter::start();
    let again = run(&case, "switch", &[URL], &nowhere(&counter));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(nothing(&again), "{}", story(&again));
    assert_eq!(counter.connections(), 0);
    assert_eq!(mtimes(&fetched), before);
}

// --------------------------------------------------------------------------- U8 pins ---

/// U8. `pin`, `unpin` and `import` never write a fetched tree: given a URL each is
/// `E_UNSUPPORTED`, before any request.
#[test]
fn u8_pin_unpin_and_import_refuse_a_url() {
    let case = case("u8-writers");
    let counter = Counter::start();
    for (verb, args) in [
        ("pin", vec!["jq", URL]),
        ("unpin", vec!["jq", URL]),
        ("pin", vec!["--all", "github:owner/hosts"]),
        ("import", vec![URL]),
    ] {
        let output = run(&case, verb, &args, &nowhere(&counter));
        assert_eq!(
            code(&output),
            Some(3),
            "{verb} {args:?}: {}",
            story(&output)
        );
        assert!(diagnosed(&output, "E_UNSUPPORTED"), "{}", story(&output));
    }
    assert_eq!(counter.connections(), 0);
    assert!(!cache(&case).exists());
}

// ------------------------------------------------------------------------ U9 aliases ---

/// U9, named red-first proof (1). `github:`, `gitlab:` and `codeberg:` reach the loopback server
/// through `LODI_FETCH_REWRITE` at their canonical HTTPS path, and each locks the same commit and
/// NAR as its expanded `git+https` URL, whose apply then needs no request.
#[test]
fn alias_resolves_through_fetch_rewrite() {
    for (alias, canonical, prefix) in [
        (
            "github:owner/hosts",
            "https://github.com/owner/hosts.git",
            "https://github.com/",
        ),
        (
            "gitlab:owner/hosts",
            "https://gitlab.com/owner/hosts.git",
            "https://gitlab.com/",
        ),
        (
            "codeberg:owner/hosts",
            "https://codeberg.org/owner/hosts.git",
            "https://codeberg.org/",
        ),
    ] {
        let case = case(&format!("u9-{}", &alias[..alias.find(':').unwrap()]));
        let server = Server::start("single", "first");
        let rewrite = server.rewrite(prefix, "/forge/");
        let applied = run(&case, "apply", &[alias], &rewrite);
        assert_eq!(code(&applied), Some(0), "{alias}: {}", story(&applied));
        let path = format!("/forge/{}", canonical.trim_start_matches(prefix));
        assert!(
            server.lines().iter().all(|line| line.contains(&path)),
            "{alias}: {:?}",
            server.lines()
        );
        let recorded = record(&case);
        let expanded = format!("git+{canonical}");
        assert_eq!(recorded["url"], expanded.as_str());
        assert_eq!(recorded["rev"], FIRST);

        let other = self::case(&format!(
            "u9-expanded-{}",
            &alias[..alias.find(':').unwrap()]
        ));
        let direct = run(&other, "apply", &[&expanded], &rewrite);
        assert_eq!(code(&direct), Some(0), "{}", story(&direct));
        assert_eq!(record(&other), recorded);

        let asked = server.connections();
        let same = run(&case, "apply", &[&expanded], &rewrite);
        assert_eq!(code(&same), Some(0), "{}", story(&same));
        assert!(nothing(&same), "{}", story(&same));
        assert_eq!(server.connections(), asked);
    }
}

/// U9. A third segment, `?` or `#` is `E_UNSUPPORTED`; a repository the forge answers 401, 403 or
/// 404 for is `E_FETCH`; and `./github:x` is a path.
#[test]
fn u9_malformed_aliases_private_repositories_and_paths() {
    let case = case("u9-refusals");
    let counter = Counter::start();
    let rewrite = format!("https://github.com/={}/", counter.url);
    for malformed in [
        "github:owner/hosts/box",
        "github:owner/hosts?ref=main",
        "gitlab:owner/hosts#box",
        "codeberg:owner",
    ] {
        let output = run(&case, "plan", &[malformed], &rewrite);
        assert_eq!(code(&output), Some(3), "{malformed}: {}", story(&output));
        assert!(diagnosed(&output, "E_UNSUPPORTED"), "{}", story(&output));
    }
    assert_eq!(counter.connections(), 0);
    for status in [401, 403, 404] {
        let refusing = Counter::answering(Some(status));
        let rewrite = format!("https://github.com/={}/", refusing.url);
        let output = run(&case, "plan", &["github:owner/private"], &rewrite);
        assert_eq!(code(&output), Some(5), "{status}: {}", story(&output));
        assert!(diagnosed(&output, "E_FETCH"), "{}", story(&output));
        assert!(refusing.connections() > 0);
    }
    let path = run(&case, "plan", &["./github:x"], &rewrite);
    assert_eq!(code(&path), Some(3), "{}", story(&path));
    assert!(diagnosed(&path, "E_NO_MANIFEST"), "{}", story(&path));
    assert_eq!(counter.connections(), 0);
    assert!(!cache(&case).exists());
}
