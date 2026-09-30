//! Declared third-party apt repositories: `[sources.NAME]`, planned as one step each (LD-365).
//!
//! A machine restored from a `host.toml` used to stop at `E_UNKNOWN_PACKAGE` the moment
//! `[packages]` named something only a third-party repository offers, because nothing armed that
//! repository first. A source is now **declared**, with the keyring apt verifies it by, and the
//! apply arms it as its own step, ordered before the index refresh and the transaction that
//! installs from it.
//!
//! One source is two managed files, and nothing more:
//!
//! - `/etc/apt/keyrings/lodi-NAME.gpg`, or `lodi-NAME.asc` when the keyring is ASCII-armored
//!   (S4) — the bytes the manifest ships under its own directory (`signed_by`), written only
//!   after their SHA-256 equals `signed_by_sha256` (S5);
//! - `/etc/apt/sources.list.d/lodi-NAME.sources` — the deb822 stanza [`super::sourceset`]
//!   renders, whose `Signed-By:` names exactly that keyring.
//!
//! Both go through the `[files]` machinery unchanged: observed without following a link, written
//! atomically, recorded in the lock as ordinary file records, drift-checked, and — when the
//! declaration leaves the manifest — removed or restored by the record's `on_remove`, exactly as a
//! departed `[files]` entry is (S10). The `lodi-` prefix under those two directories is this
//! module's namespace: a `[files]` key there is refused at parse time.
//!
//! **Lodi adds no cryptography.** It reads only OpenPGP *framing* (S4) to name the file and to
//! refuse secret key material, verifies no signature and touches no machine-wide trust store. apt
//! verifies the repository's `InRelease` against exactly the keys this entry shipped, through
//! its own `Signed-By` mechanism; `Signed-By` trusts **every** key in the file. There is no
//! `Trusted: yes` and no way to declare one.
//!
//! # A keyring by URL (LD-366)
//!
//! `signed_by_url` names the keyring by an `https://` URL instead of a file beside the manifest.
//! **The plan never fetches**: it reads the file already at the managed path, and when that file
//! has `signed_by_sha256` it is the keyring — no request is made by plan or apply — and otherwise
//! the plan says `keyring: fetch` and stops there. The apply fetches every keyring its plan names
//! so through [`crate::fetch`]'s one entry point — its HTTPS, redirect and retry rules as that
//! module enforces them, under a ceiling of [`MAX_KEYRING_BYTES`] — into memory, checks each one
//! exactly as a keyring by path is checked ([`verify`]: digest, then framing), and only then plans
//! again from those bytes under the apply lock, so the same writer writes it. Any failure ends the
//! apply before the lock, the journal or a file is written. Nothing is cached anywhere else: the
//! verified file at the managed path is the cache.
//!
//! # Fedora (LD-433)
//!
//! On a Fedora root the same step writes `/etc/pki/rpm-gpg/lodi-NAME.asc` — an ASCII-armored key
//! only, because rpm imports no other — and `/etc/yum.repos.d/lodi-NAME.repo`, rendered by
//! [`super::reposet`], with `gpgcheck=1` and `gpgkey` naming exactly that file. The forced refresh
//! is `dnf5 --refresh makecache` limited with `--repo=lodi-NAME` to the repositories that forced
//! it, and a source leaving the manifest forces none, because dnf keeps no index of a repository
//! it no longer reads. A repository another `.repo` file of the machine already lists by the same
//! `baseurl` is `E_DUP_RESOURCE`. Fedora's own files are never written: only `lodi-` names are
//! this module's.
//!
//! **What an apply writes here is a trust anchor.** The owner approved the apply's writing it on
//! 2026-09-22 (LD-365): HTTPS only, the digest checked first, every refusal by name, Arch
//! comment-only. Versions from a declared repository still float with it until M-Pin.

use std::collections::{BTreeMap, BTreeSet};

use std::io::Read as _;

use crate::diag::Diagnostic;
use crate::fetch::{FetchError, Fetcher};
use crate::util::sha256_hex;

use super::lock::HostLock;
use super::manifest::{FileEntry, FileState, HostManifest, OnRemove, SourceEntry};
use super::plan::{Action, Content, Desired, Facts, FileAction, Kind, Op, Plan};
use super::pm::Invocation;
use super::safety::Gate;

/// Where apt reads deb822 source stanzas from.
pub const SOURCES_DIR: &str = "/etc/apt/sources.list.d";
/// Where a keyring named by `Signed-By:` lives.
pub const KEYRINGS_DIR: &str = "/etc/apt/keyrings";
/// Where the key a dnf `.repo` file's `gpgkey` names lives (LD-433).
pub const RPM_KEYS_DIR: &str = "/etc/pki/rpm-gpg";
/// The file-name prefix that makes a path under those two directories this module's own.
const PREFIX: &str = "lodi-";
/// The most bytes a keyring may have.
pub const MAX_KEYRING_BYTES: usize = 1024 * 1024;
/// The option the forced refresh adds so that apt fails on a warning as well as an error, where
/// the machine's apt knows it (S9). An apt that does not know an `-o` key ignores it; the output
/// is read for the declared URIs either way. Its availability on each guest is checked by the
/// release run, not here.
pub const ERROR_MODE: &str = "APT::Update::Error-Mode=any";

/// How a keyring is framed, which decides its file name (S4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Binary OpenPGP packets: `lodi-NAME.gpg`.
    Binary,
    /// An ASCII-armored public key block: `lodi-NAME.asc`.
    Armored,
}

impl Format {
    pub fn extension(self) -> &'static str {
        match self {
            Format::Binary => "gpg",
            Format::Armored => "asc",
        }
    }

    fn other(self) -> Format {
        match self {
            Format::Binary => Format::Armored,
            Format::Armored => Format::Binary,
        }
    }
}

/// Which package manager a source is armed for: apt on Debian and Ubuntu, dnf on Fedora.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Family {
    Apt,
    Dnf,
}

impl Family {
    pub fn of(distro: super::safety::Distro) -> Family {
        if distro == super::safety::Distro::Fedora {
            Family::Dnf
        } else {
            Family::Apt
        }
    }

    /// The file a source's repository is written to.
    pub fn stanza_path(self, name: &str) -> String {
        match self {
            Family::Apt => sources_path(name),
            Family::Dnf => format!("{}/{PREFIX}{name}.repo", super::reposet::REPOS_DIR),
        }
    }

    /// The file a source's key is written to.
    pub fn key_path(self, name: &str, format: Format) -> String {
        match self {
            Family::Apt => keyring_path(name, format),
            Family::Dnf => format!("{RPM_KEYS_DIR}/{PREFIX}{name}.asc"),
        }
    }

    /// What a plan line calls the key and the repository file.
    fn labels(self) -> (&'static str, &'static str) {
        match self {
            Family::Apt => ("keyring", "sources"),
            Family::Dnf => ("key", "repo"),
        }
    }
}

/// `/etc/apt/sources.list.d/lodi-NAME.sources`.
pub fn sources_path(name: &str) -> String {
    format!("{SOURCES_DIR}/{}", super::sourceset::file_name(name))
}

/// `/etc/apt/keyrings/lodi-NAME.gpg` or `.asc`.
pub fn keyring_path(name: &str, format: Format) -> String {
    format!("{KEYRINGS_DIR}/{PREFIX}{name}.{}", format.extension())
}

/// The source a path belongs to, when it is one of the paths this module writes for a name.
pub fn name_of(family: Family, path: &str) -> Option<String> {
    let candidate = |dir: &str, suffix: &str| -> Option<String> {
        let name = path
            .strip_prefix(dir)?
            .strip_prefix('/')?
            .strip_prefix(PREFIX)?
            .strip_suffix(suffix)?;
        super::manifest::is_source_name(name).then(|| name.to_string())
    };
    match family {
        Family::Apt => candidate(SOURCES_DIR, ".sources")
            .or_else(|| candidate(KEYRINGS_DIR, ".gpg"))
            .or_else(|| candidate(KEYRINGS_DIR, ".asc")),
        Family::Dnf => candidate(super::reposet::REPOS_DIR, ".repo")
            .or_else(|| candidate(RPM_KEYS_DIR, ".asc")),
    }
}

/// The name a path under this module's namespace would be for — any file directly under one of
/// the two directories whose name starts with `lodi-` — whether or not it is one this module
/// writes. The name is what precedes the first `.`.
pub fn namespace_name(path: &str) -> Option<String> {
    let file = [SOURCES_DIR, KEYRINGS_DIR]
        .iter()
        .find_map(|dir| path.strip_prefix(dir)?.strip_prefix('/'))?;
    if file.contains('/') {
        return None;
    }
    let rest = file.strip_prefix(PREFIX)?;
    Some(rest.split('.').next().unwrap_or_default().to_string())
}

// ------------------------------------------------------------------------------ S4 framing ---

const ARMOR_PUBLIC: &str = "-----BEGIN PGP PUBLIC KEY BLOCK-----";
const ARMOR_PRIVATE: &str = "-----BEGIN PGP PRIVATE KEY BLOCK-----";

/// Why bytes are not a keyring this module writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A secret-key or secret-subkey packet, or an armored private key block.
    Secret,
    /// Anything else that is not a public keyring: empty, text, HTML, broken framing.
    NotKeyring,
}

impl Refusal {
    pub fn text(self) -> &'static str {
        match self {
            Refusal::Secret => "holds secret key material",
            Refusal::NotKeyring => "is not an OpenPGP public keyring",
        }
    }
}

/// Read the OpenPGP framing of a keyring — only its framing: no cryptography, no signature
/// check (S4). No input holds a private key block's marker anywhere, not even mid-line. An
/// armored file starts with a public key block; a binary file is walked packet by packet over
/// its whole length (old- and new-format headers and lengths only), starts with a public-key packet (tag 6), holds no secret-key or
/// secret-subkey packet (tags 5, 7), and ends exactly where its last packet does. Several public
/// keys in one file are accepted.
pub fn sniff(bytes: &[u8]) -> Result<Format, Refusal> {
    // The private block's marker anywhere in the bytes — mid-line, after other text, in a file
    // that is not UTF-8 or not armored at all — refuses the file before anything else is read.
    if bytes
        .windows(ARMOR_PRIVATE.len())
        .any(|window| window == ARMOR_PRIVATE.as_bytes())
    {
        return Err(Refusal::Secret);
    }
    if bytes.starts_with(b"-----") {
        let text = std::str::from_utf8(bytes).map_err(|_| Refusal::NotKeyring)?;
        if text.lines().map(str::trim_end).next() != Some(ARMOR_PUBLIC) {
            return Err(Refusal::NotKeyring);
        }
        return Ok(Format::Armored);
    }
    let mut at = 0usize;
    let mut first = true;
    while at < bytes.len() {
        let (tag, header, length) = packet(&bytes[at..]).ok_or(Refusal::NotKeyring)?;
        if tag == 5 || tag == 7 {
            return Err(Refusal::Secret);
        }
        if first && tag != 6 {
            return Err(Refusal::NotKeyring);
        }
        first = false;
        at = at
            .checked_add(header)
            .and_then(|at| at.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or(Refusal::NotKeyring)?;
    }
    if first {
        return Err(Refusal::NotKeyring);
    }
    Ok(Format::Binary)
}

/// One packet header: its tag, the header's length and the body's length. `None` for a header
/// that is not one, or a length this reader does not walk (indeterminate, or partial).
fn packet(bytes: &[u8]) -> Option<(u8, usize, usize)> {
    let first = *bytes.first()?;
    if first & 0x80 == 0 {
        return None;
    }
    if first & 0x40 != 0 {
        let tag = first & 0x3f;
        let l1 = usize::from(*bytes.get(1)?);
        return match l1 {
            0..=191 => Some((tag, 2, l1)),
            192..=223 => {
                let l2 = usize::from(*bytes.get(2)?);
                Some((tag, 3, ((l1 - 192) << 8) + l2 + 192))
            }
            255 => {
                let b = bytes.get(2..6)?;
                let length = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
                Some((tag, 6, usize::try_from(length).ok()?))
            }
            _ => None,
        }
        .filter(|(tag, _, _)| *tag != 0);
    }
    let tag = (first >> 2) & 0x0f;
    if tag == 0 {
        return None;
    }
    match first & 0x03 {
        0 => Some((tag, 2, usize::from(*bytes.get(1)?))),
        1 => {
            let b = bytes.get(1..3)?;
            Some((tag, 3, usize::from(u16::from_be_bytes([b[0], b[1]]))))
        }
        2 => {
            let b = bytes.get(1..5)?;
            let length = u32::from_be_bytes([b[0], b[1], b[2], b[3]]);
            Some((tag, 5, usize::try_from(length).ok()?))
        }
        _ => None,
    }
}

// ------------------------------------------------------------------------------- reading ---

/// Read every declared keyring below the host, once, beside the `[files]` sources (LD-379): the
/// plan and the re-plan under the apply lock act on exactly these bytes.
pub fn read_keyrings(
    host: &super::source::Host,
    manifest: &HostManifest,
    into: &mut BTreeMap<String, Vec<u8>>,
) -> Result<(), Diagnostic> {
    for source in manifest.sources.values() {
        // A keyring by URL is never beside the manifest ([`read_installed`], [`fetch`]).
        if source.signed_by_url.is_some() || into.contains_key(&source.signed_by) {
            continue;
        }
        let bytes = super::source::read_file(host, &source.signed_by, MAX_KEYRING_BYTES)?
            .ok_or_else(|| {
                Diagnostic::new(
                    "E_STORE_IO",
                    format!(
                        "[sources.{}]: signed_by {:?} is not there in {}",
                        source.name,
                        source.signed_by,
                        host.dir.display()
                    ),
                )
                .hint("put the keyring file there, below the manifest's own directory")
            })?;
        if bytes.len() > MAX_KEYRING_BYTES {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!(
                    "[sources.{}]: signed_by {:?} is over 1 MiB, which no keyring is",
                    source.name, source.signed_by
                ),
            ));
        }
        into.insert(source.signed_by.clone(), bytes);
    }
    Ok(())
}

/// Check one declared keyring: its digest first (S5), then its framing (S4). Returns its format.
/// Refused bytes are never copied or printed.
pub fn verify(source: &SourceEntry, bytes: &[u8]) -> Result<Format, Diagnostic> {
    // A keyring by URL is named by its URL, which passed the manifest's check (LD-366).
    let shown = keyring_key(source);
    let actual = sha256_hex(bytes);
    if actual != source.signed_by_sha256 {
        let hint = match &source.signed_by_url {
            None => format!(
                "nothing was written. A vendor that rotated its signing key shows up exactly this \
                 way: check the new key with the vendor, then put it at `{}` and its SHA-256 in \
                 `signed_by_sha256`",
                source.signed_by
            ),
            Some(_) => "nothing was written. A vendor that rotated its signing key shows up \
                        exactly this way: fetch the new key yourself, check it with the vendor, \
                        then put its SHA-256 in `signed_by_sha256`"
                .to_string(),
        };
        return Err(Diagnostic::new(
            "E_HASH_MISMATCH",
            format!(
                "[sources.{}]: the keyring {shown} has sha256:{actual}, and the manifest declares \
                 sha256:{}",
                source.name, source.signed_by_sha256
            ),
        )
        .hint(hint));
    }
    sniff(bytes).map_err(|refusal| {
        Diagnostic::new(
            "E_TYPE",
            format!(
                "[sources.{}]: the keyring {shown} {}",
                source.name,
                refusal.text()
            ),
        )
        .hint(match refusal {
            Refusal::Secret => "a keyring for apt holds public keys only; export the public key \
                                (`gpg --export`) and never ship a secret key in a manifest"
                .to_string(),
            Refusal::NotKeyring => format!(
                "`{}` names an OpenPGP public keyring: binary packets, or a file that starts \
                 with -----BEGIN PGP PUBLIC KEY BLOCK-----",
                if source.signed_by_url.is_some() {
                    "signed_by_url"
                } else {
                    "signed_by"
                }
            ),
        })
    })
}

// ------------------------------------------------------------------------------ planning ---

/// One source step: its files, in the order it performs them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceAction {
    pub name: String,
    /// What the step does as a whole, for the printed verb.
    pub op: Op,
    /// Keyring before stanza when arming, stanza before keyring when removing, and a keyring
    /// whose format changed as new keyring, stanza, old keyring — so apt never sees a stanza
    /// whose `Signed-By:` names a keyring that is not there (S8, S10).
    pub files: Vec<FileAction>,
    /// The parenthesised part of the printed line.
    pub changes: Vec<String>,
    /// The `signed_by_url` this step needs fetched before it can be planned file by file: set
    /// only by a plan, which never fetches, and never by the plan an apply performs (LD-366).
    pub fetch: Option<String>,
}

impl SourceAction {
    /// Whether any of its files would change.
    pub fn changing(&self) -> bool {
        self.fetch.is_some() || self.files.iter().any(|file| file.op != Op::Unchanged)
    }

    /// The files this step changes, each with the journal id it is announced under.
    pub fn steps<'a>(&'a self, id: &'a str) -> impl Iterator<Item = (String, &'a FileAction)> {
        self.files
            .iter()
            .filter(|file| file.op != Op::Unchanged)
            .enumerate()
            .map(move |(index, file)| (format!("{id}.{}", index + 1), file))
    }
}

/// The journal id a step's restore is announced under (S9).
pub fn restore_id(step: &str) -> String {
    format!("{step}.restore")
}

/// What the sources of a plan ask of the index refresh.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Refresh {
    /// Each declared source that forces the refresh — its step changes something, or the lock
    /// does not record its files — with its URIs, in name order.
    pub forcing: Vec<(String, Vec<String>)>,
    /// Departed sources whose files change, with URIs read from their still-present stanza
    /// before it is removed. These force the refresh and are checked for apt warnings too.
    pub departing: Vec<(String, Vec<String>)>,
}

impl Refresh {
    pub fn forced(&self) -> bool {
        !self.forcing.is_empty() || !self.departing.is_empty()
    }
}

/// Plan every declared source and every source the lock still records, in name order, each as
/// one action. Every keyring's digest and framing is checked before anything else, so a mismatch
/// stops `plan` and `apply` alike before anything is read below the root or written (S5).
#[allow(clippy::too_many_arguments)]
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    keyrings: &BTreeMap<String, Vec<u8>>,
    lock: Option<&HostLock>,
    no_update: bool,
    actions: &mut Vec<Action>,
    warnings: &mut Vec<String>,
) -> Result<Refresh, Diagnostic> {
    let family = Family::of(gate.distro);
    let mut formats = BTreeMap::new();
    for (name, source) in &manifest.sources {
        // A keyring by URL is there only when the apply fetched it or the managed file already
        // has its digest; otherwise its step says `keyring: fetch` (LD-366).
        if source.signed_by_url.is_some() && !keyrings.contains_key(keyring_key(source)) {
            continue;
        }
        let bytes = keyrings.get(keyring_key(source)).ok_or_else(|| {
            Diagnostic::new(
                "E_STORE_IO",
                format!(
                    "[sources.{name}]: signed_by {:?} was not read before the plan",
                    source.signed_by
                ),
            )
        })?;
        let format = verify(source, bytes)?;
        if family == Family::Dnf && format == Format::Binary {
            return Err(Diagnostic::new(
                "E_TYPE",
                format!(
                    "[sources.{name}]: the keyring {} is binary OpenPGP packets, and dnf reads an \
                     ASCII-armored key only",
                    keyring_key(source)
                ),
            )
            .hint("export the key armored (`gpg --armor --export`) and declare that file"));
        }
        formats.insert(name.clone(), format);
    }

    let recorded: BTreeSet<String> = lock
        .map(|lock| {
            lock.files
                .keys()
                .filter_map(|path| name_of(family, path))
                .collect()
        })
        .unwrap_or_default();
    let mut names: BTreeSet<String> = manifest.sources.keys().cloned().collect();
    names.extend(recorded.iter().cloned());
    if names.is_empty() {
        return Ok(Refresh::default());
    }

    if !manifest.sources.is_empty() {
        match family {
            Family::Apt => one_owner(gate, manifest, &names)?,
            Family::Dnf => one_repository(gate, manifest, &names)?,
        }
    }

    let mut refresh = Refresh::default();
    for (index, name) in names.iter().enumerate() {
        let id = format!("s{}", index + 1);
        let (action, unrecorded) = match manifest.sources.get(name) {
            Some(source) => match (formats.get(name), &source.signed_by_url) {
                (Some(format), _) => {
                    declared(gate, &id, source, *format, keyrings, lock, warnings)?
                }
                (None, Some(url)) => (to_fetch(gate, &id, source, url, lock, warnings)?, false),
                (None, None) => unreachable!("a keyring by path is verified above"),
            },
            None => (departed(gate, family, &id, name, lock, warnings)?, false),
        };
        let Kind::Source(step) = &action.kind else {
            unreachable!("a source plans as a source step");
        };
        if manifest.sources.contains_key(name) {
            if step.changing() || unrecorded {
                refresh
                    .forcing
                    .push((name.clone(), manifest.sources[name].uris.clone()));
            }
        } else if step.changing() && family == Family::Apt {
            // The lock records file digests, not URIs. Read the managed stanza while it still
            // exists, before the source step removes it; a missing stanza has no active URI.
            let path = sources_path(name);
            let uris = super::sourceset::read_managed_uris(&gate.root, &path)?;
            refresh.departing.push((name.clone(), uris));
        }
        actions.push(action);
    }
    if no_update && refresh.forced() {
        for action in actions.iter_mut() {
            if let Kind::Source(step) = &mut action.kind
                && step.changing()
            {
                step.changes
                    .push("index not refreshed: --no-update".to_string());
            }
        }
    }
    Ok(refresh)
}

/// S7: a declared source whose (URI, suite) pair a stanza the machine already has lists is
/// `E_DUP_RESOURCE` naming that file, because apt refuses two `Signed-By` values for one
/// repository. The files this plan owns — the stanzas of every name it plans — are not read.
fn one_owner(
    gate: &Gate,
    manifest: &HostManifest,
    names: &BTreeSet<String>,
) -> Result<(), Diagnostic> {
    let own: BTreeSet<String> = names.iter().map(|name| sources_path(name)).collect();
    let listed = super::sourceset::read_machine(&gate.root, &|path| own.contains(path))?;
    for (name, source) in &manifest.sources {
        for (uri, suite) in super::sourceset::pairs(&source.uris, &source.suites) {
            if let Some(found) = listed
                .iter()
                .find(|listed| listed.uri == uri && listed.suite == suite)
            {
                return Err(Diagnostic::new(
                    "E_DUP_RESOURCE",
                    format!(
                        "[sources.{name}] lists {uri} {suite}, which {} already lists",
                        found.file
                    ),
                )
                .hint(format!(
                    "apt refuses one repository with two Signed-By values; remove or disable \
                     that stanza by hand (`Enabled: no`, or comment it out), or drop \
                     [sources.{name}]"
                )));
            }
        }
    }
    Ok(())
}

/// On Fedora: a declared source whose address an enabled repository of another `.repo` file of
/// the machine already lists by `baseurl` is `E_DUP_RESOURCE` naming that file. The files this
/// plan owns are not read.
fn one_repository(
    gate: &Gate,
    manifest: &HostManifest,
    names: &BTreeSet<String>,
) -> Result<(), Diagnostic> {
    let own: BTreeSet<String> = names
        .iter()
        .map(|name| Family::Dnf.stanza_path(name))
        .collect();
    let repos = super::reposet::read_machine(&gate.root, &|path| own.contains(path))?;
    let normal = |uri: &str| super::sourceset::normalize_uri(uri);
    for (name, source) in &manifest.sources {
        for uri in &source.uris {
            if let Some(found) = repos.iter().filter(|repo| repo.enabled).find(|repo| {
                repo.baseurls
                    .iter()
                    .any(|other| normal(other) == normal(uri))
            }) {
                return Err(Diagnostic::new(
                    "E_DUP_RESOURCE",
                    format!(
                        "[sources.{name}] lists {uri}, which {} already lists",
                        found.file
                    ),
                )
                .hint(format!(
                    "dnf would read one repository twice; disable it there by hand (`enabled=0` \
                     in its [{}] section), or drop [sources.{name}]",
                    found.id
                )));
            }
        }
    }
    Ok(())
}

/// The file entry behind one of a source's paths: mode 0644, owner and group root — the
/// invoking user under a scratch `--root` — kept and restored like a `[files]` entry that backs
/// up what it replaces.
fn file_entry(
    gate: &Gate,
    path: &str,
    content: Option<String>,
    source: Option<String>,
) -> FileEntry {
    let owner = if gate.system_root {
        "0".to_string()
    } else {
        gate.euid.to_string()
    };
    let group = if gate.system_root {
        "0".to_string()
    } else {
        gate.egid.to_string()
    };
    FileEntry {
        declared: path.to_string(),
        path: path.to_string(),
        content,
        source,
        mode: 0o644,
        state: FileState::Present,
        backup: true,
        on_remove: OnRemove::Restore,
        owner,
        group,
        kept_in_store: false,
    }
}

/// The stanza a declared source renders to, with `Signed-By` naming the path its keyring is
/// written to.
pub fn stanza(source: &SourceEntry, format: Format) -> super::sourceset::Stanza {
    super::sourceset::Stanza {
        types: source.types.clone(),
        uris: source.uris.clone(),
        suites: source.suites.clone(),
        components: source.components.clone(),
        architectures: source.architectures.clone(),
        signed_by: keyring_path(&source.name, format),
    }
}

fn declared(
    gate: &Gate,
    id: &str,
    source: &SourceEntry,
    format: Format,
    keyrings: &BTreeMap<String, Vec<u8>>,
    lock: Option<&HostLock>,
    warnings: &mut Vec<String>,
) -> Result<(Action, bool), Diagnostic> {
    let name = &source.name;
    let family = Family::of(gate.distro);
    let keyring = file_entry(
        gate,
        &family.key_path(name, format),
        None,
        Some(keyring_key(source).to_string()),
    );
    let text = match family {
        Family::Apt => super::sourceset::render(&stanza(source, format)),
        Family::Dnf => super::reposet::render(source, &family.key_path(name, format)),
    };
    let stanza = file_entry(gate, &family.stanza_path(name), Some(text), None);
    let (key_label, file_label) = family.labels();
    let mut files = Vec::new();
    for (label, entry) in [(key_label, &keyring), (file_label, &stanza)] {
        files.push((
            label,
            file_action(super::plan::plan_path(
                gate,
                id,
                &entry.path,
                Some(entry),
                keyrings,
                lock,
                warnings,
            )?),
        ));
    }
    // A keyring whose format changed: the new path and the stanza that names it are written
    // first, and the old path goes last, its record with it (S10).
    let old = keyring_path(name, format.other());
    if family == Family::Apt && lock.is_some_and(|lock| lock.files.contains_key(&old)) {
        files.push((
            "old keyring",
            file_action(super::plan::plan_path(
                gate, id, &old, None, keyrings, lock, warnings,
            )?),
        ));
    }
    let unrecorded = files.iter().any(|(_, file)| {
        file.desired.is_some()
            && !lock.is_some_and(|lock| {
                lock.files.get(&file.path).is_some_and(|record| {
                    Some(&record.digest) == file.desired.as_ref().map(|d| &d.digest)
                })
            })
    });
    Ok((summarise(id, name, files), unrecorded))
}

fn departed(
    gate: &Gate,
    family: Family,
    id: &str,
    name: &str,
    lock: Option<&HostLock>,
    warnings: &mut Vec<String>,
) -> Result<Action, Diagnostic> {
    let mut files = Vec::new();
    let (key_label, file_label) = family.labels();
    // The stanza goes first on the way out, so that no stanza ever names a missing keyring.
    for (label, path) in [
        (file_label, family.stanza_path(name)),
        (key_label, family.key_path(name, Format::Binary)),
        (key_label, keyring_path(name, Format::Armored)),
    ] {
        if family == Family::Dnf && label == key_label && path.starts_with(KEYRINGS_DIR) {
            continue;
        }
        if !lock.is_some_and(|lock| lock.files.contains_key(&path)) {
            continue;
        }
        files.push((
            label,
            file_action(super::plan::plan_path(
                gate,
                id,
                &path,
                None,
                &BTreeMap::new(),
                lock,
                warnings,
            )?),
        ));
    }
    Ok(summarise(id, name, files))
}

fn file_action(action: Action) -> FileAction {
    match action.kind {
        Kind::File(file) => *file,
        _ => unreachable!("a path plans as a file action"),
    }
}

/// One action from its file actions: the verb is the strongest of theirs, and the bracket names
/// what each changing file does.
fn summarise(id: &str, name: &str, files: Vec<(&'static str, FileAction)>) -> Action {
    let ops: Vec<Op> = files.iter().map(|(_, file)| file.op).collect();
    let op = if ops.iter().all(|op| *op == Op::Unchanged) {
        Op::Unchanged
    } else if ops
        .iter()
        .all(|op| matches!(op, Op::Create | Op::Unchanged))
    {
        Op::Create
    } else if ops
        .iter()
        .all(|op| matches!(op, Op::Remove | Op::Restore | Op::Keep | Op::Unchanged))
    {
        Op::Remove
    } else {
        Op::Replace
    };
    let changes: Vec<String> = if op == Op::Unchanged {
        vec!["unchanged".to_string()]
    } else {
        files
            .iter()
            .filter(|(_, file)| file.op != Op::Unchanged)
            .map(|(label, file)| match file.op {
                Op::Create if op == Op::Create => (*label).to_string(),
                Op::Create => format!("{label}: create {}", file.path),
                Op::Remove => format!("{label}: remove {}", file.path),
                _ if file.changes.is_empty() => (*label).to_string(),
                _ => format!("{label}: {}", file.changes.join(", ")),
            })
            .collect()
    };
    Action {
        id: id.to_string(),
        kind: Kind::Source(Box::new(SourceAction {
            name: name.to_string(),
            op,
            files: files.into_iter().map(|(_, file)| file).collect(),
            changes,
            fetch: None,
        })),
    }
}

// ----------------------------------------------------------------------- the forced refresh ---

/// The refresh a changing source forces: the index refresh the backend builds, unchanged, with
/// [`ERROR_MODE`] added after it. It is a new invocation beside the existing one; the package
/// manager's own argv builders are not edited (S9). dnf5's refresh is limited instead to the
/// repositories of the sources that forced it, `--repo=lodi-NAME` each, before `makecache`
/// (LD-433).
pub fn refresh_invocation(base: &Invocation, forcing: &[String]) -> Invocation {
    let mut invocation = base.clone();
    if invocation.program == "dnf5" {
        let at = invocation
            .args
            .iter()
            .position(|arg| arg == "makecache")
            .unwrap_or(invocation.args.len());
        for (offset, name) in forcing.iter().enumerate() {
            invocation.args.insert(
                at + offset,
                format!("--repo={}", super::reposet::repo_id(name)),
            );
        }
        return invocation;
    }
    invocation.args.push("-o".to_string());
    invocation.args.push(ERROR_MODE.to_string());
    invocation
}

/// Run the forced refresh, and fail it when apt fails, or when it reports an error or a warning
/// for a URI of a source that forced it — which is how an unreachable repository, a missing
/// `Release` file or a key that does not verify shows up in an `apt-get update` that still
/// exits 0 (S9).
pub fn run_refresh(
    invocation: &Invocation,
    forcing: &[(String, Vec<String>)],
) -> Result<(), Diagnostic> {
    let names = || {
        forcing
            .iter()
            .map(|(name, _)| format!("[sources.{name}]"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let output = invocation.command().output().map_err(|error| {
        Diagnostic::new(
            "E_APPLY",
            format!(
                "the index refresh {} forced could not run: cannot run {}: {error}",
                names(),
                invocation.path.display()
            ),
        )
    })?;
    let text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let reported = reported_for(&text, forcing);
    if output.status.success() && reported.is_none() {
        return Ok(());
    }
    let what = match reported {
        Some((name, line)) => format!("apt reported, for [sources.{name}]: {line}"),
        None => {
            let tail: Vec<&str> = text
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .collect();
            let start = tail.len().saturating_sub(3);
            format!(
                "`{}` exited {}: {}",
                invocation.command_line(),
                output
                    .status
                    .code()
                    .map_or_else(|| "on a signal".to_string(), |c| c.to_string()),
                tail[start..].join(" / ")
            )
        }
    };
    Err(Diagnostic::new(
        "E_APPLY",
        format!("the index refresh {} forced failed: {what}", names()),
    ))
}

/// The first error or warning line apt printed that names a URI of a forcing source.
fn reported_for<'a>(
    text: &'a str,
    forcing: &'a [(String, Vec<String>)],
) -> Option<(&'a str, &'a str)> {
    for line in text.lines().map(str::trim) {
        if !(line.starts_with("W:") || line.starts_with("E:") || line.starts_with("Err:")) {
            continue;
        }
        let lower = line.to_ascii_lowercase();
        for (name, uris) in forcing {
            if uris.iter().any(|uri| {
                let uri = super::sourceset::normalize_uri(uri).to_ascii_lowercase();
                lower.contains(&uri)
            }) {
                return Some((name.as_str(), line));
            }
        }
    }
    None
}

// ------------------------------------------------------------------------------ restoring ---

/// What one file of a source step was before this apply changed it, kept in memory so that a
/// failed forced refresh can put it back (S9). Nothing is kept on disk for it: the journal's
/// announcement of the restore is what a crash is judged by.
#[derive(Debug, Clone)]
pub struct Taken {
    /// The journal id of this file's restore.
    pub id: String,
    pub file: FileAction,
    /// The bytes at the path before the change, when there were any.
    pub bytes: Option<Vec<u8>>,
    /// The bytes of the backup a restore removed, for a departed source put back.
    pub kept: Option<Vec<u8>>,
}

/// The journal id of the restore of one step file: the file's own id, `.restore` after it.
pub fn file_restore_id(file_id: &str) -> String {
    restore_id(file_id)
}

/// Keep, in memory, what one step file is about to change.
pub fn take(gate: &Gate, id: &str, file: &FileAction) -> Result<Taken, Diagnostic> {
    let read = |path: &str| -> Result<Option<Vec<u8>>, Diagnostic> {
        let facts = Facts::observe(&gate.root, path);
        if !facts.exists {
            return Ok(None);
        }
        let mut handle = super::files::open_regular(&super::plan::join(&gate.root, path), path)?;
        let mut bytes = Vec::new();
        std::io::Read::read_to_end(&mut handle, &mut bytes).map_err(|error| {
            Diagnostic::new("E_APPLY", format!("{path}: cannot keep its bytes: {error}"))
        })?;
        Ok(Some(bytes))
    };
    Ok(Taken {
        id: file_restore_id(id),
        file: file.clone(),
        bytes: read(&file.path)?,
        kept: match &file.restore_from {
            Some(backup) if file.op == Op::Restore => read(backup)?,
            _ => None,
        },
    })
}

/// The two states that bracket the restore of one step file: what the change left, and what was
/// there before it — the change's own bracket, the other way round.
pub fn restore_bracket(file: &FileAction) -> (Vec<Facts>, Vec<Facts>) {
    let mut pre = vec![file.after.clone()];
    let mut post = vec![file.before.clone()];
    if let Some(backup) = &file.backup {
        pre.push(Facts {
            path: backup.clone(),
            exists: true,
            regular: true,
            digest: file.before.digest.clone(),
            mode: None,
            uid: None,
            gid: None,
        });
        post.push(Facts::absent(backup));
    }
    if let (Some(backup), Op::Restore) = (&file.restore_from, file.op) {
        pre.push(Facts::absent(backup));
        post.push(Facts {
            path: backup.clone(),
            exists: true,
            regular: true,
            digest: file.desired.as_ref().map(|d| d.digest.clone()),
            mode: None,
            uid: None,
            gid: None,
        });
    }
    (pre, post)
}

/// Put one step file back as it was before this apply. It is done only to a file that still is
/// what this apply left: anything else was changed by someone else since, and is left alone.
pub fn put_back(gate: &Gate, taken: &Taken) -> Result<(), Diagnostic> {
    let file = &taken.file;
    let now = Facts::observe(&gate.root, &file.path);
    if now.exists != file.after.exists || (now.exists && now.digest != file.after.digest) {
        return Err(Diagnostic::new(
            "E_APPLY",
            format!(
                "{} is not what this apply left there, so it was not put back",
                file.path
            ),
        ));
    }
    let write = |path: &str, bytes: &[u8], facts: &Facts| {
        let uid = facts.uid.unwrap_or(gate.euid);
        super::files::materialise(
            &gate.root,
            path,
            &Desired {
                digest: format!("sha256:{}", sha256_hex(bytes)),
                mode: facts.mode.unwrap_or(0o644),
                uid,
                gid: facts.gid.unwrap_or(gate.egid),
                owner: uid.to_string(),
                group: facts.gid.unwrap_or(gate.egid).to_string(),
                content: Content::Inline(bytes.to_vec()),
            },
        )
    };
    if let (Some(backup), Some(kept)) = (&file.restore_from, &taken.kept) {
        write(backup, kept, &file.after)?;
    }
    match &taken.bytes {
        Some(bytes) => write(&file.path, bytes, &file.before)?,
        None if now.exists => super::files::remove(&gate.root, &file.path)?,
        None => {}
    }
    if let Some(backup) = &file.backup
        && Facts::observe(&gate.root, backup).exists
    {
        super::files::remove(&gate.root, backup)?;
    }
    Ok(())
}

/// The key a source's keyring bytes are read, fetched and looked up under: its `signed_by`
/// path, or its `signed_by_url`.
pub fn keyring_key(entry: &SourceEntry) -> &str {
    entry.signed_by_url.as_deref().unwrap_or(&entry.signed_by)
}

/// A keyring by URL the plan cannot place yet: the step says `keyring: fetch`, and the apply
/// fetches it before it plans again. Without the keyring's bytes, each of the source's paths
/// still gets every refusal and every drift a planned path gets, so an apply decides them before
/// any request: a link on the way to it or a final component that is not a regular file is
/// `E_PATH_ESCAPE`, as [`super::plan::plan_path`] makes it, and a file the lock records that was
/// changed since is `W_DRIFT` here and `E_DRIFT` on the apply (S10). Such a file is carried as the
/// replacement it will be; this step is never performed as it is, because the apply plans again
/// once the keyring is fetched.
fn to_fetch(
    gate: &Gate,
    id: &str,
    entry: &SourceEntry,
    url: &str,
    lock: Option<&HostLock>,
    warnings: &mut Vec<String>,
) -> Result<Action, Diagnostic> {
    let family = Family::of(gate.distro);
    let stanza = family.stanza_path(&entry.name);
    let mut there = BTreeSet::new();
    let mut files = Vec::new();
    let mut paths = vec![
        family.key_path(&entry.name, Format::Binary),
        family.key_path(&entry.name, Format::Armored),
        stanza.clone(),
    ];
    paths.dedup();
    for path in paths {
        super::files::inspect_trusted(&gate.root, &path, false)?;
        let before = Facts::observe(&gate.root, &path);
        if before.exists && !before.regular {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("{path} exists and is not a regular file"),
            )
            .hint(
                "lodi never follows a symlink for the final component of a managed path; move \
                 the object aside by hand",
            ));
        }
        let recorded = lock.and_then(|lock| lock.files.get(&path));
        if before.exists || recorded.is_some() {
            there.insert(path.clone());
        }
        if let (Some(record), Some(digest)) = (recorded, &before.digest)
            && &record.digest != digest
        {
            warnings.push(format!(
                "{}: {path} was changed since lodi last wrote it",
                super::plan::W_DRIFT
            ));
            files.push(FileAction {
                path: path.clone(),
                op: Op::Replace,
                desired: None,
                backup: None,
                retained_backup: None,
                restore_from: None,
                adopt: false,
                after: before.clone(),
                before,
                drift: true,
                on_remove: record.on_remove.clone(),
                changes: Vec::new(),
            });
        }
    }
    let (key_label, file_label) = family.labels();
    let mut changes = vec![format!("{key_label}: fetch")];
    if !there.contains(&stanza) {
        changes.push(file_label.to_string());
    }
    Ok(Action {
        id: id.to_string(),
        kind: Kind::Source(Box::new(SourceAction {
            name: entry.name.clone(),
            op: if there.is_empty() {
                Op::Create
            } else {
                Op::Replace
            },
            files,
            changes,
            fetch: Some(url.to_string()),
        })),
    })
}

/// Every keyring by URL `plan` still needs fetched, as (source name, URL), in name order.
pub fn wanted(plan: &Plan) -> Vec<(String, String)> {
    plan.actions
        .iter()
        .filter_map(|action| match &action.kind {
            Kind::Source(source) => source
                .fetch
                .as_ref()
                .map(|url| (source.name.clone(), url.clone())),
            _ => None,
        })
        .collect()
}

/// Fetch every keyring of `wanted` through `fetcher` — HTTPS, redirects and retries as
/// [`crate::fetch`] enforces them, at most [`MAX_KEYRING_BYTES`] each — into memory, and check
/// each one as a keyring by path is checked ([`verify`]: digest, then framing). Every keyring is
/// fetched and checked before any is returned,
/// so one failure leaves the caller with nothing to write. Keyed by URL, like [`keyring_key`].
pub fn fetch(
    manifest: &HostManifest,
    wanted: &[(String, String)],
    fetcher: &dyn Fetcher,
) -> Result<BTreeMap<String, Vec<u8>>, Diagnostic> {
    let mut out = BTreeMap::new();
    for (name, url) in wanted {
        let entry = manifest
            .sources
            .get(name)
            .expect("a wanted keyring is a declared source's");
        let mut body = Vec::new();
        fetcher
            .download(url, &mut body, MAX_KEYRING_BYTES as u64)
            .map_err(|error| fetch_failed(name, error))?;
        verify(entry, &body)?;
        out.insert(url.clone(), body);
    }
    Ok(out)
}

fn fetch_failed(name: &str, error: FetchError) -> Diagnostic {
    let code = match error {
        FetchError::Insecure(_) | FetchError::InsecureRedirect(..) => "E_INSECURE_URL",
        _ => "E_FETCH",
    };
    Diagnostic::new(code, format!("[sources.{name}] signed_by_url: {error}")).hint(
        "the keyring could not be fetched, and nothing was written: no keyring, no stanza, no \
         journal, no lock. Fix the cause and run the apply again",
    )
}

/// Put into `sources` the keyring of every source by URL whose managed file below the root
/// already has `signed_by_sha256`: that file is the keyring, and neither the plan nor the apply
/// asks the network for it. Read without following a link, never past [`MAX_KEYRING_BYTES`].
/// A path that cannot be read so is left alone: the plan refuses it as it refuses any source
/// path — a link above the apt source paths as `E_SYNTAX` (S7), any other link on the way as
/// `E_PATH_ESCAPE` — before the apply could make a request.
pub fn read_installed(
    gate: &Gate,
    manifest: &HostManifest,
    sources: &mut BTreeMap<String, Vec<u8>>,
) {
    for entry in manifest.sources.values() {
        let Some(url) = &entry.signed_by_url else {
            continue;
        };
        for format in [Format::Binary, Format::Armored] {
            let path = Family::of(gate.distro).key_path(&entry.name, format);
            let Ok(real) = super::files::inspect_trusted(&gate.root, &path, false) else {
                continue;
            };
            if real.symlink_metadata().is_err() {
                continue;
            }
            let Ok(handle) = super::files::open_regular(&real, &path) else {
                continue;
            };
            let mut bytes = Vec::new();
            if handle
                .take(MAX_KEYRING_BYTES as u64 + 1)
                .read_to_end(&mut bytes)
                .is_err()
            {
                continue;
            }
            if sha256_hex(&bytes) == entry.signed_by_sha256 && sniff(&bytes) == Ok(format) {
                sources.insert(url.clone(), bytes);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_paths_round_trip_through_name_of() {
        assert_eq!(
            keyring_path("docker", Format::Armored),
            "/etc/apt/keyrings/lodi-docker.asc"
        );
        for path in [
            keyring_path("docker", Format::Binary),
            keyring_path("docker", Format::Armored),
            sources_path("docker"),
        ] {
            assert_eq!(name_of(Family::Apt, &path).as_deref(), Some("docker"));
        }
        assert_eq!(
            name_of(Family::Apt, "/etc/apt/sources.list.d/docker.sources"),
            None
        );
        assert_eq!(
            name_of(Family::Apt, "/etc/apt/keyrings/lodi-Docker.gpg"),
            None
        );
        assert_eq!(name_of(Family::Apt, "/etc/lodi-docker.sources"), None);
        for path in [
            Family::Dnf.stanza_path("vendor"),
            Family::Dnf.key_path("vendor", Format::Armored),
        ] {
            assert_eq!(name_of(Family::Dnf, &path).as_deref(), Some("vendor"));
            assert_eq!(name_of(Family::Apt, &path), None);
        }
        assert_eq!(name_of(Family::Dnf, "/etc/yum.repos.d/fedora.repo"), None);
        assert_eq!(
            namespace_name("/etc/apt/keyrings/lodi-x.key").as_deref(),
            Some("x")
        );
        assert_eq!(namespace_name("/etc/apt/keyrings/x.key"), None);
    }
}
