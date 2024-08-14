// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <http://www.gnu.org/licenses/>.

//! Messages in the signer-miner interaction have a multi-level hierarchy.
//! Signers send messages to each other through Packet messages. These messages,
//! as well as `BlockResponse`, `Transactions`, and `DkgResults` messages are stored
//! StackerDBs based on the `MessageSlotID` for the particular message type. This is a
//! shared identifier space between the four message kinds and their subtypes.
//!
//! These four message kinds are differentiated with a `SignerMessageTypePrefix`
//! and the `SignerMessage` enum.

use std::fmt::{Debug, Display};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;

use blockstack_lib::chainstate::nakamoto::signer_set::NakamotoSigners;
use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::stacks::events::StackerDBChunksEvent;
use blockstack_lib::chainstate::stacks::StacksTransaction;
use blockstack_lib::net::api::getinfo::RPCPeerInfoData;
use blockstack_lib::net::api::postblock_proposal::{
    BlockValidateReject, BlockValidateResponse, ValidateRejectCode,
};
use blockstack_lib::util_lib::boot::boot_code_id;
use blockstack_lib::util_lib::signed_structured_data::{
    make_structured_data_domain, structured_data_message_hash,
};
use clarity::types::chainstate::{
    BlockHeaderHash, ConsensusHash, StacksPrivateKey, StacksPublicKey,
};
use clarity::types::PrivateKey;
use clarity::util::hash::Sha256Sum;
use clarity::util::retry::BoundReader;
use clarity::util::secp256k1::MessageSignature;
use clarity::vm::types::serialization::SerializationError;
use clarity::vm::types::{QualifiedContractIdentifier, TupleData};
use clarity::vm::Value;
use hashbrown::{HashMap, HashSet};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha512_256};
use stacks_common::codec::{
    read_next, read_next_at_most, read_next_exact, write_next, Error as CodecError,
    StacksMessageCodec,
};
use stacks_common::consts::SIGNER_SLOTS_PER_USER;
use stacks_common::util::hash::Sha512Trunc256Sum;
use tiny_http::{
    Method as HttpMethod, Request as HttpRequest, Response as HttpResponse, Server as HttpServer,
};

use crate::http::{decode_http_body, decode_http_request};
use crate::stacks_common::types::PublicKey;
use crate::{
    BlockProposal, EventError, MessageSlotID as MessageSlotIDTrait,
    SignerMessage as SignerMessageTrait,
};

define_u8_enum!(
/// Enum representing the stackerdb message identifier: this is
///  the contract index in the signers contracts (i.e., X in signers-0-X)
MessageSlotID {
    /// Block Response message from signers
    BlockResponse = 1,
    /// Mock Signature message from Epoch 2.5 signers
    MockSignature = 2
});

define_u8_enum!(
/// Enum representing the slots used by the miner
MinerSlotID {
    /// Block proposal from the miner
    BlockProposal = 0,
    /// Block pushed from the miner
    BlockPushed = 1
});

impl MessageSlotIDTrait for MessageSlotID {
    fn stacker_db_contract(&self, mainnet: bool, reward_cycle: u64) -> QualifiedContractIdentifier {
        NakamotoSigners::make_signers_db_contract_id(reward_cycle, self.to_u32(), mainnet)
    }
    fn all() -> &'static [Self] {
        MessageSlotID::ALL
    }
}

impl SignerMessageTrait<MessageSlotID> for SignerMessage {
    fn msg_id(&self) -> Option<MessageSlotID> {
        self.msg_id()
    }
}

define_u8_enum!(
/// Enum representing the SignerMessage type prefix
SignerMessageTypePrefix {
    /// Block Proposal message from miners
    BlockProposal = 0,
    /// Block Response message from signers
    BlockResponse = 1,
    /// Block Pushed message from miners
    BlockPushed = 2,
    /// Mock Signature message from Epoch 2.5 signers
    MockSignature = 3,
    /// Mock Pre-Nakamoto message from Epoch 2.5 miners
    MockMinerMessage = 4
});

#[cfg_attr(test, mutants::skip)]
impl MessageSlotID {
    /// Return the StackerDB contract corresponding to messages of this type
    pub fn stacker_db_contract(
        &self,
        mainnet: bool,
        reward_cycle: u64,
    ) -> QualifiedContractIdentifier {
        NakamotoSigners::make_signers_db_contract_id(reward_cycle, self.to_u32(), mainnet)
    }

    /// Return the u32 identifier for the message slot (used to index the contract that stores it)
    pub fn to_u32(self) -> u32 {
        self.to_u8().into()
    }
}

#[cfg_attr(test, mutants::skip)]
impl Display for MessageSlotID {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}({})", self, self.to_u8())
    }
}

impl TryFrom<u8> for SignerMessageTypePrefix {
    type Error = CodecError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or_else(|| {
            CodecError::DeserializeError(format!("Unknown signer message type prefix: {value}"))
        })
    }
}

impl From<&SignerMessage> for SignerMessageTypePrefix {
    #[cfg_attr(test, mutants::skip)]
    fn from(message: &SignerMessage) -> Self {
        match message {
            SignerMessage::BlockProposal(_) => SignerMessageTypePrefix::BlockProposal,
            SignerMessage::BlockResponse(_) => SignerMessageTypePrefix::BlockResponse,
            SignerMessage::BlockPushed(_) => SignerMessageTypePrefix::BlockPushed,
            SignerMessage::MockSignature(_) => SignerMessageTypePrefix::MockSignature,
            SignerMessage::MockMinerMessage(_) => SignerMessageTypePrefix::MockMinerMessage,
        }
    }
}

/// The messages being sent through the stacker db contracts
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SignerMessage {
    /// The block proposal from miners for signers to observe and sign
    BlockProposal(BlockProposal),
    /// The block response from signers for miners to observe
    BlockResponse(BlockResponse),
    /// A block pushed from miners to the signers set
    BlockPushed(NakamotoBlock),
    /// A mock signature from the epoch 2.5 signers
    MockSignature(MockSignature),
    /// A mock message from the epoch 2.5 miners
    MockMinerMessage(MockMinerMessage),
}

impl SignerMessage {
    /// Helper function to determine the slot ID for the provided stacker-db writer id
    ///  Not every message has a `MessageSlotID`: messages from the miner do not
    ///   broadcast over `.signers-0-X` contracts.
    #[cfg_attr(test, mutants::skip)]
    pub fn msg_id(&self) -> Option<MessageSlotID> {
        match self {
            Self::BlockProposal(_) | Self::BlockPushed(_) | Self::MockMinerMessage(_) => None,
            Self::BlockResponse(_) => Some(MessageSlotID::BlockResponse),
            Self::MockSignature(_) => Some(MessageSlotID::MockSignature),
        }
    }
}

impl StacksMessageCodec for SignerMessage {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        SignerMessageTypePrefix::from(self)
            .to_u8()
            .consensus_serialize(fd)?;
        match self {
            SignerMessage::BlockProposal(block_proposal) => block_proposal.consensus_serialize(fd),
            SignerMessage::BlockResponse(block_response) => block_response.consensus_serialize(fd),
            SignerMessage::BlockPushed(block) => block.consensus_serialize(fd),
            SignerMessage::MockSignature(signature) => signature.consensus_serialize(fd),
            SignerMessage::MockMinerMessage(message) => message.consensus_serialize(fd),
        }?;
        Ok(())
    }

    #[cfg_attr(test, mutants::skip)]
    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let type_prefix_byte = u8::consensus_deserialize(fd)?;
        let type_prefix = SignerMessageTypePrefix::try_from(type_prefix_byte)?;
        let message = match type_prefix {
            SignerMessageTypePrefix::BlockProposal => {
                let block_proposal = StacksMessageCodec::consensus_deserialize(fd)?;
                SignerMessage::BlockProposal(block_proposal)
            }
            SignerMessageTypePrefix::BlockResponse => {
                let block_response = StacksMessageCodec::consensus_deserialize(fd)?;
                SignerMessage::BlockResponse(block_response)
            }
            SignerMessageTypePrefix::BlockPushed => {
                let block = StacksMessageCodec::consensus_deserialize(fd)?;
                SignerMessage::BlockPushed(block)
            }
            SignerMessageTypePrefix::MockSignature => {
                let signature = StacksMessageCodec::consensus_deserialize(fd)?;
                SignerMessage::MockSignature(signature)
            }
            SignerMessageTypePrefix::MockMinerMessage => {
                let message = StacksMessageCodec::consensus_deserialize(fd)?;
                SignerMessage::MockMinerMessage(message)
            }
        };
        Ok(message)
    }
}

/// Work around for the fact that a lot of the structs being desierialized are not defined in messages.rs
pub trait StacksMessageCodecExtensions: Sized {
    /// Serialize the struct to the provided writer
    fn inner_consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError>;
    /// Deserialize the struct from the provided reader
    fn inner_consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError>;
}

/// The signer relevant peer information from the stacks node
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PeerInfo {
    /// The burn block height
    pub burn_block_height: u64,
    /// The consensus hash of the stacks tip
    pub stacks_tip_consensus_hash: ConsensusHash,
    /// The stacks tip
    pub stacks_tip: BlockHeaderHash,
    /// The stacks tip height
    pub stacks_tip_height: u64,
    /// The pox consensus
    pub pox_consensus: ConsensusHash,
    /// The server version
    pub server_version: String,
}

impl StacksMessageCodec for PeerInfo {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &self.burn_block_height)?;
        write_next(fd, self.stacks_tip_consensus_hash.as_bytes())?;
        write_next(fd, &self.stacks_tip)?;
        write_next(fd, &self.stacks_tip_height)?;
        write_next(fd, &(self.server_version.as_bytes().len() as u8))?;
        fd.write_all(self.server_version.as_bytes())
            .map_err(CodecError::WriteError)?;
        write_next(fd, &self.pox_consensus)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let burn_block_height = read_next::<u64, _>(fd)?;
        let stacks_tip_consensus_hash = read_next::<ConsensusHash, _>(fd)?;
        let stacks_tip = read_next::<BlockHeaderHash, _>(fd)?;
        let stacks_tip_height = read_next::<u64, _>(fd)?;
        let len_byte: u8 = read_next(fd)?;
        let mut bytes = vec![0u8; len_byte as usize];
        fd.read_exact(&mut bytes).map_err(CodecError::ReadError)?;
        // must encode a valid string
        let server_version = String::from_utf8(bytes).map_err(|_e| {
            CodecError::DeserializeError(
                "Failed to parse server version name: could not contruct from utf8".to_string(),
            )
        })?;
        let pox_consensus = read_next::<ConsensusHash, _>(fd)?;
        Ok(Self {
            burn_block_height,
            stacks_tip_consensus_hash,
            stacks_tip,
            stacks_tip_height,
            server_version,
            pox_consensus,
        })
    }
}

/// A snapshot of the signer view of the stacks node to be used for mock signing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MockSignData {
    /// The view of the stacks node peer information at the time of the mock signature
    pub peer_info: PeerInfo,
    /// The burn block height of the event that triggered the mock signature
    pub event_burn_block_height: u64,
    /// The chain id for the mock signature
    pub chain_id: u32,
}

impl StacksMessageCodec for MockSignData {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        self.peer_info.consensus_serialize(fd)?;
        write_next(fd, &self.event_burn_block_height)?;
        write_next(fd, &self.chain_id)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let peer_info = PeerInfo::consensus_deserialize(fd)?;
        let event_burn_block_height = read_next::<u64, _>(fd)?;
        let chain_id = read_next::<u32, _>(fd)?;
        Ok(Self {
            peer_info,
            event_burn_block_height,
            chain_id,
        })
    }
}

/// A mock signature for the stacks node to be used for mock signing.
/// This is only used by Epoch 2.5 signers to simulate the signing of a block for every sortition.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MockSignature {
    /// The signature of the mock signature
    signature: MessageSignature,
    /// The data that was signed across
    pub sign_data: MockSignData,
}

impl MockSignature {
    /// Create a new mock sign data struct from the provided event burn block height, peer info, chain id, and private key.
    /// Note that peer burn block height and event burn block height may not be the same if the peer view is stale.
    pub fn new(
        event_burn_block_height: u64,
        peer_info: PeerInfo,
        chain_id: u32,
        stacks_private_key: &StacksPrivateKey,
    ) -> Self {
        let mut sig = Self {
            signature: MessageSignature::empty(),
            sign_data: MockSignData {
                peer_info,
                event_burn_block_height,
                chain_id,
            },
        };
        sig.sign(stacks_private_key)
            .expect("Failed to sign MockSignature");
        sig
    }

    /// The signature hash for the mock signature
    pub fn signature_hash(&self) -> Sha256Sum {
        let domain_tuple =
            make_structured_data_domain("mock-signer", "1.0.0", self.sign_data.chain_id);
        let data_tuple = Value::Tuple(
            TupleData::from_data(vec![
                (
                    "stacks-tip-consensus-hash".into(),
                    Value::buff_from(
                        self.sign_data
                            .peer_info
                            .stacks_tip_consensus_hash
                            .as_bytes()
                            .into(),
                    )
                    .unwrap(),
                ),
                (
                    "stacks-tip".into(),
                    Value::buff_from(self.sign_data.peer_info.stacks_tip.as_bytes().into())
                        .unwrap(),
                ),
                (
                    "stacks-tip-height".into(),
                    Value::UInt(self.sign_data.peer_info.stacks_tip_height.into()),
                ),
                (
                    "server-version".into(),
                    Value::string_ascii_from_bytes(
                        self.sign_data.peer_info.server_version.clone().into(),
                    )
                    .unwrap(),
                ),
                (
                    "event-burn-block-height".into(),
                    Value::UInt(self.sign_data.event_burn_block_height.into()),
                ),
                (
                    "pox-consensus".into(),
                    Value::buff_from(self.sign_data.peer_info.pox_consensus.as_bytes().into())
                        .unwrap(),
                ),
            ])
            .expect("Error creating signature hash"),
        );
        structured_data_message_hash(data_tuple, domain_tuple)
    }

    /// Sign the mock signature and set the internal signature field
    fn sign(&mut self, private_key: &StacksPrivateKey) -> Result<(), String> {
        let signature_hash = self.signature_hash();
        self.signature = private_key.sign(signature_hash.as_bytes())?;
        Ok(())
    }
    /// Verify the mock signature against the provided public key
    pub fn verify(&self, public_key: &StacksPublicKey) -> Result<bool, String> {
        if self.signature == MessageSignature::empty() {
            return Ok(false);
        }
        let signature_hash = self.signature_hash();
        public_key
            .verify(&signature_hash.0, &self.signature)
            .map_err(|e| e.to_string())
    }
}

impl StacksMessageCodec for MockSignature {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &self.signature)?;
        self.sign_data.consensus_serialize(fd)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let signature = read_next::<MessageSignature, _>(fd)?;
        let sign_data = read_next::<MockSignData, _>(fd)?;
        Ok(Self {
            signature,
            sign_data,
        })
    }
}

/// A mock message for the stacks node to be used for mock mining messages
/// This is only used by Epoch 2.5 miners to simulate miners responding to mock signatures
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MockMinerMessage {
    /// The view of the stacks node peer information at the time of the mock signature
    pub peer_info: PeerInfo,
    /// The burn block height of the miner's tenure
    pub tenure_burn_block_height: u64,
    /// The chain id for the mock signature
    pub chain_id: u32,
    /// The mock signatures that the miner received
    pub mock_signatures: Vec<MockSignature>,
}

impl StacksMessageCodec for MockMinerMessage {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        self.peer_info.consensus_serialize(fd)?;
        write_next(fd, &self.tenure_burn_block_height)?;
        write_next(fd, &self.chain_id)?;
        write_next(fd, &self.mock_signatures)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let peer_info = PeerInfo::consensus_deserialize(fd)?;
        let tenure_burn_block_height = read_next::<u64, _>(fd)?;
        let chain_id = read_next::<u32, _>(fd)?;
        let mock_signatures = read_next::<Vec<MockSignature>, _>(fd)?;
        Ok(Self {
            peer_info,
            tenure_burn_block_height,
            chain_id,
            mock_signatures,
        })
    }
}

define_u8_enum!(
/// Enum representing the reject code type prefix
RejectCodeTypePrefix {
    /// The block was rejected due to validation issues
    ValidationFailed = 0,
    /// The block was rejected due to connectivity issues with the signer
    ConnectivityIssues = 1,
    /// The block was rejected in a prior round
    RejectedInPriorRound = 2,
    /// The block was rejected due to no sortition view
    NoSortitionView = 3,
    /// The block was rejected due to a mismatch with expected sortition view
    SortitionViewMismatch = 4
});

impl TryFrom<u8> for RejectCodeTypePrefix {
    type Error = CodecError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or_else(|| {
            CodecError::DeserializeError(format!("Unknown reject code type prefix: {value}"))
        })
    }
}

impl From<&RejectCode> for RejectCodeTypePrefix {
    fn from(reject_code: &RejectCode) -> Self {
        match reject_code {
            RejectCode::ValidationFailed(_) => RejectCodeTypePrefix::ValidationFailed,
            RejectCode::ConnectivityIssues => RejectCodeTypePrefix::ConnectivityIssues,
            RejectCode::RejectedInPriorRound => RejectCodeTypePrefix::RejectedInPriorRound,
            RejectCode::NoSortitionView => RejectCodeTypePrefix::NoSortitionView,
            RejectCode::SortitionViewMismatch => RejectCodeTypePrefix::SortitionViewMismatch,
        }
    }
}

/// This enum is used to supply a `reason_code` for block rejections
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RejectCode {
    /// RPC endpoint Validation failed
    ValidationFailed(ValidateRejectCode),
    /// No Sortition View to verify against
    NoSortitionView,
    /// The block was rejected due to connectivity issues with the signer
    ConnectivityIssues,
    /// The block was rejected in a prior round
    RejectedInPriorRound,
    /// The block was rejected due to a mismatch with expected sortition view
    SortitionViewMismatch,
}

define_u8_enum!(
/// Enum representing the BlockResponse type prefix
BlockResponseTypePrefix {
    /// An accepted block response
    Accepted = 0,
    /// A rejected block response
    Rejected = 1
});

impl TryFrom<u8> for BlockResponseTypePrefix {
    type Error = CodecError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Self::from_u8(value).ok_or_else(|| {
            CodecError::DeserializeError(format!("Unknown block response type prefix: {value}"))
        })
    }
}

impl From<&BlockResponse> for BlockResponseTypePrefix {
    fn from(block_response: &BlockResponse) -> Self {
        match block_response {
            BlockResponse::Accepted(_) => BlockResponseTypePrefix::Accepted,
            BlockResponse::Rejected(_) => BlockResponseTypePrefix::Rejected,
        }
    }
}

/// The response that a signer sends back to observing miners
/// either accepting or rejecting a Nakamoto block with the corresponding reason
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub enum BlockResponse {
    /// The Nakamoto block was accepted and therefore signed
    Accepted((Sha512Trunc256Sum, MessageSignature)),
    /// The Nakamoto block was rejected and therefore not signed
    Rejected(BlockRejection),
}

#[cfg_attr(test, mutants::skip)]
impl std::fmt::Display for BlockResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BlockResponse::Accepted(a) => {
                write!(
                    f,
                    "BlockAccepted: signer_sighash = {}, signature = {}",
                    a.0, a.1
                )
            }
            BlockResponse::Rejected(r) => {
                write!(
                    f,
                    "BlockRejected: signer_sighash = {}, code = {}, reason = {}",
                    r.reason_code, r.reason, r.signer_signature_hash
                )
            }
        }
    }
}

impl BlockResponse {
    /// Create a new accepted BlockResponse for the provided block signer signature hash and signature
    pub fn accepted(hash: Sha512Trunc256Sum, sig: MessageSignature) -> Self {
        Self::Accepted((hash, sig))
    }

    /// Create a new rejected BlockResponse for the provided block signer signature hash and rejection code
    pub fn rejected(hash: Sha512Trunc256Sum, reject_code: RejectCode) -> Self {
        Self::Rejected(BlockRejection::new(hash, reject_code))
    }
}

impl StacksMessageCodec for BlockResponse {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &(BlockResponseTypePrefix::from(self) as u8))?;
        match self {
            BlockResponse::Accepted((hash, sig)) => {
                write_next(fd, hash)?;
                write_next(fd, sig)?;
            }
            BlockResponse::Rejected(rejection) => {
                write_next(fd, rejection)?;
            }
        };
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let type_prefix_byte = read_next::<u8, _>(fd)?;
        let type_prefix = BlockResponseTypePrefix::try_from(type_prefix_byte)?;
        let response = match type_prefix {
            BlockResponseTypePrefix::Accepted => {
                let hash = read_next::<Sha512Trunc256Sum, _>(fd)?;
                let sig = read_next::<MessageSignature, _>(fd)?;
                BlockResponse::Accepted((hash, sig))
            }
            BlockResponseTypePrefix::Rejected => {
                let rejection = read_next::<BlockRejection, _>(fd)?;
                BlockResponse::Rejected(rejection)
            }
        };
        Ok(response)
    }
}

/// A rejection response from a signer for a proposed block
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BlockRejection {
    /// The reason for the rejection
    pub reason: String,
    /// The reason code for the rejection
    pub reason_code: RejectCode,
    /// The signer signature hash of the block that was rejected
    pub signer_signature_hash: Sha512Trunc256Sum,
}

impl BlockRejection {
    /// Create a new BlockRejection for the provided block and reason code
    pub fn new(signer_signature_hash: Sha512Trunc256Sum, reason_code: RejectCode) -> Self {
        Self {
            reason: reason_code.to_string(),
            reason_code,
            signer_signature_hash,
        }
    }
}

impl StacksMessageCodec for BlockRejection {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &self.reason.as_bytes().to_vec())?;
        write_next(fd, &self.reason_code)?;
        write_next(fd, &self.signer_signature_hash)?;
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let reason_bytes = read_next::<Vec<u8>, _>(fd)?;
        let reason = String::from_utf8(reason_bytes).map_err(|e| {
            CodecError::DeserializeError(format!("Failed to decode reason string: {:?}", &e))
        })?;
        let reason_code = read_next::<RejectCode, _>(fd)?;
        let signer_signature_hash = read_next::<Sha512Trunc256Sum, _>(fd)?;
        Ok(Self {
            reason,
            reason_code,
            signer_signature_hash,
        })
    }
}

impl From<BlockValidateReject> for BlockRejection {
    fn from(reject: BlockValidateReject) -> Self {
        Self {
            reason: reject.reason,
            reason_code: RejectCode::ValidationFailed(reject.reason_code),
            signer_signature_hash: reject.signer_signature_hash,
        }
    }
}

impl StacksMessageCodec for RejectCode {
    fn consensus_serialize<W: Write>(&self, fd: &mut W) -> Result<(), CodecError> {
        write_next(fd, &(RejectCodeTypePrefix::from(self) as u8))?;
        // Do not do a single match here as we may add other variants in the future and don't want to miss adding it
        match self {
            RejectCode::ValidationFailed(code) => write_next(fd, &(*code as u8))?,
            RejectCode::ConnectivityIssues
            | RejectCode::RejectedInPriorRound
            | RejectCode::NoSortitionView
            | RejectCode::SortitionViewMismatch => {
                // No additional data to serialize / deserialize
            }
        };
        Ok(())
    }

    fn consensus_deserialize<R: Read>(fd: &mut R) -> Result<Self, CodecError> {
        let type_prefix_byte = read_next::<u8, _>(fd)?;
        let type_prefix = RejectCodeTypePrefix::try_from(type_prefix_byte)?;
        let code = match type_prefix {
            RejectCodeTypePrefix::ValidationFailed => RejectCode::ValidationFailed(
                ValidateRejectCode::try_from(read_next::<u8, _>(fd)?).map_err(|e| {
                    CodecError::DeserializeError(format!(
                        "Failed to decode validation reject code: {:?}",
                        &e
                    ))
                })?,
            ),
            RejectCodeTypePrefix::ConnectivityIssues => RejectCode::ConnectivityIssues,
            RejectCodeTypePrefix::RejectedInPriorRound => RejectCode::RejectedInPriorRound,
            RejectCodeTypePrefix::NoSortitionView => RejectCode::NoSortitionView,
            RejectCodeTypePrefix::SortitionViewMismatch => RejectCode::SortitionViewMismatch,
        };
        Ok(code)
    }
}

#[cfg_attr(test, mutants::skip)]
impl std::fmt::Display for RejectCode {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        match self {
            RejectCode::ValidationFailed(code) => write!(f, "Validation failed: {:?}", code),
            RejectCode::ConnectivityIssues => write!(
                f,
                "The block was rejected due to connectivity issues with the signer."
            ),
            RejectCode::RejectedInPriorRound => write!(
                f,
                "The block was proposed before and rejected by the signer."
            ),
            RejectCode::NoSortitionView => {
                write!(f, "The block was rejected due to no sortition view.")
            }
            RejectCode::SortitionViewMismatch => {
                write!(
                    f,
                    "The block was rejected due to a mismatch with expected sortition view."
                )
            }
        }
    }
}

impl From<BlockResponse> for SignerMessage {
    fn from(block_response: BlockResponse) -> Self {
        Self::BlockResponse(block_response)
    }
}

impl From<BlockValidateReject> for BlockResponse {
    fn from(rejection: BlockValidateReject) -> Self {
        Self::Rejected(rejection.into())
    }
}

#[cfg(test)]
mod test {
    use blockstack_lib::chainstate::nakamoto::NakamotoBlockHeader;
    use blockstack_lib::chainstate::stacks::{
        ThresholdSignature, TransactionAnchorMode, TransactionAuth, TransactionPayload,
        TransactionPostConditionMode, TransactionSmartContract, TransactionVersion,
    };
    use blockstack_lib::util_lib::strings::StacksString;
    use clarity::consts::CHAIN_ID_MAINNET;
    use clarity::types::chainstate::{ConsensusHash, StacksBlockId, TrieHash};
    use clarity::types::PrivateKey;
    use clarity::util::hash::MerkleTree;
    use clarity::util::secp256k1::MessageSignature;
    use rand::{thread_rng, Rng, RngCore};
    use rand_core::OsRng;
    use stacks_common::bitvec::BitVec;
    use stacks_common::consts::CHAIN_ID_TESTNET;
    use stacks_common::types::chainstate::StacksPrivateKey;

    use super::{StacksMessageCodecExtensions, *};

    #[test]
    fn signer_slots_count_is_sane() {
        let slot_identifiers_len = MessageSlotID::ALL.len();
        assert!(
            SIGNER_SLOTS_PER_USER as usize >= slot_identifiers_len,
            "stacks_common::SIGNER_SLOTS_PER_USER ({}) must be >= slot identifiers ({})",
            SIGNER_SLOTS_PER_USER,
            slot_identifiers_len,
        );
    }

    #[test]
    fn serde_reject_code() {
        let code = RejectCode::ValidationFailed(ValidateRejectCode::InvalidBlock);
        let serialized_code = code.serialize_to_vec();
        let deserialized_code = read_next::<RejectCode, _>(&mut &serialized_code[..])
            .expect("Failed to deserialize RejectCode");
        assert_eq!(code, deserialized_code);

        let code = RejectCode::ConnectivityIssues;
        let serialized_code = code.serialize_to_vec();
        let deserialized_code = read_next::<RejectCode, _>(&mut &serialized_code[..])
            .expect("Failed to deserialize RejectCode");
        assert_eq!(code, deserialized_code);
    }

    #[test]
    fn serde_block_rejection() {
        let rejection = BlockRejection::new(
            Sha512Trunc256Sum([0u8; 32]),
            RejectCode::ValidationFailed(ValidateRejectCode::InvalidBlock),
        );
        let serialized_rejection = rejection.serialize_to_vec();
        let deserialized_rejection = read_next::<BlockRejection, _>(&mut &serialized_rejection[..])
            .expect("Failed to deserialize BlockRejection");
        assert_eq!(rejection, deserialized_rejection);

        let rejection =
            BlockRejection::new(Sha512Trunc256Sum([1u8; 32]), RejectCode::ConnectivityIssues);
        let serialized_rejection = rejection.serialize_to_vec();
        let deserialized_rejection = read_next::<BlockRejection, _>(&mut &serialized_rejection[..])
            .expect("Failed to deserialize BlockRejection");
        assert_eq!(rejection, deserialized_rejection);
    }

    #[test]
    fn serde_block_response() {
        let response =
            BlockResponse::Accepted((Sha512Trunc256Sum([0u8; 32]), MessageSignature::empty()));
        let serialized_response = response.serialize_to_vec();
        let deserialized_response = read_next::<BlockResponse, _>(&mut &serialized_response[..])
            .expect("Failed to deserialize BlockResponse");
        assert_eq!(response, deserialized_response);

        let response = BlockResponse::Rejected(BlockRejection::new(
            Sha512Trunc256Sum([1u8; 32]),
            RejectCode::ValidationFailed(ValidateRejectCode::InvalidBlock),
        ));
        let serialized_response = response.serialize_to_vec();
        let deserialized_response = read_next::<BlockResponse, _>(&mut &serialized_response[..])
            .expect("Failed to deserialize BlockResponse");
        assert_eq!(response, deserialized_response);
    }

    #[test]
    fn serde_signer_message() {
        let signer_message = SignerMessage::BlockResponse(BlockResponse::Accepted((
            Sha512Trunc256Sum([2u8; 32]),
            MessageSignature::empty(),
        )));
        let serialized_signer_message = signer_message.serialize_to_vec();
        let deserialized_signer_message =
            read_next::<SignerMessage, _>(&mut &serialized_signer_message[..])
                .expect("Failed to deserialize SignerMessage");
        assert_eq!(signer_message, deserialized_signer_message);

        let header = NakamotoBlockHeader::empty();
        let mut block = NakamotoBlock {
            header,
            txs: vec![],
        };
        let tx_merkle_root = {
            let txid_vecs = block
                .txs
                .iter()
                .map(|tx| tx.txid().as_bytes().to_vec())
                .collect();

            MerkleTree::<Sha512Trunc256Sum>::new(&txid_vecs).root()
        };
        block.header.tx_merkle_root = tx_merkle_root;

        let block_proposal = BlockProposal {
            block,
            burn_height: thread_rng().next_u64(),
            reward_cycle: thread_rng().next_u64(),
        };
        let signer_message = SignerMessage::BlockProposal(block_proposal);
        let serialized_signer_message = signer_message.serialize_to_vec();
        let deserialized_signer_message =
            read_next::<SignerMessage, _>(&mut &serialized_signer_message[..])
                .expect("Failed to deserialize SignerMessage");
        assert_eq!(signer_message, deserialized_signer_message);
    }

    fn random_peer_data() -> PeerInfo {
        let burn_block_height = thread_rng().next_u64();
        let stacks_tip_consensus_byte: u8 = thread_rng().gen();
        let stacks_tip_byte: u8 = thread_rng().gen();
        let stacks_tip_height = thread_rng().next_u64();
        let server_version = "0.0.0".to_string();
        let pox_consensus_byte: u8 = thread_rng().gen();
        PeerInfo {
            burn_block_height,
            stacks_tip_consensus_hash: ConsensusHash([stacks_tip_consensus_byte; 20]),
            stacks_tip: BlockHeaderHash([stacks_tip_byte; 32]),
            stacks_tip_height,
            server_version,
            pox_consensus: ConsensusHash([pox_consensus_byte; 20]),
        }
    }
    fn random_mock_sign_data() -> MockSignData {
        let chain_byte: u8 = thread_rng().gen_range(0..=1);
        let chain_id = if chain_byte == 1 {
            CHAIN_ID_TESTNET
        } else {
            CHAIN_ID_MAINNET
        };
        let peer_info = random_peer_data();
        MockSignData {
            peer_info,
            event_burn_block_height: thread_rng().next_u64(),
            chain_id,
        }
    }

    #[test]
    fn verify_sign_mock_signature() {
        let private_key = StacksPrivateKey::new();
        let public_key = StacksPublicKey::from_private(&private_key);

        let bad_private_key = StacksPrivateKey::new();
        let bad_public_key = StacksPublicKey::from_private(&bad_private_key);

        let mut mock_signature = MockSignature {
            signature: MessageSignature::empty(),
            sign_data: random_mock_sign_data(),
        };
        assert!(!mock_signature
            .verify(&public_key)
            .expect("Failed to verify MockSignature"));

        mock_signature
            .sign(&private_key)
            .expect("Failed to sign MockSignature");

        assert!(mock_signature
            .verify(&public_key)
            .expect("Failed to verify MockSignature"));
        assert!(!mock_signature
            .verify(&bad_public_key)
            .expect("Failed to verify MockSignature"));
    }

    #[test]
    fn serde_peer_data() {
        let peer_data = random_peer_data();
        let serialized_data = peer_data.serialize_to_vec();
        let deserialized_data = read_next::<PeerInfo, _>(&mut &serialized_data[..])
            .expect("Failed to deserialize PeerInfo");
        assert_eq!(peer_data, deserialized_data);
    }

    #[test]
    fn serde_mock_signature() {
        let mock_signature = MockSignature {
            signature: MessageSignature::empty(),
            sign_data: random_mock_sign_data(),
        };
        let serialized_signature = mock_signature.serialize_to_vec();
        let deserialized_signature = read_next::<MockSignature, _>(&mut &serialized_signature[..])
            .expect("Failed to deserialize MockSignature");
        assert_eq!(mock_signature, deserialized_signature);
    }

    #[test]
    fn serde_sign_data() {
        let sign_data = random_mock_sign_data();
        let serialized_data = sign_data.serialize_to_vec();
        let deserialized_data = read_next::<MockSignData, _>(&mut &serialized_data[..])
            .expect("Failed to deserialize MockSignData");
        assert_eq!(sign_data, deserialized_data);
    }

    #[test]
    fn serde_mock_miner_message() {
        let mock_signature_1 = MockSignature {
            signature: MessageSignature::empty(),
            sign_data: random_mock_sign_data(),
        };
        let mock_signature_2 = MockSignature {
            signature: MessageSignature::empty(),
            sign_data: random_mock_sign_data(),
        };
        let mock_miner_message = MockMinerMessage {
            peer_info: random_peer_data(),
            tenure_burn_block_height: thread_rng().next_u64(),
            chain_id: thread_rng().gen_range(0..=1),
            mock_signatures: vec![mock_signature_1, mock_signature_2],
        };
        let serialized_data = mock_miner_message.serialize_to_vec();
        let deserialized_data = read_next::<MockMinerMessage, _>(&mut &serialized_data[..])
            .expect("Failed to deserialize MockSignData");
        assert_eq!(mock_miner_message, deserialized_data);
    }
}
