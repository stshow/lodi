//! The repositories an apt machine has enabled, read for `lodi import` and written back as
//! `[sources]` blocks a person can uncomment (LD-367).
//!
//! # One source of truth for attribution
//!
//! Which repository an installed package came from is read from **one** place: the
//! `apt-cache policy` recording R-2 already classifies origins by ([`pm::Backend::offers`], the
//! same two readings [`pm::Backend::origins`] collapses). A package is attributed to a stanza
//! when a source line of its **installed** version is one of that stanza's (URI, suite,
//! component) triples, and to nothing else. No `Packages` index under `/var/lib/apt/lists` is
//! read, so a compressed index is not "nothing came from it", and a stanza is the
//! distribution's own when the `Release` file of a source it lists says `o=Debian` or
//! `o=Ubuntu` — never by its host name, so a country mirror is the distribution's own too.
//!
//! # A suite of the distribution's own archive that a fresh install does not enable
//!
//! A stanza of the distribution's own archive is counted, not written. A suite it lists that a
//! fresh install of the release does not enable is named under `NOT CAPTURED` by the file that
//! lists it (LD-414): the defaults are `CODENAME`, `-updates` and `-security` ([`DEFAULT_SUITES`]).
//! An extra suite of [`EXTRA_SUITES`] (Ubuntu's `-backports` and `-proposed`, Debian's
//! `-backports`, whose `Release` origin is `Debian Backports`) is written instead as a commented
//! `[sources]` block over `https://`, signed by the stanza's own key or the distribution's
//! archive keyring (si-1, S5). On Debian a suite written by its alias (`oldstable`) is the one
//! its `Release` names as its codename (`n=`); Ubuntu's `n=` is the release's codename for every
//! suite. A stanza every suite of which is named is not counted. A machine whose release states
//! no codename names none.
//!
//! The recording says which repositories **offer** the installed version, not where it was
//! installed from. A package is attributed only when exactly one distinct enabled repository of
//! the recording offers its installed version — counted by repository, not by stanza, so two
//! URIs of one stanza are two repositories, and a repository no stanza lists still counts — and
//! exactly one stanza lists it. Any other package is attributed to no block: it is named under
//! `NOT CAPTURED` as ambiguous or unattributed, and never guessed.
//!
//! # What it reads, and what it writes
//!
//! Read-only, below the root, no link followed anywhere on the way: `/etc/apt/sources.list` and
//! the `.sources` and `.list` files under `/etc/apt/sources.list.d/`, through the one apt source
//! reader, [`sourceset::read_entries`], which refuses a link at `/etc` or `/etc/apt` by name; and
//! the keyring each carried stanza's `Signed-By` names, walked component by component the same
//! way, opened with `O_NOFOLLOW`, bounded at [`MAX_KEYRING_BYTES`], whose framing
//! [`declared::sniff`] reads (S4) — so private key material anywhere in it keeps it off the
//! manifest. No process is started here and no network is touched. Nothing under
//! `/etc/apt/auth.conf.d/` is opened: those files are named, by path, as never carried, and none
//! is listed through a link.
//!
//! A repository is emitted as a **comment**, because the apply that takes it writes a trust
//! anchor — apt's own mechanism, digest-verified, but a step a person takes on purpose by
//! removing the `# `. The keyring travels beside the manifest under `files/`, at the path the
//! block names, with its digest on the line below.
//!
//! # What it refuses, by name and never by redaction
//!
//! A URI that is not `https://`, a URI with credentials, a fragment (named before a query, as the
//! manifest names it) or a query, `Trusted: yes` or `[trusted=yes]`, a stanza with no
//! `Signed-By` (its key can only be in apt's legacy trusted keyring, which lodi does not read), a
//! `Signed-By` that is not a regular file (an inline key block, a fingerprint, a link at it or on
//! the way to it, several paths), a keyring that is not an OpenPGP public keyring or that holds
//! secret key material, and a repository nothing installed came from.
//!
//! Arch is not read: the host scope arms no pacman repository, and the emitted file says so in
//! one line.
//!
//! # Fedora (LD-433)
//!
//! On Fedora the `.repo` files under `/etc/yum.repos.d` are read through the one dnf reader,
//! [`reposet::read_files`]. Every enabled section whose id is Fedora's own (`fedora`, `updates`,
//! or starting `fedora-` or `updates-`) is counted; every other enabled section is a third
//! party, to which a package is attributed by the repository dnf offers its installed build
//! from ([`pm::Origin::ThirdParty`]). It is written as a block only with an `https://`
//! `baseurl`, `gpgcheck` on, and one `gpgkey` that is a `file://` path to an ASCII-armored
//! public key, read as a keyring above is; the key then travels beside the manifest under
//! `files/etc/pki/rpm-gpg/`, as the apply names it. A disabled section is not read further.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::fs;
use std::io::Read;
use std::path::{Component, Path};

use crate::diag::Diagnostic;
use crate::util::sha256_hex;

use super::super::pm::{self, Origin};
use super::super::reposet;
use super::super::safety::{Distro, Gate};
use super::super::sources::{self as declared, Family, Format, MAX_KEYRING_BYTES, Refusal};
use super::super::sourceset::{self, Entry, normalize_uri};
use super::Machine;
use super::files::{Capture, Captured};

/// The release that first reads `[sources]`: what a taken block needs `[host]` to say. It is
/// written as the range `>=1.3.0`, because a bare version is a prefix a later lodi does not
/// satisfy (#228).
pub const SOURCES_MINIMUM: &str = "1.3.0";

/// The release that first reads `[sources]` on Fedora (LD-433).
pub const FEDORA_SOURCES_MINIMUM: &str = "1.8.0";

/// One repository the import writes as a commented `[sources]` block.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Carried {
    /// The `[sources.NAME]` the block declares: the file's stem, made a source name.
    pub name: String,
    /// The file it was read from, as a path on the machine (never the root it was read under).
    pub path: String,
    pub types: Vec<String>,
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    pub components: Vec<String>,
    pub architectures: Vec<String>,
    /// The keyring's own path on the machine, as `Signed-By` names it.
    pub keyring_path: String,
    /// The keyring as found, bytes; only its framing is read.
    pub keyring: Vec<u8>,
    /// What the keyring's framing is, which decides the name it travels under (S4).
    pub format: Format,
    pub sha256: String,
    /// The installed names attributed to this repository, sorted.
    pub packages: Vec<String>,
    /// apt, or dnf on Fedora: which paths the apply writes, and how the block reads.
    pub family: Family,
    /// A suite of the distribution's own archive that a fresh install does not enable, signed
    /// by the distribution's own key (si-1, S5).
    pub own: bool,
}

impl Carried {
    /// Where the keyring travels, relative to the manifest: the path the block's `signed_by`
    /// names, `files/etc/apt/keyrings/lodi-NAME.{asc|gpg}`, or on Fedora
    /// `files/etc/pki/rpm-gpg/lodi-NAME.asc`.
    pub fn source(&self) -> String {
        format!("files{}", self.family.key_path(&self.name, self.format))
    }
}

/// Why a stanza that was read is not written as a block. Every variant is a refusal, never an
/// error, and each is named beside the file it was read from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reason {
    NotHttps,
    Credentials,
    /// A `#` in a URI: named before a query, as the manifest names it (S2, S3).
    Fragment,
    Query,
    Trusted,
    NoSignedBy,
    SignedByNotAFile(String),
    SecretKey,
    NotAKeyring,
    NothingInstalled,
    /// A suite of the distribution's own archive, as the stanza writes it, that a fresh install
    /// of the release does not enable (LD-414).
    OwnSuite(String),
    /// A non-default suite of another origin in a stanza also listing the distribution's own.
    MixedSuite(String),
    /// The file could not be read as an apt source; the reader's own words.
    Malformed(String),
    /// A dnf repository refused, in words of its own (LD-433).
    Dnf(String),
}

impl Reason {
    pub fn why(&self) -> String {
        match self {
            Reason::NotHttps => "its URI is not https://, which every source lodi arms is".into(),
            Reason::Credentials => "its URI carries credentials, which are never carried".into(),
            Reason::Fragment => {
                "its URI carries a fragment (`#…`), which no source lodi arms has".into()
            }
            Reason::Query => "its URI carries a query (`?…`), which no source lodi arms has".into(),
            Reason::Trusted => {
                "it is marked trusted, which bypasses signing; lodi arms no such source".into()
            }
            Reason::NoSignedBy => {
                "it names no Signed-By keyring: its key is in apt's legacy trusted keyring, \
                 which lodi does not read"
                    .into()
            }
            Reason::SignedByNotAFile(detail) => {
                format!("its Signed-By is not a regular keyring file: {detail}")
            }
            Reason::SecretKey => {
                format!(
                    "its keyring {}, which is never carried",
                    Refusal::Secret.text()
                )
            }
            Reason::NotAKeyring => format!("its keyring {}", Refusal::NotKeyring.text()),
            Reason::NothingInstalled => {
                "no installed package came from it: none is attributed to it alone".into()
            }
            Reason::OwnSuite(suite) => format!(
                "{suite} is a suite of the distribution's own archive that a fresh install\n\
                 does not enable, and lodi does not write such a suite as a [sources] block yet"
            ),
            Reason::MixedSuite(suite) => format!(
                "{suite} is a non-default archive suite with a different Release origin; lodi\n\
                 does not write a mixed stanza as a [sources] block yet"
            ),
            Reason::Malformed(why) => format!("it could not be read as an apt source: {why}"),
            Reason::Dnf(why) => why.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The file the stanza was read from. Never the URI: a refused URI is refused because of
    /// what it carries.
    pub path: String,
    pub reason: Reason,
    /// The installed names attributed to it, which stay off-base.
    pub packages: Vec<String>,
}

/// Why an installed name from outside the distribution is attributed to no block.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Unattributed {
    /// More than one distinct enabled repository offers its installed version, however the
    /// stanzas list them: in one multi-URI stanza, in several, or one of them in none.
    Ambiguous(usize),
    /// One repository offers its installed version, and more than one stanza lists it.
    Duplicated(usize),
    /// No enabled repository offers its installed version.
    Offered,
    /// A repository outside the distribution offers it, and no stanza of the files apt reads
    /// sources from lists that repository.
    Unlisted,
}

impl Unattributed {
    pub fn why(&self) -> String {
        match self {
            Unattributed::Ambiguous(count) => format!(
                "ambiguous: {count} repositories offer the installed version, so none is guessed"
            ),
            Unattributed::Duplicated(count) => format!(
                "ambiguous: {count} stanzas list the one repository that offers the installed \
                 version, so none is guessed"
            ),
            Unattributed::Offered => "unattributed: no enabled repository offers the installed \
                                      version (a package file, or a repository since removed)"
                .into(),
            Unattributed::Unlisted => "unattributed: the repository that offers it is listed by \
                                       no stanza of /etc/apt/sources.list or sources.list.d"
                .into(),
        }
    }
}

/// Everything one import read about a machine's repositories.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Repositories {
    /// Sorted by name.
    pub carried: Vec<Carried>,
    /// Sorted by path, then reason.
    pub refused: Vec<Refused>,
    /// Stanzas of the distribution's own repositories: read, counted, left alone.
    pub distribution: usize,
    /// Names from outside the distribution attributed to no stanza, sorted by name.
    pub unattributed: Vec<(String, Unattributed)>,
    /// Files under `/etc/apt/auth.conf.d/` and `/etc/apt/auth.conf`, by path. Never opened.
    pub credentials: Vec<String>,
    /// Whether the family was read at all. `false` on Arch.
    pub read: bool,
    /// Whether the machine has a package index to attribute by. `false` on an apt machine
    /// whose index has never been fetched, where every stanza is left unclassified.
    pub indexed: bool,
}

impl Repositories {
    /// The capture with each carried keyring added, at the path its block names: what the
    /// import hands the landing, so that the keyrings are written — and kept by `--force` — the
    /// way a captured file is. The emitter has already written the manifest from the capture
    /// alone, so no keyring becomes a `[files]` entry.
    pub fn bundle(&self, capture: &Capture) -> Capture {
        let mut bundle = capture.clone();
        for repo in &self.carried {
            bundle.captured.push(Captured {
                path: repo.keyring_path.clone(),
                source: repo.source(),
                mode: 0o644,
                owner: "root".into(),
                group: "root".into(),
                bytes: repo.keyring.clone(),
            });
        }
        bundle
    }
}

/// Whether one source line of `apt-cache policy` is one of this entry's: its URI (after the one
/// normalization, [`normalize_uri`]) and its `SUITE/COMPONENT` — or, for a flat repository, its
/// suite.
fn lists(entry: &Entry, source: &str) -> bool {
    let mut words = source.split_whitespace();
    let (Some(uri), Some(dist)) = (words.next(), words.next()) else {
        return false;
    };
    let uri = normalize_uri(uri);
    for own in &entry.uris {
        let own = normalize_uri(own);
        for suite in &entry.suites {
            if suite.ends_with('/') {
                // A flat repository: apt names it by the URI joined with the suite, or by the
                // URI and the suite as written (unverified on a guest; LD-367).
                let joined = normalize_uri(&format!("{own}/{}", suite.trim_end_matches('/')));
                if uri == joined || (uri == own && dist == suite.as_str()) {
                    return true;
                }
                continue;
            }
            if uri == own
                && entry
                    .components
                    .iter()
                    .any(|component| dist == format!("{suite}/{component}"))
            {
                return true;
            }
        }
    }
    false
}

/// The distinct repositories among a name's source lines, each as its URI (after
/// [`normalize_uri`]) and its `SUITE/COMPONENT`, or a flat repository's suite. The architecture
/// and index words after them are not part of it: one repository printed once per architecture
/// is still one repository.
fn repositories(lines: &[String]) -> BTreeSet<(String, String)> {
    lines
        .iter()
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            Some((normalize_uri(words.next()?), words.next()?.to_string()))
        })
        .collect()
}

/// Read the repositories of the machine the gate opened, attributing the installed names whose
/// origin is outside the distribution by the backend's own `apt-cache policy` reading.
pub fn read(
    gate: &Gate,
    backend: &dyn pm::Backend,
    origins: &BTreeMap<String, Origin>,
) -> Result<Repositories, Diagnostic> {
    let mut out = Repositories::default();
    if gate.distro == Distro::Fedora {
        return read_dnf(gate, origins);
    }
    if gate.distro == Distro::Arch {
        return Ok(out);
    }
    out.read = true;
    let outside: Vec<String> = origins
        .iter()
        .filter(|(_, origin)| matches!(origin, Origin::ThirdParty(_)))
        .map(|(name, _)| name.clone())
        .collect();
    let local: Vec<String> = origins
        .iter()
        .filter(|(_, origin)| **origin == Origin::LocalOnly)
        .map(|(name, _)| name.clone())
        .collect();
    let offers = backend.offers(&outside)?;
    out.indexed = !offers.origin_of.is_empty();

    let mut stanzas: Vec<(String, Result<Entry, Reason>)> = Vec::new();
    match sourceset::read_entries(&gate.root, &|_| false) {
        Ok(files) => {
            for (path, entries) in files {
                match entries {
                    Ok(entries) => {
                        stanzas.extend(entries.into_iter().map(|entry| (path.clone(), Ok(entry))));
                    }
                    Err(error) => stanzas.push((path, Err(malformed(&error)))),
                }
            }
        }
        Err(error) => stanzas.push((
            "/etc/apt/sources.list.d".to_string(),
            Err(malformed(&error)),
        )),
    }
    // A stanza that reaches its archive through a local mirror list (`mirror+file:`, as
    // Debian's cloud image does) also lists the addresses that list names, and the list's URI
    // in the spelling `apt-cache policy` prints (`mirror+file:/PATH`).
    let entries: Vec<Entry> = stanzas
        .iter()
        .filter_map(|(_, stanza)| stanza.as_ref().ok().cloned())
        .collect();
    let mirrors = sourceset::read_mirror_lists(&gate.root, &entries).unwrap_or_default();
    for (_, stanza) in &mut stanzas {
        let Ok(stanza) = stanza else {
            continue;
        };
        let mut more = Vec::new();
        for uri in &stanza.uris {
            if let Some(listed) = mirrors.get(uri) {
                let path = uri
                    .trim_start_matches("mirror+file:")
                    .trim_start_matches("//");
                more.push(format!("mirror+file:{path}"));
                more.extend(listed.iter().cloned());
            }
        }
        for uri in more {
            if !stanza.uris.contains(&uri) {
                stanza.uris.push(uri);
            }
        }
    }

    // Attribution: each name outside the distribution to the one stanza that lists the one
    // repository offering its installed version, or to none. The repositories are counted first
    // and the stanzas only after: two repositories are ambiguous whether one stanza lists both,
    // two stanzas list one each, or no stanza lists one of them.
    let mut attributed: BTreeMap<usize, Vec<String>> = BTreeMap::new();
    for name in &outside {
        let lines = offers
            .installed_from
            .get(name)
            .map(Vec::as_slice)
            .unwrap_or_default();
        let offering = repositories(lines);
        if offering.len() > 1 {
            out.unattributed
                .push((name.clone(), Unattributed::Ambiguous(offering.len())));
            continue;
        }
        let owners: BTreeSet<usize> = stanzas
            .iter()
            .enumerate()
            .filter(|(_, (_, stanza))| {
                stanza
                    .as_ref()
                    .is_ok_and(|stanza| lines.iter().any(|line| lists(stanza, line)))
            })
            .map(|(index, _)| index)
            .collect();
        match owners.len() {
            0 if lines.is_empty() => out.unattributed.push((name.clone(), Unattributed::Offered)),
            0 => out
                .unattributed
                .push((name.clone(), Unattributed::Unlisted)),
            1 => attributed
                .entry(*owners.iter().next().expect("one owner"))
                .or_default()
                .push(name.clone()),
            many => out
                .unattributed
                .push((name.clone(), Unattributed::Duplicated(many))),
        }
    }
    for name in local {
        out.unattributed.push((name, Unattributed::Offered));
    }
    out.unattributed.sort();

    let mut taken: BTreeMap<String, usize> = BTreeMap::new();
    for (index, (path, stanza)) in stanzas.iter().enumerate() {
        let packages = attributed.remove(&index).unwrap_or_default();
        let stanza = match stanza {
            Ok(stanza) => stanza,
            Err(reason) => {
                out.refused.push(Refused {
                    path: path.clone(),
                    reason: reason.clone(),
                    packages,
                });
                continue;
            }
        };
        if !out.indexed {
            continue;
        }
        if is_distribution(gate.distro, stanza, &offers.origin_of) {
            let named = own_suites(gate.distro, &gate.os.codename, stanza, &offers);
            if named.len() < stanza.suites.len() {
                out.distribution += 1;
            }
            for reason in named {
                match extra_suite(gate, path, stanza, &reason, &packages, &offers) {
                    Some(Ok(mut carried)) => {
                        // A .list spells one suite on a deb and a deb-src line: one block.
                        if let Some(same) = out.carried.iter_mut().find(|other| {
                            other.own
                                && other.path == carried.path
                                && other.uris == carried.uris
                                && other.suites == carried.suites
                                && other.components == carried.components
                        }) {
                            for kind in carried.types {
                                if !same.types.contains(&kind) {
                                    same.types.push(kind);
                                }
                            }
                            continue;
                        }
                        carried.name = unique(&carried.name, &mut taken);
                        out.carried.push(carried);
                    }
                    Some(Err(why)) => out.refused.push(Refused {
                        path: path.clone(),
                        reason: why,
                        packages: Vec::new(),
                    }),
                    None => out.refused.push(Refused {
                        path: path.clone(),
                        reason,
                        packages: Vec::new(),
                    }),
                }
            }
            continue;
        }
        match decide(gate, stanza, &packages, true) {
            Ok((keyring_path, keyring, format)) => {
                let name = unique(&stem_name(path), &mut taken);
                out.carried.push(Carried {
                    name,
                    path: path.clone(),
                    types: stanza.types.clone(),
                    uris: stanza.uris.clone(),
                    suites: stanza.suites.clone(),
                    components: stanza.components.clone(),
                    architectures: stanza.architectures.clone(),
                    keyring_path,
                    sha256: sha256_hex(&keyring),
                    keyring,
                    format,
                    packages,
                    family: Family::Apt,
                    own: false,
                });
            }
            Err(reason) => out.refused.push(Refused {
                path: path.clone(),
                reason,
                packages,
            }),
        }
    }
    out.carried.sort_by(|a, b| a.name.cmp(&b.name));
    out.refused.sort_by(|a, b| {
        a.path
            .cmp(&b.path)
            .then_with(|| a.reason.why().cmp(&b.reason.why()))
    });
    // A .list may spell the same suite on deb and deb-src lines. Name a refusal once per
    // file and reason, but do not collapse distinct reasons or attributed package lists.
    out.refused
        .dedup_by(|a, b| a.path == b.path && a.reason == b.reason && a.packages == b.packages);
    out.credentials = credential_files(&gate.root);
    Ok(out)
}

/// Whether a dnf repository id is Fedora's own: what the `fedora-repos` files name.
fn is_fedora_own(id: &str) -> bool {
    id == "fedora" || id == "updates" || id.starts_with("fedora-") || id.starts_with("updates-")
}

/// The Fedora half of [`read`]: the enabled sections of `/etc/yum.repos.d/*.repo`.
fn read_dnf(gate: &Gate, origins: &BTreeMap<String, Origin>) -> Result<Repositories, Diagnostic> {
    let mut out = Repositories {
        read: true,
        indexed: !origins.values().any(|origin| *origin == Origin::Unchecked),
        ..Repositories::default()
    };
    if !out.indexed {
        return Ok(out);
    }
    let mut attributed: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (name, origin) in origins {
        match origin {
            Origin::ThirdParty(repo) => attributed
                .entry(repo.clone())
                .or_default()
                .push(name.clone()),
            Origin::LocalOnly => out.unattributed.push((name.clone(), Unattributed::Offered)),
            _ => {}
        }
    }
    let mut taken: BTreeMap<String, usize> = BTreeMap::new();
    for (path, repos) in reposet::read_files(&gate.root, &|_| false)? {
        let repos = match repos {
            Ok(repos) => repos,
            Err(error) => {
                let why = error
                    .message
                    .split_once(": lodi cannot read this dnf repository file: ")
                    .map_or(error.message.clone(), |(_, why)| why.to_string());
                out.refused.push(Refused {
                    path,
                    reason: Reason::Dnf(format!("it could not be read as a dnf repository: {why}")),
                    packages: Vec::new(),
                });
                continue;
            }
        };
        for repo in repos.into_iter().filter(|repo| repo.enabled) {
            if is_fedora_own(&repo.id) {
                out.distribution += 1;
                continue;
            }
            let packages = attributed.remove(&repo.id).unwrap_or_default();
            let shown = format!("{path}, repository {}", repo.id);
            match decide_dnf(gate, &repo, &packages) {
                Ok((keyring_path, keyring)) => out.carried.push(Carried {
                    name: unique(&source_name(&repo.id), &mut taken),
                    path: shown,
                    types: Vec::new(),
                    uris: repo.baseurls.clone(),
                    suites: Vec::new(),
                    components: Vec::new(),
                    architectures: Vec::new(),
                    keyring_path,
                    sha256: sha256_hex(&keyring),
                    keyring,
                    format: Format::Armored,
                    packages,
                    family: Family::Dnf,
                    own: false,
                }),
                Err(why) => out.refused.push(Refused {
                    path: shown,
                    reason: Reason::Dnf(why),
                    packages,
                }),
            }
        }
    }
    // A repository dnf offers the installed build from, that no enabled section names.
    for (_, names) in attributed {
        out.unattributed
            .extend(names.into_iter().map(|name| (name, Unattributed::Unlisted)));
    }
    out.unattributed.sort();
    out.carried.sort_by(|a, b| a.name.cmp(&b.name));
    out.refused.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// What a third-party dnf repository's refusal is, in words, or the key it is carried with.
fn decide_dnf(
    gate: &Gate,
    repo: &reposet::Repo,
    packages: &[String],
) -> Result<(String, Vec<u8>), String> {
    if repo.baseurls.is_empty() {
        return Err(if repo.mirrored {
            "it names a metalink or mirrorlist and no baseurl, which lodi does not arm".into()
        } else {
            "it names no baseurl".into()
        });
    }
    for uri in &repo.baseurls {
        if let Some((_, problem)) = super::super::manifest::uri_problem(uri) {
            return Err(if problem.starts_with("is not an https://") {
                "its baseurl is not https://, which every source lodi arms is".into()
            } else {
                format!("its baseurl {problem}")
            });
        }
    }
    if repo.gpgcheck != Some(true) {
        return Err(
            "it does not switch gpgcheck on, and lodi arms no repository whose packages go              unchecked"
                .into(),
        );
    }
    let [key] = repo.gpgkeys.as_slice() else {
        return Err(format!(
            "it names {} gpgkey values, and a [sources] block carries exactly one key",
            repo.gpgkeys.len()
        ));
    };
    let Some(path) = key.strip_prefix("file://") else {
        return Err(
            "its gpgkey is not a file on this machine; download that key, check it, and              declare it by hand"
                .into(),
        );
    };
    let keyring = read_keyring(&gate.root, path).map_err(|why| format!("its gpgkey {why}"))?;
    match declared::sniff(&keyring) {
        Ok(Format::Armored) => {}
        Ok(Format::Binary) => return Err("its gpgkey is not an ASCII-armored key".into()),
        Err(refusal) => return Err(format!("its gpgkey {}", refusal.text())),
    }
    if packages.is_empty() {
        return Err("no installed package came from it".into());
    }
    Ok((path.to_string(), keyring))
}

/// The suites of the distribution's own archive a fresh install of the release enables, after
/// its codename (LD-414). Backports is not one of them on either distribution (si-1).
pub const DEFAULT_SUITES: &[(Distro, &[&str])] = &[
    (Distro::Debian, &["", "-updates", "-security"]),
    (Distro::Ubuntu, &["", "-updates", "-security"]),
];

/// The suites of a stanza of the distribution's own that a fresh install of the release does not
/// enable, as the stanza writes them: named under `NOT CAPTURED`, never counted (LD-414).
/// A mixed stanza can also hold a suite with a different Release origin; name that with its
/// own truthful reason rather than silently counting it with the default suite (R8).
fn own_suites(distro: Distro, codename: &str, stanza: &Entry, offers: &pm::Offers) -> Vec<Reason> {
    let Some((_, defaults)) = DEFAULT_SUITES.iter().find(|(d, _)| *d == distro) else {
        return Vec::new();
    };
    if codename.is_empty() {
        return Vec::new();
    }
    let default = |suite: &str| {
        suite
            .strip_prefix(codename)
            .is_some_and(|rest| defaults.contains(&rest))
    };
    let mut out = Vec::new();
    for suite in &stanza.suites {
        let one = Entry {
            suites: vec![suite.clone()],
            ..stanza.clone()
        };
        let lines: Vec<&String> = offers
            .origin_of
            .keys()
            .filter(|source| lists(&one, source))
            .collect();
        let foreign = lines
            .iter()
            .any(|source| !offers.origin_of[*source].eq_ignore_ascii_case(distro.name()));
        let alias = distro == Distro::Debian
            && lines.iter().any(|source| {
                offers
                    .codename_of
                    .get(*source)
                    .is_some_and(|name| default(name))
            });
        if !alias && !default(suite) {
            out.push(if foreign {
                Reason::MixedSuite(suite.clone())
            } else {
                Reason::OwnSuite(suite.clone())
            });
        }
    }
    out
}

/// The reader's refusal of a file, in its own words, without the path it already names.
fn malformed(error: &Diagnostic) -> Reason {
    let why = error
        .message
        .split_once(": lodi cannot read this apt source file: ")
        .map_or(error.message.as_str(), |(_, why)| why);
    Reason::Malformed(why.to_string())
}

/// The credential files apt keeps, by path. Their contents are never opened, and nothing is
/// listed through a link: with `/etc` or `/etc/apt` a link (or not a directory) there is nothing
/// to list — the reader has already named that link — and a linked `auth.conf.d` is named by its
/// own path, never listed.
fn credential_files(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for above in ["etc", "etc/apt"] {
        match fs::symlink_metadata(root.join(above)) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
            _ => return out,
        }
    }
    let etc = root.join("etc/apt");
    if fs::symlink_metadata(etc.join("auth.conf")).is_ok() {
        out.push("/etc/apt/auth.conf".into());
    }
    let dir = etc.join("auth.conf.d");
    match fs::symlink_metadata(&dir) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            if let Ok(entries) = fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    out.push(format!(
                        "/etc/apt/auth.conf.d/{}",
                        entry.file_name().to_string_lossy()
                    ));
                }
            }
        }
        Ok(_) => out.push("/etc/apt/auth.conf.d".into()),
        Err(_) => {}
    }
    out.sort();
    out
}

/// A stanza is the distribution's own when the `Release` file of a source it lists names the
/// distribution as its origin (`o=Debian`, `o=Ubuntu`): the same test R-2 declares a package's
/// origin by, so a mirror anywhere is the distribution's own and a look-alike host is not.
fn is_distribution(distro: Distro, stanza: &Entry, origin_of: &BTreeMap<String, String>) -> bool {
    origin_of.iter().any(|(source, origin)| {
        (origin.eq_ignore_ascii_case(distro.name())
            || (distro == Distro::Debian && origin == DEBIAN_BACKPORTS))
            && lists(stanza, source)
    })
}

/// The `Release` origin of Debian's backports suite: Debian's own archive, published apart.
const DEBIAN_BACKPORTS: &str = "Debian Backports";

/// The suites of the distribution's own archive an import writes as a `[sources]` block when a
/// machine enables one, after the codename (si-1, S5).
pub const EXTRA_SUITES: &[(Distro, &[&str])] = &[
    (Distro::Debian, &["-backports"]),
    (Distro::Ubuntu, &["-backports", "-proposed"]),
];

/// The distribution's own archive key, for a stanza that names none.
const ARCHIVE_KEYRINGS: &[(Distro, &str)] = &[
    (
        Distro::Debian,
        "/usr/share/keyrings/debian-archive-keyring.gpg",
    ),
    (
        Distro::Ubuntu,
        "/usr/share/keyrings/ubuntu-archive-keyring.gpg",
    ),
];

/// One extra suite of the distribution's own archive as a block: `None` when the suite is not
/// one [`EXTRA_SUITES`] names, and the refusal when its stanza cannot be one. Its URI is taken
/// over `https://`, the one scheme a `[sources]` block arms, and its keyring is the one the
/// stanza names, or the distribution's own archive key.
fn extra_suite(
    gate: &Gate,
    path: &str,
    stanza: &Entry,
    reason: &Reason,
    attributed: &[String],
    offers: &pm::Offers,
) -> Option<Result<Carried, Reason>> {
    let (Reason::OwnSuite(suite) | Reason::MixedSuite(suite)) = reason else {
        return None;
    };
    let (_, extra) = EXTRA_SUITES.iter().find(|(d, _)| *d == gate.distro)?;
    let rest = suite.strip_prefix(gate.os.codename.as_str())?;
    if gate.os.codename.is_empty() || !extra.contains(&rest) {
        return None;
    }
    let keyring = ARCHIVE_KEYRINGS
        .iter()
        .find(|(d, _)| *d == gate.distro)
        .map(|(_, path)| (*path).to_string());
    // The addresses a mirror list names stand for it: a block names an archive by its own URI.
    let direct: Vec<&String> = stanza
        .uris
        .iter()
        .filter(|uri| !uri.starts_with("mirror+"))
        .collect();
    let one = Entry {
        uris: direct
            .into_iter()
            .map(|uri| match uri.strip_prefix("http://") {
                Some(rest) => format!("https://{rest}"),
                None => uri.clone(),
            })
            .fold(Vec::new(), |mut uris, uri| {
                if !uris.contains(&uri) {
                    uris.push(uri);
                }
                uris
            }),
        suites: vec![suite.clone()],
        signed_by: stanza.signed_by.clone().or(keyring),
        ..stanza.clone()
    };
    let listed = Entry {
        suites: vec![suite.clone()],
        ..stanza.clone()
    };
    let packages: Vec<String> = attributed
        .iter()
        .filter(|name| {
            offers
                .installed_from
                .get(*name)
                .is_some_and(|lines| lines.iter().any(|line| lists(&listed, line)))
        })
        .cloned()
        .collect();
    Some(
        decide(gate, &one, &packages, false).map(|(keyring_path, keyring, format)| Carried {
            name: rest.trim_start_matches('-').to_string(),
            path: path.to_string(),
            types: one.types.clone(),
            uris: one.uris.clone(),
            suites: one.suites.clone(),
            components: one.components.clone(),
            architectures: one.architectures.clone(),
            keyring_path,
            sha256: sha256_hex(&keyring),
            keyring,
            format,
            packages,
            family: Family::Apt,
            own: true,
        }),
    )
}

/// What a third-party stanza's refusal is, or the keyring it is carried with.
fn decide(
    gate: &Gate,
    stanza: &Entry,
    packages: &[String],
    need_packages: bool,
) -> Result<(String, Vec<u8>, Format), Reason> {
    for uri in &stanza.uris {
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Err(Reason::NotHttps);
        };
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if authority.contains('@') {
            return Err(Reason::Credentials);
        }
        if !scheme.eq_ignore_ascii_case("https") {
            return Err(Reason::NotHttps);
        }
        if rest.contains('#') {
            return Err(Reason::Fragment);
        }
        if rest.contains('?') {
            return Err(Reason::Query);
        }
    }
    if stanza.trusted {
        return Err(Reason::Trusted);
    }
    if stanza.inline_key {
        return Err(Reason::SignedByNotAFile("an inline key block".into()));
    }
    let Some(signed_by) = &stanza.signed_by else {
        return Err(Reason::NoSignedBy);
    };
    let paths: Vec<&str> = signed_by
        .split([' ', ','])
        .filter(|s| !s.is_empty())
        .collect();
    if paths.len() != 1 {
        return Err(Reason::SignedByNotAFile("more than one path".into()));
    }
    let signed_by = paths[0];
    if !signed_by.starts_with('/') {
        return Err(Reason::SignedByNotAFile(
            "not an absolute path (a fingerprint names no file)".into(),
        ));
    }
    let keyring = read_keyring(&gate.root, signed_by).map_err(Reason::SignedByNotAFile)?;
    let format = declared::sniff(&keyring).map_err(|refusal| match refusal {
        Refusal::Secret => Reason::SecretKey,
        Refusal::NotKeyring => Reason::NotAKeyring,
    })?;
    if need_packages && packages.is_empty() {
        return Err(Reason::NothingInstalled);
    }
    Ok((signed_by.to_string(), keyring, format))
}

/// The keyring a `Signed-By` path names, read below the root the way the one apt source reader
/// reads a source file: every component looked at without following it, so a link anywhere on
/// the way — at `/etc`, at `/etc/apt`, at a directory under `keyrings/`, or the file itself — is
/// refused by its in-root name before its target is read; then opened with `O_NOFOLLOW` and read
/// no further than [`MAX_KEYRING_BYTES`] and one byte. The refusal is in-root words only: no
/// path of the machine the root sits on reaches the emitted file.
fn read_keyring(root: &Path, signed_by: &str) -> Result<Vec<u8>, String> {
    let relative = Path::new(signed_by.trim_start_matches('/'));
    let parts: Vec<Component> = relative.components().collect();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("not a plain path (it has `.` or `..`)".into());
    }
    let mut real = root.to_path_buf();
    let mut shown = String::new();
    for (index, part) in parts.iter().enumerate() {
        real.push(part.as_os_str());
        shown.push('/');
        shown.push_str(&part.as_os_str().to_string_lossy());
        let last = index + 1 == parts.len();
        let meta = match fs::symlink_metadata(&real) {
            Ok(meta) => meta,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err("it is not there".into());
            }
            Err(error) => return Err(error.kind().to_string()),
        };
        if meta.file_type().is_symlink() {
            return Err(if last {
                "a symbolic link".into()
            } else {
                format!("{shown} on the way to it is a symbolic link, which lodi does not follow")
            });
        }
        if !last && !meta.is_dir() {
            return Err(format!("{shown} on the way to it is not a directory"));
        }
        if last && !meta.is_file() {
            return Err("not a regular file".into());
        }
    }
    let too_large = || format!("larger than {} KiB", MAX_KEYRING_BYTES / 1024);
    let handle = super::super::files::open_regular(&real, signed_by)
        .map_err(|_| "not a regular file".to_string())?;
    let mut keyring = Vec::new();
    handle
        .take(MAX_KEYRING_BYTES as u64 + 1)
        .read_to_end(&mut keyring)
        .map_err(|error| error.kind().to_string())?;
    if keyring.len() > MAX_KEYRING_BYTES {
        return Err(too_large());
    }
    Ok(keyring)
}

/// The `[sources.NAME]` a file stem becomes: lower case, `[a-z0-9-]`, no `lodi-` prefix.
fn stem_name(path: &str) -> String {
    let file = Path::new(path);
    let stem = if file.file_name().is_some_and(|n| n == "sources.list") {
        "main".to_string()
    } else {
        file.file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default()
    };
    source_name(&stem)
}

/// The `[sources.NAME]` a stem or a dnf repository id becomes.
fn source_name(stem: &str) -> String {
    let stem = stem.to_lowercase();
    let mut name = String::new();
    for c in stem.chars() {
        if c.is_ascii_alphanumeric() {
            name.push(c);
        } else if !name.ends_with('-') && !name.is_empty() {
            name.push('-');
        }
    }
    let name = name.trim_matches('-').to_string();
    let name = name.strip_prefix("lodi-").unwrap_or(&name).to_string();
    let name: String = name.chars().take(60).collect();
    let name = name.trim_matches('-').to_string();
    if name.is_empty() {
        "source".into()
    } else {
        name
    }
}

fn unique(base: &str, taken: &mut BTreeMap<String, usize>) -> String {
    let n = taken.entry(base.to_string()).or_insert(0);
    *n += 1;
    if *n == 1 {
        base.to_string()
    } else {
        format!("{base}-{n}")
    }
}

// ------------------------------------------------------------------------- emission ---

/// The `[sources]` half of the emitted manifest, all of it a comment: one block per carried
/// repository, after `[packages]`, its keys in the order `docs/scopes/host.md` gives them.
/// `bundled` is whether a `files/` bundle travels with this text; with `--stdout` none does,
/// and each block says its keyring was not written.
pub fn emit(out: &mut String, machine: &Machine, bundled: bool) {
    let repos = &machine.repositories;
    if !repos.read || repos.carried.is_empty() {
        return;
    }
    let dnf = repos.carried.iter().any(|repo| repo.family == Family::Dnf);
    if dnf {
        out.push_str("# Third-party dnf repositories");
    } else {
        out.push_str("# The apt repositories beyond the default ones");
    }
    out.push_str(
        " this machine installed from, each written as the\n\
         # [sources] block a switch would turn on. They are commented out, because turning one on\n\
         # writes a trust anchor, and that is a step you take on purpose: remove the `# ` from\n\
         # a block's lines to take it, then add the names it lists to [packages]. The keyring\n\
         # it names travels beside this file under files/, and a switch verifies it against\n\
         # signed_by_sha256 before it writes anything.\n\
         #\n",
    );
    for repo in &repos.carried {
        let _ = writeln!(out, "# [sources.{}]", repo.name);
        let _ = writeln!(out, "# # read from {}", repo.path);
        if repo.family == Family::Dnf {
            emit_dnf(out, repo, bundled);
            continue;
        }
        // Taken on the machine it was read from, the block and that file would list one
        // repository twice, which the plan refuses (S7, `E_DUP_RESOURCE`).
        if repo.own {
            let _ = writeln!(
                out,
                "# # {} of the distribution's own archive, signed by its own key; on this\n\
                 # # machine that file already lists it: take the suite out of it before you\n\
                 # # take this block here, because apt refuses two Signed-By values for one suite",
                repo.suites.join(" ")
            );
        } else {
            out.push_str(
                "# # on this machine that file already lists it: disable that stanza before you\n\
                 # # take this block here, because apt refuses two Signed-By values for one \
                 repository\n",
            );
        }
        if repo.packages.is_empty() {
            out.push_str("# # nothing installed on this machine came from it\n");
        } else {
            out.push_str(
                "# # installed from it, and not declared under [packages] until you take it:\n",
            );
        }
        for name in &repo.packages {
            let _ = writeln!(out, "# #   {name}");
        }
        let _ = writeln!(
            out,
            "# # a lodi that reads [sources]; when you take this block, [host] needs\n\
             # #   min_lodi_version = \">={SOURCES_MINIMUM}\""
        );
        if !bundled {
            let _ = writeln!(
                out,
                "# # the keyring was not written (--stdout): copy {}\n# # to {} beside this file",
                repo.keyring_path,
                repo.source()
            );
        }
        let _ = writeln!(out, "# types = {}", array(&repo.types));
        let _ = writeln!(out, "# uris = {}", array(&repo.uris));
        let suites: Vec<String> = repo
            .suites
            .iter()
            .map(|suite| portable_suite(suite, &machine.codename))
            .collect();
        let _ = writeln!(out, "# suites = {}", array(&suites));
        if !repo.components.is_empty() {
            let _ = writeln!(out, "# components = {}", array(&repo.components));
        }
        if !repo.architectures.is_empty() {
            let _ = writeln!(out, "# architectures = {}", array(&repo.architectures));
        }
        let _ = writeln!(out, "# signed_by = \"{}\"", repo.source());
        let _ = writeln!(out, "# signed_by_sha256 = \"{}\"\n", repo.sha256);
    }
}

/// The rest of one dnf repository's block, after its name and where it was read (LD-433).
fn emit_dnf(out: &mut String, repo: &Carried, bundled: bool) {
    out.push_str(
        "# # on this machine that file already lists it: disable that repository before you\n\
         # # take this block here, because lodi refuses one repository listed twice\n\
         # # installed from it, and not declared under [packages] until you take it:\n",
    );
    for name in &repo.packages {
        let _ = writeln!(out, "# #   {name}");
    }
    let _ = writeln!(
        out,
        "# # a lodi that reads [sources] on fedora; when you take this block, [host] needs\n\
         # #   min_lodi_version = \">={FEDORA_SOURCES_MINIMUM}\""
    );
    if !bundled {
        let _ = writeln!(
            out,
            "# # the key was not written (--stdout): copy {}\n# # to {} beside this file",
            repo.keyring_path,
            repo.source()
        );
    }
    let _ = writeln!(out, "# uris = {}", array(&repo.uris));
    let _ = writeln!(out, "# signed_by = \"{}\"", repo.source());
    let _ = writeln!(out, "# signed_by_sha256 = \"{}\"\n", repo.sha256);
}

/// The NOT CAPTURED lines for the repositories: what was refused, by path and reason, the names
/// attributed to no block, what was the distribution's own, and the credential files that are
/// never carried.
pub fn not_captured(out: &mut String, machine: &Machine) {
    let repos = &machine.repositories;
    out.push_str("#\n");
    if !repos.read {
        out.push_str(
            "# third-party pacman repositories: not read; lodi arms no pacman repository\n",
        );
        return;
    }
    if !repos.indexed && machine.distro == Distro::Fedora {
        out.push_str(
            "# dnf repositories: not classified, because this machine has no package index to\n\
             # attribute an installed package by; run dnf5 makecache and import again\n",
        );
    } else if !repos.indexed {
        out.push_str(
            "# apt repositories: not classified, because this machine has no package index to\n\
             # attribute an installed package by; run apt-get update and import again\n",
        );
    } else {
        if repos.refused.is_empty() {
            out.push_str("# repositories read and not written as a [sources] block: none\n");
        } else {
            out.push_str("# repositories read and not written as a [sources] block:\n");
            for refusal in &repos.refused {
                let _ = writeln!(out, "#   {}", refusal.path);
                for line in refusal.reason.why().lines() {
                    let _ = writeln!(out, "#     {line}");
                }
                if !refusal.packages.is_empty() {
                    let _ = writeln!(
                        out,
                        "#     installed from it: {}",
                        refusal.packages.join(", ")
                    );
                }
            }
        }
        let _ = writeln!(
            out,
            "# the distribution's own repositories, which every install has: {}",
            repos.distribution
        );
    }
    if repos.unattributed.is_empty() {
        out.push_str("# installed packages attributed to no [sources] block: none\n");
    } else {
        out.push_str("# installed packages attributed to no [sources] block:\n");
        for (name, why) in &repos.unattributed {
            let _ = writeln!(out, "#   {name}");
            let _ = writeln!(out, "#     {}", why.why());
        }
    }
    if !repos.credentials.is_empty() {
        out.push_str("# credential files apt keeps, which lodi never reads or carries:\n");
        for path in &repos.credentials {
            let _ = writeln!(out, "#   {path}");
        }
    }
}

/// A suite that is the release codename, or starts with it, spelled through `${host.codename}`
/// so that the block applies to the release it is applied on.
fn portable_suite(suite: &str, codename: &str) -> String {
    if codename.is_empty() {
        return suite.to_string();
    }
    if suite == codename {
        return "${host.codename}".into();
    }
    match suite.strip_prefix(codename) {
        Some(rest) if rest.starts_with(['-', '/']) => format!("${{host.codename}}{rest}"),
        _ => suite.to_string(),
    }
}

/// A TOML array of basic strings, `"` and `\` escaped.
fn array(items: &[String]) -> String {
    let quoted: Vec<String> = items
        .iter()
        .map(|item| format!("\"{}\"", item.replace('\\', "\\\\").replace('"', "\\\"")))
        .collect();
    format!("[{}]", quoted.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(uris: &[&str], suites: &[&str], components: &[&str]) -> Entry {
        let owned = |items: &[&str]| items.iter().map(|item| item.to_string()).collect();
        Entry {
            uris: owned(uris),
            suites: owned(suites),
            components: owned(components),
            ..Entry::default()
        }
    }

    /// A source line of `apt-cache policy` belongs to a stanza by its URI, after one
    /// normalization, and its `SUITE/COMPONENT`, and to nothing looser.
    #[test]
    fn a_policy_source_line_belongs_to_the_stanza_that_lists_it() {
        let stanza = entry(
            &["https://Download.Example.invalid/linux/debian/"],
            &["bookworm"],
            &["stable", "test"],
        );
        let line = |rest: &str| format!("https://download.example.invalid/linux/{rest}");
        assert!(lists(
            &stanza,
            &line("debian bookworm/stable amd64 Packages")
        ));
        assert!(lists(&stanza, &line("debian bookworm/test amd64 Packages")));
        assert!(!lists(
            &stanza,
            &line("debian bookworm/nightly amd64 Packages")
        ));
        assert!(!lists(
            &stanza,
            &line("ubuntu bookworm/stable amd64 Packages")
        ));
        assert!(!lists(&stanza, "/var/lib/dpkg/status"));
        let flat = entry(&["https://example.invalid/repo"], &["./"], &[]);
        assert!(lists(&flat, "https://example.invalid/repo ./ Packages"));
    }

    #[test]
    fn a_repository_is_counted_once_whatever_architectures_print_it() {
        let lines: Vec<String> = [
            "https://download.docker.com/linux/debian bookworm/stable amd64 Packages",
            "https://download.docker.com/linux/debian/ bookworm/stable all Packages",
            "https://docker-mirror.example.invalid/linux/debian bookworm/stable amd64 Packages",
            "https://download.docker.com/linux/debian trixie/stable amd64 Packages",
        ]
        .iter()
        .map(ToString::to_string)
        .collect();
        assert_eq!(repositories(&lines).len(), 3);
        assert_eq!(repositories(&lines[..2]).len(), 1);
        assert!(repositories(&[]).is_empty());
    }

    #[test]
    fn names_and_suites_are_derived_the_documented_way() {
        assert_eq!(
            stem_name("/etc/apt/sources.list.d/docker.sources"),
            "docker"
        );
        assert_eq!(
            stem_name("/etc/apt/sources.list.d/Some Vendor_x.list"),
            "some-vendor-x"
        );
        assert_eq!(
            stem_name("/etc/apt/sources.list.d/lodi-docker.sources"),
            "docker"
        );
        assert_eq!(stem_name("/etc/apt/sources.list"), "main");
        assert_eq!(portable_suite("bookworm", "bookworm"), "${host.codename}");
        assert_eq!(
            portable_suite("bookworm-backports", "bookworm"),
            "${host.codename}-backports"
        );
        assert_eq!(portable_suite("stable", "bookworm"), "stable");
        assert_eq!(portable_suite("bookwormish", "bookworm"), "bookwormish");
    }

    /// The name a block's keyring travels under is the one the apply writes: core's, by the
    /// keyring's framing (S4).
    #[test]
    fn a_blocks_keyring_travels_under_the_name_the_apply_writes() {
        let carried = |format| Carried {
            name: "docker".into(),
            path: String::new(),
            types: Vec::new(),
            uris: Vec::new(),
            suites: Vec::new(),
            components: Vec::new(),
            architectures: Vec::new(),
            keyring_path: String::new(),
            keyring: Vec::new(),
            format,
            sha256: String::new(),
            packages: Vec::new(),
            family: Family::Apt,
            own: false,
        };
        assert_eq!(
            carried(Format::Armored).source(),
            format!("files{}", declared::keyring_path("docker", Format::Armored))
        );
        assert_eq!(
            carried(Format::Binary).source(),
            "files/etc/apt/keyrings/lodi-docker.gpg"
        );
    }
}
