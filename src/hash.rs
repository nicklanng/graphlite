//! A fast hasher for integer ids, replacing SipHash in page maps and
//! traversal sets. Page and node ids aren't attacker-chosen hash inputs in a
//! way that matters for an embedded store, so HashDoS resistance isn't needed.

use std::collections::{HashMap, HashSet};
use std::hash::{BuildHasherDefault, Hasher};

pub type IdMap<V> = HashMap<u64, V, BuildHasherDefault<IdHasher>>;
pub type IdSet = HashSet<u64, BuildHasherDefault<IdHasher>>;

#[derive(Default, Clone, Copy)]
pub struct IdHasher(u64);

impl Hasher for IdHasher {
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_u64(self.0.rotate_left(8) ^ u64::from(b));
        }
    }

    /// Folded multiply: mixes every input bit into both the low bits (bucket
    /// index) and the high bits (control byte) of the hash.
    fn write_u64(&mut self, n: u64) {
        let p = u128::from(n ^ self.0) * 0x9E37_79B9_7F4A_7C15;
        self.0 = (p as u64) ^ (p >> 64) as u64;
    }

    fn finish(&self) -> u64 {
        self.0
    }
}
