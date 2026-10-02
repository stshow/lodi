//! The config a host and home command reads (#692, LD-491, LD-493): *find* it without a path,
//! *read* its layout, and *remember* a path the user typed.
//!
//! Every input is passed in ([`Identity`]): this module reads no environment variable, no passwd
//! but the one under the given root, and never the current directory.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, MetadataExt};
use std::path::{Component, Path, PathBuf};

use crate::diag::Diagnostic;
use crate::hostscope::source::{DIR_FLAGS, FILE_FLAGS, open_at, safe_component};

pub mod fetched;
pub mod import;
pub mod lock;
pub mod lookup;
pub mod switch;
pub mod verbs;

/// The file under `<state>/lodi/` that holds the remembered path.
pub const REMEMBERED: &str = "config-path";

/// The code of a stop that is a usage error, printed as `lodi: usage:` with exit status 2 and
/// no error code ([`crate::diag::EXIT_USAGE`]).
pub const USAGE: &str = "usage";

/// A flat config's host and home manifests, and the optional index naming them.
const HOST: &str = "host.toml";
const HOME: &str = "home.toml";
const INDEX: &str = "config.toml";

/// The most bytes `config.toml` may have.
const MAX_INDEX: usize = 64 * 1024;

/// What makes a folder a config ([`is_config`]).
const MARKERS: [&str; 4] = ["lodi.lock", "config.toml", "host.toml", "home.toml"];

/// The longest remembered line.
const MAX_REMEMBERED: usize = 4096;

/// The standard config, below the home folder.
const STANDARD: &str = ".config/lodi";

/// What the caller knows about who runs lodi.
#[derive(Debug, Clone)]
pub struct Identity {
    /// The effective user id.
    pub euid: u32,
    /// `SUDO_UID` as the environment holds it.
    pub sudo_uid: Option<OsString>,
    /// `DOAS_USER`, honoured as `SUDO_UID` is (LD-492) when `SUDO_UID` is not set.
    pub doas_user: Option<OsString>,
    /// `HOME`.
    pub home: Option<PathBuf>,
    /// `XDG_STATE_HOME`.
    pub state_home: Option<PathBuf>,
    /// `XDG_CONFIG_HOME`.
    pub config_home: Option<PathBuf>,
    /// `LODI_HOME`, else `XDG_DATA_HOME/lodi`.
    pub data_home: Option<PathBuf>,
    /// The root whose `etc/passwd` and `etc/group` are read: the `--root`, or `/`.
    pub root: PathBuf,
    /// This machine's hostname.
    pub hostname: String,
}

/// Where a found config came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Typed,
    Environment,
    Remembered,
    Standard,
}

/// A config folder, or a URL as typed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    Local(PathBuf),
    Url(String),
}

/// The config one invocation reads, and for whom.
#[derive(Debug, Clone)]
pub struct Found {
    pub location: Location,
    pub origin: Origin,
    pub user: User,
}

/// The person lodi acts for: the invoking user, or under `sudo` the person behind it.
#[derive(Debug, Clone)]
pub struct User {
    pub uid: u32,
    /// Their passwd entry's login and primary group, when the passwd names them.
    pub login: Option<String>,
    pub gid: Option<u32>,
    /// Their home folder, as this process reaches it.
    pub home: Option<PathBuf>,
    /// Their state folder, `${XDG_STATE_HOME:-~/.local/state}`.
    pub state: Option<PathBuf>,
    /// The folders their own `XDG_CONFIG_HOME` and `LODI_HOME` or `XDG_DATA_HOME` name; never
    /// read under `sudo` or `doas`, where the environment is root's.
    pub xdg_config: Option<PathBuf>,
    pub data: Option<PathBuf>,
    /// Whether lodi runs as root for them, under `sudo` or `doas`.
    pub sudo: bool,
    /// The root of [`Identity::root`].
    pub root: PathBuf,
    /// This machine's hostname.
    pub hostname: String,
}

impl User {
    /// Their home folder, or `/` when the passwd names none.
    pub fn home_or_root(&self) -> PathBuf {
        self.home.clone().unwrap_or_else(|| PathBuf::from("/"))
    }

    /// Where `command`'s progress log, begun `at`, goes: `lodi/logs` in their state folder,
    /// owned as their home is.
    pub fn logs(&self, command: &str, at: i64) -> Option<crate::progress::Logs> {
        let state = self.state.as_ref()?;
        Some(crate::progress::Logs {
            dir: state.join("lodi/logs"),
            command: command.into(),
            at,
            owner: home_owner(self),
        })
    }

    /// Under `sudo` (euid 0 with a `SUDO_UID` that is not 0), or else under `doas` (euid 0 with
    /// a `DOAS_USER` that is not root), the person behind it, from the root's passwd: their
    /// passwd home replaces `HOME`, and `XDG_STATE_HOME` is not read.
    pub fn of(identity: &Identity) -> Result<User, Diagnostic> {
        let root = identity.root.clone();
        let hostname = identity.hostname.clone();
        let passwd = root.join("etc/passwd");
        let unknown = |who: String| {
            Diagnostic::new(
                "E_CONFIG",
                format!("{who} is not a user {} names", passwd.display()),
            )
            .hint("run lodi as yourself, without sudo or doas")
        };
        let sudo_uid = identity
            .sudo_uid
            .as_deref()
            .and_then(crate::hostscope::source::parse_id)
            .filter(|uid| identity.euid == 0 && *uid != 0);
        let doas_user = identity
            .doas_user
            .as_deref()
            .filter(|login| identity.euid == 0 && !login.is_empty());
        let behind = match (sudo_uid, doas_user) {
            (Some(uid), _) => Some(
                crate::passwd::by_uid(&root, uid)
                    .ok_or_else(|| unknown(format!("SUDO_UID {uid}")))?,
            ),
            (None, Some(login)) => {
                let named = login
                    .to_str()
                    .and_then(|name| crate::passwd::by_name(&root, name));
                Some(
                    named
                        .ok_or_else(|| unknown(format!("DOAS_USER {}", login.to_string_lossy())))?,
                )
                .filter(|entry| entry.uid != 0)
            }
            (None, None) => None,
        };
        if let Some(entry) = behind {
            let home = normal(&root.join(entry.home.trim_start_matches('/')));
            return Ok(User {
                uid: entry.uid,
                login: Some(entry.name),
                gid: Some(entry.gid),
                state: Some(home.join(".local/state")),
                home: Some(home),
                xdg_config: None,
                data: None,
                sudo: true,
                root,
                hostname,
            });
        }
        let entry = crate::passwd::by_uid(&root, identity.euid);
        let home = identity.home.clone().filter(|home| home.is_absolute());
        let state = identity
            .state_home
            .clone()
            .filter(|state| state.is_absolute())
            .or_else(|| home.as_ref().map(|home| home.join(".local/state")));
        Ok(User {
            uid: identity.euid,
            login: entry.as_ref().map(|entry| entry.name.clone()),
            gid: entry.as_ref().map(|entry| entry.gid),
            home,
            state,
            xdg_config: identity.config_home.clone().filter(|p| p.is_absolute()),
            data: identity.data_home.clone().filter(|p| p.is_absolute()),
            sudo: false,
            root,
            hostname,
        })
    }

    /// The remembered-path file.
    fn remembered(&self) -> Option<PathBuf> {
        Some(self.state.as_ref()?.join("lodi").join(REMEMBERED))
    }
}

/// The hint of every stop that found no config.
const HINT: &str = "type the config's path or URL once (`lodi switch ~/dotfiles`), set LODI_REPO, \
                    or make one with `lodi import`";

/// The config of one invocation: the typed path or URL, `LODI_REPO` (`env`), the remembered
/// path, `~/.config/lodi`, else a stop.
pub fn find(
    typed: Option<&OsStr>,
    env: Option<&OsStr>,
    identity: &Identity,
) -> Result<Found, Diagnostic> {
    let user = User::of(identity)?;
    if let Some(typed) = typed {
        return checked(location(typed, Origin::Typed)?, Origin::Typed, user);
    }
    if let Some(env) = env.filter(|env| !env.is_empty()) {
        return checked(
            location(env, Origin::Environment)?,
            Origin::Environment,
            user,
        );
    }
    if let Some(file) = user.remembered()
        && let Some(remembered) = read_remembered(&file)?
    {
        return checked(remembered, Origin::Remembered, user);
    }
    let standard = user.home.as_ref().map(|home| home.join(STANDARD));
    if let Some(standard) = &standard
        && standard.symlink_metadata().is_ok()
    {
        return checked(Location::Local(standard.clone()), Origin::Standard, user);
    }
    let standard = standard.map_or("~/.config/lodi".into(), |path| path.display().to_string());
    Err(Diagnostic::new(
        "E_NO_MANIFEST",
        format!(
            "no config found: no path was typed, LODI_REPO is not set, no path is remembered, \
             and {standard} does not exist"
        ),
    )
    .hint(HINT))
}

/// Where `lodi import` writes (#693): [`find`]'s order, except that a typed, `LODI_REPO` or
/// standard folder need not exist or be a config yet. A remembered folder that is gone stops.
pub fn find_for_import(
    typed: Option<&OsStr>,
    env: Option<&OsStr>,
    identity: &Identity,
) -> Result<Found, Diagnostic> {
    let user = User::of(identity)?;
    let found = |location, origin| {
        Ok(Found {
            location,
            origin,
            user: user.clone(),
        })
    };
    if let Some(typed) = typed {
        return found(location(typed, Origin::Typed)?, Origin::Typed);
    }
    if let Some(env) = env.filter(|env| !env.is_empty()) {
        return found(location(env, Origin::Environment)?, Origin::Environment);
    }
    if let Some(file) = user.remembered()
        && let Some(remembered) = read_remembered(&file)?
    {
        if let Location::Local(dir) = &remembered
            && !dir.is_dir()
        {
            return Err(Diagnostic::new(
                "E_NO_MANIFEST",
                format!(
                    "{} (the remembered path) is not a folder that exists",
                    dir.display()
                ),
            )
            .hint("type the folder to import into once, to replace the remembered path"));
        }
        return found(remembered, Origin::Remembered);
    }
    match &user.home {
        Some(home) => found(Location::Local(home.join(STANDARD)), Origin::Standard),
        None => Err(Diagnostic::new(
            "E_NO_MANIFEST",
            "HOME is not set, so ~/.config/lodi is nowhere",
        )
        .hint("type the folder to import into")),
    }
}

impl Origin {
    /// How a stop names this origin.
    pub fn words(self) -> &'static str {
        from(self)
    }
}

/// Judge `dir`, or when it is missing the nearest folder above it that exists, and every folder
/// above that, by the trust rules a config is read under.
pub fn judge_folder(dir: &Path, user: &User) -> Result<(), Diagnostic> {
    let existing = dir
        .ancestors()
        .find(|at| at.symlink_metadata().is_ok())
        .unwrap_or(Path::new("/"));
    Judge::for_user(user).walk(existing)
}

/// How a stop names where a config came from.
fn from(origin: Origin) -> &'static str {
    match origin {
        Origin::Typed => "the path typed",
        Origin::Environment => "LODI_REPO",
        Origin::Remembered => "the remembered path",
        Origin::Standard => "the standard folder",
    }
}

/// `location` as the config of this invocation, once a folder is one.
fn checked(location: Location, origin: Origin, user: User) -> Result<Found, Diagnostic> {
    if let Location::Local(dir) = &location {
        if !dir.is_dir() {
            return Err(Diagnostic::new(
                "E_NO_MANIFEST",
                format!(
                    "{} ({}) is not a folder that exists",
                    dir.display(),
                    from(origin)
                ),
            )
            .hint(HINT));
        }
        Judge::for_user(&user).walk(dir).map_err(|mut refused| {
            refused.message = format!("{} (from {})", refused.message, from(origin));
            refused
        })?;
        let stop = |why: String| {
            Diagnostic::new(
                "E_NO_MANIFEST",
                format!("{} ({}) {why}", dir.display(), from(origin)),
            )
        };
        if dir.join(crate::lock::MANIFEST_FILE).exists() {
            return Err(
                stop("is a project (it holds lodi.toml), not a config".into())
                    .hint("name the config itself, not a project"),
            );
        }
        if let Some(root) = enclosing(dir, &user) {
            return Err(stop(format!("is inside the config {}", root.display()))
                .hint(format!("name the config's root, {}", root.display())));
        }
        if !is_config(dir) {
            return Err(stop(
                "holds no config: no lodi.lock, config.toml, host.toml or home.toml".into(),
            )
            .hint(HINT));
        }
    }
    Ok(Found {
        location,
        origin,
        user,
    })
}

/// A config's layout: flat, or the hosts its `config.toml` names.
#[derive(Debug, Clone)]
pub struct Config {
    /// The config root: every path of the layout is below it, and `lodi.lock` sits here.
    pub root: PathBuf,
    user: User,
    /// The hosts `config.toml` names, by name; `None` for a flat config.
    hosts: Option<BTreeMap<String, Entry>>,
}

/// One `[hosts.NAME]` of `config.toml`: its `host.toml`, and each login's `home.toml`, as
/// normal paths from the root.
#[derive(Debug, Clone)]
struct Entry {
    host: String,
    homes: BTreeMap<String, String>,
}

/// One manifest a selection names: where it is, and its key in `lodi.lock`, its path from the
/// root. The paths a manifest refers to resolve against the folder holding it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Part {
    pub path: PathBuf,
    pub key: String,
}

/// Which parts of this host a command reads: a part left out is never opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parts {
    pub host: bool,
    pub home: bool,
}

impl Parts {
    pub const BOTH: Parts = Parts {
        host: true,
        home: true,
    };
}

/// The host and home one command acts on. `name` is the host's name in `config.toml`; a flat
/// config's one host has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub name: Option<String>,
    pub host: Option<Part>,
    pub home: Option<Part>,
}

/// Read the layout of the config at `dir` (a found folder, or a fetched URL's checkout) for
/// `user`, judging the root and every file read.
pub fn read_layout(dir: &Path, user: &User) -> Result<Config, Diagnostic> {
    let judge = Judge::for_user(user);
    judge.walk(dir)?;
    let index = dir.join(INDEX);
    let hosts = if index.symlink_metadata().is_ok() {
        let mut bytes = Vec::new();
        judge
            .open(&index, FILE_FLAGS)?
            .take(MAX_INDEX as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|e| bad(&index, &format!("cannot be read ({e})")))?;
        if bytes.len() > MAX_INDEX {
            return Err(bad(&index, &format!("is over {MAX_INDEX} bytes")));
        }
        let text = String::from_utf8(bytes).map_err(|_| bad(&index, "is not UTF-8"))?;
        Some(parse_index(&index, &text)?)
    } else {
        None
    };
    Ok(Config {
        root: dir.to_path_buf(),
        user: user.clone(),
        hosts,
    })
}

/// A `config.toml` that cannot be used.
fn bad(index: &Path, why: &str) -> Diagnostic {
    Diagnostic::new("E_CONFIG", format!("{} {why}", index.display()))
}

/// Refuse a key of `table` (at `at`) that is not one of `known`.
fn only(
    index: &Path,
    at: &str,
    table: &dyn toml_edit::TableLike,
    known: &[&str],
) -> Result<(), Diagnostic> {
    match table.iter().find(|(key, _)| !known.contains(key)) {
        Some((key, _)) => {
            let key = if at.is_empty() {
                key.to_string()
            } else {
                format!("{at}.{key}")
            };
            Err(bad(index, &format!("has an unknown key `{key}`")))
        }
        None => Ok(()),
    }
}

/// `path` of the key `at` as a normal path from the root, or a refusal when it could leave it.
fn inside(index: &Path, at: &str, path: &str) -> Result<String, Diagnostic> {
    let parts: Vec<&str> = Path::new(path)
        .components()
        .filter(|part| *part != Component::CurDir)
        .map(|part| match part {
            Component::Normal(name) => name.to_str(),
            _ => None,
        })
        .collect::<Option<_>>()
        .unwrap_or_default();
    if parts.is_empty() || path.contains('\0') {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!(
                "{} gives `{at}` the path {path:?}, which is not a relative path inside the \
                 config (no leading /, no ..)",
                index.display()
            ),
        ));
    }
    Ok(parts.join("/"))
}

/// The hosts of `config.toml`'s text.
fn parse_index(index: &Path, text: &str) -> Result<BTreeMap<String, Entry>, Diagnostic> {
    let doc: toml_edit::DocumentMut = text
        .parse()
        .map_err(|e| bad(index, &format!("is not TOML: {e}")))?;
    let mut hosts = BTreeMap::new();
    only(index, "", doc.as_table(), &["hosts"])?;
    let Some(table) = doc.get("hosts") else {
        return Ok(hosts);
    };
    let table = table
        .as_table_like()
        .ok_or_else(|| bad(index, "names `hosts` as something other than a table"))?;
    for (name, item) in table.iter() {
        let at = format!("hosts.{name}");
        if !safe_component(name) {
            return Err(bad(
                index,
                &format!("names a host `{name}` that is not one safe name"),
            ));
        }
        let entry = item.as_table_like().ok_or_else(|| {
            bad(
                index,
                &format!("names `{at}` as something other than a table"),
            )
        })?;
        only(index, &at, entry, &["host", "homes"])?;
        let host = entry
            .get("host")
            .and_then(|host| host.as_str())
            .ok_or_else(|| bad(index, &format!("gives `{at}` no `host` path")))?;
        let host = inside(index, &format!("{at}.host"), host)?;
        let mut homes = BTreeMap::new();
        if let Some(listed) = entry.get("homes") {
            let listed = listed.as_table_like().ok_or_else(|| {
                bad(
                    index,
                    &format!("names `{at}.homes` as something other than a table"),
                )
            })?;
            for (login, path) in listed.iter() {
                let key = format!("{at}.homes.{login}");
                if !safe_component(login) {
                    return Err(bad(
                        index,
                        &format!("names a login `{login}` that is not one safe name"),
                    ));
                }
                let path = path
                    .as_str()
                    .ok_or_else(|| bad(index, &format!("gives `{key}` no path")))?;
                homes.insert(login.to_string(), inside(index, &key, path)?);
            }
        }
        hosts.insert(name.to_string(), Entry { host, homes });
    }
    Ok(hosts)
}

impl Config {
    /// The host names `config.toml` lists, sorted; none for a flat config.
    pub fn hosts(&self) -> Vec<String> {
        self.hosts
            .as_ref()
            .map(|hosts| hosts.keys().cloned().collect())
            .unwrap_or_default()
    }

    /// The keys `config.toml` gives the host `name`: its host file and the invoking user's
    /// home, opened or not (`lodi import` fills in a file an entry names before it is there).
    pub fn listed(&self, name: &str) -> Option<(String, Option<String>)> {
        let entry = self.hosts.as_ref()?.get(name)?;
        let home = (self.user.login.as_ref()).and_then(|login| entry.homes.get(login).cloned());
        Some((entry.host.clone(), home))
    }

    /// The home file the first host (by name) that lists one for `login` gives it.
    pub fn home_of(&self, login: &str) -> Option<String> {
        self.hosts
            .as_ref()?
            .values()
            .find_map(|entry| entry.homes.get(login).cloned())
    }

    /// This host: in a flat config the top-level `host.toml`, whatever the hostname; with
    /// `config.toml` the host named as this machine's hostname, else a stop listing the hosts,
    /// so that one machine is never given another's host.
    pub fn this_host(&self) -> Result<Selection, Diagnostic> {
        self.this_host_parts(Parts::BOTH)
    }

    /// [`Config::this_host`] with only the `parts` asked for: a part left out is never opened
    /// (`lodi switch --host` judges no home path).
    pub fn this_host_parts(&self, parts: Parts) -> Result<Selection, Diagnostic> {
        match &self.hosts {
            None => self.flat(parts),
            Some(_) => self.select(&self.user.hostname, "this machine's hostname", parts),
        }
    }

    /// The host `--host NAME` names, else the only host (for `update`, `pin`, `unpin`).
    pub fn named_or_only(&self, name: Option<&str>) -> Result<Selection, Diagnostic> {
        match (&self.hosts, name) {
            (None, None) => self.flat(Parts::BOTH),
            (None, Some(name)) => Err(Diagnostic::new(
                USAGE,
                format!(
                    "--host {name} chooses a host that {INDEX} names, and {} has no {INDEX}: \
                     its one host is its host.toml; drop --host",
                    self.root.display()
                ),
            )),
            (Some(_), Some(name)) => self.select(name, "--host", Parts::BOTH),
            (Some(hosts), None) if hosts.len() == 1 => {
                let only = hosts.keys().next().cloned().unwrap_or_default();
                self.select(&only, "the only host", Parts::BOTH)
            }
            (Some(_), None) => Err(self.no_host("no --host was given").hint(format!(
                "choose one with --host NAME: {}",
                self.hosts().join(", ")
            ))),
        }
    }

    /// Every home of `selection`'s host: each one its `config.toml` entry names, or in a flat
    /// config the top-level `home.toml` (what `lodi update` locks with the host).
    pub fn homes(&self, selection: &Selection) -> Result<Vec<Part>, Diagnostic> {
        let entry = selection
            .name
            .as_ref()
            .and_then(|name| self.hosts.as_ref()?.get(name));
        match entry {
            Some(entry) => entry
                .homes
                .values()
                .filter_map(|key| self.part(key, true).transpose())
                .collect(),
            None => Ok(selection.home.iter().cloned().collect()),
        }
    }

    /// The host `name` of `config.toml`, and the invoking user's home on it.
    fn select(&self, name: &str, by: &str, parts: Parts) -> Result<Selection, Diagnostic> {
        let Some(entry) = self.hosts.as_ref().and_then(|hosts| hosts.get(name)) else {
            return Err(self.no_host(&format!("it names no host {name:?} ({by})")));
        };
        let home = match self
            .user
            .login
            .as_ref()
            .and_then(|login| entry.homes.get(login))
        {
            Some(key) if parts.home => self.part(key, true)?,
            _ => None,
        };
        let host = if parts.host {
            self.part(&entry.host, true)?
        } else {
            None
        };
        Ok(Selection {
            name: Some(name.to_string()),
            host,
            home,
        })
    }

    fn no_host(&self, why: &str) -> Diagnostic {
        let hosts = self.hosts();
        let listed = if hosts.is_empty() {
            "it lists no host".to_string()
        } else {
            format!("the hosts it lists: {}", hosts.join(", "))
        };
        Diagnostic::new(
            "E_NO_MANIFEST",
            format!("{}: {why}; {listed}", self.root.join(INDEX).display()),
        )
    }

    fn flat(&self, parts: Parts) -> Result<Selection, Diagnostic> {
        let host = if parts.host {
            self.part(HOST, false)?
        } else {
            None
        };
        let home = if parts.home {
            self.part(HOME, false)?
        } else {
            None
        };
        Ok(Selection {
            name: None,
            host,
            home,
        })
    }

    /// The manifest at `key` below the root, judged; `None` when it is not there, unless it was
    /// named (`required`), which stops naming it.
    fn part(&self, key: &str, required: bool) -> Result<Option<Part>, Diagnostic> {
        let path = self.root.join(key);
        if path.symlink_metadata().is_err() {
            if !required {
                return Ok(None);
            }
            return Err(Diagnostic::new(
                "E_NO_MANIFEST",
                format!(
                    "{} names {key}, and {} is not there",
                    self.root.join(INDEX).display(),
                    path.display()
                ),
            )
            .hint(format!("create {}, or fix {INDEX}", path.display())));
        }
        let judge = Judge::for_user(&self.user);
        let meta = judge
            .open(&path, FILE_FLAGS)?
            .metadata()
            .map_err(|e| Judge::refuse(&path, &format!("it could not be inspected ({e})")))?;
        if !meta.is_file() {
            return Err(Judge::refuse(&path, "it is not a regular file"));
        }
        Ok(Some(Part {
            path,
            key: key.to_string(),
        }))
    }
}

/// The rule a refusal of [`Judge`] states.
const RULE: &str = "lodi reads a config only when every folder from / down to it, and every file \
                    it reads there, is not a symbolic link, belongs to root or to you (under sudo, \
                    the user who ran sudo), is not writable by others, nor by a group other than \
                    your own private group, and is not on NFS or SMB; fix what the message names";

/// The host-source rules of `crate::hostscope::source`, with one relaxation (#684): group write
/// is accepted when the group is the owner's private group — its gid is the owner's primary gid,
/// its name the owner's login, and it lists no members.
struct Judge<'a> {
    root: &'a Path,
    owners: Vec<u32>,
    private: Vec<(u32, u32)>,
}

impl<'a> Judge<'a> {
    fn for_user(user: &'a User) -> Judge<'a> {
        let owners = crate::hostscope::source::owners_for(user.uid, None);
        let private = private_groups(user, &owners);
        Judge {
            root: &user.root,
            owners,
            private,
        }
    }

    fn refuse(path: &Path, why: &str) -> Diagnostic {
        Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("{} is not trusted as a config: {why}", path.display()),
        )
        .hint(RULE)
    }

    /// Judge one open entry by its descriptor.
    fn judge(&self, path: &Path, handle: &std::fs::File) -> Result<std::fs::Metadata, Diagnostic> {
        let meta = handle
            .metadata()
            .map_err(|e| Judge::refuse(path, &format!("it could not be inspected ({e})")))?;
        let mut mode = meta.mode();
        if self.private.contains(&(meta.uid(), meta.gid())) {
            mode &= !0o020;
        }
        if let Some(why) = crate::hostscope::source::untrusted(meta.uid(), mode, &self.owners) {
            return Err(Judge::refuse(path, &why));
        }
        // SAFETY: `stat` is plain data the kernel fills; the descriptor is open for the call.
        let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstatfs(handle.as_raw_fd(), &mut stat) } != 0 {
            return Err(Judge::refuse(path, "its filesystem could not be read"));
        }
        #[allow(clippy::unnecessary_cast)]
        if let Some(kind) = crate::hostscope::source::refused_filesystem(stat.f_type as i64) {
            return Err(Judge::refuse(path, &format!("it is on {kind}")));
        }
        Ok(meta)
    }

    /// Open `path` with `flags` by walking to it from the root on descriptors, judging every
    /// step, never through a link.
    fn open(&self, path: &Path, flags: i32) -> Result<std::fs::File, Diagnostic> {
        let below = path
            .strip_prefix(self.root)
            .map_err(|_| Judge::refuse(path, &format!("it is outside {}", self.root.display())))?;
        let opened = |at: &Path, e: std::io::Error| {
            let why = match e.raw_os_error() {
                Some(libc::ELOOP) => "it is a symbolic link".to_string(),
                Some(libc::ENOTDIR) => "it is not a folder".to_string(),
                _ => format!("it could not be opened ({e})"),
            };
            Judge::refuse(at, &why)
        };
        let mut handle =
            open_at(None, self.root.as_os_str(), DIR_FLAGS).map_err(|e| opened(self.root, e))?;
        self.judge(self.root, &handle)?;
        let mut cursor = self.root.to_path_buf();
        let parts: Vec<_> = below.components().collect();
        for (i, part) in parts.iter().enumerate() {
            cursor.push(part.as_os_str());
            let last = i + 1 == parts.len();
            let step = if last { flags } else { DIR_FLAGS };
            handle =
                open_at(Some(&handle), part.as_os_str(), step).map_err(|e| opened(&cursor, e))?;
            self.judge(&cursor, &handle)?;
        }
        Ok(handle)
    }

    /// Judge the folder `dir` and every folder above it.
    fn walk(&self, dir: &Path) -> Result<(), Diagnostic> {
        self.open(dir, DIR_FLAGS).map(drop)
    }
}

/// The `(uid, gid)` of each of `owners` whose private group may write what lodi reads for
/// `user` (#684): the group of their passwd gid, named as their login, listing no members.
pub fn private_groups(user: &User, owners: &[u32]) -> Vec<(u32, u32)> {
    let groups = crate::passwd::groups(&user.root);
    owners
        .iter()
        .filter_map(|uid| {
            let entry = crate::passwd::by_uid(&user.root, *uid)?;
            groups
                .iter()
                .any(|g| g.gid == entry.gid && g.name == entry.name && g.members.is_empty())
                .then_some((*uid, entry.gid))
        })
        .collect()
}

/// Whether `dir` is a config: it holds `lodi.lock` or `config.toml` (its root, LD-491), or before
/// a first switch a top-level `host.toml` or `home.toml`, and no `lodi.toml`, which makes it a
/// project. `lodi import`'s refusal to write inside another config asks the same.
pub fn is_config(dir: &Path) -> bool {
    !dir.join(crate::lock::MANIFEST_FILE).exists()
        && MARKERS.iter().any(|marker| dir.join(marker).exists())
}

/// The config `dir` is inside, if any: the nearest folder above it that [`is_config`], looking no
/// higher than the person's home folder, the root, or `/`, none of which is asked.
pub fn enclosing(dir: &Path, user: &User) -> Option<PathBuf> {
    dir.ancestors()
        .skip(1)
        .take_while(|at| {
            at.parent().is_some() && Some(*at) != user.home.as_deref() && *at != user.root
        })
        .find(|at| is_config(at))
        .map(Path::to_path_buf)
}

/// Under `sudo` or `doas`, who acts for the person: the owner of their home (or of its nearest
/// folder that exists), the rule every file written for them follows (LD-499, LD-528).
pub fn home_owner(user: &User) -> Option<(u32, u32)> {
    use std::os::unix::fs::MetadataExt;
    if !user.sudo {
        return None;
    }
    user.home_or_root()
        .ancestors()
        .find_map(|at| at.symlink_metadata().ok())
        .map(|meta| (meta.uid(), meta.gid()))
}

/// Remember a typed path or URL, once the command that found it succeeded; any other origin is
/// left as it is. The file is replaced atomically, in a folder created 0700, and under `sudo` it
/// and every folder created for it belong to the owner of the person's home ([`home_owner`]).
pub fn remember(found: &Found) -> Result<(), Diagnostic> {
    if found.origin != Origin::Typed {
        return Ok(());
    }
    let user = &found.user;
    let failed = |what: &Path, why: String| {
        Diagnostic::new(
            "E_CONFIG",
            format!("cannot remember the config in {}: {why}", what.display()),
        )
    };
    let Some(file) = user.remembered() else {
        return Err(failed(
            Path::new("~/.local/state/lodi"),
            "HOME is not set".into(),
        ));
    };
    let line = match &found.location {
        Location::Local(dir) => dir.to_str().map(str::to_string),
        Location::Url(url) => Some(url.clone()),
    }
    .filter(|line| line.len() <= MAX_REMEMBERED)
    .ok_or_else(|| failed(&file, "the path is not UTF-8, or too long".into()))?;
    let dir = file.parent().unwrap_or(Path::new("/"));
    let person = home_owner(user);
    let owner = |path: &Path| -> Result<(), Diagnostic> {
        if let Some((uid, gid)) = person {
            std::os::unix::fs::chown(path, Some(uid), Some(gid))
                .map_err(|e| failed(path, e.to_string()))?;
        }
        Ok(())
    };
    let missing: Vec<&Path> = dir.ancestors().take_while(|at| !at.exists()).collect();
    for at in missing.into_iter().rev() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(at)
            .map_err(|e| failed(at, e.to_string()))?;
        owner(at)?;
    }
    let staged = dir.join(format!(".{REMEMBERED}.{}", std::process::id()));
    let written = std::fs::write(&staged, format!("{line}\n"))
        .map_err(|e| failed(&staged, e.to_string()))
        .and_then(|()| owner(&staged))
        .and_then(|()| std::fs::rename(&staged, &file).map_err(|e| failed(&file, e.to_string())));
    if written.is_err() {
        let _ = std::fs::remove_file(&staged);
    }
    written
}

/// The remembered path: one line holding an absolute path or a URL. `Ok(None)` when nothing is
/// remembered.
fn read_remembered(file: &Path) -> Result<Option<Location>, Diagnostic> {
    let damaged = |why: &str| {
        Diagnostic::new(
            "E_CONFIG",
            format!("the remembered config path {} {why}", file.display()),
        )
        .hint("type the config's path or URL once to replace it")
    };
    let bytes = match std::fs::read(file) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(damaged(&format!("cannot be read ({e})"))),
    };
    let text = String::from_utf8(bytes).map_err(|_| damaged("is not UTF-8"))?;
    let line = text.strip_suffix('\n').unwrap_or(&text);
    if line.is_empty() || line.contains('\n') || line.len() > MAX_REMEMBERED {
        return Err(damaged("is not one line"));
    }
    location(OsStr::new(line), Origin::Remembered)
        .map(Some)
        .map_err(|_| damaged("holds neither an absolute path nor a URL"))
}

/// A URL exactly as given, or an absolute folder in normal form. A relative path is the
/// caller's to make absolute: this module never reads the current directory.
fn location(value: &OsStr, origin: Origin) -> Result<Location, Diagnostic> {
    match value.to_str() {
        Some(url) if crate::hostscope::remote::is_url(value) => Ok(Location::Url(url.to_string())),
        _ if Path::new(value).is_absolute() => Ok(Location::Local(normal(Path::new(value)))),
        _ => Err(Diagnostic::new(
            "E_NO_MANIFEST",
            format!(
                "{} ({}) is neither an absolute path nor a URL",
                Path::new(value).display(),
                from(origin)
            ),
        )
        .hint(HINT)),
    }
}

/// `path` with `.` dropped and `..` taken back, lexically: nothing is resolved through a link.
fn normal(path: &Path) -> PathBuf {
    let mut out = PathBuf::from("/");
    for part in path.components() {
        match part {
            Component::Normal(name) => out.push(name),
            Component::ParentDir => {
                out.pop();
            }
            Component::RootDir | Component::CurDir | Component::Prefix(_) => {}
        }
    }
    out
}
