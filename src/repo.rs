//! The repository a top-level verb acts on (LD-447): `lodi plan`, `apply`, `import` and `update`
//! take it from, in order, an explicit path or URL; the current directory, once the user confirms
//! applying what is there; `LODI_REPO`; and otherwise they refuse with a hint.
//!
//! It reads no `HOME` and no XDG variable, runs no `git` and writes nothing: the answer is a
//! `SOURCE`, which the host verbs then read exactly as `lodi host … SOURCE` would (LD-379).
//! Without a terminal the question cannot be asked, so the implicit current directory is refused
//! unless `--yes` answers it, or an explicit path or `LODI_REPO` names another repository.

use std::ffi::{OsStr, OsString};
use std::io::{BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;

/// The environment variable that names a repository when the current directory holds none.
pub const ENV: &str = "LODI_REPO";

/// Where a resolved repository came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Explicit,
    CurrentDirectory,
    Environment,
}

/// The repository one invocation acts on: a directory or a URL, as a host `SOURCE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Repository {
    pub source: PathBuf,
    pub origin: Origin,
}

/// Asks the one question of step 2. [`Terminal`] is the real one.
pub trait Confirm {
    /// Whether a person can be asked: standard input and standard error are both terminals.
    fn interactive(&self) -> bool;
    /// Ask `question`; `true` only for an answer of yes.
    fn ask(&mut self, question: &str) -> bool;
}

/// The process's own terminal: the question on standard error, the answer from standard input.
pub struct Terminal;

impl Confirm for Terminal {
    fn interactive(&self) -> bool {
        std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
    }

    fn ask(&mut self, question: &str) -> bool {
        let mut err = std::io::stderr();
        let _ = write!(err, "{question} [y/N] ");
        let _ = err.flush();
        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes" | "Yes" | "YES")
    }
}

/// Whether `dir` holds a host repository: a `host.toml` of its own (one host), or a directory
/// beside it that has one (a directory of hosts). A directory with `lodi.toml` is a project, whose
/// `lodi.lock` is the project's, and never a host repository.
pub fn holds_repository(dir: &Path) -> bool {
    if dir.join(crate::lock::MANIFEST_FILE).exists() {
        return false;
    }
    if dir.join("host.toml").is_file() {
        return true;
    }
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name();
        !name.to_string_lossy().starts_with('.')
            && entry.file_type().is_ok_and(|kind| kind.is_dir())
            && entry.path().join("host.toml").is_file()
    })
}

/// Whether `dir` holds nothing but hidden entries, such as a fresh `.git`.
fn is_empty(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| {
        entries.all(|entry| {
            entry.is_ok_and(|entry| entry.file_name().as_encoded_bytes().starts_with(b"."))
        })
    })
}

/// The hint every refusal of this module ends with: what resolves a repository.
pub fn hint(verb: &str) -> String {
    format!(
        "go to your repository and run `lodi {verb}` there, pass it (`lodi {verb} ~/lodi`), set \
         {ENV}, or start one with `sudo lodi import ~/lodi`"
    )
}

/// LD-447's order. `explicit` is the positional `SOURCE`, `cwd` the current directory, `env`
/// the value of [`ENV`], `yes` the `--yes` flag. Nothing is read beyond `cwd`'s own entries.
pub fn resolve(
    verb: &str,
    explicit: Option<&OsStr>,
    cwd: &Path,
    env: Option<&OsStr>,
    yes: bool,
    confirm: &mut dyn Confirm,
) -> Result<Repository, Diagnostic> {
    if let Some(explicit) = explicit {
        return Ok(Repository {
            source: PathBuf::from(explicit),
            origin: Origin::Explicit,
        });
    }
    let env: Option<OsString> = env.filter(|value| !value.is_empty()).map(OsStr::to_owned);
    // A first import has no repository to find: an empty directory takes it once `--yes` or the
    // terminal says so, and without either it resolves as any other directory holding none (#294).
    let first_import = verb == "import" && (yes || confirm.interactive()) && is_empty(cwd);
    if first_import || holds_repository(cwd) {
        let here = Repository {
            source: cwd.to_path_buf(),
            origin: Origin::CurrentDirectory,
        };
        if yes {
            return Ok(here);
        }
        if confirm.interactive() {
            let question = if first_import {
                format!(
                    "lodi import: import this machine into the current directory, {}?",
                    cwd.display()
                )
            } else {
                format!(
                    "lodi {verb}: use the configuration in the current directory, {}?",
                    cwd.display()
                )
            };
            if confirm.ask(&question) {
                return Ok(here);
            }
            return Err(Diagnostic::new(
                "E_DECLINED",
                format!(
                    "`lodi {verb}` was not confirmed for {}; nothing was read or written",
                    cwd.display()
                ),
            )
            .hint(format!(
                "answer yes, pass the repository (`lodi {verb} {}`), or give --yes",
                cwd.display()
            )));
        }
        if let Some(env) = env {
            return Ok(Repository {
                source: PathBuf::from(env),
                origin: Origin::Environment,
            });
        }
        return Err(Diagnostic::new(
            "E_DECLINED",
            format!(
                "the current directory {} holds a repository, and with no terminal `lodi {verb}` \
                 cannot ask to use it; nothing was read or written",
                cwd.display()
            ),
        )
        .hint(format!(
            "pass it (`lodi {verb} {}`), give --yes, or set {ENV}",
            cwd.display()
        )));
    }
    if let Some(env) = env {
        return Ok(Repository {
            source: PathBuf::from(env),
            origin: Origin::Environment,
        });
    }
    Err(Diagnostic::new(
        "E_NO_MANIFEST",
        format!(
            "`lodi {verb}` found no repository: none was named, the current directory {} holds \
             none, and {ENV} is not set; nothing was read or written",
            cwd.display()
        ),
    )
    .hint(hint(verb)))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Answer {
        tty: bool,
        yes: bool,
        asked: usize,
    }

    impl Confirm for Answer {
        fn interactive(&self) -> bool {
            self.tty
        }
        fn ask(&mut self, _: &str) -> bool {
            self.asked += 1;
            self.yes
        }
    }

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lodi-repo-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_order_is_explicit_then_confirmed_current_directory_then_environment() {
        let repo = scratch("order");
        std::fs::create_dir_all(repo.join("box")).unwrap();
        std::fs::write(repo.join("box/host.toml"), "").unwrap();
        let empty = scratch("order-empty");
        let env = OsStr::new("/elsewhere");
        let mut yes = Answer {
            tty: true,
            yes: true,
            asked: 0,
        };
        let named = resolve(
            "apply",
            Some(OsStr::new("x")),
            &repo,
            Some(env),
            false,
            &mut yes,
        );
        assert_eq!(named.unwrap().origin, Origin::Explicit);
        assert_eq!(yes.asked, 0);
        let here = resolve("apply", None, &repo, Some(env), false, &mut yes).unwrap();
        assert_eq!((here.origin, yes.asked), (Origin::CurrentDirectory, 1));
        let there = resolve("apply", None, &empty, Some(env), false, &mut yes).unwrap();
        assert_eq!(there.source, PathBuf::from("/elsewhere"));
        let mut no = Answer {
            tty: true,
            yes: false,
            asked: 0,
        };
        let declined = resolve("apply", None, &repo, Some(env), false, &mut no).unwrap_err();
        assert_eq!(declined.code, "E_DECLINED");
        let mut pipe = Answer {
            tty: false,
            yes: true,
            asked: 0,
        };
        assert_eq!(
            resolve("apply", None, &repo, None, false, &mut pipe)
                .unwrap_err()
                .code,
            "E_DECLINED"
        );
        assert_eq!(
            resolve("apply", None, &repo, None, true, &mut pipe)
                .unwrap()
                .origin,
            Origin::CurrentDirectory
        );
        assert_eq!(pipe.asked, 0);
        assert_eq!(
            resolve("apply", None, &empty, None, false, &mut pipe)
                .unwrap_err()
                .code,
            "E_NO_MANIFEST"
        );
        std::fs::write(repo.join("lodi.toml"), "").unwrap();
        assert!(
            !holds_repository(&repo),
            "a project is not a host repository"
        );
        let _ = std::fs::remove_dir_all(&repo);
        let _ = std::fs::remove_dir_all(&empty);
    }
}
