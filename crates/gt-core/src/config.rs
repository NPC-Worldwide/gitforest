use crate::identity::PeerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use thiserror::Error;

#[derive(Error, Debug)]
pub enum ConfigError {
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
    #[error("parse error: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("serialize error: {0}")]
    Serialize(#[from] toml::ser::Error),
}

/// Top-level daemon configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// Directory where repos and state are stored.
    pub data_dir: PathBuf,

    /// Port for the P2P network.
    pub p2p_port: u16,

    /// Port for the local HTTP API (web UI).
    pub api_port: u16,

    /// Whether to auto-seed repos after cloning.
    pub auto_seed: bool,

    /// Maximum upload rate in bytes/sec (0 = unlimited).
    pub max_upload_rate: u64,

    /// Maximum download rate in bytes/sec (0 = unlimited).
    pub max_download_rate: u64,

    /// Repos we're hosting/seeding.
    #[serde(default)]
    pub repos: Vec<RepoConfig>,

    /// Bootstrap peers to connect to on startup (multiaddr format).
    /// Example: ["/ip4/192.168.1.50/tcp/9418", "/ip4/203.0.113.5/tcp/9418"]
    /// Fresh installs are seeded with [`default_bootstrap_peers`].
    #[serde(default)]
    pub bootstrap_peers: Vec<String>,

    #[serde(default)]
    pub advertise_addr: Option<String>,

    #[serde(default)]
    pub cluster_token: Option<String>,

    /// Compute marketplace configuration.
    pub compute: Option<ComputeConfig>,

    /// Payment settlement backends (empty = mutual credit / free mode only).
    #[serde(default)]
    pub payments: PaymentsConfig,

    /// Peer name aliases: maps short tag (e.g., "laptop#cd26") to full hex peer ID.
    #[serde(default)]
    pub peer_names: HashMap<String, String>,

    /// ACL grants per repo (repo_name -> list of grants).
    /// Only stores grants for repos we own. Owner is implicit.
    #[serde(default)]
    pub acl_grants: HashMap<String, Vec<AclGrantConfig>>,

    /// Model ACL grants per manifest id. Presence of a key restricts the
    /// model to the owner plus these peers; absence leaves it open.
    #[serde(default)]
    pub model_acl_grants: HashMap<String, Vec<PeerId>>,

    /// LLM inference backend binary (llama.cpp style CLI).
    /// Overridden by the GF_LLM_BACKEND environment variable.
    #[serde(default)]
    pub llm_backend: Option<String>,

    /// IP address the P2P transport listens on. Defaults to 0.0.0.0.
    /// Set to 127.0.0.1 for localhost-only nodes and test clusters.
    #[serde(default)]
    pub p2p_listen: Option<String>,

    /// Local-network peer discovery via mDNS. Disable for isolated test
    /// clusters so demo nodes never leak onto (or learn from) the live mesh.
    #[serde(default = "default_enable_mdns")]
    pub enable_mdns: bool,
}

fn default_enable_mdns() -> bool {
    true
}

/// Serializable ACL grant for config storage.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AclGrantConfig {
    pub peer_id: PeerId,
    pub ref_pattern: String,
}

/// Configuration for the compute marketplace.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeConfig {
    /// Enable compute worker mode (offer resources to network).
    pub worker_enabled: bool,

    /// CPU cores to offer for compute jobs.
    pub max_cpu_cores: u32,

    /// RAM in MB to offer.
    pub max_ram_mb: u64,

    /// Disk in MB to offer.
    pub max_disk_mb: u64,

    /// Docker socket path.
    pub docker_socket: String,

    /// Hourly rate in satoshis per CPU-hour.
    pub hourly_rate_sats: u64,

    /// Accepted payment chains (e.g., "lightning", "arbitrum", "base").
    pub accepted_chains: Vec<String>,

    /// Wallet addresses per chain.
    pub wallets: HashMap<String, String>,

    /// Maximum concurrent jobs to run.
    pub max_concurrent_jobs: u32,

    /// Allowed Docker images (empty = allow all).
    pub allowed_images: Vec<String>,

    /// Maximum seconds a single job may run.
    #[serde(default = "default_job_timeout")]
    pub max_job_timeout_secs: u64,

    /// Whether jobs requesting network access may run here.
    #[serde(default)]
    pub allow_network_jobs: bool,

    /// Whether this worker advertises a GPU for jobs with `gpu: true`.
    #[serde(default)]
    pub gpu_enabled: bool,

    /// Probe `nvidia-smi -L` at daemon startup and advertise a GPU when
    /// one is found. On by default; set false to opt out on GPU machines.
    #[serde(default = "default_gpu_auto_detect")]
    pub gpu_auto_detect: bool,

    /// Runtime-only result of the startup GPU probe. Never serialized.
    #[serde(skip, default)]
    pub gpu_detected: bool,

    /// How jobs execute: sandboxed Docker containers or direct local shell.
    #[serde(default)]
    pub execution_mode: ExecutionMode,

    /// Force deterministic replay verification on every submitted job,
    /// even when the submit request sets `verify: false`.
    #[serde(default)]
    pub require_verification: bool,

    /// Minimum PoW difficulty (leading zero bits) accepted on incoming
    /// worker offers. Floored at the genesis difficulty: lower values
    /// behave as the floor. Raise to stop matching laggard workers.
    #[serde(default = "default_pow_bits")]
    pub min_pow_bits: u8,

    /// PoW difficulty this worker mines when announcing offers. Raised
    /// network-wide via releases; higher-proof offers are accepted by
    /// older verifiers since their check is a lower bound.
    #[serde(default = "default_pow_bits")]
    pub pow_target_bits: u8,

    /// Mutual-credit free allowance in compute-seconds. A peer whose net
    /// consumption from us exceeds this is refused mutual-credit work,
    /// and we stop submitting mutual-credit jobs past our own deficit.
    /// Jobs covered by a money backend are unaffected. Set very high to
    /// approximate unconditional free mode.
    #[serde(default = "default_credit_allowance")]
    pub credit_allowance_secs: u64,

    /// Environment names that need manual approval before a job runs.
    /// Jobs targeting one of these wait for `POST
    /// /api/compute/jobs/{id}/approve`; anything else runs immediately.
    #[serde(default)]
    pub protected_environments: Vec<String>,
}

fn default_credit_allowance() -> u64 {
    3600
}

fn default_gpu_auto_detect() -> bool {
    true
}

fn default_pow_bits() -> u8 {
    crate::identity::MIN_POW_BITS
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionMode {
    #[default]
    Docker,
    Local,
}

fn default_job_timeout() -> u64 {
    3600
}

/// Payment settlement backend configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PaymentsConfig {
    #[serde(default)]
    pub lnd: Option<LndDaemonConfig>,
    #[serde(default)]
    pub evm: Vec<EvmDaemonConfig>,
}

/// Lightning settlement via an external LND node's gRPC API.
/// LND's hold-invoice API is gRPC-only; the node's TLS certificate is
/// pinned (no skip-verify mode) and the admin macaroon authenticates calls.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LndDaemonConfig {
    pub grpc_url: String,
    pub macaroon_path: PathBuf,
    pub tls_cert_path: PathBuf,
}

/// EVM L2 settlement via JSON-RPC plus either an external signer process
/// (`signer_command`) or the embedded keystore signer (`keystore_path`).
/// When both are set the keystore wins.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvmDaemonConfig {
    pub network: String,
    pub rpc_url: String,
    pub from_address: String,
    #[serde(default)]
    pub signer_command: String,
    #[serde(default)]
    pub keystore_path: Option<PathBuf>,
    #[serde(default)]
    pub key_password_file: Option<PathBuf>,
}

impl Default for ComputeConfig {
    fn default() -> Self {
        ComputeConfig {
            worker_enabled: false,
            max_cpu_cores: 2,
            max_ram_mb: 4096,
            max_disk_mb: 10240,
            docker_socket: "/var/run/docker.sock".into(),
            hourly_rate_sats: 1000,
            accepted_chains: vec!["lightning".into()],
            wallets: HashMap::new(),
            max_concurrent_jobs: 2,
            allowed_images: vec![],
            max_job_timeout_secs: default_job_timeout(),
            allow_network_jobs: false,
            gpu_enabled: false,
            gpu_auto_detect: true,
            gpu_detected: false,
            execution_mode: ExecutionMode::default(),
            require_verification: false,
            min_pow_bits: default_pow_bits(),
            pow_target_bits: default_pow_bits(),
            credit_allowance_secs: default_credit_allowance(),
            protected_environments: Vec::new(),
        }
    }
}

impl ComputeConfig {
    pub fn effective_gpu(&self) -> bool {
        self.gpu_enabled || (self.gpu_auto_detect && self.gpu_detected)
    }
}

/// Configuration for a single hosted repository.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    /// Local name for this repo.
    pub name: String,

    /// Path to the bare git repo on disk.
    pub path: PathBuf,

    /// Owner's peer ID (our own for repos we created).
    pub owner: PeerId,

    /// Optional description.
    pub description: Option<String>,

    /// Whether we're actively seeding this repo.
    pub seeding: bool,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            data_dir: default_data_dir(),
            p2p_port: 9418, // same as git protocol port
            api_port: 7878,
            auto_seed: true,
            max_upload_rate: 0,
            max_download_rate: 0,
            repos: Vec::new(),
            bootstrap_peers: Vec::new(),
            advertise_addr: None,
            cluster_token: None,
            compute: None,
            payments: PaymentsConfig::default(),
            peer_names: HashMap::new(),
            acl_grants: HashMap::new(),
            model_acl_grants: HashMap::new(),
            llm_backend: None,
            p2p_listen: None,
            enable_mdns: true,
        }
    }
}

impl Config {
    /// Port the git protocol server binds (always adjacent to the P2P port).
    pub fn git_port(&self) -> Option<u16> {
        self.p2p_port.checked_add(1)
    }

    /// Fail fast on port assignments that would collide at bind time
    /// (including across daemons sharing a host) instead of dying later
    /// with a misleading bind error.
    pub fn validate_ports(&self) -> Result<(), String> {
        let Some(git_port) = self.git_port() else {
            return Err(format!(
                "p2p_port {} has no adjacent git port (u16 overflow)",
                self.p2p_port
            ));
        };
        if self.api_port == self.p2p_port {
            return Err(format!("api_port {} collides with p2p_port", self.api_port));
        }
        if self.api_port == git_port {
            return Err(format!(
                "api_port {} collides with git protocol port {} (p2p_port {} + 1); space p2p_port values by 2 when running multiple daemons",
                self.api_port, git_port, self.p2p_port
            ));
        }
        Ok(())
    }

    /// Load config from a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let contents = std::fs::read_to_string(path)?;
        let config: Config = toml::from_str(&contents)?;
        Ok(config)
    }

    /// Save config to a TOML file.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string_pretty(self)?;
        std::fs::write(path, contents)?;
        Ok(())
    }

    /// Resolve a peer identifier to a full PeerId.
    ///
    /// Resolution priority:
    /// 1. If it's a 64-char hex string, parse directly as PeerId
    /// 2. If it matches a key in `peer_names`, return the stored PeerId
    /// 3. If it's `name#disc` format, search `peer_names` for a matching entry
    pub fn resolve_peer(&self, identifier: &str) -> Option<PeerId> {
        // 1. Try as full 64-char hex peer ID
        if identifier.len() == 64 && identifier.chars().all(|c| c.is_ascii_hexdigit()) {
            return identifier.parse().ok();
        }

        // 2. Exact match in peer_names map
        if let Some(hex_id) = self.peer_names.get(identifier) {
            return hex_id.parse().ok();
        }

        // 3. If name#disc format, search peer_names for matching entry
        if identifier.contains('#') {
            for (tag, hex_id) in &self.peer_names {
                if tag == identifier {
                    return hex_id.parse().ok();
                }
            }
            // Also check if the discriminator matches any stored peer ID
            if let Some((_name, disc)) = identifier.rsplit_once('#') {
                for hex_id in self.peer_names.values() {
                    if hex_id.starts_with(disc) {
                        return hex_id.parse().ok();
                    }
                }
            }
        }

        None
    }

    /// Load config from default path, or create default if it doesn't exist.
    pub fn load_or_default() -> Result<Self, ConfigError> {
        let path = default_config_path();
        if path.exists() {
            Self::load(&path)
        } else {
            let config = Config::fresh_install();
            config.save(&path)?;
            Ok(config)
        }
    }

    /// Config for a brand-new install: defaults plus the public
    /// bootstrap peers so a fresh download joins the network
    /// immediately. Kept separate from `Default` so tests and
    /// explicitly offline nodes stay hermetic.
    pub fn fresh_install() -> Self {
        Config {
            bootstrap_peers: default_bootstrap_peers(),
            ..Default::default()
        }
    }

    pub fn enable_worker(
        &mut self,
        cpu_cores: Option<u32>,
        ram_mb: Option<u64>,
        hourly_rate_sats: Option<u64>,
    ) -> &ComputeConfig {
        let compute = self.compute.get_or_insert_with(ComputeConfig::default);
        compute.worker_enabled = true;
        if let Some(cpu) = cpu_cores {
            compute.max_cpu_cores = cpu;
        }
        if let Some(ram) = ram_mb {
            compute.max_ram_mb = ram;
        }
        if let Some(rate) = hourly_rate_sats {
            compute.hourly_rate_sats = rate;
        }
        compute
    }

    pub fn disable_worker(&mut self) -> bool {
        match self.compute.as_mut() {
            Some(compute) => {
                compute.worker_enabled = false;
                true
            }
            None => false,
        }
    }

    pub fn worker_enabled(&self) -> bool {
        self.compute
            .as_ref()
            .map(|c| c.worker_enabled)
            .unwrap_or(false)
    }
}

/// Default config file path.
pub fn default_config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gitforest")
        .join("config.toml")
}

/// Default data directory.
pub fn default_data_dir() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("gitforest")
}

/// Public bootstrap nodes (static IPs, us-central1). The `/api/`
/// suffix tells the seeder where each node's HTTP API lives; the
/// `/p2p/` peer ID pins identity once known.
pub const BOOTSTRAP_NODES: &[(&str, Option<&str>)] = &[
    (
        "35.184.171.148",
        Some("12D3KooWGFVp8agDRPk3fXqYgtq696dUDQM3VPQ9VZSQYhcy36AV"),
    ),
    (
        "34.69.48.37",
        Some("12D3KooWKVYdaERYxwL12wDtU1KDQ4Heh23Kcpk3AjnNuPQyNptC"),
    ),
    (
        "136.114.20.123",
        Some("12D3KooWCGLiQRkhM3wtBLEoAu87tCtbzcKbDZm32kckWtQXLdXU"),
    ),
];

/// Default bootstrap peers for fresh installs.
pub fn default_bootstrap_peers() -> Vec<String> {
    BOOTSTRAP_NODES
        .iter()
        .map(|(ip, peer_id)| match peer_id {
            Some(id) => format!("/ip4/{ip}/tcp/9418/p2p/{id}/api/7878"),
            None => format!("/ip4/{ip}/tcp/9418/api/7878"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_roundtrip() {
        let config = Config::default();
        let serialized = toml::to_string_pretty(&config).unwrap();
        let deserialized: Config = toml::from_str(&serialized).unwrap();
        assert_eq!(config.p2p_port, deserialized.p2p_port);
        assert_eq!(config.api_port, deserialized.api_port);
    }

    #[test]
    fn test_enable_worker_creates_compute_section() {
        let mut config = Config::default();
        assert!(config.compute.is_none());
        assert!(!config.worker_enabled());
        let compute = config.enable_worker(Some(4), Some(8192), Some(1000));
        assert!(compute.worker_enabled);
        assert_eq!(compute.max_cpu_cores, 4);
        assert_eq!(compute.max_ram_mb, 8192);
        assert_eq!(compute.hourly_rate_sats, 1000);
        assert!(config.worker_enabled());
    }

    #[test]
    fn test_enable_worker_keeps_existing_values_without_overrides() {
        let mut config = Config::default();
        config.enable_worker(Some(8), Some(16384), Some(2000));
        let compute = config.enable_worker(None, None, None);
        assert!(compute.worker_enabled);
        assert_eq!(compute.max_cpu_cores, 8);
        assert_eq!(compute.max_ram_mb, 16384);
        assert_eq!(compute.hourly_rate_sats, 2000);
    }

    #[test]
    fn test_old_config_without_payments_section_loads() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(config.payments.lnd.is_none());
        assert!(config.payments.evm.is_empty());
    }

    #[test]
    fn test_old_compute_section_without_pow_keys_defaults() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 10240
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let compute = config.compute.expect("compute section loads");
        assert_eq!(compute.min_pow_bits, crate::identity::MIN_POW_BITS);
        assert_eq!(compute.pow_target_bits, crate::identity::MIN_POW_BITS);
    }

    #[test]
    fn test_protected_environments_default_empty_and_parse() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 10240
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
protected_environments = ["prod", "prod-eu"]
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let compute = config.compute.expect("compute section loads");
        assert_eq!(
            compute.protected_environments,
            vec!["prod".to_string(), "prod-eu".to_string()]
        );
        assert!(ComputeConfig::default().protected_environments.is_empty());
    }

    #[test]
    fn test_gpu_enabled_defaults_off_and_parses() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 10240
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        assert!(!config.compute.unwrap().gpu_enabled);
        let enabled = format!("{toml_str}gpu_enabled = true\n");
        let config: Config = toml::from_str(&enabled).unwrap();
        assert!(config.compute.unwrap().gpu_enabled);
        assert!(!ComputeConfig::default().gpu_enabled);
    }

    #[test]
    fn test_gpu_auto_detect_defaults_on_and_gates_effective() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 10240
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
wallets = {}
max_concurrent_jobs = 2
allowed_images = []
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let compute = config.compute.unwrap();
        assert!(compute.gpu_auto_detect);
        assert!(!compute.gpu_detected);
        assert!(!compute.effective_gpu());
        let off = format!("{toml_str}gpu_auto_detect = false\n");
        let config: Config = toml::from_str(&off).unwrap();
        assert!(!config.compute.unwrap().gpu_auto_detect);
        let mut manual = ComputeConfig {
            gpu_enabled: true,
            ..ComputeConfig::default()
        };
        assert!(manual.gpu_auto_detect);
        assert!(manual.effective_gpu());
        manual.gpu_enabled = false;
        assert!(!manual.effective_gpu());
        let mut probed = ComputeConfig {
            gpu_detected: true,
            ..ComputeConfig::default()
        };
        assert!(probed.effective_gpu());
        probed.gpu_auto_detect = false;
        assert!(!probed.effective_gpu());
        probed.gpu_enabled = true;
        assert!(probed.effective_gpu());
        let serialized = toml::to_string(&probed).unwrap();
        assert!(!serialized.contains("gpu_detected"), "{serialized}");
        let roundtrip: ComputeConfig = toml::from_str(&serialized).unwrap();
        assert!(!roundtrip.gpu_detected);
    }

    #[test]
    fn test_payments_section_roundtrips() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[payments.lnd]
grpc_url = "https://127.0.0.1:10009"
macaroon_path = "/tmp/admin.macaroon"
tls_cert_path = "/tmp/tls.cert"
[[payments.evm]]
network = "base"
rpc_url = "http://127.0.0.1:8545"
from_address = "0x1111111111111111111111111111111111111111"
signer_command = "cast wallet sign --private-key $KEY"
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let lnd = config.payments.lnd.unwrap();
        assert_eq!(lnd.grpc_url, "https://127.0.0.1:10009");
        assert_eq!(lnd.tls_cert_path, std::path::PathBuf::from("/tmp/tls.cert"));
        assert_eq!(config.payments.evm.len(), 1);
        assert_eq!(config.payments.evm[0].network, "base");
    }

    #[test]
    fn test_old_compute_section_gets_new_defaults() {
        let toml_str = r#"
data_dir = "/tmp/x"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
[compute]
worker_enabled = true
max_cpu_cores = 4
max_ram_mb = 8192
max_disk_mb = 10240
docker_socket = "/var/run/docker.sock"
hourly_rate_sats = 1000
accepted_chains = ["lightning"]
max_concurrent_jobs = 2
allowed_images = []
wallets = {}
"#;
        let config: Config = toml::from_str(toml_str).unwrap();
        let compute = config.compute.unwrap();
        assert_eq!(compute.max_job_timeout_secs, 3600);
        assert!(!compute.allow_network_jobs);
    }

    #[test]
    fn test_disable_worker_is_idempotent_without_compute_section() {
        let mut config = Config::default();
        assert!(!config.disable_worker());
        config.enable_worker(None, None, None);
        assert!(config.disable_worker());
        assert!(!config.worker_enabled());
    }

    #[test]
    fn test_compute_section_survives_save_load_roundtrip() {
        let mut config = Config {
            data_dir: std::path::PathBuf::from("/tmp/gt-test-worker"),
            ..Config::default()
        };
        config.enable_worker(Some(4), Some(8192), Some(1000));
        let tmp = std::env::temp_dir().join("gt_test_worker_config");
        let path = tmp.join("config.toml");
        let _ = std::fs::remove_dir_all(&tmp);
        config.save(&path).unwrap();
        let loaded = Config::load(&path).unwrap();
        assert!(loaded.worker_enabled());
        let compute = loaded.compute.unwrap();
        assert_eq!(compute.max_cpu_cores, 4);
        assert_eq!(compute.max_ram_mb, 8192);
        assert_eq!(compute.hourly_rate_sats, 1000);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn default_ports_validate() {
        assert!(Config::default().validate_ports().is_ok());
        assert_eq!(Config::default().git_port(), Some(9419));
    }

    #[test]
    fn mdns_defaults_on_for_old_configs() {
        let legacy = r#"
data_dir = "/tmp/gf-mdns-legacy"
p2p_port = 9418
api_port = 7878
auto_seed = true
max_upload_rate = 0
max_download_rate = 0
"#;
        let config: Config = toml::from_str(legacy).unwrap();
        assert!(config.enable_mdns);
        let off = legacy.to_string() + "enable_mdns = false\n";
        let config: Config = toml::from_str(&off).unwrap();
        assert!(!config.enable_mdns);
    }

    #[test]
    fn fresh_install_seeds_bootstrap_peers_but_default_stays_empty() {
        assert!(Config::default().bootstrap_peers.is_empty());
        let fresh = Config::fresh_install();
        assert_eq!(fresh.bootstrap_peers.len(), BOOTSTRAP_NODES.len());
        for addr in &fresh.bootstrap_peers {
            assert!(addr.starts_with("/ip4/"), "{addr}");
            assert!(addr.contains("/tcp/9418"), "{addr}");
            assert!(addr.ends_with("/api/7878"), "{addr}");
            assert!(addr.contains("/p2p/12D3Koo"), "{addr}");
        }
    }

    #[test]
    fn overlapping_ports_fail_validation() {
        let mut config = Config::default();
        config.api_port = config.p2p_port;
        assert!(config.validate_ports().is_err());
        config.api_port = config.git_port().unwrap();
        let err = config.validate_ports().unwrap_err();
        assert!(err.contains("collides with git protocol port"), "{err}");
        config.api_port = 7878;
        config.p2p_port = 65535;
        assert!(config.git_port().is_none());
        assert!(config.validate_ports().is_err());
    }
}
