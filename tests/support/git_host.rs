//! Loopback replay of gu-1's recorded git http-backend host trees (`tests/fixtures/git/host/`,
//! recorded by `docs/milestones/m-flake/probes/record-git.py hosts`).
//!
//! The server answers the recorded advertisement to a GET, the ls-refs answer of its current
//! state to an ls-refs POST, and a fetch POST only when its body is byte for byte a recorded fetch
//! request. It counts every connection it accepts and keeps each request line, so a test can say
//! that nothing was asked, and what path was asked. Nothing leaves loopback.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};

/// The commits the recorded `single` repository holds: the first, then a pushed change.
pub const FIRST: &str = "1b5779390fc03e1b00bdb6a93a193e902025938b";
pub const SECOND: &str = "264d96e677ca6c5dc8857344dba09844affbe8aa";
/// The one commit of each other recorded repository.
pub const DIR: &str = "4adac374a4f4cb9d7aa3b324c1ab9d768bddfcfb";
pub const LINK: &str = "7d0a07ae39126b70a546744f171efc14309c1243";
pub const SUBMODULE: &str = "84ca28e7823963ac56c2ca5e0eccbce4a08c345a";
pub const PINS: &str = "f010333ed6a57cd2e83105da62a32a64bcd10e61";
pub const HOME: &str = "9255067028b20dc5e8a7ca6c9acc092f34816460";
pub const HOME_TOOLS: &str = "1ca52d2390d4ecadd7f69e741f7263cee1ab83f6";

struct Recorded {
    advertisement: Vec<u8>,
    refs: BTreeMap<String, Vec<u8>>,
    fetches: Vec<(Vec<u8>, Vec<u8>)>,
}

fn recorded(case: &str) -> Recorded {
    let dir = format!(
        "{}/tests/fixtures/git/host/{case}",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut refs = BTreeMap::new();
    let mut requests = BTreeMap::new();
    let mut responses = BTreeMap::new();
    for entry in std::fs::read_dir(&dir).expect("a recorded case") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        let bytes = std::fs::read(&path).unwrap();
        if let Some(state) = name
            .strip_prefix("ls-response-")
            .and_then(|n| n.strip_suffix(".bin"))
        {
            refs.insert(state.to_string(), bytes);
        } else if let Some(id) = name
            .strip_prefix("fetch-request-")
            .and_then(|n| n.strip_suffix(".bin"))
        {
            requests.insert(id.to_string(), bytes);
        } else if let Some(id) = name
            .strip_prefix("fetch-response-")
            .and_then(|n| n.strip_suffix(".bin"))
        {
            responses.insert(id.to_string(), bytes);
        }
    }
    let fetches = requests
        .into_iter()
        .map(|(id, request)| (request, responses.remove(&id).expect("an answer")))
        .collect();
    Recorded {
        advertisement: std::fs::read(format!("{dir}/advertisement.bin")).unwrap(),
        refs,
        fetches,
    }
}

pub struct Server {
    /// `http://127.0.0.1:PORT`.
    pub url: String,
    connections: Arc<AtomicUsize>,
    lines: Arc<Mutex<Vec<String>>>,
    state: Arc<Mutex<String>>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl Server {
    /// Serve the recorded `case` in its `state` (`first`, `second`, or `only`).
    pub fn start(case: &str, state: &str) -> Server {
        let data = Arc::new(recorded(case));
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let lines = Arc::new(Mutex::new(Vec::new()));
        let state = Arc::new(Mutex::new(state.to_string()));
        let (st, count, seen, now) = (
            stop.clone(),
            connections.clone(),
            lines.clone(),
            state.clone(),
        );
        let task = thread::spawn(move || {
            for incoming in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                count.fetch_add(1, Ordering::SeqCst);
                let Ok(mut socket) = incoming else { continue };
                if let Some((line, body)) = read_request(&mut socket) {
                    seen.lock().unwrap().push(line.clone());
                    let current = now.lock().unwrap().clone();
                    answer(&mut socket, &data, &current, &line, &body);
                }
            }
        });
        Server {
            url,
            connections,
            lines,
            state,
            stop,
            task: Some(task),
        }
    }

    /// Serve the other recorded state from now on: a push to the repository.
    pub fn push(&self, state: &str) {
        *self.state.lock().unwrap() = state.to_string();
    }

    /// Every connection accepted so far.
    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }

    /// Every request line (`GET /path HTTP/1.1`) so far.
    pub fn lines(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }

    /// `LODI_FETCH_REWRITE` sending `prefix` to `path` below this server.
    pub fn rewrite(&self, prefix: &str, path: &str) -> String {
        format!("{prefix}={}{path}", self.url)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

/// A listener that accepts and counts connections and answers none: where a test points a
/// rewrite to prove nothing was asked.
pub struct Counter {
    pub url: String,
    connections: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}

impl Counter {
    pub fn start() -> Counter {
        Counter::answering(None)
    }

    /// A counter that reads each request and answers it with `status` and no body.
    pub fn answering(status: Option<u16>) -> Counter {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let connections = Arc::new(AtomicUsize::new(0));
        let (st, count) = (stop.clone(), connections.clone());
        let task = thread::spawn(move || {
            for incoming in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                count.fetch_add(1, Ordering::SeqCst);
                if let (Some(status), Ok(mut socket)) = (status, incoming)
                    && read_request(&mut socket).is_some()
                {
                    let _ = socket.write_all(
                        format!(
                            "HTTP/1.1 {status} Refused\r\nContent-Length: 0\r\n\
                             Connection: close\r\n\r\n"
                        )
                        .as_bytes(),
                    );
                }
            }
        });
        Counter {
            url,
            connections,
            stop,
            task: Some(task),
        }
    }

    pub fn connections(&self) -> usize {
        self.connections.load(Ordering::SeqCst)
    }
}

impl Drop for Counter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        if let Some(task) = self.task.take() {
            task.join().unwrap();
        }
    }
}

fn read_request(socket: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut bytes = Vec::new();
    let mut buf = [0; 8192];
    while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            return None;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&bytes[..end]).into_owned();
    let length: usize = head
        .to_ascii_lowercase()
        .lines()
        .find_map(|line| line.strip_prefix("content-length:").map(str::to_string))
        .map(|v| v.trim().parse().unwrap())
        .unwrap_or(0);
    while bytes.len() - end < length {
        let n = socket.read(&mut buf).ok()?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&buf[..n]);
    }
    let line = head.lines().next().unwrap_or("").to_string();
    Some((line, bytes[end..].to_vec()))
}

fn answer(socket: &mut TcpStream, data: &Recorded, state: &str, line: &str, body: &[u8]) {
    let found = if line.starts_with("GET ") {
        Some((
            "application/x-git-upload-pack-advertisement",
            &data.advertisement,
        ))
    } else if body.windows(15).any(|w| w == b"command=ls-refs") {
        data.refs
            .get(state)
            .map(|refs| ("application/x-git-upload-pack-result", refs))
    } else {
        data.fetches
            .iter()
            .find(|(request, _)| request == body)
            .map(|(_, pack)| ("application/x-git-upload-pack-result", pack))
    };
    let _ = match found {
        Some((kind, bytes)) => socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: {kind}\r\nContent-Length: {}\r\n\
                     Connection: close\r\n\r\n",
                    bytes.len()
                )
                .as_bytes(),
            )
            .and_then(|()| socket.write_all(bytes)),
        None => socket
            .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"),
    };
}
