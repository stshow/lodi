//! `lodi home import` (M-Import T-4, rewritten by M-Home im-1 under
//! `docs/design/HOME_PROGRAMS.md` §7 and LD-341): the import reads no file in the home directory
//! and emits commented stubs, proved against the real binary over scratch home directories this
//! file builds itself.
//!
//! Every acceptance row of the package is here, and each is proved from **behaviour** rather
//! than from a mock:
//!
//! (a) over a scratch home holding a file at every path a shipped module writes or is shadowed
//!     by, with a scratch `PATH`, the emitted `home.toml` is byte for byte the committed
//!     `import/stubs/home.toml` fixture under `tests/fixtures/home`, a second run repeats it, and
//!     `--stdout` prints the same bytes;
//! (b) invariant I7: those planted files are full of markers — an address-shaped string, a
//!     token-shaped string — and unreadable (mode 0000); none of the markers reaches the output,
//!     no file's bytes, mode or presence change, the importer's source holds no read primitive,
//!     and the write ledger names nothing but the one manifest;
//! (c) a stub per shipped module whose executable is on `PATH`, and none for one that is not;
//!     git's `user.email` line is the literal line of §7; uncommented, a stub is a table the real
//!     `lodi home plan` accepts;
//! (d) `[tools]` is present and **entirely** commented, and the emitted file declares nothing:
//!     `lodi home apply --locked` over it succeeds and writes no lock;
//! (e) no `[files]` entry, no `files/`, no `W_UNCAPTURED` and nothing on standard error;
//! (f) the WHAT THIS FILE DOES NOT CARRY note lists, by `lstat` alone, exactly the planted
//!     paths below the home directory — a dangling symbolic link included;
//! (g) with no flag the manifest lands where `lodi home plan` reads it (LD-325), `--out DIR`
//!     writes `DIR/home.toml` and nothing else, an existing manifest is `E_EXISTS` naming the file
//!     until `--force`, and `--out` may not leave the home or overlap the scope's state.
//!
//! Offline and deterministic: no network, no Podman, no guest, no clock, and no read of the
//! machine's own home directory — `support::home_env` refuses to hand a child the ambient
//! `HOME`.
//!
//! Setting `LODI_RECORD_EXPECTED=1` rewrites the committed `import/stubs/home.toml` from the
//! built binary instead of comparing against it — the idiom of `tests/host_import_core.rs` — and
//! is never set by a validation command: an unset variable compares, which is what a gate runs.

#[path = "support/credentials.rs"]
mod credentials;
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Output;

use lodi::home::programs::{self, RenderEnv};
use support::{HomeEnv, home_env, home_gate};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

// The committed expectation, in two pieces: `AGENTS.md` §1.6's sanitization expression reads
// `home/<name>/` as a home directory path, so no file of this repository spells the whole path
// out — `tests/home_plan.rs` splits it the same way.
const FIXTURES: &str = "tests/fixtures/home";
const FIXTURE: &str = "import/stubs/home.toml";

/// What every planted file holds: an address-shaped string, a token-shaped string and a plain
/// marker. The address has no dot after the `@`, so no file of this repository carries one
/// (`AGENTS.md` §1.6); to the import it is exactly as secret as a real one. The token is put
/// together at run time, so no file carries a token prefix beside its body for a secret scanner.
fn markers() -> [String; 3] {
    [
        "lodi-marker@example".to_string(),
        ["ghp", "LODIMARKER0123456789abcdefABCDEF"].join("_"),
        "LODI-PLANTED-MARKER".to_string(),
    ]
}

// ----------------------------------------------------------------- the scratch homes ---

/// The directories every shipped module renders into, for this scratch environment.
fn render_env(env: &HomeEnv) -> RenderEnv {
    RenderEnv {
        home: env.home().to_path_buf(),
        xdg_config_home: env.config().to_path_buf(),
        lodi_data_root: env.data().join("lodi"),
        tools: false,
        starship_init: Default::default(),
    }
}

/// Every path a shipped module writes or is shadowed by, from the module table itself: the same
/// function the importer asks, so a module added to the registry is planted here too.
fn module_paths(env: &HomeEnv) -> Vec<(PathBuf, &'static str)> {
    let renv = render_env(env);
    let mut out = Vec::new();
    for module in programs::registry() {
        for path in programs::stub_paths(module, &renv) {
            out.push((path, module.name));
        }
    }
    out
}

/// A file of markers at every module path, unreadable by anyone: an import that opened one
/// would fail or leak a marker, and it does neither.
fn plant(env: &HomeEnv) {
    let body = format!("{}\n", markers().join("\n"));
    for (path, _) in module_paths(env) {
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, &body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o000)).unwrap();
    }
}

/// Planted files are mode 0000; the scratch root is removed by the next run of the same name,
/// so each test gives the files back a mode the harness can remove.
fn unplant(env: &HomeEnv) {
    for (path, _) in module_paths(env) {
        if fs::symlink_metadata(&path).is_ok_and(|m| m.is_file()) {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    }
}

/// A scratch `PATH` directory: `names` as executables, plus a catalogue name that is **not**
/// executable, so that each guess is proved to be the intersection it claims.
fn path_dir(env: &HomeEnv, names: &[&str]) -> PathBuf {
    let dir = env.root().join("bin");
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    for name in names {
        let path = dir.join(name);
        fs::write(&path, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let dull = dir.join("shellcheck");
    fs::write(&dull, "not executable\n").unwrap();
    fs::set_permissions(&dull, fs::Permissions::from_mode(0o644)).unwrap();
    dir
}

/// What the golden run has on `PATH`: bash, fish, git, helix (as `hx`, its executable) and tmux
/// of the shipped modules (zsh and the rest are left off, so a module whose executable is
/// missing is proved to get no stub), two catalogue tools and a name nobody knows.
const ON_PATH: &[&str] = &[
    "bash",
    "fish",
    "frobnicate-not-a-tool",
    "git",
    "hx",
    "jq",
    "ripgrep",
    "tmux",
];

fn import_with(env: &HomeEnv, names: &[&str], args: &[&str]) -> Output {
    let bin = path_dir(env, names);
    env.command()
        .args(["home", "import"])
        .args(args)
        .env("PATH", &bin)
        .output()
        .expect("the lodi binary runs")
}

fn import(env: &HomeEnv, args: &[&str]) -> Output {
    import_with(env, ON_PATH, args)
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

fn ok(out: Output) -> Output {
    assert_eq!(
        out.status.code(),
        Some(0),
        "the import failed:\n{}",
        stderr(&out)
    );
    assert_eq!(
        stderr(&out),
        "",
        "the home import printed on standard error (it warns about nothing)"
    );
    out
}

/// Every path below `root`, relative, with its mode and — where it may be read — its sha256, or
/// `dir`, or the target of a symbolic link. The planted files are mode 0000, so the census gives
/// them their size instead of their hash; the harness, not the import, is what reads them back.
fn census(root: &Path) -> BTreeMap<String, String> {
    fn walk(root: &Path, at: &Path, out: &mut BTreeMap<String, String>) {
        let Ok(entries) = fs::read_dir(at) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().display().to_string();
            let meta = fs::symlink_metadata(&path).unwrap();
            let mode = meta.permissions().mode() & 0o777;
            if meta.file_type().is_symlink() {
                out.insert(
                    rel,
                    format!("link -> {}", fs::read_link(&path).unwrap().display()),
                );
            } else if meta.is_dir() {
                out.insert(rel, format!("dir {mode:04o}"));
                walk(root, &path, out);
            } else if mode == 0 {
                out.insert(rel, format!("0000 {} bytes", meta.len()));
            } else {
                out.insert(
                    rel,
                    format!(
                        "{mode:04o} {}",
                        lodi::util::sha256_hex(&fs::read(&path).unwrap())
                    ),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

fn ledger_under(env: &HomeEnv) -> Vec<(String, PathBuf)> {
    home_gate()
        .ledger_lines()
        .into_iter()
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect()
}

/// The configuration root `lodi home plan` reads: `$XDG_CONFIG_HOME/lodi`.
fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn fixture() -> String {
    fs::read_to_string(repo().join(FIXTURES).join(FIXTURE)).unwrap_or_else(|e| {
        panic!("{FIXTURES}/{FIXTURE} is the committed expectation of this test and must exist: {e}")
    })
}

/// Run `lodi home <verb>` in the same scratch roots, with nothing on `PATH`.
fn home(env: &HomeEnv, args: &[&str]) -> Output {
    env.command()
        .arg("home")
        .args(args)
        .env("PATH", "")
        .output()
        .expect("the lodi binary runs")
}

// -------------------------------------------- (a) the bytes, the second run, --stdout ---

#[test]
fn the_emitted_manifest_is_the_committed_bytes_and_every_form_repeats_them() {
    let env = home_env("import-bytes");
    plant(&env);

    ok(import(&env, &["--out", "starter"]));
    let written = fs::read_to_string(env.home().join("starter/home.toml")).unwrap();
    if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
        fs::write(repo().join(FIXTURES).join(FIXTURE), &written).expect("the fixture");
    }
    assert_eq!(
        written,
        fixture(),
        "the emitted home.toml is not the committed bytes of {FIXTURES}/{FIXTURE}"
    );

    // A second run, with --force, is byte for byte the first: no date, no version, no ordering
    // that depends on the filesystem.
    ok(import(&env, &["--out", "starter", "--force"]));
    let again = fs::read_to_string(env.home().join("starter/home.toml")).unwrap();
    assert_eq!(again, written, "a second import produced different bytes");

    // --stdout prints the same manifest.
    let printed = ok(import(&env, &["--stdout"]));
    assert_eq!(stdout(&printed), written, "--stdout printed other bytes");

    // No user, no host, no absolute path: the scratch roots appear nowhere in it.
    for root in [env.root(), env.home(), env.config(), env.data()] {
        assert!(
            !written.contains(&root.display().to_string()),
            "the manifest names {}",
            root.display()
        );
    }
    assert!(!written.contains("/home/"), "{written}");
    unplant(&env);
}

// ------------------------------------------------------------------------ (b) I7 ---

/// Invariant I7: every path a module writes or is shadowed by holds an unreadable file full of
/// markers, and the import reads none of them — the output holds no marker, nothing in the home
/// changed, and the ledger names only the manifest the run wrote.
#[test]
fn the_import_reads_no_file_and_never_emits_an_address() {
    let env = home_env("import-i7");
    plant(&env);
    path_dir(&env, ON_PATH);
    let before = census(env.root());
    let decoy = env.decoy_listing();

    let out = ok(import(&env, &[]));
    let manifest = fs::read_to_string(config_root(&env).join("home.toml")).unwrap();
    for text in [&manifest, &stdout(&out), &stderr(&out)] {
        for marker in markers() {
            assert!(
                !text.contains(&marker),
                "{marker} reached the output:\n{text}"
            );
        }
        assert!(
            !text.contains('@'),
            "an address-shaped string is in the output:\n{text}"
        );
    }

    // Nothing that was there moved: the planted files have their bytes (size), mode and place.
    let after = census(env.root());
    for (path, what) in &before {
        assert_eq!(after.get(path), Some(what), "{path} changed");
    }
    let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(
        added,
        ["home/.config/lodi", "home/.config/lodi/home.toml"],
        "the import wrote more than its manifest"
    );
    assert_eq!(decoy, env.decoy_listing(), "the decoy tree was touched");

    // The ledger of `fsops`, which every home-scope mutation goes through, names the manifest
    // and its directory and nothing else.
    let ledger: Vec<(String, PathBuf)> = ledger_under(&env);
    assert_eq!(
        ledger,
        vec![
            ("mkdir".to_string(), config_root(&env)),
            ("write".to_string(), config_root(&env).join("home.toml")),
        ],
        "the ledger names more than the manifest"
    );
    unplant(&env);
}

/// The source lint half of I7: the importer holds no primitive that reads a file's content,
/// lists a directory or copies one, so "reads no file" is a property of the code and not only of
/// the scratch homes above. `symlink_metadata` (`lstat`) and `metadata` (the `PATH` test) are
/// the only questions it may ask of the filesystem.
#[test]
fn the_importer_holds_no_read_primitive() {
    let source = fs::read_to_string(repo().join("src/home/import.rs")).unwrap();
    let code: String = source
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "fsops::read",
        "fs::read",
        "read_to_string",
        "read_dir",
        "File::open",
        "OpenOptions",
        "fsops::entries",
        "fsops::copy",
        "fs::copy",
        "read_link",
        "canonicalize",
    ] {
        assert!(
            !code.contains(forbidden),
            "src/home/import.rs uses `{forbidden}`"
        );
    }
    for gone in ["ALLOWLIST", "REFUSALS", "W_UNCAPTURED", "\"files\""] {
        assert!(!code.contains(gone), "src/home/import.rs still has {gone}");
    }
}

// ------------------------------------------------------------------ (c) the stubs ---

#[test]
fn a_stub_per_shipped_module_on_path_and_none_for_one_that_is_not() {
    let env = home_env("import-stubs");
    let out = ok(import(&env, &["--stdout"]));
    let manifest = stdout(&out);
    for module in programs::registry() {
        let header = format!("# [programs.{}]\n", module.name);
        let on = ON_PATH.contains(&module.executable);
        assert_eq!(
            manifest.contains(&header),
            on,
            "{}: on PATH is {on}, and the stub is {}there",
            module.name,
            if on { "not " } else { "" }
        );
        if on {
            assert!(
                manifest.contains(&format!("{header}{}", module.stub)),
                "{}'s stub is not emitted whole",
                module.name
            );
        }
    }
    assert!(
        manifest.contains("# user.email = \"\"  # your address; lodi never imports it\n"),
        "git's user.email line is not the literal line of §7:\n{manifest}"
    );

    // With nothing on PATH there is no stub, and the file says why.
    let bare = stdout(&ok(import_with(&env, &[], &["--stdout"])));
    assert!(!bare.contains("# [programs."), "{bare}");
    let shipped: Vec<&str> = programs::registry().iter().map(|m| m.name).collect();
    assert!(
        bare.contains(&format!("({})", shipped.join(", "))),
        "the empty programs block does not name the shipped modules:\n{bare}"
    );
}

/// Uncommented, each stub is a table the real `lodi home plan` accepts: it plans the files the
/// module renders, and exits 0.
#[test]
fn every_stub_uncommented_is_a_table_home_plan_accepts() {
    for module in programs::registry() {
        let env = home_env(&format!("import-stub-{}", module.name));
        let mut manifest = format!("[home]\nversion = \"1\"\n\n[programs.{}]\n", module.name);
        for line in module.stub.lines() {
            manifest.push_str(line.strip_prefix("# ").expect("a stub line is commented"));
            manifest.push('\n');
        }
        fs::create_dir_all(config_root(&env)).unwrap();
        fs::write(config_root(&env).join("home.toml"), &manifest).unwrap();
        // The shell modules render into the data root, which a module may only do below the
        // home directory (§4.1), so this run puts it there.
        let data = env.home().join(".local/share");
        let plan = env
            .command()
            .args(["home", "plan"])
            .env("PATH", "")
            .env("XDG_DATA_HOME", &data)
            .env("LODI_HOME", data.join("lodi"))
            .output()
            .expect("the lodi binary runs");
        assert_eq!(
            plan.status.code(),
            Some(0),
            "[programs.{}] uncommented is refused:\n{}",
            module.name,
            stderr(&plan)
        );
        let text = stdout(&plan);
        assert!(
            text.contains(&format!("(programs.{})", module.name)),
            "[programs.{}] uncommented plans nothing:\n{text}",
            module.name
        );
    }
}

// --------------------------------------------- (d) [tools], and a file that declares nothing ---

#[test]
fn the_tools_block_is_commented_and_the_file_declares_nothing() {
    let env = home_env("import-tools");
    plant(&env);
    ok(import(&env, &[]));
    let manifest = fs::read_to_string(config_root(&env).join("home.toml")).unwrap();

    assert!(
        manifest.contains("# [tools]\n"),
        "there is no commented [tools] block:\n{manifest}"
    );
    // The guess is the catalogue intersected with PATH.
    assert!(manifest.contains("# jq = \"latest\"\n"), "{manifest}");
    assert!(manifest.contains("# ripgrep = \"latest\"\n"), "{manifest}");
    assert!(!manifest.contains("shellcheck"), "{manifest}");
    assert!(!manifest.contains("frobnicate"), "{manifest}");

    // Every line that is not a comment is the one table the loader needs.
    let live: Vec<&str> = manifest
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(live, ["[home]", "version = \"1\""], "{manifest}");

    // The proof from behaviour: a plan finds nothing to do, and `apply --locked` succeeds and
    // writes no lock — one live [tools] line with no `home.lock` would be `E_LOCK_STALE`.
    let plan = home(&env, &["plan"]);
    assert_eq!(plan.status.code(), Some(0), "{}", stderr(&plan));
    assert_eq!(stdout(&plan), "0 files: nothing to do\n");
    let apply = home(&env, &["apply", "--locked"]);
    assert_eq!(apply.status.code(), Some(0), "{}", stderr(&apply));
    assert!(!config_root(&env).join("home.lock").exists());
    unplant(&env);
}

// ------------------------------------------------ (e) no bundle, no W_UNCAPTURED ---

#[test]
fn there_is_no_files_entry_no_bundle_and_no_uncaptured_warning() {
    let env = home_env("import-no-bundle");
    plant(&env);
    for args in [&[][..], &["--out", "starter"][..], &["--stdout"][..]] {
        let out = ok(import(&env, args));
        let text = format!("{}{}", stdout(&out), stderr(&out));
        assert!(!text.contains("W_UNCAPTURED"), "{text}");
    }
    for dir in [config_root(&env), env.home().join("starter")] {
        let manifest = fs::read_to_string(dir.join("home.toml")).unwrap();
        assert!(!manifest.contains("[files"), "{manifest}");
        assert!(!manifest.contains("NOT CAPTURED"), "{manifest}");
        assert!(
            !dir.join("files").exists(),
            "{} has a files/",
            dir.display()
        );
        let names: Vec<String> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["home.toml"], "{}", dir.display());
    }
    unplant(&env);
}

/// The credential rule of the home import (LD-335 over LD-341): the import reads no file's
/// content, so no credential can be captured and none needs refusing by name. Proved per shape:
/// each shape of `tests/support/credentials.rs` is planted **on its own and readable** in every
/// file the import asks about (every module path) and in the dotfiles a person keeps one in
/// (`.netrc`, `.inputrc`, an editor's `init.vim`, a tool's YAML), and the emitted manifest is
/// byte for byte the one emitted over the same files holding a benign configuration — the
/// negative control — so no byte of any file reaches it. No fake value is in any output, there
/// is nothing on standard error (there is no capture to refuse or warn about), the only table
/// left uncommented is `[home]` (no `[home.file]` capture), and no planted file changed. The
/// code half of the same negative is `the_importer_holds_no_read_primitive`.
#[test]
fn a_readable_credential_in_any_shape_never_reaches_the_home_import() {
    let env = home_env("import-credentials");
    let mut planted = vec![
        env.home().join(".config/nvim/init.vim"),
        env.home().join(".inputrc"),
        env.home().join(".netrc"),
        env.home().join(".config/some-tool/config.yaml"),
    ];
    planted.extend(module_paths(&env).into_iter().map(|(path, _)| path));
    let fill = |text: &str| {
        for path in &planted {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
    };

    // The negative control: the same files, holding a benign configuration.
    fill("# benign settings\nset editing-mode vi\ncolor = auto\n");
    let benign = stdout(&ok(import(&env, &["--stdout"])));
    let tables: Vec<&str> = benign
        .lines()
        .filter(|line| line.trim_start().starts_with('['))
        .collect();
    assert_eq!(
        tables,
        ["[home]"],
        "the home import emitted a live table beyond [home] — a capture:\n{benign}"
    );
    assert!(
        benign
            .replace("\n# ", " ")
            .contains("It copied nothing and read no file in your home directory"),
        "the header no longer says what the import guarantees"
    );

    for shape in credentials::shapes() {
        fill(&shape.text);
        let before = census(env.root());
        let out = ok(import(&env, &["--stdout"]));
        let manifest = stdout(&out);
        assert!(
            !manifest.contains(&shape.fake) && !stderr(&out).contains(&shape.fake),
            "{}: the fake value reached the home import's output:\n{manifest}",
            shape.name
        );
        assert_eq!(
            manifest, benign,
            "{}: a file's content changed what the home import emits",
            shape.name
        );
        assert_eq!(before, census(env.root()), "{}: a file changed", shape.name);
    }

    // The default form lands the same bytes where `lodi home plan` reads them.
    ok(import(&env, &[]));
    assert_eq!(
        fs::read_to_string(config_root(&env).join("home.toml")).unwrap(),
        benign,
        "the landed manifest is not the --stdout bytes"
    );
}

// ------------------------------------------------ (f) WHAT THIS FILE DOES NOT CARRY ---

/// The note lists exactly the module paths below the home directory that hold something — the
/// planted ones, in registry order — and nothing in lodi's own data root. A dangling symbolic
/// link counts: the question is `lstat`'s, and nothing is followed.
#[test]
fn the_note_lists_exactly_the_paths_that_exist_by_lstat() {
    let env = home_env("import-note");
    let manifest = stdout(&ok(import(&env, &["--stdout"])));
    assert!(
        manifest.contains("# WHAT THIS FILE DOES NOT CARRY\n"),
        "{manifest}"
    );
    assert!(
        manifest.contains("#   none of those paths holds a file in this home directory.\n"),
        "{manifest}"
    );

    plant(&env);
    // One path is a dangling symbolic link instead of a file.
    let (linked, _) = module_paths(&env)
        .into_iter()
        .find(|(p, _)| p.starts_with(env.home()) && !p.starts_with(env.data()))
        .unwrap();
    fs::remove_file(&linked).unwrap();
    std::os::unix::fs::symlink("nowhere-at-all", &linked).unwrap();

    let manifest = stdout(&ok(import(&env, &["--stdout"])));
    let listed: Vec<&str> = manifest
        .lines()
        .filter(|l| l.starts_with("#   ") && l.contains(" exists; "))
        .collect();
    let want: Vec<String> = module_paths(&env)
        .into_iter()
        .filter(|(p, _)| p.starts_with(env.home()))
        .map(|(p, name)| {
            format!(
                "#   {} exists; translate what you need into [programs.{name}]",
                p.strip_prefix(env.home()).unwrap().display()
            )
        })
        .collect();
    assert_eq!(listed, want, "{manifest}");
    assert!(
        manifest.contains(".gitconfig exists; translate what you need into [programs.git]"),
        "the shadowing path of §4.6 is not listed:\n{manifest}"
    );
    assert!(!manifest.contains("none of those paths"), "{manifest}");
    unplant(&env);
}

// --------------------------------------------------------- (g) where it goes, and --force ---

/// With no flag the manifest lands in the configuration root, one line names it and the next
/// step, and the plan that follows reads it.
#[test]
fn with_no_flag_the_manifest_lands_where_home_plan_reads_it() {
    let env = home_env("import-in-place");
    let out = ok(import(&env, &[]));
    assert_eq!(
        stdout(&out),
        "wrote ~/.config/lodi/home.toml; next: uncomment what you want, then lodi home plan\n"
    );
    let manifest = config_root(&env).join("home.toml");
    assert_eq!(
        fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
        0o644
    );
    fs::write(
        &manifest,
        format!(
            "{}\n[home.file.\"note.txt\"]\ntext = \"hello\\n\"\n",
            fs::read_to_string(&manifest).unwrap()
        ),
    )
    .unwrap();
    let plan = home(&env, &["plan"]);
    assert_eq!(plan.status.code(), Some(0), "{}", stderr(&plan));
    assert!(
        stdout(&plan).contains("create    note.txt"),
        "the plan did not read the manifest that landed:\n{}",
        stdout(&plan)
    );
}

/// An existing manifest is `E_EXISTS`, naming the file and `--force`, and nothing moves;
/// `--force` replaces it. A `files/` an older import left beside it is never touched.
#[test]
fn an_existing_manifest_is_refused_until_force_replaces_it() {
    let env = home_env("import-exists");
    for (args, shown) in [
        (&[][..], "~/.config/lodi/home.toml"),
        (&["--out", "starter"][..], "starter/home.toml"),
    ] {
        ok(import(&env, args));
        let dir = if args.is_empty() {
            config_root(&env)
        } else {
            env.home().join("starter")
        };
        fs::create_dir_all(dir.join("files")).unwrap();
        fs::write(dir.join("files/.inputrc"), "set editing-mode vi\n").unwrap();
        fs::write(dir.join("home.toml"), "# an older manifest\n").unwrap();
        let before = census(env.root());

        let refused = import(&env, args);
        assert_eq!(refused.status.code(), Some(3), "{}", stderr(&refused));
        let text = stderr(&refused);
        assert!(text.contains("E_EXISTS") && text.contains(shown), "{text}");
        assert!(text.contains("--force"), "{text}");
        assert_eq!(
            census(env.root()),
            before,
            "the refused run wrote something"
        );

        let mut forced = args.to_vec();
        forced.push("--force");
        ok(import(&env, &forced));
        assert_ne!(
            fs::read_to_string(dir.join("home.toml")).unwrap(),
            "# an older manifest\n"
        );
        assert_eq!(
            fs::read_to_string(dir.join("files/.inputrc")).unwrap(),
            "set editing-mode vi\n",
            "--force touched an older files/"
        );
    }
}

/// `--out DIR` writes `DIR/home.toml` and nothing else anywhere: a census of the whole scratch
/// root and the write ledger.
#[test]
fn out_writes_its_one_file_and_nothing_else() {
    let env = home_env("import-census");
    plant(&env);
    path_dir(&env, ON_PATH);
    let before = census(env.root());
    let decoy = env.decoy_listing();

    let out = ok(import(&env, &["--out", "starter"]));
    assert_eq!(
        stdout(&out),
        "wrote starter/home.toml\n\
         next: copy it into ~/.config/lodi/, uncomment what you want, and run `lodi home plan`\n"
    );
    let after = census(env.root());
    let added: Vec<&String> = after.keys().filter(|k| !before.contains_key(*k)).collect();
    assert_eq!(added, ["home/starter", "home/starter/home.toml"]);
    for (path, what) in &before {
        assert_eq!(after.get(path), Some(what), "{path} changed");
    }
    assert_eq!(decoy, env.decoy_listing(), "the decoy tree was touched");
    let ledger = ledger_under(&env);
    assert!(!ledger.is_empty(), "fsops recorded no write at all");
    for (op, path) in &ledger {
        assert!(
            path.starts_with(env.home().join("starter")),
            "fsops recorded `{op}` on {}, outside --out",
            path.display()
        );
    }
    unplant(&env);
}

/// `--out` may not leave the home directory, and may not overlap the scope's own state — the two
/// refusals that keep "an import writes nothing lodi owns" true (design call D6).
#[test]
fn an_out_directory_outside_the_home_or_inside_the_scope_state_is_refused() {
    let env = home_env("import-out");
    path_dir(&env, ON_PATH);
    let before = census(env.root());
    let outside = env.root().join("outside").display().to_string();

    for (args, code, want) in [
        (vec!["--out", outside.as_str()], 3, "E_PATH_ESCAPE"),
        (vec!["--out", "../escape"], 3, "E_PATH_ESCAPE"),
        (vec!["--out", "/etc"], 3, "E_PATH_ESCAPE"),
        (vec!["--out", ".config/lodi"], 6, "E_STORE_IO"),
        (vec!["--out", ".config/lodi/starter"], 6, "E_STORE_IO"),
        (vec!["--out", ".config"], 6, "E_STORE_IO"),
    ] {
        let out = import(&env, &args);
        assert_eq!(
            out.status.code(),
            Some(code),
            "`lodi home import {}`:\n{}",
            args.join(" "),
            stderr(&out)
        );
        assert!(
            stderr(&out).contains(want),
            "`lodi home import {}` is not {want}:\n{}",
            args.join(" "),
            stderr(&out)
        );
    }
    assert_eq!(
        census(env.root()),
        before,
        "a refused --out wrote something"
    );
    assert!(!env.root().join("outside").exists());
}

#[test]
fn the_usage_errors_are_usage_errors() {
    let env = home_env("import-usage");
    for args in [
        vec!["--out"],
        vec!["extra", "leftover"],
        vec!["--out", "a", "--out", "b"],
        vec!["--force", "--force"],
        vec!["--frobnicate"],
        vec!["--out", "starter", "leftover"],
        vec!["--stdout", "--stdout"],
        vec!["--stdout", "--out", "starter"],
        vec!["--out", "starter", "--stdout"],
        vec!["--stdout", "--force"],
    ] {
        let out = import(&env, &args);
        assert_eq!(
            out.status.code(),
            Some(2),
            "`lodi home import {}` is not a usage error:\n{}",
            args.join(" "),
            stderr(&out)
        );
    }
    assert!(!env.home().join("starter").exists());
    assert!(!config_root(&env).exists());
}

// ------------------------------------ the import-then-apply path adopts identical files ---

/// The import-then-apply path for a file the home directory already holds: import, declare the
/// file by hand under `[home.file]` with the bytes and mode it already has, and the first apply
/// **adopts** it — the plan says `adopt`, the apply prints one `adopted` line, records the path
/// and writes no byte (same inode, same mtime, nothing in the write ledger) — so status reports
/// it `ok`, a second apply has nothing to do, a hand edit is `drift`, and an apply refuses it with
/// `E_DRIFT` like any managed file.
#[test]
fn an_imported_manifest_adopts_a_hand_declared_identical_file_and_then_manages_it() {
    use std::os::unix::fs::MetadataExt;

    let env = home_env("import-adopt");
    let target = env.home().join(".inputrc");
    fs::write(&target, "set editing-mode vi\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let before = fs::metadata(&target).unwrap();

    ok(import(&env, &[]));
    let manifest = config_root(&env).join("home.toml");
    fs::write(
        &manifest,
        format!(
            "{}\n[home.file.\".inputrc\"]\ntext = \"set editing-mode vi\\n\"\n",
            fs::read_to_string(&manifest).unwrap()
        ),
    )
    .unwrap();

    let plan = home(&env, &["plan"]);
    assert_eq!(plan.status.code(), Some(0), "{}", stderr(&plan));
    assert!(
        stdout(&plan).contains("adopt     .inputrc                  0644"),
        "{}",
        stdout(&plan)
    );

    let apply = home(&env, &["apply"]);
    assert_eq!(apply.status.code(), Some(0), "{}", stderr(&apply));
    let applied = stdout(&apply);
    let adopted: Vec<&str> = applied
        .lines()
        .filter(|line| line.starts_with("adopted"))
        .collect();
    assert_eq!(adopted.len(), 1, "{applied}");
    assert!(
        adopted[0].starts_with("adopted   .inputrc"),
        "{}",
        adopted[0]
    );
    assert!(
        !stderr(&apply).contains("W_REPLACED_UNMANAGED"),
        "{}",
        stderr(&apply)
    );
    let after = fs::metadata(&target).unwrap();
    assert_eq!((after.ino(), after.dev()), (before.ino(), before.dev()));
    assert_eq!(
        (after.mtime(), after.mtime_nsec()),
        (before.mtime(), before.mtime_nsec())
    );
    assert!(
        ledger_under(&env).iter().all(|(_, path)| path != &target),
        "adopt wrote to the file: {:?}",
        ledger_under(&env)
    );
    let state = fs::read_to_string(env.data().join("lodi/home-scope/state.json")).unwrap();
    assert!(state.contains("\".inputrc\""), "{state}");

    let status = home(&env, &["status"]);
    assert!(
        stdout(&status).contains("ok           .inputrc"),
        "{}",
        stdout(&status)
    );
    let again = home(&env, &["apply"]);
    assert_eq!(again.status.code(), Some(0), "{}", stderr(&again));
    assert!(
        stdout(&again).contains("nothing to do"),
        "{}",
        stdout(&again)
    );

    fs::write(&target, "set editing-mode emacs\n").unwrap();
    let status = home(&env, &["status"]);
    assert!(
        stdout(&status).contains("drift        .inputrc"),
        "{}",
        stdout(&status)
    );
    let refused = home(&env, &["apply"]);
    assert_eq!(refused.status.code(), Some(8), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("E_DRIFT"), "{}", stderr(&refused));
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "set editing-mode emacs\n"
    );
}
