//! The package-manager runner with progress (#694): a stub package manager prints lines with
//! pauses and exits with a chosen status; a recording sink sees what the step list would.

mod support;

use std::cell::RefCell;
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::rc::Rc;

use lodi::hostscope::pm::{self, Invocation};
use lodi::progress::{Event, Mode, Progress, Sink, Stream};

#[derive(Default)]
struct Recording {
    events: Vec<Event>,
    ticks: usize,
}

impl Sink for Recording {
    fn event(&mut self, event: Event) {
        self.events.push(event);
    }
    fn tick(&mut self) {
        self.ticks += 1;
    }
}

/// Held by every test that writes and runs a stub: a script written while another thread forks
/// stays open in that child until it execs, and running it then fails with ETXTBSY.
static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serial() -> std::sync::MutexGuard<'static, ()> {
    SERIAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// A stub package manager: a shell script with this body. The runner clears the environment,
/// so the script names the test's own `PATH` for `sleep` and `seq`.
fn stub(body: &str) -> Invocation {
    let dir = support::scratch("pm-progress");
    let path: PathBuf = dir.join("stub-pm");
    let search = std::env::var("PATH").unwrap();
    std::fs::write(&path, format!("#!/bin/sh\nPATH='{search}'\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    Invocation::resolved(
        "stub-pm",
        &path,
        &["install".to_string(), "tree".to_string()],
        pm::noninteractive_env(),
    )
}

fn output(stream: Stream, line: &str) -> Event {
    Event::Output {
        stream,
        line: line.to_string(),
    }
}

fn none(_: &str) -> Option<String> {
    None
}

#[test]
fn every_line_of_both_streams_reaches_the_sink_in_order() {
    let _serial = serial();
    let invocation =
        stub("echo first\nsleep 0.05\necho 'to stderr' >&2\nsleep 0.05\nprintf 'last, no newline'");
    let mut sink = Recording::default();
    pm::run_reporting(&invocation, &mut sink, &none).unwrap();
    assert_eq!(
        sink.events,
        [
            Event::Command {
                line: "stub-pm install tree".to_string()
            },
            output(Stream::Stdout, "first"),
            output(Stream::Stderr, "to stderr"),
            output(Stream::Stdout, "last, no newline"),
        ]
    );
}

#[test]
fn the_lines_a_family_recognises_become_items() {
    let _serial = serial();
    let invocation =
        stub("echo 'Unpacking tree (2.1.0-1) ...'\necho noise\necho 'Setting up tree'");
    let mut sink = Recording::default();
    let items = |line: &str| {
        line.strip_prefix("Unpacking ")
            .map(|rest| rest.split(' ').next().unwrap().to_string())
    };
    pm::run_reporting(&invocation, &mut sink, &items).unwrap();
    assert_eq!(
        sink.events[1..],
        [
            output(Stream::Stdout, "Unpacking tree (2.1.0-1) ..."),
            Event::Item {
                name: "tree".to_string()
            },
            output(Stream::Stdout, "noise"),
            output(Stream::Stdout, "Setting up tree"),
        ]
    );
}

#[test]
fn a_quiet_package_manager_still_ticks_the_spinner() {
    let _serial = serial();
    let invocation = stub("sleep 0.45");
    let mut sink = Recording::default();
    pm::run_reporting(&invocation, &mut sink, &none).unwrap();
    assert!(sink.ticks >= 3, "{} ticks", sink.ticks);
}

#[test]
fn a_failing_exit_keeps_its_diagnostic_and_shows_the_last_ten_lines() {
    let _serial = serial();
    let invocation = stub("for n in $(seq 1 12); do echo \"E: line $n\" >&2; done\nexit 100");
    let mut sink = Recording::default();
    let error = pm::run_reporting(&invocation, &mut sink, &none).unwrap_err();
    assert_eq!(error.code, "E_APPLY");
    assert_eq!(
        error.message,
        "`stub-pm install tree` exited 100: E: line 8; E: line 9; E: line 10; E: line 11; \
         E: line 12"
    );
    assert_eq!(sink.events.len(), 13);

    // The step list shows the last ten of them under the failed step.
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
    let screen = Screen::default();
    let mut progress = Progress::start(
        screen.clone(),
        Box::new(lodi::progress::Wall::new()),
        Mode::Plain,
        false,
        &["Install packages"],
        None,
    );
    progress.event(Event::Begin {
        name: "Install packages".to_string(),
        expected: None,
    });
    let error = pm::run_reporting(&invocation, &mut progress, &none).unwrap_err();
    progress.fail(&error);
    drop(progress);
    let text = String::from_utf8(screen.0.borrow().clone()).unwrap();
    let mut want = String::from("[1/1] Install packages\n[1/1] Install packages: failed\n");
    for n in 3..=12 {
        want.push_str(&format!("    E: line {n}\n"));
    }
    assert_eq!(text, want);
}

#[test]
fn a_failure_that_printed_nothing_on_stderr_quotes_stdout() {
    let _serial = serial();
    let invocation = stub("echo 'the reason'\nexit 1");
    let error = pm::run_reporting(&invocation, &mut Recording::default(), &none).unwrap_err();
    assert_eq!(error.message, "`stub-pm install tree` exited 1: the reason");
    let silent = stub("exit 2");
    let error = pm::run(&silent).unwrap_err();
    assert_eq!(
        error.message,
        "`stub-pm install tree` exited 2: it printed nothing"
    );
}

#[test]
fn a_program_that_cannot_start_is_e_apply() {
    let _serial = serial();
    let invocation = Invocation::resolved(
        "absent",
        std::path::Path::new("/nonexistent/absent"),
        &[],
        pm::noninteractive_env(),
    );
    let error = pm::run_reporting(&invocation, &mut Recording::default(), &none).unwrap_err();
    assert_eq!(error.code, "E_APPLY");
    assert!(
        error
            .message
            .starts_with("cannot run /nonexistent/absent: "),
        "{}",
        error.message
    );
}

/// A stub that replays a recording from `tests/fixtures/host/<family>/progress/`: its standard
/// output, a pause, its standard error, and its exit status.
fn replay(folder: &str, name: &str) -> Invocation {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/host")
        .join(folder)
        .join("progress");
    let file = |ext: &str| dir.join(format!("{name}.{ext}")).display().to_string();
    let status = std::fs::read_to_string(file("status")).unwrap();
    stub(&format!(
        "cat '{}'\nsleep 0.05\ncat '{}' >&2\nexit {}",
        file("stdout"),
        file("stderr"),
        status.trim()
    ))
}

/// The distinct items a recorded run reports through `items`, in the order first reached.
fn items_of(folder: &str, name: &str, items: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut sink = Recording::default();
    pm::run_reporting(&replay(folder, name), &mut sink, items).unwrap();
    let mut seen: Vec<String> = Vec::new();
    for event in sink.events {
        if let Event::Item { name } = event
            && !seen.contains(&name)
        {
            seen.push(name);
        }
    }
    seen
}

#[test]
fn apt_counts_its_downloads_unpacks_set_ups_and_removals() {
    let _serial = serial();
    let apt = "apt/debian-12";
    let items = &pm::apt::item;
    assert_eq!(items_of(apt, "refresh", items), Vec::<String>::new());
    assert_eq!(items_of(apt, "download", items), ["sqlite3", "tree"]);
    assert_eq!(items_of(apt, "install", items), ["sqlite3", "tree"]);
    assert_eq!(items_of(apt, "remove", items), ["tree"]);
}

#[test]
fn apt_a_failed_download_is_e_apply_with_apts_own_words() {
    let _serial = serial();
    let error = pm::run(&replay("apt/debian-12", "download-fail")).unwrap_err();
    assert_eq!(error.code, "E_APPLY");
    assert!(
        error
            .message
            .ends_with("E: Unable to locate package nosuchpackage-lodi"),
        "{}",
        error.message
    );
}

#[test]
fn dnf_counts_its_downloads_installs_and_removals() {
    let _serial = serial();
    let items = &pm::dnf::item;
    assert_eq!(items_of("dnf", "refresh", items), Vec::<String>::new());
    assert_eq!(items_of("dnf", "download", items), ["libutempter", "tmux"]);
    assert_eq!(items_of("dnf", "install", items), ["tmux", "libutempter"]);
    assert_eq!(items_of("dnf", "remove", items), ["tmux", "libutempter"]);
}

#[test]
fn pacman_counts_its_downloads_installs_and_removals() {
    let _serial = serial();
    let arch = "pacman/arch";
    let items = &pm::pacman::item;
    assert_eq!(items_of(arch, "refresh", items), Vec::<String>::new());
    assert_eq!(items_of(arch, "download", items), ["tree"]);
    assert_eq!(items_of(arch, "install", items), ["tree"]);
    assert_eq!(items_of(arch, "remove", items), ["tree"]);
}

fn invocation(program: &str, args: &[&str]) -> Invocation {
    let args: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
    Invocation::resolved(
        program,
        &PathBuf::from("/usr/bin").join(program),
        &args,
        pm::noninteractive_env(),
    )
}

/// Each family downloads what its transaction will install, with the transaction's own
/// options (a pinned one's private source set included), and installs nothing.
#[test]
fn each_family_downloads_its_transaction_without_installing_it() {
    let download = |family: &dyn Fn(&Invocation) -> Option<Invocation>, inv: Invocation| {
        family(&inv).map(|d| (d.program.clone(), d.args.join(" ")))
    };
    let apt = invocation(
        "apt-get",
        &[
            "install",
            "-y",
            "-o",
            "Dir::Etc::SourceList=x",
            "tree",
            "sqlite3",
            "zip-",
        ],
    );
    assert_eq!(
        download(&pm::apt::download, apt),
        Some((
            "apt-get".to_string(),
            "install --download-only -y -o Dir::Etc::SourceList=x tree sqlite3 zip-".to_string()
        ))
    );
    assert_eq!(
        download(&pm::apt::download, invocation("apt-get", &["update"])),
        None
    );
    let dnf = invocation(
        "dnf5",
        &["-y", "--setopt=install_weak_deps=False", "install", "tmux"],
    );
    assert_eq!(
        download(&pm::dnf::download, dnf).unwrap().1,
        "-y --setopt=install_weak_deps=False install --downloadonly tmux"
    );
    assert_eq!(
        download(
            &pm::dnf::download,
            invocation("dnf5", &["-y", "remove", "x"])
        ),
        None
    );
    let pacman = invocation("pacman", &["-S", "--needed", "--noconfirm", "tree"]);
    assert_eq!(
        download(&pm::pacman::download, pacman).unwrap().1,
        "-Sw --needed --noconfirm tree"
    );
    let upgrade = invocation("pacman", &["-Syu", "--noconfirm", "tree"]);
    assert_eq!(
        download(&pm::pacman::download, upgrade).unwrap().1,
        "-Syuw --noconfirm tree"
    );
    assert_eq!(
        download(
            &pm::pacman::download,
            invocation("pacman", &["-Rns", "--noconfirm", "x"])
        ),
        None
    );
}
