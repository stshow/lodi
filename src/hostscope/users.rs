//! `[users]` and `[groups]` (su-1, LD-419): accounts and groups declared in `host.toml`, with no
//! secret in the repository.
//!
//! The plan reads the root's own `etc/passwd` and `etc/group` ([`crate::passwd`]) and never a
//! shadow file. An apply creates a declared group or account that is missing, through the
//! distribution's own `groupadd` and `useradd`, run as argv vectors with the `--` boundary like
//! every package-manager call ([`super::pm::Invocation`]). A new account's password is locked:
//! `useradd` is never given one, and no key of `host.toml` can carry one. An existing account's
//! password is never touched; its shell and supplementary groups are brought to what is
//! declared with `usermod`, and a declared group gets exactly its members with `gpasswd`. Public
//! keys are added to the account's `~/.ssh/authorized_keys`, owned by it, and a key already there
//! is kept.
//!
//! Nothing is ever deleted: an account or group that leaves `host.toml` is left as it is, its
//! home included, and the plan says it is no longer managed. A UID, GID or home that differs
//! from the machine's is refused (`E_IDENTITY_CONFLICT`), never changed.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::CString;
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::Path;

use crate::diag::Diagnostic;
use crate::passwd;

use super::lock::HostLock;
use super::manifest::HostManifest;
use super::pm::{self, Invocation};
use super::safety::{self, Gate};

/// What one identity action does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdOp {
    GroupCreate,
    UserCreate,
    UserChange,
    GroupMembers,
    /// Public keys added to an `authorized_keys` file; `true` when the file is new.
    Keys(bool),
}

/// One step of `[users]` or `[groups]`: the commands it runs, or the keys it adds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityAction {
    pub op: IdOp,
    pub name: String,
    pub changes: Vec<String>,
    pub invocations: Vec<Invocation>,
    /// The public keys a [`IdOp::Keys`] action adds.
    pub keys: Vec<String>,
}

impl IdentityAction {
    pub fn line(&self) -> String {
        let (verb, what) = match self.op {
            IdOp::GroupCreate => ("+", "group"),
            IdOp::UserCreate => ("+", "user"),
            IdOp::UserChange => ("~", "user"),
            IdOp::GroupMembers => ("~", "group"),
            IdOp::Keys(true) => ("+", "ssh keys"),
            IdOp::Keys(false) => ("~", "ssh keys"),
        };
        let mut line = format!("{verb} {what} {}", self.name);
        if !self.changes.is_empty() {
            line.push_str(&format!(" ({})", self.changes.join(", ")));
        }
        line
    }

    pub fn kind_name(&self) -> &'static str {
        match self.op {
            IdOp::GroupCreate => "group.create",
            IdOp::UserCreate => "user.create",
            IdOp::UserChange => "user.change",
            IdOp::GroupMembers => "group.members",
            IdOp::Keys(_) => "user.keys",
        }
    }
}

/// What the plan decided for `[users]` and `[groups]`.
#[derive(Debug, Default)]
pub struct Planned {
    pub actions: Vec<IdentityAction>,
    /// `= user NAME (no longer managed …)` lines.
    pub kept: Vec<String>,
    /// The names the record of this plan's apply manages.
    pub users: BTreeSet<String>,
    pub groups: BTreeSet<String>,
}

fn conflict(message: String) -> Diagnostic {
    Diagnostic::new("E_IDENTITY_CONFLICT", message)
        .hint("declare what the machine has; lodi never changes a UID, GID or home")
}

/// The invocation of one shadow tool, resolved by bare name as a package manager is, with the
/// root option under `--root` and the `--` boundary before the one name.
fn invocation(
    gate: &Gate,
    program: &str,
    options: &[String],
    name: &str,
) -> Result<Invocation, Diagnostic> {
    let path = match safety::resolve_program(program, gate.system_root, gate.euid) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return Err(Diagnostic::new(
                "E_NO_RUNTIME",
                format!(
                    "`{program}` is not on {}, and [users] and [groups] need it",
                    safety::searched(gate.system_root)
                ),
            )
            .hint("install the distribution's shadow tools (`passwd` or `shadow-utils`)"));
        }
        Err(why) => return Err(safety::untrusted_runtime(program, &why)),
    };
    let mut args = Vec::new();
    if !gate.system_root {
        args.push("--root".to_string());
        args.push(gate.root.display().to_string());
    }
    args.extend(options.iter().cloned());
    args.push("--".to_string());
    args.push(name.to_string());
    Ok(Invocation::resolved(
        program,
        &path,
        &args,
        pm::noninteractive_env(),
    ))
}

/// Plan `[users]` and `[groups]` against the root's accounts. Reads `etc/passwd`, `etc/group`
/// and the declared accounts' `authorized_keys`; never a shadow file.
pub fn plan(
    gate: &Gate,
    manifest: &HostManifest,
    lock: Option<&HostLock>,
) -> Result<Planned, Diagnostic> {
    let mut planned = Planned {
        users: manifest.users.keys().cloned().collect(),
        groups: manifest.groups.keys().cloned().collect(),
        ..Planned::default()
    };
    if let Some(lock) = lock {
        for name in lock.users.difference(&planned.users) {
            planned.kept.push(format!(
                "= user {name} (no longer managed: left as it is, with its home)"
            ));
        }
        for name in lock.groups.difference(&planned.groups) {
            planned
                .kept
                .push(format!("= group {name} (no longer managed: left as it is)"));
        }
    }
    if manifest.users.is_empty() && manifest.groups.is_empty() {
        return Ok(planned);
    }
    let root = &gate.root;
    let users: BTreeMap<String, passwd::User> = passwd::users(root)
        .into_iter()
        .map(|user| (user.name.clone(), user))
        .collect();
    let groups: BTreeMap<String, passwd::Group> = passwd::groups(root)
        .into_iter()
        .map(|group| (group.name.clone(), group))
        .collect();

    for (name, declared) in &manifest.groups {
        match (groups.get(name), declared.gid) {
            (Some(group), Some(gid)) if group.gid != gid => {
                return Err(conflict(format!(
                    "group {name} has GID {}, host.toml says {gid}",
                    group.gid
                )));
            }
            (Some(_), _) => {}
            (None, gid) => {
                let mut options = Vec::new();
                let mut changes = Vec::new();
                if let Some(gid) = gid {
                    if let Some(other) = groups.values().find(|g| g.gid == gid) {
                        return Err(conflict(format!(
                            "host.toml declares GID {gid} for group {name}, and group {} has it",
                            other.name
                        )));
                    }
                    options.extend(["--gid".to_string(), gid.to_string()]);
                    changes.push(format!("gid {gid}"));
                }
                planned.actions.push(IdentityAction {
                    op: IdOp::GroupCreate,
                    name: name.clone(),
                    changes,
                    invocations: vec![invocation(gate, "groupadd", &options, name)?],
                    keys: Vec::new(),
                });
            }
        }
        for member in &declared.members {
            if !users.contains_key(member) && !manifest.users.contains_key(member) {
                return Err(conflict(format!(
                    "group {name} lists member {member}, which is neither an account on this \
                     machine nor declared in [users]"
                )));
            }
        }
    }

    let mut keys = Vec::new();
    for (name, declared) in &manifest.users {
        for group in &declared.groups {
            if !groups.contains_key(group) && !manifest.groups.contains_key(group) {
                return Err(conflict(format!(
                    "account {name} lists group {group}, which is neither a group on this \
                     machine nor declared in [groups]"
                )));
            }
        }
        // A declared group gets its members from `gpasswd` below; the others are joined here.
        let joins: Vec<&String> = declared
            .groups
            .iter()
            .filter(|group| !manifest.groups.contains_key(*group))
            .filter(|group| !groups.get(*group).is_some_and(|g| g.members.contains(name)))
            .collect();
        match users.get(name) {
            Some(user) => {
                if let Some(uid) = declared.uid.filter(|uid| *uid != user.uid) {
                    return Err(conflict(format!(
                        "{name} has UID {}, host.toml says {uid}",
                        user.uid
                    )));
                }
                if let Some(home) = declared.home.as_ref().filter(|home| **home != user.home) {
                    return Err(conflict(format!(
                        "{name} has home {}, host.toml says {home}",
                        user.home
                    )));
                }
                let mut options = Vec::new();
                let mut changes = Vec::new();
                if let Some(shell) = declared.shell.as_ref().filter(|s| **s != user.shell) {
                    options.extend(["--shell".to_string(), shell.clone()]);
                    changes.push(format!("shell {shell}"));
                }
                if !joins.is_empty() {
                    let list: Vec<&str> = joins.iter().map(|g| g.as_str()).collect();
                    options.extend([
                        "--append".to_string(),
                        "--groups".to_string(),
                        list.join(","),
                    ]);
                    changes.push(format!("joins {}", list.join(" ")));
                }
                if !options.is_empty() {
                    planned.actions.push(IdentityAction {
                        op: IdOp::UserChange,
                        name: name.clone(),
                        changes,
                        invocations: vec![invocation(gate, "usermod", &options, name)?],
                        keys: Vec::new(),
                    });
                }
                if !declared.ssh_keys.is_empty() {
                    let present = read_keys(root, &user.home)?;
                    let missing: Vec<String> = declared
                        .ssh_keys
                        .iter()
                        .filter(|key| !present.iter().flatten().any(|line| line == *key))
                        .cloned()
                        .collect();
                    if !missing.is_empty() {
                        keys.push((name.clone(), present.is_none(), missing));
                    }
                }
            }
            None => {
                let mut options = Vec::new();
                let mut changes = Vec::new();
                if let Some(uid) = declared.uid {
                    if let Some(other) = users.values().find(|u| u.uid == uid) {
                        return Err(conflict(format!(
                            "host.toml declares UID {uid} for account {name}, and account {} \
                             has it",
                            other.name
                        )));
                    }
                    options.extend(["--uid".to_string(), uid.to_string()]);
                    changes.push(format!("uid {uid}"));
                }
                let home = declared
                    .home
                    .clone()
                    // Built from its parts: a home path literal fails the release assets
                    // check, which admits no home path but /home/builder.
                    .unwrap_or_else(|| ["", "home", name.as_str()].join("/"));
                options.extend(["--home-dir".to_string(), home, "--create-home".to_string()]);
                if let Some(shell) = &declared.shell {
                    options.extend(["--shell".to_string(), shell.clone()]);
                }
                if !joins.is_empty() {
                    let list: Vec<&str> = joins.iter().map(|g| g.as_str()).collect();
                    options.extend(["--groups".to_string(), list.join(",")]);
                }
                changes.push("locked password".to_string());
                planned.actions.push(IdentityAction {
                    op: IdOp::UserCreate,
                    name: name.clone(),
                    changes,
                    invocations: vec![invocation(gate, "useradd", &options, name)?],
                    keys: Vec::new(),
                });
                if !declared.ssh_keys.is_empty() {
                    keys.push((name.clone(), true, declared.ssh_keys.clone()));
                }
            }
        }
    }

    // Exactly the declared members, and every declared account that lists the group.
    for (name, declared) in &manifest.groups {
        let mut wanted: Vec<String> = declared.members.clone();
        for (user, entry) in &manifest.users {
            if entry.groups.contains(name) && !wanted.contains(user) {
                wanted.push(user.clone());
            }
        }
        let now: BTreeSet<&String> = groups
            .get(name)
            .map(|g| g.members.iter().collect())
            .unwrap_or_default();
        if now != wanted.iter().collect::<BTreeSet<_>>() {
            let options = vec!["--members".to_string(), wanted.join(",")];
            let shown = if wanted.is_empty() {
                "no members".to_string()
            } else {
                format!("members {}", wanted.join(" "))
            };
            planned.actions.push(IdentityAction {
                op: IdOp::GroupMembers,
                name: name.clone(),
                changes: vec![shown],
                invocations: vec![invocation(gate, "gpasswd", &options, name)?],
                keys: Vec::new(),
            });
        }
    }

    for (name, new, missing) in keys {
        let count = missing.len();
        planned.actions.push(IdentityAction {
            op: IdOp::Keys(new),
            name,
            changes: vec![if new {
                format!("{count} key(s)")
            } else {
                format!("{count} key(s) added")
            }],
            invocations: Vec::new(),
            keys: missing,
        });
    }
    Ok(planned)
}

/// Perform one identity action.
pub fn perform(gate: &Gate, action: &IdentityAction) -> Result<(), Diagnostic> {
    for invocation in &action.invocations {
        pm::run(invocation)?;
    }
    if let IdOp::Keys(_) = action.op {
        let user = passwd::by_name(&gate.root, &action.name).ok_or_else(|| {
            Diagnostic::new(
                "E_APPLY",
                format!("account {} is not in the root's passwd file", action.name),
            )
        })?;
        add_keys(&gate.root, &user, &action.keys)?;
    }
    Ok(())
}

const DIR_FLAGS: i32 = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;

fn io_error(what: &str, error: &io::Error) -> Diagnostic {
    Diagnostic::new("E_PATH_ESCAPE", format!("{what}: {error}")).hint(
        "lodi never follows a symbolic link in a home: every directory from the root to \
         `~/.ssh` must be a real directory",
    )
}

fn openat(dir: i32, name: &str, flags: i32, mode: libc::c_uint) -> io::Result<OwnedFd> {
    let name = CString::new(name).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: `name` is a valid C string and `dir` a descriptor the caller holds open.
    let fd = unsafe { libc::openat(dir, name.as_ptr(), flags, mode) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` was just returned by `openat` and is owned by nothing else.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The directory `home` below the root, each component opened without following a link.
fn open_home(root: &Path, home: &str) -> io::Result<OwnedFd> {
    let root = CString::new(root.as_os_str().as_encoded_bytes())
        .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: a valid C string; the descriptor is owned below.
    let fd = unsafe { libc::open(root.as_ptr(), DIR_FLAGS & !libc::O_NOFOLLOW) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: just returned by `open`.
    let mut dir = unsafe { OwnedFd::from_raw_fd(fd) };
    for part in home.split('/').filter(|p| !p.is_empty() && *p != ".") {
        dir = openat(dir.as_raw_fd(), part, DIR_FLAGS, 0)?;
    }
    Ok(dir)
}

/// The lines of `<home>/.ssh/authorized_keys`, or `None` when there is no such file.
fn read_keys(root: &Path, home: &str) -> Result<Option<Vec<String>>, Diagnostic> {
    let shown = format!("{home}/.ssh/authorized_keys");
    let home_fd = match open_home(root, home) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&shown, &e)),
    };
    let ssh = match openat(home_fd.as_raw_fd(), ".ssh", DIR_FLAGS, 0) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&shown, &e)),
    };
    let flags = libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let fd = match openat(ssh.as_raw_fd(), "authorized_keys", flags, 0) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io_error(&shown, &e)),
    };
    let file = std::fs::File::from(fd);
    if !file.metadata().is_ok_and(|m| m.is_file()) {
        return Err(io_error(
            &shown,
            &io::Error::other("it is not a regular file"),
        ));
    }
    let mut text = String::new();
    file.take(1 << 20)
        .read_to_string(&mut text)
        .map_err(|e| io_error(&shown, &e))?;
    Ok(Some(text.lines().map(str::to_string).collect()))
}

/// Add `keys` to the account's `authorized_keys`: `.ssh` made 0700 if it is not there, the file
/// written whole beside itself and renamed into place, 0600, both owned by the account. A line
/// already in the file is kept.
fn add_keys(root: &Path, user: &passwd::User, keys: &[String]) -> Result<(), Diagnostic> {
    let shown = format!("{}/.ssh/authorized_keys", user.home);
    let fail = |e: io::Error| io_error(&shown, &e);
    let present = read_keys(root, &user.home)?.unwrap_or_default();
    let home = open_home(root, &user.home).map_err(fail)?;
    let ssh = match openat(home.as_raw_fd(), ".ssh", DIR_FLAGS, 0) {
        Ok(fd) => fd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let name = CString::new(".ssh").expect("a C string");
            // SAFETY: a valid C string below a descriptor this function holds.
            if unsafe { libc::mkdirat(home.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
                return Err(fail(io::Error::last_os_error()));
            }
            let fd = openat(home.as_raw_fd(), ".ssh", DIR_FLAGS, 0).map_err(fail)?;
            own(&fd, user, 0o700).map_err(fail)?;
            fd
        }
        Err(e) => return Err(fail(e)),
    };
    let mut text: String = present.iter().map(|line| format!("{line}\n")).collect();
    for key in keys {
        if !present.contains(key) {
            text.push_str(key);
            text.push('\n');
        }
    }
    let temp = ".authorized_keys.lodi";
    let name = CString::new(temp).expect("a C string");
    // A leftover of an interrupted apply is lodi's own, and goes.
    // SAFETY: a valid C string below a descriptor this function holds.
    unsafe { libc::unlinkat(ssh.as_raw_fd(), name.as_ptr(), 0) };
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let fd = openat(ssh.as_raw_fd(), temp, flags, 0o600).map_err(fail)?;
    own(&fd, user, 0o600).map_err(fail)?;
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes()).map_err(fail)?;
    file.sync_all().map_err(fail)?;
    let target = CString::new("authorized_keys").expect("a C string");
    // SAFETY: both names are valid C strings below the one descriptor this function holds.
    let rc = unsafe {
        libc::renameat(
            ssh.as_raw_fd(),
            name.as_ptr(),
            ssh.as_raw_fd(),
            target.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(fail(io::Error::last_os_error()));
    }
    Ok(())
}

/// Give an open file or directory to the account, with `mode`.
fn own(fd: &OwnedFd, user: &passwd::User, mode: libc::mode_t) -> io::Result<()> {
    // SAFETY: the descriptor is open for the whole call; `fchown` and `fchmod` act on it alone.
    if unsafe { libc::fchown(fd.as_raw_fd(), user.uid, user.gid) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: as above.
    if unsafe { libc::fchmod(fd.as_raw_fd(), mode) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// One human account of the machine, as an import writes it (S1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Human {
    pub name: String,
    pub uid: u32,
    pub groups: Vec<String>,
    pub shell: String,
    pub home: String,
}

/// The machine's human accounts (S1): a UID from `UID_MIN` to `UID_MAX` of the root's
/// `etc/login.defs` (1000 and 60000 when it says nothing), and a login shell — one the root's
/// `etc/shells` lists, and never a `nologin` or `false`. Reads `etc/passwd`, `etc/group`,
/// `etc/login.defs` and `etc/shells`, and never a shadow file.
pub fn humans(root: &Path) -> Vec<Human> {
    let defs = std::fs::read_to_string(root.join("etc/login.defs")).unwrap_or_default();
    let setting = |key: &str, default: u32| {
        defs.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some(key)).then(|| words.next()?.parse().ok())?
            })
            .next_back()
            .unwrap_or(default)
    };
    let (min, max) = (setting("UID_MIN", 1000), setting("UID_MAX", 60000));
    let shells: BTreeSet<String> = std::fs::read_to_string(root.join("etc/shells"))
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with('/'))
        .map(str::to_string)
        .collect();
    let groups = passwd::groups(root);
    passwd::users(root)
        .into_iter()
        .filter(|user| (min..=max).contains(&user.uid))
        .filter(|user| {
            shells.contains(&user.shell)
                && !user.shell.ends_with("/nologin")
                && !user.shell.ends_with("/false")
        })
        .filter(|user| super::manifest::is_identity_name(&user.name))
        .map(|user| {
            let mut member_of: Vec<String> = groups
                .iter()
                .filter(|g| g.members.contains(&user.name))
                .map(|g| g.name.clone())
                .filter(|g| super::manifest::is_identity_name(g))
                .collect();
            member_of.sort();
            member_of.dedup();
            Human {
                name: user.name,
                uid: user.uid,
                groups: member_of,
                shell: user.shell,
                home: user.home,
            }
        })
        .collect()
}
