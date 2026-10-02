//! The host manifest (`/etc/lodi/host.toml`): `spec/01` §5, as this build implements it.
//!
//! `spec/01` §5 is the one manifest section that carries the 2026-09-17/18 TOML rewrite, and it
//! is implemented literally: `[host]`, `[vars]`, `[packages]` with the per-distro and per-arch
//! tables of §3.7 plus §5.2's `hold`, `mark_auto`, `optional` and `absent`, and
//! `[files."/absolute/path"]` with §4.3's keys plus `owner` and `group`, and `[services]`
//! (sc-1, LD-418). `[inputs]` and
//! `[modules]` are `E_UNSUPPORTED` (design call D12, LD-124), and a table for a resource kind
//! ADR-013 defers — `[users]`, `[groups]`, `[sysctls]`, `[hostname]`,
//! `[snapshots]`, `[tools]` — is `E_UNSUPPORTED` with the ADR's own sentence as its cause.
//!
//! Every problem in a file is reported in one run (`spec/01` §7). Loading executes nothing:
//! `content` and `source` are data here, and no path outside the manifest's own directory is
//! opened while parsing.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};

use toml_edit::{ImDocument, Item, Key, Value};

use crate::diag::{Diagnostic, Location};
use crate::manifest::{MAX_MANIFEST_BYTES, ManifestErrors, distance};

/// The `${ }` names that resolve in the host scope (`spec/01` §2.2, phase 1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    pub arch: String,
    pub os: String,
    pub distro: String,
    pub release: String,
    pub codename: String,
    pub lodi_version: String,
}

impl Context {
    /// A Linux machine's context, for this build of lodi.
    pub fn new(arch: &str, distro: &str, release: &str, codename: &str) -> Context {
        Context {
            arch: arch.to_string(),
            os: "linux".to_string(),
            distro: distro.to_string(),
            release: release.to_string(),
            codename: codename.to_string(),
            lodi_version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }

    /// The context of `distro` as its `os-release` says: the release is the version, or
    /// `rolling` where it has none (Arch).
    pub fn of(arch: &str, distro: super::safety::Distro, os: &super::safety::OsRelease) -> Context {
        let release = match os.version_id.is_empty() {
            true => "rolling",
            false => &os.version_id,
        };
        Context::new(arch, distro.name(), release, &os.codename)
    }

    /// The context of a root the safety gate has already identified.
    pub fn from_gate(gate: &super::safety::Gate) -> Context {
        Context::of(gate.arch(), gate.distro, &gate.os)
    }
}

/// A validated host manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostManifest {
    pub host: Host,
    pub vars: BTreeMap<String, String>,
    pub packages: Packages,
    /// Managed files by normalized absolute path, in path order: `[etc."PATH"]` and the legacy
    /// `[files]` table alike (si-1, LD-421).
    pub files: BTreeMap<String, FileEntry>,
    /// Whether the manifest has the legacy `[files]` table, which applies with one warning.
    pub legacy_files: bool,
    /// Declared third-party apt repositories by name, in name order (`[sources.NAME]`).
    pub sources: BTreeMap<String, SourceEntry>,
    /// `[services]`: each systemd unit and whether it is wanted enabled (sc-1, LD-418).
    pub services: BTreeMap<String, bool>,
    /// Declared accounts by name (`[users.NAME]`, su-1).
    pub users: BTreeMap<String, UserEntry>,
    /// Declared groups by name (`[groups.NAME]`, su-1).
    pub groups: BTreeMap<String, GroupEntry>,
    /// `[system]`: the hostname, time zone, locale and keymap (sd-1, LD-420).
    pub system: System,
    /// `[firewall]`: the rules of lodi's one nftables table (sd-1).
    pub firewall: Option<Firewall>,
    /// `[network]`: the interfaces lodi configures through the machine's own stack (sd-1).
    pub network: Option<Network>,
    /// `[kernel]`: parameters, modules to load and modules to blacklist (bc-1, LD-423).
    pub kernel: Kernel,
    /// `[sysctl]`: each kernel setting by its dotted name, and its value (bc-1).
    pub sysctl: BTreeMap<String, String>,
    /// `[boot]`: the loader the firmware starts, its timeout and its default entry (bl-1).
    pub boot: Option<Boot>,
}

/// `[boot]` (bl-1, LD-424): a setting it does not name is left as the loader has it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Boot {
    pub loader: Loader,
    pub timeout: Option<u32>,
    pub default: Option<String>,
}

/// The two loaders lodi installs and switches between.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Loader {
    Grub,
    SystemdBoot,
}

impl Loader {
    pub fn name(self) -> &'static str {
        match self {
            Loader::Grub => "grub",
            Loader::SystemdBoot => "systemd-boot",
        }
    }

    pub fn parse(name: &str) -> Option<Loader> {
        match name {
            "grub" => Some(Loader::Grub),
            "systemd-boot" => Some(Loader::SystemdBoot),
            _ => None,
        }
    }
}

/// `[kernel]` (bc-1, LD-423): each list lodi keeps; an empty one is not declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Kernel {
    pub parameters: Vec<String>,
    pub modules: Vec<String>,
    pub blacklist: Vec<String>,
}

/// `[system]` (sd-1, LD-420): each basic a host declares; one it does not declare is never set.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct System {
    pub hostname: Option<String>,
    pub timezone: Option<String>,
    pub locale: Option<String>,
    pub keymap: Option<String>,
}

impl System {
    /// Each basic by the name the plan and the record use, in apply order.
    pub fn each(&self) -> [(&'static str, Option<&String>); 4] {
        [
            ("hostname", self.hostname.as_ref()),
            ("timezone", self.timezone.as_ref()),
            ("locale", self.locale.as_ref()),
            ("keymap", self.keymap.as_ref()),
        ]
    }
}

/// `[firewall]`: what lodi's own table lets in. Everything else incoming is dropped, unless
/// `input = "accept"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Firewall {
    pub drop: bool,
    /// `(protocol, port or range)`: `("tcp", "22")`, `("udp", "8000-8100")`.
    pub allow: Vec<(String, String)>,
}

/// `[network]`: how long a change may take to be confirmed, what confirms it, and the interfaces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Network {
    pub confirm_within: u64,
    pub check: Option<String>,
    pub interfaces: BTreeMap<String, Interface>,
}

/// `[network.interfaces.NAME]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    pub dhcp: bool,
    pub addresses: Vec<String>,
    pub gateway: Option<String>,
    pub dns: Vec<String>,
}

/// The seconds a network change has to be confirmed in when `confirm_within` is not set.
pub const CONFIRM_WITHIN: u64 = 90;

/// `[users.NAME]`: one account an apply creates when it is missing, with a locked password
/// (LD-419). There is no key for a password or a hash: one is an unknown key.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct UserEntry {
    pub uid: Option<u32>,
    /// Supplementary groups the account is a member of.
    pub groups: Vec<String>,
    pub shell: Option<String>,
    pub home: Option<String>,
    /// Public keys, one `authorized_keys` line each.
    pub ssh_keys: Vec<String>,
}

/// `[groups.NAME]`: one group an apply creates when it is missing, with exactly its members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupEntry {
    pub gid: Option<u32>,
    pub members: Vec<String>,
}

/// `[sources.NAME]`: one third-party apt repository the apply arms before it refreshes the
/// index and installs from it (LD-365).
///
/// It is the deb822 stanza apt itself reads — `Types`, `URIs`, `Suites`, `Components`,
/// `Architectures` — plus the keyring apt is told to verify the repository with, carried as bytes
/// under the manifest's own directory the way a `[files] source` is, and pinned by its SHA-256.
/// The apply writes `/etc/apt/keyrings/lodi-NAME.{gpg|asc}` and
/// `/etc/apt/sources.list.d/lodi-NAME.sources`, whose `Signed-By:` names that keyring and
/// nothing else: Lodi holds no OpenPGP code and verifies no repository signature; apt does that,
/// against exactly the keys this entry shipped. A URI that is not `https://` is refused here, and
/// there is no `trusted` key at all.
///
/// On Fedora (LD-433) the same table is one dnf repository: `uris` are its `baseurl`, there are no
/// suites, components, types or architectures, and the key is an ASCII-armored block written to
/// `/etc/pki/rpm-gpg/lodi-NAME.asc`, which the `.repo` file's `gpgkey` names with `gpgcheck=1`.
/// A signature check is never switched off: `gpgcheck = false` or `repo_gpgcheck = false` is
/// `E_GPGCHECK_OFF` here.
///
/// A third-party pacman repository needs a key added to pacman's own keyring, which the host
/// scope refuses to do, so `[sources]` on an Arch root is `E_UNSUPPORTED` at parse time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEntry {
    pub name: String,
    /// `deb`, `deb-src`, or both; default `["deb"]`.
    pub types: Vec<String>,
    pub uris: Vec<String>,
    pub suites: Vec<String>,
    /// Empty exactly when every suite is flat (ends in `/`).
    pub components: Vec<String>,
    /// Empty when the table declares none: apt's own default then stands.
    pub architectures: Vec<String>,
    /// The keyring's path relative to the manifest's own directory, inside it; empty when the
    /// keyring is named by `signed_by_url` instead.
    pub signed_by: String,
    /// The keyring's `https://` URL (LD-366): fetched by the apply, never by the plan, and only
    /// when the file already at the managed path does not have `signed_by_sha256`.
    pub signed_by_url: Option<String>,
    /// The lowercase hex SHA-256 the keyring bytes must have, checked before anything is written.
    pub signed_by_sha256: String,
    /// Fedora only: `repo_gpgcheck = true`, dnf checks the repository's metadata signature too.
    pub repo_gpgcheck: bool,
}

/// `spec/01` §5.1.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Host {
    pub version: String,
    /// The `host.distro` assertion; `None` means "any".
    pub distro: Option<String>,
    pub auto_remove: bool,
    /// `packages = "exact" | "managed"` (LD-375); `None` when the key is not written, which this
    /// build treats as `managed` with a `W_UNDECLARED` line per package it would leave alone.
    pub packages: Option<PackagesMode>,
    pub min_lodi_version: String,
    /// The instant the machine this file was generated from was read, `YYYY-MM-DDTHH:MM:SSZ`.
    ///
    /// `lodi import` writes it (the owner's decision of 2026-09-22, which supersedes design
    /// call D10 forward); a hand-written manifest may carry it or leave it out. **Nothing in
    /// this build acts on it**: no package is pinned to it, no plan reads it and no apply
    /// behaves differently for its presence. It is recorded now so that the pinning milestone
    /// has the key it needs and so that every file written before then already carries the
    /// reading's date. The spelling is the one `[container] snapshot` already uses.
    pub snapshot: Option<String>,
}

/// What `[host] packages` says an apply does with an installed package the manifest does not
/// declare (LD-375).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackagesMode {
    /// The machine's packages are the manifest's plus the distribution's own baseline: a
    /// package that leaves the manifest leaves the machine, decided by the package manager's own
    /// simulation, and one installed by hand since lodi last recorded the machine is drift.
    Exact,
    /// 1.1.0's rule: `auto_remove` removes only what the record names.
    Managed,
}

impl PackagesMode {
    pub fn name(self) -> &'static str {
        match self {
            PackagesMode::Exact => "exact",
            PackagesMode::Managed => "managed",
        }
    }
}

impl Default for Host {
    fn default() -> Host {
        Host {
            version: "1".to_string(),
            distro: None,
            auto_remove: true,
            packages: None,
            min_lodi_version: String::new(),
            snapshot: None,
        }
    }
}

/// `spec/01` §5.2 and §3.7, merged by §6.2 into [`Packages::effective`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Packages {
    pub common: Vec<String>,
    /// Per-distro `add`/`remove`, by distribution name.
    pub per_distro: BTreeMap<String, AddRemove>,
    /// Per-architecture `add`/`remove`, by architecture name.
    pub per_arch: BTreeMap<String, AddRemove>,
    pub hold: Vec<String>,
    pub mark_auto: Vec<String>,
    pub optional: Vec<String>,
    pub absent: Vec<String>,
    /// `[packages.pin]`: a version or a date per name (M-Pin, LD-395).
    pub pin: BTreeMap<String, super::pin::Declared>,
    /// `[packages.<distro>.pin]`, by distribution, each over `[packages.pin]` on that
    /// distribution.
    pub distro_pin: BTreeMap<String, BTreeMap<String, super::pin::Declared>>,
    /// `[packages.arch] archived_keyring`: verify dated Arch pins against the dated
    /// `archlinux-keyring` in a private keyring that only `pin.conf` names (#170, LD-451).
    pub archived_keyring: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AddRemove {
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl Packages {
    /// Whether the manifest declares packages at all. `[packages]` present but empty still
    /// counts, because an empty declaration is a declaration.
    pub fn is_empty(&self) -> bool {
        self.common.is_empty()
            && self.per_distro.is_empty()
            && self.per_arch.is_empty()
            && self.hold.is_empty()
            && self.mark_auto.is_empty()
            && self.optional.is_empty()
            && self.absent.is_empty()
    }

    /// The pins of a machine of distribution `distro`: `[packages.<distro>.pin]`, then
    /// `[packages.pin]` for every name the first does not pin (LD-395's precedence; `[host]
    /// snapshot` and the live archive come after both and are not per-name).
    pub fn pins(&self, distro: &str) -> BTreeMap<String, super::pin::Declared> {
        let mut out = self.pin.clone();
        if let Some(own) = self.distro_pin.get(distro) {
            out.extend(own.iter().map(|(name, pin)| (name.clone(), pin.clone())));
        }
        out
    }

    /// `spec/01` §6.2 for distribution `distro` and architecture `arch`: start from `common`,
    /// append the two `add` lists, then remove the two `remove` lists. Order is first-seen and
    /// duplicates collapse.
    pub fn effective(&self, distro: &str, arch: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let push = |name: &String, out: &mut Vec<String>| {
            if !out.iter().any(|n| n == name) {
                out.push(name.clone());
            }
        };
        for name in &self.common {
            push(name, &mut out);
        }
        for table in [self.per_distro.get(distro), self.per_arch.get(arch)]
            .into_iter()
            .flatten()
        {
            for name in &table.add {
                push(name, &mut out);
            }
        }
        for table in [self.per_distro.get(distro), self.per_arch.get(arch)]
            .into_iter()
            .flatten()
        {
            out.retain(|n| !table.remove.contains(n));
        }
        out
    }
}

/// `spec/01` §5.3 (`[files."<path>"]`): §4.3's keys plus `owner` and `group`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The key as written, for messages.
    pub declared: String,
    /// The lexically normalized absolute path inside the root.
    pub path: String,
    pub content: Option<String>,
    /// A path relative to the manifest's own directory, inside it.
    pub source: Option<String>,
    pub mode: u32,
    pub state: FileState,
    pub backup: bool,
    pub on_remove: OnRemove,
    pub owner: String,
    pub group: String,
    /// The original is kept in lodi's own store, never beside the file: `[etc."PATH"]` writes
    /// no path under `/etc` it does not declare (si-1).
    pub kept_in_store: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileState {
    Present,
    Absent,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnRemove {
    Restore,
    Delete,
    Keep,
}

impl OnRemove {
    pub fn name(self) -> &'static str {
        match self {
            OnRemove::Restore => "restore",
            OnRemove::Delete => "delete",
            OnRemove::Keep => "keep",
        }
    }
}

const TOP_LEVEL: &[&str] = &[
    "host", "vars", "inputs", "modules", "packages", "etc", "files", "sources", "services",
    "users", "groups", "system", "firewall", "network", "kernel", "sysctl", "boot",
];
const BOOT_KEYS: &[&str] = &["loader", "timeout", "default"];
const KERNEL_KEYS: &[&str] = &["parameters", "modules", "blacklist"];
const ETC_KEYS: &[&str] = &["text", "mode", "owner", "group"];
const SYSTEM_KEYS: &[&str] = &["hostname", "timezone", "locale", "keymap"];
const FIREWALL_KEYS: &[&str] = &["input", "allow"];
const NETWORK_KEYS: &[&str] = &["confirm_within", "check", "interfaces"];
const INTERFACE_KEYS: &[&str] = &["dhcp", "addresses", "gateway", "dns"];
const USER_KEYS: &[&str] = &["uid", "groups", "shell", "home", "ssh_keys"];
const GROUP_KEYS: &[&str] = &["gid", "members"];
/// Keys a person may reach for to set a password, which lodi never writes (LD-419).
const PASSWORD_KEYS: &[&str] = &["password", "passwd", "hash", "password_hash", "shadow"];
/// The public-key types an `authorized_keys` line may start with.
const SSH_KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
];
/// The security-key types, whose names carry OpenSSH's own domain after the prefix.
const SSH_SK_PREFIXES: &[&str] = &["sk-ssh-ed25519", "sk-ecdsa-sha2-nistp256"];
const HOST_KEYS: &[&str] = &[
    "version",
    "distro",
    "auto_remove",
    "packages",
    "min_lodi_version",
    "snapshot",
];
const PACKAGES_KEYS: &[&str] = &[
    "common",
    "debian",
    "ubuntu",
    "arch",
    "fedora",
    "x86_64",
    "aarch64",
    "hold",
    "mark_auto",
    "optional",
    "absent",
    "pin",
];
const PACKAGE_TABLE_KEYS: &[&str] = &[
    "add",
    "remove",
    "hold",
    "mark_auto",
    "optional",
    "absent",
    "pin",
    "archived_keyring",
];
const FILE_KEYS: &[&str] = &[
    "content",
    "source",
    "mode",
    "state",
    "backup",
    "on_remove",
    "owner",
    "group",
];
const SOURCE_KEYS: &[&str] = &[
    "types",
    "uris",
    "suites",
    "components",
    "architectures",
    "signed_by",
    "signed_by_url",
    "signed_by_sha256",
];
/// The keys of a `[sources.NAME]` table on Fedora (LD-433): a dnf repository has no deb822 fields.
const FEDORA_SOURCE_KEYS: &[&str] = &[
    "uris",
    "signed_by",
    "signed_by_url",
    "signed_by_sha256",
    "gpgcheck",
    "repo_gpgcheck",
];
const DISTRO_TABLES: &[&str] = &["debian", "ubuntu", "arch", "fedora"];
const ARCH_TABLES: &[&str] = &["x86_64", "aarch64"];
/// The resource kinds ADR-013 defers out of v1. A table for one of them is an error, never a
/// silent default.
const DEFERRED_TABLES: &[&str] = &["sysctls", "hostname", "snapshots", "tools"];
/// The hint for every one of them: what the host scope manages instead (ADR-013).
const DEFERRED_CAUSE: &str = "remove the table: lodi manages the packages, package sources, \
                              files, services, users, groups, OS basics, firewall and network of \
                              a machine";
/// The version that may add `[inputs]` and `[modules]` (design call D12, LD-124).
const MODULES_VERSION: &str = "1.0";

/// Validate a host manifest already read into memory (LD-379): its size, its encoding, then its
/// text. `file` names it in diagnostics.
pub fn from_bytes(bytes: &[u8], file: &str, ctx: &Context) -> Result<HostManifest, ManifestErrors> {
    let fail = |d: Diagnostic| ManifestErrors {
        diagnostics: vec![d],
    };
    if bytes.len() > MAX_MANIFEST_BYTES {
        return Err(fail(Diagnostic::new(
            "E_SYNTAX",
            format!(
                "host manifest is over {MAX_MANIFEST_BYTES} bytes; the limit is \
                 {MAX_MANIFEST_BYTES} bytes (1 MiB)"
            ),
        )));
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => parse(text, file, ctx),
        Err(_) => Err(fail(Diagnostic::new(
            "E_SYNTAX",
            "host manifest is not valid UTF-8",
        ))),
    }
}

/// Validate host-manifest text; `file` names the source in diagnostics.
pub fn parse(text: &str, file: &str, ctx: &Context) -> Result<HostManifest, ManifestErrors> {
    let mut v = Validator {
        file,
        text,
        ctx,
        collected_vars: BTreeMap::new(),
        diagnostics: Vec::new(),
    };
    if text.len() > MAX_MANIFEST_BYTES {
        v.error(
            "E_SYNTAX",
            format!(
                "host manifest is {} bytes; the limit is {MAX_MANIFEST_BYTES} bytes (1 MiB)",
                text.len()
            ),
            None,
        );
        return Err(v.finish());
    }
    let doc = match ImDocument::parse(text) {
        Ok(doc) => doc,
        Err(e) => {
            let span = e.span();
            let message = e.message().lines().next().unwrap_or("").trim().to_string();
            v.error("E_SYNTAX", message, span);
            return Err(v.finish());
        }
    };
    let manifest = v.root(doc.as_table());
    if v.diagnostics.is_empty() {
        Ok(manifest)
    } else {
        Err(v.finish())
    }
}

struct Entry<'a> {
    key: &'a str,
    key_span: Option<Range<usize>>,
    item: &'a Item,
}

fn entries<'a>(table: &'a dyn toml_edit::TableLike) -> Vec<Entry<'a>> {
    table
        .iter()
        .map(|(key, item)| Entry {
            key,
            key_span: table
                .get_key_value(key)
                .and_then(|(k, _): (&Key, _)| k.span()),
            item,
        })
        .collect()
}

fn kind(item: &Item) -> &'static str {
    match item {
        Item::None => "nothing",
        Item::Table(_) => "a table",
        Item::ArrayOfTables(_) => "an array of tables",
        Item::Value(value) => match value {
            Value::String(_) => "a string",
            Value::Integer(_) => "an integer",
            Value::Float(_) => "a float",
            Value::Boolean(_) => "a boolean",
            Value::Datetime(_) => "a date-time",
            Value::Array(_) => "an array",
            Value::InlineTable(_) => "an inline table",
        },
    }
}

fn suggestion(key: &str, known: &[&str]) -> Option<String> {
    known
        .iter()
        .map(|k| (distance(key, k), *k))
        .filter(|(d, k)| *d <= 2 && *d < k.len())
        .min()
        .map(|(_, k)| k.to_string())
}

struct Validator<'t> {
    file: &'t str,
    text: &'t str,
    ctx: &'t Context,
    /// `[vars]`, read before everything else so that `${vars.*}` resolves anywhere below.
    collected_vars: BTreeMap<String, String>,
    diagnostics: Vec<Diagnostic>,
}

impl Validator<'_> {
    fn finish(&mut self) -> ManifestErrors {
        let mut diagnostics = std::mem::take(&mut self.diagnostics);
        diagnostics.sort_by_key(|d| {
            d.location
                .as_ref()
                .map_or((0, 0), |at| (at.line, at.column))
        });
        ManifestErrors { diagnostics }
    }

    fn error(&mut self, code: &'static str, message: String, span: Option<Range<usize>>) {
        let mut d = Diagnostic::new(code, message);
        if let Some(span) = span {
            d = d.at(Location::at(self.file, self.text, span.start));
        }
        self.diagnostics.push(d);
    }

    fn error_hint(
        &mut self,
        code: &'static str,
        message: String,
        hint: String,
        span: Option<Range<usize>>,
    ) {
        let mut d = Diagnostic::new(code, message).hint(hint);
        if let Some(span) = d.location.is_none().then_some(span).flatten() {
            d = d.at(Location::at(self.file, self.text, span.start));
        }
        self.diagnostics.push(d);
    }

    fn span_of(entry: &Entry<'_>) -> Option<Range<usize>> {
        entry.item.span().or_else(|| entry.key_span.clone())
    }

    fn unknown(&mut self, entry: &Entry<'_>, place: &str, known: &[&str]) {
        let (code, what) = if entry.item.is_table_like() || entry.item.is_array_of_tables() {
            ("E_UNKNOWN_BLOCK", "table")
        } else {
            ("E_UNKNOWN_ATTR", "key")
        };
        let hint = match suggestion(entry.key, known) {
            Some(near) => format!("did you mean `{near}`?"),
            None => format!("valid names: {}", known.join(", ")),
        };
        self.error_hint(
            code,
            format!("unknown {what} `{}` in {place}", entry.key),
            hint,
            entry.key_span.clone(),
        );
    }

    fn type_error(&mut self, entry: &Entry<'_>, place: &str, expected: &str) {
        let span = Self::span_of(entry);
        self.error_hint(
            "E_TYPE",
            format!(
                "`{}` in {place} must be {expected}, found {}",
                entry.key,
                kind(entry.item)
            ),
            format!("write it as {expected}"),
            span,
        );
    }

    fn table<'a>(
        &mut self,
        entry: &Entry<'a>,
        place: &str,
    ) -> Option<&'a dyn toml_edit::TableLike> {
        let table = entry.item.as_table_like();
        if table.is_none() {
            self.type_error(entry, place, "a table");
        }
        table
    }

    fn boolean(&mut self, entry: &Entry<'_>, place: &str) -> Option<bool> {
        match entry.item.as_bool() {
            Some(b) => Some(b),
            None => {
                self.type_error(entry, place, "a boolean");
                None
            }
        }
    }

    /// A string value with `spec/01` §1.3 substitution applied.
    fn string(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        match entry.item.as_str() {
            Some(s) => self.substitute(s, Self::span_of(entry)),
            None => {
                self.type_error(entry, place, "a string");
                None
            }
        }
    }

    /// An array of strings, each substituted.
    fn string_array(&mut self, entry: &Entry<'_>, place: &str) -> Vec<String> {
        let Some(array) = entry.item.as_array() else {
            self.type_error(entry, place, "an array of strings");
            return Vec::new();
        };
        let mut out = Vec::new();
        for value in array.iter() {
            match value.as_str() {
                Some(s) => {
                    if let Some(s) = self.substitute(s, value.span()) {
                        out.push(s);
                    }
                }
                None => self.error(
                    "E_TYPE",
                    format!(
                        "every element of `{}` in {place} must be a string",
                        entry.key
                    ),
                    value.span(),
                ),
            }
        }
        out
    }

    /// `spec/01` §1.3 and §2.2 in the host scope: `$${` is a literal `${`, a bare `$NAME` is
    /// left alone, and a `${ }` holds exactly one name. `vars.*`, `host.*` and `lodi.version`
    /// resolve here; the names `spec/01` defines only for other scopes are `E_UNSUPPORTED`, and
    /// anything else is `E_EXCLUDED_CONSTRUCT` (OD-09: no expressions, ever).
    fn substitute(&mut self, s: &str, span: Option<Range<usize>>) -> Option<String> {
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        let mut ok = true;
        while let Some(i) = rest.find('$') {
            out.push_str(&rest[..i]);
            let tail = &rest[i..];
            if let Some(after) = tail.strip_prefix("$${") {
                out.push_str("${");
                rest = after;
                continue;
            }
            let Some(after) = tail.strip_prefix("${") else {
                out.push('$');
                rest = &tail[1..];
                continue;
            };
            let Some(end) = after.find('}') else {
                self.error(
                    "E_EXCLUDED_CONSTRUCT",
                    "unterminated `${` in a string; write `$${` for a literal `${`".into(),
                    span.clone(),
                );
                return None;
            };
            let name = &after[..end];
            rest = &after[end + 1..];
            match self.resolve(name) {
                Ok(value) => out.push_str(&value),
                Err(d) => {
                    self.diagnostics.push(match (&span, d.location.is_none()) {
                        (Some(span), true) => d.at(Location::at(self.file, self.text, span.start)),
                        _ => d,
                    });
                    ok = false;
                }
            }
        }
        out.push_str(rest);
        ok.then_some(out)
    }

    fn resolve(&self, name: &str) -> Result<String, Diagnostic> {
        if let Some(var) = name.strip_prefix("vars.") {
            return match self.vars().get(var) {
                Some(value) => Ok(value.clone()),
                None => Err(Diagnostic::new(
                    "E_VAR_UNSET",
                    format!("`${{{name}}}` names a variable that [vars] does not declare"),
                )
                .hint(format!(
                    "add `{var} = \"…\"` to [vars], or write the value literally"
                ))),
            };
        }
        match name {
            "host.arch" => Ok(self.ctx.arch.clone()),
            "host.os" => Ok(self.ctx.os.clone()),
            "host.distro" => Ok(self.ctx.distro.clone()),
            "host.release" => Ok(self.ctx.release.clone()),
            "host.codename" => Ok(self.ctx.codename.clone()),
            "lodi.version" => Ok(self.ctx.lodi_version.clone()),
            _ if name.starts_with("project.")
                || name.starts_with("input.")
                || name.starts_with("tools.")
                || name.starts_with("container.")
                || name.starts_with("self.") =>
            {
                Err(Diagnostic::new(
                    "E_UNSUPPORTED",
                    format!("`${{{name}}}` is not defined in the host scope"),
                )
                .hint(
                    "the host scope resolves vars.*, host.arch, host.os, host.distro, \
                     host.release, host.codename and lodi.version"
                        .to_string(),
                ))
            }
            _ => Err(Diagnostic::new(
                "E_EXCLUDED_CONSTRUCT",
                format!("`${{{name}}}` is not a substitution name"),
            )
            .hint(
                "put one name inside `${ }`, such as `${host.arch}`, or write `$${` for a \
                 literal `${`"
                    .to_string(),
            )),
        }
    }

    fn vars(&self) -> &BTreeMap<String, String> {
        &self.collected_vars
    }

    fn root(&mut self, table: &dyn toml_edit::TableLike) -> HostManifest {
        // `[vars]` holds literals and is read before everything else (`spec/01` §2.1 note 1).
        for entry in entries(table) {
            if entry.key == "vars"
                && let Some(vars) = self.table(&entry, "the root")
            {
                self.read_vars(vars);
            }
        }
        let mut manifest = HostManifest {
            host: Host::default(),
            vars: self.collected_vars.clone(),
            packages: Packages::default(),
            files: BTreeMap::new(),
            legacy_files: false,
            sources: BTreeMap::new(),
            services: BTreeMap::new(),
            users: BTreeMap::new(),
            groups: BTreeMap::new(),
            system: System::default(),
            firewall: None,
            network: None,
            kernel: Kernel::default(),
            sysctl: BTreeMap::new(),
            boot: None,
        };
        let mut packages_seen = false;
        for entry in entries(table) {
            match entry.key {
                "vars" => {}
                "host" => {
                    if let Some(host) = self.table(&entry, "the root") {
                        manifest.host = self.read_host(host);
                    }
                }
                "inputs" | "modules" => self.error_hint(
                    "E_UNSUPPORTED",
                    format!("`[{}]` is not in the host scope of this build", entry.key),
                    format!(
                        "modules and registries are the recipe catalogue's machinery; \
                         {MODULES_VERSION} is the earliest release that may add them to the host \
                         scope"
                    ),
                    entry.key_span.clone(),
                ),
                "packages" => {
                    if let Some(packages) = self.table(&entry, "the root") {
                        packages_seen = true;
                        manifest.packages = self.read_packages(packages);
                    }
                }
                "files" => {
                    if let Some(files) = self.table(&entry, "the root") {
                        manifest.legacy_files = !files.is_empty();
                        let read = self.read_files(files);
                        self.merge_files(&mut manifest.files, read, &entry);
                    }
                }
                "etc" => {
                    if let Some(etc) = self.table(&entry, "the root") {
                        let read = self.read_etc(etc);
                        self.merge_files(&mut manifest.files, read, &entry);
                    }
                }
                "sources" => {
                    if let Some(sources) = self.table(&entry, "the root") {
                        manifest.sources = self.read_sources(sources, &entry);
                    }
                }
                "services" => {
                    if let Some(services) = self.table(&entry, "the root") {
                        manifest.services = self.read_services(services);
                    }
                }
                "users" => {
                    if let Some(users) = self.table(&entry, "the root") {
                        manifest.users = self.read_users(users);
                    }
                }
                "groups" => {
                    if let Some(groups) = self.table(&entry, "the root") {
                        manifest.groups = self.read_groups(groups);
                    }
                }
                "system" => {
                    if let Some(system) = self.table(&entry, "the root") {
                        manifest.system = self.read_system(system);
                    }
                }
                "firewall" => {
                    if let Some(firewall) = self.table(&entry, "the root") {
                        manifest.firewall = Some(self.read_firewall(firewall));
                    }
                }
                "network" => {
                    if let Some(network) = self.table(&entry, "the root") {
                        manifest.network = Some(self.read_network(network));
                    }
                }
                "kernel" => {
                    if let Some(kernel) = self.table(&entry, "the root") {
                        manifest.kernel = self.read_kernel(kernel);
                    }
                }
                "sysctl" => {
                    if let Some(sysctl) = self.table(&entry, "the root") {
                        manifest.sysctl = self.read_sysctl(sysctl);
                    }
                }
                "boot" => {
                    if let Some(boot) = self.table(&entry, "the root") {
                        manifest.boot = self.read_boot(&entry, boot);
                    }
                }
                key if DEFERRED_TABLES.contains(&key) => self.error_hint(
                    "E_UNSUPPORTED",
                    format!("`[{key}]` is not a resource kind this build manages"),
                    DEFERRED_CAUSE.to_string(),
                    entry.key_span.clone(),
                ),
                _ => self.unknown(&entry, "the root", TOP_LEVEL),
            }
        }
        if packages_seen {
            self.cross_check_packages(&manifest);
        }
        self.cross_check_sources(&manifest);
        self.check_min_lodi_version(&manifest.host);
        manifest
    }

    fn read_vars(&mut self, table: &dyn toml_edit::TableLike) {
        for entry in entries(table) {
            match entry.item.as_str() {
                Some(value) => {
                    self.collected_vars
                        .insert(entry.key.to_string(), value.to_string());
                }
                None => self.type_error(&entry, "[vars]", "a string"),
            }
        }
    }

    fn read_host(&mut self, table: &dyn toml_edit::TableLike) -> Host {
        let mut host = Host::default();
        for entry in entries(table) {
            match entry.key {
                "version" => {
                    if let Some(value) = self.string(&entry, "[host]") {
                        if value != "1" {
                            self.error_hint(
                                "E_UNSUPPORTED",
                                format!(
                                    "[host] version \"{value}\" is not a schema this build reads"
                                ),
                                "this build reads host manifest schema \"1\"".into(),
                                Self::span_of(&entry),
                            );
                        }
                        host.version = value;
                    }
                }
                "distro" => {
                    if let Some(value) = self.string(&entry, "[host]") {
                        host.distro = Some(value);
                    }
                }
                "auto_remove" => {
                    if let Some(value) = self.boolean(&entry, "[host]") {
                        host.auto_remove = value;
                    }
                }
                "packages" => {
                    if let Some(value) = self.string(&entry, "[host]") {
                        match value.as_str() {
                            "exact" => host.packages = Some(PackagesMode::Exact),
                            "managed" => host.packages = Some(PackagesMode::Managed),
                            _ => self.error_hint(
                                "E_TYPE",
                                format!("[host] packages = \"{value}\" is not a value this build reads"),
                                "`packages` is `exact` (the machine's packages are the manifest's) \
                                 or `managed` (only what lodi recorded leaves)"
                                    .into(),
                                Self::span_of(&entry),
                            ),
                        }
                    }
                }
                "min_lodi_version" => {
                    if let Some(value) = self.string(&entry, "[host]") {
                        host.min_lodi_version = value;
                    }
                }
                "snapshot" => {
                    if let Some(value) = self.string(&entry, "[host]") {
                        if crate::util::parse_utc(&value).is_some() {
                            host.snapshot = Some(value);
                        } else {
                            self.error_hint(
                                "E_TYPE",
                                format!(
                                    "[host] snapshot \"{value}\" is not an RFC 3339 UTC timestamp"
                                ),
                                "write it as YYYY-MM-DDTHH:MM:SSZ, or remove the line to install \
                                 from the live archive"
                                    .into(),
                                Self::span_of(&entry),
                            );
                        }
                    }
                }
                _ => self.unknown(&entry, "[host]", HOST_KEYS),
            }
        }
        if host.packages == Some(PackagesMode::Exact) && !host.auto_remove {
            self.error_hint(
                "E_ATTR_CONFLICT",
                "[host] packages = \"exact\" and auto_remove = false contradict each other".into(),
                "exact removes what the manifest does not declare; drop auto_remove = false, or \
                 write packages = \"managed\" to keep what lodi installed"
                    .into(),
                None,
            );
        }
        host
    }

    fn check_min_lodi_version(&mut self, host: &Host) {
        if host.min_lodi_version.is_empty() {
            return;
        }
        let running = crate::version::Version::parse(env!("CARGO_PKG_VERSION"))
            .expect("the crate version is a semver version");
        match crate::version::Constraint::parse(&host.min_lodi_version) {
            Ok(constraint) if constraint.matches(&running) => {}
            Ok(_) => self.error_hint(
                "E_VERSION",
                format!(
                    "[host] min_lodi_version = \"{}\" is not satisfied by lodi {}",
                    host.min_lodi_version,
                    env!("CARGO_PKG_VERSION")
                ),
                "install a lodi that satisfies it, or relax the constraint".into(),
                None,
            ),
            Err(e) => self.error_hint(
                "E_TYPE",
                format!(
                    "[host] min_lodi_version = \"{}\" is not a version constraint: {e}",
                    host.min_lodi_version
                ),
                "write a constraint such as \"0.6\" or \">=0.6.0\"".into(),
                None,
            ),
        }
    }

    fn read_packages(&mut self, table: &dyn toml_edit::TableLike) -> Packages {
        let mut packages = Packages::default();
        for entry in entries(table) {
            match entry.key {
                "common" => {
                    packages.common = self.package_names(&entry, "[packages]");
                }
                "hold" => packages.hold = self.package_names(&entry, "[packages]"),
                "mark_auto" => packages.mark_auto = self.package_names(&entry, "[packages]"),
                "optional" => packages.optional = self.package_names(&entry, "[packages]"),
                "absent" => packages.absent = self.package_names(&entry, "[packages]"),
                "pin" => packages.pin = self.read_pins(&entry, "[packages.pin]"),
                key if DISTRO_TABLES.contains(&key) || ARCH_TABLES.contains(&key) => {
                    let place = format!("[packages.{key}]");
                    if let Some(inner) = self.table(&entry, "[packages]") {
                        let table = self.read_package_table(inner, key, &place, &mut packages);
                        if DISTRO_TABLES.contains(&key) {
                            packages.per_distro.insert(key.to_string(), table);
                        } else {
                            packages.per_arch.insert(key.to_string(), table);
                        }
                    }
                }
                _ => self.unknown(&entry, "[packages]", PACKAGES_KEYS),
            }
        }
        packages
    }

    /// A per-distro or per-arch table. `spec/01` §5.2 allows `hold`, `mark_auto`, `optional` and
    /// `absent` inside them as well, and §6.1 unions those lists with the outer ones.
    fn read_package_table(
        &mut self,
        table: &dyn toml_edit::TableLike,
        key: &str,
        place: &str,
        packages: &mut Packages,
    ) -> AddRemove {
        let mut out = AddRemove::default();
        for entry in entries(table) {
            match entry.key {
                "add" => out.add = self.package_names(&entry, place),
                "remove" => out.remove = self.package_names(&entry, place),
                "hold" => {
                    let names = self.package_names(&entry, place);
                    union(&mut packages.hold, names);
                }
                "mark_auto" => {
                    let names = self.package_names(&entry, place);
                    union(&mut packages.mark_auto, names);
                }
                "optional" => {
                    let names = self.package_names(&entry, place);
                    union(&mut packages.optional, names);
                }
                "absent" => {
                    let names = self.package_names(&entry, place);
                    union(&mut packages.absent, names);
                }
                // A pin is one distribution's version or date (LD-395): a per-distribution table
                // may carry one, an architecture table may not.
                "pin" if DISTRO_TABLES.contains(&key) => {
                    let pins = self.read_pins(&entry, &format!("[packages.{key}.pin]"));
                    packages.distro_pin.insert(key.to_string(), pins);
                }
                "archived_keyring" if key == "arch" => {
                    if let Some(value) = self.boolean(&entry, place) {
                        packages.archived_keyring = value;
                    }
                }
                "archived_keyring" => self.error_hint(
                    "E_UNSUPPORTED",
                    format!("`archived_keyring` in {place} is not a key this build reads"),
                    "the archived keyring is Arch's: set it in [packages.arch]".into(),
                    entry.key_span.clone(),
                ),
                "pin" => self.error_hint(
                    "E_UNSUPPORTED",
                    format!("`pin` in {place} is not a table this build reads"),
                    "a version is one distribution's spelling: pin it in [packages.pin] or in \
                     [packages.debian.pin], [packages.ubuntu.pin]"
                        .into(),
                    entry.key_span.clone(),
                ),
                _ => self.unknown(&entry, place, PACKAGE_TABLE_KEYS),
            }
        }
        out
    }

    /// A pin table (M-Pin, LD-395): a package name per key, a version or a date per value
    /// ([`super::pin::parse_value`]). Arch reads the same declaration (LD-396).
    fn read_pins(
        &mut self,
        entry: &Entry<'_>,
        place: &str,
    ) -> BTreeMap<String, super::pin::Declared> {
        let mut out = BTreeMap::new();
        let Some(table) = self.table(entry, place) else {
            return out;
        };
        for pin in entries(table) {
            let name = pin.key.to_string();
            if !is_package_name(&name) {
                self.error_hint(
                    "E_IDENT",
                    format!("`{name}` in {place} is not a package name"),
                    "package names are the distribution's own: [A-Za-z0-9] then \
                     [A-Za-z0-9.+_@-]"
                        .into(),
                    pin.key_span.clone(),
                );
                continue;
            }
            let Some(value) = self.string(&pin, place) else {
                continue;
            };
            match super::pin::parse_value(&name, &value) {
                Ok(request) => {
                    out.insert(
                        name,
                        super::pin::Declared {
                            requested: value,
                            request,
                        },
                    );
                }
                Err(refusal) => self.error_hint(
                    refusal.code,
                    refusal.message,
                    refusal.hint,
                    Self::span_of(&pin),
                ),
            }
        }
        out
    }

    /// An array of package names, each checked against the grammar. A name carrying a version is
    /// `E_UNSUPPORTED`: host packages track the distribution and 0.6 writes no constraint and no
    /// pin (OD-15, design call D9/LD-121).
    fn package_names(&mut self, entry: &Entry<'_>, place: &str) -> Vec<String> {
        let names = self.string_array(entry, place);
        let mut out = Vec::new();
        for name in names {
            if name.chars().any(|c| "=<>~ \t/:".contains(c)) {
                let bare = name
                    .split(|c| "=<>~ \t/:".contains(c))
                    .next()
                    .unwrap_or_default();
                self.error_hint(
                    "E_UNSUPPORTED",
                    format!("`{name}` in `{}` of {place} carries a version", entry.key),
                    format!(
                        "write `{bare}` alone, or pin a version in [packages.pin]: \
                         `{bare} = \"VERSION\"`"
                    ),
                    Self::span_of(entry),
                );
                continue;
            }
            if !is_package_name(&name) {
                self.error_hint(
                    "E_IDENT",
                    format!(
                        "`{name}` in `{}` of {place} is not a package name",
                        entry.key
                    ),
                    "package names are the distribution's own: [A-Za-z0-9] then \
                     [A-Za-z0-9.+_@-]"
                        .into(),
                    Self::span_of(entry),
                );
                continue;
            }
            if !out.contains(&name) {
                out.push(name);
            }
        }
        out
    }

    /// `spec/01` §5.2 and §6.2: a `hold`, `mark_auto` or `optional` name outside the effective
    /// list, and a `remove` naming a package that is not in `common`, are `E_UNKNOWN_PACKAGE`.
    fn cross_check_packages(&mut self, manifest: &HostManifest) {
        let distro = self.ctx.distro.clone();
        let arch = self.ctx.arch.clone();
        let effective: BTreeSet<String> = manifest
            .packages
            .effective(&distro, &arch)
            .into_iter()
            .collect();
        // `optional` names may be absent from the distribution's index, but they are still part
        // of the declaration, so the effective list is what they must appear in.
        let mut declared = effective.clone();
        for table in manifest
            .packages
            .per_distro
            .values()
            .chain(manifest.packages.per_arch.values())
        {
            declared.extend(table.add.iter().cloned());
        }
        for (list, label) in [
            (&manifest.packages.hold, "hold"),
            (&manifest.packages.mark_auto, "mark_auto"),
            (&manifest.packages.optional, "optional"),
        ] {
            for name in list {
                if !declared.contains(name) {
                    self.error_hint(
                        "E_UNKNOWN_PACKAGE",
                        format!("`{name}` is in `{label}` but not in the package list"),
                        format!(
                            "add `{name}` to [packages] common or to a per-distro `add`, or \
                             remove it from `{label}`"
                        ),
                        None,
                    );
                }
            }
        }
        // A contradiction is refused, never settled by one list silently winning over another
        // (LD-377): a name this machine is told both to have and not to have, and a hold, a mark
        // or an optional on a name declared absent.
        for name in &manifest.packages.absent {
            if effective.contains(name) {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("`{name}` is declared for this machine and is in `absent`"),
                    format!(
                        "keep `{name}` in the package list or in `absent`, not both; a \
                         per-distro `remove` takes it out of `common` for one distribution"
                    ),
                    None,
                );
            }
            for (list, label) in [
                (&manifest.packages.hold, "hold"),
                (&manifest.packages.mark_auto, "mark_auto"),
                (&manifest.packages.optional, "optional"),
            ] {
                if list.contains(name) {
                    self.error_hint(
                        "E_ATTR_CONFLICT",
                        format!("`{name}` is in `{label}` and in `absent`"),
                        format!(
                            "`{name}` is absent, so it cannot also be in `{label}`: remove it \
                             from one of the two"
                        ),
                        None,
                    );
                }
            }
        }
        // A pin names a package this machine declares, and never one it declares absent: a name
        // in both is a contradiction, as a hold is (LD-377, LD-395).
        for name in manifest.packages.pins(&distro).keys() {
            if manifest.packages.absent.contains(name) {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("`{name}` is pinned and is in `absent`"),
                    format!(
                        "`{name}` is absent, so it cannot also be pinned: remove it from one of \
                         the two"
                    ),
                    None,
                );
            } else if !declared.contains(name) {
                self.error_hint(
                    "E_UNKNOWN_PACKAGE",
                    format!("`{name}` is pinned but not in the package list"),
                    format!(
                        "add `{name}` to [packages] common or to a per-distro `add`, or remove its \
                         pin"
                    ),
                    None,
                );
            }
        }
        let common: BTreeSet<&String> = manifest.packages.common.iter().collect();
        for (key, table) in manifest
            .packages
            .per_distro
            .iter()
            .chain(manifest.packages.per_arch.iter())
        {
            for name in &table.remove {
                if !common.contains(name) {
                    self.error_hint(
                        "E_UNKNOWN_PACKAGE",
                        format!("`{name}` is in `[packages.{key}] remove` but not in `common`"),
                        format!("take `{name}` out of this `remove`, or add it to `common`"),
                        None,
                    );
                }
            }
        }
    }

    /// `[services]`: `"UNIT" = "enabled"` or `"disabled"`, keyed by the unit's full name.
    fn read_services(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, bool> {
        let mut services = BTreeMap::new();
        for entry in entries(table) {
            if !is_unit_name(entry.key) {
                self.error_hint(
                    "E_TYPE",
                    format!("`{}` in [services] is not a systemd unit name", entry.key),
                    "name the unit in full, such as `ssh.service` or `fstrim.timer`".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let Some(value) = self.string(&entry, "[services]") else {
                continue;
            };
            match value.as_str() {
                "enabled" => services.insert(entry.key.to_string(), true),
                "disabled" => services.insert(entry.key.to_string(), false),
                _ => {
                    self.error_hint(
                        "E_TYPE",
                        format!("[services] \"{}\" = \"{value}\" is not a state", entry.key),
                        "a unit is `enabled` (enabled and running) or `disabled` (disabled and \
                         stopped)"
                            .into(),
                        Self::span_of(&entry),
                    );
                    continue;
                }
            };
        }
        services
    }

    fn identity_name(&mut self, entry: &Entry<'_>, table: &str) -> bool {
        if is_identity_name(entry.key) {
            return true;
        }
        self.error_hint(
            "E_IDENT",
            format!(
                "`{}` in [{table}] is not an account or group name",
                entry.key
            ),
            "use lowercase letters, digits, `_` and `-`, starting with a letter or `_`, at most \
             32 characters"
                .into(),
            entry.key_span.clone(),
        );
        false
    }

    /// A uid or gid: a whole number from 1 to 2147483646.
    fn id(&mut self, entry: &Entry<'_>, place: &str) -> Option<u32> {
        match entry.item.as_integer() {
            Some(n) if (1..=2_147_483_646).contains(&n) => u32::try_from(n).ok(),
            _ => {
                self.type_error(entry, place, "a whole number from 1 to 2147483646");
                None
            }
        }
    }

    /// An absolute path a passwd field can hold.
    fn passwd_path(&mut self, entry: &Entry<'_>, place: &str) -> Option<String> {
        let value = self.string(entry, place)?;
        match normalize_absolute(&value) {
            Some(path) if path == value && !value.contains([':', '\n', ',']) => Some(value),
            _ => {
                self.error_hint(
                    "E_PATH_ESCAPE",
                    format!("`{}` in {place} is not a plain absolute path", entry.key),
                    "write an absolute path with no `..`, `:` or `,`, such as \"/bin/bash\"".into(),
                    Self::span_of(entry),
                );
                None
            }
        }
    }

    /// A list of account or group names.
    fn names(&mut self, entry: &Entry<'_>, place: &str) -> Vec<String> {
        let names = self.string_array(entry, place);
        for name in &names {
            if !is_identity_name(name) {
                self.error_hint(
                    "E_IDENT",
                    format!(
                        "`{name}` in `{}` of {place} is not an account or group name",
                        entry.key
                    ),
                    "use lowercase letters, digits, `_` and `-`, starting with a letter or `_`"
                        .into(),
                    Self::span_of(entry),
                );
            }
        }
        names
    }

    fn read_users(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, UserEntry> {
        let mut users = BTreeMap::new();
        for entry in entries(table) {
            let place = format!("[users.{}]", entry.key);
            let Some(inner) = self.table(&entry, "[users]") else {
                continue;
            };
            if !self.identity_name(&entry, "users") {
                continue;
            }
            let mut user = UserEntry::default();
            for field in entries(inner) {
                match field.key {
                    "uid" => user.uid = self.id(&field, &place),
                    "groups" => user.groups = self.names(&field, &place),
                    "shell" => user.shell = self.passwd_path(&field, &place),
                    "home" => user.home = self.passwd_path(&field, &place),
                    "ssh_keys" => {
                        for key in self.string_array(&field, &place) {
                            match ssh_key_problem(&key) {
                                None => user.ssh_keys.push(key),
                                Some(why) => self.error_hint(
                                    "E_SSH_KEY",
                                    format!("an entry of `ssh_keys` in {place} {why}"),
                                    "write each public key as the one line of its .pub file".into(),
                                    Self::span_of(&field),
                                ),
                            }
                        }
                    }
                    key if PASSWORD_KEYS.contains(&key) => self.error_hint(
                        "E_UNKNOWN_ATTR",
                        format!("unknown key `{key}` in {place}"),
                        "lodi never writes a password: a new account's password is locked; \
                         set one on the machine with `passwd`, or log in with a key in \
                         `ssh_keys`"
                            .into(),
                        field.key_span.clone(),
                    ),
                    _ => self.unknown(&field, &place, USER_KEYS),
                }
            }
            users.insert(entry.key.to_string(), user);
        }
        users
    }

    fn read_groups(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, GroupEntry> {
        let mut groups = BTreeMap::new();
        for entry in entries(table) {
            let place = format!("[groups.{}]", entry.key);
            let Some(inner) = self.table(&entry, "[groups]") else {
                continue;
            };
            if !self.identity_name(&entry, "groups") {
                continue;
            }
            let mut group = GroupEntry::default();
            for field in entries(inner) {
                match field.key {
                    "gid" => group.gid = self.id(&field, &place),
                    "members" => group.members = self.names(&field, &place),
                    key if PASSWORD_KEYS.contains(&key) => self.error_hint(
                        "E_UNKNOWN_ATTR",
                        format!("unknown key `{key}` in {place}"),
                        "lodi never writes a group password".into(),
                        field.key_span.clone(),
                    ),
                    _ => self.unknown(&field, &place, GROUP_KEYS),
                }
            }
            groups.insert(entry.key.to_string(), group);
        }
        groups
    }

    /// `[system]` (sd-1): four strings, each held to the grammar of what it reaches.
    fn read_system(&mut self, table: &dyn toml_edit::TableLike) -> System {
        let mut system = System::default();
        for entry in entries(table) {
            if !SYSTEM_KEYS.contains(&entry.key) {
                self.unknown(&entry, "[system]", SYSTEM_KEYS);
                continue;
            }
            let Some(value) = self.string(&entry, "[system]") else {
                continue;
            };
            if !is_basic(entry.key, &value) {
                self.error_hint(
                    "E_TYPE",
                    format!(
                        "[system] {} = \"{value}\" is not a {}",
                        entry.key, entry.key
                    ),
                    basic_hint(entry.key).into(),
                    Self::span_of(&entry),
                );
                continue;
            }
            let slot = match entry.key {
                "hostname" => &mut system.hostname,
                "timezone" => &mut system.timezone,
                "locale" => &mut system.locale,
                _ => &mut system.keymap,
            };
            *slot = Some(value);
        }
        system
    }

    /// `[kernel]` (bc-1): three lists, each entry held to the grammar of where it is written.
    fn read_kernel(&mut self, table: &dyn toml_edit::TableLike) -> Kernel {
        let mut kernel = Kernel::default();
        for entry in entries(table) {
            let (slot, fits, hint): (_, fn(&str) -> bool, _) = match entry.key {
                "parameters" => (
                    &mut kernel.parameters,
                    is_parameter,
                    "a parameter is one word of letters, digits and `-_.,=:/+@%`, such as \
                     `quiet` or `mitigations=off`",
                ),
                "modules" | "blacklist" => (
                    if entry.key == "modules" {
                        &mut kernel.modules
                    } else {
                        &mut kernel.blacklist
                    },
                    is_module_name,
                    "a module is named as `lsmod` names it, such as `kvm` or `pcspkr`",
                ),
                _ => {
                    self.unknown(&entry, "[kernel]", KERNEL_KEYS);
                    continue;
                }
            };
            let values = self.string_array(&entry, "[kernel]");
            let mut fine = Vec::new();
            for value in values {
                if !fits(&value) {
                    self.error_hint(
                        "E_TYPE",
                        format!("`{value}` in [kernel] {} is not one", entry.key),
                        hint.into(),
                        Self::span_of(&entry),
                    );
                } else if !fine.contains(&value) {
                    fine.push(value);
                }
            }
            *slot = fine;
        }
        kernel
    }

    /// `[sysctl]` (bc-1): each key a setting's dotted name, each value a string or an integer.
    fn read_sysctl(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, String> {
        let mut sysctl = BTreeMap::new();
        for entry in entries(table) {
            let value = match entry.item.as_integer() {
                Some(number) => Some(number.to_string()),
                None if entry.item.is_str() => self.string(&entry, "[sysctl]"),
                None => {
                    self.type_error(&entry, "[sysctl]", "a string or an integer");
                    continue;
                }
            };
            let Some(value) = value else { continue };
            if !is_sysctl_name(entry.key) || !is_sysctl_value(&value) {
                self.error_hint(
                    "E_TYPE",
                    format!("[sysctl] \"{}\" = \"{value}\" is not a setting", entry.key),
                    "name a setting as `sysctl -a` does, such as `net.ipv4.ip_forward`, with a \
                     value on one line"
                        .into(),
                    Self::span_of(&entry),
                );
                continue;
            }
            sysctl.insert(entry.key.to_string(), value);
        }
        sysctl
    }

    /// `[boot]` (bl-1): `loader`, which it must name, and an optional `timeout` and `default`.
    fn read_boot(&mut self, at: &Entry<'_>, table: &dyn toml_edit::TableLike) -> Option<Boot> {
        let (mut loader, mut timeout, mut default) = (None, None, None);
        for entry in entries(table) {
            match entry.key {
                "loader" => match self.string(&entry, "[boot]") {
                    Some(name) => match Loader::parse(&name) {
                        Some(found) => loader = Some(found),
                        None => self.type_error(&entry, "[boot]", "\"grub\" or \"systemd-boot\""),
                    },
                    None => continue,
                },
                "timeout" => match entry.item.as_integer() {
                    Some(n) if (0..=600).contains(&n) => timeout = Some(n as u32),
                    _ => self.type_error(&entry, "[boot]", "a number of seconds, 0 to 600"),
                },
                "default" => match self.string(&entry, "[boot]") {
                    Some(value) if is_boot_default(&value) => default = Some(value),
                    Some(value) => self.error_hint(
                        "E_TYPE",
                        format!("`{value}` in [boot] default is not an entry"),
                        "name the entry as the loader does: `0` or `saved` for GRUB, an entry id \
                         or a pattern such as `debian-*` for systemd-boot"
                            .into(),
                        Self::span_of(&entry),
                    ),
                    None => continue,
                },
                _ => self.unknown(&entry, "[boot]", BOOT_KEYS),
            }
        }
        if loader.is_none() {
            self.error_hint(
                "E_TYPE",
                "[boot] names no loader".into(),
                "add `loader = \"grub\"` or `loader = \"systemd-boot\"`".into(),
                Self::span_of(at),
            );
        }
        Some(Boot {
            loader: loader?,
            timeout,
            default,
        })
    }

    /// `[firewall]` (sd-1): `input` and `allow`, each entry `PORT/PROTO` or `LOW-HIGH/PROTO`.
    fn read_firewall(&mut self, table: &dyn toml_edit::TableLike) -> Firewall {
        let mut firewall = Firewall {
            drop: true,
            allow: Vec::new(),
        };
        for entry in entries(table) {
            match entry.key {
                "input" => match self.string(&entry, "[firewall]").as_deref() {
                    Some("drop") => firewall.drop = true,
                    Some("accept") => firewall.drop = false,
                    Some(other) => self.error_hint(
                        "E_TYPE",
                        format!("[firewall] input = \"{other}\" is not a policy"),
                        "write `drop` (the default) or `accept`".into(),
                        Self::span_of(&entry),
                    ),
                    None => {}
                },
                "allow" => {
                    for rule in self.string_array(&entry, "[firewall]") {
                        match parse_allow(&rule) {
                            Some(allow) => firewall.allow.push(allow),
                            None => self.error_hint(
                                "E_TYPE",
                                format!("`{rule}` in [firewall] allow is not a port"),
                                "write `PORT/tcp`, `PORT/udp` or `LOW-HIGH/tcp`, such as \
                                 `22/tcp` or `8000-8100/udp`"
                                    .into(),
                                Self::span_of(&entry),
                            ),
                        }
                    }
                }
                _ => self.unknown(&entry, "[firewall]", FIREWALL_KEYS),
            }
        }
        firewall
    }

    /// `[network]` (sd-1): the confirmation, and one table per interface.
    fn read_network(&mut self, table: &dyn toml_edit::TableLike) -> Network {
        let mut network = Network {
            confirm_within: CONFIRM_WITHIN,
            check: None,
            interfaces: BTreeMap::new(),
        };
        for entry in entries(table) {
            match entry.key {
                "confirm_within" => match entry.item.as_integer() {
                    Some(n) if (1..=3600).contains(&n) => network.confirm_within = n as u64,
                    _ => self.type_error(&entry, "[network]", "a number of seconds, 1 to 3600"),
                },
                "check" => network.check = self.address(&entry, "[network]", false),
                "interfaces" => {
                    let Some(interfaces) = self.table(&entry, "[network]") else {
                        continue;
                    };
                    for one in entries(interfaces) {
                        if let Some(interface) = self.read_interface(&one) {
                            network.interfaces.insert(one.key.to_string(), interface);
                        }
                    }
                }
                _ => self.unknown(&entry, "[network]", NETWORK_KEYS),
            }
        }
        network
    }

    fn read_interface(&mut self, entry: &Entry<'_>) -> Option<Interface> {
        let inner = self.table(entry, "[network.interfaces]")?;
        if !is_interface_name(entry.key) {
            self.error_hint(
                "E_TYPE",
                format!(
                    "`{}` in [network.interfaces] is not an interface name",
                    entry.key
                ),
                "name the interface as `ip link` shows it, such as `enp0s2` or `eth0`".into(),
                entry.key_span.clone(),
            );
            return None;
        }
        let place = format!("[network.interfaces.{}]", entry.key);
        let mut interface = Interface {
            dhcp: true,
            addresses: Vec::new(),
            gateway: None,
            dns: Vec::new(),
        };
        for field in entries(inner) {
            match field.key {
                "dhcp" => interface.dhcp = self.boolean(&field, &place).unwrap_or(true),
                "addresses" => {
                    for address in self.string_array(&field, &place) {
                        if is_cidr(&address) {
                            interface.addresses.push(address);
                        } else {
                            self.error_hint(
                                "E_TYPE",
                                format!("`{address}` in {place} addresses is not an address"),
                                "write the address with its prefix, such as `192.168.1.10/24`"
                                    .into(),
                                Self::span_of(&field),
                            );
                        }
                    }
                }
                "gateway" => interface.gateway = self.address(&field, &place, false),
                "dns" => {
                    interface.dns = self
                        .address(&field, &place, true)
                        .map(|all| all.split(' ').map(str::to_string).collect())
                        .unwrap_or_default();
                }
                _ => self.unknown(&field, &place, INTERFACE_KEYS),
            }
        }
        if !interface.dhcp && interface.addresses.is_empty() {
            self.error_hint(
                "E_TYPE",
                format!("{place} has neither `dhcp = true` nor an address"),
                "declare `addresses`, or leave `dhcp` true".into(),
                entry.key_span.clone(),
            );
            return None;
        }
        Some(interface)
    }

    /// One IP address, or with `many` an array of them joined by spaces.
    fn address(&mut self, entry: &Entry<'_>, place: &str, many: bool) -> Option<String> {
        let values = if many {
            self.string_array(entry, place)
        } else {
            self.string(entry, place).into_iter().collect()
        };
        let mut good = Vec::new();
        for value in values {
            if value.parse::<std::net::IpAddr>().is_ok() {
                good.push(value);
            } else {
                self.error_hint(
                    "E_TYPE",
                    format!("`{value}` in {place} {} is not an IP address", entry.key),
                    "write an address such as `192.168.1.1`, without a prefix".into(),
                    Self::span_of(entry),
                );
            }
        }
        (!good.is_empty()).then(|| good.join(" "))
    }

    fn read_files(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, FileEntry> {
        let mut files: BTreeMap<String, FileEntry> = BTreeMap::new();
        for entry in entries(table) {
            let declared = entry.key.to_string();
            let place = format!("[files.\"{declared}\"]");
            let Some(inner) = self.table(&entry, "[files]") else {
                continue;
            };
            let normalized = match normalize_absolute(&declared) {
                Some(path) => path,
                None => {
                    self.error_hint(
                        "E_PATH_ESCAPE",
                        format!("`{declared}` in [files] is not an absolute path inside the root"),
                        "the key of a host `[files]` entry is an absolute path, and `..` may \
                         not leave the root"
                            .into(),
                        entry.key_span.clone(),
                    );
                    continue;
                }
            };
            if self.ctx.distro == "fedora" && super::pm::dnf::is_setting(&normalized) {
                self.error_hint(
                    "E_PROTECTED_PATH",
                    format!("`{declared}` is a Fedora package setting"),
                    "take the entry out; lodi never writes Fedora's package settings".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let mut file = FileEntry {
                declared: declared.clone(),
                path: normalized.clone(),
                content: None,
                source: None,
                mode: 0o644,
                state: FileState::Present,
                backup: true,
                on_remove: OnRemove::Restore,
                owner: "root".to_string(),
                group: "root".to_string(),
                kept_in_store: false,
            };
            for key in entries(inner) {
                match key.key {
                    "content" => file.content = self.string(&key, &place),
                    "source" => {
                        if let Some(value) = self.string(&key, &place) {
                            match relative_inside(&value) {
                                Some(rel) => file.source = Some(rel),
                                None => self.error_hint(
                                    "E_PATH_ESCAPE",
                                    format!("`source = \"{value}\"` in {place} leaves the manifest's own directory"),
                                    "`source` is a path relative to the manifest, inside its \
                                     directory tree"
                                        .into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "mode" => {
                        if let Some(value) = self.string(&key, &place) {
                            match parse_mode(&value) {
                                Some(mode) => file.mode = mode,
                                None => self.error_hint(
                                    "E_TYPE",
                                    format!("`mode = \"{value}\"` in {place} is not an octal mode"),
                                    "write it as a quoted octal string, for example \"0644\""
                                        .into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "state" => {
                        if let Some(value) = self.string(&key, &place) {
                            match value.as_str() {
                                "present" => file.state = FileState::Present,
                                "absent" => file.state = FileState::Absent,
                                _ => self.error_hint(
                                    "E_TYPE",
                                    format!("`state = \"{value}\"` in {place} is not a state"),
                                    "`state` is `present` or `absent`".into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "backup" => {
                        if let Some(value) = self.boolean(&key, &place) {
                            file.backup = value;
                        }
                    }
                    "on_remove" => {
                        if let Some(value) = self.string(&key, &place) {
                            match value.as_str() {
                                "restore" => file.on_remove = OnRemove::Restore,
                                "delete" => file.on_remove = OnRemove::Delete,
                                "keep" => file.on_remove = OnRemove::Keep,
                                _ => self.error_hint(
                                    "E_TYPE",
                                    format!("`on_remove = \"{value}\"` in {place} is not a value lodi knows"),
                                    "`on_remove` is `restore`, `delete` or `keep`".into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "owner" => {
                        if let Some(value) = self.string(&key, &place) {
                            file.owner = value;
                        }
                    }
                    "group" => {
                        if let Some(value) = self.string(&key, &place) {
                            file.group = value;
                        }
                    }
                    _ => self.unknown(&key, &place, FILE_KEYS),
                }
            }
            if file.state == FileState::Present {
                match (file.content.is_some(), file.source.is_some()) {
                    (true, true) => self.error_hint(
                        "E_ATTR_CONFLICT",
                        format!("{place} declares both `content` and `source`"),
                        "keep `content` for the text itself, or `source` to copy a file".into(),
                        entry.key_span.clone(),
                    ),
                    (false, false) => self.error_hint(
                        "E_ATTR_CONFLICT",
                        format!("{place} declares neither `content` nor `source`"),
                        "exactly one of them says what the file holds, or `state = \"absent\"` \
                         says the file must not be there"
                            .into(),
                        entry.key_span.clone(),
                    ),
                    _ => {}
                }
            }
            if let Some(first) = files.get(&normalized) {
                self.error_hint(
                    "E_DUP_RESOURCE",
                    format!(
                        "`{declared}` and `{}` are the same file, {normalized}",
                        first.declared
                    ),
                    "declare each file once: remove one of the two entries".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            files.insert(normalized, file);
        }
        files
    }

    /// One table's files into the manifest's: a path both `[etc]` and `[files]` declare is
    /// declared twice.
    fn merge_files(
        &mut self,
        files: &mut BTreeMap<String, FileEntry>,
        read: BTreeMap<String, FileEntry>,
        entry: &Entry<'_>,
    ) {
        for (path, file) in read {
            if let Some(first) = files.get(&path) {
                self.error_hint(
                    "E_DUP_RESOURCE",
                    format!(
                        "`{}` and `{}` are the same file, {path}",
                        file.declared, first.declared
                    ),
                    "declare each file once: remove one of the two entries".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            files.insert(path, file);
        }
    }

    /// `[etc."PATH"]` (si-1, LD-421): text the user wrote, at `/etc/PATH`, with an optional mode,
    /// owner and group. The path stays under `/etc`, and a path lodi never reads is refused.
    fn read_etc(&mut self, table: &dyn toml_edit::TableLike) -> BTreeMap<String, FileEntry> {
        let mut files = BTreeMap::new();
        for entry in entries(table) {
            let key = entry.key;
            let place = format!("[etc.\"{key}\"]");
            let Some(inner) = self.table(&entry, "[etc]") else {
                continue;
            };
            let path = normalize_absolute(&format!("/etc/{key}"))
                .filter(|path| !key.starts_with('/') && path.starts_with("/etc/"));
            let Some(path) = path else {
                self.error_hint(
                    "E_PATH_ESCAPE",
                    format!("`{key}` in [etc] is not a path under /etc"),
                    "the key is the path below /etc, as in [etc.\"motd\"] for /etc/motd".into(),
                    entry.key_span.clone(),
                );
                continue;
            };
            if super::import::files::never_captured(&path)
                || (self.ctx.distro == "fedora" && super::pm::dnf::is_setting(&path))
            {
                self.error_hint(
                    "E_PROTECTED_PATH",
                    format!("{place} names {path}, which lodi never writes from [etc]"),
                    "it holds an identity, a credential, a privilege or lodi's own state; \
                     take the entry out"
                        .into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let mut file = FileEntry {
                declared: path.clone(),
                path: path.clone(),
                content: None,
                source: None,
                mode: 0o644,
                state: FileState::Present,
                backup: true,
                on_remove: OnRemove::Restore,
                owner: "root".to_string(),
                group: "root".to_string(),
                kept_in_store: true,
            };
            for key in entries(inner) {
                match key.key {
                    "text" => file.content = self.string(&key, &place),
                    "mode" => {
                        if let Some(value) = self.string(&key, &place) {
                            match parse_mode(&value) {
                                Some(mode) => file.mode = mode,
                                None => self.error_hint(
                                    "E_TYPE",
                                    format!("`mode = \"{value}\"` in {place} is not an octal mode"),
                                    "write it as a quoted octal string, for example \"0644\""
                                        .into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "owner" => {
                        if let Some(value) = self.string(&key, &place) {
                            file.owner = value;
                        }
                    }
                    "group" => {
                        if let Some(value) = self.string(&key, &place) {
                            file.group = value;
                        }
                    }
                    _ => self.unknown(&key, &place, ETC_KEYS),
                }
            }
            if file.content.is_none() {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares no `text`"),
                    "`text` is what the file holds".into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            files.insert(path, file);
        }
        files
    }

    /// `[sources.NAME]` (LD-365): the deb822 fields, the keyring and its digest. Every problem
    /// is a manifest error found here, after `${…}` substitution and before anything below the
    /// root is read or written.
    fn read_sources(
        &mut self,
        table: &dyn toml_edit::TableLike,
        root_entry: &Entry<'_>,
    ) -> BTreeMap<String, SourceEntry> {
        let mut sources = BTreeMap::new();
        if self.ctx.distro == "arch" && !table.is_empty() {
            self.error_hint(
                "E_UNSUPPORTED",
                format!(
                    "`[sources]` is not a table this build applies on {}",
                    self.ctx.distro
                ),
                "a third-party pacman repository needs a key added to pacman's own keyring, \
                 which the host scope refuses to do; `[sources]` arms apt repositories on \
                 debian and ubuntu only"
                    .into(),
                root_entry.key_span.clone(),
            );
            return sources;
        }
        let fedora = self.ctx.distro == "fedora";
        let known = if fedora {
            FEDORA_SOURCE_KEYS
        } else {
            SOURCE_KEYS
        };
        for entry in entries(table) {
            let name = entry.key.to_string();
            let place = format!("[sources.{name}]");
            if !is_source_name(&name) {
                self.error_hint(
                    "E_IDENT",
                    format!("`{name}` in [sources] is not a source name"),
                    "a source name is [a-z0-9] then [a-z0-9-], at most 64 characters: it \
                     becomes the file names lodi-NAME.sources and lodi-NAME.gpg or .asc"
                        .into(),
                    entry.key_span.clone(),
                );
                continue;
            }
            let Some(inner) = self.table(&entry, "[sources]") else {
                continue;
            };
            let mut source = SourceEntry {
                name: name.clone(),
                types: vec!["deb".to_string()],
                uris: Vec::new(),
                suites: Vec::new(),
                components: Vec::new(),
                architectures: Vec::new(),
                signed_by: String::new(),
                signed_by_url: None,
                signed_by_sha256: String::new(),
                repo_gpgcheck: false,
            };
            let mut seen = std::collections::BTreeSet::new();
            let mut suites_span = None;
            let mut components_span = None;
            let mut erred = std::collections::BTreeSet::new();
            for key in entries(inner) {
                if known.contains(&key.key) {
                    seen.insert(key.key.to_string());
                }
                let before = self.diagnostics.len();
                match key.key {
                    other if !known.contains(&other) => self.unknown(&key, &place, known),
                    "gpgcheck" | "repo_gpgcheck" => match self.boolean(&key, &place) {
                        Some(true) => source.repo_gpgcheck |= key.key == "repo_gpgcheck",
                        Some(false) => self.error_hint(
                            "E_GPGCHECK_OFF",
                            format!("{place} turns the signature check `{}` off", key.key),
                            format!("leave `{}` out, or write `{} = true`", key.key, key.key),
                            Self::span_of(&key),
                        ),
                        None => {}
                    },
                    "types" => {
                        let values = self.deb822_values(&key, &place);
                        let mut kept: Vec<String> = Vec::new();
                        for (value, span) in values {
                            if value != "deb" && value != "deb-src" {
                                self.error_hint(
                                    "E_TYPE",
                                    format!("`types` in {place} names `{value}`, which is not an apt source type"),
                                    "`types` holds `deb`, `deb-src` or both".into(),
                                    span,
                                );
                            } else if kept.contains(&value) {
                                self.error_hint(
                                    "E_TYPE",
                                    format!("`types` in {place} names `{value}` twice"),
                                    "name each type once".into(),
                                    span,
                                );
                            } else {
                                kept.push(value);
                            }
                        }
                        if !kept.is_empty() {
                            source.types = kept;
                        }
                    }
                    "uris" => {
                        for (value, span) in self.deb822_values(&key, &place) {
                            if let Some((code, problem)) = uri_problem(&value) {
                                self.error_hint(
                                    code,
                                    format!("`{value}` in `uris` of {place} {problem}"),
                                    "a declared source is an https://HOST[/PATH] URI with no user \
                                     information, query or fragment; credentials for a repository \
                                     are never carried in a manifest"
                                        .into(),
                                    span,
                                );
                            } else {
                                source.uris.push(value);
                            }
                        }
                    }
                    "suites" => {
                        suites_span = Self::span_of(&key);
                        source.suites = self
                            .deb822_values(&key, &place)
                            .into_iter()
                            .map(|(value, _)| value)
                            .collect();
                    }
                    "components" => {
                        components_span = Self::span_of(&key);
                        source.components = self
                            .deb822_values(&key, &place)
                            .into_iter()
                            .map(|(value, _)| value)
                            .collect();
                    }
                    "architectures" => {
                        for (value, span) in self.deb822_values(&key, &place) {
                            if is_dpkg_architecture(&value) {
                                source.architectures.push(value);
                            } else {
                                self.error_hint(
                                    "E_TYPE",
                                    format!("`{value}` in `architectures` of {place} is not a dpkg architecture name"),
                                    "an architecture is [a-z0-9] then [a-z0-9-], such as `amd64` or \
                                     `arm64`"
                                        .into(),
                                    span,
                                );
                            }
                        }
                    }
                    "signed_by" => {
                        if let Some(value) = self.string(&key, &place) {
                            match relative_inside(&value) {
                                Some(rel) => source.signed_by = rel,
                                None => self.error_hint(
                                    "E_PATH_ESCAPE",
                                    format!("`signed_by = \"{value}\"` in {place} leaves the manifest's own directory"),
                                    "`signed_by` is a path relative to the manifest, inside its \
                                     directory tree, exactly as a `[files] source` is"
                                        .into(),
                                    Self::span_of(&key),
                                ),
                            }
                        }
                    }
                    "signed_by_url" => {
                        if let Some(value) = self.string(&key, &place) {
                            match keyring_url_refusal(&value) {
                                // The value is never echoed: it may carry a password.
                                Some((code, why)) => self.error_hint(
                                    code,
                                    format!("`signed_by_url` in {place} {why}"),
                                    "`signed_by_url` is an https:// URL with no user name, \
                                     password, query or fragment; a keyring that needs \
                                     credentials to fetch is not one lodi fetches"
                                        .into(),
                                    Self::span_of(&key),
                                ),
                                None => source.signed_by_url = Some(value),
                            }
                        }
                    }
                    "signed_by_sha256" => {
                        if let Some(value) = self.string(&key, &place) {
                            if is_sha256_hex(&value) {
                                source.signed_by_sha256 = value;
                            } else {
                                self.error_hint(
                                    "E_TYPE",
                                    format!(
                                        "`signed_by_sha256` in {place} is not a SHA-256 digest"
                                    ),
                                    "write the 64 lowercase hex digits of `sha256sum` over the \
                                     keyring file"
                                        .into(),
                                    Self::span_of(&key),
                                );
                            }
                        }
                    }
                    _ => self.unknown(&key, &place, known),
                }
                if self.diagnostics.len() > before {
                    erred.insert(key.key.to_string());
                }
            }
            for (what, cause) in [
                ("uris", "at least one https:// URI"),
                ("suites", "at least one suite"),
                ("signed_by_sha256", "the SHA-256 of the keyring bytes"),
            ] {
                if fedora && what == "suites" {
                    continue;
                }
                let empty = match what {
                    "uris" => source.uris.is_empty(),
                    "suites" => source.suites.is_empty(),
                    _ => false,
                };
                if !seen.contains(what) || (empty && !erred.contains(what)) {
                    self.error_hint(
                        "E_ATTR_CONFLICT",
                        format!("{place} declares no `{what}`"),
                        format!("a source declares `{what}`: {cause}"),
                        entry.key_span.clone(),
                    );
                }
            }
            // The keyring is named exactly once: by its path under the manifest's directory, or
            // by the URL the apply fetches it from (LD-366).
            let signed_by_seen = seen.contains("signed_by");
            if signed_by_seen == seen.contains("signed_by_url") {
                // Neither is worded as core's other missing keys are: `declares no `…``.
                let what = if signed_by_seen {
                    "both of `signed_by` and `signed_by_url`"
                } else {
                    "no `signed_by` and no `signed_by_url`"
                };
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares {what}"),
                    "a source names its keyring exactly once: `signed_by`, a file under the \
                     manifest's directory, or `signed_by_url`, an https:// URL lodi switch fetches"
                        .into(),
                    entry.key_span.clone(),
                );
            }
            // apt's own rule: a suite that ends in `/` is a flat repository and takes no
            // component; any other suite takes at least one. The two kinds are never mixed in one
            // table, because one `Components:` line cannot be right for both.
            let flat = source.suites.iter().filter(|s| s.ends_with('/')).count();
            if fedora {
                // A dnf repository has no suites and no components.
            } else if flat > 0 && flat < source.suites.len() {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} mixes flat suites (ending in `/`) with ordinary ones"),
                    "declare a flat repository and an ordinary one as two [sources] tables".into(),
                    suites_span.clone(),
                );
            } else if flat > 0 && seen.contains("components") {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares `components` for a flat repository"),
                    "a suite ending in `/` is a flat repository and takes no component; remove \
                     `components`"
                        .into(),
                    components_span.clone(),
                );
            } else if flat == 0 && !source.suites.is_empty() && source.components.is_empty() {
                self.error_hint(
                    "E_ATTR_CONFLICT",
                    format!("{place} declares no `components`"),
                    "a suite that is not a flat repository (ending in `/`) takes at least one \
                     component, such as `main` or `stable`"
                        .into(),
                    components_span.clone().or_else(|| entry.key_span.clone()),
                );
            }
            sources.insert(name, source);
        }
        sources
    }

    /// An array of strings bound for one deb822 field, each substituted and then checked for
    /// what would break the stanza it is written into (S3): whitespace, a control character, `#`
    /// or nothing at all is `E_TYPE` at the element itself. Returns every value that passed, with
    /// its place.
    fn deb822_values(
        &mut self,
        entry: &Entry<'_>,
        place: &str,
    ) -> Vec<(String, Option<Range<usize>>)> {
        let Some(array) = entry.item.as_array() else {
            self.type_error(entry, place, "an array of strings");
            return Vec::new();
        };
        let mut out = Vec::new();
        for value in array.iter() {
            let span = value.span().or_else(|| Self::span_of(entry));
            let Some(raw) = value.as_str() else {
                self.error(
                    "E_TYPE",
                    format!(
                        "every element of `{}` in {place} must be a string",
                        entry.key
                    ),
                    span,
                );
                continue;
            };
            let Some(text) = self.substitute(raw, span.clone()) else {
                continue;
            };
            if let Some(problem) = deb822_problem(&text) {
                // A `#` in a URI is S3's `E_TYPE`, and it is also a fragment S2 refuses by name:
                // the message names the fragment it found.
                if entry.key == "uris"
                    && problem == HASH_PROBLEM
                    && let Some((_, fragment)) = text.split_once('#')
                {
                    self.error_hint(
                        "E_TYPE",
                        format!(
                            "`{text}` in `uris` of {place} carries a fragment (`#{fragment}`); \
                             `#` starts a comment in a sources file"
                        ),
                        "a declared source is an https://HOST[/PATH] URI with no user \
                         information, query or fragment; drop the `#…` part"
                            .into(),
                        span,
                    );
                    continue;
                }
                self.error_hint(
                    "E_TYPE",
                    format!("a value of `{}` in {place} {problem}", entry.key),
                    "a deb822 field value is one word: no whitespace, no control character, no \
                     `#`, and never empty — after `${…}` substitution too"
                        .into(),
                    span,
                );
                continue;
            }
            out.push((text, span));
        }
        out
    }

    /// Two declarations that would own one thing: a `[files]` key under the `lodi-` namespace of
    /// `/etc/apt/sources.list.d` or `/etc/apt/keyrings`, which is `[sources]`'s own, and two
    /// `[sources]` tables that list one repository — one (URI, suite) pair, URIs compared after
    /// [`super::sourceset::normalize_uri`]. Each is `E_DUP_RESOURCE` naming both.
    fn cross_check_sources(&mut self, manifest: &HostManifest) {
        for (path, file) in &manifest.files {
            let Some(name) = super::sources::namespace_name(path) else {
                continue;
            };
            let other = if manifest.sources.contains_key(&name) {
                format!("[sources.{name}], which writes it")
            } else {
                "the lodi- namespace [sources] tables write under".to_string()
            };
            self.error_hint(
                "E_DUP_RESOURCE",
                format!("`{}` in [files] is under {other}", file.declared),
                "the lodi- files under /etc/apt/sources.list.d and /etc/apt/keyrings are a \
                 declared source's own; declare the repository as [sources.NAME] instead"
                    .into(),
                None,
            );
        }
        let mut owners: BTreeMap<(String, String), &str> = BTreeMap::new();
        let fedora = self.ctx.distro == "fedora";
        for (name, source) in &manifest.sources {
            let mut reported = std::collections::BTreeSet::new();
            // A dnf repository is its address alone.
            let suites = if fedora {
                vec![String::new()]
            } else {
                source.suites.clone()
            };
            for (uri, suite) in super::sourceset::pairs(&source.uris, &suites) {
                match owners.get(&(uri.clone(), suite.clone())) {
                    Some(first) if *first != name.as_str() => {
                        if reported.insert(*first) {
                            self.error_hint(
                                "E_DUP_RESOURCE",
                                format!(
                                    "[sources.{first}] and [sources.{name}] both list {}",
                                    format!("{uri} {suite}").trim_end()
                                ),
                                if fedora {
                                    "one repository is declared once".into()
                                } else {
                                    "apt refuses one repository with two Signed-By values; \
                                     declare it once"
                                        .into()
                                },
                                None,
                            );
                        }
                    }
                    Some(_) => {}
                    None => {
                        owners.insert((uri, suite), name.as_str());
                    }
                }
            }
        }
    }
}

/// `[a-z0-9]` then `[a-z0-9-]`, at most 64 characters: the name becomes part of two file names.
/// An account or group name every distribution's `useradd` and `groupadd` take: lowercase
/// letters, digits, `_` and `-`, starting with a letter or `_`, at most 32 characters.
pub fn is_identity_name(name: &str) -> bool {
    let mut chars = name.chars();
    name.len() <= 32
        && chars
            .next()
            .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Why `line` is not one public key an `authorized_keys` file can hold, or `None` when it is:
/// a known key type, its base64 body, and an optional comment, on one line.
pub fn ssh_key_problem(line: &str) -> Option<&'static str> {
    if line.contains("PRIVATE KEY") {
        return Some("is a private key");
    }
    if line.contains(['\n', '\r']) {
        return Some("spans more than one line");
    }
    let mut words = line.split(' ');
    let kind = words.next().unwrap_or_default();
    let body = words.next().unwrap_or_default();
    let security_key = SSH_SK_PREFIXES.iter().any(|prefix| {
        kind.strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('@'))
    });
    if !SSH_KEY_TYPES.contains(&kind) && !security_key {
        return Some("does not start with a public key type such as ssh-ed25519");
    }
    let base64 = body.len() >= 16
        && body
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'=');
    if !base64 {
        return Some("has no base64 key after its type");
    }
    None
}

pub fn is_source_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit() => {}
        _ => return false,
    }
    name.len() <= 64 && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `^[a-z0-9][a-z0-9-]*$`: a dpkg architecture name as a deb822 `Architectures:` value holds it.
fn is_dpkg_architecture(name: &str) -> bool {
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// [`deb822_problem`]'s `#`: a comment in a sources file, and in a URI a fragment too (S2).
const HASH_PROBLEM: &str = "contains `#`, which starts a comment in a sources file";

/// What would break a deb822 field if this value were written into it (S3), or `None`.
pub fn deb822_problem(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        Some("is empty")
    } else if value.chars().any(char::is_control) {
        Some("contains a control character, such as a line break")
    } else if value.chars().any(char::is_whitespace) {
        Some("contains whitespace")
    } else if value.contains('#') {
        Some(HASH_PROBLEM)
    } else {
        None
    }
}

/// Why a URI is not one a declared source may use (S2), as its code and the end of a sentence,
/// or `None` for `https://HOST[/PATH]`. The value has passed [`deb822_problem`] already.
pub fn uri_problem(uri: &str) -> Option<(&'static str, &'static str)> {
    let Some((scheme, rest)) = uri.split_once("://") else {
        return Some(("E_UNSUPPORTED", "is not an https:// URI"));
    };
    if !scheme.eq_ignore_ascii_case("https") {
        return Some((
            "E_UNSUPPORTED",
            "is not an https:// URI; a declared source is fetched over HTTPS only",
        ));
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Some((
            "E_UNSUPPORTED",
            "carries user information (user@ or user:password@)",
        ));
    }
    if authority.is_empty() {
        return Some(("E_UNSUPPORTED", "names no host"));
    }
    if rest.contains('?') {
        return Some(("E_UNSUPPORTED", "carries a query (`?…`)"));
    }
    if rest.contains('#') {
        return Some(("E_UNSUPPORTED", "carries a fragment (`#…`)"));
    }
    None
}

/// Why a `signed_by_url` is refused, as its code and the words after the key's name, or `None`
/// for an `https://` URL with a host and nothing else wrong with it (LD-366). The URL itself is
/// never part of the answer, because user information in it may be a password; a fragment is
/// named, as S2 names one in `uris`, and only once user information has been ruled out.
pub fn keyring_url_refusal(url: &str) -> Option<(&'static str, String)> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Some((
            "E_TYPE",
            "is empty or holds whitespace or a control character".into(),
        ));
    }
    let Some(rest) = url.strip_prefix("https://") else {
        let scheme = url
            .split_once("://")
            .map(|(scheme, _)| scheme)
            .filter(|scheme| {
                !scheme.is_empty()
                    && scheme
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
            });
        return Some((
            "E_INSECURE_URL",
            match scheme {
                Some(scheme) => format!("is not an HTTPS URL (its scheme is `{scheme}`)"),
                None => "is not an HTTPS URL".into(),
            },
        ));
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    if authority.contains('@') {
        return Some((
            "E_INSECURE_URL",
            "carries user information (a name or password before `@`); it is not repeated here"
                .into(),
        ));
    }
    if authority.is_empty() {
        return Some(("E_INSECURE_URL", "names no host".into()));
    }
    let (before, fragment) = match rest.split_once('#') {
        Some((before, fragment)) => (before, Some(fragment)),
        None => (rest, None),
    };
    if before.contains('?') {
        return Some(("E_TYPE", "carries a query (`?…`)".into()));
    }
    if let Some(fragment) = fragment {
        return Some((
            "E_TYPE",
            format!("carries a fragment (`#{fragment}`), which is never sent to the server"),
        ));
    }
    None
}

pub fn is_sha256_hex(text: &str) -> bool {
    text.len() == 64
        && text
            .chars()
            .all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

fn union(into: &mut Vec<String>, names: Vec<String>) {
    for name in names {
        if !into.contains(&name) {
            into.push(name);
        }
    }
}

/// Whether `value` is a `[system]` `key` lodi hands to its tool (sd-1): the characters each basic
/// is written in, never one that reads as an option, a path out of the zone directory or a
/// second argument. The same grammar holds what the record keeps to restore.
pub fn is_basic(key: &str, value: &str) -> bool {
    let (extra, most) = match key {
        "hostname" => ("-.", 64),
        "timezone" => ("-_+/", 64),
        "locale" => ("-_.@", 64),
        "keymap" => ("-_", 32),
        _ => return false,
    };
    !value.is_empty()
        && value.len() <= most
        && !value.starts_with(['-', '.', '/'])
        && !value.ends_with(['-', '/'])
        && !value.contains("..")
        && !value.contains("//")
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || extra.contains(c))
}

fn basic_hint(key: &str) -> &'static str {
    match key {
        "hostname" => "a hostname is letters, digits, `-` and `.`, at most 64, such as `box`",
        "timezone" => "name a zone as `timedatectl list-timezones` does, such as `Europe/Berlin`",
        "locale" => "name a locale as `localectl list-locales` does, such as `en_GB.UTF-8`",
        _ => "name a keyboard layout such as `us`, `de` or `fr`",
    }
}

/// `PORT/PROTO` or `LOW-HIGH/PROTO`, `tcp` or `udp`, ports 1 to 65535.
fn parse_allow(rule: &str) -> Option<(String, String)> {
    let (ports, proto) = rule.split_once('/')?;
    if !["tcp", "udp"].contains(&proto) {
        return None;
    }
    let port = |p: &str| {
        p.parse::<u16>()
            .ok()
            .filter(|n| *n > 0 && p == n.to_string())
    };
    match ports.split_once('-') {
        None => port(ports)?,
        Some((low, high)) if port(low)? < port(high)? => 0,
        Some(_) => return None,
    };
    Some((proto.to_string(), ports.to_string()))
}

/// A kernel parameter lodi writes into the loader's configuration (bc-1): one word that reads
/// the same inside double quotes in a shell file, never an option or a substitution.
pub fn is_parameter(value: &str) -> bool {
    (1..=256).contains(&value.len())
        && !value.starts_with('-')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_.,=:/+@%".contains(c))
}

/// A default entry lodi writes into a loader's file (bl-1): one line that reads the same inside
/// double quotes in a shell file, never an option or a substitution.
pub fn is_boot_default(value: &str) -> bool {
    (1..=128).contains(&value.len())
        && !value.starts_with(['-', ' '])
        && !value.ends_with(' ')
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || " -_.,:@*+>/=".contains(c))
}

/// A kernel module name, as `modprobe` takes it: 1 to 64 of `[A-Za-z0-9_-]`, not an option.
pub fn is_module_name(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && !name.starts_with('-')
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
}

/// A sysctl name: dotted words of `[A-Za-z0-9_-]`, none empty, not an option.
pub fn is_sysctl_name(name: &str) -> bool {
    (1..=128).contains(&name.len())
        && !name.starts_with('-')
        && name.split('.').all(|word| {
            !word.is_empty()
                && word
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
        })
}

/// A sysctl value: printable ASCII on one line, words separated by single spaces.
pub fn is_sysctl_value(value: &str) -> bool {
    (1..=256).contains(&value.len())
        && value.chars().all(|c| c.is_ascii_graphic() || c == ' ')
        && value.split(' ').all(|word| !word.is_empty())
}

/// A kernel interface name: 1 to 15 of `[A-Za-z0-9_.-]`, not starting with `-` or `.`.
pub fn is_interface_name(name: &str) -> bool {
    (1..=15).contains(&name.len())
        && !name.starts_with(['-', '.'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// `ADDRESS/PREFIX`, IPv4 or IPv6.
fn is_cidr(value: &str) -> bool {
    let Some((address, prefix)) = value.split_once('/') else {
        return false;
    };
    let most = match address.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(_)) => 32,
        Ok(std::net::IpAddr::V6(_)) => 128,
        Err(_) => return false,
    };
    prefix
        .parse::<u8>()
        .is_ok_and(|p| p <= most && prefix == p.to_string())
}

/// The unit suffixes `[services]` takes: units an administrator enables, disables and starts.
pub const UNIT_SUFFIXES: &[&str] = &[".service", ".socket", ".timer", ".path"];

/// A systemd unit name `[services]` may declare: systemd's own characters, a known suffix, and
/// nothing that reads as an option or a path. It reaches `systemctl`'s argv, after `--`.
pub fn is_unit_name(name: &str) -> bool {
    let Some(stem) = UNIT_SUFFIXES.iter().find_map(|s| name.strip_suffix(s)) else {
        return false;
    };
    !stem.is_empty()
        && name.len() <= 255
        && !name.starts_with(['-', '.', '@'])
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ":-_.@\\".contains(c))
}

/// `[A-Za-z0-9]` then `[A-Za-z0-9.+_@-]`: the union of what Debian and Arch accept, without a
/// version, an architecture qualifier or a path.
pub fn is_package_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= 255
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '_' | '@' | '-'))
}

/// Normalize an absolute manifest key lexically. `None` when it is not absolute or when `..`
/// would leave the root.
pub fn normalize_absolute(key: &str) -> Option<String> {
    if !key.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in Path::new(key).components() {
        match component {
            Component::RootDir => {}
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_str()?),
            Component::Prefix(_) => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(format!("/{}", parts.join("/")))
}

/// A `source` path: relative, and inside the manifest's own directory.
pub fn relative_inside(value: &str) -> Option<String> {
    if value.starts_with('/') || value.starts_with('~') {
        return None;
    }
    let mut parts: Vec<&str> = Vec::new();
    for component in Path::new(value).components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(part) => parts.push(part.to_str()?),
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// Resolve a `source` against the manifest's directory. The caller has already checked with
/// [`relative_inside`] that it does not leave that directory.
pub fn source_path(manifest_dir: &Path, source: &str) -> PathBuf {
    manifest_dir.join(source)
}

fn parse_mode(text: &str) -> Option<u32> {
    if text.is_empty() || text.len() > 5 || !text.chars().all(|c| ('0'..='7').contains(&c)) {
        return None;
    }
    u32::from_str_radix(text, 8).ok().filter(|m| *m <= 0o7777)
}
