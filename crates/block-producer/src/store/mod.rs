use std::{
    collections::BTreeMap,
    fmt::{Display, Formatter},
    num::NonZeroU32,
};

use async_trait::async_trait;
use itertools::Itertools;
use miden_node_proto::{
    domain::notes::NoteAuthenticationInfo,
    errors::{ConversionError, MissingFieldHelper},
    generated::{
        digest,
        requests::{
            ApplyBlockRequest, GetBlockInputsRequest, GetNoteAuthenticationInfoRequest,
            GetTransactionInputsRequest,
        },
        responses::{GetTransactionInputsResponse, NullifierTransactionInputRecord},
        store::api_client as store_client,
    },
    AccountState,
};
use miden_node_utils::formatting::format_opt;
use miden_objects::{
    accounts::AccountId,
    block::Block,
    notes::{NoteId, Nullifier},
    utils::Serializable,
    Digest,
};
use miden_processor::crypto::RpoDigest;
use tonic::transport::Channel;
use tracing::{debug, info, instrument};

pub use crate::errors::{ApplyBlockError, BlockInputsError, TxInputsError};
use crate::{block::BlockInputs, errors::NotePathsError, ProvenTransaction, COMPONENT};

// STORE TRAIT
// ================================================================================================

#[async_trait]
pub trait Store: ApplyBlock {
    /// Returns information needed from the store to verify a given proven transaction.
    async fn get_tx_inputs(
        &self,
        proven_tx: &ProvenTransaction,
    ) -> Result<TransactionInputs, TxInputsError>;

    /// Returns information needed from the store to build a block.
    async fn get_block_inputs(
        &self,
        updated_accounts: impl Iterator<Item = AccountId> + Send,
        produced_nullifiers: impl Iterator<Item = &Nullifier> + Send,
        notes: impl Iterator<Item = &NoteId> + Send,
    ) -> Result<BlockInputs, BlockInputsError>;

    /// Returns note authentication information for the set of specified notes.
    ///
    /// If authentication info for a note does not exist in the store, the note is omitted
    /// from the returned set of notes.
    async fn get_note_authentication_info(
        &self,
        notes: impl Iterator<Item = &NoteId> + Send,
    ) -> Result<NoteAuthenticationInfo, NotePathsError>;
}

#[async_trait]
pub trait ApplyBlock: Send + Sync + 'static {
    async fn apply_block(&self, block: &Block) -> Result<(), ApplyBlockError>;
}

// TRANSACTION INPUTS
// ================================================================================================

/// Information needed from the store to verify a transaction.
#[derive(Debug)]
pub struct TransactionInputs {
    /// Account ID
    pub account_id: AccountId,
    /// The account hash in the store corresponding to tx's account ID
    pub account_hash: Option<Digest>,
    /// Maps each consumed notes' nullifier to block number, where the note is consumed.
    ///
    /// We use NonZeroU32 as the wire format uses 0 to encode none.
    pub nullifiers: BTreeMap<Nullifier, Option<NonZeroU32>>,
    /// List of unauthenticated notes that were not found in the store
    pub missing_unauthenticated_notes: Vec<NoteId>,
    /// The current block height
    pub current_block_height: u32,
}

impl Display for TransactionInputs {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let nullifiers = self
            .nullifiers
            .iter()
            .map(|(k, v)| format!("{k}: {}", format_opt(v.as_ref())))
            .join(", ");

        let nullifiers = if nullifiers.is_empty() {
            "None".to_owned()
        } else {
            format!("{{ {} }}", nullifiers)
        };

        f.write_fmt(format_args!(
            "{{ account_id: {}, account_hash: {}, nullifiers: {} }}",
            self.account_id,
            format_opt(self.account_hash.as_ref()),
            nullifiers
        ))
    }
}

impl TryFrom<GetTransactionInputsResponse> for TransactionInputs {
    type Error = ConversionError;

    fn try_from(response: GetTransactionInputsResponse) -> Result<Self, Self::Error> {
        let AccountState { account_id, account_hash } = response
            .account_state
            .ok_or(GetTransactionInputsResponse::missing_field(stringify!(account_state)))?
            .try_into()?;

        let mut nullifiers = BTreeMap::new();
        for nullifier_record in response.nullifiers {
            let nullifier = nullifier_record
                .nullifier
                .ok_or(NullifierTransactionInputRecord::missing_field(stringify!(nullifier)))?
                .try_into()?;

            // Note that this intentionally maps 0 to None as this is the definition used in
            // protobuf.
            nullifiers.insert(nullifier, NonZeroU32::new(nullifier_record.block_num));
        }

        let missing_unauthenticated_notes = response
            .missing_unauthenticated_notes
            .into_iter()
            .map(|digest| Ok(RpoDigest::try_from(digest)?.into()))
            .collect::<Result<Vec<_>, ConversionError>>()?;

        let current_block_height = response.block_height;

        Ok(Self {
            account_id,
            account_hash,
            nullifiers,
            missing_unauthenticated_notes,
            current_block_height,
        })
    }
}

// DEFAULT STORE IMPLEMENTATION
// ================================================================================================

pub struct DefaultStore {
    store: store_client::ApiClient<Channel>,
}

impl DefaultStore {
    /// TODO: this should probably take store connection string and create a connection internally
    pub fn new(store: store_client::ApiClient<Channel>) -> Self {
        Self { store }
    }
}

#[async_trait]
impl ApplyBlock for DefaultStore {
    #[instrument(target = "miden-block-producer", skip_all, err)]
    async fn apply_block(&self, block: &Block) -> Result<(), ApplyBlockError> {
        let request = tonic::Request::new(ApplyBlockRequest { block: block.to_bytes() });

        let _ = self
            .store
            .clone()
            .apply_block(request)
            .await
            .map_err(|status| ApplyBlockError::GrpcClientError(status.message().to_string()))?;

        Ok(())
    }
}

#[async_trait]
impl Store for DefaultStore {
    #[instrument(target = "miden-block-producer", skip_all, err)]
    async fn get_tx_inputs(
        &self,
        proven_tx: &ProvenTransaction,
    ) -> Result<TransactionInputs, TxInputsError> {
        let message = GetTransactionInputsRequest {
            account_id: Some(proven_tx.account_id().into()),
            nullifiers: proven_tx.get_nullifiers().map(Into::into).collect(),
            unauthenticated_notes: proven_tx
                .get_unauthenticated_notes()
                .map(|note| note.id().into())
                .collect(),
        };

        info!(target: COMPONENT, tx_id = %proven_tx.id().to_hex());
        debug!(target: COMPONENT, ?message);

        let request = tonic::Request::new(message);
        let response = self
            .store
            .clone()
            .get_transaction_inputs(request)
            .await
            .map_err(|status| TxInputsError::GrpcClientError(status.message().to_string()))?
            .into_inner();

        debug!(target: COMPONENT, ?response);

        let tx_inputs: TransactionInputs = response.try_into()?;

        if tx_inputs.account_id != proven_tx.account_id() {
            return Err(TxInputsError::MalformedResponse(format!(
                "incorrect account id returned from store. Got: {}, expected: {}",
                tx_inputs.account_id,
                proven_tx.account_id()
            )));
        }

        debug!(target: COMPONENT, %tx_inputs);

        Ok(tx_inputs)
    }

    async fn get_block_inputs(
        &self,
        updated_accounts: impl Iterator<Item = AccountId> + Send,
        produced_nullifiers: impl Iterator<Item = &Nullifier> + Send,
        notes: impl Iterator<Item = &NoteId> + Send,
    ) -> Result<BlockInputs, BlockInputsError> {
        let request = tonic::Request::new(GetBlockInputsRequest {
            account_ids: updated_accounts.map(Into::into).collect(),
            nullifiers: produced_nullifiers.map(digest::Digest::from).collect(),
            unauthenticated_notes: notes.map(digest::Digest::from).collect(),
        });

        let store_response = self
            .store
            .clone()
            .get_block_inputs(request)
            .await
            .map_err(|err| BlockInputsError::GrpcClientError(err.message().to_string()))?
            .into_inner();

        Ok(store_response.try_into()?)
    }

    async fn get_note_authentication_info(
        &self,
        notes: impl Iterator<Item = &NoteId> + Send,
    ) -> Result<NoteAuthenticationInfo, NotePathsError> {
        let request = tonic::Request::new(GetNoteAuthenticationInfoRequest {
            note_ids: notes.map(digest::Digest::from).collect(),
        });

        let store_response = self
            .store
            .clone()
            .get_note_authentication_info(request)
            .await
            .map_err(|err| NotePathsError::GrpcClientError(err.message().to_string()))?
            .into_inner();

        let note_authentication_info = store_response
            .proofs
            .ok_or(GetTransactionInputsResponse::missing_field("proofs"))?
            .try_into()?;

        Ok(note_authentication_info)
    }
}
