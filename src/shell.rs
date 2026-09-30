//! `lodi shell` — one tool, or a set of distro packages, in one interactive shell, with nothing
//! installed permanently (M-0.3 T-2; design `spec/10-cli` §4, `spec/03` §1, OD-16, ADR-015,
//! ADR-016; design calls D3, D4 and D5 as LD-48, LD-49 and LD-50).
//!
//! ```text
//! lodi shell [--no-nest] TOOL[@CONSTRAINT]…                  host-tool mode
//! lodi shell [--no-nest] --base DISTRO[:RELEASE] PACKAGE…     container mode
//! ```
//!
//! What one invocation does, in order:
//!
//! 1. the request is parsed into an **in-memory** manifest — `[tools]` in host mode,
//!    `[container]` plus `[packages] common` in container mode. `./lodi.toml` is never read and
//!    no file is ever written in the current directory (LD-49);
//! 2. the canonical request is hashed; `<h32>` is the first 32 hex of that sha256 and the
//!    resolution lives in `$LODI_HOME/cache/shell/<h32>.lock`, which `spec/03` §1 reserves for
//!    exactly this. A cached lock that is still fresh for the request is **reused**, so a second
//!    identical `lodi shell` needs no network at all;
//! 3. host mode realizes and enters through [`crate::host::enter_host`], container mode through
//!    [`crate::container::enter`] — the project pipeline unchanged, closure check included
//!    (LD-50). Both root a live session at `gcroots/sessions/<pid>` (ADR-016, never an expiry)
//!    and release the shared store lock before the shell starts;
//! 4. the child is an interactive shell instead of a command: `$SHELL` when it is set and
//!    executable, else `/bin/sh`. Its exit status is passed through unmodified.
//!
//! There is deliberately **no `--` form**: running a command stays `lodi develop -- …` in 0.3.

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};

use serde_json::json;

use crate::catalogue;
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::init::{Base, parse_base};
use crate::lock::{self, Failure, LockFile, canonical_json};
use crate::manifest::{Arch, Container, Distro, Project, ProjectManifest, Tool};
use crate::store::Store;
use crate::util::{now_utc, sha256_tagged};
use crate::version::Constraint;

/// Where an ad-hoc resolution is recorded, below `$LODI_HOME` (`spec/03` §1).
pub const CACHE_DIR: &str = "cache/shell";
/// The versioned protocol of a canonical `lodi shell` request.
pub const REQUEST_FORMAT: &str = "lodi-shell-request/1";
/// `project.name` of the in-memory manifest: no ad-hoc shell takes its name from the directory
/// it was started in, so two shells of the same request share one environment name.
pub const ENVIRONMENT_NAME: &str = "lodi-shell";
/// The shell entered inside a container image: the image's own `/bin/sh`. The caller's `$SHELL`
/// names a program on the host that the image need not contain, and Lodi installs nothing into
/// an image (LD-64).
pub const CONTAINER_SHELL: &str = "/bin/sh";
/// The shell entered on the host when `$SHELL` is unset or not executable.
pub const HOST_SHELL: &str = "/bin/sh";

/// What one `lodi shell` invocation asks for, exactly as it was written.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Request {
    /// `--base DISTRO[:RELEASE]`; absent means host-tool mode.
    pub base: Option<String>,
    /// `TOOL[@CONSTRAINT]` in host mode, package names in container mode; never empty.
    pub items: Vec<String>,
}

/// `[a-z0-9][a-z0-9._+-]{0,127}`, the tool and package name grammar of `spec/01` §3 that
/// [`crate::manifest`] applies to a written manifest. A request is held to the same grammar.
fn is_package_name(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= 128
        && chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-')
        })
}

/// The tool names this invocation resolves, sorted: the built-in catalogue's and those of your
/// own in the repository's `recipes/` folder (LD-409).
pub fn recipe_names() -> Vec<String> {
    use crate::tools::Catalogue;
    catalogue::user::active().unwrap_or_default().names()
}

fn ident(what: &str, name: &str) -> Diagnostic {
    Diagnostic::new(
        "E_IDENT",
        format!("{what} name `{name}` must match ^[a-z0-9][a-z0-9._+-]{{0,127}}$"),
    )
}

/// `TOOL[@CONSTRAINT]` split into its two halves; the constraint defaults to `latest`.
fn split_tool(item: &str) -> (&str, &str) {
    match item.split_once('@') {
        Some((name, constraint)) => (name, constraint),
        None => (item, "latest"),
    }
}

/// What a request resolves against: the in-memory manifest, the canonical request and its
/// `<h32>` cache key.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub manifest: ProjectManifest,
    /// The canonical JSON of the request; its sha256 is the lock's `manifestHash`.
    pub request: String,
    pub base: Option<Base>,
}

impl Resolved {
    /// `sha256:<hex>` of the canonical request.
    pub fn request_hash(&self) -> String {
        sha256_tagged(self.request.as_bytes())
    }

    /// The first 32 hex of that hash: the `<h32>` of `cache/shell/<h32>.lock`.
    pub fn h32(&self) -> String {
        self.request_hash()["sha256:".len().."sha256:".len() + 32].to_string()
    }

    pub fn cache_path(&self, store: &Store) -> PathBuf {
        store
            .home()
            .join(CACHE_DIR)
            .join(format!("{}.lock", self.h32()))
    }
}

/// Turn a request into the manifest it is equivalent to. Nothing is read from the current
/// directory: an ad-hoc shell depends on its arguments and on nothing else (LD-49).
pub fn resolve_request(request: &Request) -> Result<Resolved, Diagnostic> {
    if request.items.is_empty() {
        // The binary refuses this at exit 2 before it gets here; the library states it too.
        return Err(Diagnostic::new(
            "E_IDENT",
            "lodi shell needs at least one tool or package name",
        ));
    }
    let base = match &request.base {
        Some(text) => Some(parse_base(text)?),
        None => None,
    };
    let project = Project {
        schema_version: "1".to_string(),
        name: Some(ENVIRONMENT_NAME.to_string()),
    };
    let (manifest, canonical) = match &base {
        // Container mode (LD-50): `[container] distro/release` + `[packages] common = …`.
        Some(base) => {
            let mut packages: Vec<String> = Vec::new();
            for item in &request.items {
                if !is_package_name(item) {
                    return Err(ident("package", item));
                }
                if !packages.contains(item) {
                    packages.push(item.clone());
                }
            }
            let manifest = ProjectManifest {
                project,
                container: Some(Container {
                    // The distribution the `--base` argument named, not a default: `parse_base`
                    // has already refused anything this build does not support, and discarding
                    // its answer here would resolve every base as the first one.
                    distro: Distro::parse(&base.distro)
                        .expect("parse_base accepts only supported distributions"),
                    release: base.release.clone(),
                    arch: Arch::X86_64,
                    snapshot: None,
                }),
                packages: packages.clone(),
                tools: Default::default(),
                env: Vec::new(),
                tasks: Default::default(),
            };
            let mut sorted = packages;
            sorted.sort();
            let canonical = json!({
                "schema": 1,
                "format": REQUEST_FORMAT,
                "mode": "container",
                "arch": "x86_64",
                "base": { "distro": base.distro, "release": base.release },
                "packages": sorted,
            });
            (manifest, canonical)
        }
        // Host mode: `[tools]`, each name resolved by a built-in recipe and nothing else.
        None => {
            let mut tools = std::collections::BTreeMap::new();
            for item in &request.items {
                let (name, constraint_text) = split_tool(item);
                if !is_package_name(name) {
                    return Err(ident("tool", name));
                }
                let own = catalogue::user::active()?;
                if own.recipe(name).is_none() && catalogue::builtin_recipe(name).is_none() {
                    return Err(Diagnostic::new(
                        "E_NO_RECIPE",
                        format!("no recipe for tool `{name}` in the built-in catalogue"),
                    )
                    .hint(format!(
                        "this build resolves {}; there is no fallback and nothing is guessed",
                        recipe_names().join(", ")
                    ))
                    .hint(format!("a recipe of your own goes in {}", own.place(name))));
                }
                let constraint = Constraint::parse(constraint_text).map_err(|why| {
                    Diagnostic::new(
                        "E_TYPE",
                        format!("invalid version constraint \"{constraint_text}\" for tool `{name}`: {why}"),
                    )
                    .hint("use a prefix such as \"3.12\", \"latest\", or ranges such as \">=3.11, <3.13\"")
                })?;
                if let Some(previous) = tools.insert(
                    name.to_string(),
                    Tool::simple(constraint_text.to_string(), constraint),
                ) && previous.constraint_text != constraint_text
                {
                    return Err(Diagnostic::new(
                        "E_TYPE",
                        format!(
                            "tool `{name}` is asked for twice, as `{}` and as `{constraint_text}`",
                            previous.constraint_text
                        ),
                    ));
                }
            }
            let canonical = json!({
                "schema": 1,
                "format": REQUEST_FORMAT,
                "mode": "host",
                "arch": "x86_64",
                "tools": tools
                    .iter()
                    .map(|(name, tool)| json!({ "name": name, "constraint": tool.constraint_text }))
                    .collect::<Vec<_>>(),
            });
            let manifest = ProjectManifest {
                project,
                container: None,
                packages: Vec::new(),
                tools,
                env: Vec::new(),
                tasks: Default::default(),
            };
            (manifest, canonical)
        }
    };
    Ok(Resolved {
        manifest,
        request: canonical_json(&canonical),
        base,
    })
}

/// The program an interactive shell runs: `shell` when it is set, non-empty and executable,
/// else `fallback`. `E_SHELL_NOT_FOUND` (exit 7) when neither can be run — Lodi never installs
/// a shell.
pub fn shell_program(shell: Option<&OsStr>, fallback: &Path) -> Result<PathBuf, Diagnostic> {
    let runnable = |path: &Path| -> bool {
        // A shell is a program to exec: a regular file (or a link to one) with an execute bit.
        fs::metadata(path).is_ok_and(|m| {
            use std::os::unix::fs::PermissionsExt;
            m.is_file() && m.permissions().mode() & 0o111 != 0
        })
    };
    if let Some(shell) = shell.filter(|s| !s.is_empty()) {
        let path = Path::new(shell);
        if path.is_absolute() && runnable(path) {
            return Ok(path.to_path_buf());
        }
    }
    if runnable(fallback) {
        return Ok(fallback.to_path_buf());
    }
    Err(Diagnostic::new(
        "E_SHELL_NOT_FOUND",
        format!(
            "no usable interactive shell: $SHELL{} and {} cannot be run",
            match shell.filter(|s| !s.is_empty()) {
                Some(s) => format!(" (`{}`)", s.to_string_lossy()),
                None => " (unset)".to_string(),
            },
            fallback.display()
        ),
    )
    .hint("set SHELL to an absolute path of an executable shell; lodi installs no shell"))
}

/// The argv of the interactive child. In a container it is the image's own `/bin/sh` (LD-64);
/// on the host it is [`shell_program`] of `$SHELL` with `/bin/sh` as the fallback.
pub fn interactive_argv(container: bool) -> Result<Vec<OsString>, Diagnostic> {
    if container {
        return Ok(vec![OsString::from(CONTAINER_SHELL)]);
    }
    let shell = std::env::var_os("SHELL");
    let program = shell_program(shell.as_deref(), Path::new(HOST_SHELL))?;
    Ok(vec![program.into_os_string()])
}

/// Read the cached resolution of `resolved`, if there is one that is still fresh for the
/// request. An unreadable or stale file is simply re-resolved; it is a cache, not a lock the
/// user maintains.
pub fn cached(resolved: &Resolved, path: &Path) -> Option<LockFile> {
    let lock = lock::parse_lock(&fs::read(path).ok()?).ok()?;
    lock::staleness(&resolved.manifest, &lock)
        .is_empty()
        .then_some(lock)
}

/// Resolve `resolved` (reusing `path`'s entry where it is still fresh) and write the ad-hoc
/// lock back to `path` atomically.
fn resolve_into_cache(
    resolved: &Resolved,
    path: &Path,
    fetcher: &dyn Fetcher,
) -> Result<LockFile, Failure> {
    let previous = fs::read(path)
        .ok()
        .and_then(|bytes| lock::parse_lock(&bytes).ok());
    let (lock, _) = lock::resolve_manifest(
        &resolved.manifest,
        resolved.request_hash(),
        previous.as_ref(),
        fetcher,
        now_utc(),
    )?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir).map_err(|e| {
            crate::host::fail(Diagnostic::new(
                "E_STORE_IO",
                format!("cannot create {}: {e}", dir.display()),
            ))
        })?;
    }
    lock::write_atomic(path, lock.to_canonical_json().as_bytes()).map_err(|e| {
        crate::host::fail(Diagnostic::new(
            "E_STORE_IO",
            format!("cannot write {}: {e}", path.display()),
        ))
    })?;
    Ok(lock)
}

/// `lodi shell …`: resolve the request ad hoc, enter the environment and return the shell's
/// exit status. `cwd` is where the shell starts; nothing is read from or written to it.
pub fn enter(
    cwd: &Path,
    request: &Request,
    options: &crate::host::Options,
    fetcher: &dyn Fetcher,
) -> Result<u8, Failure> {
    let resolved = resolve_request(request).map_err(crate::host::fail)?;
    let cwd = cwd.canonicalize().map_err(|e| {
        crate::host::fail(Diagnostic::new(
            "E_ENTER",
            format!("{}: {e}", cwd.display()),
        ))
    })?;
    // Entering a shell runs no task text, so the trust gate of ADR-014 has nothing to ask about
    // and `lodi shell` never prompts: the in-memory manifest declares no tasks at all.
    debug_assert!(resolved.manifest.tasks.is_empty());
    let container = resolved.manifest.is_container_mode();
    // The container runtime is looked for before anything is resolved or downloaded: a machine
    // without rootless Podman learns so immediately and offline (ADR-012; lodi installs none).
    if container {
        crate::container::Podman::find(std::env::var_os("PATH").as_deref())
            .map_err(crate::host::fail)?;
    }
    let argv = interactive_argv(container).map_err(crate::host::fail)?;
    let store = Store::open_for_write(&Store::home_from_env().map_err(crate::host::fail)?)
        .map_err(crate::host::fail)?;
    let path = resolved.cache_path(&store);
    let lock = match cached(&resolved, &path) {
        Some(lock) => lock,
        None => resolve_into_cache(&resolved, &path, fetcher)?,
    };
    if container {
        crate::container::enter(&cwd, &resolved.manifest, &lock, &argv, options, fetcher)
    } else {
        crate::host::enter_host(&cwd, &resolved.manifest, &lock, &argv, options, fetcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(base: Option<&str>, items: &[&str]) -> Request {
        Request {
            base: base.map(str::to_string),
            items: items.iter().map(|i| i.to_string()).collect(),
        }
    }

    #[test]
    fn a_host_request_becomes_a_tools_manifest() {
        let r = resolve_request(&request(None, &["python@3.12", "nodejs"])).unwrap();
        assert!(!r.manifest.is_container_mode());
        assert_eq!(r.manifest.tools["python"].constraint_text, "3.12");
        assert_eq!(r.manifest.tools["nodejs"].constraint_text, "latest");
        assert_eq!(r.manifest.project.name.as_deref(), Some(ENVIRONMENT_NAME));
        assert!(r.manifest.tasks.is_empty() && r.manifest.env.is_empty());
    }

    #[test]
    fn the_cache_key_is_the_request_and_only_the_request() {
        let a = resolve_request(&request(None, &["python@3.12", "nodejs"])).unwrap();
        // Argument order does not change the environment; the constraint does.
        let b = resolve_request(&request(None, &["nodejs", "python@3.12"])).unwrap();
        let c = resolve_request(&request(None, &["python@3.13", "nodejs"])).unwrap();
        assert_eq!(a.h32(), b.h32());
        assert_ne!(a.h32(), c.h32());
        assert_eq!(a.h32().len(), 32);
        assert!(a.h32().chars().all(|c| c.is_ascii_hexdigit()));
        // A container request is never the same key as a host request.
        let d = resolve_request(&request(Some("debian:bookworm"), &["make"])).unwrap();
        assert_ne!(a.h32(), d.h32());
    }

    #[test]
    fn a_container_request_becomes_a_container_manifest() {
        let r = resolve_request(&request(Some("debian:12"), &["make", "make", "gcc"])).unwrap();
        assert!(r.manifest.is_container_mode());
        assert_eq!(r.manifest.container.as_ref().unwrap().release, "bookworm");
        assert_eq!(r.manifest.packages, ["make", "gcc"]);
        assert!(r.manifest.tools.is_empty());
        // Duplicates collapse, so `make make` and `make` are the same environment.
        let once = resolve_request(&request(Some("debian:12"), &["gcc", "make"])).unwrap();
        assert_eq!(r.h32(), once.h32());
    }

    #[test]
    fn unknown_tools_and_bad_names_are_refused_without_a_fallback() {
        // A name the catalogue does not have and is not near one it has: the hint then lists
        // every recipe this build ships. (It used to be `go`, which M-0.4 T-4 made a recipe.)
        let d = resolve_request(&request(None, &["notatool"])).unwrap_err();
        assert_eq!(d.code, "E_NO_RECIPE");
        // Every built-in name, sorted and in that order, not two that happen to sort side by side.
        let names = catalogue::builtin_tool_names();
        assert!(names.windows(2).all(|w| w[0] < w[1]), "{names:?}");
        assert!(
            ["nodejs", "python"]
                .iter()
                .all(|n| names.iter().any(|m| m == n)),
            "{names:?}"
        );
        assert!(
            d.to_string()
                .contains(&format!("this build resolves {};", names.join(", "))),
            "{d}"
        );
        assert_eq!(
            resolve_request(&request(None, &["Python"]))
                .unwrap_err()
                .code,
            "E_IDENT"
        );
        assert_eq!(
            resolve_request(&request(None, &["python@not a constraint"]))
                .unwrap_err()
                .code,
            "E_TYPE"
        );
        assert_eq!(
            resolve_request(&request(Some("debian"), &["NOT-A-PACKAGE"]))
                .unwrap_err()
                .code,
            "E_IDENT"
        );
        assert_eq!(
            resolve_request(&request(Some("gentoo"), &["make"]))
                .unwrap_err()
                .code,
            "E_UNSUPPORTED"
        );
    }

    #[test]
    fn the_shell_is_shell_then_bin_sh_then_an_error() {
        let sh = Path::new(HOST_SHELL);
        assert_eq!(shell_program(None, sh).unwrap(), sh);
        assert_eq!(shell_program(Some(OsStr::new("")), sh).unwrap(), sh);
        // A relative or unrunnable $SHELL falls back rather than failing.
        assert_eq!(shell_program(Some(OsStr::new("sh")), sh).unwrap(), sh);
        assert_eq!(
            shell_program(Some(OsStr::new("/nonexistent/shell")), sh).unwrap(),
            sh
        );
        assert_eq!(shell_program(Some(OsStr::new(HOST_SHELL)), sh).unwrap(), sh);
        // Nothing usable at all: E_SHELL_NOT_FOUND, and the hint says lodi installs no shell.
        let d = shell_program(None, Path::new("/nonexistent/fallback")).unwrap_err();
        assert_eq!(d.code, "E_SHELL_NOT_FOUND");
        assert_eq!(crate::diag::exit_status(d.code), crate::diag::EXIT_RUNTIME);
        assert!(d.to_string().contains("installs no shell"), "{d}");
    }
}
