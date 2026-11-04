use anyhow::{Context, Result};
use gt_core::identity::Identity;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tracing::warn;

use crate::job::ComputeJob;
use crate::result::{JobResult, ResourceUsage};
use crate::runner::{
    build_script, effective_limits, enforce_disk_quota, finalize_job_outputs, persist_artifacts,
    read_capped_exact, read_capped_tail, redact_secrets, sanitize_artifact_path, valid_env_key,
    ArtifactSink, ResourceCaps, GF_API_URL_ENV, GF_JOB_TOKEN_ENV, GF_OUTPUTS_ENV, GF_SUMMARY_ENV,
    MAX_ARTIFACTS, MAX_ARTIFACT_BYTES, MAX_LOG_BYTES, MAX_SUMMARY_BYTES,
};

pub const MAX_ENV_VALUE_BYTES: usize = 1024 * 1024;

pub struct LocalExecutor {
    work_dir: PathBuf,
    resource_caps: ResourceCaps,
    artifact_sink: Option<std::sync::Arc<dyn ArtifactSink>>,
    daemon_config: Option<PathBuf>,
    api_base: Option<String>,
}

impl LocalExecutor {
    pub fn new(work_dir: PathBuf) -> Self {
        LocalExecutor {
            work_dir,
            resource_caps: ResourceCaps::default(),
            artifact_sink: None,
            daemon_config: None,
            api_base: None,
        }
    }

    pub fn set_resource_caps(&mut self, caps: ResourceCaps) {
        self.resource_caps = caps;
    }

    pub fn set_daemon_config(&mut self, path: PathBuf) {
        self.daemon_config = Some(path);
    }

    pub fn set_api_base(&mut self, base: String) {
        self.api_base = Some(base);
    }

    pub fn set_artifact_sink(&mut self, sink: std::sync::Arc<dyn ArtifactSink>) {
        self.artifact_sink = Some(sink);
    }

    pub async fn run_job(
        &self,
        job: &ComputeJob,
        repo_path: &Path,
        worker_identity: &Identity,
    ) -> Result<JobResult> {
        if job.id.is_empty()
            || job.id.contains('/')
            || job.id.contains("..")
            || job.id.chars().any(|c| c.is_whitespace() || c.is_control())
        {
            anyhow::bail!("invalid job id");
        }
        if !repo_path.is_dir() {
            anyhow::bail!("repo path missing");
        }
        if !job.job_config.services.is_empty() {
            anyhow::bail!("services require docker execution mode");
        }
        if job.job_config.container.is_some() {
            anyhow::bail!("container jobs require docker execution mode");
        }
        let config = &job.job_config;
        let (_, _, timeout_secs) =
            effective_limits(&job.requirements, config.timeout_secs, &self.resource_caps);

        let job_dir = self.work_dir.join(&job.id);
        tokio::fs::create_dir_all(&job_dir).await?;
        let workspace = job_dir.join("workspace");
        let copy_out = tokio::process::Command::new("cp")
            .args(["-a"])
            .arg(repo_path)
            .arg(&workspace)
            .output()
            .await
            .context("failed to copy repo for local run")?;
        if !copy_out.status.success() {
            anyhow::bail!("repo copy failed");
        }
        if let Err(e) = enforce_disk_quota(&workspace, self.resource_caps.disk_mb) {
            let _ = tokio::fs::remove_dir_all(&job_dir).await;
            return Err(e.context("repo exceeds job disk quota"));
        }

        let if_ctx = crate::ifexpr::IfContext {
            success: true,
            inputs: job.dispatch_inputs.clone(),
            matrix: job.matrix.clone(),
            env: config.env.clone(),
            ..Default::default()
        };
        let script = build_script(&config.steps, &if_ctx);
        let script_path = job_dir.join("run.sh");
        tokio::fs::write(&script_path, &script).await?;

        let stdout_log = std::fs::File::create(job_dir.join("stdout.log"))?;
        let stderr_log = std::fs::File::create(job_dir.join("stderr.log"))?;
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg(&script_path)
            .current_dir(&workspace)
            .stdout(Stdio::from(stdout_log))
            .stderr(Stdio::from(stderr_log));
        for (k, v) in config.env.iter().chain(config.secrets.iter()) {
            if !valid_env_key(k) {
                warn!("Skipping env with invalid key: {k}");
                continue;
            }
            if v.len() > MAX_ENV_VALUE_BYTES {
                warn!("Skipping oversized env value: {k}");
                continue;
            }
            cmd.env(k, v);
        }
        cmd.env(GF_OUTPUTS_ENV, job_dir.join("outputs.env"));
        cmd.env(GF_SUMMARY_ENV, job_dir.join("summary.md"));
        if !config.job_token.is_empty() {
            cmd.env(GF_JOB_TOKEN_ENV, &config.job_token);
            if let Some(base) = &self.api_base {
                cmd.env(GF_API_URL_ENV, base);
            }
        }
        if let Some(config) = &self.daemon_config {
            cmd.env("GITFOREST_CONFIG", config);
        }
        if let Ok(exe) = std::env::current_exe() {
            if let Some(dir) = exe.parent() {
                let path = std::env::var("PATH").unwrap_or_default();
                cmd.env("PATH", format!("{}:{path}", dir.display()));
            }
        }

        let start_time = std::time::Instant::now();
        let mut child = cmd.spawn().context("failed to spawn job script")?;
        let timeout = tokio::time::Duration::from_secs(timeout_secs);
        let quota_mb = self.resource_caps.disk_mb;
        let quota_dir = job_dir.clone();
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
        let exit_code = tokio::select! {
            biased;
            _ = quota_watcher => {
                warn!("Local job exceeded disk quota; killing");
                let _ = child.kill().await;
                let _ = child.wait().await;
                -3
            }
            result = tokio::time::timeout(timeout, child.wait()) => match result {
                Ok(Ok(status)) => status.code().unwrap_or(-1),
                Ok(Err(e)) => {
                    warn!("Local job wait error: {e}");
                    -1
                }
                Err(_) => {
                    warn!("Local job timed out after {timeout_secs}s");
                    let _ = child.kill().await;
                    let _ = child.wait().await;
                    -2
                }
            },
        };
        let execution_time_ms = start_time.elapsed().as_millis() as u64;

        let mut stdout = read_capped_tail(&job_dir.join("stdout.log"), MAX_LOG_BYTES).await;
        let mut stderr = read_capped_tail(&job_dir.join("stderr.log"), MAX_LOG_BYTES).await;
        redact_secrets(&mut stdout, &config.secrets);
        redact_secrets(&mut stderr, &config.secrets);

        let mut artifacts = Vec::new();
        for pattern in config.artifacts.iter().take(MAX_ARTIFACTS) {
            let Some(safe) = sanitize_artifact_path(pattern) else {
                warn!("Skipping artifact with unsafe path: {pattern}");
                continue;
            };
            let artifact_path = workspace.join(safe);
            match read_capped_exact(&artifact_path, MAX_ARTIFACT_BYTES).await {
                Some(data) => artifacts.push((pattern.clone(), data)),
                None => warn!("Skipping artifact {pattern}: missing or oversized"),
            }
        }

        let outputs_raw = read_capped_exact(&job_dir.join("outputs.env"), MAX_LOG_BYTES)
            .await
            .unwrap_or_default();
        let outputs = finalize_job_outputs(&outputs_raw, config, job);
        let mut summary_raw = read_capped_exact(&job_dir.join("summary.md"), MAX_SUMMARY_BYTES)
            .await
            .unwrap_or_default();
        redact_secrets(&mut summary_raw, &config.secrets);
        let summary_md = String::from_utf8_lossy(&summary_raw).into_owned();

        let _ = tokio::fs::remove_dir_all(&job_dir).await;

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
                peak_cpu_percent: 0.0,
                peak_ram_mb: 0,
                disk_used_mb: 0,
            },
        );
        result.set_artifact_blobs(worker_identity, blobs);
        result.set_outputs(worker_identity, outputs);
        result.set_summary(summary_md);
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{ComputeRequirements, JobConfig, JobStep};
    use std::collections::HashMap;

    fn sample_job(id: &str, steps: Vec<JobStep>, artifacts: Vec<String>) -> ComputeJob {
        let submitter = Identity::generate();
        let config = JobConfig {
            name: "ci".into(),
            steps,
            docker_image: "unused".into(),
            timeout_secs: 60,
            env: HashMap::new(),
            secrets: HashMap::new(),
            artifacts,
            outputs: HashMap::new(),
            services: Vec::new(),
            container: None,
            job_token: String::new(),
        };
        let mut job = ComputeJob::new(
            &submitter,
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        job.id = id.into();
        job
    }

    fn step(name: &str, run: &str) -> JobStep {
        JobStep {
            name: name.into(),
            run: run.into(),
            condition: crate::job::RunCondition::Success,
            continue_on_error: false,
            working_directory: None,
            id: None,
        }
    }

    #[tokio::test]
    async fn test_local_quota_kill_on_fill() {
        let dir = test_repo("quota-kill");
        let worker = Identity::generate();
        let job = sample_job(
            "job-local-quota",
            vec![step(
                "fill",
                "dd if=/dev/zero of=big.bin bs=1M count=3 2>/dev/null; sleep 9",
            )],
            vec![],
        );
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_resource_caps(ResourceCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            timeout_secs: 60,
            disk_mb: 1,
        });
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, -3);
        assert!(!dir.join("work").join("job-local-quota").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_refuses_oversized_repo() {
        let dir = test_repo("quota-repo");
        std::fs::write(
            dir.join("repo").join("huge.bin"),
            vec![0u8; 3 * 1024 * 1024],
        )
        .unwrap();
        let worker = Identity::generate();
        let job = sample_job("job-local-bigrepo", vec![step("noop", "true")], vec![]);
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_resource_caps(ResourceCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            timeout_secs: 60,
            disk_mb: 1,
        });
        let err = exe
            .run_job(&job, &dir.join("repo"), &worker)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("quota"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_refuses_container_jobs() {
        let dir = test_repo("container-refuse");
        let worker = Identity::generate();
        let mut job = sample_job("job-local-container", vec![step("noop", "true")], vec![]);
        job.job_config.container = Some(crate::job::JobContainer {
            image: "node:20".into(),
            credentials: None,
            env: HashMap::new(),
            ports: vec![],
            volumes: vec![],
            resources: crate::job::ContainerResources::default(),
            options: String::new(),
        });
        let exe = LocalExecutor::new(dir.join("work"));
        let err = exe
            .run_job(&job, &dir.join("repo"), &worker)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("docker execution mode"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn test_repo(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("gf_local_{tag}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("repo")).unwrap();
        std::fs::write(dir.join("repo").join("input.txt"), "data\n").unwrap();
        dir
    }

    #[tokio::test]
    async fn test_local_run_success_with_artifact() {
        let dir = test_repo("ok");
        let worker = Identity::generate();
        let job = sample_job(
            "job-local-1",
            vec![step("build", "cat input.txt && echo built > out.txt")],
            vec!["out.txt".into()],
        );
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.verify());
        assert!(result.stdout_tail.contains("data"));
        assert_eq!(result.artifact_hashes.len(), 1);
        assert!(!dir.join("work").join("job-local-1").exists());
        assert!(!dir.join("repo").join("out.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_persists_artifacts_to_sink() {
        use std::sync::{Arc, Mutex};
        struct MemorySink {
            kept: Mutex<Vec<(String, Vec<u8>)>>,
        }
        impl ArtifactSink for MemorySink {
            fn store_artifact(
                &self,
                name: &str,
                bytes: &[u8],
            ) -> anyhow::Result<gt_core::types::ArtifactBlob> {
                use sha2::{Digest, Sha256};
                let digest: [u8; 32] = Sha256::digest(bytes).into();
                self.kept
                    .lock()
                    .unwrap()
                    .push((name.to_string(), bytes.to_vec()));
                Ok(gt_core::types::ArtifactBlob {
                    name: name.to_string(),
                    blob_id: gt_core::types::BlobId(digest),
                    size: bytes.len() as u64,
                })
            }
        }
        let dir = test_repo("sink");
        let worker = Identity::generate();
        let job = sample_job(
            "job-local-sink",
            vec![step("build", "cat input.txt && echo built > out.txt")],
            vec!["out.txt".into()],
        );
        let sink = Arc::new(MemorySink {
            kept: Mutex::new(Vec::new()),
        });
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_artifact_sink(sink.clone());
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert!(result.verify());
        assert_eq!(result.artifact_blobs.len(), 1);
        assert_eq!(result.artifact_blobs[0].name, "out.txt");
        let kept = sink.kept.lock().unwrap();
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].1, b"built\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_failure_and_env() {
        let dir = test_repo("fail");
        let worker = Identity::generate();
        let submitter = Identity::generate();
        let mut env = HashMap::new();
        env.insert("GREETING".into(), "hello-local".into());
        env.insert("BAD KEY".into(), "nope".into());
        let config = JobConfig {
            name: "ci".into(),
            steps: vec![step("greet", "echo $GREETING && exit 3")],
            docker_image: "unused".into(),
            timeout_secs: 60,
            env,
            secrets: HashMap::new(),
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
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 3);
        assert!(result.stdout_tail.contains("hello-local"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_timeout_kills() {
        let dir = test_repo("timeout");
        let worker = Identity::generate();
        let submitter = Identity::generate();
        let config = JobConfig {
            name: "ci".into(),
            steps: vec![step("sleep", "sleep 30")],
            docker_image: "unused".into(),
            timeout_secs: 600,
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
            submitter.peer_id(),
            "demo".into(),
            "abc".into(),
            config,
            ComputeRequirements::default(),
            100,
            "lightning".into(),
        );
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_resource_caps(ResourceCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            timeout_secs: 1,
            disk_mb: 10240,
        });
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, -2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_honors_step_conditions() {
        use crate::job::RunCondition;
        let dir = test_repo("cond");
        let worker = Identity::generate();
        let cond_step = |name: &str, run: &str, condition: RunCondition| JobStep {
            name: name.into(),
            run: run.into(),
            condition,
            continue_on_error: false,
            working_directory: None,
            id: None,
        };
        let job = sample_job(
            "job-local-cond",
            vec![
                cond_step("fail", "echo before-fail && exit 3", RunCondition::Success),
                cond_step("skipme", "echo must-not-run", RunCondition::Success),
                cond_step("rescue", "echo rescued-ok", RunCondition::Failure),
                cond_step("wrap", "echo always-ok", RunCondition::Always),
            ],
            vec![],
        );
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 3);
        assert!(result.stdout_tail.contains("before-fail"));
        assert!(!result.stdout_tail.contains("must-not-run"));
        assert!(result.stdout_tail.contains("rescued-ok"));
        assert!(result.stdout_tail.contains("always-ok"));
        assert!(result
            .stdout_tail
            .contains("skipping step skipme (if: success())"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_redacts_secrets() {
        let dir = test_repo("secrets");
        let worker = Identity::generate();
        let mut job = sample_job(
            "job-local-secrets",
            vec![
                step("leak", "echo token-is-$API_TOKEN and $API_TOKEN-again"),
                step("clean", "echo all-done"),
            ],
            vec![],
        );
        job.job_config
            .secrets
            .insert("API_TOKEN".to_string(), "hunter2-hunter2".to_string());
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(!result.stdout_tail.contains("hunter2"));
        assert!(result.stdout_tail.contains("token-is-***"));
        assert!(result.stdout_tail.contains("all-done"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_collects_step_outputs() {
        let dir = test_repo("outputs");
        let worker = Identity::generate();
        let mut job = sample_job(
            "job-local-outputs",
            vec![
                step("produce", "echo \"SHA=abc123\" >> \"$GF_OUTPUTS\""),
                step(
                    "multi",
                    "printf 'NOTES<<EOF\\nhello\\nworld\\nEOF\\n' >> \"$GF_OUTPUTS\"",
                ),
            ],
            vec![],
        );
        job.job_config
            .secrets
            .insert("API_TOKEN".to_string(), "abc123".to_string());
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert_eq!(result.outputs.get("NOTES").unwrap(), "hello\nworld");
        assert_eq!(result.outputs.get("SHA").unwrap(), "***");
        assert!(result.verify());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_collects_summary() {
        let dir = test_repo("summary");
        let worker = Identity::generate();
        let mut job = sample_job(
            "job-local-summary",
            vec![step(
                "report",
                "echo '## Results' >> \"$GF_SUMMARY\" && echo 'secret-abc123' >> \"$GF_SUMMARY\"",
            )],
            vec![],
        );
        job.job_config
            .secrets
            .insert("API_TOKEN".to_string(), "abc123".to_string());
        let exe = LocalExecutor::new(dir.join("work"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.summary_md.contains("## Results"));
        assert!(result.summary_md.contains("secret-***"));
        assert!(!result.summary_md.contains("abc123"));
        assert!(result.verify());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_exposes_job_token_and_api_url() {
        let dir = test_repo("jobtoken");
        let worker = Identity::generate();
        let mut job = sample_job(
            "job-local-jobtoken",
            vec![step(
                "probe",
                "echo \"token=$GF_JOB_TOKEN\" && echo \"api=$GF_API_URL\"",
            )],
            vec![],
        );
        job.job_config.job_token = "gfj.test.token".to_string();
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_api_base("http://127.0.0.1:7878".to_string());
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        assert!(result.stdout_tail.contains("token=gfj.test.token"));
        assert!(result.stdout_tail.contains("api=http://127.0.0.1:7878"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_rejects_bad_id_and_missing_repo() {
        let dir = test_repo("bad");
        let worker = Identity::generate();
        let exe = LocalExecutor::new(dir.join("work"));
        let bad = sample_job("../evil", vec![step("x", "echo hi")], vec![]);
        assert!(exe.run_job(&bad, &dir.join("repo"), &worker).await.is_err());
        let ok = sample_job("job-local-4", vec![step("x", "echo hi")], vec![]);
        assert!(exe.run_job(&ok, &dir.join("nope"), &worker).await.is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_local_run_exposes_daemon_contact_to_steps() {
        let dir = test_repo("contact");
        let worker = Identity::generate();
        let job = sample_job(
            "job-local-contact",
            vec![step(
                "probe",
                "echo \"config=$GITFOREST_CONFIG\" && echo \"path=$PATH\"",
            )],
            vec![],
        );
        let mut exe = LocalExecutor::new(dir.join("work"));
        exe.set_daemon_config(dir.join("daemon.toml"));
        let result = exe.run_job(&job, &dir.join("repo"), &worker).await.unwrap();
        assert_eq!(result.exit_code, 0);
        let daemon_toml = dir.join("daemon.toml");
        assert!(
            result
                .stdout_tail
                .contains(&format!("config={}", daemon_toml.display())),
            "{}",
            result.stdout_tail
        );
        let exe_dir = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .display()
            .to_string();
        assert!(
            result.stdout_tail.contains(&format!("path={exe_dir}:")),
            "{}",
            result.stdout_tail
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
