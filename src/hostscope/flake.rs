//! The top-level `lodi plan` and `lodi apply` of a repository (DONE D2, D3; F-21, F-22,
//! LD-416): the host as root, then every home the host directory declares, each as its own user.
//!
//! The host and every `home/<login>/` are read, and checked, before the first change: a home
//! that does not parse stops the whole apply with nothing written. The host then applies as root
//! and writes its lock. Each home is planned or applied in a child process of its own, which
//! drops to the user's passwd uid and gid for good (no other group) before it opens anything of
//! the home, so no home is ever written by root and no home's failure reaches another's process.
//! Before the first home runs, root locks every home's stale tools into the repository's one
//! `lodi.lock` in one write, as `lodi update` does, so a home's own process only reads its
//! section (#295). Homes go in login order; a login the root's passwd does not name is skipped
//! with a line; a home's failure is reported and the next home still runs, and the exit status
//! is the first failure's. fh-1's `lodi host plan|apply` keeps its one home of the invoking user
//! (LD-399).

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::{Read, Write};
use std::os::fd::FromRawFd;

use crate::diag::Diagnostic;

use super::homepart::{self, Declared, Selected};
use super::{HostError, Operation, Options};

/// What one home's child reported: its lines, or its failure's code and text.
enum Child {
    Done(String),
    /// Done, and root is to turn the home's lingering off (hc-1, H1).
    LingerOff(String),
    Failed {
        code: &'static str,
        text: String,
    },
}

/// The code `name` as the catalogue spells it, for a failure carried back from a child.
fn code_of(name: &str) -> &'static str {
    crate::diag::CODES
        .iter()
        .map(|(code, _)| *code)
        .find(|code| *code == name)
        .unwrap_or("E_APPLY")
}

fn encode(child: &Child) -> Vec<u8> {
    let (tag, a, b) = match child {
        Child::Done(lines) => (0u8, "", lines.as_str()),
        Child::LingerOff(lines) => (2u8, "", lines.as_str()),
        Child::Failed { code, text } => (1u8, *code, text.as_str()),
    };
    let mut out = vec![tag];
    for part in [a, b] {
        out.extend_from_slice(&(part.len() as u64).to_le_bytes());
        out.extend_from_slice(part.as_bytes());
    }
    out
}

fn decode(bytes: &[u8]) -> Option<Child> {
    let (&tag, mut rest) = bytes.split_first()?;
    let mut parts = Vec::new();
    for _ in 0..2 {
        let (len, tail) = rest.split_at_checked(8)?;
        let len = usize::try_from(u64::from_le_bytes(len.try_into().ok()?)).ok()?;
        let (part, tail) = tail.split_at_checked(len)?;
        parts.push(String::from_utf8(part.to_vec()).ok()?);
        rest = tail;
    }
    let text = parts.pop()?;
    let code = parts.pop()?;
    match tag {
        0 => Some(Child::Done(text)),
        2 => Some(Child::LingerOff(text)),
        1 => Some(Child::Failed {
            code: code_of(&code),
            text,
        }),
        _ => None,
    }
}

fn failed(d: &Diagnostic) -> Child {
    Child::Failed {
        code: d.code,
        text: d.to_string(),
    }
}

/// Run `work` for `home` in a child that first becomes the home's user for good. A process that
/// is not root can only be that user already; another user's home is refused, not attempted.
fn as_user(home: &Selected, verb: &str, work: impl FnOnce(&Selected) -> Child) -> Child {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors pipe2 fills in.
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return failed(&Diagnostic::new(
            "E_STORE_IO",
            format!(
                "no pipe for home {}: {}",
                home.name,
                std::io::Error::last_os_error()
            ),
        ));
    }
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    // SAFETY: the child only drops privileges, does the home's work, writes its report to the
    // pipe and leaves with `_exit`; nothing it inherits is used across the fork by both sides.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        // SAFETY: closing the two descriptors pipe2 returned.
        unsafe {
            libc::close(fds[0]);
            libc::close(fds[1]);
        }
        return failed(&Diagnostic::new(
            "E_APPLY",
            format!(
                "could not start the process of home {}: {}",
                home.name,
                std::io::Error::last_os_error()
            ),
        ));
    }
    if pid == 0 {
        // SAFETY: the child's own copy of the read end.
        unsafe { libc::close(fds[0]) };
        let report = drop_to(home, verb).map_or_else(|d| failed(&d), |()| work(home));
        // SAFETY: the write end this child owns.
        let mut pipe = unsafe { std::fs::File::from_raw_fd(fds[1]) };
        let _ = pipe.write_all(&encode(&report));
        drop(pipe);
        // SAFETY: leaving the child without running the parent's exit handlers.
        unsafe { libc::_exit(0) };
    }
    // SAFETY: the parent's copy of the write end, so that the read below ends with the child.
    unsafe { libc::close(fds[1]) };
    // SAFETY: the read end this process owns from here on.
    let mut pipe = unsafe { std::fs::File::from_raw_fd(fds[0]) };
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    let mut status = 0;
    // SAFETY: waiting for the child this function started.
    unsafe { libc::waitpid(pid, &mut status, 0) };
    decode(&bytes).unwrap_or_else(|| {
        failed(&Diagnostic::new(
            "E_APPLY",
            format!(
                "the process of home {} ended without a report (wait status {status})",
                home.name
            ),
        ))
    })
}

/// Become the home's user for good, or refuse: root drops to the passwd uid and gid with no
/// other group; a user can only be that user already.
fn drop_to(home: &Selected, verb: &str) -> Result<(), Diagnostic> {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    if euid == 0 {
        use super::privilege::Privilege;
        super::privilege::System.become_user(home.uid, home.gid)?;
    } else if euid != home.uid {
        return Err(Diagnostic::new(
            "E_NEED_ROOT",
            format!(
                "home {} belongs to uid {}, and only root {verb}s another user's home",
                home.name, home.uid
            ),
        )
        .hint(format!(
            "run `sudo lodi {verb}`; the host part is unchanged by this"
        )));
    }
    home.roots.check_privilege()
}

/// Every declared home of the loaded host, read as root before anything changes. A fetched
/// tree is read-only, so a home there with tools needs a fresh committed lock first (F-15).
fn declared(loaded: &super::Loaded, options: &Options) -> Result<Vec<Declared>, HostError> {
    if options.no_home {
        return Ok(Vec::new());
    }
    let homes = homepart::declared(&loaded.gate, &loaded.input.host)?;
    if loaded.input.git.is_some() {
        for home in &homes {
            if let Declared::Home(home) = home {
                homepart::require_lock(home)?;
            }
        }
    }
    Ok(homes)
}

/// #295: as root and before any home runs as its user, each home of a checkout whose tools lock
/// is stale is resolved as `lodi update` resolves it, and every such section goes into the one
/// root lock in one write, which also finishes a move into it cut short (#296). A home whose
/// tools this process could not apply anyway is left to its refusal; one whose tools cannot be
/// resolved or written fails alone, with the reason by its login.
fn lock_homes(
    root: &std::path::Path,
    host: &std::path::Path,
    declared: &mut [Declared],
    fetcher: &dyn crate::fetch::Fetcher,
    now: i64,
    out: &mut String,
) -> (BTreeMap<String, (&'static str, String)>, Option<Diagnostic>) {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    let mut refused = BTreeMap::new();
    let mut sections = Vec::new();
    let mut fresh = Vec::new();
    let mut olds = vec![host.join(super::pin::FILE)];
    for home in declared.iter_mut() {
        let Declared::Home(home) = home else { continue };
        let Some(target) = &home.target else { continue };
        let dir = target.root.join(&target.key);
        olds.push(dir.join(crate::lock::HOME_LOCK_FILE));
        if euid != 0 && euid != home.uid {
            continue;
        }
        let tools = home.manifest.tools.clone();
        match crate::home::tools::locked_set(tools, home.lock.as_ref(), fetcher, now) {
            Ok(lock) if lock != home.lock => {
                sections.push((target.key.clone(), lock.clone()));
                fresh.push((home.name.clone(), lock));
            }
            Ok(_) => {}
            Err(failure) => {
                let text: Vec<String> = failure.diagnostics.iter().map(|d| d.to_string()).collect();
                let code = failure.diagnostics.first().map_or("E_FETCH", |d| d.code);
                refused.insert(home.name.clone(), (code, text.join("\n")));
            }
        }
    }
    match crate::flakelock::settle(root, &sections, &olds) {
        Ok(lines) => {
            for line in lines {
                let _ = writeln!(out, "{line}");
            }
            for home in declared.iter_mut() {
                if let Declared::Home(home) = home
                    && let Some((_, lock)) = fresh.iter().find(|(name, _)| *name == home.name)
                {
                    home.lock = lock.clone();
                }
            }
            (refused, None)
        }
        Err(d) if fresh.is_empty() => (refused, Some(d)),
        Err(d) => {
            for (name, _) in fresh {
                refused.insert(name, (d.code, d.to_string()));
            }
            (refused, None)
        }
    }
}

/// The lines of the homes, and the failures, after the host part.
fn homes(
    homes: Vec<Declared>,
    verb: &str,
    out: &mut String,
    system_root: bool,
    work: impl Fn(&Selected) -> Child,
) -> Vec<Diagnostic> {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let root = unsafe { libc::geteuid() } == 0;
    let mut failures = Vec::new();
    for mut home in homes {
        // Root turns lingering on before a home that asks for it runs as its user (H1).
        if let Declared::Home(home) = &mut home
            && root
            && verb == "apply"
            && home.manifest.linger
        {
            match homepart::linger(home.uid, system_root, true) {
                Ok(changed) => home.linger_enabled = changed,
                Err(d) => {
                    let _ = writeln!(out, "home {} not {}", home.name, past(verb));
                    failures.push(d);
                    continue;
                }
            }
        }
        match home {
            Declared::Missing(login) => {
                let _ = writeln!(
                    out,
                    "home {login} skipped: the root's /etc/passwd names no {login}"
                );
            }
            Declared::Home(home) => match as_user(&home, verb, &work) {
                Child::LingerOff(lines) => {
                    let _ = writeln!(out, "home {}", home.name);
                    out.push_str(&lines);
                    if let Err(d) = homepart::linger(home.uid, system_root, false) {
                        failures.push(d);
                    }
                }
                Child::Done(lines) => {
                    let _ = writeln!(out, "home {}", home.name);
                    out.push_str(&lines);
                }
                Child::Failed { code, text } => {
                    let _ = writeln!(out, "home {} not {}", home.name, past(verb));
                    let prefix = format!("lodi: error {code}: ");
                    let text = text.strip_prefix(&prefix).unwrap_or(&text);
                    failures.push(Diagnostic::new(
                        code,
                        format!("home {} not {}: {text}", home.name, past(verb)),
                    ));
                }
            },
        }
    }
    failures
}

fn past(verb: &str) -> &'static str {
    if verb == "plan" { "planned" } else { "applied" }
}

fn finish(out: String, failures: Vec<Diagnostic>) -> Result<String, HostError> {
    if failures.is_empty() {
        return Ok(out);
    }
    Err(HostError {
        diagnostics: failures,
        done: out,
    })
}

/// `lodi apply`: the host as root, then every declared home as its user.
pub fn apply(options: &Options) -> Result<String, HostError> {
    let first = super::load(options, Operation::Apply)?;
    let mut declared = declared(&first, options)?;
    let fetched = first.input.git.is_some();
    // A fetched tree is never written (F-15); a checkout's root lock is written before the homes.
    let repo = crate::flakelock::Repository::of(&first.input.host)
        .filter(|_| !fetched)
        .map(|repo| repo.dir);
    let host_dir = first.input.host.dir.clone();
    let system_root = first.gate.system_root;
    let (line, git, root) = (
        first.source_line.clone(),
        first.input.git.clone(),
        first.gate.root.clone(),
    );
    // The home state names the URL, as the host's record does, and never the cached tree.
    let url = git.as_ref().map(|git| git.url.clone());
    let host = super::apply_loaded(options, None, first)?;
    if let Some(git) = &git {
        super::remote::prune(&root, git);
    }
    let mut out = match line {
        Some(line) => format!("{line}\n{host}"),
        None => host,
    };
    let fetcher = crate::fetch::HttpFetcher::from_env()
        .map_err(|error| Diagnostic::new("E_CONFIG", error))?;
    let now = crate::util::now_utc();
    let (refused, unsettled) = match &repo {
        Some(root) => lock_homes(root, &host_dir, &mut declared, &fetcher, now, &mut out),
        None => (BTreeMap::new(), None),
    };
    // SAFETY: geteuid cannot fail and takes no arguments.
    let linger_by_root = unsafe { libc::geteuid() } == 0;
    let mut failures = homes(declared, "apply", &mut out, system_root, |home| {
        if let Some((code, text)) = refused.get(&home.name) {
            return Child::Failed {
                code,
                text: text.clone(),
            };
        }
        let options = crate::home::apply::Options {
            overwrite_drift: false,
            // A fetched tree is never written: its tools lock is read, never resolved (F-15).
            locked: fetched,
            services: crate::home::services::Context {
                fixed_path: system_root,
                linger_by_root,
                linger_enabled_by_root: home.linger_enabled,
            },
        };
        let result = crate::home::apply::apply_preloaded_to(
            &home.roots,
            &home.manifest,
            home.lock.clone(),
            if fetched { None } else { home.target.as_ref() },
            &url.clone()
                .unwrap_or_else(|| home.roots.config().display().to_string()),
            &options,
            &fetcher,
            now,
        );
        match result {
            Ok(applied) => {
                let mut lines = String::new();
                for line in applied.warnings.iter().chain(&applied.lines) {
                    let _ = writeln!(lines, "{line}");
                }
                if applied.linger_off {
                    Child::LingerOff(lines)
                } else {
                    Child::Done(lines)
                }
            }
            Err(failure) => Child::Failed {
                code: failure.code,
                text: failure.text,
            },
        }
    });
    failures.extend(unsettled);
    finish(out, failures)
}

/// `lodi plan`: what [`apply`] would do, the homes planned as their users; nothing is written.
pub fn plan(options: &Options) -> Result<String, HostError> {
    let loaded = super::load(options, Operation::Plan)?;
    let declared = declared(&loaded, options)?;
    let mut out = String::new();
    if let Some(line) = &loaded.source_line {
        out.push_str(line);
        out.push('\n');
    }
    if let Some(record) = super::journal::outstanding(&loaded.gate)? {
        let classification = record.classify_in(&loaded.gate);
        let _ = writeln!(
            out,
            "journal {} is outstanding: {}",
            record.id,
            super::describe(&classification)
        );
    }
    out.push_str(&loaded.plan.render());
    let context = crate::home::services::Context {
        fixed_path: loaded.gate.system_root,
        ..Default::default()
    };
    let failures = homes(
        declared,
        "plan",
        &mut out,
        loaded.gate.system_root,
        |home| match crate::home::plan::plan_for(&home.roots, &home.manifest, false, &context) {
            Ok(plan) => {
                let mut lines = String::new();
                for line in plan.lines() {
                    let _ = writeln!(lines, "{line}");
                }
                Child::Done(lines)
            }
            Err(d) => failed(&d),
        },
    );
    finish(out, failures)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_report_survives_the_pipe() {
        for child in [
            Child::Done("a\nb\n".into()),
            Child::Failed {
                code: "E_DRIFT",
                text: "lodi: error E_DRIFT: x".into(),
            },
        ] {
            let back = decode(&encode(&child)).unwrap();
            match (child, back) {
                (Child::Done(a), Child::Done(b)) => assert_eq!(a, b),
                (Child::Failed { code, text }, Child::Failed { code: c, text: t }) => {
                    assert_eq!((code, text), (c, t))
                }
                _ => panic!("the kind changed"),
            }
        }
        assert!(decode(&[1, 2, 3]).is_none());
        assert_eq!(code_of("E_NOT_A_CODE"), "E_APPLY");
    }
}
