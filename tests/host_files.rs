//! M-0.6 T-3 file behavior over armed scratch roots only.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use hostroot::{Root, backups, ids, journals};
use lodi::hostscope::journal::{self, Event};

fn manifest(root: &Root, path: &str, content: &str, extra: &str) -> String {
    let (uid, gid) = ids(root);
    format!(
        "[files.\"{path}\"]\ncontent = {content:?}\nowner = \"{uid}\"\ngroup = \"{gid}\"\n{extra}"
    )
}

#[test]
fn creates_declared_bytes_mode_owner_and_group_and_finishes_the_journal() {
    let root = Root::new("files-create");
    let (uid, gid) = ids(&root);
    root.debian_with(&manifest(
        &root,
        "/etc/lodi/example.conf",
        "configured\n",
        "mode = \"0640\"\n",
    ));

    let output = lodi::hostscope::apply(&root.options()).expect("apply");
    // check-host-safety: refusal — this is rendered output for a scratch-root action.
    assert!(output.contains("+ file /etc/lodi/example.conf"), "{output}");
    assert_eq!(root.read("etc/lodi/example.conf"), "configured\n");
    let meta = fs::metadata(root.path("etc/lodi/example.conf")).expect("managed file");
    assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
    assert_eq!((meta.uid(), meta.gid()), (uid, gid));

    let records = journals(&root);
    assert_eq!(records.len(), 1);
    let record = journal::read(&records[0]).expect("journal");
    assert!(!record.header.actions[0].pre[0].exists);
    assert!(matches!(record.events.last(), Some(Event::Finished { .. })));
    assert!(record.committed());

    assert_eq!(
        support::without_legacy(&lodi::hostscope::plan(&root.options()).unwrap()),
        // check-host-safety: refusal — rendered output, not a system path access.
        "= file /etc/lodi/example.conf (unchanged)\nnothing to do\n"
    );
}

/// A declared mode with a set-id bit, or one that lets anyone write the file, is named by
/// `W_FILE_MODE` before it is applied — and then applied exactly as declared, because the
/// manifest's author is trusted. Once it stands, a plan has nothing to do and says nothing.
#[test]
fn a_set_id_or_world_writable_mode_is_warned_and_still_applied() {
    let root = Root::new("files-notable-mode");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/setid.conf\"]\ncontent = \"s\\n\"\nmode = \"4755\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/shared.conf\"]\ncontent = \"w\\n\"\nmode = \"0666\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/plain.conf\"]\ncontent = \"p\\n\"\nmode = \"0644\"\n\
         owner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));

    let planned = lodi::hostscope::plan(&root.options()).expect("plan");
    for line in [
        "W_FILE_MODE: /etc/setid.conf declares mode 4755, which is set-uid; it is applied as declared",
        "W_FILE_MODE: /etc/shared.conf declares mode 0666, which lets anyone write the file; it is \
         applied as declared",
    ] {
        assert!(planned.contains(line), "{planned}");
    }
    assert!(!planned.contains("/etc/plain.conf declares"), "{planned}");

    let output = lodi::hostscope::apply(&root.options()).expect("apply");
    assert!(output.contains("W_FILE_MODE: /etc/setid.conf"), "{output}");
    for (rel, mode) in [
        ("etc/setid.conf", 0o4755),
        ("etc/shared.conf", 0o666),
        ("etc/plain.conf", 0o644),
    ] {
        let meta = fs::metadata(root.path(rel)).expect("managed file");
        assert_eq!(meta.permissions().mode() & 0o7777, mode, "{rel}");
    }
    let again = lodi::hostscope::plan(&root.options()).expect("plan");
    assert!(!again.contains("W_FILE_MODE"), "{again}");
    assert!(again.ends_with("nothing to do\n"), "{again}");
}

#[test]
fn unmanaged_file_is_warned_backed_up_and_restored_byte_for_byte() {
    let root = Root::new("files-restore");
    root.write("etc/example.conf", "original bytes\n");
    root.debian_with(&manifest(
        &root,
        "/etc/example.conf",
        "managed bytes\n",
        "on_remove = \"restore\"\n",
    ));

    let output = lodi::hostscope::apply(&root.options()).expect("take over");
    assert!(output.contains("W_REPLACED_UNMANAGED"), "{output}");
    assert_eq!(root.read("etc/example.conf"), "managed bytes\n");
    let kept = backups(&root);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(fs::read(&kept[0]).unwrap(), b"original bytes\n");
    assert_eq!(kept[0].parent(), Some(root.path("etc").as_path()));
    let backup = format!("/{}", kept[0].strip_prefix(&root.dir).unwrap().display());
    let lock = lodi::hostscope::lock::HostLock::read(&root.path("etc/lodi/host.lock"))
        .unwrap()
        .unwrap();
    assert_eq!(
        lock.files["/etc/example.conf"].backup.as_deref(),
        Some(backup.as_str())
    );
    let record = journal::read(&journals(&root)[0]).unwrap();
    assert!(
        record.header.actions[0]
            .post
            .iter()
            .any(|facts| facts.path == backup && facts.exists),
        "the journal post-state names the backup: {:?}",
        record.header.actions[0].post
    );

    // A later apply must carry the original backup reference forward rather than replacing or
    // forgetting it.
    root.write("etc/example.conf", "edited managed bytes\n");
    let mut confirmed = root.options();
    confirmed.overwrite_drift = true;
    lodi::hostscope::apply(&confirmed).expect("take a drifted managed file back");
    assert_eq!(backups(&root), kept);

    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("restore");
    assert!(output.contains("restore"), "{output}");
    assert_eq!(
        fs::read(root.path("etc/example.conf")).unwrap(),
        b"original bytes\n"
    );
    assert!(backups(&root).is_empty(), "the consumed backup is removed");
}

#[test]
fn delete_keep_and_backup_false_do_exactly_what_the_manifest_says() {
    let deleted = Root::new("files-delete");
    deleted.debian_with(&manifest(
        &deleted,
        "/etc/delete.conf",
        "delete me\n",
        "on_remove = \"delete\"\n",
    ));
    lodi::hostscope::apply(&deleted.options()).expect("create delete-policy file");
    deleted.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&deleted.options()).expect("delete departed file");
    assert!(output.contains("- file /etc/delete.conf"), "{output}");
    assert!(!deleted.exists("etc/delete.conf"));

    let kept = Root::new("files-keep");
    kept.debian_with(&manifest(
        &kept,
        "/etc/keep.conf",
        "keep me\n",
        "on_remove = \"keep\"\n",
    ));
    lodi::hostscope::apply(&kept.options()).expect("create keep-policy file");
    kept.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&kept.options()).expect("forget kept file");
    assert!(output.contains("kept; forget record"), "{output}");
    assert_eq!(kept.read("etc/keep.conf"), "keep me\n");
    assert_eq!(
        lodi::hostscope::plan(&kept.options()).unwrap(),
        "nothing to do\n"
    );

    let no_backup = Root::new("files-no-backup");
    no_backup
        .write("etc/unmanaged.conf", "old\n")
        .debian_with(&manifest(
            &no_backup,
            "/etc/unmanaged.conf",
            "new\n",
            "backup = false\n",
        ));
    let output = lodi::hostscope::apply(&no_backup.options()).expect("replace without backup");
    assert!(!output.contains("W_REPLACED_UNMANAGED"), "{output}");
    assert!(backups(&no_backup).is_empty());
    assert_eq!(no_backup.read("etc/unmanaged.conf"), "new\n");
}

#[test]
fn unknown_owner_or_group_is_apply_error_before_any_write() {
    for (field, value) in [("owner", "no-such-user"), ("group", "no-such-group")] {
        let root = Root::new(&format!("files-unknown-{field}"));
        let (uid, gid) = ids(&root);
        root.debian_with(&format!(
            "[files.\"/etc/identity.conf\"]\ncontent = \"x\\n\"\nowner = \"{}\"\ngroup = \"{}\"\n",
            if field == "owner" {
                value.to_string()
            } else {
                uid.to_string()
            },
            if field == "group" {
                value.to_string()
            } else {
                gid.to_string()
            },
        ));
        let error = lodi::hostscope::apply(&root.options()).expect_err("unknown identity");
        assert_eq!(error.codes(), vec!["E_APPLY"]);
        assert_eq!(error.exit_status(), 8);
        assert!(!root.exists("etc/identity.conf"));
        assert!(!root.exists("var/lib/lodi"));
    }
}

#[test]
fn drift_warns_in_plan_and_refuses_apply_until_confirmed() {
    let root = Root::new("files-drift");
    root.debian_with(&manifest(&root, "/etc/drift.conf", "declared\n", ""));
    lodi::hostscope::apply(&root.options()).expect("initial apply");
    root.write("etc/drift.conf", "edited\n");

    let plan = lodi::hostscope::plan(&root.options()).expect("plan");
    assert!(plan.contains("W_DRIFT"), "{plan}");
    let before = fs::read_dir(root.path("var/lib/lodi/host/journal"))
        .unwrap()
        .count();
    let error = lodi::hostscope::apply(&root.options()).expect_err("drift refusal");
    assert_eq!(error.codes(), vec!["E_DECLINED"]);
    assert_eq!(error.exit_status(), 11);
    assert_eq!(root.read("etc/drift.conf"), "edited\n");
    assert_eq!(
        fs::read_dir(root.path("var/lib/lodi/host/journal"))
            .unwrap()
            .count(),
        before
    );

    let mut confirmed = root.options();
    confirmed.overwrite_drift = true;
    lodi::hostscope::apply(&confirmed).expect("confirmed apply");
    assert_eq!(root.read("etc/drift.conf"), "declared\n");
}

#[test]
fn second_apply_is_a_true_noop_including_target_lock_and_state_mtimes() {
    let root = Root::new("files-idempotent");
    root.debian_with(&manifest(&root, "/etc/stable.conf", "stable\n", ""));
    lodi::hostscope::apply(&root.options()).expect("initial apply");
    let paths = [
        root.path("etc/stable.conf"),
        root.path("etc/lodi/host.lock"),
        root.path("var/lib/lodi/host/.lock"),
    ];
    let before: Vec<_> = paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().modified().unwrap())
        .collect();
    let count = journals(&root).len();
    std::thread::sleep(std::time::Duration::from_millis(20));
    assert_eq!(
        support::without_legacy(&lodi::hostscope::apply(&root.options()).unwrap()),
        "nothing to do\n"
    );
    let after: Vec<_> = paths
        .iter()
        .map(|path| fs::metadata(path).unwrap().modified().unwrap())
        .collect();
    assert_eq!(before, after);
    assert_eq!(journals(&root).len(), count);
}

/// M-0.6 T-4 and T-5 gave every distribution the gate accepts a backend, so a `[packages]`
/// declaration is no longer refused outright anywhere — but it is still refused *before any
/// apply state exists* when the tools that backend needs are not all there.
///
/// This suite's shims stand in for `apt-get` and `dpkg-query` only; `apt-mark`, which the
/// backend also needs, is not on this machine, and that is what stops the declaration here. On
/// a machine that does carry every apt program the first half is **skipped rather than run**: a
/// scratch-root apply there would reach for that machine's own apt, which no test may do.
/// `tests/host_apt.rs` drives that path on every machine, over shims it owns.
#[test]
fn packages_are_refused_before_any_apply_state_is_written() {
    use lodi::hostscope::safety::{Distro, Operation, which};

    let path = std::env::var_os("PATH").unwrap_or_default();
    let root = Root::new("files-packages");
    root.debian_with("[packages]\ncommon = [\"git\"]\n");
    if which("apt-mark", &path).is_none() {
        let error = lodi::hostscope::apply(&root.options()).expect_err("apt-mark is not here");
        assert_eq!(error.codes(), vec!["E_NO_RUNTIME"]);
        assert_eq!(error.exit_status(), 7);
        assert!(error.to_string().contains("apt-mark"), "{error}");
        assert!(!root.exists("var/lib/lodi"));
        assert!(!root.exists("etc/lodi/host.lock"));
    }

    // M-0.6 T-6 converged the two lanes: every distribution the gate accepts has an arm, the
    // `match` is total, and there is no "no backend" refusal left to reach. A `[packages]`
    // declaration therefore never meets `E_UNSUPPORTED` for want of a backend on any machine
    // this build runs on; what stops the declaration above is a missing apt program, not a
    // missing arm.
    for distro in [Distro::Debian, Distro::Ubuntu, Distro::Arch] {
        assert_eq!(
            lodi::hostscope::pm::backend_for(distro, &root.dir, false, Operation::Plan).distro(),
            distro
        );
    }
}

#[test]
fn no_destination_or_source_symlink_is_followed() {
    let root = Root::new("files-symlink");
    let outside = root.path("outside");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("unchanged"), "decoy\n").unwrap();
    root.debian_with(&manifest(&root, "/etc/link/file", "bad\n", ""));
    std::os::unix::fs::symlink(&outside, root.path("etc/link")).unwrap();
    let error = lodi::hostscope::apply(&root.options()).expect_err("destination symlink");
    assert_eq!(error.codes(), vec!["E_PATH_ESCAPE"]);
    assert_eq!(fs::read(outside.join("unchanged")).unwrap(), b"decoy\n");
    assert!(!outside.join("file").exists());
    assert!(!root.exists("var/lib/lodi"));

    fs::remove_file(root.path("etc/link")).unwrap();
    root.write("etc/lodi/source", "inside\n")
        .write(
            "etc/lodi/host.toml",
            &format!(
                "[files.\"/etc/from-source\"]\nsource = \"source-link\"\nowner = \"{}\"\ngroup = \"{}\"\n",
                ids(&root).0,
                ids(&root).1
            ),
        );
    std::os::unix::fs::symlink(outside.join("unchanged"), root.path("etc/lodi/source-link"))
        .unwrap();
    let error = lodi::hostscope::plan(&root.options()).expect_err("source symlink");
    assert_eq!(error.codes(), vec!["E_PATH_ESCAPE"]);
}

/// Host apply trusts a directory only if nobody else can write it: an existing ancestor of a
/// managed path — the root itself included — that is group- or world-writable is refused by
/// plan and apply alike before anything is changed, and the same tree with the ancestor fixed
/// applies. The owner half of the rule, which needs a second uid, is proved on the decision
/// function itself (`hostscope::files` unit tests).
#[test]
fn a_writable_ancestor_of_a_managed_path_is_refused_before_any_change() {
    let root = Root::new("files-writable-ancestor");
    root.write("etc/shared/app.conf", "original\n");
    root.debian_with(&manifest(&root, "/etc/shared/app.conf", "managed\n", ""));
    for (dir, mode) in [
        ("etc/shared", 0o775),
        ("etc/shared", 0o757),
        ("etc", 0o1777),
        ("", 0o770),
    ] {
        root.chmod(dir, mode);
        for (what, result) in [
            ("plan", lodi::hostscope::plan(&root.options())),
            ("apply", lodi::hostscope::apply(&root.options())),
        ] {
            let error = result.expect_err("an untrusted ancestor");
            assert_eq!(
                error.codes(),
                vec!["E_PATH_ESCAPE"],
                "{what} {dir:?} {mode:o}: {error}"
            );
            assert!(
                error.to_string().contains("is not a trusted directory"),
                "{what}: {error}"
            );
        }
        assert_eq!(root.read("etc/shared/app.conf"), "original\n");
        assert!(backups(&root).is_empty());
        assert!(!root.exists("var/lib/lodi"), "no apply state was written");
        root.chmod(dir, 0o755);
    }
    lodi::hostscope::apply(&root.options()).expect("the same tree with trusted ancestors");
    assert_eq!(root.read("etc/shared/app.conf"), "managed\n");
}

/// An owner or mode change reaches a file itself, and through it every other name the file
/// has, so it is made only to a managed file with a single link: a second link is refused
/// before any change, and the same file with one link is changed.
#[test]
fn an_owner_or_mode_change_is_refused_for_a_file_with_a_second_link() {
    let root = Root::new("files-hard-link");
    root.write("etc/linked.conf", "same bytes\n")
        .chmod("etc/linked.conf", 0o600);
    fs::hard_link(root.path("etc/linked.conf"), root.path("etc/other-name")).unwrap();
    root.debian_with(&manifest(
        &root,
        "/etc/linked.conf",
        "same bytes\n",
        "mode = \"0644\"\n",
    ));

    let error = lodi::hostscope::apply(&root.options()).expect_err("a second link");
    assert_eq!(error.codes(), vec!["E_PATH_ESCAPE"], "{error}");
    assert!(error.to_string().contains("2 hard links"), "{error}");
    let other = fs::metadata(root.path("etc/other-name")).unwrap();
    assert_eq!(
        other.permissions().mode() & 0o7777,
        0o600,
        "the other name is untouched"
    );
    assert!(!root.exists("var/lib/lodi"), "no apply state was written");

    fs::remove_file(root.path("etc/other-name")).unwrap();
    lodi::hostscope::apply(&root.options()).expect("the same file with one link");
    let meta = fs::metadata(root.path("etc/linked.conf")).unwrap();
    assert_eq!(meta.permissions().mode() & 0o7777, 0o644);
}

/// A directory apply creates on the way to a path is 0755 whatever the leaf is: a private file
/// gets no private parents, and only the host state directory and its journal directory, the
/// leaves the scope keeps for itself, are 0700.
#[test]
fn directories_apply_creates_on_the_way_are_0755_and_only_the_state_leaves_are_private() {
    let root = Root::new("files-parent-modes");
    root.debian_with(&manifest(
        &root,
        "/etc/new/deeper/app.conf",
        "private\n",
        "mode = \"0600\"\n",
    ));
    lodi::hostscope::apply(&root.options()).expect("apply");
    let mode = |rel: &str| fs::metadata(root.path(rel)).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode("etc/new/deeper/app.conf"), 0o600);
    for dir in [
        "etc/new",
        "etc/new/deeper",
        "var",
        "var/lib",
        "var/lib/lodi",
    ] {
        assert_eq!(mode(dir), 0o755, "{dir}");
    }
    for dir in ["var/lib/lodi/host", "var/lib/lodi/host/journal"] {
        assert_eq!(mode(dir), 0o700, "{dir}");
    }
}
