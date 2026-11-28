//! A small QR code encoder (byte mode, error correction level M, versions
//! 1–10, up to 213 bytes) that draws an SVG — enough for the `otpauth://` links
//! authenticator apps scan. The construction follows ISO/IEC 18004.

/// Per version (1-based index): data codewords per block for each block, and
/// error-correction codewords per block, at level M.
const BLOCKS_M: [(&[usize], usize); 10] = [
    (&[16], 10),
    (&[28], 16),
    (&[44], 26),
    (&[32, 32], 18),
    (&[43, 43], 24),
    (&[27, 27, 27, 27], 16),
    (&[31, 31, 31, 31], 18),
    (&[38, 38, 39, 39], 22),
    (&[36, 36, 36, 37, 37], 22),
    (&[43, 43, 43, 43, 44], 26),
];

const ALIGN: [&[usize]; 10] = [&[], &[6, 18], &[6, 22], &[6, 26], &[6, 30], &[6, 34], &[6, 22, 38], &[6, 24, 42], &[6, 26, 46], &[6, 28, 50]];

fn gf_mul(x: u8, y: u8) -> u8 {
    let mut z: u32 = 0;
    for i in (0..8).rev() {
        z = (z << 1) ^ ((z >> 7) * 0x11D);
        z ^= ((y as u32 >> i) & 1) * x as u32;
    }
    z as u8
}

fn rs_divisor(degree: usize) -> Vec<u8> {
    let mut r = vec![0u8; degree];
    r[degree - 1] = 1;
    let mut root = 1u8;
    for _ in 0..degree {
        for j in 0..degree {
            r[j] = gf_mul(r[j], root);
            if j + 1 < degree {
                r[j] ^= r[j + 1];
            }
        }
        root = gf_mul(root, 2);
    }
    r
}

fn rs_remainder(data: &[u8], div: &[u8]) -> Vec<u8> {
    let mut r = vec![0u8; div.len()];
    for &b in data {
        let f = b ^ r.remove(0);
        r.push(0);
        for (i, &c) in div.iter().enumerate() {
            r[i] ^= gf_mul(c, f);
        }
    }
    r
}

struct Grid {
    size: usize,
    dark: Vec<bool>,
    func: Vec<bool>,
}

impl Grid {
    fn set(&mut self, x: usize, y: usize, d: bool) {
        self.dark[y * self.size + x] = d;
        self.func[y * self.size + x] = true;
    }
    fn get(&self, x: usize, y: usize) -> bool {
        self.dark[y * self.size + x]
    }
}

fn mask_bit(m: u8, x: usize, y: usize) -> bool {
    match m {
        0 => (x + y).is_multiple_of(2),
        1 => y.is_multiple_of(2),
        2 => x.is_multiple_of(3),
        3 => (x + y).is_multiple_of(3),
        4 => (x / 3 + y / 2).is_multiple_of(2),
        5 => x * y % 2 + x * y % 3 == 0,
        6 => (x * y % 2 + x * y % 3).is_multiple_of(2),
        _ => ((x + y) % 2 + x * y % 3).is_multiple_of(2),
    }
}

fn draw_format(g: &mut Grid, mask: u8) {
    // Level M has format bits 00.
    let data = mask as u32;
    let mut rem = data;
    for _ in 0..10 {
        rem = (rem << 1) ^ ((rem >> 9) * 0x537);
    }
    let bits = ((data << 10) | rem) ^ 0x5412;
    let bit = |i: u32| (bits >> i) & 1 != 0;
    let n = g.size;
    for i in 0..=5 {
        g.set(8, i, bit(i as u32));
    }
    g.set(8, 7, bit(6));
    g.set(8, 8, bit(7));
    g.set(7, 8, bit(8));
    for i in 9..15 {
        g.set(14 - i, 8, bit(i as u32));
    }
    for i in 0..8 {
        g.set(n - 1 - i, 8, bit(i as u32));
    }
    for i in 8..15 {
        g.set(8, n - 15 + i, bit(i as u32));
    }
    g.set(8, n - 8, true);
}

/// Encode `text` as QR modules: the side length and the dark modules, row by
/// row. `None` if it is too long.
pub fn encode(text: &str) -> Option<(usize, Vec<bool>)> {
    let data = text.as_bytes();
    let (ver, (blocks, ecc)) = (1..=10).zip(BLOCKS_M).find(|(v, (b, _))| {
        let count_bits = if *v < 10 { 8 } else { 16 };
        4 + count_bits + data.len() * 8 <= b.iter().sum::<usize>() * 8
    })?;
    let capacity: usize = blocks.iter().sum();
    // Bit stream: byte mode, length, data, terminator, padding.
    let mut bits: Vec<bool> = Vec::new();
    let push = |bits: &mut Vec<bool>, v: u32, n: u32| (0..n).rev().for_each(|i| bits.push((v >> i) & 1 != 0));
    push(&mut bits, 0b0100, 4);
    push(&mut bits, data.len() as u32, if ver < 10 { 8 } else { 16 });
    data.iter().for_each(|&b| push(&mut bits, b as u32, 8));
    let room = capacity * 8;
    let terminator = 4.min(room - bits.len());
    bits.extend(std::iter::repeat_n(false, terminator));
    while !bits.len().is_multiple_of(8) {
        bits.push(false);
    }
    let mut bytes: Vec<u8> = bits.chunks(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8)).collect();
    let mut pad = [0xEC, 0x11].iter().cycle();
    while bytes.len() < capacity {
        bytes.push(*pad.next().unwrap());
    }
    // Split into blocks, add error correction, interleave.
    let div = rs_divisor(ecc);
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut at = 0;
    for &n in blocks {
        chunks.push(&bytes[at..at + n]);
        at += n;
    }
    let eccs: Vec<Vec<u8>> = chunks.iter().map(|c| rs_remainder(c, &div)).collect();
    let mut code = Vec::new();
    for i in 0..*blocks.iter().max().unwrap() {
        chunks.iter().filter(|c| i < c.len()).for_each(|c| code.push(c[i]));
    }
    for i in 0..ecc {
        eccs.iter().for_each(|e| code.push(e[i]));
    }

    let size = ver * 4 + 17;
    let mut g = Grid { size, dark: vec![false; size * size], func: vec![false; size * size] };
    for i in 0..size {
        g.set(6, i, i % 2 == 0);
        g.set(i, 6, i % 2 == 0);
    }
    for (cx, cy) in [(3, 3), (size - 4, 3), (3, size - 4)] {
        for dy in -4i32..=4 {
            for dx in -4i32..=4 {
                let (x, y) = (cx as i32 + dx, cy as i32 + dy);
                if x >= 0 && y >= 0 && (x as usize) < size && (y as usize) < size {
                    let d = dx.abs().max(dy.abs());
                    g.set(x as usize, y as usize, d != 2 && d != 4);
                }
            }
        }
    }
    let al = ALIGN[ver - 1];
    for (i, &ax) in al.iter().enumerate() {
        for (j, &ay) in al.iter().enumerate() {
            let last = al.len() - 1;
            if (i == 0 && j == 0) || (i == 0 && j == last) || (i == last && j == 0) {
                continue;
            }
            for dy in -2i32..=2 {
                for dx in -2i32..=2 {
                    g.set((ax as i32 + dx) as usize, (ay as i32 + dy) as usize, dx.abs().max(dy.abs()) != 1);
                }
            }
        }
    }
    draw_format(&mut g, 0);
    if ver >= 7 {
        let mut rem = ver as u32;
        for _ in 0..12 {
            rem = (rem << 1) ^ ((rem >> 11) * 0x1F25);
        }
        let vbits = ((ver as u32) << 12) | rem;
        for i in 0..18 {
            let b = (vbits >> i) & 1 != 0;
            let (a, c) = (size - 11 + i % 3, i / 3);
            g.set(a, c, b);
            g.set(c, a, b);
        }
    }
    // Data modules in the zigzag order.
    let mut i = 0usize;
    let mut right = size as i32 - 1;
    while right >= 1 {
        if right == 6 {
            right = 5;
        }
        for vert in 0..size {
            for j in 0..2 {
                let x = (right - j) as usize;
                let upward = (right + 1) & 2 == 0;
                let y = if upward { size - 1 - vert } else { vert };
                if !g.func[y * size + x] && i < code.len() * 8 {
                    g.dark[y * size + x] = (code[i >> 3] >> (7 - (i & 7))) & 1 != 0;
                    i += 1;
                }
            }
        }
        right -= 2;
    }
    // Pick the mask with the lowest penalty.
    let base = g.dark.clone();
    let mut best: Option<(u32, Vec<bool>)> = None;
    for m in 0..8u8 {
        g.dark = base.clone();
        for y in 0..size {
            for x in 0..size {
                if !g.func[y * size + x] && mask_bit(m, x, y) {
                    g.dark[y * size + x] ^= true;
                }
            }
        }
        draw_format(&mut g, m);
        let p = penalty(&g);
        if best.as_ref().is_none_or(|(bp, _)| p < *bp) {
            best = Some((p, g.dark.clone()));
        }
    }
    Some((size, best?.1))
}

/// The usual penalty score (runs, 2×2 blocks, finder-like patterns, balance).
fn penalty(g: &Grid) -> u32 {
    let n = g.size;
    let mut p = 0u32;
    let line = |get: &dyn Fn(usize) -> bool| -> u32 {
        let mut s = 0u32;
        let (mut run, mut prev) = (0u32, None);
        let mut seq: Vec<bool> = Vec::with_capacity(n);
        for i in 0..n {
            let d = get(i);
            seq.push(d);
            if Some(d) == prev {
                run += 1;
            } else {
                if run >= 5 {
                    s += run - 2;
                }
                run = 1;
                prev = Some(d);
            }
        }
        if run >= 5 {
            s += run - 2;
        }
        let pat = [true, false, true, true, true, false, true];
        for i in 0..n.saturating_sub(6) {
            if seq[i..i + 7] == pat {
                let before = i >= 4 && seq[i - 4..i].iter().all(|d| !d);
                let after = i + 11 <= n && seq[i + 7..i + 11].iter().all(|d| !d);
                if before || after {
                    s += 40;
                }
            }
        }
        s
    };
    for y in 0..n {
        p += line(&|x| g.get(x, y));
    }
    for x in 0..n {
        p += line(&|y| g.get(x, y));
    }
    for y in 0..n - 1 {
        for x in 0..n - 1 {
            let c = g.get(x, y);
            if c == g.get(x + 1, y) && c == g.get(x, y + 1) && c == g.get(x + 1, y + 1) {
                p += 3;
            }
        }
    }
    let dark = g.dark.iter().filter(|d| **d).count() as u32;
    let total = (n * n) as u32;
    let k = ((dark * 20).abs_diff(total * 10)).div_ceil(total).saturating_sub(1);
    p + k * 10
}

/// The QR code as a standalone SVG with a quiet zone of four modules.
pub fn svg(text: &str) -> Option<String> {
    let (n, dark) = encode(text)?;
    let q = 4;
    let mut path = String::new();
    for y in 0..n {
        for x in 0..n {
            if dark[y * n + x] {
                path.push_str(&format!("M{} {}h1v1h-1z", x + q, y + q));
            }
        }
    }
    let s = n + 2 * q;
    Some(format!(
        "<svg xmlns=\"http://www.w3.org/2000/svg\" viewBox=\"0 0 {s} {s}\" shape-rendering=\"crispEdges\"><rect width=\"{s}\" height=\"{s}\" fill=\"#fff\"/><path fill=\"#000\" d=\"{path}\"/></svg>"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn sizes_follow_the_length() {
        assert_eq!(super::encode("hello").unwrap().0, 21);
        assert_eq!(super::encode(&"x".repeat(100)).unwrap().0, 6 * 4 + 17);
        assert_eq!(super::encode(&"x".repeat(213)).unwrap().0, 57);
        assert!(super::encode(&"x".repeat(214)).is_none());
    }

    #[test]
    fn version_one_matches_a_known_symbol() {
        // Finder pattern corners and the always-dark module.
        let (n, d) = super::encode("HELLO WORLD").unwrap();
        assert!(d[0] && d[6] && d[6 * n] && d[(n - 8) * n + 8]);
        assert!(!d[7] && !d[7 * n]);
    }
}

