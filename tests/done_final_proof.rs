//! hp-1 (LD-428): the definition of done's final proof (`docs/DONE.md` §4) on a scratch root.
//!
//! One repository, committed by the test and served live on loopback by
//! `scripts/git-http-loopback.py`, is applied by URL as the root apply of a machine applies it: the host,
//! then each home. The machine then matches what is declared, checked in the definition of
//! done's order. `git revert` of the newer commit and a second apply bring the earlier pinned
//! versions back, and a dependency no manifest names is not moved.
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it; the recorded dated archives of
//! `tests/fixtures/host/pin/` and the repository are both reached through
//! `LODI_FETCH_REWRITE`. Nothing reaches `/` or a forge, and no real package manager runs
//! (`AGENTS.md` §8). The guest row `done-final-proof-https-revert` proves the same on bare guests.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/fixture.rs"]
mod fixture;
#[path = "support/git_loopback.rs"]
mod git_loopback;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::process::{Command, Output, Stdio};

use fakehost::{Machine, story};
use git_loopback::{Served, commit_all, git};
use pinverbs::{BC, OLDER, RECENT, TZ_OLDER, TZ_RECENT, Verbs, ok, pkg};
use serde_json::{Value, json};

const URL: &str = "git+https://git.example.test/hosts.git";
const HTTPS: &str = "https://git.example.test/hosts.git";

/// The dependency `bc` pulls in, which no manifest names.
const DEP: &str = "libdep";
const DEP_VERSION: &str = "3.1-2";

/// The scratch user manager keeps real uid-labelled calls; host calls still use fakepm.
const USER_MANAGER: &str = r#"#!/bin/sh
if [ "$1" != --user ]; then exec "${0%/systemctl}/host-systemctl" "$@"; fi
sd="${0%/bin/systemctl}/user-manager"
echo "uid=$(@ID@ -u) systemctl $*" >> "$sd/calls"
shift
verb=$1
shift
unit=
for arg in "$@"; do case $arg in --now|--) ;; *) unit=$arg ;; esac; done
state=disabled
active=0
[ -f "$sd/state" ] && read -r state < "$sd/state"
[ -f "$sd/active" ] && read -r active < "$sd/active"
case $verb in
  is-enabled) echo "$state"; [ "$state" = enabled ] ;;
  is-active) if [ "$active" = 1 ]; then echo active; else echo inactive; exit 3; fi ;;
  daemon-reload) ;;
  enable) echo enabled > "$sd/state"; echo 1 > "$sd/active" ;;
  disable) echo disabled > "$sd/state"; echo 0 > "$sd/active" ;;
  *) exit 64 ;;
esac
"#;

fn user_manager(verbs: &Verbs) {
    let _writing = fakehost::writing();
    let bin = verbs.case.base().join("bin");
    let sd = verbs.case.base().join("user-manager");
    fs::create_dir_all(&sd).unwrap();
    fs::write(sd.join("calls"), "").unwrap();
    fs::rename(bin.join("systemctl"), bin.join("host-systemctl")).unwrap();
    let id = std::env::split_paths(&std::env::var_os("PATH").unwrap())
        .map(|dir| dir.join("id"))
        .find(|path| path.is_file())
        .unwrap();
    fs::write(
        bin.join("systemctl"),
        USER_MANAGER.replace("@ID@", &id.display().to_string()),
    )
    .unwrap();
    fs::set_permissions(bin.join("systemctl"), fs::Permissions::from_mode(0o755)).unwrap();
    verbs.case.root.write("runtime/systemd/private", "");
}

fn homes(verbs: &Verbs, enabled: bool) {
    let body = include_bytes!("fixtures/done/proof-tool");
    verbs
        .server
        .files
        .lock()
        .unwrap()
        .insert("https://fixtures.test/proof-tool".into(), body.to_vec());
    let (uid, gid) = hostroot::ids(&verbs.case.root);
    let mut passwd = verbs.case.root.read("etc/passwd");
    if !passwd.lines().any(|line| line.starts_with("another:")) {
        passwd.push_str(&format!("another:x:{uid}:{gid}::/people/another:/bin/sh\n"));
        verbs.case.root.write("etc/passwd", &passwd);
    }
    fs::create_dir_all(verbs.case.root.path("people/another")).unwrap();
    let text =
        include_str!("fixtures/done/home.toml").replace("@SHA@", &lodi::util::sha256_hex(body));
    let text = if enabled {
        text
    } else {
        text.replace("enable = true", "enable = false")
            .replace("declared", "earlier")
    };
    let homes = verbs.case.root.path("hosts/box/home");
    fs::create_dir_all(homes.join("sample")).unwrap();
    fs::write(homes.join("sample").join("home.toml"), &text).unwrap();
    fs::create_dir_all(homes.join("another")).unwrap();
    fs::write(
        homes.join("another").join("home.toml"),
        "[home.file.\"note\"]\ntext = \"their declaration\\n\"\n",
    )
    .unwrap();
    // A fetched tree is never written: the homes' tools are locked in the checkout first.
    // `lodi update` moves a dated snapshot to today, which the recorded archive does not hold;
    // on the floating host it locks the tools and the explicit pins alone, and `pin --all`
    // then dates the host again.
    let text = verbs.host_toml();
    verbs.set_host(
        &text
            .lines()
            .filter(|line| !line.starts_with("snapshot = "))
            .map(|line| format!("{line}\n"))
            .collect::<String>(),
    );
    ok(&update(verbs));
    ok(&verbs.run("pin", &["--all", "--to", RECENT, &verbs.hosts()]));
}

/// `lodi update` of the checkout, as its owner runs it before a commit.
// check-host-safety: refusal — it carries the case's scratch --root.
fn update(verbs: &Verbs) -> Output {
    let _spawning = fakehost::spawning();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["update", &verbs.hosts(), "--host", "box", "--root"])
        .arg(&verbs.case.root.dir)
        .env("PATH", verbs.case.fake_path())
        .env("HOME", verbs.case.root.path("home"))
        .env("XDG_CONFIG_HOME", verbs.case.root.path("home/config"))
        .env("XDG_DATA_HOME", verbs.case.root.path("home/data"))
        .env("LODI_HOME", verbs.case.root.path("lodi-home"))
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env("LODI_FETCH_REWRITE", verbs.server.rewrite())
        .env_remove("LODI_REPO")
        .stdin(Stdio::null())
        .output()
        .expect("lodi runs")
}

/// A fresh Debian machine called `box`: `bc` and the newer `tzdata` installed by hand, `bc`'s
/// dependency beside them, one disabled service, the basics a fresh guest has, and the recorded
/// dated archives on loopback.
fn machine(name: &str) -> Verbs {
    let machine = Machine::debian()
        .with(pkg("bc", BC).depends(&[DEP]))
        .with(pkg("tzdata", TZ_RECENT))
        .with(pkg(DEP, DEP_VERSION).dep())
        .unit("web.service", "disabled", false, "disabled")
        .os("locale", json!(["LANG=C.UTF-8"]))
        .os("locales", json!(["C.UTF-8", "en_GB.UTF-8"]))
        .os("keymap", json!("us"));
    let verbs = Verbs::on(name, machine, false);
    let root = &verbs.case.root;
    root.write("etc/group", "root:x:0:\nsudo:x:27:\n");
    root.write("etc/shadow", "root:*:20000:0:99999:7:::\n");
    root.write("etc/login.defs", "UID_MIN 1000\nUID_MAX 60000\n");
    root.write("etc/shells", "/bin/sh\n/bin/bash\n");
    for zone in ["UTC", "Europe/Berlin"] {
        root.write(&format!("usr/share/zoneinfo/{zone}"), "TZif2\n");
    }
    symlink("../usr/share/zoneinfo/UTC", root.path("etc/localtime")).unwrap();
    fs::create_dir_all(root.path("people/sample")).unwrap();
    root.write(
        "etc/default/grub",
        "GRUB_DEFAULT=0\nGRUB_TIMEOUT=5\nGRUB_CMDLINE_LINUX=\"\"\n",
    );
    root.write("etc/default/grub.d/15_timeout.cfg", "GRUB_TIMEOUT=1\n");
    root.write(
        "boot/grub/grub.cfg",
        "menuentry 'Linux' {\n\tlinux /vmlinuz root=/dev/vda1 ro\n}\n",
    );
    root.write("boot/efi/EFI/BOOT/BOOTX64.EFI", "GRUB removable");
    root.write("sys/firmware/efi/efivars/.keep", "");
    root.write("proc/cmdline", "root=/dev/vda1 ro\n");
    root.write(
        "proc/sys/kernel/random/boot_id",
        "00000000-0000-4000-8000-000000000001\n",
    );
    root.write("proc/sys/vm/swappiness", "60\n");
    user_manager(&verbs);
    verbs
}

/// Whether a second apply changed nothing: the host and both homes each say `nothing to do`,
/// and no line plans a change (`+`, `-` or `~`).
fn settled(out: &str) -> bool {
    out.matches("\nnothing to do\n").count() + usize::from(out.starts_with("nothing to do\n")) == 3
        && !out
            .lines()
            .any(|line| line.starts_with(['+', '-', '~']) || line.starts_with("created"))
}

/// `lodi VERB ARGS` with both the archive and the repository behind loopback.
// check-host-safety: refusal — every verb carries the case's scratch --root.
fn run(verbs: &Verbs, served: &Served, verb: &str, args: &[&str]) -> Output {
    let rewrite = format!(
        "{};{}",
        served.rewrite(HTTPS, "hosts"),
        verbs.server.rewrite()
    );
    let _spawning = fakehost::spawning();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .arg(verb)
        .args(args)
        .arg("--root")
        .arg(&verbs.case.root.dir)
        .env("PATH", verbs.case.fake_path())
        .env("HOME", verbs.case.root.path("home"))
        .env("XDG_CONFIG_HOME", verbs.case.root.path("home/config"))
        .env("XDG_DATA_HOME", verbs.case.root.path("home/data"))
        .env("LODI_HOME", verbs.case.root.path("lodi-home"))
        .env("XDG_RUNTIME_DIR", verbs.case.root.path("runtime"))
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env("LODI_FETCH_REWRITE", rewrite)
        .env("LODI_FETCH_ATTEMPTS", "1")
        .env_remove("LODI_REPO")
        .env_remove("SUDO_UID")
        .env_remove("SUDO_GID")
        .stdin(Stdio::null())
        .output()
        .expect("lodi runs")
}

fn installed(verbs: &Verbs, name: &str) -> Value {
    verbs.case.machine()["installed"][name]["version"].clone()
}

/// The repository: `box/host.toml` from an import, every package pinned to the newer day and
/// `tzdata` to `tzdata_day`, then each `(was, is)` edit made to the imported text.
fn pinned_repository(verbs: &Verbs, tzdata_day: &str, edits: &[(&str, &str)]) {
    fs::remove_dir(verbs.dir()).unwrap();
    ok(&verbs.run("import", &[&verbs.hosts()]));
    ok(&verbs.run("pin", &["--all", "--to", RECENT, &verbs.hosts()]));
    ok(&verbs.run("pin", &["tzdata", "--to", tzdata_day, &verbs.hosts()]));
    let mut text = verbs.host_toml();
    for (was, is) in edits {
        assert!(text.contains(was), "the import wrote no `{was}`:\n{text}");
        text = text.replacen(was, is, 1);
    }
    verbs.set_host(&text);
}

/// Criterion 1 (D5, LD-395). A served repository pins `tzdata` older, then a newer commit pins
/// it newer; each is applied by URL. `git revert HEAD` and an apply bring the older version back,
/// `bc` stays exactly where both commits pin it, and `bc`'s undeclared dependency never moves.
#[test]
fn revert_restores_machine() {
    let verbs = machine("done-revert");
    pinned_repository(&verbs, OLDER, &[]);
    homes(&verbs, false);
    let hosts = verbs.case.root.path("hosts");
    commit_all(&hosts, "older tzdata");
    let served_dir = verbs.case.base().join("served");
    fs::create_dir_all(&served_dir).unwrap();
    let served = Served::start(&served_dir);
    served.push(&hosts, "hosts");

    let first = run(&verbs, &served, "apply", &[URL]);
    ok(&first);
    assert_eq!(installed(&verbs, "tzdata"), TZ_OLDER, "{}", story(&first));

    ok(&verbs.run("pin", &["tzdata", "--to", RECENT, &verbs.hosts()]));
    homes(&verbs, true);
    commit_all(&hosts, "newer tzdata");
    served.push(&hosts, "hosts");
    let newer = run(&verbs, &served, "apply", &[URL, "--refresh"]);
    ok(&newer);
    assert_eq!(installed(&verbs, "tzdata"), TZ_RECENT, "{}", story(&newer));

    git(&hosts, &["revert", "--no-edit", "HEAD"]);
    served.push(&hosts, "hosts");
    let reverted = run(&verbs, &served, "apply", &[URL, "--refresh"]);
    ok(&reverted);
    assert_eq!(
        installed(&verbs, "tzdata"),
        TZ_OLDER,
        "{}",
        story(&reverted)
    );
    assert_eq!(installed(&verbs, "bc"), BC, "{}", story(&reverted));
    assert_eq!(verbs.case.root.read("people/sample/note"), "earlier\n");
    assert_eq!(
        fs::read_to_string(verbs.case.base().join("user-manager/active")).unwrap(),
        "0\n"
    );
    assert_eq!(installed(&verbs, DEP), DEP_VERSION, "{}", story(&reverted));
    assert_eq!(
        verbs.case.machine()["installed"][DEP]["explicit"],
        false,
        "{}",
        story(&reverted)
    );

    let again = run(&verbs, &served, "apply", &[URL]);
    ok(&again);
    assert!(settled(&fakehost::out(&again)), "{}", story(&again));
}

/// Criterion 2. One served repository declares packages, services, users, OS basics,
/// kernel and boot, and every home. Check them in that order after one top-level apply.
#[test]
fn apply_matches_every_declared_item() {
    let verbs = machine("done-every-item");
    let (uid, gid) = hostroot::ids(&verbs.case.root);
    // The import wrote the account and the basics as they are; the repository changes them.
    pinned_repository(
        &verbs,
        OLDER,
        &[
            ("groups = []\n", "groups = [\"sudo\"]\n"),
            (
                "timezone = \"UTC\"\n",
                "timezone = \"Europe/Berlin\"\nlocale = \"en_GB.UTF-8\"\nkeymap = \"de\"\n\n\
                 [services]\n\"web.service\" = \"enabled\"\n",
            ),
        ],
    );
    let mut host = verbs.host_toml();
    host.push_str(include_str!("fixtures/done/host-extra.toml"));
    verbs.set_host(&host);
    homes(&verbs, true);
    let hosts = verbs.case.root.path("hosts");
    commit_all(&hosts, "the machine");
    let served_dir = verbs.case.base().join("served");
    fs::create_dir_all(&served_dir).unwrap();
    let served = Served::start(&served_dir);
    served.push(&hosts, "hosts");

    let applied = run(&verbs, &served, "apply", &[URL]);
    ok(&applied);
    let story = story(&applied);
    let root = &verbs.case.root;

    // 1. Packages, at their pinned versions; the undeclared dependency where it was.
    assert_eq!(installed(&verbs, "tzdata"), TZ_OLDER, "{story}");
    assert_eq!(installed(&verbs, "bc"), BC, "{story}");
    assert_eq!(installed(&verbs, DEP), DEP_VERSION, "{story}");
    // 2. Services.
    assert_eq!(
        verbs.case.unit("web.service"),
        ("enabled".into(), true),
        "{story}"
    );
    // 3. Users.
    let sudo = root
        .read("etc/group")
        .lines()
        .find(|line| line.starts_with("sudo:"))
        .unwrap()
        .to_string();
    assert!(
        sudo.split(':')
            .nth(3)
            .unwrap()
            .split(',')
            .any(|m| m == "sample"),
        "{sudo}\n{story}"
    );
    let account = format!("sample:x:{uid}:{gid}::/people/sample:/bin/sh");
    assert!(
        root.read("etc/passwd").lines().any(|l| l == account),
        "{story}"
    );
    // 4. OS basics.
    assert_eq!(root.read("etc/hostname"), "box\n", "{story}");
    let zone = fs::read_link(root.path("etc/localtime")).unwrap();
    assert!(
        zone.ends_with("zoneinfo/Europe/Berlin"),
        "{zone:?}\n{story}"
    );
    let os = &verbs.case.machine()["os"];
    assert_eq!(os["locale"], json!(["LANG=en_GB.UTF-8"]), "{story}");
    assert_eq!(os["keymap"], json!("de"), "{story}");
    // 5. Kernel and boot. The running kernel is checked after a reboot on the guests.
    assert!(
        root.read("etc/grub.d/43_lodi_trial")
            .contains("lodi_hp1=on"),
        "{story}"
    );
    assert!(
        root.read("etc/modules-load.d/lodi.conf").contains("dummy"),
        "{story}"
    );
    assert!(root.exists("sys/module/dummy"), "{story}");
    assert_eq!(root.read("proc/sys/vm/swappiness"), "17\n", "{story}");
    let boot = root.read("etc/default/grub.d/99-lodi-boot.cfg");
    assert!(
        boot.contains("GRUB_TIMEOUT=3") && boot.contains("GRUB_DEFAULT=\"0\""),
        "{boot}"
    );
    // 6. Every home.
    assert_eq!(root.read("people/sample/note"), "declared\n", "{story}");
    assert_eq!(
        root.read("people/another/note"),
        "their declaration\n",
        "{story}"
    );
    assert_eq!(
        fs::metadata(root.path("people/another/note"))
            .unwrap()
            .uid(),
        uid
    );
    let tool = root.path("people/sample/.local/share/lodi/home-scope/profile/bin/proof-tool");
    let tool = Command::new(tool).output().unwrap();
    assert!(tool.status.success());
    assert_eq!(tool.stdout, b"proof-tool 1.0.0\n");
    let sd = verbs.case.base().join("user-manager");
    assert_eq!(fs::read_to_string(sd.join("state")).unwrap(), "enabled\n");
    assert_eq!(fs::read_to_string(sd.join("active")).unwrap(), "1\n");
    let calls = fs::read_to_string(sd.join("calls")).unwrap();
    assert!(!calls.is_empty());
    assert!(
        calls
            .lines()
            .all(|line| line.starts_with(&format!("uid={uid} systemctl --user ")))
    );
    assert_ne!(uid, 0, "the user service must not run as root");

    let again = run(&verbs, &served, "apply", &[URL]);
    ok(&again);
    assert!(
        settled(&fakehost::out(&again)),
        "{}",
        fakehost::story(&again)
    );
}
