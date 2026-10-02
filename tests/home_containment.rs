//! The containment gate of the home scope (M-0.5 T-1, design calls D1, D4, D13, D15 —
//! `LD-97`, `LD-100`, `LD-109`, `LD-111`).
//!
//! "Lodi never writes outside the roots it read from the environment" is not a promise here; it
//! is four independent proofs, and this file fails when any of them stops holding:
//!
//! 1. **The type wall.** `home::fsops` takes a `Root` that only `Roots` can make and a `RelPath`
//!    that refuses everything which could leave it, so an absolute destination is not
//!    expressible. Every escape case of the acceptance row is exercised below.
//! 2. **The ledger.** Every mutating call appends `<op>\t<absolute path>` before it acts. After
//!    a run that writes, the ledger is non-empty and every line is under this process's scratch
//!    root.
//! 3. **The decoy.** A small fixed tree beside the roots, listed and hashed before and after: a
//!    write that escaped its root but stayed in the scratch tree is still caught.
//! 4. **The source lint.** One marked span of `src/roots.rs` is the only reader of `HOME`,
//!    `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `LODI_HOME` — `src/roots.rs` is **not** exempt from
//!    its own rule since `validator-repair-1` (`LD-184`); no home-scope module but `fsops` calls
//!    a mutating `std::fs` entry point; and no file in `src/` names `getpwuid` or `home_dir`.
//!
//! 5. **The lock is bounded by the same type wall as every write.** Since
//!    `validator-repair-1` `lodi home apply` takes `<data>/home-scope/.lock` through
//!    [`lodi::home::fsops::lock`] — a `Root` plus a `RelPath` — so the scope has **no** mutation
//!    outside `fsops` and the lint below carries no exception for one (`LD-185`). `fsops` is the
//!    only module of the scope that may name the store's lock type, and the run below proves the
//!    file it creates is under the data root.
//!
//! Everything here is offline and deterministic. The gate was built before the writer (T-1), and
//! since T-3 the writer runs under it: `an_apply_records_every_write_and_stays_inside_the_roots`
//! drives the real `lodi home apply` and holds its whole ledger to the same four proofs.

mod support;

use std::collections::BTreeSet;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use lodi::diag;
use lodi::home::fsops::{self, RelPath};
use lodi::roots::Roots;

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn os(text: &str) -> OsString {
    OsString::from(text)
}

// ------------------------------------------------------------------ the roots themselves ---

/// (b) An unset, empty or relative `HOME` is `E_CONFIG` at exit 3 and carries the hint. The
/// values are given rather than read so that this test changes no process's environment;
/// `Roots::from_env` is the same code over `std::env`.
#[test]
fn an_unusable_home_is_a_config_error_and_never_a_lookup() {
    let relative = os("not/absolute");
    let missing = os("/does/not/exist/at/all");
    let cases: [(&str, Option<&OsString>); 4] = [
        ("unset", None),
        ("empty", Some(&os(""))),
        ("relative", Some(&relative)),
        ("not a directory", Some(&missing)),
    ];
    for (what, home) in cases {
        let d = Roots::from_vars(home.map(OsString::as_os_str), None, None, None).expect_err(what);
        assert_eq!(d.code, "E_CONFIG", "{what}");
        assert_eq!(diag::exit_status(d.code), 3, "{what}");
        assert!(
            d.to_string()
                .contains("hint: set HOME to the directory lodi should manage"),
            "{what}: {d}"
        );
    }
    // The refusal says in so many words that there is no password-database fallback.
    let d = Roots::from_vars(None, None, None, None).unwrap_err();
    assert!(d.to_string().contains("password database"), "{d}");
}

/// A relative `XDG_CONFIG_HOME`, `XDG_DATA_HOME` or `LODI_HOME` is `E_CONFIG` too, never a
/// silent fallback to the default.
#[test]
fn a_relative_root_variable_is_refused_rather_than_ignored() {
    let env = support::home_env("relative-roots");
    let home = OsString::from(env.home());
    let relative = os("relative/place");
    for (what, config, data, lodi_home) in [
        ("XDG_CONFIG_HOME", Some(&relative), None, None),
        ("XDG_DATA_HOME", None, Some(&relative), None),
        ("LODI_HOME", None, None, Some(&relative)),
    ] {
        let d = Roots::from_vars(
            Some(home.as_os_str()),
            config.map(OsString::as_os_str),
            data.map(OsString::as_os_str),
            lodi_home.map(OsString::as_os_str),
        )
        .expect_err(what);
        assert_eq!(d.code, "E_CONFIG", "{what}: {d}");
        assert!(d.to_string().contains(what), "{what}: {d}");
        assert!(
            d.to_string().contains("not an absolute path"),
            "{what}: {d}"
        );
    }
}

/// (b) The binary itself, run with `HOME` removed and nothing else in its environment, fails
/// with `E_CONFIG` at exit 3 rather than resolving a real account's home. `lodi develop`'s trust
/// check is what needs a configuration directory here.
#[test]
fn the_binary_with_no_home_at_all_refuses_rather_than_resolving() {
    let env = support::home_env("no-home");
    let before = env.decoy_listing();
    let project = env.root().join("project");
    fs::create_dir_all(&project).unwrap();
    assert!(
        env.command()
            .arg("init")
            .current_dir(&project)
            .status()
            .unwrap()
            .success(),
        "lodi init writes the manifest lodi develop then reads"
    );

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(["develop", "--", "true"])
        .current_dir(&project)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    assert_eq!(out.status.code(), Some(3), "{text}");
    assert!(text.contains("lodi: error E_CONFIG"), "{text}");
    // Nothing was created beside the project: with no root there is no place to write one.
    assert_eq!(env.decoy_listing(), before, "the decoy tree changed");
}

// ------------------------------------------------------------------------- RelPath (c) ---

/// (c) Every escape case of the acceptance row is `E_PATH_ESCAPE` at exit 3.
#[test]
fn every_escape_case_is_refused_by_the_constructor() {
    for case in [
        "/etc/passwd",
        "~/x",
        "~",
        "../x",
        "a/../../b",
        "a/b/../../../c",
        "a\0b",
        "",
        "./x",
        "a//b",
        "a/.",
        "/",
    ] {
        let d = RelPath::new(case)
            .map(|p| p.as_str().to_string())
            .expect_err(&format!("{case:?} must be refused"));
        assert_eq!(d.code, "E_PATH_ESCAPE", "{case:?}: {d}");
        assert_eq!(diag::exit_status(d.code), 3, "{case:?}");
    }
    // A component longer than 255 bytes is refused as well.
    let long = "a".repeat(256);
    assert_eq!(
        RelPath::new(&long).unwrap_err().code,
        "E_PATH_ESCAPE",
        "a 256-byte component"
    );
    // And the ordinary shapes the home manifest uses are accepted.
    for good in [".config/git/ignore", ".inputrc", "a/b/c"] {
        assert!(RelPath::new(good).is_ok(), "{good}");
    }
}

// ----------------------------------------------------------- writes, symlinks, the ledger ---

fn rel(text: &str) -> RelPath {
    RelPath::new(text).expect("a relative path inside the root")
}

/// (d) A symlinked intermediate component and a symlinked final component are both refused, and
/// the file each link points at is unchanged.
#[test]
fn a_symlinked_component_is_never_traversed() {
    let env = support::home_env("symlink");
    let root = Roots::from_vars(
        Some(OsString::from(env.home()).as_os_str()),
        None,
        None,
        None,
    )
    .unwrap()
    .home_root();
    let outside = env.root().join("outside");
    fs::create_dir_all(&outside).unwrap();
    let target = outside.join("target");
    fs::write(&target, b"untouched\n").unwrap();

    // An intermediate component: <home>/via -> <root>/outside, then a write to `via/target`.
    std::os::unix::fs::symlink(&outside, env.home().join("via")).unwrap();
    let d = fsops::write(&root, &rel("via/target"), b"clobbered\n", 0o644).unwrap_err();
    assert_eq!(d.code, "E_PATH_ESCAPE", "{d}");
    assert!(d.to_string().contains("via"), "{d}");

    // A final component: <home>/link -> <root>/outside/target.
    std::os::unix::fs::symlink(&target, env.home().join("link")).unwrap();
    for d in [
        fsops::write(&root, &rel("link"), b"clobbered\n", 0o644).unwrap_err(),
        fsops::remove(&root, &rel("link")).unwrap_err(),
        fsops::set_mode(&root, &rel("link"), 0o600).unwrap_err(),
    ] {
        assert_eq!(d.code, "E_PATH_ESCAPE", "{d}");
        assert!(d.to_string().contains("link"), "{d}");
    }

    assert_eq!(
        fs::read(&target).unwrap(),
        b"untouched\n",
        "the file the links point at was modified"
    );
    // A11: a refused path is refused *before* the ledger line, so nothing claims it was touched.
    let named: Vec<PathBuf> = support::home_gate()
        .ledger_lines()
        .into_iter()
        .map(|(_, path)| path)
        .filter(|path| path.starts_with(&outside) || path.starts_with(env.home()))
        .collect();
    assert!(
        named.is_empty(),
        "the ledger holds lines for paths nothing was allowed to touch: {named:?}"
    );
    // The link itself is still a link: nothing replaced it either.
    assert!(
        fs::symlink_metadata(env.home().join("link"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

/// (e) A run that writes leaves a non-empty ledger, every line of it under the scratch root, and
/// the decoy tree beside the roots is untouched. This also exercises the whole `fsops` surface:
/// `mkdir_p`, `write`, `read`, `copy`, `set_mode` and `remove`.
#[test]
fn what_fsops_writes_is_recorded_and_stays_inside_the_root() {
    let gate = support::home_gate();
    let env = support::home_env("ledger");
    let before = env.decoy_listing();
    let roots = Roots::from_vars(
        Some(OsString::from(env.home()).as_os_str()),
        None,
        None,
        None,
    )
    .unwrap();
    let root = roots.home_root();
    assert_eq!(root.path(), env.home());

    fsops::mkdir_p(&root, &rel(".config/git")).unwrap();
    fsops::write(&root, &rel(".config/git/ignore"), b".lodi/\n", 0o644).unwrap();
    assert_eq!(
        fsops::read(&root, &rel(".config/git/ignore")).unwrap(),
        b".lodi/\n"
    );
    // The parent directories of a destination are created by the write itself.
    fsops::write(&root, &rel("deep/er/still/file"), b"x\n", 0o600).unwrap();
    let source = env.root().join("source");
    fs::write(&source, b"copied\n").unwrap();
    fsops::copy(&root, &rel(".inputrc"), &source, 0o644).unwrap();
    fsops::set_mode(&root, &rel(".inputrc"), 0o600).unwrap();
    fsops::remove(&root, &rel("deep/er/still/file")).unwrap();
    // Removing what is not there is not an error, and records nothing to undo.
    fsops::remove(&root, &rel("never/existed")).unwrap();

    // The modes are what was asked for, and the content is what was written.
    use std::os::unix::fs::PermissionsExt;
    let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&env.home().join(".config/git/ignore")), 0o644);
    assert_eq!(mode(&env.home().join(".inputrc")), 0o600);
    assert_eq!(fs::read(env.home().join(".inputrc")).unwrap(), b"copied\n");
    // No temporary file survives a successful write (D13: the name is the content's hash).
    let leftovers: Vec<PathBuf> = fs::read_dir(env.home().join(".config/git"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| {
            p.file_name()
                .is_some_and(|n| n.to_string_lossy().starts_with(".lodi-tmp-"))
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "temporary files were left: {leftovers:?}"
    );

    let lines = gate.ledger_lines();
    assert!(
        !lines.is_empty(),
        "the ledger is empty after a run that wrote"
    );
    for (op, path) in &lines {
        assert!(
            path.starts_with(gate.root()),
            "the ledger records {op} {} outside the scratch root {}",
            path.display(),
            gate.root().display()
        );
        assert!(
            path.is_absolute(),
            "a ledger line is not absolute: {path:?}"
        );
    }
    // Every mutating entry point announced itself; `read` is not a mutation and announced nothing.
    let ops: BTreeSet<&str> = lines.iter().map(|(op, _)| op.as_str()).collect();
    for op in ["mkdir", "write", "chmod", "remove"] {
        assert!(ops.contains(op), "no ledger line for {op}: {ops:?}");
    }
    // This test's own writes are in there, named by their absolute path.
    let paths: BTreeSet<&Path> = lines.iter().map(|(_, p)| p.as_path()).collect();
    assert!(paths.contains(env.home().join(".config/git/ignore").as_path()));

    assert_eq!(env.decoy_listing(), before, "the decoy tree changed");
}

/// A directory is not a removable home-scope file. The refusal happens before the ledger and
/// changes nothing, even when the directory is empty and the operating system would allow its
/// removal (`LD-194`).
#[test]
fn fsops_remove_refuses_a_directory_without_mutating() {
    let gate = support::home_gate();
    let env = support::home_env("remove-directory");
    let root = Roots::from_vars(
        Some(OsString::from(env.home()).as_os_str()),
        None,
        None,
        None,
    )
    .unwrap()
    .home_root();
    let directory = env.home().join("keep-directory");
    fs::create_dir(&directory).unwrap();

    let scratch_before = support::listing(env.root());
    let decoy_before = env.decoy_listing();
    let ledger_for_scratch = || {
        gate.ledger_lines()
            .into_iter()
            .filter(|(_, path)| path.starts_with(env.root()))
            .collect::<Vec<_>>()
    };
    let ledger_before = ledger_for_scratch();

    let d = fsops::remove(&root, &rel("keep-directory")).unwrap_err();
    assert_eq!(d.code, "E_STORE_IO", "{d}");
    assert_eq!(diag::exit_status(d.code), 6, "{d}");
    assert!(directory.is_dir(), "the directory was removed");
    assert_eq!(
        support::listing(env.root()),
        scratch_before,
        "the scratch tree changed"
    );
    assert_eq!(
        ledger_for_scratch(),
        ledger_before,
        "a refused directory removal was recorded as a mutation"
    );
    assert_eq!(env.decoy_listing(), decoy_before, "the decoy tree changed");
}

/// The harness itself: a child of these tests sees a scratch `HOME`, never the ambient one, and
/// nothing outside the scratch root gains a file when the binary runs.
#[test]
fn a_child_of_these_tests_never_sees_the_real_home() {
    let env = support::home_env("child");
    let ambient = std::env::var_os("HOME").map(PathBuf::from);
    assert_ne!(ambient.as_deref(), Some(env.home()));
    assert!(env.home().starts_with(support::home_gate().root()));

    let before = env.decoy_listing();
    let out = env.command().arg("--version").output().unwrap();
    assert!(out.status.success(), "{:?}", out.status);
    assert_eq!(env.decoy_listing(), before, "the decoy tree changed");
    // Neither the scratch home nor the config root gained anything from a read-only command.
    assert_eq!(listing(env.home()), vec![".config".to_string()]);
    assert!(listing(env.config()).is_empty());
}

fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

// ------------------------------------------------------------------------ the source lint ---

/// Every `.rs` file below `dir`, sorted.
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

/// A file's source with its unit tests cut off, the same cut `tests/diagnostics.rs` makes: a
/// fixture inside `#[cfg(test)]` is not what a user's process runs.
fn source_without_tests(path: &Path) -> String {
    let text = fs::read_to_string(path).unwrap();
    match text.find("#[cfg(test)]") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

/// The **only** places outside the acquisition span where a root variable may be named, each
/// with the reason it is there. A stale exception fails this test as loudly as a new reader
/// would: the list is checked to be exactly used.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "src/roots.rs",
        "never `getpwuid(getuid())->pw_dir`",
        "the module comment saying in so many words that there is no password-database fallback",
    ),
    (
        "src/container.rs",
        "[\"HOME\", \"USER\", \"LOGNAME\", \"TERM\"]",
        "the variables a container child inherits: this sets HOME *inside* a container",
    ),
    (
        "src/container.rs",
        ".get(OsStr::new(\"HOME\"))",
        "the child's own HOME inside the container, read back from the child environment",
    ),
    (
        "src/manifest.rs",
        "const RESERVED_ENV",
        "the `${var}` substitution's reserved names, which are not read from the environment",
    ),
];

/// The marked acquisition span of `src/roots.rs`, as a half-open range of line numbers. The
/// markers are asserted to exist exactly once each and in order, so deleting one to widen the
/// span fails this test rather than passing it silently.
fn acquisition_span(source: &str) -> std::ops::Range<usize> {
    let find = |marker: &str| {
        let hits: Vec<usize> = source
            .lines()
            .enumerate()
            .filter(|(_, line)| {
                line.contains(marker) && (marker.starts_with("end") || !line.contains("end of"))
            })
            .map(|(n, _)| n)
            .collect();
        assert_eq!(
            hits.len(),
            1,
            "src/roots.rs must carry exactly one `{marker}` marker: {hits:?}"
        );
        hits[0]
    };
    let begin = find("the one acquisition point ---");
    let end = find("end of the one acquisition point ---");
    assert!(
        begin < end,
        "the acquisition markers are in the wrong order"
    );
    begin..end
}

/// (a) One marked span of `src/roots.rs` is the only reader of `HOME`, `XDG_CONFIG_HOME`,
/// `XDG_DATA_HOME` and `LODI_HOME` — **`src/roots.rs` included** (`LD-184`) — and nothing
/// anywhere in `src/` consults the password database.
#[test]
fn only_roots_reads_the_environment_for_a_directory() {
    let needles = [
        "\"HOME\"",
        "\"XDG_CONFIG_HOME\"",
        "\"XDG_DATA_HOME\"",
        "\"LODI_HOME\"",
        "home_dir",
        "getpwuid",
        "getuid",
        "pw_dir",
        // A shell rc file is never named in `src/`: Lodi prints the one line a user adds to it
        // and writes none of them itself (design call D11, `LD-107`). T-5 depends on this.
        "\".bashrc\"",
        "\".zshrc\"",
        "\".profile\"",
        "\".bash_profile\"",
        "\"config.fish\"",
        "\".zprofile\"",
        "\".kshrc\"",
    ];
    let files = rust_files(&repo().join("src"));
    assert!(
        files.len() > 10,
        "the source files were not found: {files:?}"
    );
    let mut used: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut offences = Vec::new();
    for file in &files {
        let rel = file
            .strip_prefix(repo())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        let source = source_without_tests(file);
        // `src/roots.rs` is held to the same rule as every other file, minus exactly one span:
        // the acquisition point itself, which is where the four variables are read.
        let span = (rel == "src/roots.rs").then(|| acquisition_span(&source));
        for (number, line) in source.lines().enumerate() {
            if span.as_ref().is_some_and(|s| s.contains(&number)) {
                continue;
            }
            if !needles.iter().any(|n| line.contains(n)) {
                continue;
            }
            match ALLOWED
                .iter()
                .find(|(path, snippet, _)| *path == rel && line.contains(snippet))
            {
                Some((path, snippet, _)) => {
                    used.insert((path, snippet));
                }
                None => offences.push(format!("{rel}:{}: {}", number + 1, line.trim())),
            }
        }
    }
    assert!(
        offences.is_empty(),
        "only the acquisition span of src/roots.rs may read HOME, XDG_CONFIG_HOME, \
         XDG_DATA_HOME or LODI_HOME, and nothing may consult the password database (design \
         call D1, LD-97, LD-184):\n{}",
        offences.join("\n")
    );
    let stale: Vec<&(&str, &str, &str)> = ALLOWED
        .iter()
        .filter(|(path, snippet, _)| !used.contains(&(*path, *snippet)))
        .collect();
    assert!(
        stale.is_empty(),
        "these exceptions no longer match anything and must be deleted: {stale:?}"
    );

    // A name is not the only way to read a variable: `var(NAME)` with a constant would pass the
    // needles above. So every call of `src/roots.rs`'s own environment reader is held to the
    // acquisition span too, with one named exception — the write ledger, which is a different
    // variable and a test facility (LD-100).
    let source = source_without_tests(&repo().join("src/roots.rs"));
    let span = acquisition_span(&source);
    let mut ledger_seen = false;
    let mut readers = Vec::new();
    for (number, line) in source.lines().enumerate() {
        if span.contains(&number) || !line.contains("var(") || line.contains("fn var(") {
            continue;
        }
        if line.contains("var(\"LODI_FS_LEDGER\")") {
            ledger_seen = true;
            continue;
        }
        readers.push(format!("src/roots.rs:{}: {}", number + 1, line.trim()));
    }
    assert!(
        readers.is_empty(),
        "the environment is read outside the one acquisition point of src/roots.rs (LD-184):\n{}",
        readers.join("\n")
    );
    assert!(
        ledger_seen,
        "the ledger's own read is gone; delete its exception above with it"
    );
    // And `std::env` itself is reachable from exactly one helper in the file.
    let env_calls = source.matches("std::env::").count();
    assert_eq!(
        env_calls, 1,
        "src/roots.rs names std::env:: {env_calls} times; one helper reads the environment"
    );
}

/// No module of the home scope but `fsops` touches the filesystem. This is what keeps "one place
/// that writes" true as the scope grows in T-2 and T-3.
#[test]
fn only_fsops_mutates_the_filesystem_in_the_home_scope() {
    let mutators = [
        "fs::write",
        "File::create",
        "OpenOptions",
        "fs::remove_",
        "fs::rename",
        "fs::set_permissions",
        "fs::create_dir",
        "fs::copy",
        "symlink",
    ];
    let home = repo().join("src/home");
    let files = rust_files(&home);
    assert!(
        files.iter().any(|f| f.ends_with("fsops.rs")),
        "src/home/fsops.rs is the module this lint is about: {files:?}"
    );
    let mut offences = Vec::new();
    for file in &files {
        if file.ends_with("fsops.rs") {
            continue;
        }
        let rel = file
            .strip_prefix(repo())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        for (number, line) in source_without_tests(file).lines().enumerate() {
            if mutators.iter().any(|m| line.contains(m)) {
                offences.push(format!("{rel}:{}: {}", number + 1, line.trim()));
            }
        }
    }
    assert!(
        offences.is_empty(),
        "src/home/fsops.rs is the only module of the home scope that may mutate the filesystem \
         (design call D4, LD-100):\n{}",
        offences.join("\n")
    );
}

/// The lock goes through `fsops` like every other mutation of this scope (`LD-185`), so it is
/// held to three things: `fsops` is the only module that may name the store's lock type,
/// `apply.rs` carries no exception any more, and the path taken is the data root plus a relative
/// name that cannot leave it.
#[test]
fn the_apply_lock_goes_through_fsops_and_stays_under_the_data_root() {
    // `<data>/home-scope/.lock`, as a relative path the type wall accepts.
    assert_eq!(lodi::home::state::LOCK_FILE, "home-scope/.lock");
    assert!(
        RelPath::new(lodi::home::state::LOCK_FILE).is_ok(),
        "the lock's path is not one a Root would accept"
    );
    assert!(
        lodi::home::state::LOCK_FILE.starts_with(lodi::home::state::HOME_SCOPE_DIR),
        "the lock is outside the scope directory"
    );

    // `src/home/fsops.rs` is the only module of the scope that may name the store's lock at
    // all — `src/home/apply.rs` included, which is what `validator-repair-1` removed.
    let mut offenders = Vec::new();
    for file in rust_files(&repo().join("src/home")) {
        let rel = file
            .strip_prefix(repo())
            .unwrap()
            .to_string_lossy()
            .into_owned();
        if rel == "src/home/fsops.rs" {
            continue;
        }
        if source_without_tests(&file).contains("FileLock") {
            offenders.push(rel);
        }
    }
    assert!(
        offenders.is_empty(),
        "src/home/fsops.rs is the only module of the home scope that may name the store's lock: \
         {offenders:?}"
    );

    // And the entry points it offers take a `Root` and a `RelPath`: an absolute lock path, or a
    // path leaving the root, is not expressible.
    let env = support::home_env("lock-bounded");
    let data = Roots::from_vars(
        Some(OsString::from(env.home()).as_os_str()),
        None,
        None,
        Some(OsString::from(env.data()).as_os_str()),
    )
    .unwrap()
    .data_root();
    assert!(
        RelPath::new("/tmp/anywhere.lock").is_err(),
        "an absolute lock path is expressible"
    );
    fsops::mkdir_p(&data, &rel(lodi::home::state::HOME_SCOPE_DIR)).unwrap();
    let held = fsops::lock(&data, &rel(lodi::home::state::LOCK_FILE), true).unwrap();
    let path = env.data().join(lodi::home::state::LOCK_FILE);
    assert!(path.is_file(), "the lock file is not under the data root");
    assert!(
        fsops::try_lock(&data, &rel(lodi::home::state::LOCK_FILE), true)
            .unwrap()
            .is_none(),
        "a lock held by this process was handed out a second time"
    );
    drop(held);
    // A report's lock creates nothing: where there is no file there is no lock either.
    let other = support::home_env("lock-report");
    let empty = Roots::from_vars(
        Some(OsString::from(other.home()).as_os_str()),
        None,
        None,
        Some(OsString::from(other.data()).as_os_str()),
    )
    .unwrap()
    .data_root();
    assert!(
        fsops::lock_if_present(&empty, &rel(lodi::home::state::LOCK_FILE), false)
            .unwrap()
            .is_none()
    );
    assert!(
        !other.data().join(lodi::home::state::LOCK_FILE).exists(),
        "a report created the lock file"
    );
}

/// The whole of T-3 under the four proofs at once: a real `lodi switch --home` (1.x's
/// `lodi home apply`, #699) that creates files,
/// backs one up, drifts, refuses, takes the path back and then restores an original. Every line
/// its ledger gained is under the scratch roots, the decoy tree is untouched, and nothing the run
/// created sits outside the home root and the data root.
#[test]
fn an_apply_records_every_write_and_stays_inside_the_roots() {
    let gate = support::home_gate();
    let env = support::home_env("apply-contained");
    let decoy = env.decoy_listing();
    let before = gate.ledger_lines().len();

    let config = env.config().join("lodi");
    fs::create_dir_all(&config).unwrap();
    let manifest = |text: &str| fs::write(config.join("home.toml"), text).unwrap();
    let run = |args: &[&str]| env.lodi(args).output().unwrap();
    let (switch, preview) = (["switch", "--home"], ["switch", "--home", "--dry-run"]);

    manifest(
        "[home]\nversion = \"1\"\n\n\
         [home.file.\".config/git/ignore\"]\ntext = \".lodi/\\n\"\n\n\
         [home.file.\".inputrc\"]\ntext = \"lodi\\n\"\nmode = \"0600\"\non_remove = \"restore\"\n",
    );
    fs::write(env.home().join(".inputrc"), "the user's own\n").unwrap();
    assert_eq!(run(&switch).status.code(), Some(0));

    // Drift, a refusal, and a confirmed take-back.
    fs::write(env.home().join(".inputrc"), "edited\n").unwrap();
    assert_eq!(run(&switch).status.code(), Some(8));
    assert_eq!(run(&preview).status.code(), Some(0));
    assert_eq!(
        run(&["switch", "--home", "--overwrite-drift"])
            .status
            .code(),
        Some(0)
    );

    // The entry disappears, so the user's original comes back.
    manifest("[home]\nversion = \"1\"\n");
    assert_eq!(run(&switch).status.code(), Some(0));
    assert_eq!(
        fs::read_to_string(env.home().join(".inputrc")).unwrap(),
        "the user's own\n",
        "on_remove = restore did not put the original back"
    );

    // 2. The ledger. Tests of this file share one ledger and run in parallel, so the lines of
    // *this* run are the ones under this environment's own root — a unique prefix — and there
    // are lines: a run that wrote this much cannot have an empty ledger.
    let all = gate.ledger_lines();
    assert!(all.len() > before, "the ledger is empty after an apply");
    let mine: Vec<&(String, PathBuf)> = all
        .iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect();
    assert!(
        !mine.is_empty(),
        "the apply recorded nothing under its own root"
    );
    for (op, path) in &mine {
        assert!(
            path.is_absolute(),
            "a ledger line is not absolute: {path:?}"
        );
        assert!(
            path.starts_with(env.home()) || path.starts_with(env.data()),
            "the ledger records {op} {} outside the home and data roots",
            path.display()
        );
        let name = path.file_name().and_then(|name| name.to_str());
        assert!(
            !matches!(
                name,
                Some(".bashrc" | ".zshrc" | ".profile" | ".bash_profile" | "config.fish")
            ),
            "the ledger names a shell rc file: {}",
            path.display()
        );
    }
    // And nothing this run wrote landed outside the scratch roots altogether.
    for (op, path) in &all {
        assert!(
            path.starts_with(gate.root()),
            "the ledger records {op} {} outside the gate root",
            path.display()
        );
    }

    // 5. The lock exists, and it is where the constant says: under the data root `LODI_HOME`
    // names (#700 story 19).
    assert!(
        env.data()
            .join("lodi")
            .join(lodi::home::state::LOCK_FILE)
            .is_file(),
        "the apply lock is not under the data root"
    );

    // 3. And nothing escaped into the tree beside the roots.
    assert_eq!(env.decoy_listing(), decoy, "the decoy tree changed");
    assert!(
        listing(env.root()).contains(&"home".to_string()),
        "the scratch root lost its home"
    );
    let mut top = listing(env.root());
    top.sort();
    assert_eq!(
        top,
        vec!["data".to_string(), "decoy".to_string(), "home".to_string()],
        "the apply created something beside the roots"
    );
}

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/wait.rs"]
mod wait;

/// H3, the combined mode (LD-399): a switch of a config with a host and a home writes the home
/// through `src/home/fsops.rs` alone, and every write its ledger records is below the person's
/// home (`HOME`; the home part is the person's own, LD-498) or its `LODI_HOME`: none in the
/// config inside it.
#[test]
fn h3_a_combined_apply_writes_the_home_through_fsops_alone() {
    // Its own ledger, not the gate's: the gate's test holds every line of that one to its root.
    support::home_gate();
    let case = fakehost::Case::new("h3-combined-ledger", fakehost::Machine::debian());
    case.set_manifest("[host]\nversion = \"1\"\ndistro = \"debian\"\n");
    case.write_beside(
        "home.toml",
        "[home]\nversion = \"1\"\n\n[home.file.\"note\"]\ntext = \"home\\n\"\n\n\
         [home.xdg_config.\"demo/app\"]\ntext = \"owned\\n\"\n",
    );
    let home = case.root.path("home");
    fs::write(home.join("note"), "before\n").unwrap();
    let ledger = case.root.path("ledger.tsv");
    let named = ledger.display().to_string();
    let applied = case.verb_env("switch", &[], &[("LODI_FS_LEDGER", &named)]);
    assert_eq!(
        applied.status.code(),
        Some(0),
        "{}",
        fakehost::story(&applied)
    );
    assert_eq!(fs::read_to_string(home.join("note")).unwrap(), "home\n");
    let text = fs::read_to_string(&ledger).unwrap_or_default();
    let mine: Vec<(&str, PathBuf)> = text
        .lines()
        .filter_map(|line| line.split_once('\t'))
        .map(|(op, path)| (op, PathBuf::from(path)))
        .collect();
    assert!(
        !mine.is_empty(),
        "the combined apply recorded no home write"
    );
    let data = case.root.path("lodi-home");
    for (op, path) in &mine {
        assert!(
            (path.starts_with(&home) || path.starts_with(&data))
                && !path.starts_with(case.config()),
            "the ledger records {op} {} outside the home, or in the config",
            path.display()
        );
    }
}
