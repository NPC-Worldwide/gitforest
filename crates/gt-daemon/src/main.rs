use anyhow::Result;
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use gt_core::config::Config;
use gt_core::identity::{
    identity_key_path, load_display_name, save_display_name, Identity, PeerName,
};
use std::path::PathBuf;
use tracing::info;
use tracing_subscriber::EnvFilter;

mod api;
mod caches;
mod dispatch;
mod fetch;
mod models;
mod network;
mod ratelimit;
mod repo_manager;
mod secrets;
mod workflows;

#[derive(Parser)]
#[command(name = "gitforest")]
#[command(about = "gitforest — decentralized P2P git hosting")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,

    /// Path to config file
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Path to identity key file
    #[arg(long, global = true)]
    identity: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Commands {
    /// Start or stop the gitforest daemon
    Daemon {
        /// Run in foreground (don't daemonize)
        #[arg(long, default_value_t = true)]
        foreground: bool,
        /// Stop a running daemon
        #[arg(long, default_value_t = false)]
        stop: bool,
    },

    /// Initialize a new identity
    Init {
        /// Set a human-readable display name (e.g., "laptop")
        #[arg(long)]
        name: Option<String>,
    },

    /// Show your peer ID (public key)
    Id,

    /// Set or show your display name
    Name {
        #[command(subcommand)]
        action: NameAction,
    },

    /// Add a local git repo to the network
    Add {
        /// Path to the git repo
        path: PathBuf,
        /// Name for the repo on the network
        #[arg(short, long)]
        name: Option<String>,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
    },

    /// List repos we're hosting
    List,

    /// Remove a repo from the network (doesn't delete the git repo)
    Remove {
        /// Name of the repo to remove
        name: String,
    },

    /// Show network status
    Status,

    /// Compute marketplace commands
    Compute {
        #[command(subcommand)]
        action: ComputeAction,
    },

    /// Large file storage commands (models, datasets)
    Blob {
        #[command(subcommand)]
        action: BlobAction,
    },

    /// Collection commands (group blobs into models/datasets)
    Collection {
        #[command(subcommand)]
        action: CollectionAction,
    },

    /// Peer connection commands
    Peer {
        #[command(subcommand)]
        action: PeerAction,
    },

    /// Access control: grant or revoke push access to your repos
    Acl {
        #[command(subcommand)]
        action: AclAction,
    },

    /// Merge requests: propose changes to repos you don't own
    Mr {
        #[command(subcommand)]
        action: MrAction,
    },

    /// P2P LLM models: publish and search shared model collections
    Model {
        #[command(subcommand)]
        action: ModelAction,
    },

    /// Run inference against a pulled P2P model
    Llm {
        #[command(subcommand)]
        action: LlmAction,
    },

    /// gitforest-style workflows: list, dispatch, and track runs
    Workflow {
        #[command(subcommand)]
        action: WorkflowAction,
    },

    /// Repo secrets for workflows, stored outside git
    Secrets {
        /// Repo name
        repo: String,
        #[command(subcommand)]
        action: SecretsAction,
    },

    /// Workflow caches shared across runs
    Cache {
        #[command(subcommand)]
        action: CacheAction,
    },
}

#[derive(Subcommand)]
enum CacheAction {
    /// Save a path into the cache under a key
    Save {
        /// Cache key
        #[arg(long)]
        key: String,
        /// Path to archive (file or directory)
        #[arg(long)]
        path: PathBuf,
    },

    /// Restore a cached path by key, with prefix fallbacks
    Restore {
        /// Exact cache key to try first
        #[arg(long)]
        key: String,
        /// Comma-separated restore-key prefixes to try in order
        #[arg(long)]
        restore_keys: Option<String>,
        /// Path to restore (same target used at save time)
        #[arg(long)]
        path: PathBuf,
    },

    /// Delete a cache key and free its archive when unshared
    Delete {
        /// Cache key to delete
        #[arg(long)]
        key: String,
    },
}

#[derive(Subcommand)]
enum SecretsAction {
    /// Store a secret value
    Set {
        /// Secret name (valid env key)
        key: String,
        /// Secret value (visible in process list; prefer --from-file)
        #[arg(conflicts_with = "from_file")]
        value: Option<String>,
        /// Read the secret value from a file
        #[arg(long)]
        from_file: Option<PathBuf>,
    },

    /// List secret names (values are never shown)
    List,

    /// Delete a secret
    Delete {
        /// Secret name
        key: String,
    },
}

#[derive(Subcommand)]
enum ComputeAction {
    /// Submit a CI job for a repo
    Submit {
        /// Repo name
        repo: String,
        /// Specific commit (default: HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Max payment in sats
        #[arg(long, default_value_t = 10000)]
        max_sats: u64,
        /// Preferred payment chain
        #[arg(long, default_value = "lightning")]
        chain: String,
    },

    /// Check status of a compute job
    #[command(name = "status")]
    JobStatus {
        /// Job ID
        job_id: String,
    },

    /// Cancel one of our compute jobs
    #[command(name = "cancel")]
    Cancel {
        /// Job ID
        job_id: String,
    },

    /// Approve a job waiting on a protected environment
    #[command(name = "approve")]
    Approve {
        /// Job ID
        job_id: String,
    },

    /// Reject a job waiting on a protected environment
    #[command(name = "reject")]
    Reject {
        /// Job ID
        job_id: String,
    },

    /// Download a job artifact by name
    #[command(name = "download-artifact")]
    DownloadArtifact {
        /// Job ID
        job_id: String,
        /// Artifact name as recorded (e.g. dist/app.tar.gz)
        name: String,
        /// Output file (default: the artifact base name)
        #[arg(long)]
        out: Option<String>,
    },

    /// Create an encrypted EVM keystore for the embedded signer
    #[command(name = "evm-keystore-create")]
    EvmKeystoreCreate {
        /// Output keystore path (must not exist)
        #[arg(long)]
        path: PathBuf,
        /// File holding the 32-byte secret as hex (0600); conflicts with --generate
        #[arg(long, conflicts_with = "generate")]
        key_file: Option<PathBuf>,
        /// Generate a fresh random secret instead of importing
        #[arg(long)]
        generate: bool,
        /// File holding the keystore password (0600)
        #[arg(long)]
        password_file: PathBuf,
    },

    /// Rotate an EVM keystore to a fresh key (prints migration checklist)
    #[command(name = "evm-keystore-rotate")]
    EvmKeystoreRotate {
        /// Existing keystore to rotate away from
        #[arg(long)]
        from: PathBuf,
        /// Output keystore path (must not exist)
        #[arg(long)]
        path: PathBuf,
        /// File holding the old keystore password (0600)
        #[arg(long)]
        password_file: PathBuf,
        /// File holding the new keystore password (0600); defaults to --password-file
        #[arg(long)]
        new_password_file: Option<PathBuf>,
    },

    /// Start offering compute resources
    #[command(name = "worker-start")]
    WorkerStart {
        /// CPU cores to offer
        #[arg(long, default_value_t = 2)]
        cpu: u32,
        /// RAM in MB to offer
        #[arg(long, default_value_t = 4096)]
        ram: u64,
        /// Hourly rate in sats
        #[arg(long, default_value_t = 1000)]
        rate: u64,
        /// Run jobs directly on this machine instead of Docker (trusted jobs only)
        #[arg(long)]
        local: bool,
    },

    /// Stop offering compute resources
    #[command(name = "worker-stop")]
    WorkerStop,

    /// List available compute workers
    Offers,

    /// Show mutual-credit balance (given/taken compute-seconds)
    Credit,

    /// Configure a wallet address for a chain
    Wallet {
        /// Chain name (lightning, arbitrum, base, etc.)
        chain: String,
        /// Wallet address
        address: String,
    },
}

#[derive(Subcommand)]
enum BlobAction {
    /// Add a file to the blob store
    Add {
        /// Path to the file
        path: PathBuf,
    },

    /// List all stored blobs
    List,

    /// Show info about a specific blob
    Info {
        /// Blob ID (SHA-256 hash)
        blob_id: String,
    },

    /// Remove a blob from the store
    Remove {
        /// Blob ID to remove
        blob_id: String,
    },

    /// Generate a pointer file for a blob
    Pointer {
        /// Blob ID
        blob_id: String,
    },
}

#[derive(Subcommand)]
enum CollectionAction {
    /// Create a new collection from blob IDs
    Create {
        /// Collection name
        #[arg(short, long)]
        name: String,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
        /// Blob IDs to include (comma-separated blob_id:path:size)
        #[arg(short, long, value_delimiter = ',')]
        files: Vec<String>,
        /// Metadata key=value pairs
        #[arg(short, long, value_delimiter = ',')]
        metadata: Vec<String>,
    },

    /// List all collections
    List,

    /// Show collection details
    Info {
        /// Collection ID
        id: String,
    },
}

#[derive(Subcommand)]
enum NameAction {
    /// Set your display name
    Set {
        /// The display name to use (e.g., "laptop", "desktop")
        name: String,
    },

    /// Show your current peer name
    Show,
}

#[derive(Subcommand)]
enum PeerAction {
    /// Add a bootstrap peer to connect to on startup
    Add {
        /// Peer address in multiaddr format (e.g., /ip4/192.168.1.50/tcp/9418)
        address: String,
    },

    /// List configured bootstrap peers
    List,

    /// Remove a bootstrap peer
    Remove {
        /// Peer address to remove
        address: String,
    },

    /// Show connection info for this machine (share with others)
    Info,

    /// Save a remote peer's name tag locally for easy reference
    Alias {
        /// Peer name tag (e.g., "desktop#a1b2")
        tag: String,
        /// Full hex peer ID
        peer_id: String,
    },
}

#[derive(Subcommand)]
enum AclAction {
    /// Grant push access to a peer for a specific ref pattern
    Grant {
        /// Repo name
        repo: String,
        /// Peer ID to grant access to
        peer_id: String,
        /// Ref pattern (default: refs/heads/*)
        #[arg(default_value = "refs/heads/*")]
        ref_pattern: String,
    },

    /// Revoke all push access from a peer
    Revoke {
        /// Repo name
        repo: String,
        /// Peer ID to revoke access from
        peer_id: String,
    },

    /// Show current ACL for a repo
    Show {
        /// Repo name
        repo: String,
    },
}

#[derive(Subcommand)]
enum MrAction {
    /// Create a merge request proposing changes from your fork
    Create {
        /// Target repo name (on the upstream owner's node)
        repo: String,
        /// Target peer ID or name tag (e.g., laptop#cd26)
        #[arg(long)]
        target_peer: String,
        /// Your forked repo name (on your node)
        #[arg(long)]
        source_repo: String,
        /// Branch in your fork to merge from
        #[arg(long)]
        source_branch: String,
        /// Target branch to merge into (default: main)
        #[arg(long, default_value = "main")]
        target_branch: String,
        /// Title of the merge request
        #[arg(short, long)]
        title: String,
        /// Description of the changes
        #[arg(short, long, default_value = "")]
        description: String,
    },

    /// List merge requests for a repo
    List {
        /// Repo name
        repo: String,
    },

    /// Show details of a specific merge request
    Info {
        /// Repo name
        repo: String,
        /// Merge request ID
        id: String,
    },
}

#[derive(Subcommand)]
enum ModelAction {
    /// Publish a collection as an LLM model on the DHT
    Publish {
        /// Collection ID to publish
        collection_id: String,
    },

    /// Search shared models by name, architecture, or size
    Search {
        /// Free-text query against name and architecture
        #[arg(short, long)]
        query: Option<String>,
        /// Model architecture (llama, mistral, bert)
        #[arg(long)]
        architecture: Option<String>,
        /// Quantization tag (q4_0, q5_K_M)
        #[arg(long)]
        quantization: Option<String>,
        /// Weight format (gguf, safetensors, pytorch, onnx)
        #[arg(long)]
        format: Option<String>,
        /// Minimum parameter count (supports 7B, 350M, 90k)
        #[arg(long)]
        min_params: Option<String>,
        /// Maximum parameter count (supports 7B, 350M, 90k)
        #[arg(long)]
        max_params: Option<String>,
    },

    /// Show model details including shard layout
    Info {
        /// Manifest ID of the model
        id: String,
    },

    /// Pull a model manifest and all its weight blobs over P2P
    Pull {
        /// Manifest ID of the model
        id: String,
        /// Seconds to wait for the pull to finish
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
    },

    /// Manage model access control
    Acl {
        /// Manifest ID of the model
        id: String,
        /// Action: show, restrict, open, grant, revoke
        #[arg(long, default_value = "show")]
        action: String,
        /// Peer ID for grant and revoke
        #[arg(long)]
        peer: Option<String>,
    },
}

#[derive(Subcommand)]
enum LlmAction {
    /// Run one inference against a local model and print the completion
    Run {
        /// Manifest ID or name of the model
        #[arg(long)]
        model: String,
        /// Prompt text (or use --prompt-file)
        #[arg(long)]
        prompt: Option<String>,
        /// Read the prompt from a file
        #[arg(long)]
        prompt_file: Option<String>,
        /// Maximum tokens to generate
        #[arg(long)]
        max_tokens: Option<u32>,
        /// Sampling temperature 0..=2
        #[arg(long)]
        temperature: Option<f32>,
        /// System prompt
        #[arg(long)]
        system: Option<String>,
        /// Backend timeout in seconds
        #[arg(long, default_value_t = 300)]
        timeout: u64,
        /// Print completion chunks as they arrive instead of waiting
        #[arg(long)]
        stream: bool,
    },
}

#[derive(Subcommand)]
enum WorkflowAction {
    /// List workflows defined in a repo
    List {
        /// Repo name
        repo: String,
    },

    /// Manually dispatch workflows in a repo
    Dispatch {
        /// Repo name
        repo: String,
        /// Only run this workflow (name or file)
        #[arg(long)]
        workflow: Option<String>,
        /// Simulate a push to this branch instead of workflow_dispatch
        #[arg(long)]
        branch: Option<String>,
        /// Simulate a push to this tag instead of workflow_dispatch
        #[arg(long)]
        tag: Option<String>,
        /// Commit SHA to run against (default: HEAD)
        #[arg(long)]
        commit: Option<String>,
        /// Workflow dispatch input as KEY=VALUE (repeatable)
        #[arg(long = "input")]
        inputs: Vec<String>,
    },

    /// List workflow runs
    Runs,

    /// Show one workflow run with per-job status
    Show {
        /// Run ID
        run_id: String,
    },

    /// Re-run a run's failed jobs as a new run linked to the original
    Rerun {
        /// Run ID
        run_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    // Both binaries share this main; report the invoked name so `gtfo
    // --version` etc. identify themselves correctly.
    let argv0 = std::env::args().next().unwrap_or_default();
    let bin_name: &'static str = match argv0.rsplit('/').next() {
        Some("gtfo") => "gtfo",
        _ => "gitforest",
    };
    let matches = Cli::command().name(bin_name).get_matches();
    let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());

    match &cli.command {
        Commands::Init { name } => {
            let key_path = identity_key_path();
            if key_path.exists() {
                println!("Identity already exists at {}", key_path.display());
                let id = Identity::load(&key_path)?;
                if let Some(display_name) = &name {
                    save_display_name(display_name)?;
                }
                let peer_id = id.peer_id();
                if let Some(display_name) = name.clone().or_else(load_display_name) {
                    let pn = PeerName::new(display_name, peer_id.clone());
                    println!("Your peer name: {}", pn.short_tag());
                }
                println!("Your peer ID: {}", peer_id);
            } else {
                let id = Identity::generate();
                id.save(&key_path)?;
                let pow_path = gt_core::identity::pow_path_for(&key_path);
                let pow =
                    gt_core::identity::mine_pow(&id.peer_id(), gt_core::identity::MIN_POW_BITS);
                if gt_core::identity::save_pow(
                    &pow_path,
                    &id.peer_id(),
                    pow,
                    gt_core::identity::MIN_POW_BITS,
                )
                .is_err()
                {
                    eprintln!("warning: could not cache proof-of-work; it will be re-mined on use");
                }
                if let Some(display_name) = &name {
                    save_display_name(display_name)?;
                }
                println!("Identity created at {}", key_path.display());
                let peer_id = id.peer_id();
                if let Some(display_name) = name.clone().or_else(load_display_name) {
                    let pn = PeerName::new(display_name, peer_id.clone());
                    println!("Your peer name: {}", pn.short_tag());
                }
                println!("Your peer ID: {}", peer_id);
            }
        }

        Commands::Id => {
            let key_path = identity_key_path();
            if !key_path.exists() {
                eprintln!("No identity found. Run `gitforest init` first.");
                std::process::exit(1);
            }
            let id = Identity::load(&key_path)?;
            let peer_id = id.peer_id();
            if let Some(display_name) = load_display_name() {
                let pn = PeerName::new(display_name, peer_id.clone());
                println!("{} ({})", pn.short_tag(), peer_id);
            } else {
                println!("{}", peer_id);
            }
        }

        Commands::Name { action } => match action {
            NameAction::Set { name } => {
                let identity = load_identity(&cli)?;
                save_display_name(name)?;
                let pn = PeerName::new(name.to_string(), identity.peer_id());
                println!("Display name set. Your peer name: {}", pn.short_tag());
            }
            NameAction::Show => {
                let identity = load_identity(&cli)?;
                let peer_id = identity.peer_id();
                if let Some(display_name) = load_display_name() {
                    let pn = PeerName::new(display_name, peer_id.clone());
                    println!("{} ({})", pn.short_tag(), peer_id);
                } else {
                    println!("No display name set. Use `gitforest name set <name>` to set one.");
                    println!("Peer ID: {}", peer_id);
                }
            }
        },

        Commands::Daemon {
            foreground: _,
            stop,
        } => {
            let pidfile = daemon_pidfile();
            if *stop {
                if let Ok(pid_str) = std::fs::read_to_string(&pidfile) {
                    if let Ok(pid) = pid_str.trim().parse::<i32>() {
                        let _ = std::process::Command::new("kill")
                            .args(["-TERM", &pid.to_string()])
                            .output();
                        println!("Sent SIGTERM to daemon (pid {pid})");
                        let _ = std::fs::remove_file(&pidfile);
                    } else {
                        eprintln!("Invalid pid in pidfile");
                        std::process::exit(1);
                    }
                } else {
                    eprintln!("No pidfile found — daemon may not be running");
                    std::process::exit(1);
                }
                return Ok(());
            }

            let mut config = load_config(&cli.config)?;
            if let Err(e) = config.validate_ports() {
                eprintln!("Invalid config: {e}");
                std::process::exit(1);
            }
            if let Some(compute) = config.compute.as_mut() {
                if compute.gpu_auto_detect && !compute.gpu_enabled {
                    let found = gt_compute::gpu::detect_gpu_count().await;
                    compute.gpu_detected = found > 0;
                    if found > 0 {
                        info!("Detected {found} GPU(s); advertising GPU capacity");
                    }
                }
            }
            let identity = load_identity(&cli)?;
            let identity_path = cli
                .identity
                .as_ref()
                .cloned()
                .unwrap_or_else(gt_core::identity::identity_key_path);
            info!("Starting gitforest daemon");
            info!("Peer ID: {}", identity.peer_id());
            info!("API: http://127.0.0.1:{}", config.api_port);
            info!("P2P port: {}", config.p2p_port);

            // Write pidfile
            if let Some(parent) = pidfile.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            std::fs::write(&pidfile, std::process::id().to_string())?;
            install_panic_hook(&config.data_dir);

            // Start network layer
            let dht = network::start_network(&config, &identity).await?;

            // Start HTTP API
            let config_path = cli
                .config
                .unwrap_or_else(gt_core::config::default_config_path);

            // Graceful shutdown on SIGTERM / SIGINT
            let api_handle = tokio::spawn(async move {
                api::start_api(config.clone(), config_path, identity, identity_path, dht).await
            });

            #[cfg(unix)]
            let sigterm = async {
                let mut sigterm =
                    tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                        .expect("SIGTERM handler");
                sigterm.recv().await;
                info!("Received SIGTERM, shutting down...");
            };
            #[cfg(not(unix))]
            let sigterm = std::future::pending::<()>();

            tokio::select! {
                _ = tokio::signal::ctrl_c() => {
                    info!("Received SIGINT, shutting down...");
                }
                _ = sigterm => {}
                result = api_handle => {
                    if let Err(e) = result {
                        tracing::error!("API task panicked: {e}");
                    }
                }
            }

            let _ = std::fs::remove_file(&pidfile);
            info!("Daemon stopped");
        }

        Commands::Add {
            path,
            name,
            description,
        } => {
            let mut config = load_config(&cli.config)?;
            let identity = load_identity(&cli)?;

            let repo_path = std::fs::canonicalize(path)?;
            let repo_name = name.clone().unwrap_or_else(|| {
                repo_path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "unnamed".to_string())
                    .trim_end_matches(".git")
                    .to_string()
            });

            // Verify it's a git repo (bare has HEAD, normal has .git, or ask git)
            let is_git = repo_path.join("HEAD").exists()
                || repo_path.join(".git").exists()
                || std::process::Command::new("git")
                    .args(["rev-parse", "--git-dir"])
                    .current_dir(&repo_path)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false);

            if !is_git {
                eprintln!("Not a git repository: {}", repo_path.display());
                std::process::exit(1);
            }

            let repo_config = gt_core::config::RepoConfig {
                name: repo_name.clone(),
                path: repo_path,
                owner: identity.peer_id(),
                description: Some(description.to_string()),
                seeding: true,
            };

            config.repos.push(repo_config);
            let config_path = cli
                .config
                .unwrap_or_else(gt_core::config::default_config_path);
            config.save(&config_path)?;

            println!("Added repo '{}' to the network", repo_name);
            let peer_id = identity.peer_id();
            if let Some(display_name) = load_display_name() {
                let pn = PeerName::new(display_name, peer_id.clone());
                println!("Clone URL: gitforest://{}/{}", pn.short_tag(), repo_name);
            } else {
                println!("Clone URL: gitforest://{}/{}", peer_id, repo_name);
            }
        }

        Commands::List => {
            let config = load_config(&cli.config)?;
            if config.repos.is_empty() {
                println!("No repos configured. Use `gitforest add <path>` to add one.");
            } else {
                println!("{0:<20} {1:<8} PATH", "NAME", "SEEDING");
                for repo in &config.repos {
                    println!(
                        "{:<20} {:<8} {}",
                        repo.name,
                        if repo.seeding { "yes" } else { "no" },
                        repo.path.display()
                    );
                }
            }
        }

        Commands::Remove { name } => {
            let mut config = load_config(&cli.config)?;
            let before = config.repos.len();
            config.repos.retain(|r| r.name != *name);
            if config.repos.len() == before {
                eprintln!("No repo named '{}' found", name);
                std::process::exit(1);
            }
            let config_path = cli
                .config
                .unwrap_or_else(gt_core::config::default_config_path);
            config.save(&config_path)?;
            println!("Removed '{}' from the network", name);
        }

        Commands::Status => {
            let config = load_config(&cli.config)?;
            let identity = load_identity(&cli)?;
            let peer_id = identity.peer_id();
            if let Some(display_name) = load_display_name() {
                let pn = PeerName::new(display_name, peer_id.clone());
                println!("Peer name: {}", pn.short_tag());
            }
            println!("Peer ID: {}", peer_id);
            println!("Repos: {}", config.repos.len());
            println!(
                "Seeding: {}",
                config.repos.iter().filter(|r| r.seeding).count()
            );
            println!("P2P port: {}", config.p2p_port);
            println!("API port: {}", config.api_port);
            if let Some(ref compute) = config.compute {
                println!(
                    "Compute worker: {}",
                    if compute.worker_enabled {
                        "enabled"
                    } else {
                        "disabled"
                    }
                );
                if compute.worker_enabled {
                    println!(
                        "  CPU: {} cores, RAM: {} MB",
                        compute.max_cpu_cores, compute.max_ram_mb
                    );
                    println!("  Rate: {} sats/hr", compute.hourly_rate_sats);
                    println!("  Chains: {}", compute.accepted_chains.join(", "));
                }
            }
        }

        Commands::Blob { action } => {
            let config = load_config(&cli.config)?;
            let store = gt_lfs::store::BlobStore::new(&config.data_dir);

            match action {
                BlobAction::Add { path } => {
                    let file_path = std::fs::canonicalize(path)?;
                    if !file_path.exists() {
                        eprintln!("File not found: {}", file_path.display());
                        std::process::exit(1);
                    }

                    let file_size = std::fs::metadata(&file_path)?.len();
                    println!(
                        "Importing {} ({})...",
                        file_path.display(),
                        format_size(file_size)
                    );

                    let meta = store.store_file(&file_path)?;
                    println!("Blob stored:");
                    println!("  ID:     {}", meta.blob_id);
                    println!("  Size:   {}", format_size(meta.total_size));
                    println!(
                        "  Pieces: {} x {}",
                        meta.piece_count(),
                        format_size(meta.piece_size as u64)
                    );
                    println!("\nPointer file content:");
                    let pointer = gt_lfs::pointer::PointerFile::new(meta.blob_id, meta.total_size);
                    print!("{}", pointer.encode());
                }

                BlobAction::List => {
                    let ids = store.list()?;
                    if ids.is_empty() {
                        println!("No blobs stored. Use `gitforest blob add <path>` to add one.");
                    } else {
                        println!("{:<66} {:>12} {:>8}", "BLOB ID", "SIZE", "PIECES");
                        for id in &ids {
                            if let Ok(Some(meta)) = store.get_meta(id) {
                                println!(
                                    "{:<66} {:>12} {:>8}",
                                    id,
                                    format_size(meta.total_size),
                                    meta.piece_count()
                                );
                            } else {
                                println!("{:<66} {:>12} {:>8}", id, "?", "?");
                            }
                        }
                        println!(
                            "\nTotal: {} blob(s), {}",
                            ids.len(),
                            format_size(store.total_size()?)
                        );
                    }
                }

                BlobAction::Info { blob_id } => {
                    let id: gt_core::types::BlobId = blob_id
                        .parse()
                        .map_err(|e| anyhow::anyhow!("invalid blob ID: {e}"))?;
                    if !store.has(&id) {
                        eprintln!("Blob not found: {blob_id}");
                        std::process::exit(1);
                    }
                    if let Some(meta) = store.get_meta(&id)? {
                        println!("Blob ID:    {}", meta.blob_id);
                        println!("Size:       {}", format_size(meta.total_size));
                        println!("Piece size: {}", format_size(meta.piece_size as u64));
                        println!("Pieces:     {}", meta.piece_count());
                        println!("Store path: {}", store.blob_path(&id).display());
                    }
                }

                BlobAction::Remove { blob_id } => {
                    let id: gt_core::types::BlobId = blob_id
                        .parse()
                        .map_err(|e| anyhow::anyhow!("invalid blob ID: {e}"))?;
                    store.remove(&id)?;
                    println!("Blob removed: {blob_id}");
                }

                BlobAction::Pointer { blob_id } => {
                    let id: gt_core::types::BlobId = blob_id
                        .parse()
                        .map_err(|e| anyhow::anyhow!("invalid blob ID: {e}"))?;
                    if let Some(meta) = store.get_meta(&id)? {
                        let pointer =
                            gt_lfs::pointer::PointerFile::new(meta.blob_id, meta.total_size);
                        print!("{}", pointer.encode());
                    } else {
                        eprintln!("Blob not found: {blob_id}");
                        std::process::exit(1);
                    }
                }
            }
        }

        Commands::Collection { action } => {
            match action {
                CollectionAction::Create {
                    name,
                    description,
                    files,
                    metadata,
                } => {
                    let identity = load_identity(&cli)?;

                    let parsed_files: Vec<gt_core::types::CollectionFile> = files
                        .iter()
                        .map(|entry| {
                            // Format: blob_id:path:size
                            let parts: Vec<&str> = entry.splitn(3, ':').collect();
                            if parts.len() != 3 {
                                eprintln!("Invalid file entry: {entry}");
                                eprintln!("Expected format: <blob_id>:<path>:<size>");
                                std::process::exit(1);
                            }
                            let blob_id: gt_core::types::BlobId =
                                parts[0].parse().unwrap_or_else(|e| {
                                    eprintln!("Invalid blob ID: {e}");
                                    std::process::exit(1);
                                });
                            let size: u64 = parts[2].parse().unwrap_or_else(|e| {
                                eprintln!("Invalid size: {e}");
                                std::process::exit(1);
                            });
                            gt_core::types::CollectionFile {
                                blob_id,
                                path: parts[1].to_string(),
                                size,
                                media_type: None,
                            }
                        })
                        .collect();

                    let parsed_metadata: std::collections::HashMap<String, String> = metadata
                        .iter()
                        .filter_map(|kv| {
                            let parts: Vec<&str> = kv.splitn(2, '=').collect();
                            if parts.len() == 2 {
                                Some((parts[0].to_string(), parts[1].to_string()))
                            } else {
                                None
                            }
                        })
                        .collect();

                    let manifest = gt_core::types::CollectionManifest::new(
                        &identity,
                        name.clone(),
                        description.to_string(),
                        parsed_files,
                        parsed_metadata,
                    );

                    println!("Collection created:");
                    println!("  ID:    {}", manifest.id);
                    println!("  Name:  {}", manifest.name);
                    println!("  Files: {}", manifest.files.len());
                    println!("  Size:  {}", format_size(manifest.total_size()));
                    println!("  Signed by: {}", manifest.owner);
                }

                CollectionAction::List => {
                    println!("Collections are listed when the daemon is running.");
                    println!("Visit http://127.0.0.1:7878/api/collections");
                }

                CollectionAction::Info { id } => {
                    println!("Collection details available from the daemon API.");
                    println!("Visit http://127.0.0.1:7878/api/collections/{id}");
                }
            }
        }

        Commands::Peer { action } => {
            match action {
                PeerAction::Add { address } => {
                    let mut config = load_config(&cli.config)?;
                    if config.bootstrap_peers.contains(address) {
                        println!("Peer already configured: {address}");
                    } else {
                        config.bootstrap_peers.push(address.clone());
                        let config_path = cli
                            .config
                            .unwrap_or_else(gt_core::config::default_config_path);
                        config.save(&config_path)?;
                        println!("Added bootstrap peer: {address}");
                        println!("Restart the daemon to connect.");
                    }
                }

                PeerAction::List => {
                    let config = load_config(&cli.config)?;
                    if config.bootstrap_peers.is_empty() {
                        println!("No bootstrap peers configured.");
                        println!("On the same LAN, peers are discovered automatically via mDNS.");
                        println!("For remote peers, use: gitforest peer add /ip4/<IP>/tcp/9418");
                    } else {
                        println!("Bootstrap peers:");
                        for peer in &config.bootstrap_peers {
                            println!("  {peer}");
                        }
                    }
                }

                PeerAction::Remove { address } => {
                    let mut config = load_config(&cli.config)?;
                    let before = config.bootstrap_peers.len();
                    config.bootstrap_peers.retain(|p| p != address);
                    if config.bootstrap_peers.len() < before {
                        let config_path = cli
                            .config
                            .unwrap_or_else(gt_core::config::default_config_path);
                        config.save(&config_path)?;
                        println!("Removed bootstrap peer: {address}");
                    } else {
                        println!("Peer not found: {address}");
                    }
                }

                PeerAction::Info => {
                    let config = load_config(&cli.config)?;
                    let identity = load_identity(&cli)?;

                    // Get local IP addresses
                    println!("=== Share this with other machines ===\n");
                    let peer_id = identity.peer_id();
                    if let Some(display_name) = load_display_name() {
                        let pn = PeerName::new(display_name, peer_id.clone());
                        println!("Peer name: {}", pn.short_tag());
                    }
                    println!("Peer ID: {}", peer_id);
                    println!("P2P port: {}", config.p2p_port);
                    println!("\nOn other machines, run:");
                    println!(
                        "  gitforest peer add /ip4/<THIS_MACHINE_IP>/tcp/{}",
                        config.p2p_port
                    );
                    println!("\nExample (replace with your actual IP):");
                    println!(
                        "  gitforest peer add /ip4/192.168.1.100/tcp/{}",
                        config.p2p_port
                    );
                    println!("\nThen restart the daemon on both machines.");
                    println!(
                        "\nFor internet access, forward port {} and use your public IP.",
                        config.p2p_port
                    );
                }

                PeerAction::Alias { tag, peer_id } => {
                    // Validate the tag format
                    if !tag.contains('#') {
                        eprintln!("Tag must be in name#disc format (e.g., desktop#a1b2)");
                        std::process::exit(1);
                    }
                    // Validate the peer ID
                    if peer_id.len() != 64 || hex::decode(peer_id).is_err() {
                        eprintln!("Peer ID must be a 64-character hex string");
                        std::process::exit(1);
                    }
                    // Validate discriminator matches peer ID prefix
                    if let Some((_name, disc)) = tag.rsplit_once('#') {
                        if !peer_id.starts_with(disc) {
                            eprintln!("Warning: discriminator '{disc}' does not match peer ID prefix '{}'", &peer_id[..4]);
                        }
                    }

                    let mut config = load_config(&cli.config)?;
                    config.peer_names.insert(tag.clone(), peer_id.clone());
                    let config_path = cli
                        .config
                        .unwrap_or_else(gt_core::config::default_config_path);
                    config.save(&config_path)?;
                    println!("Alias saved: {} -> {}", tag, peer_id);
                }
            }
        }

        Commands::Compute { action } => match action {
            ComputeAction::Submit {
                repo,
                commit,
                max_sats,
                chain,
            } => {
                let config = load_config(&cli.config)?;
                let _identity = load_identity(&cli)?;

                let repo_config = config.repos.iter().find(|r| r.name == *repo);
                let commit_sha = commit.clone().unwrap_or_else(|| {
                    if let Some(rc) = repo_config {
                        std::process::Command::new("git")
                            .args(["rev-parse", "HEAD"])
                            .current_dir(&rc.path)
                            .output()
                            .ok()
                            .and_then(|o| String::from_utf8(o.stdout).ok())
                            .map(|s| s.trim().to_string())
                            .unwrap_or_else(|| "HEAD".into())
                    } else {
                        "HEAD".into()
                    }
                });

                let yaml = if let Some(rc) = repo_config {
                    let ci_path = rc.path.join(".gitforest-ci.yml");
                    if !ci_path.exists() {
                        eprintln!("No .gitforest-ci.yml found in repo. Create one first.");
                        std::process::exit(1);
                    }
                    std::fs::read_to_string(&ci_path)?
                } else {
                    eprintln!(
                        "Repo '{}' not found locally. Add it first with `gitforest add`.",
                        repo
                    );
                    std::process::exit(1);
                };
                if let Err(e) = gt_compute::job::parse_ci_config(&yaml) {
                    eprintln!("Invalid .gitforest-ci.yml: {e}");
                    std::process::exit(1);
                }

                let url = format!("{}/api/compute/jobs", daemon_base_url(&config));
                let resp = daemon_client()
                    .post(&url)
                    .json(&serde_json::json!({
                        "repo_name": repo,
                        "commit_sha": commit_sha,
                        "job_config_yaml": yaml,
                        "max_payment_sats": max_sats,
                        "preferred_chain": chain,
                    }))
                    .send()
                    .await;
                match resp {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        let id = body.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                        let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                        println!("Job submitted: {id}");
                        println!("  Repo: {repo}");
                        println!("  Commit: {commit_sha}");
                        println!("  Status: {status}");
                        println!("  Max payment: {max_sats} sats");
                        if let Some(warning) = body.get("warning").and_then(|v| v.as_str()) {
                            println!("  Warning: {warning}");
                        }
                        println!("\nTo check status: gitforest compute status {id}");
                    }
                    Ok(r) if r.status().as_u16() == 402 => {
                        eprintln!("Mutual credit exhausted: run `gitforest compute worker-start` to earn credit, or configure a money backend.");
                        std::process::exit(1);
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::JobStatus { job_id } => {
                let config = load_config(&cli.config)?;
                let url = format!("{}/api/compute/jobs/{job_id}", daemon_base_url(&config));
                match daemon_client().get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let job: serde_json::Value = r.json().await.unwrap_or_default();
                        println!("Job {job_id}");
                        println!(
                            "  Status: {}",
                            job.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                        );
                        println!(
                            "  Repo: {}",
                            job.get("repo_name").and_then(|v| v.as_str()).unwrap_or("?")
                        );
                        println!(
                            "  Commit: {}",
                            job.get("commit_sha")
                                .and_then(|v| v.as_str())
                                .unwrap_or("?")
                        );
                        if let Some(warning) = job.get("warning").and_then(|v| v.as_str()) {
                            println!("  Warning: {warning}");
                        }
                        if let Some(perms) = job.get("permissions").and_then(|v| v.as_object()) {
                            if !perms.is_empty() {
                                let mut grants: Vec<String> = perms
                                    .iter()
                                    .map(|(k, v)| format!("{k}:{}", v.as_str().unwrap_or("?")))
                                    .collect();
                                grants.sort();
                                println!("  Permissions: {}", grants.join(", "));
                            }
                        }
                        if let Some(result) = job.get("result") {
                            if let Some(code) = result.get("exit_code").and_then(|v| v.as_i64()) {
                                println!("  Exit code: {code}");
                            }
                            if let Some(usage) = result.get("resource_usage") {
                                let cpu = usage
                                    .get("peak_cpu_percent")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0);
                                let ram = usage
                                    .get("peak_ram_mb")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let disk = usage
                                    .get("disk_used_mb")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                println!(
                                    "  Peak usage: {cpu:.1}% CPU, {ram} MB RAM, {disk} MB disk"
                                );
                            }
                            if let Some(sum) = result.get("summary").and_then(|v| v.as_str()) {
                                if !sum.is_empty() {
                                    println!("  --- summary ---\n{sum}");
                                }
                            }
                            if let Some(out) = result.get("stdout_tail").and_then(|v| v.as_str()) {
                                if !out.is_empty() {
                                    println!("  --- stdout ---\n{out}");
                                }
                            }
                            if let Some(err) = result.get("stderr_tail").and_then(|v| v.as_str()) {
                                if !err.is_empty() {
                                    println!("  --- stderr ---\n{err}");
                                }
                            }
                            if let Some(blobs) =
                                result.get("artifact_blobs").and_then(|v| v.as_array())
                            {
                                if !blobs.is_empty() {
                                    println!("  Artifacts:");
                                    for blob in blobs {
                                        let name = blob
                                            .get("name")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let size =
                                            blob.get("size").and_then(|v| v.as_u64()).unwrap_or(0);
                                        let local = blob
                                            .get("local")
                                            .and_then(|v| v.as_bool())
                                            .unwrap_or(false);
                                        let where_ = if local { "local" } else { "remote" };
                                        println!("    {name} ({size} bytes, {where_})");
                                    }
                                }
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::Cancel { job_id } => {
                let config = load_config(&cli.config)?;
                let url = format!(
                    "{}/api/compute/jobs/{job_id}/cancel",
                    daemon_base_url(&config)
                );
                match daemon_client().post(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if body
                            .get("cancelled")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            println!("Job {job_id} cancelled.");
                            if let Some(note) = body.get("note").and_then(|v| v.as_str()) {
                                println!("  Note: {note}");
                            }
                            if body
                                .get("remote_notified")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false)
                            {
                                println!("  Worker notified.");
                            }
                        } else {
                            println!("Job {job_id} was not cancelled.");
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::Approve { job_id } => {
                let config = load_config(&cli.config)?;
                let url = format!(
                    "{}/api/compute/jobs/{job_id}/approve",
                    daemon_base_url(&config)
                );
                match daemon_client().post(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if body
                            .get("approved")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            println!("Job {job_id} approved.");
                            if let Some(env) = body.get("environment").and_then(|v| v.as_str()) {
                                println!("  Environment: {env}");
                            }
                            if let Some(note) = body.get("note").and_then(|v| v.as_str()) {
                                println!("  Note: {note}");
                            }
                        } else {
                            println!("Job {job_id} was not approved.");
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::Reject { job_id } => {
                let config = load_config(&cli.config)?;
                let url = format!(
                    "{}/api/compute/jobs/{job_id}/reject",
                    daemon_base_url(&config)
                );
                match daemon_client().post(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let body: serde_json::Value = r.json().await.unwrap_or_default();
                        if body
                            .get("rejected")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false)
                        {
                            println!("Job {job_id} rejected.");
                            if let Some(note) = body.get("note").and_then(|v| v.as_str()) {
                                println!("  Note: {note}");
                            }
                        } else {
                            println!("Job {job_id} was not rejected.");
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::DownloadArtifact { job_id, name, out } => {
                let config = load_config(&cli.config)?;
                let url = format!(
                    "{}/api/compute/jobs/{job_id}/artifacts/{name}",
                    daemon_base_url(&config)
                );
                match daemon_client().get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let bytes = r.bytes().await.unwrap_or_default();
                        let dest = out.clone().unwrap_or_else(|| {
                            name.rsplit('/').next().unwrap_or("artifact").to_string()
                        });
                        match std::fs::write(&dest, &bytes) {
                            Ok(()) => println!("Wrote {} bytes to {dest}", bytes.len()),
                            Err(e) => {
                                eprintln!("Failed to write {dest}: {e}");
                                std::process::exit(1);
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::EvmKeystoreCreate {
                path,
                key_file,
                generate,
                password_file,
            } => {
                if path.exists() {
                    eprintln!("Refusing to overwrite existing {}", path.display());
                    std::process::exit(1);
                }
                let secret: [u8; 32] = if *generate {
                    let mut raw = [0u8; 32];
                    rand::RngCore::fill_bytes(&mut rand::thread_rng(), &mut raw);
                    raw
                } else if let Some(key_path) = key_file {
                    let text = String::from_utf8(read_secret_file(key_path)?)?;
                    let text = text.trim().strip_prefix("0x").unwrap_or(text.trim());
                    let raw = hex::decode(text)?;
                    if raw.len() != 32 {
                        eprintln!("Key file must hold 32 bytes of hex");
                        std::process::exit(1);
                    }
                    let mut fixed = [0u8; 32];
                    fixed.copy_from_slice(&raw);
                    fixed
                } else {
                    eprintln!("Pass --key-file or --generate");
                    std::process::exit(1);
                };
                let mut password = read_secret_file(password_file)?;
                while password.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                    password.pop();
                }
                if password.is_empty() {
                    eprintln!("Password file is empty");
                    std::process::exit(1);
                }
                let address = gt_compute::keystore::eth_address_of_secret(&secret)?;
                gt_compute::keystore::create_keystore(path, &secret, &password, &address)?;
                println!("Keystore written to {}", path.display());
                println!("Address: {address}");
            }

            ComputeAction::EvmKeystoreRotate {
                from,
                path,
                password_file,
                new_password_file,
            } => {
                fn trimmed(mut secret: Vec<u8>) -> Vec<u8> {
                    while secret.last().is_some_and(|b| *b == b'\n' || *b == b'\r') {
                        secret.pop();
                    }
                    secret
                }
                let old_password = trimmed(read_secret_file(password_file)?);
                let new_password = match new_password_file {
                    Some(p) => trimmed(read_secret_file(p)?),
                    None => old_password.clone(),
                };
                if old_password.is_empty() || new_password.is_empty() {
                    eprintln!("Password file is empty");
                    std::process::exit(1);
                }
                let (old_address, new_address) = gt_compute::keystore::rotate_keystore(
                    from,
                    &old_password,
                    path,
                    &new_password,
                )?;
                println!("Rotated {} -> {}", from.display(), path.display());
                println!("Old address: {old_address}");
                println!("New address: {new_address}");
                println!("Next steps:");
                println!("  1. Fund {new_address} for gas + escrow.");
                println!(
                    "  2. Point the backend at {} and from_address {new_address}.",
                    path.display()
                );
                println!("  3. Restart the daemon and confirm release works on a test job.");
                println!("  4. Drain {old_address}, then delete {}.", from.display());
            }

            ComputeAction::WorkerStart {
                cpu,
                ram,
                rate,
                local,
            } => {
                let mut config = load_config(&cli.config)?;
                let compute = config
                    .compute
                    .get_or_insert_with(gt_core::config::ComputeConfig::default);
                compute.worker_enabled = true;
                compute.max_cpu_cores = *cpu;
                compute.max_ram_mb = *ram;
                compute.hourly_rate_sats = *rate;
                compute.execution_mode = if *local {
                    gt_core::config::ExecutionMode::Local
                } else {
                    gt_core::config::ExecutionMode::Docker
                };

                let config_path = cli
                    .config
                    .unwrap_or_else(gt_core::config::default_config_path);
                config.save(&config_path)?;

                println!("Compute worker enabled:");
                println!("  CPU: {cpu} cores");
                println!("  RAM: {ram} MB");
                println!("  Rate: {rate} sats/hr");
                println!("  Mode: {}", if *local { "local" } else { "docker" });
                println!("\nRestart the daemon to start offering compute.");
            }

            ComputeAction::WorkerStop => {
                let mut config = load_config(&cli.config)?;
                if let Some(ref mut compute) = config.compute {
                    compute.worker_enabled = false;
                    let config_path = cli
                        .config
                        .unwrap_or_else(gt_core::config::default_config_path);
                    config.save(&config_path)?;
                    println!("Compute worker disabled. Restart the daemon to apply.");
                } else {
                    println!("Compute worker is not configured.");
                }
            }

            ComputeAction::Offers => {
                let config = load_config(&cli.config)?;
                let url = format!("{}/api/compute/offers", daemon_base_url(&config));
                match daemon_client().get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let offers: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                        if offers.is_empty() {
                            println!("No compute workers available.");
                        } else {
                            println!(
                                "{:<20} {:>4} {:>8} {:>10} {:>6}",
                                "WORKER", "CPU", "RAM_MB", "RATE_SATS", "REP"
                            );
                            for o in offers {
                                let worker =
                                    o.get("worker_id").and_then(|v| v.as_str()).unwrap_or("?");
                                let cpu = o.get("cpu_cores").and_then(|v| v.as_u64()).unwrap_or(0);
                                let ram = o.get("ram_mb").and_then(|v| v.as_u64()).unwrap_or(0);
                                let rate = o
                                    .get("hourly_rate_sats")
                                    .and_then(|v| v.as_u64())
                                    .unwrap_or(0);
                                let rep =
                                    o.get("reputation").and_then(|v| v.as_f64()).unwrap_or(0.0);
                                println!(
                                    "{:<20} {:>4} {:>8} {:>10} {:>6.2}",
                                    &worker[..worker.len().min(20)],
                                    cpu,
                                    ram,
                                    rate,
                                    rep
                                );
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::Credit => {
                let config = load_config(&cli.config)?;
                let url = format!("{}/api/compute/credit", daemon_base_url(&config));
                match daemon_client().get(&url).send().await {
                    Ok(r) if r.status().is_success() => {
                        let credit: serde_json::Value = r.json().await.unwrap_or_default();
                        let given = credit
                            .get("given_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let taken = credit
                            .get("taken_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let deficit = credit
                            .get("own_deficit_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let allowance = credit
                            .get("allowance_secs")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(0);
                        let may_spend = credit
                            .get("may_spend")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false);
                        println!("Mutual credit (compute-seconds, owed to the network)");
                        println!("  Given: {given}s   Taken: {taken}s   Deficit: {deficit}s / {allowance}s");
                        println!(
                            "  Status: {}",
                            if may_spend {
                                "may submit"
                            } else {
                                "exhausted — run a worker to earn"
                            }
                        );
                        if let Some(peers) = credit.get("peers").and_then(|v| v.as_array()) {
                            for p in peers {
                                let id = p.get("peer_id").and_then(|v| v.as_str()).unwrap_or("?");
                                let g = p.get("given_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                                let t = p.get("taken_secs").and_then(|v| v.as_u64()).unwrap_or(0);
                                println!(
                                    "  peer {}…: given {g}s taken {t}s",
                                    &id[..id.len().min(12)]
                                );
                            }
                        }
                    }
                    Ok(r) => {
                        eprintln!("Failed: HTTP {}", r.status());
                        std::process::exit(1);
                    }
                    Err(e) => {
                        eprintln!("Failed to connect to daemon: {e}");
                        eprintln!("Make sure the daemon is running (gitforest daemon)");
                        std::process::exit(1);
                    }
                }
            }

            ComputeAction::Wallet { chain, address } => {
                let mut config = load_config(&cli.config)?;
                let compute = config
                    .compute
                    .get_or_insert_with(gt_core::config::ComputeConfig::default);

                compute.wallets.insert(chain.clone(), address.clone());
                if !compute.accepted_chains.contains(chain) {
                    compute.accepted_chains.push(chain.clone());
                }

                let config_path = cli
                    .config
                    .unwrap_or_else(gt_core::config::default_config_path);
                config.save(&config_path)?;

                println!("Wallet configured: {chain} = {address}");
            }
        },

        Commands::Acl { action } => {
            let _identity = load_identity(&cli)?;
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();

            match action {
                AclAction::Grant {
                    repo,
                    peer_id,
                    ref_pattern,
                } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/acl");
                    let resp = client
                        .post(&url)
                        .json(&serde_json::json!({
                            "action": "grant",
                            "peer_id": peer_id,
                            "ref_pattern": ref_pattern,
                        }))
                        .send()
                        .await;
                    match resp {
                        Ok(r) if r.status().is_success() => {
                            println!("Granted push access to {peer_id} on {repo} ({ref_pattern})");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                AclAction::Revoke { repo, peer_id } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/acl");
                    let resp = client
                        .post(&url)
                        .json(&serde_json::json!({
                            "action": "revoke",
                            "peer_id": peer_id,
                        }))
                        .send()
                        .await;
                    match resp {
                        Ok(r) if r.status().is_success() => {
                            println!("Revoked push access from {peer_id} on {repo}");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                AclAction::Show { repo } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/acl");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let acl: serde_json::Value = r.json().await.unwrap_or_default();
                            println!("ACL for {}:", repo);
                            println!(
                                "  Owner: {}",
                                acl.get("owner").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!(
                                "  Version: {}",
                                acl.get("version").and_then(|v| v.as_u64()).unwrap_or(0)
                            );
                            if let Some(grants) = acl.get("grants").and_then(|v| v.as_array()) {
                                if grants.is_empty() {
                                    println!("  Grants: (none)");
                                } else {
                                    println!("  Grants:");
                                    for g in grants {
                                        let peer = g
                                            .get("peer_id")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        let pat = g
                                            .get("ref_pattern")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?");
                                        println!("    {} -> {}", peer, pat);
                                    }
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Mr { action } => {
            let _identity = load_identity(&cli)?;
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();

            match action {
                MrAction::Create {
                    repo,
                    target_peer,
                    source_repo,
                    source_branch,
                    target_branch,
                    title,
                    description,
                } => {
                    // Resolve target peer
                    let our_peer_id = _identity.peer_id().to_string();
                    let target = if target_peer.len() == 64
                        && target_peer.chars().all(|c| c.is_ascii_hexdigit())
                    {
                        target_peer.clone()
                    } else {
                        // Try config aliases
                        let config = load_config(&cli.config)?;
                        config
                            .resolve_peer(target_peer)
                            .map(|p| p.to_string())
                            .unwrap_or(target_peer.clone())
                    };

                    let local_target = target == our_peer_id;
                    let target_url = if local_target {
                        format!("{daemon_url}/api/repos/{repo}/merge-requests")
                    } else {
                        // Proxy to target peer's daemon via bootstrap IP
                        let config = load_config(&cli.config)?;
                        let addr = config.bootstrap_peers.iter().find_map(|p| {
                            let parts: Vec<&str> = p.split('/').collect();
                            parts
                                .iter()
                                .position(|&s| s == "ip4")
                                .and_then(|idx| parts.get(idx + 1))
                                .map(|s| s.to_string())
                        });
                        match addr {
                            Some(ip) => format!("http://{ip}:7878/api/repos/{repo}/merge-requests"),
                            None => {
                                eprintln!(
                                    "Cannot reach target peer. Add them as a bootstrap peer first:"
                                );
                                eprintln!("  gitforest peer add /ip4/<IP>/tcp/9418");
                                std::process::exit(1);
                            }
                        }
                    };

                    let post_client = if local_target {
                        client
                    } else {
                        reqwest::Client::new()
                    };
                    let resp = post_client
                        .post(&target_url)
                        .json(&serde_json::json!({
                            "source_repo": source_repo,
                            "source_branch": source_branch,
                            "target_branch": target_branch,
                            "title": title,
                            "description": description,
                        }))
                        .send()
                        .await;
                    match resp {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            println!("Merge request created:");
                            println!(
                                "  ID:     {}",
                                json.get("id").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!(
                                "  Status: {}",
                                json.get("status").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!("\nThe repo owner will review your proposal.");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                MrAction::List { repo } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/merge-requests");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let mrs: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                            if mrs.is_empty() {
                                println!("No merge requests for {}", repo);
                            } else {
                                println!("{:<36} {:<20} {:<30}", "ID", "AUTHOR", "TITLE");
                                for mr in mrs {
                                    let id = mr.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                                    let author =
                                        mr.get("author").and_then(|v| v.as_str()).unwrap_or("?");
                                    let title =
                                        mr.get("title").and_then(|v| v.as_str()).unwrap_or("?");
                                    println!(
                                        "{:<36} {:<20} {:<30}",
                                        id,
                                        &author[..author.len().min(20)],
                                        title
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                MrAction::Info { repo, id } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/merge-requests");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let mrs: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                            let found = mrs
                                .iter()
                                .find(|mr| mr.get("id").and_then(|v| v.as_str()) == Some(id));
                            if let Some(mr) = found {
                                println!("Merge Request {}", id);
                                println!(
                                    "  Author:       {}",
                                    mr.get("author").and_then(|v| v.as_str()).unwrap_or("?")
                                );
                                println!(
                                    "  Title:        {}",
                                    mr.get("title").and_then(|v| v.as_str()).unwrap_or("?")
                                );
                                println!(
                                    "  Description:  {}",
                                    mr.get("description")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                );
                                println!(
                                    "  Source:       {}:{}",
                                    mr.get("source_repo")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?"),
                                    mr.get("source_branch")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                );
                                println!(
                                    "  Target:       {}:{}",
                                    repo,
                                    mr.get("target_branch")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("?")
                                );
                            } else {
                                eprintln!("Merge request {} not found in {}", id, repo);
                                std::process::exit(1);
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Model { action } => {
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();
            match action {
                ModelAction::Publish { collection_id } => {
                    let url = format!("{daemon_url}/api/models/publish");
                    match client
                        .post(&url)
                        .json(&serde_json::json!({ "collection_id": collection_id }))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            println!("Model published:");
                            println!(
                                "  Name:  {}",
                                json.get("name").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!(
                                "  Arch:  {}",
                                json.get("architecture")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                            );
                            println!(
                                "  Params: {}",
                                json.get("parameter_count")
                                    .map(|v| v.to_string())
                                    .unwrap_or("?".into())
                            );
                            println!(
                                "  Manifest: {}",
                                json.get("manifest_id")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?")
                            );
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                ModelAction::Search {
                    query,
                    architecture,
                    quantization,
                    format,
                    min_params,
                    max_params,
                } => {
                    let mut params: Vec<(String, String)> = Vec::new();
                    if let Some(v) = query {
                        params.push(("text".to_string(), v.clone()));
                    }
                    if let Some(v) = architecture {
                        params.push(("architecture".to_string(), v.clone()));
                    }
                    if let Some(v) = quantization {
                        params.push(("quantization".to_string(), v.clone()));
                    }
                    if let Some(v) = format {
                        params.push(("format".to_string(), v.clone()));
                    }
                    if let Some(v) = min_params {
                        params.push(("min_params".to_string(), v.clone()));
                    }
                    if let Some(v) = max_params {
                        params.push(("max_params".to_string(), v.clone()));
                    }
                    let url = format!("{daemon_url}/api/models/search");
                    match client.get(&url).query(&params).send().await {
                        Ok(r) if r.status().is_success() => {
                            let models: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                            if models.is_empty() {
                                println!("No models found");
                            } else {
                                println!(
                                    "{:<24} {:<10} {:<12} {:<10} {:<6}",
                                    "NAME", "ARCH", "PARAMS", "FORMAT", "LOCAL"
                                );
                                for m in models {
                                    println!(
                                        "{:<24} {:<10} {:<12} {:<10} {:<6}",
                                        m.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                                        m.get("architecture")
                                            .and_then(|v| v.as_str())
                                            .unwrap_or("?"),
                                        m.get("parameter_count")
                                            .map(|v| v.to_string())
                                            .unwrap_or("?".into()),
                                        m.get("format").and_then(|v| v.as_str()).unwrap_or("?"),
                                        m.get("local").and_then(|v| v.as_bool()).unwrap_or(false),
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                ModelAction::Info { id } => {
                    let url = format!("{daemon_url}/api/models/{id}");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let m: serde_json::Value = r.json().await.unwrap_or_default();
                            println!(
                                "Model {}",
                                m.get("name")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or(id.as_str())
                            );
                            println!(
                                "  Manifest: {}",
                                m.get("manifest_id").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!(
                                "  Owner: {}",
                                m.get("owner").and_then(|v| v.as_str()).unwrap_or("?")
                            );
                            println!(
                                "  Size: {}",
                                m.get("total_size")
                                    .map(|v| v.to_string())
                                    .unwrap_or("?".into())
                            );
                            if let Some(shards) = m.get("shards").and_then(|v| v.as_array()) {
                                println!("  Shards: {}", shards.len());
                                for s in shards {
                                    println!(
                                        "    {} ({} pieces, {} bytes)",
                                        s.get("base").and_then(|v| v.as_str()).unwrap_or("?"),
                                        s.get("pieces")
                                            .map(|v| v.to_string())
                                            .unwrap_or("?".into()),
                                        s.get("size").map(|v| v.to_string()).unwrap_or("?".into()),
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                ModelAction::Pull { id, timeout_secs } => {
                    let pull_url = format!("{daemon_url}/api/models/{id}/pull");
                    match client.post(&pull_url).send().await {
                        Ok(r) if r.status().is_success() => {}
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                    let info_url = format!("{daemon_url}/api/models/{id}");
                    let deadline =
                        std::time::Instant::now() + std::time::Duration::from_secs(*timeout_secs);
                    loop {
                        match client.get(&info_url).send().await {
                            Ok(r) if r.status().is_success() => {
                                let m: serde_json::Value = r.json().await.unwrap_or_default();
                                let present =
                                    m.get("blobs_present").and_then(|v| v.as_u64()).unwrap_or(0);
                                let total =
                                    m.get("blobs_total").and_then(|v| v.as_u64()).unwrap_or(0);
                                let local =
                                    m.get("local").and_then(|v| v.as_bool()).unwrap_or(false);
                                if local && present >= total {
                                    println!(
                                        "Model {} pulled ({present}/{total} blobs)",
                                        m.get("name").and_then(|v| v.as_str()).unwrap_or(id)
                                    );
                                    break;
                                }
                                println!("  fetching weights... ({present}/{total} blobs)");
                            }
                            _ => {
                                println!("  waiting for manifest...");
                            }
                        }
                        if std::time::Instant::now() >= deadline {
                            eprintln!("Timed out waiting for model {id}");
                            std::process::exit(1);
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                }
                ModelAction::Acl { id, action, peer } => {
                    let url = format!("{daemon_url}/api/models/{id}/acl");
                    match action.as_str() {
                        "show" => match client.get(&url).send().await {
                            Ok(r) if r.status().is_success() => {
                                let body: serde_json::Value = r.json().await.unwrap_or_default();
                                if body
                                    .get("restricted")
                                    .and_then(|v| v.as_bool())
                                    .unwrap_or(false)
                                {
                                    println!("Model {id} is restricted.");
                                    if let Some(grants) =
                                        body.get("grants").and_then(|v| v.as_array())
                                    {
                                        if grants.is_empty() {
                                            println!("  No grants (owner only).");
                                        }
                                        for grant in grants {
                                            if let Some(peer) = grant.as_str() {
                                                println!("  Granted: {peer}");
                                            }
                                        }
                                    }
                                } else {
                                    println!("Model {id} is open to the cluster.");
                                }
                            }
                            Ok(r) => {
                                eprintln!("Failed: HTTP {}", r.status());
                                std::process::exit(1);
                            }
                            Err(e) => {
                                eprintln!("Failed to connect to daemon: {e}");
                                std::process::exit(1);
                            }
                        },
                        "restrict" | "open" | "grant" | "revoke" => {
                            if matches!(action.as_str(), "grant" | "revoke") && peer.is_none() {
                                eprintln!("{action} needs --peer <peer-id>");
                                std::process::exit(1);
                            }
                            let payload = serde_json::json!({
                                "action": action,
                                "peer_id": peer,
                            });
                            match client.post(&url).json(&payload).send().await {
                                Ok(r) if r.status().is_success() => {
                                    let body: serde_json::Value =
                                        r.json().await.unwrap_or_default();
                                    let restricted = body
                                        .get("restricted")
                                        .and_then(|v| v.as_bool())
                                        .unwrap_or(false);
                                    println!(
                                        "Model {id} updated ({action}); restricted: {restricted}."
                                    );
                                }
                                Ok(r) => {
                                    eprintln!("Failed: HTTP {}", r.status());
                                    std::process::exit(1);
                                }
                                Err(e) => {
                                    eprintln!("Failed to connect to daemon: {e}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        other => {
                            eprintln!(
                                "Unknown acl action {other:?}; want show, restrict, open, grant, or revoke"
                            );
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Llm { action } => {
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();
            match action {
                LlmAction::Run {
                    model,
                    prompt,
                    prompt_file,
                    max_tokens,
                    temperature,
                    system,
                    timeout,
                    stream,
                } => {
                    let prompt_text = match (prompt, prompt_file) {
                        (Some(text), _) => text.clone(),
                        (None, Some(path)) => match std::fs::read_to_string(path) {
                            Ok(text) => text,
                            Err(e) => {
                                eprintln!("Failed to read prompt file: {e}");
                                std::process::exit(1);
                            }
                        },
                        (None, None) => {
                            eprintln!("Provide --prompt or --prompt-file");
                            std::process::exit(1);
                        }
                    };
                    let url = format!("{daemon_url}/api/models/{model}/infer");
                    let mut req = serde_json::json!({
                        "prompt": prompt_text,
                        "timeout_secs": timeout,
                    });
                    if let Some(n) = max_tokens {
                        req["max_tokens"] = serde_json::json!(n);
                    }
                    if let Some(t) = temperature {
                        req["temperature"] = serde_json::json!(t);
                    }
                    if let Some(s) = system {
                        req["system"] = serde_json::json!(s);
                    }
                    if *stream {
                        let url = format!("{daemon_url}/api/models/{model}/infer/stream");
                        stream_llm_completion(&client, &url, &req, model).await;
                        return Ok(());
                    }
                    match client.post(&url).json(&req).send().await {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            if json.get("status").and_then(|v| v.as_str()) == Some("fetching") {
                                eprintln!(
                                    "Model weights incomplete ({} blobs missing); run `gitforest model pull --id {model}` first",
                                    json.get("missing_blobs").and_then(|v| v.as_u64()).unwrap_or(0)
                                );
                                std::process::exit(1);
                            }
                            if json.get("proxied").and_then(|v| v.as_bool()) == Some(true) {
                                eprintln!(
                                    "served remotely by {}",
                                    json.get("served_by")
                                        .and_then(|v| v.as_str())
                                        .unwrap_or("a peer")
                                );
                            }
                            match json.get("completion").and_then(|v| v.as_str()) {
                                Some(completion) => println!("{completion}"),
                                None => {
                                    eprintln!("Daemon returned no completion: {json}");
                                    std::process::exit(1);
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Workflow { action } => {
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();
            match action {
                WorkflowAction::List { repo } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/workflows");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            let empty = Vec::new();
                            let workflows = json
                                .get("workflows")
                                .and_then(|v| v.as_array())
                                .unwrap_or(&empty);
                            if workflows.is_empty() {
                                println!("No workflows in {repo}");
                            }
                            for w in workflows {
                                let triggers = w
                                    .get("triggers")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|v| v.as_str())
                                            .collect::<Vec<_>>()
                                            .join(",")
                                    })
                                    .unwrap_or_default();
                                println!(
                                    "{} ({}) triggers=[{}]",
                                    w.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                                    w.get("file").and_then(|v| v.as_str()).unwrap_or("?"),
                                    triggers,
                                );
                            }
                            if let Some(errors) = json.get("errors").and_then(|v| v.as_array()) {
                                for e in errors {
                                    println!(
                                        "error in {}: {}",
                                        e.get("file").and_then(|v| v.as_str()).unwrap_or("?"),
                                        e.get("error").and_then(|v| v.as_str()).unwrap_or("?"),
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                WorkflowAction::Dispatch {
                    repo,
                    workflow,
                    branch,
                    tag,
                    commit,
                    inputs,
                } => {
                    let mut input_map = serde_json::Map::new();
                    for raw in inputs {
                        match raw.split_once('=') {
                            Some((key, value)) if !key.is_empty() => {
                                input_map.insert(
                                    key.to_string(),
                                    serde_json::Value::String(value.to_string()),
                                );
                            }
                            _ => {
                                eprintln!("Ignoring malformed --input (want KEY=VALUE): {raw}");
                            }
                        }
                    }
                    let url = format!("{daemon_url}/api/repos/{repo}/workflows/dispatch");
                    match client
                        .post(&url)
                        .json(&serde_json::json!({
                            "workflow": workflow,
                            "branch": branch,
                            "tag": tag,
                            "commit_sha": commit,
                            "inputs": input_map,
                        }))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            let empty = Vec::new();
                            let runs = json
                                .get("runs")
                                .and_then(|v| v.as_array())
                                .unwrap_or(&empty);
                            if runs.is_empty() {
                                println!("No workflows matched");
                            }
                            for run in runs {
                                println!(
                                    "Run {} ({})",
                                    run.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                                    run.get("workflow").and_then(|v| v.as_str()).unwrap_or("?"),
                                );
                                if let Some(jobs) = run.get("job_ids").and_then(|v| v.as_array()) {
                                    for j in jobs {
                                        println!("  job {}", j.as_str().unwrap_or("?"));
                                    }
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                WorkflowAction::Runs => {
                    let url = format!("{daemon_url}/api/workflows/runs");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let runs: Vec<serde_json::Value> = r.json().await.unwrap_or_default();
                            if runs.is_empty() {
                                println!("No workflow runs");
                            } else {
                                println!(
                                    "{:<36} {:<16} {:<16} {:<24}",
                                    "ID", "REPO", "WORKFLOW", "SUMMARY"
                                );
                                for run in runs {
                                    let summary = run.get("summary");
                                    println!(
                                        "{:<36} {:<16} {:<16} {}/{} done, {} failed",
                                        run.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                                        run.get("repo").and_then(|v| v.as_str()).unwrap_or("?"),
                                        run.get("workflow").and_then(|v| v.as_str()).unwrap_or("?"),
                                        summary
                                            .and_then(|s| s.get("completed"))
                                            .map(|v| v.to_string())
                                            .unwrap_or("?".into()),
                                        summary
                                            .and_then(|s| s.get("total"))
                                            .map(|v| v.to_string())
                                            .unwrap_or("?".into()),
                                        summary
                                            .and_then(|s| s.get("failed"))
                                            .map(|v| v.to_string())
                                            .unwrap_or("?".into()),
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                WorkflowAction::Show { run_id } => {
                    let url = format!("{daemon_url}/api/workflows/runs/{run_id}");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let run: serde_json::Value = r.json().await.unwrap_or_default();
                            println!(
                                "Run {} ({})",
                                run.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                                run.get("workflow").and_then(|v| v.as_str()).unwrap_or("?"),
                            );
                            println!(
                                "  Repo: {}  Event: {}  Commit: {}",
                                run.get("repo").and_then(|v| v.as_str()).unwrap_or("?"),
                                run.get("event").and_then(|v| v.as_str()).unwrap_or("?"),
                                run.get("commit_sha")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?"),
                            );
                            if let Some(jobs) = run.get("jobs").and_then(|v| v.as_array()) {
                                for j in jobs {
                                    println!(
                                        "  [{}] {} ({})",
                                        j.get("status").and_then(|v| v.as_str()).unwrap_or("?"),
                                        j.get("name").and_then(|v| v.as_str()).unwrap_or("?"),
                                        j.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                                    );
                                }
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                WorkflowAction::Rerun { run_id } => {
                    let url = format!("{daemon_url}/api/workflows/runs/{run_id}/rerun");
                    match client.post(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let body: serde_json::Value = r.json().await.unwrap_or_default();
                            let runs = body.get("runs").and_then(|v| v.as_array());
                            match runs.and_then(|r| r.first()) {
                                Some(run) => println!(
                                    "Rerun {} ({} jobs) of {run_id}",
                                    run.get("id").and_then(|v| v.as_str()).unwrap_or("?"),
                                    run.get("job_ids")
                                        .and_then(|v| v.as_array())
                                        .map(|a| a.len())
                                        .unwrap_or(0),
                                ),
                                None => println!("No failed jobs to rerun in {run_id}"),
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }

        Commands::Secrets { repo, action } => {
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();
            match action {
                SecretsAction::Set {
                    key,
                    value,
                    from_file,
                } => {
                    let secret = match (value, from_file) {
                        (Some(v), None) => v.clone(),
                        (None, Some(path)) => match std::fs::read_to_string(path) {
                            Ok(text) => text.trim_end_matches(['\n', '\r']).to_string(),
                            Err(e) => {
                                eprintln!("Cannot read secret file: {e}");
                                std::process::exit(1);
                            }
                        },
                        _ => {
                            eprintln!("Provide a value or --from-file (exactly one)");
                            std::process::exit(1);
                        }
                    };
                    let url = format!("{daemon_url}/api/repos/{repo}/secrets");
                    match client
                        .post(&url)
                        .json(&serde_json::json!({ "key": key, "value": secret }))
                        .send()
                        .await
                    {
                        Ok(r) if r.status().is_success() => {
                            println!("Stored secret {key} for {repo}");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                SecretsAction::List => {
                    let url = format!("{daemon_url}/api/repos/{repo}/secrets");
                    match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let json: serde_json::Value = r.json().await.unwrap_or_default();
                            let empty = Vec::new();
                            let names = json
                                .get("names")
                                .and_then(|v| v.as_array())
                                .unwrap_or(&empty);
                            if names.is_empty() {
                                println!("No secrets for {repo}");
                            }
                            for name in names {
                                println!("{}", name.as_str().unwrap_or("?"));
                            }
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }

                SecretsAction::Delete { key } => {
                    let url = format!("{daemon_url}/api/repos/{repo}/secrets/{key}");
                    match client.delete(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            println!("Deleted secret {key} from {repo}");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            eprintln!("Make sure the daemon is running (gitforest daemon)");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
        Commands::Cache { action } => {
            let config = load_config(&cli.config)?;
            let daemon_url = daemon_base_url(&config);
            let client = daemon_client();
            match action {
                CacheAction::Save { key, path } => {
                    if !path.exists() {
                        eprintln!("Nothing to cache: {} does not exist", path.display());
                        std::process::exit(1);
                    }
                    let archive = std::env::temp_dir().join(format!(
                        "gf-cache-{}.tgz",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    ));
                    let mut tar = std::process::Command::new("tar");
                    tar.arg("-czf").arg(&archive);
                    match path.file_name().and_then(|n| n.to_str()) {
                        Some(name) => {
                            let parent = path
                                .parent()
                                .map(PathBuf::from)
                                .filter(|p| !p.as_os_str().is_empty())
                                .unwrap_or_else(|| PathBuf::from("."));
                            tar.arg("-C").arg(&parent).arg(name);
                        }
                        None => {
                            tar.arg("-C").arg(path).arg(".");
                        }
                    }
                    let packed = tar.status();
                    match packed {
                        Ok(status) if status.success() => {}
                        _ => {
                            eprintln!("tar failed to archive {}", path.display());
                            std::process::exit(1);
                        }
                    }
                    let bytes = match std::fs::read(&archive) {
                        Ok(b) => b,
                        Err(e) => {
                            eprintln!("Cannot read cache archive: {e}");
                            let _ = std::fs::remove_file(&archive);
                            std::process::exit(1);
                        }
                    };
                    let _ = std::fs::remove_file(&archive);
                    let upload_url = format!("{daemon_url}/api/blobs/upload");
                    let blob_id = match client.post(&upload_url).body(bytes).send().await {
                        Ok(r) if r.status().is_success() => {
                            let body: serde_json::Value = r.json().await.unwrap_or_default();
                            body.get("blob_id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string()
                        }
                        Ok(r) => {
                            eprintln!("Cache upload failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            std::process::exit(1);
                        }
                    };
                    if blob_id.is_empty() {
                        eprintln!("Cache upload returned no blob id");
                        std::process::exit(1);
                    }
                    let url = format!("{daemon_url}/api/caches");
                    let payload = serde_json::json!({
                        "key": key,
                        "blob_id": blob_id,
                    });
                    let result = client.post(&url).json(&payload).send().await;
                    match result {
                        Ok(r) if r.status().is_success() => {
                            let body: serde_json::Value = r.json().await.unwrap_or_default();
                            let status = body.get("status").and_then(|v| v.as_str()).unwrap_or("?");
                            println!("Cache {key}: {status}");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            std::process::exit(1);
                        }
                    }
                }
                CacheAction::Restore {
                    key,
                    restore_keys,
                    path,
                } => {
                    let mut found: Option<serde_json::Value> = None;
                    let mut exact = true;
                    let direct = format!("{daemon_url}/api/caches/{key}");
                    match client.get(&direct).send().await {
                        Ok(r) if r.status().is_success() => {
                            found = r.json().await.ok();
                        }
                        Ok(_) => {}
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            std::process::exit(1);
                        }
                    }
                    if found.is_none() {
                        if let Some(prefixes) = restore_keys {
                            for prefix in
                                prefixes.split(',').map(str::trim).filter(|p| !p.is_empty())
                            {
                                let url = format!("{daemon_url}/api/caches?prefix={prefix}");
                                match client.get(&url).send().await {
                                    Ok(r) if r.status().is_success() => {
                                        found = r.json().await.ok();
                                        exact = false;
                                        break;
                                    }
                                    Ok(_) => {}
                                    Err(e) => {
                                        eprintln!("Failed to connect to daemon: {e}");
                                        std::process::exit(1);
                                    }
                                }
                            }
                        }
                    }
                    let Some(entry) = found else {
                        println!("cache-hit=false");
                        return Ok(());
                    };
                    let blob_id = entry
                        .get("blob_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let hit_key = entry
                        .get("key")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let from_peer = entry
                        .get("source")
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| s == "peer");
                    let peer = entry
                        .get("peer")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let url = format!("{daemon_url}/api/blobs/{blob_id}/download");
                    let bytes = match client.get(&url).send().await {
                        Ok(r) if r.status().is_success() => r.bytes().await.unwrap_or_default(),
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            std::process::exit(1);
                        }
                    };
                    let dest: PathBuf = match path.file_name() {
                        Some(_) => path
                            .parent()
                            .map(PathBuf::from)
                            .filter(|p| !p.as_os_str().is_empty())
                            .unwrap_or_else(|| PathBuf::from(".")),
                        None => path.clone(),
                    };
                    if let Err(e) = std::fs::create_dir_all(&dest) {
                        eprintln!("Cannot create {}: {e}", dest.display());
                        std::process::exit(1);
                    }
                    let archive = std::env::temp_dir().join(format!(
                        "gf-restore-{}.tgz",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_nanos()
                    ));
                    if std::fs::write(&archive, &bytes).is_err() {
                        eprintln!("Cannot stage cache download");
                        std::process::exit(1);
                    }
                    let unpacked = std::process::Command::new("tar")
                        .arg("-xzf")
                        .arg(&archive)
                        .arg("-C")
                        .arg(&dest)
                        .status();
                    let _ = std::fs::remove_file(&archive);
                    match unpacked {
                        Ok(status) if status.success() => {}
                        _ => {
                            eprintln!("tar failed to extract into {}", path.display());
                            std::process::exit(1);
                        }
                    }
                    if exact {
                        println!("cache-hit=true");
                    } else {
                        println!("cache-hit=false");
                    }
                    println!("restored-from-peer={from_peer}");
                    if from_peer {
                        eprintln!("Restored {hit_key} from peer {peer} at {}", path.display());
                    } else {
                        eprintln!("Restored {hit_key} at {}", path.display());
                    }
                }
                CacheAction::Delete { key } => {
                    let url = format!("{daemon_url}/api/caches/{key}");
                    match client.delete(&url).send().await {
                        Ok(r) if r.status().is_success() => {
                            let freed: u64 = r
                                .json::<serde_json::Value>()
                                .await
                                .ok()
                                .and_then(|v| v.get("bytes_freed")?.as_u64())
                                .unwrap_or(0);
                            println!("deleted {key} (freed {freed} bytes)");
                        }
                        Ok(r) => {
                            eprintln!("Failed: HTTP {}", r.status());
                            std::process::exit(1);
                        }
                        Err(e) => {
                            eprintln!("Failed to connect to daemon: {e}");
                            std::process::exit(1);
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

fn load_config(path: &Option<PathBuf>) -> Result<Config> {
    match path {
        Some(p) => Config::load(p).map_err(Into::into),
        None => match std::env::var("GITFOREST_CONFIG").ok().map(PathBuf::from) {
            Some(p) => Config::load(&p).map_err(Into::into),
            None => Config::load_or_default().map_err(Into::into),
        },
    }
}

fn daemon_pidfile() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .ok()
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("gitforest.pid")
}

fn install_panic_hook(data_dir: &std::path::Path) {
    let _ = std::fs::create_dir_all(data_dir);
    let log_path = data_dir.join("panics.log");
    std::panic::set_hook(Box::new(move |info| {
        let payload = info.payload();
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| payload.downcast_ref::<&str>().map(|s| s.to_string()))
            .unwrap_or_else(|| String::from("non-string panic payload"));
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| String::from("unknown location"));
        let trace = std::backtrace::Backtrace::force_capture();
        tracing::error!("panic: {msg} at {location}\n{trace}");
        let entry = format!("panic: {msg} at {location}\n{trace}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
            .and_then(|mut f| {
                use std::io::Write;
                f.write_all(entry.as_bytes())
            });
    }));
}

fn load_identity(cli: &Cli) -> Result<Identity> {
    let key_path = cli
        .identity
        .as_ref()
        .cloned()
        .unwrap_or_else(identity_key_path);
    if !key_path.exists() {
        eprintln!(
            "No identity found at {}. Run `gitforest init` first.",
            key_path.display()
        );
        std::process::exit(1);
    }
    Identity::load(&key_path).map_err(Into::into)
}

fn select_base_url(job_url: Option<&str>, config_port: u16) -> String {
    if let Some(url) = job_url.map(str::trim).filter(|u| !u.is_empty()) {
        return url.trim_end_matches('/').to_string();
    }
    format!("http://127.0.0.1:{config_port}")
}

fn select_api_token(job_token: Option<&str>, file_token: Option<String>) -> Option<String> {
    if let Some(tok) = job_token.map(str::trim).filter(|t| !t.is_empty()) {
        return Some(tok.to_string());
    }
    file_token.filter(|t| !t.is_empty())
}

fn daemon_base_url(config: &gt_core::config::Config) -> String {
    select_base_url(std::env::var("GF_API_URL").ok().as_deref(), config.api_port)
}

fn daemon_api_token() -> Option<String> {
    let path = std::env::var("HOME")
        .ok()
        .map(|h| std::path::PathBuf::from(h).join(".local/share/gitforest/api.token"))
        .unwrap_or_else(|| std::env::temp_dir().join("gitforest-api.token"));
    let file_token = std::fs::read_to_string(path)
        .ok()
        .map(|t| t.trim().to_string());
    select_api_token(std::env::var("GF_JOB_TOKEN").ok().as_deref(), file_token)
}

fn read_secret_file(path: &std::path::Path) -> anyhow::Result<Vec<u8>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(path)?.permissions().mode() & 0o777;
        if mode & 0o077 != 0 {
            anyhow::bail!(
                "{} is mode {:o}; secrets must not be group/world-accessible (chmod 600)",
                path.display(),
                mode
            );
        }
    }
    Ok(std::fs::read(path)?)
}

fn dispatch_llm_stream_event(payload: &str, model: &str, saw_done: &mut bool, wrote: &mut bool) {
    let event: serde_json::Value = match serde_json::from_str(payload) {
        Ok(event) => event,
        Err(_) => return,
    };
    if let Some(chunk) = event.get("chunk").and_then(|v| v.as_str()) {
        print!("{chunk}");
        *wrote = true;
    } else if let Some(done) = event.get("done") {
        *saw_done = true;
        if done.get("truncated").and_then(|v| v.as_bool()) == Some(true) {
            eprintln!("warning: completion truncated at the daemon output limit");
        }
    } else if let Some(message) = event.get("error").and_then(|v| v.as_str()) {
        eprintln!("Inference failed: {message}");
        std::process::exit(1);
    } else if event.get("status").and_then(|v| v.as_str()) == Some("fetching") {
        eprintln!(
            "Model weights incomplete ({} blobs missing); run `gitforest model pull --id {model}` first",
            event
                .get("missing_blobs")
                .and_then(|v| v.as_u64())
                .unwrap_or(0)
        );
        std::process::exit(1);
    }
}

async fn stream_llm_completion(
    client: &reqwest::Client,
    url: &str,
    req: &serde_json::Value,
    model: &str,
) {
    use std::io::Write as _;
    let mut resp = match client.post(url).json(req).send().await {
        Ok(r) if r.status().is_success() => r,
        Ok(r) => {
            eprintln!("Failed: HTTP {}", r.status());
            std::process::exit(1);
        }
        Err(e) => {
            eprintln!("Failed to connect to daemon: {e}");
            eprintln!("Make sure the daemon is running (gitforest daemon)");
            std::process::exit(1);
        }
    };
    let mut buf = String::new();
    let mut data_lines: Vec<String> = Vec::new();
    let mut saw_done = false;
    let mut wrote = false;
    loop {
        match resp.chunk().await {
            Ok(Some(bytes)) => buf.push_str(&String::from_utf8_lossy(&bytes)),
            Ok(None) => break,
            Err(e) => {
                eprintln!("Stream failed: {e}");
                std::process::exit(1);
            }
        }
        while let Some(pos) = buf.find('\n') {
            let line = buf[..pos].trim_end_matches('\r').to_string();
            buf.drain(..=pos);
            if line.is_empty() {
                if !data_lines.is_empty() {
                    let payload = data_lines.join("\n");
                    data_lines.clear();
                    dispatch_llm_stream_event(&payload, model, &mut saw_done, &mut wrote);
                    let _ = std::io::stdout().flush();
                }
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            if let Some(payload) = line.strip_prefix("data:") {
                data_lines.push(payload.trim_start().to_string());
            }
        }
    }
    if !data_lines.is_empty() {
        let payload = data_lines.join("\n");
        dispatch_llm_stream_event(&payload, model, &mut saw_done, &mut wrote);
    }
    if !saw_done {
        eprintln!("Stream ended without a completion");
        std::process::exit(1);
    }
    if wrote {
        println!();
    }
}

fn daemon_client() -> reqwest::Client {
    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(token) = daemon_api_token() {
        if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}")) {
            headers.insert(reqwest::header::AUTHORIZATION, value);
        }
    }
    reqwest::Client::builder()
        .no_proxy()
        .default_headers(headers)
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = 1024 * KB;
    const GB: u64 = 1024 * MB;
    const TB: u64 = 1024 * GB;

    if bytes >= TB {
        format!("{:.1} TB", bytes as f64 / TB as f64)
    } else if bytes >= GB {
        format!("{:.1} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.1} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.1} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn job_env_wins_over_config_for_url_and_token() {
        assert_eq!(
            select_base_url(Some("http://10.0.0.2:7878/"), 9999),
            "http://10.0.0.2:7878"
        );
        assert_eq!(select_base_url(Some("  "), 7878), "http://127.0.0.1:7878");
        assert_eq!(select_base_url(None, 7878), "http://127.0.0.1:7878");
        assert_eq!(
            select_api_token(Some("gfj.job"), Some("file-token".to_string())),
            Some("gfj.job".to_string())
        );
        assert_eq!(
            select_api_token(Some(""), Some("file-token".to_string())),
            Some("file-token".to_string())
        );
        assert_eq!(
            select_api_token(None, Some("file-token".to_string())),
            Some("file-token".to_string())
        );
        assert_eq!(select_api_token(None, None), None);
    }
}
