//! Bounded in-memory pack v2 reader. No local object database: all delta bases are in this pack.
use super::{GitError, Limits, fail};
use flate2::bufread::ZlibDecoder;
use sha1collisiondetection::Sha1CD;
use std::collections::{BTreeMap, VecDeque};
use std::io::Read;
#[cfg(test)]
#[path = "pack_tests.rs"]
mod tests;

#[derive(Clone)]
pub struct Object {
    pub kind: u8,
    pub data: Vec<u8>,
}
struct Entry {
    offset: usize,
    base: Base,
    data: Vec<u8>,
}
enum Base {
    Whole(u8),
    Offset(usize),
    Id(String),
}

pub fn id(kind: u8, data: &[u8]) -> Result<String, GitError> {
    let label = match kind {
        1 => "commit",
        2 => "tree",
        3 => "blob",
        4 => "tag",
        _ => return Err(fail("E_HASH_MISMATCH", "unknown object kind")),
    };
    let mut hash = Sha1CD::default();
    hash.update(format!("{label} {}\0", data.len()));
    hash.update(data);
    let digest = hash
        .finalize_cd()
        .map_err(|_| fail("E_HASH_MISMATCH", "SHA-1DC detected a collision attack"))?;
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}
fn integer(data: &[u8], pos: &mut usize) -> Result<usize, GitError> {
    let mut n = 0usize;
    let mut shift = 0;
    loop {
        let b = *data
            .get(*pos)
            .ok_or_else(|| fail("E_HASH_MISMATCH", "truncated delta integer"))?;
        *pos += 1;
        if shift > 56 {
            return Err(fail("E_HASH_MISMATCH", "delta integer overflow"));
        }
        n |= usize::from(b & 0x7f) << shift;
        if b & 0x80 == 0 {
            return Ok(n);
        }
        shift += 7;
    }
}
fn apply(base: &[u8], delta: &[u8], limit: usize) -> Result<Vec<u8>, GitError> {
    let mut pos = 0;
    let source = integer(delta, &mut pos)?;
    let target = integer(delta, &mut pos)?;
    if source != base.len() || target > limit {
        return Err(fail(
            "E_HASH_MISMATCH",
            "delta size mismatch or exceeds cap",
        ));
    }
    let mut result = Vec::with_capacity(target);
    while pos < delta.len() {
        let cmd = delta[pos];
        pos += 1;
        if cmd & 0x80 != 0 {
            let mut at = 0usize;
            let mut len = 0usize;
            for i in 0..4 {
                if cmd & (1 << i) != 0 {
                    at |= usize::from(
                        *delta
                            .get(pos)
                            .ok_or_else(|| fail("E_HASH_MISMATCH", "truncated delta copy"))?,
                    ) << (8 * i);
                    pos += 1;
                }
            }
            for i in 0..3 {
                if cmd & (1 << (i + 4)) != 0 {
                    len |=
                        usize::from(*delta.get(pos).ok_or_else(|| {
                            fail("E_HASH_MISMATCH", "truncated delta copy length")
                        })?) << (8 * i);
                    pos += 1;
                }
            }
            if len == 0 {
                len = 0x10000;
            }
            let end = at
                .checked_add(len)
                .ok_or_else(|| fail("E_HASH_MISMATCH", "delta copy overflow"))?;
            if end > base.len() || result.len().saturating_add(len) > target {
                return Err(fail("E_HASH_MISMATCH", "delta copy outside base"));
            }
            result.extend_from_slice(&base[at..end]);
        } else if cmd != 0 {
            let len = usize::from(cmd);
            if pos.saturating_add(len) > delta.len() || result.len().saturating_add(len) > target {
                return Err(fail("E_HASH_MISMATCH", "delta insert outside bounds"));
            }
            result.extend_from_slice(&delta[pos..pos + len]);
            pos += len;
        } else {
            return Err(fail("E_HASH_MISMATCH", "zero delta opcode"));
        }
    }
    if result.len() != target {
        return Err(fail("E_HASH_MISMATCH", "delta result size mismatch"));
    }
    Ok(result)
}

pub fn read(bytes: &[u8], limits: &Limits) -> Result<BTreeMap<String, Object>, GitError> {
    let mismatch = |why| fail("E_HASH_MISMATCH", why);
    if bytes.len() < 32 || &bytes[..4] != b"PACK" || bytes[4..8] != 2u32.to_be_bytes() {
        return Err(mismatch("not a packfile v2"));
    }
    let count = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    if count > limits.max_objects {
        return Err(mismatch("pack object count exceeds cap"));
    }
    let trailer = bytes.len() - 20;
    let mut h = Sha1CD::default();
    h.update(&bytes[..trailer]);
    if h.finalize_cd()
        .map_err(|_| mismatch("SHA-1DC collision attack in pack"))?
        .as_slice()
        != &bytes[trailer..]
    {
        return Err(mismatch("pack checksum mismatch"));
    }
    let mut pos = 12;
    let mut entries = Vec::new();
    let mut total_size = 0usize;
    for _ in 0..count {
        let offset = pos;
        let first = *bytes
            .get(pos)
            .filter(|_| pos < trailer)
            .ok_or_else(|| mismatch("truncated pack header"))?;
        pos += 1;
        let kind = (first >> 4) & 7;
        let mut size = usize::from(first & 15);
        let mut shift = 4;
        let mut b = first;
        while b & 0x80 != 0 {
            b = *bytes
                .get(pos)
                .filter(|_| pos < trailer)
                .ok_or_else(|| mismatch("truncated object size"))?;
            pos += 1;
            if shift > 56 {
                return Err(mismatch("object size overflow"));
            }
            size |= usize::from(b & 127) << shift;
            shift += 7;
        }
        total_size = total_size
            .checked_add(size)
            .ok_or_else(|| mismatch("pack size overflow"))?;
        if size > limits.max_object_bytes || total_size > limits.max_tree_bytes.saturating_mul(2) {
            return Err(mismatch("pack object or total size exceeds cap"));
        }
        let base = match kind {
            1..=4 => Base::Whole(kind),
            6 => {
                let mut b = *bytes
                    .get(pos)
                    .filter(|_| pos < trailer)
                    .ok_or_else(|| mismatch("truncated offset delta"))?;
                pos += 1;
                let mut distance = usize::from(b & 127);
                while b & 128 != 0 {
                    b = *bytes
                        .get(pos)
                        .filter(|_| pos < trailer)
                        .ok_or_else(|| mismatch("truncated offset delta"))?;
                    pos += 1;
                    distance = distance
                        .checked_add(1)
                        .and_then(|v| v.checked_mul(128))
                        .and_then(|v| v.checked_add(usize::from(b & 127)))
                        .ok_or_else(|| mismatch("offset delta overflow"))?;
                }
                Base::Offset(
                    offset
                        .checked_sub(distance)
                        .ok_or_else(|| mismatch("offset delta outside pack"))?,
                )
            }
            7 => {
                if pos + 20 > trailer {
                    return Err(mismatch("truncated reference delta"));
                }
                let id: String = bytes[pos..pos + 20]
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                pos += 20;
                Base::Id(id)
            }
            _ => return Err(mismatch("unknown pack object kind")),
        };
        let mut decoder = ZlibDecoder::new(&bytes[pos..trailer]);
        let mut data = Vec::new();
        decoder
            .by_ref()
            .take((limits.max_object_bytes as u64) + 1)
            .read_to_end(&mut data)
            .map_err(|_| mismatch("invalid zlib stream"))?;
        if data.len() > limits.max_object_bytes {
            return Err(mismatch("inflated object exceeds cap"));
        }
        let consumed = decoder.total_in() as usize;
        if consumed == 0 || pos.saturating_add(consumed) > trailer {
            return Err(mismatch("invalid zlib length"));
        }
        pos += consumed;
        if data.len() != size {
            return Err(mismatch("object size mismatch"));
        }
        entries.push(Entry { offset, base, data });
    }
    if pos != trailer {
        return Err(mismatch("extra bytes in pack"));
    }
    // Index once, then wake only dependents of each newly verified object. REF bases
    // may occur later in the pack; OFS bases must name an earlier entry boundary.
    let offsets: BTreeMap<usize, usize> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| (entry.offset, index))
        .collect();
    let mut by_offset: Vec<Vec<usize>> = vec![Vec::new(); count];
    let mut by_id: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut ready = VecDeque::new();
    for (index, entry) in entries.iter().enumerate() {
        match &entry.base {
            Base::Whole(_) => ready.push_back(index),
            Base::Offset(offset) => {
                let base_index = offsets
                    .get(offset)
                    .ok_or_else(|| mismatch("thin pack or unresolved offset delta base"))?;
                if *base_index >= index {
                    return Err(mismatch("offset delta base is not earlier in pack"));
                }
                by_offset[*base_index].push(index);
            }
            Base::Id(key) => by_id.entry(key.clone()).or_default().push(index),
        }
    }
    // Each object is held once, in `objects`; `resolved` names an entry's object. Resolved
    // bytes are capped like the header total, before `apply` allocates a result.
    let mut objects: BTreeMap<String, Object> = BTreeMap::new();
    let mut resolved: Vec<Option<String>> = vec![None; count];
    let cap = limits.max_tree_bytes.saturating_mul(2);
    let mut resolved_size = 0usize;
    let mut done = 0;
    while let Some(index) = ready.pop_front() {
        let entry = &mut entries[index];
        let limit = limits.max_object_bytes.min(cap - resolved_size);
        let obj = match &entry.base {
            Base::Whole(kind) => {
                if entry.data.len() > limit {
                    return Err(mismatch("resolved objects exceed cap"));
                }
                Object {
                    kind: *kind,
                    data: std::mem::take(&mut entry.data),
                }
            }
            Base::Offset(offset) => {
                let base = resolved[offsets[offset]]
                    .as_ref()
                    .map(|key| &objects[key])
                    .ok_or_else(|| mismatch("unresolved offset base"))?;
                Object {
                    kind: base.kind,
                    data: apply(&base.data, &entry.data, limit)?,
                }
            }
            Base::Id(key) => {
                let base = objects
                    .get(key)
                    .ok_or_else(|| mismatch("unresolved reference base"))?;
                Object {
                    kind: base.kind,
                    data: apply(&base.data, &entry.data, limit)?,
                }
            }
        };
        resolved_size += obj.data.len();
        entry.data = Vec::new();
        let key = id(obj.kind, &obj.data)?;
        if objects.insert(key.clone(), obj).is_some() {
            return Err(mismatch("duplicate object in pack"));
        }
        resolved[index] = Some(key.clone());
        done += 1;
        ready.extend(by_offset[index].drain(..));
        if let Some(dependents) = by_id.remove(&key) {
            ready.extend(dependents);
        }
    }
    if done != count {
        return Err(mismatch(
            "thin pack or unresolved delta base (no local object store)",
        ));
    }
    Ok(objects)
}
