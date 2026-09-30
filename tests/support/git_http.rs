//! Loopback replay of recorded git http-backend GET and POST exchanges.
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::thread::{self, JoinHandle};

pub struct Server {
    pub url: String,
    pub count: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<()>>,
}
impl Server {
    pub fn recorded(case: &str) -> Self {
        let prefix = format!("{}/tests/fixtures/git/{case}", env!("CARGO_MANIFEST_DIR"));
        let advert = std::fs::read(format!("{prefix}/advertisement.bin")).unwrap();
        let refs = std::fs::read(format!("{prefix}/ls-response.bin")).unwrap();
        let pack = std::fs::read(format!("{prefix}/fetch-response.bin")).unwrap();
        Self::new(advert, refs, pack)
    }
    pub fn new(advert: Vec<u8>, refs: Vec<u8>, pack: Vec<u8>) -> Self {
        Self::with_types(advert, refs, pack, None, None)
    }
    pub fn with_types(
        advert: Vec<u8>,
        refs: Vec<u8>,
        pack: Vec<u8>,
        get_type: Option<&'static str>,
        post_type: Option<&'static str>,
    ) -> Self {
        Self::serve(advert, refs, pack, get_type, post_type, false)
    }
    pub fn redirect_posts(advert: Vec<u8>, refs: Vec<u8>, pack: Vec<u8>) -> Self {
        Self::serve(advert, refs, pack, None, None, true)
    }
    fn serve(
        advert: Vec<u8>,
        refs: Vec<u8>,
        pack: Vec<u8>,
        get_type: Option<&'static str>,
        post_type: Option<&'static str>,
        redirect_post: bool,
    ) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(0));
        let (st, ct) = (stop.clone(), count.clone());
        let task = thread::spawn(move || {
            for incoming in listener.incoming() {
                if st.load(Ordering::SeqCst) {
                    break;
                }
                let mut socket = incoming.unwrap();
                let mut bytes = Vec::new();
                let mut buf = [0; 8192];
                while !bytes.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = socket.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                if bytes.is_empty() {
                    continue;
                }
                let header_end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
                let headers = String::from_utf8_lossy(&bytes[..header_end]).to_ascii_lowercase();
                let len: usize = headers
                    .lines()
                    .filter_map(|line| line.strip_prefix("content-length:"))
                    .next()
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0);
                while bytes.len() - header_end < len {
                    let n = socket.read(&mut buf).unwrap();
                    if n == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buf[..n]);
                }
                let get = headers.starts_with("get ");
                let ls = bytes[header_end..]
                    .windows(15)
                    .any(|w| w == b"command=ls-refs");
                let (content_type, body) = if get {
                    ("application/x-git-upload-pack-advertisement", &advert)
                } else if ls {
                    ("application/x-git-upload-pack-result", &refs)
                } else {
                    ("application/x-git-upload-pack-result", &pack)
                };
                let content_type = (if get { get_type } else { post_type }).unwrap_or(content_type);
                ct.fetch_add(1, Ordering::SeqCst);
                if redirect_post && !get {
                    socket.write_all(b"HTTP/1.1 307 Temporary Redirect\r\nLocation: /other\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
                } else {
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    socket.write_all(response.as_bytes()).unwrap();
                    socket.write_all(body).unwrap();
                }
            }
        });
        Self {
            url,
            count,
            stop,
            task: Some(task),
        }
    }
    pub fn rewrite(&self) -> Vec<lodi::fetch::Rewrite> {
        vec![lodi::fetch::Rewrite {
            prefix: "https://git.example.org/repo.git".into(),
            replacement: format!("{}/repo.git", self.url),
        }]
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.url.trim_start_matches("http://"));
        self.task.take().unwrap().join().unwrap();
    }
}
