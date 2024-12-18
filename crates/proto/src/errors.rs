use std::{any::type_name, num::TryFromIntError};

use miden_objects::crypto::merkle::{SmtLeafError, SmtProofError};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Error)]
pub enum ConversionError {
    #[error("Hex error: {0}")]
    HexError(#[from] hex::FromHexError),
    #[error("Note error: {0}")]
    NoteError(#[from] miden_objects::NoteError),
    #[error("SMT leaf error: {0}")]
    SmtLeafError(#[from] SmtLeafError),
    #[error("SMT proof error: {0}")]
    SmtProofError(#[from] SmtProofError),
    #[error("Integer conversion error: {0}")]
    TryFromIntError(#[from] TryFromIntError),
    #[error("Too much data, expected {expected}, got {got}")]
    TooMuchData { expected: usize, got: usize },
    #[error("Not enough data, expected {expected}, got {got}")]
    InsufficientData { expected: usize, got: usize },
    #[error("Value is not in the range 0..MODULUS")]
    NotAValidFelt,
    #[error("Field `{field_name}` required to be filled in protobuf representation of {entity}")]
    MissingFieldInProtobufRepresentation {
        entity: &'static str,
        field_name: &'static str,
    },
}

impl Eq for ConversionError {}

pub trait MissingFieldHelper {
    fn missing_field(field_name: &'static str) -> ConversionError;
}

impl<T: prost::Message> MissingFieldHelper for T {
    fn missing_field(field_name: &'static str) -> ConversionError {
        ConversionError::MissingFieldInProtobufRepresentation {
            entity: type_name::<T>(),
            field_name,
        }
    }
}
