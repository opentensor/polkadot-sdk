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

//! *BLS12-381* types and host functions.

use crate::utils::{self, HostcallResult, FAIL_MSG};
use alloc::vec::Vec;
use ark_bls12_381_ext::CurveHooks;
use ark_ec::{pairing::Pairing, AffineRepr, CurveGroup};
use sp_runtime_interface::{
	pass_by::{
		AllocateAndReturnByCodec, PassFatPointerAndRead, PassFatPointerAndReadWrite,
		PassFatPointerAndWrite,
	},
	runtime_interface,
};

mod v1 {
	use alloc::vec::Vec;
	use ark_ec_v4::{
		pairing::{MillerLoopOutput, Pairing},
		short_weierstrass::{Affine, Projective, SWCurveConfig},
		VariableBaseMSM,
	};
	use ark_scale_v4::{
		ark_serialize::{CanonicalDeserialize, CanonicalSerialize, Compress, Validate},
		scale::{Decode, Encode},
	};

	const SCALE_USAGE: u8 = ark_scale_v4::make_usage(Compress::No, Validate::No);
	type ArkScale<T> = ark_scale_v4::ArkScale<T, SCALE_USAGE>;
	type ArkScaleProjective<T> = ark_scale_v4::hazmat::ArkScaleProjective<T>;

	fn encode<T: CanonicalSerialize>(value: T) -> Vec<u8> {
		ArkScale::from(value).encode()
	}

	fn decode<T: CanonicalDeserialize>(value: Vec<u8>) -> Result<T, ()> {
		ArkScale::<T>::decode(&mut &value[..]).map(|value| value.0).map_err(|_| ())
	}

	fn encode_projective<T: SWCurveConfig>(value: &Projective<T>) -> Vec<u8> {
		ArkScaleProjective::from(value).encode()
	}

	fn decode_projective<T: SWCurveConfig>(value: Vec<u8>) -> Result<Projective<T>, ()> {
		ArkScaleProjective::decode(&mut &value[..]).map(|value| value.0).map_err(|_| ())
	}

	pub fn multi_miller_loop<T: Pairing>(g1: Vec<u8>, g2: Vec<u8>) -> Result<Vec<u8>, ()> {
		let g1 = decode::<Vec<T::G1Affine>>(g1)?;
		let g2 = decode::<Vec<T::G2Affine>>(g2)?;
		Ok(encode(T::multi_miller_loop(g1, g2).0))
	}

	pub fn final_exponentiation<T: Pairing>(target: Vec<u8>) -> Result<Vec<u8>, ()> {
		let target = decode::<T::TargetField>(target)?;
		let result = T::final_exponentiation(MillerLoopOutput(target)).ok_or(())?;
		Ok(encode(result.0))
	}

	pub fn msm<T: SWCurveConfig>(bases: Vec<u8>, scalars: Vec<u8>) -> Result<Vec<u8>, ()> {
		let bases = decode::<Vec<Affine<T>>>(bases)?;
		let scalars = decode::<Vec<T::ScalarField>>(scalars)?;
		let result = Projective::<T>::msm(&bases, &scalars).map_err(|_| ())?;
		Ok(encode_projective(&result))
	}

	pub fn mul_projective<T: SWCurveConfig>(base: Vec<u8>, scalar: Vec<u8>) -> Result<Vec<u8>, ()> {
		let base = decode_projective::<T>(base)?;
		let scalar = decode::<Vec<u64>>(scalar)?;
		Ok(encode_projective(&T::mul_projective(&base, &scalar)))
	}
}

/// Configuration for *BLS12-381* curve.
pub type Config = ark_bls12_381_ext::Config<HostHooks>;

/// *BLS12-381* pairing friendly curve.
pub type Bls12_381 = ark_bls12_381_ext::Bls12_381<HostHooks>;

/// G1 group configuration.
pub type G1Config = ark_bls12_381_ext::g1::Config<HostHooks>;
/// An element in G1 (affine).
pub type G1Affine = ark_bls12_381_ext::g1::G1Affine<HostHooks>;
/// An element in G1 (projective).
pub type G1Projective = ark_bls12_381_ext::g1::G1Projective<HostHooks>;

/// G2 group configuration.
pub type G2Config = ark_bls12_381_ext::g2::Config<HostHooks>;
/// An element in G2 (affine).
pub type G2Affine = ark_bls12_381_ext::g2::G2Affine<HostHooks>;
/// An element in G2 (projective).
pub type G2Projective = ark_bls12_381_ext::g2::G2Projective<HostHooks>;

/// G1 and G2 scalar field (Fr).
pub type ScalarField = <Bls12_381 as Pairing>::ScalarField;

/// An element in G1 preprocessed for pairing.
pub type G1Prepared = <Bls12_381 as Pairing>::G1Prepared;
/// An element in G2 preprocessed for pairing.
pub type G2Prepared = <Bls12_381 as Pairing>::G2Prepared;
/// Pairing target field.
pub type TargetField = <Bls12_381 as Pairing>::TargetField;

/// Curve hooks jumping into [`host_calls`] host functions.
#[derive(Copy, Clone)]
pub struct HostHooks;

impl CurveHooks for HostHooks {
	fn multi_miller_loop(
		g1: impl Iterator<Item = G1Prepared>,
		g2: impl Iterator<Item = G2Prepared>,
	) -> TargetField {
		let mut out = utils::buffer_for::<TargetField>();
		host_calls::bls12_381_multi_miller_loop(
			&utils::encode_iter(g1),
			&utils::encode_iter(g2),
			&mut out,
		)
		.and_then(|_| utils::decode::<TargetField>(&out))
		.expect(FAIL_MSG)
	}

	fn final_exponentiation(target: TargetField) -> TargetField {
		let mut in_out = utils::encode(target);
		host_calls::bls12_381_final_exponentiation(&mut in_out)
			.and_then(|_| utils::decode::<TargetField>(&in_out))
			.expect(FAIL_MSG)
	}

	fn msm_g1(bases: &[G1Affine], scalars: &[ScalarField]) -> G1Projective {
		let mut out = utils::buffer_for::<G1Affine>();
		host_calls::bls12_381_msm_g1(&utils::encode(bases), &utils::encode(scalars), &mut out)
			.and_then(|_| utils::decode::<G1Affine>(&out))
			.expect(FAIL_MSG)
			.into_group()
	}

	fn msm_g2(bases: &[G2Affine], scalars: &[ScalarField]) -> G2Projective {
		let mut out = utils::buffer_for::<G2Affine>();
		host_calls::bls12_381_msm_g2(&utils::encode(bases), &utils::encode(scalars), &mut out)
			.and_then(|_| utils::decode::<G2Affine>(&out))
			.expect(FAIL_MSG)
			.into_group()
	}

	fn mul_projective_g1(base: &G1Projective, scalar: &[u64]) -> G1Projective {
		let mut out = utils::buffer_for::<G1Affine>();
		host_calls::bls12_381_mul_g1(
			&utils::encode(base.into_affine()),
			&utils::encode(scalar),
			&mut out,
		)
		.and_then(|_| utils::decode::<G1Affine>(&out))
		.expect(FAIL_MSG)
		.into_group()
	}

	fn mul_projective_g2(base: &G2Projective, scalar: &[u64]) -> G2Projective {
		let mut out = utils::buffer_for::<G2Affine>();
		host_calls::bls12_381_mul_g2(
			&utils::encode(base.into_affine()),
			&utils::encode(scalar),
			&mut out,
		)
		.and_then(|_| utils::decode::<G2Affine>(&out))
		.expect(FAIL_MSG)
		.into_group()
	}
}

/// Interfaces for working with *Arkworks* *BLS12-381* elliptic curve related types
/// from within the runtime.
///
/// All types are (de-)serialized through the wrapper types from `ark-scale`.
///
/// `ArkScale`'s `Usage` generic parameter is expected to be set to "not-validated"
/// and "not-compressed".
#[runtime_interface]
pub trait HostCalls {
	/// Version 1 pairing multi Miller loop retained for deployed runtimes.
	fn bls12_381_multi_miller_loop(
		a: PassFatPointerAndRead<Vec<u8>>,
		b: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::multi_miller_loop::<ark_bls12_381_v4::Bls12_381>(a, b)
	}

	/// Pairing multi Miller loop for *BLS12-381*.
	///
	/// Receives encoded:
	/// - `a`: `Vec<G1Affine>`.
	/// - `b`: `Vec<G2Affine>`.
	/// Writes encoded `TargetField` to `out`.
	#[version(2)]
	fn bls12_381_multi_miller_loop(
		a: PassFatPointerAndRead<&[u8]>,
		b: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::multi_miller_loop::<ark_bls12_381::Bls12_381>(a, b, out)
	}

	/// Version 1 pairing final exponentiation retained for deployed runtimes.
	fn bls12_381_final_exponentiation(
		f: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::final_exponentiation::<ark_bls12_381_v4::Bls12_381>(f)
	}

	/// Pairing final exponentiation for *BLS12-381*.
	///
	/// Receives encoded: `TargetField`.
	/// Writes encoded `TargetField` to `in_out`.
	#[version(2)]
	fn bls12_381_final_exponentiation(
		in_out: PassFatPointerAndReadWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::final_exponentiation::<ark_bls12_381::Bls12_381>(in_out)
	}

	/// Version 1 multi scalar multiplication on G1 retained for deployed runtimes.
	fn bls12_381_msm_g1(
		bases: PassFatPointerAndRead<Vec<u8>>,
		scalars: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::msm::<ark_bls12_381_v4::g1::Config>(bases, scalars)
	}

	/// Multi scalar multiplication on *G1* for *BLS12-381*.
	///
	/// Receives encoded:
	/// - `bases`: `Vec<G1Affine>`.
	/// - `scalars`: `Vec<ScalarField>`.
	/// Writes encoded `G1Affine` to `out`.
	#[version(2)]
	fn bls12_381_msm_g1(
		bases: PassFatPointerAndRead<&[u8]>,
		scalars: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::msm_sw::<ark_bls12_381::g1::Config>(bases, scalars, out)
	}

	/// Version 1 multi scalar multiplication on G2 retained for deployed runtimes.
	fn bls12_381_msm_g2(
		bases: PassFatPointerAndRead<Vec<u8>>,
		scalars: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::msm::<ark_bls12_381_v4::g2::Config>(bases, scalars)
	}

	/// Multi scalar multiplication on *G2* for *BLS12-381*.
	///
	/// Receives encoded:
	/// - `bases`: `Vec<G2Affine>`.
	/// - `scalars`: `Vec<ScalarField>`.
	/// Writes encoded `G2Affine` to `out`.
	#[version(2)]
	fn bls12_381_msm_g2(
		bases: PassFatPointerAndRead<&[u8]>,
		scalars: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::msm_sw::<ark_bls12_381::g2::Config>(bases, scalars, out)
	}

	/// Version 1 projective multiplication on G1 retained for deployed runtimes.
	fn bls12_381_mul_projective_g1(
		base: PassFatPointerAndRead<Vec<u8>>,
		scalar: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::mul_projective::<ark_bls12_381_v4::g1::Config>(base, scalar)
	}

	/// Affine multiplication on *G1* for *BLS12-381*.
	///
	/// Receives encoded:
	/// - `base`: `G1Affine`.
	/// - `scalar`: `BigInteger`.
	/// Writes encoded `G1Affine` to `out`.
	fn bls12_381_mul_g1(
		base: PassFatPointerAndRead<&[u8]>,
		scalar: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::mul_sw::<ark_bls12_381::g1::Config>(base, scalar, out)
	}

	/// Version 1 projective multiplication on G2 retained for deployed runtimes.
	fn bls12_381_mul_projective_g2(
		base: PassFatPointerAndRead<Vec<u8>>,
		scalar: PassFatPointerAndRead<Vec<u8>>,
	) -> AllocateAndReturnByCodec<Result<Vec<u8>, ()>> {
		v1::mul_projective::<ark_bls12_381_v4::g2::Config>(base, scalar)
	}

	/// Affine multiplication on *G2* for *BLS12-381*.
	///
	/// Receives encoded:
	/// - `base`: `G2Affine`.
	/// - `scalar`: `BigInteger`.
	/// Writes encoded `G2Affine` to `out`.
	fn bls12_381_mul_g2(
		base: PassFatPointerAndRead<&[u8]>,
		scalar: PassFatPointerAndRead<&[u8]>,
		out: PassFatPointerAndWrite<&mut [u8]>,
	) -> HostcallResult {
		utils::mul_sw::<ark_bls12_381::g2::Config>(base, scalar, out)
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::utils::testing::*;
	use sp_runtime_interface::sp_wasm_interface::{HostFunctions as _, Signature, ValueType};

	#[test]
	fn registers_deployed_and_current_host_abis() {
		let functions = host_calls::HostFunctions::host_functions();
		let signature = |name: &str| {
			functions
				.iter()
				.find(|function| function.name() == name)
				.unwrap_or_else(|| panic!("missing host function {name}"))
				.signature()
		};

		assert_eq!(
			signature("ext_host_calls_bls12_381_final_exponentiation_version_1"),
			Signature::new(&[ValueType::I64], Some(ValueType::I64)),
		);
		assert_eq!(
			signature("ext_host_calls_bls12_381_final_exponentiation_version_2"),
			Signature::new(&[ValueType::I64], Some(ValueType::I32)),
		);
		assert_eq!(
			signature("ext_host_calls_bls12_381_multi_miller_loop_version_1"),
			Signature::new(&[ValueType::I64, ValueType::I64], Some(ValueType::I64)),
		);
		assert_eq!(
			signature("ext_host_calls_bls12_381_multi_miller_loop_version_2"),
			Signature::new(&[ValueType::I64, ValueType::I64, ValueType::I64], Some(ValueType::I32),),
		);
		for group in ["g1", "g2"] {
			assert_eq!(
				signature(&format!("ext_host_calls_bls12_381_msm_{group}_version_1")),
				Signature::new(&[ValueType::I64, ValueType::I64], Some(ValueType::I64)),
			);
			assert_eq!(
				signature(&format!("ext_host_calls_bls12_381_msm_{group}_version_2")),
				Signature::new(
					&[ValueType::I64, ValueType::I64, ValueType::I64],
					Some(ValueType::I32),
				),
			);
			assert_eq!(
				signature(&format!("ext_host_calls_bls12_381_mul_projective_{group}_version_1")),
				Signature::new(&[ValueType::I64, ValueType::I64], Some(ValueType::I64)),
			);
			assert_eq!(
				signature(&format!("ext_host_calls_bls12_381_mul_{group}_version_1")),
				Signature::new(
					&[ValueType::I64, ValueType::I64, ValueType::I64],
					Some(ValueType::I32),
				),
			);
		}
	}

	#[test]
	fn mul_g1_works() {
		mul_test::<G1Affine, ark_bls12_381::G1Affine>();
	}

	#[test]
	fn msm_g1_works() {
		msm_test::<G1Affine, ark_bls12_381::G1Affine>();
	}

	#[test]
	fn mul_g2_works() {
		mul_test::<G2Affine, ark_bls12_381::G2Affine>();
	}

	#[test]
	fn msm_g2_works() {
		msm_test::<G2Affine, ark_bls12_381::G2Affine>();
	}

	#[test]
	fn pairing_works() {
		pairing_test::<Bls12_381, ark_bls12_381::Bls12_381>();
	}
}
