//! rf-1 (LD-409): recipes of your own in the `recipes/` folder at the root of your lodi
//! repository, run as the real binary against a loopback server.
//!
//! The recipes are the `recipes/` folder of the config a switch acts on (#692, #710). `shell`,
//! `develop`, `run` and `search` look for no config, so only the built-in recipes resolve there
//! (#698 story 39). Every fetch is rewritten to a loopback server that serves a
//! synthetic Kubernetes release bucket, so nothing leaves the machine.
//!
//! The home rows run `lodi switch --home` on the config, whose `recipes/` it reads (LD-524).

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

/// A scratch tree: a person's home, and a repository `hosts/` with one host `box`, a `recipes/`
/// folder at its root and, beside them, the flat config's `home.toml` declaring `tools`.
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
        fs::create_dir_all(&home).unwrap();
        fs::create_dir_all(hosts.join("box")).unwrap();
        fs::create_dir_all(hosts.join("recipes")).unwrap();
        fs::write(hosts.join("box/host.toml"), "[host]\ndistro = \"debian\"\n").unwrap();
        fs::write(
            hosts.join("home.toml"),
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

    /// `lodi switch --home` on the config, with the tree as the machine's `--root`.
    fn home_apply(&self, server: &Server) -> Output {
        let hosts = self.hosts.to_str().unwrap();
        support::machine_at(&self.dir);
        let require = [("LODI_HOST_REQUIRE_ROOT", Path::new("1"))];
        let args = [
            "switch",
            "--home",
            hosts,
            "--root",
            self.dir.to_str().unwrap(),
        ];
        self.lodi(&self.dir, server, &args, &require)
    }

    /// The home's tool section of the repository's root lock.
    fn locked(&self, tool: &str) -> serde_json::Value {
        let root: serde_json::Value =
            serde_json::from_slice(&fs::read(self.hosts.join("lodi.lock")).unwrap()).unwrap();
        root["homes"]["home.toml"]["tools"][tool].clone()
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

/// `lodi develop` with the project's text allowed for this run only: it locks, then runs a shell that does nothing.
const DEVELOP: &[&str] = &["develop", "--trust", "--", "/bin/sh", "-c", ":"];

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
    // A recipe in every place a person might guess outside the config: none of them is a
    // repository's root. The config is one (LD-524), so its `recipes/` comes after the switch.
    let config = tree.home.join(".config/lodi");
    let project = tree.dir.join("project");
    let guess = |dir: PathBuf| {
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("kubectl-convert.toml"), KUBECTL_CONVERT).unwrap();
    };
    for dir in [
        tree.home.join("recipes"),
        tree.home.join(".config/recipes"),
        tree.home.join(".local/share/lodi/recipes"),
        project.join("recipes"),
    ] {
        guess(dir);
    }
    fs::create_dir_all(&config).unwrap();
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

    // `lodi switch --home` on the config, with the tree as the machine's `--root` (#699).
    support::machine_at(&tree.dir);
    let require = [("LODI_HOST_REQUIRE_ROOT", Path::new("1"))];
    let root = tree.dir.to_str().unwrap();
    let args = ["switch", "--home", config.to_str().unwrap(), "--root", root];
    let home = tree.lodi(&tree.dir, &server, &args, &require);
    let lock = tree.lodi(&project, &server, DEVELOP, &[]);
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
}

/// `shell`, `develop`, `run` and `search` never look for a config (#698 story 39): a config
/// `LODI_REPO` names, with a `recipes/` folder, is not read, and only the built-in recipes
/// resolve.
#[test]
fn a_project_and_search_never_take_a_config_s_recipes() {
    let tree = Tree::new("found", "");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    let server = Server::start(upstream(&[]));
    let project = tree.dir.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(
        project.join("lodi.toml"),
        "[project]\nversion = \"1\"\n\n[tools]\nkubectl-convert = \"latest\"\n",
    )
    .unwrap();
    let repo = [("LODI_REPO", tree.hosts.as_path())];
    let develop = tree.lodi(&project, &server, DEVELOP, &repo);
    assert_eq!(develop.status.code(), Some(4), "{develop:?}");
    assert!(stderr(&develop).contains("E_NO_RECIPE"), "{develop:?}");
    let search = tree.lodi(&project, &server, &["search", "kubectl-conv"], &repo);
    assert!(!stdout(&search).contains("kubectl-convert"), "{search:?}");
    assert_eq!(lines_with(&stderr(&search), "W_RECIPE_"), 0, "{search:?}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());
}

/// Run as `lodi switch --home` on the config, whose `recipes/` it reads (LD-524); `lodi search`
/// over a broken file reads no config at all.
#[test]
fn an_edited_or_broken_recipe_affects_only_its_own_tool() {
    let tree = Tree::new("edit", "kfmt = \"latest\"\nkubectl-convert = \"latest\"\n");
    tree.recipe("kubectl-convert.toml", KUBECTL_CONVERT);
    tree.recipe("kfmt.toml", &tool_recipe("kfmt", "kfmt.test"));
    // Broken: its name does not match its file.
    tree.recipe("broken.toml", &tool_recipe("mended", "broken.test"));
    let server = Server::start(upstream(&[("kfmt", "kfmt.test")]));

    let first = tree.home_apply(&server);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    server.clear();
    let fresh = tree.home_apply(&server);
    assert_eq!(fresh.status.code(), Some(0), "{fresh:?}");
    assert!(server.requests().is_empty(), "{:?}", server.requests());

    // The broken file refuses only the tool that names it, naming the file.
    let before = fs::read(tree.hosts.join("lodi.lock")).unwrap();
    fs::write(
        tree.hosts.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[tools]\nkfmt = \"latest\"\n\
         kubectl-convert = \"latest\"\nbroken = \"latest\"\n",
    )
    .unwrap();
    let refused = tree.home_apply(&server);
    assert_eq!(refused.status.code(), Some(4), "{refused:?}");
    let err = stderr(&refused);
    assert!(err.contains("E_RECIPE_INVALID"), "{err}");
    assert!(err.contains("recipes/broken.toml"), "{err}");
    assert_eq!(fs::read(tree.hosts.join("lodi.lock")).unwrap(), before);
}

/// A switch never moves a locked tool (LD-528): when a tool's version or its recipe of your own
/// changed since the lock, it stops with `E_LOCK_STALE` naming `lodi update` and leaves the lock
/// as it was; `lodi update` locks the change, and the next switch applies it.
#[test]
fn a_changed_tool_or_recipe_stops_switch_until_lodi_update() {
    let tree = Tree::new("stale", "kfmt = \"latest\"\n");
    tree.recipe("kfmt.toml", &tool_recipe("kfmt", "kfmt.test"));
    let server = Server::start(upstream(&[("kfmt", "kfmt.test")]));
    let first = tree.home_apply(&server);
    assert_eq!(first.status.code(), Some(0), "{first:?}");
    let lock = || fs::read(tree.hosts.join("lodi.lock")).unwrap();
    let stale = |output: &Output, before: &[u8]| {
        assert_eq!(output.status.code(), Some(10), "{output:?}");
        let err = stderr(output);
        assert!(err.contains("E_LOCK_STALE"), "{err}");
        assert!(err.contains("lodi update"), "{err}");
        assert_eq!(lock(), before, "{err}");
        assert!(!tree.hosts.join("home.lock").exists());
    };

    let before = lock();
    let edited = format!("{}\n# edited\n", tool_recipe("kfmt", "kfmt.test"));
    tree.recipe("kfmt.toml", &edited);
    stale(&tree.home_apply(&server), &before);

    fs::write(
        tree.hosts.join("home.toml"),
        format!("[home]\nversion = \"1\"\n\n[tools]\nkfmt = \"{VERSION}\"\n"),
    )
    .unwrap();
    stale(&tree.home_apply(&server), &before);

    let hosts = tree.hosts.to_str().unwrap();
    let root = tree.dir.to_str().unwrap();
    let require = [("LODI_HOST_REQUIRE_ROOT", Path::new("1"))];
    let update = tree.lodi(
        &tree.dir,
        &server,
        &["update", hosts, "--root", root],
        &require,
    );
    assert_eq!(update.status.code(), Some(0), "{update:?}");
    assert_eq!(
        tree.locked("kfmt")["recipe"]["sha256"],
        sha256_tagged(edited.as_bytes())
    );
    let again = tree.home_apply(&server);
    assert_eq!(again.status.code(), Some(0), "{again:?}");
}
