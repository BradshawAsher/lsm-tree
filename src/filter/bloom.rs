use bytes::{BufMut, Bytes, BytesMut};

/// A space-efficient Bloom Filter with Kirsch-Mitzenmacher double-hashing.
#[derive(Clone, Debug)]
pub struct BloomFilter {
    bitset: Bytes,
    k: u8, // Number of hash functions
}

impl BloomFilter {
    /// Builds a Bloom Filter from a list of key hashes.
    /// Uses 10 bits per key (~1% false positive rate) and optimal k = 7 hash functions.
    pub fn build(keys: &[&[u8]]) -> Self {
        if keys.is_empty() {
            return Self {
                bitset: Bytes::new(),
                k: 0,
            };
        }

        // 10 bits per key, rounded up to whole bytes
        let num_bits = (keys.len() * 10).max(64);
        let num_bytes = (num_bits + 7) / 8;
        let mut bitset = vec![0u8; num_bytes];
        let total_bits = (num_bytes * 8) as u32;

        // Optimal k = (m / n) * ln(2) ≈ 10 * 0.693 ≈ 7
        let k = 7u8;

        for key in keys {
            let (h1, h2) = hash_pair(key);
            for i in 0..k {
                let combined_hash = h1.wrapping_add((i as u32).wrapping_mul(h2));
                let bit_idx = (combined_hash % total_bits) as usize;
                bitset[bit_idx / 8] |= 1 << (bit_idx % 8);
            }
        }

        Self {
            bitset: Bytes::from(bitset),
            k,
        }
    }

    /// Tests whether a key may be present.
    /// If returns `false`, the key is GUARANTEED not to exist.
    /// If returns `true`, the key MAY exist (with ~1% false positive rate).
    pub fn may_contain(&self, key: &[u8]) -> bool {
        if self.bitset.is_empty() || self.k == 0 {
            return true; // Empty filter conservatively returns true
        }

        let total_bits = (self.bitset.len() * 8) as u32;
        let (h1, h2) = hash_pair(key);

        for i in 0..self.k {
            let combined_hash = h1.wrapping_add((i as u32).wrapping_mul(h2));
            let bit_idx = (combined_hash % total_bits) as usize;
            if (self.bitset[bit_idx / 8] & (1 << (bit_idx % 8))) == 0 {
                return false; // Found a 0 bit -> definitively not present!
            }
        }

        true
    }

    /// Serializes the filter into bytes: [k: u8] [bitset: N bytes]
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(1 + self.bitset.len());
        buf.put_u8(self.k);
        buf.put_slice(&self.bitset);
        buf.freeze()
    }

    /// Deserializes bytes into a BloomFilter.
    pub fn decode(data: Bytes) -> Self {
        if data.is_empty() {
            return Self {
                bitset: Bytes::new(),
                k: 0,
            };
        }
        let k = data[0];
        let bitset = data.slice(1..);
        Self { bitset, k }
    }
}

/// Generates two independent 32-bit hashes using FNV-1a with distinct prime offsets.
fn hash_pair(data: &[u8]) -> (u32, u32) {
    let mut h1: u32 = 0x811c9dc5;
    let mut h2: u32 = 0x9e3779b9;

    for &byte in data {
        h1 ^= byte as u32;
        h1 = h1.wrapping_mul(0x01000193);

        h2 ^= byte as u32;
        h2 = h2.wrapping_mul(0x85ebca6b);
    }

    (h1, h2)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_bloom_filter_accuracy() {
        let keys = vec![
            b"user:001".as_slice(),
            b"user:002".as_slice(),
            b"user:003".as_slice(),
            b"user:100".as_slice(),
            b"order:999".as_slice(),
        ];

        let filter = BloomFilter::build(&keys);

        // All added keys MUST be present (zero false negatives)
        for key in &keys {
            assert!(filter.may_contain(key));
        }

        // Keys not added should almost certainly return false
        assert!(!filter.may_contain(b"user:404"));
        assert!(!filter.may_contain(b"non_existent_key"));
        assert!(!filter.may_contain(b"random_query"));

        // Serialization roundtrip
        let encoded = filter.encode();
        let decoded = BloomFilter::decode(encoded);
        for key in &keys {
            assert!(decoded.may_contain(key));
        }
        assert!(!decoded.may_contain(b"user:404"));
    }
}
