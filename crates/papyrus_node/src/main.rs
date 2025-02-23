#[cfg(test)]
mod main_test;

use std::env::{self, args};
use std::future::{pending, Future};
use std::process::exit;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::FutureExt;
use papyrus_base_layer::ethereum_base_layer_contract::EthereumBaseLayerConfig;
use papyrus_common::metrics::COLLECT_PROFILING_METRICS;
use papyrus_common::pending_classes::PendingClasses;
use papyrus_common::BlockHashAndNumber;
use papyrus_config::presentation::get_config_presentation;
use papyrus_config::validators::config_validate;
use papyrus_config::ConfigError;
use papyrus_consensus::papyrus_consensus_context::PapyrusConsensusContext;
use papyrus_consensus::types::ConsensusError;
use papyrus_monitoring_gateway::MonitoringServer;
use papyrus_network::db_executor::DBExecutor;
use papyrus_network::gossipsub_impl::Topic;
use papyrus_network::network_manager::{
    BroadcastSubscriberChannels,
    NetworkError,
    SqmrQueryReceiver,
    SqmrSubscriberChannels,
};
use papyrus_network::{network_manager, NetworkConfig, Protocol};
use papyrus_node::config::NodeConfig;
use papyrus_node::version::VERSION_FULL;
use papyrus_p2p_sync::{P2PSync, P2PSyncConfig, P2PSyncError};
use papyrus_protobuf::consensus::ConsensusMessage;
use papyrus_protobuf::sync::{
    DataOrFin,
    HeaderQuery,
    SignedBlockHeader,
    StateDiffChunk,
    StateDiffQuery,
    TransactionQuery,
};
#[cfg(feature = "rpc")]
use papyrus_rpc::run_server;
use papyrus_storage::{open_storage, update_storage_metrics, StorageReader, StorageWriter};
use papyrus_sync::sources::base_layer::{BaseLayerSourceError, EthereumBaseLayerSource};
use papyrus_sync::sources::central::{CentralError, CentralSource, CentralSourceConfig};
use papyrus_sync::sources::pending::PendingSource;
use papyrus_sync::{StateSync, StateSyncError, SyncConfig};
use starknet_api::block::{BlockHash, BlockNumber};
use starknet_api::felt;
use starknet_api::state::ThinStateDiff;
use starknet_api::transaction::{Transaction, TransactionOutput};
use starknet_client::reader::objects::pending_data::{PendingBlock, PendingBlockOrDeprecated};
use starknet_client::reader::PendingData;
use tokio::sync::RwLock;
use tokio::task::{JoinError, JoinHandle};
use tracing::metadata::LevelFilter;
use tracing::{debug_span, error, info, warn, Instrument};
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};

// TODO(yair): Add to config.
const DEFAULT_LEVEL: LevelFilter = LevelFilter::INFO;

// TODO(shahak): Consider adding genesis hash to the config to support chains that have
// different genesis hash.
// TODO: Consider moving to a more general place.
const GENESIS_HASH: &str = "0x0";

// TODO(dvir): add this to config.
// Duration between updates to the storage metrics (those in the collect_storage_metrics function).
const STORAGE_METRICS_UPDATE_INTERVAL: Duration = Duration::from_secs(10);

#[cfg(feature = "rpc")]
async fn create_rpc_server_future(
    config: &NodeConfig,
    shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
    pending_data: Arc<RwLock<PendingData>>,
    pending_classes: Arc<RwLock<PendingClasses>>,
    storage_reader: StorageReader,
) -> anyhow::Result<impl Future<Output = Result<(), JoinError>>> {
    let (_, server_handle) = run_server(
        &config.rpc,
        shared_highest_block,
        pending_data,
        pending_classes,
        storage_reader,
        VERSION_FULL,
    )
    .await?;
    Ok(tokio::spawn(server_handle.stopped()))
}

#[cfg(not(feature = "rpc"))]
async fn create_rpc_server_future(
    _config: &NodeConfig,
    _shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
    _pending_data: Arc<RwLock<PendingData>>,
    _pending_classes: Arc<RwLock<PendingClasses>>,
    _storage_reader: StorageReader,
) -> anyhow::Result<impl Future<Output = Result<(), JoinError>>> {
    Ok(pending())
}

fn run_consensus(
    storage_reader: StorageReader,
    consensus_channels: BroadcastSubscriberChannels<ConsensusMessage>,
) -> anyhow::Result<JoinHandle<Result<(), ConsensusError>>> {
    let Ok(validator_id) = env::var("CONSENSUS_VALIDATOR_ID") else {
        info!("CONSENSUS_VALIDATOR_ID is not set. Not run consensus.");
        return Ok(tokio::spawn(pending()));
    };
    info!("Running consensus as validator {validator_id}");
    let validator_id = validator_id.parse::<u128>()?.into();
    let context = PapyrusConsensusContext::new(
        storage_reader.clone(),
        consensus_channels.messages_to_broadcast_sender,
    );
    // TODO(dvir): add option to configure this value.
    let start_height = BlockNumber(0);

    Ok(tokio::spawn(papyrus_consensus::run_consensus(
        Arc::new(context),
        start_height,
        validator_id,
        consensus_channels.broadcasted_messages_receiver,
    )))
}

async fn run_threads(config: NodeConfig) -> anyhow::Result<()> {
    let (storage_reader, storage_writer) = open_storage(config.storage.clone())?;

    let storage_metrics_handle = if config.monitoring_gateway.collect_metrics {
        spawn_storage_metrics_collector(storage_reader.clone(), STORAGE_METRICS_UPDATE_INTERVAL)
    } else {
        tokio::spawn(pending())
    };

    // P2P network.
    let (
        network_future,
        maybe_sync_client_channels,
        maybe_sync_server_channels,
        maybe_consensus_channels,
        local_peer_id,
    ) = run_network(config.network.clone())?;
    let network_handle = tokio::spawn(network_future);

    // Monitoring server.
    let monitoring_server = MonitoringServer::new(
        config.monitoring_gateway.clone(),
        get_config_presentation(&config, true)?,
        get_config_presentation(&config, false)?,
        storage_reader.clone(),
        VERSION_FULL,
        local_peer_id,
    )?;
    let monitoring_server_handle = monitoring_server.spawn_server().await;

    // The sync is the only writer of the syncing state.
    let shared_highest_block = Arc::new(RwLock::new(None));
    let pending_data = Arc::new(RwLock::new(PendingData {
        // The pending data might change later to DeprecatedPendingBlock, depending on the response
        // from the feeder gateway.
        block: PendingBlockOrDeprecated::Current(PendingBlock {
            parent_block_hash: BlockHash(felt!(GENESIS_HASH)),
            ..Default::default()
        }),
        ..Default::default()
    }));
    let pending_classes = Arc::new(RwLock::new(PendingClasses::default()));

    // JSON-RPC server.
    let server_handle_future = create_rpc_server_future(
        &config,
        shared_highest_block.clone(),
        pending_data.clone(),
        pending_classes.clone(),
        storage_reader.clone(),
    )
    .await?;

    // P2P Sync Server task.
    let p2p_sync_server_future = match maybe_sync_server_channels {
        Some((
            header_sync_server_channel,
            state_diff_sync_server_channel,
            transaction_server_channel,
        )) => {
            let db_executor = DBExecutor::new(
                storage_reader.clone(),
                header_sync_server_channel,
                state_diff_sync_server_channel,
                transaction_server_channel,
            );
            db_executor.run().boxed()
        }
        None => pending().boxed(),
    };
    let p2p_sync_server_handle = tokio::spawn(p2p_sync_server_future);

    // Sync task.
    let (sync_future, p2p_sync_client_future) = match (config.sync, config.p2p_sync) {
        (Some(_), Some(_)) => {
            panic!("One of --sync.#is_none or --p2p_sync.#is_none must be turned on");
        }
        (Some(sync_config), None) => {
            let configs = (sync_config, config.central, config.base_layer);
            let storage = (storage_reader.clone(), storage_writer);
            let sync_fut =
                run_sync(configs, shared_highest_block, pending_data, pending_classes, storage);
            (sync_fut.boxed(), pending().boxed())
        }
        (None, Some(p2p_sync_config)) => {
            let (header_channels, state_diff_channels) = maybe_sync_client_channels
                .expect("If p2p sync is enabled, network needs to be enabled too");
            (
                pending().boxed(),
                run_p2p_sync_client(
                    p2p_sync_config,
                    storage_reader.clone(),
                    storage_writer,
                    header_channels,
                    state_diff_channels,
                )
                .boxed(),
            )
        }
        (None, None) => (pending().boxed(), pending().boxed()),
    };
    let sync_handle = tokio::spawn(sync_future);
    let p2p_sync_client_handle = tokio::spawn(p2p_sync_client_future);

    let consensus_handle = if let Some(consensus_channels) = maybe_consensus_channels {
        run_consensus(storage_reader.clone(), consensus_channels)?
    } else {
        tokio::spawn(pending())
    };

    tokio::select! {
        res = storage_metrics_handle => {
            error!("collecting storage metrics stopped.");
            res?
        }
        res = server_handle_future => {
            error!("RPC server stopped.");
            res?
        }
        res = monitoring_server_handle => {
            error!("Monitoring server stopped.");
            res??
        }
        res = sync_handle => {
            error!("Sync stopped.");
            res??
        }
        res = p2p_sync_client_handle => {
            error!("P2P Sync stopped.");
            res??
        }
        res = p2p_sync_server_handle => {
            error!("P2P Sync server stopped");
            res?
        }
        res = network_handle => {
            error!("Network stopped.");
            res??
        }
        res = consensus_handle => {
            error!("Consensus stopped.");
            res??
        }
    };
    error!("Task ended with unexpected Ok.");
    return Ok(());

    async fn run_sync(
        configs: (SyncConfig, CentralSourceConfig, EthereumBaseLayerConfig),
        shared_highest_block: Arc<RwLock<Option<BlockHashAndNumber>>>,
        pending_data: Arc<RwLock<PendingData>>,
        pending_classes: Arc<RwLock<PendingClasses>>,
        storage: (StorageReader, StorageWriter),
    ) -> Result<(), StateSyncError> {
        let (sync_config, central_config, base_layer_config) = configs;
        let (storage_reader, storage_writer) = storage;
        let central_source =
            CentralSource::new(central_config.clone(), VERSION_FULL, storage_reader.clone())
                .map_err(CentralError::ClientCreation)?;
        let pending_source = PendingSource::new(central_config, VERSION_FULL)
            .map_err(CentralError::ClientCreation)?;
        let base_layer_source = EthereumBaseLayerSource::new(base_layer_config)
            .map_err(|e| BaseLayerSourceError::BaseLayerSourceCreationError(e.to_string()))?;
        let mut sync = StateSync::new(
            sync_config,
            shared_highest_block,
            pending_data,
            pending_classes,
            central_source,
            pending_source,
            base_layer_source,
            storage_reader.clone(),
            storage_writer,
        );
        sync.run().await
    }

    async fn run_p2p_sync_client(
        p2p_sync_config: P2PSyncConfig,
        storage_reader: StorageReader,
        storage_writer: StorageWriter,
        header_channels: SqmrSubscriberChannels<HeaderQuery, DataOrFin<SignedBlockHeader>>,
        state_diff_channels: SqmrSubscriberChannels<StateDiffQuery, DataOrFin<ThinStateDiff>>,
    ) -> Result<(), P2PSyncError> {
        let sync = P2PSync::new(
            p2p_sync_config,
            storage_reader,
            storage_writer,
            header_channels.query_sender,
            header_channels.response_receiver,
            state_diff_channels.query_sender,
            state_diff_channels.response_receiver,
        );
        sync.run().await
    }
}

type NetworkRunReturn = (
    BoxFuture<'static, Result<(), NetworkError>>,
    Option<(
        SqmrSubscriberChannels<HeaderQuery, DataOrFin<SignedBlockHeader>>,
        SqmrSubscriberChannels<StateDiffQuery, DataOrFin<ThinStateDiff>>,
    )>,
    Option<(
        SqmrQueryReceiver<HeaderQuery, DataOrFin<SignedBlockHeader>>,
        SqmrQueryReceiver<StateDiffQuery, DataOrFin<StateDiffChunk>>,
        SqmrQueryReceiver<TransactionQuery, DataOrFin<(Transaction, TransactionOutput)>>,
    )>,
    Option<BroadcastSubscriberChannels<ConsensusMessage>>,
    String,
);

fn run_network(config: Option<NetworkConfig>) -> anyhow::Result<NetworkRunReturn> {
    let Some(network_config) = config else {
        return Ok((pending().boxed(), None, None, None, "".to_string()));
    };
    let mut network_manager = network_manager::NetworkManager::new(network_config.clone());
    let local_peer_id = network_manager.get_local_peer_id();
    let header_client_channels =
        network_manager.register_sqmr_subscriber(Protocol::SignedBlockHeader);
    let state_diff_client_channels = network_manager.register_sqmr_subscriber(Protocol::StateDiff);

    let header_server_channel =
        network_manager.register_sqmr_protocol_server(Protocol::SignedBlockHeader);
    let state_diff_server_channel =
        network_manager.register_sqmr_protocol_server(Protocol::StateDiff);
    let transaction_server_channel =
        network_manager.register_sqmr_protocol_server(Protocol::Transaction);

    let consensus_channels =
        network_manager.register_broadcast_subscriber(Topic::new("consensus"), 100)?;

    Ok((
        network_manager.run().boxed(),
        Some((header_client_channels, state_diff_client_channels)),
        Some((header_server_channel, state_diff_server_channel, transaction_server_channel)),
        Some(consensus_channels),
        local_peer_id,
    ))
}

// TODO(yair): add dynamic level filtering.
// TODO(dan): filter out logs from dependencies (happens when RUST_LOG=DEBUG)
// TODO(yair): define and implement configurable filtering.
fn configure_tracing() {
    let fmt_layer = fmt::layer().compact().with_target(false);
    let level_filter_layer =
        EnvFilter::builder().with_default_directive(DEFAULT_LEVEL.into()).from_env_lossy();

    // This sets a single subscriber to all of the threads. We may want to implement different
    // subscriber for some threads and use set_global_default instead of init.
    tracing_subscriber::registry().with(fmt_layer).with(level_filter_layer).init();
}

fn spawn_storage_metrics_collector(
    storage_reader: StorageReader,
    update_interval: Duration,
) -> JoinHandle<()> {
    tokio::spawn(
        async move {
            loop {
                if let Err(error) = update_storage_metrics(&storage_reader) {
                    warn!("Failed to update storage metrics: {error}");
                }
                tokio::time::sleep(update_interval).await;
            }
        }
        .instrument(debug_span!("collect_storage_metrics")),
    )
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = NodeConfig::load_and_process(args().collect());
    if let Err(ConfigError::CommandInput(clap_err)) = config {
        clap_err.exit();
    }

    configure_tracing();

    let config = config?;
    if let Err(errors) = config_validate(&config) {
        error!("{}", errors);
        exit(1);
    }

    COLLECT_PROFILING_METRICS
        .set(config.collect_profiling_metrics)
        .expect("This should be the first and only time we set this value.");

    info!("Booting up.");
    run_threads(config).await
}
