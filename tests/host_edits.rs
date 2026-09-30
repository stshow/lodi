//! LD-377: every edit of `host.toml` takes effect, and removing a `[files]` entry never deletes
//! a file Lodi did not create.
//!
//! The package half runs the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it; the file half calls the library over an armed scratch
//! root, as `tests/host_files.rs` does. Nothing reaches `/` and no real package manager runs
//! (`AGENTS.md` §8): every host command here carries `--root`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;

use fakehost::{Case, Family, Machine, Pkg, err, out, story};
use hostroot::{Root, backups, ids};
use lodi::hostscope::safety::{Gate, Operation};
use serde_json::Value;

/// The name the scratch roots give the invoking user, so that an owner can be written by name.
const INVOKER: &str = "lodi-edit";

/// An Arch machine with `tree` and `jq` chosen, `jq` pulling `oniguruma` in, `app` owning a
/// changed configuration file, and three packages the repositories offer.
fn arch() -> Machine {
    Machine::arch()
        .with(Pkg::new("tree"))
        .with(Pkg::new("jq").depends(&["oniguruma"]))
        .with(Pkg::new("oniguruma").dep())
        .with(Pkg::new("app").conffile("/etc/app.conf"))
        .offering(Pkg::new("bc"))
        .offering(Pkg::new("htop"))
        .offering(Pkg::new("zip"))
}

/// The same machine as Debian 12 has it.
fn debian() -> Machine {
    Machine::debian()
        .with(Pkg::new("tree").depends(&["libc6"]))
        .with(Pkg::new("jq").depends(&["libjq1"]))
        .with(Pkg::new("libjq1").dep().depends(&["libc6"]))
        .with(Pkg::new("app").conffile("/etc/app.conf"))
        .offering(Pkg::new("bc"))
        .offering(Pkg::new("htop"))
        .offering(Pkg::new("zip"))
}

fn machine_for(family: Family) -> Machine {
    match family {
        Family::Pacman => arch(),
        Family::Apt => debian(),
    }
}

fn distro(family: Family) -> &'static str {
    match family {
        Family::Pacman => "arch",
        Family::Apt => "debian",
    }
}

/// A fake machine whose root names the invoking user and holds `app`'s changed configuration.
fn case(name: &str, family: Family) -> Case {
    let case = Case::new(name, machine_for(family));
    let (uid, gid) = ids(&case.root);
    case.root
        .write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\n{INVOKER}:x:{uid}:{gid}::/nonexistent:/bin/sh\n"
            ),
        )
        .write("etc/group", &format!("root:x:0:\n{INVOKER}:x:{gid}:\n"))
        .write("etc/app.conf", "mine\n")
        .chmod("etc/app.conf", 0o644);
    case
}

/// Package-edit cases exercise the unpinned import: dated Arch sync is covered separately in
/// host_pin_arch.rs, not by the fake machine's old, unrecorded package fixtures (LD-396).
fn import_for_package_edits(case: &Case) -> std::process::Output {
    let result = case.import();
    if result.status.success() && case.family == Family::Pacman {
        case.set_manifest(&fakehost::without_snapshot(&case.manifest()));
    }
    result
}

fn replace_in_manifest(case: &Case, from: &str, to: &str) {
    let text = case.manifest();
    assert!(text.contains(from), "the manifest has no {from:?}:\n{text}");
    case.set_manifest(&text.replacen(from, to, 1));
}

/// Take a table out of the manifest: its header line and every line up to the next blank one.
fn remove_table(case: &Case, header: &str) {
    let text = case.manifest();
    let mut kept = String::new();
    let mut inside = false;
    let mut found = false;
    for line in text.lines() {
        if line == header {
            inside = true;
            found = true;
            continue;
        }
        if inside && line.trim().is_empty() {
            inside = false;
            continue;
        }
        if !inside {
            kept.push_str(line);
            kept.push('\n');
        }
    }
    assert!(found, "the manifest has no {header}:\n{text}");
    case.set_manifest(&kept);
}

/// Put a table at the end of the manifest, replacing the one with the same header if it is
/// there. Each table this file writes ends with a blank line.
fn set_table(case: &Case, header: &str, body: &str) {
    if case.manifest().lines().any(|line| line == header) {
        remove_table(case, header);
    }
    let text = case.manifest();
    case.set_manifest(&format!("{text}\n{header}\n{body}\n"));
}

fn mode_of(root: &Root, rel: &str) -> u32 {
    fs::metadata(root.path(rel))
        .expect("the file is there")
        .permissions()
        .mode()
        & 0o7777
}

// ---------------------------------------------------- A1: the two file defects, red first ---

/// (a) Import, declare the changed file under `[files]` as an earlier import wrote it (since
/// si-1 the import copies no file), apply, delete the entry, apply. The removal is planned — the
/// plan names the path — and honoured: the file Lodi never wrote is left exactly as it was, and
/// the record forgets it. A second apply has nothing to do.
fn a_deleted_files_entry_is_planned_and_honoured(family: Family) {
    let case = case(&format!("a1a-{family:?}"), family);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    assert!(
        !case.manifest().contains("[files."),
        "the import copied a file\n{}",
        case.manifest()
    );
    set_table(
        &case,
        "[files.\"/etc/app.conf\"]",
        &format!(
            "content = \"mine\\n\"\nmode = \"0644\"\nowner = \"{INVOKER}\"\ngroup = \"{INVOKER}\"\n"
        ),
    );
    let first = case.apply(&[]);
    assert!(first.status.success(), "{}", story(&first));

    remove_table(&case, "[files.\"/etc/app.conf\"]");
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        out(&plan).contains("file /etc/app.conf"),
        "the removal of the entry is not planned\n{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        out(&apply).contains("file /etc/app.conf"),
        "the apply does not name the file\n{}",
        story(&apply)
    );
    assert_eq!(
        case.root.read("etc/app.conf"),
        "mine\n",
        "{}",
        story(&apply)
    );
    assert_eq!(mode_of(&case.root, "etc/app.conf"), 0o644);
    assert!(
        case.lock()["files"]["/etc/app.conf"].is_null(),
        "the record still names the file: {}",
        case.lock()
    );
    let again = case.apply(&[]);
    assert_eq!(out(&again), "nothing to do\n", "{}", story(&again));
}

#[test]
fn a1_a_deleted_files_entry_after_an_import_is_planned_and_honoured_on_pacman() {
    a_deleted_files_entry_is_planned_and_honoured(Family::Pacman);
}

#[test]
fn a1_a_deleted_files_entry_after_an_import_is_planned_and_honoured_on_apt() {
    a_deleted_files_entry_is_planned_and_honoured(Family::Apt);
}

/// One declared file, as `[files]` writes it for the invoking user.
fn entry(root: &Root, path: &str, content: &str, mode: &str, extra: &str) -> String {
    let (uid, gid) = ids(root);
    format!(
        "[files.\"{path}\"]\ncontent = {content:?}\nmode = \"{mode}\"\nowner = \"{uid}\"\n\
         group = \"{gid}\"\n{extra}\n"
    )
}

/// A root holding a configuration file Lodi did not write: `original\n`, mode 0640.
fn root_with_original(name: &str) -> Root {
    let root = Root::new(name);
    root.write("etc/pre.conf", "original\n")
        .chmod("etc/pre.conf", 0o640);
    root
}

fn assert_original(root: &Root, what: &str) {
    assert!(
        root.exists("etc/pre.conf"),
        "{what}: a file lodi did not create was deleted"
    );
    assert_eq!(root.read("etc/pre.conf"), "original\n", "{what}");
    assert_eq!(mode_of(root, "etc/pre.conf"), 0o640, "{what}");
}

/// (b) A file first recorded while it was already `unchanged` — recorded by an apply that did
/// other work — and then removed from the manifest with `on_remove = "restore"`: before LD-377
/// the record had no backup and the restore became a delete.
#[test]
fn a1_a_file_first_recorded_while_unchanged_is_never_deleted() {
    let root = root_with_original("a1b-unchanged");
    let pre = entry(&root, "/etc/pre.conf", "original\n", "0640", "");
    let other = entry(&root, "/etc/other.conf", "other\n", "0644", "");
    root.debian_with(&format!("{pre}\n{other}"));
    lodi::hostscope::apply(&root.options()).expect("the apply that records both");

    root.write("etc/lodi/host.toml", &other);
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(output.contains("file /etc/pre.conf"), "{output}");
    assert_original(&root, "removed while unchanged");
    assert_eq!(
        support::without_legacy(&lodi::hostscope::apply(&root.options()).unwrap()),
        "nothing to do\n"
    );
}

/// (b), the same file changed by a later edit and then removed: the original bytes and mode
/// come back, and the copy that carried them is consumed.
#[test]
fn a1_a_file_adopted_unchanged_then_changed_is_restored_on_removal() {
    let root = root_with_original("a1b-changed");
    let other = entry(&root, "/etc/other.conf", "other\n", "0644", "");
    root.debian_with(&format!(
        "{}\n{other}",
        entry(&root, "/etc/pre.conf", "original\n", "0640", "")
    ));
    lodi::hostscope::apply(&root.options()).expect("adopt");
    root.write(
        "etc/lodi/host.toml",
        &format!(
            "{}\n{other}",
            entry(&root, "/etc/pre.conf", "managed\n", "0600", "")
        ),
    );
    lodi::hostscope::apply(&root.options()).expect("change it");
    assert_eq!(root.read("etc/pre.conf"), "managed\n");
    assert_eq!(mode_of(&root, "etc/pre.conf"), 0o600);

    root.write("etc/lodi/host.toml", &other);
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(
        output.contains("~ file /etc/pre.conf (restore "),
        "{output}"
    );
    assert_original(&root, "adopted unchanged, changed, removed");
    assert!(backups(&root).is_empty(), "the consumed copy is removed");
}

// ------------------------------------------ A2 (LD-387): the original is kept from adoption ---

/// The original is fixed at adoption, not at the first later write: a hand edit in between
/// cannot replace the original in the restore copy, even when explicitly overwritten.
#[test]
fn a2_adoption_precedes_a_later_external_edit() {
    let root = root_with_original("a2-external-after-adopt");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    lodi::hostscope::apply(&root.options()).expect("adopt unchanged");
    root.write("etc/pre.conf", "external\n")
        .chmod("etc/pre.conf", 0o600);
    root.write(
        "etc/lodi/host.toml",
        &entry(&root, "/etc/pre.conf", "managed\n", "0644", ""),
    );
    let mut options = root.options();
    options.overwrite_drift = true;
    lodi::hostscope::apply(&options).expect("explicitly overwrite external drift");
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("restore adoption-time original");
    assert_original(&root, "external edit after adoption");
    assert!(backups(&root).is_empty());
}

/// Every adoption copy in the store below the root, sorted.
fn store(root: &Root) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![root.path(STORE)];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).into_iter().flatten().flatten() {
            let path = entry.path();
            if path.symlink_metadata().is_ok_and(|meta| meta.is_dir()) {
                dirs.push(path);
            } else {
                out.push(path);
            }
        }
    }
    out.sort();
    out
}

/// Where the adoption copies live, relative to the root (LD-387).
const STORE: &str = "etc/lodi/originals";

/// The file record the lock holds for a path; only its keys and values, never the lock's
/// `format` or `version`, are asserted by h-8's tests.
fn record_of(root: &Root, path: &str) -> Value {
    let lock: Value =
        serde_json::from_str(&root.read("etc/lodi/host.lock")).expect("the lock parses");
    lock["files"][path].clone()
}

/// The keys a file record may have: 1.1.1's, and no other (LD-387 adds none).
fn assert_record_keys(record: &Value) {
    let object = record
        .as_object()
        .unwrap_or_else(|| panic!("a file record: {record}"));
    for key in object.keys() {
        assert!(
            ["digest", "mode", "owner", "group", "backup", "onRemove"].contains(&key.as_str()),
            "{key}: {record}"
        );
    }
}

/// The one adoption copy of `/etc/pre.conf`, asserted to hold the original's bytes, mode, owner
/// and group, below directories that are the invoker's and 0700; returned as the in-root path
/// the record must name.
fn assert_adoption_copy(root: &Root) -> String {
    use std::os::unix::fs::MetadataExt;
    let copies = store(root);
    assert_eq!(copies.len(), 1, "one adoption copy: {copies:?}");
    let copy = &copies[0];
    let name = copy.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.starts_with("pre.conf.lodi-backup-"), "{name}");
    assert_eq!(
        copy.parent().unwrap(),
        root.path(&format!("{STORE}/etc")),
        "the managed path is mirrored below the store"
    );
    assert_eq!(fs::read_to_string(copy).unwrap(), "original\n");
    let meta = fs::symlink_metadata(copy).unwrap();
    let (uid, gid) = ids(root);
    assert_eq!(meta.mode() & 0o7777, 0o640, "the copy keeps the mode");
    assert_eq!(
        (meta.uid(), meta.gid()),
        (uid, gid),
        "the copy keeps owner and group"
    );
    for dir in [STORE.to_string(), format!("{STORE}/etc")] {
        let meta = fs::symlink_metadata(root.path(&dir)).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o700, "{dir}");
        assert_eq!(meta.uid(), uid, "{dir}");
    }
    format!("/{}", copy.strip_prefix(&root.dir).unwrap().display())
}

/// IM.3's shape with no record: the plan line is `unchanged` and says only that the record would
/// change; the apply changes nothing on the machine, takes the adoption copy into the store and
/// records it with `restore`; a second apply has nothing to do.
#[test]
fn a2_an_apply_that_first_records_an_equal_file_keeps_its_original_in_the_store() {
    let root = root_with_original("a2-store");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    let planned = lodi::hostscope::plan(&root.options()).expect("plan");
    assert!(
        planned.contains("= file /etc/pre.conf (unchanged)"),
        "{planned}"
    );
    assert!(
        planned.contains("no machine changes; apply would update the record"),
        "{planned}"
    );
    assert!(store(&root).is_empty(), "a plan writes nothing");

    let applied = support::without_legacy(&lodi::hostscope::apply(&root.options()).expect("adopt"));
    assert_eq!(applied, "no machine changes; record updated\n");
    let copy = assert_adoption_copy(&root);
    let record = record_of(&root, "/etc/pre.conf");
    assert_record_keys(&record);
    assert_eq!(record["backup"], copy.as_str(), "{record}");
    assert_eq!(record["onRemove"], "restore", "{record}");
    assert_original(&root, "adoption writes nothing at the path");
    assert_eq!(
        support::without_legacy(&lodi::hostscope::apply(&root.options()).unwrap()),
        "nothing to do\n"
    );
    assert_eq!(store(&root).len(), 1, "a second apply takes no second copy");
}

/// The apply that records the file while it does other work (`write_lock`) takes the same copy.
#[test]
fn a2_an_apply_that_does_other_work_keeps_the_original_of_an_equal_file_too() {
    let root = root_with_original("a2-store-other");
    let other = entry(&root, "/etc/other.conf", "other\n", "0644", "");
    root.debian_with(&format!(
        "{}\n{other}",
        entry(&root, "/etc/pre.conf", "original\n", "0640", "")
    ));
    let applied = lodi::hostscope::apply(&root.options()).expect("create and adopt");
    assert!(applied.contains("+ file /etc/other.conf"), "{applied}");
    let copy = assert_adoption_copy(&root);
    let record = record_of(&root, "/etc/pre.conf");
    assert_eq!(record["backup"], copy.as_str(), "{record}");
    assert_eq!(record["onRemove"], "restore", "{record}");
    // The file Lodi created has no original and no copy.
    assert!(record_of(&root, "/etc/other.conf")["backup"].is_null());

    root.write("etc/lodi/host.toml", &other);
    let removed = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(
        removed.contains(&format!("~ file /etc/pre.conf (restore {copy})")),
        "{removed}"
    );
    assert_original(&root, "restored from the store");
    assert!(store(&root).is_empty(), "the restore consumes the copy");
}

/// A restore from the store is journalled by the existing `restore_from` bracket: the copy is
/// present before the action and absent after it.
#[test]
fn a2_a_restore_from_the_store_is_journalled() {
    let root = root_with_original("a2-journal");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    lodi::hostscope::apply(&root.options()).expect("adopt");
    let copy = assert_adoption_copy(&root);
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, "restored");
    let restores: Vec<_> = hostroot::journals(&root)
        .iter()
        .map(|path| lodi::hostscope::journal::read(path).expect("a journal"))
        .flat_map(|record| record.header.actions)
        .filter(|entry| entry.kind == "file.restore")
        .collect();
    assert_eq!(restores.len(), 1, "{restores:?}");
    let entry = &restores[0];
    assert!(
        entry
            .pre
            .iter()
            .any(|facts| facts.path == copy && facts.exists),
        "{entry:?}"
    );
    assert!(
        entry
            .post
            .iter()
            .any(|facts| facts.path == copy && !facts.exists),
        "{entry:?}"
    );
}

/// `backup = false`, `on_remove = "keep"` and `on_remove = "delete"` take no store copy and
/// behave as 1.1.1 did.
#[test]
fn a2_no_store_copy_without_backup_and_restore() {
    for (extra, on_remove, left) in [
        ("backup = false\n", "keep", true),
        ("on_remove = \"keep\"\n", "keep", true),
        ("on_remove = \"delete\"\n", "delete", false),
    ] {
        let root = root_with_original(&format!("a2-no-store-{on_remove}-{}", extra.len()));
        root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", extra));
        let applied =
            support::without_legacy(&lodi::hostscope::apply(&root.options()).expect("adopt"));
        assert_eq!(applied, "no machine changes; record updated\n", "{extra}");
        assert!(store(&root).is_empty(), "{extra}");
        let record = record_of(&root, "/etc/pre.conf");
        assert!(record["backup"].is_null(), "{extra}: {record}");
        assert_eq!(record["onRemove"], on_remove, "{extra}: {record}");
        root.write("etc/lodi/host.toml", "");
        lodi::hostscope::apply(&root.options()).expect("the removal");
        assert_eq!(root.exists("etc/pre.conf"), left, "{extra}");
        assert!(backups(&root).is_empty(), "{extra}");
    }
}

/// The record lodi 1.1.1 wrote for a file it adopted unchanged: no copy, `onRemove` `keep`.
fn write_a_1_1_1_record(root: &Root) {
    let (uid, gid) = ids(root);
    let digest = format!(
        "sha256:{}",
        lodi::util::sha256_hex(root.read("etc/pre.conf").as_bytes())
    );
    root.write(
        "etc/lodi/host.lock",
        &format!(
            "{{\n  \"version\": 1,\n  \"format\": \"lodi-host-lock/1\",\n  \"generatedBy\": \
             \"lodi 1.1.1\",\n  \"appliedAt\": \"2026-09-23T12:00:00Z\",\n  \"distro\": \
             \"debian\",\n  \"distroVersion\": \"12\",\n  \"packages\": {{}},\n  \"files\": {{\n    \
             \"/etc/pre.conf\": {{\n      \"digest\": \"{digest}\",\n      \"mode\": \"0640\",\n      \
             \"owner\": \"{uid}\",\n      \"group\": \"{gid}\",\n      \"onRemove\": \"keep\"\n    }}\n  \
             }}\n}}\n"
        ),
    );
}

/// 1.1.1's record, the file unchanged: no retroactive copy is taken, and the store stays empty.
#[test]
fn a2_a_1_1_1_record_gets_no_retroactive_copy() {
    let root = root_with_original("a2-111-unchanged");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    write_a_1_1_1_record(&root);
    // The first apply may bring the record's other halves up to date; the file record is kept.
    lodi::hostscope::apply(&root.options()).expect("an apply over the unchanged file");
    assert_eq!(
        support::without_legacy(&lodi::hostscope::apply(&root.options()).unwrap()),
        "nothing to do\n"
    );
    assert!(store(&root).is_empty(), "{:?}", store(&root));
    let record = record_of(&root, "/etc/pre.conf");
    assert!(record["backup"].is_null(), "{record}");
    assert_eq!(record["onRemove"], "keep", "{record}");
}

/// 1.1.1's record, the entry kept and Lodi writing the file: the file is copied beside itself
/// first, as 1.1.1 does, and a later removal restores those pre-write bytes.
#[test]
fn a2_a_1_1_1_record_is_copied_beside_the_file_before_a_write() {
    let root = root_with_original("a2-111-write");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    write_a_1_1_1_record(&root);
    root.write(
        "etc/lodi/host.toml",
        &entry(&root, "/etc/pre.conf", "managed\n", "0600", ""),
    );
    let output = lodi::hostscope::apply(&root.options()).expect("change it");
    assert_eq!(root.read("etc/pre.conf"), "managed\n", "{output}");
    let beside = backups(&root);
    assert_eq!(beside.len(), 1, "{output}");
    assert_eq!(
        beside[0].parent().unwrap(),
        root.path("etc"),
        "beside the file"
    );
    assert!(store(&root).is_empty());
    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, &format!("1.1.1 record, changed, removed\n{output}"));
    assert!(backups(&root).is_empty(), "the consumed copy is removed");
}

/// 1.1.1's record, the entry gone before any write: the file is left and the record forgotten.
#[test]
fn a2_a_1_1_1_record_removed_before_a_write_is_left_in_place() {
    let root = root_with_original("a2-111-removed");
    root.debian_with("");
    write_a_1_1_1_record(&root);
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, &format!("1.1.1 record removed\n{output}"));
    assert!(record_of(&root, "/etc/pre.conf").is_null());
    assert!(store(&root).is_empty() && backups(&root).is_empty());
}

/// A file in the store that no record names is neither read nor deleted, by a plan, an apply or
/// a removal; a later adoption picks a name of its own.
#[test]
fn a2_an_orphan_in_the_store_is_left_alone() {
    let root = root_with_original("a2-orphan");
    let orphan = format!("{STORE}/etc/pre.conf.lodi-backup-20260101T000000Z-000000001");
    root.write(&orphan, "orphan\n").chmod(&orphan, 0o000);
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    lodi::hostscope::plan(&root.options()).expect("plan");
    lodi::hostscope::apply(&root.options()).expect("adopt");
    assert_eq!(store(&root).len(), 2, "{:?}", store(&root));
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, "restored beside an orphan");
    assert_eq!(
        store(&root),
        vec![root.path(&orphan)],
        "only the orphan is left"
    );
    root.chmod(&orphan, 0o600);
    assert_eq!(root.read(&orphan), "orphan\n");
}

/// A copy that cannot be made leaves the record in 1.1.1's shape, says so on one plain line,
/// and does not fail the apply.
#[test]
fn a2_a_copy_that_cannot_be_made_falls_back_to_the_1_1_1_record() {
    let root = root_with_original("a2-fallback");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    // The store's own name is taken by a regular file: no directory can be made there.
    root.write(STORE, "not a directory\n");
    let applied = support::without_legacy(
        &lodi::hostscope::apply(&root.options()).expect("the apply does not fail"),
    );
    let lines: Vec<&str> = applied.lines().collect();
    assert_eq!(lines.len(), 2, "{applied}");
    assert!(
        lines[0].starts_with("/etc/pre.conf: no copy of the original was kept"),
        "{applied}"
    );
    assert_eq!(lines[1], "no machine changes; record updated", "{applied}");
    let record = record_of(&root, "/etc/pre.conf");
    assert!(record["backup"].is_null(), "{record}");
    assert_eq!(record["onRemove"], "keep", "{record}");
    assert_eq!(root.read(STORE), "not a directory\n");
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, "left in place without a copy");
}

/// Every path below a directory with its kind, mode, size and mtime.
fn census(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    let mut out = std::collections::BTreeMap::new();
    let mut dirs = vec![dir.to_path_buf()];
    while let Some(next) = dirs.pop() {
        for entry in fs::read_dir(&next).into_iter().flatten().flatten() {
            let path = entry.path();
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            let rel = path.strip_prefix(dir).unwrap().display().to_string();
            out.insert(
                rel,
                format!(
                    "{} {:o} {} {}.{}",
                    meta.is_dir(),
                    meta.mode(),
                    meta.len(),
                    meta.mtime(),
                    meta.mtime_nsec()
                ),
            );
            if meta.is_dir() {
                dirs.push(path);
            }
        }
    }
    out
}

/// The in-place import copies and adopts no file (si-1): its three lines are unchanged, nothing
/// outside the root's `etc/lodi` is written (IM.5), no copy is taken into the store, and the
/// changed file is named with the `[etc."PATH"]` that declares it. Declared that way and applied,
/// its original is kept in the store; the entry removed, the bytes and mode it had come back.
fn an_in_place_import_adopts_nothing(family: Family) {
    let case = case(&format!("a2-import-{family:?}"), family);
    let before = census(&case.root.dir);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let told = out(&import);
    let lines: Vec<&str> = told.lines().collect();
    assert_eq!(lines.len(), 3, "the import's three lines:\n{told}");
    assert!(
        lines[0].ends_with(&format!(
            "no package and no file outside {} changed",
            case.root.path("etc/lodi").display()
        )),
        "{told}"
    );
    let after = census(&case.root.dir);
    for (path, what) in &before {
        if path != "etc/lodi" && path != "etc" {
            assert_eq!(after.get(path), Some(what), "{path} moved");
        }
    }
    for path in after.keys().filter(|k| !before.contains_key(*k)) {
        assert!(path.starts_with("etc/lodi/"), "the import wrote {path}");
    }
    assert!(store(&case.root).is_empty(), "{:?}", store(&case.root));
    assert!(!case.root.exists("etc/lodi/files"));
    let text = case.manifest();
    assert!(
        text.contains("#   /etc/app.conf\n#     declare its text as [etc.\"app.conf\"]\n"),
        "{text}"
    );
    assert!(
        case.lock()["files"]["/etc/app.conf"].is_null(),
        "{}",
        case.lock()
    );

    set_table(
        &case,
        "[etc.\"app.conf\"]",
        &format!("text = \"managed\\n\"\nowner = \"{INVOKER}\"\ngroup = \"{INVOKER}\"\n"),
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(case.root.read("etc/app.conf"), "managed\n");
    assert_eq!(
        store(&case.root).len(),
        1,
        "the original is kept in the store"
    );
    assert_eq!(
        backups(&case.root),
        store(&case.root),
        "no copy beside the file"
    );

    remove_table(&case, "[etc.\"app.conf\"]");
    let removed = case.apply(&[]);
    assert!(removed.status.success(), "{}", story(&removed));
    assert_eq!(
        case.root.read("etc/app.conf"),
        "mine\n",
        "the original bytes\n{}",
        story(&removed)
    );
    assert_eq!(mode_of(&case.root, "etc/app.conf"), 0o644);
    assert!(
        store(&case.root).is_empty(),
        "the restore consumes the copy"
    );
    assert!(backups(&case.root).is_empty());
}

#[test]
fn a2_an_in_place_import_adopts_nothing_on_apt() {
    an_in_place_import_adopts_nothing(Family::Apt);
}

#[test]
fn a2_an_in_place_import_adopts_nothing_on_pacman() {
    an_in_place_import_adopts_nothing(Family::Pacman);
}

/// A reconcile over a file a `[files]` entry declares, changed on the machine since, copies
/// nothing (si-1): it says so, keeps the declaration, and leaves the record as it was.
#[test]
fn a2_a_reconcile_of_a_declared_file_changed_on_the_machine_copies_nothing() {
    let case = case("a2-reconcile-again", Family::Apt);
    assert!(case.import().status.success());
    set_table(
        &case,
        "[files.\"/etc/app.conf\"]",
        &format!(
            "source = \"files/etc/app.conf\"\nmode = \"0644\"\nowner = \"{INVOKER}\"\n\
             group = \"{INVOKER}\"\n"
        ),
    );
    case.root.write("etc/lodi/files/etc/app.conf", "managed\n");
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    let recorded = case.lock()["files"]["/etc/app.conf"].clone();
    assert!(recorded["backup"].is_string(), "{recorded}");
    assert_eq!(recorded["onRemove"], "restore", "{recorded}");

    case.root
        .write("etc/app.conf", "changed on the machine\n")
        .chmod("etc/app.conf", 0o664);
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        err(&again).contains(
            "W_RECONCILE: /etc/app.conf changed on this machine and is not captured again"
        ),
        "{}",
        story(&again)
    );
    assert!(case.manifest().contains("[files.\"/etc/app.conf\"]"));
    assert_eq!(case.root.read("etc/lodi/files/etc/app.conf"), "managed\n");
    assert_eq!(case.lock()["files"]["/etc/app.conf"], recorded);
}

/// A reconcile that finds a file changed for the first time names it with its `[etc."PATH"]`,
/// and declares, copies and adopts nothing (si-1).
#[test]
fn a2_a_reconcile_names_a_newly_changed_file_and_adopts_nothing() {
    let case = case("a2-reconcile-first", Family::Apt);
    fs::remove_file(case.root.path("etc/app.conf")).unwrap();
    let first = case.import();
    assert!(first.status.success(), "{}", story(&first));
    case.root
        .write("etc/app.conf", "mine\n")
        .chmod("etc/app.conf", 0o644);
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    let text = case.manifest();
    assert!(!text.contains("[files."), "{}\n{text}", story(&again));
    assert!(
        text.contains("#   /etc/app.conf\n#     declare its text as [etc.\"app.conf\"]\n"),
        "{}\n{text}",
        story(&again)
    );
    assert!(store(&case.root).is_empty(), "{:?}", store(&case.root));
    assert!(!case.root.exists("etc/lodi/files"));
    assert!(
        case.lock()["files"]["/etc/app.conf"].is_null(),
        "{}",
        case.lock()
    );
}

// ------------------------------------------------------------------- A2: what adoption keeps ---

/// A file that differs only in its mode is adopted by a metadata change. Its removal puts the
/// original mode back; before LD-377 it deleted the file.
#[test]
fn a2_a_file_adopted_by_its_mode_alone_gets_its_mode_back() {
    let root = root_with_original("a2-mode");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0600", ""));
    let output = lodi::hostscope::apply(&root.options()).expect("adopt by mode");
    assert!(
        output.contains("~ file /etc/pre.conf (mode 0640 -> 0600"),
        "{output}"
    );
    assert_eq!(mode_of(&root, "etc/pre.conf"), 0o600);
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, "adopted by mode");
    assert!(backups(&root).is_empty(), "the consumed copy is removed");
}

/// A file whose bytes differ is kept once and restored exactly, mode included: 1.1.0's rule,
/// which must survive.
#[test]
fn a2_a_file_adopted_by_replacing_it_is_restored_exactly() {
    let root = root_with_original("a2-replace");
    root.debian_with(&entry(&root, "/etc/pre.conf", "managed\n", "0644", ""));
    lodi::hostscope::apply(&root.options()).expect("take over");
    assert_eq!(root.read("etc/pre.conf"), "managed\n");
    root.write("etc/lodi/host.toml", "");
    lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(&root, "adopted by replacing");
    // The restore forgot the path: the next apply leaves the original alone. Before LD-377 the
    // restored path stayed in the record, and this apply deleted it.
    let again = lodi::hostscope::apply(&root.options()).expect("a second apply");
    assert_original(&root, &format!("the apply after the restore\n{again}"));
    assert_eq!(again, "nothing to do\n");
}

/// A file Lodi created is removed on restore, as it always was.
#[test]
fn a2_a_file_lodi_created_is_removed_on_restore() {
    let root = Root::new("a2-created");
    root.debian_with(&entry(&root, "/etc/made.conf", "made\n", "0644", ""));
    lodi::hostscope::apply(&root.options()).expect("create");
    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(output.contains("- file /etc/made.conf"), "{output}");
    assert!(!root.exists("etc/made.conf"));
}

/// `backup = false` keeps no copy, so there is nothing to restore from: the file Lodi did not
/// create is then left where it is when its entry goes, and the plan says so when it takes it
/// over. Before LD-377 the restore became a delete.
#[test]
fn a2_a_file_adopted_without_a_copy_is_left_in_place_when_removed() {
    let root = root_with_original("a2-no-copy");
    root.debian_with(&entry(
        &root,
        "/etc/pre.conf",
        "managed\n",
        "0644",
        "backup = false\n",
    ));
    let planned = lodi::hostscope::plan(&root.options()).expect("plan");
    assert!(
        planned.contains("~ file /etc/pre.conf (") && planned.contains("no copy kept"),
        "{planned}"
    );
    lodi::hostscope::apply(&root.options()).expect("take over without a copy");
    assert!(backups(&root).is_empty());
    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(output.contains("file /etc/pre.conf"), "{output}");
    assert!(
        root.exists("etc/pre.conf"),
        "a file lodi did not create was deleted"
    );
    assert_eq!(root.read("etc/pre.conf"), "managed\n");
}

/// The record of an adoption is carried by the schema-1 file fields: no new key in a file's
/// record. The lock itself is schema 2 since LD-378, which added the baseline beside them.
#[test]
fn a2_the_file_record_keeps_its_schema_1_fields() {
    let root = root_with_original("a2-schema");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    lodi::hostscope::apply(&root.options()).expect("adopt");
    let lock: Value =
        serde_json::from_str(&root.read("etc/lodi/host.lock")).expect("the lock parses");
    assert_eq!(lock["format"], "lodi-host-lock/2", "{lock}");
    assert_eq!(lock["version"], 2, "{lock}");
    assert_eq!(lock["source"], "/etc/lodi", "{lock}");
    assert!(
        lock["baseline"]["files"]["/etc/pre.conf"].is_string(),
        "the apply advanced the baseline: {lock}"
    );
    let record = lock["files"]["/etc/pre.conf"]
        .as_object()
        .unwrap_or_else(|| panic!("the adopted file is recorded: {lock}"));
    for key in record.keys() {
        assert!(
            ["digest", "mode", "owner", "group", "backup", "onRemove"].contains(&key.as_str()),
            "{key}: {lock}"
        );
    }
}

/// The record 1.1.0 wrote for a file it found already right: no copy, `onRemove` `restore` — the
/// same bytes it wrote for a file it created. Written here as that release wrote it.
fn write_an_earlier_record(root: &Root, generated_by: &str) {
    let (uid, gid) = ids(root);
    let digest = format!(
        "sha256:{}",
        lodi::util::sha256_hex(root.read("etc/pre.conf").as_bytes())
    );
    root.write(
        "etc/lodi/host.lock",
        &format!(
            "{{\n  \"version\": 1,\n  \"format\": \"lodi-host-lock/1\",\n  \"generatedBy\": \
             \"{generated_by}\",\n  \"appliedAt\": \"2026-09-23T12:00:00Z\",\n  \"distro\": \
             \"debian\",\n  \"distroVersion\": \"12\",\n  \"packages\": {{}},\n  \"files\": {{\n    \
             \"/etc/pre.conf\": {{\n      \"digest\": \"{digest}\",\n      \"mode\": \"0640\",\n      \
             \"owner\": \"{uid}\",\n      \"group\": \"{gid}\",\n      \"onRemove\": \"restore\"\n    }}\n  \
             }}\n}}\n"
        ),
    );
}

/// An earlier release's record with no copy cannot say whether lodi created the file or found
/// it: its removal never deletes the file — it is left where it stands, the plan says why, and the
/// record forgets it (LD-377 validator repair).
#[test]
fn a2_an_earlier_releases_unbacked_record_never_deletes_the_file() {
    for writer in ["lodi 1.1.0", "lodi 1.0.0"] {
        let root = root_with_original(&format!("a2-earlier-{}", writer.replace(' ', "-")));
        root.debian_with("");
        write_an_earlier_record(&root, writer);
        let planned = lodi::hostscope::plan(&root.options()).expect("plan");
        assert!(
            planned.contains("= file /etc/pre.conf (") && planned.contains("left in place"),
            "{writer}: the plan does not say the file is left\n{planned}"
        );
        let output = lodi::hostscope::apply(&root.options()).expect("the removal");
        assert_original(&root, &format!("{writer}: removed\n{output}"));
        let lock: Value = serde_json::from_str(&root.read("etc/lodi/host.lock")).expect("a lock");
        assert!(lock["files"].get("/etc/pre.conf").is_none(), "{lock}");
        assert_eq!(
            lodi::hostscope::apply(&root.options()).unwrap(),
            "nothing to do\n"
        );
    }
}

/// The same record, and the entry kept and changed: the file is treated as an original, so its
/// bytes are kept before lodi writes over them and a later removal puts them back.
#[test]
fn a2_an_earlier_releases_unbacked_record_keeps_the_original_before_a_write() {
    let root = root_with_original("a2-earlier-write");
    root.debian_with(&entry(&root, "/etc/pre.conf", "original\n", "0640", ""));
    write_an_earlier_record(&root, "lodi 1.1.0");
    root.write(
        "etc/lodi/host.toml",
        &entry(&root, "/etc/pre.conf", "managed\n", "0600", ""),
    );
    let output = lodi::hostscope::apply(&root.options()).expect("change it");
    assert_eq!(root.read("etc/pre.conf"), "managed\n", "{output}");
    assert_eq!(
        backups(&root).len(),
        1,
        "the original is kept first\n{output}"
    );
    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert_original(
        &root,
        &format!("earlier record, changed, removed\n{output}"),
    );
    assert!(backups(&root).is_empty(), "the consumed copy is removed");
}

/// A record this release wrote for a file it created still means what it says: `restore`
/// removes it. Only a record an earlier release wrote is read as possibly an original.
#[test]
fn a2_this_releases_record_of_a_created_file_is_still_removed() {
    let root = Root::new("a2-created-now");
    root.debian_with(&entry(&root, "/etc/made.conf", "made\n", "0644", ""));
    lodi::hostscope::apply(&root.options()).expect("create");
    let lock: Value = serde_json::from_str(&root.read("etc/lodi/host.lock")).expect("a lock");
    assert_eq!(
        lock["generatedBy"],
        format!("lodi {}", env!("CARGO_PKG_VERSION")),
        "{lock}"
    );
    root.write("etc/lodi/host.toml", "");
    let output = lodi::hostscope::apply(&root.options()).expect("the removal");
    assert!(output.contains("- file /etc/made.conf"), "{output}");
    assert!(!root.exists("etc/made.conf"));
}

// ------------------------------------------------------------- A3: every edit takes effect ---

/// What must hold of the machine once a row is applied, or why it does not.
type Check = Box<dyn Fn(&Case) -> Result<(), String>>;

/// One edit of the table: what it changes, what the next plan must say, and what must hold of
/// the machine once it is applied.
struct Row {
    what: &'static str,
    edit: Box<dyn Fn(&Case)>,
    shows: &'static str,
    holds: Check,
}

fn row(
    what: &'static str,
    edit: impl Fn(&Case) + 'static,
    shows: &'static str,
    holds: impl Fn(&Case) -> Result<(), String> + 'static,
) -> Row {
    Row {
        what,
        edit: Box::new(edit),
        shows,
        holds: Box::new(holds),
    }
}

fn installed(name: &'static str) -> impl Fn(&Case) -> Result<(), String> {
    move |case: &Case| {
        case.installed()
            .contains(name)
            .then_some(())
            .ok_or_else(|| format!("{name} is not installed"))
    }
}

fn gone(name: &'static str) -> impl Fn(&Case) -> Result<(), String> {
    move |case: &Case| {
        (!case.installed().contains(name))
            .then_some(())
            .ok_or_else(|| format!("{name} is still installed"))
    }
}

fn held(case: &Case, name: &str) -> bool {
    case.machine()["installed"][name]["held"] == Value::Bool(true)
}

fn recorded_held(case: &Case, name: &str) -> bool {
    case.lock()["packages"][name]["held"] == Value::Bool(true)
}

fn file_is(content: &'static str, mode: u32) -> impl Fn(&Case) -> Result<(), String> {
    move |case: &Case| {
        let found = case.root.read("etc/edit.conf");
        let found_mode = mode_of(&case.root, "etc/edit.conf");
        (found == content && found_mode == mode)
            .then_some(())
            .ok_or_else(|| format!("/etc/edit.conf is {found:?} mode {found_mode:04o}"))
    }
}

fn edit_file(case: &Case, content: &str, mode: &str, owner: &str) {
    let (_, gid) = ids(&case.root);
    set_table(
        case,
        "[files.\"/etc/edit.conf\"]",
        &format!(
            "content = {content:?}\nmode = \"{mode}\"\nowner = \"{owner}\"\ngroup = \"{gid}\"\n"
        ),
    );
}

/// The rows, in order: each is one edit of the manifest the previous rows left, or — for the
/// hand-set hold — one change made by hand.
fn rows(family: Family) -> Vec<Row> {
    let distro = distro(family);
    let arch = std::env::consts::ARCH;
    let pacman = family == Family::Pacman;
    let hold_line = if pacman {
        "no machine changes; apply would update the record"
    } else {
        "~ package jq (hold)"
    };
    let unhold_line = if pacman {
        "no machine changes; apply would update the record"
    } else {
        "~ package jq (unhold)"
    };
    vec![
        row(
            "add a common name",
            |c| replace_in_manifest(c, "common = [\n", "common = [\n  \"htop\",\n"),
            "+ package htop",
            installed("htop"),
        ),
        row(
            "remove a common name",
            |c| c.delete_line("htop"),
            "- package htop",
            gone("htop"),
        ),
        row(
            "per-distro add",
            move |c| set_table(c, &format!("[packages.{distro}]"), "add = [\"bc\"]\n"),
            "+ package bc",
            installed("bc"),
        ),
        row(
            "per-distro remove",
            move |c| {
                set_table(
                    c,
                    &format!("[packages.{distro}]"),
                    "add = [\"bc\"]\nremove = [\"tree\"]\n",
                );
            },
            "- package tree",
            gone("tree"),
        ),
        row(
            "per-distro table taken out",
            move |c| remove_table(c, &format!("[packages.{distro}]")),
            "- package bc",
            |c| {
                if c.installed().contains("bc") {
                    return Err("bc is still installed".into());
                }
                installed("tree")(c)
            },
        ),
        row(
            "per-arch add",
            move |c| set_table(c, &format!("[packages.{arch}]"), "add = [\"zip\"]\n"),
            "+ package zip",
            installed("zip"),
        ),
        row(
            "per-arch table taken out",
            move |c| remove_table(c, &format!("[packages.{arch}]")),
            "- package zip",
            gone("zip"),
        ),
        row(
            "add hold",
            |c| replace_in_manifest(c, "[packages]\n", "[packages]\nhold = [\"jq\"]\n"),
            hold_line,
            move |c| {
                if !recorded_held(c, "jq") {
                    return Err(format!("the record does not hold jq: {}", c.lock()));
                }
                if !pacman && !held(c, "jq") {
                    return Err("jq is not held".into());
                }
                Ok(())
            },
        ),
        row(
            "remove hold (a hold lodi set is released)",
            |c| replace_in_manifest(c, "hold = [\"jq\"]\n", ""),
            unhold_line,
            |c| {
                if held(c, "jq") || recorded_held(c, "jq") {
                    return Err(format!("jq is still held: {}", c.lock()));
                }
                Ok(())
            },
        ),
        row(
            "a hold set by hand is kept",
            |c| c.edit(|m| m["installed"]["jq"]["held"] = Value::Bool(true)),
            "",
            |c| {
                // A second apply, as the rows' own second apply is: neither may release it.
                let again = c.apply(&[]);
                if !again.status.success() || !held(c, "jq") {
                    return Err(format!("the hand-set hold was released\n{}", story(&again)));
                }
                Ok(())
            },
        ),
        row(
            "add mark_auto",
            |c| {
                c.edit(|m| m["installed"]["jq"]["held"] = Value::Bool(false));
                replace_in_manifest(c, "[packages]\n", "[packages]\nmark_auto = [\"jq\"]\n");
            },
            "~ package jq (auto)",
            |c| {
                (!c.is_explicit("jq"))
                    .then_some(())
                    .ok_or_else(|| "jq is still explicit".to_string())
            },
        ),
        row(
            "remove mark_auto",
            |c| replace_in_manifest(c, "mark_auto = [\"jq\"]\n", ""),
            "~ package jq (manual)",
            |c| {
                c.is_explicit("jq")
                    .then_some(())
                    .ok_or_else(|| "jq is not explicit".to_string())
            },
        ),
        row(
            "optional, available",
            |c| {
                replace_in_manifest(c, "common = [\n", "common = [\n  \"zip\",\n");
                replace_in_manifest(c, "[packages]\n", "[packages]\noptional = [\"zip\"]\n");
            },
            "+ package zip",
            installed("zip"),
        ),
        row(
            "optional, unavailable",
            |c| {
                replace_in_manifest(c, "common = [\n", "common = [\n  \"nosuch\",\n");
                replace_in_manifest(
                    c,
                    "optional = [\"zip\"]\n",
                    "optional = [\"zip\", \"nosuch\"]\n",
                );
            },
            "W_OPTIONAL_SKIPPED: `nosuch` is optional",
            installed("zip"),
        ),
        row(
            "optional names taken out",
            |c| {
                c.delete_line("zip");
                c.delete_line("nosuch");
                replace_in_manifest(c, "optional = [\"zip\", \"nosuch\"]\n", "");
            },
            "- package zip",
            gone("zip"),
        ),
        row(
            "add absent",
            |c| {
                c.install_by_hand(Pkg::new("htop"));
                replace_in_manifest(c, "[packages]\n", "[packages]\nabsent = [\"htop\"]\n");
            },
            "- package htop",
            gone("htop"),
        ),
        row(
            // Nothing is installed that the line named, so the edit has nothing to change.
            "remove absent",
            |c| replace_in_manifest(c, "absent = [\"htop\"]\n", ""),
            "nothing to do",
            gone("htop"),
        ),
        row(
            "a [files] entry added",
            |c| {
                let (uid, _) = ids(&c.root);
                edit_file(c, "one\n", "0644", &uid.to_string());
            },
            "+ file /etc/edit.conf",
            file_is("one\n", 0o644),
        ),
        row(
            "its content changed",
            |c| {
                let (uid, _) = ids(&c.root);
                edit_file(c, "two\n", "0644", &uid.to_string());
            },
            "~ file /etc/edit.conf (content)",
            file_is("two\n", 0o644),
        ),
        row(
            "its mode changed",
            |c| {
                let (uid, _) = ids(&c.root);
                edit_file(c, "two\n", "0600", &uid.to_string());
            },
            "~ file /etc/edit.conf (mode 0644 -> 0600)",
            file_is("two\n", 0o600),
        ),
        row(
            // The owner by name rather than by number: a test that is not root can only name
            // itself, so the machine stays and the record follows the declaration.
            "its owner changed",
            |c| edit_file(c, "two\n", "0600", INVOKER),
            "no machine changes; apply would update the record",
            |c| {
                let owner = c.lock()["files"]["/etc/edit.conf"]["owner"].clone();
                (owner == INVOKER)
                    .then_some(())
                    .ok_or_else(|| format!("the record's owner is {owner}"))
            },
        ),
        row(
            "a [vars] value the file renders changed",
            |c| {
                set_table(c, "[vars]", "greeting = \"hi\"\n");
                edit_file(c, "${vars.greeting}\n", "0600", INVOKER);
                c.apply(&[]);
                set_table(c, "[vars]", "greeting = \"hello\"\n");
            },
            "~ file /etc/edit.conf (content)",
            file_is("hello\n", 0o600),
        ),
        row(
            "the [files] entry removed",
            |c| remove_table(c, "[files.\"/etc/edit.conf\"]"),
            "- file /etc/edit.conf",
            |c| {
                (!c.root.exists("etc/edit.conf"))
                    .then_some(())
                    .ok_or_else(|| "/etc/edit.conf is still there".to_string())
            },
        ),
    ]
}

/// A3: after an exact import, each edit — one at a time — shows in the next plan, converges on
/// apply, and a second apply has nothing to do. Every row is run and every failure is listed.
fn every_edit_takes_effect(family: Family) {
    let case = case(&format!("a3-{family:?}"), family);
    let import = import_for_package_edits(&case);
    assert!(import.status.success(), "{}", story(&import));
    assert!(case.manifest().contains("packages = \"exact\""));
    let first = case.apply(&[]);
    assert!(first.status.success(), "{}", story(&first));

    let mut failures = Vec::new();
    for row in rows(family) {
        (row.edit)(&case);
        let plan = case.plan();
        let said = format!("{}{}", out(&plan), err(&plan));
        if !plan.status.success() || !said.contains(row.shows) {
            failures.push(format!(
                "{}: the plan does not say {:?}\n{}",
                row.what,
                row.shows,
                story(&plan)
            ));
        }
        let apply = case.apply(&[]);
        if !apply.status.success() {
            failures.push(format!("{}: the apply failed\n{}", row.what, story(&apply)));
            continue;
        }
        if let Err(why) = (row.holds)(&case) {
            failures.push(format!("{}: {why}\n{}", row.what, story(&apply)));
        }
        let again = case.apply(&[]);
        if out(&again) != "nothing to do\n" {
            failures.push(format!(
                "{}: a second apply has something to do\n{}",
                row.what,
                story(&again)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{family:?}: {} row(s) failed\n\n{}",
        failures.len(),
        failures.join("\n\n")
    );
}

#[test]
fn a3_every_edit_takes_effect_on_pacman() {
    every_edit_takes_effect(Family::Pacman);
}

#[test]
fn a3_every_edit_takes_effect_on_apt() {
    every_edit_takes_effect(Family::Apt);
}

// ----------------------------------------------------- A4: contradictions are refused ---

fn refused(case: &Case, code: &str, words: &[&str]) {
    let before = case.machine();
    for output in [case.plan(), case.apply(&[])] {
        assert_eq!(output.status.code(), Some(3), "{}", story(&output));
        assert!(err(&output).contains(code), "{code}: {}", story(&output));
        for word in words {
            assert!(err(&output).contains(word), "{word}: {}", story(&output));
        }
    }
    assert_eq!(case.machine(), before, "a refusal moved the machine");
}

fn exact(common: &[&str], extra: &str) -> String {
    let names: Vec<String> = common.iter().map(|n| format!("\"{n}\"")).collect();
    format!(
        "[host]\nversion = \"1\"\npackages = \"exact\"\n\n[packages]\ncommon = [{}]\n{extra}",
        names.join(", ")
    )
}

#[test]
fn a4_a_name_both_declared_and_absent_is_refused() {
    let case = case("a4-both", Family::Apt);
    case.set_manifest(&exact(&["tree", "jq", "app"], "absent = [\"tree\"]\n"));
    refused(&case, "E_ATTR_CONFLICT", &["tree", "absent"]);
    // A per-distro table is part of the declaration for this machine.
    case.set_manifest(&exact(
        &["jq", "app"],
        "absent = [\"tree\"]\n\n[packages.debian]\nadd = [\"tree\"]\n",
    ));
    refused(&case, "E_ATTR_CONFLICT", &["tree", "absent"]);
}

#[test]
fn a4_a_hold_on_an_absent_name_is_refused() {
    let case = case("a4-hold", Family::Apt);
    case.set_manifest(&exact(
        &["jq", "app"],
        "hold = [\"tree\"]\nabsent = [\"tree\"]\n",
    ));
    refused(&case, "E_ATTR_CONFLICT", &["tree", "hold", "absent"]);
}

#[test]
fn a4_a_mark_auto_on_an_undeclared_name_is_refused() {
    let case = case("a4-mark", Family::Pacman);
    case.set_manifest(&exact(&["jq", "app"], "mark_auto = [\"tree\"]\n"));
    refused(&case, "E_UNKNOWN_PACKAGE", &["tree", "mark_auto"]);
}

// --------------------------------------------------------- A5: honest partial failure ---

/// Run the real binary as root of a user namespace of its own, whose ids 1… are the invoking
/// user's subordinate ids (`unshare --map-auto --map-root-user`). There the scratch root's files
/// are root's, and `fchown` to another id really changes a file's owner — with no privilege on
/// this machine, and nothing reachable but the scratch root the command is given. `None` when this
/// machine gives the invoking user no such namespace.
fn in_namespace(argv: &[&str]) -> Option<std::process::Output> {
    let path = format!(
        "{}:{}",
        hostroot::shims().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let probe = std::process::Command::new("unshare")
        .args(["--map-auto", "--map-root-user", "--", "true"])
        .output()
        .ok()?;
    if !probe.status.success() {
        return None;
    }
    Some(
        std::process::Command::new("unshare")
            .args(["--map-auto", "--map-root-user", "--"])
            .args(argv)
            .env("PATH", path)
            .stdin(std::process::Stdio::null())
            .output()
            .expect("unshare runs"),
    )
}

/// One host verb against the scratch root, as root of the namespace.
fn host_in_namespace(root: &Root, verb: &str) -> std::process::Output {
    let dir = root.dir.display().to_string();
    // check-host-safety: refusal — one place, and it passes this scratch root as --root.
    let argv = [
        env!("CARGO_BIN_EXE_lodi"),
        "host",
        verb,
        "--root",
        dir.as_str(),
    ];
    in_namespace(&argv).expect("the namespace was there a moment ago")
}

/// The owner and group of a path, as the namespace sees them.
fn owner_in_namespace(root: &Root, rel: &str) -> String {
    let path = root.path(rel).display().to_string();
    let output = in_namespace(&["stat", "-c", "%u:%g", path.as_str()]).expect("a namespace");
    assert!(output.status.success(), "{output:?}");
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A3's owner row with a real `chown`: a file owned by one user is declared another's, the apply
/// changes its owner and keeps the original first, a second apply has nothing to do, and taking
/// the entry out puts back the original owner, mode and bytes (LD-377 validator repair).
#[test]
fn a3_a_real_owner_change_is_made_and_undone() {
    let root = root_with_original("a3-real-owner");
    root.arm().debian();
    let path = root.path("etc/pre.conf").display().to_string();
    let Some(chown) = in_namespace(&["chown", "2:2", path.as_str()]) else {
        eprintln!(
            "a3_a_real_owner_change_is_made_and_undone: SKIPPED — this machine gives the \
             invoking user no user namespace with subordinate ids"
        );
        return;
    };
    assert!(chown.status.success(), "{chown:?}");
    assert_eq!(owner_in_namespace(&root, "etc/pre.conf"), "2:2");
    root.write(
        "etc/lodi/host.toml",
        "[files.\"/etc/pre.conf\"]\ncontent = \"original\\n\"\nmode = \"0640\"\nowner = \"1\"\n\
         group = \"1\"\n",
    );

    let plan = host_in_namespace(&root, "plan");
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        out(&plan).contains("~ file /etc/pre.conf (owner 1:1)"),
        "{}",
        story(&plan)
    );
    let apply = host_in_namespace(&root, "apply");
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(
        owner_in_namespace(&root, "etc/pre.conf"),
        "1:1",
        "{}",
        story(&apply)
    );
    let kept = backups(&root);
    assert_eq!(
        kept.len(),
        1,
        "the original is kept first\n{}",
        story(&apply)
    );
    let kept_rel = kept[0]
        .strip_prefix(&root.dir)
        .expect("below the root")
        .display()
        .to_string();
    assert_eq!(
        owner_in_namespace(&root, &kept_rel),
        "2:2",
        "the copy keeps its owner"
    );
    let again = host_in_namespace(&root, "apply");
    assert_eq!(out(&again), "nothing to do\n", "{}", story(&again));

    root.write("etc/lodi/host.toml", "");
    let removal = host_in_namespace(&root, "apply");
    assert!(removal.status.success(), "{}", story(&removal));
    assert_eq!(
        owner_in_namespace(&root, "etc/pre.conf"),
        "2:2",
        "the original owner is back\n{}",
        story(&removal)
    );
    // Owned by another user at 0640, the file is readable only inside the namespace.
    let read = in_namespace(&["cat", path.as_str()]).expect("a namespace");
    assert_eq!(
        String::from_utf8_lossy(&read.stdout),
        "original\n",
        "{read:?}"
    );
    assert_eq!(mode_of(&root, "etc/pre.conf"), 0o640);
    assert!(backups(&root).is_empty(), "the consumed copy is removed");
}

/// A package manager that changes the machine and then fails, part way through its own
/// transaction: the apply exits `E_APPLY` saying exactly what that action changed and what it did
/// not, never that it did not happen; the record keeps its last true bytes; and the next plan is
/// made from the machine as it is and converges (LD-377 validator repair).
fn a_part_way_failure_says_what_changed(family: Family) {
    let case = case(&format!("a5-part-{family:?}"), family);
    assert!(import_for_package_edits(&case).status.success());
    let lock_before = case.root.read("etc/lodi/host.lock");
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"bc\",\n  \"zip\",\n");
    let fault = match family {
        Family::Pacman => "pacman -Syu",
        Family::Apt => "apt-get install",
    };
    case.edit(|m| m["fail_part"] = serde_json::json!({ fault: "error: the fake stops part way" }));

    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    let said = err(&apply);
    assert!(said.contains("E_APPLY"), "{}", story(&apply));
    assert!(case.installed().contains("bc") && !case.installed().contains("zip"));
    assert!(
        !said.contains("this one did not happen") && !said.contains("did not run: p1"),
        "the error says the transaction did not happen, and it installed bc\n{}",
        story(&apply)
    );
    assert!(
        said.contains("changed before it failed: + package bc"),
        "the error does not say what the failed action changed\n{}",
        story(&apply)
    );
    assert!(
        said.contains("not changed: + package zip"),
        "the error does not say what the failed action left undone\n{}",
        story(&apply)
    );
    assert_eq!(
        case.root.read("etc/lodi/host.lock"),
        lock_before,
        "the record moved"
    );

    case.edit(|m| {
        m.as_object_mut().unwrap().remove("fail_part");
    });
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        !out(&plan).contains("+ package bc") && out(&plan).contains("+ package zip"),
        "the plan was not made from the machine as it is\n{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        case.recorded().contains("bc") && case.recorded().contains("zip"),
        "{}",
        case.lock()
    );
    assert_eq!(out(&case.apply(&[])), "nothing to do\n");
}

#[test]
fn a5_a_part_way_failure_says_what_changed_on_pacman() {
    a_part_way_failure_says_what_changed(Family::Pacman);
}

#[test]
fn a5_a_part_way_failure_says_what_changed_on_apt() {
    a_part_way_failure_says_what_changed(Family::Apt);
}

/// A package manager that refuses part way through an apply: the apply exits `E_APPLY` naming
/// what ran and what did not, the record keeps its last true bytes, and — the cause fixed — the
/// next plan is made from the machine as it is and converges.
fn a_partial_failure_is_honest(family: Family) {
    let case = case(&format!("a5-{family:?}"), family);
    assert!(import_for_package_edits(&case).status.success());
    let lock_before = case.root.read("etc/lodi/host.lock");
    replace_in_manifest(&case, "common = [\n", "common = [\n  \"bc\",\n");
    // pacman removes in a second action; apt marks after its one transaction.
    let (fault, ran, not_run) = match family {
        Family::Pacman => {
            case.delete_line("tree");
            ("pacman -Rs", "+ package bc", "- package tree")
        }
        Family::Apt => {
            replace_in_manifest(&case, "[packages]\n", "[packages]\nmark_auto = [\"jq\"]\n");
            ("apt-mark auto", "+ package bc", "~ package jq (auto)")
        }
    };
    case.edit(|m| m["fail"] = serde_json::json!({ fault: "error: the fake refuses" }));

    let apply = case.apply(&[]);
    assert_eq!(apply.status.code(), Some(8), "{}", story(&apply));
    assert!(err(&apply).contains("E_APPLY"), "{}", story(&apply));
    assert!(
        err(&apply).contains(&format!("ran: p1 ({ran})")),
        "the error does not name what ran\n{}",
        story(&apply)
    );
    // The failed action changed nothing here, and the error says so rather than that it did
    // not run (LD-377 validator repair).
    assert!(
        err(&apply).contains(&format!("not changed: {not_run}")),
        "the error does not name what did not run\n{}",
        story(&apply)
    );
    assert!(case.installed().contains("bc"), "{}", story(&apply));
    assert_eq!(
        case.root.read("etc/lodi/host.lock"),
        lock_before,
        "the record moved"
    );

    case.edit(|m| {
        m.as_object_mut().unwrap().remove("fail");
    });
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        !out(&plan).contains("+ package bc"),
        "the plan was not made from the machine as it is\n{}",
        story(&plan)
    );
    assert!(out(&plan).contains(not_run), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(case.recorded().contains("bc"), "{}", case.lock());
    assert_eq!(out(&case.apply(&[])), "nothing to do\n");
}

#[test]
fn a5_a_partial_failure_is_named_and_reconciled_on_pacman() {
    a_partial_failure_is_honest(Family::Pacman);
}

#[test]
fn a5_a_partial_failure_is_named_and_reconciled_on_apt() {
    a_partial_failure_is_honest(Family::Apt);
}

/// #427: a scratch base reached through a symbolic link, as a `target/tmp` on a tmpfs is. The gate
/// canonicalizes `--root` and names every path below that spelling, so a test's root must be the
/// same path, or each path a command reports differs from the one the test expects.
#[test]
fn a_scratch_root_below_a_symbolic_link_is_the_root_the_gate_opens() {
    hostroot::shims();
    let real = support::scratch("host-link-target");
    let link = support::scratch("host-link").join("tmp");
    std::os::unix::fs::symlink(&real, &link).expect("a symbolic link");
    let root = Root {
        dir: support::scratch_in(&link, "host-linked"),
    };
    root.debian().arm();
    let gate = Gate::open(&root.options(), Operation::Plan).expect("the scratch root is armed");
    assert_eq!(gate.root, root.dir, "the gate opened another spelling");
}
