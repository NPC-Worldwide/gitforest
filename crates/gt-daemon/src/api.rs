use axum::{
    extract::{ConnectInfo, Path as AxPath, State},
    http::{header, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{
        sse::{Event as SseEvent, KeepAlive, Sse},
        IntoResponse, Response,
    },
    routing::{delete, get, post},
    Json, Router,
};
use base64::Engine;
use gt_collab::acl::{ModelAcl, RepoAcl};
use gt_collab::merge_request::{MergeRequest, MergeRequestStore};
use gt_collab::patches::{PatchSet, PatchStore};
use gt_compute::job::{ComputeJob, ComputeJobStore, JobStatus};
use gt_compute::payment::{ChainType, PaymentChannel, PaymentManager};
use gt_compute::permissions::{verify_job_token, PermissionLevel, PermissionScope, Permissions};
use gt_compute::result::JobResultStore;
use gt_compute::worker::{SubmitterRegistry, WorkerRegistry};
use gt_core::config::{ComputeConfig, Config, RepoConfig};
use gt_core::identity::{Identity, PeerId};
use gt_core::sanitize::sanitize_log;
use gt_dht::node::{DhtEvent, DhtNode};
use gt_lfs::collection::CollectionStore;
use gt_lfs::store::BlobStore;
use gt_lfs::tracker::BlobTracker;
use gt_lfs::transfer::TransferManager;
use gt_transport::git_bridge;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tracing::info;

/// Shared state for the API.
#[allow(dead_code)]
pub struct AppState {
    pub config: Config,
    pub config_path: std::path::PathBuf,
    pub identity: Identity,
    pub dht: gt_dht::DhtNodeSender,
    pub patch_store: PatchStore,
    pub mr_store: MergeRequestStore,
    pub acls: HashMap<String, RepoAcl>,
    pub model_acls: HashMap<String, ModelAcl>,
    pub cache_store: crate::caches::CacheStore,
    pub remote_caches: crate::caches::RemoteCacheDir,
    pub job_store: ComputeJobStore,
    pub result_store: JobResultStore,
    pub worker_registry: WorkerRegistry,
    pub submitter_registry: SubmitterRegistry,
    pub payment_manager: PaymentManager,
    pub credit_ledger: gt_compute::credit::CreditLedger,
    pub blob_store: Arc<BlobStore>,
    pub blob_tracker: BlobTracker,
    pub collection_store: CollectionStore,
    pub transfer_manager: TransferManager,
    /// Peers discovered via gossip / DHT (multiaddr strings).
    pub known_peers: Vec<String>,
    pub peer_last_seen: HashMap<String, Instant>,
    /// Entries learned only via gossip; usable for dialing but never re-served,
    /// so stale junk cannot circulate forever.
    pub gossip_learned: HashSet<String>,
    /// Blob ids we want to download (drives the fetch loop).
    pub wanted_blobs: HashSet<gt_core::types::BlobId>,
    /// Repos fetched for foreign (remotely submitted) jobs, by job id.
    pub foreign_repos: HashMap<String, ForeignRepo>,
    /// Collection manifests we want to fetch (drives the fetch loop).
    pub wanted_collections: HashSet<String>,
    /// Known seeders per collection manifest id.
    pub collection_seeders: HashMap<String, Vec<gt_core::identity::PeerId>>,
    pub workflow_runs: crate::workflows::WorkflowRunStore,
    pub schedule_fired: HashMap<String, String>,
    pub known_models: HashMap<String, crate::models::ModelRecord>,
    /// Random bearer token for local API auth.
    pub api_token: String,
    /// Operator passphrase for v2 secrets envelopes, resolved once at startup.
    pub secrets_passphrase: Option<Vec<u8>>,
    pub peer_served: PeerServeStats,
    pub identity_path: std::path::PathBuf,
    pub analytics_history: VecDeque<AnalyticsSample>,
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct AnalyticsSample {
    pub t: u64,
    pub peers: u64,
}

pub const ANALYTICS_SAMPLE_INTERVAL_SECS: u64 = 30;
pub const ANALYTICS_HISTORY_CAP: usize = 3000;

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn record_analytics_sample(history: &mut VecDeque<AnalyticsSample>, peers: u64) {
    history.push_back(AnalyticsSample {
        t: now_millis(),
        peers,
    });
    while history.len() > ANALYTICS_HISTORY_CAP {
        history.pop_front();
    }
}

fn parse_history_window_ms(window: &str) -> Option<u64> {
    let hours = window.strip_suffix('h').map(|n| (n, 3_600_000u64));
    let days = window.strip_suffix('d').map(|n| (n, 86_400_000u64));
    let (num, unit) = hours.or(days)?;
    let n: u64 = num.parse().ok()?;
    if n == 0 || n > 30 {
        return None;
    }
    n.checked_mul(unit)
}

#[derive(Debug, Clone, Default, Serialize)]
pub struct PeerServeStats {
    pub requests: u64,
    pub stream_requests: u64,
    pub elapsed_ms: u64,
    pub by_model: HashMap<String, u64>,
}

impl PeerServeStats {
    pub fn record(&mut self, model: &str, stream: bool, elapsed_ms: u64) {
        if stream {
            self.stream_requests += 1;
        } else {
            self.requests += 1;
            self.elapsed_ms += elapsed_ms;
        }
        *self.by_model.entry(model.to_string()).or_insert(0) += 1;
    }
}

impl AppState {
    pub fn new(
        config: Config,
        config_path: std::path::PathBuf,
        identity: Identity,
        identity_path: std::path::PathBuf,
        dht: gt_dht::DhtNodeSender,
    ) -> Self {
        let mut payment_manager = PaymentManager::new();
        register_payment_backends(&config, &mut payment_manager);

        // Initialize ACLs for all owned repos from config
        let mut acls = HashMap::new();
        for repo in &config.repos {
            if repo.owner == identity.peer_id() {
                let mut acl = RepoAcl::new(&identity, &repo.name);
                // Restore grants from config
                if let Some(grants) = config.acl_grants.get(&repo.name) {
                    for g in grants {
                        let _ = acl.grant(&identity, g.peer_id.clone(), g.ref_pattern.clone());
                    }
                }
                acls.insert(repo.name.clone(), acl);
            }
        }

        let blob_store = Arc::new(BlobStore::new(&config.data_dir));
        let blob_tracker = BlobTracker::new();
        let transfer_manager = TransferManager::new(
            blob_store.clone(),
            Arc::new(RwLock::new(BlobTracker::new())),
        );
        let cache_store = crate::caches::CacheStore::load(&config.data_dir);

        // Generate a random API token for local auth
        let api_token = {
            use rand::Rng;
            let mut rng = rand::thread_rng();
            (0..32)
                .map(|_| rng.sample(rand::distributions::Alphanumeric) as char)
                .collect::<String>()
        };
        let token_path = std::env::var("HOME")
            .ok()
            .map(|h| std::path::PathBuf::from(h).join(".local/share/gitforest/api.token"))
            .unwrap_or_else(|| std::env::temp_dir().join("gitforest-api.token"));
        let _ = std::fs::create_dir_all(token_path.parent().unwrap_or(&config.data_dir));
        let _ = write_secret_file(&token_path, api_token.as_bytes());
        tracing::info!("API token written to {}", token_path.display());

        if let Some(ref cluster) = config.cluster_token {
            let cluster_path = token_path.with_file_name("cluster.token");
            let _ = write_secret_file(&cluster_path, cluster.as_bytes());
            tracing::info!("Cluster token written to {}", cluster_path.display());
        }

        let mut known_peers = Vec::new();
        if let Some(addr) = our_api_multiaddr(&config, &identity) {
            if let Some(canon) = canonical_peer_entry(&addr) {
                known_peers.push(canon);
            }
        }

        let workers_path = config.data_dir.join("workers.json");
        let submitters_path = config.data_dir.join("submitters.json");
        let credit_path = config.data_dir.join("mutual_credit.json");
        let min_pow_bits = config
            .compute
            .as_ref()
            .map(|c| c.min_pow_bits)
            .unwrap_or(gt_core::identity::MIN_POW_BITS);
        let mut worker_registry = WorkerRegistry::load(&workers_path);
        worker_registry.retain_pow(min_pow_bits);
        let model_acls = restore_model_acls(&config, &identity);
        AppState {
            config,
            config_path,
            identity,
            dht,
            patch_store: PatchStore::new(),
            mr_store: MergeRequestStore::new(),
            acls,
            model_acls,
            cache_store,
            remote_caches: crate::caches::RemoteCacheDir::default(),
            job_store: ComputeJobStore::new(),
            result_store: JobResultStore::new(),
            worker_registry,
            submitter_registry: SubmitterRegistry::load(&submitters_path),
            payment_manager,
            credit_ledger: gt_compute::credit::CreditLedger::load(&credit_path),
            blob_store,
            blob_tracker,
            collection_store: CollectionStore::new(),
            transfer_manager,
            known_peers,
            peer_last_seen: HashMap::new(),
            gossip_learned: HashSet::new(),
            wanted_blobs: HashSet::new(),
            foreign_repos: HashMap::new(),
            wanted_collections: HashSet::new(),
            collection_seeders: HashMap::new(),
            workflow_runs: crate::workflows::WorkflowRunStore::new(),
            schedule_fired: HashMap::new(),
            known_models: HashMap::new(),
            api_token,
            secrets_passphrase: crate::secrets::secrets_passphrase_from_env(),
            peer_served: PeerServeStats::default(),
            identity_path,
            analytics_history: VecDeque::new(),
        }
    }
}

#[derive(Clone)]
pub struct ForeignRepo {
    pub submitter: gt_core::identity::PeerId,
    pub path: std::path::PathBuf,
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_ACTIVE_JOBS_PER_SUBMITTER: usize = 32;

async fn limit_mw(
    State(limiter): State<Arc<crate::ratelimit::RateLimiter>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    let is_write = !matches!(req.method(), &Method::GET | &Method::HEAD);
    match limiter.check(addr.ip(), is_write) {
        Ok(()) => next.run(req).await,
        Err(retry_after) => (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, retry_after.to_string())],
            "rate limit exceeded",
        )
            .into_response(),
    }
}

async fn timeout_mw(req: axum::extract::Request, next: Next) -> Response {
    match tokio::time::timeout(REQUEST_TIMEOUT, next.run(req)).await {
        Ok(res) => res,
        Err(_) => (StatusCode::REQUEST_TIMEOUT, "request timed out").into_response(),
    }
}

fn register_payment_backends(config: &Config, manager: &mut PaymentManager) {
    if let Some(lnd) = &config.payments.lnd {
        match (
            std::fs::read(&lnd.macaroon_path),
            std::fs::read(&lnd.tls_cert_path),
        ) {
            (Ok(macaroon), Ok(tls_cert)) => {
                let mut params = gt_compute::lightning::LndConfig::new(
                    lnd.grpc_url.clone(),
                    hex::encode(macaroon),
                );
                params.tls_cert_pem = tls_cert;
                match gt_compute::lightning::LndBackend::new(params) {
                    Ok(backend) => {
                        let backend =
                            backend.with_store_path(config.data_dir.join("lnd_escrows.json"));
                        manager.register_backend(Box::new(backend));
                        tracing::info!("Lightning (LND) payment backend enabled");
                    }
                    Err(e) => tracing::warn!("Skipping LND backend: {e}"),
                }
            }
            (Err(e), _) => tracing::warn!(
                "Skipping LND backend: cannot read {}: {e}",
                lnd.macaroon_path.display()
            ),
            (_, Err(e)) => tracing::warn!(
                "Skipping LND backend: cannot read {}: {e}",
                lnd.tls_cert_path.display()
            ),
        }
    }
    for evm in &config.payments.evm {
        let network = match evm.network.parse::<ChainType>() {
            Ok(ChainType::EthereumL2(net)) => net,
            _ => {
                tracing::warn!("Skipping EVM backend: unknown network '{}'", evm.network);
                continue;
            }
        };
        let signer = match gt_compute::evm::build_signer(
            &evm.from_address,
            &evm.signer_command,
            evm.keystore_path.as_deref(),
            evm.key_password_file.as_deref(),
        ) {
            Ok(signer) => signer,
            Err(e) => {
                tracing::warn!("Skipping EVM backend: {e}");
                continue;
            }
        };
        let store_path = config.data_dir.join(format!("evm_escrows_{network}.json"));
        match gt_compute::evm::EvmL2Backend::new(network, evm.rpc_url.clone(), signer) {
            Ok(backend) => {
                let backend = backend.with_store_path(store_path);
                manager.register_backend(Box::new(backend));
                tracing::info!("EVM ({}) payment backend enabled", evm.network);
            }
            Err(e) => tracing::warn!("Skipping EVM backend: {e}"),
        }
    }
}

pub fn persist_trust(state: &AppState) {
    let _ = state
        .worker_registry
        .save(&state.config.data_dir.join("workers.json"));
    let _ = state
        .submitter_registry
        .save(&state.config.data_dir.join("submitters.json"));
}

pub fn persist_credit(state: &AppState) {
    let _ = state
        .credit_ledger
        .save(&state.config.data_dir.join("mutual_credit.json"));
}

async fn record_taken_if_unpaid(
    state: &Arc<RwLock<AppState>>,
    job_id: &str,
    worker_id: &PeerId,
    execution_time_ms: u64,
) {
    let mut s = state.write().await;
    let released = matches!(
        s.payment_manager.for_job(job_id).map(|c| &c.status),
        Some(gt_compute::payment::PaymentStatus::Released)
    );
    if s.credit_ledger.paid_for(job_id, worker_id) || released {
        return;
    }
    let units = gt_compute::credit::compute_units(execution_time_ms);
    s.credit_ledger.record_taken(worker_id, units);
    persist_credit(&s);
}

fn active_job_count_for(store: &gt_compute::job::ComputeJobStore, submitter: &PeerId) -> usize {
    store
        .list()
        .iter()
        .filter(|j| {
            &j.submitter == submitter
                && matches!(
                    j.status,
                    JobStatus::Pending
                        | JobStatus::Blocked { .. }
                        | JobStatus::Matched { .. }
                        | JobStatus::Verifying { .. }
                        | JobStatus::Running { .. }
                )
        })
        .count()
}

pub async fn start_api(
    config: Config,
    config_path: std::path::PathBuf,
    identity: Identity,
    identity_path: std::path::PathBuf,
    mut dht: DhtNode,
) -> anyhow::Result<()> {
    let api_port = config.api_port;
    let git_port = config
        .git_port()
        .ok_or_else(|| anyhow::anyhow!("p2p_port {} has no adjacent git port", config.p2p_port))?;

    let sender = dht.clone_sender();
    let state = Arc::new(RwLock::new(AppState::new(
        config,
        config_path,
        identity,
        identity_path,
        sender,
    )));

    // Spawn DHT event consumer to populate known_peers from libp2p discovery.
    let event_state = state.clone();
    tokio::spawn(async move {
        consume_dht_events(&mut dht, event_state).await;
    });

    // Seed known_peers from bootstrap peers synchronously before serving API.
    // This lets the remote proxy work immediately for configured peers without
    // waiting for mDNS/DHT discovery (which is unreliable in CI sandboxes).
    seed_bootstrap_peers(state.clone()).await;

    // Sample peer counts for /api/analytics/history.
    let analytics_state = state.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(ANALYTICS_SAMPLE_INTERVAL_SECS));
        tick.tick().await;
        loop {
            {
                let mut s = analytics_state.write().await;
                let peers = s.known_peers.len() as u64;
                record_analytics_sample(&mut s.analytics_history, peers);
            }
            tick.tick().await;
        }
    });

    let app = Router::new()
        // Identity
        .route("/api/id", get(get_id))
        .route("/api/profile", get(get_profile).post(update_profile))
        .route("/api/identity/export", post(export_identity))
        .route("/api/identity/import", post(import_identity))
        // Repos
        .route("/api/repos", get(list_repos).post(add_repo))
        .route("/api/repos/clone", post(clone_repo))
        .route("/api/repos/{name}", get(get_repo).delete(remove_repo))
        .route("/api/repos/{name}/refs", get(get_repo_refs))
        .route("/api/repos/{name}/branches", get(get_repo_branches))
        .route("/api/repos/{name}/log", get(get_repo_log))
        // Network
        .route("/api/status", get(get_status))
        .route("/api/analytics/summary", get(get_analytics_summary))
        .route("/api/analytics/history", get(get_analytics_history))
        .route("/api/peers", get(get_peers))
        .route("/api/peers/exchange", get(peer_exchange))
        // Collaboration
        .route(
            "/api/repos/{name}/patches",
            get(list_patches).post(submit_patch),
        )
        .route(
            "/api/repos/{name}/merge-requests",
            get(list_merge_requests).post(submit_merge_request),
        )
        .route(
            "/api/repos/{name}/merge-requests/{id}/accept",
            post(accept_merge_request),
        )
        .route(
            "/api/repos/{name}/merge-requests/{id}/reject",
            post(reject_merge_request),
        )
        .route("/api/repos/{name}/acl", get(get_acl).post(update_acl))
        .route("/api/repos/{name}/ci-config", get(get_ci_config))
        // Compute marketplace
        .route(
            "/api/compute/jobs",
            get(list_compute_jobs).post(submit_compute_job),
        )
        .route("/api/compute/jobs/{id}", get(get_compute_job))
        .route(
            "/api/compute/jobs/{id}/artifacts/{*name}",
            get(download_job_artifact),
        )
        .route("/api/compute/jobs/{id}/cancel", post(cancel_own_job))
        .route("/api/compute/jobs/{id}/approve", post(approve_own_job))
        .route("/api/compute/jobs/{id}/reject", post(reject_own_job))
        .route("/api/caches", post(save_cache).get(find_cache))
        .route("/api/caches/list", get(list_caches))
        .route("/api/caches/{key}", get(get_cache).delete(delete_cache))
        .route("/api/compute/offers", get(list_compute_offers))
        .route("/api/compute/worker", get(get_worker_status))
        .route("/api/compute/worker/start", post(start_worker))
        .route("/api/compute/worker/stop", post(stop_worker))
        .route("/api/compute/remote/submit", post(remote_submit_job))
        .route("/api/compute/remote/result", post(remote_job_result))
        .route("/api/compute/remote/cancel", post(remote_cancel_job))
        .route("/api/compute/payments", get(list_payments))
        .route("/api/compute/trust", get(get_trust))
        .route("/api/compute/credit", get(get_credit))
        .route(
            "/api/repos/{name}/secrets",
            get(list_secrets).post(set_secret),
        )
        .route("/api/repos/{name}/secrets/{key}", delete(delete_secret))
        .route("/api/repos/{name}/workflows", get(list_workflows))
        .route(
            "/api/repos/{name}/workflows/dispatch",
            post(dispatch_workflow),
        )
        .route("/api/workflows/runs", get(list_workflow_runs))
        .route("/api/workflows/runs/{id}", get(get_workflow_run))
        .route("/api/workflows/runs/{id}/rerun", post(rerun_workflow_run))
        // Large file storage
        .route("/api/blobs", get(list_blobs).post(upload_blob))
        .route(
            "/api/blobs/upload",
            post(upload_blob_bytes).route_layer(axum::extract::DefaultBodyLimit::disable()),
        )
        .route("/api/blobs/{id}", get(get_blob_info).delete(remove_blob))
        .route("/api/blobs/{id}/download", get(download_blob))
        .route("/api/blobs/{id}/meta", get(get_blob_meta))
        .route("/api/blobs/{id}/fetch", post(fetch_blob))
        .route(
            "/api/collections",
            get(list_collections).post(create_collection),
        )
        .route("/api/collections/{id}", get(get_collection))
        .route("/api/collections/{id}/fetch", post(fetch_collection))
        .route("/api/models/publish", post(publish_model))
        .route("/api/models/search", get(search_models))
        .route("/api/models/{id}", get(get_model))
        .route("/api/models/{id}/pull", post(pull_model))
        .route("/api/models/{id}/infer", post(infer_model))
        .route("/api/models/{id}/infer/stream", post(infer_model_stream))
        .route(
            "/api/models/{id}/acl",
            get(get_model_acl).post(update_model_acl),
        )
        // Transfers
        .route("/api/transfers", get(list_transfers))
        .route("/api/transfers/{blob_id}", get(get_transfer_status))
        .route("/api/blobs/{id}/pieces/{index}", get(serve_piece))
        // Peer name resolution
        .route("/api/resolve/{identifier}", get(resolve_peer))
        // Remote helper endpoints
        .route("/api/remote/{peer_id}/{repo}/refs", get(remote_list_refs))
        .route("/api/remote/{peer_id}/{repo}/fetch", post(remote_fetch))
        .route("/api/remote/{peer_id}/{repo}/push", post(remote_push))
        // Web UI
        .fallback(get(serve_ui))
        // Inner layer first: outer rate-limit rejects cheaply, then the
        // timeout bounds handler time. Layers run last-added-first.
        .layer(middleware::from_fn(timeout_mw))
        .layer(middleware::from_fn_with_state(
            Arc::new(crate::ratelimit::RateLimiter::new()),
            limit_mw,
        ))
        .with_state(state.clone());

    // Start peer gossip — query bootstrap peers for their known peers
    let gossip_state = state.clone();
    tokio::spawn(async move {
        start_peer_gossip(gossip_state).await;
    });

    // Start blob fetch loop — downloads wanted blobs from discovered seeders
    let fetch_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            crate::fetch::run_fetch_tick(fetch_state.clone()).await;
        }
    });

    // Start compute dispatch loop — runs pending jobs when worker mode is on
    let dispatch_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(10));
        loop {
            interval.tick().await;
            crate::dispatch::run_dispatch_tick(dispatch_state.clone()).await;
        }
    });

    // Start remote submit loop — sends pending jobs to network workers
    let submit_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(15));
        loop {
            interval.tick().await;
            crate::dispatch::run_submit_tick(submit_state.clone()).await;
        }
    });

    // Start schedule loop — fires cron-triggered workflows once per minute slot
    let schedule_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(30));
        loop {
            interval.tick().await;
            run_schedule_tick(schedule_state.clone()).await;
        }
    });

    // Start git protocol server in background
    let git_state = state.clone();
    tokio::spawn(async move {
        if let Err(e) = crate::network::start_git_server(git_state, git_port).await {
            tracing::error!("Git server failed: {e}");
        }
    });

    let listener = tokio::net::TcpListener::bind(format!("0.0.0.0:{api_port}")).await?;
    info!("API listening on http://0.0.0.0:{api_port}");
    info!("Git protocol server on port {git_port}");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;

    Ok(())
}

/// Periodically query bootstrap peers for their known peers and merge them into our list.
async fn start_peer_gossip(state: Arc<RwLock<AppState>>) {
    let client = reqwest::Client::builder()
        .no_proxy()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());

    async fn exchange(client: &reqwest::Client, state: Arc<RwLock<AppState>>) -> bool {
        let (bootstrap_peers, our_api_addr) = {
            let state = state.read().await;
            (
                state.config.bootstrap_peers.clone(),
                our_api_multiaddr(&state.config, &state.identity),
            )
        };

        let mut any_success = false;
        for peer in &bootstrap_peers {
            let Some((addr, api_port)) = parse_peer_multiaddr(peer) else {
                tracing::debug!("Could not parse bootstrap peer address: {peer}");
                continue;
            };
            let url = format!("http://{addr}:{api_port}/api/peers/exchange");
            match client
                .get(&url)
                .timeout(std::time::Duration::from_secs(10))
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let Ok(body) = resp.bytes().await else {
                        continue;
                    };
                    let exchange: PeerExchangeResponse =
                        match serde_json::from_slice::<PeerExchangeResponse>(&body) {
                            Ok(exchange) => exchange,
                            Err(_) => match serde_json::from_slice::<Vec<String>>(&body) {
                                Ok(peers) => PeerExchangeResponse {
                                    peer_id: String::new(),
                                    peers,
                                    caches: Vec::new(),
                                },
                                Err(_) => continue,
                            },
                        };
                    {
                        let mut state = state.write().await;
                        let app: &mut AppState = &mut state;
                        for p in &exchange.peers {
                            let Some(canon) = canonical_peer_entry(p) else {
                                continue;
                            };
                            if p != peer
                                && push_known_peer(
                                    &mut app.known_peers,
                                    &mut app.peer_last_seen,
                                    canon.clone(),
                                    false,
                                )
                            {
                                app.gossip_learned.insert(canon);
                            }
                        }
                        if let Some(tagged) = tag_bootstrap_addr(peer, &exchange.peer_id) {
                            if let Some(canon) = canonical_peer_entry(&tagged) {
                                push_known_peer(
                                    &mut app.known_peers,
                                    &mut app.peer_last_seen,
                                    canon.clone(),
                                    true,
                                );
                                app.gossip_learned.remove(&canon);
                            }
                        }
                        // Tell the bootstrap about ourselves so it can share our address with others.
                        if let Some(ref our_addr) = our_api_addr {
                            if let Some(canon) = canonical_peer_entry(our_addr) {
                                push_known_peer(
                                    &mut app.known_peers,
                                    &mut app.peer_last_seen,
                                    canon.clone(),
                                    true,
                                );
                                app.gossip_learned.remove(&canon);
                            }
                        }
                        let now = std::time::Instant::now();
                        for ann in &exchange.caches {
                            state.remote_caches.merge_announcement(ann, now);
                        }
                        state.remote_caches.prune_expired_at(now);
                    }
                    any_success = true;
                }
                Ok(resp) => {
                    tracing::warn!(
                        "Peer exchange from {} returned {}",
                        sanitize_log(&url),
                        resp.status()
                    );
                }
                Err(e) => {
                    tracing::debug!(
                        "Peer exchange failed for {}: {}",
                        sanitize_log(&url),
                        sanitize_log(&e.to_string())
                    );
                }
            }
        }
        any_success
    }

    // Exchange once immediately so newly-started nodes learn peers quickly.
    // While no bootstrap has answered yet (it may still be starting), retry
    // every 2s; once linked, drop back to the 30s background cadence.
    let mut linked = exchange(&client, state.clone()).await;

    let mut last_redial =
        std::time::Instant::now() - std::time::Duration::from_secs(REDIAL_INTERVAL_SECS);
    loop {
        let pause = if linked { 30 } else { 2 };
        tokio::time::sleep(std::time::Duration::from_secs(pause)).await;
        linked = exchange(&client, state.clone()).await;
        let mut guard = state.write().await;
        prune_stale_known_peers(&mut guard);
        drop(guard);
        if last_redial.elapsed() >= std::time::Duration::from_secs(REDIAL_INTERVAL_SECS) {
            last_redial = std::time::Instant::now();
            let (dht, config) = {
                let s = state.read().await;
                (s.dht.clone(), s.config.clone())
            };
            crate::network::ensure_dht_mesh(&dht, &config).await;
        }
    }
}

/// Decide whether a newly discovered GitForest peer deserves a DHT compute
/// offer lookup. Never queries self, and throttles repeat discoveries of
/// the same peer to one lookup per 15 minutes so mDNS/identify
/// rediscovery storms cannot spam the DHT.
fn offer_query_due(
    queried: &mut std::collections::HashMap<String, std::time::Instant>,
    self_id: &gt_core::identity::PeerId,
    worker: &gt_core::identity::PeerId,
) -> bool {
    if worker == self_id {
        return false;
    }
    let key = worker.to_string();
    let now = std::time::Instant::now();
    match queried.get(&key) {
        Some(last) if now.duration_since(*last) < std::time::Duration::from_secs(900) => false,
        _ => {
            if queried.len() >= 4096 {
                queried.clear();
            }
            queried.insert(key, now);
            true
        }
    }
}

/// Consume DHT events and add discovered peer addresses to known_peers.
/// If the identify protocol has provided a GitForest peer ID, tag the multiaddr
/// with `/gf/{gitforest_peer_id}` so the remote API can resolve by GitForest ID.
async fn consume_dht_events(dht: &mut DhtNode, state: Arc<RwLock<AppState>>) {
    let mut offer_queries: std::collections::HashMap<String, std::time::Instant> =
        std::collections::HashMap::new();
    loop {
        match dht.recv().await {
            Some(DhtEvent::PeerDiscovered {
                peer_id,
                addresses,
                gitforest_peer_id,
                api_port,
            }) => {
                let self_id = state.read().await.identity.peer_id();
                {
                    let mut state = state.write().await;
                    for addr in addresses {
                        let mut entry = format!("{addr}");
                        if !entry.contains("/p2p/") {
                            entry.push_str(&format!("/p2p/{peer_id}"));
                        }
                        if let Some(ref gf_id) = gitforest_peer_id {
                            if !entry.contains("/gf/") {
                                entry.push_str(&format!("/gf/{gf_id}"));
                            }
                        }
                        if let Some(port) = api_port {
                            if !entry.contains("/api/") {
                                entry.push_str(&format!("/api/{port}"));
                            }
                        }
                        let Some(entry) = canonical_peer_entry(&entry) else {
                            continue;
                        };
                        let app: &mut AppState = &mut state;
                        let fresh = push_known_peer(
                            &mut app.known_peers,
                            &mut app.peer_last_seen,
                            entry.clone(),
                            true,
                        );
                        app.gossip_learned.remove(&entry);
                        if fresh {
                            tracing::info!(
                                "DHT discovered peer: {peer_id} (gf={gitforest_peer_id:?}) at {}",
                                sanitize_log(&addr.to_string())
                            );
                        }
                    }
                }
                if let Some(gf_id) = gitforest_peer_id {
                    if let Ok(worker) = gf_id.parse::<gt_core::identity::PeerId>() {
                        if offer_query_due(&mut offer_queries, &self_id, &worker) {
                            let address = gt_compute::offer::compute_offer_address(&worker);
                            match dht.lookup_compute(address).await {
                                Ok(()) => tracing::info!("querying DHT compute offer for {gf_id}"),
                                Err(e) => {
                                    tracing::warn!(
                                        "compute offer lookup for {gf_id} failed: {}",
                                        sanitize_log(&e.to_string())
                                    )
                                }
                            }
                        }
                    }
                }
            }
            Some(DhtEvent::RepoFound(_resp)) => {
                // Could populate a repo cache here; for now peer addresses are enough.
            }
            Some(DhtEvent::BlobFound { data, .. }) => {
                if let Ok(ann) =
                    serde_json::from_slice::<gt_core::protocol::BlobAnnouncement>(&data)
                {
                    if ann.verify() {
                        let mut state = state.write().await;
                        if ann.announces_complete() {
                            state.blob_tracker.add_seeder(ann.blob_id, ann.peer_id);
                        } else {
                            state.blob_tracker.add_leecher(ann.blob_id, ann.peer_id);
                        }
                    }
                }
            }
            Some(DhtEvent::CollectionFound { data, .. }) => {
                if let Ok(ann) =
                    serde_json::from_slice::<gt_core::protocol::CollectionAnnouncement>(&data)
                {
                    if ann.verify() {
                        let mut state = state.write().await;
                        let entry = state.collection_seeders.entry(ann.manifest_id).or_default();
                        if !entry.contains(&ann.owner) {
                            entry.push(ann.owner);
                        }
                        continue;
                    }
                }
                if let Ok(ann) =
                    serde_json::from_slice::<gt_core::protocol::ModelAnnouncement>(&data)
                {
                    if let Some(record) = crate::models::ModelRecord::from_announcement(&ann) {
                        let mut state = state.write().await;
                        let seeder = ann.seeder.clone().unwrap_or_else(|| record.owner.clone());
                        state
                            .known_models
                            .entry(record.manifest_id.clone())
                            .and_modify(|r| r.merge_seeder(seeder.clone()))
                            .or_insert(record);
                        let entry = state
                            .collection_seeders
                            .entry(ann.manifest_id.clone())
                            .or_default();
                        if !entry.contains(&seeder) {
                            entry.push(seeder.clone());
                        }
                        tracing::info!(
                            "Model {} learned seeder {}",
                            sanitize_log(&ann.manifest_id),
                            sanitize_log(&seeder.to_string())
                        );
                        continue;
                    }
                }
                if let Ok(ann) =
                    serde_json::from_slice::<gt_core::protocol::CacheAnnouncement>(&data)
                {
                    let mut state = state.write().await;
                    let now = std::time::Instant::now();
                    state.remote_caches.merge_announcement(&ann, now);
                    state.remote_caches.prune_expired_at(now);
                }
            }
            Some(DhtEvent::ComputeOfferFound(msg)) => {
                let min_pow_bits = {
                    let state = state.read().await;
                    state
                        .config
                        .compute
                        .as_ref()
                        .map(|c| c.min_pow_bits)
                        .unwrap_or(gt_core::identity::MIN_POW_BITS)
                };
                if let Some(offer) =
                    gt_compute::offer::ComputeOffer::from_message_with_bits(&msg, min_pow_bits)
                {
                    let mut state = state.write().await;
                    if msg.worker_id != state.identity.peer_id() {
                        let bits = gt_core::identity::pow_bits(&msg.worker_id, msg.pow_nonce)
                            .min(u8::MAX as u32) as u8;
                        state.worker_registry.register_with_proof(offer, bits);
                        state.worker_registry.prune_expired();
                        tracing::info!("DHT discovered compute offer from {}", msg.worker_id);
                    }
                }
            }
            Some(_) | None => {
                // Channel closed or unhandled event
            }
        }
    }
}

/// Parse a peer multiaddr to extract the host (IP or DNS name) and API port.
/// Supports `/ip4/{ip}/tcp/...`, `/dns4/{host}/tcp/...`, `/dns/{host}/tcp/...`,
/// plus an optional trailing `/api/{port}`. Defaults API port to 7878.
pub(crate) const MAX_KNOWN_PEERS: usize = 1024;

pub(crate) const PEER_ENTRY_TTL: Duration = Duration::from_secs(1800);
pub(crate) const REDIAL_INTERVAL_SECS: u64 = 60;

pub(crate) fn push_known_peer(
    known: &mut Vec<String>,
    seen: &mut HashMap<String, Instant>,
    entry: String,
    refresh: bool,
) -> bool {
    let Some(incoming) = split_peer_entry(&entry) else {
        return false;
    };
    if host_is_unspecified(&incoming.host) {
        return false;
    }
    if let Some(pos) = known.iter().position(|existing| {
        split_peer_entry(existing).is_some_and(|parts| {
            parts.host.eq_ignore_ascii_case(&incoming.host) && parts.tcp_port == incoming.tcp_port
        })
    }) {
        let mut merged = split_peer_entry(&known[pos]).expect("merge target parses");
        if merged.p2p_id.is_none() {
            merged.p2p_id = incoming.p2p_id;
        }
        if merged.gf_id.is_none() {
            merged.gf_id = incoming.gf_id;
        }
        if merged.api_port.is_none() {
            merged.api_port = incoming.api_port;
        }
        let rebuilt = join_peer_entry(&merged);
        if rebuilt != known[pos] {
            seen.remove(&known[pos]);
            known[pos] = rebuilt.clone();
        }
        if refresh {
            seen.insert(rebuilt, Instant::now());
        } else {
            seen.entry(rebuilt).or_insert_with(Instant::now);
        }
        return false;
    }
    let canonical = join_peer_entry(&incoming);
    seen.insert(canonical.clone(), Instant::now());
    if known.len() >= MAX_KNOWN_PEERS {
        if let Some(evicted) = known.first().cloned() {
            seen.remove(&evicted);
        }
        known.remove(0);
    }
    known.push(canonical);
    true
}

pub(crate) fn parse_peer_multiaddr(peer: &str) -> Option<(&str, u16)> {
    let parts: Vec<&str> = peer.split('/').collect();
    let host_idx = parts
        .iter()
        .position(|&s| s == "ip4" || s == "ip6" || s == "dns4" || s == "dns")?;
    let host = parts.get(host_idx + 1)?;
    let api_port = parts
        .iter()
        .position(|&s| s == "api")
        .and_then(|idx| parts.get(idx + 1))
        .and_then(|s| s.parse().ok())
        .unwrap_or(7878);
    Some((host, api_port))
}

pub(crate) struct SplitPeerEntry {
    pub proto: String,
    pub host: String,
    pub tcp_port: u16,
    pub p2p_id: Option<String>,
    pub gf_id: Option<String>,
    pub api_port: Option<u16>,
}

pub(crate) fn split_peer_entry(entry: &str) -> Option<SplitPeerEntry> {
    let parts: Vec<&str> = entry.split('/').collect();
    let host_idx = parts
        .iter()
        .position(|&s| s == "ip4" || s == "ip6" || s == "dns4" || s == "dns")?;
    let proto = parts.get(host_idx)?.to_string();
    let host = parts.get(host_idx + 1)?.to_string();
    if host.is_empty() {
        return None;
    }
    let tcp_idx = parts.iter().position(|&s| s == "tcp")?;
    let tcp_port: u16 = parts.get(tcp_idx + 1)?.parse().ok()?;
    let mut p2p_id = None;
    let mut gf_id = None;
    let mut api_port = None;
    let mut idx = 0;
    while idx < parts.len() {
        match parts[idx] {
            "p2p" => {
                if let Some(id) = parts.get(idx + 1).filter(|s| !s.is_empty()) {
                    p2p_id.get_or_insert_with(|| id.to_string());
                }
                idx += 1;
            }
            "gf" => {
                if let Some(id) = parts.get(idx + 1).filter(|s| !s.is_empty()) {
                    gf_id.get_or_insert_with(|| id.to_string());
                }
                idx += 1;
            }
            "api" => {
                if api_port.is_none() {
                    api_port = parts.get(idx + 1).and_then(|s| s.parse().ok());
                }
                idx += 1;
            }
            _ => {}
        }
        idx += 1;
    }
    Some(SplitPeerEntry {
        proto,
        host,
        tcp_port,
        p2p_id,
        gf_id,
        api_port,
    })
}

pub(crate) fn join_peer_entry(parts: &SplitPeerEntry) -> String {
    let mut entry = format!("/{}/{}/tcp/{}", parts.proto, parts.host, parts.tcp_port);
    if let Some(ref id) = parts.p2p_id {
        entry.push_str(&format!("/p2p/{id}"));
    }
    if let Some(ref id) = parts.gf_id {
        entry.push_str(&format!("/gf/{id}"));
    }
    if let Some(port) = parts.api_port {
        entry.push_str(&format!("/api/{port}"));
    }
    entry
}

pub(crate) fn canonical_peer_entry(raw: &str) -> Option<String> {
    split_peer_entry(raw).map(|parts| join_peer_entry(&parts))
}

/// True for addresses that point back to the local machine. Loopback
/// addresses are valid when peers run in the same process/host, but
/// they are not reachable across independent Docker containers.
fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1" || host.eq_ignore_ascii_case("localhost")
}

fn entry_host(entry: &str) -> Option<&str> {
    parse_peer_multiaddr(entry).map(|(host, _)| host)
}

fn host_is_unspecified(host: &str) -> bool {
    host.parse::<std::net::IpAddr>()
        .is_ok_and(|ip| ip.is_unspecified())
}

/// True when a numeric host can only be reached from nearby networks
/// (loopback, private ranges, link-local, reserved). DNS names are kept:
/// their routability cannot be judged without resolving them.
fn host_is_nonroutable(host: &str) -> bool {
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    match ip {
        std::net::IpAddr::V4(v4) => {
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_documentation()
        }
        std::net::IpAddr::V6(v6) => v6.is_loopback() || v6.is_unspecified() || v6.is_unique_local(),
    }
}

/// Entries worth sharing with a requester at `remote_ip`. Entries with no
/// dialable host and unspecified addresses are dropped for everyone.
/// Loopback requesters (local dev, tests, demos) see the rest; LAN
/// requesters additionally lose loopback entries; internet requesters only
/// see globally routable entries and DNS names.
fn exchange_visible_peers(
    known: &[String],
    gossip: &HashSet<String>,
    remote_ip: std::net::IpAddr,
) -> Vec<String> {
    let requester_loopback = remote_ip.is_loopback();
    let requester_private = match remote_ip {
        std::net::IpAddr::V4(v4) => v4.is_private() || v4.is_link_local(),
        std::net::IpAddr::V6(v6) => v6.is_unique_local(),
    };
    known
        .iter()
        .filter(|entry| {
            if gossip.contains(entry.as_str()) {
                return false;
            }
            let Some(host) = entry_host(entry) else {
                return false;
            };
            if host_is_unspecified(host) {
                return false;
            }
            if requester_loopback {
                return true;
            }
            if is_loopback_host(host) {
                return false;
            }
            if requester_private {
                return true;
            }
            !host_is_nonroutable(host)
        })
        .cloned()
        .collect()
}

/// Score a discovered address for reachability. Higher is better.
fn score_peer_addr(host: &str, p2p_port: u16, _api_port: u16) -> i32 {
    let mut score = 0;
    if !is_loopback_host(host) {
        score += 100;
    }
    // Prefer public routable addresses over private LAN/Docker ranges.
    // Bootstrap nodes advertise their external IP, so that should win.
    let is_private =
        host.starts_with("10.") || host.starts_with("172.") || host.starts_with("192.168.");
    if !is_private {
        score += 50;
    }
    // Prefer the well-known GitForest P2P listen port (9418) and avoid ephemeral source ports.
    if p2p_port == 9418 {
        score += 30;
    } else if p2p_port > 0 && p2p_port < 32768 {
        score += 10;
    }
    score
}

fn our_api_multiaddr(config: &Config, identity: &Identity) -> Option<String> {
    config.advertise_addr.as_ref().map(|addr| {
        let peer_id = identity.peer_id().to_string();
        if addr.ends_with('/') {
            format!("{addr}gf/{peer_id}")
        } else {
            format!("{addr}/gf/{peer_id}")
        }
    })
}

/// Probe each configured bootstrap peer and add it to known_peers with its
/// GitForest peer ID tag so the remote API proxy can resolve it immediately.
async fn seed_bootstrap_peers(state: Arc<RwLock<AppState>>) {
    let (bootstrap_peers, client) = {
        let state = state.read().await;
        (
            state.config.bootstrap_peers.clone(),
            reqwest::Client::builder()
                .no_proxy()
                .timeout(std::time::Duration::from_secs(1))
                .build()
                .unwrap_or_default(),
        )
    };

    for peer in bootstrap_peers {
        let Some((host, api_port)) = parse_peer_multiaddr(&peer) else {
            tracing::debug!("Could not parse bootstrap peer address: {peer}");
            continue;
        };

        // Retry a few times in case the peer is still starting up.
        // Keep per-peer probing short (<= ~3s) so a slow or unreachable
        // bootstrap peer does not block the API from binding in CI.
        let mut peer_id = None;
        for attempt in 0..3 {
            let url = format!("http://{host}:{api_port}/api/id");
            match client.get(&url).send().await {
                Ok(resp) if resp.status().is_success() => {
                    if let Ok(body) = resp.json::<serde_json::Value>().await {
                        if let Some(pid) = body
                            .get("peer_id")
                            .and_then(|v| v.as_str())
                            .filter(|s| s.len() == 64 && s.chars().all(|c| c.is_ascii_hexdigit()))
                        {
                            peer_id = Some(pid.to_string());
                            break;
                        }
                    }
                }
                Ok(resp) => {
                    tracing::debug!("Bootstrap {url} returned {}", resp.status());
                }
                Err(e) => {
                    tracing::debug!("Bootstrap {url} unreachable on attempt {attempt}: {e}");
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }

        let Some(peer_id) = peer_id else {
            tracing::warn!("Could not resolve GitForest peer ID for bootstrap peer {peer}");
            continue;
        };

        let mut entry = peer.clone();
        if !entry.ends_with('/') && !entry.contains("/gf/") {
            entry.push('/');
        }
        if !entry.contains("/gf/") {
            entry.push_str(&format!("gf/{peer_id}"));
        }
        let Some(entry) = canonical_peer_entry(&entry) else {
            continue;
        };

        let mut state = state.write().await;
        let app: &mut AppState = &mut state;
        if push_known_peer(
            &mut app.known_peers,
            &mut app.peer_last_seen,
            entry.clone(),
            true,
        ) {
            tracing::info!("Seeded bootstrap peer {peer} as {entry}");
        }
        app.gossip_learned.remove(&entry);
    }
}

// ============ Identity ============

async fn get_profile(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let peer_id = state.identity.peer_id();
    let display_name = gt_core::identity::load_display_name();
    let peer_name = display_name
        .clone()
        .map(|name| gt_core::identity::PeerName::new(name, peer_id.clone()).short_tag());
    Json(serde_json::json!({
        "peer_id": peer_id.to_string(),
        "display_name": display_name,
        "peer_name": peer_name,
    }))
}

#[derive(Deserialize)]
struct UpdateProfileRequest {
    #[serde(default)]
    display_name: String,
}

async fn update_profile(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<UpdateProfileRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    let name = req.display_name.trim();
    if name.is_empty() || name.len() > 32 || name.chars().any(|c| c.is_control()) {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    gt_core::identity::save_display_name(name).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let peer_id = state.identity.peer_id();
    let peer_name = gt_core::identity::PeerName::new(name.to_string(), peer_id.clone()).short_tag();
    Ok(Json(serde_json::json!({
        "peer_id": peer_id.to_string(),
        "display_name": name,
        "peer_name": peer_name,
    })))
}

#[derive(Deserialize)]
struct CloneRepoRequest {
    #[serde(default)]
    url: String,
    #[serde(default)]
    dest: String,
}

fn clone_target_name(url: &str) -> Option<String> {
    let trimmed = url.trim().trim_end_matches('/');
    if trimmed.split('/').any(|c| c == "..") {
        return None;
    }
    let after_scheme = match trimmed.find("://") {
        Some(idx) => &trimmed[idx + 3..],
        None => trimmed,
    };
    let slash = after_scheme.find('/')?;
    let path = &after_scheme[slash + 1..];
    let last = path.trim_end_matches('/').rsplit('/').next()?;
    let name = last.strip_suffix(".git").unwrap_or(last);
    if name.is_empty()
        || name == "."
        || name == ".."
        || name.contains('\0')
        || name.chars().any(|c| c.is_control())
    {
        return None;
    }
    Some(name.to_string())
}

fn clone_url_scheme_ok(url: &str) -> bool {
    ["http://", "https://", "gitforest://", "file://"]
        .iter()
        .any(|prefix| url.trim_start().starts_with(prefix))
}

async fn clone_repo(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CloneRepoRequest>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let state = state.read().await;
    let err = |status: StatusCode, msg: String| (status, Json(serde_json::json!({"error": msg})));
    if let Err(s) = check_local_auth(&headers, &state.api_token) {
        return Err((s, Json(serde_json::json!({"error": "unauthorized"}))));
    }
    if !clone_url_scheme_ok(&req.url) {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "url must use http, https, gitforest, or file scheme".to_string(),
        ));
    }
    let Some(name) = clone_target_name(&req.url) else {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "could not derive a repo name from url".to_string(),
        ));
    };
    let dest = std::path::PathBuf::from(req.dest.trim());
    if !dest.is_dir() {
        return Err(err(
            StatusCode::UNPROCESSABLE_ENTITY,
            "dest must be an existing directory".to_string(),
        ));
    }
    let target = dest.join(&name);
    if target.exists() {
        return Err(err(
            StatusCode::CONFLICT,
            format!("{} already exists", target.display()),
        ));
    }
    let output = tokio::time::timeout(
        Duration::from_secs(600),
        tokio::process::Command::new("git")
            .arg("clone")
            .arg("--")
            .arg(req.url.trim())
            .arg(&target)
            .output(),
    )
    .await
    .map_err(|_| {
        err(
            StatusCode::GATEWAY_TIMEOUT,
            "git clone timed out".to_string(),
        )
    })?
    .map_err(|e| {
        err(
            StatusCode::BAD_GATEWAY,
            format!("git clone failed to start: {e}"),
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail: String = stderr.lines().rev().take(5).collect::<Vec<_>>().join("\n");
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("git clone failed: {tail}"),
        ));
    }
    Ok(Json(serde_json::json!({
        "path": target.to_string_lossy(),
        "url": req.url.trim(),
    })))
}

#[derive(Deserialize)]
struct IdentityExportRequest {
    #[serde(default)]
    path: String,
}

#[derive(Deserialize)]
struct IdentityImportRequest {
    #[serde(default)]
    path: String,
}

async fn export_identity(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<IdentityExportRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    let dest = std::path::PathBuf::from(req.path.trim());
    let parent_ok = dest
        .parent()
        .is_some_and(|p| p.as_os_str().is_empty() || p.is_dir());
    if req.path.trim().is_empty() || !parent_ok {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    if dest.exists() {
        return Err(StatusCode::CONFLICT);
    }
    state
        .identity
        .save(&dest)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "path": dest.to_string_lossy(),
        "peer_id": state.identity.peer_id().to_string(),
    })))
}

async fn import_identity(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<IdentityImportRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    let src = std::path::PathBuf::from(req.path.trim());
    if !src.is_file() {
        return Err(StatusCode::UNPROCESSABLE_ENTITY);
    }
    let imported =
        gt_core::identity::Identity::load(&src).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    if state.identity_path.exists() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let backup = state
            .identity_path
            .with_extension(format!("key.bak-{stamp}"));
        std::fs::copy(&state.identity_path, &backup)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    }
    imported
        .save(&state.identity_path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    Ok(Json(serde_json::json!({
        "peer_id": imported.peer_id().to_string(),
        "previous_peer_id": state.identity.peer_id().to_string(),
        "restart_required": true,
    })))
}

async fn get_id(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let peer_id = state.identity.peer_id();
    let peer_name = gt_core::identity::load_display_name()
        .map(|name| gt_core::identity::PeerName::new(name, peer_id.clone()).short_tag());
    Json(serde_json::json!({
        "peer_id": peer_id.to_string(),
        "peer_name": peer_name,
    }))
}

// ============ Repos ============

async fn list_repos(State(state): State<Arc<RwLock<AppState>>>) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let repos: Vec<serde_json::Value> = state.config.repos.iter().map(repo_to_json).collect();
    Json(repos)
}

async fn get_repo(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(repo_to_json(repo)))
}

/// Verify the `Authorization: Bearer <token>` header matches our API token.
/// Skipped only when `GITFOREST_SKIP_AUTH` is exactly `1` or `true` (tests).
/// Any other value, including empty, keeps auth enforced.
fn check_auth(
    headers: &axum::http::HeaderMap,
    token: &str,
    cluster_token: Option<&str>,
) -> Result<(), StatusCode> {
    let skip = matches!(
        std::env::var("GITFOREST_SKIP_AUTH").as_deref(),
        Ok("1") | Ok("true")
    );
    check_auth_with_skip(headers, token, cluster_token, skip)
}

const SESSION_COOKIE_NAME: &str = "gitforest_session";

fn session_cookie_value(token: &str) -> String {
    format!("{SESSION_COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict")
}

fn request_has_session(headers: &axum::http::HeaderMap, token: &str) -> bool {
    headers
        .get_all("cookie")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(';'))
        .any(|pair| {
            let mut parts = pair.trim().splitn(2, '=');
            parts.next().is_some_and(|n| n == SESSION_COOKIE_NAME)
                && parts.next().is_some_and(|val| val == token)
        })
}

fn check_auth_with_skip(
    headers: &axum::http::HeaderMap,
    token: &str,
    cluster_token: Option<&str>,
    skip: bool,
) -> Result<(), StatusCode> {
    if skip {
        return Ok(());
    }
    let bearer = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if bearer == token {
        return Ok(());
    }
    if request_has_session(headers, token) {
        return Ok(());
    }
    if let Some(cluster) = cluster_token {
        if !cluster.is_empty() && bearer == cluster {
            return Ok(());
        }
    }
    Err(StatusCode::UNAUTHORIZED)
}

fn check_local_auth(headers: &axum::http::HeaderMap, token: &str) -> Result<(), StatusCode> {
    check_auth(headers, token, None)
}

fn bearer_token(headers: &axum::http::HeaderMap) -> &str {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("")
}

fn bearer_job_permissions(headers: &axum::http::HeaderMap, minter: &PeerId) -> Option<Permissions> {
    let presented = bearer_token(headers);
    if !presented.starts_with(gt_compute::permissions::JOB_TOKEN_PREFIX) {
        return None;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    verify_job_token(presented, minter, now)
        .map(|claims| claims.permissions)
        .ok()
}

fn job_token_permissions(token: &str, minter: &PeerId) -> Permissions {
    if !token.starts_with(gt_compute::permissions::JOB_TOKEN_PREFIX) {
        return Permissions::none();
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    verify_job_token(token, minter, now)
        .map(|claims| claims.permissions)
        .unwrap_or_default()
}

fn check_scoped_auth(
    headers: &axum::http::HeaderMap,
    token: &str,
    cluster_token: Option<&str>,
    identity: &Identity,
    scope: PermissionScope,
    level: PermissionLevel,
) -> Result<(), StatusCode> {
    let skip = matches!(
        std::env::var("GITFOREST_SKIP_AUTH").as_deref(),
        Ok("1") | Ok("true")
    );
    check_scoped_auth_with_skip(headers, token, cluster_token, identity, scope, level, skip)
}

fn check_scoped_auth_with_skip(
    headers: &axum::http::HeaderMap,
    token: &str,
    cluster_token: Option<&str>,
    identity: &Identity,
    scope: PermissionScope,
    level: PermissionLevel,
    skip: bool,
) -> Result<(), StatusCode> {
    if check_auth_with_skip(headers, token, cluster_token, skip).is_ok() {
        return Ok(());
    }
    let presented = bearer_token(headers);
    if !presented.starts_with(gt_compute::permissions::JOB_TOKEN_PREFIX) {
        return Err(StatusCode::UNAUTHORIZED);
    }
    match bearer_job_permissions(headers, &identity.peer_id()) {
        Some(perms) if perms.allows(scope, level) => Ok(()),
        Some(_) => Err(StatusCode::FORBIDDEN),
        None => Err(StatusCode::UNAUTHORIZED),
    }
}

pub(crate) enum ModelCaller {
    Local,
    Peer(gt_core::identity::PeerId),
    Anonymous,
}

pub(crate) fn authenticate_model_caller(
    headers: &axum::http::HeaderMap,
    api_token: &str,
    method: &str,
    path: &str,
    body: &[u8],
) -> ModelCaller {
    let presented = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .unwrap_or("");
    if !presented.is_empty() && presented == api_token {
        return ModelCaller::Local;
    }
    if request_has_session(headers, api_token) {
        return ModelCaller::Local;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let peer = headers
        .get("x-gf-peer")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<gt_core::identity::PeerId>().ok());
    let timestamp = headers
        .get("x-gf-timestamp")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<i64>().ok());
    let signature = headers
        .get("x-gf-signature")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| hex::decode(v).ok());
    match (peer, timestamp, signature) {
        (Some(peer), Some(timestamp), Some(signature))
            if gt_core::identity::verify_peer_call(
                &peer, method, path, timestamp, body, &signature, now,
            ) =>
        {
            ModelCaller::Peer(peer)
        }
        _ => ModelCaller::Anonymous,
    }
}

pub(crate) fn model_accessible(
    acls: &HashMap<String, ModelAcl>,
    model_id: &str,
    caller: &ModelCaller,
) -> bool {
    let Some(acl) = acls.get(model_id) else {
        return true;
    };
    match caller {
        ModelCaller::Local => true,
        ModelCaller::Peer(peer) => acl.can_access(peer),
        ModelCaller::Anonymous => false,
    }
}

pub(crate) fn blob_accessible(
    state: &AppState,
    blob_id: &gt_core::types::BlobId,
    caller: &ModelCaller,
) -> bool {
    if matches!(caller, ModelCaller::Local) {
        return true;
    }
    for manifest in state.collection_store.list() {
        if !manifest.files.iter().any(|f| &f.blob_id == blob_id) {
            continue;
        }
        if !model_accessible(&state.model_acls, &manifest.id, caller) {
            return false;
        }
    }
    true
}

fn restore_model_acls(config: &Config, identity: &Identity) -> HashMap<String, ModelAcl> {
    let mut out = HashMap::new();
    for (model_id, grants) in &config.model_acl_grants {
        let mut acl = ModelAcl::new(identity, model_id);
        for peer in grants {
            let _ = acl.grant(identity, peer.clone());
        }
        out.insert(model_id.clone(), acl);
    }
    out
}

fn valid_remote_id(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 128
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn write_secret_file(path: &std::path::Path, contents: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    let mut file = opts.open(path)?;
    std::io::Write::write_all(&mut file, contents)?;
    #[cfg(unix)]
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[derive(Deserialize)]
struct AddRepoRequest {
    path: String,
    name: Option<String>,
    description: Option<String>,
}

async fn add_repo(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<AddRepoRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    let repo_path = PathBuf::from(&req.path);

    let is_git = repo_path.join("HEAD").exists()
        || repo_path.join(".git").exists()
        || std::process::Command::new("git")
            .args(["rev-parse", "--git-dir"])
            .current_dir(&repo_path)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);

    if !is_git {
        return Err(StatusCode::BAD_REQUEST);
    }

    let repo_name = req.name.unwrap_or_else(|| {
        repo_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "unnamed".into())
            .trim_end_matches(".git")
            .to_string()
    });

    let repo_config = RepoConfig {
        name: repo_name.clone(),
        path: repo_path,
        owner: state.identity.peer_id(),
        description: req.description,
        seeding: true,
    };

    let json = repo_to_json(&repo_config);
    let acl = RepoAcl::new(&state.identity, &repo_name);
    state.config.repos.push(repo_config);
    state.acls.insert(repo_name, acl);

    Ok(Json(json))
}

async fn remove_repo(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
) -> StatusCode {
    let mut state = state.write().await;
    if check_local_auth(&headers, &state.api_token).is_err() {
        return StatusCode::UNAUTHORIZED;
    }
    let before = state.config.repos.len();
    state.config.repos.retain(|r| r.name != name);
    state.acls.remove(&name);
    if state.config.repos.len() < before {
        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

async fn get_repo_refs(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<Vec<(String, String)>>, StatusCode> {
    let state = state.read().await;
    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let refs = git_bridge::list_refs(&repo.path).unwrap_or_default();
    Ok(Json(refs))
}

async fn get_repo_branches(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let state = state.read().await;
    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let output = Command::new("git")
        .args([
            "branch",
            "-a",
            "--format=%(refname:short) %(objectname:short) %(subject)",
        ])
        .current_dir(&repo.path)
        .output()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let branches: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let mut parts = line.splitn(3, ' ');
            let name = parts.next()?;
            let sha = parts.next().unwrap_or("");
            let message = parts.next().unwrap_or("");
            Some(serde_json::json!({
                "name": name,
                "sha": sha,
                "message": message,
            }))
        })
        .collect();

    Ok(Json(branches))
}

async fn get_repo_log(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let state = state.read().await;
    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let output = Command::new("git")
        .args(["log", "--oneline", "-20", "--format=%H|%h|%s|%an|%ar"])
        .current_dir(&repo.path)
        .output()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    let commits: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.splitn(5, '|').collect();
            if parts.len() >= 5 {
                Some(serde_json::json!({
                    "sha": parts[0],
                    "short_sha": parts[1],
                    "message": parts[2],
                    "author": parts[3],
                    "time": parts[4],
                }))
            } else {
                None
            }
        })
        .collect();

    Ok(Json(commits))
}

// ============ Network ============

async fn get_status(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let peer_id = state.identity.peer_id();
    let peer_name = gt_core::identity::load_display_name()
        .map(|name| gt_core::identity::PeerName::new(name, peer_id.clone()).short_tag());
    Json(serde_json::json!({
        "peer_id": peer_id.to_string(),
        "peer_name": peer_name,
        "repos_hosted": state.config.repos.len(),
        "repos_seeding": state.config.repos.iter().filter(|r| r.seeding).count(),
        "p2p_port": state.config.p2p_port,
        "api_port": state.config.api_port,
        "peer_served": state.peer_served,
        "dht_alive": state.dht.is_alive(),
    }))
}

async fn get_peers(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    let connected = match state
        .dht
        .send(gt_dht::node::DhtCommand::GetPeers { reply: reply_tx })
        .await
    {
        Ok(()) => tokio::time::timeout(Duration::from_millis(500), reply_rx)
            .await
            .ok()
            .and_then(|r| r.ok())
            .unwrap_or(0),
        Err(_) => {
            tracing::warn!("DHT command channel dead: swarm dials/announces/lookups are down");
            0
        }
    };
    Json(serde_json::json!({
        "connected": connected,
        "known_addresses": state.known_peers.len(),
        "bootstrap_peers": state.config.bootstrap_peers,
        "discovered_peers": state.known_peers,
        "peers": state
            .known_peers
            .iter()
            .map(|entry| {
                let (peer_id, addr) = peer_entry_parts(entry);
                let p2p_id = peer_entry_p2p_id(entry);
                serde_json::json!({ "peer_id": peer_id, "p2p_id": p2p_id, "addr": addr })
            })
            .collect::<Vec<_>>(),
    }))
}

async fn get_analytics_summary(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Json<serde_json::Value> {
    let state = state.read().await;
    let blobs = state.blob_store.list().unwrap_or_default();
    let blob_bytes = state.blob_store.total_size().unwrap_or(0);
    let jobs = state.job_store.list();
    let mut by_status: HashMap<String, u64> = HashMap::new();
    for job in &jobs {
        *by_status
            .entry(job_status_name(&job.status).to_string())
            .or_insert(0) += 1;
    }
    Json(serde_json::json!({
        "repos_seeding": state.config.repos.iter().filter(|r| r.seeding).count(),
        "repos_hosted": state.config.repos.len(),
        "blob_count": blobs.len(),
        "blob_bytes": blob_bytes,
        "job_count": jobs.len(),
        "jobs_by_status": by_status,
        "peers_now": state.known_peers.len(),
        "peer_served": state.peer_served,
    }))
}

#[derive(Deserialize)]
struct AnalyticsHistoryQuery {
    #[serde(default = "default_history_window")]
    window: String,
}

fn default_history_window() -> String {
    "24h".to_string()
}

async fn get_analytics_history(
    State(state): State<Arc<RwLock<AppState>>>,
    axum::extract::Query(query): axum::extract::Query<AnalyticsHistoryQuery>,
) -> Result<Json<Vec<AnalyticsSample>>, (StatusCode, Json<serde_json::Value>)> {
    let Some(window_ms) = parse_history_window_ms(query.window.trim()) else {
        return Err((
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(serde_json::json!({"error": "window must look like 1h, 24h, or 7d"})),
        ));
    };
    let state = state.read().await;
    let cutoff = now_millis().saturating_sub(window_ms);
    let samples: Vec<AnalyticsSample> = state
        .analytics_history
        .iter()
        .copied()
        .filter(|s| s.t >= cutoff)
        .collect();
    Ok(Json(samples))
}

fn peer_entry_parts(entry: &str) -> (String, String) {
    let mut peer_id = String::new();
    let mut addr = entry.to_string();
    for marker in ["/gf/", "/p2p/"] {
        if let Some(idx) = addr.find(marker) {
            peer_id = addr[idx + marker.len()..].to_string();
            addr.truncate(idx);
            break;
        }
    }
    if let Some(idx) = peer_id.find("/api/") {
        peer_id.truncate(idx);
    }
    if let Some(idx) = peer_id.find("/p2p/") {
        peer_id.truncate(idx);
    }
    if let Some(idx) = peer_id.find("/gf/") {
        peer_id.truncate(idx);
    }
    if let Some(idx) = addr.find("/api/") {
        addr.truncate(idx);
    }
    if let Some(idx) = addr.find("/p2p/") {
        addr.truncate(idx);
    }
    (peer_id, addr.trim_end_matches('/').to_string())
}

fn peer_entry_p2p_id(entry: &str) -> String {
    let Some(idx) = entry.find("/p2p/") else {
        return String::new();
    };
    entry[idx + "/p2p/".len()..]
        .split('/')
        .next()
        .unwrap_or_default()
        .to_string()
}

#[derive(Serialize, Deserialize)]
struct PeerExchangeResponse {
    peer_id: String,
    peers: Vec<String>,
    #[serde(default)]
    caches: Vec<gt_core::protocol::CacheAnnouncement>,
}

pub(crate) const MAX_GOSSIP_CACHES: usize = 512;

pub(crate) fn local_cache_adverts(state: &AppState) -> Vec<gt_core::protocol::CacheAnnouncement> {
    let mut adverts: Vec<gt_core::protocol::CacheAnnouncement> = state
        .cache_store
        .entries()
        .map(|e| {
            let mut ann = gt_core::protocol::CacheAnnouncement {
                owner: state.identity.peer_id(),
                key: e.key.clone(),
                blob_id: e.blob_id.clone(),
                size: e.size,
                updated_at: e.created_at.timestamp(),
                signature: Vec::new(),
            };
            ann.signature = state.identity.sign(&ann.signable_payload());
            ann
        })
        .collect();
    adverts.sort_by(|a, b| a.key.cmp(&b.key));
    adverts.truncate(MAX_GOSSIP_CACHES);
    adverts
}

async fn peer_exchange(
    State(state): State<Arc<RwLock<AppState>>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Json<PeerExchangeResponse> {
    let state = state.read().await;
    Json(PeerExchangeResponse {
        peer_id: state.identity.peer_id().to_string(),
        peers: exchange_visible_peers(&state.known_peers, &state.gossip_learned, addr.ip()),
        caches: local_cache_adverts(&state),
    })
}

/// Drop peer entries unseen for longer than the TTL. Bootstrap peers
/// (matched by host and API port so tagged variants survive) and entries
/// without a timestamp (pinned or test-seeded) are never pruned.
pub(crate) fn prune_stale_known_peers(state: &mut AppState) {
    let now = Instant::now();
    let protected: Vec<(String, u16)> = state
        .config
        .bootstrap_peers
        .iter()
        .filter_map(|b| parse_peer_multiaddr(b).map(|(h, p)| (h.to_string(), p)))
        .collect();
    let stale: Vec<String> = state
        .known_peers
        .iter()
        .filter(|entry| {
            let Some(first_seen) = state.peer_last_seen.get(*entry) else {
                return false;
            };
            if now.duration_since(*first_seen) < PEER_ENTRY_TTL {
                return false;
            }
            let keep = parse_peer_multiaddr(entry).is_some_and(|(host, port)| {
                protected.iter().any(|(ph, pp)| ph == host && *pp == port)
            });
            !keep
        })
        .cloned()
        .collect();
    if stale.is_empty() {
        return;
    }
    state.known_peers.retain(|e| !stale.contains(e));
    for entry in &stale {
        state.peer_last_seen.remove(entry);
        state.gossip_learned.remove(entry);
    }
    tracing::info!("Pruned {} stale peer entries", stale.len());
}

/// Tag a bootstrap multiaddr with the GitForest peer ID that answered there.
///
/// The exchanger reached the responder over this exact address, so the tagged
/// entry is dialable by construction even when the responder has no
/// `advertise_addr` configured. Any stale `/gf/` segment already present is
/// replaced: the live responder is authoritative for who answers at the
/// address. Returns `None` when the responder ID is not a valid peer ID or
/// the bootstrap address has no dialable host part.
fn tag_bootstrap_addr(bootstrap: &str, responder: &str) -> Option<String> {
    responder.parse::<gt_core::identity::PeerId>().ok()?;
    parse_peer_multiaddr(bootstrap)?;
    let mut segments: Vec<&str> = Vec::new();
    let parts: Vec<&str> = bootstrap.split('/').collect();
    let mut idx = 0;
    while idx < parts.len() {
        if parts[idx] == "gf" {
            idx += 2;
            continue;
        }
        segments.push(parts[idx]);
        idx += 1;
    }
    let mut tagged = segments.join("/");
    if !tagged.ends_with('/') {
        tagged.push('/');
    }
    tagged.push_str(&format!("gf/{responder}"));
    Some(tagged)
}

// ============ Collaboration ============

async fn list_patches(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let patches: Vec<serde_json::Value> = state
        .patch_store
        .for_repo(&name)
        .iter()
        .map(|p| {
            serde_json::json!({
                "id": p.id,
                "author": p.author.to_string(),
                "title": p.title,
                "description": p.description,
                "target_branch": p.target_branch,
                "patch_count": p.patches.len(),
            })
        })
        .collect();
    Json(patches)
}

#[derive(Deserialize)]
struct SubmitPatchRequest {
    target_branch: String,
    title: String,
    description: String,
    patches: Vec<String>, // base64-encoded patches
}

async fn submit_patch(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
    Json(req): Json<SubmitPatchRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;

    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let patches: Vec<Vec<u8>> = req
        .patches
        .iter()
        .filter_map(|p| base64_decode(p))
        .collect();

    let patch_set = PatchSet::new(
        &state.identity,
        repo.owner.clone(),
        name.clone(),
        req.target_branch,
        req.title,
        req.description,
        patches,
    );

    let id = patch_set.id.clone();
    state.patch_store.add(patch_set);

    Ok(Json(serde_json::json!({ "id": id, "status": "submitted" })))
}

async fn list_merge_requests(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let mrs: Vec<serde_json::Value> = state
        .mr_store
        .for_repo(&name)
        .iter()
        .map(|mr| {
            let status = match state.mr_store.status_of(&mr.id) {
                gt_core::types::MergeRequestStatus::Open => "open",
                gt_core::types::MergeRequestStatus::Merged => "merged",
                gt_core::types::MergeRequestStatus::Closed => "closed",
            };
            serde_json::json!({
                "id": mr.id,
                "author": mr.author.to_string(),
                "title": mr.title,
                "description": mr.description,
                "source_repo": mr.source_repo,
                "source_branch": mr.source_branch,
                "target_branch": mr.target_branch,
                "status": status,
            })
        })
        .collect();
    Json(mrs)
}

#[derive(Deserialize)]
struct SubmitMergeRequest {
    source_repo: String,
    source_branch: String,
    target_branch: String,
    title: String,
    description: String,
}

async fn submit_merge_request(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
    Json(req): Json<SubmitMergeRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;

    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;

    let source_repo_name = req.source_repo.clone();
    let source_branch = req.source_branch.clone();
    let target_branch = req.target_branch.clone();
    let mr = MergeRequest::new(
        &state.identity,
        req.source_repo,
        req.source_branch,
        repo.owner.clone(),
        name.clone(),
        req.target_branch,
        req.title,
        req.description,
    );

    let id = mr.id.clone();
    state.mr_store.add(mr);

    let target_path = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .map(|r| (r.path.clone(), r.owner.clone()));
    let source_path = state
        .config
        .repos
        .iter()
        .find(|r| r.name == source_repo_name)
        .map(|r| r.path.clone());
    if let Some((repo_path, repo_owner)) = target_path {
        let commit_sha = source_path
            .map(|p| branch_sha_of(&p, &source_branch))
            .unwrap_or_else(|| "HEAD".to_string());
        let event = gt_compute::workflow::WorkflowEvent::PullRequest {
            branch: target_branch,
        };
        let force_verify = state
            .config
            .compute
            .as_ref()
            .is_some_and(|c| c.require_verification);
        let protected_envs = state
            .config
            .compute
            .as_ref()
            .map(|c| c.protected_environments.clone())
            .unwrap_or_default();
        let identity = state.identity.clone();
        let repo_secrets =
            crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
                .with_passphrase(state.secrets_passphrase.as_deref())
                .load(&name);
        let repo_dirs = crate::workflows::collect_repo_dirs(&state.config.repos);
        let state_ref = &mut *state;
        crate::workflows::trigger_workflows(
            &mut state_ref.job_store,
            &mut state_ref.workflow_runs,
            &identity,
            &repo_owner,
            &name,
            &repo_path,
            &event,
            &commit_sha,
            None,
            0,
            "credit",
            force_verify,
            &repo_secrets,
            &protected_envs,
            &repo_dirs,
        );
    }

    Ok(Json(serde_json::json!({ "id": id, "status": "submitted" })))
}

fn merge_request_target(
    state: &AppState,
    name: &str,
) -> Option<(std::path::PathBuf, gt_core::identity::PeerId)> {
    state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .map(|r| (r.path.clone(), r.owner.clone()))
}

async fn reject_merge_request(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath((name, id)): AxPath<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    let target = merge_request_target(&state, &name).ok_or(StatusCode::NOT_FOUND)?;
    if target.1 != state.identity.peer_id() {
        return Err(StatusCode::FORBIDDEN);
    }
    let mr = state
        .mr_store
        .get(&id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    if mr.target_repo != name {
        return Err(StatusCode::NOT_FOUND);
    }
    if !state
        .mr_store
        .set_status(&id, gt_core::types::MergeRequestStatus::Closed)
    {
        return Err(StatusCode::NOT_FOUND);
    }
    Ok(Json(serde_json::json!({ "id": id, "status": "closed" })))
}

fn git_daemon_env(config_path: &std::path::Path, api_port: u16) -> Vec<(String, String)> {
    vec![
        (
            "GITFOREST_CONFIG".to_string(),
            config_path.to_string_lossy().into_owned(),
        ),
        (
            "GITFOREST_DAEMON_URL".to_string(),
            format!("http://127.0.0.1:{api_port}"),
        ),
    ]
}

async fn run_git(
    repo: &std::path::Path,
    config_path: &std::path::Path,
    api_port: u16,
    args: &[&str],
) -> std::io::Result<std::process::Output> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.arg("-C").arg(repo).args(args);
    for (key, value) in git_daemon_env(config_path, api_port) {
        cmd.env(key, value);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let path = std::env::var("PATH").unwrap_or_default();
            cmd.env("PATH", format!("{}:{path}", dir.display()));
        }
    }
    tokio::time::timeout(Duration::from_secs(300), cmd.output())
        .await
        .map_err(std::io::Error::other)?
}

async fn accept_merge_request(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath((name, id)): AxPath<(String, String)>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    let err = |status: StatusCode, msg: String| (status, Json(serde_json::json!({"error": msg})));
    let (repo_path, config_path, api_port, mr) = {
        let s = state.read().await;
        check_local_auth(&headers, &s.api_token)
            .map_err(|c| (c, Json(serde_json::json!({"error": "unauthorized"}))))?;
        let target = merge_request_target(&s, &name)
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown repo".to_string()))?;
        if target.1 != s.identity.peer_id() {
            return Err(err(
                StatusCode::FORBIDDEN,
                "only the repo owner can accept".to_string(),
            ));
        }
        let mr = s
            .mr_store
            .get(&id)
            .cloned()
            .ok_or_else(|| err(StatusCode::NOT_FOUND, "unknown merge request".to_string()))?;
        if mr.target_repo != name {
            return Err(err(
                StatusCode::NOT_FOUND,
                "unknown merge request".to_string(),
            ));
        }
        match s.mr_store.status_of(&id) {
            gt_core::types::MergeRequestStatus::Merged => {
                return Ok(Json(serde_json::json!({ "id": id, "status": "merged" })));
            }
            gt_core::types::MergeRequestStatus::Closed => {
                return Err(err(
                    StatusCode::CONFLICT,
                    "merge request is closed".to_string(),
                ));
            }
            gt_core::types::MergeRequestStatus::Open => {}
        }
        (target.0, s.config_path.clone(), s.config.api_port, mr)
    };
    let source_url = format!("gitforest://{}/{}", mr.author, mr.source_repo);
    let fetch = run_git(
        &repo_path,
        &config_path,
        api_port,
        &["fetch", &source_url, &mr.source_branch],
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("fetch failed: {e}")))?;
    if !fetch.status.success() {
        let tail = String::from_utf8_lossy(&fetch.stderr);
        return Err(err(
            StatusCode::BAD_GATEWAY,
            format!("fetch failed: {tail}"),
        ));
    }
    let checkout = run_git(
        &repo_path,
        &config_path,
        api_port,
        &["checkout", &mr.target_branch],
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("checkout failed: {e}")))?;
    if !checkout.status.success() {
        return Err(err(
            StatusCode::BAD_GATEWAY,
            "checkout of target branch failed".to_string(),
        ));
    }
    let message = format!("Merge {}: {}", id, mr.title);
    let merge = run_git(
        &repo_path,
        &config_path,
        api_port,
        &["merge", "--no-ff", "FETCH_HEAD", "-m", &message],
    )
    .await
    .map_err(|e| err(StatusCode::BAD_GATEWAY, format!("merge failed: {e}")))?;
    if !merge.status.success() {
        let _ = run_git(&repo_path, &config_path, api_port, &["merge", "--abort"]).await;
        return Err(err(
            StatusCode::CONFLICT,
            "merge has conflicts; resolve them in the source branch".to_string(),
        ));
    }
    {
        let mut s = state.write().await;
        s.mr_store
            .set_status(&id, gt_core::types::MergeRequestStatus::Merged);
    }
    Ok(Json(serde_json::json!({ "id": id, "status": "merged" })))
}

async fn get_acl(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    let acl = state.acls.get(&name).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(serde_json::json!({
        "owner": acl.owner.to_string(),
        "repo_name": acl.repo_name,
        "version": acl.version,
        "grants": acl.grants.iter().map(|g| serde_json::json!({
            "peer_id": g.peer_id.to_string(),
            "ref_pattern": g.ref_pattern,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
struct AclUpdateRequest {
    action: String, // "grant" or "revoke"
    peer_id: String,
    ref_pattern: Option<String>,
}

async fn update_acl(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
    Json(req): Json<AclUpdateRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;

    let peer_id: gt_core::identity::PeerId =
        req.peer_id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    // Clone identity to avoid borrow conflict with acls
    let identity = state.identity.clone();

    // Mutate ACL in a limited scope so the borrow on state.acls ends
    // before we touch state.config.
    let (version, grants) = {
        let acl = state.acls.get_mut(&name).ok_or(StatusCode::NOT_FOUND)?;

        match req.action.as_str() {
            "grant" => {
                let pattern = req.ref_pattern.unwrap_or_else(|| "refs/heads/*".into());
                acl.grant(&identity, peer_id, pattern)
                    .map_err(|_| StatusCode::FORBIDDEN)?;
            }
            "revoke" => {
                acl.revoke(&identity, &peer_id)
                    .map_err(|_| StatusCode::FORBIDDEN)?;
            }
            _ => return Err(StatusCode::BAD_REQUEST),
        }

        let grants: Vec<gt_core::config::AclGrantConfig> = acl
            .grants
            .iter()
            .map(|g| gt_core::config::AclGrantConfig {
                peer_id: g.peer_id.clone(),
                ref_pattern: g.ref_pattern.clone(),
            })
            .collect();

        (acl.version, grants)
    };

    // Sync ACL grants back to config and save to disk
    state.config.acl_grants.insert(name.clone(), grants);
    let _ = state.config.save(&state.config_path);

    Ok(Json(
        serde_json::json!({ "status": "updated", "version": version }),
    ))
}

async fn get_ci_config(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;

    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    let yaml = std::fs::read_to_string(repo.path.join(".gitforest-ci.yml"))
        .map_err(|_| StatusCode::NOT_FOUND)?;
    if yaml.len() > 65536 {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    Ok(Json(serde_json::json!({ "yaml": yaml })))
}

#[derive(Deserialize)]
struct SetSecretRequest {
    key: String,
    value: String,
}

async fn list_secrets(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    if !state.config.repos.iter().any(|r| r.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let store = crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
        .with_passphrase(state.secrets_passphrase.as_deref());
    Ok(Json(serde_json::json!({ "names": store.names(&name) })))
}

async fn set_secret(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
    Json(req): Json<SetSecretRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    if !state.config.repos.iter().any(|r| r.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let store = crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
        .with_passphrase(state.secrets_passphrase.as_deref());
    store
        .set(&name, &req.key, &req.value)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    Ok(Json(
        serde_json::json!({ "key": req.key, "status": "stored" }),
    ))
}

async fn delete_secret(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath((name, key)): AxPath<(String, String)>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    if !state.config.repos.iter().any(|r| r.name == name) {
        return Err(StatusCode::NOT_FOUND);
    }
    let store = crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
        .with_passphrase(state.secrets_passphrase.as_deref());
    match store.delete(&name, &key) {
        Ok(true) => Ok(Json(serde_json::json!({ "key": key, "status": "deleted" }))),
        Ok(false) => Err(StatusCode::NOT_FOUND),
        Err(_) => Err(StatusCode::BAD_REQUEST),
    }
}

async fn list_workflows(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    let repo = state
        .config
        .repos
        .iter()
        .find(|r| r.name == name)
        .ok_or(StatusCode::NOT_FOUND)?;
    let mut workflows = Vec::new();
    let mut errors = Vec::new();
    for (path, parsed) in gt_compute::workflow::load_workflows(&repo.path) {
        let file = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        match parsed {
            Ok(w) => {
                let mut triggers = Vec::new();
                if w.on.push.is_some() {
                    triggers.push("push");
                }
                if w.on.pull_request.is_some() {
                    triggers.push("pull_request");
                }
                if w.on.workflow_dispatch.is_some() {
                    triggers.push("workflow_dispatch");
                }
                let mut jobs: Vec<String> = w.jobs.keys().cloned().collect();
                jobs.sort();
                workflows.push(serde_json::json!({
                    "file": file,
                    "name": if w.name.is_empty() { file.clone() } else { w.name },
                    "triggers": triggers,
                    "jobs": jobs,
                }));
            }
            Err(e) => {
                errors.push(serde_json::json!({
                    "file": file,
                    "error": e.to_string(),
                }));
            }
        }
    }
    Ok(Json(serde_json::json!({
        "repo": name,
        "workflows": workflows,
        "errors": errors,
    })))
}

#[derive(Deserialize)]
struct DispatchWorkflowRequest {
    workflow: Option<String>,
    branch: Option<String>,
    tag: Option<String>,
    commit_sha: Option<String>,
    #[serde(default)]
    inputs: HashMap<String, String>,
    #[serde(default)]
    max_payment_sats: u64,
    #[serde(default = "default_chain")]
    preferred_chain: String,
}

fn head_sha_of(repo_path: &std::path::Path) -> String {
    branch_sha_of(repo_path, "HEAD")
}

fn job_is_terminal(status: &JobStatus) -> bool {
    matches!(
        status,
        JobStatus::Completed { .. }
            | JobStatus::Failed { .. }
            | JobStatus::Cancelled
            | JobStatus::Disputed
    )
}

pub(crate) fn fire_workflow_run_triggers(state: &mut AppState, settled_job_id: &str) {
    let settled = match state.job_store.get(settled_job_id) {
        Some(job) => job.clone(),
        None => return,
    };
    let candidates: Vec<String> = state
        .workflow_runs
        .list()
        .iter()
        .filter(|r| !r.workflow_run_fired && r.job_ids.iter().any(|id| id == settled_job_id))
        .filter(|r| {
            !r.job_ids.is_empty()
                && r.job_ids.iter().all(|id| {
                    state
                        .job_store
                        .get(id)
                        .is_some_and(|j| job_is_terminal(&j.status))
                })
        })
        .map(|r| r.id.clone())
        .collect();
    for run_id in candidates {
        let Some(run) = state.workflow_runs.get(&run_id).cloned() else {
            continue;
        };
        state.workflow_runs.mark_workflow_run_fired(&run_id);
        let mut conclusion = "success";
        for id in &run.job_ids {
            match state.job_store.get(id).map(|j| &j.status) {
                Some(JobStatus::Failed { .. }) | Some(JobStatus::Disputed) => {
                    conclusion = "failure";
                    break;
                }
                Some(JobStatus::Cancelled) => conclusion = "cancelled",
                _ => {}
            }
        }
        let repo_path = match state.config.repos.iter().find(|r| r.name == run.repo_name) {
            Some(repo) => repo.path.clone(),
            None => continue,
        };
        let event = gt_compute::workflow::WorkflowEvent::WorkflowRun {
            workflow: run.workflow_name.clone(),
            file: run.workflow_file.clone(),
            conclusion: conclusion.to_string(),
            branch: run.branch.clone(),
        };
        let identity = state.identity.clone();
        let repo_secrets =
            crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
                .with_passphrase(state.secrets_passphrase.as_deref())
                .load(&run.repo_name);
        let protected_envs = state
            .config
            .compute
            .as_ref()
            .map(|c| c.protected_environments.clone())
            .unwrap_or_default();
        let force_verify = state
            .config
            .compute
            .as_ref()
            .is_some_and(|c| c.require_verification);
        let repo_dirs = crate::workflows::collect_repo_dirs(&state.config.repos);
        let created = crate::workflows::trigger_workflows(
            &mut state.job_store,
            &mut state.workflow_runs,
            &identity,
            &settled.repo_owner,
            &run.repo_name,
            &repo_path,
            &event,
            &run.commit_sha,
            None,
            0,
            "credit",
            force_verify,
            &repo_secrets,
            &protected_envs,
            &repo_dirs,
        );
        if !created.is_empty() {
            tracing::info!(
                "workflow_run {} ({conclusion}) fired {} follow-up runs",
                run.workflow_name,
                created.len()
            );
        }
    }
}

fn default_branch_of(repo_path: &std::path::Path) -> String {
    std::process::Command::new("git")
        .args(["symbolic-ref", "--short", "HEAD"])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "main".to_string())
}

pub(crate) async fn run_schedule_tick(state: Arc<RwLock<AppState>>) {
    fire_due_schedules(&mut *state.write().await, &chrono::Utc::now());
}

pub(crate) fn fire_due_schedules(state: &mut AppState, now: &chrono::DateTime<chrono::Utc>) {
    let slot = now.format("%Y-%m-%dT%H:%M").to_string();
    let live: Vec<String> = state
        .config
        .repos
        .iter()
        .map(|r| format!("{}\0", r.name))
        .collect();
    state
        .schedule_fired
        .retain(|key, _| live.iter().any(|prefix| key.starts_with(prefix)));
    let mut due: Vec<(String, std::path::PathBuf, String, String)> = Vec::new();
    for repo in &state.config.repos {
        for (path, parsed) in gt_compute::workflow::load_workflows(&repo.path) {
            let Ok(workflow) = parsed else {
                continue;
            };
            let Some(entries) = workflow.on.schedule.as_ref() else {
                continue;
            };
            let file_name = path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            for entry in entries {
                if !gt_compute::workflow::cron_due(&entry.cron, now) {
                    continue;
                }
                let key = format!("{}\0{}\0{}", repo.name, file_name, entry.cron);
                if state.schedule_fired.get(&key).is_some_and(|s| s == &slot) {
                    continue;
                }
                state.schedule_fired.insert(key, slot.clone());
                due.push((
                    repo.name.clone(),
                    repo.path.clone(),
                    file_name.clone(),
                    entry.cron.clone(),
                ));
            }
        }
    }
    for (repo_name, repo_path, file_name, cron) in due {
        let branch = default_branch_of(&repo_path);
        let sha = branch_sha_of(&repo_path, "HEAD");
        let owner = state
            .config
            .repos
            .iter()
            .find(|r| r.name == repo_name)
            .map(|r| r.owner.clone());
        let Some(repo_owner) = owner else {
            continue;
        };
        let event = gt_compute::workflow::WorkflowEvent::Schedule {
            cron: cron.clone(),
            branch,
        };
        let identity = state.identity.clone();
        let repo_secrets =
            crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
                .with_passphrase(state.secrets_passphrase.as_deref())
                .load(&repo_name);
        let protected_envs = state
            .config
            .compute
            .as_ref()
            .map(|c| c.protected_environments.clone())
            .unwrap_or_default();
        let force_verify = state
            .config
            .compute
            .as_ref()
            .is_some_and(|c| c.require_verification);
        let repo_dirs = crate::workflows::collect_repo_dirs(&state.config.repos);
        let created = crate::workflows::trigger_workflows(
            &mut state.job_store,
            &mut state.workflow_runs,
            &identity,
            &repo_owner,
            &repo_name,
            &repo_path,
            &event,
            &sha,
            Some(&file_name),
            0,
            "credit",
            force_verify,
            &repo_secrets,
            &protected_envs,
            &repo_dirs,
        );
        if !created.is_empty() {
            tracing::info!(
                "schedule {cron} fired {} runs for {repo_name}",
                created.len()
            );
        }
    }
}

fn branch_sha_of(repo_path: &std::path::Path, branch: &str) -> String {
    std::process::Command::new("git")
        .args(["rev-parse", branch])
        .current_dir(repo_path)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "HEAD".to_string())
}

async fn dispatch_workflow(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(name): AxPath<String>,
    Json(req): Json<DispatchWorkflowRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Workflows,
        PermissionLevel::Write,
    )?;
    let (repo_path, repo_owner) = {
        let repo = state
            .config
            .repos
            .iter()
            .find(|r| r.name == name)
            .ok_or(StatusCode::NOT_FOUND)?;
        (repo.path.clone(), repo.owner.clone())
    };
    let commit_sha = req.commit_sha.unwrap_or_else(|| head_sha_of(&repo_path));
    let event = match (req.tag, req.branch) {
        (Some(tag), _) => gt_compute::workflow::WorkflowEvent::Tag { tag },
        (None, Some(branch)) => gt_compute::workflow::WorkflowEvent::Push { branch },
        (None, None) => {
            gt_compute::workflow::WorkflowEvent::WorkflowDispatch { inputs: req.inputs }
        }
    };
    let force_verify = state
        .config
        .compute
        .as_ref()
        .is_some_and(|c| c.require_verification);
    let protected_envs = state
        .config
        .compute
        .as_ref()
        .map(|c| c.protected_environments.clone())
        .unwrap_or_default();
    let identity = state.identity.clone();
    let repo_secrets =
        crate::secrets::RepoSecretsStore::new(&state.config.data_dir, &state.identity)
            .with_passphrase(state.secrets_passphrase.as_deref())
            .load(&name);
    let repo_dirs = crate::workflows::collect_repo_dirs(&state.config.repos);
    let state_ref = &mut *state;
    let created = crate::workflows::trigger_workflows(
        &mut state_ref.job_store,
        &mut state_ref.workflow_runs,
        &identity,
        &repo_owner,
        &name,
        &repo_path,
        &event,
        &commit_sha,
        req.workflow.as_deref(),
        req.max_payment_sats,
        &req.preferred_chain,
        force_verify,
        &repo_secrets,
        &protected_envs,
        &repo_dirs,
    );
    let runs: Vec<serde_json::Value> = created
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "workflow": r.workflow_name,
                "file": r.workflow_file,
                "event": r.event,
                "job_ids": r.job_ids,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "runs": runs })))
}

#[derive(Deserialize, Default)]
struct WorkflowRunsQuery {
    repo: Option<String>,
}

async fn list_workflow_runs(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<WorkflowRunsQuery>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let state = state.read().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Workflows,
        PermissionLevel::Read,
    )?;
    let listed: Vec<&crate::workflows::WorkflowRun> = match params.repo.as_deref() {
        Some(repo) => state.workflow_runs.for_repo(repo),
        None => state.workflow_runs.list(),
    };
    let runs: Vec<serde_json::Value> = listed
        .iter()
        .map(|r| {
            let summary = crate::workflows::summarize_run(r, &state.job_store);
            serde_json::json!({
                "id": r.id,
                "repo": r.repo_name,
                "workflow": r.workflow_name,
                "event": r.event,
                "branch": r.branch,
                "commit_sha": r.commit_sha,
                "job_ids": r.job_ids,
                "summary": summary,
                "created_at": r.created_at.to_rfc3339(),
            })
        })
        .collect();
    Ok(Json(runs))
}

async fn get_workflow_run(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Workflows,
        PermissionLevel::Read,
    )?;
    let run = state.workflow_runs.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    let summary = crate::workflows::summarize_run(run, &state.job_store);
    let jobs: Vec<serde_json::Value> = run
        .job_ids
        .iter()
        .map(|jid| match state.job_store.get(jid) {
            Some(j) => serde_json::json!({
                "id": j.id,
                "name": j.job_config.name,
                "status": job_status_name(&j.status),
            }),
            None => serde_json::json!({ "id": jid, "status": "missing" }),
        })
        .collect();
    Ok(Json(serde_json::json!({
        "id": run.id,
        "repo": run.repo_name,
        "workflow": run.workflow_name,
        "file": run.workflow_file,
        "event": run.event,
        "branch": run.branch,
        "commit_sha": run.commit_sha,
        "jobs": jobs,
        "summary": summary,
        "created_at": run.created_at.to_rfc3339(),
    })))
}

async fn rerun_workflow_run(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Workflows,
        PermissionLevel::Write,
    )?;
    if state.workflow_runs.get(&id).is_none() {
        return Err(StatusCode::NOT_FOUND);
    }
    let identity = state.identity.clone();
    let state_ref = &mut *state;
    let rerun = crate::workflows::rerun_failed_jobs(
        &mut state_ref.job_store,
        &mut state_ref.workflow_runs,
        &identity,
        &id,
    );
    gt_compute::dispatch::unblock_ready_jobs(&mut state_ref.job_store, &state_ref.result_store);
    let runs: Vec<serde_json::Value> = rerun
        .iter()
        .map(|r| {
            serde_json::json!({
                "id": r.id,
                "workflow": r.workflow_name,
                "file": r.workflow_file,
                "event": r.event,
                "job_ids": r.job_ids,
                "rerun_of": r.rerun_of,
            })
        })
        .collect();
    Ok(Json(serde_json::json!({ "runs": runs })))
}

// ============ Peer Name Resolution ============

async fn resolve_peer(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(identifier): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    // Try resolving via config's peer_names map
    if let Some(peer_id) = state.config.resolve_peer(&identifier) {
        return Ok(Json(serde_json::json!({
            "peer_id": peer_id.to_string(),
            "resolved_from": identifier,
        })));
    }
    // Check if identifier is our own peer name
    let our_peer_id = state.identity.peer_id();
    if let Some(display_name) = gt_core::identity::load_display_name() {
        let our_name = gt_core::identity::PeerName::new(display_name, our_peer_id.clone());
        if our_name.short_tag() == identifier {
            return Ok(Json(serde_json::json!({
                "peer_id": our_peer_id.to_string(),
                "resolved_from": identifier,
            })));
        }
    }
    Err(StatusCode::NOT_FOUND)
}

// ============ Remote Helper Endpoints ============

/// Resolve a peer identifier (name tag, alias, or full hex) to a full peer ID string.
fn resolve_peer_id(
    config: &gt_core::config::Config,
    identity: &gt_core::identity::Identity,
    identifier: &str,
) -> Option<String> {
    // 1. Full hex peer ID
    if identifier.len() == 64 && identifier.chars().all(|c| c.is_ascii_hexdigit()) {
        return Some(identifier.to_string());
    }
    // 2. Config-based resolution
    if let Some(pid) = config.resolve_peer(identifier) {
        return Some(pid.to_string());
    }
    // 3. Check if it's our own peer name
    let our_peer_id = identity.peer_id();
    if let Some(display_name) = gt_core::identity::load_display_name() {
        let our_name = gt_core::identity::PeerName::new(display_name, our_peer_id.clone());
        if our_name.short_tag() == identifier {
            return Some(our_peer_id.to_string());
        }
    }
    None
}

async fn remote_list_refs(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath((peer_id_param, repo)): AxPath<(String, String)>,
) -> Result<Json<Vec<(String, String)>>, StatusCode> {
    let state = state.read().await;
    let resolved = resolve_peer_id(&state.config, &state.identity, &peer_id_param);
    let our_peer_id = state.identity.peer_id().to_string();
    let target_peer = resolved.as_deref().unwrap_or(&peer_id_param);

    // If this is our own repo, serve locally
    if target_peer == our_peer_id {
        if let Some(repo_config) = state.config.repos.iter().find(|r| r.name == repo) {
            let refs = git_bridge::list_refs(&repo_config.path).unwrap_or_default();
            return Ok(Json(refs));
        }
        tracing::warn!(
            "remote_list_refs: repo '{}' not found among {} repos",
            sanitize_log(&repo),
            state.config.repos.len()
        );
        return Err(StatusCode::NOT_FOUND);
    }

    tracing::info!(
        "remote_list_refs: proxying for target={} our={our_peer_id} repo={}",
        sanitize_log(target_peer),
        sanitize_log(&repo)
    );

    // Remote peer — proxy the request to their API
    if let Some((addr, peer_api_port)) = resolve_peer_api_address(&state, target_peer) {
        let url = format!("http://{addr}:{peer_api_port}/api/remote/{target_peer}/{repo}/refs");
        tracing::info!("Proxying refs request to {}", sanitize_log(&url));
        let client = crate::fetch::peer_client().ok_or(StatusCode::BAD_GATEWAY)?;
        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                match resp.json::<Vec<(String, String)>>().await {
                    Ok(refs) => return Ok(Json(refs)),
                    Err(e) => {
                        tracing::warn!(
                            "Failed to parse refs from peer: {}",
                            sanitize_log(&e.to_string())
                        );
                        return Err(StatusCode::BAD_GATEWAY);
                    }
                }
            }
            Ok(resp) => {
                tracing::warn!("Peer returned {}", resp.status());
                return Err(StatusCode::BAD_GATEWAY);
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to connect to peer API: {}",
                    sanitize_log(&e.to_string())
                );
                return Err(StatusCode::BAD_GATEWAY);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

async fn remote_fetch(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath((peer_id_param, repo)): AxPath<(String, String)>,
) -> Result<axum::response::Response, StatusCode> {
    use axum::response::IntoResponse;

    let state = state.read().await;
    let resolved = resolve_peer_id(&state.config, &state.identity, &peer_id_param);
    let our_peer_id = state.identity.peer_id().to_string();
    let target_peer = resolved.as_deref().unwrap_or(&peer_id_param);

    if target_peer == our_peer_id {
        // Local repo — create bundle and return as binary
        let repo_config = state
            .config
            .repos
            .iter()
            .find(|r| r.name == repo)
            .ok_or(StatusCode::NOT_FOUND)?;
        let tmp_bundle = std::env::temp_dir().join(format!(
            "gf_bundle_{}_{}.bundle",
            repo,
            uuid::Uuid::new_v4()
        ));
        gt_transport::bundle::create_bundle(&repo_config.path, &tmp_bundle).map_err(|e| {
            tracing::error!("Failed to create bundle: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
        let bundle_bytes = tokio::fs::read(&tmp_bundle)
            .await
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        let _ = tokio::fs::remove_file(&tmp_bundle).await;
        tracing::info!(
            "Serving bundle for {}: {} bytes",
            sanitize_log(&repo),
            bundle_bytes.len()
        );
        return Ok((
            [(axum::http::header::CONTENT_TYPE, "application/x-git-bundle")],
            bundle_bytes,
        )
            .into_response());
    }

    // Remote peer — download bundle from peer and forward
    if let Some((addr, peer_api_port)) = resolve_peer_api_address(&state, target_peer) {
        let url = format!("http://{addr}:{peer_api_port}/api/remote/{target_peer}/{repo}/fetch");
        tracing::info!("Proxying fetch to {}", sanitize_log(&url));
        let client = crate::fetch::peer_client().ok_or(StatusCode::BAD_GATEWAY)?;
        match client.post(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                let bytes = resp.bytes().await.map_err(|_| StatusCode::BAD_GATEWAY)?;
                // Forward the binary bundle directly to the client
                return Ok((
                    [(axum::http::header::CONTENT_TYPE, "application/x-git-bundle")],
                    bytes,
                )
                    .into_response());
            }
            Ok(resp) => {
                tracing::warn!("Peer fetch returned {}", resp.status());
                return Err(StatusCode::BAD_GATEWAY);
            }
            Err(e) => {
                tracing::warn!("Peer fetch failed: {}", sanitize_log(&e.to_string()));
                return Err(StatusCode::BAD_GATEWAY);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// A ref update carried in a push request.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PushRefUpdate {
    pusher: String,
    ref_name: String,
    new_sha: String,
    old_sha: String,
}

/// Parse the base64-encoded JSON ref updates from the push header.
fn parse_push_refs(headers: &axum::http::HeaderMap) -> Option<Vec<PushRefUpdate>> {
    let header = headers.get("x-gitforest-push-refs")?;
    let b64 = header.to_str().ok()?;
    let json_bytes = base64::engine::general_purpose::STANDARD.decode(b64).ok()?;
    serde_json::from_slice(&json_bytes).ok()
}

async fn remote_push(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath((peer_id_param, repo)): AxPath<(String, String)>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state_arc = state.clone();
    let state = state.read().await;
    let cluster_token = state.config.cluster_token.as_deref();
    check_auth(&headers, &state.api_token, cluster_token)?;
    let resolved = resolve_peer_id(&state.config, &state.identity, &peer_id_param);
    let our_peer_id = state.identity.peer_id().to_string();
    let target_peer = resolved.as_deref().unwrap_or(&peer_id_param);

    let ref_updates = parse_push_refs(&headers).ok_or(StatusCode::BAD_REQUEST)?;
    if ref_updates.is_empty() {
        return Ok(Json(serde_json::json!({
            "ok": false,
            "error": "no ref updates provided"
        })));
    }
    if ref_updates
        .iter()
        .any(|u| u.pusher != ref_updates[0].pusher)
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let refs_b64 = headers
        .get("x-gitforest-push-refs")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::BAD_REQUEST)?;
    let claimed_pusher: gt_core::identity::PeerId = ref_updates[0]
        .pusher
        .parse()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let sig_b64 = headers
        .get("x-gitforest-push-sig")
        .and_then(|v| v.to_str().ok())
        .ok_or(StatusCode::UNAUTHORIZED)?;
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_b64)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;
    let payload = gt_core::protocol::push_signable_payload(&repo, refs_b64, &body);
    gt_core::identity::verify_signature(&claimed_pusher, &payload, &sig)
        .map_err(|_| StatusCode::UNAUTHORIZED)?;

    if target_peer == our_peer_id {
        // Local repo — unbundle and update refs
        let repo_config = state
            .config
            .repos
            .iter()
            .find(|r| r.name == repo)
            .ok_or(StatusCode::NOT_FOUND)?;

        // Pusher id was signature-verified above; reuse it for ACL checks.
        let pusher_id = claimed_pusher.clone();

        let acl = state.acls.get(&repo);

        // Check authorization for every ref update
        let mut unauthorized_refs: Vec<String> = Vec::new();
        for update in &ref_updates {
            let authorized = acl.is_some_and(|a| a.can_push(&pusher_id, &update.ref_name));
            // Owner is implicitly authorized by can_push, but double-check just in case
            let is_owner = repo_config.owner == pusher_id;
            if !authorized && !is_owner {
                unauthorized_refs.push(update.ref_name.clone());
            }
        }

        if !unauthorized_refs.is_empty() {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "error": format!(
                    "not authorized to push to {}. \
                     The repo owner must grant you access via: \
                     gitforest acl grant {repo} <your_peer_id> \"refs/heads/*\" \
                     Or propose changes as a merge request instead.",
                    unauthorized_refs.join(", ")
                ),
                "unauthorized_refs": unauthorized_refs,
                "suggestion": "merge_request",
            })));
        }

        // Unbundle objects if a bundle was sent
        if !body.is_empty() {
            let tmp_bundle =
                std::env::temp_dir().join(format!("gf_recv_push_{}.bundle", uuid::Uuid::new_v4()));
            tokio::fs::write(&tmp_bundle, &body)
                .await
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let output = std::process::Command::new("git")
                .args(["bundle", "unbundle", &tmp_bundle.display().to_string()])
                .current_dir(&repo_config.path)
                .output()
                .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

            let _ = tokio::fs::remove_file(&tmp_bundle).await;

            if !output.status.success() {
                let stderr = String::from_utf8_lossy(&output.stderr);
                return Ok(Json(serde_json::json!({
                    "ok": false,
                    "error": format!("bundle unbundle failed: {}", stderr)
                })));
            }
        }

        // Verify ref state matches old_sha (conflict detection)
        let mut stale_refs: Vec<String> = Vec::new();
        for update in &ref_updates {
            let current = std::process::Command::new("git")
                .args(["rev-parse", &update.ref_name])
                .current_dir(&repo_config.path)
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            // Empty old_sha means the ref should not exist yet
            let expected = &update.old_sha;
            if expected.is_empty() && !current.is_empty() {
                stale_refs.push(format!(
                    "{}: expected new ref but found {}",
                    update.ref_name, current
                ));
            } else if !expected.is_empty() && expected != &current {
                stale_refs.push(format!(
                    "{}: expected {} but found {}",
                    update.ref_name, expected, current
                ));
            }
        }
        if !stale_refs.is_empty() {
            return Ok(Json(serde_json::json!({
                "ok": false,
                "error": format!("stale refs — re-fetch and try again: {}", stale_refs.join("; ")),
            })));
        }

        // Apply ref updates
        for update in &ref_updates {
            if update.new_sha.is_empty() {
                let _ = std::process::Command::new("git")
                    .args(["update-ref", "-d", &update.ref_name])
                    .current_dir(&repo_config.path)
                    .output();
            } else {
                let _ = std::process::Command::new("git")
                    .args(["update-ref", &update.ref_name, &update.new_sha])
                    .current_dir(&repo_config.path)
                    .output();
            }
        }

        let pushes: Vec<(gt_compute::workflow::WorkflowEvent, String)> = ref_updates
            .iter()
            .filter(|u| !u.new_sha.is_empty())
            .filter_map(|u| {
                let event = if let Some(branch) = u.ref_name.strip_prefix("refs/heads/") {
                    gt_compute::workflow::WorkflowEvent::Push {
                        branch: branch.to_string(),
                    }
                } else {
                    gt_compute::workflow::WorkflowEvent::Tag {
                        tag: u.ref_name.strip_prefix("refs/tags/")?.to_string(),
                    }
                };
                Some((event, u.new_sha.clone()))
            })
            .collect();
        let repo_name = repo.to_string();
        let repo_path = repo_config.path.clone();
        let repo_owner = repo_config.owner.clone();
        drop(state);
        if !pushes.is_empty() {
            let mut s = state_arc.write().await;
            let force_verify = s
                .config
                .compute
                .as_ref()
                .is_some_and(|c| c.require_verification);
            let protected_envs = s
                .config
                .compute
                .as_ref()
                .map(|c| c.protected_environments.clone())
                .unwrap_or_default();
            let identity = s.identity.clone();
            let repo_secrets =
                crate::secrets::RepoSecretsStore::new(&s.config.data_dir, &s.identity)
                    .with_passphrase(s.secrets_passphrase.as_deref())
                    .load(&repo_name);
            let repo_dirs = crate::workflows::collect_repo_dirs(&s.config.repos);
            let s_ref = &mut *s;
            for (event, sha) in pushes {
                crate::workflows::trigger_workflows(
                    &mut s_ref.job_store,
                    &mut s_ref.workflow_runs,
                    &identity,
                    &repo_owner,
                    &repo_name,
                    &repo_path,
                    &event,
                    &sha,
                    None,
                    0,
                    "credit",
                    force_verify,
                    &repo_secrets,
                    &protected_envs,
                    &repo_dirs,
                );
            }
        }

        return Ok(Json(serde_json::json!({ "ok": true })));
    }

    // Remote peer — proxy the bundle + ref updates to their daemon
    if let Some((addr, peer_api_port)) = resolve_peer_api_address(&state, target_peer) {
        let url = format!("http://{addr}:{peer_api_port}/api/remote/{target_peer}/{repo}/push");
        let ref_b64 = headers
            .get("x-gitforest-push-refs")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        let sig_b64 = headers
            .get("x-gitforest-push-sig")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");

        let client = crate::fetch::peer_client().ok_or(StatusCode::BAD_GATEWAY)?;
        let mut request = client
            .post(&url)
            .header("X-GitForest-Push-Refs", ref_b64)
            .header("X-GitForest-Push-Sig", sig_b64)
            .header("Content-Type", "application/x-git-bundle")
            .body(body);
        if let Some(ref cluster) = state.config.cluster_token {
            request = request.header("Authorization", format!("Bearer {cluster}"));
        }

        tracing::info!("Proxying push to {}", sanitize_log(&url));
        match request.send().await {
            Ok(resp) if resp.status().is_success() => {
                let json = resp
                    .json::<serde_json::Value>()
                    .await
                    .map_err(|_| StatusCode::BAD_GATEWAY)?;
                return Ok(Json(json));
            }
            Ok(resp) => {
                tracing::warn!("Peer push returned {}", resp.status());
                return Err(StatusCode::BAD_GATEWAY);
            }
            Err(e) => {
                tracing::warn!("Peer push failed: {}", sanitize_log(&e.to_string()));
                return Err(StatusCode::BAD_GATEWAY);
            }
        }
    }

    Err(StatusCode::NOT_FOUND)
}

/// Resolve a target peer ID to its HTTP API address using known peers.
/// Returns (host, api_port). Discovered addresses are scored so that
/// non-loopback, stable P2P-port addresses are preferred over loopback or
/// ephemeral source-port observations.
fn resolve_peer_api_address(state: &AppState, target_peer: &str) -> Option<(String, u16)> {
    let target_tag = format!("/gf/{target_peer}");
    // (host, api_port, p2p_port, has explicit /api/ segment)
    let mut candidates: Vec<(String, u16, u16, bool)> = Vec::new();

    for peer in &state.known_peers {
        let tagged = peer.contains(&format!("{target_tag}/")) || peer.ends_with(&target_tag);
        if !tagged {
            continue;
        }
        let parts: Vec<&str> = peer.split('/').collect();
        let host_idx = parts
            .iter()
            .position(|&s| s == "ip4" || s == "ip6" || s == "dns4" || s == "dns");
        let host = match host_idx.and_then(|idx| parts.get(idx + 1)) {
            Some(h) => *h,
            None => continue,
        };
        if crate::fetch::dial_blocked(host) {
            continue;
        }
        let p2p_port = parts
            .iter()
            .position(|&s| s == "tcp")
            .and_then(|idx| parts.get(idx + 1))
            .and_then(|s| s.parse().ok())
            .unwrap_or(0u16);
        // Only use discovered addresses that carry an explicit /api segment.
        // mDNS/identify observations lack the API port and defaulting to 7878
        // causes the proxy to hit the wrong daemon in multi-peer/local-test setups.
        let Some(api_idx) = parts.iter().position(|&s| s == "api") else {
            continue;
        };
        let api_port = parts.get(api_idx + 1).and_then(|s| s.parse().ok());
        let explicit_api = api_port.is_some();
        candidates.push((
            host.to_string(),
            api_port.unwrap_or(7878),
            p2p_port,
            explicit_api,
        ));
    }

    // Fallback: match the first bootstrap peer that contains this target (legacy).
    if candidates.is_empty() {
        if let Some(peer) = state.config.bootstrap_peers.first() {
            if peer.contains(&format!("/p2p/{target_peer}/"))
                || peer.ends_with(&format!("/p2p/{target_peer}"))
            {
                return parse_peer_multiaddr(peer)
                    .filter(|(host, _)| !crate::fetch::dial_blocked(host))
                    .map(|(host, port)| (host.to_string(), port));
            }
        }
        return None;
    }

    candidates.sort_by(|a, b| {
        let score_a = score_peer_addr(&a.0, a.2, a.1) + if a.3 { 200 } else { 0 };
        let score_b = score_peer_addr(&b.0, b.2, b.1) + if b.3 { 200 } else { 0 };
        score_a.cmp(&score_b).reverse()
    });

    let (host, api_port, _p2p_port, _explicit) = candidates.into_iter().next()?;
    tracing::info!(
        "Resolved {} API to http://{}:{api_port}",
        sanitize_log(target_peer),
        sanitize_log(&host)
    );
    Some((host, api_port))
}

fn job_status_name(status: &JobStatus) -> &'static str {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Blocked { .. } => "blocked",
        JobStatus::Matched { .. } => "matched",
        JobStatus::Verifying { .. } => "verifying",
        JobStatus::Running { .. } => "running",
        JobStatus::Completed { .. } => "completed",
        JobStatus::Failed { .. } => "failed",
        JobStatus::Disputed => "disputed",
        JobStatus::Cancelled => "cancelled",
    }
}

async fn list_compute_jobs(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let jobs: Vec<serde_json::Value> = state
        .job_store
        .list()
        .iter()
        .map(|j| {
            serde_json::json!({
                "id": j.id,
                "name": j.job_config.name,
                "submitter": j.submitter.to_string(),
                "repo": j.repo_name,
                "repo_name": j.repo_name,
                "commit_sha": j.commit_sha,
                "docker_image": j.job_config.docker_image,
                "status": job_status_name(&j.status),
                "environment": j.environment,
                "env_approval_pending": j.env_approval_pending,
                "max_payment_sats": j.max_payment_sats,
                "created_at": j.created_at.to_rfc3339(),
            })
        })
        .collect();
    Json(jobs)
}

#[derive(Deserialize)]
struct SubmitComputeJobRequest {
    repo_name: String,
    commit_sha: String,
    job_config_yaml: String,
    max_payment_sats: u64,
    #[serde(default = "default_chain")]
    preferred_chain: String,
    #[serde(default)]
    max_payment_units: Option<u64>,
    #[serde(default)]
    verify: bool,
}

fn default_chain() -> String {
    "lightning".into()
}

async fn submit_compute_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SubmitComputeJobRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;

    let submitter = state.identity.peer_id();
    if active_job_count_for(&state.job_store, &submitter) >= MAX_ACTIVE_JOBS_PER_SUBMITTER {
        return Err(StatusCode::TOO_MANY_REQUESTS);
    }
    let allowance = state
        .config
        .compute
        .as_ref()
        .map(|c| c.credit_allowance_secs)
        .unwrap_or(gt_compute::credit::DEFAULT_CREDIT_ALLOWANCE_SECS);
    let money_path = req
        .preferred_chain
        .parse::<ChainType>()
        .ok()
        .is_some_and(|c| state.payment_manager.backend_for(&c).is_some());
    if !money_path && !state.credit_ledger.may_spend(allowance) {
        return Err(StatusCode::PAYMENT_REQUIRED);
    }

    let job_config = gt_compute::job::parse_ci_config(&req.job_config_yaml)
        .map_err(|_| StatusCode::BAD_REQUEST)?;

    let mut job = ComputeJob::new(
        &state.identity,
        submitter,
        req.repo_name,
        req.commit_sha,
        job_config,
        gt_compute::job::ComputeRequirements::default(),
        req.max_payment_sats,
        req.preferred_chain,
    );
    if let Some(units) = req.max_payment_units {
        let identity = state.identity.clone();
        job = job.with_payment_units(&identity, units);
    }
    job.verify = req.verify
        || state
            .config
            .compute
            .as_ref()
            .is_some_and(|c| c.require_verification);

    let worker_enabled = state
        .config
        .compute
        .as_ref()
        .is_some_and(|c| c.worker_enabled);
    let secret_count = job.job_config.secrets.len();
    let warning = if (!job.job_config.env.is_empty() || secret_count > 0) && !worker_enabled {
        Some(format!(
            "job carries {} env var(s) and {} secret(s) and may run on untrusted remote workers; values are visible to whoever runs the job",
            job.job_config.env.len(),
            secret_count
        ))
    } else {
        None
    };

    let id = job.id.clone();
    state.job_store.add(job);

    Ok(Json(
        serde_json::json!({ "id": id, "status": "Pending", "warning": warning }),
    ))
}

async fn get_compute_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Read,
    )?;
    let job = state.job_store.get(&id).ok_or(StatusCode::NOT_FOUND)?;

    let mut resp = serde_json::json!({
        "id": job.id,
        "submitter": job.submitter.to_string(),
        "repo_name": job.repo_name,
        "commit_sha": job.commit_sha,
        "status": job_status_name(&job.status),
        "permissions": job.permissions,
        "concurrency_group": job.concurrency_group,
        "cancel_in_progress": job.cancel_in_progress,
        "environment": job.environment,
        "environment_url": job.environment_url,
        "env_approval_pending": job.env_approval_pending,
        "max_payment_sats": job.max_payment_sats,
        "preferred_chain": job.preferred_chain,
        "job_config": {
            "name": job.job_config.name,
            "docker_image": job.job_config.docker_image,
            "steps": job.job_config.steps.len(),
            "timeout_secs": job.job_config.timeout_secs,
        },
        "created_at": job.created_at.to_rfc3339(),
    });

    // Include result if completed
    if let Some(result) = state.result_store.for_job(&job.id) {
        let blobs: Vec<serde_json::Value> = result
            .artifact_blobs
            .iter()
            .map(|b| {
                serde_json::json!({
                    "name": b.name,
                    "blob_id": b.blob_id.to_string(),
                    "size": b.size,
                    "local": state.blob_store.has(&b.blob_id),
                })
            })
            .collect();
        resp["result"] = serde_json::json!({
            "id": result.id,
            "exit_code": result.exit_code,
            "execution_time_ms": result.execution_time_ms,
            "worker_id": result.worker_id.to_string(),
            "artifacts": result.artifact_hashes.len(),
            "artifact_blobs": blobs,
            "outputs": result.outputs,
            "summary": result.summary_md,
            "stdout_tail": result.stdout_tail,
            "stderr_tail": result.stderr_tail,
            "resource_usage": {
                "peak_cpu_percent": result.resource_usage.peak_cpu_percent,
                "peak_ram_mb": result.resource_usage.peak_ram_mb,
                "disk_used_mb": result.resource_usage.disk_used_mb,
            },
        });
    }

    // Include payment if exists
    if let Some(channel) = state.payment_manager.for_job(&job.id) {
        resp["payment"] = serde_json::json!({
            "channel_id": channel.id,
            "status": format!("{:?}", channel.status),
            "escrow_amount": channel.escrow_amount,
            "released_amount": channel.released_amount,
        });
    }

    Ok(Json(resp))
}

async fn approve_own_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut s = state.write().await;
    check_local_auth(&headers, &s.api_token)?;
    let job = s.job_store.get(&id).ok_or(StatusCode::NOT_FOUND)?;
    if job.submitter != s.identity.peer_id() {
        return Err(StatusCode::FORBIDDEN);
    }
    if !job.env_approval_pending {
        return Ok(Json(serde_json::json!({
            "approved": true,
            "note": "no approval pending",
        })));
    }
    let environment = job.environment.clone();
    gt_compute::dispatch::approve_job(&mut s.job_store, &id);
    Ok(Json(serde_json::json!({
        "approved": true,
        "environment": environment,
    })))
}

async fn cancel_own_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let target = {
        let s = state.read().await;
        check_local_auth(&headers, &s.api_token)?;
        let job = s.job_store.get(&id).ok_or(StatusCode::NOT_FOUND)?;
        if job.submitter != s.identity.peer_id() {
            return Err(StatusCode::FORBIDDEN);
        }
        match &job.status {
            JobStatus::Matched { worker, .. } => Some(worker.clone()),
            JobStatus::Verifying {
                verifier: Some(v), ..
            } => Some(v.clone()),
            JobStatus::Completed { .. }
            | JobStatus::Failed { .. }
            | JobStatus::Disputed
            | JobStatus::Cancelled => {
                return Ok(Json(serde_json::json!({
                    "cancelled": true,
                    "note": "already terminal",
                })));
            }
            _ => None,
        }
    };
    let remote_notified = match target {
        Some(worker) => {
            crate::dispatch::send_cancel_to_worker(&state, &id, &worker, "submitter cancelled")
                .await
        }
        None => false,
    };
    crate::dispatch::settle_job_payment(&state, &id, false).await;
    {
        let mut s = state.write().await;
        if let Some(job) = s.job_store.get_mut(&id) {
            job.status = JobStatus::Cancelled;
            job.env_approval_pending = false;
        }
        let s_ref = &mut *s;
        gt_compute::dispatch::unblock_ready_jobs(&mut s_ref.job_store, &s_ref.result_store);
    }
    Ok(Json(serde_json::json!({
        "cancelled": true,
        "remote_notified": remote_notified,
    })))
}

async fn reject_own_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let s = state.read().await;
        check_local_auth(&headers, &s.api_token)?;
        let job = s.job_store.get(&id).ok_or(StatusCode::NOT_FOUND)?;
        if job.submitter != s.identity.peer_id() {
            return Err(StatusCode::FORBIDDEN);
        }
        if !job.env_approval_pending {
            return Ok(Json(serde_json::json!({
                "rejected": true,
                "note": "no approval pending",
            })));
        }
    }
    let Json(mut value) = cancel_own_job(State(state), headers, AxPath(id)).await?;
    value["rejected"] = serde_json::json!(true);
    Ok(Json(value))
}

async fn list_compute_offers(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let offers: Vec<serde_json::Value> = state
        .worker_registry
        .available()
        .iter()
        .map(|w| {
            serde_json::json!({
                "worker_id": w.offer.worker_id.to_string(),
                "cpu_cores": w.offer.cpu_cores,
                "ram_mb": w.offer.ram_mb,
                "disk_mb": w.offer.disk_mb,
                "has_gpu": w.offer.has_gpu,
                "hourly_rate_sats": w.offer.hourly_rate_sats,
                "accepted_chains": w.offer.accepted_chains,
                "max_concurrent_jobs": w.offer.max_concurrent_jobs,
                "current_load": w.offer.current_load,
                "completed_jobs": w.offer.completed_jobs,
                "reputation": w.reputation.score,
            })
        })
        .collect();
    Json(offers)
}

async fn get_trust(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let workers: Vec<serde_json::Value> = state
        .worker_registry
        .entries()
        .iter()
        .map(|(id, w)| {
            serde_json::json!({
                "peer_id": id.to_string(),
                "score": w.reputation.score,
                "total_jobs": w.reputation.total_jobs,
                "successful_jobs": w.reputation.successful_jobs,
                "failed_jobs": w.reputation.failed_jobs,
                "disputed_jobs": w.reputation.disputed_jobs,
                "ghosted_jobs": w.reputation.ghosted_jobs,
                "last_seen": w.last_seen.to_rfc3339(),
            })
        })
        .collect();
    let submitters: Vec<serde_json::Value> = state
        .submitter_registry
        .entries()
        .iter()
        .map(|(id, s)| {
            serde_json::json!({
                "peer_id": id.to_string(),
                "score": s.score,
                "submitted": s.submitted,
                "runnable": s.runnable,
                "junk": s.junk,
            })
        })
        .collect();
    Json(serde_json::json!({ "workers": workers, "submitters": submitters }))
}

async fn get_credit(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let (given_secs, taken_secs) = state.credit_ledger.totals();
    let allowance = state
        .config
        .compute
        .as_ref()
        .map(|c| c.credit_allowance_secs)
        .unwrap_or(gt_compute::credit::DEFAULT_CREDIT_ALLOWANCE_SECS);
    let peers: Vec<serde_json::Value> = state
        .credit_ledger
        .peers()
        .iter()
        .map(|(id, e)| {
            serde_json::json!({
                "peer_id": id.to_string(),
                "given_secs": e.given_secs,
                "taken_secs": e.taken_secs,
            })
        })
        .collect();
    Json(serde_json::json!({
        "given_secs": given_secs,
        "taken_secs": taken_secs,
        "own_deficit_secs": state.credit_ledger.own_deficit(),
        "allowance_secs": allowance,
        "may_spend": state.credit_ledger.may_spend(allowance),
        "peers": peers,
    }))
}

#[derive(Deserialize, Default)]
struct StartWorkerRequest {
    #[serde(alias = "cpu_cores")]
    cpu: Option<u32>,
    ram_mb: Option<u64>,
    #[serde(alias = "hourly_rate_sats")]
    rate_sats: Option<u64>,
}

async fn get_worker_status(State(state): State<Arc<RwLock<AppState>>>) -> Json<serde_json::Value> {
    let state = state.read().await;
    let compute = state.config.compute.clone().unwrap_or_default();
    Json(serde_json::json!({
        "worker_enabled": compute.worker_enabled,
        "cpu_cores": compute.max_cpu_cores,
        "ram_mb": compute.max_ram_mb,
        "hourly_rate_sats": compute.hourly_rate_sats,
        "has_gpu": compute.effective_gpu(),
    }))
}

async fn start_worker(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    body: Option<Json<StartWorkerRequest>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    let req = body.map(|Json(r)| r).unwrap_or_default();
    let compute: ComputeConfig = state
        .config
        .enable_worker(req.cpu, req.ram_mb, req.rate_sats)
        .clone();
    let _ = state.config.save(&state.config_path);

    let mut offer_msg = gt_core::protocol::ComputeOfferMessage {
        worker_id: state.identity.peer_id(),
        cpu_cores: compute.max_cpu_cores,
        ram_mb: compute.max_ram_mb,
        disk_mb: compute.max_disk_mb,
        has_gpu: compute.effective_gpu(),
        hourly_rate_sats: compute.hourly_rate_sats,
        accepted_chains: compute.accepted_chains.clone(),
        max_concurrent_jobs: compute.max_concurrent_jobs,
        pow_nonce: gt_core::identity::offer_pow_nonce_with_target(
            &state.identity,
            compute.pow_target_bits,
        ),
        signature: vec![],
    };
    let payload = offer_msg.signable_payload();
    offer_msg.signature = state.identity.sign(&payload);

    let _ = state.dht.announce_compute(offer_msg).await;

    Ok(Json(serde_json::json!({
        "status": "ok",
        "worker_enabled": true,
        "message": "Worker announced on network",
        "cpu_cores": compute.max_cpu_cores,
        "ram_mb": compute.max_ram_mb,
        "rate_sats": compute.hourly_rate_sats,
    })))
}

async fn stop_worker(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    state.config.disable_worker();
    let _ = state.config.save(&state.config_path);
    let address = gt_compute::offer::compute_offer_address(&state.identity.peer_id());
    let _ = state.dht.withdraw_compute(address).await;
    Ok(Json(
        serde_json::json!({ "status": "ok", "worker_enabled": false, "message": "Worker withdrawn from network" }),
    ))
}

async fn remote_submit_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(msg): Json<gt_core::protocol::JobSubmissionMessage>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (compute, data_dir, our_id, submitter_base) = {
        let s = state.read().await;
        check_auth(&headers, &s.api_token, s.config.cluster_token.as_deref())?;
        if !msg.verify() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if !valid_remote_id(&msg.job_id)
            || msg.repo_name.contains('/')
            || msg.repo_name.contains("..")
            || msg
                .repo_name
                .chars()
                .any(|c| c.is_whitespace() || c.is_control())
            || msg.commit_sha.len() < 4
            || !msg.commit_sha.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(StatusCode::BAD_REQUEST);
        }
        let compute = match s.config.compute.clone() {
            Some(c) if c.worker_enabled => c,
            _ => {
                return Ok(Json(serde_json::json!({
                    "accepted": false,
                    "reason": "worker not enabled",
                })))
            }
        };
        if s.job_store.get(&msg.job_id).is_some() {
            return Ok(Json(serde_json::json!({
                "accepted": true,
                "job_id": msg.job_id,
                "note": "duplicate",
            })));
        }
        if active_job_count_for(&s.job_store, &msg.submitter) >= MAX_ACTIVE_JOBS_PER_SUBMITTER {
            return Ok(Json(serde_json::json!({
                "accepted": false,
                "reason": "submitter job quota exceeded",
            })));
        }
        if !s.submitter_registry.is_allowed(&msg.submitter) {
            return Ok(Json(serde_json::json!({
                "accepted": false,
                "reason": "submitter validity too low",
            })));
        }
        let allowance = s
            .config
            .compute
            .as_ref()
            .map(|c| c.credit_allowance_secs)
            .unwrap_or(gt_compute::credit::DEFAULT_CREDIT_ALLOWANCE_SECS);
        let money_path = msg
            .preferred_chain
            .parse::<ChainType>()
            .ok()
            .is_some_and(|c| s.payment_manager.backend_for(&c).is_some());
        if !money_path
            && !s
                .credit_ledger
                .worker_should_serve(&msg.submitter, allowance)
        {
            return Ok(Json(serde_json::json!({
                "accepted": false,
                "reason": "mutual credit exhausted: run a worker to earn credit",
            })));
        }
        let base = crate::fetch::peer_http_base(&s.known_peers, &msg.submitter);
        (
            compute,
            s.config.data_dir.clone(),
            s.identity.peer_id(),
            base,
        )
    };
    if our_id == msg.submitter {
        return Ok(Json(serde_json::json!({
            "accepted": false,
            "reason": "cannot execute own job remotely",
        })));
    }
    let caps = gt_compute::dispatch::LocalCaps {
        cpu_cores: compute.max_cpu_cores,
        ram_mb: compute.max_ram_mb,
        disk_mb: compute.max_disk_mb,
        has_gpu: compute.effective_gpu(),
        has_network: compute.allow_network_jobs,
        docker: matches!(
            compute.execution_mode,
            gt_core::config::ExecutionMode::Docker
        ),
    };
    let req = gt_compute::job::ComputeRequirements {
        min_cpu_cores: msg.min_cpu_cores,
        min_ram_mb: msg.min_ram_mb,
        min_disk_mb: msg.min_disk_mb,
        gpu_required: msg.gpu_required,
        network_access: msg.network_access,
    };
    if !gt_compute::dispatch::caps_match(&caps, &req) {
        return Ok(Json(serde_json::json!({
            "accepted": false,
            "reason": "requirements exceed worker capacity",
        })));
    }
    let job_config: gt_compute::job::JobConfig = match serde_json::from_str(&msg.job_config_json) {
        Ok(config) => config,
        Err(_) => {
            let mut s = state.write().await;
            s.submitter_registry.record_junk(&msg.submitter);
            persist_trust(&s);
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    let Some(base) = submitter_base else {
        return Ok(Json(serde_json::json!({
            "accepted": false,
            "reason": "submitter unreachable",
        })));
    };
    let repo_path = match crate::dispatch::fetch_job_repo(
        &base,
        &msg.submitter.to_string(),
        &msg.repo_name,
        &msg.commit_sha,
        &data_dir.join("jobs").join("repos").join(&msg.job_id),
    )
    .await
    {
        Ok(p) => p,
        Err(e) => {
            let mut s = state.write().await;
            s.submitter_registry.record_junk(&msg.submitter);
            persist_trust(&s);
            return Ok(Json(serde_json::json!({
                "accepted": false,
                "reason": format!("repo fetch failed: {e}"),
            })));
        }
    };
    {
        let mut s = state.write().await;
        let token_perms = job_token_permissions(&job_config.job_token, &msg.submitter);
        s.job_store.add(ComputeJob {
            id: msg.job_id.clone(),
            submitter: msg.submitter.clone(),
            repo_owner: msg.repo_owner.clone(),
            repo_name: msg.repo_name.clone(),
            commit_sha: msg.commit_sha.clone(),
            job_config,
            requirements: req,
            max_payment_sats: msg.max_payment_sats,
            preferred_chain: msg.preferred_chain.clone(),
            max_payment_units: None,
            verify: false,
            run_if: gt_compute::job::RunCondition::Success,
            workflow_job: String::new(),
            matrix_siblings: Vec::new(),
            fail_fast: false,
            concurrency_group: String::new(),
            cancel_in_progress: false,
            environment: String::new(),
            environment_url: String::new(),
            env_approval_pending: false,
            continue_on_error: false,
            max_parallel: 0,
            matrix: std::collections::HashMap::new(),
            dispatch_inputs: std::collections::HashMap::new(),
            needs: Vec::new(),
            call_id: None,
            call_outputs: std::collections::HashMap::new(),
            permissions: token_perms,
            status: JobStatus::Pending,
            created_at: chrono::Utc::now(),
            signature: msg.signature.clone(),
        });
        s.foreign_repos.insert(
            msg.job_id.clone(),
            ForeignRepo {
                submitter: msg.submitter.clone(),
                path: repo_path,
            },
        );
        s.submitter_registry.record_submitted(&msg.submitter);
        persist_trust(&s);
    }
    let payment_details = open_worker_channel(&state, &msg, &our_id).await;
    let worker_wallet = msg
        .preferred_chain
        .parse::<ChainType>()
        .ok()
        .and_then(|c| compute.wallets.get(&c.to_string()).cloned());
    Ok(Json(serde_json::json!({
        "accepted": true,
        "job_id": msg.job_id,
        "payment_details": payment_details,
        "worker_wallet": worker_wallet,
    })))
}

async fn open_worker_channel(
    state: &Arc<RwLock<AppState>>,
    msg: &gt_core::protocol::JobSubmissionMessage,
    our_id: &PeerId,
) -> Option<String> {
    if msg.preferred_chain.parse::<ChainType>().as_ref() != Ok(&ChainType::BitcoinLightning) {
        return None;
    }
    let backend = {
        state
            .read()
            .await
            .payment_manager
            .backend_for(&ChainType::BitcoinLightning)
    }?;
    let amount = msg.max_payment_sats;
    let payer = msg.submitter.to_string();
    let payee = our_id.to_string();
    let info = tokio::task::spawn_blocking(move || backend.create_escrow(amount, &payer, &payee))
        .await
        .ok()?
        .ok()?;
    let channel = PaymentChannel::new(
        msg.job_id.clone(),
        msg.submitter.clone(),
        our_id.clone(),
        ChainType::BitcoinLightning,
        amount,
        msg.submitter.to_string(),
        our_id.to_string(),
    );
    let details = info.payment_details.clone();
    state
        .write()
        .await
        .payment_manager
        .insert_funded(channel, info);
    Some(details)
}

async fn remote_job_result(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(msg): Json<gt_core::protocol::JobResultMessage>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut s = state.write().await;
    check_auth(&headers, &s.api_token, s.config.cluster_token.as_deref())?;
    if !msg.verify() {
        return Err(StatusCode::BAD_REQUEST);
    }
    enum Accept {
        First,
        Verify {
            first_worker: PeerId,
            candidate_result_id: String,
        },
    }
    let (accept, wants_verify) = {
        let job = s.job_store.get(&msg.job_id).ok_or(StatusCode::NOT_FOUND)?;
        let accept = match &job.status {
            JobStatus::Matched { worker, .. } if worker == &msg.worker_id => Accept::First,
            JobStatus::Verifying {
                verifier: Some(verifier),
                first_worker,
                candidate_result_id,
                ..
            } if verifier == &msg.worker_id => Accept::Verify {
                first_worker: first_worker.clone(),
                candidate_result_id: candidate_result_id.clone(),
            },
            _ => {
                return Ok(Json(serde_json::json!({
                    "ok": false,
                    "reason": "job not matched to this worker",
                })))
            }
        };
        (accept, job.verify)
    };
    if msg.result_signature.is_empty() {
        let job_id = msg.job_id.clone();
        let worker_id = msg.worker_id.clone();
        match accept {
            Accept::First => {
                let cancelled = gt_compute::dispatch::fail_job_fail_fast(
                    &mut s.job_store,
                    &job_id,
                    msg.stderr_tail.clone(),
                );
                let s_ref = &mut *s;
                gt_compute::dispatch::unblock_ready_jobs(&mut s_ref.job_store, &s_ref.result_store);
                s.worker_registry.record_failure(&worker_id);
                persist_trust(&s);
                drop(s);
                crate::dispatch::settle_job_payment(&state, &job_id, false).await;
                crate::dispatch::settle_cancelled_siblings(&state, &cancelled).await;
            }
            Accept::Verify { .. } => {
                if let Some(job) = s.job_store.get_mut(&job_id) {
                    if let JobStatus::Verifying { verifier, .. } = &mut job.status {
                        *verifier = None;
                    }
                }
                s.worker_registry.record_failure(&worker_id);
                persist_trust(&s);
                drop(s);
                crate::dispatch::refund_channel_for_payee(&state, &job_id, &worker_id).await;
            }
        }
        return Ok(Json(serde_json::json!({ "ok": true })));
    }
    let result = gt_compute::result::JobResult {
        id: msg.result_id.clone(),
        job_id: msg.job_id.clone(),
        worker_id: msg.worker_id.clone(),
        exit_code: msg.exit_code,
        stdout_hash: msg.stdout_hash.clone(),
        stderr_hash: msg.stderr_hash.clone(),
        stdout_tail: msg.stdout_tail.clone(),
        stderr_tail: msg.stderr_tail.clone(),
        artifact_hashes: msg.artifact_hashes.clone(),
        artifact_blobs: msg.artifact_blobs.clone(),
        execution_time_ms: msg.execution_time_ms,
        outputs: msg.outputs.clone(),
        summary_md: msg.summary_md.clone(),
        resource_usage: gt_compute::result::ResourceUsage {
            peak_cpu_percent: msg.peak_cpu_percent,
            peak_ram_mb: msg.peak_ram_mb,
            disk_used_mb: msg.disk_used_mb,
        },
        completed_at: chrono::Utc::now(),
        signature: msg.result_signature.clone(),
    };
    if !result.verify() {
        s.worker_registry.record_dispute(&msg.worker_id);
        persist_trust(&s);
        return Ok(Json(serde_json::json!({
            "ok": false,
            "reason": "result signature invalid",
        })));
    }
    let job_id = msg.job_id.clone();
    let worker_id = msg.worker_id.clone();
    match accept {
        Accept::First if wants_verify => {
            let result_id = result.id.clone();
            s.result_store.add(result);
            if let Some(job) = s.job_store.get_mut(&job_id) {
                job.status = JobStatus::Verifying {
                    first_worker: worker_id,
                    candidate_result_id: result_id,
                    verifier: None,
                    matched_at: chrono::Utc::now(),
                };
            }
        }
        Accept::First => {
            let cancelled = {
                let inner = &mut *s;
                if result.succeeded() {
                    gt_compute::dispatch::complete_job(
                        &mut inner.job_store,
                        &mut inner.result_store,
                        result,
                    );
                    gt_compute::dispatch::unblock_ready_jobs(
                        &mut inner.job_store,
                        &inner.result_store,
                    );
                    Vec::new()
                } else {
                    let code = result.exit_code;
                    inner.result_store.add(result);
                    let cancelled = gt_compute::dispatch::fail_job_fail_fast(
                        &mut inner.job_store,
                        &job_id,
                        format!("exit code {code}"),
                    );
                    gt_compute::dispatch::unblock_ready_jobs(
                        &mut inner.job_store,
                        &inner.result_store,
                    );
                    cancelled
                }
            };
            s.worker_registry.record_success(&worker_id);
            persist_trust(&s);
            fire_workflow_run_triggers(&mut s, &job_id);
            drop(s);
            crate::dispatch::settle_job_payment_for_worker(&state, &job_id, Some(&worker_id), true)
                .await;
            crate::dispatch::settle_cancelled_siblings(&state, &cancelled).await;
            {
                let ms = {
                    let s = state.read().await;
                    s.result_store.for_job(&job_id).map(|r| r.execution_time_ms)
                };
                if let Some(ms) = ms {
                    record_taken_if_unpaid(&state, &job_id, &worker_id, ms).await;
                }
            }
        }
        Accept::Verify {
            first_worker,
            candidate_result_id,
        } => {
            let matched = s
                .result_store
                .get(&candidate_result_id)
                .map(|c| gt_compute::result::results_match(c, &result))
                .unwrap_or(false);
            if matched {
                let candidate = s.result_store.get(&candidate_result_id).cloned();
                let candidate_ms = candidate.as_ref().map(|c| c.execution_time_ms);
                let cancelled = if let Some(candidate) = candidate {
                    let inner = &mut *s;
                    if candidate.succeeded() {
                        gt_compute::dispatch::complete_job(
                            &mut inner.job_store,
                            &mut inner.result_store,
                            candidate,
                        );
                        gt_compute::dispatch::unblock_ready_jobs(
                            &mut inner.job_store,
                            &inner.result_store,
                        );
                        Vec::new()
                    } else {
                        let code = candidate.exit_code;
                        let cancelled = gt_compute::dispatch::fail_job_fail_fast(
                            &mut inner.job_store,
                            &job_id,
                            format!("exit code {code}"),
                        );
                        gt_compute::dispatch::unblock_ready_jobs(
                            &mut inner.job_store,
                            &inner.result_store,
                        );
                        cancelled
                    }
                } else {
                    Vec::new()
                };
                s.worker_registry.record_success(&first_worker);
                s.worker_registry.record_success(&worker_id);
                persist_trust(&s);
                fire_workflow_run_triggers(&mut s, &job_id);
                drop(s);
                crate::dispatch::settle_job_payment(&state, &job_id, true).await;
                crate::dispatch::settle_cancelled_siblings(&state, &cancelled).await;
                let verifier_ms = result.execution_time_ms;
                if let Some(ms) = candidate_ms {
                    record_taken_if_unpaid(&state, &job_id, &first_worker, ms).await;
                }
                record_taken_if_unpaid(&state, &job_id, &worker_id, verifier_ms).await;
            } else {
                if let Some(job) = s.job_store.get_mut(&job_id) {
                    job.status = JobStatus::Disputed;
                }
                let s_ref = &mut *s;
                gt_compute::dispatch::unblock_ready_jobs(&mut s_ref.job_store, &s_ref.result_store);
                s.worker_registry.record_failure(&first_worker);
                s.worker_registry.record_failure(&worker_id);
                persist_trust(&s);
                fire_workflow_run_triggers(&mut s, &job_id);
                drop(s);
                crate::dispatch::settle_job_payment(&state, &job_id, false).await;
            }
        }
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

async fn remote_cancel_job(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(msg): Json<gt_core::protocol::JobCancelMessage>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let job_id = msg.job_id.clone();
    {
        let s = state.read().await;
        check_auth(&headers, &s.api_token, s.config.cluster_token.as_deref())?;
        if !msg.verify() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if !valid_remote_id(&job_id) || msg.reason.len() > 280 {
            return Err(StatusCode::BAD_REQUEST);
        }
        let job = s.job_store.get(&job_id).ok_or(StatusCode::NOT_FOUND)?;
        if job.submitter != msg.submitter {
            return Ok(Json(serde_json::json!({
                "cancelled": false,
                "reason": "not job submitter",
            })));
        }
        if matches!(
            job.status,
            JobStatus::Completed { .. }
                | JobStatus::Failed { .. }
                | JobStatus::Disputed
                | JobStatus::Cancelled
        ) {
            return Ok(Json(serde_json::json!({
                "cancelled": true,
                "note": "already terminal",
            })));
        }
    }
    crate::dispatch::settle_job_payment(&state, &job_id, false).await;
    {
        let mut s = state.write().await;
        s.job_store.remove(&job_id);
        if let Some(fr) = s.foreign_repos.remove(&job_id) {
            let _ = std::fs::remove_dir_all(&fr.path);
        }
    }
    tracing::info!("cancelled foreign job {job_id} at submitter request");
    Ok(Json(serde_json::json!({ "cancelled": true })))
}

async fn list_payments(State(state): State<Arc<RwLock<AppState>>>) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let channels: Vec<serde_json::Value> = state
        .payment_manager
        .list_channels()
        .iter()
        .map(|c| {
            serde_json::json!({
                "id": c.id,
                "job_id": c.job_id,
                "payer": c.payer.to_string(),
                "payee": c.payee.to_string(),
                "chain": format!("{}", c.chain),
                "escrow_amount": c.escrow_amount,
                "released_amount": c.released_amount,
                "status": format!("{:?}", c.status),
                "created_at": c.created_at.to_rfc3339(),
                "payment_details": c.payment_details,
            })
        })
        .collect();
    Json(channels)
}

// ============ Large File Storage ============

async fn list_blobs(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    let state = state.read().await;
    let ids = state
        .blob_store
        .list()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let blobs: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| {
            let meta = state.blob_store.get_meta(id).ok().flatten();
            let seeder_count = state.blob_tracker.seeder_count(id);
            let leecher_count = state.blob_tracker.leecher_count(id);
            serde_json::json!({
                "blob_id": id.to_string(),
                "size": meta.as_ref().map(|m| m.total_size),
                "piece_count": meta.as_ref().map(|m| m.piece_count()),
                "piece_size": meta.as_ref().map(|m| m.piece_size),
                "seeders": seeder_count,
                "leechers": leecher_count,
            })
        })
        .collect();
    Ok(Json(blobs))
}

#[derive(Deserialize)]
struct UploadBlobRequest {
    path: String,
}

async fn upload_blob(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<UploadBlobRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Write,
    )?;
    let path = PathBuf::from(&req.path);

    if !path.exists() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let meta = state
        .blob_store
        .store_file(&path)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    // Track ourselves as a seeder
    let peer_id = state.identity.peer_id();
    state.blob_tracker.add_seeder(meta.blob_id.clone(), peer_id);

    // Announce on DHT
    let dht_addr = gt_lfs::blob::blob_dht_address(&meta.blob_id);
    let mut announcement = gt_core::protocol::BlobAnnouncement {
        peer_id: state.identity.peer_id(),
        blob_id: meta.blob_id.clone(),
        size: meta.total_size,
        piece_count: meta.piece_count() as u32,
        have_pieces: None,
        signature: Vec::new(),
    };
    announcement.signature = state.identity.sign(&announcement.signable_payload());
    let data = serde_json::to_vec(&announcement).unwrap_or_default();
    let _ = state.dht.announce_blob(dht_addr, data).await;

    Ok(Json(serde_json::json!({
        "blob_id": meta.blob_id.to_string(),
        "size": meta.total_size,
        "piece_count": meta.piece_count(),
        "piece_size": meta.piece_size,
    })))
}

async fn upload_blob_bytes(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Write,
    )?;
    if body.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    if body.len() as u64 > MAX_CACHE_BYTES {
        return Err(StatusCode::PAYLOAD_TOO_LARGE);
    }
    let meta = state
        .blob_store
        .store_bytes(&body)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let peer_id = state.identity.peer_id();
    state.blob_tracker.add_seeder(meta.blob_id.clone(), peer_id);
    let dht_addr = gt_lfs::blob::blob_dht_address(&meta.blob_id);
    let mut announcement = gt_core::protocol::BlobAnnouncement {
        peer_id: state.identity.peer_id(),
        blob_id: meta.blob_id.clone(),
        size: meta.total_size,
        piece_count: meta.piece_count() as u32,
        have_pieces: None,
        signature: Vec::new(),
    };
    announcement.signature = state.identity.sign(&announcement.signable_payload());
    let data = serde_json::to_vec(&announcement).unwrap_or_default();
    let _ = state.dht.announce_blob(dht_addr, data).await;
    Ok(Json(serde_json::json!({
        "blob_id": meta.blob_id.to_string(),
        "size": meta.total_size,
        "piece_count": meta.piece_count(),
        "piece_size": meta.piece_size,
    })))
}

const MAX_CACHE_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Deserialize)]
struct SaveCacheRequest {
    key: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    blob_id: Option<String>,
}

async fn list_caches(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Read,
    )?;
    let mut local: Vec<serde_json::Value> = state
        .cache_store
        .entries()
        .map(|e| {
            serde_json::json!({
                "key": e.key,
                "size": e.size,
                "created_at": e.created_at.to_rfc3339(),
            })
        })
        .collect();
    local.sort_by(|a, b| a["key"].as_str().cmp(&b["key"].as_str()));
    Ok(Json(serde_json::json!({
        "local": local,
        "remote": state.remote_caches.snapshot(),
    })))
}

async fn delete_cache(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(key): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Write,
    )?;
    let entry = state
        .cache_store
        .remove(&key)
        .ok_or(StatusCode::NOT_FOUND)?;
    let mut bytes_freed = 0u64;
    if !state
        .cache_store
        .blob_referenced_elsewhere(&entry.blob_id, &key)
    {
        bytes_freed = entry.size;
        let _ = state.blob_store.remove(&entry.blob_id);
    }
    Ok(Json(serde_json::json!({
        "key": key,
        "removed": true,
        "bytes_freed": bytes_freed,
    })))
}

async fn save_cache(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<SaveCacheRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Contents,
        PermissionLevel::Write,
    )?;
    if !valid_remote_id(&req.key) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(existing) = state.cache_store.get(&req.key) {
        return Ok(Json(serde_json::json!({
            "key": req.key,
            "status": "hit",
            "blob_id": existing.blob_id.to_string(),
            "size": existing.size,
        })));
    }
    let meta = if let Some(blob_id) = req.blob_id.as_deref().filter(|s| !s.is_empty()) {
        let id: gt_core::types::BlobId = blob_id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
        if !state.blob_store.has(&id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        state
            .blob_store
            .get_meta(&id)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
            .ok_or(StatusCode::INTERNAL_SERVER_ERROR)?
    } else if let Some(path) = req.path.as_deref().filter(|s| !s.is_empty()) {
        let file = PathBuf::from(path);
        let md = std::fs::metadata(&file).map_err(|_| StatusCode::BAD_REQUEST)?;
        if !md.is_file() {
            return Err(StatusCode::BAD_REQUEST);
        }
        if md.len() > MAX_CACHE_BYTES {
            return Err(StatusCode::PAYLOAD_TOO_LARGE);
        }
        state
            .blob_store
            .store_file(&file)
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
    } else {
        return Err(StatusCode::BAD_REQUEST);
    };
    let peer_id = state.identity.peer_id();
    state.blob_tracker.add_seeder(meta.blob_id.clone(), peer_id);
    let dht_addr = gt_lfs::blob::blob_dht_address(&meta.blob_id);
    let mut announcement = gt_core::protocol::BlobAnnouncement {
        peer_id: state.identity.peer_id(),
        blob_id: meta.blob_id.clone(),
        size: meta.total_size,
        piece_count: meta.piece_count() as u32,
        have_pieces: None,
        signature: Vec::new(),
    };
    announcement.signature = state.identity.sign(&announcement.signable_payload());
    let data = serde_json::to_vec(&announcement).unwrap_or_default();
    let _ = state.dht.announce_blob(dht_addr, data).await;
    let created_at = chrono::Utc::now();
    state.cache_store.insert(crate::caches::CacheEntry {
        key: req.key.clone(),
        blob_id: meta.blob_id.clone(),
        size: meta.total_size,
        created_at,
    });
    let mut cache_ann = gt_core::protocol::CacheAnnouncement {
        owner: state.identity.peer_id(),
        key: req.key.clone(),
        blob_id: meta.blob_id.clone(),
        size: meta.total_size,
        updated_at: created_at.timestamp(),
        signature: Vec::new(),
    };
    cache_ann.signature = state.identity.sign(&cache_ann.signable_payload());
    let cache_addr = gt_lfs::blob::cache_dht_address(&req.key);
    let cache_data = serde_json::to_vec(&cache_ann).unwrap_or_default();
    let _ = state.dht.announce_collection(cache_addr, cache_data).await;
    Ok(Json(serde_json::json!({
        "key": req.key,
        "status": "stored",
        "blob_id": meta.blob_id.to_string(),
        "size": meta.total_size,
    })))
}

async fn get_cache(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(key): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    {
        let s = state.read().await;
        check_scoped_auth(
            &headers,
            &s.api_token,
            None,
            &s.identity,
            PermissionScope::Contents,
            PermissionLevel::Read,
        )?;
        if let Some(entry) = s.cache_store.get(&key) {
            return Ok(Json(serde_json::json!({
                "key": entry.key,
                "blob_id": entry.blob_id.to_string(),
                "size": entry.size,
                "created_at": entry.created_at.to_rfc3339(),
                "source": "local",
            })));
        }
    }
    let winner = {
        let s = state.read().await;
        s.remote_caches.get_exact(&key).cloned()
    };
    let Some(winner) = winner else {
        let addr = gt_lfs::blob::cache_dht_address(&key);
        let _ = state.read().await.dht.lookup_collection(addr).await;
        return Err(StatusCode::NOT_FOUND);
    };
    let mut candidates = vec![winner.clone()];
    {
        let s = state.read().await;
        for seeder in s.remote_caches.seeders_for(&key) {
            if seeder.seeder != winner.seeder {
                candidates.push(seeder.clone());
            }
        }
    }
    fetch_and_store_remote_cache(&state, &candidates)
        .await
        .ok_or(StatusCode::NOT_FOUND)
}

#[derive(Deserialize, Default)]
struct CacheLookupQuery {
    prefix: Option<String>,
}

/// True when the remote prefix candidate beats the local one: longer keys
/// win outright, and same-length keys defer to the newer timestamp.
fn remote_cache_wins(
    local: &crate::caches::CacheEntry,
    remote: &crate::caches::RemoteCacheEntry,
) -> bool {
    remote.key.len() > local.key.len()
        || (remote.key.len() == local.key.len() && remote.updated_at > local.created_at)
}

async fn find_cache(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    axum::extract::Query(params): axum::extract::Query<CacheLookupQuery>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let (local, remote) = {
        let s = state.read().await;
        check_scoped_auth(
            &headers,
            &s.api_token,
            None,
            &s.identity,
            PermissionScope::Contents,
            PermissionLevel::Read,
        )?;
        let prefix = params.prefix.as_deref().unwrap_or_default();
        if prefix.is_empty() {
            return Err(StatusCode::BAD_REQUEST);
        }
        (
            s.cache_store.newest_with_prefix(prefix).cloned(),
            s.remote_caches.best_with_prefix(prefix).cloned(),
        )
    };
    let use_remote = match (&local, &remote) {
        (None, None) => return Err(StatusCode::NOT_FOUND),
        (Some(_), None) => false,
        (None, Some(_)) => true,
        (Some(local), Some(remote)) => remote_cache_wins(local, remote),
    };
    if !use_remote {
        let entry = local.expect("checked above");
        return Ok(Json(serde_json::json!({
            "key": entry.key,
            "blob_id": entry.blob_id.to_string(),
            "size": entry.size,
            "created_at": entry.created_at.to_rfc3339(),
            "source": "local",
        })));
    }
    let winner = remote.expect("checked above");
    let mut candidates = vec![winner.clone()];
    {
        let s = state.read().await;
        for seeder in s.remote_caches.seeders_for(&winner.key) {
            if seeder.seeder != winner.seeder {
                candidates.push(seeder.clone());
            }
        }
    }
    fetch_and_store_remote_cache(&state, &candidates)
        .await
        .ok_or(StatusCode::NOT_FOUND)
}

async fn download_blob_from_peer(
    client: &reqwest::Client,
    base: &str,
    blob_id: &gt_core::types::BlobId,
    identity: &gt_core::identity::Identity,
) -> Option<Vec<u8>> {
    let url = format!("{base}/api/blobs/{blob_id}/download");
    let path = format!("/api/blobs/{blob_id}/download");
    let mut request = client.get(&url);
    for (name, value) in gt_core::identity::peer_call_headers(identity, "GET", &path, b"") {
        request = request.header(name, value);
    }
    let resp = request.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    if let Some(len) = resp.content_length() {
        if len > MAX_CACHE_BYTES {
            return None;
        }
    }
    let bytes = resp.bytes().await.ok()?;
    if bytes.len() as u64 > MAX_CACHE_BYTES {
        return None;
    }
    Some(bytes.to_vec())
}

/// Fetch a remotely advertised cache archive from its seeders, store it
/// locally, and return the stored entry. Candidates are tried newest-first.
async fn fetch_and_store_remote_cache(
    state: &Arc<RwLock<AppState>>,
    candidates: &[crate::caches::RemoteCacheEntry],
) -> Option<Json<serde_json::Value>> {
    let (known_peers, own_id, identity) = {
        let s = state.read().await;
        (
            s.known_peers.clone(),
            s.identity.peer_id(),
            s.identity.clone(),
        )
    };
    let client = crate::fetch::peer_client()?;
    for candidate in candidates {
        if candidate.seeder == own_id {
            continue;
        }
        let Some(base) = crate::fetch::peer_http_base(&known_peers, &candidate.seeder) else {
            continue;
        };
        let Some(bytes) =
            download_blob_from_peer(&client, &base, &candidate.blob_id, &identity).await
        else {
            continue;
        };
        if bytes.len() as u64 != candidate.size {
            continue;
        }
        let stored = {
            let s = state.read().await;
            s.blob_store.store_bytes(&bytes).ok()
        };
        let Some(meta) = stored else {
            continue;
        };
        if meta.blob_id != candidate.blob_id {
            let s = state.read().await;
            let _ = s.blob_store.remove(&meta.blob_id);
            continue;
        }
        let mut s = state.write().await;
        if let Some(entry) = s.cache_store.get(&candidate.key) {
            return Some(Json(serde_json::json!({
                "key": entry.key,
                "blob_id": entry.blob_id.to_string(),
                "size": entry.size,
                "created_at": entry.created_at.to_rfc3339(),
                "source": "local",
            })));
        }
        let entry = crate::caches::CacheEntry {
            key: candidate.key.clone(),
            blob_id: candidate.blob_id.clone(),
            size: meta.total_size,
            created_at: candidate.updated_at,
        };
        s.cache_store.insert(entry.clone());
        s.blob_tracker
            .add_seeder(candidate.blob_id.clone(), own_id.clone());
        return Some(Json(serde_json::json!({
            "key": entry.key,
            "blob_id": entry.blob_id.to_string(),
            "size": entry.size,
            "created_at": entry.created_at.to_rfc3339(),
            "source": "peer",
            "peer": candidate.seeder.to_string(),
        })));
    }
    None
}

async fn get_blob_info(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    if !state.blob_store.has(&blob_id) {
        return Err(StatusCode::NOT_FOUND);
    }

    let meta = state
        .blob_store
        .get_meta(&blob_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;

    Ok(Json(serde_json::json!({
        "blob_id": blob_id.to_string(),
        "size": meta.as_ref().map(|m| m.total_size),
        "piece_count": meta.as_ref().map(|m| m.piece_count()),
        "piece_size": meta.as_ref().map(|m| m.piece_size),
        "seeders": state.blob_tracker.seeder_count(&blob_id),
        "leechers": state.blob_tracker.leecher_count(&blob_id),
        "pointer": format!(
            "version gitforest-lfs/1\noid sha256:{}\nsize {}\n",
            blob_id,
            meta.as_ref().map(|m| m.total_size).unwrap_or(0)
        ),
    })))
}

const ARTIFACT_FETCH_TIMEOUT_SECS: u64 = 90;

async fn wait_for_artifact_blob(
    state: &Arc<RwLock<AppState>>,
    blob_id: &gt_core::types::BlobId,
    timeout_secs: u64,
) -> bool {
    {
        let s = state.read().await;
        if s.blob_store.has(blob_id) {
            return true;
        }
    }
    {
        let mut s = state.write().await;
        s.wanted_blobs.insert(blob_id.clone());
        let addr = gt_lfs::blob::blob_dht_address(blob_id);
        let _ = s.dht.lookup_blob(addr).await;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        {
            let s = state.read().await;
            if s.blob_store.has(blob_id) {
                return true;
            }
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
    }
}

async fn download_job_artifact(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath((id, name)): AxPath<(String, String)>,
) -> Result<impl IntoResponse, StatusCode> {
    let blob_id = {
        let state = state.read().await;
        check_scoped_auth(
            &headers,
            &state.api_token,
            None,
            &state.identity,
            PermissionScope::Contents,
            PermissionLevel::Read,
        )?;
        let result = state
            .result_store
            .for_job(&id)
            .ok_or(StatusCode::NOT_FOUND)?;
        result
            .artifact_blobs
            .iter()
            .find(|b| b.name == name)
            .map(|b| b.blob_id.clone())
            .ok_or(StatusCode::NOT_FOUND)?
    };
    if !wait_for_artifact_blob(&state, &blob_id, ARTIFACT_FETCH_TIMEOUT_SECS).await {
        return Err(StatusCode::NOT_FOUND);
    }
    let path = {
        let state = state.read().await;
        state.blob_store.blob_path(&blob_id)
    };
    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok(axum::body::Body::from_stream(stream))
}

async fn download_blob(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<impl IntoResponse, StatusCode> {
    let state = state.read().await;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let caller =
        authenticate_model_caller(&headers, &state.api_token, method.as_str(), uri.path(), b"");
    if !blob_accessible(&state, &blob_id, &caller)
        && !bearer_job_permissions(&headers, &state.identity.peer_id())
            .is_some_and(|p| p.allows(PermissionScope::Contents, PermissionLevel::Read))
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let path = state.blob_store.blob_path(&blob_id);
    let file = tokio::fs::File::open(&path).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            StatusCode::NOT_FOUND
        } else {
            StatusCode::INTERNAL_SERVER_ERROR
        }
    })?;
    let stream = tokio_util::io::ReaderStream::new(file);
    Ok(axum::body::Body::from_stream(stream))
}

async fn get_blob_meta(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<gt_lfs::blob::BlobMeta>, StatusCode> {
    let state = state.read().await;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let caller =
        authenticate_model_caller(&headers, &state.api_token, method.as_str(), uri.path(), b"");
    if !blob_accessible(&state, &blob_id, &caller)
        && !bearer_job_permissions(&headers, &state.identity.peer_id())
            .is_some_and(|p| p.allows(PermissionScope::Contents, PermissionLevel::Read))
    {
        return Err(StatusCode::FORBIDDEN);
    }
    let meta = state
        .blob_store
        .get_meta(&blob_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(meta))
}

async fn fetch_blob(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    if state.blob_store.has(&blob_id) {
        return Ok(Json(
            serde_json::json!({"status": "have", "blob_id": blob_id.to_string()}),
        ));
    }
    state.wanted_blobs.insert(blob_id.clone());
    let addr = gt_lfs::blob::blob_dht_address(&blob_id);
    let _ = state.dht.lookup_blob(addr).await;
    Ok(Json(
        serde_json::json!({"status": "fetching", "blob_id": blob_id.to_string()}),
    ))
}

async fn remove_blob(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<StatusCode, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token).map_err(|_| StatusCode::UNAUTHORIZED)?;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    let peer_id = state.identity.peer_id();
    state
        .blob_store
        .remove(&blob_id)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    state.blob_tracker.remove_seeder(&blob_id, &peer_id);

    Ok(StatusCode::OK)
}

async fn list_collections(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let collections: Vec<serde_json::Value> = state
        .collection_store
        .list()
        .iter()
        .map(|m| {
            serde_json::json!({
                "id": m.id,
                "name": m.name,
                "description": m.description,
                "owner": m.owner.to_string(),
                "file_count": m.files.len(),
                "total_size": m.total_size(),
                "metadata": m.metadata,
                "created_at": m.created_at.to_rfc3339(),
            })
        })
        .collect();
    Json(collections)
}

#[derive(Deserialize)]
struct CreateCollectionRequest {
    name: String,
    description: String,
    files: Vec<CollectionFileRequest>,
    #[serde(default)]
    metadata: std::collections::HashMap<String, String>,
}

#[derive(Deserialize)]
struct CollectionFileRequest {
    blob_id: String,
    path: String,
    size: u64,
    media_type: Option<String>,
}

async fn create_collection(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<CreateCollectionRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;

    let files: Vec<gt_core::types::CollectionFile> = req
        .files
        .into_iter()
        .map(|f| {
            let blob_id: gt_core::types::BlobId =
                f.blob_id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
            Ok(gt_core::types::CollectionFile {
                blob_id,
                path: f.path,
                size: f.size,
                media_type: f.media_type,
            })
        })
        .collect::<Result<Vec<_>, StatusCode>>()?;

    let manifest = gt_core::types::CollectionManifest::new(
        &state.identity,
        req.name,
        req.description,
        files,
        req.metadata,
    );

    let id = manifest.id.clone();
    let total_size = manifest.total_size();

    // Announce on DHT
    let dht_addr = gt_lfs::blob::collection_dht_address(&manifest.id);
    let mut announcement = gt_core::protocol::CollectionAnnouncement {
        owner: state.identity.peer_id(),
        manifest_id: manifest.id.clone(),
        name: manifest.name.clone(),
        file_count: manifest.files.len() as u32,
        total_size,
        signature: Vec::new(),
    };
    announcement.signature = state.identity.sign(&announcement.signable_payload());
    let data = serde_json::to_vec(&announcement).unwrap_or_default();
    let _ = state.dht.announce_collection(dht_addr, data).await;

    state.collection_store.add(manifest);

    Ok(Json(serde_json::json!({
        "id": id,
        "status": "created",
        "total_size": total_size,
    })))
}

async fn get_collection(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    let manifest = state
        .collection_store
        .get(&id)
        .ok_or(StatusCode::NOT_FOUND)?;
    let caller =
        authenticate_model_caller(&headers, &state.api_token, method.as_str(), uri.path(), b"");
    if !model_accessible(&state.model_acls, &manifest.id, &caller) {
        return Err(StatusCode::FORBIDDEN);
    }

    Ok(Json(serde_json::json!({
        "id": manifest.id,
        "name": manifest.name,
        "description": manifest.description,
        "owner": manifest.owner.to_string(),
        "files": manifest.files.iter().map(|f| serde_json::json!({
            "blob_id": f.blob_id.to_string(),
            "path": f.path,
            "size": f.size,
            "media_type": f.media_type,
        })).collect::<Vec<_>>(),
        "metadata": manifest.metadata,
        "total_size": manifest.total_size(),
        "verified": manifest.verify(),
        "created_at": manifest.created_at.to_rfc3339(),
        "signature": manifest.signature,
    })))
}

async fn fetch_collection(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    if !valid_remote_id(&id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if state.collection_store.get(&id).is_some() {
        return Ok(Json(serde_json::json!({ "status": "have", "id": id })));
    }
    state.wanted_collections.insert(id.clone());
    let addr = gt_lfs::blob::collection_dht_address(&id);
    let _ = state.dht.lookup_collection(addr).await;
    Ok(Json(serde_json::json!({ "status": "fetching", "id": id })))
}

#[derive(Deserialize)]
struct PublishModelRequest {
    collection_id: String,
}

async fn publish_model(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PublishModelRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Models,
        PermissionLevel::Write,
    )?;
    let manifest = state
        .collection_store
        .get(&req.collection_id)
        .cloned()
        .ok_or(StatusCode::NOT_FOUND)?;
    let meta = gt_lfs::model::ModelMetadata::from_manifest(&manifest)
        .map_err(|_| StatusCode::BAD_REQUEST)?;
    let ann = crate::models::build_announcement(&state.identity, &manifest)
        .ok_or(StatusCode::BAD_REQUEST)?;
    let data = serde_json::to_vec(&ann).unwrap_or_default();
    let name_addr = gt_lfs::model::model_name_dht_address(&manifest.name);
    let arch_addr = gt_lfs::model::model_arch_dht_address(&meta.architecture);
    let _ = state.dht.announce_collection(name_addr, data.clone()).await;
    let _ = state.dht.announce_collection(arch_addr, data).await;
    if let Some(record) = crate::models::ModelRecord::from_manifest(&manifest, true) {
        state
            .known_models
            .insert(record.manifest_id.clone(), record);
    }
    Ok(Json(serde_json::json!({
        "manifest_id": manifest.id,
        "name": manifest.name,
        "architecture": meta.architecture,
        "parameter_count": meta.parameter_count,
        "status": "published",
    })))
}

#[derive(Deserialize, Default)]
struct ModelSearchParams {
    text: Option<String>,
    architecture: Option<String>,
    quantization: Option<String>,
    format: Option<String>,
    min_params: Option<String>,
    max_params: Option<String>,
}

async fn search_models(
    State(state): State<Arc<RwLock<AppState>>>,
    axum::extract::Query(params): axum::extract::Query<ModelSearchParams>,
) -> Json<Vec<serde_json::Value>> {
    let query = gt_lfs::model::ModelQuery {
        text: params.text.clone(),
        architecture: params.architecture.clone(),
        quantization: params.quantization.clone(),
        format: params
            .format
            .as_deref()
            .map(gt_lfs::model::ModelFormat::parse),
        min_params: params
            .min_params
            .as_deref()
            .and_then(|v| gt_lfs::model::parse_param_count(v).ok()),
        max_params: params
            .max_params
            .as_deref()
            .and_then(|v| gt_lfs::model::parse_param_count(v).ok()),
    };
    let state = state.read().await;
    if let Some(text) = params.text.as_deref() {
        let addr = gt_lfs::model::model_name_dht_address(text);
        let _ = state.dht.lookup_collection(addr).await;
    }
    if let Some(arch) = params.architecture.as_deref() {
        let addr = gt_lfs::model::model_arch_dht_address(arch);
        let _ = state.dht.lookup_collection(addr).await;
    }
    let manifests = state.collection_store.list();
    let local = crate::models::local_model_records(&manifests, &query);
    let merged = crate::models::merge_records(local, &state.known_models, &query);
    Json(
        merged
            .iter()
            .map(|r| {
                serde_json::json!({
                    "manifest_id": r.manifest_id,
                    "name": r.name,
                    "architecture": r.architecture,
                    "parameter_count": r.parameter_count,
                    "quantization": r.quantization,
                    "format": r.format,
                    "owner": r.owner.to_string(),
                    "seeders": r.seeders.len(),
                    "local": r.local,
                })
            })
            .collect(),
    )
}

async fn get_model_acl(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    check_local_auth(&headers, &state.api_token)?;
    match state.model_acls.get(&id) {
        None => Ok(Json(serde_json::json!({
            "model_id": id,
            "restricted": false,
            "grants": [],
        }))),
        Some(acl) => Ok(Json(serde_json::json!({
            "model_id": acl.model_id,
            "owner": acl.owner.to_string(),
            "version": acl.version,
            "restricted": true,
            "grants": acl.grants.iter().map(|g| g.to_string()).collect::<Vec<_>>(),
        }))),
    }
}

#[derive(Deserialize)]
struct ModelAclUpdateRequest {
    action: String,
    peer_id: Option<String>,
}

async fn update_model_acl(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
    Json(req): Json<ModelAclUpdateRequest>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_local_auth(&headers, &state.api_token)?;
    if !valid_remote_id(&id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    let identity = state.identity.clone();
    match req.action.as_str() {
        "open" => {
            state.model_acls.remove(&id);
            state.config.model_acl_grants.remove(&id);
        }
        "restrict" => {
            state
                .model_acls
                .entry(id.clone())
                .or_insert_with(|| ModelAcl::new(&identity, &id));
            state.config.model_acl_grants.entry(id.clone()).or_default();
        }
        "grant" => {
            let peer_id: gt_core::identity::PeerId = req
                .peer_id
                .as_deref()
                .ok_or(StatusCode::BAD_REQUEST)?
                .parse()
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let acl = state
                .model_acls
                .entry(id.clone())
                .or_insert_with(|| ModelAcl::new(&identity, &id));
            acl.grant(&identity, peer_id.clone())
                .map_err(|_| StatusCode::FORBIDDEN)?;
            let grants = state.config.model_acl_grants.entry(id.clone()).or_default();
            if !grants.contains(&peer_id) {
                grants.push(peer_id);
            }
        }
        "revoke" => {
            let peer_id: gt_core::identity::PeerId = req
                .peer_id
                .as_deref()
                .ok_or(StatusCode::BAD_REQUEST)?
                .parse()
                .map_err(|_| StatusCode::BAD_REQUEST)?;
            let acl = state.model_acls.get_mut(&id).ok_or(StatusCode::NOT_FOUND)?;
            acl.revoke(&identity, &peer_id)
                .map_err(|_| StatusCode::FORBIDDEN)?;
            if let Some(grants) = state.config.model_acl_grants.get_mut(&id) {
                grants.retain(|g| g != &peer_id);
            }
        }
        _ => return Err(StatusCode::BAD_REQUEST),
    }
    let _ = state.config.save(&state.config_path);
    let version = state.model_acls.get(&id).map(|acl| acl.version);
    Ok(Json(serde_json::json!({
        "status": "updated",
        "model_id": id,
        "restricted": version.is_some(),
        "version": version,
    })))
}

async fn pull_model(
    State(state): State<Arc<RwLock<AppState>>>,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    check_scoped_auth(
        &headers,
        &state.api_token,
        None,
        &state.identity,
        PermissionScope::Models,
        PermissionLevel::Read,
    )?;
    if !valid_remote_id(&id) {
        return Err(StatusCode::BAD_REQUEST);
    }
    if let Some(manifest) = state.collection_store.get(&id).cloned() {
        let mut missing = Vec::new();
        for file in &manifest.files {
            if !state.blob_store.has(&file.blob_id) {
                state.wanted_blobs.insert(file.blob_id.clone());
                missing.push(file.blob_id.clone());
            }
        }
        for blob_id in &missing {
            let addr = gt_lfs::blob::blob_dht_address(blob_id);
            let _ = state.dht.lookup_blob(addr).await;
        }
        if missing.is_empty() {
            crate::fetch::announce_model_seeder(&mut state, &manifest).await;
            return Ok(Json(serde_json::json!({
                "manifest_id": id,
                "status": "complete",
            })));
        }
        return Ok(Json(serde_json::json!({
            "manifest_id": id,
            "status": "fetching",
            "missing_blobs": missing.len(),
        })));
    }
    state.wanted_collections.insert(id.clone());
    let addr = gt_lfs::blob::collection_dht_address(&id);
    let _ = state.dht.lookup_collection(addr).await;
    Ok(Json(serde_json::json!({
        "manifest_id": id,
        "status": "fetching",
    })))
}

#[derive(Debug, serde::Deserialize, serde::Serialize)]
struct InferRequest {
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    max_tokens: Option<u32>,
    #[serde(default)]
    temperature: Option<f32>,
    #[serde(default)]
    system: Option<String>,
    #[serde(default)]
    timeout_secs: Option<u64>,
}

struct InferenceReady {
    backend: String,
    weights: std::path::PathBuf,
    request: gt_compute::llm::LlmRequest,
    model_dir: std::path::PathBuf,
}

enum PreparedInference {
    Ready(InferenceReady),
    Fetching(serde_json::Value),
    Proxy {
        peer: String,
        base: String,
        cluster: String,
        manifest_id: String,
    },
}

fn select_infer_proxy(state: &AppState, manifest_id: &str) -> Option<(PeerId, String)> {
    let seeders = state.collection_seeders.get(manifest_id)?;
    let own = state.identity.peer_id();
    seeders
        .iter()
        .filter(|peer| *peer != &own)
        .find_map(|peer| {
            crate::fetch::peer_http_base(&state.known_peers, peer).map(|base| (peer.clone(), base))
        })
}

struct PeerCall<'a> {
    method: &'a str,
    path: &'a str,
    body: &'a [u8],
}

async fn prepare_inference(
    state: &Arc<RwLock<AppState>>,
    headers: &axum::http::HeaderMap,
    call: PeerCall<'_>,
    id: &str,
    body: &InferRequest,
    allow_remote_manifest: bool,
) -> Result<PreparedInference, StatusCode> {
    let data_dir: std::path::PathBuf;
    let backend: String;
    let manifest: gt_core::types::CollectionManifest;
    let blob_store: Arc<gt_lfs::store::BlobStore>;
    {
        let mut state = state.write().await;
        let caller =
            authenticate_model_caller(headers, &state.api_token, call.method, call.path, call.body);
        let peer_caller = !matches!(caller, ModelCaller::Local);
        if matches!(caller, ModelCaller::Anonymous) {
            let job_write = bearer_job_permissions(headers, &state.identity.peer_id())
                .is_some_and(|p| p.allows(PermissionScope::Models, PermissionLevel::Write));
            if !job_write {
                check_auth(
                    headers,
                    &state.api_token,
                    state.config.cluster_token.as_deref(),
                )?;
            }
        }
        if !valid_remote_id(id) {
            return Err(StatusCode::BAD_REQUEST);
        }
        let Some(found) = state.collection_store.get(id).cloned().or_else(|| {
            state
                .collection_store
                .list()
                .iter()
                .find(|m| m.name == id)
                .map(|m| (*m).clone())
        }) else {
            if allow_remote_manifest && !peer_caller {
                let canonical = if state.known_models.contains_key(id) {
                    Some(id.to_string())
                } else {
                    state
                        .known_models
                        .values()
                        .find(|r| r.name == id)
                        .map(|r| r.manifest_id.clone())
                };
                if let Some(manifest_id) = canonical {
                    let cluster = state.config.cluster_token.clone().filter(|t| !t.is_empty());
                    if let (Some(cluster), Some((peer, base))) =
                        (cluster, select_infer_proxy(&state, &manifest_id))
                    {
                        return Ok(PreparedInference::Proxy {
                            peer: peer.to_string(),
                            base,
                            cluster,
                            manifest_id,
                        });
                    }
                }
            }
            return Err(StatusCode::NOT_FOUND);
        };
        if !model_accessible(&state.model_acls, &found.id, &caller) {
            return Err(StatusCode::FORBIDDEN);
        }
        let missing: Vec<_> = found
            .files
            .iter()
            .filter(|f| !state.blob_store.has(&f.blob_id))
            .collect();
        if !missing.is_empty() {
            let fetching = serde_json::json!({
                "manifest_id": id,
                "status": "fetching",
                "missing_blobs": missing.len(),
            });
            let cluster = state.config.cluster_token.clone().filter(|t| !t.is_empty());
            if !peer_caller {
                if let (Some(cluster), Some((peer, base))) =
                    (cluster, select_infer_proxy(&state, &found.id))
                {
                    return Ok(PreparedInference::Proxy {
                        peer: peer.to_string(),
                        base,
                        cluster,
                        manifest_id: found.id.clone(),
                    });
                }
            }
            for file in &missing {
                state.wanted_blobs.insert(file.blob_id.clone());
                let addr = gt_lfs::blob::blob_dht_address(&file.blob_id);
                let _ = state.dht.lookup_blob(addr).await;
            }
            return Ok(PreparedInference::Fetching(fetching));
        }
        data_dir = state.config.data_dir.clone();
        backend = gt_compute::llm::resolve_backend(state.config.llm_backend.as_deref());
        blob_store = state.blob_store.clone();
        manifest = found;
    }
    let request = infer_llm_request(id, body)?;
    let model_dir = data_dir.join("models").join(id);
    let files =
        gt_lfs::model::materialize_manifest(&blob_store, &manifest, &model_dir).map_err(|e| {
            tracing::warn!("Model {} materialize failed: {e}", sanitize_log(id));
            StatusCode::INTERNAL_SERVER_ERROR
        })?;
    let Some(weights) = gt_lfs::model::pick_weights_file(&files) else {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    };
    Ok(PreparedInference::Ready(InferenceReady {
        backend,
        weights,
        request,
        model_dir,
    }))
}

fn infer_llm_request(
    id: &str,
    body: &InferRequest,
) -> Result<gt_compute::llm::LlmRequest, StatusCode> {
    let mut with = std::collections::HashMap::new();
    with.insert("model".to_string(), id.to_string());
    with.insert("prompt".to_string(), body.prompt.clone());
    if let Some(n) = body.max_tokens {
        with.insert("max-tokens".to_string(), n.to_string());
    }
    if let Some(t) = body.temperature {
        with.insert("temperature".to_string(), t.to_string());
    }
    if let Some(system) = &body.system {
        with.insert("system".to_string(), system.clone());
    }
    if let Some(timeout) = body.timeout_secs {
        with.insert("timeout-secs".to_string(), timeout.to_string());
    }
    gt_compute::llm::LlmRequest::from_with(&with).map_err(|e| {
        tracing::warn!("Bad infer request for {}: {e}", sanitize_log(id));
        StatusCode::BAD_REQUEST
    })
}

async fn proxy_infer(
    base: &str,
    manifest_id: &str,
    body: &serde_json::Value,
    cluster_token: &str,
    identity: &Identity,
    total_timeout_secs: u64,
) -> Result<serde_json::Value, StatusCode> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(total_timeout_secs))
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let url = format!("{base}/api/models/{manifest_id}/infer");
    let path = format!("/api/models/{manifest_id}/infer");
    let raw = serde_json::to_vec(body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut request = client.post(&url).bearer_auth(cluster_token);
    for (name, value) in gt_core::identity::peer_call_headers(identity, "POST", &path, &raw) {
        request = request.header(name, value);
    }
    let resp = request
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                StatusCode::GATEWAY_TIMEOUT
            } else {
                StatusCode::BAD_GATEWAY
            }
        })?;
    if !resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    resp.json::<serde_json::Value>()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)
}

fn forward_infer_body(request: &gt_compute::llm::LlmRequest) -> serde_json::Value {
    serde_json::json!({
        "prompt": request.prompt,
        "max_tokens": request.max_tokens,
        "temperature": request.temperature,
        "system": request.system,
        "timeout_secs": request.timeout_secs,
    })
}

async fn relay_infer_stream(
    base: &str,
    manifest_id: &str,
    body: &serde_json::Value,
    cluster_token: &str,
    identity: &Identity,
    total_timeout_secs: u64,
) -> Result<Response, StatusCode> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(total_timeout_secs))
        .build()
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let url = format!("{base}/api/models/{manifest_id}/infer/stream");
    let path = format!("/api/models/{manifest_id}/infer/stream");
    let raw = serde_json::to_vec(body).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    let mut request = client
        .post(&url)
        .header("authorization", format!("Bearer {cluster_token}"));
    for (name, value) in gt_core::identity::peer_call_headers(identity, "POST", &path, &raw) {
        request = request.header(name, value);
    }
    let resp = request
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await
        .map_err(|_| StatusCode::BAD_GATEWAY)?;
    if !resp.status().is_success() {
        return Err(StatusCode::BAD_GATEWAY);
    }
    Ok((
        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
        axum::body::Body::from_stream(resp.bytes_stream()),
    )
        .into_response())
}

async fn infer_model(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
    body: axum::body::Bytes,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let request: InferRequest =
        serde_json::from_slice(&body).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let ready = match prepare_inference(
        &state,
        &headers,
        PeerCall {
            method: method.as_str(),
            path: uri.path(),
            body: &body,
        },
        &id,
        &request,
        true,
    )
    .await?
    {
        PreparedInference::Fetching(value) => return Ok(Json(value)),
        PreparedInference::Proxy {
            peer,
            base,
            cluster,
            manifest_id,
        } => {
            let llm_request = infer_llm_request(&id, &request)?;
            let forward = forward_infer_body(&llm_request);
            let identity = state.read().await.identity.clone();
            let mut value = proxy_infer(
                &base,
                &manifest_id,
                &forward,
                &cluster,
                &identity,
                llm_request.timeout_secs.saturating_add(30),
            )
            .await
            .inspect_err(|status| {
                tracing::warn!(
                    "Model {} proxied inference via {} failed: {status}",
                    sanitize_log(&id),
                    sanitize_log(&peer),
                );
            })?;
            if let Some(map) = value.as_object_mut() {
                map.insert("proxied".to_string(), serde_json::Value::Bool(true));
                map.insert("served_by".to_string(), serde_json::Value::String(peer));
            }
            return Ok(Json(value));
        }
        PreparedInference::Ready(ready) => ready,
    };
    let outcome = gt_compute::llm::run_inference(
        &ready.backend,
        &ready.weights,
        &ready.request,
        &ready.model_dir,
    )
    .await
    .map_err(|e| {
        tracing::warn!(
            "Model {} inference via {} failed: {e:?}",
            sanitize_log(&id),
            sanitize_log(&ready.backend)
        );
        StatusCode::BAD_GATEWAY
    })?;
    let api_token = state.read().await.api_token.clone();
    if let ModelCaller::Peer(peer) =
        authenticate_model_caller(&headers, &api_token, method.as_str(), uri.path(), &body)
    {
        let mut s = state.write().await;
        s.peer_served.record(&id, false, outcome.elapsed_ms);
        tracing::info!(
            "Served peer inference: model {} to {} in {}ms",
            sanitize_log(&id),
            peer,
            outcome.elapsed_ms
        );
    }
    Ok(Json(serde_json::json!({
        "manifest_id": id,
        "status": "complete",
        "completion": outcome.completion,
        "backend": outcome.backend,
        "truncated": outcome.truncated,
        "elapsed_ms": outcome.elapsed_ms,
    })))
}

type InferenceEventStream = std::pin::Pin<
    Box<dyn tokio_stream::Stream<Item = Result<SseEvent, std::convert::Infallible>> + Send>,
>;

fn inference_event(item: gt_compute::llm::StreamItem) -> SseEvent {
    let payload = match item {
        gt_compute::llm::StreamItem::Chunk(text) => serde_json::json!({"chunk": text}),
        gt_compute::llm::StreamItem::Finished {
            truncated,
            elapsed_ms,
        } => serde_json::json!({"done": {"truncated": truncated, "elapsed_ms": elapsed_ms}}),
        gt_compute::llm::StreamItem::Error(message) => serde_json::json!({"error": message}),
    };
    SseEvent::default().data(payload.to_string())
}

async fn infer_model_stream(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath(id): AxPath<String>,
    body: axum::body::Bytes,
) -> Result<Response, StatusCode> {
    use tokio_stream::StreamExt as _;
    let request: InferRequest =
        serde_json::from_slice(&body).map_err(|_| StatusCode::UNPROCESSABLE_ENTITY)?;
    let ready = match prepare_inference(
        &state,
        &headers,
        PeerCall {
            method: method.as_str(),
            path: uri.path(),
            body: &body,
        },
        &id,
        &request,
        true,
    )
    .await?
    {
        PreparedInference::Fetching(value) => {
            let stream: InferenceEventStream = Box::pin(tokio_stream::once(Ok(SseEvent::default(
            )
            .data(value.to_string()))));
            return Ok(Sse::new(stream).into_response());
        }
        PreparedInference::Proxy {
            peer,
            base,
            cluster,
            manifest_id,
        } => {
            let llm_request = infer_llm_request(&id, &request)?;
            let forward = forward_infer_body(&llm_request);
            let identity = state.read().await.identity.clone();
            return relay_infer_stream(
                &base,
                &manifest_id,
                &forward,
                &cluster,
                &identity,
                llm_request.timeout_secs.saturating_add(30),
            )
            .await
            .inspect_err(|status| {
                tracing::warn!(
                    "Model {} proxied stream via {} failed: {status}",
                    sanitize_log(&id),
                    sanitize_log(&peer),
                );
            });
        }
        PreparedInference::Ready(ready) => ready,
    };
    let rx = gt_compute::llm::run_inference_stream(
        &ready.backend,
        &ready.weights,
        &ready.request,
        &ready.model_dir,
    )
    .await
    .map_err(|e| {
        tracing::warn!(
            "Model {} streaming inference via {} failed: {e:?}",
            sanitize_log(&id),
            sanitize_log(&ready.backend)
        );
        StatusCode::BAD_GATEWAY
    })?;
    let api_token = state.read().await.api_token.clone();
    if let ModelCaller::Peer(peer) =
        authenticate_model_caller(&headers, &api_token, method.as_str(), uri.path(), &body)
    {
        let mut s = state.write().await;
        s.peer_served.record(&id, true, 0);
        tracing::info!(
            "Serving peer stream: model {} to {}",
            sanitize_log(&id),
            peer,
        );
    }
    let stream: InferenceEventStream = Box::pin(
        tokio_stream::wrappers::ReceiverStream::new(rx).map(|item| Ok(inference_event(item))),
    );
    Ok(Sse::new(stream)
        .keep_alive(KeepAlive::default())
        .into_response())
}

async fn get_model(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let mut state = state.write().await;
    if let Some(manifest) = state.collection_store.get(&id).cloned() {
        let meta = gt_lfs::model::ModelMetadata::from_manifest(&manifest).ok();
        let groups = gt_lfs::model::group_shards(&manifest.files);
        let mut shards: Vec<serde_json::Value> = groups
            .iter()
            .map(|(base, members)| {
                serde_json::json!({
                    "base": base,
                    "pieces": members.len(),
                    "size": members.iter().map(|f| f.size).sum::<u64>(),
                })
            })
            .collect();
        shards.sort_by(|a, b| a["base"].as_str().cmp(&b["base"].as_str()));
        let blobs_total = manifest.files.len();
        let blobs_present = manifest
            .files
            .iter()
            .filter(|f| state.blob_store.has(&f.blob_id))
            .count();
        return Ok(Json(serde_json::json!({
            "manifest_id": manifest.id,
            "name": manifest.name,
            "description": manifest.description,
            "owner": manifest.owner.to_string(),
            "files": manifest.files.iter().map(|f| serde_json::json!({
                "blob_id": f.blob_id.to_string(),
                "path": f.path,
                "size": f.size,
                "present": state.blob_store.has(&f.blob_id),
            })).collect::<Vec<_>>(),
            "metadata": manifest.metadata,
            "model": meta,
            "shards": shards,
            "total_size": manifest.total_size(),
            "blobs_present": blobs_present,
            "blobs_total": blobs_total,
            "verified": manifest.verify(),
            "local": true,
        })));
    }
    if let Some(record) = state.known_models.get(&id).cloned() {
        state.wanted_collections.insert(id.clone());
        let addr = gt_lfs::blob::collection_dht_address(&id);
        let _ = state.dht.lookup_collection(addr).await;
        return Ok(Json(serde_json::json!({
            "manifest_id": record.manifest_id,
            "name": record.name,
            "architecture": record.architecture,
            "parameter_count": record.parameter_count,
            "quantization": record.quantization,
            "format": record.format,
            "owner": record.owner.to_string(),
            "seeders": record.seeders.len(),
            "local": false,
            "status": "fetching",
        })));
    }
    Err(StatusCode::NOT_FOUND)
}

// ============ Transfers ============

async fn list_transfers(
    State(state): State<Arc<RwLock<AppState>>>,
) -> Json<Vec<serde_json::Value>> {
    let state = state.read().await;
    let transfers: Vec<serde_json::Value> = state
        .transfer_manager
        .active_transfers()
        .iter()
        .map(|t| {
            serde_json::json!({
                "blob_id": t.blob_id.to_string(),
                "total_size": t.total_size,
                "piece_count": t.piece_count,
                "pieces_done": t.pieces_done,
                "progress": t.progress,
                "state": format!("{:?}", t.state),
                "active_peers": t.active_peers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Json(transfers)
}

async fn get_transfer_status(
    State(state): State<Arc<RwLock<AppState>>>,
    AxPath(blob_id): AxPath<String>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = state.read().await;
    let id: gt_core::types::BlobId = blob_id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;

    let status = state
        .transfer_manager
        .status(&id)
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(Json(serde_json::json!({
        "blob_id": status.blob_id.to_string(),
        "total_size": status.total_size,
        "piece_count": status.piece_count,
        "pieces_done": status.pieces_done,
        "progress": status.progress,
        "state": format!("{:?}", status.state),
        "active_peers": status.active_peers.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
    })))
}

async fn serve_piece(
    State(state): State<Arc<RwLock<AppState>>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    AxPath((id, index)): AxPath<(String, usize)>,
) -> Result<Vec<u8>, StatusCode> {
    let state = state.read().await;
    let blob_id: gt_core::types::BlobId = id.parse().map_err(|_| StatusCode::BAD_REQUEST)?;
    let caller =
        authenticate_model_caller(&headers, &state.api_token, method.as_str(), uri.path(), b"");
    if !blob_accessible(&state, &blob_id, &caller)
        && !bearer_job_permissions(&headers, &state.identity.peer_id())
            .is_some_and(|p| p.allows(PermissionScope::Contents, PermissionLevel::Read))
    {
        return Err(StatusCode::FORBIDDEN);
    }

    let data = state
        .transfer_manager
        .serve_piece(&blob_id, index)
        .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?
        .ok_or(StatusCode::NOT_FOUND)?;

    Ok(data)
}

// ============ Web UI ============

async fn serve_ui(
    State(state): State<Arc<RwLock<AppState>>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    uri: Uri,
) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');
    let mut response = gt_web::serve_static(if path.is_empty() { "index.html" } else { path })
        .await
        .into_response();
    let is_html = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|ct| ct.starts_with("text/html"));
    if is_html && addr.ip().is_loopback() {
        let token = state.read().await.api_token.clone();
        if let Ok(value) = header::HeaderValue::from_str(&session_cookie_value(&token)) {
            response.headers_mut().insert(header::SET_COOKIE, value);
        }
    }
    response
}

// ============ Helpers ============

fn repo_to_json(r: &RepoConfig) -> serde_json::Value {
    let owner_display = if let Some(display_name) = gt_core::identity::load_display_name() {
        let pn = gt_core::identity::PeerName::new(display_name, r.owner.clone());
        pn.short_tag()
    } else {
        r.owner.to_string()
    };
    serde_json::json!({
        "name": r.name,
        "path": r.path.display().to_string(),
        "owner": r.owner.to_string(),
        "description": r.description,
        "seeding": r.seeding,
        "clone_url": format!("gitforest://{}/{}", owner_display, r.name),
    })
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;
    use gt_dht::node::DhtCommand;

    fn test_state(
        name: &str,
    ) -> (
        Arc<RwLock<AppState>>,
        PathBuf,
        tokio::sync::mpsc::Receiver<DhtCommand>,
    ) {
        std::env::set_var("GITFOREST_SKIP_AUTH", "1");
        let tmp = std::env::temp_dir().join(format!("gf_api_test_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        std::env::set_var("GITFOREST_POW_PATH", tmp.join("test.pow"));
        let data_dir = tmp.join("data");
        let config = Config {
            data_dir: data_dir.clone(),
            ..Config::default()
        };
        let config_path = tmp.join("config.toml");
        config.save(&config_path).unwrap();
        let identity = Identity::generate();
        let (dht, cmd_rx) = gt_dht::DhtNodeSender::for_test();
        let blob_store = Arc::new(BlobStore::new(&data_dir));
        let state = AppState {
            config,
            config_path,
            identity,
            dht,
            patch_store: PatchStore::new(),
            mr_store: MergeRequestStore::new(),
            acls: HashMap::new(),
            model_acls: HashMap::new(),
            cache_store: crate::caches::CacheStore::load(&data_dir),
            remote_caches: crate::caches::RemoteCacheDir::default(),
            job_store: ComputeJobStore::new(),
            result_store: JobResultStore::new(),
            worker_registry: WorkerRegistry::new(),
            submitter_registry: SubmitterRegistry::new(),
            payment_manager: PaymentManager::new(),
            credit_ledger: gt_compute::credit::CreditLedger::new(),
            blob_store: blob_store.clone(),
            blob_tracker: BlobTracker::new(),
            collection_store: CollectionStore::new(),
            transfer_manager: TransferManager::new(
                blob_store,
                Arc::new(RwLock::new(BlobTracker::new())),
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
            peer_served: PeerServeStats::default(),
            identity_path: tmp.join("identity.key"),
            analytics_history: VecDeque::new(),
        };
        (Arc::new(RwLock::new(state)), tmp, cmd_rx)
    }

    #[tokio::test]
    async fn test_submit_quota_rejects_when_full() {
        let (state, _tmp, _rx) = test_state("quota");
        let yaml =
            "name: t\ndocker_image: x\ntimeout_secs: 60\nsteps:\n  - name: s\n    run: echo hi\n";
        let job_config = gt_compute::job::parse_ci_config(yaml).unwrap();
        let (peer_id, identity) = {
            let s = state.read().await;
            (s.identity.peer_id(), s.identity.clone())
        };
        {
            let mut s = state.write().await;
            for _ in 0..MAX_ACTIVE_JOBS_PER_SUBMITTER {
                let job = ComputeJob::new(
                    &identity,
                    peer_id.clone(),
                    "repo".into(),
                    "abc123".into(),
                    job_config.clone(),
                    gt_compute::job::ComputeRequirements::default(),
                    0,
                    "lightning".into(),
                );
                s.job_store.add(job);
            }
        }
        let req = SubmitComputeJobRequest {
            repo_name: "repo".into(),
            commit_sha: "abc123".into(),
            job_config_yaml: yaml.into(),
            max_payment_sats: 0,
            preferred_chain: "lightning".into(),
            max_payment_units: None,
            verify: false,
        };
        let err = submit_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(req),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::TOO_MANY_REQUESTS);
    }

    #[tokio::test]
    async fn test_remote_submit_refuses_credit_exhausted_peer() {
        let (state, tmp, _rx) = test_state("credit-remote");
        let submitter = Identity::generate();
        {
            let mut s = state.write().await;
            let compute = gt_core::config::ComputeConfig {
                worker_enabled: true,
                credit_allowance_secs: 10,
                ..Default::default()
            };
            s.config.compute = Some(compute);
            s.credit_ledger.record_given(&submitter.peer_id(), 11);
        }
        let mut msg = gt_core::protocol::JobSubmissionMessage {
            job_id: "job-credit-1".into(),
            submitter: submitter.peer_id(),
            repo_owner: submitter.peer_id(),
            repo_name: "demo".into(),
            commit_sha: "abc123".into(),
            job_config_json: "{}".into(),
            min_cpu_cores: 1,
            min_ram_mb: 512,
            min_disk_mb: 100,
            gpu_required: false,
            network_access: false,
            max_payment_sats: 500,
            preferred_chain: "lightning".into(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["accepted"], false);
        assert!(res.0["reason"].as_str().unwrap().contains("credit"));
        {
            let mut s = state.write().await;
            s.credit_ledger.record_taken(&submitter.peer_id(), 11);
        }
        let mut msg = gt_core::protocol::JobSubmissionMessage {
            job_id: "job-credit-2".into(),
            submitter: submitter.peer_id(),
            repo_owner: submitter.peer_id(),
            repo_name: "demo".into(),
            commit_sha: "abc123".into(),
            job_config_json: r#"{"name":"t","steps":[{"name":"s","run":"echo hi"}],"docker_image":"x","timeout_secs":60,"env":{},"artifacts":[]}"#.into(),
            min_cpu_cores: 1,
            min_ram_mb: 512,
            min_disk_mb: 100,
            gpu_required: false,
            network_access: false,
            max_payment_sats: 500,
            preferred_chain: "lightning".into(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["accepted"], false);
        assert_eq!(res.0["reason"], "submitter unreachable");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_submit_refuses_when_own_credit_exhausted() {
        let (state, tmp, _rx) = test_state("credit-submit");
        let yaml =
            "name: t\ndocker_image: x\ntimeout_secs: 60\nsteps:\n  - name: s\n    run: echo hi\n";
        let req = || SubmitComputeJobRequest {
            repo_name: "repo".into(),
            commit_sha: "abc123".into(),
            job_config_yaml: yaml.into(),
            max_payment_sats: 0,
            preferred_chain: "lightning".into(),
            max_payment_units: None,
            verify: false,
        };
        let res = submit_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(req()),
        )
        .await
        .unwrap();
        assert!(res.0["id"].is_string());
        {
            let mut s = state.write().await;
            let peer = Identity::generate().peer_id();
            s.credit_ledger.record_taken(&peer, 10_000);
        }
        let err = submit_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(req()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::PAYMENT_REQUIRED);
        {
            let mut s = state.write().await;
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: gt_compute::payment::ChainType::BitcoinLightning,
                }));
        }
        let res = submit_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(req()),
        )
        .await
        .unwrap();
        assert!(res.0["id"].is_string());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_payment_backend_registration_skips_bad_config() {
        let mut config = Config::default();
        config.payments.lnd = Some(gt_core::config::LndDaemonConfig {
            grpc_url: "https://127.0.0.1:10009".into(),
            macaroon_path: "/nonexistent/admin.macaroon".into(),
            tls_cert_path: "/nonexistent/tls.cert".into(),
        });
        config.payments.evm = vec![gt_core::config::EvmDaemonConfig {
            network: "not-a-chain".into(),
            rpc_url: "http://127.0.0.1:8545".into(),
            from_address: "0x0".into(),
            signer_command: "true".into(),
            keystore_path: None,
            key_password_file: None,
        }];
        let mut manager = PaymentManager::new();
        register_payment_backends(&config, &mut manager);

        let payer = Identity::generate();
        let payee = Identity::generate();
        let channel = gt_compute::payment::PaymentChannel::new(
            "job-1".into(),
            payer.peer_id(),
            payee.peer_id(),
            ChainType::BitcoinLightning,
            1000,
            "a".into(),
            "b".into(),
        );
        let err = manager.open_channel(channel).unwrap_err();
        assert!(err.to_string().contains("no payment backend"), "{err}");
    }

    #[test]
    fn test_payment_backend_registration_accepts_evm() {
        let closed_port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            port
        };
        let mut config = Config::default();
        config.payments.evm = vec![gt_core::config::EvmDaemonConfig {
            network: "base".into(),
            rpc_url: format!("http://127.0.0.1:{closed_port}"),
            from_address: "0x1111111111111111111111111111111111111111".into(),
            signer_command: "true".into(),
            keystore_path: None,
            key_password_file: None,
        }];
        let mut manager = PaymentManager::new();
        register_payment_backends(&config, &mut manager);

        let payer = Identity::generate();
        let payee = Identity::generate();
        let channel = gt_compute::payment::PaymentChannel::new(
            "job-1".into(),
            payer.peer_id(),
            payee.peer_id(),
            ChainType::EthereumL2(gt_compute::payment::L2Network::Base),
            1000,
            "0xa".into(),
            "0xb".into(),
        );
        let err = manager.open_channel(channel).unwrap_err();
        assert!(!err.to_string().contains("no payment backend"), "{err}");
    }

    #[tokio::test]
    async fn test_rate_limit_middleware_rejects_write_flood() {
        let limiter = Arc::new(crate::ratelimit::RateLimiter::new());
        let app = Router::new()
            .route("/ping", post(|| async { "ok" }))
            .layer(middleware::from_fn(timeout_mw))
            .layer(middleware::from_fn_with_state(limiter, limit_mw));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(
                listener,
                app.into_make_service_with_connect_info::<SocketAddr>(),
            )
            .await
            .unwrap();
        });
        let client = reqwest::Client::builder().no_proxy().build().unwrap();
        let mut saw_429 = false;
        for _ in 0..(crate::ratelimit::WRITE_LIMIT_PER_MIN + 5) {
            let res = client
                .post(format!("http://{addr}/ping"))
                .send()
                .await
                .unwrap();
            if res.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
                assert!(res.headers().contains_key(reqwest::header::RETRY_AFTER));
                saw_429 = true;
                break;
            }
        }
        assert!(saw_429);
    }

    #[tokio::test]
    async fn test_start_worker_creates_config_and_announces() {
        let (state, tmp, mut cmd_rx) = test_state("start");
        let headers = axum::http::HeaderMap::new();
        let body = Some(Json(StartWorkerRequest {
            cpu: Some(4),
            ram_mb: Some(8192),
            rate_sats: Some(1000),
        }));
        let res = start_worker(State(state.clone()), headers, body)
            .await
            .unwrap();
        assert_eq!(res.0["status"], "ok");
        assert_eq!(res.0["worker_enabled"], true);
        {
            let s = state.read().await;
            assert!(s.config.worker_enabled());
            assert_eq!(s.config.compute.as_ref().unwrap().max_cpu_cores, 4);
        }
        let saved = Config::load(&tmp.join("config.toml")).unwrap();
        assert!(saved.worker_enabled());
        assert_eq!(saved.compute.as_ref().unwrap().max_ram_mb, 8192);
        match cmd_rx.try_recv().unwrap() {
            DhtCommand::AnnounceCompute(msg) => {
                assert_eq!(msg.cpu_cores, 4);
                assert_eq!(msg.ram_mb, 8192);
                assert_eq!(msg.hourly_rate_sats, 1000);
            }
            other => panic!("expected AnnounceCompute, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_start_worker_uses_defaults_without_body() {
        let (state, tmp, _cmd_rx) = test_state("defaults");
        let headers = axum::http::HeaderMap::new();
        let res = start_worker(State(state.clone()), headers, None)
            .await
            .unwrap();
        assert_eq!(res.0["status"], "ok");
        let s = state.read().await;
        let compute = s.config.compute.as_ref().unwrap();
        assert!(compute.worker_enabled);
        assert_eq!(compute.max_cpu_cores, 2);
        assert_eq!(compute.max_ram_mb, 4096);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_stop_worker_disables_and_withdraws() {
        let (state, tmp, mut cmd_rx) = test_state("stop");
        let headers = axum::http::HeaderMap::new();
        let _ = start_worker(State(state.clone()), headers.clone(), None).await;
        let _ = cmd_rx.try_recv().unwrap();
        let res = stop_worker(State(state.clone()), headers).await.unwrap();
        assert_eq!(res.0["status"], "ok");
        assert_eq!(res.0["worker_enabled"], false);
        {
            let s = state.read().await;
            assert!(!s.config.worker_enabled());
        }
        let saved = Config::load(&tmp.join("config.toml")).unwrap();
        assert!(!saved.worker_enabled());
        match cmd_rx.try_recv().unwrap() {
            DhtCommand::WithdrawCompute { .. } => {}
            other => panic!("expected WithdrawCompute, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_push_known_peer_dedups_and_caps() {
        let mut known = Vec::new();
        let mut seen = HashMap::new();
        assert!(push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/10.0.0.1/tcp/9418".into(),
            true
        ));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/10.0.0.1/tcp/9418".into(),
            true
        ));
        for i in 0..MAX_KNOWN_PEERS + 10 {
            push_known_peer(
                &mut known,
                &mut seen,
                format!("/ip4/10.0.0.2/tcp/{}", 10000 + i),
                true,
            );
        }
        assert_eq!(known.len(), MAX_KNOWN_PEERS);
        assert!(!known.contains(&"/ip4/10.0.0.1/tcp/9418".to_string()));
        assert!(known.contains(&format!(
            "/ip4/10.0.0.2/tcp/{}",
            10000 + MAX_KNOWN_PEERS + 9
        )));
    }

    #[test]
    fn gossip_reinsert_does_not_extend_ttl() {
        let mut known = Vec::new();
        let mut seen = HashMap::new();
        let entry = "/ip4/192.0.2.10/tcp/51234".to_string();
        assert!(push_known_peer(&mut known, &mut seen, entry.clone(), false));
        let aged = Instant::now() - PEER_ENTRY_TTL;
        seen.insert(entry.clone(), aged);
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            entry.clone(),
            false
        ));
        assert_eq!(seen[&entry], aged);
        assert!(!push_known_peer(&mut known, &mut seen, entry.clone(), true));
        assert!(seen[&entry] > aged);
    }

    #[tokio::test]
    async fn gossip_entries_are_quarantined_from_exchange() {
        let (state, tmp, _cmd_rx) = test_state("peer-quarantine");
        let public_ip: std::net::IpAddr = "8.8.8.8".parse().unwrap();
        let real = "/ip4/35.184.171.148/tcp/9418/api/7878".to_string();
        let junk = "/ip4/192.0.2.10/tcp/51234".to_string();
        {
            let mut guard = state.write().await;
            let s: &mut AppState = &mut guard;
            assert!(push_known_peer(
                &mut s.known_peers,
                &mut s.peer_last_seen,
                real.clone(),
                true
            ));
            if push_known_peer(
                &mut s.known_peers,
                &mut s.peer_last_seen,
                junk.clone(),
                false,
            ) {
                s.gossip_learned.insert(junk.clone());
            }
            let visible = exchange_visible_peers(&s.known_peers, &s.gossip_learned, public_ip);
            assert_eq!(visible, vec![real.clone()]);
            assert!(!push_known_peer(
                &mut s.known_peers,
                &mut s.peer_last_seen,
                junk.clone(),
                true
            ));
            s.gossip_learned.remove(&junk);
            let visible = exchange_visible_peers(&s.known_peers, &s.gossip_learned, public_ip);
            assert_eq!(visible.len(), 2);
            s.peer_last_seen.insert(
                junk.clone(),
                Instant::now() - PEER_ENTRY_TTL - Duration::from_secs(1),
            );
            prune_stale_known_peers(s);
            assert!(!s.known_peers.contains(&junk));
            assert!(!s.gossip_learned.contains(&junk));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn prune_drops_stale_but_keeps_bootstrap_and_fresh() {
        let (state, tmp, _cmd_rx) = test_state("peer-prune");
        let bootstrap = "/ip4/35.184.171.148/tcp/9418/api/7878";
        {
            let mut s = state.write().await;
            s.config.bootstrap_peers = vec![bootstrap.to_string()];
            let old = Instant::now() - PEER_ENTRY_TTL - Duration::from_secs(60);
            s.known_peers = vec![
                bootstrap.to_string(),
                "/ip4/192.0.2.10/tcp/9418/api/7878".to_string(),
                "/ip4/34.69.48.37/tcp/9418/api/7878".to_string(),
                "/ip4/10.9.9.9/tcp/9418".to_string(),
            ];
            s.peer_last_seen.insert(bootstrap.to_string(), old);
            s.peer_last_seen
                .insert("/ip4/192.0.2.10/tcp/9418/api/7878".to_string(), old);
            s.peer_last_seen.insert(
                "/ip4/34.69.48.37/tcp/9418/api/7878".to_string(),
                Instant::now(),
            );
            prune_stale_known_peers(&mut s);
            assert!(s.known_peers.contains(&bootstrap.to_string()));
            assert!(!s
                .known_peers
                .contains(&"/ip4/192.0.2.10/tcp/9418/api/7878".to_string()));
            assert!(s
                .known_peers
                .contains(&"/ip4/34.69.48.37/tcp/9418/api/7878".to_string()));
            assert!(s
                .known_peers
                .contains(&"/ip4/10.9.9.9/tcp/9418".to_string()));
            assert!(!s
                .peer_last_seen
                .contains_key("/ip4/192.0.2.10/tcp/9418/api/7878"));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_push_known_peer_rejects_undialable() {
        let mut known = Vec::new();
        let mut seen = HashMap::new();
        assert!(!push_known_peer(&mut known, &mut seen, "".into(), true));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "not-an-addr".into(),
            true
        ));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/p2p/12D3KooWExample".into(),
            true
        ));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/0.0.0.0/tcp/9418".into(),
            true
        ));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/ip6/::/tcp/9418".into(),
            true
        ));
        assert!(push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/127.0.0.1/tcp/9418/api/7878".into(),
            true
        ));
        assert!(push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/192.168.1.2/tcp/9418".into(),
            true
        ));
        assert!(push_known_peer(
            &mut known,
            &mut seen,
            "/dns/bootstrap.example/tcp/9418/api/7878".into(),
            true
        ));
        assert_eq!(known.len(), 3);
    }

    #[test]
    fn test_tag_bootstrap_addr() {
        let responder = Identity::generate().peer_id().to_string();
        let tagged = tag_bootstrap_addr("/ip4/127.0.0.1/tcp/19418/api/17878", &responder).unwrap();
        assert_eq!(
            tagged,
            format!("/ip4/127.0.0.1/tcp/19418/api/17878/gf/{responder}")
        );
        let stale = format!("/ip4/127.0.0.1/tcp/19418/api/17878/gf/{:064x}", 0);
        let retagged = tag_bootstrap_addr(&stale, &responder).unwrap();
        assert_eq!(retagged, tagged);
        assert!(tag_bootstrap_addr("/ip4/127.0.0.1/tcp/19418/api/17878", "not-a-peer").is_none());
        assert!(tag_bootstrap_addr("garbage", &responder).is_none());
    }

    #[test]
    fn test_resolve_prefers_explicit_api_port() {
        let (state, _tmp, _rx) = test_state("resolve_explicit");
        let target = Identity::generate().peer_id().to_string();
        {
            let mut s = state.blocking_write();
            s.known_peers.push(format!(
                "/ip4/172.30.1.2/tcp/19418/p2p/12D3KooWExample/gf/{target}"
            ));
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/19418/api/17878/gf/{target}"));
        }
        let s = state.blocking_read();
        let resolved = resolve_peer_api_address(&s, &target).unwrap();
        assert_eq!(resolved, ("127.0.0.1".to_string(), 17878));
    }

    #[test]
    fn peer_entry_parts_split_id_and_addr() {
        assert_eq!(
            peer_entry_parts("/ip4/127.0.0.1/tcp/19418/api/17878/gf/abc123"),
            ("abc123".to_string(), "/ip4/127.0.0.1/tcp/19418".to_string())
        );
        assert_eq!(
            peer_entry_parts("/ip4/1.2.3.4/tcp/19418/p2p/12D3KooWExample"),
            (
                "12D3KooWExample".to_string(),
                "/ip4/1.2.3.4/tcp/19418".to_string()
            )
        );
        assert_eq!(
            peer_entry_parts("/ip4/1.2.3.4/tcp/19418/"),
            ("".to_string(), "/ip4/1.2.3.4/tcp/19418".to_string())
        );
        assert_eq!(
            peer_entry_parts("/ip4/172.17.0.2/tcp/9418/p2p/12D3KooWKVY/gf/8fc2a9f9/api/7878"),
            (
                "8fc2a9f9".to_string(),
                "/ip4/172.17.0.2/tcp/9418".to_string()
            )
        );
        assert_eq!(
            peer_entry_parts("/ip4/1.2.3.4/tcp/9418/api/7878/gf/abc123"),
            ("abc123".to_string(), "/ip4/1.2.3.4/tcp/9418".to_string())
        );
    }

    #[test]
    fn canonical_peer_entry_reorders_segments() {
        assert_eq!(
            canonical_peer_entry("/ip4/1.2.3.4/tcp/9418/api/7878/gf/abc123").as_deref(),
            Some("/ip4/1.2.3.4/tcp/9418/gf/abc123/api/7878")
        );
        assert_eq!(
            canonical_peer_entry("/ip4/10.0.0.1/tcp/9418").as_deref(),
            Some("/ip4/10.0.0.1/tcp/9418")
        );
        assert_eq!(canonical_peer_entry(""), None);
        assert_eq!(canonical_peer_entry("/p2p/12D3KooWExample"), None);
        assert_eq!(canonical_peer_entry("/ip4/1.2.3.4/tcp/notaport"), None);
    }

    #[test]
    fn push_known_peer_merges_same_host_port() {
        let mut known = Vec::new();
        let mut seen = HashMap::new();
        assert!(push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/34.69.48.37/tcp/9418/api/7878/gf/8fc2a9f9".into(),
            true
        ));
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/34.69.48.37/tcp/9418/p2p/12D3KooWKVY".into(),
            true
        ));
        assert_eq!(known.len(), 1);
        assert_eq!(
            known[0],
            "/ip4/34.69.48.37/tcp/9418/p2p/12D3KooWKVY/gf/8fc2a9f9/api/7878"
        );
        assert!(seen.contains_key(&known[0]));
        assert_eq!(peer_entry_p2p_id(&known[0]), "12D3KooWKVY");
        assert!(!push_known_peer(
            &mut known,
            &mut seen,
            "/ip4/34.69.48.37/tcp/9418/p2p/12D3KooWOther/gf/other/api/9999".into(),
            true
        ));
        assert_eq!(
            known[0],
            "/ip4/34.69.48.37/tcp/9418/p2p/12D3KooWKVY/gf/8fc2a9f9/api/7878"
        );
    }

    #[test]
    fn test_resolve_skips_metadata_ip() {
        let (state, _tmp, _rx) = test_state("resolve_ssrf");
        let target = Identity::generate().peer_id().to_string();
        {
            let mut s = state.blocking_write();
            s.known_peers.push(format!(
                "/ip4/169.254.169.254/tcp/19418/api/17878/gf/{target}"
            ));
        }
        let s = state.blocking_read();
        assert!(resolve_peer_api_address(&s, &target).is_none());
    }

    #[test]
    fn test_exchange_response_carries_responder_id() {
        let resp = PeerExchangeResponse {
            peer_id: "abc".into(),
            peers: vec!["p".into()],
            caches: Vec::new(),
        };
        let body = serde_json::to_vec(&resp).unwrap();
        let back: PeerExchangeResponse = serde_json::from_slice(&body).unwrap();
        assert_eq!(back.peer_id, "abc");
        assert_eq!(back.peers, vec!["p".to_string()]);
        assert!(back.caches.is_empty());
        let legacy_struct = serde_json::json!({"peer_id": "abc", "peers": ["p"]});
        let back: PeerExchangeResponse = serde_json::from_value(legacy_struct).unwrap();
        assert!(back.caches.is_empty());
        let legacy = serde_json::to_vec(&vec!["p".to_string()]).unwrap();
        assert!(serde_json::from_slice::<PeerExchangeResponse>(&legacy).is_err());
        assert!(serde_json::from_slice::<Vec<String>>(&legacy).is_ok());
    }

    #[tokio::test]
    async fn get_peers_reports_dht_connected_count() {
        let (state, tmp, mut cmd_rx) = test_state("peers-count");
        tokio::spawn(async move {
            while let Some(cmd) = cmd_rx.recv().await {
                if let DhtCommand::GetPeers { reply } = cmd {
                    let _ = reply.send(7);
                }
            }
        });
        let Json(got) = get_peers(State(state.clone())).await;
        assert_eq!(got["connected"], serde_json::json!(7));
        assert_eq!(got["known_addresses"], serde_json::json!(0));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn peer_exchange_hides_unroutable_from_remote() {
        let (state, tmp, _cmd_rx) = test_state("pex-filter");
        {
            let mut s = state.write().await;
            s.known_peers = vec![
                "/ip4/35.184.171.148/tcp/9418/api/7878".to_string(),
                "/ip4/127.0.0.1/tcp/9418/api/7878".to_string(),
                "/ip4/172.17.0.2/tcp/9418/p2p/abc".to_string(),
                "/ip4/192.168.1.2/tcp/9418".to_string(),
                "/ip4/0.0.0.0/tcp/9418".to_string(),
                "/p2p/12D3KooWExample".to_string(),
                "/dns/bootstrap.example/tcp/9418/api/7878".to_string(),
            ];
        }
        let local = ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap());
        let Json(full) = peer_exchange(State(state.clone()), local).await;
        assert_eq!(full.peers.len(), 5);
        let remote = ConnectInfo("203.0.113.9:5555".parse::<SocketAddr>().unwrap());
        let Json(filtered) = peer_exchange(State(state.clone()), remote).await;
        assert_eq!(
            filtered.peers,
            vec![
                "/ip4/35.184.171.148/tcp/9418/api/7878".to_string(),
                "/dns/bootstrap.example/tcp/9418/api/7878".to_string(),
            ]
        );
        let lan = ConnectInfo("192.168.1.9:5555".parse::<SocketAddr>().unwrap());
        let Json(lan_view) = peer_exchange(State(state.clone()), lan).await;
        assert_eq!(
            lan_view.peers,
            vec![
                "/ip4/35.184.171.148/tcp/9418/api/7878".to_string(),
                "/ip4/172.17.0.2/tcp/9418/p2p/abc".to_string(),
                "/ip4/192.168.1.2/tcp/9418".to_string(),
                "/dns/bootstrap.example/tcp/9418/api/7878".to_string(),
            ]
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_write_secret_file_restricts_permissions() {
        let tmp = std::env::temp_dir().join(format!("gf_secret_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let path = tmp.join("api.token");
        write_secret_file(&path, b"topsecret").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"topsecret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            write_secret_file(&path, b"rotated").unwrap();
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_get_blob_meta_roundtrip() {
        let (state, tmp, _cmd_rx) = test_state("meta");
        let file_path = tmp.join("data.bin");
        std::fs::write(&file_path, b"meta-test-payload").unwrap();
        let blob_id = {
            let s = state.read().await;
            s.blob_store.store_file(&file_path).unwrap().blob_id
        };
        let res = get_blob_meta(
            State(state.clone()),
            axum::http::Method::GET,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(blob_id.to_string()),
        )
        .await
        .unwrap();
        assert_eq!(res.0.blob_id, blob_id);
        assert_eq!(res.0.total_size, 17);
        let missing = get_blob_meta(
            State(state.clone()),
            axum::http::Method::GET,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(gt_core::types::BlobId([9u8; 32]).to_string()),
        )
        .await;
        assert!(missing.is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_blob_records_want_and_looks_up() {
        let (state, tmp, mut cmd_rx) = test_state("want");
        let headers = axum::http::HeaderMap::new();
        let id = gt_core::types::BlobId([7u8; 32]);
        let res = fetch_blob(State(state.clone()), headers, AxPath(id.to_string()))
            .await
            .unwrap();
        assert_eq!(res.0["status"], "fetching");
        assert!(state.read().await.wanted_blobs.contains(&id));
        match cmd_rx.try_recv().unwrap() {
            DhtCommand::LookupBlob { .. } => {}
            other => panic!("expected LookupBlob, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_tick_downloads_from_seeder_peer() {
        let tmp = std::env::temp_dir().join(format!("gf_fetch_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let seeder_dir = tmp.join("seeder");
        std::fs::create_dir_all(&seeder_dir).unwrap();
        let data: Vec<u8> = (0..600_000).map(|i| (i % 251) as u8).collect();
        let file_path = seeder_dir.join("model.bin");
        std::fs::write(&file_path, &data).unwrap();
        let seeder_store = std::sync::Arc::new(BlobStore::new(&seeder_dir));
        let meta = seeder_store.store_file(&file_path).unwrap();
        assert_eq!(meta.piece_count(), 3);

        let meta_store = seeder_store.clone();
        let piece_store = seeder_store.clone();
        let app = axum::Router::new()
            .route(
                "/api/blobs/{id}/meta",
                axum::routing::get(
                    move |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        let blob_id: gt_core::types::BlobId = id.parse().unwrap();
                        let m = meta_store.get_meta(&blob_id).unwrap().unwrap();
                        axum::Json(m)
                    },
                ),
            )
            .route(
                "/api/blobs/{id}/pieces/{index}",
                axum::routing::get(
                    move |axum::extract::Path((id, index)): axum::extract::Path<(
                        String,
                        usize,
                    )>| async move {
                        let blob_id: gt_core::types::BlobId = id.parse().unwrap();
                        let m = piece_store.get_meta(&blob_id).unwrap().unwrap();
                        piece_store.read_piece(&blob_id, &m, index).unwrap()
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, ltmp, _cmd_rx) = test_state("fetch");
        let seeder_peer = Identity::generate().peer_id();
        {
            let mut s = state.write().await;
            s.wanted_blobs.insert(meta.blob_id.clone());
            s.blob_tracker
                .add_seeder(meta.blob_id.clone(), seeder_peer.clone());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmTest/gf/{seeder_peer}"
            ));
        }
        crate::fetch::run_fetch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(s.blob_store.has(&meta.blob_id));
            assert!(!s.wanted_blobs.contains(&meta.blob_id));
        }
        let stored = std::fs::read(state.read().await.blob_store.blob_path(&meta.blob_id)).unwrap();
        assert_eq!(stored, data);
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&ltmp);
    }

    #[tokio::test]
    async fn cache_list_reports_local_and_remote_keys() {
        let (state, tmp, _cmd_rx) = test_state("cache-list");
        let owner = Identity::generate();
        {
            let mut s = state.write().await;
            s.cache_store.insert(crate::caches::CacheEntry {
                key: "linux-cargo-abc".into(),
                blob_id: gt_core::types::BlobId([7u8; 32]),
                size: 512,
                created_at: chrono::Utc::now(),
            });
            let mut ann = gt_core::protocol::CacheAnnouncement {
                owner: owner.peer_id(),
                key: "linux-cargo-xyz".into(),
                blob_id: gt_core::types::BlobId([9u8; 32]),
                size: 1024,
                updated_at: 1_700_000_000,
                signature: vec![],
            };
            ann.signature = owner.sign(&ann.signable_payload());
            assert!(s
                .remote_caches
                .merge_announcement(&ann, std::time::Instant::now()));
        }
        let headers = axum::http::HeaderMap::new();
        let Json(value) = list_caches(State(state.clone()), headers).await.unwrap();
        assert_eq!(value["local"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["local"][0]["key"],
            serde_json::json!("linux-cargo-abc")
        );
        assert_eq!(value["remote"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["remote"][0]["key"],
            serde_json::json!("linux-cargo-xyz")
        );
        assert_eq!(value["remote"][0]["seeders"], serde_json::json!(1));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cache_delete_frees_blob_only_when_unshared() {
        let (state, tmp, _cmd_rx) = test_state("cache-delete");
        let blob_id = {
            let s = state.read().await;
            s.blob_store.store_bytes(b"archive-bytes").unwrap().blob_id
        };
        {
            let mut s = state.write().await;
            for key in ["k1", "k2"] {
                s.cache_store.insert(crate::caches::CacheEntry {
                    key: key.into(),
                    blob_id: blob_id.clone(),
                    size: 13,
                    created_at: chrono::Utc::now(),
                });
            }
        }
        let headers = axum::http::HeaderMap::new();
        let Json(first) = delete_cache(State(state.clone()), headers.clone(), AxPath("k1".into()))
            .await
            .unwrap();
        assert_eq!(first["bytes_freed"], serde_json::json!(0));
        assert!(state.read().await.blob_store.has(&blob_id));
        let Json(second) = delete_cache(State(state.clone()), headers, AxPath("k2".into()))
            .await
            .unwrap();
        assert_eq!(second["bytes_freed"], serde_json::json!(13));
        assert!(!state.read().await.blob_store.has(&blob_id));
        assert!(state.read().await.cache_store.get("k1").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn profile_update_rejects_bad_names() {
        let (state, tmp, _cmd_rx) = test_state("profile-bad");
        let headers = axum::http::HeaderMap::new();
        for bad in ["", "   ", "x".repeat(33).as_str(), "a\nb"] {
            let res = update_profile(
                State(state.clone()),
                headers.clone(),
                Json(UpdateProfileRequest {
                    display_name: bad.to_string(),
                }),
            )
            .await;
            assert!(
                matches!(res, Err(StatusCode::UNPROCESSABLE_ENTITY)),
                "{bad:?}"
            );
        }
        let Json(got) = get_profile(State(state.clone())).await;
        assert!(got.get("peer_id").is_some());
        assert!(got.get("display_name").is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn clone_target_name_parses_repo_segments() {
        assert_eq!(
            clone_target_name("https://example.com/foo/bar.git"),
            Some("bar".to_string())
        );
        assert_eq!(
            clone_target_name("gitforest://peer123/myrepo"),
            Some("myrepo".to_string())
        );
        assert_eq!(clone_target_name("file:///tmp/x/"), Some("x".to_string()));
        assert_eq!(clone_target_name("https://example.com/"), None);
        assert_eq!(clone_target_name("https://example.com/../x"), None);
        assert!(clone_url_scheme_ok("https://example.com/a"));
        assert!(clone_url_scheme_ok("gitforest://p/r"));
        assert!(clone_url_scheme_ok("file:///tmp/r"));
        assert!(!clone_url_scheme_ok("ftp://example.com/r"));
        assert!(!clone_url_scheme_ok("/tmp/local-path"));
    }

    #[tokio::test]
    async fn clone_repo_clones_local_fixture_and_rejects_bad_input() {
        let (state, tmp, _cmd_rx) = test_state("clone-repo");
        let src = tmp.join("src");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            let st = std::process::Command::new("git")
                .args(args)
                .current_dir(&src)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@t")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@t")
                .status()
                .unwrap();
            assert!(st.success());
        };
        git(&["init", "-q", "--initial-branch=main"]);
        std::fs::write(src.join("hello.txt"), b"hi").unwrap();
        git(&["add", "."]);
        git(&["commit", "-qm", "init"]);
        let headers = axum::http::HeaderMap::new();
        let parent = tmp.join("clones");
        std::fs::create_dir_all(&parent).unwrap();
        let url = format!("file://{}", src.display());
        let Json(ok) = clone_repo(
            State(state.clone()),
            headers.clone(),
            Json(CloneRepoRequest {
                url: url.clone(),
                dest: parent.to_str().unwrap().to_string(),
            }),
        )
        .await
        .unwrap();
        assert!(parent.join("src").join("hello.txt").exists());
        assert_eq!(ok["url"], serde_json::json!(url));
        let again = clone_repo(
            State(state.clone()),
            headers.clone(),
            Json(CloneRepoRequest {
                url,
                dest: parent.to_str().unwrap().to_string(),
            }),
        )
        .await;
        assert!(matches!(again, Err((StatusCode::CONFLICT, _))));
        let bad_scheme = clone_repo(
            State(state.clone()),
            headers.clone(),
            Json(CloneRepoRequest {
                url: "ftp://example.com/r".into(),
                dest: parent.to_str().unwrap().to_string(),
            }),
        )
        .await;
        assert!(matches!(
            bad_scheme,
            Err((StatusCode::UNPROCESSABLE_ENTITY, _))
        ));
        let bad_dest = clone_repo(
            State(state.clone()),
            headers,
            Json(CloneRepoRequest {
                url: "file:///tmp/does-not-exist-src".into(),
                dest: tmp.join("nope").to_str().unwrap().to_string(),
            }),
        )
        .await;
        assert!(matches!(
            bad_dest,
            Err((StatusCode::UNPROCESSABLE_ENTITY, _))
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn identity_export_import_roundtrip_with_backup() {
        let (state, tmp, _cmd_rx) = test_state("identity-backup");
        let headers = axum::http::HeaderMap::new();
        let dest = tmp.join("backup.enc");
        let Json(exported) = export_identity(
            State(state.clone()),
            headers.clone(),
            Json(IdentityExportRequest {
                path: dest.to_str().unwrap().to_string(),
            }),
        )
        .await
        .unwrap();
        let own = state.read().await.identity.peer_id().to_string();
        assert_eq!(exported["peer_id"], serde_json::json!(own));
        let loaded = gt_core::identity::Identity::load(&dest).unwrap();
        assert_eq!(loaded.peer_id().to_string(), own);
        let again = export_identity(
            State(state.clone()),
            headers.clone(),
            Json(IdentityExportRequest {
                path: dest.to_str().unwrap().to_string(),
            }),
        )
        .await;
        assert!(matches!(again, Err(StatusCode::CONFLICT)));
        let fresh = gt_core::identity::Identity::generate();
        let fresh_path = tmp.join("fresh.key");
        fresh.save(&fresh_path).unwrap();
        {
            let s = state.read().await;
            s.identity.save(&s.identity_path).unwrap();
        }
        let Json(imported) = import_identity(
            State(state.clone()),
            headers.clone(),
            Json(IdentityImportRequest {
                path: fresh_path.to_str().unwrap().to_string(),
            }),
        )
        .await
        .unwrap();
        assert_eq!(
            imported["peer_id"],
            serde_json::json!(fresh.peer_id().to_string())
        );
        assert_eq!(imported["restart_required"], serde_json::json!(true));
        let live = gt_core::identity::Identity::load(&state.read().await.identity_path).unwrap();
        assert_eq!(live.peer_id(), fresh.peer_id());
        let backups: Vec<_> = std::fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("identity.key.bak-"))
            })
            .collect();
        assert_eq!(backups.len(), 1);
        let kept = gt_core::identity::Identity::load(&backups[0].path()).unwrap();
        assert_eq!(kept.peer_id().to_string(), own);
        let garbage = tmp.join("garbage.key");
        std::fs::write(&garbage, b"nope").unwrap();
        let bad = import_identity(
            State(state.clone()),
            headers,
            Json(IdentityImportRequest {
                path: garbage.to_str().unwrap().to_string(),
            }),
        )
        .await;
        assert!(matches!(bad, Err(StatusCode::UNPROCESSABLE_ENTITY)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn reject_cancels_held_job_and_clears_flag() {
        let (state, tmp, _cmd_rx) = test_state("job-reject");
        let id = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            let mut job = ComputeJob::new(
                &id,
                id.peer_id(),
                "repo-a".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            job.submitter = s.identity.peer_id();
            job.environment = "prod".into();
            job.env_approval_pending = true;
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        let headers = axum::http::HeaderMap::new();
        let Json(res) = reject_own_job(
            State(state.clone()),
            headers.clone(),
            AxPath(job_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(res["rejected"], serde_json::json!(true));
        let job = state.read().await.job_store.get(&job_id).unwrap().clone();
        assert!(matches!(job.status, JobStatus::Cancelled));
        assert!(!job.env_approval_pending);
        let Json(again) = reject_own_job(State(state.clone()), headers, AxPath(job_id.clone()))
            .await
            .unwrap();
        assert_eq!(again["note"], serde_json::json!("no approval pending"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn mr_repo(state: &Arc<RwLock<AppState>>, tmp: &std::path::Path, owner_mine: bool) {
        let mut s = state.write().await;
        let owner = if owner_mine {
            s.identity.peer_id()
        } else {
            Identity::generate().peer_id()
        };
        s.config.repos.push(gt_core::config::RepoConfig {
            name: "demo".into(),
            path: tmp.join("repo"),
            owner,
            description: None,
            seeding: true,
        });
    }

    async fn open_mr(state: &Arc<RwLock<AppState>>) -> String {
        let mut s = state.write().await;
        let author = Identity::generate();
        let mr = gt_collab::merge_request::MergeRequest::new(
            &author,
            "fork".into(),
            "feature".into(),
            s.identity.peer_id(),
            "demo".into(),
            "main".into(),
            "Add feature".into(),
            "Please merge".into(),
        );
        let id = mr.id.clone();
        assert!(s.mr_store.add(mr));
        id
    }

    #[tokio::test]
    async fn mr_reject_closes_for_owner_only() {
        let (state, tmp, _cmd_rx) = test_state("mr-reject");
        mr_repo(&state, &tmp, true).await;
        let id = open_mr(&state).await;
        let headers = axum::http::HeaderMap::new();
        let Json(res) = reject_merge_request(
            State(state.clone()),
            headers.clone(),
            AxPath(("demo".to_string(), id.clone())),
        )
        .await
        .unwrap();
        assert_eq!(res["status"], serde_json::json!("closed"));
        let s = state.read().await;
        assert_eq!(
            s.mr_store.status_of(&id),
            gt_core::types::MergeRequestStatus::Closed
        );
        drop(s);
        let missing = reject_merge_request(
            State(state.clone()),
            headers,
            AxPath(("demo".to_string(), "nope".to_string())),
        )
        .await;
        assert!(matches!(missing, Err(StatusCode::NOT_FOUND)));
        let (foreign, ftmp, _frx) = test_state("mr-reject-foreign");
        mr_repo(&foreign, &ftmp, false).await;
        let fid = open_mr(&foreign).await;
        let denied = reject_merge_request(
            State(foreign.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), fid)),
        )
        .await;
        assert!(matches!(denied, Err(StatusCode::FORBIDDEN)));
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&ftmp);
    }

    #[tokio::test]
    async fn mr_accept_fails_closed_when_source_unreachable() {
        let (state, tmp, _cmd_rx) = test_state("mr-accept");
        mr_repo(&state, &tmp, true).await;
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let st = std::process::Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(&repo)
            .status()
            .unwrap();
        assert!(st.success());
        let id = open_mr(&state).await;
        let res = accept_merge_request(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), id.clone())),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::BAD_GATEWAY, _))));
        assert_eq!(
            state.read().await.mr_store.status_of(&id),
            gt_core::types::MergeRequestStatus::Open
        );
        let Json(closed) = reject_merge_request(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), id.clone())),
        )
        .await
        .unwrap();
        assert_eq!(closed["status"], serde_json::json!("closed"));
        let res = accept_merge_request(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), id)),
        )
        .await;
        assert!(matches!(res, Err((StatusCode::CONFLICT, _))));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn ci_job_config() -> gt_compute::job::JobConfig {
        gt_compute::job::JobConfig {
            name: "ci".into(),
            steps: vec![gt_compute::job::JobStep {
                name: "t".into(),
                run: "echo hi".into(),
                condition: gt_compute::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "alpine:3".into(),
            timeout_secs: 60,
            env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        }
    }

    #[tokio::test]
    async fn test_dispatch_tick_skips_when_worker_disabled() {
        let (state, tmp, _cmd_rx) = test_state("dispatch-off");
        let id = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            let job = ComputeJob::new(
                &id,
                id.peer_id(),
                "repo-a".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_dispatch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Pending
            ));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn compute_job_list_carries_env_approval_fields() {
        let (state, tmp, _cmd_rx) = test_state("job-list-env");
        let id = Identity::generate();
        {
            let mut s = state.write().await;
            let mut job = ComputeJob::new(
                &id,
                id.peer_id(),
                "repo-a".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            job.environment = "prod".into();
            job.env_approval_pending = true;
            s.job_store.add(job);
        }
        let Json(jobs) = list_compute_jobs(State(state.clone())).await;
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0]["environment"], serde_json::json!("prod"));
        assert_eq!(jobs[0]["env_approval_pending"], serde_json::json!(true));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_dispatch_tick_leaves_pending_when_docker_down() {
        let (state, tmp, _cmd_rx) = test_state("dispatch-nodocker");
        let id = Identity::generate();
        let repo_dir = tmp.join("repo-a");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let job_id = {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            s.config.compute.as_mut().unwrap().docker_socket = "/nonexistent/docker.sock".into();
            s.config.repos.push(gt_core::config::RepoConfig {
                name: "repo-a".into(),
                path: repo_dir,
                owner: id.peer_id(),
                description: None,
                seeding: true,
            });
            let job = ComputeJob::new(
                &id,
                id.peer_id(),
                "repo-a".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_dispatch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Pending
            ));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn cluster_headers() -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            "Bearer shared".parse::<axum::http::HeaderValue>().unwrap(),
        );
        headers
    }

    fn make_bundle_repo(dir: &std::path::Path) -> (String, Vec<u8>) {
        let run = |args: &[&str]| {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?} failed");
            out
        };
        std::fs::create_dir_all(dir).unwrap();
        run(&["init", "-q"]);
        run(&["config", "user.email", "t@t"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.join("README.md"), "hello\n").unwrap();
        run(&["add", "."]);
        run(&["commit", "-qm", "initial"]);
        let sha = String::from_utf8_lossy(&run(&["rev-parse", "HEAD"]).stdout)
            .trim()
            .to_string();
        run(&["bundle", "create", "repo.bundle", "HEAD"]);
        let bundle = std::fs::read(dir.join("repo.bundle")).unwrap();
        (sha, bundle)
    }

    fn signed_submission(
        submitter: &Identity,
        job_id: &str,
        repo: &str,
        sha: &str,
    ) -> gt_core::protocol::JobSubmissionMessage {
        let mut msg = gt_core::protocol::JobSubmissionMessage {
            job_id: job_id.into(),
            submitter: submitter.peer_id(),
            repo_owner: submitter.peer_id(),
            repo_name: repo.into(),
            commit_sha: sha.into(),
            job_config_json: serde_json::to_string(&ci_job_config()).unwrap(),
            min_cpu_cores: 1,
            min_ram_mb: 512,
            min_disk_mb: 1024,
            gpu_required: false,
            network_access: false,
            max_payment_sats: 500,
            preferred_chain: "lightning".into(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        msg
    }

    #[tokio::test]
    async fn test_remote_submit_fetches_repo_and_stores_job() {
        let tmp = std::env::temp_dir().join(format!("gf_rsubmit_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let (sha, bundle) = make_bundle_repo(&tmp.join("src"));
        let app = axum::Router::new().route(
            "/api/remote/{s}/{repo}/fetch",
            axum::routing::post(move || async move { bundle.clone() }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let submitter = Identity::generate();
        let (state, ltmp, _cmd_rx) = test_state("rsubmit");
        {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            s.config.cluster_token = Some("shared".into());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                submitter.peer_id()
            ));
        }
        let msg = signed_submission(&submitter, "job-remote-1", "demo", &sha);
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["accepted"], true);
        {
            let s = state.read().await;
            let job = s.job_store.get("job-remote-1").unwrap();
            assert!(matches!(job.status, JobStatus::Pending));
            assert_eq!(job.commit_sha, sha);
            let fr = s.foreign_repos.get("job-remote-1").unwrap();
            assert_eq!(fr.submitter, submitter.peer_id());
            let head = std::process::Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&fr.path)
                .output()
                .unwrap();
            assert_eq!(String::from_utf8_lossy(&head.stdout).trim(), sha);
        }
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&ltmp);
    }

    #[tokio::test]
    async fn test_remote_submit_rejects_bad_cases() {
        let submitter = Identity::generate();
        let (state, tmp, _cmd_rx) = test_state("rsubmit-neg");
        {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            s.config.cluster_token = Some("shared".into());
        }
        let mut msg = signed_submission(&submitter, "job-neg-1", "demo", &"a".repeat(40));
        msg.signature = vec![0; 64];
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg)).await;
        assert!(res.is_err());

        let msg = signed_submission(&submitter, "../evil", "demo", &"b".repeat(40));
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg)).await;
        assert!(res.is_err());

        let msg = signed_submission(&submitter, "job-neg-2", "demo", &"c".repeat(40));
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["accepted"], false);
        assert_eq!(res.0["reason"], "submitter unreachable");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_remote_result_completes_matched_job() {
        let (state, tmp, _cmd_rx) = test_state("rresult");
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        let result = gt_compute::result::JobResult::new(
            &worker,
            job_id.clone(),
            0,
            b"build ok\n",
            b"",
            vec![],
            1200,
            gt_compute::result::ResourceUsage {
                peak_cpu_percent: 50.0,
                peak_ram_mb: 512,
                disk_used_mb: 100,
            },
        );
        let mut msg = gt_core::protocol::JobResultMessage {
            job_id: job_id.clone(),
            worker_id: worker.peer_id(),
            result_id: result.id.clone(),
            exit_code: result.exit_code,
            stdout_hash: result.stdout_hash.clone(),
            stderr_hash: result.stderr_hash.clone(),
            stdout_tail: result.stdout_tail.clone(),
            stderr_tail: result.stderr_tail.clone(),
            artifact_hashes: result.artifact_hashes.clone(),
            artifact_blobs: result.artifact_blobs.clone(),
            execution_time_ms: result.execution_time_ms,
            outputs: result.outputs.clone(),
            summary_md: result.summary_md.clone(),
            peak_cpu_percent: result.resource_usage.peak_cpu_percent,
            peak_ram_mb: result.resource_usage.peak_ram_mb,
            disk_used_mb: result.resource_usage.disk_used_mb,
            result_signature: result.signature.clone(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Completed { .. }
            ));
            let stored = s.result_store.for_job(&job_id).unwrap();
            assert!(stored.verify());
            assert_eq!(stored.stdout_tail, "build ok\n");
        }
        let detail = get_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(job_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(detail.0["result"]["exit_code"], 0);
        assert_eq!(detail.0["result"]["stdout_tail"], "build ok\n");
        assert_eq!(detail.0["result"]["stderr_tail"], "");
        assert_eq!(
            detail.0["result"]["resource_usage"]["peak_cpu_percent"],
            50.0
        );
        assert_eq!(detail.0["result"]["resource_usage"]["peak_ram_mb"], 512);
        assert_eq!(detail.0["result"]["resource_usage"]["disk_used_mb"], 100);
        {
            let s = state.read().await;
            assert_eq!(s.credit_ledger.entry(&worker.peer_id()).taken_secs, 2);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn make_result_msg(worker: &Identity, job_id: &str) -> gt_core::protocol::JobResultMessage {
        make_result_msg_with_output(worker, job_id, 0, b"build ok\n", b"")
    }

    fn make_result_msg_with_output(
        worker: &Identity,
        job_id: &str,
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
    ) -> gt_core::protocol::JobResultMessage {
        let result = gt_compute::result::JobResult::new(
            worker,
            job_id.to_string(),
            exit_code,
            stdout,
            stderr,
            vec![],
            1200,
            gt_compute::result::ResourceUsage {
                peak_cpu_percent: 50.0,
                peak_ram_mb: 512,
                disk_used_mb: 100,
            },
        );
        let mut msg = gt_core::protocol::JobResultMessage {
            job_id: job_id.to_string(),
            worker_id: worker.peer_id(),
            result_id: result.id.clone(),
            exit_code: result.exit_code,
            stdout_hash: result.stdout_hash.clone(),
            stderr_hash: result.stderr_hash.clone(),
            stdout_tail: result.stdout_tail.clone(),
            stderr_tail: result.stderr_tail.clone(),
            artifact_hashes: result.artifact_hashes.clone(),
            artifact_blobs: result.artifact_blobs.clone(),
            execution_time_ms: result.execution_time_ms,
            outputs: result.outputs.clone(),
            summary_md: result.summary_md.clone(),
            peak_cpu_percent: result.resource_usage.peak_cpu_percent,
            peak_ram_mb: result.resource_usage.peak_ram_mb,
            disk_used_mb: result.resource_usage.disk_used_mb,
            result_signature: result.signature.clone(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        msg
    }

    #[tokio::test]
    async fn test_result_records_worker_reputation() {
        let (state, tmp, _cmd_rx) = test_state("rresult-rep");
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.worker_registry
                .register(gt_compute::offer::ComputeOffer::new(
                    &worker,
                    4,
                    8192,
                    50000,
                    false,
                    1000,
                    vec![gt_compute::payment::ChainType::BitcoinLightning],
                    HashMap::new(),
                    2,
                ));
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        let msg = make_result_msg(&worker, &job_id);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            let rep = &s.worker_registry.get(&worker.peer_id()).unwrap().reputation;
            assert_eq!(rep.successful_jobs, 1);
            assert!(rep.score > 0.5);
        }
        let trust = get_trust(State(state.clone())).await;
        assert_eq!(trust.0["workers"][0]["successful_jobs"], 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_requeue_records_ghost() {
        let (state, tmp, _cmd_rx) = test_state("requeue-ghost");
        let submitter = Identity::generate();
        let worker = Identity::generate();
        {
            let mut s = state.write().await;
            s.worker_registry
                .register(gt_compute::offer::ComputeOffer::new(
                    &worker,
                    4,
                    8192,
                    50000,
                    false,
                    1000,
                    vec![gt_compute::payment::ChainType::BitcoinLightning],
                    HashMap::new(),
                    2,
                ));
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now() - chrono::Duration::hours(2),
            };
            s.job_store.add(job);
        }
        crate::dispatch::run_submit_tick(state.clone()).await;
        {
            let s = state.read().await;
            let rep = &s.worker_registry.get(&worker.peer_id()).unwrap().reputation;
            assert_eq!(rep.ghosted_jobs, 1);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_remote_submit_rejects_low_validity_submitter() {
        let submitter = Identity::generate();
        let (state, tmp, _cmd_rx) = test_state("rsubmit-validity");
        {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            s.config.cluster_token = Some("shared".into());
            for _ in 0..4 {
                s.submitter_registry.record_junk(&submitter.peer_id());
            }
        }
        let msg = signed_submission(&submitter, "job-x", "demo", &"b".repeat(40));
        let res = remote_submit_job(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["accepted"], false);
        assert_eq!(res.0["reason"], "submitter validity too low");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn signed_push_headers(
        signer: &Identity,
        repo: &str,
        updates: &serde_json::Value,
        body: &[u8],
    ) -> axum::http::HeaderMap {
        let ref_b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(updates).unwrap());
        let payload = gt_core::protocol::push_signable_payload(repo, &ref_b64, body);
        let sig_b64 = base64::engine::general_purpose::STANDARD.encode(signer.sign(&payload));
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-gitforest-push-refs", ref_b64.parse().unwrap());
        headers.insert("x-gitforest-push-sig", sig_b64.parse().unwrap());
        headers
    }

    async fn push_test_repo(
        state: &Arc<RwLock<AppState>>,
        dir: &std::path::Path,
        owner: &Identity,
    ) {
        let repo_path = dir.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        let mut s = state.write().await;
        s.config.repos.push(RepoConfig {
            name: "demo".into(),
            path: repo_path,
            owner: owner.peer_id(),
            description: None,
            seeding: true,
        });
    }

    fn deletion_update(pusher: &str) -> serde_json::Value {
        serde_json::json!([{
            "pusher": pusher,
            "ref_name": "refs/heads/gone",
            "new_sha": "",
            "old_sha": "",
        }])
    }

    #[tokio::test]
    async fn test_push_accepts_valid_signature() {
        let (state, tmp, _cmd_rx) = test_state("push-sig-ok");
        let owner = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let self_id = state.read().await.identity.peer_id().to_string();
        let updates = deletion_update(&owner.peer_id().to_string());
        let headers = signed_push_headers(&owner, "demo", &updates, b"");
        let res = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap();
        assert_eq!(res.0["ok"], true);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_tag_fires_tag_workflow() {
        let (state, tmp, _cmd_rx) = test_state("push-tag");
        let owner = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let repo_path = tmp.join("repo");
        let workflows_dir = repo_path.join(".github").join("workflows");
        std::fs::create_dir_all(&workflows_dir).unwrap();
        std::fs::write(
            workflows_dir.join("release.yml"),
            "name: Release\non:\n  push:\n    tags: ['v*']\njobs:\n  package:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo package\n",
        )
        .unwrap();
        std::fs::write(repo_path.join("README.md"), "# demo\n").unwrap();
        for args in [
            ["config", "user.email", "test@test.com"].as_slice(),
            ["config", "user.name", "Test"].as_slice(),
            ["add", "."].as_slice(),
            ["commit", "-qm", "initial"].as_slice(),
        ] {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(&repo_path)
                .output()
                .unwrap()
                .status
                .success());
        }
        let sha_output = std::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        let sha = String::from_utf8_lossy(&sha_output.stdout)
            .trim()
            .to_string();
        let self_id = state.read().await.identity.peer_id().to_string();
        let updates = serde_json::json!([{
            "pusher": owner.peer_id().to_string(),
            "ref_name": "refs/tags/v1.0",
            "new_sha": sha,
            "old_sha": "",
        }]);
        let headers = signed_push_headers(&owner, "demo", &updates, b"");
        let res = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap();
        assert_eq!(res.0["ok"], true);
        let output = std::process::Command::new("git")
            .args(["rev-parse", "refs/tags/v1.0"])
            .current_dir(&repo_path)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), sha.as_str());
        let guard = state.read().await;
        let runs = guard.workflow_runs.list();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].event, "tag");
        assert_eq!(runs[0].job_ids.len(), 1);
        drop(guard);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_rejects_missing_signature() {
        let (state, tmp, _cmd_rx) = test_state("push-sig-missing");
        let owner = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let self_id = state.read().await.identity.peer_id().to_string();
        let updates = deletion_update(&owner.peer_id().to_string());
        let ref_b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&updates).unwrap());
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("x-gitforest-push-refs", ref_b64.parse().unwrap());
        let err = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_rejects_forged_pusher() {
        let (state, tmp, _cmd_rx) = test_state("push-sig-forge");
        let owner = Identity::generate();
        let attacker = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let self_id = state.read().await.identity.peer_id().to_string();
        let updates = deletion_update(&owner.peer_id().to_string());
        let headers = signed_push_headers(&attacker, "demo", &updates, b"");
        let err = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_rejects_tampered_refs() {
        let (state, tmp, _cmd_rx) = test_state("push-sig-tamper");
        let owner = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let self_id = state.read().await.identity.peer_id().to_string();
        let signed = deletion_update(&owner.peer_id().to_string());
        let mut headers = signed_push_headers(&owner, "demo", &signed, b"");
        let tampered = serde_json::json!([{
            "pusher": owner.peer_id().to_string(),
            "ref_name": "refs/heads/main",
            "new_sha": "ff".repeat(20),
            "old_sha": "",
        }]);
        let tampered_b64 = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&tampered).unwrap());
        headers.insert("x-gitforest-push-refs", tampered_b64.parse().unwrap());
        let err = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_push_rejects_mixed_pushers() {
        let (state, tmp, _cmd_rx) = test_state("push-sig-mixed");
        let owner = Identity::generate();
        let other = Identity::generate();
        push_test_repo(&state, &tmp, &owner).await;
        let self_id = state.read().await.identity.peer_id().to_string();
        let updates = serde_json::json!([
            {"pusher": owner.peer_id().to_string(), "ref_name": "refs/heads/a",
             "new_sha": "", "old_sha": ""},
            {"pusher": other.peer_id().to_string(), "ref_name": "refs/heads/b",
             "new_sha": "", "old_sha": ""},
        ]);
        let headers = signed_push_headers(&owner, "demo", &updates, b"");
        let err = remote_push(
            State(state.clone()),
            AxPath((self_id, "demo".into())),
            headers,
            axum::body::Bytes::new(),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::BAD_REQUEST);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn verify_job_fixture(
        tag: &str,
    ) -> (
        Arc<RwLock<AppState>>,
        std::path::PathBuf,
        Identity,
        Identity,
        String,
    ) {
        let (state, tmp, _cmd_rx) = test_state(tag);
        let submitter = Identity::generate();
        let worker_a = Identity::generate();
        let worker_b = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: ChainType::BitcoinLightning,
                }));
            for worker in [&worker_a, &worker_b] {
                s.worker_registry
                    .register(gt_compute::offer::ComputeOffer::new(
                        worker,
                        4,
                        8192,
                        50000,
                        false,
                        1000,
                        vec![ChainType::BitcoinLightning],
                        HashMap::new(),
                        2,
                    ));
            }
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.verify = true;
            job.status = JobStatus::Matched {
                worker: worker_a.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            s.payment_manager
                .open_channel(gt_compute::payment::PaymentChannel::new(
                    job_id.clone(),
                    submitter.peer_id(),
                    worker_a.peer_id(),
                    ChainType::BitcoinLightning,
                    500,
                    "payer".into(),
                    "payee-a".into(),
                ))
                .unwrap();
            s.payment_manager
                .open_channel(gt_compute::payment::PaymentChannel::new(
                    job_id.clone(),
                    submitter.peer_id(),
                    worker_b.peer_id(),
                    ChainType::BitcoinLightning,
                    500,
                    "payer".into(),
                    "payee-b".into(),
                ))
                .unwrap();
            job_id
        };
        (state, tmp, worker_a, worker_b, job_id)
    }

    #[tokio::test]
    async fn test_verify_first_result_holds_candidate() {
        let (state, tmp, worker_a, _worker_b, job_id) = verify_job_fixture("verify-hold").await;
        let msg = make_result_msg(&worker_a, &job_id);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            let job = s.job_store.get(&job_id).unwrap();
            assert!(matches!(job.status, JobStatus::Verifying { .. }));
            for channel in s.payment_manager.job_channels(&job_id) {
                assert_eq!(channel.status, gt_compute::payment::PaymentStatus::Funded);
            }
            let rep = &s
                .worker_registry
                .get(&worker_a.peer_id())
                .unwrap()
                .reputation;
            assert_eq!(rep.total_jobs, 0);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_verify_match_completes_and_pays_both() {
        let (state, tmp, worker_a, worker_b, job_id) = verify_job_fixture("verify-match").await;
        let first = make_result_msg(&worker_a, &job_id);
        let first_res = remote_job_result(State(state.clone()), cluster_headers(), Json(first))
            .await
            .unwrap();
        assert_eq!(first_res.0["ok"], true);
        let candidate_id = {
            let mut s = state.write().await;
            let job = s.job_store.get_mut(&job_id).unwrap();
            let candidate = match &job.status {
                JobStatus::Verifying {
                    candidate_result_id,
                    ..
                } => candidate_result_id.clone(),
                other => panic!("expected verifying, got {other:?}"),
            };
            if let JobStatus::Verifying { verifier, .. } = &mut job.status {
                *verifier = Some(worker_b.peer_id());
            }
            candidate
        };
        let second = make_result_msg(&worker_b, &job_id);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(second))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            let job = s.job_store.get(&job_id).unwrap();
            match &job.status {
                JobStatus::Completed { result_id } => assert_eq!(result_id, &candidate_id),
                other => panic!("expected completed, got {other:?}"),
            }
            for channel in s.payment_manager.job_channels(&job_id) {
                assert_eq!(channel.status, gt_compute::payment::PaymentStatus::Released);
            }
            for worker in [&worker_a, &worker_b] {
                let rep = &s.worker_registry.get(&worker.peer_id()).unwrap().reputation;
                assert_eq!(rep.successful_jobs, 1);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_verify_mismatch_disputes_and_refunds() {
        let (state, tmp, worker_a, worker_b, job_id) = verify_job_fixture("verify-mismatch").await;
        let first = make_result_msg(&worker_a, &job_id);
        let first_res = remote_job_result(State(state.clone()), cluster_headers(), Json(first))
            .await
            .unwrap();
        assert_eq!(first_res.0["ok"], true);
        {
            let mut s = state.write().await;
            let job = s.job_store.get_mut(&job_id).unwrap();
            if let JobStatus::Verifying { verifier, .. } = &mut job.status {
                *verifier = Some(worker_b.peer_id());
            }
        }
        let lying = make_result_msg_with_output(&worker_b, &job_id, 3, b"other\n", b"");
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(lying))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            let job = s.job_store.get(&job_id).unwrap();
            assert!(matches!(job.status, JobStatus::Disputed));
            for channel in s.payment_manager.job_channels(&job_id) {
                assert_eq!(channel.status, gt_compute::payment::PaymentStatus::Refunded);
            }
            for worker in [&worker_a, &worker_b] {
                let rep = &s.worker_registry.get(&worker.peer_id()).unwrap().reputation;
                assert_eq!(rep.failed_jobs, 1);
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_verify_tick_matches_new_worker_not_first() {
        let (state, tmp, _cmd_rx) = test_state("verify-tick");
        let submitter = Identity::generate();
        let worker_a = Identity::generate();
        let worker_b = Identity::generate();
        let app = Router::new().route(
            "/api/compute/remote/submit",
            post(|| async {
                Json(serde_json::json!({
                    "accepted": true,
                    "job_id": "x",
                    "payment_details": serde_json::Value::Null,
                    "worker_wallet": serde_json::Value::Null,
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app.into_make_service())
                .await
                .unwrap();
        });
        let first_worker_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let hits = first_worker_hits.clone();
        let trap_app = Router::new().route(
            "/api/compute/remote/submit",
            post(move || {
                let hits = hits.clone();
                async move {
                    hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Json(serde_json::json!({ "accepted": true }))
                }
            }),
        );
        let trap_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let trap_port = trap_listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(trap_listener, trap_app.into_make_service())
                .await
                .unwrap();
        });
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            for worker in [&worker_a, &worker_b] {
                s.worker_registry
                    .register(gt_compute::offer::ComputeOffer::new(
                        worker,
                        4,
                        8192,
                        50000,
                        false,
                        1000,
                        vec![ChainType::BitcoinLightning],
                        HashMap::new(),
                        2,
                    ));
            }
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/gf/{}",
                worker_b.peer_id()
            ));
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{trap_port}/gf/{}",
                worker_a.peer_id()
            ));
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.verify = true;
            job.status = JobStatus::Verifying {
                first_worker: worker_a.peer_id(),
                candidate_result_id: "res-1".into(),
                verifier: None,
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_submit_tick(state.clone()).await;
        {
            let s = state.read().await;
            let job = s.job_store.get(&job_id).unwrap();
            match &job.status {
                JobStatus::Verifying { verifier, .. } => {
                    assert_eq!(verifier.as_ref(), Some(&worker_b.peer_id()));
                }
                other => panic!("expected verifying with verifier, got {other:?}"),
            }
        }
        assert_eq!(
            first_worker_hits.load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_offer_query_due_skips_self_and_throttles_repeats() {
        let me = Identity::generate();
        let worker = Identity::generate();
        let other = Identity::generate();
        let mut queried = std::collections::HashMap::new();
        assert!(!offer_query_due(&mut queried, &me.peer_id(), &me.peer_id()));
        assert!(offer_query_due(
            &mut queried,
            &me.peer_id(),
            &worker.peer_id()
        ));
        assert!(!offer_query_due(
            &mut queried,
            &me.peer_id(),
            &worker.peer_id()
        ));
        assert!(offer_query_due(
            &mut queried,
            &me.peer_id(),
            &other.peer_id()
        ));
        queried.insert(
            worker.peer_id().to_string(),
            std::time::Instant::now() - std::time::Duration::from_secs(901),
        );
        assert!(offer_query_due(
            &mut queried,
            &me.peer_id(),
            &worker.peer_id()
        ));
    }

    #[test]
    fn test_auth_skip_flag_is_explicit() {
        let headers = axum::http::HeaderMap::new();
        assert!(check_auth_with_skip(&headers, "tok-9", None, true).is_ok());
        assert_eq!(
            check_auth_with_skip(&headers, "tok-9", None, false).unwrap_err(),
            StatusCode::UNAUTHORIZED
        );
        let mut authed = axum::http::HeaderMap::new();
        authed.insert(
            "authorization",
            format!("{} {}", "Bearer", "tok-9")
                .parse::<axum::http::HeaderValue>()
                .unwrap(),
        );
        assert!(check_auth_with_skip(&authed, "tok-9", None, false).is_ok());
        assert!(check_auth_with_skip(&authed, "other", Some("tok-9"), false).is_ok());
    }

    #[tokio::test]
    async fn test_submit_applies_payment_units_and_resigns() {
        let (state, tmp, _cmd_rx) = test_state("units");
        let yaml =
            "name: t\ndocker_image: x\ntimeout_secs: 60\nsteps:\n  - name: s\n    run: echo hi\n";
        let req = SubmitComputeJobRequest {
            repo_name: "repo".into(),
            commit_sha: "abc123".into(),
            job_config_yaml: yaml.into(),
            max_payment_sats: 1000,
            preferred_chain: "base".into(),
            max_payment_units: Some(2_000_000_000_000_000),
            verify: false,
        };
        let res = submit_compute_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(req),
        )
        .await
        .unwrap();
        let job_id = res.0["id"].as_str().unwrap().to_string();
        let s = state.read().await;
        let job = s.job_store.get(&job_id).unwrap();
        assert_eq!(job.payment_units(), 2_000_000_000_000_000);
        assert!(job.verify());
        drop(s);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_submit_forces_verify_under_policy() {
        let yaml =
            "name: t\ndocker_image: x\ntimeout_secs: 60\nsteps:\n  - name: s\n    run: echo hi\n";
        for (tag, policy, requested, expected) in [
            ("policy-off", false, false, false),
            ("policy-off-req", false, true, true),
            ("policy-on", true, false, true),
        ] {
            let (state, tmp, _cmd_rx) = test_state(tag);
            {
                let mut s = state.write().await;
                s.config.compute = Some(gt_core::config::ComputeConfig {
                    require_verification: policy,
                    ..Default::default()
                });
            }
            let req = SubmitComputeJobRequest {
                repo_name: "repo".into(),
                commit_sha: "abc123".into(),
                job_config_yaml: yaml.into(),
                max_payment_sats: 1000,
                preferred_chain: "base".into(),
                max_payment_units: None,
                verify: requested,
            };
            let res = submit_compute_job(
                State(state.clone()),
                axum::http::HeaderMap::new(),
                Json(req),
            )
            .await
            .unwrap();
            let job_id = res.0["id"].as_str().unwrap().to_string();
            let s = state.read().await;
            let job = s.job_store.get(&job_id).unwrap();
            assert_eq!(job.verify, expected, "case {tag}");
            drop(s);
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    #[tokio::test]
    async fn test_submit_warns_on_env_for_remote_workers() {
        let yaml_env = "name: t\ndocker_image: x\ntimeout_secs: 60\nenv:\n  TOKEN: abc\nsteps:\n  - name: s\n    run: echo hi\n";
        let yaml_secrets = "name: t\ndocker_image: x\ntimeout_secs: 60\nsecrets:\n  TOKEN: abc\nsteps:\n  - name: s\n    run: echo hi\n";
        let yaml_plain =
            "name: t\ndocker_image: x\ntimeout_secs: 60\nsteps:\n  - name: s\n    run: echo hi\n";
        for (tag, yaml, worker_enabled, expect_warning) in [
            ("env-remote", yaml_env, false, true),
            ("env-local", yaml_env, true, false),
            ("secrets-remote", yaml_secrets, false, true),
            ("secrets-local", yaml_secrets, true, false),
            ("plain-remote", yaml_plain, false, false),
        ] {
            let (state, tmp, _cmd_rx) = test_state(tag);
            {
                let mut s = state.write().await;
                s.config.compute = Some(gt_core::config::ComputeConfig {
                    worker_enabled,
                    ..Default::default()
                });
            }
            let req = SubmitComputeJobRequest {
                repo_name: "repo".into(),
                commit_sha: "abc123".into(),
                job_config_yaml: yaml.into(),
                max_payment_sats: 1000,
                preferred_chain: "base".into(),
                max_payment_units: None,
                verify: false,
            };
            let res = submit_compute_job(
                State(state.clone()),
                axum::http::HeaderMap::new(),
                Json(req),
            )
            .await
            .unwrap();
            assert_eq!(res.0["warning"].is_string(), expect_warning, "case {tag}");
            if expect_warning {
                assert!(
                    res.0["warning"]
                        .as_str()
                        .unwrap()
                        .contains("untrusted remote workers"),
                    "case {tag}"
                );
            }
            let _ = std::fs::remove_dir_all(&tmp);
        }
    }

    #[tokio::test]
    async fn test_settle_pays_winner_and_refunds_loser() {
        let (state, tmp, _cmd_rx) = test_state("settle");
        let worker_a = Identity::generate();
        let worker_b = Identity::generate();
        let payer = Identity::generate();
        {
            let mut s = state.write().await;
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: ChainType::EthereumL2(gt_compute::payment::L2Network::Base),
                }));
        }
        for worker in [&worker_a, &worker_b] {
            crate::dispatch::open_job_channel(
                &state,
                crate::dispatch::OpenChannelParams {
                    job_id: "job-1",
                    payer: &payer.peer_id(),
                    payee: &worker.peer_id(),
                    chain: ChainType::EthereumL2(gt_compute::payment::L2Network::Base),
                    amount: 500,
                    payer_address: "0xpayer".into(),
                    payee_address: "0xworker".into(),
                },
            )
            .await;
        }
        {
            let s = state.read().await;
            let channels = s.payment_manager.job_channels("job-1");
            assert_eq!(channels.len(), 2);
            assert!(channels.iter().all(|c| c.payment_details.is_some()));
        }
        crate::dispatch::settle_job_payment_for_worker(
            &state,
            "job-1",
            Some(&worker_a.peer_id()),
            true,
        )
        .await;
        {
            let s = state.read().await;
            let channels = s.payment_manager.job_channels("job-1");
            let status_for = |w: &Identity| {
                channels
                    .iter()
                    .find(|c| c.payee == w.peer_id())
                    .unwrap()
                    .status
                    .clone()
            };
            assert_eq!(
                status_for(&worker_a),
                gt_compute::payment::PaymentStatus::Released
            );
            assert_eq!(
                status_for(&worker_b),
                gt_compute::payment::PaymentStatus::Refunded
            );
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_remote_result_releases_payment_channel() {
        let (state, tmp, _cmd_rx) = test_state("rresult-pay");
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: ChainType::BitcoinLightning,
                }));
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            let channel = gt_compute::payment::PaymentChannel::new(
                job_id.clone(),
                submitter.peer_id(),
                worker.peer_id(),
                ChainType::BitcoinLightning,
                500,
                "payer".into(),
                "payee".into(),
            );
            s.payment_manager.open_channel(channel).unwrap();
            job_id
        };
        let result = gt_compute::result::JobResult::new(
            &worker,
            job_id.clone(),
            0,
            b"build ok\n",
            b"",
            vec![],
            1200,
            gt_compute::result::ResourceUsage {
                peak_cpu_percent: 50.0,
                peak_ram_mb: 512,
                disk_used_mb: 100,
            },
        );
        let mut msg = gt_core::protocol::JobResultMessage {
            job_id: job_id.clone(),
            worker_id: worker.peer_id(),
            result_id: result.id.clone(),
            exit_code: result.exit_code,
            stdout_hash: result.stdout_hash.clone(),
            stderr_hash: result.stderr_hash.clone(),
            stdout_tail: result.stdout_tail.clone(),
            stderr_tail: result.stderr_tail.clone(),
            artifact_hashes: result.artifact_hashes.clone(),
            artifact_blobs: result.artifact_blobs.clone(),
            execution_time_ms: result.execution_time_ms,
            outputs: result.outputs.clone(),
            summary_md: result.summary_md.clone(),
            peak_cpu_percent: result.resource_usage.peak_cpu_percent,
            peak_ram_mb: result.resource_usage.peak_ram_mb,
            disk_used_mb: result.resource_usage.disk_used_mb,
            result_signature: result.signature.clone(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = worker.sign(&payload);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], true);
        {
            let s = state.read().await;
            let channel = s.payment_manager.for_job(&job_id).unwrap();
            assert_eq!(channel.status, gt_compute::payment::PaymentStatus::Released);
            assert_eq!(channel.released_amount, 500);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_remote_result_rejects_wrong_worker() {
        let (state, tmp, _cmd_rx) = test_state("rresult-neg");
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let other = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        let mut msg = gt_core::protocol::JobResultMessage {
            job_id: job_id.clone(),
            worker_id: other.peer_id(),
            result_id: String::new(),
            exit_code: -1,
            stdout_hash: String::new(),
            stderr_hash: String::new(),
            stdout_tail: String::new(),
            stderr_tail: "boom".into(),
            artifact_hashes: vec![],
            artifact_blobs: vec![],
            execution_time_ms: 0,
            outputs: std::collections::HashMap::new(),
            summary_md: String::new(),
            peak_cpu_percent: 0.0,
            peak_ram_mb: 0,
            disk_used_mb: 0,
            result_signature: vec![],
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = other.sign(&payload);
        let res = remote_job_result(State(state.clone()), cluster_headers(), Json(msg))
            .await
            .unwrap();
        assert_eq!(res.0["ok"], false);
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Matched { .. }
            ));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_submit_tick_matches_job_to_worker() {
        let app = axum::Router::new().route(
            "/api/compute/remote/submit",
            axum::routing::post(|| async { axum::Json(serde_json::json!({ "accepted": true })) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, tmp, _cmd_rx) = test_state("stick");
        let worker = Identity::generate();
        let job_id = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                worker.peer_id()
            ));
            let offer = gt_compute::offer::ComputeOffer::new(
                &worker,
                4,
                8192,
                50000,
                false,
                1000,
                vec![gt_compute::payment::ChainType::BitcoinLightning],
                std::collections::HashMap::new(),
                2,
            );
            s.worker_registry.register(offer);
            let submitter = Identity::generate();
            let job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_submit_tick(state.clone()).await;
        {
            let s = state.read().await;
            match &s.job_store.get(&job_id).unwrap().status {
                JobStatus::Matched { worker: w, .. } => assert_eq!(w, &worker.peer_id()),
                other => panic!("expected Matched, got {other:?}"),
            }
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_ship_result_back_posts_and_cleans_up() {
        let got: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let got_route = got.clone();
        let app = axum::Router::new().route(
            "/api/compute/remote/result",
            axum::routing::post(|axum::Json(v): axum::Json<serde_json::Value>| async move {
                *got_route.lock().unwrap() = Some(v);
                axum::Json(serde_json::json!({ "ok": true }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, tmp, _cmd_rx) = test_state("ship");
        let submitter = Identity::generate();
        let worker = state.read().await.identity.clone();
        let repo_dir = tmp.join("jobs").join("repos").join("job-ship-1");
        std::fs::create_dir_all(&repo_dir).unwrap();
        {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                submitter.peer_id()
            ));
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.id = "job-ship-1".into();
            job.status = JobStatus::Running {
                worker: worker.peer_id(),
                started_at: chrono::Utc::now(),
            };
            s.job_store.add(job);
            let result = gt_compute::result::JobResult::new(
                &worker,
                "job-ship-1".into(),
                0,
                b"done\n",
                b"",
                vec![],
                300,
                gt_compute::result::ResourceUsage {
                    peak_cpu_percent: 20.0,
                    peak_ram_mb: 256,
                    disk_used_mb: 10,
                },
            );
            s.result_store.add(result);
            s.foreign_repos.insert(
                "job-ship-1".into(),
                ForeignRepo {
                    submitter: submitter.peer_id(),
                    path: repo_dir.clone(),
                },
            );
        }
        crate::dispatch::ship_result_back(&state, "job-ship-1").await;
        {
            let posted = got.lock().unwrap().clone().unwrap();
            assert_eq!(posted["job_id"], "job-ship-1");
            assert_eq!(posted["exit_code"], 0);
            let msg: gt_core::protocol::JobResultMessage = serde_json::from_value(posted).unwrap();
            assert!(msg.verify());
            let s = state.read().await;
            assert!(!s.foreign_repos.contains_key("job-ship-1"));
            assert!(!repo_dir.exists());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_submit_tick_requeues_stale_match() {
        let (state, tmp, _cmd_rx) = test_state("requeue");
        let job_id = {
            let mut s = state.write().await;
            let submitter = Identity::generate();
            let worker = Identity::generate();
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now() - chrono::Duration::hours(2),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_submit_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Pending
            ));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_tick_downloads_collection_manifest() {
        let owner = Identity::generate();
        let manifest = gt_core::types::CollectionManifest::new(
            &owner,
            "tiny-model".into(),
            "test model".into(),
            vec![gt_core::types::CollectionFile {
                blob_id: gt_core::types::BlobId([0x44; 32]),
                path: "weights.bin".into(),
                size: 100,
                media_type: None,
            }],
            std::collections::HashMap::new(),
        );
        let body = serde_json::json!({
            "id": manifest.id,
            "name": manifest.name,
            "description": manifest.description,
            "owner": manifest.owner.to_string(),
            "files": [{
                "blob_id": gt_core::types::BlobId([0x44; 32]).to_string(),
                "path": "weights.bin",
                "size": 100,
                "media_type": None::<String>,
            }],
            "metadata": {},
            "total_size": 100,
            "verified": true,
            "created_at": manifest.created_at.to_rfc3339(),
            "signature": manifest.signature,
        });
        let app = axum::Router::new().route(
            "/api/collections/{id}",
            axum::routing::get(move || async move { axum::Json(body.clone()) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, tmp, _cmd_rx) = test_state("collfetch");
        {
            let mut s = state.write().await;
            s.wanted_collections.insert(manifest.id.clone());
            s.collection_seeders
                .insert(manifest.id.clone(), vec![owner.peer_id()]);
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                owner.peer_id()
            ));
        }
        crate::fetch::run_fetch_tick(state.clone()).await;
        {
            let s = state.read().await;
            let stored = s.collection_store.get(&manifest.id).unwrap();
            assert!(stored.verify());
            assert_eq!(stored.name, "tiny-model");
            assert!(!s.wanted_collections.contains(&manifest.id));
            assert!(s.wanted_blobs.contains(&gt_core::types::BlobId([0x44; 32])));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_dispatch_tick_runs_job_locally_end_to_end() {
        let (state, tmp, _cmd_rx) = test_state("e2e");
        let repo_dir = tmp.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        std::fs::write(repo_dir.join("input.txt"), "e2e-data\n").unwrap();
        let job_id = {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            let compute = s.config.compute.as_mut().unwrap();
            compute.execution_mode = gt_core::config::ExecutionMode::Local;
            compute.docker_socket = tmp.join("no-docker.sock").to_string_lossy().to_string();
            let submitter = Identity::generate();
            s.config.repos.push(RepoConfig {
                name: "demo".into(),
                path: repo_dir.clone(),
                owner: submitter.peer_id(),
                description: None,
                seeding: true,
            });
            let mut cfg = ci_job_config();
            cfg.steps = vec![gt_compute::job::JobStep {
                name: "build".into(),
                run: "cat input.txt && echo done > result.txt".into(),
                condition: gt_compute::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }];
            cfg.artifacts = vec!["result.txt".into()];
            let job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                cfg,
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_dispatch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Completed { .. }
            ));
            let result = s.result_store.for_job(&job_id).unwrap();
            assert_eq!(result.exit_code, 0);
            assert!(result.verify());
            assert!(result.stdout_tail.contains("e2e-data"));
            assert_eq!(result.artifact_hashes.len(), 1);
        }
        assert!(!repo_dir.join("result.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_dispatch_tick_failing_exit_code_fails_job() {
        let (state, tmp, _cmd_rx) = test_state("e2e-fail");
        let repo_dir = tmp.join("repo");
        std::fs::create_dir_all(&repo_dir).unwrap();
        let job_id = {
            let mut s = state.write().await;
            s.config.enable_worker(Some(4), Some(8192), Some(1000));
            let compute = s.config.compute.as_mut().unwrap();
            compute.execution_mode = gt_core::config::ExecutionMode::Local;
            compute.docker_socket = tmp.join("no-docker.sock").to_string_lossy().to_string();
            let submitter = Identity::generate();
            s.config.repos.push(RepoConfig {
                name: "demo".into(),
                path: repo_dir.clone(),
                owner: submitter.peer_id(),
                description: None,
                seeding: true,
            });
            let mut cfg = ci_job_config();
            cfg.steps = vec![gt_compute::job::JobStep {
                name: "fail".into(),
                run: "exit 3".into(),
                condition: gt_compute::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }];
            let job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                cfg,
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            let job_id = job.id.clone();
            s.job_store.add(job);
            job_id
        };
        crate::dispatch::run_dispatch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Failed { .. }
            ));
            let result = s.result_store.for_job(&job_id).unwrap();
            assert_eq!(result.exit_code, 3);
            assert!(result.verify());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_tick_downloads_full_collection() {
        let tmp = std::env::temp_dir().join(format!("gf_colldl_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let seeder_dir = tmp.join("seeder");
        std::fs::create_dir_all(&seeder_dir).unwrap();
        let weights: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(seeder_dir.join("w.bin"), &weights).unwrap();
        std::fs::write(seeder_dir.join("tok.json"), b"{\"v\":1}").unwrap();
        let seeder_store = std::sync::Arc::new(BlobStore::new(&seeder_dir));
        let meta_w = seeder_store.store_file(&seeder_dir.join("w.bin")).unwrap();
        let meta_t = seeder_store
            .store_file(&seeder_dir.join("tok.json"))
            .unwrap();
        let owner = Identity::generate();
        let manifest = gt_core::types::CollectionManifest::new(
            &owner,
            "dl-model".into(),
            "download test".into(),
            vec![
                gt_core::types::CollectionFile {
                    blob_id: meta_w.blob_id.clone(),
                    path: "w.bin".into(),
                    size: meta_w.total_size,
                    media_type: None,
                },
                gt_core::types::CollectionFile {
                    blob_id: meta_t.blob_id.clone(),
                    path: "tok.json".into(),
                    size: meta_t.total_size,
                    media_type: None,
                },
            ],
            std::collections::HashMap::new(),
        );
        let body = serde_json::json!({
            "id": manifest.id,
            "name": manifest.name,
            "description": manifest.description,
            "owner": manifest.owner.to_string(),
            "files": [
                {
                    "blob_id": meta_w.blob_id.to_string(),
                    "path": "w.bin",
                    "size": meta_w.total_size,
                    "media_type": None::<String>,
                },
                {
                    "blob_id": meta_t.blob_id.to_string(),
                    "path": "tok.json",
                    "size": meta_t.total_size,
                    "media_type": None::<String>,
                },
            ],
            "metadata": {},
            "total_size": manifest.total_size(),
            "verified": true,
            "created_at": manifest.created_at.to_rfc3339(),
            "signature": manifest.signature,
        });
        let meta_store = seeder_store.clone();
        let piece_store = seeder_store.clone();
        let app = axum::Router::new()
            .route(
                "/api/collections/{id}",
                axum::routing::get(move || async move { axum::Json(body.clone()) }),
            )
            .route(
                "/api/blobs/{id}/meta",
                axum::routing::get(
                    move |axum::extract::Path(id): axum::extract::Path<String>| async move {
                        let blob_id: gt_core::types::BlobId = id.parse().unwrap();
                        let m = meta_store.get_meta(&blob_id).unwrap().unwrap();
                        axum::Json(m)
                    },
                ),
            )
            .route(
                "/api/blobs/{id}/pieces/{index}",
                axum::routing::get(
                    move |axum::extract::Path((id, index)): axum::extract::Path<(
                        String,
                        usize,
                    )>| async move {
                        let blob_id: gt_core::types::BlobId = id.parse().unwrap();
                        let m = piece_store.get_meta(&blob_id).unwrap().unwrap();
                        piece_store.read_piece(&blob_id, &m, index).unwrap()
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, ltmp, _cmd_rx) = test_state("colldl");
        {
            let mut s = state.write().await;
            s.wanted_collections.insert(manifest.id.clone());
            s.collection_seeders
                .insert(manifest.id.clone(), vec![owner.peer_id()]);
            s.blob_tracker
                .add_seeder(meta_w.blob_id.clone(), owner.peer_id());
            s.blob_tracker
                .add_seeder(meta_t.blob_id.clone(), owner.peer_id());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                owner.peer_id()
            ));
        }
        for _ in 0..4 {
            crate::fetch::run_fetch_tick(state.clone()).await;
        }
        {
            let s = state.read().await;
            assert!(s.collection_store.get(&manifest.id).is_some());
            assert!(s.wanted_collections.is_empty());
            assert!(s.wanted_blobs.is_empty());
            assert!(s.blob_store.has(&meta_w.blob_id));
            assert!(s.blob_store.has(&meta_t.blob_id));
        }
        let got_w =
            std::fs::read(state.read().await.blob_store.blob_path(&meta_w.blob_id)).unwrap();
        assert_eq!(got_w, weights);
        let got_t =
            std::fs::read(state.read().await.blob_store.blob_path(&meta_t.blob_id)).unwrap();
        assert_eq!(got_t, b"{\"v\":1}");
        let _ = std::fs::remove_dir_all(&tmp);
        let _ = std::fs::remove_dir_all(&ltmp);
    }

    #[tokio::test]
    async fn test_worker_status_reflects_config() {
        let (state, tmp, _cmd_rx) = test_state("status");
        let res = get_worker_status(State(state.clone())).await;
        assert_eq!(res.0["worker_enabled"], false);
        assert_eq!(res.0["has_gpu"], false);
        {
            let mut s = state.write().await;
            s.config.enable_worker(Some(8), None, None);
            if let Some(compute) = s.config.compute.as_mut() {
                compute.gpu_enabled = true;
            }
        }
        let res = get_worker_status(State(state.clone())).await;
        assert_eq!(res.0["worker_enabled"], true);
        assert_eq!(res.0["cpu_cores"], 8);
        assert_eq!(res.0["has_gpu"], true);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn session_cookie_value_sets_lockdown_flags() {
        let value = session_cookie_value("tok-abc");
        assert!(value.starts_with("gitforest_session=tok-abc;"));
        assert!(value.contains("Path=/"));
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("SameSite=Strict"));
    }

    #[test]
    fn request_has_session_parses_cookie_header() {
        let mut headers = axum::http::HeaderMap::new();
        assert!(!request_has_session(&headers, "tok-abc"));
        headers.insert("cookie", "gitforest_session=wrong".parse().unwrap());
        assert!(!request_has_session(&headers, "tok-abc"));
        headers.insert(
            "cookie",
            "other=1; gitforest_session=tok-abc".parse().unwrap(),
        );
        assert!(request_has_session(&headers, "tok-abc"));
        headers.insert("cookie", "xgitforest_session=tok-abc".parse().unwrap());
        assert!(!request_has_session(&headers, "tok-abc"));
        headers.insert("cookie", "gitforest_sessionid=tok-abc".parse().unwrap());
        assert!(!request_has_session(&headers, "tok-abc"));
        headers.append("cookie", "gitforest_session=tok-abc".parse().unwrap());
        assert!(request_has_session(&headers, "tok-abc"));
    }

    #[test]
    fn check_auth_accepts_session_cookie() {
        let mut headers = axum::http::HeaderMap::new();
        assert_eq!(
            check_auth_with_skip(&headers, "tok-abc", None, false),
            Err(StatusCode::UNAUTHORIZED)
        );
        headers.insert("cookie", "gitforest_session=tok-abc".parse().unwrap());
        assert_eq!(
            check_auth_with_skip(&headers, "tok-abc", None, false),
            Ok(())
        );
    }

    fn bearer_headers(token: &str) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        headers
    }

    #[test]
    fn scoped_auth_accepts_api_token_and_adequate_job_token() {
        use gt_compute::permissions::{job_token_expiry, mint_job_token};
        let id = Identity::generate();
        let perms = Permissions::all(PermissionLevel::Read);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let token = mint_job_token(&id, "job-1", &perms, job_token_expiry(now, 600));
        let headers = bearer_headers(&token);
        assert_eq!(
            check_scoped_auth_with_skip(
                &headers,
                "tok-abc",
                None,
                &id,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Ok(())
        );
        assert_eq!(
            check_scoped_auth_with_skip(
                &headers,
                "tok-abc",
                None,
                &id,
                PermissionScope::Contents,
                PermissionLevel::Write,
                false,
            ),
            Err(StatusCode::FORBIDDEN)
        );
        let api_headers = bearer_headers("tok-abc");
        assert_eq!(
            check_scoped_auth_with_skip(
                &api_headers,
                "tok-abc",
                None,
                &id,
                PermissionScope::Workflows,
                PermissionLevel::Write,
                false,
            ),
            Ok(())
        );
        assert_eq!(
            check_scoped_auth_with_skip(
                &axum::http::HeaderMap::new(),
                "tok-abc",
                None,
                &id,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[test]
    fn scoped_auth_rejects_expired_and_foreign_job_tokens() {
        use gt_compute::permissions::{job_token_expiry, mint_job_token};
        let id = Identity::generate();
        let other = Identity::generate();
        let perms = Permissions::all(PermissionLevel::Write);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let stale = mint_job_token(&id, "job-1", &perms, now.saturating_sub(10));
        assert_eq!(
            check_scoped_auth_with_skip(
                &bearer_headers(&stale),
                "tok-abc",
                None,
                &id,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Err(StatusCode::UNAUTHORIZED)
        );
        let foreign = mint_job_token(&other, "job-1", &perms, job_token_expiry(now, 600));
        assert_eq!(
            check_scoped_auth_with_skip(
                &bearer_headers(&foreign),
                "tok-abc",
                None,
                &id,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Err(StatusCode::UNAUTHORIZED)
        );
    }

    #[tokio::test]
    async fn job_show_honors_job_token_scope() {
        use gt_compute::permissions::{job_token_expiry, mint_job_token};
        let (state, tmp, _cmd_rx) = test_state("job-token-show");
        let (identity, api_token) = {
            let s = state.read().await;
            (s.identity.clone(), s.api_token.clone())
        };
        let job = ComputeJob::new(
            &identity,
            identity.peer_id(),
            "repo".to_string(),
            "abc123".to_string(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements {
                min_cpu_cores: 1,
                min_ram_mb: 64,
                min_disk_mb: 64,
                gpu_required: false,
                network_access: false,
            },
            0,
            "credit".to_string(),
        );
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let reader = mint_job_token(
            &identity,
            "reader",
            &Permissions::all(PermissionLevel::Read),
            job_token_expiry(now, 600),
        );
        let detail = get_compute_job(
            State(state.clone()),
            bearer_headers(&reader),
            AxPath(job_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(detail.0["id"], job_id);
        assert_eq!(
            check_scoped_auth_with_skip(
                &bearer_headers(&reader),
                &api_token,
                None,
                &identity,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Ok(())
        );
        let denied = mint_job_token(
            &identity,
            "denied",
            &Permissions::none(),
            job_token_expiry(now, 600),
        );
        assert_eq!(
            check_scoped_auth_with_skip(
                &bearer_headers(&denied),
                &api_token,
                None,
                &identity,
                PermissionScope::Contents,
                PermissionLevel::Read,
                false,
            ),
            Err(StatusCode::FORBIDDEN)
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn serve_ui_sets_cookie_for_loopback_html_only() {
        let (state, tmp, _cmd_rx) = test_state("cookie");
        let token = state.read().await.api_token.clone();
        let local = ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap());
        let lan = ConnectInfo("192.168.1.2:5555".parse::<SocketAddr>().unwrap());

        let resp = serve_ui(State(state.clone()), local, "/".parse::<Uri>().unwrap())
            .await
            .into_response();
        let set_cookie = resp
            .headers()
            .get(header::SET_COOKIE)
            .expect("loopback HTML sets session cookie")
            .to_str()
            .unwrap()
            .to_string();
        assert!(set_cookie.contains(&format!("gitforest_session={token}")));

        let resp = serve_ui(
            State(state.clone()),
            ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap()),
            "/app.js".parse::<Uri>().unwrap(),
        )
        .await
        .into_response();
        assert!(resp.headers().get(header::SET_COOKIE).is_none());

        let resp = serve_ui(State(state.clone()), lan, "/".parse::<Uri>().unwrap())
            .await
            .into_response();
        assert!(resp.headers().get(header::SET_COOKIE).is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn ci_config_serves_repo_yaml() {
        let (state, tmp, _cmd_rx) = test_state("ciconfig");
        let repo_path = tmp.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join(".gitforest-ci.yml"), "name: x\n").unwrap();
        let owner = state.read().await.identity.clone();
        state.write().await.config.repos.push(RepoConfig {
            name: "demo".into(),
            path: repo_path,
            owner: owner.peer_id(),
            description: None,
            seeding: true,
        });
        let headers = axum::http::HeaderMap::new();
        let res = get_ci_config(State(state.clone()), headers, AxPath("demo".into())).await;
        assert_eq!(res.unwrap().0["yaml"], "name: x\n");
        let headers = axum::http::HeaderMap::new();
        let res = get_ci_config(State(state.clone()), headers, AxPath("missing".into())).await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn ci_config_rejects_oversize_file() {
        let (state, tmp, _cmd_rx) = test_state("ciconfigbig");
        let repo_path = tmp.join("repo");
        std::fs::create_dir_all(&repo_path).unwrap();
        std::fs::write(repo_path.join(".gitforest-ci.yml"), "x".repeat(70000)).unwrap();
        let owner = state.read().await.identity.clone();
        state.write().await.config.repos.push(RepoConfig {
            name: "demo".into(),
            path: repo_path,
            owner: owner.peer_id(),
            description: None,
            seeding: true,
        });
        let headers = axum::http::HeaderMap::new();
        let res = get_ci_config(State(state.clone()), headers, AxPath("demo".into())).await;
        assert!(matches!(res, Err(StatusCode::PAYLOAD_TOO_LARGE)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn signed_cancel(submitter: &Identity, job_id: &str) -> gt_core::protocol::JobCancelMessage {
        let mut msg = gt_core::protocol::JobCancelMessage {
            job_id: job_id.into(),
            submitter: submitter.peer_id(),
            reason: "stale match".into(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = submitter.sign(&payload);
        msg
    }

    #[tokio::test]
    async fn cancel_accepts_and_refunds() {
        let (state, tmp, _cmd_rx) = test_state("cancelok");
        let submitter = Identity::generate();
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        let job_id = job.id.clone();
        let our_id = state.read().await.identity.peer_id();
        {
            let mut s = state.write().await;
            s.job_store.add(job);
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: gt_compute::payment::ChainType::BitcoinLightning,
                }));
            let channel = gt_compute::payment::PaymentChannel::new(
                job_id.clone(),
                submitter.peer_id(),
                our_id,
                gt_compute::payment::ChainType::BitcoinLightning,
                500,
                "payer".into(),
                "payee".into(),
            );
            s.payment_manager.insert_funded(
                channel,
                gt_compute::payment::EscrowInfo {
                    escrow_id: "esc-1".into(),
                    payment_details: "inv".into(),
                },
            );
        }
        let headers = axum::http::HeaderMap::new();
        let res = remote_cancel_job(
            State(state.clone()),
            headers,
            Json(signed_cancel(&submitter, &job_id)),
        )
        .await
        .unwrap();
        assert_eq!(res.0["cancelled"], true);
        let s = state.read().await;
        assert!(s.job_store.get(&job_id).is_none());
        let channels = s.payment_manager.job_channels(&job_id);
        assert_eq!(channels.len(), 1);
        assert!(matches!(
            channels[0].status,
            gt_compute::payment::PaymentStatus::Refunded
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cancel_rejects_wrong_submitter() {
        let (state, tmp, _cmd_rx) = test_state("cancelwrong");
        let submitter = Identity::generate();
        let impostor = Identity::generate();
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let headers = axum::http::HeaderMap::new();
        let res = remote_cancel_job(
            State(state.clone()),
            headers,
            Json(signed_cancel(&impostor, &job_id)),
        )
        .await
        .unwrap();
        assert_eq!(res.0["cancelled"], false);
        assert!(state.read().await.job_store.get(&job_id).is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cancel_unknown_job_is_not_found() {
        let (state, tmp, _cmd_rx) = test_state("cancel404");
        let submitter = Identity::generate();
        let headers = axum::http::HeaderMap::new();
        let res = remote_cancel_job(
            State(state.clone()),
            headers,
            Json(signed_cancel(&submitter, "no-such-job")),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cancel_bad_signature_is_rejected() {
        let (state, tmp, _cmd_rx) = test_state("cancel400");
        let submitter = Identity::generate();
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let mut msg = signed_cancel(&submitter, &job_id);
        msg.reason = "tampered".into();
        let headers = axum::http::HeaderMap::new();
        let res = remote_cancel_job(State(state.clone()), headers, Json(msg)).await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        assert!(state.read().await.job_store.get(&job_id).is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn stale_match_sends_signed_cancel() {
        let got: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let got_auth: std::sync::Arc<std::sync::Mutex<Option<String>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let got_route = got.clone();
        let got_auth_route = got_auth.clone();
        let app =
            axum::Router::new().route(
                "/api/compute/remote/cancel",
                axum::routing::post(
                    |headers: axum::http::HeaderMap,
                     axum::Json(v): axum::Json<serde_json::Value>| async move {
                        *got_route.lock().unwrap() = Some(v);
                        *got_auth_route.lock().unwrap() = headers
                            .get("authorization")
                            .and_then(|h| h.to_str().ok())
                            .map(|s| s.to_string());
                        axum::Json(serde_json::json!({ "cancelled": true }))
                    },
                ),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, tmp, _cmd_rx) = test_state("sendcancel");
        let worker = Identity::generate();
        let (job_id, our_id) = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                worker.peer_id()
            ));
            let submitter = s.identity.clone();
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now() - chrono::Duration::hours(2),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            (job_id, submitter.peer_id())
        };
        crate::dispatch::run_submit_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(matches!(
                s.job_store.get(&job_id).unwrap().status,
                JobStatus::Pending
            ));
        }
        let body = got.lock().unwrap().clone().expect("cancel was posted");
        let msg: gt_core::protocol::JobCancelMessage =
            serde_json::from_value(body).expect("cancel parses");
        assert_eq!(msg.job_id, job_id);
        assert_eq!(msg.submitter, our_id);
        assert!(msg.verify());
        assert_eq!(got_auth.lock().unwrap().as_deref(), Some("Bearer shared"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn cancel_terminal_job_is_noop() {
        let (state, tmp, _cmd_rx) = test_state("cancelterm");
        let submitter = Identity::generate();
        let mut job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        job.status = JobStatus::Completed {
            result_id: "res-1".into(),
        };
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let headers = axum::http::HeaderMap::new();
        let res = remote_cancel_job(
            State(state.clone()),
            headers,
            Json(signed_cancel(&submitter, &job_id)),
        )
        .await
        .unwrap();
        assert_eq!(res.0["cancelled"], true);
        assert_eq!(res.0["note"], "already terminal");
        assert!(state.read().await.job_store.get(&job_id).is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_cancel_marks_pending_cancelled() {
        let (state, tmp, _cmd_rx) = test_state("owncancel");
        let submitter = state.read().await.identity.clone();
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        let job_id = job.id.clone();
        {
            let mut s = state.write().await;
            s.job_store.add(job);
            s.payment_manager
                .register_backend(Box::new(gt_compute::payment::StubPaymentBackend {
                    chain: gt_compute::payment::ChainType::BitcoinLightning,
                }));
            let channel = gt_compute::payment::PaymentChannel::new(
                job_id.clone(),
                submitter.peer_id(),
                submitter.peer_id(),
                gt_compute::payment::ChainType::BitcoinLightning,
                500,
                "payer".into(),
                "payee".into(),
            );
            s.payment_manager.insert_funded(
                channel,
                gt_compute::payment::EscrowInfo {
                    escrow_id: "esc-1".into(),
                    payment_details: "inv".into(),
                },
            );
        }
        let headers = axum::http::HeaderMap::new();
        let res = cancel_own_job(State(state.clone()), headers, AxPath(job_id.clone()))
            .await
            .unwrap();
        assert_eq!(res.0["cancelled"], true);
        assert_eq!(res.0["remote_notified"], false);
        let s = state.read().await;
        assert!(matches!(
            s.job_store.get(&job_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert!(matches!(
            s.payment_manager.job_channels(&job_id)[0].status,
            gt_compute::payment::PaymentStatus::Refunded
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_cancel_notifies_matched_worker() {
        let got: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>> =
            std::sync::Arc::new(std::sync::Mutex::new(None));
        let got_route = got.clone();
        let app = axum::Router::new().route(
            "/api/compute/remote/cancel",
            axum::routing::post(|axum::Json(v): axum::Json<serde_json::Value>| async move {
                *got_route.lock().unwrap() = Some(v);
                axum::Json(serde_json::json!({ "cancelled": true }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let (state, tmp, _cmd_rx) = test_state("owncancelremote");
        let worker = Identity::generate();
        let (job_id, our_id) = {
            let mut s = state.write().await;
            s.config.cluster_token = Some("shared".into());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/9418/api/{port}/p2p/QmT/gf/{}",
                worker.peer_id()
            ));
            let submitter = s.identity.clone();
            let mut job = ComputeJob::new(
                &submitter,
                submitter.peer_id(),
                "demo".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                500,
                "lightning".into(),
            );
            job.status = JobStatus::Matched {
                worker: worker.peer_id(),
                matched_at: chrono::Utc::now(),
            };
            let job_id = job.id.clone();
            s.job_store.add(job);
            (job_id, submitter.peer_id())
        };
        let headers = axum::http::HeaderMap::new();
        let res = cancel_own_job(State(state.clone()), headers, AxPath(job_id.clone()))
            .await
            .unwrap();
        assert_eq!(res.0["cancelled"], true);
        assert_eq!(res.0["remote_notified"], true);
        assert!(matches!(
            state.read().await.job_store.get(&job_id).unwrap().status,
            JobStatus::Cancelled
        ));
        let body = got.lock().unwrap().clone().expect("cancel was posted");
        let msg: gt_core::protocol::JobCancelMessage =
            serde_json::from_value(body).expect("cancel parses");
        assert_eq!(msg.job_id, job_id);
        assert_eq!(msg.submitter, our_id);
        assert!(msg.verify());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_cancel_terminal_is_noop() {
        let (state, tmp, _cmd_rx) = test_state("owncancelterm");
        let submitter = state.read().await.identity.clone();
        let mut job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        job.status = JobStatus::Completed {
            result_id: "res-1".into(),
        };
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let headers = axum::http::HeaderMap::new();
        let res = cancel_own_job(State(state.clone()), headers, AxPath(job_id.clone()))
            .await
            .unwrap();
        assert_eq!(res.0["cancelled"], true);
        assert_eq!(res.0["note"], "already terminal");
        assert!(matches!(
            state.read().await.job_store.get(&job_id).unwrap().status,
            JobStatus::Completed { .. }
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn local_cancel_foreign_is_forbidden() {
        let (state, tmp, _cmd_rx) = test_state("owncancelforeign");
        let other = Identity::generate();
        let job = ComputeJob::new(
            &other,
            other.peer_id(),
            "demo".into(),
            "abc123".into(),
            ci_job_config(),
            gt_compute::job::ComputeRequirements::default(),
            500,
            "lightning".into(),
        );
        let job_id = job.id.clone();
        state.write().await.job_store.add(job);
        let headers = axum::http::HeaderMap::new();
        let res = cancel_own_job(State(state.clone()), headers, AxPath(job_id.clone())).await;
        assert!(matches!(res, Err(StatusCode::FORBIDDEN)));
        assert!(matches!(
            state.read().await.job_store.get(&job_id).unwrap().status,
            JobStatus::Pending
        ));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    const WORKFLOW_YAML: &str = r#"
name: CI
on:
  push:
    branches: [main]
  pull_request:
  workflow_dispatch:
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo test
"#;

    async fn register_repo_with_workflow(
        state: &Arc<RwLock<AppState>>,
        tmp: &std::path::Path,
        repo_name: &str,
    ) -> std::path::PathBuf {
        let repo_path = tmp.join(repo_name);
        let wf_dir = repo_path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(wf_dir.join("ci.yml"), WORKFLOW_YAML).unwrap();
        let owner = state.read().await.identity.peer_id();
        state.write().await.config.repos.push(RepoConfig {
            name: repo_name.to_string(),
            path: repo_path.clone(),
            owner,
            description: None,
            seeding: true,
        });
        repo_path
    }

    #[tokio::test]
    async fn test_workflow_list_dispatch_runs_roundtrip() {
        let (state, tmp, _rx) = test_state("wf-roundtrip");
        register_repo_with_workflow(&state, &tmp, "demo").await;
        let headers = axum::http::HeaderMap::new();
        let listed = list_workflows(State(state.clone()), headers, AxPath("demo".to_string()))
            .await
            .unwrap()
            .0;
        assert_eq!(listed["workflows"].as_array().unwrap().len(), 1);
        assert_eq!(listed["workflows"][0]["name"], "CI");
        assert!(listed["errors"].as_array().unwrap().is_empty());

        let dispatch = dispatch_workflow(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(DispatchWorkflowRequest {
                workflow: None,
                branch: Some("main".to_string()),
                tag: None,
                inputs: HashMap::new(),
                commit_sha: Some("abc123".to_string()),
                max_payment_sats: 0,
                preferred_chain: "credit".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(dispatch["runs"].as_array().unwrap().len(), 1);
        let run_id = dispatch["runs"][0]["id"].as_str().unwrap().to_string();
        assert_eq!(dispatch["runs"][0]["job_ids"].as_array().unwrap().len(), 2);

        let runs = list_workflow_runs(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            axum::extract::Query(WorkflowRunsQuery::default()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["summary"]["total"], 2);
        assert_eq!(runs[0]["summary"]["pending"], 1);
        assert_eq!(runs[0]["summary"]["blocked"], 1);

        let detail = get_workflow_run(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(run_id),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(detail["jobs"].as_array().unwrap().len(), 2);
        assert_eq!(detail["jobs"][0]["status"], "pending");
        assert_eq!(detail["jobs"][1]["status"], "blocked");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_dispatch_unknown_repo_and_workflow() {
        let (state, tmp, _rx) = test_state("wf-missing");
        let res = dispatch_workflow(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("ghost".to_string()),
            Json(DispatchWorkflowRequest {
                workflow: None,
                branch: None,
                tag: None,
                inputs: HashMap::new(),
                commit_sha: Some("abc".to_string()),
                max_payment_sats: 0,
                preferred_chain: "credit".to_string(),
            }),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        register_repo_with_workflow(&state, &tmp, "demo").await;
        let dispatch = dispatch_workflow(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(DispatchWorkflowRequest {
                workflow: Some("nope".to_string()),
                branch: None,
                tag: None,
                inputs: HashMap::new(),
                commit_sha: Some("abc".to_string()),
                max_payment_sats: 0,
                preferred_chain: "credit".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert!(dispatch["runs"].as_array().unwrap().is_empty());
        let res = get_workflow_run(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("ghost".to_string()),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    fn model_manifest(owner: &Identity, name: &str) -> gt_core::types::CollectionManifest {
        let mut meta = std::collections::HashMap::new();
        meta.insert("architecture".to_string(), "llama".to_string());
        meta.insert("parameter_count".to_string(), "7B".to_string());
        meta.insert("format".to_string(), "gguf".to_string());
        meta.insert("quantization".to_string(), "q4_0".to_string());
        gt_core::types::CollectionManifest::new(
            owner,
            name.to_string(),
            "test weights".to_string(),
            vec![gt_core::types::CollectionFile {
                blob_id: gt_core::types::BlobId([7u8; 32]),
                path: "weights/model-00001-of-00002.gguf".to_string(),
                size: 100,
                media_type: None,
            }],
            meta,
        )
    }

    #[tokio::test]
    async fn test_model_publish_search_info_roundtrip() {
        let (state, tmp, mut rx) = test_state("model-roundtrip");
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "llama-7b-q4")
        };
        let manifest_id = manifest.id.clone();
        state.write().await.collection_store.add(manifest);
        let published = publish_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(PublishModelRequest {
                collection_id: manifest_id.clone(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(published["status"], "published");
        assert_eq!(published["parameter_count"], 7_000_000_000u64);
        let mut announces = 0;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, DhtCommand::AnnounceCollection { .. }) {
                announces += 1;
            }
        }
        assert_eq!(announces, 2);

        let found = search_models(
            State(state.clone()),
            axum::extract::Query(ModelSearchParams {
                architecture: Some("llama".to_string()),
                ..Default::default()
            }),
        )
        .await
        .0;
        assert_eq!(found.len(), 1);
        assert_eq!(found[0]["name"], "llama-7b-q4");
        assert_eq!(found[0]["local"], true);
        let none = search_models(
            State(state.clone()),
            axum::extract::Query(ModelSearchParams {
                architecture: Some("bert".to_string()),
                ..Default::default()
            }),
        )
        .await
        .0;
        assert!(none.is_empty());

        let detail = get_model(State(state.clone()), AxPath(manifest_id))
            .await
            .unwrap()
            .0;
        assert_eq!(detail["shards"].as_array().unwrap().len(), 1);
        assert_eq!(detail["shards"][0]["base"], "weights/model");
        assert_eq!(detail["model"]["architecture"], "llama");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_publish_rejects_plain_collection() {
        let (state, tmp, _rx) = test_state("model-plain");
        let manifest = {
            let s = state.read().await;
            gt_core::types::CollectionManifest::new(
                &s.identity,
                "plain".to_string(),
                "no meta".to_string(),
                vec![],
                std::collections::HashMap::new(),
            )
        };
        let manifest_id = manifest.id.clone();
        state.write().await.collection_store.add(manifest);
        let res = publish_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(PublishModelRequest {
                collection_id: manifest_id,
            }),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        let res = publish_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            Json(PublishModelRequest {
                collection_id: "ghost".to_string(),
            }),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let res = get_model(State(state.clone()), AxPath("ghost".to_string())).await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_mr_submit_triggers_pr_workflow() {
        let (state, tmp, _rx) = test_state("mr-trigger");
        register_repo_with_workflow(&state, &tmp, "upstream").await;
        let res = submit_merge_request(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("upstream".to_string()),
            Json(SubmitMergeRequest {
                source_repo: "fork".to_string(),
                source_branch: "feature".to_string(),
                target_branch: "main".to_string(),
                title: "t".to_string(),
                description: "d".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "submitted");
        let runs = list_workflow_runs(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            axum::extract::Query(WorkflowRunsQuery {
                repo: Some("upstream".to_string()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0]["event"], "pull_request");
        assert_eq!(runs[0]["branch"], "main");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_cancel_root_fails_blocked_child() {
        let (state, tmp, _rx) = test_state("cancel-unblock");
        register_repo_with_workflow(&state, &tmp, "demo").await;
        let dispatch = dispatch_workflow(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(DispatchWorkflowRequest {
                workflow: None,
                branch: Some("main".to_string()),
                tag: None,
                inputs: HashMap::new(),
                commit_sha: Some("abc123".to_string()),
                max_payment_sats: 0,
                preferred_chain: "credit".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        let root = dispatch["runs"][0]["job_ids"][0]
            .as_str()
            .unwrap()
            .to_string();
        let child = dispatch["runs"][0]["job_ids"][1]
            .as_str()
            .unwrap()
            .to_string();
        let _ = cancel_own_job(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(root.clone()),
        )
        .await
        .unwrap();
        let s = state.read().await;
        assert!(matches!(
            s.job_store.get(&child).unwrap().status,
            JobStatus::Failed { .. }
        ));
        drop(s);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_pull_queues_missing_blobs() {
        let (state, tmp, mut rx) = test_state("model-pull");
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "pull-me")
        };
        let manifest_id = manifest.id.clone();
        let blob_id = manifest.files[0].blob_id.clone();
        state.write().await.collection_store.add(manifest);
        let res = pull_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id.clone()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "fetching");
        assert_eq!(res["missing_blobs"], 1);
        assert!(state.read().await.wanted_blobs.contains(&blob_id));
        let mut lookups = 0;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, DhtCommand::LookupBlob { .. }) {
                lookups += 1;
            }
        }
        assert_eq!(lookups, 1);
        let detail = get_model(State(state.clone()), AxPath(manifest_id))
            .await
            .unwrap()
            .0;
        assert_eq!(detail["blobs_present"], 0);
        assert_eq!(detail["blobs_total"], 1);
        assert_eq!(detail["files"][0]["present"], false);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_pull_unknown_queues_manifest_and_rejects_bad_id() {
        let (state, tmp, mut rx) = test_state("model-pull-unknown");
        let res = pull_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("missing-manifest".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "fetching");
        assert!(state
            .read()
            .await
            .wanted_collections
            .contains("missing-manifest"));
        let mut lookups = 0;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, DhtCommand::LookupCollection { .. }) {
                lookups += 1;
            }
        }
        assert_eq!(lookups, 1);
        let res = pull_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("bad id!".to_string()),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_runs_stub_backend_over_materialized_model() {
        let (state, tmp, _rx) = test_state("model-infer");
        let weights_src = tmp.join("tiny.gguf");
        std::fs::write(&weights_src, b"GGUFfake").unwrap();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap()
        };
        let manifest = {
            let s = state.read().await;
            let mut manifest = model_manifest(&s.identity, "tiny-llama");
            manifest.files[0].blob_id = meta.blob_id.clone();
            manifest.files[0].size = meta.total_size;
            manifest
        };
        let manifest_id = manifest.id.clone();
        let backend = tmp.join("fake-llama");
        std::fs::write(&backend, "#!/bin/sh\necho 'stub completion for test'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&backend).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&backend, perms).unwrap();
        }
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.config.llm_backend = Some(backend.to_str().unwrap().to_string());
        }
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: Some(0.0),
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "complete");
        assert!(res["completion"]
            .as_str()
            .unwrap()
            .contains("stub completion"));
        assert_eq!(res["backend"], backend.to_str().unwrap());
        let by_name = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath("tiny-llama".to_string()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(by_name["status"], "complete");
        let materialized = state
            .read()
            .await
            .config
            .data_dir
            .join("models")
            .join(&manifest_id)
            .join("weights/model-00001-of-00002.gguf");
        assert!(materialized.exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_peer_infer_is_counted_in_serve_stats() {
        let (state, tmp, _rx) = test_state("model-peer-stats");
        let weights_src = tmp.join("tiny.gguf");
        std::fs::write(&weights_src, b"GGUFfake").unwrap();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap()
        };
        let manifest = {
            let s = state.read().await;
            let mut manifest = model_manifest(&s.identity, "tiny-llama");
            manifest.files[0].blob_id = meta.blob_id.clone();
            manifest.files[0].size = meta.total_size;
            manifest
        };
        let manifest_id = manifest.id.clone();
        let backend = tmp.join("fake-llama");
        std::fs::write(&backend, "#!/bin/sh\necho 'stub completion for test'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&backend).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&backend, perms).unwrap();
        }
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.config.llm_backend = Some(backend.to_str().unwrap().to_string());
        }
        let caller = Identity::generate();
        let raw = serde_json::to_vec(&InferRequest {
            prompt: "hello".to_string(),
            max_tokens: Some(16),
            temperature: Some(0.0),
            system: None,
            timeout_secs: Some(30),
        })
        .unwrap();
        let path = format!("/api/models/{manifest_id}/infer");
        let uri: axum::http::Uri = path.parse().unwrap();
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            uri,
            peer_auth_headers(&caller, "POST", &path, &raw),
            AxPath(manifest_id.clone()),
            axum::body::Bytes::from(raw),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "complete");
        let stats = state.read().await.peer_served.clone();
        assert_eq!(stats.requests, 1);
        assert_eq!(stats.by_model.get(&manifest_id), Some(&1));
        let Json(status) = get_status(State(state.clone())).await;
        assert_eq!(status["peer_served"]["requests"], serde_json::json!(1));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_stream_emits_chunks_then_done() {
        let (state, tmp, _rx) = test_state("model-infer-stream");
        let weights_src = tmp.join("tiny.gguf");
        std::fs::write(&weights_src, b"GGUFfake").unwrap();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap()
        };
        let manifest = {
            let s = state.read().await;
            let mut manifest = model_manifest(&s.identity, "tiny-llama-stream");
            manifest.files[0].blob_id = meta.blob_id.clone();
            manifest.files[0].size = meta.total_size;
            manifest
        };
        let manifest_id = manifest.id.clone();
        let backend = tmp.join("fake-llama");
        std::fs::write(
            &backend,
            "#!/bin/sh\nprintf 'stub '\nsleep 1\nprintf 'completion stream'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&backend).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&backend, perms).unwrap();
        }
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.config.llm_backend = Some(backend.to_str().unwrap().to_string());
        }
        let response = infer_model_stream(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: Some(0.0),
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        let mut completion = String::new();
        let mut saw_done = false;
        for line in text.lines() {
            let Some(payload) = line.strip_prefix("data:") else {
                continue;
            };
            let event: serde_json::Value = serde_json::from_str(payload.trim()).unwrap();
            if let Some(chunk) = event.get("chunk").and_then(|v| v.as_str()) {
                completion.push_str(chunk);
            }
            if event.get("done").is_some() {
                saw_done = true;
            }
        }
        assert!(saw_done, "no done event in {text}");
        assert!(
            completion.contains("stub completion stream"),
            "chunks reassemble to {completion:?}"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_missing_blobs_returns_fetching() {
        let (state, tmp, _rx) = test_state("model-infer-missing");
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "ghost-model")
        };
        let manifest_id = manifest.id.clone();
        state.write().await.collection_store.add(manifest);
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "fetching");
        assert_eq!(res["missing_blobs"], 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn stub_infer_peer(
        delay_secs: u64,
    ) -> (
        String,
        std::sync::Arc<std::sync::atomic::AtomicBool>,
        std::sync::Arc<std::sync::Mutex<Option<String>>>,
    ) {
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let auth = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_handler = seen.clone();
        let auth_handler = auth.clone();
        let app = axum::Router::new().route(
            "/api/models/{id}/infer",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let seen_handler = seen_handler.clone();
                let auth_handler = auth_handler.clone();
                async move {
                    seen_handler.store(true, std::sync::atomic::Ordering::SeqCst);
                    *auth_handler.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(|s| s.to_string());
                    if delay_secs > 0 {
                        tokio::time::sleep(std::time::Duration::from_secs(delay_secs)).await;
                    }
                    axum::Json(serde_json::json!({
                        "manifest_id": "remote",
                        "status": "complete",
                        "completion": "remote completion",
                        "backend": "stub",
                        "truncated": false,
                        "elapsed_ms": 1,
                    }))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), seen, auth)
    }

    #[tokio::test]
    async fn test_infer_proxies_to_seeder_when_blobs_missing() {
        let (base, seen, auth) = stub_infer_peer(0).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let (state, tmp, _rx) = test_state("model-infer-proxy");
        let seeder = Identity::generate().peer_id();
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "proxied-model")
        };
        let manifest_id = manifest.id.clone();
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.collection_seeders
                .insert(manifest_id.clone(), vec![seeder.clone()]);
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/1/api/{port}/gf/{seeder}"));
            s.config.cluster_token = Some("cluster-secret".to_string());
        }
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            local_auth_headers(&state).await,
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: None,
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "complete");
        assert_eq!(res["completion"], "remote completion");
        assert_eq!(res["proxied"], true);
        assert_eq!(res["served_by"], seeder.to_string());
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            auth.lock().unwrap().as_deref(),
            Some("Bearer cluster-secret")
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_proxies_without_local_manifest() {
        let (base, seen, _auth) = stub_infer_peer(0).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let (state, tmp, _rx) = test_state("model-infer-remote-only");
        let seeder = Identity::generate().peer_id();
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "remote-only-model")
        };
        let manifest_id = manifest.id.clone();
        let record = crate::models::ModelRecord::from_manifest(&manifest, false).expect("record");
        {
            let mut s = state.write().await;
            s.known_models.insert(manifest_id.clone(), record);
            s.collection_seeders
                .insert(manifest_id.clone(), vec![seeder.clone()]);
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/1/api/{port}/gf/{seeder}"));
            s.config.cluster_token = Some("cluster-secret".to_string());
        }
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            local_auth_headers(&state).await,
            AxPath("remote-only-model".to_string()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "complete");
        assert_eq!(res["proxied"], true);
        assert_eq!(res["served_by"], seeder.to_string());
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_peer_request_never_proxies() {
        let (base, seen, _auth) = stub_infer_peer(0).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let (state, tmp, _rx) = test_state("model-infer-no-reproxy");
        let seeder = Identity::generate().peer_id();
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "no-reproxy-model")
        };
        let manifest_id = manifest.id.clone();
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.collection_seeders
                .insert(manifest_id.clone(), vec![seeder]);
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/1/api/{port}/gf/{}",
                Identity::generate().peer_id()
            ));
            s.config.cluster_token = Some("cluster-secret".to_string());
        }
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer cluster-secret".parse().unwrap());
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            headers,
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "fetching");
        assert!(!seen.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn stub_infer_stream_peer(
        chunk_delay_secs: u64,
    ) -> (String, std::sync::Arc<std::sync::atomic::AtomicBool>) {
        let seen = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let seen_handler = seen.clone();
        let app = axum::Router::new().route(
            "/api/models/{id}/infer/stream",
            axum::routing::post(move |_headers: axum::http::HeaderMap| {
                let seen_handler = seen_handler.clone();
                async move {
                    seen_handler.store(true, std::sync::atomic::Ordering::SeqCst);
                    let (tx, rx) = tokio::sync::mpsc::channel(4);
                    tokio::spawn(async move {
                        tx.send(Ok::<_, std::convert::Infallible>(
                            axum::body::Bytes::from_static(b"data: {\"chunk\": \"remote \"}\n\n"),
                        ))
                        .await
                        .unwrap();
                        if chunk_delay_secs > 0 {
                            tokio::time::sleep(std::time::Duration::from_secs(chunk_delay_secs))
                                .await;
                        }
                        tx.send(Ok(axum::body::Bytes::from_static(
                            b"data: {\"chunk\": \"stream\"}\n\ndata: {\"done\": {}}\n\n",
                        )))
                        .await
                        .unwrap();
                    });
                    (
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        axum::body::Body::from_stream(tokio_stream::wrappers::ReceiverStream::new(
                            rx,
                        )),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://127.0.0.1:{port}"), seen)
    }

    #[tokio::test]
    async fn test_infer_stream_relays_seeder_bytes_live() {
        let (base, seen) = stub_infer_stream_peer(2).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let (state, tmp, _rx) = test_state("model-stream-relay");
        let seeder = Identity::generate().peer_id();
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "relay-model")
        };
        let manifest_id = manifest.id.clone();
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.collection_seeders
                .insert(manifest_id.clone(), vec![seeder.clone()]);
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/1/api/{port}/gf/{seeder}"));
            s.config.cluster_token = Some("cluster-secret".to_string());
        }
        let response = infer_model_stream(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            local_auth_headers(&state).await,
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: None,
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
        let mut body = response.into_body();
        let start = std::time::Instant::now();
        use http_body_util::BodyExt as _;
        let mut first_at: Option<std::time::Duration> = None;
        let mut buf = Vec::new();
        while let Some(frame) = body.frame().await {
            if let Ok(data) = frame.unwrap().into_data() {
                if first_at.is_none() {
                    first_at = Some(start.elapsed());
                }
                buf.extend_from_slice(&data);
            }
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(text.contains("remote "), "{text}");
        assert!(text.contains("stream"), "{text}");
        assert!(text.contains("\"done\""), "{text}");
        let first_latency = first_at.expect("no bytes relayed");
        assert!(
            first_latency < std::time::Duration::from_millis(1500),
            "first bytes relayed late: {first_latency:?}"
        );
        assert!(
            start.elapsed() >= std::time::Duration::from_millis(1900),
            "stream ended before the delayed chunk"
        );
        assert!(seen.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_stream_peer_request_never_relays() {
        let (base, seen) = stub_infer_stream_peer(0).await;
        let port: u16 = base.rsplit(':').next().unwrap().parse().unwrap();
        let (state, tmp, _rx) = test_state("model-stream-no-relay");
        let seeder = Identity::generate().peer_id();
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "no-relay-model")
        };
        let manifest_id = manifest.id.clone();
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.collection_seeders
                .insert(manifest_id.clone(), vec![seeder]);
            let only = s.collection_seeders.get(&manifest_id).unwrap()[0].clone();
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/1/api/{port}/gf/{only}"));
            s.config.cluster_token = Some("cluster-secret".to_string());
        }
        let mut headers = axum::http::HeaderMap::new();
        headers.insert("authorization", "Bearer cluster-secret".parse().unwrap());
        let response = infer_model_stream(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            headers,
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap();
        let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&body);
        assert!(text.contains("fetching"), "{text}");
        assert!(!seen.load(std::sync::atomic::Ordering::SeqCst));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_proxy_infer_timeout_maps_to_gateway_timeout() {
        let (base, _seen, _auth) = stub_infer_peer(5).await;
        let err = proxy_infer(
            &base,
            "m",
            &serde_json::json!({"prompt": "hi"}),
            "cluster-secret",
            &Identity::generate(),
            1,
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::GATEWAY_TIMEOUT);
    }

    #[tokio::test]
    async fn test_select_infer_proxy_skips_self_and_unknown_addrs() {
        let (state, tmp, _rx) = test_state("model-infer-select");
        let own = state.read().await.identity.peer_id();
        let stranger = Identity::generate().peer_id();
        {
            let mut s = state.write().await;
            s.collection_seeders
                .insert("mid".to_string(), vec![own.clone(), stranger.clone()]);
        }
        {
            let s = state.read().await;
            assert!(select_infer_proxy(&s, "mid").is_none());
            assert!(select_infer_proxy(&s, "other").is_none());
        }
        {
            let mut s = state.write().await;
            s.known_peers
                .push(format!("/ip4/127.0.0.1/tcp/1/api/9/gf/{stranger}"));
        }
        let (peer, base) = {
            let s = state.read().await;
            select_infer_proxy(&s, "mid").unwrap()
        };
        assert_eq!(peer, stranger);
        assert_eq!(base, "http://127.0.0.1:9");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_infer_rejects_unknown_model_and_empty_prompt() {
        let (state, tmp, _rx) = test_state("model-infer-bad");
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath("no-such-model".to_string()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let manifest = {
            let s = state.read().await;
            model_manifest(&s.identity, "empty-prompt")
        };
        let manifest_id = manifest.id.clone();
        state.write().await.collection_store.add(manifest);
        let weights_src = tmp.join("w.gguf");
        std::fs::write(&weights_src, vec![0u8; 100]).unwrap();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap()
        };
        {
            let mut s = state.write().await;
            let stored = s.collection_store.get(&manifest_id).cloned().unwrap();
            let mut updated = stored;
            updated.files[0].blob_id = meta.blob_id;
            updated.files[0].size = meta.total_size;
            s.collection_store.add(updated);
        }
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            axum::http::Uri::from_static("/"),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: String::new(),
                    max_tokens: None,
                    temperature: None,
                    system: None,
                    timeout_secs: None,
                })
                .unwrap(),
            ),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_download_job_artifact_serves_blob_bytes() {
        let (state, tmp, _rx) = test_state("artifact-dl");
        let worker = Identity::generate();
        let bytes = b"build output bytes";
        let meta = {
            let s = state.read().await;
            s.blob_store.store_bytes(bytes).unwrap()
        };
        let mut result = gt_compute::result::JobResult::new(
            &worker,
            "job-artifact-1".to_string(),
            0,
            b"out",
            b"",
            vec![("app.bin".to_string(), bytes.to_vec())],
            10,
            gt_compute::result::ResourceUsage {
                peak_cpu_percent: 0.0,
                peak_ram_mb: 0,
                disk_used_mb: 0,
            },
        );
        result.set_artifact_blobs(
            &worker,
            vec![gt_core::types::ArtifactBlob {
                name: "app.bin".to_string(),
                blob_id: meta.blob_id.clone(),
                size: meta.total_size,
            }],
        );
        state.write().await.result_store.add(result);
        let resp = download_job_artifact(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("job-artifact-1".to_string(), "app.bin".to_string())),
        )
        .await
        .unwrap()
        .into_response();
        let body = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), bytes);
        let missing = download_job_artifact(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("job-artifact-1".to_string(), "nope.bin".to_string())),
        )
        .await;
        assert!(matches!(missing, Err(StatusCode::NOT_FOUND)));
        let missing_job = download_job_artifact(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("no-job".to_string(), "app.bin".to_string())),
        )
        .await;
        assert!(matches!(missing_job, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_wait_for_artifact_blob_fetches_and_times_out() {
        let (state, tmp, mut rx) = test_state("artifact-wait");
        let bytes = b"late bytes";
        let blob_id = gt_lfs::blob::BlobMeta::from_data(bytes).blob_id;
        let waiter = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let s = waiter.read().await;
            s.blob_store.store_bytes(bytes).unwrap();
        });
        assert!(wait_for_artifact_blob(&state, &blob_id, 10).await);
        assert!(state.read().await.wanted_blobs.contains(&blob_id));
        let mut saw_lookup = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, DhtCommand::LookupBlob { .. }) {
                saw_lookup = true;
            }
        }
        assert!(saw_lookup);
        let ghost = gt_core::types::BlobId([0xab; 32]);
        assert!(!wait_for_artifact_blob(&state, &ghost, 1).await);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_announce_result_blobs_tracks_and_announces() {
        let (state, tmp, mut rx) = test_state("artifact-announce");
        let meta = {
            let s = state.read().await;
            s.blob_store.store_bytes(b"announced").unwrap()
        };
        crate::dispatch::announce_result_blobs(
            &state,
            &[gt_core::types::ArtifactBlob {
                name: "out.bin".to_string(),
                blob_id: meta.blob_id.clone(),
                size: meta.total_size,
            }],
        )
        .await;
        assert!(state
            .read()
            .await
            .blob_tracker
            .seeders_for(&meta.blob_id)
            .contains(&state.read().await.identity.peer_id()));
        let mut saw_announce = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, DhtCommand::AnnounceBlob { .. }) {
                saw_announce = true;
            }
        }
        assert!(saw_announce);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_repo_secrets_roundtrip() {
        let (state, tmp, _rx) = test_state("repo-secrets");
        register_repo_with_workflow(&state, &tmp, "demo").await;
        let stored = set_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(SetSecretRequest {
                key: "TOKEN".to_string(),
                value: "hunter2".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(stored["status"], "stored");
        let listed = list_secrets(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(listed["names"].as_array().unwrap().len(), 1);
        assert!(!listed.to_string().contains("hunter2"));
        let res = set_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(SetSecretRequest {
                key: "1BAD".to_string(),
                value: "x".to_string(),
            }),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::BAD_REQUEST)));
        let res = set_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("ghost".to_string()),
            Json(SetSecretRequest {
                key: "TOKEN".to_string(),
                value: "x".to_string(),
            }),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let deleted = delete_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), "TOKEN".to_string())),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(deleted["status"], "deleted");
        let res = delete_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(("demo".to_string(), "TOKEN".to_string())),
        )
        .await;
        assert!(matches!(res, Err(StatusCode::NOT_FOUND)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_dispatch_injects_repo_secrets() {
        let (state, tmp, _rx) = test_state("dispatch-secrets");
        let repo_path = tmp.join("demo");
        let wf_dir = repo_path.join(".github").join("workflows");
        std::fs::create_dir_all(&wf_dir).unwrap();
        std::fs::write(
            wf_dir.join("ci.yml"),
            "name: CI\non: push\njobs:\n  deploy:\n    runs-on: ubuntu-latest\n    secrets:\n      TOKEN: yaml-value\n    steps:\n      - run: deploy ${{ secrets.TOKEN }} ${{ secrets.STORE_ONLY }}\n",
        )
        .unwrap();
        let owner = state.read().await.identity.peer_id();
        state.write().await.config.repos.push(RepoConfig {
            name: "demo".to_string(),
            path: repo_path,
            owner,
            description: None,
            seeding: true,
        });
        let _ = set_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(SetSecretRequest {
                key: "TOKEN".to_string(),
                value: "store-value".to_string(),
            }),
        )
        .await
        .unwrap();
        let _ = set_secret(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(SetSecretRequest {
                key: "STORE_ONLY".to_string(),
                value: "from-store".to_string(),
            }),
        )
        .await
        .unwrap();
        let dispatch = dispatch_workflow(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath("demo".to_string()),
            Json(DispatchWorkflowRequest {
                workflow: None,
                branch: Some("main".to_string()),
                tag: None,
                inputs: HashMap::new(),
                commit_sha: Some("abc123".to_string()),
                max_payment_sats: 0,
                preferred_chain: "credit".to_string(),
            }),
        )
        .await
        .unwrap()
        .0;
        let job_id = dispatch["runs"][0]["job_ids"][0].as_str().unwrap();
        let s = state.read().await;
        let job = s.job_store.get(job_id).unwrap();
        assert!(job.job_config.steps[0]
            .run
            .contains("deploy store-value from-store"));
        assert_eq!(job.job_config.secrets.get("TOKEN").unwrap(), "store-value");
        drop(s);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    struct MockSeeder {
        port: u16,
        shutdown: Arc<std::sync::atomic::AtomicBool>,
        handle: Option<std::thread::JoinHandle<()>>,
        max_concurrent: Arc<std::sync::atomic::AtomicUsize>,
    }

    fn serve_mock_conn(
        stream: std::net::TcpStream,
        meta: &gt_lfs::blob::BlobMeta,
        meta_json: &[u8],
        data: &[u8],
        allowed: &Option<Vec<usize>>,
        current: &std::sync::atomic::AtomicUsize,
        max: &std::sync::atomic::AtomicUsize,
    ) {
        use std::io::{Read, Write};
        use std::sync::atomic::Ordering;
        let _ = stream.set_nonblocking(false);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let mut reader = match stream.try_clone() {
            Ok(r) => r,
            Err(_) => return,
        };
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while head.len() < 8192 {
            match reader.read(&mut byte) {
                Ok(1) => {
                    head.push(byte[0]);
                    if head.ends_with(b"\r\n\r\n") {
                        break;
                    }
                }
                _ => break,
            }
        }
        let head = String::from_utf8_lossy(&head);
        let path = head
            .lines()
            .next()
            .unwrap_or("")
            .split_whitespace()
            .nth(1)
            .unwrap_or("")
            .to_string();
        let (status, body) = if path.ends_with("/meta") {
            ("200 OK", meta_json.to_vec())
        } else if let Some(idx) = path
            .rsplit("/pieces/")
            .next()
            .and_then(|s| {
                s.parse::<usize>()
                    .ok()
                    .filter(|_| path.contains("/pieces/"))
            })
            .filter(|idx| allowed.as_ref().is_none_or(|set| set.contains(idx)))
        {
            let now = current.fetch_add(1, Ordering::SeqCst) + 1;
            max.fetch_max(now, Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(100));
            let start = idx * meta.piece_size;
            let end = (start + meta.piece_size).min(data.len());
            let body = if start < data.len() {
                data[start..end].to_vec()
            } else {
                Vec::new()
            };
            current.fetch_sub(1, Ordering::SeqCst);
            ("200 OK", body)
        } else {
            ("404 Not Found", b"nope".to_vec())
        };
        let mut writer = stream;
        let _ = writer.write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        );
        let _ = writer.write_all(&body);
    }

    impl MockSeeder {
        fn start(meta: gt_lfs::blob::BlobMeta, data: Vec<u8>, allowed: Option<Vec<usize>>) -> Self {
            use std::sync::atomic::Ordering;
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            listener.set_nonblocking(true).unwrap();
            let meta_json = Arc::new(serde_json::to_vec(&meta).unwrap());
            let meta = Arc::new(meta);
            let data = Arc::new(data);
            let current = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let max = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let shutdown = Arc::new(std::sync::atomic::AtomicBool::new(false));
            let worker_shutdown = shutdown.clone();
            let worker_max = max.clone();
            let worker_allowed = Arc::new(allowed);
            let handle = std::thread::spawn(move || {
                while !worker_shutdown.load(Ordering::Relaxed) {
                    let (stream, _) = match listener.accept() {
                        Ok(pair) => pair,
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(5));
                            continue;
                        }
                    };
                    let conn_meta = meta.clone();
                    let conn_json = meta_json.clone();
                    let conn_data = data.clone();
                    let conn_current = current.clone();
                    let conn_max = worker_max.clone();
                    let conn_allowed = worker_allowed.clone();
                    std::thread::spawn(move || {
                        serve_mock_conn(
                            stream,
                            &conn_meta,
                            &conn_json,
                            &conn_data,
                            &conn_allowed,
                            &conn_current,
                            &conn_max,
                        );
                    });
                }
            });
            MockSeeder {
                port,
                shutdown,
                handle: Some(handle),
                max_concurrent: max,
            }
        }
    }

    impl Drop for MockSeeder {
        fn drop(&mut self) {
            self.shutdown
                .store(true, std::sync::atomic::Ordering::Relaxed);
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
        }
    }

    #[tokio::test]
    async fn test_fetch_tick_downloads_pieces_in_parallel() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
        let meta = gt_lfs::blob::BlobMeta::from_data(&data);
        assert_eq!(meta.piece_count(), 8);
        let seeder = MockSeeder::start(meta.clone(), data.clone(), None);
        let seeder_peer = Identity::generate().peer_id();
        let (state, tmp, _rx) = test_state("swarm");
        {
            let mut s = state.write().await;
            s.blob_tracker
                .add_seeder(meta.blob_id.clone(), seeder_peer.clone());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/{0}/api/{0}/p2p/mock/gf/{seeder_peer}",
                seeder.port
            ));
            s.wanted_blobs.insert(meta.blob_id.clone());
        }
        crate::fetch::run_fetch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(s.blob_store.has(&meta.blob_id));
            assert!(!s.wanted_blobs.contains(&meta.blob_id));
            let mut assembled = Vec::new();
            for i in 0..meta.piece_count() {
                assembled.extend(s.blob_store.read_piece(&meta.blob_id, &meta, i).unwrap());
            }
            assert_eq!(assembled, data);
        }
        let max = seeder
            .max_concurrent
            .load(std::sync::atomic::Ordering::SeqCst);
        assert!(max >= 2, "pieces fetched serially, max concurrent {max}");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_tick_reassembles_split_seeders() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
        let meta = gt_lfs::blob::BlobMeta::from_data(&data);
        assert_eq!(meta.piece_count(), 8);
        let evens: Vec<usize> = (0..8).filter(|i| i % 2 == 0).collect();
        let odds: Vec<usize> = (0..8).filter(|i| i % 2 == 1).collect();
        let seeder_a = MockSeeder::start(meta.clone(), data.clone(), Some(evens));
        let seeder_b = MockSeeder::start(meta.clone(), data.clone(), Some(odds));
        let peer_a = Identity::generate().peer_id();
        let peer_b = Identity::generate().peer_id();
        let (state, tmp, _rx) = test_state("swarm_split");
        {
            let mut s = state.write().await;
            s.blob_tracker
                .add_seeder(meta.blob_id.clone(), peer_a.clone());
            s.blob_tracker
                .add_seeder(meta.blob_id.clone(), peer_b.clone());
            for (peer, port) in [(&peer_a, seeder_a.port), (&peer_b, seeder_b.port)] {
                s.known_peers.push(format!(
                    "/ip4/127.0.0.1/tcp/{0}/api/{0}/p2p/mock/gf/{peer}",
                    port
                ));
            }
            s.wanted_blobs.insert(meta.blob_id.clone());
        }
        crate::fetch::run_fetch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(s.blob_store.has(&meta.blob_id));
            assert!(!s.wanted_blobs.contains(&meta.blob_id));
            let mut assembled = Vec::new();
            for i in 0..meta.piece_count() {
                assembled.extend(s.blob_store.read_piece(&meta.blob_id, &meta, i).unwrap());
            }
            assert_eq!(assembled, data);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_fetch_tick_uses_partial_leecher_and_announces_progress() {
        let data: Vec<u8> = (0..2 * 1024 * 1024).map(|i| (i * 31 % 251) as u8).collect();
        let meta = gt_lfs::blob::BlobMeta::from_data(&data);
        assert_eq!(meta.piece_count(), 8);
        let seeder = MockSeeder::start(meta.clone(), data.clone(), Some(vec![0, 1]));
        let peer = Identity::generate().peer_id();
        let (state, tmp, mut rx) = test_state("swarm_partial");
        {
            let mut s = state.write().await;
            s.blob_tracker
                .add_leecher(meta.blob_id.clone(), peer.clone());
            s.known_peers.push(format!(
                "/ip4/127.0.0.1/tcp/{0}/api/{0}/p2p/mock/gf/{peer}",
                seeder.port
            ));
            s.wanted_blobs.insert(meta.blob_id.clone());
        }
        crate::fetch::run_fetch_tick(state.clone()).await;
        {
            let s = state.read().await;
            assert!(!s.blob_store.has(&meta.blob_id));
            assert!(s.wanted_blobs.contains(&meta.blob_id));
            let status = s.transfer_manager.status(&meta.blob_id).unwrap();
            assert_eq!(status.pieces_done, 2);
            let piece = s
                .transfer_manager
                .serve_piece(&meta.blob_id, 0)
                .unwrap()
                .unwrap();
            assert!(meta.verify_piece(0, &piece));
            assert!(s
                .transfer_manager
                .serve_piece(&meta.blob_id, 5)
                .unwrap()
                .is_none());
        }
        let mut saw_partial = false;
        while let Ok(cmd) = rx.try_recv() {
            if let DhtCommand::AnnounceBlob { data, .. } = cmd {
                if let Ok(ann) =
                    serde_json::from_slice::<gt_core::protocol::BlobAnnouncement>(&data)
                {
                    if ann.blob_id == meta.blob_id {
                        assert_eq!(ann.have_pieces, Some(2));
                        assert!(!ann.announces_complete());
                        assert!(ann.verify());
                        saw_partial = true;
                    }
                }
            }
        }
        assert!(saw_partial, "no partial progress announcement captured");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    async fn local_auth_headers(state: &Arc<RwLock<AppState>>) -> axum::http::HeaderMap {
        let s = state.read().await;
        let mut headers = axum::http::HeaderMap::new();
        headers.insert(
            "authorization",
            format!("Bearer {}", s.api_token).parse().unwrap(),
        );
        headers
    }

    fn peer_auth_headers(
        id: &Identity,
        method: &str,
        path: &str,
        body: &[u8],
    ) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (name, value) in gt_core::identity::peer_call_headers(id, method, path, body) {
            headers.insert(
                name.parse::<axum::http::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn test_model_caller_classification() {
        let caller = Identity::generate();
        let path = "/api/models/m/infer";
        let peer_headers = peer_auth_headers(&caller, "POST", path, b"{}");
        assert!(matches!(
            authenticate_model_caller(&peer_headers, "local-token", "POST", path, b"{}"),
            ModelCaller::Peer(_)
        ));
        assert!(matches!(
            authenticate_model_caller(
                &peer_headers,
                "local-token",
                "POST",
                "/api/models/other/infer",
                b"{}"
            ),
            ModelCaller::Anonymous
        ));
        assert!(matches!(
            authenticate_model_caller(&peer_headers, "local-token", "POST", path, b"tampered"),
            ModelCaller::Anonymous
        ));
        let mut local_headers = axum::http::HeaderMap::new();
        local_headers.insert("authorization", "Bearer local-token".parse().unwrap());
        assert!(matches!(
            authenticate_model_caller(&local_headers, "local-token", "POST", path, b"{}"),
            ModelCaller::Local
        ));
        assert!(matches!(
            authenticate_model_caller(
                &axum::http::HeaderMap::new(),
                "local-token",
                "POST",
                path,
                b"{}",
            ),
            ModelCaller::Anonymous
        ));
    }

    #[tokio::test]
    async fn test_model_acl_endpoints_roundtrip_and_persist() {
        let (state, tmp, _rx) = test_state("model-acl");
        let friend = Identity::generate().peer_id();
        let local = {
            let s = state.read().await;
            let mut headers = axum::http::HeaderMap::new();
            headers.insert(
                "authorization",
                format!("Bearer {}", s.api_token).parse().unwrap(),
            );
            headers
        };
        let open = get_model_acl(
            State(state.clone()),
            local.clone(),
            AxPath("model-1".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(open["restricted"], false);
        let grant = |action: String, peer: Option<String>| {
            let state = state.clone();
            let local = local.clone();
            async move {
                update_model_acl(
                    State(state),
                    local,
                    AxPath("model-1".to_string()),
                    Json(ModelAclUpdateRequest {
                        action,
                        peer_id: peer,
                    }),
                )
                .await
            }
        };
        let res = grant("grant".to_string(), Some(friend.to_string()))
            .await
            .unwrap()
            .0;
        assert_eq!(res["restricted"], true);
        let got = get_model_acl(
            State(state.clone()),
            local.clone(),
            AxPath("model-1".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(got["restricted"], true);
        assert_eq!(got["grants"][0], friend.to_string());
        let res = grant("revoke".to_string(), Some(friend.to_string()))
            .await
            .unwrap()
            .0;
        assert_eq!(res["restricted"], true);
        let res = grant("open".to_string(), None).await.unwrap().0;
        assert_eq!(res["restricted"], false);
        let res = grant("restrict".to_string(), None).await.unwrap().0;
        assert_eq!(res["restricted"], true);
        {
            let s = state.read().await;
            assert!(s.model_acls.contains_key("model-1"));
            assert!(s.config.model_acl_grants.contains_key("model-1"));
            let reloaded = Config::load(&s.config_path).unwrap();
            assert!(reloaded.model_acl_grants.contains_key("model-1"));
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_restricted_blob_piece_denied_and_granted() {
        let (state, tmp, _rx) = test_state("model-piece-acl");
        let weights_src = tmp.join("w.gguf");
        std::fs::write(&weights_src, b"GGUFfake").unwrap();
        let blob_id = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap().blob_id
        };
        let manifest = {
            let s = state.read().await;
            let mut manifest = model_manifest(&s.identity, "gated-model");
            manifest.files[0].blob_id = blob_id.clone();
            manifest.files[0].size = 8;
            manifest
        };
        let manifest_id = manifest.id.clone();
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            let acl = ModelAcl::new(&s.identity, &manifest_id);
            s.model_acls.insert(manifest_id.clone(), acl);
        }
        let path = format!("/api/blobs/{blob_id}/pieces/0");
        let uri: axum::http::Uri = path.parse().unwrap();
        let denied = serve_piece(
            State(state.clone()),
            axum::http::Method::GET,
            uri.clone(),
            axum::http::HeaderMap::new(),
            AxPath((blob_id.to_string(), 0)),
        )
        .await;
        assert!(matches!(denied, Err(StatusCode::FORBIDDEN)));
        let friend = Identity::generate();
        {
            let mut s = state.write().await;
            let identity = s.identity.clone();
            s.model_acls
                .get_mut(&manifest_id)
                .unwrap()
                .grant(&identity, friend.peer_id())
                .unwrap();
        }
        let headers = peer_auth_headers(&friend, "GET", &path, b"");
        let piece = serve_piece(
            State(state.clone()),
            axum::http::Method::GET,
            uri,
            headers,
            AxPath((blob_id.to_string(), 0)),
        )
        .await
        .unwrap();
        assert_eq!(piece, b"GGUFfake".to_vec());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_restricted_infer_denied_for_stranger() {
        let (state, tmp, _rx) = test_state("model-infer-acl");
        let weights_src = tmp.join("tiny.gguf");
        std::fs::write(&weights_src, b"GGUFfake").unwrap();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_file(&weights_src).unwrap()
        };
        let manifest = {
            let s = state.read().await;
            let mut manifest = model_manifest(&s.identity, "tiny-llama");
            manifest.files[0].blob_id = meta.blob_id.clone();
            manifest.files[0].size = meta.total_size;
            manifest
        };
        let manifest_id = manifest.id.clone();
        let backend = tmp.join("fake-llama");
        std::fs::write(&backend, "#!/bin/sh\necho 'stub completion for test'\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&backend).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&backend, perms).unwrap();
        }
        {
            let mut s = state.write().await;
            s.collection_store.add(manifest);
            s.config.llm_backend = Some(backend.to_str().unwrap().to_string());
            let acl = ModelAcl::new(&s.identity, &manifest_id);
            s.model_acls.insert(manifest_id.clone(), acl);
        }
        let stranger = Identity::generate();
        let path = format!("/api/models/{manifest_id}/infer");
        let raw = axum::body::Bytes::from(
            serde_json::to_vec(&InferRequest {
                prompt: "hello".to_string(),
                max_tokens: Some(16),
                temperature: Some(0.0),
                system: None,
                timeout_secs: Some(30),
            })
            .unwrap(),
        );
        let headers = peer_auth_headers(&stranger, "POST", &path, &raw);
        let uri: axum::http::Uri = path.parse().unwrap();
        let denied = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            uri,
            headers,
            AxPath(manifest_id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: Some(0.0),
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await;
        assert!(matches!(denied, Err(StatusCode::FORBIDDEN)));
        {
            let mut s = state.write().await;
            let identity = s.identity.clone();
            s.model_acls
                .get_mut(&manifest_id)
                .unwrap()
                .grant(&identity, stranger.peer_id())
                .unwrap();
        }
        let path = format!("/api/models/{manifest_id}/infer");
        let raw = axum::body::Bytes::from(
            serde_json::to_vec(&InferRequest {
                prompt: "hello".to_string(),
                max_tokens: Some(16),
                temperature: Some(0.0),
                system: None,
                timeout_secs: Some(30),
            })
            .unwrap(),
        );
        let headers = peer_auth_headers(&stranger, "POST", &path, &raw);
        let uri: axum::http::Uri = path.parse().unwrap();
        let res = infer_model(
            State(state.clone()),
            axum::http::Method::POST,
            uri,
            headers,
            AxPath(manifest_id.clone()),
            axum::body::Bytes::from(
                serde_json::to_vec(&InferRequest {
                    prompt: "hello".to_string(),
                    max_tokens: Some(16),
                    temperature: Some(0.0),
                    system: None,
                    timeout_secs: Some(30),
                })
                .unwrap(),
            ),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(res["status"], "complete");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_cache_save_get_find_roundtrip() {
        let (state, tmp, _rx) = test_state("cache-ep");
        let headers = local_auth_headers(&state).await;
        let file = tmp.join("payload.txt");
        std::fs::write(&file, b"cached-bytes").unwrap();
        let save = |key: String| {
            let state = state.clone();
            let headers = headers.clone();
            let path = file.to_str().unwrap().to_string();
            async move {
                save_cache(
                    State(state),
                    headers,
                    Json(SaveCacheRequest {
                        key,
                        path: Some(path),
                        blob_id: None,
                    }),
                )
                .await
            }
        };
        let first = save("build-cache-1".to_string()).await.unwrap().0;
        assert_eq!(first["status"], "stored");
        let again = save("build-cache-1".to_string()).await.unwrap().0;
        assert_eq!(again["status"], "hit");
        assert_eq!(again["blob_id"], first["blob_id"]);
        let got = get_cache(
            State(state.clone()),
            headers.clone(),
            AxPath("build-cache-1".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(got["blob_id"], first["blob_id"]);
        let found = find_cache(
            State(state.clone()),
            headers.clone(),
            axum::extract::Query(CacheLookupQuery {
                prefix: Some("build-".to_string()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(found["key"], "build-cache-1");
        assert!(get_cache(
            State(state.clone()),
            headers.clone(),
            AxPath("missing".to_string())
        )
        .await
        .is_err());
        {
            let s = state.read().await;
            assert!(s.cache_store.get("build-cache-1").is_some());
        }
        assert!(tmp.join("data").join("caches.json").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_blob_bytes_upload_and_save_by_ref() {
        let (state, tmp, _rx) = test_state("cache-bytes");
        let headers = local_auth_headers(&state).await;
        let uploaded = upload_blob_bytes(
            State(state.clone()),
            headers.clone(),
            axum::body::Bytes::from_static(b"remote-bytes-payload"),
        )
        .await
        .unwrap()
        .0;
        let blob_id = uploaded["blob_id"].as_str().unwrap().to_string();
        assert_eq!(uploaded["size"], 20);
        {
            let s = state.read().await;
            let id: gt_core::types::BlobId = blob_id.parse().unwrap();
            assert!(s.blob_store.has(&id));
        }
        let saved = save_cache(
            State(state.clone()),
            headers.clone(),
            Json(SaveCacheRequest {
                key: "bytes-key-1".to_string(),
                path: None,
                blob_id: Some(blob_id.clone()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(saved["status"], "stored");
        assert_eq!(saved["blob_id"], blob_id);
        let got = get_cache(
            State(state.clone()),
            headers.clone(),
            AxPath("bytes-key-1".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(got["blob_id"], blob_id);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_blob_bytes_upload_and_save_reject_bad_input() {
        let (state, tmp, _rx) = test_state("cache-bytes-bad");
        let headers = local_auth_headers(&state).await;
        let empty = upload_blob_bytes(
            State(state.clone()),
            headers.clone(),
            axum::body::Bytes::new(),
        )
        .await;
        assert!(matches!(empty, Err(StatusCode::BAD_REQUEST)));
        let huge = upload_blob_bytes(
            State(state.clone()),
            headers.clone(),
            axum::body::Bytes::from(vec![0u8; (MAX_CACHE_BYTES + 1) as usize]),
        )
        .await;
        assert!(matches!(huge, Err(StatusCode::PAYLOAD_TOO_LARGE)));
        let unknown = save_cache(
            State(state.clone()),
            headers.clone(),
            Json(SaveCacheRequest {
                key: "ghost-key".to_string(),
                path: None,
                blob_id: Some("0".repeat(64)),
            }),
        )
        .await;
        assert!(matches!(unknown, Err(StatusCode::BAD_REQUEST)));
        let neither = save_cache(
            State(state.clone()),
            headers.clone(),
            Json(SaveCacheRequest {
                key: "empty-key".to_string(),
                path: None,
                blob_id: None,
            }),
        )
        .await;
        assert!(matches!(neither, Err(StatusCode::BAD_REQUEST)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_cache_save_announces_key_on_dht() {
        let (state, tmp, mut rx) = test_state("cache-dht");
        let headers = local_auth_headers(&state).await;
        let file = tmp.join("payload.txt");
        std::fs::write(&file, b"cached-bytes").unwrap();
        let saved = save_cache(
            State(state.clone()),
            headers,
            Json(SaveCacheRequest {
                key: "shared-key-1".to_string(),
                path: Some(file.to_str().unwrap().to_string()),
                blob_id: None,
            }),
        )
        .await
        .unwrap()
        .0;
        let mut cache_data = None;
        while let Ok(cmd) = rx.try_recv() {
            if let DhtCommand::AnnounceCollection { address, data } = cmd {
                assert_eq!(address, gt_lfs::blob::cache_dht_address("shared-key-1"));
                cache_data = Some(data);
            }
        }
        let data = cache_data.expect("save must announce the cache key");
        let ann: gt_core::protocol::CacheAnnouncement = serde_json::from_slice(&data).unwrap();
        assert!(ann.verify());
        assert_eq!(ann.key, "shared-key-1");
        assert_eq!(ann.blob_id.to_string(), saved["blob_id"].as_str().unwrap());
        assert_eq!(ann.size, saved["size"].as_u64().unwrap());
        let dir_headers = local_auth_headers(&state).await;
        let dir_save = save_cache(
            State(state.clone()),
            dir_headers,
            Json(SaveCacheRequest {
                key: "dir-key".to_string(),
                path: Some(tmp.to_str().unwrap().to_string()),
                blob_id: None,
            }),
        )
        .await;
        assert!(matches!(dir_save, Err(StatusCode::BAD_REQUEST)));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_peer_exchange_shares_cache_directory() {
        let (a, tmp_a, _rx) = test_state("cache-gossip-a");
        let (b, tmp_b, _rx) = test_state("cache-gossip-b");
        let headers = local_auth_headers(&a).await;
        let file = tmp_a.join("payload.txt");
        std::fs::write(&file, b"shared-bytes").unwrap();
        let _ = save_cache(
            State(a.clone()),
            headers,
            Json(SaveCacheRequest {
                key: "gossip-key-1".to_string(),
                path: Some(file.to_str().unwrap().to_string()),
                blob_id: None,
            }),
        )
        .await
        .unwrap();
        let loopback = ConnectInfo("127.0.0.1:5555".parse::<SocketAddr>().unwrap());
        let exchange = peer_exchange(State(a.clone()), loopback).await.0;
        assert_eq!(exchange.caches.len(), 1);
        assert_eq!(exchange.caches[0].key, "gossip-key-1");
        assert!(exchange.caches[0].verify());
        {
            let mut s = b.write().await;
            let now = std::time::Instant::now();
            for ann in &exchange.caches {
                assert!(s.remote_caches.merge_announcement(ann, now));
            }
        }
        {
            let s = b.read().await;
            let entry = s.remote_caches.get_exact("gossip-key-1").unwrap();
            assert_eq!(entry.blob_id, exchange.caches[0].blob_id);
            assert_eq!(entry.size, exchange.caches[0].size);
            assert!(s.cache_store.get("gossip-key-1").is_none());
        }
        let _ = std::fs::remove_dir_all(&tmp_a);
        let _ = std::fs::remove_dir_all(&tmp_b);
    }

    #[tokio::test]
    async fn test_get_cache_miss_triggers_dht_lookup() {
        let (state, tmp, mut rx) = test_state("cache-miss");
        let headers = local_auth_headers(&state).await;
        let err = get_cache(
            State(state.clone()),
            headers,
            AxPath("no-such-key".to_string()),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
        let mut saw_lookup = false;
        while let Ok(cmd) = rx.try_recv() {
            if let DhtCommand::LookupCollection { address } = cmd {
                saw_lookup = address == gt_lfs::blob::cache_dht_address("no-such-key");
            }
        }
        assert!(saw_lookup);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_find_cache_local_hit_reports_local_source() {
        let (state, tmp, _rx) = test_state("cache-local-src");
        let headers = local_auth_headers(&state).await;
        let file = tmp.join("payload.txt");
        std::fs::write(&file, b"local-bytes").unwrap();
        let _ = save_cache(
            State(state.clone()),
            headers.clone(),
            Json(SaveCacheRequest {
                key: "local-prefix-1".to_string(),
                path: Some(file.to_str().unwrap().to_string()),
                blob_id: None,
            }),
        )
        .await
        .unwrap();
        let found = find_cache(
            State(state.clone()),
            headers.clone(),
            axum::extract::Query(CacheLookupQuery {
                prefix: Some("local-".to_string()),
            }),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(found["key"], "local-prefix-1");
        assert_eq!(found["source"], "local");
        let got = get_cache(
            State(state.clone()),
            headers,
            AxPath("local-prefix-1".to_string()),
        )
        .await
        .unwrap()
        .0;
        assert_eq!(got["source"], "local");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_find_cache_unreachable_remote_is_not_found() {
        let (state, tmp, _rx) = test_state("cache-unreach");
        let headers = local_auth_headers(&state).await;
        let seeder = Identity::generate();
        let mut ann = gt_core::protocol::CacheAnnouncement {
            owner: seeder.peer_id(),
            key: "remote-only-1".to_string(),
            blob_id: "aa".repeat(32).parse().unwrap(),
            size: 8,
            updated_at: chrono::Utc::now().timestamp(),
            signature: Vec::new(),
        };
        ann.signature = seeder.sign(&ann.signable_payload());
        {
            let mut s = state.write().await;
            assert!(s
                .remote_caches
                .merge_announcement(&ann, std::time::Instant::now()));
        }
        let err = find_cache(
            State(state.clone()),
            headers,
            axum::extract::Query(CacheLookupQuery {
                prefix: Some("remote-".to_string()),
            }),
        )
        .await
        .unwrap_err();
        assert_eq!(err, StatusCode::NOT_FOUND);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_remote_cache_wins_prefers_longest_then_newest() {
        let local =
            |key: &str, created_at: chrono::DateTime<chrono::Utc>| crate::caches::CacheEntry {
                key: key.to_string(),
                blob_id: gt_core::types::BlobId([1u8; 32]),
                size: 10,
                created_at,
            };
        let remote = |key: &str, updated_at: chrono::DateTime<chrono::Utc>| {
            crate::caches::RemoteCacheEntry {
                key: key.to_string(),
                blob_id: gt_core::types::BlobId([2u8; 32]),
                size: 10,
                updated_at,
                seeder: Identity::generate().peer_id(),
                seen_at: std::time::Instant::now(),
            }
        };
        let old = chrono::DateTime::from_timestamp(100, 0).unwrap();
        let new = chrono::DateTime::from_timestamp(200, 0).unwrap();
        assert!(remote_cache_wins(&local("ab", new), &remote("abc", old)));
        assert!(remote_cache_wins(&local("ab", old), &remote("ab", new)));
        assert!(!remote_cache_wins(&local("ab", new), &remote("ab", old)));
        assert!(!remote_cache_wins(&local("abc", old), &remote("ab", new)));
    }

    #[test]
    fn analytics_window_parses_hours_and_days() {
        assert_eq!(parse_history_window_ms("1h"), Some(3_600_000));
        assert_eq!(parse_history_window_ms("24h"), Some(86_400_000));
        assert_eq!(parse_history_window_ms("7d"), Some(604_800_000));
        assert_eq!(parse_history_window_ms(""), None);
        assert_eq!(parse_history_window_ms("0h"), None);
        assert_eq!(parse_history_window_ms("31d"), None);
        assert_eq!(parse_history_window_ms("10m"), None);
        assert_eq!(parse_history_window_ms("nope"), None);
    }

    #[tokio::test]
    async fn analytics_summary_reports_store_counts() {
        let (state, tmp, _cmd_rx) = test_state("analytics-summary");
        {
            let s = state.read().await;
            s.blob_store.store_bytes(b"hello").unwrap();
        }
        {
            let mut s = state.write().await;
            s.known_peers.push("/ip4/127.0.0.1/tcp/1".into());
            let id = Identity::generate();
            let job = ComputeJob::new(
                &id,
                id.peer_id(),
                "repo-a".into(),
                "abc".into(),
                ci_job_config(),
                gt_compute::job::ComputeRequirements::default(),
                100,
                "lightning".into(),
            );
            s.job_store.add(job);
        }
        let Json(got) = get_analytics_summary(State(state.clone())).await;
        assert_eq!(got["blob_count"], serde_json::json!(1));
        assert_eq!(got["blob_bytes"], serde_json::json!(5));
        assert_eq!(got["job_count"], serde_json::json!(1));
        assert_eq!(got["jobs_by_status"]["pending"], serde_json::json!(1));
        assert_eq!(got["peers_now"], serde_json::json!(1));
        assert!(got.get("repos_seeding").is_some());
        assert!(got.get("peer_served").is_some());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn git_daemon_env_points_at_custom_port() {
        let env = git_daemon_env(std::path::Path::new("/tmp/c.toml"), 17894);
        assert!(env.contains(&("GITFOREST_CONFIG".to_string(), "/tmp/c.toml".to_string())));
        assert!(env.contains(&(
            "GITFOREST_DAEMON_URL".to_string(),
            "http://127.0.0.1:17894".to_string()
        )));
    }

    #[tokio::test]
    async fn pull_complete_announces_model_seeder() {
        let (state, tmp, mut cmd_rx) = test_state("pull-seeder");
        let owner = Identity::generate();
        let meta = {
            let s = state.read().await;
            s.blob_store.store_bytes(b"weights").unwrap()
        };
        let mut metadata = HashMap::new();
        metadata.insert("architecture".to_string(), "llama".to_string());
        metadata.insert("parameter_count".to_string(), "1M".to_string());
        metadata.insert("format".to_string(), "gguf".to_string());
        let manifest = gt_core::types::CollectionManifest::new(
            &owner,
            "tiny-llama".into(),
            "test model".into(),
            vec![gt_core::types::CollectionFile {
                blob_id: meta.blob_id.clone(),
                path: "weights.bin".into(),
                size: 7,
                media_type: None,
            }],
            metadata,
        );
        let manifest_id = manifest.id.clone();
        state.write().await.collection_store.add(manifest);
        let Json(res) = pull_model(
            State(state.clone()),
            axum::http::HeaderMap::new(),
            AxPath(manifest_id.clone()),
        )
        .await
        .unwrap();
        assert_eq!(res["status"], serde_json::json!("complete"));
        let s = state.read().await;
        let own = s.identity.peer_id();
        let record = s.known_models.get(&manifest_id).unwrap();
        assert!(record.local);
        assert!(record.seeders.contains(&own));
        assert_eq!(record.owner, owner.peer_id());
        assert!(s.collection_seeders[&manifest_id].contains(&own));
        drop(s);
        let mut announces = Vec::new();
        for _ in 0..2 {
            match tokio::time::timeout(std::time::Duration::from_secs(5), cmd_rx.recv()).await {
                Ok(Some(DhtCommand::AnnounceCollection { data, .. })) => {
                    announces.push(data);
                }
                other => panic!("expected collection announce, got {other:?}"),
            }
        }
        assert_eq!(announces.len(), 2);
        for data in announces {
            let ann: gt_core::protocol::ModelAnnouncement = serde_json::from_slice(&data).unwrap();
            assert_eq!(ann.owner, owner.peer_id());
            assert_eq!(ann.seeder, Some(own.clone()));
            assert!(ann.verify());
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn analytics_history_filters_by_window() {
        let (state, tmp, _cmd_rx) = test_state("analytics-history");
        {
            let mut s = state.write().await;
            let now = now_millis();
            s.analytics_history.push_back(AnalyticsSample {
                t: now.saturating_sub(2 * 3_600_000),
                peers: 3,
            });
            s.analytics_history.push_back(AnalyticsSample {
                t: now.saturating_sub(30_000),
                peers: 7,
            });
        }
        let history = |window: &str| {
            get_analytics_history(
                State(state.clone()),
                axum::extract::Query(AnalyticsHistoryQuery {
                    window: window.to_string(),
                }),
            )
        };
        let Json(hour) = history("1h").await.unwrap();
        assert_eq!(hour.len(), 1);
        assert_eq!(hour[0].peers, 7);
        let Json(day) = history("24h").await.unwrap();
        assert_eq!(day.len(), 2);
        let bad = history("10m").await;
        assert!(matches!(bad, Err((StatusCode::UNPROCESSABLE_ENTITY, _))));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_workflow_run_chain_fires_once() {
        let (state, tmp, _rx) = test_state("chain");
        let dir = tmp.join("repo").join(".github").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("first.yml"),
            "name: First\non: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo a\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("second.yml"),
            "name: Second\non:\n  workflow_run:\n    workflows: [First]\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo b\n",
        )
        .unwrap();
        let repo_root = tmp.join("repo");
        let first_job;
        {
            let mut s = state.write().await;
            let identity = s.identity.clone();
            s.config.repos.push(RepoConfig {
                name: "demo".into(),
                path: repo_root.clone(),
                owner: identity.peer_id(),
                description: None,
                seeding: true,
            });
            let s_ref = &mut *s;
            let created = crate::workflows::trigger_workflows(
                &mut s_ref.job_store,
                &mut s_ref.workflow_runs,
                &identity,
                &identity.peer_id(),
                "demo",
                &repo_root,
                &gt_compute::workflow::WorkflowEvent::Push {
                    branch: "main".into(),
                },
                "abc123",
                None,
                0,
                "credit",
                false,
                &HashMap::new(),
                &[],
                &std::collections::HashMap::new(),
            );
            assert_eq!(created.len(), 1);
            first_job = created[0].job_ids[0].clone();
        }
        {
            let mut s = state.write().await;
            fire_workflow_run_triggers(&mut s, &first_job);
            assert_eq!(s.workflow_runs.list().len(), 1);
        }
        {
            let mut s = state.write().await;
            s.job_store.get_mut(&first_job).unwrap().status = JobStatus::Completed {
                result_id: "r".into(),
            };
            fire_workflow_run_triggers(&mut s, &first_job);
            let runs = s.workflow_runs.list();
            assert_eq!(runs.len(), 2);
            let chained = runs.iter().find(|r| r.event == "workflow_run").unwrap();
            assert_eq!(chained.workflow_name, "Second");
            assert_eq!(chained.job_ids.len(), 1);
            fire_workflow_run_triggers(&mut s, &first_job);
            assert_eq!(s.workflow_runs.list().len(), 2);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_schedule_fires_once_per_slot() {
        let (state, tmp, _rx) = test_state("sched");
        let dir = tmp.join("repo").join(".github").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("nightly.yml"),
            "name: Nightly\non:\n  schedule:\n    - cron: '* * * * *'\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo night\n",
        )
        .unwrap();
        {
            let mut s = state.write().await;
            let identity = s.identity.clone();
            s.config.repos.push(RepoConfig {
                name: "demo".into(),
                path: tmp.join("repo"),
                owner: identity.peer_id(),
                description: None,
                seeding: true,
            });
        }
        let now = chrono::Utc::now();
        {
            let mut s = state.write().await;
            fire_due_schedules(&mut s, &now);
            fire_due_schedules(&mut s, &now);
            let runs = s.workflow_runs.list();
            assert_eq!(runs.len(), 1);
            assert_eq!(runs[0].event, "schedule");
            assert_eq!(runs[0].workflow_name, "Nightly");
        }
        {
            let mut s = state.write().await;
            let later = now + chrono::Duration::seconds(61);
            fire_due_schedules(&mut s, &later);
            assert_eq!(s.workflow_runs.list().len(), 2);
        }
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
