//! Debian control data: RFC 822-style stanzas (`Packages`, `Release`) and relationship fields
//! (`Depends`, `Pre-Depends`, `Provides`, `Conflicts`, `Breaks`).

use super::version::{DebVersion, Relation};

/// One stanza: fields in file order. Continuation lines are joined to their field with `\n`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stanza {
    pub fields: Vec<(String, String)>,
}

impl Stanza {
    /// A field's value; field names are case-insensitive.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Parse control data into stanzas separated by blank lines.
///
/// Refuses (with the 1-based line number) a line that is neither `Field: value`, a continuation
/// line nor blank, so a truncated or corrupted index is an error rather than a shorter index.
pub fn parse_stanzas(text: &str) -> Result<Vec<Stanza>, String> {
    let mut stanzas = Vec::new();
    let mut current = Stanza::default();
    for (index, line) in text.lines().enumerate() {
        let line_no = index + 1;
        if line.trim().is_empty() {
            if !current.fields.is_empty() {
                stanzas.push(std::mem::take(&mut current));
            }
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let Some((_, value)) = current.fields.last_mut() else {
                return Err(format!("line {line_no}: continuation line without a field"));
            };
            value.push('\n');
            value.push_str(line.trim());
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(format!("line {line_no}: expected `Field: value`"));
        };
        let valid_name = !name.is_empty()
            && name
                .bytes()
                .all(|b| b.is_ascii_graphic() && b != b':' && b != b'#');
        if !valid_name {
            return Err(format!("line {line_no}: invalid field name `{name}`"));
        }
        current
            .fields
            .push((name.to_string(), value.trim().to_string()));
    }
    if !current.fields.is_empty() {
        stanzas.push(current);
    }
    Ok(stanzas)
}

/// `name [:archqual] [(op version)]` inside a relationship field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    /// `any` or `native`; other qualifiers are refused when the closure is computed.
    pub arch_qualifier: Option<String>,
    pub version: Option<(Relation, DebVersion)>,
}

impl std::fmt::Display for Dependency {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.name)?;
        if let Some(q) = &self.arch_qualifier {
            write!(f, ":{q}")?;
        }
        if let Some((rel, v)) = &self.version {
            write!(f, " ({} {v})", rel.as_str())?;
        }
        Ok(())
    }
}

/// A relationship field: a conjunction of alternatives (`a | b, c`).
pub type Relationships = Vec<Vec<Dependency>>;

/// Parse a binary-package relationship field. Build profiles (`<...>`) and architecture
/// restrictions (`[...]`) belong to source packages and are refused here.
pub fn parse_relationships(text: &str) -> Result<Relationships, String> {
    let mut all = Vec::new();
    for group in text.split(',') {
        let group = group.trim();
        if group.is_empty() {
            continue;
        }
        let mut alternatives = Vec::new();
        for alt in group.split('|') {
            alternatives.push(parse_dependency(alt.trim())?);
        }
        all.push(alternatives);
    }
    Ok(all)
}

fn parse_dependency(text: &str) -> Result<Dependency, String> {
    let outside = text.split_once('(').map_or(text, |(head, _)| head);
    if outside.contains(['[', '<']) || text.ends_with([']', '>']) {
        return Err(format!(
            "`{text}`: architecture restrictions and build profiles are not valid here"
        ));
    }
    let (head, version) = match text.split_once('(') {
        Some((head, rest)) => {
            let inner = rest
                .strip_suffix(')')
                .ok_or_else(|| format!("`{text}`: unterminated version relation"))?
                .trim();
            let split = inner
                .find(|c: char| !"<>=".contains(c))
                .ok_or_else(|| format!("`{text}`: missing version"))?;
            let (op, version) = inner.split_at(split);
            let relation = Relation::parse(op.trim())
                .ok_or_else(|| format!("`{text}`: unknown relation `{op}`"))?;
            let version = DebVersion::parse(version.trim())?;
            (head.trim(), Some((relation, version)))
        }
        None => (text, None),
    };
    let (name, arch_qualifier) = match head.split_once(':') {
        Some((name, qualifier)) => (name, Some(qualifier.to_string())),
        None => (head, None),
    };
    let valid = name.len() >= 2
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || "+-.".contains(c));
    if !valid {
        return Err(format!("`{text}`: invalid package name `{name}`"));
    }
    Ok(Dependency {
        name: name.to_string(),
        arch_qualifier,
        version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stanzas_and_continuations() {
        let text = "Package: a\nDescription: short\n long line\n .\n\n\nPackage: b\nVersion: 1\n";
        let s = parse_stanzas(text).unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].get("package"), Some("a"));
        assert_eq!(s[0].get("Description"), Some("short\nlong line\n."));
        assert_eq!(s[1].get("Version"), Some("1"));
        assert!(parse_stanzas(" orphan\n").is_err());
        assert!(parse_stanzas("Package: a\ngarbage\n").is_err());
    }

    #[test]
    fn relationships() {
        let r = parse_relationships(
            "libc6 (>= 2.34), libssl3 (= 3.0.17-1~deb12u2), gcc | c-compiler, python3:any (>= 3.11~)",
        )
        .unwrap();
        assert_eq!(r.len(), 4);
        assert_eq!(r[0][0].name, "libc6");
        assert_eq!(r[0][0].version.as_ref().unwrap().0, Relation::LaterEqual);
        assert_eq!(
            r[1][0].version.as_ref().unwrap().1.as_str(),
            "3.0.17-1~deb12u2"
        );
        assert_eq!(r[2].len(), 2);
        assert_eq!(r[2][1].name, "c-compiler");
        assert_eq!(r[3][0].arch_qualifier.as_deref(), Some("any"));
        assert_eq!(r[3][0].to_string(), "python3:any (>= 3.11~)");
        assert!(parse_relationships("").unwrap().is_empty());
        for bad in ["foo [amd64]", "foo (>= 1.0", "foo (!! 1)", "Foo", "a"] {
            assert!(parse_relationships(bad).is_err(), "{bad}");
        }
    }
}
