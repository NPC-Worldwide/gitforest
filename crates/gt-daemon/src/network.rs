use anyhow::Result;
use gt_core::config::Config;
use gt_core::identity::Identity;
use gt_core::sanitize::sanitize_log;
use gt_dht::node::DhtNode;
use gt_transport::git_bridge;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, Semaphore};
use tracing::{error, info, warn};

use crate::repo_manager::RepoManager;

/// Start the network layer: DHT node + git protocol TCP server.
pub async fn start_network(config: &Config, identity: &Identity) -> Result<DhtNode> {
    let listen: std::net::IpAddr = config
        .p2p_listen
        .as_deref()
        .unwrap_or("0.0.0.0")
        .parse()
        .map_err(|_| anyhow::anyhow!("bad p2p_listen address"))?;
    info!("Starting DHT node on {listen}:{}", config.p2p_port);
    let dht = DhtNode::start_on(
        listen,
        config.p2p_port,
        config.api_port,
        identity,
        config.enable_mdns,
    )
    .await
    .map_err(|e| anyhow::anyhow!("failed to start DHT: {e:?}"))?;
    info!("DHT node started");

    for peer_addr in &config.bootstrap_peers {
        if bootstrap_is_self(config, peer_addr) {
            info!("Skipping bootstrap dial to self: {peer_addr}");
            continue;
        }
        let dht_addr = dht_bootstrap_addr(peer_addr);
        if dht_addr.is_none() {
            warn!("Bootstrap peer address has no dialable part: {peer_addr}");
            continue;
        }
        let dht_addr = dht_addr.unwrap();
        info!("Connecting to bootstrap peer: {dht_addr}");
        if let Err(e) = dht.connect_peer_str(&dht_addr).await {
            warn!("Failed to connect to bootstrap peer {peer_addr}: {e}");
        }
    }

    // Announce all configured repos
    let repo_manager = RepoManager::new(config.data_dir.clone());
    for repo_config in &config.repos {
        if repo_config.seeding {
            match repo_manager.make_announcement(identity, repo_config) {
                Ok(announcement) => {
                    info!("Announcing repo '{}' on the network", repo_config.name);
                    if let Err(e) = dht.announce(announcement).await {
                        warn!("Failed to announce '{}': {e}", repo_config.name);
                    }
                }
                Err(e) => {
                    warn!(
                        "Failed to create announcement for '{}': {e}",
                        repo_config.name
                    );
                }
            }
        }
    }

    // Announce compute offer if worker is enabled
    if let Some(ref compute) = config.compute {
        if compute.worker_enabled {
            let mut offer = gt_core::protocol::ComputeOfferMessage {
                worker_id: identity.peer_id(),
                cpu_cores: compute.max_cpu_cores,
                ram_mb: compute.max_ram_mb,
                disk_mb: compute.max_disk_mb,
                has_gpu: compute.effective_gpu(),
                hourly_rate_sats: compute.hourly_rate_sats,
                accepted_chains: compute.accepted_chains.clone(),
                max_concurrent_jobs: compute.max_concurrent_jobs,
                pow_nonce: gt_core::identity::offer_pow_nonce_with_target(
                    identity,
                    compute.pow_target_bits,
                ),
                signature: vec![],
            };
            let payload = offer.signable_payload();
            offer.signature = identity.sign(&payload);
            info!(
                "Announcing compute worker: {} CPU, {} MB RAM, {} sats/hr",
                compute.max_cpu_cores, compute.max_ram_mb, compute.hourly_rate_sats
            );
            if let Err(e) = dht.announce_compute(offer).await {
                warn!("Failed to announce compute offer: {e}");
            }
        }
    }

    // Announce all stored blobs
    let blob_store = gt_lfs::store::BlobStore::new(&config.data_dir);
    if let Ok(blob_ids) = blob_store.list() {
        for blob_id in &blob_ids {
            if let Ok(Some(meta)) = blob_store.get_meta(blob_id) {
                let dht_addr = gt_lfs::blob::blob_dht_address(blob_id);
                let mut announcement = gt_core::protocol::BlobAnnouncement {
                    peer_id: identity.peer_id(),
                    blob_id: blob_id.clone(),
                    size: meta.total_size,
                    piece_count: meta.piece_count() as u32,
                    have_pieces: None,
                    signature: Vec::new(),
                };
                announcement.signature = identity.sign(&announcement.signable_payload());
                let data = serde_json::to_vec(&announcement).unwrap_or_default();
                if let Err(e) = dht.announce_blob(dht_addr, data).await {
                    warn!("Failed to announce blob {}: {e}", blob_id);
                }
            }
        }
        if !blob_ids.is_empty() {
            info!("Announced {} blobs on the network", blob_ids.len());
        }
    }

    let cache_store = crate::caches::CacheStore::load(&config.data_dir);
    let mut cache_count = 0;
    for entry in cache_store.entries() {
        let mut ann = gt_core::protocol::CacheAnnouncement {
            owner: identity.peer_id(),
            key: entry.key.clone(),
            blob_id: entry.blob_id.clone(),
            size: entry.size,
            updated_at: entry.created_at.timestamp(),
            signature: Vec::new(),
        };
        ann.signature = identity.sign(&ann.signable_payload());
        let addr = gt_lfs::blob::cache_dht_address(&entry.key);
        let data = serde_json::to_vec(&ann).unwrap_or_default();
        if dht.announce_collection(addr, data).await.is_ok() {
            cache_count += 1;
        }
    }
    if cache_count > 0 {
        info!("Announced {cache_count} cache keys on the network");
    }

    Ok(dht)
}

/// Start the git protocol TCP server for direct P2P git operations.
/// Listens on git_port and routes fetch/push to the appropriate git subprocess.
const GIT_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_GIT_CONNECTIONS: usize = 64;
const MAX_REQUEST_LINE: usize = 4096;

pub async fn start_git_server(
    config: Arc<RwLock<crate::api::AppState>>,
    git_port: u16,
) -> Result<()> {
    let listener = TcpListener::bind(format!("0.0.0.0:{git_port}")).await?;
    info!("Git protocol server listening on port {git_port}");
    let connection_cap = Arc::new(Semaphore::new(MAX_GIT_CONNECTIONS));

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                let Ok(permit) = connection_cap.clone().try_acquire_owned() else {
                    warn!("Git connection from {addr} rejected: at connection cap");
                    continue;
                };
                info!("Incoming git connection from {addr}");
                let state = config.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(e) = handle_git_connection(stream, state, GIT_READ_TIMEOUT).await {
                        error!(
                            "Git connection from {addr} failed: {}",
                            sanitize_log(&e.to_string())
                        );
                    }
                });
            }
            Err(e) => {
                error!("Failed to accept git connection: {e}");
            }
        }
    }
}

/// Handle an incoming git protocol connection.
/// Reads the initial request line to determine the repo and operation,
/// then delegates to git-upload-pack or git-receive-pack.
async fn handle_git_connection(
    mut stream: tokio::net::TcpStream,
    state: Arc<RwLock<crate::api::AppState>>,
    read_timeout: Duration,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    // Read the git protocol pkt-line header to figure out what's being requested.
    // Git protocol format: "XXXX" (4 hex chars for length) + "git-upload-pack /repo\0host=...\0"
    // Each read is timeout-bounded so a client that connects and sends nothing
    // (slowloris) cannot hold a connection slot forever.
    let mut len_buf = [0u8; 4];
    tokio::time::timeout(read_timeout, stream.read_exact(&mut len_buf))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading git request"))??;
    let len_str = std::str::from_utf8(&len_buf)?;
    let pkt_len = usize::from_str_radix(len_str, 16)?;

    if !(4..=MAX_REQUEST_LINE).contains(&pkt_len) {
        anyhow::bail!("invalid pkt-line length: {pkt_len}");
    }

    let mut payload = vec![0u8; pkt_len - 4];
    tokio::time::timeout(read_timeout, stream.read_exact(&mut payload))
        .await
        .map_err(|_| anyhow::anyhow!("timed out reading git request"))??;
    let request = String::from_utf8_lossy(&payload);

    // Parse: "git-upload-pack /reponame\0host=...\0"
    let parts: Vec<&str> = request.splitn(2, ' ').collect();
    if parts.len() < 2 {
        anyhow::bail!("invalid git request: {request}");
    }

    let command = parts[0];
    let repo_path_raw = parts[1].split('\0').next().unwrap_or("");
    let repo_name = repo_path_raw
        .trim_start_matches('/')
        .trim_end_matches(".git");

    info!(
        "Git request: {} for repo '{}'",
        sanitize_log(command),
        sanitize_log(repo_name)
    );

    // Find the repo in our config
    let state = state.read().await;
    let repo_config = state.config.repos.iter().find(|r| r.name == repo_name);

    let repo_config = match repo_config {
        Some(r) => r,
        None => {
            anyhow::bail!("repo '{}' not found", repo_name);
        }
    };

    let repo_path = repo_config.path.clone();
    drop(state);

    match command {
        "git-upload-pack" => {
            git_bridge::serve_upload_pack(&repo_path, stream).await?;
        }
        "git-receive-pack" => {
            // Direct TCP pushes are not supported for security reasons — there is no
            // transport-level identity verification on the raw git protocol port.
            // All pushes must go through the HTTP API (/api/remote/{peer}/{repo}/push)
            // where ACL checks and bundle validation are enforced.
            anyhow::bail!(
                "direct git push over TCP is disabled. Use the gitforest remote helper \
                 (git push gitforest://<peer_id>/<repo>) which routes through the authenticated HTTP API."
            );
        }
        _ => {
            anyhow::bail!("unsupported git command: {command}");
        }
    }

    Ok(())
}

fn tcp_host_port(peer_addr: &str) -> Option<(&str, u16)> {
    let parts: Vec<&str> = peer_addr.split('/').collect();
    let host_idx = parts
        .iter()
        .position(|&s| s == "ip4" || s == "ip6" || s == "dns4" || s == "dns")?;
    let host = parts.get(host_idx + 1)?;
    let port = parts
        .iter()
        .position(|&s| s == "tcp")
        .and_then(|idx| parts.get(idx + 1))
        .and_then(|s| s.parse().ok())?;
    Some((host, port))
}

/// True when a bootstrap entry points at this daemon itself: either our
/// advertised external address, or our own p2p port on loopback. Shared
/// bootstrap lists always contain every node including self, so without
/// this every node noisily dials itself on startup.
pub(crate) fn bootstrap_is_self(config: &Config, peer_addr: &str) -> bool {
    let Some((host, port)) = tcp_host_port(peer_addr) else {
        return false;
    };
    if port != config.p2p_port {
        return false;
    }
    if host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost") {
        return true;
    }
    if let Some(ref adv) = config.advertise_addr {
        if let Some((adv_host, adv_port)) = tcp_host_port(adv) {
            return host.eq_ignore_ascii_case(adv_host) && adv_port == config.p2p_port;
        }
    }
    false
}

pub(crate) fn dht_bootstrap_addr(peer_addr: &str) -> Option<String> {
    let mut addr = peer_addr.to_string();
    for marker in ["/api/", "/gf/"] {
        if let Some(idx) = addr.find(marker) {
            addr.truncate(idx);
        }
    }
    addr = addr.trim_end_matches('/').to_string();
    if addr.is_empty() {
        None
    } else {
        Some(addr)
    }
}

/// Re-dial bootstrap peers when DHT connectivity has decayed. libp2p prunes
/// idle connections after 120s and startup is the only other dial site, so
/// without this the DHT mesh rots to zero whenever traffic goes quiet.
/// Returns the number of dials issued.
pub(crate) async fn ensure_dht_mesh(dht: &gt_dht::node::DhtNodeSender, config: &Config) -> usize {
    let targets: Vec<String> = config
        .bootstrap_peers
        .iter()
        .filter(|p| !bootstrap_is_self(config, p))
        .filter_map(|p| dht_bootstrap_addr(p))
        .collect();
    if targets.is_empty() {
        return 0;
    }
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let connected = match dht
        .send(gt_dht::node::DhtCommand::GetPeers { reply: reply_tx })
        .await
    {
        Ok(()) => tokio::time::timeout(Duration::from_millis(300), reply_rx)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0),
        Err(_) => 0,
    };
    if connected >= targets.len() {
        return 0;
    }
    let mut dialed = 0;
    for addr in &targets {
        match dht.connect_peer_str(addr).await {
            Ok(()) => dialed += 1,
            Err(e) => warn!("DHT redial to {addr} failed: {e}"),
        }
    }
    if dialed > 0 {
        info!(
            "DHT mesh redial: {connected}/{} connected, dialed {dialed}",
            targets.len()
        );
    }
    dialed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpStream;

    #[test]
    fn bootstrap_self_detection_skips_own_addrs() {
        let mut config = Config {
            p2p_port: 9418,
            advertise_addr: Some("/ip4/136.114.20.123/tcp/9418/api/7878".into()),
            ..Config::default()
        };
        assert!(bootstrap_is_self(
            &config,
            "/ip4/136.114.20.123/tcp/9418/api/7878"
        ));
        assert!(bootstrap_is_self(
            &config,
            "/ip4/127.0.0.1/tcp/9418/api/7878"
        ));
        assert!(!bootstrap_is_self(
            &config,
            "/ip4/35.184.171.148/tcp/9418/api/7878"
        ));
        assert!(!bootstrap_is_self(
            &config,
            "/ip4/136.114.20.123/tcp/9999/api/7878"
        ));
        assert!(!bootstrap_is_self(&config, "not-an-addr"));
        config.advertise_addr = None;
        assert!(!bootstrap_is_self(
            &config,
            "/ip4/136.114.20.123/tcp/9418/api/7878"
        ));
        assert!(bootstrap_is_self(
            &config,
            "/ip4/127.0.0.1/tcp/9418/api/7878"
        ));
    }

    fn mesh_test_config() -> Config {
        Config {
            p2p_port: 9418,
            bootstrap_peers: vec![
                "/ip4/10.0.0.1/tcp/9418/p2p/12D3KooWGFVp8agDRPk3fXqYgtq696dUDQM3VPQ9VZSQYhcy36AV"
                    .into(),
                "/ip4/10.0.0.2/tcp/9418/p2p/12D3KooWKVYdaERYxwL12wDtU1KDQ4Heh23Kcpk3AjnNuPQyNptC"
                    .into(),
            ],
            ..Config::default()
        }
    }

    #[tokio::test]
    async fn redial_dials_bootstraps_when_disconnected() {
        use gt_dht::node::{DhtCommand, DhtNodeSender};
        let (dht, mut cmd_rx) = DhtNodeSender::for_test();
        let config = mesh_test_config();
        let handle = tokio::spawn(async move { ensure_dht_mesh(&dht, &config).await });
        let cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let reply = match cmd {
            DhtCommand::GetPeers { reply } => reply,
            other => panic!("expected GetPeers, got {other:?}"),
        };
        reply.send(0).unwrap();
        for _ in 0..2 {
            let cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
                .await
                .unwrap()
                .unwrap();
            assert!(matches!(cmd, DhtCommand::ConnectPeer { .. }));
        }
        assert_eq!(handle.await.unwrap(), 2);
    }

    #[tokio::test]
    async fn redial_stays_quiet_when_meshed() {
        use gt_dht::node::{DhtCommand, DhtNodeSender};
        let (dht, mut cmd_rx) = DhtNodeSender::for_test();
        let config = mesh_test_config();
        let handle = tokio::spawn(async move { ensure_dht_mesh(&dht, &config).await });
        let cmd = tokio::time::timeout(Duration::from_secs(5), cmd_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let reply = match cmd {
            DhtCommand::GetPeers { reply } => reply,
            other => panic!("expected GetPeers, got {other:?}"),
        };
        reply.send(2).unwrap();
        assert_eq!(handle.await.unwrap(), 0);
        let after = tokio::time::timeout(Duration::from_millis(100), cmd_rx.recv()).await;
        assert!(matches!(after, Err(_) | Ok(None)));
    }

    fn test_app_state() -> Arc<RwLock<crate::api::AppState>> {
        let tmp = std::env::temp_dir().join(format!("gf_net_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let data_dir = tmp.join("data");
        let config = Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        };
        let identity = Identity::generate();
        let (dht, _rx) = gt_dht::DhtNodeSender::for_test();
        let blob_store = Arc::new(gt_lfs::store::BlobStore::new(&data_dir));
        let state = crate::api::AppState {
            config,
            config_path: tmp.join("config.toml"),
            identity,
            dht,
            patch_store: gt_collab::patches::PatchStore::new(),
            mr_store: gt_collab::merge_request::MergeRequestStore::new(),
            acls: HashMap::new(),
            model_acls: HashMap::new(),
            cache_store: crate::caches::CacheStore::load(&data_dir),
            remote_caches: crate::caches::RemoteCacheDir::default(),
            job_store: gt_compute::job::ComputeJobStore::new(),
            result_store: gt_compute::result::JobResultStore::new(),
            worker_registry: gt_compute::worker::WorkerRegistry::new(),
            submitter_registry: gt_compute::worker::SubmitterRegistry::new(),
            payment_manager: gt_compute::payment::PaymentManager::new(),
            credit_ledger: gt_compute::credit::CreditLedger::new(),
            blob_store: blob_store.clone(),
            blob_tracker: gt_lfs::tracker::BlobTracker::new(),
            collection_store: gt_lfs::collection::CollectionStore::new(),
            transfer_manager: gt_lfs::transfer::TransferManager::new(
                blob_store,
                Arc::new(RwLock::new(gt_lfs::tracker::BlobTracker::new())),
            ),
            known_peers: Vec::new(),
            peer_last_seen: HashMap::new(),
            gossip_learned: std::collections::HashSet::new(),
            wanted_blobs: std::collections::HashSet::new(),
            foreign_repos: HashMap::new(),
            wanted_collections: std::collections::HashSet::new(),
            collection_seeders: HashMap::new(),
            workflow_runs: crate::workflows::WorkflowRunStore::new(),
            schedule_fired: HashMap::new(),
            known_models: HashMap::new(),
            api_token: "test-token".into(),
            secrets_passphrase: None,
            peer_served: crate::api::PeerServeStats::default(),
            identity_path: tmp.join("identity.key"),
            analytics_history: std::collections::VecDeque::new(),
        };
        Arc::new(RwLock::new(state))
    }

    async fn accepted_pair() -> (tokio::net::TcpStream, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).await.unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (server, client)
    }

    #[tokio::test]
    async fn silent_client_times_out_instead_of_hanging() {
        let (server, _client) = accepted_pair().await;
        let start = std::time::Instant::now();
        let err = handle_git_connection(server, test_app_state(), Duration::from_millis(100))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn oversized_request_line_is_rejected() {
        let (server, mut client) = accepted_pair().await;
        client.write_all(b"ffff").await.unwrap();
        let err = handle_git_connection(server, test_app_state(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("invalid pkt-line"), "{err}");
    }

    #[tokio::test]
    async fn unknown_repo_yields_not_found() {
        let (server, mut client) = accepted_pair().await;
        let payload = b"git-upload-pack /nonexistent\0host=x\0";
        let header = format!("{:04x}", payload.len() + 4);
        client.write_all(header.as_bytes()).await.unwrap();
        client.write_all(payload).await.unwrap();
        let err = handle_git_connection(server, test_app_state(), Duration::from_secs(5))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not found"), "{err}");
    }
}
