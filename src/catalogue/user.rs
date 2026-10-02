//! Recipes of your own (LD-409): `recipes/NAME.toml` at the root of the config, with exactly
//! the standing of a built-in recipe.
//!
//! - **Where.** The root of the config a switch, update, pin or unpin acts on (#692, #710).
//!   `shell`, `develop`, `run` and `search` never look for a config (#698 story 39), so only the
//!   built-in recipes resolve there, and no other folder is read: not the current directory, not
//!   a `recipes/` beside a project manifest.
//! - **What.** Flat regular `NAME.toml` files directly in `recipes/`, each read through the host
//!   scope's walk and caps. A link, a directory or a file that is not `NAME.toml` with a valid
//!   package name is skipped with `W_RECIPE_SKIPPED`. A file that does not read or parse refuses
//!   only its own tool, naming `recipes/NAME.toml`.
//! - **Precedence.** A recipe of your own with a built-in's name wins, and `W_RECIPE_SHADOWED`
//!   names the built-in it hides, once per invocation.
//! - **Once.** An invocation reads one repository's folder: the entry point that knows the
//!   repository calls [`install`] before anything resolves, and every reader asks [`active`].
//!   Nothing installed is no repository.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use crate::catalogue::{Recipe, builtin_recipe, builtin_tool_names, parse_user_recipe};
use crate::diag::Diagnostic;
use crate::hostscope::source::{self, Host};
use crate::util::sha256_tagged;

/// The folder at the repository's root.
pub const DIR: &str = "recipes";

/// The most bytes one recipe file may have.
pub const MAX_BYTES: usize = 256 * 1024;

/// One `recipes/NAME.toml`: the digest of its bytes, and the recipe or why it is refused.
#[derive(Debug, Clone)]
struct File {
    sha256: Option<String>,
    recipe: Result<Recipe, Diagnostic>,
}

/// The recipes of one repository's `recipes/` folder, or none.
#[derive(Debug, Clone, Default)]
pub struct UserRecipes {
    /// The folder as read, `<repository>/recipes`; `None` when there is no repository.
    pub dir: Option<PathBuf>,
    files: BTreeMap<String, File>,
    skipped: Vec<String>,
}

fn skipped(what: &str, why: &str) -> String {
    format!("lodi: warning W_RECIPE_SKIPPED: {what} {why}; it is not read as a recipe")
}

impl UserRecipes {
    /// No repository: the built-in recipes alone.
    pub fn none() -> UserRecipes {
        UserRecipes::default()
    }

    /// The recipe of your own named `name`, if the folder has a file for it.
    pub fn recipe(&self, name: &str) -> Option<Result<Recipe, Diagnostic>> {
        self.files.get(name).map(|file| file.recipe.clone())
    }

    /// The digest of `recipes/<name>.toml` as read, if there is one: what a user tool's lock
    /// entry is fresh against.
    pub fn sha256(&self, name: &str) -> Option<&str> {
        self.files.get(name).and_then(|file| file.sha256.as_deref())
    }

    /// Whether the folder has a file for `name`, readable or not.
    pub fn has(&self, name: &str) -> bool {
        self.files.contains_key(name)
    }

    /// Where a recipe of your own named `name` goes: this repository's folder, or where one
    /// would be.
    pub fn place(&self, name: &str) -> String {
        match &self.dir {
            Some(dir) => format!("{}", dir.join(format!("{name}.toml")).display()),
            None => format!("{DIR}/{name}.toml at your config's root"),
        }
    }

    /// The lines every reader prints once: each entry skipped, and each built-in hidden.
    pub fn warnings(&self) -> Vec<String> {
        let mut lines = self.skipped.clone();
        for name in self.files.keys() {
            if builtin_recipe(name).is_some() {
                lines.push(format!(
                    "lodi: warning W_RECIPE_SHADOWED: {DIR}/{name}.toml is used in place of the \
                     built-in recipe {name}"
                ));
            }
        }
        lines
    }
}

/// Resolution through the recipes of your own first, then the built-in ones.
impl crate::tools::Catalogue for UserRecipes {
    fn recipe(&self, name: &str) -> Option<Result<Recipe, Diagnostic>> {
        UserRecipes::recipe(self, name).or_else(|| builtin_recipe(name))
    }

    fn names(&self) -> Vec<String> {
        let mut names = builtin_tool_names();
        names.extend(self.files.keys().cloned());
        names.sort();
        names.dedup();
        names
    }

    fn place(&self, name: &str) -> String {
        UserRecipes::place(self, name)
    }
}

/// Read `recipes/` below `reader.dir` through the host scope's walk: every directory on the way
/// is judged, and each file is opened without following a link, as the manifest beside it is.
pub fn read(reader: &Host) -> Result<UserRecipes, Diagnostic> {
    let mut out = UserRecipes {
        dir: Some(reader.dir.join(DIR)),
        ..UserRecipes::default()
    };
    let Some(entries) = source::read_dir(reader, DIR)? else {
        return Ok(out);
    };
    for (entry, kind) in entries {
        let entry = entry.to_string_lossy().into_owned();
        let shown = format!("{DIR}/{entry}");
        let stem = entry
            .strip_suffix(".toml")
            .filter(|stem| crate::tools::is_package_name(stem));
        let why = if kind.is_symlink() {
            "is a symbolic link"
        } else if kind.is_dir() {
            "is a directory; recipes are the files directly in recipes/"
        } else if !kind.is_file() {
            "is not a regular file"
        } else if stem.is_none() {
            "is not NAME.toml with NAME a package name"
        } else {
            ""
        };
        let Some(stem) = stem.filter(|_| why.is_empty()) else {
            out.skipped.push(skipped(&shown, why));
            continue;
        };
        let file = match source::read_file(reader, &shown, MAX_BYTES) {
            Ok(None) => continue,
            Ok(Some(bytes)) => File {
                sha256: Some(sha256_tagged(&bytes)),
                recipe: parse_bytes(&bytes, &entry, &shown),
            },
            Err(refused) => File {
                sha256: None,
                recipe: Err(refused),
            },
        };
        out.files.insert(stem.to_string(), file);
    }
    Ok(out)
}

fn parse_bytes(bytes: &[u8], file: &str, shown: &str) -> Result<Recipe, Diagnostic> {
    let invalid =
        |why: String| Diagnostic::new("E_RECIPE_INVALID", format!("recipe {shown}: {why}"));
    if bytes.len() > MAX_BYTES {
        return Err(invalid(format!("it exceeds {MAX_BYTES} bytes")));
    }
    let text = std::str::from_utf8(bytes).map_err(|_| invalid("it is not UTF-8".into()))?;
    parse_user_recipe(text, file)
}

type Loaded = Result<Arc<UserRecipes>, Diagnostic>;

static ACTIVE: RwLock<Option<Loaded>> = RwLock::new(None);

/// Make `recipes` the set this invocation resolves through, and print its warnings once.
pub fn install(recipes: Result<UserRecipes, Diagnostic>) {
    let loaded = recipes.map(Arc::new);
    if let Ok(recipes) = &loaded {
        for line in recipes.warnings() {
            eprintln!("{line}");
        }
    }
    *ACTIVE.write().unwrap_or_else(|e| e.into_inner()) = Some(loaded);
}

/// The set this invocation resolves through: what was installed, else no repository.
pub fn active() -> Result<Arc<UserRecipes>, Diagnostic> {
    match &*ACTIVE.read().unwrap_or_else(|e| e.into_inner()) {
        Some(loaded) => loaded.clone(),
        None => Ok(Arc::new(UserRecipes::none())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const RECIPE: &str = "[recipe]\nname = \"widget\"\ndescription = \"a tool\"\n\
        homepage = \"https://widget.test/\"\n\n[versions]\nstrategy = \"text_index\"\n\
        url = \"https://widget.test/latest.txt\"\nline = \"v${version}\"\n\n[asset]\n\
        url = \"https://widget.test/v${version}/widget\"\nformat = \"binary\"\n\
        checksum_sidecar = \".sha256\"\n\n[spec]\nbin = [\"bin/widget\"]\npath = []\n";

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("lodi-user-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(DIR)).unwrap();
        dir
    }

    fn host(dir: &Path) -> Host {
        Host {
            dir: dir.to_path_buf(),
            anchor: dir.to_path_buf(),
            source: String::new(),
            owners: source::owners_for(unsafe { libc::geteuid() }, None),
            private: Vec::new(),
            named: None,
        }
    }

    /// U1: flat regular `NAME.toml` files only; everything else is skipped by name, and a broken
    /// file stands only for itself.
    #[test]
    fn the_folder_holds_flat_recipe_files_and_skips_the_rest() {
        let dir = scratch("folder");
        let folder = dir.join(DIR);
        std::fs::write(folder.join("widget.toml"), RECIPE).unwrap();
        std::fs::write(folder.join("jq.toml"), RECIPE.replace("widget", "jq")).unwrap();
        std::fs::write(folder.join("gadget.toml"), RECIPE).unwrap();
        std::fs::write(folder.join("Bad.toml"), RECIPE).unwrap();
        std::fs::write(folder.join("notes.txt"), "").unwrap();
        std::fs::create_dir(folder.join("nested")).unwrap();
        std::os::unix::fs::symlink("widget.toml", folder.join("linked.toml")).unwrap();
        let read = read(&host(&dir)).unwrap();
        assert!(read.recipe("widget").unwrap().is_ok());
        let widget = read.recipe("widget").unwrap().unwrap();
        assert_eq!((widget.input, widget.lock_path()), ("user", "widget.toml"));
        assert_eq!(widget.file, "recipes/widget.toml");
        let gadget = read.recipe("gadget").unwrap().unwrap_err();
        assert_eq!(gadget.code, "E_RECIPE_INVALID");
        assert!(
            gadget.message.contains("recipes/gadget.toml"),
            "{}",
            gadget.message
        );
        let warnings = read.warnings();
        for name in ["Bad.toml", "notes.txt", "nested", "linked.toml"] {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.contains("W_RECIPE_SKIPPED") && w.contains(name)),
                "{name}: {warnings:?}"
            );
        }
        let shadowed: Vec<&String> = warnings
            .iter()
            .filter(|w| w.contains("W_RECIPE_SHADOWED"))
            .collect();
        assert_eq!(shadowed.len(), 1, "{warnings:?}");
        assert!(shadowed[0].contains("built-in recipe jq"));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A writable folder is the walk's refusal, never a recipe read from it.
    #[test]
    fn a_writable_folder_is_a_path_escape() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch("writable");
        std::fs::set_permissions(dir.join(DIR), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(read(&host(&dir)).unwrap_err().code, "E_PATH_ESCAPE");
        let _ = std::fs::remove_dir_all(dir);
    }

    /// U2: the catalogue lint's rules hold for a recipe of your own, naming its path.
    #[test]
    fn a_recipe_of_your_own_is_held_to_the_catalogue_lint() {
        for broken in [
            RECIPE.replace("https://widget.test/v", "http://widget.test/v"),
            RECIPE.replace("${version}/widget", "1.2/widget"),
            RECIPE.replace("description = \"a tool\"", "description = \"\""),
        ] {
            let e = parse_user_recipe(&broken, "widget.toml").unwrap_err();
            assert_eq!(e.code, "E_RECIPE_INVALID");
            assert!(
                e.message.starts_with("recipe recipes/widget.toml: "),
                "{}",
                e.message
            );
        }
        assert!(parse_user_recipe(&format!("# 1.2 in a comment\n{RECIPE}"), "widget.toml").is_ok());
    }
}
