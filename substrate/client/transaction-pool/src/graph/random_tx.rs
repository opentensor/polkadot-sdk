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
    let lanes = salt
        .chunks_exact(8)
        .map(|chunk| {
            chunk.iter().enumerate().fold(0u64, |acc, (i, &b)| {
                acc | ((b as u64) << (i * 8))
            })
        });

    for (atomic, lane) in TX_ORDER_SALT_U64.iter().zip(lanes) {
        atomic.store(lane, AtomicOrdering::Relaxed);
    }
}

#[inline]
pub fn current_tx_order_salt() -> [u8; 32] {
    let bytes_iter = TX_ORDER_SALT_U64
        .iter()
        .flat_map(|a| a.load(AtomicOrdering::Relaxed).to_le_bytes());

    let mut out = [0u8; 32];
    for (dst, byte) in out.iter_mut().zip(bytes_iter) {
        *dst = byte;
    }
    out
}

#[inline]
pub fn salted_key_from_tx_hash<Hash: Encode>(tx_hash: &Hash) -> [u8; 32] {
    let salt = current_tx_order_salt();
    let h = tx_hash.encode();

    let mut data = Vec::with_capacity(salt.len() + h.len());
    data.extend_from_slice(&salt);
    data.extend_from_slice(&h);

    blake2_256(&data)
}