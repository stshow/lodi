//! The elevator helper (#705, LD-503): run one typed [`Request`] as root for a lodi that started
//! as the user (LD-491, #682).
//!
//! Already root (euid 0), the request runs in this process. Otherwise lodi's own binary runs
//! again by absolute path through the first of `sudo`, `doas` and `run0` on `PATH` ([`choose`]),
//! with the hidden entry [`ENTRY`]. Elevators clear the environment, so everything the child
//! needs is on its command line (a request's own data after `--with`, #709); the child reads no `HOME`, `XDG_*` or `LODI_*` variable of its own environment (the person's fetch settings travel in the switch request, LD-515). The
//! child reports its progress as relay lines on its standard output (`crate::progress::Relay`),
//! which the parent hands to its own sink, and its last line is its result. A refused or
//! cancelled password is the elevator's failure before any line: `E_NEED_ROOT`, and nothing ran.
//!
//! The hidden entry is in no help text, and it refuses (usage error, nothing done) unless it runs
//! as euid 0, or under a scratch `--root` that the invoking user stands for root of (LD-114). It
//! carries no token: whoever may run it as root is root already. Whether to elevate at all is the
//! caller's decision.

use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use serde_json::{Value, json};

use crate::config::User;
use crate::diag::{self, Diagnostic};
use crate::progress::{self, Event, Relay, Sink};

/// The hidden entry: `lodi __elevated ...`. Not a command; never listed.
pub const ENTRY: &str = "__elevated";

/// The elevators lodi knows, in the order it prefers them.
const ELEVATORS: [&str; 3] = ["sudo", "doas", "run0"];

/// What runs as root. Callers add their own (import's capture, switch's host part).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    /// Write the "may manage" marker (`crate::marker`): `{"created": bool}`.
    WriteMarker,
    /// `lodi import`'s host capture, then the marker when `mark` (#707), then, with `record`
    /// (the host file's folder, after `--with`), the host record (LD-515): the capture
    /// (`crate::config::import`).
    Import { mark: bool, record: Option<PathBuf> },
    /// `lodi switch`'s host part (#709): the switch request, as JSON text, on the child's command
    /// line after `--with` (`crate::config::switch::elevated`).
    Switch(String),
}

impl Request {
    fn name(&self) -> &'static str {
        match self {
            Request::WriteMarker => "write-marker",
            Request::Import { mark: false, .. } => "import",
            Request::Import { mark: true, .. } => "import-and-mark",
            Request::Switch(_) => "switch",
        }
    }

    /// What the request carries besides its name.
    fn with(&self) -> Option<OsString> {
        match self {
            Request::Switch(with) => Some(with.into()),
            Request::Import {
                record: Some(folder),
                ..
            } => Some(folder.into()),
            _ => None,
        }
    }

    fn named(name: &OsStr, with: Option<OsString>) -> Option<Request> {
        let import = |mark: bool, with: Option<OsString>| {
            let record = match with.map(PathBuf::from) {
                Some(folder) if !folder.is_absolute() => return None,
                folder => folder,
            };
            Some(Request::Import { mark, record })
        };
        match (name.to_str()?, with) {
            ("write-marker", None) => Some(Request::WriteMarker),
            ("import", with) => import(false, with),
            ("import-and-mark", with) => import(true, with),
            ("switch", Some(with)) => Some(Request::Switch(with.into_string().ok()?)),
            _ => None,
        }
    }
}

/// What a request runs with, in either process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Context {
    /// The root acted on: `--root`, or `/`.
    pub root: PathBuf,
    /// The found config root, when the caller has one.
    pub config: Option<PathBuf>,
    /// The uid lodi acts for (`crate::config::User::uid`).
    pub user: u32,
    /// The process's effective uid.
    pub euid: u32,
    pub verbose: bool,
}

/// Run `request` in this process, reporting into `sink`.
fn perform(request: &Request, cx: &Context, sink: &mut dyn Sink) -> Result<Value, Diagnostic> {
    match request {
        Request::WriteMarker => {
            sink.event(Event::Begin {
                name: "Write the marker".into(),
                expected: None,
            });
            let created = crate::marker::write(&cx.root, cx.euid)?;
            sink.event(Event::Finish {
                count: None,
                detail: None,
            });
            Ok(json!({ "created": created }))
        }
        Request::Import { mark, record } => {
            crate::config::import::elevated(*mark, record.as_deref(), cx, sink)
        }
        Request::Switch(with) => crate::config::switch::elevated(with, cx, sink),
    }
}

/// The first of `sudo`, `doas` and `run0` that `path` (a `PATH` value) holds as an executable
/// file; none is `E_NEED_ROOT`.
pub fn choose(path: Option<&OsStr>) -> Result<PathBuf, Diagnostic> {
    let dirs: Vec<PathBuf> = path
        .map(|path| std::env::split_paths(path).collect())
        .unwrap_or_default();
    ELEVATORS
        .iter()
        .find_map(|name| {
            dirs.iter()
                .filter(|dir| dir.is_absolute())
                .map(|dir| dir.join(name))
                .find(|at| {
                    std::fs::metadata(at)
                        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
                })
        })
        .ok_or_else(|| {
            Diagnostic::new(
                "E_NEED_ROOT",
                "this part needs root, and none of sudo, doas or run0 is on PATH",
            )
            .hint("run lodi as root, or use --home to change only your home")
        })
}

/// One elevation: who runs, for whom, and what the child is told.
#[derive(Debug, Clone)]
pub struct Elevation {
    /// lodi's own binary, absolute (`std::env::current_exe`).
    pub exe: PathBuf,
    /// `PATH`, searched for the elevator.
    pub path: Option<OsString>,
    /// This process's effective uid.
    pub euid: u32,
    /// The uid lodi acts for, `SUDO_UID` or `DOAS_USER`'s under either.
    pub user: u32,
    /// `--root`, when given.
    pub root: Option<PathBuf>,
    /// The found config root.
    pub config: Option<PathBuf>,
    /// `-v`.
    pub verbose: bool,
}

impl Elevation {
    pub fn new(exe: &Path, path: Option<&OsStr>, user: &User, euid: u32) -> Elevation {
        Elevation {
            exe: exe.to_path_buf(),
            path: path.map(OsStr::to_os_string),
            euid,
            user: user.uid,
            root: None,
            config: None,
            verbose: false,
        }
    }

    fn context(&self) -> Context {
        Context {
            root: self.root.clone().unwrap_or_else(|| PathBuf::from("/")),
            config: self.config.clone(),
            user: self.user,
            euid: self.euid,
            verbose: self.verbose,
        }
    }

    /// Run `request` in this process as whoever it runs as, never through an elevator: what a
    /// preview reads (#702 story 79).
    pub fn here(&self, request: &Request, sink: &mut dyn Sink) -> Result<Value, Diagnostic> {
        perform(request, &self.context(), sink)
    }

    /// Run `request` as root, its progress into `sink`: its payload, or its diagnostic.
    pub fn run(&self, request: &Request, sink: &mut dyn Sink) -> Result<Value, Diagnostic> {
        if self.euid == 0 {
            return perform(request, &self.context(), sink);
        }
        let elevator = choose(self.path.as_deref())?;
        machine_s_own(&elevator)?;
        let mut args: Vec<OsString> = vec![
            ENTRY.into(),
            "--request".into(),
            request.name().into(),
            "--user".into(),
            self.user.to_string().into(),
        ];
        if let Some(with) = request.with() {
            args.extend(["--with".into(), with]);
        }
        if let Some(root) = &self.root {
            args.extend(["--root".into(), root.into()]);
        }
        if let Some(config) = &self.config {
            args.extend(["--config".into(), config.into()]);
        }
        if self.verbose {
            args.push("--verbose".into());
        }
        let refused = |why: String| {
            Diagnostic::new(
                "E_NEED_ROOT",
                format!("{} did not run lodi as root: {why}", elevator.display()),
            )
            .hint("nothing was changed; run it again and give the password, or run lodi as root")
        };
        let mut child = Command::new(&elevator)
            .arg("--")
            .arg(&self.exe)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .map_err(|e| refused(e.to_string()))?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let (send, lines) = std::sync::mpsc::channel();
        let reading = progress::read_lines(stdout, progress::Stream::Stdout, send);
        let mut result = None;
        let mut seen = false;
        progress::pump(&lines, sink, |(_, line), sink| {
            seen = true;
            match outcome(&line) {
                Some(outcome) => result = Some(outcome),
                None => progress::relayed(line, sink),
            }
        });
        let _ = reading.join();
        let status = child.wait().map_err(|e| refused(e.to_string()))?;
        match result {
            Some(result) => result,
            None if !seen => Err(refused(format!("it ended with {status}"))),
            None => Err(Diagnostic::new(
                "E_APPLY",
                format!("the part lodi ran as root ended with {status} and no result"),
            )),
        }
    }
}

/// Under `LODI_HOST_REQUIRE_ROOT=1` (lanes and the gate, LD-376) an elevator root owns is this
/// machine's own and would run lodi as its real root: refused before it runs (#715, LD-513).
fn machine_s_own(elevator: &Path) -> Result<(), Diagnostic> {
    use std::os::unix::fs::MetadataExt;
    let guarded = std::env::var_os(crate::hostscope::REQUIRE_ROOT_VAR).is_some_and(|v| v == "1");
    if !guarded || !std::fs::metadata(elevator).is_ok_and(|meta| meta.uid() == 0) {
        return Ok(());
    }
    Err(Diagnostic::new(
        "E_HOST_ROOT_REQUIRED",
        format!(
            "{}=1 is set, so lodi runs no elevator of this machine's own, and {} belongs to \
             root; nothing was changed",
            crate::hostscope::REQUIRE_ROOT_VAR,
            elevator.display()
        ),
    )
    .hint("where it is set, put only stub elevators on PATH"))
}

/// The child's last line: `{"result": {"ok": PAYLOAD}}` or `{"result": {"error": {...}}}`.
fn outcome(line: &str) -> Option<Result<Value, Diagnostic>> {
    let value: Value = serde_json::from_str(line).ok()?;
    let result = value.as_object()?.get("result")?.as_object()?;
    if let Some(ok) = result.get("ok") {
        return Some(Ok(ok.clone()));
    }
    let error = result.get("error")?;
    let code = error.get("code")?.as_str()?;
    let code = diag::CODES
        .iter()
        .map(|(known, _)| *known)
        .find(|known| *known == code)
        .unwrap_or("E_APPLY");
    let mut diagnostic = Diagnostic::new(code, error.get("message")?.as_str()?);
    diagnostic.notes = error
        .get("notes")?
        .as_array()?
        .iter()
        .filter_map(|note| note.as_str().map(str::to_string))
        .collect();
    Some(Err(diagnostic))
}

fn encode(result: &Result<Value, Diagnostic>) -> Value {
    match result {
        Ok(payload) => json!({"result": {"ok": payload}}),
        Err(d) => json!({"result": {"error": {
            "code": d.code, "message": d.message, "notes": d.notes,
        }}}),
    }
}

/// The child's command line, parsed strictly: each flag once, nothing unknown.
struct Child {
    request: Request,
    user: u32,
    root: Option<PathBuf>,
    config: Option<PathBuf>,
    verbose: bool,
}

fn parse(args: &[OsString]) -> Option<Child> {
    let (mut request, mut user, mut root, mut config) = (None, None, None, None);
    let mut with = None;
    let mut verbose = false;
    let mut at = args.iter();
    while let Some(flag) = at.next() {
        let slot = match flag.to_str()? {
            "--verbose" if !verbose => {
                verbose = true;
                continue;
            }
            "--request" if request.is_none() => &mut request,
            "--user" if user.is_none() => &mut user,
            "--root" if root.is_none() => &mut root,
            "--config" if config.is_none() => &mut config,
            "--with" if with.is_none() => &mut with,
            _ => return None,
        };
        *slot = Some(at.next()?.clone());
    }
    let absolute = |path: OsString| Some(PathBuf::from(path)).filter(|p| p.is_absolute());
    Some(Child {
        request: Request::named(&request?, with)?,
        user: user?.to_str()?.parse().ok()?,
        root: match root {
            Some(root) => Some(absolute(root)?),
            None => None,
        },
        config: match config {
            Some(config) => Some(absolute(config)?),
            None => None,
        },
        verbose,
    })
}

/// Whether this process may act as root on `root`: it is root, or `root` is a scratch root
/// (anything but `/`), whose owner stands for root (LD-114).
fn may_act(euid: u32, root: Option<&Path>) -> bool {
    euid == 0
        || root
            .and_then(|root| std::fs::canonicalize(root).ok())
            .is_some_and(|root| root != Path::new("/"))
}

/// `lodi __elevated ...`: the child's side. `args` follow the entry's name.
pub fn entry(args: &[OsString]) -> ExitCode {
    // SAFETY: geteuid cannot fail and takes no arguments.
    let euid = unsafe { libc::geteuid() };
    let child = parse(args).filter(|child| may_act(euid, child.root.as_deref()));
    let Some(child) = child else {
        eprintln!("lodi: {ENTRY} is lodi's own internal entry, not a command; nothing was done");
        return ExitCode::from(diag::EXIT_USAGE);
    };
    let cx = Context {
        root: child.root.unwrap_or_else(|| PathBuf::from("/")),
        config: child.config,
        user: child.user,
        euid,
        verbose: child.verbose,
    };
    let mut relay = Relay::new(std::io::stdout());
    let result = perform(&child.request, &cx, &mut relay);
    let mut line = encode(&result).to_string();
    line.push('\n');
    let mut out = std::io::stdout();
    let _ = out.write_all(line.as_bytes());
    let _ = out.flush();
    match result {
        Ok(_) => ExitCode::SUCCESS,
        Err(d) => ExitCode::from(diag::exit_status(d.code)),
    }
}
