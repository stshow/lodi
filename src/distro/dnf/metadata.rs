//! Fedora repository metadata: `repodata/repomd.xml` and the `primary` index it names (LD-435).
//!
//! `repomd.xml` states the SHA-256 and size of the compressed `primary` file and the SHA-256 and
//! size of its decompressed bytes. [`fetch_primary`] checks all four before a byte of `primary`
//! is parsed, and decompresses under [`MAX_PRIMARY_BYTES`], so metadata that is not the file the
//! repository describes, or that expands past the cap, is refused rather than read.
//!
//! The parser reads only the elements of `primary.xml` the resolver needs; it is a small scanner
//! for the fixed shape createrepo_c writes, not a general XML parser. Anything it cannot place is
//! an error, never a package with fewer dependencies.

use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::util::sha256_hex;

/// The largest decompressed `primary.xml` accepted. Fedora 44 Everything's is 185 MB and the
/// updates archive's 413 MB; the cap is the apt family's (`MAX_INDEX_BYTES`).
pub const MAX_PRIMARY_BYTES: usize = crate::debian::base::MAX_INDEX_BYTES;
/// The largest `repomd.xml` accepted (Fedora's is about 4 KB).
const MAX_REPOMD_BYTES: usize = 1 << 20;

/// What `repomd.xml` says about the `primary` index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrimaryRef {
    /// Relative to the repository URL, as `<location href>` gives it.
    pub location: String,
    pub sha256: String,
    pub size: u64,
    pub open_sha256: String,
    pub open_size: u64,
}

/// One dependency as `primary.xml` states it: a plain name with an optional version relation,
/// or, when `name` starts with `(`, a rich dependency whose whole text is `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    pub name: String,
    pub flags: Option<Flags>,
    pub evr: Option<Evr>,
}

/// A version relation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flags {
    Eq,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Flags {
    fn parse(text: &str) -> Option<Flags> {
        Some(match text {
            "EQ" | "=" | "==" => Flags::Eq,
            "LT" | "<" => Flags::Lt,
            "LE" | "<=" | "=<" => Flags::Le,
            "GT" | ">" => Flags::Gt,
            "GE" | ">=" | "=>" => Flags::Ge,
            _ => return None,
        })
    }

    pub fn parse_operator(text: &str) -> Option<Flags> {
        Flags::parse(text)
    }

    fn less(self) -> bool {
        matches!(self, Flags::Lt | Flags::Le)
    }

    fn greater(self) -> bool {
        matches!(self, Flags::Gt | Flags::Ge)
    }

    fn equal(self) -> bool {
        matches!(self, Flags::Eq | Flags::Le | Flags::Ge)
    }
}

/// `[EPOCH:]VERSION[-RELEASE]`. An absent epoch is 0; an absent release matches any release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Evr {
    pub epoch: u64,
    pub version: String,
    pub release: Option<String>,
}

impl Evr {
    /// `[EPOCH:]VERSION[-RELEASE]`, as a rich dependency writes it.
    pub fn parse(text: &str) -> Option<Evr> {
        let (epoch, rest) = match text.split_once(':') {
            Some((e, rest)) => (e.parse().ok()?, rest),
            None => (0, text),
        };
        let (version, release) = match rest.rsplit_once('-') {
            Some((v, r)) => (v, Some(r.to_string())),
            None => (rest, None),
        };
        (!version.is_empty()).then(|| Evr {
            epoch,
            version: version.to_string(),
            release,
        })
    }

    /// rpm's comparison: epoch, then version, then release when both sides state one.
    pub fn compare(&self, other: &Evr) -> std::cmp::Ordering {
        use crate::hostscope::pm::dnf::vercmp;
        self.epoch
            .cmp(&other.epoch)
            .then_with(|| vercmp(&self.version, &other.version))
            .then_with(|| match (&self.release, &other.release) {
                (Some(a), Some(b)) => vercmp(a, b),
                _ => std::cmp::Ordering::Equal,
            })
    }
}

/// Whether a provide `(pf, pe)` satisfies a requirement `(rf, re)`: rpm's range overlap
/// (`rpmdsCompare`). An unversioned side satisfies, or is satisfied by, any version.
pub fn overlaps(
    provide: (Option<Flags>, Option<&Evr>),
    require: (Option<Flags>, Option<&Evr>),
) -> bool {
    let (Some(pf), Some(pe), Some(rf), Some(re)) = (provide.0, provide.1, require.0, require.1)
    else {
        return true;
    };
    match pe.compare(re) {
        std::cmp::Ordering::Less => pf.greater() || rf.less(),
        std::cmp::Ordering::Greater => pf.less() || rf.greater(),
        std::cmp::Ordering::Equal => {
            (pf.equal() && rf.equal()) || (pf.less() && rf.less()) || (pf.greater() && rf.greater())
        }
    }
}

/// One package of a `primary.xml`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RpmPackage {
    pub name: String,
    pub arch: String,
    pub evr: Evr,
    /// The package file's SHA-256 (`<checksum type="sha256" pkgid="YES">`).
    pub sha256: String,
    pub size: u64,
    /// Relative to the repository URL.
    pub location: String,
    /// The repository this record came from.
    pub repository: String,
    pub provides: Vec<Dep>,
    pub requires: Vec<Dep>,
    /// The file paths `primary.xml` lists (the executables and `/etc` files createrepo_c keeps).
    pub files: Vec<String>,
}

impl RpmPackage {
    /// `NAME-[EPOCH:]VERSION-RELEASE.ARCH`, with the epoch always written.
    pub fn nevra(&self) -> String {
        format!(
            "{}-{}:{}-{}.{}",
            self.name,
            self.evr.epoch,
            self.evr.version,
            self.evr.release.as_deref().unwrap_or(""),
            self.arch
        )
    }
}

fn repo_error(url: &str, why: impl std::fmt::Display) -> Diagnostic {
    Diagnostic::new("E_REPO_UNREACHABLE", format!("{url}: {why}"))
}

/// Read what `repomd.xml` says about `primary`.
pub fn parse_repomd(text: &str) -> Result<PrimaryRef, String> {
    let mut rest = text;
    while let Some(start) = rest.find("<data ") {
        rest = &rest[start..];
        let end = rest.find("</data>").ok_or("unterminated <data> element")?;
        let data = &rest[..end];
        rest = &rest[end..];
        let head = &data[..data.find('>').ok_or("unterminated <data> tag")?];
        if attribute(head, "type").as_deref() != Some("primary") {
            continue;
        }
        let checksum = |element: &str| -> Result<String, String> {
            let open = format!("<{element} ");
            let at = data
                .find(&open)
                .ok_or_else(|| format!("primary has no <{element}>"))?;
            let tag_end = data[at..].find('>').ok_or("unterminated tag")? + at;
            if attribute(&data[at..tag_end], "type").as_deref() != Some("sha256") {
                return Err(format!("primary's <{element}> is not a SHA-256"));
            }
            let close = data[tag_end..].find('<').ok_or("unterminated checksum")? + tag_end;
            let hex = data[tag_end + 1..close].trim().to_string();
            if hex.len() != 64 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
                return Err(format!("primary's <{element}> is not a SHA-256"));
            }
            Ok(hex.to_ascii_lowercase())
        };
        let number = |element: &str| -> Result<u64, String> {
            let open = format!("<{element}>");
            let at = data
                .find(&open)
                .ok_or_else(|| format!("primary has no <{element}>"))?
                + open.len();
            let close = data[at..].find('<').ok_or("unterminated size")? + at;
            data[at..close]
                .trim()
                .parse()
                .map_err(|_| format!("primary's <{element}> is not a number"))
        };
        let location = data
            .find("<location ")
            .and_then(|at| {
                let tag = &data[at..at + data[at..].find('>')?];
                attribute(tag, "href")
            })
            .ok_or("primary has no <location href>")?;
        if location.starts_with('/') || location.contains("..") || location.contains("://") {
            return Err(format!(
                "primary's location `{location}` leaves the repository"
            ));
        }
        return Ok(PrimaryRef {
            location,
            sha256: checksum("checksum")?,
            size: number("size")?,
            open_sha256: checksum("open-checksum")?,
            open_size: number("open-size")?,
        });
    }
    Err("repomd.xml lists no primary index".into())
}

/// Fetch `repomd.xml` and the `primary` index it names from `url`, check the index against the
/// digests and sizes `repomd.xml` states, decompress it under the cap and parse it. Returns the
/// SHA-256 of `repomd.xml`, what it says about `primary`, and the packages.
pub fn fetch_primary(
    fetcher: &dyn Fetcher,
    url: &str,
    repository: &str,
) -> Result<(String, PrimaryRef, Vec<RpmPackage>), Diagnostic> {
    let repomd_url = format!("{url}repodata/repomd.xml");
    let repomd = fetcher
        .get(&repomd_url)
        .map_err(crate::debian::base::repo_error)?;
    if repomd.len() > MAX_REPOMD_BYTES {
        return Err(repo_error(&repomd_url, "repomd.xml is larger than 1 MiB"));
    }
    let repomd_sha256 = sha256_hex(&repomd);
    let text = std::str::from_utf8(&repomd).map_err(|_| repo_error(&repomd_url, "not UTF-8"))?;
    let primary = parse_repomd(text).map_err(|why| repo_error(&repomd_url, why))?;
    let primary_url = format!("{url}{}", primary.location);
    let compressed = fetcher
        .get(&primary_url)
        .map_err(crate::debian::base::repo_error)?;
    let actual = sha256_hex(&compressed);
    if actual != primary.sha256 || compressed.len() as u64 != primary.size {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{primary_url}: repomd.xml states sha256:{} ({} bytes), got sha256:{actual} ({} \
                 bytes)",
                primary.sha256,
                primary.size,
                compressed.len()
            ),
        )
        .hint("the metadata is not the file its repomd.xml describes; nothing was locked"));
    }
    let plain = decompress(&primary_url, &primary, &compressed)?;
    let packages =
        parse_primary(&plain, repository).map_err(|why| repo_error(&primary_url, why))?;
    Ok((repomd_sha256, primary, packages))
}

/// Decompress `primary` under [`MAX_PRIMARY_BYTES`] and check the decompressed digest and size.
fn decompress(url: &str, primary: &PrimaryRef, compressed: &[u8]) -> Result<Vec<u8>, Diagnostic> {
    if primary.open_size > MAX_PRIMARY_BYTES as u64 {
        return Err(repo_error(
            url,
            format!(
                "repomd.xml states {} decompressed bytes, past the {MAX_PRIMARY_BYTES}-byte cap",
                primary.open_size
            ),
        ));
    }
    let plain = if primary.location.ends_with(".zst") {
        super::super::zstd::decompress(compressed, MAX_PRIMARY_BYTES)
    } else if primary.location.ends_with(".gz") {
        use std::io::Read;
        let mut out = Vec::new();
        flate2::read::GzDecoder::new(compressed)
            .take(MAX_PRIMARY_BYTES as u64 + 1)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())
            .and_then(|_| {
                (out.len() <= MAX_PRIMARY_BYTES)
                    .then_some(out)
                    .ok_or_else(|| {
                        format!("decompressed size exceeds the {MAX_PRIMARY_BYTES}-byte limit")
                    })
            })
    } else if primary.location.ends_with(".xz") {
        crate::debian::base::xz_decompress(compressed)
    } else {
        Err(format!("unknown compression of `{}`", primary.location))
    }
    .map_err(|why| repo_error(url, why))?;
    let actual = sha256_hex(&plain);
    if actual != primary.open_sha256 || plain.len() as u64 != primary.open_size {
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "{url}: repomd.xml states decompressed sha256:{} ({} bytes), got sha256:{actual} \
                 ({} bytes)",
                primary.open_sha256,
                primary.open_size,
                plain.len()
            ),
        )
        .hint("the metadata is not the file its repomd.xml describes; nothing was locked"));
    }
    Ok(plain)
}

/// The value of attribute `name` in the text of one tag, unescaped.
fn attribute(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let at = rest.find(name)?;
        let before = rest[..at].chars().last();
        let after = &rest[at + name.len()..];
        rest = after;
        if before.is_some_and(|c| c.is_whitespace())
            && let Some(value) = after.strip_prefix("=\"")
        {
            let end = value.find('"')?;
            return unescape(&value[..end]);
        }
    }
}

/// XML's five named entities and numeric character references.
fn unescape(text: &str) -> Option<String> {
    if !text.contains('&') {
        return Some(text.to_string());
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let end = rest[at..].find(';')? + at;
        let entity = &rest[at + 1..end];
        out.push(match entity {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let code = if let Some(hex) = entity.strip_prefix("#x") {
                    u32::from_str_radix(hex, 16).ok()?
                } else {
                    entity.strip_prefix('#')?.parse().ok()?
                };
                char::from_u32(code)?
            }
        });
        rest = &rest[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// One tag of the scan: its name, its attribute text, whether it closes itself, whether it is a
/// closing tag, and the text that follows it up to the next tag.
struct Tag<'a> {
    name: &'a str,
    attributes: &'a str,
    closing: bool,
    text: &'a str,
}

fn tags(text: &str) -> impl Iterator<Item = Result<Tag<'_>, String>> {
    let mut rest = text;
    std::iter::from_fn(move || {
        loop {
            let at = rest.find('<')?;
            rest = &rest[at + 1..];
            if rest.starts_with('?') || rest.starts_with('!') {
                let Some(end) = rest.find('>') else {
                    return Some(Err("unterminated declaration".to_string()));
                };
                rest = &rest[end + 1..];
                continue;
            }
            let Some(end) = rest.find('>') else {
                return Some(Err("unterminated tag".to_string()));
            };
            let body = &rest[..end];
            rest = &rest[end + 1..];
            let text = &rest[..rest.find('<').unwrap_or(rest.len())];
            let (closing, body) = match body.strip_prefix('/') {
                Some(b) => (true, b),
                None => (false, body.strip_suffix('/').unwrap_or(body)),
            };
            let (name, attributes) = match body.find(char::is_whitespace) {
                Some(i) => (&body[..i], &body[i..]),
                None => (body, ""),
            };
            return Some(Ok(Tag {
                name,
                attributes,
                closing,
                text,
            }));
        }
    })
}

/// Parse a decompressed `primary.xml`.
pub fn parse_primary(bytes: &[u8], repository: &str) -> Result<Vec<RpmPackage>, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "primary.xml is not UTF-8".to_string())?;
    let mut packages = Vec::new();
    let mut current: Option<Partial> = None;
    let mut section: Option<&str> = None;
    let mut declared: Option<usize> = None;
    for tag in tags(text) {
        let tag = tag?;
        let attr = |name: &str| attribute(tag.attributes, name);
        match (tag.name, tag.closing) {
            ("metadata", false) => {
                declared = attr("packages").and_then(|n| n.parse().ok());
            }
            ("package", false) => {
                if current.is_some() {
                    return Err("nested <package>".into());
                }
                if attr("type").as_deref() != Some("rpm") {
                    return Err("a <package> that is not type=\"rpm\"".into());
                }
                current = Some(Partial::default());
            }
            ("package", true) => {
                let p = current.take().ok_or("stray </package>")?;
                packages.push(p.finish(repository)?);
            }
            (name, closing) => {
                let Some(p) = current.as_mut() else { continue };
                match (name, closing) {
                    ("name", false) => p.name = Some(unescape(tag.text).ok_or("bad entity")?),
                    ("arch", false) => p.arch = Some(tag.text.to_string()),
                    ("version", false) => {
                        p.evr = Some(Evr {
                            epoch: attr("epoch")
                                .unwrap_or_else(|| "0".into())
                                .parse()
                                .map_err(|_| "a <version> epoch that is not a number")?,
                            version: attr("ver").ok_or("a <version> without ver")?,
                            release: Some(attr("rel").ok_or("a <version> without rel")?),
                        })
                    }
                    ("checksum", false) => {
                        if attr("type").as_deref() != Some("sha256") {
                            return Err("a package checksum that is not a SHA-256".into());
                        }
                        p.sha256 = Some(tag.text.trim().to_ascii_lowercase());
                    }
                    ("size", false) => {
                        p.size = attr("package").and_then(|s| s.parse().ok());
                    }
                    ("location", false) => p.location = attr("href"),
                    ("rpm:provides" | "rpm:requires", false) => section = Some(name),
                    ("rpm:provides" | "rpm:requires", true) => section = None,
                    ("rpm:entry", false) => {
                        let Some(which) = section else { continue };
                        let flags = match attr("flags") {
                            None => None,
                            Some(f) => Some(
                                Flags::parse(&f).ok_or_else(|| format!("unknown flags `{f}`"))?,
                            ),
                        };
                        let evr = match attr("ver") {
                            None => None,
                            Some(version) => Some(Evr {
                                epoch: attr("epoch")
                                    .map(|e| e.parse())
                                    .transpose()
                                    .map_err(|_| "an entry epoch that is not a number")?
                                    .unwrap_or(0),
                                version,
                                release: attr("rel"),
                            }),
                        };
                        let dep = Dep {
                            name: attr("name").ok_or("an entry without a name")?,
                            flags,
                            evr,
                        };
                        if which == "rpm:provides" {
                            p.provides.push(dep);
                        } else {
                            p.requires.push(dep);
                        }
                    }
                    ("file", false) => p.files.push(unescape(tag.text).ok_or("bad entity")?),
                    _ => {}
                }
            }
        }
    }
    if current.is_some() {
        return Err("unterminated <package>".into());
    }
    match declared {
        Some(n) if n == packages.len() => Ok(packages),
        Some(n) => Err(format!(
            "primary.xml declares {n} packages and holds {}",
            packages.len()
        )),
        None => Err("primary.xml has no <metadata packages=\"N\">".into()),
    }
}

#[derive(Default)]
struct Partial {
    name: Option<String>,
    arch: Option<String>,
    evr: Option<Evr>,
    sha256: Option<String>,
    size: Option<u64>,
    location: Option<String>,
    provides: Vec<Dep>,
    requires: Vec<Dep>,
    files: Vec<String>,
}

impl Partial {
    fn finish(self, repository: &str) -> Result<RpmPackage, String> {
        let name = self.name.ok_or("a package without a name")?;
        let missing = |what: &str| format!("package {name} has no {what}");
        let sha256 = self.sha256.ok_or_else(|| missing("checksum"))?;
        if sha256.len() != 64 || !sha256.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("package {name} has a malformed SHA-256"));
        }
        let location = self.location.ok_or_else(|| missing("location"))?;
        if location.starts_with('/') || location.contains("..") || location.contains("://") {
            return Err(format!(
                "package {name}'s location `{location}` leaves the repository"
            ));
        }
        Ok(RpmPackage {
            arch: self.arch.ok_or_else(|| missing("arch"))?,
            evr: self.evr.ok_or_else(|| missing("version"))?,
            size: self.size.ok_or_else(|| missing("size"))?,
            sha256,
            location,
            repository: repository.to_string(),
            provides: self.provides,
            requires: self.requires,
            files: self.files,
            name,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_overlap_as_rpm_compares_them() {
        let e = |t: &str| Evr::parse(t).unwrap();
        let p = e("1:2.0-3.fc44");
        assert!(overlaps(
            (Some(Flags::Eq), Some(&p)),
            (Some(Flags::Ge), Some(&e("1:2.0")))
        ));
        assert!(!overlaps(
            (Some(Flags::Eq), Some(&p)),
            (Some(Flags::Ge), Some(&e("2:1.0")))
        ));
        // A requirement without a release matches every release of its version.
        assert!(overlaps(
            (Some(Flags::Eq), Some(&p)),
            (Some(Flags::Eq), Some(&e("1:2.0")))
        ));
        assert!(!overlaps(
            (Some(Flags::Eq), Some(&p)),
            (Some(Flags::Lt), Some(&e("1:2.0")))
        ));
        assert!(overlaps(
            (Some(Flags::Eq), Some(&e("1.0~rc1-1"))),
            (Some(Flags::Lt), Some(&e("1.0")))
        ));
        assert!(overlaps((None, None), (Some(Flags::Gt), Some(&e("9")))));
    }

    #[test]
    fn entities_and_attributes_unescape() {
        assert_eq!(
            attribute(r#" name="(a &lt; 2 with a &gt;= 1)" flags="EQ""#, "name").as_deref(),
            Some("(a < 2 with a >= 1)")
        );
        assert_eq!(
            attribute(r#" rel="1" ver="2""#, "ver").as_deref(),
            Some("2")
        );
        assert_eq!(unescape("&#65;&#x42;&amp;").as_deref(), Some("AB&"));
    }
}
