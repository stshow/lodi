//! The one reader and writer of apt source stanzas (LD-365).
//!
//! Every `.sources` file Lodi writes is rendered here, and every apt source file Lodi reads is
//! parsed here: [`super::sources`] arms a declared `[sources.NAME]` under
//! `/etc/apt/sources.list.d/` with [`render`], and reads the machine's own
//! `/etc/apt/sources.list` and `sources.list.d/*.{list,sources}` with [`read_machine`] to find a
//! repository another stanza already lists (S7). The pin work (M-Pin) is the planned second
//! caller of the writer. Callers decide where the bytes go; nothing else in the build parses or
//! renders a stanza.
//!
//! The writer is a pure function of its input: one fixed comment line with no date, version,
//! user, host or absolute path, then `Types`, `URIs`, `Suites`, `Components` (omitted when the
//! repository is flat), `Architectures` (omitted when none is declared) and `Signed-By`, values
//! in declared order joined by one space, `\n` line ends, no trailing blank line (S1). The
//! values reaching it have passed the manifest's S3 check, so none of them can break a line.
//!
//! The reader reads below a root, never follows a link, and names a file it cannot parse rather
//! than guessing at it.

use std::fmt::Write as _;
use std::fs;
use std::io::Read as _;
use std::path::Path;

use crate::diag::Diagnostic;

/// The file name a source set is written under: `lodi-NAME.sources`.
pub fn file_name(name: &str) -> String {
    format!("lodi-{name}.sources")
}

/// The comment line every stanza Lodi writes starts with. It is fixed: nothing in it depends on
/// the machine, the manifest, the date or the build.
pub const COMMENT: &str = "# Written by lodi from a [sources] table of the host manifest; edit that table, not this file.";

/// The comment the private dated source set of a pinned apply starts with (M-Pin, D14). It is
/// fixed, like [`COMMENT`].
pub const PIN_COMMENT: &str =
    "# Written by lodi for one apply of a pinned host; removed when that apply ends.";

/// The most bytes one source file of the machine may have before the reader refuses it.
const MAX_FILE_BYTES: u64 = 1024 * 1024;

/// One deb822 stanza, as apt's `sources(5)` reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Stanza {
    pub types: Vec<String>,
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    /// Empty for a flat repository: no `Components:` line.
    pub components: Vec<String>,
    /// Empty when none is declared: no `Architectures:` line.
    pub architectures: Vec<String>,
    /// The keyring's absolute path on the machine, as apt will read it.
    pub signed_by: String,
}

impl Stanza {
    /// The stanza's own lines, without a comment and without a trailing blank line.
    pub fn deb822(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "Types: {}", self.types.join(" "));
        let _ = writeln!(out, "URIs: {}", self.uris.join(" "));
        let _ = writeln!(out, "Suites: {}", self.suites.join(" "));
        if !self.components.is_empty() {
            let _ = writeln!(out, "Components: {}", self.components.join(" "));
        }
        if !self.architectures.is_empty() {
            let _ = writeln!(out, "Architectures: {}", self.architectures.join(" "));
        }
        // A `[sources]` stanza always names its keyring. A machine stanza carried into the pin
        // set may name none, and then apt's own default keyrings stand, as on the machine.
        if !self.signed_by.is_empty() {
            let _ = writeln!(out, "Signed-By: {}", self.signed_by);
        }
        out
    }
}

/// A whole `.sources` file for one stanza: [`COMMENT`], then the stanza (S1).
pub fn render(stanza: &Stanza) -> String {
    let mut out = String::with_capacity(256);
    out.push_str(COMMENT);
    out.push('\n');
    out.push_str(&stanza.deb822());
    out
}

/// The private source set of one pinned apply (M-Pin, P4): [`PIN_COMMENT`], then each stanza in
/// order with a blank line between two. A stanza marked dated reads a dated archive, whose
/// `Release` file's `Valid-Until` has passed by construction, and is the one kind that carries
/// `Check-Valid-Until: no`; every other stanza is rendered exactly as [`Stanza::deb822`] renders
/// it.
pub fn render_pin_set(stanzas: &[(Stanza, bool)]) -> String {
    let mut out = String::from(PIN_COMMENT);
    out.push('\n');
    for (at, (stanza, dated)) in stanzas.iter().enumerate() {
        if at > 0 {
            out.push('\n');
        }
        out.push_str(&stanza.deb822());
        if *dated {
            out.push_str("Check-Valid-Until: no\n");
        }
    }
    out
}

/// A URI as two stanzas are compared by (S7): the scheme and the host lower-cased, and one
/// trailing `/` removed. Nothing else is rewritten: a path is compared as written.
pub fn normalize_uri(uri: &str) -> String {
    let trimmed = uri.strip_suffix('/').unwrap_or(uri);
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return trimmed.to_string();
    };
    let (authority, path) = match rest.find('/') {
        Some(index) => rest.split_at(index),
        None => (rest, ""),
    };
    format!(
        "{}://{}{path}",
        scheme.to_ascii_lowercase(),
        authority.to_ascii_lowercase()
    )
}

/// Every (URI, suite) pair a stanza lists, URIs normalized, in declared order.
pub fn pairs(uris: &[String], suites: &[String]) -> Vec<(String, String)> {
    uris.iter()
        .flat_map(|uri| {
            let uri = normalize_uri(uri);
            suites.iter().map(move |suite| (uri.clone(), suite.clone()))
        })
        .collect()
}

/// One repository an enabled stanza of the machine lists: where, and which file lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    /// The in-root path of the file, as the operator knows it.
    pub file: String,
    pub uri: String,
    pub suite: String,
}

/// `/etc/apt/sources.list` and `/etc/apt/sources.list.d/*.{list,sources}` below `root`, read
/// only, each file through a descriptor that follows no link, in name order. `skip` names the
/// in-root paths the caller owns itself and does not want read. Files with any other extension
/// are not read, as apt does not read them.
///
/// A file this reader cannot parse, and a link where a file or the directory should be, is
/// `E_SYNTAX` naming it: a repository hidden in a file nobody read is a conflict nobody saw.
///
/// `sources.list` is read, and refused, before anything under `sources.list.d`: a refusal names
/// the first file of the machine that cannot be read, in the order apt reads them.
pub fn read_machine(root: &Path, skip: &dyn Fn(&str) -> bool) -> Result<Vec<Listed>, Diagnostic> {
    Ok(read_machine_entries(root, skip)?
        .iter()
        .flat_map(Entry::listed)
        .collect())
}

/// [`read_machine`], each enabled entry whole: the same files, the same order and the same
/// refusals, with every field an entry carries — the components and the keyring a pinned apply
/// renders the machine's own stanzas into its private source set with (M-Pin, P4).
pub fn read_machine_entries(
    root: &Path,
    skip: &dyn Fn(&str) -> bool,
) -> Result<Vec<Entry>, Diagnostic> {
    let mut out = Vec::new();
    let list = "/etc/apt/sources.list";
    if !skip(list)
        && let Some(text) = read_file(root, list)?
    {
        out.extend(entries_one_line(list, &text)?);
    }
    for (_, entries) in read_entries(root, &|path| path == list || skip(path))? {
        out.extend(entries?);
    }
    Ok(out)
}

/// One file [`read_entries`] read: its in-root path, and its entries or why it cannot be read.
pub type FileEntries = (String, Result<Vec<Entry>, Diagnostic>);

/// The same files as [`read_machine`], each with every enabled entry it holds — or the reason it
/// cannot be read, **per file**, so that a caller that reports rather than refuses (the host
/// import, LD-367) can name that one file and read the rest. A link or anything but a directory
/// at `sources.list.d` is still an error for the whole reading.
pub fn read_entries(
    root: &Path,
    skip: &dyn Fn(&str) -> bool,
) -> Result<Vec<FileEntries>, Diagnostic> {
    let mut out = Vec::new();
    let list = "/etc/apt/sources.list";
    if !skip(list) {
        match read_file(root, list) {
            Ok(Some(text)) => out.push((list.to_string(), entries_one_line(list, &text))),
            Ok(None) => {}
            Err(error) => out.push((list.to_string(), Err(error))),
        }
    }
    let dir_path = "/etc/apt/sources.list.d";
    if !parents_unlinked(root, dir_path)? {
        return Ok(out);
    }
    let dir = super::plan::join(root, dir_path);
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.file_type().is_symlink() || !meta.is_dir() => {
            return Err(unreadable(
                dir_path,
                "is not a directory lodi reads without following a link",
            ));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(out),
        Err(error) => return Err(unreadable(dir_path, &error.to_string())),
    }
    let entries = fs::read_dir(&dir).map_err(|error| unreadable(dir_path, &error.to_string()))?;
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| unreadable(dir_path, &error.to_string()))?;
        names.push(entry.file_name().to_string_lossy().into_owned());
    }
    names.sort();
    for name in names {
        let stanzas = name.ends_with(".sources");
        if !stanzas && !name.ends_with(".list") {
            continue;
        }
        let path = format!("{dir_path}/{name}");
        if skip(&path) {
            continue;
        }
        let read = match read_file(root, &path) {
            Ok(None) => continue,
            Ok(Some(text)) if stanzas => entries_deb822(&path, &text),
            Ok(Some(text)) => entries_one_line(&path, &text),
            Err(error) => Err(error),
        };
        out.push((path, read));
    }
    Ok(out)
}

fn unreadable(path: &str, why: &str) -> Diagnostic {
    Diagnostic::new(
        "E_SYNTAX",
        format!("{path}: lodi cannot read this apt source file: {why}"),
    )
    .hint(
        "lodi reads every apt source file of the machine before it arms a declared source, so \
         that two stanzas never list one repository; repair or remove this file by hand",
    )
}

/// Whether every directory above `path` below the root — `/etc`, `/etc/apt` — is there as a
/// directory, looked at without following anything. A link among them is refused by name before
/// anything under it, or its target, is read; `false` when one is not there, so neither is the
/// path.
fn parents_unlinked(root: &Path, path: &str) -> Result<bool, Diagnostic> {
    parents_unlinked_or(root, path, unreadable)
}

/// [`parents_unlinked`], with the refusal worded by `unreadable` (LD-433: the dnf reader's own).
pub(super) fn parents_unlinked_or(
    root: &Path,
    path: &str,
    unreadable: fn(&str, &str) -> Diagnostic,
) -> Result<bool, Diagnostic> {
    let mut above = String::new();
    let parents: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    for component in &parents[..parents.len() - 1] {
        above.push('/');
        above.push_str(component);
        match fs::symlink_metadata(super::plan::join(root, &above)) {
            Ok(meta) if meta.file_type().is_symlink() => {
                return Err(unreadable(
                    path,
                    &format!(
                        "{above} on the way to it is a symbolic link, which lodi does not follow"
                    ),
                ));
            }
            Ok(meta) if !meta.is_dir() => {
                return Err(unreadable(
                    path,
                    &format!("{above} on the way to it is not a directory"),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(unreadable(path, &error.to_string())),
        }
    }
    Ok(true)
}

/// One file's text, or `None` when it is not there. A link, a directory or a device where the
/// file should be, or a link among the directories above it, is refused by name, never followed
/// or read.
fn read_file(root: &Path, path: &str) -> Result<Option<String>, Diagnostic> {
    read_file_or(root, path, unreadable)
}

/// [`read_file`], with the refusal worded by `unreadable` (LD-433: the dnf reader's own).
pub(super) fn read_file_or(
    root: &Path,
    path: &str,
    unreadable: fn(&str, &str) -> Diagnostic,
) -> Result<Option<String>, Diagnostic> {
    if !parents_unlinked_or(root, path, unreadable)? {
        return Ok(None);
    }
    let real = super::plan::join(root, path);
    match fs::symlink_metadata(&real) {
        Ok(meta) if meta.file_type().is_symlink() => {
            return Err(unreadable(
                path,
                "it is a symbolic link, which lodi does not follow",
            ));
        }
        Ok(meta) if !meta.is_file() => {
            return Err(unreadable(path, "it is not a regular file"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(unreadable(path, &error.to_string())),
    }
    let handle = super::files::open_regular(&real, path)?;
    let mut bytes = Vec::new();
    handle
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| unreadable(path, &error.to_string()))?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(unreadable(path, "it is over 1 MiB"));
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|_| unreadable(path, "it is not UTF-8 text"))
}

/// The addresses each local apt mirror list among `entries`' URIs names, by the URI as written
/// (`mirror+file:`, apt-transport-mirror(1)): the first field of every line that is neither
/// blank nor a comment, read through the same no-link, bounded reader as the source files.
/// Debian's cloud image reaches both of its archives this way, so a pinned apply needs these to
/// know which of the machine's stanzas are its base archives (M-Pin, D-A). A list that is not
/// there names nothing; a remote list (`mirror://`, `mirror+http://`) is not read.
pub fn read_mirror_lists(
    root: &Path,
    entries: &[Entry],
) -> Result<std::collections::BTreeMap<String, Vec<String>>, Diagnostic> {
    let mut out = std::collections::BTreeMap::new();
    for uri in entries.iter().flat_map(|entry| &entry.uris) {
        let Some(rest) = uri.strip_prefix("mirror+file:") else {
            continue;
        };
        // `mirror+file:/path` and `mirror+file:///path` name the same local file.
        let path = rest.strip_prefix("//").unwrap_or(rest);
        if !path.starts_with('/') || out.contains_key(uri) {
            continue;
        }
        if path.split('/').any(|component| component == "..") {
            return Err(unreadable(
                path,
                "its path climbs with `..`, which lodi does not follow",
            ));
        }
        let Some(text) = read_file(root, path)? else {
            continue;
        };
        let listed = text
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .filter_map(|line| line.split_whitespace().next())
            .map(str::to_string)
            .collect();
        out.insert(uri.clone(), listed);
    }
    Ok(out)
}

/// URIs of a managed stanza about to depart, read through the same no-link, bounded apt
/// source reader as the machine's other stanzas. A missing file has no active URI. Keep the
/// name separately so even that case names its source if the forced refresh fails.
pub fn read_managed_uris(root: &Path, path: &str) -> Result<Vec<String>, Diagnostic> {
    let Some(text) = read_file(root, path)? else {
        return Ok(Vec::new());
    };
    let mut uris = Vec::new();
    for listed in deb822(path, &text)? {
        if !uris.contains(&listed.uri) {
            uris.push(listed.uri);
        }
    }
    Ok(uris)
}

/// One enabled entry of a machine's apt source file — a deb822 stanza or a one-line source — with
/// every field Lodi reads, as written: what [`read_machine`] projects to (URI, suite) pairs, and
/// what the host import decides a `[sources]` block from (LD-367). There is one parser, and this
/// is its output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Entry {
    /// The in-root path of the file.
    pub file: String,
    /// The line the stanza or the source line starts on.
    pub line: usize,
    pub types: Vec<String>,
    /// As written; [`normalize_uri`] is the comparison.
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    pub components: Vec<String>,
    pub architectures: Vec<String>,
    /// `Signed-By`, or the `signed-by=` option, as written, when it is a value on one line.
    pub signed_by: Option<String>,
    /// `Signed-By` given as an inline key block rather than a path.
    pub inline_key: bool,
    /// `Trusted: yes`, or the `trusted=yes` option.
    pub trusted: bool,
}

impl Entry {
    /// Every (URI, suite) pair this entry lists, URIs normalized.
    pub fn listed(&self) -> Vec<Listed> {
        pairs(&self.uris, &self.suites)
            .into_iter()
            .map(|(uri, suite)| Listed {
                file: self.file.clone(),
                uri,
                suite,
            })
            .collect()
    }
}

/// The one-line format of `sources.list(5)`: `deb [OPTIONS] URI SUITE [COMPONENT…]`, `#` to the
/// end of a line a comment.
pub fn one_line(path: &str, text: &str) -> Result<Vec<Listed>, Diagnostic> {
    Ok(entries_one_line(path, text)?
        .iter()
        .flat_map(Entry::listed)
        .collect())
}

/// The same, each source line as the [`Entry`] it is.
pub fn entries_one_line(path: &str, text: &str) -> Result<Vec<Entry>, Diagnostic> {
    let mut out = Vec::new();
    for (index, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or_default().trim();
        if line.is_empty() {
            continue;
        }
        let bad = |why: &str| unreadable(path, &format!("line {}: {why}", index + 1));
        let (kind, mut rest) = line
            .split_once(char::is_whitespace)
            .ok_or_else(|| bad("a source line needs a type, a URI and a suite"))?;
        if kind != "deb" && kind != "deb-src" {
            return Err(bad("a source line starts with `deb` or `deb-src`"));
        }
        let mut entry = Entry {
            file: path.to_string(),
            line: index + 1,
            types: vec![kind.to_string()],
            ..Entry::default()
        };
        rest = rest.trim_start();
        if let Some(options) = rest.strip_prefix('[') {
            let end = options
                .find(']')
                .ok_or_else(|| bad("an option list `[` has no closing `]`"))?;
            for option in options[..end].split_whitespace() {
                let (key, value) = option.split_once('=').unwrap_or((option, ""));
                match key.to_ascii_lowercase().as_str() {
                    "signed-by" => entry.signed_by = Some(value.to_string()),
                    "trusted" => entry.trusted = value.eq_ignore_ascii_case("yes"),
                    "arch" => {
                        entry.architectures = value.split(',').map(str::to_string).collect();
                    }
                    _ => {}
                }
            }
            rest = options[end + 1..].trim_start();
        }
        let mut words = rest.split_whitespace();
        let (Some(uri), Some(suite)) = (words.next(), words.next()) else {
            return Err(bad("a source line needs a URI and a suite"));
        };
        entry.uris = vec![uri.to_string()];
        entry.suites = vec![suite.to_string()];
        entry.components = words.map(str::to_string).collect();
        out.push(entry);
    }
    Ok(out)
}

/// The deb822 format of `sources.list(5)`: stanzas separated by blank lines, `Field: value` with
/// continuation lines indented, `#` lines comments. A stanza with `Enabled: no` lists nothing.
pub fn deb822(path: &str, text: &str) -> Result<Vec<Listed>, Diagnostic> {
    Ok(entries_deb822(path, text)?
        .iter()
        .flat_map(Entry::listed)
        .collect())
}

/// The same, each enabled stanza as the [`Entry`] it is.
pub fn entries_deb822(path: &str, text: &str) -> Result<Vec<Entry>, Diagnostic> {
    let mut out = Vec::new();
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut start = 0;
    let lines: Vec<&str> = text.lines().collect();
    for (index, line) in lines.iter().enumerate() {
        let bad = |why: &str| unreadable(path, &format!("line {}: {why}", index + 1));
        if line.starts_with('#') {
            continue;
        }
        if line.trim().is_empty() {
            out.extend(stanza_entry(path, start, &fields)?);
            fields.clear();
            continue;
        }
        if line.starts_with([' ', '\t']) {
            let Some((_, value)) = fields.last_mut() else {
                return Err(bad("a continuation line belongs to no field"));
            };
            value.push('\n');
            value.push_str(line.trim());
            continue;
        }
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| bad("a line is neither `Field: value`, a continuation nor a comment"))?;
        if name.is_empty() || name.contains(char::is_whitespace) {
            return Err(bad("a field name holds no whitespace"));
        }
        if fields.is_empty() {
            start = index + 1;
        }
        fields.push((name.to_string(), value.trim().to_string()));
    }
    out.extend(stanza_entry(path, start, &fields)?);
    Ok(out)
}

fn stanza_entry(
    path: &str,
    line: usize,
    fields: &[(String, String)],
) -> Result<Option<Entry>, Diagnostic> {
    if fields.is_empty() {
        return Ok(None);
    }
    let get = |name: &str| {
        fields
            .iter()
            .find(|(field, _)| field.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    };
    if get("Enabled").is_some_and(|value| value.trim().eq_ignore_ascii_case("no")) {
        return Ok(None);
    }
    let words = |value: Option<&str>| -> Vec<String> {
        value
            .unwrap_or_default()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    let uris = words(get("URIs"));
    let suites = words(get("Suites"));
    if uris.is_empty() || suites.is_empty() {
        return Err(unreadable(
            path,
            &format!("the stanza at line {line} has no URIs or no Suites"),
        ));
    }
    let mut types = words(get("Types"));
    if types.is_empty() {
        types = vec!["deb".to_string()];
    }
    let signed_by = get("Signed-By");
    let inline_key = signed_by
        .is_some_and(|value| value.contains('\n') || value.trim_start().starts_with("-----"));
    Ok(Some(Entry {
        file: path.to_string(),
        line,
        types,
        uris,
        suites,
        components: words(get("Components")),
        architectures: words(get("Architectures")),
        signed_by: signed_by
            .filter(|_| !inline_key)
            .map(|value| value.trim().to_string()),
        inline_key,
        trusted: get("Trusted").is_some_and(|value| value.trim().eq_ignore_ascii_case("yes")),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uri_is_compared_by_scheme_and_host_lower_cased_and_one_slash() {
        assert_eq!(
            normalize_uri("HTTPS://Download.Docker.COM/linux/Debian/"),
            "https://download.docker.com/linux/Debian"
        );
        assert_eq!(normalize_uri("https://a.example//"), "https://a.example/");
        assert_eq!(normalize_uri("https://a.example"), "https://a.example");
    }

    #[test]
    fn both_formats_read_and_a_disabled_or_commented_stanza_lists_nothing() {
        let listed = one_line(
            "/x.list",
            "# deb https://no.example off main\n\
             deb [arch=amd64 signed-by=/k.gpg] https://A.example/d/ bookworm main # c\n\
             deb-src https://b.example ./\n",
        )
        .unwrap();
        let got: Vec<(&str, &str)> = listed
            .iter()
            .map(|l| (l.uri.as_str(), l.suite.as_str()))
            .collect();
        assert_eq!(
            got,
            [
                ("https://a.example/d", "bookworm"),
                ("https://b.example", "./")
            ]
        );
        let listed = deb822(
            "/x.sources",
            "# comment\nTypes: deb\nURIs: https://a.example https://b.example\nSuites: s t\n\
             Components: main\nSigned-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n\n\
             #Types: deb\n#URIs: https://c.example\n#Suites: u\n\n\
             Types: deb\nURIs: https://d.example\nSuites: v\nEnabled: no\n",
        )
        .unwrap();
        assert_eq!(listed.len(), 4);
        assert!(listed.iter().all(|l| l.file == "/x.sources"));
        assert!(deb822("/y.sources", "Types deb\n").is_err());
        assert!(deb822("/y.sources", "Types: deb\nSuites: s\n").is_err());
        assert!(one_line("/y.list", "rpm https://a.example s\n").is_err());
    }

    /// The one parser's full reading of an entry, which the host import decides from (LD-367):
    /// both formats give the same fields, an inline key block and `trusted` are seen as what
    /// they are, and a disabled stanza is no entry.
    #[test]
    fn both_formats_give_every_field_the_import_reads() {
        let deb = entries_deb822(
            "/d.sources",
            "Types: deb\nURIs: https://example.invalid/debian\nSuites: bookworm\n\
             Components: stable\nArchitectures: amd64\nSigned-By: /etc/apt/keyrings/x.gpg\n",
        )
        .unwrap();
        let list = entries_one_line(
            "/d.list",
            "# a comment\ndeb [arch=amd64 signed-by=/etc/apt/keyrings/x.gpg] \
             https://example.invalid/debian bookworm stable\n",
        )
        .unwrap();
        for entry in [&deb[0], &list[0]] {
            assert_eq!(entry.types, ["deb"]);
            assert_eq!(entry.uris, ["https://example.invalid/debian"]);
            assert_eq!(entry.suites, ["bookworm"]);
            assert_eq!(entry.components, ["stable"]);
            assert_eq!(entry.architectures, ["amd64"]);
            assert_eq!(entry.signed_by.as_deref(), Some("/etc/apt/keyrings/x.gpg"));
            assert!(!entry.trusted && !entry.inline_key);
        }
        let inline = entries_deb822(
            "/i.sources",
            "URIs: https://example.invalid/debian\nSuites: bookworm\n\
             Signed-By:\n -----BEGIN PGP PUBLIC KEY BLOCK-----\n .\n -----END PGP PUBLIC KEY BLOCK-----\n",
        )
        .unwrap();
        assert!(inline[0].inline_key && inline[0].signed_by.is_none());
        assert_eq!(inline[0].types, ["deb"]);
        assert!(
            entries_one_line(
                "/t.list",
                "deb [trusted=yes] https://example.invalid/x ./\n"
            )
            .unwrap()[0]
                .trusted
        );
        assert!(
            entries_deb822(
                "/t.sources",
                "URIs: https://x.invalid\nSuites: y\nTrusted: yes\n"
            )
            .unwrap()[0]
                .trusted
        );
        assert!(
            entries_deb822(
                "/e.sources",
                "Enabled: no\nURIs: https://x.invalid\nSuites: y\n"
            )
            .unwrap()
            .is_empty()
        );
    }
}
