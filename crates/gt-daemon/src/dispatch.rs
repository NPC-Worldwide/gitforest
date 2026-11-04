use gt_compute::dispatch::LocalCaps;
use gt_core::sanitize::sanitize_log;
use std::sync::Arc;
use tokio::sync::RwLock;

pub const UNPAID_JOB_TIMEOUT_SECS: i64 = 86400;

pub async fn settle_job_payment(
    state: &Arc<RwLock<crate::api::AppState>>,
    job_id: &str,
    success: bool,
) {
    settle_job_payment_for_worker(state, job_id, None, success).await;
}

pub struct BlobArtifactSink {
    store: std::sync::Arc<gt_lfs::store::BlobStore>,
}

impl BlobArtifactSink {
    pub fn new(store: std::sync::Arc<gt_lfs::store::BlobStore>) -> Self {
        BlobArtifactSink { store }
    }
}

impl gt_compute::runner::ArtifactSink for BlobArtifactSink {
    fn store_artifact(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> anyhow::Result<gt_core::types::ArtifactBlob> {
        let meta = self.store.store_bytes(bytes)?;
        Ok(gt_core::types::ArtifactBlob {
            name: name.to_string(),
            blob_id: meta.blob_id,
            size: meta.total_size,
        })
    }
}

pub async fn announce_result_blobs(
    state: &Arc<RwLock<crate::api::AppState>>,
    blobs: &[gt_core::types::ArtifactBlob],
) {
    if blobs.is_empty() {
        return;
    }
    let mut s = state.write().await;
    for blob in blobs {
        let peer_id = s.identity.peer_id();
        s.blob_tracker.add_seeder(blob.blob_id.clone(), peer_id);
        let piece_count = s
            .blob_store
            .get_meta(&blob.blob_id)
            .ok()
            .flatten()
            .map(|m| m.piece_count() as u32)
            .unwrap_or(1);
        let mut announcement = gt_core::protocol::BlobAnnouncement {
            peer_id: s.identity.peer_id(),
            blob_id: blob.blob_id.clone(),
            size: blob.size,
            piece_count,
            have_pieces: None,
            signature: Vec::new(),
        };
        announcement.signature = s.identity.sign(&announcement.signable_payload());
        let addr = gt_lfs::blob::blob_dht_address(&blob.blob_id);
        let data = serde_json::to_vec(&announcement).unwrap_or_default();
        let _ = s.dht.announce_blob(addr, data).await;
    }
}

pub async fn settle_cancelled_siblings(
    state: &Arc<RwLock<crate::api::AppState>>,
    cancelled: &[(String, Option<gt_core::identity::PeerId>)],
) {
    for (id, worker) in cancelled {
        if let Some(worker) = worker {
            send_cancel_to_worker(state, id, worker, "matrix fail-fast").await;
        }
        settle_job_payment(state, id, false).await;
    }
}

pub async fn settle_job_payment_for_worker(
    state: &Arc<RwLock<crate::api::AppState>>,
    job_id: &str,
    worker: Option<&gt_core::identity::PeerId>,
    success: bool,
) {
    let channels = { state.read().await.payment_manager.job_channels(job_id) };
    for channel in channels {
        let release = success && worker.map(|w| &channel.payee == w).unwrap_or(true);
        settle_one_channel(state, job_id, channel, release).await;
    }
}

pub async fn refund_channel_for_payee(
    state: &Arc<RwLock<crate::api::AppState>>,
    job_id: &str,
    payee: &gt_core::identity::PeerId,
) {
    let channels = { state.read().await.payment_manager.job_channels(job_id) };
    for channel in channels {
        if &channel.payee == payee {
            settle_one_channel(state, job_id, channel, false).await;
        }
    }
}

async fn settle_one_channel(
    state: &Arc<RwLock<crate::api::AppState>>,
    job_id: &str,
    channel: gt_compute::payment::PaymentChannel,
    release: bool,
) {
    if matches!(
        channel.status,
        gt_compute::payment::PaymentStatus::Released | gt_compute::payment::PaymentStatus::Refunded
    ) {
        return;
    }
    let Some(escrow) = channel.escrow_ref.clone() else {
        return;
    };
    let backend = {
        state
            .read()
            .await
            .payment_manager
            .backend_for(&channel.chain)
    };
    let Some(backend) = backend else {
        tracing::warn!("cannot settle job {job_id}: {} backend gone", channel.chain);
        return;
    };
    let channel_id = channel.id.clone();
    let chain_name = channel.chain.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        if release {
            backend.release_escrow(&escrow)
        } else {
            backend.refund_escrow(&escrow)
        }
    })
    .await;
    match outcome {
        Ok(Ok(())) => {
            state
                .write()
                .await
                .payment_manager
                .mark_settled(&channel_id, release);
        }
        Ok(Err(e)) => tracing::warn!(
            "settlement for job {job_id} on {chain_name} failed: {}",
            sanitize_log(&e.to_string())
        ),
        Err(e) => tracing::warn!("settlement task for job {job_id} panicked: {e}"),
    }
}

pub struct OpenChannelParams<'a> {
    pub job_id: &'a str,
    pub payer: &'a gt_core::identity::PeerId,
    pub payee: &'a gt_core::identity::PeerId,
    pub chain: gt_compute::payment::ChainType,
    pub amount: u64,
    pub payer_address: String,
    pub payee_address: String,
}

pub async fn open_job_channel(
    state: &Arc<RwLock<crate::api::AppState>>,
    params: OpenChannelParams<'_>,
) {
    let OpenChannelParams {
        job_id,
        payer,
        payee,
        chain,
        amount,
        payer_address,
        payee_address,
    } = params;
    let backend = { state.read().await.payment_manager.backend_for(&chain) };
    let Some(backend) = backend else {
        return;
    };
    let (rpc_payer, rpc_payee) = (payer_address.clone(), payee_address.clone());
    let outcome =
        tokio::task::spawn_blocking(move || backend.create_escrow(amount, &rpc_payer, &rpc_payee))
            .await;
    match outcome {
        Ok(Ok(info)) => {
            let channel = gt_compute::payment::PaymentChannel::new(
                job_id.to_string(),
                payer.clone(),
                payee.clone(),
                chain,
                amount,
                payer_address,
                payee_address,
            );
            state
                .write()
                .await
                .payment_manager
                .insert_funded(channel, info);
        }
        Ok(Err(e)) => tracing::warn!(
            "could not open payment channel for job {job_id}: {}",
            sanitize_log(&e.to_string())
        ),
        Err(e) => tracing::warn!("channel-open task for job {job_id} panicked: {e}"),
    }
}

async fn sweep_unpaid_foreign_jobs(state: &Arc<RwLock<crate::api::AppState>>) {
    let candidates: Vec<(String, String, gt_compute::payment::ChainType, String)> = {
        let s = state.read().await;
        s.job_store
            .list()
            .iter()
            .filter(|j| {
                matches!(j.status, gt_compute::job::JobStatus::Pending)
                    && s.foreign_repos.contains_key(&j.id)
                    && (chrono::Utc::now() - j.created_at).num_seconds() > UNPAID_JOB_TIMEOUT_SECS
            })
            .filter_map(|j| {
                s.payment_manager
                    .settle_info(&j.id)
                    .map(|(channel_id, chain, escrow)| (j.id.clone(), channel_id, chain, escrow))
            })
            .collect()
    };
    for (job_id, _channel_id, chain, escrow) in candidates {
        let backend = { state.read().await.payment_manager.backend_for(&chain) };
        let Some(backend) = backend else {
            continue;
        };
        let funded = tokio::task::spawn_blocking(move || backend.escrow_funded(&escrow)).await;
        if !matches!(funded, Ok(Ok(Some(false)))) {
            continue;
        }
        tracing::warn!("dropping unpaid foreign job {job_id} (invoice never paid)");
        settle_job_payment(state, &job_id, false).await;
        let mut s = state.write().await;
        s.job_store.remove(&job_id);
        if let Some(fr) = s.foreign_repos.remove(&job_id) {
            let _ = std::fs::remove_dir_all(&fr.path);
        }
    }
}

pub async fn run_dispatch_tick(state: Arc<RwLock<crate::api::AppState>>) {
    let (compute, data_dir, identity) = {
        let s = state.read().await;
        match s.config.compute.clone() {
            Some(c) if c.worker_enabled => (c, s.config.data_dir.clone(), s.identity.clone()),
            _ => return,
        }
    };
    let caps = LocalCaps {
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
    sweep_unpaid_foreign_jobs(&state).await;
    let (repos, foreign_ids): (Vec<(String, std::path::PathBuf)>, Vec<String>) = {
        let s = state.read().await;
        (
            s.config
                .repos
                .iter()
                .map(|r| (r.name.clone(), r.path.clone()))
                .collect(),
            s.foreign_repos.keys().cloned().collect(),
        )
    };
    let repo_available = |job: &gt_compute::job::ComputeJob| {
        foreign_ids.iter().any(|id| id == &job.id)
            || repos.iter().any(|(n, p)| n == &job.repo_name && p.exists())
    };
    let any_selectable =
        !gt_compute::dispatch::selectable(&state.read().await.job_store, &caps, &repo_available)
            .is_empty();
    if !any_selectable {
        let pending = state.read().await.job_store.pending().len();
        if pending > 0 {
            tracing::debug!(
                "worker idle with {pending} pending job(s): none fit caps (cpu={} ram={} disk={}) or lack repos",
                caps.cpu_cores,
                caps.ram_mb,
                caps.disk_mb
            );
        }
        return;
    }
    let running = gt_compute::dispatch::running_count(&state.read().await.job_store);
    if running >= compute.max_concurrent_jobs as usize {
        return;
    }
    let docker_runner = match compute.execution_mode {
        gt_core::config::ExecutionMode::Docker => {
            let mut runner = match gt_compute::runner::JobRunner::with_socket(
                &compute.docker_socket,
                data_dir.join("jobs"),
            ) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("worker cannot init docker runner: {e}");
                    return;
                }
            };
            if !runner.check_docker().await {
                tracing::warn!("worker skipping dispatch: docker unreachable");
                return;
            }
            runner.set_allowed_images(compute.allowed_images.clone());
            runner.set_resource_caps(gt_compute::runner::ResourceCaps {
                cpu_cores: compute.max_cpu_cores,
                ram_mb: compute.max_ram_mb,
                timeout_secs: compute.max_job_timeout_secs,
                disk_mb: compute.max_disk_mb,
            });
            runner.set_artifact_sink(std::sync::Arc::new(BlobArtifactSink::new(
                state.read().await.blob_store.clone(),
            )));
            Some(runner)
        }
        gt_core::config::ExecutionMode::Local => None,
    };
    let claimed = {
        let mut s = state.write().await;
        gt_compute::dispatch::claim_next_job(
            &mut s.job_store,
            &caps,
            identity.peer_id(),
            &repo_available,
        )
    };
    let Some(job) = claimed else {
        return;
    };
    let api_base = {
        let s = state.read().await;
        if job.submitter == identity.peer_id() {
            Some(format!("http://127.0.0.1:{}", s.config.api_port))
        } else {
            crate::fetch::peer_http_base(&s.known_peers, &job.submitter)
        }
    };
    if !job_is_funded(&state, &job.id).await {
        tracing::info!("deferring job {}: payment not yet received", job.id);
        let mut s = state.write().await;
        if let Some(j) = s.job_store.get_mut(&job.id) {
            j.status = gt_compute::job::JobStatus::Pending;
        }
        return;
    }
    let repo_path = {
        let s = state.read().await;
        if let Some(fr) = s.foreign_repos.get(&job.id) {
            Some(fr.path.clone())
        } else {
            repos
                .iter()
                .find(|(n, _)| *n == job.repo_name)
                .map(|(_, p)| p.clone())
        }
    };
    let Some(repo_path) = repo_path else {
        let cancelled = {
            let s = &mut *state.write().await;
            let cancelled = gt_compute::dispatch::fail_job_fail_fast(
                &mut s.job_store,
                &job.id,
                "repo unavailable".into(),
            );
            gt_compute::dispatch::unblock_ready_jobs(&mut s.job_store, &s.result_store);
            crate::api::fire_workflow_run_triggers(s, &job.id);
            cancelled
        };
        settle_job_payment(&state, &job.id, false).await;
        settle_cancelled_siblings(&state, &cancelled).await;
        return;
    };
    tracing::info!("worker running job {} for repo {}", job.id, job.repo_name);
    let result = match compute.execution_mode {
        gt_core::config::ExecutionMode::Local => {
            let mut local = gt_compute::local::LocalExecutor::new(data_dir.join("jobs"));
            local.set_resource_caps(gt_compute::runner::ResourceCaps {
                cpu_cores: compute.max_cpu_cores,
                ram_mb: compute.max_ram_mb,
                timeout_secs: compute.max_job_timeout_secs,
                disk_mb: compute.max_disk_mb,
            });
            local.set_artifact_sink(std::sync::Arc::new(BlobArtifactSink::new(
                state.read().await.blob_store.clone(),
            )));
            local.set_daemon_config(state.read().await.config_path.clone());
            if let Some(base) = &api_base {
                local.set_api_base(base.clone());
            }
            local.run_job(&job, &repo_path, &identity).await
        }
        gt_core::config::ExecutionMode::Docker => match docker_runner {
            Some(mut runner) => {
                if let Some(base) = &api_base {
                    runner.set_api_base(base.clone());
                }
                runner.run_job(&job, &repo_path, &identity).await
            }
            None => Err(anyhow::anyhow!("docker runner missing")),
        },
    };
    let succeeded = result.is_ok();
    let (cancelled, blobs) = {
        let s = &mut *state.write().await;
        let out = match result {
            Ok(r) => {
                let blobs = r.artifact_blobs.clone();
                let units = gt_compute::credit::compute_units(r.execution_time_ms);
                let money_path = s.payment_manager.for_job(&job.id).is_some()
                    || job
                        .preferred_chain
                        .parse::<gt_compute::payment::ChainType>()
                        .ok()
                        .is_some_and(|c| s.payment_manager.backend_for(&c).is_some());
                if job.submitter != identity.peer_id() && !money_path {
                    s.credit_ledger.record_given(&job.submitter, units);
                    crate::api::persist_credit(s);
                }
                if r.succeeded() {
                    gt_compute::dispatch::complete_job(&mut s.job_store, &mut s.result_store, r);
                    gt_compute::dispatch::unblock_ready_jobs(&mut s.job_store, &s.result_store);
                    (Vec::new(), blobs)
                } else {
                    let code = r.exit_code;
                    s.result_store.add(r);
                    let cancelled = gt_compute::dispatch::fail_job_fail_fast(
                        &mut s.job_store,
                        &job.id,
                        format!("exit code {code}"),
                    );
                    gt_compute::dispatch::unblock_ready_jobs(&mut s.job_store, &s.result_store);
                    (cancelled, blobs)
                }
            }
            Err(e) => {
                let mut reason = e.to_string();
                if reason.len() > 500 {
                    reason.truncate(500);
                }
                let cancelled =
                    gt_compute::dispatch::fail_job_fail_fast(&mut s.job_store, &job.id, reason);
                gt_compute::dispatch::unblock_ready_jobs(&mut s.job_store, &s.result_store);
                (cancelled, Vec::new())
            }
        };
        crate::api::fire_workflow_run_triggers(s, &job.id);
        out
    };
    settle_job_payment(&state, &job.id, succeeded).await;
    settle_cancelled_siblings(&state, &cancelled).await;
    announce_result_blobs(&state, &blobs).await;
    {
        let mut s = state.write().await;
        s.submitter_registry.record_runnable(&job.submitter);
        crate::api::persist_trust(&s);
    }
    ship_result_back(&state, &job.id).await;
}

async fn job_is_funded(state: &Arc<RwLock<crate::api::AppState>>, job_id: &str) -> bool {
    let (info, backend) = {
        let s = state.read().await;
        match s.payment_manager.settle_info(job_id) {
            Some((_, chain, escrow)) => (Some(escrow), s.payment_manager.backend_for(&chain)),
            None => return true,
        }
    };
    let (Some(escrow), Some(backend)) = (info, backend) else {
        return true;
    };
    match tokio::task::spawn_blocking(move || backend.escrow_funded(&escrow)).await {
        Ok(Ok(Some(funded))) => funded,
        Ok(Ok(None)) => true,
        Ok(Err(e)) => {
            tracing::warn!(
                "deferring job {job_id}: funding check failed: {}",
                sanitize_log(&e.to_string())
            );
            false
        }
        Err(e) => {
            tracing::warn!("deferring job {job_id}: funding check panicked: {e}");
            false
        }
    }
}

pub async fn fetch_job_repo(
    submitter_base: &str,
    submitter: &str,
    repo: &str,
    commit_sha: &str,
    dest: &std::path::Path,
) -> anyhow::Result<std::path::PathBuf> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(120))
        .build()?;
    let url = format!("{submitter_base}/api/remote/{submitter}/{repo}/fetch");
    let resp = client.post(&url).send().await?;
    if !resp.status().is_success() {
        anyhow::bail!("fetch failed: {}", resp.status());
    }
    if let Some(len) = resp.content_length() {
        if len > 512 * 1024 * 1024 {
            anyhow::bail!("bundle too large: {len}");
        }
    }
    let bytes = resp.bytes().await?;
    std::fs::create_dir_all(dest)?;
    let bundle_path = std::env::temp_dir().join(format!("gf_job_{}.bundle", uuid::Uuid::new_v4()));
    std::fs::write(&bundle_path, &bytes)?;
    let run_git = async |args: &[&str]| {
        tokio::process::Command::new("git")
            .args(args)
            .current_dir(dest)
            .output()
            .await
    };
    let init_out = run_git(&["init", "-q"]).await?;
    if !init_out.status.success() {
        anyhow::bail!("git init failed");
    }
    let bundle_arg = bundle_path.display().to_string();
    let unbundle_out = run_git(&["bundle", "unbundle", &bundle_arg]).await?;
    let _ = std::fs::remove_file(&bundle_path);
    if !unbundle_out.status.success() {
        anyhow::bail!("bundle unbundle failed");
    }
    let verify_out =
        run_git(&["rev-parse", "--verify", &format!("{commit_sha}^{{commit}}")]).await?;
    if !verify_out.status.success() {
        anyhow::bail!("commit {commit_sha} not in bundle");
    }
    let checkout_out = run_git(&["checkout", "-q", commit_sha]).await?;
    if !checkout_out.status.success() {
        anyhow::bail!("checkout of {commit_sha} failed");
    }
    Ok(dest.to_path_buf())
}

pub async fn ship_result_back(state: &Arc<RwLock<crate::api::AppState>>, job_id: &str) {
    let (foreign, result, failed_reason, known_peers, cluster, our_id) = {
        let s = state.read().await;
        let foreign = s.foreign_repos.get(job_id).cloned();
        let result = s.result_store.for_job(job_id).cloned();
        let failed_reason = match s.job_store.get(job_id).map(|j| &j.status) {
            Some(gt_compute::job::JobStatus::Failed { reason }) => Some(reason.clone()),
            _ => None,
        };
        (
            foreign,
            result,
            failed_reason,
            s.known_peers.clone(),
            s.config.cluster_token.clone(),
            s.identity.clone(),
        )
    };
    let Some(fr) = foreign else {
        return;
    };
    if result.is_none() && failed_reason.is_none() {
        return;
    }
    let Some(base) = crate::fetch::peer_http_base(&known_peers, &fr.submitter) else {
        tracing::warn!("cannot ship job {job_id}: submitter unreachable");
        return;
    };
    let Some(cluster) = cluster else {
        tracing::warn!("cannot ship job {job_id}: no cluster token");
        return;
    };
    let mut msg = if let Some(r) = result {
        gt_core::protocol::JobResultMessage {
            job_id: job_id.to_string(),
            worker_id: our_id.peer_id(),
            result_id: r.id.clone(),
            exit_code: r.exit_code,
            stdout_hash: r.stdout_hash.clone(),
            stderr_hash: r.stderr_hash.clone(),
            stdout_tail: r.stdout_tail.clone(),
            stderr_tail: r.stderr_tail.clone(),
            artifact_hashes: r.artifact_hashes.clone(),
            artifact_blobs: r.artifact_blobs.clone(),
            execution_time_ms: r.execution_time_ms,
            outputs: r.outputs.clone(),
            summary_md: r.summary_md.clone(),
            peak_cpu_percent: r.resource_usage.peak_cpu_percent,
            peak_ram_mb: r.resource_usage.peak_ram_mb,
            disk_used_mb: r.resource_usage.disk_used_mb,
            result_signature: r.signature.clone(),
            signature: vec![],
        }
    } else {
        gt_core::protocol::JobResultMessage {
            job_id: job_id.to_string(),
            worker_id: our_id.peer_id(),
            result_id: String::new(),
            exit_code: -1,
            stdout_hash: String::new(),
            stderr_hash: String::new(),
            stdout_tail: String::new(),
            stderr_tail: failed_reason.unwrap_or_default(),
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
        }
    };
    let payload = msg.signable_payload();
    msg.signature = our_id.sign(&payload);
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let url = format!("{base}/api/compute/remote/result");
    let shipped = match client
        .post(&url)
        .header("Authorization", format!("Bearer {cluster}"))
        .json(&msg)
        .send()
        .await
    {
        Ok(resp) => resp
            .json::<serde_json::Value>()
            .await
            .ok()
            .and_then(|v| v.get("ok").and_then(|o| o.as_bool()))
            .unwrap_or(false),
        Err(e) => {
            tracing::warn!(
                "ship job {job_id} to {} failed: {}",
                sanitize_log(&base),
                sanitize_log(&e.to_string())
            );
            false
        }
    };
    if shipped {
        let mut s = state.write().await;
        if let Some(fr) = s.foreign_repos.remove(job_id) {
            let _ = std::fs::remove_dir_all(&fr.path);
        }
    }
}

pub const MATCH_TIMEOUT_SECS: i64 = 1800;

async fn pay_for_match(
    state: &Arc<RwLock<crate::api::AppState>>,
    job: &gt_compute::job::ComputeJob,
    worker: &gt_core::identity::PeerId,
    response: Option<&serde_json::Value>,
) {
    let chain: Result<gt_compute::payment::ChainType, String> = job.preferred_chain.parse();
    let Ok(chain) = chain else {
        tracing::warn!(
            "job {} has unknown chain '{}'",
            job.id,
            sanitize_log(&job.preferred_chain)
        );
        return;
    };
    let backend = { state.read().await.payment_manager.backend_for(&chain) };
    let Some(backend) = backend else {
        tracing::info!(
            "job {} runs on mutual credit (no {} backend)",
            job.id,
            chain
        );
        return;
    };
    match chain {
        gt_compute::payment::ChainType::BitcoinLightning => {
            let invoice = response
                .and_then(|v| v.get("payment_details"))
                .and_then(|d| d.as_str())
                .map(|s| s.to_string());
            let Some(invoice) = invoice else {
                tracing::info!(
                    "job {} runs on mutual credit (worker sent no invoice)",
                    job.id
                );
                return;
            };
            let amount = job.max_payment_sats;
            let job_id = job.id.clone();
            let outcome =
                tokio::task::spawn_blocking(move || backend.pay_invoice(&invoice, amount)).await;
            match outcome {
                Ok(Ok(())) => {
                    tracing::info!("paid worker invoice for job {job_id}");
                    let mut s = state.write().await;
                    s.credit_ledger.mark_paid(&job_id, worker);
                    crate::api::persist_credit(&s);
                }
                Ok(Err(e)) => tracing::warn!(
                    "could not pay for job {job_id}: {}",
                    sanitize_log(&e.to_string())
                ),
                Err(e) => tracing::warn!("payment task for job {job_id} panicked: {e}"),
            }
        }
        gt_compute::payment::ChainType::EthereumL2(_) => {
            let (worker_wallet, our_address) = {
                let s = state.read().await;
                let wallet = response
                    .and_then(|v| v.get("worker_wallet"))
                    .and_then(|w| w.as_str())
                    .map(|s| s.to_string());
                let ours = s
                    .config
                    .payments
                    .evm
                    .iter()
                    .find(|e| {
                        e.network.parse::<gt_compute::payment::ChainType>().as_ref() == Ok(&chain)
                    })
                    .map(|e| e.from_address.clone());
                (wallet, ours)
            };
            let (Some(worker_wallet), Some(our_address)) = (worker_wallet, our_address) else {
                tracing::warn!("job {} has no EVM wallet route; runs unpaid", job.id);
                return;
            };
            open_job_channel(
                state,
                OpenChannelParams {
                    job_id: &job.id,
                    payer: &job.submitter,
                    payee: worker,
                    chain,
                    amount: job.payment_units(),
                    payer_address: our_address,
                    payee_address: worker_wallet,
                },
            )
            .await;
        }
        gt_compute::payment::ChainType::Solana => {
            tracing::info!("job {} runs on mutual credit (solana unsupported)", job.id);
        }
    }
}

async fn send_job_cancels(
    state: &Arc<RwLock<crate::api::AppState>>,
    cancels: Vec<(String, gt_core::identity::PeerId, &'static str)>,
) {
    if cancels.is_empty() {
        return;
    }
    let (cluster, identity, known_peers) = {
        let s = state.read().await;
        let Some(cluster) = s.config.cluster_token.clone() else {
            return;
        };
        (cluster, s.identity.clone(), s.known_peers.clone())
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    let our_id = identity.peer_id();
    for (job_id, worker, reason) in cancels {
        let Some(base) = crate::fetch::peer_http_base(&known_peers, &worker) else {
            continue;
        };
        let mut msg = gt_core::protocol::JobCancelMessage {
            job_id: job_id.clone(),
            submitter: our_id.clone(),
            reason: reason.into(),
            signature: vec![],
        };
        let payload = msg.signable_payload();
        msg.signature = identity.sign(&payload);
        let ok = post_job_cancel(&client, &base, &cluster, &msg).await;
        tracing::info!(
            "cancel for job {job_id} sent to {}: {ok}",
            sanitize_log(&base)
        );
    }
}

async fn post_job_cancel(
    client: &reqwest::Client,
    base: &str,
    cluster: &str,
    msg: &gt_core::protocol::JobCancelMessage,
) -> bool {
    match client
        .post(format!("{base}/api/compute/remote/cancel"))
        .header("Authorization", format!("Bearer {cluster}"))
        .json(msg)
        .send()
        .await
    {
        Ok(resp) => resp.status().is_success(),
        Err(e) => {
            tracing::warn!(
                "cancel for job {} to {} failed: {}",
                msg.job_id,
                sanitize_log(base),
                sanitize_log(&e.to_string())
            );
            false
        }
    }
}

pub async fn send_cancel_to_worker(
    state: &Arc<RwLock<crate::api::AppState>>,
    job_id: &str,
    worker: &gt_core::identity::PeerId,
    reason: &str,
) -> bool {
    let (cluster, identity, known_peers) = {
        let s = state.read().await;
        let Some(cluster) = s.config.cluster_token.clone() else {
            return false;
        };
        (cluster, s.identity.clone(), s.known_peers.clone())
    };
    let Some(base) = crate::fetch::peer_http_base(&known_peers, worker) else {
        return false;
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    let mut msg = gt_core::protocol::JobCancelMessage {
        job_id: job_id.to_string(),
        submitter: identity.peer_id(),
        reason: reason.to_string(),
        signature: vec![],
    };
    let payload = msg.signable_payload();
    msg.signature = identity.sign(&payload);
    post_job_cancel(&client, &base, &cluster, &msg).await
}

pub async fn run_submit_tick(state: Arc<RwLock<crate::api::AppState>>) {
    {
        let mut s = state.write().await;
        let ghosts: Vec<(String, gt_core::identity::PeerId)> = s
            .job_store
            .list()
            .iter()
            .filter_map(|j| match &j.status {
                gt_compute::job::JobStatus::Matched { worker, matched_at }
                    if (chrono::Utc::now() - *matched_at).num_seconds() > MATCH_TIMEOUT_SECS =>
                {
                    Some((j.id.clone(), worker.clone()))
                }
                _ => None,
            })
            .collect();
        let requeued =
            gt_compute::dispatch::requeue_stale_matches(&mut s.job_store, MATCH_TIMEOUT_SECS);
        if !requeued.is_empty() {
            tracing::warn!("requeued {} stale matched jobs", requeued.len());
        }
        for (job_id, worker) in &ghosts {
            if requeued.contains(job_id) {
                s.worker_registry.record_ghost(worker);
            }
        }
        let stale_verifiers: Vec<(String, gt_core::identity::PeerId)> = s
            .job_store
            .list()
            .iter()
            .filter_map(|j| match &j.status {
                gt_compute::job::JobStatus::Verifying {
                    verifier: Some(verifier),
                    matched_at,
                    ..
                } if (chrono::Utc::now() - *matched_at).num_seconds() > MATCH_TIMEOUT_SECS => {
                    Some((j.id.clone(), verifier.clone()))
                }
                _ => None,
            })
            .collect();
        for (job_id, verifier) in &stale_verifiers {
            if let Some(job) = s.job_store.get_mut(job_id) {
                if let gt_compute::job::JobStatus::Verifying { verifier: slot, .. } =
                    &mut job.status
                {
                    *slot = None;
                }
            }
            s.worker_registry.record_ghost(verifier);
        }
        if !stale_verifiers.is_empty() {
            tracing::warn!(
                "reset {} ghosted verification matches",
                stale_verifiers.len()
            );
        }
        crate::api::persist_trust(&s);
        drop(s);
        for job_id in &requeued {
            settle_job_payment(&state, job_id, false).await;
        }
        for (job_id, verifier) in &stale_verifiers {
            refund_channel_for_payee(&state, job_id, verifier).await;
        }
        let cancels: Vec<(String, gt_core::identity::PeerId, &'static str)> = ghosts
            .into_iter()
            .filter(|(job_id, _)| requeued.contains(job_id))
            .map(|(job_id, worker)| (job_id, worker, "stale match"))
            .chain(
                stale_verifiers
                    .into_iter()
                    .map(|(job_id, verifier)| (job_id, verifier, "stale verification")),
            )
            .collect();
        send_job_cancels(&state, cancels).await;
    }
    let (cluster, our_id, identity, known_peers, jobs) = {
        let s = state.read().await;
        if s.config
            .compute
            .as_ref()
            .map(|c| c.worker_enabled)
            .unwrap_or(false)
        {
            return;
        }
        let Some(cluster) = s.config.cluster_token.clone() else {
            return;
        };
        let jobs: Vec<gt_compute::job::ComputeJob> = s
            .job_store
            .list()
            .into_iter()
            .filter(|j| {
                !s.foreign_repos.contains_key(&j.id)
                    && matches!(
                        j.status,
                        gt_compute::job::JobStatus::Pending
                            | gt_compute::job::JobStatus::Verifying { verifier: None, .. }
                    )
            })
            .take(2)
            .cloned()
            .collect();
        if jobs.is_empty() {
            return;
        }
        (
            cluster,
            s.identity.peer_id(),
            s.identity.clone(),
            s.known_peers.clone(),
            jobs,
        )
    };
    let client = match reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(60))
        .build()
    {
        Ok(c) => c,
        Err(_) => return,
    };
    for job in jobs {
        let excluded = match &job.status {
            gt_compute::job::JobStatus::Verifying { first_worker, .. } => {
                Some(first_worker.clone())
            }
            _ => None,
        };
        let candidates: Vec<gt_core::identity::PeerId> = {
            let s = state.read().await;
            s.worker_registry
                .find_matching(&job.requirements)
                .into_iter()
                .map(|w| w.offer.worker_id.clone())
                .filter(|w| w != &our_id && Some(w) != excluded.as_ref())
                .collect()
        };
        for worker in candidates {
            let Some(base) = crate::fetch::peer_http_base(&known_peers, &worker) else {
                continue;
            };
            let config_json = match serde_json::to_string(&job.job_config) {
                Ok(j) => j,
                Err(_) => continue,
            };
            let mut msg = gt_core::protocol::JobSubmissionMessage {
                job_id: job.id.clone(),
                submitter: our_id.clone(),
                repo_owner: job.repo_owner.clone(),
                repo_name: job.repo_name.clone(),
                commit_sha: job.commit_sha.clone(),
                job_config_json: config_json,
                min_cpu_cores: job.requirements.min_cpu_cores,
                min_ram_mb: job.requirements.min_ram_mb,
                min_disk_mb: job.requirements.min_disk_mb,
                gpu_required: job.requirements.gpu_required,
                network_access: job.requirements.network_access,
                max_payment_sats: job.max_payment_sats,
                preferred_chain: job.preferred_chain.clone(),
                signature: vec![],
            };
            let payload = msg.signable_payload();
            msg.signature = identity.sign(&payload);
            let url = format!("{base}/api/compute/remote/submit");
            let response: Option<serde_json::Value> = match client
                .post(&url)
                .header("Authorization", format!("Bearer {cluster}"))
                .json(&msg)
                .send()
                .await
            {
                Ok(resp) => resp.json::<serde_json::Value>().await.ok(),
                Err(e) => {
                    tracing::warn!(
                        "submit job {} to {} failed: {}",
                        job.id,
                        sanitize_log(&base),
                        sanitize_log(&e.to_string())
                    );
                    None
                }
            };
            let accepted = response
                .as_ref()
                .and_then(|v| v.get("accepted").and_then(|a| a.as_bool()))
                .unwrap_or(false);
            if accepted {
                pay_for_match(&state, &job, &worker, response.as_ref()).await;
                let mut s = state.write().await;
                if let Some(j) = s.job_store.get_mut(&job.id) {
                    match &mut j.status {
                        gt_compute::job::JobStatus::Verifying {
                            verifier,
                            matched_at,
                            ..
                        } => {
                            *verifier = Some(worker.clone());
                            *matched_at = chrono::Utc::now();
                        }
                        status => {
                            *status = gt_compute::job::JobStatus::Matched {
                                worker: worker.clone(),
                                matched_at: chrono::Utc::now(),
                            };
                        }
                    }
                }
                break;
            }
        }
    }
}
