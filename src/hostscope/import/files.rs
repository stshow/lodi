//! What an import says about `/etc`: which configuration files changed, and nothing more
//! (M-Import T-2; si-1, LD-421).
//!
//! # Candidates come from the package manager, never from a walk of `/etc`
//!
//! The only files named are the ones the package manager itself tracks *and* reports as
//! changed: a dpkg conffile whose recorded digest no longer matches the file on disk, a pacman
//! backup file `pacman -Qii` calls `MODIFIED`, or what `rpm -Va` reports. A file no package
//! tracks is not a candidate at all (design call D9, LD-277), and its count is stated in the
//! comment block so that nothing is silently left out.
//!
//! # No file is copied (si-1)
//!
//! Since 1.10 the import copies no file from `/etc`: every changed file is named under
//! `NOT CAPTURED` with the `[etc."PATH"]` a person writes to declare the text they want there.
//! A path on the hard list of [`NEVER_CAPTURED`], its prefixes and its endings is named as never
//! read, and no function of the import opens it: the dpkg digest comparison skips it too.
//!
//! # What this module does not do
//!
//! It reads no configuration file, writes nothing below the root — [`Capture::write_bundle`]
//! writes only the keyrings a `[sources]` block names, under the directory it is handed — runs
//! no process of its own and follows no symbolic link.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

use super::super::pm::{self, ConffileStatus};
use super::super::safety::Gate;

/// The warning every refusal prints, once, on standard error. It is a warning of
/// `docs/ERRORS.md`'s warnings paragraph and never a row of its code table: a refusal does not
/// change an exit status, and an import that refuses every file it was offered still exits 0.
pub const W_UNCAPTURED: &str = "W_UNCAPTURED";

/// The one directory a host capture looks in. A candidate outside it is refused whatever the
/// package manager says about it.
pub const ETC: &str = "/etc/";

/// How many entries the `/etc` census visits before it stops counting. It exists so that a
/// pathological tree cannot turn a count into unbounded work; a root that reaches it is
/// reported as "at least this many", never as a smaller number.
pub const MAX_CENSUS_ENTRIES: usize = 20_000;

/// Paths that are never captured, whatever their mode, size or content, and whatever flag is
/// given: this list has no override and this build offers none (design call D7, LD-275).
/// `/etc/netrc` holds login credentials by its format, and a mirror list's server URLs may
/// carry them; neither is read even when its mode lets every user read it.
pub const NEVER_CAPTURED: &[&str] = &[
    "/etc/crypttab",
    "/etc/fstab",
    "/etc/group",
    "/etc/group-",
    "/etc/gshadow",
    "/etc/gshadow-",
    "/etc/hostname",
    "/etc/krb5.keytab",
    "/etc/machine-id",
    "/etc/netrc",
    "/etc/pacman.d/mirrorlist",
    "/etc/passwd",
    "/etc/passwd-",
    "/etc/shadow",
    "/etc/shadow-",
    "/etc/subgid",
    "/etc/subuid",
    "/etc/sudoers",
    "/etc/apt/auth.conf",
];

/// Directories nothing is ever captured from, stated as the prefix a path must not begin with.
/// `/etc/sudoers.d/` grants privilege to named users, which is an identity of the machine read.
pub const NEVER_CAPTURED_UNDER: &[&str] = &[
    "/etc/NetworkManager/system-connections/",
    "/etc/apt/auth.conf.d/",
    "/etc/lodi/",
    "/etc/pacman.d/gnupg/",
    "/etc/ssl/private/",
    "/etc/sudoers.d/",
    "/etc/wpa_supplicant/",
];

/// Endings that name a key or a certificate store wherever they appear.
pub const NEVER_CAPTURED_ENDING: &[&str] = &[".key", ".keytab", ".p12", ".pem", ".pfx"];

/// An ssh **host** key: `/etc/ssh/*_key`. It is stated as a directory and an ending rather than
/// as a pattern, because there are no regular expressions anywhere in this tree (LD-80).
pub const SSH_KEYS_UNDER: &str = "/etc/ssh/";
/// The ending of an ssh host key's private half.
pub const SSH_KEY_ENDING: &str = "_key";

/// One configuration file the capture took, with everything the emitted declaration needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The absolute path on the machine that was read, exactly as declared.
    pub path: String,
    /// Where the bytes travel, relative to the manifest's own directory: the path without its
    /// leading slash, under `files/`.
    pub source: String,
    pub mode: u32,
    pub owner: String,
    pub group: String,
    /// The bytes as found. Nothing is rewritten, reformatted or redacted.
    pub bytes: Vec<u8>,
}

/// One candidate the policy turned away, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    pub path: String,
    pub reason: Reason,
}

impl Refused {
    /// The one line this refusal prints on standard error.
    pub fn warning(&self) -> String {
        format!(
            "{W_UNCAPTURED}: {} is not captured: {}",
            self.path,
            self.why()
        )
    }

    /// The reason in the words the emitted comment block and the warning both use.
    pub fn why(&self) -> String {
        self.reason.why()
    }
}

/// Why one candidate was not captured. Every variant is a refusal, never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    /// The candidate is not under `/etc`.
    OutsideEtc,
    /// The candidate is on the hard list, which no flag overrides: it is never opened.
    NeverCaptured,
    /// The package manager reports it changed; the import copies no file from `/etc` (si-1).
    Changed,
}

impl Reason {
    pub fn why(&self) -> String {
        match self {
            Reason::OutsideEtc => {
                "a host capture takes configuration from /etc and nowhere else".to_string()
            }
            Reason::NeverCaptured => "it is on lodi's list of paths that are never read, \
                 because it carries an identity, a credential, a privilege or lodi's own state"
                .to_string(),
            Reason::Changed => "lodi copies no file from /etc; declare the text you want there \
                 as [etc.\"PATH\"]"
                .to_string(),
        }
    }

    /// The `[etc."PATH"]` table a person writes for this path, when one may be written.
    pub fn table(&self, path: &str) -> Option<String> {
        match self {
            Reason::Changed => path
                .strip_prefix(ETC)
                .map(|rest| format!("[etc.\"{rest}\"]")),
            _ => None,
        }
    }
}

/// Everything one capture found: what it took, what it refused, and the two counts that keep
/// the emitted file honest about what it never looked at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Capture {
    /// Sorted by path, which is the order they are declared and written in.
    pub captured: Vec<Captured>,
    /// Sorted by path.
    pub refused: Vec<Refused>,
    /// Files the package manager tracks and reports unchanged. They are not candidates and are
    /// not named one by one: a machine has hundreds of them and the emitted file is meant to be
    /// short. The count says they were considered.
    pub unchanged: usize,
    /// Regular files under `<root>/etc` that no package tracks as a configuration file. They
    /// are never candidates in this version (design call D9, LD-277); the count says how much
    /// of `/etc` the capture deliberately did not look at.
    pub untracked: usize,
    /// Whether the census above reached [`MAX_CENSUS_ENTRIES`] and stopped early.
    pub untracked_is_a_floor: bool,
}

impl Capture {
    /// One `W_UNCAPTURED` line per refusal, in path order, for standard error.
    pub fn warnings(&self) -> Vec<String> {
        self.refused.iter().map(Refused::warning).collect()
    }

    /// Write the bundle under `out`: one file per captured path, at the `source` the declaration
    /// names, with the mode it was found with.
    ///
    /// Nothing below the machine's root is touched. Directories are made at 0755 one checked
    /// component at a time, a file is created fresh with the mode it was captured with, and an
    /// existing regular file at the same place is replaced by a rename, so that the path is
    /// never absent while a manifest beside it names it (LD-325) — `out` is a directory the
    /// caller has already decided may be written (design call D6, LD-274). No symbolic link below
    /// `out` is followed, whether as a directory on the way or as the file itself: a link
    /// planted there before the import is `E_PATH_ESCAPE`, and whatever it points at is left
    /// exactly as it was (M-1.0 S-1).
    pub fn write_bundle(&self, out: &Path) -> Result<(), Diagnostic> {
        use crate::hostscope::files as boundary;

        for file in &self.captured {
            let shown = format!("/{}", file.source);
            let target = boundary::ensure_parent(out, &shown, 0o755)?;
            match fs::symlink_metadata(&target) {
                // `ensure_parent` has already refused a link; what is left is replaced below.
                Ok(meta) if meta.is_file() => {}
                Ok(_) => {
                    return Err(Diagnostic::new(
                        "E_PATH_ESCAPE",
                        format!(
                            "{}: the final component is not a regular file",
                            target.display()
                        ),
                    )
                    .hint("lodi never follows a symbolic link in a host-scope destination"));
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(&target, &error.to_string())),
            }
            // A fresh sibling, never through a link, then a rename over the target.
            let temp = target.with_file_name(format!(
                ".{}.lodi-import-{}",
                target
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                std::process::id()
            ));
            let written = (|| -> std::io::Result<()> {
                let mut handle = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .mode(0o600)
                    .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                    .open(&temp)?;
                handle.write_all(&file.bytes)?;
                // The umask has been applied by the open. The declared `mode` is the captured one;
                // the copy itself is never writable by group or others, because a host directory
                // holding such a file is not applied (LD-379).
                handle.set_permissions(fs::Permissions::from_mode(file.mode & !0o022))?;
                handle.sync_all()?;
                crate::util::keep_label(&target, &handle)?;
                drop(handle);
                fs::rename(&temp, &target)
            })();
            if let Err(e) = written {
                let _ = fs::remove_file(&temp);
                return Err(io(&target, &e.to_string()));
            }
        }
        Ok(())
    }
}

fn io(path: &Path, detail: &str) -> Diagnostic {
    Diagnostic::new("E_STORE_IO", format!("{}: {detail}", path.display()))
}

/// Name the configuration files of the machine the gate opened that its package manager reports
/// as changed. No file is read: each is a [`Refused`] with its reason.
///
/// The only failure is a failure to read the package manager.
pub fn capture(gate: &Gate) -> Result<Capture, Diagnostic> {
    let backend = pm::backend_for(gate.distro, &gate.root, false, gate.operation);
    capture_with(gate, backend.as_ref())
}

/// The same, over a backend the caller already has — what the tests drive.
pub fn capture_with(gate: &Gate, backend: &dyn pm::Backend) -> Result<Capture, Diagnostic> {
    let tracked = backend.conffiles()?;
    let mut out = Capture::default();
    let mut names: BTreeSet<String> = BTreeSet::new();
    let mut candidates: Vec<String> = Vec::new();
    for file in &tracked {
        names.insert(file.path.clone());
        match file.status {
            ConffileStatus::Unmodified => out.unchanged += 1,
            // An undetermined file is named on purpose: the family declined to open it.
            ConffileStatus::Modified | ConffileStatus::Undetermined => {
                candidates.push(file.path.clone());
            }
        }
    }
    candidates.sort();
    candidates.dedup();
    for path in candidates {
        let reason = consider(gate, &path);
        out.refused.push(Refused { path, reason });
    }
    let (untracked, floored) = census(&gate.root, &names);
    out.untracked = untracked;
    out.untracked_is_a_floor = floored;
    Ok(out)
}

/// Why one changed file is not captured, from its path alone.
fn consider(gate: &Gate, path: &str) -> Reason {
    if !path.starts_with(ETC) {
        return Reason::OutsideEtc;
    }
    if never_captured(path)
        || (gate.distro == super::super::safety::Distro::Fedora
            && super::super::pm::dnf::is_setting(path))
    {
        return Reason::NeverCaptured;
    }
    Reason::Changed
}

/// Is this path one of the ones lodi never captures?
pub fn never_captured(path: &str) -> bool {
    if NEVER_CAPTURED.contains(&path) {
        return true;
    }
    if NEVER_CAPTURED_UNDER
        .iter()
        .any(|prefix| path.starts_with(prefix))
    {
        return true;
    }
    if NEVER_CAPTURED_ENDING
        .iter()
        .any(|ending| path.ends_with(ending))
    {
        return true;
    }
    path.starts_with(SSH_KEYS_UNDER) && path.ends_with(SSH_KEY_ENDING)
}

/// How many regular files under `<root>/etc` no package tracks.
///
/// It counts and opens nothing: `symlink_metadata` on each entry, no symbolic link followed and
/// no directory below one descended into. The walk is bounded by [`MAX_CENSUS_ENTRIES`], and the
/// second half of the answer says whether it stopped there.
fn census(root: &Path, tracked: &BTreeSet<String>) -> (usize, bool) {
    let etc = root.join("etc");
    let mut stack: Vec<PathBuf> = vec![etc.clone()];
    let mut visited = 0usize;
    let mut untracked = 0usize;
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            visited += 1;
            if visited > MAX_CENSUS_ENTRIES {
                return (untracked, true);
            }
            let path = entry.path();
            let Ok(meta) = fs::symlink_metadata(&path) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                // `/etc/lodi` is Lodi's own — its marker, its manifest, its record and the bundle
                // beside them — and never a configuration file anybody chose. Counting it would
                // make the block of every reconcile differ from the one before it by what Lodi
                // itself wrote (LD-378).
                if path != etc.join("lodi") {
                    stack.push(path);
                }
                continue;
            }
            if !meta.is_file() {
                continue;
            }
            let Ok(relative) = path.strip_prefix(root) else {
                continue;
            };
            let shown = format!("/{}", relative.display());
            if !tracked.contains(&shown) {
                untracked += 1;
            }
        }
    }
    (untracked, false)
}
