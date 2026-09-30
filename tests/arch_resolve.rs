mod support;

use lodi::arch::db::{Dependency, Package, parse as parse_database};
use lodi::arch::resolve::{Limits, Resolution, resolve, resolve_with_limits};
use lodi::arch::version::PacmanVersion;
use lodi::debian::resolve::Origin;
use lodi::diag::{Diagnostic, exit_status};
use support::{file, gzip, tar};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const FIXTURE: &str = include_str!("fixtures/arch/resolver-packages.tsv");

fn dependency(text: &str) -> Dependency {
    Dependency::parse(text).unwrap()
}

fn package(name: &str, version: &str) -> Package {
    Package {
        name: name.to_string(),
        version: PacmanVersion::parse(version).unwrap(),
        arch: "x86_64".to_string(),
        filename: format!("{name}-{version}-x86_64.pkg.tar.zst"),
        sha256: HASH.to_string(),
        size: Some(100),
        depends: Vec::new(),
        provides: Vec::new(),
        replaces: Vec::new(),
        conflicts: Vec::new(),
        groups: Vec::new(),
        description: format!("the {name} package"),
        repository: "core".to_string(),
    }
}

fn names(resolution: &Resolution) -> Vec<&str> {
    resolution
        .closure
        .iter()
        .map(|package| package.name.as_str())
        .collect()
}

fn fail(result: Result<Resolution, Diagnostic>, parts: &[&str]) -> Diagnostic {
    let diagnostic = result.unwrap_err();
    assert_eq!(diagnostic.code, "E_NO_MATCH", "{diagnostic}");
    assert_eq!(exit_status(diagnostic.code), 4, "{diagnostic}");
    let rendered = diagnostic.to_string();
    for part in parts {
        assert!(rendered.contains(part), "missing {part:?}: {rendered}");
    }
    diagnostic
}

#[test]
fn versioned_dependencies_accept_real_packages_and_versioned_provides() {
    let real = package("libreal", "2.4-1");
    let mut real_consumer = package("real-consumer", "1.0-1");
    real_consumer.depends.push(dependency("libreal>=2.0"));

    let mut provider = package("crypto-impl", "3.0-1");
    provider.provides.push(dependency("crypto=3.0"));
    let mut provided_consumer = package("provided-consumer", "1.0-1");
    provided_consumer.depends.push(dependency("crypto>=2.9"));

    let resolution = resolve(
        &[real, real_consumer, provider, provided_consumer],
        &[],
        &["real-consumer".into(), "provided-consumer".into()],
    )
    .unwrap();
    assert_eq!(
        names(&resolution),
        [
            "crypto-impl",
            "libreal",
            "provided-consumer",
            "real-consumer"
        ]
    );
}

#[test]
fn an_unversioned_provide_never_satisfies_a_versioned_dependency() {
    let mut provider = package("feature-impl", "9.0-1");
    provider.provides.push(dependency("feature"));
    let mut consumer = package("consumer", "1.0-1");
    consumer.depends.push(dependency("feature>=2.0"));

    fail(
        resolve(&[provider, consumer], &[], &["consumer".into()]),
        &["feature>=2.0", "consumer 1.0-1"],
    );
}

#[test]
fn soname_abi_tags_are_compared_as_literal_strings() {
    let mut exact = package("exact-crypto", "1.0-1");
    exact.provides.push(dependency("libcrypto.so=3-64"));
    let mut consumer = package("consumer", "1.0-1");
    consumer.depends.push(dependency("libcrypto.so=3-64"));
    assert_eq!(
        names(&resolve(&[exact, consumer.clone()], &[], &["consumer".into()]).unwrap()),
        ["consumer", "exact-crypto"]
    );

    // Pacman package versions consider these equal because numeric segments ignore leading
    // zeroes. ABI tags do not: reversing the literal rule would make this resolution succeed.
    let mut leading_zero = package("leading-zero-crypto", "1.0-1");
    leading_zero.provides.push(dependency("libcrypto.so=03-64"));
    fail(
        resolve(&[leading_zero, consumer], &[], &["consumer".into()]),
        &["libcrypto.so=3-64", "consumer 1.0-1"],
    );
}

#[test]
fn replaces_are_used_only_after_names_and_providers_and_ambiguity_names_every_candidate() {
    let mut replacement = package("new-name", "2.0-1");
    replacement.replaces.push(dependency("old-name"));
    let resolution = resolve(&[replacement], &[], &["old-name".into()]).unwrap();
    assert_eq!(resolution.requested, ["old-name"]);
    assert_eq!(names(&resolution), ["new-name"]);

    let mut first = package("first-new-name", "2.0-1");
    first.replaces.push(dependency("old-name"));
    let mut second = package("second-new-name", "2.0-1");
    second.replaces.push(dependency("old-name"));
    fail(
        resolve(&[first, second], &[], &["old-name".into()]),
        &[
            "old-name",
            "replacements",
            "first-new-name",
            "second-new-name",
        ],
    );

    let mut provider = package("provided-name", "1.0-1");
    provider.provides.push(dependency("old-name"));
    let mut ignored_replacement = package("replacement", "9.0-1");
    ignored_replacement.replaces.push(dependency("old-name"));
    assert_eq!(
        names(&resolve(&[provider, ignored_replacement], &[], &["old-name".into()]).unwrap()),
        ["provided-name"]
    );
}

#[test]
fn ambiguous_providers_name_every_candidate_and_choose_none() {
    let mut first = package("first-provider", "1.0-1");
    first.provides.push(dependency("virtual"));
    let mut second = package("second-provider", "1.0-1");
    second.provides.push(dependency("virtual"));
    fail(
        resolve(&[first, second], &[], &["virtual".into()]),
        &["virtual", "providers", "first-provider", "second-provider"],
    );
}

#[test]
fn groups_record_sorted_members_and_a_package_name_wins_a_collision() {
    let mut alpha = package("alpha", "1.0-1");
    alpha.groups.extend(["toolchain".into(), "dev".into()]);
    let mut beta = package("beta", "1.0-1");
    beta.groups.extend(["toolchain".into(), "dev".into()]);
    let dev = package("dev", "7.0-1");
    let index = [beta, dev, alpha];

    let expanded = resolve(&index, &[], &["toolchain".into()]).unwrap();
    assert_eq!(expanded.requested, ["alpha", "beta"]);
    assert_eq!(names(&expanded), ["alpha", "beta"]);
    let replay = resolve(&index, &[], &expanded.requested).unwrap();
    assert_eq!(replay.closure, expanded.closure);

    let collision = resolve(&index, &[], &["dev".into()]).unwrap();
    assert_eq!(collision.requested, ["dev"]);
    assert_eq!(names(&collision), ["dev"]);
}

#[test]
fn conflicts_inside_the_resolved_set_are_refused_and_name_both_packages() {
    let mut alpha = package("alpha", "1.0-1");
    alpha.conflicts.push(dependency("beta>=2.0"));
    let beta = package("beta", "2.0-1");
    fail(
        resolve(&[alpha, beta], &[], &["alpha".into(), "beta".into()]),
        &["alpha 1.0-1", "beta>=2.0", "beta 2.0-1"],
    );
}

#[test]
fn optional_dependencies_parsed_from_a_database_never_enter_the_closure() {
    fn desc(name: &str, optdepends: &str) -> String {
        format!(
            "%FILENAME%\n{name}-1.0-1-x86_64.pkg.tar.zst\n\n\
             %NAME%\n{name}\n\n\
             %VERSION%\n1.0-1\n\n\
             %ARCH%\nx86_64\n\n\
             %CSIZE%\n100\n\n\
             %SHA256SUM%\n{HASH}\n\n\
             %OPTDEPENDS%\n{optdepends}\n\n"
        )
    }
    let bytes = gzip(&tar(&[
        file(
            "application-1.0-1/desc",
            &desc("application", "documentation: offline manual"),
            0o644,
        ),
        file(
            "documentation-1.0-1/desc",
            &desc("documentation", "none: irrelevant"),
            0o644,
        ),
    ]));
    let index = parse_database(&bytes, "extra").unwrap();
    assert_eq!(
        names(&resolve(&index, &[], &["application".into()]).unwrap()),
        ["application"]
    );
}

#[test]
fn all_resolution_bounds_trip_with_the_code_status_and_named_limit() {
    let a = package("a", "1.0-1");
    let b = package("b", "1.0-1");
    fail(
        resolve_with_limits(
            &[a.clone(), b.clone()],
            &[],
            &["a".into(), "b".into()],
            Limits {
                max_requests: 1,
                max_closure_size: 10,
                max_iterations: 10,
            },
        ),
        &["request count limit of 1"],
    );

    let mut requiring = a.clone();
    requiring.depends.push(dependency("b"));
    fail(
        resolve_with_limits(
            &[requiring.clone(), b.clone()],
            &[],
            &["a".into()],
            Limits {
                max_requests: 10,
                max_closure_size: 1,
                max_iterations: 10,
            },
        ),
        &["closure size limit of 1"],
    );
    fail(
        resolve_with_limits(
            &[requiring, b],
            &[],
            &["a".into()],
            Limits {
                max_requests: 10,
                max_closure_size: 10,
                max_iterations: 1,
            },
        ),
        &["iteration count limit of 1"],
    );
}

#[test]
fn failures_name_relations_candidates_and_nearest_names_from_descriptions() {
    let mut old = package("library", "1.0-1");
    old.description = "fast recursive text search".into();
    fail(
        resolve(&[old.clone()], &[], &["recursive".into()]),
        &["recursive", "did you mean library"],
    );

    let mut consumer = package("consumer", "1.0-1");
    consumer.depends.push(dependency("library>=2.0"));
    fail(
        resolve(&[old, consumer], &[], &["consumer".into()]),
        &["library>=2.0", "library 1.0-1", "consumer 1.0-1"],
    );
}

#[test]
fn a_missing_compressed_size_is_a_coded_failure_not_a_silent_zero() {
    let mut incomplete = package("incomplete", "1.0-1");
    incomplete.size = None;
    let diagnostic = resolve(&[incomplete], &[], &["incomplete".into()]).unwrap_err();
    assert_eq!(diagnostic.code, "E_RECIPE_INVALID", "{diagnostic}");
    assert_eq!(exit_status(diagnostic.code), 4, "{diagnostic}");
    assert!(diagnostic.to_string().contains("%CSIZE%"), "{diagnostic}");
}

#[test]
fn committed_fixture_closure_is_sorted_and_byte_stable_across_runs() {
    let index = fixture_packages();
    let first = resolve(&index, &[], &["gamma".into()]).unwrap();
    assert_eq!(names(&first), ["beta", "gamma"]);
    assert!(
        first
            .closure
            .windows(2)
            .all(|pair| pair[0].name < pair[1].name)
    );
    let expected = closure_bytes(&first);
    for _ in 0..16 {
        assert_eq!(
            closure_bytes(&resolve(&index, &[], &["gamma".into()]).unwrap()),
            expected
        );
    }
}

fn fixture_packages() -> Vec<Package> {
    FIXTURE
        .lines()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let columns: Vec<&str> = line.split('\t').collect();
            let [
                name,
                version,
                depends,
                provides,
                replaces,
                conflicts,
                groups,
                description,
            ] = columns.as_slice()
            else {
                panic!("fixture row does not have eight columns: {line}");
            };
            let mut result = package(name, version);
            result.depends = dependency_list(depends);
            result.provides = dependency_list(provides);
            result.replaces = dependency_list(replaces);
            result.conflicts = dependency_list(conflicts);
            result.groups = list(groups).into_iter().map(str::to_string).collect();
            result.description = description.to_string();
            result
        })
        .collect()
}

fn dependency_list(text: &str) -> Vec<Dependency> {
    list(text).into_iter().map(dependency).collect()
}

fn list(text: &str) -> Vec<&str> {
    if text == "-" {
        Vec::new()
    } else {
        text.split(',').collect()
    }
}

fn closure_bytes(resolution: &Resolution) -> Vec<u8> {
    let mut bytes = Vec::new();
    for package in &resolution.closure {
        let artifact = package.artifact.as_ref().unwrap();
        bytes.extend_from_slice(
            format!(
                "{}\t{}\t{}\t{}\t{}\t{}\t{}\t{}\n",
                package.name,
                package.version,
                package.arch.as_deref().unwrap_or("-"),
                match package.origin {
                    Origin::Base => "base",
                    Origin::Install => "install",
                },
                artifact.filename,
                artifact.sha256,
                artifact.size,
                artifact.repository
            )
            .as_bytes(),
        );
    }
    bytes
}
