//! M-Arch-Base T-1: the package-manager family seam, and the pins that prove it moved nothing.
//!
//! The pinning part of this file was written and seen to pass **before** the seam existed and
//! passes **unedited** after it. It pins:
//!
//! 1. the SHA-256 of every file an image build generates, for both container bases this build
//!    carries, over the whole surface those files vary on — `has_packages`, `installRecommends`,
//!    `locale`, and each distribution's own repositories read from the committed base
//!    definition;
//! 2. `container::image_identity` for the committed fixture lock, which hashes those digests
//!    together with the locked rootfs and closure, so a changed build file is a changed image
//!    identity and every warm environment would rebuild.
//!
//! A pinned value that has to be edited to make this file pass is the behaviour change T-1
//! forbids, not a value to update. Offline: no network, no Podman, no temporary directory.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use lodi::catalogue::{builtin_base, render};
use lodi::container::{build_file_digests, image_identity};
use lodi::lock::{BaseEntry, RepositoryEntry, parse_lock};
use lodi::util::snapshot_id;

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The committed fixture lock's Debian base (`tests/fixtures/spike/lodi.lock`).
fn fixture_base() -> BaseEntry {
    let bytes = fs::read(root().join("tests/fixtures/spike/lodi.lock")).unwrap();
    parse_lock(&bytes).unwrap().base.unwrap()
}

/// The fixture base carrying `distro`'s own repositories, rendered from the committed base
/// definition at a fixed snapshot. Everything a generated build file reads comes either from
/// the repositories or from `options`, so this covers both bases with no network and no lock run.
fn base_of(distro: &str, release: &str) -> BaseEntry {
    let def = builtin_base(distro).unwrap().unwrap();
    let id = snapshot_id(1_758_153_600);
    let r = &def.releases[release];
    let repositories = r
        .repositories
        .iter()
        .map(|name| {
            let repo = &def.repositories[name];
            RepositoryEntry {
                name: name.clone(),
                url: render(&repo.snapshot_url, &[("snapshot_id", &id)]).unwrap(),
                suites: repo
                    .suites
                    .iter()
                    .map(|s| render(s, &[("release", &r.codename)]).unwrap())
                    .collect(),
                components: repo.components.clone(),
                indexes: Vec::new(),
            }
        })
        .collect();
    let mut base = fixture_base();
    base.distro = distro.to_string();
    base.release = r.codename.clone();
    base.repositories = repositories;
    base
}

/// Every generated build file's digest, `name=sha256:…` joined by one space, for one shape of
/// one base.
fn digests(
    distro: &str,
    release: &str,
    recommends: bool,
    locale: &str,
    has_packages: bool,
) -> String {
    let mut base = base_of(distro, release);
    base.options.install_recommends = recommends;
    base.options.locale = locale.to_string();
    let d: BTreeMap<String, String> = build_file_digests(&base, has_packages);
    d.iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `(distro, release, installRecommends, locale, has_packages)` and the digest of every file
/// the image build generates for it. Recorded from the tree as it stood before `src/distro/`
/// existed (M-Arch-Base T-1; the values are in the task's record).
#[rustfmt::skip]
const PINNED: &[(&str, &str, bool, &str, bool, &str)] = &[
    ("debian", "bookworm", false, "C.UTF-8", true,
     "90lodi=sha256:d847dd1ed28816cd9b4a1ba3d3fabcfd4ab7591516af8dbb6e0a2d394c6cf2cb \
      Containerfile=sha256:21d5d2ab5858e830e6e375cb301dd471268d10d18155c134a80086a34b50d1c3 \
      locale=sha256:89dd29db91ea608d72b5b4d3d3f5816cc2d3c1dd730741dc41b20ce12f1c2b3b \
      lodi.sources=sha256:c0f3997ebf3c213730b460858dfd99dd9de5d8009fadb9e3c4cf5f8175a06699 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
    ("debian", "bookworm", false, "C.UTF-8", false,
     "90lodi=sha256:d847dd1ed28816cd9b4a1ba3d3fabcfd4ab7591516af8dbb6e0a2d394c6cf2cb \
      Containerfile=sha256:8c158787e44ea2955640a7f6156d64ea9ca5971a125e9ce2739245f290ca34b8 \
      locale=sha256:89dd29db91ea608d72b5b4d3d3f5816cc2d3c1dd730741dc41b20ce12f1c2b3b \
      lodi.sources=sha256:c0f3997ebf3c213730b460858dfd99dd9de5d8009fadb9e3c4cf5f8175a06699 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
    ("debian", "bookworm", true, "en_US.UTF-8", true,
     "90lodi=sha256:3c6656ccef36a45f900522ab21d88abe80e317511337606e71b79b1fb6275c24 \
      Containerfile=sha256:21d5d2ab5858e830e6e375cb301dd471268d10d18155c134a80086a34b50d1c3 \
      locale=sha256:033252d5100cbb0028d7c15e075f63e4eb5ff7f1a923cd5cb547bcfd6b024f5b \
      lodi.sources=sha256:c0f3997ebf3c213730b460858dfd99dd9de5d8009fadb9e3c4cf5f8175a06699 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
    ("ubuntu", "noble", false, "C.UTF-8", true,
     "90lodi=sha256:d847dd1ed28816cd9b4a1ba3d3fabcfd4ab7591516af8dbb6e0a2d394c6cf2cb \
      Containerfile=sha256:2596ef4b45faa1c2d324d1ecf56e1bfa530f1992e18d57ab738dae5daf0f51ec \
      locale=sha256:89dd29db91ea608d72b5b4d3d3f5816cc2d3c1dd730741dc41b20ce12f1c2b3b \
      lodi.sources=sha256:a85569340cbe4c611528fe1229fef914cdf525f477dceea0ba7da18dfd8dee10 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
    ("ubuntu", "noble", false, "C.UTF-8", false,
     "90lodi=sha256:d847dd1ed28816cd9b4a1ba3d3fabcfd4ab7591516af8dbb6e0a2d394c6cf2cb \
      Containerfile=sha256:d1bd31884a15f273c17063926fd9c9884495aebe06b29080161a71ea1652b961 \
      locale=sha256:89dd29db91ea608d72b5b4d3d3f5816cc2d3c1dd730741dc41b20ce12f1c2b3b \
      lodi.sources=sha256:a85569340cbe4c611528fe1229fef914cdf525f477dceea0ba7da18dfd8dee10 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
    ("ubuntu", "noble", true, "en_US.UTF-8", true,
     "90lodi=sha256:3c6656ccef36a45f900522ab21d88abe80e317511337606e71b79b1fb6275c24 \
      Containerfile=sha256:2596ef4b45faa1c2d324d1ecf56e1bfa530f1992e18d57ab738dae5daf0f51ec \
      locale=sha256:033252d5100cbb0028d7c15e075f63e4eb5ff7f1a923cd5cb547bcfd6b024f5b \
      lodi.sources=sha256:a85569340cbe4c611528fe1229fef914cdf525f477dceea0ba7da18dfd8dee10 \
      policy-rc.d=sha256:c2bcd9decf63ff2c0d9f473f38bc3607900530aad80f99139855d56678456230"),
];

/// `container::image_identity` of the committed fixture lock's base, recorded before the move.
const PINNED_IDENTITY: &str =
    "sha256:2cdf87ad9d7a8e65cf5574c527497ffe62c747927258343a28b094fe4d608080";

/// Collapse the line continuations of a pinned value into single spaces.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[test]
fn every_generated_build_file_has_the_digest_it_had_before_the_seam() {
    for (distro, release, recommends, locale, has_packages, expected) in PINNED {
        let actual = digests(distro, release, *recommends, locale, *has_packages);
        assert_eq!(
            actual,
            normalize(expected),
            "{distro} {release} installRecommends={recommends} locale={locale} \
             has_packages={has_packages}: a generated build file changed, which changes every \
             image identity and rebuilds every warm environment",
        );
    }
}

#[test]
fn the_fixture_locks_image_identity_is_unchanged() {
    assert_eq!(
        image_identity(&fixture_base()),
        PINNED_IDENTITY,
        "the image identity of tests/fixtures/spike/lodi.lock changed",
    );
}

// ---------------------------------------------------------------------------------------------
// The seam itself. These are the tests of what T-1 built; the pins above are the tests of what
// it did not change.
// ---------------------------------------------------------------------------------------------

/// The `family` key, when a base definition names one, and the shape the two committed
/// definitions imply when they do not.
mod family {
    use lodi::catalogue::{builtin_base, parse_base};
    use lodi::distro::Family;

    /// A base definition, with `extra` spliced into its `[base]` table.
    fn definition(extra: &str, suites: &str, components: &str) -> String {
        format!(
            "[base]\ndistro = \"x\"\n{extra}\n\
             [release.r]\naliases = []\nsnapshot_min = \"2020-01-01T00:00:00Z\"\n\
             repositories = [\"a\"]\n\
             [release.r.rootfs.x86_64]\nstrategy = \"github_oci_layout\"\n\
             repo = \"o/r\"\nbranch = \"b\"\ndir = \"d\"\nlayer = \"l\"\n\
             [repository.a]\nsnapshot_url = \"https://example.invalid/${{snapshot_id}}/\"\n\
             suites = [{suites}]\ncomponents = [{components}]\n"
        )
    }

    #[test]
    fn both_committed_bases_are_apt_and_neither_names_a_family() {
        for distro in ["debian", "ubuntu"] {
            let def = builtin_base(distro).unwrap().unwrap();
            assert_eq!(def.family, Family::Apt, "{distro}");
            assert!(def.family.ops().is_ok(), "{distro}");
        }
    }

    #[test]
    fn a_family_the_definition_names_is_the_family_that_is_used() {
        let apt = parse_base(&definition("family = \"apt\"", "\"s\"", "\"c\""), "x.toml").unwrap();
        assert_eq!(apt.family, Family::Apt);
        // The second family is named the same way and is its own implementation, never
        // silently treated as apt: both halves of the seam answer, and differently.
        let pacman = parse_base(&definition("family = \"pacman\"", "", ""), "x.toml").unwrap();
        assert_eq!(pacman.family, Family::Pacman);
        assert!(pacman.family.ops().is_ok());
        assert_ne!(
            pacman.family.image_ops().package_mount(),
            apt.family.image_ops().package_mount()
        );
    }

    #[test]
    fn an_unknown_family_is_a_recipe_error_naming_the_file() {
        let e =
            parse_base(&definition("family = \"yum\"", "\"s\"", "\"c\""), "x.toml").unwrap_err();
        assert_eq!(e.code, "E_RECIPE_INVALID");
        assert!(e.message.contains("x.toml"), "{}", e.message);
        assert!(e.message.contains("yum"), "{}", e.message);
    }

    #[test]
    fn a_definition_that_implies_no_family_is_a_recipe_error_naming_the_file() {
        // Suites without components (and the other way round) name no family: there is no
        // default, so the definition is refused rather than guessed at.
        for (suites, components) in [("\"s\"", ""), ("", "\"c\"")] {
            let e = parse_base(&definition("", suites, components), "x.toml").unwrap_err();
            assert_eq!(e.code, "E_RECIPE_INVALID");
            assert!(e.message.contains("x.toml"), "{}", e.message);
        }
    }
}

/// The four distributions this build supports are one list, and a fifth is refused at exit 3
/// with all four named (M-Arch-Base T-6; Fedora joined with fd-1, LD-435).
#[test]
fn the_supported_distros_are_one_list_and_a_fourth_is_unsupported_at_exit_3() {
    use lodi::diag::exit_status;
    use lodi::manifest::{Distro, parse_project_manifest};

    for distro in lodi::lock::SUPPORTED_DISTROS {
        let kind = Distro::parse(distro).unwrap_or_else(|| panic!("{distro} parses"));
        assert_eq!(kind.name(), *distro);
        let release = kind.releases()[0];
        let text = format!("[container]\ndistro = \"{distro}\"\nrelease = \"{release}\"\n");
        parse_project_manifest(&text, "lodi.toml").unwrap_or_else(|e| panic!("{distro}: {e}"));
        assert!(
            lodi::catalogue::builtin_base(distro).unwrap().is_ok(),
            "{distro} has a built-in base definition"
        );
    }
    assert_eq!(
        lodi::lock::SUPPORTED_DISTROS,
        ["arch", "debian", "fedora", "ubuntu"],
        "lock validation and the manifest must know the same distributions"
    );
    // A distribution outside that list is refused with the whole list named, at the same exit
    // status an unsupported release has.
    assert!(Distro::parse("gentoo").is_none());
    let text = "[container]\ndistro = \"gentoo\"\nrelease = \"41\"\n";
    let errors = parse_project_manifest(text, "lodi.toml").unwrap_err();
    assert_eq!(errors.codes(), vec!["E_TYPE"]);
    assert_eq!(exit_status("E_TYPE"), exit_status("E_UNSUPPORTED"));
    assert_eq!(exit_status("E_TYPE"), 3);
    let shown = errors.to_string();
    for distro in lodi::lock::SUPPORTED_DISTROS {
        assert!(shown.contains(distro), "{distro} is not offered: {shown}");
    }
    // A release the distribution does not have is E_UNSUPPORTED, and the hint names the one
    // release a rolling distribution has rather than assuming there is a second.
    let text = "[container]\ndistro = \"arch\"\nrelease = \"2026.09.01\"\n";
    let errors = parse_project_manifest(text, "lodi.toml").unwrap_err();
    assert_eq!(errors.codes(), vec!["E_UNSUPPORTED"]);
    let shown = errors.to_string();
    assert!(shown.contains("arch \"rolling\""), "{shown}");
}

/// Nothing outside `src/distro/` names a package manager.
///
/// The exceptions are explicit and **checked**: a file listed here that no longer matches fails
/// the test, so an exception cannot outlive its reason.
///
/// - `src/debian/` is the apt implementation's own format modules — the `Packages` and `Release`
///   parsers, dpkg version ordering and the resolver. LD-74 keeps the module's name; it is
///   reached only through `src/distro/apt.rs`.
/// - `src/arch/` is the other family's format implementation and the two halves of its seam
///   implementation, reached only through `src/distro/`, exactly as `src/debian/` is.
/// - `src/hostscope/` is the **host** scope's own package-manager seam (M-0.6 T-1), a different
///   scope with no command behind it; this milestone does not touch it.
/// - `src/container.rs` names both managers in exactly one place: the hint that tells a user how
///   to install **Podman itself** on their host. That is not a container package operation, and
///   the test checks that this is the only line of that file that matches.
#[test]
fn nothing_outside_src_distro_names_a_package_manager() {
    const TOKENS: &[&str] = &[
        "apt-get",
        "dpkg",
        "pacman",
        "apt.conf",
        "sources.list",
        "lodi-debs",
        "APT::",
    ];
    const EXEMPT_DIRS: &[&str] = &["src/distro/", "src/debian/", "src/arch/", "src/hostscope/"];

    let src = root().join("src");
    let mut offenders = Vec::new();
    let mut podman_hint_lines = 0;
    let mut files = Vec::new();
    collect(&src, &mut files);
    files.sort();
    assert!(files.len() > 20, "the walk found almost nothing: {files:?}");
    for path in &files {
        let relative = path
            .strip_prefix(root())
            .unwrap()
            .to_string_lossy()
            .replace('\\', "/");
        if EXEMPT_DIRS.iter().any(|d| relative.starts_with(d)) {
            continue;
        }
        for (n, line) in fs::read_to_string(path).unwrap().lines().enumerate() {
            if !TOKENS.iter().any(|t| line.contains(t)) {
                continue;
            }
            // The one exempt line: installing Podman itself on the host.
            if relative == "src/container.rs" && line.contains("podman") {
                podman_hint_lines += 1;
                continue;
            }
            offenders.push(format!("{relative}:{}: {}", n + 1, line.trim()));
        }
    }
    assert!(
        offenders.is_empty(),
        "these name a package manager outside src/distro/:\n{}",
        offenders.join("\n")
    );
    assert_eq!(
        podman_hint_lines, 2,
        "the only package-manager names left in src/container.rs are the two lines of the \
         hint for installing Podman on the host; that is no longer true",
    );
    // Every exempt directory must still exist and still match, or the exception is stale.
    for dir in EXEMPT_DIRS {
        let mut in_dir = Vec::new();
        collect(&root().join(dir), &mut in_dir);
        assert!(!in_dir.is_empty(), "{dir} does not exist any more");
        let matches = in_dir.iter().any(|p| {
            let text = fs::read_to_string(p).unwrap();
            TOKENS.iter().any(|t| text.contains(t))
        });
        assert!(
            matches,
            "{dir} no longer names a package manager: drop the exception"
        );
    }
}

/// Every `*.rs` file below `dir`.
fn collect(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            out.push(path);
        }
    }
}
