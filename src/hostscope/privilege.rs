//! Who an import writes a host directory as (LD-379).
//!
//! An import of `SOURCE` under `sudo` reads the machine as root and writes the baseline into the
//! root-owned lock, and only then becomes the owner of `SOURCE` — `setgroups`, `setresgid`,
//! `setresuid`, for good — before it writes anything under it. The files are born that user's,
//! with no `chown` afterwards and so no window in which root has made something a user can
//! swap. `SUDO_UID` only ever lowers privilege: the owner of `SOURCE` must already be trusted
//! ([`super::source::trusted_owners`]), which under `sudo` means root or `SUDO_UID`, and a
//! `SOURCE` root owns is written as root.
//!
//! [`Privilege`] is the seam: [`System`] is this process, and a test hands in its own to prove
//! the order without being root.

use std::ffi::OsStr;
use std::path::Path;

use crate::diag::Diagnostic;

/// What the process is, and how it becomes someone less.
pub trait Privilege {
    /// The effective user id.
    fn euid(&self) -> u32;
    /// `SUDO_UID` and `SUDO_GID`, when both are plain decimal ids and the user is not root.
    fn sudo(&self) -> Option<(u32, u32)>;
    /// Become `uid`:`gid` for the rest of the process, with no supplementary group and no way
    /// back.
    fn become_user(&self, uid: u32, gid: u32) -> Result<(), Diagnostic>;
}

/// This process.
pub struct System;

impl Privilege for System {
    fn euid(&self) -> u32 {
        super::safety::current_euid()
    }

    fn sudo(&self) -> Option<(u32, u32)> {
        sudo_from(
            std::env::var_os("SUDO_UID").as_deref(),
            std::env::var_os("SUDO_GID").as_deref(),
        )
    }

    fn become_user(&self, uid: u32, gid: u32) -> Result<(), Diagnostic> {
        let fail = |what: &str| {
            Diagnostic::new(
                "E_NEED_ROOT",
                format!(
                    "could not become uid {uid} (gid {gid}) to write the host directory: {what} \
                     failed ({})",
                    std::io::Error::last_os_error()
                ),
            )
            .hint("nothing was written under the host directory")
        };
        let groups = [gid as libc::gid_t];
        // SAFETY: plain system calls; the group list is ours and its length is passed with it.
        // The order is the only one that works: groups and gid while still root, uid last.
        unsafe {
            if libc::setgroups(1, groups.as_ptr()) != 0 {
                return Err(fail("setgroups"));
            }
            if libc::setresgid(gid, gid, gid) != 0 {
                return Err(fail("setresgid"));
            }
            if libc::setresuid(uid, uid, uid) != 0 {
                return Err(fail("setresuid"));
            }
            let (mut r, mut e, mut s) = (0, 0, 0);
            if libc::getresuid(&mut r, &mut e, &mut s) != 0 || (r, e, s) != (uid, uid, uid) {
                return Err(fail("getresuid"));
            }
        }
        Ok(())
    }
}

/// `SUDO_UID` and `SUDO_GID` as [`Privilege::sudo`] reads them.
pub fn sudo_from(uid: Option<&OsStr>, gid: Option<&OsStr>) -> Option<(u32, u32)> {
    let uid = uid.and_then(super::source::parse_id)?;
    let gid = gid.and_then(super::source::parse_id)?;
    (uid != 0).then_some((uid, gid))
}

/// The owners a process of `privilege` trusts on `root` ([`super::source::owners_for`]): root,
/// and the person behind it — `SUDO_UID`, or else `DOAS_USER` in `root`'s passwd, the person
/// [`crate::config::User::of`] reads the config for.
pub fn owners(privilege: &dyn Privilege, root: &Path) -> Vec<u32> {
    let behind = privilege
        .sudo()
        .map(|(uid, _)| uid)
        .or_else(|| doas_uid(root));
    super::source::owners_for(privilege.euid(), behind)
}

/// `DOAS_USER`'s uid in `root`'s passwd.
fn doas_uid(root: &Path) -> Option<u32> {
    let login = std::env::var("DOAS_USER")
        .ok()
        .filter(|login| !login.is_empty())?;
    crate::passwd::by_name(root, &login).map(|entry| entry.uid)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sudo_is_read_only_as_two_plain_ids_for_a_user_who_is_not_root() {
        let s = |v: &str| Some(OsStr::new(v).to_owned());
        assert_eq!(
            sudo_from(s("1000").as_deref(), s("100").as_deref()),
            Some((1000, 100))
        );
        assert_eq!(sudo_from(s("0").as_deref(), s("0").as_deref()), None);
        assert_eq!(sudo_from(s("1000").as_deref(), None), None);
        assert_eq!(sudo_from(None, s("100").as_deref()), None);
        assert_eq!(sudo_from(s("x").as_deref(), s("100").as_deref()), None);
    }
}
