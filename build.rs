//! Embed the recipe catalogue (`catalogue/`) into the binary (design call D1, LD-76).
//!
//! `catalogue/tools/` and `catalogue/bases/` are walked at build time and turned into the two
//! sorted slices `BUILTIN_TOOLS` and `BUILTIN_BASES` of `(file name, text)` that
//! `src/catalogue/mod.rs` includes, so **no Rust file names a recipe**: adding a recipe is
//! adding a file. Anything this script does not recognize — a file that is not `*.toml`, a
//! sub-directory, a stem that is not a valid package name — is a build failure, never a silent
//! skip.
//!
//! A catalogue file is embedded **exactly as it is on disk**: nothing is stripped, and the
//! digest `lodi.lock` records for a recipe or a base is the digest of the whole file. The
//! catalogue is under the repository's own GPL-3.0-or-later `LICENSE` like everything else, so
//! it carries no per-file licence header; removing the header line it used to carry, in the
//! same change that stopped this script stripping one, left every recorded digest unmoved.
//!
//! `std` only: no dependency, no network, and nothing outside this repository is read.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

fn fail(message: String) -> ! {
    // A build script's stderr is shown when it exits non-zero; `cargo:warning` keeps the line
    // readable in a quiet build too.
    println!("cargo:warning={message}");
    eprintln!("catalogue: {message}");
    std::process::exit(1);
}

/// `^[a-z0-9][a-z0-9._+-]{0,127}$`, the package-name rule the binary uses.
fn is_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    let first = chars.next();
    first.is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name.len() <= 128
        && chars.all(|c| {
            c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '+' | '-')
        })
}

/// Read one catalogue directory: file name -> the file's text, verbatim.
fn read_dir(dir: &Path) -> BTreeMap<String, String> {
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| fail(format!("{} cannot be read: {e}", dir.display())));
    let mut out = BTreeMap::new();
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| fail(format!("{}: {e}", dir.display())));
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry
            .file_type()
            .unwrap_or_else(|e| fail(format!("{}: {e}", path.display())));
        if !kind.is_file() {
            fail(format!(
                "{} is not a regular file; catalogue directories hold recipe files only",
                path.display()
            ));
        }
        let Some(stem) = name.strip_suffix(".toml") else {
            fail(format!(
                "{} is not a .toml file; catalogue directories hold recipe files only",
                path.display()
            ));
        };
        if !is_package_name(stem) {
            fail(format!(
                "{}: `{stem}` is not a valid package name (^[a-z0-9][a-z0-9._+-]{{0,127}}$)",
                path.display()
            ));
        }
        let text = fs::read_to_string(&path)
            .unwrap_or_else(|e| fail(format!("{} cannot be read: {e}", path.display())));
        println!("cargo:rerun-if-changed={}", path.display());
        out.insert(name, text);
    }
    if out.is_empty() {
        fail(format!("{} holds no recipe", dir.display()));
    }
    out
}

/// Write each embedded text to `OUT_DIR` and render the slice that includes them.
fn slice(name: &str, kind: &str, files: &BTreeMap<String, String>, out_dir: &Path) -> String {
    let dir = out_dir.join("catalogue").join(kind);
    fs::create_dir_all(&dir)
        .unwrap_or_else(|e| fail(format!("{} cannot be created: {e}", dir.display())));
    let mut rendered = format!("pub const {name}: &[(&str, &str)] = &[\n");
    for (file, text) in files {
        fs::write(dir.join(file), text)
            .unwrap_or_else(|e| fail(format!("{}/{file} cannot be written: {e}", dir.display())));
        rendered.push_str(&format!(
            "    ({file:?}, include_str!(concat!(env!(\"OUT_DIR\"), \"/catalogue/{kind}/{file}\"))),\n"
        ));
    }
    rendered.push_str("];\n");
    rendered
}

fn main() {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let out_dir = PathBuf::from(
        std::env::var_os("OUT_DIR").unwrap_or_else(|| fail("OUT_DIR is not set".into())),
    );
    let catalogue = root.join("catalogue");
    for watched in ["tools", "bases", "catalogue.toml"] {
        println!(
            "cargo:rerun-if-changed={}",
            catalogue.join(watched).display()
        );
    }
    println!("cargo:rerun-if-changed=build.rs");

    let tools = read_dir(&catalogue.join("tools"));
    let bases = read_dir(&catalogue.join("bases"));
    for file in tools.keys() {
        if bases.contains_key(file) {
            fail(format!(
                "{file} is both a tool recipe and a base definition"
            ));
        }
    }
    let generated = format!(
        "// Generated by build.rs from catalogue/. Do not edit; add a file to catalogue/ instead.\n\
         {}\n{}",
        slice("BUILTIN_TOOLS", "tools", &tools, &out_dir),
        slice("BUILTIN_BASES", "bases", &bases, &out_dir),
    );
    let target = out_dir.join("catalogue_builtin.rs");
    fs::write(&target, generated)
        .unwrap_or_else(|e| fail(format!("{} cannot be written: {e}", target.display())));
}
