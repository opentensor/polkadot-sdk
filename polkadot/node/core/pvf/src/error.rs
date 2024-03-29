// Copyright (C) Parity Technologies (UK) Ltd.
// This file is part of Polkadot.

// Polkadot is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Polkadot is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Polkadot.  If not, see <http://www.gnu.org/licenses/>.

use polkadot_node_core_pvf_common::error::{InternalValidationError, PrepareError};

/// A error raised during validation of the candidate.
#[derive(Debug, Clone)]
pub enum ValidationError {
	/// The error was raised because the candidate is invalid.
	///
	/// Whenever we are unsure if the error was due to the candidate or not, we must vote invalid.
	InvalidCandidate(InvalidCandidate),
	/// Some internal error occurred.
	InternalError(InternalValidationError),
}

/// A description of an error raised during executing a PVF and can be attributed to the combination
/// of the candidate [`polkadot_parachain_primitives::primitives::ValidationParams`] and the PVF.
#[derive(Debug, Clone)]
pub enum InvalidCandidate {
	/// PVF preparation ended up with a deterministic error.
	PrepareError(String),
	/// The candidate is reported to be invalid by the execution worker. The string contains the
	/// error message.
	WorkerReportedInvalid(String),
	/// The worker process (not the job) has died during validation of a candidate.
	///
	/// It's unlikely that this is caused by malicious code since workers spawn separate job
	/// processes, and those job processes are sandboxed. But, it is possible. We retry in this
	/// case, and if the error persists, we assume it's caused by the candidate and vote against.
	AmbiguousWorkerDeath,
	/// PVF execution (compilation is not included) took more time than was allotted.
	HardTimeout,
	/// The job process (not the worker) has died for one of the following reasons:
	///
	/// (a) A seccomp violation occurred, most likely due to an attempt by malicious code to
	/// execute arbitrary code. Note that there is no foolproof way to detect this if the operator
	/// has seccomp auditing disabled.
	///
	/// (b) The host machine ran out of free memory and the OOM killer started killing the
	/// processes, and in order to save the parent it will "sacrifice child" first.
	///
	/// (c) Some other reason, perhaps transient or perhaps caused by malicious code.
	///
	/// We cannot treat this as an internal error because malicious code may have caused this.
	AmbiguousJobDeath(String),
	/// An unexpected error occurred in the job process and we can't be sure whether the candidate
	/// is really invalid or some internal glitch occurred. Whenever we are unsure, we can never
	/// treat an error as internal as we would abstain from voting. This is bad because if the
	/// issue was due to the candidate, then all validators would abstain, stalling finality on the
	/// chain. So we will first retry the candidate, and if the issue persists we are forced to
	/// vote invalid.
	JobError(String),
}

impl From<InternalValidationError> for ValidationError {
	fn from(error: InternalValidationError) -> Self {
		Self::InternalError(error)
	}
}

impl From<PrepareError> for ValidationError {
	fn from(error: PrepareError) -> Self {
		// Here we need to classify the errors into two errors: deterministic and non-deterministic.
		// See [`PrepareError::is_deterministic`].
		if error.is_deterministic() {
			Self::InvalidCandidate(InvalidCandidate::PrepareError(error.to_string()))
		} else {
			Self::InternalError(InternalValidationError::NonDeterministicPrepareError(error))
		}
	}
}
