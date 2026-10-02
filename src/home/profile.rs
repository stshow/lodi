//! The stable PATH surface of the home tool set (M-0.5 T-5, design call D11, `LD-107`).
//!
//! One Lodi-owned bin farm is rebuilt from the realized lock in lexicographic tool-name order;
//! the first tool to provide an executable wins. One generated POSIX script removes that one
//! directory from `PATH`, prepends it once, and exports the tool environment. Lodi prints the
//! line that sources the script and never opens a shell configuration file.
//!
//! When `[programs.fish]` is declared, a fish twin of the script, `profile.fish`, sets the same
//! bin directory and environment in fish syntax, and the module's `conf.d/lodi.fish` sources it
//! (M-Home, `docs/design/HOME_PROGRAMS.md` §5). The bash and zsh modules print their own line
//! after this one ([`hook_lines`]).

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;
use crate::home::fsops::{self, RelPath};
use crate::home::tools::Resolution;
use crate::roots::Roots;

pub const PROFILE_DIR: &str = "home-scope/profile";
pub const PROFILE_BIN: &str = "home-scope/profile/bin";
pub const PROFILE_SH: &str = "home-scope/profile.sh";
/// The fish twin of [`PROFILE_SH`], written only while `[programs.fish]` is declared.
pub const PROFILE_FISH: &str = "home-scope/profile.fish";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Built {
    pub notes: Vec<String>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Notice {
    pub lines: Vec<String>,
    pub warnings: Vec<String>,
}

fn rel(text: &str) -> RelPath {
    RelPath::new(text).expect("the generated profile stays inside the data root")
}

fn entry_rel(name: &str) -> Result<RelPath, Diagnostic> {
    RelPath::new(&format!("{PROFILE_BIN}/{name}"))
}

fn ensure_dir(roots: &Roots, path: &str) -> Result<(), Diagnostic> {
    let data = roots.data_root();
    if fsops::is_dir(&data, &rel(path)) {
        return Ok(());
    }
    // The scope directory above it is created owner-only (LD-361); the profile's own directories
    // keep their explicit 0755.
    fsops::mkdir_private(&data, &rel(path))?;
    fsops::set_mode(&data, &rel(path), 0o755)
}

/// Empty the bin farm. Links are routine generated content; a non-link regular file is removed
/// too, with the note design call D11 requires. Directories and special files are refused.
fn empty_bin(roots: &Roots) -> Result<Vec<String>, Diagnostic> {
    let path = roots.data().join(PROFILE_BIN);
    let Some(entries) = fsops::entries(&roots.data_root(), &rel(PROFILE_BIN))? else {
        return Ok(Vec::new());
    };
    let mut notes = Vec::new();
    for (name, kind) in entries {
        let Some(name) = name.to_str() else {
            return Err(Diagnostic::new(
                "E_STORE_IO",
                "the generated profile bin directory contains a non-UTF-8 file name",
            )
            .hint(
                "remove that entry from Lodi's profile bin directory and run lodi switch again",
            ));
        };
        if kind.is_dir() {
            return Err(Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "cannot rebuild the generated profile: {} is a directory",
                    path.join(name).display()
                ),
            )
            .hint("remove that directory yourself; lodi never removes a directory"));
        }
        if kind.is_file() {
            notes.push(format!(
                "removed non-link `{name}` from Lodi's generated profile bin directory"
            ));
        }
        fsops::remove_owned(&roots.data_root(), &entry_rel(name)?)?;
    }
    Ok(notes)
}

fn executable(path: &Path) -> bool {
    fs::metadata(path).is_ok_and(|meta| !meta.is_dir() && meta.permissions().mode() & 0o111 != 0)
}

fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\"'\"'"))
}

fn fish_quote(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

fn resolved_env(
    roots: &Roots,
    resolution: &Resolution,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    let mut env = BTreeMap::new();
    let arts: BTreeMap<&str, &str> = resolution
        .plan
        .arts
        .iter()
        .map(|(label, art)| (label.as_str(), art.as_str()))
        .collect();
    for (label, tool) in &resolution.lock.packages {
        let art = arts.get(label.as_str()).ok_or_else(|| {
            Diagnostic::new(
                "E_RECIPE_CTX",
                format!("the home plan has no store entry for tool `{label}`"),
            )
        })?;
        let path = roots
            .data()
            .join("store")
            .join(art)
            .to_string_lossy()
            .into_owned();
        for (name, value) in &tool.spec.env {
            let value = value
                .replace("@{self.path}", &path)
                .replace("${self.path}", &path)
                .replace("${self.version}", &tool.version);
            if value.contains("@{") || value.contains("${self.") {
                return Err(Diagnostic::new(
                    "E_RECIPE_CTX",
                    format!("tool `{label}` sets {name} with an unknown placeholder"),
                ));
            }
            env.insert(name.clone(), value);
        }
    }
    Ok(env)
}

fn script(roots: &Roots, resolution: &Resolution) -> Result<Vec<u8>, Diagnostic> {
    let bin = roots
        .data()
        .join(PROFILE_BIN)
        .to_string_lossy()
        .into_owned();
    let mut text = format!(
        "# Generated by lodi; source this file.\n\
         _lodi_bin={}\n\
         _lodi_rest=${{PATH-}}\n\
         _lodi_path=\n\
         _lodi_sep=\n\
         while :; do\n\
           case $_lodi_rest in\n\
             *:*) _lodi_part=${{_lodi_rest%%:*}}; _lodi_rest=${{_lodi_rest#*:}}; _lodi_more=1 ;;\n\
             *) _lodi_part=$_lodi_rest; _lodi_rest=; _lodi_more= ;;\n\
           esac\n\
           if [ \"$_lodi_part\" != \"$_lodi_bin\" ]; then\n\
             _lodi_path=${{_lodi_path}}${{_lodi_sep}}${{_lodi_part}}\n\
             _lodi_sep=:\n\
           fi\n\
           [ -n \"$_lodi_more\" ] || break\n\
         done\n\
         PATH=$_lodi_bin${{_lodi_path:+:$_lodi_path}}\n\
         export PATH\n",
        shell_quote(&bin)
    );
    for (name, value) in resolved_env(roots, resolution)? {
        text.push_str(&format!("{name}={}\nexport {name}\n", shell_quote(&value)));
    }
    text.push_str("unset _lodi_bin _lodi_rest _lodi_path _lodi_sep _lodi_part _lodi_more\n");
    Ok(text.into_bytes())
}

/// [`script`] in fish syntax: the same bin directory, taken out of `PATH` and prepended once, and
/// the same environment, every variable global and none universal. Its variables of its own are
/// local to the file.
fn fish_script(roots: &Roots, resolution: &Resolution) -> Result<Vec<u8>, Diagnostic> {
    let bin = roots
        .data()
        .join(PROFILE_BIN)
        .to_string_lossy()
        .into_owned();
    let mut text = format!(
        "# Generated by lodi; lodi.fish sources this file.\n\
         set -l _lodi_bin {}\n\
         set -l _lodi_path\n\
         set -l _lodi_part\n\
         for _lodi_part in $PATH\n\
         \x20   if test \"$_lodi_part\" != \"$_lodi_bin\"\n\
         \x20       set -a _lodi_path $_lodi_part\n\
         \x20   end\n\
         end\n\
         set -gx PATH $_lodi_bin $_lodi_path\n",
        fish_quote(&bin)
    );
    for (name, value) in resolved_env(roots, resolution)? {
        text.push_str(&format!("set -gx {name} {}\n", fish_quote(&value)));
    }
    Ok(text.into_bytes())
}

/// Rebuild the one generated bin directory and POSIX activation script from the realized lock,
/// and its fish twin when `fish` (`[programs.fish]` is declared) — or remove the twin when not.
pub fn rebuild(roots: &Roots, resolution: &Resolution, fish: bool) -> Result<Built, Diagnostic> {
    ensure_dir(roots, PROFILE_DIR)?;
    ensure_dir(roots, PROFILE_BIN)?;
    let notes = empty_bin(roots)?;
    let mut warnings = Vec::new();
    let mut owner: BTreeMap<OsString, String> = BTreeMap::new();
    let arts: BTreeMap<&str, &str> = resolution
        .plan
        .arts
        .iter()
        .map(|(label, art)| (label.as_str(), art.as_str()))
        .collect();

    for (label, tool) in &resolution.lock.packages {
        let art = arts.get(label.as_str()).ok_or_else(|| {
            Diagnostic::new(
                "E_RECIPE_CTX",
                format!("the home plan has no store entry for tool `{label}`"),
            )
        })?;
        for dir in &tool.spec.path {
            let source = roots.data().join("store").join(art).join(dir);
            let Ok(read) = fs::read_dir(&source) else {
                continue;
            };
            let mut names: Vec<OsString> = read
                .flatten()
                .filter(|entry| executable(&entry.path()))
                .map(|entry| entry.file_name())
                .collect();
            names.sort();
            for name in names {
                let Some(text) = name.to_str() else {
                    return Err(Diagnostic::new(
                        "E_STORE_IO",
                        format!("tool `{label}` provides an executable with a non-UTF-8 name"),
                    ));
                };
                if let Some(first) = owner.get(&name) {
                    if first != label {
                        warnings.push(format!(
                            "lodi: warning W_BIN_CONFLICT: `{text}` is provided by {first} and \
                             {label}; {first} wins"
                        ));
                    }
                    continue;
                }
                owner.insert(name.clone(), label.clone());
                let target = PathBuf::from("../../../store")
                    .join(art)
                    .join(dir)
                    .join(&name);
                fsops::link(&roots.data_root(), &entry_rel(text)?, &target)?;
            }
        }
    }

    fsops::write(
        &roots.data_root(),
        &rel(PROFILE_SH),
        &script(roots, resolution)?,
        0o644,
    )?;
    if fish {
        fsops::write(
            &roots.data_root(),
            &rel(PROFILE_FISH),
            &fish_script(roots, resolution)?,
            0o644,
        )?;
    } else {
        fsops::remove_owned(&roots.data_root(), &rel(PROFILE_FISH))?;
    }
    Ok(Built { notes, warnings })
}

/// Remove generated files when the manifest no longer declares tools. Directories remain: the
/// home scope never removes one.
pub fn clear(roots: &Roots) -> Result<Vec<String>, Diagnostic> {
    let notes = empty_bin(roots)?;
    fsops::remove_owned(&roots.data_root(), &rel(PROFILE_SH))?;
    fsops::remove_owned(&roots.data_root(), &rel(PROFILE_FISH))?;
    Ok(notes)
}

fn display_under_home(roots: &Roots, path: &Path) -> String {
    match path.strip_prefix(roots.home()) {
        Ok(rest) => format!("$HOME/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

fn source_line(roots: &Roots) -> String {
    source_line_of(roots, &roots.data().join(PROFILE_SH))
}

/// The line that sources `path`: `$HOME`-relative below the home directory, quoted otherwise.
fn source_line_of(roots: &Roots, path: &Path) -> String {
    match path.strip_prefix(roots.home()) {
        Ok(rest) => format!("  . \"$HOME/{}\"", rest.display()),
        Err(_) => format!("  . {}", shell_quote(&path.to_string_lossy())),
    }
}

/// The stable instruction printed by apply and status, or the fish warning in its place. With
/// `fish_module` (`[programs.fish]` is declared) a fish user needs no line and gets no warning:
/// `conf.d/lodi.fish` includes the tools (§5).
pub fn notice(roots: &Roots, fish_module: bool) -> Notice {
    let bin = roots.data().join(PROFILE_BIN);
    let shell_value = std::env::var_os("SHELL");
    let shell = shell_value
        .as_deref()
        .map(Path::new)
        .and_then(|path| path.file_name())
        .and_then(OsStr::to_str);
    if shell == Some("fish") && fish_module {
        return Notice::default();
    }
    if shell == Some("fish") {
        return Notice {
            warnings: vec![format!(
                "lodi: warning W_FISH_HOOKS: fish does not read the POSIX shell profile; declare \
                 [programs.fish] to have lodi.fish include the tools, or add {} to fish's PATH \
                 yourself",
                display_under_home(roots, &bin)
            )],
            lines: Vec::new(),
        };
    }
    Notice {
        lines: vec![
            "add this line to your shell rc file (lodi never edits it):".into(),
            source_line(roots),
        ],
        warnings: Vec::new(),
    }
}

/// The lines apply and status print last for the rendered shell files the user sources from a
/// shell's own start-up file (`[programs.bash]`, `[programs.zsh]`, §5): per file, the same form
/// as the `profile.sh` line. Lodi never adds them anywhere.
pub fn hook_lines(roots: &Roots, hooks: &[(String, PathBuf)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (shell, path) in hooks {
        lines.push(format!(
            "add this line to your {shell} rc file (lodi never edits it):"
        ));
        lines.push(source_line_of(roots, path));
    }
    lines
}

/// Whether the current process already has the one generated bin directory on `PATH`.
pub fn path_status(roots: &Roots) -> String {
    let bin = roots.data().join(PROFILE_BIN);
    let present = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == bin));
    format!(
        "home tools: {} is {}on PATH",
        display_under_home(roots, &bin),
        if present { "" } else { "not " }
    )
}
