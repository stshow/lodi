//! Metadata fetching for `lodi lock` (LD-16).
//!
//! Every URL must be HTTPS. `LODI_FETCH_REWRITE` redirects URL prefixes to a mirror
//! (`<prefix>=<replacement>`, several separated by `;`): the lock always records the original
//! URL, and a replacement must be HTTPS or plain HTTP to a loopback address (a local mirror or
//! the offline test server). Every request is logged so callers can show what was fetched.
//!
//! Redirects are followed by this module, not by the HTTP client, so that every hop is checked:
//! at most [`MAX_REDIRECTS`] of them, and each target must be HTTPS. The one exemption from the
//! HTTPS rule is the loopback origin a rewrite named, and only for the request that rewrite
//! produced: a redirect that stays on that origin is followed, any other plain-HTTP target is
//! refused before it is connected to (M-1.0 u-4, LD-363).
//!
//! Time limits (LD-385, [`FetchTimeouts`]): resolving, connecting (TLS included), sending the
//! request and receiving the response headers each have their own; a body has no total limit.
//! It fails only when it stalls — no byte for the stall window, which also bounds every socket
//! read — or averages less than the floor over a whole window.
//!
//! Retries (LD-386, [`Retry`]): a failure that passes — an HTTP status 500, 502, 503 or 504, a
//! step before the response that timed out, a connection refused, reset or cut off, a name
//! lookup the resolver says to try again, a body that stalled or dripped under the floor — is
//! tried again from its first byte after a bounded, jittered, growing wait, with one line on
//! stderr per retry. An answer is never tried again: every 4xx, a refused URL or redirect, a body
//! over its limit. What is fetched and how it is verified are the same on every attempt.
//! `LODI_FETCH_ATTEMPTS` (1 to 10) sets fewer attempts; 1 never tries again.
//!
//! Conditional requests (LD-404, [`api_cache`]): a fetcher given an [`ApiCache`] keeps every answer
//! of the GitHub API that carried an `ETag` and sends `If-None-Match` with it the next time; a
//! `304 Not Modified` is answered from the kept body. Every other answer is what it was without
//! the cache, and the cache is never asked instead of the network.

pub mod api_cache;

pub use api_cache::ApiCache;

use std::io::{Read, Seek, Write};
use std::sync::{Arc, Mutex};

/// Every git smart-HTTP request sent by this process (`HttpFetcher::git_requests`).
static GIT_REQUESTS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
use std::time::Duration;

/// Largest metadata body accepted (a Debian `Packages.xz` is about 9 MB; the release list of
/// python-build-standalone about 2 MB per release).
pub const MAX_BODY_BYTES: u64 = 128 * 1024 * 1024;

/// Most redirects one request follows before it fails (the pinned client's own default is 10).
pub const MAX_REDIRECTS: u32 = 5;

/// Why a fetch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchError {
    /// Not HTTPS (and not an allowed loopback mirror).
    Insecure(String),
    /// A redirect of the first URL to a second that is not HTTPS (and not on the loopback
    /// origin a rewrite of the first named).
    InsecureRedirect(String, String),
    /// HTTP status other than 200, with the status.
    Status(String, u16),
    /// Connection, TLS, redirect or body failure.
    Transport(String, String),
    /// The URL is not in a fixture fetcher's table (tests only).
    NotFound(String),
    /// A failure that passes (LD-386), still there after the number of attempts given: the last
    /// attempt's error. Only a transient failure is ever wrapped so; an answer comes back as it is.
    Exhausted(u32, Box<FetchError>),
}

impl FetchError {
    /// The HTTP status the fetch ended with, when it ended with one, tried again or not.
    pub fn status(&self) -> Option<u16> {
        match self {
            FetchError::Status(_, status) => Some(*status),
            FetchError::Exhausted(_, last) => last.status(),
            _ => None,
        }
    }
}

impl std::fmt::Display for FetchError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FetchError::Insecure(url) => write!(f, "{url} is not an HTTPS URL"),
            FetchError::InsecureRedirect(url, to) => {
                write!(f, "{url}: redirected to {to}, which is not an HTTPS URL")
            }
            FetchError::Status(url, status) => write!(f, "{url}: HTTP status {status}"),
            FetchError::Transport(url, why) => write!(f, "{url}: {why}"),
            FetchError::NotFound(url) => write!(f, "{url}: not found"),
            FetchError::Exhausted(attempts, last) => {
                write!(f, "{last}; gave up after {attempts} attempts")
            }
        }
    }
}

/// Where a download's body goes: a writer that can be emptied, so that a download tried again
/// starts from its first byte and nothing a failed attempt wrote stays behind (LD-386).
pub trait Sink: Write {
    /// Drop everything written so far and start again at the beginning.
    fn restart(&mut self) -> std::io::Result<()>;
}

impl Sink for Vec<u8> {
    fn restart(&mut self) -> std::io::Result<()> {
        self.clear();
        Ok(())
    }
}

impl Sink for std::fs::File {
    fn restart(&mut self) -> std::io::Result<()> {
        self.set_len(0)?;
        self.seek(std::io::SeekFrom::Start(0)).map(drop)
    }
}

/// Something that returns the bytes behind a URL.
pub trait Fetcher {
    /// The body of a successful (200) GET of `url`.
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError>;

    /// The same, with request headers the caller supplies. A header **value is a secret**: it
    /// is handed to the transport and never logged, recorded or put in a diagnostic — only the
    /// URL is (`github_releases` sends `Authorization` from `GITHUB_TOKEN` this way). A fetcher
    /// that has no use for headers ignores them, which is what the test fetchers do.
    fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<Vec<u8>, FetchError> {
        let _ = headers;
        self.get(url)
    }
    /// Every URL requested so far, in order (the original URLs, before any rewrite).
    fn requests(&self) -> Vec<String>;

    /// Stream the body of a successful GET of `url` into `out`, failing once it exceeds
    /// `limit` bytes; returns the byte count. Artifacts use this instead of [`Fetcher::get`].
    /// A fetcher that tries again empties `out` first ([`Sink::restart`]).
    fn download(&self, url: &str, out: &mut dyn Sink, limit: u64) -> Result<u64, FetchError> {
        let body = self.get(url)?;
        if body.len() as u64 > limit {
            return Err(FetchError::Transport(
                url.to_string(),
                format!("body larger than {limit} bytes"),
            ));
        }
        out.write_all(&body)
            .map_err(|e| FetchError::Transport(url.to_string(), e.to_string()))?;
        Ok(body.len() as u64)
    }
}

/// A prefix rewrite from `LODI_FETCH_REWRITE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rewrite {
    pub prefix: String,
    pub replacement: String,
}

/// Parse `LODI_FETCH_REWRITE`: `<prefix>=<replacement>` pairs separated by `;`.
pub fn parse_rewrites(text: &str) -> Result<Vec<Rewrite>, String> {
    let mut out = Vec::new();
    for pair in text.split(';').map(str::trim).filter(|p| !p.is_empty()) {
        let (prefix, replacement) = pair
            .split_once('=')
            .ok_or_else(|| format!("`{pair}` is not <prefix>=<replacement>"))?;
        if !prefix.starts_with("https://") {
            return Err(format!("rewrite prefix `{prefix}` is not an HTTPS URL"));
        }
        if !(replacement.starts_with("https://") || is_loopback_http(replacement)) {
            return Err(format!(
                "rewrite target `{replacement}` must be HTTPS or http://127.0.0.1 / http://[::1] / http://localhost"
            ));
        }
        out.push(Rewrite {
            prefix: prefix.to_string(),
            replacement: replacement.to_string(),
        });
    }
    Ok(out)
}

/// Parse `LODI_FETCH_ATTEMPTS` (LD-386): how many attempts a fetch whose failure passes gets, the
/// first included — a whole number from 1, which never tries again, to the default's 10, whose
/// waits add up to [`Retry::budget`]. Nothing else is read as a count: not a sign, a space, an
/// empty value or anything past 10.
pub fn parse_attempts(text: &str) -> Result<u32, String> {
    let most = Retry::default().attempts;
    match text.parse::<u32>() {
        Ok(n) if text.bytes().all(|b| b.is_ascii_digit()) && (1..=most).contains(&n) => Ok(n),
        _ => Err(format!(
            "expected a whole number of attempts from 1 to {most}, not {text:?}"
        )),
    }
}

/// Whether `url` is plain HTTP to a loopback host.
pub fn is_loopback_http(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("http://") else {
        return false;
    };
    let host = rest.split('/').next().unwrap_or("");
    let host = match host.strip_prefix('[') {
        Some(v6) => v6.split(']').next().unwrap_or(""),
        None => host.rsplit_once(':').map_or(host, |(h, _)| h),
    };
    matches!(host, "127.0.0.1" | "::1" | "localhost")
}

/// `scheme://authority` of an absolute URL, lower-cased; `None` for anything else.
fn origin(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    if scheme.is_empty()
        || !scheme
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))
    {
        return None;
    }
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    Some(format!("{scheme}://{authority}").to_ascii_lowercase())
}

/// The absolute URL a `Location` header names, resolved against the URL that answered with it.
pub fn redirect_target(base: &str, location: &str) -> Option<String> {
    let location = location.trim();
    if location.is_empty() {
        return None;
    }
    if let Some(found) = origin(location) {
        // An absolute target keeps its own path; only its scheme and host are normalised.
        return Some(format!("{found}{}", &location[found.len()..]));
    }
    let base_origin = origin(base)?;
    if location.starts_with("//") {
        let scheme = base_origin.split("://").next()?;
        return redirect_target(base, &format!("{scheme}:{location}"));
    }
    if location.starts_with('/') {
        return Some(format!("{base_origin}{location}"));
    }
    let path = &base[base_origin.len()..];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let dir = path.rsplit_once('/').map_or("", |(dir, _)| dir);
    Some(format!("{base_origin}{dir}/{location}"))
}

fn has_userinfo(url: &str) -> bool {
    url.split_once("://").is_some_and(|(_, rest)| {
        rest.split(['/', '?', '#'])
            .next()
            .is_some_and(|authority| authority.contains('@'))
    })
}

/// Whether a redirect to `next` may be followed. HTTPS always may; plain HTTP only to
/// `exempt`, the loopback origin a rewrite named for this very request.
pub fn redirect_allowed(next: &str, exempt: Option<&str>) -> bool {
    if origin(next).is_some_and(|o| o.starts_with("https://")) {
        return true;
    }
    exempt.is_some_and(|exempt| is_loopback_http(next) && origin(next).as_deref() == Some(exempt))
}

/// How long each phase of a request may take, and the stall rule of its body (LD-385).
///
/// There is no cap on a whole request or on a whole body: a download may take as long as it
/// takes while bytes keep arriving. What fails it is a stall — no byte for [`stall`], every
/// socket read of the connection bounded by that window — or a body slower than [`min_rate`]
/// bytes a second over a whole window of that length.
///
/// [`stall`]: FetchTimeouts::stall
/// [`min_rate`]: FetchTimeouts::min_rate
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FetchTimeouts {
    /// Resolving the host name.
    pub resolve: Duration,
    /// Opening the connection, the TLS handshake included.
    pub connect: Duration,
    /// Sending the request.
    pub send: Duration,
    /// Receiving the response headers once the request is sent.
    pub headers: Duration,
    /// The longest a body may go without a single byte arriving, and the window `min_rate` is
    /// measured over.
    pub stall: Duration,
    /// The fewest bytes a second a body may average over a whole `stall` window.
    pub min_rate: u64,
}

impl Default for FetchTimeouts {
    fn default() -> Self {
        FetchTimeouts {
            resolve: Duration::from_secs(30),
            connect: Duration::from_secs(30),
            send: Duration::from_secs(30),
            headers: Duration::from_secs(60),
            stall: Duration::from_secs(60),
            min_rate: 1024,
        }
    }
}

/// How a fetch whose failure passes is tried again (LD-386).
///
/// After the first attempt fails for a reason that passes, retry `n` (1 for the first) waits
/// [`base_wait`]`(n)` — `first` doubled `n − 1` times, at most `max_wait` — less a random share of
/// up to `jitter_percent` of it, so that clients that failed together do not come back together.
/// A 503's `Retry-After` lengthens a wait up to `max_wait` and never shortens one. The waits of
/// one fetch never add up to more than [`budget`], the undrawn schedule's sum: a retry that would
/// pass it is not made. At most `attempts` attempts are made in all, the first included.
///
/// [`base_wait`]: Retry::base_wait
/// [`budget`]: Retry::budget
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Attempts in all, the first included; 1 never tries again.
    pub attempts: u32,
    /// The wait before the first retry.
    pub first: Duration,
    /// The longest a single wait may be, a `Retry-After` included.
    pub max_wait: Duration,
    /// The largest share of a wait, in percent, its jitter may take off.
    pub jitter_percent: u32,
}

impl Default for Retry {
    fn default() -> Self {
        Retry {
            attempts: 10,
            first: Duration::from_secs(2),
            max_wait: Duration::from_secs(60),
            jitter_percent: 20,
        }
    }
}

impl Retry {
    /// One attempt, never another.
    pub const ONCE: Retry = Retry {
        attempts: 1,
        first: Duration::ZERO,
        max_wait: Duration::ZERO,
        jitter_percent: 0,
    };

    /// The wait before retry `n` (1 for the first), before jitter.
    pub fn base_wait(&self, n: u32) -> Duration {
        let doubling = 1u32.checked_shl(n.saturating_sub(1)).unwrap_or(u32::MAX);
        self.first.saturating_mul(doubling).min(self.max_wait)
    }

    /// The most the waits of one fetch add up to: every base wait of the schedule.
    pub fn budget(&self) -> Duration {
        (1..self.attempts).map(|n| self.base_wait(n)).sum()
    }

    /// Retry `n`'s wait with its jitter drawn: `random`, in `[0, 1)`, of `jitter_percent` taken
    /// off the base wait, to the millisecond.
    pub fn drawn(&self, n: u32, random: f64) -> Duration {
        let share = f64::from(self.jitter_percent.min(100)) / 100.0 * random.clamp(0.0, 1.0);
        let wait = self.base_wait(n).mul_f64(1.0 - share);
        Duration::from_millis(wait.as_millis() as u64)
    }
}

/// What a retry does besides fetching again (LD-386): say so, draw its jitter, wait. `lodi`
/// writes the line to stderr and sleeps; a test records both and returns at once.
pub trait Waiter: Send + Sync {
    /// Say that a fetch is about to be tried again: one line, already worded.
    fn notice(&self, line: &str);
    /// Wait `d` before the next attempt.
    fn wait(&self, d: Duration);
    /// A number in `[0, 1)` that draws the next wait's jitter.
    fn random(&self) -> f64;
}

/// The waiter `lodi` uses: stderr, the clock, and the standard library's per-process random keys
/// for the jitter (timing only; nothing fetched or verified depends on it).
struct Stderr;

impl Waiter for Stderr {
    fn notice(&self, line: &str) {
        eprintln!("lodi: {line}");
    }

    fn wait(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn random(&self) -> f64 {
        use std::hash::{BuildHasher, Hasher};
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        hasher.write_u128(now.map_or(0, |d| d.as_nanos()));
        (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// One attempt that failed: its error, whether trying again may succeed, and how long a 503 asked
/// to be left alone.
struct Failure {
    error: FetchError,
    transient: bool,
    retry_after: Option<Duration>,
}

impl From<FetchError> for Failure {
    /// A refusal: not tried again.
    fn from(error: FetchError) -> Failure {
        Failure {
            error,
            transient: false,
            retry_after: None,
        }
    }
}

/// A response other than 200, classified once for every path — GET, download and the git
/// exchange (#175): 500, 502, 503 and 504 pass, and a 503's `Retry-After` says how long to wait.
fn refused(url: &str, status: u16, headers: &ureq::http::HeaderMap) -> Failure {
    let retry_after = headers
        .get("retry-after")
        .filter(|_| status == 503)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| retry_after(v, crate::util::now_utc()));
    Failure {
        error: FetchError::Status(url.to_string(), status),
        transient: matches!(status, 500 | 502 | 503 | 504),
        retry_after,
    }
}

/// Whether a request that failed may succeed if it is tried again (LD-386): a timeout of any
/// step, and the I/O failures [`transient_io`] names. A certificate or protocol error, a name
/// with no address, a malformed response are not.
fn transient(e: &ureq::Error) -> bool {
    match e {
        ureq::Error::Timeout(_) => true,
        ureq::Error::Io(io) => transient_io(io),
        _ => false,
    }
}

/// Whether an I/O failure passes: a connection refused, reset, aborted or cut off, a network or
/// host unreachable for now, a timeout, or a name lookup the resolver says to try again. The
/// client hands its own errors through `std::io` wrapped; those are judged by [`transient`].
fn transient_io(e: &std::io::Error) -> bool {
    use std::io::ErrorKind as Kind;
    if let Some(inner) = e.get_ref().and_then(|i| i.downcast_ref::<ureq::Error>()) {
        return transient(inner);
    }
    matches!(
        e.kind(),
        Kind::ConnectionRefused
            | Kind::ConnectionReset
            | Kind::ConnectionAborted
            | Kind::NotConnected
            | Kind::BrokenPipe
            | Kind::TimedOut
            | Kind::WouldBlock
            | Kind::UnexpectedEof
            | Kind::NetworkUnreachable
            | Kind::HostUnreachable
            | Kind::NetworkDown
    ) || temporary_lookup_failure(e)
}

/// A name lookup that failed with `EAI_AGAIN`, in glibc's words and in musl's (the static binary):
/// the standard library hands every lookup failure through as text. A name that does not exist is
/// an answer and is not tried again.
fn temporary_lookup_failure(e: &std::io::Error) -> bool {
    let text = e.to_string();
    text.starts_with("failed to lookup address information: ")
        && (text.ends_with("Temporary failure in name resolution") || text.ends_with("Try again"))
}

/// The wait a `Retry-After` value asks for (RFC 9110 §10.2.3): a number of seconds, or an
/// IMF-fixdate less `now` (Unix seconds), a date already past being no wait. The obsolete date
/// forms and anything else give `None`, and the schedule's wait applies.
fn retry_after(value: &str, now: i64) -> Option<Duration> {
    let value = value.trim();
    if value.bytes().all(|b| b.is_ascii_digit()) {
        return value.parse().ok().map(Duration::from_secs);
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let fields: Vec<&str> = value.split(' ').collect();
    let [weekday, day, month, year, time, "GMT"] = fields[..] else {
        return None;
    };
    if weekday.len() != 4 || !weekday.ends_with(',') {
        return None;
    }
    let month = MONTHS.iter().position(|m| *m == month)? + 1;
    let at = crate::util::parse_utc(&format!("{year}-{month:02}-{day}T{time}Z"))?;
    Some(Duration::from_secs(u64::try_from(at - now).unwrap_or(0)))
}

/// A wait as the retry line says it: whole seconds, or [`seconds`] under one (tests' waits).
fn wait_text(d: Duration) -> String {
    if d >= Duration::from_secs(1) {
        format!("{} s", (d.as_millis() + 500) / 1000)
    } else {
        seconds(d)
    }
}

/// The host (and port) of `url` as a retry line names it: its authority without any user part.
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |(_, rest)| rest);
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    authority.rsplit('@').next().unwrap_or(authority)
}

/// `60 s`, or `0.3 s` for a window shorter than a whole number of seconds (tests use those).
fn seconds(d: Duration) -> String {
    let ms = d.as_millis();
    if ms % 1000 == 0 {
        format!("{} s", ms / 1000)
    } else {
        format!("{} s", ms as f64 / 1000.0)
    }
}

/// `300 of 1000 bytes` when the response said how long its body is, else `300 bytes`.
fn progress(got: u64, length: Option<u64>) -> String {
    match length {
        Some(length) => format!("{got} of {length} bytes"),
        None => format!("{got} bytes"),
    }
}

/// Whether a body read failed because its time ran out: the client hands its own timeout
/// through `std::io` wrapped, and a plain socket timeout is the same thing.
fn timed_out(e: &std::io::Error) -> bool {
    let wrapped = e
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<ureq::Error>())
        .is_some_and(|inner| matches!(inner, ureq::Error::Timeout(_)));
    wrapped
        || matches!(
            e.kind(),
            std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock
        )
}

/// A connector that bounds every read of a connection by the stall window. The pinned client's
/// own body timeout is a cap on the whole body, and with none set a read blocks for as long as
/// the peer stays silent; this layer, outermost in the client's own default chain, hands each
/// read the smaller of the client's deadline and the window, so no read can hang (LD-385).
/// A read it cut short fails as [`Timeout::RecvBody`](ureq::Timeout::RecvBody).
#[derive(Debug)]
struct ReadCap(Duration);

impl ureq::unversioned::transport::Connector<Box<dyn ureq::unversioned::transport::Transport>>
    for ReadCap
{
    type Out = CappedReads;

    fn connect(
        &self,
        _: &ureq::unversioned::transport::ConnectionDetails,
        chained: Option<Box<dyn ureq::unversioned::transport::Transport>>,
    ) -> Result<Option<CappedReads>, ureq::Error> {
        Ok(chained.map(|inner| CappedReads {
            inner,
            cap: self.0,
            http_1_0: false,
        }))
    }
}

/// A connection whose reads [`ReadCap`] bounds. It also marks itself closed once the server has
/// answered in HTTP/1.0 (#286): the pinned client honours `Connection: close` but keeps an
/// HTTP/1.0 connection that did not say `keep-alive`, and a request sent on it while the server
/// is closing fails. Everything else passes through unchanged.
#[derive(Debug)]
struct CappedReads {
    inner: Box<dyn ureq::unversioned::transport::Transport>,
    cap: Duration,
    http_1_0: bool,
}

impl ureq::unversioned::transport::Transport for CappedReads {
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        self.inner.buffers()
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<(), ureq::Error> {
        self.inner.transmit_output(amount, timeout)
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> Result<bool, ureq::Error> {
        let cap = ureq::unversioned::transport::time::Duration::from(self.cap);
        let timeout = if timeout.after > cap {
            ureq::unversioned::transport::NextTimeout {
                after: cap,
                reason: ureq::Timeout::RecvBody,
            }
        } else {
            timeout
        };
        let got = self.inner.await_input(timeout)?;
        // A response's head starts the unread input; an HTTP/1.0 one is never reused.
        if self.inner.buffers().input().starts_with(b"HTTP/1.0 ") {
            self.http_1_0 = true;
        }
        Ok(got)
    }

    fn is_open(&mut self) -> bool {
        !self.http_1_0 && self.inner.is_open()
    }

    fn is_tls(&self) -> bool {
        self.inner.is_tls()
    }
}

/// How a request that was answered ended (LD-404): with a body — a 200, whose `ETag` is kept —
/// or, for a conditional request only, with `304 Not Modified`.
enum Answer {
    Body { bytes: u64, etag: Option<String> },
    NotModified,
}

/// The HTTPS fetcher used by the `lodi` binary.
pub struct HttpFetcher {
    agent: ureq::Agent,
    rewrites: Vec<Rewrite>,
    timeouts: FetchTimeouts,
    retry: Retry,
    waiter: Arc<dyn Waiter>,
    log: Mutex<Vec<String>>,
    api_cache: Option<ApiCache>,
}

impl HttpFetcher {
    pub fn new(rewrites: Vec<Rewrite>) -> HttpFetcher {
        HttpFetcher::with_timeouts(rewrites, FetchTimeouts::default())
    }

    /// The same, with the time limits a test injects instead of [`FetchTimeouts::default`].
    pub fn with_timeouts(rewrites: Vec<Rewrite>, timeouts: FetchTimeouts) -> HttpFetcher {
        HttpFetcher::with_retry(rewrites, timeouts, Retry::default(), Arc::new(Stderr))
    }

    /// The same, with the retry schedule and the waiter a test injects instead of `lodi`'s own
    /// (LD-386): a test's waiter records the retry lines and the waits and never sleeps.
    pub fn with_retry(
        rewrites: Vec<Rewrite>,
        timeouts: FetchTimeouts,
        retry: Retry,
        waiter: Arc<dyn Waiter>,
    ) -> HttpFetcher {
        use ureq::unversioned::transport::{Connector, DefaultConnector};
        // The client neither follows redirects nor applies an agent-wide scheme rule: `open`
        // checks the first URL and every redirect itself, so the loopback exemption is scoped to
        // the one request a rewrite produced rather than to every request of the agent. Each
        // phase up to the response headers has its own limit; the body has none of the
        // client's, only the stall rule (`ReadCap` and `copy_body`).
        let config = ureq::Agent::config_builder()
            .https_only(false)
            .max_redirects(0)
            .http_status_as_error(false)
            .timeout_resolve(Some(timeouts.resolve))
            .timeout_connect(Some(timeouts.connect))
            .timeout_send_request(Some(timeouts.send))
            .timeout_recv_response(Some(timeouts.headers))
            .user_agent(concat!("lodi/", env!("CARGO_PKG_VERSION")))
            .build();
        let connector = DefaultConnector::new().chain(ReadCap(timeouts.stall));
        let agent = ureq::Agent::with_parts(
            config,
            connector,
            ureq::unversioned::resolver::DefaultResolver::default(),
        );
        HttpFetcher {
            agent,
            rewrites,
            timeouts,
            retry,
            waiter,
            log: Mutex::new(Vec::new()),
            api_cache: None,
        }
    }

    /// The same fetcher, keeping the GitHub API's answers in `cache` for conditional requests
    /// (LD-404). Without one, every request is made as it always was.
    pub fn with_api_cache(mut self, cache: Option<ApiCache>) -> HttpFetcher {
        self.api_cache = cache;
        self
    }

    /// What a request that failed before its body says: the phase that ran out of time and its
    /// limit, or the client's own words for anything else.
    fn request_error(&self, e: ureq::Error) -> String {
        let t = &self.timeouts;
        match e {
            ureq::Error::Timeout(ureq::Timeout::Resolve) => {
                format!("could not resolve the host within {}", seconds(t.resolve))
            }
            ureq::Error::Timeout(ureq::Timeout::Connect) => {
                format!("could not connect within {}", seconds(t.connect))
            }
            ureq::Error::Timeout(ureq::Timeout::SendRequest) => {
                format!("could not send the request within {}", seconds(t.send))
            }
            ureq::Error::Timeout(ureq::Timeout::RecvResponse) => {
                format!("no response headers within {}", seconds(t.headers))
            }
            ureq::Error::Timeout(_) => format!(
                "stalled: no data for {} while waiting for the response headers",
                seconds(t.stall)
            ),
            other => other.to_string(),
        }
    }

    /// Copy a response body into `out` under the stall rule, failing once it exceeds `limit`
    /// bytes. `length` is the body's `Content-Length`, when the response has one, for the
    /// message only. No limit applies to the whole body: it fails when no byte arrives for
    /// `stall` (the socket read `ReadCap` bounds times out), or when a whole window of that
    /// length averaged fewer than `min_rate` bytes a second. Both pass, and so does a connection
    /// cut off mid-body (LD-386); a body over its limit and a failed write to `out` do not.
    fn copy_body(
        &self,
        url: &str,
        reader: &mut dyn Read,
        out: &mut dyn Sink,
        limit: u64,
        length: Option<u64>,
    ) -> Result<u64, Failure> {
        let t = &self.timeouts;
        let failed = |why: String, transient: bool| Failure {
            error: FetchError::Transport(url.to_string(), why),
            transient,
            retry_after: None,
        };
        let mut buf = vec![0u8; 64 * 1024];
        let mut got: u64 = 0;
        let mut window = std::time::Instant::now();
        let mut in_window: u64 = 0;
        loop {
            let n = match reader.read(&mut buf) {
                Ok(0) => return Ok(got),
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if timed_out(&e) => {
                    let why = format!(
                        "stalled: no data for {} after {}",
                        seconds(t.stall),
                        progress(got, length)
                    );
                    return Err(failed(why, true));
                }
                Err(e) => return Err(failed(e.to_string(), transient_io(&e))),
            };
            got += n as u64;
            if got > limit {
                return Err(failed(format!("body larger than {limit} bytes"), false));
            }
            out.write_all(&buf[..n])
                .map_err(|e| failed(e.to_string(), false))?;
            in_window += n as u64;
            let elapsed = window.elapsed();
            if elapsed >= t.stall {
                if u128::from(in_window) * 1000 < u128::from(t.min_rate) * elapsed.as_millis() {
                    let why = format!(
                        "too slow: under {} bytes/s for {} after {}",
                        t.min_rate,
                        seconds(t.stall),
                        progress(got, length)
                    );
                    return Err(failed(why, true));
                }
                window = std::time::Instant::now();
                in_window = 0;
            }
        }
    }

    /// A fetcher configured from `LODI_FETCH_REWRITE` and `LODI_FETCH_ATTEMPTS`.
    pub fn from_env() -> Result<HttpFetcher, String> {
        let rewrites = match std::env::var("LODI_FETCH_REWRITE") {
            Ok(text) => parse_rewrites(&text).map_err(|e| format!("LODI_FETCH_REWRITE: {e}"))?,
            Err(_) => Vec::new(),
        };
        let mut retry = Retry::default();
        if let Some(text) = std::env::var_os("LODI_FETCH_ATTEMPTS") {
            retry.attempts = parse_attempts(&text.to_string_lossy())
                .map_err(|e| format!("LODI_FETCH_ATTEMPTS: {e}"))?;
        }
        Ok(HttpFetcher::with_retry(
            rewrites,
            FetchTimeouts::default(),
            retry,
            Arc::new(Stderr),
        ))
    }

    fn target(&self, url: &str) -> String {
        for r in &self.rewrites {
            if let Some(rest) = url.strip_prefix(&r.prefix) {
                return format!("{}{rest}", r.replacement);
            }
        }
        url.to_string()
    }

    /// GET `url` through its rewrite, following at most [`MAX_REDIRECTS`] redirects, each of
    /// which must pass [`redirect_allowed`]. The caller's headers go to the first request only:
    /// a redirect never carries them (the client's own default, kept).
    fn open(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<ureq::http::Response<ureq::Body>, Failure> {
        if !url.starts_with("https://") {
            return Err(FetchError::Insecure(url.to_string()).into());
        }
        let mut target = self.target(url);
        let exempt = is_loopback_http(&target).then(|| origin(&target)).flatten();
        let mut redirects = 0;
        loop {
            let mut request = self.agent.get(&target);
            if redirects == 0 {
                for (name, value) in headers {
                    request = request.header(*name, value.as_str());
                }
            }
            let response = request.call().map_err(|e| Failure {
                transient: transient(&e),
                error: FetchError::Transport(url.to_string(), self.request_error(e)),
                retry_after: None,
            })?;
            if !matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                return Ok(response);
            }
            let next = response
                .headers()
                .get("location")
                .and_then(|v| v.to_str().ok())
                .and_then(|location| redirect_target(&target, location))
                .ok_or_else(|| {
                    FetchError::Transport(
                        url.to_string(),
                        "a redirect with no usable Location".into(),
                    )
                })?;
            if !redirect_allowed(&next, exempt.as_deref()) {
                return Err(FetchError::InsecureRedirect(url.to_string(), next).into());
            }
            if redirects == MAX_REDIRECTS {
                return Err(FetchError::Transport(
                    url.to_string(),
                    format!("more than {MAX_REDIRECTS} redirects"),
                )
                .into());
            }
            redirects += 1;
            target = next;
        }
    }

    /// One attempt at `url`: the response, and its body copied into `out`. A status other than
    /// 200 fails the attempt; 500, 502, 503 and 504 pass, and a 503 says how long to wait. A 304
    /// is an answer only to a `conditional` request, which carried `If-None-Match`.
    fn attempt(
        &self,
        url: &str,
        headers: &[(&str, String)],
        conditional: bool,
        out: &mut dyn Sink,
        limit: u64,
    ) -> Result<Answer, Failure> {
        let mut response = self.open(url, headers)?;
        let status = response.status().as_u16();
        if status == 304 && conditional {
            return Ok(Answer::NotModified);
        }
        if status != 200 {
            return Err(refused(url, status, response.headers()));
        }
        let etag = response
            .headers()
            .get("etag")
            .and_then(|v| v.to_str().ok())
            .map(str::to_string);
        let length = response.body().content_length();
        let reader = &mut response.body_mut().as_reader();
        let bytes = self.copy_body(url, reader, out, limit, length)?;
        Ok(Answer::Body { bytes, etag })
    }

    /// Fetch `url` into `out`, trying a failure that passes again under [`Retry`] (LD-386).
    /// Every attempt is the same request, verified the same way, and starts from nothing: `out`
    /// is emptied before each retry. An answer, and the last failure once the attempts or the
    /// budget are spent, end the fetch; the latter says how many attempts were made.
    fn fetch(
        &self,
        url: &str,
        headers: &[(&str, String)],
        conditional: bool,
        out: &mut dyn Sink,
        limit: u64,
    ) -> Result<Answer, FetchError> {
        let retry = &self.retry;
        let budget = retry.budget();
        let mut waited = Duration::ZERO;
        let mut made = 0;
        loop {
            made += 1;
            let failure = match self.attempt(url, headers, conditional, out, limit) {
                Ok(n) => return Ok(n),
                Err(failure) => failure,
            };
            let gave_up = |error: FetchError| match made {
                1 => error,
                _ => FetchError::Exhausted(made, Box::new(error)),
            };
            if !failure.transient {
                return Err(failure.error);
            }
            if made >= retry.attempts {
                return Err(gave_up(failure.error));
            }
            let mut wait = retry.drawn(made, self.waiter.random());
            if let Some(asked) = failure.retry_after {
                wait = wait.max(asked).min(retry.max_wait);
            }
            if waited + wait > budget {
                return Err(gave_up(failure.error));
            }
            out.restart()
                .map_err(|e| FetchError::Transport(url.to_string(), e.to_string()))?;
            let what = match &failure.error {
                FetchError::Status(_, status) => format!(" answered HTTP status {status}"),
                FetchError::Transport(_, why) => format!(": {why}"),
                other => format!(": {other}"),
            };
            self.waiter.notice(&format!(
                "{}{what}; retrying in {} (attempt {} of {})",
                host_of(url),
                wait_text(wait),
                made + 1,
                retry.attempts
            ));
            self.waiter.wait(wait);
            waited += wait;
        }
    }

    /// Run `attempt`, a download another program makes (a package manager's dated sync), again
    /// under this fetcher's [`Retry`] while `passes` finds a reason in its failure, saying so as a
    /// fetch does (LD-386). The last failure is returned, with the attempts made once there was
    /// more than one.
    pub fn retrying(
        &self,
        mut attempt: impl FnMut() -> Result<(), String>,
        passes: impl Fn(&str) -> Option<String>,
    ) -> Result<(), String> {
        let mut made = 0;
        let mut waited = Duration::ZERO;
        loop {
            made += 1;
            let error = match attempt() {
                Ok(()) => return Ok(()),
                Err(error) => error,
            };
            let wait = self.retry.drawn(made, self.waiter.random());
            let reason = passes(&error);
            let Some(reason) = reason
                .filter(|_| made < self.retry.attempts && waited + wait <= self.retry.budget())
            else {
                return Err(match made {
                    1 => error,
                    _ => format!("{error}; gave up after {made} attempts"),
                });
            };
            self.waiter.notice(&format!(
                "{reason}; retrying in {} (attempt {} of {})",
                wait_text(wait),
                made + 1,
                self.retry.attempts
            ));
            self.waiter.wait(wait);
            waited += wait;
        }
    }

    /// The git smart-HTTP requests this process has sent, retries and redirects included: a
    /// refused URL names it, so a gate step can record what it asked a forge (LD-456).
    pub fn git_requests() -> u64 {
        GIT_REQUESTS.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Git smart HTTP v2 exchange. This is deliberately beside GET and download: POSTing
    /// upload-pack is read-only, so a transient failure can retry from its first byte.
    /// Both response types, all redirects, and the entire response body are checked here.
    pub fn git_exchange(
        &self,
        url: &str,
        body: Option<&[u8]>,
        limit: u64,
    ) -> Result<Vec<u8>, FetchError> {
        if !url.starts_with("https://") {
            return Err(FetchError::Insecure(url.to_string()));
        }
        // A plain HTTPS git URL is public-only. Never forward a userinfo authority to ureq.
        if has_userinfo(url) {
            return Err(FetchError::Insecure("git URL with credentials".into()));
        }
        self.logged(url);
        let mut made = 0;
        let mut waited = Duration::ZERO;
        loop {
            made += 1;
            let result = self.git_attempt(url, body, limit);
            let failure = match result {
                Ok(result) => return Ok(result),
                Err(failure) => failure,
            };
            if !failure.transient {
                return Err(failure.error);
            }
            if made >= self.retry.attempts {
                return Err(if made == 1 {
                    failure.error
                } else {
                    FetchError::Exhausted(made, Box::new(failure.error))
                });
            }
            let mut wait = self.retry.drawn(made, self.waiter.random());
            if let Some(asked) = failure.retry_after {
                wait = wait.max(asked).min(self.retry.max_wait);
            }
            if waited + wait > self.retry.budget() {
                return Err(FetchError::Exhausted(made, Box::new(failure.error)));
            }
            self.waiter.notice(&format!(
                "{}: {}; retrying in {} (attempt {} of {})",
                host_of(url),
                failure.error,
                wait_text(wait),
                made + 1,
                self.retry.attempts
            ));
            self.waiter.wait(wait);
            waited += wait;
        }
    }

    fn git_attempt(&self, url: &str, body: Option<&[u8]>, limit: u64) -> Result<Vec<u8>, Failure> {
        let mut target = self.target(url);
        if has_userinfo(&target) {
            return Err(FetchError::Insecure("git rewrite with credentials".into()).into());
        }
        let exempt = is_loopback_http(&target).then(|| origin(&target)).flatten();
        let mut redirects = 0;
        let expected = if body.is_some() {
            "application/x-git-upload-pack-result"
        } else {
            "application/x-git-upload-pack-advertisement"
        };
        loop {
            GIT_REQUESTS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let response = if let Some(body) = body {
                self.agent
                    .post(&target)
                    .header("Git-Protocol", "version=2")
                    .header("Content-Type", "application/x-git-upload-pack-request")
                    .send(body)
            } else {
                self.agent
                    .get(&target)
                    .header("Git-Protocol", "version=2")
                    .call()
            };
            let mut response = response.map_err(|e| Failure {
                transient: transient(&e),
                error: FetchError::Transport(url.to_string(), self.request_error(e)),
                retry_after: None,
            })?;
            let status = response.status().as_u16();
            if matches!(status, 301 | 302 | 303 | 307 | 308) {
                // POST carries the upload-pack command and object IDs. Do not replay it
                // at any redirect target, even on the same origin or for 307/308.
                if body.is_some() {
                    return Err(FetchError::Transport(
                        url.to_string(),
                        "git POST redirect refused".into(),
                    )
                    .into());
                }
                let next = response
                    .headers()
                    .get("location")
                    .and_then(|h| h.to_str().ok())
                    .and_then(|h| redirect_target(&target, h))
                    .ok_or_else(|| {
                        FetchError::Transport(
                            url.to_string(),
                            "a redirect with no usable Location".into(),
                        )
                    })?;
                if has_userinfo(&next) {
                    return Err(FetchError::Insecure("git redirect with credentials".into()).into());
                }
                if !redirect_allowed(&next, exempt.as_deref()) {
                    return Err(FetchError::InsecureRedirect(url.to_string(), next).into());
                }
                if redirects == MAX_REDIRECTS {
                    return Err(FetchError::Transport(
                        url.to_string(),
                        format!("more than {MAX_REDIRECTS} redirects"),
                    )
                    .into());
                }
                redirects += 1;
                target = next;
                continue;
            }
            if status != 200 {
                return Err(refused(url, status, response.headers()));
            }
            if response
                .headers()
                .get("content-type")
                .and_then(|h| h.to_str().ok())
                .map(|h| h.split(';').next().unwrap_or("").trim())
                != Some(expected)
            {
                return Err(FetchError::Transport(
                    url.to_string(),
                    format!("expected Content-Type {expected}"),
                )
                .into());
            }
            let length = response.body().content_length();
            let mut out = Vec::new();
            self.copy_body(
                url,
                &mut response.body_mut().as_reader(),
                &mut out,
                limit,
                length,
            )?;
            return Ok(out);
        }
    }

    fn logged(&self, url: &str) {
        if let Ok(mut log) = self.log.lock() {
            log.push(url.to_string());
        }
    }
}

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, FetchError> {
        self.get_with_headers(url, &[])
    }

    fn get_with_headers(
        &self,
        url: &str,
        headers: &[(&str, String)],
    ) -> Result<Vec<u8>, FetchError> {
        self.logged(url);
        // LD-404: an answer of the GitHub API is asked for with the tag of the one kept, when a
        // sound one is kept. The request is made either way; only a 304 is answered from the
        // cache, and a 200 replaces what was kept.
        let slot = self
            .api_cache
            .as_ref()
            .and_then(|cache| cache.slot(url, headers));
        let kept = slot.as_ref().and_then(api_cache::Slot::load);
        let mut sent = headers.to_vec();
        if let Some(kept) = &kept {
            sent.push(("If-None-Match", kept.etag.clone()));
        }
        let mut body = Vec::new();
        match self.fetch(url, &sent, kept.is_some(), &mut body, MAX_BODY_BYTES)? {
            Answer::NotModified => match kept {
                Some(kept) => Ok(kept.body),
                None => Err(FetchError::Status(url.to_string(), 304)),
            },
            Answer::Body { etag, .. } => {
                if let (Some(slot), Some(etag)) = (&slot, etag) {
                    slot.store(&etag, &body);
                }
                Ok(body)
            }
        }
    }

    fn requests(&self) -> Vec<String> {
        self.log.lock().map(|l| l.clone()).unwrap_or_default()
    }

    fn download(&self, url: &str, out: &mut dyn Sink, limit: u64) -> Result<u64, FetchError> {
        self.logged(url);
        match self.fetch(url, &[], false, out, limit)? {
            Answer::Body { bytes, .. } => Ok(bytes),
            Answer::NotModified => Err(FetchError::Status(url.to_string(), 304)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rewrites_are_https_or_loopback() {
        let r = parse_rewrites(
            "https://snapshot.debian.org/=http://127.0.0.1:8080/snap/; https://a/=https://b/",
        )
        .unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].replacement, "http://127.0.0.1:8080/snap/");
        for bad in [
            "https://a/=http://example.com/",
            "http://a/=https://b/",
            "https://a/",
            "https://a/=http://127.0.0.1.evil/",
        ] {
            assert!(parse_rewrites(bad).is_err(), "{bad}");
        }
        assert!(is_loopback_http("http://[::1]:9/x"));
        assert!(is_loopback_http("http://localhost/x"));
        assert!(!is_loopback_http("https://127.0.0.1/"));
    }

    #[test]
    fn plain_http_is_refused_before_any_connection() {
        let f = HttpFetcher::new(Vec::new());
        assert_eq!(
            f.get("http://snapshot.debian.org/archive/"),
            Err(FetchError::Insecure(
                "http://snapshot.debian.org/archive/".into()
            ))
        );
        assert_eq!(f.requests(), ["http://snapshot.debian.org/archive/"]);
    }

    /// A loopback server answering `/hop/<n>` with a redirect to `/hop/<n-1>` (and `/hop/0`
    /// with `ok`), `/away` with a redirect to plain HTTP on another loopback origin,
    /// `/relative/<n>` like `/hop` but with a relative `Location`, and `/location?<url>` with a
    /// redirect to `<url>`.
    fn redirect_server() -> u16 {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                let _ = reader.read_line(&mut line);
                loop {
                    let mut header = String::new();
                    if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                        break;
                    }
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                let redirect = |to: String| {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: {to}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    )
                };
                let reply = if path == "/hop/0" || path == "/relative/0" {
                    "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok"
                        .to_string()
                } else if let Some(n) = path.strip_prefix("/hop/") {
                    let n: u32 = n.parse().unwrap();
                    redirect(format!("/hop/{}", n - 1))
                } else if let Some(n) = path.strip_prefix("/relative/") {
                    let n: u32 = n.parse().unwrap();
                    redirect(format!("{}", n - 1))
                } else if path == "/away" {
                    redirect("http://localhost:9/elsewhere".to_string())
                } else if let Some(to) = path.strip_prefix("/location?") {
                    redirect(to.to_string())
                } else {
                    "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                        .to_string()
                };
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        port
    }

    fn loopback_fetcher(port: u16) -> HttpFetcher {
        HttpFetcher::new(
            parse_rewrites(&format!("https://mirror.test/=http://127.0.0.1:{port}/")).unwrap(),
        )
    }

    /// A loopback server that answers every request with `reply` and then, like a slow
    /// HTTP/1.0 server, waits 0.3 s before it closes the connection (#286).
    fn slow_close_server(reply: &'static str) -> u16 {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut line = String::new();
                        if reader.read_line(&mut line).is_err() || line.trim().is_empty() {
                            break;
                        }
                    }
                    let _ = stream.write_all(reply.as_bytes());
                    std::thread::sleep(Duration::from_millis(300));
                });
            }
        });
        port
    }

    fn two_gets(reply: &'static str) -> (Result<Vec<u8>, FetchError>, Result<Vec<u8>, FetchError>) {
        let port = slow_close_server(reply);
        let f = HttpFetcher::with_retry(
            parse_rewrites(&format!("https://mirror.test/=http://127.0.0.1:{port}/")).unwrap(),
            FetchTimeouts::default(),
            Retry::ONCE,
            Arc::new(Stderr),
        );
        let first = f.get("https://mirror.test/a");
        let second = f.get("https://mirror.test/b");
        (first, second)
    }

    /// #286: an HTTP/1.0 answer without `keep-alive` closes its connection; the next request
    /// opens a new one instead of failing on the closing one.
    #[test]
    fn an_http_1_0_answer_is_not_reused() {
        let (first, second) = two_gets("HTTP/1.0 200 OK\r\nContent-Length: 2\r\n\r\nok");
        assert_eq!(first.unwrap(), b"ok");
        assert_eq!(second.unwrap(), b"ok");
    }

    /// The same for an HTTP/1.1 answer that says `Connection: close`.
    #[test]
    fn a_connection_close_answer_is_not_reused() {
        let (first, second) =
            two_gets("HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
        assert_eq!(first.unwrap(), b"ok");
        assert_eq!(second.unwrap(), b"ok");
    }

    #[test]
    fn the_loopback_rewrite_follows_redirects_on_its_own_origin_up_to_the_cap() {
        let f = loopback_fetcher(redirect_server());
        let within = format!("https://mirror.test/hop/{MAX_REDIRECTS}");
        assert_eq!(f.get(&within).unwrap(), b"ok");
        let relative = format!("https://mirror.test/relative/{MAX_REDIRECTS}");
        assert_eq!(f.get(&relative).unwrap(), b"ok");
        let mut out = Vec::new();
        assert_eq!(f.download(&within, &mut out, 10).unwrap(), 2);
    }

    #[test]
    fn a_redirect_chain_past_the_cap_is_refused() {
        // Five redirects are followed and a sixth is not: fewer than the client's own ten.
        assert_eq!(MAX_REDIRECTS, 5);
        let f = loopback_fetcher(redirect_server());
        assert_eq!(f.get("https://mirror.test/hop/5").unwrap(), b"ok");
        let past = "https://mirror.test/hop/6";
        let expected = FetchError::Transport(past.into(), "more than 5 redirects".into());
        assert_eq!(f.get(past).unwrap_err(), expected);
        assert_eq!(f.download(past, &mut Vec::new(), 10).unwrap_err(), expected);
    }

    #[test]
    fn a_redirect_to_plain_http_off_the_rewritten_origin_is_refused() {
        let f = loopback_fetcher(redirect_server());
        let url = "https://mirror.test/away";
        let refused =
            FetchError::InsecureRedirect(url.into(), "http://localhost:9/elsewhere".into());
        assert_eq!(f.get(url).unwrap_err(), refused);
        assert_eq!(f.download(url, &mut Vec::new(), 10).unwrap_err(), refused);
    }

    /// A loopback listener that counts the connections made to it and answers none of them.
    fn tripwire() -> (u16, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                count.fetch_add(1, Ordering::SeqCst);
                drop(stream);
            }
        });
        (port, seen)
    }

    #[test]
    fn an_https_fetch_redirected_to_plain_http_is_refused_before_connecting() {
        // The fetch starts from an HTTPS URL; its rewrite exempts the loopback server's own
        // origin and nothing else. The server redirects it to plain HTTP elsewhere: a
        // documentation address (RFC 5737, never routed) and a second loopback port that counts
        // connections. Each is refused with `InsecureRedirect`; a followed hop would end in a
        // transport error instead, and the counter shows no connection was made.
        let (trip, connections) = tripwire();
        let f = loopback_fetcher(redirect_server());
        let (mut got, mut want) = (Vec::new(), Vec::new());
        for to in [
            "http://203.0.113.1/elsewhere".to_string(),
            format!("http://127.0.0.1:{trip}/elsewhere"),
        ] {
            let url = format!("https://mirror.test/location?{to}");
            got.push(f.get(&url).map(drop));
            got.push(f.download(&url, &mut Vec::new(), 10).map(drop));
            let refused = FetchError::InsecureRedirect(url, to);
            want.extend([Err(refused.clone()), Err(refused)]);
        }
        assert_eq!(got, want);
        let made = connections.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(made, 0, "a refused redirect target was connected to");
    }

    #[test]
    fn an_https_request_is_never_redirected_to_plain_http() {
        // No rewrite: nothing is exempt, loopback or not.
        for next in [
            "http://example.org/x",
            "http://127.0.0.1:8080/x",
            "HTTP://example.org/x",
            "ftp://example.org/x",
        ] {
            assert!(!redirect_allowed(next, None), "{next}");
        }
        assert!(redirect_allowed("https://example.org/x", None));
        assert!(redirect_allowed("HTTPS://Example.org/x", None));
        // A rewrite to loopback exempts that origin only.
        let exempt = Some("http://127.0.0.1:8080");
        assert!(redirect_allowed("http://127.0.0.1:8080/y", exempt));
        assert!(!redirect_allowed("http://127.0.0.1:8081/y", exempt));
        assert!(!redirect_allowed("http://localhost:8080/y", exempt));
        assert!(!redirect_allowed("http://example.org/y", exempt));
        assert!(redirect_allowed("https://example.org/y", exempt));
    }

    #[test]
    fn redirect_targets_resolve_against_the_answering_url() {
        let base = "https://a.test/x/y/z?q=1";
        for (location, want) in [
            ("https://b.test/p", "https://b.test/p"),
            ("HTTP://B.test/P", "http://b.test/P"),
            ("//c.test/p", "https://c.test/p"),
            ("/p?r=2", "https://a.test/p?r=2"),
            ("w", "https://a.test/x/y/w"),
        ] {
            assert_eq!(
                redirect_target(base, location).as_deref(),
                Some(want),
                "{location}"
            );
        }
        assert_eq!(
            redirect_target("https://a.test", "w").as_deref(),
            Some("https://a.test/w")
        );
        assert_eq!(redirect_target(base, " "), None);
    }

    /// What the pinned client does on its own, for the record: it follows ten redirects and
    /// fails on the eleventh, and with its scheme rule off it follows a redirect to any plain
    /// HTTP origin. That is why this module neither leaves redirects to it nor switches its
    /// scheme rule off for the whole agent.
    #[test]
    fn the_pinned_client_alone_follows_ten_redirects_to_any_scheme() {
        let port = redirect_server();
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build()
            .into();
        let url = |p: &str| format!("http://127.0.0.1:{port}{p}");
        let status = |p: &str| agent.get(&url(p)).call().map(|r| r.status().as_u16());
        assert_eq!(status("/hop/10").unwrap(), 200);
        assert!(matches!(
            status("/hop/11"),
            Err(ureq::Error::TooManyRedirects)
        ));
        // `/away` points at a plain-HTTP port nothing listens on: the client tried to follow it.
        assert!(matches!(status("/away"), Err(ureq::Error::Io(_))));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    /// Time limits a test can wait out: every one of them a second or less.
    const QUICK: FetchTimeouts = FetchTimeouts {
        resolve: Duration::from_secs(1),
        connect: Duration::from_secs(1),
        send: Duration::from_secs(1),
        headers: Duration::from_secs(1),
        stall: Duration::from_secs(1),
        min_rate: 1024,
    };

    /// One attempt under `timeouts` and never another: the limits of a single request are what
    /// these tests check; the retries around them are LD-386's tests below.
    fn fetcher_with(port: u16, timeouts: FetchTimeouts) -> HttpFetcher {
        HttpFetcher::with_retry(
            parse_rewrites(&format!("https://mirror.test/=http://127.0.0.1:{port}/")).unwrap(),
            timeouts,
            Retry::ONCE,
            Recorder::new(),
        )
    }

    /// How long a test server keeps a silent connection open when the client does not give up
    /// first: a fetcher that never timed out fails its test on the close, it does not hang it.
    const HOLD: Duration = Duration::from_secs(30);

    /// A loopback server that reads each request and hands its path and the connection to
    /// `answer`, one thread per connection.
    fn serve(answer: fn(&str, &mut std::net::TcpStream)) -> u16 {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    let mut request = String::new();
                    let _ = reader.read_line(&mut request);
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0
                            || header.trim().is_empty()
                        {
                            break;
                        }
                    }
                    let path = request.split_whitespace().nth(1).unwrap_or("/").to_string();
                    answer(&path, &mut stream);
                });
            }
        });
        port
    }

    /// Keep the connection open and silent until the client closes it, or for [`HOLD`].
    fn hold(s: &mut std::net::TcpStream) {
        let _ = s.set_read_timeout(Some(HOLD));
        let mut sink = [0u8; 64];
        while matches!(s.read(&mut sink), Ok(n) if n > 0) {}
    }

    const PACED_CHUNKS: usize = 30;
    const PACED_CHUNK: usize = 1024;

    /// The measured case of LD-385, scaled down: a body that keeps arriving, 1 KiB every
    /// 50 ms, never pausing for anywhere near the stall window and far above the floor, but
    /// taking 1.5 s in all, longer than any time limit the fetcher is given. Before LD-385 a cap
    /// on the whole request failed it (`timeout: global`).
    #[test]
    fn a_slow_body_that_never_stalls_is_fetched_whole() {
        let port = serve(|_, s| {
            let length = PACED_CHUNKS * PACED_CHUNK;
            let head =
                format!("HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n");
            let _ = s.write_all(head.as_bytes());
            for _ in 0..PACED_CHUNKS {
                std::thread::sleep(ms(50));
                if s.write_all(&[b'x'; PACED_CHUNK]).is_err() {
                    return;
                }
            }
        });
        let f = fetcher_with(port, QUICK);
        let url = "https://mirror.test/paced";
        let whole = (PACED_CHUNKS * PACED_CHUNK) as u64;
        assert_eq!(f.get(url).map(|b| b.len() as u64), Ok(whole));
        let mut out = Vec::new();
        assert_eq!(f.download(url, &mut out, whole), Ok(whole));
        assert_eq!(out.len() as u64, whole);
    }

    /// A body that stops: 300 bytes and then silence. It fails once no byte has arrived for the
    /// stall window, saying how far it got — of how many bytes, when the response said.
    #[test]
    fn a_body_that_stops_arriving_fails_as_stalled() {
        let port = serve(|path, s| {
            let head = if path == "/sized" {
                "HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n"
            } else {
                "HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n"
            };
            let _ = s.write_all(head.as_bytes());
            let _ = s.write_all(&[b'x'; 300]);
            hold(s);
        });
        let f = fetcher_with(
            port,
            FetchTimeouts {
                stall: ms(300),
                ..QUICK
            },
        );
        for (path, got) in [("sized", "300 of 1000 bytes"), ("unsized", "300 bytes")] {
            let url = format!("https://mirror.test/{path}");
            let why = format!("stalled: no data for 0.3 s after {got}");
            let stalled = FetchError::Transport(url.clone(), why);
            assert_eq!(f.get(&url), Err(stalled.clone()));
            assert_eq!(f.download(&url, &mut Vec::new(), 1 << 20), Err(stalled));
        }
    }

    /// A server that takes the request and never answers fails at the header wait, which the
    /// message names, and not at the longer stall window. With a header wait longer than the
    /// window the window bounds that wait too, and the message says so.
    #[test]
    fn a_server_that_never_answers_fails_at_the_header_wait() {
        let port = serve(|_, s| hold(s));
        let url = "https://mirror.test/silent";
        let t = FetchTimeouts {
            headers: ms(300),
            stall: Duration::from_secs(5),
            ..QUICK
        };
        let f = fetcher_with(port, t);
        let started = std::time::Instant::now();
        let silent = FetchError::Transport(url.into(), "no response headers within 0.3 s".into());
        assert_eq!(f.get(url), Err(silent.clone()));
        assert!(started.elapsed() < t.stall, "{:?}", started.elapsed());
        assert_eq!(f.download(url, &mut Vec::new(), 10), Err(silent));

        let f = fetcher_with(
            port,
            FetchTimeouts {
                headers: Duration::from_secs(5),
                stall: ms(300),
                ..QUICK
            },
        );
        let why = "stalled: no data for 0.3 s while waiting for the response headers";
        assert_eq!(
            f.get(url),
            Err(FetchError::Transport(url.into(), why.into()))
        );
    }

    /// A body that is never silent for long but arrives below the floor — 8 bytes every 10 ms,
    /// 800 bytes a second against a floor of 100000 — fails as too slow once a whole window has
    /// passed.
    #[test]
    fn a_body_below_the_floor_fails_as_too_slow() {
        let port = serve(|_, s| {
            let _ = s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 1000000\r\nConnection: close\r\n\r\n",
            );
            let until = std::time::Instant::now() + HOLD;
            while std::time::Instant::now() < until {
                std::thread::sleep(ms(10));
                if s.write_all(&[b'x'; 8]).is_err() {
                    return;
                }
            }
        });
        let f = fetcher_with(
            port,
            FetchTimeouts {
                stall: ms(500),
                min_rate: 100_000,
                ..QUICK
            },
        );
        let url = "https://mirror.test/trickle";
        for got in [
            f.get(url).map(drop),
            f.download(url, &mut Vec::new(), 1 << 20).map(drop),
        ] {
            let Err(FetchError::Transport(at, why)) = got else {
                panic!("{got:?}")
            };
            assert_eq!(at, url);
            let prefix = "too slow: under 100000 bytes/s for 0.5 s after ";
            assert!(why.starts_with(prefix), "{why}");
            assert!(why.ends_with(" of 1000000 bytes"), "{why}");
        }
    }

    /// What `lodi` itself uses: no cap on a whole request or body, a stall window of a minute.
    #[test]
    fn the_default_limits_are_the_recorded_ones() {
        assert_eq!(
            FetchTimeouts::default(),
            FetchTimeouts {
                resolve: Duration::from_secs(30),
                connect: Duration::from_secs(30),
                send: Duration::from_secs(30),
                headers: Duration::from_secs(60),
                stall: Duration::from_secs(60),
                min_rate: 1024,
            }
        );
        assert_eq!(seconds(Duration::from_secs(60)), "60 s");
        assert_eq!(seconds(ms(300)), "0.3 s");
    }

    #[test]
    fn rewrite_applies_to_the_first_matching_prefix() {
        let f =
            HttpFetcher::new(parse_rewrites("https://x.test/a/=http://127.0.0.1:1/m/").unwrap());
        assert_eq!(f.target("https://x.test/a/b/c"), "http://127.0.0.1:1/m/b/c");
        assert_eq!(f.target("https://x.test/other"), "https://x.test/other");
    }

    // ------------------------------------------------------------ retries (LD-386) ---

    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    /// The real waiter's stand-in: it records every retry line and every wait and returns at
    /// once, so no test sleeps for a backoff. `random` is the jitter draw it hands out, and
    /// `on_wait` runs at each wait with the number of waits so far.
    struct Recorder {
        random: f64,
        notices: Mutex<Vec<String>>,
        waits: Mutex<Vec<Duration>>,
        on_wait: Box<dyn Fn(usize) + Send + Sync>,
    }

    impl Recorder {
        fn new() -> Arc<Recorder> {
            Recorder::with(0.0, |_| {})
        }

        fn with(random: f64, on_wait: impl Fn(usize) + Send + Sync + 'static) -> Arc<Recorder> {
            Arc::new(Recorder {
                random,
                notices: Mutex::new(Vec::new()),
                waits: Mutex::new(Vec::new()),
                on_wait: Box::new(on_wait),
            })
        }

        fn notices(&self) -> Vec<String> {
            self.notices.lock().unwrap().clone()
        }

        fn waits(&self) -> Vec<Duration> {
            self.waits.lock().unwrap().clone()
        }
    }

    impl Waiter for Recorder {
        fn notice(&self, line: &str) {
            self.notices.lock().unwrap().push(line.to_string());
        }

        fn wait(&self, d: Duration) {
            let n = {
                let mut waits = self.waits.lock().unwrap();
                waits.push(d);
                waits.len()
            };
            (self.on_wait)(n);
        }

        fn random(&self) -> f64 {
            self.random
        }
    }

    /// The fetcher `lodi` uses — the recorded limits and the recorded retry schedule — with the
    /// recorder in place of the clock, and `https://mirror.test/` rewritten to `port`.
    fn retrying(port: u16, timeouts: FetchTimeouts, waiter: &Arc<Recorder>) -> HttpFetcher {
        HttpFetcher::with_retry(
            parse_rewrites(&format!("https://mirror.test/=http://127.0.0.1:{port}/")).unwrap(),
            timeouts,
            Retry::default(),
            waiter.clone(),
        )
    }

    /// A loopback server that hands the number of each request (from 0, in the order they
    /// arrive) and its connection to `answer`, one thread per connection, and counts them.
    fn serve_counted(
        answer: impl Fn(usize, &mut std::net::TcpStream) + Send + Sync + 'static,
    ) -> (u16, Arc<AtomicUsize>) {
        use std::io::{BufRead, BufReader};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(AtomicUsize::new(0));
        let count = seen.clone();
        let answer = Arc::new(answer);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let (count, answer) = (count.clone(), answer.clone());
                std::thread::spawn(move || {
                    let mut reader = BufReader::new(stream.try_clone().unwrap());
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header).unwrap_or(0) == 0
                            || header.trim().is_empty()
                        {
                            break;
                        }
                    }
                    let n = count.fetch_add(1, Ordering::SeqCst);
                    answer(n, &mut stream);
                });
            }
        });
        (port, seen)
    }

    /// Write a whole response: the status line's code and reason, extra header lines, a body.
    fn reply(s: &mut std::net::TcpStream, status: &str, headers: &str, body: &[u8]) {
        let head = format!(
            "HTTP/1.1 {status}\r\n{headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = s.write_all(head.as_bytes());
        let _ = s.write_all(body);
    }

    fn reason(status: u16) -> &'static str {
        match status {
            200 => "OK",
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            404 => "Not Found",
            408 => "Request Timeout",
            410 => "Gone",
            429 => "Too Many Requests",
            500 => "Internal Server Error",
            501 => "Not Implemented",
            502 => "Bad Gateway",
            503 => "Service Unavailable",
            504 => "Gateway Timeout",
            _ => "HTTP Version Not Supported",
        }
    }

    fn status_line(status: u16) -> String {
        format!("{status} {}", reason(status))
    }

    /// The measured case of LD-386: a server that answers 503 for a while and then 200. The
    /// fetch is tried again after 2, 4 and 8 s — the recorder's jitter draw is 0 — says so once
    /// per retry, and returns the body the fourth attempt got; `download` alike.
    #[test]
    fn a_503_that_clears_is_tried_again_until_the_answer_comes() {
        for fetch in ["get", "download"] {
            let (port, seen) = serve_counted(|n, s| {
                if n < 3 {
                    reply(s, "503 Service Unavailable", "", b"");
                } else {
                    reply(s, "200 OK", "", b"ok");
                }
            });
            let waiter = Recorder::new();
            let f = retrying(port, QUICK, &waiter);
            let url = "https://mirror.test/dists/noble/Release";
            match fetch {
                "get" => assert_eq!(f.get(url), Ok(b"ok".to_vec())),
                _ => {
                    let mut out = Vec::new();
                    assert_eq!(f.download(url, &mut out, 10), Ok(2));
                    assert_eq!(out, b"ok");
                }
            }
            assert_eq!(seen.load(Ordering::SeqCst), 4, "{fetch}");
            assert_eq!(
                waiter.notices(),
                [
                    "mirror.test answered HTTP status 503; retrying in 2 s (attempt 2 of 10)",
                    "mirror.test answered HTTP status 503; retrying in 4 s (attempt 3 of 10)",
                    "mirror.test answered HTTP status 503; retrying in 8 s (attempt 4 of 10)",
                ],
                "{fetch}"
            );
            assert_eq!(waiter.waits(), [secs(2), secs(4), secs(8)], "{fetch}");
            // One fetch, one entry in the log, however many attempts it took.
            assert_eq!(f.requests(), [url], "{fetch}");
        }
    }

    /// 500, 502, 503 and 504 are the statuses that pass; each is tried again.
    #[test]
    fn every_server_error_that_passes_is_tried_again() {
        for status in [500u16, 502, 503, 504] {
            let (port, seen) = serve_counted(move |n, s| {
                if n == 0 {
                    reply(s, &status_line(status), "", b"");
                } else {
                    reply(s, "200 OK", "", b"ok");
                }
            });
            let waiter = Recorder::new();
            let f = retrying(port, QUICK, &waiter);
            assert_eq!(
                f.get("https://mirror.test/x"),
                Ok(b"ok".to_vec()),
                "{status}"
            );
            assert_eq!(seen.load(Ordering::SeqCst), 2, "{status}");
            assert_eq!(
                waiter.notices(),
                [format!(
                    "mirror.test answered HTTP status {status}; retrying in 2 s (attempt 2 of 10)"
                )]
            );
        }
    }

    /// An answer is not an accident: every 4xx — 404 above all, and 408 and 429 too — and a
    /// 5xx other than the four above is asked for once, with no retry line and no wait.
    #[test]
    fn an_answer_is_asked_for_once() {
        for status in [400u16, 401, 403, 404, 408, 410, 429, 501, 505] {
            let (port, seen) = serve_counted(move |_, s| {
                reply(s, &status_line(status), "Retry-After: 1\r\n", b"")
            });
            let waiter = Recorder::new();
            let f = retrying(port, QUICK, &waiter);
            let url = "https://mirror.test/x";
            assert_eq!(
                f.get(url),
                Err(FetchError::Status(url.into(), status)),
                "{status}"
            );
            assert_eq!(
                f.download(url, &mut Vec::new(), 10),
                Err(FetchError::Status(url.into(), status)),
                "{status}"
            );
            assert_eq!(
                seen.load(Ordering::SeqCst),
                2,
                "{status}: one request per fetch"
            );
            assert!(waiter.notices().is_empty(), "{status}");
            assert!(waiter.waits().is_empty(), "{status}");
        }
    }

    /// `docs/ERRORS.md`: a 403 or 429 from the GitHub API — Lodi asks once, and falls back to no
    /// other source. A `Retry-After` on the refusal changes nothing.
    #[test]
    fn a_github_api_refusal_is_asked_for_once() {
        for status in [403u16, 429] {
            let (port, seen) = serve_counted(move |_, s| {
                reply(s, &status_line(status), "Retry-After: 1\r\n", b"")
            });
            let waiter = Recorder::new();
            let f = HttpFetcher::with_retry(
                parse_rewrites(&format!("https://api.github.com/=http://127.0.0.1:{port}/"))
                    .unwrap(),
                QUICK,
                Retry::default(),
                waiter.clone(),
            );
            let url = "https://api.github.com/repos/o/r/releases?per_page=30";
            let headers = [("Accept", "application/vnd.github+json".to_string())];
            assert_eq!(
                f.get_with_headers(url, &headers),
                Err(FetchError::Status(url.into(), status))
            );
            assert_eq!(seen.load(Ordering::SeqCst), 1, "{status}");
            assert!(waiter.notices().is_empty(), "{status}");
        }
    }

    /// A server that stays down: ten attempts, nine waits of the recorded schedule adding up to
    /// the budget, and an error that is still the status — the caller's code does not change —
    /// saying how many attempts were made.
    #[test]
    fn a_503_that_never_clears_fails_after_the_budget_saying_how_many_attempts() {
        for fetch in ["get", "download"] {
            let (port, seen) = serve_counted(|_, s| reply(s, "503 Service Unavailable", "", b""));
            let waiter = Recorder::new();
            let f = retrying(port, QUICK, &waiter);
            let url = "https://mirror.test/down";
            let got = match fetch {
                "get" => f.get(url).map(drop),
                _ => f.download(url, &mut Vec::new(), 10).map(drop),
            };
            let exhausted =
                FetchError::Exhausted(10, Box::new(FetchError::Status(url.into(), 503)));
            assert_eq!(got, Err(exhausted.clone()), "{fetch}");
            assert_eq!(
                exhausted.to_string(),
                "https://mirror.test/down: HTTP status 503; gave up after 10 attempts"
            );
            assert_eq!(exhausted.status(), Some(503));
            assert_eq!(seen.load(Ordering::SeqCst), 10, "{fetch}");
            let waits = waiter.waits();
            assert_eq!(
                waits,
                [2, 4, 8, 16, 32, 60, 60, 60, 60].map(secs),
                "{fetch}"
            );
            assert_eq!(waits.iter().sum::<Duration>(), Retry::default().budget());
            let notices = waiter.notices();
            assert_eq!(notices.len(), 9, "{fetch}");
            assert_eq!(
                notices[8],
                "mirror.test answered HTTP status 503; retrying in 60 s (attempt 10 of 10)"
            );
        }
    }

    /// A loopback port kept for a server that starts later (LD-393). Its socket is bound from
    /// the start and never released, so the kernel gives the port to no other socket; it does
    /// not listen, so a connection to it is refused, exactly as when nothing is bound there,
    /// until [`LatePort::listen`] makes that same socket the server's. `std` binds a TCP socket
    /// only on the way to listening on it, hence `libc`; `SO_REUSEADDR` stays off, so that not
    /// even a socket that sets it can bind the port meanwhile.
    struct LatePort(std::net::TcpListener);

    impl LatePort {
        fn new() -> LatePort {
            use std::os::fd::{FromRawFd, OwnedFd};
            let loopback = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: 0,
                sin_addr: libc::in_addr {
                    s_addr: u32::from(std::net::Ipv4Addr::LOCALHOST).to_be(),
                },
                sin_zero: [0; 8],
            };
            let size = std::mem::size_of_val(&loopback) as libc::socklen_t;
            // SAFETY: `socket` returns a new descriptor that `OwnedFd` then owns alone, and `bind`
            // is given a whole `sockaddr_in` and its size.
            let socket = unsafe {
                let fd = libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
                assert!(fd >= 0, "socket: {}", std::io::Error::last_os_error());
                let socket = OwnedFd::from_raw_fd(fd);
                let bound = libc::bind(fd, (&raw const loopback).cast(), size);
                assert_eq!(bound, 0, "bind: {}", std::io::Error::last_os_error());
                socket
            };
            // Bound and not listening yet: `std`'s type for the socket, whose `listen` comes later.
            LatePort(std::net::TcpListener::from(socket))
        }

        fn port(&self) -> u16 {
            self.0.local_addr().unwrap().port()
        }

        /// Start listening on the kept port: from here a connection to it is accepted.
        fn listen(self) -> std::net::TcpListener {
            use std::os::fd::AsRawFd;
            // SAFETY: `listen` on the descriptor this value owns.
            let listening = unsafe { libc::listen(self.0.as_raw_fd(), 128) };
            assert_eq!(listening, 0, "listen: {}", std::io::Error::last_os_error());
            self.0
        }
    }

    /// LD-393: the port a late server listens on is never free before it does. Another socket
    /// asking for it meanwhile — what the kernel once did, handing it to a concurrent test's
    /// counting server, which then counted this module's refused request as its own — is
    /// refused; a connection meanwhile is refused as when nothing is bound there; and the late
    /// server then gets the port and the connection.
    #[test]
    fn a_port_kept_for_a_late_server_is_never_free_before_it_listens() {
        use std::io::ErrorKind;
        use std::net::{TcpListener, TcpStream};
        let late = LatePort::new();
        let port = late.port();
        let other = TcpListener::bind(("127.0.0.1", port))
            .map(drop)
            .map_err(|e| e.kind());
        assert_eq!(
            other,
            Err(ErrorKind::AddrInUse),
            "another socket was given the kept port {port}"
        );
        let early = TcpStream::connect(("127.0.0.1", port))
            .map(drop)
            .map_err(|e| e.kind());
        assert_eq!(early, Err(ErrorKind::ConnectionRefused));
        let listener = late.listen();
        assert_eq!(listener.local_addr().unwrap().port(), port);
        let client = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let (_, from) = listener.accept().unwrap();
        assert_eq!(from, client.local_addr().unwrap());
    }

    /// A connection refused — the server not listening yet — is tried again, and the fetch
    /// succeeds once the server is up (it starts during the first wait).
    #[test]
    fn a_refused_connection_is_tried_again_until_the_server_listens() {
        let late = LatePort::new();
        let port = late.port();
        let late = Mutex::new(Some(late));
        let waiter = Recorder::with(0.0, move |_| {
            if let Some(late) = late.lock().unwrap().take() {
                let listener = late.listen();
                std::thread::spawn(move || {
                    for stream in listener.incoming() {
                        let Ok(mut s) = stream else { continue };
                        let mut request = [0u8; 1024];
                        let _ = s.read(&mut request);
                        reply(&mut s, "200 OK", "", b"ok");
                    }
                });
            }
        });
        let f = retrying(port, QUICK, &waiter);
        assert_eq!(f.get("https://mirror.test/late"), Ok(b"ok".to_vec()));
        let notices = waiter.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(notices[0].starts_with("mirror.test: "), "{}", notices[0]);
        assert!(notices[0].contains("Connection refused"), "{}", notices[0]);
        assert!(
            notices[0].ends_with("; retrying in 2 s (attempt 2 of 10)"),
            "{}",
            notices[0]
        );
    }

    /// A body that stalls half-way is fetched again from its first byte: what the failed attempt
    /// wrote is gone, for `get` and for `download` alike, and the result is the second body
    /// whole — never the first attempt's 300 bytes followed by anything.
    #[test]
    fn a_body_that_stalls_is_fetched_again_from_its_first_byte() {
        for fetch in ["get", "download"] {
            let (port, seen) = serve_counted(|n, s| {
                let _ = s.write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\nConnection: close\r\n\r\n",
                );
                if n == 0 {
                    let _ = s.write_all(&[b'a'; 300]);
                    hold(s);
                } else {
                    let _ = s.write_all(&[b'b'; 1000]);
                }
            });
            let waiter = Recorder::new();
            let f = retrying(
                port,
                FetchTimeouts {
                    stall: ms(300),
                    ..QUICK
                },
                &waiter,
            );
            let url = "https://mirror.test/big";
            let body = match fetch {
                "get" => f.get(url).unwrap(),
                _ => {
                    let mut out = Vec::new();
                    assert_eq!(f.download(url, &mut out, 1 << 20), Ok(1000));
                    out
                }
            };
            assert_eq!(body, [b'b'; 1000], "{fetch}");
            assert_eq!(seen.load(Ordering::SeqCst), 2, "{fetch}");
            assert_eq!(
                waiter.notices(),
                [
                    "mirror.test: stalled: no data for 0.3 s after 300 of 1000 bytes; \
                  retrying in 2 s (attempt 2 of 10)"
                ],
                "{fetch}"
            );
        }
    }

    /// Refusals are final: a body over its limit, a redirect to plain HTTP and a sixth redirect
    /// are each one attempt, with no retry line.
    #[test]
    fn a_refusal_is_not_tried_again() {
        let waiter = Recorder::new();
        let (port, seen) = serve_counted(|_, s| reply(s, "200 OK", "", &[b'x'; 100]));
        let url = "https://mirror.test/large";
        assert_eq!(
            retrying(port, QUICK, &waiter).download(url, &mut Vec::new(), 10),
            Err(FetchError::Transport(
                url.into(),
                "body larger than 10 bytes".into()
            ))
        );
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        let to = "http://203.0.113.1/x";
        let (port, seen) =
            serve_counted(move |_, s| reply(s, "302 Found", &format!("Location: {to}\r\n"), b""));
        let url = "https://mirror.test/away";
        assert_eq!(
            retrying(port, QUICK, &waiter).get(url),
            Err(FetchError::InsecureRedirect(url.into(), to.into()))
        );
        assert_eq!(seen.load(Ordering::SeqCst), 1);

        let (port, seen) = serve_counted(|_, s| reply(s, "302 Found", "Location: /loop\r\n", b""));
        let url = "https://mirror.test/loop";
        assert_eq!(
            retrying(port, QUICK, &waiter).get(url),
            Err(FetchError::Transport(
                url.into(),
                "more than 5 redirects".into()
            ))
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            6,
            "one attempt: the request and five hops"
        );
        assert!(waiter.notices().is_empty(), "{:?}", waiter.notices());
    }

    /// A 503's `Retry-After` lengthens a wait up to the longest wait of the schedule and never
    /// shortens one; another status's is ignored.
    #[test]
    fn a_503_retry_after_is_honoured_up_to_the_longest_wait() {
        let (port, _) = serve_counted(|n, s| match n {
            0 => reply(s, "503 Service Unavailable", "Retry-After: 30\r\n", b""),
            1 => reply(s, "503 Service Unavailable", "Retry-After: 600\r\n", b""),
            2 => reply(s, "503 Service Unavailable", "Retry-After: 1\r\n", b""),
            3 => reply(s, "502 Bad Gateway", "Retry-After: 30\r\n", b""),
            _ => reply(s, "200 OK", "", b"ok"),
        });
        let waiter = Recorder::new();
        let f = retrying(port, QUICK, &waiter);
        assert_eq!(f.get("https://mirror.test/busy"), Ok(b"ok".to_vec()));
        assert_eq!(waiter.waits(), [secs(30), secs(60), secs(8), secs(16)]);
        assert_eq!(
            waiter.notices()[0],
            "mirror.test answered HTTP status 503; retrying in 30 s (attempt 2 of 10)"
        );
    }

    /// However long each `Retry-After` asks for, the waits of one fetch never add up to more than
    /// the budget: the retry that would pass it is not made.
    #[test]
    fn a_retry_after_never_carries_the_waits_past_the_budget() {
        let (port, seen) =
            serve_counted(|_, s| reply(s, "503 Service Unavailable", "Retry-After: 60\r\n", b""));
        let waiter = Recorder::new();
        let url = "https://mirror.test/busy";
        assert_eq!(
            retrying(port, QUICK, &waiter).get(url),
            Err(FetchError::Exhausted(
                6,
                Box::new(FetchError::Status(url.into(), 503))
            ))
        );
        assert_eq!(seen.load(Ordering::SeqCst), 6);
        assert_eq!(waiter.waits(), [secs(60); 5]);
    }

    /// The schedule `lodi` uses: ten attempts, waits from 2 s doubling up to 60 s, 302 s in all,
    /// each drawn down by at most a fifth.
    #[test]
    fn the_default_retry_is_the_recorded_one() {
        let r = Retry::default();
        assert_eq!(
            r,
            Retry {
                attempts: 10,
                first: secs(2),
                max_wait: secs(60),
                jitter_percent: 20,
            }
        );
        let base: Vec<Duration> = (1..r.attempts).map(|n| r.base_wait(n)).collect();
        assert_eq!(base, [2, 4, 8, 16, 32, 60, 60, 60, 60].map(secs));
        assert_eq!(r.budget(), secs(302));
        assert_eq!(r.drawn(3, 0.0), secs(8));
        assert_eq!(r.drawn(3, 0.5), ms(7200));
        assert!(r.drawn(3, 0.999_999) >= ms(6400));
        assert_eq!(wait_text(ms(7200)), "7 s");
        assert_eq!(wait_text(ms(1600)), "2 s");
        assert_eq!(wait_text(ms(250)), "0.25 s");
        // A drawn wait never exceeds its base, so the drawn waits never pass the budget either.
        let most: Duration = (1..r.attempts).map(|n| r.drawn(n, 0.0)).sum();
        assert_eq!(most, r.budget());
        assert_eq!(Retry::ONCE.attempts, 1);
    }

    /// `LODI_FETCH_ATTEMPTS` is a whole number from 1 to 10 and nothing else; fewer attempts
    /// shorten the budget with the schedule.
    #[test]
    fn the_attempts_setting_is_a_whole_number_from_one_to_ten() {
        for (text, n) in [("1", 1), ("3", 3), ("10", 10), ("03", 3)] {
            assert_eq!(parse_attempts(text), Ok(n), "{text:?}");
        }
        for text in [
            "0",
            "11",
            "4294967296",
            "",
            " 2",
            "2 ",
            "+2",
            "-1",
            "two",
            "2.0",
        ] {
            let e = parse_attempts(text).expect_err(text);
            assert!(
                e.contains("from 1 to 10") && e.contains(&format!("{text:?}")),
                "{e}"
            );
        }
        let three = Retry {
            attempts: 3,
            ..Retry::default()
        };
        assert_eq!(three.budget(), secs(6));
        assert_eq!(Retry::ONCE.budget(), Duration::ZERO);
    }

    /// A 3-attempt schedule's jitter reaches the wait it draws: a draw of 0.5 takes a tenth off.
    #[test]
    fn the_jitter_draw_shortens_the_wait() {
        let (port, _) = serve_counted(|n, s| {
            if n == 0 {
                reply(s, "503 Service Unavailable", "", b"");
            } else {
                reply(s, "200 OK", "", b"ok");
            }
        });
        let waiter = Recorder::with(0.5, |_| {});
        assert_eq!(
            retrying(port, QUICK, &waiter).get("https://mirror.test/x"),
            Ok(b"ok".to_vec())
        );
        assert_eq!(waiter.waits(), [ms(1800)]);
    }

    /// `Retry-After` is a number of seconds or an HTTP-date (RFC 9110 §10.2.3); a date in the
    /// past is no wait, and any other form is ignored.
    #[test]
    fn retry_after_is_seconds_or_an_http_date() {
        let now = crate::util::parse_utc("1994-11-06T08:49:07Z").unwrap();
        assert_eq!(retry_after("120", now), Some(secs(120)));
        assert_eq!(retry_after(" 0 ", now), Some(secs(0)));
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:49:37 GMT", now),
            Some(secs(30))
        );
        assert_eq!(
            retry_after("Sun, 06 Nov 1994 08:48:37 GMT", now),
            Some(secs(0))
        );
        for other in [
            "soon",
            "-5",
            "Sunday, 06-Nov-94 08:49:37 GMT",
            "Sun Nov  6 08:49:37 1994",
            "Sun, 31 Feb 1994 08:49:37 GMT",
        ] {
            assert_eq!(retry_after(other, now), None, "{other}");
        }
    }

    /// Which failures pass: every step's timeout, a connection refused, reset, aborted or cut
    /// off, an unreachable network, and a name lookup the resolver says to try again — in
    /// glibc's words and in musl's. A name that does not exist, a certificate and a malformed
    /// response are answers.
    #[test]
    fn only_a_failure_that_passes_is_transient() {
        use std::io::{Error, ErrorKind};
        for kind in [
            ErrorKind::ConnectionRefused,
            ErrorKind::ConnectionReset,
            ErrorKind::ConnectionAborted,
            ErrorKind::BrokenPipe,
            ErrorKind::TimedOut,
            ErrorKind::UnexpectedEof,
            ErrorKind::NetworkUnreachable,
            ErrorKind::HostUnreachable,
        ] {
            assert!(transient_io(&Error::from(kind)), "{kind:?}");
        }
        for kind in [
            ErrorKind::PermissionDenied,
            ErrorKind::InvalidData,
            ErrorKind::NotFound,
        ] {
            assert!(!transient_io(&Error::from(kind)), "{kind:?}");
        }
        let lookup =
            |detail: &str| Error::other(format!("failed to lookup address information: {detail}"));
        assert!(transient_io(&lookup(
            "Temporary failure in name resolution"
        )));
        assert!(transient_io(&lookup("Try again")));
        assert!(!transient_io(&lookup("Name or service not known")));
        assert!(!transient_io(&lookup("Name does not resolve")));
        assert!(transient(&ureq::Error::Timeout(ureq::Timeout::Connect)));
        assert!(transient(&ureq::Error::Timeout(ureq::Timeout::Resolve)));
        assert!(transient(&ureq::Error::Io(Error::from(
            ErrorKind::ConnectionReset
        ))));
        assert!(transient_io(&Error::other(ureq::Error::Timeout(
            ureq::Timeout::RecvBody
        ))));
        assert!(!transient(&ureq::Error::HostNotFound));
        assert!(!transient(&ureq::Error::Tls("refused")));
    }
}
