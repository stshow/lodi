//! Select the invoking user's home below the chosen host without opening another user's home.

use std::fs;
use std::path::{Component, Path};

use crate::diag::Diagnostic;
use crate::home;
use crate::roots::Roots;

use super::{HostError, privilege::Privilege, safety::Gate, source};

pub struct Selected {
    pub name: String,
    pub roots: Roots,
    pub manifest: home::manifest::HomeManifest,
    pub lock: Option<crate::lock::LockFile>,
    /// Where the home's tool lock is written when the host lives in a repository (LD-416).
    pub target: Option<crate::flakelock::HomeTarget>,
    /// The user the home belongs to, from the root's passwd.
    pub uid: u32,
    pub gid: u32,
    /// Root turned lingering on for this home just before it ran (H1, [`linger`]).
    pub linger_enabled: bool,
}

/// One `home/<login>/` of a host directory, as `lodi apply` finds it (F-21): read and checked
/// as root, or a login the root's passwd does not name, which is skipped visibly.
pub enum Declared {
    Home(Box<Selected>),
    Missing(String),
}

pub fn selected_user(
    gate: &Gate,
    privilege: &dyn Privilege,
) -> Result<crate::passwd::User, HostError> {
    let uid = privilege.sudo().map_or(gate.euid, |(uid, _)| uid);
    let user = crate::passwd::by_uid(&gate.root, uid).ok_or_else(|| {
        Diagnostic::new(
            "E_CONFIG",
            format!("uid {uid} is not in the root's /etc/passwd"),
        )
        .hint("add the user to the root's passwd file, or use --no-home")
    })?;
    if !source::safe_component(&user.name) {
        return Err(Diagnostic::new(
            "E_PATH_ESCAPE",
            format!("passwd login {:?} is not one safe component", user.name),
        )
        .into());
    }
    let passwd_home = Path::new(&user.home);
    if !passwd_home.is_absolute()
        || passwd_home
            .components()
            .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
    {
        return Err(Diagnostic::new("E_CONFIG", "passwd home is not an absolute safe path").into());
    }
    Ok(user)
}

/// Root under `sudo` becomes `SUDO_UID:SUDO_GID` for good, with no group beyond `SUDO_GID`,
/// before the home part reads or writes anything of the home; root without `sudo` plans and
/// applies `home/root/` as itself, and a user is already who the home belongs to. The home
/// scope's own privilege guard then judges the passwd roots, as it judges the environment's.
pub fn drop_to_user(privilege: &dyn Privilege, roots: &Roots) -> Result<(), HostError> {
    if privilege.euid() == 0
        && let Some((uid, gid)) = privilege.sudo()
    {
        privilege.become_user(uid, gid)?;
    }
    roots.check_privilege()?;
    Ok(())
}

/// An absent `home/` preserves the host-only path, including roots without a passwd entry.
pub fn select(
    gate: &Gate,
    host: &source::Host,
    privilege: &dyn Privilege,
) -> Result<Option<Selected>, HostError> {
    let homes = host.dir.join("home");
    match fs::symlink_metadata(&homes) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("cannot inspect {}: {error}", homes.display()),
            )
            .into());
        }
        Ok(_) => {}
    }
    let user = selected_user(gate, privilege)?;
    let selected = select_user(gate, host, user)?;
    if selected.is_some() {
        install_recipes(host)?;
    }
    Ok(selected)
}

/// The repository's own recipes, read here as root with everything else before the host changes,
/// for the homes to resolve through (LD-409).
fn install_recipes(host: &source::Host) -> Result<(), HostError> {
    let recipes = crate::catalogue::user::for_repository(host)?;
    crate::catalogue::user::install(Ok(recipes));
    Ok(())
}

/// Every `home/<login>/` of the host with a `home.toml`, in login order (F-21, LD-416): each is
/// read and checked here, as root, before anything changes; a login the root's passwd does not
/// name is [`Declared::Missing`]. An absent `home/` is no home at all, and reads no passwd.
pub fn declared(gate: &Gate, host: &source::Host) -> Result<Vec<Declared>, HostError> {
    let homes = host.dir.join("home");
    match fs::symlink_metadata(&homes) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("cannot inspect {}: {error}", homes.display()),
            )
            .into());
        }
        Ok(meta) if !meta.is_dir() => {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("{} is not a directory", homes.display()),
            )
            .into());
        }
        Ok(_) => {}
    }
    let mut logins: Vec<String> = fs::read_dir(&homes)
        .map_err(|error| {
            Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("cannot read {}: {error}", homes.display()),
            )
        })?
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();
    logins.sort();
    let mut out = Vec::new();
    for login in logins {
        if !source::safe_component(&login) {
            return Err(Diagnostic::new(
                "E_PATH_ESCAPE",
                format!("home/{login:?} is not one safe component"),
            )
            .into());
        }
        let Some(user) = crate::passwd::by_name(&gate.root, &login) else {
            out.push(Declared::Missing(login));
            continue;
        };
        let passwd_home = Path::new(&user.home);
        if !passwd_home.is_absolute()
            || passwd_home
                .components()
                .any(|part| matches!(part, Component::ParentDir | Component::CurDir))
        {
            return Err(Diagnostic::new(
                "E_CONFIG",
                format!("the passwd home of {login} is not an absolute safe path"),
            )
            .into());
        }
        if let Some(selected) = select_user(gate, host, user)? {
            out.push(Declared::Home(Box::new(selected)));
        }
    }
    if out.iter().any(|home| matches!(home, Declared::Home(_))) {
        install_recipes(host)?;
    }
    Ok(out)
}

/// The home of `user` below the host, read and checked as root: `None` when it has no
/// `home.toml`.
pub fn select_user(
    gate: &Gate,
    host: &source::Host,
    user: crate::passwd::User,
) -> Result<Option<Selected>, HostError> {
    let prefix = format!("home/{}", user.name);
    let relative = format!("{prefix}/home.toml");
    let Some(bytes) = source::read_file(host, &relative, home::manifest::MAX_HOME_MANIFEST_BYTES)?
    else {
        return Ok(None);
    };
    let roots = Roots::for_host(
        gate.root.join(user.home.trim_start_matches('/')),
        host.dir.join("home").join(&user.name),
    );
    let base = roots.config_root();
    let manifest = home::manifest::parse_home_manifest_host_bytes(
        &bytes,
        &base.path().join("home.toml").display().to_string(),
        &base,
        home::manifest::HostSource {
            host,
            prefix: &prefix,
        },
    )?;
    let lock = source::read_file(
        host,
        &format!("{prefix}/home.lock"),
        super::MAX_SOURCE_BYTES,
    )?
    .map(|bytes| {
        if bytes.len() > super::MAX_SOURCE_BYTES {
            return Err(crate::lock::LockProblem::Invalid(format!(
                "home.lock exceeds {} bytes",
                super::MAX_SOURCE_BYTES
            )));
        }
        crate::lock::parse_lock(&bytes)
    })
    .transpose()
    .map_err(|problem| home::lock::diagnostic(&problem))?;
    // The home's section of the repository's root lock, else that `home.lock` (LD-416).
    let at = host.dir.join("home").join(&user.name);
    let lock = crate::flakelock::read_home(host, &at, lock)?;
    let target = crate::flakelock::HomeTarget::of(host, &at);
    Ok(Some(Selected {
        name: user.name,
        roots,
        manifest,
        lock,
        target,
        uid: user.uid,
        gid: user.gid,
        linger_enabled: false,
    }))
}

/// Root's half of lingering under `sudo lodi apply` (hc-1, H1): `loginctl enable-linger UID`
/// or `disable-linger UID`, run as root because logind lets a user change their own lingering
/// only from a local session. Returns whether it changed anything. Turning it on waits, briefly,
/// for the user manager it starts, so the home's services find it.
pub fn linger(uid: u32, system_root: bool, on: bool) -> Result<bool, Diagnostic> {
    use super::pm::{Invocation, noninteractive_env};
    let path = match super::safety::resolve_program("loginctl", system_root, 0) {
        Ok(Some(path)) => path,
        Ok(None) => {
            return Err(Diagnostic::new(
                "E_NO_RUNTIME",
                "`loginctl` is not on PATH, and a home.toml declares linger",
            )
            .hint("linger needs a machine that runs systemd-logind"));
        }
        Err(why) => return Err(super::safety::untrusted_runtime("loginctl", &why)),
    };
    let run = |args: &[&str]| {
        let argv: Vec<String> = args
            .iter()
            .map(|arg| (*arg).to_string())
            .chain([uid.to_string()])
            .collect();
        Invocation::resolved("loginctl", &path, &argv, noninteractive_env())
    };
    // `show-user` fails for a user with no session and no lingering: that is a "no".
    let shown = run(&["show-user", "--property=Linger", "--value"])
        .command()
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).trim() == "yes")
        .unwrap_or(false);
    if shown == on {
        return Ok(false);
    }
    super::pm::run(&run(&[if on {
        "enable-linger"
    } else {
        "disable-linger"
    }]))?;
    if on && system_root {
        let socket = std::path::PathBuf::from(format!("/run/user/{uid}/systemd/private"));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
    }
    Ok(true)
}

/// A fetched tree is never written, so its home applies only from the `home.lock` it carries:
/// one that is missing or does not match the home's tools is refused before the host changes
/// (LD-401, U7).
pub fn require_lock(home: &Selected) -> Result<(), HostError> {
    let Some(problem) = home::tools::locked_refusal(&home.manifest.tools, home.lock.as_ref())
    else {
        return Ok(());
    };
    let mut refused = home::lock::diagnostic(&problem);
    refused.notes.retain(|note| !note.starts_with("hint: "));
    Err(refused
        .hint(
            "a fetched tree is never written: lock its tools in a checkout with `lodi home \
             apply CHECKOUT` (in a repository, `lodi update CHECKOUT`), commit and push the \
             lock, then apply the URL with --refresh",
        )
        .into())
}
