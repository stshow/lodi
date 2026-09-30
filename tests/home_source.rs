mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

#[test]
fn h7_bare_home_apply_refuses_state_from_named_source() {
    let dir = support::scratch("h7-home-fence");
    let home = dir.join("person");
    fs::create_dir_all(home.join(".config/lodi")).unwrap();
    fs::write(
        home.join(".config/lodi/home.toml"),
        "[home]\nversion = \"1\"\n",
    )
    .unwrap();
    let state_dir = home.join(".local/share/lodi/home-scope");
    fs::create_dir_all(&state_dir).unwrap();
    let source = serde_json::to_string(&dir.join("chosen").display().to_string()).unwrap();
    fs::write(
        state_dir.join("state.json"),
        format!(r#"{{"version":3,"source":{source},"files":{{}},"directories":[]}}"#),
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "apply"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(11), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_DECLINED"));
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_apply() {
    let dir = support::scratch("h6-home-source");
    let home = dir.join("person");
    let source = dir.join("source");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"sourced\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "apply"])
        .arg(&source)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fs::read(home.join("note")).unwrap(), b"sourced");
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join(".local/share/lodi/home-scope/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["source"], source.to_str().unwrap());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_apply_selects_home_in_named_host() {
    let dir = support::scratch("h6-host-source");
    let home = dir.join("person");
    let hosts = dir.join("hosts");
    let login = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let selected = hosts.join("box").join("home").join(login);
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&selected).unwrap();
    fs::write(hosts.join("box/host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
    fs::write(
        selected.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"host source\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "apply"])
        .arg(&hosts)
        .args(["--host", "box"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(fs::read(home.join("note")).unwrap(), b"host source");
    let record: serde_json::Value = serde_json::from_slice(
        &fs::read(home.join(".local/share/lodi/home-scope/state.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(record["source"], selected.to_str().unwrap());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_plan_direct_source_without_writing() {
    let dir = support::scratch("h6-plan-source");
    let home = dir.join("person");
    let source = dir.join("source");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"planned\"\n",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "plan"])
        .arg(&source)
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("note"));
    assert!(!home.join("note").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_status_reports_named_source() {
    let dir = support::scratch("h6-status-source");
    let home = dir.join("person");
    let source = dir.join("source");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"managed\"\n",
    )
    .unwrap();
    let command = |verb: &str| {
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(["home", verb])
            .arg(&source)
            .env("HOME", &home)
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("LODI_HOME")
            .output()
            .unwrap()
    };
    assert_eq!(command("apply").status.code(), Some(0));
    let status = command("status");
    assert_eq!(status.status.code(), Some(0), "{status:?}");
    let text = String::from_utf8_lossy(&status.stdout);
    assert!(text.contains("ok") && text.contains("note"), "{text}");
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_init_writes_stub_in_named_host() {
    let dir = support::scratch("h6-init-source");
    let home = dir.join("person");
    let source = dir.join("hosts");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(source.join("box")).unwrap();
    fs::write(
        source.join("box/host.toml"),
        "[host]\ndistro = \"debian\"\n",
    )
    .unwrap();
    let login = String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string();
    let target = source
        .join("box")
        .join("home")
        .join(login)
        .join("home.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "init"])
        .arg(&source)
        .args(["--host", "box"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(fs::read_to_string(target).unwrap().contains("[home]"));
    assert!(!home.join(".config/lodi/home.toml").exists());
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h7_noop_apply_keeps_schema_two_state_bytes_and_inode() {
    let dir = support::scratch("h7-noop-state");
    let home = dir.join("person");
    let config = home.join(".config/lodi");
    let state = home.join(".local/share/lodi/home-scope/state.json");
    fs::create_dir_all(&config).unwrap();
    fs::create_dir_all(state.parent().unwrap()).unwrap();
    fs::write(config.join("home.toml"), "[home]\nversion = \"1\"\n").unwrap();
    let bytes = br#"{"version":2,"files":{},"directories":[]}"#;
    fs::write(&state, bytes).unwrap();
    let inode = fs::metadata(&state).unwrap().ino();
    let applied = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "apply"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert_eq!(fs::read(&state).unwrap(), bytes);
    assert_eq!(fs::metadata(&state).unwrap().ino(), inode);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_init_refuses_link_in_named_host_home_path() {
    let dir = support::scratch("h6-init-link");
    let home = dir.join("person");
    let hosts = dir.join("hosts");
    let decoy = dir.join("decoy");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(hosts.join("box")).unwrap();
    fs::create_dir_all(&decoy).unwrap();
    fs::write(hosts.join("box/host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
    std::os::unix::fs::symlink(&decoy, hosts.join("box/home")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "init"])
        .arg(&hosts)
        .args(["--host", "box"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("E_PATH_ESCAPE"));
    assert_eq!(fs::read_dir(&decoy).unwrap().count(), 0);
    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn h6_host_flag_needs_a_non_option_name() {
    let dir = support::scratch("h6-host-name");
    let home = dir.join("person");
    let hosts = dir.join("hosts");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&hosts).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["home", "plan"])
        .arg(&hosts)
        .args(["--host", "--bogus"])
        .env("HOME", &home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("LODI_HOME")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    fs::remove_dir_all(dir).unwrap();
}

fn login() -> String {
    String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string()
}

fn home_command(home: &std::path::Path, args: &[&std::ffi::OsStr]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
    command
        .arg("home")
        .args(args)
        .env("HOME", home)
        .env_remove("XDG_CONFIG_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_STATE_HOME")
        .env_remove("LODI_HOME");
    command
}

/// H6: a selected home's sources resolve relative to `home/<login>/`, and its `home.lock` is
/// written beside the manifest it was read from, never in `~/.config/lodi`.
#[test]
fn h6_sources_resolve_in_the_selected_home_and_its_lock_lands_in_the_root_lock() {
    let dir = support::scratch("h6-lock-beside");
    let _cleanup = support::RemoveOnDrop(dir.clone());
    let home = dir.join("person");
    let hosts = dir.join("hosts");
    let selected = hosts.join("box").join("home").join(login());
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(selected.join("inputs")).unwrap();
    fs::write(hosts.join("box/host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
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
    let output = home_command(&home, &["apply".as_ref(), hosts.as_os_str()])
        .args(["--host", "box"])
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(
        fs::read_to_string(home.join("note")).unwrap(),
        "from inputs\n"
    );
    // The home lives in a repository, so its tool lock is its section of the root lock
    // (LD-416; handoff F-5 and F-15: home.lock "initially", the root lodi.lock after the verbs).
    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(hosts.join("lodi.lock")).unwrap()).unwrap();
    assert!(root["homes"][format!("box/home/{}", login())]["tools"]["hello"].is_object());
    assert!(!selected.join("home.lock").exists());
    assert!(!home.join(".config/lodi/home.lock").exists());
}

/// H7: the fence names the source the last apply read, and naming it is the way on.
#[test]
fn h7_the_fence_names_the_source_and_naming_it_proceeds() {
    let dir = support::scratch("h7-fence-names");
    let home = dir.join("person");
    let source = dir.join("source");
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&source).unwrap();
    fs::write(
        source.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"sourced\"\n",
    )
    .unwrap();
    let named = home_command(&home, &["apply".as_ref(), source.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(named.status.code(), Some(0), "{named:?}");
    for verb in ["plan", "apply", "status"] {
        let bare = home_command(&home, &[verb.as_ref()]).output().unwrap();
        assert_eq!(bare.status.code(), Some(11), "{verb}: {bare:?}");
        let error = String::from_utf8_lossy(&bare.stderr);
        assert!(error.contains("E_DECLINED"), "{error}");
        assert!(error.contains(source.to_str().unwrap()), "{error}");
    }
    let again = home_command(&home, &["apply".as_ref(), source.as_os_str()])
        .output()
        .unwrap();
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert!(
        String::from_utf8_lossy(&again.stdout).contains("nothing to do"),
        "{again:?}"
    );
    fs::remove_dir_all(dir).unwrap();
}

/// H6: a SOURCE that is not there is `E_NO_MANIFEST`, as for the host verbs, not a trust refusal.
#[test]
fn h6_a_missing_source_is_no_manifest() {
    let dir = support::scratch("h6-missing-source");
    let home = dir.join("person");
    fs::create_dir_all(&home).unwrap();
    for verb in ["plan", "apply", "status", "init"] {
        let output = home_command(&home, &[verb.as_ref(), dir.join("absent").as_os_str()])
            .output()
            .unwrap();
        let error = String::from_utf8_lossy(&output.stderr);
        assert!(error.contains("E_NO_MANIFEST"), "{verb}: {error}");
        assert!(error.contains("does not exist"), "{verb}: {error}");
    }
    assert!(!dir.join("absent").exists());
    fs::remove_dir_all(dir).unwrap();
}
