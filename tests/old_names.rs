//! lodi 2.0 (#699): a 1.x command name stops with its replacement and runs nothing.
//!
//! Each name is its own case, against the real binary: a scratch `HOME` with a config at
//! `~/.config/lodi`, XDG state, a scratch `--root` with a passwd under `LODI_HOST_REQUIRE_ROOT=1`,
//! a current folder holding a project, and recording stub elevators as the whole `PATH`. Every
//! form — the name alone, with arguments after it, `lodi help NAME` and `NAME --help` — exits
//! with the usage status, prints nothing on standard output, names the old command and its
//! replacement on standard error, calls no elevator and changes no byte of the scratch.

mod support;

use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A scratch machine: root, home, state and a project folder, below one directory.
struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Scratch {
        let dir = support::scratch(&format!("old-names-{tag}"));
        let scratch = Scratch { dir };
        scratch.write(
            "root/etc/passwd",
            "root:x:0:0::/root:/bin/sh\nsample:x:1000:1000::/home:/bin/sh\n",
        );
        scratch.write("root/etc/hostname", "box\n");
        scratch.write("home/.config/lodi/host.toml", "[host]\nversion = \"1\"\n");
        scratch.write("home/.config/lodi/home.toml", "[home]\nversion = \"1\"\n");
        scratch.write("work/lodi.toml", "[project]\nname = \"p\"\n");
        fs::create_dir_all(scratch.dir.join("state")).unwrap();
        fs::create_dir_all(scratch.dir.join("bin")).unwrap();
        for elevator in ["sudo", "doas", "run0"] {
            let shim = scratch.dir.join("bin").join(elevator);
            fs::write(
                &shim,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' {elevator} >> '{}'\nexit 1\n",
                    scratch.dir.join("elevator-calls").display()
                ),
            )
            .unwrap();
            fs::set_permissions(&shim, fs::Permissions::from_mode(0o755)).unwrap();
        }
        scratch
    }

    fn write(&self, rel: &str, text: &str) {
        let path = self.dir.join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn root(&self) -> String {
        self.dir.join("root").display().to_string()
    }

    // check-host-safety: refusal — every invocation names this case's scratch root as --root.
    fn lodi(&self, args: &[&str]) -> Output {
        let home = self.dir.join("home");
        Command::new(env!("CARGO_BIN_EXE_lodi"))
            .args(args)
            .current_dir(self.dir.join("work"))
            .env_clear()
            .env("PATH", self.dir.join("bin"))
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", home.join(".config"))
            .env("XDG_STATE_HOME", self.dir.join("state"))
            .env("LODI_HOME", self.dir.join("store"))
            .env("LODI_HOST_REQUIRE_ROOT", "1")
            .output()
            .expect("the lodi binary runs")
    }

    /// Every path below the scratch with its mode and contents.
    fn snapshot(&self) -> BTreeMap<PathBuf, (u32, Vec<u8>)> {
        let mut out = BTreeMap::new();
        walk(&self.dir, &mut out);
        out
    }
}

fn walk(dir: &Path, out: &mut BTreeMap<PathBuf, (u32, Vec<u8>)>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        let mode = meta.permissions().mode();
        if meta.is_dir() {
            out.insert(path.clone(), (mode, Vec::new()));
            walk(&path, out);
        } else {
            out.insert(path.clone(), (mode, fs::read(&path).unwrap()));
        }
    }
}

/// `words` in every form, each refused the same way, naming `lodi OLD` and every `replacement`.
fn stops(tag: &str, words: &[&str], trailing: &[&str], replacement: &[&str]) {
    let scratch = Scratch::new(tag);
    let root = scratch.root();
    let before = scratch.snapshot();
    let old = format!("`lodi {}`", words.join(" "));
    let with_root = [words, trailing, &["--root", &root]].concat();
    let forms: Vec<Vec<&str>> = vec![
        [words, &["--root", &root]].concat(),
        with_root.clone(),
        [&["help"], words].concat(),
        [words, &["--help"]].concat(),
        [&with_root[..], &["-h"]].concat(),
    ];
    let mut first = None;
    for form in forms {
        let output = scratch.lodi(&form);
        let text = String::from_utf8_lossy(&output.stderr).into_owned();
        assert_eq!(output.status.code(), Some(2), "{form:?}: {text}");
        assert!(output.stdout.is_empty(), "{form:?} printed on stdout");
        assert!(text.contains(&old), "{form:?} does not name {old}: {text}");
        for want in replacement {
            assert!(text.contains(want), "{form:?} does not name {want}: {text}");
        }
        assert!(!text.contains("unsupported"), "{form:?}: {text}");
        assert_eq!(
            text,
            *first.get_or_insert_with(|| text.clone()),
            "{form:?} is refused differently"
        );
    }
    assert!(
        !scratch.dir.join("elevator-calls").exists(),
        "an old name ran an elevator"
    );
    assert_eq!(
        scratch.snapshot(),
        before,
        "an old name changed the scratch"
    );
}

macro_rules! old_name {
    ($test:ident, $words:expr, $trailing:expr, $replacement:expr) => {
        #[test]
        fn $test() {
            stops(stringify!($test), &$words, &$trailing, &$replacement);
        }
    };
}

old_name!(plan, ["plan"], ["./cfg"], ["`lodi switch --dry-run`"]);
old_name!(apply, ["apply"], ["./cfg", "--yes"], ["`lodi switch`"]);
// check-host-safety: refusal — the old name stops; every form carries --root.
old_name!(host_arm, ["host", "arm"], [], ["`lodi import`"]);
old_name!(
    home_init,
    ["home", "init"],
    ["--stdout"],
    ["`lodi import --home`"]
);
old_name!(
    home_import,
    ["home", "import"],
    ["--force"],
    ["`lodi import --home`"]
);
old_name!(boot, ["boot"], [], ["`lodi switch`", "at once"]);
old_name!(
    boot_confirm,
    ["boot", "confirm"],
    ["now"],
    ["`lodi switch`", "at once"]
);
old_name!(
    lock,
    ["lock"],
    ["--check"],
    ["`lodi develop`", "`lodi run`", "`lodi update`"]
);
old_name!(
    trust,
    ["trust"],
    ["--revoke"],
    [
        "`lodi develop`",
        "`lodi run`",
        "`--trust`",
        "`LODI_TRUST=1`"
    ]
);
old_name!(info, ["info"], ["jq"], ["`lodi search TOOL`"]);
old_name!(
    host_import,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "import"],
    ["--stdout"],
    ["`lodi import`"]
);
old_name!(
    host_plan,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "plan"],
    ["./cfg"],
    ["`lodi switch --dry-run`"]
);
old_name!(
    host_apply,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "apply"],
    ["--no-update"],
    ["`lodi switch`"]
);
old_name!(
    host_versions,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "versions"],
    ["curl"],
    ["`lodi pin PKG`"]
);
old_name!(
    host_pin,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "pin"],
    ["curl", "--to", "1"],
    ["`lodi pin PKG --to V`"]
);
old_name!(
    host_unpin,
    // check-host-safety: refusal — the old name stops; every form carries --root.
    ["host", "unpin"],
    ["--all"],
    ["`lodi unpin PKG`"]
);
old_name!(
    host,
    ["host"],
    ["status"],
    [
        "`lodi import`",
        "`lodi switch`",
        "`lodi pin PKG`",
        "`lodi unpin PKG`"
    ]
);
old_name!(
    home_plan,
    ["home", "plan"],
    ["./cfg"],
    ["`lodi switch --dry-run`", "`lodi switch --home --dry-run`"]
);
old_name!(
    home_apply,
    ["home", "apply"],
    ["--locked"],
    ["`lodi switch --home`"]
);
old_name!(
    home_status,
    ["home", "status"],
    ["./cfg"],
    ["`lodi switch --dry-run`", "`lodi switch --home --dry-run`"]
);
old_name!(
    home,
    ["home"],
    ["frobnicate"],
    [
        "`lodi import --home`",
        "`lodi switch --home`",
        "`lodi switch --home --dry-run`"
    ]
);

/// The 1.x command forms no message may name (LD-523), and the 1.x wording that calls a step of
/// `lodi switch` a command of its own (LD-529): a hint names the 2.0 command instead.
const OLD_FORMS: &[&str] = &[
    "lodi host ",
    "lodi home ",
    "lodi plan",
    "lodi apply",
    "--host NAME",
    "apply again",
    "plan again",
    "the apply",
    "this apply",
    "last apply",
    "next apply",
    "apply leaves",
    "have apply",
    "apply lock",
    "apply the checkout",
    "apply with --refresh",
    "apply this manifest",
    "URL Lodi applies",
    "a plan of",
];

/// Where `src/` may still spell an [`OLD_FORMS`] form, as `(file below src/, text on the line,
/// why)`. Each is a 2.0 form that only looks old.
const ALLOWED: &[(&str, &str, &str)] = &[
    (
        "main.rs",
        "--host NAME",
        "update, pin and unpin take --host NAME",
    ),
    (
        "config.rs",
        "choose one with --host NAME",
        "update, pin and unpin take --host NAME",
    ),
];

/// Every `.rs` file below `dir`, sorted.
fn sources(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            sources(&path, out);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
}

/// The lines of `text` a person can be shown, numbered from 1: no comment line and nothing of
/// an item under `#[cfg(test)]`.
fn shown_lines(text: &str) -> Vec<(usize, &str)> {
    let mut shown = Vec::new();
    let mut skipping = false;
    let mut depth: Option<i64> = None;
    for (index, line) in text.lines().enumerate() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("#[cfg(test)]") {
            skipping = true;
            depth = None;
            continue;
        }
        if skipping {
            let change = line.matches('{').count() as i64 - line.matches('}').count() as i64;
            let now = depth.map_or(change, |d| d + change);
            if depth.is_some() || line.contains('{') {
                depth = Some(now);
            }
            if depth.is_some_and(|d| d <= 0) || (depth.is_none() && trimmed.ends_with(';')) {
                skipping = false;
            }
            continue;
        }
        if !trimmed.starts_with("//") {
            shown.push((index + 1, line));
        }
    }
    shown
}

/// Whether `line` names `form` as a command, not as the start of a longer word.
fn names(line: &str, form: &str) -> bool {
    line.match_indices(form).any(|(at, _)| {
        line[at + form.len()..]
            .chars()
            .next()
            .is_none_or(|next| form.ends_with(' ') || !next.is_alphanumeric())
    })
}

/// No message in `src/` names a 1.x command form but where [`ALLOWED`] says why (LD-523), and
/// every allowance still matches a line, so the list shrinks as the 1.x code goes.
#[test]
fn no_message_names_a_1x_command_form() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    sources(&root, &mut files);
    let mut used = vec![false; ALLOWED.len()];
    let mut named = Vec::new();
    for path in &files {
        let file = path.strip_prefix(&root).unwrap().display().to_string();
        let text = fs::read_to_string(path).unwrap();
        for (number, line) in shown_lines(&text) {
            let Some(form) = OLD_FORMS.iter().find(|form| names(line, form)) else {
                continue;
            };
            let allowed = ALLOWED
                .iter()
                .position(|(at, needle, _)| *at == file && line.contains(needle));
            match allowed {
                Some(index) => used[index] = true,
                None => named.push(format!("src/{file}:{number}: {form:?} in {}", line.trim())),
            }
        }
    }
    assert!(
        named.is_empty(),
        "1.x forms in messages:\n{}",
        named.join("\n")
    );
    let stale: Vec<_> = ALLOWED
        .iter()
        .zip(&used)
        .filter(|(_, used)| !**used)
        .map(|((file, needle, _), _)| format!("{file}: {needle:?}"))
        .collect();
    assert!(
        stale.is_empty(),
        "allowances that match nothing:\n{}",
        stale.join("\n")
    );
}
