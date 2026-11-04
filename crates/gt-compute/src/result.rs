use chrono::{DateTime, Utc};
use gt_core::identity::{verify_signature, Identity, PeerId};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// Result of a completed compute job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobResult {
    pub id: String,
    pub job_id: String,
    pub worker_id: PeerId,
    pub exit_code: i32,
    pub stdout_hash: String,
    pub stderr_hash: String,
    pub stdout_tail: String,
    pub stderr_tail: String,
    pub artifact_hashes: Vec<(String, String)>,
    #[serde(default)]
    pub artifact_blobs: Vec<gt_core::types::ArtifactBlob>,
    pub execution_time_ms: u64,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    #[serde(default)]
    pub summary_md: String,
    pub resource_usage: ResourceUsage,
    pub completed_at: DateTime<Utc>,
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub peak_cpu_percent: f32,
    pub peak_ram_mb: u64,
    pub disk_used_mb: u64,
}

impl JobResult {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identity: &Identity,
        job_id: String,
        exit_code: i32,
        stdout: &[u8],
        stderr: &[u8],
        artifacts: Vec<(String, Vec<u8>)>,
        execution_time_ms: u64,
        resource_usage: ResourceUsage,
    ) -> Self {
        let stdout_hash = hex::encode(Sha256::digest(stdout));
        let stderr_hash = hex::encode(Sha256::digest(stderr));
        let artifact_hashes: Vec<(String, String)> = artifacts
            .iter()
            .map(|(name, data)| (name.clone(), hex::encode(Sha256::digest(data))))
            .collect();

        let mut result = JobResult {
            id: uuid::Uuid::new_v4().to_string(),
            job_id,
            worker_id: identity.peer_id(),
            exit_code,
            stdout_hash,
            stderr_hash,
            stdout_tail: tail_text(stdout),
            stderr_tail: tail_text(stderr),
            artifact_hashes,
            artifact_blobs: Vec::new(),
            execution_time_ms,
            outputs: HashMap::new(),
            summary_md: String::new(),
            resource_usage,
            completed_at: Utc::now(),
            signature: vec![],
        };

        let payload = result.signable_payload();
        result.signature = identity.sign(&payload);
        result
    }

    pub fn set_outputs(&mut self, identity: &Identity, outputs: HashMap<String, String>) {
        self.outputs = outputs;
        let payload = self.signable_payload();
        self.signature = identity.sign(&payload);
    }

    pub fn set_summary(&mut self, summary_md: String) {
        self.summary_md = summary_md;
    }

    pub fn set_artifact_blobs(
        &mut self,
        identity: &Identity,
        blobs: Vec<gt_core::types::ArtifactBlob>,
    ) {
        self.artifact_blobs = blobs;
        let payload = self.signable_payload();
        self.signature = identity.sign(&payload);
    }

    pub fn signable_payload(&self) -> Vec<u8> {
        let mut hasher = Sha256::new();
        hasher.update(b"gitforest:job-result:");
        hasher.update(&self.id);
        hasher.update(&self.job_id);
        hasher.update(self.worker_id.0);
        hasher.update(self.exit_code.to_le_bytes());
        hasher.update(&self.stdout_hash);
        hasher.update(&self.stderr_hash);
        hasher.update(&self.stdout_tail);
        hasher.update(&self.stderr_tail);
        for (name, hash) in &self.artifact_hashes {
            hasher.update(name);
            hasher.update(hash);
        }
        let mut blobs: Vec<&gt_core::types::ArtifactBlob> = self.artifact_blobs.iter().collect();
        blobs.sort_by(|a, b| a.name.cmp(&b.name));
        for blob in blobs {
            hasher.update(&blob.name);
            hasher.update(blob.blob_id.0);
            hasher.update(blob.size.to_le_bytes());
        }
        let mut outputs: Vec<(&String, &String)> = self.outputs.iter().collect();
        outputs.sort_by(|a, b| a.0.cmp(b.0));
        for (key, value) in outputs {
            hasher.update(key);
            hasher.update([0]);
            hasher.update(value);
            hasher.update([0]);
        }
        hasher.update(self.execution_time_ms.to_le_bytes());
        hasher.update(self.resource_usage.peak_cpu_percent.to_le_bytes());
        hasher.update(self.resource_usage.peak_ram_mb.to_le_bytes());
        hasher.update(self.resource_usage.disk_used_mb.to_le_bytes());
        hasher.finalize().to_vec()
    }

    pub fn verify(&self) -> bool {
        verify_signature(&self.worker_id, &self.signable_payload(), &self.signature).is_ok()
    }

    pub fn succeeded(&self) -> bool {
        self.exit_code == 0
    }
}

/// Compare two independent runs of the same job for deterministic replay.
/// Matches on exit code and full output hashes; tails are excerpts and
/// timing/resource usage legitimately vary, so they are excluded.
pub fn results_match(a: &JobResult, b: &JobResult) -> bool {
    a.exit_code == b.exit_code
        && a.stdout_hash == b.stdout_hash
        && a.stderr_hash == b.stderr_hash
        && a.outputs == b.outputs
}

pub fn tail_text(data: &[u8]) -> String {
    let text = String::from_utf8_lossy(data);
    let chars: Vec<char> = text.chars().collect();
    if chars.len() > 8192 {
        chars[chars.len() - 8192..].iter().collect()
    } else {
        text.into_owned()
    }
}

/// In-memory store for job results.
pub struct JobResultStore {
    results: HashMap<String, JobResult>,
    by_job: HashMap<String, String>,
}

impl Default for JobResultStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JobResultStore {
    pub fn new() -> Self {
        JobResultStore {
            results: HashMap::new(),
            by_job: HashMap::new(),
        }
    }

    pub fn add(&mut self, result: JobResult) {
        self.by_job.insert(result.job_id.clone(), result.id.clone());
        self.results.insert(result.id.clone(), result);
    }

    pub fn get(&self, id: &str) -> Option<&JobResult> {
        self.results.get(id)
    }

    pub fn for_job(&self, job_id: &str) -> Option<&JobResult> {
        self.by_job.get(job_id).and_then(|id| self.results.get(id))
    }

    pub fn list(&self) -> Vec<&JobResult> {
        self.results.values().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_core::identity::Identity;

    #[test]
    fn test_create_and_verify_result() {
        let worker = Identity::generate();

        let result = JobResult::new(
            &worker,
            "job-123".into(),
            0,
            b"Build succeeded\n",
            b"",
            vec![("binary".into(), b"ELF...".to_vec())],
            45000,
            ResourceUsage {
                peak_cpu_percent: 85.0,
                peak_ram_mb: 2048,
                disk_used_mb: 500,
            },
        );

        assert!(result.verify());
        assert!(result.succeeded());
        assert_eq!(result.worker_id, worker.peer_id());
        assert_eq!(result.artifact_hashes.len(), 1);
    }

    #[test]
    fn test_summary_is_display_only_and_unsigned() {
        let worker = Identity::generate();
        let mut result = JobResult::new(
            &worker,
            "job-123".into(),
            0,
            b"ok\n",
            b"",
            vec![],
            1000,
            ResourceUsage {
                peak_cpu_percent: 1.0,
                peak_ram_mb: 64,
                disk_used_mb: 8,
            },
        );
        assert!(result.verify());
        result.set_summary("## Deploy\n- sha abc".to_string());
        assert_eq!(result.summary_md, "## Deploy\n- sha abc");
        assert!(result.verify());
    }

    #[test]
    fn test_artifact_blobs_are_signed() {
        let worker = Identity::generate();
        let mut result = JobResult::new(
            &worker,
            "job-123".into(),
            0,
            b"out",
            b"",
            vec![],
            10,
            ResourceUsage {
                peak_cpu_percent: 0.0,
                peak_ram_mb: 0,
                disk_used_mb: 0,
            },
        );
        result.set_artifact_blobs(
            &worker,
            vec![gt_core::types::ArtifactBlob {
                name: "dist/app".into(),
                blob_id: gt_core::types::BlobId([7u8; 32]),
                size: 99,
            }],
        );
        assert!(result.verify());
        result.artifact_blobs[0].size = 100;
        assert!(!result.verify());
    }

    #[test]
    fn test_tampered_result_fails() {
        let worker = Identity::generate();

        let mut result = JobResult::new(
            &worker,
            "job-456".into(),
            0,
            b"output",
            b"",
            vec![],
            1000,
            ResourceUsage {
                peak_cpu_percent: 10.0,
                peak_ram_mb: 256,
                disk_used_mb: 50,
            },
        );

        result.exit_code = 1; // tamper
        assert!(!result.verify());
        result.exit_code = 0;
        assert!(result.verify());
        result.resource_usage.peak_ram_mb = 257; // tamper with usage
        assert!(!result.verify());
    }

    #[test]
    fn test_result_store() {
        let worker = Identity::generate();

        let result = JobResult::new(
            &worker,
            "job-789".into(),
            0,
            b"ok",
            b"",
            vec![],
            500,
            ResourceUsage {
                peak_cpu_percent: 5.0,
                peak_ram_mb: 128,
                disk_used_mb: 10,
            },
        );

        let id = result.id.clone();
        let mut store = JobResultStore::new();
        store.add(result);

        assert!(store.get(&id).is_some());
        assert!(store.for_job("job-789").is_some());
        assert!(store.for_job("nonexistent").is_none());
    }

    #[test]
    fn test_results_match_ignores_timing_but_not_outputs() {
        let worker_a = Identity::generate();
        let worker_b = Identity::generate();
        let usage = ResourceUsage {
            peak_cpu_percent: 5.0,
            peak_ram_mb: 128,
            disk_used_mb: 10,
        };
        let base = JobResult::new(
            &worker_a,
            "job-1".into(),
            0,
            b"ok\n",
            b"",
            vec![],
            500,
            usage.clone(),
        );
        let twin = JobResult::new(
            &worker_b,
            "job-1".into(),
            0,
            b"ok\n",
            b"",
            vec![],
            9000,
            usage.clone(),
        );
        assert!(results_match(&base, &twin));
        let other_exit = JobResult::new(
            &worker_b,
            "job-1".into(),
            3,
            b"ok\n",
            b"",
            vec![],
            500,
            usage.clone(),
        );
        assert!(!results_match(&base, &other_exit));
        let other_out = JobResult::new(
            &worker_a,
            "job-1".into(),
            0,
            b"nope\n",
            b"",
            vec![],
            500,
            usage,
        );
        assert!(!results_match(&base, &other_out));
    }

    #[test]
    fn test_results_match_compares_step_outputs() {
        let worker = Identity::generate();
        let usage = ResourceUsage {
            peak_cpu_percent: 1.0,
            peak_ram_mb: 64,
            disk_used_mb: 1,
        };
        let mut base = JobResult::new(
            &worker,
            "job-1".into(),
            0,
            b"ok\n",
            b"",
            vec![],
            10,
            usage.clone(),
        );
        base.set_outputs(
            &worker,
            HashMap::from([("SHA".to_string(), "abc".to_string())]),
        );
        assert!(base.verify());
        let twin = JobResult::new(&worker, "job-1".into(), 0, b"ok\n", b"", vec![], 99, usage);
        assert!(!results_match(&base, &twin));
        let mut twin_outputs = twin.clone();
        twin_outputs.set_outputs(
            &worker,
            HashMap::from([("SHA".to_string(), "abc".to_string())]),
        );
        assert!(results_match(&base, &twin_outputs));
        twin_outputs.set_outputs(
            &worker,
            HashMap::from([("SHA".to_string(), "tampered".to_string())]),
        );
        assert!(!results_match(&base, &twin_outputs));
    }
}
