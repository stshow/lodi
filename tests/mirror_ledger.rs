//! The loopback server's ledger of what it served (LD-390), which the gate's download counts
//! read: a request that LD-386 tried again is a failed attempt, named, and never a second
//! download; a second whole download of a URL still fails the count.
//!
//! Offline: a loopback [`support::Server`], and Lodi's own fetcher with the clock taken out of
//! its retry waits.

mod support;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lodi::fetch::{FetchTimeouts, Fetcher, HttpFetcher, Retry, Waiter, parse_rewrites};
use support::*;

/// LD-386's waiter with the clock taken out: it keeps each retry line and never sleeps.
#[derive(Default)]
struct Lines(Mutex<Vec<String>>);

impl Waiter for Lines {
    fn notice(&self, line: &str) {
        self.0.lock().unwrap().push(line.to_string());
    }

    fn wait(&self, _: Duration) {}

    fn random(&self) -> f64 {
        0.0
    }
}

/// The mirror's case in the gate: its own upstream fetch outlasts the client's wait for the
/// response headers, the client tries again (LD-386), and the second request is served whole.
/// The server holds the first request until its client has gone, then answers it.
#[test]
fn a_request_tried_again_after_its_client_gave_up_is_one_download() {
    let url = "https://archive.test/core/x.pkg.tar.zst".to_string();
    let body = b"one package file".to_vec();
    let server = Server::start([(url.clone(), body.clone())].into());
    server.outwait.lock().unwrap().push(url.clone());
    let lines = Arc::new(Lines::default());
    let fetcher = HttpFetcher::with_retry(
        parse_rewrites(&server.rewrite()).unwrap(),
        FetchTimeouts {
            headers: Duration::from_millis(500),
            ..FetchTimeouts::default()
        },
        Retry::default(),
        lines.clone(),
    );
    let mut got = Vec::new();
    fetcher.download(&url, &mut got, 1 << 20).unwrap();
    assert_eq!(got, body);
    assert_eq!(fetcher.requests(), std::slice::from_ref(&url), "one fetch");
    let lines = lines.0.lock().unwrap().clone();
    assert_eq!(
        lines.first().map(String::as_str),
        Some("archive.test: no response headers within 0.5 s; retrying in 2 s (attempt 2 of 10)"),
        "{lines:?}"
    );

    // The count the gate makes: one whole download, and every request LD-386 tried again named
    // as a failed attempt, one for each retry line.
    let served = server.served(Duration::from_secs(300));
    assert_eq!(served.len(), 1 + lines.len(), "{served:?}");
    let retried = downloaded_once(&served, &BTreeSet::from([url.clone()])).unwrap();
    assert_eq!(retried.len(), lines.len(), "{retried:?}");
    assert!(retried[0].starts_with(&format!("{url}: ")), "{retried:?}");
}

fn whole(url: &str) -> (String, Served) {
    (url.to_string(), Served::Complete)
}

fn failed(url: &str) -> (String, Served) {
    let why = "the client had stopped waiting before the answer began";
    (url.to_string(), Served::Failed(why.into()))
}

fn expected(urls: &[&str]) -> BTreeSet<String> {
    urls.iter().map(|u| u.to_string()).collect()
}

const A: &str = "https://archive.test/a.pkg.tar.zst";
const B: &str = "https://archive.test/b.pkg.tar.zst";

/// A failed attempt, then the whole download, of one URL: one download, the attempt named.
#[test]
fn a_failed_attempt_before_the_whole_download_is_named_and_not_counted() {
    let served = [failed(A), whole(A), whole(B)];
    assert_eq!(
        downloaded_once(&served, &expected(&[A, B])),
        Ok(vec![format!(
            "{A}: the client had stopped waiting before the answer began"
        )])
    );
}

/// A request the mirror had not answered when its client went on to the next attempt is a
/// failed attempt too.
#[test]
fn an_attempt_never_answered_is_named_and_not_counted() {
    let served = [(A.to_string(), Served::Waiting), whole(A)];
    let retried = downloaded_once(&served, &expected(&[A])).unwrap();
    assert_eq!(retried.len(), 1, "{retried:?}");
    assert!(
        retried[0].starts_with(&format!("{A}: never answered")),
        "{retried:?}"
    );
}

/// Only a whole answer is a download: a URL whose every request failed was not downloaded.
#[test]
fn a_url_never_served_whole_was_not_downloaded() {
    let e = downloaded_once(&[failed(A)], &expected(&[A])).unwrap_err();
    assert!(e.contains(&format!("{A} was never served whole")), "{e}");
}

/// Two whole downloads of one URL are a duplicate download, whatever else happened.
#[test]
fn a_second_whole_download_still_fails() {
    let e = downloaded_once(&[whole(A), failed(A), whole(A)], &expected(&[A])).unwrap_err();
    assert!(e.contains(&format!("{A} was downloaded 2 times")), "{e}");
}

/// LD-386 tries a fetch again only before it has its answer: a request after the whole download
/// is a second fetch, not a retry, even when it failed.
#[test]
fn a_request_after_the_whole_download_is_a_second_fetch() {
    let e = downloaded_once(&[whole(A), failed(A)], &expected(&[A])).unwrap_err();
    assert!(
        e.contains(&format!("{A} was asked for again after its whole download")),
        "{e}"
    );
}

/// One fetch makes at most `Retry::default().attempts` attempts; more failed attempts than
/// that before the whole download are more than one fetch.
#[test]
fn more_failed_attempts_than_one_fetch_makes_fail() {
    let attempts = Retry::default().attempts as usize;
    let mut served = vec![failed(A); attempts - 1];
    served.push(whole(A));
    assert!(downloaded_once(&served, &expected(&[A])).is_ok());
    served.insert(0, failed(A));
    let e = downloaded_once(&served, &expected(&[A])).unwrap_err();
    assert!(e.contains(&format!("{A} failed {attempts} times")), "{e}");
}

/// A request outside the expected set fails the count, answered or not, and so does an expected
/// URL never asked for.
#[test]
fn an_unexpected_request_or_a_missing_download_fails() {
    let e = downloaded_once(&[whole(A), failed(B)], &expected(&[A])).unwrap_err();
    assert!(e.contains(&format!("{B} was asked for")), "{e}");
    let e = downloaded_once(&[whole(A)], &expected(&[A, B])).unwrap_err();
    assert!(e.contains(&format!("{B} was never served whole")), "{e}");
}
