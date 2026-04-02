//! Runtime API definition for the MEV Shield.

extern crate alloc;

use crate::{ShieldError, ShieldedTransaction};
use alloc::vec::Vec;
use sp_runtime::traits::Block as BlockT;

type ExtrinsicOf<Block> = <Block as BlockT>::Extrinsic;

sp_api::decl_runtime_apis! {
	/// Runtime API for the MEV Shield.
	///
	/// V1: original signatures (`Option`-based, no error details).
	/// V2: error-aware signatures (`Result`-based with `ShieldError`).
	#[api_version(2)]
	pub trait ShieldApi {
		/// Try to decode a shielded transaction from an extrinsic.
		///
		/// Returns `None` if this is not a shielded extrinsic (i.e. not a `submit_encrypted` call).
		/// Returns `Some(Ok(tx))` if the shielded transaction was decoded successfully.
		/// Returns `Some(Err(e))` if it is a shielded extrinsic but decoding failed.
		fn try_decode_shielded_tx(uxt: ExtrinsicOf<Block>) -> Option<Result<ShieldedTransaction, ShieldError>>;

		/// Check if a transaction is shielded using the current key.
		fn is_shielded_using_current_key(key_hash: &[u8; 16]) -> bool;

		/// Try to unshield a transaction using a decapsulation key.
		fn try_unshield_tx(dec_key_bytes: Vec<u8>, shielded_tx: ShieldedTransaction) -> Result<ExtrinsicOf<Block>, ShieldError>;

		/// Build an unsigned extrinsic that reports an unshield error.
		/// The proposer pushes this into the block in place of the failed inner tx.
		fn make_unshield_error_extrinsic(wrapper_tx_hash: <Block as BlockT>::Hash, error: ShieldError) -> ExtrinsicOf<Block>;

		/// V1: try to decode a shielded transaction (no error details).
		#[changed_in(2)]
		fn try_decode_shielded_tx(uxt: ExtrinsicOf<Block>) -> Option<ShieldedTransaction>;

		/// V1: try to unshield a transaction (no error details).
		#[changed_in(2)]
		fn try_unshield_tx(dec_key_bytes: Vec<u8>, shielded_tx: ShieldedTransaction) -> Option<ExtrinsicOf<Block>>;
	}
}
