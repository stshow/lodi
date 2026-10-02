//! Every code in `docs/ERRORS.md` produced by a real run, and the catalogue held to its text
//! (M-1.0 T-3, design calls D9 and D11).
//!
//! `tests/diagnostics.rs` proves that the catalogue and the build name the same codes. What it
//! cannot prove is that any of those codes was ever *produced*, or that the words of a row
//! resemble what the user is shown. This binary closes both gaps:
//!
//! 1. a table with one entry per code, whose closure drives a **real** code path — the built
//!    binary through `Command`, or the exact library function the binary calls — and returns what
//!    reached standard error, plus the observed process exit status where a process was run;
//! 2. the recorded specimens in `tests/fixtures/errors/specimens.json`, compared byte for byte
//!    with what this run produced;
//! 3. the text contract: every row must quote, in backticks, a span that appears verbatim in its
//!    own specimen — there is no exemption list;
//! 4. the closed registry: a code that appears as a literal in `src/` and not in
//!    `lodi::diag::CODES` fails here, so nothing reaches `exit_status`'s `_` arm unnoticed.
//!
//! Everything is offline and deterministic. Nothing is downloaded and no container is started.
//! Scratch state lives under `CARGO_TARGET_TMPDIR`; the one write outside it is the specimen file
//! itself, and only in the recording mode described below.
//!
//! **Recording.** The specimen file is rewritten by a `cargo test` run with the environment
//! variable [`RECORD_VAR`] set. That variable is read by this test binary and by nothing else:
//! `the_recording_mode_is_a_test_harness_mode_only` scans `src/` and fails if it ever is.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/mod.rs"]
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::diag::{self, Diagnostic};
use lodi::fetch::Fetcher;
use lodi::manifest::{ManifestErrors, parse_project_manifest, parse_project_manifest_bytes};
use serde::{Deserialize, Serialize};

/// The environment variable that turns this binary into its recording mode. It is a
/// **test-harness** variable: no product source file reads it, and a test proves that.
const RECORD_VAR: &str = "LODI_RECORD_SPECIMENS";

/// The one command that records a specimen. Every failure of this binary that a new code causes
/// prints this line and nothing else by way of a remedy.
const RECORD_COMMAND: &str = "LODI_RECORD_SPECIMENS=1 cargo test --locked --test errors_catalogue";

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The directory this binary's scratch directories are made in ([`support::scratch`]), read to
/// take it out of a specimen; no path is built under it by hand (#350).
fn target_tmp() -> &'static Path {
    static TMP: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    TMP.get_or_init(|| {
        let probe = support::scratch("errors-catalogue-tmp");
        probe
            .parent()
            .expect("a scratch directory has a parent")
            .to_path_buf()
    })
}

fn specimen_file() -> PathBuf {
    repo().join("tests/fixtures/errors/specimens.json")
}

fn recording() -> bool {
    std::env::var_os(RECORD_VAR).is_some()
}

// ------------------------------------------------------------------------ the specimen file ---

/// One recorded diagnostic, as it reaches standard error.
///
/// `message` is the `lodi: error CODE: …` line, `hint` the `= hint: …` line if the diagnostic has
/// one, and `notes` every other rendered line of it — the `--> file:line:column` location and any
/// note that is not a hint — in the order they are printed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Specimen {
    code: String,
    exit: u8,
    message: String,
    hint: Option<String>,
    notes: Vec<String>,
}

impl Specimen {
    /// Everything of the specimen a catalogue row may quote: the message **without** the
    /// `lodi: error CODE: ` (or `lodi: warning CODE: `) prefix, the hint and the notes. The prefix
    /// is cut deliberately, so that a row cannot satisfy the text contract by quoting the code it
    /// is a row for.
    fn quotable(&self) -> String {
        let body = self
            .message
            .split_once(&head(&self.code))
            .map_or(self.message.clone(), |(_, rest)| rest.to_string());
        let mut text = body;
        if let Some(hint) = &self.hint {
            text.push('\n');
            text.push_str(hint);
        }
        for note in &self.notes {
            text.push('\n');
            text.push_str(note);
        }
        text
    }
}

fn read_specimens() -> BTreeMap<String, Specimen> {
    let path = specimen_file();
    let text = fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "{} cannot be read ({e}); record it with\n{RECORD_COMMAND}",
            path.display()
        )
    });
    let list: Vec<Specimen> = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{} is not the specimen format: {e}", path.display()));
    let mut map = BTreeMap::new();
    for specimen in list {
        assert!(
            map.insert(specimen.code.clone(), specimen).is_none(),
            "{} lists a code twice",
            path.display()
        );
    }
    map
}

fn write_specimens(specimens: &BTreeMap<String, Specimen>) {
    let path = specimen_file();
    fs::create_dir_all(path.parent().expect("a parent")).expect("the fixture directory");
    let list: Vec<&Specimen> = specimens.values().collect();
    fs::write(&path, lodi::lock::canonical_json(&list)).expect("the specimen file is written");
}

// ------------------------------------------------------------------------------ sanitization ---

/// What a run put on standard error, and the exit status of the process where one was run.
struct Produced {
    stderr: String,
    exit: Option<u8>,
}

/// Replace everything in a rendered diagnostic that this machine, this checkout or this process
/// would otherwise put into a committed file.
///
/// Paths are the only variable part of any message here, and every one of them is either the
/// scratch directory the test made or the checkout it runs in. `AGENTS.md` §1 forbids committing
/// either; the placeholders keep the specimen readable and keep the file identical on every
/// machine. `assert_sanitized` then fails if anything absolute survived.
fn sanitize(text: &str) -> String {
    let mut out = text.to_string();
    // The host scope canonicalizes the root it was given, so the resolved spelling of the
    // scratch directory has to be replaced as well as the one the test handed over.
    let canonical_tmp = fs::canonicalize(target_tmp())
        .unwrap_or_else(|_| target_tmp().to_path_buf())
        .display()
        .to_string();
    for (needle, placeholder) in [
        (canonical_tmp, "<tmp>"),
        (target_tmp().display().to_string(), "<tmp>"),
        (repo().display().to_string(), "<repo>"),
    ] {
        out = out.replace(&needle, placeholder);
    }
    // A scratch directory names the process that made it (`tests/support/hostroot.rs`), and the
    // store names its staging directory the same way. The placeholder ends in a word character
    // on purpose: `scripts/check-host-safety.py` reads a committed `etc/lodi` path written with
    // a leading slash as an absolute system path, and a path segment is what this really is.
    // A case's own scratch directory is named as the specimens were recorded; any other scratch
    // directory is `NAME-PID-N` (`support::scratch`, a host root), and its counter N depends on
    // the order the tests ran in, so the whole suffix becomes `-pid`.
    for (made, name) in SCRATCH_NAMES.lock().unwrap().iter() {
        out = out.replace(made, name);
    }
    let suffix = format!("-{}-", std::process::id());
    let mut kept = String::new();
    let mut rest = out.as_str();
    while let Some(at) = rest.find(&suffix) {
        let after = &rest[at + suffix.len()..];
        let digits = after.bytes().take_while(u8::is_ascii_digit).count();
        kept.push_str(&rest[..at]);
        if digits > 0 {
            kept.push_str("-pid");
            rest = &after[digits..];
        } else {
            kept.push_str(&suffix);
            rest = after;
        }
    }
    kept.push_str(rest);
    out = kept.replace(&std::process::id().to_string(), "pid");
    if let Some(home) = std::env::var_os("HOME") {
        let home = home.to_string_lossy().to_string();
        if home.len() > 1 {
            out = out.replace(&home, "<home>");
        }
    }
    // The store names its staging directory after the process and an allocation counter, so the
    // segment below `store/tmp/` is replaced whole rather than digit by digit.
    while let Some(at) = out.find("store/tmp/") {
        let rest = &out[at + "store/tmp/".len()..];
        let end = rest.find('/').unwrap_or(rest.len());
        let segment = rest[..end].to_string();
        if segment == "<staging>" {
            break;
        }
        out = out.replacen(&format!("store/tmp/{segment}"), "store/tmp/<staging>", 1);
    }
    // A message that names the running Lodi would make every specimen a hostage of the next
    // version bump, and T-10 moves the version. The code is what a specimen pins, not the
    // number: `tests/diagnostics.rs` and the `E_VERSION` case itself hold the real number.
    out.replace(env!("CARGO_PKG_VERSION"), "<version>")
}

/// Set every process-wide variable this binary's producers need, once, before any test body
/// proceeds.
///
/// `hostroot::shims` puts its shim directory on `PATH` and `support::home_gate` sets
/// `LODI_FS_LEDGER`, each behind its own one-shot guard. Two of those running in different test
/// threads would be two concurrent writes to the environment, so every test in this binary goes
/// through this one gate first and all of the writing happens inside it.
fn prime() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        hostroot::shims();
        support::home_gate();
        // The trust gate reads its store from the environment and there is no way to hand it
        // one. A scratch directory keeps this test out of the developer's own trust store.
        let config = support::scratch("errors-catalogue-config");
        // SAFETY: every test of this binary blocks on this `Once` before it does anything, so
        // no thread of this process reads the environment while it is being written.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", &config) };
    });
}

/// Nothing absolute, and nothing of a home, survives into a committed specimen.
///
/// `store/tmp/<staging>` is the store's own relative staging path, already replaced whole by
/// [`sanitize`], so it is taken out before the scan for a temporary directory of this machine.
fn assert_sanitized(code: &str, text: &str) {
    let text = &text.replace("store/tmp/<staging>", "store/<staging>");
    for forbidden in ["/home/", "/root/", "/tmp/"] {
        assert!(
            !text.contains(forbidden),
            "{code}: the specimen still carries {forbidden}: {text}"
        );
    }
    assert!(
        !text.contains(&target_tmp().display().to_string()),
        "{code}: the specimen still carries the target directory: {text}"
    );
}

fn from_diagnostic(d: &Diagnostic) -> Produced {
    Produced {
        stderr: sanitize(&d.to_string()),
        exit: None,
    }
}

fn from_errors(e: &ManifestErrors) -> Produced {
    Produced {
        stderr: sanitize(&e.to_string()),
        exit: None,
    }
}

fn from_output(out: &Output) -> Produced {
    Produced {
        stderr: sanitize(&String::from_utf8_lossy(&out.stderr)),
        exit: Some(u8::try_from(out.status.code().expect("the process exited")).expect("a status")),
    }
}

/// How a diagnostic's line starts: `lodi: error CODE: `, or `lodi: warning CODE: ` for the one
/// kind of `W_` code the catalogue has a row for (`W_SHADOWED`, LD-349).
fn head(code: &str) -> String {
    let kind = if code.starts_with("W_") {
        "warning"
    } else {
        "error"
    };
    format!("lodi: {kind} {code}: ")
}

/// The rendered block of one diagnostic inside everything a run printed: its `lodi: error CODE:`
/// (or `lodi: warning CODE:`) line and the indented lines that belong to it.
fn extract(code: &str, produced: &Produced) -> Specimen {
    let head = head(code);
    let mut lines = produced.stderr.lines();
    let message = lines
        .find(|line| line.starts_with(&head))
        .unwrap_or_else(|| {
            panic!(
                "the run produced no {code} line. What it printed:\n{}",
                produced.stderr
            )
        })
        .to_string();
    let mut hint = None;
    let mut notes = Vec::new();
    for line in lines {
        if !line.starts_with(' ') {
            break;
        }
        let body = line.trim_start();
        if body.starts_with("= hint: ") && hint.is_none() {
            hint = Some(line.to_string());
        } else {
            notes.push(line.to_string());
        }
    }
    // Where a process ran, the status it really exited with is the specimen's and this assertion
    // is the check that the command and the registry agree. Where the case called the library
    // function directly there is nothing observed to compare, so the specimen carries the
    // registry's status and `docs/ERRORS.md`'s exit column is checked against it by the caller.
    let exit = produced.exit.unwrap_or_else(|| diag::exit_status(code));
    assert_eq!(
        exit,
        diag::exit_status(code),
        "{code}: the process exited {exit}, lodi::diag::exit_status says {}",
        diag::exit_status(code)
    );
    let specimen = Specimen {
        code: code.to_string(),
        exit,
        message,
        hint,
        notes,
    };
    assert_sanitized(code, &specimen.message);
    for line in specimen.hint.iter().chain(specimen.notes.iter()) {
        assert_sanitized(code, line);
    }
    specimen
}

// ------------------------------------------------------------------- docs/ERRORS.md, parsed ---

/// One row of the catalogue: the exit column, the cause cell and the next-step cell.
#[derive(Debug, Clone)]
struct Row {
    exit: u8,
    cause: String,
    next: String,
}

/// The rows of a catalogue file. The parameter is a path so that a negative control can point it
/// at a deliberately reworded copy in a scratch directory.
fn catalogue_rows(path: &Path) -> BTreeMap<String, Row> {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| `E_") && !line.starts_with("| `W_") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(cells.len(), 4, "{}: {line}", path.display());
        let code = cells[0].trim_matches('`').to_string();
        let exit: u8 = cells[1]
            .parse()
            .unwrap_or_else(|_| panic!("{}: {code}'s exit column", path.display()));
        rows.insert(
            code,
            Row {
                exit,
                cause: cells[2].to_string(),
                next: cells[3].to_string(),
            },
        );
    }
    assert!(!rows.is_empty(), "{} has no rows", path.display());
    rows
}

/// The table of situations the catalogue gives a code under the heading that says each one
/// prints it, or nothing when the code has no such table.
fn situations(doc: &str, code: &str) -> String {
    let marker = format!("Each situation below prints `{code}`");
    let Some(start) = doc.find(&marker) else {
        return String::new();
    };
    let rest = &doc[start..];
    rest[..rest.find("\n#").unwrap_or(rest.len())].to_string()
}

/// Every span between backticks in a cell.
fn quoted_spans(cell: &str) -> Vec<String> {
    cell.split('`')
        .skip(1)
        .step_by(2)
        .filter(|span| !span.trim().is_empty())
        .map(str::to_string)
        .collect()
}

/// The codes whose row quotes nothing that its specimen actually prints (D11). A pure function of
/// its two arguments, so that the negative control can feed it a reworded catalogue.
fn rows_that_quote_nothing_real(
    rows: &BTreeMap<String, Row>,
    specimens: &BTreeMap<String, Specimen>,
) -> Vec<String> {
    let mut bad = Vec::new();
    for (code, row) in rows {
        let Some(specimen) = specimens.get(code) else {
            continue;
        };
        let haystack = specimen.quotable();
        let spans: Vec<String> = quoted_spans(&row.cause)
            .into_iter()
            .chain(quoted_spans(&row.next))
            .collect();
        if !spans.iter().any(|span| haystack.contains(span.as_str())) {
            bad.push(code.clone());
        }
    }
    bad
}

// ------------------------------------------------------------------- src/, scanned for codes ---

fn rust_files(dir: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(at) = stack.pop() {
        for entry in fs::read_dir(&at).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

/// Every `"E_…"` literal in one source file, outside its `#[cfg(test)]` module — the same cut
/// `tests/diagnostics.rs` makes, and for the same reason: a unit test's literal is not something
/// this build can put in front of a user.
fn codes_in(source: &str) -> BTreeSet<String> {
    let code = match source.find("#[cfg(test)]") {
        Some(at) => &source[..at],
        None => source,
    };
    let mut found = BTreeSet::new();
    let mut rest = code;
    while let Some(at) = rest.find("\"E_") {
        rest = &rest[at + 1..];
        if let Some(end) = rest.find('"') {
            let candidate = &rest[..end];
            if !candidate.is_empty()
                && candidate
                    .chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
            {
                found.insert(candidate.to_string());
            }
        }
    }
    found
}

/// The codes a build emits that its registry does not know (D9). Pure in its arguments so that a
/// negative control can hand it a source file this repository does not contain.
fn codes_missing_from_registry(sources: &[String], registry: &[(&str, u8)]) -> Vec<String> {
    let known: BTreeSet<&str> = registry.iter().map(|(code, _)| *code).collect();
    let mut missing = BTreeSet::new();
    for source in sources {
        for code in codes_in(source) {
            if !known.contains(code.as_str()) {
                missing.insert(code);
            }
        }
    }
    missing.into_iter().collect()
}

/// The registry entries the catalogue has no row for (D9). Pure in its arguments.
fn registry_codes_without_a_row(
    registry: &[(&str, u8)],
    rows: &BTreeMap<String, Row>,
) -> Vec<String> {
    registry
        .iter()
        .filter(|(code, _)| !rows.contains_key(*code))
        .map(|(code, _)| (*code).to_string())
        .collect()
}

fn product_sources() -> Vec<String> {
    let files = rust_files(&repo().join("src"));
    assert!(files.len() > 10, "the product sources were not found");
    files
        .iter()
        .map(|file| fs::read_to_string(file).unwrap())
        .collect()
}

// =========================================================================== the table ==========

/// The directories [`scratch`] made, each with the name a specimen gives it: the case's own,
/// without the process and counter [`support::scratch`] adds (#350).
static SCRATCH_NAMES: std::sync::Mutex<Vec<(String, String)>> = std::sync::Mutex::new(Vec::new());

/// A scratch directory named after the case, under `CARGO_TARGET_TMPDIR` and nowhere else.
fn scratch(tag: &str) -> PathBuf {
    let name = format!("errors-catalogue-{tag}");
    let dir = support::scratch(&name);
    let made = dir.file_name().expect("a scratch name").to_string_lossy();
    SCRATCH_NAMES
        .lock()
        .unwrap()
        .push((format!("/{made}"), format!("/{name}")));
    dir
}

/// Run the built binary in `dir`, as a user would.
fn lodi(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the lodi binary runs")
}

fn project_errors(text: &str) -> ManifestErrors {
    parse_project_manifest(text, "lodi.toml").expect_err("the manifest is refused")
}

/// A program module for the `W_SHADOWED` case alone. The case drives the functions `lodi home
/// plan`, `apply` and `status` call — `plan_with`, `apply_with` and `status_with` — with this
/// module as their registry, which is the seam `tests/home_programs.rs` proves the framework
/// through. (Written while no module shipped, LD-349; the shipped `git` module reaches the same
/// line through the binary, `tests/home_programs_shipped.rs`.)
struct Shade;

impl lodi::home::programs::Program for Shade {
    const NAME: &'static str = "shade";

    fn read(fields: &mut lodi::home::programs::Fields<'_>) -> Option<Self> {
        (!fields.failed()).then_some(Shade)
    }

    fn render(&self, env: &lodi::home::programs::RenderEnv) -> Vec<lodi::home::programs::Rendered> {
        vec![lodi::home::programs::Rendered {
            path: env.xdg_config_home.join("shade/config.toml"),
            body: lodi::home::programs::Body::Toml(lodi::home::toml::Table::new()),
            state: lodi::home::programs::FileState::Managed,
            takes_extra: true,
        }]
    }

    fn shadowed_by(
        &self,
        env: &lodi::home::programs::RenderEnv,
    ) -> Vec<lodi::home::programs::Shadow> {
        vec![lodi::home::programs::Shadow {
            by: env.home.join(".shaderc"),
            of: env.xdg_config_home.join("shade/config.toml"),
        }]
    }
}

const SHADE: &[lodi::home::programs::Module] = &[lodi::home::programs::module::<Shade>()];

/// One entry of the table: a code, and a closure that drives a real code path and returns what
/// reached standard error together with the process exit status where a process was run.
struct Case {
    code: &'static str,
    run: fn() -> Produced,
}

// ================================================================================ the checks ====

/// Acceptance (e). The recording mode is a property of this test binary; no product source file
/// reads the variable, so no user can put the build into it.
#[test]
fn the_recording_mode_is_a_test_harness_mode_only() {
    prime();
    let hits: Vec<&str> = product_sources()
        .iter()
        .filter(|source| source.contains(RECORD_VAR))
        .map(|_| RECORD_VAR)
        .collect();
    assert!(
        hits.is_empty(),
        "{RECORD_VAR} is read by {} product source file(s); recording is a test-harness mode \
         and must never be a product code path",
        hits.len()
    );
}

/// Acceptance (c), the real check: every code this build can put in front of a user is in the
/// closed registry, so none of them reaches `exit_status`'s `_` arm and silently becomes exit 3.
#[test]
fn every_code_the_sources_emit_is_in_the_registry() {
    prime();
    let missing = codes_missing_from_registry(&product_sources(), diag::CODES);
    assert!(
        missing.is_empty(),
        "{missing:?} appear in src/ and not in lodi::diag::CODES. Add each one to CODES with its \
         exit status, give it a row in docs/ERRORS.md, and record its specimen:\n{RECORD_COMMAND}"
    );
}

/// The `E_` codes `sources` can print: every literal outside a test module, less the registry's
/// own table, which names a code without printing it. Pure in its arguments.
fn codes_printed(sources: &[String]) -> BTreeSet<String> {
    let mut printed = BTreeSet::new();
    for source in sources {
        let source = match source.find("pub const CODES: &[(&str, u8)] = &[") {
            Some(at) => {
                let end = at + source[at..].find("\n];").expect("the table ends");
                format!("{}{}", &source[..at], &source[end..])
            }
            None => source.clone(),
        };
        printed.extend(codes_in(&source));
    }
    printed
}

/// The error reference both ways (#700): every `E_` code the sources can print has a row in
/// `docs/ERRORS.md`, and every `E_` row is a code they can print. The negative controls: a code
/// only the registry's table names is not printed, and a source's new code is.
#[test]
fn the_error_reference_is_the_codes_the_sources_print() {
    prime();
    let printed = codes_printed(&product_sources());
    let rows: BTreeSet<String> = catalogue_rows(&repo().join("docs/ERRORS.md"))
        .into_keys()
        .filter(|code| code.starts_with("E_"))
        .collect();
    let gone: Vec<&String> = rows.difference(&printed).collect();
    assert!(
        gone.is_empty(),
        "docs/ERRORS.md has rows for {gone:?}, which no source prints"
    );
    let missing: Vec<&String> = printed.difference(&rows).collect();
    assert!(
        missing.is_empty(),
        "the sources print {missing:?}, which docs/ERRORS.md has no row for"
    );
    let table =
        "pub const CODES: &[(&str, u8)] = &[\n    (\"E_ONLY_TABLED\", 3),\n];\n".to_string();
    assert!(codes_printed(std::slice::from_ref(&table)).is_empty());
    let new = format!("{table}fn f() {{ Diagnostic::new(\"E_NEW\", \"x\"); }}");
    assert_eq!(codes_printed(&[new]), BTreeSet::from(["E_NEW".to_string()]));
}

/// Acceptance (c), the negative controls. Each check is a pure function of its arguments, so it
/// can be shown to fire on input this repository does not contain.
#[test]
fn the_registry_checks_catch_what_they_are_for() {
    prime();
    let rows = catalogue_rows(&repo().join("docs/ERRORS.md"));

    // A code a lane adds to src/ without touching the registry.
    let invented = "fn refuse() -> Diagnostic { Diagnostic::new(\"E_INVENTED_BY_A_LANE\", \"x\") }"
        .to_string();
    assert_eq!(
        codes_missing_from_registry(&[invented], diag::CODES),
        vec!["E_INVENTED_BY_A_LANE"]
    );

    // A literal inside a unit-test module is not something a user can meet, and is not a code.
    let only_in_a_test =
        "#[cfg(test)]\nmod tests { const C: &str = \"E_ONLY_IN_A_UNIT_TEST\"; }".to_string();
    assert!(codes_missing_from_registry(&[only_in_a_test], diag::CODES).is_empty());

    // A registry entry the catalogue has no row for.
    assert!(registry_codes_without_a_row(diag::CODES, &rows).is_empty());
    let mut extended: Vec<(&str, u8)> = diag::CODES.to_vec();
    extended.push(("E_NOT_IN_THE_CATALOGUE", 3));
    assert_eq!(
        registry_codes_without_a_row(&extended, &rows),
        vec!["E_NOT_IN_THE_CATALOGUE"]
    );
}

/// Acceptance (b), the negative control: a row whose backticked spans are not what the build
/// prints fails the text contract. The catalogue is copied into a scratch directory and reworded
/// there; `docs/ERRORS.md` itself is never written by a test.
#[test]
fn a_reworded_row_fails_the_text_contract() {
    prime();
    let specimens = read_specimens();
    let dir = support::scratch("errors-catalogue-reworded");
    let copy = dir.join("ERRORS.md");

    let text = fs::read_to_string(repo().join("docs/ERRORS.md")).expect("the catalogue");
    let victim = "E_NO_MANIFEST";
    let reworded: String = text
        .lines()
        .map(|line| {
            if line.trim_start().starts_with(&format!("| `{victim}` |")) {
                format!(
                    "| `{victim}` | 3 | A cause that quotes \
                     `a-span-no-diagnostic-of-this-build-prints`. | A next step that quotes \
                     `another-span-this-build-never-prints`. |"
                )
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<String>>()
        .join("\n");
    fs::write(&copy, &reworded).expect("the reworded copy");

    let rows = catalogue_rows(&copy);
    assert!(
        rows[victim].cause.contains("a-span-no-diagnostic"),
        "the control really did reword the row"
    );
    assert_eq!(
        rows_that_quote_nothing_real(&rows, &specimens),
        vec![victim.to_string()],
        "a reworded row must fail the text contract"
    );
    fs::remove_dir_all(&dir).expect("the scratch directory is removed");
}

/// Acceptance (a), (b) and (d): every documented code is produced by a real run here, its
/// specimen is the one committed, its exit status is the row's and `exit_status`'s, and its row
/// quotes something the build actually prints.
///
/// The three sets — the table, `docs/ERRORS.md` and `lodi::diag::CODES` — are compared **at test
/// time**, so a code another lane lands has nowhere to hide and there is no allow-list.
#[test]
fn every_documented_code_is_produced_by_a_real_run() {
    prime();
    let rows = catalogue_rows(&repo().join("docs/ERRORS.md"));
    let documented: BTreeSet<String> = rows.keys().cloned().collect();
    let registered: BTreeSet<String> = diag::CODES
        .iter()
        .map(|(code, _)| (*code).to_string())
        .collect();
    let tabled: BTreeSet<String> = CASES.iter().map(|c| c.code.to_string()).collect();
    assert_eq!(
        tabled.len(),
        CASES.len(),
        "the table lists a code twice: {tabled:?}"
    );
    assert_eq!(
        documented, registered,
        "docs/ERRORS.md and lodi::diag::CODES are not the same set"
    );
    let unproduced: Vec<&String> = documented.difference(&tabled).collect();
    assert!(
        unproduced.is_empty(),
        "no entry in tests/errors_catalogue.rs produces {unproduced:?}. Add one that drives the \
         real code path, then record its specimen:\n{RECORD_COMMAND}"
    );
    let undocumented: Vec<&String> = tabled.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "the table produces {undocumented:?}, which docs/ERRORS.md has no row for"
    );

    let mut produced = BTreeMap::new();
    for case in CASES {
        let run = (case.run)();
        produced.insert(case.code.to_string(), extract(case.code, &run));
    }

    if recording() {
        write_specimens(&produced);
    }
    let recorded = read_specimens();

    for (code, specimen) in &produced {
        let Some(committed) = recorded.get(code) else {
            panic!("{code} has no committed specimen:\n{RECORD_COMMAND}");
        };
        assert_eq!(
            committed, specimen,
            "{code}: the committed specimen is not what this run produced.\n  committed: \
             {committed:?}\n  produced:  {specimen:?}\n{RECORD_COMMAND}"
        );
        let row = &rows[code];
        assert_eq!(
            row.exit, specimen.exit,
            "{code}: docs/ERRORS.md says exit {}, the run exited {}",
            row.exit, specimen.exit
        );
        assert_eq!(
            row.exit,
            diag::exit_status(code),
            "{code}: docs/ERRORS.md and lodi::diag::exit_status disagree"
        );
    }
    let stale: Vec<&String> = recorded
        .keys()
        .filter(|c| !produced.contains_key(*c))
        .collect();
    assert!(
        stale.is_empty(),
        "the specimen file still holds {stale:?}, which nothing produces any more:\n\
         {RECORD_COMMAND}"
    );

    let silent = rows_that_quote_nothing_real(&rows, &recorded);
    assert!(
        silent.is_empty(),
        "the docs/ERRORS.md rows for {silent:?} quote nothing their diagnostic prints. Quote a \
         span of the message or the hint in backticks, in the Cause or the Next step cell."
    );
}

/// The context a host command builds from the root it was pointed at, with the values
/// `tests/host_core.rs` uses: the manifest reader never looks at this machine.
fn host_context() -> lodi::hostscope::manifest::Context {
    lodi::hostscope::manifest::Context {
        arch: "x86_64".into(),
        os: "linux".into(),
        distro: "debian".into(),
        release: "12".into(),
        codename: "bookworm".into(),
        lodi_version: env!("CARGO_PKG_VERSION").into(),
    }
}

fn host_errors(text: &str) -> ManifestErrors {
    lodi::hostscope::manifest::parse(text, "host.toml", &host_context())
        .expect_err("the manifest is refused")
}

/// A scratch host root, armed and given a Debian identity, with the manifest under test.
///
/// Every host case below runs against one of these: a directory this test created under
/// `CARGO_TARGET_TMPDIR`, armed by the test itself. **No host code in this repository is ever
/// run against `/`** (`AGENTS.md` §8, LD-45), not even to produce a specimen.
fn host_root(tag: &str, manifest: &str) -> hostroot::Root {
    let root = hostroot::Root::new(&format!("cat-{tag}"));
    root.debian_with(manifest);
    root
}

fn from_host_error(e: &lodi::hostscope::HostError) -> Produced {
    Produced {
        stderr: sanitize(&e.to_string()),
        exit: None,
    }
}

/// An in-process fetcher over a fixed set of URLs: the idiom every offline test of this
/// repository uses (`tests/spike_lock.rs`, `tests/arch_base.rs`). A URL it does not hold is an
/// HTTP 404, which is what upstream not publishing something looks like. Nothing reaches the
/// network from this binary.
struct MapFetcher {
    files: BTreeMap<String, Vec<u8>>,
    log: std::sync::Mutex<Vec<String>>,
}

impl MapFetcher {
    fn new(files: &[(&str, &[u8])]) -> MapFetcher {
        MapFetcher {
            files: files
                .iter()
                .map(|(url, body)| ((*url).to_string(), body.to_vec()))
                .collect(),
            log: std::sync::Mutex::new(Vec::new()),
        }
    }
}

impl lodi::fetch::Fetcher for MapFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, lodi::fetch::FetchError> {
        self.log.lock().expect("the log").push(url.to_string());
        self.files
            .get(url)
            .cloned()
            .ok_or_else(|| lodi::fetch::FetchError::Status(url.to_string(), 404))
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().expect("the log").clone()
    }
}

/// The version index of the synthetic recipe below.
const WIDGET_INDEX: &str = "https://index.invalid/versions.txt";

// The synthetic tool's own version is deliberately not a version Lodi will ever carry. A
// specimen's message is redacted of `CARGO_PKG_VERSION` below, so a tool version equal to
// Lodi's would be redacted out of the asset URL too, and the specimen would change with every
// release; at 1.0.0 it did.

/// A synthetic catalogue recipe, parsed by the real recipe reader.
///
/// The upstream codes are decided by the resolver from the recipe and what upstream answers, so
/// a recipe of this repository's own shape — read by `lodi::catalogue::parse_recipe`, resolved by
/// `lodi::upstream::resolve_tool`, which is what `lodi lock` calls — drives them all without
/// pinning a specimen to a real tool whose upstream may change.
fn widget(asset_url: &str) -> lodi::catalogue::Recipe {
    let text = format!(
        "[recipe]\nname = \"widget\"\ndescription = \"a synthetic tool\"\n\
         homepage = \"https://example.invalid/\"\n\n\
         [versions]\nstrategy = \"text_index\"\nurl = \"{WIDGET_INDEX}\"\nline = \"${{version}}\"\n\n\
         [asset]\nurl = \"{asset_url}\"\nformat = \"binary\"\nchecksum_sidecar = \".sha256\"\n\n\
         [spec]\nbin = [\"bin/widget\"]\npath = []\n"
    );
    lodi::catalogue::parse_recipe(&text, "widget.toml").expect("the synthetic recipe parses")
}

fn resolve_widget(
    fetcher: &MapFetcher,
    asset_url: &str,
    constraint: &str,
    arch: &str,
) -> Diagnostic {
    let parsed = lodi::version::Constraint::parse(constraint).expect("the constraint parses");
    lodi::upstream::resolve_tool(
        fetcher,
        &widget(asset_url),
        "widget",
        constraint,
        &parsed,
        arch,
    )
    .expect_err("the resolution is refused")
}

/// A `ToolEntry` with the artifacts given: the shape the store reads a locked tool from.
fn widget_entry(artifacts: Vec<lodi::lock::ArtifactEntry>) -> lodi::lock::ToolEntry {
    lodi::lock::ToolEntry {
        request: lodi::lock::ToolRequest {
            constraint: "1.0.0".into(),
            name: "widget".into(),
            provider: "builtin".into(),
            declaration: None,
        },
        provider: "builtin".into(),
        version: "1.0.0".into(),
        tag: None,
        recipe: lodi::lock::RecipeRef {
            input: "builtin".into(),
            path: "widget.toml".into(),
            sha256: format!("sha256:{}", "0".repeat(64)),
        },
        artifacts,
        spec: lodi::lock::ToolSpec {
            arch: "x86_64".into(),
            bin: Vec::new(),
            path: Vec::new(),
            env: BTreeMap::new(),
        },
    }
}

fn no_hook(_: lodi::store::Point, _: &str) -> std::io::Result<()> {
    Ok(())
}

/// A stand-in for the `podman` executable, in a directory handed to `Podman::find`.
///
/// `E_LAYER_BUILD` and `E_CLOSURE_DRIFT` are decided by what an **external program** reports, so
/// they cannot be produced without one. This is the same seam the host scope's package-manager
/// shims are (M-0.6 design call D3, `tests/support/hostroot.rs`): the product resolves `podman`
/// by path and runs it, and what it finds here answers exactly as the real one would in the
/// failing case. Nothing about the product is mocked — the argv, the log, the closure query and
/// the refusals are all real.
fn podman_shim(dir: &Path, build_exit: i32) -> PathBuf {
    fs::create_dir_all(dir).expect("the shim directory");
    let path = dir.join("podman");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\n\
             # A stand-in for podman: this build starts no container (M-1.0 T-3).\n\
             shift\n\
             case \"$1\" in\n\
             \timage)\n\
             \t\tcase \"$2\" in\n\
             \t\t\texists) exit 0 ;;\n\
             \t\t\tinspect) echo 'sha256:{id}'; exit 0 ;;\n\
             \t\tesac ;;\n\
             \tbuild) echo 'the build refused the layer'; exit {build_exit} ;;\n\
             \trun) exit 0 ;;\n\
             esac\n\
             exit 0\n",
            id = "1".repeat(64)
        ),
    )
    .expect("the shim");
    fs::set_permissions(&path, PermissionsExt::from_mode(0o755)).expect("the shim is executable");
    dir.to_path_buf()
}

/// The committed fixture lock's base, pointed at bytes this test owns and trimmed to one
/// `base`-origin package, so that nothing is downloaded but the rootfs and the closure has
/// exactly one difference to report.
fn offline_base(rootfs: &[u8], url: &str) -> lodi::lock::BaseEntry {
    let bytes = fs::read(repo().join("tests/fixtures/spike/lodi.lock")).expect("the fixture lock");
    let mut base = lodi::lock::parse_lock(&bytes)
        .expect("the fixture lock parses")
        .base
        .expect("the fixture lock has a base");
    base.rootfs.url = url.to_string();
    base.rootfs.sha256 = lodi::util::sha256_tagged(rootfs);
    base.rootfs.size = Some(rootfs.len() as u64);
    base.closure.packages = vec![lodi::lock::ClosureEntry {
        name: "zlib1g".into(),
        version: "1:1.2.13.dfsg-1".into(),
        arch: Some("amd64".into()),
        origin: "base".into(),
        sha256: None,
        filename: None,
        size: None,
        repository: None,
        suite: None,
        epoch: None,
        release: None,
    }];
    base
}

/// One image realization against the shim, returning the refusal it produced.
fn realize_with_shim(tag: &str, build_exit: i32) -> Produced {
    let root = scratch(tag);
    let store = lodi::store::Store::open(&root.join("lodi-home")).expect("the scratch store");
    let podman =
        lodi::container::Podman::find(Some(podman_shim(&root.join("bin"), build_exit).as_os_str()))
            .expect("the stand-in is found where the product looks");
    let rootfs = b"a synthetic rootfs, never imported by a real podman".to_vec();
    let url = "https://artifacts.test/base/rootfs.tar.gz";
    let base = offline_base(&rootfs, url);
    let fetcher = MapFetcher::new(&[(url, &rootfs)]);
    let error = lodi::container::realize_image(
        &store,
        &podman,
        &fetcher,
        &base,
        &mut lodi::store::Report::default(),
    )
    .expect_err("the image is refused");
    assert!(
        fetcher.requests() == vec![url.to_string()],
        "only the rootfs was fetched: {:?}",
        fetcher.requests()
    );
    let produced = from_diagnostic(&error);
    support::make_writable(&root);
    fs::remove_dir_all(&root).expect("the scratch directory is removed");
    produced
}

/// The table: one entry per code in `docs/ERRORS.md`, in the order the catalogue lists them.
///
/// Each closure drives a real code path. Where the code belongs to a surface a command reaches,
/// the built binary is run; where it belongs to a library the binary calls — the manifest
/// readers, the archive reader, the store, the host scope — the closure calls exactly the
/// function the command calls, because that is what produces the diagnostic.
const CASES: &[Case] = &[
    // ---------------------------------------------------------------- the manifest (exit 3) ---
    Case {
        code: "E_NO_MANIFEST",
        // The built binary, in an empty directory: `lodi develop` refuses before it locks,
        // fetches or opens a store, so the observed exit status is recorded with the text.
        run: || {
            // A short name keeps the recorded path within the line length.
            let dir = scratch("nm");
            let produced = from_output(&lodi(&dir, &["develop", "--", "true"]));
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_EXISTS",
        run: || {
            let dir = scratch("exists");
            assert!(
                lodi(&dir, &["init", "--name", "hello"]).status.success(),
                "the first init writes the manifest"
            );
            let out = lodi(&dir, &["init", "--name", "other"]);
            let produced = from_output(&out);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_SYNTAX",
        run: || {
            from_errors(
                &parse_project_manifest_bytes(b"[env]\nA = \"\xff\"\n", "lodi.toml")
                    .expect_err("not UTF-8"),
            )
        },
    },
    Case {
        code: "E_TYPE",
        run: || from_errors(&project_errors("[project]\nversion = 1\n")),
    },
    Case {
        code: "E_IDENT",
        run: || from_errors(&project_errors("[tools.\"Bad Name\"]\nversion = \"1\"\n")),
    },
    Case {
        code: "E_UNKNOWN_ATTR",
        run: || from_errors(&project_errors("[project]\nnmae = \"hello\"\n")),
    },
    Case {
        code: "E_UNKNOWN_BLOCK",
        run: || from_errors(&project_errors("[contaienr]\n")),
    },
    Case {
        code: "E_UNSUPPORTED",
        // A block `spec/01` defines and this build deliberately does not implement. (It was
        // `[project] min_lodi_version` until M-1.0 T-1 implemented that key in the project
        // scope, where it is now `E_VERSION` — the specimen file is what caught the change.)
        run: || from_errors(&project_errors("[services]\nweb = { run = \"serve\" }\n")),
    },
    Case {
        code: "E_MODE",
        run: || from_errors(&project_errors("[packages]\ncommon = [\"git\"]\n")),
    },
    Case {
        code: "E_BLOCK_NOT_ALLOWED",
        run: || {
            from_errors(&project_errors(
                "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n\n\
                 [packages]\nhold = [\"git\"]\n",
            ))
        },
    },
    Case {
        code: "E_INLINE_NEEDS_EXACT",
        run: || {
            from_errors(&project_errors(
                "[tools.python]\nversion = \"3.12\"\nurl = \"https://example.invalid/p.tar.gz\"\n",
            ))
        },
    },
    Case {
        code: "E_EXCLUDED_CONSTRUCT",
        run: || from_errors(&project_errors("[tasks.t]\nrun = \"echo ${a + b}\"\n")),
    },
    Case {
        code: "E_CONFIG",
        // The function every command calls before it touches a path, with nothing in the
        // environment for it to use. It returns before it looks at the filesystem.
        run: || {
            from_diagnostic(
                &lodi::roots::Roots::from_vars(None, None, None, None)
                    .expect_err("there is no usable home"),
            )
        },
    },
    // --------------------------------------------------------------- build and store (exit 6) ---
    Case {
        code: "E_STORE_IO",
        // `lodi init` opens `./.gitignore` with `O_NOFOLLOW`; a symbolic link there is a write
        // it refuses rather than follows. No store is involved.
        run: || {
            let root = scratch("store-io");
            let project = root.join("project");
            fs::create_dir(&project).expect("the project directory");
            fs::write(root.join("outside"), b"target\n.lodi/\n").expect("the link target");
            std::os::unix::fs::symlink(root.join("outside"), project.join(".gitignore"))
                .expect("the symbolic link");
            let out = lodi(&project, &["init", "--name", "hello"]);
            let produced = from_output(&out);
            fs::remove_dir_all(&root).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_STORE_VERSION",
        // `lodi::layout::read` is exactly what `Store::open_for_write` calls before it creates a
        // single directory (M-1.0 T-2), so this is the refusal a `lodi develop`, `lodi run`,
        // `lodi shell`, `lodi home apply` or `lodi gc` against a store from the future prints.
        run: || {
            let home = scratch("store-version");
            fs::write(
                home.join(lodi::layout::MARKER),
                b"{\n  \"layout\": 99,\n  \"lodiVersion\": \"99.0.0\",\n  \
                  \"written\": \"2026-09-21T00:00:00Z\"\n}\n",
            )
            .expect("the marker of a store from the future");
            let produced =
                from_diagnostic(&lodi::layout::read(&home).expect_err("the layout is refused"));
            fs::remove_dir_all(&home).expect("the scratch directory is removed");
            produced
        },
    },
    // --------------------------------------------- the manifest (exit 3), home and host scope ---
    Case {
        code: "E_ATTR_CONFLICT",
        run: || {
            from_errors(&host_errors(
                "[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nsource = \"b\"\n",
            ))
        },
    },
    Case {
        code: "E_DUP_RESOURCE",
        run: || {
            from_errors(&host_errors(
                "[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\n\n\
                 [files.\"/etc/./sub/../x.conf\"]\ncontent = \"b\\n\"\n",
            ))
        },
    },
    Case {
        code: "E_PATH_ESCAPE",
        run: || from_errors(&host_errors("[files.\"etc/x.conf\"]\ncontent = \"a\\n\"\n")),
    },
    Case {
        code: "E_VAR_UNSET",
        run: || {
            from_errors(&host_errors(
                "[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nowner = \"${vars.absent}\"\n",
            ))
        },
    },
    Case {
        code: "E_VERSION",
        run: || from_errors(&host_errors("[host]\nmin_lodi_version = \">=99.0.0\"\n")),
    },
    Case {
        code: "E_UNKNOWN_PACKAGE",
        run: || {
            from_errors(&host_errors(
                "[packages]\ncommon = [\"git\"]\nhold = [\"curl\"]\n",
            ))
        },
    },
    Case {
        code: "E_UNKNOWN_UNIT",
        // A real host plan by the built binary over a scratch root and the fake systemctl
        // (sc-1): `[services]` declares a unit the machine does not have.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-unknown-unit", Machine::debian());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [services]\n\"x.service\" = \"enabled\"\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_UNKNOWN_SETTING",
        // A real host plan over a scratch root and the fake basics tools (sd-1): `[system]` names
        // a time zone the root has no zone file for.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-unknown-setting", Machine::debian());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [system]\ntimezone = \"Mars/Olympus\"\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_FIREWALL_CONFLICT",
        // A real host plan (sd-1): `[firewall]` on a machine whose fake systemctl has ufw active.
        run: || {
            use fakehost::{Case, Machine};
            let machine = Machine::debian().unit("ufw.service", "enabled", true, "enabled");
            let case = Case::new("errors-firewall-conflict", machine);
            case.root.write("etc/ufw/ufw.conf", "ENABLED=yes\n");
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [firewall]\nallow = [\"22/tcp\"]\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_BOOT_LOADER",
        // A real host plan (bc-1): `[kernel] parameters` on a root with no GRUB file.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-boot-loader", Machine::debian());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [kernel]\nparameters = [\"quiet\"]\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_BOOT_NOT_UEFI",
        // A real host plan (bl-1): `[boot]` on a root that did not start through UEFI.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-boot-not-uefi", Machine::debian());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [boot]\nloader = \"grub\"\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_NETWORK_STACK",
        // A real host plan (sd-1): `[network]` on a machine that runs none of the three stacks.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-network-stack", Machine::debian());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\n\n\
                 [network.interfaces.enp0s2]\ndhcp = true\n",
            );
            from_output(&case.plan())
        },
    },
    Case {
        code: "E_NETWORK_ROLLED_BACK",
        // A real host apply (sd-1): a network change the fake ping never confirms, put back after
        // `confirm_within` seconds.
        run: || {
            use fakehost::{Case, Machine};
            let machine = Machine::arch()
                .unit("systemd-networkd.service", "enabled", true, "enabled")
                .os("cut", serde_json::json!("192.168.77.10"));
            let case = Case::new("errors-network-rolled-back", machine);
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n\
                 [network]\nconfirm_within = 1\n\n[network.interfaces.eth0]\ndhcp = false\n\
                 addresses = [\"192.168.77.10/24\"]\ngateway = \"192.168.77.1\"\n",
            );
            from_output(&case.apply(&[]))
        },
    },
    Case {
        code: "E_HOST_MISMATCH",
        // The assertion the safety gate makes from the root's own `os-release`, with the file
        // name a command passes; no root and no filesystem are needed to make it.
        run: || {
            let os = lodi::hostscope::safety::OsRelease::parse(
                "ID=debian\nVERSION_ID=\"12\"\nVERSION_CODENAME=bookworm\n",
            );
            from_diagnostic(
                &os.assert_distro(Some("ubuntu"), "host.toml")
                    .expect_err("the manifest is for another distribution"),
            )
        },
    },
    Case {
        code: "E_PROTECTED_PATH",
        // A Fedora manifest declaring one of the package settings lodi never writes (LD-432).
        run: || {
            let mut context = host_context();
            context.distro = "fedora".into();
            context.release = "44".into();
            context.codename = String::new();
            from_errors(
                &lodi::hostscope::manifest::parse(
                    "[files.\"/etc/pki/lodi\"]\ncontent = \"x\"\n",
                    "host.toml",
                    &context,
                )
                .expect_err("a Fedora package setting is refused"),
            )
        },
    },
    Case {
        code: "E_GPGCHECK_OFF",
        // A Fedora `[sources]` table that switches the package signature check off (LD-433).
        run: || {
            let mut context = host_context();
            context.distro = "fedora".into();
            context.release = "44".into();
            context.codename = String::new();
            from_errors(
                &lodi::hostscope::manifest::parse(
                    "[sources.v]\nuris = [\"https://vendor.example/f44/\"]\n\
                     signed_by = \"vendor.asc\"\nsigned_by_sha256 = \"{}\"\ngpgcheck = false\n"
                        .replace("{}", &"0".repeat(64))
                        .as_str(),
                    "host.toml",
                    &context,
                )
                .expect_err("a repository with its signature check off is refused"),
            )
        },
    },
    Case {
        code: "E_SSH_KEY",
        // A private key pasted where a public one belongs, refused when the manifest is read.
        run: || {
            from_errors(&host_errors(
                "[users.alice]\nssh_keys = [\"-----BEGIN OPENSSH PRIVATE KEY-----\"]\n",
            ))
        },
    },
    Case {
        code: "E_IDENTITY_CONFLICT",
        // A real host plan by the built binary over a scratch root whose account alice has
        // another UID than host.toml declares: refused before anything is run.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-identity-conflict", Machine::debian());
            case.root.write(
                "etc/passwd",
                "root:x:0:0::/root:/bin/sh\nalice:x:1001:1001::/home/alice:/bin/bash\n",
            );
            case.set_manifest("[users.alice]\nuid = 1500\n");
            from_output(&case.verb("plan", &["--no-home"]))
        },
    },
    Case {
        code: "E_HOST_OSTREE",
        // A scratch root marked as booted from an ostree deployment, as Silverblue is.
        run: || {
            let root = hostroot::Root::new("cat-ostree");
            root.may_manage()
                .write("etc/os-release", "ID=fedora\nVERSION_ID=44\n")
                .write("etc/lodi/host.toml", "[host]\nversion = \"1\"\n")
                .write("run/ostree-booted", "");
            from_host_error(
                &lodi::hostscope::plan(&root.options()).expect_err("an rpm-ostree root"),
            )
        },
    },
    // ------------------------------------------- the host scope's transaction (exit 8) and 9 ---
    Case {
        code: "E_APPLY",
        // The plan's identity preflight, which writes nothing and needs no journal.
        run: || {
            let root = host_root(
                "apply",
                "[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nowner = \"nobodyatall\"\n",
            );
            from_host_error(
                &lodi::hostscope::plan(&root.options()).expect_err("an owner the root has not"),
            )
        },
    },
    Case {
        code: "E_JOURNAL_AMBIGUOUS",
        // `--resolved` naming a journal when nothing is outstanding: the same refusal an
        // operator meets when they answer a question the machine is not asking.
        run: || {
            let root = host_root("journal", "");
            let mut options = root.options();
            options.resolved = Some("20000101T000000Z-0000".to_string());
            from_host_error(&lodi::hostscope::apply(&options).expect_err("nothing is outstanding"))
        },
    },
    Case {
        code: "E_DECLINED",
        // Host drift without `--overwrite-drift`: one apply writes the file, a hand edits it,
        // the second apply refuses.
        run: || {
            let root = hostroot::Root::new("cat-declined");
            let (uid, gid) = hostroot::ids(&root);
            root.debian_with(&format!(
                "[files.\"/etc/drift.conf\"]\ncontent = \"declared\\n\"\nowner = \"{uid}\"\n\
                 group = \"{gid}\"\n"
            ));
            lodi::hostscope::apply(&root.options()).expect("the first apply");
            root.write("etc/drift.conf", "edited by hand\n");
            from_host_error(
                &lodi::hostscope::apply(&root.options()).expect_err("drift stops the apply"),
            )
        },
    },
    Case {
        code: "E_HOST_NOT_ARMED",
        run: || {
            let root = hostroot::Root::new("cat-unarmed");
            root.debian().write("etc/lodi/host.toml", "");
            from_host_error(
                &lodi::hostscope::plan(&root.options()).expect_err("the root is not armed"),
            )
        },
    },
    Case {
        code: "E_HOST_ROOT_REQUIRED",
        // The exact function `main` calls before every host command, with the setting a lane
        // and the gate export and no `--root` (LD-376). It reads nothing, so no root is involved.
        run: || {
            let refused =
                lodi::hostscope::require_root_for("switch", None, Some(std::ffi::OsStr::new("1")))
                    .expect_err("a rootless command under the guard is refused");
            from_diagnostic(&refused)
        },
    },
    Case {
        code: "E_NEED_ROOT",
        // The apply's identity preflight, under a scratch `--root`: an `owner` this process
        // cannot set stops it before anything is written.
        run: || {
            assert_ne!(
                lodi::hostscope::safety::current_euid(),
                0,
                "this case cannot be produced honestly by a suite running as root, and no test \
                 of this repository is ever run that way"
            );
            let root = host_root(
                "needroot",
                "[files.\"/etc/x.conf\"]\ncontent = \"a\\n\"\nowner = \"root\"\n",
            );
            root.write("etc/passwd", "root:x:0:0:root:/root:/bin/sh\n")
                .write("etc/group", "root:x:0:\n");
            from_host_error(
                &lodi::hostscope::apply(&root.options())
                    .expect_err("this process cannot set that owner"),
            )
        },
    },
    Case {
        code: "E_SYSTEM_BUSY",
        // The apply lock exists and cannot be opened. A real apply over a scratch root
        // (`lodi host apply --root <scratch>`, through the library entry point the command
        // calls), refused at step 6 of the gate, with nothing written.
        //
        // The other route to this code is a second apply while one holds the lock. Its message
        // names the command, and committing that sentence would put an unrooted host command
        // into a scanned file:
        // check-host-safety: refusal — what follows is a product message, not a command.
        // `another lodi host apply holds …`. `scripts/check-host-safety.py` is a gate step whose
        // whole job is that no committed file can run the host scope against this machine, and
        // a JSON fixture cannot carry the escape comment above. That route stays proven where
        // it already is, by `tests/host_safety.rs`'s
        // `a_second_apply_while_one_holds_the_lock_is_refused` (real contention, no sleep).
        run: || {
            let root = hostroot::Root::new("cat-busy");
            let (uid, gid) = hostroot::ids(&root);
            root.debian_with(&format!(
                "[files.\"/etc/x\"]\ncontent = \"x\\n\"\nowner = \"{uid}\"\ngroup = \"{gid}\"\n"
            ));
            root.write("/var/lib/lodi/host/.lock", "");
            root.chmod("/var/lib/lodi/host/.lock", 0o000);
            from_host_error(&lodi::hostscope::apply(&root.options()).expect_err("the apply"))
        },
    },
    // ----------------------------------------------- the home scope refuses (exit 8) ---
    Case {
        code: "E_DRIFT",
        // The whole home scope through the built binary, in a scratch HOME that
        // `tests/support/mod.rs` proves is not the ambient one.
        run: || {
            let env = support::home_env("cat-drift");
            let dir = env.config().join("lodi");
            fs::create_dir_all(&dir).expect("the config directory");
            fs::write(
                dir.join("home.toml"),
                "[home]\nversion = \"1\"\n\n[home.file.\".inputrc\"]\n\
                 text = \"set editing-mode vi\\n\"\n",
            )
            .expect("the home manifest");
            let first = env
                .lodi(&["switch", "--home"])
                .output()
                .expect("the first apply runs");
            assert!(
                first.status.success(),
                "the first apply: {}",
                String::from_utf8_lossy(&first.stderr)
            );
            fs::write(env.home().join(".inputrc"), "edited by hand\n").expect("the hand edit");
            let out = env
                .lodi(&["switch", "--home"])
                .output()
                .expect("the second apply runs");
            from_output(&out)
        },
    },
    // ------------------------------------------------------------------ resolution (exit 4) ---
    Case {
        code: "E_NO_RECIPE",
        // The built binary: `lodi develop` locks a project naming a tool the catalogue has no
        // recipe for, and refuses before any request, so this is a whole command with its real
        // exit status and no network.
        run: || {
            let dir = scratch("no-recipe");
            fs::write(
                dir.join("lodi.toml"),
                "[project]\nname = \"x\"\n\n[tools]\npyton = \"3\"\n",
            )
            .expect("the manifest");
            let env = dir.join(".env");
            let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
                .args(["develop", "--trust", "--", "/bin/sh", "-c", ":"])
                .current_dir(&dir)
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", env.join("home"))
                .env("XDG_CONFIG_HOME", env.join("config"))
                .env("LODI_HOME", env.join("lodi-home"))
                .output()
                .expect("the lodi binary runs");
            let produced = from_output(&out);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_NO_MATCH",
        run: || {
            let fetcher = MapFetcher::new(&[(WIDGET_INDEX, b"9.8.7\n")]);
            from_diagnostic(&resolve_widget(
                &fetcher,
                "https://dl.invalid/widget-${version}",
                "2",
                "x86_64",
            ))
        },
    },
    Case {
        code: "E_UNSUPPORTED_ARCH",
        run: || {
            let fetcher = MapFetcher::new(&[]);
            from_diagnostic(&resolve_widget(
                &fetcher,
                "https://dl.invalid/widget-${version}",
                "latest",
                "aarch64",
            ))
        },
    },
    Case {
        code: "E_RECIPE_CTX",
        // What the store does with a locked tool whose recipe gave it no artifact.
        run: || {
            from_diagnostic(
                &lodi::store::artifacts_identity(&widget_entry(Vec::new()))
                    .expect_err("a tool with no artifact"),
            )
        },
    },
    Case {
        code: "E_RECIPE_INVALID",
        run: || {
            let text = "[recipe]\nname = \"widget\"\ndescription = \"a synthetic tool\"\n\
                        homepage = \"https://example.invalid/\"\n\n\
                        [versions]\nstrategy = \"text_index\"\n\
                        url = \"https://index.invalid/versions.txt\"\nline = \"${version}\"\n\n\
                        [asset]\nurl = \"https://dl.invalid/widget\"\nformat = \"binary\"\n\
                        checksum_sidecar = \".sha256\"\nexclude = [\"/absolute\"]\n\n\
                        [spec]\nbin = [\"bin/widget\"]\npath = []\n";
            from_diagnostic(
                &lodi::catalogue::parse_recipe(text, "widget.toml")
                    .expect_err("the recipe is inconsistent"),
            )
        },
    },
    Case {
        code: "E_REPO_UNREACHABLE",
        run: || {
            let ops = lodi::distro::Family::Apt
                .ops()
                .expect("this build carries the apt family");
            let repository = lodi::distro::Repository {
                name: "main".into(),
                url: "https://mirror.invalid/ubuntu/".into(),
                suites: vec!["noble".into()],
                components: vec!["main".into()],
            };
            from_diagnostic(
                &ops.fetch_index(
                    &MapFetcher::new(&[]),
                    &repository,
                    "amd64",
                    &mut lodi::distro::IndexPackages::default(),
                )
                .expect_err("the index is not published"),
            )
        },
    },
    Case {
        code: "E_SNAPSHOT_TOO_OLD",
        // Both snapshot checks run before any request, so the fetcher is never consulted.
        run: || {
            let definition = lodi::catalogue::builtin_base("arch")
                .expect("the arch base definition parses")
                .expect("this build carries it");
            let fetcher = MapFetcher::new(&[]);
            // M-Arch-Base T-6 unified the lock path: one `lock_base` serves every family, and
            // the arch definition reaches the pacman family through the seam.
            let error = lodi::debian::base::lock_base(
                &fetcher,
                &definition,
                &lodi::debian::base::BaseRequest {
                    release: "rolling",
                    arch: "x86_64",
                    snapshot: lodi::util::parse_utc("2024-04-30T23:59:59Z").expect("a timestamp"),
                    now: lodi::util::parse_utc("2026-09-21T00:00:00Z").expect("a timestamp"),
                    requested: &[],
                },
            )
            .expect_err("the snapshot is before the floor");
            assert!(fetcher.requests().is_empty(), "nothing was requested");
            from_diagnostic(&error)
        },
    },
    Case {
        code: "E_SNAPSHOT_FUTURE",
        run: || {
            let definition = lodi::catalogue::builtin_base("arch")
                .expect("the arch base definition parses")
                .expect("this build carries it");
            let fetcher = MapFetcher::new(&[]);
            let error = lodi::debian::base::lock_base(
                &fetcher,
                &definition,
                &lodi::debian::base::BaseRequest {
                    release: "rolling",
                    arch: "x86_64",
                    snapshot: lodi::util::parse_utc("2026-09-22T00:00:00Z").expect("a timestamp"),
                    now: lodi::util::parse_utc("2026-09-21T00:00:00Z").expect("a timestamp"),
                    requested: &[],
                },
            )
            .expect_err("the snapshot is in the future");
            assert!(fetcher.requests().is_empty(), "nothing was requested");
            from_diagnostic(&error)
        },
    },
    Case {
        code: "E_LOCK_VERSION",
        // The real lock parser, on a lock whose version this build does not read, and the
        // diagnostic the command builds from what it returned.
        run: || {
            let problem =
                lodi::lock::parse_lock(br#"{"version": 9}"#).expect_err("a newer lock version");
            from_diagnostic(&problem.diagnostic(lodi::lock::LOCK_FILE))
        },
    },
    // ---------------------------------------------------------- fetch and integrity (exit 5) ---
    Case {
        code: "E_BUILD_UNAVAILABLE",
        // A Fedora lock's build that its repository no longer serves: the image realization
        // `lodi develop` runs, against the stand-in podman, stops at that download (LD-435).
        run: || {
            let root = scratch("build-unavailable");
            let store =
                lodi::store::Store::open(&root.join("lodi-home")).expect("the scratch store");
            let podman =
                lodi::container::Podman::find(Some(podman_shim(&root.join("bin"), 0).as_os_str()))
                    .expect("the stand-in is found where the product looks");
            let rootfs = b"a synthetic rootfs, never imported by a real podman".to_vec();
            let url = "https://artifacts.test/base/rootfs.tar.gz";
            let mut base = offline_base(&rootfs, url);
            base.distro = "fedora".into();
            base.release = "44".into();
            base.repositories[0].url = "https://repo.test/fedora/".into();
            base.closure.packages = vec![lodi::lock::ClosureEntry {
                name: "jq".into(),
                version: "1.8.1".into(),
                arch: Some("x86_64".into()),
                origin: "install".into(),
                sha256: Some(format!("sha256:{}", "0".repeat(64))),
                filename: Some("Packages/j/jq-1.8.1-2.fc44.x86_64.rpm".into()),
                size: Some(1),
                repository: Some(base.repositories[0].name.clone()),
                suite: Some(String::new()),
                epoch: Some(0),
                release: Some("2.fc44".into()),
            }];
            let error = lodi::container::realize_image(
                &store,
                &podman,
                &MapFetcher::new(&[(url, &rootfs)]),
                &base,
                &mut lodi::store::Report::default(),
            )
            .expect_err("the locked build is not served");
            let produced = from_diagnostic(&error);
            support::make_writable(&root);
            fs::remove_dir_all(&root).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_FETCH",
        run: || {
            let fetcher = MapFetcher::new(&[]);
            from_diagnostic(&resolve_widget(
                &fetcher,
                "https://dl.invalid/widget-${version}",
                "latest",
                "x86_64",
            ))
        },
    },
    Case {
        code: "E_INSECURE_URL",
        // The resolver refuses the URL before it is handed to any transport: the fetcher is
        // asked for the index and never for the asset.
        run: || {
            let fetcher = MapFetcher::new(&[(WIDGET_INDEX, b"9.8.7\n")]);
            let error = resolve_widget(
                &fetcher,
                "http://dl.invalid/widget-${version}",
                "latest",
                "x86_64",
            );
            assert_eq!(
                fetcher.requests(),
                vec![WIDGET_INDEX.to_string()],
                "no connection was opened to the insecure URL"
            );
            from_diagnostic(&error)
        },
    },
    Case {
        code: "E_NO_CHECKSUM",
        run: || {
            let fetcher = MapFetcher::new(&[(WIDGET_INDEX, b"9.8.7\n")]);
            from_diagnostic(&resolve_widget(
                &fetcher,
                "https://dl.invalid/widget-${version}",
                "latest",
                "x86_64",
            ))
        },
    },
    Case {
        code: "E_HASH_MISMATCH",
        // A pinned package index whose bytes are not the ones the pin records.
        run: || {
            let url = "https://archive.invalid/core.db";
            let body = b"not a pacman database".to_vec();
            from_diagnostic(
                &lodi::arch::base::verify_index(
                    &MapFetcher::new(&[(url, &body)]),
                    "core",
                    url,
                    &"0".repeat(64),
                )
                .expect_err("the bytes are not the pin"),
            )
        },
    },
    Case {
        code: "E_LOCK_STALE",
        run: || {
            from_diagnostic(
                &lodi::lock::LockProblem::Stale(vec!["python 3.12".into(), "base bookworm".into()])
                    .diagnostic(lodi::lock::LOCK_FILE),
            )
        },
    },
    // ---------------------------------------------------------- build and store (exit 6) ---
    Case {
        code: "E_ARCHIVE_FORMAT",
        run: || {
            let dir = scratch("archive-format");
            let archive = dir.join("artifact");
            fs::write(&archive, b"").expect("the artifact file");
            let error = lodi::archive::extract(
                &archive,
                "zip",
                0,
                "",
                &dir.join("out"),
                &lodi::archive::Limits::default(),
            )
            .expect_err("this build carries no zip decoder");
            let produced = from_diagnostic(&error);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_ARCHIVE_UNSAFE",
        run: || {
            let dir = scratch("archive-unsafe");
            let archive = dir.join("artifact");
            fs::write(
                &archive,
                support::gzip(&support::tar(&[support::file("../evil", "x", 0o644)])),
            )
            .expect("the hostile archive");
            let error = lodi::archive::extract(
                &archive,
                "tar.gz",
                0,
                "",
                &dir.join("out"),
                &lodi::archive::Limits::default(),
            )
            .expect_err("the entry leaves its destination");
            let produced = from_diagnostic(&error);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_ARCHIVE_EMPTY",
        run: || {
            let dir = scratch("archive-empty");
            let archive = dir.join("artifact");
            fs::write(
                &archive,
                support::gzip(&support::tar(&[support::file("top", "x", 0o644)])),
            )
            .expect("the archive");
            let error = lodi::archive::extract(
                &archive,
                "tar.gz",
                1,
                "",
                &dir.join("out"),
                &lodi::archive::Limits::default(),
            )
            .expect_err("nothing is left after strip_components");
            let produced = from_diagnostic(&error);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_BIN_MISSING",
        // The store realizing a locked tool whose recipe promises an executable the artifact
        // does not carry. The artifact is synthetic and the fetcher is in-process.
        run: || {
            let dir = scratch("bin-missing");
            let store = lodi::store::Store::open(&dir).expect("the scratch store");
            let mut tool = support::Tool::python("3.12.14");
            tool.bin.push("bin/python3-config".into());
            let fetcher = MapFetcher::new(&[(tool.url.as_str(), &tool.bytes)]);
            let error = store
                .realize_tool(
                    &fetcher,
                    &tool.entry(),
                    &mut lodi::store::Report::default(),
                    None,
                )
                .expect_err("the promised binary is not in the artifact");
            let produced = from_diagnostic(&error);
            support::make_writable(&dir);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_TREE_UNSUPPORTED",
        // A real FIFO in a tree being hashed into the store: the NAR serialization has no way
        // to represent it, and the publication is refused rather than skipping the member.
        run: || {
            let dir = scratch("tree-unsupported");
            let store = lodi::store::Store::open(&dir).expect("the scratch store");
            let hook: lodi::store::Hook = &no_hook;
            let error = store
                .publish(
                    lodi::store::Entry {
                        name: "art-a-fifo".into(),
                        kind: "art",
                        identity: format!("sha256:{}", "0".repeat(64)),
                        references: Vec::new(),
                    },
                    &mut lodi::store::Report::default(),
                    hook,
                    |staging, _report, _hook| {
                        fs::create_dir_all(staging).expect("the staging directory");
                        let pipe = staging.join("pipe");
                        let c = std::ffi::CString::new(pipe.to_str().expect("a path")).unwrap();
                        // SAFETY: `c` is a valid NUL-terminated path in a directory this
                        // closure just created below the scratch store.
                        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
                        assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
                        Ok(())
                    },
                )
                .expect_err("a special file cannot be published");
            let produced = from_diagnostic(&error);
            support::make_writable(&dir);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    Case {
        code: "E_LAYER_BUILD",
        run: || realize_with_shim("layer-build", 125),
    },
    Case {
        code: "E_CLOSURE_DRIFT",
        run: || realize_with_shim("closure-drift", 0),
    },
    Case {
        code: "E_PIN_UNSATISFIABLE",
        // A real host apply by the built binary, over a scratch root and the fake package
        // manager (M-Pin, P7):
        // the config's `lodi.lock` records tzdata at a version the recorded dated Debian archive
        // does not serve, and the preflight's simulation refuses it before the journal exists. Nothing is fetched: the pin is recorded, and the archive is the
        // loopback fixture the fake's private update reads.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-pin-unsatisfiable", Machine::debian());
            let fixtures =
                repo().join("tests/fixtures/host/pin/archive/snapshot.debian.org/archive");
            for repository in ["debian", "debian-security"] {
                case.archive(
                    &format!("https://snapshot.debian.org/archive/{repository}/20250301T000000Z/"),
                    &fixtures.join(repository).join("20250301T000000Z"),
                );
            }
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"debian\"\npackages = \"managed\"\n\n\
                 [packages]\ncommon = [\"tzdata\"]\n\n[packages.pin]\ntzdata = \"2025-03-01\"\n",
            );
            let mut pins = std::collections::BTreeMap::new();
            pins.insert(
                "tzdata".to_string(),
                lodi::hostscope::pin::PinRecord {
                    policy: "date".into(),
                    requested: "2025-03-01".into(),
                    snapshot: Some("2025-03-01T00:00:00Z".into()),
                    version: "2024b-0+deb12u9".into(),
                    sha256: format!("sha256:{}", "0".repeat(64)),
                    filename: "pool/main/t/tzdata/tzdata_2024b-0+deb12u9_all.deb".into(),
                    repository: "debian".into(),
                    epoch: None,
                    release: None,
                    arch: None,
                    source: None,
                },
            );
            let mut lock = lodi::config::lock::Lock::default();
            lock.hosts.insert(
                "host.toml".to_string(),
                lodi::config::lock::HostSection {
                    distro: "debian".into(),
                    release: "bookworm".into(),
                    arch: "x86_64".into(),
                    snapshot: None,
                    pins,
                    keys: std::collections::BTreeMap::new(),
                },
            );
            case.write_beside("lodi.lock", lock.render());
            from_output(&case.apply(&[]))
        },
    },
    Case {
        code: "E_PIN_UNAVAILABLE",
        // A real host apply by the built binary over a scratch root and the fake dnf5 (fk-1):
        // the config's lock records pv's older Fedora 44 build, no configured repository serves
        // it, and Koji's copy is gone from the loopback archive. The preflight stops before any
        // change.
        run: || {
            use fakehost::{Case, Machine};
            let case = Case::new("errors-pin-unavailable", Machine::fedora());
            case.edit(|m| {
                m["installed"]["pv"] = serde_json::json!({"version": "0:1.10.5-1.fc44"});
                m["available"]["pv"] =
                    serde_json::json!({"version": "0:1.10.5-1.fc44", "repo": "updates"});
            });
            let build = "0:1.10.4-1.fc44.x86_64";
            case.set_manifest(&format!(
                "[host]\nversion = \"1\"\ndistro = \"fedora\"\n\n[packages.fedora]\n\
                 add = [\"pv\"]\n\n[packages.fedora.pin]\npv = \"{build}\"\n"
            ));
            let mut pins = std::collections::BTreeMap::new();
            pins.insert(
                "pv".to_string(),
                lodi::hostscope::pin::PinRecord {
                    policy: "version".into(),
                    requested: build.into(),
                    snapshot: None,
                    version: "1.10.4".into(),
                    sha256: format!("sha256:{}", "4".repeat(64)),
                    filename: "pv-1.10.4-1.fc44.x86_64.rpm".into(),
                    repository: "fedora".into(),
                    epoch: Some("0".into()),
                    release: Some("1.fc44".into()),
                    arch: Some("x86_64".into()),
                    source: Some(
                        "https://kojipkgs.fedoraproject.org/packages/pv/1.10.4/1.fc44/data/\
                         signed/6d9f90a6/x86_64/pv-1.10.4-1.fc44.x86_64.rpm"
                            .into(),
                    ),
                },
            );
            let mut lock = lodi::config::lock::Lock::default();
            lock.hosts.insert(
                "host.toml".to_string(),
                lodi::config::lock::HostSection {
                    distro: "fedora".into(),
                    release: "44".into(),
                    arch: "x86_64".into(),
                    snapshot: None,
                    pins,
                    keys: std::collections::BTreeMap::new(),
                },
            );
            case.write_beside("lodi.lock", lock.render());
            let server = support::Server::start(std::collections::BTreeMap::new());
            from_output(&case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]))
        },
    },
    Case {
        code: "E_PIN_UNTRUSTED",
        // A real host apply by the built binary over a scratch root and the fake pacman (#170):
        // the measured `tree` of 2026-09-01 and its real signature, served on loopback, and a
        // machine keyring that lacks the signer. The preflight `-Up` refuses it.
        run: || {
            use fakehost::{Case, Machine, Pkg};
            let dated = "https://archive.archlinux.org/repos/2026/09/01/";
            let folder = repo()
                .join("tests/fixtures/host/pin/archive/archive.archlinux.org/repos/2026/09/01");
            let tree = "extra/os/x86_64/tree-2.3.2-1-x86_64.pkg.tar.zst";
            let mut urls = std::collections::BTreeMap::new();
            for path in [
                "core/os/x86_64/core.db",
                "extra/os/x86_64/extra.db",
                tree,
                &format!("{tree}.sig"),
            ] {
                urls.insert(
                    format!("{dated}{path}"),
                    fs::read(folder.join(path)).unwrap(),
                );
            }
            let server = support::Server::start(urls);
            let case = Case::new(
                "errors-pin-untrusted",
                Machine::arch().offering(Pkg::new("tree")),
            );
            for repository in ["core", "extra"] {
                case.archive(
                    &format!("{dated}{repository}/os/x86_64/"),
                    &folder.join(repository).join("os/x86_64"),
                );
            }
            case.root.write("etc/pacman.d/gnupg/pubring.kbx", "");
            case.edit(|m| m["keyring"] = true.into());
            case.set_manifest(
                "[host]\nversion = \"1\"\ndistro = \"arch\"\n\n[packages]\ncommon = [\"tree\"]\n\n\
                 [packages.pin]\ntree = \"2026-09-01\"\n",
            );
            from_output(&case.verb_env("apply", &[], &[("LODI_FETCH_REWRITE", &server.rewrite())]))
        },
    },
    // ------------------------------------------------------------------- runtime (exit 7) ---
    Case {
        code: "E_NO_RUNTIME",
        run: || {
            from_diagnostic(
                &lodi::container::Podman::find(None).expect_err("there is no podman to find"),
            )
        },
    },
    Case {
        code: "E_ENTER",
        // The argv builder the runtime uses, refusing a project path Podman's bind syntax
        // cannot express. No container is started.
        run: || {
            let spec = lodi::container::RunSpec {
                name: "lodi-widget-0".into(),
                image: "localhost/lodi-env:0".into(),
                hostname: "widget".into(),
                project_root: PathBuf::from("/projects/a:b"),
                home: None,
                store: PathBuf::from("/store"),
                env: BTreeMap::new(),
                tty: false,
            };
            from_diagnostic(
                &lodi::container::run_arguments(&spec, &[]).expect_err("the path cannot be bound"),
            )
        },
    },
    Case {
        code: "E_SHELL_NOT_FOUND",
        run: || {
            from_diagnostic(
                &lodi::shell::shell_program(None, Path::new("/nonexistent/sh"))
                    .expect_err("there is no shell to run"),
            )
        },
    },
    Case {
        code: "E_NESTED",
        // The nesting decision, on the environment map a command hands it.
        run: || {
            let activation = lodi::host::Activation {
                envhash: format!("sha256:{}", "a".repeat(64)),
                project_root: PathBuf::from("/projects/widget"),
                profile: lodi::host::PROFILE.into(),
                runtime: lodi::host::RUNTIME.into(),
            };
            let env: BTreeMap<std::ffi::OsString, std::ffi::OsString> = [
                (
                    std::ffi::OsString::from("LODI_MODE"),
                    std::ffi::OsString::from("container"),
                ),
                (
                    std::ffi::OsString::from("LODI_ACTIVATION"),
                    std::ffi::OsString::from(format!("sha256:{}", "c".repeat(64))),
                ),
            ]
            .into_iter()
            .collect();
            from_diagnostic(
                &lodi::host::nesting(&env, &activation, false)
                    .expect_err("the runtimes are not the same"),
            )
        },
    },
    // ------------------------------------------------------------- permission (exit 9) ---
    Case {
        code: "E_STORE_PERM",
        run: || {
            from_diagnostic(
                &lodi::store::Store::open(Path::new("relative/store"))
                    .expect_err("a store location that is not absolute"),
            )
        },
    },
    // ------------------------------------------------------------------ trust (exit 11) ---
    Case {
        code: "E_TRUST_REQUIRED",
        run: || {
            let dir = scratch("trust");
            let text = fs::read_to_string(repo().join("tests/fixtures/init/default.toml"))
                .expect("the init fixture")
                + "\n[tasks.versions]\nrun = \"python3 --version\"\n";
            fs::write(dir.join("lodi.toml"), &text).expect("the manifest");
            let manifest = parse_project_manifest(&text, "lodi.toml").expect("it parses");
            let options = lodi::host::Options {
                no_nest: false,
                trust_once: false,
                interactive: false,
            };
            let failure = lodi::host::trust_gate(&dir, &manifest, &options)
                .expect_err("the task text is not trusted");
            let produced = from_diagnostic(&failure.diagnostics[0]);
            fs::remove_dir_all(&dir).expect("the scratch directory is removed");
            produced
        },
    },
    // --------------------------------------------------- warnings (exit status unchanged) ---
    Case {
        code: "W_SHADOWED",
        // Plan, apply and status over a scratch HOME in which `~/.shaderc` shadows what
        // `[programs.shade]` renders: each succeeds — the warning changes no exit status — and
        // each prints the same line.
        run: || {
            let env = support::home_env("cat-shadowed");
            let dir = env.config().join("lodi");
            fs::create_dir_all(&dir).expect("the config directory");
            fs::write(
                dir.join("home.toml"),
                "[home]\nversion = \"1\"\n\n[programs.shade]\n",
            )
            .expect("the home manifest");
            fs::write(env.home().join(".shaderc"), "set by hand\n").expect("the shadowing file");
            let store = env.data().join("lodi");
            let roots = lodi::roots::Roots::from_vars(
                Some(env.home().as_os_str()),
                Some(env.config().as_os_str()),
                None,
                Some(store.as_os_str()),
            )
            .expect("the scratch roots");
            let plan = lodi::home::plan::plan_with(&roots, SHADE)
                .unwrap_or_else(|e| panic!("the plan fails: {e}"));
            let options = lodi::home::apply::Options::default();
            let applied =
                lodi::home::apply::apply_with(&roots, &options, &MapFetcher::new(&[]), 0, SHADE)
                    .unwrap_or_else(|f| panic!("the apply fails: {}", f.text));
            let status = lodi::home::apply::status_with(&roots, SHADE)
                .unwrap_or_else(|f| panic!("the status fails: {}", f.text));
            // `shade` is on no `PATH`, so a `W_PROGRAM_NOT_FOUND` line comes first (LD-337).
            let shadowed = plan
                .warnings
                .iter()
                .filter(|w| w.contains("W_SHADOWED"))
                .count();
            assert_eq!(shadowed, 1, "{:?}", plan.warnings);
            assert_eq!(applied.warnings, plan.warnings);
            assert_eq!(status.warnings, plan.warnings);
            Produced {
                stderr: sanitize(&format!("{}\n", plan.warnings.join("\n"))),
                exit: None,
            }
        },
    },
    Case {
        code: "W_PROGRAM_NOT_FOUND",
        // `lodi switch --home --dry-run` (1.x's `lodi home plan`, #699) for a `[programs.git]`
        // table, with a `PATH` that holds no `git`: the warning is printed and the preview
        // exits 0 (LD-337).
        run: || {
            let env = support::home_env("cat-program-not-found");
            let dir = env.config().join("lodi");
            fs::create_dir_all(&dir).expect("the config directory");
            fs::write(
                dir.join("home.toml"),
                "[home]\nversion = \"1\"\n\n[programs.git]\ninit.defaultBranch = \"main\"\n",
            )
            .expect("the home manifest");
            let empty = env.root().join("empty-path");
            fs::create_dir_all(&empty).expect("an empty PATH directory");
            let out = env
                .lodi(&["switch", "--home", "--dry-run"])
                .env("PATH", &empty)
                .output()
                .expect("the lodi binary runs");
            from_output(&out)
        },
    },
];

/// LD-401: the causes a host `SOURCE` URL adds to rows that already exist, each produced by the
/// real binary (`lodi switch` and `lodi import`, LD-522) before any request, with its row's exit
/// status. No code is new.
#[test]
fn a_url_source_adds_causes_to_existing_codes() {
    prime();
    let dir = scratch("url-causes");
    let root = dir.join("root").display().to_string();
    let rows = catalogue_rows(&repo().join("docs/ERRORS.md"));
    let doc = fs::read_to_string(repo().join("docs/ERRORS.md")).expect("the catalogue");
    // The machine is named, so the URL is what is refused (a nameless root stops earlier).
    fs::create_dir_all(dir.join("root/etc")).expect("the scratch root");
    fs::write(dir.join("root/etc/hostname"), "box\n").expect("its hostname");
    let dry = ["switch", "--host", "--dry-run"];
    for (args, code) in [
        (
            [&dry[..], &["git+ssh://git.example.test/hosts.git"]].concat(),
            "E_INSECURE_URL",
        ),
        (
            [&dry[..], &["git@localhost:hosts.git"]].concat(),
            "E_INSECURE_URL",
        ),
        (
            vec![
                "switch",
                "--host",
                "git+https://git.example.test/hosts.git?ref=main",
            ],
            "E_UNSUPPORTED",
        ),
        (
            [&dry[..], &["github:owner/hosts/box"]].concat(),
            "E_UNSUPPORTED",
        ),
        (
            vec!["import", "--yes", "codeberg:owner/hosts"],
            "E_UNSUPPORTED",
        ),
    ] {
        // check-host-safety: refusal — the root is a scratch path below this test's directory.
        let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(&args)
            .args(["--root", &root])
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_ATTEMPTS", "1")
            // Refused before any request; were one made, it would find nothing on loopback.
            .env("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/")
            .env("HOME", dir.join("home"))
            .env("XDG_CONFIG_HOME", dir.join("home/.config"))
            .env("XDG_STATE_HOME", dir.join("home/.local/state"))
            .env_remove("LODI_REPO")
            .output()
            .expect("the lodi binary runs");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains(code), "{args:?}: {stderr}");
        assert_eq!(
            out.status.code(),
            Some(i32::from(rows[code].exit)),
            "{args:?}"
        );
        assert_eq!(rows[code].exit, diag::exit_status(code), "{code}");
        let documented = format!("{}\n{}", rows[code].cause, situations(&doc, code));
        assert!(documented.contains("git+https://"), "{code}: no URL cause");
    }
    let _ = fs::remove_dir_all(&dir);
}
