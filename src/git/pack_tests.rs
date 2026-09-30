//! G5: offset and reference deltas built from the recorded http-backend blob bytes.
use super::*;
use flate2::{Compression, write::ZlibEncoder};
use std::io::Write;

fn zlib(input: &[u8]) -> Vec<u8> {
    let mut e = ZlibEncoder::new(Vec::new(), Compression::fast());
    e.write_all(input).unwrap();
    e.finish().unwrap()
}
fn recorded_blob() -> Vec<u8> {
    let response = include_bytes!("../../tests/fixtures/git/tip/fetch-response.bin");
    let original = read(
        &crate::git::protocol::pack(response).unwrap(),
        &Limits::at("unused".into()),
    )
    .unwrap();
    original
        .values()
        .find(|obj| obj.kind == 3 && obj.data == b"two\n")
        .unwrap()
        .data
        .clone()
}
#[test]
fn real_repacked_offset_and_reference_delta_headers_are_data_sizes() {
    // The fixture was made by git repack and git pack-objects from five revisions.
    // Both delta formats carry an inflated *instruction* size in the pack entry header,
    // not the reconstructed blob size (242940 bytes in this example).
    for bytes in [
        &include_bytes!("../../tests/fixtures/git/repacked/offset.pack")[..],
        &include_bytes!("../../tests/fixtures/git/repacked/reference.pack")[..],
    ] {
        let objects = read(bytes, &Limits::at("unused".into())).unwrap();
        for (key, size) in [
            ("40e15bf57d0cfcf86fee096bac643686683502e3", 242940),
            ("406e5d936028aff9417fdc2ccae52c5f63ed22da", 242880),
            ("1fe93c3c4437a47ff19be156979296e0ee7e7207", 242820),
            ("6c81c0c57e64aa1b7a3e1acf70df4fca6dd90a05", 242760),
            ("a078abc1c9a7720f71b6cc995d72f2e6527531e5", 242700),
        ] {
            assert_eq!(objects[key].kind, 3);
            assert_eq!(objects[key].data.len(), size);
        }
    }
}
fn fixture_pack(change: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let base = recorded_blob();
    let mut output = Vec::from(&b"PACK\x00\x00\x00\x02\x00\x00\x00\x03"[..]);
    let base_offset = output.len();
    output.push(0x30 | (base.len() as u8));
    output.extend(zlib(&base));
    let delta_offset = output.len();
    let distance = delta_offset - base_offset;
    assert!(distance < 128);
    let mut delta = vec![base.len() as u8, base.len() as u8, 0x90, 3, 1, b'!'];
    output.push(0x60 | (delta.len() as u8));
    output.push(distance as u8);
    output.extend(zlib(&delta));
    delta[5] = b'?';
    output.push(0x70 | (delta.len() as u8));
    let key = id(3, &base).unwrap();
    for i in (0..40).step_by(2) {
        output.push(u8::from_str_radix(&key[i..i + 2], 16).unwrap());
    }
    output.extend(zlib(&delta));
    change(&mut output);
    let mut hash = Sha1CD::default();
    hash.update(&output);
    output.extend_from_slice(&hash.finalize_cd().unwrap());
    output
}
#[test]
fn reference_dependencies_can_precede_their_bases() {
    let mut bytes = b"PACK\0\0\0\x02\0\0\0\x03".to_vec();
    let base = recorded_blob();
    let child = b"two!";
    for (key, data) in [
        (id(3, child).unwrap(), b"?".as_slice()),
        (id(3, &base).unwrap(), b"!".as_slice()),
    ] {
        let delta = [4, 4, 0x90, 3, 1, data[0]];
        bytes.push(0x76); // REF_DELTA with six inflated instruction bytes
        for i in (0..40).step_by(2) {
            bytes.push(u8::from_str_radix(&key[i..i + 2], 16).unwrap());
        }
        bytes.extend(zlib(&delta));
    }
    bytes.push(0x30 | (base.len() as u8));
    bytes.extend(zlib(&base));
    let mut hash = Sha1CD::default();
    hash.update(&bytes);
    bytes.extend_from_slice(&hash.finalize_cd().unwrap());
    let objects = read(&bytes, &Limits::at("unused".into())).unwrap();
    assert_eq!(objects[&id(3, b"two?").unwrap()].data, b"two?");
    assert_eq!(objects.len(), 3);
}

#[test]
fn offset_base_must_point_to_an_entry_boundary() {
    let bytes = fixture_pack(|bytes| {
        let distance_at = 12 + 1 + zlib(&recorded_blob()).len() + 1;
        bytes[distance_at] -= 1; // valid distance, but not an entry boundary
    });
    let error = read(&bytes, &Limits::at("unused".into())).err().unwrap();
    assert_eq!(error.code, "E_HASH_MISMATCH");
    assert!(error.message.contains("offset delta base"));
}

#[test]
fn g5_pack_offset_and_reference_deltas_resolve_inside_pack() {
    let bytes = fixture_pack(|_| {});
    let objects = read(&bytes, &Limits::at("unused".into())).unwrap();
    assert_eq!(objects.len(), 3);
    let data: Vec<_> = objects.values().map(|obj| obj.data.as_slice()).collect();
    assert!(data.contains(&&b"two!"[..]));
    assert!(data.contains(&&b"two?"[..]));
}
#[test]
fn g5_thin_pack_and_extra_bytes_are_hash_mismatch() {
    let bytes = fixture_pack(|_| {});
    // A reference to an object not in this pack: no local object store may satisfy it.
    let mut thin = bytes[..bytes.len() - 20].to_vec();
    let mut delta = vec![4, 4, 0x90, 3, 1, b'!'];
    let at = 12 + 1 + zlib(&recorded_blob()).len() + 2 + zlib(&delta).len();
    delta[5] = b'?';
    assert_eq!(
        thin[at], 0x76,
        "reference delta header derived from the recorded blob"
    );
    thin[at + 1..at + 21].fill(0);
    let mut hash = Sha1CD::default();
    hash.update(&thin);
    thin.extend_from_slice(&hash.finalize_cd().unwrap());
    let err = read(&thin, &Limits::at("unused".into())).err().unwrap();
    assert_eq!(err.code, "E_HASH_MISMATCH");
    assert!(err.message.contains("thin pack"));
    let extra = fixture_pack(|bytes| bytes.extend_from_slice(b"extra"));
    assert_eq!(
        read(&extra, &Limits::at("unused".into()))
            .err()
            .unwrap()
            .code,
        "E_HASH_MISMATCH"
    );
}
fn varint(mut n: usize, out: &mut Vec<u8>) {
    while n >= 128 {
        out.push(0x80 | (n & 127) as u8);
        n >>= 7;
    }
    out.push(n as u8);
}
/// A 300-byte blob, then OFS deltas that each copy the previous result and append one byte.
/// With `broken_last`, the last delta keeps its size header but carries a zero opcode.
fn growing_chain(deltas: usize, broken_last: bool) -> Vec<u8> {
    let mut out = b"PACK\0\0\0\x02".to_vec();
    out.extend_from_slice(&(1 + deltas as u32).to_be_bytes());
    let base = vec![b'x'; 300];
    let mut previous = out.len();
    out.extend([0xb0 | (300 & 15) as u8, (300 >> 4) as u8]);
    out.extend(zlib(&base));
    for i in 0..deltas {
        let len = 300 + i;
        let mut delta = Vec::new();
        varint(len, &mut delta);
        varint(len + 1, &mut delta);
        if broken_last && i + 1 == deltas {
            delta.push(0);
        } else {
            delta.extend([0xb0, len as u8, (len >> 8) as u8, 1, b'y']);
        }
        let offset = out.len();
        assert!(delta.len() < 16 && offset - previous < 128);
        out.extend([0x60 | delta.len() as u8, (offset - previous) as u8]);
        out.extend(zlib(&delta));
        previous = offset;
    }
    let mut hash = Sha1CD::default();
    hash.update(&out);
    out.extend_from_slice(&hash.finalize_cd().unwrap());
    out
}
#[test]
fn resolved_delta_bytes_are_capped_cumulatively() {
    // Every entry and every result is within the per-object cap, and the pack's inflated
    // sizes are tiny; only the resolved results (300 + 301 + 302 + 303) exceed 2 * 512.
    let small = Limits {
        max_tree_bytes: 512,
        ..Limits::at("unused".into())
    };
    assert_eq!(
        read(&growing_chain(3, false), &Limits::at("unused".into()))
            .unwrap()
            .len(),
        4
    );
    assert_eq!(read(&growing_chain(2, false), &small).unwrap().len(), 3);
    for broken_last in [false, true] {
        // With a broken last delta, only a refusal made before `apply` runs names the cap.
        let error = read(&growing_chain(3, broken_last), &small).err().unwrap();
        assert_eq!(error.code, "E_HASH_MISMATCH");
        assert!(error.message.contains("cap"), "{}", error.message);
    }
}
