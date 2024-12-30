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

use super::{core_selector, relay_chain_data_cache::RelayChainDataCache, CollatorMessage};
use codec::Encode;
use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_primitives_core::{GetCoreSelectorApi, PersistedValidationData};
use cumulus_relay_chain_interface::{RelayChainInterface, RelayChainResult};
use futures::prelude::*;
use polkadot_node_primitives::{MaybeCompressedPoV, SubmitCollationParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{CollatorPair, Hash as RHash, Header as RHeader, Id as ParaId};
use sc_utils::mpsc::TracingUnboundedReceiver;
use schnellru::{ByLength, LruMap};
use sp_api::ProvideRuntimeApi;
use sp_runtime::traits::{Block as BlockT, Header as HeaderT, One};
use std::{pin::Pin, sync::Arc};

const LOG_TARGET: &str = "aura::cumulus::collation_task";

/// Parameters for the collation task.
pub struct Params<Block: BlockT, Client, RClient, CS> {
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
}

/// Asynchronously executes the collation task for a parachain.
///
/// This function initializes the collator subsystems necessary for producing and submitting
/// collations to the relay chain. It listens for new best relay chain block notifications and
/// handles collator messages. If our parachain is scheduled on a core and we have a candidate,
/// the task will build a collation and send it to the relay chain.
pub async fn run_collation_task<Block, Client, RClient, CS>(
	Params {
		relay_client,
		collator_key,
		para_id,
		reinitialize,
		collator_service,
		mut collator_receiver,
		mut block_import_handle,
		para_client,
	}: Params<Block, Client, RClient, CS>,
) where
	Block: BlockT,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	RClient: RelayChainInterface + Clone + 'static,
	Client: ProvideRuntimeApi<Block> + Send + Sync,
	Client::Api: GetCoreSelectorApi<Block>,
{
	let Ok(mut overseer_handle) = relay_client.overseer_handle() else {
		tracing::error!(target: LOG_TARGET, "Failed to get overseer handle.");
		return
	};

	let mut relay_storage_root_to_hash = match RelayStorageRootToBlockHash::new(&relay_client).await
	{
		Ok(r) => r,
		Err(error) => {
			tracing::error!(target: LOG_TARGET, ?error, "Failed to get relay chain import notifications stream");
			return
		},
	};

	cumulus_client_collator::initialize_collator_subsystems(
		&mut overseer_handle,
		collator_key,
		para_id,
		reinitialize,
	)
	.await;

	let mut relay_chain_data_cache = RelayChainDataCache::new(relay_client.clone(), para_id);

	loop {
		futures::select! {
			collator_message = collator_receiver.next() => {
				let Some(message) = collator_message else {
					return;
				};

				handle_collation_message(message, &collator_service, &mut overseer_handle).await;
			},
			(block, storage_proof) = block_import_handle.next().fuse() => {
				let Some(relay_parent) = extract_relay_parent(block.header(), &mut relay_storage_root_to_hash) else {
					tracing::debug!(target: LOG_TARGET, block_hash = ?block.hash(), "Could not extract relay parent from parachain block");
					continue
				};

				// Retrieve the core selector.
				let (core_selector, claim_queue_offset) =
					match core_selector(&*para_client, *block.header().parent_hash(), *block.header().number() - One::one()) {
						Ok(core_selector) => core_selector,
						Err(err) => {
							tracing::trace!(
								target: crate::LOG_TARGET,
								"Unable to retrieve the core selector from the runtime API: {}",
								err
							);
							continue
						},
					};

				let Some(relay_data) = relay_chain_data_cache.get_mut_relay_chain_data(relay_parent, claim_queue_offset).await else {
					tracing::debug!(target: LOG_TARGET, block_hash = ?block.hash(), "Failed to retrieve required relay chain data");
					continue
				};


				let core_selector = core_selector.0 as usize % scheduled_cores.len();
				let Some(core_index) = scheduled_cores.get(core_selector) else {
					// This cannot really happen, as we modulo the core selector with the
					// scheduled_cores length and we check that the scheduled_cores is not empty.
					continue;
				};
			},
			_ = relay_storage_root_to_hash.process().fuse() => {}
		}
	}
}

fn extract_relay_parent<Header: HeaderT>(
	header: &Header,
	relay_storage_root_to_block_hash: &mut RelayStorageRootToBlockHash,
) -> Option<RHash> {
	let digest = header.digest();

	if let Some(relay_parent) = cumulus_primitives_core::extract_relay_parent(digest) {
		return Some(relay_parent)
	}

	if let Some(relay_parent) =
		cumulus_primitives_core::rpsr_digest::extract_relay_parent_storage_root(digest)
			.and_then(|(r, _)| relay_storage_root_to_block_hash.root_to_hash(r))
	{
		return Some(relay_parent)
	}

	None
}

/// Maps from relay chain storage root to relay chain block hash.
struct RelayStorageRootToBlockHash {
	root_to_hash: LruMap<RHash, RHash>,
	import_notifications: Pin<Box<dyn Stream<Item = RHeader> + Send>>,
}

impl RelayStorageRootToBlockHash {
	async fn new(relay_interface: &impl RelayChainInterface) -> RelayChainResult<Self> {
		Ok(Self {
			root_to_hash: LruMap::new(ByLength::new(50)),
			import_notifications: relay_interface.import_notification_stream().await?,
		})
	}

	fn root_to_hash(&mut self, root: RHash) -> Option<RHash> {
		self.root_to_hash.get(&root).cloned()
	}

	/// Process relay chain import notifications.
	async fn process(&mut self) {
		while let Some(new_block) = self.import_notifications.next().await {
			self.root_to_hash.insert(*new_block.state_root(), new_block.hash());
		}
	}
}

/// Handle an incoming collation message from the block builder task.
/// This builds the collation from the [`CollatorMessage`] and submits it to
/// the collation-generation subsystem of the relay chain.
async fn handle_collation_message<Block: BlockT>(
	message: CollatorMessage<Block>,
	collator_service: &impl CollatorServiceInterface<Block>,
	overseer_handle: &mut OverseerHandle,
) {
	let CollatorMessage {
		parent_header,
		parachain_candidate,
		validation_code_hash,
		relay_parent,
		core_index,
	} = message;

	let hash = parachain_candidate.block.header().hash();
	let number = *parachain_candidate.block.header().number();
	let (collation, block_data) =
		match collator_service.build_collation(&parent_header, hash, parachain_candidate) {
			Some(collation) => collation,
			None => {
				tracing::warn!(target: LOG_TARGET, %hash, ?number, ?core_index, "Unable to build collation.");
				return;
			},
		};

	tracing::info!(
		target: LOG_TARGET,
		"PoV size {{ header: {:.2}kB, extrinsics: {:.2}kB, storage_proof: {:.2}kB }}",
		block_data.header().encoded_size() as f64 / 1024f64,
		block_data.extrinsics().encoded_size() as f64 / 1024f64,
		block_data.storage_proof().encoded_size() as f64 / 1024f64,
	);

	if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
		tracing::info!(
			target: LOG_TARGET,
			"Compressed PoV size: {}kb",
			pov.block_data.0.len() as f64 / 1024f64,
		);
	}

	tracing::debug!(target: LOG_TARGET, ?core_index, %hash, %number, "Submitting collation for core.");
	overseer_handle
		.send_msg(
			CollationGenerationMessage::SubmitCollation(SubmitCollationParams {
				relay_parent,
				collation,
				parent_head: parent_header.encode().into(),
				validation_code_hash,
				core_index,
				result_sender: None,
			}),
			"SubmitCollation",
		)
		.await;
}
