// Copyright (c) The Libra Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Remotely authenticated vs. unauthenticated network end-points:
//! ---------------------------------------------------
//! A network end-point operates with remote authentication if it only accepts connections
//! from a known set of peers (`trusted_peers`) identified by their network identity keys.
//! This does not mean that the other end-point of a connection also needs to operate with
//! authentication -- a network end-point running with remote authentication enabled will
//! connect to or accept connections from an end-point running in authenticated mode as
//! long as the latter is in its trusted peers set.
use crate::{
    common::NetworkPublicKeys,
    connectivity_manager::{ConnectivityManager, ConnectivityRequest},
    counters,
    peer_manager::{
        conn_notifs_channel, ConnectionRequest, ConnectionRequestSender, PeerManager,
        PeerManagerNotification, PeerManagerRequest, PeerManagerRequestSender,
    },
    protocols::{
        discovery::{self, Discovery},
        health_checker::{self, HealthChecker},
        wire::handshake::v1::SupportedProtocols,
    },
    transport,
    transport::*,
    ProtocolId,
};
use channel::{self, libra_channel, message_queues::QueueStyle};
use futures::stream::StreamExt;
use libra_config::config::{RoleType, HANDSHAKE_VERSION};
use libra_crypto::x25519;
use libra_logger::prelude::*;
use libra_metrics::IntCounterVec;
use libra_network_address::NetworkAddress;
use libra_types::PeerId;
use netcore::transport::Transport;
use std::{
    clone::Clone,
    collections::HashMap,
    num::NonZeroUsize,
    sync::{Arc, RwLock},
    time::Duration,
};
use tokio::{runtime::Handle, time::interval};
use tokio_retry::strategy::ExponentialBackoff;

// NB: Almost all of these values are educated guesses, and not determined using any empirical
// data. If you run into a limit and believe that it is unreasonably tight, please submit a PR
// with your use-case. If you do change a value, please add a comment linking to the PR which
// advocated the change.
pub const NETWORK_CHANNEL_SIZE: usize = 1024;
pub const DISCOVERY_INTERVAL_MS: u64 = 1000;
pub const PING_INTERVAL_MS: u64 = 1000;
pub const PING_TIMEOUT_MS: u64 = 10_000;
pub const DISOVERY_MSG_TIMEOUT_MS: u64 = 10_000;
pub const CONNECTIVITY_CHECK_INTERNAL_MS: u64 = 5000;
pub const INBOUND_RPC_TIMEOUT_MS: u64 = 10_000;
pub const MAX_CONCURRENT_OUTBOUND_RPCS: u32 = 100;
pub const MAX_CONCURRENT_INBOUND_RPCS: u32 = 100;
pub const PING_FAILURES_TOLERATED: u64 = 10;
pub const MAX_CONCURRENT_NETWORK_REQS: usize = 100;
pub const MAX_CONCURRENT_NETWORK_NOTIFS: usize = 100;
pub const MAX_CONNECTION_DELAY_MS: u64 = 10 * 60 * 1000 /* 10 minutes */;

#[derive(Debug)]
pub enum AuthenticationMode {
    /// Inbound and outbound connections are unauthenticated and unencrypted, using
    /// a simple PeerId exchange protocol to share each peer's PeerId in-band.
    /// This mode is obviously insecure and should never be used in production.
    // TODO(philiphayes): remove? gate with cfg(test)? should never use this in prod.
    Unauthenticated,
    /// Inbound and outbound connections are secured with NoiseIK; however, only
    /// clients/dialers will authenticate the servers/listeners. More specifically,
    /// dialers will pin the connection to a specific, expected pubkey while
    /// listeners will accept any inbound dialer's pubkey.
    ServerOnly(x25519::PrivateKey),
    /// Inbound and outbound connections are secured with NoiseIK. Both dialer and
    /// listener will only accept connections that successfully authenticate to a
    /// pubkey in their "trusted peers" set.
    Mutual(x25519::PrivateKey),
}

/// Append the correct libranet protocols to the given base address, depending on
/// the configured authentication mode.
fn append_libranet_protocols(
    addr: NetworkAddress,
    auth_mode: &AuthenticationMode,
) -> NetworkAddress {
    match auth_mode {
        AuthenticationMode::Unauthenticated => addr.append_test_protos(HANDSHAKE_VERSION),
        AuthenticationMode::ServerOnly(key) | AuthenticationMode::Mutual(key) => {
            addr.append_prod_protos(key.public_key(), HANDSHAKE_VERSION)
        }
    }
}

/// Build Network module with custom configuration values.
/// Methods can be chained in order to set the configuration values.
/// MempoolNetworkHandler and ConsensusNetworkHandler are constructed by calling
/// [`NetworkBuilder::build`].  New instances of `NetworkBuilder` are obtained
/// via [`NetworkBuilder::new`].
// TODO(philiphayes): refactor NetworkBuilder and libra-node; current config is
// pretty tangled.
pub struct NetworkBuilder {
    executor: Handle,
    peer_id: PeerId,
    role: RoleType,
    // TODO(philiphayes): better support multiple listening addrs
    listen_address: NetworkAddress,
    advertised_address: Option<NetworkAddress>,
    seed_peers: HashMap<PeerId, Vec<NetworkAddress>>,
    trusted_peers: Arc<RwLock<HashMap<PeerId, NetworkPublicKeys>>>,
    authentication_mode: Option<AuthenticationMode>,
    channel_size: usize,
    direct_send_protocols: Vec<ProtocolId>,
    rpc_protocols: Vec<ProtocolId>,
    discovery_interval_ms: u64,
    ping_interval_ms: u64,
    ping_timeout_ms: u64,
    ping_failures_tolerated: u64,
    upstream_handlers:
        HashMap<ProtocolId, libra_channel::Sender<(PeerId, ProtocolId), PeerManagerNotification>>,
    connection_event_handlers: Vec<conn_notifs_channel::Sender>,
    pm_reqs_tx: libra_channel::Sender<(PeerId, ProtocolId), PeerManagerRequest>,
    pm_reqs_rx: libra_channel::Receiver<(PeerId, ProtocolId), PeerManagerRequest>,
    connection_reqs_tx: libra_channel::Sender<PeerId, ConnectionRequest>,
    connection_reqs_rx: libra_channel::Receiver<PeerId, ConnectionRequest>,
    conn_mgr_reqs_tx: Option<channel::Sender<ConnectivityRequest>>,
    connectivity_check_interval_ms: u64,
    inbound_rpc_timeout_ms: u64,
    max_concurrent_outbound_rpcs: u32,
    max_concurrent_inbound_rpcs: u32,
    max_concurrent_network_reqs: usize,
    max_concurrent_network_notifs: usize,
    max_connection_delay_ms: u64,
}

impl NetworkBuilder {
    /// Return a new NetworkBuilder initialized with default configuration values.
    pub fn new(
        executor: Handle,
        peer_id: PeerId,
        role: RoleType,
        listen_address: NetworkAddress,
    ) -> NetworkBuilder {
        // Setup channel to send requests to peer manager.
        let (pm_reqs_tx, pm_reqs_rx) = libra_channel::new(
            QueueStyle::FIFO,
            NonZeroUsize::new(NETWORK_CHANNEL_SIZE).unwrap(),
            Some(&counters::PENDING_PEER_MANAGER_REQUESTS),
        );
        // Setup channel to send connection requests to peer manager.
        let (connection_reqs_tx, connection_reqs_rx) = libra_channel::new(
            QueueStyle::FIFO,
            NonZeroUsize::new(NETWORK_CHANNEL_SIZE).unwrap(),
            None,
        );
        NetworkBuilder {
            executor,
            peer_id,
            role,
            listen_address,
            advertised_address: None,
            seed_peers: HashMap::new(),
            trusted_peers: Arc::new(RwLock::new(HashMap::new())),
            authentication_mode: None,
            channel_size: NETWORK_CHANNEL_SIZE,
            direct_send_protocols: vec![],
            rpc_protocols: vec![],
            upstream_handlers: HashMap::new(),
            connection_event_handlers: Vec::new(),
            pm_reqs_tx,
            pm_reqs_rx,
            connection_reqs_tx,
            connection_reqs_rx,
            conn_mgr_reqs_tx: None,
            discovery_interval_ms: DISCOVERY_INTERVAL_MS,
            ping_interval_ms: PING_INTERVAL_MS,
            ping_timeout_ms: PING_TIMEOUT_MS,
            ping_failures_tolerated: PING_FAILURES_TOLERATED,
            connectivity_check_interval_ms: CONNECTIVITY_CHECK_INTERNAL_MS,
            inbound_rpc_timeout_ms: INBOUND_RPC_TIMEOUT_MS,
            max_concurrent_outbound_rpcs: MAX_CONCURRENT_OUTBOUND_RPCS,
            max_concurrent_inbound_rpcs: MAX_CONCURRENT_INBOUND_RPCS,
            max_concurrent_network_reqs: MAX_CONCURRENT_NETWORK_REQS,
            max_concurrent_network_notifs: MAX_CONCURRENT_NETWORK_NOTIFS,
            max_connection_delay_ms: MAX_CONNECTION_DELAY_MS,
        }
    }

    /// Set network authentication mode.
    pub fn authentication_mode(&mut self, authentication_mode: AuthenticationMode) -> &mut Self {
        self.authentication_mode = Some(authentication_mode);
        self
    }

    /// Set an address to advertise, if different from the listen address
    pub fn advertised_address(&mut self, advertised_address: NetworkAddress) -> &mut Self {
        self.advertised_address = Some(advertised_address);
        self
    }

    /// Set trusted peers.
    pub fn trusted_peers(
        &mut self,
        trusted_peers: HashMap<PeerId, NetworkPublicKeys>,
    ) -> &mut Self {
        *self.trusted_peers.write().unwrap() = trusted_peers;
        self
    }

    /// Set seed peers to bootstrap discovery
    pub fn seed_peers(&mut self, seed_peers: HashMap<PeerId, Vec<NetworkAddress>>) -> &mut Self {
        self.seed_peers = seed_peers;
        self
    }

    /// Set discovery ticker interval
    pub fn discovery_interval_ms(&mut self, discovery_interval_ms: u64) -> &mut Self {
        self.discovery_interval_ms = discovery_interval_ms;
        self
    }

    /// Set ping interval.
    pub fn ping_interval_ms(&mut self, ping_interval_ms: u64) -> &mut Self {
        self.ping_interval_ms = ping_interval_ms;
        self
    }

    /// Set number of ping failures tolerated.
    pub fn ping_failures_tolerated(&mut self, ping_failures_tolerated: u64) -> &mut Self {
        self.ping_failures_tolerated = ping_failures_tolerated;
        self
    }

    /// Set ping timeout.
    pub fn ping_timeout_ms(&mut self, ping_timeout_ms: u64) -> &mut Self {
        self.ping_timeout_ms = ping_timeout_ms;
        self
    }

    /// Set connectivity check ticker interval
    pub fn connectivity_check_interval_ms(
        &mut self,
        connectivity_check_interval_ms: u64,
    ) -> &mut Self {
        self.connectivity_check_interval_ms = connectivity_check_interval_ms;
        self
    }

    /// Set inbound rpc timeout.
    pub fn inbound_rpc_timeout_ms(&mut self, inbound_rpc_timeout_ms: u64) -> &mut Self {
        self.inbound_rpc_timeout_ms = inbound_rpc_timeout_ms;
        self
    }

    /// The maximum number of concurrent outbound rpc requests we will service.
    pub fn max_concurrent_outbound_rpcs(&mut self, max_concurrent_outbound_rpcs: u32) -> &mut Self {
        self.max_concurrent_outbound_rpcs = max_concurrent_outbound_rpcs;
        self
    }

    /// The maximum number of concurrent inbound rpc requests we will service.
    pub fn max_concurrent_inbound_rpcs(&mut self, max_concurrent_inbound_rpcs: u32) -> &mut Self {
        self.max_concurrent_inbound_rpcs = max_concurrent_inbound_rpcs;
        self
    }

    /// The maximum number of concurrent NetworkRequests we will service in NetworkProvider.
    pub fn max_concurrent_network_reqs(&mut self, max_concurrent_network_reqs: usize) -> &mut Self {
        self.max_concurrent_network_reqs = max_concurrent_network_reqs;
        self
    }

    /// The maximum number of concurrent Notifications from each actor we will service in
    /// NetworkProvider.
    pub fn max_concurrent_network_notifs(
        &mut self,
        max_concurrent_network_notifs: usize,
    ) -> &mut Self {
        self.max_concurrent_network_notifs = max_concurrent_network_notifs;
        self
    }

    /// The maximum duration (in milliseconds) we should wait before dialing a peer we should
    /// connect to.
    pub fn max_connection_delay_ms(&mut self, max_connection_delay_ms: u64) -> &mut Self {
        self.max_connection_delay_ms = max_connection_delay_ms;
        self
    }

    /// Set the size of the channels between different network actors.
    pub fn channel_size(&mut self, channel_size: usize) -> &mut Self {
        self.channel_size = channel_size;
        self
    }

    pub fn conn_mgr_reqs_tx(&self) -> Option<channel::Sender<ConnectivityRequest>> {
        self.conn_mgr_reqs_tx.clone()
    }

    fn supported_protocols(&self) -> SupportedProtocols {
        self.direct_send_protocols
            .iter()
            .chain(&self.rpc_protocols)
            .into()
    }

    /// Add a handler for given protocols using raw bytes.
    pub fn add_protocol_handler(
        &mut self,
        rpc_protocols: Vec<ProtocolId>,
        direct_send_protocols: Vec<ProtocolId>,
        queue_preference: QueueStyle,
        max_queue_size_per_peer: usize,
        counter: Option<&'static IntCounterVec>,
    ) -> (
        PeerManagerRequestSender,
        libra_channel::Receiver<(PeerId, ProtocolId), PeerManagerNotification>,
        ConnectionRequestSender,
        conn_notifs_channel::Receiver,
    ) {
        self.direct_send_protocols
            .extend(direct_send_protocols.clone());
        self.rpc_protocols.extend(rpc_protocols.clone());
        let (network_notifs_tx, network_notifs_rx) = libra_channel::new(
            queue_preference,
            NonZeroUsize::new(max_queue_size_per_peer).unwrap(),
            counter,
        );
        for protocol in rpc_protocols
            .iter()
            .chain(direct_send_protocols.iter())
            .cloned()
        {
            self.upstream_handlers
                .insert(protocol, network_notifs_tx.clone());
        }
        let (connection_notifs_tx, connection_notifs_rx) = conn_notifs_channel::new();
        // Auto-subscribe all application level handlers to connection events.
        self.connection_event_handlers.push(connection_notifs_tx);
        (
            PeerManagerRequestSender::new(self.pm_reqs_tx.clone()),
            network_notifs_rx,
            ConnectionRequestSender::new(self.connection_reqs_tx.clone()),
            connection_notifs_rx,
        )
    }

    pub fn add_connection_event_listener(&mut self) -> conn_notifs_channel::Receiver {
        let (tx, rx) = conn_notifs_channel::new();
        self.connection_event_handlers.push(tx);
        rx
    }

    /// Add a [`ConnectivityManager`] to the network.
    ///
    /// [`ConnectivityManager`] is responsible for ensuring that we are connected
    /// to a node iff. it is an eligible node and maintaining persistent
    /// connections with all eligible nodes. A list of eligible nodes is received
    /// at initialization, and updates are received on changes to system membership.
    ///
    /// Note: a connectivity manager should only be added if the network is
    /// permissioned.
    pub fn add_connectivity_manager(&mut self) -> &mut Self {
        let (conn_mgr_reqs_tx, conn_mgr_reqs_rx) = channel::new(
            self.channel_size,
            &counters::PENDING_CONNECTIVITY_MANAGER_REQUESTS,
        );
        self.conn_mgr_reqs_tx = Some(conn_mgr_reqs_tx);
        let peer_id = self.peer_id;
        let trusted_peers = self.trusted_peers.clone();
        let seed_peers = self.seed_peers.clone();
        let max_connection_delay_ms = self.max_connection_delay_ms;
        let connectivity_check_interval_ms = self.connectivity_check_interval_ms;
        let pm_conn_mgr_notifs_rx = self.add_connection_event_listener();
        let conn_mgr = self.executor.enter(|| {
            ConnectivityManager::new(
                peer_id,
                trusted_peers,
                seed_peers,
                interval(Duration::from_millis(connectivity_check_interval_ms)).fuse(),
                ConnectionRequestSender::new(self.connection_reqs_tx.clone()),
                pm_conn_mgr_notifs_rx,
                conn_mgr_reqs_rx,
                ExponentialBackoff::from_millis(2).factor(1000),
                max_connection_delay_ms,
            )
        });
        self.executor.spawn(conn_mgr.start());
        self
    }

    /// Add the (gossip) [`Discovery`] protocol to the network.
    ///
    /// (gossip) [`Discovery`] discovers other eligible peers' network addresses
    /// by exchanging the full set of known peer network addresses with connected
    /// peers as a network protocol.
    ///
    /// This is for testing purposes only and should not be used in production networks.
    pub fn add_gossip_discovery(&mut self) -> &mut Self {
        let peer_id = self.peer_id;
        let conn_mgr_reqs_tx = self
            .conn_mgr_reqs_tx()
            .expect("ConnectivityManager not enabled");
        // Get handles for network events and sender.
        let (discovery_network_tx, discovery_network_rx) = discovery::add_to_network(self);

        // TODO(philiphayes): the current setup for gossip discovery doesn't work
        // when we don't have an `advertised_address` set, since it uses the
        // `listen_address`, which might not be bound to a port yet. For example,
        // if our `listen_address` is "/ip6/::1/tcp/0" and `advertised_address` is
        // `None`, then this will set our `advertised_address` to something like
        // "/ip6/::1/tcp/0/ln-noise-ik/<pubkey>/ln-handshake/0", which is wrong
        // since the actual bound port will be something > 0.

        // TODO(philiphayes): in network_builder setup, only bind the channels.
        // wait until PeerManager is running to actual setup gossip discovery.

        let advertised_address = self
            .advertised_address
            .clone()
            .unwrap_or_else(|| self.listen_address.clone());
        let authentication_mode = self
            .authentication_mode
            .as_ref()
            .expect("Authentication Mode not set");
        let advertised_address = append_libranet_protocols(advertised_address, authentication_mode);

        let addrs = vec![advertised_address];
        let role = self.role;
        let discovery_interval_ms = self.discovery_interval_ms;
        let discovery = self.executor.enter(|| {
            Discovery::new(
                peer_id,
                role,
                addrs,
                interval(Duration::from_millis(discovery_interval_ms)).fuse(),
                discovery_network_tx,
                discovery_network_rx,
                conn_mgr_reqs_tx,
            )
        });
        self.executor.spawn(discovery.start());
        debug!("Started discovery protocol actor");
        self
    }

    pub fn add_connection_monitoring(&mut self) -> &mut Self {
        // Initialize and start HealthChecker.
        let (hc_network_tx, hc_network_rx) = health_checker::add_to_network(self);
        let ping_interval_ms = self.ping_interval_ms;
        let ping_timeout_ms = self.ping_timeout_ms;
        let ping_failures_tolerated = self.ping_failures_tolerated;
        let health_checker = self.executor.enter(|| {
            HealthChecker::new(
                interval(Duration::from_millis(ping_interval_ms)).fuse(),
                hc_network_tx,
                hc_network_rx,
                Duration::from_millis(ping_timeout_ms),
                ping_failures_tolerated,
            )
        });
        self.executor.spawn(health_checker.start());
        debug!("Started health checker");
        self
    }

    /// Create the configured transport and start PeerManager.
    /// Return the actual NetworkAddress over which this peer is listening.
    pub fn build(mut self) -> NetworkAddress {
        use libra_network_address::Protocol::*;

        let peer_id = self.peer_id;
        let protos = self.supported_protocols();

        let trusted_peers = self.trusted_peers.clone();
        let authentication_mode = self
            .authentication_mode
            .take()
            .expect("Authentication Mode not set");

        // formatting the listen address here is a temporary hack until `transport.rs`
        // gets refactored and returns the fully rendered listen_addr in
        // `Transport::listen_on()`.

        // would like to do this in `build_with_transport` but ownership is very
        // tricky here since the private keys aren't cloneable and we don't have
        // a safe key container yet...
        let unbound_listen_addr = self.listen_address.clone();
        let unbound_len = unbound_listen_addr.len();
        let unbound_listen_addr =
            append_libranet_protocols(unbound_listen_addr, &authentication_mode);

        let bound_listen_addr = match self.listen_address.as_slice() {
            [Ip4(_), Tcp(_)] | [Ip6(_), Tcp(_)] => match authentication_mode {
                AuthenticationMode::Unauthenticated => {
                    self.build_with_transport(build_tcp_transport(peer_id, protos))
                }
                AuthenticationMode::ServerOnly(key) => self
                    .build_with_transport(build_unauthenticated_tcp_noise_transport(key, protos)),
                AuthenticationMode::Mutual(key) => {
                    self.build_with_transport(build_tcp_noise_transport(key, trusted_peers, protos))
                }
            },
            [Memory(_)] => match authentication_mode {
                AuthenticationMode::Unauthenticated => {
                    self.build_with_transport(build_memory_transport(peer_id, protos))
                }
                AuthenticationMode::ServerOnly(key) => self.build_with_transport(
                    build_unauthenticated_memory_noise_transport(key, protos),
                ),
                AuthenticationMode::Mutual(key) => self
                    .build_with_transport(build_memory_noise_transport(key, trusted_peers, protos)),
            },
            _ => panic!(
                "Unsupported listen_address: '{}', expected '/memory/<port>', \
                 '/ip4/<addr>/tcp/<port>', or '/ip6/<addr>/tcp/<port>'.",
                self.listen_address
            ),
        };

        // do some surgery here...
        let libranet_protos = &unbound_listen_addr.as_slice()[unbound_len..];
        bound_listen_addr.extend_from_slice(libranet_protos)
    }

    /// Given a transport build and launch PeerManager.
    /// Return the actual NetworkAddress over which this peer is listening.
    fn build_with_transport<TTransport, TSocket>(self, transport: TTransport) -> NetworkAddress
    where
        TTransport: Transport<Output = Connection<TSocket>> + Send + 'static,
        TSocket: transport::TSocket,
    {
        let peer_mgr = PeerManager::new(
            self.executor.clone(),
            transport,
            self.peer_id,
            self.role,
            // TODO(philiphayes): peer manager should take `Vec<NetworkAddress>`
            // (which could be empty, like in client use case)
            self.listen_address,
            self.pm_reqs_rx,
            self.connection_reqs_rx,
            self.upstream_handlers,
            self.connection_event_handlers,
            self.max_concurrent_network_reqs,
            self.max_concurrent_network_notifs,
            self.channel_size,
        );
        let listen_addr = peer_mgr.listen_addr().clone();

        self.executor.spawn(peer_mgr.start());
        debug!("Started peer manager");

        listen_addr
    }
}
