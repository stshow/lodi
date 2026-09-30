//! Debian package versions and their ordering: `dpkg --compare-versions` semantics (Debian
//! Policy §5.6.12): `[epoch:]upstream[-revision]`, where letters sort before non-letters and `~`
//! sorts before everything, even the end of the string.

use std::cmp::Ordering;
use std::fmt;

/// A parsed Debian version. Equality and ordering follow dpkg, so `1.0` equals `0:1.0`.
#[derive(Debug, Clone)]
pub struct DebVersion {
    pub epoch: u64,
    pub upstream: String,
    pub revision: String,
    text: String,
}

impl DebVersion {
    pub fn parse(text: &str) -> Result<DebVersion, String> {
        let bad = |why: &str| format!("`{text}` is not a Debian version: {why}");
        if text.is_empty() || text.chars().any(char::is_whitespace) {
            return Err(bad("empty or contains whitespace"));
        }
        let (epoch, rest) = match text.split_once(':') {
            Some((epoch, rest)) => {
                if epoch.is_empty() || !epoch.bytes().all(|b| b.is_ascii_digit()) {
                    return Err(bad("the epoch is not a number"));
                }
                let epoch = epoch.parse::<u64>().map_err(|_| bad("epoch too large"))?;
                (epoch, rest)
            }
            None => (0, text),
        };
        let (upstream, revision) = match rest.rsplit_once('-') {
            Some((upstream, revision)) => (upstream, revision),
            None => (rest, ""),
        };
        if !upstream.starts_with(|c: char| c.is_ascii_digit()) {
            return Err(bad("the upstream version must start with a digit"));
        }
        let upstream_ok = upstream
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+~-:".contains(c));
        let revision_ok = revision
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".+~".contains(c));
        if !upstream_ok || !revision_ok || (rest.contains('-') && revision.is_empty()) {
            return Err(bad("contains a character dpkg does not allow"));
        }
        Ok(DebVersion {
            epoch,
            upstream: upstream.to_string(),
            revision: revision.to_string(),
            text: text.to_string(),
        })
    }

    /// The version exactly as written.
    pub fn as_str(&self) -> &str {
        &self.text
    }
}

impl fmt::Display for DebVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.text)
    }
}

impl PartialEq for DebVersion {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for DebVersion {}

impl PartialOrd for DebVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DebVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| compare_part(&self.upstream, &other.upstream))
            .then_with(|| compare_part(&self.revision, &other.revision))
    }
}

/// The weight of one non-digit character in dpkg's ordering.
fn order(c: Option<u8>) -> i32 {
    match c {
        None => 0,
        Some(b'~') => -1,
        Some(c) if c.is_ascii_digit() => 0,
        Some(c) if c.is_ascii_alphabetic() => c as i32,
        Some(c) => c as i32 + 256,
    }
}

/// dpkg's `verrevcmp`: alternate non-digit and digit runs.
fn compare_part(a: &str, b: &str) -> Ordering {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        while (i < a.len() && !a[i].is_ascii_digit()) || (j < b.len() && !b[j].is_ascii_digit()) {
            let ac = order(a.get(i).copied().filter(|c| !c.is_ascii_digit()));
            let bc = order(b.get(j).copied().filter(|c| !c.is_ascii_digit()));
            if ac != bc {
                return ac.cmp(&bc);
            }
            i += 1;
            j += 1;
        }
        while i < a.len() && a[i] == b'0' {
            i += 1;
        }
        while j < b.len() && b[j] == b'0' {
            j += 1;
        }
        let mut first = Ordering::Equal;
        while i < a.len() && a[i].is_ascii_digit() && j < b.len() && b[j].is_ascii_digit() {
            if first == Ordering::Equal {
                first = a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
        if i < a.len() && a[i].is_ascii_digit() {
            return Ordering::Greater;
        }
        if j < b.len() && b[j].is_ascii_digit() {
            return Ordering::Less;
        }
        if first != Ordering::Equal {
            return first;
        }
    }
    Ordering::Equal
}

/// A version relation in a dependency field (`Depends: foo (>= 1.0)`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    Earlier,
    EarlierEqual,
    Equal,
    LaterEqual,
    Later,
}

impl Relation {
    pub fn parse(text: &str) -> Option<Relation> {
        Some(match text {
            "<<" => Relation::Earlier,
            "<=" | "<" => Relation::EarlierEqual,
            "=" => Relation::Equal,
            ">=" | ">" => Relation::LaterEqual,
            ">>" => Relation::Later,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Relation::Earlier => "<<",
            Relation::EarlierEqual => "<=",
            Relation::Equal => "=",
            Relation::LaterEqual => ">=",
            Relation::Later => ">>",
        }
    }

    /// Whether `version` stands in this relation to `bound`.
    pub fn holds(self, version: &DebVersion, bound: &DebVersion) -> bool {
        let o = version.cmp(bound);
        match self {
            Relation::Earlier => o == Ordering::Less,
            Relation::EarlierEqual => o != Ordering::Greater,
            Relation::Equal => o == Ordering::Equal,
            Relation::LaterEqual => o != Ordering::Less,
            Relation::Later => o == Ordering::Greater,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(text: &str) -> DebVersion {
        DebVersion::parse(text).unwrap()
    }

    #[test]
    fn dpkg_ordering() {
        // Pairs from dpkg's own test suite and Debian Policy §5.6.12.
        let less = [
            ("1.0~rc1", "1.0"),
            ("1.0", "1.0-1"),
            ("1.0-1", "1.0-2"),
            ("1.0", "1.0a"),
            ("1.0a", "1.0.1"),
            ("1.0~~", "1.0~"),
            ("1.0~~a", "1.0~"),
            ("1.2.3", "1.2.10"),
            ("2.36-9+deb12u9", "2.36-9+deb12u10"),
            ("3.0.17-1~deb12u1", "3.0.17-1~deb12u2"),
            ("3.0.17-1~deb12u2", "3.0.17-1"),
            ("9:1.0", "10:0.1"),
            ("1:0.1", "2:0"),
            ("1.0+b1", "1.0.1"),
            ("0.9", "1:0.1"),
        ];
        for (a, b) in less {
            assert!(v(a) < v(b), "{a} < {b}");
            assert!(v(b) > v(a), "{b} > {a}");
        }
        assert_eq!(v("1.0"), v("0:1.0"));
        assert_eq!(v("1.00"), v("1.0"));
        assert_eq!(v("1.0-0"), v("1.0-0"));
    }

    #[test]
    fn parses_components_and_refuses_garbage() {
        let x = v("1:2.3-4-5");
        assert_eq!(
            (x.epoch, x.upstream.as_str(), x.revision.as_str()),
            (1, "2.3-4", "5")
        );
        for bad in ["", "a1.0", "1.0 1", "x:1.0", "1.0-", "1.0_1"] {
            assert!(DebVersion::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn relations() {
        let (a, b) = (v("1.0"), v("1.1"));
        assert!(Relation::Earlier.holds(&a, &b));
        assert!(!Relation::Earlier.holds(&b, &b));
        assert!(Relation::EarlierEqual.holds(&b, &b));
        assert!(Relation::Equal.holds(&a, &v("0:1.0")));
        assert!(Relation::LaterEqual.holds(&b, &a));
        assert!(Relation::Later.holds(&b, &a));
        assert_eq!(Relation::parse(">>"), Some(Relation::Later));
        assert_eq!(Relation::parse("!="), None);
    }
}
