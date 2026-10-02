//! What 1.x's `lodi home status` reported, as the home part's preview, `lodi switch --home
//! --dry-run` (M-0.5 T-3, acceptance rows (d) and (h); LD-518).
//!
//! The preview prints what a switch would do, it exits 0 whatever that is — drift included: the
//! switch is the gate (`spec/11` §1) — and it writes nothing at all. The last of those is the
//! property worth asserting the hard way, so every test here lists the whole scratch root with
//! modes and hashes before and after and compares the two.
//!
//! As everywhere in this repository, the binary runs through `support::home_env`, which clears
//! the environment and refuses to hand a child the ambient `HOME` (design call D15, `LD-111`).

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use support::{HomeEnv, home_env, home_gate, home_part, nothing};

fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = config_root(env);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

fn run(env: &HomeEnv, args: &[&str]) -> Output {
    env.lodi(args).output().expect("the lodi binary runs")
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

/// Run the preview and assert the two properties it must always have: exit 0, and a scratch
/// root that is byte-identical before and after, with not one new ledger line.
fn status(env: &HomeEnv) -> Output {
    let before = tree(env.root());
    let ledger_before = home_gate().ledger_lines().len();
    let decoy_before = env.decoy_listing();

    let out = run(env, &["home", "status"]);

    assert_eq!(
        out.status.code(),
        Some(0),
        "the preview exited non-zero: {}",
        stderr(&out)
    );
    assert_eq!(tree(env.root()), before, "the preview wrote something");
    let appended: Vec<(String, PathBuf)> = home_gate()
        .ledger_lines()
        .into_iter()
        .skip(ledger_before)
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect();
    assert!(
        appended.is_empty(),
        "the preview appended to the ledger: {appended:?}"
    );
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
    out
}

const SEVERAL: &str = r#"[home]
version = "1"

[home.file.".config/git/ignore"]
text = ".lodi/\n"

[home.file.".gitconfig"]
text = "[core]\n\tautocrlf = input\n"

[home.file.".inputrc"]
text = "set editing-mode vi\n"
mode = "0600"
"#;

/// Before anything is applied, every declared path is one to create — and finding that out
/// creates nothing, not the data root, not the lock.
#[test]
fn a_declared_path_that_was_never_applied_is_unmanaged() {
    let env = home_env("status-unmanaged");
    write_manifest(&env, SEVERAL);

    let out = status(&env);
    assert!(out.stdout.is_empty(), "{}", stdout(&out));
    assert_eq!(
        home_part(&out),
        "\
create    .config/git/ignore        0644
create    .gitconfig                0644
create    .inputrc                  0600
3 files: 3 create
"
    );
    assert!(
        !env.share().join("lodi/home-scope").exists(),
        "a preview created the scope directory"
    );
    assert!(
        !config_root(&env).join("lodi.lock").exists(),
        "a preview wrote the lock"
    );
}

/// After a switch, every path is as declared: the preview has nothing to switch.
#[test]
fn an_applied_path_is_ok() {
    let env = home_env("status-ok");
    write_manifest(&env, SEVERAL);
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));

    let out = status(&env);
    assert!(nothing(&out), "{}", stderr(&out));
}

/// (d) A hand-edited managed file is a `drift` line in the preview — **at exit 0**: the preview
/// reports, and the switch is the gate (design call D6, `spec/11` §1).
#[test]
fn a_hand_edited_file_is_drift_at_exit_zero() {
    let env = home_env("status-drift");
    write_manifest(&env, SEVERAL);
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    fs::write(env.home().join(".gitconfig"), "[user]\n\tname = me\n").unwrap();

    // `status` asserts the exit status and the byte-identity of the root for us.
    let out = status(&env);
    let text = home_part(&out);
    assert!(
        text.contains("unchanged .config/git/ignore        0644\n"),
        "{text}"
    );
    assert!(
        text.contains(
            "drift     .gitconfig                0644    (edited by hand: lodi switch --home \
             --overwrite-drift takes it back)"
        ),
        "{text}"
    );
    assert!(text.ends_with("3 files: 1 drift, 2 unchanged\n"), "{text}");
    assert!(
        stderr(&out).contains("\nhome: 1 file\n"),
        "{}",
        stderr(&out)
    );
}

/// A managed file the user deleted is planned to be written again, and a managed file whose
/// mode was changed is updated to its declared mode (design call D6).
#[test]
fn a_deleted_file_is_missing_and_a_changed_mode_is_only_a_note() {
    let env = home_env("status-missing");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\"gone\"]\ntext = \"x\\n\"\n\n\
         [home.file.\"remoded\"]\ntext = \"y\\n\"\nmode = \"0600\"\n",
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
        home_part(&out),
        "\
create    gone                      0644
update    remoded                   0600
2 files: 1 create, 1 update
"
    );
}

/// A backup kept for a path the state no longer names stays in the store: lodi never removes a
/// backup (design call D5), and a home with nothing else to do has nothing to switch.
#[test]
fn a_backup_of_a_forgotten_path_is_reported_as_stale() {
    let env = home_env("status-stale");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".netrc\"]\ntext = \"lodi\\n\"\non_remove = \"delete\"\n",
    );
    fs::write(env.home().join(".netrc"), "the user's own\n").unwrap();
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    // The entry disappears, so the file is deleted and the state forgets the path — but the copy
    // of what the user had is still in the store.
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    assert_eq!(run(&env, &["home", "apply"]).status.code(), Some(0));
    assert!(!env.home().join(".netrc").exists());

    let out = status(&env);
    assert!(nothing(&out), "{}", stderr(&out));
    let kept: Vec<String> = fs::read_dir(env.share().join("lodi/home-scope/backups"))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != "index.json")
        .collect();
    assert_eq!(kept.len(), 1, "{kept:?}");
    let stored = env.share().join("lodi/home-scope/backups").join(&kept[0]);
    assert_eq!(fs::read_to_string(stored).unwrap(), "the user's own\n");
}

/// A path declared `state = "absent"` that is indeed absent is `unchanged`, and one that is still
/// there is a `remove`: work the switch has not done yet.
#[test]
fn an_absent_declaration_reports_whether_it_holds() {
    let env = home_env("status-absent");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\"is-gone\"]\nstate = \"absent\"\n\n\
         [home.file.\"is-still-here\"]\nstate = \"absent\"\n",
    );
    fs::write(env.home().join("is-still-here"), "the user's own\n").unwrap();

    let out = status(&env);
    assert_eq!(
        home_part(&out),
        "\
unchanged is-gone                   0644
remove    is-still-here             0644    (backup: first unmanaged copy kept)
2 files: 1 remove, 1 unchanged
"
    );
}

/// With no config at all, the preview is `E_NO_MANIFEST` at exit 3 naming the folder it looked
/// in, and it still creates nothing.
#[test]
fn a_status_without_a_manifest_names_the_file_it_wants() {
    let env = home_env("status-no-manifest");
    let before = tree(env.root());
    let out = run(&env, &["home", "status"]);
    let text = stderr(&out);
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_NO_MANIFEST"), "{text}");
    assert!(text.contains(".config/lodi"), "{text}");
    assert_eq!(
        tree(env.root()),
        before,
        "a refused preview wrote something"
    );
}

/// An empty manifest has nothing to switch, and says so rather than printing an empty page.
#[test]
fn an_empty_manifest_has_nothing_to_report() {
    let env = home_env("status-empty");
    write_manifest(&env, "[home]\nversion = \"1\"\n");
    let out = status(&env);
    assert!(nothing(&out), "{}", stderr(&out));
}
