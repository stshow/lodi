//! Debian repository indexes: the suite `Release` file and the `Packages` index of one
//! component and architecture.
//!
//! Integrity (design `spec/05` §4.1, LD-17): the `Release` file is fetched over HTTPS from the
//! snapshot service and its SHA-256 is recorded in the lock; its OpenPGP signature is not
//! verified by this build. Every `Packages` index must match the size and SHA-256 that
//! `Release` lists for it, or the lock fails with `E_HASH_MISMATCH`.

use std::collections::BTreeMap;

use super::control::{Relationships, Stanza, parse_relationships, parse_stanzas};
use super::version::{DebVersion, Relation};

/// The parts of a suite's `Release` file that locking uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub suite: String,
    pub codename: String,
    pub architectures: Vec<String>,
    pub components: Vec<String>,
    /// `SHA256:` entries: path relative to the suite directory -> (hex digest, size).
    pub sha256: BTreeMap<String, (String, u64)>,
}

impl Release {
    pub fn parse(text: &str) -> Result<Release, String> {
        let stanzas = parse_stanzas(text)?;
        let [stanza] = stanzas.as_slice() else {
            return Err(format!(
                "expected one stanza in a Release file, found {}",
                stanzas.len()
            ));
        };
        let field = |name: &str| {
            stanza
                .get(name)
                .map(str::to_string)
                .ok_or_else(|| format!("Release file has no `{name}` field"))
        };
        let words = |name: &str| -> Result<Vec<String>, String> {
            Ok(field(name)?
                .split_whitespace()
                .map(str::to_string)
                .collect())
        };
        let mut sha256 = BTreeMap::new();
        for line in field("SHA256")?.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let parts: Vec<&str> = line.split_whitespace().collect();
            let [hash, size, path] = parts.as_slice() else {
                return Err(format!("malformed SHA256 line in Release: `{line}`"));
            };
            if !is_sha256_hex(hash) {
                return Err(format!("malformed SHA-256 `{hash}` in Release"));
            }
            let size = size
                .parse::<u64>()
                .map_err(|_| format!("malformed size `{size}` in Release"))?;
            sha256.insert(path.to_string(), (hash.to_string(), size));
        }
        Ok(Release {
            suite: field("Suite")?,
            codename: field("Codename")?,
            architectures: words("Architectures")?,
            components: words("Components")?,
            sha256,
        })
    }
}

/// Whether `text` is 64 lowercase hexadecimal digits.
pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One binary package from a `Packages` index.
#[derive(Debug, Clone)]
pub struct BinaryPackage {
    pub name: String,
    pub version: DebVersion,
    /// `amd64` or `all`.
    pub arch: String,
    pub filename: String,
    pub sha256: String,
    pub size: u64,
    /// `Pre-Depends` followed by `Depends`.
    pub depends: Relationships,
    /// Virtual names this package provides, with the optional `(= version)`.
    pub provides: Vec<(String, Option<DebVersion>)>,
    pub conflicts: Relationships,
    pub breaks: Relationships,
    pub essential: bool,
    pub priority: String,
    /// The repository name (`debian`, `debian-security`) and suite the entry came from.
    pub repository: String,
    pub suite: String,
}

/// Parse a `Packages` index. Entries for other architectures are refused: the index of
/// `binary-<arch>` lists only that architecture and `all`.
pub fn parse_packages(
    text: &str,
    arch: &str,
    repository: &str,
    suite: &str,
) -> Result<Vec<BinaryPackage>, String> {
    let mut out = Vec::new();
    for stanza in parse_stanzas(text)? {
        out.push(binary_package(&stanza, arch, repository, suite)?);
    }
    Ok(out)
}

fn binary_package(
    s: &Stanza,
    arch: &str,
    repository: &str,
    suite: &str,
) -> Result<BinaryPackage, String> {
    let name = s
        .get("Package")
        .ok_or("a Packages stanza has no `Package` field")?
        .to_string();
    let need = |field: &str| {
        s.get(field)
            .map(str::to_string)
            .ok_or_else(|| format!("package `{name}` has no `{field}` field"))
    };
    let version = DebVersion::parse(&need("Version")?)?;
    let package_arch = need("Architecture")?;
    if package_arch != arch && package_arch != "all" {
        return Err(format!(
            "package `{name}` has architecture `{package_arch}` in the {arch} index"
        ));
    }
    let sha256 = need("SHA256")?;
    if !is_sha256_hex(&sha256) {
        return Err(format!(
            "package `{name}` has a malformed SHA256 `{sha256}`"
        ));
    }
    let size = need("Size")?
        .parse::<u64>()
        .map_err(|_| format!("package `{name}` has a malformed Size"))?;
    let relationships = |field: &str| -> Result<Relationships, String> {
        match s.get(field) {
            Some(text) => {
                parse_relationships(text).map_err(|e| format!("package `{name}` {field}: {e}"))
            }
            None => Ok(Vec::new()),
        }
    };
    let mut depends = relationships("Pre-Depends")?;
    depends.extend(relationships("Depends")?);
    let mut provides = Vec::new();
    for group in relationships("Provides")? {
        for dep in group {
            let version = match dep.version {
                None => None,
                Some((Relation::Equal, v)) => Some(v),
                Some(_) => {
                    return Err(format!(
                        "package `{name}` Provides `{}` with a relation other than `=`",
                        dep.name
                    ));
                }
            };
            provides.push((dep.name, version));
        }
    }
    Ok(BinaryPackage {
        version,
        arch: package_arch,
        filename: need("Filename")?,
        sha256,
        size,
        depends,
        provides,
        conflicts: relationships("Conflicts")?,
        breaks: relationships("Breaks")?,
        essential: s.get("Essential") == Some("yes"),
        priority: s.get("Priority").unwrap_or("optional").to_string(),
        repository: repository.to_string(),
        suite: suite.to_string(),
        name,
    })
}

/// One row `lodi search` shows for a distro package: the name, the version as written and the
/// short description (the first line of `Description`, empty when the index carries none).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexHit {
    pub name: String,
    pub version: String,
    pub description: String,
}

/// Scan a `Packages` index for the entries whose name or short description contains `query`
/// (ASCII case-insensitively), for `lodi search` (M-0.4 T-6, design call D13).
///
/// This is deliberately **not** [`parse_packages`]: browsing must not fail because one stanza of
/// a 50 MB index lacks a field the resolver needs. It reads the three fields it shows, skips a
/// stanza with no `Package`, and never allocates a [`BinaryPackage`]. Nothing here fetches,
/// writes or verifies anything; the caller has already checked the file against the lock.
pub fn search_index(text: &str, query: &str) -> Vec<IndexHit> {
    let query = query.to_ascii_lowercase();
    let mut hits = Vec::new();
    for stanza in text.split("\n\n") {
        let mut name = "";
        let mut version = "";
        let mut description = "";
        for line in stanza.lines() {
            // Continuation lines of a folded field start with a space or a tab; the short
            // description is the first line of `Description`, so they are all skipped.
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            if field.starts_with([' ', '\t']) {
                continue;
            }
            let value = value.trim();
            match field {
                "Package" => name = value,
                "Version" => version = value,
                "Description" => description = value,
                _ => {}
            }
        }
        if name.is_empty() {
            continue;
        }
        if name.to_ascii_lowercase().contains(&query)
            || description.to_ascii_lowercase().contains(&query)
        {
            hits.push(IndexHit {
                name: name.to_string(),
                version: version.to_string(),
                description: description.to_string(),
            });
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASE: &str = "Origin: Debian
Label: Debian
Suite: stable
Version: 12.12
Codename: bookworm
Date: Sat, 06 Sep 2025 10:00:00 UTC
Acquire-By-Hash: yes
Architectures: all amd64 arm64
Components: main contrib non-free-firmware
Description: Debian 12.12 Released 06 September 2025
MD5Sum:
 0123456789abcdef0123456789abcdef 100 main/binary-amd64/Packages.xz
SHA256:
 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1000 main/binary-amd64/Packages
 bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb  200 main/binary-amd64/Packages.xz
";

    #[test]
    fn release_file() {
        let r = Release::parse(RELEASE).unwrap();
        assert_eq!(
            (r.suite.as_str(), r.codename.as_str()),
            ("stable", "bookworm")
        );
        assert_eq!(r.architectures, ["all", "amd64", "arm64"]);
        assert_eq!(r.components[0], "main");
        assert_eq!(r.sha256["main/binary-amd64/Packages.xz"].1, 200);
        assert!(Release::parse("Suite: x\n").is_err());
        let bad = RELEASE.replace(" 200 ", " x ");
        assert!(Release::parse(&bad).is_err());
    }

    #[test]
    fn the_search_scan_reads_only_what_it_shows() {
        let text = "Package: python3\nVersion: 3.11.2-1+b1\nArchitecture: amd64\n\
                    Description: interactive high-level object-oriented language\n\
                     This package is a dependency package.\n\n\
                    Package: cowsay\nVersion: 3.03+dfsg2-8\n\
                    Description: configurable talking cow\n\n\
                    Package: broken-but-listed\n\n\
                    Description: a stanza with no Package field is skipped\n";
        let hits = search_index(text, "PYTHON");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "python3");
        assert_eq!(hits[0].version, "3.11.2-1+b1");
        assert_eq!(
            hits[0].description,
            "interactive high-level object-oriented language"
        );
        // The description matches too, and a stanza with no `Package` is not a row.
        let by_description = search_index(text, "talking cow");
        assert_eq!(by_description.len(), 1);
        assert_eq!(by_description[0].name, "cowsay");
        assert!(search_index(text, "no Package field").is_empty());
        // A field the resolver needs but this scan does not is not required.
        assert_eq!(search_index(text, "broken-but-listed").len(), 1);
    }

    #[test]
    fn packages_index() {
        let text = format!(
            "Package: libfoo1\nVersion: 1.0-1\nArchitecture: amd64\nPre-Depends: libc6 (>= 2.34)\n\
             Depends: libbar | libbaz\nProvides: foo-abi (= 1)\nBreaks: libfoo0 (<< 1.0)\n\
             Filename: pool/main/f/foo/libfoo1_1.0-1_amd64.deb\nSize: 42\nSHA256: {}\n\n\
             Package: base-files\nVersion: 12.4\nArchitecture: amd64\nEssential: yes\n\
             Priority: required\nFilename: pool/b.deb\nSize: 1\nSHA256: {}\n",
            "c".repeat(64),
            "d".repeat(64)
        );
        let p = parse_packages(&text, "amd64", "debian", "bookworm").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].depends.len(), 2);
        assert_eq!(p[0].depends[0][0].name, "libc6");
        assert_eq!(p[0].provides[0].0, "foo-abi");
        assert!(p[1].essential && p[1].priority == "required");
        let wrong_arch = text.replace("Architecture: amd64\nPre", "Architecture: arm64\nPre");
        assert!(parse_packages(&wrong_arch, "amd64", "debian", "bookworm").is_err());
        let no_hash = text.replace(&format!("SHA256: {}", "c".repeat(64)), "SHA256: xyz");
        assert!(parse_packages(&no_hash, "amd64", "debian", "bookworm").is_err());
    }
}
