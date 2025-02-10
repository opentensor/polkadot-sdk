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

use super::{
	core_selector, relay_chain_data_cache::RelayChainDataCache, CollatorMessage,
	SlotBasedBlockImportHandle,
};
use codec::Encode;
use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_client_consensus_common::{ParachainCandidate, ValidationCodeHashProvider};
use cumulus_primitives_core::{ClaimQueueOffset, GetCoreSelectorApi, PersistedValidationData};
use cumulus_relay_chain_interface::{RelayChainInterface, RelayChainResult};
use futures::prelude::*;
use polkadot_node_primitives::{MaybeCompressedPoV, SubmitCollationParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{
	CollatorPair, CoreIndex, Hash as RHash, Header as RHeader, Id as ParaId, ValidationCodeHash,
};
use sc_utils::mpsc::TracingUnboundedReceiver;
use schnellru::{ByLength, LruMap};
use sp_api::{ProvideRuntimeApi, StorageProof};
use sp_blockchain::HeaderBackend;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, One};
use std::{collections::HashMap, pin::Pin, sync::Arc};
use stream::{Fuse, FusedStream};

const LOG_TARGET: &str = "aura::cumulus::collation_task";

/// Parameters for the collation task.
pub struct Params<Block: BlockT, Client, RClient, CS, CHP> {
	/// A handle to the parachain client.
	pub para_client: Arc<Client>,
	/// A handle to the relay-chain client.
	pub relay_client: RClient,
	/// The collator key used to sign collations before submitting to validators.
	pub collator_key: CollatorPair,
	/// The para's ID.
	pub para_id: ParaId,
	/// Whether we should reinitialize the collator config (i.e. we are transitioning to aura).
	pub reinitialize: bool,
	/// Collator service interface
	pub collator_service: CS,
	/// Receiver channel for communication with the block builder task.
	pub collator_receiver: TracingUnboundedReceiver<CollatorMessage<Block>>,
	/// The handle from the special slot based block import.
	pub block_import_handle: super::SlotBasedBlockImportHandle<Block>,
	/// A validation code hash provider, used to get the current validation code hash.
	pub code_hash_provider: CHP,
}

/// Asynchronously executes the collation task for a parachain.
///
/// This function initializes the collator subsystems necessary for producing and submitting
/// collations to the relay chain. It listens for new best relay chain block notifications and
/// handles collator messages. If our parachain is scheduled on a core and we have a candidate,
/// the task will build a collation and send it to the relay chain.
pub async fn run_collation_task<Block, Client, RClient, CS, CHP>(
	Params {
		relay_client,
		collator_key,
		para_id,
		reinitialize,
		collator_service,
		collator_receiver,
		block_import_handle,
		para_client,
		code_hash_provider,
	}: Params<Block, Client, RClient, CS, CHP>,
) where
	Block: BlockT,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	RClient: RelayChainInterface + Clone + 'static,
	Client: ProvideRuntimeApi<Block> + HeaderBackend<Block> + Send + Sync,
	Client::Api: GetCoreSelectorApi<Block>,
	CHP: ValidationCodeHashProvider<Block::Hash> + Send + 'static,
{
	let Ok(mut overseer_handle) = relay_client.overseer_handle() else {
		tracing::error!(target: LOG_TARGET, "Failed to get overseer handle.");
		return
	};

	cumulus_client_collator::initialize_collator_subsystems(
		&mut overseer_handle,
		collator_key,
		para_id,
		reinitialize,
	)
	.await;

	match CollationTask::new(
		para_client,
		relay_client,
		code_hash_provider,
		para_id,
		collator_service,
		overseer_handle,
	)
	.await
	{
		Ok(task) => task.run(collator_receiver, block_import_handle).await,
		Err(error) => {
			tracing::error!(target: LOG_TARGET, ?error, "Failed to initialize collation task");
		},
	}
}

#[derive(Clone, Copy)]
enum AddOutcome {
	SubmitCollation,
	Nothing,
}

impl AddOutcome {
	fn submit_collation(self) -> bool {
		matches!(self, Self::SubmitCollation)
	}
}

struct PerCore<Block: BlockT> {
	blocks: Vec<ParachainCandidate<Block>>,
	parent_header: Block::Header,
}

struct PerRelayParent<Block: BlockT> {
	/// Parachain blocks build on top of this relay parent.
	parachain_blocks: HashMap<CoreIndex, PerCore<Block>>,
	/// Scheduled cores for this parachain at this relay parent.
	scheduled_cores: Vec<CoreIndex>,
	/// The number of blocks per core that are put into one collation.
	blocks_per_core: usize,
}

impl<Block: BlockT> PerRelayParent<Block> {
	fn new(
		scheduled_cores: Vec<CoreIndex>,
		parachain_slot_duration: u64,
		relay_chain_slot_duration: u64,
	) -> Self {
		let blocks_per_core = if parachain_slot_duration < relay_chain_slot_duration &&
			!scheduled_cores.is_empty()
		{
			(relay_chain_slot_duration / parachain_slot_duration) as usize / scheduled_cores.len()
		} else {
			// If the `parachain_slot_duration` is bigger than the `relay_chain_slot_duration`,
			// logically it only makes sense to have at maximum one block.
			scheduled_cores.len()
		};

		tracing::trace!(target: LOG_TARGET, %blocks_per_core, ?scheduled_cores, "Initializing `PerRelayParent`");

		Self { parachain_blocks: Default::default(), scheduled_cores, blocks_per_core }
	}

	fn add_parachain_block(
		&mut self,
		block: ParachainCandidate<Block>,
		parent_header: Block::Header,
		core: CoreIndex,
	) -> AddOutcome {
		if !self.scheduled_cores.contains(&core) {
			return AddOutcome::Nothing
		}

		//TODO: Handle forks?
		let per_core = self
			.parachain_blocks
			.entry(core)
			.or_insert_with(|| PerCore { blocks: Default::default(), parent_header });
		per_core.blocks.push(block);

		if per_core.blocks.len() == self.blocks_per_core {
			//TODO: Also use a time based approach, if we are at the end of a relay chain slot, we
			// should send all available blocks.
			AddOutcome::SubmitCollation
		} else {
			AddOutcome::Nothing
		}
	}
}

struct CollationTask<Block: BlockT, RI, CHP, Client, CS> {
	per_relay_parent: HashMap<RHash, PerRelayParent<Block>>,
	relay_import_notifications: Fuse<Pin<Box<dyn Stream<Item = RHeader> + Send>>>,
	code_hash_provider: CHP,
	para_client: Arc<Client>,
	relay_data_cache: RelayChainDataCache<RI>,
	/// Maps from relay chain storage root to relay chain block hash.
	root_to_hash: LruMap<RHash, RHash>,
	collator_service: CS,
	overseer_handle: OverseerHandle,
}

impl<Block, RI, CHP, Client, CS> CollationTask<Block, RI, CHP, Client, CS>
where
	Block: BlockT,
	RI: RelayChainInterface + Clone,
	CHP: ValidationCodeHashProvider<Block::Hash>,
	Client: ProvideRuntimeApi<Block> + HeaderBackend<Block> + Send + Sync,
	Client::Api: GetCoreSelectorApi<Block>,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
{
	async fn new(
		para_client: Arc<Client>,
		relay_interface: RI,
		code_hash_provider: CHP,
		para_id: ParaId,
		collator_service: CS,
		overseer_handle: OverseerHandle,
	) -> RelayChainResult<Self> {
		let relay_import_notifications = relay_interface.import_notification_stream().await?.fuse();
		let relay_data_cache = RelayChainDataCache::new(relay_interface, para_id);

		Ok(Self {
			per_relay_parent: Default::default(),
			relay_import_notifications,
			code_hash_provider,
			para_client,
			root_to_hash: LruMap::new(ByLength::new(50)),
			relay_data_cache,
			collator_service,
			overseer_handle,
		})
	}

	fn extract_relay_parent(&mut self, header: &Block::Header) -> Option<RHash> {
		let digest = header.digest();

		if let Some(relay_parent) = cumulus_primitives_core::extract_relay_parent(digest) {
			return Some(relay_parent)
		}

		if let Some(relay_parent) =
			cumulus_primitives_core::rpsr_digest::extract_relay_parent_storage_root(digest)
				.and_then(|(r, _)| self.root_to_hash.get(&r).cloned())
		{
			return Some(relay_parent)
		}

		None
	}

	fn relay_parent(
		&mut self,
		relay_parent: RHash,
		scheduled_cores: Vec<CoreIndex>,
	) -> &mut PerRelayParent<Block> {
		self.per_relay_parent
			.entry(relay_parent)
			.or_insert_with(|| PerRelayParent::new(scheduled_cores, 500, 6000))
	}

	/// Handle an incoming collation message from the block builder task.
	///
	/// This builds the collation from the [`CollatorMessage`] and submits it to
	/// the collation-generation subsystem of the relay chain.
	async fn handle_collation_message(&mut self, message: CollatorMessage<Block>) {
		let CollatorMessage {
			parent_header,
			parachain_candidate,
			validation_code_hash,
			relay_parent,
			core_index,
			scheduled_cores,
		} = message;

		let per_relay_parent = self.relay_parent(relay_parent, scheduled_cores);

		if per_relay_parent
			.add_parachain_block(parachain_candidate, parent_header, core_index)
			.submit_collation()
		{
			let per_core = per_relay_parent.parachain_blocks.get(&core_index).unwrap();
			let blocks = per_core.blocks.clone();
			let parent_header = per_core.parent_header.clone();
			self.submit_collation(
				blocks,
				parent_header,
				relay_parent,
				core_index,
				validation_code_hash,
			)
			.await;
		}
	}

	async fn submit_collation(
		&mut self,
		candidates: Vec<ParachainCandidate<Block>>,
		parent_header: Block::Header,
		relay_parent: RHash,
		core_index: CoreIndex,
		validation_code_hash: ValidationCodeHash,
	) {
		let parent_head_hash = parent_header.hash();
		let parent_head = parent_header.encode();
		let (collation, block_data) =
			match self.collator_service.build_collation(parent_header, candidates) {
				Some(collation) => collation,
				None => {
					tracing::warn!(target: LOG_TARGET, ?core_index, "Unable to build collation.");
					return;
				},
			};

		block_data.log_size_info();

		if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
			tracing::info!(
				target: LOG_TARGET,
				"Compressed PoV size: {}kb",
				pov.block_data.0.len() as f64 / 1024f64,
			);
		}

		tracing::debug!(target: LOG_TARGET, ?core_index, ?parent_head_hash, "Submitting collation for core.");
		self.overseer_handle
			.send_msg(
				CollationGenerationMessage::SubmitCollation(SubmitCollationParams {
					relay_parent,
					collation,
					parent_head: parent_head.into(),
					validation_code_hash,
					core_index,
					result_sender: None,
				}),
				"SubmitCollation",
			)
			.await;
	}

	async fn handle_block_import(
		&mut self,
		block: Block,
		storage_proof: StorageProof,
		relay_parent: RHash,
	) {
		let parent_hash = *block.header().parent_hash();

		// Retrieve the core selector.
		let (core_selector, claim_queue_offset) = match core_selector(
			&*self.para_client,
			parent_hash,
			*block.header().number() - One::one(),
		) {
			Ok(core_selector) => core_selector,
			Err(err) => {
				tracing::trace!(
					target: crate::LOG_TARGET,
					"Unable to retrieve the core selector from the runtime API: {}",
					err
				);
				return
			},
		};

		let Some(relay_data) = self
			.relay_data_cache
			.get_mut_relay_chain_data(relay_parent, claim_queue_offset)
			.await
		else {
			tracing::debug!(target: LOG_TARGET, block_hash = ?block.hash(), "Failed to retrieve required relay chain data");
			return
		};

		let core_selector = core_selector.0 as usize % relay_data.scheduled_cores.len();
		let Some(core_index) = relay_data.scheduled_cores.get(core_selector).cloned() else {
			// This cannot really happen, as we modulo the core selector with the
			// scheduled_cores length and we check that the scheduled_cores is not empty.
			return;
		};

		let Some(validation_code_hash) = self.code_hash_provider.code_hash_at(parent_hash) else {
			tracing::error!(target: crate::LOG_TARGET, ?parent_hash, "Could not fetch validation code hash");
			return
		};

		let parent_header = self.para_client.header(parent_hash).unwrap().unwrap();
		let scheduled_cores = relay_data.scheduled_cores.clone();

		let per_relay_parent = self.relay_parent(relay_parent, scheduled_cores);

		per_relay_parent.add_parachain_block(
			ParachainCandidate { block, proof: storage_proof },
			parent_header,
			core_index,
		);
	}

	/// Execute the collation task.
	async fn run(
		mut self,
		mut collator_receiver: TracingUnboundedReceiver<CollatorMessage<Block>>,
		mut block_import_handle: SlotBasedBlockImportHandle<Block>,
	) {
		loop {
			futures::select! {
				collator_message = collator_receiver.next() => {
					let Some(message) = collator_message else {
						return;
					};

					tracing::trace!(
						target: LOG_TARGET,
						block = ?message.parachain_candidate.block.hash(),
						"Received block from block production",
					);

					self.handle_collation_message(message).await;
				},
				(block, storage_proof) = block_import_handle.next().fuse() => {
					let Some(relay_parent) = self.extract_relay_parent(block.header())
					else {
						tracing::debug!(
							target: LOG_TARGET,
							block_hash = ?block.hash(),
							"Could not extract relay parent from parachain block",
						);
						continue
					};

					tracing::trace!(target: LOG_TARGET, block = ?block.hash(), "Received block from block import");

					self.handle_block_import(block, storage_proof, relay_parent).await
				},
				relay_block_import = self.relay_import_notifications.next() => {
					let Some(relay_block_import) = relay_block_import else { return; };

					let state_root = *relay_block_import.state_root();
					let relay_block =  relay_block_import.hash();

					tracing::trace!(target: LOG_TARGET, ?relay_block, ?state_root, "Relay block mapped");

					self.root_to_hash.insert(state_root, relay_block);
				}
			}
		}
	}
}
