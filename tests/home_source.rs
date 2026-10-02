//! The home part read from a config folder given by path (`lodi switch --home PATH`, 1.x's
//! `lodi home apply SOURCE`; LD-518): what it applies, what it records, and where a home's
//! sources and tool lock live in a config of several hosts. Each case runs in a
//! [`support::home_env`] on the gate's machine.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::process::Output;

use support::{HomeEnv, home_env};

/// `lodi home VERB SOURCE` as the 2.0 switch that keeps it ([`HomeEnv::lodi`]).
fn run(env: &HomeEnv, verb: &str, source: &std::path::Path) -> Output {
    env.lodi(&["home", verb, source.to_str().unwrap()])
        .output()
        .unwrap()
}

#[test]
fn h6_apply() {
    let env = home_env("h6-home-source");
    let home = env.home();
    let source = env.root().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"sourced\"\n",
    )
    .unwrap();
    let output = run(&env, "apply", &source);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fs::read(home.join("note")).unwrap(), b"sourced");
    let record: serde_json::Value =
        serde_json::from_slice(&fs::read(env.share().join("lodi/home-scope/state.json")).unwrap())
            .unwrap();
    assert_eq!(record["source"], source.to_str().unwrap());
}

#[test]
fn h6_plan_direct_source_without_writing() {
    let env = home_env("h6-plan-source");
    let home = env.home();
    let source = env.root().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"planned\"\n",
    )
    .unwrap();
    let output = run(&env, "plan", &source);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(support::home_part(&output).contains("note"), "{output:?}");
    assert!(!home.join("note").exists());
    assert!(!source.join("lodi.lock").exists());
}

#[test]
fn h6_status_reports_named_source() {
    let env = home_env("h6-status-source");
    let source = env.root().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"managed\"\n",
    )
    .unwrap();
    let applied = run(&env, "apply", &source);
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    // What 1.x's status called `ok` is a preview with nothing to switch.
    let status = run(&env, "plan", &source);
    assert!(support::nothing(&status), "{status:?}");
}

/// H6: a home a `config.toml` names resolves its sources relative to its own folder, and its
/// tool lock is its section of the config's root `lodi.lock`, never a `home.lock`.
#[test]
fn h6_sources_resolve_in_the_selected_home_and_its_lock_lands_in_the_root_lock() {
    let env = home_env("h6-lock-beside");
    let home = env.home();
    let config = env.root().join("config");
    let selected = config.join("box").join("homes").join("sample");
    fs::create_dir_all(selected.join("inputs")).unwrap();
    fs::write(
        config.join("box/host.toml"),
        "[host]\ndistro = \"debian\"\n",
    )
    .unwrap();
    fs::write(
        config.join("config.toml"),
        "[hosts.box]\nhost = \"box/host.toml\"\nhomes.sample = \"box/homes/sample/home.toml\"\n",
    )
    .unwrap();
    fs::write(selected.join("inputs/note"), "from inputs\n").unwrap();
    let url = "https://fixtures.test/hello";
    let body = b"#!/bin/sh\necho hello\n";
    let server = support::Server::start(BTreeMap::from([(url.to_string(), body.to_vec())]));
    fs::write(
        selected.join("home.toml"),
        format!(
            "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\nsource = \"inputs/note\"\n\n\
             [tools.hello]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{}\"\n\
             format = \"binary\"\npath = [\"bin\"]\n",
            lodi::util::sha256_hex(body)
        ),
    )
    .unwrap();
    let output = env
        .lodi(&["home", "apply", config.to_str().unwrap()])
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        fs::read_to_string(home.join("note")).unwrap(),
        "from inputs\n"
    );
    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(config.join("lodi.lock")).unwrap()).unwrap();
    assert!(
        root["homes"]["box/homes/sample/home.toml"]["tools"]["hello"].is_object(),
        "{root}"
    );
    assert!(!selected.join("home.lock").exists());
    assert!(!env.config().join("lodi/home.lock").exists());
}

/// H6: a path that is not there is `E_NO_MANIFEST`, as for the host part, not a trust refusal.
#[test]
fn h6_a_missing_source_is_no_manifest() {
    let env = home_env("h6-missing-source");
    let dir = env.root();
    for verb in ["plan", "apply"] {
        let output = run(&env, verb, &dir.join("absent"));
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("E_NO_MANIFEST"), "{verb}: {error}");
        assert!(
            error.contains("is not a folder that exists"),
            "{verb}: {error}"
        );
    }
    assert!(!dir.join("absent").exists());
}

/// H7: the fence names the source the last switch read, and naming it is the way on. A bare
/// `lodi switch --home` reads the remembered folder; one found otherwise (`LODI_REPO`, here) that
/// is not the last switch's source is `E_DECLINED` and changes nothing (LD-524).
#[test]
fn h7_the_fence_names_the_source_and_naming_it_proceeds() {
    let env = home_env("h7-fence-names");
    let source = env.root().join("source");
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"sourced\"\n",
    )
    .unwrap();
    let home = |args: &[&str]| {
        let args: Vec<&str> = ["home"].iter().chain(args).copied().collect();
        env.lodi(&args).output().unwrap()
    };
    let named = home(&["apply", source.to_str().unwrap()]);
    assert_eq!(named.status.code(), Some(0), "{named:?}");
    let other = env.config().join("lodi");
    fs::create_dir_all(&other).unwrap();
    fs::write(other.join("home.toml"), "[home]\nversion = \"1\"\n").unwrap();
    for verb in ["plan", "apply"] {
        let bare = env
            .lodi(&["home", verb])
            .env("LODI_REPO", &other)
            .output()
            .unwrap();
        assert_eq!(bare.status.code(), Some(11), "{verb}: {bare:?}");
        let error = String::from_utf8_lossy(&bare.stderr);
        assert!(error.contains("E_DECLINED"), "{error}");
        assert!(error.contains(source.to_str().unwrap()), "{error}");
        assert_eq!(fs::read(env.home().join("note")).unwrap(), b"sourced");
    }
    let again = home(&["apply", source.to_str().unwrap()]);
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert!(support::nothing(&again), "{again:?}");
}
