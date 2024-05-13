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
//
use blockstack_lib::net::api::poststackerdbchunk::StackerDBErrorCodes;
use hashbrown::HashMap;
use libsigner::v0::messages::{MessageSlotID, SignerMessage};
use libsigner::{SignerSession, StackerDBSession};
use libstackerdb::{StackerDBChunkAckData, StackerDBChunkData};
use slog::{slog_debug, slog_warn};
use stacks_common::codec::StacksMessageCodec;
use stacks_common::types::chainstate::StacksPrivateKey;
use stacks_common::{debug, warn};

use crate::client::{retry_with_exponential_backoff, ClientError, SignerSlotID};
use crate::config::SignerConfig;

/// The StackerDB client for communicating with the .signers contract
#[derive(Debug)]
pub struct StackerDB {
    /// The stacker-db sessions for each signer set and message type.
    /// Maps message ID to the DB session.
    signers_message_stackerdb_sessions: HashMap<MessageSlotID, StackerDBSession>,
    /// The private key used in all stacks node communications
    stacks_private_key: StacksPrivateKey,
    /// A map of a message ID to last chunk version for each session
    slot_versions: HashMap<MessageSlotID, HashMap<SignerSlotID, u32>>,
    /// The signer slot ID -- the index into the signer list for this signer daemon's signing key.
    signer_slot_id: SignerSlotID,
    /// The reward cycle of the connecting signer
    reward_cycle: u64,
}

impl From<&SignerConfig> for StackerDB {
    fn from(config: &SignerConfig) -> Self {
        Self::new(
            &config.node_host,
            config.stacks_private_key,
            config.mainnet,
            config.reward_cycle,
            config.signer_slot_id,
        )
    }
}
impl StackerDB {
    /// Create a new StackerDB client
    pub fn new(
        host: &str,
        stacks_private_key: StacksPrivateKey,
        is_mainnet: bool,
        reward_cycle: u64,
        signer_slot_id: SignerSlotID,
    ) -> Self {
        let mut signers_message_stackerdb_sessions = HashMap::new();
        for msg_id in MessageSlotID::ALL {
            signers_message_stackerdb_sessions.insert(
                *msg_id,
                StackerDBSession::new(host, msg_id.stacker_db_contract(is_mainnet, reward_cycle)),
            );
        }

        Self {
            signers_message_stackerdb_sessions,
            stacks_private_key,
            slot_versions: HashMap::new(),
            signer_slot_id,
            reward_cycle,
        }
    }

    /// Sends messages to the .signers stacker-db with an exponential backoff retry
    pub fn send_message_with_retry(
        &mut self,
        message: SignerMessage,
    ) -> Result<StackerDBChunkAckData, ClientError> {
        let msg_id = message.msg_id();
        let message_bytes = message.serialize_to_vec();
        self.send_message_bytes_with_retry(&msg_id, message_bytes)
    }

    /// Sends message (as a raw msg ID and bytes) to the .signers stacker-db with an
    /// exponential backoff retry
    pub fn send_message_bytes_with_retry(
        &mut self,
        msg_id: &MessageSlotID,
        message_bytes: Vec<u8>,
    ) -> Result<StackerDBChunkAckData, ClientError> {
        let slot_id = self.signer_slot_id;
        loop {
            let mut slot_version = if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                if let Some(version) = versions.get(&slot_id) {
                    *version
                } else {
                    versions.insert(slot_id, 0);
                    1
                }
            } else {
                let mut versions = HashMap::new();
                versions.insert(slot_id, 0);
                self.slot_versions.insert(*msg_id, versions);
                1
            };

            let mut chunk = StackerDBChunkData::new(slot_id.0, slot_version, message_bytes.clone());
            chunk.sign(&self.stacks_private_key)?;

            let Some(session) = self.signers_message_stackerdb_sessions.get_mut(msg_id) else {
                panic!("FATAL: would loop forever trying to send a message with ID {}, for which we don't have a session", msg_id);
            };

            debug!(
                "Sending a chunk to stackerdb slot ID {slot_id} with version {slot_version} and message ID {msg_id} to contract {:?}!\n{chunk:?}",
                &session.stackerdb_contract_id
            );

            let send_request = || session.put_chunk(&chunk).map_err(backoff::Error::transient);
            let chunk_ack: StackerDBChunkAckData = retry_with_exponential_backoff(send_request)?;

            if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                // NOTE: per the above, this is always executed
                versions.insert(slot_id, slot_version.saturating_add(1));
            } else {
                return Err(ClientError::NotConnected);
            }

            if chunk_ack.accepted {
                debug!("Chunk accepted by stackerdb: {chunk_ack:?}");
                return Ok(chunk_ack);
            } else {
                warn!("Chunk rejected by stackerdb: {chunk_ack:?}");
            }
            if let Some(code) = chunk_ack.code {
                match StackerDBErrorCodes::from_code(code) {
                    Some(StackerDBErrorCodes::DataAlreadyExists) => {
                        if let Some(slot_metadata) = chunk_ack.metadata {
                            warn!("Failed to send message to stackerdb due to wrong version number. Attempted {}. Expected {}. Retrying...", slot_version, slot_metadata.slot_version);
                            slot_version = slot_metadata.slot_version;
                        } else {
                            warn!("Failed to send message to stackerdb due to wrong version number. Attempted {}. Expected unknown version number. Incrementing and retrying...", slot_version);
                        }
                        if let Some(versions) = self.slot_versions.get_mut(msg_id) {
                            // NOTE: per the above, this is always executed
                            versions.insert(slot_id, slot_version.saturating_add(1));
                        } else {
                            return Err(ClientError::NotConnected);
                        }
                    }
                    _ => {
                        warn!("Failed to send message to stackerdb: {:?}", chunk_ack);
                        return Err(ClientError::PutChunkRejected(
                            chunk_ack
                                .reason
                                .unwrap_or_else(|| "No reason given".to_string()),
                        ));
                    }
                }
            }
        }
    }

    /// Retrieve the signer set this stackerdb client is attached to
    pub fn get_signer_set(&self) -> u32 {
        u32::try_from(self.reward_cycle % 2).expect("FATAL: reward cycle % 2 exceeds u32::MAX")
    }

    /// Retrieve the signer slot ID
    pub fn get_signer_slot_id(&mut self) -> SignerSlotID {
        self.signer_slot_id
    }
}

#[cfg(test)]
mod tests {
    use std::thread::spawn;
    use std::time::Duration;

    use blockstack_lib::chainstate::nakamoto::{NakamotoBlock, NakamotoBlockHeader};
    use blockstack_lib::chainstate::stacks::ThresholdSignature;
    use clarity::types::chainstate::{ConsensusHash, StacksBlockId, TrieHash};
    use clarity::util::hash::{MerkleTree, Sha512Trunc256Sum};
    use clarity::util::secp256k1::MessageSignature;
    use libsigner::BlockProposal;
    use rand::{thread_rng, RngCore};
    use stacks_common::bitvec::BitVec;

    use super::*;
    use crate::client::tests::{generate_signer_config, mock_server_from_config, write_response};
    use crate::config::{build_signer_config_tomls, GlobalConfig, Network};

    #[test]
    fn send_signer_message_should_succeed() {
        let signer_config = build_signer_config_tomls(
            &[StacksPrivateKey::new()],
            "localhost:20443",
            Some(Duration::from_millis(128)), // Timeout defaults to 5 seconds. Let's override it to 128 milliseconds.
            &Network::Testnet,
            "1234",
            16,
            3000,
            Some(100_000),
            None,
            Some(9000),
        );
        let config = GlobalConfig::load_from_str(&signer_config[0]).unwrap();
        let signer_config = generate_signer_config(&config, 5, 20);
        let mut stackerdb = StackerDB::from(&signer_config);

        let header = NakamotoBlockHeader {
            version: 1,
            chain_length: 2,
            burn_spent: 3,
            consensus_hash: ConsensusHash([0x04; 20]),
            parent_block_id: StacksBlockId([0x05; 32]),
            tx_merkle_root: Sha512Trunc256Sum([0x06; 32]),
            state_index_root: TrieHash([0x07; 32]),
            miner_signature: MessageSignature::empty(),
            signer_signature: ThresholdSignature::empty(),
            signer_bitvec: BitVec::zeros(1).unwrap(),
        };
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
        let ack = StackerDBChunkAckData {
            accepted: true,
            reason: None,
            metadata: None,
            code: None,
        };
        let mock_server = mock_server_from_config(&config);
        let h = spawn(move || stackerdb.send_message_with_retry(signer_message));
        let mut response_bytes = b"HTTP/1.1 200 OK\n\n".to_vec();
        let payload = serde_json::to_string(&ack).expect("Failed to serialize ack");
        response_bytes.extend(payload.as_bytes());
        std::thread::sleep(Duration::from_millis(500));
        write_response(mock_server, response_bytes.as_slice());
        assert_eq!(ack, h.join().unwrap().unwrap());
    }
}
