//! The step list: what `switch`, `update` and `import` show while they work (#694, LD-494).
//!
//! A command describes its run as **steps** and reports [`Event`]s into a [`Sink`]; everything
//! about drawing, timing and logging lives here. [`Progress`] is the one sink that owns the
//! terminal: each finished step keeps one line (`✓ [2/6] Download packages  34 in 4.1s`), the
//! step in progress is one live line with a spinner, a count and the item being worked on, and
//! the run ends with one summary line. Without a terminal ([`Mode::Plain`]) it prints a plain
//! ASCII line when a step starts and when it ends, with no cursor movement. Every run that
//! starts writes a log of every step, command and output line ([`Logs`]).
//!
//! A child process (an elevated host part, or a home part dropped to the user) reports through
//! a [`Relay`]: the same events as JSON lines on an inherited pipe, which the parent hands to its
//! own `Progress` with [`receive`]. A child's events carry no step numbers; the parent assigns
//! them.
//!
//! There is no timer thread: whoever waits on a process or a pipe receives its lines with a
//! [`TICK`] timeout and calls [`Sink::tick`] on each timeout ([`pump`]).

use std::collections::{BTreeSet, VecDeque};
use std::fs::File;
use std::io::{BufRead, BufReader, IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::diag::Diagnostic;

/// How often the spinner may advance, and how long a waiting loop blocks between redraws.
pub const TICK: Duration = Duration::from_millis(100);
/// How many logs are kept after each run.
pub const KEEP_LOGS: usize = 20;
/// How many of the failed step's last output lines are shown under it.
pub const TAIL_LINES: usize = 10;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// Carriage return and erase-line: the live line is redrawn in place.
const REDRAW: &str = "\r\x1b[2K";

/// Which of a process's two streams a line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

impl Stream {
    fn name(self) -> &'static str {
        match self {
            Stream::Stdout => "stdout",
            Stream::Stderr => "stderr",
        }
    }
}

/// One thing that happened during a run. Steps are begun in order; the number is the receiver's.
/// It crosses a pipe as JSON ([`Relay`]) and is never written to disk as one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// The next step starts, with the number of items expected when it is known.
    Begin {
        name: String,
        expected: Option<usize>,
    },
    /// One item reached; the count advances on each distinct name.
    Item { name: String },
    /// One line a process printed.
    Output { stream: Stream, line: String },
    /// One command line run, for the log.
    Command { line: String },
    /// The step ended; without a count, the number of distinct items seen.
    Finish {
        count: Option<usize>,
        detail: Option<String>,
    },
    /// The step ended the run with this diagnostic.
    Fail { message: String, notes: Vec<String> },
}

/// Where a run's events go.
pub trait Sink {
    fn event(&mut self, event: Event);
    /// Nothing arrived for a [`TICK`]: a chance to advance the spinner.
    fn tick(&mut self) {}
}

/// A sink for a caller with nothing to show.
pub struct Silent;

impl Sink for Silent {
    fn event(&mut self, _: Event) {}
}

/// Time since the run started.
pub trait Clock {
    fn elapsed(&self) -> Duration;
}

/// The real clock.
pub struct Wall(Instant);

impl Wall {
    pub fn new() -> Wall {
        Wall(Instant::now())
    }
}

impl Default for Wall {
    fn default() -> Wall {
        Wall::new()
    }
}

impl Clock for Wall {
    fn elapsed(&self) -> Duration {
        self.0.elapsed()
    }
}

/// How the step list is drawn, chosen once when the run starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Finished lines above one live line, redrawn in place and cut to `width` columns.
    Terminal { width: usize },
    /// One ASCII line when a step starts and one when it ends.
    Plain,
}

impl Mode {
    /// [`Mode::Terminal`] when standard error is a terminal and `TERM` is not `dumb`.
    pub fn detect() -> Mode {
        let dumb = std::env::var_os("TERM").is_some_and(|term| term == "dumb");
        if dumb || !std::io::stderr().is_terminal() {
            return Mode::Plain;
        }
        Mode::Terminal {
            width: terminal_width().unwrap_or(80),
        }
    }
}

fn terminal_width() -> Option<usize> {
    // SAFETY: `winsize` is plain data, and TIOCGWINSZ writes exactly one into the pointer given.
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let done = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, &mut size) };
    (done == 0 && size.ws_col > 0).then_some(usize::from(size.ws_col))
}

/// Where a run's log goes: `<dir>/<UTC time>-<command>.log`, owned by `owner` when the run acts
/// for another user (`sudo`).
pub struct Logs {
    pub dir: PathBuf,
    pub command: String,
    /// Seconds since the epoch, for the log's name.
    pub at: i64,
    pub owner: Option<(u32, u32)>,
}

struct Log {
    file: File,
    path: PathBuf,
}

impl Log {
    fn create(logs: &Logs) -> std::io::Result<Log> {
        use std::os::unix::fs::DirBuilderExt;
        let mut missing: Vec<&Path> = logs
            .dir
            .ancestors()
            .take_while(|dir| std::fs::symlink_metadata(dir).is_err())
            .collect();
        missing.reverse();
        for dir in missing {
            std::fs::DirBuilder::new().mode(0o700).create(dir)?;
            chown(dir, logs.owner)?;
        }
        let stamp = crate::util::format_utc(logs.at).replace(':', "-");
        let path = logs.dir.join(format!("{stamp}-{}.log", logs.command));
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        chown(&path, logs.owner)?;
        prune(&logs.dir)?;
        Ok(Log { file, path })
    }
}

fn chown(path: &Path, owner: Option<(u32, u32)>) -> std::io::Result<()> {
    if let Some((uid, gid)) = owner {
        std::os::unix::fs::chown(path, Some(uid), Some(gid))?;
    }
    Ok(())
}

/// Keep the [`KEEP_LOGS`] newest logs; the names sort by time.
fn prune(dir: &Path) -> std::io::Result<()> {
    let mut names: Vec<String> = std::fs::read_dir(dir)?
        .filter_map(|entry| entry.ok()?.file_name().into_string().ok())
        .filter(|name| name.ends_with(".log") && name.get(19..21) == Some("Z-"))
        .collect();
    names.sort();
    let surplus = names.len().saturating_sub(KEEP_LOGS);
    for name in &names[..surplus] {
        std::fs::remove_file(dir.join(name))?;
    }
    Ok(())
}

struct Live {
    number: usize,
    name: String,
    began: Duration,
    expected: Option<usize>,
    seen: BTreeSet<String>,
    last: Option<String>,
    tail: VecDeque<String>,
}

/// The sink that owns the terminal and writes the log.
pub struct Progress<W: Write> {
    out: W,
    clock: Box<dyn Clock>,
    mode: Mode,
    verbose: bool,
    total: usize,
    begun: usize,
    live: Option<Live>,
    /// The live line as last drawn, when one is on the screen.
    drawn: Option<String>,
    log: Option<Log>,
    interrupt: Option<libc::sighandler_t>,
}

impl<W: Write> Progress<W> {
    /// Start a run of `steps` (only the steps that have something to do), and its log.
    pub fn start(
        out: W,
        clock: Box<dyn Clock>,
        mode: Mode,
        verbose: bool,
        steps: &[&str],
        logs: Option<Logs>,
    ) -> Progress<W> {
        let mut progress = Progress {
            out,
            clock,
            mode,
            verbose,
            total: steps.len(),
            begun: 0,
            live: None,
            drawn: None,
            log: None,
            interrupt: None,
        };
        if let Some(logs) = logs {
            match Log::create(&logs) {
                Ok(log) => progress.log = Some(log),
                Err(error) => progress.print(&format!(
                    "lodi: warning: cannot write the log in {}: {error}\n",
                    logs.dir.display()
                )),
            }
        }
        if matches!(mode, Mode::Terminal { .. }) {
            progress.interrupt = Some(end_line_on_interrupt());
        }
        progress
    }

    /// The step in progress failed with `diagnostic`, which ends the run.
    pub fn fail(&mut self, diagnostic: &Diagnostic) {
        self.event(Event::Fail {
            message: format!("{}: {}", diagnostic.code, diagnostic.message),
            notes: diagnostic.notes.clone(),
        });
    }

    /// The closing line: `✓ <verb> in <total time>: <detail>`.
    pub fn summary(&mut self, verb: &str, detail: &str) {
        let mut line = format!("{verb} in {}", seconds(self.clock.elapsed()));
        if !detail.is_empty() {
            line.push_str(": ");
            line.push_str(detail);
        }
        self.write_log(&line);
        let shown = match self.mode {
            Mode::Terminal { .. } => format!("✓ {line}\n"),
            Mode::Plain => format!("{line}\n"),
        };
        self.print(&shown);
    }

    fn label(&self, live: &Live) -> String {
        format!(
            "[{}/{}] {}",
            live.number,
            self.total.max(live.number),
            live.name
        )
    }

    fn write_log(&mut self, line: &str) {
        let Some(log) = &mut self.log else {
            return;
        };
        let at = seconds(self.clock.elapsed());
        if let Err(error) = writeln!(log.file, "{at} {line}") {
            let warning = format!(
                "lodi: warning: cannot write the log {}: {error}\n",
                log.path.display()
            );
            self.log = None;
            self.print(&warning);
        }
    }

    /// Print finished text above the live line, which is drawn again after it.
    fn print(&mut self, text: &str) {
        let mut bytes = String::new();
        if self.drawn.take().is_some() {
            bytes.push_str(REDRAW);
        }
        bytes.push_str(text);
        let _ = self.out.write_all(bytes.as_bytes());
        self.draw();
    }

    /// Draw the live line when it differs from what the screen shows.
    fn draw(&mut self) {
        let Mode::Terminal { width } = self.mode else {
            return;
        };
        let Some(live) = &self.live else {
            return;
        };
        let since = self.clock.elapsed().saturating_sub(live.began);
        let frame = SPINNER[(since.as_millis() / TICK.as_millis()) as usize % SPINNER.len()];
        let mut line = format!("{frame} {}  ", self.label(live));
        if live.seen.is_empty() {
            line.push_str(&seconds(since));
        } else {
            line.push_str(&live.seen.len().to_string());
            // The plan's total counts what it names, not what the package manager adds.
            if let Some(expected) = live.expected.filter(|&total| total >= live.seen.len()) {
                line.push_str(&format!("/{expected}"));
            }
            if let Some(last) = &live.last {
                line.push_str("  ");
                line.push_str(last);
            }
        }
        let line: String = line.chars().take(width.saturating_sub(1)).collect();
        if self.drawn.as_ref() == Some(&line) {
            return;
        }
        let _ = self.out.write_all(format!("{REDRAW}{line}").as_bytes());
        let _ = self.out.flush();
        self.drawn = Some(line);
    }

    fn end(&mut self, ending: Result<String, (String, Vec<String>)>) {
        let Some(live) = self.live.take() else {
            if let Err((message, notes)) = ending {
                self.write_log(&format!("error {message}"));
                self.failure_lines(&VecDeque::new(), &notes);
            }
            return;
        };
        let label = self.label(&live);
        let took = seconds(self.clock.elapsed().saturating_sub(live.began));
        match ending {
            Ok(done) => {
                self.write_log(&format!("end {label}: done, {done}"));
                match self.mode {
                    Mode::Terminal { .. } => self.print(&format!("✓ {label}  {done}\n")),
                    Mode::Plain => self.print(&format!("{label}: done, {done}\n")),
                }
            }
            Err((message, notes)) => {
                self.write_log(&format!("end {label}: failed"));
                self.write_log(&format!("error {message}"));
                match self.mode {
                    Mode::Terminal { .. } => self.print(&format!("✗ {label}  {took}\n")),
                    Mode::Plain => self.print(&format!("{label}: failed\n")),
                }
                self.failure_lines(&live.tail, &notes);
            }
        }
    }

    fn failure_lines(&mut self, tail: &VecDeque<String>, notes: &[String]) {
        let mut text = String::new();
        for line in tail {
            text.push_str(&format!("    {line}\n"));
        }
        for note in notes {
            self.write_log(&format!("note {note}"));
            text.push_str(&format!("  {note}\n"));
        }
        if let Some(log) = &self.log {
            text.push_str(&format!("full log: {}\n", log.path.display()));
        }
        self.print(&text);
    }
}

impl<W: Write> Sink for Progress<W> {
    fn event(&mut self, event: Event) {
        match event {
            Event::Begin { name, expected } => {
                if self.live.is_some() {
                    self.end(Ok(String::new()));
                }
                self.begun += 1;
                let live = Live {
                    number: self.begun,
                    name,
                    began: self.clock.elapsed(),
                    expected,
                    seen: BTreeSet::new(),
                    last: None,
                    tail: VecDeque::new(),
                };
                let label = self.label(&live);
                self.live = Some(live);
                self.write_log(&format!("begin {label}"));
                if self.mode == Mode::Plain {
                    self.print(&format!("{label}\n"));
                }
                self.draw();
            }
            Event::Item { name } => {
                if let Some(live) = &mut self.live {
                    live.seen.insert(name.clone());
                    live.last = Some(name);
                }
                self.draw();
            }
            Event::Output { stream, line } => {
                self.write_log(&format!("{} {line}", stream.name()));
                if let Some(live) = &mut self.live {
                    if live.tail.len() == TAIL_LINES {
                        live.tail.pop_front();
                    }
                    live.tail.push_back(line.clone());
                }
                if self.verbose {
                    self.print(&format!("    {line}\n"));
                }
            }
            Event::Command { line } => self.write_log(&format!("run {line}")),
            Event::Finish { count, detail } => {
                let Some(live) = &self.live else {
                    return;
                };
                let took = seconds(self.clock.elapsed().saturating_sub(live.began));
                let count = count.or((!live.seen.is_empty()).then_some(live.seen.len()));
                let mut done = match count {
                    Some(count) => format!("{count} in {took}"),
                    None => took,
                };
                if let Some(detail) = detail {
                    done.push_str(&format!(" ({detail})"));
                }
                self.end(Ok(done));
            }
            Event::Fail { message, notes } => self.end(Err((message, notes))),
        }
    }

    fn tick(&mut self) {
        self.draw();
    }
}

impl<W: Write> Drop for Progress<W> {
    fn drop(&mut self) {
        if self.drawn.take().is_some() {
            let _ = self.out.write_all(b"\n");
            let _ = self.out.flush();
        }
        if let Some(previous) = self.interrupt.take() {
            // SAFETY: puts back the disposition `end_line_on_interrupt` replaced.
            unsafe { libc::signal(libc::SIGINT, previous) };
        }
    }
}

extern "C" fn interrupted(signal: libc::c_int) {
    // SAFETY: write, signal and raise are async-signal-safe; the handler ends the live line, then
    // lets the default action end the process exactly as it would have.
    unsafe {
        libc::write(libc::STDERR_FILENO, b"\n".as_ptr().cast(), 1);
        libc::signal(signal, libc::SIG_DFL);
        libc::raise(signal);
    }
}

/// On Ctrl-C the prompt comes back on a clean line: the live line is ended first.
fn end_line_on_interrupt() -> libc::sighandler_t {
    let handler = interrupted as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: installs a handler that only calls async-signal-safe functions.
    let previous = unsafe { libc::signal(libc::SIGINT, handler) };
    if previous != libc::SIG_DFL && previous != handler {
        // Someone else handles SIGINT; leave theirs in place.
        unsafe { libc::signal(libc::SIGINT, previous) };
    }
    previous
}

/// `2.3s` under ten seconds, `19s` under a minute, `1m05s` after.
fn seconds(duration: Duration) -> String {
    let millis = duration.as_millis();
    if millis < 10_000 {
        format!("{}.{}s", millis / 1000, millis % 1000 / 100)
    } else if millis < 60_000 {
        format!("{}s", millis / 1000)
    } else {
        format!("{}m{:02}s", millis / 60_000, millis % 60_000 / 1000)
    }
}

/// A child's sink: each event as one JSON line on the pipe its parent reads.
pub struct Relay<W: Write>(W);

impl<W: Write> Relay<W> {
    pub fn new(out: W) -> Relay<W> {
        Relay(out)
    }
}

impl<W: Write> Sink for Relay<W> {
    fn event(&mut self, event: Event) {
        let mut line = encode(&event).to_string();
        line.push('\n');
        let _ = self.0.write_all(line.as_bytes());
        let _ = self.0.flush();
    }
}

/// An event as the relay writes it: one object, named by its `event` field.
fn encode(event: &Event) -> Value {
    match event {
        Event::Begin { name, expected } => {
            json!({"event": "begin", "name": name, "expected": expected})
        }
        Event::Item { name } => json!({"event": "item", "name": name}),
        Event::Output { stream, line } => {
            json!({"event": "output", "stream": stream.name(), "line": line})
        }
        Event::Command { line } => json!({"event": "command", "line": line}),
        Event::Finish { count, detail } => {
            json!({"event": "finish", "count": count, "detail": detail})
        }
        Event::Fail { message, notes } => {
            json!({"event": "fail", "message": message, "notes": notes})
        }
    }
}

/// [`encode`]'s inverse; anything else is not an event.
fn decode(value: &Value) -> Option<Event> {
    let object: &Map<String, Value> = value.as_object()?;
    let text = |key: &str| object.get(key)?.as_str().map(str::to_string);
    let number = |key: &str| match object.get(key)? {
        Value::Null => Some(None),
        value => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .map(Some),
    };
    Some(match object.get("event")?.as_str()? {
        "begin" => Event::Begin {
            name: text("name")?,
            expected: number("expected")?,
        },
        "item" => Event::Item {
            name: text("name")?,
        },
        "output" => Event::Output {
            stream: match object.get("stream")?.as_str()? {
                "stdout" => Stream::Stdout,
                "stderr" => Stream::Stderr,
                _ => return None,
            },
            line: text("line")?,
        },
        "command" => Event::Command {
            line: text("line")?,
        },
        "finish" => Event::Finish {
            count: number("count")?,
            detail: match object.get("detail")? {
                Value::Null => None,
                value => Some(value.as_str()?.to_string()),
            },
        },
        "fail" => Event::Fail {
            message: text("message")?,
            notes: object
                .get("notes")?
                .as_array()?
                .iter()
                .map(|note| note.as_str().map(str::to_string))
                .collect::<Option<Vec<String>>>()?,
        },
        _ => return None,
    })
}

/// The parent's side of a [`Relay`]: render a child's events until its pipe closes. A line that
/// is not an event is shown as output.
pub fn receive<R: Read + Send + 'static>(reader: R, sink: &mut dyn Sink) {
    let (send, lines) = std::sync::mpsc::channel();
    let reading = read_lines(reader, Stream::Stdout, send);
    pump(&lines, sink, |(_, line), sink| relayed(line, sink));
    let _ = reading.join();
}

/// One line of a [`Relay`] handed to `sink`: its event, or the line as output when it is not one.
pub fn relayed(line: String, sink: &mut dyn Sink) {
    match serde_json::from_str::<Value>(&line)
        .ok()
        .as_ref()
        .and_then(decode)
    {
        Some(event) => sink.event(event),
        None => sink.event(Event::Output {
            stream: Stream::Stderr,
            line,
        }),
    }
}

/// Read `reader` line by line on a thread of its own and send each line, tagged with `stream`.
pub fn read_lines<R: Read + Send + 'static>(
    reader: R,
    stream: Stream,
    send: Sender<(Stream, String)>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let mut reader = BufReader::new(reader);
        let mut bytes = Vec::new();
        loop {
            bytes.clear();
            match reader.read_until(b'\n', &mut bytes) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&bytes);
                    let line = text.trim_end_matches(['\n', '\r']).to_string();
                    if send.send((stream, line)).is_err() {
                        break;
                    }
                }
            }
        }
    })
}

/// Hand every line to `each` until every sender is gone, ticking `sink` while none arrives.
pub fn pump(
    lines: &Receiver<(Stream, String)>,
    sink: &mut dyn Sink,
    mut each: impl FnMut((Stream, String), &mut dyn Sink),
) {
    loop {
        match lines.recv_timeout(TICK) {
            Ok(line) => each(line, sink),
            Err(RecvTimeoutError::Timeout) => sink.tick(),
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
}
