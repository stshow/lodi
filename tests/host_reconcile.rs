//! LD-378 (W2): a host import over an existing manifest **reconciles** the machine into it.
//!
//! Ours is the manifest, theirs is the machine now; on the 2.0 `lodi import` the base is the host
//! record's baseline of the last sync of this folder (LD-519). A change made outside Lodi is
//! adopted, the person's own edits are kept, and a conflict keeps the manifest's side with
//! `W_RECONCILE`. The 1.x baseline rows (`host.lock` schema 2, a version-1 lock, no record,
//! `--force`) went with the 1.x import (#699).
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it. Nothing reaches `/` and no real package manager runs
//! (`AGENTS.md` §8): every host command here carries `--root`.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use fakehost::{Case, Family, Machine, Pkg, err, nothing, out, story};

/// A machine a person chose `tree`, `jq` and `bat` on, with `zip` offered and not installed.
fn machine_for(family: Family) -> Machine {
    let machine = match family {
        Family::Pacman => Machine::arch(),
        Family::Apt => Machine::debian(),
    };
    machine
        .with(Pkg::new("tree"))
        .with(Pkg::new("jq"))
        .with(Pkg::new("bat"))
        .offering(Pkg::new("zip"))
}

/// The `common` array's elements, in the order the file has them.
fn common(manifest: &str) -> Vec<String> {
    let start = manifest.find("common = [").expect("a common array");
    let body = &manifest[start + "common = [".len()..];
    let end = body.find(']').expect("the array closes");
    body[..end]
        .lines()
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

/// Swap two adjacent element lines of the manifest, as a person reordering the list by hand.
fn swap_lines(case: &Case, first: &str, second: &str) {
    let text = case.manifest();
    let a = format!("  \"{first}\",\n");
    let b = format!("  \"{second}\",\n");
    let pair = format!("{a}{b}");
    assert!(text.contains(&pair), "no {first} then {second}:\n{text}");
    case.set_manifest(&text.replacen(&pair, &format!("{b}{a}"), 1));
}

/// Remove a package from the fake machine by hand, as `pacman -R` or `apt-get remove` would.
fn remove_by_hand(case: &Case, name: &str) {
    case.edit(|m| {
        m["installed"]
            .as_object_mut()
            .expect("an installed set")
            .remove(name);
    });
}

// ------------------------------------------------------------- R1: the owner's sequence ---

/// The owner's W2 sequence: import; install one package and remove another outside Lodi; edit
/// the manifest by hand (a comment, a reordered line, one line deleted); import again. The
/// manifest gains the one, loses the other, keeps the comment, the order and the deletion, and
/// the plan has nothing to do for anything the machine has — only the deleted line's removal.
fn reimport_reconciles(family: Family) {
    let case = Case::new(&format!("r1-{family:?}"), machine_for(family));
    let first = case.import();
    assert!(first.status.success(), "{}", story(&first));
    let kernel = match family {
        Family::Pacman => "linux",
        Family::Apt => "linux-image-amd64",
    };
    let mut expected: Vec<&str> = match family {
        Family::Pacman => vec!["base", "bat", "jq", "linux", "tree"],
        Family::Apt => vec!["bat", "jq", "linux-image-amd64", "tree"],
    };
    assert_eq!(common(&case.manifest()), expected);

    // Outside Lodi: zip installed from the distribution, tree removed.
    case.install_by_hand(Pkg::new("zip"));
    remove_by_hand(&case, "tree");
    // By hand: a comment, the kernel before jq, and bat's line deleted.
    swap_lines(&case, "jq", kernel);
    let text = case
        .manifest()
        .replacen("common = [\n", "# my own note\ncommon = [\n", 1);
    case.set_manifest(&text);
    case.delete_line("bat");

    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    let said = out(&again);
    assert!(said.contains("merged into"), "{}", story(&again));
    assert!(
        said.contains("1 added, 1 removed") && said.contains("0 conflict"),
        "{}",
        story(&again)
    );
    let manifest = case.manifest();
    // Gains zip, loses tree, keeps the order a person gave it and bat's deletion. The list is
    // no longer sorted, so the adopted name goes at the end.
    expected.retain(|name| !["bat", "tree"].contains(name));
    let jq = expected.iter().position(|name| *name == "jq").unwrap();
    expected.swap(jq, jq + 1);
    expected.push("zip");
    assert_eq!(common(&manifest), expected, "{manifest}");
    assert!(manifest.contains("# my own note\ncommon = ["), "{manifest}");
    assert!(!manifest.contains("\"bat\""), "{manifest}");

    // The plan has nothing to do for anything the machine has; bat's deletion is the one
    // change, and the apply makes it.
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let planned = err(&plan);
    assert!(planned.contains("bat"), "{planned}");
    for name in ["zip", "jq", "tree"] {
        assert!(
            !planned.lines().any(|line| line.contains(name)),
            "the plan touches {name}:\n{planned}"
        );
    }
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(!case.installed().contains("bat"));
    let quiet = case.plan();
    assert!(nothing(&quiet), "{}", story(&quiet));

    // A second reconcile of the converged machine changes nothing: identical bytes.
    let before = case.manifest();
    let noop = case.import();
    assert!(noop.status.success(), "{}", story(&noop));
    assert_eq!(case.manifest(), before);
}

#[test]
fn r1_reimport_reconciles_on_pacman() {
    reimport_reconciles(Family::Pacman);
}

#[test]
fn r1_reimport_reconciles_on_apt() {
    reimport_reconciles(Family::Apt);
}

// ---------------------------------------------------------------------- helpers, part two ---

/// The name the scratch roots give the invoking user, so that a captured file's owner resolves.
const INVOKER: &str = "lodi-reconcile";

/// A case whose root names the invoking user and carries `app`'s changed configuration file.
fn case_with_conffile(name: &str, family: Family) -> Case {
    let machine = machine_for(family).with(Pkg::new("app").conffile("/etc/app.conf"));
    let case = Case::new(name, machine);
    let (uid, gid) = hostroot::ids(&case.root);
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

/// Declare `app`'s file under `[files]` the way an import before 1.10 wrote it — a `source` in
/// the bundle, the mode, owner and group it was found with — and apply it, so that the record
/// names it. Since si-1 the import copies no file, so a test of a declared file sets it up here.
fn declare_as_an_earlier_import(case: &Case) {
    case.set_manifest(&format!(
        "{}\n[files.\"/etc/app.conf\"]\nsource = \"files/etc/app.conf\"\nmode = \"0644\"\n\
         owner = \"{INVOKER}\"\ngroup = \"{INVOKER}\"\n",
        case.manifest()
    ));
    case.write_beside("files/etc/app.conf", "mine\n");
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
}

/// A file beside the manifest, as text.
fn beside(case: &Case, rel: &str) -> String {
    std::fs::read_to_string(case.beside(rel)).expect("a file beside the manifest")
}

/// Every path below a directory with its kind, mode, size and mtime: what "writes nothing" is
/// checked against.
fn census(dir: &std::path::Path) -> std::collections::BTreeMap<String, String> {
    use std::os::unix::fs::MetadataExt;
    fn visit(
        base: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, String>,
    ) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = path.symlink_metadata() else {
                continue;
            };
            let rel = path
                .strip_prefix(base)
                .unwrap_or(&path)
                .display()
                .to_string();
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
                visit(base, &path, out);
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    visit(dir, dir, &mut out);
    out
}

// --------------------------------------------- R3: the rules, end to end where they bite ---

/// A declared configuration file changed only on the machine is not captured again (si-1): the
/// reconcile says so, prints no content, and the declaration and its bytes stay as they were.
#[test]
fn r3_a_file_changed_only_on_the_machine_is_not_captured_again() {
    let case = case_with_conffile("r3-file", Family::Apt);
    assert!(case.import().status.success());
    declare_as_an_earlier_import(&case);
    case.root
        .write("etc/app.conf", "mine, changed\n")
        .chmod("etc/app.conf", 0o664);
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        fakehost::err(&again).contains(
            "W_RECONCILE: /etc/app.conf changed on this machine and is not captured again"
        ),
        "{}",
        story(&again)
    );
    let said = out(&again);
    assert!(!said.contains("captured"), "{said}");
    assert!(
        !said.contains("mine, changed"),
        "the report printed file contents:\n{said}"
    );
    assert_eq!(beside(&case, "files/etc/app.conf"), "mine\n");
    assert!(
        case.manifest().contains("mode = \"0644\""),
        "{}",
        case.manifest()
    );
}

/// A file edited only in the manifest's `files/` keeps its edit; one changed on both sides is a
/// conflict that keeps the manifest's side, says `W_RECONCILE` and exits 0.
#[test]
fn r3_a_file_edited_in_the_manifest_is_kept_and_a_conflict_keeps_the_manifest() {
    let case = case_with_conffile("r3-conflict", Family::Apt);
    assert!(case.import().status.success());
    declare_as_an_earlier_import(&case);
    case.write_beside("files/etc/app.conf", "edited by hand\n");
    let kept = case.import();
    assert!(kept.status.success(), "{}", story(&kept));
    assert_eq!(beside(&case, "files/etc/app.conf"), "edited by hand\n");
    assert!(out(&kept).contains("1 kept"), "{}", story(&kept));

    case.root.write("etc/app.conf", "changed on the machine\n");
    let conflict = case.import();
    assert_eq!(conflict.status.code(), Some(0), "{}", story(&conflict));
    assert!(
        fakehost::err(&conflict)
            .contains("W_RECONCILE: /etc/app.conf changed on this machine and in host.toml"),
        "{}",
        story(&conflict)
    );
    assert!(
        out(&conflict).contains("1 conflict(s)"),
        "{}",
        story(&conflict)
    );
    assert_eq!(beside(&case, "files/etc/app.conf"), "edited by hand\n");
}

/// A declared file whose bytes stay fixed while its mode, owner and group change on the machine
/// is not captured again (si-1): the reconcile says so and the manifest keeps its metadata. The
/// owner and group change the deterministic way a scratch root allows: the root's own `passwd`
/// and `group` now give the file's ids to another user and group, which is what a `chown` by
/// root would have made of them. A manifest-side metadata edit is then kept, one changed on both
/// sides differently is a conflict that keeps the manifest, and the lock stays schema 2 with the
/// same three baseline members (validator-repair-1, R3).
#[test]
fn r3_a_mode_owner_or_group_changed_only_on_the_machine_is_not_captured_again() {
    let case = case_with_conffile("r3-meta", Family::Apt);
    assert!(case.import().status.success());
    declare_as_an_earlier_import(&case);
    let (uid, gid) = hostroot::ids(&case.root);
    let declared = case.manifest();
    assert!(
        declared.contains(&format!("owner = \"{INVOKER}\""))
            && declared.contains("mode = \"0644\""),
        "{declared}"
    );

    // The machine alone: the same bytes, another mode, and the file's ids now another user's
    // and another group's.
    case.root
        .write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\n\
                 {INVOKER}:x:{}:{}::/nonexistent:/bin/sh\n\
                 lodi-other:x:{uid}:{gid}::/nonexistent:/bin/sh\n",
                uid + 1,
                gid + 1
            ),
        )
        .write(
            "etc/group",
            &format!("root:x:0:\n{INVOKER}:x:{}:\nlodi-staff:x:{gid}:\n", gid + 1),
        )
        .chmod("etc/app.conf", 0o444);
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        fakehost::err(&again).contains(
            "W_RECONCILE: /etc/app.conf changed on this machine and is not captured again"
        ),
        "{}",
        story(&again)
    );
    let manifest = case.manifest();
    for key in [
        "mode = \"0644\"".to_string(),
        format!("owner = \"{INVOKER}\""),
        format!("group = \"{INVOKER}\""),
    ] {
        assert!(manifest.contains(&key), "no {key}:\n{manifest}");
    }
    assert_eq!(beside(&case, "files/etc/app.conf"), "mine\n");

    // The schema-2 baseline is unchanged: the same members, and a file's baseline is still its
    // digest alone. The record's version follows what else it holds (the `[system]` basics an
    // import records make it 5), so only its floor is checked.
    let lock = case.lock();
    assert!(lock["version"].as_u64().is_some_and(|v| v >= 2), "{lock}");
    let baseline = lock["baseline"].as_object().expect("a baseline");
    let mut members: Vec<&str> = baseline.keys().map(String::as_str).collect();
    members.sort_unstable();
    assert_eq!(members, ["explicit", "files", "holds"]);
    assert!(
        baseline["files"]["/etc/app.conf"]
            .as_str()
            .is_some_and(|digest| digest.starts_with("sha256:")),
        "{lock}"
    );

    // The machine back as recorded; then a metadata edit made only in the manifest is kept.
    case.root
        .write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\n{INVOKER}:x:{uid}:{gid}::/nonexistent:/bin/sh\n"
            ),
        )
        .write("etc/group", &format!("root:x:0:\n{INVOKER}:x:{gid}:\n"))
        .chmod("etc/app.conf", 0o644);
    case.set_manifest(&manifest.replace("mode = \"0644\"", "mode = \"0440\""));
    let kept = case.import();
    assert!(kept.status.success(), "{}", story(&kept));
    assert!(out(&kept).contains("1 kept"), "{}", story(&kept));
    assert!(
        case.manifest().contains("mode = \"0440\""),
        "{}",
        case.manifest()
    );

    // Changed on both sides, differently: a conflict, and the manifest's side stays.
    case.root.chmod("etc/app.conf", 0o404);
    let conflict = case.import();
    assert_eq!(conflict.status.code(), Some(0), "{}", story(&conflict));
    assert!(
        fakehost::err(&conflict).contains(
            "W_RECONCILE: /etc/app.conf's mode, owner or group changed on this machine and in \
             host.toml, differently; host.toml is kept"
        ),
        "{}",
        story(&conflict)
    );
    assert!(
        out(&conflict).contains("1 conflict(s)"),
        "{}",
        story(&conflict)
    );
    assert!(
        case.manifest().contains("mode = \"0440\""),
        "{}",
        case.manifest()
    );
}

/// A name the manifest declares absent that was installed by hand is a conflict: the manifest
/// keeps it absent, `W_RECONCILE` names it, and the exit status is 0.
#[test]
fn r3_an_absent_name_installed_by_hand_is_a_conflict() {
    let case = Case::new("r3-absent", machine_for(Family::Pacman));
    assert!(case.import().status.success());
    let text = case
        .manifest()
        .replacen("[packages]\n", "[packages]\nabsent = [\"zip\"]\n", 1);
    case.set_manifest(&text);
    case.install_by_hand(Pkg::new("zip"));
    let again = case.import();
    assert_eq!(again.status.code(), Some(0), "{}", story(&again));
    assert!(
        fakehost::err(&again).contains("W_RECONCILE: package zip was installed by hand"),
        "{}",
        story(&again)
    );
    assert!(!common(&case.manifest()).contains(&"zip".to_string()));
    assert!(case.manifest().contains("absent = [\"zip\"]"));
}

/// An apt hold set outside Lodi is adopted into `hold`.
#[test]
fn r3_a_hold_set_outside_lodi_is_adopted() {
    let case = Case::new("r3-hold", machine_for(Family::Apt));
    assert!(case.import().status.success());
    case.edit(|m| {
        m["installed"]["jq"]["held"] = serde_json::json!(true);
    });
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        case.manifest().contains("hold = [\n  \"jq\",\n]"),
        "{}",
        case.manifest()
    );
    assert!(nothing(&case.plan()));
}

/// A package from a third party installed outside Lodi is a line of the regenerated `NOT
/// CAPTURED` block; once the person deletes the block, it is never added back.
#[test]
fn r3_a_third_party_install_is_not_captured_and_a_deleted_block_stays_deleted() {
    let case = Case::new("r3-vendor", machine_for(Family::Apt));
    assert!(case.import().status.success());
    case.install_by_hand(Pkg::new("vendor-tool").repo(Some("vendor")));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    let manifest = case.manifest();
    assert!(!common(&manifest).contains(&"vendor-tool".to_string()));
    let block = &manifest[manifest.find("# NOT CAPTURED").expect("the block")..];
    assert!(block.contains("vendor-tool"), "{manifest}");

    let cut = manifest[..manifest.find("# NOT CAPTURED").unwrap()].to_string();
    case.set_manifest(&cut);
    case.install_by_hand(Pkg::new("other-tool").repo(Some("vendor")));
    let third = case.import();
    assert!(third.status.success(), "{}", story(&third));
    assert!(
        !case.manifest().contains("# NOT CAPTURED"),
        "{}",
        case.manifest()
    );
}

// --------------------------------------------------------------------------- R4: --dry-run ---

/// `--dry-run` previews the merge on standard error, never a changed file's contents — no file
/// is captured (si-1) — and writes nothing, the lock and the run's log included.
#[test]
fn r4_dry_run_prints_the_diff_and_writes_nothing() {
    let case = case_with_conffile("r4-dry", Family::Apt);
    assert!(case.import().status.success());
    case.install_by_hand(Pkg::new("zip"));
    case.root.write("etc/app.conf", "changed quietly\n");
    let before = census(&case.root.dir);
    let dry = case.verb("import", &["--dry-run"]);
    assert!(dry.status.success(), "{}", story(&dry));
    let said = err(&dry);
    let host = case.config().join("host.toml").display().to_string();
    assert!(
        said.contains(&format!("would change {host}: 1 added, 0 removed")),
        "{said}"
    );
    assert!(said.contains("+ package zip"), "{said}");
    assert!(!said.contains("would write"), "{said}");
    assert!(!said.contains("changed quietly"), "{said}");
    assert!(said.contains("nothing was written (--dry-run)"), "{said}");
    assert!(
        !out(&dry).contains("zip"),
        "the preview is on standard error"
    );
    assert_eq!(before, census(&case.root.dir), "--dry-run wrote something");
}

// ------------------------------------------------------ R5: a no-op writes nothing at all ---

/// A reconcile of a machine that has not changed writes nothing to the config: identical
/// bytes, and the manifest's and the lock's mtimes unchanged.
#[test]
fn r5_a_no_op_reconcile_writes_nothing() {
    let case = case_with_conffile("r5-noop", Family::Pacman);
    assert!(case.import().status.success());
    // The first import may leave the lock unfilled (the offline archive has no dated Arch
    // repository for today); importing again fills it. The no-op is the import after that, run
    // as is: `Case::import` would rewrite the file itself.
    assert!(case.verb("import", &[]).status.success());
    let before = census(&case.config());
    let manifest = case.manifest();
    let again = case.verb("import", &[]);
    assert!(again.status.success(), "{}", story(&again));
    let host = case.config().join("host.toml").display().to_string();
    assert!(
        out(&again).contains(&format!("{host} is unchanged: nothing to merge")),
        "{}",
        story(&again)
    );
    assert_eq!(case.manifest(), manifest);
    assert_eq!(
        before,
        census(&case.config()),
        "a no-op reconcile wrote something"
    );
}

// ------------------------------ R7: an archive-dated index, found on the Ubuntu guest ---

/// Set the mtime of every file of the fake apt's index `hours` back, leaving its change time at
/// now. apt stamps each list file it fetches with the archive's own `Last-Modified`, so this is
/// what `apt-get update` leaves behind when the archive last published that long ago.
fn age_the_index(case: &Case, hours: u64) {
    let when = std::time::SystemTime::now() - std::time::Duration::from_secs(hours * 3600);
    let lists = case.root.path("var/lib/apt/lists");
    for entry in std::fs::read_dir(&lists).expect("the lists directory") {
        let path = entry.expect("an entry").path();
        if path.is_file() {
            std::fs::File::options()
                .write(true)
                .open(&path)
                .expect("an index file")
                .set_modified(when)
                .expect("an older index file");
        }
    }
}

/// The W2 row's failure on a real Ubuntu 24.04 guest (LD-378). The row ran `apt-get update`
/// minutes before the reconcile, but Ubuntu's archive had published nothing for almost seven
/// hours, so every list file's mtime was seven hours back. The plan after the reconcile then
/// said `~ index (apt-get update)` and `1 action(s)` on a machine that has everything its
/// manifest names, and would have said it after every refresh. An index counts from when apt
/// last touched it: the plan has nothing to do and the apply runs no `apt-get update`.
#[test]
fn r7_after_a_reconcile_an_archive_dated_index_is_nothing_to_do_on_apt() {
    let case = Case::new("r7-old-index", machine_for(Family::Apt));
    let first = case.import();
    assert!(first.status.success(), "{}", story(&first));
    case.install_by_hand(Pkg::new("zip"));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    assert!(
        out(&again).contains("1 added, 0 removed"),
        "{}",
        story(&again)
    );

    age_the_index(&case, 9);
    let plan = case.plan();
    assert!(nothing(&plan), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert!(nothing(&apply), "{}", story(&apply));
    assert!(
        !case
            .log()
            .iter()
            .any(|argv| argv.contains("apt-get update")),
        "{:?}",
        case.log()
    );
}
