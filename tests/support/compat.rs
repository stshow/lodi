//! A compatibility walk (M-Pin P3): one host manifest planned, applied and planned again against
//! the fake machine of `tests/support/fakepm.py`, with the `host.lock` and the journal the apply
//! left — the plan output, every argv the journal records and the machine record — normalized
//! so that two binaries' transcripts compare byte for byte. It is the walk of
//! `tests/host_sources.rs` (`compat_1_2_*`, LD-365), with the release it holds a build to named
//! by the caller.
//!
//! What cannot hold still is a placeholder, the same way for both binaries: the scratch root
//! `<root>`, the fake machine's directory `<fake>`, every UTC instant `<time>`, a journal id
//! `<journal>`, a backup's stamp, the invoking user's ids, and the running binary's own version,
//! written back as the recorded one.

#![allow(dead_code)]

use std::fs;
use std::path::Path;

use crate::fakehost::{Case, err, out};
use crate::hostroot;

/// The binary a walk runs: the one `LODI_COMPAT_BINARY` names while recording, this build's
/// otherwise.
pub fn binary() -> std::ffi::OsString {
    std::env::var_os("LODI_COMPAT_BINARY")
        .unwrap_or_else(|| std::ffi::OsString::from(env!("CARGO_BIN_EXE_lodi")))
}

/// Whether this run records goldens rather than checking them.
pub fn recording() -> bool {
    std::env::var_os("LODI_RECORD_EXPECTED").is_some()
        && std::env::var_os("LODI_COMPAT_BINARY").is_some()
}

/// Every `YYYY-MM-DDTHH:MM:SSZ` and `YYYYMMDDTHHMMSSZ-<32 hex>`, masked.
pub fn mask_times(text: &str) -> String {
    let bytes: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    let digit = |j: usize, from: &[char]| from.get(j).is_some_and(|c| c.is_ascii_digit());
    while i < bytes.len() {
        let iso = (0..4).all(|k| digit(i + k, &bytes))
            && bytes.get(i + 4) == Some(&'-')
            && bytes.get(i + 10) == Some(&'T')
            && bytes.get(i + 13) == Some(&':');
        if iso {
            let mut j = i + 19;
            while bytes
                .get(j)
                .is_some_and(|c| c.is_ascii_digit() || *c == '.')
            {
                j += 1;
            }
            if bytes.get(j) == Some(&'Z') {
                out.push_str("<time>");
                i = j + 1;
                continue;
            }
        }
        let stamp = (0..8).all(|k| digit(i + k, &bytes))
            && bytes.get(i + 8) == Some(&'T')
            && (9..15).all(|k| digit(i + k, &bytes))
            && bytes.get(i + 15) == Some(&'Z');
        if stamp {
            let mut j = i + 16;
            if bytes.get(j) == Some(&'-') {
                let mut k = j + 1;
                while bytes.get(k).is_some_and(|c| c.is_ascii_hexdigit()) {
                    k += 1;
                }
                if k - j - 1 == 32 {
                    out.push_str("<journal>");
                    i = k;
                    continue;
                }
                if k - j - 1 == 9 {
                    j = k;
                }
            }
            out.push_str("<stamp>");
            i = j;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    out
}

pub fn normalize(case: &Case, text: &str, recorded: &str) -> String {
    let (uid, gid) = hostroot::ids(&case.root);
    let mut text = text
        .replace(&case.root.dir.display().to_string(), "<root>")
        .replace(&case.base().display().to_string(), "<fake>")
        .replace(
            &format!("lodi {}", lodi::schema::LODI_VERSION),
            &format!("lodi {recorded}"),
        )
        .replace(
            &format!("\"lodiVersion\":\"{}\"", lodi::schema::LODI_VERSION),
            &format!("\"lodiVersion\":\"{recorded}\""),
        );
    text = mask_times(&text);
    for (prefix, id) in [("\"uid\":", uid), ("\"gid\":", gid), ("\"euid\":", uid)] {
        text = text.replace(&format!("{prefix}{id}"), &format!("{prefix}<id>"));
    }
    text.replace(&format!("owner {uid}:{gid}"), "owner <uid>:<gid>")
        .replace(&format!("\"owner\": \"{uid}\""), "\"owner\": \"<uid>\"")
        .replace(&format!("\"group\": \"{gid}\""), "\"group\": \"<gid>\"")
        .replace(&format!("owner = \"{uid}\""), "owner = \"<uid>\"")
        .replace(&format!("group = \"{gid}\""), "group = \"<gid>\"")
}

/// One walk: plan, apply, the lock and the journal it left, and the plan after it.
pub fn walk(case: &Case, recorded: &str) -> String {
    walk_with(case, recorded, &[])
}

/// [`walk`], each verb given `extra` (a host directory's `SOURCE`).
pub fn walk_with(case: &Case, recorded: &str, extra: &[&str]) -> String {
    let binary = binary();
    let legacy = !recording() && has_files_table(case);
    let run = |label: &str, verb: &str| {
        let output = case.verb_by(&binary, verb, extra);
        format!(
            "== {label}: exit {:?}\n-- stdout\n{}-- stderr\n{}",
            output.status.code(),
            out(&output),
            without_legacy_warning(&err(&output), legacy)
        )
    };
    let mut transcript = run("plan", "plan");
    transcript.push_str(&run("apply", "apply"));
    let lock = case.root.read("etc/lodi/host.lock");
    let journal = hostroot::journals(&case.root)
        .first()
        .map(|path| fs::read_to_string(path).unwrap())
        .unwrap_or_default();
    transcript.push_str(&format!("== host.lock\n{lock}\n== journal\n{journal}"));
    transcript.push_str(&run("plan after", "plan"));
    normalize(case, &transcript, recorded)
}

/// Whether a manifest the walk reads — in place, or in a host directory below the root — has
/// the legacy `[files]` table, whose one warning this build prints and an earlier one did not.
pub fn has_files_table(case: &Case) -> bool {
    let mut manifests = vec![case.root.path("etc/lodi/host.toml")];
    if let Ok(hosts) = fs::read_dir(case.root.path("hosts")) {
        manifests.extend(hosts.flatten().map(|host| host.path().join("host.toml")));
    }
    manifests.iter().any(|path| {
        fs::read_to_string(path).is_ok_and(|text| text.lines().any(|l| l.starts_with("[files.")))
    })
}

/// Standard error with the one `W_LEGACY_FILES` line (si-1) taken out, after asserting it was
/// there exactly when the manifest has `[files]`: that line is the one difference an earlier
/// release's `host.toml` shows.
pub fn without_legacy_warning(stderr: &str, legacy: bool) -> String {
    let (warned, rest): (Vec<&str>, Vec<&str>) = stderr
        .split_inclusive('\n')
        .partition(|line| line.starts_with("W_LEGACY_FILES: "));
    assert_eq!(
        warned.len(),
        usize::from(legacy),
        "the [files] warning:\n{stderr}"
    );
    rest.concat()
}

/// The names a committed import manifest declares.
pub fn declared_names(text: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if line.starts_with("  \"") && trimmed.ends_with("\",") {
            let name = trimmed.trim_end_matches(',').trim_matches('"').to_string();
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    names
}

/// A manifest with the `[system]` table an import writes since si-1 taken out, header to blank
/// line: the file an earlier release's import wrote, as far as a plan and an apply read it.
pub fn without_system(text: &str) -> String {
    let Some(start) = text.find("[system]\n") else {
        return text.to_string();
    };
    let end = text[start..]
        .find("\n\n")
        .map_or(text.len(), |at| start + at + 2);
    format!("{}{}", &text[..start], &text[end..])
}

/// The journal's `manifestDigest`, masked: a committed import manifest's comments are this
/// build's, so its digest is not the one an earlier release recorded (si-1).
pub fn mask_manifest_digest(text: &str) -> String {
    let key = "\"manifestDigest\":\"sha256:";
    let mut out = String::new();
    let mut rest = text;
    while let Some(at) = rest.find(key) {
        let after = at + key.len();
        out.push_str(&rest[..after]);
        out.push_str("<digest>");
        rest = &rest[after..];
        rest = &rest[rest.find('"').unwrap_or(rest.len())..];
    }
    out.push_str(rest);
    out
}

/// [`check`] with `mask` applied to both the transcript and the golden.
pub fn check_masked(
    dir: &Path,
    name: &str,
    transcript: &str,
    recorded: &str,
    mask: fn(&str) -> String,
) {
    if recording() {
        check(dir, name, transcript, recorded);
        return;
    }
    let path = dir.join(format!("{name}.txt"));
    let golden = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; record it (README)", path.display()));
    assert_eq!(
        mask(transcript),
        mask(&golden),
        "{name}: this build differs from {recorded} ({})",
        path.display()
    );
}

/// Compare `transcript` with the golden `dir/name.txt`, or write it while recording.
pub fn check(dir: &Path, name: &str, transcript: &str, recorded: &str) {
    let path = dir.join(format!("{name}.txt"));
    if recording() {
        fs::create_dir_all(dir).unwrap();
        fs::write(&path, transcript).unwrap();
        return;
    }
    let golden = fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{}: {e}; record it (README)", path.display()));
    assert_eq!(
        transcript,
        golden,
        "{name}: this build differs from {recorded} ({})",
        path.display()
    );
}
