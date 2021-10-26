// Copyright 2021 IOTA Stiftung
// SPDX-License-Identifier: Apache-2.0

use super::*;
use bee_common::packable::Packable;
use bee_message::{
    prelude::{
        Output,
        TransactionId,
        TreasuryInput,
        UnlockBlock,
        UtxoInput,
    },
    MessageId,
};
use std::{
    io::Cursor,
    ops::{
        Deref,
        DerefMut,
    },
};

/// Index type
pub type Index = u16;
/// Amount type
pub type Amount = u64;
/// Output type
pub type OutputType = u8;
/// ParentIndex type
pub type ParentIndex = u16;
/// Identify theoretical nodeid which updated/set the synced_by column in sync table
pub type SyncedBy = u8;
/// Identify theoretical nodeid which updated/set the logged_by column in sync table.
/// This enables the admin to locate the generated logs across cluster of chronicles
pub type LoggedBy = u8;

/// A `bee` type wrapper which is used to apply the `ColumnEncoder`
/// functionality over predefined types which are `Packable`.
#[derive(Copy, Clone, Serialize, Deserialize, Hash, PartialEq, Eq)]
pub struct Bee<Type> {
    inner: Type,
}

impl<Type> Bee<Type> {
    /// Wrap a `bee` type
    pub fn wrap(t: Type) -> Bee<Type> {
        Bee { inner: t }
    }

    /// Consume the wrapper and return the inner `bee` type
    pub fn into_inner(self) -> Type {
        self.inner
    }
}

impl<Type> Deref for Bee<Type> {
    type Target = Type;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<Type> DerefMut for Bee<Type> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl<Type> From<Type> for Bee<Type> {
    fn from(t: Type) -> Self {
        Bee::wrap(t)
    }
}

impl<P: Packable> ColumnDecoder for Bee<P> {
    fn try_decode_column(slice: &[u8]) -> anyhow::Result<Self> {
        P::unpack(&mut Cursor::new(slice))
            .map_err(|e| anyhow!("{:?}", e))
            .map(Into::into)
    }
}

/// A transaction's unlock data, to be stored in a `transactions` row.
/// Holds a reference to the input which it signs.
#[derive(Debug, Clone)]
pub struct UnlockData {
    /// it holds the transaction_id of the input which created the unlock_block
    pub input_tx_id: TransactionId,
    /// it holds the input_index of the input which created the unlock_block
    pub input_index: u16,
    /// it's the unlock_block
    pub unlock_block: UnlockBlock,
}
impl UnlockData {
    /// Creates a new unlock data
    pub fn new(input_tx_id: TransactionId, input_index: u16, unlock_block: UnlockBlock) -> Self {
        Self {
            input_tx_id,
            input_index,
            unlock_block,
        }
    }
}
impl Packable for UnlockData {
    type Error = anyhow::Error;
    fn packed_len(&self) -> usize {
        self.input_tx_id.packed_len() + self.input_index.packed_len() + self.unlock_block.packed_len()
    }
    fn pack<W: std::io::Write>(&self, writer: &mut W) -> Result<(), Self::Error> {
        self.input_tx_id.pack(writer)?;
        self.input_index.pack(writer)?;
        self.unlock_block.pack(writer)?;
        Ok(())
    }
    fn unpack_inner<R: std::io::Read + ?Sized, const CHECK: bool>(reader: &mut R) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        Ok(Self {
            input_tx_id: TransactionId::unpack(reader)?,
            input_index: u16::unpack(reader)?,
            unlock_block: UnlockBlock::unpack(reader)?,
        })
    }
}

/// A transaction's input data, to be stored in a `transactions` row.
#[derive(Debug, Clone)]
pub enum InputData {
    /// An regular Input which spends a prior Output and its unlock block
    Utxo(UtxoInput, UnlockBlock),
    /// A special input for migrating funds from another network
    Treasury(TreasuryInput),
}

impl InputData {
    /// Creates a regular Input Data
    pub fn utxo(utxo_input: UtxoInput, unlock_block: UnlockBlock) -> Self {
        Self::Utxo(utxo_input, unlock_block)
    }
    /// Creates a special migration Input Data
    pub fn treasury(treasury_input: TreasuryInput) -> Self {
        Self::Treasury(treasury_input)
    }
}

impl Packable for InputData {
    type Error = anyhow::Error;
    fn packed_len(&self) -> usize {
        match self {
            InputData::Utxo(utxo_input, unlock_block) => {
                0u8.packed_len() + utxo_input.packed_len() + unlock_block.packed_len()
            }
            InputData::Treasury(treasury_input) => 0u8.packed_len() + treasury_input.packed_len(),
        }
    }
    fn pack<W: std::io::Write>(&self, writer: &mut W) -> Result<(), Self::Error> {
        match self {
            InputData::Utxo(utxo_input, unlock_block) => {
                0u8.pack(writer)?;
                utxo_input.pack(writer)?;
                unlock_block.pack(writer)?;
            }
            InputData::Treasury(treasury_input) => {
                1u8.pack(writer)?;
                treasury_input.pack(writer)?;
            }
        }
        Ok(())
    }
    fn unpack_inner<R: std::io::Read + ?Sized, const CHECK: bool>(reader: &mut R) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        Ok(match u8::unpack(reader)? {
            0 => InputData::Utxo(UtxoInput::unpack(reader)?, UnlockBlock::unpack(reader)?),
            1 => InputData::Treasury(TreasuryInput::unpack(reader)?),
            _ => bail!("Tried to unpack an invalid inputdata variant!"),
        })
    }
}

// input unlocked my input
#[derive(Debug, Clone)]
/// Chrysalis transaction data
pub enum TransactionData {
    /// An unspent transaction input
    Input(InputData),
    /// A transaction output
    Output(Output),
    /// A signed block which can be used to unlock an input
    Unlock(UnlockData),
}

impl Packable for TransactionData {
    type Error = anyhow::Error;

    fn packed_len(&self) -> usize {
        match self {
            TransactionData::Input(utxo_input) => 0u8.packed_len() + utxo_input.packed_len(),
            TransactionData::Output(output) => 0u8.packed_len() + output.packed_len(),
            TransactionData::Unlock(block) => 0u8.packed_len() + block.packed_len(),
        }
    }

    fn pack<W: std::io::Write>(&self, writer: &mut W) -> Result<(), Self::Error> {
        match self {
            TransactionData::Input(input_data) => {
                0u8.pack(writer)?;
                input_data.pack(writer)?;
            }
            TransactionData::Output(output) => {
                1u8.pack(writer)?;
                output.pack(writer)?;
            }
            TransactionData::Unlock(block_data) => {
                2u8.pack(writer)?;
                block_data.pack(writer)?;
            }
        }
        Ok(())
    }

    fn unpack_inner<R: std::io::Read + ?Sized, const CHECK: bool>(reader: &mut R) -> Result<Self, Self::Error>
    where
        Self: Sized,
    {
        Ok(match u8::unpack(reader)? {
            0 => TransactionData::Input(InputData::unpack(reader)?),
            1 => TransactionData::Output(Output::unpack(reader)?),
            2 => TransactionData::Unlock(UnlockData::unpack(reader)?),
            _ => bail!("Tried to unpack an invalid transaction variant!"),
        })
    }
}

impl ColumnDecoder for TransactionData {
    fn try_decode_column(slice: &[u8]) -> anyhow::Result<Self> {
        Self::unpack(&mut Cursor::new(slice)).map(Into::into)
    }
}
/// MessageMetadata storage object
#[allow(missing_docs)]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct MessageMetadata {
    #[serde(rename = "messageId")]
    pub message_id: MessageId,
    #[serde(rename = "parentMessageIds")]
    pub parent_message_ids: Vec<MessageId>,
    #[serde(rename = "isSolid")]
    pub is_solid: bool,
    #[serde(rename = "referencedByMilestoneIndex")]
    pub referenced_by_milestone_index: Option<u32>,
    #[serde(rename = "ledgerInclusionState")]
    pub ledger_inclusion_state: Option<LedgerInclusionState>,
    #[serde(rename = "shouldPromote")]
    pub should_promote: Option<bool>,
    #[serde(rename = "shouldReattach")]
    pub should_reattach: Option<bool>,
}

/// A message's ledger inclusion state
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum LedgerInclusionState {
    /// A conflicting message, ex. a double spend
    #[serde(rename = "conflicting")]
    Conflicting,
    /// A successful, included message
    #[serde(rename = "included")]
    Included,
    /// A message without a transaction
    #[serde(rename = "noTransaction")]
    NoTransaction,
}

impl ColumnEncoder for LedgerInclusionState {
    fn encode(&self, buffer: &mut Vec<u8>) {
        let bytes = bincode_config().serialize(self).unwrap();
        buffer.extend(&i32::to_be_bytes(bytes.len() as i32));
        buffer.extend(bytes)
    }
}

impl ColumnDecoder for LedgerInclusionState {
    fn try_decode_column(slice: &[u8]) -> anyhow::Result<Self> {
        bincode_config().deserialize(slice).map_err(Into::into)
    }
}

impl ColumnEncoder for MessageMetadata {
    fn encode(&self, buffer: &mut Vec<u8>) {
        let bytes = bincode_config().serialize(self).unwrap();
        buffer.extend(&i32::to_be_bytes(bytes.len() as i32));
        buffer.extend(bytes)
    }
}

impl ColumnDecoder for MessageMetadata {
    fn try_decode_column(slice: &[u8]) -> anyhow::Result<Self> {
        bincode_config().deserialize(slice).map_err(Into::into)
    }
}
impl ColumnEncoder for TransactionData {
    fn encode(&self, buffer: &mut Vec<u8>) {
        let mut bytes = Vec::new();
        self.pack(&mut bytes).expect("Unable to pack TransactionData");
        buffer.extend(&i32::to_be_bytes(bytes.len() as i32));
        buffer.extend(bytes)
    }
}

/// A result struct which holds a retrieved output as well as all associated unlock blocks
#[derive(Debug, Clone)]
pub struct OutputRes {
    /// The created output's message id
    pub message_id: MessageId,
    /// The output
    pub output: Output,
    /// Zero or more unlock blocks for this output.
    /// Only one can be valid, which indicates the output `is_spent`.
    pub unlock_blocks: Vec<UnlockRes>,
}

/// A result struct which holds a retrieved transaction
#[derive(Debug, Clone)]
pub struct TransactionRes {
    /// The transaction's message id
    pub message_id: MessageId,
    /// The transaction's milestone index
    pub milestone_index: Option<MilestoneIndex>,
    /// The output
    pub outputs: Vec<(Output, Option<UnlockRes>)>,
    /// The inputs, if any exist
    pub inputs: Vec<InputData>,
}

/// A result struct which holds an unlock row from the `transactions` table
#[derive(Debug, Clone)]
pub struct UnlockRes {
    /// The message ID for the transaction which this unlocks
    pub message_id: MessageId,
    /// The unlock block
    pub block: UnlockBlock,
    /// This transaction's ledger inclusion state
    pub inclusion_state: Option<LedgerInclusionState>,
}

/// A "full" message payload, including both message and metadata
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FullMessage(pub Message, pub MessageMetadata);

impl FullMessage {
    /// Create a new full message
    pub fn new(message: Message, metadata: MessageMetadata) -> Self {
        Self(message, metadata)
    }
    /// Get the message ID
    pub fn message_id(&self) -> &MessageId {
        &self.1.message_id
    }
    /// Get the message's metadata
    pub fn metadata(&self) -> &MessageMetadata {
        &self.1
    }
    /// Get the message
    pub fn message(&self) -> &Message {
        &self.0
    }
    /// Get the milestone index that references this
    pub fn ref_ms(&self) -> Option<u32> {
        self.1.referenced_by_milestone_index
    }
}

/// A type alias for partition ids
pub type PartitionId = u16;

/// An index in plain-text, unhashed
#[derive(Clone)]
pub struct Indexation(pub String);

/// A hint, used to lookup in the `hints` table
#[derive(Clone)]
pub struct Hint {
    /// The hint string
    pub hint: String,
    /// The hint variant. Can be 'parent', 'address', or 'index'.
    pub variant: HintVariant,
}

impl Hint {
    /// Creates a new index hint
    pub fn index(index: String) -> Self {
        Self {
            hint: index,
            variant: HintVariant::Index,
        }
    }

    /// Creates a new address hint
    pub fn address(address: String) -> Self {
        Self {
            hint: address,
            variant: HintVariant::Address,
        }
    }

    /// Creates a new parent hint
    pub fn parent(parent: String) -> Self {
        Self {
            hint: parent,
            variant: HintVariant::Parent,
        }
    }
}

/// Hint variants
#[derive(Clone)]
pub enum HintVariant {
    /// An address
    Address,
    /// An unhashed index
    Index,
    /// A parent message id
    Parent,
}

impl std::fmt::Display for HintVariant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}",
            match self {
                HintVariant::Address => "address",
                HintVariant::Index => "index",
                HintVariant::Parent => "parent",
            }
        )
    }
}

/// A marker for a paged result
#[derive(Clone, Debug)]
pub struct Paged<T> {
    inner: T,
    /// The paging state for the query
    pub paging_state: Option<Vec<u8>>,
}

impl<T> Paged<T> {
    /// Creates a new paged marker with an inner type and a paging state
    pub fn new(inner: T, paging_state: Option<Vec<u8>>) -> Self {
        Self { inner, paging_state }
    }
}

impl<T> Deref for Paged<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl<T> DerefMut for Paged<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

/// Wrapper for json data
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct JsonData<T> {
    data: T,
}

impl<T> Deref for JsonData<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.data
    }
}

impl<T> Wrapper for JsonData<T> {
    fn into_inner(self) -> Self::Target {
        self.data
    }
}
#[derive(Clone, Debug)]
/// Wrapper around MessageCount u32
pub struct MessageCount(pub u32);
impl Deref for MessageCount {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
#[derive(Clone, Debug)]
/// Wrapper around TransactionCount u32
pub struct TransactionCount(pub u32);
impl Deref for TransactionCount {
    type Target = u32;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
#[derive(Clone, Debug)]
/// Wrapper around MessageCount u64
pub struct TransferredTokens(pub u64);
impl Deref for TransferredTokens {
    type Target = u64;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
#[derive(Clone, Debug)]
/// MilestoneData analytics information.
pub struct AnalyticRecord {
    /// Duh it's the milestone index
    pub milestone_index: MilestoneIndex,
    /// The total number of messages within a milestone cone
    pub message_count: MessageCount,
    /// The total number of transactions within a milestone cone
    pub transaction_count: TransactionCount,
    /// Transferred IOTA tokens volume within a milestone cone
    pub transferred_tokens: TransferredTokens,
}

impl AnalyticRecord {
    /// Create new MilestoneDataInfo object
    pub fn new(
        milestone_index: MilestoneIndex,
        message_count: MessageCount,
        transaction_count: TransactionCount,
        transferred_tokens: TransferredTokens,
    ) -> Self {
        Self {
            milestone_index,
            message_count,
            transaction_count,
            transferred_tokens,
        }
    }
    /// Gets the milestone index
    pub fn milestone_index(&self) -> &MilestoneIndex {
        &self.milestone_index
    }
    /// Gets the message_count
    pub fn message_count(&self) -> &MessageCount {
        &self.message_count
    }
    /// Gets the transaction count
    pub fn transaction_count(&self) -> &TransactionCount {
        &self.transaction_count
    }
    /// Gets the transferred tokens
    pub fn transferred_tokens(&self) -> &TransferredTokens {
        &self.transferred_tokens
    }
}

#[derive(Debug, Clone)]
/// Sync key, used to select or insert sync records into sync table;
pub struct SyncKey {
    pub(crate) sync_range: SyncRange,
}
impl SyncKey {
    /// The start range
    pub fn start(&self) -> u32 {
        self.sync_range.from
    }
    /// The end range 
    pub fn end(&self) -> u32 {
        self.sync_range.to
    }
}

impl From<SyncRange> for SyncKey {
    fn from(sync_range: SyncRange) -> Self {
        Self { sync_range }
    }
}

impl TokenEncoder for SyncKey {
    fn token(&self) -> i64 {
        "permanode".token()
    }
}
