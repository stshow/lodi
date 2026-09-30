//! Bounded pkt-line v2 framing and ls-refs / fetch responses.
use super::{GitError, Refs, fail};

pub const MAX_PKT: usize = 65520;

#[derive(Debug, PartialEq, Eq)]
pub enum Packet<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

pub struct Packets<'a> {
    bytes: &'a [u8],
    pub offset: usize,
}
impl<'a> Packets<'a> {
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, offset: 0 }
    }
    pub fn next(&mut self) -> Result<Option<Packet<'a>>, GitError> {
        if self.offset == self.bytes.len() {
            return Ok(None);
        }
        let rest = &self.bytes[self.offset..];
        if rest.len() < 4 {
            return Err(fail("E_FETCH", "truncated pkt-line length"));
        }
        let len = std::str::from_utf8(&rest[..4])
            .ok()
            .and_then(|v| usize::from_str_radix(v, 16).ok())
            .ok_or_else(|| fail("E_FETCH", "invalid pkt-line length"))?;
        let (packet, used) = match len {
            0 => (Packet::Flush, 4),
            1 => (Packet::Delim, 4),
            2 => (Packet::End, 4),
            3 => return Err(fail("E_FETCH", "invalid pkt-line length 3")),
            4..=MAX_PKT if len <= rest.len() => (Packet::Data(&rest[4..len]), len),
            _ => {
                return Err(fail(
                    "E_FETCH",
                    "pkt-line length exceeds bounds or response",
                ));
            }
        };
        self.offset += used;
        Ok(Some(packet))
    }
}

pub fn pkt(data: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(format!("{:04x}", data.len() + 4).as_bytes());
    out.extend_from_slice(data);
}

pub fn advertise(body: &[u8]) -> Result<(), GitError> {
    let mut reader = Packets::new(body);
    let mut v2 = false;
    let mut ls = false;
    let mut shallow = false;
    let mut ended = false;
    while let Some(p) = reader.next()? {
        ended = matches!(p, Packet::Flush | Packet::End);
        if let Packet::Data(data) = p {
            let data = data.strip_suffix(b"\n").unwrap_or(data);
            if data == b"version 2" {
                v2 = true;
            }
            if data.starts_with(b"ls-refs") {
                ls = true;
            }
            if data
                .strip_prefix(b"fetch=")
                .is_some_and(|rest| rest.split(|&b| b == b' ').any(|p| p == b"shallow"))
            {
                shallow = true;
            }
            if data.starts_with(b"object-format=") && data != b"object-format=sha1" {
                return Err(fail(
                    "E_FETCH",
                    format!("{} repository is unsupported", printable(data)),
                ));
            }
            if data.starts_with(b"ERR ") {
                return Err(server_error(&data[4..]));
            }
        }
    }
    if !ended {
        return Err(fail(
            "E_FETCH",
            "protocol version 2 advertisement missing terminal flush",
        ));
    }
    if !v2 || !ls || !shallow {
        return Err(fail(
            "E_FETCH",
            "protocol version 2 requires ls-refs and fetch with shallow",
        ));
    }
    Ok(())
}

pub fn printable(bytes: &[u8]) -> String {
    bytes
        .iter()
        .take(512)
        .map(|b| {
            if (b' '..=b'~').contains(b) {
                *b as char
            } else {
                '?'
            }
        })
        .collect()
}
pub fn server_error(bytes: &[u8]) -> GitError {
    fail("E_FETCH", format!("git server: {}", printable(bytes)))
}

pub fn refs(body: &[u8]) -> Result<Refs, GitError> {
    let mut r = Packets::new(body);
    let mut names = Vec::new();
    let mut head = None;
    let mut ended = false;
    while let Some(packet) = r.next()? {
        match packet {
            Packet::Flush | Packet::End => {
                ended = true;
                break;
            }
            Packet::Delim => return Err(fail("E_FETCH", "unexpected delimiter in ls-refs")),
            Packet::Data(bytes) => {
                if let Some(err) = bytes.strip_prefix(b"ERR ") {
                    return Err(server_error(err));
                }
                let text =
                    std::str::from_utf8(bytes).map_err(|_| fail("E_FETCH", "non-UTF-8 ls-refs"))?;
                let mut words = text.trim_end_matches('\n').split_whitespace();
                let id = words
                    .next()
                    .ok_or_else(|| fail("E_FETCH", "empty ls-refs entry"))?;
                let name = words
                    .next()
                    .ok_or_else(|| fail("E_FETCH", "missing ls-refs name"))?;
                if name.len() > 255 || !name.bytes().all(|b| (b'!'..=b'~').contains(&b)) {
                    return Err(fail("E_FETCH", "invalid ls-refs name"));
                }
                if id.len() != 40 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err(fail("E_FETCH", "invalid ls-refs object id"));
                }
                if name == "HEAD" {
                    head = words
                        .find_map(|p| p.strip_prefix("symref-target:"))
                        .map(str::to_owned);
                }
                names.push((name.to_owned(), id.to_owned()));
                if let Some(peeled) = words.find_map(|p| p.strip_prefix("peeled:")) {
                    if peeled.len() != 40 || !peeled.bytes().all(|b| b.is_ascii_hexdigit()) {
                        return Err(fail("E_FETCH", "invalid peeled object id"));
                    }
                    names.push((format!("{name}^{{}}"), peeled.to_owned()));
                }
            }
        }
    }
    if !ended || r.offset != body.len() {
        return Err(fail(
            "E_FETCH",
            "ls-refs missing terminal flush or trailing bytes",
        ));
    }
    Ok(Refs { head, names })
}

pub fn pack(body: &[u8]) -> Result<Vec<u8>, GitError> {
    let mut r = Packets::new(body);
    let mut section = "";
    let mut out = Vec::new();
    let mut seen_pack = false;
    let mut ended = false;
    while let Some(packet) = r.next()? {
        match packet {
            Packet::Delim => {
                section = "";
            }
            Packet::End | Packet::Flush => {
                ended = true;
                if r.offset != body.len() {
                    return Err(fail("E_FETCH", "trailing fetch bytes"));
                }
                break;
            }
            Packet::Data(data) if data.starts_with(b"ERR ") => {
                return Err(server_error(&data[4..]));
            }
            Packet::Data(data) if section.is_empty() => {
                section = match data {
                    b"shallow-info\n" => "shallow",
                    b"packfile\n" => "pack",
                    b"acknowledgments\n" => "acks",
                    _ => return Err(fail("E_FETCH", "unknown fetch response section")),
                };
                if section == "pack" {
                    seen_pack = true;
                }
            }
            Packet::Data(data) if section == "pack" => {
                match data.first() {
                    Some(1) => {
                        if out.len().saturating_add(data.len()) > 160 * 1024 * 1024 {
                            return Err(fail("E_FETCH", "pack body exceeds 160 MiB"));
                        }
                        out.extend_from_slice(&data[1..]);
                    }
                    Some(2) => {} // Progress must not appear in diagnostics.
                    Some(3) => return Err(server_error(&data[1..])),
                    _ => return Err(fail("E_FETCH", "invalid pack side-band channel")),
                }
            }
            Packet::Data(_) => {}
        }
    }
    if !ended {
        return Err(fail(
            "E_FETCH",
            "fetch response missing terminal flush or response-end",
        ));
    }
    if !seen_pack || out.is_empty() {
        return Err(fail("E_FETCH", "no packfile in fetch response"));
    }
    Ok(out)
}
