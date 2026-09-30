//! A repository a test commits, served live by `scripts/git-http-loopback.py` (LD-455) on
//! loopback, for the host trees gu-1's recordings do not hold: a Fedora host folder (ff-1).
//!
//! The server is a [`fixture::Fixture`], ended with the test. Commits carry a fixed identity and
//! date and no configuration of the machine's, so a tree gives the same commit on every run.
//! Included by path; the including binary also declares `mod fixture` and `mod wait`.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::fixture::Fixture;

/// `git ARGS` in `dir`, which must succeed; its standard output, trimmed.
pub fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@users.noreply.github.com")
        .env("GIT_COMMITTER_NAME", "fixture")
        .env("GIT_COMMITTER_EMAIL", "fixture@users.noreply.github.com")
        .env("GIT_AUTHOR_DATE", "2001-01-01T00:00:00+0000")
        .env("GIT_COMMITTER_DATE", "2001-01-01T00:00:00+0000")
        .stdin(Stdio::null())
        .output()
        .expect("git runs");
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_string()
}

/// Everything below `work` committed on `main` of a new repository there; the commit.
pub fn commit_all(work: &Path, message: &str) -> String {
    if !work.join(".git").exists() {
        git(work, &["init", "-q", "-b", "main"]);
    }
    git(work, &["add", "-A"]);
    git(work, &["commit", "-qm", message]);
    git(work, &["rev-parse", "HEAD"])
}

/// The server, over bare repositories below its directory.
pub struct Served {
    process: Fixture,
    dir: PathBuf,
    port: u16,
}

impl Served {
    /// Serve `dir`, which holds bare repositories (`NAME.git`), and wait for the port.
    pub fn start(dir: &Path) -> Served {
        let port_file = dir.join("port");
        let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("scripts/git-http-loopback.py");
        let process = Fixture::spawn(
            "git-http-loopback",
            Command::new("python3")
                .arg("-B")
                .arg(script)
                .arg("serve")
                .arg("--root")
                .arg(dir)
                .arg("--port-file")
                .arg(&port_file)
                .arg("--log")
                .arg(dir.join("requests"))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null()),
        );
        let deadline = Instant::now() + crate::wait::ceiling();
        let port = loop {
            let read = fs::read_to_string(&port_file).ok();
            if let Some(port) = read.and_then(|text| text.trim().parse().ok()) {
                break port;
            }
            assert!(Instant::now() < deadline, "the git server never started");
            std::thread::sleep(Duration::from_millis(20));
        };
        Served {
            process,
            dir: dir.to_path_buf(),
            port,
        }
    }

    /// Clone `work` bare as `NAME.git` below the served directory: a push of everything it holds.
    pub fn push(&self, work: &Path, name: &str) {
        let bare = format!("{name}.git");
        let _ = fs::remove_dir_all(self.dir.join(&bare));
        git(
            &self.dir,
            &["clone", "-q", "--bare", &work.display().to_string(), &bare],
        );
    }

    /// `LODI_FETCH_REWRITE` sending `prefix` to `NAME.git` on this server.
    pub fn rewrite(&self, prefix: &str, name: &str) -> String {
        format!("{prefix}=http://127.0.0.1:{}/{name}.git", self.port)
    }

    /// How many requests the server answered.
    pub fn requests(&self) -> usize {
        let _ = self.process.id();
        fs::read_to_string(self.dir.join("requests"))
            .unwrap_or_default()
            .lines()
            .count()
    }
}
