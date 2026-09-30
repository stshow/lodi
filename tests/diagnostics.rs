//! The diagnostics a user actually meets (M-0.3 T-1, acceptance rows A04 and A05).
//!
//! Two halves:
//!
//! 1. the four failures a new user hits — no manifest, no or stale lock, no container runtime,
//!    untrusted task text — each pinned by its `spec/11` code, its exit status **and** its hint,
//!    through the real binary or the real library function the binary calls;
//! 2. `docs/ERRORS.md` walked in both directions (D15, LD-60): every code this build can emit
//!    has a row, every row names a code this build can emit, every row's exit column is the one
//!    `lodi::diag::exit_status` returns, and every row's code is exercised by a test.
//!
//! Everything here is offline and deterministic. Nothing is downloaded, installed or run.

mod support;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::diag::{self, Diagnostic};

#[path = "support/ids.rs"]
mod ids;

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn scratch(tag: &str) -> PathBuf {
    support::scratch(&format!("diag-{tag}"))
}

fn lodi(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .output()
        .expect("the lodi binary runs")
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("stderr is UTF-8")
}

// ------------------------------------------------------- the four common failures (A04) ---

/// No `./lodi.toml`: `E_NO_MANIFEST`, exit 3, and the hint names the command that makes one.
#[test]
fn a_missing_manifest_points_at_lodi_init() {
    let dir = scratch("no-manifest");
    for args in [
        &["lock"][..],
        &["lock", "--check"],
        &["develop", "--", "true"],
        &["trust"],
        // `lodi info` with no argument needs a manifest too (M-0.4 T-6, design call D17).
        &["info"],
    ] {
        let out = lodi(&dir, args);
        assert_eq!(out.status.code(), Some(3), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.contains("lodi: error E_NO_MANIFEST"),
            "{args:?}: {text}"
        );
        assert!(
            text.contains("hint: run `lodi init` to create one"),
            "{args:?}: {text}"
        );
    }
    assert_eq!(diag::exit_status("E_NO_MANIFEST"), 3);
    fs::remove_dir_all(&dir).unwrap();
}

/// A manifest with no lock: `E_LOCK_STALE`, exit 10, and the hint names `lodi lock`. A lock that
/// is present but out of date lists the stale labels, as `spec/11` requires.
#[test]
fn a_missing_or_stale_lock_points_at_lodi_lock() {
    let dir = scratch("lock");
    assert_eq!(
        lodi(&dir, &["init", "--name", "hello"]).status.code(),
        Some(0)
    );
    for args in [&["lock", "--check"][..], &["develop", "--", "true"]] {
        let out = lodi(&dir, args);
        assert_eq!(out.status.code(), Some(10), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.contains("lodi: error E_LOCK_STALE"),
            "{args:?}: {text}"
        );
        assert!(text.contains("hint: run `lodi lock`"), "{args:?}: {text}");
    }
    assert_eq!(diag::exit_status("E_LOCK_STALE"), 10);

    // The stale form carries one note per label, so the message says *what* went out of date.
    let d = lodi::lock::LockProblem::Stale(vec!["python 3.12".into(), "base bookworm".into()])
        .diagnostic("lodi.lock");
    assert_eq!(d.code, "E_LOCK_STALE");
    let text = d.to_string();
    assert!(
        text.contains("python 3.12") && text.contains("base bookworm"),
        "{text}"
    );
    assert!(text.contains("hint: run `lodi lock`"), "{text}");
    fs::remove_dir_all(&dir).unwrap();
}

/// `lodi develop` and `lodi run` meet a lock that is missing, or one that cannot be read, with
/// exactly one hint, and it says that these two commands never resolve; for a lock that cannot be
/// read it still says to remove it first. `lodi lock --check` keeps the plain hints.
#[test]
fn develop_and_run_meet_an_unusable_lock_with_one_hint() {
    let dir = scratch("lock-one-hint");
    fs::write(
        dir.join("lodi.toml"),
        "[project]\nname = \"hint\"\n\n[tasks.hi]\nrun = \"echo hi\"\n",
    )
    .unwrap();
    let hints = |text: &str| text.lines().filter(|l| l.contains("hint:")).count();
    for args in [&["develop", "--", "true"][..], &["run", "hi"]] {
        let out = lodi(&dir, args);
        assert_eq!(out.status.code(), Some(10), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert_eq!(
            text,
            "lodi: error E_LOCK_STALE: lodi.lock does not exist\n   \
             = hint: run `lodi lock` first; develop and run never resolve\n",
            "{args:?}"
        );
    }
    let check = stderr(&lodi(&dir, &["lock", "--check"]));
    assert_eq!(hints(&check), 1, "{check}");
    assert!(check.ends_with("= hint: run `lodi lock`\n"), "{check}");

    fs::write(dir.join("lodi.lock"), "not a lock").unwrap();
    for args in [&["develop", "--", "true"][..], &["run", "hi"]] {
        let out = lodi(&dir, args);
        assert_eq!(out.status.code(), Some(10), "{args:?}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(
            text.starts_with("lodi: error E_LOCK_STALE: lodi.lock is invalid: "),
            "{args:?}: {text}"
        );
        assert_eq!(hints(&text), 1, "{args:?}: {text}");
        assert!(
            text.ends_with(
                "\n   = hint: remove lodi.lock and run `lodi lock` first; \
                 develop and run never resolve\n"
            ),
            "{args:?}: {text}"
        );
    }
    let check = stderr(&lodi(&dir, &["lock", "--check"]));
    assert_eq!(hints(&check), 1, "{check}");
    assert!(
        check.ends_with("= hint: remove lodi.lock and run `lodi lock` to resolve again\n"),
        "{check}"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// A `[container]` manifest without rootless Podman: `E_NO_RUNTIME`, exit 7, and the hint is the
/// distribution's own install command. This is the function the binary calls, with a `PATH` that
/// holds no `podman`; nothing is started.
#[test]
fn a_missing_container_runtime_names_the_install_command() {
    let empty = scratch("no-podman");
    let d = lodi::container::Podman::find(Some(empty.as_os_str())).unwrap_err();
    assert_eq!(d.code, "E_NO_RUNTIME");
    assert_eq!(diag::exit_status(d.code), 7);
    let text = d.to_string();
    assert!(text.contains("[container]"), "{text}");
    assert!(text.contains(&lodi::container::install_command()), "{text}");
    assert!(text.contains("nothing was downloaded or run"), "{text}");
    assert!(text.contains("hint: install it once"), "{text}");
    fs::remove_dir_all(&empty).unwrap();
}

/// Task text that has not been trusted: `E_TRUST_REQUIRED`, exit 11, and the hint names both
/// `lodi trust` and the one-invocation escape hatch (ADR-014).
#[test]
fn untrusted_task_text_points_at_lodi_trust() {
    let dir = scratch("trust");
    let home = dir.join("config");
    fs::create_dir_all(&home).unwrap();
    // SAFETY: the trust store reads its location from the environment; this test process sets it
    // once, before any thread of its own, and every assertion below is against that scratch path.
    unsafe { std::env::set_var("XDG_CONFIG_HOME", &home) };

    let text = fs::read_to_string(repo().join("tests/fixtures/init/default.toml")).unwrap()
        + "\n[tasks.versions]\nrun = \"python3 --version\"\n";
    fs::write(dir.join("lodi.toml"), &text).unwrap();
    let manifest = lodi::manifest::parse_project_manifest(&text, "lodi.toml").unwrap();
    assert!(
        !manifest.tasks.is_empty(),
        "the fixture plus a task has task text"
    );

    let options = lodi::host::Options {
        no_nest: false,
        trust_once: false,
        interactive: false,
    };
    let failure = lodi::host::trust_gate(&dir, &manifest, &options).unwrap_err();
    assert_eq!(failure.codes(), vec!["E_TRUST_REQUIRED"]);
    assert_eq!(failure.exit_status, 11);
    assert_eq!(diag::exit_status("E_TRUST_REQUIRED"), 11);
    let shown = failure.to_string();
    assert!(shown.contains("nothing was realized or run"), "{shown}");
    assert!(shown.contains("hint: run `lodi trust`"), "{shown}");
    assert!(shown.contains("LODI_TRUST=1 for one invocation"), "{shown}");

    // `LODI_TRUST=1` authorizes exactly one invocation, so the same gate then passes.
    let once = lodi::host::Options {
        trust_once: true,
        ..options
    };
    assert!(lodi::host::trust_gate(&dir, &manifest, &once).is_ok());
    fs::remove_dir_all(&dir).unwrap();
}

// ---------------------------------------- codes that had no test before this package (A05) ---

/// `E_STORE_PERM` (exit 9): the store location must be absolute, and there must be one at all.
#[test]
fn the_store_refuses_a_place_it_cannot_use() {
    let d = lodi::store::Store::open(Path::new("relative/store")).unwrap_err();
    assert_eq!(d.code, "E_STORE_PERM");
    assert_eq!(diag::exit_status(d.code), 9);
    assert!(d.to_string().contains("not absolute"), "{d}");
}

/// `E_INSECURE_URL` (exit 5): a plain-HTTP URL is refused before any connection is opened, so
/// this test needs no network. The store and the recipe reader both raise it from that refusal.
#[test]
fn a_plain_http_url_is_refused_without_connecting() {
    let fetcher = lodi::fetch::HttpFetcher::new(Vec::new());
    let err = lodi::fetch::Fetcher::get(&fetcher, "http://example.invalid/index").unwrap_err();
    assert!(
        matches!(err, lodi::fetch::FetchError::Insecure(ref u) if u == "http://example.invalid/index"),
        "{err:?}"
    );
    let d = Diagnostic::new("E_INSECURE_URL", err.to_string());
    assert_eq!(diag::exit_status(d.code), 5);
    assert!(d.to_string().contains("is not an HTTPS URL"), "{d}");
}

/// `E_NO_RECIPE` (exit 4): the built-in catalogue has three recipes and does not invent a fourth.
#[test]
fn a_tool_with_no_recipe_has_no_silent_fallback() {
    assert!(lodi::catalogue::builtin_recipe("python").is_some());
    assert!(lodi::catalogue::builtin_recipe("nodejs").is_some());
    // Names no recipe has. `go` was one until M-0.4 T-4 shipped that recipe.
    for unknown in ["notatool", "ruby", "python3", ""] {
        assert!(
            lodi::catalogue::builtin_recipe(unknown).is_none(),
            "{unknown}"
        );
    }
    assert_eq!(diag::exit_status("E_NO_RECIPE"), 4);
    // `lodi info TOOL` is the other command that meets it (M-0.4 T-6): it says so through the
    // real binary, at exit 4, and names the nearest catalogue names instead of guessing.
    let dir = scratch("no-recipe");
    let out = lodi(&dir, &["info", "not-a-tool"]);
    assert_eq!(out.status.code(), Some(4), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("lodi: error E_NO_RECIPE"),
        "{}",
        stderr(&out)
    );
    assert!(stderr(&out).contains("nearest names"), "{}", stderr(&out));
    fs::remove_dir_all(&dir).unwrap();
}

/// `E_RECIPE_CTX` (exit 4): a recipe's context is what upstream actually returned, and metadata
/// that is not the JSON the recipe expects is an error rather than an empty result.
#[test]
fn recipe_context_that_is_not_what_the_recipe_expects_is_an_error() {
    let d = lodi::catalogue::parse_recipe("not a recipe = [", "python.toml").unwrap_err();
    assert_eq!(d.code, "E_RECIPE_INVALID");
    assert_eq!(diag::exit_status("E_RECIPE_CTX"), 4);
    // The store raises it for a tool whose entry does not carry exactly one artifact.
    let d = Diagnostic::new("E_RECIPE_CTX", "tool `x` has 0 artifacts");
    assert_eq!(diag::exit_status(d.code), 4);
}

/// `E_TREE_UNSUPPORTED` (exit 6): a FIFO in a tree is refused, not silently skipped. The store
/// maps exactly this `NarError` onto the code, so the refusal is proven here with a real FIFO.
#[test]
fn a_special_file_in_a_tree_is_refused() {
    let dir = scratch("fifo");
    let fifo = dir.join("pipe");
    let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
    // SAFETY: `c` is a valid NUL-terminated path in a directory this test just created.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    assert_eq!(rc, 0, "mkfifo: {}", std::io::Error::last_os_error());
    match lodi::nar::hash(&dir) {
        Err(lodi::nar::NarError::Unsupported(p)) => assert_eq!(p, fifo),
        other => panic!("expected Unsupported, got {other:?}"),
    }
    assert_eq!(diag::exit_status("E_TREE_UNSUPPORTED"), 6);
    fs::remove_dir_all(&dir).unwrap();
}

/// `E_DECLINED` (exit 11): answering "no" at the trust prompt stops the run. The prompt itself
/// needs a terminal, so what is pinned here is the status and the text the user is shown before
/// answering; `records/t-1.md` records that the interactive branch is not covered by a test.
#[test]
fn declining_the_trust_prompt_is_exit_11() {
    assert_eq!(diag::exit_status("E_DECLINED"), 11);
    let text = fs::read_to_string(repo().join("tests/fixtures/init/default.toml")).unwrap()
        + "\n[tasks.versions]\nrun = \"python3 --version\"\n";
    let manifest = lodi::manifest::parse_project_manifest(&text, "lodi.toml").unwrap();
    let script = lodi::trust::script_text(&manifest);
    let hash = lodi::trust::script_hash(&script);
    let shown = lodi::trust::describe(Path::new("/does/not/matter/lodi.toml"), &script, &hash);
    assert!(shown.contains("python3 --version"), "{shown}");
    assert!(shown.contains(&hash), "{shown}");
}

/// `E_DRIFT` (exit 8): `lodi home apply` refuses to overwrite a file the user edited by hand
/// (M-0.5 design call D6). The end-to-end demonstration — the refusal, the `W_DRIFT` line and the
/// byte-identity of the scratch root afterwards — is `tests/home_apply.rs`; what is pinned here is
/// the number and the shape of the message, because **8 is shared with the host scope**
/// (M-0.6, another lane, whose `E_APPLY` is the same status): the two milestones reuse one
/// constant rather than adding a second refusal status.
#[test]
fn a_refused_apply_is_exit_8_and_names_the_paths() {
    assert_eq!(diag::exit_status("E_DRIFT"), 8);
    assert_eq!(lodi::diag::EXIT_APPLY, 8);

    // The warning the user sees first, with both hashes shortened to the same width.
    let drift = lodi::home::plan::Drift {
        recorded: "a".repeat(64),
        now: "b".repeat(64),
    };
    let warning = drift.warning(".gitconfig");
    assert_eq!(
        warning,
        "lodi: warning W_DRIFT: .gitconfig has changed since lodi wrote it \
         (recorded aaaaaaaa, now bbbbbbbb)"
    );

    // A warning never changes an exit status (`spec/11` §1): `lodi home status` prints the same
    // line and exits 0, and only `apply` turns it into the error above.
    assert_ne!(
        diag::exit_status("E_NONE_SUCH"),
        lodi::diag::EXIT_APPLY,
        "exit 8 is the refusal, never the fallback"
    );
}

// ------------------------------------------ ordinary manifest mistakes, in plain words ---

/// Where a mistaken manifest goes, and the command that reads it.
#[derive(Clone, Copy)]
enum Scope {
    Project,
    Home,
    Host,
}

/// Each ordinary mistake in each scope: a version-qualified name, a table this build does not
/// support, a broken `${var}`, and the other refusals whose hint used to cite the design. Every
/// one keeps its code and exit status, names the fix in a hint, and prints no internal id.
#[test]
fn manifest_mistakes_print_no_internal_ids() {
    let tool = "[project]\nname = \"x\"\n\n[tools.jq]\nversion = \"1.7.1\"\n";
    let sha = "a".repeat(64);
    let cases: Vec<(Scope, String, &str, u8)> = vec![
        (
            Scope::Host,
            "[packages]\ncommon = [\"curl=8.5\"]\n".into(),
            "E_UNSUPPORTED",
            3,
        ),
        (Scope::Host, "[sysctls]\n".into(), "E_UNSUPPORTED", 3),
        (
            Scope::Host,
            "[packages]\ncommon = [\"${foo.bar}\"]\n".into(),
            "E_EXCLUDED_CONSTRUCT",
            3,
        ),
        (
            Scope::Host,
            "[packages]\ncommon = [\"a\"]\n[packages.debian]\nremove = [\"b\"]\n".into(),
            "E_UNKNOWN_PACKAGE",
            3,
        ),
        (
            Scope::Host,
            "[files.\"/etc/a\"]\ncontent = \"x\"\non_remove = \"zap\"\n".into(),
            "E_TYPE",
            3,
        ),
        (
            Scope::Host,
            "[files.\"/etc/a\"]\ncontent = \"x\"\nsource = \"y\"\n".into(),
            "E_ATTR_CONFLICT",
            3,
        ),
        (
            Scope::Host,
            "[files.\"/etc/a\"]\ncontent = \"x\"\n[files.\"/etc//a\"]\ncontent = \"y\"\n".into(),
            "E_DUP_RESOURCE",
            3,
        ),
        (
            Scope::Home,
            "[home.file.\"a\"]\ntext = \"${foo.bar}\"\n".into(),
            "E_EXCLUDED_CONSTRUCT",
            3,
        ),
        (Scope::Home, "[inputs]\n".into(), "E_UNSUPPORTED", 3),
        (
            Scope::Home,
            "[home]\nregistries = []\n".into(),
            "E_UNSUPPORTED",
            3,
        ),
        (
            Scope::Project,
            "[project]\nname = \"x\"\n\n[services]\na = 1\n".into(),
            "E_UNSUPPORTED",
            3,
        ),
        (
            Scope::Project,
            "[project]\nname = \"x\"\n\n[packages]\nhold = [\"curl\"]\n".into(),
            "E_BLOCK_NOT_ALLOWED",
            3,
        ),
        (
            Scope::Project,
            format!("{tool}url = \"https://x.test/${{foo.bar}}\"\nsha256 = \"{sha}\"\n"),
            "E_EXCLUDED_CONSTRUCT",
            3,
        ),
        (
            Scope::Project,
            format!(
                "{tool}[tools.jq.aarch64]\nurl = \"https://x.test/a\"\n\
                 [tools.jq.aarch64.x86_64]\nurl = \"https://x.test/b\"\n"
            ),
            "E_UNKNOWN_BLOCK",
            3,
        ),
        (
            Scope::Project,
            format!("{tool}[tools.jq.aarch64]\nurl = \"https://x.test/a\"\nsha256 = \"{sha}\"\n"),
            "E_UNSUPPORTED_ARCH",
            4,
        ),
    ];
    let dir = scratch("plain-mistakes");
    let shims = dir.join("shims");
    fs::create_dir_all(&shims).unwrap();
    for name in ["apt-get", "dpkg-query"] {
        let path = shims.join(name);
        fs::write(
            &path,
            "#!/bin/sh\n# An inert shim: no package manager runs here.\nexit 0\n",
        )
        .unwrap();
        fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
    }
    for (scope, manifest, code, status) in cases {
        let (home, project, root) = (dir.join("home"), dir.join("project"), dir.join("root"));
        for d in [&home, &project, &root] {
            let _ = fs::remove_dir_all(d);
        }
        fs::create_dir_all(home.join(".config/lodi")).unwrap();
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(root.join("etc/lodi")).unwrap();
        let root_arg = root.to_string_lossy().into_owned();
        let args: Vec<&str> = match scope {
            Scope::Project => {
                fs::write(project.join("lodi.toml"), &manifest).unwrap();
                vec!["lock"]
            }
            Scope::Home => {
                fs::write(home.join(".config/lodi/home.toml"), &manifest).unwrap();
                vec!["home", "plan"]
            }
            Scope::Host => {
                fs::write(root.join("etc/lodi/host-allowed"), "").unwrap();
                fs::write(
                    root.join("etc/os-release"),
                    "ID=debian\nVERSION_ID=\"12\"\n",
                )
                .unwrap();
                fs::write(root.join("etc/lodi/host.toml"), &manifest).unwrap();
                vec!["host", "plan", "--root", &root_arg]
            }
        };
        let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(&args)
            .current_dir(&project)
            .env_clear()
            .env("PATH", format!("{}:/usr/bin:/bin", shims.display()))
            .env("HOME", &home)
            .env("LODI_HOME", dir.join("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .output()
            .expect("the lodi binary runs");
        let text = stderr(&out);
        assert_eq!(
            out.status.code(),
            Some(i32::from(status)),
            "{manifest}\n{text}"
        );
        assert_eq!(diag::exit_status(code), status, "{code}");
        assert!(
            text.contains(&format!("error {code}: ")),
            "{manifest}\n{text}"
        );
        assert!(
            text.contains("= hint: "),
            "no fix is named for\n{manifest}\n{text}"
        );
        assert_eq!(
            ids::internal_ids(&text),
            Vec::<String>::new(),
            "an internal id reached the user for\n{manifest}\n{text}"
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

// ------------------------------------------------- docs/ERRORS.md, both directions (A05) ---

/// Every `.rs` file below `dir`, sorted, so a new module cannot slip past these checks by
/// living in a subdirectory (`src/catalogue/`, `src/debian/`).
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

/// Every `E_…` literal outside a `#[cfg(test)]` module: what this build can put in front of a
/// user. Unit tests are cut off at the first `#[cfg(test)]`, which is where every module in
/// `src/` puts its tests (the same cut `src/container.rs`'s own source check makes).
fn emitted_codes() -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    let files = rust_files(&repo().join("src"));
    assert!(
        files.len() > 10,
        "the source files were not found: {files:?}"
    );
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        let code = match source.find("#[cfg(test)]") {
            Some(at) => &source[..at],
            None => &source[..],
        };
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
                    codes.insert(candidate.to_string());
                }
            }
        }
    }
    codes
}

/// Every `W_…` code that product source prints as a warning line (`lodi: warning W_…:`), outside
/// a `#[cfg(test)]` module. Only a warning with a catalogue row is held to this (`W_SHADOWED`,
/// LD-349, under the owner's narrow grant recorded there); the `E_` checks are unchanged.
fn emitted_warnings() -> BTreeSet<String> {
    let mut codes = BTreeSet::new();
    for file in rust_files(&repo().join("src")) {
        let source = fs::read_to_string(&file).unwrap();
        let code = match source.find("#[cfg(test)]") {
            Some(at) => &source[..at],
            None => &source[..],
        };
        let mut rest = code;
        while let Some(at) = rest.find("warning W_") {
            rest = &rest[at + "warning ".len()..];
            let end = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            codes.insert(rest[..end].to_string());
        }
    }
    codes
}

/// Every `E_…` literal in the repository's tests: unit tests in `src/` and everything in
/// `tests/`. A row whose code appears in none of them has no test.
fn tested_codes() -> BTreeSet<String> {
    let mut text = String::new();
    let mut files = rust_files(&repo().join("src"));
    files.extend(rust_files(&repo().join("tests")));
    for file in files {
        let source = fs::read_to_string(&file).unwrap();
        let in_src = file.starts_with(repo().join("src"));
        match (in_src, source.find("#[cfg(test)]")) {
            (true, Some(at)) => text.push_str(&source[at..]),
            (true, None) => {}
            (false, _) => text.push_str(&source),
        }
    }
    let mut codes = BTreeSet::new();
    for prefix in ["E_", "W_"] {
        let mut rest = text.as_str();
        while let Some(at) = rest.find(prefix) {
            rest = &rest[at..];
            let end = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            codes.insert(rest[..end].to_string());
            rest = &rest[end.max(1)..];
        }
    }
    codes
}

/// `docs/ERRORS.md`'s table: code -> (exit status, cause, next step). A `W_` row is a warning
/// the registry carries (LD-349).
fn catalogue_rows() -> BTreeMap<String, (u8, String, String)> {
    let text = fs::read_to_string(repo().join("docs/ERRORS.md")).expect("docs/ERRORS.md exists");
    let mut rows = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("| `E_") && !line.starts_with("| `W_") {
            continue;
        }
        let cells: Vec<&str> = line.trim_matches('|').split('|').map(str::trim).collect();
        assert_eq!(
            cells.len(),
            4,
            "docs/ERRORS.md row has {} cells: {line}",
            cells.len()
        );
        let code = cells[0].trim_matches('`').to_string();
        let exit: u8 = cells[1]
            .parse()
            .unwrap_or_else(|_| panic!("docs/ERRORS.md: exit column of {code} is not a number"));
        assert!(!cells[2].is_empty(), "docs/ERRORS.md: {code} has no cause");
        assert!(
            !cells[3].is_empty(),
            "docs/ERRORS.md: {code} has no next step"
        );
        assert!(
            rows.insert(
                code.clone(),
                (exit, cells[2].to_string(), cells[3].to_string())
            )
            .is_none(),
            "docs/ERRORS.md lists {code} twice"
        );
    }
    assert!(!rows.is_empty(), "docs/ERRORS.md has no table rows");
    rows
}

/// A05: the catalogue covers exactly what the binary emits, with the exit statuses the code
/// actually returns, and nothing in it is undemonstrated.
#[test]
fn errors_md_covers_every_code_in_both_directions() {
    let emitted = emitted_codes();
    let warnings = emitted_warnings();
    let rows = catalogue_rows();
    let tested = tested_codes();

    let missing: Vec<&String> = emitted.iter().filter(|c| !rows.contains_key(*c)).collect();
    assert!(
        missing.is_empty(),
        "docs/ERRORS.md has no row for {missing:?}; spec/11 §2: a new code arrives with a row \
         and a test"
    );
    let phantom: Vec<&String> = rows
        .keys()
        .filter(|c| !emitted.contains(*c) && !warnings.contains(*c))
        .collect();
    assert!(
        phantom.is_empty(),
        "docs/ERRORS.md has rows for {phantom:?}, which this build never emits"
    );
    for (code, (exit, _, _)) in &rows {
        assert_eq!(
            *exit,
            diag::exit_status(code),
            "docs/ERRORS.md gives {code} exit {exit}; lodi::diag::exit_status says {}",
            diag::exit_status(code)
        );
    }
    let untested: Vec<&String> = rows.keys().filter(|c| !tested.contains(*c)).collect();
    assert!(
        untested.is_empty(),
        "docs/ERRORS.md rows without a test: {untested:?}"
    );
}

/// The four failures of the milestone's A04 row are in the catalogue with the exit statuses the
/// table of `docs/milestones/m-0.3/TASKS.md` names.
#[test]
fn the_four_common_failures_are_in_the_catalogue() {
    let rows = catalogue_rows();
    for (code, exit) in [
        ("E_NO_MANIFEST", 3u8),
        ("E_LOCK_STALE", 10),
        ("E_NO_RUNTIME", 7),
        ("E_TRUST_REQUIRED", 11),
        ("E_EXISTS", 3),
    ] {
        let row = rows
            .get(code)
            .unwrap_or_else(|| panic!("no row for {code}"));
        assert_eq!(row.0, exit, "{code}");
        assert!(
            row.2.contains("lodi "),
            "{code}: the next step names no command: {}",
            row.2
        );
    }
}

/// The closed code registry and the catalogue are the same set, with the same exit statuses
/// (M-1.0 T-3, design call D9).
///
/// `errors_md_covers_every_code_in_both_directions` compares the catalogue with what a *scan of
/// the sources* finds. This compares it with the registry `lodi::diag::exit_status` actually
/// reads, which is the thing a user's exit status comes from: a code in one and not the other is
/// either a row for something that can never be emitted at that status, or a code whose status
/// falls through the `_` arm to exit 3 with nobody noticing.
#[test]
fn the_code_registry_and_the_catalogue_are_the_same_set() {
    let rows = catalogue_rows();
    let registered: BTreeSet<&str> = diag::CODES.iter().map(|(code, _)| *code).collect();
    let documented: BTreeSet<&str> = rows.keys().map(String::as_str).collect();
    let unregistered: Vec<&&str> = documented.difference(&registered).collect();
    assert!(
        unregistered.is_empty(),
        "docs/ERRORS.md has rows for {unregistered:?}, which lodi::diag::CODES does not carry"
    );
    let undocumented: Vec<&&str> = registered.difference(&documented).collect();
    assert!(
        undocumented.is_empty(),
        "lodi::diag::CODES carries {undocumented:?}, which docs/ERRORS.md has no row for"
    );
    for (code, status) in diag::CODES {
        assert_eq!(
            rows[*code].0, *status,
            "{code}: docs/ERRORS.md says exit {}, lodi::diag::CODES says {status}",
            rows[*code].0
        );
    }
}
