//! SHA-256 digests as lowercase hex: content identities and cache keys.
use sha2::{Digest, Sha256};

/// One key for several byte strings: each part is length-prefixed, so
/// `["a", "bc"]` and `["ab", "c"]` differ. Response caches, job ids and
/// project hashes all use it.
pub fn cache_key(parts: &[&[u8]]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update((part.len() as u64).to_le_bytes());
        hash.update(part);
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

pub(crate) fn bytes_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_separates_endpoint_model_and_payload() {
        assert_ne!(cache_key(&[b"a", b"bc"]), cache_key(&[b"ab", b"c"]));
        assert_ne!(
            cache_key(&[b"model-a", b"audio"]),
            cache_key(&[b"model-b", b"audio"])
        );
        assert_ne!(
            cache_key(&[b"frames", b"one"]),
            cache_key(&[b"frames", b"two"])
        );
    }
}
