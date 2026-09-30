//! M-1.0 T-8: the CLI surface is frozen, and the freeze is checked rather than promised.
//!
//! `docs/CLI.md` is the 1.x contract for the command line. This file is what makes it a contract
//! instead of a comment, in **both** directions:
//!
//! 1. every command and flag `docs/CLI.md` names is accepted by the real binary — one well-formed
//!    invocation per command in a scratch directory, whose exit status is **not** 2, with a
//!    nonsense flag beside it that **is** 2 as the negative control;
//! 2. every command and flag the parser accepts is in `docs/CLI.md` — the string literals of the
//!    dispatch's `match` arms and of each `parse_*` function of `src/main.rs`, read out of the
//!    source the same way `tests/home_containment.rs` and `tests/distro_seam.rs` read theirs;
//! 3. `lodi --help` and `docs/CLI.md` name the same commands, and `docs/CLI.md`'s exit-status
//!    table is `docs/ERRORS.md`'s;
//! 4. the deprecation resolver of `src/surface.rs` answers for a table entry and for nothing
//!    else, renders the documented `W_DEPRECATED` line, and changes no exit status;
//! 5. help at every level (LD-360): every command `docs/CLI.md` documents answers
//!    `lodi COMMAND --help`, `-h` and `lodi help COMMAND` with its part of the help text, and every
//!    part of the help text those forms can print is about a command `docs/CLI.md` documents;
//! 6. the first run (LD-380): `lodi --help` opens with the three scopes, a bare `lodi` prints
//!    them at exit 0 as `docs/CLI.md` says it does, and `README.md`'s quick start neither
//!    upgrades nor arms a machine by copy.
//!
//! Everything here is **offline and deterministic**: `PATH` is emptied, the roots are scratch
//! directories, and every invocation was chosen so that it fails — or succeeds — before any
//! network request is made. Nothing is written outside `CARGO_TARGET_TMPDIR`.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::diag;
use lodi::surface::{DEPRECATIONS, Deprecation, deprecation, typed};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    support::scratch(&format!("surface-{tag}"))
}

/// One invocation of the built binary in `dir`, with every root pointed inside `dir` and an
/// **empty** `PATH`, so that nothing on this machine — `podman` included — is reachable.
fn lodi(dir: &Path, args: &[String]) -> Output {
    lodi_with(dir, args, &[])
}

/// [`lodi`] with further variables set, `SUDO_UID` for one.
fn lodi_with(dir: &Path, args: &[String], vars: &[(&str, &str)]) -> Output {
    let home = dir.join("home");
    fs::create_dir_all(home.join(".config")).unwrap();
    fs::create_dir_all(home.join(".local/share")).unwrap();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", home.join(".config"))
        .env("XDG_DATA_HOME", home.join(".local/share"))
        .env("LODI_HOME", dir.join("store"))
        .envs(vars.iter().copied())
        .output()
        .expect("the lodi binary runs")
}

fn argv(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_string()).collect()
}

// --------------------------------------------------------- reading src/main.rs's parser ---

/// A file's source with its unit tests cut off and its comment lines removed. A `//` line can
/// name a flag this build does not implement — `src/main.rs`'s `parse_gc` names three of
/// `spec/03`'s on purpose — and a comment is not a surface the parser accepts.
fn parser_source() -> String {
    let text = fs::read_to_string(repo().join("src/main.rs")).expect("src/main.rs exists");
    let text = match text.find("#[cfg(test)]") {
        Some(at) => &text[..at],
        None => &text[..],
    };
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The body of every function whose name begins with `parse`, brace-matched. `fn parse` itself
/// is the dispatch's `match`; the rest are the per-command parsers.
fn parser_bodies(source: &str) -> Vec<String> {
    let mut bodies = Vec::new();
    let mut at = 0;
    while let Some(found) = source[at..].find("fn parse") {
        let start = at + found;
        let Some(open) = source[start..].find('{') else {
            break;
        };
        let open = start + open;
        let mut depth = 0usize;
        let mut in_string = false;
        let mut escaped = false;
        let mut end = None;
        for (i, ch) in source[open..].char_indices() {
            if in_string {
                match ch {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_string = false,
                    _ => {}
                }
                continue;
            }
            match ch {
                '"' => in_string = true,
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = Some(open + i);
                        break;
                    }
                }
                _ => {}
            }
        }
        let end = end.expect("a parse function has a matching closing brace");
        bodies.push(source[open..=end].to_string());
        at = end;
    }
    assert!(
        bodies.len() >= 8,
        "src/main.rs should have the dispatch and one parser per multi-argument command, found {}",
        bodies.len()
    );
    bodies
}

/// Every string literal in `body`.
fn literals(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut chars = body.char_indices().peekable();
    while let Some((_, ch)) = chars.next() {
        if ch != '"' {
            continue;
        }
        let mut literal = String::new();
        let mut escaped = false;
        for (_, c) in chars.by_ref() {
            if escaped {
                literal.push(c);
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                break;
            } else {
                literal.push(c);
            }
        }
        out.push(literal);
    }
    out
}

/// Whether a literal is a surface token — a command word, a flag, or the `--` separator — as
/// opposed to a diagnostic string such as `run (no task)` or a `format!` template.
fn is_surface_token(text: &str) -> bool {
    if text == "--" {
        return true;
    }
    let body = text.trim_start_matches('-');
    let dashes = text.len() - body.len();
    if body.is_empty() || dashes > 2 {
        return false;
    }
    body.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// Every command word and flag the parser of `src/main.rs` accepts.
fn parser_surface() -> BTreeSet<String> {
    let source = parser_source();
    let mut out = BTreeSet::new();
    for body in parser_bodies(&source) {
        for literal in literals(&body) {
            if is_surface_token(&literal) {
                out.insert(literal);
            }
        }
    }
    assert!(out.len() > 20, "the parser scan found only {out:?}");
    out
}

// ----------------------------------------------------------------- reading docs/CLI.md ---

/// The section of `text` headed by `heading`, up to the next heading of the same level.
fn section<'a>(text: &'a str, heading: &str) -> &'a str {
    let level = heading.split(' ').next().unwrap();
    let start = text
        .find(heading)
        .unwrap_or_else(|| panic!("no section `{heading}`"));
    let rest = &text[start..];
    match rest[1..].find(&format!("\n{level} ")) {
        Some(at) => &rest[..at + 1],
        None => rest,
    }
}

fn cli_md() -> String {
    fs::read_to_string(repo().join("docs/CLI.md")).expect("docs/CLI.md exists")
}

/// Every command `docs/CLI.md` documents, as the words after `lodi` in its `### \`lodi …\``
/// headings: `init`, `lock`, `home status`, `--version`.
fn documented_commands(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("### `lodi ") {
            let command = rest.trim_end().trim_end_matches('`');
            assert!(!command.is_empty(), "{line}");
            assert!(
                out.insert(command.to_string()),
                "{command} is documented twice"
            );
        }
    }
    assert!(!out.is_empty(), "docs/CLI.md documents no command");
    out
}

/// Every flag `docs/CLI.md` documents, per command: the first cell of each row of a command
/// section's flag table.
fn documented_flags(text: &str) -> BTreeMap<String, BTreeSet<String>> {
    let mut out = BTreeMap::new();
    for command in documented_commands(text) {
        let body = section(text, &format!("### `lodi {command}`"));
        let mut flags = BTreeSet::new();
        for line in body.lines() {
            let line = line.trim();
            let Some(rest) = line.strip_prefix("| `") else {
                continue;
            };
            let Some(flag) = rest.split('`').next() else {
                continue;
            };
            if flag.starts_with('-') {
                assert!(
                    flags.insert(flag.to_string()),
                    "docs/CLI.md lists {flag} twice under lodi {command}"
                );
            }
        }
        out.insert(command, flags);
    }
    out
}

/// Every command word and flag `docs/CLI.md` names, flattened the way the parser scan sees them.
fn documented_surface(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for (command, flags) in documented_flags(text) {
        for word in command.split(' ') {
            out.insert(word.to_string());
        }
        out.extend(flags);
    }
    out
}

/// An exit-status table: status -> meaning. Used on the `## Exit statuses …` section of both
/// `docs/CLI.md` and `docs/ERRORS.md`, which must be the same table.
fn exit_status_table(text: &str) -> BTreeMap<u8, String> {
    let heading = "## Exit statuses";
    let body = section(text, heading);
    let mut out = BTreeMap::new();
    for line in body.lines() {
        let line = line.trim();
        if !line.starts_with('|') {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        if cells.len() != 2 {
            continue;
        }
        let Ok(status) = cells[0].parse::<u8>() else {
            continue;
        };
        assert!(!cells[1].is_empty(), "exit status {status} has no meaning");
        assert!(
            out.insert(status, cells[1].to_string()).is_none(),
            "exit status {status} is listed twice"
        );
    }
    assert!(
        out.len() > 5,
        "an exit-status table with {} rows",
        out.len()
    );
    out
}

// ---------------------------------------------- 1. docs/CLI.md -> the real binary (a) ---

/// One documented command, a well-formed invocation of it, and a nonsense flag beside it.
///
/// Every invocation here was chosen to be offline: it is refused, or answered, before any
/// network request. `WANT_NOT_USAGE` is not an expected status — the point is only that it is
/// not 2 — but it is recorded so that a change of behaviour is visible in the failure message.
struct Case {
    command: &'static str,
    well_formed: &'static [&'static str],
    nonsense: &'static [&'static str],
}

const CASES: &[Case] = &[
    Case {
        command: "--version",
        well_formed: &["--version"],
        nonsense: &["--version", "--frobnicate"],
    },
    // Since LD-360 `--help` beside anything is help, so the negative control is `--help` about
    // a command that does not exist, and `-h` is the same request.
    Case {
        command: "--help",
        well_formed: &["--help"],
        nonsense: &["frobnicate", "--help"],
    },
    Case {
        command: "--help",
        well_formed: &["init", "-h"],
        nonsense: &["home", "frobnicate", "-h"],
    },
    Case {
        command: "help",
        well_formed: &["help", "home", "plan"],
        nonsense: &["help", "frobnicate"],
    },
    Case {
        command: "init",
        well_formed: &[
            "init",
            "--name",
            "surface",
            "--base",
            "debian:bookworm",
            "--force",
        ],
        nonsense: &["init", "--frobnicate"],
    },
    // Never bare `lodi lock`: that resolves, and this file makes no network request.
    Case {
        command: "lock",
        well_formed: &["lock", "--check"],
        nonsense: &["lock", "--frobnicate"],
    },
    // A name the catalogue does not carry is `E_NO_RECIPE` before anything is fetched, and
    // `--base` with an empty `PATH` is `E_NO_RUNTIME` before anything is resolved.
    Case {
        command: "shell",
        well_formed: &["shell", "--no-nest", "surface-has-no-such-tool"],
        nonsense: &["shell", "--frobnicate", "surface-has-no-such-tool"],
    },
    Case {
        command: "shell",
        well_formed: &[
            "shell",
            "--base",
            "debian:bookworm",
            "surface-has-no-such-package",
        ],
        nonsense: &["shell", "--base", "debian:bookworm", "--frobnicate", "p"],
    },
    Case {
        command: "develop",
        well_formed: &["develop", "--no-nest", "--", "true"],
        nonsense: &["develop", "--frobnicate", "--", "true"],
    },
    Case {
        command: "run",
        well_formed: &["run", "--no-nest", "build"],
        nonsense: &["run", "--frobnicate", "build"],
    },
    Case {
        command: "gc",
        well_formed: &["gc", "--dry-run", "--keep-days", "7", "--images", "-v"],
        nonsense: &["gc", "--frobnicate"],
    },
    Case {
        command: "search",
        well_formed: &["search", "--registry", "python"],
        nonsense: &["search", "--frobnicate", "python"],
    },
    Case {
        command: "search",
        well_formed: &["search", "--distro", "python"],
        nonsense: &["search", "--distro", "--frobnicate", "python"],
    },
    Case {
        command: "info",
        well_formed: &["info", "python"],
        nonsense: &["info", "--frobnicate"],
    },
    Case {
        command: "home plan",
        well_formed: &["home", "plan"],
        nonsense: &["home", "plan", "--frobnicate"],
    },
    Case {
        command: "home apply",
        well_formed: &["home", "apply", "--overwrite-drift", "--locked"],
        nonsense: &["home", "apply", "--frobnicate"],
    },
    // `--out` names a directory inside the scratch HOME, which is the only place the home
    // scope writes; the manifest lands in the case directory's own home and nowhere else.
    Case {
        command: "home import",
        well_formed: &["home", "import", "--out", "bundle", "--force"],
        nonsense: &["home", "import", "--frobnicate"],
    },
    // `--stdout` prints the manifest and writes nothing at all (LD-325).
    Case {
        command: "home import",
        well_formed: &["home", "import", "--stdout"],
        nonsense: &["home", "import", "--stdout", "--frobnicate"],
    },
    // `home init` is `home import` under the name the home scope starts with (LD-382): the same
    // flags, so the same two cases.
    Case {
        command: "home init",
        well_formed: &["home", "init", "--out", "bundle", "--force"],
        nonsense: &["home", "init", "--frobnicate"],
    },
    Case {
        command: "home init",
        well_formed: &["home", "init", "--stdout"],
        nonsense: &["home", "init", "--stdout", "--frobnicate"],
    },
    Case {
        command: "home status",
        well_formed: &["home", "status"],
        nonsense: &["home", "status", "--frobnicate"],
    },
    Case {
        command: "trust",
        well_formed: &["trust", "--revoke"],
        nonsense: &["trust", "--frobnicate"],
    },
];

/// The host cases, which need a scratch `--root` and so cannot be a `const`.
/// The root is a directory this test made inside `CARGO_TARGET_TMPDIR` and never armed, so both
/// invocations stop at the arming gate with nothing below the root opened. `host arm` gets a
/// scratch root of its own beside it, so that arming it cannot change what the others see.
fn host_cases(root: &Path) -> Vec<(&'static str, Vec<String>, Vec<String>)> {
    let arm_root = format!("{}-arm", root.to_string_lossy());
    fs::create_dir_all(&arm_root).expect("the arm case's own scratch root");
    let root = root.to_string_lossy().into_owned();
    vec![
        (
            "host arm",
            // check-host-safety: refusal — the root is a scratch directory this test owns.
            vec!["host".into(), "arm".into(), "--root".into(), arm_root],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "arm".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host plan",
            // check-host-safety: refusal — the root is a scratch directory this test owns.
            vec![
                "host".into(),
                "plan".into(),
                "--root".into(),
                root.clone(),
                "--no-home".into(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "plan".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host plan",
            vec![
                // check-host-safety: refusal — the same scratch root, which is not armed.
                "host".into(),
                "plan".into(),
                "github:owner/hosts".into(),
                "--ref".into(),
                "main".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "plan".into(),
                "--ref".into(),
                "main".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host apply",
            vec![
                // check-host-safety: refusal — the same scratch root, which is not armed.
                "host".into(),
                "apply".into(),
                "github:owner/hosts".into(),
                "--rev".into(),
                "0".repeat(40),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "apply".into(),
                "github:owner/hosts".into(),
                "--rev".into(),
                "0".repeat(40),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host plan",
            vec![
                // check-host-safety: refusal — the same scratch root, which is not armed.
                "host".into(),
                "plan".into(),
                "github:owner/hosts".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "plan".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host import",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--root".into(),
                root.clone(),
                "--out".into(),
                format!("{root}-import-out"),
                "--force".into(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host import",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--root".into(),
                root.clone(),
                "--stdout".into(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--stdout".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        // `--dry-run` shows what an import would change and writes nothing (LD-378); beside
        // `--stdout`, which writes nothing either, it is the usage error.
        (
            "host import",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--root".into(),
                root.clone(),
                "--dry-run".into(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                "--dry-run".into(),
                "--stdout".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        // A positional `SOURCE` and `--host` (LD-379): the root is unarmed, so each stops at
        // the arming gate; a `--host` with no value is the usage error.
        (
            "host plan",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "plan".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "plan".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host import",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "import".into(),
                format!("{root}-hosts"),
                "--stdout".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host apply",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "apply".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "apply".into(),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        // M-Pin's verbs (LD-397): each stops at the arming gate; `--to` with no value, `--to`
        // on `unpin`, `--all` on `versions` and a missing name are the usage errors.
        (
            "host versions",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "versions".into(),
                "curl".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "versions".into(),
                "curl".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host versions",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "versions".into(),
                "curl".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "versions".into(),
                "--all".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host pin",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "pin".into(),
                "curl".into(),
                "--to".into(),
                "2026-09-01".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "pin".into(),
                "--all".into(),
                "--to".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host pin",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "pin".into(),
                "--all".into(),
                "--to".into(),
                "2026-09-01".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "pin".into(),
                "--to".into(),
                "2026-09-01".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host unpin",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "unpin".into(),
                "curl".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "unpin".into(),
                "curl".into(),
                "--to".into(),
                "2026-09-01".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host unpin",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "unpin".into(),
                "--all".into(),
                format!("{root}-hosts"),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "unpin".into(),
                "--all".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "host apply",
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "apply".into(),
                "--root".into(),
                root.clone(),
                "--overwrite-drift".into(),
                "--resolved".into(),
                "surface-journal".into(),
                "--no-update".into(),
                "--no-home".into(),
                "--unsupported-partial-upgrade".into(),
            ],
            vec![
                // check-host-safety: refusal — the same scratch root.
                "host".into(),
                "apply".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        // The top-level verbs of a repository (LD-416): each names the unarmed scratch root, so a
        // well-formed one stops at the repository or the gate, never at the parser.
        (
            "apply",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            vec![
                "apply".into(),
                "github:owner/hosts".into(),
                "--host".into(),
                "box".into(),
                "--ref".into(),
                "main".into(),
                "--yes".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "apply".into(),
                "--frobnicate".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "plan",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            vec![
                "plan".into(),
                "github:owner/hosts".into(),
                "--rev".into(),
                "0".repeat(40),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "plan".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "plan",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            vec![
                "plan".into(),
                "github:owner/hosts".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "plan".into(),
                "--no-home".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "import",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            vec![
                "import".into(),
                "--yes".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "import".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "update",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            vec![
                "update".into(),
                "--yes".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "update".into(),
                "a".into(),
                "b".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "boot confirm",
            // The same scratch root, which is not armed (bv-1).
            vec![
                "boot".into(),
                "confirm".into(),
                "--root".into(),
                root.clone(),
            ],
            vec![
                "boot".into(),
                "confirm".into(),
                "now".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
    ]
}

/// (a), first direction: every command and flag `docs/CLI.md` names is accepted by the real
/// binary, and a nonsense flag beside it is the usage error — the negative control that stops
/// "not 2" from being true of everything.
#[test]
fn every_documented_command_and_flag_is_accepted_by_the_binary() {
    let dir = scratch("accepted");
    let root = dir.join("unarmed-root");
    fs::create_dir_all(&root).unwrap();

    let mut exercised: BTreeSet<String> = BTreeSet::new();
    let mut run_case = |command: &str, well_formed: &[String], nonsense: &[String], n: usize| {
        let here = dir.join(format!("case-{n}"));
        fs::create_dir_all(&here).unwrap();

        let ok = lodi(&here, well_formed);
        assert_ne!(
            ok.status.code(),
            Some(2),
            "`lodi {}` is documented but refused as a usage error:\n{}",
            well_formed.join(" "),
            String::from_utf8_lossy(&ok.stderr)
        );
        let bad = lodi(&here, nonsense);
        assert_eq!(
            bad.status.code(),
            Some(2),
            "`lodi {}` should be a usage error:\n{}",
            nonsense.join(" "),
            String::from_utf8_lossy(&bad.stderr)
        );
        for word in command.split(' ') {
            exercised.insert(word.to_string());
        }
        for word in well_formed {
            if word.starts_with('-') {
                exercised.insert(word.clone());
            }
        }
    };

    for (n, case) in CASES.iter().enumerate() {
        run_case(
            case.command,
            &argv(case.well_formed),
            &argv(case.nonsense),
            n,
        );
    }
    for (n, (command, well_formed, nonsense)) in host_cases(&root).iter().enumerate() {
        run_case(command, well_formed, nonsense, CASES.len() + n);
    }

    // Every surface the document names was actually typed at the binary above: a row nobody
    // runs would make this direction vacuous.
    let documented = documented_surface(&cli_md());
    let untested: Vec<&String> = documented.difference(&exercised).collect();
    assert!(
        untested.is_empty(),
        "docs/CLI.md names {untested:?}, which no invocation in this test exercises"
    );
    fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------------- 2. the real parser -> docs/CLI.md (a) ---

/// (a), second direction: every command word and flag the parser accepts has a row in
/// `docs/CLI.md`, with a negative control that the comparison notices a missing one.
#[test]
fn every_surface_the_parser_accepts_is_documented() {
    let parser = parser_surface();
    let documented = documented_surface(&cli_md());

    let undocumented: Vec<&String> = parser.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "src/main.rs accepts {undocumented:?}, which docs/CLI.md does not name. \
         The surface is frozen: document it, or take it out."
    );
    let phantom: Vec<&String> = documented.difference(&parser).collect();
    assert!(
        phantom.is_empty(),
        "docs/CLI.md names {phantom:?}, which src/main.rs's parser never accepts"
    );

    // Negative control: drop one real surface from the documented set and the check must fail.
    let mut doctored = documented.clone();
    assert!(doctored.remove("--force"), "`--force` is a documented flag");
    assert!(
        parser.difference(&doctored).count() == 1,
        "the comparison does not notice a flag that lost its row"
    );
    // And a surface the parser does not accept must show up as a phantom.
    let mut invented = documented;
    invented.insert("--frobnicate".to_string());
    assert!(
        invented.difference(&parser).count() == 1,
        "the comparison does not notice a documented flag the parser refuses"
    );
}

// ------------------------------- 3. --help, docs/CLI.md and docs/ERRORS.md agree (b) ---

/// The commands `lodi --help` names, read from its command groups: each line after the
/// `Commands` header is a scope label, then `lodi` and a comma-separated list. The first item
/// carries the scope word its followers share (`host arm, import` is `host arm` and
/// `host import`); a bracketed argument (`help [COMMAND]`) is not part of the name.
fn help_commands() -> BTreeSet<String> {
    let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .arg("--help")
        .output()
        .expect("the lodi binary runs");
    assert!(out.status.success());
    let text = String::from_utf8(out.stdout).expect("--help is UTF-8");
    let groups = text
        .split("\nCommands (")
        .nth(1)
        .expect("--help has a Commands header");
    let mut commands = BTreeSet::new();
    for line in groups.lines().skip(1).take_while(|l| l.starts_with("  ")) {
        let list = line
            .split_once(" lodi ")
            .expect("a group lists lodi commands")
            .1;
        let mut scope = String::new();
        for (at, item) in list.split(", ").enumerate() {
            let words: Vec<&str> = item
                .split_whitespace()
                .filter(|w| !w.starts_with('['))
                .collect();
            if at == 0 && words.len() == 2 {
                scope = format!("{} ", words[0]);
                commands.insert(words.join(" "));
            } else {
                commands.insert(format!("{scope}{}", words.join(" ")));
            }
        }
    }
    assert!(commands.len() > 10, "--help named only {commands:?}");
    commands
}

/// (b): `lodi --help` and `docs/CLI.md` name the same commands.
#[test]
fn help_and_the_document_name_the_same_commands() {
    let documented = documented_commands(&cli_md());
    let help = help_commands();
    // Two empty sets are equal. Both scans must really have found the inventory.
    assert!(
        documented.len() >= 15 && documented.contains("init") && documented.contains("home status"),
        "docs/CLI.md's command sections did not parse: {documented:?}"
    );
    assert_eq!(
        documented, help,
        "`lodi --help` and docs/CLI.md disagree about the command inventory"
    );

    // The negative control: a command in one and not the other must fail the comparison.
    let mut missing = documented.clone();
    missing.remove("init");
    assert_ne!(
        missing, help,
        "a command missing from docs/CLI.md went unnoticed"
    );
    let mut extra = documented.clone();
    extra.insert("frobnicate".to_string());
    assert_ne!(extra, help, "a command docs/CLI.md invented went unnoticed");
}

// ------------------------------------------------ 5. help at every level (LD-360) ---

/// The printed part of the help text for one invocation, with its status.
fn help_part(args: &[&str]) -> (Option<i32>, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .env("PATH", "")
        .output()
        .expect("the lodi binary runs");
    (
        out.status.code(),
        String::from_utf8(out.stdout).expect("help is UTF-8"),
    )
}

/// The command a part of the help text is about: the words its usage lines share. The usage
/// lines are the `lodi` lines of its `Usage:` block; examples and details are not read.
fn part_subject(part: &str) -> String {
    let usage = part
        .split("\nUsage:\n")
        .nth(1)
        .unwrap_or_else(|| panic!("a help part with no usage block:\n{part}"));
    let keys: Vec<Vec<String>> = usage
        .lines()
        .take_while(|l| !l.is_empty())
        .filter_map(|l| l.strip_prefix("  lodi "))
        .map(|rest| {
            rest.split_whitespace()
                .take_while(|w| w.chars().all(|c| c.is_ascii_lowercase()))
                .map(str::to_string)
                .collect()
        })
        .collect();
    assert!(!keys.is_empty(), "a help part with no usage line:\n{part}");
    let mut shared = keys[0].clone();
    for key in &keys[1..] {
        let common = shared.iter().zip(key).take_while(|(a, b)| a == b).count();
        shared.truncate(common);
    }
    shared.join(" ")
}

/// Everything `lodi help` answers about: each command `docs/CLI.md` documents, bar the two
/// global flags, and each scope of two or more of those commands. A scope of one command, as
/// `boot` of `boot confirm` (bv-1), prints that command's usage alone, so its part is about it.
fn help_subjects(documented: &BTreeSet<String>) -> BTreeSet<String> {
    let mut subjects: BTreeSet<String> = documented
        .iter()
        .filter(|c| !c.starts_with('-'))
        .cloned()
        .collect();
    for command in documented.iter().filter(|c| c.contains(' ')) {
        let scope = command.split(' ').next().unwrap();
        let prefix = format!("{scope} ");
        if documented.iter().filter(|c| c.starts_with(&prefix)).count() > 1 {
            subjects.insert(scope.to_string());
        }
    }
    assert!(
        subjects.contains("help") && subjects.contains("home") && subjects.contains("host arm"),
        "{subjects:?}"
    );
    subjects
}

/// 5, first direction: every command `docs/CLI.md` documents — and each scope its commands
/// belong to — answers `lodi COMMAND --help`, `lodi COMMAND -h` and `lodi help COMMAND` with
/// exit 0, the same part of `lodi --help` each time, and that part is about that command.
/// Second direction: every part of the help text these forms print is about a command
/// `docs/CLI.md` documents (or a scope of them), so no help reaches a command the document does
/// not name. The negative controls: help about an undocumented command is a usage error, and a
/// part is recognised as being about something else when it is.
#[test]
fn help_at_every_level_and_the_document_name_the_same_commands() {
    let documented = documented_commands(&cli_md());
    let scratch_root = scratch("help-root");
    let root = scratch_root.to_string_lossy().into_owned();
    let (_, whole) = help_part(&["--help"]);
    let subjects = help_subjects(&documented);
    for subject in &subjects {
        let words: Vec<&str> = subject.split(' ').collect();
        let mut forms = vec![
            [&words[..], &["--help"]].concat(),
            [&words[..], &["-h"]].concat(),
            [&["help"], &words[..]].concat(),
        ];
        if words == ["help"] {
            forms = vec![vec!["help", "help"], vec!["--help", "help"]];
        }
        if words[0] == "host" && words.len() > 1 {
            for form in &mut forms {
                form.extend(["--root", root.as_str()]);
            }
        }
        let mut seen: Option<String> = None;
        for form in &forms {
            let (status, part) = help_part(form);
            assert_eq!(status, Some(0), "`lodi {}` is not help", form.join(" "));
            assert_ne!(
                part,
                whole,
                "`lodi {}` printed the whole text",
                form.join(" ")
            );
            assert_eq!(&part_subject(&part), subject, "`lodi {}`", form.join(" "));
            let about = part_subject(&part);
            assert!(
                documented.contains(&about) || subjects.contains(&about),
                "`lodi {}` prints help about `{about}`, which docs/CLI.md does not document",
                form.join(" ")
            );
            match &seen {
                None => seen = Some(part),
                Some(same) => assert_eq!(&part, same, "`lodi {}`", form.join(" ")),
            }
        }
    }
    for bare in ["home", "host"] {
        let (status, part) = help_part(&[bare]);
        assert_eq!(status, Some(0), "bare `lodi {bare}`");
        assert_eq!(part_subject(&part), bare);
    }
    // Negative controls: an undocumented command has no help, and a part about `home plan` is
    // not mistaken for one about `home`.
    assert_eq!(help_part(&["help", "frobnicate"]).0, Some(2));
    assert_eq!(help_part(&["frobnicate", "--help"]).0, Some(2));
    assert_eq!(
        part_subject(&help_part(&["home", "plan", "-h"]).1),
        "home plan"
    );
    assert!(
        !scratch_root.join("etc").exists(),
        "a help request read its --root"
    );
    fs::remove_dir_all(&scratch_root).unwrap();
}

/// The block of `part` under the header `name`: its lines up to the next blank line.
fn help_block<'a>(part: &'a str, name: &str) -> Vec<&'a str> {
    part.split(&format!("\n{name}\n"))
        .nth(1)
        .map(|rest| rest.lines().take_while(|l| !l.is_empty()).collect())
        .unwrap_or_default()
}

/// An example line as the user types it: indented by two, with `sudo` where root is needed.
fn example_command(line: &str) -> &str {
    let typed = line.strip_prefix("  ").unwrap_or(line);
    typed.strip_prefix("sudo ").unwrap_or(typed)
}

/// Whether `command` runs the lodi command `subject`: `lodi SUBJECT`, then an argument or an end.
fn runs(command: &str, subject: &str) -> bool {
    command
        .strip_prefix(&format!("lodi {subject}"))
        .is_some_and(|rest| rest.is_empty() || rest.starts_with(' '))
}

/// The words no help text uses: they name how lodi works inside, not what it does for you.
const HELP_JARGON: [&str; 3] = ["journalled transaction", "realized", "realised"];

/// `lodi --help` fits one terminal of 80 columns and 40 lines. Its opening names the first
/// command of each scope, an Examples block shows each of them typed out, and no help text uses
/// the jargon that says how lodi works inside rather than what it does.
#[test]
fn top_level_help_fits_one_screen_with_examples() {
    let (status, whole) = help_part(&["--help"]);
    assert_eq!(status, Some(0));
    let lines: Vec<&str> = whole.lines().collect();
    assert!(
        lines.len() <= 40,
        "--help is {} lines long:\n{whole}",
        lines.len()
    );
    for line in &lines {
        assert!(line.chars().count() <= 80, "wider than 80 columns: {line}");
    }
    let first: Vec<&str> = lines[3..7]
        .iter()
        .filter_map(|l| l.split("  ").map(str::trim).find(|c| c.contains("lodi ")))
        .map(example_command)
        .collect();
    assert_eq!(
        first.len(),
        4,
        "the opening names no first commands:\n{whole}"
    );
    let examples = help_block(&whole, "Examples:");
    assert!(
        (1..=10).contains(&examples.len()),
        "--help has no Examples block:\n{whole}"
    );
    for command in &first {
        let subject = command.strip_prefix("lodi ").unwrap();
        assert!(
            examples.iter().any(|e| runs(example_command(e), subject)),
            "no example of {command}:\n{whole}"
        );
    }
    let mut every = vec![whole.clone()];
    for subject in help_subjects(&documented_commands(&cli_md())) {
        let words: Vec<&str> = subject.split(' ').collect();
        every.push(help_part(&[&["help"], &words[..]].concat()).1);
    }
    for text in &every {
        for word in HELP_JARGON {
            assert!(
                !text.to_lowercase().contains(word),
                "help says {word:?}:\n{text}"
            );
        }
    }
}

/// Every command `docs/CLI.md` documents, and each scope, answers `lodi help COMMAND` with a
/// one-line summary of what it does, then its usage lines, then an Examples block of one to
/// three commands that each run that command with flags its usage line offers.
#[test]
fn every_command_help_shows_examples() {
    for subject in help_subjects(&documented_commands(&cli_md())) {
        let words: Vec<&str> = subject.split(' ').collect();
        let (status, part) = help_part(&[&["help"], &words[..]].concat());
        assert_eq!(status, Some(0), "{subject}");
        let lines: Vec<&str> = part.lines().collect();
        assert!(
            lines.len() > 2 && !lines[0].trim().is_empty() && !lines[0].starts_with(' '),
            "{subject}: no summary line:\n{part}"
        );
        assert_eq!(
            lines[1], "",
            "{subject}: the summary is not one line:\n{part}"
        );
        assert_eq!(
            lines[2], "Usage:",
            "{subject}: no usage after the summary:\n{part}"
        );
        let usage = help_block(&part, "Usage:");
        assert!(
            usage.iter().any(|l| runs(example_command(l), &subject)),
            "{subject}: no usage line:\n{part}"
        );
        let examples = help_block(&part, "Examples:");
        assert!(
            (1..=3).contains(&examples.len()),
            "{subject}: {} examples:\n{part}",
            examples.len()
        );
        let offered = usage.join(" ");
        for example in &examples {
            let command = example_command(example);
            assert!(runs(command, &subject), "{subject}: {example}");
            for flag in command.split_whitespace().filter(|w| w.starts_with("--")) {
                assert!(
                    flag == "--" || offered.contains(flag),
                    "{subject}: {example} uses {flag}, which its usage does not offer"
                );
            }
        }
        let order = [part.find("\nUsage:\n"), part.find("\nExamples:\n")];
        assert!(
            order[0] < order[1],
            "{subject}: examples before usage:\n{part}"
        );
    }
}

/// (b): `docs/CLI.md`'s exit-status table is `docs/ERRORS.md`'s, and every code a command
/// section names is a registered code at the status that section puts it under.
#[test]
fn the_exit_statuses_agree_with_the_error_catalogue() {
    let cli = cli_md();
    let errors = fs::read_to_string(repo().join("docs/ERRORS.md")).expect("docs/ERRORS.md exists");
    let statuses = exit_status_table(&cli);
    // Two empty maps are equal, so the comparison below would be vacuous if the section heading
    // ever moved and the parse came back with nothing.
    assert!(
        statuses.len() >= 10 && statuses.contains_key(&0) && statuses.contains_key(&2),
        "docs/CLI.md's exit-status table did not parse: {statuses:?}"
    );
    assert_eq!(
        statuses,
        exit_status_table(&errors),
        "docs/CLI.md's exit-status table is not docs/ERRORS.md's"
    );

    // The negative control: reword one meaning and the comparison must notice. A copy nobody
    // would catch drifting is the reason LD-249 keeps the table in two places at all.
    let mut reworded = statuses.clone();
    let (status, meaning) = reworded
        .iter()
        .next()
        .map(|(k, v)| (*k, v.clone()))
        .unwrap();
    reworded.insert(status, format!("{meaning} (and something nobody wrote)"));
    assert_ne!(
        reworded,
        exit_status_table(&errors),
        "a reworded meaning went unnoticed"
    );

    let registered: BTreeSet<&str> = diag::CODES.iter().map(|(code, _)| *code).collect();
    let mut seen = 0usize;
    for command in documented_commands(&cli) {
        let body = section(&cli, &format!("### `lodi {command}`"));
        for line in body.lines() {
            let line = line.trim();
            if !line.starts_with('|') {
                continue;
            }
            let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
            if cells.len() != 3 {
                continue;
            }
            let Ok(status) = cells[0].parse::<u8>() else {
                continue;
            };
            assert!(
                statuses.contains_key(&status),
                "lodi {command} names exit status {status}, which no exit-status table has"
            );
            for token in cells[1].split('`') {
                if !token.starts_with("E_") {
                    continue;
                }
                assert!(
                    registered.contains(token),
                    "lodi {command} names {token}, which lodi::diag::CODES does not carry"
                );
                assert_eq!(
                    diag::exit_status(token),
                    status,
                    "docs/CLI.md puts {token} under exit {status} for lodi {command}"
                );
                seen += 1;
            }
        }
    }
    assert!(seen > 50, "only {seen} code cells were checked");
}

// ------------------------------------------------ 4. the deprecation mechanism (c), (d) ---

/// The table the **test** supplies, which is the shape a real one would have. The committed
/// table deprecates no command and no flag; this is what proves the resolver works for them.
const SUPPLIED: &[Deprecation] = &[
    Deprecation {
        surface: "home status",
        announced_in: "1.1",
        removable_in: "1.2",
        replacement: "home plan",
    },
    Deprecation {
        surface: "init --force",
        announced_in: "1.3",
        removable_in: "1.4",
        replacement: "",
    },
];

/// (c): the resolver returns the documented line for a table entry and nothing for a surface
/// with no entry — including every surface this build really accepts, against the committed
/// table, which is how "no command and no flag is deprecated" is proved rather than asserted.
#[test]
fn the_resolver_answers_for_an_entry_and_for_nothing_else() {
    let warning = deprecation(SUPPLIED, "home status").expect("the entry resolves");
    assert_eq!(warning.code, "W_DEPRECATED");
    assert_eq!(
        warning.to_string(),
        "lodi: warning W_DEPRECATED: `lodi home status` is deprecated since 1.1 and may be \
         removed in 1.2; use `lodi home plan` instead"
    );
    let no_replacement = deprecation(SUPPLIED, "init --force").expect("the entry resolves");
    assert_eq!(
        no_replacement.to_string(),
        "lodi: warning W_DEPRECATED: `lodi init --force` is deprecated since 1.3 and may be \
         removed in 1.4; there is no replacement"
    );

    // The negative control: a near miss is not a match, and nothing else in the surface is.
    for absent in ["home", "status", "home status extra", "--force", "init"] {
        assert_eq!(
            deprecation(SUPPLIED, absent),
            None,
            "{absent} resolved against a table that has no entry for it"
        );
    }

    // (d): the committed table's one row is a manifest table (D5), so no surface this build
    // accepts resolves at all, and neither does the row's own spelling typed as an argument.
    assert!(DEPRECATIONS.iter().all(|d| d.manifest().is_some()));
    assert_eq!(deprecation(DEPRECATIONS, "home.toml `[files]`"), None);
    for token in parser_surface() {
        assert_eq!(deprecation(DEPRECATIONS, &token), None, "{token}");
    }
}

/// (c): the surfaces the dispatch derives from an argv are the ones a table row is written in,
/// and a table entry is announced exactly once however the invocation reached it.
#[test]
fn the_dispatch_derives_the_surfaces_a_table_row_is_written_in() {
    let words = |args: &[&str]| -> Vec<OsString> { args.iter().map(OsString::from).collect() };

    assert_eq!(
        typed(&words(&["home", "status"])),
        vec!["home".to_string(), "home status".to_string()]
    );
    assert_eq!(
        typed(&words(&["init", "--force"])),
        vec![
            "--force".to_string(),
            "init".to_string(),
            "init --force".to_string()
        ]
    );
    assert_eq!(typed(&words(&["--help"])), vec!["--help".to_string()]);
    assert!(typed(&words(&[])).is_empty());
    // Arguments after a bare `--` are the user's own command and are never surfaces of lodi.
    assert_eq!(
        typed(&words(&["develop", "--", "git", "--version"])),
        vec!["develop".to_string()]
    );

    // One entry, one line, whatever else the invocation carries.
    let lines: Vec<String> = typed(&words(&["home", "status"]))
        .iter()
        .filter_map(|s| deprecation(SUPPLIED, s))
        .map(|w| w.to_string())
        .collect();
    assert_eq!(lines.len(), 1, "{lines:?}");
    assert!(lines[0].contains("W_DEPRECATED"));
}

/// (c): the warning never changes an exit status. With no command or flag in the committed
/// table, no invocation of the real binary over a home with no manifest prints `W_DEPRECATED` —
/// `lodi --help` included, which is the one surface a user reads — and the statuses are the ones
/// every other test in this repository pins. The
/// dispatch reaches the resolver through exactly one call site, whose only effect is a line on
/// standard error: the source lint below is what keeps it that way.
#[test]
fn the_deprecation_point_is_one_call_site_that_only_prints() {
    let source = parser_source();
    assert_eq!(
        source.matches("surface::deprecation(").count(),
        1,
        "src/main.rs must reach the resolver through exactly one call site"
    );
    let at = source.find("for surface in lodi::surface::typed(").expect(
        "src/main.rs derives the invocation's surfaces once, before the dispatch (M-1.0 T-8)",
    );
    let loop_end = at + source[at..].find("\n    }").expect("the loop is closed") + "\n    }".len();
    let body = &source[at..loop_end];
    assert!(body.contains("eprintln!"), "{body}");
    for forbidden in ["return", "ExitCode", "std::process::exit", "exit_status"] {
        assert!(
            !body.contains(forbidden),
            "the deprecation point must not affect the exit status: it names `{forbidden}`"
        );
    }

    let dir = scratch("no-warning");
    for case in CASES {
        let out = lodi(&dir, &argv(case.well_formed));
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            !stderr.contains("W_DEPRECATED"),
            "`lodi {}` printed a deprecation warning, and no command is deprecated: {stderr}",
            case.well_formed.join(" ")
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("W_DEPRECATED"),
            "`lodi {}` printed W_DEPRECATED on standard output",
            case.well_formed.join(" ")
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// The one line a `home.toml` using the 1.0 `[files]` table prints per run from 1.2 (decision
/// D5, LD-324), exactly as `docs/CLI.md`'s Deprecations section quotes it.
const FILES_WARNING: &str = "lodi: warning W_DEPRECATED: `[files]` in home.toml is deprecated \
                             since 1.2 and may be removed in 2.0; use `[home.file]` (rename \
                             `content` to `text`)";

/// (d): the table has exactly two homes and they hold the same rows. Every row of
/// `docs/CLI.md`'s Deprecations table is a row of `src/surface.rs`'s `DEPRECATIONS`, cell for
/// field, and the other way round; from 1.2 that is the one row of `[files]` in `home.toml`
/// (D5), whose line the section quotes in a `text` block, character for character.
#[test]
fn the_document_s_deprecation_table_is_the_committed_one() {
    let text = cli_md();
    let body = section(&text, "## Deprecations");
    let rows: Vec<Vec<String>> = body
        .lines()
        .map(str::trim)
        .filter(|line| {
            line.starts_with("| ") && !line.starts_with("| Surface") && !line.starts_with("|---")
        })
        .map(|line| {
            line.trim_start_matches("| ")
                .trim_end_matches(" |")
                .split(" | ")
                .map(str::to_string)
                .collect()
        })
        .collect();
    let committed: Vec<Vec<String>> = DEPRECATIONS
        .iter()
        .map(|d| {
            [d.surface, d.announced_in, d.removable_in, d.replacement]
                .iter()
                .map(|cell| (*cell).to_string())
                .collect()
        })
        .collect();
    assert_eq!(
        rows, committed,
        "docs/CLI.md's deprecation table and src/surface.rs's DEPRECATIONS differ"
    );
    assert!(
        DEPRECATIONS.contains(&Deprecation {
            surface: "home.toml `[files]`",
            announced_in: "1.2",
            removable_in: "2.0",
            replacement: "`[home.file]` (rename `content` to `text`)",
        }),
        "the committed table has no row for `[files]` in home.toml (D5): {DEPRECATIONS:?}"
    );
    // A quoted line longer than the page is wrapped onto an indented next line.
    assert!(
        fenced(body, "text")
            .iter()
            .any(|block| block.join(" ").contains(FILES_WARNING)),
        "docs/CLI.md's Deprecations section does not quote the line `[files]` prints:\n\
         {FILES_WARNING}"
    );
}

/// `home.toml` in the scratch home [`lodi`] gives `dir`.
fn home_manifest(dir: &Path, text: &str) {
    let config = dir.join("home/.config/lodi");
    fs::create_dir_all(&config).unwrap();
    fs::write(config.join("home.toml"), text).unwrap();
}

/// The lines of an invocation's standard error that carry `W_DEPRECATED`.
fn deprecation_lines(out: &Output) -> Vec<String> {
    String::from_utf8_lossy(&out.stderr)
        .lines()
        .filter(|line| line.contains("W_DEPRECATED"))
        .map(str::to_string)
        .collect()
}

/// D5 (LD-324): a `home.toml` whose `[files]` table has three entries prints exactly one
/// `W_DEPRECATED` line, the documented one, first on standard error, for every run of
/// `lodi home plan`, `apply` and `status` — once per run, not once per entry, and never on
/// standard output — and each run exits as it did before the warning existed: 0, and 8 for the
/// apply a hand edit refuses (`E_DRIFT`).
#[test]
fn a_home_toml_using_files_warns_once_per_run() {
    let dir = scratch("files-deprecated");
    home_manifest(
        &dir,
        "[home]\nversion = \"1\"\n\n\
         [files.\".a\"]\ncontent = \"a\\n\"\n\n\
         [files.\".b\"]\ncontent = \"b\\n\"\n\n\
         [files.\".config/c\"]\ncontent = \"c\\n\"\n",
    );
    let check = |verb: &str, status: i32| {
        let out = lodi(&dir, &argv(&["home", verb]));
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(status),
            "lodi home {verb}: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("W_DEPRECATED"),
            "lodi home {verb} printed W_DEPRECATED on standard output"
        );
        assert_eq!(
            deprecation_lines(&out),
            vec![FILES_WARNING.to_string()],
            "lodi home {verb} over a [files] manifest: {stderr}"
        );
        assert_eq!(
            stderr.lines().next(),
            Some(FILES_WARNING),
            "lodi home {verb}: the warning is not the first line on standard error"
        );
    };
    for verb in ["plan", "apply", "status", "plan"] {
        check(verb, 0);
    }
    // A hand edit: plan and status report it at 0, and the apply refuses at 8, as it always did.
    fs::write(dir.join("home/.a"), "edited by hand\n").unwrap();
    check("plan", 0);
    check("apply", 8);
    check("status", 0);
    fs::remove_dir_all(&dir).unwrap();
}

/// The negative control of D5: a `home.toml` that declares its files in `[home.file]` and
/// `[home.xdg_config]` only prints no `W_DEPRECATED` on either stream, from any verb.
#[test]
fn a_home_toml_using_only_home_file_prints_no_deprecation() {
    let dir = scratch("home-file-only");
    home_manifest(
        &dir,
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".a\"]\ntext = \"a\\n\"\n\n\
         [home.file.\".b\"]\ntext = \"b\\n\"\n\n\
         [home.xdg_config.\"c\"]\ntext = \"c\\n\"\n",
    );
    for verb in ["plan", "apply", "status", "plan"] {
        let out = lodi(&dir, &argv(&["home", verb]));
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "lodi home {verb}: {stderr}");
        assert!(
            deprecation_lines(&out).is_empty(),
            "lodi home {verb}: {stderr}"
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("W_DEPRECATED"),
            "lodi home {verb} printed W_DEPRECATED on standard output"
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// D5 is about reading the manifest: help and usage errors never read it, so over a home whose
/// `home.toml` uses `[files]` they print no `W_DEPRECATED`, at their usual statuses.
#[test]
fn help_and_usage_print_no_deprecation_over_a_files_manifest() {
    let dir = scratch("files-help");
    home_manifest(
        &dir,
        "[home]\nversion = \"1\"\n\n[files.\".a\"]\ncontent = \"a\\n\"\n",
    );
    let cases: &[(&[&str], i32)] = &[
        (&["--help"], 0),
        (&["-h"], 0),
        (&["home"], 0),
        (&["home", "--help"], 0),
        (&["home", "plan", "--help"], 0),
        (&["home", "apply", "-h"], 0),
        (&["help", "home", "status"], 0),
        (&["home", "frobnicate"], 2),
        (&["home", "plan", "--frobnicate"], 2),
        (&["home", "status", "extra", "more"], 2),
    ];
    for (args, status) in cases {
        let out = lodi(&dir, &argv(args));
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(
            out.status.code(),
            Some(*status),
            "lodi {}: {stderr}",
            args.join(" ")
        );
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("W_DEPRECATED")
                && deprecation_lines(&out).is_empty(),
            "lodi {} printed W_DEPRECATED: {stderr}",
            args.join(" ")
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

// -------------------------------------------- 6. the first run says where to start (LD-380) ---

/// The opening of `lodi --help`: the three scopes and the command each starts with, then where
/// a command without a scope word acts. Lines 3 to 8 of it (1-based) are what a bare `lodi`
/// prints.
fn first_run_opening() -> String {
    format!(
        "lodi {} — environment and system manager for Ubuntu, Debian, Arch and Fedora

Three independent scopes. Start with the one you want. None needs another:
  this machine  sudo {lodi} host arm      once: allow lodi to manage this machine
                sudo {lodi} host import   copy its packages and changed /etc files
  your home     {lodi} home init          write a starting home.toml
  a project     {lodi} init               write ./lodi.toml here; then {lodi} lock
Commands without 'host' or 'home' act on the current directory.

Examples:
",
        env!("CARGO_PKG_VERSION"),
        lodi = "lodi"
    )
}

/// F1 of u-1: `lodi --help` opens with the three scopes, and its command inventory is unchanged
/// below them. The negative control: the opening is recognised as missing from a text that
/// lacks it.
#[test]
fn the_help_opens_with_the_three_scopes() {
    let dir = scratch("first-run-help");
    let out = lodi(&dir, &argv(&["--help"]));
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("--help is UTF-8");
    let opening = first_run_opening();
    assert!(
        text.starts_with(&opening),
        "`lodi --help` does not open with the three scopes:\n{}",
        text.lines().take(10).collect::<Vec<_>>().join("\n")
    );
    // Every scope line names a command the inventory below it lists.
    let inventory = help_commands();
    for command in [
        "host arm",
        "host import",
        "home init",
        "home import",
        "init",
    ] {
        assert!(inventory.contains(command), "{command}: {inventory:?}");
    }
    assert!(!"lodi 1.0.0 — environment and system manager\n\nUsage:\n".starts_with(&opening));
    fs::remove_dir_all(&dir).unwrap();
}

/// F1 of u-1: a bare `lodi` prints the scope lines of that opening — lines 3 to 8 — on standard
/// output and exits 0, as a bare `lodi home` or `lodi host` prints its part of the help (LD-360).
#[test]
fn a_bare_lodi_orients_at_exit_0() {
    let dir = scratch("first-run-bare");
    let out = lodi(&dir, &[]);
    let stdout = String::from_utf8(out.stdout).expect("UTF-8");
    let stderr = String::from_utf8(out.stderr).expect("UTF-8");
    assert_eq!(
        out.status.code(),
        Some(0),
        "stdout:\n{stdout}\nstderr:\n{stderr}"
    );
    let opening = first_run_opening();
    let scopes: Vec<&str> = opening.lines().skip(2).take(6).collect();
    assert_eq!(stdout, format!("{}\n", scopes.join("\n")));
    assert_eq!(stderr, "");
    // Nothing was written where it ran.
    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "home")
        .collect();
    assert!(entries.is_empty(), "a bare lodi wrote {entries:?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// F3 of u-1 (validator-repair-1): `docs/CLI.md` records the bare `lodi` change as LD-360's was,
/// and what it says is what the binary does. The compatibility promise carries the entry, and the
/// sentence of the Exits preamble about `lodi` with no command states exit 0 and the six scope
/// lines; a bare `lodi` then exits with the status that sentence states and prints lines 3 to 8
/// of `lodi --help`. The pre-LD-380 document (exit 2, `no command given`) and the pre-LD-380
/// binary each fail it.
#[test]
fn the_cli_document_says_what_a_bare_lodi_does() {
    let text = cli_md();
    let prose = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let from = prose
        .find("`lodi` with no command at all ")
        .expect("docs/CLI.md says nothing about `lodi` with no command");
    let sentence = &prose[from..from + prose[from..].find(". ").expect("a sentence")];
    let documented: i32 = sentence
        .rsplit_once("exits ")
        .and_then(|(_, n)| n.parse().ok())
        .unwrap_or_else(|| panic!("no exit status in: {sentence}"));
    assert_eq!(
        documented, 0,
        "docs/CLI.md does not say a bare `lodi` exits 0: {sentence}"
    );
    assert!(
        sentence.contains("lines 3 to 8") && !sentence.contains("no command given"),
        "{sentence}"
    );

    // The promise names the change in plain words: a user page cites no decision id (LD-464).
    assert!(
        prose.contains(
            "- **A bare `lodi` orients.** It prints the six scope lines of the help and exits 0"
        ),
        "docs/CLI.md's compatibility promise does not record the bare `lodi` change"
    );

    let dir = scratch("first-run-document");
    let out = lodi(&dir, &[]);
    assert_eq!(
        out.status.code(),
        Some(documented),
        "a bare `lodi` does not exit as docs/CLI.md says: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let help = lodi(&dir, &argv(&["--help"]));
    let help = String::from_utf8(help.stdout).expect("UTF-8");
    let scopes: Vec<&str> = help.lines().skip(2).take(6).collect();
    assert_eq!(
        String::from_utf8(out.stdout).expect("UTF-8"),
        format!("{}\n", scopes.join("\n"))
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// A `##`/`###` section of a Markdown text: from its heading to the next heading of the same
/// or a higher level.
fn markdown_section<'a>(text: &'a str, heading: &str) -> &'a str {
    let start = text
        .find(&format!("\n{heading}\n"))
        .unwrap_or_else(|| panic!("no {heading}"))
        + 1;
    let level = heading.split(' ').next().unwrap();
    let body = &text[start + heading.len()..];
    let end = body
        .lines()
        .scan(0usize, |at, line| {
            let here = *at;
            *at += line.len() + 1;
            Some((here, line))
        })
        .find(|(_, line)| {
            let hashes = line.split(' ').next().unwrap_or("");
            !hashes.is_empty() && hashes.chars().all(|c| c == '#') && hashes.len() <= level.len()
        })
        .map_or(body.len(), |(at, _)| at);
    &body[..end]
}

/// The fenced blocks of `text` with the info string `info`, each as its lines, indented or not.
fn fenced(text: &str, info: &str) -> Vec<Vec<String>> {
    let mut blocks = Vec::new();
    let mut open: Option<Vec<String>> = None;
    for line in text.lines() {
        let bare = line.trim_start();
        match open.as_mut() {
            None if bare == format!("```{info}") => open = Some(Vec::new()),
            Some(_) if bare == "```" => blocks.push(open.take().unwrap()),
            Some(block) => block.push(bare.to_string()),
            None => {}
        }
    }
    blocks
}

/// The directory of hosts the README's host commands use (W3, LD-379): the owner's request of
/// 2026-09-25 puts the host in a directory the reader owns, `~/lodi/<hostname>/`, like a flake.
const README_HOSTS: &str = "~/lodi";

/// The README's repository command `verb` on [`README_HOSTS`], exactly as a reader types it:
/// from 1.5.0 the README leads with `lodi import`, `plan` and `apply` (LD-416, LD-402).
fn readme_repo(verb: &str) -> String {
    // check-host-safety: refusal — README text compared, never run.
    format!("sudo lodi {verb} {README_HOSTS}")
}

/// The one line that makes [`README_HOSTS`] before the import writes into it. An import's
/// `SOURCE` must exist (`E_NO_MANIFEST` otherwise), and one that group or others can write is
/// `E_PATH_ESCAPE`, which a plain `mkdir` under a umask of 002 would make.
fn readme_hosts_mkdir() -> String {
    format!("mkdir -pm 755 {README_HOSTS}")
}

/// The README with runs of whitespace joined, so a sentence reads the same across line breaks.
fn prose(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The numbered steps of a README section: each runs from a line that starts with its number to
/// the next such line.
fn numbered_steps(section: &str) -> Vec<String> {
    let mut steps: Vec<String> = Vec::new();
    for line in section.lines() {
        let numbered = line
            .split_once(". ")
            .is_some_and(|(n, _)| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        match steps.last_mut() {
            Some(step) if !numbered => {
                step.push_str(line);
                step.push('\n');
            }
            _ if numbered => steps.push(format!("{line}\n")),
            _ => {}
        }
    }
    steps
}

/// F2 of u-1 (LD-380): the README's snapshot step makes the directory of hosts, imports into it
/// and stops at the plan; it never applies.
#[test]
fn the_readme_snapshot_step_stops_at_the_plan() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let host = markdown_section(&readme, "### 2. Snapshot the machine you are on");
    let blocks = fenced(host, "sh");
    let snapshot: Vec<&Vec<String>> = blocks
        .iter()
        .filter(|b| b.iter().any(|l| *l == readme_repo("import")))
        .collect();
    assert_eq!(snapshot.len(), 1, "{blocks:?}");
    let block = snapshot[0];
    assert_eq!(block.last(), Some(&readme_repo("plan")), "{block:?}");
    let made = block.iter().position(|l| *l == readme_hosts_mkdir());
    let imported = block.iter().position(|l| *l == readme_repo("import"));
    assert!(
        made.is_some() && made < imported,
        "no mkdir first: {block:?}"
    );
    assert!(
        !block.iter().any(|l| l.contains("lodi apply")),
        "the snapshot step applies: {block:?}"
    );
}

/// F2 of u-1: the apply is a numbered step of its own, with no import in it, and that step warns
/// that on Arch it runs `pacman -Syu` (the behaviour `tests/host_pacman.rs` proves).
#[test]
fn the_readme_apply_is_its_own_step_and_warns_of_the_arch_upgrade() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let host = markdown_section(&readme, "### 2. Snapshot the machine you are on");
    let steps = numbered_steps(host);
    let apply: Vec<&String> = steps
        .iter()
        .filter(|step| {
            fenced(step, "sh")
                .iter()
                .any(|b| *b == [readme_repo("apply")])
        })
        .collect();
    assert_eq!(apply.len(), 1, "no numbered apply step: {steps:#?}");
    let step = apply[0];
    assert!(
        !fenced(step, "sh")
            .iter()
            .flatten()
            .any(|l| l.contains("lodi import")),
        "{step}"
    );
    assert!(step.contains("pacman -Syu"), "no Arch warning:\n{step}");
}

/// F2 of u-1: a fresh machine is given the directory of hosts, never the root's own host and
/// never the arming marker; `tests/host_directory.rs` proves a copied marker does not arm.
#[test]
fn the_readme_fresh_machine_gets_the_host_directory_not_the_marker() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let host = prose(markdown_section(
        &readme,
        "### 2. Snapshot the machine you are on",
    ));
    // check-host-safety: refusal — README text compared, never run.
    assert!(!host.contains("copy `/etc/lodi/` across"), "{host}");
    assert!(
        host.contains(&format!("copy `{README_HOSTS}/` across")),
        "a fresh machine is not given the host directory:\n{host}"
    );
    // check-host-safety: refusal — README text compared, never run.
    let marker = "never copy `/etc/lodi/host-allowed`";
    assert!(
        host.to_lowercase().contains(marker),
        "a fresh machine is not told to leave host-allowed behind:\n{host}"
    );
}

/// F2 of u-1: the project walk-through uses no `--force`, and its home example declares a
/// `[home.file]` entry, never the deprecated `[files]`.
#[test]
fn the_readme_walk_through_uses_home_file_and_no_force() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let walk = fenced(markdown_section(&readme, "### 3. A first project"), "sh");
    assert!(!walk.is_empty());
    for line in walk.iter().flatten() {
        assert!(!line.contains("--force"), "uses --force: {line}");
        assert!(!line.contains("[files."), "uses [files]: {line}");
    }
    assert!(
        walk.iter().flatten().any(|l| l.contains("[home.file.")),
        "the home example declares no [home.file] entry"
    );
}

/// F2 of u-1: the name paragraph runs nothing and names the files a machine is rebuilt from.
#[test]
fn the_readme_name_paragraph_names_the_manifests_and_runs_nothing() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let name = markdown_section(&readme, "### About the name");
    let pun = name.trim_start().split("\n\n").next().unwrap();
    assert!(!pun.contains("stuck in a Lodi"), "{pun}");
    // check-host-safety: refusal — README text compared, never run.
    assert!(!pun.contains("lodi host apply"), "{pun}");
    for manifest in ["host.toml", "home.toml", "lodi.lock"] {
        assert!(pun.contains(manifest), "no {manifest}:\n{pun}");
    }
}

/// The owner's request of 2026-09-25 (LD-440): the README opens, after its title and a one-line
/// description, with `## Three commands to get up and running`.
#[test]
fn the_readme_opens_with_the_three_commands() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let heading = "## Three commands to get up and running";
    assert_eq!(
        readme.lines().find(|l| l.starts_with("## ")),
        Some(heading),
        "the README's first section is not the three commands"
    );
    let before = &readme[..readme.find(&format!("\n{heading}\n")).expect("the heading")];
    let paragraphs: Vec<&str> = before
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    assert!(
        paragraphs.len() == 2
            && paragraphs[0].starts_with("# ")
            && paragraphs[1].lines().count() == 1,
        "more than a title and a one-line description: {paragraphs:?}"
    );
}

/// LD-440: that section's one `sh` block is exactly arm, make-and-import, apply, each line with a
/// short trailing comment.
#[test]
fn the_three_commands_are_arm_import_apply_with_short_comments() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let section = markdown_section(&readme, "## Three commands to get up and running");
    let blocks = fenced(section, "sh");
    assert_eq!(blocks.len(), 1, "{blocks:?}");
    let commands: Vec<&str> = blocks[0]
        .iter()
        .map(|line| {
            let (command, comment) = line
                .split_once(" # ")
                .unwrap_or_else(|| panic!("no trailing comment: {line}"));
            let comment = comment.trim();
            assert!(
                !comment.is_empty() && comment.len() <= 64,
                "not a short comment: {line}"
            );
            command.trim()
        })
        .collect();
    let import = format!("{} && {}", readme_hosts_mkdir(), readme_repo("import"));
    let apply = readme_repo("apply");
    // check-host-safety: refusal — README text compared, never run.
    let expected = ["sudo lodi host arm", import.as_str(), apply.as_str()];
    assert_eq!(commands, expected, "{section}");
}

/// LD-440: the three commands name the plan that writes nothing and the Arch full upgrade.
#[test]
fn the_three_commands_name_the_plan_and_the_arch_upgrade() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let section = prose(markdown_section(
        &readme,
        "## Three commands to get up and running",
    ));
    assert!(
        section.contains(&format!("`{}`", readme_repo("plan"))),
        "no plan:\n{section}"
    );
    assert!(
        section.contains("pacman -Syu"),
        "no Arch upgrade:\n{section}"
    );
}

// ------------------------------------ 7. the init forms: the home scope starts at home init ---

/// Every file below `dir`, relative to it, with its bytes.
fn tree(dir: &Path) -> BTreeMap<PathBuf, String> {
    fn walk(base: &Path, at: &Path, out: &mut BTreeMap<PathBuf, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(base, &path, out);
            } else {
                let bytes = String::from_utf8_lossy(&fs::read(&path).unwrap()).into_owned();
                out.insert(path.strip_prefix(base).unwrap().to_path_buf(), bytes);
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

/// One run's status, standard output and standard error, with the scratch directory's own path
/// replaced so that two scratch directories compare equal.
fn observed(dir: &Path, out: &Output) -> (Option<i32>, String, String) {
    let at = dir.to_string_lossy().into_owned();
    (
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).replace(&at, "<dir>"),
        String::from_utf8_lossy(&out.stderr).replace(&at, "<dir>"),
    )
}

/// I1 of i-1 (LD-382): `lodi home init` is an exact alias of `lodi home import`. Each sequence of
/// invocations runs once with `import` and once with `init`, each in a fresh scratch home, and
/// every step must give the same status, the same standard output and the same standard error,
/// and leave the same files with the same bytes. The sequences cover every flag, the default
/// destination, `E_EXISTS` and its `--force`, `E_PATH_ESCAPE`, and each usage error; a usage
/// error names the words that were typed, so there `init` stands where `import` stood and
/// nothing else differs. The negative control: `home plan` is recognised as not the same.
#[test]
fn home_init_is_an_exact_alias_of_home_import() {
    let sequences: &[&[&[&str]]] = &[
        &[&[], &[], &["--force"]],
        &[&["--stdout"]],
        &[
            &["--out", "bundle"],
            &["--out", "bundle"],
            &["--out", "bundle", "--force"],
        ],
        &[&["--force"]],
        &[&["--out", "/"]],
        &[&["--frobnicate"]],
        &[&["--out"]],
        &[&["--force", "--force"]],
        &[&["--stdout", "--stdout"]],
        &[&["--stdout", "--out", "bundle"]],
        &[&["--stdout", "--force"]],
        &[&["extra"]],
    ];
    let mut statuses = BTreeSet::new();
    for (n, sequence) in sequences.iter().enumerate() {
        let mut runs = Vec::new();
        for verb in ["import", "init"] {
            let dir = scratch(&format!("init-alias-{n}-{verb}"));
            let mut steps = Vec::new();
            for flags in *sequence {
                let words = [&["home", verb][..], flags].concat();
                let out = lodi(&dir, &argv(&words));
                let (status, stdout, stderr) = observed(&dir, &out);
                statuses.insert(status);
                steps.push((status, stdout, stderr.replace("home init", "home import")));
            }
            runs.push((steps, tree(&dir)));
            fs::remove_dir_all(&dir).unwrap();
        }
        assert_eq!(
            runs[0], runs[1],
            "`lodi home init` is not `lodi home import` for {sequence:?}"
        );
    }
    // The sequences reached success, the usage error and the manifest's refusals.
    assert_eq!(statuses, BTreeSet::from([Some(0), Some(2), Some(3)]));

    // The negative control: the comparison sees a different command.
    let a = scratch("init-alias-control-a");
    let b = scratch("init-alias-control-b");
    let import = observed(&a, &lodi(&a, &argv(&["home", "import", "--stdout"])));
    let plan = observed(&b, &lodi(&b, &argv(&["home", "plan"])));
    assert_ne!(import, plan);
    fs::remove_dir_all(&a).unwrap();
    fs::remove_dir_all(&b).unwrap();
}

/// I1 of i-1 (LD-382): the first guesses a newcomer types are still usage errors (exit 2, nothing
/// written), and each says which command they meant. Each guess is its own test, so each pointer
/// is red or green on its own (validator-repair-1); `refused_with_a_pointer` is the one check they
/// share. The negative control: a command nobody would guess gets no pointer.
fn refused_with_a_pointer(words: &[&str], named: &[&str]) {
    let dir = scratch(&format!("guess-{}", words.join("-")));
    let out = lodi(&dir, &argv(words));
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(
        out.status.code(),
        Some(2),
        "`lodi {}`: {stderr}",
        words.join(" ")
    );
    assert!(out.stdout.is_empty(), "`lodi {}`", words.join(" "));
    let hint = stderr
        .lines()
        .find(|l| l.starts_with("   = hint: "))
        .unwrap_or_else(|| panic!("`lodi {}` names no command:\n{stderr}", words.join(" ")));
    for command in named {
        assert!(
            hint.contains(&format!("`{command}`")),
            "`lodi {}` does not name `{command}`:\n{stderr}",
            words.join(" ")
        );
    }
    assert!(tree(&dir.join("home")).is_empty() && !dir.join("lodi.toml").exists());
    assert!(!dir.join("store").exists());
    fs::remove_dir_all(&dir).unwrap();
}

// check-host-safety: refusal — expected text of a refused guess, never run.
const GUESS_ARM: &str = "sudo lodi host arm";
// check-host-safety: refusal — expected text of a refused guess, never run.
const GUESS_IMPORT: &str = "sudo lodi host import";

/// `lodi import` is a command since LD-416 (DONE D1, D2), no longer a guess: with no repository
/// it is the LD-447 refusal, whose hint names the import that starts one, and nothing is written.
#[test]
fn lodi_import_is_a_command_that_names_where_to_start() {
    let dir = scratch("guess-import");
    // check-host-safety: refusal — the guard refuses it before anything is read.
    let out = lodi_with(&dir, &argv(&["import"]), &[("LODI_HOST_REQUIRE_ROOT", "1")]);
    assert_eq!(out.status.code(), Some(9), "{out:?}");
    let out = lodi_with(&dir, &argv(&["import", "--root", "unarmed"]), &[]);
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    // check-host-safety: refusal — the hint's text, never run.
    assert!(stderr.contains("lodi import ~/lodi"), "{stderr}");
    assert!(tree(&dir.join("home")).is_empty() && !dir.join("lodi.lock").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lodi_setup_points_to_every_scope_s_first_command() {
    refused_with_a_pointer(
        &["setup"],
        &[GUESS_ARM, GUESS_IMPORT, "lodi home init", "lodi init"],
    );
}

#[test]
fn lodi_init_host_points_to_host_arm_and_host_import() {
    refused_with_a_pointer(&["init", "host"], &[GUESS_ARM, GUESS_IMPORT]);
}

#[test]
fn lodi_init_home_points_to_home_init() {
    refused_with_a_pointer(&["init", "home"], &["lodi home init"]);
    refused_with_a_pointer(&["init", "--force", "home"], &["lodi home init"]);
}

#[test]
fn a_command_nobody_would_guess_gets_no_pointer() {
    let dir = scratch("guess-control");
    let out = lodi(&dir, &argv(&["frobnicate"]));
    assert_eq!(out.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&out.stderr).contains("= hint:"));
    fs::remove_dir_all(&dir).unwrap();
}

/// This process's effective uid, and another one.
fn uids() -> (u32, u32) {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    (euid, euid.wrapping_add(1))
}

/// I1 of i-1 (LD-382): a home verb run under `sudo` — `SUDO_UID` names a user other than the one
/// the process runs as — prints one `W_HOME_SUDO` line on standard error first, and everything
/// else is as it would have been: the same status, the same standard output, the same standard
/// error after that line. The negative controls: no line without `SUDO_UID`, with the process's
/// own uid in it, with a value that is no uid, and for a command outside the home scope.
#[test]
fn a_home_verb_under_sudo_warns_and_changes_nothing_else() {
    let (euid, other) = uids();
    let other = other.to_string();
    let own = euid.to_string();
    let verbs: &[&[&str]] = &[
        &["home", "plan"],
        &["home", "status"],
        &["home", "apply", "--locked"],
        &["home", "import", "--stdout"],
        &["home", "init", "--stdout"],
        &["home", "init"],
    ];
    for words in verbs {
        let plain_dir = scratch("sudo-plain");
        let plain = observed(&plain_dir, &lodi(&plain_dir, &argv(words)));
        let plain_tree = tree(&plain_dir.join("home"));
        let dir = scratch("sudo-warned");
        let warned = observed(
            &dir,
            &lodi_with(&dir, &argv(words), &[("SUDO_UID", &other)]),
        );
        let (first, rest) = warned.2.split_once('\n').expect("a line on standard error");
        assert!(
            first.starts_with("lodi: warning W_HOME_SUDO: ")
                && first.contains(&format!("uid {other}"))
                && first.contains(&format!("uid {euid}")),
            "`lodi {}` under sudo:\n{}",
            words.join(" "),
            warned.2
        );
        assert_eq!(
            (warned.0, &warned.1, rest),
            (plain.0, &plain.1, plain.2.as_str()),
            "`lodi {}`: the warning changed something else",
            words.join(" ")
        );
        assert_eq!(tree(&dir.join("home")), plain_tree);
        fs::remove_dir_all(&dir).unwrap();

        for quiet in [own.as_str(), "", "root", "-1"] {
            let dir = scratch("sudo-quiet");
            let out = observed(&dir, &lodi_with(&dir, &argv(words), &[("SUDO_UID", quiet)]));
            assert_eq!(out, plain, "SUDO_UID={quiet:?} `lodi {}`", words.join(" "));
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::remove_dir_all(&plain_dir).unwrap();
    }
    for words in [
        &["lock", "--check"][..],
        &["init", "--force"],
        &["frobnicate"],
    ] {
        let dir = scratch("sudo-other-scope");
        let out = lodi_with(&dir, &argv(words), &[("SUDO_UID", &other)]);
        assert!(
            !String::from_utf8_lossy(&out.stderr).contains("W_HOME_SUDO"),
            "`lodi {}` is not a home verb",
            words.join(" ")
        );
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// I2 of i-1 (LD-382): the documents start the home scope at `lodi home init` and name
/// `lodi home import` as its 1.0 name, with no deprecation. `docs/CLI.md` documents both, and the
/// help's opening says `home init` (the tests above hold the binary to both).
#[test]
fn the_documents_start_the_home_scope_at_home_init() {
    for file in [
        "docs/CLI.md",
        "docs/scopes/home.md",
        "README.md",
        "docs/GUIDE.md",
    ] {
        let text = fs::read_to_string(repo().join(file)).unwrap();
        let prose = text.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            prose.contains("`lodi home init`"),
            "{file} names no `lodi home init`"
        );
        assert!(
            prose.contains("`lodi home import` is its 1.0 name"),
            "{file} does not name `lodi home import` as the 1.0 name of `lodi home init`"
        );
    }
    let cli = cli_md();
    let commands = documented_commands(&cli);
    assert!(commands.contains("home init") && commands.contains("home import"));
    let flags = documented_flags(&cli);
    assert_eq!(flags["home init"], flags["home import"]);
    // No command is deprecated: `home import` stays for all of 1.x with no `W_DEPRECATED`.
    assert_eq!(deprecation(DEPRECATIONS, "home import"), None);
    assert!(DEPRECATIONS.iter().all(|d| d.manifest().is_some()));
}

/// validator-repair-1 of i-1: `docs/scopes/home.md` opens with the home scope's command
/// inventory, and that inventory names every home verb `lodi --help` names — `init` and `import`, its 1.0
/// name, included. A count it states is the count of those verbs.
#[test]
fn the_home_scope_s_verb_inventory_is_the_help_s() {
    let verbs: Vec<String> = help_commands()
        .iter()
        .filter_map(|c| c.strip_prefix("home ").map(str::to_string))
        .collect();
    assert!(verbs.iter().any(|v| v == "init"), "--help names {verbs:?}");
    let text = fs::read_to_string(repo().join("docs/scopes/home.md")).unwrap();
    let intro = &text[..text
        .find("\n## ")
        .expect("docs/scopes/home.md has a section")];
    let prose = intro.split_whitespace().collect::<Vec<_>>().join(" ");
    let inventory = prose
        .split(". ")
        .find(|s| s.contains(" commands"))
        .unwrap_or_else(|| panic!("docs/scopes/home.md's opening has no command inventory"));
    let words: BTreeSet<&str> = inventory
        .split(|c: char| !c.is_ascii_alphanumeric())
        .collect();
    for verb in &verbs {
        assert!(
            words.contains(verb.as_str()),
            "docs/scopes/home.md's verb inventory names no `{verb}`: {inventory}"
        );
    }
    let numbers = ["one", "two", "three", "four", "five", "six", "seven"];
    for (n, word) in numbers.iter().enumerate() {
        if inventory.contains(&format!(" {word} commands")) {
            assert_eq!(
                n + 1,
                verbs.len(),
                "docs/scopes/home.md counts {word} commands; --help names {verbs:?}"
            );
        }
    }
}

/// The GUIDE's four starting home commands must include `init`, not count its 1.0 alias `import`
/// in place of `init`. Check the inventory sentence itself, not a mention elsewhere in the GUIDE.
#[test]
fn the_guide_home_verb_inventory_starts_with_init() {
    let text = fs::read_to_string(repo().join("docs/GUIDE.md")).unwrap();
    let home = markdown_section(&text, "## The home scope");
    let opening = home
        .trim_start()
        .split("\n\n")
        .next()
        .expect("home scope opening");
    let prose = opening.split_whitespace().collect::<Vec<_>>().join(" ");
    let inventory = prose
        .split_once("four commands:")
        .expect("GUIDE names four starting home commands")
        .1
        .split(". ")
        .next()
        .expect("inventory sentence");
    let named: Vec<&str> = inventory
        .split('`')
        .filter(|span| span.starts_with("lodi home "))
        .collect();
    assert_eq!(
        named,
        [
            "lodi home init",
            "lodi home plan",
            "lodi home apply",
            "lodi home status",
        ],
        "GUIDE's four starting home verbs must name init rather than its import alias"
    );
    assert!(
        prose.contains("`lodi home import` is its 1.0 name"),
        "GUIDE must still name the import alias separately"
    );
}

/// The commands on `main` that no release has yet, as `lodi …` (validator-repair-1 of i-1). The
/// README's quick start is followed verbatim with the release it links to (`AGENTS.md` §9.4), so
/// none of these may be in its shell blocks. Empty this list in the release that ships them:
/// 1.2.0 shipped `lodi home init` (LD-382, LD-388), 1.5.0 the repository verbs (LD-416, LD-402).
const UNRELEASED: &[&str] = &[];

/// validator-repair-1 of i-1: the README's quick start installs the release `Cargo.toml` names,
/// so every shell line in it works with that release: no command in [`UNRELEASED`]. Its prose
/// says that `lodi home init` and `lodi home import` are the same command.
#[test]
fn the_readme_quick_start_runs_with_the_release_it_installs() {
    let text = fs::read_to_string(repo().join("README.md")).unwrap();
    let quick = section(&text, "## Quick start");
    let manifest = fs::read_to_string(repo().join("Cargo.toml")).unwrap();
    let version = manifest
        .lines()
        .find_map(|l| l.strip_prefix("version = \""))
        .and_then(|v| v.strip_suffix('"'))
        .expect("Cargo.toml has a version");
    assert!(
        quick.contains(&format!("releases/tag/v{version}")),
        "the quick start does not install {version}"
    );
    let mut in_shell = false;
    let mut lines = 0;
    for line in quick.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("```") {
            in_shell = trimmed == "```sh";
            continue;
        }
        if !in_shell {
            continue;
        }
        lines += 1;
        let command = trimmed.split('#').next().unwrap().trim();
        for unreleased in UNRELEASED {
            assert!(
                !command.contains(unreleased),
                "the {version} quick start runs `{command}`, which {version} does not have"
            );
        }
    }
    assert!(lines > 0, "the quick start has no shell block");
    let prose = quick.split_whitespace().collect::<Vec<_>>().join(" ");
    assert!(
        prose.contains("`lodi home import`") && prose.contains("`lodi home init`"),
        "the quick start does not name both `lodi home import` and `lodi home init`"
    );
    assert!(
        prose.contains("`lodi home init` is the same command"),
        "the quick start does not say that `lodi home init` is the same command"
    );
}
