//! si-1: the host import copies no file from `/etc`. It writes the typed options lodi knows,
//! names every changed configuration file with the `[etc."PATH"]` a person can write instead,
//! and never opens a secret. `[etc."PATH"]` holds text the user wrote; the old `[files]` table
//! still applies, with one warning. An enabled extra suite of the distribution's own archive is
//! a verified `[sources]` block.
//!
//! Every case is the real binary against a scratch `--root` with a fake machine behind it
//! (`tests/support/fakehost.rs`). Nothing reaches `/` and no real tool runs (`AGENTS.md` §8).

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use fakehost::{Case, Machine, Pkg, err, story};
use serde_json::json;

/// Text no fake and no product string holds: where it shows up, a file's bytes travelled.
const EDITED: &str = "edited-by-the-owner-7f3a";

fn manifest(distro: &str, body: &str) -> String {
    format!("[host]\nversion = \"1\"\ndistro = \"{distro}\"\n\n{body}")
}

/// `owner` and `group` lines naming the invoking user, whom a scratch apply can make an owner.
fn ours(case: &Case) -> String {
    let (uid, gid) = hostroot::ids(&case.root);
    format!("owner = \"{uid}\"\ngroup = \"{gid}\"\n")
}

/// A Debian root with its basics set, a package whose configuration file the owner edited, and
/// an edited `/etc/motd`.
fn debian(name: &str) -> Case {
    let machine = Machine::debian()
        .with(Pkg::new("app").conffile("/etc/app.conf"))
        .os("locale", json!(["LANG=en_GB.UTF-8"]))
        .os("locales", json!(["C.UTF-8", "en_GB.UTF-8"]))
        .os("keymap", json!("de"));
    let case = Case::new(name, machine);
    case.root.write("etc/hostname", "lodi-test\n");
    case.root.write("etc/default/locale", "LANG=en_GB.UTF-8\n");
    case.root.write(
        "etc/default/keyboard",
        "XKBMODEL=\"pc105\"\nXKBLAYOUT=\"de\"\n",
    );
    case.root
        .write("usr/share/zoneinfo/Europe/Berlin", "TZif2\n");
    symlink(
        "../usr/share/zoneinfo/Europe/Berlin",
        case.root.path("etc/localtime"),
    )
    .expect("localtime");
    case.root
        .write("etc/app.conf", &format!("setting = {EDITED}\n"));
    case.root.write("etc/motd", &format!("{EDITED}\n"));
    case
}

/// Every regular file below `dir`, by path relative to it.
fn files(dir: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    let mut out = BTreeMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        let Ok(entries) = fs::read_dir(&at) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let meta = fs::symlink_metadata(&path).expect("a listed entry");
            if meta.is_dir() {
                stack.push(path);
            } else if meta.is_file() {
                let bytes = fs::read(&path).expect("a file");
                out.insert(path.strip_prefix(dir).unwrap().to_path_buf(), bytes);
            }
        }
    }
    out
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn import_never_copies_etc() {
    let case = debian("etc-import");
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let text = case.manifest();

    // The typed options, as the machine has them.
    for line in [
        "[system]\n",
        "hostname = \"lodi-test\"\n",
        "timezone = \"Europe/Berlin\"\n",
        "locale = \"en_GB.UTF-8\"\n",
        "keymap = \"de\"\n",
    ] {
        assert!(text.contains(line), "no {line:?}:\n{text}");
    }
    for table in ["# [services]", "# [firewall]", "# [network]"] {
        assert!(text.contains(table), "no {table}:\n{text}");
    }
    // No file copied: no [files] table, nothing beside the manifest holds the owner's bytes.
    assert!(!text.contains("[files."), "{text}");
    assert!(!case.root.exists("etc/lodi/files"), "a bundle was written");
    for (path, bytes) in files(&case.root.path("etc/lodi")) {
        assert!(
            !contains(&bytes, EDITED),
            "{} carries /etc bytes",
            path.display()
        );
    }
    // The changed file is named, with the form a person writes instead.
    assert!(text.contains("#   /etc/app.conf\n"), "{text}");
    assert!(text.contains("[etc.\"app.conf\"]"), "{text}");

    // The imported basics are what the machine has: an apply sets none of them.
    case.set_manifest(&fakehost::without_snapshot(&text));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert!(
        !case.log().iter().any(|line| line.starts_with("hostnamectl")
            || line.starts_with("timedatectl")
            || line.contains("set-locale")
            || line.contains("set-x11-keymap")),
        "{:?}",
        case.log()
    );
}

#[test]
fn etc_path_opt_in() {
    let case = debian("etc-opt-in");
    case.set_manifest(&manifest("debian", ""));
    let first = case.apply(&[]);
    assert!(first.status.success(), "{}", story(&first));
    let etc = case.root.path("etc");
    let before = files(&etc);

    let text = "Welcome.\nThis machine is declared.\n";
    case.set_manifest(&manifest(
        "debian",
        &format!(
            "[etc.\"motd\"]\ntext = \"\"\"\n{text}\"\"\"\n{}",
            ours(&case)
        ),
    ));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    assert!(
        fakehost::out(&plan).contains("/etc/motd"),
        "{}",
        story(&plan)
    );
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(case.root.read("etc/motd"), text);
    assert!(!err(&apply).contains("W_LEGACY_FILES"), "{}", story(&apply));

    // No other path under /etc was written, lodi's own directory aside.
    let after = files(&etc);
    for (path, bytes) in &after {
        if path.starts_with("lodi") || path == Path::new("motd") {
            continue;
        }
        assert_eq!(
            before.get(path),
            Some(bytes),
            "{} was written",
            path.display()
        );
    }
    let again = case.apply(&[]);
    assert!(again.status.success(), "{}", story(&again));
    assert_eq!(case.root.read("etc/motd"), text);

    // Only a path under /etc, and never a secret one.
    for (key, code) in [
        ("../root/.profile", "E_PATH_ESCAPE"),
        ("/etc/motd", "E_PATH_ESCAPE"),
        ("shadow", "E_PROTECTED_PATH"),
        ("ssh/ssh_host_ed25519_key", "E_PROTECTED_PATH"),
    ] {
        case.set_manifest(&manifest(
            "debian",
            &format!("[etc.\"{key}\"]\ntext = \"x\\n\"\n"),
        ));
        let refused = case.plan();
        assert!(!refused.status.success(), "{key}: {}", story(&refused));
        assert!(err(&refused).contains(code), "{key}: {}", story(&refused));
    }
}

#[test]
fn legacy_files_warn() {
    let case = debian("etc-legacy");
    case.set_manifest(&manifest(
        "debian",
        &format!(
            "[files.\"/etc/motd\"]\ncontent = \"legacy\\n\"\n{}",
            ours(&case)
        ),
    ));
    let plan = case.plan();
    assert!(plan.status.success(), "{}", story(&plan));
    let apply = case.apply(&[]);
    assert!(apply.status.success(), "{}", story(&apply));
    assert_eq!(case.root.read("etc/motd"), "legacy\n");
    for output in [&plan, &apply] {
        let warnings: Vec<&str> = std::str::from_utf8(&output.stderr)
            .unwrap()
            .lines()
            .filter(|line| line.contains("W_LEGACY_FILES"))
            .collect();
        assert_eq!(warnings.len(), 1, "{}", story(output));
        assert!(warnings[0].contains("[etc.\"PATH\"]"), "{}", story(output));
    }
}

/// An armored "public keyring": the framing is real, the body is invented.
const KEYRING: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n\
                       bm90IGEga2V5OiBnZW5lcmF0ZWQgZm9yIGEgdGVzdA==\n=AAAA\n\
                       -----END PGP PUBLIC KEY BLOCK-----\n";

#[test]
fn import_extra_suite_uses_own_archive() {
    let case = Case::new("etc-backports", Machine::debian());
    case.root.write(
        "etc/os-release",
        "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\nVERSION_ID=\"24.04\"\n\
         VERSION_CODENAME=noble\n",
    );
    case.root.write(
        "etc/apt/sources.list.d/ubuntu.sources",
        "Types: deb\nURIs: http://archive.ubuntu.com/ubuntu/\n\
         Suites: noble noble-updates noble-backports\nComponents: main universe\n\
         Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n",
    );
    case.root
        .write("usr/share/keyrings/ubuntu-archive-keyring.gpg", KEYRING);
    case.edit(|m| {
        let archive = "http://archive.ubuntu.com/ubuntu";
        m["sources"] = json!({});
        for suite in ["noble", "noble-updates", "noble-backports"] {
            m["sources"][suite] = json!({
                "key": format!("{archive} {suite}/main amd64 Packages"),
                "release": format!("o=Ubuntu,a={suite},n=noble,l=Ubuntu,c=main,b=amd64"),
            });
        }
        for record in m["available"].as_object_mut().unwrap().values_mut() {
            record["repo"] = json!("noble");
        }
    });
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let text = case.manifest();
    let digest = lodi::util::sha256_hex(KEYRING.as_bytes());
    for line in [
        "# [sources.backports]\n".to_string(),
        "# uris = [\"https://archive.ubuntu.com/ubuntu/\"]\n".to_string(),
        "# suites = [\"${host.codename}-backports\"]\n".to_string(),
        "# components = [\"main\", \"universe\"]\n".to_string(),
        "# signed_by = \"files/etc/apt/keyrings/lodi-backports.asc\"\n".to_string(),
        format!("# signed_by_sha256 = \"{digest}\"\n"),
        // #228: a range a later lodi satisfies, never a bare version.
        "#   min_lodi_version = \">=1.3.0\"\n".to_string(),
    ] {
        assert!(text.contains(&line), "no {line:?}:\n{text}");
    }
    let tail = &text[text.find("# NOT CAPTURED").expect("the block")..];
    assert!(!tail.contains("noble-backports"), "{tail}");
    // The key travels beside the file, verified by that digest.
    let key = fs::read(
        case.root
            .path("etc/lodi/files/etc/apt/keyrings/lodi-backports.asc"),
    )
    .expect("the keyring travels");
    assert_eq!(lodi::util::sha256_hex(&key), digest);
    // Uncommented, the block is a [sources] entry the manifest reads.
    let taken: String = text
        .lines()
        .skip_while(|line| *line != "# [sources.backports]")
        .take_while(|line| !line.is_empty())
        .filter(|line| !line.starts_with("# # "))
        .map(|line| format!("{}\n", line.trim_start_matches("# ")))
        .collect();
    case.set_manifest(&manifest(
        "ubuntu",
        &format!("min_lodi_version = \">=1.3.0\"\n\n{taken}"),
    ));
    let plan = case.plan();
    assert!(
        !err(&plan).contains("E_SYNTAX") && !err(&plan).contains("E_TYPE"),
        "{taken}\n{}",
        story(&plan)
    );
}

/// The secrets a machine keeps under `/etc`, each readable by everyone so that nothing but
/// lodi's own rule keeps it closed.
const SECRETS: &[&str] = &[
    "etc/shadow",
    "etc/gshadow",
    "etc/ssh/ssh_host_ed25519_key",
    "etc/ssl/private/server.key",
];

/// A watch on each path for an open, through the kernel's own inotify: an import that opened one
/// shows as an `IN_OPEN` event, whoever opened it.
struct Opened {
    fd: i32,
    watched: BTreeMap<i32, String>,
}

impl Opened {
    fn watch(root: &Path, paths: &[&str]) -> Opened {
        // SAFETY: plain syscalls on a descriptor this struct owns and closes on drop.
        let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
        assert!(fd >= 0, "inotify_init1");
        let mut watched = BTreeMap::new();
        for path in paths {
            let full = CString::new(root.join(path).as_os_str().as_bytes()).unwrap();
            let wd = unsafe { libc::inotify_add_watch(fd, full.as_ptr(), libc::IN_OPEN) };
            assert!(wd >= 0, "a watch on {path}");
            watched.insert(wd, (*path).to_string());
        }
        Opened { fd, watched }
    }

    /// The watched paths something opened since the watch began.
    fn seen(&self) -> Vec<String> {
        let mut buffer = [0u8; 4096];
        let mut out = Vec::new();
        loop {
            let n = unsafe { libc::read(self.fd, buffer.as_mut_ptr().cast(), buffer.len()) };
            if n <= 0 {
                return out;
            }
            let mut at = 0usize;
            while at < n as usize {
                // SAFETY: the kernel writes whole `inotify_event` records into the buffer.
                let event: libc::inotify_event =
                    unsafe { std::ptr::read_unaligned(buffer[at..].as_ptr().cast()) };
                if let Some(path) = self.watched.get(&event.wd) {
                    out.push(path.clone());
                }
                at += std::mem::size_of::<libc::inotify_event>() + event.len as usize;
            }
        }
    }
}

impl Drop for Opened {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

#[test]
fn import_never_reads_secrets() {
    let secret = "secret-bytes-9c1e";
    // A package that says it ships each secret as a configuration file, and a shipped digest
    // none of them matches: the package manager itself names every one as changed.
    let mut pkg = Pkg::new("keeper");
    for path in SECRETS {
        pkg = pkg.conffile(&format!("/{path}"));
    }
    let case = Case::new("etc-secrets", Machine::debian().with(pkg));
    for path in SECRETS {
        case.root.write(path, &format!("{secret}\n"));
        case.root.chmod(path, 0o644);
    }
    let watch = Opened::watch(&case.root.dir, SECRETS);
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    assert_eq!(watch.seen(), Vec::<String>::new(), "{}", story(&import));
    for (path, bytes) in files(&case.root.path("etc/lodi")) {
        assert!(
            !contains(&bytes, secret),
            "{} carries a secret",
            path.display()
        );
    }
    assert!(!contains(&import.stdout, secret) && !contains(&import.stderr, secret));
    // Each is named as never read, by its path alone.
    let text = case.manifest();
    for path in SECRETS {
        assert!(text.contains(&format!("#   /{path}\n")), "{path}:\n{text}");
    }
}

/// Debian's cloud image reaches its archive through a local mirror list, and `backports` is
/// Debian's own archive under the origin `Debian Backports`: the block takes the list's address.
#[test]
fn import_extra_suite_through_a_mirror_list() {
    let case = Case::new("etc-mirror", Machine::debian());
    case.root.write(
        "etc/apt/sources.list.d/debian.sources",
        "Types: deb\nURIs: mirror+file:///etc/apt/mirrors/debian.list\n\
         Suites: bookworm bookworm-updates bookworm-backports\nComponents: main\n\
         Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n",
    );
    case.root.write(
        "etc/apt/mirrors/debian.list",
        "https://deb.debian.org/debian\n",
    );
    case.root
        .write("usr/share/keyrings/debian-archive-keyring.gpg", KEYRING);
    case.edit(|m| {
        let list = "mirror+file:/etc/apt/mirrors/debian.list";
        m["sources"] = json!({});
        for (suite, origin) in [
            ("bookworm", "Debian"),
            ("bookworm-updates", "Debian"),
            ("bookworm-backports", "Debian Backports"),
        ] {
            m["sources"][suite] = json!({
                "key": format!("{list} {suite}/main amd64 Packages"),
                "release": format!("o={origin},a={suite},n={suite},l={origin},c=main,b=amd64"),
            });
        }
        for record in m["available"].as_object_mut().unwrap().values_mut() {
            record["repo"] = json!("bookworm");
        }
    });
    let import = case.import();
    assert!(import.status.success(), "{}", story(&import));
    let text = case.manifest();
    for line in [
        "# [sources.backports]\n",
        "# uris = [\"https://deb.debian.org/debian\"]\n",
        "# suites = [\"${host.codename}-backports\"]\n",
    ] {
        assert!(text.contains(line), "no {line:?}:\n{text}");
    }
    assert!(
        text.contains("# the distribution's own repositories, which every install has: 1\n"),
        "{text}"
    );
}
