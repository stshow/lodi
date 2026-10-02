//! The home part's preview, `lodi switch --home --dry-run` (1.x's `lodi home plan`; M-0.5 T-2,
//! acceptance row (a), design calls D12 and D13; LD-518).
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

use support::{HomeEnv, home_env, home_gate, home_part, nothing};

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
    env.lodi(&["home", "plan"])
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

/// (a) A manifest with several `[home.file]` entries plans in lexicographic order with the mode
/// column, and the ledger is **empty**.
#[test]
fn several_files_plan_in_order_with_their_modes_and_nothing_is_written() {
    let env = home_env("plan-several");
    install_fixture(&env, "several");
    // Two of the four paths already exist: one with exactly the declared bytes and mode, which
    // is adopted, and one that has to go. The other two do not.
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
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    assert_eq!(
        home_part(&out),
        "\
create    .config/git/ignore        0644
adopt     .gitconfig                0644    (backup: first unmanaged copy kept)
create    .inputrc                  0600
remove    .netrc-that-must-go       0644    (backup: first unmanaged copy kept)
4 files: 2 create, 1 adopt, 1 remove
"
    );
    assert!(
        stderr(&out).contains("\nhome: 4 files\n"),
        "{}",
        stderr(&out)
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
    assert!(!env.share().join("lodi/home-scope").exists());
    assert!(!config_root(&env).join("lodi.lock").exists());
}

/// The same plan twice prints the same bytes: the order is the path's, not the document's, and
/// nothing in the output is derived from a clock, a pid or a counter (design call D13).
#[test]
fn the_same_manifest_plans_the_same_way_twice() {
    let env = home_env("plan-stable");
    // Declared in an order that is not the lexicographic one, to prove the renderer sorts.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\"z\"]\ntext = \"z\"\n\n\
         [home.file.\"a\"]\ntext = \"a\"\n\n\
         [home.file.\"m/n\"]\ntext = \"n\"\n",
    );
    let first = plan(&env);
    let second = plan(&env);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    assert_eq!(stderr(&first), stderr(&second));
    let printed = home_part(&first);
    let paths: Vec<&str> = printed
        .lines()
        .filter(|l| l.starts_with("create"))
        .map(|l| l.split_whitespace().nth(1).unwrap())
        .collect();
    assert_eq!(paths, ["a", "m/n", "z"]);
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

/// A `create` over a file Lodi did not write names the backup that an apply would keep, and a
/// file that already holds the declared bytes at the declared mode is `adopt`. Exit 0 either way.
#[test]
fn a_file_that_differs_is_created_over_and_one_that_matches_is_adopted() {
    let env = home_env("plan-replace");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"new\"\nmode = \"0600\"\n",
    );
    let path = env.home().join(".inputrc");
    fs::write(&path, "old").unwrap();
    let text = home_part(&plan(&env));
    assert!(text.starts_with("create    .inputrc"), "{text}");
    assert!(
        text.contains("(backup: first unmanaged copy kept)"),
        "{text}"
    );
    assert!(text.contains("1 file: 1 create"), "{text}");

    // The same bytes at the wrong mode is still a write: the mode is part of the declaration.
    fs::write(&path, "new").unwrap();
    let text = home_part(&plan(&env));
    assert!(text.starts_with("create    .inputrc"), "{text}");

    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = home_part(&out);
    assert!(text.starts_with("adopt     .inputrc"), "{text}");
    assert!(text.contains("1 file: 1 adopt"), "{text}");

    // `backup = false` drops the note, and says so by saying nothing.
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\ntext = \"other\"\nbackup = false\n",
    );
    let text = home_part(&plan(&env));
    assert!(text.starts_with("create    .inputrc"), "{text}");
    assert!(!text.contains("backup"), "{text}");
    assert!(ledger_under(&env).is_empty(), "the plan wrote something");
}

/// A manifest with no `[home.file]` at all plans nothing and exits 0: `plan` succeeds whether or
/// not there is anything to do.
#[test]
fn an_empty_manifest_plans_nothing_and_succeeds() {
    let env = home_env("plan-empty");
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let before = tree(env.root());
    let out = plan(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(nothing(&out), "{}", stderr(&out));
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
        "[home]\nversion = \"1\"\n\n[home.file.\".config/git/ignore\"]\ntext = \".lodi/\\n\"\n",
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
