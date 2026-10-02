//! `[programs.<name>]`: the module framework, its writers and a rendered file's lifecycle
//! (M-Home c-2, `docs/design/HOME_PROGRAMS.md` §4, §6, §10; invariants I1–I4, I8, I9, I11–I13).
//!
//! The modules here are **test modules**, one per writer format (the shipped modules are
//! `tests/home_programs_shipped.rs`'s), driven through the library's registry seams
//! (`parse_home_manifest_in`, `plan_with`, `apply_with`, `status_with`) — the whole loader,
//! planner and apply with an explicit registry.
//! The built binary is asked only what it answers with the shipped registry.
//!
//! The rendered bytes of each writer are compared with the committed goldens under
//! `tests/fixtures/programs/`. Setting `LODI_RECORD_EXPECTED=1` rewrites them from the code
//! instead of comparing; no validation command sets it, so a gate always compares.
//!
//! Offline and deterministic. No network, no Podman, no clock; every case works below
//! `CARGO_TARGET_TMPDIR`.

mod support;

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use lodi::diag::Diagnostic;
use lodi::fetch::{FetchError, Fetcher};
use lodi::home::apply::{Options, apply_reporting, apply_with, status_with};
use lodi::home::manifest::{FileState, HomeManifest, OnRemove, Origin, parse_home_manifest_in};
use lodi::home::plan::{Verb, plan_with};
use lodi::home::programs::{
    self, Body, Comment, Fields, Module, Program, RenderEnv, Rendered, Shadow,
};
use lodi::home::render::{self, GitSection, Scalar};
use lodi::home::toml::{Table, Value};
use lodi::progress::{Event, Sink};
use lodi::roots::Roots;

use support::{HomeEnv, home_env};

// ------------------------------------------------------------------------- the test modules ---

/// Canonical TOML: `$XDG_CONFIG_HOME/demo/config.toml`, shadowed by `~/.demorc`.
struct Demo {
    theme: Option<String>,
    size: Option<i64>,
    settings: Table,
}

impl Program for Demo {
    const NAME: &'static str = "demo";

    fn read(f: &mut Fields<'_>) -> Option<Self> {
        let theme = f.choice("theme", &["dark", "light"]);
        let size = f.integer("font.size", 6..=72);
        let settings = match f.get("settings") {
            None => Table::new(),
            Some(Value::Table(t)) => t.clone(),
            Some(_) => {
                f.refuse("settings", "E_TYPE", "must be a table", "write a table");
                Table::new()
            }
        };
        (!f.failed()).then_some(Demo {
            theme,
            size,
            settings,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let mut table = Table::new();
        if let Some(theme) = &self.theme {
            table.insert("theme".into(), Value::String(theme.clone()));
        }
        if let Some(size) = self.size {
            let mut font = Table::new();
            font.insert("size".into(), Value::Integer(size));
            table.insert("font".into(), Value::Table(font));
        }
        if !self.settings.is_empty() {
            table.insert("settings".into(), Value::Table(self.settings.clone()));
        }
        vec![Rendered {
            path: env.xdg_config_home.join("demo/config.toml"),
            body: Body::Toml(table),
            state: FileState::Managed,
            takes_extra: true,
        }]
    }

    fn shadowed_by(&self, env: &RenderEnv) -> Vec<Shadow> {
        vec![Shadow {
            by: env.home.join(".demorc"),
            of: env.xdg_config_home.join("demo/config.toml"),
        }]
    }
}

/// git-config INI: sections and subsections, every string quoted.
struct Gitish {
    name: Option<String>,
    editor: Option<String>,
    aliases: BTreeMap<String, String>,
    branches: BTreeMap<String, String>,
}

impl Program for Gitish {
    const NAME: &'static str = "gitish";

    fn read(f: &mut Fields<'_>) -> Option<Self> {
        let name = f.string("user.name");
        let editor = f.string("core.editor");
        let aliases = f.string_map("aliases").unwrap_or_default();
        let branches = f.string_map("branches").unwrap_or_default();
        for (key, value) in [("user.name", &name), ("core.editor", &editor)] {
            if value
                .as_deref()
                .is_some_and(|v| !render::git_representable(v))
            {
                f.refuse(
                    key,
                    "E_TYPE",
                    "holds a control character git cannot store",
                    "remove it",
                );
            }
        }
        (!f.failed()).then_some(Gitish {
            name,
            editor,
            aliases,
            branches,
        })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let str_keys = |map: &BTreeMap<String, String>| -> Vec<(String, Scalar)> {
            map.iter()
                .map(|(k, v)| (k.clone(), Scalar::Str(v.clone())))
                .collect()
        };
        let mut sections = Vec::new();
        if !self.aliases.is_empty() {
            sections.push(GitSection {
                name: "alias".into(),
                subsection: None,
                keys: str_keys(&self.aliases),
            });
        }
        for (branch, remote) in &self.branches {
            sections.push(GitSection {
                name: "branch".into(),
                subsection: Some(branch.clone()),
                keys: vec![("remote".into(), Scalar::Str(remote.clone()))],
            });
        }
        if let Some(editor) = &self.editor {
            sections.push(GitSection {
                name: "core".into(),
                subsection: None,
                keys: vec![("editor".into(), Scalar::Str(editor.clone()))],
            });
        }
        if let Some(name) = &self.name {
            sections.push(GitSection {
                name: "user".into(),
                subsection: None,
                keys: vec![
                    ("name".into(), Scalar::Str(name.clone())),
                    ("useConfigOnly".into(), Scalar::Bool(true)),
                ],
            });
        }
        vec![Rendered {
            path: env.xdg_config_home.join("gitish/config"),
            body: Body::Text {
                comment: Comment::Hash,
                text: render::git_config(&sections),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// Space-separated key-value lines.
struct Kvish {
    settings: BTreeMap<String, String>,
}

impl Program for Kvish {
    const NAME: &'static str = "kvish";

    fn read(f: &mut Fields<'_>) -> Option<Self> {
        let settings = f.string_map("settings").unwrap_or_default();
        (!f.failed()).then_some(Kvish { settings })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let pairs: Vec<(String, String)> = self
            .settings
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        vec![Rendered {
            path: env.xdg_config_home.join("kvish/kvish.conf"),
            body: Body::Text {
                comment: Comment::Hash,
                text: render::kv_lines(&pairs, " "),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// tmux `set` lines.
struct Tmuxish {
    options: Vec<(String, Scalar)>,
}

impl Program for Tmuxish {
    const NAME: &'static str = "tmuxish";

    fn read(f: &mut Fields<'_>) -> Option<Self> {
        let mut options = Vec::new();
        if let Some(mouse) = f.boolean("mouse") {
            options.push(("mouse".to_string(), Scalar::Bool(mouse)));
        }
        if let Some(history) = f.integer("history_limit", 0..=1_000_000) {
            options.push(("history-limit".to_string(), Scalar::Int(history)));
        }
        if let Some(left) = f.string("status_left") {
            options.push(("status-left".to_string(), Scalar::Str(left)));
        }
        if let Some(right) = f.string("status_right") {
            options.push(("status-right".to_string(), Scalar::Str(right)));
        }
        (!f.failed()).then_some(Tmuxish { options })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.xdg_config_home.join("tmuxish/tmux.conf"),
            body: Body::Text {
                comment: Comment::Hash,
                text: render::tmux_lines(&self.options),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// Lua assignments, with a `--` header.
struct Luaish {
    options: Table,
}

impl Program for Luaish {
    const NAME: &'static str = "luaish";

    fn read(f: &mut Fields<'_>) -> Option<Self> {
        let options = match f.get("options") {
            None => Table::new(),
            Some(Value::Table(t)) => t.clone(),
            Some(_) => {
                f.refuse("options", "E_TYPE", "must be a table", "write a table");
                Table::new()
            }
        };
        (!f.failed()).then_some(Luaish { options })
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        let pairs: Vec<(String, Value)> = self
            .options
            .iter()
            .map(|(k, v)| (format!("vim.opt.{k}"), v.clone()))
            .collect();
        vec![Rendered {
            path: env.xdg_config_home.join("luaish/init.lua"),
            body: Body::Text {
                comment: Comment::DoubleDash,
                text: render::lua_assignments(&pairs),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// No typed keys at all: the file is the header and the opaque `extra`, byte-exact (I11).
struct Opaque;

impl Program for Opaque {
    const NAME: &'static str = "opaque";

    fn read(_: &mut Fields<'_>) -> Option<Self> {
        Some(Opaque)
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.home.join(".opaquerc"),
            body: Body::Text {
                comment: Comment::Hash,
                text: String::new(),
            },
            state: FileState::Managed,
            takes_extra: true,
        }]
    }
}

/// A file that takes no `extra`, so an `extra` on its table has nowhere to go.
struct Plain;

impl Program for Plain {
    const NAME: &'static str = "plain";

    fn read(_: &mut Fields<'_>) -> Option<Self> {
        Some(Plain)
    }

    fn render(&self, env: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: env.home.join(".plainrc"),
            body: Body::Text {
                comment: Comment::Hash,
                text: "plain\n".into(),
            },
            state: FileState::Managed,
            takes_extra: false,
        }]
    }
}

/// A module that renders outside the home directory, which the loader must refuse.
struct Stray;

impl Program for Stray {
    const NAME: &'static str = "stray";

    fn read(_: &mut Fields<'_>) -> Option<Self> {
        Some(Stray)
    }

    fn render(&self, _: &RenderEnv) -> Vec<Rendered> {
        vec![Rendered {
            path: PathBuf::from("/nonexistent-elsewhere/stray.conf"),
            body: Body::Text {
                comment: Comment::Hash,
                text: "x\n".into(),
            },
            state: FileState::Managed,
            takes_extra: false,
        }]
    }
}

const REGISTRY: &[Module] = &[
    programs::module::<Demo>(),
    programs::module::<Gitish>(),
    programs::module::<Kvish>(),
    programs::module::<Luaish>(),
    programs::module::<Opaque>(),
    programs::module::<Plain>(),
    programs::module::<Stray>(),
    programs::module::<Tmuxish>(),
];

// ---------------------------------------------------------------------------------- helpers ---

const HEAD: &str = "[home]\nversion = \"1\"\n";

#[derive(Default)]
struct NoFetch {
    requests: Mutex<Vec<String>>,
}

impl Fetcher for NoFetch {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.requests.lock().unwrap().push(url.to_string());
        Err(FetchError::NotFound(url.to_string()))
    }

    fn requests(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

fn roots(env: &HomeEnv) -> Roots {
    let home = OsString::from(env.home());
    let config = OsString::from(env.config());
    let store = OsString::from(env.data().join("lodi"));
    Roots::from_vars(
        Some(home.as_os_str()),
        Some(config.as_os_str()),
        None,
        Some(store.as_os_str()),
    )
    .unwrap()
}

fn write_manifest(env: &HomeEnv, text: &str) {
    let dir = env.config().join("lodi");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("home.toml"), text).unwrap();
}

fn load(roots: &Roots, text: &str) -> Result<HomeManifest, Vec<Diagnostic>> {
    parse_home_manifest_in(
        text,
        "home.toml",
        &roots.config_root(),
        &RenderEnv::of(roots),
        REGISTRY,
    )
    .map_err(|e| e.diagnostics)
}

fn refused(text: &str) -> Vec<Diagnostic> {
    let env = home_env("programs-refused");
    match load(&roots(&env), text) {
        Ok(_) => panic!("accepted:\n{text}"),
        Err(ds) => ds,
    }
}

fn codes(text: &str) -> Vec<&'static str> {
    refused(text).iter().map(|d| d.code).collect()
}

fn options() -> Options {
    Options::default()
}

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

// ---------------------------------------------------------------------------------- writers ---

/// One golden per writer: the manifest that exercises it, the home-relative file it renders
/// and the committed bytes that file must have.
const GOLDENS: &[(&str, &str, &str)] = &[
    (
        "demo.toml",
        ".config/demo/config.toml",
        r##"[programs.demo]
theme = "dark"
font.size = 12
extra = '''
[window]
opacity = 0.9
[settings.nested]
deep = true
'''
[programs.demo.settings]
z = 1
a = [1, 2.5, "three"]
"key with space" = "quote \" and backslash \\ and tab \t"
when = 1979-05-27T07:32:00Z
"##,
    ),
    (
        "gitish.gitconfig",
        ".config/gitish/config",
        r##"[programs.gitish]
user.name = "  A. Tester ; # not a comment  "
core.editor = "vi -c \"set tw=72\" \\ end"
aliases = { lg = "log --graph\tall", st = "status" }
branches = { main = "origin", 'we"ird\' = "up" }
extra = """
[pull]
	rebase = true
"""
"##,
    ),
    (
        "kvish.conf",
        ".config/kvish/kvish.conf",
        r##"[programs.kvish]
settings = { font_family = "Fira Code", font_size = "12", background_opacity = "0.9" }
extra = "include local.conf\n"
"##,
    ),
    (
        "tmuxish.conf",
        ".config/tmuxish/tmux.conf",
        r##"[programs.tmuxish]
mouse = true
history_limit = 50000
status_left = "#S $HOME ~"
status_right = "it's \"$USER\" at ~ \\ %H:%M"
extra = "bind r source-file ~/.config/tmuxish/tmux.conf\n"
"##,
    ),
    (
        "luaish.lua",
        ".config/luaish/init.lua",
        r##"[programs.luaish]
extra = "vim.g.mapleader = \" \"\n"
[programs.luaish.options]
number = true
tabstop = 4
scrolloff = 2.5
listchars = { tab = "» ", trail = "·", "not-an-ident" = "x", end = "e" }
shortmess = "quote \" newline \n bell \u0007"
wildignore = ["*.o", "*.a"]
"##,
    ),
    (
        "opaque.txt",
        ".opaquerc",
        "[programs.opaque]\nextra = \"line one\\r\\n\\ttabbed  \\u0001 no final newline\"\n",
    ),
];

fn golden_path(name: &str) -> PathBuf {
    repo().join("tests/fixtures/programs").join(name)
}

#[test]
fn every_writer_renders_its_committed_golden() {
    let env = home_env("programs-golden");
    let roots = roots(&env);
    for (name, rel, table) in GOLDENS {
        let manifest =
            load(&roots, &format!("{HEAD}{table}")).unwrap_or_else(|e| panic!("{name}: {e:?}"));
        let entry = &manifest.files[*rel];
        let written = &entry.bytes;
        if std::env::var_os("LODI_RECORD_EXPECTED").is_some() {
            fs::write(golden_path(name), written).unwrap();
        }
        let expected = fs::read(golden_path(name)).unwrap_or_else(|e| {
            panic!(
                "tests/fixtures/programs/{name} is the committed expectation and must exist: {e}"
            )
        });
        assert_eq!(
            String::from_utf8_lossy(written),
            String::from_utf8_lossy(&expected),
            "{name}: the rendered bytes are not the committed golden"
        );
        assert_eq!(written, &expected, "{name}");
        let module = name.split('.').next().unwrap();
        assert_eq!(entry.origin, Origin::Program(module.to_string()));
        assert_eq!(entry.table, format!("programs.{module}"));
        assert_eq!(entry.mode, 0o644, "no module has a mode key");
    }
}

#[test]
fn the_opaque_extra_is_appended_byte_exact() {
    let expected = b"line one\r\n\ttabbed  \x01 no final newline";
    let bytes = fs::read(golden_path("opaque.txt")).unwrap();
    assert!(bytes.ends_with(expected), "{bytes:?}");
    let header = programs::header(Comment::Hash, "opaque");
    assert_eq!(&bytes[..header.len()], header.as_bytes());
}

/// I3, I12: the header names the table and nothing else, and no golden carries a date, a
/// version, a user, a host or an absolute path.
#[test]
fn a_rendered_file_carries_nothing_of_the_machine() {
    for (name, _, _) in GOLDENS {
        let text = String::from_utf8_lossy(&fs::read(golden_path(name)).unwrap()).into_owned();
        let module = name.split('.').next().unwrap();
        let comment = if module == "luaish" {
            Comment::DoubleDash
        } else {
            Comment::Hash
        };
        assert!(
            text.starts_with(&programs::header(comment, module)),
            "{name}: {text}"
        );
        let head: String = text.lines().take(2).collect::<Vec<_>>().join("\n");
        for needle in ["/home/", "/tmp", "/nix", env!("CARGO_PKG_VERSION"), "2026"] {
            assert!(!head.contains(needle), "{name} header names `{needle}`");
        }
        assert!(!text.contains(env!("CARGO_TARGET_TMPDIR")), "{name}");
    }
}

/// I1 and I3: the bytes are a function of the declared values alone — not of the order they
/// are written in, and not of the home directory they render below.
#[test]
fn rendering_is_independent_of_key_order_and_of_the_machine() {
    let one = home_env("programs-order-one");
    let two = home_env("programs-order-two");
    let (roots_one, roots_two) = (roots(&one), roots(&two));
    let forward = format!(
        "{HEAD}[programs.demo]\ntheme = \"light\"\n[programs.demo.settings]\nz = 1\na = [1, 2]\n\
         [programs.demo.settings.t]\nk = true\n[programs.luaish.options]\nb = 1\na = 2\n"
    );
    let backward = format!(
        "{HEAD}[programs.luaish.options]\na = 2\nb = 1\n[programs.demo.settings.t]\nk = true\n\
         [programs.demo.settings]\na = [1, 2]\nz = 1\n[programs.demo]\ntheme = \"light\"\n"
    );
    let a = load(&roots_one, &forward).unwrap();
    let b = load(&roots_two, &backward).unwrap();
    let c = load(&roots_one, &forward).unwrap();
    assert_eq!(a.files, b.files);
    assert_eq!(a.files, c.files, "rendering twice gives the same bytes");
}

// ---------------------------------------------------------------------------------- loading ---

#[test]
fn the_common_keys_are_the_loaders_and_never_the_modules() {
    let env = home_env("programs-common");
    let m = load(
        &roots(&env),
        &format!(
            "{HEAD}[programs.demo]\nenable = true\nbackup = false\non_remove = \"delete\"\n\
             theme = \"dark\"\n"
        ),
    )
    .unwrap();
    let e = &m.files[".config/demo/config.toml"];
    assert_eq!((e.backup, e.on_remove), (false, OnRemove::Delete));
    assert_eq!(m.programs[0].name, "demo");
    assert_eq!(m.programs[0].paths, [".config/demo/config.toml"]);
}

#[test]
fn a_disabled_table_renders_nothing_but_is_still_validated() {
    let env = home_env("programs-disabled");
    let m = load(
        &roots(&env),
        &format!("{HEAD}[programs.demo]\nenable = false\ntheme = \"dark\"\n"),
    )
    .unwrap();
    assert!(m.files.is_empty() && m.programs.is_empty());
    assert_eq!(
        codes(&format!(
            "{HEAD}[programs.demo]\nenable = false\nthme = \"dark\"\n"
        )),
        ["E_UNKNOWN_ATTR"]
    );
}

#[test]
fn an_undeclared_key_is_unknown_at_its_line_and_column_with_the_nearest_key() {
    let ds = refused(&format!("{HEAD}[programs.demo]\nthme = \"dark\"\n"));
    assert_eq!(ds.len(), 1, "{ds:?}");
    let text = ds[0].to_string();
    assert_eq!(ds[0].code, "E_UNKNOWN_ATTR");
    assert!(text.contains("home.toml:4:1"), "{text}");
    assert!(text.contains("did you mean `theme`?"), "{text}");
    // A key below a table read by dotted path, too.
    let ds = refused(&format!("{HEAD}[programs.demo]\nfont.sise = 12\n"));
    assert_eq!(ds[0].code, "E_UNKNOWN_ATTR");
    assert!(ds[0].to_string().contains("font.size"), "{}", ds[0]);
    // No module has `mode`.
    assert_eq!(
        codes(&format!("{HEAD}[programs.demo]\nmode = \"0600\"\n")),
        ["E_UNKNOWN_ATTR"]
    );
}

#[test]
fn a_wrong_type_or_range_is_a_type_error_at_the_value() {
    let ds = refused(&format!("{HEAD}[programs.demo]\nfont.size = 100\n"));
    assert_eq!(ds[0].code, "E_TYPE");
    let text = ds[0].to_string();
    assert!(text.contains("home.toml:4:13"), "{text}");
    assert!(text.contains("from 6 to 72"), "{text}");

    let ds = refused(&format!("{HEAD}[programs.demo]\ntheme = 3\n"));
    assert_eq!(ds[0].code, "E_TYPE");
    assert!(ds[0].to_string().contains("home.toml:4:9"), "{}", ds[0]);

    let ds = refused(&format!("{HEAD}[programs.demo]\ntheme = \"blue\"\n"));
    assert_eq!(ds[0].code, "E_TYPE");
    assert!(ds[0].to_string().contains("dark, light"), "{}", ds[0]);

    for common in [
        "enable = \"yes\"",
        "backup = 1",
        "on_remove = \"burn\"",
        "extra = 3",
    ] {
        assert_eq!(
            codes(&format!("{HEAD}[programs.demo]\n{common}\n")),
            ["E_TYPE"],
            "{common}"
        );
    }
    // A module's own refusal: git cannot store a control character.
    assert_eq!(
        codes(&format!(
            "{HEAD}[programs.gitish]\nuser.name = \"a\\u0001b\"\n"
        )),
        ["E_TYPE"]
    );
}

#[test]
fn a_toml_extra_that_does_not_parse_is_a_syntax_error_inside_the_string() {
    let ds = refused(&format!(
        "{HEAD}[programs.demo]\nextra = '''\nok = 1\nbad =\n'''\n"
    ));
    assert_eq!(ds.len(), 1, "{ds:?}");
    assert_eq!(ds[0].code, "E_SYNTAX");
    assert!(ds[0].to_string().contains("home.toml:6:"), "{}", ds[0]);
}

#[test]
fn a_key_set_both_typed_and_in_extra_is_a_conflict() {
    let ds = refused(&format!(
        "{HEAD}[programs.demo]\ntheme = \"dark\"\nextra = \"theme = 'light'\"\n"
    ));
    assert_eq!(ds[0].code, "E_ATTR_CONFLICT");
    assert!(ds[0].to_string().contains("theme"), "{}", ds[0]);
    // Below a table: merged where the keys differ, refused where one repeats.
    let env = home_env("programs-merge");
    let m = load(
        &roots(&env),
        &format!("{HEAD}[programs.demo]\nfont.size = 10\nextra = \"font.family = 'mono'\"\n"),
    )
    .unwrap();
    let text = String::from_utf8(m.files[".config/demo/config.toml"].bytes.clone()).unwrap();
    assert!(
        text.ends_with("[font]\nfamily = \"mono\"\nsize = 10\n"),
        "{text}"
    );
    assert_eq!(
        codes(&format!(
            "{HEAD}[programs.demo]\nfont.size = 10\nextra = \"font.size = 11\"\n"
        )),
        ["E_ATTR_CONFLICT"]
    );
}

#[test]
fn an_extra_no_file_takes_is_unknown() {
    assert_eq!(
        codes(&format!("{HEAD}[programs.plain]\nextra = \"x\"\n")),
        ["E_UNKNOWN_ATTR"]
    );
}

#[test]
fn an_unknown_module_names_the_nearest_and_the_list() {
    let ds = refused(&format!("{HEAD}[programs.dmeo]\n"));
    assert_eq!(ds[0].code, "E_UNKNOWN_BLOCK");
    let text = ds[0].to_string();
    assert!(text.contains("`demo`"), "{text}");
    assert!(text.contains("gitish"), "{text}");
}

#[test]
fn a_module_that_renders_outside_home_is_refused() {
    assert_eq!(codes(&format!("{HEAD}[programs.stray]\n")), ["E_CONFIG"]);
}

#[test]
fn a_rendered_path_declared_again_is_a_duplicate_naming_both() {
    let ds = refused(&format!(
        "{HEAD}[home.xdg_config.\"demo/config.toml\"]\ntext = \"b\"\n[programs.demo]\n"
    ));
    assert_eq!(ds[0].code, "E_DUP_RESOURCE");
    let text = ds[0].to_string();
    assert!(
        text.contains("home.xdg_config") && text.contains("programs.demo"),
        "{text}"
    );
}

#[test]
fn a_program_rendering_below_an_xdg_config_home_outside_home_is_refused() {
    let env = home_env("programs-xdg");
    let outside = env.root().join("xdg-outside");
    fs::create_dir_all(outside.join("lodi")).unwrap();
    let home = OsString::from(env.home());
    let roots = Roots::from_vars(
        Some(home.as_os_str()),
        Some(outside.as_os_str()),
        None,
        None,
    )
    .unwrap();
    let ds = load(&roots, &format!("{HEAD}[programs.demo]\n")).unwrap_err();
    assert_eq!(ds[0].code, "E_CONFIG");
    assert!(ds[0].to_string().contains("XDG_CONFIG_HOME"), "{}", ds[0]);
}

// -------------------------------------------------------------------------------- lifecycle ---

/// A plan step as `(verb, path, note)`.
type StepLine = (Verb, String, Option<String>);

fn ok_plan(roots: &Roots) -> (Vec<StepLine>, Vec<String>) {
    let plan = plan_with(roots, REGISTRY).unwrap_or_else(|e| panic!("{e:?}"));
    let steps = plan
        .steps
        .iter()
        .map(|s| (s.verb, s.path.clone(), s.note.clone()))
        .collect();
    (steps, shadow_only(&plan.warnings))
}

/// The `W_SHADOWED` lines of a run. A test module names no real executable, so every run also
/// prints `W_PROGRAM_NOT_FOUND` for it, which depends on the test process's own `PATH`;
/// `tests/home_programs_shipped.rs` proves that warning with a `PATH` it controls.
fn shadow_only(warnings: &[String]) -> Vec<String> {
    warnings
        .iter()
        .filter(|w| !w.contains("W_PROGRAM_NOT_FOUND"))
        .cloned()
        .collect()
}

fn stamp(path: &Path) -> (u64, i64, i64) {
    let m = fs::metadata(path).unwrap();
    (m.ino(), m.mtime(), m.mtime_nsec())
}

/// I4, I8, I13: a rendered file is created, left alone when nothing changed, and a hand edit
/// is drift at exit 8 — with `W_SHADOWED` on plan, apply and status, and the exit unchanged.
#[test]
fn a_rendered_file_is_managed_like_any_other() {
    let env = home_env("programs-life");
    let roots = roots(&env);
    write_manifest(&env, &format!("{HEAD}[programs.demo]\ntheme = \"dark\"\n"));
    fs::write(env.home().join(".demorc"), "old\n").unwrap();
    let shadowed = "lodi: warning W_SHADOWED: .demorc exists, and demo reads it before or \
                    alongside .config/demo/config.toml; move its content into [programs.demo] \
                    and delete it";

    let (steps, warnings) = ok_plan(&roots);
    assert_eq!(
        steps,
        [(
            Verb::Create,
            ".config/demo/config.toml".to_string(),
            Some("programs.demo".to_string())
        )]
    );
    assert_eq!(warnings, [shadowed]);

    let out = apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY)
        .unwrap_or_else(|f| panic!("{}", f.text));
    assert_eq!(shadow_only(&out.warnings), [shadowed]);
    let file = env.home().join(".config/demo/config.toml");
    let bytes = fs::read(&file).unwrap();
    assert_eq!(
        bytes,
        fs::read(golden_like(&roots, "theme = \"dark\"")).unwrap()
    );
    assert_eq!(fs::read(env.home().join(".demorc")).unwrap(), b"old\n");

    // Unchanged: the re-apply writes nothing, not even the state.
    let state = env.data().join("lodi/home-scope/state.json");
    let before = (stamp(&file), stamp(&state));
    let (steps, _) = ok_plan(&roots);
    assert_eq!(steps[0].0, Verb::Unchanged);
    apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY).unwrap();
    assert_eq!((stamp(&file), stamp(&state)), before);

    let status = status_with(&roots, REGISTRY).unwrap_or_else(|f| panic!("{}", f.text));
    assert!(
        status.lines.iter().any(|l| l.starts_with("ok ")),
        "{status:?}"
    );
    assert_eq!(shadow_only(&status.warnings), [shadowed]);

    // A hand edit is drift: E_DRIFT at exit 8, and the edit stays.
    fs::write(&file, "hand\n").unwrap();
    let failure = apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY).unwrap_err();
    assert_eq!((failure.code, failure.status), ("E_DRIFT", 8));
    assert!(
        failure.warnings.iter().any(|w| w.contains("W_DRIFT")),
        "{failure:?}"
    );
    assert_eq!(fs::read(&file).unwrap(), b"hand\n");
    let status = status_with(&roots, REGISTRY).unwrap();
    assert!(
        status.lines.iter().any(|l| l.starts_with("drift ")),
        "{status:?}"
    );

    // No shadowing file, no warning.
    fs::remove_file(env.home().join(".demorc")).unwrap();
    assert!(ok_plan(&roots).1.is_empty());
}

/// The home part of the step list (#694): tools, files and user services, in that order, each
/// begun and finished, and every file written counted as an item of the files step.
#[test]
fn a_home_apply_reports_its_tools_files_and_user_services_steps() {
    struct Recording(Vec<Event>);
    impl Sink for Recording {
        fn event(&mut self, event: Event) {
            self.0.push(event);
        }
    }
    let env = home_env("programs-steps");
    let roots = roots(&env);
    write_manifest(&env, &format!("{HEAD}[programs.demo]\ntheme = \"dark\"\n"));
    let mut sink = Recording(Vec::new());
    apply_reporting(
        &roots,
        &options(),
        &NoFetch::default(),
        0,
        REGISTRY,
        &mut sink,
    )
    .unwrap_or_else(|f| panic!("{}", f.text));
    let begin = |name: &str, expected: Option<usize>| Event::Begin {
        name: name.to_string(),
        expected,
    };
    let finish = Event::Finish {
        count: None,
        detail: None,
    };
    assert_eq!(
        sink.0,
        [
            begin("Tools", None),
            finish.clone(),
            begin("Files", Some(1)),
            Event::Item {
                name: ".config/demo/config.toml".to_string()
            },
            finish.clone(),
            begin("User services", None),
            finish,
        ]
    );
    assert_eq!(
        lodi::home::apply::STEPS,
        ["Tools", "Files", "User services"]
    );
}

/// What the demo module renders for `keys`, written beside the scratch roots for comparison.
fn golden_like(roots: &Roots, keys: &str) -> PathBuf {
    let m = load(roots, &format!("{HEAD}[programs.demo]\n{keys}\n")).unwrap();
    let at = roots.home().parent().unwrap().join("expected-demo.toml");
    fs::write(&at, &m.files[".config/demo/config.toml"].bytes).unwrap();
    at
}

/// I9: the first unmanaged copy is backed up once, and restored when the table goes.
#[test]
fn a_rendered_file_backs_up_once_and_restores() {
    let env = home_env("programs-restore");
    let roots = roots(&env);
    let file = env.home().join(".config/demo/config.toml");
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(&file, "mine\n").unwrap();
    write_manifest(
        &env,
        &format!("{HEAD}[programs.demo]\non_remove = \"restore\"\ntheme = \"dark\"\n"),
    );
    let (steps, _) = ok_plan(&roots);
    // Over a file Lodi did not write, a 1.1 table plans `create` with the backup note (§4.3).
    assert_eq!(steps[0].0, Verb::Create);
    assert_eq!(
        steps[0].2.as_deref(),
        Some("programs.demo; backup: first unmanaged copy kept")
    );
    apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY).unwrap();
    assert_ne!(fs::read(&file).unwrap(), b"mine\n");

    // A changed declaration updates Lodi's own bytes with no second backup (§4.4).
    write_manifest(
        &env,
        &format!("{HEAD}[programs.demo]\non_remove = \"restore\"\ntheme = \"light\"\n"),
    );
    let (steps, _) = ok_plan(&roots);
    assert_eq!(
        (steps[0].0, steps[0].2.as_deref()),
        (Verb::Update, Some("programs.demo"))
    );
    apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY).unwrap();

    write_manifest(&env, HEAD);
    let (steps, _) = ok_plan(&roots);
    assert_eq!(steps[0].0, Verb::Restore);
    apply_with(&roots, &options(), &NoFetch::default(), 0, REGISTRY).unwrap();
    assert_eq!(fs::read(&file).unwrap(), b"mine\n");
}

// ------------------------------------------------------------------------------- the binary ---

/// The binary ships `bash`, `fish`, `git` and `zsh` (M-Home pa-1) and `alacritty`, `helix`,
/// `kitty`, `nvim`, `starship` and `tmux` (pb-1): any other `[programs]` table is
/// an unknown block naming the nearest module and the shipped list.
#[test]
fn the_shipped_binary_names_its_modules() {
    let names: Vec<&str> = programs::registry().iter().map(|m| m.name).collect();
    assert_eq!(
        names,
        [
            "alacritty",
            "bash",
            "fish",
            "git",
            "helix",
            "kitty",
            "nvim",
            "starship",
            "tmux",
            "zsh"
        ]
    );
    let env = home_env("programs-binary");
    write_manifest(&env, &format!("{HEAD}[programs.gti]\nenable = true\n"));
    let out = env.lodi(&["home", "plan"]).output().unwrap();
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert_eq!(out.status.code(), Some(3), "{stderr}");
    assert!(stderr.contains("E_UNKNOWN_BLOCK"), "{stderr}");
    assert!(
        stderr.contains(
            "the modules of this build are: alacritty, bash, fish, git, helix, kitty, nvim, \
             starship, tmux, zsh"
        ),
        "{stderr}"
    );
    assert!(stderr.contains("did you mean `git`?"), "{stderr}");
}

// ---------------------------------------------------------------------------------- purity ---

/// I1, I2: a module and a writer spawn nothing, read no environment, touch no file and read no
/// clock. The lint reads the source of `src/home/programs` and `src/home/render`.
#[test]
fn modules_and_writers_are_pure_by_construction() {
    let root = repo().join("src/home");
    let forbidden = [
        "Command",
        "std::env",
        "env::var",
        "fsops",
        "std::fs",
        "fs::",
        "File::",
        "SystemTime",
        "Instant",
        "process::",
        "std::io",
        "std::net",
        "rand",
    ];
    let mut seen = 0;
    for dir in ["programs", "render"] {
        let mut stack = vec![root.join(dir)];
        while let Some(at) = stack.pop() {
            for entry in fs::read_dir(&at).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    stack.push(path);
                    continue;
                }
                if path.extension() != Some(OsStr::new("rs")) {
                    continue;
                }
                seen += 1;
                let text = fs::read_to_string(&path).unwrap();
                for word in forbidden {
                    assert!(
                        !text.contains(word),
                        "{} names `{word}`: a module is a pure function (I2)",
                        path.display()
                    );
                }
            }
        }
    }
    assert!(seen >= 2);
}
