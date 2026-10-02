//! `lodi switch` (#695, slice 1 of 2; #709 elevates): the host part, then the invoking person's
//! home part, of the config discovery finds, previewed first.
//!
//! Order: find and read the config; lock what is missing (or, with `--update`, refresh as
//! `lodi update` does), in memory; plan both parts; print the preview and the git line on
//! standard error; stop for `--dry-run`, a declined `--ask`, or a host part with changes that
//! cannot get root (`E_NEED_ROOT`), each having written nothing; write the lock as the person;
//! run the host part (today's host apply, on the "may manage" marker of #705 and the config's pins) —
//! when not root, in lodi's hidden entry through the first of sudo, doas and run0 (#709,
//! [`crate::elevate`]), which plans again as root and applies only the plan previewed — then,
//! if it worked, the home part as the person (today's home apply for one home, under `sudo` or
//! `doas` in a child that becomes the person). Nothing goes to standard output.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::io::{self, BufRead, IsTerminal, Write as _};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};

use super::fetched::{self, Fetched};
use super::lock::{self, HomeSection, Lock};
use super::lookup::{Archive, Machine};
use super::verbs::{Invocation, commit, plan_update};
use super::{Config, Found, Location, Part, Parts, Selection, User};
use crate::diag::Diagnostic;
use crate::elevate::{self, Context, Elevation, Request};
use crate::home::manifest::HomeManifest;
use crate::hostscope::plan::{Kind, Op};
use crate::hostscope::remote::Ask;
use crate::hostscope::safety::{ConfigPins, Operation, Options};
use crate::hostscope::{self, HostError};
use crate::progress::{self, Event, Progress, Sink};
use crate::roots::Roots;
use serde::Deserialize;
use serde_json::{Value, json};

/// What `lodi switch` was given besides the config's path.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flags {
    pub ask: bool,
    pub dry_run: bool,
    /// `--host`: only the host part.
    pub host_only: bool,
    /// `--home`: only the home part.
    pub home_only: bool,
    pub update: bool,
    pub overwrite_drift: bool,
    pub resolved: Option<String>,
    pub git_ref: Option<String>,
    pub rev: Option<String>,
    pub refresh: bool,
    /// `--root DIR`, the scratch root of every test (LD-114).
    pub root: Option<PathBuf>,
}

impl Flags {
    /// The URL flags given, by name.
    fn url_flags(&self) -> Vec<&'static str> {
        let mut named = Vec::new();
        if self.git_ref.is_some() {
            named.push("--ref");
        }
        if self.rev.is_some() {
            named.push("--rev");
        }
        if self.refresh {
            named.push("--refresh");
        }
        named
    }
}

/// Why a switch stopped: a usage error (status 2), or a failure with its code.
#[derive(Debug)]
pub enum Stopped {
    Usage(String),
    Failed(HostError),
}

impl From<Diagnostic> for Stopped {
    fn from(d: Diagnostic) -> Stopped {
        Stopped::Failed(d.into())
    }
}

impl From<HostError> for Stopped {
    fn from(error: HostError) -> Stopped {
        Stopped::Failed(error)
    }
}

/// The fixed warning lines (#689).
pub const DIRTY: &str = "config has uncommitted changes; git can't take you back to this";
pub const NOT_IN_GIT: &str = "config isn't in git; nothing to go back to";

/// The config's git state, read once before the preview: the short hash of `HEAD`, with
/// `+uncommitted` when files under the config root differ, and the one warning line.
struct Git {
    commit: Option<String>,
    warning: Option<&'static str>,
}

impl Git {
    fn read(dir: &Path, user: &User) -> Git {
        let run = |args: &[&str]| -> Option<String> {
            let mut command = std::process::Command::new("git");
            command
                .arg("-C")
                .arg(dir)
                .args(args)
                .stdin(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            if let Some((uid, gid)) = super::home_owner(user) {
                command.uid(uid).gid(gid);
            }
            let output = command.output().ok()?;
            output.status.success().then(|| {
                String::from_utf8_lossy(&output.stdout)
                    .trim_end()
                    .to_string()
            })
        };
        if run(&["rev-parse", "--is-inside-work-tree"]).as_deref() != Some("true") {
            return Git {
                commit: None,
                warning: Some(NOT_IN_GIT),
            };
        }
        let head = run(&["rev-parse", "--short", "HEAD"]);
        let dirty = run(&["status", "--porcelain", "--untracked-files=all", "--", "."])
            .is_none_or(|status| !status.is_empty());
        match head {
            Some(head) if dirty => Git {
                commit: Some(format!("{head}+uncommitted")),
                warning: Some(DIRTY),
            },
            Some(head) => Git {
                commit: Some(head),
                warning: None,
            },
            None => Git {
                commit: None,
                warning: Some(DIRTY),
            },
        }
    }

    /// ` (a1b2c3d)`, or nothing.
    fn suffix(&self) -> String {
        self.commit
            .as_ref()
            .map_or_else(String::new, |commit| format!(" ({commit})"))
    }
}

/// The home part, read and planned.
struct Home {
    part: Part,
    roots: crate::roots::Roots,
    manifest: crate::home::manifest::HomeManifest,
    lock: Option<crate::lock::LockFile>,
    /// The plan's lines, and what would change: files, tools and services, each counted.
    lines: Vec<String>,
    changes: Counts,
    /// The plan's `W_…` lines, printed after the count.
    warnings: Vec<String>,
    /// The lines every run prints last: the rc-file lines.
    tail: Vec<String>,
    /// Whether the tools' bin folder is on `PATH`, which a preview reports.
    on_path: Option<String>,
    /// Lingering the plan turns on or off, which root does when the switch has root.
    linger: Option<bool>,
}

/// What the home part's planning hands back, from a child under `sudo` too: JSON over a pipe,
/// never a file.
#[derive(Debug, Default, Deserialize)]
struct Planned {
    changes: Counts,
    lines: Vec<String>,
    warnings: Vec<String>,
    tail: Vec<String>,
    on_path: Option<String>,
    #[serde(default)]
    linger: Option<bool>,
}

/// What a home plan changes, each kind counted on its own (#695 story 51).
#[derive(Debug, Clone, Copy, Default, Deserialize)]
struct Counts {
    files: usize,
    tools: usize,
    services: usize,
}

impl Counts {
    fn total(&self) -> usize {
        self.files + self.tools + self.services
    }

    /// The home steps with something to do, in their order (#694 story 7).
    fn steps(&self) -> impl Iterator<Item = &'static str> + use<> {
        let [tools, files, services] = crate::home::apply::STEPS;
        [
            (tools, self.tools),
            (files, self.files),
            (services, self.services),
        ]
        .into_iter()
        .filter(|(_, n)| *n > 0)
        .map(|(name, _)| name)
    }

    /// `1 file, 2 tools`, or `no changes`.
    fn shown(&self) -> String {
        let one = |n: usize, word: &str| match n {
            1 => format!("1 {word}"),
            n => format!("{n} {word}s"),
        };
        let parts: Vec<String> = [
            (self.files, "file"),
            (self.tools, "tool"),
            (self.services, "service"),
        ]
        .into_iter()
        .filter(|(n, _)| *n > 0)
        .map(|(n, word)| one(n, word))
        .collect();
        match parts.is_empty() {
            true => "no changes".to_string(),
            false => parts.join(", "),
        }
    }
}

/// A sink that passes on only the steps in `steps`: a home step with nothing to do is not
/// shown (#694 story 7).
struct Only<'a> {
    steps: Vec<&'static str>,
    inner: &'a mut dyn Sink,
    skipping: bool,
}

impl Sink for Only<'_> {
    fn event(&mut self, event: Event) {
        if let Event::Begin { name, .. } = &event {
            self.skipping = !self.steps.contains(&name.as_str());
        }
        match &event {
            // A failure is always shown, whichever step it ends.
            Event::Fail { .. } => self.inner.event(event),
            Event::Finish { .. } if self.skipping => self.skipping = false,
            _ if self.skipping => {}
            _ => self.inner.event(event),
        }
    }

    fn tick(&mut self) {
        self.inner.tick();
    }
}

/// Make the config's own `recipes/` the ones this run resolves tools through, before the built-in
/// ones (LD-409, LD-524): read through the config's walk, as the person, its warnings printed
/// once. For a fetched config too, read from its tree.
pub(super) fn recipes(config: &Config, user: &User) {
    let owners = crate::hostscope::source::owners_for(user.uid, None);
    let host = crate::hostscope::source::Host {
        anchor: config.root.clone(),
        dir: config.root.clone(),
        source: config.root.display().to_string(),
        private: super::private_groups(user, &owners),
        owners,
        named: None,
    };
    crate::catalogue::user::install(crate::catalogue::user::read(&host));
}

/// `lodi switch` for `inv`, with `flags`.
pub fn run(inv: &Invocation, flags: &Flags) -> Result<(), Stopped> {
    let switch = Switch::open(inv, flags)?;
    let read = switch.read_home()?;
    let locked = switch.lock(read.as_ref().map(|(_, manifest)| manifest))?;
    let plans = switch.plan(&locked, read)?;
    let warnings = switch.warnings(&locked);
    let preview = switch.preview(&plans)?;
    if !plans.host_changes && !plans.home_changes {
        return switch.nothing_to_switch(&locked, &plans, &warnings);
    }
    eprint!("{}", preview.text);
    for warning in &warnings {
        eprintln!("lodi: warning: {warning}");
    }
    if let Some(commit) = &switch.git.commit {
        eprintln!("config: {commit}");
    }
    if flags.dry_run {
        print_tail(plans.home.as_ref(), true);
        return Ok(());
    }
    let elevation = switch.settle(&locked, &plans)?;
    // Every refusal is past: the lock, as the person, before the host part reads it as root.
    // What it replaces is kept, to put back if the root run never starts (a refused password).
    let before = switch.kept_before(&locked);
    switch.write_lock(&locked)?;
    switch.apply(&plans, elevation, &before, &preview)?;
    switch.remember(&locked.new);
    Ok(())
}

/// The elevation of a host part with changes, and the request it carries.
type Elevated = (Elevation, String);

/// A switch, once its config is read and the refusals that need no lookup are past.
struct Switch<'a> {
    inv: &'a Invocation,
    flags: &'a Flags,
    found: Found,
    fetched: Option<Fetched>,
    config: Config,
    user: User,
    selection: Selection,
    git: Git,
    now: i64,
}

/// The lock a switch holds: `old` as read, `new` with what was missing filled in (or with
/// `--update`, refreshed), in memory until [`Switch::write_lock`].
struct Locked {
    held: lock::Held,
    old: Lock,
    new: Lock,
    texts: BTreeMap<PathBuf, String>,
}

/// Both parts planned.
struct Plans {
    released: BTreeMap<String, crate::hostscope::pin::PinRecord>,
    options: Option<Options>,
    loaded: Option<hostscope::Loaded>,
    home: Option<Home>,
    host_changes: bool,
    home_changes: bool,
}

/// The preview's lines, and each part's count for the summary.
struct Preview {
    text: String,
    host_count: String,
    home_count: String,
}

/// The config's folder: a local one, or a URL's checkout, fetched as `--ref`, `--rev` and
/// `--refresh` ask.
fn locate(found: &Found, flags: &Flags) -> Result<(PathBuf, Option<Fetched>), Stopped> {
    match &found.location {
        Location::Local(dir) => {
            if let Some(flag) = flags.url_flags().first() {
                return Err(Stopped::Usage(format!(
                    "`switch {flag}` chooses what is fetched from a git URL, and {} is a folder",
                    dir.display()
                )));
            }
            Ok((dir.clone(), None))
        }
        Location::Url(url) if flags.update => Err(Diagnostic::new(
            "E_UNSUPPORTED",
            format!(
                "{url} is a fetched config, which lodi never writes, so --update cannot refresh \
                 its pins"
            ),
        )
        .hint("update a checkout of it instead (lodi update PATH), commit and push")
        .into()),
        Location::Url(url) => {
            let ask = Ask {
                git_ref: flags.git_ref.as_deref(),
                rev: flags.rev.as_deref(),
                refresh: flags.refresh,
            };
            let user = &found.user;
            let fetched = fetched::fetch(url, ask, user, super::home_owner(user))?;
            Ok((fetched.dir.clone(), Some(fetched)))
        }
    }
}

/// The parts of this host the config declares and `--host` or `--home` asks for; a part left
/// out is never opened, nor its path judged.
fn select(config: &Config, flags: &Flags, user: &User) -> Result<Selection, Diagnostic> {
    let selection = config.this_host_parts(Parts {
        host: !flags.home_only,
        home: !flags.host_only,
    })?;
    let login = user.login.clone().unwrap_or_else(|| user.uid.to_string());
    let root = config.root.display();
    let (why, hint) = match (&selection.host, &selection.home) {
        (None, _) if flags.host_only => (
            format!("{root} declares no host for this machine, and --host runs only the host part"),
            "drop --host, or import this host with `lodi import`",
        ),
        (_, None) if flags.home_only => (
            format!("{root} declares no home for {login}, and --home runs only the home part"),
            "drop --home, or import your home with `lodi import --home`",
        ),
        (None, None) => (
            format!("{root} declares neither a host for this machine nor a home for {login}"),
            "import them with `lodi import`",
        ),
        _ => return Ok(selection),
    };
    Err(Diagnostic::new("E_NO_MANIFEST", why).hint(hint))
}

impl<'a> Switch<'a> {
    /// Find and read the config; stop for a part it lacks and for a host lodi may not manage.
    fn open(inv: &'a Invocation, flags: &'a Flags) -> Result<Switch<'a>, Stopped> {
        let found = super::find(inv.typed.as_deref(), inv.env.as_deref(), &inv.identity)?;
        let (dir, fetched) = locate(&found, flags)?;
        let user = found.user.clone();
        let config = super::read_layout(&dir, &user)?;
        recipes(&config, &user);
        let selection = select(&config, flags, &user)?;
        let euid = inv.identity.euid;
        // `sudo -u` to another user than root acts on that user's home, not on the invoker's:
        // one line says so first (LD-382, LD-524). Root under `sudo` acts for them.
        if selection.home.is_some()
            && euid != 0
            && let Some(line) = crate::home::sudo_warning(euid, inv.identity.sudo_uid.as_deref())
        {
            eprintln!("{line}");
        }
        // A host lodi may not manage stops before any lookup, prompt or change (LD-492).
        if selection.host.is_some() && !crate::marker::may_manage(&user.root, euid) {
            return Err(crate::marker::refusal(&user.root).into());
        }
        // A fetched config is a commit: named, never warned about (#689).
        let git = match &fetched {
            Some(fetched) => Git {
                commit: Some(fetched.git.rev.chars().take(7).collect()),
                warning: None,
            },
            None => Git::read(&config.root, &user),
        };
        Ok(Switch {
            inv,
            flags,
            found,
            fetched,
            config,
            user,
            selection,
            git,
            now: crate::util::now_utc(),
        })
    }

    /// The home part read, before the lock's lookups read it for its tools. A home last
    /// switched from another folder is not switched from a config found without being typed.
    fn read_home(&self) -> Result<Option<(Roots, HomeManifest)>, Stopped> {
        let Some(part) = &self.selection.home else {
            return Ok(None);
        };
        let read = read_home(&self.config, part, &self.user)?;
        if self.found.origin != super::Origin::Typed && self.fetched.is_none() {
            home_fence(&read.0, &self.config.root)?;
        }
        Ok(Some(read))
    }

    /// The lock, as the person, before the host part: fill what is missing, or refresh. A home
    /// part alone waits for another lodi; a host part does not.
    fn lock(&self, manifest: Option<&HomeManifest>) -> Result<Locked, Stopped> {
        let root = &self.config.root;
        let held = match self.selection.host {
            Some(_) => lock::hold(root)?,
            None => lock::wait(root)?,
        };
        // A fetched tree is never written: what its lock lacked was resolved by the last switch
        // of that commit and kept beside it (LD-528).
        let kept = self.fetched.as_ref().map(fetched::kept_lock).transpose()?;
        let old = match kept.flatten() {
            Some(kept) => kept,
            None => held.read()?,
        };
        if !self.flags.update {
            self.refuse_stale(&old, manifest)?;
        }
        let machine = Machine::of(&self.user.root)?;
        let mut archive = Archive::new(root, machine, &self.user.home_or_root(), self.now);
        let (new, texts) = if self.flags.update {
            let silent = &mut progress::Silent;
            let planned = plan_update(&self.config, &self.selection, &old, &mut archive, silent)?;
            (planned.lock, planned.texts)
        } else {
            let keys = |part: &Option<Part>| part.iter().map(|p| p.key.clone()).collect();
            let (hosts, homes): (Vec<String>, Vec<String>) =
                (keys(&self.selection.host), keys(&self.selection.home));
            let hosts: Vec<&str> = hosts.iter().map(String::as_str).collect();
            let homes: Vec<&str> = homes.iter().map(String::as_str).collect();
            let filled = lock::fill(&old, &hosts, &homes, &mut archive)?;
            (filled, BTreeMap::new())
        };
        Ok(Locked {
            held,
            old,
            new,
            texts,
        })
    }

    /// A switch locks only what is missing: a locked tool whose request or recipe moved is
    /// `lodi update`'s to lock again (LD-528), so the lock never changes behind a preview.
    fn refuse_stale(&self, old: &Lock, manifest: Option<&HomeManifest>) -> Result<(), Diagnostic> {
        let (Some(part), Some(manifest)) = (&self.selection.home, manifest) else {
            return Ok(());
        };
        let Some(section) = old.homes.get(&part.key) else {
            return Ok(());
        };
        let moved = crate::lock::tools_moved(&manifest.tools, &tool_lock(&manifest.tools, section));
        if moved.is_empty() {
            return Ok(());
        }
        let mut stale = Diagnostic::new(
            "E_LOCK_STALE",
            format!(
                "{} is out of date with {}",
                self.config.root.join(lock::FILE).display(),
                part.path.display()
            ),
        );
        stale.notes = moved;
        Err(stale.hint("run `lodi update` to lock the change, then `lodi switch`"))
    }

    /// Both parts planned, the host part's as the person.
    fn plan(&self, locked: &Locked, read: Option<(Roots, HomeManifest)>) -> Result<Plans, Stopped> {
        let host = self.selection.host.as_ref();
        let (old, new) = (&locked.old, &locked.new);
        let released = host
            .map(|part| released(old, new, &part.key))
            .unwrap_or_default();
        let options = host.map(|part| host_options(part, self.flags, new, &released));
        let loaded = match &options {
            Some(options) => Some(hostscope::load(options, Operation::Plan)?),
            None => None,
        };
        let home = match (&self.selection.home, read) {
            (Some(part), Some(read)) => {
                Some(plan_home(part, &self.user, read, new.homes.get(&part.key))?)
            }
            _ => None,
        };
        let host_changes = loaded
            .as_ref()
            .is_some_and(|loaded| !loaded.plan.is_noop() || loaded.plan.record_changes);
        let home_changes = home.as_ref().is_some_and(|home| home.changes.total() > 0);
        Ok(Plans {
            released,
            options,
            loaded,
            home,
            host_changes,
            home_changes,
        })
    }

    /// The warnings every outcome prints: the config's git state, and a fetched config's lock
    /// that does not lock everything.
    fn warnings(&self, locked: &Locked) -> Vec<String> {
        let mut warnings: Vec<String> = self.git.warning.iter().map(|w| w.to_string()).collect();
        if let Some(fetched) = &self.fetched
            && locked.new != locked.old
        {
            warnings.push(format!(
                "{}'s lodi.lock does not lock everything; resolved and kept beside its fetched \
                 tree",
                fetched.git.url
            ));
        }
        warnings
    }

    /// The preview: the fetched commit, the host part's lines and count, the home part's.
    fn preview(&self, plans: &Plans) -> Result<Preview, Stopped> {
        let mut text = String::new();
        if let Some(fetched) = &self.fetched {
            let _ = writeln!(text, "{}", fetched.line);
        }
        let mut host_count = String::new();
        if let Some(loaded) = &plans.loaded {
            host_count = host_preview(loaded, plans.host_changes, &mut text)?;
        }
        let mut home_count = String::new();
        if let Some(home) = &plans.home {
            for line in &home.lines {
                let _ = writeln!(text, "{line}");
            }
            home_count = home.changes.shown();
            let _ = writeln!(text, "home: {home_count}");
            for warning in &home.warnings {
                let _ = writeln!(text, "{warning}");
            }
        }
        Ok(Preview {
            text,
            host_count,
            home_count,
        })
    }

    /// Nothing to change: say so, with every warning, and write the lock unless `--dry-run`.
    fn nothing_to_switch(
        &self,
        locked: &Locked,
        plans: &Plans,
        warnings: &[String],
    ) -> Result<(), Stopped> {
        if let Some(fetched) = &self.fetched {
            eprintln!("{}", fetched.line);
        }
        eprintln!("nothing to switch{}", self.git.suffix());
        // The host plan's own warnings stand with nothing to do too, as with changes (LD-515).
        for warning in plans.loaded.iter().flat_map(|loaded| &loaded.plan.warnings) {
            eprintln!("{warning}");
        }
        for warning in plans.home.iter().flat_map(|home| &home.warnings) {
            eprintln!("{warning}");
        }
        for warning in warnings {
            eprintln!("lodi: warning: {warning}");
        }
        if !self.flags.dry_run {
            self.write_lock(locked)?;
            self.remember(&locked.new);
        }
        print_tail(plans.home.as_ref(), self.flags.dry_run);
        Ok(())
    }

    /// Drift, `--ask` without a terminal, then root for the host part, are settled before the
    /// question, so nobody is asked in vain; then `--ask`'s question.
    fn settle(&self, locked: &Locked, plans: &Plans) -> Result<Option<Elevated>, Stopped> {
        let flags = self.flags;
        if let Some(loaded) = plans.loaded.as_ref().filter(|_| plans.host_changes) {
            hostscope::refuse_drift(&loaded.plan, flags.overwrite_drift)?;
        }
        if flags.ask && !io::stdin().is_terminal() {
            return Err(Diagnostic::new(
                "E_DECLINED",
                "--ask waits for a yes, and standard input is not a terminal; nothing was changed",
            )
            .hint("drop --ask to switch without asking")
            .into());
        }
        let elevation = match (&plans.loaded, plans.host_changes && self.euid() != 0) {
            (Some(loaded), true) => {
                let part = self.selection.host.as_ref().expect("a loaded host part");
                let lock = (&locked.new, self.fetched.is_some(), &plans.released);
                let (inv, user, config) = (self.inv, &self.user, &self.config);
                // The root run changes the person's lingering too, before the home part.
                let linger = plans.home.as_ref().filter(|_| plans.home_changes);
                let linger = linger.and_then(|home| home.linger);
                let host = (part, loaded, linger);
                Some(elevation_for(inv, user, config, host, flags, lock)?)
            }
            _ => None,
        };
        if flags.ask && !confirm()? {
            return Err(
                Diagnostic::new("E_DECLINED", "switch declined; nothing was changed").into(),
            );
        }
        Ok(elevation)
    }

    fn euid(&self) -> u32 {
        self.inv.identity.euid
    }

    /// The files the lock's write replaces, as they are now: none for a fetched config.
    fn kept_before(&self, locked: &Locked) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        if self.fetched.is_some() {
            return Vec::new();
        }
        std::iter::once(self.config.root.join(lock::FILE))
            .chain(locked.texts.keys().cloned())
            .map(|path| {
                let old = std::fs::read(&path).ok();
                (path, old)
            })
            .collect()
    }

    /// Write the lock as the person: a folder's, once every refusal is past. A fetched config
    /// is never written; what its lock lacks is kept beside it ([`Switch::remember`]).
    fn write_lock(&self, locked: &Locked) -> Result<(), Diagnostic> {
        if self.fetched.is_none() {
            commit(&locked.held, &self.user, &locked.texts, &locked.new)?;
        }
        Ok(())
    }

    /// Run the parts: the host part's steps, then the home part's, one numbering; then the
    /// summary and what the apply said.
    fn apply(
        &self,
        plans: &Plans,
        elevation: Option<Elevated>,
        before: &[(PathBuf, Option<Vec<u8>>)],
        preview: &Preview,
    ) -> Result<(), Stopped> {
        let mut names: Vec<&str> = Vec::new();
        if let Some(loaded) = plans.loaded.as_ref().filter(|_| plans.host_changes) {
            names.extend(hostscope::apply::steps(&loaded.plan));
        }
        if let Some(home) = plans.home.as_ref().filter(|_| plans.home_changes) {
            names.extend(home.changes.steps());
        }
        let mut shown = Progress::start(
            io::stderr(),
            Box::new(progress::Wall::new()),
            progress::Mode::detect(),
            self.inv.verbose,
            &names,
            self.user.logs("switch", self.now),
        );
        let (host_report, by_root) = self.apply_host(plans, elevation, before, &mut shown)?;
        let mut said = Vec::new();
        if let Some(home) = plans.home.as_ref().filter(|_| plans.home_changes) {
            let home = (home, by_root);
            match apply_home(&self.config, home, &self.user, self.flags, &mut shown) {
                Ok(warnings) => said = warnings,
                Err(failed) => {
                    let (d, warnings) = *failed;
                    shown.fail(&d);
                    for warning in warnings {
                        eprintln!("{warning}");
                    }
                    return Err(d.into());
                }
            }
        }
        let mut detail: Vec<String> = Vec::new();
        if plans.host_changes {
            detail.push(preview.host_count.clone());
        }
        if plans.home_changes {
            detail.push(format!("home {}", preview.home_count));
        }
        let detail = format!("{}{}", detail.join(", "), self.git.suffix());
        shown.summary("switched", &detail);
        drop(shown);
        // The outstanding journal the preview named, as the apply classified it: "journal ID
        // was CLASSIFICATION", not the "journal ID" of this switch's own journal.
        let classified = |line: &&str| line.starts_with("journal ") && line.contains(" was ");
        for line in host_report.lines().filter(classified) {
            eprintln!("{line}");
        }
        for warning in &said {
            eprintln!("{warning}");
        }
        print_tail(plans.home.as_ref(), false);
        Ok(())
    }

    /// The host part, with changes: as root in lodi's hidden entry, or in this process when it
    /// is root. A root run that never started puts the lock's write back. Its report, and from
    /// the hidden entry whether it changed the person's lingering.
    fn apply_host(
        &self,
        plans: &Plans,
        elevation: Option<Elevated>,
        before: &[(PathBuf, Option<Vec<u8>>)],
        shown: &mut Progress<io::Stderr>,
    ) -> Result<(String, Option<bool>), Stopped> {
        let elevated = elevation.is_some();
        let done = match (
            elevation,
            plans.options.as_ref().filter(|_| plans.host_changes),
        ) {
            (Some((elevation, request)), _) => elevation
                .run(&Request::Switch(request), shown)
                .map(|done| {
                    let report = done["applied"].as_str().unwrap_or_default().to_string();
                    (report, done["linger"].as_bool())
                })
                .map_err(HostError::from),
            (None, Some(options)) => hostscope::load(options, Operation::Apply)
                .and_then(|loaded| hostscope::apply_for_switch(options, loaded, shown))
                .map(|report| (report, None)),
            (None, None) => Ok((String::new(), None)),
        };
        let mut error = match done {
            Ok(report) => return Ok(report),
            Err(error) => error,
        };
        let never_ran = elevated
            && error
                .diagnostics
                .first()
                .is_some_and(|d| matches!(d.code, "E_NEED_ROOT" | "E_HOST_ROOT_REQUIRED"));
        if never_ran {
            put_back(before, &self.user);
        }
        if let Some(first) = error.diagnostics.first() {
            shown.fail(first);
        }
        // With `--host`, or a config with no home, there is no home part to speak of.
        if let Some(first) = error
            .diagnostics
            .first_mut()
            .filter(|_| plans.home.is_some())
        {
            first
                .notes
                .push("the host part failed; your home part did not run".into());
        }
        Err(error.into())
    }

    /// After a switch that succeeded: remember a typed path or URL, and record a fetched
    /// commit with the lock it was switched with.
    fn remember(&self, lock: &Lock) {
        let user = &self.found.user;
        let recorded = self.fetched.as_ref().map_or(Ok(()), |fetched| {
            fetched::record(fetched, lock, user, super::home_owner(user))
        });
        for result in [recorded, super::remember(&self.found)] {
            if let Err(error) = result {
                eprintln!("lodi: warning: {}", error.message);
            }
        }
    }
}

/// The host part's preview lines into `text`: an outstanding journal, the plan, each input
/// only root may read that a root run will read; then its count, which it returns.
fn host_preview(
    loaded: &hostscope::Loaded,
    changes: bool,
    text: &mut String,
) -> Result<String, Stopped> {
    if let Some(record) = hostscope::journal::outstanding(&loaded.gate)? {
        let classification = record.classify_in(&loaded.gate);
        let _ = writeln!(
            text,
            "journal {} is outstanding: {}",
            record.id,
            hostscope::describe(&classification)
        );
    }
    let plan = &loaded.plan;
    let rendered = plan.render();
    let mut lines: Vec<&str> = rendered.lines().collect();
    lines.pop();
    for line in lines {
        let _ = writeln!(text, "{line}");
    }
    let changing = |path: &String| {
        plan.file_actions()
            .any(|file| &file.path == path && file.op != Op::Unchanged)
    };
    let files = unread_files(plan).filter(changing);
    // An input only root reads is named only when a root run will read it.
    let others = plan.unread.iter().filter(|_| changes);
    for shown in files.chain(others.map(|u| u.shown.clone())) {
        let _ = writeln!(
            text,
            "{shown}: not readable as you; the root run reads it before changing it"
        );
    }
    let count = host_counts(plan);
    let _ = writeln!(text, "host: {count}");
    Ok(count)
}

/// Put back what the lock's write replaced, when the root run never started: each file as it
/// was, and one that was not there removed.
fn put_back(before: &[(PathBuf, Option<Vec<u8>>)], user: &User) {
    for (path, old) in before {
        if std::fs::read(path).ok() == *old {
            continue;
        }
        let _ = match old {
            Some(bytes) => super::verbs::write_text(path, bytes, user),
            None => std::fs::remove_file(path),
        };
    }
}

/// The home part's last lines: the rc-file lines, after whether the tools are on `PATH` for a
/// preview.
fn print_tail(home: Option<&Home>, preview: bool) {
    let Some(home) = home else {
        return;
    };
    for line in home.on_path.iter().filter(|_| preview).chain(&home.tail) {
        eprintln!("{line}");
    }
}

/// What the host part run as root is handed (#709): the host and what it was previewed as.
/// The config root, `--root`, `-v` and the person's uid travel as [`Elevation`]'s own fields.
#[derive(Debug, Deserialize)]
struct HostRequest {
    /// The host's `host.toml`, and its key in `lodi.lock`.
    host: PathBuf,
    key: String,
    overwrite_drift: bool,
    resolved: Option<String>,
    /// [`unread`] of the preview's plan.
    unread: Vec<String>,
    /// [`digest`] of the host part as previewed.
    digest: String,
    /// The host's `lodi.lock` section when it was resolved for this run and never written (a
    /// fetched config); the child reads the config's own lock otherwise.
    section: Option<lock::HostSection>,
    /// The person's fetch settings, which the elevator clears with the rest of the environment.
    #[serde(default)]
    fetch: FetchSettings,
    /// [`released`] of the preview: the lock the child reads no longer has them.
    #[serde(default)]
    released: BTreeMap<String, crate::hostscope::pin::PinRecord>,
    /// The person's lingering to turn on or off once the host part is applied: their home plan
    /// changes it, and no process of theirs may.
    #[serde(default)]
    linger: Option<bool>,
}

/// The fetch settings the root run takes from the request (LD-515): the person's
/// `LODI_FETCH_REWRITE` (a mirror, or a test's loopback archive) and `LODI_FETCH_ATTEMPTS`, each
/// checked as `crate::fetch` reads it. Nothing else of the person's environment reaches root.
#[derive(Debug, Default, Deserialize)]
struct FetchSettings {
    rewrite: Option<String>,
    attempts: Option<String>,
}

impl FetchSettings {
    const REWRITE: &str = "LODI_FETCH_REWRITE";
    const ATTEMPTS: &str = "LODI_FETCH_ATTEMPTS";

    /// This process's settings, for the request.
    fn of_env() -> Value {
        let read = |name: &str| std::env::var(name).ok();
        json!({ "rewrite": read(Self::REWRITE), "attempts": read(Self::ATTEMPTS) })
    }

    /// Make them this process's, after checking each, where they differ from its environment:
    /// the hidden entry's child is one thread until it performs the request, and a root lodi
    /// that runs the request in place already has them.
    fn take(&self) -> Result<(), Diagnostic> {
        let bad = |name: &str, why: String| {
            Diagnostic::new(
                "E_APPLY",
                format!("the switch request lodi was handed carries a bad {name}: {why}"),
            )
        };
        if let Some(text) = &self.rewrite {
            crate::fetch::parse_rewrites(text).map_err(|why| bad(Self::REWRITE, why))?;
        }
        if let Some(text) = &self.attempts {
            crate::fetch::parse_attempts(text).map_err(|why| bad(Self::ATTEMPTS, why))?;
        }
        for (name, value) in [
            (Self::REWRITE, &self.rewrite),
            (Self::ATTEMPTS, &self.attempts),
        ] {
            if std::env::var(name).ok() == *value {
                continue;
            }
            // SAFETY: no other thread runs yet in this process (see above), so nothing reads the
            // environment while it changes.
            unsafe {
                match value {
                    Some(value) => std::env::set_var(name, value),
                    None => std::env::remove_var(name),
                }
            }
        }
        Ok(())
    }
}

/// What this process could not read, so that the root run reads it (#709): each managed file
/// there but not readable as the person (a root-only file), by its path, and each other action
/// an input only root may read decides ([`hostscope::plan::Unread`]), by its key. The preview
/// names them.
fn unread(plan: &hostscope::plan::Plan) -> Vec<String> {
    let mut keys: Vec<String> = unread_files(plan).collect();
    for item in &plan.unread {
        if !keys.contains(&item.action) {
            keys.push(item.action.clone());
        }
    }
    keys
}

/// The managed files there but not readable as the person.
fn unread_files(plan: &hostscope::plan::Plan) -> impl Iterator<Item = String> + '_ {
    plan.actions.iter().filter_map(|action| match &action.kind {
        Kind::File(file)
            if file.before.exists && file.before.regular && file.before.digest.is_none() =>
        {
            Some(file.path.clone())
        }
        _ => None,
    })
}

/// The key [`unread`] gives the action that `kind` is, when an unread input can decide it.
fn unread_key(kind: &Kind) -> Option<String> {
    match kind {
        Kind::Basic(basic) if hostscope::boot::KEYS.contains(&basic.key) => {
            Some(hostscope::boot::UNREAD.to_string())
        }
        Kind::Basic(basic) => Some(basic.key.to_string()),
        Kind::Identity(id) if matches!(id.op, hostscope::users::IdOp::Keys(_)) => {
            Some(format!("ssh keys {}", id.name))
        }
        _ => None,
    }
}

/// The host part as previewed: its manifest, its `lodi.lock` section and its plan's lines. The
/// child plans again as root and applies only when this is the same. A file in `unread` counts
/// by what it is declared to be, never by bytes only root could read, so that the root run's
/// better view of it is no difference; drift found there still stops that run. Another action
/// in `unread` is left out: the manifest's digest holds its declaration.
fn digest(loaded: &hostscope::Loaded, lock: &Lock, key: &str, unread: &[String]) -> String {
    let section = serde_json::to_string(&lock.hosts.get(key)).unwrap_or_default();
    let plan = &loaded.plan;
    let mut text = format!(
        "{}\n{section}\n{}\n",
        crate::util::sha256_tagged(&loaded.input.manifest),
        plan.record_changes,
    );
    let names_unread = |line: &str| {
        unread
            .iter()
            .any(|path| path.starts_with('/') && line.contains(path.as_str()))
    };
    for line in plan.warnings.iter().chain(&plan.notes).chain(&plan.kept) {
        if !names_unread(line) {
            let _ = writeln!(text, "{line}");
        }
    }
    for action in &plan.actions {
        match &action.kind {
            Kind::File(file) if file.op == Op::Restore && unread.contains(&file.path) => {
                let _ = writeln!(text, "unread {} restore {:?}", file.path, file.restore_from);
            }
            Kind::File(file) if unread.contains(&file.path) => {
                let desired = file.desired.as_ref();
                let _ = writeln!(
                    text,
                    "unread {} {:?}",
                    file.path,
                    desired.map(|d| (&d.digest, d.mode, &d.owner, &d.group))
                );
            }
            kind if unread_key(kind).is_some_and(|key| unread.contains(&key)) => {}
            _ => {
                for line in action.lines() {
                    let _ = writeln!(text, "{line}");
                }
            }
        }
    }
    crate::util::sha256_tagged(text.as_bytes())
}

/// Not root, with host changes: a person on a terminal and an elevator, or `E_NEED_ROOT`
/// before anything changes; then the elevation and the request it carries.
fn elevation_for(
    inv: &Invocation,
    user: &User,
    config: &Config,
    (part, loaded, linger): (&Part, &hostscope::Loaded, Option<bool>),
    flags: &Flags,
    (lock, unwritten, released): (
        &Lock,
        bool,
        &BTreeMap<String, crate::hostscope::pin::PinRecord>,
    ),
) -> Result<(Elevation, String), Diagnostic> {
    let euid = inv.identity.euid;
    let need_root = |why: &str| {
        Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "the host part has changes, this process runs as uid {euid}, and {why}; \
                 nothing was changed"
            ),
        )
        .hint(
            "run it under sudo (sudo lodi switch), or switch only your home with lodi switch \
             --home",
        )
    };
    if !(io::stdin().is_terminal() && io::stderr().is_terminal()) {
        return Err(need_root("there is no terminal to ask for a password on"));
    }
    let path = std::env::var_os("PATH");
    elevate::choose(path.as_deref())
        .map_err(|_| need_root("none of sudo, doas or run0 is on PATH"))?;
    let exe = std::env::current_exe()
        .map_err(|e| Diagnostic::new("E_STORE_IO", format!("lodi's own binary: {e}")))?;
    let mut elevation = Elevation::new(&exe, path.as_deref(), user, euid);
    elevation.root = (user.root != Path::new("/")).then(|| user.root.clone());
    elevation.config = Some(config.root.clone());
    elevation.verbose = inv.verbose;
    let unread = unread(&loaded.plan);
    // Built as a value: the request travels on the child's command line, never to disk, so it is
    // no serialized record of the schema registry's.
    let request = json!({
        "host": part.path,
        "key": part.key,
        "overwrite_drift": flags.overwrite_drift,
        "resolved": flags.resolved,
        "digest": digest(loaded, lock, &part.key, &unread),
        "unread": unread,
        "section": unwritten.then(|| lock.hosts.get(&part.key)).flatten(),
        "released": released,
        "fetch": FetchSettings::of_env(),
        "linger": linger,
    });
    let text = request.to_string();
    Ok((elevation, text))
}

/// The host part as root, in lodi's hidden entry (#709): plan again, now reading as root, and
/// apply only when the plan is the one previewed; else `E_DECLINED` with nothing changed.
pub fn elevated(with: &str, cx: &Context, sink: &mut dyn Sink) -> Result<Value, Diagnostic> {
    let unreadable = || {
        Diagnostic::new(
            "E_APPLY",
            "the switch request lodi was handed is unreadable",
        )
    };
    let request: HostRequest = serde_json::from_str(with).map_err(|_| unreadable())?;
    request.fetch.take()?;
    let config = cx.config.as_ref().ok_or_else(unreadable)?;
    let mut lock = lock::read(config)?;
    if let Some(section) = request.section.clone() {
        lock.hosts.insert(request.key.clone(), section);
    }
    let part = Part {
        path: request.host.clone(),
        key: request.key.clone(),
    };
    let flags = Flags {
        overwrite_drift: request.overwrite_drift,
        resolved: request.resolved.clone(),
        root: (cx.root != Path::new("/")).then(|| cx.root.clone()),
        ..Flags::default()
    };
    let options = host_options(&part, &flags, &lock, &request.released);
    let first = |error: HostError| {
        error
            .diagnostics
            .into_iter()
            .next()
            .unwrap_or_else(|| Diagnostic::new("E_APPLY", "the host part failed"))
    };
    // One plan: the one the preview is checked against is the one applied.
    let loaded = hostscope::load(&options, Operation::Apply).map_err(first)?;
    if digest(&loaded, &lock, &part.key, &request.unread) != request.digest {
        return Err(Diagnostic::new(
            "E_DECLINED",
            "the host changed since the preview; nothing was changed",
        )
        .hint("run lodi switch again"));
    }
    let report = hostscope::apply_for_switch(&options, loaded, sink).map_err(first)?;
    let linger = match request.linger {
        Some(on) => {
            let system = cx.root == Path::new("/");
            Some(crate::home::services::linger_as_root(cx.user, system, on)?)
        }
        None => None,
    };
    Ok(json!({ "applied": report, "linger": linger }))
}

/// A child's failure as one diagnostic, without the `lodi: error CODE: ` its text may carry.
/// A diagnostic as a child (or the home part) rendered it, read back: its message without the
/// `lodi: error CODE: ` prefix, its notes and hint, and apart from it the `lodi: warning` lines
/// that came with it.
fn child_failure(code: &'static str, text: &str) -> (Diagnostic, Vec<String>) {
    let prefix = format!("lodi: error {code}: ");
    let mut lines = text.lines();
    let first = lines.next().unwrap_or_default();
    let mut d = Diagnostic::new(
        code,
        first.strip_prefix(&prefix).unwrap_or(first).to_string(),
    );
    let mut warnings = Vec::new();
    for line in lines {
        match line.strip_prefix("   = ") {
            Some(note) => d.notes.push(note.to_string()),
            None if line.starts_with("lodi: warning") => warnings.push(line.to_string()),
            None => d.message = format!("{}\n{line}", d.message),
        }
    }
    (d, warnings)
}

/// The pins of host `key` that `old` records and `new` no longer does: removed from the host
/// file since the last lock. Their holds go, and the plan says why ("no longer pinned", P9).
fn released(
    old: &Lock,
    new: &Lock,
    key: &str,
) -> BTreeMap<String, crate::hostscope::pin::PinRecord> {
    let (Some(old), Some(new)) = (old.hosts.get(key), new.hosts.get(key)) else {
        return BTreeMap::new();
    };
    old.pins
        .iter()
        .filter(|(name, _)| !new.pins.contains_key(*name))
        .map(|(name, pin)| (name.clone(), pin.clone()))
        .collect()
}

/// The host part's options for `part`, on the config's pins and the ones just `released`.
fn host_options(
    part: &Part,
    flags: &Flags,
    lock: &Lock,
    released: &BTreeMap<String, crate::hostscope::pin::PinRecord>,
) -> Options {
    let section = lock.hosts.get(&part.key);
    Options {
        root: flags.root.clone(),
        source: part.path.parent().map(Path::to_path_buf),
        resolved: flags.resolved.clone(),
        overwrite_drift: flags.overwrite_drift,
        config: Some(ConfigPins {
            pins: section.map(|s| crate::hostscope::pin::PinsLock {
                snapshot: s.snapshot.clone(),
                pins: released
                    .iter()
                    .chain(&s.pins)
                    .map(|(name, pin)| (name.clone(), pin.clone()))
                    .collect(),
                ..crate::hostscope::pin::PinsLock::new(&s.distro, &s.release, &s.arch)
            }),
            keys: section.map(|s| s.keys.clone()).unwrap_or_default(),
        }),
        ..Options::default()
    }
}

/// One line of host counts: `+1 -2 packages, 1 file, 1 service`, `~1` for a package moved to
/// another version, or `no changes`.
fn host_counts(plan: &hostscope::plan::Plan) -> String {
    let (mut install, mut remove, mut moves) = (0, 0, 0);
    let (mut files, mut services, mut other) = (0, 0, 0);
    // A mark or hold on a package this plan installs or removes is part of that change; one on
    // any other package (an unpin's unhold) is a change of its own.
    let moved: BTreeSet<&String> = plan
        .changing()
        .filter_map(|action| match &action.kind {
            Kind::Package(packages) => Some(
                packages
                    .install
                    .iter()
                    .chain(&packages.remove)
                    .chain(&packages.changed),
            ),
            _ => None,
        })
        .flatten()
        .collect();
    for action in plan.changing() {
        match &action.kind {
            Kind::Package(packages) => {
                install += packages.install.len();
                remove += packages.remove.len();
                moves += packages.changed.len();
                let marks = [
                    &packages.auto,
                    &packages.manual,
                    &packages.hold,
                    &packages.unhold,
                ];
                if packages.install.is_empty()
                    && packages.remove.is_empty()
                    && packages.changed.is_empty()
                    && marks
                        .iter()
                        .flat_map(|names| names.iter())
                        .any(|n| !moved.contains(n))
                {
                    other += 1;
                }
            }
            Kind::File(file) if file.op != Op::Unchanged => files += 1,
            Kind::Service(_) => services += 1,
            _ => other += 1,
        }
    }
    let plural = |n: usize, word: &str| format!("{n} {word}{}", if n == 1 { "" } else { "s" });
    let mut parts = Vec::new();
    if moves > 0 {
        parts.push(format!("+{install} -{remove} ~{moves} packages"));
    } else if install + remove > 0 {
        parts.push(format!("+{install} -{remove} packages"));
    }
    if files > 0 {
        parts.push(plural(files, "file"));
    }
    if services > 0 {
        parts.push(plural(services, "service"));
    }
    if other > 0 {
        parts.push(plural(other, "other change"));
    }
    if parts.is_empty() {
        if plan.record_changes {
            return "no machine changes; the record is updated".to_string();
        }
        return "no changes".to_string();
    }
    parts.join(", ")
}

/// A home last switched from another folder is not switched from a config found without being
/// typed (LD-524): that would undo the other folder's home without a word.
fn home_fence(roots: &crate::roots::Roots, root: &Path) -> Result<(), Diagnostic> {
    // A state that cannot be read is the apply's own refusal, with its own notes.
    let Ok(state) = crate::home::state::read(&roots.data_root()) else {
        return Ok(());
    };
    let source = state.source();
    if source == crate::home::state::DEFAULT_SOURCE || Path::new(source) == root {
        return Ok(());
    }
    Err(Diagnostic::new(
        "E_DECLINED",
        format!(
            "your home was last switched from {source}, and {} was found without being typed",
            root.display()
        ),
    )
    .hint(format!(
        "name the one you mean: lodi switch --home {source}, or lodi switch --home {}",
        root.display()
    )))
}

/// Read the home `part` for `user`: every error of a manifest that does not parse at once,
/// before the lock's lookup reads it for its tools.
fn read_home(
    config: &Config,
    part: &Part,
    user: &User,
) -> Result<(crate::roots::Roots, crate::home::manifest::HomeManifest), HostError> {
    let shown = part.path.display().to_string();
    let bytes = std::fs::read(&part.path)
        .map_err(|e| Diagnostic::new("E_NO_MANIFEST", format!("cannot read {shown}: {e}")))?;
    let folder = part.path.parent().unwrap_or(&config.root).to_path_buf();
    // Every `source` is read as a host's is: walked to from the home's folder (the
    // config's walk judged the folders above it) on descriptors, never through a link, and
    // refused (`E_PATH_ESCAPE`) when anyone but root, the person or the person's private
    // group (#684, as the config's walk allows) may write it, before any part changes anything.
    let owners = crate::hostscope::source::owners_for(user.uid, None);
    let sources = crate::hostscope::source::Host {
        anchor: folder.clone(),
        dir: folder.clone(),
        source: folder.display().to_string(),
        private: super::private_groups(user, &owners),
        owners,
        named: None,
    };
    let roots = crate::roots::Roots::for_host(user.home_or_root(), folder)
        .moved(user.xdg_config.as_deref(), user.data.as_deref());
    let every = |errors: crate::manifest::ManifestErrors| match errors.diagnostics.is_empty() {
        true => Diagnostic::new("E_SYNTAX", format!("{shown} is not valid")).into(),
        false => HostError {
            diagnostics: errors.diagnostics,
            done: String::new(),
        },
    };
    // As the lock's lookup reads it first (a `source` that is a link is `E_CONFIG` there), then
    // through the walk below.
    crate::home::manifest::parse_home_manifest_bytes(&bytes, &shown, &roots.config_root())
        .map_err(every)?;
    let manifest = crate::home::manifest::parse_home_manifest_host_bytes(
        &bytes,
        &shown,
        &roots.config_root(),
        &crate::home::programs::RenderEnv::of(&roots),
        crate::home::manifest::HostSource {
            host: &sources,
            prefix: "",
        },
    )
    .map_err(every)?;
    Ok((roots, manifest))
}

/// The tool lock of a config's home `section`, for `tools`.
fn tool_lock(tools: &crate::tools::ToolSet, section: &HomeSection) -> crate::lock::LockFile {
    crate::lock::LockFile {
        version: crate::lock::LOCK_VERSION,
        format: crate::lock::LOCK_FORMAT.to_string(),
        generated_by: format!("lodi {}", env!("CARGO_PKG_VERSION")),
        manifest_hash: crate::lock::tool_set_hash(tools),
        base: None,
        packages: section.tools.clone(),
        profiles: section.profiles.clone(),
    }
}

/// Who the home part runs as under `sudo`: the home's owner, else the invoking user.
fn person(user: &User) -> (u32, u32) {
    super::home_owner(user).unwrap_or((user.uid, user.gid.unwrap_or(user.uid)))
}

/// Plan the home `part` read by [`read_home`] for `user`, as the person (in a child under
/// `sudo`).
fn plan_home(
    part: &Part,
    user: &User,
    (roots, manifest): (crate::roots::Roots, crate::home::manifest::HomeManifest),
    section: Option<&HomeSection>,
) -> Result<Home, Diagnostic> {
    let section = section.cloned().unwrap_or_default();
    // A home without tools has no tool lock, as `locked_set` gives none.
    let lock = (!manifest.tools.is_empty()).then(|| tool_lock(&manifest.tools, &section));
    let context = crate::home::services::Context {
        fixed_path: user.root == Path::new("/") && user.sudo,
        ..Default::default()
    };
    let plan = |roots: &crate::roots::Roots| plan_lines(roots, &manifest, lock.as_ref(), &context);
    let planned = if user.sudo {
        let (uid, gid) = person(user);
        crate::hostscope::flake::as_person(uid, gid, &part.key, || plan(&roots))
    } else {
        plan(&roots)
    }
    .map_err(|(code, text)| child_failure(code, &text).0)?;
    let planned: Planned = serde_json::from_str(&planned)
        .map_err(|e| Diagnostic::new("E_APPLY", format!("the home plan did not read: {e}")))?;
    Ok(Home {
        part: part.clone(),
        roots,
        manifest,
        lock,
        lines: planned.lines,
        changes: planned.changes,
        warnings: planned.warnings,
        tail: planned.tail,
        on_path: planned.on_path,
        linger: planned.linger,
    })
}

/// The home plan on `roots`, as the JSON [`Planned`] a child under `sudo` hands back.
fn plan_lines(
    roots: &crate::roots::Roots,
    manifest: &crate::home::manifest::HomeManifest,
    lock: Option<&crate::lock::LockFile>,
    context: &crate::home::services::Context,
) -> Result<String, (&'static str, String)> {
    let plan = crate::home::plan::plan_for(roots, manifest, false, context)
        .map_err(|d| (d.code, d.to_string()))?;
    // Tools to install or remove are a change of their own (LD-524).
    let tools = !crate::home::tools::current(roots, &manifest.tools, lock);
    let files = plan.steps.iter();
    let changes = Counts {
        files: files
            .filter(|step| step.action != crate::home::plan::Action::Nothing)
            .count(),
        // The set is linked again whole: each declared tool, or the one unlinking.
        tools: if tools {
            manifest.tools.len().max(1)
        } else {
            0
        },
        services: plan.services.changes(),
    };
    let mut lines = plan.lines();
    if tools {
        let names: Vec<&str> = manifest.tools.keys().map(String::as_str).collect();
        lines.push(match names.is_empty() {
            true => "tools     none: the installed tools are unlinked".to_string(),
            false => format!("tools     {}", names.join(" ")),
        });
    }
    let mut warnings = crate::home::plan::program_warnings(roots, manifest);
    let mut tail = Vec::new();
    let mut on_path = None;
    if !manifest.tools.is_empty() {
        on_path = Some(crate::home::profile::path_status(roots));
        let notice = crate::home::profile::notice(roots, manifest.uses_program("fish"));
        warnings.extend(notice.warnings);
        tail.extend(notice.lines);
    }
    tail.extend(crate::home::profile::hook_lines(roots, &manifest.hooks()));
    let planned = serde_json::json!({
        "changes": {
            "files": changes.files,
            "tools": changes.tools,
            "services": changes.services,
        },
        "lines": lines,
        "warnings": warnings,
        "tail": tail,
        "on_path": on_path,
        "linger": plan.services.linger_change(),
    });
    Ok(planned.to_string())
}

/// Apply the home part as the person: in this process, or under `sudo` in a child that
/// becomes them first. Lingering the plan changes is root's to change whenever the switch has
/// root: under `sudo` this process turns it on before the child and off after it; `by_root` is
/// whether the root run of the host part already did (H1).
/// `Ok` is the apply's own `W_…` lines the preview did not already show; a failure carries
/// them too.
fn apply_home(
    config: &Config,
    (home, by_root): (&Home, Option<bool>),
    user: &User,
    flags: &Flags,
    shown: &mut dyn Sink,
) -> Result<Vec<String>, Box<(Diagnostic, Vec<String>)>> {
    let alone = |d: Diagnostic| Box::new((d, Vec::new()));
    let fetcher = super::lookup::http().map_err(alone)?;
    let now = crate::util::now_utc();
    let fixed_path = user.root == Path::new("/") && user.sudo;
    let root_linger = |on: bool| {
        let uid = person(user).0;
        crate::home::services::linger_as_root(uid, fixed_path, on).map_err(alone)
    };
    let by_root = match (user.sudo, home.linger) {
        (true, Some(true)) => Some(root_linger(true)?),
        (true, Some(false)) => Some(false),
        _ => by_root,
    };
    let options = crate::home::apply::Options {
        overwrite_drift: flags.overwrite_drift,
        locked: true,
        services: crate::home::services::Context {
            fixed_path,
            linger_by_root: by_root.is_some(),
            linger_enabled_by_root: home.linger == Some(true) && by_root == Some(true),
        },
    };
    let source = config.root.display().to_string();
    let failed = |failure: crate::home::apply::Failure| {
        let prefix = format!("lodi: error {}: ", failure.code);
        let text = failure.text.strip_prefix(&prefix).unwrap_or(&failure.text);
        let mut text = text.to_string();
        for warning in failure.warnings {
            text.push('\n');
            text.push_str(&warning);
        }
        (failure.code, text)
    };
    let apply = |sink: &mut dyn Sink| {
        let mut only = Only {
            steps: home.changes.steps().collect(),
            inner: sink,
            skipping: false,
        };
        crate::home::apply::apply_preloaded_reporting(
            &home.roots,
            &home.manifest,
            home.lock.clone(),
            &source,
            &options,
            fetcher.as_ref(),
            now,
            &mut only,
        )
        .map(|outcome| outcome.warnings.join("\n"))
        .map_err(failed)
    };
    // Under `sudo` the child's own progress, relayed (#694 story 17).
    let done = if user.sudo {
        let (uid, gid) = person(user);
        crate::hostscope::flake::as_person_reporting(uid, gid, &home.part.key, shown, apply)
    } else {
        apply(shown)
    };
    let new = |text: String| {
        text.lines()
            .filter(|line| !home.warnings.iter().any(|shown| shown == line))
            .map(str::to_string)
            .collect()
    };
    let (code, text) = match done {
        Ok(text) if user.sudo && home.linger == Some(false) => {
            root_linger(false)?;
            return Ok(new(text));
        }
        Ok(text) => return Ok(new(text)),
        Err(failed) => failed,
    };
    let (d, warnings) = home_failed(code, &text);
    Err(Box::new((d, new(warnings.join("\n")))))
}

/// A home apply's failure, as the switch says it, and its `W_…` lines.
fn home_failed(code: &'static str, text: &str) -> (Diagnostic, Vec<String>) {
    let (inner, warnings) = child_failure(code, text);
    let mut d = Diagnostic::new(
        code,
        format!("your home part was not applied: {}", inner.message),
    );
    let (hints, notes): (Vec<String>, Vec<String>) = inner
        .notes
        .into_iter()
        .partition(|note| note.starts_with("hint: "));
    d.notes = notes;
    d.notes
        .push("the host part, if it had changes, was applied and is kept".into());
    d = match hints.into_iter().next() {
        Some(hint) => d.hint(hint.trim_start_matches("hint: ")),
        None => d.hint("fix the home and run lodi switch again"),
    };
    (d, warnings)
}

/// `--ask`: `switch? [y/N]` on standard error, one line read from the terminal.
fn confirm() -> Result<bool, Diagnostic> {
    eprint!("switch? [y/N] ");
    let _ = io::stderr().flush();
    let mut line = String::new();
    io::stdin()
        .lock()
        .read_line(&mut line)
        .map_err(|e| Diagnostic::new("E_DECLINED", format!("no answer could be read: {e}")))?;
    Ok(matches!(line.trim(), "y" | "yes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_failure_loses_its_rendered_prefix() {
        let (d, _) = child_failure("E_APPLY", "lodi: error E_APPLY: it broke");
        assert_eq!((d.code, d.message.as_str()), ("E_APPLY", "it broke"));
    }
}
