//! M-Spike S-3: host-tool `lodi develop -- <command>` and `lodi run <task>`, with their trust
//! prompt and `--trust` (LD-496), run as the real binary.
//!
//! Offline: the tools are synthetic archives (`tests/support`) served by a loopback server
//! that the binary reaches through `LODI_FETCH_REWRITE`, with its own `LODI_HOME`,
//! `XDG_CONFIG_HOME` and `HOME` under the target directory. Every child environment is built
//! from a cleared environment, so an outer Lodi activation of the developer cannot leak in.

/// Long-lived children that end with their test, pass or panic (LD-372).
#[path = "support/fixture.rs"]
mod fixture;
mod support;
#[path = "support/terminal.rs"]
mod terminal;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use fixture::{Fixture, hold};
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");

/// One isolated user: a store, a config directory, a home and an artifact server.
struct User {
    base: PathBuf,
    server: Server,
}

impl User {
    fn new(name: &str, tools: &[Tool]) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        User {
            base,
            server: Server::for_tools(tools),
        }
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    /// In lodi's data folder, never in the config folder `lodi import` writes (#702 story 33).
    fn trust_file(&self) -> PathBuf {
        self.lodi_home().join("trust.json")
    }

    fn command(&self, dir: &Path, args: &[&str]) -> Command {
        let mut c = Command::new(LODI);
        c.args(args)
            .current_dir(dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.lodi_home())
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            .env("LODI_BIN", LODI)
            .stdin(Stdio::null());
        c
    }

    fn run(&self, dir: &Path, args: &[&str]) -> Output {
        self.command(dir, args).output().unwrap()
    }

    fn run_with(&self, dir: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = self.command(dir, args);
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    fn store_entries(&self) -> Vec<String> {
        let Ok(entries) = fs::read_dir(self.lodi_home().join("store")) else {
            return Vec::new();
        };
        let mut names: Vec<String> = entries
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("art-") || n.starts_with("env-"))
            .collect();
        names.sort();
        names
    }

    fn sessions(&self) -> Vec<String> {
        session_listing(&self.lodi_home().join("gcroots/sessions"))
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn env_map(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

fn canonical(p: &Path) -> String {
    p.canonicalize().unwrap().to_string_lossy().into_owned()
}

/// Wait for something a child process does. The ceiling is the suite's shared one
/// (`tests/support/wait.rs`), because these children compete with every other lane on the
/// machine; what is waited *for* is unchanged, and a child that never gets there still fails.
fn wait_for(what: &str, cond: impl Fn() -> bool) {
    wait::until(what, cond);
}

fn tools() -> Vec<Tool> {
    vec![Tool::python("3.12.14"), Tool::node("22", "22.23.2")]
}

#[test]
fn develop_preserves_argv_status_cwd_and_environment() {
    let before: Vec<_> = std::env::vars_os().collect();
    let tools = tools();
    let user = User::new("host-develop", &tools);
    let project = user.base.join("project");
    write_project(
        &project,
        &manifest(
            &tools,
            "[env]\nGREETING = \"hello $FROM_PARENT and $UNSET_VAR.\"\n\
             LITERAL = \"$(touch pwned) `touch pwned2` $${HOME}\"\n",
        ),
        &tools,
    );
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    record_trust(&user.trust_file(), &project);

    // Arguments arrive unchanged: spaces, empty strings, quotes, $, a leading --, newlines.
    let args = ["a b", "", "$HOME", "it's", "--", "x\ny", "*"];
    let mut argv = vec![
        "develop",
        "--",
        "/bin/sh",
        "-c",
        "printf '<%s>\\n' \"$@\"",
        "argv0",
    ];
    argv.extend(args);
    let o = user.run(&project, &argv);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let expected: String = args.iter().map(|a| format!("<{a}>\n")).collect();
    assert_eq!(out(&o), expected, "stdout carries only the child's output");
    assert!(err(&o).contains("lodi: realized"), "{}", err(&o));

    // Exit statuses pass through; a signal becomes 128 + n; a missing program is 127.
    for (script, code) in [
        ("exit 0", 0),
        ("exit 7", 7),
        ("exit 255", 255),
        ("kill -TERM $$", 128 + 15),
        ("kill -KILL $$", 128 + 9),
    ] {
        let o = user.run(&project, &["develop", "--", "/bin/sh", "-c", script]);
        assert_eq!(o.status.code(), Some(code), "{script}: {}", err(&o));
    }
    let o = user.run(
        &project,
        &["develop", "--", "no-such-command-in-lodi-tests"],
    );
    assert_eq!(o.status.code(), Some(127));
    assert!(err(&o).contains("command not found"));

    // The locked tools are on PATH first; the working directory is the project root.
    for (cmd, expect) in [
        ("python3", "Python 3.12.14 (synthetic)\n"),
        ("pip3", "pip synthetic\n"),
        ("node", "v22.23.2\n"),
        ("npm", "npm synthetic\n"),
        ("pwd", &format!("{}\n", canonical(&project))),
    ] {
        let o = user.run(&project, &["develop", "--", cmd]);
        assert_eq!(out(&o), expect, "{cmd}: {}", err(&o));
    }

    // The child's environment: the parent's, plus the activation; the parent keeps its own.
    let o = user.run_with(
        &project,
        &["develop", "--", "env"],
        &[("FROM_PARENT", "parent")],
    );
    let env = env_map(&out(&o));
    let store = canonical(&user.lodi_home()) + "/store/env-";
    assert!(env["LODI_ENV"].starts_with(&store));
    assert!(env["PATH"].starts_with(&format!("{}/tree/bin:", env["LODI_ENV"])));
    assert!(env["PATH"].ends_with(&std::env::var("PATH").unwrap()));
    assert_eq!(env["LODI_ORIG_PATH"], std::env::var("PATH").unwrap());
    assert_eq!(env["LODI_PROJECT_ROOT"], canonical(&project));
    assert_eq!(env["LODI_PROFILE"], "default");
    assert_eq!(env["LODI_MODE"], "host");
    assert_eq!(env["LODI_ENV_NAME"], "demo");
    assert!(env["LODI_ENV_HASH"].starts_with("sha256:"));
    assert!(env["LODI_ACTIVATION"].starts_with("sha256:"));
    assert_eq!(env["FROM_PARENT"], "parent");
    assert_eq!(env["GREETING"], "hello parent and .");
    assert_eq!(env["LITERAL"], "$(touch pwned) `touch pwned2` ${HOME}");
    assert!(!project.join("pwned").exists() && !project.join("pwned2").exists());
    let after: Vec<_> = std::env::vars_os().collect();
    assert_eq!(
        before, after,
        "the calling process's environment is unchanged"
    );

    // Exit cleanup: no session root, no staging, no partial download remains.
    assert!(user.sessions().is_empty());
    assert!(listing(&user.lodi_home().join("store/tmp")).is_empty());
    assert!(
        listing(&user.lodi_home().join("cache/dl"))
            .iter()
            .all(|f| !f.ends_with(".part"))
    );
}

#[test]
fn run_executes_trusted_tasks_with_arguments_and_status() {
    let tools = tools();
    let user = User::new("host-run", &tools);
    let project = user.base.join("project");
    write_project(
        &project,
        &manifest(
            &tools,
            "[tasks.echo]\nrun = 'printf \"[%s]\" \"$@\"; echo'\n\n\
             [tasks.fail]\nrun = \"exit 3\"\n\n\
             [tasks.where]\nrun = \"pwd && python3 && node\"\n",
        ),
        &tools,
    );
    record_trust(&user.trust_file(), &project);
    let o = user.run(&project, &["run", "echo", "a", "b c", "", "--flag"]);
    assert_eq!(out(&o), "[a][b c][][--flag]\n", "{}", err(&o));
    assert_eq!(user.run(&project, &["run", "fail"]).status.code(), Some(3));
    let o = user.run(&project, &["run", "where"]);
    assert_eq!(
        out(&o),
        format!(
            "{}\nPython 3.12.14 (synthetic)\nv22.23.2\n",
            canonical(&project)
        )
    );
    let o = user.run(&project, &["run", "nope"]);
    assert_eq!(o.status.code(), Some(2));
    assert!(err(&o).contains("no task `nope`") && err(&o).contains("echo, fail, where"));
}

/// `lodi ARGS` in `dir` on a pseudo-terminal that already holds `answer`: the output, and what
/// the terminal was shown.
fn on_terminal(user: &User, dir: &Path, args: &[&str], answer: &str) -> (Output, String) {
    let (o, shown) = terminal::on_terminal(answer, |input, error| {
        user.command(dir, args)
            .stdin(input)
            .stderr(error)
            .output()
            .unwrap()
    });
    (o, String::from_utf8_lossy(&shown).into_owned())
}

/// Without a terminal an untrusted project stops before anything is locked, fetched, realized
/// or run, for run and for develop alike, and the hint names `--trust` and `LODI_TRUST=1`.
#[test]
fn untrusted_without_a_terminal_stops_before_any_lock_or_request() {
    let tools = tools();
    let user = User::new("host-trust-required", &tools);
    let project = user.base.join("project");
    write_project(
        &project,
        &manifest(&tools, "[tasks.mark]\nrun = \"touch marker\"\n"),
        &tools,
    );
    fs::remove_file(project.join("lodi.lock")).unwrap();
    for args in [&["run", "mark"][..], &["develop", "--", "touch", "marker"]] {
        let o = user.run(&project, args);
        assert_eq!(o.status.code(), Some(11), "{args:?}: {}", err(&o));
        assert!(err(&o).contains("E_TRUST_REQUIRED"), "{}", err(&o));
        assert!(err(&o).contains("--trust"), "{}", err(&o));
        assert!(!err(&o).contains("lodi trust"), "{}", err(&o));
    }
    assert!(!project.join("lodi.lock").exists(), "a lock before trust");
    assert!(user.server.requests().is_empty(), "a request before trust");
    assert!(user.store_entries().is_empty());
    assert!(!project.join("marker").exists());
}

/// `--trust` and `LODI_TRUST=1` each authorize one run, say so with the hash and the boundary,
/// and record nothing: the next run without either is refused again.
#[test]
fn trust_for_one_run_records_nothing() {
    let tools = tools();
    let user = User::new("host-trust-once", &tools);
    let project = user.base.join("project");
    write_project(
        &project,
        &manifest(&tools, "[tasks.mark]\nrun = \"touch marker\"\n"),
        &tools,
    );
    let flag = user.run(&project, &["run", "--trust", "mark"]);
    let variable = user.run_with(&project, &["run", "mark"], &[("LODI_TRUST", "1")]);
    let develop = user.run(&project, &["develop", "--trust", "--", "true"]);
    for o in [&flag, &variable, &develop] {
        assert_eq!(o.status.code(), Some(0), "{}", err(o));
        let notice = err(o);
        assert!(
            notice.contains("this run only (script text sha256:")
                && notice.contains("outside this authorization"),
            "{notice}"
        );
    }
    assert!(project.join("marker").exists());
    assert!(!user.trust_file().exists(), "a one-run trust was recorded");
    let refused = user.run(&project, &["run", "mark"]);
    assert_eq!(refused.status.code(), Some(11), "{}", err(&refused));
}

/// On a terminal the first run shows the task text, its hash and the boundary and asks; a yes
/// records trust, and the next run asks nothing. A change to the task text warns and asks again;
/// a change outside it (a tool dropped) does not.
#[test]
fn the_prompt_on_a_terminal_records_a_yes_and_asks_again_after_a_change() {
    let tools = tools();
    let user = User::new("host-trust-prompt", &tools);
    let project = user.base.join("project");
    let text =
        |tools: &[Tool], run: &str| manifest(tools, &format!("[tasks.mark]\nrun = \"{run}\"\n"));
    write_project(&project, &text(&tools, "touch marker"), &tools);

    let (o, shown) = on_terminal(&user, &project, &["run", "mark"], "y\n");
    assert_eq!(o.status.code(), Some(0), "{shown}");
    assert!(
        shown.contains("task mark:") && shown.contains("| touch marker"),
        "{shown}"
    );
    assert!(shown.contains("script text hash: sha256:"), "{shown}");
    assert!(shown.contains("outside this authorization"), "{shown}");
    assert!(shown.contains("[y/N]"), "{shown}");
    assert!(project.join("marker").exists());
    let recorded = fs::read_to_string(user.trust_file()).unwrap();
    assert!(recorded.contains(&canonical(&project.join("lodi.toml"))));
    assert!(
        !user.base.join("config/lodi").exists(),
        "trust left a file in the config folder"
    );

    // Trusted and unchanged: no prompt, nothing shown about trust.
    let again = user.run(&project, &["run", "mark"]);
    assert_eq!(again.status.code(), Some(0), "{}", err(&again));
    assert!(!err(&again).contains("trust"), "{}", err(&again));

    // A tool dropped from the manifest is outside the task text: still trusted.
    let python = [Tool::python("3.12.14")];
    fs::write(project.join("lodi.toml"), text(&python, "touch marker")).unwrap();
    let dropped = user.run(&project, &["run", "mark"]);
    assert_eq!(dropped.status.code(), Some(0), "{}", err(&dropped));

    // A changed task text warns and asks again; a yes records the new text.
    fs::write(project.join("lodi.toml"), text(&python, "touch changed")).unwrap();
    let (o, shown) = on_terminal(&user, &project, &["run", "mark"], "y\n");
    assert_eq!(o.status.code(), Some(0), "{shown}");
    assert!(shown.contains("W_TRUST_CHANGED"), "{shown}");
    assert!(shown.contains("| touch changed"), "{shown}");
    assert!(project.join("changed").exists());
    let again = user.run(&project, &["run", "mark"]);
    assert_eq!(again.status.code(), Some(0), "{}", err(&again));
}

/// A no, and an empty answer, decline: `E_DECLINED`, and nothing locked, fetched, realized or
/// run, and nothing recorded.
#[test]
fn a_no_or_an_empty_answer_declines_with_nothing_locked_or_run() {
    let tools = tools();
    let user = User::new("host-trust-declined", &tools);
    let project = user.base.join("project");
    write_project(
        &project,
        &manifest(&tools, "[tasks.mark]\nrun = \"touch marker\"\n"),
        &tools,
    );
    fs::remove_file(project.join("lodi.lock")).unwrap();
    for answer in ["n\n", "\n"] {
        let (o, shown) = on_terminal(&user, &project, &["run", "mark"], answer);
        assert_eq!(o.status.code(), Some(11), "{answer:?}: {shown}");
        assert!(shown.contains("E_DECLINED"), "{shown}");
    }
    assert!(!project.join("lodi.lock").exists());
    assert!(user.server.requests().is_empty());
    assert!(user.store_entries().is_empty());
    assert!(!project.join("marker").exists());
    assert!(!user.trust_file().exists());
}

/// A manifest with tools and no tasks is trusted whole (LD-359): the prompt shows every line of
/// it. One with no tasks, tools or base is never asked about.
#[test]
fn a_manifest_without_tasks_is_trusted_whole_and_an_empty_one_is_not_asked_about() {
    let tools = tools();
    let user = User::new("host-trust-whole", &tools);
    let plain = user.base.join("plain");
    write_project(&plain, &manifest(&tools, ""), &tools);
    let o = user.run(&plain, &["develop", "--", "touch", "marker"]);
    assert_eq!(o.status.code(), Some(11), "{}", err(&o));
    assert!(err(&o).contains("E_TRUST_REQUIRED"), "{}", err(&o));
    let (o, shown) = on_terminal(
        &user,
        &plain,
        &["develop", "--", "touch", "marker"],
        "yes\n",
    );
    assert_eq!(o.status.code(), Some(0), "{shown}");
    assert!(
        shown.contains("| [tools]") && shown.contains("manifest hash: sha256:"),
        "{shown}"
    );
    assert!(plain.join("marker").exists());

    let empty = user.base.join("empty");
    write_project(&empty, &manifest(&[], ""), &[]);
    let o = user.run(&empty, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!err(&o).contains("trust"), "{}", err(&o));
}

#[test]
fn unsupported_script_features_are_refused_first() {
    let tools = tools();
    let user = User::new("host-refusals", &tools);
    for (what, extra) in [
        ("hooks", "[[hooks.enter]]\nscript = \"touch hooked\"\n"),
        ("modules", "[modules]\nx = { path = \"./m\" }\n"),
        ("services", "[services.db]\nrun = \"touch served\"\n"),
    ] {
        let project = user.base.join(what);
        write_project(&project, &manifest(&tools, extra), &tools);
        for args in [&["develop", "--", "true"][..], &["run", "x"]] {
            let o = user.run(&project, args);
            assert_eq!(o.status.code(), Some(3), "{what} {args:?}: {}", err(&o));
            assert!(err(&o).contains("E_UNSUPPORTED"), "{what}: {}", err(&o));
        }
        assert!(!project.join("hooked").exists() && !project.join("served").exists());
    }

    assert!(user.server.requests().is_empty());
    assert!(user.store_entries().is_empty());
}

fn spawn(user: &User, dir: &Path, args: &[&str]) -> Fixture {
    Fixture::spawn(
        "lodi",
        user.command(dir, args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped()),
    )
}

#[test]
fn live_roots_name_the_entries_while_a_child_runs_and_are_removed_on_exit() {
    let tools = tools();
    let user = User::new("host-roots", &tools);
    let project = user.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    record_trust(&user.trust_file(), &project);

    // A root of a process that no longer exists is pruned on the next entry.
    let mut dead = Command::new("true").spawn().unwrap();
    let dead_pid = dead.id();
    dead.wait().unwrap();
    fs::create_dir_all(user.lodi_home().join("gcroots/sessions")).unwrap();
    fs::write(
        user.lodi_home()
            .join(format!("gcroots/sessions/{dead_pid}")),
        "{}",
    )
    .unwrap();

    let script = "cat \"$LODI_HOME\"/gcroots/sessions/* > root.json; \
                  ls \"$LODI_HOME\"/gcroots/sessions > roots.txt; exit 5";
    let child = spawn(&user, &project, &["develop", "--", "/bin/sh", "-c", script]);
    let pid = child.id();
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert_eq!(
        fs::read_to_string(project.join("roots.txt")).unwrap(),
        format!("{pid}\n"),
        "exactly the live session's root, the dead one pruned"
    );
    let root: serde_json::Value =
        serde_json::from_slice(&fs::read(project.join("root.json")).unwrap()).unwrap();
    assert_eq!(root["pid"], pid);
    assert_eq!(root["activation"]["projectRoot"], canonical(&project));
    assert_eq!(root["activation"]["runtime"], "host");
    let named: Vec<&str> = root["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(named.len(), 3);
    assert!(named[0].starts_with("env-"));
    for entry in user.store_entries() {
        assert!(named.contains(&entry.as_str()), "{entry} is rooted");
    }
    assert!(
        user.sessions().is_empty(),
        "the root is removed when the child exits"
    );

    // SIGINT/SIGQUIT to Lodi alone do not stop it or the child; SIGTERM is forwarded.
    let wait_loop = format!("echo ready > ready; {}; echo done", hold("[ ! -f stop ]"));
    let wait_loop = wait_loop.as_str();
    let mut child = spawn(
        &user,
        &project,
        &["develop", "--", "/bin/sh", "-c", wait_loop],
    );
    wait_for("the child", || project.join("ready").exists());
    assert_eq!(user.sessions(), [child.id().to_string()]);
    for signal in [libc::SIGINT, libc::SIGQUIT] {
        // SAFETY: signalling the lodi process this test started.
        unsafe { libc::kill(child.id() as i32, signal) };
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(
        child.child().try_wait().unwrap().is_none(),
        "lodi survived SIGINT/SIGQUIT"
    );
    fs::write(project.join("stop"), "").unwrap();
    let o = child.wait_with_output().unwrap();
    assert_eq!((o.status.code(), out(&o)), (Some(0), "done\n".into()));
    fs::remove_file(project.join("stop")).unwrap();
    fs::remove_file(project.join("ready")).unwrap();

    let child = spawn(
        &user,
        &project,
        &["develop", "--", "/bin/sh", "-c", wait_loop],
    );
    wait_for("the child", || project.join("ready").exists());
    // SAFETY: as above.
    unsafe { libc::kill(child.id() as i32, libc::SIGTERM) };
    let o = child.wait_with_output().unwrap();
    assert_eq!(o.status.code(), Some(128 + 15), "{}", err(&o));
    assert!(o.status.signal().is_none(), "lodi itself exited normally");
    assert!(user.sessions().is_empty());
}

#[test]
fn two_project_roots_share_entries_but_have_distinct_activations_and_trust() {
    let tools = tools();
    let node24 = Tool::node("24", "24.21.0");
    let mut all = tools.clone();
    all.push(node24.clone());
    let user = User::new("host-two-roots", &all);
    let text = manifest(&tools, "[tasks.t]\nrun = \"true\"\n");
    let a = user.base.join("a");
    let b = user.base.join("b");
    write_project(&a, &text, &tools);
    write_project(&b, &text, &tools);
    let c = user.base.join("c");
    let c_tools = [Tool::python("3.12.14"), node24.clone()];
    write_project(&c, &manifest(&c_tools, ""), &c_tools);

    record_trust(&user.trust_file(), &a);
    assert_eq!(
        user.run(&b, &["run", "t"]).status.code(),
        Some(11),
        "trust is per root"
    );
    record_trust(&user.trust_file(), &b);
    // `c` has tools and no tasks, and is trusted before it is entered like the others (LD-359).
    record_trust(&user.trust_file(), &c);

    // All three live at once; each child reports its identity and its node.
    let script = format!(
        "env > env.txt; node > node.txt; echo up > up; {}",
        hold("[ ! -f \"$STOP\" ]")
    );
    let stop = user.base.join("stop");
    let children: Vec<(PathBuf, Fixture)> = [&a, &b, &c]
        .into_iter()
        .map(|dir| {
            let child = Fixture::spawn(
                "a develop session",
                user.command(dir, &["develop", "--", "/bin/sh", "-c", &script])
                    .env("STOP", &stop)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped()),
            );
            (dir.clone(), child)
        })
        .collect();
    for (dir, _) in &children {
        wait_for("the three children", || dir.join("up").exists());
    }
    assert_eq!(user.sessions().len(), 3, "three live session roots at once");
    fs::write(&stop, "").unwrap();
    for (_, child) in children {
        assert_eq!(child.wait_with_output().unwrap().status.code(), Some(0));
    }
    let env = |d: &Path| env_map(&fs::read_to_string(d.join("env.txt")).unwrap());
    let (ea, eb, ec) = (env(&a), env(&b), env(&c));
    assert_eq!(
        ea["LODI_ENV_HASH"], eb["LODI_ENV_HASH"],
        "same plan, same envhash"
    );
    assert_eq!(ea["LODI_ENV"], eb["LODI_ENV"], "one shared env entry");
    assert_ne!(ea["LODI_PROJECT_ROOT"], eb["LODI_PROJECT_ROOT"]);
    assert_ne!(ea["LODI_ACTIVATION"], eb["LODI_ACTIVATION"]);
    assert_ne!(ea["LODI_ENV_HASH"], ec["LODI_ENV_HASH"]);
    let node = |d: &Path| fs::read_to_string(d.join("node.txt")).unwrap();
    assert_eq!(
        (node(&a), node(&c)),
        ("v22.23.2\n".into(), "v24.21.0\n".into())
    );
    assert!(user.sessions().is_empty());
    // python is shared by a and c: two env entries, three art entries.
    let entries = user.store_entries();
    assert_eq!(entries.iter().filter(|e| e.starts_with("env-")).count(), 2);
    assert_eq!(entries.iter().filter(|e| e.starts_with("art-")).count(), 3);
}

#[test]
fn nesting_is_idempotent_for_one_activation_and_stacks_another() {
    let tools = tools();
    let user = User::new("host-nesting", &tools);
    let a = user.base.join("a");
    let b = user.base.join("b");
    write_project(&a, &manifest(&tools, ""), &tools);
    write_project(&b, &manifest(&tools, ""), &tools);
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    for dir in [&a, &b] {
        record_trust(&user.trust_file(), dir);
    }
    let strip = |text: String| -> BTreeMap<String, String> {
        let mut m = env_map(&text);
        for k in ["_", "SHLVL", "PWD", "OLDPWD"] {
            m.remove(k);
        }
        m
    };

    // Same identity: the inner entry changes nothing and warns nothing.
    let script = "env > outer.txt; \"$LODI_BIN\" develop -- env > inner.txt 2> inner.err";
    let o = user.run(&a, &["develop", "--", "/bin/sh", "-c", script]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let outer = strip(fs::read_to_string(a.join("outer.txt")).unwrap());
    let inner = strip(fs::read_to_string(a.join("inner.txt")).unwrap());
    assert_eq!(outer, inner, "re-entry is idempotent: nothing is stacked");
    assert_eq!(fs::read_to_string(a.join("inner.err")).unwrap(), "");

    // Another root (same plan): stacked in the child with W_NESTED; the outer is unchanged.
    let script = "env > outer.txt; cd \"$B\" && \"$LODI_BIN\" develop -- env > \"$A/inner.txt\" \
                  2> \"$A/inner.err\"; cd \"$A\"; env > after.txt";
    let o = user
        .command(&a, &["develop", "--", "/bin/sh", "-c", script])
        .env("A", canonical(&a))
        .env("B", canonical(&b))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let outer = strip(fs::read_to_string(a.join("outer.txt")).unwrap());
    let inner = strip(fs::read_to_string(a.join("inner.txt")).unwrap());
    let after = strip(fs::read_to_string(a.join("after.txt")).unwrap());
    assert!(
        fs::read_to_string(a.join("inner.err"))
            .unwrap()
            .contains("W_NESTED")
    );
    assert_eq!(inner["LODI_PROJECT_ROOT"], canonical(&b));
    assert_ne!(inner["LODI_ACTIVATION"], outer["LODI_ACTIVATION"]);
    assert_eq!(
        inner["PATH"],
        format!("{}/tree/bin:{}", inner["LODI_ENV"], outer["PATH"])
    );
    assert_eq!(
        after, outer,
        "leaving the inner environment returns to the outer one"
    );

    // --no-nest refuses to stack; a different runtime is refused.
    let script = "cd \"$B\" && exec \"$LODI_BIN\" develop --no-nest -- true";
    let o = user
        .command(&a, &["develop", "--", "/bin/sh", "-c", script])
        .env("B", canonical(&b))
        .output()
        .unwrap();
    assert_eq!(o.status.code(), Some(7));
    assert!(err(&o).contains("E_NESTED"));
    let o = user.run_with(
        &a,
        &["develop", "--", "true"],
        &[("LODI_MODE", "container"), ("LODI_ACTIVATION", "sha256:x")],
    );
    assert_eq!(o.status.code(), Some(7));
    assert!(err(&o).contains("cross-runtime"));
}

#[test]
fn concurrent_entries_realize_once_and_a_warm_entry_downloads_nothing() {
    let tools = tools();
    let user = User::new("host-concurrent", &tools);
    *user.server.delay.lock().unwrap() = Duration::from_millis(300);
    let dirs: Vec<PathBuf> = (0..4).map(|i| user.base.join(format!("p{i}"))).collect();
    for d in &dirs {
        write_project(d, &manifest(&tools, ""), &tools);
        // A manifest with tools and no tasks is trusted before it is entered (LD-359).
        record_trust(&user.trust_file(), d);
    }
    let children: Vec<Fixture> = dirs
        .iter()
        .map(|d| spawn(&user, d, &["develop", "--", "node"]))
        .collect();
    for child in children {
        let o = child.wait_with_output().unwrap();
        assert_eq!(
            (o.status.code(), out(&o)),
            (Some(0), "v22.23.2\n".into()),
            "{}",
            err(&o)
        );
    }
    for t in &tools {
        assert_eq!(user.server.count(&t.url), 1, "{} downloaded once", t.label);
    }
    assert_eq!(user.store_entries().len(), 3);

    user.server.clear();
    let o = user.run(&dirs[0], &["develop", "--", "python3"]);
    assert_eq!(o.status.code(), Some(0));
    assert!(
        user.server.requests().is_empty(),
        "warm entry: zero requests"
    );
    assert!(!err(&o).contains("realized"), "{}", err(&o));
}

#[test]
fn an_interrupted_or_tampered_download_publishes_nothing_and_recovery_works() {
    let tools = tools();
    let user = User::new("host-interrupt", &tools);
    let project = user.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    record_trust(&user.trust_file(), &project);
    let python = &tools[0];

    user.server.stall.lock().unwrap().push(python.url.clone());
    let mut child = spawn(&user, &project, &["develop", "--", "true"]);
    wait_for("the stalled download", || {
        user.server
            .stalled
            .load(std::sync::atomic::Ordering::SeqCst)
    });
    child.child().kill().unwrap();
    assert_eq!(child.wait().unwrap().signal(), Some(libc::SIGKILL));
    let dl = listing(&user.lodi_home().join("cache/dl"));
    assert!(
        !dl.contains(&python.sha256_hex()),
        "no unverified download in the cache"
    );
    assert!(
        user.store_entries().iter().all(|e| !e.contains("python")),
        "no python entry"
    );
    assert!(user.sessions().is_empty());

    user.server.stall.lock().unwrap().clear();
    let o = user.run(&project, &["develop", "--", "python3"]);
    assert_eq!(out(&o), "Python 3.12.14 (synthetic)\n", "{}", err(&o));
    assert!(listing(&user.lodi_home().join("store/tmp")).is_empty());
    assert!(
        listing(&user.lodi_home().join("cache/dl"))
            .iter()
            .all(|f| !f.ends_with(".part")),
        "the partial download of the killed process is pruned"
    );

    // Tampered bytes on the server: exit 5, nothing published for that tool.
    let other = User::new("host-tamper", &tools);
    let project = other.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    record_trust(&other.trust_file(), &project);
    let mut bytes = python.bytes.clone();
    bytes[20] ^= 1;
    other
        .server
        .files
        .lock()
        .unwrap()
        .insert(python.url.clone(), bytes);
    let o = other.run(&project, &["develop", "--", "true"]);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(err(&o).contains("E_HASH_MISMATCH"));
    assert!(other.store_entries().iter().all(|e| !e.contains("python")));
    assert!(other.sessions().is_empty());
}

/// LD-372: a test that fails between starting its `lodi develop` fixture and releasing it leaves
/// no process behind — neither `lodi` nor the shell it runs. The panic is caught here only so
/// that this test can look afterwards; any other test's panic unwinds the same way.
#[test]
fn a_test_that_panics_between_spawn_and_stop_leaves_no_process() {
    let tools = tools();
    let user = User::new("host-fixture-panic", &tools);
    let project = user.base.join("project");
    write_project(&project, &manifest(&tools, ""), &tools);
    record_trust(&user.trust_file(), &project);

    let stop = user.base.join("stop-never-written");
    let script = format!("echo $$ > shell.pid; {}", hold("[ ! -f \"$STOP\" ]"));
    let pids = std::sync::Mutex::new(Vec::<i32>::new());
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let fixture = Fixture::spawn(
            "the develop fixture",
            user.command(&project, &["develop", "--", "/bin/sh", "-c", &script])
                .env("STOP", &stop)
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        pids.lock().unwrap().push(fixture.id() as i32);
        wait_for("the fixture's shell", || {
            fs::read_to_string(project.join("shell.pid")).is_ok_and(|t| t.ends_with('\n'))
        });
        let shell: i32 = fs::read_to_string(project.join("shell.pid"))
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        pids.lock().unwrap().push(shell);
        assert!(!fixture::gone(shell), "the shell runs while the test does");
        panic!("a failure between spawn and STOP");
    }));
    assert!(outcome.is_err(), "the fixture's test failed as staged");
    let pids = pids.into_inner().unwrap();
    assert_eq!(pids.len(), 2, "both pids were seen before the failure");
    for pid in pids {
        fixture::wait_gone(&format!("pid {pid} of the failed test's fixture"), pid);
    }
    assert!(!stop.exists(), "the fixture was never released");
}

/// LD-372: the loop a fixture waits in ends on its own after its deadline, even if nothing ever
/// releases it, and says so with its own exit status.
#[test]
fn a_fixture_loop_that_is_never_released_ends_at_its_deadline() {
    let never = scratch("fixture-deadline").join("never-written");
    let started = std::time::Instant::now();
    let status = Command::new("/bin/sh")
        .arg("-c")
        .arg(fixture::hold_within("[ ! -f \"$1\" ]", 1))
        .arg("hold")
        .arg(&never)
        .stdin(Stdio::null())
        .status()
        .unwrap();
    assert_eq!(status.code(), Some(fixture::HOLD_EXPIRED));
    assert!(
        started.elapsed() < wait::ceiling(),
        "the loop ended by its own deadline"
    );
    assert!(!never.exists());
}
