//! `lodi import` (#693, #707 and #708, slices 1 to 3 of 3).
//!
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it (`LODI_HOST_REQUIRE_ROOT=1`), a scratch `HOME` below
//! the root, a decoy current folder holding a config, a `PATH` of only a recording `git` stub,
//! recording stub elevators (`sudo`, `doas`, `run0`), the fake's shims and `git`
//! (`tests/support/elevators.rs`, #715), and the recorded dated archives of
//! `tests/fixtures/host/pin/` served on loopback. Unless a case says otherwise it runs on a
//! terminal (`TERM=dumb`, so the step list is plain lines) whose input holds the answer "y". The
//! 1.x arming marker is removed: 2.0 reads none (#688). Nothing reaches `/` or the network.

#[path = "support/elevators.rs"]
mod elevators;
#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
#[path = "support/pinverbs.rs"]
mod pinverbs;
mod support;
#[path = "support/terminal.rs"]
mod terminal;
#[path = "support/wait.rs"]
mod wait;

use std::fs;
use std::io::Write as _;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use elevators::{ELEVATORS, Elevators};
use fakehost::{err, out, story};
use pinverbs::*;

/// The 2.0 "may manage" marker, below the root.
const MARKER: &str = "etc/lodi/may-manage";
/// The one-time question.
const QUESTION: &str = "May lodi manage this host?";

/// What standard input is: a terminal holding typed text, or a pipe holding text.
#[derive(Clone, Copy)]
enum Input<'a> {
    Terminal(&'a str),
    Pipe(&'a str),
}

/// One machine, a scratch home, a decoy current folder, and a `git` that records its calls.
struct Imp {
    v: Verbs,
    uid: u32,
    gid: u32,
    el: Elevators,
}

fn mkdir(path: &Path, mode: u32) {
    fs::create_dir_all(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

impl Imp {
    fn new(name: &str) -> Imp {
        let v = Verbs::debian(name, &[("bc", BC), ("tzdata", TZ_OLDER)]);
        v.alias_now();
        let (uid, gid) = hostroot::ids(&v.case.root);
        let el = Elevators::new(&v.case.root.dir);
        let imp = Imp { v, uid, gid, el };
        fs::remove_file(imp.path(lodi::marker::MARKER)).unwrap();
        imp.v.case.root.write(
            "etc/passwd",
            &format!("root:x:0:0::/root:/bin/sh\nsample:x:{uid}:{gid}::/home:/bin/sh\n"),
        );
        imp.v
            .case
            .root
            .write("etc/group", &format!("root:x:0:\nsample:x:{gid}:\n"));
        mkdir(&imp.path("home"), 0o755);
        mkdir(&imp.decoy(), 0o755);
        fs::write(imp.decoy().join("host.toml"), "# a decoy\n").unwrap();
        let stub = imp.path("stub");
        mkdir(&stub, 0o755);
        let real = which("git");
        let script = format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\n{}\n",
            imp.path("git-calls").display(),
            match &real {
                Some(git) => format!("exec '{}' \"$@\"", git.display()),
                None => "mkdir -p \"$3/.git\"".to_string(),
            }
        );
        {
            let _writing = fakehost::writing();
            fs::write(stub.join("git"), script).unwrap();
            fs::set_permissions(stub.join("git"), fs::Permissions::from_mode(0o755)).unwrap();
        }
        imp
    }

    fn marked(&self) -> bool {
        self.path(MARKER).is_file()
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.v.case.root.path(rel)
    }

    fn standard(&self) -> PathBuf {
        self.path("home/.config/lodi")
    }

    fn decoy(&self) -> PathBuf {
        self.path("decoy")
    }

    fn remembered(&self) -> Option<String> {
        fs::read_to_string(
            self.path("home/.local/state/lodi")
                .join(lodi::config::REMEMBERED),
        )
        .ok()
    }

    fn git_calls(&self) -> Vec<String> {
        fs::read_to_string(self.path("git-calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }

    /// `lodi ARGS --root ROOT` from `dir`, under `sh -c` so the umask is the test's.
    fn run_with(&self, dir: &Path, args: &[&str], envs: &[(&str, &str)], path: &str) -> Output {
        let yes = Input::Terminal("y\n");
        self.run_by(Command::new("/bin/sh"), dir, args, envs, path, yes)
    }

    /// As `sudo` runs it for the test's own user: root in a user namespace of its own whose root
    /// is that user, `SUDO_UID` and `SUDO_GID` naming it and `HOME` root's own.
    fn run_as_root(&self, args: &[&str], input: Input) -> Output {
        let (uid, gid) = (self.uid.to_string(), self.gid.to_string());
        let envs = [("SUDO_UID", uid.as_str()), ("SUDO_GID", gid.as_str())];
        self.run_as_root_with(args, &envs, input)
    }

    /// Root in a user namespace of its own, with `HOME` root's own and `envs`.
    fn run_as_root_with(&self, args: &[&str], envs: &[(&str, &str)], input: Input) -> Output {
        let mut unshare = Command::new(which("unshare").expect("unshare"));
        unshare.args(["--user", "--map-root-user", "/bin/sh"]);
        let root_home = self.path("root-home").display().to_string();
        let mut envs = envs.to_vec();
        envs.push(("HOME", root_home.as_str()));
        let path = self.path_var();
        self.run_by(unshare, &self.decoy(), args, &envs, &path, input)
    }

    /// `lodi ARGS --root ROOT` from `dir` with `input` on standard input; on a terminal,
    /// standard error is what the terminal showed.
    fn run_by(
        &self,
        mut command: Command,
        dir: &Path,
        args: &[&str],
        envs: &[(&str, &str)],
        path: &str,
        input: Input,
    ) -> Output {
        let rewrite = self.v.server.rewrite();
        let _spawning = fakehost::spawning();
        command
            .arg("-c")
            .arg("umask \"$LODI_TEST_UMASK\"; exec \"$0\" \"$@\"")
            .arg(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .arg("--root")
            .arg(&self.v.case.root.dir)
            .current_dir(dir)
            .env("PATH", path)
            .env("HOME", self.path("home"))
            .env("TERM", "dumb")
            .env("LODI_TEST_UMASK", "022")
            .env_remove("XDG_STATE_HOME")
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("LODI_REPO")
            .env_remove("SUDO_UID")
            .env_remove("SUDO_GID")
            .env_remove("DOAS_USER")
            .env_remove("LODI_FS_LEDGER")
            .env("LODI_HOME", self.path("lodi-home"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .env("LODI_FETCH_REWRITE", rewrite)
            .envs(envs.iter().copied())
            .stdout(Stdio::piped());
        match input {
            Input::Terminal(typed) => {
                // The command moves in, so its copies of the terminal close when it is done.
                let (mut output, shown) = terminal::on_terminal(typed, move |stdin, stderr| {
                    let mut command = command;
                    command
                        .stdin(stdin)
                        .stderr(stderr)
                        .output()
                        .expect("lodi runs")
                });
                output.stderr = String::from_utf8_lossy(&shown)
                    .replace("\r\n", "\n")
                    .into_bytes();
                output
            }
            Input::Pipe(text) => {
                let mut child = command
                    .stdin(Stdio::piped())
                    .stderr(Stdio::piped())
                    .spawn()
                    .expect("lodi runs");
                let mut stdin = child.stdin.take().unwrap();
                let _ = stdin.write_all(text.as_bytes());
                drop(stdin);
                child.wait_with_output().expect("lodi ends")
            }
        }
    }

    /// Only the stub elevators, the `git` stub, the fake's shims and the tools: no elevator of
    /// the machine's own (`tests/support/elevators.rs`).
    fn path_var(&self) -> String {
        let shims = self.v.case.base().join("bin");
        self.el.path(&[&self.path("stub"), &shims])
    }

    /// `lodi ARGS` from the decoy folder with `input`, as the test's own user.
    fn run_input(&self, args: &[&str], input: Input) -> Output {
        let path = self.path_var();
        let sh = Command::new("/bin/sh");
        self.run_by(sh, &self.decoy(), args, &[], &path, input)
    }

    fn run_env(&self, args: &[&str], envs: &[(&str, &str)]) -> Output {
        self.run_with(&self.decoy(), args, envs, &self.path_var())
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_env(args, &[])
    }

    /// Everything below the root but the fake's state, for "nothing was written".
    fn tree(&self) -> std::collections::BTreeMap<PathBuf, (u32, u64, String, i128)> {
        census(&self.v.case.root.dir)
    }

    /// [`Imp::tree`] less the stub elevators' own record of their calls: what a run that reads
    /// the host as root but writes nothing leaves as it was.
    fn written(&self) -> std::collections::BTreeMap<PathBuf, (u32, u64, String, i128)> {
        let mut tree = self.tree();
        tree.remove(self.el.record());
        tree
    }
}

fn which(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|file| file.is_file())
    })
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o7777
}

fn lock(dir: &Path) -> serde_json::Value {
    serde_json::from_str(&fs::read_to_string(dir.join("lodi.lock")).expect("lodi.lock")).unwrap()
}

/// A refusal: its code, its exit status, the path it names, and nothing below the root changed.
#[track_caller]
fn refused_untouched(imp: &Imp, args: &[&str], envs: &[(&str, &str)], code: &str, names: &Path) {
    let (before, git) = (imp.tree(), imp.git_calls());
    let output = imp.run_env(args, envs);
    refused(&output, code);
    assert!(
        err(&output).contains(&names.display().to_string()),
        "{}",
        story(&output)
    );
    assert_eq!(imp.tree(), before, "{}", story(&output));
    assert_eq!(imp.git_calls(), git);
}

// ---------------------------------------------------------------------- where it writes ---

#[test]
fn import_with_no_argument_lands_a_flat_config_in_the_standard_folder() {
    let imp = Imp::new("imp-standard");
    let decoy = census(&imp.decoy());
    let output = imp.run(&["import"]);
    ok(&output);
    let dir = imp.standard();
    for file in ["host.toml", "home.toml", "lodi.lock"] {
        assert!(dir.join(file).is_file(), "{file}: {}", story(&output));
        assert_eq!(mode(&dir.join(file)), 0o644, "{file}");
    }
    let host = fs::read_to_string(dir.join("host.toml")).unwrap();
    assert!(host.contains("distro = \"debian\""), "{host}");
    assert!(host.contains("\"bc\""), "{host}");
    // 2.0's floor (LD-528): no 1.x lodi applies a config 2.0 imported (#701).
    assert!(host.contains("min_lodi_version = \">=2.0.0\"\n"), "{host}");
    let home = fs::read_to_string(dir.join("home.toml")).unwrap();
    assert!(home.contains("[home]"), "{home}");
    assert!(!dir.join("home").exists(), "no 1.x home/<login>/ folder");
    let lock = lock(&dir);
    assert_eq!(lock["format"], "lodi-config-lock");
    assert!(lock["hosts"]["host.toml"].is_object(), "{lock}");
    assert!(lock["homes"]["home.toml"].is_object(), "{lock}");
    assert_eq!(mode(&dir), 0o755);
    assert_eq!(mode(&imp.path("home/.config")), 0o700);
    assert_eq!(
        census(&imp.decoy()),
        decoy,
        "the current folder is never used"
    );
    assert_eq!(
        imp.remembered(),
        None,
        "the standard folder is not remembered"
    );
    let next = out(&output);
    assert!(next.contains("lodi switch --dry-run"), "{}", story(&output));
    assert!(
        next.contains(&dir.display().to_string()),
        "{}",
        story(&output)
    );
    assert!(
        err(&output).contains("[1/"),
        "plain step lines: {}",
        story(&output)
    );
}

/// Under `sudo` the config is the person's: their home from the root's passwd, not root's
/// `HOME`, every file theirs, and `git init` run as them.
#[test]
fn under_sudo_the_config_lands_in_the_person_s_home_as_theirs() {
    let imp = Imp::new("imp-sudo");
    mkdir(&imp.path("root-home"), 0o700);
    let output = imp.run_as_root(&["import"], Input::Terminal("y\n"));
    ok(&output);
    let dir = imp.standard();
    assert!(dir.join("host.toml").is_file(), "{}", story(&output));
    assert!(dir.join(".git").is_dir(), "{}", story(&output));
    for (path, _) in census(&imp.path("home")) {
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(
            (meta.uid(), meta.gid()),
            (imp.uid, imp.gid),
            "{}",
            path.display()
        );
    }
    assert!(
        census(&imp.path("root-home")).is_empty(),
        "root's HOME is never used"
    );
}

#[test]
fn a_typed_path_is_used_and_remembered_only_after_success() {
    let imp = Imp::new("imp-typed");
    let output = imp.run(&["import", "../mine"]);
    ok(&output);
    let mine = imp.path("mine");
    assert!(mine.join("host.toml").is_file(), "{}", story(&output));
    assert!(!imp.standard().exists());
    assert_eq!(imp.remembered(), Some(format!("{}\n", mine.display())));
}

#[test]
fn a_typed_url_is_refused_and_nothing_is_written() {
    let imp = Imp::new("imp-url");
    let before = imp.tree();
    let output = imp.run(&["import", "https://example.org/config.git"]);
    refused(&output, "E_UNSUPPORTED");
    assert!(err(&output).contains("https://example.org/config.git"));
    assert_eq!(imp.tree(), before);
}

#[test]
fn lodi_repo_is_honoured_and_not_remembered() {
    let imp = Imp::new("imp-env");
    let target = imp.path("home/dotfiles");
    let output = imp.run_env(&["import"], &[("LODI_REPO", target.to_str().unwrap())]);
    ok(&output);
    assert!(target.join("host.toml").is_file(), "{}", story(&output));
    assert!(!imp.standard().exists());
    assert_eq!(imp.remembered(), None);
}

#[test]
fn the_remembered_path_is_honoured_and_a_gone_one_stops() {
    let imp = Imp::new("imp-remembered");
    let target = imp.path("home/kept");
    mkdir(&target, 0o755);
    let state = imp.path("home/.local/state/lodi");
    mkdir(&state, 0o700);
    fs::write(
        state.join(lodi::config::REMEMBERED),
        format!("{}\n", target.display()),
    )
    .unwrap();
    ok(&imp.run(&["import"]));
    assert!(target.join("host.toml").is_file());

    let gone = imp.path("home/gone");
    fs::write(
        state.join(lodi::config::REMEMBERED),
        format!("{}\n", gone.display()),
    )
    .unwrap();
    refused_untouched(&imp, &["import"], &[], "E_NO_MANIFEST", &gone);
}

// --------------------------------------------------------------------------- the folder ---

#[test]
fn a_missing_folder_is_created_0755_under_umask_0002() {
    let imp = Imp::new("imp-umask");
    let output = imp.run_env(&["import"], &[("LODI_TEST_UMASK", "002")]);
    ok(&output);
    assert_eq!(mode(&imp.standard()), 0o755);
    assert_eq!(mode(&imp.standard().join("host.toml")), 0o644);
}

#[test]
fn a_private_group_0775_folder_is_accepted_and_others_are_refused() {
    let imp = Imp::new("imp-trust");
    let own = imp.path("home/own");
    mkdir(&own, 0o775);
    ok(&imp.run(&["import", own.to_str().unwrap()]));
    assert!(own.join("host.toml").is_file());
    assert!(
        !own.join(".git").exists(),
        "no git init on a folder that was there"
    );

    let shared = imp.path("home/shared");
    mkdir(&shared, 0o775);
    imp.v.case.root.write(
        "etc/group",
        &format!("root:x:0:\nsample:x:{}:friend\n", imp.gid),
    );
    let typed = shared.to_str().unwrap();
    refused_untouched(&imp, &["import", typed], &[], "E_PATH_ESCAPE", &shared);

    let open = imp.path("home/open");
    mkdir(&open, 0o757);
    let typed = open.to_str().unwrap();
    refused_untouched(&imp, &["import", typed], &[], "E_PATH_ESCAPE", &open);
}

#[test]
fn git_init_runs_only_on_a_created_folder_and_never_commits() {
    let imp = Imp::new("imp-git");
    ok(&imp.run(&["import"]));
    let calls = imp.git_calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("init"), "{calls:?}");
    for word in ["commit", "push", "add", "remote"] {
        assert!(!calls[0].contains(word), "{calls:?}");
    }
    let git = imp.standard().join(".git");
    assert!(git.is_dir());
    assert_eq!(fs::metadata(&git).unwrap().uid(), imp.uid);
    assert!(
        !git.join("refs/heads").read_dir().unwrap().any(|_| true),
        "no commit"
    );
}

/// A git setting of the person's that blocks — `~/.gitconfig` a FIFO no one writes — never hangs
/// the import (LD-519): `git init` is given a bounded wait, then the folder is left out of git
/// with one warning, and everything else is written.
#[test]
fn a_git_config_that_blocks_never_hangs_the_import() {
    let imp = Imp::new("imp-git-fifo");
    let fifo = imp.path("home/.gitconfig");
    let made = Command::new(which("mkfifo").expect("mkfifo"))
        .arg(&fifo)
        .status()
        .expect("mkfifo runs");
    assert!(made.success());
    let (tx, rx) = std::sync::mpsc::channel();
    let output = std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            let output = imp.run(&["import"]);
            let _ = tx.send(());
            output
        });
        if rx.recv_timeout(wait::ceiling()).is_err() {
            // Hung on the FIFO, which git opens again and again: a writer lets the open it waits
            // in return, and a plain file in its place answers every later one, so the run ends.
            let writer = fs::OpenOptions::new().write(true).open(&fifo);
            let plain = imp.path("home/.gitconfig.plain");
            fs::write(&plain, "").unwrap();
            fs::rename(&plain, &fifo).unwrap();
            drop(writer);
            let _ = run.join();
            panic!("import hung on a git config that blocks");
        }
        run.join().expect("the run")
    });
    ok(&output);
    assert!(imp.standard().join("host.toml").is_file());
    assert!(!imp.standard().join(".git").exists(), "{}", story(&output));
    let stderr = err(&output);
    let warned: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("git") && line.contains("warning"))
        .collect();
    assert_eq!(warned.len(), 1, "{}", story(&output));
}

#[test]
fn without_git_import_warns_once_and_goes_on() {
    let imp = Imp::new("imp-nogit");
    let output = imp.run_with(
        &imp.decoy(),
        &["import"],
        &[],
        &format!(
            "{}:{}",
            imp.path("elevators").display(),
            imp.v.case.base().join("bin").display()
        ),
    );
    ok(&output);
    assert!(imp.standard().join("host.toml").is_file());
    assert!(!imp.standard().join(".git").exists());
    let stderr = err(&output);
    let warned: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("git") && line.contains("warning"))
        .collect();
    assert_eq!(warned.len(), 1, "{}", story(&output));
}

// ----------------------------------------------------------------------------- refusals ---

#[test]
fn a_host_inside_another_config_is_refused_naming_its_root() {
    let imp = Imp::new("imp-nested");
    let config = imp.path("home/cfg");
    mkdir(&config, 0o755);
    fs::write(config.join("host.toml"), "[host]\n").unwrap();
    // The #680 shape: the home folder of a 1.x host directory, inside that host.
    let inner = config.join("home/sample");
    mkdir(&inner, 0o755);
    refused_untouched(
        &imp,
        &["import", inner.to_str().unwrap()],
        &[],
        "E_CONFIG",
        &config,
    );
}

#[test]
fn a_project_folder_and_a_non_empty_non_config_folder_are_refused() {
    let imp = Imp::new("imp-project");
    let project = imp.path("home/project");
    mkdir(&project, 0o755);
    fs::write(project.join("lodi.toml"), "[tools]\n").unwrap();
    let typed = project.to_str().unwrap();
    refused_untouched(&imp, &["import", typed], &[], "E_CONFIG", &project);

    let other = imp.path("home/notes");
    mkdir(&other, 0o755);
    fs::write(other.join("todo.txt"), "milk\n").unwrap();
    let typed = other.to_str().unwrap();
    refused_untouched(&imp, &["import", typed], &[], "E_CONFIG", &other);
}

#[test]
fn a_failing_write_removes_what_the_run_created() {
    let imp = Imp::new("imp-fault");
    let before = census(&imp.path("home"));
    let ledger = imp.path("no/such/ledger");
    let output = imp.run_env(&["import"], &[("LODI_FS_LEDGER", ledger.to_str().unwrap())]);
    refused(&output, "E_STORE_IO");
    let after: std::collections::BTreeMap<_, _> = census(&imp.path("home"))
        .into_iter()
        .filter(|(path, _)| !path.starts_with(imp.path("home/.local")))
        .collect();
    assert_eq!(after, before, "{}", story(&output));
    assert_eq!(imp.remembered(), None);
}

// ----------------------------------------------------------------------------- home only ---

#[test]
fn home_only_writes_only_home_toml() {
    let imp = Imp::new("imp-home");
    let output = imp.run(&["import", "--home"]);
    ok(&output);
    let names: Vec<String> = fs::read_dir(imp.standard())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !name.starts_with('.'))
        .collect();
    assert_eq!(names, ["home.toml"], "{}", story(&output));
    assert_eq!(imp.v.case.log(), Vec::<String>::new(), "no package manager");
    assert!(
        out(&output).contains("then run lodi switch --home --dry-run"),
        "the next step previews the home only: {}",
        story(&output)
    );
}

#[test]
fn an_existing_home_toml_is_kept_and_the_flat_config_gains_a_host() {
    let imp = Imp::new("imp-keep-home");
    let config = imp.path("home/cfg");
    mkdir(&config, 0o755);
    fs::write(
        config.join("home.toml"),
        "[home]\nversion = \"1\"\n# mine\n",
    )
    .unwrap();
    let typed = config.to_str().unwrap();
    ok(&imp.run(&["import", "--home", typed]));
    let home = fs::read_to_string(config.join("home.toml")).unwrap();
    assert_eq!(home, "[home]\nversion = \"1\"\n# mine\n");
    let output = imp.run(&["import", typed]);
    ok(&output);
    assert!(config.join("host.toml").is_file());
    assert_eq!(fs::read_to_string(config.join("home.toml")).unwrap(), home);
    assert!(lock(&config)["homes"]["home.toml"].is_object());
}

#[test]
fn home_only_in_a_config_without_this_host_stops() {
    let imp = Imp::new("imp-home-index");
    let config = imp.path("home/cfg");
    mkdir(&config, 0o755);
    fs::write(
        config.join("config.toml"),
        "[hosts.other]\nhost = \"other/host.toml\"\n",
    )
    .unwrap();
    let typed = config.to_str().unwrap();
    refused_untouched(&imp, &["import", "--home", typed], &[], "E_CONFIG", &config);
}

/// The entry the stop above asks for, added by hand and naming files not there yet: `--home`
/// writes the home it names, and the full import fills in the host file it names.
#[test]
fn a_config_toml_entry_added_by_hand_is_filled_in() {
    let imp = Imp::new("imp-hand-entry");
    let config = imp.path("home/cfg");
    mkdir(&config, 0o755);
    let index = "[hosts.other]\nhost = \"other/host.toml\"\n\n[hosts.box]\n\
                 host = \"box/host.toml\"\nhomes = { sample = \"mine.toml\" }\n";
    fs::write(config.join("config.toml"), index).unwrap();
    let typed = config.to_str().unwrap();
    let output = imp.run(&["import", "--home", typed]);
    ok(&output);
    assert!(config.join("mine.toml").is_file(), "{}", story(&output));
    assert!(!config.join("home.toml").exists());
    let output = imp.run(&["import", typed]);
    ok(&output);
    let host = read(&config.join("box/host.toml"));
    assert!(host.contains("\"bc\""), "{host}\n{}", story(&output));
    assert!(!config.join("host.toml").exists());
    assert_eq!(read(&config.join("config.toml")), index);
    assert!(lock(&config)["hosts"]["box/host.toml"].is_object());
}

// ------------------------------------------------------------------------------ the lock ---

#[test]
fn without_the_network_import_warns_and_succeeds_without_the_lock() {
    let imp = Imp::new("imp-offline");
    let offline = [
        ("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/"),
        ("LODI_FETCH_ATTEMPTS", "1"),
    ];
    let output = imp.run_env(&["import"], &offline);
    ok(&output);
    assert!(imp.standard().join("host.toml").is_file());
    assert!(!imp.standard().join("lodi.lock").exists());
    assert!(
        err(&output).contains(&imp.standard().join("lodi.lock").display().to_string()),
        "{}",
        story(&output)
    );
}

/// Offline, the archive cannot be reached: the import says so at once and writes no lock,
/// without trying again for minutes (LD-528). The run is root in a user and network namespace
/// of its own, the name-service cache hidden, so the archive's real name fails as an offline
/// machine's does (no lookup, or no network to reach).
#[test]
fn offline_the_lock_stops_at_once_and_the_import_goes_on() {
    let imp = Imp::new("imp-no-dns");
    let mut unshare = Command::new(which("unshare").expect("unshare"));
    unshare.args([
        "--user",
        "--map-root-user",
        "--mount",
        "--net",
        "/bin/sh",
        "-c",
    ]);
    unshare.arg("mount -t tmpfs lodi /var/run/nscd 2>/dev/null; exec /bin/sh \"$@\"");
    unshare.arg("sh");
    mkdir(&imp.path("root-home"), 0o700);
    let (uid, gid) = (imp.uid.to_string(), imp.gid.to_string());
    let root_home = imp.path("root-home").display().to_string();
    let envs = [
        ("SUDO_UID", uid.as_str()),
        ("SUDO_GID", gid.as_str()),
        ("HOME", root_home.as_str()),
        (
            "LODI_FETCH_REWRITE",
            "https://unused.invalid/=https://unused.invalid/",
        ),
    ];
    let started = std::time::Instant::now();
    let output = imp.run_by(
        unshare,
        &imp.decoy(),
        &["import", "--yes"],
        &envs,
        &imp.path_var(),
        Input::Pipe(""),
    );
    let took = started.elapsed();
    ok(&output);
    assert!(
        imp.standard().join("host.toml").is_file(),
        "{}",
        story(&output)
    );
    assert!(!imp.standard().join("lodi.lock").exists());
    assert!(err(&output).contains("lodi.lock"), "{}", story(&output));
    assert!(!err(&output).contains("retrying"), "{}", story(&output));
    assert!(took.as_secs() < 30, "took {took:?}: {}", story(&output));
}

// --------------------------------------------------------------- the one-time question ---

#[test]
fn the_first_import_asks_once_and_yes_writes_the_marker() {
    let imp = Imp::new("imp-ask");
    let output = imp.run(&["import"]);
    ok(&output);
    assert!(err(&output).contains(QUESTION), "{}", story(&output));
    assert!(imp.marked(), "{}", story(&output));
    assert_eq!(mode(&imp.path(MARKER)), 0o644);

    let again = imp.path("home/again");
    let output = imp.run(&["import", again.to_str().unwrap()]);
    ok(&output);
    assert!(!err(&output).contains(QUESTION), "{}", story(&output));
    assert!(again.join("host.toml").is_file());
}

#[test]
fn no_stops_with_nothing_written() {
    let imp = Imp::new("imp-no");
    let before = imp.tree();
    let output = imp.run_input(&["import"], Input::Terminal("n\n"));
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains(QUESTION), "{}", story(&output));
    assert_eq!(imp.tree(), before, "{}", story(&output));
}

#[test]
fn yes_answers_the_question_without_a_terminal() {
    let imp = Imp::new("imp-yes");
    mkdir(&imp.path("root-home"), 0o700);
    let output = imp.run_as_root(&["import", "--yes"], Input::Pipe(""));
    ok(&output);
    assert!(!err(&output).contains(QUESTION), "{}", story(&output));
    assert!(imp.marked());
    assert!(imp.standard().join("host.toml").is_file());
}

/// "yes" piped into lodi import as root (#680) names `--yes` instead of reading the pipe.
#[test]
fn without_a_terminal_and_without_yes_it_stops_naming_yes() {
    let imp = Imp::new("imp-piped");
    mkdir(&imp.path("root-home"), 0o700);
    let before = imp.tree();
    let output = imp.run_as_root(&["import"], Input::Pipe("y\ny\ny\n"));
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains("--yes"), "{}", story(&output));
    assert_eq!(imp.tree(), before, "{}", story(&output));
}

/// A host a 1.x lodi armed is asked as any other (#688).
#[test]
fn a_1x_marker_does_not_count() {
    let imp = Imp::new("imp-1x");
    imp.v.case.root.write("etc/lodi/host-allowed", "");
    let before = imp.tree();
    let output = imp.run_input(&["import"], Input::Terminal("n\n"));
    refused(&output, "E_DECLINED");
    assert!(err(&output).contains(QUESTION), "{}", story(&output));
    assert_eq!(imp.tree(), before, "{}", story(&output));
}

#[test]
fn home_never_asks_and_never_elevates() {
    let imp = Imp::new("imp-home-ask");
    let output = imp.run(&["import", "--home"]);
    ok(&output);
    assert!(!err(&output).contains(QUESTION), "{}", story(&output));
    let output = imp.run_input(&["import", "--home", "../plain"], Input::Pipe(""));
    ok(&output);
    assert!(imp.path("plain/home.toml").is_file(), "{}", story(&output));
    assert!(imp.el.calls().is_empty());
    assert!(!imp.marked());
}

// ----------------------------------------------------------------------------- elevation ---

/// Not root, on a terminal: the elevator runs once, for the capture, the marker and the host
/// record only; every file of the config is the person's, and the elevated steps are lines of
/// the one step list.
#[test]
fn not_root_runs_only_the_capture_through_the_elevator_once() {
    let imp = Imp::new("imp-elevate");
    let output = imp.run(&["import"]);
    ok(&output);
    let calls = imp.el.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("sudo -- "), "{calls:?}");
    assert!(
        calls[0].contains(" __elevated --request import"),
        "{calls:?}"
    );
    for (path, _) in census(&imp.standard()) {
        let meta = fs::symlink_metadata(&path).unwrap();
        assert_eq!(meta.uid(), imp.uid, "{}", path.display());
    }
    let shown = err(&output);
    for line in [
        "[1/7] ask: done",
        "[2/7] read the host: done",
        "[3/7] write the marker: done",
        "[4/7] record the host: done",
        "[5/7] fill the lock: done",
        "[7/7] git init: done",
    ] {
        assert!(shown.contains(line), "{line}: {}", story(&output));
    }
}

#[test]
fn the_elevator_is_the_first_of_sudo_doas_and_run0() {
    let imp = Imp::new("imp-which");
    let cases: [(&[&str], &str); 3] = [
        (&ELEVATORS, "sudo"),
        (&["doas"], "doas"),
        (&["run0"], "run0"),
    ];
    for (at, (present, used)) in cases.into_iter().enumerate() {
        imp.el.install(present, false, "");
        imp.el.forget();
        let dir = imp.path(&format!("home/c{at}"));
        let args = ["import", dir.to_str().unwrap()];
        let output = imp.run_with(&imp.decoy(), &args, &[], &imp.path_var());
        ok(&output);
        let calls = imp.el.calls();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert!(calls[0].starts_with(&format!("{used} ")), "{calls:?}");
    }
}

#[test]
fn no_elevator_at_all_stops_before_anything_is_written() {
    let imp = Imp::new("imp-none");
    imp.el.install(&[], false, "");
    let before = imp.tree();
    let output = imp.run_with(&imp.decoy(), &["import"], &[], &imp.path_var());
    refused(&output, "E_NEED_ROOT");
    for hint in ["--home", "sudo"] {
        assert!(err(&output).contains(hint), "{hint}: {}", story(&output));
    }
    assert_eq!(imp.tree(), before, "{}", story(&output));
}

/// A script that is not root never waits on a password prompt.
#[test]
fn without_a_terminal_a_user_s_host_import_stops_naming_home() {
    let imp = Imp::new("imp-script");
    let before = imp.tree();
    let output = imp.run_input(&["import", "--yes"], Input::Pipe(""));
    refused(&output, "E_NEED_ROOT");
    assert!(err(&output).contains("--home"), "{}", story(&output));
    assert_eq!(imp.tree(), before, "{}", story(&output));
}

#[test]
fn a_refused_password_writes_nothing_and_no_marker() {
    let imp = Imp::new("imp-refused");
    imp.el.install(&ELEVATORS, true, "");
    let before = census(&imp.path("home"));
    let output = imp.run(&["import"]);
    refused(&output, "E_NEED_ROOT");
    assert_eq!(imp.el.calls().len(), 1);
    assert!(!imp.marked(), "{}", story(&output));
    let after: std::collections::BTreeMap<_, _> = census(&imp.path("home"))
        .into_iter()
        .filter(|(path, _)| !path.starts_with(imp.path("home/.local")))
        .collect();
    assert_eq!(after, before, "{}", story(&output));
}

#[test]
fn already_root_or_under_sudo_runs_no_elevator() {
    let imp = Imp::new("imp-root");
    mkdir(&imp.path("root-home"), 0o700);
    ok(&imp.run_as_root(&["import"], Input::Terminal("y\n")));
    assert!(imp.marked());
    let mine = imp.path("mine");
    let args = ["import", "--yes", mine.to_str().unwrap()];
    let output = imp.run_as_root_with(&args, &[], Input::Pipe(""));
    ok(&output);
    assert!(mine.join("host.toml").is_file(), "{}", story(&output));
    assert_eq!(imp.el.calls(), Vec::<String>::new());
}

// ------------------------------------------------- a second host, and importing again (#708) ---

impl Imp {
    /// This machine is now called `name`: the hostname lodi reads from the root.
    fn rename(&self, name: &str) {
        self.v.case.root.write("etc/hostname", &format!("{name}\n"));
    }

    /// The first import, into the standard folder; the folder.
    fn first(&self) -> PathBuf {
        let output = self.run(&["import"]);
        ok(&output);
        self.standard()
    }
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn index(dir: &Path) -> toml_edit::DocumentMut {
    read(&dir.join("config.toml"))
        .parse()
        .expect("config.toml is TOML")
}

#[track_caller]
fn names_host(doc: &toml_edit::DocumentMut, name: &str, host: &str, home: &str) {
    let entry = &doc["hosts"][name];
    assert_eq!(entry["host"].as_str(), Some(host), "{doc}");
    assert_eq!(entry["homes"]["sample"].as_str(), Some(home), "{doc}");
}

#[test]
fn a_second_host_lands_beside_the_first_and_both_share_home_toml() {
    let imp = Imp::new("imp-second");
    let dir = imp.first();
    let (host, home) = (read(&dir.join("host.toml")), read(&dir.join("home.toml")));
    imp.rename("other");
    let output = imp.run(&["import"]);
    ok(&output);
    let second = read(&dir.join("other/host.toml"));
    assert!(second.contains("distro = \"debian\""), "{second}");
    assert_eq!(
        read(&dir.join("host.toml")),
        host,
        "the first host is untouched"
    );
    assert_eq!(read(&dir.join("home.toml")), home);
    let doc = index(&dir);
    assert_eq!(
        read(&dir.join("config.toml")),
        "[hosts.box]\nhost = \"host.toml\"\nhomes = { sample = \"home.toml\" }\n\n\
         [hosts.other]\nhost = \"other/host.toml\"\nhomes = { sample = \"home.toml\" }\n"
    );
    names_host(&doc, "box", "host.toml", "home.toml");
    names_host(&doc, "other", "other/host.toml", "home.toml");
    assert!(!dir.join("other/home.toml").exists());
}

/// Importing again with nothing new writes nothing, and its summary line says so rather than
/// ending on an empty list (LD-528).
#[test]
fn importing_again_with_nothing_new_says_nothing_changed() {
    let imp = Imp::new("imp-same");
    let dir = imp.first();
    let output = imp.run(&["import"]);
    ok(&output);
    let shown = err(&output);
    let summary = shown.lines().find(|line| line.contains("imported in"));
    let summary = summary.unwrap_or_else(|| panic!("{}", story(&output)));
    let said = format!("{}: nothing changed", dir.display());
    assert!(summary.ends_with(&said), "{}", story(&output));
}

#[test]
fn config_toml_is_extended_keeping_its_comments_and_order() {
    let imp = Imp::new("imp-extend");
    let dir = imp.first();
    let mine = "# my machines\n\n[hosts.box] # this laptop\nhomes = { sample = \"home.toml\" }\n\
                host = \"host.toml\"\n";
    fs::write(dir.join("config.toml"), mine).unwrap();
    imp.rename("other");
    let output = imp.run(&["import"]);
    ok(&output);
    let text = read(&dir.join("config.toml"));
    assert!(text.starts_with(mine), "{text}");
    names_host(&index(&dir), "other", "other/host.toml", "home.toml");
    assert!(dir.join("other/host.toml").is_file());
}

#[test]
fn a_host_config_toml_names_is_imported_again_into_its_own_file() {
    let imp = Imp::new("imp-named");
    let dir = imp.first();
    mkdir(&dir.join("machines"), 0o755);
    fs::rename(dir.join("host.toml"), dir.join("machines/box.toml")).unwrap();
    fs::write(
        dir.join("config.toml"),
        "[hosts.box]\nhost = \"machines/box.toml\"\nhomes = { sample = \"home.toml\" }\n",
    )
    .unwrap();
    let index = read(&dir.join("config.toml"));
    imp.v.case.install_by_hand(fakehost::Pkg::new("jq"));
    let output = imp.run(&["import"]);
    ok(&output);
    let host = read(&dir.join("machines/box.toml"));
    assert!(host.contains("\"jq\""), "{host}\n{}", story(&output));
    assert!(!dir.join("host.toml").exists());
    assert!(!dir.join("box").exists());
    assert_eq!(read(&dir.join("config.toml")), index);
}

/// The person's edits stay (a comment, a name they added, a name they keep off the host); what
/// was installed by hand is added; each disagreement is one `W_RECONCILE` line.
#[test]
fn importing_again_merges_the_host_keeps_edits_and_warns_once_per_conflict() {
    let imp = Imp::new("imp-merge");
    let dir = imp.first();
    let path = dir.join("host.toml");
    let edited = read(&path)
        .replacen(
            "[packages]\n",
            "[packages]\n# mine, kept\nabsent = [\"nano\"]\n",
            1,
        )
        .replacen("common = [", "common = [\"zzz-mine\", ", 1);
    fs::write(&path, &edited).unwrap();
    imp.v.case.install_by_hand(fakehost::Pkg::new("jq"));
    imp.v.case.install_by_hand(fakehost::Pkg::new("nano"));
    let output = imp.run(&["import"]);
    ok(&output);
    let host = read(&path);
    assert!(host.contains("\"jq\""), "{host}\n{}", story(&output));
    assert!(host.contains("# mine, kept"), "{host}");
    assert!(host.contains("\"zzz-mine\""), "{host}");
    assert!(
        !host.contains("\"nano\", ") && host.contains("absent = [\"nano\"]"),
        "{host}"
    );
    let warned: Vec<String> = err(&output)
        .lines()
        .filter(|line| line.starts_with("W_RECONCILE"))
        .map(str::to_string)
        .collect();
    assert_eq!(warned.len(), 2, "{}", story(&output));
    assert!(
        warned.iter().any(|line| line.contains("nano")),
        "{warned:?}"
    );
    assert!(
        warned.iter().any(|line| line.contains("zzz-mine")),
        "{warned:?}"
    );
    assert!(!dir.join("box").exists() && !dir.join("config.toml").exists());
}

#[test]
fn a_flat_host_toml_naming_no_hostname_is_refused() {
    let imp = Imp::new("imp-unnamed");
    let config = imp.path("home/cfg");
    mkdir(&config, 0o755);
    fs::write(config.join("host.toml"), "[host]\n").unwrap();
    let typed = config.to_str().unwrap();
    let host = config.join("host.toml");
    refused_untouched(&imp, &["import", typed], &[], "E_CONFIG", &host);
    let dry = ["import", "--dry-run", typed];
    refused_untouched(&imp, &dry, &[], "E_CONFIG", &host);
}

#[test]
fn a_hostname_that_is_no_safe_folder_name_is_refused() {
    let imp = Imp::new("imp-unsafe");
    let dir = imp.first();
    imp.rename("..");
    refused_untouched(&imp, &["import"], &[], "E_PATH_ESCAPE", &dir);
}

#[test]
fn a_folder_of_the_hostname_that_is_not_the_host_s_is_refused() {
    let imp = Imp::new("imp-foreign");
    let dir = imp.first();
    mkdir(&dir.join("other"), 0o755);
    fs::write(dir.join("other/notes.txt"), "mine\n").unwrap();
    imp.rename("other");
    let folder = dir.join("other");
    refused_untouched(&imp, &["import"], &[], "E_CONFIG", &folder);
    refused_untouched(&imp, &["import", "--dry-run"], &[], "E_CONFIG", &folder);
}

#[test]
fn home_toml_stays_byte_identical_after_a_second_host_and_a_re_import() {
    let imp = Imp::new("imp-home-kept");
    let dir = imp.first();
    let home = format!("{}# my own line\n", read(&dir.join("home.toml")));
    fs::write(dir.join("home.toml"), &home).unwrap();
    ok(&imp.run(&["import"]));
    assert_eq!(read(&dir.join("home.toml")), home, "after a re-import");
    imp.rename("other");
    ok(&imp.run(&["import"]));
    assert_eq!(read(&dir.join("home.toml")), home, "after a second host");
    ok(&imp.run(&["import"]));
    assert_eq!(
        read(&dir.join("home.toml")),
        home,
        "after re-importing the second host"
    );
}

#[test]
fn the_lock_gains_the_new_host_and_moves_no_existing_pin() {
    let imp = Imp::new("imp-lock");
    let dir = imp.first();
    let first = lock(&dir);
    assert!(first["hosts"]["host.toml"].is_object(), "{first}");
    imp.v.case.install_by_hand(fakehost::Pkg::new("jq"));
    ok(&imp.run(&["import"]));
    assert_eq!(
        lock(&dir)["hosts"]["host.toml"],
        first["hosts"]["host.toml"]
    );
    imp.rename("other");
    let output = imp.run(&["import"]);
    ok(&output);
    let after = lock(&dir);
    assert_eq!(after["hosts"]["host.toml"], first["hosts"]["host.toml"]);
    assert_eq!(after["homes"]["home.toml"], first["homes"]["home.toml"]);
    assert!(
        after["hosts"]["other/host.toml"].is_object(),
        "{after}\n{}",
        story(&output)
    );
}

// ---------------------------------------------------------------------------- --dry-run ---

#[test]
fn dry_run_previews_a_re_import_and_writes_nothing() {
    let imp = Imp::new("imp-dry-again");
    let dir = imp.first();
    imp.v.case.install_by_hand(fakehost::Pkg::new("jq"));
    let before = imp.written();
    let output = imp.run(&["import", "--dry-run", dir.to_str().unwrap()]);
    ok(&output);
    assert_eq!(imp.written(), before, "{}", story(&output));
    assert_eq!(imp.remembered(), None);
    let shown = err(&output);
    let host = dir.join("host.toml").display().to_string();
    assert!(
        shown
            .lines()
            .any(|line| line.contains(&host) && line.contains("change")),
        "{}",
        story(&output)
    );
    assert!(shown.contains("+ package jq"), "{}", story(&output));
    assert!(
        !out(&output).contains("jq"),
        "the preview is on standard error"
    );
}

#[test]
fn dry_run_previews_a_second_host_and_config_toml_and_writes_nothing() {
    let imp = Imp::new("imp-dry-second");
    let dir = imp.first();
    imp.rename("other");
    let before = imp.written();
    let output = imp.run(&["import", "--dry-run"]);
    ok(&output);
    assert_eq!(imp.written(), before, "{}", story(&output));
    let shown = err(&output);
    for path in ["other/host.toml", "config.toml"] {
        let path = dir.join(path).display().to_string();
        assert!(shown.contains(&path), "{path}: {}", story(&output));
    }
}

#[test]
fn dry_run_on_a_first_import_asks_nothing_and_writes_nothing() {
    let imp = Imp::new("imp-dry-first");
    let before = imp.written();
    let output = imp.run_input(&["import", "--dry-run"], Input::Terminal(""));
    ok(&output);
    assert!(!err(&output).contains(QUESTION), "{}", story(&output));
    assert_eq!(imp.written(), before, "{}", story(&output));
    assert!(!imp.marked());
    assert!(imp.git_calls().is_empty());
    let output = imp.run_input(&["import", "--home", "--dry-run"], Input::Pipe(""));
    ok(&output);
    assert_eq!(imp.written(), before, "{}", story(&output));
}

/// A preview never asks for a password (#702 story 79): `import --dry-run` reads as the person,
/// with no elevator and no terminal, and names what only root could read.
#[test]
fn dry_run_reads_as_the_person_and_names_what_needs_root() {
    let imp = Imp::new("imp-dry-person");
    let dir = imp.first();
    let root = &imp.v.case.root;
    root.write("etc/secret.conf", "changed by hand\n");
    imp.v
        .case
        .install_by_hand(fakehost::Pkg::new("jq").conffile("/etc/secret.conf"));
    imp.el.forget();
    let before = imp.written();
    fs::set_permissions(
        root.path("etc/secret.conf"),
        fs::Permissions::from_mode(0o000),
    )
    .unwrap();
    let output = imp.run_input(&["import", "--dry-run"], Input::Pipe(""));
    fs::set_permissions(
        root.path("etc/secret.conf"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    ok(&output);
    assert!(imp.el.calls().is_empty(), "{}", story(&output));
    assert_eq!(imp.written(), before, "{}", story(&output));
    let shown = err(&output);
    assert!(shown.contains("+ package jq"), "{}", story(&output));
    assert!(
        shown.contains("/etc/secret.conf: needs root to read"),
        "{}",
        story(&output)
    );
    assert!(dir.join("host.toml").exists());
}

/// The files a host's manifest refers to (here a vendor repository's keyring) travel beside it,
/// each host's in its own folder.
#[test]
fn the_second_host_s_files_go_in_its_own_files_folder() {
    let imp = Imp::new("imp-files");
    let keyring = "-----BEGIN PGP PUBLIC KEY BLOCK-----\n\n\
                   bm90IGEga2V5OiBnZW5lcmF0ZWQgZm9yIGEgdGVzdA==\n=AAAA\n\
                   -----END PGP PUBLIC KEY BLOCK-----\n";
    let root = &imp.v.case.root;
    root.write(
        "etc/apt/sources.list.d/vendor.sources",
        "Types: deb\nURIs: https://vendor.example/debian\nSuites: bookworm\n\
         Components: main\nSigned-By: /etc/apt/keyrings/vendor.asc\n",
    );
    root.write("etc/apt/keyrings/vendor.asc", keyring);
    imp.v.case.edit(|m| {
        m["sources"]["vendor"] = serde_json::json!({
            "key": "https://vendor.example/debian bookworm/main amd64 Packages",
            "release": "o=Vendor,a=bookworm,n=bookworm,l=Vendor,c=main,b=amd64",
        });
    });
    imp.v
        .case
        .install_by_hand(fakehost::Pkg::new("vend").repo(Some("vendor")));
    let dir = imp.first();
    let first = census(&dir.join("files"));
    assert!(!first.is_empty(), "{}", read(&dir.join("host.toml")));
    imp.rename("other");
    let output = imp.run(&["import"]);
    ok(&output);
    assert_eq!(
        census(&dir.join("files")),
        first,
        "the first host's files are untouched"
    );
    for path in first.keys() {
        let rel = path.strip_prefix(&dir).unwrap();
        let theirs = dir.join("other").join(rel);
        assert!(theirs.exists(), "{}: {}", theirs.display(), story(&output));
    }
}

// ------------------------------------------------------------- the first switch after it ---

/// `host.toml` with the package `name`'s entry taken out of its lists.
fn without_package(text: &str, name: &str) -> String {
    let quoted = format!("\"{name}\"");
    let edited = text
        .lines()
        .filter(|line| line.trim().trim_end_matches(',') != quoted)
        .map(|line| {
            line.replace(&format!("{quoted}, "), "")
                .replace(&format!(", {quoted}"), "")
                .replace(&format!("[{quoted}]"), "[]")
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(!edited.contains(&quoted), "{name} is still in:\n{edited}");
    edited + "\n"
}

/// The owner's loop (LD-515): import, take a package out of `host.toml`, switch; the switch
/// removes it, with no drift to settle, as a switch after a switch would.
#[track_caller]
fn a_removal_is_applied_by_the_first_switch(imp: &Imp, dir: &Path, host: &str) {
    let path = dir.join(host);
    fs::write(&path, without_package(&read(&path), "bc")).unwrap();
    let output = imp.run(&["switch"]);
    ok(&output);
    assert!(!imp.v.case.installed().contains("bc"), "{}", story(&output));
    assert!(imp.v.case.installed().contains("tzdata"));
}

#[test]
fn a_package_taken_out_after_import_is_removed_by_the_first_switch() {
    let imp = Imp::new("imp-then-switch");
    let dir = imp.first();
    a_removal_is_applied_by_the_first_switch(&imp, &dir, "host.toml");
}

#[test]
fn the_first_switch_after_an_import_has_nothing_to_switch() {
    let imp = Imp::new("imp-then-nothing");
    imp.first();
    imp.el.forget();
    let output = imp.run(&["switch"]);
    ok(&output);
    assert!(
        err(&output).contains("nothing to switch"),
        "the import recorded the host as a switch would: {}",
        story(&output)
    );
    assert_eq!(imp.el.calls(), Vec::<String>::new(), "no elevator");
}

#[test]
fn a_package_taken_out_after_importing_again_is_removed_by_the_next_switch() {
    let imp = Imp::new("imp-again-switch");
    let dir = imp.first();
    imp.v.case.install_by_hand(fakehost::Pkg::new("jq"));
    ok(&imp.run(&["import"]));
    assert!(read(&dir.join("host.toml")).contains("\"jq\""));
    a_removal_is_applied_by_the_first_switch(&imp, &dir, "host.toml");
    assert!(imp.v.case.installed().contains("jq"), "jq is declared");
}

#[test]
fn a_package_taken_out_of_a_second_host_is_removed_by_its_first_switch() {
    let imp = Imp::new("imp-second-switch");
    let dir = imp.first();
    imp.rename("other");
    ok(&imp.run(&["import"]));
    a_removal_is_applied_by_the_first_switch(&imp, &dir, "other/host.toml");
}

/// The owner's first rule across an import elsewhere: a package a switch installed from a
/// vendor's repository leaves with its line, though another folder was imported in between,
/// whose `host.toml` cannot declare it (the record stays the switched folder's).
#[test]
fn an_import_into_another_folder_keeps_the_record_of_the_switched_one() {
    let imp = Imp::new("imp-elsewhere");
    let root = &imp.v.case.root;
    root.write(
        "etc/apt/sources.list.d/vendor.sources",
        "Types: deb\nURIs: https://vendor.example/debian\nSuites: bookworm\n\
         Components: main\n",
    );
    imp.v.case.edit(|m| {
        m["sources"]["vendor"] = serde_json::json!({
            "key": "https://vendor.example/debian bookworm/main amd64 Packages",
            "release": "o=Vendor,a=bookworm,n=bookworm,l=Vendor,c=main,b=amd64",
        });
        m["available"]["vend"] = serde_json::json!({
            "version": "1.0-1", "depends": [], "recommends": [], "conflicts": [],
            "provides": [], "priority": "optional", "section": "misc", "repo": "vendor",
        });
    });
    let dir = imp.first();
    let path = dir.join("host.toml");
    // Unpinned, so that the switch installs from the machine's repositories, the vendor's too.
    let text = fakehost::without_snapshot(&read(&path));
    fs::write(&path, text.replacen("\"bc\"", "\"bc\", \"vend\"", 1)).unwrap();
    ok(&imp.run(&["switch"]));
    assert!(imp.v.case.installed().contains("vend"));
    let elsewhere = imp.path("home/elsewhere");
    ok(&imp.run(&["import", elsewhere.to_str().unwrap()]));
    fs::write(&path, text).unwrap();
    let output = imp.run(&["switch"]);
    ok(&output);
    assert!(
        !imp.v.case.installed().contains("vend"),
        "{}",
        story(&output)
    );
}

/// The tables the imported file shows as comments, to write on purpose, are each one a switch
/// reads: uncommented as they stand, the file still parses.
#[test]
fn the_commented_examples_in_an_imported_host_toml_parse() {
    let imp = Imp::new("imp-examples");
    let dir = imp.first();
    let path = dir.join("host.toml");
    let text = read(&path);
    let start = text
        .find("# Tables lodi does not read off a machine")
        .expect("the examples");
    let end = start + text[start..].find("\n\n").expect("their end");
    let examples: String = text[start..end]
        .lines()
        .skip(1)
        .map(|line| format!("{}\n", line.strip_prefix("# ").expect(line)))
        .collect();
    let uncommented = format!("{}{examples}{}", &text[..start], &text[end..]);
    fs::write(&path, &uncommented).unwrap();
    // A file that does not parse stops with status 3; this fake machine runs no network stack,
    // so the preview goes on past the parse and stops at [network] instead.
    let output = imp.run(&["switch", "--dry-run"]);
    assert_ne!(code(&output), Some(3), "{uncommented}\n{}", story(&output));
    assert!(
        err(&output).contains("E_NETWORK_STACK"),
        "{}",
        story(&output)
    );
}

#[test]
fn an_edit_after_a_home_import_is_applied_by_the_first_switch() {
    let imp = Imp::new("imp-home-switch");
    ok(&imp.run(&["import", "--home"]));
    let path = imp.standard().join("home.toml");
    let text = read(&path) + "\n[home.file.\".inputrc\"]\ntext = \"set bell-style none\\n\"\n";
    fs::write(&path, text).unwrap();
    let output = imp.run(&["switch", "--home"]);
    ok(&output);
    assert_eq!(
        fs::read_to_string(imp.path("home/.inputrc"))
            .ok()
            .as_deref(),
        Some("set bell-style none\n"),
        "{}",
        story(&output)
    );
}

// ------------------------------------------------------------------- 1.x leftovers (#710) ---

/// A machine a 1.x lodi managed: its repository `~/lodi/<hostname>/` with `home/<login>/`, the
/// arm marker, the host in place, `pins.lock`, `home.lock` and a 1.x repository lock, a trial
/// boot waiting, and 1.x records where 2.0 keeps its own. 2.0 reads none of it as its own.
fn with_1x_leftovers(imp: &Imp) {
    let write = |rel: &str, text: &str| {
        let path = imp.path(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    };
    write("etc/hostname", "box\n");
    write(
        "home/lodi/box/host.toml",
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n",
    );
    write(
        "home/lodi/box/pins.lock",
        "{\"format\": \"lodi-host-pins/1\", \"version\": 1}\n",
    );
    // 1.x's `home/<login>/` below the host folder, for the passwd's one person.
    let login = ["home/lodi/box/home", "sample"].join("/");
    write(&format!("{login}/home.toml"), "[home]\nversion = \"1\"\n");
    write(
        &format!("{login}/home.lock"),
        "{\"format\": \"lodi-spike-lock/1\"}\n",
    );
    write(
        "home/lodi/lodi.lock",
        "{\"format\": \"lodi-repository-lock/1\", \"version\": 1, \"hosts\": {}, \"homes\": {}}\n",
    );
    write("etc/lodi/host-allowed", "");
    write(
        "etc/lodi/host.toml",
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n",
    );
    write("var/lib/lodi/host/boot-trial", "lodi.trial=1\n");
    write(
        "etc/lodi/host.lock",
        "{\"version\": 5, \"format\": \"lodi-host-lock/5\", \"generatedBy\": \"lodi 1.12.2\"}\n",
    );
    // Where the fake host's `LODI_HOME` keeps the home part's records.
    write(
        "lodi-home/home-scope/state.json",
        "{\"version\": 4, \"lodiVersion\": \"1.12.2\", \"files\": {}, \"directories\": []}\n",
    );
}

/// Everything below the root but the stubs' own records of their calls (`git`, the elevators).
fn untouched(imp: &Imp) -> std::collections::BTreeMap<PathBuf, (u32, u64, String, i128)> {
    let mut tree = imp.written();
    tree.remove(&imp.path("git-calls"));
    tree
}

/// #710: 2.0 finds no 1.x repository and counts no 1.x arming: import still asks, and a no
/// leaves the machine as it was. A 1.x record where 2.0 keeps its own stops a switch with its
/// code and its path, and nothing is repaired, moved or deleted.
#[test]
fn the_1x_leftovers_are_never_read_as_2_0_s_own_and_never_touched() {
    let imp = Imp::new("imp-leftovers");
    with_1x_leftovers(&imp);
    let before = untouched(&imp);

    let output = imp.run_input(&["import"], Input::Terminal("n\n"));
    assert!(err(&output).contains(QUESTION), "{}", story(&output));
    assert!(!output.status.success(), "{}", story(&output));
    assert!(!imp.marked());
    assert!(!err(&output).contains("lodi/box"), "{}", story(&output));
    assert_eq!(untouched(&imp), before, "{}", story(&output));

    // Discovery never takes `~/lodi/<hostname>/`: with no 2.0 config there is nothing to switch.
    let output = imp.run(&["switch", "--dry-run"]);
    assert!(!output.status.success(), "{}", story(&output));
    assert!(!story(&output).contains("lodi/box"), "{}", story(&output));
    assert_eq!(untouched(&imp), before, "{}", story(&output));

    // A 2.0 config, armed: the 1.x host lock stops the switch, named, and stays as it was.
    let config = imp.standard();
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("host.toml"),
        "[host]\nversion = \"1\"\ndistro = \"debian\"\n",
    )
    .unwrap();
    imp.v.case.root.write(MARKER, "");
    let before = untouched(&imp);
    let output = imp.run(&["switch", "--dry-run"]);
    refused(&output, "E_LOCK_VERSION");
    assert!(
        story(&output).contains(&imp.path("etc/lodi/host.lock").display().to_string())
            || story(&output).contains("/etc/lodi/host.lock"),
        "{}",
        story(&output)
    );
    assert_eq!(untouched(&imp), before, "{}", story(&output));

    // Without it, the 1.x home state stops the home part, named, and stays as it was.
    fs::remove_file(imp.path("etc/lodi/host.lock")).unwrap();
    fs::write(config.join("home.toml"), "[home]\nversion = \"1\"\n").unwrap();
    let before = untouched(&imp);
    let output = imp.run(&["switch", "--home", "--dry-run"]);
    refused(&output, "E_STORE_IO");
    assert!(
        story(&output).contains("home-scope/state.json"),
        "{}",
        story(&output)
    );
    assert_eq!(untouched(&imp), before, "{}", story(&output));
}
