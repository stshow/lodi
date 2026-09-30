//! M-0.4 T-2: the `[tools]` contract of design `spec/01` §3.8, as the real binary honours it.
//!
//! Offline. Every artifact is built byte by byte by `tests/support` and served by a loopback
//! server the binary reaches through `LODI_FETCH_REWRITE`, with its own `LODI_HOME`, `HOME` and
//! `XDG_CONFIG_HOME` below the target directory. The server's request log is the evidence that
//! an inline tool asks for its artifact and nothing else: no discovery, no catalogue.

mod support;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use lodi::fetch::{HttpFetcher, parse_rewrites};
use lodi::lock::{
    ArtifactEntry, LockFile, RecipeRef, ToolEntry, ToolRequest, ToolSpec, parse_lock,
};
use lodi::store::{Report, Store};
use lodi::util::{sha256_hex, sha256_tagged};
use support::*;

const LODI: &str = env!("CARGO_BIN_EXE_lodi");
const FIXTURES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tools");

/// One isolated user with a loopback server over `files`.
struct Lane {
    base: PathBuf,
    server: Server,
}

impl Lane {
    fn new(name: &str, files: BTreeMap<String, Vec<u8>>) -> Lane {
        let base = scratch(name);
        for d in ["home", "config", "lodi-home", "project"] {
            fs::create_dir_all(base.join(d)).unwrap();
        }
        Lane {
            base,
            server: Server::start(files),
        }
    }

    fn project(&self) -> PathBuf {
        self.base.join("project")
    }

    fn lodi_home(&self) -> PathBuf {
        self.base.join("lodi-home")
    }

    fn write_manifest(&self, text: &str) {
        fs::write(self.project().join("lodi.toml"), text).unwrap();
    }

    fn run(&self, args: &[&str]) -> Output {
        self.run_with(args, &[])
    }

    /// [`Lane::run`] with `env` set on top, `PATH` included.
    fn run_with(&self, args: &[&str], env: &[(&str, OsString)]) -> Output {
        Command::new(LODI)
            .args(args)
            .current_dir(self.project())
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("HOME", self.base.join("home"))
            .env("XDG_CONFIG_HOME", self.base.join("config"))
            .env("LODI_HOME", self.lodi_home())
            .env("LODI_FETCH_REWRITE", self.server.rewrite())
            .env("LODI_BIN", LODI)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }

    fn lock_file(&self) -> LockFile {
        let bytes = fs::read(self.project().join("lodi.lock")).unwrap();
        parse_lock(&bytes).expect("the lock parses")
    }

    fn store(&self) -> Store {
        Store::open(&self.lodi_home()).unwrap()
    }
}

fn out(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

fn err(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

/// A `tar.gz` holding `pkg/dist/bin/<name>` (a runnable script that echoes `stamp`) and
/// `pkg/dist/libexec/<name>.helper`: `strip_components = 1` and `subdir = "dist"` reduce it to
/// `bin/` and `libexec/`.
fn jaq_archive(name: &str, stamp: &str) -> Vec<u8> {
    gzip(&tar(&[
        dir("pkg/"),
        dir("pkg/dist/"),
        dir("pkg/dist/bin/"),
        file(
            &format!("pkg/dist/bin/{name}"),
            &format!("#!/bin/sh\necho {stamp}\n"),
            0o755,
        ),
        dir("pkg/dist/libexec/"),
        file(
            &format!("pkg/dist/libexec/{name}.helper"),
            "# synthetic\n",
            0o644,
        ),
    ]))
}

/// The fixture manifest with each `@NAME@` placeholder replaced.
fn fixture(name: &str, subs: &[(&str, &str)]) -> String {
    let mut text = fs::read_to_string(Path::new(FIXTURES).join(name)).expect("the fixture exists");
    for (key, value) in subs {
        text = text.replace(key, value);
    }
    assert!(!text.contains('@'), "a placeholder was left in {name}");
    text
}

// ------------------------------------------------------------------ (a) every key locks ---

/// A manifest that uses every key of the T-2 table locks, and the lock records the pin the
/// **host** architecture's table selected, with the declaration's env, path and bin.
#[test]
fn a_manifest_using_every_key_locks_the_host_architectures_pin() {
    let base = jaq_archive("jaq", "generic");
    let x86 = jaq_archive("jaq", "x86_64");
    let arm = jaq_archive("jaq", "aarch64");
    let url = |s: &str| format!("https://artifacts.test/jaq/jaq-1.7.1-{s}.tar.gz");
    let lane = Lane::new(
        "tools-every-key",
        BTreeMap::from([
            (url("generic"), base.clone()),
            (url("x86_64"), x86.clone()),
            (url("aarch64"), arm.clone()),
        ]),
    );
    lane.write_manifest(&fixture(
        "every-key.toml",
        &[
            ("@SHA_BASE@", &sha256_hex(&base)),
            ("@SHA_X86@", &sha256_hex(&x86)),
            ("@SHA_AARCH64@", &sha256_hex(&arm)),
        ],
    ));

    let o = lane.run(&["lock"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));

    // Nothing was asked of the network: an inline declaration is already a pin.
    assert!(
        lane.server.requests().is_empty(),
        "locking an inline tool made requests: {:?}",
        lane.server.requests()
    );

    let lock = lane.lock_file();
    let entry = &lock.packages["jaq-cli"];
    assert_eq!(entry.request.name, "jaq", "the `name` key names the tool");
    assert_eq!(entry.request.constraint, "1.7.1");
    assert!(
        entry.request.declaration.is_some(),
        "a declaration with keys beyond `version` is digested"
    );
    assert_eq!(entry.recipe.input, "inline");
    assert_eq!(entry.version, "1.7.1");
    let artifact = &entry.artifacts[0];
    assert_eq!(artifact.url, url("x86_64"), "the per-arch table wins");
    assert_eq!(artifact.sha256, format!("sha256:{}", sha256_hex(&x86)));
    assert_eq!(artifact.format, "tar.gz");
    assert_eq!(artifact.strip_components, 1);
    assert_eq!(artifact.subdir, "dist");
    assert_eq!(entry.spec.arch, "x86_64");
    assert_eq!(entry.spec.bin, ["bin/jaq"]);
    assert_eq!(entry.spec.path, ["bin", "libexec"]);
    assert_eq!(
        entry.spec.env,
        BTreeMap::from([
            ("JAQ_HOME".to_string(), "${self.path}".to_string()),
            ("JAQ_VERSION".to_string(), "${self.version}".to_string()),
        ])
    );

    // The lock is a fixed point: a second `lodi lock` changes nothing and asks nothing.
    let before = fs::read(lane.project().join("lodi.lock")).unwrap();
    let o = lane.run(&["lock"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(fs::read(lane.project().join("lodi.lock")).unwrap(), before);
    assert!(lane.server.requests().is_empty());

    // `lodi lock --check` agrees, and the declaration's env reaches the child with
    // `${self.path}` and `${self.version}` substituted.
    assert_eq!(lane.run(&["lock", "--check"]).status.code(), Some(0));
    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    assert_eq!(lane.run(&["trust"]).status.code(), Some(0));
    let o = lane.run(&["develop", "--", "jaq"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o).trim(), "x86_64");
    let o = lane.run(&["develop", "--", "sh", "-c", "echo $JAQ_HOME $JAQ_VERSION"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let text = out(&o);
    let (home, version) = text.trim().split_once(' ').expect("both are set");
    assert!(
        home.starts_with(
            &lane
                .lodi_home()
                .join("store/art-")
                .to_string_lossy()
                .to_string()
        ),
        "JAQ_HOME is the store path: {home}"
    );
    assert_eq!(version, "1.7.1");
}

// ------------------------------------------- (b) an inline tool realizes without discovery ---

/// An inline tool with `url` and `sha256` realizes and runs. The request log holds exactly one
/// URL — the artifact's — so there was no discovery request and no catalogue recipe.
#[test]
fn an_inline_tool_runs_without_a_single_discovery_request() {
    let bytes = jaq_archive("jaq", "inline-1.7.1");
    let url = "https://artifacts.test/jaq/jaq-1.7.1.tar.gz";
    let lane = Lane::new(
        "tools-inline",
        BTreeMap::from([(url.to_string(), bytes.clone())]),
    );
    lane.write_manifest(&format!(
        "[project]\nname = \"demo\"\n\n[tools.jaq]\nversion = \"1.7.1\"\n\
         url = \"{url}\"\nsha256 = \"{}\"\nstrip_components = 1\nsubdir = \"dist\"\n\
         bin = [\"bin/jaq\"]\n",
        sha256_hex(&bytes)
    ));
    assert_eq!(lane.run(&["lock"]).status.code(), Some(0));
    assert!(lane.server.requests().is_empty(), "locking asked upstream");

    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    assert_eq!(lane.run(&["trust"]).status.code(), Some(0));
    let o = lane.run(&["develop", "--", "jaq"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o).trim(), "inline-1.7.1");
    assert_eq!(
        lane.server.requests(),
        [url],
        "the artifact, and nothing else, was fetched"
    );

    // Warm: the second run downloads nothing at all.
    lane.server.clear();
    let o = lane.run(&["develop", "--", "jaq"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(lane.server.requests().is_empty());
}

// --------------------------------------------------------- (c) several artifacts, one entry ---

/// A tool with more than one artifact extracts them in the order the lock lists them, into one
/// store entry, and the entry's tree hash is the same in a second, independent store.
#[test]
fn several_artifacts_become_one_entry_with_a_stable_tree_hash() {
    let first = gzip(&tar(&[
        dir("core/"),
        dir("core/bin/"),
        file("core/bin/duo", "#!/bin/sh\necho duo core\n", 0o755),
    ]));
    let second = gzip(&tar(&[
        dir("extra/"),
        dir("extra/bin/"),
        file(
            "extra/bin/duo-plugin",
            "#!/bin/sh\necho duo plugin\n",
            0o755,
        ),
        dir("extra/share/"),
        file("extra/share/data.txt", "plugin data\n", 0o644),
    ]));
    let urls = [
        "https://artifacts.test/duo/core.tar.gz",
        "https://artifacts.test/duo/extra.tar.gz",
    ];
    let files = BTreeMap::from([
        (urls[0].to_string(), first.clone()),
        (urls[1].to_string(), second.clone()),
    ]);
    let entry = ToolEntry {
        request: ToolRequest {
            constraint: "1.0.0".into(),
            name: "duo".into(),
            provider: "binary".into(),
            declaration: Some(sha256_tagged(b"duo declaration")),
        },
        provider: "binary".into(),
        version: "1.0.0".into(),
        tag: None,
        recipe: RecipeRef {
            input: "inline".into(),
            path: "[tools.duo]".into(),
            sha256: sha256_tagged(b"duo declaration"),
        },
        artifacts: [&first, &second]
            .iter()
            .zip(urls)
            .map(|(bytes, url)| ArtifactEntry {
                url: url.to_string(),
                sha256: sha256_tagged(bytes),
                size: Some(bytes.len() as u64),
                format: "tar.gz".into(),
                strip_components: 1,
                subdir: String::new(),
                exclude: Vec::new(),
            })
            .collect(),
        spec: ToolSpec {
            arch: "x86_64".into(),
            bin: vec!["bin/duo".into(), "bin/duo-plugin".into()],
            path: vec!["bin".into()],
            env: BTreeMap::new(),
        },
    };

    let mut seen = Vec::new();
    for run in 0..2 {
        let lane = Lane::new(&format!("tools-multi-{run}"), files.clone());
        let fetcher = HttpFetcher::new(parse_rewrites(&lane.server.rewrite()).unwrap());
        let store = lane.store();
        let (name, meta) = store
            .realize_tool(&fetcher, &entry, &mut Report::default(), None)
            .unwrap();
        let path = store.entry_path(&name);
        for bin in &entry.spec.bin {
            assert!(path.join(bin).is_file(), "{bin} is missing from {name}");
        }
        assert!(path.join("share/data.txt").is_file());
        assert_eq!(
            lane.server.requests(),
            urls,
            "the artifacts are fetched in the order the lock lists them"
        );
        seen.push((name, meta.tree_hash));
    }
    assert_eq!(seen[0], seen[1], "the entry name and tree hash are stable");
}

// ---------------------------------------------------------------- (d) `format = "binary"` ---

/// `format = "binary"` installs the downloaded file itself as `bin/<name>`, mode 0755 before
/// the entry is sealed, and it runs.
#[test]
fn a_binary_artifact_becomes_an_executable_under_bin() {
    let bytes = b"#!/bin/sh\necho bare-binary 2.3.4\n".to_vec();
    let url = "https://artifacts.test/bare/bare-2.3.4";
    let lane = Lane::new(
        "tools-binary",
        BTreeMap::from([(url.to_string(), bytes.clone())]),
    );
    lane.write_manifest(&format!(
        "[project]\nname = \"demo\"\n\n[tools.bare]\nversion = \"2.3.4\"\n\
         url = \"{url}\"\nsha256 = \"{}\"\nformat = \"binary\"\n",
        sha256_hex(&bytes)
    ));
    assert_eq!(lane.run(&["lock"]).status.code(), Some(0));
    let lock = lane.lock_file();
    let entry = &lock.packages["bare"];
    assert_eq!(entry.artifacts[0].format, "binary");
    assert_eq!(
        entry.spec.bin,
        ["bin/bare"],
        "a bare binary promises exactly itself"
    );

    // A manifest with tools and no tasks is trusted before its environment is entered (LD-359).
    assert_eq!(lane.run(&["trust"]).status.code(), Some(0));
    let o = lane.run(&["develop", "--", "bare"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o).trim(), "bare-binary 2.3.4");

    let store = lane.store();
    let name = lodi::store::art_name(entry).unwrap();
    let installed = store.entry_path(&name).join("bin/bare");
    let mode = fs::metadata(&installed).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode & 0o111, 0o111, "executable for everyone: {mode:o}");
    assert_eq!(
        fs::read(&installed).unwrap(),
        bytes,
        "the bytes are the tool"
    );
}

// -------------------------------------------------------------------- (e) every refusal ---

/// Each refusal `spec/01` §3.8 asks for: the code, the exit status and the hint the developer
/// acts on, produced by the real binary on a manifest that is one declaration wide.
#[test]
fn every_refusal_names_its_code_its_status_and_the_next_step() {
    let hex = sha256_hex(b"an artifact nobody will download");
    let cases: Vec<(&str, String, i32, &str, &str)> = vec![
        (
            "inline-needs-exact",
            format!(
                "[tools.jaq]\nversion = \"1.7\"\nurl = \"https://artifacts.test/jaq.tar.gz\"\n\
                 sha256 = \"{hex}\"\n"
            ),
            3,
            "E_INLINE_NEEDS_EXACT",
            "version = \"1.7.1\"",
        ),
        (
            "no-checksum",
            "[tools.jaq]\nversion = \"1.7.1\"\nurl = \"https://artifacts.test/jaq.tar.gz\"\n"
                .to_string(),
            5,
            "E_NO_CHECKSUM",
            "sha256",
        ),
        (
            "no-recipe",
            "[tools]\npyton = \"3.12\"\n".to_string(),
            4,
            "E_NO_RECIPE",
            "python",
        ),
        (
            "unsupported-format",
            format!(
                "[tools.jaq]\nversion = \"1.7.1\"\nurl = \"https://artifacts.test/jaq.bin\"\n\
                 sha256 = \"{hex}\"\nformat = \"zip\"\n"
            ),
            3,
            "E_UNSUPPORTED",
            "tar.gz",
        ),
        (
            "unsupported-arch",
            format!(
                "[tools.jaq]\nversion = \"1.7.1\"\n\n[tools.jaq.aarch64]\n\
                 url = \"https://artifacts.test/jaq-arm.tar.gz\"\nsha256 = \"{hex}\"\n"
            ),
            4,
            "E_UNSUPPORTED_ARCH",
            "aarch64",
        ),
    ];
    for (tag, tools, status, code, hint) in cases {
        let lane = Lane::new(&format!("tools-{tag}"), BTreeMap::new());
        lane.write_manifest(&format!("[project]\nname = \"demo\"\n\n{tools}"));
        let o = lane.run(&["lock"]);
        let text = err(&o);
        assert_eq!(o.status.code(), Some(status), "{tag}: {text}");
        assert!(text.contains(code), "{tag}: {text}");
        assert!(
            text.contains(hint),
            "{tag}: the next step is missing: {text}"
        );
        assert_eq!(
            lodi::diag::exit_status(code),
            u8::try_from(status).unwrap(),
            "{tag}"
        );
        assert!(
            !lane.project().join("lodi.lock").exists(),
            "{tag}: a refused manifest wrote a lock"
        );
    }
}

/// The same aarch64-only tool marked `optional` is a warning and an entry left out, and the
/// exit status is unchanged.
#[test]
fn an_optional_tool_without_a_host_build_is_a_warning_and_nothing_else() {
    let hex = sha256_hex(b"an artifact nobody will download");
    let lane = Lane::new("tools-optional", BTreeMap::new());
    lane.write_manifest(&format!(
        "[project]\nname = \"demo\"\n\n[tools.jaq]\nversion = \"1.7.1\"\noptional = true\n\n\
         [tools.jaq.aarch64]\nurl = \"https://artifacts.test/jaq-arm.tar.gz\"\n\
         sha256 = \"{hex}\"\n"
    ));
    let o = lane.run(&["lock"]);
    let text = err(&o);
    assert_eq!(o.status.code(), Some(0), "{text}");
    assert!(text.contains("W_OPTIONAL_SKIPPED"), "{text}");
    assert!(text.contains("jaq"), "{text}");
    let lock = lane.lock_file();
    assert!(
        !lock.packages.contains_key("jaq"),
        "an optional tool with no host build is not locked"
    );
    // And it stays that way: the lock is not stale because of it.
    assert_eq!(
        lane.run(&["lock", "--check"]).status.code(),
        Some(0),
        "{text}"
    );
}

// ------------------------------------------------- (f) the existing locks are untouched ---

/// A declaration that uses no key beyond its constraint writes exactly the bytes earlier builds
/// wrote: the new `declaration` field is absent, and the S-2 lock fixture round-trips byte for
/// byte through this build's serializer.
#[test]
fn a_plain_declaration_locks_byte_identically() {
    let path = Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/tests/fixtures/spike/lodi.lock"
    ));
    let text = fs::read_to_string(path).unwrap();
    let lock = parse_lock(text.as_bytes()).expect("the fixture parses");
    assert_eq!(
        lock.to_canonical_json(),
        text,
        "the S-2 lock fixture no longer round-trips"
    );
    assert!(
        !text.contains("\"declaration\""),
        "a plain declaration must not write a declaration digest"
    );
    for entry in lock.packages.values() {
        assert!(entry.request.declaration.is_none());
        assert_eq!(entry.recipe.input, "builtin");
        assert_eq!(entry.artifacts.len(), 1);
    }
}

// ------------------------------------------------------------------ (g) the reuse contract ---

/// The point of the module: the same `[tools]` text, parsed out of a document that is **not** a
/// project manifest and through a sink of the caller's own, gives the same `ToolSet` and the
/// same diagnostics. Nothing in `lodi::tools` knows which scope's file it came from.
#[test]
fn the_same_tools_table_parses_the_same_way_outside_a_project_manifest() {
    #[derive(Default)]
    struct Sink {
        seen: Vec<(String, String, Vec<String>, Option<usize>)>,
        offset: usize,
    }
    impl lodi::tools::Sink for Sink {
        fn emit(&mut self, d: lodi::diag::Diagnostic, span: Option<std::ops::Range<usize>>) {
            self.seen.push((
                d.code.to_string(),
                d.message.clone(),
                d.notes.clone(),
                span.map(|s| s.start - self.offset),
            ));
        }
    }

    // Inline tables, so the same text is one `[tools]` table wherever it is embedded.
    let tools = "jaq = \"1.7.1\"\nPyton = \"3.12\"\n\
                 nodejs = { version = \"22\", path = [\"bin\"] }\n\
                 bad = { version = \"1.7\", url = \"https://artifacts.test/bad.tar.gz\", \
                 sha256 = \"aa\" }\n";

    let parse = |text: &str, header: &str| {
        let doc = toml_edit::ImDocument::parse(text.to_string()).expect("the document parses");
        let table = doc
            .as_table()
            .get(header)
            .and_then(toml_edit::Item::as_table_like)
            .expect("the document has the table");
        let mut sink = Sink {
            seen: Vec::new(),
            // Spans are offsets into the caller's own text, so they are compared relative to
            // where the `[tools]` table starts in each document.
            offset: text.find(tools).expect("the table text is embedded"),
        };
        let set = lodi::tools::parse(table, &mut sink);
        (set, sink.seen)
    };

    let manifest = format!("[project]\nname = \"demo\"\n\n[tools]\n{tools}");
    // Not a project manifest: another scope's document, with its own surrounding tables.
    let elsewhere = format!(
        "[home]\nprofile = \"default\"\n\n[settings]\ncolour = \"auto\"\n\n[tools]\n{tools}"
    );
    let (from_manifest, manifest_diags) = parse(&manifest, "tools");
    let (from_elsewhere, elsewhere_diags) = parse(&elsewhere, "tools");

    assert_eq!(from_manifest, from_elsewhere, "the same ToolSet");
    assert_eq!(manifest_diags, elsewhere_diags, "the same diagnostics");
    assert!(
        !manifest_diags.is_empty(),
        "the fixture text must exercise the diagnostics too"
    );
    assert_eq!(
        from_manifest.keys().collect::<Vec<_>>(),
        ["jaq", "nodejs"],
        "the refused declarations are left out"
    );

    // And the project manifest agrees with the module: the same text through `lodi::manifest`
    // produces the same set.
    let dir = scratch("tools-reuse");
    fs::write(dir.join("lodi.toml"), &manifest).unwrap();
    let parsed = lodi::manifest::load_project_manifest(&dir.join("lodi.toml"));
    let (project, diagnostics) = match parsed {
        Ok(m) => (m.tools.clone(), Vec::new()),
        Err(f) => (BTreeMap::new(), f.diagnostics),
    };
    assert!(
        !diagnostics.is_empty(),
        "the same text is refused as a manifest"
    );
    assert!(project.is_empty());
    // The manifest sorts its diagnostics by location before reporting them, so the comparison
    // is of the set of codes, not of the order the module emitted them in.
    let mut codes: Vec<&str> = diagnostics.iter().map(|d| d.code).collect();
    let mut module: Vec<&str> = manifest_diags
        .iter()
        .map(|(c, _, _, _)| c.as_str())
        .collect();
    codes.sort_unstable();
    module.sort_unstable();
    assert_eq!(codes, module, "the manifest path emits the module's codes");
}

// ------------------------------------ (f) tools on PATH are trusted like task text (LD-359) ---

/// A project whose one tool is a bare binary called `name`, locked; the lane and its bytes.
fn bare_tool(lane_name: &str, name: &str) -> Lane {
    let bytes = format!("#!/bin/sh\necho {name} from the environment\n").into_bytes();
    let url = format!("https://artifacts.test/{name}/{name}-2.3.4");
    let lane = Lane::new(lane_name, BTreeMap::from([(url.clone(), bytes.clone())]));
    lane.write_manifest(&format!(
        "[project]\nname = \"demo\"\n\n[tools.{name}]\nversion = \"2.3.4\"\n\
         url = \"{url}\"\nsha256 = \"{}\"\nformat = \"binary\"\n",
        sha256_hex(&bytes)
    ));
    assert_eq!(lane.run(&["lock"]).status.code(), Some(0));
    lane
}

/// The `art-` entries of a lane's store.
fn realized(lane: &Lane) -> Vec<String> {
    fs::read_dir(lane.lodi_home().join("store"))
        .map(|d| {
            d.flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.starts_with("art-"))
                .collect()
        })
        .unwrap_or_default()
}

/// A manifest with tools and no tasks realizes and runs nothing until it is trusted, like one
/// with task text: `LODI_TRUST=1` authorizes one invocation, `lodi trust` shows the whole
/// manifest and records it, any change to the file asks again, and a revoked one is refused.
#[test]
fn a_manifest_with_tools_and_no_tasks_is_trusted_before_it_is_entered() {
    let lane = bare_tool("tools-trust", "bare");
    let trust_file = lane.base.join("config/lodi/trust.json");

    let o = lane.run(&["develop", "--", "bare"]);
    assert_eq!(o.status.code(), Some(11), "{}", err(&o));
    assert!(
        err(&o).contains("E_TRUST_REQUIRED")
            && err(&o).contains("declares tools or a base and has not been trusted"),
        "{}",
        err(&o)
    );
    assert!(lane.server.requests().is_empty(), "something was fetched");
    assert!(realized(&lane).is_empty(), "something was realized");

    let once = [("LODI_TRUST", OsString::from("1"))];
    let o = lane.run_with(&["develop", "--", "bare"], &once);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o).trim(), "bare from the environment");
    assert!(
        err(&o).contains("LODI_TRUST=1 authorizes this invocation only (manifest sha256:"),
        "{}",
        err(&o)
    );
    assert!(!trust_file.exists(), "LODI_TRUST=1 recorded something");
    assert_eq!(lane.run(&["develop", "--", "bare"]).status.code(), Some(11));

    let o = lane.run(&["trust"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    let shown = out(&o);
    assert!(
        shown.contains("declares no tasks, and tools or a base")
            && shown.contains("    | [tools.bare]")
            && shown.contains("manifest hash: sha256:")
            && shown.contains("outside this authorization"),
        "{shown}"
    );
    let o = lane.run(&["develop", "--", "bare"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));

    // Any change to the file is a manifest that was not trusted in this form.
    let path = lane.project().join("lodi.toml");
    let mut text = fs::read_to_string(&path).unwrap();
    text.push_str("# edited\n");
    fs::write(&path, &text).unwrap();
    let o = lane.run(&["develop", "--", "bare"]);
    assert_eq!(o.status.code(), Some(11), "{}", err(&o));
    assert!(
        err(&o).contains("W_TRUST_CHANGED") && err(&o).contains("changed since it was trusted"),
        "{}",
        err(&o)
    );

    assert_eq!(lane.run(&["trust"]).status.code(), Some(0));
    assert!(out(&lane.run(&["trust", "--revoke"])).contains("revoked"));
    assert_eq!(lane.run(&["develop", "--", "bare"]).status.code(), Some(11));
}

/// What a trust record covers, decided from the manifest alone: task text when there are
/// tasks, the whole file when there are tools or a container base and no tasks, nothing else.
#[test]
fn tools_or_a_base_without_tasks_need_trust_and_nothing_else_does() {
    use lodi::trust::Subject;
    let parse = |text: &str| {
        lodi::manifest::parse_project_manifest(text, "lodi.toml").expect("a valid manifest")
    };
    let tools = "[project]\nname = \"d\"\n\n[tools]\npython = \"3.12\"\n";
    assert_eq!(
        Subject::of(&parse(tools), tools.as_bytes()),
        Subject::Manifest(tools.as_bytes().to_vec())
    );
    let base = "[project]\nname = \"d\"\n\n[container]\ndistro = \"debian\"\n\
                release = \"bookworm\"\n";
    assert_eq!(
        Subject::of(&parse(base), base.as_bytes()),
        Subject::Manifest(base.as_bytes().to_vec())
    );
    let tasks = format!("{tools}\n[tasks.hello]\nrun = \"echo hello\"\n");
    assert_eq!(
        Subject::of(&parse(&tasks), tasks.as_bytes()),
        Subject::Tasks(vec![("hello".into(), "echo hello".into())])
    );
    let plain = "[project]\nname = \"d\"\n";
    let subject = Subject::of(&parse(plain), plain.as_bytes());
    assert_eq!(subject, Subject::Nothing);
    assert_eq!(subject.hash(), None);
}

/// A PATH directory holding an executable called `name`, like a host command of that name.
fn host_command(lane: &Lane, name: &str, mode: u32) -> PathBuf {
    let dir = lane.base.join("host-bin");
    fs::create_dir_all(&dir).unwrap();
    let file = dir.join(name);
    fs::write(&file, "#!/bin/sh\necho host\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(mode)).unwrap();
    dir
}

/// Entering an environment whose bin has a name that an executable on the caller's `PATH` also
/// has warns once with `W_SHADOWS_HOST`, naming it; the environment's command still wins. One
/// that shadows nothing, or only a file that is not executable, prints no such warning.
#[test]
fn an_environment_that_shadows_a_host_command_says_so() {
    let lane = bare_tool("tools-shadow", "bare");
    assert_eq!(lane.run(&["trust"]).status.code(), Some(0));
    let with_dir = |dir: &Path| {
        let mut path = dir.as_os_str().to_os_string();
        path.push(":");
        path.push(std::env::var_os("PATH").unwrap_or_default());
        [("PATH", path)]
    };

    let o = lane.run(&["develop", "--", "bare"]);
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!err(&o).contains("W_SHADOWS_HOST"), "{}", err(&o));

    let inert = host_command(&lane, "bare", 0o644);
    let o = lane.run_with(&["develop", "--", "bare"], &with_dir(&inert));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert!(!err(&o).contains("W_SHADOWS_HOST"), "{}", err(&o));

    let host = host_command(&lane, "bare", 0o755);
    let o = lane.run_with(&["develop", "--", "bare"], &with_dir(&host));
    assert_eq!(o.status.code(), Some(0), "{}", err(&o));
    assert_eq!(out(&o).trim(), "bare from the environment");
    let warning = "lodi: warning W_SHADOWS_HOST: this environment runs its own `bare` in place \
                   of the host command of the same name on PATH\n";
    assert_eq!(err(&o).matches("W_SHADOWS_HOST").count(), 1, "{}", err(&o));
    assert!(err(&o).contains(warning), "{}", err(&o));
}

/// Only a name that runs on both sides is a shadow: an entry of the environment's `bin` that is
/// not an executable file (a plain file, a directory, a link to a directory) runs nothing, so
/// it shadows nothing even where the caller's `PATH` has an executable of that name.
#[test]
fn only_an_executable_in_the_environment_shadows_a_host_command() {
    let base = scratch("tools-shadow-kinds");
    let (bin, host) = (base.join("env-bin"), base.join("host-bin"));
    let program = |file: &Path, mode: u32| {
        fs::write(file, "#!/bin/sh\necho program\n").unwrap();
        fs::set_permissions(file, fs::Permissions::from_mode(mode)).unwrap();
    };
    fs::create_dir_all(&host).unwrap();
    for name in ["tool", "plain", "folder", "folder-link"] {
        program(&host.join(name), 0o755);
    }
    fs::create_dir_all(bin.join("folder")).unwrap();
    fs::set_permissions(bin.join("folder"), fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(bin.join("folder"), bin.join("folder-link")).unwrap();
    program(&bin.join("plain"), 0o644);
    program(&bin.join("tool"), 0o755);

    let path = host.into_os_string();
    assert_eq!(
        lodi::host::shadowed_host_commands(&bin, &path),
        vec!["tool".to_string()],
        "only an executable file of the environment runs in place of the host command"
    );
    let warning = "lodi: warning W_SHADOWS_HOST: this environment runs its own `tool` in place \
                   of the host command of the same name on PATH";
    assert_eq!(
        lodi::host::shadow_warning(&bin, Some(path.as_os_str())).as_deref(),
        Some(warning)
    );
    let _ = fs::remove_dir_all(&base);
}

// ------------------------------- (i) a catalogue comment edit leaves existing locks fresh ---

/// The spike fixture's lock was written before the catalogue's recipe comments were edited, so
/// it records each recipe file's old digest. Freshness is decided by what the manifest asks for,
/// not by that digest: `lodi lock` keeps every entry, rewrites no byte and fetches nothing.
#[test]
fn lock_from_before_the_catalogue_comment_edit_stays_fresh() {
    let spike = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/spike"));
    let lane = Lane::new("tools-comment-edit", BTreeMap::new());
    let before = fs::read(spike.join("lodi.lock")).unwrap();
    fs::copy(spike.join("lodi.toml"), lane.project().join("lodi.toml")).unwrap();
    fs::write(lane.project().join("lodi.lock"), &before).unwrap();
    let lock = lane.lock_file();
    assert!(!lock.packages.is_empty(), "the fixture locks no tool");
    for (label, entry) in &lock.packages {
        let name = entry.recipe.path.strip_suffix(".toml").unwrap();
        let recipe = lodi::catalogue::builtin_recipe(name)
            .expect("a built-in recipe")
            .expect("it parses");
        assert_ne!(
            entry.recipe.sha256, recipe.sha256,
            "{label}: the fixture does not predate the edit of {name}.toml"
        );
    }
    for args in [&["lock"][..], &["lock", "--check"]] {
        let o = lane.run(args);
        assert_eq!(o.status.code(), Some(0), "{args:?}: {}", err(&o));
        assert!(
            out(&o).starts_with("lodi.lock is up to date: "),
            "{args:?}: {}",
            out(&o)
        );
    }
    assert_eq!(fs::read(lane.project().join("lodi.lock")).unwrap(), before);
    assert!(
        lane.server.requests().is_empty(),
        "a fresh lock fetched {:?}",
        lane.server.requests()
    );
}
