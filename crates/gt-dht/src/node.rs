use crate::store::BoundedStore;
use gt_core::protocol::{ComputeOfferMessage, RepoAnnouncement, RepoInfoResponse};
use gt_core::sanitize::sanitize_log;
use libp2p::futures::StreamExt;
use libp2p::identify;
use libp2p::identity::Keypair;
use libp2p::kad::{self};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p::swarm::NetworkBehaviour;
use libp2p::{mdns, noise, ping, tcp, yamux, Multiaddr, PeerId as Libp2pPeerId};
use std::error::Error;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

/// Commands the daemon can send to the DHT node.
#[derive(Debug)]
pub enum DhtCommand {
    /// Announce we're hosting a repo.
    Announce(RepoAnnouncement),
    /// Look up a repo by its address hash.
    Lookup { address: [u8; 32] },
    /// Get our count of connected peers.
    GetPeers {
        reply: tokio::sync::oneshot::Sender<usize>,
    },
    /// Announce available compute resources.
    AnnounceCompute(ComputeOfferMessage),
    /// Look up compute providers.
    LookupCompute { address: [u8; 32] },
    /// Withdraw our compute offer.
    WithdrawCompute { address: [u8; 32] },
    /// Announce we're seeding a blob.
    AnnounceBlob { address: [u8; 32], data: Vec<u8> },
    /// Look up a blob by its DHT address.
    LookupBlob { address: [u8; 32] },
    /// Announce a collection manifest.
    AnnounceCollection { address: [u8; 32], data: Vec<u8> },
    /// Look up a collection by its DHT address.
    LookupCollection { address: [u8; 32] },
    /// Connect to a specific peer by multiaddr.
    ConnectPeer { addr: Multiaddr },
    /// Shutdown the DHT node.
    Shutdown,
}

/// Events the DHT node sends back to the daemon.
#[derive(Debug)]
pub enum DhtEvent {
    /// Found info about a repo.
    RepoFound(RepoInfoResponse),
    /// Repo not found on the network.
    RepoNotFound { address: [u8; 32] },
    /// Peer discovered.
    PeerDiscovered {
        peer_id: String,
        addresses: Vec<Multiaddr>,
        /// gitforest peer ID (64-char hex public key), if known from identify.
        gitforest_peer_id: Option<String>,
        /// Peer's HTTP API port, when advertised via the identify agent version.
        api_port: Option<u16>,
    },
    /// Node is ready and listening.
    Ready { listen_addrs: Vec<Multiaddr> },
    /// Found compute offers.
    ComputeOfferFound(ComputeOfferMessage),
    /// Found a blob announcement.
    BlobFound { address: [u8; 32], data: Vec<u8> },
    /// Blob not found on the network.
    BlobNotFound { address: [u8; 32] },
    /// Found a collection manifest.
    CollectionFound { address: [u8; 32], data: Vec<u8> },
}

/// What kind of record an outbound Kademlia `get_record` query is after.
/// The query id is tracked so a found record is decoded as the type the
/// caller asked for instead of guessing from the bytes.
#[derive(Clone, Copy, Debug)]
enum PendingKind {
    Repo,
    Compute,
    Blob,
    Collection,
}

/// Cap on inbound connections that are accepted but not yet negotiated.
/// Handshakes normally complete in milliseconds; anything parked beyond
/// this is a slowloris-style flood, so the excess is refused outright.
pub const MAX_PENDING_INCOMING: u32 = 32;
/// Cap on fully established inbound connections per node. Legitimate
/// peers hold one connection each; floods die here.
pub const MAX_ESTABLISHED_INCOMING: u32 = 128;
/// Cap on established connections to any single peer. Concurrent dials
/// can legitimately race into two; more is a bug or an attack.
pub const MAX_ESTABLISHED_PER_PEER: u32 = 3;

/// Combined network behaviour for the DHT node.
#[derive(NetworkBehaviour)]
struct GitforestBehaviour {
    identify: identify::Behaviour,
    kademlia: kad::Behaviour<BoundedStore>,
    mdns: Toggle<mdns::tokio::Behaviour>,
    limits: libp2p_connection_limits::Behaviour,
    ping: ping::Behaviour,
}

/// Parse a peer's HTTP API port from its identify agent version.
///
/// Our nodes advertise `gitforest/1.0.0 api/{port}`. Anything else, including
/// a missing or malformed token, yields `None` and the caller falls back to
/// the default API port. Port 0 is rejected as undialable.
/// Remote address worth recording from a new connection, if any. We only
/// trust the address when we placed the dial: inbound remote addresses are
/// the dialer's ephemeral source ports and can never be dialed back.
fn recordable_connection_addr(endpoint: &libp2p::core::ConnectedPoint) -> Option<Multiaddr> {
    endpoint
        .is_dialer()
        .then(|| endpoint.get_remote_address().clone())
}

fn multiaddr_is_loopback(addr: &Multiaddr) -> bool {
    addr.iter().any(|proto| match proto {
        libp2p::core::multiaddr::Protocol::Ip4(ip) => ip.is_loopback(),
        libp2p::core::multiaddr::Protocol::Ip6(ip) => ip.is_loopback(),
        _ => false,
    })
}

pub fn panic_payload_str(payload: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    if let Some(s) = payload.downcast_ref::<&str>() {
        return s.to_string();
    }
    String::from("non-string panic payload")
}

fn parse_agent_api_port(agent: &str) -> Option<u16> {
    for token in agent.split_whitespace() {
        let Some(digits) = token.strip_prefix("api/") else {
            continue;
        };
        if digits.is_empty() || digits.len() > 5 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            continue;
        }
        let Ok(port) = digits.parse::<u16>() else {
            continue;
        };
        if port == 0 {
            continue;
        }
        return Some(port);
    }
    None
}

/// A cloneable handle to a DHT node that can only send commands.
/// Useful when one task owns `recv()` and other tasks need to issue commands.
#[derive(Clone)]
pub struct DhtNodeSender {
    cmd_tx: mpsc::Sender<DhtCommand>,
}

/// The DHT node that participates in the gitforest network.
pub struct DhtNode {
    cmd_tx: mpsc::Sender<DhtCommand>,
    event_rx: mpsc::Receiver<DhtEvent>,
}

impl DhtNode {
    /// Send a command to the underlying DHT event loop.
    #[allow(clippy::result_large_err)]
    pub async fn send(&self, cmd: DhtCommand) -> Result<(), mpsc::error::SendError<DhtCommand>> {
        self.cmd_tx.send(cmd).await
    }

    /// Return a cloneable handle that can only send commands to this node.
    /// Useful when one task owns `recv()` and other tasks need to issue commands.
    pub fn clone_sender(&self) -> DhtNodeSender {
        DhtNodeSender {
            cmd_tx: self.cmd_tx.clone(),
        }
    }

    /// Start a new DHT node on the given port, using the provided identity.
    /// The libp2p keypair is derived from the same Ed25519 seed so the DHT
    /// identity is stable across restarts and matches our gitforest peer ID.
    pub async fn start(
        port: u16,
        api_port: u16,
        identity: &gt_core::identity::Identity,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        Self::start_on(
            std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED),
            port,
            api_port,
            identity,
            true,
        )
        .await
    }

    pub async fn start_on(
        listen: std::net::IpAddr,
        port: u16,
        api_port: u16,
        identity: &gt_core::identity::Identity,
        enable_mdns: bool,
    ) -> Result<Self, Box<dyn Error + Send + Sync>> {
        let (cmd_tx, mut cmd_rx) = mpsc::channel::<DhtCommand>(256);
        let (event_tx, event_rx) = mpsc::channel::<DhtEvent>(256);

        let secret = identity.secret_bytes();
        let local_key =
            Keypair::ed25519_from_bytes(secret).map_err(|e| format!("invalid key: {e}"))?;
        let local_peer_id = Libp2pPeerId::from(local_key.public());
        info!("DHT node peer ID: {local_peer_id}");

        let mut swarm = libp2p::SwarmBuilder::with_existing_identity(local_key)
            .with_tokio()
            .with_tcp(
                tcp::Config::default(),
                noise::Config::new,
                yamux::Config::default,
            )?
            .with_dns()?
            .with_behaviour(|key: &Keypair| {
                let peer_id = Libp2pPeerId::from(key.public());
                let store = BoundedStore::new(peer_id);
                let kad_config = kad::Config::new(libp2p::StreamProtocol::new("/ipfs/kad/1.0.0"));
                let mut kademlia = kad::Behaviour::with_config(peer_id, store, kad_config);
                kademlia.set_mode(Some(kad::Mode::Server));
                let mdns: Toggle<mdns::tokio::Behaviour> = if !enable_mdns {
                    None.into()
                } else {
                    match mdns::tokio::Behaviour::new(mdns::Config::default(), peer_id) {
                        Ok(behaviour) => Some(behaviour).into(),
                        Err(e) => {
                            warn!(
                            "mDNS unavailable ({e}); continuing with DHT and bootstrap peers only"
                        );
                            None.into()
                        }
                    }
                };
                let identify = identify::Behaviour::new(
                    identify::Config::new("/gitforest/1.0.0".into(), key.public())
                        .with_agent_version(format!("gitforest/1.0.0 api/{api_port}")),
                );
                let limits = libp2p_connection_limits::Behaviour::new(
                    libp2p_connection_limits::ConnectionLimits::default()
                        .with_max_pending_incoming(Some(MAX_PENDING_INCOMING))
                        .with_max_established_incoming(Some(MAX_ESTABLISHED_INCOMING))
                        .with_max_established_per_peer(Some(MAX_ESTABLISHED_PER_PEER)),
                );
                Ok(GitforestBehaviour {
                    identify,
                    kademlia,
                    mdns,
                    limits,
                    ping: ping::Behaviour::new(
                        ping::Config::new().with_interval(std::time::Duration::from_secs(30)),
                    ),
                })
            })?
            .with_swarm_config(|c| {
                c.with_idle_connection_timeout(std::time::Duration::from_secs(600))
            })
            .build();

        let listen_addr: Multiaddr = match listen {
            std::net::IpAddr::V4(ip) => format!("/ip4/{ip}/tcp/{port}").parse()?,
            std::net::IpAddr::V6(ip) => format!("/ip6/{ip}/tcp/{port}").parse()?,
        };
        swarm.listen_on(listen_addr)?;

        let loop_handle = tokio::spawn(async move {
            use libp2p::swarm::SwarmEvent;

            let mut pending_lookups: std::collections::HashMap<
                kad::QueryId,
                (PendingKind, [u8; 32]),
            > = std::collections::HashMap::new();
            let track_lookup =
                |pending: &mut std::collections::HashMap<kad::QueryId, (PendingKind, [u8; 32])>,
                 id: kad::QueryId,
                 kind: PendingKind,
                 address: [u8; 32]| {
                    if pending.len() >= 4096 {
                        pending.clear();
                    }
                    pending.insert(id, (kind, address));
                };

            loop {
                tokio::select! {
                    cmd = cmd_rx.recv() => {
                        match cmd {
                            Some(DhtCommand::Announce(announcement)) => {
                                let key = kad::RecordKey::new(
                                    &gt_core::identity::repo_address(
                                        &announcement.owner,
                                        &announcement.name,
                                    ),
                                );
                                let value = serde_json::to_vec(&announcement)
                                    .unwrap_or_default();
                                let record = kad::Record {
                                    key,
                                    value,
                                    publisher: None,
                                    expires: None,
                                };
                                if let Err(e) = swarm.behaviour_mut().kademlia.put_record(
                                    record,
                                    kad::Quorum::One,
                                ) {
                                    warn!("Failed to announce repo: {e:?}");
                                }
                            }
                            Some(DhtCommand::Lookup { address }) => {
                                let key = kad::RecordKey::new(&address);
                                let id = swarm.behaviour_mut().kademlia.get_record(key);
                                track_lookup(&mut pending_lookups, id, PendingKind::Repo, address);
                            }
                            Some(DhtCommand::GetPeers { reply }) => {
                                let count = swarm.connected_peers().count();
                                let _ = reply.send(count);
                            }
                            Some(DhtCommand::AnnounceCompute(offer)) => {
                                let mut hasher = sha2::Sha256::new();
                                use sha2::Digest;
                                hasher.update(b"gitforest:compute:");
                                hasher.update(offer.worker_id.0);
                                let address: [u8; 32] = hasher.finalize().into();

                                let key = kad::RecordKey::new(&address);
                                let value = serde_json::to_vec(&offer).unwrap_or_default();
                                let record = kad::Record {
                                    key,
                                    value,
                                    publisher: None,
                                    expires: None,
                                };
                                if let Err(e) = swarm.behaviour_mut().kademlia.put_record(
                                    record,
                                    kad::Quorum::One,
                                ) {
                                    warn!("Failed to announce compute offer: {e:?}");
                                }
                                info!("Announced compute offer on DHT");
                            }
                            Some(DhtCommand::LookupCompute { address }) => {
                                let key = kad::RecordKey::new(&address);
                                let id = swarm.behaviour_mut().kademlia.get_record(key);
                                track_lookup(
                                    &mut pending_lookups,
                                    id,
                                    PendingKind::Compute,
                                    address,
                                );
                            }
                            Some(DhtCommand::WithdrawCompute { address }) => {
                                let key = kad::RecordKey::new(&address);
                                swarm.behaviour_mut().kademlia.remove_record(&key);
                                info!("Withdrew compute offer from DHT");
                            }
                            Some(DhtCommand::AnnounceBlob { address, data }) => {
                                let key = kad::RecordKey::new(&address);
                                let record = kad::Record {
                                    key,
                                    value: data,
                                    publisher: None,
                                    expires: None,
                                };
                                if let Err(e) = swarm.behaviour_mut().kademlia.put_record(
                                    record,
                                    kad::Quorum::One,
                                ) {
                                    warn!("Failed to announce blob: {e:?}");
                                }
                                info!("Announced blob on DHT");
                            }
                            Some(DhtCommand::LookupBlob { address }) => {
                                let key = kad::RecordKey::new(&address);
                                let id = swarm.behaviour_mut().kademlia.get_record(key);
                                track_lookup(&mut pending_lookups, id, PendingKind::Blob, address);
                            }
                            Some(DhtCommand::AnnounceCollection { address, data }) => {
                                let key = kad::RecordKey::new(&address);
                                let record = kad::Record {
                                    key,
                                    value: data,
                                    publisher: None,
                                    expires: None,
                                };
                                if let Err(e) = swarm.behaviour_mut().kademlia.put_record(
                                    record,
                                    kad::Quorum::One,
                                ) {
                                    warn!("Failed to announce collection: {e:?}");
                                }
                                info!("Announced collection on DHT");
                            }
                            Some(DhtCommand::LookupCollection { address }) => {
                                let key = kad::RecordKey::new(&address);
                                let id = swarm.behaviour_mut().kademlia.get_record(key);
                                track_lookup(
                                    &mut pending_lookups,
                                    id,
                                    PendingKind::Collection,
                                    address,
                                );
                            }
                            Some(DhtCommand::ConnectPeer { addr }) => {
                                info!("Dialing peer at {}", sanitize_log(&addr.to_string()));
                                match swarm.dial(addr.clone()) {
                                    Ok(_) => info!(
                                        "Dial initiated to {}",
                                        sanitize_log(&addr.to_string())
                                    ),
                                    Err(e) => warn!(
                                        "Failed to dial {}: {e:?}",
                                        sanitize_log(&addr.to_string())
                                    ),
                                }
                            }
                            Some(DhtCommand::Shutdown) => {
                                info!("DHT node shutting down");
                                break;
                            }
                            None => {
                                error!("DHT command channel closed without shutdown; loop exiting");
                                break;
                            }
                        }
                    }
                    event = swarm.select_next_some() => {
                        match event {
                            SwarmEvent::NewListenAddr { address, .. } => {
                                info!("DHT listening on {address}");
                            }
                            SwarmEvent::Behaviour(
                                GitforestBehaviourEvent::Mdns(
                                    mdns::Event::Discovered(peers)
                                )
                            ) => {
                                for (peer_id, addr) in peers {
                                    info!(
                                        "mDNS discovered peer: {peer_id} at {}",
                                        sanitize_log(&addr.to_string())
                                    );
                                    swarm.behaviour_mut().kademlia.add_address(
                                        &peer_id,
                                        addr.clone(),
                                    );
                                    let _ = event_tx.send(DhtEvent::PeerDiscovered {
                                        peer_id: peer_id.to_string(),
                                        addresses: vec![addr],
                                        gitforest_peer_id: None,
                                        api_port: None,
                                    }).await;
                                }
                            }
                            SwarmEvent::Behaviour(
                                GitforestBehaviourEvent::Kademlia(
                                    kad::Event::OutboundQueryProgressed {
                                        id,
                                        result: kad::QueryResult::GetRecord(query),
                                        ..
                                    }
                                )
                            ) => {
                                let tracked = pending_lookups.get(&id).copied();
                                match query {
                                    Ok(kad::GetRecordOk::FoundRecord(
                                        kad::PeerRecord { record, .. },
                                    )) => {
                                        match tracked.map(|(kind, _)| kind) {
                                            Some(PendingKind::Compute) => {
                                                match serde_json::from_slice::<
                                                    ComputeOfferMessage,
                                                >(
                                                    &record.value
                                                ) {
                                                    Ok(offer) => {
                                                        let _ = event_tx
                                                            .send(DhtEvent::ComputeOfferFound(offer))
                                                            .await;
                                                    }
                                                    Err(e) => warn!(
                                                        "compute offer record failed to parse: {}",
                                                        sanitize_log(&e.to_string())
                                                    ),
                                                }
                                            }
                                            Some(PendingKind::Blob) => {
                                                if let Some((_, address)) = tracked {
                                                    let _ = event_tx
                                                        .send(DhtEvent::BlobFound {
                                                            address,
                                                            data: record.value.clone(),
                                                        })
                                                        .await;
                                                }
                                            }
                                            Some(PendingKind::Collection) => {
                                                if let Some((_, address)) = tracked {
                                                    let _ = event_tx
                                                        .send(DhtEvent::CollectionFound {
                                                            address,
                                                            data: record.value.clone(),
                                                        })
                                                        .await;
                                                }
                                            }
                                            Some(PendingKind::Repo) | None => {
                                                if let Ok(announcement) =
                                                    serde_json::from_slice::<RepoAnnouncement>(
                                                        &record.value,
                                                    )
                                                {
                                                    let _ = event_tx
                                                        .send(DhtEvent::RepoFound(
                                                            RepoInfoResponse {
                                                                announcement: Some(announcement),
                                                                seeders: vec![],
                                                                leecher_count: 0,
                                                            },
                                                        ))
                                                        .await;
                                                } else if tracked.is_none() {
                                                    if let Ok(offer) = serde_json::from_slice::<
                                                        ComputeOfferMessage,
                                                    >(
                                                        &record.value
                                                    ) {
                                                        let _ = event_tx
                                                            .send(DhtEvent::ComputeOfferFound(offer))
                                                            .await;
                                                    }
                                                }
                                            }
                                        }
                                    }
                                    Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord {
                                        ..
                                    }) => {
                                        pending_lookups.remove(&id);
                                    }
                                    Err(kad::GetRecordError::NotFound { .. }) => {
                                        pending_lookups.remove(&id);
                                        match tracked {
                                            Some((PendingKind::Repo, address)) => {
                                                let _ = event_tx
                                                    .send(DhtEvent::RepoNotFound { address })
                                                    .await;
                                            }
                                            Some((PendingKind::Blob, address)) => {
                                                let _ = event_tx
                                                    .send(DhtEvent::BlobNotFound { address })
                                                    .await;
                                            }
                                            Some((PendingKind::Compute, address)) => {
                                                info!(
                                                    "no compute offer on DHT for {}",
                                                    hex::encode(address)
                                                );
                                            }
                                            Some((PendingKind::Collection, _)) | None => {}
                                        }
                                    }
                                    Err(e) => {
                                        pending_lookups.remove(&id);
                                        warn!("DHT record query failed: {e:?}");
                                    }
                                }
                            }
                            SwarmEvent::Behaviour(
                                GitforestBehaviourEvent::Identify(
                                    identify::Event::Received { peer_id, info, .. }
                                )
                            ) => {
                                info!("Identified peer {peer_id}: agent={}, protocols={:?}",
                                    sanitize_log(&info.agent_version),
                                    info.protocols.iter().map(|p| p.to_string()).collect::<Vec<_>>());
                                for addr in &info.listen_addrs {
                                    if multiaddr_is_loopback(addr) {
                                        continue;
                                    }
                                    swarm.behaviour_mut().kademlia.add_address(&peer_id, addr.clone());
                                }
                                // Derive gitforest peer ID from the Ed25519 public key, if available.
                                let gf_peer_id = info
                                    .public_key
                                    .try_into_ed25519()
                                    .ok()
                                    .map(|pk| gt_core::identity::PeerId(pk.to_bytes()).to_string());
                                let mut addresses: Vec<Multiaddr> = info
                                    .listen_addrs
                                    .iter()
                                    .filter(|addr| !multiaddr_is_loopback(addr))
                                    .cloned()
                                    .collect();
                                addresses.push(
                                    format!("/p2p/{peer_id}").parse().expect("valid p2p multiaddr")
                                );
                                let _ = event_tx.send(DhtEvent::PeerDiscovered {
                                    peer_id: peer_id.to_string(),
                                    addresses,
                                    gitforest_peer_id: gf_peer_id,
                                    api_port: parse_agent_api_port(&info.agent_version),
                                }).await;
                            }
                            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                                info!("Connected to peer {peer_id} via {endpoint:?}");
                                // Only record the remote address when WE dialed: it is the
                                // listen address we connected to. Inbound remote addresses
                                // are ephemeral source ports — undialable noise.
                                if let Some(addr) = recordable_connection_addr(&endpoint) {
                                    swarm.behaviour_mut().kademlia.add_address(
                                        &peer_id,
                                        addr.clone(),
                                    );
                                    let _ = event_tx.send(DhtEvent::PeerDiscovered {
                                        peer_id: peer_id.to_string(),
                                        addresses: vec![addr],
                                        gitforest_peer_id: None,
                                        api_port: None,
                                    }).await;
                                }
                                // Trigger a bootstrap to open Kademlia substreams and keep the connection alive
                                if let Err(e) = swarm.behaviour_mut().kademlia.bootstrap() {
                                    warn!("Bootstrap failed after connection: {e:?}");
                                }
                            }
                            SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                                info!("Disconnected from peer {peer_id}: {cause:?}");
                            }
                            SwarmEvent::OutgoingConnectionError { error, .. } => {
                                warn!(
                                    "Outgoing connection failed: {}",
                                    sanitize_log(&error.to_string())
                                );
                            }
                            SwarmEvent::IncomingConnectionError { error, .. } => {
                                warn!(
                                    "Incoming connection failed: {}",
                                    sanitize_log(&error.to_string())
                                );
                            }
                            _ => {}
                        }
                    }
                }
            }
        });
        tokio::spawn(async move {
            match loop_handle.await {
                Ok(()) => warn!("DHT event loop exited"),
                Err(e) => {
                    if e.is_panic() {
                        let payload = e.into_panic();
                        error!(
                            "DHT event loop panicked: {}",
                            panic_payload_str(payload.as_ref())
                        );
                    } else {
                        error!("DHT event loop failed: {e}");
                    }
                }
            }
        });

        Ok(DhtNode { cmd_tx, event_rx })
    }

    /// Receive the next event from the DHT node.
    pub async fn recv(&mut self) -> Option<DhtEvent> {
        self.event_rx.recv().await
    }

    /// Announce a repo on the network.
    pub async fn announce(&self, announcement: RepoAnnouncement) -> anyhow::Result<()> {
        self.send(DhtCommand::Announce(announcement))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send announce command: {e}"))
    }

    /// Look up a repo on the network.
    pub async fn lookup(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::Lookup { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send lookup command: {e}"))
    }

    /// Announce available compute resources on the network.
    pub async fn announce_compute(&self, offer: ComputeOfferMessage) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceCompute(offer))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute announce: {e}"))
    }

    /// Look up compute offers on the network.
    pub async fn lookup_compute(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupCompute { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute lookup: {e}"))
    }

    /// Withdraw our compute offer from the network.
    pub async fn withdraw_compute(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::WithdrawCompute { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute withdraw: {e}"))
    }

    /// Announce a blob on the network.
    pub async fn announce_blob(&self, address: [u8; 32], data: Vec<u8>) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceBlob { address, data })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send blob announce: {e}"))
    }

    /// Look up a blob on the network.
    pub async fn lookup_blob(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupBlob { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send blob lookup: {e}"))
    }

    /// Announce a collection on the network.
    pub async fn announce_collection(
        &self,
        address: [u8; 32],
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceCollection { address, data })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send collection announce: {e}"))
    }

    /// Look up a collection on the network.
    pub async fn lookup_collection(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupCollection { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send collection lookup: {e}"))
    }

    /// Connect to a peer by multiaddr (e.g., "/ip4/192.168.1.50/tcp/9418").
    pub async fn connect_peer(&self, addr: Multiaddr) -> anyhow::Result<()> {
        self.send(DhtCommand::ConnectPeer { addr })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send connect command: {e}"))
    }

    /// Connect to a peer by multiaddr string.
    pub async fn connect_peer_str(&self, addr: &str) -> anyhow::Result<()> {
        let multiaddr: Multiaddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid multiaddr '{addr}': {e}"))?;
        self.connect_peer(multiaddr).await
    }

    /// Shut down the DHT node.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.send(DhtCommand::Shutdown)
            .await
            .map_err(|e| anyhow::anyhow!("failed to send shutdown command: {e}"))
    }
}

impl DhtNodeSender {
    pub fn for_test() -> (Self, mpsc::Receiver<DhtCommand>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(256);
        (DhtNodeSender { cmd_tx }, cmd_rx)
    }

    /// Send a raw command to the DHT node.
    #[allow(clippy::result_large_err)]
    pub async fn send(&self, cmd: DhtCommand) -> Result<(), mpsc::error::SendError<DhtCommand>> {
        self.cmd_tx.send(cmd).await
    }

    /// True while the DHT event loop is alive to receive commands.
    /// A dead loop means no dials, announces, or lookups happen even
    /// though the HTTP API keeps serving, so callers surface this.
    pub fn is_alive(&self) -> bool {
        !self.cmd_tx.is_closed()
    }

    /// Announce a repo on the network.
    pub async fn announce(&self, announcement: RepoAnnouncement) -> anyhow::Result<()> {
        self.send(DhtCommand::Announce(announcement))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send announce command: {e}"))
    }

    /// Look up a repo on the network.
    pub async fn lookup(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::Lookup { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send lookup command: {e}"))
    }

    /// Announce available compute resources on the network.
    pub async fn announce_compute(&self, offer: ComputeOfferMessage) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceCompute(offer))
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute announce: {e}"))
    }

    /// Look up compute offers on the network.
    pub async fn lookup_compute(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupCompute { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute lookup: {e}"))
    }

    /// Withdraw our compute offer from the network.
    pub async fn withdraw_compute(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::WithdrawCompute { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send compute withdraw: {e}"))
    }

    /// Announce a blob on the network.
    pub async fn announce_blob(&self, address: [u8; 32], data: Vec<u8>) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceBlob { address, data })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send blob announce: {e}"))
    }

    /// Look up a blob on the network.
    pub async fn lookup_blob(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupBlob { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send blob lookup: {e}"))
    }

    /// Announce a collection on the network.
    pub async fn announce_collection(
        &self,
        address: [u8; 32],
        data: Vec<u8>,
    ) -> anyhow::Result<()> {
        self.send(DhtCommand::AnnounceCollection { address, data })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send collection announce: {e}"))
    }

    /// Look up a collection on the network.
    pub async fn lookup_collection(&self, address: [u8; 32]) -> anyhow::Result<()> {
        self.send(DhtCommand::LookupCollection { address })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send collection lookup: {e}"))
    }

    /// Connect to a peer by multiaddr.
    pub async fn connect_peer(&self, addr: Multiaddr) -> anyhow::Result<()> {
        self.send(DhtCommand::ConnectPeer { addr })
            .await
            .map_err(|e| anyhow::anyhow!("failed to send connect command: {e}"))
    }

    /// Connect to a peer by multiaddr string.
    pub async fn connect_peer_str(&self, addr: &str) -> anyhow::Result<()> {
        let multiaddr: Multiaddr = addr
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid multiaddr '{addr}': {e}"))?;
        self.connect_peer(multiaddr).await
    }

    /// Shut down the DHT node.
    pub async fn shutdown(&self) -> anyhow::Result<()> {
        self.send(DhtCommand::Shutdown)
            .await
            .map_err(|e| anyhow::anyhow!("failed to send shutdown command: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inbound_connection_addrs_are_not_recorded() {
        use libp2p::core::transport::PortUse;
        use libp2p::core::{ConnectedPoint, Endpoint};
        let addr: Multiaddr = "/ip4/203.0.113.9/tcp/9418".parse().unwrap();
        let dialed = ConnectedPoint::Dialer {
            address: addr.clone(),
            role_override: Endpoint::Dialer,
            port_use: PortUse::New,
        };
        assert_eq!(recordable_connection_addr(&dialed), Some(addr.clone()));
        let inbound = ConnectedPoint::Listener {
            local_addr: "/ip4/10.0.0.1/tcp/9418".parse().unwrap(),
            send_back_addr: addr,
        };
        assert_eq!(recordable_connection_addr(&inbound), None);
    }

    #[test]
    fn loopback_listen_addrs_are_detected() {
        let loopback: Multiaddr = "/ip4/127.0.0.1/tcp/9418".parse().unwrap();
        assert!(multiaddr_is_loopback(&loopback));
        let loopback6: Multiaddr = "/ip6/::1/tcp/9418".parse().unwrap();
        assert!(multiaddr_is_loopback(&loopback6));
        let public: Multiaddr = "/ip4/35.184.171.148/tcp/9418".parse().unwrap();
        assert!(!multiaddr_is_loopback(&public));
        let lan: Multiaddr = "/ip4/192.168.1.7/tcp/9418".parse().unwrap();
        assert!(!multiaddr_is_loopback(&lan));
    }

    #[test]
    fn test_sender_reports_dead_loop() {
        let (sender, rx) = DhtNodeSender::for_test();
        assert!(sender.is_alive());
        drop(rx);
        assert!(!sender.is_alive());
    }

    #[test]
    fn panic_payload_formats_known_shapes() {
        let owned: Box<dyn std::any::Any + Send> = Box::new(String::from("boom"));
        assert_eq!(panic_payload_str(owned.as_ref()), "boom");
        let borrowed: Box<dyn std::any::Any + Send> = Box::new("kaboom");
        assert_eq!(panic_payload_str(borrowed.as_ref()), "kaboom");
        let foreign: Box<dyn std::any::Any + Send> = Box::new(42u32);
        assert_eq!(
            panic_payload_str(foreign.as_ref()),
            "non-string panic payload"
        );
    }

    #[test]
    fn test_parse_agent_api_port() {
        assert_eq!(
            parse_agent_api_port("gitforest/1.0.0 api/17878"),
            Some(17878)
        );
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/7878"), Some(7878));
        assert_eq!(parse_agent_api_port("libp2p/0.53.0"), None);
        assert_eq!(parse_agent_api_port(""), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0"), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/0"), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/99999"), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/abc"), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/"), None);
        assert_eq!(parse_agent_api_port("gitforest/1.0.0 api/17878junk"), None);
        assert_eq!(
            parse_agent_api_port("api/17879 extra tokens here"),
            Some(17879)
        );
    }
}
