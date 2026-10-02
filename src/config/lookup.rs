//! The real [`Lookup`] behind the config lock (#696, LD-499): a host section from the dated
//! archive (the `[host] snapshot`, every `[packages.pin]`) and each `signed_by_url` key, and a
//! home section from the tools' upstreams. `lodi update`, `pin` and `unpin` call it, and so do
//! `lodi import` (fill) and `lodi switch --update` (refresh).
//!
//! The machine a host section is for is the root's own (`<root>/etc/os-release`): its
//! distribution, release and architecture, read with no package manager and no permission to
//! manage the machine. A manifest's text can be given in place of its file ([`Archive::with_text`]),
//! so a command looks up an edit before it writes it; a refresh that moves `[host] snapshot`
//! leaves the moved text in [`Archive::moved`], for the caller to write with the lock.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::lock::{HomeSection, HostSection, Lookup, Mode};
use crate::diag::Diagnostic;
use crate::fetch::Fetcher;
use crate::hostscope::manifest::{self, HostManifest};
use crate::hostscope::pin::verbs::rewrite;
use crate::hostscope::pin::{self, Context, PinsLock, Request};
use crate::hostscope::safety::{Distro, OsRelease};

/// The machine a host section is resolved for.
#[derive(Debug, Clone)]
pub struct Machine {
    pub root: PathBuf,
    pub os: OsRelease,
    pub distro: Distro,
}

impl Machine {
    /// The root's distribution, from its `etc/os-release`.
    pub fn of(root: &Path) -> Result<Machine, Diagnostic> {
        let (os, distro) = crate::hostscope::safety::read_distro(root)?;
        Ok(Machine {
            root: root.to_path_buf(),
            os,
            distro,
        })
    }

    /// The release a section records: the codename, `rolling` on Arch, the version on Fedora.
    pub fn release(&self) -> &str {
        match self.distro {
            Distro::Arch => "rolling",
            Distro::Fedora => &self.os.version_id,
            _ => &self.os.codename,
        }
    }

    pub fn arch(&self) -> &'static str {
        std::env::consts::ARCH
    }

    /// `text`, the host manifest shown as `shown`, parsed for this machine.
    pub fn parse(&self, text: &str, shown: &str) -> Result<HostManifest, Diagnostic> {
        let ctx = manifest::Context::of(self.arch(), self.distro, &self.os);
        let parsed = manifest::parse(text, shown, &ctx).map_err(|errors| {
            let mut errors = errors.diagnostics.into_iter();
            let mut first = errors
                .next()
                .unwrap_or_else(|| Diagnostic::new("E_SYNTAX", format!("{shown} is not valid")));
            first
                .notes
                .extend(errors.map(|d| format!("{}: {}", d.code, d.message)));
            first
        })?;
        self.os
            .assert_distro(parsed.host.distro.as_deref(), shown)?;
        Ok(parsed)
    }

    pub fn context<'a>(
        &'a self,
        sources: &'a BTreeMap<String, manifest::SourceEntry>,
        now: i64,
    ) -> Context<'a> {
        Context {
            distro: self.distro,
            codename: self.release(),
            arch: self.arch(),
            sources,
            now,
        }
    }
}

/// The fetcher every lookup reads the archive through, configured from the environment.
pub fn http() -> Result<Box<dyn Fetcher>, Diagnostic> {
    crate::fetch::HttpFetcher::from_env()
        .map(|fetcher| Box::new(fetcher) as Box<dyn Fetcher>)
        .map_err(|error| Diagnostic::new("E_CONFIG", error))
}

/// The lookups of one config, for one machine and one instant.
pub struct Archive {
    config: PathBuf,
    machine: Machine,
    /// The person's home folder, which a home manifest's paths render against.
    home: PathBuf,
    now: i64,
    texts: BTreeMap<String, String>,
    moved: BTreeMap<String, String>,
}

impl Archive {
    pub fn new(config: &Path, machine: Machine, home: &Path, now: i64) -> Archive {
        Archive {
            config: config.to_path_buf(),
            machine,
            home: home.to_path_buf(),
            now,
            texts: BTreeMap::new(),
            moved: BTreeMap::new(),
        }
    }

    pub fn machine(&self) -> &Machine {
        &self.machine
    }

    /// Look the manifest `key` up from `text` instead of its file.
    pub fn with_text(&mut self, key: &str, text: String) {
        self.texts.insert(key.to_string(), text);
    }

    /// The host manifests a refresh changed, by key: their `[host] snapshot` moved.
    pub fn moved(&self) -> &BTreeMap<String, String> {
        &self.moved
    }

    /// The text of the manifest `key`: the one given, else its file.
    pub fn text(&self, key: &str) -> Result<String, Diagnostic> {
        if let Some(text) = self.texts.get(key) {
            return Ok(text.clone());
        }
        let path = self.config.join(key);
        let bytes = std::fs::read(&path).map_err(|e| {
            Diagnostic::new(
                "E_NO_MANIFEST",
                format!("cannot read {}: {e}", path.display()),
            )
        })?;
        String::from_utf8(bytes)
            .map_err(|_| Diagnostic::new("E_SYNTAX", format!("{} is not UTF-8", path.display())))
    }

    fn shown(&self, key: &str) -> String {
        self.config.join(key).display().to_string()
    }
}

impl Lookup for Archive {
    fn host(
        &mut self,
        key: &str,
        current: Option<&HostSection>,
        mode: Mode,
    ) -> Result<HostSection, Diagnostic> {
        let shown = self.shown(key);
        let mut text = self.text(key)?;
        let mut parsed = self.machine.parse(&text, &shown)?;
        let distro = self.machine.distro.name();
        if mode == Mode::Refresh && parsed.host.snapshot.is_some() {
            let value = rewrite::default_snapshot(distro, self.now)?;
            let edit = rewrite::set_snapshot(&text, Some(&value))?;
            if edit.changed {
                text = edit.text;
                parsed = self.machine.parse(&text, &shown)?;
                self.moved.insert(key.to_string(), text.clone());
            }
        }
        let machine = &self.machine;
        let cx = machine.context(&parsed.sources, self.now);
        let mut work = PinsLock::new(distro, machine.release(), machine.arch());
        if let Some(current) = current.filter(|c| {
            c.distro == distro && c.release == machine.release() && c.arch == machine.arch()
        }) {
            work.snapshot = current.snapshot.clone();
            work.pins = current.pins.clone();
        }
        let declared = parsed.packages.pins(distro);
        if machine.distro == Distro::Fedora {
            fedora_record(machine, &declared, &mut work)?;
        }
        let mut resolution = pin::resolve(
            &cx,
            parsed.host.snapshot.as_deref(),
            &declared,
            Some(&work),
            &mut http,
        )?;
        if let Some(snapshot) = resolution.snapshot.as_mut()
            && snapshot.indexes.is_empty()
            && !resolution.fetched.contains_key(&snapshot.instant)
        {
            snapshot.indexes =
                pin::index_digests(http()?.as_ref(), cx.distro, cx.codename, snapshot.instant)?;
        }
        let lock = resolution.lock(distro, cx.codename, cx.arch);
        let reuse = (mode == Mode::Fill).then_some(current).flatten();
        Ok(HostSection {
            distro: lock.distro,
            release: lock.release,
            arch: lock.arch,
            snapshot: lock.snapshot,
            pins: lock.pins,
            keys: keys(&parsed, reuse)?,
        })
    }

    fn home(
        &mut self,
        key: &str,
        current: Option<&HomeSection>,
        _mode: Mode,
    ) -> Result<HomeSection, Diagnostic> {
        let path = self.config.join(key);
        let shown = path.display().to_string();
        let text = self.text(key)?;
        let folder = path.parent().unwrap_or(&self.config).to_path_buf();
        let base = crate::roots::Roots::for_host(self.home.clone(), folder).config_root();
        let parsed =
            crate::home::manifest::parse_home_manifest_bytes(text.as_bytes(), &shown, &base)
                .map_err(|errors| {
                    errors.diagnostics.into_iter().next().unwrap_or_else(|| {
                        Diagnostic::new("E_SYNTAX", format!("{shown} is not valid"))
                    })
                })?;
        let previous = current.map(|section| crate::lock::LockFile {
            version: crate::lock::LOCK_VERSION,
            format: crate::lock::LOCK_FORMAT.to_string(),
            generated_by: format!("lodi {}", env!("CARGO_PKG_VERSION")),
            manifest_hash: crate::lock::tool_set_hash(&parsed.tools),
            base: None,
            packages: section.tools.clone(),
            profiles: section.profiles.clone(),
        });
        let fetcher = http()?;
        let locked = crate::home::tools::locked_set(
            parsed.tools,
            previous.as_ref(),
            fetcher.as_ref(),
            self.now,
        )
        .map_err(|failure| {
            let mut diagnostics = failure.diagnostics.into_iter();
            let mut first = diagnostics
                .next()
                .unwrap_or_else(|| Diagnostic::new("E_FETCH", "the tools could not be resolved"));
            first.message = format!("{shown}: {}", first.message);
            first
        })?;
        Ok(
            locked.map_or_else(HomeSection::default, |lock| HomeSection {
                tools: lock.packages,
                profiles: lock.profiles,
            }),
        )
    }
}

/// Every `signed_by_url` key's `sha256:` digest, fetched and checked against the declared one,
/// or taken from `current` when it already records exactly that digest.
fn keys(
    parsed: &HostManifest,
    current: Option<&HostSection>,
) -> Result<BTreeMap<String, String>, Diagnostic> {
    let mut keys = BTreeMap::new();
    let mut wanted = Vec::new();
    for (name, source) in &parsed.sources {
        let Some(url) = &source.signed_by_url else {
            continue;
        };
        let declared = format!("sha256:{}", source.signed_by_sha256);
        if current.and_then(|c| c.keys.get(name)) == Some(&declared) {
            keys.insert(name.clone(), declared);
        } else {
            wanted.push((name.clone(), url.clone()));
        }
    }
    if !wanted.is_empty() {
        let fetched = crate::hostscope::sources::fetch(parsed, &wanted, http()?.as_ref())?;
        for (name, url) in wanted {
            let digest = crate::util::sha256_hex(&fetched[&url]);
            keys.insert(name, format!("sha256:{digest}"));
        }
    }
    Ok(keys)
}

/// On Fedora a version pin `work` does not record is resolved from a configured repository;
/// Fedora has no dated archive.
fn fedora_record(
    machine: &Machine,
    declared: &BTreeMap<String, pin::Declared>,
    work: &mut PinsLock,
) -> Result<(), Diagnostic> {
    for (name, wanted) in declared {
        let Request::Version(value) = &wanted.request else {
            continue;
        };
        if work
            .pins
            .get(name)
            .is_some_and(|r| r.requested == wanted.requested && r.policy == "version")
        {
            continue;
        }
        let build = pin::fedora::parse_value(name, value)?;
        let operation = crate::hostscope::safety::Operation::Pin;
        let mut resolved = pin::fedora::lookup(&machine.root, operation, name, &build)?;
        resolved.requested = wanted.requested.clone();
        work.pins.insert(name.clone(), resolved.record());
    }
    Ok(())
}
