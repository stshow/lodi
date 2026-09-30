//! Exact Fedora builds (fk-1, LD-434): a pin names one build, and the lock records its bytes.
//!
//! Fedora has no dated archive, so a Fedora pin is never a date: it is one build, written as dnf
//! prints it (`EPOCH:VERSION-RELEASE.ARCH`), and the lock records its name, epoch, version,
//! release, architecture, the repository that served it, the file's SHA-256 and the HTTPS URL of
//! Fedora's signed Koji copy of exactly that file. `lodi host pin` resolves a build once: dnf5
//! downloads it from a configured repository, and lodi hashes it and reads its signing key. A
//! plan and an apply never resolve: a pin the lock does not record is refused.
//!
//! An apply that installs or moves a pinned build fetches the file into the pin stage: first
//! from a configured repository (`dnf5 download`), kept only when its SHA-256 is the locked one;
//! else from the Koji URL the lock records, over HTTPS, held to the same SHA-256. A copy with
//! another digest, no signature or another signing key is refused before any change, and dnf5
//! installs the staged file with `localpkg_gpgcheck` on, so the machine's own keys verify it
//! again. When neither place serves the locked bytes the apply stops with `E_PIN_UNAVAILABLE`
//! naming the build. Koji may delete old builds; nothing here assumes it keeps them.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::diag::Diagnostic;
use crate::fetch::{FetchError, Fetcher};

use super::{Declared, PinRecord, PinsLock, Request, Resolution, Resolved};

/// Where Fedora's build system keeps each build, and below it the copies signed by each key.
pub const KOJI: &str = "https://kojipkgs.fedoraproject.org/packages";
/// The architectures a Fedora 44 x86_64 machine installs.
const ARCHES: &[&str] = &["x86_64", "noarch"];
/// The most bytes one package file may have.
const MAX_FILE: usize = 512 * 1024 * 1024;

/// One build, as a pin value names it. The epoch defaults to 0; the architecture may be left out
/// of a value, and is then the one the configured repositories serve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Build {
    pub epoch: String,
    pub version: String,
    pub release: String,
    pub arch: Option<String>,
}

impl Build {
    /// `[EPOCH:]VERSION-RELEASE[.ARCH]`, or `None`.
    pub fn parse(value: &str) -> Option<Build> {
        let (epoch, rest) = match value.split_once(':') {
            Some((e, rest)) if !e.is_empty() && e.bytes().all(|b| b.is_ascii_digit()) => {
                (e.to_string(), rest)
            }
            Some(_) => return None,
            None => ("0".to_string(), value),
        };
        let (rest, arch) = match rest.rsplit_once('.') {
            Some((rest, a)) if ARCHES.contains(&a) => (rest, Some(a.to_string())),
            _ => (rest, None),
        };
        let (version, release) = rest.rsplit_once('-')?;
        let word = |s: &str| {
            !s.is_empty()
                && s.starts_with(|c: char| c.is_ascii_alphanumeric())
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._+~^".contains(&b))
        };
        (word(version) && word(release)).then(|| Build {
            epoch,
            version: version.to_string(),
            release: release.to_string(),
            arch,
        })
    }

    /// `EPOCH:VERSION-RELEASE.ARCH`: the spelling of an installed build (`pm::dnf`).
    pub fn full(&self) -> String {
        format!(
            "{}:{}-{}.{}",
            self.epoch,
            self.version,
            self.release,
            self.arch.as_deref().unwrap_or("?")
        )
    }

    fn matches(&self, other: &Build) -> bool {
        self.epoch == other.epoch
            && self.version == other.version
            && self.release == other.release
            && (self.arch.is_none() || other.arch.is_none() || self.arch == other.arch)
    }
}

/// The package file's name, as Fedora's repositories and Koji both name it.
pub fn file_name(name: &str, build: &Build) -> String {
    format!(
        "{name}-{}-{}.{}.rpm",
        build.version,
        build.release,
        build.arch.as_deref().unwrap_or("?")
    )
}

/// The URL of Koji's copy of a build's file signed by `key` (its last eight hex digits).
pub fn koji_url(source_name: &str, name: &str, build: &Build, key: &str) -> String {
    format!(
        "{KOJI}/{source_name}/{}/{}/data/signed/{key}/{}/{}",
        build.version,
        build.release,
        build.arch.as_deref().unwrap_or("?"),
        file_name(name, build)
    )
}

/// The eight-digit key directory of a Koji URL.
fn url_key(url: &str) -> Option<&str> {
    let (_, tail) = url.split_once("/data/signed/")?;
    let key = tail.split('/').next()?;
    (key.len() == 8
        && key
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()))
    .then_some(key)
}

/// The issuer of the header signature of an rpm package file, as `keyring::signing_key` spells
/// it (a fingerprint or a key id, upper-case hex); `None` for a file with no signature. It reads
/// the signature header after the 96-byte lead: the RSA or DSA header signature, else the
/// header-and-payload one.
pub fn signing_key(bytes: &[u8]) -> Option<String> {
    let header = bytes.get(96..)?;
    if header.get(..4)? != [0x8e, 0xad, 0xe8, 0x01] {
        return None;
    }
    let word = |at: usize| -> Option<usize> {
        Some(u32::from_be_bytes(header.get(at..at + 4)?.try_into().ok()?) as usize)
    };
    let (count, size) = (word(8)?, word(12)?);
    let store = header.get(16 + 16 * count..16 + 16 * count + size)?;
    let mut found = BTreeMap::new();
    for index in 0..count.min(1024) {
        let at = 16 + 16 * index;
        let (tag, kind, offset, length) = (word(at)?, word(at + 4)?, word(at + 8)?, word(at + 12)?);
        if kind == 7 {
            found.insert(tag, store.get(offset..offset.checked_add(length)?)?);
        }
    }
    [268, 267, 1002, 1005]
        .iter()
        .find_map(|tag| found.get(tag))
        .and_then(|signature| super::keyring::signing_key(signature))
}

// ---------------------------------------------------------------------------- the record ---

/// The lock's record of a resolved Fedora build.
pub fn record(resolved: &Resolved) -> PinRecord {
    let build = Build::parse(&resolved.version);
    let field = |f: fn(&Build) -> String| build.as_ref().map(f);
    PinRecord {
        policy: resolved.policy.to_string(),
        requested: resolved.requested.clone(),
        snapshot: None,
        version: field(|b| b.version.clone()).unwrap_or_default(),
        sha256: resolved.sha256.clone(),
        filename: resolved.filename.clone(),
        repository: resolved.repository.clone(),
        epoch: field(|b| b.epoch.clone()),
        release: field(|b| b.release.clone()),
        arch: build.as_ref().and_then(|b| b.arch.clone()),
        source: resolved.source.clone(),
    }
}

/// A recorded Fedora build, checked field by field: the lock is user-owned, so nothing in it is
/// used as a path or a URL until it has the one shape this build writes.
fn from_record(name: &str, record: &PinRecord) -> Result<Resolved, Diagnostic> {
    let bad = |why: &str| {
        Diagnostic::new(
            "E_LOCK_VERSION",
            format!("the lock's record of the fedora pin `{name}` {why}"),
        )
        .hint(format!(
            "record it again with `lodi host pin {name} --to BUILD`; nothing was changed"
        ))
    };
    let (Some(epoch), Some(release), Some(arch), Some(source)) =
        (&record.epoch, &record.release, &record.arch, &record.source)
    else {
        return Err(bad("lacks its epoch, release, architecture or source"));
    };
    let build = Build::parse(&format!("{epoch}:{}-{release}.{arch}", record.version))
        .filter(|b| b.arch.is_some())
        .ok_or_else(|| bad("is not a build"))?;
    if record.filename != file_name(name, &build) {
        return Err(bad("names another file"));
    }
    if !record
        .sha256
        .strip_prefix("sha256:")
        .is_some_and(crate::debian::index::is_sha256_hex)
    {
        return Err(bad("has no SHA-256"));
    }
    let koji = source
        .strip_prefix(&format!("{KOJI}/"))
        .and_then(|rest| rest.split('/').next())
        .zip(url_key(source))
        .map(|(source_name, key)| koji_url(source_name, name, &build, key));
    if koji.as_deref() != Some(source.as_str()) {
        return Err(bad(
            "names a source that is not Fedora's signed Koji copy of it",
        ));
    }
    Ok(Resolved {
        name: name.to_string(),
        requested: record.requested.clone(),
        policy: "version",
        version: build.full(),
        sha256: record.sha256.clone(),
        filename: record.filename.clone(),
        repository: record.repository.clone(),
        snapshot: None,
        recorded: true,
        source: Some(source.clone()),
    })
}

/// The refusal of a date on Fedora: there is no dated archive to read one from.
fn no_dates(what: &str) -> Diagnostic {
    Diagnostic::new(
        "E_UNSUPPORTED",
        format!("{what} is a date, and fedora has no dated archive to pin to"),
    )
    .hint(
        "pin a build such as \"0:1.10.4-1.fc44.x86_64\"; `lodi host versions NAME` lists the \
         builds. Nothing was changed",
    )
}

/// What a plan knows of this machine's Fedora pins: each one from the lock's record of exactly
/// that request, with no request of its own. A pin the lock does not record is `E_LOCK_STALE`.
pub fn resolve(
    release: &str,
    snapshot: Option<&str>,
    declared: &BTreeMap<String, Declared>,
    lock: Option<&PinsLock>,
) -> Result<Resolution, Diagnostic> {
    if snapshot.is_some() {
        return Err(no_dates("`[host] snapshot`"));
    }
    let lock = lock.filter(|lock| lock.distro == "fedora" && lock.release == release);
    let mut resolution = Resolution::default();
    for (name, pin) in declared {
        let Request::Version(value) = &pin.request else {
            return Err(no_dates(&format!("the pin of `{name}`")));
        };
        let wanted = parse_value(name, value)?;
        let Some(record) = lock
            .and_then(|lock| lock.pins.get(name))
            .filter(|r| r.requested == pin.requested && r.policy == "version")
        else {
            return Err(Diagnostic::new(
                "E_LOCK_STALE",
                format!(
                    "`{name}` is pinned to {value} in host.toml, and the lock records no build \
                     for that pin"
                ),
            )
            .hint(format!(
                "record it with `lodi host pin {name} --to {value}` on the repository, then \
                 commit both files; nothing was changed"
            )));
        };
        let resolved = from_record(name, record)?;
        if !Build::parse(&resolved.version).is_some_and(|b| wanted.matches(&b)) {
            return Err(Diagnostic::new(
                "E_LOCK_STALE",
                format!(
                    "the lock records {} for `{name}`, pinned to {value}",
                    resolved.version
                ),
            )
            .hint(format!(
                "record it again with `lodi host pin {name} --to {value}`; nothing was changed"
            )));
        }
        resolution.pins.insert(name.clone(), resolved);
    }
    Ok(resolution)
}

/// A pin value as a build, or `E_TYPE`.
pub fn parse_value(name: &str, value: &str) -> Result<Build, Diagnostic> {
    Build::parse(value).ok_or_else(|| {
        Diagnostic::new(
            "E_TYPE",
            format!("the pin of `{name}` is {value}, which is not a fedora build"),
        )
        .hint("write a build as dnf prints it, such as \"0:1.10.4-1.fc44.x86_64\"")
    })
}

// ------------------------------------------------------------------------ the pin verb ---

/// One build a configured repository offers: its build, repository and source package name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    pub build: Build,
    pub repository: String,
    pub source_name: String,
}

/// Every build of `name` the configured repositories offer, from dnf5's
/// `%{name} %{epoch} %{version} %{release} %{arch} %{repoid} %{sourcerpm}` rows.
pub fn offers(name: &str, text: &str) -> Vec<Offer> {
    let mut out = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [n, epoch, version, release, arch, repository, srpm] = fields.as_slice() else {
            continue;
        };
        let suffix = format!("-{version}-{release}.src.rpm");
        let (Some(source_name), true) = (srpm.strip_suffix(&suffix), ARCHES.contains(arch)) else {
            continue;
        };
        if *n != name {
            continue;
        }
        if let Some(build) = Build::parse(&format!("{epoch}:{version}-{release}.{arch}")) {
            out.push(Offer {
                build,
                repository: (*repository).to_string(),
                source_name: source_name.to_string(),
            });
        }
    }
    out.sort_by(|a, b| {
        crate::hostscope::pm::dnf::compare_evr(&b.build.full(), &a.build.full())
            .then_with(|| a.repository.cmp(&b.repository))
    });
    out.dedup_by(|a, b| a.build == b.build);
    out
}

/// The private directory one pin downloads into, created afresh with mode 0700 in the
/// temporary directory and removed when it is dropped.
pub struct Downloads(pub PathBuf);

impl Downloads {
    pub fn new() -> Result<Downloads, Diagnostic> {
        use std::os::unix::fs::DirBuilderExt;
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.subsec_nanos());
        let dir = std::env::temp_dir().join(format!(
            "lodi-pin.{}.{}{nanos:09}",
            std::process::id(),
            crate::util::now_utc()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", dir.display())))?;
        Ok(Downloads(dir))
    }
}

impl Drop for Downloads {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `E_PIN_UNAVAILABLE` for a build no place serves, naming the places tried.
pub fn unavailable(name: &str, build: &str, tried: &str) -> Diagnostic {
    let mut d = Diagnostic::new(
        "E_PIN_UNAVAILABLE",
        format!("{name} {build} is pinned, and not served"),
    );
    d.notes.push(format!("{tried} serves those exact bytes"));
    d.hint("nothing was installed or removed; pin another build, or remove the pin")
}

/// `E_PIN_UNTRUSTED` for a file with no signature, or one by another key than the lock's.
fn untrusted(name: &str, build: &str, found: Option<&str>, want: Option<&str>) -> Diagnostic {
    let mut d = Diagnostic::new(
        "E_PIN_UNTRUSTED",
        format!(
            "the file of {name} {build} {}",
            match found {
                None => "is not signed".to_string(),
                Some(key) => format!("is signed by key {key}"),
            }
        ),
    );
    if let Some(want) = want {
        d.notes.push(format!("the lock names key {want}"));
    }
    d.notes
        .push("nothing was installed; no other copy or key is used in its place".into());
    d.hint("pin a build Fedora signed, or remove the pin")
}

/// Resolve `name` at `build` for `lodi host pin`: the newest offer that matches, downloaded by
/// dnf5 from a configured repository, hashed and its signing key read.
pub fn lookup(
    root: &Path,
    operation: crate::hostscope::safety::Operation,
    name: &str,
    build: &Build,
) -> Result<Resolved, Diagnostic> {
    let downloads = Downloads::new()?;
    let dnf = crate::hostscope::pm::dnf::Dnf::new(root, operation).with_home(&downloads.0);
    let offer = dnf
        .offers(name)?
        .into_iter()
        .find(|offer| build.matches(&offer.build))
        .ok_or_else(|| unavailable(name, &build.full(), "no configured repository"))?;
    let nevra = format!("{name}-{}", offer.build.full());
    let file = file_name(name, &offer.build);
    let bytes = dnf
        .download(&nevra, &downloads.0, &file)?
        .ok_or_else(|| unavailable(name, &offer.build.full(), "no configured repository"))?;
    let key =
        signing_key(&bytes).ok_or_else(|| untrusted(name, &offer.build.full(), None, None))?;
    let short = key[key.len().saturating_sub(8)..].to_ascii_lowercase();
    let full = offer.build.full();
    Ok(Resolved {
        name: name.to_string(),
        requested: full.clone(),
        policy: "version",
        version: full,
        sha256: format!("sha256:{}", crate::util::sha256_hex(&bytes)),
        filename: file,
        repository: offer.repository.clone(),
        snapshot: None,
        recorded: false,
        source: Some(koji_url(&offer.source_name, name, &offer.build, &short)),
    })
}

// ----------------------------------------------------------------------------- the apply ---

/// One pinned build an apply installs: where it may come from and the bytes it must be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FedoraFile {
    pub name: String,
    /// `NAME-EPOCH:VERSION-RELEASE.ARCH`, as dnf5 downloads it.
    pub nevra: String,
    pub digest: String,
    /// The file's basename in the stage.
    pub path: String,
    /// Fedora's signed Koji copy.
    pub koji: String,
}

/// The file an apply fetches for `pin`.
pub fn file(pin: &Resolved) -> Result<FedoraFile, Diagnostic> {
    let koji = pin
        .source
        .clone()
        .ok_or_else(|| Diagnostic::new("E_LOCK_VERSION", "a fedora pin without its source"))?;
    Ok(FedoraFile {
        name: pin.name.clone(),
        nevra: format!("{}-{}", pin.name, pin.version),
        digest: pin.sha256.clone(),
        path: pin.filename.clone(),
        koji,
    })
}

/// Stage `file` in `dir`: the configured repository's copy when its SHA-256 is the locked one,
/// else the Koji copy held to it; its signing key must be the one the Koji URL names. On any
/// refusal nothing is left in `dir` of it.
pub fn stage(
    dnf: &crate::hostscope::pm::dnf::Dnf,
    fetcher: &dyn Fetcher,
    file: &FedoraFile,
    dir: &Path,
) -> Result<PathBuf, Diagnostic> {
    let path = dir.join(&file.path);
    let build = &file.nevra[file.name.len() + 1..];
    let digest = |bytes: &[u8]| format!("sha256:{}", crate::util::sha256_hex(bytes));
    let mut bytes = dnf
        .download(&file.nevra, dir, &file.path)?
        .filter(|bytes| digest(bytes) == file.digest);
    if bytes.is_none() {
        let _ = std::fs::remove_file(&path);
        let fetched = match fetcher.get(&file.koji) {
            Ok(body) => body,
            Err(e) if matches!(e.status(), Some(403 | 404 | 410)) => {
                return Err(unavailable(
                    &file.name,
                    build,
                    "neither a configured repository nor Fedora's signed Koji copy",
                ));
            }
            Err(FetchError::NotFound(_)) => {
                return Err(unavailable(
                    &file.name,
                    build,
                    "neither a configured repository nor Fedora's signed Koji copy",
                ));
            }
            Err(e) => return Err(crate::debian::base::repo_error(e)),
        };
        if fetched.len() > MAX_FILE {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!("Koji's copy of {} is larger than any package", file.nevra),
            ));
        }
        let got = digest(&fetched);
        if got != file.digest {
            return Err(Diagnostic::new(
                "E_HASH_MISMATCH",
                format!(
                    "Koji's copy of {} has {got}, and the lock records {}; nothing was \
                     installed",
                    file.nevra, file.digest
                ),
            )
            .hint("no other copy is used in its place; pin another build, or remove the pin"));
        }
        std::fs::write(&path, &fetched)
            .map_err(|e| Diagnostic::new("E_STORE_IO", format!("{}: {e}", path.display())))?;
        bytes = Some(fetched);
    }
    let bytes = bytes.expect("staged");
    let want = url_key(&file.koji);
    let key = signing_key(&bytes);
    let same = key
        .as_deref()
        .zip(want)
        .is_some_and(|(key, want)| key.to_ascii_lowercase().ends_with(want));
    if !same {
        let _ = std::fs::remove_file(&path);
        return Err(untrusted(&file.name, build, key.as_deref(), want));
    }
    Ok(path)
}

/// `lodi host versions NAME` on Fedora: every build the configured repositories offer, newest
/// first, with the installed, the pinned and the latest marked, then one line to copy.
pub fn listing(
    offers: &[Offer],
    installed: Option<&str>,
    pinned: Option<&str>,
    copy: &str,
) -> String {
    let width = offers
        .iter()
        .map(|o| o.build.full().len())
        .chain(std::iter::once("build".len()))
        .max()
        .unwrap_or(0);
    let mut out = format!("{:width$}  {:10}  state\n", "build", "repository");
    for (index, offer) in offers.iter().enumerate() {
        let full = offer.build.full();
        let states: Vec<&str> = [
            (installed == Some(full.as_str()), "installed"),
            (pinned == Some(full.as_str()), "pinned"),
            (index == 0, "latest"),
        ]
        .into_iter()
        .filter_map(|(on, s)| on.then_some(s))
        .collect();
        out.push_str(
            format!(
                "{full:width$}  {:10}  {}",
                offer.repository,
                states.join(", ")
            )
            .trim_end(),
        );
        out.push('\n');
    }
    out.push_str(&format!("\n{copy}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_build_is_read_with_or_without_its_epoch_and_architecture() {
        let full = Build::parse("0:1.10.4-1.fc44.x86_64").unwrap();
        assert_eq!(full.full(), "0:1.10.4-1.fc44.x86_64");
        let bare = Build::parse("1.10.4-1.fc44").unwrap();
        assert_eq!((bare.epoch.as_str(), bare.arch.as_deref()), ("0", None));
        assert!(bare.matches(&full));
        assert_eq!(Build::parse("2:26.0.0-4.fc44").unwrap().epoch, "2");
        for bad in ["1.10.4", "-1", "x:1-1", "1.0-1;rm", "--x-1"] {
            assert_eq!(Build::parse(bad), None, "{bad}");
        }
    }

    #[test]
    fn the_koji_url_is_rebuilt_from_the_record_alone() {
        let build = Build::parse("0:1.10.4-1.fc44.x86_64").unwrap();
        let url = koji_url("pv", "pv", &build, "6d9f90a6");
        assert_eq!(
            url,
            "https://kojipkgs.fedoraproject.org/packages/pv/1.10.4/1.fc44/data/signed/\
             6d9f90a6/x86_64/pv-1.10.4-1.fc44.x86_64.rpm"
        );
        assert_eq!(url_key(&url), Some("6d9f90a6"));
    }

    #[test]
    fn the_signing_key_is_read_from_the_real_signature_header() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/host/dnf/pin");
        let read = |file: &str| signing_key(&std::fs::read(dir.join(file)).unwrap());
        for file in [
            "pv-1.10.4-1.fc44.x86_64.rpm",
            "htop-3.4.1-3.fc44.x86_64.rpm",
        ] {
            let key = read(file).unwrap();
            assert!(key.ends_with("DBFCF71C6D9F90A6"), "{file}: {key}");
        }
        assert_eq!(read("unsigned-pv-1.10.4-1.fc44.x86_64.rpm"), None);
        assert_eq!(signing_key(b"not a package"), None);
    }

    #[test]
    fn dnf5_rows_become_offers_newest_first() {
        let text = "pv 0 1.10.4 1.fc44 x86_64 fedora pv-1.10.4-1.fc44.src.rpm\n\
                    pv 0 1.10.5 1.fc44 x86_64 updates pv-1.10.5-1.fc44.src.rpm\n\
                    ripgrep 0 15.2.0 1.fc44 x86_64 updates rust-ripgrep-15.2.0-1.fc44.src.rpm\n";
        let offers = offers("pv", text);
        assert_eq!(offers.len(), 2);
        assert_eq!(offers[0].build.full(), "0:1.10.5-1.fc44.x86_64");
        assert_eq!(offers[1].repository, "fedora");
        let rg = super::offers("ripgrep", text);
        assert_eq!(rg[0].source_name, "rust-ripgrep");
    }
}
