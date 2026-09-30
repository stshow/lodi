//! M-0.4 T-6: `lodi search` and `lodi info`, run as the real binary.
//!
//! Every child here runs with a cleared environment, an empty `PATH` and a `LODI_HOME` that may
//! not even exist. The proof that neither command makes a request (design call D12) is in **two
//! parts**, because no single run shows both halves of it:
//!
//! * **A closed port.** `lodi_in` rewrites every fetch to a loopback port nothing listens on, so
//!   a request would fail loudly rather than reach anything. Most tests here run this way; they
//!   observe the exit status and the output, not a request count.
//! * **A recording server.** `search_answers_from_the_catalogue_with_nothing_else_present` and
//!   `neither_command_makes_a_request_in_any_form` rewrite every fetch to a **listening**
//!   loopback server instead, and assert its request log is still empty afterwards — a request
//!   would have been recorded rather than refused. The first of the two runs the exact
//!   `lodi search ython` case both ways, so that case is covered by a refused request and by a
//!   counted zero.
//!
//! Neither shape is the other's substitute, and no test does both at once: a rewrite target is
//! either listening or it is not.
//!
//! The tests that compare a recursive listing of their scratch tree before and after are the
//! ones whose comment says so; they are what proves nothing is written.
//!
//! The distro half (design call D13) reads only the index `./lodi.lock` names, out of
//! `cache/dl/<sha256>`; the fixtures under `tests/fixtures/search/` are described in their own
//! `README.md`. Nothing here needs Podman, a network or a store that Lodi built.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use support::{ClosedPort, Server, Tool, listing, manifest, scratch, write_project, xz};

/// The binary, in `dir`, with nothing of the developer's environment: no `PATH`, no `HOME`, the
/// store at `home` (which need not exist) and every fetch pointed at a closed port, kept closed
/// until the binary has exited (LD-393).
fn lodi_in(dir: &Path, home: &Path, args: &[&str]) -> Output {
    let closed = ClosedPort::bind();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
        .env_clear()
        .env("PATH", "")
        .env("LODI_HOME", home)
        .env(
            "LODI_FETCH_REWRITE",
            format!("http://127.0.0.1:{}/", closed.port()),
        )
        .output()
        .expect("the lodi binary runs")
}

fn stdout(o: &Output) -> String {
    String::from_utf8(o.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(o: &Output) -> String {
    String::from_utf8(o.stderr.clone()).expect("stderr is UTF-8")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/search")
            .join(name),
    )
    .expect("the search fixture is committed")
}

/// The fixture container project in `dir`, with its index compressed into `home/cache/dl` under
/// the SHA-256 the lock records for it — where `lodi search` looks, and where a **caching**
/// `lodi lock` would leave it. No command of this build puts one there (LD-182), which is why
/// the warm path is proved by this fixture. Returns the cached index's path.
fn container_project(dir: &Path, home: &Path) -> PathBuf {
    let compressed = xz(fixture("packages").as_bytes());
    let hex = lodi::util::sha256_hex(&compressed);
    let lock = fixture("lodi.lock")
        .replace("__PACKAGES_SHA256__", &format!("sha256:{hex}"))
        .replace("__PACKAGES_SIZE__", &compressed.len().to_string());
    fs::create_dir_all(dir).unwrap();
    fs::write(dir.join("lodi.toml"), fixture("lodi.toml")).unwrap();
    fs::write(dir.join("lodi.lock"), &lock).unwrap();
    let cache = home.join("cache/dl");
    fs::create_dir_all(&cache).unwrap();
    let path = cache.join(&hex);
    fs::write(&path, &compressed).unwrap();
    path
}

// --------------------------------------------------------------------------- lodi search ---

/// Acceptance (a): the catalogue answers with no store, no lock, no manifest, no `PATH` and the
/// network refused — run twice over the **exact** `lodi search ython` case, once with every
/// fetch rewritten to a closed port and once against a listening server whose request log is
/// asserted empty afterwards. Both runs exit 0 and name `python`, and neither leaves a byte in
/// the project directory or creates `$LODI_HOME`.
#[test]
fn search_answers_from_the_catalogue_with_nothing_else_present() {
    let root = scratch("search-cold");
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    // A store location that does not exist: `search` must not create it either.
    let home = root.join("store-that-is-not-there");

    /// What the one case must show, whatever the rewrite target was.
    fn check(out: &Output, project: &Path, home: &Path) {
        assert_eq!(out.status.code(), Some(0), "{}", stderr(out));
        let text = stdout(out);
        assert!(text.contains("python"), "{text}");
        assert!(
            text.contains("python-build-standalone"),
            "the row carries the description and the homepage: {text}"
        );
        // No lock, so no distro index is named, and someone browsing the catalogue outside a
        // project has nothing to act on: no `W_NO_INDEX`, nothing on standard error at all.
        assert_eq!(stderr(out), "", "a browse outside a project warned");
        assert!(!home.exists(), "lodi search created the store at {home:?}");
        assert_eq!(
            listing(project),
            Vec::<String>::new(),
            "lodi search wrote into the current directory"
        );
    }

    // Part one: every fetch rewritten to a port nothing listens on, so a request would fail
    // loudly. This run observes the refusal shape, not a count — nothing is listening to count.
    let refused = lodi_in(&project, &home, &["search", "ython"]);
    check(&refused, &project, &home);

    // Part two: the same invocation with a server that would record any request it received.
    // A count of zero is only meaningful against something that was listening.
    let server = Server::start(BTreeMap::new());
    let recorded = Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["search", "ython"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", "")
        .env("LODI_HOME", &home)
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .output()
        .expect("the lodi binary runs");
    check(&recorded, &project, &home);
    assert!(
        server.requests().is_empty(),
        "lodi search made a request: {:?}",
        server.requests()
    );

    fs::remove_dir_all(&root).unwrap();
}

/// Acceptance (b): the distro half reads the cached index the lock names, and the same project
/// with the cache emptied prints the note and still exits 0.
#[test]
fn search_reads_the_cached_index_the_lock_names_and_notes_a_cold_cache() {
    let root = scratch("search-distro");
    let project = root.join("project");
    let home = root.join("home");
    let cached = container_project(&project, &home);
    let before = listing(&root);

    let warm = lodi_in(&project, &home, &["search", "cow"]);
    assert_eq!(warm.status.code(), Some(0), "{}", stderr(&warm));
    let text = stdout(&warm);
    assert!(text.contains("debian bookworm"), "{text}");
    assert!(text.contains("cowsay"), "{text}");
    assert!(text.contains("3.03+dfsg2-8"), "{text}");
    assert!(text.contains("configurable talking cow"), "{text}");
    assert!(
        !stderr(&warm).contains("W_NO_INDEX"),
        "a warm cache makes no note: {}",
        stderr(&warm)
    );
    // A query that only the description carries, and one that spans both halves.
    let both = lodi_in(&project, &home, &["search", "python"]);
    let text = stdout(&both);
    assert!(text.contains("catalogue"), "{text}");
    assert!(text.contains("python3-minimal"), "{text}");

    // Nothing was written anywhere: not the project, not the store, not the cache.
    assert_eq!(listing(&root), before, "lodi search wrote a file");

    fs::remove_file(&cached).unwrap();
    let cold = lodi_in(&project, &home, &["search", "cow"]);
    assert_eq!(cold.status.code(), Some(0), "a cold cache is not a failure");
    assert!(stderr(&cold).contains("W_NO_INDEX"), "{}", stderr(&cold));
    assert!(
        stderr(&cold).contains("debian bookworm/main"),
        "the note names the index that is missing: {}",
        stderr(&cold)
    );
    assert!(!stdout(&cold).contains("cowsay"), "{}", stdout(&cold));
    assert_eq!(stdout(&cold), "no match for \"cow\"\n");
    fs::remove_dir_all(&root).unwrap();
}

/// Acceptance (c): `--registry` and `--distro` each restrict the output, and the combination is
/// a usage error. So are a missing query, a second query and `--json` (design call D14).
#[test]
fn the_two_scopes_restrict_and_every_usage_error_is_exit_two() {
    let root = scratch("search-scopes");
    let project = root.join("project");
    let home = root.join("home");
    container_project(&project, &home);

    let registry = lodi_in(&project, &home, &["search", "--registry", "python"]);
    assert_eq!(registry.status.code(), Some(0));
    assert!(stdout(&registry).contains("catalogue"));
    assert!(
        !stdout(&registry).contains("python3-minimal"),
        "--registry showed a distro row: {}",
        stdout(&registry)
    );
    assert!(
        !stderr(&registry).contains("W_NO_INDEX"),
        "--registry does not look at the index at all"
    );

    let distro = lodi_in(&project, &home, &["search", "--distro", "python"]);
    assert_eq!(distro.status.code(), Some(0));
    assert!(stdout(&distro).contains("python3-minimal"));
    assert!(
        !stdout(&distro).contains("catalogue"),
        "--distro showed a catalogue row: {}",
        stdout(&distro)
    );

    for args in [
        &["search", "--registry", "--distro", "python"][..],
        &["search"],
        &["search", "--json", "python"],
        &["search", "python", "nodejs"],
        &["search", "--frobnicate", "python"],
        &["search", "--registry"],
    ] {
        let out = lodi_in(&project, &home, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
    }
    fs::remove_dir_all(&root).unwrap();
}

/// A query nothing answers is one line and exit 0, not an error.
#[test]
fn a_query_with_no_match_is_exit_zero() {
    let root = scratch("search-nomatch");
    let project = root.join("project");
    let home = root.join("home");
    container_project(&project, &home);
    let out = lodi_in(&project, &home, &["search", "zzzzznothing"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(stdout(&out), "no match for \"zzzzznothing\"\n");
    fs::remove_dir_all(&root).unwrap();
}

// ----------------------------------------------------------------------------- lodi info ---

/// Acceptance (d): the tools, their constraints, their locked versions and `lock: fresh`; and
/// after the manifest is edited, `lock: stale` with the label that changed — exit 0 for both,
/// because reporting the staleness is the command's job (design call D17).
#[test]
fn info_reports_the_project_and_the_lock_state() {
    let root = scratch("info-project");
    let project = root.join("project");
    let home = root.join("home");
    let python = Tool::python("3.12.11");
    let text = manifest(std::slice::from_ref(&python), "");
    write_project(&project, &text, &[python]);
    let before = listing(&root);

    let fresh = lodi_in(&project, &home, &["info"]);
    assert_eq!(fresh.status.code(), Some(0), "{}", stderr(&fresh));
    let out = stdout(&fresh);
    assert!(out.contains("project: demo"), "{out}");
    assert!(out.contains("mode:    host tools"), "{out}");
    assert!(out.contains("python"), "{out}");
    assert!(out.contains("3.12"), "the constraint is shown: {out}");
    assert!(
        out.contains("3.12.11"),
        "the locked version is shown: {out}"
    );
    assert!(out.contains("art-"), "the store entry is shown: {out}");
    assert!(out.contains("lock:    fresh"), "{out}");

    fs::write(project.join("lodi.toml"), text.replace("3.12", "3.11")).unwrap();
    let stale = lodi_in(&project, &home, &["info"]);
    assert_eq!(stale.status.code(), Some(0), "a stale lock is not an error");
    let out = stdout(&stale);
    assert!(out.contains("lock:    stale"), "{out}");
    assert!(
        out.contains("python: locked for `3.12`, the manifest asks for `3.11`"),
        "the changed label is named: {out}"
    );

    fs::write(project.join("lodi.toml"), &text).unwrap();
    fs::remove_file(project.join("lodi.lock")).unwrap();
    let missing = lodi_in(&project, &home, &["info"]);
    assert_eq!(missing.status.code(), Some(0));
    assert!(
        stdout(&missing).contains("lock:    missing"),
        "{}",
        stdout(&missing)
    );

    // `info` writes nothing: the lock this test removed is the only difference.
    let mut expected = before.clone();
    expected.retain(|p| !p.ends_with("lodi.lock"));
    assert_eq!(listing(&root), expected, "lodi info wrote a file");
    fs::remove_dir_all(&root).unwrap();
}

/// An inline `[tools]` declaration (M-0.4 T-2's contract) is a project's own pin, not a
/// catalogue recipe: `lodi info` reports it like any other row, and `lodi info NAME` for it is
/// still `E_NO_RECIPE`, because there is no recipe to print.
#[test]
fn info_reports_an_inline_tool_but_has_no_recipe_for_it() {
    let root = scratch("info-inline");
    let project = root.join("project");
    let home = root.join("home");
    let text = manifest(
        &[],
        "jaq = { url = \"https://example.invalid/jaq\", sha256 = \"\
         0000000000000000000000000000000000000000000000000000000000000000\", \
         version = \"1.7.1\", format = \"binary\" }\n",
    );
    fs::create_dir_all(&project).unwrap();
    fs::write(project.join("lodi.toml"), &text).unwrap();
    let before = listing(&root);

    let out = lodi_in(&project, &home, &["info"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.contains("jaq"), "the inline tool is a row: {shown}");
    assert!(shown.contains("1.7.1"), "with what it pins: {shown}");
    assert!(shown.contains("lock:    missing"), "{shown}");
    assert!(
        !shown.contains("no recipe"),
        "an inline tool needs no recipe: {shown}"
    );

    let no_recipe = lodi_in(&project, &home, &["info", "jaq"]);
    assert_eq!(no_recipe.status.code(), Some(4), "{}", stdout(&no_recipe));
    assert!(
        stderr(&no_recipe).contains("E_NO_RECIPE"),
        "{}",
        stderr(&no_recipe)
    );

    assert_eq!(listing(&root), before, "lodi info wrote a file");
    fs::remove_dir_all(&root).unwrap();
}

/// A `[tools]` name the catalogue has no recipe for is annotated in `lodi info`'s row, with the
/// nearest names `lodi lock` would give, offline and at exit 0: `lodi info` reports, and the
/// refusal is `lodi lock`'s. A name that resembles nothing gets the whole catalogue, as there.
#[test]
fn info_names_a_tool_the_catalogue_has_no_recipe_for() {
    let root = scratch("info-no-recipe");
    let project = root.join("project");
    let home = root.join("home");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("lodi.toml"),
        "[project]\nname = \"typo\"\n\n[tools]\npyhton = \"3.12\"\nzzzzzzzzzz = \"1\"\n\
         python = \"3.12\"\n",
    )
    .unwrap();
    let before = listing(&root);
    let out = lodi_in(&project, &home, &["info"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let rows: Vec<String> = stdout(&out).lines().map(str::to_owned).collect();
    let row = |label: &str| {
        rows.iter()
            .find(|r| r.trim_start().starts_with(&format!("{label} ")))
            .unwrap_or_else(|| panic!("no row for {label}: {rows:?}"))
            .clone()
    };
    assert!(
        row("pyhton").ends_with("(no recipe `pyhton` in the catalogue; nearest names: python)"),
        "{rows:?}"
    );
    let all = lodi::catalogue::builtin_tool_names().join(", ");
    assert!(
        row("zzzzzzzzzz").ends_with(&format!(
            "(no recipe `zzzzzzzzzz` in the catalogue; nearest names: {all})"
        )),
        "{rows:?}"
    );
    assert!(!row("python").contains("no recipe"), "{rows:?}");
    assert_eq!(listing(&root), before, "lodi info wrote a file");
    fs::remove_dir_all(&root).unwrap();
}

/// `W_NO_INDEX` is printed only where the user can act on it: when the distro half is asked for
/// by name (`--distro`), or when the lock names a container base whose index is missing. A
/// catalogue search with no lock, or in a host-tool project, prints none. Neither changes the
/// exit status.
#[test]
fn the_missing_index_is_noted_only_where_it_can_be_acted_on() {
    let root = scratch("search-actionable");
    let home = root.join("home");

    // No lock at all.
    let empty = root.join("empty");
    fs::create_dir_all(&empty).unwrap();
    let browse = lodi_in(&empty, &home, &["search", "python"]);
    assert_eq!(browse.status.code(), Some(0), "{}", stderr(&browse));
    assert_eq!(stderr(&browse), "");
    let asked = lodi_in(&empty, &home, &["search", "--distro", "python"]);
    assert_eq!(asked.status.code(), Some(0), "{}", stderr(&asked));
    assert!(stderr(&asked).contains("W_NO_INDEX"), "{}", stderr(&asked));
    assert!(
        stderr(&asked).contains("run `lodi lock`"),
        "{}",
        stderr(&asked)
    );

    // A host-tool project: a lock that pins no container base.
    let host = root.join("host");
    let python = Tool::python("3.12.11");
    write_project(
        &host,
        &manifest(std::slice::from_ref(&python), ""),
        &[python],
    );
    let browse = lodi_in(&host, &home, &["search", "python"]);
    assert_eq!(browse.status.code(), Some(0), "{}", stderr(&browse));
    assert_eq!(stderr(&browse), "");
    assert!(stdout(&browse).contains("catalogue"), "{}", stdout(&browse));
    let asked = lodi_in(&host, &home, &["search", "--distro", "python"]);
    assert_eq!(asked.status.code(), Some(0), "{}", stderr(&asked));
    assert!(
        stderr(&asked).contains("pins no container base"),
        "{}",
        stderr(&asked)
    );

    // A lock that cannot be read is named with `--distro` only.
    fs::write(host.join("lodi.lock"), "not a lock").unwrap();
    let browse = lodi_in(&host, &home, &["search", "python"]);
    assert_eq!(browse.status.code(), Some(0), "{}", stderr(&browse));
    assert_eq!(stderr(&browse), "");
    let asked = lodi_in(&host, &home, &["search", "--distro", "python"]);
    assert_eq!(asked.status.code(), Some(0), "{}", stderr(&asked));
    assert!(stderr(&asked).contains("W_NO_INDEX"), "{}", stderr(&asked));

    // A container project whose index is not cached: the base is this project's, so it is noted
    // in the default search too.
    let container = root.join("container");
    let cached = container_project(&container, &home);
    fs::remove_file(&cached).unwrap();
    let browse = lodi_in(&container, &home, &["search", "python"]);
    assert_eq!(browse.status.code(), Some(0), "{}", stderr(&browse));
    assert!(
        stderr(&browse).contains("W_NO_INDEX"),
        "{}",
        stderr(&browse)
    );
    assert!(stdout(&browse).contains("catalogue"), "{}", stdout(&browse));
    fs::remove_dir_all(&root).unwrap();
}

/// A container project: the base and its snapshot, and the packages it asks for.
#[test]
fn info_names_the_base_of_a_container_project() {
    let root = scratch("info-container");
    let project = root.join("project");
    let home = root.join("home");
    container_project(&project, &home);
    let out = lodi_in(&project, &home, &["info"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("mode:    container"), "{text}");
    assert!(
        text.contains("base:    debian bookworm snapshot 2026-09-18T00:00:00Z"),
        "{text}"
    );
    assert!(text.contains("cowsay"), "{text}");
    assert!(text.contains("lock:    fresh"), "{text}");
    fs::remove_dir_all(&root).unwrap();
}

/// Acceptance (e): no manifest is `E_NO_MANIFEST` (3), and an unknown tool is `E_NO_RECIPE` (4)
/// with the nearest names. Acceptance (f): `lodi info python` in an empty directory with no
/// lock prints the recipe and exits 0.
#[test]
fn info_needs_a_manifest_but_a_recipe_is_a_property_of_the_binary() {
    let root = scratch("info-errors");
    let empty = root.join("empty");
    let home = root.join("home");
    fs::create_dir_all(&empty).unwrap();

    let none = lodi_in(&empty, &home, &["info"]);
    assert_eq!(none.status.code(), Some(3), "{}", stderr(&none));
    assert!(stderr(&none).contains("E_NO_MANIFEST"), "{}", stderr(&none));

    let unknown = lodi_in(&empty, &home, &["info", "not-a-tool"]);
    assert_eq!(unknown.status.code(), Some(4), "{}", stderr(&unknown));
    let text = stderr(&unknown);
    assert!(text.contains("E_NO_RECIPE"), "{text}");
    assert!(text.contains("nearest names"), "{text}");
    assert!(text.contains("python"), "{text}");

    let typo = lodi_in(&empty, &home, &["info", "pyhton"]);
    assert_eq!(typo.status.code(), Some(4));
    assert!(
        stderr(&typo).contains("did you mean python?"),
        "{}",
        stderr(&typo)
    );

    let recipe = lodi_in(&empty, &home, &["info", "python"]);
    assert_eq!(recipe.status.code(), Some(0), "{}", stderr(&recipe));
    let text = stdout(&recipe);
    for expected in [
        "description:",
        "homepage:",
        "strategy:",
        "upstream:",
        "bin:",
        "path:",
        "recipe:",
    ] {
        assert!(text.contains(expected), "{expected} is missing: {text}");
    }
    assert!(
        !text.contains("locked:"),
        "there is no lock here, so nothing is locked: {text}"
    );
    assert_eq!(
        listing(&empty),
        Vec::<String>::new(),
        "lodi info wrote a file"
    );
    assert!(!home.exists(), "lodi info created the store");

    // Every flag is a usage error: there is no `--json`. `info --help` is the command's help
    // since LD-360, answered before anything is read, so it writes nothing either.
    let out = lodi_in(&empty, &home, &["info", "--help"]);
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(
        listing(&empty),
        Vec::<String>::new(),
        "info --help wrote a file"
    );
    assert!(!home.exists(), "info --help created the store");
    for args in [&["info", "--json"][..], &["info", "python", "nodejs"]] {
        let out = lodi_in(&empty, &home, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
    fs::remove_dir_all(&root).unwrap();
}

/// `lodi info TOOL` adds what this project locked for that label, and only then.
#[test]
fn info_tool_adds_the_lock_of_the_current_directory() {
    let root = scratch("info-tool-lock");
    let project = root.join("project");
    let home = root.join("home");
    let python = Tool::python("3.12.11");
    let text = manifest(std::slice::from_ref(&python), "");
    write_project(&project, &text, &[python]);
    let out = lodi_in(&project, &home, &["info", "python"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let text = stdout(&out);
    assert!(text.contains("locked:"), "{text}");
    assert!(text.contains("3.12.11"), "{text}");
    assert!(text.contains("art-"), "{text}");
    assert!(text.contains("sha256:"), "{text}");
    // A recipe the project does not lock keeps the catalogue half alone.
    let other = lodi_in(&project, &home, &["info", "ripgrep"]);
    assert_eq!(other.status.code(), Some(0), "{}", stderr(&other));
    assert!(!stdout(&other).contains("locked:"), "{}", stdout(&other));
    fs::remove_dir_all(&root).unwrap();
}

/// The recording half of the request proof, over every form this milestone ships: five
/// invocations — both commands, every scope, a manifest and a lock present and the cache
/// warm — against a **listening** loopback server, whose request log is asserted empty after
/// each one. The closed-port half is `lodi_in`, which the rest of this file uses; this test
/// does not use it, because a count of zero needs something that was listening.
#[test]
fn neither_command_makes_a_request_in_any_form() {
    let root = scratch("search-offline");
    let project = root.join("project");
    let home = root.join("home");
    container_project(&project, &home);
    let server = Server::start(BTreeMap::new());
    for args in [
        &["search", "python"][..],
        &["search", "--registry", "python"],
        &["search", "--distro", "python"],
        &["info"],
        &["info", "python"],
    ] {
        let out = Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .current_dir(&project)
            .env_clear()
            .env("PATH", "")
            .env("LODI_HOME", &home)
            .env("LODI_FETCH_REWRITE", server.rewrite())
            .output()
            .expect("the lodi binary runs");
        assert_eq!(out.status.code(), Some(0), "{args:?}: {}", stderr(&out));
        assert!(
            server.requests().is_empty(),
            "{args:?} made a request: {:?}",
            server.requests()
        );
    }
    fs::remove_dir_all(&root).unwrap();
}
