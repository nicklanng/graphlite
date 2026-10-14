//! CRC-32 (IEEE), table-driven. Kept in-tree to stay dependency-free.
//!
//! TODO: slicing-by-8 or hardware CRC once page checksums show up in profiles.

const TABLE: [u32; 256] = make_table();

const fn make_table() -> [u32; 256] {
    let mut table = [0u32; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u32;
        let mut k = 0;
        while k < 8 {
            c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            k += 1;
        }
        table[i] = c;
        i += 1;
    }
    table
}

pub fn crc32(data: &[u8]) -> u32 {
    let mut h = Hasher::new();
    h.update(data);
    h.finish()
}

/// Incremental CRC-32 over several slices.
pub struct Hasher(u32);

impl Hasher {
    pub fn new() -> Hasher {
        Hasher(!0)
    }

    pub fn update(&mut self, data: &[u8]) {
        for &b in data {
            self.0 = TABLE[((self.0 ^ b as u32) & 0xff) as usize] ^ (self.0 >> 8);
        }
    }

    pub fn finish(&self) -> u32 {
        !self.0
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn check_value() {
        assert_eq!(super::crc32(b"123456789"), 0xCBF4_3926);
        let mut h = super::Hasher::new();
        h.update(b"1234");
        h.update(b"56789");
        assert_eq!(h.finish(), 0xCBF4_3926);
    }
}
