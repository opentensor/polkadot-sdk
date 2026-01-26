// This file is part of Substrate.

// Copyright (C) Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: Apache-2.0

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// 	http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Proc macro of Support code for the runtime.

#![recursion_limit = "512"]
#![deny(rustdoc::broken_intra_doc_links)]

pub mod benchmark;
pub mod construct_runtime;
pub mod crate_version;
pub mod deprecation;
pub mod derive_impl;
pub mod dummy_part_checker;
pub mod dynamic_params;
pub mod key_prefix;
pub mod match_and_insert;
pub mod no_bound;
pub mod pallet;
pub mod pallet_error;
pub mod runtime;
pub mod storage_alias;
pub mod transactional;
pub mod tt_macro;

use std::{cell::RefCell, str::FromStr};

pub(crate) const INHERENT_INSTANCE_NAME: &str = "__InherentHiddenInstance";

thread_local! {
	/// A global counter, can be used to generate a relatively unique identifier.
	static COUNTER: RefCell<Counter> = RefCell::new(Counter(0));
}

/// Counter to generate a relatively unique identifier for macros. This is necessary because
/// declarative macros gets hoisted to the crate root, which shares the namespace with other pallets
/// containing the very same macros.
struct Counter(u64);

impl Counter {
	fn inc(&mut self) -> u64 {
		let ret = self.0;
		self.0 += 1;
		ret
	}
}

/// Get the value from the given environment variable set by cargo.
///
/// The value is parsed into the requested destination type.
fn get_cargo_env_var<T: FromStr>(version_env: &str) -> std::result::Result<T, ()> {
	let version = std::env::var(version_env)
		.unwrap_or_else(|_| panic!("`{}` is always set by cargo; qed", version_env));

	T::from_str(&version).map_err(drop)
}

/// Generate the counter_prefix related to the storage.
/// counter_prefix is used by counted storage map.
fn counter_prefix(prefix: &str) -> String {
	format!("CounterFor{}", prefix)
}

/// The number of module instances supported by the runtime, starting at index 1,
/// and up to `NUMBER_OF_INSTANCE`.
pub(crate) const NUMBER_OF_INSTANCE: u8 = 16;