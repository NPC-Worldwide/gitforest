use anyhow::Context;
use chrono::{DateTime, Utc};
use gt_compute::job::{ComputeJob, ComputeJobStore, JobStatus};
use gt_compute::permissions::{job_token_expiry, mint_job_token};
use gt_compute::workflow::{self, UsesTarget, WorkflowEvent};
use gt_core::config::RepoConfig;
use gt_core::identity::{Identity, PeerId};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowRun {
    pub id: String,
    pub repo_name: String,
    pub workflow_name: String,
    pub workflow_file: String,
    pub event: String,
    pub branch: String,
    pub commit_sha: String,
    pub job_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default)]
    pub workflow_run_fired: bool,
    #[serde(default)]
    pub rerun_of: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RunSummary {
    pub total: usize,
    pub completed: usize,
    pub failed: usize,
    pub running: usize,
    pub pending: usize,
    pub blocked: usize,
}

pub struct WorkflowRunStore {
    runs: HashMap<String, WorkflowRun>,
}

impl WorkflowRunStore {
    pub fn new() -> Self {
        WorkflowRunStore {
            runs: HashMap::new(),
        }
    }

    pub fn add(&mut self, run: WorkflowRun) {
        self.runs.insert(run.id.clone(), run);
    }

    pub fn get(&self, id: &str) -> Option<&WorkflowRun> {
        self.runs.get(id)
    }

    pub fn mark_workflow_run_fired(&mut self, id: &str) {
        if let Some(run) = self.runs.get_mut(id) {
            run.workflow_run_fired = true;
        }
    }

    pub fn list(&self) -> Vec<&WorkflowRun> {
        let mut out: Vec<&WorkflowRun> = self.runs.values().collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        out
    }

    pub fn for_repo(&self, repo_name: &str) -> Vec<&WorkflowRun> {
        let mut out: Vec<&WorkflowRun> = self
            .runs
            .values()
            .filter(|r| r.repo_name == repo_name)
            .collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.created_at));
        out
    }
}

impl Default for WorkflowRunStore {
    fn default() -> Self {
        Self::new()
    }
}

pub fn event_name_branch(event: &WorkflowEvent) -> (String, String) {
    match event {
        WorkflowEvent::Push { branch } => ("push".to_string(), branch.clone()),
        WorkflowEvent::PullRequest { branch } => ("pull_request".to_string(), branch.clone()),
        WorkflowEvent::WorkflowDispatch { .. } => ("workflow_dispatch".to_string(), String::new()),
        WorkflowEvent::Tag { tag } => ("tag".to_string(), tag.clone()),
        WorkflowEvent::WorkflowRun { branch, .. } => ("workflow_run".to_string(), branch.clone()),
        WorkflowEvent::Schedule { branch, .. } => ("schedule".to_string(), branch.clone()),
    }
}

pub fn gitforest_context(
    event: &WorkflowEvent,
    repo_name: &str,
    workflow_name: &str,
    commit_sha: &str,
    run_id: &str,
) -> HashMap<String, String> {
    let (event_name, branch) = event_name_branch(event);
    let (git_ref, ref_name, head_ref) = match event {
        WorkflowEvent::Tag { tag } => (format!("refs/tags/{tag}"), tag.clone(), String::new()),
        _ => {
            let name = if branch.is_empty() { "main" } else { &branch };
            (
                format!("refs/heads/{name}"),
                name.to_string(),
                branch.clone(),
            )
        }
    };
    let mut ctx = HashMap::from([
        ("event_name".to_string(), event_name),
        ("ref".to_string(), git_ref),
        ("ref_name".to_string(), ref_name),
        ("sha".to_string(), commit_sha.to_string()),
        ("repository".to_string(), repo_name.to_string()),
        ("workflow".to_string(), workflow_name.to_string()),
        ("run_id".to_string(), run_id.to_string()),
        ("head_ref".to_string(), head_ref),
        ("base_ref".to_string(), String::new()),
    ]);
    if let WorkflowEvent::WorkflowRun {
        workflow,
        conclusion,
        ..
    } = event
    {
        ctx.insert("workflow_run_workflow".to_string(), workflow.clone());
        ctx.insert("workflow_run_conclusion".to_string(), conclusion.clone());
    }
    if let WorkflowEvent::Schedule { cron, .. } = event {
        ctx.insert("schedule".to_string(), cron.clone());
    }
    ctx
}

fn reduce_gitforest_conditions(
    job: &mut ComputeJob,
    gitforest: &std::collections::HashMap<String, String>,
) {
    let ctx = gt_compute::ifexpr::IfContext {
        gitforest: gitforest.clone(),
        ..Default::default()
    };
    if let gt_compute::job::RunCondition::Expr(raw) = &mut job.run_if {
        *raw = gt_compute::ifexpr::substitute_roots(raw, &ctx, &["gitforest"]);
    }
    for step in &mut job.job_config.steps {
        if let gt_compute::job::RunCondition::Expr(raw) = &mut step.condition {
            *raw = gt_compute::ifexpr::substitute_roots(raw, &ctx, &["gitforest"]);
        }
    }
}

fn reduce_dispatch_conditions(job: &mut ComputeJob) {
    let ctx = gt_compute::ifexpr::IfContext {
        inputs: job.dispatch_inputs.clone(),
        matrix: job.matrix.clone(),
        env: job.job_config.env.clone(),
        ..Default::default()
    };
    if let gt_compute::job::RunCondition::Expr(raw) = &mut job.run_if {
        *raw = gt_compute::ifexpr::substitute_roots(raw, &ctx, &["inputs", "matrix", "env"]);
    }
    for step in &mut job.job_config.steps {
        if let gt_compute::job::RunCondition::Expr(raw) = &mut step.condition {
            *raw = gt_compute::ifexpr::substitute_roots(raw, &ctx, &["inputs", "matrix", "env"]);
        }
    }
}

fn root_condition_holds(
    job: &ComputeJob,
    gitforest: &std::collections::HashMap<String, String>,
) -> bool {
    let cond = match &job.run_if {
        gt_compute::job::RunCondition::Expr(raw) => raw.clone(),
        other => other.name().to_string(),
    };
    let ctx = gt_compute::ifexpr::IfContext {
        success: true,
        inputs: job.dispatch_inputs.clone(),
        matrix: job.matrix.clone(),
        env: job.job_config.env.clone(),
        gitforest: gitforest.clone(),
        ..Default::default()
    };
    gt_compute::ifexpr::eval(&cond, &ctx)
}

pub fn summarize_run(run: &WorkflowRun, jobs: &ComputeJobStore) -> RunSummary {
    let mut summary = RunSummary {
        total: run.job_ids.len(),
        ..Default::default()
    };
    for id in &run.job_ids {
        match jobs.get(id).map(|j| &j.status) {
            Some(JobStatus::Completed { .. }) => summary.completed += 1,
            Some(JobStatus::Failed { .. })
            | Some(JobStatus::Disputed)
            | Some(JobStatus::Cancelled) => summary.failed += 1,
            Some(JobStatus::Running { .. })
            | Some(JobStatus::Matched { .. })
            | Some(JobStatus::Verifying { .. }) => summary.running += 1,
            Some(JobStatus::Blocked { .. }) => summary.blocked += 1,
            Some(JobStatus::Pending) | None => summary.pending += 1,
        }
    }
    summary
}

pub fn collect_repo_dirs(repos: &[RepoConfig]) -> HashMap<String, PathBuf> {
    repos
        .iter()
        .map(|r| (r.name.clone(), r.path.clone()))
        .collect()
}

fn uses_entries(doc: &serde_yaml::Value) -> Vec<String> {
    let mut out = Vec::new();
    let Some(jobs) = doc.get("jobs").and_then(|j| j.as_mapping()) else {
        return out;
    };
    for (_, job) in jobs {
        if let Some(target) = job.get("uses").and_then(|u| u.as_str()) {
            out.push(target.to_string());
        }
        if let Some(steps) = job.get("steps").and_then(|s| s.as_sequence()) {
            for step in steps {
                if let Some(target) = step.get("uses").and_then(|u| u.as_str()) {
                    out.push(target.to_string());
                }
            }
        }
    }
    out
}

fn replace_uses_target(doc: &mut serde_yaml::Value, from: &str, to: &str) {
    let Some(jobs) = doc.get_mut("jobs").and_then(|j| j.as_mapping_mut()) else {
        return;
    };
    let key = serde_yaml::Value::String("uses".to_string());
    let replacement = serde_yaml::Value::String(to.to_string());
    for (_, job) in jobs.iter_mut() {
        let Some(map) = job.as_mapping_mut() else {
            continue;
        };
        if map.get(&key).and_then(|u| u.as_str()) == Some(from) {
            map.insert(key.clone(), replacement.clone());
        }
        if let Some(steps) = map
            .get_mut(serde_yaml::Value::String("steps".to_string()))
            .and_then(|s| s.as_sequence_mut())
        {
            for step in steps.iter_mut() {
                if let Some(step_map) = step.as_mapping_mut() {
                    let hit = step_map.get(&key).and_then(|u| u.as_str()) == Some(from);
                    if hit {
                        step_map.insert(key.clone(), replacement.clone());
                    }
                }
            }
        }
    }
}

fn sanitize_overlay_component(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '_'
            }
        })
        .take(64)
        .collect()
}

fn join_within_repo(parent: &Path, rel: &str) -> Option<PathBuf> {
    let mut parts: Vec<String> = parent
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    for comp in Path::new(rel).components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                parts.pop()?;
            }
            Component::Normal(s) => parts.push(s.to_string_lossy().into_owned()),
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(parts.iter().collect())
}

fn blob_at_ref(repo_dir: &Path, git_ref: &str, repo_rel: &Path) -> anyhow::Result<Vec<u8>> {
    let repo = git2::Repository::open(repo_dir)
        .with_context(|| format!("open repo {}", repo_dir.display()))?;
    let object = repo
        .revparse_single(git_ref)
        .with_context(|| format!("resolve ref {git_ref}"))?;
    let tree = object
        .peel_to_tree()
        .context("ref does not point at a commit")?;
    let entry = tree
        .get_path(repo_rel)
        .with_context(|| format!("{} missing at {}", repo_rel.display(), git_ref))?;
    let blob = entry
        .to_object(&repo)?
        .peel_to_blob()
        .context("not a file")?;
    Ok(blob.content().to_vec())
}

struct RemoteMaterializer<'a> {
    repo_dirs: &'a HashMap<String, PathBuf>,
    overlay: &'a Path,
    done: HashMap<String, PathBuf>,
    visiting: Vec<String>,
}

impl RemoteMaterializer<'_> {
    fn materialize(
        &mut self,
        repo: &str,
        repo_rel: &Path,
        git_ref: &str,
    ) -> anyhow::Result<PathBuf> {
        let key = format!("{repo}@{git_ref}:{}", repo_rel.display());
        if let Some(done) = self.done.get(&key) {
            return Ok(done.clone());
        }
        if self.visiting.contains(&key) {
            anyhow::bail!("reusable workflow cycle via {key}");
        }
        if self.done.len() >= 64 {
            anyhow::bail!("too many remote workflow files");
        }
        let repo_dir = self
            .repo_dirs
            .get(repo)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown repo {repo:?} in uses"))?;
        self.visiting.push(key.clone());
        let result = self.materialize_inner(&repo_dir, repo, repo_rel, git_ref);
        self.visiting.pop();
        let path = result?;
        self.done.insert(key, path.clone());
        Ok(path)
    }

    fn materialize_inner(
        &mut self,
        repo_dir: &Path,
        repo: &str,
        repo_rel: &Path,
        git_ref: &str,
    ) -> anyhow::Result<PathBuf> {
        let bytes = blob_at_ref(repo_dir, git_ref, repo_rel)
            .with_context(|| format!("read {repo}/{repo_rel:?}@{git_ref}"))?;
        let text = String::from_utf8(bytes).context("remote workflow file is not UTF-8")?;
        let mut doc: serde_yaml::Value =
            serde_yaml::from_str(&text).context("parse remote workflow")?;
        let parent = repo_rel.parent().unwrap_or_else(|| Path::new(""));
        for raw in uses_entries(&doc) {
            let parsed = match workflow::parse_uses_target(&raw) {
                Ok(target) => target,
                Err(_) => continue,
            };
            let next = match parsed {
                UsesTarget::Local(rel) if Path::new(&rel).is_absolute() => None,
                UsesTarget::Local(rel) => join_within_repo(parent, &rel)
                    .map(|joined| self.materialize(repo, &joined, git_ref))
                    .transpose()?
                    .map(|p| p.display().to_string()),
                UsesTarget::Remote {
                    repo: nested,
                    path,
                    git_ref: nested_ref,
                } => Some(
                    self.materialize(&nested, Path::new(&path), &nested_ref)?
                        .display()
                        .to_string(),
                ),
            };
            if let Some(rewritten) = next {
                replace_uses_target(&mut doc, &raw, &rewritten);
            }
        }
        let dest = self
            .overlay
            .join(format!(
                "{}__{}",
                sanitize_overlay_component(repo),
                sanitize_overlay_component(git_ref)
            ))
            .join(repo_rel);
        if let Some(dir) = dest.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&dest, serde_yaml::to_string(&doc)?)?;
        Ok(dest)
    }
}

fn materialize_remote_uses(
    top_path: &Path,
    repo_dirs: &HashMap<String, PathBuf>,
    overlay: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    let text = std::fs::read_to_string(top_path)?;
    let mut doc: serde_yaml::Value = serde_yaml::from_str(&text)?;
    let entries = uses_entries(&doc);
    let has_remote = entries.iter().any(|raw| {
        matches!(
            workflow::parse_uses_target(raw),
            Ok(UsesTarget::Remote { .. })
        )
    });
    if !has_remote {
        return Ok(None);
    }
    let mut remote = RemoteMaterializer {
        repo_dirs,
        overlay,
        done: HashMap::new(),
        visiting: Vec::new(),
    };
    let mut changed = false;
    let top_dir = top_path.parent().unwrap_or_else(|| Path::new(""));
    for raw in entries {
        let Ok(target) = workflow::parse_uses_target(&raw) else {
            continue;
        };
        match target {
            UsesTarget::Local(rel) if !Path::new(&rel).is_absolute() => {
                let anchored = top_dir.join(&rel);
                replace_uses_target(&mut doc, &raw, &anchored.display().to_string());
                changed = true;
            }
            UsesTarget::Local(_) => {}
            UsesTarget::Remote {
                repo,
                path,
                git_ref,
            } => {
                let dest = remote.materialize(&repo, Path::new(&path), &git_ref)?;
                replace_uses_target(&mut doc, &raw, &dest.display().to_string());
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(None);
    }
    let dir = overlay.join("__caller__");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(top_path.file_name().unwrap_or_default());
    std::fs::write(&dest, serde_yaml::to_string(&doc)?)?;
    Ok(Some(dest))
}

#[allow(clippy::too_many_arguments)]
pub fn trigger_workflows(
    job_store: &mut ComputeJobStore,
    runs: &mut WorkflowRunStore,
    identity: &Identity,
    repo_owner: &PeerId,
    repo_name: &str,
    repo_path: &Path,
    event: &WorkflowEvent,
    commit_sha: &str,
    only_workflow: Option<&str>,
    max_payment_sats: u64,
    preferred_chain: &str,
    force_verify: bool,
    repo_secrets: &HashMap<String, String>,
    protected_envs: &[String],
    repo_dirs: &HashMap<String, PathBuf>,
) -> Vec<WorkflowRun> {
    let (event_name, branch) = event_name_branch(event);
    let mut created = Vec::new();
    let overlay = tempfile::TempDir::new().ok();
    for (path, parsed) in workflow::load_workflows(repo_path) {
        let (path, parsed) = match (&overlay, repo_dirs.is_empty()) {
            (Some(dir), false) => match materialize_remote_uses(&path, repo_dirs, dir.path()) {
                Ok(Some(rewritten)) => {
                    let reparsed = workflow::parse_workflow_file(&rewritten);
                    (rewritten, reparsed)
                }
                Ok(None) => (path, parsed),
                Err(e) => {
                    tracing::warn!("skip workflow {}: remote use error: {e}", path.display());
                    continue;
                }
            },
            _ => (path, parsed),
        };
        let workflow = match parsed {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!("skip bad workflow {}: {e}", path.display());
                continue;
            }
        };
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let label = if workflow.name.is_empty() {
            file_name.clone()
        } else {
            workflow.name.clone()
        };
        if let Some(only) = only_workflow {
            if only != label && only != file_name {
                continue;
            }
        }
        if !workflow.matches(event) {
            continue;
        }
        let dispatch_inputs = match event {
            WorkflowEvent::WorkflowDispatch { inputs } => Some(inputs),
            _ => None,
        };
        let expanded = match workflow::resolve_and_expand_with_secrets(
            &workflow,
            repo_secrets,
            dispatch_inputs,
        ) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!("skip workflow {}: call error: {}", label, e);
                continue;
            }
        };
        let ordered = match workflow::order_jobs(&expanded) {
            Ok(list) => list,
            Err(e) => {
                tracing::warn!("skip workflow {}: DAG error: {}", label, e);
                continue;
            }
        };
        let mut id_map: HashMap<String, String> = HashMap::new();
        let mut made: Vec<ComputeJob> = Vec::new();
        for job in &ordered {
            let mut created_job = ComputeJob::new(
                identity,
                repo_owner.clone(),
                repo_name.to_string(),
                commit_sha.to_string(),
                job.job_config.clone(),
                job.requirements.clone(),
                max_payment_sats,
                preferred_chain.to_string(),
            );
            created_job.job_config.name = format!("{} / {}", label, job.display_name);
            created_job.permissions = job.permissions.clone();
            if !job.permissions.grants.is_empty() {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();
                created_job.job_config.job_token = mint_job_token(
                    identity,
                    &created_job.id,
                    &job.permissions,
                    job_token_expiry(now, created_job.job_config.timeout_secs),
                );
            }
            created_job.run_if = job.run_if.clone();
            created_job.workflow_job = job.job_id.clone();
            created_job.call_id = job.call_id.clone();
            created_job.call_outputs = job.call_outputs.clone();
            created_job.fail_fast = job.fail_fast;
            created_job.matrix = job.matrix.clone();
            created_job.dispatch_inputs = dispatch_inputs.cloned().unwrap_or_default();
            if force_verify {
                created_job.verify = true;
            }
            id_map.insert(job.job_id.clone(), created_job.id.clone());
            made.push(created_job);
        }
        let mut groups: HashMap<String, Vec<String>> = HashMap::new();
        for job in &ordered {
            if let Some(parent) = &job.matrix_parent {
                if let Some(uuid) = id_map.get(&job.job_id) {
                    groups.entry(parent.clone()).or_default().push(uuid.clone());
                }
            }
        }
        let run_id = uuid::Uuid::new_v4().to_string();
        let gitforest = gitforest_context(event, repo_name, &label, commit_sha, &run_id);
        let mut job_ids = Vec::new();
        for (job, created_job) in ordered.iter().zip(made.iter_mut()) {
            created_job.concurrency_group =
                workflow::substitute_gitforest(&job.concurrency_group, &gitforest);
            created_job.cancel_in_progress = job.cancel_in_progress;
            created_job.environment = workflow::substitute_gitforest(&job.environment, &gitforest);
            created_job.environment_url =
                workflow::substitute_gitforest(&job.environment_url, &gitforest);
            created_job.continue_on_error = job.continue_on_error;
            created_job.max_parallel = job.max_parallel;
            created_job.env_approval_pending = !created_job.environment.is_empty()
                && protected_envs.contains(&created_job.environment);
            reduce_gitforest_conditions(created_job, &gitforest);
            if job.needs.is_empty() {
                reduce_dispatch_conditions(created_job);
                if root_condition_holds(created_job, &gitforest) {
                    created_job.status = JobStatus::Pending;
                } else {
                    created_job.status = JobStatus::Cancelled;
                }
            } else {
                let mapped: Vec<String> = job
                    .needs
                    .iter()
                    .filter_map(|n| id_map.get(n).cloned())
                    .collect();
                created_job.status = JobStatus::Blocked {
                    needs: mapped.clone(),
                };
                created_job.needs = mapped;
            }
            if let Some(parent) = &job.matrix_parent {
                if let Some(members) = groups.get(parent) {
                    created_job.matrix_siblings = members
                        .iter()
                        .filter(|id| *id != &created_job.id)
                        .cloned()
                        .collect();
                }
            }
            let payload = created_job.signable_payload();
            created_job.signature = identity.sign(&payload);
            job_ids.push(created_job.id.clone());
        }
        for created_job in made {
            job_store.add(created_job);
        }
        let keep: std::collections::HashSet<String> = job_ids.iter().cloned().collect();
        let mut cancel_groups: Vec<String> = Vec::new();
        for id in &job_ids {
            if let Some(job) = job_store.get(id) {
                if job.cancel_in_progress && !job.concurrency_group.is_empty() {
                    cancel_groups.push(job.concurrency_group.clone());
                }
            }
        }
        cancel_groups.sort();
        cancel_groups.dedup();
        for group in cancel_groups {
            let cancelled = gt_compute::dispatch::cancel_group_jobs(job_store, &group, &keep);
            if !cancelled.is_empty() {
                tracing::info!(
                    "concurrency group {group} cancelled {} prior job(s)",
                    cancelled.len()
                );
            }
        }
        let run = WorkflowRun {
            id: run_id,
            repo_name: repo_name.to_string(),
            workflow_name: label,
            workflow_file: file_name,
            event: event_name.clone(),
            branch: branch.clone(),
            commit_sha: commit_sha.to_string(),
            job_ids,
            created_at: Utc::now(),
            workflow_run_fired: false,
            rerun_of: None,
        };
        runs.add(run.clone());
        created.push(run);
    }
    created
}

pub fn rerun_failed_jobs(
    job_store: &mut ComputeJobStore,
    runs: &mut WorkflowRunStore,
    identity: &Identity,
    run_id: &str,
) -> Option<WorkflowRun> {
    let run = runs.get(run_id)?.clone();
    let mut rerun: Vec<String> = run
        .job_ids
        .iter()
        .filter(|id| {
            matches!(
                job_store.get(id).map(|j| &j.status),
                Some(JobStatus::Failed { .. }) | Some(JobStatus::Disputed)
            )
        })
        .cloned()
        .collect();
    if rerun.is_empty() {
        return None;
    }
    loop {
        let mut add: Vec<String> = Vec::new();
        for id in run.job_ids.iter().filter(|id| !rerun.contains(id)) {
            let cancelled_never_ran = matches!(
                job_store.get(id).map(|j| &j.status),
                Some(JobStatus::Cancelled)
            );
            let needs_rerun = job_store
                .get(id)
                .is_some_and(|j| j.needs.iter().any(|n| rerun.contains(n)));
            if cancelled_never_ran && needs_rerun {
                add.push(id.clone());
            }
        }
        if add.is_empty() {
            break;
        }
        rerun.extend(add);
    }
    let mut remap: HashMap<String, String> = HashMap::new();
    for old in &rerun {
        remap.insert(old.clone(), uuid::Uuid::new_v4().to_string());
    }
    let mut new_ids = Vec::new();
    for old in &rerun {
        let job = job_store.get(old)?.clone();
        let mut cloned = job.clone();
        cloned.id = remap[old].clone();
        cloned.submitter = identity.peer_id();
        cloned.created_at = Utc::now();
        cloned.needs = job
            .needs
            .iter()
            .map(|n| remap.get(n).cloned().unwrap_or_else(|| n.clone()))
            .collect();
        cloned.status = JobStatus::Blocked {
            needs: cloned.needs.clone(),
        };
        cloned.matrix_siblings = job
            .matrix_siblings
            .iter()
            .map(|s| remap.get(s).cloned().unwrap_or_else(|| s.clone()))
            .collect();
        if cloned.permissions.grants.is_empty() {
            cloned.job_config.job_token.clear();
        } else {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            cloned.job_config.job_token = mint_job_token(
                identity,
                &cloned.id,
                &cloned.permissions,
                job_token_expiry(now, cloned.job_config.timeout_secs),
            );
        }
        let payload = cloned.signable_payload();
        cloned.signature = identity.sign(&payload);
        new_ids.push(cloned.id.clone());
        job_store.add(cloned);
    }
    let rerun_run = WorkflowRun {
        id: uuid::Uuid::new_v4().to_string(),
        repo_name: run.repo_name.clone(),
        workflow_name: run.workflow_name.clone(),
        workflow_file: run.workflow_file.clone(),
        event: run.event.clone(),
        branch: run.branch.clone(),
        commit_sha: run.commit_sha.clone(),
        job_ids: new_ids,
        created_at: Utc::now(),
        workflow_run_fired: false,
        rerun_of: Some(run.id.clone()),
    };
    runs.add(rerun_run.clone());
    Some(rerun_run)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gt_compute::dispatch::unblock_ready_jobs;
    use std::process::Command;

    fn git_repo_with_workflow(yaml: &str) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gf_trigger_{}", uuid::Uuid::new_v4()));
        let dir = root.join(".gitforest").join("workflows");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ci.yml"), yaml).unwrap();
        Command::new("git")
            .args(["init", "-q", "--initial-branch=main"])
            .current_dir(&root)
            .output()
            .unwrap();
        root
    }

    fn git_repo_with_committed_files(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gf_trigger_{}", uuid::Uuid::new_v4()));
        for (name, text) in files {
            let path = root.join(name);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        let steps: &[&[&str]] = &[
            &["init", "-q", "--initial-branch=main"],
            &["config", "user.email", "test@gitforest.net"],
            &["config", "user.name", "gf-test"],
            &["add", "-A"],
            &["commit", "-qm", "test"],
        ];
        for args in steps {
            let status = Command::new("git")
                .args(*args)
                .current_dir(&root)
                .status()
                .unwrap();
            assert!(status.success());
        }
        root
    }

    fn git_repo_with_committed_workflow(yaml: &str, filename: &str) -> std::path::PathBuf {
        git_repo_with_committed_files(&[(&format!(".gitforest/workflows/{filename}"), yaml)])
    }

    const YAML: &str = r#"
name: CI
on:
  push:
    branches: [main]
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

    const TAG_YAML: &str = r#"
name: Release
on:
  push:
    tags: ['v*']
jobs:
  package:
    runs-on: ubuntu-latest
    steps:
      - run: echo package
"#;

    const IF_YAML: &str = r#"
name: Gated
on: push
jobs:
  muted:
    if: false
    runs-on: ubuntu-latest
    steps:
      - run: echo muted
  vocal:
    if: gitforest.event_name == 'push'
    runs-on: ubuntu-latest
    steps:
      - run: echo vocal
"#;

    #[test]
    fn trigger_mints_job_tokens_from_effective_permissions() {
        use gt_compute::permissions::{
            verify_job_token, PermissionLevel, PermissionScope, JOB_TOKEN_PREFIX,
        };
        let yaml = r#"
name: Perms
on: push
permissions:
  contents: read
jobs:
  reader:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
  none:
    runs-on: ubuntu-latest
    permissions:
      contents: none
    steps:
      - run: echo hi
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let mut saw_token = false;
        for id in &created[0].job_ids {
            let job = jobs.get(id).unwrap();
            assert!(job.verify());
            if job.workflow_job == "reader" {
                assert!(job.job_config.job_token.starts_with(JOB_TOKEN_PREFIX));
                let claims =
                    verify_job_token(&job.job_config.job_token, &identity.peer_id(), now).unwrap();
                assert_eq!(claims.job_id, *id);
                assert!(claims
                    .permissions
                    .allows(PermissionScope::Contents, PermissionLevel::Read));
                assert!(!claims
                    .permissions
                    .allows(PermissionScope::Contents, PermissionLevel::Write));
                saw_token = true;
            } else {
                assert!(job.job_config.job_token.is_empty());
                assert!(job.permissions.grants.is_empty());
            }
        }
        assert!(saw_token);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn rerun_remints_job_tokens_for_cloned_jobs() {
        use gt_compute::permissions::{verify_job_token, JOB_TOKEN_PREFIX};
        let yaml = r#"
name: Perms
on: push
permissions: read-all
jobs:
  flaky:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        let old_id = created[0].job_ids[0].clone();
        let old_token = jobs.get(&old_id).unwrap().job_config.job_token.clone();
        assert!(old_token.starts_with(JOB_TOKEN_PREFIX));
        jobs.get_mut(&old_id).unwrap().status = JobStatus::Failed {
            reason: "boom".to_string(),
        };
        let rerun = rerun_failed_jobs(&mut jobs, &mut runs, &identity, &created[0].id).unwrap();
        assert_eq!(rerun.job_ids.len(), 1);
        let new_id = rerun.job_ids[0].clone();
        assert_ne!(new_id, old_id);
        let cloned = jobs.get(&new_id).unwrap();
        assert!(cloned.verify());
        assert!(cloned.job_config.job_token.starts_with(JOB_TOKEN_PREFIX));
        assert_ne!(cloned.job_config.job_token, old_token);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let claims =
            verify_job_token(&cloned.job_config.job_token, &identity.peer_id(), now).unwrap();
        assert_eq!(claims.job_id, new_id);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_root_if_false_cancels_at_birth() {
        let root = git_repo_with_workflow(IF_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let push = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &push,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        let by_name: HashMap<String, JobStatus> = created[0]
            .job_ids
            .iter()
            .map(|id| {
                let job = jobs.get(id).unwrap();
                (job.workflow_job.clone(), job.status.clone())
            })
            .collect();
        assert_eq!(by_name.get("muted"), Some(&JobStatus::Cancelled));
        assert_eq!(by_name.get("vocal"), Some(&JobStatus::Pending));
    }

    #[test]
    fn trigger_tag_event_matches_tags_only_workflow() {
        let root = git_repo_with_workflow(TAG_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let tag = WorkflowEvent::Tag {
            tag: "v1.2.3".to_string(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &tag,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].event, "tag");
        assert_eq!(created[0].job_ids.len(), 1);
        let branch_push = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let not_created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &branch_push,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert!(not_created.is_empty());
        let other_tag = WorkflowEvent::Tag {
            tag: "nightly".to_string(),
        };
        let not_matched = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &other_tag,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert!(not_matched.is_empty());
    }

    const INPUTS_YAML: &str = r#"
name: Greet
on:
  workflow_dispatch:
    inputs:
      name:
        required: true
      greeting:
        default: hello
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo "${{ inputs.greeting }}-${{ inputs.name }}"
"#;

    #[test]
    fn trigger_dispatch_inputs_reach_expanded_steps() {
        let root = git_repo_with_workflow(INPUTS_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::from([("name".to_string(), "ada".to_string())]),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].event, "workflow_dispatch");
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert!(
            job.job_config.steps[0].run.contains("hello-ada"),
            "{}",
            job.job_config.steps[0].run
        );
        let missing = WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::new(),
        };
        let skipped = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &missing,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn trigger_creates_run_with_blocked_child() {
        let root = git_repo_with_workflow(YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        assert_eq!(created[0].event, "push");
        let root_job = jobs.get(&created[0].job_ids[0]).unwrap();
        let child_job = jobs.get(&created[0].job_ids[1]).unwrap();
        assert!(matches!(root_job.status, JobStatus::Pending));
        assert!(matches!(child_job.status, JobStatus::Blocked { .. }));
        assert_eq!(root_job.workflow_job, "build");
        assert_eq!(child_job.workflow_job, "test");
        assert!(root_job.verify());
        assert_eq!(runs.list().len(), 1);
        assert_eq!(runs.for_repo("demo").len(), 1);
        assert!(runs.for_repo("other").is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_reduces_root_step_inputs() {
        let yaml = r#"
name: CI
on:
  workflow_dispatch:
    inputs:
      env:
        default: dev
jobs:
  root:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        if: inputs.env == 'prod'
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::from([("env".to_string(), "prod".to_string())]),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert!(matches!(job.status, JobStatus::Pending));
        let condition = &job.job_config.steps[0].condition;
        let reduced = match condition {
            gt_compute::job::RunCondition::Expr(raw) => raw.clone(),
            other => panic!("expected Expr condition, got {other:?}"),
        };
        assert!(
            !reduced.contains("inputs.env"),
            "condition left unreduced: {reduced}"
        );
        assert!(reduced.contains("'prod'"), "missing value: {reduced}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_signs_after_condition_reduction() {
        let yaml = r#"
name: CI
on:
  workflow_dispatch:
jobs:
  root:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
        if: gitforest.event_name == 'workflow_dispatch'
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::new(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert!(
            job.verify(),
            "stored signature does not match stored config"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_links_matrix_siblings_for_fail_fast() {
        let yaml = r#"
name: CI
on:
  push:
    branches: [main]
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        rust: [stable, beta]
    steps:
      - run: echo test
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        let first = jobs.get(&created[0].job_ids[0]).unwrap();
        let second = jobs.get(&created[0].job_ids[1]).unwrap();
        assert!(first.fail_fast);
        assert_eq!(first.matrix_siblings, vec![second.id.clone()]);
        assert_eq!(second.matrix_siblings, vec![first.id.clone()]);
        assert!(first.verify());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn non_matching_branch_triggers_nothing() {
        let root = git_repo_with_workflow(YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::Push {
            branch: "feature".to_string(),
        };
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert!(created.is_empty());
        assert!(jobs.list().is_empty());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn completing_root_unblocks_child() {
        let root = git_repo_with_workflow(YAML);
        let identity = Identity::generate();
        let worker = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut results = gt_compute::result::JobResultStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::WorkflowDispatch {
                inputs: HashMap::new(),
            },
            "abc123",
            None,
            0,
            "credit",
            true,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let root_id = created[0].job_ids[0].clone();
        let child_id = created[0].job_ids[1].clone();
        assert!(jobs.get(&root_id).unwrap().verify);
        let result = gt_compute::result::JobResult::new(
            &worker,
            root_id.clone(),
            0,
            b"ok",
            b"",
            vec![],
            50,
            gt_compute::result::ResourceUsage {
                peak_cpu_percent: 1.0,
                peak_ram_mb: 64,
                disk_used_mb: 1,
            },
        );
        assert!(gt_compute::dispatch::complete_job(
            &mut jobs,
            &mut results,
            result
        ));
        let (pending, failed, skipped) = unblock_ready_jobs(&mut jobs, &results);
        assert_eq!(pending, vec![child_id.clone()]);
        assert!(failed.is_empty());
        assert!(skipped.is_empty());
        let summary = summarize_run(&created[0], &jobs);
        assert_eq!(summary.total, 2);
        assert_eq!(summary.completed, 1);
        assert_eq!(summary.pending, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_carries_run_conditions() {
        let yaml = r#"
name: CI
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
  notify:
    needs: build
    if: failure()
    runs-on: ubuntu-latest
    steps:
      - run: echo notify
"#;
        let root = git_repo_with_workflow(yaml);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let notify = jobs.get(&created[0].job_ids[1]).unwrap();
        assert_eq!(notify.run_if, gt_compute::job::RunCondition::Failure);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_resolves_reusable_calls() {
        let caller = r#"
name: CI
on: push
jobs:
  greet:
    uses: ./reusable.yml
    with:
      greeting: hello
"#;
        let reusable = r#"
name: Reusable
on:
  workflow_call:
    inputs:
      greeting:
        required: true
jobs:
  say:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.greeting }}
"#;
        let root = git_repo_with_workflow(caller);
        let dir = root.join(".gitforest").join("workflows");
        std::fs::write(dir.join("reusable.yml"), reusable).unwrap();
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 1);
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert!(job.job_config.steps[0].run.contains("echo hello"));
        assert!(job.job_config.name.contains("greet/say"));
        let _ = std::fs::remove_dir_all(&root);
    }

    const CONCURRENCY_YAML: &str = r#"
name: Deploy
on:
  push:
    branches: [main]
jobs:
  deploy:
    runs-on: ubuntu-latest
    concurrency:
      group: deploy-${{ gitforest.ref_name }}
      cancel-in-progress: true
    steps:
      - run: echo deploy
"#;

    #[test]
    fn gitforest_context_builds_ref_shapes() {
        let push = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let ctx = gitforest_context(&push, "demo", "CI", "abc123", "run-1");
        assert_eq!(ctx["event_name"], "push");
        assert_eq!(ctx["ref"], "refs/heads/main");
        assert_eq!(ctx["ref_name"], "main");
        assert_eq!(ctx["sha"], "abc123");
        assert_eq!(ctx["repository"], "demo");
        assert_eq!(ctx["workflow"], "CI");
        assert_eq!(ctx["run_id"], "run-1");
        let tag = WorkflowEvent::Tag {
            tag: "v1.2.3".to_string(),
        };
        let tctx = gitforest_context(&tag, "demo", "Release", "def456", "run-2");
        assert_eq!(tctx["ref"], "refs/tags/v1.2.3");
        assert_eq!(tctx["ref_name"], "v1.2.3");
        let chained = WorkflowEvent::WorkflowRun {
            workflow: "CI".to_string(),
            file: "ci.yml".to_string(),
            conclusion: "success".to_string(),
            branch: "main".to_string(),
        };
        let cctx = gitforest_context(&chained, "demo", "Report", "abc123", "run-3");
        assert_eq!(cctx["event_name"], "workflow_run");
        assert_eq!(cctx["workflow_run_workflow"], "CI");
        assert_eq!(cctx["workflow_run_conclusion"], "success");
    }

    #[test]
    fn trigger_resolves_group_and_second_run_cancels_first() {
        let root = git_repo_with_workflow(CONCURRENCY_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let event = WorkflowEvent::Push {
            branch: "main".to_string(),
        };
        let first = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(first.len(), 1);
        let first_job = first[0].job_ids[0].clone();
        assert_eq!(
            jobs.get(&first_job).unwrap().concurrency_group,
            "deploy-main"
        );
        assert!(jobs.get(&first_job).unwrap().cancel_in_progress);
        let second = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &event,
            "def456",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(second.len(), 1);
        let second_job = second[0].job_ids[0].clone();
        assert!(matches!(
            jobs.get(&first_job).unwrap().status,
            JobStatus::Cancelled
        ));
        assert!(matches!(
            jobs.get(&second_job).unwrap().status,
            JobStatus::Pending
        ));
        assert_ne!(first[0].id, second[0].id);
        let _ = std::fs::remove_dir_all(&root);
    }

    const ENV_YAML: &str = r#"
name: Deploy
on:
  push:
    branches: [main]
jobs:
  ship:
    runs-on: ubuntu-latest
    environment:
      name: prod-${{ gitforest.ref_name }}
      url: https://example.com/prod
    steps:
      - run: echo ship
  stage:
    runs-on: ubuntu-latest
    environment: staging
    steps:
      - run: echo stage
"#;

    #[test]
    fn trigger_marks_protected_environments_pending() {
        let root = git_repo_with_workflow(ENV_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &["prod-main".to_string()],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        let mut envs: Vec<(String, bool)> = created[0]
            .job_ids
            .iter()
            .map(|id| {
                let job = jobs.get(id).unwrap();
                (job.environment.clone(), job.env_approval_pending)
            })
            .collect();
        envs.sort();
        assert_eq!(
            envs,
            vec![
                ("prod-main".to_string(), true),
                ("staging".to_string(), false),
            ]
        );
        let prod = created[0]
            .job_ids
            .iter()
            .map(|id| jobs.get(id).unwrap())
            .find(|job| job.environment == "prod-main")
            .unwrap();
        assert_eq!(prod.environment_url, "https://example.com/prod");
        let _ = std::fs::remove_dir_all(&root);
    }

    const COE_YAML: &str = r#"
name: Tolerant
on:
  push:
    branches: [main]
jobs:
  flaky:
    runs-on: ubuntu-latest
    continue-on-error: true
    strategy:
      max-parallel: 1
      matrix:
        shard: [a, b]
    steps:
      - run: echo one
      - run: echo two
        continue-on-error: true
"#;

    #[test]
    fn trigger_stamps_continue_on_error_and_max_parallel() {
        let root = git_repo_with_workflow(COE_YAML);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        for id in &created[0].job_ids {
            let job = jobs.get(id).unwrap();
            assert!(job.continue_on_error);
            assert_eq!(job.max_parallel, 1);
            assert_eq!(job.matrix_siblings.len(), 1);
            assert!(!job.job_config.steps[0].continue_on_error);
            assert!(job.job_config.steps[1].continue_on_error);
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn join_within_repo_rejects_escapes() {
        let parent = Path::new(".gitforest/workflows");
        assert_eq!(
            join_within_repo(parent, "./other.yml").unwrap(),
            PathBuf::from(".gitforest/workflows/other.yml")
        );
        assert_eq!(
            join_within_repo(parent, "../shared.yml").unwrap(),
            PathBuf::from(".gitforest/shared.yml")
        );
        assert_eq!(
            join_within_repo(parent, "../../evil.yml").unwrap(),
            PathBuf::from("evil.yml")
        );
        assert!(join_within_repo(parent, "../../../evil.yml").is_none());
        assert!(join_within_repo(parent, "/abs.yml").is_none());
    }

    #[test]
    fn trigger_cross_repo_uses_materializes() {
        let shared = r#"
name: Shared
on:
  workflow_call:
    inputs:
      greeting:
        required: true
    secrets:
      TOKEN:
        required: true
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.greeting }} ${{ secrets.TOKEN }}
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: shared/.gitforest/workflows/build.yml@main
    with:
      greeting: hi
    secrets: inherit
"#;
        let shared_root = git_repo_with_committed_workflow(shared, "build.yml");
        let caller_root = git_repo_with_workflow(caller);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let repo_dirs = HashMap::from([
            ("shared".to_string(), shared_root.clone()),
            ("caller".to_string(), caller_root.clone()),
        ]);
        let secrets = HashMap::from([("TOKEN".to_string(), "s3cret".to_string())]);
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "caller",
            &caller_root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &secrets,
            &[],
            &repo_dirs,
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 1);
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert_eq!(job.workflow_job, "hello/greet");
        assert!(job.job_config.steps[0].run.contains("hi"));
        assert!(job.job_config.steps[0].run.contains("s3cret"));
        let _ = std::fs::remove_dir_all(&shared_root);
        let _ = std::fs::remove_dir_all(&caller_root);
    }

    #[test]
    fn rerun_failed_jobs_clones_and_remaps() {
        let chain = r#"
name: Chain
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo a
  b:
    needs: [a]
    runs-on: ubuntu-latest
    steps:
      - run: echo b
  c:
    needs: [b]
    runs-on: ubuntu-latest
    steps:
      - run: echo c
"#;
        let root = git_repo_with_workflow(chain);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        assert_eq!(created.len(), 1);
        let by_name = |wants: &str| {
            created[0]
                .job_ids
                .iter()
                .find(|id| jobs.get(id).unwrap().workflow_job == wants)
                .unwrap()
                .clone()
        };
        let (a_id, b_id, c_id) = (by_name("a"), by_name("b"), by_name("c"));
        jobs.get_mut(&a_id).unwrap().status = JobStatus::Completed {
            result_id: "r".into(),
        };
        jobs.get_mut(&b_id).unwrap().status = JobStatus::Failed {
            reason: "boom".into(),
        };
        jobs.get_mut(&c_id).unwrap().status = JobStatus::Cancelled;
        let rerun = rerun_failed_jobs(&mut jobs, &mut runs, &identity, &created[0].id).unwrap();
        assert_eq!(rerun.rerun_of.as_deref(), Some(created[0].id.as_str()));
        assert_eq!(rerun.job_ids.len(), 2);
        let new_b = rerun
            .job_ids
            .iter()
            .find(|id| jobs.get(id).unwrap().workflow_job == "b")
            .unwrap()
            .clone();
        let new_c = rerun
            .job_ids
            .iter()
            .find(|id| jobs.get(id).unwrap().workflow_job == "c")
            .unwrap()
            .clone();
        assert_ne!(new_b, b_id);
        let jb = jobs.get(&new_b).unwrap();
        assert_eq!(jb.needs, vec![a_id.clone()]);
        assert!(jb.verify());
        assert!(matches!(jb.status, JobStatus::Blocked { .. }));
        let jc = jobs.get(&new_c).unwrap();
        assert_eq!(jc.needs, vec![new_b.clone()]);
        let results = gt_compute::result::JobResultStore::new();
        let (pending, _, _) = unblock_ready_jobs(&mut jobs, &results);
        assert_eq!(pending, vec![new_b.clone()]);
        let mut jobs2 = ComputeJobStore::new();
        let mut runs2 = WorkflowRunStore::new();
        let created2 = trigger_workflows(
            &mut jobs2,
            &mut runs2,
            &identity,
            &identity.peer_id(),
            "demo",
            &root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &HashMap::new(),
        );
        for id in &created2[0].job_ids {
            jobs2.get_mut(id).unwrap().status = JobStatus::Completed {
                result_id: "r".into(),
            };
        }
        assert!(rerun_failed_jobs(&mut jobs2, &mut runs2, &identity, &created2[0].id).is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn trigger_unknown_remote_repo_skips_workflow() {
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: nosuch/.gitforest/workflows/build.yml@main
"#;
        let caller_root = git_repo_with_workflow(caller);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let repo_dirs = HashMap::from([("caller".to_string(), caller_root.clone())]);
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "caller",
            &caller_root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &repo_dirs,
        );
        assert!(created.is_empty());
        let _ = std::fs::remove_dir_all(&caller_root);
    }

    #[test]
    fn trigger_remote_composite_step_materializes() {
        let action = r#"
name: Echo
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "said ${{ inputs.text }}"
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: shared/.gitforest/actions/echo/action.yml@main
        with:
          text: hello-remote
  local:
    runs-on: ubuntu-latest
    steps:
      - uses: ../actions/near/action.yml
        with:
          text: hello-local
"#;
        let near = r#"
name: Near
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "said ${{ inputs.text }}"
"#;
        let shared_root =
            git_repo_with_committed_files(&[(".gitforest/actions/echo/action.yml", action)]);
        let caller_root = git_repo_with_workflow(caller);
        let near_dir = caller_root.join(".gitforest").join("actions").join("near");
        std::fs::create_dir_all(&near_dir).unwrap();
        std::fs::write(near_dir.join("action.yml"), near).unwrap();
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let repo_dirs = HashMap::from([
            ("shared".to_string(), shared_root.clone()),
            ("caller".to_string(), caller_root.clone()),
        ]);
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "caller",
            &caller_root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &repo_dirs,
        );
        assert_eq!(created.len(), 1);
        assert_eq!(created[0].job_ids.len(), 2);
        let runs: Vec<String> = created[0]
            .job_ids
            .iter()
            .map(|id| jobs.get(id).unwrap().job_config.steps[0].run.clone())
            .collect();
        assert!(
            runs.iter().any(|r| r.contains("said hello-remote")),
            "{runs:?}"
        );
        assert!(
            runs.iter().any(|r| r.contains("said hello-local")),
            "{runs:?}"
        );
        let _ = std::fs::remove_dir_all(&shared_root);
        let _ = std::fs::remove_dir_all(&caller_root);
    }

    #[test]
    fn trigger_registry_step_actions_survive_materialize_scan() {
        let caller = r#"
name: Caller
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: gitforest/llm@v1
        with:
          model: m
          prompt: p
"#;
        let caller_root = git_repo_with_workflow(caller);
        let identity = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut runs = WorkflowRunStore::new();
        let repo_dirs = HashMap::from([("caller".to_string(), caller_root.clone())]);
        let created = trigger_workflows(
            &mut jobs,
            &mut runs,
            &identity,
            &identity.peer_id(),
            "caller",
            &caller_root,
            &WorkflowEvent::Push {
                branch: "main".to_string(),
            },
            "abc123",
            None,
            0,
            "credit",
            false,
            &HashMap::new(),
            &[],
            &repo_dirs,
        );
        assert_eq!(created.len(), 1);
        let job = jobs.get(&created[0].job_ids[0]).unwrap();
        assert!(job.job_config.steps[0].run.contains("already present"));
        assert!(job.job_config.steps[1].run.contains("llm run"));
        let _ = std::fs::remove_dir_all(&caller_root);
    }
}
