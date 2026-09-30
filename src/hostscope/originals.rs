//! The originals store: where the original of a file adopted while already equal is kept
//! (LD-387).
//!
//! **Adoption** is the first time Lodi records a file it did not create: the in-place import of a
//! file it captures, a reconcile that captures a file for the first time, or the first apply that
//! finds a declared file already there with no record of it. When such a file already has the
//! declared bytes, mode, owner and group, nothing is written at its path, so its original would
//! otherwise be kept only at the first later write — after a hand edit, which would then be what a
//! removal puts back. So at adoption, when the declaration keeps a copy (`backup = true`) and
//! restores it (`on_remove = "restore"`), the file is copied here first:
//!
//! * `<root>/etc/lodi/originals/<path>.lodi-backup-<UTC>-<nanos>`, every directory below the store
//!   the applying user's and `0700`. The store is under `/etc/lodi` so that an import still
//!   changes no file outside it.
//! * The copy keeps the bytes, mode, owner and group ([`super::files::copy`]), is fsynced with its
//!   directory, and is recorded only if it has the facts the plan read. It is recorded in the
//!   existing `backup` field with `onRemove` `restore`, so the removal restores it and consumes
//!   it exactly as it does a copy beside the file. No field is added to the record.
//! * A copy that cannot be made, or does not match, leaves the record in 1.1.1's shape (no copy,
//!   `keep`), removes the copy this run made, and is said on one plain line; it fails nothing.
//! * The copy writes no managed path, so it has no journal bracket: the record is written after
//!   it is durable, and a crash between the two leaves a file here that no record names. Such a
//!   file is never read and never deleted, and a later adoption picks a name of its own.

use std::collections::BTreeMap;
use std::path::Path;

use super::lock::FileRecord;
use super::plan::Facts;

/// The store, as an in-root absolute path.
pub const STORE: &str = "/etc/lodi/originals";

/// The mode of the store and of every directory below it.
const STORE_MODE: u32 = 0o700;

/// What the adoption read of a file, which its copy must have: the digest, mode, owner and group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Planned {
    pub path: String,
    pub digest: String,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

impl Planned {
    /// The facts of a file as a plan observed them, when it observed a regular file.
    pub fn from_facts(facts: &Facts) -> Option<Planned> {
        Some(Planned {
            path: facts.path.clone(),
            digest: facts.digest.clone()?,
            mode: facts.mode?,
            uid: facts.uid?,
            gid: facts.gid?,
        })
    }

    /// The facts of a file an import captured: its bytes' digest, its mode, and the ids the
    /// root's own `passwd` and `group` give its owner and group — the names the capture wrote.
    pub fn captured(root: &Path, file: &super::import::files::Captured) -> Option<Planned> {
        Some(Planned {
            path: file.path.clone(),
            digest: format!("sha256:{}", crate::util::sha256_hex(&file.bytes)),
            mode: file.mode,
            uid: super::plan::resolve_user(root, &file.owner)?,
            gid: super::plan::resolve_group(root, &file.group)?,
        })
    }
}

/// Where the adoption copy of `path` goes: the path mirrored below the store, with the suffix a
/// copy beside the file has.
pub fn store_path(path: &str) -> String {
    format!("{STORE}{}", super::plan::backup_path(path))
}

/// Take the adoption copy of every file in `planned` whose record is still the no-copy
/// adoption (`backup` none, `onRemove` `keep`) and name it in that record with `restore`.
///
/// Returns one line for each file whose copy was not kept, naming the path and why; its record
/// is left as it was.
pub fn adopt(
    root: &Path,
    records: &mut BTreeMap<String, FileRecord>,
    planned: &[Planned],
) -> Vec<String> {
    let mut lines = Vec::new();
    for file in planned {
        let Some(record) = records.get_mut(&file.path) else {
            continue;
        };
        if record.backup.is_some() || record.on_remove != "keep" {
            continue;
        }
        match take(root, file) {
            Ok(copy) => {
                record.backup = Some(copy);
                record.on_remove = "restore".to_string();
            }
            Err(why) => lines.push(format!(
                "{}: no copy of the original was kept ({why}); it is left in place when its \
                 entry is removed",
                file.path
            )),
        }
    }
    lines
}

/// Copy one file into the store and prove the copy has the planned facts. On any failure the copy
/// this call made is removed, and the reason is returned.
pub fn take(root: &Path, file: &Planned) -> Result<String, String> {
    let copy = store_path(&file.path);
    make_dirs(root, &copy).map_err(|error| error.message)?;
    if let Err(error) = super::files::copy(root, &file.path, &copy) {
        // `copy` refuses an existing destination and writes through a temporary name, so a
        // failed copy left nothing at `copy` that this call made.
        return Err(error.message);
    }
    let made = Facts::observe(root, &copy);
    let matches = made.regular
        && made.digest.as_deref() == Some(file.digest.as_str())
        && made.mode == Some(file.mode)
        && made.uid == Some(file.uid)
        && made.gid == Some(file.gid);
    if !matches {
        let _ = super::files::remove(root, &copy);
        return Err("the copy does not match the file the plan read".to_string());
    }
    Ok(copy)
}

/// Create the store and every directory below it on the way to `copy`, each `0700`, one checked
/// component at a time. A copy outside the store, beside its file, needs none.
pub fn make_dirs(root: &Path, copy: &str) -> Result<(), crate::diag::Diagnostic> {
    if !copy.starts_with(STORE) {
        return Ok(());
    }
    let parent = Path::new(copy).parent().unwrap_or(Path::new(STORE));
    let below = parent.strip_prefix(STORE).unwrap_or(Path::new(""));
    let mut dir = STORE.to_string();
    super::files::ensure_dir_trusted(root, &dir, STORE_MODE)?;
    for part in below.components() {
        dir.push('/');
        dir.push_str(&part.as_os_str().to_string_lossy());
        super::files::ensure_dir_trusted(root, &dir, STORE_MODE)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    /// A scratch root under the build directory, never in the temporary directory (`AGENTS.md`
    /// §6), as `files.rs`'s unit tests make theirs.
    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = Path::new(env!("OUT_DIR")).join(format!("hostscope-originals-{name}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("etc")).unwrap();
        fs::set_permissions(&dir, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(dir.join("etc/pre.conf"), "original\n").unwrap();
        fs::set_permissions(dir.join("etc/pre.conf"), fs::Permissions::from_mode(0o640)).unwrap();
        dir
    }

    fn record() -> FileRecord {
        FileRecord {
            digest: String::new(),
            mode: "0640".into(),
            owner: "0".into(),
            group: "0".into(),
            backup: None,
            on_remove: "keep".into(),
        }
    }

    fn copies(root: &Path) -> Vec<std::path::PathBuf> {
        fs::read_dir(root.join("etc/lodi/originals/etc"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .collect()
    }

    /// A copy that does not have the facts the plan read — the file changed after it was planned
    /// — is not recorded: the record keeps 1.1.1's shape, the copy made is removed, and one line
    /// names the path and why.
    #[test]
    fn a_copy_that_does_not_match_the_plan_is_removed_and_not_recorded() {
        let root = scratch("mismatch");
        let facts = Facts::observe(&root, "/etc/pre.conf");
        let mut stale = Planned::from_facts(&facts).unwrap();
        stale.digest = format!("sha256:{}", "0".repeat(64));
        let mut records = BTreeMap::from([("/etc/pre.conf".to_string(), record())]);
        let lines = adopt(&root, &mut records, &[stale]);
        assert_eq!(lines.len(), 1, "{lines:?}");
        assert!(
            lines[0].starts_with(
                "/etc/pre.conf: no copy of the original was kept (the copy does not match"
            ),
            "{lines:?}"
        );
        assert_eq!(records["/etc/pre.conf"], record());
        assert!(copies(&root).is_empty(), "{:?}", copies(&root));
        assert_eq!(
            fs::read_to_string(root.join("etc/pre.conf")).unwrap(),
            "original\n"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A copy that matches is recorded with `restore`, below directories that are `0700`.
    #[test]
    fn a_matching_copy_is_recorded_with_restore() {
        let root = scratch("match");
        let facts = Facts::observe(&root, "/etc/pre.conf");
        let planned = Planned::from_facts(&facts).unwrap();
        let mut records = BTreeMap::from([("/etc/pre.conf".to_string(), record())]);
        assert!(adopt(&root, &mut records, &[planned]).is_empty());
        let recorded = &records["/etc/pre.conf"];
        assert_eq!(recorded.on_remove, "restore");
        let copy = recorded.backup.clone().unwrap();
        assert!(
            copy.starts_with("/etc/lodi/originals/etc/pre.conf.lodi-backup-"),
            "{copy}"
        );
        for dir in ["etc/lodi/originals", "etc/lodi/originals/etc"] {
            assert_eq!(
                fs::metadata(root.join(dir)).unwrap().mode() & 0o7777,
                0o700,
                "{dir}"
            );
        }
        let _ = fs::remove_dir_all(&root);
    }
}
