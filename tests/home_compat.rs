//! I10: a 1.0 home manifest plans, applies and reports exactly as Lodi 1.0.0 did (M-Home,
//! `docs/design/HOME_PROGRAMS.md` §10).
//!
//! Scripted walks over every committed 1.0 home fixture (`tests/fixtures/home/several`, `errors`
//! and the import bundle `import/full`): a fresh plan, an apply and the plan after it, a hand
//! edit, removed entries, an unmanaged file replaced and then restored, paths that already hold
//! exactly the declared bytes, and the import bundle populated in a scratch configuration root.
//! Each command's exit status, standard output and standard error go into a transcript, and the
//! transcripts of **this** build must equal, byte for byte, the ones committed under
//! `tests/fixtures/home/golden-1.0.0/` — which were recorded from the 1.0.0 binary by these same
//! walks. There is no exception list: no case is left out, and the one substitution is the
//! running binary's own version in `E_VERSION`'s "and this is lodi <version>" (the golden names
//! 1.0.0, the binary that recorded it; this build names its own), which `check` writes back as
//! 1.0.0 before comparing (LD-343). Every other byte is compared as recorded, save one line: from
//! 1.2 a manifest that uses `[files]` prints one `W_DEPRECATED` line first on standard error
//! (decision D5, LD-324), which 1.0.0 did not print. Every run whose manifest loads must open its
//! standard error with exactly that line, once, and `run` takes it off before the transcript is
//! written; a manifest refused at exit 3 prints its errors only, as it did. From 1.5 the errors
//! that cited the design say the same in plain words (LD-463): [`REWORDED`] names each such line
//! as 1.0.0 printed it and as this build prints it, `check` writes it back as 1.0.0 printed it,
//! and every entry must be a line of a golden, so the list names each change and hides nothing.
//! Codes, places, exit statuses and every other byte are compared as recorded.
//!
//! Recording (never a validation command): a `git worktree` of the tag `v1.0.0` outside the
//! repository, built with the repository's toolchain, then
//! `LODI_RECORD_EXPECTED=1 LODI_COMPAT_BINARY=<that build's lodi> cargo test --locked --test
//! home_compat`, and the worktree removed. The walk runs the binary with an emptied environment,
//! scratch `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and `LODI_HOME`, and `LODI_FETCH_REWRITE`
//! at a closed port — the recipe of `tests/fixtures/schemas/README.md`. The same run writes the
//! state record 1.0.0 left after the apply to `tests/fixtures/schemas/1.0.0/home-state.json`,
//! the specimen the last step here reads back with this build. The scratch root is written as
//! `<root>` in a transcript, so no machine path is committed.
//!
//! Offline and deterministic. No network, no Podman, no clock.

mod support;

use std::ffi::OsString;
use std::fmt::Write as _;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use support::{HomeEnv, home_env};

const GOLDEN: &str = "tests/fixtures/home/golden-1.0.0";
const SPECIMEN: &str = "tests/fixtures/schemas/1.0.0/home-state.json";
/// The version of the binary that recorded the goldens.
const RECORDED: &str = "1.0.0";
/// The line every run over a `[files]` manifest prints first from 1.2 (decision D5, LD-324), and
/// the one line of this build's transcripts the 1.0.0 goldens do not hold.
const FILES_WARNING: &str = "lodi: warning W_DEPRECATED: `[files]` in home.toml is deprecated \
                             since 1.2 and may be removed in 2.0; use `[home.file]` (rename \
                             `content` to `text`)";

/// The lines this build words differently from 1.0.0, each as 1.0.0 printed it and as this build
/// prints it: the same code at the same place, with the design citation gone (LD-463).
const REWORDED: &[(&str, &str)] = &[
    (
        "lodi: error E_UNSUPPORTED: `registries` is a name spec/01 §4 defines that this build \
         does not implement",
        "lodi: error E_UNSUPPORTED: `registries` is not supported by this version of lodi",
    ),
    (
        "   = hint: it would need a recipe registry outside the built-in catalogue; there is none \
         in this build",
        "   = hint: remove it: lodi uses only its built-in recipes",
    ),
    (
        "lodi: error E_UNSUPPORTED: `[vars]` is a name spec/01 §4 defines that this build does \
         not implement",
        "lodi: error E_UNSUPPORTED: `[vars]` is not supported by this version of lodi",
    ),
    (
        "   = hint: it would need `${ }` substitution, which exists in no scope yet",
        "   = hint: remove `[vars]` from home.toml: lodi does not support `${ }` substitution in \
         home.toml",
    ),
    (
        "lodi: error E_UNSUPPORTED: `[inputs]` is a name spec/01 §4 defines that this build does \
         not implement",
        "lodi: error E_UNSUPPORTED: `[inputs]` is not supported by this version of lodi",
    ),
    (
        "   = hint: it would need fetched inputs and their lock entries",
        "   = hint: remove `[inputs]` from home.toml: lodi does not support inputs fetched from \
         elsewhere",
    ),
    (
        "lodi: error E_UNSUPPORTED: `[modules]` is a name spec/01 §4 defines that this build does \
         not implement",
        "lodi: error E_UNSUPPORTED: `[modules]` is not supported by this version of lodi",
    ),
    (
        "   = hint: it would need manifest composition across files",
        "   = hint: remove `[modules]` from home.toml: lodi does not support a manifest split \
         across several files",
    ),
    (
        "   = hint: only a single name from spec/01 §2.2 may appear inside `${ }`",
        "   = hint: there are no expressions: write the value, or `$${` for a literal `${`",
    ),
];

fn repo() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR"))
}

/// The binary under test: this build's, or — to record — the one `LODI_COMPAT_BINARY` names.
fn binary() -> OsString {
    std::env::var_os("LODI_COMPAT_BINARY")
        .unwrap_or_else(|| OsString::from(env!("CARGO_BIN_EXE_lodi")))
}

fn recording() -> bool {
    std::env::var_os("LODI_RECORD_EXPECTED").is_some()
        && std::env::var_os("LODI_COMPAT_BINARY").is_some()
}

struct Walk {
    env: HomeEnv,
    transcript: String,
}

impl Walk {
    fn new(name: &str) -> Walk {
        Walk {
            env: home_env(&format!("compat-{name}")),
            transcript: String::new(),
        }
    }

    fn home(&self) -> &Path {
        self.env.home()
    }

    fn config_root(&self) -> PathBuf {
        self.env.config().join("lodi")
    }

    fn state(&self) -> PathBuf {
        self.env.data().join("lodi/home-scope/state.json")
    }

    fn install(&self, fixture: &str) {
        copy_tree(
            &repo().join("tests/fixtures/home").join(fixture),
            &self.config_root(),
        );
    }

    fn manifest(&self) -> String {
        fs::read_to_string(self.config_root().join("home.toml")).unwrap()
    }

    fn set_manifest(&self, text: &str) {
        fs::write(self.config_root().join("home.toml"), text).unwrap();
    }

    fn write(&self, rel: &str, body: &str) {
        let path = self.home().join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, body).unwrap();
    }

    /// [`Walk::write`] at an explicit mode, so the transcript does not depend on the umask.
    fn write_mode(&self, rel: &str, body: &str, mode: u32) {
        self.write(rel, body);
        fs::set_permissions(self.home().join(rel), fs::Permissions::from_mode(mode)).unwrap();
    }

    /// Run `lodi home <verb>` and append what it did to the transcript.
    fn run(&mut self, verb: &str) {
        let data = self.env.data();
        let out = std::process::Command::new(binary())
            .env_clear()
            .env("PATH", "/usr/bin:/bin")
            .env("HOME", self.env.home())
            .env("XDG_CONFIG_HOME", self.env.config())
            .env("XDG_DATA_HOME", data)
            .env("LODI_HOME", data.join("lodi"))
            .env("LODI_FETCH_REWRITE", "https://=http://127.0.0.1:9/")
            .env("SHELL", "/bin/sh")
            .current_dir(self.env.root())
            .args(["home", verb])
            .output()
            .expect("the lodi binary runs");
        let root = self.env.root().display().to_string();
        let clean = |bytes: &[u8]| String::from_utf8_lossy(bytes).replace(&root, "<root>");
        let stderr = clean(&out.stderr);
        let stderr = if recording() || out.status.code() == Some(3) {
            stderr
        } else {
            assert!(
                self.manifest().contains("\n[files."),
                "lodi home {verb}: every 1.0 walk's manifest uses [files]"
            );
            let line = format!("{FILES_WARNING}\n");
            let rest = stderr.strip_prefix(&line).unwrap_or_else(|| {
                panic!("lodi home {verb}: standard error does not open with the D5 line:\n{stderr}")
            });
            rest.to_string()
        };
        assert!(
            recording() || !stderr.contains("W_DEPRECATED"),
            "lodi home {verb}: W_DEPRECATED is printed more than once, or by a refused manifest"
        );
        let _ = write!(
            self.transcript,
            "$ lodi home {verb}\nexit {}\n--- stdout\n{}--- stderr\n{}",
            out.status
                .code()
                .map_or("signal".to_string(), |c| c.to_string()),
            clean(&out.stdout),
            stderr,
        );
    }

    /// Append a file's bytes, or its absence, to the transcript.
    fn show(&mut self, rel: &str) {
        let _ = match fs::read_to_string(self.home().join(rel)) {
            Ok(body) => write!(self.transcript, "--- ~/{rel}\n{body}"),
            Err(_) => writeln!(self.transcript, "--- ~/{rel} is not there"),
        };
    }

    /// Compare the transcript with its golden, or record it.
    fn check(self, name: &str) {
        let path = repo().join(GOLDEN).join(format!("{name}.txt"));
        if recording() {
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(&path, &self.transcript).unwrap();
        }
        let expected = fs::read_to_string(&path).unwrap_or_else(|e| {
            panic!("{GOLDEN}/{name}.txt is the committed 1.0.0 transcript and must exist: {e}")
        });
        let transcript = if recording() {
            self.transcript
        } else {
            let own = concat!(", and this is lodi ", env!("CARGO_PKG_VERSION"), "\n");
            let transcript = self
                .transcript
                .replace(own, &format!(", and this is lodi {RECORDED}\n"));
            transcript
                .split_inclusive('\n')
                .map(|line| {
                    REWORDED
                        .iter()
                        .find(|(_, now)| line.strip_suffix('\n') == Some(now))
                        .map_or(line.to_string(), |(then, _)| format!("{then}\n"))
                })
                .collect()
        };
        assert_eq!(
            transcript, expected,
            "{name}: this build does not do what 1.0.0 did"
        );
    }
}

fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// The manifest without the `[files."<path>"]` table of `path`.
fn without(manifest: &str, path: &str) -> String {
    let header = format!("[files.\"{path}\"]\n");
    let start = manifest
        .find(&header)
        .expect("the table is in the manifest");
    let rest = &manifest[start + header.len()..];
    let end = rest
        .find("\n[")
        .map_or(manifest.len(), |i| start + header.len() + i + 1);
    format!("{}{}", &manifest[..start], &manifest[end..])
}

/// Declared files over a home that has none of them, and one where two paths hold other bytes:
/// `create`, `replace` and `remove` with the backup notes.
#[test]
fn a_fresh_plan_is_what_1_0_printed() {
    let mut walk = Walk::new("fresh");
    walk.install("several");
    walk.run("plan");
    walk.write(".gitconfig", "[user]\n\tname = someone\n");
    walk.write(".netrc-that-must-go", "machine example.invalid\n");
    walk.run("plan");
    walk.check("fresh");
}

/// Apply, then everything unchanged; a hand edit is drift and the apply stops at exit 8; the
/// state 1.0.0 wrote is the specimen.
#[test]
fn apply_unchanged_and_drift_are_what_1_0_printed() {
    let mut walk = Walk::new("apply");
    walk.install("several");
    walk.run("apply");
    walk.run("plan");
    walk.run("status");
    walk.run("apply");
    if recording() {
        fs::copy(walk.state(), repo().join(SPECIMEN)).unwrap();
    }
    walk.write(".inputrc", "set editing-mode emacs\n");
    walk.run("plan");
    walk.run("apply");
    walk.run("status");
    walk.show(".inputrc");
    walk.check("apply");
}

/// An unmanaged file replaced with a backup, then its entry and another removed: the original
/// comes back and Lodi's own file goes.
#[test]
fn removed_entries_restore_and_remove_as_1_0_did() {
    let mut walk = Walk::new("removed");
    walk.install("several");
    walk.write(".gitconfig", "[user]\n\tname = someone\n");
    walk.write(".netrc-that-must-go", "machine example.invalid\n");
    walk.run("apply");
    walk.show(".gitconfig");
    walk.show(".netrc-that-must-go");
    let manifest = without(&walk.manifest(), ".gitconfig");
    let manifest = without(&manifest, ".config/git/ignore");
    walk.set_manifest(&manifest);
    walk.run("plan");
    walk.run("apply");
    walk.run("status");
    walk.show(".gitconfig");
    walk.show(".config/git/ignore");
    walk.check("removed");
}

/// Every load error of the errors fixture, in one run, sorted by position: the same errors at
/// the same places with the same messages.
#[test]
fn the_errors_fixture_is_refused_as_1_0_refused_it() {
    let mut walk = Walk::new("errors");
    walk.install("errors");
    walk.run("plan");
    walk.check("errors");
}

/// [`REWORDED`] names changes and hides nothing: each line it writes back is a line a golden
/// holds, and no golden holds the line this build prints instead.
#[test]
fn every_reworded_line_is_a_line_of_a_golden() {
    let mut goldens = String::new();
    for entry in fs::read_dir(repo().join(GOLDEN)).unwrap() {
        goldens.push_str(&fs::read_to_string(entry.unwrap().path()).unwrap());
    }
    let lines: Vec<&str> = goldens.lines().collect();
    for (then, now) in REWORDED {
        assert!(lines.contains(then), "no golden holds {then:?}");
        assert!(!lines.contains(now), "a golden already holds {now:?}");
    }
}

/// Every declared path already holds exactly the declared bytes at the declared mode, and none
/// is managed yet: 1.0 planned each one `unchanged`, applied nothing and recorded nothing.
#[test]
fn identical_unmanaged_files_are_what_1_0_printed() {
    let mut walk = Walk::new("identical");
    walk.install("several");
    walk.write_mode(".config/git/ignore", ".lodi/\n", 0o644);
    walk.write_mode(".gitconfig", "[core]\n\tautocrlf = input\n", 0o644);
    walk.write_mode(".inputrc", "set editing-mode vi\n", 0o600);
    walk.run("plan");
    walk.run("apply");
    walk.run("plan");
    walk.run("status");
    walk.show(".inputrc");
    walk.check("identical");
}

/// The bundle a 1.0 `lodi home import` wrote for the scratch home of 1.0's
/// `tests/home_import.rs`, in the order of its `DOTFILES` table: each captured path and the bytes
/// its bundle file holds. 1.1's import writes no bundle (LD-341); a 1.0 bundle is an ordinary
/// `[files]` manifest and keeps planning exactly as it did (§9 of `docs/design/HOME_PROGRAMS.md`).
const IMPORT_BUNDLE: &[(&str, &str)] = &[
    (".config/git/ignore", ".lodi/\n*.swp\n"),
    (
        ".config/nvim/init.vim",
        "set number\nset expandtab\nset shiftwidth=2\n",
    ),
    (
        ".config/starship.toml",
        "add_newline = false\n\n[character]\nsuccess_symbol = \"[>](bold green)\"\n",
    ),
    (
        ".editorconfig",
        "root = true\n\n[*]\nindent_style = space\nindent_size = 4\n",
    ),
    (".inputrc", "set editing-mode vi\nset bell-style none\n"),
    (".tmux.conf", "set -g mouse on\nset -g history-limit 5000\n"),
];

/// The import fixture with its bundle populated in the scratch configuration root: over an empty
/// home every path is `create`; over the home it was imported from — every path already holding
/// exactly the bundle's bytes, none managed — 1.0 planned `unchanged`, applied nothing and
/// reported every path `unmanaged`.
#[test]
fn the_import_bundle_plans_as_1_0_planned_it() {
    let mut walk = Walk::new("import");
    walk.install("import/full");
    for (rel, body) in IMPORT_BUNDLE {
        let path = walk.config_root().join("files").join(rel);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, body).unwrap();
    }
    walk.run("plan");
    for (rel, body) in IMPORT_BUNDLE {
        walk.write_mode(rel, body, 0o644);
    }
    walk.run("plan");
    walk.run("apply");
    walk.run("status");
    walk.check("import");
}

/// The state record 1.0.0 wrote still reads: over the same home, this build finds every path
/// unchanged and reports it `ok`, and rewrites nothing.
#[test]
fn the_state_1_0_wrote_still_reads() {
    let mut walk = Walk::new("specimen");
    walk.install("several");
    walk.run("apply");
    fs::copy(repo().join(SPECIMEN), walk.state()).unwrap();
    let before = fs::read(walk.state()).unwrap();
    walk.run("plan");
    walk.run("status");
    walk.run("apply");
    assert_eq!(
        fs::read(walk.state()).unwrap(),
        before,
        "an apply with nothing to do rewrote the 1.0 state"
    );
    walk.check("specimen");
}
