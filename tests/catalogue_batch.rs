//! The recipe batch (rb-1): eighteen more built-in recipes, thirty in all, as data only.
//!
//! Offline and deterministic throughout. The upstream metadata is the **recorded real payload**
//! under `tests/fixtures/catalogue/` — a GitHub releases slice per `github_releases` recipe and,
//! for the two `text_index` recipes, the discovery document and the `.sha256` sidecar — served by
//! a loopback server the binary reaches through `LODI_FETCH_REWRITE`. Artifacts are synthetic,
//! because upstream's bytes cannot be reproduced offline; where a test realizes one, the digest
//! the server hands out is that artifact's own. The expected pins are read out of the fixtures
//! here, independently of the resolver. The live pass is `sh scripts/mrecipes-local.sh --case
//! batch`, and the guest row `recipes-batch-every-tool-locks-realizes-and-runs`.
//!
//! `locks_from_before_the_batch_stay_byte_identical` reads the jq and python projects under
//! `tests/fixtures/catalogue/batch/before/`, whose `lodi.lock` the binary **before** the batch
//! wrote; `LODI_RECORD_EXPECTED=1 LODI_BATCH_BEFORE_BINARY=<that lodi>` re-records them.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use lodi::catalogue::{BUILTIN_TOOLS, ChecksumSource, Recipe, arch_vars, parse_recipe, render};
use lodi::upstream::strategy::Discovery;
use lodi::util::sha256_hex;
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");

/// The twelve built-ins before the batch.
const OLD: [&str; 12] = [
    "fd",
    "go",
    "hyperfine",
    "jdk",
    "jq",
    "just",
    "nodejs",
    "python",
    "ripgrep",
    "rust",
    "shellcheck",
    "uv",
];

/// The eighteen the owner approved on 2026-09-25, in the owner's table order.
const NEW: [&str; 18] = [
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

// ---------------------------------------------------------------------------------------------
// The recipes and what their recorded fixtures say, read independently of the resolver.

fn recipe(name: &str) -> Recipe {
    let file = format!("{name}.toml");
    let (_, text) = BUILTIN_TOOLS
        .iter()
        .find(|(f, _)| *f == file)
        .unwrap_or_else(|| panic!("{file} is not a built-in recipe"));
    parse_recipe(text, &file).unwrap_or_else(|e| panic!("{file}: {}", e.message))
}

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalogue")
}

fn api_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases?per_page=30")
}

fn recorded(repo: &str) -> serde_json::Value {
    let path = fixtures()
        .join("github_releases")
        .join(format!("{}.json", repo.replace('/', "--")));
    let text = fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap()
}

fn leaf(url: &str) -> &str {
    url.rsplit('/').next().unwrap()
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

/// What `lodi lock` must pin for `name` at `latest`, from the fixture alone.
struct Expected {
    version: String,
    url: String,
    sha256: String,
    /// Every URL the fixture server must answer for the lock: the discovery document and, for a
    /// sidecar recipe, the sidecar.
    files: BTreeMap<String, Vec<u8>>,
}

fn expected(name: &str) -> Expected {
    let recipe = recipe(name);
    match &recipe.versions {
        Discovery::GithubReleases {
            repo,
            releases,
            tag,
            asset,
            ..
        } => {
            let payload = recorded(repo);
            let (before, after) = tag.split_once("${version}").unwrap();
            let mut best: Option<(Vec<u64>, String, serde_json::Value)> = None;
            for release in payload.as_array().unwrap().iter().take(*releases as usize) {
                let name = release["tag_name"].as_str().unwrap();
                if release["draft"].as_bool().unwrap() || release["prerelease"].as_bool().unwrap() {
                    continue;
                }
                let Some(middle) = name
                    .strip_prefix(before)
                    .and_then(|rest| rest.strip_suffix(after))
                else {
                    continue;
                };
                if !middle.split('.').all(|p| p.parse::<u64>().is_ok()) {
                    continue;
                }
                if best.as_ref().is_none_or(|(v, _, _)| numeric(middle) > *v) {
                    best = Some((numeric(middle), middle.to_string(), release.clone()));
                }
            }
            let (_, version, release) = best.unwrap_or_else(|| panic!("{name}: no release"));
            let tag = release["tag_name"].as_str().unwrap();
            let file = rendered(&recipe, asset, &version, tag);
            let hit = release["assets"]
                .as_array()
                .unwrap()
                .iter()
                .find(|a| a["name"].as_str() == Some(file.as_str()))
                .unwrap_or_else(|| panic!("{name}: {file} is not in the recorded release"));
            Expected {
                version,
                url: hit["browser_download_url"].as_str().unwrap().to_string(),
                sha256: hit["digest"].as_str().unwrap().to_string(),
                files: BTreeMap::from([(api_url(repo), serde_json::to_vec(&payload).unwrap())]),
            }
        }
        Discovery::TextIndex { url, line } => {
            let index_url = rendered(&recipe, url, "", "");
            let index = fs::read(fixtures().join(name).join(leaf(&index_url))).unwrap();
            let (before, after) = line.split_once("${version}").unwrap();
            let text = String::from_utf8(index.clone()).unwrap();
            let version = text
                .lines()
                .find_map(|l| l.strip_prefix(before)?.strip_suffix(after))
                .unwrap_or_else(|| panic!("{name}: the recorded index names no version"))
                .trim()
                .to_string();
            let asset = &recipe.assets[0];
            let ChecksumSource::Sidecar(suffix) = &asset.checksum else {
                panic!("{name}: a text_index recipe of the batch takes a sidecar digest");
            };
            let asset_url = rendered(&recipe, asset.url.as_deref().unwrap(), &version, "");
            let sidecar_url = format!("{asset_url}{suffix}");
            let sidecar = fs::read(fixtures().join(name).join(leaf(&sidecar_url))).unwrap();
            let digest = String::from_utf8(sidecar.clone()).unwrap();
            let digest = digest.split_whitespace().next().unwrap().to_string();
            Expected {
                version,
                url: asset_url,
                sha256: format!("sha256:{digest}"),
                files: BTreeMap::from([(index_url, index), (sidecar_url, sidecar)]),
            }
        }
        other => panic!("{name}: the batch uses no {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------------
// Synthetic artifacts, and the fixture as it must be served for one to verify.

/// An artifact shaped the way `recipe` says upstream's is: the bin entries as scripts printing
/// their version, behind as many top directories as the recipe strips.
fn artifact(recipe: &Recipe, version: &str) -> Vec<u8> {
    let asset = &recipe.assets[0];
    if asset.format == "binary" {
        return format!(
            "#!/bin/sh\necho \"{} {version} (synthetic)\"\n",
            recipe.name
        )
        .into_bytes();
    }
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
        members.push(file(
            &format!("{prefix}{entry}"),
            &format!("#!/bin/sh\necho \"{name} {version} (synthetic)\"\n"),
            0o755,
        ));
    }
    members.push(file(&format!("{prefix}LICENSE"), "synthetic\n", 0o644));
    assert_eq!(asset.format, "tar.gz", "{}", recipe.file);
    gzip(&tar(&members))
}

/// Everything `name`'s upstream must answer for `lodi develop` to realize `bytes`: the fixture
/// with the chosen asset's digest (or sidecar) replaced by the synthetic artifact's own.
fn serving(name: &str, bytes: &[u8]) -> BTreeMap<String, Vec<u8>> {
    let want = expected(name);
    let hex = sha256_hex(bytes);
    let mut files = BTreeMap::new();
    for (url, body) in want.files {
        let body = if url.starts_with("https://api.github.com/") {
            let mut releases: serde_json::Value = serde_json::from_slice(&body).unwrap();
            for release in releases.as_array_mut().unwrap() {
                for asset in release["assets"].as_array_mut().unwrap() {
                    if asset["browser_download_url"].as_str() == Some(want.url.as_str()) {
                        asset["digest"] = format!("sha256:{hex}").into();
                        asset["size"] = bytes.len().into();
                    }
                }
            }
            serde_json::to_vec(&releases).unwrap()
        } else if url == format!("{}.sha256", want.url) {
            format!("{hex}\n").into_bytes()
        } else {
            body
        };
        files.insert(url, body);
    }
    files.insert(want.url, bytes.to_vec());
    files
}

// ---------------------------------------------------------------------------------------------
// One project user of the binary, with its own store, config and home under the target dir.

struct Project {
    base: PathBuf,
    server: Server,
}

impl Project {
    fn new(name: &str, manifest: &str, files: BTreeMap<String, Vec<u8>>) -> Project {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home", "project"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        fs::write(base.join("project/lodi.toml"), manifest).unwrap();
        Project {
            base,
            server: Server::start(files),
        }
    }

    fn dir(&self) -> PathBuf {
        self.base.join("project")
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(LODI.as_ref(), args)
    }

    fn run_with(&self, binary: &Path, args: &[&str]) -> Output {
        Command::new(binary)
            .args(args)
            .current_dir(self.dir())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.base.join("lodi-home"))
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            .env("LODI_TRUST", "1")
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap()
    }

    fn lock_bytes(&self) -> Vec<u8> {
        fs::read(self.dir().join("lodi.lock")).unwrap()
    }

    fn lock(&self) -> serde_json::Value {
        serde_json::from_slice(&self.lock_bytes()).unwrap()
    }

    /// The published store entries, by name.
    fn entries(&self) -> Vec<String> {
        let store = self.base.join("lodi-home/store");
        let mut names: Vec<String> = fs::read_dir(&store)
            .map(|dir| {
                dir.flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .filter(|n| n.starts_with("art-") || n.starts_with("env-"))
                    .collect()
            })
            .unwrap_or_default();
        names.sort();
        names
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn tools_manifest(names: &[&str]) -> String {
    let tools: String = names
        .iter()
        .map(|n| format!("{n} = \"latest\"\n"))
        .collect();
    format!("[project]\nname = \"batch\"\n\n[tools]\n{tools}")
}

// ---------------------------------------------------------------------------------------------
// 1. `lodi home apply` installs gh from its built-in recipe, for an ordinary user

#[test]
fn home_apply_installs_gh_from_its_built_in_recipe() {
    let env = home_env("batch-home-gh");
    let gh = recipe("gh");
    let want = expected("gh");
    let bytes = artifact(&gh, &want.version);
    let server = Server::start(serving("gh", &bytes));
    let config = env.config().join("lodi");
    fs::create_dir_all(&config).unwrap();
    fs::write(
        config.join("home.toml"),
        "[home]\nversion = \"1\"\n\n[tools]\ngh = \"latest\"\n",
    )
    .unwrap();

    let apply = env
        .command()
        .env_remove("LODI_HOME")
        .env("XDG_DATA_HOME", env.home().join(".local/share"))
        .env("LODI_FETCH_REWRITE", server.rewrite())
        .args(["home", "apply"])
        .output()
        .unwrap();
    assert_eq!(apply.status.code(), Some(0), "{}", err(&apply));

    // One question to the API and one download of the pinned asset, checked against its digest.
    assert_eq!(
        server.count(&api_url("cli/cli")),
        1,
        "{:?}",
        server.requests()
    );
    assert_eq!(server.count(&want.url), 1, "{:?}", server.requests());
    let lock: serde_json::Value =
        serde_json::from_slice(&fs::read(config.join(lodi::lock::HOME_LOCK_FILE)).unwrap())
            .unwrap();
    let text = lock.to_string();
    assert!(text.contains(&want.url), "{text}");
    assert!(
        text.contains(&format!("sha256:{}", sha256_hex(&bytes))),
        "{text}"
    );

    // Linked into the user's own home, owned by the user who ran it, and it runs.
    let link = env
        .home()
        .join(".local/share/lodi")
        .join(lodi::home::profile::PROFILE_BIN)
        .join("gh");
    let meta = fs::symlink_metadata(&link).unwrap();
    assert!(meta.file_type().is_symlink(), "{}", link.display());
    let target = fs::canonicalize(&link).unwrap();
    let home = fs::canonicalize(env.home()).unwrap();
    assert!(target.starts_with(&home), "{}", target.display());
    assert_eq!(
        fs::metadata(&target).unwrap().uid(),
        fs::metadata(env.home()).unwrap().uid()
    );
    let version = Command::new(&link).arg("--version").output().unwrap();
    assert_eq!(
        out(&version),
        format!("gh {} (synthetic)\n", want.version),
        "gh --version"
    );
}

// ---------------------------------------------------------------------------------------------
// 2. `lodi search` lists all thirty built-ins, offline

#[test]
fn search_lists_the_thirty_built_in_tools_offline() {
    let mut all: Vec<&str> = OLD.iter().chain(NEW.iter()).copied().collect();
    all.sort();
    assert_eq!(lodi::catalogue::builtin_tool_names(), all);

    let project = Project::new("batch-search", &tools_manifest(&[]), BTreeMap::new());
    let closed = ClosedPort::bind();
    for name in &all {
        let recipe = recipe(name);
        for rewrite in [closed.rewrite(), project.server.rewrite()] {
            let o = Command::new(LODI)
                .args(["search", name])
                .current_dir(project.dir())
                .env_clear()
                .env("PATH", "")
                .env("LODI_HOME", project.base.join("lodi-home"))
                .env("LODI_FETCH_REWRITE", rewrite)
                .output()
                .unwrap();
            assert_eq!(o.status.code(), Some(0), "{name}: {}", err(&o));
            let text = out(&o);
            assert!(text.starts_with("catalogue\n"), "{name}: {text}");
            let row = text
                .lines()
                .find(|l| l.split_whitespace().next() == Some(name))
                .unwrap_or_else(|| panic!("{name} is not listed: {text}"));
            assert!(row.contains(&recipe.description), "{name}: {row}");
            assert!(row.contains(&recipe.homepage), "{name}: {row}");
            for listed in text.lines().skip(1).filter(|l| l.starts_with("  ")) {
                let listed = listed.split_whitespace().next().unwrap();
                assert!(all.contains(&listed), "{name}: {listed} is not a built-in");
            }
        }
    }
    assert!(
        project.server.requests().is_empty(),
        "search asked the network"
    );
}

// ---------------------------------------------------------------------------------------------
// 3. `lodi lock` pins each new tool from its recorded fixture

#[test]
fn each_new_recipe_locks_from_its_recorded_fixture() {
    let mut files = BTreeMap::new();
    let mut want = BTreeMap::new();
    for name in NEW {
        let e = expected(name);
        files.extend(e.files.clone());
        want.insert(name, e);
    }
    let project = Project::new("batch-lock", &tools_manifest(&NEW), files);
    let o = project.run(&["lock"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));

    let lock = project.lock();
    for name in NEW {
        let e = &want[name];
        let entry = &lock["packages"][name];
        assert_eq!(entry["version"], e.version.as_str(), "{name}");
        assert_eq!(entry["artifacts"][0]["url"], e.url.as_str(), "{name}");
        assert_eq!(entry["artifacts"][0]["sha256"], e.sha256.as_str(), "{name}");
        assert_eq!(entry["recipe"]["input"], "builtin", "{name}");
        assert_eq!(entry["artifacts"].as_array().unwrap().len(), 1, "{name}");
    }
    // One digest source each: sixteen API pages and two index-and-sidecar pairs, and not one
    // artifact downloaded to lock.
    let requests = project.server.requests();
    let api = requests
        .iter()
        .filter(|u| u.starts_with("https://api.github.com/"));
    assert_eq!(api.count(), 16, "{requests:?}");
    assert_eq!(requests.len(), 16 + 4, "{requests:?}");
    for e in want.values() {
        assert!(!requests.contains(&e.url), "{} was downloaded", e.url);
    }

    // neovim's newest releases carry the moving `stable` and `nightly` tags; neither is pinned.
    let Discovery::GithubReleases { repo, releases, .. } = &recipe("neovim").versions else {
        panic!("neovim is a github_releases recipe");
    };
    assert_eq!(*releases, 5);
    let tags: Vec<String> = recorded(repo)
        .as_array()
        .unwrap()
        .iter()
        .take(5)
        .map(|r| r["tag_name"].as_str().unwrap().to_string())
        .collect();
    assert!(tags.contains(&"stable".to_string()), "{tags:?}");
    assert!(tags.contains(&"nightly".to_string()), "{tags:?}");
    let neovim = &lock["packages"]["neovim"];
    assert_eq!(
        neovim["tag"],
        format!("v{}", neovim["version"].as_str().unwrap())
    );
}

// ---------------------------------------------------------------------------------------------
// 4. A missing asset or digest, or a wrong sidecar digest, publishes nothing

#[test]
fn a_missing_or_wrong_digest_publishes_nothing() {
    // helm is locked and realized first; each refusal below must leave it as it was.
    let helm = recipe("helm");
    let helm_bytes = artifact(&helm, &expected("helm").version);
    let kubectl = recipe("kubectl");
    let kubectl_bytes = artifact(&kubectl, &expected("kubectl").version);
    let gh = expected("gh");
    let zoxide = expected("zoxide");

    // gh's releases without this architecture's asset, and again with it but with no digest.
    let asset_name = leaf(&gh.url).replace(&gh.version, "");
    let mut no_asset: serde_json::Value = recorded("cli/cli");
    let mut no_digest: serde_json::Value = recorded("cli/cli");
    for (a, b) in no_asset
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .zip(no_digest.as_array_mut().unwrap())
    {
        a["assets"].as_array_mut().unwrap().retain(|x| {
            x["name"]
                .as_str()
                .unwrap()
                .replace(|c: char| c.is_ascii_digit() || c == '.', "")
                != asset_name.replace(|c: char| c.is_ascii_digit() || c == '.', "")
        });
        for asset in b["assets"].as_array_mut().unwrap() {
            asset.as_object_mut().unwrap().remove("digest");
        }
    }

    let mut files = serving("helm", &helm_bytes);
    files.extend(zoxide.files.clone());
    // kubectl's recorded sidecar is the real binary's digest, not the synthetic one's.
    files.extend(expected("kubectl").files);
    files.insert(expected("kubectl").url, kubectl_bytes);
    let project = Project::new("batch-refusals", &tools_manifest(&["helm"]), files);
    let o = project.run(&["lock"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let o = project.run(&["develop", "--", "helm", "version"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let lock_before = project.lock_bytes();
    let entries_before = project.entries();
    assert_eq!(
        entries_before
            .iter()
            .filter(|n| n.starts_with("art-"))
            .count(),
        1
    );

    let refused = |manifest: &str, code: &str, api: Option<(&str, &serde_json::Value)>| {
        if let Some((url, body)) = api {
            let body = serde_json::to_vec(body).unwrap();
            project
                .server
                .files
                .lock()
                .unwrap()
                .insert(url.to_string(), body);
        }
        fs::write(project.dir().join("lodi.toml"), manifest).unwrap();
        project.server.clear();
        // The lock refuses the tool; develop, which never resolves, then refuses the stale lock.
        for (args, code) in [
            (&["lock"][..], code),
            (&["develop", "--", "helm", "version"], "E_LOCK_STALE"),
        ] {
            let o = project.run(args);
            assert_ne!(o.status.code(), Some(0), "{args:?} {manifest}");
            assert!(err(&o).contains(code), "{args:?} {code}: {}", err(&o));
        }
        assert_eq!(project.lock_bytes(), lock_before, "the lock changed");
        assert_eq!(project.entries(), entries_before, "the store changed");
        let requests = project.server.requests();
        assert!(
            requests.iter().all(|u| !u.contains("/releases/download/")),
            "an artifact was downloaded: {requests:?}"
        );
    };
    let with = |extra: &str| format!("{}{extra}", tools_manifest(&["helm"]));
    refused(
        &with("gh = \"latest\"\n"),
        "E_UNSUPPORTED_ARCH",
        Some((&api_url("cli/cli"), &no_asset)),
    );
    refused(
        &with("gh = \"latest\"\n"),
        "E_NO_CHECKSUM",
        Some((&api_url("cli/cli"), &no_digest)),
    );
    // Recorded, not crafted: a release of zoxide the recipe reads whose asset has no digest.
    let older = recorded("ajeetdsouza/zoxide")
        .as_array()
        .unwrap()
        .iter()
        .take(3)
        .find(|r| {
            r["assets"].as_array().unwrap().iter().any(|a| {
                a["name"]
                    .as_str()
                    .unwrap()
                    .ends_with("-x86_64-unknown-linux-musl.tar.gz")
                    && a.get("digest").is_none()
            })
        })
        .map(|r| {
            r["tag_name"]
                .as_str()
                .unwrap()
                .trim_start_matches('v')
                .to_string()
        })
        .expect("the recorded zoxide slice keeps a release published without a digest");
    assert_ne!(older, zoxide.version);
    refused(
        &with(&format!("zoxide = \"={older}\"\n")),
        "E_NO_CHECKSUM",
        None,
    );

    // A sidecar that disagrees with the bytes: the lock pins it, realization refuses it.
    fs::write(
        project.dir().join("lodi.toml"),
        with("kubectl = \"latest\"\n"),
    )
    .unwrap();
    let o = project.run(&["lock"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let locked = project.lock();
    assert_eq!(
        locked["packages"]["helm"],
        serde_json::from_slice::<serde_json::Value>(&lock_before).unwrap()["packages"]["helm"]
    );
    let o = project.run(&["develop", "--", "kubectl", "version", "--client"]);
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(err(&o).contains("E_HASH_MISMATCH"), "{}", err(&o));
    assert_eq!(
        project.entries(),
        entries_before,
        "a refused artifact was published"
    );
}

// ---------------------------------------------------------------------------------------------
// 5. The jq and python locks written before the batch stay byte-identical and realize

/// python's upstream as the tests of `lodi shell` serve it: one release, its checksum file and
/// the synthetic archive.
fn python_upstream() -> BTreeMap<String, Vec<u8>> {
    const PYTHON: &str = "3.12.14";
    const TAG: &str = "20260901";
    let bytes = python_archive(PYTHON);
    let hex = sha256_hex(&bytes);
    let name = format!("cpython-{PYTHON}+{TAG}-x86_64-unknown-linux-gnu-install_only.tar.gz");
    let base = "https://github.com/astral-sh/python-build-standalone/releases/download";
    let url = format!("{base}/{TAG}/{name}");
    let releases = serde_json::json!([{
        "tag_name": TAG,
        "draft": false,
        "prerelease": false,
        "assets": [
            { "name": name, "size": bytes.len(), "digest": format!("sha256:{hex}"),
              "browser_download_url": url },
            { "name": "SHA256SUMS", "size": 64,
              "browser_download_url": format!("{base}/{TAG}/SHA256SUMS") },
        ],
    }]);
    BTreeMap::from([
        (
            api_url("astral-sh/python-build-standalone").replace("per_page=30", "per_page=2"),
            serde_json::to_vec(&releases).unwrap(),
        ),
        (
            format!("{base}/{TAG}/SHA256SUMS"),
            format!("{hex}  {name}\n").into_bytes(),
        ),
        (url, bytes),
    ])
}

#[test]
fn locks_from_before_the_batch_stay_byte_identical() {
    let jq = recipe("jq");
    let jq_version = expected("jq").version;
    let cases = [
        (
            "jq",
            serving("jq", &artifact(&jq, &jq_version)),
            vec!["jq", "--version"],
            format!("jq {jq_version} (synthetic)\n"),
        ),
        (
            "python",
            python_upstream(),
            vec!["python3", "--version"],
            "Python 3.12.14 (synthetic)\n".to_string(),
        ),
    ];
    for (name, files, command, printed) in cases {
        let fixture = fixtures().join("batch/before").join(name);
        let manifest = fs::read_to_string(fixture.join("lodi.toml")).unwrap();
        let project = Project::new(&format!("batch-before-{name}"), &manifest, files);
        if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
            let before = std::env::var_os("LODI_BATCH_BEFORE_BINARY")
                .expect("recording needs LODI_BATCH_BEFORE_BINARY, the binary before the batch");
            let o = project.run_with(Path::new(&before), &["lock"]);
            assert_eq!(o.status.code(), Some(0), "{name}: {}", err(&o));
            fs::copy(project.dir().join("lodi.lock"), fixture.join("lodi.lock")).unwrap();
        }
        let recorded = fs::read(fixture.join("lodi.lock")).unwrap();
        fs::write(project.dir().join("lodi.lock"), &recorded).unwrap();

        let check = project.run(&["lock", "--check"]);
        assert_eq!(check.status.code(), Some(0), "{name}: {}", err(&check));
        let mut args = vec!["develop", "--"];
        args.extend(&command);
        let ran = project.run(&args);
        assert_eq!(ran.status.code(), Some(0), "{name}: {}", err(&ran));
        assert_eq!(out(&ran), printed, "{name}");
        assert_eq!(
            project.lock_bytes(),
            recorded,
            "{name}: the lock was rewritten"
        );
        let entries = project.entries();
        assert_eq!(entries.iter().filter(|n| n.starts_with("art-")).count(), 1);
    }
}
