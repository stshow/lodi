//! The intent journal and the restart classifier, over **real interrupted applies** (M-0.6 T-1).
//!
//! Every interruption here is a real one: the test re-runs this very test binary as a child
//! process, holds that process still at a real point inside a real action, and kills it with
//! `SIGKILL`. The product has no idea it is under a test — there is no test-only branch, no
//! environment variable and no hidden flag in any host path. What holds the child still is a
//! [`hostroot::Hold`]: the child's own thread stopped by the kernel at the open of the temporary
//! file one write goes through (LD-379, which retired the named pipe these tests once used as a
//! `[files]` `source`: a source is now read once, into memory, and only as a regular file).
//!
//! Every root is a scratch directory this test owns. Nothing here runs against `/`.

#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;

use lodi::hostscope::journal::{self, Ambiguity, Classification, Event};

use hostroot::{Hold, Root, backups, ids, journals, kill, wait_until};

/// The child half of every interruption below: one real apply, in its own process, over the
/// root the parent names. It does nothing at all in the parent's own run.
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

fn read_only_journal(root: &Root) -> journal::Record {
    let files = journals(root);
    assert_eq!(files.len(), 1, "exactly one journal: {files:?}");
    journal::read(&files[0]).expect("the journal reads back")
}

// ------------------------------------------------------------------- acceptance (c), and (d)/1

/// The journal is on disk, complete and readable by another process, **before** the first
/// managed path is touched. The only writes that precede it are the host scope's own state
/// directory and its apply lock, which are the scope's bookkeeping and not the machine's state.
#[test]
fn the_journal_is_on_disk_before_the_first_mutation_and_an_unstarted_action_is_incomplete() {
    let root = Root::new("journal-first");
    let (uid, gid) = ids(&root);
    root.debian_with(&format!(
        "[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));

    // Held at the open of the temporary file its one write goes through: the journal is written
    // and says the action has begun, and nothing of the file is there yet.
    let hold = Hold::at_write_of(&root, "a.conf");
    let mut child = hold.spawn(&root);
    hold.wait_held();
    wait_until("the apply to announce its first action", || {
        journals(&root)
            .first()
            .and_then(|path| journal::read(path).ok())
            .is_some_and(|record| record.interrupted().is_some())
    });
    kill(&mut child);

    assert!(
        !root.exists("etc/a.conf"),
        "the journal was on disk before the managed path was touched"
    );
    let record = read_only_journal(&root);
    assert_eq!(record.header.version, 1);
    assert_eq!(record.header.root, root.dir.display().to_string());
    assert_eq!(record.header.euid, lodi::hostscope::safety::current_euid());
    assert!(
        record
            .header
            .programs
            .iter()
            .any(|(name, path)| name == "apt-get" && path.ends_with("apt-get")),
        "the journal records the process seam it resolved: {:?}",
        record.header.programs
    );
    assert_eq!(record.header.actions.len(), 1);
    assert_eq!(record.header.actions[0].kind, "file.create");
    assert!(!record.committed());
    assert!(matches!(record.events[0], Event::Begin { .. }));

    // The action's `pre` state is what the machine still shows, so a restart may simply plan
    // again: **incomplete**.
    assert_eq!(
        record.classify(&root.dir),
        Classification::Incomplete {
            action: "f1".into(),
            summary: "+ file /etc/a.conf (mode 0644, owner ".to_string() + &format!("{uid}:{gid})"),
        }
    );
}

// --------------------------------------------------- acceptance (d)/2 and (d)/3, and (e)

/// An apply killed between keeping the old bytes and putting the new ones in place leaves a
/// machine that matches neither state. Lodi says so, stops, and offers nothing it cannot do.
#[test]
fn an_interrupted_replace_is_ambiguous_and_stops_the_next_apply() {
    let root = Root::new("journal-ambiguous");
    let (uid, gid) = ids(&root);
    root.write("etc/b.conf", "old\n").debian_with(&format!(
        "[files.\"/etc/a.conf\"]\ncontent = \"alpha\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n\n\
         [files.\"/etc/b.conf\"]\ncontent = \"beta\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
    ));

    // Held at the second action's write of the new bytes, which comes after it has kept the old
    // ones and before it has put the new ones in place: the machine is then neither.
    let hold = Hold::at_write_of(&root, "b.conf");
    let mut child = hold.spawn(&root);
    hold.wait_held();
    assert!(
        !backups(&root).is_empty(),
        "the replacing action kept the old bytes before its write"
    );
    kill(&mut child);

    // The first action finished; the second kept the old bytes and never put the new ones in.
    assert_eq!(root.read("etc/a.conf"), "alpha\n");
    assert_eq!(root.read("etc/b.conf"), "old\n");
    let kept = backups(&root);
    assert_eq!(kept.len(), 1, "{kept:?}");
    assert_eq!(fs::read_to_string(&kept[0]).unwrap(), "old\n");

    let record = read_only_journal(&root);
    let ambiguous = record.classify(&root.dir);
    match &ambiguous {
        Classification::Ambiguous { action, detail, .. } => {
            assert_eq!(action, "f2");
            // The file half names a subject: a path the operator can put into a state.
            assert_eq!(detail, &Ambiguity::Subject("/etc/b.conf".to_string()));
        }
        other => panic!("expected ambiguous, got {other:?}"),
    }

    // The same real journal, classified again after the operator has put the machine back the
    // way it was: **incomplete**, and a restart may simply plan again.
    let backup = kept[0].clone();
    fs::remove_file(&backup).unwrap();
    assert!(matches!(
        record.classify(&root.dir),
        Classification::Incomplete { .. }
    ));

    // And once more after the operator has instead finished the action by hand: **completed**,
    // because the machine shows exactly the state the journal said the action would leave.
    fs::copy(root.path("etc/b.conf"), &backup).unwrap();
    fs::write(root.path("etc/b.conf"), "beta\n").unwrap();
    assert_eq!(record.classify(&root.dir), Classification::Completed);

    // Put the ambiguity back, and prove the apply stops on it.
    fs::write(root.path("etc/b.conf"), "old\n").unwrap();
    assert!(matches!(
        record.classify(&root.dir),
        Classification::Ambiguous { .. }
    ));

    let error = lodi::hostscope::apply(&root.options()).expect_err("the ambiguity stops it");
    assert_eq!(error.codes(), vec!["E_JOURNAL_AMBIGUOUS"]);
    assert_eq!(error.exit_status(), 8);
    let text = error.to_string();
    assert!(
        text.contains(&record.id),
        "the stop names the journal: {text}"
    );
    assert!(text.contains("/etc/b.conf"), "{text}");
    for word in ["rollback", "undo", "generation", "revert"] {
        assert!(
            !text.to_lowercase().contains(word),
            "the stop offers {word}: {text}"
        );
    }

    // A declaration that names the wrong journal is refused, and says which one is waiting.
    let mut wrong = root.options();
    wrong.resolved = Some("20000101T000000Z-0000".to_string());
    let refused = lodi::hostscope::apply(&wrong).expect_err("a wrong id is refused");
    assert_eq!(refused.codes(), vec!["E_JOURNAL_AMBIGUOUS"]);
    assert!(refused.to_string().contains(&record.id), "{refused}");
    assert_eq!(
        root.read("etc/b.conf"),
        "old\n",
        "a refused declaration changes nothing"
    );

    // The right one proceeds, and the declaration is recorded in the **new** journal, so that
    // the record keeps the fact that a human, not lodi, decided the outcome.
    let mut right = root.options();
    right.resolved = Some(record.id.clone());
    let report = lodi::hostscope::apply(&right).expect("the declared apply");
    assert!(report.contains("~ file /etc/b.conf"), "{report}");
    assert_eq!(root.read("etc/b.conf"), "beta\n");

    let after = journals(&root);
    assert_eq!(after.len(), 2, "{after:?}");
    let latest = journal::read(&after[1]).expect("the new journal");
    assert_eq!(latest.header.resolved.as_deref(), Some(record.id.as_str()));
    assert!(latest.committed(), "the declared apply ran to its commit");

    // And a declaration with nothing waiting on a human is itself refused.
    let mut spurious = root.options();
    spurious.resolved = Some(latest.id.clone());
    let error = lodi::hostscope::apply(&spurious).expect_err("nothing is waiting");
    assert_eq!(error.codes(), vec!["E_JOURNAL_AMBIGUOUS"]);
}

// ------------------------------------------------------------------------------ a failed action

/// A path that cannot be traversed is visible during the read-only preflight, so it is refused
/// before even a journal is created.
#[test]
fn an_invalid_parent_is_refused_during_read_only_preflight() {
    let root = Root::new("journal-failed");
    let (uid, gid) = ids(&root);
    root.write("etc/blocked.conf", "a file, not a directory\n")
        .debian_with(&format!(
            "[files.\"/etc/blocked.conf/inner.conf\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\n\
             group = \"{gid}\"\n"
        ));

    let error = lodi::hostscope::apply(&root.options()).expect_err("the action cannot be done");
    assert_eq!(error.codes(), vec!["E_PATH_ESCAPE"]);
    assert_eq!(error.exit_status(), 3);
    let text = error.to_string();
    assert!(text.contains("/etc/blocked.conf/inner.conf"), "{text}");
    assert!(text.contains("not a directory"), "{text}");
    assert!(journals(&root).is_empty());
    assert!(!root.exists("etc/lodi/host.lock"));
}
