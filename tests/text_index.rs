//! M-0.4 T-4's package check: the `text_index` strategy, index-supplied digests, and the
//! multi-artifact recipes that need them.
//!
//! Offline and deterministic throughout. The discovery documents are the **recorded real
//! payloads** under `tests/fixtures/catalogue/` (see each directory's `README.md`), served by a
//! loopback server the binary and the resolver reach through `LODI_FETCH_REWRITE`; the
//! artifacts are synthetic archives built here, because the bytes upstream publishes cannot be
//! reproduced offline (and cannot even be executed on every host this suite runs on), and the
//! digests the fixture server hands out for them — in the index for one recipe, in a sidecar
//! for the other — are those archives' own. Those synthetic archives do **not** collide with
//! one another; the real component tarballs of the multi-artifact recipe do, and the test that
//! states what happens then is `two_artifacts_that_write_the_same_path_are_refused` below.
//!
//! What is checked here is acceptance (a)…(f) of the package: a recipe whose digest comes from
//! its index resolves from the recorded slice, pins that digest and realizes; a recipe
//! discovered from a plain-text channel manifest pins **every** one of its artifacts against
//! its own sidecar and realizes all of them into **one** store entry whose tree hash is the
//! same in a second, independent store; an index digest that disagrees with the bytes is
//! `E_HASH_MISMATCH` and nothing is published; a sidecar upstream does not publish is
//! `E_NO_CHECKSUM`; a constraint the channel does not satisfy is `E_NO_MATCH` naming the
//! version that does exist; and the recipes this package did not touch still declare one
//! artifact each. Acceptance (g), the one live resolution, is
//! `sh scripts/m04-local.sh --case toolchains`.
//!
//! No test here names a recipe: the recipes under test are the ones the catalogue itself says
//! use these mechanisms.

mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use lodi::catalogue::{BUILTIN_TOOLS, ChecksumSource, Recipe, arch_vars, parse_recipe, render};
use lodi::diag::exit_status;
use lodi::fetch::{Fetcher, HttpFetcher, parse_rewrites};
use lodi::upstream::strategy::{Context, Discovery, discoverer};
use lodi::upstream::{LockedTool, resolve_tool};
use lodi::version::Constraint;
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");

// ---------------------------------------------------------------------------------------------
// The recipes under test, read from the catalogue itself.

fn parsed() -> Vec<Recipe> {
    BUILTIN_TOOLS
        .iter()
        .map(|(file, text)| {
            parse_recipe(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message))
        })
        .collect()
}

/// The recipe whose versions come from a plain-text document and which this package gave
/// several artifacts and sidecar digests. The recipe batch's one-artifact `text_index` recipes
/// are `tests/catalogue_batch.rs`'s.
fn text_indexed() -> Recipe {
    parsed()
        .into_iter()
        .find(|r| matches!(r.versions, Discovery::TextIndex { .. }) && r.assets.len() > 1)
        .expect("a recipe whose [versions] strategy is text_index, with several artifacts")
}

/// The recipe that takes its artifact's digest from the index that lists the file.
fn index_digested() -> Recipe {
    parsed()
        .into_iter()
        .find(|r| {
            r.assets
                .iter()
                .any(|a| matches!(a.checksum, ChecksumSource::Index(_)))
        })
        .expect("a recipe whose [asset] checksum = \"index\"")
}

/// Both of them, in a stable order.
fn under_test() -> Vec<Recipe> {
    vec![index_digested(), text_indexed()]
}

fn vars_of(recipe: &Recipe) -> Vec<(String, String)> {
    arch_vars(&recipe.arch_names, "x86_64")
}

fn refs(vars: &[(String, String)]) -> Vec<(&str, &str)> {
    vars.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect()
}

/// The discovery document's URL, as the recipe asks for it.
fn index_url(recipe: &Recipe) -> String {
    let template = match &recipe.versions {
        Discovery::TextIndex { url, .. } | Discovery::JsonIndex { url, .. } => url,
        other => panic!("{}: {other:?} is not an index strategy", recipe.file),
    };
    let vars = vars_of(recipe);
    render(template, &refs(&vars)).unwrap_or_else(|e| panic!("{}: {e}", recipe.file))
}

/// The committed slice of that document, under the name `m04probe.py --record` writes.
fn index_bytes(recipe: &Recipe) -> Vec<u8> {
    let leaf = match &recipe.versions {
        Discovery::JsonIndex { .. } => "index.json".to_string(),
        _ => index_url(recipe)
            .rsplit('/')
            .next()
            .unwrap()
            .split('?')
            .next()
            .unwrap()
            .to_string(),
    };
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/catalogue")
        .join(&recipe.name)
        .join(&leaf);
    std::fs::read(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

// ---------------------------------------------------------------------------------------------
// Synthetic artifacts, shaped by the recipe under test.

/// The version the recorded document offers for this architecture, read through the strategy
/// itself: the artifact URLs cannot be rendered before it is known.
fn newest_version(recipe: &Recipe) -> String {
    let server = Server::start(BTreeMap::from([(index_url(recipe), index_bytes(recipe))]));
    let fetcher = HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap());
    let vars = vars_of(recipe);
    let var_refs = refs(&vars);
    let constraint = Constraint::parse("latest").unwrap();
    let ctx = Context {
        file: &recipe.file,
        vars: &var_refs,
        constraint_text: "latest",
        constraint: &constraint,
    };
    let discover = discoverer(recipe.strategy).expect("a registered strategy");
    let candidates = discover(&fetcher as &dyn Fetcher, &recipe.versions, &ctx)
        .unwrap_or_else(|e| panic!("{}: {} {}", recipe.file, e.code, e.message));
    candidates
        .into_iter()
        .filter(|c| c.for_arch && c.version.pre.is_none())
        .max_by(|a, b| a.version.cmp(&b.version))
        .unwrap_or_else(|| panic!("{}: the recorded document lists no version", recipe.file))
        .text
}

/// The artifact URLs this recipe pins for `version`, in declaration order.
fn asset_urls(recipe: &Recipe, version: &str) -> Vec<String> {
    let vars = vars_of(recipe);
    let mut all = refs(&vars);
    let (major, minor) = {
        let mut parts = version.split('.');
        (
            parts.next().unwrap_or_default().to_string(),
            parts.next().unwrap_or_default().to_string(),
        )
    };
    all.push(("version", version));
    all.push(("version_major", &major));
    all.push(("version_minor", &minor));
    recipe
        .assets
        .iter()
        .map(|a| {
            let template = a.url.as_deref().expect("these recipes render their URLs");
            render(template, &all).unwrap_or_else(|e| panic!("{}: {e}", recipe.file))
        })
        .collect()
}

/// An archive shaped the way the recipe says the real one is: the `[spec] bin` entries shared
/// out over the recipe's artifacts as tiny executables that print their version, one data file
/// per artifact so that each one's contribution to the merged tree is visible, and as many
/// levels of top directory as the recipe strips.
fn artifact(
    recipe: &Recipe,
    nth: usize,
    version: &str,
    shared: &[&str],
    include_excluded: bool,
    excluded_as_directory: bool,
) -> Vec<u8> {
    let asset = &recipe.assets[nth];
    let mut prefix = String::new();
    let mut members = Vec::new();
    for level in 0..asset.strip_components {
        prefix.push_str(&format!("top{level}/"));
        members.push(dir(&prefix));
    }
    let mut made_bin = false;
    for (i, entry) in recipe.bin.iter().enumerate() {
        if i % recipe.assets.len() != nth {
            continue;
        }
        if let Some((parent, _)) = entry.rsplit_once('/') {
            members.push(dir(&format!("{prefix}{parent}/")));
        }
        let name = entry.rsplit('/').next().unwrap();
        members.push(file(
            &format!("{prefix}{entry}"),
            &format!("#!/bin/sh\necho \"{name} {version} (synthetic)\"\n"),
            0o755,
        ));
        made_bin = true;
    }
    // Every one of these upstreams ships data beside the binaries; it must not reach PATH.
    members.push(dir(&format!("{prefix}share/")));
    members.push(file(
        &format!("{prefix}share/artifact-{nth}.txt"),
        &format!("synthetic artifact {nth} of {} {version}\n", recipe.name),
        0o644,
    ));
    // The metadata files every artifact of a family ships under the same name, when the caller
    // asks for them: what upstream's own component tarballs look like.
    for name in asset
        .exclude
        .iter()
        .map(String::as_str)
        .filter(|_| include_excluded)
        .chain(shared.iter().copied())
    {
        if excluded_as_directory && asset.exclude.iter().any(|path| path == name) {
            members.push(dir(&format!("{prefix}{name}/")));
            continue;
        }
        members.push(file(
            &format!("{prefix}{name}"),
            &format!("artifact {nth} of {} {version}\n", recipe.name),
            0o644,
        ));
    }
    assert!(
        made_bin || recipe.bin.len() < recipe.assets.len(),
        "{}: artifact {nth} carries no binary",
        recipe.file
    );
    let tar = tar(&members);
    match asset.format.as_str() {
        "tar.gz" => gzip(&tar),
        "tar.xz" => xz(&tar),
        other => panic!("no synthetic artifact for format `{other}`"),
    }
}

/// The recorded index with the chosen file's digest replaced by the synthetic archive's own,
/// for the recipe that takes its digest from the index. Any other document is served as it was
/// recorded.
fn serving_index(recipe: &Recipe, urls: &[String], artifacts: &[Vec<u8>]) -> Vec<u8> {
    let Discovery::JsonIndex {
        files_key,
        file_key,
        ..
    } = &recipe.versions
    else {
        return index_bytes(recipe);
    };
    let (Some(files_key), Some(file_key)) = (files_key.as_deref(), file_key.as_deref()) else {
        return index_bytes(recipe);
    };
    let mut index: serde_json::Value = serde_json::from_slice(&index_bytes(recipe)).unwrap();
    for (nth, asset) in recipe.assets.iter().enumerate() {
        let ChecksumSource::Index(key) = &asset.checksum else {
            continue;
        };
        let wanted = urls[nth].rsplit('/').next().unwrap().to_string();
        let hex = lodi::util::sha256_hex(&artifacts[nth]);
        for entry in index.as_array_mut().unwrap() {
            let Some(files) = entry.get_mut(files_key).and_then(|f| f.as_array_mut()) else {
                continue;
            };
            for f in files {
                if f.get(file_key).and_then(|v| v.as_str()) == Some(wanted.as_str()) {
                    f[key.as_str()] = serde_json::Value::String(hex.clone());
                }
            }
        }
    }
    serde_json::to_vec(&index).unwrap()
}

/// Everything one recipe's upstream has to answer for a whole run to succeed: the discovery
/// document, every artifact, and every sidecar digest.
struct Upstream {
    version: String,
    urls: Vec<String>,
    artifacts: Vec<Vec<u8>>,
    files: BTreeMap<String, Vec<u8>>,
}

fn upstream(recipe: &Recipe) -> Upstream {
    upstream_generated(recipe, &[], true, false)
}

/// The same, with every artifact carrying the named files at its root.
fn upstream_with(recipe: &Recipe, shared: &[&str]) -> Upstream {
    upstream_generated(recipe, shared, true, false)
}

fn upstream_without_excluded(recipe: &Recipe) -> Upstream {
    upstream_generated(recipe, &[], false, false)
}

fn upstream_with_excluded_directory(recipe: &Recipe) -> Upstream {
    upstream_generated(recipe, &[], true, true)
}

fn upstream_generated(
    recipe: &Recipe,
    shared: &[&str],
    include_excluded: bool,
    excluded_as_directory: bool,
) -> Upstream {
    let version = newest_version(recipe);
    let urls = asset_urls(recipe, &version);
    let artifacts: Vec<Vec<u8>> = (0..recipe.assets.len())
        .map(|nth| {
            artifact(
                recipe,
                nth,
                &version,
                shared,
                include_excluded,
                excluded_as_directory,
            )
        })
        .collect();
    let mut files = BTreeMap::from([(index_url(recipe), serving_index(recipe, &urls, &artifacts))]);
    for (nth, asset) in recipe.assets.iter().enumerate() {
        files.insert(urls[nth].clone(), artifacts[nth].clone());
        if let ChecksumSource::Sidecar(suffix) = &asset.checksum {
            let hex = lodi::util::sha256_hex(&artifacts[nth]);
            let name = urls[nth].rsplit('/').next().unwrap();
            // Upstream publishes both shapes; the resolver reads both.
            let body = if nth % 2 == 0 {
                format!("{hex}  {name}\n")
            } else {
                format!("{hex}\n")
            };
            files.insert(format!("{}{suffix}", urls[nth]), body.into_bytes());
        }
    }
    Upstream {
        version,
        urls,
        artifacts,
        files,
    }
}

// ---------------------------------------------------------------------------------------------
// One isolated user of the binary: a store, a config directory and a home under the target
// directory, and a loopback server holding one tool's upstream files.

struct User {
    base: PathBuf,
    server: Server,
}

impl User {
    fn new(name: &str, files: BTreeMap<String, Vec<u8>>) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        User {
            base,
            server: Server::start(files),
        }
    }

    fn shell(&self, args: &[&str], script: &str) -> Output {
        let dir = self.base.join("elsewhere");
        std::fs::create_dir_all(&dir).unwrap();
        let mut child = Command::new(LODI)
            .args(args)
            .current_dir(&dir)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.base.join("lodi-home"))
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(script.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    }

    fn store(&self) -> PathBuf {
        self.base.join("lodi-home/store")
    }

    /// The published `art-` entries of this user's store, by name.
    fn entries(&self) -> Vec<String> {
        let mut names = Vec::new();
        let Ok(read) = std::fs::read_dir(self.store()) else {
            return names;
        };
        for entry in read.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with("art-") {
                names.push(name);
            }
        }
        names.sort();
        names
    }

    fn tree_hash(&self, entry: &str) -> String {
        let meta = self.store().join(".meta").join(format!("{entry}.json"));
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(meta).unwrap()).unwrap();
        v["treeHash"].as_str().unwrap().to_string()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// Every `[spec] bin` of the recipe, called the two ways these tools are asked their version.
fn version_script(recipe: &Recipe) -> String {
    recipe
        .bin
        .iter()
        .map(|b| {
            let binary = b.rsplit('/').next().unwrap();
            format!("{binary} version\n{binary} --version\n")
        })
        .collect()
}

fn resolve(
    recipe: &Recipe,
    files: BTreeMap<String, Vec<u8>>,
    constraint: &str,
) -> Result<LockedTool, lodi::diag::Diagnostic> {
    let server = Server::start(files);
    let fetcher = HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap());
    let parsed = if constraint == "latest" {
        Constraint::Latest
    } else {
        Constraint::parse(constraint).unwrap()
    };
    resolve_tool(
        &fetcher,
        recipe,
        &recipe.name,
        constraint,
        &parsed,
        "x86_64",
    )
}

// ---------------------------------------------------------------------------------------------
// (a) and (b) Every recipe resolves from its recorded document and pins one digest per artifact

#[test]
fn every_recipe_resolves_from_its_recorded_document_and_pins_every_artifact() {
    for recipe in under_test() {
        let up = upstream(&recipe);
        let locked = resolve(&recipe, up.files.clone(), "latest")
            .unwrap_or_else(|e| panic!("{}: {} {}", recipe.file, e.code, e.message));

        assert_eq!(locked.version, up.version, "{}", recipe.file);
        assert_eq!(
            locked.artifacts.len(),
            recipe.assets.len(),
            "{}: one pin per [asset]",
            recipe.file
        );
        for (nth, artifact) in locked.artifacts.iter().enumerate() {
            assert_eq!(artifact.url, up.urls[nth], "{}", recipe.file);
            assert!(artifact.url.starts_with("https://"), "{}", artifact.url);
            assert_eq!(
                artifact.sha256,
                lodi::util::sha256_hex(&up.artifacts[nth]),
                "{}: artifact {nth} is not pinned to the bytes upstream serves",
                recipe.file
            );
            assert_eq!(
                artifact.format, recipe.assets[nth].format,
                "{}",
                recipe.file
            );
            assert_eq!(
                artifact.strip_components, recipe.assets[nth].strip_components,
                "{}",
                recipe.file
            );
            assert_eq!(
                artifact.exclude, recipe.assets[nth].exclude,
                "{}",
                recipe.file
            );
        }
    }
}

/// The digest of the index-digest recipe is the one the **recorded** document publishes: the
/// resolver reads it from the index entry it matched, and asks for nothing else.
#[test]
fn the_index_supplied_digest_is_the_one_the_recorded_index_publishes() {
    let recipe = index_digested();
    let url = index_url(&recipe);
    let server = Server::start(BTreeMap::from([(url.clone(), index_bytes(&recipe))]));
    let fetcher = HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap());
    let locked = resolve_tool(
        &fetcher,
        &recipe,
        &recipe.name,
        "latest",
        &Constraint::Latest,
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));

    // One request: the index. No checksum file, no sidecar, no download.
    assert_eq!(server.requests(), vec![url], "{}", recipe.file);

    let ChecksumSource::Index(key) = &recipe.assets[0].checksum else {
        unreachable!()
    };
    let Discovery::JsonIndex {
        files_key,
        file_key,
        ..
    } = &recipe.versions
    else {
        unreachable!()
    };
    let wanted = locked.artifacts[0].url.rsplit('/').next().unwrap();
    let index: serde_json::Value = serde_json::from_slice(&index_bytes(&recipe)).unwrap();
    let recorded = index
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|e| e[files_key.as_deref().unwrap()].as_array().unwrap())
        .find(|f| f[file_key.as_deref().unwrap()].as_str() == Some(wanted))
        .and_then(|f| f[key.as_str()].as_str())
        .unwrap_or_else(|| panic!("{wanted} is not in the recorded index"));
    assert_eq!(recorded, locked.artifacts[0].sha256);
    assert_eq!(recorded.len(), 64);
}

/// Every artifact of the text-indexed recipe is pinned against a sidecar of its own: as many
/// sidecar requests as there are artifacts, each beside its own URL.
#[test]
fn every_artifact_is_pinned_against_its_own_sidecar() {
    let recipe = text_indexed();
    assert!(
        recipe.assets.len() >= 3,
        "{}: this package gives it three artifacts, found {}",
        recipe.file,
        recipe.assets.len()
    );
    let up = upstream(&recipe);
    let server = Server::start(up.files.clone());
    let fetcher = HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap());
    let locked = resolve_tool(
        &fetcher,
        &recipe,
        &recipe.name,
        "latest",
        &Constraint::Latest,
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));

    let mut expected = vec![index_url(&recipe)];
    for (nth, asset) in recipe.assets.iter().enumerate() {
        let ChecksumSource::Sidecar(suffix) = &asset.checksum else {
            panic!("{}: artifact {nth} has no sidecar", recipe.file)
        };
        expected.push(format!("{}{suffix}", up.urls[nth]));
    }
    // The document and one sidecar per artifact, in order, and no artifact downloaded: the
    // resolver pins, it does not fetch.
    assert_eq!(server.requests(), expected, "{}", recipe.file);
    assert_eq!(locked.artifacts.len(), recipe.assets.len());
}

// ---------------------------------------------------------------------------------------------
// (a) and (b) Realization: the artifacts become one store entry and its binaries run

#[test]
fn every_recipe_realizes_into_one_entry_whose_binaries_run() {
    for recipe in under_test() {
        let up = upstream(&recipe);
        let mut seen: Vec<(Vec<String>, String)> = Vec::new();
        for run in 0..2 {
            let user = User::new(
                &format!("text-index-{}-{run}", recipe.name),
                up.files.clone(),
            );
            let o = user.shell(&["shell", &recipe.name], &version_script(&recipe));
            assert!(o.status.success(), "{}: {}", recipe.file, err(&o));
            for entry in &recipe.bin {
                let binary = entry.rsplit('/').next().unwrap();
                assert!(
                    out(&o).contains(&format!("{binary} {} (synthetic)", up.version)),
                    "{}: `{binary}` did not run from the store: {}",
                    recipe.file,
                    out(&o)
                );
            }
            // Several artifacts, one entry.
            let entries = user.entries();
            assert_eq!(
                entries.len(),
                1,
                "{}: {} artifacts became {entries:?}",
                recipe.file,
                recipe.assets.len()
            );
            // Every artifact really contributed to that one tree.
            let tree = user.store().join(&entries[0]);
            for nth in 0..recipe.assets.len() {
                assert!(
                    tree.join(format!("share/artifact-{nth}.txt")).is_file(),
                    "{}: artifact {nth} is not in the entry",
                    recipe.file
                );
            }
            for path in recipe.assets.iter().flat_map(|asset| &asset.exclude) {
                assert!(
                    !tree.join(path).exists(),
                    "{}: excluded path `{path}` was written",
                    recipe.file
                );
            }
            // Only executables reach the environment: the data beside them does not.
            let links = listing(&user.store());
            assert!(
                !links.iter().any(|p| p.contains("/bin/artifact-")),
                "{}: a data file reached PATH: {links:?}",
                recipe.file
            );
            let hash = user.tree_hash(&entries[0]);
            seen.push((entries, hash));
        }
        assert_eq!(
            seen[0], seen[1],
            "{}: the entry name and tree hash are not stable across two runs",
            recipe.file
        );
    }
}

/// Without `exclude`, two artifacts that write the same path are still refused and nothing is
/// published. The narrow recipe feature does not weaken the duplicate-path safety rule.
#[test]
fn two_artifacts_that_write_the_same_path_are_refused() {
    let recipe = text_indexed();
    let up = upstream_with(&recipe, &["collision.txt"]);
    let user = User::new("text-index-collision", up.files.clone());
    let o = user.shell(&["shell", &recipe.name], "true\n");
    assert_eq!(
        o.status.code(),
        Some(exit_status("E_ARCHIVE_UNSAFE") as i32),
        "{}: {}",
        recipe.file,
        err(&o)
    );
    assert!(err(&o).contains("E_ARCHIVE_UNSAFE"), "{}", err(&o));
    assert!(err(&o).contains("collision.txt"), "{}", err(&o));
    assert_eq!(
        user.entries(),
        Vec::<String>::new(),
        "{}: an entry was published from artifacts that collided",
        recipe.file
    );
}

#[test]
fn a_missing_excluded_path_is_a_recipe_error_and_nothing_is_published() {
    let recipe = text_indexed();
    let up = upstream_without_excluded(&recipe);
    let user = User::new("text-index-missing-exclude", up.files);
    let output = user.shell(&["shell", &recipe.name], "true\n");
    assert_eq!(
        output.status.code(),
        Some(exit_status("E_RECIPE_INVALID") as i32),
        "{}",
        err(&output)
    );
    assert!(err(&output).contains(&recipe.file), "{}", err(&output));
    assert!(err(&output).contains("manifest.in"), "{}", err(&output));
    assert_eq!(user.entries(), Vec::<String>::new());
}

#[test]
fn an_excluded_directory_is_a_recipe_error_and_nothing_is_published() {
    let recipe = text_indexed();
    let up = upstream_with_excluded_directory(&recipe);
    let user = User::new("text-index-directory-exclude", up.files);
    let output = user.shell(&["shell", &recipe.name], "true\n");
    assert_eq!(
        output.status.code(),
        Some(exit_status("E_RECIPE_INVALID") as i32),
        "{}",
        err(&output)
    );
    assert!(err(&output).contains(&recipe.file), "{}", err(&output));
    assert!(err(&output).contains("regular file"), "{}", err(&output));
    assert_eq!(user.entries(), Vec::<String>::new());
}

// ---------------------------------------------------------------------------------------------
// (c) An index digest that disagrees with the bytes is E_HASH_MISMATCH (5), nothing published

#[test]
fn an_index_digest_that_disagrees_with_the_bytes_is_a_hash_mismatch() {
    let recipe = index_digested();
    let up = upstream(&recipe);
    // The document as it was recorded: its digest is the real upstream artifact's, and what the
    // server hands out under that URL is the synthetic archive.
    let mut files = up.files.clone();
    files.insert(index_url(&recipe), index_bytes(&recipe));

    let user = User::new("text-index-hash-mismatch", files);
    let o = user.shell(&["shell", &recipe.name], "true\n");
    assert_eq!(
        o.status.code(),
        Some(exit_status("E_HASH_MISMATCH") as i32),
        "{}: {}",
        recipe.file,
        err(&o)
    );
    assert!(err(&o).contains("E_HASH_MISMATCH"), "{}", err(&o));
    assert_eq!(
        user.entries(),
        Vec::<String>::new(),
        "{}: an entry was published from bytes that failed verification",
        recipe.file
    );
}

// ---------------------------------------------------------------------------------------------
// (d) A sidecar upstream does not publish is E_NO_CHECKSUM (5)

#[test]
fn a_missing_sidecar_is_e_no_checksum() {
    let recipe = text_indexed();
    let up = upstream(&recipe);
    // Everything upstream has, except the digest beside the last artifact.
    let last = recipe.assets.len() - 1;
    let ChecksumSource::Sidecar(suffix) = &recipe.assets[last].checksum else {
        unreachable!()
    };
    let mut files = up.files.clone();
    files.remove(&format!("{}{suffix}", up.urls[last]));

    let e = resolve(&recipe, files.clone(), "latest").unwrap_err();
    assert_eq!(e.code, "E_NO_CHECKSUM", "{}", e.message);
    assert_eq!(exit_status(e.code), 5);
    assert!(
        e.message.contains(&format!("{}{suffix}", up.urls[last])),
        "{}",
        e.message
    );

    // And the binary refuses the same way, with nothing downloaded and nothing published.
    let user = User::new("text-index-no-checksum", files);
    let o = user.shell(&["shell", &recipe.name], "true\n");
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(err(&o).contains("E_NO_CHECKSUM"), "{}", err(&o));
    assert_eq!(user.entries(), Vec::<String>::new());
    assert!(
        !user.server.requests().iter().any(|u| u == &up.urls[last]),
        "an artifact was downloaded before its digest was known: {:?}",
        user.server.requests()
    );
}

// ---------------------------------------------------------------------------------------------
// (e) A constraint the document does not satisfy is E_NO_MATCH (4), naming what does exist

#[test]
fn a_constraint_the_document_does_not_satisfy_is_e_no_match_naming_what_exists() {
    for recipe in under_test() {
        let up = upstream(&recipe);
        let e = resolve(&recipe, up.files.clone(), "0.1.0").unwrap_err();
        assert_eq!(e.code, "E_NO_MATCH", "{}: {}", recipe.file, e.message);
        assert_eq!(exit_status(e.code), 4);
        assert!(
            e.message.contains(&recipe.name) && e.message.contains("0.1.0"),
            "{}: {}",
            recipe.file,
            e.message
        );
        assert!(
            e.notes.join(" ").contains(&up.version),
            "{}: the refusal does not name the version that exists: {:?}",
            recipe.file,
            e.notes
        );
    }
}

// ---------------------------------------------------------------------------------------------
// (f) The recipes this package did not touch still declare exactly one artifact

#[test]
fn the_other_recipes_still_declare_one_artifact_each() {
    let multi: Vec<String> = under_test().iter().map(|r| r.file.clone()).collect();
    for recipe in parsed() {
        if multi.contains(&recipe.file) {
            continue;
        }
        assert_eq!(
            recipe.assets.len(),
            1,
            "{}: this package gives no other recipe a second artifact",
            recipe.file
        );
    }
}
