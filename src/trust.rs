//! The trust gate (design ADR-014, `spec/12` §5; M-Spike S-3; LD-359).
//!
//! Before Lodi realizes or runs anything for a manifest that declares script text, or that puts
//! tools on `PATH`, the user must have trusted exactly that manifest. What a trust record covers
//! is the manifest's [`Subject`]:
//!
//! - a manifest with tasks is trusted for its task text: every task name and `run` string,
//!   hashed exactly as every release before LD-359 hashed it, so a record those releases wrote
//!   stays valid byte for byte. (Hooks, modules, inputs and services are refused by the manifest
//!   loader, so nothing is merged in and nothing is silently left out.)
//! - a manifest with no tasks that declares tools or a container base is trusted for its whole
//!   file (LD-359): those tools go first on `PATH` in its environment, where they run in place
//!   of host commands of the same name, so they are authorized like task text is.
//! - a manifest that declares neither has nothing to trust.
//!
//! Authorization boundary: trusting a manifest authorizes what Lodi shows and hashes. Files that
//! text executes or sources (a `Makefile`, a script in the project, anything on disk or
//! downloaded), and what a declared tool does when it runs, are outside the boundary.
//!
//! Trust is recorded in `$XDG_CONFIG_HOME/lodi/trust.json` (default `~/.config/lodi`), keyed by
//! the manifest's absolute path. It is local runtime state and holds that path.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::diag::Diagnostic;
use crate::lock::canonical_json;
use crate::manifest::ProjectManifest;
use crate::util::{format_utc, now_utc, sha256_tagged};

/// The statement shown with every trust decision about task text.
pub const BOUNDARY: &str = "Trusting authorizes only the task text shown above. Files that \
text runs or sources (a Makefile, project scripts, anything on disk or downloaded) are outside \
this authorization and are not shown or hashed by lodi.";

/// The statement shown with every trust decision about a manifest without tasks (LD-359).
pub const MANIFEST_BOUNDARY: &str = "Trusting authorizes only the manifest shown above: the \
tools and base it declares, which its environment puts first on PATH. What those tools do when \
they run, and anything they read or download, is outside this authorization.";

/// The script text of a manifest in the order it is shown and hashed: `(task name, run)`.
pub fn script_text(manifest: &ProjectManifest) -> Vec<(String, String)> {
    manifest
        .tasks
        .iter()
        .map(|(name, task)| (name.clone(), task.run.clone()))
        .collect()
}

/// `sha256:<hex>` over the script text: a format line, then for every task (sorted by name)
/// its name and `run` string, each preceded by its byte length.
pub fn script_hash(text: &[(String, String)]) -> String {
    let mut bytes = b"lodi-trust/1\n".to_vec();
    for (name, run) in text {
        for part in [name, run] {
            bytes.extend_from_slice(format!("{}\n", part.len()).as_bytes());
            bytes.extend_from_slice(part.as_bytes());
            bytes.push(b'\n');
        }
    }
    sha256_tagged(&bytes)
}

/// `sha256:<hex>` over a whole manifest file: a format line of its own, which no task-text
/// preimage starts with, then the file's byte length and its bytes (LD-359).
pub fn manifest_hash(bytes: &[u8]) -> String {
    let mut all = format!("lodi-trust-manifest/1\n{}\n", bytes.len()).into_bytes();
    all.extend_from_slice(bytes);
    all.push(b'\n');
    sha256_tagged(&all)
}

/// The text shown to the user before a trust decision.
pub fn describe(manifest_path: &Path, text: &[(String, String)], hash: &str) -> String {
    let mut out = format!(
        "{} declares {} task{}:\n",
        manifest_path.display(),
        text.len(),
        if text.len() == 1 { "" } else { "s" }
    );
    for (name, run) in text {
        out.push_str(&format!("  task {name}:\n"));
        for line in run.lines() {
            out.push_str(&format!("    | {line}\n"));
        }
    }
    out.push_str(&format!("script text hash: {hash}\n{BOUNDARY}"));
    out
}

/// What a trust record for one manifest authorizes (LD-359).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// No tasks, no tools and no base: nothing to trust.
    Nothing,
    /// The task text, as [`script_text`] gives it.
    Tasks(Vec<(String, String)>),
    /// No tasks, but tools or a container base: the manifest file's own bytes.
    Manifest(Vec<u8>),
}

impl Subject {
    /// The subject of `manifest`, whose file holds `bytes`.
    pub fn of(manifest: &ProjectManifest, bytes: &[u8]) -> Subject {
        let text = script_text(manifest);
        if !text.is_empty() {
            Subject::Tasks(text)
        } else if !manifest.tools.is_empty() || manifest.container.is_some() {
            Subject::Manifest(bytes.to_vec())
        } else {
            Subject::Nothing
        }
    }

    /// The hash a trust record holds for this subject; `None` for [`Subject::Nothing`].
    pub fn hash(&self) -> Option<String> {
        match self {
            Subject::Nothing => None,
            Subject::Tasks(text) => Some(script_hash(text)),
            Subject::Manifest(bytes) => Some(manifest_hash(bytes)),
        }
    }

    /// What is trusted, in the words of a message: `task text` or `manifest`.
    pub fn noun(&self) -> &'static str {
        match self {
            Subject::Manifest(_) => "manifest",
            _ => "task text",
        }
    }

    /// The boundary statement of this subject.
    pub fn boundary(&self) -> &'static str {
        match self {
            Subject::Manifest(_) => MANIFEST_BOUNDARY,
            _ => BOUNDARY,
        }
    }

    /// The text shown to the user before a trust decision about this subject.
    pub fn describe(&self, manifest_path: &Path) -> String {
        let hash = self.hash().unwrap_or_default();
        match self {
            Subject::Nothing => String::new(),
            Subject::Tasks(text) => describe(manifest_path, text, &hash),
            Subject::Manifest(bytes) => {
                let mut out = format!(
                    "{} declares no tasks, and tools or a base that its environment puts \
                     first on PATH:\n",
                    manifest_path.display()
                );
                for line in String::from_utf8_lossy(bytes).lines() {
                    out.push_str(&format!("    | {line}\n"));
                }
                out.push_str(&format!("manifest hash: {hash}\n{MANIFEST_BOUNDARY}"));
                out
            }
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustFile {
    version: u64,
    /// The Lodi that last wrote the file. `#[serde(default)]` because releases through 0.3.0
    /// wrote this record without it (M-1.0 T-1, design call D5).
    #[serde(default)]
    lodi_version: String,
    entries: BTreeMap<String, TrustEntry>,
}

/// The registry row the trust store is registered as (`src/schema.rs`).
pub const TRUST_ARTIFACT: &str = "trust-store";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct TrustEntry {
    hash: String,
    trusted: String,
}

/// The state of a manifest's [`Subject`] against the trust store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// [`Subject::Nothing`]: nothing to trust.
    NothingToTrust,
    Trusted,
    /// Never trusted.
    Unknown,
    /// Trusted before with another subject (`W_TRUST_CHANGED`).
    Changed,
}

/// The trust store file.
#[derive(Debug, Clone)]
pub struct TrustStore {
    path: PathBuf,
}

impl TrustStore {
    /// `$XDG_CONFIG_HOME/lodi/trust.json`, else `$HOME/.config/lodi/trust.json`. The variables
    /// are the ones `crate::roots::capture` read; this precedence is its `trust_dir`
    /// projection, unchanged since S-3 (`LD-184`).
    pub fn from_env() -> Result<TrustStore, Diagnostic> {
        Ok(TrustStore {
            path: crate::roots::capture().trust_dir()?.join("trust.json"),
        })
    }

    pub fn at(path: PathBuf) -> TrustStore {
        TrustStore { path }
    }

    fn read(&self) -> Result<TrustFile, Diagnostic> {
        match fs::read(&self.path) {
            Ok(bytes) => self.parse(&bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(TrustFile {
                version: crate::schema::artifact(TRUST_ARTIFACT).writes,
                lodi_version: crate::schema::LODI_VERSION.to_string(),
                entries: BTreeMap::new(),
            }),
            Err(e) => Err(Diagnostic::new(
                "E_CONFIG",
                format!("cannot read {}: {e}", self.path.display()),
            )),
        }
    }

    /// The schema version is decided from the raw JSON **before** the rest is deserialized, and
    /// a version outside the read-set keeps this file's own code, `E_CONFIG` (M-1.0 T-1).
    fn parse(&self, bytes: &[u8]) -> Result<TrustFile, Diagnostic> {
        let invalid = |why: String| {
            Diagnostic::new(
                "E_CONFIG",
                format!("{} is not a valid trust file: {why}", self.path.display()),
            )
        };
        let raw: serde_json::Value =
            serde_json::from_slice(bytes).map_err(|e| invalid(e.to_string()))?;
        let artifact = crate::schema::artifact(TRUST_ARTIFACT);
        match crate::schema::version_of(TRUST_ARTIFACT, &raw) {
            Some(v) if artifact.reads_version(v) => {}
            found => {
                let carries = match found {
                    Some(v) => format!("is schema version {v}"),
                    None => "records no schema version".to_string(),
                };
                let mut d = Diagnostic::new(
                    "E_CONFIG",
                    format!(
                        "the trust file {} {carries}; this lodi reads version {}",
                        self.path.display(),
                        artifact.read_set()
                    ),
                );
                d.notes
                    .push(crate::schema::writer_note(TRUST_ARTIFACT, &raw));
                return Err(d.hint("use the lodi that wrote it, or move it aside and trust again"));
            }
        }
        serde_json::from_value(raw).map_err(|e| invalid(e.to_string()))
    }

    fn update(&self, change: impl FnOnce(&mut TrustFile)) -> Result<(), Diagnostic> {
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let io = |e: std::io::Error| {
            Diagnostic::new(
                "E_CONFIG",
                format!("cannot write {}: {e}", self.path.display()),
            )
        };
        fs::create_dir_all(dir).map_err(io)?;
        let lock_path = dir.join("trust.json.lock");
        let lock = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(io)?;
        // SAFETY: flock on a descriptor owned by `lock`, released when it is closed.
        unsafe { libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX) };
        let mut file = self.read()?;
        change(&mut file);
        file.version = crate::schema::artifact(TRUST_ARTIFACT).writes;
        file.lodi_version = crate::schema::LODI_VERSION.to_string();
        crate::lock::write_atomic(&self.path, canonical_json(&file).as_bytes()).map_err(io)
    }

    /// The trust status of `subject` for the manifest at `manifest_path` (absolute).
    pub fn status(&self, manifest_path: &Path, subject: &Subject) -> Result<Status, Diagnostic> {
        let Some(hash) = subject.hash() else {
            return Ok(Status::NothingToTrust);
        };
        let file = self.read()?;
        Ok(match file.entries.get(&key(manifest_path)) {
            None => Status::Unknown,
            Some(e) if e.hash == hash => Status::Trusted,
            Some(_) => Status::Changed,
        })
    }

    /// Record trust in `subject` for the manifest at `manifest_path`; the hash recorded, or
    /// `None` (and nothing written) for [`Subject::Nothing`].
    pub fn trust(
        &self,
        manifest_path: &Path,
        subject: &Subject,
    ) -> Result<Option<String>, Diagnostic> {
        let Some(hash) = subject.hash() else {
            return Ok(None);
        };
        let entry = TrustEntry {
            hash: hash.clone(),
            trusted: format_utc(now_utc()),
        };
        self.update(|f| {
            f.entries.insert(key(manifest_path), entry);
        })?;
        Ok(Some(hash))
    }

    /// Remove any trust recorded for the manifest at `manifest_path`; whether there was one.
    pub fn revoke(&self, manifest_path: &Path) -> Result<bool, Diagnostic> {
        let mut removed = false;
        self.update(|f| removed = f.entries.remove(&key(manifest_path)).is_some())?;
        Ok(removed)
    }
}

fn key(manifest_path: &Path) -> String {
    manifest_path.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect()
    }

    #[test]
    fn the_hash_covers_names_and_text_without_ambiguity() {
        let a = script_hash(&text(&[("build", "make")]));
        assert_eq!(a, script_hash(&text(&[("build", "make")])));
        assert_ne!(a, script_hash(&text(&[("build", "make ")])));
        assert_ne!(a, script_hash(&text(&[("buil", "dmake")])));
        assert_ne!(a, script_hash(&text(&[("build", "make"), ("x", "")])));
    }

    /// The task-text hash is the one every earlier release recorded: the trust stores they
    /// wrote stay valid byte for byte (LD-359).
    #[test]
    fn a_manifest_with_tasks_hashes_as_before() {
        assert_eq!(
            script_hash(&text(&[("hello", "echo hello")])),
            "sha256:344f561fd0a19dfa46b99a34393e5cfe091b5f1b9ce42268707a41ebf5b195b1"
        );
    }

    #[test]
    fn a_manifest_hash_is_never_a_task_text_hash() {
        let bytes = b"[tools]\ngo = \"1.22\"\n";
        let whole = manifest_hash(bytes);
        assert_eq!(whole, manifest_hash(bytes));
        assert_ne!(whole, manifest_hash(b"[tools]\ngo = \"1.23\"\n"));
        assert_ne!(whole, script_hash(&[]));
        assert_ne!(manifest_hash(b""), script_hash(&[]));
    }

    #[test]
    fn describe_shows_every_line_and_the_boundary() {
        let t = text(&[("build", "make\nmake install")]);
        let shown = describe(Path::new("/p/lodi.toml"), &t, "sha256:x");
        assert!(shown.contains("    | make\n    | make install"));
        assert!(shown.contains("outside"));
    }
}
