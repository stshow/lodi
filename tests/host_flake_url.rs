//! gu-1 (LD-401): a host plan and apply of a public git URL, locked and re-applied offline.
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

use fakehost::{Case, Machine, Pkg, err, out, story};
use git_host::{Counter, DIR, FIRST, HOME, HOME_TOOLS, LINK, PINS, SECOND, SUBMODULE, Server};

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
    case.root.path("var/lib/lodi/host/git")
}

fn tree(case: &Case, url: &str, commit: &str) -> PathBuf {
    cache(case)
        .join(lodi::util::sha256_hex(url.as_bytes()))
        .join(commit)
}

fn lock(case: &Case) -> serde_json::Value {
    serde_json::from_str(&case.root.read("etc/lodi/host.lock")).expect("the host lock")
}

fn installed(case: &Case, name: &str) -> bool {
    case.installed().contains(name)
}

/// Every path below the root with its modification time and length.
fn mtimes(root: &Path) -> BTreeMap<PathBuf, (i64, i64, u64)> {
    let mut seen = BTreeMap::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            seen.insert(path.clone(), (meta.mtime(), meta.mtime_nsec(), meta.len()));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    seen
}

// ---------------------------------------------------------------------------- U1 grammar ---

/// U1. Insecure forms and credentials are `E_INSECURE_URL`, malformed paths `E_UNSUPPORTED`,
/// and a selector flag without a URL, or `--rev` with `--refresh`, a usage error: each before any
/// request, as a counting listener every form points at shows.
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
    let hosts = case.root.path("hosts").display().to_string();
    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    for args in [
        vec!["--ref", "main"],
        vec!["--rev", FIRST],
        vec!["--refresh"],
        vec![hosts.as_str(), "--ref", "main"],
        vec![URL, "--rev", FIRST, "--refresh"],
    ] {
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
    assert!(!cache(&case).exists());
}

/// #297. Top-level `plan` and `apply` refuse `--ref` with `--rev` in either order as the host
/// verbs do: the same usage error and exit status, before any request.
#[test]
fn top_level_ref_with_rev_is_a_usage_error_before_any_request() {
    let case = case("u1-ref-with-rev");
    let counter = Counter::start();
    let rewrite = nowhere(&counter);
    for (args, second) in [
        ([URL, "--ref", "main", "--rev", FIRST], "--rev"),
        ([URL, "--rev", FIRST, "--ref", "main"], "--ref"),
    ] {
        for verb in ["plan", "apply"] {
            let host = run(&case, verb, &args, &rewrite);
            let _spawning = fakehost::spawning();
            let top = std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
                .arg(verb)
                .args(args)
                .arg("--root")
                .arg(&case.root.dir)
                .env("PATH", case.fake_path())
                .env("HOME", case.root.path("home"))
                .env("LODI_HOME", case.root.path("lodi-home"))
                .env("LODI_HOST_REQUIRE_ROOT", "1")
                .env("LODI_FETCH_REWRITE", &rewrite)
                .env("LODI_FETCH_ATTEMPTS", "1")
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert_eq!(code(&host), Some(2), "{}", story(&host));
            assert_eq!(code(&top), code(&host), "{verb} {args:?}: {}", story(&top));
            assert_eq!(
                err(&top),
                err(&host).replace(&format!("'host {verb} "), &format!("'{verb} ")),
                "{}",
                story(&top)
            );
            assert!(
                err(&top).contains(&format!("'{verb} {second}'")),
                "{}",
                story(&top)
            );
        }
    }
    assert_eq!(counter.connections(), 0, "a refusal made a request");
    assert!(!case.root.exists("etc/lodi/host.lock"));
    assert!(!cache(&case).exists());
}

// ------------------------------------------------------------ U2 layouts and selection ---

/// U2. A fetched tree with a top-level `host.toml` applies; one that is a directory of hosts
/// applies the host the root's hostname or `--host` names, and a miss lists what it holds.
#[test]
fn u2_a_top_level_host_and_a_directory_of_hosts_both_apply() {
    let single = case("u2-single");
    let server = Server::start("single", "first");
    let applied = run(&single, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&single, "jq"));

    let named = case("u2-dir");
    let dir = Server::start("dir", "only");
    let applied = run(&named, "apply", &[URL], &to(&dir));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&named, "jq") && !installed(&named, "tree"));
    let other = run(&named, "plan", &[URL, "--host", "other"], &to(&dir));
    assert_eq!(code(&other), Some(0), "{}", story(&other));
    assert!(out(&other).contains("tree"), "{}", story(&other));
    let line = out(&other).lines().next().unwrap_or("").to_string();
    assert!(
        line.contains("other") && line.contains(DIR),
        "{}",
        story(&other)
    );

    let missing = run(&named, "plan", &[URL, "--host", "absent"], &to(&dir));
    assert_eq!(code(&missing), Some(3), "{}", story(&missing));
    assert!(diagnosed(&missing, "E_NO_MANIFEST"), "{}", story(&missing));
    assert!(
        err(&missing).contains("box") && err(&missing).contains("other"),
        "{}",
        story(&missing)
    );
    assert!(tree(&named, URL, DIR).join("box/host.toml").is_file());
}

// ------------------------------------------------------------------------- U3 the cache ---

/// U3. The fetched tree lives at `var/lib/lodi/host/git/<URL sha256>/<commit>/` below the root,
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
    let entries: Vec<_> = fs::read_dir(first.parent().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from(FIRST)]);

    server.push("second");
    let refreshed = run(&case, "apply", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refreshed), Some(0), "{}", story(&refreshed));
    assert!(installed(&case, "pv"));
    assert!(tree(&case, URL, SECOND).is_dir());
    assert!(!first.exists(), "the tree no longer locked was not pruned");

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

/// U4. A URL apply records the git table (url, ref, rev, narHash) in a schema bumped for it
/// alone; a directory apply's record stays the earlier schema without it; and after the URL
/// apply a bare apply is `E_DECLINED` naming the URL.
#[test]
fn u4_the_lock_records_the_git_source_and_fences_a_bare_apply() {
    let case = case("u4-lock");
    case.root.write(
        "hosts/box/host.toml",
        "[host]\ndistro = \"debian\"\n\n[packages]\ncommon = [\"tree\"]\n",
    );
    let hosts = case.root.path("hosts").display().to_string();
    let directory = run(&case, "apply", &[&hosts], "");
    assert_eq!(code(&directory), Some(0), "{}", story(&directory));
    let record = lock(&case);
    assert_eq!(record["version"], 2);
    assert!(record.get("git").is_none(), "{record}");

    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let record = lock(&case);
    assert_eq!(record["version"], 3, "{record}");
    assert_eq!(record["format"], "lodi-host-lock/3");
    assert_eq!(record["source"], URL);
    let git = &record["git"];
    assert_eq!(git["url"], URL);
    assert_eq!(git["ref"], "HEAD");
    assert_eq!(git["rev"], FIRST);
    let nar = git["narHash"].as_str().unwrap();
    assert!(nar.starts_with("sha256:") && nar.len() == 71, "{nar}");

    case.set_manifest("[host]\ndistro = \"debian\"\n");
    let bare = run(&case, "apply", &[], "");
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
    assert!(diagnosed(&bare, "E_DECLINED"), "{}", story(&bare));
    assert!(err(&bare).contains(URL), "{}", story(&bare));
}

// ------------------------------------------------------------------------ U5 offline ---

/// U5, named red-first proof (2). After a locked apply the server is stopped; a re-apply works
/// from host.lock and the verified cache alone, with zero connections, no mtime moved below the
/// root and `locked, no request` printed.
#[test]
fn offline_reapply_from_the_lock_alone() {
    let case = case("u5-offline");
    let server = Server::start("single", "first");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let asked = server.connections();
    drop(server);

    let before = mtimes(&case.root.dir);
    let counter = Counter::start();
    let again = run(&case, "apply", &[URL], &nowhere(&counter));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert!(
        out(&again).contains("locked, no request"),
        "{}",
        story(&again)
    );
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
    assert!(out(&again).contains("locked, fetched"), "{}", story(&again));
    let asked = &server.lines()[before..];
    assert_eq!(asked.len(), 2, "one advertisement and one fetch: {asked:?}");
    assert!(tree(&case, URL, FIRST).is_dir());
    assert!(!installed(&case, "pv"), "the pushed change was applied");

    let mut record = lock(&case);
    record["git"]["narHash"] = serde_json::json!(format!("sha256:{}", "0".repeat(64)));
    case.root.write(
        "etc/lodi/host.lock",
        &format!("{}\n", serde_json::to_string_pretty(&record).unwrap()),
    );
    fs::remove_dir_all(tree(&case, URL, FIRST)).unwrap();
    let mismatch = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&mismatch), Some(5), "{}", story(&mismatch));
    assert!(
        diagnosed(&mismatch, "E_HASH_MISMATCH"),
        "{}",
        story(&mismatch)
    );
    let left: Vec<_> = fs::read_dir(tree(&case, URL, FIRST).parent().unwrap())
        .unwrap()
        .collect();
    assert!(left.is_empty(), "a mismatched tree was stored: {left:?}");
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
    assert_eq!(lock(&case)["git"]["rev"], FIRST, "a push moved the lock");

    let refreshed = run(&case, "apply", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refreshed), Some(0), "{}", story(&refreshed));
    assert!(
        out(&refreshed).contains(&format!("refresh {FIRST} -> {SECOND}")),
        "{}",
        story(&refreshed)
    );
    assert_eq!(lock(&case)["git"]["rev"], SECOND);
    assert!(installed(&case, "pv"));

    let pinned = run(&case, "apply", &[URL, "--rev", FIRST], &to(&server));
    assert_eq!(code(&pinned), Some(0), "{}", story(&pinned));
    assert_eq!(lock(&case)["git"]["rev"], FIRST);
    let kept = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&kept), Some(0), "{}", story(&kept));
    assert_eq!(lock(&case)["git"]["rev"], FIRST);

    let stable = run(&case, "apply", &[URL, "--ref", "stable"], &to(&server));
    assert_eq!(code(&stable), Some(0), "{}", story(&stable));
    assert_eq!(lock(&case)["git"]["ref"], "stable");
    assert_eq!(lock(&case)["git"]["rev"], FIRST);
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
    let locked = case.root.read("etc/lodi/host.lock");
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
    assert_eq!(case.root.read("etc/lodi/host.lock"), locked);
    assert_eq!(mtimes(&case.root.dir), before);
}

// --------------------------------------------------------------------------- U6 plan ---

/// U6. The plan's source line carries each of its four states; a plan writes no lock and no
/// state; and a directory plan has no source line.
#[test]
fn u6_plan_source_line_states() {
    let case = case("u6-plan");
    let server = Server::start("single", "first");
    let first = run(&case, "plan", &[URL], &to(&server));
    assert_eq!(code(&first), Some(0), "{}", story(&first));
    let line = out(&first).lines().next().unwrap_or("").to_string();
    for part in [URL, "HEAD", FIRST, "resolved, not locked yet"] {
        assert!(line.contains(part), "{part}: {}", story(&first));
    }
    assert!(!case.root.exists("etc/lodi/host.lock"));
    let state: Vec<_> = fs::read_dir(case.root.path("var/lib/lodi/host"))
        .map(|d| d.map(|e| e.unwrap().file_name()).collect())
        .unwrap_or_default();
    assert_eq!(
        state,
        vec![std::ffi::OsString::from("git")],
        "a plan wrote state"
    );

    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    let locked = run(&case, "plan", &[URL], &to(&server));
    assert!(
        out(&locked).contains("locked, no request"),
        "{}",
        story(&locked)
    );
    fs::remove_dir_all(tree(&case, URL, FIRST)).unwrap();
    let fetched = run(&case, "plan", &[URL], &to(&server));
    assert!(
        out(&fetched).contains("locked, fetched"),
        "{}",
        story(&fetched)
    );
    server.push("second");
    let lock_before = case.root.read("etc/lodi/host.lock");
    let refresh = run(&case, "plan", &[URL, "--refresh"], &to(&server));
    assert_eq!(code(&refresh), Some(0), "{}", story(&refresh));
    assert!(
        out(&refresh).contains(&format!("refresh {FIRST} -> {SECOND}")),
        "{}",
        story(&refresh)
    );
    assert_eq!(case.root.read("etc/lodi/host.lock"), lock_before);

    case.root
        .write("hosts/box/host.toml", "[host]\ndistro = \"debian\"\n");
    let hosts = case.root.path("hosts").display().to_string();
    let directory = run(&case, "plan", &[&hosts], "");
    assert_eq!(code(&directory), Some(0), "{}", story(&directory));
    assert!(!out(&directory).contains(URL), "{}", story(&directory));
}

// ---------------------------------------------------------------- U7 the fetched home ---

/// A case whose root's passwd names the invoking uid `sample`, home `/people/sample`.
fn with_sample(name: &str) -> Case {
    let case = case(name);
    let (uid, gid) = hostroot::ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    fs::create_dir_all(case.root.path("people/sample")).unwrap();
    case
}

/// U7. A fetched `home/<login>/` goes through fh-1's home part (LD-399): the plan shows it, the
/// apply writes the host and then the home, the home state names the URL, and a locked re-apply
/// with no request writes nothing into the fetched tree.
#[test]
fn u7_a_fetched_home_applies_after_the_host_and_the_tree_is_never_written() {
    let case = with_sample("u7-home");
    let server = Server::start("home", "only");
    let planned = run(&case, "plan", &[URL], &to(&server));
    assert_eq!(code(&planned), Some(0), "{}", story(&planned));
    assert!(out(&planned).contains("home sample"), "{}", story(&planned));
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&case, "jq"), "{}", story(&applied));
    assert_eq!(case.root.read("people/sample/note"), "home\n");
    let state = case
        .root
        .read("people/sample/.local/share/lodi/home-scope/state.json");
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["source"], URL);
    // A bare home verb is fenced by that URL, and is told the verb that applies it.
    let bare = std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "plan"])
        .env("HOME", case.root.path("people/sample"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
    assert!(
        err(&bare).contains(&format!("host apply {URL}")),
        "{}",
        story(&bare)
    );
    let fetched = tree(&case, URL, HOME);
    let before = mtimes(&fetched);
    let counter = Counter::start();
    let again = run(&case, "apply", &[URL], &nowhere(&counter));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert_eq!(counter.connections(), 0);
    assert_eq!(mtimes(&fetched), before);
    assert_eq!(
        before.keys().filter(|path| path.is_file()).count(),
        2,
        "{before:?}"
    );
}

/// U7 on Fedora (ff-1): a Fedora host folder with the home of `sample`, committed and served
/// live on loopback, plans and applies the host with dnf and then the home, which names the URL;
/// a locked re-apply asks nothing.
#[test]
fn u7_a_fetched_home_applies_after_the_host_on_fedora() {
    let case = Case::new("u7-home-fedora", Machine::fedora().offering(Pkg::new("jq")));
    case.root.write("etc/hostname", "box\n");
    let (uid, gid) = hostroot::ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh\n"),
    );
    fs::create_dir_all(case.root.path("people/sample")).unwrap();
    let work = case.base().join("work");
    fs::create_dir_all(work.join("home/sample")).unwrap();
    fs::write(
        work.join("host.toml"),
        "[host]\ndistro = \"fedora\"\n\n[packages.fedora]\nadd = [\"jq\"]\n",
    )
    .unwrap();
    fs::write(
        work.join("home/sample/home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\\n\"\n",
    )
    .unwrap();
    let commit = git_loopback::commit_all(&work, "fedora");
    let served = case.base().join("served");
    fs::create_dir_all(&served).unwrap();
    let server = git_loopback::Served::start(&served);
    server.push(&work, "hosts");
    let rewrite = server.rewrite(HTTPS, "hosts");
    let planned = run(&case, "plan", &[URL], &rewrite);
    assert_eq!(code(&planned), Some(0), "{}", story(&planned));
    assert!(out(&planned).contains("home sample"), "{}", story(&planned));
    let applied = run(&case, "apply", &[URL], &rewrite);
    assert_eq!(code(&applied), Some(0), "{}", story(&applied));
    assert!(installed(&case, "jq"), "{}", story(&applied));
    assert_eq!(case.root.read("people/sample/note"), "home\n");
    let state = case
        .root
        .read("people/sample/.local/share/lodi/home-scope/state.json");
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["source"], URL);
    assert!(case.root.read("etc/lodi/host.lock").contains(&commit));
    let counter = Counter::start();
    let again = run(&case, "apply", &[URL], &nowhere(&counter));
    assert_eq!(code(&again), Some(0), "{}", story(&again));
    assert_eq!(counter.connections(), 0);
}

/// A bare `lodi home plan` as `sample`, with no source named.
fn bare_home_plan(case: &Case) -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "plan"])
        .env("HOME", case.root.path("people/sample"))
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap()
}

/// U7. The home state names the URL that last applied the home, even when that apply had no home
/// change to make, so a bare home verb names the host apply that now owns it.
#[test]
fn u7_the_home_state_follows_the_url_that_last_applied_it() {
    let case = with_sample("u7-home-moves");
    let server = Server::start("home", "only");
    let alias = "git+https://github.com/owner/hosts.git";
    let both = format!(
        "{};{}",
        to(&server),
        server.rewrite("https://github.com/owner/hosts.git", "/hosts.git")
    );
    let first = run(&case, "apply", &[URL], &both);
    assert_eq!(code(&first), Some(0), "{}", story(&first));
    let moved = run(&case, "apply", &["github:owner/hosts"], &both);
    assert_eq!(code(&moved), Some(0), "{}", story(&moved));
    let state = case
        .root
        .read("people/sample/.local/share/lodi/home-scope/state.json");
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["source"], alias);
    let bare = bare_home_plan(&case);
    assert_eq!(code(&bare), Some(11), "{}", story(&bare));
    assert!(
        err(&bare).contains(&format!("host apply {alias}")),
        "{}",
        story(&bare)
    );
}

/// U7. A fetched home that declares tools with no `home.lock` beside it is `E_LOCK_STALE` before
/// the host or the home is written, and nothing is written into the fetched tree; `--no-home`
/// applies the host alone.
#[test]
fn u7_a_fetched_home_without_its_tools_lock_refuses_before_any_write() {
    let case = with_sample("u7-home-tools");
    let server = Server::start("home-tools", "only");
    let applied = run(&case, "apply", &[URL], &to(&server));
    assert_eq!(code(&applied), Some(10), "{}", story(&applied));
    assert!(diagnosed(&applied, "E_LOCK_STALE"), "{}", story(&applied));
    assert!(!installed(&case, "jq"), "{}", story(&applied));
    assert!(!case.root.exists("etc/lodi/host.lock"));
    assert!(!case.root.exists("people/sample/.local"));
    let fetched = tree(&case, URL, HOME_TOOLS);
    assert!(!fetched.join("home/sample/home.lock").exists());
    let host_only = run(&case, "apply", &[URL, "--no-home"], &to(&server));
    assert_eq!(code(&host_only), Some(0), "{}", story(&host_only));
    assert!(installed(&case, "jq"));
    assert!(!fetched.join("home/sample/home.lock").exists());
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

/// U8. A fetched tree's `pins.lock` is read as a host directory's is, under the cache's trust
/// rules, and never written: the same bytes in a directory and in a fetched tree meet the same
/// refusal (here a later format, `E_LOCK_VERSION`), and the tree keeps them.
#[test]
fn u8_a_fetched_pins_lock_is_read_as_a_directory_one() {
    let case = case("u8-pins");
    let later = "{\n  \"format\": \"lodi-host-pins/99\",\n  \"version\": 99\n}\n";
    case.root.write(
        "hosts/box/host.toml",
        "[host]\ndistro = \"debian\"\n\n[packages]\ncommon = [\"jq\"]\n",
    );
    case.root.write("hosts/box/pins.lock", later);
    let hosts = case.root.path("hosts").display().to_string();
    let directory = run(&case, "plan", &[&hosts], "");
    let server = Server::start("pins", "only");
    let fetched = run(&case, "plan", &[URL], &to(&server));
    for output in [&directory, &fetched] {
        assert_eq!(code(output), Some(4), "{}", story(output));
        assert!(diagnosed(output, "E_LOCK_VERSION"), "{}", story(output));
    }
    let kept = fs::read_to_string(tree(&case, URL, PINS).join("pins.lock")).unwrap();
    assert_eq!(kept, later);
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
        let record = lock(&case);
        let expanded = format!("git+{canonical}");
        assert_eq!(record["git"]["url"], expanded.as_str());
        assert_eq!(record["git"]["rev"], FIRST);

        let other = self::case(&format!(
            "u9-expanded-{}",
            &alias[..alias.find(':').unwrap()]
        ));
        let direct = run(&other, "apply", &[&expanded], &rewrite);
        assert_eq!(code(&direct), Some(0), "{}", story(&direct));
        assert_eq!(lock(&other)["git"], record["git"]);

        let asked = server.connections();
        let same = run(&case, "apply", &[&expanded], &rewrite);
        assert_eq!(code(&same), Some(0), "{}", story(&same));
        assert!(
            out(&same).contains("locked, no request"),
            "{}",
            story(&same)
        );
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
