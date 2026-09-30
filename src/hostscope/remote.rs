//! A host from a git URL (LD-401, DONE D6): `lodi host plan|apply URL [--host NAME] [--ref NAME |
//! --rev COMMIT | --refresh]`.
//!
//! - **Grammar, before any request.** `git+https://HOST[:PORT]/PATH`, and the public aliases
//!   `github:`, `gitlab:` and `codeberg:` `OWNER/REPO`, which expand to the canonical HTTPS URLs
//!   LD-400 measured (with `.git`, which GitLab redirects to). Every other scheme, an scp-like
//!   `user@host:path` and credentials are `E_INSECURE_URL`; `?`, `#`, an empty path, a `.` or `..`
//!   segment or a byte outside `[A-Za-z0-9._~/+-]` is `E_UNSUPPORTED` ([`parse`]).
//! - **The verified cache.** A commit's tree lives at
//!   `<root>/var/lib/lodi/host/git/<sha256 of the URL>/<commit>/`: directories 0700, files 0600
//!   (0700 if executable), owned by who fetched it — root on `/`, where nobody else is trusted, and
//!   root or the invoker under a scratch `--root` (LD-357). It is staged in a temporary sibling and
//!   renamed into place, its NAR is hashed again at every read (`E_HASH_MISMATCH`), and it is then
//!   read as a host directory by [`source::select`]'s descriptor walk. After an apply every other
//!   tree is pruned ([`prune`]); the cache is not a record.
//! - **The lock.** host.lock's git table names the URL, the ref asked for, the commit and its NAR
//!   (schema 3). A re-apply of the same URL and ref uses the locked commit and asks nobody when the
//!   tree is cached; `--refresh` resolves again, `--rev` names the commit. Offline, anything that
//!   must resolve is `E_FETCH`, with nothing written.
//!
//! The transport is `crate::git::HttpGitRemote` over `crate::fetch` (HTTPS only, or loopback HTTP
//! through `LODI_FETCH_REWRITE`); there is no credential helper and no token variable.

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;
use crate::git::{GitError, GitRemote, HttpGitRemote, Limits};

use super::lock::{GitRecord, HostLock};
use super::safety::{Gate, Options, STATE};
use super::source;

/// The cache of fetched trees, below the root.
pub const CACHE: &str = "var/lib/lodi/host/git";

/// The public aliases and the host each expands to (LD-400 G1).
const ALIASES: [(&str, &str); 3] = [
    ("github:", "github.com"),
    ("gitlab:", "gitlab.com"),
    ("codeberg:", "codeberg.org"),
];

const PUBLIC_ONLY: &str = "only public git+https:// repositories, and the github:, gitlab: and \
                           codeberg: aliases of public repositories, are applied; for a private \
                           repository, clone it and apply the checkout";

/// A URL the grammar accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// `git+https://host[:port]/path`, the host lowercased: what the lock and the fence name.
    pub canonical: String,
    /// The same without `git+`: what is asked.
    pub https: String,
}

/// Whether `source` is a URL rather than a path (F-7): it names a scheme, is scp-like, or is an
/// alias. `./github:x` and every absolute path are paths.
pub fn is_url(source: &OsStr) -> bool {
    let Some(text) = source.to_str() else {
        return false;
    };
    if text.starts_with('/') || text.starts_with("./") || text.starts_with("../") {
        return false;
    }
    text.starts_with("git+")
        || text.contains("://")
        || ALIASES.iter().any(|(alias, _)| text.starts_with(alias))
        || scp_like(text)
}

/// `user@host:path` or `host:path`: a colon before any slash, which git reads as SSH.
fn scp_like(text: &str) -> bool {
    text.find(':')
        .is_some_and(|colon| !text[..colon].contains('/'))
}

fn insecure(text: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_INSECURE_URL",
        format!("{text} is not an HTTPS URL Lodi applies: {why}; nothing was fetched"),
    )
    .hint(PUBLIC_ONLY)
}

fn unsupported(text: &str, why: String) -> Diagnostic {
    Diagnostic::new(
        "E_UNSUPPORTED",
        format!("{text}: {why}; nothing was fetched"),
    )
    .hint(
        "a URL names one repository: choose a host of it with --host NAME, a branch or tag \
         with --ref NAME, or a commit with --rev COMMIT",
    )
}

/// `?` and `#` are selectors Lodi does not read from a URL; the flags that do their work.
fn selector(text: &str) -> Option<Diagnostic> {
    let (byte, flag) = if text.contains('?') {
        ('?', "--ref NAME or --rev COMMIT")
    } else if text.contains('#') {
        ('#', "--host NAME")
    } else {
        return None;
    };
    Some(unsupported(
        text,
        format!("a URL carries no `{byte}` selector; use {flag}"),
    ))
}

/// Parse `source` as a URL. `Ok(None)` is a path.
pub fn parse(source: &OsStr) -> Result<Option<Url>, Diagnostic> {
    if !is_url(source) {
        return Ok(None);
    }
    let text = source.to_str().unwrap_or_default();
    if let Some((alias, host)) = ALIASES.iter().find(|(a, _)| text.starts_with(a)) {
        return alias_url(text, &text[alias.len()..], host).map(Some);
    }
    let Some(rest) = text.strip_prefix("git+https://") else {
        if let Some(rest) = text.strip_prefix("https://") {
            return Err(unsupported(
                text,
                format!("a git URL is spelled git+https://{rest}"),
            ));
        }
        let why = if text.contains("://") || text.starts_with("git+") {
            "only git+https:// is fetched"
        } else {
            "an scp-like address is SSH, which Lodi does not use"
        };
        return Err(insecure(text, why));
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    if authority.contains('@') {
        return Err(insecure(text, "a URL carries no credentials"));
    }
    if let Some(refused) = selector(text) {
        return Err(refused);
    }
    let (host, port) = authority.split_once(':').unwrap_or((authority, ""));
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        || (authority.contains(':')
            && (port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit())))
    {
        return Err(unsupported(
            text,
            format!("`{authority}` is not a host name and port"),
        ));
    }
    let path = path.trim_end_matches('/');
    check_path(text, path)?;
    let canonical = format!("git+https://{}/{path}", authority.to_ascii_lowercase());
    Ok(Some(Url {
        https: canonical["git+".len()..].to_string(),
        canonical,
    }))
}

/// A URL's path: at least one segment, none empty, `.` or `..`, and every byte in
/// `[A-Za-z0-9._~/+-]`.
fn check_path(text: &str, path: &str) -> Result<(), Diagnostic> {
    if path.is_empty() {
        return Err(unsupported(text, "the URL names no repository path".into()));
    }
    if let Some(byte) = path
        .bytes()
        .find(|b| !(b.is_ascii_alphanumeric() || b"._~/+-".contains(b)))
    {
        let shown = if byte.is_ascii_graphic() || byte == b' ' {
            format!("`{}`", byte as char)
        } else {
            format!("byte 0x{byte:02x}")
        };
        return Err(unsupported(
            text,
            format!("{shown} is not allowed in a repository path"),
        ));
    }
    if let Some(segment) = path
        .split('/')
        .find(|s| s.is_empty() || *s == "." || *s == "..")
    {
        return Err(unsupported(
            text,
            format!("the repository path has a `{segment}` segment"),
        ));
    }
    Ok(())
}

/// `OWNER/REPO` of an alias, expanded to `git+https://HOST/OWNER/REPO.git`.
fn alias_url(text: &str, rest: &str, host: &str) -> Result<Url, Diagnostic> {
    if let Some(refused) = selector(text) {
        return Err(refused);
    }
    let parts: Vec<&str> = rest.split('/').collect();
    if parts.len() != 2 {
        return Err(unsupported(
            text,
            format!(
                "an alias names exactly OWNER/REPO, and this names {} segment(s)",
                parts.len()
            ),
        ));
    }
    let path = format!(
        "{}/{}.git",
        parts[0],
        parts[1].strip_suffix(".git").unwrap_or(parts[1])
    );
    check_path(text, &path)?;
    if parts.iter().any(|p| p.starts_with('.') || p.is_empty()) {
        return Err(unsupported(text, "OWNER and REPO are plain names".into()));
    }
    let canonical = format!("git+https://{host}/{path}");
    Ok(Url {
        https: canonical["git+".len()..].to_string(),
        canonical,
    })
}

/// `pin`, `unpin` and `import` write a host directory, and a fetched tree is never written: given
/// a URL each is refused before anything is read (U8, F-16).
pub fn refuse_writer(verb: &str, options: &Options) -> Result<(), Diagnostic> {
    let Some(source) = &options.source else {
        return Ok(());
    };
    if !matches!(verb, "pin" | "unpin" | "import") || !is_url(source.as_os_str()) {
        return Ok(());
    }
    Err(Diagnostic::new(
        "E_UNSUPPORTED",
        format!(
            "lodi host {verb} writes a host directory, and {} is a URL; nothing was read",
            source.display()
        ),
    )
    .hint(format!(
        "use a checkout: clone the repository, run lodi host {verb} in it, commit and push, then \
         apply the URL with --refresh"
    )))
}

/// The owners the cache may have (U3, LD-357): root on `/`, and root or the invoker under a
/// scratch `--root`.
pub fn owners(system_root: bool, euid: u32) -> Vec<u32> {
    if system_root { vec![0] } else { vec![0, euid] }
}

/// A non-root URL plan on `/` would fetch into a cache only root may write: `E_NEED_ROOT`, before
/// any request (U6, F-14).
pub fn need_root(system_root: bool, euid: u32, url: &Url) -> Result<(), Diagnostic> {
    if !system_root || euid == 0 {
        return Ok(());
    }
    Err(Diagnostic::new(
        "E_NEED_ROOT",
        format!(
            "a plan of {} fetches into a cache only root may write; nothing was fetched",
            url.canonical
        ),
    )
    .hint(format!(
        "run it as root: sudo lodi host plan {}",
        url.canonical
    )))
}

/// Where the source line's commit came from (U6).
#[derive(Debug, Clone, PartialEq, Eq)]
enum State {
    LockedCached,
    LockedFetched,
    Resolved,
    Refresh(String),
}

/// A host read from a URL: the host directory in the cache, the git record the apply locks, and
/// the plan's first line.
pub struct Fetched {
    pub host: source::Host,
    pub git: GitRecord,
    pub line: String,
}

fn git_error(url: &Url, error: GitError) -> Diagnostic {
    let mut diagnostic =
        Diagnostic::new(error.code, format!("{}: {}", url.canonical, error.message));
    if ["401", "403", "404"]
        .iter()
        .any(|s| error.message.contains(&format!("HTTP status {s}")))
    {
        diagnostic = diagnostic.hint(format!(
            "the server refused the repository or does not have it; {PUBLIC_ONLY}"
        ));
    }
    diagnostic
}

/// Resolve, fetch and verify the tree `url` names for `options` against `record`, and select the
/// host in it. Writes nothing but a verified tree into the cache.
pub fn load(
    gate: &Gate,
    options: &Options,
    url: &Url,
    record: Option<&HostLock>,
) -> Result<Fetched, Diagnostic> {
    need_root(gate.system_root, gate.euid, url)?;
    let asked = options.git_ref.clone().unwrap_or_else(|| "HEAD".into());
    let locked = record
        .and_then(|r| r.git.as_ref())
        .filter(|git| git.url == url.canonical && git.reference == asked);
    let remote = || -> Result<HttpGitRemote, Diagnostic> {
        Ok(HttpGitRemote {
            fetcher: crate::fetch::HttpFetcher::from_env()
                .map_err(|error| Diagnostic::new("E_CONFIG", error))?,
        })
    };
    let resolve = || -> Result<String, Diagnostic> {
        remote()?
            .ls_refs(&url.https)
            .and_then(|refs| refs.resolve(Some(&asked)))
            .map_err(|e| git_error(url, e))
    };
    let (commit, state) = match &options.rev {
        Some(rev) => {
            if rev.len() != 40 || !rev.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(unsupported(
                    rev,
                    "--rev names a commit by its 40 hexadecimal digits".into(),
                ));
            }
            let rev = rev.to_ascii_lowercase();
            let state = match locked {
                Some(git) if git.rev == rev => State::LockedCached,
                _ => State::Resolved,
            };
            (rev, state)
        }
        None if options.refresh => {
            let new = resolve()?;
            match locked {
                Some(git) => (new, State::Refresh(git.rev.clone())),
                None => (new, State::Resolved),
            }
        }
        None => match locked {
            Some(git) => (git.rev.clone(), State::LockedCached),
            None => (resolve()?, State::Resolved),
        },
    };
    // The NAR the tree must have: the locked one, when the commit is the locked commit.
    let expected = locked
        .filter(|git| git.rev == commit)
        .map(|git| git.nar_hash.clone());
    let dir = gate
        .root
        .join(CACHE)
        .join(crate::util::sha256_hex(url.canonical.as_bytes()));
    let tree = dir.join(&commit);
    let mut state = state;
    let nar_hash = if fs::symlink_metadata(&tree).is_ok() && expected.is_some() {
        verify(&tree, expected.as_deref())?
    } else {
        // Anything not both locked and cached is fetched, verified, and compared with what the
        // cache already holds of it.
        if state == State::LockedCached {
            state = State::LockedFetched;
        }
        fetch(gate, url, &dir, &commit, expected.as_deref(), &remote)?
    };
    let owners = owners(gate.system_root, gate.euid);
    let chosen = Options {
        source: Some(tree.clone()),
        host: options.host.clone(),
        ..Options::default()
    };
    let shown = tree.display().to_string();
    let mut host = source::select(gate, &chosen, &owners, false).map_err(|mut d| {
        d.message = format!(
            "{} (commit {commit}, verified; {} git requests)",
            d.message.replace(&shown, &url.canonical),
            crate::fetch::HttpFetcher::git_requests()
        );
        d
    })?;
    let name = if host.dir == tree {
        "(top level)".to_string()
    } else {
        host.dir
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned()
    };
    host.source = url.canonical.clone();
    // The fetched tree is the repository: its root `lodi.lock` is read as a checkout's is, and
    // never written (LD-416, F-23).
    host.named = Some(tree.clone());
    let said = match &state {
        State::LockedCached => "locked, no request".to_string(),
        State::LockedFetched => "locked, fetched".to_string(),
        State::Resolved => "resolved, not locked yet".to_string(),
        State::Refresh(old) => format!("refresh {old} -> {commit}"),
    };
    Ok(Fetched {
        host,
        line: format!(
            "source {} host {name} ref {asked} commit {commit}: {said}",
            url.canonical
        ),
        git: GitRecord {
            url: url.canonical.clone(),
            reference: asked,
            rev: commit,
            nar_hash,
        },
    })
}

/// The NAR of the cached `tree`, which must be `expected` when one is known (F-12).
fn verify(tree: &Path, expected: Option<&str>) -> Result<String, Diagnostic> {
    let got = crate::nar::hash(tree).map_err(|e| {
        Diagnostic::new(
            "E_HASH_MISMATCH",
            format!("the cached tree {} cannot be hashed: {e}", tree.display()),
        )
    })?;
    match expected {
        Some(expected) if expected != got => Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "the cached tree {} does not have the NAR the lock records: expected \
                 {expected}, got {got}",
                tree.display()
            ),
        )
        .hint("the cache is not a record: remove that tree and apply again to fetch it")),
        _ => Ok(got),
    }
}

/// Fetch `commit` into a staging sibling of its place in `dir`, check its NAR against `expected`
/// and against a tree already cached, and rename it into place. A mismatch stores nothing.
fn fetch(
    gate: &Gate,
    url: &Url,
    dir: &Path,
    commit: &str,
    expected: Option<&str>,
    remote: &dyn Fn() -> Result<HttpGitRemote, Diagnostic>,
) -> Result<String, Diagnostic> {
    let remote = remote()?;
    for path in [format!("/{STATE}"), format!("/{CACHE}")] {
        super::files::ensure_dir_trusted(&gate.root, &path, 0o700)?;
    }
    let cached = format!(
        "/{CACHE}/{}",
        dir.file_name().unwrap_or_default().to_string_lossy()
    );
    super::files::ensure_dir_trusted(&gate.root, &cached, 0o700)?;
    let staged = dir.join(format!(".{commit}-{}", std::process::id()));
    let _ = fs::remove_dir_all(&staged);
    let checkout = remote
        .fetch_commit(&url.https, commit, &Limits::at(staged.clone()))
        .map_err(|e| git_error(url, e))?;
    let result = (|| {
        let got = verify(&checkout.tree, None)?;
        if let Some(expected) = expected
            && expected != got
        {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!(
                    "{} at {commit} does not have the NAR the lock records: expected {expected}, \
                     got {got}; nothing was stored",
                    url.canonical
                ),
            )
            .hint(
                "the lock names another tree for this commit; apply with --refresh to lock again",
            ));
        }
        let tree = dir.join(commit);
        if fs::symlink_metadata(&tree).is_ok() {
            verify(&tree, Some(&got))?;
            return Ok(got);
        }
        fs::rename(&checkout.tree, &tree).map_err(|e| {
            Diagnostic::new(
                "E_STORE_IO",
                format!("cannot place the fetched tree at {}: {e}", tree.display()),
            )
        })?;
        Ok(got)
    })();
    let _ = fs::remove_dir_all(&staged);
    result
}

/// After an apply that locked `kept`, remove every other cached tree (F-11). Nothing is touched
/// when nothing else is there, so an unchanged apply moves no mtime.
pub fn prune(root: &Path, kept: &GitRecord) {
    let cache = root.join(CACHE);
    let keep = crate::util::sha256_hex(kept.url.as_bytes());
    for (dir, name) in entries(&cache) {
        if name != keep {
            remove(&dir);
            continue;
        }
        for (tree, commit) in entries(&dir) {
            if commit != kept.rev {
                remove(&tree);
            }
        }
    }
}

fn entries(dir: &Path) -> Vec<(PathBuf, String)> {
    fs::read_dir(dir)
        .map(|found| {
            found
                .flatten()
                .map(|e| (e.path(), e.file_name().to_string_lossy().into_owned()))
                .collect()
        })
        .unwrap_or_default()
}

fn remove(path: &Path) {
    // A link or a file is removed itself, never followed.
    match fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => drop(fs::remove_dir_all(path)),
        Ok(_) => drop(fs::remove_file(path)),
        Err(_) => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn url(text: &str) -> Result<Option<Url>, Diagnostic> {
        parse(OsStr::new(text))
    }

    #[test]
    fn paths_are_paths_and_urls_are_urls() {
        for path in ["./github:x", "/srv/hosts", "hosts", "../hosts", "./a:b"] {
            assert_eq!(url(path).unwrap(), None, "{path}");
        }
        let expanded = url("github:owner/repo").unwrap().unwrap();
        assert_eq!(expanded.canonical, "git+https://github.com/owner/repo.git");
        assert_eq!(
            url("gitlab:owner/repo.git").unwrap().unwrap().https,
            "https://gitlab.com/owner/repo.git"
        );
        let direct = url("git+https://Git.Example.ORG:8443/Team/Hosts.git/").unwrap();
        assert_eq!(
            direct.unwrap().canonical,
            "git+https://git.example.org:8443/Team/Hosts.git"
        );
    }

    #[test]
    fn refusals_carry_their_codes() {
        for (text, code) in [
            ("git+http://h/x", "E_INSECURE_URL"),
            ("git+ssh://h/x", "E_INSECURE_URL"),
            ("git+file:///x", "E_INSECURE_URL"),
            ("ssh://h/x", "E_INSECURE_URL"),
            ("host:x", "E_INSECURE_URL"),
            ("git+https://u@h/x", "E_INSECURE_URL"),
            ("https://h/x", "E_UNSUPPORTED"),
            ("git+https://h/x?y", "E_UNSUPPORTED"),
            ("git+https://h:/x", "E_UNSUPPORTED"),
            ("git+https://h/a//b", "E_UNSUPPORTED"),
            ("github:a/b/c", "E_UNSUPPORTED"),
            ("github:a", "E_UNSUPPORTED"),
            ("github:./b", "E_UNSUPPORTED"),
        ] {
            assert_eq!(url(text).unwrap_err().code, code, "{text}");
        }
    }

    /// U3 and U6's synthetic root-owner decisions: on `/` only root owns the cache and only root
    /// plans a URL; under a scratch root the invoker does too.
    #[test]
    fn only_root_is_trusted_and_plans_on_the_system_root() {
        assert_eq!(owners(true, 0), vec![0]);
        assert_eq!(owners(true, 1000), vec![0]);
        assert_eq!(owners(false, 1000), vec![0, 1000]);
        let u = url("github:o/r").unwrap().unwrap();
        assert_eq!(need_root(true, 1000, &u).unwrap_err().code, "E_NEED_ROOT");
        assert!(need_root(true, 0, &u).is_ok() && need_root(false, 1000, &u).is_ok());
    }
}
