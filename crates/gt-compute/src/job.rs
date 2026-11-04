use chrono::{DateTime, Utc};
use gt_core::identity::{verify_signature, Identity, PeerId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeJob {
    pub id: String,
    pub submitter: PeerId,
    pub repo_owner: PeerId,
    pub repo_name: String,
    pub commit_sha: String,
    pub job_config: JobConfig,
    pub requirements: ComputeRequirements,
    pub max_payment_sats: u64,
    pub preferred_chain: String,
    #[serde(default)]
    pub max_payment_units: Option<u64>,
    #[serde(default)]
    pub verify: bool,
    #[serde(default)]
    pub run_if: RunCondition,
    #[serde(default)]
    pub workflow_job: String,
    #[serde(default)]
    pub matrix_siblings: Vec<String>,
    #[serde(default)]
    pub fail_fast: bool,
    #[serde(default)]
    pub concurrency_group: String,
    #[serde(default)]
    pub cancel_in_progress: bool,
    #[serde(default)]
    pub environment: String,
    #[serde(default)]
    pub environment_url: String,
    #[serde(default)]
    pub env_approval_pending: bool,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub max_parallel: u32,
    #[serde(default)]
    pub matrix: HashMap<String, String>,
    #[serde(default)]
    pub dispatch_inputs: HashMap<String, String>,
    #[serde(default)]
    pub needs: Vec<String>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub call_outputs: HashMap<String, String>,
    #[serde(default)]
    pub permissions: crate::permissions::Permissions,
    pub status: JobStatus,
    pub created_at: DateTime<Utc>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobConfig {
    pub name: String,
    pub steps: Vec<JobStep>,
    pub docker_image: String,
    pub timeout_secs: u64,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    #[serde(default)]
    pub services: Vec<JobService>,
    #[serde(default)]
    pub container: Option<JobContainer>,
    #[serde(default)]
    pub job_token: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobContainerCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobVolume {
    pub source: Option<String>,
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ContainerResources {
    #[serde(default)]
    pub nano_cpus: Option<i64>,
    #[serde(default)]
    pub memory_bytes: Option<i64>,
    #[serde(default)]
    pub shm_bytes: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobContainer {
    pub image: String,
    #[serde(default)]
    pub credentials: Option<JobContainerCredentials>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    #[serde(default)]
    pub volumes: Vec<JobVolume>,
    #[serde(default)]
    pub resources: ContainerResources,
    #[serde(default)]
    pub options: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PortBinding {
    pub container: u16,
    #[serde(default)]
    pub host: Option<u16>,
    #[serde(default)]
    pub ip: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceHealth {
    pub cmd: String,
    pub interval_secs: u64,
    pub timeout_secs: u64,
    pub retries: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct JobService {
    pub name: String,
    pub image: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<PortBinding>,
    #[serde(default)]
    pub health: Option<ServiceHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStep {
    pub name: String,
    pub run: String,
    #[serde(default)]
    pub condition: RunCondition,
    #[serde(default)]
    pub continue_on_error: bool,
    #[serde(default)]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum RunCondition {
    #[default]
    Success,
    Failure,
    Always,
    Cancelled,
    Expr(String),
}

impl RunCondition {
    pub fn name(&self) -> &'static str {
        match self {
            RunCondition::Success => "success()",
            RunCondition::Failure => "failure()",
            RunCondition::Always => "always()",
            RunCondition::Cancelled => "cancelled()",
            RunCondition::Expr(_) => "if",
        }
    }

    pub fn parse_full(value: &str) -> Self {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return RunCondition::Success;
        }
        if let Some(known) = RunCondition::parse(trimmed) {
            return known;
        }
        RunCondition::Expr(trimmed.to_string())
    }

    pub fn parse(value: &str) -> Option<Self> {
        let cleaned = value.trim().to_lowercase();
        let name = cleaned
            .strip_suffix("()")
            .map(str::trim)
            .unwrap_or(cleaned.as_str());
        match name {
            "success" => Some(RunCondition::Success),
            "failure" => Some(RunCondition::Failure),
            "always" => Some(RunCondition::Always),
            "cancelled" => Some(RunCondition::Cancelled),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum JobStatus {
    Pending,
    Blocked {
        needs: Vec<String>,
    },
    Matched {
        worker: PeerId,
        matched_at: DateTime<Utc>,
    },
    Verifying {
        first_worker: PeerId,
        candidate_result_id: String,
        verifier: Option<PeerId>,
        matched_at: DateTime<Utc>,
    },
    Running {
        worker: PeerId,
        started_at: DateTime<Utc>,
    },
    Completed {
        result_id: String,
    },
    Failed {
        reason: String,
    },
    Disputed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeRequirements {
    pub min_cpu_cores: u32,
    pub min_ram_mb: u64,
    pub min_disk_mb: u64,
    #[serde(default)]
    pub gpu_required: bool,
    #[serde(default)]
    pub network_access: bool,
}

impl Default for ComputeRequirements {
    fn default() -> Self {
        ComputeRequirements {
            min_cpu_cores: 1,
            min_ram_mb: 512,
            min_disk_mb: 1024,
            gpu_required: false,
            network_access: false,
        }
    }
}

impl ComputeJob {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: &Identity,
        repo_owner: PeerId,
        repo_name: String,
        commit_sha: String,
        job_config: JobConfig,
        requirements: ComputeRequirements,
        max_payment_sats: u64,
        preferred_chain: String,
    ) -> Self {
        let id = uuid::Uuid::new_v4().to_string();
        let created_at = Utc::now();

        let mut job = ComputeJob {
            id,
            submitter: identity.peer_id(),
            repo_owner,
            repo_name,
            commit_sha,
            job_config,
            requirements,
            max_payment_sats,
            preferred_chain,
            max_payment_units: None,
            verify: false,
            run_if: RunCondition::Success,
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
            matrix: HashMap::new(),
            dispatch_inputs: HashMap::new(),
            needs: Vec::new(),
            call_id: None,
            call_outputs: HashMap::new(),
            permissions: crate::permissions::Permissions::none(),
            status: JobStatus::Pending,
            created_at,
            signature: vec![],
        };

        let payload = job.signable_payload();
        job.signature = identity.sign(&payload);
        job
    }

    pub fn with_payment_units(mut self, identity: &Identity, units: u64) -> Self {
        self.max_payment_units = Some(units);
        let payload = self.signable_payload();
        self.signature = identity.sign(&payload);
        self
    }

    pub fn payment_units(&self) -> u64 {
        self.max_payment_units.unwrap_or(self.max_payment_sats)
    }

    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:compute-job:");
        hasher.update(&self.id);
        hasher.update(self.submitter.0);
        hasher.update(self.repo_owner.0);
        hasher.update(&self.repo_name);
        hasher.update(&self.commit_sha);
        hasher.update(serde_json::to_string(&self.job_config).unwrap_or_default());
        hasher.update(self.max_payment_sats.to_le_bytes());
        hasher.update(self.max_payment_units.unwrap_or(0).to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        let payload = self.signable_payload();
        verify_signature(&self.submitter, &payload, &self.signature).is_ok()
    }
}

/// Parse a .gitforest-ci.yml file into a JobConfig.
pub fn parse_ci_config(yaml_content: &str) -> anyhow::Result<JobConfig> {
    let config: JobConfig = serde_yaml::from_str(yaml_content)?;
    Ok(config)
}

/// In-memory store for compute jobs.
pub struct ComputeJobStore {
    jobs: HashMap<String, ComputeJob>,
}

impl Default for ComputeJobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl ComputeJobStore {
    pub fn new() -> Self {
        ComputeJobStore {
            jobs: HashMap::new(),
        }
    }

    pub fn add(&mut self, job: ComputeJob) {
        self.jobs.insert(job.id.clone(), job);
    }

    pub fn remove(&mut self, id: &str) -> Option<ComputeJob> {
        self.jobs.remove(id)
    }

    pub fn get(&self, id: &str) -> Option<&ComputeJob> {
        self.jobs.get(id)
    }

    pub fn get_mut(&mut self, id: &str) -> Option<&mut ComputeJob> {
        self.jobs.get_mut(id)
    }

    pub fn list(&self) -> Vec<&ComputeJob> {
        self.jobs.values().collect()
    }

    pub fn pending(&self) -> Vec<&ComputeJob> {
        self.jobs
            .values()
            .filter(|j| matches!(j.status, JobStatus::Pending))
            .collect()
    }

    pub fn for_repo(&self, repo_name: &str) -> Vec<&ComputeJob> {
        self.jobs
            .values()
            .filter(|j| j.repo_name == repo_name)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    #[test]
    fn test_create_and_verify_job() {
        let submitter = Identity::generate();
        let owner = Identity::generate();

        let config = JobConfig {
            name: "Build and Test".into(),
            steps: vec![
                JobStep {
                    name: "Build".into(),
                    run: "cargo build".into(),
                    condition: RunCondition::Success,
                    continue_on_error: false,
                    working_directory: None,
                    id: None,
                },
                JobStep {
                    name: "Test".into(),
                    run: "cargo test".into(),
                    condition: RunCondition::Success,
                    continue_on_error: false,
                    working_directory: None,
                    id: None,
                },
            ],
            docker_image: "rust:1.75".into(),
            timeout_secs: 600,
            env: HashMap::new(),
            secrets: HashMap::new(),
            artifacts: vec!["target/release/myapp".into()],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };

        let job = ComputeJob::new(
            &submitter,
            owner.peer_id(),
            "myrepo".into(),
            "abc123".into(),
            config,
            ComputeRequirements::default(),
            10000,
            "lightning".into(),
        );

        assert!(job.verify());
        assert_eq!(job.submitter, submitter.peer_id());
        assert!(matches!(job.status, JobStatus::Pending));
    }

    #[test]
    fn test_tampered_job_fails() {
        let submitter = Identity::generate();
        let owner = Identity::generate();

        let config = JobConfig {
            name: "Test".into(),
            steps: vec![],
            docker_image: "node:20".into(),
            timeout_secs: 300,
            env: HashMap::new(),
            secrets: HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };

        let mut job = ComputeJob::new(
            &submitter,
            owner.peer_id(),
            "repo".into(),
            "def456".into(),
            config,
            ComputeRequirements::default(),
            5000,
            "lightning".into(),
        );

        job.max_payment_sats = 999999; // tamper
        assert!(!job.verify());
    }

    #[test]
    fn test_parse_ci_config() {
        let yaml = r#"
name: Build and Test
docker_image: rust:1.75
timeout_secs: 600
steps:
  - name: Build
    run: cargo build --release
  - name: Test
    run: cargo test
artifacts:
  - target/release/myapp
"#;
        let config = parse_ci_config(yaml).unwrap();
        assert_eq!(config.name, "Build and Test");
        assert_eq!(config.steps.len(), 2);
        assert_eq!(config.docker_image, "rust:1.75");
        assert_eq!(config.timeout_secs, 600);
    }

    #[test]
    fn test_job_store() {
        let submitter = Identity::generate();
        let owner = Identity::generate();

        let config = JobConfig {
            name: "Test".into(),
            steps: vec![],
            docker_image: "alpine".into(),
            timeout_secs: 60,
            env: HashMap::new(),
            secrets: HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };

        let job = ComputeJob::new(
            &submitter,
            owner.peer_id(),
            "repo1".into(),
            "sha1".into(),
            config,
            ComputeRequirements::default(),
            1000,
            "lightning".into(),
        );

        let id = job.id.clone();
        let mut store = ComputeJobStore::new();
        store.add(job);

        assert!(store.get(&id).is_some());
        assert_eq!(store.list().len(), 1);
        assert_eq!(store.pending().len(), 1);
        assert_eq!(store.for_repo("repo1").len(), 1);
        assert_eq!(store.for_repo("other").len(), 0);
    }
}
