//! rf-1 (LD-409): recipes of your own in the `recipes/` folder at the root of your lodi
//! repository, run as the real binary against a loopback server.
//!
//! The repository is found as `lodi apply` finds it: the `SOURCE` a verb names, else the current
//! directory once confirmed (never, here: no child has a terminal), else `LODI_REPO`. With none,
//! only the built-in recipes resolve. Every fetch is rewritten to a loopback server that serves a
//! synthetic Kubernetes release bucket, so nothing leaves the machine.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::util::{sha256_hex, sha256_tagged};
use support::{RemoveOnDrop, Server};

const K8S: &str = "https://dl.k8s.io/release";
const VERSION: &str = "9.8.7";

/// The recipe a person writes for a tool lodi does not carry.
const KUBECTL_CONVERT: &str = "\
# kubectl-convert from the Kubernetes release bucket: the stable text index names the version.
[recipe]
name        = \"kubectl-convert\"
description = \"Convert Kubernetes manifests between API versions\"
homepage    = \"https://kubernetes.io/docs/tasks/tools/\"

[versions]
strategy = \"text_index\"
url      = \"https://dl.k8s.io/release/stable.txt\"
line     = \"v${version}\"

[asset]
url              = \"https://dl.k8s.io/release/v${version}/bin/linux/${arch_alt}/kubectl-convert\"
format           = \"binary\"
checksum_sidecar = \".sha256\"

[spec]
bin  = [\"bin/kubectl-convert\"]
path = [\"bin\"]
";

/// A second tool of the person's own, from another synthetic upstream.
fn tool_recipe(name: &str, host: &str) -> String {
    format!(
        "[recipe]\nname = \"{name}\"\ndescription = \"{name}, a tool of my own\"\n\
         homepage = \"https://{host}/\"\n\n\
         [versions]\nstrategy = \"text_index\"\nurl = \"https://{host}/latest.txt\"\n\
         line = \"v${{version}}\"\n\n\
         [asset]\nurl = \"https://{host}/v${{version}}/{name}-${{arch_alt}}\"\n\
         format = \"binary\"\nchecksum_sidecar = \".sha256\"\n\n\
         [spec]\nbin = [\"bin/{name}\"]\npath = [\"bin\"]\n"
    )
}

fn said(name: &str) -> String {
    format!("{name} {VERSION} from the recipe of my own\n")
}

fn script(name: &str) -> Vec<u8> {
    format!("#!/bin/sh\necho {}", said(name)).into_bytes()
}

/// The files the loopback server answers: the bucket's stable index, the binary and its
/// `.sha256` sidecar, and the same for each synthetic upstream in `others`.
fn upstream(others: &[(&str, &str)]) -> BTreeMap<String, Vec<u8>> {
    let mut files = BTreeMap::new();
    let binary = format!("{K8S}/v{VERSION}/bin/linux/amd64/kubectl-convert");
    let body = script("kubectl-convert");
    files.insert(
        format!("{K8S}/stable.txt"),
        format!("v{VERSION}\n").into_bytes(),
    );
    files.insert(format!("{binary}.sha256"), sha256_hex(&body).into_bytes());
    files.insert(binary, body);
    for (name, host) in others {
        let binary = format!("https://{host}/v{VERSION}/{name}-amd64");
        let body = script(name);
        files.insert(
            format!("https://{host}/latest.txt"),
            format!("v{VERSION}\n").into_bytes(),
        );
        files.insert(format!("{binary}.sha256"), sha256_hex(&body).into_bytes());
        files.insert(binary, body);
    }
    files
}

fn login() -> String {
    String::from_utf8(Command::new("id").arg("-un").output().unwrap().stdout)
        .unwrap()
        .trim()
        .to_string()
}

/// A scratch tree: a person's home, and a repository `hosts/` with one host `box` whose home for
/// the invoking user declares `tools`, and a `recipes/` folder at its root.
struct Tree {
    dir: PathBuf,
    home: PathBuf,
    hosts: PathBuf,
    _cleanup: RemoveOnDrop,
}

impl Tree {
    fn new(name: &str, tools: &str) -> Tree {
        let dir = support::scratch(&format!("user-recipes-{name}"));
        let home = dir.join("person");
        let hosts = dir.join("hosts");
        let selected = hosts.join("box/home").join(login());
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(&selected).unwrap();
        fs::create_dir_all(hosts.join("recipes")).unwrap();
        fs::write(hosts.join("box/host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
        fs::write(
            selected.join("home.toml"),
            format!("[home]\nversion = \"1\"\n\n[tools]\n{tools}"),
        )
        .unwrap();
        Tree {
            _cleanup: RemoveOnDrop(dir.clone()),
            dir,
            home,
            hosts,
        }
    }

    fn recipe(&self, file: &str, text: &str) {
        fs::write(self.hosts.join("recipes").join(file), text).unwrap();
    }

    /// The binary in `cwd` with nothing of the developer's environment but a `PATH`.
    fn lodi(&self, cwd: &Path, server: &Server, args: &[&str], env: &[(&str, &Path)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_lodi"));
        command
            .args(args)
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", &self.home)
            .env("LODI_FETCH_REWRITE", server.rewrite())
            .current_dir(cwd);
        for (key, value) in env {
            command.env(key, value);
        }
        command.output().unwrap()
    }

    fn home_apply(&self, server: &Server) -> Output {
        let hosts = self.hosts.to_str().unwrap();
        self.lodi(
            &self.dir,
            server,
            &["home", "apply", hosts, "--host", "box"],
            &[],
        )
    }

    /// The home's tool section of the repository's root lock.
    fn locked(&self, tool: &str) -> serde_json::Value {
        let root: serde_json::Value =
            serde_json::from_slice(&fs::read(self.hosts.join("lodi.lock")).unwrap()).unwrap();
        root["homes"][format!("box/home/{}", login())]["tools"][tool].clone()
    }

    /// The executable the home's profile puts on `PATH` for `tool`.
    fn profile_bin(&self, tool: &str) -> PathBuf {
        self.home
            .join(".local/share/lodi/home-scope/profile/bin")
            .join(tool)
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn lines_with(text: &str, code: &str) -> usize {
    text.lines().filter(|line| line.contains(code)).count()
}

#[test]
fn home_apply_installs_a_tool_from_the_repository_recipes_folder() {
    let tree = Tree::new("apply", "kubectl-convert = \"latest\"\n");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let server = Server::start(upstream(&[]));
    let output = tree.home_apply(&server);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(lines_with(&stderr(&output), "W_RECIPE_"), 0, "{output:?}");

    let run = Command::new(tree.profile_bin("kubectl-convert"))
        .output()
        .unwrap();
    assert!(run.status.success(), "{run:?}");
    assert_eq!(stdout(&run), said("kubectl-convert"));

    let entry = tree.locked("kubectl-convert");
    assert_eq!(entry["version"], VERSION, "{entry}");
    assert_eq!(entry["recipe"]["input"], "user", "{entry}");
    assert_eq!(entry["recipe"]["path"], "kubectl-convert.toml", "{entry}");
    assert_eq!(
        entry["recipe"]["sha256"],
        sha256_tagged(KUBECTL_CONVERT.as_bytes()),
        "{entry}"
    );
    // No GitHub API request: the bucket's index, the sidecar and the binary, nothing else.
    for request in server.requests() {
        assert!(
            request.starts_with("https://dl.k8s.io/release/"),
            "{request}"
        );
    }

    // Applied again, the lock is fresh and nothing is fetched.
    server.clear();
    let again = tree.home_apply(&server);
    assert_eq!(again.status.code(), Some(0), "{again:?}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
fn info_and_search_name_the_repository_recipe() {
    let tree = Tree::new("browse", "");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let server = Server::start(BTreeMap::new());
    let repo: &[(&str, &Path)] = &[("LODI_REPO", &tree.hosts)];
    let shown = tree.hosts.join("recipes/kubectl-convert.toml");

    let info = tree.lodi(&tree.hosts, &server, &["info", "kubectl-convert"], repo);
    assert_eq!(info.status.code(), Some(0), "{info:?}");
    let recipe_line = stdout(&info)
        .lines()
        .find(|line| line.trim_start().starts_with("recipe:"))
        .map(str::to_string)
        .unwrap_or_default();
    assert!(
        recipe_line.contains(shown.to_str().unwrap()),
        "{recipe_line:?} in {info:?}"
    );

    let search = tree.lodi(&tree.hosts, &server, &["search", "kubectl-convert"], repo);
    assert_eq!(search.status.code(), Some(0), "{search:?}");
    let text = stdout(&search);
    let lines: Vec<&str> = text.lines().collect();
    let row = lines
        .iter()
        .position(|line| line.trim_start().starts_with("kubectl-convert "))
        .unwrap_or_else(|| panic!("no kubectl-convert row: {text}"));
    let heading = lines[..row]
        .iter()
        .rev()
        .find(|line| !line.starts_with(' '))
        .unwrap();
    assert_ne!(*heading, "catalogue", "{text}");
    assert!(
        heading.contains(tree.hosts.join("recipes").to_str().unwrap()),
        "{text}"
    );
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

#[test]
fn a_repository_recipe_replaces_a_built_in_and_warns_once() {
    let tree = Tree::new("shadow", "jq = \"latest\"\nkubectl-convert = \"latest\"\n");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let jq = tool_recipe("jq", "jq.test");
    tree.recipe("jq.toml", &jq);
    let server = Server::start(upstream(&[("jq", "jq.test")]));
    let output = tree.home_apply(&server);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let err = stderr(&output);
    assert_eq!(lines_with(&err, "W_RECIPE_SHADOWED"), 1, "{err}");
    let warning = err
        .lines()
        .find(|line| line.contains("W_RECIPE_SHADOWED"))
        .unwrap();
    assert!(warning.contains("jq"), "{warning}");
    assert!(warning.contains("built-in"), "{warning}");
    assert!(!warning.contains("kubectl-convert"), "{warning}");

    let entry = tree.locked("jq");
    assert_eq!(entry["recipe"]["input"], "user", "{entry}");
    assert_eq!(entry["recipe"]["path"], "jq.toml", "{entry}");
    assert_eq!(entry["recipe"]["sha256"], sha256_tagged(jq.as_bytes()));
    let run = Command::new(tree.profile_bin("jq")).output().unwrap();
    assert_eq!(stdout(&run), said("jq"), "{run:?}");
    for request in server.requests() {
        assert!(!request.contains("github"), "{request}");
    }

    // A repository whose recipes shadow nothing prints no recipe warning at all.
    let quiet = Tree::new("quiet", "kubectl-convert = \"latest\"\n");
    quiet.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let output = quiet.home_apply(&server);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(lines_with(&stderr(&output), "W_RECIPE_"), 0, "{output:?}");
}

#[test]
fn no_repository_means_built_in_recipes_only() {
    let tree = Tree::new("none", "");
    let server = Server::start(upstream(&[]));
    // A recipe in every place a person might guess: none of them is a repository's root.
    let config = tree.home.join(".config/lodi");
    let project = tree.dir.join("project");
    for dir in [
        tree.home.join("recipes"),
        config.join("recipes"),
        tree.home.join(".config/recipes"),
        tree.home.join(".local/share/lodi/recipes"),
        project.join("recipes"),
    ] {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("kubectl-convert.toml"), KUBECTL_CONVERT).unwrap();
    }
    fs::write(
        config.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[tools]\nkubectl-convert = \"latest\"\n",
    )
    .unwrap();
    fs::write(
        project.join("lodi.toml"),
        "[project]\nversion = \"1\"\n\n[tools]\nkubectl-convert = \"latest\"\n",
    )
    .unwrap();

    let home = tree.lodi(&tree.dir, &server, &["home", "apply"], &[]);
    let lock = tree.lodi(&project, &server, &["lock"], &[]);
    for output in [&home, &lock] {
        assert_eq!(output.status.code(), Some(4), "{output:?}");
        let err = stderr(output);
        assert!(err.contains("E_NO_RECIPE"), "{err}");
        // The refusal says where a recipe of your own would go.
        assert!(err.contains("recipes/kubectl-convert.toml"), "{err}");
        assert_eq!(lines_with(&err, "W_RECIPE_"), 0, "{err}");
    }
    assert!(!project.join("lodi.lock").exists());
    assert!(server.requests().is_empty(), "{:?}", server.requests());

    // Named by LODI_REPO, the repository's own folder is read, and only it.
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let named = tree.lodi(&project, &server, &["lock"], &[("LODI_REPO", &tree.hosts)]);
    assert_eq!(named.status.code(), Some(0), "{named:?}");
    let written: serde_json::Value =
        serde_json::from_slice(&fs::read(project.join("lodi.lock")).unwrap()).unwrap();
    assert_eq!(
        written["packages"]["kubectl-convert"]["recipe"]["input"],
        "user"
    );
}

#[test]
fn an_edited_or_broken_recipe_affects_only_its_own_tool() {
    let tree = Tree::new("edit", "");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    tree.recipe("kfmt.toml", &tool_recipe("kfmt", "kfmt.test"));
    // Broken: its name does not match its file.
    tree.recipe("broken.toml", &tool_recipe("mended", "broken.test"));
    let server = Server::start(upstream(&[("kfmt", "kfmt.test")]));
    let project = tree.dir.join("project");
    fs::create_dir_all(&project).unwrap();
    let manifest = "[project]\nversion = \"1\"\n\n[tools]\nkfmt = \"latest\"\n\
                    kubectl-convert = \"latest\"\n";
    fs::write(project.join("lodi.toml"), manifest).unwrap();
    let repo: &[(&str, &Path)] = &[("LODI_REPO", &tree.hosts)];

    let first = tree.lodi(&project, &server, &["lock"], repo);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let check = tree.lodi(&project, &server, &["lock", "--check"], repo);
    assert_eq!(check.status.code(), Some(0), "{check:?}");

    // A comment is an edit: the recipe's digest moves, and only its tool is stale.
    tree.recipe(
        "kubectl-convert.toml",
        &format!("# edited by hand\n{KUBECTL_CONVERT}"),
    );
    let stale = tree.lodi(&project, &server, &["lock", "--check"], repo);
    assert_eq!(stale.status.code(), Some(10), "{stale:?}");
    let err = stderr(&stale);
    assert!(err.contains("E_LOCK_STALE"), "{err}");
    assert!(err.contains("kubectl-convert"), "{err}");
    assert!(!err.contains("kfmt"), "{err}");

    server.clear();
    let relock = tree.lodi(&project, &server, &["lock"], repo);
    assert_eq!(relock.status.code(), Some(0), "{relock:?}");
    assert!(
        stdout(&relock).contains("resolved kubectl-convert)"),
        "{relock:?}"
    );
    for request in server.requests() {
        assert!(request.starts_with("https://dl.k8s.io/"), "{request}");
    }

    // The broken file refuses only the tool that names it, naming the file.
    let before = fs::read(project.join("lodi.lock")).unwrap();
    fs::write(
        project.join("lodi.toml"),
        format!("{manifest}broken = \"latest\"\n"),
    )
    .unwrap();
    let refused = tree.lodi(&project, &server, &["lock"], repo);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let err = stderr(&refused);
    assert!(err.contains("E_RECIPE_INVALID"), "{err}");
    assert!(err.contains("recipes/broken.toml"), "{err}");
    assert_eq!(fs::read(project.join("lodi.lock")).unwrap(), before);

    // `lodi search` skips it with a warning and lists the others.
    let search = tree.lodi(&project, &server, &["search", "k"], repo);
    assert_eq!(search.status.code(), Some(0), "{search:?}");
    let err = stderr(&search);
    assert_eq!(lines_with(&err, "W_RECIPE_SKIPPED"), 1, "{err}");
    assert!(err.contains("recipes/broken.toml"), "{err}");
    let out = stdout(&search);
    assert!(out.contains("kubectl-convert"), "{out}");
    assert!(out.contains("kfmt"), "{out}");
}

/// The binary the compatibility test applies the lock with: the one `LODI_COMPAT_BINARY` names
/// while recording (the released lodi 1.5.0), this build's otherwise.
fn compat_binary() -> std::ffi::OsString {
    std::env::var_os("LODI_COMPAT_BINARY")
        .unwrap_or_else(|| std::ffi::OsString::from(env!("CARGO_BIN_EXE_lodi")))
}

fn compat_recording() -> bool {
    std::env::var_os("LODI_RECORD_EXPECTED").is_some()
        && std::env::var_os("LODI_COMPAT_BINARY").is_some()
}

/// The oldest lodi that can use a `lodi.lock` 1.6.0 writes (rel-1-release-1-6-0, LD-411).
/// This build locks a home whose tool comes from the repository's `recipes/` folder; then, in an
/// emptied home, the lock is applied `--locked` by lodi 1.5.0 while recording and by this build
/// otherwise. What each prints, fetches, leaves in the lock and puts on the home's `PATH` is held
/// to one golden recorded from 1.5.0, so 1.5.0 applies such a lock exactly as 1.6.0 does and the
/// lock needs no newer lodi. The recipe is not edited: lodi 1.5 reads the lock, not `recipes/`.
#[test]
fn a_lock_pinning_a_repository_recipe_is_one_lodi_1_5_0_applies_as_1_6_0_does() {
    let tree = Tree::new("compat-150", "kubectl-convert = \"latest\"\n");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let server = Server::start(upstream(&[]));
    let written = tree.home_apply(&server);
    assert_eq!(written.status.code(), Some(0), "{written:?}");
    let lock = fs::read(tree.hosts.join("lodi.lock")).unwrap();
    assert_eq!(tree.locked("kubectl-convert")["recipe"]["input"], "user");

    // A person's second machine: the same repository, a home lodi has never applied.
    let second = tree.dir.join("second");
    fs::create_dir_all(&second).unwrap();
    server.clear();
    let hosts = tree.hosts.to_str().unwrap();
    let applied = Command::new(compat_binary())
        .args(["home", "apply", "--locked", hosts, "--host", "box"])
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &second)
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .current_dir(&tree.dir)
        .output()
        .unwrap();
    let run = Command::new(second.join(".local/share/lodi/home-scope/profile/bin/kubectl-convert"))
        .output();
    let dir = tree.dir.to_str().unwrap();
    let mut requests = server.requests();
    requests.sort();
    let transcript = format!(
        "exit: {:?}\nstdout:\n{}stderr:\n{}requests:\n{}\nlock kept: {}\nkubectl-convert: {}",
        applied.status.code(),
        stdout(&applied).replace(dir, "<dir>"),
        stderr(&applied).replace(dir, "<dir>"),
        requests.join("\n"),
        fs::read(tree.hosts.join("lodi.lock")).unwrap() == lock,
        run.map(|r| stdout(&r)).unwrap_or_else(|e| format!("{e}\n")),
    );
    let golden = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/user-recipes/lock-applied-by-1.5.0.txt");
    if compat_recording() {
        fs::create_dir_all(golden.parent().unwrap()).unwrap();
        fs::write(&golden, &transcript).unwrap();
        return;
    }
    assert_eq!(transcript, fs::read_to_string(&golden).unwrap());
    assert_eq!(applied.status.code(), Some(0), "{applied:?}");
    assert!(
        transcript.ends_with(&said("kubectl-convert")),
        "{transcript}"
    );
}
