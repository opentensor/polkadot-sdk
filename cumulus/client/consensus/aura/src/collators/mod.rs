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

//! Stock, pure Aura collators.
//!
//! This includes the [`basic`] collator, which only builds on top of the most recently
//! included parachain block, as well as the [`lookahead`] collator, which prospectively
//! builds on parachain blocks which have not yet been included in the relay chain.

use cumulus_primitives_core::{relay_chain::Hash as PHash, ParaId};
use futures::channel::oneshot;
use polkadot_node_subsystem::messages::{RuntimeApiMessage, RuntimeApiRequest};
use polkadot_overseer::Handle as OverseerHandle;

pub mod basic;
pub mod lookahead;
pub mod slot_based;

// Checks if there exists a scheduled core for the para at the provided relay parent.
//
// Falls back to `false` in case of an error.
async fn is_para_scheduled(
	relay_parent: PHash,
	para_id: ParaId,
	overseer_handle: &mut OverseerHandle,
) -> bool {
	let (tx, rx) = oneshot::channel();
	let request = RuntimeApiRequest::AvailabilityCores(tx);
	overseer_handle
		.send_msg(RuntimeApiMessage::Request(relay_parent, request), "Aura::is_para_scheduled")
		.await;

	let cores = match rx.await {
		Ok(Ok(cores)) => cores,
		Ok(Err(error)) => {
			tracing::error!(
				target: crate::LOG_TARGET,
				?error,
				?relay_parent,
				"Failed to query availability cores runtime API",
			);
			return false
		},
		Err(oneshot::Canceled) => {
			tracing::error!(
				target: crate::LOG_TARGET,
				?relay_parent,
				"Sender for availability cores runtime request dropped",
			);
			return false
		},
	};

	cores.iter().any(|core| core.para_id() == Some(para_id))
}
