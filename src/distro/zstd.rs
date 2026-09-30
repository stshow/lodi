//! A bounded Zstandard decoder (RFC 8878), for the one thing this build reads in that format:
//! Fedora's repository metadata (`primary.xml.zst`, LD-435).
//!
//! Decoding only, one pass, into memory, with the output capped: a frame that would produce more
//! than `limit` bytes is refused before the byte past the cap is written, so a small file that
//! expands without bound (an RLE block repeated, or a match copied over and over) cannot exhaust
//! memory. Dictionaries are refused; skippable frames are skipped; the optional content checksum
//! is not checked here, because every caller checks a SHA-256 of the decompressed bytes that the
//! repository's own `repomd.xml` states (`open-checksum`).

/// Decompress every frame of `input`, refusing output past `limit` bytes.
pub fn decompress(input: &[u8], limit: usize) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut at = 0;
    if input.is_empty() {
        return Err("empty zstd input".into());
    }
    while at < input.len() {
        let magic = le(input, at, 4)? as u32;
        if magic & 0xFFFF_FFF0 == 0x184D_2A50 {
            let size = le(input, at + 4, 4)? as usize;
            at = at
                .checked_add(8 + size)
                .filter(|end| *end <= input.len())
                .ok_or("truncated skippable frame")?;
            continue;
        }
        if magic != 0xFD2F_B528 {
            return Err(format!("not a zstd frame (magic {magic:#010x})"));
        }
        at = frame(input, at + 4, &mut out, limit)?;
    }
    Ok(out)
}

fn le(input: &[u8], at: usize, n: usize) -> Result<u64, String> {
    let bytes = input.get(at..at + n).ok_or("truncated zstd input")?;
    Ok(bytes
        .iter()
        .rev()
        .fold(0u64, |acc, b| (acc << 8) | u64::from(*b)))
}

/// The state that persists across the blocks of one frame.
struct Frame {
    huffman: Option<Huffman>,
    tables: [Option<Fse>; 3],
    repeat: [usize; 3],
    start: usize,
}

const LL: usize = 0;
const OF: usize = 1;
const ML: usize = 2;

fn frame(input: &[u8], mut at: usize, out: &mut Vec<u8>, limit: usize) -> Result<usize, String> {
    let descriptor = *input.get(at).ok_or("truncated frame header")?;
    at += 1;
    let fcs_flag = descriptor >> 6;
    let single_segment = descriptor & 0x20 != 0;
    if descriptor & 0x08 != 0 {
        return Err("reserved bit set in the frame header".into());
    }
    let checksum = descriptor & 0x04 != 0;
    let dictionary = [0usize, 1, 2, 4][usize::from(descriptor & 3)];
    if !single_segment {
        at += 1;
    }
    if dictionary > 0 && le(input, at, dictionary)? != 0 {
        return Err("zstd frames with a dictionary are not supported".into());
    }
    at += dictionary;
    let fcs_size = match (fcs_flag, single_segment) {
        (0, false) => 0,
        (0, true) => 1,
        (1, _) => 2,
        (2, _) => 4,
        _ => 8,
    };
    if fcs_size > 0 {
        let mut size = le(input, at, fcs_size)?;
        if fcs_size == 2 {
            size += 256;
        }
        if size > (limit - out.len().min(limit)) as u64 {
            return Err(format!("decompressed size exceeds the {limit}-byte limit"));
        }
    }
    at += fcs_size;
    let mut state = Frame {
        huffman: None,
        tables: [None, None, None],
        repeat: [1, 4, 8],
        start: out.len(),
    };
    loop {
        let header = le(input, at, 3)? as usize;
        at += 3;
        let last = header & 1 != 0;
        let size = header >> 3;
        match (header >> 1) & 3 {
            0 => {
                let raw = input.get(at..at + size).ok_or("truncated raw block")?;
                grow(out, size, limit)?;
                out.extend_from_slice(raw);
                at += size;
            }
            1 => {
                let byte = *input.get(at).ok_or("truncated RLE block")?;
                grow(out, size, limit)?;
                out.resize(out.len() + size, byte);
                at += 1;
            }
            2 => {
                let block = input
                    .get(at..at + size)
                    .ok_or("truncated compressed block")?;
                compressed_block(block, &mut state, out, limit)?;
                at += size;
            }
            _ => return Err("reserved block type".into()),
        }
        if last {
            break;
        }
    }
    if checksum {
        at += 4;
        if at > input.len() {
            return Err("truncated frame checksum".into());
        }
    }
    Ok(at)
}

fn grow(out: &[u8], by: usize, limit: usize) -> Result<(), String> {
    if out.len().saturating_add(by) > limit {
        return Err(format!("decompressed size exceeds the {limit}-byte limit"));
    }
    Ok(())
}

fn compressed_block(
    block: &[u8],
    state: &mut Frame,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), String> {
    let (literals, used) = literals_section(block, state)?;
    sequences_section(&block[used..], &literals, state, out, limit)
}

// ---------------------------------------------------------------------------------------------
// Bit readers.
// ---------------------------------------------------------------------------------------------

/// Reads a little-endian bit stream forward (the FSE table descriptions).
struct Forward<'a> {
    data: &'a [u8],
    bit: usize,
}

impl Forward<'_> {
    fn peek(&self, n: u32) -> u32 {
        let mut value = 0u64;
        for i in 0..8 {
            let byte = self.data.get(self.bit / 8 + i).copied().unwrap_or(0);
            value |= u64::from(byte) << (8 * i);
        }
        ((value >> (self.bit % 8)) & ((1u64 << n) - 1)) as u32
    }

    fn skip(&mut self, n: u32) {
        self.bit += n as usize;
    }

    fn read(&mut self, n: u32) -> u32 {
        let v = self.peek(n);
        self.skip(n);
        v
    }
}

/// Reads a bit stream backward from its end marker (Huffman and FSE payloads). Reading past the
/// start yields zeros and leaves `pos` negative, which is how the format signals the end.
struct Backward<'a> {
    data: &'a [u8],
    pos: i64,
}

impl<'a> Backward<'a> {
    fn new(data: &'a [u8]) -> Result<Self, String> {
        let last = *data.last().ok_or("empty bit stream")?;
        if last == 0 {
            return Err("bit stream without an end marker".into());
        }
        let pos = data.len() as i64 * 8 - i64::from(last.leading_zeros()) - 1;
        Ok(Backward { data, pos })
    }

    fn bits_at(&self, from: i64, n: u32) -> u64 {
        if n == 0 {
            return 0;
        }
        let (start, shift) = if from < 0 {
            (0, (-from) as u32)
        } else {
            (from, 0)
        };
        if shift >= n {
            return 0;
        }
        let width = n - shift;
        let byte = (start / 8) as usize;
        let mut value = 0u64;
        for i in 0..8 {
            let b = self.data.get(byte + i).copied().unwrap_or(0);
            value |= u64::from(b) << (8 * i);
        }
        let v = (value >> (start % 8)) & ((1u64 << width) - 1);
        v << shift
    }

    fn peek(&self, n: u32) -> u64 {
        self.bits_at(self.pos - i64::from(n), n)
    }

    fn read(&mut self, n: u32) -> u64 {
        let v = self.peek(n);
        self.pos -= i64::from(n);
        v
    }

    fn overflowed(&self) -> bool {
        self.pos < 0
    }
}

// ---------------------------------------------------------------------------------------------
// FSE.
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Fse {
    log: u32,
    /// Per state: (symbol, bits to read, baseline of the next state).
    cells: Vec<(u8, u8, u16)>,
}

impl Fse {
    fn build(norm: &[i32], log: u32) -> Result<Fse, String> {
        let size = 1usize << log;
        let mut cells = vec![(0u8, 0u8, 0u16); size];
        let mut high = size - 1;
        let mut next = vec![0u32; norm.len()];
        for (s, &n) in norm.iter().enumerate() {
            if n == -1 {
                cells[high].0 = s as u8;
                high = high.wrapping_sub(1);
                next[s] = 1;
            } else {
                next[s] = n.max(0) as u32;
            }
        }
        let step = (size >> 1) + (size >> 3) + 3;
        let mask = size - 1;
        let mut pos = 0usize;
        for (s, &n) in norm.iter().enumerate() {
            for _ in 0..n.max(0) {
                cells[pos].0 = s as u8;
                loop {
                    pos = (pos + step) & mask;
                    if high == usize::MAX || pos <= high {
                        break;
                    }
                }
            }
        }
        if pos != 0 {
            return Err("FSE table does not add up".into());
        }
        for cell in &mut cells {
            let s = usize::from(cell.0);
            let state = next[s];
            next[s] += 1;
            if state == 0 {
                return Err("FSE table does not add up".into());
            }
            let bits = log - (31 - state.leading_zeros());
            cell.1 = bits as u8;
            cell.2 = ((state << bits) as usize - size) as u16;
        }
        Ok(Fse { log, cells })
    }

    fn rle(symbol: u8) -> Fse {
        Fse {
            log: 0,
            cells: vec![(symbol, 0, 0)],
        }
    }

    /// Read a table description from the start of `data`; returns the table and bytes used.
    fn read(data: &[u8], max_symbol: usize, max_log: u32) -> Result<(Fse, usize), String> {
        let mut r = Forward { data, bit: 0 };
        let log = r.read(4) + 5;
        if log > max_log {
            return Err(format!("FSE accuracy log {log} exceeds {max_log}"));
        }
        let mut remaining = (1i32 << log) + 1;
        let mut threshold = 1i32 << log;
        let mut bits = log + 1;
        let mut norm: Vec<i32> = Vec::new();
        while remaining > 1 {
            if norm.len() > max_symbol {
                return Err("FSE table names too many symbols".into());
            }
            let max = (2 * threshold - 1) - remaining;
            let low = r.peek(bits - 1) as i32 & (threshold - 1);
            let mut count = if low < max {
                r.skip(bits - 1);
                low
            } else {
                let mut c = r.peek(bits) as i32 & (2 * threshold - 1);
                if c >= threshold {
                    c -= max;
                }
                r.skip(bits);
                c
            };
            count -= 1;
            remaining -= count.abs();
            norm.push(count);
            if count == 0 {
                loop {
                    let repeat = r.read(2);
                    norm.extend(std::iter::repeat_n(0, repeat as usize));
                    if repeat != 3 {
                        break;
                    }
                }
            }
            while remaining < threshold && bits > 1 {
                bits -= 1;
                threshold >>= 1;
            }
            if r.bit > data.len() * 8 {
                return Err("truncated FSE table description".into());
            }
        }
        if remaining != 1 || norm.len() > max_symbol + 1 {
            return Err("corrupt FSE table description".into());
        }
        let used = r.bit.div_ceil(8);
        Ok((Fse::build(&norm, log)?, used))
    }
}

struct State<'t> {
    table: &'t Fse,
    state: usize,
}

impl<'t> State<'t> {
    fn new(table: &'t Fse, bits: &mut Backward) -> Self {
        let state = bits.read(table.log) as usize;
        State { table, state }
    }

    fn symbol(&self) -> u8 {
        self.table.cells[self.state].0
    }

    fn update(&mut self, bits: &mut Backward) {
        let (_, n, base) = self.table.cells[self.state];
        self.state = usize::from(base) + bits.read(u32::from(n)) as usize;
    }
}

// ---------------------------------------------------------------------------------------------
// Literals and Huffman.
// ---------------------------------------------------------------------------------------------

#[derive(Clone)]
struct Huffman {
    bits: u32,
    /// Per `bits`-wide prefix: (symbol, code length).
    table: Vec<(u8, u8)>,
}

impl Huffman {
    fn read(data: &[u8]) -> Result<(Huffman, usize), String> {
        let header = usize::from(*data.first().ok_or("truncated Huffman tree")?);
        let (mut weights, used) = if header < 128 {
            let payload = data.get(1..1 + header).ok_or("truncated Huffman tree")?;
            let (fse, n) = Fse::read(payload, 255, 6)?;
            let stream = payload.get(n..).ok_or("truncated Huffman tree")?;
            let mut bits = Backward::new(stream)?;
            let mut one = State::new(&fse, &mut bits);
            let mut two = State::new(&fse, &mut bits);
            let mut weights = Vec::new();
            loop {
                weights.push(one.symbol());
                one.update(&mut bits);
                if bits.overflowed() {
                    weights.push(two.symbol());
                    break;
                }
                weights.push(two.symbol());
                two.update(&mut bits);
                if bits.overflowed() {
                    weights.push(one.symbol());
                    break;
                }
                if weights.len() > 255 {
                    return Err("Huffman tree names too many symbols".into());
                }
            }
            (weights, 1 + header)
        } else {
            let n = header - 127;
            let bytes = data
                .get(1..1 + n.div_ceil(2))
                .ok_or("truncated Huffman tree")?;
            let weights = (0..n)
                .map(|i| {
                    let b = bytes[i / 2];
                    if i % 2 == 0 { b >> 4 } else { b & 15 }
                })
                .collect();
            (weights, 1 + n.div_ceil(2))
        };
        let total: u32 = weights
            .iter()
            .filter(|w| **w > 0)
            .map(|w| 1u32 << (w - 1))
            .sum();
        if total == 0 || weights.iter().any(|w| *w > 11) {
            return Err("corrupt Huffman weights".into());
        }
        let bits = 32 - total.leading_zeros();
        let rest = (1u32 << bits) - total;
        if !rest.is_power_of_two() {
            return Err("corrupt Huffman weights".into());
        }
        weights.push((rest.trailing_zeros() + 1) as u8);
        if bits > 11 {
            return Err("Huffman code longer than 11 bits".into());
        }
        let mut table = vec![(0u8, 0u8); 1 << bits];
        let mut at = 0usize;
        for weight in 1..=bits as u8 {
            for (symbol, _) in weights.iter().enumerate().filter(|(_, w)| **w == weight) {
                let span = 1usize << (weight - 1);
                let len = bits as u8 + 1 - weight;
                for cell in &mut table[at..at + span] {
                    *cell = (symbol as u8, len);
                }
                at += span;
            }
        }
        if at != table.len() {
            return Err("corrupt Huffman weights".into());
        }
        Ok((Huffman { bits, table }, used))
    }

    fn decode(&self, stream: &[u8], count: usize, out: &mut Vec<u8>) -> Result<(), String> {
        let mut bits = Backward::new(stream)?;
        for _ in 0..count {
            let (symbol, len) = self.table[bits.peek(self.bits) as usize];
            bits.pos -= i64::from(len);
            out.push(symbol);
        }
        if bits.pos != 0 {
            return Err("Huffman stream does not end where its literals do".into());
        }
        Ok(())
    }
}

fn literals_section(block: &[u8], state: &mut Frame) -> Result<(Vec<u8>, usize), String> {
    let b0 = usize::from(*block.first().ok_or("empty compressed block")?);
    let byte = |i: usize| -> Result<usize, String> {
        block
            .get(i)
            .map(|b| usize::from(*b))
            .ok_or_else(|| "truncated literals header".to_string())
    };
    let kind = b0 & 3;
    let format = (b0 >> 2) & 3;
    if kind < 2 {
        let (size, header) = match format {
            0 | 2 => (b0 >> 3, 1),
            1 => ((b0 >> 4) + (byte(1)? << 4), 2),
            _ => ((b0 >> 4) + (byte(1)? << 4) + (byte(2)? << 12), 3),
        };
        if kind == 0 {
            let raw = block
                .get(header..header + size)
                .ok_or("truncated raw literals")?;
            return Ok((raw.to_vec(), header + size));
        }
        let b = *block.get(header).ok_or("truncated RLE literals")?;
        return Ok((vec![b; size], header + 1));
    }
    let (regenerated, compressed, header, streams) = match format {
        0 | 1 => {
            let v = b0 | byte(1)? << 8 | byte(2)? << 16;
            (
                (v >> 4) & 0x3FF,
                (v >> 14) & 0x3FF,
                3,
                if format == 0 { 1 } else { 4 },
            )
        }
        2 => {
            let v = b0 | byte(1)? << 8 | byte(2)? << 16 | byte(3)? << 24;
            ((v >> 4) & 0x3FFF, (v >> 18) & 0x3FFF, 4, 4)
        }
        _ => {
            let v = b0 | byte(1)? << 8 | byte(2)? << 16 | byte(3)? << 24 | byte(4)? << 32;
            ((v >> 4) & 0x3FFFF, (v >> 22) & 0x3FFFF, 5, 4)
        }
    };
    let payload = block
        .get(header..header + compressed)
        .ok_or("truncated compressed literals")?;
    let mut data = payload;
    if kind == 2 {
        let (huffman, used) = Huffman::read(payload)?;
        state.huffman = Some(huffman);
        data = &payload[used..];
    }
    let huffman = state
        .huffman
        .as_ref()
        .ok_or("treeless literals without a previous Huffman table")?;
    let mut literals = Vec::with_capacity(regenerated);
    if streams == 1 {
        huffman.decode(data, regenerated, &mut literals)?;
    } else {
        let jump = |i: usize| -> Result<usize, String> {
            Ok(usize::from(*data.get(i).ok_or("truncated jump table")?)
                | usize::from(*data.get(i + 1).ok_or("truncated jump table")?) << 8)
        };
        let sizes = [jump(0)?, jump(2)?, jump(4)?];
        let per = regenerated.div_ceil(4);
        let mut at = 6;
        for (i, size) in sizes.iter().enumerate() {
            let stream = data.get(at..at + size).ok_or("truncated literal stream")?;
            huffman.decode(stream, per, &mut literals)?;
            at += size;
            let _ = i;
        }
        let last = regenerated
            .checked_sub(3 * per)
            .ok_or("corrupt literal stream sizes")?;
        huffman.decode(
            data.get(at..).ok_or("truncated literal stream")?,
            last,
            &mut literals,
        )?;
    }
    Ok((literals, header + compressed))
}

// ---------------------------------------------------------------------------------------------
// Sequences.
// ---------------------------------------------------------------------------------------------

const LL_DEFAULT: [i32; 36] = [
    4, 3, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 2, 2, 2, 2, 2, 2, 2, 2, 2, 3, 2, 1, 1, 1, 1, 1,
    -1, -1, -1, -1,
];
const ML_DEFAULT: [i32; 53] = [
    1, 4, 3, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1,
    1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1, -1, -1,
];
const OF_DEFAULT: [i32; 29] = [
    1, 1, 1, 1, 1, 1, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, -1, -1, -1, -1, -1,
];

const LL_BASE: [(u32, u32); 20] = [
    (16, 1),
    (18, 1),
    (20, 1),
    (22, 1),
    (24, 2),
    (28, 2),
    (32, 3),
    (40, 3),
    (48, 4),
    (64, 6),
    (128, 7),
    (256, 8),
    (512, 9),
    (1024, 10),
    (2048, 11),
    (4096, 12),
    (8192, 13),
    (16384, 14),
    (32768, 15),
    (65536, 16),
];
const ML_BASE: [(u32, u32); 21] = [
    (35, 1),
    (37, 1),
    (39, 1),
    (41, 1),
    (43, 2),
    (47, 2),
    (51, 3),
    (59, 3),
    (67, 4),
    (83, 4),
    (99, 5),
    (131, 7),
    (259, 8),
    (515, 9),
    (1027, 10),
    (2051, 11),
    (4099, 12),
    (8195, 13),
    (16387, 14),
    (32771, 15),
    (65539, 16),
];

fn sequences_section(
    data: &[u8],
    literals: &[u8],
    state: &mut Frame,
    out: &mut Vec<u8>,
    limit: usize,
) -> Result<(), String> {
    let b0 = usize::from(*data.first().ok_or("truncated sequences header")?);
    let byte = |i: usize| -> Result<usize, String> {
        data.get(i)
            .map(|b| usize::from(*b))
            .ok_or_else(|| "truncated sequences header".to_string())
    };
    let (count, mut at) = match b0 {
        0 => {
            grow(out, literals.len(), limit)?;
            out.extend_from_slice(literals);
            return Ok(());
        }
        1..=127 => (b0, 1),
        128..=254 => (((b0 - 128) << 8) + byte(1)?, 2),
        _ => (byte(1)? + (byte(2)? << 8) + 0x7F00, 3),
    };
    let modes = byte(at)?;
    at += 1;
    if modes & 3 != 0 {
        return Err("reserved bits set in the sequence modes".into());
    }
    let specs: [(usize, &[i32], u32, usize, u32); 3] = [
        (LL, &LL_DEFAULT, 6, 35, 9),
        (OF, &OF_DEFAULT, 5, 31, 8),
        (ML, &ML_DEFAULT, 6, 52, 9),
    ];
    for (i, (which, default, default_log, max_symbol, max_log)) in specs.into_iter().enumerate() {
        let mode = (modes >> (6 - 2 * i)) & 3;
        match mode {
            0 => state.tables[which] = Some(Fse::build(default, default_log)?),
            1 => {
                let symbol = *data.get(at).ok_or("truncated RLE sequence table")?;
                if usize::from(symbol) > max_symbol {
                    return Err("RLE sequence symbol out of range".into());
                }
                state.tables[which] = Some(Fse::rle(symbol));
                at += 1;
            }
            2 => {
                let (table, used) =
                    Fse::read(data.get(at..).unwrap_or_default(), max_symbol, max_log)?;
                state.tables[which] = Some(table);
                at += used;
            }
            _ => {
                if state.tables[which].is_none() {
                    return Err("repeated sequence table without a previous one".into());
                }
            }
        }
    }
    let [ll_table, of_table, ml_table] = &state.tables;
    let (ll_table, of_table, ml_table) = (
        ll_table.as_ref().ok_or("missing table")?,
        of_table.as_ref().ok_or("missing table")?,
        ml_table.as_ref().ok_or("missing table")?,
    );
    let mut bits = Backward::new(data.get(at..).ok_or("truncated sequences")?)?;
    let mut ll_state = State::new(ll_table, &mut bits);
    let mut of_state = State::new(of_table, &mut bits);
    let mut ml_state = State::new(ml_table, &mut bits);
    let mut lit = 0usize;
    for n in 0..count {
        let of_code = u32::from(of_state.symbol());
        let ll_code = usize::from(ll_state.symbol());
        let ml_code = usize::from(ml_state.symbol());
        if of_code > 31 {
            return Err("offset code out of range".into());
        }
        let offset_value = (1u64 << of_code) + bits.read(of_code);
        let match_length = if ml_code < 32 {
            ml_code + 3
        } else {
            let (base, extra) = *ML_BASE.get(ml_code - 32).ok_or("match code out of range")?;
            base as usize + bits.read(extra) as usize
        };
        let literal_length = if ll_code < 16 {
            ll_code
        } else {
            let (base, extra) = *LL_BASE
                .get(ll_code - 16)
                .ok_or("literal code out of range")?;
            base as usize + bits.read(extra) as usize
        };
        let rep = &mut state.repeat;
        let offset = if offset_value > 3 {
            let offset = offset_value as usize - 3;
            *rep = [offset, rep[0], rep[1]];
            offset
        } else {
            let value = offset_value as usize + usize::from(literal_length == 0);
            match value {
                1 => rep[0],
                2 => {
                    *rep = [rep[1], rep[0], rep[2]];
                    rep[0]
                }
                3 => {
                    *rep = [rep[2], rep[0], rep[1]];
                    rep[0]
                }
                _ => {
                    let offset = rep[0]
                        .checked_sub(1)
                        .filter(|o| *o > 0)
                        .ok_or("zero offset")?;
                    *rep = [offset, rep[0], rep[1]];
                    offset
                }
            }
        };
        let run = literals
            .get(lit..lit + literal_length)
            .ok_or("sequence reads past its literals")?;
        grow(out, literal_length + match_length, limit)?;
        out.extend_from_slice(run);
        lit += literal_length;
        if offset == 0 || offset > out.len() - state.start {
            return Err("match offset before the start of the frame".into());
        }
        let from = out.len() - offset;
        if offset >= match_length {
            out.extend_from_within(from..from + match_length);
        } else {
            for i in 0..match_length {
                let b = out[from + i];
                out.push(b);
            }
        }
        if n + 1 < count {
            ll_state.update(&mut bits);
            ml_state.update(&mut bits);
            of_state.update(&mut bits);
        }
    }
    if bits.pos != 0 {
        return Err("sequence stream does not end where its sequences do".into());
    }
    let rest = &literals[lit..];
    grow(out, rest.len(), limit)?;
    out.extend_from_slice(rest);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A frame of raw and RLE blocks, the two block types a test can write by hand.
    fn frame(blocks: &[(u8, &[u8], usize)]) -> Vec<u8> {
        let mut out = vec![0x28, 0xB5, 0x2F, 0xFD, 0x00, 0x58];
        for (i, (kind, data, size)) in blocks.iter().enumerate() {
            let last = usize::from(i + 1 == blocks.len());
            let header = last | usize::from(*kind) << 1 | size << 3;
            out.extend_from_slice(&header.to_le_bytes()[..3]);
            out.extend_from_slice(data);
        }
        out
    }

    #[test]
    fn raw_and_rle_blocks_decode_and_the_cap_holds() {
        let bytes = frame(&[(0, b"abc", 3), (1, b"z", 5)]);
        assert_eq!(decompress(&bytes, 100).unwrap(), b"abczzzzz");
        let err = decompress(&bytes, 7).unwrap_err();
        assert!(err.contains("exceeds the 7-byte limit"), "{err}");
        // An RLE block of 16 MiB from four bytes of input is refused, not allocated.
        let bomb = frame(&[(1, b"x", (1 << 21) - 1)]);
        assert!(decompress(&bomb, 1024).is_err());
        assert!(decompress(b"not zstd", 100).is_err());
        assert!(decompress(&bytes[..bytes.len() - 1], 100).is_err());
    }
}
