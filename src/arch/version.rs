//! Pacman package versions and their ordering.
//!
//! Versions have the form `[epoch:]pkgver[-pkgrel]`. Comparison follows the contract fixed by
//! M-Arch-Base design call D8: numeric and alphabetic runs are compared separately, numeric runs
//! ignore leading zeroes and outrank alphabetic runs, and a trailing alphabetic run denotes a
//! pre-release while a trailing numeric run denotes a later release.

use std::cmp::Ordering;
use std::fmt;

/// A parsed pacman package version.
#[derive(Debug, Clone)]
pub struct PacmanVersion {
    pub epoch: u64,
    pub pkgver: String,
    pub pkgrel: Option<String>,
    text: String,
}

impl PacmanVersion {
    /// Parse one complete `epoch:pkgver-pkgrel` package version.
    pub fn parse(text: &str) -> Result<Self, String> {
        let bad = |why: &str| format!("`{text}` is not a pacman version: {why}");
        if text.is_empty() || !text.is_ascii() || text.chars().any(char::is_whitespace) {
            return Err(bad("empty, non-ASCII or contains whitespace"));
        }
        let (epoch, rest) = match text.split_once(':') {
            Some((epoch, rest)) => {
                if rest.contains(':')
                    || epoch.is_empty()
                    || !epoch.bytes().all(|b| b.is_ascii_digit())
                {
                    return Err(bad("the epoch is not one decimal number"));
                }
                let epoch = epoch.parse::<u64>().map_err(|_| bad("epoch too large"))?;
                (epoch, rest)
            }
            None => (0, text),
        };
        let (pkgver, pkgrel) = match rest.rsplit_once('-') {
            Some((version, release)) => {
                if version.contains('-') || release.is_empty() {
                    return Err(bad("the package release is empty or ambiguous"));
                }
                (version, Some(release))
            }
            None => (rest, None),
        };
        if pkgver.is_empty() {
            return Err(bad("the package version is empty"));
        }
        let allowed = |part: &str| {
            part.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+'))
        };
        if !allowed(pkgver) || pkgrel.is_some_and(|release| !allowed(release)) {
            return Err(bad(
                "contains a character pacman package versions do not allow",
            ));
        }
        Ok(Self {
            epoch,
            pkgver: pkgver.to_string(),
            pkgrel: pkgrel.map(str::to_string),
            text: text.to_string(),
        })
    }

    /// The version exactly as it appeared in the repository database.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for PacmanVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq for PacmanVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for PacmanVersion {}

impl PartialOrd for PacmanVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PacmanVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| compare_part(&self.pkgver, &other.pkgver))
            .then_with(|| match (&self.pkgrel, &other.pkgrel) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Less,
                (Some(_), None) => Ordering::Greater,
                (Some(a), Some(b)) => compare_part(a, b),
            })
    }
}

#[derive(Clone, Copy)]
struct Segment<'a> {
    numeric: bool,
    bytes: &'a [u8],
}

fn segments(text: &str) -> Vec<Segment<'_>> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        while at < bytes.len() && !bytes[at].is_ascii_alphanumeric() {
            at += 1;
        }
        if at == bytes.len() {
            break;
        }
        let start = at;
        let numeric = bytes[at].is_ascii_digit();
        while at < bytes.len()
            && bytes[at].is_ascii_alphanumeric()
            && bytes[at].is_ascii_digit() == numeric
        {
            at += 1;
        }
        out.push(Segment {
            numeric,
            bytes: &bytes[start..at],
        });
    }
    out
}

fn compare_part(a: &str, b: &str) -> Ordering {
    let a = segments(a);
    let b = segments(b);
    for (left, right) in a.iter().zip(&b) {
        let ordering = match (left.numeric, right.numeric) {
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => left.bytes.cmp(right.bytes),
            (true, true) => compare_numeric(left.bytes, right.bytes),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    match (a.get(b.len()), b.get(a.len())) {
        (None, None) => Ordering::Equal,
        (Some(extra), None) => {
            if extra.numeric {
                Ordering::Greater
            } else {
                Ordering::Less
            }
        }
        (None, Some(extra)) => {
            if extra.numeric {
                Ordering::Less
            } else {
                Ordering::Greater
            }
        }
        (Some(_), Some(_)) => unreachable!("zipped prefixes cannot both have a remainder"),
    }
}

fn compare_numeric(a: &[u8], b: &[u8]) -> Ordering {
    let a = trim_zeroes(a);
    let b = trim_zeroes(b);
    a.len().cmp(&b.len()).then_with(|| a.cmp(b))
}

fn trim_zeroes(mut value: &[u8]) -> &[u8] {
    while value.first() == Some(&b'0') {
        value = &value[1..];
    }
    value
}
