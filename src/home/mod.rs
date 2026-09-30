//! The home scope (ADR-011, M-0.5): the user's own files, described by `home.toml` and applied
//! into the roots [`crate::roots`] read from the environment.
//!
//! - [`fsops`] is the only module in the tree permitted to mutate a file below a root, and
//!   `tests/home_containment.rs` is the gate that proves it (M-0.5 T-1, design call D4).
//! - [`manifest`] is the loader of `<config>/home.toml`, a sibling of [`crate::manifest`]'s
//!   project schema rather than an extension of it (M-0.5 T-2).
//! - [`plan`] is `lodi home plan`, which shows what an apply would do and **writes nothing**
//!   (M-0.5 T-2, design call D12). Since T-3 it computes against the state record too, so it is
//!   the same computation [`apply`] performs.
//! - [`state`] is `<data>/home-scope/state.json`, the record of what Lodi wrote and what it
//!   promised to do when an entry disappears (design call D5).
//! - [`backup`] is `<data>/home-scope/backups/`, the one copy Lodi keeps of what was at a path
//!   before it took it over — taken once, never overwritten (design call D5).
//! - [`apply`] is `lodi home apply` and `lodi home status` (M-0.5 T-3): the apply refuses to
//!   touch anything when a managed file has been edited by hand, and the status only reports.
//! - [`import`] is `lodi home init` and its 1.0 name `lodi home import` (M-Import T-4,
//!   rewritten by M-Home im-1, LD-341; `init` since LD-382): it writes a commented starting
//!   `home.toml` — a stub per program on `PATH`, `[tools]`, a `[home.file]` example — and reads
//!   no file in the home directory.
//! - [`sudo_warning`] is the `W_HOME_SUDO` line a home verb prints under `sudo` (LD-382).
//! - [`programs`] is the program-module framework of M-Home (`[programs.<name>]`) and its
//!   registry of shipped modules; [`render`] holds the deterministic writers the modules
//!   render with, and [`toml`] the owned TOML model a module reads its table as.
//! - [`services`] is `[services]`: the user's own systemd units, run as the user (hc-1).
//! - [`lock`] and [`tools`] pin and realize the user-wide tool set; [`profile`] turns it into the
//!   one PATH directory and generated POSIX script T-5 exposes.

pub mod apply;
pub mod backup;
pub mod fsops;
pub mod import;
pub mod lock;
pub mod manifest;
pub mod plan;
pub mod profile;
pub mod programs;
pub mod render;
pub mod services;
pub mod source;
pub mod state;
pub mod toml;
pub mod tools;

/// The `W_HOME_SUDO` line (LD-382), or `None`: a pure function of the effective uid and
/// `SUDO_UID`, so it is tested without a root process. It names no directory: only
/// [`crate::roots`] reads `HOME`, and the line is printed before the roots are read.
///
/// `sudo` sets `SUDO_UID` to the uid of the person who typed the command. When that is a uid and
/// not the uid this process runs as, the home verb is about to act on the home the environment
/// names — root's, under a plain `sudo` — and not on that person's. The home scope never needs
/// root, so the line says whose home this is and how to reach one's own. An unset, empty or
/// non-numeric `SUDO_UID`, or the process's own uid, is not `sudo` changing who acts, and gets no
/// line. It warns and never refuses: a root that manages its own home is allowed (`crate::roots`
/// refuses a root process a home that is not root's), and a warning never changes an exit status.
pub fn sudo_warning(euid: u32, sudo_uid: Option<&std::ffi::OsStr>) -> Option<String> {
    let invoker: u32 = sudo_uid?.to_str()?.parse().ok()?;
    if invoker == euid {
        return None;
    }
    Some(format!(
        "lodi: warning W_HOME_SUDO: this runs under sudo as uid {euid}, so it acts on the home \
         HOME names for uid {euid}, not on the home of uid {invoker} who ran sudo; the home scope \
         needs no root: run lodi home without sudo"
    ))
}

/// [`sudo_warning`] for this process: its effective uid and `SUDO_UID`.
pub fn sudo_warning_here() -> Option<String> {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    sudo_warning(euid, std::env::var_os("SUDO_UID").as_deref())
}

#[cfg(test)]
mod tests {
    use super::sudo_warning;
    use std::ffi::OsStr;

    #[test]
    fn sudo_warns_only_when_it_changed_who_acts() {
        let line = sudo_warning(0, Some(OsStr::new("1000"))).expect("a plain sudo");
        assert!(line.starts_with("lodi: warning W_HOME_SUDO: "), "{line}");
        assert!(line.contains("for uid 0,") && line.contains("uid 1000 who ran sudo"));
        // `sudo -u alice` typed by bob acts on alice's home: warned too.
        assert!(sudo_warning(1001, Some(OsStr::new("1000"))).is_some());
        for quiet in [
            None,
            Some(""),
            Some("0"),
            Some("root"),
            Some("-1"),
            Some(" 1000"),
        ] {
            assert_eq!(sudo_warning(0, quiet.map(OsStr::new)), None, "{quiet:?}");
        }
    }
}
