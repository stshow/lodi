//! A config from a git URL (`lodi switch URL`, #695, LD-512): 1.x's fetch of a public git URL
//! (LD-401: the grammar, `--ref`, `--rev`, `--refresh`, a NAR-verified tree), kept in the
//! person's state and read as a config folder that is never written.
//!
//! A commit's tree lives at `<state>/lodi/git/<sha256 of the URL>/<commit>/`, folders 0700,
//! fetched by the person (under `sudo` or `doas` in a child that becomes them). The commit the
//! last successful switch applied is recorded in `<state>/lodi/fetched.json` (URL, ref, commit,
//! NAR); a switch of the same URL and ref uses that commit and asks nobody while its tree is kept.
//! What the tree's own `lodi.lock` lacked is resolved once and kept beside it, `<commit>.lock`.

use std::ffi::OsStr;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};

use super::User;
use super::lock::Lock;
use crate::diag::Diagnostic;
use crate::hostscope::lock::GitRecord;
use crate::hostscope::remote::{self, Ask};

/// The cache of fetched trees, below the person's state folder.
const CACHE: &str = "lodi/git";
/// The record of the commit the last switch from a URL applied.
const RECORD: &str = "lodi/fetched.json";

/// A fetched config: its tree, the record that locks it, and the preview's source line.
pub struct Fetched {
    pub dir: PathBuf,
    pub git: GitRecord,
    pub line: String,
}

fn state(user: &User) -> Result<&Path, Diagnostic> {
    user.state.as_deref().ok_or_else(|| {
        Diagnostic::new(
            "E_CONFIG",
            "HOME is not set, so a fetched config has nowhere to be kept",
        )
    })
}

/// Run `work` as the person (`owner`, under `sudo` or `doas`) in a child, or here.
fn as_person(
    owner: Option<(u32, u32)>,
    work: impl FnOnce() -> Result<String, Diagnostic>,
) -> Result<String, Diagnostic> {
    match owner {
        None => work(),
        Some((uid, gid)) => crate::hostscope::flake::as_person(uid, gid, "fetch", || {
            work().map_err(|d| (d.code, d.message))
        })
        .map_err(|(code, text)| {
            let prefix = format!("lodi: error {code}: ");
            Diagnostic::new(
                code,
                text.strip_prefix(&prefix).unwrap_or(&text).to_string(),
            )
        }),
    }
}

/// Fetch `typed` (a URL as typed) for `user` as `ask` says, against the recorded commit.
pub fn fetch(
    typed: &str,
    ask: Ask,
    user: &User,
    owner: Option<(u32, u32)>,
) -> Result<Fetched, Diagnostic> {
    let url = remote::parse(OsStr::new(typed))?
        .ok_or_else(|| Diagnostic::new("E_UNSUPPORTED", format!("{typed} is not a git URL")))?;
    let state = state(user)?;
    let cache = state.join(CACHE);
    let locked = read_record(&state.join(RECORD))?;
    let fetched = as_person(owner, || {
        let trees = remote::trees(&cache, &url);
        let prepare = || make_dir(&trees);
        let tree = remote::tree(&cache, &url, ask, locked.as_ref(), &prepare)?;
        Ok(format!(
            "{}\n{}\n{}",
            tree.git.rev, tree.git.nar_hash, tree.said
        ))
    })?;
    let mut lines = fetched.lines();
    let (rev, nar_hash, said) = (
        lines.next().unwrap_or_default().to_string(),
        lines.next().unwrap_or_default().to_string(),
        lines.next().unwrap_or_default().to_string(),
    );
    let reference = ask.git_ref.unwrap_or("HEAD").to_string();
    Ok(Fetched {
        dir: remote::trees(&cache, &url).join(&rev),
        line: format!(
            "source {} ref {reference} commit {rev}: {said}",
            url.canonical
        ),
        git: GitRecord {
            url: url.canonical,
            reference,
            rev,
            nar_hash,
        },
    })
}

/// The lock kept beside `fetched`'s tree by the last switch of its commit ([`record`]): what the
/// tree's own `lodi.lock` lacked, resolved once (LD-528). `None` when there is none.
pub fn kept_lock(fetched: &Fetched) -> Result<Option<Lock>, Diagnostic> {
    let path = kept(fetched);
    if std::fs::symlink_metadata(&path).is_err() {
        return Ok(None);
    }
    super::lock::read_file(&path).map(Some)
}

/// `<state>/lodi/git/<sha256 of the URL>/<commit>.lock`, beside the tree it locks.
fn kept(fetched: &Fetched) -> PathBuf {
    fetched.dir.with_extension("lock")
}

/// After a switch that succeeded: record `fetched`'s commit and keep `lock` beside its tree, and
/// remove every other kept tree. A file that already holds these bytes is left as it is.
pub fn record(
    fetched: &Fetched,
    lock: &Lock,
    user: &User,
    owner: Option<(u32, u32)>,
) -> Result<(), Diagnostic> {
    let state = state(user)?;
    let file = state.join(RECORD);
    let failed = |e: std::io::Error| {
        Diagnostic::new(
            "E_STORE_IO",
            format!(
                "cannot record the fetched commit in {}: {e}",
                file.display()
            ),
        )
    };
    let text = serde_json::to_string_pretty(&fetched.git)
        .map_err(|e| Diagnostic::new("E_STORE_IO", e.to_string()))?;
    let text = format!("{text}\n");
    let locked = lock.render();
    as_person(owner, || {
        put(&kept(fetched), locked.as_bytes()).map_err(failed)?;
        make_dir(file.parent().unwrap_or(state))?;
        put(&file, text.as_bytes()).map_err(failed)?;
        remote::prune_in(&state.join(CACHE), &fetched.git);
        Ok(String::new())
    })
    .map(drop)
}

/// `bytes` at `file` through a staged file and a rename, unless it already holds them.
fn put(file: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if std::fs::read(file).is_ok_and(|now| now == bytes) {
        return Ok(());
    }
    let staged = file.with_extension(format!("staged.{}", std::process::id()));
    let written = std::fs::write(&staged, bytes).and_then(|()| std::fs::rename(&staged, file));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// `dir` and every missing folder above it, 0700.
fn make_dir(dir: &Path) -> Result<(), Diagnostic> {
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("cannot make {}: {e}", dir.display())))
}

/// The recorded commit, if any.
fn read_record(file: &Path) -> Result<Option<GitRecord>, Diagnostic> {
    let text = match std::fs::read_to_string(file) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(Diagnostic::new(
                "E_CONFIG",
                format!("cannot read {}: {e}", file.display()),
            ));
        }
    };
    serde_json::from_str(&text).map(Some).map_err(|e| {
        Diagnostic::new(
            "E_CONFIG",
            format!("{} is not a fetched commit's record: {e}", file.display()),
        )
        .hint("remove it; the next switch from a URL resolves its ref again")
    })
}
