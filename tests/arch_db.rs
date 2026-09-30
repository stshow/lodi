mod support;

use lodi::arch::db::{Dependency, DependencyVersion, Limits, Relation, parse, parse_with_limits};
use support::{Kind, Member, file, gzip, tar};

const HASH_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const HASH_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

fn stanza(name: &str, version: &str, hash: &str) -> String {
    format!(
        "%FILENAME%\n{name}-{version}-x86_64.pkg.tar.zst\n\n\
         %NAME%\n{name}\n\n\
         %VERSION%\n{version}\n\n\
         %DESC%\n{name} short description\na second line ignored by browsing\n\n\
         %CSIZE%\n12345\n\n\
         %ISIZE%\n67890\n\n\
         %MD5SUM%\ndeadbeef\n\n\
         %SHA256SUM%\n{hash}\n\n\
         %PGPSIG%\nignored-signature\n\n\
         %URL%\nhttps://example.invalid/{name}\n\n\
         %LICENSE%\nGPL\n\n\
         %ARCH%\nx86_64\n\n\
         %BUILDDATE%\n1700000000\n\n\
         %PACKAGER%\nArch packager\n\n\
         %BASE%\n{name}\n\n\
         %GROUPS%\nbase-devel\nutilities\n\n\
         %DEPENDS%\nglibc>=2.39\nlibcrypto.so=3-64\n\n\
         %OPTDEPENDS%\ndocs: optional prose\n\n\
         %PROVIDES%\n{name}-virtual={version}\nlib{name}.so=1-64\n\n\
         %REPLACES%\nold-{name}<1.0\n\n\
         %CONFLICTS%\nother-{name}\n\n\
         %UNKNOWN-FUTURE-KEY%\nignored\n\n"
    )
}

fn database(entries: &[(&str, &str, &str)]) -> Vec<u8> {
    let members: Vec<Member> = entries
        .iter()
        .map(|(name, version, hash)| {
            file(
                &format!("{name}-{version}/desc"),
                &stanza(name, version, hash),
                0o644,
            )
        })
        .collect();
    gzip(&tar(&members))
}

#[test]
fn synthetic_database_is_sorted_and_carries_every_field() {
    let bytes = database(&[("zeta", "1:2.0-3", HASH_B), ("alpha", "1.2.3-4", HASH_A)]);
    let packages = parse(&bytes, "core").unwrap();
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].name, "alpha");
    assert_eq!(packages[1].name, "zeta");
    let package = &packages[0];
    assert_eq!(package.version.as_str(), "1.2.3-4");
    assert_eq!(package.arch, "x86_64");
    assert_eq!(package.filename, "alpha-1.2.3-4-x86_64.pkg.tar.zst");
    assert_eq!(package.sha256, HASH_A);
    assert_eq!(package.size, Some(12345));
    assert_eq!(package.repository, "core");
    assert_eq!(package.description, "alpha short description");
    assert_eq!(package.groups, ["base-devel", "utilities"]);
    assert_eq!(package.depends.len(), 2);
    assert_eq!(package.provides.len(), 2);
    assert_eq!(package.replaces.len(), 1);
    assert_eq!(package.conflicts.len(), 1);
    assert_eq!(package.depends[0].name, "glibc");
    assert!(matches!(
        &package.depends[0].constraint,
        Some((Relation::GreaterEqual, DependencyVersion::Package(version)))
            if version.as_str() == "2.39"
    ));
    assert!(matches!(
        &package.depends[1].constraint,
        Some((Relation::Equal, DependencyVersion::SonameAbi(abi))) if abi == "3-64"
    ));
}

#[test]
fn dependency_operators_and_soname_type_are_explicit() {
    for (text, relation) in [
        ("a<1", Relation::Less),
        ("a<=1", Relation::LessEqual),
        ("a=1", Relation::Equal),
        ("a>=1", Relation::GreaterEqual),
        ("a>1", Relation::Greater),
    ] {
        assert_eq!(
            Dependency::parse(text).unwrap().constraint.unwrap().0,
            relation
        );
    }
    assert!(
        Dependency::parse("plain-name")
            .unwrap()
            .constraint
            .is_none()
    );
    assert!(matches!(
        Dependency::parse("libthing.so=7-64").unwrap().constraint,
        Some((Relation::Equal, DependencyVersion::SonameAbi(ref value))) if value == "7-64"
    ));
}

fn remove_block(text: &str, key: &str) -> String {
    text.split("\n\n")
        .filter(|block| !block.starts_with(key))
        .collect::<Vec<_>>()
        .join("\n\n")
        + "\n\n"
}

#[test]
fn every_missing_mandatory_key_names_the_key_and_member() {
    let member = "alpha-1.2.3-4/desc";
    for key in ["%NAME%", "%VERSION%", "%FILENAME%", "%ARCH%", "%SHA256SUM%"] {
        let desc = remove_block(&stanza("alpha", "1.2.3-4", HASH_A), key);
        let bytes = gzip(&tar(&[file(member, &desc, 0o644)]));
        let error = parse(&bytes, "core").unwrap_err();
        assert!(error.contains(member), "{key}: {error}");
        assert!(error.contains(key), "{key}: {error}");
    }
}

#[test]
fn malformed_hash_path_and_dependency_are_distinct_errors() {
    let member = "alpha-1.2.3-4/desc";
    let bad_hash = database(&[("alpha", "1.2.3-4", "ABC")]);
    let error = parse(&bad_hash, "core").unwrap_err();
    assert!(
        error.contains(member) && error.contains("malformed SHA256SUM `ABC`"),
        "{error}"
    );

    let bad_path = gzip(&tar(&[file(
        "alpha/metadata",
        &stanza("alpha", "1.2.3-4", HASH_A),
        0o644,
    )]));
    let error = parse(&bad_path, "core").unwrap_err();
    assert!(
        error.contains("alpha/metadata") && error.contains("member path"),
        "{error}"
    );

    let desc = stanza("alpha", "1.2.3-4", HASH_A).replace("glibc>=2.39", "glibc=>2.39");
    let malformed = gzip(&tar(&[file(member, &desc, 0o644)]));
    let error = parse(&malformed, "core").unwrap_err();
    assert!(
        error.contains(member) && error.contains("`glibc=>2.39`"),
        "{error}"
    );
}

#[test]
fn all_three_resource_bounds_trip() {
    let bytes = database(&[("alpha", "1.2.3-4", HASH_A)]);
    let error = parse_with_limits(
        &bytes,
        "core",
        Limits {
            max_decompressed_size: 511,
            max_members: 100,
            max_member_size: 4096,
        },
    )
    .unwrap_err();
    assert!(
        error.contains("decompressed size limit of 511 bytes"),
        "{error}"
    );

    let error = parse_with_limits(
        &bytes,
        "core",
        Limits {
            max_decompressed_size: 1_000_000,
            max_members: 0,
            max_member_size: 4096,
        },
    )
    .unwrap_err();
    assert!(error.contains("member count limit of 0"), "{error}");

    let error = parse_with_limits(
        &bytes,
        "core",
        Limits {
            max_decompressed_size: 1_000_000,
            max_members: 100,
            max_member_size: 10,
        },
    )
    .unwrap_err();
    assert!(
        error.contains("member size limit of 10 bytes") && error.contains("alpha"),
        "{error}"
    );
}

#[test]
fn real_core_database_slice_parses() {
    let encoded = include_str!("fixtures/arch/core-2024-09-01.slice.db.hex");
    let bytes = decode_hex(encoded);
    let packages = parse(&bytes, "core").unwrap();
    assert_eq!(packages.len(), 2);
    assert_eq!(
        packages.iter().map(|p| p.name.as_str()).collect::<Vec<_>>(),
        ["acl", "archlinux-keyring"]
    );
}

fn decode_hex(text: &str) -> Vec<u8> {
    let digits: Vec<u8> = text
        .bytes()
        .filter(|byte| !byte.is_ascii_whitespace())
        .collect();
    assert_eq!(digits.len() % 2, 0);
    digits
        .chunks_exact(2)
        .map(|pair| {
            let high = (pair[0] as char).to_digit(16).unwrap();
            let low = (pair[1] as char).to_digit(16).unwrap();
            ((high << 4) | low) as u8
        })
        .collect()
}

#[test]
fn non_regular_member_is_not_silently_ignored() {
    let bytes = gzip(&tar(&[Member {
        path: b"alpha-1.0-1/desc".to_vec(),
        kind: Kind::Symlink("elsewhere".into()),
    }]));
    let error = parse(&bytes, "core").unwrap_err();
    assert!(
        error.contains("alpha-1.0-1/desc") && error.contains("member path"),
        "{error}"
    );
}
