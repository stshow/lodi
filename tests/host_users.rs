//! su-1 (LD-419): users and groups declared in `host.toml`, with no secret in the repository.
//!
//! `[users.NAME]` declares an account and `[groups.NAME]` a group and its members. An apply
//! creates a missing account with a locked password and its public keys, creates declared groups
//! with exactly their members, and never deletes an account, a group or a home: one no longer
//! declared is left where it is and said to be no longer managed. An import writes the machine's
//! human accounts only and never opens the shadow file.
//!
//! Every case is the real binary against a scratch `--root`, with the fake machine of
//! `tests/support/fakehost.rs` behind it, whose shadow tools keep the root's own account files.
//! Nothing reaches `/` and no real account tool runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::io::Write;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use fakehost::{Case, Machine, err, story};

/// An invented public key: the shape of one, and no real key anywhere.
const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIGxvZGktc3UtMS10ZXN0LWtleS1ub3QtcmVhbC0wMQ \
                   su-1-test";

/// What a converged apply or plan with `--no-home` prints.
/// Whether a switch changed nothing on the machine: nothing at all, or only the record, as after
/// an import, which writes none (LD-514).
fn unchanged(output: &std::process::Output) -> bool {
    fakehost::nothing(output)
        || (output.status.success() && err(output).contains("host: no machine changes"))
}

/// A root with no human account yet: root, two system accounts and the groups they need.
fn case(name: &str) -> Case {
    let case = Case::new(name, Machine::debian());
    case.root.write(
        "etc/passwd",
        "root:x:0:0::/root:/bin/sh\n\
         daemon:x:1:1::/usr/sbin:/usr/sbin/nologin\n\
         nobody:x:65534:65534::/nonexistent:/usr/sbin/nologin\n",
    );
    case.root.write(
        "etc/group",
        "root:x:0:\ndaemon:x:1:\nsudo:x:27:\nnogroup:x:65534:\n",
    );
    case.root.write("etc/shadow", "root:*:20000:0:99999:7:::\n");
    case.root
        .write("etc/login.defs", "UID_MIN 1000\nUID_MAX 60000\n");
    case.root
        .write("etc/shells", "/bin/sh\n/bin/bash\n/usr/bin/zsh\n");
    case
}

/// The invoking user's uid: an unprivileged test can own only files that are its own, so the
/// declared account that gets a key file is given that uid.
fn uid(case: &Case) -> u32 {
    hostroot::ids(&case.root).0
}

fn row(case: &Case, file: &str, name: &str) -> Option<Vec<String>> {
    case.root
        .read(file)
        .lines()
        .map(|line| line.split(':').map(str::to_string).collect::<Vec<_>>())
        .find(|fields| fields[0] == name)
}

fn members(case: &Case, group: &str) -> Vec<String> {
    let row = row(case, "etc/group", group).unwrap_or_else(|| panic!("no group {group}"));
    row[3]
        .split(',')
        .filter(|m| !m.is_empty())
        .map(str::to_string)
        .collect()
}

fn apply(case: &Case) -> std::process::Output {
    case.apply(&["--no-home"])
}

fn alice(uid: u32) -> String {
    format!(
        "[users.alice]\nuid = {uid}\ngroups = [\"sudo\"]\nshell = \"/bin/bash\"\n\
         home = \"/home/alice\"\nssh_keys = [\"{KEY}\"]\n"
    )
}

/// Criterion 1: a declared account is created with a locked password and its public key in
/// `authorized_keys`, owned by it; a second apply changes nothing.
#[test]
fn users_created_locked_with_ssh_keys() {
    let case = case("users-created");
    let uid = uid(&case);
    case.set_manifest(&alice(uid));

    let plan = case.verb("plan", &["--no-home"]);
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        err(&plan).contains("+ user alice"),
        "the plan names the account: {}",
        story(&plan)
    );

    let first = apply(&case);
    assert!(first.status.success(), "{}", story(&first));
    let account = row(&case, "etc/passwd", "alice").expect("alice exists");
    assert_eq!(account[2], uid.to_string());
    assert_eq!(account[5], "/home/alice");
    assert_eq!(account[6], "/bin/bash");
    let shadow = row(&case, "etc/shadow", "alice").expect("alice's shadow entry");
    assert!(
        shadow[1].starts_with('!'),
        "the password is locked: {shadow:?}"
    );
    assert!(members(&case, "sudo").contains(&"alice".to_string()));
    let useradd: Vec<String> = case
        .log()
        .into_iter()
        .filter(|l| l.starts_with("useradd "))
        .collect();
    assert_eq!(useradd.len(), 1, "{:?}", case.log());
    assert!(
        !useradd[0].contains("--password") && !useradd[0].contains(" -p "),
        "lodi never sets a password: {}",
        useradd[0]
    );

    let keys = case.root.path("home/alice/.ssh/authorized_keys");
    assert_eq!(
        fs::read_to_string(&keys).expect("the key file"),
        format!("{KEY}\n")
    );
    let meta = fs::symlink_metadata(&keys).expect("the key file's metadata");
    assert_eq!((meta.uid(), meta.mode() & 0o7777), (uid, 0o600));
    let dir = fs::symlink_metadata(case.root.path("home/alice/.ssh")).expect("the .ssh dir");
    assert_eq!((dir.uid(), dir.mode() & 0o7777), (uid, 0o700));

    let passwd = case.root.read("etc/passwd");
    let second = apply(&case);
    assert!(second.status.success(), "{}", story(&second));
    assert!(unchanged(&second), "{}", story(&second));
    assert_eq!(case.root.read("etc/passwd"), passwd);
    assert!(
        case.log().iter().all(|l| !l.starts_with("user")),
        "{:?}",
        case.log()
    );
}

/// Criterion 2: a declared group exists with exactly its declared members.
#[test]
fn groups_declared_and_members() {
    let case = case("groups-declared");
    let mut passwd = case.root.read("etc/passwd");
    passwd.push_str(
        "bob:x:2001:2001::/home/bob:/bin/bash\n\
         carol:x:2002:2002::/home/carol:/bin/bash\n\
         dave:x:2003:2003::/home/dave:/bin/bash\n",
    );
    case.root.write("etc/passwd", &passwd);
    let mut group = case.root.read("etc/group");
    group.push_str("bob:x:2001:\ncarol:x:2002:\ndave:x:2003:\nops:x:3001:carol,dave\n");
    case.root.write("etc/group", &group);
    case.set_manifest(
        "[groups.devs]\ngid = 3000\nmembers = [\"bob\", \"carol\"]\n\n\
         [groups.ops]\nmembers = [\"bob\"]\n",
    );

    let first = apply(&case);
    assert!(first.status.success(), "{}", story(&first));
    let devs = row(&case, "etc/group", "devs").expect("devs exists");
    assert_eq!(devs[2], "3000");
    assert_eq!(members(&case, "devs"), ["bob", "carol"]);
    assert_eq!(members(&case, "ops"), ["bob"]);

    let second = apply(&case);
    assert!(second.status.success(), "{}", story(&second));
    assert!(unchanged(&second), "{}", story(&second));
}

/// A root with human, system, no-login and below-`UID_MIN` accounts.
fn populated(name: &str) -> Case {
    let case = case(name);
    let mut passwd = case.root.read("etc/passwd");
    passwd.push_str(
        "sys:x:999:999::/var/lib/sys:/bin/bash\n\
         alice:x:1000:1000::/home/alice:/bin/bash\n\
         bob:x:1001:1001::/home/bob:/usr/bin/zsh\n\
         svc:x:1002:1002::/var/lib/svc:/usr/sbin/nologin\n\
         locked:x:1003:1003::/home/locked:/bin/false\n",
    );
    case.root.write("etc/passwd", &passwd);
    case.root.write(
        "etc/group",
        "root:x:0:\nsudo:x:27:alice\nsys:x:999:\nalice:x:1000:\nbob:x:1001:\n\
         svc:x:1002:\nlocked:x:1003:\ndevs:x:3000:alice,bob\nnogroup:x:65534:\n",
    );
    case
}

/// Criterion 3: an import writes the accounts at or above `UID_MIN` with a login shell, and
/// nothing else; a re-import keeps the `[users]` the person wrote.
#[test]
fn users_import_humans_only() {
    let case = populated("users-import");
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let manifest = case.manifest();
    assert!(manifest.contains("[users.alice]\n"), "{manifest}");
    assert!(manifest.contains("[users.bob]\n"), "{manifest}");
    for other in ["root", "daemon", "nobody", "sys", "svc", "locked"] {
        assert!(
            !manifest.contains(&format!("[users.{other}]")),
            "{other} is not a human account: {manifest}"
        );
    }
    let alice = &manifest[manifest.find("[users.alice]").unwrap()..];
    let alice = &alice[..alice.find("\n\n").map_or(alice.len(), |end| end + 1)];
    assert!(alice.contains("uid = 1000\n"), "{alice}");
    assert!(alice.contains("groups = [\"devs\", \"sudo\"]\n"), "{alice}");
    assert!(alice.contains("shell = \"/bin/bash\"\n"), "{alice}");
    assert!(alice.contains("home = \"/home/alice\"\n"), "{alice}");

    let plan = case.verb("plan", &["--no-home"]);
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(unchanged(&plan), "{}", story(&plan));

    // The person's own edit to the table survives a re-import (a reconcile).
    let edited = manifest.replace("shell = \"/usr/bin/zsh\"", "shell = \"/bin/bash\"");
    assert_ne!(edited, manifest);
    case.set_manifest(&edited);
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    let after = case.manifest();
    let bob = &after[after.find("[users.bob]").expect("bob is kept")..];
    assert!(bob.contains("shell = \"/bin/bash\"\n"), "{after}");
}

/// A re-import over an imported machine that did not change records the accounts its manifest
/// declares, as the first import did: the plan after it has nothing to do, and no record to
/// update (#561).
#[test]
fn users_reimport_leaves_nothing_to_do() {
    let case = populated("users-reimport");
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let plan = case.verb("plan", &["--no-home"]);
    assert!(unchanged(&plan), "after the import: {}", story(&plan));
    let again = case.import();
    assert!(again.status.success(), "{}", story(&again));
    let plan = case.verb("plan", &["--no-home"]);
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(unchanged(&plan), "after the re-import: {}", story(&plan));
}

/// Criterion 4: the import never opens the shadow file, and no hash, shadow field or private key
/// reaches anything it writes.
#[test]
fn users_never_capture_shadow() {
    let case = populated("users-shadow");
    let hash = "$6$c2FsdHNhbHQ$bm90YXJlYWxoYXNoYnV0dGhlc2hhcGVvZm9uZQ";
    case.root
        .write("etc/gshadow", &format!("devs:{hash}::alice,bob\n"));
    case.root
        .write("etc/shadow-", &format!("alice:{hash}:20000:0:99999:7:::\n"));
    case.root.write(
        "home/alice/.ssh/id_ed25519",
        "-----BEGIN OPENSSH PRIVATE KEY-----\nbm90IGEga2V5\n-----END OPENSSH PRIVATE KEY-----\n",
    );
    case.root
        .write("home/alice/.ssh/authorized_keys", &format!("{KEY}\n"));
    // The shadow file is a fifo with a writer waiting on it: an open of it for reading would be
    // seen, and would read a hash.
    fs::remove_file(case.root.path("etc/shadow")).expect("the plain shadow file goes");
    let fifo = case.root.fifo("etc/shadow");
    let (done, opened) = (
        Arc::new(AtomicBool::new(false)),
        Arc::new(AtomicBool::new(false)),
    );
    let writer = {
        let (done, opened, fifo) = (done.clone(), opened.clone(), fifo.clone());
        std::thread::spawn(move || {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .open(&fifo)
                .expect("the fifo opens for writing");
            if !done.load(Ordering::SeqCst) {
                opened.store(true, Ordering::SeqCst);
                let _ = file.write_all(format!("alice:{hash}:20000:0:99999:7:::\n").as_bytes());
            }
        })
    };

    let import = case.import();
    done.store(true, Ordering::SeqCst);
    // Release the waiting writer: this process opens the read end itself.
    let release = fs::OpenOptions::new()
        .read(true)
        .custom_flags_nonblock()
        .open(&fifo)
        .expect("the fifo opens for reading");
    writer.join().expect("the writer ends");
    drop(release);
    assert!(import.status.success(), "{}", story(&import));
    assert!(
        !opened.load(Ordering::SeqCst),
        "the import opened the shadow file"
    );
    let manifest = case.manifest();
    assert!(manifest.contains("[users.alice]\n"), "{manifest}");
    let written = written_files(&case.root.path("etc/lodi"));
    assert!(!written.is_empty());
    for (path, bytes) in &written {
        let text = String::from_utf8_lossy(bytes);
        for needle in ["$6$", hash, "PRIVATE KEY", ":20000:", "ssh-ed25519"] {
            assert!(!text.contains(needle), "{path} carries {needle:?}:\n{text}");
        }
    }
    let _ = fs::set_permissions(&fifo, fs::Permissions::from_mode(0o600));
}

trait NonBlock {
    fn custom_flags_nonblock(&mut self) -> &mut Self;
}

impl NonBlock for fs::OpenOptions {
    fn custom_flags_nonblock(&mut self) -> &mut Self {
        use std::os::unix::fs::OpenOptionsExt;
        self.custom_flags(0o4000)
    }
}

/// Every regular file below `dir`, by path, with its bytes.
fn written_files(dir: &std::path::Path) -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let meta = fs::symlink_metadata(&path).expect("metadata");
        if meta.is_dir() {
            out.extend(written_files(&path));
        } else if meta.is_file() {
            out.push((path.display().to_string(), fs::read(&path).expect("a file")));
        }
    }
    out
}

/// Criterion 5: a user and a group taken out of `host.toml` stay exactly as they were, home
/// included, and the apply says they are no longer managed.
#[test]
fn users_undeclared_left_untouched() {
    let case = case("users-undeclared");
    let uid = uid(&case);
    case.set_manifest(&format!(
        "{}\n[groups.devs]\nmembers = [\"alice\"]\n",
        alice(uid)
    ));
    let first = apply(&case);
    assert!(first.status.success(), "{}", story(&first));
    case.root.write("home/alice/notes", "kept\n");
    let before: Vec<String> = ["etc/passwd", "etc/group", "etc/shadow"]
        .iter()
        .map(|file| case.root.read(file))
        .collect();

    case.set_manifest("");
    let plan = case.verb("plan", &["--no-home"]);
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(err(&plan).contains("no longer managed"), "{}", story(&plan));
    let second = apply(&case);
    assert!(second.status.success(), "{}", story(&second));
    let said = err(&second);
    assert!(
        said.contains("= user alice (no longer managed"),
        "{}",
        story(&second)
    );
    assert!(
        said.contains("= group devs (no longer managed"),
        "{}",
        story(&second)
    );
    let after: Vec<String> = ["etc/passwd", "etc/group", "etc/shadow"]
        .iter()
        .map(|file| case.root.read(file))
        .collect();
    assert_eq!(before, after, "no account or group changed");
    assert_eq!(case.root.read("home/alice/notes"), "kept\n");
    assert!(case.root.exists("home/alice/.ssh/authorized_keys"));
    assert!(
        case.log().iter().all(|l| !l.starts_with("user")
            && !l.starts_with("group")
            && !l.starts_with("gpasswd")),
        "{:?}",
        case.log()
    );

    let third = apply(&case);
    assert!(third.status.success(), "{}", story(&third));
    assert!(unchanged(&third), "{}", story(&third));
}
