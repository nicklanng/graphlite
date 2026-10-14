//! CRC-32C (Castagnoli), using the CPU's CRC instructions where available
//! (ARMv8 `crc`, x86-64 SSE4.2) and slicing-by-8 tables otherwise. Kept
//! in-tree to stay dependency-free.
//!
//! The CRC instructions have a latency of several cycles but can issue every
//! cycle, so long inputs run three independent streams over adjacent blocks
//! and stitch them together. CRC registers are linear over GF(2):
//!
//! ```text
//! crc(s, A ++ B) = shift(crc(s, A), |B|) ^ crc(0, B)
//! ```
//!
//! where `shift(c, n)` is the register after feeding `n` zero bytes. `shift`
//! for the fixed block size is precomputed as four byte-indexed tables.

use std::sync::OnceLock;

const POLY: u32 = 0x82F6_3B78; // reflected Castagnoli polynomial

const TABLES: [[u32; 256]; 8] = make_tables();

/// Bytes per stream in the three-way loop: 3 × 1360 = 4080 covers a 4 KiB
/// page body in one pass.
const BLOCK: usize = 1360;

/// `shift(c, BLOCK)` as byte tables: XOR of `ZEROS[i][byte i of c]`.
fn zeros() -> &'static [[u32; 256]; 4] {
    static ZEROS: OnceLock<[[u32; 256]; 4]> = OnceLock::new();
    ZEROS.get_or_init(|| {
        let zero_block = [0u8; BLOCK];
        // shift is linear, so it's determined by its effect on each bit.
        let basis: Vec<u32> = (0..32).map(|bit| update_soft(1 << bit, &zero_block)).collect();
        let mut t = [[0u32; 256]; 4];
        for (i, table) in t.iter_mut().enumerate() {
            for (b, entry) in table.iter_mut().enumerate() {
                *entry = (0..8).filter(|k| b >> k & 1 == 1).fold(0, |acc, k| acc ^ basis[i * 8 + k]);
            }
        }
        t
    })
}

#[inline]
fn shift_block(z: &[[u32; 256]; 4], c: u32) -> u32 {
    z[0][(c & 0xff) as usize]
        ^ z[1][((c >> 8) & 0xff) as usize]
        ^ z[2][((c >> 16) & 0xff) as usize]
        ^ z[3][(c >> 24) as usize]
}

const fn make_tables() -> [[u32; 256]; 8] {
    let mut t = [[0u32; 256]; 8];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { POLY ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        t[0][i] = c;
        i += 1;
    }
    let mut s = 1;
    while s < 8 {
        let mut i = 0;
        while i < 256 {
            let prev = t[s - 1][i];
            t[s][i] = (prev >> 8) ^ t[0][(prev & 0xff) as usize];
            i += 1;
        }
        s += 1;
    }
    t
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = Hasher::new();
    h.update(data);
    h.finish()
}

/// Incremental CRC-32C over several slices.
pub struct Hasher(u32);

impl Hasher {
    pub fn new() -> Hasher {
        Hasher(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        #[cfg(target_arch = "aarch64")]
        if std::arch::is_aarch64_feature_detected!("crc") {
            // SAFETY: the `crc` feature was just detected.
            self.0 = unsafe { update_aarch64(self.0, data) };
            return;
        }
        #[cfg(target_arch = "x86_64")]
        if std::arch::is_x86_feature_detected!("sse4.2") {
            // SAFETY: the `sse4.2` feature was just detected.
            self.0 = unsafe { update_x86_64(self.0, data) };
            return;
        }
        self.0 = update_soft(self.0, data);
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

/// Defines a hardware CRC update from an 8-byte and a 1-byte step function.
macro_rules! hw_update {
    ($name:ident, $feature:literal, $word:expr, $byte:expr) => {
        #[target_feature(enable = $feature)]
        fn $name(mut crc: u32, data: &[u8]) -> u32 {
            let word = $word;
            let byte = $byte;
            let (chunks, data) = data.as_chunks::<{ 3 * BLOCK }>();
            if !chunks.is_empty() {
                let z = zeros();
                for chunk in chunks {
                    let (a, rest) = chunk.split_at(BLOCK);
                    let (b, c) = rest.split_at(BLOCK);
                    let (a, b, c) = (a.as_chunks::<8>().0, b.as_chunks::<8>().0, c.as_chunks::<8>().0);
                    let (mut x, mut y, mut w) = (crc, 0, 0);
                    for i in 0..BLOCK / 8 {
                        x = word(x, u64::from_le_bytes(a[i]));
                        y = word(y, u64::from_le_bytes(b[i]));
                        w = word(w, u64::from_le_bytes(c[i]));
                    }
                    crc = shift_block(z, shift_block(z, x) ^ y) ^ w;
                }
            }
            let (words, rest) = data.as_chunks::<8>();
            for w in words {
                crc = word(crc, u64::from_le_bytes(*w));
            }
            for &b in rest {
                crc = byte(crc, b);
            }
            crc
        }
    };
}

#[cfg(target_arch = "aarch64")]
hw_update!(
    update_aarch64,
    "crc",
    |c, w| std::arch::aarch64::__crc32cd(c, w),
    |c, b| std::arch::aarch64::__crc32cb(c, b)
);

#[cfg(target_arch = "x86_64")]
hw_update!(
    update_x86_64,
    "sse4.2",
    |c: u32, w| std::arch::x86_64::_mm_crc32_u64(u64::from(c), w) as u32,
    |c, b| std::arch::x86_64::_mm_crc32_u8(c, b)
);

fn update_soft(mut crc: u32, data: &[u8]) -> u32 {
    let (words, rest) = data.as_chunks::<8>();
    for w in words {
        let lo = crc ^ u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
        crc = TABLES[7][(lo & 0xff) as usize]
            ^ TABLES[6][((lo >> 8) & 0xff) as usize]
            ^ TABLES[5][((lo >> 16) & 0xff) as usize]
            ^ TABLES[4][(lo >> 24) as usize]
            ^ TABLES[3][w[4] as usize]
            ^ TABLES[2][w[5] as usize]
            ^ TABLES[1][w[6] as usize]
            ^ TABLES[0][w[7] as usize];
    }
    for &b in rest {
        crc = TABLES[0][((crc ^ b as u32) & 0xff) as usize] ^ (crc >> 8);
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHECK: u32 = 0xE306_9283; // CRC-32C of "123456789"

    #[test]
    fn check_value() {
        assert_eq!(crc32(b"123456789"), CHECK);
        let mut h = Hasher::new();
        h.update(b"1234");
        h.update(b"56789");
        assert_eq!(h.finish(), CHECK);
    }

    #[test]
    fn hardware_and_software_agree() {
        let data: Vec<u8> = (0..20_000u32).map(|i| (i.wrapping_mul(2_654_435_761) >> 13) as u8).collect();
        for len in [0, 1, 7, 8, 9, 63, 4079, 4080, 4081, 4092, 4096, 8160, 8167, 20_000] {
            let soft = !update_soft(!0, &data[..len]);
            assert_eq!(crc32(&data[..len]), soft, "len {len}");
        }
        assert_eq!(!update_soft(!0, b"123456789"), CHECK);
    }
}
