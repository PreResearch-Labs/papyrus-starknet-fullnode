/// This crate is responsible for sending messages to a given peer and responding to them according
/// to the [`Starknet p2p specs`]
///
/// [`Starknet p2p specs`]: https://github.com/starknet-io/starknet-p2p-specs/
pub mod bin_utils;
pub mod db_executor;
mod discovery;
pub mod gossipsub_impl;
pub mod mixed_behaviour;
pub mod network_manager;
mod peer_manager;
pub mod sqmr;
#[cfg(test)]
mod test_utils;
mod utils;

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use derive_more::Display;
use enum_iterator::Sequence;
use lazy_static::lazy_static;
use libp2p::{Multiaddr, StreamProtocol};
use papyrus_config::converters::{
    deserialize_optional_vec_u8,
    deserialize_seconds_to_duration,
    serialize_optional_vec_u8,
};
use papyrus_config::dumping::{ser_optional_param, ser_param, SerializeConfig};
use papyrus_config::validators::validate_vec_u256;
use papyrus_config::{ParamPath, ParamPrivacyInput, SerializedParam};
use serde::{Deserialize, Serialize};
use validator::Validate;

pub use crate::network_manager::SqmrSubscriberChannels;

// TODO: add peer manager config to the network config
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Validate)]
pub struct NetworkConfig {
    pub tcp_port: u16,
    pub quic_port: u16,
    #[serde(deserialize_with = "deserialize_seconds_to_duration")]
    pub session_timeout: Duration,
    #[serde(deserialize_with = "deserialize_seconds_to_duration")]
    pub idle_connection_timeout: Duration,
    pub header_buffer_size: usize,
    pub bootstrap_peer_multiaddr: Option<Multiaddr>,
    #[validate(custom = "validate_vec_u256")]
    #[serde(deserialize_with = "deserialize_optional_vec_u8")]
    pub(crate) secret_key: Option<Vec<u8>>,
}

/// This is a part of the exposed API of the network manager.
/// This is meant to represent the different underlying p2p protocols the network manager supports.
// TODO(shahak): Change protocol to a wrapper of string.
#[derive(Debug, Display, PartialEq, Eq, Clone, Copy, Hash, Sequence)]
pub enum Protocol {
    SignedBlockHeader,
    StateDiff,
    Transaction,
}

impl Protocol {
    pub fn as_str(&self) -> &'static str {
        match self {
            Protocol::SignedBlockHeader => "/starknet/headers/1",
            Protocol::StateDiff => "/starknet/state_diffs/1",
            Protocol::Transaction => "/starknet/transactions/1",
        }
    }
}

impl From<Protocol> for StreamProtocol {
    fn from(protocol: Protocol) -> StreamProtocol {
        StreamProtocol::new(protocol.as_str())
    }
}

#[derive(thiserror::Error, Debug)]
#[error("Unknown protocol: {0}")]
pub struct UnknownProtocolConversionError(String);

lazy_static! {
    static ref PROTOCOL_NAME_TO_PROTOCOL: HashMap<&'static str, Protocol> =
        enum_iterator::all::<Protocol>().map(|protocol| (protocol.as_str(), protocol)).collect();
}

impl TryFrom<StreamProtocol> for Protocol {
    type Error = UnknownProtocolConversionError;

    fn try_from(protocol: StreamProtocol) -> Result<Self, Self::Error> {
        PROTOCOL_NAME_TO_PROTOCOL
            .get(protocol.as_ref())
            .ok_or(UnknownProtocolConversionError(protocol.as_ref().to_string()))
            .copied()
    }
}

impl SerializeConfig for NetworkConfig {
    fn dump(&self) -> BTreeMap<ParamPath, SerializedParam> {
        let mut config = BTreeMap::from_iter([
            ser_param(
                "tcp_port",
                &self.tcp_port,
                "The port that the node listens on for incoming tcp connections.",
                ParamPrivacyInput::Public,
            ),
            ser_param(
                "quic_port",
                &self.quic_port,
                "The port that the node listens on for incoming quic connections.",
                ParamPrivacyInput::Public,
            ),
            ser_param(
                "session_timeout",
                &self.session_timeout.as_secs(),
                "Maximal time in seconds that each session can take before failing on timeout.",
                ParamPrivacyInput::Public,
            ),
            ser_param(
                "idle_connection_timeout",
                &self.idle_connection_timeout.as_secs(),
                "Amount of time in seconds that a connection with no active sessions will stay \
                 alive.",
                ParamPrivacyInput::Public,
            ),
            ser_param(
                "header_buffer_size",
                &self.header_buffer_size,
                "Size of the buffer for headers read from the storage.",
                ParamPrivacyInput::Public,
            ),
        ]);
        config.extend(ser_optional_param(
            &self.bootstrap_peer_multiaddr,
            Multiaddr::empty(),
            "bootstrap_peer_multiaddr",
            "The multiaddress of the peer node. It should include the peer's id. For more info: https://docs.libp2p.io/concepts/fundamentals/peers/",
            ParamPrivacyInput::Public,
        ));
        config.extend([ser_param(
            "secret_key",
            &serialize_optional_vec_u8(&self.secret_key),
            "The secret key used for building the peer id. If it's an empty string a random one \
             will be used.",
            ParamPrivacyInput::Private,
        )]);
        config
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            tcp_port: 10000,
            quic_port: 10001,
            session_timeout: Duration::from_secs(120),
            idle_connection_timeout: Duration::from_secs(120),
            header_buffer_size: 100000,
            bootstrap_peer_multiaddr: None,
            secret_key: None,
        }
    }
}
