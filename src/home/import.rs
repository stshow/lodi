//! The starting `home.toml` of `lodi import`: the declarative shape, written from the programs
//! on the machine and from nothing in the home directory (`docs/design/HOME_PROGRAMS.md` §7,
//! LD-321, LD-341).
//!
//! No package manager, no network, no clock — and **no read of any file's content**.
//!
//! # What it emits
//!
//! - one **commented** `[programs.<name>]` stub per module of this build
//!   ([`crate::home::programs::REGISTRY`]) whose executable is on [`FIXED_PATH`] under the
//!   machine's root — the same test that feeds `[tools]` — holding the module's own
//!   [`Program::STUB`] and never a value from the machine (git's `user.email` line says so);
//! - the commented `[tools]` block, unchanged (LD-276);
//! - a commented `[home.file]` example, with `text` and with `source`;
//! - the **WHAT THIS FILE DOES NOT CARRY** note: nothing was copied or read, and which paths the
//!   modules write, or are shadowed by (§4.6), already exist — asked with `lstat` and nothing
//!   else. Those paths come from running each module over its own stub
//!   ([`crate::home::programs::stub_paths`]), so no file name is listed here and no shell rc
//!   file name enters `src/` (`tests/home_containment.rs`).
//!
//! # Determinism
//!
//! The emitted bytes are a pure function of the build, the programs found and which of a fixed
//! set of paths exist: a fixed template in registry order, and **no date, no `lodi` version, no
//! user name, no host name and no absolute path** (design call D10).
//!
//! [`Program::STUB`]: crate::home::programs::Program::STUB

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::home::fsops;
use crate::home::programs::{self, Module, RenderEnv};
use crate::roots::Roots;

/// The manifest's name in its directory.
pub const MANIFEST: &str = "home.toml";

/// What the emitted file says about the files a person will look for first and not find.
pub const SHELL_NOTE: &str = "\
# lodi never opens, reads or writes a shell configuration file — not a login profile, not an
# rc file, whatever shell it belongs to. [programs.bash] and [programs.zsh] render a file of
# lodi's own that you source with the one line `lodi switch --home` prints; [programs.fish]
# renders a drop-in fish loads by itself.
";

/// Where the starting `home.toml` looks for programs, whoever runs it.
pub const FIXED_PATH: [&str; 3] = ["/usr/local/bin", "/usr/bin", "/bin"];

/// The starting `home.toml` of a home whose config is `roots`' (#693): programs looked for on
/// [`FIXED_PATH`] under `machine`, the root of the machine the home is on (`--root`; LD-524).
pub fn template(roots: &Roots, machine: &Path) -> String {
    let dirs = FIXED_PATH.map(|dir| machine.join(dir.trim_start_matches('/')));
    let modules: Vec<&Module> = programs::registry()
        .iter()
        .filter(|module| on_path(&dirs, module.executable))
        .collect();
    emit(&modules, &tools_on_path(&dirs), &existing(roots))
}

/// One path a module writes or is shadowed by that is already there, as the note shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Existing {
    /// Relative to the home directory.
    path: String,
    module: &'static str,
}

/// Every path a shipped module renders into or is shadowed by that is below the home directory
/// and holds something — asked of `lstat` alone, so nothing is followed and nothing is read.
/// Paths in lodi's own configuration and data roots are left out: they are lodi's files, not
/// the application's.
fn existing(roots: &Roots) -> Vec<Existing> {
    let env = RenderEnv::of(roots);
    let mut out = Vec::new();
    for module in programs::registry() {
        for path in programs::stub_paths(module, &env) {
            if path.starts_with(roots.data()) || path.starts_with(roots.config()) {
                continue;
            }
            let Ok(rel) = path.strip_prefix(roots.home()) else {
                continue;
            };
            if fsops::lexists(&path) {
                out.push(Existing {
                    path: rel.display().to_string(),
                    module: module.name,
                });
            }
        }
    }
    out
}

fn on_path(dirs: &[PathBuf], name: &str) -> bool {
    dirs.iter().any(|dir| is_executable(&dir.join(name)))
}

/// The catalogue's names that also name an executable in `dirs` right now, sorted.
///
/// It is a guess, which is exactly why the block it feeds is emitted commented out (design call
/// D8): a person who has `jq` on their `PATH` may want lodi to own it, and may not.
fn tools_on_path(dirs: &[PathBuf]) -> Vec<String> {
    let mut names: Vec<String> = crate::catalogue::builtin_tool_names()
        .into_iter()
        .filter(|name| on_path(dirs, name))
        .collect();
    names.sort();
    names.dedup();
    names
}

/// Whether a path is a regular file somebody may execute — the question `PATH` lookup asks, so
/// a symbolic link into `/usr/bin` counts, as it does when a shell runs the name.
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .map(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

// ------------------------------------------------------------------------- the emitter ---

/// The whole emitted manifest. A pure function of its arguments: the same build, `PATH` and set
/// of existing paths give the same bytes, whatever the hour and whoever runs it.
fn emit(modules: &[&Module], tools: &[String], existing: &[Existing]) -> String {
    let mut out = String::new();
    out.push_str(
        "# Generated by `lodi import --home`. It copied nothing and read no file in your home\n\
         # directory: every table below is commented out and holds no value from this machine,\n\
         # so no setting, secret or address of yours is in it. It names no user, no machine and\n\
         # no path outside the home directory, and it carries no date and no lodi version.\n\
         #\n\
         # Uncomment what you want, fill in your own values, then preview the switch:\n\
         #\n\
         #     lodi switch --home --dry-run     then     lodi switch --home\n\n",
    );
    out.push_str("[home]\nversion = \"1\"\n\n");
    programs_block(&mut out, modules);
    tools_block(&mut out, tools);
    home_file_block(&mut out);
    does_not_carry(&mut out, existing);
    out
}

fn programs_block(out: &mut String, modules: &[&Module]) {
    let shipped: Vec<&str> = programs::registry().iter().map(|m| m.name).collect();
    if modules.is_empty() {
        let _ = writeln!(
            out,
            "# [programs] — lodi renders a program's own configuration file from a table of its\n\
             # keys. None of the programs it has a module for ({}) is on your PATH\n\
             # right now, so there is no stub here; declaring one installs nothing.\n",
            shipped.join(", ")
        );
        return;
    }
    out.push_str(
        "# [programs] — lodi renders a program's own configuration file from a table of its keys.\n\
         # Each stub below is a program lodi has a module for whose executable is on your PATH\n\
         # right now; the values are examples, not yours. Declaring a table installs nothing,\n\
         # and `enable`, `backup`, `on_remove` and `extra` work in every one.\n\n",
    );
    for module in modules {
        let _ = writeln!(out, "# [programs.{}]", module.name);
        out.push_str(module.stub);
        out.push('\n');
    }
}

/// The one line of the `[tools]` block that says where a recipe of your own goes (LD-409). The
/// import writes none: it captures no recipe.
const RECIPES_LINE: &str = concat!(
    "# A tool lodi has no recipe for takes one of your own: ",
    "recipes/NAME.toml at your config's root.\n"
);

fn tools_block(out: &mut String, tools: &[String]) {
    if tools.is_empty() {
        out.push_str(
            "# [tools] — lodi can put pinned upstream tools on your PATH. No name in lodi's\n\
             # catalogue matches an executable on your PATH right now, so there is nothing to\n\
             # suggest here; `lodi search` lists what the catalogue has.\n",
        );
        out.push_str(RECIPES_LINE);
        out.push('\n');
        return;
    }
    out.push_str(
        "# [tools] — lodi can put pinned upstream tools on your PATH. Each name below is in\n\
         # lodi's catalogue and an executable of the same name is on your PATH right now, so\n\
         # you may already want lodi to own it. This block is commented out on purpose: a\n\
         # switch would take over those names, and \"latest\" for a tool you did not ask lodi to\n\
         # manage is not a choice this command makes for you. Uncomment what you want.\n",
    );
    out.push_str(RECIPES_LINE);
    out.push_str("# [tools]\n");
    for name in tools {
        let _ = writeln!(out, "# {name} = \"latest\"");
    }
    out.push('\n');
}

fn home_file_block(out: &mut String) {
    out.push_str(
        "# [home.file] — for a file no module covers, declare its bytes yourself: inline with\n\
         # `text`, or from a file beside this manifest with `source`. lodi writes the file and\n\
         # owns it from then on; a different file already at the path is backed up once first.\n\
         # [home.file.\".editorconfig\"]\n\
         # text = \"root = true\\n\"\n\
         # [home.file.\".inputrc\"]\n\
         # source = \"dotfiles/inputrc\"\n\n",
    );
}

fn does_not_carry(out: &mut String, existing: &[Existing]) {
    out.push_str(
        "# WHAT THIS FILE DOES NOT CARRY\n\
         #\n\
         # Your settings. This import copied nothing and read no file's content, so nothing of\n\
         # yours is in this file. It only asked whether a file is there, never what it holds, at\n\
         # each path lodi's program modules write or that a program reads before them:\n\
         #\n",
    );
    if existing.is_empty() {
        out.push_str("#   none of those paths holds a file in this home directory.\n");
    }
    for entry in existing {
        let _ = writeln!(
            out,
            "#   {} exists; translate what you need into [programs.{}]",
            entry.path, entry.module
        );
    }
    out.push_str("#\n");
    out.push_str(SHELL_NOTE);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `[tools]` block says once, in a comment, where a recipe of your own goes, whether it
    /// suggests tools or not, and the import captures no recipe (LD-409).
    #[test]
    fn the_tools_block_names_where_recipes_go_once() {
        for tools in [Vec::new(), vec!["jq".to_string()]] {
            let text = emit(&[], &tools, &[]);
            let lines: Vec<&str> = text.lines().filter(|l| l.contains("recipes/")).collect();
            assert_eq!(lines, [RECIPES_LINE.trim_end()], "{text}");
            assert!(lines[0].starts_with("# ") && lines[0].len() <= 100);
            assert!(!text.lines().any(|l| l.starts_with("[tools")), "{text}");
        }
    }

    /// Each shipped stub, with its `# ` removed, is a table its module accepts, and every line of
    /// it is a comment in the emitted file.
    #[test]
    fn every_shipped_stub_is_commented_and_a_table_its_module_accepts() {
        let env = RenderEnv::placeholder();
        for module in programs::registry() {
            assert!(!module.stub.is_empty(), "{} ships no stub", module.name);
            assert!(module.stub.ends_with('\n'), "{}", module.name);
            for line in module.stub.lines() {
                assert!(line.starts_with("# "), "{}: {line:?}", module.name);
            }
            assert!(
                programs::stub_table(module).is_some(),
                "{}'s stub is not TOML",
                module.name
            );
            assert!(
                !programs::stub_paths(module, &env).is_empty(),
                "{}'s stub is not a table its module renders",
                module.name
            );
        }
    }

    #[test]
    fn the_emitted_file_is_a_pure_function_of_its_arguments() {
        let modules: Vec<&Module> = programs::registry().iter().collect();
        let existing = [Existing {
            path: ".gitconfig".into(),
            module: "git",
        }];
        let a = emit(&modules, &["jq".to_string()], &existing);
        assert_eq!(a, emit(&modules, &["jq".to_string()], &existing));
        assert!(a.contains("#   .gitconfig exists; translate what you need into [programs.git]\n"));
        assert!(a.contains("# user.email = \"\"  # your address; lodi never imports it\n"));
        // With every comment line removed, what is left is the one table the loader needs.
        let live: Vec<&str> = a
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(live, ["[home]", "version = \"1\""]);
    }
}
