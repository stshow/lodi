//! `lodi home plan` (M-0.5 T-2, acceptance rows (a) and (f), design calls D12 and D13).
//!
//! Two things are proved here, both against the **built binary** in a throwaway set of roots:
//!
//! 1. the plan is a truthful, ordered, stable report of what an apply would do — one line per
//!    declared path in lexicographic order, with the mode column and the summary;
//! 2. **the plan writes nothing at all.** Not a state file, not a lock, not a directory, not even
//!    the configuration directory. The proof is the write ledger of `lodi::home::fsops` — empty
//!    for this environment's root afterwards — plus a listing of the whole scratch root, with
//!    every file's sha256, taken before and after and compared byte for byte.
//!
//! Offline and deterministic. No network, no Podman, no clock.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{HomeEnv, files_warning, home_env, home_gate};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_root(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
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

fn install_fixture(env: &HomeEnv, name: &str) {
    copy_tree(
        &repo().join("tests/fixtures/home").join(name),
        &config_root(env),
    );
}

fn plan(env: &HomeEnv) -> Output {
    env.command()
        .args(["home", "plan"])
        .output()
        .expect("the lodi binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

fn ledger_under(env: &HomeEnv) -> Vec<(String, PathBuf)> {
    home_gate()
        .ledger_lines()
        .into_iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect()
}

/// Every path below `root`, relative, with its sha256 (or `dir`, or the target of a link),
/// sorted: an exact listing **and** the content, so neither a new file, a changed one nor a
/// changed mode can pass unnoticed.
fn tree(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("link {}", fs::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(rel, "dir".into());
                walk(root, &path, out);
            } else {
                use std::os::unix::fs::PermissionsExt;
                out.insert(
                    rel,
                    format!(
                        "{:04o} {}",
                        meta.permissions().mode() & 0o7777,
                        lodi::util::sha256_hex(&fs::read(&path).unwrap())
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

// ------------------------------------------------------------------------- (a) the plan ---

/// (a) A manifest with several `[files]` entries plans in lexicographic order with the mode
/// column, and the ledger is **empty**. Its one line on standard error is the `W_DEPRECATED` of
/// `[files]` (decision D5, from 1.2).
#[test]
fn several_files_plan_in_order_with_their_modes_and_nothing_is_written() {
    let env = home_env("plan-several");
    install_fixture(&env, "several");
    // Two of the four paths already exist: one with exactly the declared bytes and mode, one
    // that has to go. The other two do not, so all four verbs of this build appear at once.
    fs::write(
        env.home().join(".gitconfig"),
        "[core]\n\tautocrlf = input\n",
    )
    .unwrap();
    fs::write(env.home().join(".netrc-that-must-go"), "secret\n").unwrap();

    let before = tree(env.root());
    let decoy_before = env.decoy_listing();

    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), files_warning());
    assert_eq!(
        stdout(&out),
        "\
create    .config/git/ignore        0644
unchanged .gitconfig                0644
create    .inputrc                  0600
remove    .netrc-that-must-go       0644    (backup: first unmanaged copy kept)
4 files: 2 create, 1 remove, 1 unchanged
"
    );

    // The one property `plan` must have.
    assert!(
        ledger_under(&env).is_empty(),
        "the plan wrote: {:?}",
        ledger_under(&env)
    );
    assert_eq!(tree(env.root()), before, "the scratch root changed");
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
    // Nothing of the machine-side footprint was made either (design calls D2 and D12).
    assert!(!env.data().join("lodi/home-scope").exists());
    assert!(!config_root(&env).join("home.lock").exists());
}

/// The same plan twice prints the same bytes: the order is the path's, not the document's, and
/// nothing in the output is derived from a clock, a pid or a counter (design call D13).
#[test]
fn the_same_manifest_plans_the_same_way_twice() {
    let env = home_env("plan-stable");
    // Declared in an order that is not the lexicographic one, to prove the renderer sorts.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\"z\"]\ncontent = \"z\"\n\n[files.\"a\"]\ncontent = \"a\"\n\n[files.\"m/n\"]\ncontent = \"n\"\n",
    );
    let first = plan(&env);
    let second = plan(&env);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(stdout(&first), stdout(&second));
    let printed = stdout(&first);
    let paths: Vec<&str> = printed
        .lines()
        .filter(|l| l.starts_with("create"))
        .map(|l| l.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(paths, ["a", "m/n", "z"]);
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

/// A `replace` names the backup that an apply would keep, and a file that already holds the
/// declared bytes at the declared mode is `unchanged`. Exit 0 either way.
#[test]
fn a_file_that_differs_is_replaced_and_one_that_matches_is_unchanged() {
    let env = home_env("plan-replace");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".inputrc\"]\ncontent = \"new\"\nmode = \"0600\"\n",
    );
    let path = env.home().join(".inputrc");
    fs::write(&path, "old").unwrap();
    let text = stdout(&plan(&env));
    assert!(text.starts_with("replace   .inputrc"), "{text}");
    assert!(
        text.contains("(backup: first unmanaged copy kept)"),
        "{text}"
    );
    assert!(text.contains("1 file: 1 replace"), "{text}");

    // The same bytes at the wrong mode is still a replace: the mode is part of the declaration.
    fs::write(&path, "new").unwrap();
    let text = stdout(&plan(&env));
    assert!(text.starts_with("replace   .inputrc"), "{text}");

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stdout(&out).starts_with("unchanged .inputrc"),
        "{}",
        stdout(&out)
    );
    assert!(
        stdout(&out).contains("1 file: 1 unchanged"),
        "{}",
        stdout(&out)
    );

    // `backup = false` drops the note, and says so by saying nothing.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".inputrc\"]\ncontent = \"other\"\nbackup = false\n",
    );
    let text = stdout(&plan(&env));
    assert!(text.starts_with("replace   .inputrc"), "{text}");
    assert!(!text.contains("backup"), "{text}");
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

/// A manifest with no `[files]` at all plans nothing and exits 0: `plan` succeeds whether or not
/// there is anything to do.
#[test]
fn an_empty_manifest_plans_nothing_and_succeeds() {
    let env = home_env("plan-empty");
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let before = tree(env.root());
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "0 files: nothing to do\n");
    assert_eq!(tree(env.root()), before, "the scratch root changed");
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

/// A declared path whose way through the home is a symbolic link is `E_PATH_ESCAPE` at exit 3,
/// so the plan predicts the refusal instead of promising a write that would never happen.
#[test]
fn a_symlinked_component_is_refused_by_the_plan_too() {
    let env = home_env("plan-link");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".config/git/ignore\"]\ncontent = \".lodi/\\n\"\n",
    );
    let elsewhere = env.root().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    std::os::unix::fs::symlink(&elsewhere, env.home().join(".config/git")).unwrap();

    let before = tree(env.root());
    let out = plan(&env);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_PATH_ESCAPE"), "{text}");
    assert!(text.contains("symbolic link"), "{text}");
    assert_eq!(
        tree(env.root()),
        before,
        "the refused plan changed the tree"
    );
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

// ------------------------------------------------------------------------ (f) the verbs ---

/// (f) `lodi home frobnicate` and `lodi home plan extra` are usage errors: exit 2, no `E_` code,
/// nothing written. `apply` and `status` are T-3's and are usage errors until then, which is why
/// `lodi --help` does not name them (design call D3). A bare `lodi home` and `lodi home --help`
/// are the scope's help since LD-360: exit 0, and nothing written either.
#[test]
fn the_home_verbs_that_do_not_exist_are_usage_errors() {
    let env = home_env("plan-usage");
    install_fixture(&env, "several");
    let before = tree(env.root());
    for args in [&["home"][..], &["home", "--help"]] {
        let out = env.command().args(args).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert!(stdout(&out).contains("lodi home plan"), "{args:?}");
    }
    for args in [
        &["home", "frobnicate"][..],
        &["home", "plan", "extra", "more"],
        &["home", "plan", "--json"],
        // `apply` and `status` work since T-3; what is still a usage error is a bad argument
        // to one of them, and a verb that does not exist.
        &["home", "apply", "extra", "more"],
        &["home", "apply", "--json"],
        &["home", "status", "extra", "more"],
        // `init` is `import` since LD-382, so a bad argument to it is the usage error, and
        // `restore` is still no verb.
        &["home", "init", "extra", "more"],
        &["home", "restore"],
    ] {
        let out = env.command().args(args).output().unwrap();
        let text = stderr(&out);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {text}");
        assert!(out.stdout.is_empty(), "{args:?}: {}", stdout(&out));
        assert!(text.contains("unsupported"), "{args:?}: {text}");
        assert!(
            !text.contains("E_"),
            "{args:?}: a usage error carries no code: {text}"
        );
    }
    assert_eq!(tree(env.root()), before, "a usage error changed the tree");
    assert!(
        ledger_under(&env).is_empty(),
        "a usage error wrote something"
    );
}

/// `lodi help home` names the home verbs that work — `plan`, `apply`, `status`, and `init` with its
/// 1.0 name `import` (LD-382) — and no verb that does not: nothing half-built is reachable from
/// the command line.
#[test]
fn help_names_the_home_verbs_that_work_and_no_others() {
    let out = env_help();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.contains("lodi home plan"), "{text}");
    assert!(
        text.contains("lodi home apply [--overwrite-drift]"),
        "{text}"
    );
    assert!(text.contains("lodi home status"), "{text}");
    assert!(
        text.contains("lodi home init [--out DIR | --stdout] [--force]"),
        "{text}"
    );
    assert!(!text.contains("home restore"), "{text}");
}

fn env_help() -> Output {
    std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["help", "home"])
        .output()
        .expect("the lodi binary runs")
}
