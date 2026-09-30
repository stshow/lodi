//! Where `lodi host import` puts what it read (the owner's decision of 2026-09-22, LD-325).
//!
//! With no flag the import **lands in place**: the manifest at `<root>/etc/lodi/host.toml`, at
//! 0644 and owned by whoever ran it, and the captured bytes under `<root>/etc/lodi/files/`, so
//! that every `source` resolves where `lodi host plan` reads it. `--out DIR` writes the same
//! bundle to a directory of the operator's instead, and `--stdout` prints the manifest alone and
//! captures nothing. Every decision here that can refuse is taken **before the machine is read**:
//!
//! 1. **privilege** — landing on `/` writes root-owned files, so it needs euid 0
//!    (`E_NEED_ROOT`, exit 9). That decision is [`may_land_in_place`], a pure function of the
//!    root and the effective uid, so it is proved without ever pointing the command at `/`;
//! 2. **no symbolic link** at `etc`, `etc/lodi`, `etc/lodi/host.toml` or `etc/lodi/files`
//!    (`E_PATH_ESCAPE`, exit 3), and `files` is a directory when it is there;
//! 3. an existing manifest without `--force` is **reconciled** ([`super::reconcile`], LD-378),
//!    which superseded that case's `E_EXISTS`; a reconcile that writes on `/` needs root for the
//!    same reason an import does, and for one more: the base it merges from lives in the
//!    root-owned lock ([`may_reconcile_in_place`]).
//!
//! # Replacing, and why a manifest's sources never go missing
//!
//! The bundle is written first and the manifest last. Each captured file replaces its old copy
//! by a rename, so a path the old manifest names is never absent; the manifest is replaced by a
//! rename too; and only then, under `--force`, is every file below `files/` that the new
//! manifest does not name removed. At every instant the manifest on disk — old or new — finds
//! every `source` it declares, and a failure at any step leaves one of the two whole.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

use super::files;
use super::import::files::Capture;
use super::plan::{Content, Desired};
use super::safety::Gate;

/// The directory an import lands in, inside the root.
pub const IN_PLACE: &str = "/etc/lodi";
/// The manifest's name inside a bundle.
pub const MANIFEST: &str = "host.toml";
/// The directory inside a bundle that holds the captured bytes.
pub const FILES: &str = "files";

/// Step 1, decided without touching the filesystem: may `euid` land an import on `root`?
///
/// `system_root` is whether the root the gate resolved is the running system's own `/`. Under a
/// `--root` the invoking user writes into a tree of their own, as LD-114 says for every host
/// command.
pub fn may_land_in_place(system_root: bool, euid: u32) -> Result<(), Diagnostic> {
    if system_root && euid != 0 {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "importing into /etc/lodi writes the root-owned /etc/lodi/{MANIFEST}, and this \
                 process runs as uid {euid}"
            ),
        )
        .hint(
            "run it as root: sudo lodi host import; or print the manifest with --stdout, or \
             write the bundle to a directory of your own with --out DIR. Nothing was read",
        ));
    }
    Ok(())
}

/// Step 1 for an existing manifest reconciled on `root` (LD-378): the reconcile writes the
/// manifest and the root-owned lock the base of its merge lives in. `--dry-run` writes neither and
/// is not asked.
pub fn may_reconcile_in_place(system_root: bool, euid: u32) -> Result<(), Diagnostic> {
    if system_root && euid != 0 {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "reconciling /etc/lodi/{MANIFEST} with this machine writes it and the root-owned                  /etc/lodi/host.lock, which holds the record the merge starts from, and this                  process runs as uid {euid}"
            ),
        )
        .hint(
            "run it as root: sudo lodi host import; or see what it would change with --dry-run,              which needs no root. Nothing was read",
        ));
    }
    Ok(())
}

/// Steps 1 to 3 for the default destination. Returns the directory the bundle lands in.
pub fn in_place(gate: &Gate, force: bool, dry_run: bool) -> Result<PathBuf, Diagnostic> {
    let manifest_path = gate.root.join(&IN_PLACE[1..]).join(MANIFEST);
    let reconciling = !force && manifest_path.symlink_metadata().is_ok();
    if !dry_run {
        if reconciling {
            may_reconcile_in_place(gate.system_root, gate.euid)?;
        } else {
            may_land_in_place(gate.system_root, gate.euid)?;
        }
    }
    let manifest = format!("{IN_PLACE}/{MANIFEST}");
    let bundle = format!("{IN_PLACE}/{FILES}");
    files::inspect(&gate.root, &manifest, true)?;
    let bundle_path = files::inspect(&gate.root, &bundle, false)?;
    if let Ok(meta) = fs::symlink_metadata(&bundle_path)
        && !meta.is_dir()
    {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("{}: it is not a directory", bundle_path.display()),
        )
        .hint("lodi never follows a symbolic link in a host-scope destination"));
    }
    Ok(gate.root.join(&IN_PLACE[1..]))
}

/// Write the bundle, then the manifest, then — under `--force` — remove what the new manifest
/// no longer names. `dir` is a directory already decided on; nothing here follows a link below
/// it. Returns the manifest's path.
pub fn write(
    gate: &Gate,
    dir: &Path,
    text: &str,
    capture: Option<&Capture>,
    force: bool,
) -> Result<PathBuf, Diagnostic> {
    if let Some(capture) = capture {
        capture.write_bundle(dir)?;
    }
    let desired = Desired {
        digest: String::new(),
        mode: 0o644,
        uid: gate.euid,
        gid: gate.egid,
        owner: gate.euid.to_string(),
        group: gate.egid.to_string(),
        content: Content::Inline(text.as_bytes().to_vec()),
    };
    files::materialise(dir, &format!("/{MANIFEST}"), &desired)?;
    if force {
        let keep: BTreeSet<PathBuf> = capture
            .map(|c| c.captured.iter().map(|f| dir.join(&f.source)).collect())
            .unwrap_or_default();
        prune(&dir.join(FILES), &keep).map_err(|e| {
            Diagnostic::new("E_STORE_IO", format!("{}: {e}", dir.join(FILES).display()))
        })?;
    }
    Ok(dir.join(MANIFEST))
}

/// Remove every non-directory below `at` that is not in `keep`, then every directory left
/// empty. A symbolic link is removed as a link and never followed; `at` itself is kept.
fn prune(at: &Path, keep: &BTreeSet<PathBuf>) -> io::Result<()> {
    let entries = match fs::read_dir(at) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    for entry in entries {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            prune(&path, keep)?;
            // A directory that still holds a kept file is not empty, and stays.
            let _ = fs::remove_dir(&path);
        } else if !keep.contains(&path) {
            fs::remove_file(&path)?;
        }
    }
    files::sync_parent(&at.join("."));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_root_may_land_an_import_on_the_system_root() {
        let refused = may_land_in_place(true, 1000).unwrap_err();
        assert_eq!(refused.code, "E_NEED_ROOT");
        assert_eq!(crate::diag::exit_status(refused.code), 9);
        assert!(may_land_in_place(true, 0).is_ok());
        assert!(may_land_in_place(false, 1000).is_ok());
    }

    /// A reconcile that writes on `/` needs root, and says why: the base is in the lock.
    #[test]
    fn only_root_may_reconcile_on_the_system_root_and_the_refusal_names_the_reason() {
        let refused = may_reconcile_in_place(true, 1000).unwrap_err();
        assert_eq!(refused.code, "E_NEED_ROOT");
        assert!(refused.message.contains("host.lock"), "{}", refused.message);
        assert!(refused.message.contains("record the merge starts from"));
        assert!(
            refused.notes.iter().any(|n| n.contains("--dry-run")),
            "{:?}",
            refused.notes
        );
        assert!(may_reconcile_in_place(true, 0).is_ok());
        assert!(may_reconcile_in_place(false, 1000).is_ok());
    }
}
