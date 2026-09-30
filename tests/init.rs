//! `lodi init` (M-0.3 T-1, acceptance rows A01–A03): the real binary, in real scratch
//! directories, offline. Nothing here mocks anything, and nothing here reaches the network:
//! `lodi init` downloads nothing, installs nothing and runs nothing.

mod support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

#[path = "support/ids.rs"]
mod ids;

/// A fresh, empty directory under `CARGO_TARGET_TMPDIR` (AGENTS.md §6.1: scratch belongs there),
/// removed by the caller on every path it takes.
fn scratch(tag: &str) -> PathBuf {
    support::scratch(&format!("init-{tag}"))
}

fn lodi(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(dir)
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
            .join("tests/fixtures/init")
            .join(name),
    )
    .expect("the committed fixture is readable")
}

/// A01: an empty directory, one command, and the next two commands have something to read.
#[test]
fn init_writes_the_committed_template_and_the_ignore_line() {
    let dir = scratch("a01");
    let out = lodi(&dir, &["init", "--name", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "wrote ./lodi.toml\nadded .lodi/ to ./.gitignore\nnext: lodi lock\n",
        "the three lines of spec/10-cli §4, in order"
    );

    // Byte-identical to the committed fixture: the template cannot drift silently (D2, LD-47).
    let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
    assert_eq!(written, fixture("default.toml"));

    // And those exact bytes are a manifest this build accepts.
    let parsed = lodi::manifest::parse_project_manifest(&written, "lodi.toml")
        .expect("the written manifest parses");
    assert_eq!(parsed.project.name.as_deref(), Some("hello"));
    assert!(parsed.tools.contains_key("python"));
    assert!(!parsed.is_container_mode());

    assert_eq!(
        fs::read_to_string(dir.join(".gitignore")).unwrap(),
        ".lodi/\n"
    );
    // Only the two files: `.lodi/` is ignored, never created (T-1 "Must not").
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![".gitignore".to_string(), "lodi.toml".to_string()]
    );
    assert!(!dir.join(".lodi").exists());

    // 0644, as spec/10-cli §4 asks.
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &fs::metadata(dir.join("lodi.toml")).unwrap().permissions(),
    );
    assert_eq!(mode & 0o777, 0o644);

    fs::remove_dir_all(&dir).unwrap();
}

/// The lodi.toml `lodi init` writes explains its keys in plain words, for every template it can
/// write: no design spec section, no section sign and no decision id, a comment right above
/// every table, and the file still parses.
#[test]
fn init_template_cites_no_internal_ids() {
    for base in [
        None,
        Some("debian:bookworm"),
        Some("ubuntu:noble"),
        Some("arch:rolling"),
    ] {
        let dir = scratch("plain-words");
        let mut args = vec!["init", "--name", "hello"];
        args.extend(base.map(|b| ["--base", b]).iter().flatten());
        let out = lodi(&dir, &args);
        assert_eq!(out.status.code(), Some(0), "{base:?}: {}", stderr(&out));
        let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
        assert_eq!(
            ids::internal_ids(&written),
            Vec::<String>::new(),
            "{base:?}:\n{written}"
        );
        let lines: Vec<&str> = written.lines().collect();
        for (at, line) in lines.iter().enumerate() {
            if line.starts_with('[') {
                assert!(
                    at > 0 && lines[at - 1].starts_with("# "),
                    "{base:?}: no comment explains {line}:\n{written}"
                );
            }
        }
        lodi::manifest::parse_project_manifest(&written, "lodi.toml")
            .expect("the written manifest parses");
        fs::remove_dir_all(&dir).unwrap();
    }
}

/// The default name comes from the project directory, sanitized (`spec/01` §3.2).
#[test]
fn without_a_name_the_directory_names_the_project() {
    let dir = scratch("Name_Case").join("My Project_2");
    fs::create_dir_all(&dir).unwrap();
    let out = lodi(&dir, &["init"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
    assert!(written.contains("name = \"my-project-2\""), "{written}");
    assert_eq!(
        written,
        fixture("default.toml").replace("hello", "my-project-2")
    );
    fs::remove_dir_all(dir.parent().unwrap()).unwrap();
}

/// A02: the refusal writes nothing, and `--force` replaces the file.
#[test]
fn a_second_init_refuses_with_e_exists_and_changes_no_byte() {
    let dir = scratch("a02");
    assert_eq!(
        lodi(&dir, &["init", "--name", "hello"]).status.code(),
        Some(0)
    );
    let before = fs::read(dir.join("lodi.toml")).unwrap();
    let before_hash = lodi::util::sha256_hex(&before);

    let out = lodi(&dir, &["init", "--name", "other"]);
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(
        out.stdout.is_empty(),
        "the refusal prints nothing to stdout"
    );
    let text = stderr(&out);
    assert!(text.contains("lodi: error E_EXISTS"), "{text}");
    assert!(
        text.contains("hint: use --force to overwrite ./lodi.toml"),
        "{text}"
    );
    assert_eq!(
        lodi::util::sha256_hex(&fs::read(dir.join("lodi.toml")).unwrap()),
        before_hash,
        "the manifest's bytes are unchanged"
    );

    // --force replaces it, atomically, and says so.
    let out = lodi(&dir, &["init", "--name", "other", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stdout(&out),
        "replaced ./lodi.toml\nnext: lodi lock\n",
        "the ignore line is already there, so it is not reported again"
    );
    let after = fs::read_to_string(dir.join("lodi.toml")).unwrap();
    assert_eq!(after, fixture("default.toml").replace("hello", "other"));
    assert_ne!(lodi::util::sha256_hex(after.as_bytes()), before_hash);

    // No temporary file is left behind by either run.
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![".gitignore".to_string(), "lodi.toml".to_string()]
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// `--base` writes the container manifest of `spec/01` §3.6; an unsupported base is
/// `E_UNSUPPORTED` at exit 3 and names what this build does write.
#[test]
fn base_writes_a_container_manifest_and_refuses_what_it_cannot_write() {
    let dir = scratch("base");
    let out = lodi(
        &dir,
        &["init", "--name", "hello", "--base", "debian:bookworm"],
    );
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
    assert_eq!(written, fixture("container.toml"));
    let parsed = lodi::manifest::parse_project_manifest(&written, "lodi.toml").unwrap();
    assert!(parsed.is_container_mode());
    assert!(parsed.packages.is_empty());
    // D2: `[tools]` stays active in container mode too.
    assert!(parsed.tools.contains_key("python"));

    // `debian` alone means the one release this build writes, and `debian:12` normalizes to it.
    for base in ["debian", "debian:12"] {
        fs::remove_file(dir.join("lodi.toml")).unwrap();
        assert_eq!(
            lodi(&dir, &["init", "--name", "hello", "--base", base])
                .status
                .code(),
            Some(0),
            "{base}"
        );
        assert_eq!(
            fs::read_to_string(dir.join("lodi.toml")).unwrap(),
            fixture("container.toml")
        );
    }

    // M-0.3 T-4: Ubuntu noble is the second base, written from the same template — the two
    // manifests differ in the two lines the base names and nowhere else.
    for base in ["ubuntu", "ubuntu:24.04", "ubuntu:noble"] {
        fs::remove_file(dir.join("lodi.toml")).unwrap();
        let out = lodi(&dir, &["init", "--name", "hello", "--base", base]);
        assert_eq!(out.status.code(), Some(0), "{base}: {}", stderr(&out));
        let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
        assert_eq!(
            written,
            fixture("container.toml")
                .replace("distro = \"debian\"", "distro = \"ubuntu\"")
                .replace("release = \"bookworm\"", "release = \"noble\""),
            "{base}"
        );
        let parsed = lodi::manifest::parse_project_manifest(&written, "lodi.toml").unwrap();
        assert!(parsed.is_container_mode(), "{base}");
        assert_eq!(
            parsed.container.as_ref().map(|c| c.release.as_str()),
            Some("noble"),
            "{base}"
        );
    }

    // M-Arch-Base T-6: Arch rolling is the third base, from the same template, and `arch` alone
    // means the one release this build writes.
    for base in ["arch", "arch:rolling"] {
        fs::remove_file(dir.join("lodi.toml")).unwrap();
        let out = lodi(&dir, &["init", "--name", "hello", "--base", base]);
        assert_eq!(out.status.code(), Some(0), "{base}: {}", stderr(&out));
        let written = fs::read_to_string(dir.join("lodi.toml")).unwrap();
        assert_eq!(
            written,
            fixture("container.toml")
                .replace("distro = \"debian\"", "distro = \"arch\"")
                .replace("release = \"bookworm\"", "release = \"rolling\""),
            "{base}"
        );
        let parsed = lodi::manifest::parse_project_manifest(&written, "lodi.toml").unwrap();
        assert!(parsed.is_container_mode(), "{base}");
        assert_eq!(
            parsed.container.as_ref().map(|c| c.release.as_str()),
            Some("rolling"),
            "{base}"
        );
    }

    fs::remove_file(dir.join("lodi.toml")).unwrap();
    for base in [
        "arch:2026.09.01",
        "arch:latest",
        "debian:trixie",
        "ubuntu:jammy",
        "ubuntu:22.04",
        "fedora:43",
    ] {
        let out = lodi(&dir, &["init", "--base", base]);
        assert_eq!(out.status.code(), Some(3), "{base}: {}", stderr(&out));
        let text = stderr(&out);
        assert!(text.contains("lodi: error E_UNSUPPORTED"), "{base}: {text}");
        assert!(text.contains("arch:rolling"), "{base}: {text}");
        assert!(text.contains("debian:bookworm"), "{base}: {text}");
        assert!(text.contains("ubuntu:noble"), "{base}: {text}");
        assert!(text.contains("fedora:44"), "{base}: {text}");
        assert!(
            !dir.join("lodi.toml").exists(),
            "{base} wrote a manifest anyway"
        );
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// The ignore file is appended to, never rewritten, and an existing `.lodi/` line is left alone.
#[test]
fn the_gitignore_is_appended_to_and_never_rewritten() {
    let dir = scratch("gitignore");

    // An existing file without the line: every other line survives, byte for byte.
    fs::write(dir.join(".gitignore"), "target\n/dist\n").unwrap();
    let out = lodi(&dir, &["init", "--name", "hello"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(stdout(&out).contains("added .lodi/ to ./.gitignore"));
    assert_eq!(
        fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "target\n/dist\n.lodi/\n"
    );

    // A second, forced run does not add it twice and prints no ignore line.
    let out = lodi(&dir, &["init", "--name", "hello", "--force"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stdout(&out), "replaced ./lodi.toml\nnext: lodi lock\n");
    assert_eq!(
        fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "target\n/dist\n.lodi/\n"
    );

    // A file with no trailing newline keeps its last line intact.
    fs::remove_file(dir.join("lodi.toml")).unwrap();
    fs::write(dir.join(".gitignore"), "target").unwrap();
    assert_eq!(lodi(&dir, &["init"]).status.code(), Some(0));
    assert_eq!(
        fs::read_to_string(dir.join(".gitignore")).unwrap(),
        "target\n.lodi/\n"
    );
    fs::remove_dir_all(&dir).unwrap();
}

/// A project-local `.gitignore` symlink must not make `init` inspect or modify an external file.
#[test]
fn a_gitignore_symlink_is_refused_before_any_write() {
    let root = scratch("gitignore-symlink");
    let project = root.join("project");
    let sentinel = root.join("external-sentinel");
    fs::create_dir(&project).unwrap();
    let before = b"target\n.lodi/\n";
    fs::write(&sentinel, before).unwrap();
    std::os::unix::fs::symlink(&sentinel, project.join(".gitignore")).unwrap();

    let out = lodi(&project, &["init", "--name", "hello"]);
    assert_eq!(out.status.code(), Some(6), "{}", stderr(&out));
    assert!(
        out.stdout.is_empty(),
        "the refusal prints nothing to stdout"
    );
    let text = stderr(&out);
    assert!(text.contains("lodi: error E_STORE_IO"), "{text}");
    assert!(text.contains("./.gitignore"), "{text}");
    assert!(
        !project.join("lodi.toml").exists(),
        "the refusal wrote a manifest"
    );
    assert_eq!(
        fs::read(&sentinel).unwrap(),
        before,
        "the external target changed"
    );

    fs::remove_dir_all(&root).unwrap();
}

/// `init` takes no positional argument, and every flag but its three is a usage error: exit 2.
/// `init --help` is the command's help since LD-360: exit 0, and no manifest either.
#[test]
fn bad_init_arguments_are_usage_errors() {
    let dir = scratch("usage");
    let help = lodi(&dir, &["init", "--help"]);
    assert_eq!(help.status.code(), Some(0), "{}", stderr(&help));
    assert!(
        !dir.join("lodi.toml").exists(),
        "init --help wrote a manifest"
    );
    for args in [
        &["init", "--name"][..],
        &["init", "--base"],
        &["init", "here"],
        &["init", "--frobnicate"],
    ] {
        let out = lodi(&dir, args);
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", stderr(&out));
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(stderr(&out).contains("unsupported"), "{args:?}");
        assert!(!dir.join("lodi.toml").exists(), "{args:?} wrote a manifest");
    }
    fs::remove_dir_all(&dir).unwrap();
}

/// `README.md`'s quick start shows the manifest `lodi init --name hello` writes, byte for byte.
/// AGENTS.md §9.4 makes that block part of the product: the fresh-guest gate writes it verbatim.
#[test]
fn the_readme_quick_start_shows_exactly_what_init_writes() {
    let readme = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md"))
        .expect("README.md is readable");
    let quick_start = readme
        .split("\n## Quick start\n")
        .nth(1)
        .and_then(|rest| rest.split("\n## ").next())
        .expect("README.md has a '## Quick start' section");
    let blocks: Vec<&str> = quick_start.split("```toml\n").skip(1).collect();
    assert_eq!(
        blocks.len(),
        1,
        "the quick start has exactly one toml block"
    );
    let block = blocks[0].split("```").next().unwrap();
    assert_eq!(block, fixture("default.toml"));
}
