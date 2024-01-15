// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Cumulus.

// Cumulus is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Cumulus is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Cumulus.  If not, see <http://www.gnu.org/licenses/>.

//! This provides the option to run a basic relay-chain driven Aura implementation.
//!
//! This collator only builds on top of the most recently included block, limiting the
//! block time to a maximum of two times the relay-chain block time, and requiring the
//! block to be built and distributed to validators between two relay-chain blocks.
//!
//! For more information about AuRa, the Substrate crate should be checked.

use super::is_para_scheduled;
use crate::collator as collator_util;
use codec::{Codec, Decode};
use cumulus_client_collator::{
	relay_chain_driven::CollationRequest, service::ServiceInterface as CollatorServiceInterface,
};
use cumulus_client_consensus_common::ParachainBlockImportMarker;
use cumulus_client_consensus_proposer::ProposerInterface;
use cumulus_primitives_core::{
	extract_relay_parent,
	relay_chain::{BlockId as RBlockId, Hash as RHash},
	CollectCollationInfo, ParachainBlockData,
};
use cumulus_relay_chain_interface::RelayChainInterface;
use futures::{channel::mpsc::Receiver, prelude::*};
use polkadot_node_primitives::{BlockData, Collation, CollationResult, MaybeCompressedPoV, PoV};
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{CollatorPair, Id as ParaId, OccupiedCoreAssumption};
use sc_client_api::{backend::AuxStore, Backend, BlockBackend, BlockOf, StorageProof};
use sc_consensus::BlockImport;
use sp_api::{ApiExt, Core as _, ProvideRuntimeApi};
use sp_application_crypto::AppPublic;
use sp_blockchain::{Backend as _, HeaderBackend};
use sp_consensus::SyncOracle;
use sp_consensus_aura::{AuraApi, SlotDuration};
use sp_core::crypto::Pair;
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::KeystorePtr;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, Member};
use std::{
	collections::{HashMap, VecDeque},
	convert::TryFrom,
	sync::Arc,
	time::Duration,
};

/// Parameters for [`run`].
pub struct Params<BI, CIDP, Client, RClient, SO, Proposer, CS> {
	/// Inherent data providers. Only non-consensus inherent data should be provided, i.e.
	/// the timestamp, slot, and paras inherents should be omitted, as they are set by this
	/// collator.
	pub create_inherent_data_providers: CIDP,
	/// Used to actually import blocks.
	pub block_import: BI,
	/// The underlying para client.
	pub para_client: Arc<Client>,
	/// A handle to the relay-chain client.
	pub relay_client: RClient,
	/// A chain synchronization oracle.
	pub sync_oracle: SO,
	/// The underlying keystore, which should contain Aura consensus keys.
	pub keystore: KeystorePtr,
	/// The collator key used to sign collations before submitting to validators.
	pub collator_key: CollatorPair,
	/// The para's ID.
	pub para_id: ParaId,
	/// A handle to the relay-chain client's "Overseer" or task orchestrator.
	pub overseer_handle: OverseerHandle,
	/// The length of slots in this chain.
	pub slot_duration: SlotDuration,
	/// The length of slots in the relay chain.
	pub relay_chain_slot_duration: Duration,
	/// The underlying block proposer this should call into.
	pub proposer: Proposer,
	/// The generic collator service used to plug into this consensus engine.
	pub collator_service: CS,
	/// The amount of time to spend authoring each block.
	pub authoring_duration: Duration,
	/// Receiver for collation requests. If `None`, Aura consensus will establish a new receiver.
	/// Should be used when a chain migrates from a different consensus algorithm and was already
	/// processing collation requests before initializing Aura.
	pub collation_request_receiver: Option<Receiver<CollationRequest>>,
}

/// Run bare Aura consensus as a relay-chain-driven collator.
pub fn run<Block, P, BI, CIDP, Client, RClient, SO, Proposer, CS>(
	params: Params<BI, CIDP, Client, RClient, SO, Proposer, CS>,
) -> impl Future<Output = ()> + Send + 'static
where
	Block: BlockT + Send,
	Client: ProvideRuntimeApi<Block>
		+ BlockOf
		+ AuxStore
		+ HeaderBackend<Block>
		+ BlockBackend<Block>
		+ Send
		+ Sync
		+ 'static,
	Client::Api: AuraApi<Block, P::Public> + CollectCollationInfo<Block>,
	RClient: RelayChainInterface + Send + Clone + 'static,
	CIDP: CreateInherentDataProviders<Block, ()> + Send + 'static,
	CIDP::InherentDataProviders: Send,
	BI: BlockImport<Block> + ParachainBlockImportMarker + Send + Sync + 'static,
	SO: SyncOracle + Send + Sync + Clone + 'static,
	Proposer: ProposerInterface<Block> + Send + Sync + 'static,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	P: Pair,
	P::Public: AppPublic + Member + Codec,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	async move {
		let mut collation_requests = match params.collation_request_receiver {
			Some(receiver) => receiver,
			None =>
				cumulus_client_collator::relay_chain_driven::init(
					params.collator_key,
					params.para_id,
					params.overseer_handle,
				)
				.await,
		};

		let mut collator = {
			let params = collator_util::Params {
				create_inherent_data_providers: params.create_inherent_data_providers,
				block_import: params.block_import,
				relay_client: params.relay_client.clone(),
				keystore: params.keystore.clone(),
				para_id: params.para_id,
				proposer: params.proposer,
				collator_service: params.collator_service,
			};

			collator_util::Collator::<Block, P, _, _, _, _, _>::new(params)
		};

		while let Some(request) = collation_requests.next().await {
			macro_rules! reject_with_error {
				($err:expr) => {{
					request.complete(None);
					tracing::error!(target: crate::LOG_TARGET, err = ?{ $err });
					continue;
				}};
			}

			macro_rules! try_request {
				($x:expr) => {{
					match $x {
						Ok(x) => x,
						Err(e) => reject_with_error!(e),
					}
				}};
			}

			let validation_data = request.persisted_validation_data();

			let parent_header =
				try_request!(Block::Header::decode(&mut &validation_data.parent_head.0[..]));

			let parent_hash = parent_header.hash();

			if !collator.collator_service().check_block_status(parent_hash, &parent_header) {
				continue
			}

			let relay_parent_header =
				match params.relay_client.header(RBlockId::hash(*request.relay_parent())).await {
					Err(e) => reject_with_error!(e),
					Ok(None) => continue, // sanity: would be inconsistent to get `None` here
					Ok(Some(h)) => h,
				};

			let claim = match collator_util::claim_slot::<_, _, P>(
				&*params.para_client,
				parent_hash,
				&relay_parent_header,
				params.slot_duration,
				params.relay_chain_slot_duration,
				&params.keystore,
			)
			.await
			{
				Ok(None) => continue,
				Ok(Some(c)) => c,
				Err(e) => reject_with_error!(e),
			};

			let (parachain_inherent_data, other_inherent_data) = try_request!(
				collator
					.create_inherent_data(
						*request.relay_parent(),
						&validation_data,
						parent_hash,
						claim.timestamp(),
					)
					.await
			);

			let (collation, _, post_hash) = try_request!(
				collator
					.collate(
						&parent_header,
						&claim,
						None,
						(parachain_inherent_data, other_inherent_data),
						params.authoring_duration,
						// Set the block limit to 50% of the maximum PoV size.
						//
						// TODO: If we got benchmarking that includes the proof size,
						// we should be able to use the maximum pov size.
						(validation_data.max_pov_size / 2) as usize,
					)
					.await
			);

			let result_sender = Some(collator.collator_service().announce_with_barrier(post_hash));
			request.complete(Some(CollationResult { collation, result_sender }));
		}
	}
}

async fn submit_collations<Block: BlockT, Client>(
	relay_client: impl RelayChainInterface,
	para_id: ParaId,
	mut overseer_handle: OverseerHandle,
	client: Arc<Client>,
	backend: impl Backend<Block>,
) where
	Client: ProvideRuntimeApi<Block>,
	Client::Api: CollectCollationInfo<Block> + ApiExt<Block>,
{
	let mut import_notifications = match relay_client.import_notification_stream().await {
		Ok(s) => s,
		Err(err) => {
			tracing::error!(
				target: crate::LOG_TARGET,
				?err,
				"Failed to initialize consensus: no relay chain import notification stream"
			);

			return
		},
	};

	while let Some(relay_chain_header) = import_notifications.next().await {
		let relay_parent = relay_chain_header.hash();

		if !is_para_scheduled(relay_parent, para_id, &mut overseer_handle).await {
			tracing::trace!(
				target: crate::LOG_TARGET,
				?relay_parent,
				%para_id,
				"Para is not scheduled on any core, skipping import notification",
			);

			continue
		}

		let included_header = match relay_client
			.persisted_validation_data(relay_parent, para_id, OccupiedCoreAssumption::Included)
			.await
		{
			Ok(Some(validation_data)) => validation_data,
			Ok(None) => {
				tracing::trace!(
				target: crate::LOG_TARGET,
					%para_id,
				"No validation found, para not scheduled?"
				);
				continue
			},
			Err(err) => {
				tracing::error!(
					target: crate::LOG_TARGET,
					%para_id,
					?err,
					"Failed to get `ValidationData`",
				);
				continue
			},
		};

		let included_header = match Block::Header::decode(&mut &included_header.parent_head.0[..]) {
			Err(err) => {
				tracing::error!(
					target: crate::LOG_TARGET,
					?err,
					%para_id,
					"Failed to decode para header fetched from the relay chain"
				);
				continue
			},
			Ok(x) => x,
		};

		let included_header_hash = included_header.hash();

		struct Info<Block: BlockT> {
			first: bool,
			children: Vec<Block::Hash>,
			relay_parent: RHash,
		}

		let mut collected = HashMap::<Block::Hash, Info<Block>>::new();

		let backend = backend.blockchain();

		let mut children = match backend.children(included_header_hash) {
			Ok(c) => c.into_iter().map(|c| (c, true)).collect::<Vec<_>>(),
			Err(_err) => continue,
		};

		while let Some((child, first)) = children.pop() {
			let c_header = match backend.expect_header(child) {
				Ok(h) => h,
				Err(_err) => continue,
			};

			let c_relay_parent = match extract_relay_parent(c_header.digest()) {
				Some(r) => r,
				None => continue,
			};

			if !first {
				let Some(parent) = collected.get_mut(c_header.parent_hash()) else {
					// Should not happen
					continue
				};

				// We can only include blocks that were build on the same relay chain block in a
				// collation.
				if parent.relay_parent != c_relay_parent {
					continue
				}

				parent.children.push(child);
			}

			collected.insert(
				child,
				Info::<Block> { first, children: Vec::new(), relay_parent: c_relay_parent },
			);

			children.extend(
				backend.children(child).ok().unwrap_or_default().into_iter().map(|c| (c, false)),
			);
		}

		let mut longest_chain = Vec::new();

		collected.iter().filter(|(_, v)| v.first).for_each(|(p, i)| {
			let mut to_process = i.children.iter().map(|c| (*c, vec![*p, *c])).collect::<Vec<_>>();

			while let Some((parent_hash, chain)) = to_process.pop() {
				let Some(parent) = collected.get(&parent_hash) else { continue };

				to_process.extend(parent.children.iter().map(|c| {
					let mut chain = chain.clone();
					chain.push(*c);
					(*c, chain)
				}));

				if parent.children.is_empty() && chain.len() > longest_chain.len() {
					longest_chain = chain;
				}
			}
		});

		let blocks = match longest_chain
			.into_iter()
			.map(|h| {
				let header = backend.expect_header(h)?;
				let body = backend
					.body(h)?
					//TODO
					.ok_or_else(|| sp_blockchain::Error::Backend("BODY NOT FOUND".into()))?;

				Ok::<_, sp_blockchain::Error>(Block::new(header, body))
			})
			.try_fold(Vec::new(), |mut acc, v| {
				acc.push(v?);
				Ok::<_, sp_blockchain::Error>(acc)
			}) {
			Ok(b) => b,
			Err(_err) => continue,
		};

		let mut collation_info = VecDeque::new();

		for block in blocks {
			let mut runtime_api = client.runtime_api();
			runtime_api.record_proof();
			if runtime_api.execute_block(*block.header().parent_hash(), block.clone()).is_err() {
				//TODO: Handle properly
				break
			}

			let info =
				match runtime_api.collect_collation_info(block.header().hash(), block.header()) {
					Ok(i) => i,
					Err(_e) => {
						// TODO: Handle me
						break
					},
				};

			collation_info.push_back((
				runtime_api.extract_proof().expect("We enabled proof recording; qed"),
				info,
			));
		}

		let (mut proofs, mut info) = match collation_info.pop_front() {
			Some((p, i)) => (vec![p], i),
			None => {
				// TODO: Is this problematic?
				continue
			},
		};

		collation_info.into_iter().for_each(|(p, i)| {
			proofs.push(p);

			info.upward_messages.extend(i.upward_messages);
			info.horizontal_messages.extend(i.horizontal_messages);
			// TODO: `validation_code` can actually be only once `Some`, this should already be
			// handled by the runtime, so not sure we need to check this here as the relay chain
			// would reject the blocks otherwise anyway.
			info.new_validation_code =
				info.new_validation_code.clone().or_else(|| i.new_validation_code.clone());
			// TODO: `processed_downward_messages` should always be Zero after the first block.
			//
			// As we build all blocks on the same relay chain block, there can not be anymore new
			// messages.
			info.processed_downward_messages += i.processed_downward_messages;
			// TODO: Watermark should also not change
			info.hrmp_watermark = i.hrmp_watermark;
			info.head_data = i.head_data;
		});

		let block_data =
			ParachainBlockData::new(blocks, StorageProof::merge(proofs).into_compact_proof());

		let pov = polkadot_node_primitives::maybe_compress_pov(PoV {
			block_data: BlockData(block_data.encode()),
		});

		let Some(upward_messages) = info
			.upward_messages
			.try_into()
			.map_err(|e| {
				tracing::error!(
					target: crate::LOG_TARGET,
					error = ?e,
					"Number of upward messages should not be greater than `MAX_UPWARD_MESSAGE_NUM`",
				)
			})
			.ok()
		else {
			continue
		};

		let Some(horizontal_messages) = info
			.horizontal_messages
			.try_into()
			.map_err(|e| {
				tracing::error!(
					target: crate::LOG_TARGET,
					error = ?e,
					"Number of horizontal messages should not be greater than `MAX_HORIZONTAL_MESSAGE_NUM`",
				)
			})
			.ok()
		else {
			continue
		};

		let collation = Collation {
			upward_messages,
			new_validation_code: info.new_validation_code,
			processed_downward_messages: info.processed_downward_messages,
			horizontal_messages,
			hrmp_watermark: info.hrmp_watermark,
			head_data: info.head_data,
			proof_of_validity: MaybeCompressedPoV::Compressed(pov),
		};
	}
}
