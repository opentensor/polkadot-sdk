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

use std::collections::HashMap;

use codec::Encode;

use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_relay_chain_interface::RelayChainInterface;

use polkadot_node_primitives::{MaybeCompressedPoV, SubmitCollationParams};
use polkadot_node_subsystem::messages::CollationGenerationMessage;
use polkadot_overseer::Handle as OverseerHandle;
use polkadot_primitives::{CollatorPair, Hash as RHash, Id as ParaId};

use futures::prelude::*;

use sc_utils::mpsc::TracingUnboundedReceiver;
use sp_runtime::traits::{Block as BlockT, Header};

use super::CollatorMessage;

const LOG_TARGET: &str = "aura::cumulus::collation_task";

/// Parameters for the collation task.
pub struct Params<Block: BlockT, RClient, CS> {
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
}

/// Asynchronously executes the collation task for a parachain.
///
/// This function initializes the collator subsystems necessary for producing and submitting
/// collations to the relay chain. It listens for new best relay chain block notifications and
/// handles collator messages. If our parachain is scheduled on a core and we have a candidate,
/// the task will build a collation and send it to the relay chain.
pub async fn run_collation_task<Block, RClient, CS>(mut params: Params<Block, RClient, CS>)
where
	Block: BlockT,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	RClient: RelayChainInterface + Clone + 'static,
{
	let Ok(mut overseer_handle) = params.relay_client.overseer_handle() else {
		tracing::error!(target: LOG_TARGET, "Failed to get overseer handle.");
		return
	};

	cumulus_client_collator::initialize_collator_subsystems(
		&mut overseer_handle,
		params.collator_key,
		params.para_id,
		params.reinitialize,
	)
	.await;

	let mut collected = HashMap::<RHash, Vec<CollatorMessage<Block>>>::new();

	let collator_service = params.collator_service;
	while let Some(collator_message) = params.collator_receiver.next().await {
		let relay_parent = collator_message.relay_parent;
		let entry = collected.entry(collator_message.relay_parent);

		let messages = entry.or_default();

		if messages.is_empty() {
			messages.push(collator_message);
		} else {
			if messages.last().unwrap().parachain_candidate.block.header().hash() ==
				*collator_message.parachain_candidate.block.header().parent_hash()
			{
				tracing::error!("INSERTING {}", messages.len());
				messages.push(collator_message);
			}
		}

		if messages.len() == 12 {
			handle_collation_message(
				std::mem::take(messages),
				&collator_service,
				&mut overseer_handle,
			)
			.await;
			collected.remove(&relay_parent);
		}
	}
}

/// Handle an incoming collation message from the block builder task.
/// This builds the collation from the [`CollatorMessage`] and submits it to
/// the collation-generation subsystem of the relay chain.
async fn handle_collation_message<Block: BlockT>(
	messages: Vec<CollatorMessage<Block>>,
	collator_service: &impl CollatorServiceInterface<Block>,
	overseer_handle: &mut OverseerHandle,
) {
	let parent_header = messages.first().unwrap().parent_header.clone();
	let validation_code_hash = messages.first().unwrap().validation_code_hash.clone();
	let relay_parent = messages.first().unwrap().relay_parent.clone();
	let core_index = messages.first().unwrap().core_index.clone();

	let candidates = messages.into_iter().map(|c| c.parachain_candidate).collect::<Vec<_>>();

	let (collation, block_data) = match collator_service.build_collation(&parent_header, candidates)
	{
		Some(collation) => collation,
		None => {
			// tracing::warn!(target: LOG_TARGET, %hash, ?number, ?core_index, "Unable to build
			// collation.");
			return;
		},
	};

	/*
	tracing::info!(
		target: LOG_TARGET,
		"PoV size {{ header: {:.2}kB, extrinsics: {:.2}kB, storage_proof: {:.2}kB }}",
		block_data.header().encoded_size() as f64 / 1024f64,
		block_data.extrinsics().encoded_size() as f64 / 1024f64,
		block_data.storage_proof().encoded_size() as f64 / 1024f64,
	);
	*/

	if let MaybeCompressedPoV::Compressed(ref pov) = collation.proof_of_validity {
		tracing::info!(
			target: LOG_TARGET,
			"Compressed PoV size: {}kb",
			pov.block_data.0.len() as f64 / 1024f64,
		);
	}

	// tracing::debug!(target: LOG_TARGET, ?core_index, %hash, %number, "Submitting collation for
	// core.");
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
