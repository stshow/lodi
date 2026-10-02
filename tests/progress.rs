//! The step list (#694), at its interface: events in, with a fake clock, a fixed width and a
//! chosen mode; the exact bytes out, and the log on disk.

mod support;

use std::cell::{Cell, RefCell};
use std::io::Write;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use lodi::diag::Diagnostic;
use lodi::progress::{Clock, Event, Logs, Mode, Progress, Relay, Sink, Stream};

#[derive(Clone, Default)]
struct Screen(Rc<RefCell<Vec<u8>>>);

impl Write for Screen {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.borrow_mut().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Screen {
    fn text(&self) -> String {
        String::from_utf8(self.0.borrow().clone()).unwrap()
    }
    fn clear(&self) {
        self.0.borrow_mut().clear();
    }
}

#[derive(Clone, Default)]
struct Fake(Rc<Cell<Duration>>);

impl Clock for Fake {
    fn elapsed(&self) -> Duration {
        self.0.get()
    }
}

impl Fake {
    fn at(&self, millis: u64) {
        self.0.set(Duration::from_millis(millis));
    }
}

fn start(mode: Mode, verbose: bool, steps: &[&str]) -> (Progress<Screen>, Screen, Fake) {
    let screen = Screen::default();
    let clock = Fake::default();
    let progress = Progress::start(
        screen.clone(),
        Box::new(clock.clone()),
        mode,
        verbose,
        steps,
        None,
    );
    (progress, screen, clock)
}

fn begin(name: &str, expected: Option<usize>) -> Event {
    Event::Begin {
        name: name.to_string(),
        expected,
    }
}

fn item(name: &str) -> Event {
    Event::Item {
        name: name.to_string(),
    }
}

fn finish(count: Option<usize>) -> Event {
    Event::Finish {
        count,
        detail: None,
    }
}

fn out(line: &str) -> Event {
    Event::Output {
        stream: Stream::Stdout,
        line: line.to_string(),
    }
}

const STEPS: &[&str] = &[
    "Refresh package index",
    "Download packages",
    "Install packages",
];

#[test]
fn plain_mode_prints_a_line_when_each_step_starts_and_ends() {
    let (mut progress, screen, clock) = start(Mode::Plain, false, STEPS);
    progress.event(begin("Refresh package index", None));
    clock.at(2300);
    progress.tick();
    progress.event(finish(None));
    progress.event(begin("Download packages", Some(2)));
    progress.event(item("tree"));
    progress.event(item("tree"));
    progress.event(item("sqlite3"));
    clock.at(6400);
    progress.event(finish(None));
    clock.at(19_000);
    progress.summary("switched", "+2 packages");
    drop(progress);
    assert_eq!(
        screen.text(),
        "[1/3] Refresh package index\n\
         [1/3] Refresh package index: done, 2.3s\n\
         [2/3] Download packages\n\
         [2/3] Download packages: done, 2 in 4.1s\n\
         switched in 19s: +2 packages\n"
    );
}

#[test]
fn plain_mode_is_ascii_and_has_no_cursor_movement() {
    let (mut progress, screen, clock) = start(Mode::Plain, false, STEPS);
    progress.event(begin("Install packages", Some(3)));
    for name in ["a", "b", "c"] {
        clock.at(clock.0.get().as_millis() as u64 + 150);
        progress.event(item(name));
        progress.tick();
    }
    progress.event(finish(Some(3)));
    progress.summary("switched", "");
    drop(progress);
    let text = screen.text();
    assert!(text.is_ascii(), "{text:?}");
    assert!(!text.contains('\r') && !text.contains('\x1b'), "{text:?}");
    assert_eq!(
        text,
        "[1/3] Install packages\n[1/3] Install packages: done, 3 in 0.4s\nswitched in 0.4s\n"
    );
}

#[test]
fn terminal_mode_keeps_finished_lines_above_one_live_line() {
    let (mut progress, screen, clock) = start(Mode::Terminal { width: 80 }, false, STEPS);
    progress.event(begin("Refresh package index", None));
    assert_eq!(
        screen.text(),
        "\r\x1b[2K⠋ [1/3] Refresh package index  0.0s"
    );
    screen.clear();
    clock.at(2300);
    progress.event(finish(None));
    assert_eq!(
        screen.text(),
        "\r\x1b[2K✓ [1/3] Refresh package index  2.3s\n"
    );
    screen.clear();
    progress.event(begin("Download packages", Some(34)));
    screen.clear();
    clock.at(2450);
    progress.event(item("neovim"));
    assert_eq!(
        screen.text(),
        "\r\x1b[2K⠙ [2/3] Download packages  1/34  neovim"
    );
    screen.clear();
    clock.at(6400);
    progress.event(finish(Some(34)));
    assert_eq!(
        screen.text(),
        "\r\x1b[2K✓ [2/3] Download packages  34 in 4.1s\n"
    );
    screen.clear();
    clock.at(19_000);
    progress.summary("switched", "+34 -2 packages, 1 service, 3 home files");
    drop(progress);
    assert_eq!(
        screen.text(),
        "✓ switched in 19s: +34 -2 packages, 1 service, 3 home files\n"
    );
}

#[test]
fn the_spinner_advances_on_a_tick_about_every_tenth_of_a_second() {
    let (mut progress, screen, clock) = start(Mode::Terminal { width: 80 }, false, STEPS);
    progress.event(begin("Install packages", None));
    screen.clear();
    clock.at(50);
    progress.tick();
    assert_eq!(screen.text(), "", "nothing changed, nothing is redrawn");
    clock.at(120);
    progress.tick();
    assert_eq!(screen.text(), "\r\x1b[2K⠙ [1/3] Install packages  0.1s");
    screen.clear();
    clock.at(230);
    progress.tick();
    assert_eq!(screen.text(), "\r\x1b[2K⠹ [1/3] Install packages  0.2s");
}

#[test]
fn a_count_past_the_planned_total_shows_without_the_total() {
    let (mut progress, screen, _clock) = start(Mode::Terminal { width: 80 }, false, STEPS);
    progress.event(begin("Install packages", Some(1)));
    progress.event(item("tree"));
    progress.event(item("libdep"));
    assert!(
        screen
            .text()
            .ends_with("\r\x1b[2K⠋ [1/3] Install packages  2  libdep"),
        "{:?}",
        screen.text()
    );
}

#[test]
fn the_live_line_is_cut_to_the_terminal_width() {
    let (mut progress, screen, _clock) = start(Mode::Terminal { width: 20 }, false, STEPS);
    progress.event(begin("Install packages", Some(34)));
    progress.event(item("a-package-with-a-long-name"));
    let text = screen.text();
    let last = text.rsplit("\r\x1b[2K").next().unwrap();
    assert_eq!(last, "⠋ [1/3] Install pac");
    assert_eq!(last.chars().count(), 19);
}

#[test]
fn the_live_line_is_ended_with_a_newline_when_the_run_stops() {
    let (mut progress, screen, _clock) = start(Mode::Terminal { width: 80 }, false, STEPS);
    progress.event(begin("Install packages", None));
    drop(progress);
    assert!(screen.text().ends_with("0.0s\n"), "{:?}", screen.text());
}

#[test]
fn verbose_mode_streams_output_above_the_live_line() {
    let (mut progress, screen, _clock) = start(Mode::Terminal { width: 80 }, true, STEPS);
    progress.event(begin("Install packages", None));
    screen.clear();
    progress.event(out("Unpacking tree (2.1.0-1) ..."));
    assert_eq!(
        screen.text(),
        "\r\x1b[2K    Unpacking tree (2.1.0-1) ...\n\
         \r\x1b[2K⠋ [1/3] Install packages  0.0s"
    );
    let (mut plain, screen, _clock) = start(Mode::Plain, true, STEPS);
    plain.event(begin("Install packages", None));
    plain.event(out("Unpacking tree (2.1.0-1) ..."));
    assert_eq!(
        screen.text(),
        "[1/3] Install packages\n    Unpacking tree (2.1.0-1) ...\n"
    );
}

#[test]
fn without_verbose_the_package_managers_output_is_not_shown() {
    let (mut progress, screen, _clock) = start(Mode::Plain, false, STEPS);
    progress.event(begin("Install packages", None));
    progress.event(out("Unpacking tree (2.1.0-1) ..."));
    assert_eq!(screen.text(), "[1/3] Install packages\n");
}

fn failing(progress: &mut Progress<Screen>, clock: &Fake) {
    progress.event(begin("Install packages", Some(2)));
    for n in 1..=12 {
        progress.event(out(&format!("line {n}")));
    }
    clock.at(1500);
    let mut diagnostic = Diagnostic::new("E_APPLY", "action 3 failed");
    diagnostic.notes.push("ran: 1 (refresh)".to_string());
    diagnostic
        .notes
        .push("did not run: 4 (services)".to_string());
    progress.fail(&diagnostic);
}

#[test]
fn a_failure_shows_a_cross_the_last_ten_lines_the_notes_and_the_log() {
    let state = support::scratch("progress-fail");
    let logs = state.join("logs");
    let screen = Screen::default();
    let clock = Fake::default();
    let mut progress = Progress::start(
        screen.clone(),
        Box::new(clock.clone()),
        Mode::Terminal { width: 80 },
        false,
        STEPS,
        Some(Logs {
            dir: logs.clone(),
            command: "switch".to_string(),
            at: 1_790_000_000,
            owner: None,
        }),
    );
    screen.clear();
    failing(&mut progress, &clock);
    let path = logs.join("2026-09-21T14-13-20Z-switch.log");
    let mut want = String::from("\r\x1b[2K⠋ [1/3] Install packages  0.0s");
    want.push_str("\r\x1b[2K✗ [1/3] Install packages  1.5s\n");
    for n in 3..=12 {
        want.push_str(&format!("    line {n}\n"));
    }
    want.push_str("  ran: 1 (refresh)\n  did not run: 4 (services)\n");
    want.push_str(&format!("full log: {}\n", path.display()));
    assert_eq!(screen.text(), want);
    drop(progress);
    assert_eq!(
        screen.text(),
        want,
        "a failed step leaves no live line to end"
    );

    let (mut plain, screen, clock) = start(Mode::Plain, false, STEPS);
    failing(&mut plain, &clock);
    let text = screen.text();
    assert!(
        text.starts_with("[1/3] Install packages\n[1/3] Install packages: failed\n    line 3\n"),
        "{text:?}"
    );
}

fn logs_in(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn every_run_writes_a_log_of_steps_commands_and_output() {
    use std::os::unix::fs::PermissionsExt;
    let state = support::scratch("progress-log");
    let logs = state.join("state/lodi/logs");
    let clock = Fake::default();
    let mut progress = Progress::start(
        Screen::default(),
        Box::new(clock.clone()),
        Mode::Plain,
        false,
        STEPS,
        Some(Logs {
            dir: logs.clone(),
            command: "switch".to_string(),
            at: 1_790_000_000,
            owner: None,
        }),
    );
    progress.event(begin("Refresh package index", None));
    progress.event(Event::Command {
        line: "apt-get update".to_string(),
    });
    progress.event(out("Hit:1 http://deb.debian.org/debian bookworm InRelease"));
    progress.event(Event::Output {
        stream: Stream::Stderr,
        line: "W: something".to_string(),
    });
    clock.at(2300);
    progress.event(finish(None));
    progress.summary("switched", "nothing to change");
    drop(progress);
    let mode = std::fs::metadata(&logs).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700);
    assert_eq!(logs_in(&logs), ["2026-09-21T14-13-20Z-switch.log"]);
    let text = std::fs::read_to_string(logs.join("2026-09-21T14-13-20Z-switch.log")).unwrap();
    assert_eq!(
        text,
        "0.0s begin [1/3] Refresh package index\n\
         0.0s run apt-get update\n\
         0.0s stdout Hit:1 http://deb.debian.org/debian bookworm InRelease\n\
         0.0s stderr W: something\n\
         2.3s end [1/3] Refresh package index: done, 2.3s\n\
         2.3s switched in 2.3s: nothing to change\n"
    );
}

#[test]
fn only_the_twenty_newest_logs_are_kept() {
    let state = support::scratch("progress-prune");
    let logs = state.join("logs");
    for at in 0..25 {
        let progress = Progress::start(
            Screen::default(),
            Box::new(Fake::default()),
            Mode::Plain,
            false,
            STEPS,
            Some(Logs {
                dir: logs.clone(),
                command: "update".to_string(),
                at: 1_790_000_000 + at * 60,
                owner: None,
            }),
        );
        drop(progress);
    }
    let names = logs_in(&logs);
    assert_eq!(names.len(), 20);
    assert_eq!(names[0], "2026-09-21T14-18-20Z-update.log");
    assert_eq!(names[19], "2026-09-21T14-37-20Z-update.log");
}

#[test]
fn a_log_that_cannot_be_written_is_a_warning_and_the_run_goes_on() {
    let state = support::scratch("progress-nolog");
    let blocker = state.join("file");
    std::fs::write(&blocker, "").unwrap();
    let screen = Screen::default();
    let mut progress = Progress::start(
        screen.clone(),
        Box::new(Fake::default()),
        Mode::Plain,
        false,
        STEPS,
        Some(Logs {
            dir: blocker.join("logs"),
            command: "switch".to_string(),
            at: 1_790_000_000,
            owner: None,
        }),
    );
    progress.event(begin("Refresh package index", None));
    let text = screen.text();
    assert!(
        text.starts_with("lodi: warning: cannot write the log in "),
        "{text:?}"
    );
    assert!(text.ends_with("[1/3] Refresh package index\n"), "{text:?}");
}

#[test]
fn a_childs_events_cross_a_pipe_and_the_parent_numbers_them() {
    let mut wire = Vec::new();
    {
        let mut child = Relay::new(&mut wire);
        child.event(begin("Home files", Some(1)));
        child.event(out("wrote .bashrc"));
        child.event(item(".bashrc"));
        child.event(finish(None));
    }
    let text = String::from_utf8(wire.clone()).unwrap();
    assert_eq!(text.lines().count(), 4, "one JSON line per event: {text}");
    assert!(
        !text.contains("[1/"),
        "a child's events carry no step numbers"
    );

    let steps = ["Install packages", "Home files"];
    let (mut parent, screen, _clock) = start(Mode::Plain, false, &steps);
    parent.event(begin("Install packages", None));
    parent.event(finish(Some(3)));
    lodi::progress::receive(std::io::Cursor::new(wire), &mut parent);
    assert_eq!(
        screen.text(),
        "[1/2] Install packages\n\
         [1/2] Install packages: done, 3 in 0.0s\n\
         [2/2] Home files\n\
         [2/2] Home files: done, 1 in 0.0s\n"
    );
}

/// The relay's wire format, one JSON object per line named by its `event` field, both ways; a
/// line that is not one reaches the parent as standard-error output.
#[test]
fn the_relay_speaks_one_json_object_per_line() {
    let mut wire = Vec::new();
    {
        let mut child = Relay::new(&mut wire);
        child.event(begin("Files", Some(2)));
        child.event(Event::Output {
            stream: Stream::Stderr,
            line: "warn".to_string(),
        });
        child.event(Event::Fail {
            message: "E_APPLY: no".to_string(),
            notes: vec!["hint".to_string()],
        });
    }
    let text = String::from_utf8(wire).unwrap();
    let lines: Vec<serde_json::Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(text.ends_with('\n'));
    assert_eq!(
        lines,
        [
            serde_json::json!({"event": "begin", "name": "Files", "expected": 2}),
            serde_json::json!({"event": "output", "stream": "stderr", "line": "warn"}),
            serde_json::json!({"event": "fail", "message": "E_APPLY: no", "notes": ["hint"]}),
        ]
    );

    #[derive(Default)]
    struct Recording(Vec<Event>);
    impl Sink for Recording {
        fn event(&mut self, event: Event) {
            self.0.push(event);
        }
    }
    let wire = "{\"event\":\"item\",\"name\":\"tree\"}\n\
                {\"event\":\"finish\",\"count\":null,\"detail\":\"ok\"}\n\
                {\"event\":\"command\",\"line\":\"apt-get update\"}\n\
                not an event\n";
    let mut parent = Recording::default();
    lodi::progress::receive(std::io::Cursor::new(wire.as_bytes().to_vec()), &mut parent);
    assert_eq!(
        parent.0,
        [
            item("tree"),
            Event::Finish {
                count: None,
                detail: Some("ok".to_string())
            },
            Event::Command {
                line: "apt-get update".to_string()
            },
            Event::Output {
                stream: Stream::Stderr,
                line: "not an event".to_string()
            },
        ]
    );
}
