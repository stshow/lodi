//! Diagnostics in the design `spec/11` human format and the process exit statuses.
//!
//! ```text
//! lodi: error E_UNKNOWN_ATTR: unknown key `nmae` in [project]
//!   --> lodi.toml:2:1
//!    = hint: did you mean `name`?
//! ```

use std::fmt;

/// Exit status of a command that succeeded, and of a warning, which never changes it.
pub const EXIT_SUCCESS: u8 = 0;
/// Exit status for a usage error: an unknown argument or a missing command (`spec/10` §2).
pub const EXIT_USAGE: u8 = 2;
/// Exit status for a manifest error: parse, schema or validation (`spec/10` §2).
pub const EXIT_MANIFEST: u8 = 3;
/// Exit status for a resolution error: no recipe, no match, unreachable index (`spec/11` §2).
pub const EXIT_RESOLUTION: u8 = 4;
/// Exit status for a fetch or integrity error: hash mismatch, HTTP failure (`spec/11` §2).
pub const EXIT_FETCH: u8 = 5;
/// Exit status for a build or store error; `lodi lock` uses it when the lock cannot be written.
pub const EXIT_BUILD: u8 = 6;
/// Exit status for a runtime error: cannot enter, nesting refused, no runtime (`spec/11` §2).
pub const EXIT_RUNTIME: u8 = 7;
/// Exit status for an apply that stopped part way, or refused to start (`spec/11` §2 `E_APPLY`,
/// ADR-013).
///
/// One constant for one status, shared by the host scope and the home scope: whichever lane
/// lands first defines it and the other reuses it (LD-131, M-0.6 design call D19). There is no
/// rollback to offer at this status; the message names the journal and the next command (LD-119).
/// The home scope reuses it for `E_DRIFT`, which refuses to overwrite a file the user edited by
/// hand (M-0.5 design call D6, `LD-102`).
pub const EXIT_APPLY: u8 = 8;
/// Exit status for a permission or capability error: the store cannot be created.
pub const EXIT_PERMISSION: u8 = 9;
/// Exit status for a missing, stale or invalid lock where a current one is required.
pub const EXIT_LOCK: u8 = 10;
/// Exit status when trust is declined or required and absent (`spec/11` §2, ADR-014).
pub const EXIT_DECLINED: u8 = 11;

/// Every diagnostic code this build can emit, with its exit status (`spec/11` §2).
///
/// The closed registry of M-1.0 T-3 (design call D9). It is sorted and each code appears once;
/// `exit_status` reads it, `tests/diagnostics.rs` holds it to `docs/ERRORS.md` in both
/// directions, and `tests/errors_catalogue.rs` fails when a code appears as a literal in `src/`
/// and not here — which is what keeps a new code from reaching the fallback of `exit_status`
/// unnoticed and silently becoming exit 3.
///
/// Groups, in the order the exit statuses above define them:
///
/// - **3** the manifest, plus `E_EXISTS`: `lodi init` finds `./lodi.toml` before it writes
///   anything, the "planned before apply" case `spec/11` §2 maps to exit 3 (D1, LD-46; the
///   departure from its tabulated 8 is `docs/DISCREPANCIES.md` D-01);
/// - **4** resolution, **5** fetch and integrity, **6** build and store — `E_STORE_VERSION`, a
///   store whose layout version this build does not know, is a code `spec/11` §2 does not have
///   at a status it already defines (M-1.0 T-2, design call D8, `docs/DISCREPANCIES.md` D-07,
///   recorded the way D-03 and D-04 record theirs) — **7** runtime;
/// - **8** an apply that stopped part way or refused to start: the host scope's failed action
///   and unclassifiable restart (ADR-013, `docs/DISCREPANCIES.md` D-02 and D-03, LD-118,
///   LD-119) and the home scope's refused apply, which has not acted at all (M-0.5 T-3, D-05,
///   LD-102);
/// - **9** permission and capability — an unarmed root, a root-only action without the
///   privilege for it, and an apply another apply already holds (M-0.6 T-1, LD-113), and a
///   host verb without `--root` where `LODI_HOST_REQUIRE_ROOT=1` forbids one (LD-376);
/// - **10** the lock, **11** trust;
/// - **0** a warning with a catalogue row, after the errors: `W_PROGRAM_NOT_FOUND` (M-Home,
///   LD-337) and `W_SHADOWED` (M-Home, LD-349), which never change the exit status a command
///   would have had. Every other `W_` code is documented
///   in `docs/ERRORS.md`'s warnings paragraph and has no row here.
pub const CODES: &[(&str, u8)] = &[
    ("E_APPLY", EXIT_APPLY),
    ("E_ARCHIVE_EMPTY", EXIT_BUILD),
    ("E_ARCHIVE_FORMAT", EXIT_BUILD),
    ("E_ARCHIVE_UNSAFE", EXIT_BUILD),
    ("E_ATTR_CONFLICT", EXIT_MANIFEST),
    ("E_BIN_MISSING", EXIT_BUILD),
    ("E_BLOCK_NOT_ALLOWED", EXIT_MANIFEST),
    ("E_BOOT_LOADER", EXIT_RUNTIME),
    ("E_BOOT_NOT_UEFI", EXIT_RUNTIME),
    ("E_BOOT_TRIAL", EXIT_RUNTIME),
    ("E_BUILD_UNAVAILABLE", EXIT_FETCH),
    ("E_CLOSURE_DRIFT", EXIT_BUILD),
    ("E_CONFIG", EXIT_MANIFEST),
    ("E_DECLINED", EXIT_DECLINED),
    ("E_DRIFT", EXIT_APPLY),
    ("E_DUP_RESOURCE", EXIT_MANIFEST),
    ("E_ENTER", EXIT_RUNTIME),
    ("E_EXCLUDED_CONSTRUCT", EXIT_MANIFEST),
    ("E_EXISTS", EXIT_MANIFEST),
    ("E_FETCH", EXIT_FETCH),
    ("E_FIREWALL_CONFLICT", EXIT_RUNTIME),
    ("E_GPGCHECK_OFF", EXIT_MANIFEST),
    ("E_HASH_MISMATCH", EXIT_FETCH),
    ("E_HOST_MISMATCH", EXIT_MANIFEST),
    ("E_HOST_NOT_ARMED", EXIT_PERMISSION),
    ("E_HOST_OSTREE", EXIT_MANIFEST),
    ("E_HOST_ROOT_REQUIRED", EXIT_PERMISSION),
    ("E_IDENT", EXIT_MANIFEST),
    ("E_IDENTITY_CONFLICT", EXIT_MANIFEST),
    ("E_INLINE_NEEDS_EXACT", EXIT_MANIFEST),
    ("E_INSECURE_URL", EXIT_FETCH),
    ("E_JOURNAL_AMBIGUOUS", EXIT_APPLY),
    ("E_LAYER_BUILD", EXIT_BUILD),
    ("E_LOCK_STALE", EXIT_LOCK),
    ("E_LOCK_VERSION", EXIT_RESOLUTION),
    ("E_MODE", EXIT_MANIFEST),
    ("E_NEED_ROOT", EXIT_PERMISSION),
    ("E_NESTED", EXIT_RUNTIME),
    ("E_NETWORK_ROLLED_BACK", EXIT_APPLY),
    ("E_NETWORK_STACK", EXIT_RUNTIME),
    ("E_NO_CHECKSUM", EXIT_FETCH),
    ("E_NO_MANIFEST", EXIT_MANIFEST),
    ("E_NO_MATCH", EXIT_RESOLUTION),
    ("E_NO_RECIPE", EXIT_RESOLUTION),
    ("E_NO_RUNTIME", EXIT_RUNTIME),
    ("E_PATH_ESCAPE", EXIT_MANIFEST),
    ("E_PIN_UNAVAILABLE", EXIT_RESOLUTION),
    ("E_PIN_UNSATISFIABLE", EXIT_RESOLUTION),
    ("E_PIN_UNTRUSTED", EXIT_FETCH),
    ("E_PROTECTED_PATH", EXIT_MANIFEST),
    ("E_RECIPE_CTX", EXIT_RESOLUTION),
    ("E_RECIPE_INVALID", EXIT_RESOLUTION),
    ("E_REPO_UNREACHABLE", EXIT_RESOLUTION),
    ("E_SHELL_NOT_FOUND", EXIT_RUNTIME),
    ("E_SNAPSHOT_FUTURE", EXIT_RESOLUTION),
    ("E_SNAPSHOT_TOO_OLD", EXIT_RESOLUTION),
    ("E_SSH_KEY", EXIT_MANIFEST),
    ("E_STORE_IO", EXIT_BUILD),
    ("E_STORE_PERM", EXIT_PERMISSION),
    ("E_STORE_VERSION", EXIT_BUILD),
    ("E_SYNTAX", EXIT_MANIFEST),
    ("E_SYSTEM_BUSY", EXIT_PERMISSION),
    ("E_TREE_UNSUPPORTED", EXIT_BUILD),
    ("E_TRUST_REQUIRED", EXIT_DECLINED),
    ("E_TYPE", EXIT_MANIFEST),
    ("E_UNKNOWN_ATTR", EXIT_MANIFEST),
    ("E_UNKNOWN_BLOCK", EXIT_MANIFEST),
    ("E_UNKNOWN_PACKAGE", EXIT_MANIFEST),
    ("E_UNKNOWN_SETTING", EXIT_MANIFEST),
    ("E_UNKNOWN_UNIT", EXIT_MANIFEST),
    ("E_UNSUPPORTED", EXIT_MANIFEST),
    ("E_UNSUPPORTED_ARCH", EXIT_RESOLUTION),
    ("E_VAR_UNSET", EXIT_MANIFEST),
    ("E_VERSION", EXIT_MANIFEST),
    ("W_PROGRAM_NOT_FOUND", EXIT_SUCCESS),
    ("W_SHADOWED", EXIT_SUCCESS),
];

/// The exit status of a diagnostic code (`spec/11` §2), read from [`CODES`].
///
/// A code that is not in the registry is a manifest error, as it was before the registry
/// existed. D9 keeps that fallback deliberately — a lookup on `&str` needs one — and makes the
/// *tests* the thing that guarantees nothing reaches it.
pub fn exit_status(code: &str) -> u8 {
    match CODES.iter().find(|(known, _)| *known == code) {
        Some((_, status)) => *status,
        None => EXIT_MANIFEST,
    }
}

/// A position in a manifest file. Line and column are 1-based; the column counts characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

impl Location {
    /// The location of byte offset `offset` in `text` (clamped to the end of `text`).
    pub fn at(file: &str, text: &str, offset: usize) -> Location {
        let mut offset = offset.min(text.len());
        while !text.is_char_boundary(offset) {
            offset -= 1;
        }
        let before = &text[..offset];
        let line = before.matches('\n').count() + 1;
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        let column = before[line_start..].chars().count() + 1;
        Location {
            file: file.to_string(),
            line,
            column,
        }
    }
}

/// One error with a stable code (`spec/11` §2), a message, an optional location and context lines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub code: &'static str,
    pub message: String,
    pub location: Option<Location>,
    /// Rendered as `= <line>`; a fix hint starts with `hint: `.
    pub notes: Vec<String>,
}

impl Diagnostic {
    pub fn new(code: &'static str, message: impl Into<String>) -> Diagnostic {
        Diagnostic {
            code,
            message: message.into(),
            location: None,
            notes: Vec::new(),
        }
    }

    pub fn at(mut self, location: Location) -> Diagnostic {
        self.location = Some(location);
        self
    }

    pub fn hint(mut self, hint: impl Into<String>) -> Diagnostic {
        self.notes.push(format!("hint: {}", hint.into()));
        self
    }
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "lodi: error {}: {}", self.code, self.message)?;
        if let Some(at) = &self.location {
            write!(f, "\n  --> {}:{}:{}", at.file, at.line, at.column)?;
        }
        for note in &self.notes {
            write!(f, "\n   = {note}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The registry is sorted and holds each code once, so a reader can find a code and a
    /// second row for one cannot hide behind the first (D9).
    #[test]
    fn the_code_registry_is_sorted_and_has_no_duplicate() {
        for pair in CODES.windows(2) {
            assert!(
                pair[0].0 < pair[1].0,
                "lodi::diag::CODES is out of order at {} / {}",
                pair[0].0,
                pair[1].0
            );
        }
        for (code, status) in CODES {
            assert_eq!(exit_status(code), *status, "{code}");
            if code.starts_with("W_") {
                assert_eq!(
                    *status, EXIT_SUCCESS,
                    "{code}: a warning never changes the exit"
                );
                continue;
            }
            assert!(
                code.starts_with("E_"),
                "{code} is neither an error nor a warning"
            );
            assert!(
                (2..=11).contains(status) && *status != 2,
                "{code} has exit {status}, which is not an error status of spec/10 §2"
            );
        }
        // The fallback stays, and is not something a registered code reaches.
        assert_eq!(exit_status("E_NONE_SUCH"), EXIT_MANIFEST);
    }

    #[test]
    fn location_counts_lines_and_characters() {
        let text = "a = 1\nb = \"é\" x\n";
        assert_eq!(Location::at("f", text, 0), Location::at("f", text, 0));
        let at = Location::at("f", text, text.find('x').unwrap());
        assert_eq!((at.line, at.column), (2, 9));
        let end = Location::at("f", text, 10_000);
        assert_eq!((end.line, end.column), (3, 1));
    }

    #[test]
    fn renders_the_spec_11_format() {
        let d = Diagnostic::new("E_TYPE", "expected a string")
            .at(Location {
                file: "lodi.toml".into(),
                line: 3,
                column: 7,
            })
            .hint("quote the value");
        assert_eq!(
            d.to_string(),
            "lodi: error E_TYPE: expected a string\n  --> lodi.toml:3:7\n   = hint: quote the value"
        );
    }
}
