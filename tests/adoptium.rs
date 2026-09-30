//! M-0.4 T-5's package check: the `adoptium` strategy and the `jdk` recipe.
//!
//! Offline and deterministic throughout. The upstream metadata is the **recorded real payload**
//! of `tests/fixtures/catalogue/adoptium/` (see that directory's `README.md`), served by a
//! loopback server the binary and the resolver reach through `LODI_FETCH_REWRITE`; the artifact
//! is a synthetic archive built here, because a 200 MB JDK tarball cannot be reproduced offline
//! and its bytes prove nothing this package is about. No test needs the network or Podman.
//!
//! What is checked here is acceptance (a)…(e) of the package: `jdk = "21"` resolves from the
//! committed page and pins the API's **own** link and checksum, realizes, and `java` runs from
//! the store entry; `JAVA_HOME` is that entry's path inside `lodi develop`, which is what
//! proves `[tools.<name>.env]` and `${self.path}`; `jdk = "latest"` is the newest LTS the
//! fixture's `available_lts_releases` names and not the newest early-access line; a page whose
//! binary carries no `package.checksum` is `E_NO_CHECKSUM`; and a recipe declaring both
//! `adoptium` and `[asset] url` is `E_RECIPE_INVALID`. Acceptance (f), the one live
//! resolution, is `sh scripts/m04-local.sh --case adoptium`.

mod support;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};

use lodi::catalogue::{BUILTIN_TOOLS, Recipe, parse_recipe};
use lodi::diag::exit_status;
use lodi::fetch::{HttpFetcher, parse_rewrites};
use lodi::upstream::resolve_tool;
use lodi::upstream::strategy::Discovery;
use lodi::version::Constraint;
use serde_json::Value;
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
const INFO_URL: &str = "https://api.adoptium.net/v3/info/available_releases";

// ---------------------------------------------------------------------------------------------
// The recipe under test, read from the catalogue itself: no test names a recipe file.

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalogue/adoptium")
}

fn payload(file: &str) -> Value {
    let text =
        std::fs::read_to_string(fixtures().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"))
}

fn text_of(file: &str) -> String {
    std::fs::read_to_string(fixtures().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"))
}

/// The single embedded recipe whose strategy is `adoptium`, with its own file's text. The
/// package must not add a second JVM implementation or a second vendor, and this asserts it.
fn adoptium_recipe() -> (Recipe, &'static str) {
    let mut found = Vec::new();
    for (file, text) in BUILTIN_TOOLS {
        let recipe = parse_recipe(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message));
        if matches!(recipe.versions, Discovery::Adoptium { .. }) {
            found.push((recipe, *text));
        }
    }
    assert_eq!(
        found.len(),
        1,
        "exactly one adoptium recipe ships; found {:?}",
        found
            .iter()
            .map(|(r, _)| r.name.clone())
            .collect::<Vec<_>>()
    );
    found.pop().unwrap()
}

fn line_url(line: u64) -> String {
    format!(
        "https://api.adoptium.net/v3/assets/feature_releases/{line}/ga?os=linux\
         &architecture=x64&image_type=jdk&jvm_impl=hotspot&page_size=20"
    )
}

/// The newest long-term-support line the recorded `available_releases` names.
fn newest_lts() -> u64 {
    payload("available-releases.json")["available_lts_releases"]
        .as_array()
        .expect("available_lts_releases")
        .iter()
        .filter_map(Value::as_u64)
        .max()
        .expect("a non-empty LTS list")
}

/// The release of a recorded page that the documented ordering names, derived here from the
/// page's own fields rather than from the strategy: `major`, `minor`, `security`, `patch`
/// (absent means 0) and `build`, in that order.
fn newest_release(page: &Value) -> Value {
    let key = |r: &Value| {
        let n = |k: &str| r["version_data"][k].as_u64().unwrap_or(0);
        (
            n("major"),
            n("minor"),
            n("security"),
            n("patch"),
            n("build"),
        )
    };
    let mut releases = page.as_array().expect("a release page").clone();
    releases.sort_by_key(key);
    releases.pop().expect("a non-empty page")
}

fn semver(release: &Value) -> String {
    release["version_data"]["semver"]
        .as_str()
        .expect("semver")
        .to_string()
}

fn package(release: &Value) -> &Value {
    &release["binaries"][0]["package"]
}

fn fetcher(server: &Server) -> HttpFetcher {
    HttpFetcher::new(parse_rewrites(&server.rewrite()).unwrap())
}

fn json_bytes(value: &Value) -> Vec<u8> {
    serde_json::to_vec(value).unwrap()
}

// ---------------------------------------------------------------------------------------------
// (a) `jdk = "21"` resolves from the committed page and pins the API's own link and checksum

#[test]
fn jdk_21_resolves_offline_and_pins_the_apis_own_link_and_checksum() {
    let (recipe, _) = adoptium_recipe();
    let page = payload("feature-releases-21.json");
    let server = Server::start(BTreeMap::from([(line_url(21), json_bytes(&page))]));

    let locked = resolve_tool(
        &fetcher(&server),
        &recipe,
        &recipe.name,
        "21",
        &Constraint::parse("21").unwrap(),
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));

    // One request: the line the constraint named. No `available_releases` (the constraint says
    // which line), and no checksum file — the API is the hash source.
    assert_eq!(server.requests(), vec![line_url(21)]);
    assert_eq!(recipe.assets.len(), 1);
    assert!(
        matches!(
            recipe.assets[0].checksum,
            lodi::catalogue::ChecksumSource::Strategy
        ),
        "the API is the recipe's hash source"
    );
    assert!(recipe.assets[0].url.is_none(), "the recipe renders no URL");

    let expected = newest_release(&page);
    let artifact = &locked.artifacts[0];
    assert_eq!(locked.version, semver(&expected));
    assert_eq!(
        artifact.url,
        package(&expected)["link"].as_str().unwrap(),
        "the pinned URL is the API's own package.link"
    );
    assert_eq!(
        artifact.sha256,
        package(&expected)["checksum"].as_str().unwrap(),
        "the pinned SHA-256 is the API's own package.checksum"
    );
    assert_eq!(artifact.size, package(&expected)["size"].as_u64());
    assert_eq!(artifact.sha256.len(), 64);
    assert!(artifact.url.starts_with("https://"));
    // The recipe's own contributed environment travels with the pin, unsubstituted.
    assert_eq!(
        locked.env.get("JAVA_HOME").map(String::as_str),
        Some("${self.path}")
    );
}

#[test]
fn the_ordering_key_prefers_a_patched_build_over_the_build_it_patches() {
    let (recipe, _) = adoptium_recipe();
    let page = payload("feature-releases-21.json");
    // The recorded page carries both 21.0.12+101.0.LTS (OpenJDK 21.0.12.1+1) and
    // 21.0.12+8.0.LTS. Version precedence ignores build metadata, so ordering by the semver
    // string alone would tie them and could pin the older one; the strategy orders by the
    // record's own fields, so the patched build wins.
    let both: Vec<String> = page
        .as_array()
        .unwrap()
        .iter()
        .map(semver)
        .filter(|v| v.starts_with("21.0.12+"))
        .collect();
    assert_eq!(
        both.len(),
        2,
        "the recorded page must keep the pair that proves the ordering: {both:?}"
    );

    let server = Server::start(BTreeMap::from([(line_url(21), json_bytes(&page))]));
    let locked = resolve_tool(
        &fetcher(&server),
        &recipe,
        &recipe.name,
        "21.0.12",
        &Constraint::parse("21.0.12").unwrap(),
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));
    assert_eq!(locked.version, "21.0.12+101.0.LTS");
}

// ---------------------------------------------------------------------------------------------
// (c) `latest` is the newest LTS the API names, never the newest early-access line

#[test]
fn jdk_latest_resolves_the_newest_lts_and_not_the_newest_line() {
    let (recipe, _) = adoptium_recipe();
    let info = payload("available-releases.json");
    let lts = newest_lts();
    let newest_line = info["available_releases"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_u64)
        .max()
        .unwrap();
    // The fixture is only evidence if the two really differ: the API knows lines above the
    // newest LTS, and the newest feature version it names is above them all.
    assert!(
        newest_line > lts,
        "the recorded API must know a line above the newest LTS ({newest_line} vs {lts})"
    );
    assert!(info["most_recent_feature_version"].as_u64().unwrap() > lts);

    let page = payload(&format!("feature-releases-{lts}.json"));
    let server = Server::start(BTreeMap::from([
        (INFO_URL.to_string(), json_bytes(&info)),
        (line_url(lts), json_bytes(&page)),
    ]));
    let locked = resolve_tool(
        &fetcher(&server),
        &recipe,
        &recipe.name,
        "latest",
        &Constraint::Latest,
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));

    // Exactly two requests, in that order: the API's own list of lines, then the LTS line.
    // Nothing asked for the newest line, so a 404 was never even a possibility.
    assert_eq!(server.requests(), vec![INFO_URL.to_string(), line_url(lts)]);
    assert!(
        locked.version.starts_with(&format!("{lts}.")),
        "latest is the newest LTS line: {}",
        locked.version
    );
    assert_eq!(locked.version, semver(&newest_release(&page)));
    assert_eq!(
        locked.artifacts[0].url,
        package(&newest_release(&page))["link"].as_str().unwrap()
    );
}

// ---------------------------------------------------------------------------------------------
// (a) and (b) end to end: realization, `java -version` from the store, and JAVA_HOME

/// An archive shaped the way the recipe says a JDK is: `bin/java` and `bin/javac` that print
/// their version, a non-executable `NOTICE` beside them, and one top directory to strip.
fn jdk_artifact(recipe: &Recipe, version: &str) -> Vec<u8> {
    let prefix = "artifact/";
    let mut members = vec![dir(prefix), dir(&format!("{prefix}bin/"))];
    for entry in &recipe.bin {
        let name = entry.rsplit('/').next().unwrap();
        members.push(file(
            &format!("{prefix}{entry}"),
            &format!("#!/bin/sh\necho \"{name} {version} (synthetic)\"\n"),
            0o755,
        ));
    }
    members.push(file(&format!("{prefix}NOTICE"), "synthetic\n", 0o644));
    assert_eq!(recipe.assets[0].format, "tar.gz");
    assert_eq!(recipe.assets[0].strip_components, 1);
    gzip(&tar(&members))
}

/// The recorded page with the newest binary's `checksum` and `size` replaced by the synthetic
/// archive's own, so that what the fixture server serves is what the lock pins.
fn page_serving(page: &Value, link: &str, bytes: &[u8]) -> Value {
    let mut page = page.clone();
    let hex = lodi::util::sha256_hex(bytes);
    for release in page.as_array_mut().unwrap() {
        for binary in release["binaries"].as_array_mut().unwrap() {
            if binary["package"]["link"].as_str() == Some(link) {
                binary["package"]["checksum"] = Value::String(hex.clone());
                binary["package"]["size"] = Value::from(bytes.len());
            }
        }
    }
    page
}

struct User {
    base: PathBuf,
    server: Server,
}

impl User {
    fn new(name: &str, files: BTreeMap<String, Vec<u8>>) -> User {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home", "project"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        User {
            base,
            server: Server::start(files),
        }
    }

    fn run(&self, args: &[&str], script: &str) -> Output {
        let mut child = Command::new(LODI)
            .args(args)
            .current_dir(self.base.join("project"))
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.base.join("lodi-home"))
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            .env("LODI_TRUST", "1")
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
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn jdk_21_realizes_and_java_runs_from_the_store_with_java_home_set() {
    let (recipe, _) = adoptium_recipe();
    let page = payload("feature-releases-21.json");
    let expected = newest_release(&page);
    let version = semver(&expected);
    let link = package(&expected)["link"].as_str().unwrap().to_string();
    let bytes = jdk_artifact(&recipe, &version);
    let serving = page_serving(&page, &link, &bytes);

    let user = User::new(
        "adoptium-jdk-21",
        BTreeMap::from([
            (line_url(21), json_bytes(&serving)),
            (link.clone(), bytes.clone()),
        ]),
    );
    std::fs::write(
        user.base.join("project/lodi.toml"),
        format!(
            "[project]\nname = \"jdk-probe\"\n\n[tools]\n{} = \"21\"\n",
            recipe.name
        ),
    )
    .unwrap();

    // `lodi develop` never resolves: the lock comes first, from metadata alone.
    let locking = user.run(&["lock"], "");
    assert!(locking.status.success(), "lodi lock: {}", err(&locking));
    assert_eq!(
        user.server.requests(),
        vec![line_url(21)],
        "locking asks for the line's metadata and downloads nothing"
    );

    // `${self.path}` is the artifact's own store entry, so `$JAVA_HOME/bin/java` is the very
    // executable `PATH` exposes. The child runs both, and prints the path it was given.
    let o = user.run(
        &[
            "develop",
            "--",
            "/bin/sh",
            "-c",
            "echo \"JAVA_HOME=$JAVA_HOME\"; java -version; \"$JAVA_HOME/bin/java\" -version",
        ],
        "",
    );
    assert!(o.status.success(), "lodi develop: {}", err(&o));
    let stdout = out(&o);

    let java_home = stdout
        .lines()
        .find_map(|l| l.strip_prefix("JAVA_HOME="))
        .unwrap_or_else(|| panic!("JAVA_HOME was not set: {stdout}"))
        .to_string();
    let store = user.base.join("lodi-home/store");
    assert!(
        PathBuf::from(&java_home).starts_with(&store),
        "JAVA_HOME is not a store entry: {java_home}"
    );
    assert!(
        PathBuf::from(&java_home).join("bin/java").is_file(),
        "JAVA_HOME holds no bin/java: {java_home}"
    );
    assert!(
        java_home.contains(&format!("-{}-", recipe.name)),
        "JAVA_HOME is not this tool's entry: {java_home}"
    );
    // Both invocations ran the same synthetic `java`, once off `PATH` and once through
    // `$JAVA_HOME`, and both report the version the API published.
    assert_eq!(
        stdout
            .lines()
            .filter(|l| *l == format!("java {version} (synthetic)"))
            .count(),
        2,
        "java did not run twice from the store entry: {stdout}"
    );

    // The lock pins the API's own link and checksum, and nothing else was fetched.
    let lock = std::fs::read_to_string(user.base.join("project/lodi.lock")).unwrap();
    assert!(lock.contains(&link), "the lock does not pin the API's link");
    assert!(lock.contains(&lodi::util::sha256_hex(&bytes)));
    assert_eq!(
        user.server.requests(),
        vec![line_url(21), link],
        "one metadata request and one download, and nothing else"
    );
}

// ---------------------------------------------------------------------------------------------
// (d) A binary the API publishes with no checksum is E_NO_CHECKSUM (5) and not a download

#[test]
fn a_binary_with_no_package_checksum_is_e_no_checksum() {
    let (recipe, _) = adoptium_recipe();
    let page = payload("no-checksum.json");
    let stripped = newest_release(&page);
    assert!(
        package(&stripped)["checksum"].is_null(),
        "the crafted page must be missing exactly the newest binary's checksum"
    );
    let server = Server::start(BTreeMap::from([(line_url(21), json_bytes(&page))]));
    let e = resolve_tool(
        &fetcher(&server),
        &recipe,
        &recipe.name,
        "21",
        &Constraint::parse("21").unwrap(),
        "x86_64",
    )
    .unwrap_err();
    assert_eq!(e.code, "E_NO_CHECKSUM", "{}", e.message);
    assert_eq!(exit_status(e.code), 5);
    // Nothing was downloaded, and no second source was tried: one request, the metadata.
    assert_eq!(server.requests(), vec![line_url(21)]);
}

// ---------------------------------------------------------------------------------------------
// M-1.0 u-4 (LD-363): the binary's link is held to Adoptium's own repositories, and each
// record's version text must be the version its structured fields name

/// Resolve `jdk = "21"` against the recorded page with the newest release changed by `edit`.
fn resolve_edited(edit: impl Fn(&mut Value)) -> lodi::diag::Diagnostic {
    let (recipe, _) = adoptium_recipe();
    let mut page = payload("feature-releases-21.json");
    let newest = newest_release(&page);
    for release in page.as_array_mut().unwrap() {
        if *release == newest {
            edit(release);
        }
    }
    let server = Server::start(BTreeMap::from([(line_url(21), json_bytes(&page))]));
    let e = resolve_tool(
        &fetcher(&server),
        &recipe,
        &recipe.name,
        "21",
        &Constraint::parse("21").unwrap(),
        "x86_64",
    )
    .unwrap_err();
    // Nothing was downloaded: one request, the metadata.
    assert_eq!(server.requests(), vec![line_url(21)]);
    e
}

#[test]
fn a_binary_link_on_another_host_is_refused() {
    for link in [
        "https://example.org/adoptium/OpenJDK21U-jdk_x64_linux_hotspot.tar.gz",
        "https://github.com/someone-else/temurin21-binaries/x.tar.gz",
    ] {
        let e = resolve_edited(|release| {
            release["binaries"][0]["package"]["link"] = Value::String(link.into());
        });
        assert_eq!(e.code, "E_RECIPE_CTX", "{link}: {}", e.message);
        assert_eq!(exit_status(e.code), 4);
        assert!(e.message.contains(link), "{}", e.message);
    }
}

#[test]
fn an_unparsable_or_disagreeing_version_is_refused() {
    for text in [
        "not-a-version",
        "21.0.99+7.0.LTS",
        "21.0.12-ea+8",
        "21 .0.12",
    ] {
        let e = resolve_edited(|release| {
            release["version_data"]["semver"] = Value::String(text.into());
        });
        assert_eq!(e.code, "E_RECIPE_CTX", "{text}: {}", e.message);
        assert!(e.message.contains(text), "{}", e.message);
    }
}

// ---------------------------------------------------------------------------------------------
// (e) A recipe that declares both `adoptium` and an `[asset] url` is E_RECIPE_INVALID (4)

#[test]
fn a_recipe_declaring_both_adoptium_and_an_asset_url_is_refused() {
    let file = "both-url-and-adoptium.toml";
    let e = parse_recipe(&text_of(file), file).unwrap_err();
    assert_eq!(e.code, "E_RECIPE_INVALID", "{}", e.message);
    assert_eq!(exit_status(e.code), 4);
    assert!(e.message.contains(file), "{}", e.message);
    assert!(e.message.contains("url"), "{}", e.message);
}

// ---------------------------------------------------------------------------------------------
// The strategy's own refusals: unknown input is an error, never a silent default

#[test]
fn a_malformed_adoptium_recipe_is_refused() {
    let (recipe, text) = adoptium_recipe();
    let cases = [
        // Only HotSpot in 0.4; a second implementation is a package of its own, not a default.
        text.replace(r#"jvm_impl   = "hotspot""#, r#"jvm_impl   = "openj9""#),
        // The API distinguishes jdk and jre and nothing else.
        text.replace(r#"image_type = "jdk""#, r#"image_type = "jdk-headless""#),
        // A feature-release line is a number in range, not a string and not anything.
        text.replace("[asset]", "feature_version = 3\n\n[asset]"),
        text.replace("[asset]", "feature_version = \"21\"\n\n[asset]"),
        // Unknown keys and wrong types are refused, never ignored.
        text.replace("[asset]", "vendor = \"other\"\n\n[asset]"),
        text.replace(r#"jvm_impl   = "hotspot""#, "jvm_impl   = true"),
        // The API is the hash source: a second one beside it is refused, not cross-checked.
        text.replace(
            "[spec]",
            "checksum_file = \"https://example.invalid/sums\"\n\n[spec]",
        ),
        // `[spec] env` may substitute the tool's own path and version and nothing else.
        text.replace("${self.path}", "${project.root}"),
        text.replace("JAVA_HOME = ", "1BAD = "),
    ];
    for case in cases {
        let Err(e) = parse_recipe(&case, &recipe.file) else {
            panic!("accepted a recipe it must refuse:\n{case}");
        };
        assert_eq!(e.code, "E_RECIPE_INVALID", "{}", e.message);
        assert!(e.message.contains(&recipe.file), "{}", e.message);
    }
}
