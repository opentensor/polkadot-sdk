//! Error types for the MEV Shield.

extern crate alloc;

use alloc::vec::Vec;
use codec::{Decode, DecodeWithMemTracking, Encode};
use core::fmt;
use scale_info::TypeInfo;

/// Top-level error returned when processing a shielded transaction fails.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum ShieldError {
	/// Failed to parse the ciphertext envelope.
	Parsing(ParsingError),
	/// The wrapper extrinsic itself is invalid.
	WrapperExtrinsic(WrapperExtrinsicError),
	/// Decryption of the shielded payload failed.
	Decryption(DecryptionError),
	/// The decrypted inner extrinsic is invalid.
	InnerExtrinsic(InnerExtrinsicError),
}

/// Errors when parsing the ciphertext envelope (`key_hash || kem_len || kem_ct || nonce ||
/// aead_ct`).
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum ParsingError {
	/// Ciphertext is too short to contain the key hash (16 bytes).
	TruncatedKeyHash,
	/// Ciphertext is too short to contain the KEM ciphertext length field (2 bytes).
	TruncatedKemLen,
	/// KEM ciphertext length field exceeds the remaining ciphertext bytes.
	KemLenExceedsRemaining,
	/// Ciphertext is too short to contain the nonce (24 bytes).
	TruncatedNonce,
	/// Ciphertext has no AEAD payload after the nonce.
	MissingAead,
}

/// Errors when validating the wrapper (`submit_encrypted`) extrinsic.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum WrapperExtrinsicError {
	/// The wrapper extrinsic failed to decode (e.g. exceeded nesting depth).
	/// Contains the debug representation of the decode error.
	DecodeFailed(Vec<u8>),
	/// The wrapper extrinsic failed validation (e.g. bad signature, AncientBirthBlock).
	/// Contains the SCALE-encoded `TransactionValidityError`.
	CheckFailed(Vec<u8>),
}

/// Errors during ML-KEM-768 + XChaCha20-Poly1305 decryption.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum DecryptionError {
	/// The decapsulation key bytes are malformed.
	InvalidDecapsulationKey,
	/// The KEM ciphertext is malformed (wrong length for ML-KEM-768).
	InvalidKemCiphertext,
	/// ML-KEM-768 decapsulation failed.
	DecapsulationFailed,
	/// XChaCha20-Poly1305 decryption failed (wrong key or tampered payload).
	AeadDecryptionFailed,
	/// Decrypted plaintext is empty.
	EmptyPlaintext,
}

/// Errors when processing the decrypted inner extrinsic.
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode, DecodeWithMemTracking, TypeInfo)]
pub enum InnerExtrinsicError {
	/// Decrypted bytes are not a valid SCALE-encoded extrinsic.
	/// Contains the debug representation of the decode error.
	DecodeFailed(Vec<u8>),
	/// The inner extrinsic failed validation (e.g. AncientBirthBlock, BadProof).
	/// Contains the SCALE-encoded `TransactionValidityError`.
	ValidationFailed(Vec<u8>),
}

// Convenience `From` impls so call sites can use `?` with sub-errors.

impl From<ParsingError> for ShieldError {
	fn from(e: ParsingError) -> Self {
		ShieldError::Parsing(e)
	}
}

impl From<WrapperExtrinsicError> for ShieldError {
	fn from(e: WrapperExtrinsicError) -> Self {
		ShieldError::WrapperExtrinsic(e)
	}
}

impl From<DecryptionError> for ShieldError {
	fn from(e: DecryptionError) -> Self {
		ShieldError::Decryption(e)
	}
}

impl From<InnerExtrinsicError> for ShieldError {
	fn from(e: InnerExtrinsicError) -> Self {
		ShieldError::InnerExtrinsic(e)
	}
}

// Display impls — human-readable messages for SDK/wallet developers.

/// Lossy UTF-8 helper for `Vec<u8>` payloads (debug strings or opaque bytes).
fn lossy(bytes: &[u8]) -> alloc::string::String {
	alloc::string::String::from_utf8_lossy(bytes).into_owned()
}

impl fmt::Display for ShieldError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::Parsing(e) => write!(f, "ciphertext parsing failed: {e}"),
			Self::WrapperExtrinsic(e) => write!(f, "wrapper extrinsic invalid: {e}"),
			Self::Decryption(e) => write!(f, "decryption failed: {e}"),
			Self::InnerExtrinsic(e) => write!(f, "inner extrinsic invalid: {e}"),
		}
	}
}

impl fmt::Display for ParsingError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::TruncatedKeyHash =>
				write!(f, "ciphertext too short for key hash (need 16 bytes)"),
			Self::TruncatedKemLen =>
				write!(f, "ciphertext too short for KEM length field (need 2 bytes)"),
			Self::KemLenExceedsRemaining =>
				write!(f, "KEM ciphertext length exceeds remaining data"),
			Self::TruncatedNonce => write!(f, "ciphertext too short for nonce (need 24 bytes)"),
			Self::MissingAead => write!(f, "no AEAD ciphertext after nonce"),
		}
	}
}

impl fmt::Display for WrapperExtrinsicError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::DecodeFailed(msg) => write!(f, "SCALE decode failed: {}", lossy(msg)),
			Self::CheckFailed(encoded) => {
				write!(f, "validation check failed (encoded error: 0x")?;
				for b in encoded {
					write!(f, "{b:02x}")?;
				}
				write!(f, ")")
			},
		}
	}
}

impl fmt::Display for DecryptionError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::InvalidDecapsulationKey => write!(f, "decapsulation key bytes are malformed"),
			Self::InvalidKemCiphertext =>
				write!(f, "KEM ciphertext has wrong length for ML-KEM-768"),
			Self::DecapsulationFailed => write!(f, "ML-KEM-768 decapsulation failed"),
			Self::AeadDecryptionFailed =>
				write!(f, "XChaCha20-Poly1305 decryption failed (wrong key or tampered payload)"),
			Self::EmptyPlaintext => write!(f, "decrypted plaintext is empty"),
		}
	}
}

impl fmt::Display for InnerExtrinsicError {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		match self {
			Self::DecodeFailed(msg) => write!(f, "SCALE decode failed: {}", lossy(msg)),
			Self::ValidationFailed(encoded) => {
				write!(f, "validation failed (encoded error: 0x")?;
				for b in encoded {
					write!(f, "{b:02x}")?;
				}
				write!(f, ")")
			},
		}
	}
}
