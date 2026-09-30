//! The program modules this build ships: `git` (M-Home pa-1, `docs/design/HOME_PROGRAMS.md`
//! §6.1, decision D1), the shells `bash`, `zsh` and `fish` (pa-2, §5, §6.2–§6.4, decision D3),
//! the editors `nvim` and `helix` (pb-1, §6.5, §6.6), and the terminals and prompt `alacritty`,
//! `kitty`, `tmux` and `starship` with starship's `init` line in the shell files (pb-2,
//! §6.7–§6.10); invariants I1, I3, I6, I8, I9, I11–I13, I15 and I16.
//!
//! Each module renders the committed goldens under `tests/fixtures/programs/<module>/expected/`
//! from `home.toml` beside them — twice in one run, under two different roots, and from
//! `home.reordered.toml`, which declares the same table in another order and spelling. Setting
//! `LODI_RECORD_EXPECTED=1` rewrites the goldens from the code instead of comparing; no
//! validation command sets it, so a gate always compares. Each golden is then read by the
//! application itself where it is on `PATH` (`git config`, `bash -n`, `zsh -n`, `fish -n`,
//! `nvim --headless`, `tmux -f`, `starship print-config`); a check whose application is not on
//! `PATH` prints that it was not run.
//!
//! The built binary is driven through `support::home_env` with the data root below the scratch
//! home, with a `PATH` each test sets, and — for `[tools]` — the offline fixture server. Nothing
//! runs against a real home: no rc file is created by Lodi, and the only shell start-up the tests
//! run is `bash --norc`/`zsh -f` sourcing the rendered file by hand. Offline and deterministic.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::diag::Diagnostic;
use lodi::home::manifest::{HomeManifest, load_home_manifest};
use lodi::home::programs;
use lodi::roots::Roots;
use lodi::util::sha256_hex;
use support::{HomeEnv, Server, home_env, home_gate};

const MODULES: &[&str] = &[
    "alacritty",
    "bash",
    "fish",
    "git",
    "helix",
    "kitty",
    "nvim",
    "starship",
    "tmux",
    "zsh",
];
const HEAD: &str = "[home]\nversion = \"1\"\n";

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

fn fixture(module: &str) -> PathBuf {
    repo().join("tests/fixtures/programs").join(module)
}

fn recording() -> bool {
    std::env::var_os("LODI_RECORD_EXPECTED").is_some_and(|v| v == "1")
}

/// Lodi's data root, below the scratch home so that a shell module can render there (§5).
fn data(env: &HomeEnv) -> PathBuf {
    env.home().join(".local/share/lodi")
}

fn manifest_dir(env: &HomeEnv) -> PathBuf {
    env.config().join("lodi")
}

fn roots(env: &HomeEnv) -> Roots {
    let home = OsString::from(env.home());
    let config = OsString::from(env.config());
    let data = OsString::from(data(env));
    Roots::from_vars(
        Some(home.as_os_str()),
        Some(config.as_os_str()),
        None,
        Some(data.as_os_str()),
    )
    .unwrap()
}

fn write_manifest(env: &HomeEnv, text: &str) {
    fs::create_dir_all(manifest_dir(env)).unwrap();
    fs::write(manifest_dir(env).join("home.toml"), text).unwrap();
}

/// Copy `<fixture>/<name>` and its `snippets/` into the scratch configuration directory.
fn install(env: &HomeEnv, module: &str, name: &str) {
    write_manifest(
        env,
        &fs::read_to_string(fixture(module).join(name)).unwrap(),
    );
    let snippets = fixture(module).join("snippets");
    if snippets.is_dir() {
        let to = manifest_dir(env).join("snippets");
        fs::create_dir_all(&to).unwrap();
        for entry in fs::read_dir(&snippets).unwrap() {
            let entry = entry.unwrap();
            fs::copy(entry.path(), to.join(entry.file_name())).unwrap();
        }
    }
}

fn load(env: &HomeEnv) -> Result<HomeManifest, Vec<Diagnostic>> {
    load_home_manifest(&roots(env)).map_err(|e| e.diagnostics)
}

/// What `[programs.<module>]` of a manifest renders: home-relative path to bytes.
fn rendered(env: &HomeEnv, module: &str) -> BTreeMap<String, Vec<u8>> {
    let manifest = load(env).unwrap_or_else(|ds| panic!("{module}: {ds:#?}"));
    let table = format!("programs.{module}");
    manifest
        .files
        .into_iter()
        .filter(|(_, entry)| entry.table == table)
        .map(|(path, entry)| {
            assert_eq!(entry.mode, 0o644, "{path}");
            (path, entry.bytes)
        })
        .collect()
}

fn render_fixture(tag: &str, module: &str, name: &str) -> BTreeMap<String, Vec<u8>> {
    let env = home_env(tag);
    install(&env, module, name);
    let files = rendered(&env, module);
    for bytes in files.values() {
        let text = String::from_utf8_lossy(bytes);
        assert!(
            !text.contains(&env.root().display().to_string()),
            "{module}: a rendered file names the scratch root (I3):\n{text}"
        );
    }
    files
}

fn refused(text: &str) -> Vec<Diagnostic> {
    let env = home_env("shipped-refused");
    write_manifest(&env, text);
    match load(&env) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(ds) => ds,
    }
}

/// The executable `name` in this process's `PATH`, if there is one.
fn on_path(name: &str) -> Option<PathBuf> {
    home_gate();
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(name))
        .find(|p| fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0))
}

fn stdout(out: &Output) -> String {
    String::from_utf8(out.stdout.clone()).unwrap()
}

fn stderr(out: &Output) -> String {
    String::from_utf8(out.stderr.clone()).unwrap()
}

/// A directory holding exactly the named executables (each a shell script that exits 0): the
/// `PATH` a run of the binary sees.
fn path_with(env: &HomeEnv, tag: &str, names: &[&str]) -> PathBuf {
    let dir = env.root().join(format!("path-{tag}"));
    fs::create_dir_all(&dir).unwrap();
    for name in names {
        let file = dir.join(name);
        fs::write(&file, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o755)).unwrap();
    }
    dir
}

/// The binary, with the data root below the scratch home and `PATH` set to `path`.
fn command(env: &HomeEnv, path: &Path) -> Command {
    let mut command = env.command();
    command
        .env("LODI_HOME", data(env))
        .env("XDG_DATA_HOME", env.home().join(".local/share"))
        .env("PATH", path);
    command
}

/// The ledger's lines since `before` that fall under this test's scratch root: the gate's ledger
/// is shared by every test of the binary, and they run in parallel.
fn ledger_since(env: &HomeEnv, before: usize) -> Vec<(String, PathBuf)> {
    let mut lines = home_gate().ledger_lines().split_off(before);
    lines.retain(|(_, path)| path.starts_with(env.root()));
    lines
}

fn state(env: &HomeEnv) -> serde_json::Value {
    let text = fs::read_to_string(data(env).join("home-scope/state.json")).unwrap();
    serde_json::from_str(&text).unwrap()
}

// ---------------------------------------------------------------------------------- goldens ---

/// I1, I3, I11, I12: each module renders its committed goldens — the same bytes twice in one run,
/// under a second root, and from the reordered declaration — and exactly those files.
#[test]
fn each_module_renders_its_goldens_twice_across_roots_and_orders() {
    for module in MODULES {
        let first = render_fixture("golden-a", module, "home.toml");
        let again = render_fixture("golden-a", module, "home.toml");
        let elsewhere = render_fixture("golden-b", module, "home.toml");
        let reordered = render_fixture("golden-c", module, "home.reordered.toml");
        assert_eq!(first, again, "{module}: two renders in one run differ");
        assert_eq!(first, elsewhere, "{module}: the render depends on the root");
        assert_eq!(
            first, reordered,
            "{module}: the render depends on key order"
        );

        let expected = fixture(module).join("expected");
        let names: Vec<String> = first
            .keys()
            .map(|p| p.rsplit('/').next().unwrap().to_string())
            .collect();
        for (path, bytes) in &first {
            let golden = expected.join(path.rsplit('/').next().unwrap());
            if recording() {
                fs::create_dir_all(&expected).unwrap();
                fs::write(&golden, bytes).unwrap();
                continue;
            }
            let committed = fs::read(&golden).unwrap_or_else(|e| {
                panic!(
                    "{}: {e}; record with LODI_RECORD_EXPECTED=1",
                    golden.display()
                )
            });
            assert!(
                *bytes == committed,
                "{module}: {path} is not {} byte for byte\nrendered:  {bytes:?}\ncommitted: {committed:?}",
                golden.display()
            );
            let comment = if *module == "nvim" { "--" } else { "#" };
            assert!(
                committed.starts_with(
                    format!("{comment} Managed by lodi from home.toml [programs.{module}].")
                        .as_bytes()
                ),
                "{module}: {path} does not start with the ownership header (I12)"
            );
        }
        let mut committed: Vec<String> = fs::read_dir(&expected)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        committed.sort();
        let mut names = names;
        names.sort();
        assert_eq!(
            names, committed,
            "{module}: rendered files and goldens differ"
        );
    }
}

/// The paths the modules render: git's two below the XDG configuration directory, the bash and
/// zsh files in the data root, fish's drop-in below `conf.d`.
#[test]
fn each_module_renders_at_its_path() {
    let paths = |module: &str| -> Vec<String> {
        render_fixture("paths", module, "home.toml")
            .into_keys()
            .collect()
    };
    assert_eq!(paths("git"), [".config/git/config", ".config/git/ignore"]);
    assert_eq!(
        paths("bash"),
        [".local/share/lodi/home-scope/shell/init.bash"]
    );
    assert_eq!(
        paths("zsh"),
        [".local/share/lodi/home-scope/shell/init.zsh"]
    );
    assert_eq!(paths("fish"), [".config/fish/conf.d/lodi.fish"]);
    assert_eq!(paths("nvim"), [".config/nvim/init.lua"]);
    assert_eq!(paths("helix"), [".config/helix/config.toml"]);
    assert_eq!(paths("alacritty"), [".config/alacritty/alacritty.toml"]);
    assert_eq!(paths("kitty"), [".config/kitty/kitty.conf"]);
    assert_eq!(paths("tmux"), [".config/tmux/tmux.conf"]);
    assert_eq!(paths("starship"), [".config/starship.toml"]);
}

// ---------------------------------------------------------------------------------- git -------

/// §6.1: the golden is read back by git itself, value for value, where git is on `PATH`.
#[test]
fn git_reads_the_golden_config_value_for_value() {
    let Some(git) = on_path("git") else {
        eprintln!("not run: git is not on PATH");
        return;
    };
    let config = fixture("git").join("expected/config");
    let get = |key: &str| {
        let out = Command::new(&git)
            .args(["config", "--file"])
            .arg(&config)
            .args(["--get", key])
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{key}: {}", stderr(&out));
        stdout(&out).trim_end_matches('\n').to_string()
    };
    assert_eq!(get("user.name"), "Example User");
    assert_eq!(get("user.email"), "someone@example");
    assert_eq!(get("init.defaultBranch"), "main");
    assert_eq!(get("core.pager"), "less -FRX");
    assert_eq!(get("pull.rebase"), "true");
    assert_eq!(get("push.autoSetupRemote"), "true");
    assert_eq!(get("merge.conflictStyle"), "zdiff3");
    assert_eq!(get("alias.lg"), "log --graph --oneline");
    assert_eq!(get("diff.lockb.textconv"), "cat");
}

/// §6.1: every string is quoted and escaped as git-config requires, so the characters git-config
/// treats specially — `"`, `\`, `#`, `;`, a tab, blanks at either end — come back exactly.
#[test]
fn git_quotes_and_escapes_every_value_as_git_reads_it() {
    let Some(git) = on_path("git") else {
        eprintln!("not run: git is not on PATH");
        return;
    };
    let name = "  A \"quoted\" name ; # not a comment \\ end\t";
    let alias = "!f() { echo \"$1\" ; }; f";
    let env = home_env("git-quoting");
    write_manifest(
        &env,
        &format!(
            "{HEAD}[programs.git]\nuser.name = {name:?}\nalias.say = {alias:?}\n",
            name = name,
            alias = alias
        ),
    );
    let files = rendered(&env, "git");
    let config = env.root().join("rendered-config");
    fs::write(&config, &files[".config/git/config"]).unwrap();
    for (key, want) in [("user.name", name), ("alias.say", alias)] {
        let out = Command::new(&git)
            .args(["config", "--file"])
            .arg(&config)
            .args(["--get", key])
            .env_clear()
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(out.status.success(), "{key}: {}", stderr(&out));
        assert_eq!(stdout(&out), format!("{want}\n"), "{key}");
    }
}

/// §6.1: a value holding a newline or a NUL is `E_TYPE` at the value, and so is an alias name git
/// does not allow or a conflict style it does not know.
#[test]
fn git_refuses_a_newline_a_nul_and_names_outside_its_grammar() {
    for (table, key) in [
        ("user.name = \"a\\nb\"", "user.name"),
        ("user.email = \"a\\u0000b\"", "user.email"),
        ("core.editor = \"vi\\r\"", "core.editor"),
        ("alias = { st = \"status\\n\" }", "alias.st"),
        ("alias = { \"a b\" = \"status\" }", "alias.a b"),
        ("ignores = [\"*.swp\\n.env\"]", "ignores"),
        ("merge.conflictStyle = \"diff4\"", "merge.conflictStyle"),
        ("pull.rebase = \"yes\"", "pull.rebase"),
    ] {
        let ds = refused(&format!("{HEAD}[programs.git]\n{table}\n"));
        assert_eq!(ds.len(), 1, "{table}: {ds:#?}");
        let d = &ds[0];
        assert_eq!(d.code, "E_TYPE", "{table}: {d}");
        assert!(d.message.contains(&format!("`{key}`")), "{table}: {d}");
        let at = d.location.as_ref().expect("located");
        assert_eq!(at.line, 4, "{table}: {d}");
    }
}

/// §6.1: `git/ignore` is rendered only when `ignores` holds a pattern.
#[test]
fn git_ignore_is_absent_when_ignores_is_empty() {
    for table in ["user.name = \"N\"", "ignores = []"] {
        let env = home_env("git-no-ignore");
        write_manifest(&env, &format!("{HEAD}[programs.git]\n{table}\n"));
        let files: Vec<String> = rendered(&env, "git").into_keys().collect();
        assert_eq!(files, [".config/git/config"], "{table}");
    }
}

/// I13, I15, decision D1: `W_PROGRAM_NOT_FOUND` when git is not on `PATH` and `W_SHADOWED` naming
/// `~/.gitconfig`, on plan, apply and status, exit 0 — the file rendered all the same, nothing
/// installed, `~/.gitconfig` untouched. With a `git` on `PATH`, no `W_PROGRAM_NOT_FOUND`.
#[test]
fn git_warns_when_absent_or_shadowed_and_installs_nothing() {
    let env = home_env("git-warnings");
    write_manifest(
        &env,
        &format!("{HEAD}[programs.git]\nuser.name = \"Example User\"\n"),
    );
    let gitconfig = env.home().join(".gitconfig");
    fs::write(&gitconfig, "[user]\n\tname = older\n").unwrap();
    let empty = path_with(&env, "empty", &[]);
    let not_found = "lodi: warning W_PROGRAM_NOT_FOUND: [programs.git] is declared, but `git` is \
                     not on PATH; its files are rendered all the same, and lodi installs nothing: \
                     install it with [tools], the host scope's [packages] or the distribution";
    let shadowed = "lodi: warning W_SHADOWED: .gitconfig exists, and git reads it before or \
                    alongside .config/git/config; move its content into [programs.git] and \
                    delete it";

    let before = home_gate().ledger_lines().len();
    for verb in ["plan", "apply", "status"] {
        let out = command(&env, &empty).args(["home", verb]).output().unwrap();
        assert_eq!(out.status.code(), Some(0), "{verb}: {}", stderr(&out));
        let lines: Vec<String> = stderr(&out).lines().map(str::to_string).collect();
        assert_eq!(lines, [not_found, shadowed], "{verb}");
    }
    let written = env.home().join(".config/git/config");
    assert_eq!(
        fs::read(&written).unwrap(),
        fs::read(fixture("git").join("expected/config"))
            .map(|_| rendered(&env, "git")[".config/git/config"].clone())
            .unwrap()
    );
    assert_eq!(
        fs::read_to_string(&gitconfig).unwrap(),
        "[user]\n\tname = older\n"
    );
    for (op, path) in ledger_since(&env, before) {
        assert!(
            path == written
                || path.starts_with(data(&env).join("home-scope"))
                || data(&env).starts_with(&path)
                || path.starts_with(env.home().join(".config/git")),
            "I15: the ledger names {op} {} — nothing but the rendered file and the state",
            path.display()
        );
        assert!(!path.starts_with(data(&env).join("store")), "{op} {path:?}");
        assert_ne!(path, gitconfig, "{op}: ~/.gitconfig was touched");
    }

    let with_git = path_with(&env, "git", &["git"]);
    let out = command(&env, &with_git)
        .args(["home", "plan"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), format!("{shadowed}\n"));
}

// ---------------------------------------------------------------------------------- shells ----

/// I17's local half: each shell golden passes its shell's own syntax check where the shell is on
/// `PATH`; a shell that is not is reported as not run.
#[test]
fn each_shell_golden_passes_its_shells_syntax_check() {
    for (shell, golden) in [
        ("bash", "bash/expected/init.bash"),
        ("zsh", "zsh/expected/init.zsh"),
        ("fish", "fish/expected/lodi.fish"),
    ] {
        let Some(exe) = on_path(shell) else {
            eprintln!("not run: {shell} -n, {shell} is not on PATH");
            continue;
        };
        let file = repo().join("tests/fixtures/programs").join(golden);
        let out = Command::new(exe)
            .arg("-n")
            .arg(&file)
            .env_clear()
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{shell} -n {golden}: {}",
            stderr(&out)
        );
    }
}

/// §5: the file's order is fixed — header; tools; env sorted; path; aliases sorted; the shell's
/// typed keys; snippets in declared order; extra — and without `[tools]` the tools line is gone
/// and nothing else changes.
#[test]
fn a_shell_file_keeps_the_fixed_order_and_includes_the_tools_only_with_tools() {
    let golden = fs::read_to_string(fixture("bash").join("expected/init.bash")).unwrap();
    let at = |needle: &str| {
        golden
            .find(needle)
            .unwrap_or_else(|| panic!("{needle} missing:\n{golden}"))
    };
    let order = [
        "# Managed by lodi",
        "home-scope/profile.sh",
        "export EDITOR=",
        "export GOPATH=",
        "export PAGER=",
        "export QUOTED=",
        "PATH=\"$HOME\"/'go/bin'",
        "export PATH",
        "alias gs=",
        "alias la=",
        "alias ll=",
        "HISTSIZE=",
        "HISTFILESIZE=",
        "HISTCONTROL=",
        "shopt -s histappend",
        "PS1=",
        "echo no trailing newline",
        "bind '",
    ];
    for pair in order.windows(2) {
        assert!(at(pair[0]) < at(pair[1]), "{} after {}", pair[0], pair[1]);
    }
    assert!(
        golden.contains("export GOPATH=\"$HOME\"/'go'\n"),
        "{golden}"
    );
    assert!(
        golden.contains("export QUOTED='it'\"'\"'s $literal'\n"),
        "{golden}"
    );

    for module in ["bash", "zsh", "fish"] {
        let env = home_env("shell-no-tools");
        install(&env, module, "home.toml");
        let text = fs::read_to_string(manifest_dir(&env).join("home.toml")).unwrap();
        write_manifest(&env, &text.replace("[tools]\njq = \"latest\"\n", ""));
        let files = rendered(&env, module);
        let without = String::from_utf8(files.into_values().next().unwrap()).unwrap();
        let name = if module == "fish" {
            "lodi.fish"
        } else {
            &format!("init.{module}")
        };
        let with = fs::read_to_string(fixture(module).join("expected").join(name)).unwrap();
        let tools_line: Vec<&str> = with
            .lines()
            .filter(|l| l.contains("home-scope/profile"))
            .collect();
        assert_eq!(tools_line.len(), 1, "{module}:\n{with}");
        assert_eq!(
            without,
            with.replace(&format!("{}\n", tools_line[0]), ""),
            "{module}"
        );
    }
}

/// §5: names outside the grammar are `E_TYPE`, and so is a value no shell word can carry.
#[test]
fn shell_names_outside_the_grammar_are_type_errors() {
    for (module, table, key) in [
        ("bash", "env = { \"1X\" = \"v\" }", "env.1X"),
        ("bash", "env = { \"A-B\" = \"v\" }", "env.A-B"),
        ("zsh", "aliases = { \"a b\" = \"ls\" }", "aliases.a b"),
        ("bash", "aliases = { \"a=b\" = \"ls\" }", "aliases.a=b"),
        ("fish", "aliases = { \"a/b\" = \"ls\" }", "aliases.a/b"),
        ("fish", "abbrs = { \"g'\" = \"git\" }", "abbrs.g'"),
        ("bash", "path = [\"a:b\"]", "path"),
        ("zsh", "env = { A = \"x\\u0000y\" }", "env.A"),
        ("bash", "shopt = [\"Bad\"]", "shopt"),
        ("zsh", "setopt = [\"no-beep\"]", "setopt"),
        ("bash", "histcontrol = [\"ignoreall\"]", "histcontrol"),
        ("bash", "histsize = -1", "histsize"),
        ("fish", "greeting = 1", "greeting"),
    ] {
        let ds = refused(&format!("{HEAD}[programs.{module}]\n{table}\n"));
        assert!(
            ds.iter()
                .all(|d| d.code == "E_TYPE" && d.message.contains(&format!("`{key}`"))),
            "{module} {table}: {ds:#?}"
        );
        assert!(!ds.is_empty());
    }
}

/// §5, §8: a `snippets` path that leaves the manifest's directory is `E_PATH_ESCAPE`; one that is
/// missing, a directory, a link or not UTF-8 text is `E_CONFIG` — each at the key's value.
#[test]
fn a_snippet_is_a_regular_file_below_the_manifest() {
    let env = home_env("snippet-refused");
    let dir = manifest_dir(&env);
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("ok.sh"), "true\n").unwrap();
    fs::write(dir.join("binary.sh"), [0xff, 0xfe, b'\n']).unwrap();
    std::os::unix::fs::symlink("ok.sh", dir.join("link.sh")).unwrap();
    for (path, code) in [
        ("../outside.sh", "E_PATH_ESCAPE"),
        ("/etc/hostname", "E_PATH_ESCAPE"),
        ("missing.sh", "E_CONFIG"),
        ("sub", "E_CONFIG"),
        ("link.sh", "E_CONFIG"),
        ("binary.sh", "E_CONFIG"),
    ] {
        write_manifest(
            &env,
            &format!("{HEAD}[programs.bash]\nsnippets = [\"ok.sh\", {path:?}]\n"),
        );
        let ds = load(&env).expect_err(path);
        assert_eq!(ds.len(), 1, "{path}: {ds:#?}");
        assert_eq!(ds[0].code, code, "{path}: {}", ds[0]);
        assert_eq!(ds[0].location.as_ref().map(|l| l.line), Some(4), "{path}");
    }
    write_manifest(
        &env,
        &format!("{HEAD}[programs.bash]\nsnippets = [\"./ok.sh\"]\n"),
    );
    let files = rendered(&env, "bash");
    let text = String::from_utf8(files.into_values().next().unwrap()).unwrap();
    assert!(text.ends_with("\ntrue\n"), "{text}");
}

/// §4.1, §5: a shell module renders into the data root, which must be below the home directory
/// like every managed path; a data root outside it is `E_CONFIG`.
#[test]
fn a_shell_module_needs_the_data_root_below_home() {
    let env = home_env("shell-data-outside");
    write_manifest(&env, &format!("{HEAD}[programs.zsh]\nhistsize = 1\n"));
    let out = env.command().args(["home", "plan"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3), "{}", stderr(&out));
    assert!(stderr(&out).contains("E_CONFIG"), "{}", stderr(&out));
    assert!(stderr(&out).contains("renders a file outside the home directory"));
}

fn tool_declaration(url: &str, body: &[u8]) -> String {
    format!(
        "[tools.alpha]\nversion = \"1.0.0\"\nurl = \"{url}\"\nsha256 = \"{}\"\n\
         format = \"binary\"\npath = [\"bin\"]\nenv = {{ ALPHA_VERSION = \"${{self.version}}\" }}\n",
        sha256_hex(body)
    )
}

const SHELLS: &str = r#"
[programs.bash]
env = { EDITOR = "nvim", QUOTED = "it's $x" }
path = ["~/.local/bin"]
aliases = { ll = "ls -l" }

[programs.zsh]
env = { EDITOR = "nvim", QUOTED = "it's $x" }
path = ["~/.local/bin"]
aliases = { ll = "ls -l" }

[programs.fish]
env = { EDITOR = "nvim" }
path = ["~/.local/bin"]
"#;

/// §5, I6, I16, decision D3, end to end with a real tool: apply prints the `profile.sh` line and
/// then one line per bash and zsh file, last; sourcing either file (twice) by hand in a clean
/// shell puts the tool and the declared environment in place exactly once; fish's drop-in
/// sources the fish twin of the profile, which sets the same bin directory and environment with
/// no universal variable; no rc file is ever named in the ledger. Removing `[programs.fish]`
/// removes the drop-in and the twin.
#[test]
fn the_shell_files_include_the_tools_and_apply_prints_their_lines_last() {
    let env = home_env("shells-e2e");
    let url = "https://fixtures.test/alpha-shells";
    let body = b"#!/bin/sh\necho alpha\n";
    let server = Server::start(BTreeMap::from([(url.to_string(), body.to_vec())]));
    let tools = tool_declaration(url, body);
    write_manifest(&env, &format!("{HEAD}{SHELLS}\n{tools}"));
    let path = path_with(&env, "shells", &["bash", "fish", "zsh"]);
    let run = |args: &[&str]| {
        command(&env, &path)
            .env("LODI_FETCH_REWRITE", server.rewrite())
            .args(args)
            .output()
            .unwrap()
    };

    let before = home_gate().ledger_lines().len();
    let out = run(&["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(
        stderr(&out),
        "",
        "the shells are on PATH, nothing is shadowed"
    );
    let shown = stdout(&out);
    assert!(
        shown.ends_with(
            "add this line to your shell rc file (lodi never edits it):\n  . \
             \"$HOME/.local/share/lodi/home-scope/profile.sh\"\n\
             add this line to your bash rc file (lodi never edits it):\n  . \
             \"$HOME/.local/share/lodi/home-scope/shell/init.bash\"\n\
             add this line to your zsh rc file (lodi never edits it):\n  . \
             \"$HOME/.local/share/lodi/home-scope/shell/init.zsh\"\n"
        ),
        "{shown}"
    );
    for (op, path) in ledger_since(&env, before) {
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        assert!(
            ![
                ".bashrc",
                ".zshrc",
                ".profile",
                ".bash_profile",
                ".zprofile",
                "config.fish"
            ]
            .contains(&name),
            "I6/I16: the ledger names {op} {}",
            path.display()
        );
    }
    for rc in [".bashrc", ".zshrc", ".profile", ".config/fish/config.fish"] {
        assert!(!env.home().join(rc).exists(), "{rc} was created");
    }

    let bin = data(&env).join("home-scope/profile/bin");
    let local = env.home().join(".local/bin");
    // `PATH` is set inside the script as well: a system `/etc/zshenv` is read even by `zsh -f`.
    for (shell, args, file) in [
        ("bash", &["--norc", "--noprofile", "-c"][..], "init.bash"),
        ("zsh", &["-f", "-c"][..], "init.zsh"),
    ] {
        let Some(exe) = on_path(shell) else {
            eprintln!("not run: sourcing {file}, {shell} is not on PATH");
            continue;
        };
        let out = Command::new(exe)
            .args(args)
            .arg(
                "PATH=/usr/bin:/bin; f=\"$HOME/.local/share/lodi/home-scope/shell/$1\"; \
                 . \"$f\"; . \"$f\"; \
                 command -v alpha; printf '%s\\n' \"$PATH\" \"$ALPHA_VERSION\" \"$EDITOR\" \
                 \"$QUOTED\"; alias ll",
            )
            .arg(shell)
            .arg(file)
            .env_clear()
            .env("HOME", env.home())
            .env("PATH", "/usr/bin:/bin")
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell}: {}", stderr(&out));
        let text = stdout(&out);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], bin.join("alpha").display().to_string(), "{shell}");
        // Sourcing twice leaves each directory on PATH once. The second `profile.sh` moves the
        // tools' bin directory back to the front, as it does whenever it is sourced; a `path`
        // entry already on PATH is left where it is.
        let mut dirs: Vec<&str> = lines[1].split(':').collect();
        dirs.sort();
        let mut want = [
            local.to_str().unwrap(),
            bin.to_str().unwrap(),
            "/usr/bin",
            "/bin",
        ];
        want.sort();
        assert_eq!(dirs, want, "{shell}: sourcing twice is harmless");
        assert_eq!(&lines[2..5], ["1.0.0", "nvim", "it's $x"], "{shell}");
        assert!(
            lines[5].contains("ll=") && lines[5].contains("ls -l"),
            "{shell}: {text}"
        );
    }

    let dropin = env.home().join(".config/fish/conf.d/lodi.fish");
    let dropin_text = fs::read_to_string(&dropin).unwrap();
    assert!(
        dropin_text.contains(
            "if test -r \"$HOME\"/'.local/share/lodi/home-scope/profile.fish'; source \
             \"$HOME\"/'.local/share/lodi/home-scope/profile.fish'; end\n"
        ),
        "{dropin_text}"
    );
    let twin = fs::read_to_string(data(&env).join("home-scope/profile.fish")).unwrap();
    assert!(
        twin.contains(&format!("set -l _lodi_bin '{}'\n", bin.display())),
        "{twin}"
    );
    assert!(
        twin.contains("set -gx PATH $_lodi_bin $_lodi_path\n"),
        "{twin}"
    );
    assert!(twin.contains("set -gx ALPHA_VERSION '1.0.0'\n"), "{twin}");
    for text in [&dropin_text, &twin] {
        assert!(
            !text.contains(" -U") && !text.contains("--universal") && !text.contains(" -Ux"),
            "a universal variable: {text}"
        );
    }
    if let Some(fish) = on_path("fish") {
        for file in [&dropin, &data(&env).join("home-scope/profile.fish")] {
            let out = Command::new(&fish).arg("-n").arg(file).output().unwrap();
            assert!(
                out.status.success(),
                "fish -n {}: {}",
                file.display(),
                stderr(&out)
            );
        }
    } else {
        eprintln!("not run: fish -n on lodi.fish and profile.fish, fish is not on PATH");
    }

    // With fish as $SHELL and [programs.fish] declared, status prints neither W_FISH_HOOKS nor the
    // POSIX line; without the module, W_FISH_HOOKS is back.
    let status = |shell: &str| {
        command(&env, &path)
            .env("SHELL", shell)
            .args(["home", "status"])
            .output()
            .unwrap()
    };
    let out = status("/usr/bin/fish");
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(!stderr(&out).contains("W_FISH_HOOKS"), "{}", stderr(&out));
    assert!(!stdout(&out).contains(
        "rc file (lodi never edits it):\n  . \"$HOME/.local/share/lodi/home-scope/profile.sh\""
    ));

    let without_fish = SHELLS.split("[programs.fish]").next().unwrap().to_string();
    write_manifest(&env, &format!("{HEAD}{without_fish}\n{tools}"));
    let out = run(&["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        !dropin.exists(),
        "removing [programs.fish] removes lodi.fish"
    );
    assert!(!data(&env).join("home-scope/profile.fish").exists());
    let out = status("/usr/bin/fish");
    assert!(stderr(&out).contains("W_FISH_HOOKS"), "{}", stderr(&out));
}

/// I8, I9, I16, decision D3: `conf.d/lodi.fish` is an ordinary owned file — a state record with
/// its module and hash, drift at exit 8 with nothing written, `--overwrite-drift`, a file already
/// there backed up once and restored with its mode when the table goes.
#[test]
fn the_fish_dropin_is_an_ordinary_owned_file() {
    let env = home_env("fish-owned");
    let path = path_with(&env, "fish", &["fish"]);
    let dropin = env.home().join(".config/fish/conf.d/lodi.fish");
    fs::create_dir_all(dropin.parent().unwrap()).unwrap();
    fs::write(&dropin, "set -g mine 1\n").unwrap();
    fs::set_permissions(&dropin, fs::Permissions::from_mode(0o600)).unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[programs.fish]\ngreeting = \"\"\nabbrs = {{ gs = \"git status\" }}\n"),
    );
    let run = |args: &[&str]| command(&env, &path).args(args).output().unwrap();

    let out = run(&["home", "plan"]);
    assert!(
        stdout(&out).contains("backup: first unmanaged copy kept"),
        "{}",
        stdout(&out)
    );
    let out = run(&["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert!(
        stderr(&out).contains("W_REPLACED_UNMANAGED"),
        "{}",
        stderr(&out)
    );
    let bytes = fs::read(&dropin).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(
        text.contains("abbr --add --global gs 'git status'\n"),
        "{text}"
    );
    assert!(text.contains("set -g fish_greeting ''\n"), "{text}");
    let record = &state(&env)["files"][".config/fish/conf.d/lodi.fish"];
    assert_eq!(record["origin"]["kind"], "program", "{record}");
    assert_eq!(record["origin"]["module"], "fish", "{record}");
    assert_eq!(record["sha256"], sha256_hex(&bytes), "{record}");

    // A hand edit is drift: exit 8, nothing written.
    fs::write(&dropin, format!("{text}set -g edited 1\n")).unwrap();
    let before = home_gate().ledger_lines().len();
    let out = run(&["home", "apply"]);
    assert_eq!(out.status.code(), Some(8), "{}", stderr(&out));
    assert!(stderr(&out).contains("E_DRIFT"), "{}", stderr(&out));
    assert!(
        ledger_since(&env, before).is_empty(),
        "a refused apply wrote"
    );
    let out = run(&["home", "apply", "--overwrite-drift"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read(&dropin).unwrap(), bytes);

    // Removing the table restores the first unmanaged bytes and mode.
    write_manifest(&env, HEAD);
    let out = run(&["home", "apply"]);
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(fs::read_to_string(&dropin).unwrap(), "set -g mine 1\n");
    assert_eq!(
        fs::metadata(&dropin).unwrap().permissions().mode() & 0o7777,
        0o600
    );
    assert!(!env.home().join(".config/fish/config.fish").exists());
}

/// §3.1: `enable = false` renders nothing, so no line is printed and no warning is given.
#[test]
fn a_disabled_shell_module_prints_no_line() {
    let env = home_env("shell-disabled");
    write_manifest(
        &env,
        &format!("{HEAD}[programs.bash]\nenable = false\nhistsize = 1\n"),
    );
    let empty = path_with(&env, "none", &[]);
    let out = command(&env, &empty)
        .args(["home", "apply"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(0), "{}", stderr(&out));
    assert_eq!(stderr(&out), "");
    assert!(!stdout(&out).contains("rc file"), "{}", stdout(&out));
    assert!(!data(&env).join("home-scope/shell/init.bash").exists());
}

/// The registry rows: each shipped module looks for the executable of its own name, but helix,
/// whose executable is `hx`.
#[test]
fn each_shipped_module_looks_for_its_own_executable() {
    for module in programs::registry() {
        let expected = if module.name == "helix" {
            "hx"
        } else {
            module.name
        };
        assert_eq!(module.executable, expected);
    }
}

// ----------------------------------------------------------- editors, terminals, prompt (pb) ---

/// I12: each TOML golden (helix, alacritty, starship) parses back, header and all, with the
/// project's own TOML parser, and writing what was read gives the golden's body again.
#[test]
fn each_toml_golden_parses_back_to_its_own_body() {
    for golden in [
        "helix/expected/config.toml",
        "alacritty/expected/alacritty.toml",
        "starship/expected/starship.toml",
    ] {
        let text = fs::read_to_string(repo().join("tests/fixtures/programs").join(golden)).unwrap();
        let table = lodi::home::toml::parse(&text).unwrap_or_else(|e| panic!("{golden}: {e:?}"));
        let body = text.split_once("\n\n").unwrap().1;
        assert_eq!(lodi::home::render::toml_document(&table), body, "{golden}");
    }
}

/// A scratch directory below the test's root, for an application's own home and runtime files.
fn scratch(env: &HomeEnv, name: &str) -> PathBuf {
    let dir = env.root().join(name);
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// I17's local half for pb: nvim, tmux and starship read their goldens without an error where
/// they are on `PATH`, each under a scratch home and runtime directory; an application that is
/// not on `PATH` (alacritty, kitty and helix on the build machine) is reported as not run.
#[test]
fn the_applications_on_path_read_their_goldens() {
    let env = home_env("pb-apps");
    let golden = |path: &str| repo().join("tests/fixtures/programs").join(path);
    let home = scratch(&env, "app-home");

    match on_path("nvim") {
        None => eprintln!("not run: nvim --headless, nvim is not on PATH"),
        Some(nvim) => {
            let out = Command::new(nvim)
                .args(["--headless", "-i", "NONE", "-u"])
                .arg(golden("nvim/expected/init.lua"))
                .args([
                    "+lua io.stdout:write(vim.g.mapleader .. '|' .. vim.o.shiftwidth)",
                    "+qa!",
                ])
                .env_clear()
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", home.join(".config"))
                .env("XDG_DATA_HOME", home.join(".local/share"))
                .env("XDG_STATE_HOME", home.join(".local/state"))
                .env("XDG_CACHE_HOME", home.join(".cache"))
                .env("XDG_RUNTIME_DIR", scratch(&env, "nvim-run"))
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "nvim: {}", stderr(&out));
            assert_eq!(stderr(&out), "", "nvim reported an error");
            assert_eq!(stdout(&out), " |4", "nvim did not apply the golden");
        }
    }

    match on_path("tmux") {
        None => eprintln!("not run: tmux -f, tmux is not on PATH"),
        Some(tmux) => {
            // A private socket in a scratch directory: the server this starts is stopped by the
            // same command line, and no other server is reached. The socket is named relative to
            // that directory (`-S` from inside it), because a socket's absolute path is limited
            // to ~108 bytes and a checkout's target directory can be longer than that (#308).
            let run = scratch(&env, "tmux-run");
            let out = Command::new(tmux)
                .args(["-S", "lodi-pb", "-f"])
                .arg(golden("tmux/expected/tmux.conf"))
                .args(["start-server", ";", "show-options", "-g", "prefix", ";"])
                .args(["show-options", "-g", "history-limit", ";", "kill-server"])
                .current_dir(&run)
                .env_clear()
                .env("HOME", &home)
                .env("TMUX_TMPDIR", &run)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "tmux: {}", stderr(&out));
            assert_eq!(stderr(&out), "", "tmux reported an error");
            assert_eq!(stdout(&out), "prefix C-a\nhistory-limit 50000\n");
        }
    }

    match on_path("starship") {
        None => eprintln!("not run: starship print-config, starship is not on PATH"),
        Some(starship) => {
            // At starship's default path, with STARSHIP_CONFIG unset (§6.10).
            let config = home.join(".config/starship.toml");
            fs::create_dir_all(config.parent().unwrap()).unwrap();
            fs::copy(golden("starship/expected/starship.toml"), &config).unwrap();
            let out = Command::new(starship)
                .args(["print-config", "format", "add_newline", "git_branch.symbol"])
                .env_clear()
                .env("HOME", &home)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap();
            assert!(out.status.success(), "starship: {}", stderr(&out));
            let text = stdout(&out);
            assert!(
                text.contains("format = \"$directory$git_branch$character\""),
                "{text}"
            );
            assert!(text.contains("add_newline = false"), "{text}");
            assert!(text.contains("symbol = \"git \""), "{text}");
        }
    }
    for app in ["alacritty", "kitty", "hx"] {
        if on_path(app).is_none() {
            eprintln!("not run: {app} is not on PATH");
        }
    }
}

/// §6.5–§6.10, §8: every enumerated value outside its set and every number outside its range is
/// `E_TYPE` at the value, the hint naming the allowed values or the range; a wrong type is
/// `E_TYPE`, and an unknown key `E_UNKNOWN_ATTR` naming the nearest key.
#[test]
fn the_pb_modules_refuse_values_outside_their_sets_and_ranges() {
    for (module, line, key, hint) in [
        (
            "nvim",
            "opt.shiftwidth = -1",
            "opt.shiftwidth",
            "from 0 to 2147483647",
        ),
        (
            "nvim",
            "opt.tabstop = 2147483648",
            "opt.tabstop",
            "from 0 to 2147483647",
        ),
        ("nvim", "opt.number = \"yes\"", "opt.number", ""),
        ("nvim", "leader = 1", "leader", ""),
        (
            "helix",
            "editor.line-number = \"hybrid\"",
            "editor.line-number",
            "one of: absolute, relative",
        ),
        (
            "helix",
            "editor.cursor-shape.insert = \"beam\"",
            "editor.cursor-shape.insert",
            "one of: block, bar, underline, hidden",
        ),
        (
            "helix",
            "editor.rulers = [0]",
            "editor.rulers",
            "from 1 to 65535",
        ),
        (
            "helix",
            "editor.rulers = [80, \"100\"]",
            "editor.rulers",
            "",
        ),
        ("alacritty", "font.size = 0", "font.size", "greater than 0"),
        (
            "alacritty",
            "font.size = -1.5",
            "font.size",
            "greater than 0",
        ),
        (
            "alacritty",
            "window.opacity = 1.5",
            "window.opacity",
            "from 0 to 1",
        ),
        (
            "alacritty",
            "window.opacity = -0.1",
            "window.opacity",
            "from 0 to 1",
        ),
        (
            "alacritty",
            "window.padding.x = -1",
            "window.padding.x",
            "from 0 to",
        ),
        (
            "alacritty",
            "window.decorations = \"none\"",
            "window.decorations",
            "one of: Full, None, Transparent, Buttonless",
        ),
        (
            "alacritty",
            "scrolling.history = 100001",
            "scrolling.history",
            "from 0 to 100000",
        ),
        ("kitty", "font_size = 0", "font_size", "greater than 0"),
        (
            "kitty",
            "background_opacity = 2",
            "background_opacity",
            "from 0 to 1",
        ),
        (
            "kitty",
            "font_family = \"a\\nb\"",
            "font_family",
            "one line",
        ),
        (
            "kitty",
            "map = { \"ctrl+a\" = \"a\\nb\" }",
            "map.ctrl+a",
            "one line",
        ),
        (
            "kitty",
            "map = { \"ctrl a\" = \"copy\" }",
            "map.ctrl a",
            "without blanks",
        ),
        (
            "kitty",
            "enable_audio_bell = \"no\"",
            "enable_audio_bell",
            "",
        ),
        (
            "tmux",
            "base-index = -1",
            "base-index",
            "from 0 to 2147483647",
        ),
        (
            "tmux",
            "history-limit = 2147483648",
            "history-limit",
            "from 0 to 2147483647",
        ),
        (
            "tmux",
            "mode-keys = \"vim\"",
            "mode-keys",
            "one of: vi, emacs",
        ),
        ("tmux", "prefix = \"C-a\\nC-b\"", "prefix", "one line"),
        ("tmux", "default-terminal = \"\"", "default-terminal", ""),
        (
            "starship",
            "command_timeout = -1",
            "command_timeout",
            "from 0 to",
        ),
        ("starship", "scan_timeout = -5", "scan_timeout", "from 0 to"),
        (
            "starship",
            "shells = [\"bash\", \"tcsh\"]",
            "shells",
            "one of: bash, zsh, fish",
        ),
        ("starship", "shells = \"bash\"", "shells", ""),
    ] {
        let ds = refused(&format!("{HEAD}[programs.{module}]\n{line}\n"));
        assert_eq!(ds.len(), 1, "{module} {line}: {ds:#?}");
        let d = &ds[0];
        assert_eq!(d.code, "E_TYPE", "{module} {line}: {d}");
        assert!(
            d.message.contains(&format!("`{key}`")),
            "{module} {line}: {d}"
        );
        assert!(
            hint.is_empty() || d.notes.iter().any(|n| n.contains(hint)),
            "{module} {line}: the hint does not say {hint:?}: {d}"
        );
        assert_eq!(
            d.location.as_ref().expect("located").line,
            4,
            "{module} {line}: {d}"
        );
    }

    let ds = refused(&format!("{HEAD}[programs.nvim]\nopt.numbr = true\n"));
    assert_eq!(ds.len(), 1, "{ds:#?}");
    assert_eq!(ds[0].code, "E_UNKNOWN_ATTR");
    assert!(
        ds[0]
            .notes
            .iter()
            .any(|n| n.contains("did you mean `opt.number`?")),
        "{}",
        ds[0]
    );
    let ds = refused(&format!("{HEAD}[programs.tmux]\nstatus-left = \"x\"\n"));
    assert_eq!(ds[0].code, "E_UNKNOWN_ATTR", "{}", ds[0]);
}

/// §6, I11: a TOML `extra` (helix, alacritty, starship) that sets a typed key is
/// `E_ATTR_CONFLICT`, and one that does not parse is `E_SYNTAX`; `shells` is never written.
#[test]
fn a_toml_extra_that_sets_a_typed_key_is_a_conflict() {
    for (module, typed, extra, path) in [
        (
            "helix",
            "editor.mouse = false",
            "[editor]\nmouse = true\n",
            "editor.mouse",
        ),
        (
            "alacritty",
            "font.size = 11",
            "font = { size = 12.0 }\n",
            "font.size",
        ),
        (
            "starship",
            "add_newline = false",
            "add_newline = true\n",
            "add_newline",
        ),
    ] {
        let ds = refused(&format!(
            "{HEAD}[programs.{module}]\n{typed}\nextra = '''\n{extra}'''\n"
        ));
        assert_eq!(ds.len(), 1, "{module}: {ds:#?}");
        assert_eq!(ds[0].code, "E_ATTR_CONFLICT", "{module}: {}", ds[0]);
        assert!(ds[0].to_string().contains(path), "{module}: {}", ds[0]);

        let ds = refused(&format!(
            "{HEAD}[programs.{module}]\n{typed}\nextra = \"x = \"\n"
        ));
        assert_eq!(ds[0].code, "E_SYNTAX", "{module}: {}", ds[0]);
    }
    let files = render_fixture("pb-shells-key", "starship", "home.toml");
    let text = String::from_utf8(files[".config/starship.toml"].clone()).unwrap();
    assert!(!text.contains("shells"), "{text}");
}

/// §4.6, I13, I15: `nvim/init.vim` beside `[programs.nvim]` and `~/.tmux.conf` beside
/// `[programs.tmux]` are `W_SHADOWED` on plan, apply and status, after `W_PROGRAM_NOT_FOUND` when
/// the executable is not on `PATH`; the exit status is 0, the file is rendered, and the shadowing
/// file is never touched.
#[test]
fn nvim_and_tmux_warn_when_shadowed_or_absent() {
    for (module, executable, shadow, rendered_at) in [
        (
            "nvim",
            "nvim",
            ".config/nvim/init.vim",
            ".config/nvim/init.lua",
        ),
        ("tmux", "tmux", ".tmux.conf", ".config/tmux/tmux.conf"),
    ] {
        let env = home_env(&format!("pb-shadow-{module}"));
        write_manifest(&env, &format!("{HEAD}[programs.{module}]\n"));
        let by = env.home().join(shadow);
        fs::create_dir_all(by.parent().unwrap()).unwrap();
        fs::write(&by, "older\n").unwrap();
        let not_found = format!(
            "lodi: warning W_PROGRAM_NOT_FOUND: [programs.{module}] is declared, but \
             `{executable}` is not on PATH; its files are rendered all the same, and lodi \
             installs nothing: install it with [tools], the host scope's [packages] or the \
             distribution"
        );
        let shadowed = format!(
            "lodi: warning W_SHADOWED: {shadow} exists, and {module} reads it before or \
             alongside {rendered_at}; move its content into [programs.{module}] and delete it"
        );
        let empty = path_with(&env, "empty", &[]);
        for verb in ["plan", "apply", "status"] {
            let out = command(&env, &empty).args(["home", verb]).output().unwrap();
            assert_eq!(
                out.status.code(),
                Some(0),
                "{module} {verb}: {}",
                stderr(&out)
            );
            let lines: Vec<String> = stderr(&out).lines().map(str::to_string).collect();
            assert_eq!(
                lines,
                [not_found.clone(), shadowed.clone()],
                "{module} {verb}"
            );
        }
        assert!(env.home().join(rendered_at).is_file(), "{module}");
        assert_eq!(fs::read_to_string(&by).unwrap(), "older\n", "{module}");

        let with = path_with(&env, module, &[executable]);
        let out = command(&env, &with)
            .args(["home", "plan"])
            .output()
            .unwrap();
        assert_eq!(stderr(&out), format!("{shadowed}\n"), "{module}");
    }

    // helix looks for `hx`, not `helix`.
    let env = home_env("pb-helix-hx");
    write_manifest(
        &env,
        &format!("{HEAD}[programs.helix]\ntheme = \"onedark\"\n"),
    );
    let named_helix = path_with(&env, "helix", &["helix"]);
    let out = command(&env, &named_helix)
        .args(["home", "plan"])
        .output()
        .unwrap();
    assert!(
        stderr(&out).contains("`hx` is not on PATH"),
        "{}",
        stderr(&out)
    );
    let hx = path_with(&env, "hx", &["hx"]);
    let out = command(&env, &hx).args(["home", "plan"]).output().unwrap();
    assert_eq!(stderr(&out), "");
}

const STARSHIP_SHELLS: &str = r#"
[programs.bash]
histsize = 100
snippets = ["snippets/after.sh"]

[programs.zsh]
savehist = 100
snippets = ["snippets/after.sh"]

[programs.fish]
greeting = ""
"#;

/// Render the three shell files with `starship` as given (`None`: no `[programs.starship]`), and
/// return each shell's file text, by module name.
fn shell_files(tag: &str, starship: Option<&str>) -> BTreeMap<String, String> {
    let env = home_env(tag);
    let mut text = format!("{HEAD}{STARSHIP_SHELLS}");
    if let Some(table) = starship {
        text.push_str(&format!("\n[programs.starship]\n{table}\n"));
    }
    write_manifest(&env, &text);
    let snippets = manifest_dir(&env).join("snippets");
    fs::create_dir_all(&snippets).unwrap();
    fs::write(snippets.join("after.sh"), "echo after-snippet\n").unwrap();
    ["bash", "zsh", "fish"]
        .iter()
        .map(|shell| {
            let files = rendered(&env, shell);
            assert_eq!(files.len(), 1, "{shell}");
            let text = String::from_utf8(files.into_values().next().unwrap()).unwrap();
            (shell.to_string(), text)
        })
        .collect()
}

const INIT_BASH: &str =
    "if command -v starship >/dev/null 2>&1; then eval \"$(starship init bash)\"; fi\n";
const INIT_ZSH: &str =
    "if command -v starship >/dev/null 2>&1; then eval \"$(starship init zsh)\"; fi\n";
const INIT_FISH: &str = "if command -v starship >/dev/null; starship init fish | source; end\n";

/// §5, §6.10: `shells` defaults to every declared shell module; the guarded `init` line comes
/// after the shell's typed lines and before its snippets; an explicit `shells` picks the files,
/// and a disabled or absent `[programs.starship]` adds nothing. `STARSHIP_CONFIG` is never set.
#[test]
fn starship_adds_its_guarded_init_line_to_the_declared_shell_files() {
    let every = shell_files("pb-starship-default", Some("add_newline = false"));
    assert!(every["bash"].contains(&format!("HISTSIZE=100\n{INIT_BASH}echo after-snippet\n")));
    assert!(every["zsh"].contains(&format!("SAVEHIST=100\n{INIT_ZSH}echo after-snippet\n")));
    assert!(
        every["fish"].ends_with(&format!("set -g fish_greeting ''\n{INIT_FISH}")),
        "{}",
        every["fish"]
    );
    for text in every.values() {
        assert!(!text.contains("STARSHIP_CONFIG"), "{text}");
    }

    let zsh_only = shell_files("pb-starship-zsh", Some("shells = [\"zsh\"]"));
    assert!(zsh_only["zsh"].contains(INIT_ZSH));
    assert!(
        !zsh_only["bash"].contains("starship"),
        "{}",
        zsh_only["bash"]
    );
    assert!(
        !zsh_only["fish"].contains("starship"),
        "{}",
        zsh_only["fish"]
    );

    let none = shell_files("pb-starship-empty", Some("shells = []"));
    let disabled = shell_files("pb-starship-off", Some("enable = false"));
    let absent = shell_files("pb-starship-absent", None);
    for (shell, text) in absent.iter() {
        assert!(!text.contains("starship"), "{shell}: {text}");
        assert_eq!(&none[shell], text, "{shell}: shells = [] changes the file");
        assert_eq!(
            &disabled[shell], text,
            "{shell}: enable = false changes the file"
        );
    }
}

/// §5, §6.10, real behaviour: the bash and zsh files with the `init` line pass their shell's
/// syntax check; sourced in a clean shell with starship on `PATH` they start it (starship's
/// own `init` sets `STARSHIP_SHELL`), and with it absent they source without an error or a
/// word of output. `STARSHIP_CONFIG` stays unset either way.
#[test]
fn the_starship_init_line_starts_starship_only_where_it_is_installed() {
    let files = shell_files("pb-starship-run", Some("add_newline = false"));
    let env = home_env("pb-starship-run-shells");
    let home = scratch(&env, "shell-home");
    let empty = path_with(&env, "none", &[]);
    for shell in ["bash", "zsh"] {
        let Some(exe) = on_path(shell) else {
            eprintln!("not run: {shell} with the starship line, {shell} is not on PATH");
            continue;
        };
        let file = env.root().join(format!("init.{shell}"));
        fs::write(&file, &files[shell]).unwrap();
        let clean = if shell == "bash" { "--norc" } else { "-f" };
        let out = Command::new(&exe)
            .arg("-n")
            .arg(&file)
            .env_clear()
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell} -n: {}", stderr(&out));

        // `PATH` is set inside the script too: a system-wide zsh start-up file (NixOS's
        // `/etc/zshenv`, read even under `-f`) may reset it.
        let script = format!(
            "PATH=$TEST_PATH; . '{}' >/dev/null; printf '%s|%s' \"${{STARSHIP_SHELL-unset}}\" \
             \"${{STARSHIP_CONFIG-unset}}\"",
            file.display()
        );
        let run = |path: &Path| {
            Command::new(&exe)
                .args([clean, "-c", &script])
                .env_clear()
                .env("HOME", &home)
                .env("PATH", path)
                .env("TEST_PATH", path)
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
        };
        let out = run(&empty);
        assert!(
            out.status.success(),
            "{shell} without starship: {}",
            stderr(&out)
        );
        assert_eq!(stderr(&out), "", "{shell} without starship");
        assert_eq!(stdout(&out), "unset|unset", "{shell} without starship");

        match on_path("starship") {
            None => eprintln!("not run: {shell} with starship, starship is not on PATH"),
            Some(starship) => {
                let dir = starship.parent().unwrap().to_path_buf();
                let path = std::env::join_paths([dir, empty.clone()]).unwrap();
                let out = run(Path::new(&path));
                assert!(
                    out.status.success(),
                    "{shell} with starship: {}",
                    stderr(&out)
                );
                assert_eq!(
                    stdout(&out),
                    format!("{shell}|unset"),
                    "{shell} with starship"
                );
            }
        }
    }
    if on_path("fish").is_none() {
        eprintln!("not run: fish -n with the starship line, fish is not on PATH");
    }
}
