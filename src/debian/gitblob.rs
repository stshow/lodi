//! The git object id of a file (`git hash-object`), which is how a git tree names the files it
//! holds. The Debian OCI-layout base is pinned to a git commit, and the one file of it that no
//! digest chain covers, `rootfs.manifest`, is checked against the id the commit's tree lists for
//! it (M-1.0 u-4, LD-363).
//!
//! Git names blobs by SHA-1. No crate of the binary provides SHA-1, so it is written out here
//! (FIPS 180-4 §6.1); it identifies a file inside a tree Lodi has already pinned, and nothing
//! else in Lodi uses it.

/// The git blob id of `bytes`: SHA-1 of `blob <len>\0<bytes>`, lower-case hex.
pub fn blob_id(bytes: &[u8]) -> String {
    let mut data = format!("blob {}\0", bytes.len()).into_bytes();
    data.extend_from_slice(bytes);
    sha1_hex(&data)
}

/// SHA-1 of `data`, lower-case hex.
pub fn sha1_hex(data: &[u8]) -> String {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xEFCD_AB89,
        0x98BA_DCFE,
        0x1032_5476,
        0xC3D2_E1F0,
    ];
    let mut message = data.to_vec();
    message.push(0x80);
    while message.len() % 64 != 56 {
        message.push(0);
    }
    message.extend_from_slice(&((data.len() as u64).wrapping_mul(8)).to_be_bytes());
    for block in message.chunks_exact(64) {
        let mut w = [0u32; 80];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([word[0], word[1], word[2], word[3]]);
        }
        for i in 16..80 {
            w[i] = (w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16]).rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, word) in w.iter().enumerate() {
            let (f, k) = match i {
                0..=19 => ((b & c) | (!b & d), 0x5A82_7999),
                20..=39 => (b ^ c ^ d, 0x6ED9_EBA1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8F1B_BCDC),
                _ => (b ^ c ^ d, 0xCA62_C1D6),
            };
            let t = a
                .rotate_left(5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = t;
        }
        for (state, v) in h.iter_mut().zip([a, b, c, d, e]) {
            *state = state.wrapping_add(v);
        }
    }
    h.iter().map(|v| format!("{v:08x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha1_matches_the_published_test_vectors() {
        assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
        assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
        assert_eq!(
            sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
        );
        assert_eq!(
            sha1_hex(&vec![b'a'; 1_000_000]),
            "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
        );
    }

    #[test]
    fn blob_ids_are_the_ones_git_computes() {
        // `git hash-object` of an empty file and of `hello\n`.
        assert_eq!(blob_id(b""), "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391");
        assert_eq!(
            blob_id(b"hello\n"),
            "ce013625030ba8dba906f756967f9e9ca394464a"
        );
    }
}
