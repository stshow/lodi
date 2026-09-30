//! Bounded parser for Arch Linux repository databases (`core.db`, `extra.db`).
//!
//! The database is a gzip-compressed tar stream of `<name>-<version>/desc` members. Its detached
//! OpenPGP signature is not verified by this build (LD-57); callers pin the database bytes over
//! HTTPS and every package artifact is then pinned by the `%SHA256SUM%` carried here.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::io::{Cursor, Read};

use flate2::read::GzDecoder;

use super::version::PacmanVersion;
use crate::debian::index::is_sha256_hex;

/// Production limits for one repository database.
pub const LIMITS: Limits = Limits {
    max_decompressed_size: 128 * 1024 * 1024,
    max_members: 100_000,
    max_member_size: 4 * 1024 * 1024,
};

/// Resource bounds applied before stanza parsing allocates from hostile input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_decompressed_size: u64,
    pub max_members: u64,
    pub max_member_size: u64,
}

/// A comparison operator in an Arch dependency expression.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Less,
    LessEqual,
    Equal,
    GreaterEqual,
    Greater,
}

impl Relation {
    pub fn holds(self, ordering: Ordering) -> bool {
        match self {
            Self::Less => ordering == Ordering::Less,
            Self::LessEqual => ordering != Ordering::Greater,
            Self::Equal => ordering == Ordering::Equal,
            Self::GreaterEqual => ordering != Ordering::Less,
            Self::Greater => ordering == Ordering::Greater,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Less => "<",
            Self::LessEqual => "<=",
            Self::Equal => "=",
            Self::GreaterEqual => ">=",
            Self::Greater => ">",
        }
    }
}

/// The right-hand side of a dependency. Soname ABI tags are deliberately a separate variant so
/// the resolver cannot accidentally feed them to the package-version comparator (design call D7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DependencyVersion {
    Package(PacmanVersion),
    SonameAbi(String),
}

/// `name` or `name<op>version` from `%DEPENDS%`, `%PROVIDES%`, `%REPLACES%` or `%CONFLICTS%`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub constraint: Option<(Relation, DependencyVersion)>,
}

impl Dependency {
    pub fn parse(text: &str) -> Result<Self, String> {
        let malformed = || format!("malformed Arch dependency expression `{text}`");
        if text.is_empty() || !text.is_ascii() || text.chars().any(char::is_whitespace) {
            return Err(malformed());
        }
        let operator = text
            .char_indices()
            .find(|(_, c)| matches!(c, '<' | '=' | '>'))
            .map(|(at, _)| at);
        let (name, constraint) = match operator {
            None => (text, None),
            Some(at) => {
                let (name, tail) = text.split_at(at);
                let (relation, value) = if let Some(value) = tail.strip_prefix("<=") {
                    (Relation::LessEqual, value)
                } else if let Some(value) = tail.strip_prefix(">=") {
                    (Relation::GreaterEqual, value)
                } else if let Some(value) = tail.strip_prefix('<') {
                    (Relation::Less, value)
                } else if let Some(value) = tail.strip_prefix('=') {
                    (Relation::Equal, value)
                } else if let Some(value) = tail.strip_prefix('>') {
                    (Relation::Greater, value)
                } else {
                    return Err(malformed());
                };
                if value.is_empty() || value.bytes().any(|b| matches!(b, b'<' | b'=' | b'>')) {
                    return Err(malformed());
                }
                let version = if name.ends_with(".so") {
                    DependencyVersion::SonameAbi(value.to_string())
                } else {
                    DependencyVersion::Package(
                        PacmanVersion::parse(value).map_err(|_| malformed())?,
                    )
                };
                (name, Some((relation, version)))
            }
        };
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'@' | b'.' | b'_' | b'+' | b'-'))
        {
            return Err(malformed());
        }
        Ok(Self {
            name: name.to_string(),
            constraint,
        })
    }
}

/// One binary package from an Arch repository database.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub name: String,
    pub version: PacmanVersion,
    pub arch: String,
    pub filename: String,
    pub sha256: String,
    /// `%CSIZE%`; absent only when the source stanza omitted this optional field.
    pub size: Option<u64>,
    pub depends: Vec<Dependency>,
    pub provides: Vec<Dependency>,
    pub replaces: Vec<Dependency>,
    pub conflicts: Vec<Dependency>,
    pub groups: Vec<String>,
    /// First line of `%DESC%`, or empty when the optional field is absent.
    pub description: String,
    pub repository: String,
}

/// Parse one gzip-tar repository database with the production limits.
pub fn parse(bytes: &[u8], repository: &str) -> Result<Vec<Package>, String> {
    parse_with_limits(bytes, repository, LIMITS)
}

/// The same parser with caller-supplied limits, exposed so each bound has a small deterministic
/// regression rather than constructing a production-sized hostile archive.
pub fn parse_with_limits(
    bytes: &[u8],
    repository: &str,
    limits: Limits,
) -> Result<Vec<Package>, String> {
    let mut decoder = GzDecoder::new(bytes);
    let mut decompressed = Vec::new();
    decoder
        .by_ref()
        .take(limits.max_decompressed_size.saturating_add(1))
        .read_to_end(&mut decompressed)
        .map_err(|e| format!("malformed Arch repository database gzip stream: {e}"))?;
    if decompressed.len() as u64 > limits.max_decompressed_size {
        return Err(format!(
            "Arch repository database exceeds the decompressed size limit of {} bytes",
            limits.max_decompressed_size
        ));
    }

    let mut archive = tar::Archive::new(Cursor::new(decompressed));
    let entries = archive
        .entries()
        .map_err(|e| format!("malformed Arch repository database tar stream: {e}"))?;
    let mut packages = Vec::new();
    let mut count = 0u64;
    for entry in entries {
        let mut entry = entry.map_err(|e| format!("malformed Arch repository member: {e}"))?;
        count += 1;
        if count > limits.max_members {
            return Err(format!(
                "Arch repository database exceeds the member count limit of {}",
                limits.max_members
            ));
        }
        let raw_path = entry.path_bytes().into_owned();
        let shown = String::from_utf8_lossy(&raw_path).into_owned();
        let path = std::str::from_utf8(&raw_path)
            .map_err(|_| format!("malformed Arch repository member path `{shown}`"))?;
        let parts: Vec<&str> = path.split('/').filter(|part| !part.is_empty()).collect();
        if entry.header().entry_type().is_dir() {
            if parts.len() != 1 || matches!(parts[0], "." | "..") {
                return Err(format!("malformed Arch repository member path `{shown}`"));
            }
            continue;
        }
        if !entry.header().entry_type().is_file()
            || parts.len() != 2
            || parts[1] != "desc"
            || matches!(parts[0], "." | "..")
        {
            return Err(format!("malformed Arch repository member path `{shown}`"));
        }
        let size = entry
            .header()
            .size()
            .map_err(|e| format!("malformed Arch repository member `{shown}`: {e}"))?;
        if size > limits.max_member_size {
            return Err(format!(
                "Arch repository member `{shown}` exceeds the member size limit of {} bytes",
                limits.max_member_size
            ));
        }
        let mut body = Vec::new();
        entry
            .read_to_end(&mut body)
            .map_err(|e| format!("could not read Arch repository member `{shown}`: {e}"))?;
        let body = std::str::from_utf8(&body)
            .map_err(|_| format!("Arch repository member `{shown}` is not UTF-8"))?;
        packages.push(parse_desc(body, repository, &shown, parts[0])?);
    }
    packages.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.version.cmp(&b.version)));
    Ok(packages)
}

fn parse_desc(
    text: &str,
    repository: &str,
    member: &str,
    directory: &str,
) -> Result<Package, String> {
    let mut fields: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for block in text.split("\n\n").filter(|block| !block.is_empty()) {
        let mut lines = block.lines();
        let key = lines
            .next()
            .ok_or_else(|| format!("malformed desc stanza in member `{member}`"))?;
        if key.len() < 3 || !key.starts_with('%') || !key.ends_with('%') {
            return Err(format!(
                "malformed desc key `{key}` in Arch repository member `{member}`"
            ));
        }
        if fields.contains_key(key) {
            return Err(format!(
                "duplicate desc key `{key}` in Arch repository member `{member}`"
            ));
        }
        fields.insert(key, lines.collect());
    }
    let one = |key: &'static str| -> Result<&str, String> {
        let values = fields
            .get(key)
            .ok_or_else(|| format!("Arch repository member `{member}` has no `{key}` key"))?;
        match values.as_slice() {
            [value] if !value.is_empty() => Ok(*value),
            _ => Err(format!(
                "Arch repository member `{member}` has malformed `{key}` values"
            )),
        }
    };
    let name = one("%NAME%")?;
    let version_text = one("%VERSION%")?;
    if directory != format!("{name}-{version_text}") {
        return Err(format!("malformed Arch repository member path `{member}`"));
    }
    let version = PacmanVersion::parse(version_text)
        .map_err(|e| format!("Arch repository member `{member}`: {e}"))?;
    let sha256 = one("%SHA256SUM%")?;
    if !is_sha256_hex(sha256) {
        return Err(format!(
            "Arch repository member `{member}` has malformed SHA256SUM `{sha256}`"
        ));
    }
    let size = match fields.get("%CSIZE%") {
        None => None,
        Some(values) => match values.as_slice() {
            [value] => Some(value.parse::<u64>().map_err(|_| {
                format!("Arch repository member `{member}` has malformed CSIZE `{value}`")
            })?),
            _ => {
                return Err(format!(
                    "Arch repository member `{member}` has malformed `%CSIZE%` values"
                ));
            }
        },
    };
    let dependencies = |key: &'static str| -> Result<Vec<Dependency>, String> {
        fields
            .get(key)
            .into_iter()
            .flat_map(|values| values.iter())
            .map(|value| {
                Dependency::parse(value)
                    .map_err(|e| format!("Arch repository member `{member}`: {e}"))
            })
            .collect()
    };
    Ok(Package {
        name: name.to_string(),
        version,
        arch: one("%ARCH%")?.to_string(),
        filename: one("%FILENAME%")?.to_string(),
        sha256: sha256.to_string(),
        size,
        depends: dependencies("%DEPENDS%")?,
        provides: dependencies("%PROVIDES%")?,
        replaces: dependencies("%REPLACES%")?,
        conflicts: dependencies("%CONFLICTS%")?,
        groups: fields
            .get("%GROUPS%")
            .into_iter()
            .flat_map(|values| values.iter())
            .map(|value| (*value).to_string())
            .collect(),
        description: fields
            .get("%DESC%")
            .and_then(|values| values.first())
            .copied()
            .unwrap_or("")
            .to_string(),
        repository: repository.to_string(),
    })
}
