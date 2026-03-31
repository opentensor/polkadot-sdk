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
use sc_network_sync::strategy::warp::{EncodedProof, VerificationResult, WarpSyncProvider};
use sp_blockchain::{Backend as BlockchainBackend, HeaderBackend};
use sp_consensus_grandpa::{
	AuthorityList, SetId, CLIENT_LOG_TARGET as LOG_TARGET, GRANDPA_ENGINE_ID,
};
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, NumberFor, One},
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
	/// The finalized block that signaled the authority set change for this fragment.
	pub header: Block::Header,
	/// Headers in the range `(header; justification.target]`, ordered by block number.
	///
	/// These are only needed when the block that signaled the authority set change does not itself
	/// carry a stored justification. Finality of the justification target implies finality of all
	/// ancestors in this range, including `header`.
	pub descendant_headers: Vec<Block::Header>,
	/// A justification for the header above which proves its finality. In order to validate it the
	/// verifier must be aware of the authorities and set id for which the justification refers to.
	pub justification: GrandpaJustification<Block>,
}

impl<Block: BlockT> WarpSyncFragment<Block> {
	fn target_header(&self) -> Result<&Block::Header, Error> {
		let (target_number, target_hash) = self.justification.target();

		if let Some(last_header) = self.descendant_headers.last() {
			if last_header.hash() != target_hash || *last_header.number() != target_number {
				return Err(Error::InvalidProof(
					"Mismatch between descendant headers and justification".to_owned(),
				));
			}

			return Ok(last_header);
		}

		if self.header.hash() != target_hash || *self.header.number() != target_number {
			return Err(Error::InvalidProof("Mismatch between header and justification".to_owned()));
		}

		Ok(&self.header)
	}

	fn verify_descendant_headers(&self) -> Result<(), Error>
	where
		NumberFor<Block>: BlockNumberOps,
	{
		if self.descendant_headers.is_empty() {
			self.target_header()?;
			return Ok(());
		}

		let mut parent_hash = self.header.hash();
		let mut parent_number = *self.header.number();

		for header in &self.descendant_headers {
			if *header.parent_hash() != parent_hash ||
				*header.number() != parent_number + One::one()
			{
				return Err(Error::InvalidProof(
					"Descendant headers do not form a contiguous chain".to_string(),
				));
			}

			parent_hash = header.hash();
			parent_number = *header.number();
		}

		self.target_header()?;
		Ok(())
	}
}

/// An accumulated proof of multiple authority set changes.
#[derive(Decode, Encode)]
pub struct WarpSyncProof<Block: BlockT> {
	proofs: Vec<WarpSyncFragment<Block>>,
	is_finished: bool,
}

impl<Block: BlockT> WarpSyncProof<Block> {
	/// Generates a warp sync proof starting at the given block. It will generate authority set
	/// change proofs for all changes that happened from `begin` until the current authority set
	/// (capped by MAX_WARP_SYNC_PROOF_SIZE).
	fn generate<Backend>(
		backend: &Backend,
		begin: Block::Hash,
		set_changes: &AuthoritySetChanges<NumberFor<Block>>,
	) -> Result<WarpSyncProof<Block>, Error>
	where
		Backend: ClientBackend<Block>,
		NumberFor<Block>: BlockNumberOps,
	{
		// TODO: cache best response (i.e. the one with lowest begin_number)
		let blockchain = backend.blockchain();

		let begin_number = blockchain
			.block_number_from_id(&BlockId::Hash(begin))?
			.ok_or_else(|| Error::InvalidRequest("Missing start block".to_string()))?;

		if begin_number > blockchain.info().finalized_number {
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

		let mut proofs = Vec::new();
		let mut proofs_encoded_len = 0;
		let mut proof_limit_reached = false;
		let mut lower_bound = begin_number;

		let set_changes = set_changes.iter_from(begin_number).ok_or(Error::MissingData)?;

		for (_, last_block) in set_changes {
			let (header, descendant_headers) =
				match find_fragment_header::<Block, _>(blockchain, lower_bound, *last_block)? {
					Some(fragment) => fragment,
					None => {
						// If we could not walk back to a scheduling digest inside this authority
						// set, the transition was either forced or the required history is
						// missing. In either case we stop extending the trust chain here.
						break;
					},
				};

			let target_header = descendant_headers.last().unwrap_or(&header);

			let justification = blockchain
				.justifications(target_header.hash())?
				.and_then(|just| just.into_justification(GRANDPA_ENGINE_ID))
				.ok_or_else(|| Error::MissingData)?;

			let justification = GrandpaJustification::<Block>::decode_all(&mut &justification[..])?;

			let proof = WarpSyncFragment { header, descendant_headers, justification };
			let proof_size = proof.encoded_size();

			// Check for the limit. We remove some bytes from the maximum size, because we're only
			// counting the size of the `WarpSyncFragment`s. The extra margin is here to leave
			// room for rest of the data (the size of the `Vec` and the boolean).
			if proofs_encoded_len + proof_size >= MAX_WARP_SYNC_PROOF_SIZE - 50 {
				proof_limit_reached = true;
				break;
			}

			proofs_encoded_len += proof_size;
			proofs.push(proof);
			lower_bound = *last_block;
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

				let proof = WarpSyncFragment {
					header,
					descendant_headers: Vec::new(),
					justification: latest_justification,
				};

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

			if let Some((set_id, list)) = hard_forks.get_hard_forked_authorities(&(hash, number)) {
				current_set_id = set_id;
				current_authorities = list.clone();
			} else if let Some(initial_set_id) = hard_forks.get_new_initial_set_id() {
				current_set_id += initial_set_id;
			}
			{
				proof.verify_descendant_headers()?;
				proof
					.justification
					.verify(current_set_id, &current_authorities)
					.map_err(|err| Error::InvalidProof(err.to_string()))?;

				if let Some(scheduled_change) = find_scheduled_change::<Block>(&proof.header) {
					current_authorities = scheduled_change.next_authorities;
					current_set_id += 1;
				} else if fragment_num != self.proofs.len() - 1 || !self.is_finished {
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

fn expect_header<Block, Backend>(
	blockchain: &Backend,
	block_number: NumberFor<Block>,
) -> Result<Block::Header, Error>
where
	Block: BlockT,
	Backend: BlockchainBackend<Block>,
{
	let hash = blockchain.block_hash_from_id(&BlockId::Number(block_number))?.ok_or_else(|| {
		log::debug!(target: LOG_TARGET, "Ignorning warp proof with invalid block number.");
		Error::InvalidRequest(
			"header number comes from previously applied set changes; corresponding hash must exist in db."
				.to_string(),
		)
	})?;

	blockchain.header(hash)?.ok_or_else(|| {
		log::debug!(target: LOG_TARGET, "Ignorning warp proof with invalid block hash.");
		Error::InvalidRequest(
			"header hash obtained from header number exists in db; corresponding header must exist in db too."
				.to_string(),
		)
	})
}

fn find_fragment_header<Block, Backend>(
	blockchain: &Backend,
	lower_bound: NumberFor<Block>,
	last_block: NumberFor<Block>,
) -> Result<Option<(Block::Header, Vec<Block::Header>)>, Error>
where
	Block: BlockT,
	Backend: BlockchainBackend<Block>,
	NumberFor<Block>: BlockNumberOps,
{
	let mut current_header = expect_header::<Block, _>(blockchain, last_block)?;
	let mut descendant_headers = Vec::new();

	loop {
		if find_scheduled_change::<Block>(&current_header).is_some() {
			descendant_headers.reverse();
			return Ok(Some((current_header, descendant_headers)));
		}

		if *current_header.number() <= lower_bound {
			return Ok(None);
		}

		let parent_hash = *current_header.parent_hash();
		descendant_headers.push(current_header);
		current_header = blockchain.header(parent_hash)?.ok_or(Error::MissingData)?;
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
		)
		.map_err(Box::new)?;
		Ok(EncodedProof(proof.encode()))
	}

	fn verify(
		&self,
		proof: &EncodedProof,
		set_id: SetId,
		authorities: AuthorityList,
	) -> Result<VerificationResult<Block>, Box<dyn std::error::Error + Send + Sync>> {
		let EncodedProof(proof) = proof;
		let proof = WarpSyncProof::<Block>::decode_all(&mut proof.as_slice())
			.map_err(|e| format!("Proof decoding error: {:?}", e))?;
		let last_header = proof
			.proofs
			.last()
			.map(WarpSyncFragment::target_header)
			.transpose()
			.map_err(Box::new)?
			.cloned()
			.ok_or_else(|| "Empty proof".to_string())?;
		let (next_set_id, next_authorities) =
			proof.verify(set_id, authorities, &self.hard_forks).map_err(Box::new)?;
		if proof.is_finished {
			Ok(VerificationResult::<Block>::Complete(next_set_id, next_authorities, last_header))
		} else {
			Ok(VerificationResult::<Block>::Partial(
				next_set_id,
				next_authorities,
				last_header.hash(),
			))
		}
	}

	fn current_authorities(&self) -> AuthorityList {
		self.authority_set.inner().current_authorities.clone()
	}
}

#[cfg(test)]
mod tests {
	use super::{HardForks, WarpSyncProof};
	use crate::{AuthoritySetChanges, GrandpaJustification};
	use codec::{DecodeAll, Encode};
	use rand::prelude::*;
	use sc_block_builder::BlockBuilderBuilder;
	use sc_client_api::{apply_aux, BlockBackend, LockImportRun};
	use sp_blockchain::HeaderBackend;
	use sp_consensus::BlockOrigin;
	use sp_consensus_grandpa::GRANDPA_ENGINE_ID;
	use sp_keyring::Ed25519Keyring;
	use sp_runtime::traits::{Header as HeaderT, NumberFor};
	use std::{collections::VecDeque, sync::Arc};
	use substrate_test_runtime_client::{
		runtime::Block, BlockBuilderExt, ClientBlockImportExt, ClientExt,
		DefaultTestClientBuilderExt, TestClient, TestClientBuilder, TestClientBuilderExt,
	};

	fn finalize_with_justification(
		client: &Arc<TestClient>,
		current_set_id: u64,
		current_authorities: &[Ed25519Keyring],
		target_hash: <Block as sp_runtime::traits::Block>::Hash,
		target_number: NumberFor<Block>,
	) {
		let mut precommits = Vec::new();
		for keyring in current_authorities {
			let precommit = finality_grandpa::Precommit { target_hash, target_number };

			let msg = finality_grandpa::Message::Precommit(precommit.clone());
			let encoded = sp_consensus_grandpa::localized_payload(42, current_set_id, &msg);
			let signature = keyring.sign(&encoded[..]).into();

			precommits.push(finality_grandpa::SignedPrecommit {
				precommit,
				signature,
				id: keyring.public().into(),
			});
		}

		let commit = finality_grandpa::Commit { target_hash, target_number, precommits };
		let justification = GrandpaJustification::from_commit(client, 42, commit).unwrap();

		client
			.finalize_block(target_hash, Some((GRANDPA_ENGINE_ID, justification.encode())))
			.unwrap();
	}

	fn store_best_justification(client: &TestClient, just: &GrandpaJustification<Block>) {
		client
			.lock_import_and_run(|import_op| {
				crate::aux_schema::update_best_justification(just, |insert| {
					apply_aux(import_op, insert, &[])
				})
			})
			.unwrap();
	}

	#[test]
	fn warp_sync_proof_generate_verify() {
		let mut rng = rand::rngs::StdRng::from_seed([0; 32]);
		let builder = TestClientBuilder::new();
		let backend = builder.backend();
		let client = Arc::new(builder.build());

		let available_authorities = Ed25519Keyring::iter().collect::<Vec<_>>();
		let genesis_authorities = vec![(Ed25519Keyring::Alice.public().into(), 1)];

		let mut current_authorities = vec![Ed25519Keyring::Alice];
		let mut current_set_id = 0;
		let mut authority_set_changes = Vec::new();

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

			if let Some(new_authorities) = new_authorities {
				let (target_hash, target_number) = {
					let info = client.info();
					(info.best_hash, info.best_number)
				};
				finalize_with_justification(
					&client,
					current_set_id,
					&current_authorities,
					target_hash,
					target_number,
				);

				authority_set_changes.push((current_set_id, n));

				current_set_id += 1;
				current_authorities = new_authorities;
			}
		}

		let authority_set_changes = AuthoritySetChanges::from(authority_set_changes);

		// generate a warp sync proof
		let genesis_hash = client.hash(0).unwrap().unwrap();

		let warp_sync_proof =
			WarpSyncProof::generate(&*backend, genesis_hash, &authority_set_changes).unwrap();

		// verifying the proof should yield the last set id and authorities
		let no_hard_forks = HardForks::new_hard_forked_authorities(Vec::new());
		let (new_set_id, new_authorities) =
			warp_sync_proof.verify(0, genesis_authorities, &no_hard_forks).unwrap();

		let expected_authorities = current_authorities
			.iter()
			.map(|keyring| (keyring.public().into(), 1))
			.collect::<Vec<_>>();

		assert_eq!(new_set_id, current_set_id);
		assert_eq!(new_authorities, expected_authorities);
	}

	#[test]
	fn warp_sync_proof_generate_verify_with_delayed_changes() {
		let builder = TestClientBuilder::new();
		let backend = builder.backend();
		let client = Arc::new(builder.build());

		let genesis_authorities = vec![(Ed25519Keyring::Alice.public().into(), 1)];
		let mut current_authorities = vec![Ed25519Keyring::Alice];
		let mut current_set_id = 0;
		let mut authority_set_changes = Vec::new();
		let mut pending_changes = VecDeque::from([
			(10u64, 13u64, vec![Ed25519Keyring::Bob, Ed25519Keyring::Charlie]),
			(20u64, 22u64, vec![Ed25519Keyring::Dave, Ed25519Keyring::Eve]),
		]);

		for n in 1..=30u64 {
			let mut builder = BlockBuilderBuilder::new(&*client)
				.on_parent_block(client.chain_info().best_hash)
				.with_parent_block_number(client.chain_info().best_number)
				.build()
				.unwrap();

			if let Some((scheduled_at, effective_at, next_authorities)) = pending_changes.front() {
				if *scheduled_at == n {
					let next_authorities = next_authorities
						.iter()
						.map(|keyring| (keyring.public().into(), 1))
						.collect::<Vec<_>>();

					let digest = sp_runtime::generic::DigestItem::Consensus(
						sp_consensus_grandpa::GRANDPA_ENGINE_ID,
						sp_consensus_grandpa::ConsensusLog::ScheduledChange(
							sp_consensus_grandpa::ScheduledChange {
								delay: effective_at - scheduled_at,
								next_authorities,
							},
						)
						.encode(),
					);

					builder.push_deposit_log_digest_item(digest).unwrap();
				}
			}

			let block = builder.build().unwrap().block;
			futures::executor::block_on(client.import(BlockOrigin::Own, block)).unwrap();

			if let Some((_, effective_at, next_authorities)) = pending_changes.front() {
				if *effective_at == n {
					let (target_hash, target_number) = {
						let info = client.info();
						(info.best_hash, info.best_number)
					};

					finalize_with_justification(
						&client,
						current_set_id,
						&current_authorities,
						target_hash,
						target_number,
					);
					authority_set_changes.push((current_set_id, n));
					current_set_id += 1;
					current_authorities = next_authorities.clone();
					pending_changes.pop_front();
				}
			}
		}

		let (best_hash, best_number) = {
			let info = client.info();
			(info.best_hash, info.best_number)
		};
		finalize_with_justification(
			&client,
			current_set_id,
			&current_authorities,
			best_hash,
			best_number,
		);
		let best_justification = client
			.justifications(best_hash)
			.unwrap()
			.and_then(|just| just.into_justification(GRANDPA_ENGINE_ID))
			.map(|just| GrandpaJustification::<Block>::decode_all(&mut &just[..]).unwrap())
			.unwrap();
		store_best_justification(&client, &best_justification);

		let authority_set_changes = AuthoritySetChanges::from(authority_set_changes);
		let genesis_hash = client.hash(0).unwrap().unwrap();
		let warp_sync_proof =
			WarpSyncProof::generate(&*backend, genesis_hash, &authority_set_changes).unwrap();

		assert_eq!(warp_sync_proof.proofs.len(), 3);
		assert_eq!(*warp_sync_proof.proofs[0].header.number(), 10);
		assert_eq!(
			warp_sync_proof.proofs[0]
				.descendant_headers
				.iter()
				.map(|header| *header.number())
				.collect::<Vec<_>>(),
			vec![11, 12, 13],
		);
		assert_eq!(*warp_sync_proof.proofs[1].header.number(), 20);
		assert_eq!(
			warp_sync_proof.proofs[1]
				.descendant_headers
				.iter()
				.map(|header| *header.number())
				.collect::<Vec<_>>(),
			vec![21, 22],
		);
		assert_eq!(*warp_sync_proof.proofs[2].header.number(), 30);
		assert!(warp_sync_proof.proofs[2].descendant_headers.is_empty());

		let no_hard_forks = HardForks::new_hard_forked_authorities(Vec::new());
		let (new_set_id, new_authorities) =
			warp_sync_proof.verify(0, genesis_authorities, &no_hard_forks).unwrap();

		let expected_authorities = current_authorities
			.iter()
			.map(|keyring| (keyring.public().into(), 1))
			.collect::<Vec<_>>();

		assert_eq!(new_set_id, current_set_id);
		assert_eq!(new_authorities, expected_authorities);
	}
}
