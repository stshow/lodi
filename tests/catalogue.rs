//! M-0.4 T-1's package check: the catalogue directory and what the binary embeds are the same
//! set, every file in it parses with the real parser, and the embedded text is the file's own
//! bytes — no header is stripped and none is carried (LD-76, LD-135, and M-1.0 R-6, which made
//! the catalogue GPL-3.0-or-later like the rest of the repository).
//!
//! Offline and deterministic: it reads `catalogue/` from the source tree and the embedded
//! constants from the library, and makes no request.

use std::collections::BTreeSet;
use std::fs;
use std::path::PathBuf;

use lodi::catalogue::{
    BUILTIN_BASES, BUILTIN_TOOLS, ChecksumSource, RootfsDefinition, builtin_base, builtin_recipe,
    builtin_tool_names, parse_base, parse_recipe,
};
use lodi::upstream::strategy::{Discovery, STRATEGIES};

fn catalogue_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("catalogue")
}

/// The `*.toml` file names in one catalogue sub-directory, read from the tree.
fn walk(kind: &str) -> BTreeSet<String> {
    fs::read_dir(catalogue_dir().join(kind))
        .expect("the catalogue directory is readable")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

fn text(kind: &str, file: &str) -> String {
    fs::read_to_string(catalogue_dir().join(kind).join(file)).expect("the recipe is readable")
}

// ---------------------------------------------------------------------------------------------
// The embedded set is the directory

#[test]
fn what_is_embedded_is_exactly_what_the_directory_holds() {
    let embedded_tools: BTreeSet<String> =
        BUILTIN_TOOLS.iter().map(|(f, _)| f.to_string()).collect();
    let embedded_bases: BTreeSet<String> =
        BUILTIN_BASES.iter().map(|(f, _)| f.to_string()).collect();
    assert_eq!(
        embedded_tools,
        walk("tools"),
        "BUILTIN_TOOLS and catalogue/tools/ must be the same set"
    );
    assert_eq!(
        embedded_bases,
        walk("bases"),
        "BUILTIN_BASES and catalogue/bases/ must be the same set"
    );
    assert!(!embedded_tools.is_empty() && !embedded_bases.is_empty());
    // Sorted, so the embedded order is the directory's order and never an accident of a walk.
    let files: Vec<&str> = BUILTIN_TOOLS.iter().map(|(f, _)| *f).collect();
    let mut sorted = files.clone();
    sorted.sort_unstable();
    assert_eq!(files, sorted);
}

#[test]
fn the_embedded_text_is_the_file_itself_and_carries_no_licence_header() {
    for (kind, set) in [("tools", BUILTIN_TOOLS), ("bases", BUILTIN_BASES)] {
        for (file, embedded) in set {
            let on_disk = text(kind, file);
            assert_eq!(
                *embedded, &on_disk,
                "{kind}/{file}: the embedded text is the file itself; build.rs strips nothing"
            );
            assert!(
                !embedded.contains("SPDX-License-Identifier"),
                "{kind}/{file}: the catalogue is under the repository's own LICENSE and carries \
                 no per-file licence header"
            );
        }
    }
}

#[test]
fn the_catalogue_is_under_the_repository_licence_alone() {
    // M-1.0 R-6: one licence for the whole repository, GPL-3.0-or-later. The catalogue has no
    // licence file of its own, and nothing under it claims a second licence.
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    assert!(
        !catalogue_dir().join("LICENSE").exists(),
        "catalogue/LICENSE must not come back: the catalogue is GPL-3.0-or-later like the tool"
    );
    assert!(
        !root.join("docs/licenses/RECIPES-MIT.txt").exists(),
        "docs/licenses/RECIPES-MIT.txt records a boundary that no longer exists"
    );
    let licence = fs::read_to_string(root.join("LICENSE")).expect("the repository LICENSE");
    assert!(licence.contains("GNU GENERAL PUBLIC LICENSE"));
    let readme = fs::read_to_string(catalogue_dir().join("README.md")).unwrap();
    assert!(
        readme.contains("GPL-3.0-or-later"),
        "catalogue/README.md states the one licence this directory is under"
    );
    for kind in ["tools", "bases"] {
        for file in walk(kind) {
            assert!(
                !text(kind, &file).contains("MIT"),
                "{kind}/{file} must name no second licence"
            );
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Every embedded file parses with the real parser

#[test]
fn every_embedded_recipe_and_base_parses() {
    for (file, text) in BUILTIN_TOOLS {
        let recipe = parse_recipe(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message));
        assert_eq!(format!("{}.toml", recipe.name), *file);
        assert!(
            !recipe.bin.is_empty(),
            "{file}: [spec] bin must not be empty"
        );
        for p in recipe.bin.iter().chain(&recipe.path) {
            assert!(
                !p.starts_with('/') && !p.split('/').any(|c| c == ".." || c.is_empty()),
                "{file}: `{p}` must be a relative path inside the artifact"
            );
        }
        assert!(
            STRATEGIES.iter().any(|(n, _)| *n == recipe.strategy),
            "{file}: strategy `{}` is not registered",
            recipe.strategy
        );
        // Every artifact has a hash source, and `ChecksumSource::Strategy` is only reachable
        // for a strategy that really carries the digest (catalogue/README.md).
        assert!(!recipe.assets.is_empty(), "{file}: no [asset]");
        for asset in &recipe.assets {
            match &asset.checksum {
                ChecksumSource::File(url) => {
                    assert!(url.starts_with("https://"), "{file}: {url}")
                }
                ChecksumSource::Sidecar(suffix) => {
                    assert!(!suffix.is_empty(), "{file}: empty checksum_sidecar")
                }
                ChecksumSource::Index(key) => assert!(!key.is_empty(), "{file}: empty key"),
                ChecksumSource::Strategy => assert!(
                    STRATEGIES
                        .iter()
                        .any(|(n, s)| *n == recipe.strategy && s.supplies_digest),
                    "{file}: no hash source and `{}` does not supply the digest",
                    recipe.strategy
                ),
            }
            if let Some(url) = &asset.url {
                assert!(url.starts_with("https://"), "{file}: {url}");
            }
        }
    }
    for (file, text) in BUILTIN_BASES {
        let base = parse_base(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message));
        assert_eq!(format!("{}.toml", base.distro), *file);
        assert!(!base.releases.is_empty() && !base.repositories.is_empty());
    }
}

#[test]
fn builtin_lookups_see_tools_and_bases_apart() {
    let names = builtin_tool_names();
    assert_eq!(
        names,
        walk("tools")
            .iter()
            .map(|f| f.trim_end_matches(".toml").to_string())
            .collect::<Vec<_>>()
    );
    for name in &names {
        assert!(builtin_recipe(name).is_some(), "{name} resolves as a tool");
        assert!(builtin_base(name).is_none(), "{name} is not a base");
    }
    for file in walk("bases") {
        let distro = file.trim_end_matches(".toml");
        assert!(
            builtin_base(distro).is_some(),
            "{distro} resolves as a base"
        );
        assert!(
            builtin_recipe(distro).is_none(),
            "{distro} is a base, never a tool recipe"
        );
    }
    assert!(builtin_recipe("no-such-tool").is_none());
    assert!(builtin_base("no-such-base").is_none());
}

// ---------------------------------------------------------------------------------------------
// The substance of the unit tests that moved out of src/catalogue/mod.rs

#[test]
fn builtin_catalogue_parses() {
    let python = builtin_recipe("python").unwrap().unwrap();
    assert!(matches!(
        python.versions,
        Discovery::GithubAssets { releases: 2, .. }
    ));
    assert_eq!(python.assets[0].strip_components, 1);
    let node = builtin_recipe("nodejs").unwrap().unwrap();
    assert_eq!(node.arch_names["x86_64"], "x64");
    let debian = builtin_base("debian").unwrap().unwrap();
    let bookworm = &debian.releases["bookworm"];
    assert_eq!(bookworm.aliases, ["12"]);
    assert!(matches!(
        &bookworm.rootfs["x86_64"],
        RootfsDefinition::GithubOciLayout { branch, .. } if branch == "dist-amd64"
    ));
    assert_eq!(
        debian.repositories["debian-security"].suites,
        ["${release}-security"]
    );
    let ubuntu = builtin_base("ubuntu").unwrap().unwrap();
    assert_eq!(ubuntu.distro, "ubuntu");
    let noble = &ubuntu.releases["noble"];
    assert_eq!(noble.aliases, ["24.04"]);
    assert_eq!(noble.repositories, ["ubuntu"]);
    assert_eq!(
        ubuntu.repositories["ubuntu"].suites,
        ["${release}", "${release}-updates", "${release}-security"]
    );
    assert_eq!(
        ubuntu.repositories["ubuntu"].components,
        ["main", "universe"]
    );
    assert!(matches!(
        &noble.rootfs["x86_64"],
        RootfsDefinition::DatedChecksumDir { checksums, .. } if checksums == "SHA256SUMS"
    ));
}

#[test]
fn recipes_hold_no_version_lists() {
    for (file, text) in BUILTIN_TOOLS {
        for line in text.lines().filter(|l| !l.trim_start().starts_with('#')) {
            let digits_dot = line.contains("3.12") || line.contains("22.") || line.contains("v22");
            assert!(!digits_dot, "{file} lists a version: {line}");
        }
    }
}

#[test]
fn invalid_recipes_are_refused() {
    let python = BUILTIN_TOOLS
        .iter()
        .find(|(f, _)| *f == "python.toml")
        .unwrap()
        .1;
    let cases = [
        python.replace("[spec]", "[spec]\nextra = 1"),
        python.replace("github_assets", "static"),
        python.replace("${arch_rust}", "${arch_nope}"),
        python.replace("name        = \"python\"", "name = \"py\""),
        python.replace("tar.gz\"\n", "zip\"\n"),
        python.replace("[asset]", "[asset]\nurl = \"https://x/\""),
        python.replace("bin/pip3", "../pip3"),
        "not toml [".to_string(),
    ];
    for text in cases {
        let e = parse_recipe(&text, "python.toml").unwrap_err();
        assert_eq!(e.code, "E_RECIPE_INVALID", "{text}");
        assert!(e.message.contains("python.toml"));
    }
}

fn rust_with_exclude(replacement: &str) -> String {
    let rust = BUILTIN_TOOLS
        .iter()
        .find(|(file, _)| *file == "rust.toml")
        .unwrap()
        .1;
    rust.replacen("exclude          = [\"manifest.in\"]", replacement, 1)
}

fn assert_invalid_exclude(replacement: &str) {
    let text = rust_with_exclude(replacement);
    let error = parse_recipe(&text, "rust.toml").unwrap_err();
    assert_eq!(error.code, "E_RECIPE_INVALID", "{}", error.message);
    assert!(error.message.contains("rust.toml"), "{}", error.message);
}

#[test]
fn an_exclude_glob_is_refused() {
    assert_invalid_exclude("exclude = [\"*.in\"]");
}

#[test]
fn an_exclude_directory_is_refused() {
    assert_invalid_exclude("exclude = [\"manifest.in/\"]");
}

#[test]
fn an_absolute_exclude_is_refused() {
    assert_invalid_exclude("exclude = [\"/manifest.in\"]");
}

#[test]
fn an_exclude_with_parent_is_refused() {
    assert_invalid_exclude("exclude = [\"../manifest.in\"]");
}

#[test]
fn an_empty_exclude_is_refused() {
    assert_invalid_exclude("exclude = [\"\"]");
}

#[test]
fn duplicate_excludes_are_refused() {
    assert_invalid_exclude("exclude = [\"manifest.in\", \"manifest.in\"]");
}

#[test]
fn exact_excludes_are_recorded_per_asset() {
    let rust = builtin_recipe("rust").unwrap().unwrap();
    assert_eq!(rust.assets.len(), 3);
    assert!(
        rust.assets
            .iter()
            .all(|asset| asset.exclude == ["manifest.in"])
    );
}

#[test]
fn invalid_base_definitions_are_refused() {
    let ubuntu = BUILTIN_BASES
        .iter()
        .find(|(f, _)| *f == "ubuntu.toml")
        .unwrap()
        .1;
    let cases = [
        ubuntu.replace("dated_checksum_dir", "dated_checksum_tarball"),
        ubuntu.replace("index_url    = \"https://", "index_url    = \"http://"),
        ubuntu.replace(
            "checksums    = \"SHA256SUMS\"",
            "checksums    = \"a/SHA256SUMS\"",
        ),
        ubuntu.replace("${deb_arch}", "${deb_arch_nope}"),
        ubuntu.replace(
            "repositories = [\"ubuntu\"]",
            "repositories = [\"universe\"]",
        ),
        ubuntu.replace(
            "[release.noble.rootfs.x86_64]",
            "[release.noble.rootfs.x86_64]\nextra = 1",
        ),
        ubuntu.replace(
            "snapshot_min = \"2024-04-25T00:00:00Z\"",
            "snapshot_min = \"2024-04-25\"",
        ),
    ];
    for text in cases {
        let e = parse_base(&text, "ubuntu.toml").unwrap_err();
        assert_eq!(e.code, "E_RECIPE_INVALID", "{text}");
        assert!(e.message.contains("ubuntu.toml"));
    }
    // The github strategy keeps its own key set: an Ubuntu key is unknown there.
    let debian = BUILTIN_BASES
        .iter()
        .find(|(f, _)| *f == "debian.toml")
        .unwrap()
        .1;
    let e = parse_base(
        &debian.replace("branch   = \"dist-amd64\"", "checksums = \"SHA256SUMS\""),
        "debian.toml",
    )
    .unwrap_err();
    assert_eq!(e.code, "E_RECIPE_INVALID");
}

/// The catalogue is a catalogue, not a demonstration: the count is printed so that a reader of
/// the test output sees what this build ships, and the floor is what M-0.4 requires of it.
#[test]
fn the_catalogue_ships_a_real_number_of_tools() {
    let names = builtin_tool_names();
    println!("catalogue: {} tools: {}", names.len(), names.join(" "));
    assert!(
        names.len() >= 8,
        "the catalogue ships {} tools, fewer than the eight M-0.4 requires: {}",
        names.len(),
        names.join(" ")
    );
}

/// `[spec] env` (M-0.4 T-5): a recipe may contribute an environment, and the only substitutions
/// its values carry into the lock are the two that are resolved at realization. A recipe with
/// none of it keeps an empty map, so nothing about earlier recipes changed.
#[test]
fn a_recipes_contributed_environment_is_parsed_and_bounded() {
    let mut with_env = 0;
    for (file, text) in BUILTIN_TOOLS {
        let recipe = parse_recipe(text, file).unwrap_or_else(|e| panic!("{file}: {}", e.message));
        if recipe.env.is_empty() {
            continue;
        }
        with_env += 1;
        for (name, value) in &recipe.env {
            assert!(
                name.bytes()
                    .next()
                    .is_some_and(|b| b.is_ascii_uppercase() || b == b'_')
                    && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'),
                "{file}: `{name}` is not an environment variable name"
            );
            // `${self.path}` and `${self.version}` are resolved at realization; anything else
            // would reach a child unsubstituted, so the parser refuses it.
            for part in value.split("${").skip(1) {
                let placeholder = part.split('}').next().unwrap_or(part);
                assert!(
                    ["self.path", "self.version"].contains(&placeholder),
                    "{file}: `{name}` substitutes `{placeholder}`"
                );
            }
        }
    }
    assert!(
        with_env >= 1,
        "no shipped recipe contributes an environment, so `[spec] env` is untested data"
    );

    // And the refusals, over a recipe that really has the table.
    let (file, text) = BUILTIN_TOOLS
        .iter()
        .find(|(_, t)| t.contains("env  = {"))
        .expect("a recipe with a [spec] env table");
    for broken in [
        text.replace("${self.path}", "${self.root}"),
        text.replace("env  = {", "env  = \"JAVA_HOME\" #"),
    ] {
        let e = parse_recipe(&broken, file).unwrap_err();
        assert_eq!(e.code, "E_RECIPE_INVALID", "{}", e.message);
    }
}
