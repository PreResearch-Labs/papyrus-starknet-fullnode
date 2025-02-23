use starknet_api::block::BlockHash;
use starknet_api::core::ContractAddress;
use starknet_api::transaction::Transaction;

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct Proposal {
    pub height: u64,
    pub proposer: ContractAddress,
    pub transactions: Vec<Transaction>,
    pub block_hash: BlockHash,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum ConsensusMessage {
    Proposal(Proposal),
}
