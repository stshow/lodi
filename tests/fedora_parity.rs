//! fx-1 (LD-482): everything lodi did before 1.7 on Debian, Ubuntu and Arch, also on Fedora 44.
//!
//! The older checks, grouped by area: the home and its program modules; exact mode, drift and
//! the reconcile; a user-owned host directory, a file owner and a metapackage; and the recipes.
//! Every case is the real binary against a scratch `--root` with the fake machine of
//! `tests/support/fakehost.rs` behind it, whose dnf5 and rpm answer as a Fedora 44 guest printed,
//! and, where the check compares, the same steps on a scratch Debian root. Upstreams are served
//! on loopback from the recorded fixtures through `LODI_FETCH_REWRITE`; nothing reaches the
//! network or `/` (`AGENTS.md` §8), and nothing runs as root.

#[path = "support/fakehost.rs"]
mod fakehost;
#[path = "support/hostroot.rs"]
mod hostroot;
mod support;
/// The suite's one wait ceiling, declared once at this binary's root (`tests/support/wait.rs`).
#[path = "support/wait.rs"]
mod wait;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use fakehost::{Case, Machine, Pkg, err, out, story};
use hostroot::ids;
use lodi::catalogue::{BUILTIN_TOOLS, Recipe, arch_vars, parse_recipe, render};
use lodi::upstream::strategy::Discovery;
use lodi::util::sha256_hex;
use support::{Server, dir, file, gzip, tar};

/// The child half of every held apply of this binary (`tests/support/hostroot.rs`).
#[test]
fn child_apply() {
    if !hostroot::is_child() {
        return;
    }
    hostroot::child_apply();
}

/// A package at a Fedora 44 build.
fn fc44(name: &str, version: &str) -> Pkg {
    let mut pkg = Pkg::new(name);
    pkg.version = format!("0:{version}.fc44");
    pkg
}

/// The login of the test's own user, as `lodi home apply SOURCE` selects `home/<login>/`.
fn login() -> String {
    let id = Command::new("id").arg("-un").output().unwrap();
    String::from_utf8(id.stdout).unwrap().trim().to_string()
}

/// A case called `box` whose passwd names the test's own user by its login, with its home at
/// `/people/sample`, so a home's apply drops to who the test already is.
fn machine_box(name: &str, machine: Machine) -> Case {
    let case = Case::new(name, machine);
    let (uid, gid) = ids(&case.root);
    case.root.write(
        "etc/passwd",
        &format!(
            "root:x:0:0::/root:/bin/sh\n{}:x:{uid}:{gid}::/people/sample:/bin/sh\n",
            login()
        ),
    );
    case.root
        .write("etc/group", &format!("root:x:0:\n{}:x:{gid}:\n", login()));
    case.root.write("etc/hostname", "box\n");
    fs::create_dir_all(case.root.path("people/sample")).unwrap();
    case
}

/// The user's repository `~/lodi` on the case, with `box/host.toml` and the user's home.
fn repository(case: &Case, host: &str, home: &str) -> PathBuf {
    let repo = case.root.path("people/sample/lodi");
    let selected = repo.join("box/home").join(login());
    fs::create_dir_all(&selected).unwrap();
    fs::write(repo.join("box/host.toml"), host).unwrap();
    fs::write(selected.join("home.toml"), home).unwrap();
    repo
}

/// `lodi VERB EXTRA… --root <the case's root>` as `sudo` runs it for the test's own user, from a
/// scratch directory, with a decoy `HOME` and no terminal.
// check-host-safety: refusal — one place, and it appends this scratch root as --root.
fn top(case: &Case, verb: &str, extra: &[&str], envs: &[(&str, &str)]) -> Output {
    let cwd = case.base().join("cwd");
    fs::create_dir_all(&cwd).unwrap();
    let decoy = case.base().join("decoy-home");
    fs::create_dir_all(&decoy).unwrap();
    let (uid, gid) = ids(&case.root);
    let _spawning = fakehost::spawning();
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .arg(verb)
        .args(extra)
        .arg("--root")
        .arg(&case.root.dir)
        .current_dir(&cwd)
        .env("PATH", case.fake_path())
        .env("HOME", &decoy)
        .env("XDG_CONFIG_HOME", decoy.join(".config"))
        .env("XDG_DATA_HOME", decoy.join(".local/share"))
        .env("LODI_HOME", decoy.join("lodi-home"))
        .env("SUDO_UID", uid.to_string())
        .env("SUDO_GID", gid.to_string())
        .env_remove("LODI_REPO")
        .env("LODI_HOST_REQUIRE_ROOT", "1")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("lodi runs")
}

/// `lodi ARGS…` as the user of the case's home, in `cwd`: no root, no host verb.
fn as_user(case: &Case, cwd: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let home = case.root.path("people/sample");
    Command::new(env!("CARGO_BIN_EXE_lodi"))
        .args(args)
        .current_dir(cwd)
        .env_clear()
        .env("PATH", "/usr/bin:/bin")
        .env("HOME", &home)
        .env("SHELL", "/bin/sh")
        .envs(envs.iter().copied())
        .stdin(Stdio::null())
        .output()
        .expect("lodi runs")
}

fn ok(output: &Output) {
    assert_eq!(output.status.code(), Some(0), "{}", story(output));
}

// ------------------------------------------------------------- the home and its programs ---

/// The home the older rows declare: a file of lodi's own at 0600, a path the user already had
/// taken over at 0644 and restored on removal, and each program module the rows load.
const HOME: &str = r#"[home]
version = "1"

[home.file.".lodi-gate-managed.conf"]
text = "managed by the lodi home scope"
mode = "0600"
on_remove = "delete"

[home.file.".lodi-gate-unmanaged.conf"]
text = "taken over by the lodi home scope"
mode = "0644"
on_remove = "restore"

[programs.git]
user.name = "Lodi Gate"
init.defaultBranch = "main"
core.editor = "nvim"
alias = { st = "status --short" }

[programs.tmux]
prefix = "C-a"
mouse = true
history-limit = 50000

[programs.nvim]
leader = " "
opt.number = true
opt.shiftwidth = 3

[programs.fish]
env = { EDITOR = "nvim" }
path = ["~/.local/bin"]
aliases = { ll = "ls -l" }

[programs.bash]
env = { EDITOR = "nvim" }
path = ["~/.local/bin"]
histsize = 10000

[programs.zsh]
env = { EDITOR = "nvim" }
setopt = ["HIST_IGNORE_DUPS"]
"#;

/// What the user had at the path the home takes over.
const OWN: &str = "the user wrote this\n";

/// Every file below the case's home except the repository and lodi's state: its path, mode and
/// bytes, the home's own spelling replaced so that two roots compare.
fn home_files(case: &Case) -> BTreeMap<String, (u32, String)> {
    fn walk(dir: &Path, home: &Path, out: &mut BTreeMap<String, (u32, String)>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(home).unwrap().display().to_string();
            if rel == "lodi" || rel.ends_with("state.json") || rel.contains("backups") {
                continue;
            }
            let meta = fs::symlink_metadata(&path).unwrap();
            if meta.is_dir() {
                walk(&path, home, out);
            } else if meta.is_file() {
                let text = String::from_utf8_lossy(&fs::read(&path).unwrap())
                    .replace(&home.display().to_string(), "<home>");
                out.insert(rel, (meta.permissions().mode() & 0o7777, text));
            }
        }
    }
    let home = case.root.path("people/sample");
    let mut out = BTreeMap::new();
    walk(&home, &home, &mut out);
    out
}

/// The whole home story on one machine: apply, second apply, drift refused then confirmed, and
/// removal restoring what the user had. Returns the files after the first apply.
fn home_story(case: &Case, distro: &str) -> BTreeMap<String, (u32, String)> {
    let home = case.root.path("people/sample");
    fs::write(home.join(".lodi-gate-unmanaged.conf"), OWN).unwrap();
    let host = format!("[host]\ndistro = \"{distro}\"\n");
    let repo = repository(case, &host, HOME);
    let source = repo.display().to_string();

    let applied = top(case, "apply", &[&source], &[]);
    ok(&applied);
    assert!(
        out(&applied).contains("W_REPLACED_UNMANAGED"),
        "{}",
        story(&applied)
    );
    let files = home_files(case);
    for (path, mode) in [
        (".lodi-gate-managed.conf", 0o600),
        (".lodi-gate-unmanaged.conf", 0o644),
    ] {
        assert_eq!(
            files.get(path).map(|f| f.0),
            Some(mode),
            "{path}: {files:?}"
        );
    }
    for rendered in [".config/tmux/tmux.conf", ".config/fish/conf.d/lodi.fish"] {
        assert!(files.contains_key(rendered), "{rendered}: {files:?}");
    }
    let uid = ids(&case.root).0;
    let managed = home.join(".lodi-gate-managed.conf");
    assert_eq!(fs::metadata(&managed).unwrap().uid(), uid);

    // A second apply changes nothing, not even a modification time.
    let stamp = fs::metadata(&managed).unwrap().mtime_nsec();
    ok(&top(case, "apply", &[&source], &[]));
    assert_eq!(home_files(case), files);
    assert_eq!(fs::metadata(&managed).unwrap().mtime_nsec(), stamp);

    // A hand edit is drift: refused, nothing written, until the user confirms it.
    fs::write(&managed, "edited by hand\n").unwrap();
    let refused = top(case, "apply", &[&source], &[]);
    assert_eq!(refused.status.code(), Some(8), "{}", story(&refused));
    assert!(err(&refused).contains("E_DRIFT"), "{}", story(&refused));
    assert_eq!(fs::read_to_string(&managed).unwrap(), "edited by hand\n");
    let confirmed = as_user(
        case,
        &home,
        &[
            "home",
            "apply",
            "--overwrite-drift",
            &source,
            "--host",
            "box",
        ],
        &[],
    );
    ok(&confirmed);
    assert_eq!(home_files(case), files, "{}", story(&confirmed));

    // Removal: the taken-over path gets the user's bytes back, lodi's own file goes.
    let selected = repo.join("box/home").join(login()).join("home.toml");
    fs::write(&selected, "[home]\nversion = \"1\"\n").unwrap();
    ok(&top(case, "apply", &[&source], &[]));
    assert_eq!(
        fs::read_to_string(home.join(".lodi-gate-unmanaged.conf")).unwrap(),
        OWN
    );
    assert!(!managed.exists());
    let quiet = top(case, "apply", &[&source], &[]);
    ok(&quiet);
    assert!(
        out(&quiet).matches("nothing to do").count() >= 2,
        "{}",
        story(&quiet)
    );
    files
}

#[test]
fn home_and_program_modules_apply_on_fedora_as_on_debian() {
    let fedora = machine_box("fx-home-fedora", Machine::fedora());
    let debian = machine_box("fx-home-debian", Machine::debian());
    let on_fedora = home_story(&fedora, "fedora");
    let on_debian = home_story(&debian, "debian");
    assert_eq!(on_fedora, on_debian, "Fedora's home differs from Debian's");
}

// ------------------------------------------------------ exact mode, drift and the reconcile ---

/// The names of `[packages.fedora]`'s `add` list, in the order the file has them.
fn declared(manifest: &str) -> Vec<String> {
    manifest
        .split("\n[packages.fedora]\n")
        .nth(1)
        .unwrap_or_else(|| panic!("no [packages.fedora]:\n{manifest}"))
        .lines()
        .skip_while(|line| *line != "add = [")
        .skip(1)
        .take_while(|line| *line != "]")
        .map(|line| line.trim().trim_end_matches(',').trim_matches('"'))
        .filter(|name| !name.is_empty() && !name.starts_with('#'))
        .map(ToString::to_string)
        .collect()
}

fn remove_by_hand(case: &Case, name: &str) {
    case.edit(|m| {
        m["installed"].as_object_mut().unwrap().remove(name);
    });
}

#[test]
fn exact_drift_and_reconcile_on_fedora() {
    let machine = Machine::fedora()
        .with(fc44("tree", "2.2.1-4").depends(&["glibc"]))
        .with(fc44("jq", "1.8.1-1").depends(&["glibc"]))
        .with(fc44("bat", "0.25.0-2").depends(&["glibc"]))
        .offering(fc44("htop", "3.4.1-3").depends(&["glibc"]))
        .offering(fc44("zip", "3.0-44").depends(&["glibc"]));
    let case = Case::new("fx-exact", machine);
    ok(&case.import());
    let manifest = case.manifest();
    assert!(manifest.contains("packages = \"exact\""), "{manifest}");
    assert_eq!(
        declared(&manifest),
        ["bash", "bat", "dnf5", "jq", "kernel", "tree"]
    );

    // A hand install is drift: named, refused at 11 with the machine unchanged, then removed
    // when the user says so; a second apply has nothing to do.
    case.install_by_hand(fc44("htop", "3.4.1-3").depends(&["glibc"]));
    let plan = case.plan();
    ok(&plan);
    assert!(
        err(&plan).contains("W_DRIFT: package htop was installed by hand"),
        "{}",
        story(&plan)
    );
    let before = case.machine();
    let refused = case.apply(&[]);
    assert_eq!(refused.status.code(), Some(11), "{}", story(&refused));
    assert!(
        err(&refused).contains("E_DECLINED") && err(&refused).contains("htop"),
        "{}",
        story(&refused)
    );
    assert_eq!(case.machine(), before, "the refusal moved the machine");
    ok(&case.apply(&["--overwrite-drift"]));
    assert!(!case.installed().contains("htop"));
    assert_eq!(out(&case.apply(&[])), "nothing to do\n");

    // Outside lodi: zip installed, tree removed. By hand: a note, jq after the kernel, and
    // bat's line deleted. The re-import adopts zip, drops tree and keeps every edit.
    case.install_by_hand(fc44("zip", "3.0-44").depends(&["glibc"]));
    remove_by_hand(&case, "tree");
    let text = case
        .manifest()
        .replacen(
            "  \"jq\",\n  \"kernel\",\n",
            "  \"kernel\",\n  \"jq\",\n",
            1,
        )
        .replacen("add = [\n", "# my own note\nadd = [\n", 1);
    case.set_manifest(&text);
    case.delete_line("bat");
    let again = case.import();
    ok(&again);
    assert!(
        out(&again).contains("1 added, 1 removed") && out(&again).contains("0 conflict"),
        "{}",
        story(&again)
    );
    let manifest = case.manifest();
    assert_eq!(
        declared(&manifest),
        ["bash", "dnf5", "kernel", "jq", "zip"],
        "{manifest}"
    );
    assert!(manifest.contains("# my own note\nadd = ["), "{manifest}");

    // The deleted line's package is removed, and a later import does not add it back.
    let apply = case.apply(&[]);
    ok(&apply);
    assert!(!case.installed().contains("bat"), "{}", story(&apply));
    assert!(case.installed().contains("zip") && case.installed().contains("jq"));
    let noop = case.import();
    ok(&noop);
    assert_eq!(case.manifest(), manifest, "{}", story(&noop));
    assert!(!case.installed().contains("bat"));
    assert!(out(&case.plan()).ends_with("nothing to do\n"));
}

// ----------------------------------------- a user-owned host directory, owner, metapackage ---

#[test]
fn host_directory_owner_and_metapackage_on_fedora() {
    let machine = Machine::fedora()
        .with(fc44("server-meta", "44-1").depends(&["gitlike", "lvmlike"]))
        .with(fc44("gitlike", "2.51.0-1").dep().depends(&["glibc"]))
        .with(fc44("lvmlike", "2.03.32-1").depends(&["glibc"]));
    let case = machine_box("fx-hostdir", machine);
    ok(&case.import());
    let imported = case.manifest();
    assert!(imported.contains("\"server-meta\""), "{imported}");
    assert!(!imported.contains("\"gitlike\""), "{imported}");

    // The host is a directory the user owns, chosen by the machine's hostname, declaring a file
    // owned by that user.
    let owned = "/etc/fx-owned.conf";
    let user = login();
    let host = format!(
        "{imported}\n[files.\"{owned}\"]\ncontent = \"the user's own\\n\"\nowner = \"{user}\"\n\
         group = \"{user}\"\nmode = \"0640\"\n"
    );
    case.root.write("hosts/box/host.toml", &host);
    case.root
        .write("hosts/other/host.toml", "not a manifest [\n");
    let hosts = case.root.path("hosts").display().to_string();
    let (uid, gid) = ids(&case.root);
    assert_eq!(
        fs::metadata(case.root.path("hosts/box")).unwrap().uid(),
        uid
    );

    let before = case.installed();
    let apply = case.apply(&[&hosts]);
    ok(&apply);
    let meta = fs::metadata(case.root.path("etc/fx-owned.conf")).unwrap();
    assert_eq!((meta.uid(), meta.gid()), (uid, gid), "{}", story(&apply));
    assert_eq!(meta.permissions().mode() & 0o7777, 0o640);
    assert_eq!(case.root.read("etc/fx-owned.conf"), "the user's own\n");

    // A hand removal takes the metapackage and a member; the next apply puts both back and
    // removes nothing, and a second apply has nothing to do.
    remove_by_hand(&case, "server-meta");
    remove_by_hand(&case, "gitlike");
    let plan = case.plan_with(&[&hosts]);
    ok(&plan);
    assert!(
        out(&plan).contains("+ package server-meta") && !err(&plan).contains("W_DRIFT"),
        "{}",
        story(&plan)
    );
    ok(&case.apply(&[&hosts]));
    assert_eq!(case.installed(), before);
    assert!(case.is_explicit("server-meta") && case.is_explicit("lvmlike"));
    let again = case.apply(&[&hosts]);
    ok(&again);
    assert_eq!(out(&again), "nothing to do\n", "{}", story(&again));
}

// ---------------------------------------------------------------------------- the recipes ---

/// The catalogue tool of the older rows, and the recipe batch (rb-1).
const CATALOGUE: &str = "jq";
const BATCH: [&str; 18] = [
    "gh",
    "bat",
    "fzf",
    "lazygit",
    "yq",
    "delta",
    "neovim",
    "starship",
    "k9s",
    "ruff",
    "kubectl",
    "helm",
    "zoxide",
    "eza",
    "opentofu",
    "actionlint",
    "sops",
    "shfmt",
];

/// A recipe of the user's own, in the repository's `recipes/` folder (rf-1).
const USER_RECIPE: &str = "[recipe]\nname = \"fx-tool\"\ndescription = \"a tool of my own\"\n\
    homepage = \"https://fx.example/\"\n\n[versions]\nstrategy = \"text_index\"\n\
    url = \"https://fx.example/latest.txt\"\nline = \"v${version}\"\n\n\
    [asset]\nurl = \"https://fx.example/v${version}/fx-tool-${arch_alt}\"\nformat = \"binary\"\n\
    checksum_sidecar = \".sha256\"\n\n[spec]\nbin = [\"bin/fx-tool\"]\npath = [\"bin\"]\n";

fn recipe(name: &str) -> Recipe {
    let file = format!("{name}.toml");
    let (_, text) = BUILTIN_TOOLS
        .iter()
        .find(|(f, _)| *f == file)
        .unwrap_or_else(|| panic!("{file} is not a built-in recipe"));
    parse_recipe(text, &file).unwrap_or_else(|e| panic!("{file}: {}", e.message))
}

fn rendered(recipe: &Recipe, template: &str, version: &str, tag: &str) -> String {
    let mut vars = arch_vars(&recipe.arch_names, "x86_64");
    vars.push(("version".into(), version.into()));
    vars.push(("tag".into(), tag.into()));
    let refs: Vec<(&str, &str)> = vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    render(template, &refs).unwrap_or_else(|e| panic!("{}: {e}", recipe.file))
}

fn numeric(version: &str) -> Vec<u64> {
    version.split('.').map(|p| p.parse().unwrap()).collect()
}

/// An artifact shaped as the recipe says upstream's is, each bin entry a script printing
/// `NAME VERSION (synthetic)`.
fn artifact(recipe: &Recipe, version: &str) -> Vec<u8> {
    let asset = &recipe.assets[0];
    if asset.format == "binary" {
        return format!(
            "#!/bin/sh\necho \"{} {version} (synthetic)\"\n",
            recipe.name
        )
        .into();
    }
    assert_eq!(asset.format, "tar.gz", "{}", recipe.file);
    let mut prefix = String::new();
    let mut members = Vec::new();
    for level in 0..asset.strip_components {
        prefix.push_str(&format!("top{level}/"));
        members.push(dir(&prefix));
    }
    for entry in &recipe.bin {
        if let Some((parent, _)) = entry.rsplit_once('/') {
            members.push(dir(&format!("{prefix}{parent}/")));
        }
        let name = entry.rsplit('/').next().unwrap();
        let body = format!("#!/bin/sh\necho \"{name} {version} (synthetic)\"\n");
        members.push(file(&format!("{prefix}{entry}"), &body, 0o755));
    }
    gzip(&tar(&members))
}

/// What `name`'s upstream answers, from its recorded fixture, with the chosen asset replaced by
/// a synthetic one and its digest by that asset's own; and the version it resolves to.
fn upstream(name: &str) -> (String, BTreeMap<String, Vec<u8>>) {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalogue");
    let recipe = recipe(name);
    let mut files = BTreeMap::new();
    let version = match &recipe.versions {
        Discovery::GithubReleases {
            repo,
            releases,
            tag,
            asset,
            ..
        } => {
            let path = fixtures
                .join("github_releases")
                .join(format!("{}.json", repo.replace('/', "--")));
            let mut payload: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            let (before, after) = tag.split_once("${version}").unwrap();
            let mut best: Option<(Vec<u64>, String, String)> = None;
            for release in payload.as_array().unwrap().iter().take(*releases as usize) {
                let tag = release["tag_name"].as_str().unwrap();
                if release["draft"].as_bool().unwrap() || release["prerelease"].as_bool().unwrap() {
                    continue;
                }
                let Some(middle) = tag.strip_prefix(before).and_then(|r| r.strip_suffix(after))
                else {
                    continue;
                };
                if middle.split('.').all(|p| p.parse::<u64>().is_ok())
                    && best.as_ref().is_none_or(|(v, _, _)| numeric(middle) > *v)
                {
                    best = Some((numeric(middle), middle.to_string(), tag.to_string()));
                }
            }
            let (_, version, tag) = best.unwrap_or_else(|| panic!("{name}: no release"));
            let wanted = rendered(&recipe, asset, &version, &tag);
            let bytes = artifact(&recipe, &version);
            for release in payload.as_array_mut().unwrap() {
                for hit in release["assets"].as_array_mut().unwrap() {
                    if hit["name"].as_str() == Some(wanted.as_str()) {
                        hit["digest"] = format!("sha256:{}", sha256_hex(&bytes)).into();
                        hit["size"] = bytes.len().into();
                        let url = hit["browser_download_url"].as_str().unwrap().to_string();
                        files.insert(url, bytes.clone());
                    }
                }
            }
            let api = format!("https://api.github.com/repos/{repo}/releases?per_page=30");
            files.insert(api, serde_json::to_vec(&payload).unwrap());
            version
        }
        Discovery::TextIndex { url, line } => {
            let index_url = rendered(&recipe, url, "", "");
            let leaf = index_url.rsplit('/').next().unwrap();
            let index = fs::read(fixtures.join(name).join(leaf)).unwrap();
            let (before, after) = line.split_once("${version}").unwrap();
            let version = String::from_utf8(index.clone())
                .unwrap()
                .lines()
                .find_map(|l| {
                    Some(
                        l.strip_prefix(before)?
                            .strip_suffix(after)?
                            .trim()
                            .to_string(),
                    )
                })
                .unwrap_or_else(|| panic!("{name}: the recorded index names no version"));
            let asset_url = rendered(
                &recipe,
                recipe.assets[0].url.as_deref().unwrap(),
                &version,
                "",
            );
            let bytes = artifact(&recipe, &version);
            files.insert(index_url, index);
            files.insert(
                format!("{asset_url}.sha256"),
                format!("{}\n", sha256_hex(&bytes)).into(),
            );
            files.insert(asset_url, bytes);
            version
        }
        other => panic!("{name}: no {other:?} here"),
    };
    (version, files)
}

/// The recipes' story on one machine: `lodi apply` of the host folder, then, as the user, a
/// project's `lodi lock` and `lodi run` of a task running every tool. Returns the lock's bytes
/// and what the task printed.
fn recipes_story(case: &Case, distro: &str) -> (Vec<u8>, String) {
    let mut served = BTreeMap::new();
    let mut said = Vec::new();
    for name in std::iter::once(CATALOGUE).chain(BATCH) {
        let (version, files) = upstream(name);
        served.extend(files);
        for entry in &recipe(name).bin {
            let bin = entry.rsplit('/').next().unwrap().to_string();
            said.push((bin, version.clone()));
        }
    }
    let own = b"#!/bin/sh\necho \"fx-tool 9.8.7 (synthetic)\"\n".to_vec();
    served.insert("https://fx.example/latest.txt".into(), b"v9.8.7\n".to_vec());
    let own_url = "https://fx.example/v9.8.7/fx-tool-amd64";
    served.insert(format!("{own_url}.sha256"), sha256_hex(&own).into_bytes());
    served.insert(own_url.into(), own);
    said.push(("fx-tool".into(), "9.8.7".into()));
    let server = Server::start(served);
    let rewrite = server.rewrite();

    let names: Vec<&str> = std::iter::once(CATALOGUE)
        .chain(BATCH)
        .chain(["fx-tool"])
        .collect();
    let tools: String = names
        .iter()
        .map(|n| format!("{n} = \"latest\"\n"))
        .collect();
    let host = format!("[host]\ndistro = \"{distro}\"\n");
    let repo = repository(
        case,
        &host,
        &format!("[home]\nversion = \"1\"\n\n[tools]\n{tools}"),
    );
    fs::create_dir_all(repo.join("recipes")).unwrap();
    fs::write(repo.join("recipes/fx-tool.toml"), USER_RECIPE).unwrap();
    let source = repo.display().to_string();

    // The home's tools, applied through the host folder, each on the user's PATH and running.
    let applied = top(
        case,
        "apply",
        &[&source],
        &[("LODI_FETCH_REWRITE", &rewrite)],
    );
    ok(&applied);
    let home = case.root.path("people/sample");
    let bins = home.join(".local/share/lodi/home-scope/profile/bin");
    let uid = ids(&case.root).0;
    for (bin, version) in &said {
        let run = Command::new(bins.join(bin)).output().unwrap();
        assert_eq!(
            String::from_utf8_lossy(&run.stdout),
            format!("{bin} {version} (synthetic)\n"),
            "{bin}"
        );
        let target = fs::canonicalize(bins.join(bin)).unwrap();
        assert_eq!(fs::metadata(&target).unwrap().uid(), uid, "{bin}");
    }

    // A project of the user's: lock, then a task that runs every tool from the lock.
    let project = home.join("project");
    fs::create_dir_all(&project).unwrap();
    let task: Vec<String> = said.iter().map(|(bin, _)| bin.clone()).collect();
    fs::write(
        project.join("lodi.toml"),
        format!(
            "[project]\nname = \"fx\"\n\n[tools]\n{tools}\n[tasks.every]\nrun = \"{}\"\n",
            task.join(" && ")
        ),
    )
    .unwrap();
    let envs = [
        ("LODI_FETCH_REWRITE", rewrite.as_str()),
        ("LODI_REPO", source.as_str()),
        ("LODI_TRUST", "1"),
    ];
    ok(&as_user(case, &project, &["lock"], &envs));
    let ran = as_user(case, &project, &["run", "every"], &envs);
    ok(&ran);
    let expected: String = said
        .iter()
        .map(|(bin, version)| format!("{bin} {version} (synthetic)\n"))
        .collect();
    assert_eq!(out(&ran), expected, "{}", story(&ran));
    (fs::read(project.join("lodi.lock")).unwrap(), out(&ran))
}

#[test]
fn recipes_lock_realize_and_run_on_fedora() {
    let fedora = machine_box("fx-recipes-fedora", Machine::fedora());
    let debian = machine_box("fx-recipes-debian", Machine::debian());
    let on_fedora = recipes_story(&fedora, "fedora");
    let on_debian = recipes_story(&debian, "debian");
    assert_eq!(
        on_fedora, on_debian,
        "Fedora's lock or tools differ from Debian's"
    );
}
