use sp_core::{Encode, hashing::blake2_256};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};


static TX_ORDER_SALT_U64: [AtomicU64; 4] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// Set the per‑block salt used to randomize the ordering of ready transactions.
///
/// # Parameters
/// - `salt`: 32‑byte value (typically `blake2_256(seed || parent_hash)`).
pub fn set_tx_ordering_salt(salt: [u8; 32]) {
    let a = u64::from_le_bytes(salt[ 0.. 8].try_into().expect("len; qed"));
    let b = u64::from_le_bytes(salt[ 8..16].try_into().expect("len; qed"));
    let c = u64::from_le_bytes(salt[16..24].try_into().expect("len; qed"));
    let d = u64::from_le_bytes(salt[24..32].try_into().expect("len; qed"));
    TX_ORDER_SALT_U64[0].store(a, AtomicOrdering::Relaxed);
    TX_ORDER_SALT_U64[1].store(b, AtomicOrdering::Relaxed);
    TX_ORDER_SALT_U64[2].store(c, AtomicOrdering::Relaxed);
    TX_ORDER_SALT_U64[3].store(d, AtomicOrdering::Relaxed);
}

#[inline]
pub fn current_tx_order_salt() -> [u8; 32] {
    let mut out = [0u8; 32];
    out[ 0.. 8].copy_from_slice(&TX_ORDER_SALT_U64[0].load(AtomicOrdering::Relaxed).to_le_bytes());
    out[ 8..16].copy_from_slice(&TX_ORDER_SALT_U64[1].load(AtomicOrdering::Relaxed).to_le_bytes());
    out[16..24].copy_from_slice(&TX_ORDER_SALT_U64[2].load(AtomicOrdering::Relaxed).to_le_bytes());
    out[24..32].copy_from_slice(&TX_ORDER_SALT_U64[3].load(AtomicOrdering::Relaxed).to_le_bytes());
    out
}

#[inline]
pub fn salted_key_from_tx_hash<Hash: Encode>(tx_hash: &Hash) -> [u8; 32] {
    let salt = current_tx_order_salt();
    let h = tx_hash.encode();

    if h.len() <= 32 {
        let mut buf = [0u8; 64];
        buf[..32].copy_from_slice(&salt);
        buf[32..32 + h.len()].copy_from_slice(&h);
        blake2_256(&buf[..32 + h.len()])
    } else {
        let mut v = Vec::with_capacity(32 + h.len());
        v.extend_from_slice(&salt);
        v.extend_from_slice(&h);
        blake2_256(&v)
    }
}