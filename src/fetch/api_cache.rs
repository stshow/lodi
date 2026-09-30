//! Conditional requests to the GitHub API (LD-404).
//!
//! Every answer of `https://api.github.com/` that carried an `ETag` is kept, body and tag, in one
//! file of `$LODI_HOME/cache/api/`, and the next request for the same URL sends `If-None-Match`
//! with that tag. A `304 Not Modified` is answered from the kept body; any other answer is used
//! exactly as it would be without the cache — a 200 replaces the entry, a 403 or 429 is still the
//! caller's error, a failed connection is still a failure. So the cache never decides what a lock
//! pins: the server does, every time, and the network is still asked every time.
//!
//! The file is untrusted input. It is opened without following a link, read only when it is a
//! regular file of this user's with no permission for anyone else, in a directory of this user's
//! that no one else may write, bounded, parsed, held to its registered schema, to the URL and
//! request it names, to a tag that can be sent as a header, and to the SHA-256 of its own body.
//! Anything else is ignored, and the request is made as if there were no entry. It is written
//! whole, owner-only, through a new temporary file renamed over it; a symbolic link or anything
//! else that is not a regular file in its place is left alone and not written through. No
//! failure of the cache ever fails a fetch.

use std::fs;
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

use crate::util::sha256_hex;

/// The registered on-disk artifact ([`crate::schema::ARTIFACTS`]).
pub const ARTIFACT: &str = "api-cache";

/// The cache's directory, relative to the store root.
pub const DIR: &str = "cache/api";

/// The URLs whose answers are kept.
pub const PREFIX: &str = "https://api.github.com/";

/// The longest tag kept; GitHub's are 68 bytes.
const MAX_ETAG_BYTES: usize = 1024;

/// The largest entry read: the largest body a fetch accepts, every byte of it escaped, and room
/// for the rest of the document.
const MAX_ENTRY_BYTES: u64 = 2 * super::MAX_BODY_BYTES + 64 * 1024;

static TEMPORARY: AtomicU64 = AtomicU64::new(0);

/// Whether the answer of `url` is kept.
pub fn applies(url: &str) -> bool {
    url.starts_with(PREFIX)
}

/// One kept answer, as it is written: the URL and the request headers it answered — never
/// `Authorization` — its tag, and its body with the body's SHA-256.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CachedResponse {
    pub version: u64,
    pub lodi_version: String,
    pub url: String,
    pub request: Vec<String>,
    pub etag: String,
    pub sha256: String,
    pub body: String,
}

/// An answer read back from the cache: the tag to send, and the body a 304 stands for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
    pub etag: String,
    pub body: Vec<u8>,
}

/// The cache under one store root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiCache {
    root: PathBuf,
}

/// One request's place in the cache: the file its answer is kept in, and what it was asked with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    root: PathBuf,
    url: String,
    request: Vec<String>,
    name: String,
}

impl ApiCache {
    /// The cache of the store at `root`, which must be absolute; `None` otherwise.
    pub fn new(root: &Path) -> Option<ApiCache> {
        root.is_absolute().then(|| ApiCache {
            root: root.to_path_buf(),
        })
    }

    /// The cache of the store `$LODI_HOME` names ([`crate::store::Store::home_from_env`]);
    /// `None` when there is no such store.
    pub fn from_env() -> Option<ApiCache> {
        ApiCache::new(&crate::store::Store::home_from_env().ok()?)
    }

    /// The directory the entries are in.
    pub fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    /// Where the answer of `url`, asked with `headers`, is kept — or `None` when it is not kept:
    /// a URL outside [`PREFIX`], or a request header other than `Accept` and `Authorization`,
    /// whose value is never written anywhere and so is not part of the key either.
    pub fn slot(&self, url: &str, headers: &[(&str, String)]) -> Option<Slot> {
        if !applies(url) {
            return None;
        }
        let mut request = Vec::new();
        for (name, value) in headers {
            let name = name.to_ascii_lowercase();
            match name.as_str() {
                "authorization" => {}
                "accept" => request.push(format!("{name}: {value}")),
                _ => return None,
            }
        }
        let mut key = format!("GET {url}\n");
        for line in &request {
            key.push_str(line);
            key.push('\n');
        }
        let name = format!("{}.json", &sha256_hex(key.as_bytes())[..32]);
        Some(Slot {
            root: self.root.clone(),
            url: url.to_string(),
            request,
            name,
        })
    }
}

impl Slot {
    fn dir(&self) -> PathBuf {
        self.root.join(DIR)
    }

    /// The entry's path.
    pub fn path(&self) -> PathBuf {
        self.dir().join(&self.name)
    }

    /// The kept answer, when there is a sound one. Everything that is not is `None`.
    pub fn load(&self) -> Option<Kept> {
        private_dir(&self.dir())?;
        let file = fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC | libc::O_NOCTTY)
            .open(self.path())
            .ok()?;
        let meta = file.metadata().ok()?;
        if !meta.is_file() || meta.uid() != euid() || meta.mode() & 0o077 != 0 {
            return None;
        }
        let mut bytes = Vec::new();
        file.take(MAX_ENTRY_BYTES + 1)
            .read_to_end(&mut bytes)
            .ok()?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return None;
        }
        let kept: CachedResponse = crate::schema::parse_registered(ARTIFACT, &bytes)?;
        let sound = kept.url == self.url
            && kept.request == self.request
            && sendable(&kept.etag)
            && kept.sha256 == sha256_hex(kept.body.as_bytes());
        sound.then(|| Kept {
            etag: kept.etag,
            body: kept.body.into_bytes(),
        })
    }

    /// Keep `body` under `etag`, replacing what was kept. Nothing is written for a tag that could
    /// not be sent back as a header, a body that is not UTF-8 (every answer of the API is JSON),
    /// a store written by a Lodi whose layout this build does not know, a directory or an entry
    /// this build would not read, or anything but a regular file in the entry's place.
    pub fn store(&self, etag: &str, body: &[u8]) {
        let _ = self.try_store(etag, body);
    }

    fn try_store(&self, etag: &str, body: &[u8]) -> io::Result<()> {
        let refused = || io::Error::other("not kept");
        let body = std::str::from_utf8(body).map_err(|_| refused())?;
        if !sendable(etag) {
            return Err(refused());
        }
        crate::layout::read(&self.root).map_err(|_| refused())?;
        let dir = self.dir();
        if fs::symlink_metadata(&dir).is_err() {
            fs::create_dir_all(dir.parent().ok_or_else(refused)?)?;
            match fs::DirBuilder::new().mode(0o700).create(&dir) {
                Ok(()) => fs::set_permissions(&dir, fs::Permissions::from_mode(0o700))?,
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {}
                Err(e) => return Err(e),
            }
        }
        private_dir(&dir).ok_or_else(refused)?;
        let path = self.path();
        match fs::symlink_metadata(&path) {
            Ok(meta) if !meta.file_type().is_file() => return Err(refused()),
            _ => {}
        }
        let record = CachedResponse {
            version: crate::schema::artifact(ARTIFACT).writes,
            lodi_version: crate::schema::LODI_VERSION.to_string(),
            url: self.url.clone(),
            request: self.request.clone(),
            etag: etag.to_string(),
            sha256: sha256_hex(body.as_bytes()),
            body: body.to_string(),
        };
        let text = crate::schema::stamped_json(ARTIFACT, &record);
        let temporary = dir.join(format!(
            ".{}.{}-{}.tmp",
            self.name,
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let written = (|| {
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(&temporary)?;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.write_all(text.as_bytes())?;
            file.sync_all()?;
            drop(file);
            // `rename(2)` replaces what is at `path` and never follows it.
            fs::rename(&temporary, &path)
        })();
        if written.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        written
    }
}

/// Whether `dir` is a directory, not a link, of this user's, that no one else may write.
fn private_dir(dir: &Path) -> Option<()> {
    let meta = fs::symlink_metadata(dir).ok()?;
    (meta.file_type().is_dir() && meta.uid() == euid() && meta.mode() & 0o022 == 0).then_some(())
}

/// Whether `etag` can be sent back as a header value: 1 to [`MAX_ETAG_BYTES`] visible ASCII
/// characters, so no line break, no control character and nothing else a header cannot carry.
fn sendable(etag: &str) -> bool {
    (1..=MAX_ETAG_BYTES).contains(&etag.len()) && etag.bytes().all(|b| b.is_ascii_graphic())
}

fn euid() -> u32 {
    // SAFETY: `geteuid` cannot fail and has no preconditions.
    unsafe { libc::geteuid() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "lodi-api-cache-{name}-{}-{}",
            std::process::id(),
            TEMPORARY.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const URL: &str = "https://api.github.com/repos/o/r/releases?per_page=2";

    #[test]
    fn only_the_github_api_is_kept_and_only_under_known_headers() {
        let cache = ApiCache::new(Path::new("/nonexistent-store")).unwrap();
        assert!(cache.slot("https://github.com/o/r/releases", &[]).is_none());
        assert!(cache.slot("https://example.org/x", &[]).is_none());
        let plain = cache.slot(URL, &[]).unwrap();
        let accept = ("Accept", "application/vnd.github+json".to_string());
        let with_accept = cache.slot(URL, std::slice::from_ref(&accept)).unwrap();
        assert_ne!(
            plain.path(),
            with_accept.path(),
            "Accept is part of the key"
        );
        // The token is neither part of the key nor of what is written.
        let token = ("Authorization", "Bearer not-a-real-token-0000".to_string());
        let with_token = cache.slot(URL, &[accept.clone(), token]).unwrap();
        assert_eq!(with_token, with_accept);
        assert!(!format!("{with_token:?}").contains("not-a-real-token"));
        // A header the cache does not know is not kept at all.
        let other = ("X-Other", "1".to_string());
        assert!(cache.slot(URL, &[other]).is_none());
        assert!(ApiCache::new(Path::new("relative/store")).is_none());
    }

    #[test]
    fn a_kept_answer_reads_back_and_a_tampered_one_does_not() {
        let root = scratch("round-trip");
        let cache = ApiCache::new(&root).unwrap();
        let slot = cache.slot(URL, &[]).unwrap();
        assert_eq!(slot.load(), None);
        slot.store("W/\"abc\"", b"[1]");
        let kept = slot.load().unwrap();
        assert_eq!(kept.etag, "W/\"abc\"");
        assert_eq!(kept.body, b"[1]");
        let meta = fs::metadata(slot.path()).unwrap();
        assert_eq!(meta.mode() & 0o7777, 0o600);
        assert_eq!(fs::metadata(cache.dir()).unwrap().mode() & 0o7777, 0o700);

        // Not sendable, not UTF-8: nothing is written, what was kept stays.
        slot.store("W/\"a\"\r\nX: 1", b"[2]");
        slot.store("W/\"b\"", &[0xff, 0xfe]);
        assert_eq!(slot.load().unwrap().body, b"[1]");

        // Readable by others: not believed.
        fs::set_permissions(slot.path(), fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(slot.load(), None);
        fs::set_permissions(slot.path(), fs::Permissions::from_mode(0o600)).unwrap();
        // A directory others may write: not believed, and not written into.
        fs::set_permissions(cache.dir(), fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(slot.load(), None);
        slot.store("W/\"c\"", b"[3]");
        fs::set_permissions(cache.dir(), fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(slot.load().unwrap().body, b"[1]");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_store_of_an_unknown_layout_is_not_written_into() {
        let root = scratch("layout");
        fs::write(
            root.join(".layout.json"),
            br#"{"layout":99,"lodiVersion":"9.9.9"}"#,
        )
        .unwrap();
        let slot = ApiCache::new(&root).unwrap().slot(URL, &[]).unwrap();
        slot.store("W/\"abc\"", b"[1]");
        assert!(fs::symlink_metadata(root.join(DIR)).is_err());
        let _ = fs::remove_dir_all(&root);
    }
}
