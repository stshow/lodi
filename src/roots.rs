//! The one place that reads the environment for a directory Lodi may use (design call D1,
//! `LD-97`).
//!
//! Three roots, computed once:
//!
//! ```text
//! home    $HOME                          the directory the home scope manages
//! config  $XDG_CONFIG_HOME/lodi          else $HOME/.config/lodi   home.toml, home.lock, trust
//! data    $LODI_HOME                     else $XDG_DATA_HOME/lodi  else $HOME/.local/share/lodi
//! ```
//!
//! [`capture`] is the **one** acquisition point: the marked span it sits in is the only part of
//! the tree that names `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` or `LODI_HOME` or reads one,
//! and everything else — [`Roots`], the store's root and the trust store's directory alike — is
//! a **projection** of the [`EnvRoots`] it returns. `tests/home_containment.rs` lints the tree
//! for a second reader, and since M-0.5 `validator-repair-1` it lints `src/roots.rs` itself too:
//! a read outside that span fails the gate wherever it is written (`LD-184`).
//!
//! There is no password-database fallback and no crate that hides one: an unset `HOME`
//! is `E_CONFIG` (exit 3), never `getpwuid(getuid())->pw_dir`, because the whole safety argument
//! of the home scope is that one variable moves the directory a test writes into (`LD-97`).
//!
//! A [`Root`] is the token [`crate::home::fsops`] needs to touch the filesystem at all. Its field
//! is private and the only constructors are the three methods below, so a path that did not come
//! from a root of this module cannot be handed to the writer.

use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

/// The hint an unusable `HOME` carries.
const HOME_HINT: &str = "set HOME to the directory lodi should manage";

/// A directory Lodi may write inside, and the only thing [`crate::home::fsops`] accepts. It is
/// always absolute, because every constructor is a root this module computed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root(PathBuf);

impl Root {
    pub fn path(&self) -> &Path {
        &self.0
    }
}

/// The decision of the home scope's privilege guard (M-1.0 S-1), separated from the syscalls
/// that feed it so that the refusal is testable without a root process. A process with an
/// effective uid of 0 manages a home only when both `HOME` and the configuration root are owned
/// by uid 0: `HOME` and `XDG_CONFIG_HOME` are independently steerable (`sudo -E`, a `SETENV`
/// sudoers rule, a root job that inherits a user's `HOME`), and a root process that trusted them
/// absolutely would write root-owned files, at the manifest author's modes, into another user's
/// home. Any other effective uid is untouched: the guard adds nothing for an ordinary user.
pub fn privilege_guard(
    euid: u32,
    home_uid: u32,
    config_uid: u32,
    home: &Path,
    config: &Path,
) -> Result<(), Diagnostic> {
    if euid != 0 || (home_uid == 0 && config_uid == 0) {
        return Ok(());
    }
    let (what, path, uid) = if home_uid != 0 {
        ("the home directory", home, home_uid)
    } else {
        ("the configuration directory", config, config_uid)
    };
    Err(Diagnostic::new(
        "E_CONFIG",
        format!(
            "this process is root, but {what} `{}` is owned by uid {uid}: lodi manages a home \
             as root only when HOME and the configuration directory are root's own",
            path.display()
        ),
    )
    .hint("run lodi home as the user whose home it is, without sudo"))
}

/// The uid that owns `path`, or its deepest existing ancestor when it does not exist yet.
fn owner_of_deepest_existing(path: &Path) -> Result<u32, Diagnostic> {
    use std::os::unix::fs::MetadataExt;
    let mut at = path;
    loop {
        match std::fs::metadata(at) {
            Ok(meta) => return Ok(meta.uid()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => match at.parent() {
                Some(parent) => at = parent,
                None => {
                    return Err(Diagnostic::new(
                        "E_CONFIG",
                        format!("`{}` has no existing ancestor", path.display()),
                    ));
                }
            },
            Err(e) => {
                return Err(Diagnostic::new(
                    "E_CONFIG",
                    format!("cannot stat `{}`: {e}", at.display()),
                ));
            }
        }
    }
}

/// The three roots of one process, read from the environment exactly once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Roots {
    home: PathBuf,
    config: PathBuf,
    data: PathBuf,
    /// The XDG configuration directory the configuration root is `lodi/` below:
    /// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset. `[home.xdg_config]` and the program
    /// modules resolve their paths below it (M-Home, `docs/design/HOME_PROGRAMS.md` §4.1).
    xdg_config: PathBuf,
}

impl Roots {
    /// Roots for a host-directory home, from its passwd entry and selected manifest directory.
    /// The caller has already confined both paths to its selected root.
    pub fn for_host(home: PathBuf, config: PathBuf) -> Roots {
        let xdg_config = home.join(".config");
        let data = home.join(".local/share/lodi");
        Roots {
            home,
            config,
            data,
            xdg_config,
        }
    }

    /// The three roots of the process's own environment: [`capture`] once, then the projection
    /// [`EnvRoots::roots`]. `HOME` must be set, absolute and a directory; a relative value in any
    /// of the four is `E_CONFIG` rather than a silent fallback.
    pub fn from_env() -> Result<Roots, Diagnostic> {
        let roots = capture().roots()?;
        roots.check_privilege()?;
        Ok(roots)
    }

    /// The thin syscall wrapper around [`privilege_guard`]: the effective uid of this process
    /// and the owners of `HOME` and of the configuration root (its deepest existing ancestor,
    /// when it does not exist yet). An unprivileged process never reaches a `stat`.
    pub fn check_privilege(&self) -> Result<(), Diagnostic> {
        // SAFETY: geteuid cannot fail and takes no arguments.
        let euid = unsafe { libc::geteuid() };
        if euid != 0 {
            return Ok(());
        }
        let home_uid = owner_of_deepest_existing(&self.home)?;
        let config_uid = owner_of_deepest_existing(&self.config)?;
        privilege_guard(euid, home_uid, config_uid, &self.home, &self.config)
    }

    /// [`Roots::from_env`] over values given rather than read, so that the refusals above are
    /// testable without a process ever changing its own environment. An empty variable is an
    /// unset one, which is what [`var`] has already applied to the real environment.
    pub fn from_vars(
        home: Option<&OsStr>,
        config: Option<&OsStr>,
        data: Option<&OsStr>,
        lodi_home: Option<&OsStr>,
    ) -> Result<Roots, Diagnostic> {
        let home = match home.filter(|v| !v.is_empty()) {
            Some(home) => PathBuf::from(home),
            None => {
                return Err(Diagnostic::new(
                    "E_CONFIG",
                    "HOME is not set, and lodi never looks a home directory up in the password \
                     database",
                )
                .hint(HOME_HINT));
            }
        };
        if !home.is_absolute() {
            return Err(Diagnostic::new(
                "E_CONFIG",
                format!(
                    "HOME is `{}`, which is not an absolute path",
                    home.display()
                ),
            )
            .hint(HOME_HINT));
        }
        if !home.is_dir() {
            return Err(Diagnostic::new(
                "E_CONFIG",
                format!("HOME is `{}`, which is not a directory", home.display()),
            )
            .hint(HOME_HINT));
        }
        let xdg_config = match absolute(VAR_CONFIG, config)? {
            Some(dir) => dir,
            None => home.join(".config"),
        };
        let config = xdg_config.join("lodi");
        let data = match absolute(VAR_LODI_HOME, lodi_home)? {
            Some(dir) => dir,
            None => match absolute(VAR_DATA, data)? {
                Some(dir) => dir.join("lodi"),
                None => home.join(".local/share/lodi"),
            },
        };
        Ok(Roots {
            home,
            config,
            data,
            xdg_config,
        })
    }

    pub fn home(&self) -> &Path {
        &self.home
    }

    pub fn config(&self) -> &Path {
        &self.config
    }

    pub fn with_config(&self, config: PathBuf) -> Roots {
        let mut roots = self.clone();
        roots.config = config;
        roots
    }

    pub fn data(&self) -> &Path {
        &self.data
    }

    /// `$XDG_CONFIG_HOME`, or `~/.config` when it is unset: the precedence the configuration
    /// root already follows, one directory up.
    pub fn xdg_config(&self) -> &Path {
        &self.xdg_config
    }

    /// The home directory as a writable root.
    pub fn home_root(&self) -> Root {
        Root(self.home.clone())
    }

    /// The configuration directory as a writable root.
    pub fn config_root(&self) -> Root {
        Root(self.config.clone())
    }

    /// The data directory as a writable root.
    pub fn data_root(&self) -> Root {
        Root(self.data.clone())
    }
}

/// The four root variables of one process, read **once** by [`capture`] and never read again.
///
/// Every consumer of a root in this tree takes one of the projections below rather than the
/// environment, so the three precedence rules cannot drift apart: `Roots` for the home scope,
/// [`EnvRoots::store_root`] for [`crate::store::Store::home_from_env`] and
/// [`EnvRoots::trust_dir`] for [`crate::trust::TrustStore::from_env`]. The projections are
/// deliberately **not** one rule: a store that `LODI_HOME` names needs no `HOME` at all, and
/// tightening that here would change an existing exit status (M-0.5 T-1, kept by
/// `validator-repair-1`).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EnvRoots {
    home: Option<OsString>,
    config: Option<OsString>,
    data: Option<OsString>,
    lodi_home: Option<OsString>,
}

// ---------------------------------------------------------- the one acquisition point ---
// Everything between this marker and its `end` is the only part of the tree that names the four
// root variables or reads one. `tests/home_containment.rs` reads this file, takes exactly this
// span, and fails on a name or a `var(` call anywhere else in it — `src/roots.rs` is no longer
// exempt from its own rule (`LD-184`). The names below are used as *labels* outside the span,
// in diagnostics; what the lint forbids outside it is the reading.

/// The variable that names the directory the home scope manages.
const VAR_HOME: &str = "HOME";
/// The variable that names the configuration directory.
const VAR_CONFIG: &str = "XDG_CONFIG_HOME";
/// The variable that names the data directory.
const VAR_DATA: &str = "XDG_DATA_HOME";
/// The variable that names the store, and with it the data root.
const VAR_LODI_HOME: &str = "LODI_HOME";

/// **The one acquisition point**: the four variables, read once, in one place. An empty variable
/// is an unset one, which is the rule every consumer had before this capture existed, so no
/// precedence and no exit status moves with it.
pub fn capture() -> EnvRoots {
    EnvRoots {
        home: var(VAR_HOME),
        config: var(VAR_CONFIG),
        data: var(VAR_DATA),
        lodi_home: var(VAR_LODI_HOME),
    }
}

// ------------------------------------------------------ end of the one acquisition point ---

impl EnvRoots {
    /// The captured values as they were read, for a caller that wants to project them itself.
    pub fn from_values(
        home: Option<OsString>,
        config: Option<OsString>,
        data: Option<OsString>,
        lodi_home: Option<OsString>,
    ) -> EnvRoots {
        EnvRoots {
            home,
            config,
            data,
            lodi_home,
        }
    }

    /// The home scope's three roots, with every refusal of [`Roots::from_vars`].
    pub fn roots(&self) -> Result<Roots, Diagnostic> {
        Roots::from_vars(
            self.home.as_deref(),
            self.config.as_deref(),
            self.data.as_deref(),
            self.lodi_home.as_deref(),
        )
    }

    /// The store's root, with the precedence and the code
    /// [`crate::store::Store::home_from_env`] has had since S-3: `LODI_HOME`, else
    /// `XDG_DATA_HOME/lodi`, else `$HOME/.local/share/lodi`, and `E_STORE_PERM` when nothing
    /// names one. No value here has to be absolute: `Store::open` is what refuses that, with its
    /// own message.
    pub fn store_root(&self) -> Result<PathBuf, Diagnostic> {
        if let Some(home) = &self.lodi_home {
            return Ok(PathBuf::from(home));
        }
        if let Some(data) = &self.data {
            return Ok(PathBuf::from(data).join("lodi"));
        }
        if let Some(home) = &self.home {
            return Ok(PathBuf::from(home).join(".local/share/lodi"));
        }
        Err(Diagnostic::new(
            "E_STORE_PERM",
            "neither LODI_HOME, XDG_DATA_HOME nor HOME is set; there is no place for the store",
        ))
    }

    /// The configuration directory, with the precedence and the code
    /// [`crate::trust::TrustStore::from_env`] has had since S-3: `XDG_CONFIG_HOME/lodi`, else
    /// `$HOME/.config/lodi`, and `E_CONFIG` when neither is set.
    pub fn trust_dir(&self) -> Result<PathBuf, Diagnostic> {
        if let Some(config) = &self.config {
            return Ok(PathBuf::from(config).join("lodi"));
        }
        if let Some(home) = &self.home {
            return Ok(PathBuf::from(home).join(".config/lodi"));
        }
        Err(Diagnostic::new(
            "E_CONFIG",
            "neither XDG_CONFIG_HOME nor HOME is set; trust cannot be recorded",
        ))
    }
}

/// The write ledger `LODI_FS_LEDGER` names, when it names an absolute path. It is a test and
/// debugging facility (`AGENTS.md` §5, design call D4 / `LD-100`), inert when the variable is
/// unset, and it is the one path [`crate::home::fsops`] appends to outside a [`Root`]. A
/// relative value is ignored rather than an error: the ledger never changes what a command does.
pub fn ledger_path() -> Option<PathBuf> {
    var("LODI_FS_LEDGER")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

/// A set, non-empty environment variable. Every read of the environment in `src/` is here.
fn var(name: &str) -> Option<OsString> {
    std::env::var_os(name).filter(|v| !v.is_empty())
}

/// A set, non-empty variable that must be an absolute path.
fn absolute(name: &str, value: Option<&OsStr>) -> Result<Option<PathBuf>, Diagnostic> {
    match value.filter(|v| !v.is_empty()) {
        None => Ok(None),
        Some(value) => {
            let path = PathBuf::from(value);
            if path.is_absolute() {
                Ok(Some(path))
            } else {
                Err(Diagnostic::new(
                    "E_CONFIG",
                    format!(
                        "{name} is `{}`, which is not an absolute path",
                        path.display()
                    ),
                )
                .hint(format!("set {name} to an absolute path, or unset it")))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn osstring(s: &str) -> OsString {
        OsString::from(s)
    }

    #[test]
    fn the_defaults_hang_off_home() {
        let home = osstring("/");
        let roots = Roots::from_vars(Some(&home), None, None, None).unwrap();
        assert_eq!(roots.home(), Path::new("/"));
        assert_eq!(roots.config(), Path::new("/.config/lodi"));
        assert_eq!(roots.data(), Path::new("/.local/share/lodi"));
    }

    #[test]
    fn lodi_home_wins_over_xdg_data_home() {
        let home = osstring("/");
        let data = osstring("/d");
        let lodi_home = osstring("/l");
        let roots = Roots::from_vars(Some(&home), None, Some(&data), Some(&lodi_home)).unwrap();
        assert_eq!(roots.data(), Path::new("/l"));
        let roots = Roots::from_vars(Some(&home), None, Some(&data), None).unwrap();
        assert_eq!(roots.data(), Path::new("/d/lodi"));
    }

    fn captured(
        home: Option<&str>,
        config: Option<&str>,
        data: Option<&str>,
        lodi: Option<&str>,
    ) -> EnvRoots {
        let own = |v: Option<&str>| v.map(OsString::from);
        EnvRoots::from_values(own(home), own(config), own(data), own(lodi))
    }

    /// The store's projection keeps S-3's precedence and its `E_STORE_PERM` exactly: a store
    /// `LODI_HOME` names needs no `HOME`, and a relative value is still not this function's
    /// refusal to make.
    #[test]
    fn the_store_projection_is_the_precedence_it_always_had() {
        let all = captured(Some("/h"), Some("/c"), Some("/d"), Some("/l"));
        assert_eq!(all.store_root().unwrap(), PathBuf::from("/l"));
        assert_eq!(
            captured(Some("/h"), None, Some("/d"), None)
                .store_root()
                .unwrap(),
            PathBuf::from("/d/lodi")
        );
        assert_eq!(
            captured(Some("/h"), None, None, None).store_root().unwrap(),
            PathBuf::from("/h/.local/share/lodi")
        );
        assert_eq!(
            captured(None, Some("/c"), None, Some("relative"))
                .store_root()
                .unwrap(),
            PathBuf::from("relative"),
            "the store's root is not required to be absolute here"
        );
        let d = captured(None, Some("/c"), None, None)
            .store_root()
            .unwrap_err();
        assert_eq!(d.code, "E_STORE_PERM");
    }

    /// The trust store's projection likewise: `XDG_CONFIG_HOME/lodi`, else `$HOME/.config/lodi`,
    /// else `E_CONFIG` — and `LODI_HOME` has never had a say in it.
    #[test]
    fn the_trust_projection_is_the_precedence_it_always_had() {
        assert_eq!(
            captured(Some("/h"), Some("/c"), None, Some("/l"))
                .trust_dir()
                .unwrap(),
            PathBuf::from("/c/lodi")
        );
        assert_eq!(
            captured(Some("/h"), None, None, Some("/l"))
                .trust_dir()
                .unwrap(),
            PathBuf::from("/h/.config/lodi")
        );
        let d = captured(None, None, Some("/d"), Some("/l"))
            .trust_dir()
            .unwrap_err();
        assert_eq!(d.code, "E_CONFIG");
    }

    /// The home scope's projection is `Roots::from_vars` over the same captured values, so one
    /// capture answers all three questions.
    #[test]
    fn one_capture_answers_all_three_questions() {
        let captured = captured(Some("/"), Some("/c"), Some("/d"), Some("/l"));
        let roots = captured.roots().unwrap();
        assert_eq!(roots.home(), Path::new("/"));
        assert_eq!(roots.config(), Path::new("/c/lodi"));
        assert_eq!(roots.data(), Path::new("/l"));
        assert_eq!(captured.store_root().unwrap(), PathBuf::from("/l"));
        assert_eq!(captured.trust_dir().unwrap(), PathBuf::from("/c/lodi"));
    }

    #[test]
    fn a_root_is_only_ever_one_of_the_three() {
        let home = osstring("/");
        let roots = Roots::from_vars(Some(&home), None, None, None).unwrap();
        assert_eq!(roots.home_root().path(), roots.home());
        assert_eq!(roots.config_root().path(), roots.config());
        assert_eq!(roots.data_root().path(), roots.data());
    }
}
