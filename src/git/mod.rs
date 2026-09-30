//! Verified single-commit smart-HTTP v2 reader; not yet a CLI surface.
mod pack;
mod protocol;
#[cfg(test)]
mod tests;

use pack::Object;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Limits {
    pub destination: PathBuf,
    pub max_objects: usize,
    pub max_object_bytes: usize,
    pub max_tree_bytes: usize,
}
impl Limits {
    pub fn at(destination: PathBuf) -> Self {
        Self {
            destination,
            max_objects: 100_000,
            max_object_bytes: 16 * 1024 * 1024,
            max_tree_bytes: 128 * 1024 * 1024,
        }
    }
}
#[derive(Debug, Clone)]
pub struct GitError {
    pub code: &'static str,
    pub message: String,
}
impl std::fmt::Display for GitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for GitError {}
fn fail(code: &'static str, message: impl Into<String>) -> GitError {
    GitError {
        code,
        message: message.into(),
    }
}
#[derive(Debug, Clone)]
pub struct Refs {
    pub head: Option<String>,
    pub names: Vec<(String, String)>,
}
impl Refs {
    /// HEAD by default, or an unambiguous branch or tag. Annotated tags use the peeled commit.
    pub fn resolve(&self, asked: Option<&str>) -> Result<String, GitError> {
        let name = asked.unwrap_or("HEAD");
        if name.len() > 255 || !name.bytes().all(|b| (b'!'..=b'~').contains(&b)) {
            return Err(fail(
                "E_NO_MATCH",
                "ref must be a printable name of at most 255 bytes",
            ));
        }
        let branch = format!("refs/heads/{name}");
        let tag = format!("refs/tags/{name}");
        if self.names.iter().any(|(n, _)| n == &branch) && self.names.iter().any(|(n, _)| n == &tag)
        {
            return Err(fail(
                "E_NO_MATCH",
                format!("{name} names both {branch} and {tag}; use a full ref"),
            ));
        }
        let selected = if name == "HEAD" {
            "HEAD".to_string()
        } else if name.starts_with("refs/") {
            name.to_string()
        } else if self.names.iter().any(|(n, _)| n == &branch) {
            branch
        } else {
            tag
        };
        let peeled = format!("{selected}^{{}}");
        self.names
            .iter()
            .find(|(n, _)| n == &peeled)
            .or_else(|| self.names.iter().find(|(n, _)| n == &selected))
            .map(|(_, id)| id.clone())
            .ok_or_else(|| {
                let mut candidates: Vec<_> = self
                    .names
                    .iter()
                    .filter(|(n, _)| !n.ends_with("^{}") && n != "HEAD")
                    .map(|(n, _)| n.as_str())
                    .collect();
                candidates.sort_by_key(|n| (distance(name, n.rsplit('/').next().unwrap_or(n)), *n));
                let nearest = candidates
                    .into_iter()
                    .take(8)
                    .collect::<Vec<_>>()
                    .join(", ");
                fail(
                    "E_NO_MATCH",
                    format!("unknown ref {name}; nearest refs: {nearest}"),
                )
            })
    }
}
fn distance(a: &str, b: &str) -> usize {
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, left) in a.bytes().enumerate() {
        let mut next = vec![i + 1; b.len() + 1];
        for (j, right) in b.bytes().enumerate() {
            next[j + 1] = (prev[j + 1] + 1)
                .min(next[j] + 1)
                .min(prev[j] + usize::from(left != right));
        }
        prev = next;
    }
    prev[b.len()]
}
#[derive(Debug, Clone)]
pub struct Checkout {
    pub commit: String,
    pub tree: PathBuf,
    pub nar_hash: String,
}

pub trait GitRemote {
    fn ls_refs(&self, url: &str) -> Result<Refs, GitError>;
    fn fetch_commit(&self, url: &str, rev: &str, limits: &Limits) -> Result<Checkout, GitError>;
}

pub struct HttpGitRemote {
    pub fetcher: crate::fetch::HttpFetcher,
}
const URL: &str = "/git-upload-pack";
fn request(command: &[u8], args: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    protocol::pkt(command, &mut out);
    protocol::pkt(b"object-format=sha1\n", &mut out);
    out.extend_from_slice(b"0001");
    for arg in args {
        protocol::pkt(arg, &mut out);
    }
    out.extend_from_slice(b"0000");
    out
}
fn fetch_error(err: crate::fetch::FetchError) -> GitError {
    fail("E_FETCH", err.to_string())
}
impl HttpGitRemote {
    fn advertise(&self, url: &str) -> Result<(), GitError> {
        let body = self
            .fetcher
            .git_exchange(
                &format!("{url}/info/refs?service=git-upload-pack"),
                None,
                64 * 1024,
            )
            .map_err(fetch_error)?;
        protocol::advertise(&body)
    }
    fn post(&self, url: &str, body: &[u8], limit: u64) -> Result<Vec<u8>, GitError> {
        self.fetcher
            .git_exchange(&format!("{url}{URL}"), Some(body), limit)
            .map_err(fetch_error)
    }
}
impl GitRemote for HttpGitRemote {
    fn ls_refs(&self, url: &str) -> Result<Refs, GitError> {
        self.advertise(url)?;
        let body = self.post(
            url,
            &request(b"command=ls-refs\n", &[b"peel\n", b"symrefs\n"]),
            1024 * 1024,
        )?;
        protocol::refs(&body)
    }
    fn fetch_commit(&self, url: &str, rev: &str, limits: &Limits) -> Result<Checkout, GitError> {
        if limits.max_objects > 100_000
            || limits.max_object_bytes > 16 * 1024 * 1024
            || limits.max_tree_bytes > 128 * 1024 * 1024
        {
            return Err(fail(
                "E_UNSUPPORTED",
                "caller limits exceed the fixed caps on a git fetch",
            ));
        }
        if rev.len() != 40 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(fail(
                "E_NO_MATCH",
                "a commit must be 40 hexadecimal SHA-1 digits",
            ));
        }
        self.advertise(url)?;
        let want = format!("want {rev}\n");
        let body = request(
            b"command=fetch\n",
            &[want.as_bytes(), b"deepen 1\n", b"done\n"],
        );
        let response = self.post(url, &body, 160 * 1024 * 1024)?;
        let pack = protocol::pack(&response)?;
        let objects = pack::read(&pack, limits)?;
        let mut used = BTreeSet::new();
        let commit_id = rev.to_ascii_lowercase();
        let commit = needed(&objects, &commit_id, 1, &mut used)?;
        let tree_id = commit
            .data
            .split(|b| *b == b'\n')
            .next()
            .and_then(|line| line.strip_prefix(b"tree "))
            .and_then(|id| std::str::from_utf8(id).ok())
            .filter(|id| id.len() == 40 && id.bytes().all(|b| b.is_ascii_hexdigit()))
            .ok_or_else(|| fail("E_HASH_MISMATCH", "commit has no valid tree"))?;
        let mut files = Vec::new();
        let mut total = 0usize;
        let mut state = WalkState {
            used: &mut used,
            files: &mut files,
            total: &mut total,
            limits,
        };
        walk(&objects, tree_id, Path::new(""), &mut state, 0)?;
        if objects.len() != used.len() {
            return Err(fail("E_HASH_MISMATCH", "extra object in pack"));
        }
        let dir = &limits.destination;
        if dir.exists() {
            return Err(fail("E_FETCH", "checkout destination already exists"));
        }
        let parent = dir
            .parent()
            .ok_or_else(|| fail("E_FETCH", "checkout has no parent"))?;
        let filename = dir
            .file_name()
            .ok_or_else(|| fail("E_FETCH", "checkout has no name"))?;
        let temp = parent.join(format!(
            ".{}-{}",
            filename.to_string_lossy(),
            std::process::id()
        ));
        fs::create_dir(&temp).map_err(|e| fail("E_FETCH", format!("checkout: {e}")))?;
        // No partially written tree is returned, including on failure to compute the NAR.
        let result = (|| {
            fs::set_permissions(&temp, fs::Permissions::from_mode(0o700))?;
            for (path, data, executable) in &files {
                let to = temp.join(path);
                fs::create_dir_all(to.parent().unwrap())?;
                let mut child = temp.clone();
                for part in path.parent().unwrap().components() {
                    child.push(part);
                    fs::set_permissions(&child, fs::Permissions::from_mode(0o700))?;
                }
                fs::write(&to, data)?;
                fs::set_permissions(
                    to,
                    fs::Permissions::from_mode(if *executable { 0o700 } else { 0o600 }),
                )?;
            }
            let hash = crate::nar::hash(&temp).map_err(|e| std::io::Error::other(e.to_string()))?;
            fs::rename(&temp, dir)?;
            Ok::<_, std::io::Error>(hash)
        })();
        let _ = fs::remove_dir_all(&temp);
        let nar_hash = result.map_err(|e| fail("E_FETCH", format!("materialise checkout: {e}")))?;
        Ok(Checkout {
            commit: commit_id,
            tree: dir.clone(),
            nar_hash,
        })
    }
}
fn needed<'a>(
    objects: &'a BTreeMap<String, Object>,
    id: &str,
    kind: u8,
    used: &mut BTreeSet<String>,
) -> Result<&'a Object, GitError> {
    let obj = objects
        .get(id)
        .filter(|obj| obj.kind == kind)
        .ok_or_else(|| {
            fail(
                "E_HASH_MISMATCH",
                format!("missing or mismatched git object {id}"),
            )
        })?;
    used.insert(id.to_owned()); // Git permits identical blobs (and subtrees) at several paths.
    Ok(obj)
}
struct WalkState<'a> {
    used: &'a mut BTreeSet<String>,
    files: &'a mut Vec<(PathBuf, Vec<u8>, bool)>,
    total: &'a mut usize,
    limits: &'a Limits,
}
fn walk(
    objects: &BTreeMap<String, Object>,
    id: &str,
    prefix: &Path,
    state: &mut WalkState<'_>,
    depth: usize,
) -> Result<(), GitError> {
    if depth > 128 {
        return Err(fail("E_PATH_ESCAPE", "tree exceeds 128 directory levels"));
    }
    let tree = needed(objects, id, 2, state.used)?;
    let mut pos = 0usize;
    let mut names = BTreeSet::new();
    while pos < tree.data.len() {
        let rest = &tree.data[pos..];
        let space = rest
            .iter()
            .position(|b| *b == b' ')
            .ok_or_else(|| fail("E_HASH_MISMATCH", "malformed tree mode"))?;
        let nul = rest
            .iter()
            .position(|b| *b == 0)
            .ok_or_else(|| fail("E_HASH_MISMATCH", "malformed tree name"))?;
        if nul < space || rest.len() < nul + 21 {
            return Err(fail("E_HASH_MISMATCH", "truncated tree object"));
        }
        let mode = &rest[..space];
        let name = std::str::from_utf8(&rest[space + 1..nul])
            .map_err(|_| fail("E_PATH_ESCAPE", "non-UTF-8 tree name"))?;
        if !crate::hostscope::source::safe_component(name) || name.eq_ignore_ascii_case(".git") {
            return Err(fail(
                "E_PATH_ESCAPE",
                format!("unsafe tree component {name:?}"),
            ));
        }
        if !names.insert(name) {
            return Err(fail(
                "E_PATH_ESCAPE",
                format!("duplicate tree component {name}"),
            ));
        }
        let path = prefix.join(name);
        let child: String = rest[nul + 1..nul + 21]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        match mode {
            b"40000" | b"040000" => walk(objects, &child, &path, state, depth + 1)?,
            b"100644" | b"100755" => {
                let blob = needed(objects, &child, 3, state.used)?;
                if blob.data.len() > state.limits.max_object_bytes
                    || ((name == "host.toml" || name == "home.toml")
                        && blob.data.len() > 1024 * 1024)
                {
                    return Err(fail(
                        "E_PATH_ESCAPE",
                        format!("file {} exceeds its size cap", path.display()),
                    ));
                }
                *state.total = state
                    .total
                    .checked_add(blob.data.len())
                    .ok_or_else(|| fail("E_PATH_ESCAPE", "tree size overflow"))?;
                if *state.total > state.limits.max_tree_bytes {
                    return Err(fail("E_PATH_ESCAPE", "tree exceeds whole-tree cap"));
                }
                state
                    .files
                    .push((path, blob.data.clone(), mode == b"100755"));
            }
            b"120000" => {
                return Err(fail(
                    "E_PATH_ESCAPE",
                    format!("symbolic link {} is refused", path.display()),
                ));
            }
            b"160000" => {
                return Err(fail(
                    "E_UNSUPPORTED",
                    format!("submodule {} is refused", path.display()),
                ));
            }
            _ => {
                return Err(fail(
                    "E_UNSUPPORTED",
                    format!(
                        "git mode {} at {} is unsupported",
                        String::from_utf8_lossy(mode),
                        path.display()
                    ),
                ));
            }
        }
        pos += nul + 21;
    }
    Ok(())
}
