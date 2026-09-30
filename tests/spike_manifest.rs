//! S-1: the strict project-manifest subset of design `spec/01` (M-Spike).
//!
//! Every accepted key is checked for its parsed value; every refusal is checked for its
//! diagnostic code and, where it matters, its location. The supported subset is listed in
//! `docs/milestones/m-spike/DATA_MODEL.md`.

mod support;

use std::path::{Path, PathBuf};

use lodi::manifest::{
    Arch, Distro, MAX_MANIFEST_BYTES, ManifestErrors, ProjectManifest, load_project_manifest,
    parse_project_manifest, parse_project_manifest_bytes,
};
use lodi::version::{Constraint, Version};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/spike/lodi.toml")
}

fn ok(text: &str) -> ProjectManifest {
    match parse_project_manifest(text, "lodi.toml") {
        Ok(manifest) => manifest,
        Err(errors) => panic!("expected a valid manifest, got:\n{errors}"),
    }
}

fn errors(text: &str) -> ManifestErrors {
    match parse_project_manifest(text, "lodi.toml") {
        Ok(manifest) => panic!("expected errors, parsed {manifest:?}"),
        Err(errors) => errors,
    }
}

/// The manifest fails with exactly these codes, in report order.
fn codes(text: &str) -> Vec<&'static str> {
    errors(text).codes()
}

const CONTAINER: &str = "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n";

#[test]
fn the_spike_fixture_parses_every_supported_field() {
    let m = load_project_manifest(&fixture()).expect("fixture is valid");
    assert_eq!(m.project.schema_version, "1");
    assert_eq!(m.project.name.as_deref(), Some("spike-openssl"));
    let c = m.container.as_ref().expect("container mode");
    assert_eq!(c.distro, Distro::Debian);
    assert_eq!(c.release, "bookworm");
    assert_eq!(c.arch, Arch::X86_64);
    assert_eq!(c.snapshot.as_deref(), Some("2026-09-18T00:00:00Z"));
    assert_eq!(m.packages, ["build-essential", "libssl-dev", "pkg-config"]);
    assert_eq!(m.tools.keys().collect::<Vec<_>>(), ["nodejs", "python"]);
    let python = &m.tools["python"];
    assert_eq!(python.constraint_text, "3.12");
    assert!(
        python
            .constraint
            .matches(&Version::parse("3.12.7").unwrap())
    );
    assert!(
        !python
            .constraint
            .matches(&Version::parse("3.13.0").unwrap())
    );
    assert!(
        m.tools["nodejs"]
            .constraint
            .matches(&Version::parse("22.9.0").unwrap())
    );
    assert_eq!(
        m.env,
        [
            ("OUT_DIR".to_string(), ".lodi/build".to_string()),
            (
                "PKG_CONFIG_PATH".to_string(),
                "/usr/lib/x86_64-linux-gnu/pkgconfig:/usr/share/pkgconfig".to_string()
            ),
        ]
    );
    assert_eq!(m.tasks["build"].run, "make");
    assert_eq!(
        m.tasks["versions"].run,
        "python3 --version && node --version"
    );
    assert!(m.is_container_mode());
    assert_eq!(
        m.environment_name(Path::new("ignored")).as_deref(),
        Some("spike-openssl")
    );
}

#[test]
fn host_tool_mode_needs_no_container_and_uses_defaults() {
    let m = ok("[tools]\nnodejs = \"22\"\n\n[tools.python]\n");
    assert!(!m.is_container_mode());
    assert_eq!(m.project.schema_version, "1");
    assert_eq!(m.project.name, None);
    assert_eq!(m.tools["python"].constraint, Constraint::Latest);
    assert_eq!(m.tools["python"].constraint_text, "latest");
    assert_eq!(
        m.environment_name(Path::new("/srv/My Project")).as_deref(),
        Some("my-project")
    );
    assert!(ok("").tools.is_empty());
}

#[test]
fn equivalent_toml_spellings_are_accepted() {
    let m = ok(r#"
project = { version = "1", name = "a" }
container.distro = "debian"
container.release = "12"
container.arch = "x86_64"
container.home = "shared"
container.user = "same"
container.writable = "ephemeral"
packages.common = ["git", "git", 'make', "g++"]
tools.python = { version = ">=3.11, <3.13" }
tasks.test = { run = 'echo "$HOME" $${literal}' }

[env]
'QUOTED' = """multi
line"""
"#);
    let c = m.container.expect("container");
    assert_eq!(
        c.release, "bookworm",
        "release 12 is normalized to the codename"
    );
    assert_eq!(
        c.snapshot, None,
        "an absent snapshot is pinned by lodi lock"
    );
    assert_eq!(m.packages, ["git", "make", "g++"], "duplicates collapse");
    assert!(
        m.tools["python"]
            .constraint
            .matches(&Version::parse("3.12.1").unwrap())
    );
    assert_eq!(m.tasks["test"].run, "echo \"$HOME\" ${literal}");
    assert_eq!(m.env, [("QUOTED".to_string(), "multi\nline".to_string())]);
}

#[test]
fn unknown_keys_and_tables_are_errors_with_hints() {
    let e = errors("nmae = 1\n[project]\nnmae = \"x\"\n[extras]\na = 1\n");
    assert_eq!(
        e.codes(),
        ["E_UNKNOWN_ATTR", "E_UNKNOWN_ATTR", "E_UNKNOWN_BLOCK"]
    );
    let project = &e.diagnostics[1];
    assert!(project.message.contains("`nmae` in [project]"));
    assert!(
        project
            .notes
            .iter()
            .any(|n| n.contains("did you mean `name`?"))
    );
    let at = project.location.as_ref().unwrap();
    assert_eq!((at.file.as_str(), at.line, at.column), ("lodi.toml", 3, 1));
    assert_eq!(codes("[[tasks]]\nrun = \"x\"\n"), ["E_TYPE"]);
    assert_eq!(codes("[[extra]]\n"), ["E_UNKNOWN_BLOCK"]);
    assert_eq!(
        codes(&format!("{CONTAINER}distroo = \"debian\"\n")),
        ["E_UNKNOWN_ATTR"]
    );
    assert_eq!(
        codes("[tools.python]\nverison = \"3\"\n"),
        ["E_UNKNOWN_ATTR"]
    );
    assert_eq!(
        codes("[tasks.t]\nrun = \"x\"\nrn = \"y\"\n"),
        ["E_UNKNOWN_ATTR"]
    );
    assert_eq!(
        codes(&format!("{CONTAINER}[packages]\ncomon = []\n")),
        ["E_UNKNOWN_ATTR"]
    );
}

#[test]
fn spec_features_outside_the_spike_are_explicitly_unsupported() {
    let cases = [
        "path = [\"bin\"]\n",
        "[vars]\npython = \"3.12\"\n",
        "[inputs.registry]\nsource = \"path:reg\"\n",
        "[modules.m]\nsource = \"./m\"\n",
        "[[hooks.enter]]\nscript = \"echo ready\"\n",
        "[profiles.ci]\nenv = { CI = \"1\" }\n",
        "[services.db]\n",
        "[project]\ndescription = \"x\"\n",
        "[project]\nshell = \"zsh\"\n",
        "[project]\nversion = \"2\"\n",
        // `ubuntu` was outside the spike and is a supported base from M-0.3 T-4 (LD-72); an
        // Ubuntu release this build does not write still is not. `arch` is the third base from
        // M-Arch-Base T-6, with `rolling` its one release — a dated one is not a release.
        "[container]\ndistro = \"ubuntu\"\nrelease = \"jammy\"\n",
        "[container]\ndistro = \"arch\"\nrelease = \"2026.09.01\"\n",
        "[container]\ndistro = \"arch\"\nrelease = \"latest\"\n",
        "[container]\ndistro = \"arch\"\nrelease = \"rolling\"\n[packages.arch]\nadd = [\"x\"]\n",
        "[container]\ndistro = \"debian\"\nrelease = \"trixie\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\narch = \"aarch64\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nunpinned = true\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nnetwork = \"none\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nhome = \"isolated\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nuser = \"root\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nwritable = \"persistent\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nextra_args = [\"--privileged\"]\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n[packages.debian]\nadd = [\"x\"]\n",
        "[tasks.t]\nrun = \"x\"\ndepends_on = [\"u\"]\n",
        "[tasks.t]\nrun = \"x\"\ndescription = \"d\"\n",
    ];
    for text in cases {
        assert_eq!(codes(text), ["E_UNSUPPORTED"], "{text}");
    }

    // M-0.4 T-2 implemented the rest of `spec/01` §3.8, so four cases left this list: an
    // inline `url`/`sha256`, a tool's `env` and a per-architecture table are supported keys
    // now, and an unknown tool name is no longer a manifest error at all — it is `E_NO_RECIPE`
    // at resolution, with the catalogue's nearest names (design call D10). `tests/tools.rs`
    // holds the checks; what belongs here is that the manifest accepts them.
    assert!(
        ok("[tools]\nripgrep = \"latest\"\n")
            .tools
            .contains_key("ripgrep")
    );
    assert!(
        ok("[tools.python]\nenv = { PYTHONHOME = \"x\" }\n")
            .tools
            .contains_key("python")
    );
    assert!(
        ok("[tools.python.x86_64]\nversion = \"3.12\"\n")
            .tools
            .contains_key("python")
    );
    assert_eq!(
        codes("[tools.python]\nversion = \"3.12\"\nurl = \"https://example.invalid/p.tar.gz\"\n"),
        ["E_INLINE_NEEDS_EXACT"]
    );
}

#[test]
fn distro_packages_need_a_container() {
    assert_eq!(codes("[packages]\ncommon = [\"git\"]\n"), ["E_MODE"]);
    assert_eq!(
        codes(&format!("{CONTAINER}[packages]\nhold = [\"git\"]\n")),
        ["E_BLOCK_NOT_ALLOWED"]
    );
}

#[test]
fn wrong_types_and_values_are_e_type() {
    let cases = [
        "[project]\nversion = 1\n",
        "[project]\nname = [\"a\"]\n",
        "project = \"x\"\n",
        "[container]\ndistro = \"gentoo\"\nrelease = \"40\"\n",
        "[container]\nrelease = \"bookworm\"\n",
        "[container]\ndistro = \"debian\"\n",
        // Every supported distribution requires its release; none has a default (T-6).
        "[container]\ndistro = \"arch\"\n",
        "[container]\ndistro = \"ubuntu\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\narch = \"riscv64\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nsnapshot = 2026-09-18T00:00:00Z\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nsnapshot = \"2026-09-18\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nsnapshot = \"2026-02-30T00:00:00Z\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\nhome = \"everywhere\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n[packages]\ncommon = \"git\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n[packages]\ncommon = [1]\n",
        "[tools]\npython = 3.12\n",
        "[tools]\npython = 3\n",
        "[tools]\npython = \"3.x\"\n",
        "[tools]\nnodejs = \">=22, 23\"\n",
        "[env]\nA = 1\n",
        "[env]\nA = [\"x\", 2]\n",
        "[env]\nA = { b = \"c\" }\n",
        "[tasks]\nt = \"make\"\n",
        "[tasks.t]\n",
        "[tasks.t]\nrun = [\"make\"]\n",
        "[tasks.t]\nrun = 1.5\n",
    ];
    for text in cases {
        assert_eq!(codes(text), ["E_TYPE"], "{text}");
    }
    let datetime = errors(&format!("{CONTAINER}snapshot = 2026-09-18T00:00:00Z\n"));
    assert!(
        datetime.diagnostics[0]
            .notes
            .iter()
            .any(|n| n.contains("quoted RFC 3339"))
    );
}

#[test]
fn identifier_patterns_and_reserved_names_are_e_ident() {
    let cases = [
        "[project]\nname = \"!!!\"\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n[packages]\ncommon = [\"Git\"]\n",
        "[container]\ndistro = \"debian\"\nrelease = \"bookworm\"\n[packages]\ncommon = [\"\"]\n",
        "[tools]\nPython = \"3.12\"\n",
        "[env]\n\"1BAD\" = \"x\"\n",
        "[env]\n\"A-B\" = \"x\"\n",
        "[env]\nPATH = \"/bin\"\n",
        "[env]\nHOME = \"/tmp\"\n",
        "[env]\nUSER = \"u\"\n",
        "[env]\nSHELL = \"sh\"\n",
        "[env]\nLODI_HOME = \"x\"\n",
        "[tasks.\"2fast\"]\nrun = \"x\"\n",
    ];
    for text in cases {
        assert_eq!(codes(text), ["E_IDENT"], "{text}");
    }
}

#[test]
fn substitution_is_excluded_or_unsupported_never_silently_kept() {
    assert_eq!(
        codes("[env]\nA = \"${project.root}/src\"\n"),
        ["E_UNSUPPORTED"]
    );
    assert_eq!(
        codes("[tools]\npython = \"${vars.python}\"\n"),
        ["E_VAR_UNSET"]
    );
    for construct in [
        "${a + b}",
        "${upper(x)}",
        "${x ? y : z}",
        "${env.HOME}",
        "${list[0]}",
        "${",
    ] {
        let text = format!("[tasks.t]\nrun = \"echo {construct}\"\n");
        assert_eq!(codes(&text), ["E_EXCLUDED_CONSTRUCT"], "{construct}");
    }
    let m = ok("[env]\nA = \"$HOME/$${x}/$USER\"\nB = \"cost: $5\"\n");
    assert_eq!(m.env[0].1, "$HOME/${x}/$USER");
    assert_eq!(m.env[1].1, "cost: $5");
}

#[test]
fn syntax_errors_include_toml_1_1_only_forms() {
    let cases = [
        "a = \n",
        "[project]\nname = \"a\"\nname = \"b\"\n",
        "[project]\n[project]\n",
        "[env]\nA = \"unterminated\n",
        // TOML 1.1 relaxations that TOML 1.0.0 (spec/01 §1.1) does not allow.
        "project = { version = \"1\", }\n",
        "project = {\n  version = \"1\"\n}\n",
        "[env]\nA = \"\\e[0m\"\n",
        "[env]\nA = \"\\x41\"\n",
    ];
    for text in cases {
        assert_eq!(codes(text), ["E_SYNTAX"], "{text:?}");
    }
    let e = errors("[project]\nname = \"a\"\nname = \"b\"\n");
    let at = e.diagnostics[0]
        .location
        .as_ref()
        .expect("syntax errors carry a location");
    assert_eq!(at.line, 3);
}

#[test]
fn encoding_and_size_limits() {
    let e = parse_project_manifest_bytes(b"[env]\nA = \"\xff\"\n", "lodi.toml").unwrap_err();
    assert_eq!(e.codes(), ["E_SYNTAX"]);
    assert_eq!(e.diagnostics[0].location.as_ref().unwrap().line, 2);
    let mut big = b"# ".to_vec();
    big.resize(MAX_MANIFEST_BYTES + 1, b'x');
    let e = parse_project_manifest_bytes(&big, "lodi.toml").unwrap_err();
    assert_eq!(e.codes(), ["E_SYNTAX"]);
    assert!(e.diagnostics[0].message.contains("1 MiB"));
    let mut limit = b"# ".to_vec();
    limit.resize(MAX_MANIFEST_BYTES, b'x');
    assert!(parse_project_manifest_bytes(&limit, "lodi.toml").is_ok());
}

#[test]
fn all_errors_in_a_file_are_reported_in_order_with_locations() {
    let text = r#"[project]
version = "2"
colour = "blue"

[container]
distro = "debian"
release = "bookworm"
gui = false

[tools]
ruby = "${project.root}"

[env]
PATH = "/opt/bin"
"#;
    let e = errors(text);
    assert_eq!(
        e.codes(),
        [
            "E_UNSUPPORTED",
            "E_UNKNOWN_ATTR",
            "E_UNSUPPORTED",
            "E_UNSUPPORTED",
            "E_IDENT"
        ]
    );
    let lines: Vec<usize> = e
        .diagnostics
        .iter()
        .map(|d| d.location.as_ref().unwrap().line)
        .collect();
    assert_eq!(lines, [2, 3, 8, 11, 14]);
    let rendered = e.to_string();
    assert!(rendered.starts_with(
        "lodi: error E_UNSUPPORTED: manifest schema version \"2\" is not supported\n  --> lodi.toml:2:11\n   = hint: only version \"1\" exists\n"
    ));
    assert_eq!(rendered.matches("lodi: error ").count(), 5);
}

/// A key lodi does not support points the user at a page they have: the project scope's guide,
/// never a file of the development records.
#[test]
fn an_unsupported_key_points_at_the_project_guide() {
    let rendered =
        errors("[project]\nversion = \"1\"\nname = \"p\"\n\n[vars]\nx = \"1\"\n").to_string();
    assert!(rendered.contains("E_UNSUPPORTED"), "{rendered}");
    assert!(rendered.contains("docs/scopes/project.md"), "{rendered}");
    assert!(!rendered.contains("docs/milestones"), "{rendered}");
}

#[test]
fn manifest_errors_use_exit_status_3() {
    assert_eq!(ManifestErrors::EXIT_STATUS, 3);
    assert_eq!(lodi::diag::EXIT_MANIFEST, 3);
    assert_eq!(lodi::diag::EXIT_USAGE, 2);
}

#[test]
fn a_missing_manifest_is_e_no_manifest() {
    let dir = scratch("missing");
    let e = load_project_manifest(&dir.join("lodi.toml")).unwrap_err();
    assert_eq!(e.codes(), ["E_NO_MANIFEST"]);
    let e = load_project_manifest(&dir).unwrap_err();
    assert_eq!(
        e.codes(),
        ["E_NO_MANIFEST"],
        "a directory cannot be read as a manifest"
    );
}

#[test]
fn loading_never_executes_task_text() {
    let dir = scratch("no-exec");
    let marker = dir.join("marker");
    let manifest = dir.join("lodi.toml");
    let run = format!("touch '{}'", marker.display());
    std::fs::write(
        &manifest,
        format!(
            "[tasks.t]\nrun = \"{}\"\n",
            run.replace('\\', "\\\\").replace('"', "\\\"")
        ),
    )
    .unwrap();
    let m = load_project_manifest(&manifest).expect("valid");
    assert_eq!(m.tasks["t"].run, run);
    assert!(!marker.exists(), "the loader ran task text");
}

/// A fresh directory under the cargo target directory, removed first if left over.
fn scratch(name: &str) -> PathBuf {
    support::scratch(&format!("spike-manifest-{name}"))
}
