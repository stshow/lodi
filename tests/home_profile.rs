//! M-0.5 T-5: the user-wide tool profile, one PATH directory and one printed source line.
//!
//! Every product invocation is `lodi switch --home` (1.x's `lodi home apply`, LD-518) in a
//! `support::home_env`, with the data root moved below its scratch
//! home so the stable instruction contains literal `$HOME`. Only the tests create a login-shell
//! startup file, and only the tests start a login shell.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::home::profile::{PROFILE_BIN, PROFILE_SH};
use lodi::store::art_name;
use lodi::util::sha256_hex;
use support::{HomeEnv, Server, home_env, home_gate};

fn data(env: &HomeEnv) -> PathBuf {
    env.home().join(".local/share/lodi")
}

fn config(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn write_manifest(env: &HomeEnv, text: &str) {
    fs::create_dir_all(config(env)).unwrap();
    fs::write(config(env).join("home.toml"), text).unwrap();
}

fn command(env: &HomeEnv, server: &Server) -> Command {
    let mut command = env.command();
    command
        .env_remove("LODI_HOME")
        .env("XDG_DATA_HOME", env.home().join(".local/share"))
        .env("LODI_FETCH_REWRITE", server.rewrite());
    command
}

fn run(env: &HomeEnv, server: &Server, args: &[&str]) -> Output {
    let mut command = command(env, server);
    env.on_gate(&mut command, args).output().unwrap()
}

/// What a switch said: all of it is on standard error, standard output stays empty.
fn stdout(out: &Output) -> String {
    assert!(out.stdout.is_empty(), "{out:?}");
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// The art of `tool` the home's section of the config's `lodi.lock` holds (1.x's `home.lock`).
fn art(env: &HomeEnv, tool: &str) -> String {
    let lock = lodi::config::lock::read(&config(env)).unwrap();
    let [(_, home)] = lock.homes.iter().collect::<Vec<_>>()[..] else {
        panic!("not one home in {lock:?}");
    };
    art_name(&home.tools[tool]).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

fn declaration(label: &str, name: Option<&str>, url: &str, bytes: &[u8], env: &str) -> String {
    let name = name.map_or(String::new(), |name| format!("name = \"{name}\"\n"));
    let env = if env.is_empty() {
        String::new()
    } else {
        format!("env = {{ {env} }}\n")
    };
    format!(
        "[tools.{label}]\nversion = \"1.0.0\"\n{name}url = \"{url}\"\nsha256 = \"{}\"\n\
         format = \"binary\"\npath = [\"bin\"]\n{env}",
        sha256_hex(bytes)
    )
}

/// A home of `blocks` and one file: `lodi switch --home` has nothing to switch for a home of
/// tools alone (the finding `catalogue_batch` keeps open), so every home here has a file too.
fn manifest(blocks: &[String]) -> String {
    format!(
        "[home]\nversion = \"1\"\n\n[home.file.\".marker\"]\ntext = \"{}\"\n\n{}",
        blocks.len(),
        blocks.join("\n")
    )
}

fn server(files: &[(&str, &[u8])]) -> Server {
    Server::start(
        files
            .iter()
            .map(|(url, bytes)| ((*url).to_string(), bytes.to_vec()))
            .collect::<BTreeMap<_, _>>(),
    )
}

fn profile(env: &HomeEnv) -> PathBuf {
    data(env).join(PROFILE_SH)
}

fn bin(env: &HomeEnv) -> PathBuf {
    data(env).join(PROFILE_BIN)
}

fn assert_link(path: &Path) {
    assert!(
        fs::symlink_metadata(path).unwrap().file_type().is_symlink(),
        "{} is not a symbolic link",
        path.display()
    );
}

#[test]
fn two_tools_reach_one_idempotent_path_and_a_new_login_shell() {
    let env = home_env("profile-two-tools");
    let alpha_url = "https://fixtures.test/alpha";
    let beta_url = "https://fixtures.test/beta";
    let alpha = b"#!/bin/sh\necho alpha\n";
    let beta = b"#!/bin/sh\necho beta\n";
    let server = server(&[(alpha_url, alpha), (beta_url, beta)]);
    let alpha_decl = declaration(
        "alpha",
        None,
        alpha_url,
        alpha,
        "ALPHA_HOME = \"${self.path}\", ALPHA_VERSION = \"${self.version}\"",
    );
    let beta_decl = declaration("beta", None, beta_url, beta, "");
    write_manifest(&env, &manifest(&[alpha_decl.clone(), beta_decl]));

    let out = run(&env, &server, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.ends_with(
        "add this line to your shell rc file (lodi never edits it):\n  . \
         \"$HOME/.local/share/lodi/home-scope/profile.sh\"\n"
    ));
    assert!(
        !shown.contains(&env.root().display().to_string()),
        "the stable line exposed the scratch root: {shown}"
    );

    let bin = bin(&env);
    assert_link(&bin.join("alpha"));
    assert_link(&bin.join("beta"));
    let mode = fs::metadata(profile(&env)).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o644);

    let path = format!("{}:/usr/bin:{}", bin.display(), bin.display());
    let sourced = Command::new("/bin/sh")
        .args([
            "-c",
            ". \"$1\"; . \"$1\"; command -v alpha; printf '%s\\n%s\\n%s\\n' \"$PATH\" \
             \"$ALPHA_VERSION\" \"$ALPHA_HOME\"",
            "sh",
        ])
        .arg(profile(&env))
        .env_clear()
        .env("PATH", path)
        .output()
        .unwrap();
    assert_eq!(sourced.status.code(), Some(0), "{:?}", sourced.status);
    let sourced = String::from_utf8(sourced.stdout).unwrap();
    let lines: Vec<&str> = sourced.lines().collect();
    assert_eq!(lines[0], bin.join("alpha").display().to_string());
    assert_eq!(
        lines[1]
            .split(':')
            .filter(|part| *part == bin.to_str().unwrap())
            .count(),
        1,
        "sourcing twice duplicated the bin directory: {}",
        lines[1]
    );
    assert_eq!(lines[2], "1.0.0");
    assert_eq!(
        lines[3],
        data(&env)
            .join("store")
            .join(art(&env, "alpha"))
            .display()
            .to_string()
    );

    // The test, never Lodi, adds the printed line and starts a new login shell in the scratch
    // home. With an otherwise empty environment the tool is available there.
    fs::write(
        env.home().join(".profile"),
        ". \"$HOME/.local/share/lodi/home-scope/profile.sh\"\n",
    )
    .unwrap();
    let login = Command::new("/bin/sh")
        .args(["-lc", "command -v alpha"])
        .env_clear()
        .env("HOME", env.home())
        .env("SHELL", "/bin/sh")
        .output()
        .unwrap();
    assert_eq!(login.status.code(), Some(0), "{}", stderr(&login));
    assert_eq!(
        String::from_utf8(login.stdout).unwrap().trim(),
        bin.join("alpha").display().to_string()
    );

    // A foreign regular file in Lodi's own bin directory is removed with a note on the next
    // rebuild. Dropping beta from the manifest removes beta's old link and leaves alpha's.
    fs::write(bin.join("foreign"), b"not a link\n").unwrap();
    write_manifest(&env, &manifest(&[alpha_decl]));
    let out = run(&env, &server, &["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_link(&bin.join("alpha"));
    assert!(!bin.join("beta").exists());
    assert!(!bin.join("foreign").exists());
}

#[test]
fn a_conflict_is_first_tool_wins_and_is_stable_across_applies() {
    let env = home_env("profile-conflict");
    let alpha_url = "https://fixtures.test/shared-alpha";
    let beta_url = "https://fixtures.test/shared-beta";
    let alpha = b"#!/bin/sh\necho alpha owns this\n";
    let beta = b"#!/bin/sh\necho beta loses this\n";
    let server = server(&[(alpha_url, alpha), (beta_url, beta)]);
    write_manifest(
        &env,
        &manifest(&[
            declaration("alpha", Some("shared"), alpha_url, alpha, ""),
            declaration("beta", Some("shared"), beta_url, beta, ""),
        ]),
    );

    let first = run(&env, &server, &["home", "apply"]);
    assert_eq!(first.status.code(), Some(0), "{}", stderr(&first));
    let warning = "W_BIN_CONFLICT: `shared` is provided by alpha and beta; alpha wins";
    assert!(stderr(&first).contains(warning), "{}", stderr(&first));
    assert!(!stderr(&first).contains("beta wins"), "{}", stderr(&first));

    let alpha_art = art(&env, "alpha");
    let link = bin(&env).join("shared");
    assert_eq!(
        fs::read_link(&link).unwrap(),
        PathBuf::from("../../../store")
            .join(alpha_art)
            .join("bin/shared")
    );
    let first_target = fs::read_link(&link).unwrap();

    let second = run(&env, &server, &["home", "apply"]);
    assert!(support::nothing(&second), "{}", stderr(&second));
    assert_eq!(fs::read_link(&link).unwrap(), first_target);
}

#[test]
fn fish_gets_only_its_warning_and_status_reports_path_without_writing() {
    let env = home_env("profile-fish-status");
    let url = "https://fixtures.test/alpha-fish";
    let body = b"#!/bin/sh\necho alpha\n";
    let server = server(&[(url, body)]);
    write_manifest(
        &env,
        &manifest(&[declaration("alpha", None, url, body, "")]),
    );

    let mut apply = command(&env, &server);
    apply.env("SHELL", "/usr/bin/fish");
    let out = env
        .on_gate(&mut apply, &["home", "apply"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.contains("W_FISH_HOOKS"), "{shown}");
    assert!(shown.contains("$HOME/.local/share/lodi/home-scope/profile/bin"));
    assert!(!shown.contains("add this line"), "{shown}");
    assert!(!shown.contains("profile.sh"), "{shown}");

    let before = home_gate().ledger_lines().len();
    let mut status = command(&env, &server);
    status.env("PATH", format!("{}:/usr/bin", bin(&env).display()));
    let out = env
        .on_gate(&mut status, &["home", "status"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    let shown = stdout(&out);
    assert!(shown.contains(" is on PATH"), "{shown}");
    assert!(shown.contains("$HOME/.local/share/lodi/home-scope/profile.sh"));
    let appended: Vec<_> = home_gate()
        .ledger_lines()
        .into_iter()
        .skip(before)
        .filter(|(_, path)| path.starts_with(env.root()))
        .collect();
    assert!(appended.is_empty(), "status wrote: {appended:?}");
}

/// Real-network package evidence: the public `home apply` path resolves and realizes jq, builds
/// the profile, then a new login shell in the scratch home finds it. The package probe selects
/// this ignored test; ordinary `cargo test` stays offline.
#[test]
#[ignore = "selected only by scripts/m05-local.sh --case home-tools"]
fn live_home_apply_puts_a_real_tool_on_a_new_login_path() {
    assert_eq!(std::env::var("LODI_M05_LIVE").as_deref(), Ok("1"));
    let env = home_env("profile-live");
    write_manifest(
        &env,
        "[home]\nversion = \"1\"\n\n[home.file.\".marker\"]\ntext = \"1\"\n\n[tools]\njq = \"latest\"\n",
    );
    let server = Server::start(BTreeMap::new());
    let mut apply = command(&env, &server);
    apply.env_remove("LODI_FETCH_REWRITE");
    let out = env
        .on_gate(&mut apply, &["home", "apply"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_link(&bin(&env).join("jq"));
    fs::write(
        env.home().join(".profile"),
        ". \"$HOME/.local/share/lodi/home-scope/profile.sh\"\n",
    )
    .unwrap();
    let login = Command::new("/bin/sh")
        .args(["-lc", "command -v jq"])
        .env_clear()
        .env("HOME", env.home())
        .env("SHELL", "/bin/sh")
        .output()
        .unwrap();
    assert_eq!(login.status.code(), Some(0));
}
