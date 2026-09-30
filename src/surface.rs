//! The deprecation table of the published CLI surface, and the resolver that reads it.
//!
//! `docs/CLI.md` writes the 1.x promise down: a command or a flag published at 1.0 is stable for
//! 1.x, new ones may be added, and an existing one is **removed only after at least one minor
//! version in which using it prints `W_DEPRECATED`** (design `spec/10-cli` §8). That sentence
//! needed a mechanism rather than good intentions, and this module is it.
//!
//! It has no command of its own and reads nothing:
//!
//! - [`DEPRECATIONS`] is the committed table. It was empty at 1.0 and 1.1 — an empty table was
//!   the whole point of the seam, not an oversight: a resolver with no entries is not an
//!   unreachable command, the same shape M-0.6 T-1 landed the package-manager `match` with no
//!   arms in (M-1.0 design call D13). From 1.2 it has one row, the home manifest's `[files]`
//!   table (decision D5, LD-324), and still no command or flag.
//! - A surface is either typed on the command line (`home status`, `init --force`) or a table of
//!   a manifest, written ``home.toml `[files]` `` ([`Deprecation::manifest`]).
//! - [`deprecation`] answers for **the table it is given** and a surface typed on the command
//!   line, so the test supplies its own table. It returns `None` for a surface with no entry, and
//!   a [`Warning`] for one with an entry. [`manifest_deprecation`] answers the same way for a
//!   table of a manifest a command has read.
//!
//! A warning is a line on standard error and nothing else: it never changes an exit status
//! (`docs/ERRORS.md`, "Warnings are not in this table and never change the exit status").
//! `src/main.rs` prints a command-line surface's line in exactly one place, before the dispatch
//! runs; the home scope prints a manifest table's line with the other warnings of the manifest it
//! loaded (`crate::home::plan::manifest_warnings`), once per run.

use std::ffi::OsString;
use std::fmt;

/// The code every line this module renders carries.
///
/// It is a warning, not an error: it is deliberately **not** a row of `docs/ERRORS.md`'s code
/// table and not an entry of [`crate::diag::CODES`], both of which are `E_` codes with an exit
/// status. It is documented in that file's warnings paragraph instead.
pub const W_DEPRECATED: &str = "W_DEPRECATED";

/// One row of the published deprecation table.
///
/// The same four columns `docs/CLI.md`'s deprecation table has, in the same order, so that the
/// committed table and the document say the same thing by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Deprecation {
    /// The surface as a user types it, without the program name: `home status`, `init --force`,
    /// `--no-nest`. `src/main.rs` derives the surfaces of one invocation the same way, and
    /// `docs/CLI.md` states that derivation. A table of a manifest is written as the manifest's
    /// file name, a space and the table in backquotes: ``home.toml `[files]` ``.
    pub surface: &'static str,
    /// The minor version whose release announced the deprecation, as `X.Y`.
    pub announced_in: &'static str,
    /// The earliest minor version in which the surface may be removed, as `X.Y`. It is never the
    /// version that announced it: the promise is at least one whole minor version of warning.
    pub removable_in: &'static str,
    /// What to use instead, as a user would type it. Empty when there is no replacement, which
    /// the rendered line says in so many words rather than leaving blank. For a manifest table it
    /// is printed as written, backquotes included.
    pub replacement: &'static str,
}

impl Deprecation {
    /// The manifest file and the table, when the surface is a table of a manifest
    /// (``home.toml `[files]` `` gives `("home.toml", "[files]")`), and `None` for a surface typed
    /// on the command line.
    pub fn manifest(&self) -> Option<(&'static str, &'static str)> {
        in_manifest(self.surface)
    }
}

/// [`Deprecation::manifest`] over a surface string.
fn in_manifest(surface: &str) -> Option<(&str, &str)> {
    let (file, rest) = surface.split_once(" `")?;
    let table = rest.strip_suffix('`')?;
    (file.ends_with(".toml") && !file.contains(' ') && !table.is_empty()).then_some((file, table))
}

/// The committed table. Nothing was deprecated at 1.0 or 1.1; from 1.2 the one row is the home
/// manifest's `[files]` table (decision D5, LD-324), an exact alias of `[home.file]` through 1.x.
/// No command and no flag is deprecated.
///
/// A row is added here and to `docs/CLI.md`'s deprecation table in the same change, and nowhere
/// else — those two places are the whole table (M-1.0 T-8).
pub const DEPRECATIONS: &[Deprecation] = &[Deprecation {
    surface: "home.toml `[files]`",
    announced_in: "1.2",
    removable_in: "2.0",
    replacement: "`[home.file]` (rename `content` to `text`)",
}];

/// A `W_DEPRECATED` line, ready to print on standard error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Warning {
    pub code: &'static str,
    pub surface: String,
    pub announced_in: String,
    pub removable_in: String,
    pub replacement: String,
}

impl fmt::Display for Warning {
    /// The documented line: the code, the surface, the version that announced it, the version
    /// that may remove it, and the replacement. A manifest table reads
    /// ``lodi: warning W_DEPRECATED: `[files]` in home.toml is deprecated since …``.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let manifest = in_manifest(&self.surface);
        match manifest {
            Some((file, table)) => write!(f, "lodi: warning {}: `{table}` in {file}", self.code)?,
            None => write!(f, "lodi: warning {}: `lodi {}`", self.code, self.surface)?,
        }
        write!(
            f,
            " is deprecated since {} and may be removed in {}; ",
            self.announced_in, self.removable_in
        )?;
        match (self.replacement.is_empty(), manifest) {
            (true, _) => write!(f, "there is no replacement"),
            (false, Some(_)) => write!(f, "use {}", self.replacement),
            (false, None) => write!(f, "use `lodi {}` instead", self.replacement),
        }
    }
}

/// The surfaces one invocation typed, in the spelling [`Deprecation::surface`] and
/// `docs/CLI.md`'s deprecation table use.
///
/// The rule, stated in `docs/CLI.md` so that a maintainer writing a table row knows exactly what
/// string to write:
///
/// - the first argument, alone (`init`, `--help`);
/// - the first two arguments joined by a space, when the second does not begin with `-`
///   (`home status`, `host plan`);
/// - every argument that begins with `-`, alone and prefixed by the first argument
///   (`--force`, `init --force`).
///
/// Arguments after a bare `--` are the user's own command and are never surfaces of `lodi`.
/// Nothing here parses: it reads the argv as typed, so a deprecated spelling is announced even
/// when the rest of the invocation turns out to be a usage error. The result is sorted and holds
/// each surface once, so one entry is announced once however the argument reached it.
pub fn typed(args: &[OsString]) -> Vec<String> {
    let typed: Vec<String> = args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .take_while(|a| a != "--")
        .collect();
    let Some(first) = typed.first() else {
        return Vec::new();
    };
    let mut out = vec![first.clone()];
    if let Some(second) = typed.get(1) {
        if !second.starts_with('-') {
            out.push(format!("{first} {second}"));
        }
    }
    for arg in typed.iter().filter(|a| a.starts_with('-')) {
        out.push(arg.clone());
        if arg != first {
            out.push(format!("{first} {arg}"));
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The `W_DEPRECATED` line for `surface`, typed on the command line, or `None` when `table`
/// has no entry for it.
///
/// The match is on the whole surface string and is exact: `home` does not answer for
/// `home status`, and `--force` does not answer for `init --force`. A caller that wants both
/// asks for both, which is what `src/main.rs` does. A manifest table's row never answers here,
/// whatever was typed: it is announced by the command that reads the manifest.
pub fn deprecation(table: &[Deprecation], surface: &str) -> Option<Warning> {
    let entry = table
        .iter()
        .find(|d| d.surface == surface && d.manifest().is_none())?;
    Some(warning(entry))
}

/// The `W_DEPRECATED` line for the table `name` (as written, `[files]`) of the manifest `file`
/// (`home.toml`), or `None` when `table` has no entry for it. The caller asks once per run, for a
/// manifest it has read and found the table in.
pub fn manifest_deprecation(table: &[Deprecation], file: &str, name: &str) -> Option<Warning> {
    let entry = table.iter().find(|d| d.manifest() == Some((file, name)))?;
    Some(warning(entry))
}

fn warning(entry: &Deprecation) -> Warning {
    Warning {
        code: W_DEPRECATED,
        surface: entry.surface.to_string(),
        announced_in: entry.announced_in.to_string(),
        removable_in: entry.removable_in.to_string(),
        replacement: entry.replacement.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// (d) of M-1.0 T-8: every row of the committed table is well formed — a surface, two
    /// different versions, and the announcement strictly before the removal, because the promise
    /// is one whole minor version of warning. From 1.2 its one row is a manifest table (D5), so
    /// no command and no flag is deprecated.
    #[test]
    fn the_committed_table_is_well_formed_and_deprecates_no_command() {
        let version = |v: &str| -> (u32, u32) {
            let (major, minor) = v.split_once('.').expect("X.Y");
            (major.parse().unwrap(), minor.parse().unwrap())
        };
        for entry in DEPRECATIONS {
            assert!(!entry.surface.is_empty());
            assert!(
                version(entry.announced_in) < version(entry.removable_in),
                "{}",
                entry.surface
            );
            assert!(
                entry.manifest().is_some(),
                "{} is a command-line surface; docs/CLI.md says none is deprecated",
                entry.surface
            );
            assert_eq!(
                DEPRECATIONS
                    .iter()
                    .filter(|d| d.surface == entry.surface)
                    .count(),
                1,
                "{} is listed twice",
                entry.surface
            );
        }
        // No command-line surface answers, the manifest row's own spelling included.
        assert_eq!(deprecation(DEPRECATIONS, "init"), None);
        assert_eq!(deprecation(DEPRECATIONS, "home.toml `[files]`"), None);
    }

    /// D5 (LD-324): the manifest row renders the manifest form of the line, and only for the
    /// file and table it names.
    #[test]
    fn the_files_table_of_home_toml_renders_the_manifest_form() {
        let line = manifest_deprecation(DEPRECATIONS, "home.toml", "[files]")
            .expect("the D5 row resolves")
            .to_string();
        assert_eq!(
            line,
            "lodi: warning W_DEPRECATED: `[files]` in home.toml is deprecated since 1.2 and may \
             be removed in 2.0; use `[home.file]` (rename `content` to `text`)"
        );
        for (file, name) in [
            ("home.toml", "[home.file]"),
            ("home.toml", "files"),
            ("host.toml", "[files]"),
            ("lodi.toml", "[files]"),
        ] {
            assert_eq!(
                manifest_deprecation(DEPRECATIONS, file, name),
                None,
                "{file} {name}"
            );
        }
        let no_replacement = Deprecation {
            surface: "lodi.toml `[old]`",
            announced_in: "1.3",
            removable_in: "2.0",
            replacement: "",
        };
        assert_eq!(no_replacement.manifest(), Some(("lodi.toml", "[old]")));
        assert_eq!(
            manifest_deprecation(&[no_replacement], "lodi.toml", "[old]")
                .unwrap()
                .to_string(),
            "lodi: warning W_DEPRECATED: `[old]` in lodi.toml is deprecated since 1.3 and may be \
             removed in 2.0; there is no replacement"
        );
        // A command-line surface is not a manifest table, however it is spelled.
        for typed in [
            "home status",
            "init --force",
            "home.toml",
            "home.toml [files]",
        ] {
            assert_eq!(in_manifest(typed), None, "{typed}");
        }
    }
}
