//! M-Import T-2, as si-1 (LD-421) leaves it: the import names every configuration file the
//! package manager reports as changed and copies none of them.
//!
//! Offline, deterministic and guestless, in the idiom `tests/host_import_core.rs` established:
//! the product's own library runs against a scratch root below `CARGO_TARGET_TMPDIR`, behind
//! test-owned shims first on `PATH` that log their argv and replay a hand-authored recording.
//! No package manager runs here and none could — this machine has none and must never get one
//! (`AGENTS.md` §8) — and no `lodi host` command is run against `/` anywhere in this file.
//!
//! # What the recordings are
//!
//! `tests/fixtures/host/import/debian-12/{capture,refusals}/` and
//! `tests/fixtures/host/import/arch/capture/` add one file to the T-1 recordings: the
//! configuration list each family prints — dpkg's `${Conffiles}` and pacman's `-Qii` backup
//! rows. They are hand-authored from public package metadata, exactly as `PROVENANCE.json`
//! beside them says, and were captured from no machine. The files themselves are written by the
//! case into its own scratch root: a real mode, a real symbolic link, real content.
//!
//! Since 1.10 no file is captured: each changed one is named with the `[etc."PATH"]` a person
//! writes instead, a path on the hard list is named as never read and is never opened, and
//! nothing a file holds reaches the emitted manifest or a warning.

#[path = "support/credentials.rs"]
mod credentials;
#[path = "support/hostroot.rs"]
mod hostroot;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`,
/// LD-286): `hostroot.rs` is included by `#[path]` and takes the helper from here.
#[path = "support/wait.rs"]
mod wait;

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, Once, RwLock};

use hostroot::Root;
use lodi::hostscope::import::files::{self, Capture, Reason};
use lodi::hostscope::manifest;
use lodi::hostscope::pm::{self, ConffileStatus};
use lodi::hostscope::safety::{Gate, Operation};

/// The instant the emitted manifest is stamped with, so that the round trip is over bytes a
/// real import would write.
const CAPTURED_AT: &str = "2026-09-22T01:37:11Z";

/// A host name planted in every scratch root: the emitted bytes must not carry it out.
const HOST_NAME: &str = "a-machine-that-was-read";

/// The names this test gives the invoking user and group inside its scratch roots. They are
/// invented: no real user name, group name or home path is ever written into a root here.
const OWNER: &str = "lodi-test-owner";
const GROUP: &str = "lodi-test-group";

/// The bytes `/etc/default/keyboard` is recorded as having shipped with, whose MD5 is the digest
/// `capture/conffiles.txt` carries for it. A file equal to this is **unmodified**.
const SHIPPED_KEYBOARD: &str = "# keyboard defaults\nXKBMODEL=\"pc105\"\nXKBLAYOUT=\"us\"\n";

/// The shims are one directory shared by the whole binary, so cases run one at a time.
static SERIAL: Mutex<()> = Mutex::new(());

/// Held for writing while this file's own [`write_shims`] writes them, and for reading around
/// every spawn that might exec one (#465, the same class of defect #456 fixed for the
/// fakehost-based binaries in `tests/support/fakehost.rs`). `SERIAL` already keeps this file's own
/// cases from overlapping, but the write and every spawn still take this guard so a shim write can
/// never race a spawn from anywhere else in this process.
static SHIMS: RwLock<()> = RwLock::new(());

fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/import")
}

/// The shim directory, first on this process's `PATH`, and the state directory they log into.
fn shims() -> (PathBuf, PathBuf) {
    static SETUP: Once = Once::new();
    static BASE: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    let base = BASE
        .get_or_init(|| support::scratch("import-files-shims"))
        .clone();
    let (bin, state) = (base.join("bin"), base.join("state"));
    SETUP.call_once(|| {
        hostroot::shims();
        fs::create_dir_all(&bin).expect("the shim directory");
        fs::create_dir_all(&state).expect("the shim state directory");
        write_shims(&bin, &state);
        let existing = std::env::var("PATH").unwrap_or_default();
        let joined = format!("{}:{existing}", bin.display());
        // SAFETY: every `PATH` access in this binary happens after this `Once` completes.
        unsafe { std::env::set_var("PATH", joined) };
    });
    (bin, state)
}

/// One shim per program either family can reach for. `-Qii` is matched before `-Qi`, because
/// the first is a prefix of nothing and the second is a prefix of the first.
fn write_shims(bin: &Path, state: &Path) {
    let _writing = SHIMS.write().unwrap_or_else(|e| e.into_inner());
    let bodies = [
        ("apt-get", "exit 0\n"),
        ("dpkg", "exit 0\n"),
        (
            "dpkg-query",
            "case \"$*\" in\n\
             *Conffiles*) cat \"$fixtures/conffiles.txt\" ;;\n\
             *Priority*) cat \"$fixtures/dpkg-survey.txt\" ;;\n\
             *) cat \"$fixtures/dpkg-query.txt\" ;;\n\
             esac\n",
        ),
        (
            "apt-cache",
            "case \"$1\" in\npolicy) cat \"$fixtures/policy.txt\" ;;\nesac\n",
        ),
        (
            "apt-mark",
            "case \"$1\" in\n\
             showauto) cat \"$fixtures/apt-mark-showauto.txt\" ;;\n\
             showhold) cat \"$fixtures/apt-mark-showhold.txt\" ;;\n\
             esac\n",
        ),
        (
            "pacman",
            "case \"$1\" in\n\
             -Qii) cat \"$fixtures/pacman-Qii.txt\" ;;\n\
             -Q) cat \"$fixtures/pacman-Q.txt\" ;;\n\
             -Qe) cat \"$fixtures/pacman-Qe.txt\" ;;\n\
             -Qi) cat \"$fixtures/pacman-Qi.txt\" ;;\n\
             -Qqem) cat \"$fixtures/pacman-Qqem.txt\" ;;\n\
             -Si)\n\
             \x20 cat \"$fixtures/pacman-Si.txt\"\n\
             \x20 if [ -f \"$fixtures/pacman-Si.err\" ]; then\n\
             \x20   cat \"$fixtures/pacman-Si.err\" >&2; exit 1\n\
             \x20 fi ;;\n\
             esac\n",
        ),
    ];
    for (name, body) in bodies {
        let script = format!(
            "#!/bin/sh\n\
             # A test-owned shim of tests/host_import_files.rs. It logs its argv and replays a\n\
             # hand-authored recording. No package manager runs on this machine.\n\
             # The product starts it with a cleared environment and a fixed PATH (LD-357), so the\n\
             # PATH its own tools are found on is baked in when the test writes it.\n\
             PATH='{path}'; export PATH\n\
             state='{state}'\n\
             fixtures=$(cat \"$state/fixtures\")\n\
             {{ printf '%s' '{name}'\n\
             \x20 for a in \"$@\"; do printf '\\037%s' \"$a\"; done\n\
             \x20 printf '\\036'; }} >> \"$state/log\"\n\
             {body}",
            state = state.display(),
            path = std::env::var("PATH").unwrap_or_default(),
        );
        let path = bin.join(name);
        fs::write(&path, script).expect("a shim");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("a shim mode");
    }
}

/// One scratch root behind the shims, with one scenario's recordings selected.
struct Case {
    root: Root,
    state: PathBuf,
    _serial: MutexGuard<'static, ()>,
}

impl Case {
    fn new(name: &str, distro: &str, scenario: &str) -> Case {
        let serial = SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (_, state) = shims();
        let fixtures = fixtures_root().join(distro).join(scenario);
        assert!(
            fixtures.is_dir(),
            "the recordings {} are not there",
            fixtures.display()
        );
        let root = Root::new(&format!("import-files-{name}"));
        root.arm();
        root.write(
            "etc/os-release",
            &fs::read_to_string(fixtures_root().join(distro).join("os-release"))
                .expect("the os-release recording"),
        );
        root.write("etc/hostname", &format!("{HOST_NAME}\n"));
        root.write("var/lib/apt/lists/recorded_Packages", "recorded index\n");
        root.write("var/lib/pacman/sync/core.db", "recorded database\n");
        fs::write(state.join("fixtures"), fixtures.display().to_string()).expect("the selection");
        let _ = fs::remove_file(state.join("log"));
        Case {
            root,
            state,
            _serial: serial,
        }
    }

    /// Give the root a `passwd` and a `group` that name the invoking user's own ids. Every file
    /// this test creates is owned by them, because an unprivileged process cannot create a file
    /// owned by anyone else.
    fn name_the_invoker(&self) -> &Case {
        self.root.write(
            "etc/passwd",
            &format!(
                "root:x:0:0:root:/root:/bin/sh\n{OWNER}:x:{}:{}::/nonexistent:/bin/sh\n",
                uid(),
                gid()
            ),
        );
        self.root
            .write("etc/group", &format!("root:x:0:\n{GROUP}:x:{}:\n", gid()));
        self
    }

    fn gate(&self) -> Gate {
        Gate::open(&self.root.options(), Operation::Plan).expect("the scratch root is armed")
    }

    fn capture(&self) -> Capture {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        files::capture(&self.gate()).expect("a refusal is never an error")
    }

    /// Every argv the shims saw, one line per invocation, in order.
    fn log(&self) -> Vec<String> {
        let bytes = fs::read(self.state.join("log")).unwrap_or_default();
        String::from_utf8_lossy(&bytes)
            .split('\u{1e}')
            .filter(|record| !record.is_empty())
            .map(|record| {
                record
                    .split('\u{1f}')
                    .collect::<Vec<&str>>()
                    .join(" ")
                    .trim()
                    .to_string()
            })
            .collect()
    }
}

fn uid() -> u32 {
    fs::metadata(env!("CARGO_MANIFEST_DIR"))
        .expect("the checkout")
        .uid()
}

fn gid() -> u32 {
    fs::metadata(env!("CARGO_MANIFEST_DIR"))
        .expect("the checkout")
        .gid()
}

/// The eight configuration files of acceptance (a), written into the root as real files: before
/// si-1 two were captured and five refused by the policy's gates; now each changed one is named.
fn write_the_eight(case: &Case) {
    // (1) and (2): modified, world readable, small — the two the capture takes.
    case.root
        .write("etc/sysctl.d/99-local.conf", "vm.swappiness = 10\n")
        .chmod("etc/sysctl.d/99-local.conf", 0o644);
    case.root
        .write("etc/ssh/sshd_config", "PermitRootLogin no\nPort 2222\n")
        .chmod("etc/ssh/sshd_config", 0o600);
    // sshd_config is captured, so it must be world readable: 0644 it deliberately.
    case.root.chmod("etc/ssh/sshd_config", 0o644);
    // (3): unmodified — its bytes hash to the digest the recording carries, so the package
    // manager reports no change and it is not a candidate at all.
    case.root
        .write("etc/default/keyboard", SHIPPED_KEYBOARD)
        .chmod("etc/default/keyboard", 0o644);
    // (4): a mode that denies read to other.
    case.root
        .write("etc/nslcd.conf", "uri ldap://directory\nbinddn cn=lodi\n")
        .chmod("etc/nslcd.conf", 0o600);
    // (5): a private-key marker in the content.
    case.root
        .write(
            "etc/openvpn/client.conf",
            "client\n<key>\n-----BEGIN PRIVATE KEY-----\nnot-a-real-key\n-----END PRIVATE KEY-----\n</key>\n",
        )
        .chmod("etc/openvpn/client.conf", 0o644);
    // (6): 100 KiB, over the per-file cap.
    let big = "# a very long certificate list\n".repeat(100 * 1024 / 30 + 1);
    case.root
        .write("etc/ca-certificates.conf", &big)
        .chmod("etc/ca-certificates.conf", 0o644);
    // (7): a path whose final component is a symbolic link.
    case.root.write("run/resolv.conf", "nameserver 192.0.2.1\n");
    let link = case.root.path("etc/resolv.conf");
    fs::create_dir_all(link.parent().expect("a parent")).expect("etc");
    symlink("../run/resolv.conf", &link).expect("the symbolic link");
}

// ------------------------------------------------------------------ acceptance (a) ---

/// Every file the package manager reports as changed is named with the `[etc."PATH"]` that
/// declares it, none is captured, the unmodified one is counted, and each prints exactly one
/// `W_UNCAPTURED` line.
#[test]
fn the_root_of_eight_names_every_changed_file_and_copies_none() {
    let case = Case::new("eight", "debian-12", "capture");
    case.name_the_invoker();
    write_the_eight(&case);
    let capture = case.capture();
    assert!(capture.captured.is_empty(), "{:?}", capture.captured);
    assert_eq!(capture.unchanged, 1, "the unmodified conffile is counted");
    let named: Vec<&str> = capture.refused.iter().map(|r| r.path.as_str()).collect();
    assert_eq!(
        named,
        vec![
            "/etc/ca-certificates.conf",
            "/etc/nslcd.conf",
            "/etc/openvpn/client.conf",
            "/etc/resolv.conf",
            "/etc/ssh/sshd_config",
            "/etc/sysctl.d/99-local.conf",
        ]
    );
    for refusal in &capture.refused {
        assert_eq!(refusal.reason, Reason::Changed, "{}", refusal.path);
        let table = refusal.reason.table(&refusal.path).expect("a table");
        assert_eq!(
            table,
            format!("[etc.\"{}\"]", &refusal.path["/etc/".len()..])
        );
    }
    // No reason names a path of the machine the capture ran on (`AGENTS.md` §1.2).
    let here = case.root.dir.display().to_string();
    for refusal in &capture.refused {
        assert!(!refusal.why().contains(&here), "{}", refusal.why());
    }
    assert_warnings_match_refusals(&capture);
}

fn assert_warnings_match_refusals(capture: &Capture) {
    let warnings = capture.warnings();
    assert_eq!(warnings.len(), capture.refused.len());
    for (line, refusal) in warnings.iter().zip(&capture.refused) {
        assert!(line.starts_with("W_UNCAPTURED: "), "{line}");
        assert!(line.contains(&refusal.path), "{line}");
        assert!(line.ends_with(&refusal.why()), "{line}");
        assert_eq!(line.matches("W_UNCAPTURED").count(), 1, "{line}");
    }
}

/// The hard path list is not overridable and is applied whatever a file's mode, size or content
/// is — and a candidate outside `/etc` is refused for being outside `/etc`.
#[test]
fn the_hard_path_list_refuses_every_name_on_it() {
    let case = Case::new("refusals", "debian-12", "refusals");
    case.name_the_invoker();
    let listed = [
        "etc/apt/auth.conf",
        "etc/apt/auth.conf.d/90local",
        "etc/crypttab",
        "etc/fstab",
        "etc/group",
        "etc/gshadow",
        "etc/hostname",
        "etc/krb5.keytab",
        "etc/machine-id",
        "etc/netrc",
        "etc/NetworkManager/system-connections/home.nmconnection",
        "etc/pacman.d/gnupg/pubring.conf",
        "etc/pacman.d/mirrorlist",
        "etc/passwd",
        "etc/pki/client.p12",
        "etc/shadow",
        "etc/ssh/ssh_host_ed25519_key",
        "etc/ssl/private/site.conf",
        "etc/subgid",
        "etc/subuid",
        "etc/sudoers.d/90-local",
        "etc/wpa_supplicant/wpa_supplicant.conf",
    ];
    for path in listed.iter().chain(&["usr/local/etc/outside.conf"]) {
        case.root
            .write(path, "harmless = true\n")
            .chmod(path, 0o644);
    }
    // `etc/passwd` and `etc/group` were just overwritten; put the real ones back.
    case.name_the_invoker();
    let capture = case.capture();
    assert!(capture.captured.is_empty());
    for path in listed {
        let path = format!("/{path}");
        assert_eq!(
            reason(&capture, &path),
            Reason::NeverCaptured,
            "{path} is not refused by the hard list"
        );
        assert_eq!(Reason::NeverCaptured.table(&path), None);
    }
    assert_eq!(
        reason(&capture, "/usr/local/etc/outside.conf"),
        Reason::OutsideEtc
    );
    for path in [
        // check-host-safety: refusal — a string this rule refuses, in the test that proves it.
        "/etc/lodi/host.toml",
        // check-host-safety: refusal — the same, for the scope's other state file.
        "/etc/lodi/host.lock",
    ] {
        assert!(
            files::never_captured(path),
            "{path} must be on the hard list"
        );
    }
    // dpkg's own `obsolete` marker: an entry no installed package owns any more is not offered.
    assert!(
        !capture
            .refused
            .iter()
            .any(|r| r.path == "/etc/apt/apt.conf.d/99obsolete"),
        "an obsolete conffile is not a candidate"
    );
}

/// A file that holds a login credential by its format, a mirror list whose server URLs may carry
/// one, and a privilege grant to named users are never read, whatever their mode or content.
#[test]
fn netrc_the_mirror_list_and_sudoers_drop_ins_are_never_captured() {
    let case = Case::new("never-credentials", "debian-12", "refusals");
    case.name_the_invoker();
    for rel in [
        "etc/netrc",
        "etc/pacman.d/mirrorlist",
        "etc/sudoers.d/90-local",
    ] {
        case.root.write(rel, "harmless = true\n").chmod(rel, 0o644);
    }
    let capture = case.capture();
    for path in [
        "/etc/netrc",
        "/etc/pacman.d/mirrorlist",
        "/etc/sudoers.d/90-local",
    ] {
        assert!(
            files::never_captured(path),
            "{path} is not on the hard list"
        );
        assert_eq!(reason(&capture, path), Reason::NeverCaptured);
    }
    // The directory rule is a prefix: any drop-in below it is refused, and the file beside it
    // that the directory is named after is not claimed by it.
    assert!(files::never_captured("/etc/sudoers.d/README"));
    assert!(!files::never_captured("/etc/sudoers.dist"));
}

/// A credential in every shape people write one, each in its own world-readable file the package
/// manager reports as changed: each file is named, none is captured, and no fake value reaches
/// the emitted manifest, its comment block or a warning.
#[test]
fn a_credential_in_any_shape_never_leaves_the_file() {
    let case = Case::new("credentials", "debian-12", "credentials");
    case.name_the_invoker();
    let shapes = credentials::shapes();
    for (index, shape) in shapes.iter().enumerate() {
        let rel = format!("etc/credential-shapes/{index:02}.conf");
        case.root.write(&rel, &shape.text).chmod(&rel, 0o644);
    }
    let benign = "etc/credential-shapes/benign.conf";
    case.root
        .write(benign, credentials::BENIGN)
        .chmod(benign, 0o644);
    let capture = case.capture();
    assert!(capture.captured.is_empty());
    assert_eq!(
        capture.refused.len(),
        shapes.len() + 1,
        "{:?}",
        capture.refused
    );
    let emitted = emit(&case, &capture);
    let warnings = capture.warnings().join("\n");
    for shape in &shapes {
        for text in [&emitted, &warnings] {
            assert!(
                !text.contains(&shape.fake),
                "{}: the fake value left the file",
                shape.name
            );
        }
    }
}

/// A path on the hard list is never opened, not even to compare it with the digest its package
/// recorded: the kernel's own inotify sees no open of any of them while the capture runs.
#[test]
fn nothing_on_the_hard_list_is_opened() {
    let case = Case::new("unopened", "debian-12", "refusals");
    case.name_the_invoker();
    let secrets = [
        "etc/gshadow",
        "etc/shadow",
        "etc/ssh/ssh_host_ed25519_key",
        "etc/ssl/private/site.conf",
        "etc/krb5.keytab",
    ];
    for path in secrets {
        case.root.write(path, "secret-bytes\n").chmod(path, 0o644);
    }
    let watch = Opened::watch(&case.root.dir, &secrets);
    let capture = case.capture();
    assert_eq!(watch.seen(), Vec::<String>::new());
    for path in secrets {
        assert_eq!(reason(&capture, &format!("/{path}")), Reason::NeverCaptured);
    }
}

/// A watch on each path for an open, through inotify: whoever opened one shows as `IN_OPEN`.
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
            let full = std::ffi::CString::new(std::os::unix::ffi::OsStrExt::as_bytes(
                root.join(path).as_os_str(),
            ))
            .unwrap();
            let wd = unsafe { libc::inotify_add_watch(fd, full.as_ptr(), libc::IN_OPEN) };
            assert!(wd >= 0, "a watch on {path}");
            watched.insert(wd, (*path).to_string());
        }
        Opened { fd, watched }
    }

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

// ------------------------------------------------------------------ acceptance (e) ---

/// (e) A refusal is a warning and never an error: every case returns a [`Capture`], and a root
/// whose every candidate is refused still emits a manifest that parses, with no `[files]`.
#[test]
fn every_refusal_case_succeeds() {
    for (name, scenario, prepare) in [
        ("ok-eight", "capture", true),
        ("ok-hard", "refusals", false),
    ] {
        let case = Case::new(name, "debian-12", scenario);
        case.name_the_invoker();
        if prepare {
            write_the_eight(&case);
        }
        let capture = {
            let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
            files::capture(&case.gate())
        };
        assert!(
            capture.is_ok(),
            "{name}: a refusal became an error: {:?}",
            capture.err()
        );
    }
    let case = Case::new("all-refused", "debian-12", "refusals");
    case.name_the_invoker();
    let capture = case.capture();
    assert!(capture.captured.is_empty() && !capture.refused.is_empty());
    let emitted = emit(&case, &capture);
    assert!(!emitted.contains("[files."));
    let ctx = manifest::Context::from_gate(&case.gate());
    manifest::parse(&emitted, "host.toml", &ctx).expect("it still parses");
}

// ------------------------------------------------------------------ the comment block ---

/// Every changed file is named in the emitted comment block with the `[etc."PATH"]` that
/// declares it, and the two counts say what the capture never looked at.
#[test]
fn the_comment_block_names_every_changed_file_and_counts_what_was_not_looked_at() {
    let case = Case::new("comment", "debian-12", "capture");
    case.name_the_invoker();
    write_the_eight(&case);
    let capture = case.capture();
    let emitted = emit(&case, &capture);
    for refusal in &capture.refused {
        let table = refusal.reason.table(&refusal.path).expect("a changed file");
        assert!(
            emitted.contains(&format!(
                "#   {}\n#     declare its text as {table}\n",
                refusal.path
            )),
            "{} is not named in the comment block:\n{emitted}",
            refusal.path
        );
    }
    assert!(
        emitted.contains(
            "# configuration files the package manager tracks and reports unchanged: 1\n"
        )
    );
    // The census: every regular file under the root's own etc/ that the recording does not
    // track, less Lodi's own etc/lodi (LD-378): os-release, hostname, passwd and group.
    assert_eq!(capture.untracked, 4, "the /etc census");
    assert!(!capture.untracked_is_a_floor);
    assert!(emitted.contains("# files under /etc that no package tracks as configuration: 4\n"));
    // No byte of a changed file is in the manifest.
    for text in [
        "vm.swappiness",
        "PermitRootLogin",
        "binddn",
        "PRIVATE KEY",
        "nameserver",
    ] {
        assert!(!emitted.contains(text), "{text} left the machine");
    }
    // The host name is written once, as the [system] hostname (si-1), and nowhere else.
    let hostname = format!("hostname = \"{HOST_NAME}\"\n");
    assert_eq!(emitted.matches(&hostname).count(), 1, "{emitted}");
    assert!(!emitted.replace(&hostname, "").contains(HOST_NAME));
    assert!(!emitted.contains("/home/"));
    assert!(!emitted.contains('@'));
}

// ------------------------------------------------------------------ the families ---

/// pacman answers the question itself: `-Qii` prints `MODIFIED` or `UNMODIFIED` beside each
/// backup path, and `UNMODIFIED` is never read as a suffix of `MODIFIED`.
#[test]
fn pacman_reads_its_own_verdict_and_the_capture_honours_it() {
    let case = Case::new("arch", "arch", "capture");
    case.name_the_invoker();
    for (path, body) in [
        ("etc/makepkg.conf", "MAKEFLAGS=\"-j8\"\n"),
        ("etc/ssh/sshd_config", "PermitRootLogin no\n"),
        ("etc/pacman.conf", "[options]\n"),
        ("etc/ssh/ssh_config", "Host *\n"),
        ("etc/shadow", "root:!:20000:0:99999:7:::\n"),
        ("etc/fstab", "# /etc/fstab\n"),
    ] {
        case.root.write(path, body).chmod(path, 0o644);
    }
    let gate = case.gate();
    let backend = pm::backend_for(gate.distro, &gate.root, false, gate.operation);
    let tracked = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        backend.conffiles().expect("pacman -Qii is read")
    };
    let verdicts: BTreeMap<&str, ConffileStatus> = tracked
        .iter()
        .map(|c| (c.path.as_str(), c.status))
        .collect();
    assert_eq!(verdicts["/etc/makepkg.conf"], ConffileStatus::Modified);
    assert_eq!(verdicts["/etc/pacman.conf"], ConffileStatus::Unmodified);
    assert_eq!(verdicts["/etc/ssh/ssh_config"], ConffileStatus::Unmodified);
    assert_eq!(verdicts["/etc/ssh/sshd_config"], ConffileStatus::Modified);

    let capture = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        files::capture_with(&gate, backend.as_ref()).expect("a refusal is not an error")
    };
    assert!(capture.captured.is_empty());
    assert_eq!(reason(&capture, "/etc/makepkg.conf"), Reason::Changed);
    assert_eq!(reason(&capture, "/etc/ssh/sshd_config"), Reason::Changed);
    assert_eq!(capture.unchanged, 2);
    assert_eq!(reason(&capture, "/etc/shadow"), Reason::NeverCaptured);
    assert_eq!(reason(&capture, "/etc/fstab"), Reason::NeverCaptured);
    // `Backup Files : None` contributes no row.
    assert!(!capture.refused.iter().any(|r| r.path == "None"));
}

/// Nothing this module runs mutates a machine: the whole shim argv log holds only reads, and a
/// full census of the scratch root is identical before and after.
#[test]
fn the_capture_runs_only_read_commands_and_writes_nothing_below_the_root() {
    const READS: &[&str] = &[
        "apt-cache policy",
        "apt-mark showauto",
        "apt-mark showhold",
        "dpkg-query -W",
        "pacman -Q",
        "pacman -Qe",
        "pacman -Qi",
        "pacman -Qii",
        "pacman -Qqem",
        "pacman -Si",
    ];
    const MUTATIONS: &[&str] = &[
        "--asdeps",
        "--asexplicit",
        "--noconfirm",
        "-Rns",
        "-S",
        "-Syu",
        "-U",
        "auto",
        "hold",
        "install",
        "manual",
        "remove",
        "unhold",
        "update",
        "upgrade",
    ];
    let case = Case::new("readonly", "debian-12", "capture");
    case.name_the_invoker();
    write_the_eight(&case);
    let before = census(&case.root.dir);
    let capture = case.capture();
    assert!(!capture.refused.is_empty());
    let after = census(&case.root.dir);
    assert_eq!(
        before, after,
        "the capture changed something below the root"
    );
    let log = case.log();
    assert!(!log.is_empty(), "the shims were reached at all");
    for line in &log {
        let words: Vec<&str> = line.split(' ').collect();
        let head = format!("{} {}", words[0], words.get(1).copied().unwrap_or(""));
        assert!(
            READS.contains(&head.trim()),
            "the capture ran something that is not a read: {line}"
        );
        for word in &words[1..] {
            assert!(
                !MUTATIONS.contains(word),
                "a mutating word reached the package manager: {line}"
            );
        }
    }
    assert!(
        log.iter().any(|line| line.contains("Conffiles")),
        "the conffile list really was read: {log:?}"
    );
}

// ------------------------------------------------------------------ the digest ---

/// dpkg records its conffile digests as MD5, so the comparison is made in MD5. The
/// implementation is checked against the test vectors RFC 1321 §A.5 publishes.
#[test]
fn the_dpkg_digest_matches_the_published_test_vectors() {
    for (input, expected) in [
        ("", "d41d8cd98f00b204e9800998ecf8427e"),
        ("a", "0cc175b9c0f1b6a831c399e269772661"),
        ("abc", "900150983cd24fb0d6963f7d28e17f72"),
        ("message digest", "f96b697d7cb7938d525a2f31aaf161d0"),
        (
            "abcdefghijklmnopqrstuvwxyz",
            "c3fcd3d76192e4007dfb496cca67e13b",
        ),
        (
            "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
            "d174ab98d277d9f5a5611c2c9f419d9f",
        ),
        (
            "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
            "57edf4a22be3c955ac49da2e2107b67a",
        ),
    ] {
        assert_eq!(
            lodi::hostscope::pm::apt::md5_hex(input.as_bytes()),
            expected,
            "RFC 1321 vector {input:?}"
        );
    }
    // The digest the `capture` recording carries for the unmodified conffile really is the one
    // of the bytes this test writes; without that the "unmodified" case would prove nothing.
    let recorded = fs::read_to_string(fixtures_root().join("debian-12/capture/conffiles.txt"))
        .expect("the recording");
    let line = recorded
        .lines()
        .find(|line| line.contains("/etc/default/keyboard"))
        .expect("the row");
    assert!(
        line.contains(&lodi::hostscope::pm::apt::md5_hex(
            SHIPPED_KEYBOARD.as_bytes()
        )),
        "the recording's digest is not the digest of the bytes the test writes: {line}"
    );
}

// ------------------------------------------------------------------ helpers ---

/// The manifest an import writes from this root and this capture.
fn emit(case: &Case, capture: &Capture) -> String {
    let machine = {
        let _spawning = SHIMS.read().unwrap_or_else(|e| e.into_inner());
        lodi::hostscope::import::read(&case.gate()).expect("the recordings")
    };
    let snapshot = lodi::hostscope::import::Snapshot::at(
        lodi::util::parse_utc(CAPTURED_AT).expect("a timestamp"),
    );
    lodi::hostscope::import::emit::manifest(&machine, &snapshot, Some(capture))
}

fn reason(capture: &Capture, path: &str) -> Reason {
    capture
        .refused
        .iter()
        .find(|r| r.path == path)
        .unwrap_or_else(|| {
            panic!(
                "{path} was not named; the names were {:?}",
                capture.refused.iter().map(|r| &r.path).collect::<Vec<_>>()
            )
        })
        .reason
        .clone()
}

/// A full mode, size and nanosecond-mtime census of a tree, for the before-and-after assertion.
fn census(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            out.push(format!(
                "{} {:o} {} {}.{}",
                path.display(),
                meta.permissions().mode(),
                meta.len(),
                meta.mtime(),
                meta.mtime_nsec()
            ));
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    out.sort();
    out
}
