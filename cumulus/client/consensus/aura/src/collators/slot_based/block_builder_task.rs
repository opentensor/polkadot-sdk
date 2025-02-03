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

use codec::{Codec, Encode};

use cumulus_client_collator::service::ServiceInterface as CollatorServiceInterface;
use cumulus_client_consensus_common::{self as consensus_common, ParachainBlockImportMarker};
use cumulus_client_consensus_proposer::ProposerInterface;
use cumulus_primitives_aura::AuraUnincludedSegmentApi;
use cumulus_primitives_core::{GetCoreSelectorApi, PersistedValidationData};
use cumulus_relay_chain_interface::{RelayChainInterface, RelayChainResult};

use polkadot_primitives::{Block as RelayBlock, Hash as RHash, Header as RHeader, Id as ParaId};

use futures::prelude::*;
use sc_client_api::{backend::AuxStore, BlockBackend, BlockOf, UsageProvider};
use sc_consensus::BlockImport;
use sc_consensus_babe::CompatibleDigestItem;
use schnellru::LruMap;
use sp_api::ProvideRuntimeApi;
use sp_application_crypto::AppPublic;
use sp_blockchain::HeaderBackend;
use sp_consensus_aura::{AuraApi, Slot};
use sp_core::crypto::{Pair, UncheckedInto};
use sp_inherents::CreateInherentDataProviders;
use sp_keystore::KeystorePtr;
use sp_runtime::{
	generic::BlockId,
	traits::{Block as BlockT, Header as HeaderT, Member},
};
use sp_timestamp::Timestamp;
use std::{
	marker::PhantomData,
	pin::Pin,
	sync::Arc,
	time::{Duration, SystemTime},
};

use super::CollatorMessage;
use crate::{
	collator::{self as collator_util},
	collators::{
		check_validation_code_or_log,
		slot_based::{
			core_selector,
			relay_chain_data_cache::{RelayChainData, RelayChainDataCache},
		},
		FindParent,
	},
	LOG_TARGET,
};

/// Parameters for [`run_block_builder`].
pub struct BuilderTaskParams<
	Block: BlockT,
	BI,
	CIDP,
	Client,
	Backend,
	RelayClient,
	CHP,
	Proposer,
	CS,
> {
	/// Inherent data providers. Only non-consensus inherent data should be provided, i.e.
	/// the timestamp, slot, and paras inherents should be omitted, as they are set by this
	/// collator.
	pub create_inherent_data_providers: CIDP,
	/// Used to actually import blocks.
	pub block_import: BI,
	/// The underlying para client.
	pub para_client: Arc<Client>,
	/// The para client's backend, used to access the database.
	pub para_backend: Arc<Backend>,
	/// A handle to the relay-chain client.
	pub relay_client: RelayClient,
	/// A validation code hash provider, used to get the current validation code hash.
	pub code_hash_provider: CHP,
	/// The underlying keystore, which should contain Aura consensus keys.
	pub keystore: KeystorePtr,
	/// The para's ID.
	pub para_id: ParaId,
	/// The underlying block proposer this should call into.
	pub proposer: Proposer,
	/// The generic collator service used to plug into this consensus engine.
	pub collator_service: CS,
	/// The amount of time to spend authoring each block.
	pub authoring_duration: Duration,
	/// Channel to send built blocks to the collation task.
	pub collator_sender: sc_utils::mpsc::TracingUnboundedSender<CollatorMessage<Block>>,
	/// Drift every slot by this duration.
	/// This is a time quantity that is subtracted from the actual timestamp when computing
	/// the time left to enter a new slot. In practice, this *left-shifts* the clock time with the
	/// intent to keep our "clock" slightly behind the relay chain one and thus reducing the
	/// likelihood of encountering unfavorable notification arrival timings (i.e. we don't want to
	/// wait for relay chain notifications because we woke up too early).
	pub slot_drift: Duration,
}

#[derive(Debug)]
struct SlotInfo {
	pub timestamp: Timestamp,
	pub slot: Slot,
}

#[derive(Debug)]
struct SlotTimer<Block, Client, P> {
	client: Arc<Client>,
	drift: Duration,
	_marker: std::marker::PhantomData<(Block, Box<dyn Fn(P) + Send + Sync + 'static>)>,
}

/// Returns current duration since Unix epoch.
fn duration_now() -> Duration {
	use std::time::SystemTime;
	let now = SystemTime::now();
	now.duration_since(SystemTime::UNIX_EPOCH).unwrap_or_else(|e| {
		panic!("Current time {:?} is before Unix epoch. Something is wrong: {:?}", now, e)
	})
}

/// Returns the duration until the next slot from now.
fn time_until_next_slot(slot_duration: Duration, drift: Duration) -> Duration {
	let now = duration_now().as_millis() - drift.as_millis();

	let next_slot = (now + slot_duration.as_millis()) / slot_duration.as_millis();
	let remaining_millis = next_slot * slot_duration.as_millis() - now;
	Duration::from_millis(remaining_millis as u64)
}

impl<Block, Client, P> SlotTimer<Block, Client, P>
where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block> + Send + Sync + 'static + UsageProvider<Block>,
	Client::Api: AuraApi<Block, P::Public>,
	P: Pair,
	P::Public: AppPublic + Member + Codec,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	pub fn new_with_drift(client: Arc<Client>, drift: Duration) -> Self {
		Self { client, drift, _marker: Default::default() }
	}

	/// Returns a future that resolves when the next slot arrives.
	pub async fn wait_until_next_slot(&self) -> Result<SlotInfo, ()> {
		let Ok(slot_duration) = crate::slot_duration(&*self.client) else {
			tracing::error!(target: crate::LOG_TARGET, "Failed to fetch slot duration from runtime.");
			return Err(())
		};

		let time_until_next_slot = time_until_next_slot(slot_duration.as_duration(), self.drift);
		tokio::time::sleep(time_until_next_slot).await;
		let timestamp = sp_timestamp::Timestamp::current();
		Ok(SlotInfo { slot: Slot::from_timestamp(timestamp, slot_duration), timestamp })
	}
}

/// Run block-builder.
pub async fn run_block_builder<
	Block,
	P,
	BI,
	CIDP,
	Client,
	Backend,
	RelayClient,
	CHP,
	Proposer,
	CS,
>(
	params: BuilderTaskParams<Block, BI, CIDP, Client, Backend, RelayClient, CHP, Proposer, CS>,
) where
	Block: BlockT,
	Client: ProvideRuntimeApi<Block>
		+ UsageProvider<Block>
		+ BlockOf
		+ AuxStore
		+ HeaderBackend<Block>
		+ BlockBackend<Block>
		+ Send
		+ Sync
		+ 'static,
	Client::Api:
		AuraApi<Block, P::Public> + GetCoreSelectorApi<Block> + AuraUnincludedSegmentApi<Block>,
	Backend: sc_client_api::Backend<Block> + 'static,
	RelayClient: RelayChainInterface + Clone + 'static,
	CIDP: CreateInherentDataProviders<Block, ()> + 'static,
	CIDP::InherentDataProviders: Send,
	BI: BlockImport<Block> + ParachainBlockImportMarker + Send + Sync + 'static,
	Proposer: ProposerInterface<Block> + Send + Sync + 'static,
	CS: CollatorServiceInterface<Block> + Send + Sync + 'static,
	CHP: consensus_common::ValidationCodeHashProvider<Block::Hash> + Send + 'static,
	P: Pair,
	P::Public: AppPublic + Member + Codec,
	P::Signature: TryFrom<Vec<u8>> + Member + Codec,
{
	tracing::info!(target: LOG_TARGET, "Starting slot-based block-builder task.");
	let BuilderTaskParams {
		relay_client,
		create_inherent_data_providers,
		para_client,
		keystore,
		block_import,
		para_id,
		proposer,
		collator_service,
		collator_sender,
		code_hash_provider,
		authoring_duration,
		para_backend,
		slot_drift,
	} = params;

	let slot_timer = SlotTimer::<_, _, P>::new_with_drift(para_client.clone(), slot_drift);

	let mut collator = {
		let params = collator_util::Params {
			create_inherent_data_providers,
			block_import,
			relay_client: relay_client.clone(),
			keystore: keystore.clone(),
			para_id,
			proposer,
			collator_service,
		};

		collator_util::Collator::<Block, P, _, _, _, _, _>::new(params)
	};

	let mut relay_chain_data_cache = RelayChainDataCache::new(relay_client.clone(), para_id);
	let mut relay_chain_parent =
		match RelayParentToBuildOn::new(relay_client.clone(), para_client.clone()).await {
			Ok(r) => r,
			Err(error) => {
				tracing::error!(target: LOG_TARGET, ?error, "Failed to initialize `RelayParentToBuildOn`");
				return;
			},
		};

	loop {
		// We wait here until the next slot arrives.
		let Ok(para_slot) = slot_timer.wait_until_next_slot().await else {
			return;
		};

		let relay_parent = relay_chain_parent.relay_parent_to_build_on().await;

		let Some(FindParent { included_block, best_parent, pending_block }) =
			crate::collators::find_parent(relay_parent, para_id, &*para_backend, &relay_client)
				.await
		else {
			continue
		};

		let parent_hash = best_parent.hash;

		// Retrieve the core selector.
		let (core_selector, claim_queue_offset) =
			match core_selector(&*para_client, best_parent.hash, *best_parent.header.number()) {
				Ok(core_selector) => core_selector,
				Err(err) => {
					tracing::debug!(
						target: crate::LOG_TARGET,
						"Unable to retrieve the core selector from the runtime API: {}",
						err
					);
					continue
				},
			};

		let Some(RelayChainData {
			relay_parent_header,
			max_pov_size,
			scheduled_cores,
			claimed_cores,
		}) = relay_chain_data_cache
			.get_mut_relay_chain_data(relay_parent, claim_queue_offset)
			.await
		else {
			continue;
		};

		if scheduled_cores.is_empty() {
			tracing::debug!(target: LOG_TARGET, "Parachain not scheduled, skipping slot.");
			continue;
		} else {
			tracing::debug!(
				target: LOG_TARGET,
				?relay_parent,
				"Parachain is scheduled on cores: {:?}",
				scheduled_cores
			);
		}

		let core_selector = core_selector.0 as usize % scheduled_cores.len();
		let Some(core_index) = scheduled_cores.get(core_selector) else {
			// This cannot really happen, as we modulo the core selector with the
			// scheduled_cores length and we check that the scheduled_cores is not empty.
			continue;
		};

		//TODO: FIX
		// if !claimed_cores.insert(*core_index) {
		// 	tracing::debug!(
		// 		target: LOG_TARGET,
		// 		"Core {:?} was already claimed at this relay chain slot",
		// 		core_index
		// 	);
		// 	continue
		// }

		let parent_header = best_parent.header;

		// We mainly call this to inform users at genesis if there is a mismatch with the
		// on-chain data.
		collator.collator_service().check_block_status(parent_hash, &parent_header);

		let Ok(relay_slot) = sc_consensus_babe::find_pre_digest::<RelayBlock>(&relay_parent_header)
			.map(|babe_pre_digest| babe_pre_digest.slot())
		else {
			tracing::error!(target: crate::LOG_TARGET, "Relay chain does not contain babe slot. This should never happen.");
			continue;
		};

		let slot_claim = match crate::collators::can_build_upon::<_, _, P>(
			para_slot.slot,
			relay_slot,
			para_slot.timestamp,
			parent_hash,
			included_block.hash,
			&*para_client,
			&keystore,
		)
		.await
		{
			Some(slot) => slot,
			None => {
				tracing::debug!(
					target: crate::LOG_TARGET,
					?core_index,
					slot_info = ?para_slot,
					unincluded_segment_len = best_parent.depth,
					relay_parent = ?relay_parent,
					included = ?included_block.hash,
					parent = ?parent_hash,
					"Not building block."
				);
				continue
			},
		};

		tracing::debug!(
			target: crate::LOG_TARGET,
			?core_index,
			slot_info = ?para_slot,
			unincluded_segment_len = best_parent.depth,
			relay_parent = ?relay_parent,
			included = ?included_block.hash,
			pending_block = ?pending_block.as_ref().map(|p| p.hash),
			parent = ?parent_hash,
			"Building block."
		);

		let validation_data = PersistedValidationData {
			parent_head: pending_block
				.map_or_else(|| included_block.header, |p| p.header)
				.encode()
				.into(),
			relay_parent_number: *relay_parent_header.number(),
			relay_parent_storage_root: *relay_parent_header.state_root(),
			max_pov_size: *max_pov_size,
		};

		let (parachain_inherent_data, other_inherent_data) = match collator
			.create_inherent_data(
				relay_parent,
				&validation_data,
				parent_hash,
				slot_claim.timestamp(),
			)
			.await
		{
			Err(err) => {
				tracing::error!(target: crate::LOG_TARGET, ?err);
				break
			},
			Ok(x) => x,
		};

		let validation_code_hash = match code_hash_provider.code_hash_at(parent_hash) {
			None => {
				tracing::error!(target: crate::LOG_TARGET, ?parent_hash, "Could not fetch validation code hash");
				break
			},
			Some(v) => v,
		};

		check_validation_code_or_log(&validation_code_hash, para_id, &relay_client, relay_parent)
			.await;

		let allowed_pov_size = if cfg!(feature = "full-pov-size") {
			validation_data.max_pov_size
		} else {
			// Set the block limit to 50% of the maximum PoV size.
			//
			// TODO: If we got benchmarking that includes the proof size,
			// we should be able to use the maximum pov size.
			validation_data.max_pov_size / 2
		} as usize;

		let Ok(Some(parachain_candidate)) = collator
			.build_block_and_import(
				&parent_header,
				&slot_claim,
				None,
				(parachain_inherent_data, other_inherent_data),
				authoring_duration,
				allowed_pov_size,
			)
			.await
		else {
			tracing::error!(target: crate::LOG_TARGET, "Unable to build block at slot.");
			continue;
		};

		let new_block_hash = parachain_candidate.block.header().hash();

		// Announce the newly built block to our peers.
		collator.collator_service().announce_block(new_block_hash, None);

		if let Err(err) = collator_sender.unbounded_send(CollatorMessage {
			relay_parent,
			parent_header,
			parachain_candidate,
			validation_code_hash,
			core_index: *core_index,
			scheduled_cores: scheduled_cores.clone(),
		}) {
			tracing::error!(target: crate::LOG_TARGET, ?err, "Unable to send block to collation task.");
			return
		}
	}
}

struct RelayParentToBuildOn<Block, Client, RI> {
	best_relay_block: RHeader,
	relay_import_notifications: Pin<Box<dyn Stream<Item = RHeader> + Send>>,
	root_to_header: LruMap<RHash, RHeader>,
	hash_to_header: LruMap<RHash, RHeader>,
	para_client: Arc<Client>,
	max_para_blocks_per_relay: usize,
	relay_interface: RI,
	_marker: PhantomData<Block>,
}

impl<Block, Client, RI> RelayParentToBuildOn<Block, Client, RI>
where
	Block: BlockT,
	Client: HeaderBackend<Block>,
	RI: RelayChainInterface,
{
	async fn new(relay_interface: RI, para_client: Arc<Client>) -> RelayChainResult<Self> {
		let relay_import_notifications = relay_interface.import_notification_stream().await?;

		let best_relay_block = relay_interface.best_block_hash().await?;
		let best_relay_block_header =
			relay_interface.header(BlockId::Hash(best_relay_block)).await?.unwrap();

		let mut root_to_header = LruMap::new(50.into());
		root_to_header
			.insert(*best_relay_block_header.state_root(), best_relay_block_header.clone());

		let mut hash_to_header = LruMap::new(50.into());
		hash_to_header.insert(best_relay_block_header.hash(), best_relay_block_header.clone());

		Ok(Self {
			relay_import_notifications,
			root_to_header,
			hash_to_header,
			best_relay_block: best_relay_block_header,
			para_client,
			relay_interface,
			// TODO: Calculate this
			max_para_blocks_per_relay: 12,
			_marker: Default::default(),
		})
	}

	/// Returns the relay parent to build on.
	async fn relay_parent_to_build_on(&mut self) -> RHash {
		while let Some(header) = self.relay_import_notifications.next().now_or_never().flatten() {
			//TODO: Do we need a better best block selection?
			if header.number() > self.best_relay_block.number() {
				self.best_relay_block = header.clone();
			}

			self.root_to_header.insert(*header.state_root(), header);
		}

		let current_relay_slot = Self::current_relay_chain_slot();
		//TODO: Use `SelectChain`?
		let best_para_block = self.para_client.info().best_hash;
		let Ok(Some(para_header)) = self.para_client.header(best_para_block) else {
			// Should not happen..
			return self.determine_best_relay_block_to_build_on(current_relay_slot, None).await
		};

		let Some(best_para_block_relay_parent) = self.extract_relay_parent(&para_header) else {
			// Should not happen..
			return self.determine_best_relay_block_to_build_on(current_relay_slot, None).await
		};

		let Ok(Some(relay_header)) =
			self.relay_interface.header(BlockId::Hash(best_para_block_relay_parent)).await
		else {
			// Should not happen..
			return self.determine_best_relay_block_to_build_on(current_relay_slot, None).await
		};

		let best_para_block_relay_slot = Self::relay_slot_for_header(&relay_header);

		//TODO: 2 needs to be configurable/depending on schedule lookahead
		if best_para_block_relay_slot + 2 < current_relay_slot {
			return self
				.determine_best_relay_block_to_build_on(
					current_relay_slot,
					Some(best_para_block_relay_slot),
				)
				.await;
		}

		// `best_para_block` is build on top of the relay parent, thus we need to start counting
		// from `1`.
		let mut on_same_parent = 1;
		let mut next_hash = *para_header.parent_hash();

		loop {
			let Ok(Some(next_header)) = self.para_client.header(next_hash) else {
				break;
			};

			let Some(relay_parent) = self.extract_relay_parent(&next_header) else {
				break;
			};

			if relay_parent == best_para_block_relay_parent {
				next_hash = *next_header.parent_hash();
				on_same_parent += 1;
			} else {
				break;
			}
		}

		if dbg!(on_same_parent) >= self.max_para_blocks_per_relay {
			self.determine_best_relay_block_to_build_on(
				current_relay_slot,
				Some(best_para_block_relay_slot),
			)
			.await
		} else {
			best_para_block_relay_parent
		}
	}

	fn current_relay_chain_slot() -> Slot {
		Slot::from(
			((SystemTime::now()
				.duration_since(SystemTime::UNIX_EPOCH)
				.unwrap_or_default()
				.as_millis()) -
				4000 / 6000) as u64,
		)
	}

	async fn determine_best_relay_block_to_build_on(
		&mut self,
		current_slot: Slot,
		best_para_relay_parent_slot: Option<Slot>,
	) -> RHash {
		let best_relay_block = self.relay_interface.best_block_hash().await.unwrap();

		if let Some(slot) = best_para_relay_parent_slot
			.map(|s| s + 1)
			.filter(|s| current_slot - 2.into() < *s)
		{
			let mut header = self
				.relay_interface
				.header(BlockId::Hash(best_relay_block))
				.await
				.unwrap()
				.unwrap();
			let mut header_slot = Self::relay_slot_for_header(&header);

			//TODO: Take into account `current_slot` maybe already being at its end
			while header_slot > slot {
				header = self
					.relay_interface
					.header(BlockId::Hash(*header.parent_hash()))
					.await
					.unwrap()
					.unwrap();
				header_slot = Self::relay_slot_for_header(&header);
			}

			if header_slot == slot {
				return header.hash()
			}
		}

		let mut header = self
			.relay_interface
			.header(BlockId::Hash(best_relay_block))
			.await
			.unwrap()
			.unwrap();
		let mut header_slot = Self::relay_slot_for_header(&header);

		while header_slot > current_slot {
			header = self
				.relay_interface
				.header(BlockId::Hash(*header.parent_hash()))
				.await
				.unwrap()
				.unwrap();
			header_slot = Self::relay_slot_for_header(&header);
		}

		header.hash()
	}

	fn relay_slot_for_header(header: &RHeader) -> Slot {
		if *header.number() == 0 {
			return Slot::from(0)
		}

		header.digest().logs.iter().find_map(|d| d.as_babe_pre_digest().map(|p| p.slot()))
			.expect("Every relay chain block has a BABE digest, otherwise it should not have been imported; qed").into()
	}

	fn extract_relay_parent(&mut self, header: &Block::Header) -> Option<RHash> {
		let digest = header.digest();

		if let Some(relay_parent) = cumulus_primitives_core::extract_relay_parent(digest) {
			return Some(relay_parent)
		}

		if let Some(relay_parent) =
			cumulus_primitives_core::rpsr_digest::extract_relay_parent_storage_root(digest)
				.and_then(|(r, _)| self.root_to_header.get(&r).map(|h| h.hash()))
		{
			return Some(relay_parent)
		}

		None
	}
}
