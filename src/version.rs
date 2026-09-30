//! Version constraints for upstream (binary-provider) tools, design `spec/04` §4 and §4.1.
//!
//! ```text
//! constraint := "latest" | range ( "," range )* | prefix
//! range      := op version
//! op         := "=" | ">=" | ">" | "<=" | "<" | "~" | "^"
//! prefix     := version
//! version    := part ( "." part )* [ "-" pre ] [ "+" build ]
//! ```

use std::cmp::Ordering;
use std::fmt;

/// A binary-provider version: numeric parts, optional prerelease and build metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    pub parts: Vec<u64>,
    pub pre: Option<String>,
    pub build: Option<String>,
}

impl Version {
    pub fn parse(text: &str) -> Result<Version, String> {
        let (rest, build) = match text.split_once('+') {
            Some((rest, build)) => (rest, Some(identifier(build, "build metadata")?)),
            None => (text, None),
        };
        let (numbers, pre) = match rest.split_once('-') {
            Some((numbers, pre)) => (numbers, Some(identifier(pre, "prerelease")?)),
            None => (rest, None),
        };
        let mut parts = Vec::new();
        for part in numbers.split('.') {
            if part.is_empty() || !part.bytes().all(|b| b.is_ascii_digit()) {
                return Err(format!(
                    "`{text}` is not a version (numeric parts separated by `.`)"
                ));
            }
            let value = part
                .parse::<u64>()
                .map_err(|_| format!("version part `{part}` is too large"))?;
            parts.push(value);
        }
        Ok(Version { parts, pre, build })
    }

    fn part(&self, index: usize) -> u64 {
        self.parts.get(index).copied().unwrap_or(0)
    }
}

fn identifier(text: &str, what: &str) -> Result<String, String> {
    let valid = !text.is_empty()
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-');
    if valid {
        Ok(text.to_string())
    } else {
        Err(format!("{what} `{text}` may only contain [0-9A-Za-z.-]"))
    }
}

/// SemVer §11 precedence of two prerelease strings.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut left = a.split('.');
    let mut right = b.split('.');
    loop {
        match (left.next(), right.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let numeric = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
                let order = match (numeric(x), numeric(y)) {
                    (true, true) => (x.len(), x).cmp(&(y.len(), y)),
                    (true, false) => Ordering::Less,
                    (false, true) => Ordering::Greater,
                    (false, false) => x.cmp(y),
                };
                if order != Ordering::Equal {
                    return order;
                }
            }
        }
    }
}

impl Version {
    /// Precedence (`spec/04` §4.1): numeric parts compared component-wise with missing parts as
    /// 0, then a prerelease before the same release; build metadata is ignored. `3.12` and
    /// `3.12.0` have equal precedence.
    pub fn precedence(&self, other: &Version) -> Ordering {
        let len = self.parts.len().max(other.parts.len());
        for i in 0..len {
            let order = self.part(i).cmp(&other.part(i));
            if order != Ordering::Equal {
                return order;
            }
        }
        match (&self.pre, &other.pre) {
            (None, None) => Ordering::Equal,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(a), Some(b)) => compare_pre(a, b),
        }
    }
}

impl Ord for Version {
    /// Precedence, with the written part count and build metadata breaking ties only so that
    /// `Ord` agrees with `Eq`.
    fn cmp(&self, other: &Version) -> Ordering {
        self.precedence(other)
            .then_with(|| self.parts.len().cmp(&other.parts.len()))
            .then_with(|| self.build.cmp(&other.build))
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Version) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let parts: Vec<String> = self.parts.iter().map(u64::to_string).collect();
        write!(f, "{}", parts.join("."))?;
        if let Some(pre) = &self.pre {
            write!(f, "-{pre}")?;
        }
        if let Some(build) = &self.build {
            write!(f, "+{build}")?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ge,
    Gt,
    Le,
    Lt,
    Tilde,
    Caret,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Constraint {
    /// The highest non-prerelease version.
    Latest,
    /// The first *n* numeric components equal the given ones (`"3.12"`, `"22"`).
    Prefix(Version),
    /// Every range must hold.
    Ranges(Vec<(Op, Version)>),
}

impl Constraint {
    /// Parse a constraint. Whitespace around operators and commas is insignificant.
    pub fn parse(text: &str) -> Result<Constraint, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("an empty version constraint".to_string());
        }
        if trimmed == "latest" {
            return Ok(Constraint::Latest);
        }
        let starts_with_op = |s: &str| s.starts_with(['=', '>', '<', '~', '^']);
        if !trimmed.contains(',') && !starts_with_op(trimmed) {
            return Ok(Constraint::Prefix(Version::parse(trimmed)?));
        }
        let mut ranges = Vec::new();
        for range in trimmed.split(',') {
            let range = range.trim();
            let (op, rest) = if let Some(rest) = range.strip_prefix(">=") {
                (Op::Ge, rest)
            } else if let Some(rest) = range.strip_prefix("<=") {
                (Op::Le, rest)
            } else if let Some(rest) = range.strip_prefix('>') {
                (Op::Gt, rest)
            } else if let Some(rest) = range.strip_prefix('<') {
                (Op::Lt, rest)
            } else if let Some(rest) = range.strip_prefix('=') {
                (Op::Eq, rest)
            } else if let Some(rest) = range.strip_prefix('~') {
                (Op::Tilde, rest)
            } else if let Some(rest) = range.strip_prefix('^') {
                (Op::Caret, rest)
            } else {
                return Err(format!(
                    "`{range}` in a list of ranges needs an operator (=, >=, >, <=, <, ~, ^)"
                ));
            };
            ranges.push((op, Version::parse(rest.trim())?));
        }
        Ok(Constraint::Ranges(ranges))
    }

    fn mentions_prerelease(&self) -> bool {
        match self {
            Constraint::Latest => false,
            Constraint::Prefix(v) => v.pre.is_some(),
            Constraint::Ranges(ranges) => ranges.iter().any(|(_, v)| v.pre.is_some()),
        }
    }

    /// Whether `version` satisfies this constraint (`spec/04` §4.1). Prereleases match only when
    /// the constraint itself names a prerelease.
    pub fn matches(&self, version: &Version) -> bool {
        if version.pre.is_some() && !self.mentions_prerelease() {
            return false;
        }
        match self {
            Constraint::Latest => true,
            Constraint::Prefix(prefix) => {
                prefix
                    .parts
                    .iter()
                    .enumerate()
                    .all(|(i, part)| version.part(i) == *part)
                    && (prefix.pre.is_none() || prefix.pre == version.pre)
            }
            Constraint::Ranges(ranges) => ranges.iter().all(|(op, bound)| {
                let order = version.precedence(bound);
                match op {
                    Op::Eq => order == Ordering::Equal,
                    Op::Ge => order != Ordering::Less,
                    Op::Gt => order == Ordering::Greater,
                    Op::Le => order != Ordering::Greater,
                    Op::Lt => order == Ordering::Less,
                    Op::Tilde | Op::Caret => {
                        order != Ordering::Less
                            && version.precedence(&upper_bound(*op, bound)) == Ordering::Less
                    }
                }
            }),
        }
    }
}

/// The exclusive upper bound of `~X.Y[.Z]` (next minor; `~X`: next major) and `^X.Y.Z`
/// (next major; `^0.Y.Z`: next minor).
fn upper_bound(op: Op, bound: &Version) -> Version {
    let next = |index: usize| {
        let mut parts: Vec<u64> = bound.parts.iter().take(index + 1).copied().collect();
        parts.resize(index + 1, 0);
        parts[index] = parts[index].saturating_add(1);
        Version {
            parts,
            pre: None,
            build: None,
        }
    };
    match op {
        Op::Tilde if bound.parts.len() >= 2 => next(1),
        Op::Caret if bound.part(0) == 0 && bound.parts.len() >= 2 => next(1),
        _ => next(0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> Version {
        Version::parse(text).unwrap()
    }

    fn c(text: &str) -> Constraint {
        Constraint::parse(text).unwrap()
    }

    #[test]
    fn prefix_matches_leading_components_only() {
        assert!(c("3.12").matches(&v("3.12.0")));
        assert!(c("3.12").matches(&v("3.12.99")));
        assert!(!c("3.12").matches(&v("3.120.0")));
        assert!(!c("3.12").matches(&v("3.13.0")));
        assert!(c("22").matches(&v("22.9.1")));
        assert!(!c("22").matches(&v("23.0.0")));
        assert!(!c("3.12").matches(&v("3.12.1-rc1")));
    }

    #[test]
    fn latest_excludes_prereleases() {
        assert!(c("latest").matches(&v("1.0.0")));
        assert!(!c("latest").matches(&v("1.0.0-rc1")));
    }

    #[test]
    fn ranges_combine_and_tolerate_whitespace() {
        let range = c(">= 3.11 , < 3.13");
        assert!(range.matches(&v("3.11.0")) && range.matches(&v("3.12.9")));
        assert!(!range.matches(&v("3.13.0")) && !range.matches(&v("3.10.9")));
        assert!(c("=1.2.3").matches(&v("1.2.3")) && !c("=1.2.3").matches(&v("1.2.4")));
        assert!(c("=3.12").matches(&v("3.12.0")));
    }

    #[test]
    fn tilde_and_caret_bounds() {
        assert!(c("~1.2.3").matches(&v("1.2.9")) && !c("~1.2.3").matches(&v("1.3.0")));
        assert!(c("^1.2.3").matches(&v("1.9.0")) && !c("^1.2.3").matches(&v("2.0.0")));
        assert!(c("^0.2.3").matches(&v("0.2.9")) && !c("^0.2.3").matches(&v("0.3.0")));
        assert!(!c("^1.2.3").matches(&v("1.2.2")));
    }

    #[test]
    fn prereleases_follow_semver_precedence() {
        assert!(v("1.0.0-alpha") < v("1.0.0-alpha.1"));
        assert!(v("1.0.0-alpha.1") < v("1.0.0-alpha.beta"));
        assert!(v("1.0.0-beta.2") < v("1.0.0-beta.11"));
        assert!(v("1.0.0-rc.1") < v("1.0.0"));
        assert!(c(">=1.0.0-rc1").matches(&v("1.0.0-rc2")));
    }

    #[test]
    fn invalid_constraints_are_refused() {
        for bad in [
            "",
            "  ",
            "3.x",
            "v3.12",
            "3..12",
            "3.12, 3.13",
            ">=",
            "3.12-",
            "1+",
            "!3",
        ] {
            assert!(Constraint::parse(bad).is_err(), "{bad:?}");
        }
    }
}
