use std::collections::HashSet;

use hkdf::Hkdf;
use sha2::{Digest, Sha256};

pub mod scrypt;
pub mod tag;
pub mod tagpq;
pub mod x25519;

fn static_tag(pk: &[u8]) -> [u8; 4] {
    let digest = Sha256::digest(pk);
    let mut output = [0; 4];
    output.copy_from_slice(&digest[..4]);
    output
}

/// Derives a tag for the tagged age recipient formats.
fn stanza_tag(ikm: &[u8], salt: &str) -> [u8; 4] {
    let (tag, _) = Hkdf::<Sha256>::extract(Some(salt.as_bytes()), ikm);
    let mut output = [0; 4];
    output.copy_from_slice(&tag[..4]);
    output
}

fn label_pq_only() -> HashSet<String> {
    ["postquantum".into()].into_iter().collect()
}
