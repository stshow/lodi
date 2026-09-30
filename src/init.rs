//! `lodi init`: write a starter project manifest into the current directory (design
//! `spec/10-cli` §4, adapted to TOML per OD-03/ADR-020; `docs/milestones/m-0.3/DESIGN_CALLS.md`
//! D1 and D2, recorded as LD-46 and LD-47).
//!
//! It is offline and self-contained: it reads and writes only `./lodi.toml` and `./.gitignore`,
//! downloads nothing, installs nothing, runs nothing, and never creates `.lodi/` itself — only
//! the ignore line that keeps it out of version control.
//!
//! The manifest it writes is the smallest one the *next two* commands accept: `lodi lock`
//! resolves it and `lodi develop` enters it with no manual edit (D2). Its exact bytes are
//! committed as `tests/fixtures/init/*.toml`, and `tests/init.rs` compares what this module
//! writes against them, so the template cannot drift from what the tests prove.

use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::diag::Diagnostic;
use crate::lock::{Failure, MANIFEST_FILE};
use crate::manifest::{Distro, sanitize_name};

/// The ignore file `lodi init` appends to, creating it when it is absent.
pub const GITIGNORE_FILE: &str = ".gitignore";
/// The one line `lodi init` adds to it; `.lodi/` itself is never created here.
pub const IGNORE_LINE: &str = ".lodi/";
/// The mode of every file this module creates (`spec/10-cli` §4).
pub const MANIFEST_MODE: u32 = 0o644;
/// The tool the written manifest activates: the recipe the built-in catalogue resolves (LD-18).
pub const DEFAULT_TOOL: &str = "python";
/// Its constraint, the one `README.md`'s quick start has always used.
pub const DEFAULT_TOOL_CONSTRAINT: &str = "3.12";
/// The container bases `--base` can write, spelled as they are accepted on the command line.
/// This build's manifest reader supports exactly these (`src/manifest.rs`, `E_UNSUPPORTED`).
pub const SUPPORTED_BASES: &[&str] = &[
    "arch:rolling",
    "arch",
    "debian:bookworm",
    "debian:12",
    "fedora:44",
    "fedora",
    "ubuntu:noble",
    "ubuntu:24.04",
];

/// What one `lodi init` invocation asks for.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// `--name NAME`; absent means the project directory's basename (`spec/01` §3.2).
    pub name: Option<String>,
    /// `--base DISTRO[:RELEASE]`; absent means host-tool mode.
    pub base: Option<String>,
    /// `--force`: replace an existing `./lodi.toml` instead of refusing it.
    pub force: bool,
}

/// A container base `--base` accepts, normalized to what the manifest reader expects.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub distro: String,
    pub release: String,
}

/// Parse `DISTRO[:RELEASE]`. Anything this build cannot write is `E_UNSUPPORTED` (exit 3),
/// naming what it does support; there is no silent default distribution.
pub fn parse_base(text: &str) -> Result<Base, Diagnostic> {
    let unsupported = |what: String| {
        Diagnostic::new("E_UNSUPPORTED", what).hint(format!(
            "this build writes {}; another base needs a lodi that implements it",
            SUPPORTED_BASES
                .iter()
                .map(|b| format!("`--base {b}`"))
                .collect::<Vec<_>>()
                .join(" or ")
        ))
    };
    let written = text.split_once(':').map_or(text, |(d, _)| d);
    let Some(distro) = Distro::parse(written) else {
        return Err(unsupported(format!(
            "container distribution `{written}` is not supported by this build"
        )));
    };
    // The canonical release is the default: `--base ubuntu` writes noble, `--base debian`
    // bookworm, `--base arch` rolling.
    let release = text
        .split_once(':')
        .map_or(distro.releases()[0], |(_, r)| r);
    if !distro.releases().contains(&release) {
        return Err(unsupported(format!(
            "container release `{release}` is not supported by this build"
        )));
    }
    Ok(Base {
        distro: distro.name().to_string(),
        release: distro.releases()[0].to_string(),
    })
}

/// The environment name the template carries: `--name` when given, else the project directory's
/// basename, sanitized to `[a-z0-9-]` the same way [`crate::manifest::ProjectManifest`] does.
pub fn resolve_name(root: &Path, requested: Option<&str>) -> Result<String, Diagnostic> {
    let (source, from_flag) = match requested {
        Some(name) => (name.to_string(), true),
        None => (
            root.canonicalize()
                .ok()
                .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                .unwrap_or_default(),
            false,
        ),
    };
    let name = sanitize_name(&source);
    if name.is_empty() {
        let what = if from_flag {
            format!("`--name {source}` leaves no usable project name")
        } else {
            "the project directory's name leaves no usable project name".to_string()
        };
        return Err(Diagnostic::new("E_IDENT", what)
            .hint("pass `--name NAME` with at least one of a-z, 0-9 or -"));
    }
    Ok(name)
}

/// The exact bytes `lodi init` writes for `name`, in host-tool mode when `base` is `None`.
pub fn template(name: &str, base: Option<&Base>) -> String {
    let mut out = String::new();
    out.push_str(
        "# lodi.toml: this project's environment, written by `lodi init`. `lodi lock` pins\n\
         # what it asks for in ./lodi.lock, and `lodi develop -- COMMAND` runs a command in it.\n\
         \n\
         # The project's name. `version` is the format of this file, and \"1\" is the only one.\n\
         [project]\n\
         version = \"1\"\n",
    );
    out.push_str(&format!("name = \"{name}\"\n"));
    if let Some(base) = base {
        out.push_str(&format!(
            "\n\
             # A container environment: `lodi develop` runs the command inside a rootless Podman\n\
             # image built from this distribution's own packages. It needs `podman` on PATH. Lodi\n\
             # never installs it and changes no setting on this machine.\n\
             [container]\n\
             distro = \"{}\"\n\
             release = \"{}\"\n\
             \n\
             # Distribution packages to install in that image. `lodi lock` pins each one and all\n\
             # it depends on, and the image is rebuilt when this list changes.\n\
             [packages]\n\
             common = []\n",
            base.distro, base.release
        ));
    }
    out.push_str(&format!(
        "\n\
         # Upstream tools, such as python and nodejs, that lodi downloads and unpacks. A version\n\
         # is a prefix (\"{DEFAULT_TOOL_CONSTRAINT}\"), \"latest\", or a range such as \">=3.11, <3.13\".\n\
         [tools]\n\
         {DEFAULT_TOOL} = \"{DEFAULT_TOOL_CONSTRAINT}\"\n\
         \n\
         # Tasks run with /bin/sh -c in that environment, and only after `lodi trust`:\n\
         #   lodi run versions\n\
         # [tasks.versions]\n\
         # run = \"python3 --version\"\n\
         \n\
         # Variables set for the commands you run. A value may use ${{var}}, with no expressions.\n\
         # [env]\n\
         # PYTHONDONTWRITEBYTECODE = \"1\"\n"
    ));
    out
}

fn io_error(path: &Path, e: std::io::Error) -> Failure {
    Failure::one(Diagnostic::new(
        "E_STORE_IO",
        format!("cannot write {}: {e}", path.display()),
    ))
}

/// Write `text` to `path` through a temporary file in the same directory and one `rename`, so a
/// reader of `path` sees either the old file or the whole new one and never a partial write.
fn write_atomically(path: &Path, text: &str) -> Result<(), Failure> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let tmp = dir.join(format!(
        ".{}.lodi-init-{}",
        path.file_name().unwrap_or_default().to_string_lossy(),
        std::process::id()
    ));
    let write = || -> std::io::Result<()> {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(MANIFEST_MODE)
            .open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        // `create` keeps the mode of a temporary file left by an earlier run; set it explicitly.
        fs::set_permissions(
            &tmp,
            std::os::unix::fs::PermissionsExt::from_mode(MANIFEST_MODE),
        )?;
        fs::rename(&tmp, path)
    };
    write().map_err(|e| {
        let _ = fs::remove_file(&tmp);
        io_error(path, e)
    })
}

/// Whether `text` already ignores `.lodi/`: an exact line, blanks and trailing `\r` aside.
fn ignores_lodi(text: &str) -> bool {
    text.lines()
        .any(|l| l.trim_end_matches('\r') == IGNORE_LINE)
}

/// An existing ignore file, opened once for both inspection and any append, or an absent one.
enum IgnoreFile {
    Existing { file: fs::File, text: String },
    Missing,
}

/// Open an existing `.gitignore` without following its final path component. Keeping this
/// descriptor through the append means a path swap cannot redirect the write after inspection.
fn open_ignore_file(path: &Path) -> Result<IgnoreFile, Failure> {
    let mut file = match fs::OpenOptions::new()
        .read(true)
        .append(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(IgnoreFile::Missing),
        Err(e) => return Err(io_error(path, e)),
    };
    let metadata = file.metadata().map_err(|e| io_error(path, e))?;
    if !metadata.file_type().is_file() {
        return Err(io_error(
            path,
            std::io::Error::other("refusing to use an object that is not a regular file"),
        ));
    }
    let mut text = String::new();
    file.read_to_string(&mut text)
        .map_err(|e| io_error(path, e))?;
    Ok(IgnoreFile::Existing { file, text })
}

/// `lodi init` for the project in `root` (the current directory). Returns the lines to print, in
/// the order `spec/10-cli` §4 asks for: what was written, the ignore line if it was added, and
/// the next command. Nothing outside `root` is read or written, and nothing is downloaded.
pub fn init(root: &Path, options: &Options) -> Result<Vec<String>, Failure> {
    let manifest_path = root.join(MANIFEST_FILE);
    let existed = fs::symlink_metadata(&manifest_path).is_ok();
    if existed && !options.force {
        // D1/LD-46: the refusal is detected before anything is written, which is the case
        // `spec/11` §2 maps to exit 3. Nothing on disk is touched.
        return Err(Failure::one(
            Diagnostic::new(
                "E_EXISTS",
                format!("{} already exists", display(&manifest_path)),
            )
            .hint(format!(
                "use --force to overwrite {}",
                display(&manifest_path)
            )),
        ));
    }
    let base = options
        .base
        .as_deref()
        .map(parse_base)
        .transpose()
        .map_err(Failure::one)?;
    let name = resolve_name(root, options.name.as_deref()).map_err(Failure::one)?;
    let text = template(&name, base.as_ref());

    // Inspect `.gitignore` before publishing the manifest, so an unsafe object refuses the whole
    // operation. An absent path is later created by same-directory atomic rename, which does not
    // follow a final symlink that appears in the meantime.
    let ignore_path = root.join(GITIGNORE_FILE);
    let ignore_file = open_ignore_file(&ignore_path)?;
    write_atomically(&manifest_path, &text)?;

    let mut printed = vec![format!(
        "{} {}",
        if existed { "replaced" } else { "wrote" },
        display(&manifest_path)
    )];

    match ignore_file {
        IgnoreFile::Existing { text, .. } if ignores_lodi(&text) => {}
        IgnoreFile::Existing { mut file, text } => {
            // Append, never rewrite: every other line of the file is left exactly as it was.
            let prefix = if text.is_empty() || text.ends_with('\n') {
                ""
            } else {
                "\n"
            };
            file.write_all(format!("{prefix}{IGNORE_LINE}\n").as_bytes())
                .map_err(|e| io_error(&ignore_path, e))?;
            printed.push(format!("added {IGNORE_LINE} to {}", display(&ignore_path)));
        }
        IgnoreFile::Missing => {
            write_atomically(&ignore_path, &format!("{IGNORE_LINE}\n"))?;
            printed.push(format!("added {IGNORE_LINE} to {}", display(&ignore_path)));
        }
    }
    printed.push("next: lodi lock".to_string());
    Ok(printed)
}

/// `./lodi.toml` rather than `lodi.toml` for a path in the current directory, so the hint names
/// the file the way the user would type it.
fn display(path: &Path) -> String {
    let shown = path.display().to_string();
    match shown.strip_prefix("./") {
        Some(_) => shown,
        None if shown.starts_with('/') => shown,
        None => format!("./{shown}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::parse_project_manifest;

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("lodi-unit-init-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_template_is_a_manifest_this_build_accepts() {
        for base in [
            None,
            Some(Base {
                distro: "debian".into(),
                release: "bookworm".into(),
            }),
            Some(Base {
                distro: "arch".into(),
                release: "rolling".into(),
            }),
        ] {
            let text = template("demo", base.as_ref());
            let m = parse_project_manifest(&text, "lodi.toml").expect("the template parses");
            assert_eq!(m.project.name.as_deref(), Some("demo"));
            assert_eq!(m.project.schema_version, "1");
            assert!(m.tools.contains_key(DEFAULT_TOOL), "{text}");
            assert_eq!(m.is_container_mode(), base.is_some(), "{text}");
            assert!(m.tasks.is_empty(), "the template activates no task text");
            assert!(text.ends_with('\n') && !text.starts_with('\n'));
        }
    }

    #[test]
    fn base_parsing_normalizes_and_refuses() {
        assert_eq!(parse_base("debian").unwrap().release, "bookworm");
        assert_eq!(parse_base("debian:12").unwrap().release, "bookworm");
        assert_eq!(parse_base("debian:bookworm").unwrap().distro, "debian");
        assert_eq!(parse_base("ubuntu").unwrap().release, "noble");
        assert_eq!(parse_base("ubuntu:24.04").unwrap().release, "noble");
        assert_eq!(parse_base("ubuntu:noble").unwrap().distro, "ubuntu");
        assert_eq!(parse_base("arch").unwrap().release, "rolling");
        assert_eq!(parse_base("arch:rolling").unwrap().distro, "arch");
        assert_eq!(parse_base("fedora").unwrap().release, "44");
        assert_eq!(parse_base("fedora:44").unwrap().distro, "fedora");
        for bad in [
            "arch:2026.09.01",
            "arch:latest",
            "debian:trixie",
            "ubuntu:jammy",
            "ubuntu:22.04",
            "debian:noble",
            "fedora:43",
            "",
        ] {
            let d = parse_base(bad).unwrap_err();
            assert_eq!(d.code, "E_UNSUPPORTED", "{bad}");
            assert!(d.to_string().contains("debian:bookworm"), "{bad}: {d}");
            assert!(d.to_string().contains("ubuntu:noble"), "{bad}: {d}");
        }
    }

    #[test]
    fn a_name_that_sanitizes_to_nothing_is_an_error() {
        let dir = scratch("name");
        let d = resolve_name(&dir, Some("!!!")).unwrap_err();
        assert_eq!(d.code, "E_IDENT");
        assert!(d.to_string().contains("--name"), "{d}");
        assert_eq!(resolve_name(&dir, Some("My App_2")).unwrap(), "my-app-2");
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_existing_ignore_line_is_recognized_exactly() {
        assert!(ignores_lodi(".lodi/\n"));
        assert!(ignores_lodi("target\n.lodi/\r\n"));
        assert!(!ignores_lodi(".lodi\n"));
        assert!(!ignores_lodi("/.lodi/\n"));
        assert!(!ignores_lodi("# .lodi/\n"));
    }

    #[test]
    fn writing_is_atomic_and_leaves_no_temporary_file() {
        let dir = scratch("atomic");
        write_atomically(&dir.join("f"), "x\n").unwrap();
        assert_eq!(fs::read_to_string(dir.join("f")).unwrap(), "x\n");
        let left: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left.len(), 1, "{left:?}");
        fs::remove_dir_all(&dir).unwrap();
    }
}
