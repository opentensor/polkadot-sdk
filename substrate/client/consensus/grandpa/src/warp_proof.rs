// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Substrate.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate. If not, see <https://www.gnu.org/licenses/>.

//! Utilities for generating and verifying GRANDPA warp sync proofs.

use codec::{Decode, DecodeAll, Encode};

use crate::{
	best_justification, find_scheduled_change, AuthoritySetChanges, AuthoritySetHardFork,
	BlockNumberOps, GrandpaJustification, SharedAuthoritySet,
};
use sc_client_api::Backend as ClientBackend;
use sc_network_sync::strategy::warp::{
	EncodedProof, VerificationResult, Verifier, WarpSyncProvider,
};
use sp_blockchain::{Backend as BlockchainBackend, HeaderBackend};
use sp_consensus_grandpa::{
	AuthorityList, SetId, CLIENT_LOG_TARGET as LOG_TARGET, GRANDPA_ENGINE_ID,
};
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, NumberFor, One},
	Justifications,
};

use std::{collections::HashMap, sync::Arc};

/// Warp proof processing error.
#[derive(Debug, thiserror::Error)]
pub enum Error {
	/// Decoding error.
	#[error("Failed to decode block hash: {0}.")]
	DecodeScale(#[from] codec::Error),
	/// Client backend error.
	#[error("{0}")]
	Client(#[from] sp_blockchain::Error),
	/// Invalid request data.
	#[error("{0}")]
	InvalidRequest(String),
	/// Invalid warp proof.
	#[error("{0}")]
	InvalidProof(String),
	/// Missing header or authority set change data.
	#[error("Missing required data to be able to answer request.")]
	MissingData,
}

/// The maximum size in bytes of the `WarpSyncProof`.
pub(super) const MAX_WARP_SYNC_PROOF_SIZE: usize = 8 * 1024 * 1024;

/// A proof of an authority set change.
#[derive(Decode, Encode, Debug)]
pub struct WarpSyncFragment<Block: BlockT> {
	/// The last block that the given authority set finalized. This block should contain a digest
	/// signaling an authority set change from which we can fetch the next authority set, unless it
	/// is an explicitly configured off-chain authority-set checkpoint.
	pub header: Block::Header,
	/// A justification for the header above which proves its finality. In order to validate it the
	/// verifier must be aware of the authorities and set id for which the justification refers to.
	pub justification: GrandpaJustification<Block>,
}

/// An accumulated proof of multiple authority set changes.
#[derive(Decode, Encode)]
pub struct WarpSyncProof<Block: BlockT> {
	proofs: Vec<WarpSyncFragment<Block>>,
	is_finished: bool,
}

impl<Block: BlockT> WarpSyncProof<Block> {
	fn push_fragment<Blockchain>(
		blockchain: &Blockchain,
		header: Block::Header,
		proofs: &mut Vec<WarpSyncFragment<Block>>,
		proofs_encoded_len: &mut usize,
	) -> Result<bool, Error>
	where
		Blockchain: BlockchainBackend<Block>,
	{
		let justification = blockchain
			.justifications(header.hash())?
			.and_then(|just| just.into_justification(GRANDPA_ENGINE_ID))
			.ok_or(Error::MissingData)?;
		let justification = GrandpaJustification::<Block>::decode_all(&mut &justification[..])?;
		let proof = WarpSyncFragment { header, justification };
		let proof_size = proof.encoded_size();

		// Leave room for the encoded `Vec` length and `is_finished` flag, which are not part of
		// the fragment sizes accumulated here.
		if *proofs_encoded_len + proof_size >= MAX_WARP_SYNC_PROOF_SIZE - 50 {
			return Ok(false)
		}

		*proofs_encoded_len += proof_size;
		proofs.push(proof);
		Ok(true)
	}

	/// Generates a warp sync proof starting at the given block. It will generate authority set
	/// change proofs for all changes that happened from `begin` until the current authority set
	/// (capped by MAX_WARP_SYNC_PROOF_SIZE).
	fn generate<Backend>(
		backend: &Backend,
		begin: Block::Hash,
		set_changes: &AuthoritySetChanges<NumberFor<Block>>,
		hard_forks: &HardForks<Block>,
	) -> Result<WarpSyncProof<Block>, Error>
	where
		Backend: ClientBackend<Block>,
	{
		// TODO: cache best response (i.e. the one with lowest begin_number)
		let blockchain = backend.blockchain();

		let begin_number = blockchain
			.block_number_from_id(&BlockId::Hash(begin))?
			.ok_or_else(|| Error::InvalidRequest("Missing start block".to_string()))?;

		let finalized_number = blockchain.info().finalized_number;
		if begin_number > finalized_number {
			return Err(Error::InvalidRequest("Start block is not finalized".to_string()));
		}

		let canon_hash = blockchain.hash(begin_number)?.expect(
			"begin number is lower than finalized number; \
			 all blocks below finalized number must have been imported; \
			 qed.",
		);

		if canon_hash != begin {
			return Err(Error::InvalidRequest(
				"Start block is not in the finalized chain".to_string(),
			));
		}

		let mut latest_checkpoint = None;
		if let Some(checkpoints) = hard_forks.authority_set_checkpoints() {
			for ((hash, number), (set_id, _)) in checkpoints {
				if *number <= begin_number || *number > finalized_number {
					continue
				}
				if latest_checkpoint
					.as_ref()
					.is_some_and(|(_, latest_number, _)| number <= *latest_number)
				{
					continue
				}
				if blockchain.hash(*number)?.as_ref() == Some(hash) {
					latest_checkpoint = Some((hash, number, *set_id));
				}
			}
		}

		let (checkpoint_header, set_change_blocks) = if let Some((hash, number, set_id)) =
			latest_checkpoint
		{
			let header = blockchain.header(*hash)?.ok_or(Error::MissingData)?;
			let expected_set_id = if find_scheduled_change::<Block>(&header).is_some() {
				set_id.checked_add(1).ok_or(Error::MissingData)?
			} else {
				set_id
			};
			let changes = set_changes
				.contiguous_changes_after(expected_set_id, *number)
				.ok_or(Error::MissingData)?;
			(Some(header), changes)
		} else {
			let changes = set_changes.iter_from(begin_number).ok_or(Error::MissingData)?.collect();
			(None, changes)
		};

		let mut proofs = Vec::new();
		let mut proofs_encoded_len = 0;
		let mut proof_limit_reached = if let Some(header) = checkpoint_header {
			!Self::push_fragment(blockchain, header, &mut proofs, &mut proofs_encoded_len)?
		} else {
			false
		};

		for (_, last_block) in set_change_blocks {
			if proof_limit_reached {
				break
			}
			let hash = match blockchain.block_hash_from_id(&BlockId::Number(*last_block))? {
				Some(hash) => hash,
				None => {
					log::debug!(target: LOG_TARGET, "Ignorning warp proof with invalid block number.");
					return Err(Error::InvalidRequest("header number comes from previously applied set changes; corresponding hash must exist in db.".to_string()))
				},
			};

			let header = match blockchain.header(hash)? {
				Some(header) => header,
				None => {
					log::debug!(target: LOG_TARGET, "Ignorning warp proof with invalid block hash.");
					return Err(Error::InvalidRequest("header hash obtained from header number exists in db; corresponding header must exist in db too.".to_string()))
				},
			};

			// Recorded changes must contain the runtime signal that hands authority to the next
			// set. Configured hard-fork checkpoints are emitted separately above and need no such
			// signal.
			if find_scheduled_change::<Block>(&header).is_none() {
				// if it doesn't contain a signal for standard change then the set must have changed
				// through a forced changed, in which case we stop collecting proofs as the chain of
				// trust in authority handoffs was broken.
				break;
			}

			if !Self::push_fragment(blockchain, header, &mut proofs, &mut proofs_encoded_len)? {
				proof_limit_reached = true;
				break;
			}
		}

		let is_finished = if proof_limit_reached {
			false
		} else {
			let latest_justification = best_justification(backend)?.filter(|justification| {
				// the existing best justification must be for a block higher than the
				// last authority set change. if we didn't prove any authority set
				// change then we fallback to make sure it's higher or equal to the
				// initial warp sync block.
				let limit = proofs
					.last()
					.map(|proof| proof.justification.target().0 + One::one())
					.unwrap_or(begin_number);

				justification.target().0 >= limit
			});

			if let Some(latest_justification) = latest_justification {
				let header = blockchain.header(latest_justification.target().1)?
					.expect("header hash corresponds to a justification in db; must exist in db as well; qed.");

				let proof = WarpSyncFragment { header, justification: latest_justification };

				// Check for the limit. We remove some bytes from the maximum size, because we're
				// only counting the size of the `WarpSyncFragment`s. The extra margin is here
				// to leave room for rest of the data (the size of the `Vec` and the boolean).
				if proofs_encoded_len + proof.encoded_size() >= MAX_WARP_SYNC_PROOF_SIZE - 50 {
					false
				} else {
					proofs.push(proof);
					true
				}
			} else {
				true
			}
		};

		let final_outcome = WarpSyncProof { proofs, is_finished };
		debug_assert!(final_outcome.encoded_size() <= MAX_WARP_SYNC_PROOF_SIZE);
		Ok(final_outcome)
	}

	/// Verifies the warp sync proof starting at the given set id and with the given authorities.
	/// Verification stops when either the proof is exhausted or finality for the target header can
	/// be proven. If the proof is valid the new set id and authorities is returned.
	fn verify(
		&self,
		set_id: SetId,
		authorities: AuthorityList,
		hard_forks: &HardForks<Block>,
	) -> Result<(SetId, AuthorityList), Error>
	where
		NumberFor<Block>: BlockNumberOps,
	{
		let mut current_set_id = set_id;
		let mut current_authorities = authorities;

		for (fragment_num, proof) in self.proofs.iter().enumerate() {
			let hash = proof.header.hash();
			let number = *proof.header.number();

			let is_checkpoint = if let Some((set_id, list)) =
				hard_forks.get_hard_forked_authorities(&(hash, number))
			{
				current_set_id = set_id;
				current_authorities = list.clone();
				true
			} else if let Some(initial_set_id) = hard_forks.get_new_initial_set_id() {
				current_set_id += initial_set_id;
				false
			} else {
				false
			};
			{
				proof
					.justification
					.verify(current_set_id, &current_authorities)
					.map_err(|err| Error::InvalidProof(err.to_string()))?;

				if proof.justification.target().1 != hash {
					return Err(Error::InvalidProof(
						"Mismatch between header and justification".to_owned(),
					));
				}

				if let Some(scheduled_change) = find_scheduled_change::<Block>(&proof.header) {
					current_authorities = scheduled_change.next_authorities;
					current_set_id += 1;
				} else if !is_checkpoint &&
					(fragment_num != self.proofs.len() - 1 || !self.is_finished)
				{
					// Only the last fragment of the last proof message is allowed to be missing the
					// authority set change.
					return Err(Error::InvalidProof(
						"Header is missing authority set change digest".to_string(),
					));
				}
			}
		}
		Ok((current_set_id, current_authorities))
	}
}

/// Implements network API for warp sync.
pub struct NetworkProvider<Block: BlockT, Backend: ClientBackend<Block>>
where
	NumberFor<Block>: BlockNumberOps,
{
	backend: Arc<Backend>,
	authority_set: SharedAuthoritySet<Block::Hash, NumberFor<Block>>,
	hard_forks: HardForks<Block>,
}

/// Contains the data needed to verify a warp sync proof for hard forks
#[derive(Clone)]
pub enum HardForks<Block: BlockT> {
	/// Sets new authorities and set ID by block hash and number
	AuthoritySetHardForks {
		/// Maps block to authority list and set ID
		hard_forks: HashMap<(Block::Hash, NumberFor<Block>), (SetId, AuthorityList)>,
	},
	/// Provides new initial set ID for granpda block import
	ReinitializeSetId {
		/// New initial set ID
		new_set_id: SetId,
	},
}

impl<Block: BlockT> HardForks<Block> {
	/// Create a new instance for a given hard fork authorities
	pub fn new_hard_forked_authorities(hard_forks: Vec<AuthoritySetHardFork<Block>>) -> Self {
		HardForks::AuthoritySetHardForks {
			hard_forks: hard_forks
				.into_iter()
				.map(|fork| (fork.block, (fork.set_id, fork.authorities)))
				.collect(),
		}
	}

	/// Create a new instance for a given hard fork authorities
	pub fn new_initial_set_id(set_id: SetId) -> Self {
		HardForks::ReinitializeSetId { new_set_id: set_id }
	}

	fn get_hard_forked_authorities(
		&self,
		block: &(Block::Hash, NumberFor<Block>),
	) -> Option<(SetId, AuthorityList)> {
		if let HardForks::AuthoritySetHardForks { hard_forks } = self {
			hard_forks.get(block).cloned()
		} else {
			None
		}
	}

	fn get_new_initial_set_id(&self) -> Option<SetId> {
		if let HardForks::ReinitializeSetId { new_set_id } = self {
			Some(*new_set_id)
		} else {
			None
		}
	}

	fn authority_set_checkpoints(
		&self,
	) -> Option<&HashMap<(Block::Hash, NumberFor<Block>), (SetId, AuthorityList)>> {
		match self {
			Self::AuthoritySetHardForks { hard_forks } => Some(hard_forks),
			Self::ReinitializeSetId { .. } => None,
		}
	}
}

impl<Block: BlockT, Backend: ClientBackend<Block>> NetworkProvider<Block, Backend>
where
	NumberFor<Block>: BlockNumberOps,
{
	/// Create a new instance for a given backend and authority set.
	pub fn new(
		backend: Arc<Backend>,
		authority_set: SharedAuthoritySet<Block::Hash, NumberFor<Block>>,
		hard_forks: HardForks<Block>,
	) -> Self {
		NetworkProvider { backend, authority_set, hard_forks }
	}
}

/// Verifier state for GRANDPA warp sync.
struct VerifierState<Block: BlockT> {
	set_id: SetId,
	authorities: AuthorityList,
	next_proof_context: Block::Hash,
}

/// Verifier implementation for GRANDPA warp sync.
struct GrandpaVerifier<Block: BlockT> {
	state: VerifierState<Block>,
	hard_forks: HardForks<Block>,
	eras_synced: u64,
}

impl<Block: BlockT> Verifier<Block> for GrandpaVerifier<Block>
where
	NumberFor<Block>: BlockNumberOps,
{
	fn verify(
		&mut self,
		proof: &EncodedProof,
	) -> Result<VerificationResult<Block>, Box<dyn std::error::Error + Send + Sync>> {
		let EncodedProof(proof) = proof;
		let proof = WarpSyncProof::<Block>::decode_all(&mut proof.as_slice())
			.map_err(|e| format!("Proof decoding error: {:?}", e))?;
		let last_header = proof
			.proofs
			.last()
			.map(|p| p.header.clone())
			.ok_or_else(|| "Empty proof".to_string())?;

		let (current_set_id, current_authorities) =
			(self.state.set_id, self.state.authorities.clone());

		let (next_set_id, next_authorities) = proof
			.verify(current_set_id, current_authorities, &self.hard_forks)
			.map_err(Box::new)?;

		self.state = VerifierState {
			set_id: next_set_id,
			authorities: next_authorities,
			next_proof_context: last_header.hash(),
		};

		// Track eras synced
		self.eras_synced += proof.proofs.len() as u64;

		let justifications = proof
			.proofs
			.into_iter()
			.map(|p| {
				let justifications =
					Justifications::new(vec![(GRANDPA_ENGINE_ID, p.justification.encode())]);
				(p.header, justifications)
			})
			.collect::<Vec<_>>();

		if proof.is_finished {
			Ok(VerificationResult::Complete(last_header, justifications))
		} else {
			Ok(VerificationResult::Partial(justifications))
		}
	}

	fn next_proof_context(&self) -> Block::Hash {
		self.state.next_proof_context
	}

	fn status(&self) -> Option<String> {
		Some(format!("{} eras synced", self.eras_synced))
	}
}

impl<Block: BlockT, Backend: ClientBackend<Block>> WarpSyncProvider<Block>
	for NetworkProvider<Block, Backend>
where
	NumberFor<Block>: BlockNumberOps,
{
	fn generate(
		&self,
		start: Block::Hash,
	) -> Result<EncodedProof, Box<dyn std::error::Error + Send + Sync>> {
		let proof = WarpSyncProof::<Block>::generate(
			&*self.backend,
			start,
			&self.authority_set.authority_set_changes(),
			&self.hard_forks,
		)
		.map_err(Box::new)?;
		Ok(EncodedProof(proof.encode()))
	}

	fn create_verifier(&self) -> Box<dyn Verifier<Block>> {
		let authority_set = self.authority_set.inner();
		let genesis_hash = self.backend.blockchain().info().genesis_hash;
		Box::new(GrandpaVerifier {
			state: VerifierState {
				set_id: authority_set.set_id,
				authorities: authority_set.current_authorities.clone(),
				next_proof_context: genesis_hash,
			},
			hard_forks: self.hard_forks.clone(),
			eras_synced: 0,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::{Error, HardForks, WarpSyncProof};
	use crate::{
		find_scheduled_change, AuthoritySetChanges, AuthoritySetHardFork, GrandpaJustification,
	};
	use codec::Encode;
	use rand::prelude::*;
	use sc_block_builder::BlockBuilderBuilder;
	use sp_blockchain::HeaderBackend;
	use sp_consensus::BlockOrigin;
	use sp_consensus_grandpa::{AuthorityList, SetId, GRANDPA_ENGINE_ID};
	use sp_keyring::Ed25519Keyring;
	use sp_runtime::traits::{Block as BlockT, Header as _};
	use std::sync::Arc;
	use substrate_test_runtime_client::{
		runtime::Block, Backend as TestBackend, BlockBuilderExt, ClientBlockImportExt, ClientExt,
		DefaultTestClientBuilderExt, TestClientBuilder, TestClientBuilderExt,
	};

	type BlockHash = <Block as BlockT>::Hash;

	struct TestCheckpoint {
		set_id: SetId,
		block: (BlockHash, u64),
		authorities: AuthorityList,
	}

	impl TestCheckpoint {
		fn hard_fork(&self) -> AuthoritySetHardFork<Block> {
			AuthoritySetHardFork {
				set_id: self.set_id,
				block: self.block,
				authorities: self.authorities.clone(),
				last_finalized: None,
			}
		}
	}

	struct TestChain {
		backend: Arc<TestBackend>,
		genesis_hash: BlockHash,
		genesis_authorities: AuthorityList,
		expected_set_id: SetId,
		expected_authorities: AuthorityList,
		change_records: Vec<(SetId, u64)>,
		scheduled_checkpoint: TestCheckpoint,
		offchain_checkpoint: TestCheckpoint,
	}

	impl TestChain {
		fn authority_set_changes(&self) -> AuthoritySetChanges<u64> {
			AuthoritySetChanges::from(self.change_records.clone())
		}

		fn incomplete_authority_set_changes(&self) -> AuthoritySetChanges<u64> {
			// Model a node that warp-synced to the checkpoint and therefore has no historical
			// prefix, while retaining the bad lower-set record seen on the affected chain.
			let mut changes = self
				.change_records
				.iter()
				.cloned()
				.filter(|(set_id, _)| *set_id >= self.offchain_checkpoint.set_id)
				.collect::<Vec<_>>();
			changes.push((0, self.offchain_checkpoint.block.1));
			changes.sort_unstable_by_key(|(_, block)| *block);
			AuthoritySetChanges::from(changes)
		}

		fn verify(
			&self,
			proof: &WarpSyncProof<Block>,
			hard_forks: &HardForks<Block>,
		) -> (SetId, AuthorityList) {
			proof.verify(0, self.genesis_authorities.clone(), hard_forks).unwrap()
		}
	}

	fn test_chain() -> TestChain {
		let mut rng = rand::rngs::StdRng::from_seed([0; 32]);
		let builder = TestClientBuilder::new();
		let backend = builder.backend();
		let client = Arc::new(builder.build());

		let available_authorities = Ed25519Keyring::iter().collect::<Vec<_>>();
		let genesis_authorities = vec![(Ed25519Keyring::Alice.public().into(), 1)];

		let mut current_authorities = vec![Ed25519Keyring::Alice];
		let mut current_set_id = 0;
		let mut authority_set_changes = Vec::new();
		let mut scheduled_checkpoint = None;
		let mut offchain_checkpoint = None;

		for n in 1..=100 {
			let mut builder = BlockBuilderBuilder::new(&*client)
				.on_parent_block(client.chain_info().best_hash)
				.with_parent_block_number(client.chain_info().best_number)
				.build()
				.unwrap();
			let mut new_authorities = None;

			// we will trigger an authority set change every 10 blocks
			if n != 0 && n % 10 == 0 {
				// pick next authorities and add digest for the set change
				let n_authorities = rng.gen_range(1..available_authorities.len());
				let next_authorities = available_authorities
					.choose_multiple(&mut rng, n_authorities)
					.cloned()
					.collect::<Vec<_>>();

				new_authorities = Some(next_authorities.clone());

				let next_authorities = next_authorities
					.iter()
					.map(|keyring| (keyring.public().into(), 1))
					.collect::<Vec<_>>();

				let digest = sp_runtime::generic::DigestItem::Consensus(
					sp_consensus_grandpa::GRANDPA_ENGINE_ID,
					sp_consensus_grandpa::ConsensusLog::ScheduledChange(
						sp_consensus_grandpa::ScheduledChange { delay: 0u64, next_authorities },
					)
					.encode(),
				);

				builder.push_deposit_log_digest_item(digest).unwrap();
			}

			let block = builder.build().unwrap().block;

			futures::executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();

			if new_authorities.is_some() || n == 55 {
				// Generate a justification for every recorded change and for a hard-fork checkpoint
				// that deliberately has no runtime change digest.
				let (target_hash, target_number) = {
					let info = client.info();
					(info.best_hash, info.best_number)
				};

				let mut precommits = Vec::new();
				for keyring in &current_authorities {
					let precommit = finality_grandpa::Precommit { target_hash, target_number };

					let msg = finality_grandpa::Message::Precommit(precommit.clone());
					let encoded = sp_consensus_grandpa::localized_payload(42, current_set_id, &msg);
					let signature = keyring.sign(&encoded[..]).into();

					let precommit = finality_grandpa::SignedPrecommit {
						precommit,
						signature,
						id: keyring.public().into(),
					};

					precommits.push(precommit);
				}

				let commit = finality_grandpa::Commit { target_hash, target_number, precommits };

				let justification = GrandpaJustification::from_commit(&client, 42, commit).unwrap();

				if n == 50 {
					scheduled_checkpoint = Some(TestCheckpoint {
						set_id: current_set_id,
						block: (target_hash, target_number),
						authorities: current_authorities
							.iter()
							.map(|keyring| (keyring.public().into(), 1))
							.collect(),
					});
				}
				if n == 55 {
					offchain_checkpoint = Some(TestCheckpoint {
						set_id: current_set_id,
						block: (target_hash, target_number),
						authorities: current_authorities
							.iter()
							.map(|keyring| (keyring.public().into(), 1))
							.collect(),
					});
				}

				client
					.finalize_block(target_hash, Some((GRANDPA_ENGINE_ID, justification.encode())))
					.unwrap();
			}

			if let Some(new_authorities) = new_authorities {
				authority_set_changes.push((current_set_id, n));

				current_set_id += 1;
				current_authorities = new_authorities;
			}
		}

		let genesis_hash = client.hash(0).unwrap().unwrap();
		let expected_authorities =
			current_authorities.iter().map(|keyring| (keyring.public().into(), 1)).collect();

		TestChain {
			backend,
			genesis_hash,
			genesis_authorities,
			expected_set_id: current_set_id,
			expected_authorities,
			change_records: authority_set_changes,
			scheduled_checkpoint: scheduled_checkpoint
				.expect("block 50 is an authority-set transition"),
			offchain_checkpoint: offchain_checkpoint
				.expect("block 55 is a justified off-chain checkpoint"),
		}
	}

	#[test]
	fn warp_sync_proof_generate_verify() {
		let chain = test_chain();
		let authority_set_changes = chain.authority_set_changes();

		let hard_forks = HardForks::new_hard_forked_authorities(vec![]);
		let warp_sync_proof = WarpSyncProof::generate(
			&*chain.backend,
			chain.genesis_hash,
			&authority_set_changes,
			&hard_forks,
		)
		.unwrap();
		let legacy_proof = warp_sync_proof.encode();

		// Reinitializing the starting set ID is Subtensor mainnet's existing mode. Supplying that
		// mode must not alter which proof fragments the server generates.
		let reinitialized_set_id = HardForks::new_initial_set_id(3);
		let reinitialized_proof = WarpSyncProof::generate(
			&*chain.backend,
			chain.genesis_hash,
			&authority_set_changes,
			&reinitialized_set_id,
		)
		.unwrap();
		assert_eq!(reinitialized_proof.encode(), legacy_proof);

		// verifying the proof should yield the last set id and authorities
		let (new_set_id, new_authorities) = chain.verify(&warp_sync_proof, &hard_forks);
		assert_eq!(new_set_id, chain.expected_set_id);
		assert_eq!(new_authorities, chain.expected_authorities);
	}

	#[test]
	fn warp_sync_uses_scheduled_checkpoint_with_incomplete_history() {
		let chain = test_chain();
		// A trusted checkpoint replaces earlier authority-set history during both proof generation
		// and verification. This allows a chain to skip historical transitions that cannot produce
		// a valid warp fragment without changing the proof's wire format.
		let hard_fork = chain.scheduled_checkpoint.hard_fork();
		let noncanonical_hard_fork = AuthoritySetHardFork {
			set_id: 99,
			block: (Default::default(), hard_fork.block.1),
			authorities: hard_fork.authorities.clone(),
			last_finalized: None,
		};
		let future_hard_fork = AuthoritySetHardFork {
			set_id: 10,
			block: (Default::default(), 110),
			authorities: hard_fork.authorities.clone(),
			last_finalized: None,
		};
		let hard_forks = HardForks::new_hard_forked_authorities(vec![
			hard_fork,
			noncanonical_hard_fork,
			future_hard_fork,
		]);
		let authority_set_changes = chain.incomplete_authority_set_changes();
		let warp_sync_proof = WarpSyncProof::generate(
			&*chain.backend,
			chain.genesis_hash,
			&authority_set_changes,
			&hard_forks,
		)
		.unwrap();
		assert_eq!(*warp_sync_proof.proofs[0].header.number(), 50);
		assert_eq!(
			warp_sync_proof
				.proofs
				.iter()
				.filter(|proof| *proof.header.number() == 50)
				.count(),
			1,
		);

		let (new_set_id, new_authorities) = chain.verify(&warp_sync_proof, &hard_forks);
		assert_eq!(new_set_id, chain.expected_set_id);
		assert_eq!(new_authorities, chain.expected_authorities);
	}

	#[test]
	fn warp_sync_uses_latest_checkpoint_without_change_digest() {
		let chain = test_chain();
		// A real hard-fork checkpoint is defined off-chain and therefore does not need a runtime
		// scheduled-change digest. Only the latest canonical checkpoint is needed: the verifier can
		// switch directly to its configured authorities before verifying later fragments.
		let hard_forks = HardForks::new_hard_forked_authorities(vec![
			chain.scheduled_checkpoint.hard_fork(),
			chain.offchain_checkpoint.hard_fork(),
		]);
		let authority_set_changes = chain.incomplete_authority_set_changes();
		let warp_sync_proof = WarpSyncProof::generate(
			&*chain.backend,
			chain.genesis_hash,
			&authority_set_changes,
			&hard_forks,
		)
		.unwrap();
		assert_eq!(*warp_sync_proof.proofs[0].header.number(), 55);
		assert!(find_scheduled_change::<Block>(&warp_sync_proof.proofs[0].header).is_none());
		assert!(
			warp_sync_proof
				.proofs
				.last()
				.expect("proof contains the checkpoint")
				.header
				.number() > &55,
		);
		assert!(!warp_sync_proof.proofs.iter().any(|proof| *proof.header.number() == 50));

		let (new_set_id, new_authorities) = chain.verify(&warp_sync_proof, &hard_forks);
		assert_eq!(new_set_id, chain.expected_set_id);
		assert_eq!(new_authorities, chain.expected_authorities);
	}

	#[test]
	fn warp_sync_rejects_gapped_checkpoint_suffix() {
		let chain = test_chain();
		// A checkpoint can replace missing history before it, but not missing authority sets after
		// it. Unit tests on `AuthoritySetChanges` cover both leading and internal gaps; keep one
		// end-to-end assertion here to prove the generator maps that condition to `MissingData`.
		let expected_first_set = chain.offchain_checkpoint.set_id;
		let changes = AuthoritySetChanges::from(
			chain
				.change_records
				.iter()
				.cloned()
				.filter(|(set_id, _)| *set_id >= expected_first_set + 2)
				.collect::<Vec<_>>(),
		);
		let hard_forks =
			HardForks::new_hard_forked_authorities(vec![chain.offchain_checkpoint.hard_fork()]);
		assert!(matches!(
			WarpSyncProof::generate(&*chain.backend, chain.genesis_hash, &changes, &hard_forks,),
			Err(Error::MissingData),
		));
	}
}
