//! Small shared helpers: SHA-256 digests and RFC 3339 UTC timestamps.

use sha2::{Digest, Sha256};

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// `sha256:<hex>`, the lock's hash notation (design `spec/02` §1).
pub fn sha256_tagged(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// The hex part of `sha256:<64 lowercase hex>`, or `None` if `text` is not in that form.
pub fn untag_sha256(text: &str) -> Option<&str> {
    let hex = text.strip_prefix("sha256:")?;
    crate::debian::index::is_sha256_hex(hex).then_some(hex)
}

/// Days since 1970-01-01 of a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (m + if m > 2 { -3 } else { 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (
        if m <= 2 {
            yoe + era * 400 + 1
        } else {
            yoe + era * 400
        },
        m,
        d,
    )
}

/// Seconds since the Unix epoch of `YYYY-MM-DDTHH:MM:SSZ` (the manifest's snapshot format,
/// already validated by the manifest loader), or `None` if `text` is not in that form.
pub fn parse_utc(text: &str) -> Option<i64> {
    let b = text.as_bytes();
    if b.len() != 20 || b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' {
        return None;
    }
    if b[16] != b':' || b[19] != b'Z' {
        return None;
    }
    let num = |r: std::ops::Range<usize>| text.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, s) = (num(11..13)?, num(14..16)?, num(17..19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    // Reject dates such as 2026-02-30 that the day-count arithmetic would roll over.
    if civil_from_days(days) != (y, mo, d) {
        return None;
    }
    Some(days * 86400 + h * 3600 + mi * 60 + s)
}

/// `YYYY-MM-DDTHH:MM:SSZ` for seconds since the Unix epoch.
pub fn format_utc(secs: i64) -> String {
    let (y, m, d) = civil_from_days(secs.div_euclid(86400));
    let rem = secs.rem_euclid(86400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// The snapshot service's timestamp form `YYYYMMDDTHHMMSSZ`.
pub fn snapshot_id(secs: i64) -> String {
    format_utc(secs).replace(['-', ':'], "")
}

/// The current time in seconds since the Unix epoch.
pub fn now_utc() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The SELinux label an existing file carries, if it carries one.
const LABEL: &[u8] = b"security.selinux\0";

/// Give `temp`, written beside `destination` to replace it, the SELinux label `destination`
/// carries, so that the rename leaves the destination with the label it had (LD-432). A file
/// system with no label changes nothing, and a destination not there yet takes the label the
/// policy gives its path ([`policy_label`]). Nothing is ever relabelled but the one file being
/// written, and enforcement is never touched.
pub fn keep_label(destination: &std::path::Path, temp: &std::fs::File) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    let Ok(path) = std::ffi::CString::new(destination.as_os_str().as_bytes()) else {
        return Ok(());
    };
    let mut want = vec![0u8; 256];
    // SAFETY: `path` and the attribute name are NUL-terminated and `want` is writable for its
    // length; `lgetxattr` never follows a final symbolic link.
    let len = unsafe {
        libc::lgetxattr(
            path.as_ptr(),
            LABEL.as_ptr().cast(),
            want.as_mut_ptr().cast(),
            want.len(),
        )
    };
    let Ok(len) = usize::try_from(len) else {
        return match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::ENOENT) => policy_label(destination, temp),
            Some(libc::ENODATA | libc::ENOTSUP) => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        };
    };
    want.truncate(len);
    if label_of(temp).is_some_and(|have| have == want) {
        return Ok(());
    }
    set_label(temp, &want)
}

/// Where libselinux-utils installs `matchpathcon`, which prints the label the policy's file
/// contexts give a path: the one `restorecon` sets.
const MATCHPATHCON: &str = "/usr/sbin/matchpathcon";

/// Give `file`, created new for `path`, the type the policy's file contexts give `path`, as
/// `restorecon` would (#422). A new file otherwise takes the type its creator's domain is given
/// in its directory: under a service's domain a file in `/etc` is `etc_runtime_t` where the
/// policy says `etc_t`. Only the type changes, as with `restorecon` without `-F`. A file with no
/// label (no SELinux), or no root-owned `matchpathcon` to ask the policy, changes nothing.
pub fn policy_label(path: &std::path::Path, file: &std::fs::File) -> std::io::Result<()> {
    let Some(have) = label_of(file) else {
        return Ok(());
    };
    match policy_context(path).and_then(|policy| relabelled(&have, &policy)) {
        Some(label) => set_label(file, &label),
        None => Ok(()),
    }
}

/// What `matchpathcon` prints for a regular file at the absolute `path`, when the program is
/// root-owned and writable by nobody else.
fn policy_context(path: &std::path::Path) -> Option<String> {
    use std::os::unix::fs::MetadataExt;
    let meta = std::fs::metadata(MATCHPATHCON).ok()?;
    if !path.is_absolute() || meta.uid() != 0 || meta.mode() & 0o022 != 0 {
        return None;
    }
    let output = std::process::Command::new(MATCHPATHCON)
        .args(["-n", "-m", "file"])
        .arg(path)
        .env_clear()
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// `current`, a label, with its type replaced by `policy`'s; `None` when the two types agree or
/// either is not a label (`user:role:type[:level]`).
fn relabelled(current: &[u8], policy: &str) -> Option<Vec<u8>> {
    let current = std::str::from_utf8(current).ok()?.trim_end_matches('\0');
    let mut parts: Vec<&str> = current.splitn(4, ':').collect();
    let want = policy.trim().split(':').nth(2).filter(|t| !t.is_empty())?;
    if parts.len() < 3 || parts[2] == want {
        return None;
    }
    parts[2] = want;
    Some(parts.join(":").into_bytes())
}

/// The label `file` carries, or `None` when it carries none.
fn label_of(file: &std::fs::File) -> Option<Vec<u8>> {
    use std::os::fd::AsRawFd;
    let mut have = vec![0u8; 256];
    // SAFETY: the attribute name is NUL-terminated and `have` is writable for its length, on
    // the descriptor `file` owns.
    let got = unsafe {
        libc::fgetxattr(
            file.as_raw_fd(),
            LABEL.as_ptr().cast(),
            have.as_mut_ptr().cast(),
            have.len(),
        )
    };
    have.truncate(usize::try_from(got).ok()?);
    Some(have)
}

/// Set `label` on `file`.
fn set_label(file: &std::fs::File, label: &[u8]) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    // SAFETY: the attribute name is NUL-terminated and `label` holds its length in bytes, on the
    // descriptor `file` owns.
    let rc = unsafe {
        libc::fsetxattr(
            file.as_raw_fd(),
            LABEL.as_ptr().cast(),
            label.as_ptr().cast(),
            label.len(),
            0,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A new file takes the type the policy gives its path and keeps the rest of the label its
    /// creator's domain gave it, as `restorecon` without `-F` does (#422).
    #[test]
    fn a_new_file_takes_the_policy_type_and_keeps_the_rest_of_its_label() {
        let policy = "system_u:object_r:etc_t:s0\n";
        assert_eq!(
            relabelled(b"system_u:object_r:etc_runtime_t:s0\0", policy),
            Some(b"system_u:object_r:etc_t:s0".to_vec())
        );
        assert_eq!(
            relabelled(
                b"unconfined_u:object_r:etc_runtime_t:s0-s0:c0.c1023",
                policy
            ),
            Some(b"unconfined_u:object_r:etc_t:s0-s0:c0.c1023".to_vec())
        );
        assert_eq!(
            relabelled(b"unconfined_u:object_r:etc_t:s0\0", policy),
            None
        );
        assert_eq!(
            relabelled(b"system_u:object_r:etc_runtime_t:s0", "<<none>>\n"),
            None
        );
        assert_eq!(relabelled(b"not a label", policy), None);
    }

    #[test]
    fn digests() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let tagged = sha256_tagged(b"abc");
        assert_eq!(untag_sha256(&tagged), Some(&tagged[7..]));
        assert_eq!(untag_sha256("sha256:ABC"), None);
        assert_eq!(untag_sha256("md5:x"), None);
    }

    #[test]
    fn timestamps() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        let t = parse_utc("2026-09-18T00:00:00Z").unwrap();
        assert_eq!(t, 1789689600);
        assert_eq!(format_utc(t), "2026-09-18T00:00:00Z");
        assert_eq!(snapshot_id(t + 3661), "20260918T010101Z");
        assert_eq!(
            format_utc(parse_utc("2024-02-29T23:59:59Z").unwrap()),
            "2024-02-29T23:59:59Z"
        );
        for bad in [
            "2026-02-30T00:00:00Z",
            "2026-09-18 00:00:00Z",
            "2026-09-18T24:00:00Z",
            "x",
        ] {
            assert_eq!(parse_utc(bad), None, "{bad}");
        }
    }
}
