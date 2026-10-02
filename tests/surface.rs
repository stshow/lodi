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
//! 6. the first run (LD-380): `lodi --help` opens with where to start (`import`, `switch`,
//!    `init`), a bare `lodi` prints that opening at exit 0 as `docs/CLI.md` says it does, and
//!    `README.md`'s quick start neither upgrades nor arms a machine by copy;
//! 7. every option a command accepts is in its help and its `docs/CLI.md` flag table (#700).
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
        nonsense: &["frobnicate", "init", "-h"],
    },
    Case {
        command: "help",
        well_formed: &["help", "switch"],
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
        command: "develop",
        well_formed: &["develop", "--trust", "--no-nest", "--", "true"],
        nonsense: &["develop", "--trust", "--frobnicate", "--", "true"],
    },
    Case {
        command: "run",
        well_formed: &["run", "--no-nest", "build"],
        nonsense: &["run", "--frobnicate", "build"],
    },
    Case {
        command: "run",
        well_formed: &["run", "--trust", "build"],
        nonsense: &["run", "--trust", "--frobnicate", "build"],
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
];

/// The config cases, which need a scratch `--root` and so cannot be a `const`.
/// The root is a directory this test made inside `CARGO_TARGET_TMPDIR`, and each invocation
/// stops before anything below it is opened.
fn host_cases(root: &Path) -> Vec<(&'static str, Vec<String>, Vec<String>)> {
    let root = root.to_string_lossy().into_owned();
    vec![
        (
            "import",
            // check-host-safety: refusal — the same scratch root, which is not armed.
            // A URL, refused before anything is read or written (#693).
            vec![
                "import".into(),
                "github:owner/hosts".into(),
                "--home".into(),
                "--yes".into(),
                "--dry-run".into(),
                "-v".into(),
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
            "switch",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "switch".into(),
                "missing-config".into(),
                "--ask".into(),
                "--home".into(),
                "--update".into(),
                "--overwrite-drift".into(),
                "--resolved".into(),
                "1".into(),
                "-v".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "--no-update".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "switch",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "switch".into(),
                "missing-config".into(),
                "--dry-run".into(),
                "--host".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "--ask".into(),
                "--dry-run".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        // A URL config's selectors (LD-401): the scratch root has no hostname, so each stops
        // before anything is fetched; `--ref` with `--rev` and `--rev` with `--refresh` are
        // the usage errors.
        (
            "switch",
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "github:owner/hosts".into(),
                "--ref".into(),
                "main".into(),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "github:owner/hosts".into(),
                "--ref".into(),
                "main".into(),
                "--rev".into(),
                "0".repeat(40),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "switch",
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "github:owner/hosts".into(),
                "--rev".into(),
                "0".repeat(40),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "switch".into(),
                "github:owner/hosts".into(),
                "--rev".into(),
                "0".repeat(40),
                "--refresh".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "update",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "update".into(),
                "missing-config".into(),
                "--host".into(),
                "box".into(),
                "-v".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "update".into(),
                "--yes".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "pin",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "pin".into(),
                "bc".into(),
                "--to".into(),
                "2026-09-01".into(),
                "missing-config".into(),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec!["pin".into(), "--root".into(), root.clone()],
        ),
        (
            "pin",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "pin".into(),
                "--all".into(),
                "./missing-config".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "pin".into(),
                "--all".into(),
                "bc".into(),
                "--root".into(),
                root.clone(),
            ],
        ),
        (
            "unpin",
            // check-host-safety: refusal — the same scratch root; the typed config is missing.
            vec![
                "unpin".into(),
                "--all".into(),
                "./missing-config".into(),
                "--host".into(),
                "box".into(),
                "--root".into(),
                root.clone(),
            ],
            // check-host-safety: refusal — the same scratch root.
            vec![
                "unpin".into(),
                "bc".into(),
                "--to".into(),
                "1".into(),
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

/// A flag as a help text or a table writes it: one or two dashes, then lowercase words.
fn is_flag(word: &str) -> bool {
    let body = word.trim_start_matches('-');
    (1..=2).contains(&(word.len() - body.len()))
        && body.starts_with(|c: char| c.is_ascii_lowercase())
        && body.chars().all(|c| c.is_ascii_lowercase() || c == '-')
}

/// Whether `lodi COMMAND FLAG` gets past `FLAG`: with a value after it, then without, a
/// nonsense flag ends the line, and the binary either refuses that nonsense flag (so it read
/// `FLAG`) or ran without a usage error. Nothing here reaches a package manager: every probe
/// is a usage error or a refusal in a scratch folder.
fn accepts(dir: &Path, command: &str, flag: &str) -> bool {
    let past = format!("'{command} --frobnicate'");
    [
        vec![command, flag, "7", "--frobnicate"],
        vec![command, flag, "--frobnicate"],
    ]
    .iter()
    .enumerate()
    .any(|(n, words)| {
        let here = dir.join(format!("{command}{flag}{n}"));
        fs::create_dir_all(&here).unwrap();
        let out = lodi(&here, &argv(words));
        out.status.code() != Some(2) || String::from_utf8_lossy(&out.stderr).contains(&past)
    })
}

/// Every option a command's parser accepts is in its help's Options block and in its
/// `docs/CLI.md` flag table, and nothing else is (#700). What a command accepts is asked of the
/// binary, flag by flag, from every flag any parser names. The negative control: `--to` is
/// `pin`'s and not `unpin`'s.
#[test]
fn every_option_a_command_accepts_is_in_its_help_and_its_reference_entry() {
    let dir = scratch("options");
    let candidates: Vec<String> = parser_surface()
        .into_iter()
        .filter(|f| is_flag(f) && !["--help", "-h", "--version"].contains(&f.as_str()))
        .collect();
    let documented = documented_flags(&cli_md());
    for command in help_commands()
        .iter()
        .filter(|c| c.as_str() != "help" && !c.starts_with('-'))
    {
        let accepted: BTreeSet<String> = candidates
            .iter()
            .filter(|flag| accepts(&dir, command, flag))
            .cloned()
            .collect();
        let (status, part) = help_part(&["help", command]);
        assert_eq!(status, Some(0), "{command}");
        let options: BTreeSet<String> = help_block(&part, "Options:")
            .iter()
            .filter_map(|l| l.split_whitespace().next())
            .flat_map(|w| w.split(','))
            .filter(|w| is_flag(w))
            .map(str::to_string)
            .collect();
        assert_eq!(
            options, accepted,
            "lodi help {command}'s options are not the flags it accepts:\n{part}"
        );
        let table: BTreeSet<String> = documented
            .get(command.as_str())
            .unwrap_or_else(|| panic!("docs/CLI.md has no entry for lodi {command}"))
            .iter()
            .filter(|f| *f != "--")
            .cloned()
            .collect();
        assert_eq!(
            table, accepted,
            "docs/CLI.md's flag table for lodi {command} is not the flags it accepts"
        );
    }
    assert!(accepts(&dir, "pin", "--to") && !accepts(&dir, "unpin", "--to"));
    fs::remove_dir_all(&dir).unwrap();
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
        documented.len() >= 14 && documented.contains("init") && documented.contains("switch"),
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
/// global flags, and each scope of two or more of those commands. A scope of one command prints
/// that command's usage alone, so its part is about it.
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
        subjects.contains("help") && subjects.contains("init") && subjects.contains("switch"),
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
    // Negative controls: an undocumented command has no help, a 1.x name is refused (#699),
    // and a part about `switch` is not mistaken for one about `init`.
    assert_eq!(help_part(&["help", "frobnicate"]).0, Some(2));
    assert_eq!(help_part(&["frobnicate", "--help"]).0, Some(2));
    assert_eq!(help_part(&["home", "--help"]).0, Some(2));
    assert_eq!(part_subject(&help_part(&["switch", "-h"]).1), "switch");
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
    let first = first_commands(&whole);
    assert_eq!(
        first.len(),
        3,
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

/// (d): the table has exactly two homes and they hold the same rows. Every row of
/// `docs/CLI.md`'s Deprecations table is a row of `src/surface.rs`'s `DEPRECATIONS`, cell for
/// field, and the other way round; from 2.0 both are empty.
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

/// `lodi switch --home`, a preview with `--dry-run`, on a machine of its own at `dir` (#699).
fn switch_home(dir: &Path, words: &[&str]) -> Output {
    support::machine_at(dir);
    let mut args = argv(words);
    // check-host-safety: refusal — every run names its scratch machine as --root.
    args.extend(["--root".to_string(), dir.display().to_string()]);
    lodi_with(dir, &args, &[("LODI_HOST_REQUIRE_ROOT", "1")])
}

/// The home preview and the home switch (#699, LD-525).
const PREVIEW: &[&str] = &["switch", "--home", "--dry-run"];
const SWITCH: &[&str] = &["switch", "--home"];

/// The 1.x `[files]` table of `home.toml` (deprecated since 1.2 for removal in 2.0) is refused
/// as an unknown table: `lodi switch --home` and its `--dry-run` stop at 3 with
/// `E_UNKNOWN_BLOCK`, naming the manifest file and the table, print no `W_DEPRECATED`, and write
/// nothing into the home (#710).
#[test]
fn a_home_toml_using_files_is_refused_naming_the_file_and_the_table() {
    let dir = scratch("files-refused");
    home_manifest(
        &dir,
        "[home]\nversion = \"1\"\n\n[files.\".a\"]\ncontent = \"a\\n\"\n",
    );
    let manifest = dir.join("home/.config/lodi/home.toml");
    for words in [PREVIEW, SWITCH] {
        let verb = words.join(" ");
        let out = switch_home(&dir, words);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(3), "lodi {verb}: {stderr}");
        let line = stderr
            .lines()
            .find(|line| line.contains("E_UNKNOWN_BLOCK"))
            .unwrap_or_else(|| panic!("lodi {verb} did not refuse [files]: {stderr}"));
        assert!(stderr.contains(manifest.to_str().unwrap()), "{stderr}");
        assert!(line.contains("`files`"), "{line}");
        assert!(deprecation_lines(&out).is_empty(), "lodi {verb}: {stderr}");
        assert!(!dir.join("home/.a").exists(), "lodi {verb} wrote the file");
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// The negative control of D5: a `home.toml` that declares its files in `[home.file]` and
/// `[home.xdg_config]` only prints no `W_DEPRECATED` on either stream, from the preview or the
/// switch.
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
    for words in [PREVIEW, SWITCH, PREVIEW] {
        let verb = words.join(" ");
        let out = switch_home(&dir, words);
        let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
        assert_eq!(out.status.code(), Some(0), "lodi {verb}: {stderr}");
        assert!(deprecation_lines(&out).is_empty(), "lodi {verb}: {stderr}");
        assert!(
            !String::from_utf8_lossy(&out.stdout).contains("W_DEPRECATED"),
            "lodi {verb} printed W_DEPRECATED on standard output"
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
        (&["switch", "--help"], 0),
        (&["switch", "--home", "-h"], 0),
        (&["help", "switch"], 0),
        (&["switch", "--frobnicate"], 2),
        (&["home"], 2),
        (&["home", "plan", "--help"], 2),
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

// ------------------------------------ 6. the first run says where to start (LD-380, #700) ---

/// The opening of `lodi --help`: the lines after the banner and its blank line, up to the next
/// blank line. A bare `lodi` prints them.
fn opening(help: &str) -> Vec<&str> {
    help.lines().skip(2).take_while(|l| !l.is_empty()).collect()
}

/// The commands the opening of `lodi --help` starts you with, as typed.
fn first_commands(help: &str) -> Vec<String> {
    opening(help)
        .iter()
        .filter_map(|l| {
            l.split("  ")
                .map(str::trim)
                .find(|c| c.starts_with("lodi "))
        })
        .map(|c| example_command(c).to_string())
        .collect()
}

/// `lodi --help` opens with where to start: `lodi import`, `lodi switch` and `lodi init`, each a
/// command the inventory below lists. The negative control: another opening is not mistaken for it.
#[test]
fn the_help_opens_with_where_to_start() {
    let dir = scratch("first-run-help");
    let out = lodi(&dir, &argv(&["--help"]));
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).expect("--help is UTF-8");
    assert_eq!(
        first_commands(&text),
        ["lodi import", "lodi switch", "lodi init"],
        "{text}"
    );
    let inventory = help_commands();
    for command in ["import", "switch", "init"] {
        assert!(inventory.contains(command), "{command}: {inventory:?}");
    }
    let old = "lodi 1.0.0\n\nStart:\n  lodi frobnicate  a\n  lodi init  b\n";
    assert_eq!(first_commands(old), ["lodi frobnicate", "lodi init"]);
    fs::remove_dir_all(&dir).unwrap();
}

/// A bare `lodi` prints the opening of `lodi --help` on standard output and exits 0 (LD-360),
/// and writes nothing where it ran.
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
    let help = lodi(&dir, &argv(&["--help"]));
    let help = String::from_utf8(help.stdout).expect("UTF-8");
    assert_eq!(stdout, format!("{}\n", opening(&help).join("\n")));
    assert_eq!(stderr, "");
    let entries: Vec<_> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n != "home")
        .collect();
    assert!(entries.is_empty(), "a bare lodi wrote {entries:?}");
    fs::remove_dir_all(&dir).unwrap();
}

/// `docs/CLI.md` says what a bare `lodi` does, and the binary does it: its sentence about `lodi`
/// with no command names the lines of `lodi --help` it prints and the exit status.
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
    let numbers: Vec<usize> = sentence
        .match_indices("lines ")
        .map(|(at, _)| {
            sentence[at + 6..]
                .split_whitespace()
                .take(3)
                .filter_map(|w| w.parse().ok())
                .collect::<Vec<usize>>()
        })
        .find(|numbers| numbers.len() == 2)
        .unwrap_or_default();
    assert_eq!(numbers.len(), 2, "no `lines A to B` in: {sentence}");

    let dir = scratch("first-run-document");
    let out = lodi(&dir, &[]);
    assert_eq!(out.status.code(), Some(documented), "{sentence}");
    let help = lodi(&dir, &argv(&["--help"]));
    let help = String::from_utf8(help.stdout).expect("UTF-8");
    let lines: Vec<&str> = help.lines().collect();
    assert_eq!(
        String::from_utf8(out.stdout).expect("UTF-8"),
        format!("{}\n", lines[numbers[0] - 1..numbers[1]].join("\n")),
        "{sentence}"
    );
    assert_eq!(numbers[1] - numbers[0] + 1, opening(&help).len());
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

/// The README with runs of whitespace joined, so a sentence reads the same across line breaks.
fn prose(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// #711: the README opens with its title, one short description and the command list, and its
/// first section is the quick start.
#[test]
fn the_readme_opens_with_a_description_then_the_quick_start() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    assert_eq!(
        readme.lines().find(|l| l.starts_with("## ")),
        Some("## Quick start"),
        "the README's first section is not the quick start"
    );
    let before = &readme[..readme.find("\n## Quick start\n").unwrap()];
    let paragraphs: Vec<&str> = before
        .split("\n\n")
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .collect();
    assert!(
        paragraphs.len() == 3
            && paragraphs[0].starts_with("# ")
            && paragraphs[1].lines().count() <= 2,
        "more than a title, a short description and the command list: {paragraphs:?}"
    );
}

/// #711: the import is a step of its own with `lodi import` alone, and the switch step previews
/// before it switches, with the Arch full upgrade named before its commands.
#[test]
fn the_readme_imports_then_previews_then_switches() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let import = markdown_section(&readme, "### 2. Import this machine");
    assert_eq!(fenced(import, "sh"), [["lodi import"]], "{import}");
    let switch = markdown_section(&readme, "### 3. Switch");
    assert_eq!(
        fenced(switch, "sh"),
        [["lodi switch --dry-run", "lodi switch"]],
        "{switch}"
    );
    let warned = switch
        .find("pacman -Syu")
        .expect("no Arch full-upgrade warning");
    assert!(
        warned < switch.find("```sh").unwrap(),
        "the warning follows the commands"
    );
}

/// The project walk-through uses no `--force`, and its home example declares a `[home.file]`
/// entry, never the `[files]` that 2.0 refuses.
#[test]
fn the_readme_walk_through_uses_home_file_and_no_force() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let walk = fenced(markdown_section(&readme, "### 4. A first project"), "sh");
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

/// The name's pun runs nothing and names the files a machine is rebuilt from.
#[test]
fn the_readme_name_paragraph_names_the_manifests_and_runs_nothing() {
    let readme = fs::read_to_string(repo().join("README.md")).unwrap();
    let about = prose(markdown_section(&readme, "## About Lodi"));
    let pun = &about[about.find("pun").expect("no pun on the name")..];
    assert!(!pun.contains("stuck in a Lodi"), "{pun}");
    assert!(!pun.contains("lodi switch"), "{pun}");
    for manifest in ["host.toml", "home.toml", "lodi.lock"] {
        assert!(pun.contains(manifest), "no {manifest}:\n{pun}");
    }
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

/// `lodi import` (#693) is held by the LD-376 guard before it reads anything.
#[test]
fn lodi_import_is_a_command_the_guard_holds() {
    let dir = scratch("guess-import");
    // check-host-safety: refusal — the guard refuses it before anything is read.
    let out = lodi_with(&dir, &argv(&["import"]), &[("LODI_HOST_REQUIRE_ROOT", "1")]);
    assert_eq!(out.status.code(), Some(9), "{out:?}");
    assert!(tree(&dir.join("home")).is_empty() && !dir.join("lodi.lock").exists());
    fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn lodi_setup_points_to_every_scope_s_first_command() {
    refused_with_a_pointer(
        &["setup"],
        &["lodi import", "lodi import --home", "lodi init"],
    );
}

#[test]
fn lodi_init_host_points_to_import() {
    refused_with_a_pointer(&["init", "host"], &["lodi import"]);
}

#[test]
fn lodi_init_home_points_to_import_home() {
    refused_with_a_pointer(&["init", "home"], &["lodi import --home"]);
    refused_with_a_pointer(&["init", "--force", "home"], &["lodi import --home"]);
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
///
/// The home verbs are 2.0's `lodi switch --home` and its `--dry-run`, each on a machine of its
/// own (`--root`) whose config holds a home with one file (LD-524).
#[test]
fn a_home_verb_under_sudo_warns_and_changes_nothing_else() {
    let (euid, other) = uids();
    let other = other.to_string();
    let own = euid.to_string();
    let verbs: &[&[&str]] = &[&["switch", "--home", "--dry-run"], &["switch", "--home"]];
    // `words` on the machine at `dir`, with `vars`, observed with every duration (`0.1s`) as `Ts`.
    let switch = |dir: &Path, words: &[&str], vars: &[(&str, &str)]| {
        support::machine_at(dir);
        let config = dir.join("home/.config/lodi");
        fs::create_dir_all(&config).unwrap();
        fs::write(
            config.join("home.toml"),
            "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"hi\"\n",
        )
        .unwrap();
        let mut args = argv(words);
        args.extend(["--root".to_string(), dir.display().to_string()]);
        let mut vars = vars.to_vec();
        vars.push(("LODI_HOST_REQUIRE_ROOT", "1"));
        let (code, out, err) = observed(dir, &lodi_with(dir, &args, &vars));
        let timeless = |text: String| {
            let timed = |word: &str| {
                let bare = word.trim_end_matches([':', ',']);
                let seconds = bare.strip_suffix('s').filter(|n| n.contains('.'));
                match seconds.is_some_and(|n| n.parse::<f64>().is_ok()) {
                    true => format!("Ts{}", &word[bare.len()..]),
                    false => word.to_string(),
                }
            };
            let lines = text.split('\n').map(|line| {
                let words = line.split(' ').map(timed);
                words.collect::<Vec<_>>().join(" ")
            });
            lines.collect::<Vec<_>>().join("\n")
        };
        (code, timeless(out), timeless(err))
    };
    // The home's files, its scratch folder spelled `<dir>`, without the run's dated log.
    let home = |dir: &Path| -> BTreeMap<PathBuf, String> {
        let at = dir.to_string_lossy().into_owned();
        tree(&dir.join("home"))
            .into_iter()
            .filter(|(path, _)| !path.starts_with(".local/state/lodi/logs"))
            .map(|(path, text)| (path, text.replace(&at, "<dir>")))
            .collect()
    };
    for words in verbs {
        let plain_dir = scratch("sudo-plain");
        let plain = switch(&plain_dir, words, &[]);
        let plain_tree = home(&plain_dir);
        let dir = scratch("sudo-warned");
        let warned = switch(&dir, words, &[("SUDO_UID", &other)]);
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
        assert_eq!(home(&dir), plain_tree);
        fs::remove_dir_all(&dir).unwrap();

        for quiet in [own.as_str(), "", "root", "-1"] {
            let dir = scratch("sudo-quiet");
            let out = switch(&dir, words, &[("SUDO_UID", quiet)]);
            assert_eq!(out, plain, "SUDO_UID={quiet:?} `lodi {}`", words.join(" "));
            fs::remove_dir_all(&dir).unwrap();
        }
        fs::remove_dir_all(&plain_dir).unwrap();
    }
    for words in [
        &["search", "python"][..],
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

/// The commands on `main` that no release has yet, as `lodi …` (validator-repair-1 of i-1). The
/// README's quick start is followed verbatim with the release it links to (`AGENTS.md` §9.4), so
/// none of these may be in its shell blocks. Empty this list in the release that ships them:
/// 1.2.0 shipped `lodi home init` (LD-382, LD-388), 1.5.0 the repository verbs (LD-416, LD-402).
const UNRELEASED: &[&str] = &[];

/// validator-repair-1 of i-1: the README's quick start installs the release `Cargo.toml` names,
/// so every shell line in it works with that release: no command in [`UNRELEASED`]. Its prose
/// names no command that stopped in 2.0 (#699).
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
    // check-host-safety: refusal — README text compared, never run.
    for gone in ["`lodi home import`", "`lodi home init`", "`lodi host arm`"] {
        assert!(
            !prose.contains(gone),
            "the quick start names {gone}, which stopped in 2.0 (#699)"
        );
    }
}

/// The `lodi` commands of the quick start's unindented `sh` blocks, in order, each as its
/// first word after `lodi`: what a reader pastes, comments left out.
fn quick_start_commands(quick: &str) -> Vec<String> {
    let mut in_shell = false;
    let mut commands = Vec::new();
    for line in quick.lines() {
        if line.starts_with("```") {
            in_shell = line == "```sh";
            continue;
        }
        let command = line.split('#').next().unwrap();
        let words: Vec<&str> = command.split_whitespace().collect();
        if in_shell && words.first() == Some(&"lodi") && words.len() > 1 {
            commands.push(words[1].to_string());
        }
    }
    commands
}

/// #711: the README quick start is install, then `lodi import`, then `lodi switch`, and its
/// first command is the first one `lodi --help` names under "Start here".
#[test]
fn the_quick_start_and_the_help_both_start_with_import_then_switch() {
    let text = fs::read_to_string(repo().join("README.md")).unwrap();
    let commands = quick_start_commands(section(&text, "## Quick start"));
    assert_eq!(
        commands.get(..2),
        Some(&["import".to_string(), "switch".to_string()][..]),
        "the quick start's first two lodi commands are {commands:?}"
    );
    let dir = scratch("help-opening");
    let help = String::from_utf8(lodi(&dir, &argv(&["--help"])).stdout).unwrap();
    let first = help
        .split("Start here:")
        .nth(1)
        .and_then(|rest| rest.split_whitespace().nth(1))
        .expect("`lodi --help` has a Start here list");
    assert_eq!(first, commands[0], "the help starts with `lodi {first}`");
    fs::remove_dir_all(&dir).unwrap();
}

/// The 1.x words a comment lodi writes may not use (#700): the verbs 2.0 renamed, and the
/// scopes and commands it removed. `None` when `comment` has none of them.
fn old_word(comment: &str) -> Option<String> {
    let words: Vec<&str> = comment
        .split(|c: char| c.is_whitespace() || "`'\"()".contains(c))
        .map(|w| w.trim_end_matches(['.', ',', ':', ';']))
        .filter(|w| !w.is_empty())
        .collect();
    for (at, word) in words.iter().enumerate() {
        if ["apply", "plan"].contains(word) {
            return Some((*word).to_string());
        }
        let next = words.get(at + 1).copied().unwrap_or_default();
        if *word == "lodi" && ["host", "home", "lock", "trust", "info", "boot"].contains(&next) {
            return Some(format!("lodi {next}"));
        }
    }
    comment
        .contains("repository's root")
        .then(|| "repository".to_string())
}

/// The comments of one file lodi writes, each with the 1.x word it uses.
fn old_words(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| Some(line.trim_start().strip_prefix('#')?.to_string()))
        .filter_map(|comment| Some((old_word(&comment)?, comment)))
        .collect()
}

/// The comments lodi writes into `host.toml`, `home.toml` and `lodi.toml` name only 2.0
/// commands (#700). The import's host and home files are the fixtures their emitters are
/// compared with byte for byte; `lodi.toml` is what `lodi init` writes, plain and on a base.
#[test]
fn the_comments_lodi_writes_name_only_2_0_commands() {
    let mut files: Vec<(String, String)> = Vec::new();
    let mut dirs = vec![repo().join("tests/fixtures/host/import")];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                dirs.push(path);
            } else if path.file_name().is_some_and(|n| n == "expected.toml") {
                files.push((
                    path.display().to_string(),
                    fs::read_to_string(&path).unwrap(),
                ));
            }
        }
    }
    assert!(files.len() > 10, "the host import fixtures were not found");
    let home = repo()
        .join("tests/fixtures")
        .join("home")
        .join("import/stubs/home.toml");
    files.push((
        home.display().to_string(),
        fs::read_to_string(&home).unwrap(),
    ));
    for (tag, args) in [
        ("plain", vec![]),
        ("base", vec!["--base", "debian:bookworm"]),
    ] {
        let dir = scratch(&format!("comments-{tag}"));
        let mut words = vec!["init"];
        words.extend(args);
        let out = lodi(&dir, &argv(&words));
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        files.push((
            format!("lodi init ({tag})"),
            fs::read_to_string(dir.join("lodi.toml")).unwrap(),
        ));
    }
    for (name, text) in &files {
        assert_eq!(
            old_words(text),
            vec![],
            "{name} has comments with 1.x words"
        );
    }
    // The negative control: each kind of 1.x word is noticed in a comment, and not outside one.
    for comment in [
        "# an apply installs it",
        "# then read the plan:",
        &format!("# run lodi {} first", "home"),
        "# recipes/NAME.toml at your repository's root.",
    ] {
        assert_eq!(old_words(comment).len(), 1, "{comment} went unnoticed");
    }
    assert!(old_words("apply = true\n# a switch installs it").is_empty());
}
