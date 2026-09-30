//! `lodi home status` (M-0.5 T-3, acceptance rows (d) and (h)).
//!
//! `status` is the **report**: it prints what it finds, it exits 0 whatever that is — a warning
//! never changes an exit status (`spec/11` §1) — and it writes nothing at all. The last of those
//! is the property worth asserting the hard way, so every test here lists the whole scratch root
//! with modes and hashes before and after and compares the two.
//!
//! As everywhere in this repository, the binary runs through `support::home_env`, which clears
//! the environment and refuses to hand a child the ambient `HOME` (design call D15, `LD-111`).

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{HomeEnv, files_warning, home_env, home_gate};

fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_root(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

fn run(env: &HomeEnv, args: &[&str]) -> Output {
    env.command()
        .args(args)
        .output()
        .expect("the lodi binary runs")
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

/// Every path below `root`, relative, with its mode and sha256 (or `dir`, or a link's target).
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

/// Run `lodi home status` and assert the two properties it must always have: exit 0, and a
/// scratch root that is byte-identical before and after, with not one new ledger line.
fn status(env: &HomeEnv) -> Output {
    let before = tree(env.root());
    let ledger_before = home_gate().ledger_lines().len();
    let decoy_before = env.decoy_listing();

    let out = run(env, &["home", "status"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "status exited non-zero: {}",
        stderr(&out)
    );
    assert_eq!(tree(env.root()), before, "status wrote something");
    let appended: Vec<(String, PathBuf)> = home_gate()
        .ledger_lines()
        .into_iter()
        .skip(ledger_before)
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect();
    assert!(
        appended.is_empty(),
        "status appended to the ledger: {appended:?}"
    );
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
    out
}

const SEVERAL: &str = r#"[home]
version = "1"

[files.".config/git/ignore"]
content = ".lodi/\n"

[files.".gitconfig"]
content = "[core]\n\tautocrlf = input\n"

[files.".inputrc"]
content = "set editing-mode vi\n"
mode = "0600"
"#;

/// Before anything is applied, every declared path is `unmanaged` — and finding that out creates
/// nothing, not the data root, not the lock.
#[test]
fn a_declared_path_that_was_never_applied_is_unmanaged() {
    let env = home_env("status-unmanaged");
    write_manifest(&env, SEVERAL);

    let out = status(&env);
    assert_eq!(
        stdout(&out),
        "\
unmanaged    .config/git/ignore        0644    (declared, not yet applied)
unmanaged    .gitconfig                0644    (declared, not yet applied)
unmanaged    .inputrc                  0600    (declared, not yet applied)
3 paths: 3 unmanaged
"
    );
    assert_eq!(stderr(&out), files_warning());
    assert!(
        !env.data().join("lodi/home-scope").exists(),
        "a report created the scope directory"
    );
}

/// After an apply, every path is `ok` and nothing is warned about but the `[files]` table itself
/// (`W_DEPRECATED`, decision D5).
#[test]
fn an_applied_path_is_ok() {
    let env = home_env("status-ok");
    write_manifest(&env, SEVERAL);
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));

    let out = status(&env);
    assert_eq!(
        stdout(&out),
        "\
ok           .config/git/ignore        0644
ok           .gitconfig                0644
ok           .inputrc                  0600
3 paths: 3 ok
"
    );
    assert_eq!(stderr(&out), files_warning());
}

/// (d) A hand-edited managed file makes `status` print `W_DRIFT` — **at exit 0**: the report
/// reports, and `apply` is the gate (design call D6, `spec/11` §1).
#[test]
fn a_hand_edited_file_is_drift_at_exit_zero() {
    let env = home_env("status-drift");
    write_manifest(&env, SEVERAL);
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    fs::write(env.home().join(".gitconfig"), "[user]\n\tname = me\n").unwrap();

    // `status` asserts the exit status and the byte-identity of the root for us.
    let out = status(&env);
    let text = stdout(&out);
    assert!(
        text.contains(
            "drift        .gitconfig                0644    (edited by hand since lodi wrote it)"
        ),
        "{text}"
    );
    assert!(text.ends_with("3 paths: 2 ok, 1 drift\n"), "{text}");

    // The manifest uses `[files]`, so its `W_DEPRECATED` line comes first (decision D5).
    let warned = stderr(&out);
    let warned = warned
        .strip_prefix(&files_warning())
        .unwrap_or_else(|| panic!("the D5 line does not come first: {warned}"));
    assert!(
        warned.starts_with("lodi: warning W_DRIFT: .gitconfig has changed since lodi wrote it ("),
        "{warned}"
    );
    assert!(
        warned.contains("recorded ") && warned.contains("now "),
        "{warned}"
    );
    assert_eq!(warned.lines().count(), 1, "{warned}");
}

/// A managed file the user deleted is `missing`, and a managed file whose mode was changed is
/// still `ok` — with a note, because resetting a mode destroys nothing (design call D6).
#[test]
fn a_deleted_file_is_missing_and_a_changed_mode_is_only_a_note() {
    let env = home_env("status-missing");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [files.\"gone\"]\ncontent = \"x\\n\"\n\n\
         [files.\"remoded\"]\ncontent = \"y\\n\"\nmode = \"0600\"\n",
    );
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    fs::remove_file(env.home().join("gone")).unwrap();
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(
        env.home().join("remoded"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();

    let out = status(&env);
    assert_eq!(
        stdout(&out),
        "\
missing      gone                      0644    (managed, but the file is gone)
ok           remoded                   0644    (mode 0600 was set by lodi)
2 paths: 1 ok, 1 missing
"
    );
    assert_eq!(stderr(&out), files_warning());
}

/// A backup kept for a path the state no longer names is `stale-backup`: lodi never removes a
/// backup, so the report is how the user learns what is still recoverable (design call D5).
#[test]
fn a_backup_of_a_forgotten_path_is_reported_as_stale() {
    let env = home_env("status-stale");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[files.\".netrc\"]\ncontent = \"lodi\\n\"\non_remove = \"delete\"\n",
    );
    fs::write(env.home().join(".netrc"), "the user's own\n").unwrap();
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    // The entry disappears, so the file is deleted and the state forgets the path — but the copy
    // of what the user had is still in the store.
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));

    let out = status(&env);
    let text = stdout(&out);
    assert!(text.starts_with("stale-backup .netrc"), "{text}");
    assert!(text.contains("0644    (original kept as "), "{text}");
    assert!(text.ends_with("1 path: 1 stale-backup\n"), "{text}");
    assert_eq!(stderr(&out), "");
}

/// A path declared `state = "absent"` that is indeed absent is `ok`, and one that is still there
/// is `unmanaged`: work the apply has not done yet.
#[test]
fn an_absent_declaration_reports_whether_it_holds() {
    let env = home_env("status-absent");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [files.\"is-gone\"]\nstate = \"absent\"\n\n\
         [files.\"is-still-here\"]\nstate = \"absent\"\n",
    );
    fs::write(env.home().join("is-still-here"), "the user's own\n").unwrap();

    let out = status(&env);
    assert_eq!(
        stdout(&out),
        "\
ok           is-gone                   0644    (declared absent, and it is)
unmanaged    is-still-here             0644    (declared, not yet applied)
2 paths: 1 ok, 1 unmanaged
"
    );
}

/// With no `home.toml` at all, `status` is the manifest loader's own `E_NO_MANIFEST` at exit 3,
/// and it still creates nothing.
#[test]
fn a_status_without_a_manifest_names_the_file_it_wants() {
    let env = home_env("status-no-manifest");
    let before = tree(env.root());
    let out = run(&env, &["home", "status"]);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_NO_MANIFEST"), "{text}");
    assert!(text.contains("home.toml"), "{text}");
    assert_eq!(tree(env.root()), before, "a refused status wrote something");
}

/// An empty manifest reports nothing, and says so rather than printing an empty page.
#[test]
fn an_empty_manifest_has_nothing_to_report() {
    let env = home_env("status-empty");
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let out = status(&env);
    assert_eq!(stdout(&out), "0 paths: nothing to report\n");
    assert_eq!(stderr(&out), "");
}
