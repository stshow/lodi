//! M-0.4 T-3's package check: the `github_releases` strategy and the recipes that use it.
//!
//! Offline and deterministic throughout. The upstream metadata is the **recorded real payload**
//! of `tests/fixtures/catalogue/github_releases/` (see that directory's `README.md`), served by
//! a loopback server the binary and the resolver reach through `LODI_FETCH_REWRITE`; the
//! artifacts are synthetic archives built here, because the bytes upstream publishes cannot be
//! reproduced offline, and the digest the fixture server hands out for one is that archive's
//! own. No test needs the network, `GITHUB_TOKEN` or Podman.
//!
//! What is checked here is acceptance (a)…(e) and (g) of the package: all shipped recipes —
//! T-3's seven, and since the recipe batch (rb-1) sixteen more — are present, each resolves
//! from the recorded payload and pins a URL and a SHA-256, realizes from that pin and runs —
//! `jq` through `[asset] format = "binary"`, where the downloaded file is the program and
//! becomes `bin/jq` — a tag the template does not match is ignored, an asset the API publishes
//! without a digest is `E_NO_CHECKSUM` and a 403 is `E_FETCH` with the `GITHUB_TOKEN` hint.
//! Acceptance (f), the one live resolution, is `sh scripts/m04-local.sh --case github-releases`.

mod support;

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::sync::{Arc, Mutex};

use lodi::catalogue::{BUILTIN_TOOLS, Recipe, parse_recipe};
use lodi::diag::exit_status;
use lodi::fetch::{HttpFetcher, parse_rewrites};
use lodi::upstream::resolve_tool;
use lodi::upstream::strategy::Discovery;
use lodi::version::Constraint;
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
/// A token shape that is obviously not a credential, used only to prove the header is sent and
/// that its value never reaches a log, a diagnostic or the lock.
const FAKE_TOKEN: &str = "not-a-real-token-0000";
/// The `github_releases` recipes: T-3's seven and the sixteen of the recipe batch (rb-1). The
/// tests below read the catalogue rather than this list and use it only as the floor: a recipe
/// that silently left the catalogue, or never arrived, is a failure here rather than a test
/// that checks one fewer and passes. `jq`, `shfmt`, `sops` and `yq` are the ones whose artifact
/// is a bare executable (`[asset] format = "binary"`); the others are archives.
const EXPECTED: [&str; 23] = [
    "actionlint",
    "bat",
    "delta",
    "eza",
    "fd",
    "fzf",
    "gh",
    "hyperfine",
    "jq",
    "just",
    "k9s",
    "lazygit",
    "neovim",
    "opentofu",
    "ripgrep",
    "ruff",
    "shellcheck",
    "shfmt",
    "sops",
    "starship",
    "uv",
    "yq",
    "zoxide",
];

// ---------------------------------------------------------------------------------------------
// The recipes under test, read from the catalogue itself: no test names a recipe.

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/catalogue/github_releases")
}

/// Every embedded recipe whose strategy is `github_releases`, with its repository.
fn shipped() -> Vec<(String, Recipe, String)> {
    let mut out = Vec::new();
    for (file, text) in BUILTIN_TOOLS {
        let recipe = parse_recipe(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message));
        if let Discovery::GithubReleases { repo, .. } = &recipe.versions {
            out.push((recipe.name.clone(), recipe.clone(), repo.clone()));
        }
    }
    out
}

fn api_url(repo: &str) -> String {
    format!("https://api.github.com/repos/{repo}/releases?per_page=30")
}

fn payload(file: &str) -> serde_json::Value {
    let text =
        std::fs::read_to_string(fixtures().join(file)).unwrap_or_else(|e| panic!("{file}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{file}: {e}"))
}

fn recorded(repo: &str) -> serde_json::Value {
    payload(&format!("{}.json", repo.replace('/', "--")))
}

// ---------------------------------------------------------------------------------------------
// A loopback API server. `support::Server` answers 200 or 404 only; a refused request and the
// request headers are what this package has to prove, so this file has its own. Since LD-404 a
// route may carry an `ETag`: the server sends it with a 200 and answers a request whose
// `If-None-Match` names it with `304 Not Modified` and no body, as GitHub does.

/// One route: the status, the body and the `ETag` a 200 carries.
#[derive(Clone)]
struct Route {
    status: u16,
    body: Vec<u8>,
    etag: Option<String>,
}

/// One request as the server saw it. Of `Authorization` **only the presence is kept**: the value
/// is never read, so no test can record a token even by accident. `If-None-Match` is not a
/// secret and is kept as sent.
#[derive(Clone, Debug)]
struct Seen {
    url: String,
    authorized: bool,
    if_none_match: Option<String>,
    status: u16,
}

struct Api {
    port: u16,
    routes: Arc<Mutex<BTreeMap<String, Route>>>,
    log: Arc<Mutex<Vec<Seen>>>,
}

impl Api {
    /// `url -> (status, body)`, with no `ETag` on any of them.
    fn start(routes: BTreeMap<String, (u16, Vec<u8>)>) -> Api {
        let api = Api::empty();
        for (url, (status, body)) in routes {
            api.route(&url, status, body, None);
        }
        api
    }

    fn empty() -> Api {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let log = Arc::new(Mutex::new(Vec::new()));
        let routes = Arc::new(Mutex::new(BTreeMap::new()));
        let (thread_routes, thread_log) = (Arc::clone(&routes), Arc::clone(&log));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (routes, log) = (Arc::clone(&thread_routes), Arc::clone(&thread_log));
                std::thread::spawn(move || serve(stream, &routes, &log));
            }
        });
        Api { port, routes, log }
    }

    /// Serve `url` from now on with `status`, `body` and, on a 200, `etag`.
    fn route(&self, url: &str, status: u16, body: Vec<u8>, etag: Option<&str>) {
        let route = Route {
            status,
            body,
            etag: etag.map(str::to_string),
        };
        self.routes.lock().unwrap().insert(url.to_string(), route);
    }

    fn fetcher(&self) -> HttpFetcher {
        HttpFetcher::new(parse_rewrites(&self.rewrite()).unwrap())
    }

    fn rewrite(&self) -> String {
        format!("https://=http://127.0.0.1:{}/", self.port)
    }

    /// The API requests, in order. An artifact download `lodi develop` attempts after it has
    /// locked is not one of them.
    fn seen(&self) -> Vec<Seen> {
        let log = self.log.lock().unwrap();
        log.iter()
            .filter(|s| s.url.starts_with("https://api.github.com/"))
            .cloned()
            .collect()
    }

    fn requests(&self) -> Vec<String> {
        self.seen().into_iter().map(|s| s.url).collect()
    }

    /// Whether each request carried an `Authorization` header.
    fn authorized(&self) -> Vec<bool> {
        self.seen().iter().map(|s| s.authorized).collect()
    }
}

fn serve(mut stream: TcpStream, routes: &Mutex<BTreeMap<String, Route>>, log: &Mutex<Vec<Seen>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let mut request_line = String::new();
    if reader.read_line(&mut request_line).is_err() {
        return;
    }
    let mut authorized = false;
    let mut if_none_match = None;
    loop {
        let mut header = String::new();
        if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
            break;
        }
        if let Some((name, value)) = header.split_once(':') {
            if name.trim().eq_ignore_ascii_case("authorization") {
                authorized = true;
            } else if name.trim().eq_ignore_ascii_case("if-none-match") {
                if_none_match = Some(value.trim().to_string());
            }
        }
    }
    let path = request_line.split_whitespace().nth(1).unwrap_or("/");
    let url = format!("https://{}", &path[1..]);
    let route = routes.lock().unwrap().get(&url).cloned().unwrap_or(Route {
        status: 404,
        body: Vec::new(),
        etag: None,
    });
    let not_modified = route.status == 200
        && route.etag.is_some()
        && if_none_match.is_some()
        && route.etag == if_none_match;
    let (status, body) = if not_modified {
        (304, Vec::new())
    } else {
        (route.status, route.body)
    };
    log.lock().unwrap().push(Seen {
        url,
        authorized,
        if_none_match,
        status,
    });
    let reason = match status {
        200 => "OK",
        304 => "Not Modified",
        403 => "Forbidden",
        429 => "Too Many Requests",
        _ => "Not Found",
    };
    let etag = match (&route.etag, status) {
        (Some(etag), 200 | 304) => format!("ETag: {etag}\r\n"),
        _ => String::new(),
    };
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\n{etag}\
         x-ratelimit-remaining: 0\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(&body);
    let _ = stream.flush();
}

// ---------------------------------------------------------------------------------------------
// Synthetic artifacts, shaped by the recipe under test.

/// An archive shaped the way `recipe` says the real one is: the `[spec] bin` entries as tiny
/// executables that print their version, a **non-executable** `README.md` beside them, and one
/// level of top directory when the recipe strips one.
fn artifact(recipe: &Recipe, version: &str) -> Vec<u8> {
    // `format = "binary"` is not an archive at all: the downloaded file **is** the program,
    // and the store installs those bytes as `bin/<name>`. The synthetic stand-in is therefore
    // the executable itself, with nothing around it to strip or to keep off `PATH`.
    if recipe.assets[0].format == "binary" {
        return format!(
            "#!/bin/sh\necho \"{} {version} (synthetic)\"\n",
            recipe.name
        )
        .into_bytes();
    }
    let prefix = if recipe.assets[0].strip_components == 0 {
        String::new()
    } else {
        "artifact/".to_string()
    };
    let mut members = Vec::new();
    if !prefix.is_empty() {
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
    // Every one of these upstreams ships its README beside the binary. It must not reach PATH.
    members.push(file(&format!("{prefix}README.md"), "# synthetic\n", 0o644));
    let tar = tar(&members);
    match recipe.assets[0].format.as_str() {
        "tar.gz" => gzip(&tar),
        "tar.xz" => xz(&tar),
        other => panic!("no synthetic artifact for format `{other}`"),
    }
}

/// The recorded payload with the chosen asset's `digest` and `size` replaced by the synthetic
/// archive's own, so that what the fixture server serves is what the lock pins.
fn payload_serving(repo: &str, asset_name: &str, bytes: &[u8]) -> serde_json::Value {
    let mut releases = recorded(repo);
    let hex = lodi::util::sha256_hex(bytes);
    for release in releases.as_array_mut().unwrap() {
        for asset in release["assets"].as_array_mut().unwrap() {
            if asset["name"].as_str() == Some(asset_name) {
                asset["digest"] = serde_json::Value::String(format!("sha256:{hex}"));
                asset["size"] = serde_json::Value::from(bytes.len());
            }
        }
    }
    releases
}

// ---------------------------------------------------------------------------------------------
// (a) Every shipped recipe resolves from the recorded payload and pins a URL and a SHA-256

#[test]
fn every_github_releases_recipe_is_shipped() {
    let mut names: Vec<String> = shipped().into_iter().map(|(n, _, _)| n).collect();
    names.sort();
    assert_eq!(
        names, EXPECTED,
        "T-3's seven github_releases recipes and rb-1's sixteen, and no fewer"
    );
}

#[test]
fn every_shipped_recipe_resolves_from_the_recorded_payload() {
    let recipes = shipped();
    assert_eq!(
        recipes.len(),
        EXPECTED.len(),
        "the catalogue ships {} github_releases recipes; found {}",
        EXPECTED.len(),
        recipes.len()
    );
    for (name, recipe, repo) in &recipes {
        let body = serde_json::to_vec(&recorded(repo)).unwrap();
        let api = Api::start(BTreeMap::from([(api_url(repo), (200, body))]));
        let locked = resolve_tool(
            &api.fetcher(),
            recipe,
            name,
            "latest",
            &Constraint::Latest,
            "x86_64",
        )
        .unwrap_or_else(|e| panic!("{name}: {} {}", e.code, e.message));

        assert!(
            locked.artifacts[0]
                .url
                .starts_with(&format!("https://github.com/{repo}/releases/download/")),
            "{name}: {} is not a release asset of {repo}",
            locked.artifacts[0].url
        );
        assert_eq!(
            locked.artifacts[0].sha256.len(),
            64,
            "{name}: {}",
            locked.artifacts[0].sha256
        );
        assert!(
            locked.artifacts[0]
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
            "{name}: {} is not lower-case hex",
            locked.artifacts[0].sha256
        );
        // The digest was taken from the release, not from a checksum file: exactly one request.
        assert_eq!(api.requests(), vec![api_url(repo)], "{name}");
        assert_eq!(
            recipe.assets[0].checksum,
            lodi::catalogue::ChecksumSource::Strategy,
            "{name}: the digest is the release's own, not a checksum file"
        );

        // And it is the digest the recorded payload really carries for that asset.
        let asset_name = locked.artifacts[0].url.rsplit('/').next().unwrap();
        let recorded = recorded(repo);
        let digest = recorded
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|r| r["assets"].as_array().unwrap())
            // By URL, not name: some upstreams publish every release's asset under one name.
            .find(|a| a["browser_download_url"].as_str() == Some(&locked.artifacts[0].url))
            .and_then(|a| a["digest"].as_str())
            .unwrap_or_else(|| panic!("{name}: {asset_name} has no recorded digest"));
        assert_eq!(
            digest,
            format!("sha256:{}", locked.artifacts[0].sha256),
            "{name}"
        );
        assert!(
            locked.tag.is_some(),
            "{name}: the lock records no release tag"
        );
    }
}

// ---------------------------------------------------------------------------------------------
// (b) Every shipped recipe realizes from that lock and its bin entries run

/// One isolated user of the binary: a store, a config directory and a home under the target
/// directory, and a loopback server holding the upstream files of one tool.
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
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

#[test]
fn every_shipped_recipe_realizes_and_its_binaries_run() {
    for (name, recipe, repo) in shipped() {
        // Resolve once against the recorded payload to learn which asset and version upstream
        // offers now, then serve a synthetic archive under exactly that name and digest.
        let probe = Api::start(BTreeMap::from([(
            api_url(&repo),
            (200, serde_json::to_vec(&recorded(&repo)).unwrap()),
        )]));
        let locked = resolve_tool(
            &probe.fetcher(),
            &recipe,
            &name,
            "latest",
            &Constraint::Latest,
            "x86_64",
        )
        .unwrap();
        let asset_name = locked.artifacts[0]
            .url
            .rsplit('/')
            .next()
            .unwrap()
            .to_string();
        let bytes = artifact(&recipe, &locked.version);
        let serving = payload_serving(&repo, &asset_name, &bytes);

        let user = User::new(
            &format!("github-releases-{name}"),
            BTreeMap::from([
                (api_url(&repo), serde_json::to_vec(&serving).unwrap()),
                (locked.artifacts[0].url.clone(), bytes),
            ]),
        );
        let script = recipe
            .bin
            .iter()
            .map(|b| format!("{} --version\n", b.rsplit('/').next().unwrap()))
            .collect::<String>();
        let o = user.shell(&["shell", &name], &script);
        assert!(o.status.success(), "{name}: {}", err(&o));
        for entry in &recipe.bin {
            let binary = entry.rsplit('/').next().unwrap();
            assert!(
                out(&o).contains(&format!("{binary} {} (synthetic)", locked.version)),
                "{name}: `{binary}` did not run from the store: {}",
                out(&o)
            );
        }
        // Only executables reach the environment: the artifact's README does not.
        let links = listing(&user.base.join("lodi-home/store"));
        assert!(
            !links.iter().any(|p| p.ends_with("tree/bin/README.md")),
            "{name}: a non-executable file reached PATH: {links:?}"
        );
    }
}

/// The binary-format recipe on its own: `jq`, whose upstream publishes a bare executable. It
/// pins the URL and the digest the recorded payload carries, realizes through
/// `format = "binary"` rather than through an unpacker, and `jq --version` runs from the store
/// entry the realization published.
#[test]
fn the_binary_format_recipe_pins_realizes_and_runs_from_the_store() {
    let (name, recipe, repo) = shipped()
        .into_iter()
        .find(|(_, r, _)| r.assets[0].format == "binary")
        .expect("the catalogue's one `format = \"binary\"` github_releases recipe");
    assert_eq!(name, "jq", "the binary-format recipe of T-3's seven");
    assert_eq!(recipe.bin, ["bin/jq"], "the installed path is `bin/<name>`");

    // (1) It pins the recorded payload's own URL and digest, from one request and no other.
    let api = Api::start(BTreeMap::from([(
        api_url(&repo),
        (200, serde_json::to_vec(&recorded(&repo)).unwrap()),
    )]));
    let locked = resolve_tool(
        &api.fetcher(),
        &recipe,
        &name,
        "latest",
        &Constraint::Latest,
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{}: {} {}", name, e.code, e.message));
    assert_eq!(api.requests(), vec![api_url(&repo)]);
    let locked_asset = &locked.artifacts[0];
    let asset_name = locked_asset.url.rsplit('/').next().unwrap().to_string();
    let recorded_asset = recorded(&repo)
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["tag_name"].as_str() == locked.tag.as_deref())
        .and_then(|r| r["assets"].as_array())
        .and_then(|assets| {
            assets
                .iter()
                .find(|a| a["name"].as_str() == Some(asset_name.as_str()))
                .cloned()
        })
        .unwrap_or_else(|| panic!("{asset_name} is not an asset of the recorded release"));
    assert_eq!(
        recorded_asset["browser_download_url"].as_str(),
        Some(locked_asset.url.as_str()),
        "the lock must pin the payload's own download URL"
    );
    assert_eq!(
        recorded_asset["digest"].as_str(),
        Some(format!("sha256:{}", locked_asset.sha256).as_str()),
        "the lock must pin the payload's own digest"
    );

    // (2) It realizes through the binary format: the downloaded bytes become `bin/jq`, the
    // one executable of the entry, with nothing unpacked beside it.
    let bytes = artifact(&recipe, &locked.version);
    let serving = payload_serving(&repo, &asset_name, &bytes);
    let user = User::new(
        "github-releases-binary-format",
        BTreeMap::from([
            (api_url(&repo), serde_json::to_vec(&serving).unwrap()),
            (locked_asset.url.clone(), bytes),
        ]),
    );
    let o = user.shell(
        &["shell", &name],
        "jq --version
",
    );
    assert!(o.status.success(), "{name}: {}", err(&o));

    // (3) And it is the store entry that answered, not anything on the ambient PATH.
    assert!(
        out(&o).contains(&format!("jq {} (synthetic)", locked.version)),
        "`jq --version` did not run from the store: {}",
        out(&o)
    );
    let links = listing(&user.base.join("lodi-home/store"));
    assert!(
        links.iter().any(|p| p.ends_with("tree/bin/jq")),
        "the artifact was not installed as bin/jq: {links:?}"
    );
    assert!(
        !links.iter().any(|p| p.ends_with(&asset_name)),
        "the downloaded file was unpacked rather than installed: {links:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// (c) A tag the template does not match is ignored, never mis-parsed

#[test]
fn a_tag_the_template_does_not_match_is_ignored() {
    let (name, recipe, repo) = shipped()
        .into_iter()
        .find(|(n, _, _)| n == "fd")
        .expect("the fd recipe, whose tag template is the default v${version}");
    let body = serde_json::to_vec(&payload("crafted-odd-tags.json")).unwrap();
    let api = Api::start(BTreeMap::from([(api_url(&repo), (200, body))]));
    let locked = resolve_tool(
        &api.fetcher(),
        &recipe,
        &name,
        "latest",
        &Constraint::Latest,
        "x86_64",
    )
    .unwrap_or_else(|e| panic!("{} {}", e.code, e.message));

    // `nightly` and `v1.2.3-rc1` are in the payload and are not versions of this tool; the one
    // tag the template matches is.
    assert_eq!(locked.version, "1.2.2");
    assert_eq!(locked.tag.as_deref(), Some("v1.2.2"));
    assert!(
        locked.artifacts[0]
            .url
            .ends_with("fd-v1.2.2-x86_64-unknown-linux-musl.tar.gz")
    );
}

// ---------------------------------------------------------------------------------------------
// (d) No digest is E_NO_CHECKSUM (5); a 403 is E_FETCH (5) with the GITHUB_TOKEN hint

/// The oldest recorded release of a repository whose asset the API published **without** a
/// digest — the real shape of a release made before GitHub published one.
fn without_digest(repo: &str, recipe: &Recipe) -> (String, String) {
    let Discovery::GithubReleases { tag, asset, .. } = &recipe.versions else {
        unreachable!()
    };
    let prefix = tag.split("${version}").next().unwrap();
    for release in recorded(repo).as_array().unwrap() {
        let tag_name = release["tag_name"].as_str().unwrap();
        let Some(version) = tag_name.strip_prefix(prefix) else {
            continue;
        };
        let wanted = asset
            .replace("${version}", version)
            .replace("${tag}", tag_name)
            .replace("${arch}", "x86_64");
        for a in release["assets"].as_array().unwrap() {
            if a["name"].as_str() == Some(wanted.as_str()) && a["digest"].is_null() {
                return (version.to_string(), wanted);
            }
        }
    }
    panic!("{repo}: every recorded release publishes a digest");
}

#[test]
fn an_asset_the_api_publishes_without_a_digest_is_e_no_checksum() {
    let (name, recipe, repo) = shipped()
        .into_iter()
        .find(|(n, _, _)| n == "hyperfine")
        .expect("the hyperfine recipe");
    let (version, _) = without_digest(&repo, &recipe);
    let body = serde_json::to_vec(&recorded(&repo)).unwrap();
    let api = Api::start(BTreeMap::from([(api_url(&repo), (200, body))]));
    let e = resolve_tool(
        &api.fetcher(),
        &recipe,
        &name,
        &version,
        &Constraint::parse(&version).unwrap(),
        "x86_64",
    )
    .unwrap_err();
    assert_eq!(e.code, "E_NO_CHECKSUM", "{}", e.message);
    assert_eq!(exit_status(e.code), 5);
    // Nothing else was asked for: no checksum file, no mirror, no second source.
    assert_eq!(api.requests(), vec![api_url(&repo)]);
}

#[test]
fn a_refused_request_is_e_fetch_with_the_github_token_hint_and_no_fallback() {
    let (name, recipe, repo) = shipped().into_iter().next().unwrap();
    for status in [403u16, 429] {
        let api = Api::start(BTreeMap::from([(api_url(&repo), (status, Vec::new()))]));
        let e = resolve_tool(
            &api.fetcher(),
            &recipe,
            &name,
            "latest",
            &Constraint::Latest,
            "x86_64",
        )
        .unwrap_err();
        assert_eq!(e.code, "E_FETCH", "{status}: {}", e.message);
        assert_eq!(exit_status(e.code), 5);
        let hint = e.notes.join(" ");
        assert!(hint.contains("GITHUB_TOKEN"), "{status}: {hint}");
        assert!(hint.contains("x-ratelimit-reset"), "{status}: {hint}");
        // One request and one only: no retry loop, no releases.atom, no mirror.
        assert_eq!(api.requests(), vec![api_url(&repo)], "{status}");
    }
}

#[test]
fn the_authorization_header_is_sent_only_when_github_token_is_set_and_never_recorded() {
    let (name, recipe, repo) = shipped().into_iter().next().unwrap();
    let body = serde_json::to_vec(&recorded(&repo)).unwrap();

    let run = |api: &Api| {
        resolve_tool(
            &api.fetcher(),
            &recipe,
            &name,
            "latest",
            &Constraint::Latest,
            "x86_64",
        )
    };

    // This process's environment is its own; the variable is removed again immediately.
    let previous = std::env::var_os("GITHUB_TOKEN");
    let anonymous = Api::start(BTreeMap::from([(api_url(&repo), (200, body.clone()))]));
    unsafe { std::env::remove_var("GITHUB_TOKEN") };
    let locked = run(&anonymous).unwrap();
    assert_eq!(anonymous.authorized(), vec![false]);

    let authenticated = Api::start(BTreeMap::from([(api_url(&repo), (200, body))]));
    unsafe { std::env::set_var("GITHUB_TOKEN", FAKE_TOKEN) };
    let with_token = run(&authenticated).unwrap();
    unsafe {
        match previous {
            Some(v) => std::env::set_var("GITHUB_TOKEN", v),
            None => std::env::remove_var("GITHUB_TOKEN"),
        }
    }
    assert_eq!(authenticated.authorized(), vec![true]);

    // The token changes nothing that is written down, and appears in nothing that is.
    assert_eq!(locked, with_token);
    let written = format!(
        "{:?} {:?} {:?}",
        with_token,
        authenticated.requests(),
        anonymous.requests()
    );
    assert!(!written.contains(FAKE_TOKEN), "the token was recorded");
}

// ---------------------------------------------------------------------------------------------
// The strategy's own refusals: a recipe that names it must be well formed

#[test]
fn a_malformed_github_releases_recipe_is_refused() {
    let (_, recipe, _) = shipped().into_iter().next().unwrap();
    let text = BUILTIN_TOOLS
        .iter()
        .find(|(f, _)| *f == recipe.file)
        .unwrap()
        .1;
    let cases = [
        // `repo` is interpolated into URLs: only `owner/name`.
        text.replace("repo     = \"", "repo     = \"../"),
        text.replace("repo     = \"", "repo     = \"one/two/"),
        // `releases` is one page at most.
        text.replace("[asset]", "releases = 99\n\n[asset]"),
        // A tag template with no capture cannot yield a version.
        text.replace("[asset]", "tag = \"stable\"\n\n[asset]"),
        // Unknown keys and wrong types are refused, never defaulted.
        text.replace("[asset]", "branch = \"main\"\n\n[asset]"),
        text.replace("[asset]", "prereleases = \"yes\"\n\n[asset]"),
        // The URL comes from the release, so the recipe may not also give one.
        text.replace("[asset]", "[asset]\nurl = \"https://example.invalid/x\""),
    ];
    for case in cases {
        let e = parse_recipe(&case, &recipe.file).unwrap_err();
        assert_eq!(e.code, "E_RECIPE_INVALID", "{case}");
        assert!(e.message.contains(&recipe.file), "{}", e.message);
    }
}

#[test]
fn a_release_with_no_build_for_this_architecture_is_unsupported_arch_not_silence() {
    let (name, recipe, repo) = shipped()
        .into_iter()
        .find(|(n, _, _)| n == "fd")
        .expect("the fd recipe");
    // fd v10.4.0 really is a release with no x86_64 musl tarball; the recorded payload has it,
    // behind newer ones, so the page is served from that release on — the recipe reads one page
    // and considers the newest three of it.
    let mut page = recorded(&repo);
    let releases = page.as_array_mut().unwrap();
    let from = releases
        .iter()
        .position(|r| r["tag_name"].as_str() == Some("v10.4.0"))
        .expect("the recorded v10.4.0 release");
    releases.drain(..from);
    let body = serde_json::to_vec(&page).unwrap();
    let api = Api::start(BTreeMap::from([(api_url(&repo), (200, body))]));
    let e = resolve_tool(
        &api.fetcher(),
        &recipe,
        &name,
        "10.4.0",
        &Constraint::parse("10.4.0").unwrap(),
        "x86_64",
    )
    .unwrap_err();
    assert_eq!(e.code, "E_UNSUPPORTED_ARCH", "{}", e.message);
    assert_eq!(exit_status(e.code), 4);
}

// ---------------------------------------------------------------------------------------------
// LD-404: conditional requests. Every GitHub API response that carried an `ETag` is kept, body
// and tag, in `$LODI_HOME/cache/api/`; the next request for the same URL sends `If-None-Match`,
// and a `304 Not Modified` is answered from that entry. These drive the real binary through
// `lodi develop`, which locks a new project first (LD-496), one fresh project per lock and one
// store for all of them, so a second lock is a second resolution of the same release.

/// The recipe whose release list these tests serve: jq, the catalogue's binary-format recipe.
fn jq() -> (String, Recipe, String) {
    shipped()
        .into_iter()
        .find(|(n, _, _)| n == "jq")
        .expect("the jq recipe")
}

/// The recorded jq page, and the same page without its newest release: an older answer of the
/// same URL, so that a changed answer pins a different version.
fn jq_pages(repo: &str) -> (Vec<u8>, Vec<u8>) {
    let full = recorded(repo);
    let mut older = full.clone();
    older.as_array_mut().unwrap().remove(0);
    (
        serde_json::to_vec(&full).unwrap(),
        serde_json::to_vec(&older).unwrap(),
    )
}

const ETAG_1: &str = "W/\"0000000000000000000000000000000000000000000000000000000000000001\"";
const ETAG_2: &str = "W/\"0000000000000000000000000000000000000000000000000000000000000002\"";
const JQ_MANIFEST: &str =
    "[project]\nversion = \"1\"\nname = \"conditional\"\n\n[tools]\njq = \"latest\"\n";

/// One user: a home, a configuration root and a store under the target directory, shared by
/// every lock it makes; each lock gets a project directory of its own.
struct Locker {
    base: PathBuf,
    locks: std::cell::Cell<u32>,
    token: Option<&'static str>,
}

impl Locker {
    fn new(name: &str) -> Locker {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home"] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        Locker {
            base,
            locks: std::cell::Cell::new(0),
            token: None,
        }
    }

    fn store(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn cache_dir(&self) -> PathBuf {
        self.store().join("cache/api")
    }

    /// The one cache entry the store holds; a failure when there is not exactly one.
    fn entry(&self) -> PathBuf {
        let entries: Vec<PathBuf> = std::fs::read_dir(self.cache_dir())
            .unwrap_or_else(|e| panic!("{}: {e}", self.cache_dir().display()))
            .map(|e| e.unwrap().path())
            .filter(|p| !p.file_name().unwrap().to_string_lossy().starts_with('.'))
            .collect();
        assert_eq!(entries.len(), 1, "one cache entry: {entries:?}");
        entries[0].clone()
    }

    /// `lodi develop --trust` of [`JQ_MANIFEST`] in a new project against `rewrite`, which locks
    /// it first: its output and the lock it wrote, if it wrote one. The artifact is not served,
    /// so entering stops after the lock.
    fn lock(&self, rewrite: &str) -> (Output, Option<Vec<u8>>) {
        let n = self.locks.get() + 1;
        self.locks.set(n);
        let project = self.base.join(format!("project-{n}"));
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("lodi.toml"), JQ_MANIFEST).unwrap();
        let mut command = Command::new(LODI);
        command
            .args(["develop", "--trust", "--", "/bin/sh", "-c", ":"])
            .current_dir(&project)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.store())
            .env("LODI_FETCH_REWRITE", rewrite)
            .env("LODI_FETCH_ATTEMPTS", "1");
        if let Some(token) = self.token {
            command.env("GITHUB_TOKEN", token);
        }
        let output = command.output().unwrap();
        let lock = std::fs::read(project.join("lodi.lock")).ok();
        (output, lock)
    }

    /// [`Locker::lock`], which must succeed: the lock's bytes.
    fn locked(&self, api: &Api) -> Vec<u8> {
        let (o, lock) = self.lock(&api.rewrite());
        assert!(err(&o).contains("lodi: wrote lodi.lock"), "{}", err(&o));
        lock.expect("lodi develop wrote lodi.lock")
    }
}

fn pinned_version(lock: &[u8]) -> String {
    let lock: serde_json::Value = serde_json::from_slice(lock).unwrap();
    lock["packages"]["jq"]["version"]
        .as_str()
        .expect("the lock pins jq")
        .to_string()
}

#[test]
fn ld404_a_second_resolve_sends_if_none_match_and_a_304_pins_the_same_release() {
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, full, Some(ETAG_1));
    let user = Locker::new("ld404-304");

    let first = user.locked(&api);
    let second = user.locked(&api);

    let seen = api.seen();
    assert_eq!(seen.len(), 2, "{seen:?}");
    assert_eq!(
        seen[0].if_none_match, None,
        "the first request is not conditional"
    );
    assert_eq!(
        seen[1].if_none_match.as_deref(),
        Some(ETAG_1),
        "the second request names the first response's ETag"
    );
    assert_eq!(seen[1].status, 304, "{seen:?}");
    // The 304 carried no body; the lock is the one the 200 gave, byte for byte.
    assert_eq!(second, first);
}

#[test]
fn ld404_a_200_with_a_new_etag_replaces_the_cache() {
    let (_, _, repo) = jq();
    let (full, older) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, older, Some(ETAG_1));
    let user = Locker::new("ld404-replaced");
    let first = user.locked(&api);

    // A new release: the same URL answers a new body under a new tag.
    api.route(&api_url(&repo), 200, full, Some(ETAG_2));
    let second = user.locked(&api);
    let third = user.locked(&api);

    let seen = api.seen();
    assert_eq!(seen.len(), 3, "{seen:?}");
    assert_eq!(seen[1].if_none_match.as_deref(), Some(ETAG_1));
    assert_eq!(seen[1].status, 200, "the old tag no longer matches");
    assert_eq!(
        seen[2].if_none_match.as_deref(),
        Some(ETAG_2),
        "the cache holds the new tag"
    );
    assert_eq!(seen[2].status, 304);
    // The fresh 200 decided the lock, and the 304 after it kept that answer, not the old one.
    assert_ne!(pinned_version(&second), pinned_version(&first));
    assert_eq!(third, second);
}

#[test]
fn ld404_a_corrupt_cache_file_is_ignored_and_refetched() {
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, full, Some(ETAG_1));
    let user = Locker::new("ld404-corrupt");
    let first = user.locked(&api);
    let entry = user.entry();
    let good = std::fs::read(&entry).unwrap();
    let text = String::from_utf8(good.clone()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&text).unwrap();

    let edited = |edit: &dyn Fn(&mut serde_json::Value)| {
        let mut doc = doc.clone();
        edit(&mut doc);
        serde_json::to_vec(&doc).unwrap()
    };
    let cases: Vec<(&str, Vec<u8>)> = vec![
        ("not JSON", b"\x00\xffnot json".to_vec()),
        ("truncated", good[..good.len() / 2].to_vec()),
        ("empty", Vec::new()),
        (
            "a body that is not the one its digest names",
            edited(&|d| d["body"] = serde_json::Value::from("[]")),
        ),
        (
            "an ETag that would inject a header",
            edited(&|d| d["etag"] = serde_json::Value::from("W/\"1\"\r\nX-Injected: 1")),
        ),
        (
            "another URL's entry",
            edited(&|d| {
                d["url"] = serde_json::Value::from("https://api.github.com/repos/o/r/releases")
            }),
        ),
        (
            "a schema version this build does not read",
            edited(&|d| d["version"] = serde_json::Value::from(99)),
        ),
    ];
    for (what, bytes) in cases {
        std::fs::write(&entry, &bytes).unwrap();
        let before = api.seen().len();
        let again = user.locked(&api);
        let seen = api.seen();
        assert_eq!(
            seen[before].if_none_match, None,
            "{what}: the entry was believed"
        );
        assert_eq!(seen[before].status, 200, "{what}");
        assert_eq!(again, first, "{what}");
        // Refetched: the entry is whole again, so the next request is conditional once more.
        let _ = user.locked(&api);
        let seen = api.seen();
        assert_eq!(
            seen[before + 1].if_none_match.as_deref(),
            Some(ETAG_1),
            "{what}"
        );
        assert_eq!(seen[before + 1].status, 304, "{what}");
    }
}

#[test]
fn ld404_a_rate_limit_is_e_fetch_with_the_token_hint_with_or_without_a_cache_entry() {
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    for status in [403u16, 429] {
        // No entry: a store that has never asked.
        let api = Api::empty();
        api.route(&api_url(&repo), status, Vec::new(), None);
        let user = Locker::new("ld404-limited-cold");
        let (o, lock) = user.lock(&api.rewrite());
        assert_refused(status, &o, lock.as_deref());
        assert_eq!(api.seen()[0].if_none_match, None);

        // An entry: the request is conditional, and the refusal still wins over the cache.
        let api = Api::empty();
        api.route(&api_url(&repo), 200, full.clone(), Some(ETAG_1));
        let user = Locker::new("ld404-limited-warm");
        let _ = user.locked(&api);
        let _ = user.entry();
        api.route(&api_url(&repo), status, Vec::new(), None);
        let (o, lock) = user.lock(&api.rewrite());
        assert_refused(status, &o, lock.as_deref());
        let seen = api.seen();
        assert_eq!(
            seen.len(),
            2,
            "{status}: one request, no fallback: {seen:?}"
        );
        assert_eq!(seen[1].if_none_match.as_deref(), Some(ETAG_1), "{status}");
    }
}

fn assert_refused(status: u16, o: &Output, lock: Option<&[u8]>) {
    assert_eq!(o.status.code(), Some(5), "{status}: {}", err(o));
    let stderr = err(o);
    assert!(stderr.contains("E_FETCH"), "{status}: {stderr}");
    assert!(
        stderr.contains(&format!("HTTP status {status}")),
        "{status}: {stderr}"
    );
    assert!(stderr.contains("GITHUB_TOKEN"), "{status}: {stderr}");
    assert!(stderr.contains("x-ratelimit-reset"), "{status}: {stderr}");
    assert_eq!(lock, None, "{status}: a refused lock writes no lodi.lock");
}

#[test]
fn ld404_offline_is_still_e_fetch_when_a_cache_entry_exists() {
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, full, Some(ETAG_1));
    let user = Locker::new("ld404-offline");
    let _ = user.locked(&api);
    let _ = user.entry();
    // Nothing answers: the cache is not a fallback for the network.
    let closed = ClosedPort::bind();
    let (o, lock) = user.lock(&closed.rewrite());
    assert_eq!(o.status.code(), Some(5), "{}", err(&o));
    assert!(err(&o).contains("E_FETCH"), "{}", err(&o));
    assert_eq!(lock, None);
}

#[test]
fn ld404_github_token_is_still_sent_on_the_conditional_request_and_never_cached() {
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, full, Some(ETAG_1));
    let mut user = Locker::new("ld404-token");
    user.token = Some(FAKE_TOKEN);
    let first = user.locked(&api);
    let second = user.locked(&api);
    let seen = api.seen();
    assert_eq!(api.authorized(), vec![true, true], "{seen:?}");
    assert_eq!(seen[1].if_none_match.as_deref(), Some(ETAG_1));
    assert_eq!(second, first);
    // The token is in no file of the store, the cache entry included.
    let entry = std::fs::read(user.entry()).unwrap();
    assert!(!String::from_utf8_lossy(&entry).contains(FAKE_TOKEN));
    for path in listing(&user.store()) {
        let path = user.store().join(path);
        if path.is_file() {
            let bytes = std::fs::read(&path).unwrap_or_default();
            assert!(
                !String::from_utf8_lossy(&bytes).contains(FAKE_TOKEN),
                "{}",
                path.display()
            );
        }
    }
}

#[test]
fn ld404_the_cache_file_is_owner_only_and_a_symlink_in_its_place_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    let (_, _, repo) = jq();
    let (full, _) = jq_pages(&repo);
    let api = Api::empty();
    api.route(&api_url(&repo), 200, full, Some(ETAG_1));
    let user = Locker::new("ld404-mode-link");
    let first = user.locked(&api);
    let entry = user.entry();
    let mode = |p: &std::path::Path| std::fs::symlink_metadata(p).unwrap().permissions().mode();
    assert_eq!(mode(&entry) & 0o7777, 0o600, "the entry is owner-only");
    assert_eq!(
        mode(&user.cache_dir()) & 0o7777,
        0o700,
        "its directory is owner-only"
    );

    // The entry moves out, and a link to it takes its place: were the link followed, the next
    // request would name the entry's tag and the next write would land on the decoy.
    let decoy = user.base.join("decoy.json");
    std::fs::rename(&entry, &decoy).unwrap();
    let decoy_bytes = std::fs::read(&decoy).unwrap();
    std::os::unix::fs::symlink(&decoy, &entry).unwrap();
    let before = api.seen().len();
    let again = user.locked(&api);
    let seen = api.seen();
    assert_eq!(
        seen[before].if_none_match, None,
        "the link was read through"
    );
    assert_eq!(again, first);
    assert_eq!(
        std::fs::read(&decoy).unwrap(),
        decoy_bytes,
        "the link was written through"
    );
    let link = std::fs::symlink_metadata(&entry).unwrap();
    assert!(
        link.file_type().is_symlink(),
        "the link was replaced, not refused"
    );
    assert_eq!(std::fs::read_link(&entry).unwrap(), decoy);
}
