//! The home manifest loader (M-0.5 T-2, acceptance rows (b), (c), (d), (e)), read by the home
//! part of `lodi switch --home --dry-run` (1.x's `lodi home plan`; LD-518).
//!
//! Every case here runs the **built binary** against a throwaway set of roots through
//! `support::home_env`, which clears the environment and refuses to hand a child the ambient
//! `HOME` (design call D15, `LD-111`). Nothing is mocked: the manifests are real files in a real
//! configuration root, and what the test reads is what a user would see on their terminal.
//!
//! Offline and deterministic. No network, no Podman, no clock.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{HomeEnv, home_env, home_gate, home_part, nothing};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The configuration root `Roots` computes for one of these environments: `$XDG_CONFIG_HOME/lodi`.
fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

/// Put `text` at `<config>/home.toml`. The **test** makes the directory; the point of several
/// cases below is that `lodi` never does.
fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_root(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

/// Copy a committed fixture directory into `<config>`.
fn install_fixture(env: &HomeEnv, name: &str) {
    let from = repo().join("tests/fixtures/home").join(name);
    let to = config_root(env);
    copy_tree(&from, &to);
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

fn plan(env: &HomeEnv) -> Output {
    env.lodi(&["home", "plan"])
        .output()
        .expect("the lodi binary runs")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

/// The ledger lines this environment's own run produced. The ledger is shared by every test in
/// the process, so a line is this test's only when it is under this environment's root.
fn ledger_under(env: &HomeEnv) -> Vec<(String, PathBuf)> {
    home_gate()
        .ledger_lines()
        .into_iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect()
}

/// Every diagnostic the run printed, as `(code, line, column)`, in the order they appeared.
fn diagnostics(out: &Output) -> Vec<(String, usize, usize)> {
    let text = stderr(out);
    let mut lines = text.lines().peekable();
    let mut found = Vec::new();
    while let Some(line) = lines.next() {
        let Some(rest) = line.strip_prefix("lodi: error ") else {
            continue;
        };
        let code = rest.split(':').next().unwrap().to_string();
        let at = lines
            .peek()
            .and_then(|l| l.trim().strip_prefix("--> "))
            .unwrap_or_else(|| panic!("{code} carries no location:\n{text}"));
        let mut parts = at.rsplit(':');
        let column: usize = parts.next().unwrap().parse().expect("a column");
        let line_number: usize = parts.next().unwrap().parse().expect("a line");
        found.push((code, line_number, column));
    }
    found
}

// ------------------------------------------------------------------- (e) no manifest at all ---

/// (e) No config is `E_NO_MANIFEST` at exit 3, naming the standard folder `~/.config/lodi` and
/// with a hint naming `lodi import`, and **the configuration directory is not created** by the
/// attempt (design call D12).
#[test]
fn no_manifest_is_e_no_manifest_with_the_path_to_create() {
    let env = home_env("no-home-manifest");
    let before = env.decoy_listing();
    assert!(
        !config_root(&env).exists(),
        "the test starts with no <config>"
    );

    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(out.stdout.is_empty(), "{text}");
    assert!(text.contains("lodi: error E_NO_MANIFEST"), "{text}");
    assert!(
        text.contains(&config_root(&env).display().to_string()),
        "the message does not name the folder to create: {text}"
    );
    assert!(text.contains("make one with `lodi import`"), "{text}");
    assert!(
        !config_root(&env).exists(),
        "the preview created the configuration directory"
    );
    assert!(ledger_under(&env).is_empty(), "a plan wrote something");
    assert_eq!(env.decoy_listing(), before, "the decoy tree changed");
}

// -------------------------------------------------------- (b) every error, in a single run ---

/// (b) The committed `errors/home.toml` holds every load error of this task at once. One run
/// prints all of them, each with its code, its `file:line:column` and a hint; the run exits 3
/// and writes nothing.
#[test]
fn every_load_error_is_reported_in_one_run_sorted_by_position() {
    let env = home_env("home-errors");
    let before = env.decoy_listing();
    install_fixture(&env, "errors");

    let out = env
        .lodi(&["home", "plan"])
        .output()
        .expect("the lodi binary runs");
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(!text.contains("\nhome: "), "a failing plan printed a plan");

    let found = diagnostics(&out);
    assert!(
        found.len() > 20,
        "one run must report every problem, not the first: {text}"
    );

    // Every code this task can emit from a manifest is in there, from one run of one file.
    let codes: Vec<&str> = found.iter().map(|(c, _, _)| c.as_str()).collect();
    for code in [
        "E_UNSUPPORTED",
        "E_VERSION",
        "E_UNKNOWN_ATTR",
        "E_UNKNOWN_BLOCK",
        "E_PATH_ESCAPE",
        "E_ATTR_CONFLICT",
        "E_TYPE",
        "E_CONFIG",
        "E_DUP_RESOURCE",
        "E_EXCLUDED_CONSTRUCT",
    ] {
        assert!(codes.contains(&code), "no {code} in one run:\n{text}");
    }

    // Sorted by position, and every location names the manifest itself.
    let positions: Vec<(usize, usize)> = found.iter().map(|(_, l, c)| (*l, *c)).collect();
    let mut sorted = positions.clone();
    sorted.sort_unstable();
    assert_eq!(positions, sorted, "the diagnostics are not sorted:\n{text}");
    let shown = format!("--> {}:", env.config().join("lodi/home.toml").display());
    assert_eq!(
        text.matches(&shown).count(),
        found.len(),
        "a diagnostic points somewhere else:\n{text}"
    );

    // Every diagnostic carries a hint naming the next step.
    assert_eq!(
        text.matches("= hint: ").count(),
        found.len(),
        "a diagnostic has no hint:\n{text}"
    );

    assert!(
        ledger_under(&env).is_empty(),
        "a failed load wrote something"
    );
    assert_eq!(env.decoy_listing(), before, "the decoy tree changed");
}

// ------------------------------------------------------------------ (c) a path that escapes ---

/// (c) A key that could leave the home directory is `E_PATH_ESCAPE` **at the key's column**, at
/// load time, before anything is planned.
#[test]
fn a_path_that_escapes_the_home_is_refused_at_the_keys_column() {
    let env = home_env("home-escape");
    for (key, why) in [
        ("/etc/passwd", "it is absolute"),
        ("../escape", "it has a `..` component"),
        ("~/tilde", "starts with `~`"),
        ("a/./b", "it has a `.` component"),
        ("a//b", "it has an empty component"),
        // The key is written `a\\b` in TOML, which decodes to the one backslash below.
        ("a\\\\b", "it has a backslash"),
    ] {
        write_manifest(
            &env,
            &format!("[home]\nversion = \"1\"\n\n[home.file.\"{key}\"]\ntext = \"x\"\n"),
        );
        let out = plan(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{key}: {text}");
        assert!(text.contains("lodi: error E_PATH_ESCAPE"), "{key}: {text}");
        assert!(text.contains(why), "{key}: {text}");
        // Line 4 is the `[home.file."…"]` header and column 12 is where the quoted key begins.
        assert!(
            text.contains("home.toml:4:12\n"),
            "{key}: the column is not the key's: {text}"
        );
    }
    assert!(
        ledger_under(&env).is_empty(),
        "a failed load wrote something"
    );
}

/// A `source` that would leave the manifest's own directory is `E_PATH_ESCAPE` too, and one that
/// is a symbolic link or absent is `E_CONFIG` naming it. A directory expands into one entry per
/// file below it (M-Home, `docs/design/HOME_PROGRAMS.md` §3.2; `tests/home_file.rs`).
#[test]
fn a_source_outside_the_manifests_directory_is_refused() {
    let env = home_env("home-source");
    let dir = config_root(&env);
    fs::create_dir_all(dir.join("dotfiles")).unwrap();
    fs::write(dir.join("dotfiles/inputrc"), "set editing-mode vi\n").unwrap();
    std::os::unix::fs::symlink("dotfiles/inputrc", dir.join("linked")).unwrap();

    for (source, code, needle) in [
        ("../../escape", "E_PATH_ESCAPE", "it has a `..` component"),
        ("/etc/passwd", "E_PATH_ESCAPE", "it is absolute"),
        ("./nope", "E_CONFIG", "is not there"),
        ("./linked", "E_CONFIG", "is a symbolic link"),
    ] {
        write_manifest(
            &env,
            &format!(
                "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\nsource = \"{source}\"\n"
            ),
        );
        let out = plan(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{source}: {text}");
        assert!(
            text.contains(&format!("lodi: error {code}")),
            "{source}: {text}"
        );
        assert!(text.contains(needle), "{source}: {text}");
    }

    // The one that is a real file below the manifest's directory is read verbatim and plans.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\nsource = \"./dotfiles/inputrc\"\n",
    );
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(ledger_under(&env).is_empty(), "a plan wrote something");
}

// ------------------------------------------------------------------------- (d) [tools] ---

/// T-5 makes `[tools]` part of the command-facing schema. The preview names the tools an apply
/// would install, a change of their own (LD-524), and remains completely read-only.
#[test]
fn tools_uses_the_shared_schema_and_plan_stays_read_only() {
    let env = home_env("home-tools");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[tools]\npython = \"3.12\"\n",
    );
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!nothing(&out), "{}", stderr(&out));
    assert!(home_part(&out).contains("python"), "{}", stderr(&out));
    assert!(
        ledger_under(&env).is_empty(),
        "a plan with tools wrote something"
    );
}

/// `[vars]`, `[inputs]`, `[modules]` and `registries` are refused the same way, each naming what
/// it would need — never as a typo, because they are names the language has (design call D8).
#[test]
fn the_other_defined_names_are_refused_with_what_each_would_need() {
    let env = home_env("home-defined-names");
    for (text, needs) in [
        ("[vars]\na = \"b\"\n", "substitution"),
        ("[inputs]\na = \"b\"\n", "inputs"),
        ("[modules]\na = \"b\"\n", "several files"),
        ("[home]\nregistries = [\"x\"]\n", "built-in recipes"),
    ] {
        write_manifest(&env, text);
        let out = plan(&env);
        let printed = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{text}: {printed}");
        assert!(
            printed.contains("lodi: error E_UNSUPPORTED"),
            "{text}: {printed}"
        );
        assert!(printed.contains(needs), "{text}: {printed}");
        assert!(
            !printed.contains("did you mean"),
            "a name spec/01 defines was reported as a typo: {printed}"
        );
    }
}

/// An unknown table or key is `E_UNKNOWN_BLOCK` / `E_UNKNOWN_ATTR` with a "did you mean" hint.
#[test]
fn an_unknown_name_gets_a_did_you_mean_hint() {
    let env = home_env("home-unknown");
    for (manifest, code, hint) in [
        (
            "[home]\nversion = \"1\"\n\n\
             [home.file.\".inputrc\"]\ntext = \"x\"\nonremove = \"keep\"\n",
            "E_UNKNOWN_ATTR",
            "did you mean `on_remove`?",
        ),
        (
            "[home]\nversion = \"1\"\n\n[programz]\na = \"b\"\n",
            "E_UNKNOWN_BLOCK",
            "did you mean `programs`?",
        ),
    ] {
        write_manifest(&env, manifest);
        let out = plan(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{text}");
        assert!(text.contains(&format!("lodi: error {code}")), "{text}");
        assert!(text.contains(hint), "{text}");
    }
}

// ----------------------------------------------------------------- the rest of the schema ---

/// `version` must be `"1"`; `min_lodi_version` is honoured, and a build older than it is
/// `E_VERSION`. Neither case pins this repository's version number: the test asks for a version
/// no release will ever carry, and for one every release already passes.
#[test]
fn the_schema_version_and_the_minimum_lodi_version_are_honoured() {
    let env = home_env("home-versions");

    write_manifest(&env, "[home]\nversion = \"2\"\n");
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_UNSUPPORTED"), "{text}");
    assert!(text.contains("only version \"1\" exists"), "{text}");

    write_manifest(&env, "[home]\nmin_lodi_version = \"99.0.0\"\n");
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_VERSION"), "{text}");
    assert!(text.contains("99.0.0"), "{text}");

    write_manifest(&env, "[home]\nmin_lodi_version = \"not-a-version\"\n");
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_TYPE"), "{text}");

    // A minimum every release satisfies, and the empty string of the documented example, both
    // load. An empty `min_lodi_version` declares no minimum.
    for text in [
        "[home]\nversion = \"1\"\nmin_lodi_version = \"0.0.1\"\n",
        "[home]\nversion = \"1\"\nmin_lodi_version = \"\"\n",
    ] {
        write_manifest(&env, text);
        let out = plan(&env);
        assert_eq!(out.status.code(), Some(0), "{text}: {}", stderr(&out));
    }
}

/// `mode` is three or four octal digits, and a set-uid, set-gid or sticky bit is `E_UNSUPPORTED`:
/// the home scope writes plain files only.
#[test]
fn a_mode_is_octal_and_never_carries_a_special_bit() {
    let env = home_env("home-mode");
    for (mode, code) in [
        ("rw-", "E_TYPE"),
        ("64", "E_TYPE"),
        ("06440", "E_TYPE"),
        ("0648", "E_TYPE"),
        ("4755", "E_UNSUPPORTED"),
        ("2755", "E_UNSUPPORTED"),
        ("1777", "E_UNSUPPORTED"),
    ] {
        write_manifest(
            &env,
            &format!(
                "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\ntext = \"x\"\nmode = \"{mode}\"\n"
            ),
        );
        let out = plan(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{mode}: {text}");
        assert!(
            text.contains(&format!("lodi: error {code}")),
            "{mode}: {text}"
        );
    }
    for mode in ["644", "0600", "0755"] {
        write_manifest(
            &env,
            &format!(
                "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\ntext = \"x\"\nmode = \"{mode}\"\n"
            ),
        );
        let out = plan(&env);
        assert_eq!(out.status.code(), Some(0), "{mode}: {}", stderr(&out));
    }
}

/// Exactly one of `text` and `source` gives a present file its bytes; an absent one declares
/// neither, because there is nothing for it to hold.
#[test]
fn exactly_one_origin_describes_a_present_file() {
    let env = home_env("home-origin");
    let dir = config_root(&env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("src.txt"), "x").unwrap();

    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\ntext = \"a\"\nsource = \"./src.txt\"\n",
    );
    let text = stderr(&plan(&env));
    assert!(text.contains("lodi: error E_ATTR_CONFLICT"), "{text}");
    assert!(text.contains("both `text` and `source`"), "{text}");

    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\nmode = \"0644\"\n",
    );
    let text = stderr(&plan(&env));
    assert!(text.contains("lodi: error E_ATTR_CONFLICT"), "{text}");
    assert!(text.contains("neither `text` nor `source`"), "{text}");

    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\nstate = \"absent\"\ntext = \"a\"\n",
    );
    let text = stderr(&plan(&env));
    assert!(text.contains("lodi: error E_ATTR_CONFLICT"), "{text}");
    assert!(text.contains("absent"), "{text}");
}

/// Two entries that name the same path are `E_DUP_RESOURCE`: a trailing `/` is not part of a
/// path, so `bin` and `bin/` are one resource declared twice.
#[test]
fn two_entries_for_one_path_are_a_duplicate_resource() {
    let env = home_env("home-duplicate");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".x\"]\ntext = \"a\"\n\n\
         [home.file.\".x/\"]\ntext = \"b\"\n",
    );
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_DUP_RESOURCE"), "{text}");
    assert!(text.contains("declared twice"), "{text}");
}

/// No `${ }` substitution exists in the home scope in 0.5: a well-formed name is
/// `E_UNSUPPORTED`, `vars.*` is `E_VAR_UNSET`, and anything else is `E_EXCLUDED_CONSTRUCT` —
/// exactly what the project manifest does with the same string.
#[test]
fn substitution_fails_the_same_way_as_in_the_project_manifest() {
    let env = home_env("home-substitution");
    for (value, code) in [
        ("${host.arch}", "E_UNSUPPORTED"),
        ("${vars.name}", "E_VAR_UNSET"),
        ("${not a name}", "E_EXCLUDED_CONSTRUCT"),
        ("${unterminated", "E_EXCLUDED_CONSTRUCT"),
    ] {
        write_manifest(
            &env,
            &format!("[home]\nversion = \"1\"\n\n[home.file.\".x\"]\ntext = \"{value}\"\n"),
        );
        let out = plan(&env);
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(3), "{value}: {text}");
        assert!(
            text.contains(&format!("lodi: error {code}")),
            "{value}: {text}"
        );
    }
    // `$${` is a literal `${`, and a bare `$NAME` is left for whoever reads the file.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".x\"]\ntext = \"$${literal} and $PATH\"\n",
    );
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
}

/// A manifest that is not TOML is `E_SYNTAX` at the position the parser stopped, and a manifest
/// that is not UTF-8 is `E_SYNTAX` too.
#[test]
fn a_manifest_that_is_not_toml_is_a_syntax_error() {
    let env = home_env("home-syntax");
    write_manifest(&env, "[home\nversion = \"1\"\n");
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_SYNTAX"), "{text}");

    let dir = config_root(&env);
    fs::write(dir.join("home.toml"), [0xff, 0xfe, b'\n']).unwrap();
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("UTF-8"), "{text}");
}
