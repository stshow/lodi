//! The one reader and writer of dnf repository files (LD-433).
//!
//! [`super::sources`] arms a declared `[sources.NAME]` on Fedora as
//! `/etc/yum.repos.d/lodi-NAME.repo`, rendered here, and reads the machine's own `.repo` files
//! with [`read_machine`] to find a repository another file already lists; the import reads them
//! for its commented blocks. Nothing else in the build parses or renders a `.repo` file.
//!
//! The writer is a pure function of its input: the fixed comment line [`super::sourceset`]'s
//! stanzas start with, one section `[lodi-NAME]`, then `name`, `baseurl` (the declared URIs in
//! order, one space between), `enabled=1`, `gpgcheck=1`, `repo_gpgcheck=1` only when declared,
//! `gpgkey` naming the key file, and `skip_if_unavailable=0` so a repository that cannot be
//! fetched fails the refresh instead of being skipped. The values reaching it have passed the
//! manifest's checks, so none of them can break a line.
//!
//! The reader reads below a root, never follows a link, and names a file it cannot parse rather
//! than guessing at it: dnf's INI dialect, `#` and `;` comments, a line that starts with
//! whitespace continuing the value above it.

use std::path::Path;

use crate::diag::Diagnostic;

use super::manifest::SourceEntry;

/// Where dnf reads repository files from.
pub const REPOS_DIR: &str = "/etc/yum.repos.d";

/// The repository id and section a declared source is written under: `lodi-NAME`.
pub fn repo_id(name: &str) -> String {
    format!("lodi-{name}")
}

/// The `.repo` file of one declared source, whose `gpgkey` names `key_path`.
pub fn render(source: &SourceEntry, key_path: &str) -> String {
    let mut out = format!(
        "{}\n[{}]\nname={} (lodi)\nbaseurl={}\nenabled=1\ngpgcheck=1\n",
        super::sourceset::COMMENT,
        repo_id(&source.name),
        source.name,
        source.uris.join(" ")
    );
    if source.repo_gpgcheck {
        out.push_str("repo_gpgcheck=1\n");
    }
    out.push_str(&format!(
        "gpgkey=file://{key_path}\nskip_if_unavailable=0\n"
    ));
    out
}

/// One section of a `.repo` file, as dnf reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repo {
    /// The in-root path of the file, as the operator knows it.
    pub file: String,
    pub id: String,
    pub baseurls: Vec<String>,
    /// A `metalink` or `mirrorlist` is set.
    pub mirrored: bool,
    /// dnf's default is enabled.
    pub enabled: bool,
    /// `gpgcheck` (or dnf5's `pkg_gpgcheck`) as written; `None` when not written.
    pub gpgcheck: Option<bool>,
    /// The `gpgkey` values, split on whitespace and commas.
    pub gpgkeys: Vec<String>,
}

fn unreadable(path: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_SYNTAX",
        format!("{path}: lodi cannot read this dnf repository file: {why}"),
    )
    .hint(
        "lodi reads every dnf repository file of the machine before it adds a declared \
         repository, so that no repository is listed twice; repair or remove this file by hand",
    )
}

fn boolean(value: &str) -> Option<bool> {
    match value.to_ascii_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => Some(true),
        "0" | "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

fn list(value: &str) -> Vec<String> {
    value
        .split(|c: char| c.is_whitespace() || c == ',')
        .filter(|item| !item.is_empty())
        .map(str::to_string)
        .collect()
}

/// Every section of one `.repo` file's text.
pub fn parse(path: &str, text: &str) -> Result<Vec<Repo>, Diagnostic> {
    let mut repos: Vec<Repo> = Vec::new();
    let mut last: Option<String> = None;
    for (number, line) in text.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with(';') {
            continue;
        }
        let at = |why: &str| unreadable(path, &format!("line {}: {why}", number + 1));
        if line.starts_with([' ', '\t']) {
            // A continuation of the value above; only list values continue.
            let repo = repos
                .last_mut()
                .ok_or_else(|| at("a continuation outside a section"))?;
            match last.as_deref() {
                Some("baseurl") => repo.baseurls.extend(list(trimmed)),
                Some("gpgkey") => repo.gpgkeys.extend(list(trimmed)),
                _ => {}
            }
            continue;
        }
        if let Some(id) = trimmed.strip_prefix('[') {
            let id = id
                .strip_suffix(']')
                .ok_or_else(|| at("a section with no `]`"))?;
            repos.push(Repo {
                file: path.to_string(),
                id: id.trim().to_string(),
                enabled: true,
                ..Repo::default()
            });
            last = None;
            continue;
        }
        let (key, value) = trimmed
            .split_once('=')
            .ok_or_else(|| at("neither a section, a key = value nor a comment"))?;
        let repo = repos
            .last_mut()
            .ok_or_else(|| at("a key outside a section"))?;
        let (key, value) = (key.trim().to_ascii_lowercase(), value.trim());
        match key.as_str() {
            "baseurl" => repo.baseurls.extend(list(value)),
            "metalink" | "mirrorlist" => repo.mirrored |= !value.is_empty(),
            "enabled" => {
                repo.enabled = boolean(value).ok_or_else(|| at("`enabled` is not 0 or 1"))?
            }
            "gpgcheck" | "pkg_gpgcheck" => repo.gpgcheck = boolean(value),
            "gpgkey" => repo.gpgkeys.extend(list(value)),
            _ => {}
        }
        last = Some(key);
    }
    Ok(repos)
}

/// One `.repo` file by its in-root path, with its sections or why it cannot be read.
pub type File = (String, Result<Vec<Repo>, Diagnostic>);

/// The `.repo` files under `/etc/yum.repos.d` below `root`, read only, each through a descriptor
/// that follows no link, in name order, with the sections each holds or why it cannot be read.
/// `skip` names the in-root paths the caller owns itself and does not want read.
pub fn read_files(root: &Path, skip: &dyn Fn(&str) -> bool) -> Result<Vec<File>, Diagnostic> {
    let mut out = Vec::new();
    if !super::sourceset::parents_unlinked_or(root, &format!("{REPOS_DIR}/x"), unreadable)? {
        return Ok(out);
    }
    let dir = super::plan::join(root, REPOS_DIR);
    match std::fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unreadable(
                REPOS_DIR,
                "is not a directory lodi reads without following a link",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(unreadable(REPOS_DIR, &error.to_string())),
    }
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .map_err(|error| unreadable(REPOS_DIR, &error.to_string()))?
        .map(|entry| {
            entry
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .map_err(|error| unreadable(REPOS_DIR, &error.to_string()))
        })
        .collect::<Result<_, _>>()?;
    names.sort();
    for name in names.into_iter().filter(|name| name.ends_with(".repo")) {
        let path = format!("{REPOS_DIR}/{name}");
        if skip(&path) {
            continue;
        }
        let read = match super::sourceset::read_file_or(root, &path, unreadable) {
            Ok(None) => continue,
            Ok(Some(text)) => parse(&path, &text),
            Err(error) => Err(error),
        };
        out.push((path, read));
    }
    Ok(out)
}

/// Every section of the machine's `.repo` files, the first file dnf cannot read being an error.
pub fn read_machine(root: &Path, skip: &dyn Fn(&str) -> bool) -> Result<Vec<Repo>, Diagnostic> {
    let mut out = Vec::new();
    for (_, repos) in read_files(root, skip)? {
        out.extend(repos?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_repo_file_parses_as_dnf_reads_it() {
        let text = "# a comment\n[vendor]\nname=Vendor\nbaseurl=https://a.example/x/\n  \
                    https://b.example/x/\nenabled=0\nGPGCHECK=1\ngpgkey=file:///k1,file:///k2\n\
                    \n[other]\nmetalink=https://m.example/\n";
        let repos = parse("/etc/yum.repos.d/v.repo", text).unwrap();
        assert_eq!(repos.len(), 2);
        assert_eq!(
            repos[0].baseurls,
            ["https://a.example/x/", "https://b.example/x/"]
        );
        assert!(!repos[0].enabled && repos[0].gpgcheck == Some(true));
        assert_eq!(repos[0].gpgkeys, ["file:///k1", "file:///k2"]);
        assert!(repos[1].enabled && repos[1].mirrored && repos[1].gpgcheck.is_none());
        assert!(parse("/f.repo", "baseurl=x\n").is_err());
    }
}
