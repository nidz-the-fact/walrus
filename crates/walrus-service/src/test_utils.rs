// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

//! Test utilities for using storage nodes in tests.
//!
//! For creating an instance of a single storage node in a test, see [`StorageNodeHandleBuilder`] .
//!
//! For creating a cluster of test storage nodes, see [`TestClusterBuilder`].

use std::{
    borrow::Borrow,
    net::{SocketAddr, TcpStream},
    num::NonZeroU16,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Mutex},
};

use async_trait::async_trait;
use chrono::Utc;
use futures::{stream::FuturesUnordered, StreamExt};
use prometheus::Registry;
use sui_types::base_types::ObjectID;
use tempfile::TempDir;
use tokio::{task::JoinHandle, time::Duration};
use tokio_stream::{wrappers::BroadcastStream, Stream};
use tokio_util::sync::CancellationToken;
use tracing::Instrument;
use typed_store::{rocks::MetricConf, Map};
use walrus_core::{
    encoding::EncodingConfig,
    keys::{NetworkKeyPair, ProtocolKeyPair},
    merkle::MerkleProof,
    messages::InvalidBlobCertificate,
    metadata::VerifiedBlobMetadataWithId,
    BlobId,
    Epoch,
    InconsistencyProof as InconsistencyProofEnum,
    NetworkPublicKey,
    PublicKey,
    ShardIndex,
    Sliver,
    SliverPairIndex,
    SliverType,
};
use walrus_sdk::client::Client;
use walrus_sui::{
    client::FixedSystemParameters,
    test_utils::{system_setup::SystemContext, TestClusterHandle},
    types::{
        move_structs::{EpochState, VotingParams},
        Committee,
        ContractEvent,
        NetworkAddress,
        NodeRegistrationParams,
        StorageNode as SuiStorageNode,
        StorageNodeCap,
        GENESIS_EPOCH,
    },
};
use walrus_test_utils::WithTempDir;

#[cfg(msim)]
use crate::node::config::{self, SuiConfig};
use crate::{
    common::active_committees::ActiveCommittees,
    node::{
        committee::{
            BeginCommitteeChangeError,
            CommitteeLookupService,
            CommitteeService,
            DefaultNodeServiceFactory,
            EndCommitteeChangeError,
            NodeCommitteeService,
        },
        config::{EventProviderConfig, StorageNodeConfig},
        contract_service::SystemContractService,
        errors::SyncShardClientError,
        events::{
            event_processor::EventProcessor,
            EventSequenceNumber,
            EventStreamCursor,
            IndexedStreamElement,
        },
        server::{UserServer, UserServerConfig},
        system_events::{EventManager, EventRetentionManager, SystemEventProvider},
        DatabaseConfig,
        Storage,
        StorageNode,
    },
};

/// A system event manager that provides events from a stream. It does not support dropping events.
#[derive(Debug)]
pub struct DefaultSystemEventManager {
    event_provider: Box<dyn SystemEventProvider>,
}

impl DefaultSystemEventManager {
    /// Creates a new system event manager with the provided event provider.
    pub fn new(event_provider: Box<dyn SystemEventProvider>) -> Self {
        Self { event_provider }
    }
}

#[async_trait]
impl SystemEventProvider for DefaultSystemEventManager {
    async fn events(
        &self,
        cursor: EventStreamCursor,
    ) -> anyhow::Result<
        Box<dyn Stream<Item = IndexedStreamElement> + Send + Sync + 'life0>,
        anyhow::Error,
    > {
        self.event_provider.events(cursor).await
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait]
impl EventRetentionManager for DefaultSystemEventManager {
    async fn drop_events_before(&self, _cursor: EventStreamCursor) -> Result<(), anyhow::Error> {
        Ok(())
    }
}

#[async_trait]
impl EventManager for DefaultSystemEventManager {}

/// Trait representing a storage node handle.
/// The trait is used to abstract over the different types of storage node handles.
/// More specifically, there are two types:
///   - a node handle that the tester has control over all the internal components of the node.
///   - a node handle that is used in the simulation tests which supports crash and recovery.
pub trait StorageNodeHandleTrait {
    /// Cancels the node's event loop and REST API.
    fn cancel(&self);

    /// Returns the client that can be used to communicate with the node.
    fn client(&self) -> &Client;

    /// Builds a new storage node handle, and starts the node.
    fn build_and_run(
        builder: StorageNodeHandleBuilder,
        sui_cluster_handle: Option<Arc<TestClusterHandle>>,
        system_context: Option<SystemContext>,
        storage_dir: TempDir,
    ) -> impl std::future::Future<Output = anyhow::Result<Self>> + Send
    where
        Self: Sized;

    /// Returns the storage node.
    fn storage_node(&self) -> &Arc<StorageNode>;

    /// Returns the node's protocol public key.
    fn public_key(&self) -> &PublicKey;

    /// Returns whether the storage node should use a distinct IP address.
    fn use_distinct_ip() -> bool;
}

/// A storage node and associated data for testing.
#[derive(Debug)]
pub struct StorageNodeHandle {
    /// The wrapped storage node.
    pub storage_node: Arc<StorageNode>,
    /// The temporary directory containing the node's storage.
    pub storage_directory: TempDir,
    /// The node's protocol public key.
    pub public_key: PublicKey,
    /// The node's protocol public key.
    pub network_public_key: NetworkPublicKey,
    /// The address of the REST API.
    pub rest_api_address: SocketAddr,
    /// The address of the metric service.
    pub metrics_address: SocketAddr,
    /// Handle the REST API.
    pub rest_api: Arc<UserServer<StorageNode>>,
    /// Cancellation token for the REST API.
    pub cancel: CancellationToken,
    /// Client that can be used to communicate with the node.
    pub client: Client,
    /// The storage capability object of the node.
    pub storage_node_capability: Option<StorageNodeCap>,
    /// The handle to the node runtime.
    pub node_runtime_handle: Option<JoinHandle<()>>,
}

impl StorageNodeHandleTrait for StorageNodeHandle {
    fn cancel(&self) {
        self.cancel.cancel();
    }

    fn client(&self) -> &Client {
        &self.client
    }

    fn storage_node(&self) -> &Arc<StorageNode> {
        &self.storage_node
    }

    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    async fn build_and_run(
        builder: StorageNodeHandleBuilder,
        _sui_cluster_handle: Option<Arc<TestClusterHandle>>,
        _system_context: Option<SystemContext>,
        _storage_dir: TempDir,
    ) -> anyhow::Result<Self> {
        builder.build().await
    }

    // StorageNodeHandle is only used in integration test without crash and recovery. No need to
    // use distinct IP.
    fn use_distinct_ip() -> bool {
        false
    }
}

impl StorageNodeHandle {
    /// Creates a new builder.
    pub fn builder() -> StorageNodeHandleBuilder {
        StorageNodeHandleBuilder::default()
    }
}

impl AsRef<StorageNode> for StorageNodeHandle {
    fn as_ref(&self) -> &StorageNode {
        &self.storage_node
    }
}

/// A storage node handle for simulation tests. Comparing to StorageNodeHandle, this struct
/// removes any storage node internal state, and adds a simulator node id for node management.
#[cfg(msim)]
#[derive(Debug)]
pub struct SimStorageNodeHandle {
    /// The temporary directory containing the node's storage.
    pub storage_directory: TempDir,
    /// The node's protocol public key.
    pub public_key: PublicKey,
    /// The node's protocol public key.
    pub network_public_key: NetworkPublicKey,
    /// The address of the REST API.
    pub rest_api_address: SocketAddr,
    /// The address of the metric service.
    pub metrics_address: SocketAddr,
    /// Cancellation token for the node.
    pub cancel_token: CancellationToken,
    /// The wrapped simulator node id.
    pub node_id: sui_simulator::task::NodeId,
    /// The storage capability object of the node.
    pub storage_node_capability: Option<StorageNodeCap>,
}

#[cfg(msim)]
impl SimStorageNodeHandle {
    // Starts and runs a storage node with the provided configuration in a dedicated simulator
    // node.
    async fn spawn_node(
        config: StorageNodeConfig,
        cancel_token: CancellationToken,
    ) -> sui_simulator::runtime::NodeHandle {
        let (startup_sender, mut startup_receiver) = tokio::sync::watch::channel(false);

        // Extract the IP address from the configuration.
        let ip = match config.rest_api_address {
            SocketAddr::V4(v4) => std::net::IpAddr::V4(*v4.ip()),
            _ => panic!("unsupported protocol"),
        };

        let startup_sender = Arc::new(startup_sender);
        let handle = sui_simulator::runtime::Handle::current();
        let builder = handle.create_node();

        // This is the entry point of node restart.
        let node_handle = builder
            .ip(ip)
            .name(&format!(
                "{:?}",
                config
                    .protocol_key_pair
                    .get()
                    .expect("config must contain protocol key pair")
                    .public()
            ))
            .init(move || {
                tracing::info!("Starting simulator node. Ip: {:?}", ip);

                let config = config.clone();
                let cancel_token = cancel_token.clone();
                let startup_sender = startup_sender.clone();

                async move {
                    let (rest_api_handle, node_handle, event_processor_handle) =
                        Self::build_and_run_node(config, cancel_token.clone())
                            .await
                            .expect("Should start node successfully");

                    startup_sender.send(true).ok();

                    tokio::select! {
                        _ = rest_api_handle => {
                            tracing::info!("rest API stopped");
                        }
                        _ = node_handle => {
                            tracing::info!("node stopped");
                        }
                        _ = event_processor_handle => {
                            tracing::info!("event processor stopped");
                        }
                        _ = cancel_token.cancelled() => {
                            tracing::info!("node stopped by cancellation");
                        }
                    }

                    tracing::info!("node stopped");
                }
            })
            .build();

        // Wait for the node to start and running.
        startup_receiver
            .changed()
            .await
            .expect("waiting for node to start failed");

        node_handle
    }

    /// Builds and runs a storage node with the provided configuration. Returns the handles to the
    /// REST API, the node, and the event processor.
    async fn build_and_run_node(
        config: StorageNodeConfig,
        cancel_token: CancellationToken,
    ) -> anyhow::Result<(
        tokio::task::JoinHandle<Result<(), std::io::Error>>,
        tokio::task::JoinHandle<Result<(), anyhow::Error>>,
        tokio::task::JoinHandle<Result<(), anyhow::Error>>,
    )> {
        // TODO(#996): extract the common code to start the code, and use it here as well as in
        // StorageNodeRuntime::start.

        let metrics_registry = Registry::default();

        let sui_config = config
            .sui
            .clone()
            .expect("simulation must set sui config in storage node config");

        // Starts the event processor thread if the node is configured to use the checkpoint
        // based event processor.
        let sui_read_client = sui_config.new_read_client().await?;
        let event_provider: Box<dyn EventManager> = match &config.event_provider_config {
            EventProviderConfig::CheckpointBasedEventProcessor(event_processor_config) => {
                let event_processor_config = event_processor_config.clone().unwrap_or_else(|| {
                    crate::node::events::EventProcessorConfig::new_with_default_pruning_interval(
                        sui_config.rpc.clone(),
                    )
                });

                Box::new(
                    EventProcessor::new(
                        &event_processor_config,
                        sui_config.rpc,
                        sui_read_client.get_system_package_id(),
                        Duration::from_millis(100),
                        tempfile::tempdir()
                            .expect("temporary directory creation must succeed")
                            .path(),
                        &metrics_registry,
                    )
                    .await?,
                )
            }
            EventProviderConfig::LegacyEventProvider => {
                Box::new(crate::node::system_events::SuiSystemEventProvider::new(
                    sui_read_client.clone(),
                    Duration::from_millis(100),
                ))
            }
        };

        // Starts the event processor thread if it is configured, otherwise it produces a JoinHandle
        // that never returns.
        let event_processor_handle = if let Some(event_processor) =
            event_provider.as_any().downcast_ref::<EventProcessor>()
        {
            let cloned_cancel_token = cancel_token.clone();
            let cloned_event_processor = event_processor.clone();
            tokio::spawn(
                async move { cloned_event_processor.start(cloned_cancel_token).await }
                    .instrument(tracing::info_span!("cluster-event-processor",
                    address = %config.rest_api_address)),
            )
        } else {
            tokio::spawn(async { std::future::pending::<Result<(), anyhow::Error>>().await })
        };

        // Build storage node with the current configuration and event manager.
        let node = StorageNode::builder()
            .with_system_event_manager(event_provider)
            .build(&config, metrics_registry.clone())
            .await?;
        let node = Arc::new(node);

        // Starts rest api and node threads.
        let rest_api = Arc::new(UserServer::new(
            node.clone(),
            cancel_token.clone(),
            UserServerConfig::from(&config),
            &metrics_registry,
        ));

        let rest_api_handle = tokio::task::spawn(async move { rest_api.run().await }.instrument(
            tracing::info_span!("cluster-rest-api", address = %config.rest_api_address),
        ));

        let node_handle =
            tokio::task::spawn(async move { node.run(cancel_token).await }.instrument(
                tracing::info_span!("cluster-node", address = %config.rest_api_address),
            ));

        Ok((rest_api_handle, node_handle, event_processor_handle))
    }
}

/// A storage node handle builder for simulation tests. The main difference between
/// SimStorageNodeHandle and StorageNodeHandle is that SimStorageNodeHandle is used in crash
/// recovery simulation and therefore the StorageNode instance may change overtime.
#[cfg(msim)]
impl StorageNodeHandleTrait for SimStorageNodeHandle {
    fn cancel(&self) {
        unimplemented!("cannot directly cancel a storage proactively.")
    }

    fn client(&self) -> &Client {
        unimplemented!("simulation test does not use the pre-built storage node client.")
    }

    fn storage_node(&self) -> &Arc<StorageNode> {
        // Storage node state changes everytime the node restarts.
        unimplemented!("simulation test does not have a stable internal state.")
    }

    fn public_key(&self) -> &PublicKey {
        &self.public_key
    }

    async fn build_and_run(
        builder: StorageNodeHandleBuilder,
        sui_cluster_handle: Option<Arc<TestClusterHandle>>,
        system_context: Option<SystemContext>,
        storage_dir: TempDir,
    ) -> anyhow::Result<Self>
    where
        Self: Sized,
    {
        builder
            .start_node(
                sui_cluster_handle.expect("SUI cluster handle must be provided in simtest"),
                system_context.expect("System context must be provided"),
                storage_dir,
            )
            .await
    }

    // Storage node in simulation requires to have distinct IP since ip represents the id of
    // the simulator node.
    fn use_distinct_ip() -> bool {
        true
    }
}

#[cfg(msim)]
impl Drop for SimStorageNodeHandle {
    fn drop(&mut self) {
        self.cancel_token.cancel();
    }
}

/// Builds a new [`StorageNodeHandle`] with custom configuration values.
///
/// Can be created with the methods [`StorageNodeHandle::builder()`] or with
/// [`StorageNodeHandleBuilder::new()`].
///
/// Methods can be chained in order to set the configuration values, with the `StorageNode` being
/// constructed by calling [`build`][Self::build`].
///
/// See function level documentation for details on the various configuration settings.
#[derive(Debug)]
pub struct StorageNodeHandleBuilder {
    storage: Option<WithTempDir<Storage>>,
    event_provider: Box<dyn SystemEventProvider>,
    committee_service: Option<Box<dyn CommitteeService>>,
    contract_service: Option<Box<dyn SystemContractService>>,
    run_rest_api: bool,
    run_node: bool,
    test_config: Option<StorageNodeTestConfig>,
    initial_epoch: Option<Epoch>,
    storage_node_capability: Option<StorageNodeCap>,
    node_wallet_dir: Option<PathBuf>,
}

impl StorageNodeHandleBuilder {
    /// Creates a new builder, which by default creates a storage node without any assigned shards
    /// and without its REST API or event loop running.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the storage associated with the node.
    ///
    /// If a committee service is *not* provided with [`Self::with_committee_service`], then the
    /// storage also dictates the shard assignment to this storage node in the created committee.
    pub fn with_storage(mut self, storage: WithTempDir<Storage>) -> Self {
        self.storage = Some(storage);
        self
    }

    /// Sets the service providing events to the storage node.
    pub fn with_system_event_provider<T>(self, event_provider: T) -> Self
    where
        T: SystemEventProvider + Into<Box<T>> + 'static,
    {
        self.with_boxed_system_event_provider(event_provider.into())
    }

    /// Sets the service providing events to the storage node.
    pub fn with_boxed_system_event_provider(
        mut self,
        event_provider: Box<dyn SystemEventProvider>,
    ) -> Self {
        self.event_provider = event_provider;
        self
    }

    /// Sets the [`CommitteeService`] used with the node.
    ///
    /// If not provided, defaults to [`StubCommitteeService`] created with a valid committee
    /// constructed over at most 1 other node. Note that if the node has no shards assigned to it
    /// (as inferred from the storage), it will not be in the committee.
    pub fn with_committee_service(mut self, service: Box<dyn CommitteeService>) -> Self {
        self.committee_service = Some(service);
        self
    }

    /// Sets the [`SystemContractService`] to be used with the node.
    ///
    /// If not provided, defaults to a [`StubContractService`].
    pub fn with_system_contract_service(
        mut self,
        contract_service: Box<dyn SystemContractService>,
    ) -> Self {
        self.contract_service = Some(contract_service);
        self
    }

    /// Enable or disable the node's event loop being started on build.
    pub fn with_node_started(mut self, run_node: bool) -> Self {
        self.run_node = run_node;
        self
    }

    /// Enable or disable the REST API being started on build.
    pub fn with_rest_api_started(mut self, run_rest_api: bool) -> Self {
        self.run_rest_api = run_rest_api;
        self
    }

    /// Specify the shard assignment for this node.
    ///
    /// If specified, it will determine the the shard assignment for the node in the committee. If
    /// not, the shard assignment will be inferred from the shards present in the storage.
    ///
    /// Resets any prior calls to [`Self::with_test_config`].
    pub fn with_shard_assignment(mut self, shards: &[ShardIndex]) -> Self {
        self.test_config = Some(StorageNodeTestConfig::new(shards.into(), false));
        self
    }

    /// Specify the test config for this node.
    ///
    /// If specified, it will determine the the shard assignment for the node in the committee
    /// as well as the network address and the protocol key.
    ///
    /// Resets any prior calls to [`Self::with_shard_assignment`].
    pub fn with_test_config(mut self, test_config: StorageNodeTestConfig) -> Self {
        self.test_config = Some(test_config);
        self
    }

    /// Specify the initial epoch for the node.
    ///
    /// If specified, the committee service will be created with the provided epoch.
    pub fn with_initial_epoch(mut self, epoch: Option<Epoch>) -> Self {
        self.initial_epoch = epoch;
        self
    }

    /// Specify the storage capability for the node.
    pub fn with_storage_node_capability(
        mut self,
        storage_node_capability: Option<StorageNodeCap>,
    ) -> Self {
        self.storage_node_capability = storage_node_capability;
        self
    }

    /// Specify the sui wallet directory for the node.
    pub fn with_node_wallet_dir(mut self, node_wallet_dir: Option<PathBuf>) -> Self {
        self.node_wallet_dir = node_wallet_dir;
        self
    }

    /// Creates the configured [`StorageNodeHandle`].
    pub async fn build(self) -> anyhow::Result<StorageNodeHandle> {
        // Identify the storage being used, as it allows us to extract the shards
        // that should be assigned to this storage node.
        let WithTempDir {
            inner: storage,
            temp_dir,
        } = self
            .storage
            .unwrap_or_else(|| empty_storage_with_shards(&[]));

        let node_info = self
            .test_config
            .unwrap_or_else(|| StorageNodeTestConfig::new(storage.shards_present(), false));
        // To be in the committee, the node must have at least one shard assigned to it.
        let is_in_committee = !node_info.shards.is_empty();

        let public_key = node_info.key_pair.public().clone();
        let network_public_key = node_info.network_key_pair.public().clone();

        let committee_service = self.committee_service.unwrap_or_else(|| {
            // Create a list of the committee members, that contains one or two nodes.
            let committee_members = [
                is_in_committee.then(|| node_info.to_storage_node_info("node-under-test")),
                committee_partner(&node_info).map(|info| info.to_storage_node_info("other-node")),
            ];
            debug_assert!(committee_members[0].is_some() || committee_members[1].is_some());

            let committee = committee_from_members(
                // Remove the possible None in the members list
                committee_members.into_iter().flatten().collect(),
                self.initial_epoch,
            );
            Box::new(StubCommitteeService {
                encoding: EncodingConfig::new(committee.n_shards()).into(),
                committee: committee.into(),
            })
        });

        let contract_service = self.contract_service.unwrap_or_else(|| {
            Box::new(StubContractService {
                system_parameters: FixedSystemParameters {
                    n_shards: committee_service.committee().n_shards(),
                    max_epochs_ahead: 200,
                    epoch_duration: Duration::from_secs(600),
                    epoch_zero_end: Utc::now() + Duration::from_secs(60),
                },
            })
        });

        // Create the node's config using the previously generated keypair and address.
        let config = StorageNodeConfig {
            storage_path: temp_dir.path().to_path_buf(),
            protocol_key_pair: node_info.key_pair.into(),
            network_key_pair: node_info.network_key_pair.into(),
            rest_api_address: node_info.rest_api_address,
            public_host: Some(node_info.rest_api_address.ip().to_string()),
            ..storage_node_config().inner
        };

        let cancel_token = CancellationToken::new();

        if let Some(event_processor) = self
            .event_provider
            .as_any()
            .downcast_ref::<EventProcessor>()
        {
            let cloned_cancel_token = cancel_token.clone();
            let cloned_event_processor = event_processor.clone();
            spawn_event_processor(
                cloned_event_processor,
                cloned_cancel_token,
                config.rest_api_address.to_string(),
            );
            wait_for_event_processor_to_start(
                event_processor.clone(),
                event_processor.client.clone(),
            )
            .await?;
        }

        let metrics_registry = Registry::default();
        let node = StorageNode::builder()
            .with_storage(storage)
            .with_system_event_manager(Box::new(DefaultSystemEventManager::new(
                self.event_provider,
            )))
            .with_committee_service(committee_service)
            .with_system_contract_service(contract_service)
            .build(&config, metrics_registry.clone())
            .await?;
        let node = Arc::new(node);

        let rest_api = Arc::new(UserServer::new(
            node.clone(),
            cancel_token.clone(),
            UserServerConfig::from(&config),
            &metrics_registry,
        ));

        if self.run_rest_api {
            let rest_api_clone = rest_api.clone();

            tokio::task::spawn(
                async move {
                    let status = rest_api_clone.run().await;
                    if let Err(error) = status {
                        tracing::error!("rest api stopped with error: {:?}", error);
                        std::process::exit(1);
                    }
                }
                .instrument(
                    tracing::info_span!("cluster-node", address = %config.rest_api_address),
                ),
            );
        }

        let node_runtime_handle = if self.run_node {
            let node = node.clone();
            let cancel_token = cancel_token.clone();

            Some(tokio::task::spawn(
                async move {
                    let status = node.run(cancel_token).await;
                    if let Err(error) = status {
                        tracing::error!("node stopped with error: {:?}", error);
                        std::process::exit(1);
                    }
                }
                .instrument(
                    tracing::info_span!("cluster-node", address = %config.rest_api_address),
                ),
            ))
        } else {
            None
        };

        let client = Client::builder()
            .authenticate_with_public_key(network_public_key.clone())
            // Disable proxy and root certs from the OS for tests.
            .no_proxy()
            .tls_built_in_root_certs(false)
            .build_for_remote_ip(config.rest_api_address)?;

        if self.run_rest_api {
            wait_for_rest_api_ready(&client).await?;
        }

        Ok(StorageNodeHandle {
            storage_node: node,
            storage_directory: temp_dir,
            public_key,
            network_public_key,
            rest_api_address: config.rest_api_address,
            metrics_address: config.metrics_address,
            rest_api,
            cancel: cancel_token,
            client,
            storage_node_capability: self.storage_node_capability,
            node_runtime_handle,
        })
    }

    /// Starts and running a storage node with the provided configuration.
    #[cfg(msim)]
    pub async fn start_node(
        self,
        sui_cluster_handle: Arc<TestClusterHandle>,
        system_context: SystemContext,
        storage_dir: TempDir,
    ) -> anyhow::Result<SimStorageNodeHandle> {
        let node_info = self
            .test_config
            .expect("test config must be provided to spawn a storage node");

        // Builds the storage node config used to run the node.
        let storage_node_config = StorageNodeConfig {
            storage_path: storage_dir.path().to_path_buf(),
            protocol_key_pair: node_info.key_pair.into(),
            network_key_pair: node_info.network_key_pair.into(),
            rest_api_address: node_info.rest_api_address,
            public_host: Some(node_info.rest_api_address.ip().to_string()),
            sui: Some(SuiConfig {
                rpc: sui_cluster_handle.cluster().rpc_url().to_string(),
                system_object: system_context.system_object,
                staking_object: system_context.staking_object,
                wallet_config: self.node_wallet_dir.unwrap().join("wallet_config.yaml"),
                event_polling_interval: config::defaults::polling_interval(),
                gas_budget: config::defaults::gas_budget(),
            }),
            ..storage_node_config().inner
        };

        let cancel_token = CancellationToken::new();

        let handle =
            SimStorageNodeHandle::spawn_node(storage_node_config.clone(), cancel_token.clone())
                .await;

        Ok(SimStorageNodeHandle {
            storage_directory: storage_dir,
            public_key: storage_node_config
                .protocol_key_pair
                .get()
                .expect("protocol key must be set")
                .public()
                .clone(),
            network_public_key: storage_node_config
                .network_key_pair
                .get()
                .expect("network key must be set")
                .public()
                .clone(),
            rest_api_address: storage_node_config.rest_api_address,
            metrics_address: storage_node_config.metrics_address,
            cancel_token,
            node_id: handle.id(),
            storage_node_capability: self.storage_node_capability,
        })
    }
}

impl Default for StorageNodeHandleBuilder {
    fn default() -> Self {
        Self {
            event_provider: Box::<Vec<ContractEvent>>::default(),
            committee_service: None,
            storage: Default::default(),
            run_rest_api: Default::default(),
            run_node: Default::default(),
            contract_service: None,
            test_config: None,
            initial_epoch: None,
            storage_node_capability: None,
            node_wallet_dir: None,
        }
    }
}

/// Waits until the node is ready by querying the node's health info endpoint using the node
/// client.
async fn wait_for_rest_api_ready(client: &Client) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(10), async {
        while let Err(err) = client.get_server_health_info(false).await {
            tracing::trace!(%err, "node is not ready yet, retrying...");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        Ok(())
    })
    .await?
}

/// Returns with a test config for a storage node that would make a valid committee when paired
/// with the provided node, if necessary.
///
/// The number of shards in the system inferred from the shards assigned in the provided config.
/// It is at least 3 and is defined as `n = max(max(shard_ids) + 1, 3)`. If the shards `0..n` are
/// assigned to the existing node, then this function returns `None`. Otherwise, there must be a
/// second node in the committee with the shards not managed by the provided node.
fn committee_partner(node_config: &StorageNodeTestConfig) -> Option<StorageNodeTestConfig> {
    const MIN_SHARDS: u16 = 3;
    let n_shards = node_config
        .shards
        .iter()
        .max()
        .map(|index| index.get() + 1)
        .unwrap_or(MIN_SHARDS)
        .max(MIN_SHARDS);

    let other_shards: Vec<_> = ShardIndex::range(..n_shards)
        .filter(|id| !node_config.shards.contains(id))
        .collect();

    if !other_shards.is_empty() {
        Some(StorageNodeTestConfig::new(other_shards, false))
    } else {
        None
    }
}

#[cfg(not(msim))]
fn spawn_event_processor(
    event_processor: EventProcessor,
    cancellation_token: CancellationToken,
    rest_api_address: String,
) -> JoinHandle<()> {
    get_runtime().spawn(
        async move {
            let status = event_processor.start(cancellation_token).await;
            if let Err(error) = status {
                tracing::error!("event processor stopped with error: {:?}", error);
                std::process::exit(1);
            }
        }
        .instrument(tracing::info_span!("cluster-node", address = %rest_api_address)),
    )
}

#[cfg(msim)]
fn spawn_event_processor(
    event_processor: EventProcessor,
    cancellation_token: CancellationToken,
    rest_api_address: String,
) -> JoinHandle<()> {
    tokio::task::spawn(
        async move {
            let status = event_processor.start(cancellation_token).await;
            if let Err(error) = status {
                tracing::error!("event processor stopped with error: {:?}", error);
                std::process::exit(1);
            }
        }
        .instrument(tracing::info_span!("cluster-node", address = %rest_api_address)),
    )
}

/// Returns a runtime that can be used to spawn tasks.
#[cfg(not(msim))]
fn get_runtime() -> std::sync::MutexGuard<'static, tokio::runtime::Runtime> {
    use std::sync::OnceLock;

    static CLUSTER: OnceLock<std::sync::Mutex<tokio::runtime::Runtime>> = OnceLock::new();
    CLUSTER
        .get_or_init(|| {
            std::sync::Mutex::new(
                std::thread::spawn(move || {
                    tokio::runtime::Builder::new_multi_thread()
                        .enable_all()
                        .build()
                        .expect("should be able to build runtime")
                })
                .join()
                .expect("should be able to wait for thread to finish"),
            )
        })
        .lock()
        .unwrap()
}

fn create_previous_committee(committee: &Committee) -> Option<Committee> {
    if committee.epoch == GENESIS_EPOCH {
        None
    } else {
        let members = if committee.epoch - 1 == GENESIS_EPOCH {
            vec![]
        } else {
            committee.members().to_vec()
        };

        Some(
            Committee::new(members, committee.epoch - 1, committee.n_shards())
                .expect("all fields are valid"),
        )
    }
}

/// A stub `CommitteeLookupService`.
///
/// Does not perform any network operations.
#[derive(Debug, Clone)]
pub struct StubLookupService {
    committees: Arc<Mutex<ActiveCommittees>>,
}

impl StubLookupService {
    /// Create a new `StubLookupService` with the specified committee.
    ///
    /// The previous committee is set to the same committee but with an epoch of 1 less. If the
    /// provided committee is at epoch 0 or 1, then the prior committee is non-existent or the empty
    /// committee respectively.
    pub fn new(committee: Committee) -> Self {
        let previous = create_previous_committee(&committee);
        Self {
            committees: Arc::new(Mutex::new(ActiveCommittees::new(committee, previous))),
        }
    }

    /// Returns a handle that can be used to modify the ActiveCommittees returned by the
    /// lookup service.
    pub fn handle(&self) -> StubLookupServiceHandle {
        StubLookupServiceHandle {
            committees: self.committees.clone(),
        }
    }
}

/// A handle to a [`StubLookupService`] which can be used to modify the value of the services.
#[derive(Debug, Clone)]
pub struct StubLookupServiceHandle {
    /// The active committees.
    pub committees: Arc<Mutex<ActiveCommittees>>,
}

impl StubLookupServiceHandle {
    /// Returns the current epoch.
    pub fn epoch(&self) -> Epoch {
        self.committees.lock().unwrap().epoch()
    }

    /// Advances the epoch to the next epoch, skipping the transitioning phase.
    pub fn advance_epoch(&self) -> Epoch {
        let mut committees = self.committees.lock().unwrap();
        let current = (**committees.current_committee()).clone();
        let next_epoch = current.epoch + 1;
        let next = if let Some(next_committee) = committees.next_committee() {
            (**next_committee).clone()
        } else {
            Committee::new(current.members().to_vec(), next_epoch, current.n_shards())
                .expect("static committee is valid")
        };
        *committees = ActiveCommittees::new(next, Some(current));
        next_epoch
    }

    /// Sets the next epoch committee.
    pub fn set_next_epoch_committee(&self, next_committee: Committee) {
        let mut committees = self.committees.lock().unwrap();
        *committees = ActiveCommittees::new_with_next(
            committees.current_committee().clone(),
            committees.previous_committee().cloned(),
            Some(Arc::new(next_committee)),
            true,
        )
    }
}

#[async_trait]
impl CommitteeLookupService for StubLookupService {
    async fn get_active_committees(&self) -> Result<ActiveCommittees, anyhow::Error> {
        Ok(self.committees.lock().unwrap().clone())
    }
}

/// A stub [`CommitteeService`].
///
/// Does not perform any network operations.
#[derive(Debug)]
pub struct StubCommitteeService {
    /// The current committee.
    pub committee: Arc<Committee>,
    /// The system's encoding config.
    pub encoding: Arc<EncodingConfig>,
}

#[async_trait]
impl CommitteeService for StubCommitteeService {
    fn get_epoch(&self) -> Epoch {
        1
    }

    fn get_shard_count(&self) -> NonZeroU16 {
        self.committee.n_shards()
    }

    fn encoding_config(&self) -> &Arc<EncodingConfig> {
        &self.encoding
    }

    async fn get_and_verify_metadata(
        &self,
        _blob_id: BlobId,
        _certified_epoch: Epoch,
    ) -> VerifiedBlobMetadataWithId {
        std::future::pending().await
    }

    async fn recover_sliver(
        &self,
        _metadata: Arc<VerifiedBlobMetadataWithId>,
        _sliver_id: SliverPairIndex,
        _sliver_type: SliverType,
        _certified_epoch: Epoch,
    ) -> Result<Sliver, InconsistencyProofEnum<MerkleProof>> {
        std::future::pending().await
    }

    async fn get_invalid_blob_certificate(
        &self,
        _blob_id: BlobId,
        _inconsistency_proof: &InconsistencyProofEnum,
    ) -> InvalidBlobCertificate {
        std::future::pending().await
    }

    async fn sync_shard_before_epoch(
        &self,
        _shard_index: ShardIndex,
        _starting_blob_id: BlobId,
        _sliver_type: SliverType,
        _sliver_count: u64,
        _epoch: Epoch,
        _key_pair: &ProtocolKeyPair,
    ) -> Result<Vec<(BlobId, Sliver)>, SyncShardClientError> {
        std::future::pending().await
    }

    fn committee(&self) -> Arc<Committee> {
        self.committee.clone()
    }

    fn active_committees(&self) -> ActiveCommittees {
        ActiveCommittees::new(
            self.committee.as_ref().clone(),
            create_previous_committee(&self.committee),
        )
    }

    fn is_walrus_storage_node(&self, public_key: &PublicKey) -> bool {
        self.committee
            .members()
            .iter()
            .any(|node| node.public_key == *public_key)
    }

    async fn begin_committee_change(
        &self,
        _new_epoch: Epoch,
    ) -> Result<(), BeginCommitteeChangeError> {
        // Return immediately in test since begin committee change is a required step in starting
        // new storage node.
        Ok(())
    }

    fn end_committee_change(&self, _epoch: Epoch) -> Result<(), EndCommitteeChangeError> {
        Ok(())
    }

    async fn begin_committee_change_to_latest_committee(
        &self,
    ) -> Result<(), BeginCommitteeChangeError> {
        Ok(())
    }
}

/// A stub [`SystemContractService`].
///
/// Performs a no-op when calling [`invalidate_blob_id()`][Self::invalidate_blob_id]
#[derive(Debug)]
pub struct StubContractService {
    system_parameters: FixedSystemParameters,
}

#[async_trait]
impl SystemContractService for StubContractService {
    async fn invalidate_blob_id(&self, _certificate: &InvalidBlobCertificate) {}

    async fn epoch_sync_done(&self, _epoch: Epoch) {}

    async fn get_epoch_and_state(&self) -> Result<(Epoch, EpochState), anyhow::Error> {
        anyhow::bail!("stub service does not store the epoch or state")
    }

    async fn fixed_system_parameters(&self) -> Result<FixedSystemParameters, anyhow::Error> {
        Ok(self.system_parameters.clone())
    }

    async fn end_voting(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!("stub service cannot end voting")
    }

    async fn initiate_epoch_change(&self) -> Result<(), anyhow::Error> {
        anyhow::bail!("stub service cannot initiate epoch change")
    }
}

/// Returns a socket address that is not currently in use on the system.
///
/// distinct_ip: If true, the returned address will have a distinct IP address from the local
/// machine.
pub fn unused_socket_address(distinct_ip: bool) -> SocketAddr {
    if distinct_ip {
        try_unused_socket_address_with_distinct_ip()
            .expect("unused socket address with distinct ip to be available")
    } else {
        try_unused_socket_address().expect("unused socket address to be available")
    }
}

fn try_unused_socket_address() -> anyhow::Result<SocketAddr> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;

    // Create and accept a connection to force the port into the TIME_WAIT state
    let _client_stream = TcpStream::connect(address)?;
    let _server_stream = listener.accept()?;
    Ok(address)
}

fn try_unused_socket_address_with_distinct_ip() -> anyhow::Result<SocketAddr> {
    Ok(SocketAddr::from_str(
        (sui_config::local_ip_utils::get_new_ip() + ":1314").as_str(),
    )?)
}

#[async_trait::async_trait]
impl SystemEventProvider for Vec<ContractEvent> {
    async fn events(
        &self,
        _cursor: EventStreamCursor,
    ) -> Result<Box<dyn Stream<Item = IndexedStreamElement> + Send + Sync + 'life0>, anyhow::Error>
    {
        Ok(Box::new(
            tokio_stream::iter(
                self.clone()
                    .into_iter()
                    .map(|c| IndexedStreamElement::new(c, EventSequenceNumber::new(0, 0))),
            )
            .chain(tokio_stream::pending()),
        ))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

#[async_trait::async_trait]
impl SystemEventProvider for tokio::sync::broadcast::Sender<ContractEvent> {
    async fn events(
        &self,
        _cursor: EventStreamCursor,
    ) -> Result<Box<dyn Stream<Item = IndexedStreamElement> + Send + Sync + 'life0>, anyhow::Error>
    {
        Ok(Box::new(BroadcastStream::new(self.subscribe()).map(
            |value| {
                IndexedStreamElement::new(
                    value.expect("should not return errors in test"),
                    EventSequenceNumber::new(0, 0),
                )
            },
        )))
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// A cluster of [`StorageNodeHandle`]s corresponding to several running storage nodes.
#[derive(Debug)]
pub struct TestCluster<T: StorageNodeHandleTrait = StorageNodeHandle> {
    /// The running storage nodes.
    pub nodes: Vec<T>,
    /// A handle to the stub lookup service, is used.
    pub lookup_service_handle: Option<StubLookupServiceHandle>,
    /// The number of shards in the system.
    pub n_shards: usize,
}

impl<T: StorageNodeHandleTrait> TestCluster<T> {
    /// Returns a new builder to create the [`TestCluster`].
    pub fn builder() -> TestClusterBuilder {
        TestClusterBuilder::new(T::use_distinct_ip())
    }

    /// Returns an encoding config valid for use with the storage nodes.
    pub fn encoding_config(&self) -> EncodingConfig {
        let n_shards: u16 = self.n_shards.try_into().expect("valid number of shards");
        EncodingConfig::new(NonZeroU16::new(n_shards).expect("more than 1 shard"))
    }

    /// Stops the storage node with index `idx` by cancelling its task.
    pub fn cancel_node(&mut self, idx: usize) {
        assert!(
            idx < self.nodes.len(),
            "the index of the node to be dropped must be within the node vector"
        );
        self.nodes[idx].cancel();
    }

    /// Returns the client for the node at the specified index.
    pub fn client(&self, index: usize) -> &Client {
        self.nodes[index].client()
    }

    /// Wait for all nodes to arrive at at least the specified epoch.
    pub async fn wait_for_nodes_to_reach_epoch(&self, epoch: Epoch) {
        let waits: FuturesUnordered<_> = self
            .nodes
            .iter()
            .map(|handle| handle.storage_node().wait_for_epoch(epoch))
            .collect();
        waits.for_each(|_| std::future::ready(())).await;
    }
}

/// Builds a new [`TestCluster`] with custom configuration values.
///
/// Methods can be chained in order to set the configuration values, with the `TestCluster` being
/// constructed by calling [`build`][Self::build`].
///
/// Without further configuration, this will build a test cluster of 4 storage nodes with shards
/// being assigned as {0}, {1, 2}, {3, 4, 5}, {6, 7, 8}, {9, 10, 11, 12} to the nodes.
///
/// See function level documentation for details on the various configuration settings.
#[derive(Debug)]
pub struct TestClusterBuilder {
    storage_node_configs: Vec<StorageNodeTestConfig>,
    system_context: Option<SystemContext>,
    sui_cluster_handle: Option<Arc<TestClusterHandle>>,
    use_distinct_ip: bool,
    // INV: Reset if shard_assignment is changed.
    event_providers: Vec<Option<Box<dyn SystemEventProvider>>>,
    committee_services: Vec<Option<Box<dyn CommitteeService>>>,
    contract_services: Vec<Option<Box<dyn SystemContractService>>>,
    storage_capabilities: Vec<Option<StorageNodeCap>>,
    node_wallet_dirs: Vec<Option<PathBuf>>,
}

impl TestClusterBuilder {
    /// Returns a new default builder.
    pub fn new(use_distinct_ip: bool) -> Self {
        Self {
            storage_node_configs: TestClusterBuilder::default()
                .storage_node_configs
                .into_iter()
                .map(|config| StorageNodeTestConfig::new(config.shards, use_distinct_ip))
                .collect(),
            use_distinct_ip,
            ..Default::default()
        }
    }

    /// Returns a reference to the storage node test configs of the builder.
    pub fn storage_node_test_configs(&self) -> &Vec<StorageNodeTestConfig> {
        &self.storage_node_configs
    }

    /// Sets the number of storage nodes and their shard assignments from a sequence of the shards
    /// assigned to each storage.
    ///
    /// Resets any prior calls to [`Self::with_test_configs`],
    /// [`Self::with_system_event_providers`], and [`Self::with_committee_services`].
    pub fn with_shard_assignment<S, I>(mut self, assignment: &[S]) -> Self
    where
        S: Borrow<[I]>,
        for<'a> &'a I: Into<ShardIndex>,
    {
        let configs =
            storage_node_test_configs_from_shard_assignment(assignment, self.use_distinct_ip);

        self.event_providers = configs.iter().map(|_| None).collect();
        self.committee_services = configs.iter().map(|_| None).collect();
        self.storage_capabilities = configs.iter().map(|_| None).collect();
        self.node_wallet_dirs = configs.iter().map(|_| None).collect();
        self.storage_node_configs = configs;
        self
    }

    /// Sets the configurations for each storage node based on `configs`.
    ///
    /// Resets any prior calls to [`Self::with_shard_assignment`],
    /// [`Self::with_system_event_providers`], [`Self::with_committee_services`],
    /// and [`Self::with_system_contract_services`].
    pub fn with_test_configs(mut self, configs: Vec<StorageNodeTestConfig>) -> Self {
        self.event_providers = configs.iter().map(|_| None).collect();
        self.committee_services = configs.iter().map(|_| None).collect();
        self.contract_services = configs.iter().map(|_| None).collect();
        self.storage_capabilities = configs.iter().map(|_| None).collect();
        self.node_wallet_dirs = configs.iter().map(|_| None).collect();
        self.storage_node_configs = configs;
        self
    }

    /// Clones an event provider to be used with each storage node.
    ///
    /// Should be called after the storage nodes have been specified.
    pub fn with_system_event_providers<T>(mut self, event_provider: T) -> Self
    where
        T: SystemEventProvider + Clone + 'static,
    {
        self.event_providers = self
            .storage_node_configs
            .iter()
            .map(|_| Some(Box::new(event_provider.clone()) as _))
            .collect();
        self
    }

    /// Sets the individual event providers for each storage node.
    /// Requires: `event_providers.len() == storage_node_configs.len()`.
    pub fn with_individual_system_event_providers<T>(mut self, event_providers: &[T]) -> Self
    where
        T: SystemEventProvider + Clone + 'static,
    {
        assert_eq!(event_providers.len(), self.storage_node_configs.len());
        self.event_providers = event_providers
            .iter()
            .map(|provider| Some(Box::new(provider.clone()) as _))
            .collect();
        self
    }

    /// Sets the [`CommitteeService`] used for each storage node.
    ///
    /// Should be called after the storage nodes have been specified.
    pub async fn with_committee_services<F, Fut, T>(mut self, make_service: F) -> Self
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = T>,
        T: CommitteeService + 'static,
    {
        self.committee_services.clear();
        for _ in self.storage_node_configs.iter() {
            self.committee_services
                .push(Some(Box::new(make_service().await) as _));
        }
        self
    }

    /// Sets the [`SystemContractService`] used for each storage node.
    ///
    /// Should be called after the storage nodes have been specified.
    pub fn with_system_contract_services<T>(mut self, contract_services: &[T]) -> Self
    where
        T: SystemContractService + Clone + 'static,
    {
        assert_eq!(contract_services.len(), self.storage_node_configs.len());
        self.contract_services = contract_services
            .iter()
            .map(|service| Some(Box::new(service.clone()) as _))
            .collect();
        self
    }

    /// Sets the system context for the cluster.
    pub fn with_system_context(mut self, system_context: SystemContext) -> Self {
        self.system_context = Some(system_context);
        self
    }

    /// Sets the SUI cluster handle for the cluster.
    pub fn with_sui_cluster_handle(mut self, sui_cluster_handle: Arc<TestClusterHandle>) -> Self {
        self.sui_cluster_handle = Some(sui_cluster_handle);
        self
    }

    /// Sets the storage capabilities for each storage node.
    pub fn with_storage_capabilities(mut self, capabilities: Vec<StorageNodeCap>) -> Self {
        self.storage_capabilities = capabilities.into_iter().map(Some).collect();
        self
    }

    /// Sets the sui wallet config directory for each storage node.
    pub fn with_node_wallet_dirs(mut self, wallet_dirs: Vec<PathBuf>) -> Self {
        self.node_wallet_dirs = wallet_dirs.into_iter().map(Some).collect();
        self
    }

    /// Creates the configured `TestCluster`.
    pub async fn build<T: StorageNodeHandleTrait>(self) -> anyhow::Result<TestCluster<T>> {
        let mut nodes = vec![];

        let committee_members: Vec<_> = self
            .storage_node_configs
            .iter()
            .enumerate()
            .map(|(i, info)| info.to_storage_node_info(&format!("node-{i}")))
            .collect();

        let n_shards = self
            .storage_node_configs
            .iter()
            .map(|config| config.shards.len())
            .sum::<usize>();

        // Create the stub lookup service and handles that may be used if none is provided.
        let mut lookup_service_and_handle = None;

        for (
            ((((config, event_provider), service), contract_service), capability),
            node_wallet_dir,
        ) in self
            .storage_node_configs
            .into_iter()
            .zip(self.event_providers.into_iter())
            .zip(self.committee_services.into_iter())
            .zip(self.contract_services.into_iter())
            .zip(self.storage_capabilities.into_iter())
            .zip(self.node_wallet_dirs.into_iter())
        {
            let local_identity = config.key_pair.public().clone();
            let mut builder = StorageNodeHandle::builder()
                .with_storage(empty_storage_with_shards(&config.shards))
                .with_test_config(config)
                .with_rest_api_started(true)
                .with_node_started(true)
                .with_storage_node_capability(capability)
                .with_node_wallet_dir(node_wallet_dir);

            if let Some(provider) = event_provider {
                builder = builder.with_boxed_system_event_provider(provider);
            }

            builder = if let Some(service) = service {
                builder.with_committee_service(service)
            } else {
                let (lookup_service, _) = lookup_service_and_handle.get_or_insert_with(|| {
                    let committee = committee_from_members(committee_members.clone(), Some(1));
                    let lookup_service = StubLookupService::new(committee.clone());
                    let lookup_service_handle = lookup_service.handle();
                    (lookup_service, lookup_service_handle)
                });

                let service = NodeCommitteeService::builder()
                    .local_identity(local_identity)
                    .node_service_factory(DefaultNodeServiceFactory::avoid_system_services())
                    .build(lookup_service.clone())
                    .await?;
                builder.with_committee_service(Box::new(service))
            };

            if let Some(service) = contract_service {
                builder = builder.with_system_contract_service(service);
            }

            nodes.push(
                T::build_and_run(
                    builder,
                    self.sui_cluster_handle.clone(),
                    self.system_context.clone(),
                    tempfile::tempdir().expect("temporary directory creation must succeed"),
                )
                .await?,
            );
        }

        Ok(TestCluster {
            nodes,
            lookup_service_handle: lookup_service_and_handle.map(|(_, handle)| handle),
            n_shards,
        })
    }
}

/// Configuration for a test cluster storage node.
#[derive(Debug)]
pub struct StorageNodeTestConfig {
    key_pair: ProtocolKeyPair,
    network_key_pair: NetworkKeyPair,
    shards: Vec<ShardIndex>,
    rest_api_address: SocketAddr,
}

impl StorageNodeTestConfig {
    fn new(shards: Vec<ShardIndex>, use_distinct_ip: bool) -> Self {
        Self {
            key_pair: ProtocolKeyPair::generate(),
            network_key_pair: NetworkKeyPair::generate(),
            rest_api_address: unused_socket_address(use_distinct_ip),
            shards,
        }
    }

    /// Creates a `SuiStorageNode` from `self`.
    pub fn to_storage_node_info(&self, name: &str) -> SuiStorageNode {
        SuiStorageNode {
            node_id: ObjectID::random(),
            name: name.into(),
            network_address: NetworkAddress(self.rest_api_address.to_string()),
            public_key: self.key_pair.public().clone(),
            next_epoch_public_key: None,
            network_public_key: self.network_key_pair.public().clone(),
            shard_ids: self.shards.clone(),
        }
    }

    /// Creates `NodeRegistrationParams` from `self`.
    pub fn to_node_registration_params(&self, name: &str) -> NodeRegistrationParams {
        NodeRegistrationParams {
            name: name.into(),
            network_address: NetworkAddress(self.rest_api_address.to_string()),
            public_key: self.key_pair.public().clone(),
            network_public_key: self.network_key_pair.public().clone(),
            commission_rate: 0,
            storage_price: 5,
            write_price: 1,
            node_capacity: 1_000_000_000,
        }
    }
}

fn test_config_from_node_shard_assignment<I>(
    shards: &[I],
    use_distinct_ip: bool,
) -> StorageNodeTestConfig
where
    for<'a> &'a I: Into<ShardIndex>,
{
    let shards: Vec<ShardIndex> = shards.iter().map(|i| i.into()).collect();
    StorageNodeTestConfig::new(shards, use_distinct_ip)
}

/// Returns storage node test configs for the given `shard_assignment`.
pub fn storage_node_test_configs_from_shard_assignment<S, I>(
    shard_assignment: &[S],
    use_distinct_ip: bool,
) -> Vec<StorageNodeTestConfig>
where
    S: Borrow<[I]>,
    for<'a> &'a I: Into<ShardIndex>,
{
    let storage_node_configs: Vec<_> = shard_assignment
        .iter()
        .map(|node_shard_assignment| {
            test_config_from_node_shard_assignment(node_shard_assignment.borrow(), use_distinct_ip)
        })
        .collect();

    assert!(
        !storage_node_configs.is_empty(),
        "assignments for at least 1 node must be specified"
    );

    storage_node_configs
}

impl Default for TestClusterBuilder {
    fn default() -> Self {
        let shard_assignment = vec![
            vec![ShardIndex(0)],
            vec![ShardIndex(1), ShardIndex(2)],
            ShardIndex::range(3..6).collect(),
            ShardIndex::range(6..9).collect(),
            ShardIndex::range(9..13).collect(),
        ];
        Self {
            event_providers: shard_assignment.iter().map(|_| None).collect(),
            committee_services: shard_assignment.iter().map(|_| None).collect(),
            contract_services: shard_assignment.iter().map(|_| None).collect(),
            storage_capabilities: shard_assignment.iter().map(|_| None).collect(),
            node_wallet_dirs: shard_assignment.iter().map(|_| None).collect(),
            storage_node_configs: shard_assignment
                .into_iter()
                .map(|shards| StorageNodeTestConfig::new(shards, false))
                .collect(),
            system_context: None,
            sui_cluster_handle: None,
            use_distinct_ip: false,
        }
    }
}

#[async_trait]
impl<T> SystemContractService for Arc<WithTempDir<T>>
where
    T: SystemContractService,
{
    async fn end_voting(&self) -> Result<(), anyhow::Error> {
        self.as_ref().inner.end_voting().await
    }

    async fn invalidate_blob_id(&self, certificate: &InvalidBlobCertificate) {
        self.as_ref().inner.invalidate_blob_id(certificate).await
    }

    async fn epoch_sync_done(&self, epoch: Epoch) {
        self.as_ref().inner.epoch_sync_done(epoch).await
    }

    async fn get_epoch_and_state(&self) -> Result<(Epoch, EpochState), anyhow::Error> {
        self.as_ref().inner.get_epoch_and_state().await
    }

    async fn fixed_system_parameters(&self) -> Result<FixedSystemParameters, anyhow::Error> {
        self.as_ref().inner.fixed_system_parameters().await
    }

    async fn initiate_epoch_change(&self) -> Result<(), anyhow::Error> {
        self.as_ref().inner.initiate_epoch_change().await
    }
}

/// Returns a test-committee with members with the specified number of shards each.
#[cfg(test)]
#[allow(unused)]
pub(crate) fn test_committee(weights: &[u16]) -> Committee {
    test_committee_with_epoch(weights, 1)
}

#[cfg(test)]
#[allow(unused)]
pub(crate) fn test_committee_with_epoch(weights: &[u16], epoch: Epoch) -> Committee {
    use sui_types::base_types::ObjectID;

    let n_shards: u16 = weights.iter().sum();
    let mut shards = 0..n_shards;

    let members = weights
        .iter()
        .map(|&node_shard_count| SuiStorageNode {
            node_id: ObjectID::random(),
            shard_ids: (&mut shards)
                .take(node_shard_count.into())
                .map(ShardIndex)
                .collect(),
            public_key: ProtocolKeyPair::generate().public().clone(),
            next_epoch_public_key: None,
            network_public_key: NetworkKeyPair::generate().public().clone(),
            name: String::new(),
            network_address: NetworkAddress("host:0".to_owned()),
        })
        .collect();

    Committee::new(members, epoch, NonZeroU16::new(n_shards).unwrap()).unwrap()
}

/// A module for creating a Walrus test cluster.
#[cfg(all(feature = "client", feature = "node"))]
pub mod test_cluster {
    use std::sync::OnceLock;

    use tokio::sync::Mutex;
    use walrus_sui::{
        client::{SuiContractClient, SuiReadClient},
        test_utils::{
            self,
            system_setup::{
                create_and_init_system_for_test,
                end_epoch_zero,
                register_committee_and_stake,
            },
            TestClusterHandle,
            DEFAULT_GAS_BUDGET,
        },
    };

    use super::*;
    use crate::{
        client::{self, ClientCommunicationConfig, Config},
        node::{
            committee::DefaultNodeServiceFactory,
            contract_service::SuiSystemContractService,
            events::EventProcessorConfig,
        },
    };

    /// The weight of each storage node in the test cluster.
    pub const FROST_PER_NODE_WEIGHT: u64 = 1_000_000;

    /// Performs the default setup for the test cluster using StorageNodeHandle as default storage
    /// node handle.
    pub async fn default_setup() -> anyhow::Result<(
        Arc<TestClusterHandle>,
        TestCluster,
        WithTempDir<client::Client<SuiContractClient>>,
    )> {
        default_setup_with_epoch_duration(Duration::from_secs(60 * 60)).await
    }

    /// Performs the default setup with the input epoch duration for the test cluster using
    /// StorageNodeHandle as default storage node handle.
    pub async fn default_setup_with_epoch_duration(
        epoch_duration: Duration,
    ) -> anyhow::Result<(
        Arc<TestClusterHandle>,
        TestCluster,
        WithTempDir<client::Client<SuiContractClient>>,
    )> {
        let node_weights = [1, 2, 3, 3, 4];
        default_setup_with_epoch_duration_generic::<StorageNodeHandle>(
            epoch_duration,
            &node_weights,
            true,
            ClientCommunicationConfig::default_for_test(),
        )
        .await
    }

    /// Performs the default setup with the input epoch duration for the test cluster with the
    /// specified storage node handle.
    pub async fn default_setup_with_epoch_duration_generic<T: StorageNodeHandleTrait>(
        epoch_duration: Duration,
        node_weights: &[u16],
        use_legacy_event_processor: bool,
        communication_config: ClientCommunicationConfig,
    ) -> anyhow::Result<(
        Arc<TestClusterHandle>,
        TestCluster<T>,
        WithTempDir<client::Client<SuiContractClient>>,
    )> {
        #[cfg(not(msim))]
        let sui_cluster = test_utils::using_tokio::global_sui_test_cluster();
        #[cfg(msim)]
        let sui_cluster = test_utils::using_msim::global_sui_test_cluster().await;

        // Get a wallet on the global sui test cluster
        let mut wallet = test_utils::new_wallet_on_sui_test_cluster(sui_cluster.clone()).await?;

        // Specify an empty assignment to ensure that storage nodes are not created with invalid
        // shard assignments.
        let n_shards = NonZeroU16::new(node_weights.iter().sum())
            .expect("sum of non-zero weights is not zero");
        let cluster_builder =
            TestCluster::<T>::builder().with_shard_assignment(&vec![[]; node_weights.len()]);

        // Get the default committee from the test cluster builder
        let (members, protocol_keypairs): (Vec<_>, Vec<_>) = cluster_builder
            .storage_node_test_configs()
            .iter()
            .enumerate()
            .map(|(i, info)| {
                (
                    info.to_node_registration_params(&format!("node-{i}")),
                    info.key_pair.to_owned(),
                )
            })
            .unzip();

        let system_ctx = create_and_init_system_for_test(
            &mut wallet.inner,
            n_shards,
            Duration::from_secs(0),
            epoch_duration,
            None,
        )
        .await?;

        let mut contract_clients = vec![];
        let mut node_wallet_dirs = vec![];
        for _ in members.iter() {
            let client = test_utils::new_wallet_on_sui_test_cluster(sui_cluster.clone())
                .await?
                .and_then_async(|wallet| system_ctx.new_contract_client(wallet, DEFAULT_GAS_BUDGET))
                .await?;
            node_wallet_dirs.push(client.temp_dir.path().to_owned());
            contract_clients.push(client);
        }
        let contract_clients_refs = contract_clients
            .iter()
            .map(|client| &client.inner)
            .collect::<Vec<_>>();

        let amounts_to_stake = node_weights
            .iter()
            .map(|&weight| FROST_PER_NODE_WEIGHT * weight as u64)
            .collect::<Vec<_>>();
        let storage_capabilities = register_committee_and_stake(
            &mut wallet.inner,
            &system_ctx,
            &members,
            &protocol_keypairs,
            &contract_clients_refs,
            1_000_000_000_000,
            &amounts_to_stake,
        )
        .await?;

        end_epoch_zero(contract_clients_refs.first().unwrap()).await?;

        let (node_contract_services, _wallet_dirs): (Vec<_>, Vec<_>) = contract_clients
            .into_iter()
            .map(|client| (client.inner, client.temp_dir))
            .map(|(client, tmp_dir)| {
                (
                    SuiSystemContractService::new(client),
                    // In simtest, storage nodes load sui wallet config from the `tmp_dir`. We need
                    // to keep the directory alive throughout the test.
                    Box::leak(Box::new(tmp_dir)),
                )
            })
            .unzip();

        // Build the walrus cluster
        let sui_read_client = SuiReadClient::new(
            wallet.as_ref().get_client().await?,
            system_ctx.system_object,
            system_ctx.staking_object,
        )
        .await?;

        // Create a contract service for the storage nodes using a wallet in a temp dir
        // The sui test cluster handler can be dropped since we already have one
        // Set up the cluster
        let cluster_builder = cluster_builder
            .with_committee_services(|| async {
                NodeCommitteeService::builder()
                    .node_service_factory(DefaultNodeServiceFactory::avoid_system_services())
                    .build(sui_read_client.clone())
                    .await
                    .expect("service construction must succeed in tests")
            })
            .await
            .with_system_contract_services(&node_contract_services);

        let event_processor_config =
            EventProcessorConfig::new_with_default_pruning_interval(sui_cluster.rpc_url().clone());
        let cluster_builder = if use_legacy_event_processor {
            setup_legacy_event_processors(
                &event_processor_config,
                sui_read_client.clone(),
                cluster_builder,
            )
            .await?
        } else {
            setup_checkpoint_based_event_processors(
                &event_processor_config,
                sui_read_client.clone(),
                cluster_builder,
            )
            .await?
        };

        let cluster_builder = cluster_builder
            .with_system_context(system_ctx.clone())
            .with_sui_cluster_handle(sui_cluster.clone())
            .with_storage_capabilities(storage_capabilities)
            .with_node_wallet_dirs(node_wallet_dirs);

        let cluster = {
            // Lock to avoid race conditions.
            let _lock = global_test_lock().lock().await;
            cluster_builder.build().await?
        };

        // Create the client with the admin wallet to ensure that we have some WAL.
        let sui_contract_client = wallet.and_then(|wallet| {
            SuiContractClient::new_with_read_client(wallet, DEFAULT_GAS_BUDGET, sui_read_client)
        })?;
        let config = Config {
            system_object: system_ctx.system_object,
            staking_object: system_ctx.staking_object,
            exchange_object: None,
            wallet_config: None,
            communication_config,
        };

        let client = sui_contract_client
            .and_then_async(|contract_client| {
                client::Client::new_contract_client(config, contract_client)
            })
            .await?;

        Ok((sui_cluster, cluster, client))
    }

    async fn setup_checkpoint_based_event_processors(
        event_processor_config: &EventProcessorConfig,
        sui_read_client: SuiReadClient,
        test_cluster_builder: TestClusterBuilder,
    ) -> anyhow::Result<TestClusterBuilder> {
        let mut event_processors = vec![];
        for _ in test_cluster_builder.storage_node_test_configs().iter() {
            let event_processor = EventProcessor::new(
                event_processor_config,
                event_processor_config.rest_url.clone(),
                sui_read_client.get_system_package_id(),
                Duration::from_millis(100),
                tempfile::tempdir()
                    .expect("temporary directory creation must succeed")
                    .path(),
                &Registry::default(),
            )
            .await?;
            event_processors.push(event_processor.clone());
        }
        let res = test_cluster_builder.with_individual_system_event_providers(&event_processors);
        Ok(res)
    }

    async fn setup_legacy_event_processors(
        _event_processor_config: &EventProcessorConfig,
        sui_read_client: SuiReadClient,
        test_cluster_builder: TestClusterBuilder,
    ) -> anyhow::Result<TestClusterBuilder> {
        let event_provider = crate::node::system_events::SuiSystemEventProvider::new(
            sui_read_client.clone(),
            Duration::from_millis(100),
        );
        let res = test_cluster_builder.with_system_event_providers(event_provider);
        Ok(res)
    }

    // Prevent tests running simultaneously to avoid interferences or race conditions.
    fn global_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(Mutex::default)
    }
}

/// Creates a new [`StorageNodeConfig`] object for testing.
pub fn storage_node_config() -> WithTempDir<StorageNodeConfig> {
    let temp_dir = TempDir::new().expect("able to create a temporary directory");
    let rest_api_address = unused_socket_address(false);
    WithTempDir {
        inner: StorageNodeConfig {
            name: Some("node".to_string()),
            protocol_key_pair: walrus_core::test_utils::protocol_key_pair().into(),
            network_key_pair: walrus_core::test_utils::network_key_pair().into(),
            rest_api_address,
            metrics_address: unused_socket_address(false),
            storage_path: temp_dir.path().to_path_buf(),
            db_config: None,
            sui: None,
            blob_recovery: Default::default(),
            tls: Default::default(),
            rest_graceful_shutdown_period_secs: Some(Some(0)),
            shard_sync_config: Default::default(),
            event_provider_config: EventProviderConfig::LegacyEventProvider,
            commission_rate: 0,
            voting_params: VotingParams {
                storage_price: 5,
                write_price: 1,
                node_capacity: 1_000_000_000,
            },
            public_host: Some(rest_api_address.ip().to_string()),
            public_port: Some(rest_api_address.port()),
            metrics_push: None,
        },
        temp_dir,
    }
}

async fn wait_for_event_processor_to_start(
    event_processor: EventProcessor,
    client: sui_rest_api::Client,
) -> anyhow::Result<()> {
    // Wait until event processor is actually running and downloaded a few checkpoints
    tokio::time::sleep(Duration::from_secs(5)).await;
    let checkpoint = client.get_latest_checkpoint().await?;
    while let Some(event_processor_checkpoint) = event_processor.checkpoint_store.get(&())? {
        if event_processor_checkpoint.inner().sequence_number >= checkpoint.sequence_number {
            break;
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
    Ok(())
}

/// Returns an empty storage, with the column families for the specified shards already created.
pub fn empty_storage_with_shards(shards: &[ShardIndex]) -> WithTempDir<Storage> {
    let temp_dir = tempfile::tempdir().expect("temporary directory creation must succeed");
    let db_config = DatabaseConfig::default();
    let storage = Storage::open(temp_dir.path(), db_config, MetricConf::default())
        .expect("storage creation must succeed");

    for shard in shards {
        // TODO: call create_storage_for_shards once use the list of shards.
        storage
            .create_storage_for_shards(&[*shard])
            .expect("shard should be successfully created");
    }

    WithTempDir {
        inner: storage,
        temp_dir,
    }
}

fn committee_from_members(members: Vec<SuiStorageNode>, initial_epoch: Option<Epoch>) -> Committee {
    let n_shards = NonZeroU16::new(members.iter().map(|node| node.shard_ids.len() as u16).sum())
        .expect("committee cannot have zero shards");
    Committee::new(members, initial_epoch.unwrap_or(1), n_shards)
        .expect("valid members to be provided for tests")
}

#[cfg(test)]
mod test {
    use std::collections::HashSet;

    use super::unused_socket_address;

    #[test]
    #[ignore = "ignore to not bind sockets unnecessarily"]
    fn test_unused_socket_addr() {
        let n = 1000;
        assert_eq!(
            (0..n)
                .map(|_| unused_socket_address(false))
                .collect::<HashSet<_>>()
                .len(),
            n
        )
    }
}
