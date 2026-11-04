use anyhow::{Context, Result};
use bollard::auth::DockerCredentials;
use bollard::container::{
    Config as ContainerConfig, CreateContainerOptions, LogsOptions, NetworkingConfig,
    RemoveContainerOptions, StartContainerOptions, Stats, StatsOptions, StopContainerOptions,
    WaitContainerOptions,
};
use bollard::image::CreateImageOptions;
use bollard::models::{EndpointSettings, HealthConfig, HealthStatusEnum, HostConfig, PortBinding};
use bollard::network::CreateNetworkOptions;
use bollard::Docker;
use futures_util::StreamExt;
use std::path::Path;
use tracing::{info, warn};

use crate::job::{ComputeJob, ComputeRequirements, JobStep};
use crate::result::{JobResult, ResourceUsage};
use gt_core::identity::Identity;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

pub const MAX_LOG_BYTES: u64 = 8 * 1024 * 1024;
pub const MAX_ARTIFACT_BYTES: u64 = 64 * 1024 * 1024;
pub const MAX_ARTIFACTS: usize = 16;

pub struct ResourceCaps {
    pub cpu_cores: u32,
    pub ram_mb: u64,
    pub timeout_secs: u64,
    pub disk_mb: u64,
}

#[derive(Debug, Default)]
struct ContainerPeakStats {
    prev_cpu_total: u64,
    prev_cpu_system: u64,
    have_prev: bool,
    peak_cpu_percent: f64,
    peak_ram_bytes: u64,
}

impl ContainerPeakStats {
    fn observe(&mut self, stats: &Stats) {
        if let Some(usage) = stats.memory_stats.usage {
            self.peak_ram_bytes = self.peak_ram_bytes.max(usage);
        }
        let total = stats.cpu_stats.cpu_usage.total_usage;
        let system = stats.cpu_stats.system_cpu_usage.unwrap_or(0);
        if self.have_prev {
            let cpu_delta = total.saturating_sub(self.prev_cpu_total);
            let sys_delta = system.saturating_sub(self.prev_cpu_system);
            if sys_delta > 0 && cpu_delta > 0 {
                let ncpu = stats
                    .cpu_stats
                    .online_cpus
                    .or_else(|| {
                        stats
                            .cpu_stats
                            .cpu_usage
                            .percpu_usage
                            .as_ref()
                            .map(|v| v.len() as u64)
                    })
                    .filter(|n| *n > 0)
                    .unwrap_or(1) as f64;
                let pct = cpu_delta as f64 / sys_delta as f64 * ncpu * 100.0;
                if pct.is_finite() {
                    self.peak_cpu_percent = self.peak_cpu_percent.max(pct);
                }
            }
        }
        self.prev_cpu_total = total;
        self.prev_cpu_system = system;
        self.have_prev = true;
    }
}

async fn sample_container_peaks(
    docker: Docker,
    container_id: String,
    peaks: std::sync::Arc<std::sync::Mutex<(f64, u64)>>,
) {
    let mut state = ContainerPeakStats::default();
    loop {
        let mut stream = docker.stats(
            &container_id,
            Some(StatsOptions {
                stream: false,
                one_shot: true,
            }),
        );
        match stream.next().await {
            Some(Ok(stats)) => {
                state.observe(&stats);
                if let Ok(mut guard) = peaks.lock() {
                    *guard = (state.peak_cpu_percent, state.peak_ram_bytes);
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            _ => break,
        }
    }
}

impl Default for ResourceCaps {
    fn default() -> Self {
        ResourceCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            timeout_secs: 3600,
            disk_mb: 10240,
        }
    }
}

pub fn effective_limits(
    req: &ComputeRequirements,
    timeout_secs: u64,
    caps: &ResourceCaps,
) -> (u32, u64, u64) {
    (
        req.min_cpu_cores.min(caps.cpu_cores).max(1),
        req.min_ram_mb.min(caps.ram_mb).max(64),
        timeout_secs.min(caps.timeout_secs).max(1),
    )
}

pub fn sanitize_container_name(job_id: &str) -> String {
    let short: String = job_id
        .chars()
        .take(8)
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!("gitforest-job-{short}")
}

pub const GF_OUTPUTS_ENV: &str = "GF_OUTPUTS";
pub const GF_JOB_TOKEN_ENV: &str = "GF_JOB_TOKEN";
pub const GF_API_URL_ENV: &str = "GF_API_URL";
pub const GF_SUMMARY_ENV: &str = "GF_SUMMARY";
pub const MAX_SUMMARY_BYTES: u64 = 262144;
pub const MAX_OUTPUT_KEYS: usize = 100;
pub const MAX_OUTPUT_KEY_LEN: usize = 128;
pub const MAX_OUTPUT_VALUE_BYTES: usize = 65536;

pub fn valid_output_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    key.len() <= MAX_OUTPUT_KEY_LEN && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub const STEP_MARKER_KEY: &str = "__GF_STEP__";

pub fn parse_github_outputs(data: &[u8]) -> std::collections::HashMap<String, String> {
    let mut out = std::collections::HashMap::new();
    for (_, key, value) in parse_output_entries(data) {
        if out.len() >= MAX_OUTPUT_KEYS && !out.contains_key(&key) {
            break;
        }
        out.insert(key, value);
    }
    out
}

pub fn parse_github_outputs_with_steps(
    data: &[u8],
) -> (
    std::collections::HashMap<String, String>,
    std::collections::HashMap<String, std::collections::HashMap<String, String>>,
) {
    let mut union = std::collections::HashMap::new();
    let mut per_step: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    for (step, key, value) in parse_output_entries(data) {
        if union.len() >= MAX_OUTPUT_KEYS && !union.contains_key(&key) {
            break;
        }
        union.insert(key.clone(), value.clone());
        if let Some(id) = step {
            per_step.entry(id).or_default().insert(key, value);
        }
    }
    (union, per_step)
}

fn parse_output_entries(data: &[u8]) -> Vec<(Option<String>, String, String)> {
    let mut out = Vec::new();
    let mut step: Option<String> = None;
    let text = String::from_utf8_lossy(data);
    let lines: Vec<&str> = text.split('\n').collect();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
        index += 1;
        if line.is_empty() {
            continue;
        }
        if let Some((key, delim)) = line.split_once("<<") {
            if !valid_output_key(key) {
                continue;
            }
            if delim.is_empty() {
                continue;
            }
            let mut value_lines: Vec<&str> = Vec::new();
            while index < lines.len() {
                let body = lines[index].strip_suffix('\r').unwrap_or(lines[index]);
                index += 1;
                if body == delim {
                    break;
                }
                value_lines.push(body);
            }
            let mut value = value_lines.join("\n");
            value.truncate(MAX_OUTPUT_VALUE_BYTES);
            if key == STEP_MARKER_KEY {
                continue;
            }
            out.push((step.clone(), key.to_string(), value));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if key == STEP_MARKER_KEY {
            step = Some(value.to_string());
            continue;
        }
        if !valid_output_key(key) {
            continue;
        }
        let mut value = value.to_string();
        value.truncate(MAX_OUTPUT_VALUE_BYTES);
        out.push((step.clone(), key.to_string(), value));
    }
    out
}

pub fn finalize_job_outputs(
    outputs_raw: &[u8],
    config: &crate::job::JobConfig,
    job: &crate::job::ComputeJob,
) -> std::collections::HashMap<String, String> {
    let (mut outputs, per_step) = parse_github_outputs_with_steps(outputs_raw);
    if !config.outputs.is_empty() {
        let ctx = crate::ifexpr::IfContext {
            inputs: job.dispatch_inputs.clone(),
            matrix: job.matrix.clone(),
            env: config.env.clone(),
            ..Default::default()
        };
        outputs = resolve_job_outputs(&config.outputs, &per_step, &ctx);
    }
    scrub_output_values(&mut outputs, &config.secrets);
    outputs
}

pub struct ServiceSet {
    pub network: String,
    pub containers: Vec<(String, String)>,
}

impl ServiceSet {
    pub async fn teardown(&self, docker: &Docker) {
        for (name, id) in &self.containers {
            let _ = docker
                .stop_container(id, Some(StopContainerOptions { t: 5 }))
                .await;
            let _ = docker
                .remove_container(
                    id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
            info!("Removed service container {name} ({id})");
        }
        let _ = docker.remove_network(&self.network).await;
    }
}

async fn pull_docker_image(
    docker: &Docker,
    image: &str,
    auth: Option<DockerCredentials>,
) -> Result<()> {
    info!("Pulling Docker image: {image}");

    let opts = CreateImageOptions {
        from_image: image,
        ..Default::default()
    };

    let mut stream = docker.create_image(Some(opts), None, auth);
    while let Some(result) = stream.next().await {
        match result {
            Ok(info) => {
                if let Some(status) = info.status {
                    tracing::debug!("Pull: {status}");
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!("failed to pull image {image}: {e}"));
            }
        }
    }

    info!("Image {image} pulled successfully");
    Ok(())
}

async fn cleanup_stale_service_net(docker: &Docker, network: &str) {
    let Ok(info) = docker.inspect_network::<String>(network, None).await else {
        return;
    };
    if let Some(containers) = info.containers {
        for id in containers.keys() {
            let _ = docker
                .remove_container(
                    id,
                    Some(RemoveContainerOptions {
                        force: true,
                        ..Default::default()
                    }),
                )
                .await;
        }
    }
    let _ = docker.remove_network(network).await;
}

pub fn service_network_name(job_id: &str) -> String {
    let mut base = sanitize_container_name(job_id);
    base.truncate(48);
    format!("gf-svc-{base}")
}

async fn container_log_tail(docker: &Docker, id: &str) -> String {
    let mut out = String::new();
    let mut stream = docker.logs(
        id,
        Some(LogsOptions {
            follow: false,
            stdout: true,
            stderr: true,
            tail: "20",
            ..Default::default()
        }),
    );
    while let Some(item) = stream.next().await {
        if let Ok(log) = item {
            out.push_str(&log.to_string());
        }
    }
    out.chars().take(2000).collect()
}

async fn wait_service_healthy(
    docker: &Docker,
    name: &str,
    id: &str,
    health: &crate::job::ServiceHealth,
) -> Result<()> {
    let deadline_secs =
        30 + u64::from(health.retries) * (health.interval_secs + health.timeout_secs) + 15;
    let start = std::time::Instant::now();
    loop {
        let inspect = docker.inspect_container(id, None).await?;
        let status = inspect.state.and_then(|s| s.health).and_then(|h| h.status);
        match status {
            Some(HealthStatusEnum::HEALTHY) => {
                info!("Service {name} is healthy");
                return Ok(());
            }
            Some(HealthStatusEnum::UNHEALTHY) => {
                let logs = container_log_tail(docker, id).await;
                anyhow::bail!("service {name} unhealthy:\n{logs}");
            }
            _ => {}
        }
        if start.elapsed().as_secs() >= deadline_secs {
            let logs = container_log_tail(docker, id).await;
            anyhow::bail!("service {name} never became healthy:\n{logs}");
        }
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
    }
}

pub async fn start_job_services(
    docker: &Docker,
    job_id: &str,
    services: &[crate::job::JobService],
    network_access: bool,
) -> Result<ServiceSet> {
    let network = service_network_name(job_id);
    let mut set = ServiceSet {
        network: network.clone(),
        containers: Vec::new(),
    };
    let result: Result<()> = async {
        cleanup_stale_service_net(docker, &network).await;
        docker
            .create_network(CreateNetworkOptions {
                name: network.as_str(),
                check_duplicate: true,
                driver: "bridge",
                internal: !network_access,
                ..Default::default()
            })
            .await
            .context("failed to create services network")?;
        for svc in services {
            pull_docker_image(docker, &svc.image, None).await?;
            let cname = format!(
                "gf-svc-{}-{}",
                sanitize_container_name(job_id),
                sanitize_container_name(&svc.name)
            );
            let mut port_bindings = std::collections::HashMap::new();
            let mut exposed = std::collections::HashMap::new();
            for port in &svc.ports {
                let key = format!("{}/tcp", port.container);
                exposed.insert(key.clone(), std::collections::HashMap::new());
                if let Some(host) = port.host {
                    port_bindings.insert(key, Some(vec![loopback_port_binding(host)]));
                }
            }
            let healthcheck = svc.health.as_ref().map(|h| HealthConfig {
                test: Some(vec!["CMD-SHELL".to_string(), h.cmd.clone()]),
                interval: Some(h.interval_secs as i64 * 1_000_000_000),
                timeout: Some(h.timeout_secs as i64 * 1_000_000_000),
                retries: Some(h.retries as i64),
                start_period: Some(30_000_000_000),
                ..Default::default()
            });
            let mut endpoints = std::collections::HashMap::new();
            endpoints.insert(
                network.clone(),
                EndpointSettings {
                    aliases: Some(vec![svc.name.clone()]),
                    ..Default::default()
                },
            );
            let container_config = ContainerConfig {
                image: Some(svc.image.clone()),
                env: Some(
                    svc.env
                        .iter()
                        .filter(|(k, _)| valid_env_key(k))
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect(),
                ),
                exposed_ports: if exposed.is_empty() {
                    None
                } else {
                    Some(exposed)
                },
                healthcheck,
                host_config: Some(HostConfig {
                    port_bindings: if port_bindings.is_empty() {
                        None
                    } else {
                        Some(port_bindings)
                    },
                    pids_limit: Some(512),
                    ..Default::default()
                }),
                networking_config: Some(NetworkingConfig {
                    endpoints_config: endpoints,
                }),
                ..Default::default()
            };
            let created = docker
                .create_container(
                    Some(CreateContainerOptions {
                        name: cname.as_str(),
                        platform: None,
                    }),
                    container_config,
                )
                .await
                .with_context(|| format!("failed to create service {}", svc.name))?;
            docker
                .start_container(&created.id, None::<StartContainerOptions<String>>)
                .await
                .with_context(|| format!("failed to start service {}", svc.name))?;
            info!("Service {} started as {}", svc.name, created.id);
            set.containers.push((svc.name.clone(), created.id));
        }
        for (name, id) in &set.containers {
            let Some(svc) = services.iter().find(|s| &s.name == name) else {
                continue;
            };
            if let Some(health) = &svc.health {
                wait_service_healthy(docker, name, id, health).await?;
            }
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        set.teardown(docker).await;
    }
    result.map(|()| set)
}

pub fn resolve_job_outputs(
    mapping: &std::collections::HashMap<String, String>,
    per_step: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
    ctx: &crate::ifexpr::IfContext,
) -> std::collections::HashMap<String, String> {
    let mut full = ctx.clone();
    full.steps = per_step.clone();
    mapping
        .iter()
        .map(|(name, template)| {
            (
                name.clone(),
                crate::ifexpr::substitute_all_raw(template, &full),
            )
        })
        .collect()
}

pub fn scrub_output_values(
    outputs: &mut std::collections::HashMap<String, String>,
    secrets: &std::collections::HashMap<String, String>,
) {
    let mut values: Vec<&str> = secrets
        .values()
        .filter(|v| !v.is_empty())
        .map(|v| v.as_str())
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for output in outputs.values_mut() {
        for secret in &values {
            if output.contains(secret) {
                *output = output.replace(secret, "***");
            }
        }
    }
}

pub fn redact_secrets(data: &mut Vec<u8>, secrets: &std::collections::HashMap<String, String>) {
    let mut values: Vec<&[u8]> = secrets
        .values()
        .filter(|v| !v.is_empty())
        .map(|v| v.as_bytes())
        .collect();
    values.sort_by_key(|v| std::cmp::Reverse(v.len()));
    for secret in values {
        if secret.len() > data.len() {
            continue;
        }
        let mut at = 0;
        while let Some(pos) = data[at..].windows(secret.len()).position(|w| w == secret) {
            let start = at + pos;
            data.splice(start..start + secret.len(), *b"***");
            at = start + 3;
        }
    }
}

pub fn valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub fn valid_volume_source(source: &str) -> bool {
    if source.is_empty() || source.len() > 128 {
        return false;
    }
    let mut chars = source.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

pub fn valid_volume_target(target: &str) -> bool {
    target.len() > 1 && target.starts_with('/') && !target.contains("..")
}

pub fn loopback_port_binding(host: u16) -> PortBinding {
    PortBinding {
        host_ip: Some("127.0.0.1".to_string()),
        host_port: Some(host.to_string()),
    }
}

pub fn build_container_binds(
    workspace: &std::path::Path,
    job_dir: &std::path::Path,
    volumes: &[crate::job::JobVolume],
) -> anyhow::Result<Vec<String>> {
    let mut binds = vec![
        format!("{}:/workspace:rw", workspace.display()),
        format!("{}:/job:rw", job_dir.display()),
    ];
    for volume in volumes {
        if !valid_volume_target(&volume.target) {
            anyhow::bail!(
                "container volume target {:?} is not an absolute container path",
                volume.target
            );
        }
        let mode = if volume.read_only { "ro" } else { "rw" };
        match &volume.source {
            Some(source) => {
                if !valid_volume_source(source) {
                    anyhow::bail!("container volume source {source:?} is not a named volume");
                }
                binds.push(format!("{source}:{}:{mode}", volume.target));
            }
            None => {
                if volume.read_only {
                    anyhow::bail!(
                        "container volume {} cannot be read-only without a source",
                        volume.target
                    );
                }
                binds.push(volume.target.clone());
            }
        }
    }
    Ok(binds)
}

pub async fn read_capped_tail(path: &std::path::Path, cap: u64) -> Vec<u8> {
    let file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return Vec::new(),
    };
    let len = file.metadata().await.map(|m| m.len()).unwrap_or(0);
    if len <= cap {
        return tokio::fs::read(path).await.unwrap_or_default();
    }
    let mut file = file;
    if file
        .seek(std::io::SeekFrom::Start(len - cap))
        .await
        .is_err()
    {
        return Vec::new();
    }
    let mut buf = Vec::new();
    let _ = file.read_to_end(&mut buf).await;
    buf
}

pub fn dir_size_bytes(path: &std::path::Path) -> u64 {
    let mut total = 0u64;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let meta = match entry.path().symlink_metadata() {
                Ok(m) => m,
                Err(_) => continue,
            };
            if meta.file_type().is_dir() && !meta.file_type().is_symlink() {
                stack.push(entry.path());
            } else if meta.is_file() {
                total = total.saturating_add(meta.len());
            }
        }
    }
    total
}

pub fn enforce_disk_quota(path: &std::path::Path, cap_mb: u64) -> anyhow::Result<()> {
    if cap_mb == 0 {
        return Ok(());
    }
    let used_mb = dir_size_bytes(path) / (1024 * 1024);
    if used_mb > cap_mb {
        anyhow::bail!("job disk quota exceeded: {used_mb}MB used, {cap_mb}MB allowed");
    }
    Ok(())
}

pub async fn read_capped_exact(path: &std::path::Path, cap: u64) -> Option<Vec<u8>> {
    let file = tokio::fs::File::open(path).await.ok()?;
    let mut taken = file.take(cap + 1);
    let mut buf = Vec::new();
    taken.read_to_end(&mut buf).await.ok()?;
    if buf.len() as u64 > cap {
        return None;
    }
    Some(buf)
}

/// Executes compute jobs in Docker containers.
pub struct JobRunner {
    docker: Docker,
    work_dir: std::path::PathBuf,
    allowed_images: Vec<String>,
    resource_caps: ResourceCaps,
    artifact_sink: Option<std::sync::Arc<dyn ArtifactSink>>,
    api_base: Option<String>,
}

/// Persists artifact bytes somewhere content-addressed and returns the pins.
/// Executors keep working without one; artifacts are then hashes only.
pub trait ArtifactSink: Send + Sync {
    fn store_artifact(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> anyhow::Result<gt_core::types::ArtifactBlob>;
}

pub fn persist_artifacts(
    sink: Option<&std::sync::Arc<dyn ArtifactSink>>,
    artifacts: &[(String, Vec<u8>)],
) -> Vec<gt_core::types::ArtifactBlob> {
    let Some(sink) = sink else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for (name, data) in artifacts {
        match sink.store_artifact(name, data) {
            Ok(blob) => out.push(blob),
            Err(e) => tracing::warn!("artifact {name} bytes not stored: {e}"),
        }
    }
    out
}

pub fn image_allowed(image: &str, allowed: &[String]) -> bool {
    if allowed.is_empty() {
        return true;
    }
    allowed.iter().any(|a| a == image)
}

pub(crate) fn sanitize_artifact_path(pattern: &str) -> Option<std::path::PathBuf> {
    let path = std::path::Path::new(pattern);
    if path.is_absolute() {
        return None;
    }
    if path
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(path.to_path_buf())
}

impl JobRunner {
    /// Connect to Docker via the default socket.
    pub fn new(work_dir: std::path::PathBuf) -> Result<Self> {
        let docker = Docker::connect_with_socket_defaults()
            .context("failed to connect to Docker — is Docker running?")?;
        Ok(JobRunner {
            docker,
            work_dir,
            allowed_images: Vec::new(),
            resource_caps: ResourceCaps::default(),
            artifact_sink: None,
            api_base: None,
        })
    }

    /// Connect to Docker via a custom socket path.
    pub fn with_socket(socket: &str, work_dir: std::path::PathBuf) -> Result<Self> {
        let docker = Docker::connect_with_socket(socket, 120, bollard::API_DEFAULT_VERSION)
            .context("failed to connect to Docker socket")?;
        Ok(JobRunner {
            docker,
            work_dir,
            allowed_images: Vec::new(),
            resource_caps: ResourceCaps::default(),
            artifact_sink: None,
            api_base: None,
        })
    }

    pub fn set_allowed_images(&mut self, images: Vec<String>) {
        self.allowed_images = images;
    }

    pub fn set_resource_caps(&mut self, caps: ResourceCaps) {
        self.resource_caps = caps;
    }

    pub fn set_artifact_sink(&mut self, sink: std::sync::Arc<dyn ArtifactSink>) {
        self.artifact_sink = Some(sink);
    }

    pub fn set_api_base(&mut self, base: String) {
        self.api_base = Some(base);
    }

    pub async fn check_docker(&self) -> bool {
        self.docker.version().await.is_ok()
    }

    /// Execute a compute job and return the signed result.
    pub async fn run_job(
        &self,
        job: &ComputeJob,
        repo_path: &Path,
        worker_identity: &Identity,
    ) -> Result<JobResult> {
        let config = &job.job_config;
        let container_name = sanitize_container_name(&job.id);
        let (cpu_cores, ram_mb, timeout_secs) =
            effective_limits(&job.requirements, config.timeout_secs, &self.resource_caps);

        if self.allowed_images.is_empty() {
            warn!("No allowed_images configured — any Docker image may run on this worker");
        }
        if !image_allowed(&config.docker_image, &self.allowed_images) {
            anyhow::bail!(
                "docker image '{}' is not in the worker allowlist",
                config.docker_image
            );
        }
        for svc in &config.services {
            if !image_allowed(&svc.image, &self.allowed_images) {
                anyhow::bail!(
                    "service image '{}' is not in the worker allowlist",
                    svc.image
                );
            }
        }

        info!("Running job {} in container {}", job.id, container_name);

        // Pull the Docker image
        let registry_auth = config.container.as_ref().and_then(|c| {
            c.credentials.as_ref().map(|creds| DockerCredentials {
                username: Some(creds.username.clone()),
                password: Some(creds.password.clone()),
                ..Default::default()
            })
        });
        self.pull_image(&config.docker_image, registry_auth).await?;

        // Build the combined script from job steps
        let if_ctx = crate::ifexpr::IfContext {
            success: true,
            inputs: job.dispatch_inputs.clone(),
            matrix: job.matrix.clone(),
            env: config.env.clone(),
            ..Default::default()
        };
        let script = build_script(&config.steps, &if_ctx);

        // Create the job working directory
        let job_dir = self.work_dir.join(&job.id);
        tokio::fs::create_dir_all(&job_dir).await?;
        let workspace = job_dir.join("workspace");
        let copy_out = tokio::process::Command::new("cp")
            .args(["-a"])
            .arg(repo_path)
            .arg(&workspace)
            .output()
            .await
            .context("failed to copy repo for docker run")?;
        if !copy_out.status.success() {
            anyhow::bail!("repo copy failed");
        }
        if let Err(e) = enforce_disk_quota(&workspace, self.resource_caps.disk_mb) {
            let _ = tokio::fs::remove_dir_all(&job_dir).await;
            return Err(e.context("repo exceeds job disk quota"));
        }

        // Write the script
        let script_path = job_dir.join("run.sh");
        tokio::fs::write(&script_path, &script).await?;

        let start_time = std::time::Instant::now();

        // Start service sidecars first; health gates run before the main container.
        let services = if config.services.is_empty() {
            None
        } else {
            Some(
                start_job_services(
                    &self.docker,
                    &job.id,
                    &config.services,
                    job.requirements.network_access,
                )
                .await?,
            )
        };

        // Create container with resource limits
        let empty_ports = Vec::new();
        let empty_volumes = Vec::new();
        let empty_env = std::collections::HashMap::new();
        let (wanted_ports, wanted_volumes, wanted_env, wanted_resources) =
            match config.container.as_ref() {
                Some(c) => (&c.ports, &c.volumes, &c.env, Some(&c.resources)),
                None => (
                    &empty_ports,
                    &empty_volumes,
                    &empty_env,
                    Option::<&crate::job::ContainerResources>::None,
                ),
            };
        let network_isolated = !job.requirements.network_access && services.is_none();
        if network_isolated && wanted_ports.iter().any(|p| p.host.is_some()) {
            anyhow::bail!("container ports need network access: set network: true");
        }
        let binds = build_container_binds(&workspace, &job_dir, wanted_volumes)?;
        let mut port_bindings = std::collections::HashMap::new();
        let mut exposed = std::collections::HashMap::new();
        for port in wanted_ports {
            let key = format!("{}/tcp", port.container);
            exposed.insert(key.clone(), std::collections::HashMap::new());
            if let Some(host) = port.host {
                port_bindings.insert(key, Some(vec![loopback_port_binding(host)]));
            }
        }
        let nano_cpus = wanted_resources
            .and_then(|r| r.nano_cpus)
            .map(|ask| ask.min((cpu_cores as i64) * 1_000_000_000).max(1))
            .unwrap_or((cpu_cores as i64) * 1_000_000_000);
        let memory = wanted_resources
            .and_then(|r| r.memory_bytes)
            .map(|ask| ask.min((ram_mb as i64) * 1024 * 1024).max(64 * 1024 * 1024))
            .unwrap_or((ram_mb as i64) * 1024 * 1024);
        let host_config = HostConfig {
            binds: Some(binds),
            memory: Some(memory),
            nano_cpus: Some(nano_cpus),
            shm_size: wanted_resources.and_then(|r| r.shm_bytes),
            network_mode: if network_isolated {
                Some("none".into())
            } else {
                None
            },
            port_bindings: if port_bindings.is_empty() {
                None
            } else {
                Some(port_bindings)
            },
            cap_drop: Some(vec!["ALL".into()]),
            cap_add: Some(vec!["DAC_OVERRIDE".into()]),
            security_opt: Some(vec!["no-new-privileges".into()]),
            pids_limit: Some(512),
            ..Default::default()
        };

        let container_config = ContainerConfig {
            image: Some(config.docker_image.clone()),
            cmd: Some(vec![
                "sh".into(),
                "-c".into(),
                "cd /workspace && sh /job/run.sh > /job/stdout.log 2> /job/stderr.log; echo $? > /job/exit_code".to_string(),
            ]),
            working_dir: Some("/workspace".into()),
            env: Some({
                let mut env: Vec<String> = config
                    .env
                    .iter()
                    .chain(config.secrets.iter())
                    .chain(wanted_env.iter())
                    .filter(|(k, _)| valid_env_key(k))
                    .map(|(k, v)| format!("{k}={v}"))
                    .chain(std::iter::once(format!("{GF_OUTPUTS_ENV}=/job/outputs.env")))
                    .chain(std::iter::once(format!("{GF_SUMMARY_ENV}=/job/summary.md")))
                    .collect();
                if !config.job_token.is_empty() {
                    env.push(format!("{GF_JOB_TOKEN_ENV}={}", config.job_token));
                    if let Some(base) = &self.api_base {
                        env.push(format!("{GF_API_URL_ENV}={base}"));
                    }
                }
                env
            }),
            exposed_ports: if exposed.is_empty() {
                None
            } else {
                Some(exposed)
            },
            host_config: Some(host_config),
            networking_config: services.as_ref().map(|set| NetworkingConfig {
                endpoints_config: std::collections::HashMap::from([(
                    set.network.clone(),
                    EndpointSettings {
                        aliases: Some(vec!["main".to_string()]),
                        ..Default::default()
                    },
                )]),
            }),
            ..Default::default()
        };

        let create_opts = CreateContainerOptions {
            name: &container_name,
            platform: None,
        };

        let container = match self
            .docker
            .create_container(Some(create_opts), container_config)
            .await
        {
            Ok(container) => container,
            Err(e) => {
                if let Some(set) = &services {
                    set.teardown(&self.docker).await;
                }
                return Err(e).context("failed to create container");
            }
        };

        // Start the container
        if let Err(e) = self
            .docker
            .start_container(&container.id, None::<StartContainerOptions<String>>)
            .await
        {
            if let Some(set) = &services {
                set.teardown(&self.docker).await;
            }
            return Err(e).context("failed to start container");
        }

        info!("Container {} started for job {}", container.id, job.id);

        let stat_peaks = std::sync::Arc::new(std::sync::Mutex::new((0.0f64, 0u64)));
        let stats_handle = tokio::spawn(sample_container_peaks(
            self.docker.clone(),
            container.id.clone(),
            stat_peaks.clone(),
        ));

        // Wait for completion with timeout, watching the disk quota live
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let quota_mb = self.resource_caps.disk_mb;
        let quota_dir = job_dir.clone();
        let wait_result = tokio::time::timeout(timeout, async {
            let mut stream = self
                .docker
                .wait_container(&container.id, None::<WaitContainerOptions<String>>);
            let quota_watcher = async {
                if quota_mb == 0 {
                    std::future::pending::<()>().await;
                    return;
                }
                loop {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let dir = quota_dir.clone();
                    let over = tokio::task::spawn_blocking(move || {
                        enforce_disk_quota(&dir, quota_mb).is_err()
                    })
                    .await
                    .unwrap_or(false);
                    if over {
                        break;
                    }
                }
            };
            tokio::select! {
                biased;
                _ = quota_watcher => Err(anyhow::anyhow!("job disk quota exceeded")),
                result = stream.next() => {
                    if let Some(result) = result {
                        match result {
                            Ok(response) => Ok(response.status_code),
                            Err(e) => Err(anyhow::anyhow!("wait error: {e}")),
                        }
                    } else {
                        Err(anyhow::anyhow!("container wait stream ended unexpectedly"))
                    }
                }
            }
        })
        .await;

        stats_handle.abort();
        let (peak_cpu_percent, peak_ram_bytes) = stat_peaks.lock().map(|g| *g).unwrap_or((0.0, 0));

        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        // Read output
        let wait_code = match wait_result {
            Ok(Ok(code)) => code as i32,
            Ok(Err(e)) if e.to_string().contains("disk quota") => {
                warn!("Job exceeded disk quota; killing container");
                let _ = self
                    .docker
                    .kill_container::<String>(&container.id, None)
                    .await;
                -3
            }
            Ok(Err(e)) => {
                warn!("Container error: {e}");
                -1
            }
            Err(_) => {
                warn!("Job timed out after {}s", timeout_secs);
                // Kill the container
                let _ = self
                    .docker
                    .kill_container::<String>(&container.id, None)
                    .await;
                -2 // timeout
            }
        };
        let exit_code = if wait_code == -2 || wait_code == -3 {
            wait_code
        } else {
            match tokio::fs::read_to_string(job_dir.join("exit_code")).await {
                Ok(text) => text.trim().parse::<i32>().unwrap_or(wait_code),
                Err(_) => wait_code,
            }
        };

        // Read stdout and stderr from the job directory
        let mut stdout = read_capped_tail(&job_dir.join("stdout.log"), MAX_LOG_BYTES).await;
        let mut stderr = read_capped_tail(&job_dir.join("stderr.log"), MAX_LOG_BYTES).await;
        redact_secrets(&mut stdout, &config.secrets);
        redact_secrets(&mut stderr, &config.secrets);
        let outputs_raw = read_capped_exact(&job_dir.join("outputs.env"), MAX_LOG_BYTES)
            .await
            .unwrap_or_default();
        let outputs = finalize_job_outputs(&outputs_raw, config, job);
        let mut summary_raw = read_capped_exact(&job_dir.join("summary.md"), MAX_SUMMARY_BYTES)
            .await
            .unwrap_or_default();
        redact_secrets(&mut summary_raw, &config.secrets);
        let summary_md = String::from_utf8_lossy(&summary_raw).into_owned();

        // Collect artifacts
        let mut artifacts = Vec::new();
        for pattern in config.artifacts.iter().take(MAX_ARTIFACTS) {
            let Some(safe) = sanitize_artifact_path(pattern) else {
                warn!("Skipping artifact with unsafe path: {pattern}");
                continue;
            };
            let artifact_path = job_dir.join("workspace").join(safe);
            match read_capped_exact(&artifact_path, MAX_ARTIFACT_BYTES).await {
                Some(data) => artifacts.push((pattern.clone(), data)),
                None => warn!("Skipping artifact {pattern}: missing or oversized"),
            }
        }

        // Clean up container
        let _ = self
            .docker
            .remove_container(
                &container.id,
                Some(RemoveContainerOptions {
                    force: true,
                    v: true,
                    ..Default::default()
                }),
            )
            .await;
        if let Some(set) = &services {
            set.teardown(&self.docker).await;
        }

        if let Err(e) = enforce_disk_quota(&job_dir, self.resource_caps.disk_mb) {
            warn!("Job {} exceeded disk quota: {e}", job.id);
        }
        let disk_used_mb = dir_size_bytes(&job_dir) / (1024 * 1024);

        // Clean up job directory
        let _ = tokio::fs::remove_dir_all(&job_dir).await;

        info!("Job {} completed with exit code {}", job.id, exit_code);

        // Create signed result
        let blobs = persist_artifacts(self.artifact_sink.as_ref(), &artifacts);
        let mut result = JobResult::new(
            worker_identity,
            job.id.clone(),
            exit_code,
            &stdout,
            &stderr,
            artifacts,
            execution_time_ms,
            ResourceUsage {
                peak_cpu_percent: peak_cpu_percent as f32,
                peak_ram_mb: peak_ram_bytes / (1024 * 1024),
                disk_used_mb,
            },
        );
        result.set_artifact_blobs(worker_identity, blobs);
        result.set_outputs(worker_identity, outputs);
        result.set_summary(summary_md);

        Ok(result)
    }

    async fn pull_image(&self, image: &str, auth: Option<DockerCredentials>) -> Result<()> {
        pull_docker_image(&self.docker, image, auth).await
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build a shell script from job steps.
///
/// `ctx` carries the static `if:` context (inputs/matrix/env; needs.* were
/// already reduced at dispatch). Status atoms compile to `GF_FAILED` guards.
pub(crate) fn build_script(steps: &[JobStep], ctx: &crate::ifexpr::IfContext) -> String {
    let mut script = String::from("#!/bin/sh\nGF_FAILED=0\nGF_FAIL_CODE=0\n\n");
    let needs_lookup = steps.iter().any(|s| {
        matches!(&s.condition, crate::job::RunCondition::Expr(raw) if crate::ifexpr::mentions_steps(raw))
    });
    if needs_lookup {
        script.push_str(GF_STEP_OUTPUT_FN);
    }
    for step in steps {
        let outcome_var = step
            .id
            .as_ref()
            .map(|id| format!("GF_STEP_{}_OUTCOME", crate::ifexpr::sanitize_step_id(id)));
        let marker = step.id.as_ref().map(|id| {
            format!("[ -n \"$GF_OUTPUTS\" ] && echo \"__GF_STEP__={id}\" >> \"$GF_OUTPUTS\"\n")
        });
        let skipped_outcome = outcome_var
            .as_ref()
            .map(|var| format!("{var}=skipped\n"))
            .unwrap_or_default();
        script.push_str(&format!("echo '=== {} ==='\n", step.name));
        let body = match &step.working_directory {
            Some(dir) => format!("(cd {} && set -e\n{}\n)\n", shell_quote(dir), step.run),
            None => format!("(set -e\n{}\n)\n", step.run),
        };
        let record = record_snippet(step.continue_on_error, outcome_var.as_deref());
        let run_branch = || {
            let mut branch = String::new();
            if let Some(marker) = &marker {
                branch.push_str(marker);
            }
            branch.push_str(&body);
            branch.push_str(&record);
            branch
        };
        match &step.condition {
            crate::job::RunCondition::Always => {
                script.push_str(&run_branch());
            }
            crate::job::RunCondition::Success => {
                script.push_str("if [ \"$GF_FAILED\" -eq 0 ]; then\n");
                script.push_str(&run_branch());
                script.push_str("else\n");
                script.push_str(&skipped_outcome);
                script.push_str(&format!(
                    "echo '[gitforest] skipping step {} (if: success())'\n",
                    step.name
                ));
                script.push_str("fi\n");
            }
            crate::job::RunCondition::Failure => {
                script.push_str("if [ \"$GF_FAILED\" -ne 0 ]; then\n");
                script.push_str(&run_branch());
                script.push_str("else\n");
                script.push_str(&skipped_outcome);
                script.push_str(&format!(
                    "echo '[gitforest] skipping step {} (if: failure())'\n",
                    step.name
                ));
                script.push_str("fi\n");
            }
            crate::job::RunCondition::Cancelled => {
                script.push_str(&skipped_outcome);
                script.push_str(&format!(
                    "echo '[gitforest] skipping step {} (if: cancelled())'\n",
                    step.name
                ));
            }
            crate::job::RunCondition::Expr(raw) => {
                let reduced =
                    crate::ifexpr::fold_constants(&crate::ifexpr::substitute_known(raw, ctx));
                let skip_echo = format!(
                    "echo '[gitforest] skipping step {} (if: {raw})'\n",
                    step.name
                );
                let skip_branch = || format!("{skipped_outcome}{skip_echo}");
                match reduced.trim() {
                    "true" => {
                        script.push_str(&run_branch());
                    }
                    "false" => {
                        script.push_str(&skip_branch());
                    }
                    _ => match crate::ifexpr::compile_shell_guard(&reduced) {
                        Some(guard) => {
                            script.push_str(&format!("if {guard}; then\n"));
                            script.push_str(&run_branch());
                            script.push_str("else\n");
                            script.push_str(&skip_branch());
                            script.push_str("fi\n");
                        }
                        None => {
                            tracing::warn!(
                                "step {} has runtime-only if {raw:?}; evaluating statically",
                                step.name
                            );
                            if crate::ifexpr::eval(&reduced, ctx) {
                                script.push_str(&run_branch());
                            } else {
                                script.push_str(&skip_branch());
                            }
                        }
                    },
                }
            }
        }
        script.push('\n');
    }
    script.push_str("exit $GF_FAIL_CODE\n");
    script
}

const GF_STEP_OUTPUT_FN: &str = "gf_step_output() {\nawk -F= -v id=\"$1\" -v key=\"$2\" '\nin_h { if ($0 == hdelim) { in_h = 0 } next }\n$1 == \"__GF_STEP__\" { cur = ($2 == id); next }\n/^[^=]+<<[^=]*$/ { in_h = 1; hdelim = substr($0, index($0, \"<<\") + 2); next }\ncur && index($0, \"=\") > 0 { k = substr($0, 1, index($0, \"=\") - 1); if (k == key) { val = substr($0, index($0, \"=\") + 1) } }\nEND { print val }\n' \"${GF_OUTPUTS:-/dev/null}\" 2>/dev/null\n}\n\n";

fn record_snippet(continue_on_error: bool, outcome_var: Option<&str>) -> String {
    let Some(var) = outcome_var else {
        return if continue_on_error {
            GF_SWALLOW.to_string()
        } else {
            GF_RECORD.to_string()
        };
    };
    if continue_on_error {
        format!(
            "GF_RC=$?\n{var}=success\nif [ $GF_RC -ne 0 ]; then\necho \"[gitforest] step failed with $GF_RC (continue-on-error)\"\n{var}=failure\nfi\n"
        )
    } else {
        format!(
            "GF_RC=$?\n{var}=success\nif [ $GF_RC -ne 0 ]; then\nif [ \"$GF_FAILED\" -eq 0 ]; then GF_FAIL_CODE=$GF_RC; fi\nGF_FAILED=1\n{var}=failure\nfi\n"
        )
    }
}

const GF_SWALLOW: &str = "GF_RC=$?\nif [ $GF_RC -ne 0 ]; then\necho \"[gitforest] step failed with $GF_RC (continue-on-error)\"\nfi\n";

const GF_RECORD: &str = "GF_RC=$?\nif [ $GF_RC -ne 0 ]; then\nif [ \"$GF_FAILED\" -eq 0 ]; then GF_FAIL_CODE=$GF_RC; fi\nGF_FAILED=1\nfi\n";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_volume_source_rejects_host_paths() {
        for bad in [
            "/etc",
            "/var/run/docker.sock",
            "../x",
            "a/b",
            "a:b",
            "",
            ".",
            "-x",
            "has space",
        ] {
            assert!(!valid_volume_source(bad), "{bad:?}");
        }
        for good in ["data", "my-vol_1", "a.b", "v2"] {
            assert!(valid_volume_source(good), "{good:?}");
        }
        assert!(!valid_volume_source(&"v".repeat(129)));
    }

    #[test]
    fn test_volume_target_needs_absolute_path() {
        for bad in ["", "etc", "a/../b", "/a/../b", "/"] {
            assert!(!valid_volume_target(bad), "{bad:?}");
        }
        assert!(valid_volume_target("/data"));
    }

    #[test]
    fn test_build_binds_rejects_host_escape() {
        let ws = std::path::PathBuf::from("/tmp/ws");
        let jd = std::path::PathBuf::from("/tmp/jd");
        let hostile = |source: Option<&str>, target: &str| crate::job::JobVolume {
            source: source.map(str::to_string),
            target: target.to_string(),
            read_only: true,
        };
        assert!(build_container_binds(&ws, &jd, &[hostile(Some("/etc"), "/host")]).is_err());
        assert!(
            build_container_binds(&ws, &jd, &[hostile(Some("/var/run/docker.sock"), "/host")])
                .is_err()
        );
        assert!(build_container_binds(&ws, &jd, &[hostile(Some("data"), "relative")]).is_err());
        assert!(build_container_binds(&ws, &jd, &[hostile(None, "/anon")]).is_err());
        let binds = build_container_binds(&ws, &jd, &[hostile(Some("data"), "/data")]).unwrap();
        assert_eq!(binds.len(), 3);
        assert_eq!(binds[2], "data:/data:ro");
    }

    #[test]
    fn test_host_ports_pin_loopback() {
        let bound = loopback_port_binding(8080);
        assert_eq!(bound.host_ip.as_deref(), Some("127.0.0.1"));
        assert_eq!(bound.host_port.as_deref(), Some("8080"));
    }

    fn stats_sample(total_usage: u64, system_usage: u64, mem_bytes: u64, ncpu: u64) -> Stats {
        let cpu = serde_json::json!({
            "cpu_usage": {
                "total_usage": total_usage,
                "usage_in_usermode": 0,
                "usage_in_kernelmode": 0
            },
            "system_cpu_usage": system_usage,
            "online_cpus": ncpu,
            "throttling_data": {
                "periods": 0,
                "throttled_periods": 0,
                "throttled_time": 0
            }
        });
        serde_json::from_value(serde_json::json!({
            "read": "2026-01-01T00:00:00Z",
            "preread": "2026-01-01T00:00:00Z",
            "num_procs": 1,
            "pids_stats": {},
            "memory_stats": { "usage": mem_bytes },
            "blkio_stats": {},
            "cpu_stats": cpu,
            "precpu_stats": {
                "cpu_usage": {
                    "total_usage": 0,
                    "usage_in_usermode": 0,
                    "usage_in_kernelmode": 0
                },
                "throttling_data": {
                    "periods": 0,
                    "throttled_periods": 0,
                    "throttled_time": 0
                }
            },
            "storage_stats": {}
        }))
        .unwrap()
    }

    #[test]
    fn test_peak_stats_tracks_max_cpu_and_ram() {
        let mut peaks = ContainerPeakStats::default();
        peaks.observe(&stats_sample(1_000_000, 100_000_000, 50 * 1024 * 1024, 4));
        assert_eq!(peaks.peak_ram_bytes, 50 * 1024 * 1024);
        assert_eq!(peaks.peak_cpu_percent, 0.0);
        peaks.observe(&stats_sample(1_500_000, 200_000_000, 30 * 1024 * 1024, 4));
        assert!((peaks.peak_cpu_percent - 2.0).abs() < 1e-9);
        assert_eq!(peaks.peak_ram_bytes, 50 * 1024 * 1024);
        peaks.observe(&stats_sample(1_500_000, 200_000_000, 90 * 1024 * 1024, 4));
        assert!((peaks.peak_cpu_percent - 2.0).abs() < 1e-9);
        assert_eq!(peaks.peak_ram_bytes, 90 * 1024 * 1024);
    }

    #[test]
    fn test_build_script() {
        let steps = vec![
            JobStep {
                name: "Build".into(),
                run: "cargo build --release".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
            JobStep {
                name: "Test".into(),
                run: "cargo test".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
        ];

        let script = build_script(&steps, &crate::ifexpr::IfContext::default());
        assert!(script.contains("GF_FAILED=0"));
        assert!(script.contains("=== Build ==="));
        assert!(script.contains("cargo build --release"));
        assert!(script.contains("=== Test ==="));
        assert!(script.contains("cargo test"));
        assert!(script.contains("exit $GF_FAIL_CODE"));
    }

    #[test]
    fn test_build_script_conditions() {
        use crate::job::RunCondition;
        let steps = vec![
            JobStep {
                name: "Main".into(),
                run: "do_work".into(),
                condition: RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
            JobStep {
                name: "Notify".into(),
                run: "send_alert".into(),
                condition: RunCondition::Failure,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
            JobStep {
                name: "Cleanup".into(),
                run: "rm_tmp".into(),
                condition: RunCondition::Always,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
        ];
        let script = build_script(&steps, &crate::ifexpr::IfContext::default());
        assert!(script.contains("if [ \"$GF_FAILED\" -eq 0 ]; then"));
        assert!(script.contains("if [ \"$GF_FAILED\" -ne 0 ]; then"));
        assert!(script.contains("skipping step Main (if: success())"));
        assert!(script.contains("skipping step Notify (if: failure())"));
        assert!(!script.contains("skipping step Cleanup"));
    }

    #[test]
    fn test_build_script_expr_guard_executes() {
        use crate::job::RunCondition;
        use std::process::Command;
        let steps = vec![
            JobStep {
                name: "Main".into(),
                run: "exit 1".into(),
                condition: RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
            JobStep {
                name: "Notify".into(),
                run: "echo NOTIFIED".into(),
                condition: RunCondition::Expr("failure() && inputs.env == 'prod'".to_string()),
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
            JobStep {
                name: "Gone".into(),
                run: "echo SHOULD_NOT_RUN".into(),
                condition: RunCondition::Expr("false".to_string()),
                continue_on_error: false,
                working_directory: None,
                id: None,
            },
        ];
        let ctx = crate::ifexpr::IfContext {
            inputs: std::collections::HashMap::from([("env".to_string(), "prod".to_string())]),
            ..Default::default()
        };
        let script = build_script(&steps, &ctx);
        assert!(script.contains("NOTIFIED"));
        assert!(!script.contains("SHOULD_NOT_RUN"));
        let path = std::env::temp_dir().join(format!("gf_if_{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, &script).unwrap();
        let out = Command::new("sh").arg(&path).output().unwrap();
        let _ = std::fs::remove_file(&path);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(stdout.contains("NOTIFIED"), "{stdout}");
        assert!(stdout.contains("skipping step Gone"), "{stdout}");
        assert!(!out.status.success());
    }

    #[test]
    fn test_build_script_steps_refs_execute() {
        use crate::job::RunCondition;
        use std::process::Command;
        let step = |name: &str, id: Option<&str>, run: &str, cond: RunCondition| JobStep {
            name: name.into(),
            run: run.into(),
            condition: cond,
            continue_on_error: false,
            working_directory: None,
            id: id.map(str::to_string),
        };
        let steps = vec![
            step(
                "Emit",
                Some("emit"),
                "echo \"SHA=abc123\" >> \"$GF_OUTPUTS\"",
                RunCondition::Success,
            ),
            step(
                "Match",
                None,
                "echo OUTPUT_MATCH",
                RunCondition::Expr("steps.emit.outputs.SHA == 'abc123'".to_string()),
            ),
            step(
                "Miss",
                None,
                "echo SHOULD_NOT_PRINT",
                RunCondition::Expr("steps.emit.outputs.SHA == 'zzz'".to_string()),
            ),
            step(
                "Outcome",
                None,
                "echo OUTCOME_MATCH",
                RunCondition::Expr("steps.emit.outcome == 'success'".to_string()),
            ),
        ];
        let script = build_script(&steps, &crate::ifexpr::IfContext::default());
        assert!(script.contains("gf_step_output()"), "{script}");
        assert!(script.contains("__GF_STEP__=emit"), "{script}");
        let dir = std::env::temp_dir().join(format!("gf_steps_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("run.sh");
        std::fs::write(&path, &script).unwrap();
        let out = Command::new("sh")
            .arg(&path)
            .env("GF_OUTPUTS", dir.join("outputs.env"))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(out.status.success(), "{stdout}");
        assert!(stdout.contains("OUTPUT_MATCH"), "{stdout}");
        assert!(!stdout.contains("SHOULD_NOT_PRINT"), "{stdout}");
        assert!(stdout.contains("OUTCOME_MATCH"), "{stdout}");
        assert!(stdout.contains("skipping step Miss"), "{stdout}");
    }

    #[test]
    fn test_continue_on_error_step_does_not_fail_script() {
        use crate::job::RunCondition;
        use std::process::Command;
        let run_script = |steps: Vec<JobStep>| {
            let script = build_script(&steps, &crate::ifexpr::IfContext::default());
            let path = std::env::temp_dir().join(format!("gf_coe_{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, &script).unwrap();
            let out = Command::new("sh").arg(&path).output().unwrap();
            let _ = std::fs::remove_file(&path);
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        let plain = |name: &str, run: &str| JobStep {
            name: name.into(),
            run: run.into(),
            condition: RunCondition::Success,
            continue_on_error: false,
            working_directory: None,
            id: None,
        };
        let forgiving = |name: &str, run: &str| JobStep {
            name: name.into(),
            run: run.into(),
            condition: RunCondition::Success,
            continue_on_error: true,
            working_directory: None,
            id: None,
        };
        let script = build_script(
            &[forgiving("x", "exit 1")],
            &crate::ifexpr::IfContext::default(),
        );
        assert!(script.contains("continue-on-error"));
        assert!(!script.contains("GF_FAILED=1"));
        let (code, log) = run_script(vec![
            forgiving("flaky", "echo before && exit 3"),
            plain("next", "echo next-ran"),
        ]);
        assert_eq!(code, 0);
        assert!(log.contains("before"));
        assert!(log.contains("next-ran"));
        assert!(log.contains("continue-on-error"));
        let (code, _) = run_script(vec![forgiving("flaky", "exit 3"), plain("hard", "exit 7")]);
        assert_eq!(code, 7);
    }

    #[test]
    fn test_conditional_script_executes() {
        use crate::job::RunCondition;
        use std::process::Command;
        let run_script = |steps: Vec<JobStep>| {
            let script = build_script(&steps, &crate::ifexpr::IfContext::default());
            let path = std::env::temp_dir().join(format!("gf_step_{}", uuid::Uuid::new_v4()));
            std::fs::write(&path, &script).unwrap();
            let out = Command::new("sh").arg(&path).output().unwrap();
            let _ = std::fs::remove_file(&path);
            (
                out.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };
        let step = |name: &str, run: &str, condition: RunCondition| JobStep {
            name: name.into(),
            run: run.into(),
            condition,
            continue_on_error: false,
            working_directory: None,
            id: None,
        };
        let (code, log) = run_script(vec![
            step("a", "echo hello", RunCondition::Success),
            step("b", "echo world", RunCondition::Success),
        ]);
        assert_eq!(code, 0);
        assert!(log.contains("hello") && log.contains("world"));
        let (code, log) = run_script(vec![
            step("a", "echo before && exit 3", RunCondition::Success),
            step("b", "echo skipped", RunCondition::Success),
            step("c", "echo rescued", RunCondition::Failure),
            step("d", "echo always", RunCondition::Always),
        ]);
        assert_eq!(code, 3);
        assert!(log.contains("before"));
        assert!(!log.contains("skipped\n"));
        assert!(log.contains("rescued"));
        assert!(log.contains("always"));
        assert!(log.contains("skipping step b (if: success())"));
        let (code, log) = run_script(vec![
            step("a", "echo ok", RunCondition::Success),
            step("c", "echo no_rescue", RunCondition::Failure),
        ]);
        assert_eq!(code, 0);
        assert!(!log.contains("no_rescue"));
        assert!(log.contains("skipping step c (if: failure())"));
        let (code, log) = run_script(vec![
            step("a", "echo ok", RunCondition::Success),
            step("z", "echo never_runs", RunCondition::Cancelled),
        ]);
        assert_eq!(code, 0);
        assert!(!log.contains("never_runs"));
        assert!(log.contains("skipping step z (if: cancelled())"));
    }

    #[test]
    fn test_working_directory_runs_step_in_subdir() {
        use crate::job::RunCondition;
        use std::process::Command;
        let root = std::env::temp_dir().join(format!("gf_wd_{}", uuid::Uuid::new_v4()));
        let sub = root.join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let steps = vec![
            JobStep {
                name: "here".into(),
                run: "pwd > seen.txt".into(),
                condition: RunCondition::Success,
                continue_on_error: false,
                working_directory: Some("sub".into()),
                id: None,
            },
            JobStep {
                name: "quoted".into(),
                run: "true".into(),
                condition: RunCondition::Success,
                continue_on_error: true,
                working_directory: Some("a'b".into()),
                id: None,
            },
        ];
        let script = build_script(&steps, &crate::ifexpr::IfContext::default());
        assert!(script.contains("(cd 'sub' && set -e"));
        assert!(script.contains("(cd 'a'\\''b' && set -e"));
        let path = std::env::temp_dir().join(format!("gf_step_{}", uuid::Uuid::new_v4()));
        std::fs::write(&path, &script).unwrap();
        let out = Command::new("sh")
            .arg(&path)
            .current_dir(&root)
            .output()
            .unwrap();
        let _ = std::fs::remove_file(&path);
        assert!(out.status.success());
        assert!(!root.join("seen.txt").exists());
        let seen = std::fs::read_to_string(sub.join("seen.txt")).unwrap();
        assert!(seen.trim().ends_with("sub"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn test_image_allowlist() {
        let empty: Vec<String> = vec![];
        assert!(image_allowed("anything:1", &empty));
        let allowed = vec!["rust:1.75".to_string(), "alpine:3".to_string()];
        assert!(image_allowed("rust:1.75", &allowed));
        assert!(!image_allowed("rust:1.76", &allowed));
        assert!(!image_allowed("evil:latest", &allowed));
        assert!(!image_allowed("rust:1.75-malicious", &allowed));
    }

    #[test]
    fn test_artifact_path_sanitizer() {
        assert!(sanitize_artifact_path("target/release/app").is_some());
        assert!(sanitize_artifact_path("out.txt").is_some());
        assert!(sanitize_artifact_path("/etc/passwd").is_none());
        assert!(sanitize_artifact_path("../escape").is_none());
        assert!(sanitize_artifact_path("a/../../escape").is_none());
    }

    #[test]
    fn test_effective_limits_clamp_to_caps() {
        let caps = ResourceCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            timeout_secs: 3600,
            disk_mb: 10240,
        };
        let greedy = ComputeRequirements {
            min_cpu_cores: 1024,
            min_ram_mb: 1 << 40,
            min_disk_mb: 0,
            gpu_required: false,
            network_access: false,
        };
        assert_eq!(effective_limits(&greedy, u64::MAX, &caps), (4, 8192, 3600));
        let modest = ComputeRequirements {
            min_cpu_cores: 2,
            min_ram_mb: 1024,
            min_disk_mb: 0,
            gpu_required: false,
            network_access: false,
        };
        assert_eq!(effective_limits(&modest, 60, &caps), (2, 1024, 60));
        let zero = ComputeRequirements::default();
        let zero_caps = ResourceCaps {
            cpu_cores: 0,
            ram_mb: 0,
            timeout_secs: 0,
            disk_mb: 0,
        };
        assert_eq!(effective_limits(&zero, 0, &zero_caps), (1, 64, 1));
    }

    #[test]
    fn test_dir_size_counts_files_not_symlinks() {
        let dir = std::env::temp_dir().join(format!("gf_dirsize_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), vec![0u8; 100]).unwrap();
        std::fs::write(dir.join("sub").join("b.txt"), vec![0u8; 50]).unwrap();
        assert_eq!(dir_size_bytes(&dir), 150);
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("sub"), dir.join("loop")).unwrap();
            std::os::unix::fs::symlink(dir.join("a.txt"), dir.join("alias")).unwrap();
            assert_eq!(dir_size_bytes(&dir), 150);
        }
        assert_eq!(dir_size_bytes(&dir.join("missing")), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_enforce_disk_quota() {
        let dir = std::env::temp_dir().join(format!("gf_quota_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("f.bin"), vec![0u8; 3 * 1024 * 1024]).unwrap();
        assert!(enforce_disk_quota(&dir, 0).is_ok());
        assert!(enforce_disk_quota(&dir, 3).is_ok());
        let err = enforce_disk_quota(&dir, 1).unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_sanitize_container_name() {
        assert_eq!(sanitize_container_name("abc123"), "gitforest-job-abc123");
        assert_eq!(
            sanitize_container_name("0123456789abcdef"),
            "gitforest-job-01234567"
        );
        assert_eq!(sanitize_container_name("../../x"), "gitforest-job-..-..-x");
        assert_eq!(sanitize_container_name(""), "gitforest-job-");
    }

    #[tokio::test]
    async fn test_docker_run_echo() {
        let tmp = std::env::temp_dir().join(format!("gf_dock_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("input.txt"), "docker-data\n").unwrap();
        let runner = match JobRunner::new(tmp.join("work")) {
            Ok(r) => r,
            Err(_) => return,
        };
        if !runner.check_docker().await {
            return;
        }
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let config = crate::job::JobConfig {
            name: "docker-echo".into(),
            steps: vec![crate::job::JobStep {
                name: "echo".into(),
                run: "cat input.txt && echo built > out.txt".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "alpine:3".into(),
            timeout_secs: 300,
            env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            artifacts: vec!["out.txt".into()],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        let result = runner.run_job(&job, &repo, &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.verify());
        assert!(result.stdout_tail.contains("docker-data"));
        assert_eq!(result.artifact_hashes.len(), 1);
        assert!(!repo.join("out.txt").exists());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_docker_run_failure_exit() {
        let tmp = std::env::temp_dir().join(format!("gf_dockf_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let runner = match JobRunner::new(tmp.join("work")) {
            Ok(r) => r,
            Err(_) => return,
        };
        if !runner.check_docker().await {
            return;
        }
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let config = crate::job::JobConfig {
            name: "docker-fail".into(),
            steps: vec![crate::job::JobStep {
                name: "fail".into(),
                run: "exit 3".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "alpine:3".into(),
            timeout_secs: 300,
            env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        let result = runner.run_job(&job, &repo, &worker).await.unwrap();
        assert_eq!(result.exit_code, 3);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_services_start_gate_and_teardown() {
        let tmp = std::env::temp_dir().join(format!("gf_svc_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let runner = match JobRunner::new(tmp.join("work")) {
            Ok(r) => r,
            Err(_) => return,
        };
        if !runner.check_docker().await {
            return;
        }
        let services = vec![crate::job::JobService {
            name: "cache".into(),
            image: "nginx:alpine".into(),
            env: std::collections::HashMap::new(),
            ports: vec![],
            health: Some(crate::job::ServiceHealth {
                cmd: "true".into(),
                interval_secs: 1,
                timeout_secs: 1,
                retries: 3,
            }),
        }];
        let job_id = format!("test-svc-{}", std::process::id());
        let set = start_job_services(&runner.docker, &job_id, &services, true)
            .await
            .unwrap();
        assert_eq!(set.network, service_network_name(&job_id));
        assert!(runner
            .docker
            .inspect_network::<String>(&set.network, None)
            .await
            .is_ok());
        let network = set.network.clone();
        set.teardown(&runner.docker).await;
        assert!(runner
            .docker
            .inspect_network::<String>(&network, None)
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_docker_run_container_env_volume() {
        let tmp = std::env::temp_dir().join(format!("gf_ctnr_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let runner = match JobRunner::new(tmp.join("work")) {
            Ok(r) => r,
            Err(_) => return,
        };
        if !runner.check_docker().await {
            return;
        }
        let volumes_before: std::collections::HashSet<String> = runner
            .docker
            .list_volumes::<String>(None)
            .await
            .map(|v| {
                v.volumes
                    .unwrap_or_default()
                    .into_iter()
                    .map(|vol| vol.name)
                    .collect()
            })
            .unwrap_or_default();
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let config = crate::job::JobConfig {
            name: "docker-ctnr".into(),
            steps: vec![crate::job::JobStep {
                name: "prove".into(),
                run: "echo \"env=$CONTAINER_ENV_OK\" && echo volume-marker > /data/scratch/marker.txt && cat /data/scratch/marker.txt".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "alpine:3".into(),
            timeout_secs: 300,
            env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: Some(crate::job::JobContainer {
                image: "alpine:3".into(),
                credentials: None,
                env: std::collections::HashMap::from([(
                    "CONTAINER_ENV_OK".to_string(),
                    "container-env-ok".to_string(),
                )]),
                ports: vec![],
                volumes: vec![crate::job::JobVolume {
                    source: None,
                    target: "/data/scratch".into(),
                    read_only: false,
                }],
                resources: crate::job::ContainerResources {
                    nano_cpus: Some(1_000_000_000),
                    memory_bytes: Some(256 * 1024 * 1024),
                    shm_bytes: Some(64 * 1024 * 1024),
                },
                options: "--cpus 1 --memory 256m --shm-size 64m".into(),
            }),
            job_token: String::new(),
        };
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        let result = runner.run_job(&job, &repo, &worker).await.unwrap();
        assert_eq!(result.exit_code, 0, "stderr: {}", result.stderr_tail);
        assert!(
            result.stdout_tail.contains("env=container-env-ok"),
            "{}",
            result.stdout_tail
        );
        assert!(
            result.stdout_tail.contains("volume-marker"),
            "{}",
            result.stdout_tail
        );
        let volumes_after: std::collections::HashSet<String> = runner
            .docker
            .list_volumes::<String>(None)
            .await
            .map(|v| {
                v.volumes
                    .unwrap_or_default()
                    .into_iter()
                    .map(|vol| vol.name)
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            volumes_after.difference(&volumes_before).next().is_none(),
            "anonymous volumes leaked"
        );
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn test_docker_run_resolves_service_hostname() {
        let tmp = std::env::temp_dir().join(format!("gf_svce_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        let runner = match JobRunner::new(tmp.join("work")) {
            Ok(r) => r,
            Err(_) => return,
        };
        if !runner.check_docker().await {
            return;
        }
        let submitter = Identity::generate();
        let worker = Identity::generate();
        let config = crate::job::JobConfig {
            name: "docker-svc".into(),
            steps: vec![crate::job::JobStep {
                name: "resolve".into(),
                run: "getent hosts cache".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "alpine:3".into(),
            timeout_secs: 300,
            env: std::collections::HashMap::new(),
            secrets: std::collections::HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![crate::job::JobService {
                name: "cache".into(),
                image: "nginx:alpine".into(),
                env: std::collections::HashMap::new(),
                ports: vec![],
                health: Some(crate::job::ServiceHealth {
                    cmd: "true".into(),
                    interval_secs: 1,
                    timeout_secs: 1,
                    retries: 3,
                }),
            }],
            container: None,
            job_token: String::new(),
        };
        let job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        let network = service_network_name(&job.id);
        let result = runner.run_job(&job, &repo, &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(runner
            .docker
            .inspect_network::<String>(&network, None)
            .await
            .is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn test_valid_env_key() {
        assert!(valid_env_key("PATH"));
        assert!(valid_env_key("_FOO_1"));
        assert!(!valid_env_key("1BAD"));
        assert!(!valid_env_key("HAS space"));
        assert!(!valid_env_key("HAS=EQUALS"));
        assert!(!valid_env_key(""));
        assert!(!valid_env_key("semi;colon"));
    }

    #[test]
    fn test_redact_secrets() {
        use std::collections::HashMap;
        let secrets = HashMap::from([
            ("TOKEN".to_string(), "hunter2-hunter2".to_string()),
            ("SHORT".to_string(), "abc".to_string()),
            ("EMPTY".to_string(), "".to_string()),
        ]);
        let mut data = b"login hunter2-hunter2 ok hunter2-hunter2 abc!".to_vec();
        redact_secrets(&mut data, &secrets);
        assert_eq!(data, b"login *** ok *** ***!".to_vec());
        let mut binary = vec![0xff, 0xfe, b'a', b'b', b'c', 0x00];
        redact_secrets(&mut binary, &secrets);
        assert_eq!(binary, vec![0xff, 0xfe, b'*', b'*', b'*', 0x00]);
        let mut overlapping = b"xxhunter2-hunter2yy".to_vec();
        let overlap = HashMap::from([
            ("A".to_string(), "hunter2".to_string()),
            ("B".to_string(), "hunter2-hunter2".to_string()),
        ]);
        redact_secrets(&mut overlapping, &overlap);
        assert_eq!(overlapping, b"xx***yy".to_vec());
        let mut clean = b"nothing to hide".to_vec();
        redact_secrets(&mut clean, &HashMap::new());
        assert_eq!(clean, b"nothing to hide".to_vec());
    }

    #[test]
    fn test_parse_github_outputs() {
        use std::collections::HashMap;
        let data = b"SHA=abc123\nEMPTY=\n1BAD=no\nno-equals-here\nMULTI<<EOF\nline one\nline two\nEOF\nAFTER=yes\n";
        let parsed = parse_github_outputs(data);
        assert_eq!(parsed.get("SHA").unwrap(), "abc123");
        assert_eq!(parsed.get("EMPTY").unwrap(), "");
        assert!(!parsed.contains_key("1BAD"));
        assert_eq!(parsed.get("MULTI").unwrap(), "line one\nline two");
        assert_eq!(parsed.get("AFTER").unwrap(), "yes");
        let crlf = b"KEY=value\r\nOTHER<<DONE\r\na\r\nDONE\r\n";
        let parsed = parse_github_outputs(crlf);
        assert_eq!(parsed.get("KEY").unwrap(), "value");
        assert_eq!(parsed.get("OTHER").unwrap(), "a");
        let dupes = b"KEY=first\nKEY=second\n";
        assert_eq!(parse_github_outputs(dupes).get("KEY").unwrap(), "second");
        let marked = b"PRE=0\n__GF_STEP__=build\nSHA=abc\n__GF_STEP__=test\nOK=1\n";
        let (union, per_step) = parse_github_outputs_with_steps(marked);
        assert!(!union.contains_key("__GF_STEP__"));
        assert_eq!(union.get("SHA").unwrap(), "abc");
        assert_eq!(union.get("PRE").unwrap(), "0");
        assert_eq!(per_step["build"].get("SHA").unwrap(), "abc");
        assert!(!per_step["build"].contains_key("PRE"));
        assert_eq!(per_step["test"].get("OK").unwrap(), "1");
        assert!(!per_step["test"].contains_key("SHA"));
        let ctx = crate::ifexpr::IfContext::default();
        let mapping = HashMap::from([(
            "digest".to_string(),
            "${{ steps.build.outputs.SHA }}".to_string(),
        )]);
        let resolved = resolve_job_outputs(&mapping, &per_step, &ctx);
        assert_eq!(resolved.get("digest").unwrap(), "abc");
        assert_eq!(resolved.len(), 1);
        let unterminated = b"KEY<<EOF\npartial";
        assert_eq!(
            parse_github_outputs(unterminated).get("KEY").unwrap(),
            "partial"
        );
        let mut many = Vec::new();
        for i in 0..200 {
            many.extend_from_slice(format!("K{i:03}=v\n").as_bytes());
        }
        assert_eq!(parse_github_outputs(&many).len(), MAX_OUTPUT_KEYS);
        let big_value = format!("BIG={}\n", "x".repeat(MAX_OUTPUT_VALUE_BYTES + 10));
        assert_eq!(
            parse_github_outputs(big_value.as_bytes())
                .get("BIG")
                .unwrap()
                .len(),
            MAX_OUTPUT_VALUE_BYTES
        );
        let scrubbed_in =
            HashMap::from([("A".to_string(), "token-hunter2 and hunter2".to_string())]);
        let mut outputs = scrubbed_in;
        let secrets = HashMap::from([("T".to_string(), "hunter2".to_string())]);
        scrub_output_values(&mut outputs, &secrets);
        assert_eq!(outputs.get("A").unwrap(), "token-*** and ***");
    }

    #[tokio::test]
    async fn test_capped_reads() {
        let tmp = std::env::temp_dir().join(format!("gf_cap_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let big = tmp.join("big.log");
        let data: Vec<u8> = (0..100_000).map(|i| (i % 251) as u8).collect();
        std::fs::write(&big, &data).unwrap();
        let tail = read_capped_tail(&big, 1000).await;
        assert_eq!(tail, data[data.len() - 1000..]);
        let small = tmp.join("small.log");
        std::fs::write(&small, b"hi").unwrap();
        assert_eq!(read_capped_tail(&small, 1000).await, b"hi");
        assert_eq!(read_capped_exact(&small, 1000).await.unwrap(), b"hi");
        assert!(read_capped_exact(&big, 1000).await.is_none());
        assert!(read_capped_exact(&tmp.join("missing"), 1000)
            .await
            .is_none());
        assert!(read_capped_tail(&tmp.join("missing"), 1000)
            .await
            .is_empty());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
