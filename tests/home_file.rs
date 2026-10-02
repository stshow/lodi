//! `[home.file]` and `[home.xdg_config]` through the built binary (M-Home,
//! `docs/design/HOME_PROGRAMS.md` §3.2, §4.3, §10): a seed is written once and never managed
//! again, identical bytes are adopted without a write, a drifted file is refused, and a backed-up
//! original comes back when its entry goes.
//!
//! Every case runs in a throwaway set of roots through `support::home_env`, which clears the
//! environment and refuses to hand a child the ambient `HOME`. Offline and deterministic.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use lodi::diag::Diagnostic;
use lodi::home::manifest::{FileState, HomeManifest, OnRemove, Origin, parse_home_manifest_in};
use lodi::home::programs::{self, RenderEnv};
use lodi::roots::Roots;
use serde::Deserialize;

use support::{HomeEnv, home_env, home_gate, home_part};

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = env.config().join("lodi");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

/// `lodi ARGS` with a 1.x home verb read as `lodi switch --home` (LD-518).
fn run(env: &HomeEnv, args: &[&str]) -> Output {
    env.lodi(args).output().expect("the lodi binary runs")
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8(bytes.to_vec()).unwrap()
}

/// The ledger lines naming `path`.
fn writes_to(env: &HomeEnv, path: &Path) -> Vec<(String, PathBuf)> {
    home_gate()
        .ledger_lines()
        .into_iter()
        .filter(|(_, p)| p.starts_with(env.root()) && p == path)
        .collect()
}

const HEAD: &str = "[home]\nversion = \"1\"\n";

#[test]
fn a_seed_is_written_once_and_never_compared_again() {
    let env = home_env("file-seed");
    write_manifest(
        &env,
        &format!(
            "{HEAD}[home.xdg_config.\"demo/seed.conf\"]\ntext = \"start\\n\"\nstate = \"seed\"\n"
        ),
    );
    let target = env.home().join(".config/demo/seed.conf");
    let plan = run(&env, &["home", "plan"]);
    assert!(
        home_part(&plan).contains("seed      .config/demo/seed.conf"),
        "{}",
        text(&plan.stderr)
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&target).unwrap(), "start\n");

    // The user edits it: no drift, no write, and status is clean.
    fs::write(&target, "edited\n").unwrap();
    let before = writes_to(&env, &target).len();
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&target).unwrap(), "edited\n");
    assert_eq!(
        writes_to(&env, &target).len(),
        before,
        "a seed is written once"
    );
    let status = run(&env, &["home", "status"]);
    assert_eq!(
        status.status.code(),
        Some(0),
        "{}{}",
        text(&status.stdout),
        text(&status.stderr)
    );

    // Its entry goes: the changed seed stays.
    write_manifest(&env, HEAD);
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&target).unwrap(), "edited\n");
}

#[test]
fn a_seed_never_overwrites_a_file_already_there() {
    let env = home_env("file-seed-there");
    fs::write(env.home().join(".seedrc"), "mine\n").unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[home.file.\".seedrc\"]\ntext = \"theirs\\n\"\nstate = \"seed\"\n"),
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        fs::read_to_string(env.home().join(".seedrc")).unwrap(),
        "mine\n"
    );
}

#[test]
fn identical_bytes_are_adopted_without_a_write_and_then_managed() {
    let env = home_env("file-adopt");
    let target = env.home().join(".adoptrc");
    fs::write(&target, "same\n").unwrap();
    fs::set_permissions(&target, std::os::unix::fs::PermissionsExt::from_mode(0o644)).unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[home.file.\".adoptrc\"]\ntext = \"same\\n\"\n"),
    );
    let plan = run(&env, &["home", "plan"]);
    assert!(
        home_part(&plan).contains("adopt     .adoptrc"),
        "{}",
        text(&plan.stderr)
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(
        writes_to(&env, &target).is_empty(),
        "adopt writes nothing to the file"
    );

    // Now it is managed: an edit is drift, refused with exit 8, and nothing is written.
    fs::write(&target, "edited\n").unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[home.file.\".adoptrc\"]\ntext = \"new\\n\"\n"),
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(8), "{}", text(&out.stderr));
    assert!(
        text(&out.stderr).contains("E_DRIFT"),
        "{}",
        text(&out.stderr)
    );
    assert_eq!(fs::read_to_string(&target).unwrap(), "edited\n");
}

#[test]
fn a_backed_up_original_is_restored_when_its_entry_goes() {
    let env = home_env("file-restore");
    let target = env.home().join(".config/demo/app.conf");
    fs::create_dir_all(target.parent().unwrap()).unwrap();
    fs::write(&target, "original\n").unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[home.xdg_config.\"demo/app.conf\"]\ntext = \"lodi\\n\"\nbackup = true\n"),
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&target).unwrap(), "lodi\n");

    write_manifest(&env, HEAD);
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(fs::read_to_string(&target).unwrap(), "original\n");
}

#[test]
fn a_duplicate_target_is_refused_before_anything_is_planned() {
    let env = home_env("file-dup");
    write_manifest(
        &env,
        &format!("{HEAD}[home.file.\".x\"]\ntext = \"a\"\n[home.file.\".x/\"]\ntext = \"b\"\n"),
    );
    let out = run(&env, &["home", "plan"]);
    assert_ne!(out.status.code(), Some(0));
    assert!(
        text(&out.stderr).contains("E_DUP_RESOURCE"),
        "{}",
        text(&out.stderr)
    );
    assert!(text(&out.stdout).is_empty());
}

// ------------------------------------------------------------------------------ the loader ---

fn roots(env: &HomeEnv) -> Roots {
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

fn load(env: &HomeEnv, text: &str) -> Result<HomeManifest, Vec<Diagnostic>> {
    let roots = roots(env);
    parse_home_manifest_in(
        text,
        "home.toml",
        &roots.config_root(),
        &RenderEnv::of(&roots),
        programs::registry(),
    )
    .map_err(|e| e.diagnostics)
}

fn codes(text: &str) -> Vec<&'static str> {
    let env = home_env("file-codes");
    match load(&env, text) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(ds) => ds.iter().map(|d| d.code).collect(),
    }
}

#[test]
fn home_file_and_xdg_config_parse_into_the_one_entry_type() {
    let env = home_env("file-parse");
    let m = load(
        &env,
        &format!(
            "{HEAD}[home.file.\".local/bin/hello\"]\ntext = \"#!/bin/sh\\n\"\nexecutable = true\n\
             [home.xdg_config.\"demo/extra.conf\"]\ntext = \"a\\n\"\nmode = \"0600\"\n\
             [home.file.\".profile-seed\"]\ntext = \"s\\n\"\nstate = \"seed\"\n\
             [home.file.\".gone\"]\nstate = \"absent\"\n"
        ),
    )
    .unwrap();
    let bin = &m.files[".local/bin/hello"];
    assert_eq!((bin.mode, bin.table.as_str()), (0o755, "home.file"));
    assert_eq!(bin.origin, Origin::Content);
    let xdg = &m.files[".config/demo/extra.conf"];
    assert_eq!((xdg.mode, xdg.table.as_str()), (0o600, "home.xdg_config"));
    let seed = &m.files[".profile-seed"];
    assert_eq!(seed.state, FileState::Seed);
    assert_eq!(seed.on_remove, OnRemove::Keep, "a seed is kept by default");
    assert_eq!(m.files[".gone"].state, FileState::Absent);
}

#[test]
fn text_source_and_mode_conflicts_are_refused() {
    for body in [
        "text = \"x\"\nsource = \"./a\"",
        "mode = \"0644\"",
        "text = \"x\"\nmode = \"0644\"\nexecutable = true",
    ] {
        assert_eq!(
            codes(&format!("{HEAD}[home.file.\".a\"]\n{body}\n")),
            ["E_ATTR_CONFLICT"],
            "{body}"
        );
    }
    assert_eq!(
        codes(&format!("{HEAD}[home.file.\".a\"]\ncontent = \"x\"\n")),
        ["E_ATTR_CONFLICT", "E_UNKNOWN_ATTR"],
        "`content` is not a key of [home.file], so the entry has neither `text` nor `source`"
    );
}

#[test]
fn keys_are_validated_as_files_keys_are() {
    for key in ["../elsewhere", "/etc/passwd", "a/../../b"] {
        assert_eq!(
            codes(&format!("{HEAD}[home.file.\"{key}\"]\ntext = \"x\"\n")),
            ["E_PATH_ESCAPE"],
            "{key}"
        );
    }
    assert_eq!(
        codes(&format!(
            "{HEAD}[home.xdg_config.\"../../elsewhere\"]\ntext = \"x\"\n"
        )),
        ["E_PATH_ESCAPE"]
    );
}

#[test]
fn a_path_declared_twice_in_any_combination_names_both() {
    let pairs = [
        (
            "[home.file.\".config/x\"]\ntext = \"a\"",
            "[home.file.\".config/x/\"]\ntext = \"b\"",
        ),
        (
            "[home.file.\".config/x\"]\ntext = \"a\"",
            "[home.xdg_config.\"x\"]\ntext = \"b\"",
        ),
    ];
    for (one, two) in pairs {
        let env = home_env("file-dup-pairs");
        let ds = load(&env, &format!("{HEAD}{one}\n{two}\n")).unwrap_err();
        assert_eq!(ds.len(), 1, "{ds:?}");
        assert_eq!(ds[0].code, "E_DUP_RESOURCE");
        let text = ds[0].to_string();
        let table = |s: &str| s[1..].split('.').next().unwrap().to_string();
        let (a, b) = (table(one), table(two));
        assert!(text.contains(&a) && text.contains(&b), "{text}");
    }
}

#[test]
fn a_directory_source_expands_to_sorted_files_and_refuses_what_is_not_a_file() {
    let env = home_env("file-tree");
    let tree = env.config().join("lodi/tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("b"), "b\n").unwrap();
    fs::write(tree.join("sub/a"), "a\n").unwrap();
    for table in ["home.xdg_config.\"demo\"", "home.file.\".config/demo\""] {
        let m = load(&env, &format!("{HEAD}[{table}]\nsource = \"./tree\"\n")).unwrap();
        let keys: Vec<&String> = m.files.keys().collect();
        assert_eq!(keys, [".config/demo/b", ".config/demo/sub/a"], "{table}");
        assert_eq!(m.files[".config/demo/sub/a"].bytes, b"a\n");
    }
    let text = format!("{HEAD}[home.xdg_config.\"demo\"]\nsource = \"./tree\"\n");
    std::os::unix::fs::symlink("b", tree.join("sub/link")).unwrap();
    let ds = load(&env, &text).unwrap_err();
    assert_eq!(
        ds.iter().map(|d| d.code).collect::<Vec<_>>(),
        ["E_PATH_ESCAPE"]
    );
    fs::remove_file(tree.join("sub/link")).unwrap();
    let fifo =
        std::ffi::CString::new(tree.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: a NUL-terminated path and a mode.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
    let ds = load(&env, &text).unwrap_err();
    assert_eq!(
        ds.iter().map(|d| d.code).collect::<Vec<_>>(),
        ["E_PATH_ESCAPE"]
    );
}

/// I14: files expanded from a directory come and go; no directory is ever owned or removed.
#[test]
fn an_expanded_directory_is_never_removed() {
    let env = home_env("file-tree-remove");
    let tree = env.config().join("lodi/tree");
    fs::create_dir_all(tree.join("sub")).unwrap();
    fs::write(tree.join("sub/a"), "a\n").unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[home.xdg_config.\"demo\"]\nsource = \"./tree\"\n"),
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let dir = env.home().join(".config/demo/sub");
    assert_eq!(fs::read(dir.join("a")).unwrap(), b"a\n");
    write_manifest(&env, HEAD);
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!dir.join("a").exists());
    assert!(dir.is_dir(), "the directory stays");
}

#[test]
fn the_plan_prints_seed_and_adopt_with_the_backup_note_and_absent_removes() {
    let env = home_env("file-verbs");
    fs::write(env.home().join(".adoptrc"), "same\n").unwrap();
    fs::write(env.home().join(".gone"), "bye\n").unwrap();
    write_manifest(
        &env,
        &format!(
            "{HEAD}[home.file.\".adoptrc\"]\ntext = \"same\\n\"\n\
             [home.file.\".seedrc\"]\ntext = \"s\\n\"\nstate = \"seed\"\n\
             [home.file.\".gone\"]\nstate = \"absent\"\n"
        ),
    );
    let plan = home_part(&run(&env, &["home", "plan"]));
    let line = |verb: &str, path: &str| {
        plan.lines()
            .find(|l| l.starts_with(verb) && l.contains(path))
            .unwrap_or_else(|| panic!("no `{verb}` line for {path}:\n{plan}"))
            .to_string()
    };
    assert!(
        line("adopt", ".adoptrc").contains("backup: first unmanaged copy kept"),
        "{plan}"
    );
    line("seed", ".seedrc");
    line("remove", ".gone");
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert!(!env.home().join(".gone").exists());
    // Everything done: an unchanged re-apply writes nothing.
    let before = home_gate().ledger_lines().len();
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let after: Vec<_> = home_gate().ledger_lines()[before..]
        .iter()
        .filter(|(_, p)| p.starts_with(env.root()))
        .cloned()
        .collect();
    assert!(after.is_empty(), "{after:?}");
}

/// The one planner, both tables side by side: `create` with the backup note over a file Lodi did
/// not write, `adopt` for identical bytes and `update` for new bytes over Lodi's own file, in the
/// plan, the apply and an apply over drift.
#[test]
fn each_table_plans_new_bytes_with_the_same_verbs() {
    let env = home_env("file-update");
    let manifest = |version: &str| {
        format!(
            "{HEAD}[home.file.\".h-new\"]\ntext = \"{version}\\n\"\n\
             [home.file.\".h-same\"]\ntext = \"same\\n\"\n\
             [home.xdg_config.\"demo/x-new\"]\ntext = \"{version}\\n\"\n"
        )
    };
    fs::create_dir_all(env.home().join(".config/demo")).unwrap();
    for rel in [".h-new", ".config/demo/x-new"] {
        fs::write(env.home().join(rel), "mine\n").unwrap();
    }
    fs::write(env.home().join(".h-same"), "same\n").unwrap();
    write_manifest(&env, &manifest("one"));
    let out = run(&env, &["home", "plan"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        home_part(&out),
        "\
create    .config/demo/x-new        0644    (backup: first unmanaged copy kept)
create    .h-new                    0644    (backup: first unmanaged copy kept)
adopt     .h-same                   0644    (backup: first unmanaged copy kept)
3 files: 2 create, 1 adopt
"
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));

    // New bytes over what Lodi wrote: `update`.
    write_manifest(&env, &manifest("two"));
    let out = run(&env, &["home", "plan"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    assert_eq!(
        home_part(&out),
        "\
update    .config/demo/x-new        0644
update    .h-new                    0644
unchanged .h-same                   0644
3 files: 2 update, 1 unchanged
"
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for rel in [".config/demo/x-new", ".h-new"] {
        assert_eq!(fs::read(env.home().join(rel)).unwrap(), b"two\n", "{rel}");
    }

    // Drift overwritten on request.
    write_manifest(&env, &manifest("three"));
    for rel in [".h-new", ".config/demo/x-new"] {
        fs::write(env.home().join(rel), "hand\n").unwrap();
    }
    let out = run(&env, &["home", "apply", "--overwrite-drift"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    for rel in [".config/demo/x-new", ".h-new"] {
        assert_eq!(fs::read(env.home().join(rel)).unwrap(), b"three\n", "{rel}");
    }
    // The edited bytes are kept in the backup store, one copy per drifted file.
    let backups = fs::read_dir(env.share().join("lodi/home-scope/backups"))
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name();
            name.to_string_lossy().contains(".drift-")
        })
        .count();
    assert_eq!(backups, 2);
}

// ------------------------------------------------------------------------ the state record ---

/// Lodi 1.0's reader of a state record, copied field for field from `v1.0.0:src/home/state.rs`
/// (the same serde attributes): what a 1.0 build does with what this build writes.
mod v1_0 {
    use super::*;

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct OriginRecord {
        pub kind: String,
        pub sha256: String,
        #[serde(default)]
        pub source: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct ManagedFile {
        pub path: String,
        pub sha256: String,
        pub mode: String,
        pub origin: OriginRecord,
        pub on_remove: String,
        #[serde(default)]
        pub backup: Option<String>,
    }

    #[derive(Debug, Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    #[allow(dead_code)]
    pub struct HomeState {
        pub version: u32,
        #[serde(default)]
        pub lodi_version: String,
        pub files: BTreeMap<String, ManagedFile>,
        pub directories: Vec<String>,
    }
}

/// 1.0's reader accepts an origin `kind` it does not know — it is a free string — but refuses
/// the fields this build adds beside it (`module`, `state`). Refused as JSON it did not write
/// would be the wrong message, so the record moves to schema version 2 and 1.0 refuses it by the
/// version instead (`docs/SCHEMAS.md`).
#[test]
fn the_1_0_reader_takes_an_unknown_kind_but_not_the_new_fields() {
    let record = |origin: &str, extra: &str| {
        format!(
            "{{\"directories\":[],\"files\":{{\".x\":{{\"mode\":\"0644\",\"onRemove\":\"keep\",\
             \"origin\":{origin},\"path\":\".x\",\"sha256\":\"00\"{extra}}}}},\
             \"lodiVersion\":\"1.1.0\",\"version\":1}}"
        )
    };
    let read = |json: &str| serde_json::from_str::<v1_0::HomeState>(json);
    let unknown_kind = record("{\"kind\":\"program\",\"sha256\":\"00\"}", "");
    assert!(read(&unknown_kind).is_ok(), "an unknown kind alone reads");
    let module = record(
        "{\"kind\":\"program\",\"module\":\"git\",\"sha256\":\"00\"}",
        "",
    );
    let err = read(&module).unwrap_err().to_string();
    assert!(err.contains("unknown field `module`"), "{err}");
    let seed = record(
        "{\"kind\":\"content\",\"sha256\":\"00\"}",
        ",\"state\":\"seed\"",
    );
    let err = read(&seed).unwrap_err().to_string();
    assert!(err.contains("unknown field `state`"), "{err}");
}

#[test]
fn the_state_record_carries_the_origin_kind_at_current_version() {
    let env = home_env("file-state-v2");
    write_manifest(
        &env,
        &format!(
            "{HEAD}[home.file.\".t\"]\ntext = \"t\\n\"\n\
             [home.file.\".s\"]\ntext = \"s\\n\"\nstate = \"seed\"\n"
        ),
    );
    let out = run(&env, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", text(&out.stderr));
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(env.share().join("lodi/home-scope/state.json")).unwrap())
            .unwrap();
    assert_eq!(state["version"], 5);
    assert_eq!(state["files"][".t"]["origin"]["kind"], "content");
    assert!(state["files"][".t"].get("state").is_none());
    assert_eq!(state["files"][".s"]["state"], "seed");
}
