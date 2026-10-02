//! The home template `lodi import --home` writes (`src/home/import.rs`, M-Home im-1 under
//! `docs/design/HOME_PROGRAMS.md` §7 and LD-341): it reads no file in the home directory and
//! emits commented stubs, proved against the real binary over scratch homes (LD-518).
//!
//! The rows of 1.x's `lodi home import` that 2.0 keeps run as `lodi import --home` and
//! `lodi switch --home` on the home gate's scratch machine: invariant I7 (planted unreadable
//! files full of markers are never read, and nothing but the config is written), the commented
//! `[tools]` block of a file that declares nothing, no `[files]` bundle, the credential rule, the
//! WHAT THIS FILE DOES NOT CARRY note, where the file lands, that it is the library's template
//! and a second import keeps it, an identical file adopted, and the committed bytes of
//! `import/stubs/home.toml` over a machine of the test's own whose `/usr/bin` the template looks
//! in (LD-524). Not here: `--out`, `--stdout` and `--force`, which 2.0's import does not have.

#[path = "support/credentials.rs"]
mod credentials;
mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use lodi::home::programs::{self, RenderEnv};
use support::{HomeEnv, home_env, home_gate, home_part, nothing};

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).expect("stderr is UTF-8")
}

/// The config `lodi import --home` writes and `lodi switch --home` reads: `~/.config/lodi`.
fn config_root(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

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

/// The directories every shipped module renders into, as `lodi switch` gives them.
fn render_env(env: &HomeEnv) -> RenderEnv {
    RenderEnv {
        home: env.home().to_path_buf(),
        xdg_config_home: env.config().to_path_buf(),
        lodi_data_root: env.share().join("lodi"),
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

/// `lodi ARGS` (a 1.x home verb read as `switch --home`) with nothing on standard input.
fn lodi(env: &HomeEnv, args: &[&str]) -> Output {
    env.lodi(args)
        .stdin(Stdio::null())
        .output()
        .expect("the lodi binary runs")
}

/// `lodi import --home`, which must succeed.
fn import(env: &HomeEnv) -> Output {
    let out = lodi(env, &["import", "--home"]);
    assert_eq!(
        out.status.code(),
        Some(0),
        "the import failed:\n{}",
        stderr(&out)
    );
    out
}

fn manifest(env: &HomeEnv) -> String {
    fs::read_to_string(config_root(env).join("home.toml")).unwrap()
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

/// What an import may add besides the config: the folders above it and its run's log.
fn the_config_or_its_log(rel: &str) -> bool {
    [".config/lodi", ".local/state"]
        .iter()
        .any(|kept| Path::new(rel).starts_with(Path::new("home").join(kept)))
        || ["home/.config", "home/.local"].contains(&rel)
}

/// The source lint half of I7: the importer holds no primitive that reads a file's content,
/// lists a directory or copies one, so "reads no file" is a property of the code.
/// `symlink_metadata` (`lstat`) and `metadata` (the `PATH` test) are the only questions it may
/// ask of the filesystem.
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

// ------------------------------------------------- the committed bytes, on a machine ---

/// What the machine has in its `/usr/bin` for the golden run: bash, fish, git, helix (as `hx`,
/// its executable) and tmux of the shipped modules (zsh and the rest are left off, so a module
/// whose executable is missing is proved to get no stub), two catalogue tools, a name nobody
/// knows, and `shellcheck` there but not executable.
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

/// `lodi import --home` on a machine of the test's own at `env.root()` (its `--root`), whose
/// `/usr/bin` holds [`ON_PATH`]: the template looks there, never at the real `/usr/bin` (LD-524).
fn import_on_machine(env: &HomeEnv) -> Output {
    support::machine_at(env.root());
    let bin = env.root().join("usr/bin");
    fs::create_dir_all(&bin).unwrap();
    for name in ON_PATH {
        fs::write(bin.join(name), "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(bin.join(name), fs::Permissions::from_mode(0o755)).unwrap();
    }
    fs::write(bin.join("shellcheck"), "not executable\n").unwrap();
    env.command()
        .args(["import", "--home", "--root"])
        .arg(env.root())
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .env("GIT_CEILING_DIRECTORIES", env.root())
        .stdin(Stdio::null())
        .output()
        .expect("the lodi binary runs")
}

/// Over a home with a file at every path a shipped module writes or is shadowed by, the written
/// `home.toml` is byte for byte the committed `import/stubs/home.toml`, and names no scratch
/// path. `LODI_RECORD_EXPECTED=1` records it from the built binary instead of comparing.
#[test]
fn the_written_manifest_is_the_committed_bytes() {
    let env = home_env("import-bytes");
    plant(&env);
    let out = import_on_machine(&env);
    unplant(&env);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let written = manifest(&env);
    let fixture = repo()
        .join("tests/fixtures/home")
        .join("import/stubs/home.toml");
    if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
        fs::write(&fixture, &written).expect("the fixture");
    }
    assert_eq!(written, fs::read_to_string(&fixture).unwrap());
    for root in [env.root(), env.home(), env.config(), &env.share()] {
        assert!(!written.contains(&root.display().to_string()), "{written}");
    }
}

// ------------------------------------------------------------------ (c) the stubs ---

/// Uncommented, each stub is a table the real `lodi switch --home` accepts: its preview plans
/// the files the module renders, and exits 0.
#[test]
fn every_stub_uncommented_is_a_table_the_switch_accepts() {
    for module in programs::registry() {
        let env = home_env(&format!("import-stub-{}", module.name));
        let mut manifest = format!("[home]\nversion = \"1\"\n\n[programs.{}]\n", module.name);
        for line in module.stub.lines() {
            manifest.push_str(line.strip_prefix("# ").expect("a stub line is commented"));
            manifest.push('\n');
        }
        fs::create_dir_all(config_root(&env)).unwrap();
        fs::write(config_root(&env).join("home.toml"), &manifest).unwrap();
        // A module renders only below the home: the data and config folders the variables
        // would move outside it are left at their defaults.
        let plan = env
            .lodi(&["home", "plan"])
            .env("PATH", "")
            .env_remove("LODI_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .output()
            .expect("the lodi binary runs");
        assert_eq!(
            plan.status.code(),
            Some(0),
            "[programs.{}] uncommented is refused:\n{}",
            module.name,
            stderr(&plan)
        );
        let text = home_part(&plan);
        assert!(
            text.contains(&format!("(programs.{})", module.name)),
            "[programs.{}] uncommented plans nothing:\n{text}",
            module.name
        );
    }
}

// ------------------------------------------------------------------------ (b) I7 ---

/// Invariant I7: every path a module writes or is shadowed by holds an unreadable file full of
/// markers, and the import reads none of them — the output holds no marker, nothing in the home
/// changed, and the ledger names nothing outside the config and the run's log.
#[test]
fn the_import_reads_no_file_and_never_emits_an_address() {
    let env = home_env("import-i7");
    plant(&env);
    let before = census(env.root());
    let decoy = env.decoy_listing();

    let out = import(&env);
    let written = manifest(&env);
    for text in [&written, &stdout(&out), &stderr(&out)] {
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
    // No user, no host, no absolute path: the scratch roots appear nowhere in the file.
    for root in [env.root(), env.home()] {
        assert!(
            !written.contains(&root.display().to_string()),
            "the manifest names {}",
            root.display()
        );
    }

    // Nothing that was there moved: the planted files have their bytes (size), mode and place.
    let after = census(env.root());
    for (path, what) in &before {
        assert_eq!(after.get(path), Some(what), "{path} changed");
    }
    let added: Vec<&String> = after
        .keys()
        .filter(|k| !before.contains_key(*k) && !the_config_or_its_log(k))
        .collect();
    assert!(
        added.is_empty(),
        "the import wrote more than its config: {added:?}"
    );
    assert!(after.contains_key("home/.config/lodi/home.toml"));
    assert_eq!(decoy, env.decoy_listing(), "the decoy tree was touched");
    for (op, path) in ledger_under(&env) {
        assert!(
            path.starts_with(config_root(&env)),
            "fsops recorded `{op}` on {}, outside the config",
            path.display()
        );
    }
    unplant(&env);
}

// --------------------------------------------- (d) [tools], and a file that declares nothing ---

/// `[tools]` is present and entirely commented, and the file declares nothing: the preview finds
/// nothing to do, and a switch over it succeeds with nothing to do.
#[test]
fn the_tools_block_is_commented_and_the_file_declares_nothing() {
    let env = home_env("import-tools");
    plant(&env);
    import(&env);
    let written = manifest(&env);
    assert!(
        written.contains("# [tools]"),
        "there is no commented [tools] block:\n{written}"
    );
    // Every line that is not a comment is the one table the loader needs.
    let live: Vec<&str> = written
        .lines()
        .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
        .collect();
    assert_eq!(live, ["[home]", "version = \"1\""], "{written}");

    let plan = lodi(&env, &["home", "plan"]);
    assert!(nothing(&plan), "{}", stderr(&plan));
    let apply = lodi(&env, &["home", "apply"]);
    assert!(nothing(&apply), "{}", stderr(&apply));
    unplant(&env);
}

// ------------------------------------------------ (e) no bundle, no W_UNCAPTURED ---

#[test]
fn there_is_no_files_entry_no_bundle_and_no_uncaptured_warning() {
    let env = home_env("import-no-bundle");
    plant(&env);
    let out = import(&env);
    let text = format!("{}{}", stdout(&out), stderr(&out));
    assert!(!text.contains("W_UNCAPTURED"), "{text}");
    let written = manifest(&env);
    assert!(!written.contains("[files"), "{written}");
    assert!(!written.contains("NOT CAPTURED"), "{written}");
    let names: Vec<String> = fs::read_dir(config_root(&env))
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".git")
        .collect();
    assert_eq!(
        names,
        ["home.toml"],
        "the config holds more than its manifest"
    );
    unplant(&env);
}

/// The credential rule of the home import (LD-335 over LD-341): the import reads no file's
/// content, so no credential can be captured and none needs refusing by name. Each shape of
/// `tests/support/credentials.rs` is planted on its own and readable in every file the import
/// asks about and in the dotfiles a person keeps one in, and the written manifest is byte for
/// byte the one written over the same files holding a benign configuration — the negative
/// control — so no byte of any file reaches it, and no planted file changed.
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
    let fresh = |env: &HomeEnv| {
        let _ = fs::remove_dir_all(config_root(env));
        let out = import(env);
        (manifest(env), out)
    };

    // The negative control: the same files, holding a benign configuration.
    fill("# benign settings\nset editing-mode vi\ncolor = auto\n");
    let (benign, _) = fresh(&env);
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
        let before = census(env.home());
        let (written, out) = fresh(&env);
        assert!(
            !written.contains(&shape.fake) && !stderr(&out).contains(&shape.fake),
            "{}: the fake value reached the home import's output:\n{written}",
            shape.name
        );
        assert_eq!(
            written, benign,
            "{}: a file's content changed what the home import emits",
            shape.name
        );
        let after = census(env.home());
        for (path, what) in &before {
            if !the_config_or_its_log(&format!("home/{path}")) {
                assert_eq!(
                    after.get(path),
                    Some(what),
                    "{}: {path} changed",
                    shape.name
                );
            }
        }
    }
}

// ------------------------------------------------ (f) WHAT THIS FILE DOES NOT CARRY ---

/// The note lists exactly the module paths below the home directory that hold something — the
/// planted ones, in registry order — and nothing in lodi's own config and data roots. A dangling
/// symbolic link counts: the question is `lstat`'s, and nothing is followed.
#[test]
fn the_note_lists_exactly_the_paths_that_exist_by_lstat() {
    let env = home_env("import-note");
    import(&env);
    let written = manifest(&env);
    assert!(
        written.contains("# WHAT THIS FILE DOES NOT CARRY\n"),
        "{written}"
    );
    assert!(
        written.contains("#   none of those paths holds a file in this home directory.\n"),
        "{written}"
    );

    plant(&env);
    let lodis = [config_root(&env), env.share().join("lodi")];
    let theirs = |p: &Path| p.starts_with(env.home()) && !lodis.iter().any(|l| p.starts_with(l));
    // One path is a dangling symbolic link instead of a file.
    let (linked, _) = module_paths(&env)
        .into_iter()
        .find(|(p, _)| theirs(p))
        .unwrap();
    fs::remove_file(&linked).unwrap();
    std::os::unix::fs::symlink("nowhere-at-all", &linked).unwrap();

    fs::remove_dir_all(config_root(&env)).unwrap();
    import(&env);
    let written = manifest(&env);
    let listed: Vec<&str> = written
        .lines()
        .filter(|l| l.starts_with("#   ") && l.contains(" exists; "))
        .collect();
    let want: Vec<String> = module_paths(&env)
        .into_iter()
        .filter(|(p, _)| theirs(p))
        .map(|(p, name)| {
            format!(
                "#   {} exists; translate what you need into [programs.{name}]",
                p.strip_prefix(env.home()).unwrap().display()
            )
        })
        .collect();
    assert_eq!(listed, want, "{written}");
    assert!(
        written.contains(".gitconfig exists; translate what you need into [programs.git]"),
        "the shadowing path of §4.6 is not listed:\n{written}"
    );
    assert!(!written.contains("none of those paths"), "{written}");
    unplant(&env);
}

// --------------------------------------------------------- (g) where it goes ---

/// With no flag the manifest lands in the standard config, mode 0644, and the switch that
/// follows reads it.
#[test]
fn with_no_flag_the_manifest_lands_where_the_switch_reads_it() {
    let env = home_env("import-in-place");
    import(&env);
    let path = config_root(&env).join("home.toml");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    fs::write(
        &path,
        format!(
            "{}\n[home.file.\"note.txt\"]\ntext = \"hello\\n\"\n",
            manifest(&env)
        ),
    )
    .unwrap();
    let plan = lodi(&env, &["home", "plan"]);
    assert_eq!(plan.status.code(), Some(0), "{}", stderr(&plan));
    assert!(
        home_part(&plan).contains("create    note.txt"),
        "the preview did not read the manifest that landed:\n{}",
        stderr(&plan)
    );
}

/// The file the import writes is the library's template for the same home and config, byte for
/// byte (the stub `lodi import` writes in a host's config is the same function), and an import
/// over a `home.toml` that is already there keeps the person's bytes: 1.x's refusal without
/// `--force` is 2.0's "the home file is there, leave it" (LD-518).
#[test]
fn the_written_file_is_the_template_and_a_second_import_keeps_an_edit() {
    let env = home_env("import-template");
    import(&env);
    let roots =
        lodi::roots::Roots::for_host(env.home().to_path_buf(), config_root(&env).to_path_buf());
    assert_eq!(
        manifest(&env),
        lodi::home::import::template(&roots, home_gate().root())
    );

    let edited = format!("{}\n# kept by hand\n", manifest(&env));
    fs::write(config_root(&env).join("home.toml"), &edited).unwrap();
    import(&env);
    assert_eq!(manifest(&env), edited, "a second import replaced the file");
}

// ------------------------------------ the import-then-switch path adopts identical files ---

/// The import-then-switch path for a file the home directory already holds: import, declare the
/// file by hand under `[home.file]` with the bytes and mode it already has, and the first switch
/// **adopts** it — the preview says `adopt`, and the switch records the path and writes no byte
/// (same inode, same mtime, nothing in the write ledger) — so a second switch has nothing to do,
/// a hand edit is `drift`, and a switch refuses it with `E_DRIFT` like any managed file.
#[test]
fn an_imported_manifest_adopts_a_hand_declared_identical_file_and_then_manages_it() {
    use std::os::unix::fs::MetadataExt;

    let env = home_env("import-adopt");
    let target = env.home().join(".inputrc");
    fs::write(&target, "set editing-mode vi\n").unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o644)).unwrap();
    let before = fs::metadata(&target).unwrap();

    import(&env);
    fs::write(
        config_root(&env).join("home.toml"),
        format!(
            "{}\n[home.file.\".inputrc\"]\ntext = \"set editing-mode vi\\n\"\n",
            manifest(&env)
        ),
    )
    .unwrap();

    let plan = lodi(&env, &["home", "plan"]);
    assert_eq!(plan.status.code(), Some(0), "{}", stderr(&plan));
    assert!(
        home_part(&plan).contains("adopt     .inputrc                  0644"),
        "{}",
        stderr(&plan)
    );

    let apply = lodi(&env, &["home", "apply"]);
    assert_eq!(apply.status.code(), Some(0), "{}", stderr(&apply));
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
    let state = fs::read_to_string(env.share().join("lodi/home-scope/state.json")).unwrap();
    assert!(state.contains("\".inputrc\""), "{state}");

    let again = lodi(&env, &["home", "apply"]);
    assert!(nothing(&again), "{}", stderr(&again));

    fs::write(&target, "set editing-mode emacs\n").unwrap();
    let status = lodi(&env, &["home", "status"]);
    assert!(
        home_part(&status).contains("drift     .inputrc"),
        "{}",
        stderr(&status)
    );
    let refused = lodi(&env, &["home", "apply"]);
    assert_eq!(refused.status.code(), Some(8), "{}", stderr(&refused));
    assert!(stderr(&refused).contains("E_DRIFT"), "{}", stderr(&refused));
    assert_eq!(
        fs::read_to_string(&target).unwrap(),
        "set editing-mode emacs\n",
        "the refused switch touched the edited file"
    );
}
