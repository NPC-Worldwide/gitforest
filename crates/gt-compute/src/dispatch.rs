use chrono::Utc;
use gt_core::identity::PeerId;

use crate::job::{ComputeJob, ComputeJobStore, ComputeRequirements, JobStatus};
use crate::result::{JobResult, JobResultStore};

pub struct LocalCaps {
    pub cpu_cores: u32,
    pub ram_mb: u64,
    pub disk_mb: u64,
    pub has_gpu: bool,
    pub has_network: bool,
    pub docker: bool,
}

pub fn caps_match(caps: &LocalCaps, req: &ComputeRequirements) -> bool {
    caps.cpu_cores >= req.min_cpu_cores
        && caps.ram_mb >= req.min_ram_mb
        && caps.disk_mb >= req.min_disk_mb
        && (!req.gpu_required || caps.has_gpu)
        && (!req.network_access || caps.has_network)
}

pub fn selectable<'a>(
    store: &'a ComputeJobStore,
    caps: &LocalCaps,
    repo_available: &dyn Fn(&ComputeJob) -> bool,
) -> Vec<&'a ComputeJob> {
    store
        .pending()
        .into_iter()
        .filter(|j| {
            caps_match(caps, &j.requirements)
                && repo_available(j)
                && !j.env_approval_pending
                && !group_occupied(store, &j.concurrency_group, &j.id)
                && !matrix_saturated(store, j)
                && ((j.job_config.services.is_empty() && j.job_config.container.is_none())
                    || caps.docker)
        })
        .collect()
}

fn matrix_saturated(store: &ComputeJobStore, job: &ComputeJob) -> bool {
    if job.max_parallel == 0 || job.matrix_siblings.is_empty() {
        return false;
    }
    let active = job
        .matrix_siblings
        .iter()
        .filter(|id| {
            store.get(id).is_some_and(|sibling| {
                matches!(
                    sibling.status,
                    JobStatus::Running { .. }
                        | JobStatus::Matched { .. }
                        | JobStatus::Verifying { .. }
                )
            })
        })
        .count();
    active >= job.max_parallel as usize
}

pub fn approve_job(store: &mut ComputeJobStore, job_id: &str) -> bool {
    let Some(job) = store.get_mut(job_id) else {
        return false;
    };
    if !job.env_approval_pending {
        return false;
    }
    job.env_approval_pending = false;
    true
}

fn group_occupied(store: &ComputeJobStore, group: &str, exclude_id: &str) -> bool {
    if group.is_empty() {
        return false;
    }
    store.list().iter().any(|j| {
        j.id != exclude_id
            && j.concurrency_group == group
            && matches!(
                j.status,
                JobStatus::Running { .. } | JobStatus::Matched { .. } | JobStatus::Verifying { .. }
            )
    })
}

pub fn cancel_group_jobs(
    store: &mut ComputeJobStore,
    group: &str,
    keep: &std::collections::HashSet<String>,
) -> Vec<(String, Option<PeerId>)> {
    if group.is_empty() {
        return Vec::new();
    }
    let targets: Vec<String> = store
        .list()
        .iter()
        .filter(|j| {
            j.concurrency_group == group
                && !keep.contains(&j.id)
                && matches!(
                    j.status,
                    JobStatus::Pending
                        | JobStatus::Blocked { .. }
                        | JobStatus::Matched { .. }
                        | JobStatus::Verifying { .. }
                )
        })
        .map(|j| j.id.clone())
        .collect();
    let mut cancelled = Vec::new();
    for id in targets {
        let worker = match store.get(&id).map(|j| &j.status) {
            Some(JobStatus::Pending) | Some(JobStatus::Blocked { .. }) => Some(None),
            Some(JobStatus::Matched { worker, .. }) => Some(Some(worker.clone())),
            Some(JobStatus::Verifying { verifier, .. }) => Some(verifier.clone()),
            _ => None,
        };
        if let Some(worker) = worker {
            if let Some(job) = store.get_mut(&id) {
                job.status = JobStatus::Cancelled;
            }
            cancelled.push((id, worker));
        }
    }
    cancelled
}

pub fn running_count(store: &ComputeJobStore) -> usize {
    store
        .list()
        .iter()
        .filter(|j| matches!(j.status, JobStatus::Running { .. }))
        .count()
}

pub fn claim_next_job(
    store: &mut ComputeJobStore,
    caps: &LocalCaps,
    worker: PeerId,
    repo_available: &dyn Fn(&ComputeJob) -> bool,
) -> Option<ComputeJob> {
    let mut jobs = selectable(store, caps, repo_available);
    jobs.sort_by(|a, b| {
        (&a.created_at, &a.workflow_job, &a.id).cmp(&(&b.created_at, &b.workflow_job, &b.id))
    });
    let id = jobs.first().map(|j| j.id.clone())?;
    let job = store.get_mut(&id)?;
    job.status = JobStatus::Running {
        worker,
        started_at: Utc::now(),
    };
    Some(job.clone())
}

pub fn complete_job(
    jobs: &mut ComputeJobStore,
    results: &mut JobResultStore,
    result: JobResult,
) -> bool {
    let Some(job) = jobs.get_mut(&result.job_id) else {
        return false;
    };
    let result_id = result.id.clone();
    results.add(result);
    job.status = JobStatus::Completed { result_id };
    true
}

pub fn fail_job(jobs: &mut ComputeJobStore, job_id: &str, reason: String) -> bool {
    let Some(job) = jobs.get_mut(job_id) else {
        return false;
    };
    job.status = JobStatus::Failed { reason };
    cancel_matrix_siblings(jobs, job_id);
    true
}

pub fn fail_job_fail_fast(
    jobs: &mut ComputeJobStore,
    job_id: &str,
    reason: String,
) -> Vec<(String, Option<PeerId>)> {
    let Some(job) = jobs.get_mut(job_id) else {
        return Vec::new();
    };
    job.status = JobStatus::Failed { reason };
    cancel_matrix_siblings(jobs, job_id)
}

pub fn cancel_matrix_siblings(
    jobs: &mut ComputeJobStore,
    job_id: &str,
) -> Vec<(String, Option<PeerId>)> {
    let (siblings, fail_fast) = match jobs.get(job_id) {
        Some(job) => (job.matrix_siblings.clone(), job.fail_fast),
        None => return Vec::new(),
    };
    if siblings.is_empty() || !fail_fast {
        return Vec::new();
    }
    let mut cancelled = Vec::new();
    for id in &siblings {
        if id == job_id {
            continue;
        }
        let worker = match jobs.get(id).map(|j| &j.status) {
            Some(JobStatus::Pending) | Some(JobStatus::Blocked { .. }) => Some(None),
            Some(JobStatus::Matched { worker, .. }) => Some(Some(worker.clone())),
            Some(JobStatus::Verifying { verifier, .. }) => Some(verifier.clone()),
            _ => None,
        };
        if let Some(worker) = worker {
            if let Some(job) = jobs.get_mut(id) {
                job.status = JobStatus::Cancelled;
            }
            cancelled.push((id.clone(), worker));
        }
    }
    cancelled
}

pub fn substitute_needs(
    text: &str,
    needs: &[(String, std::collections::HashMap<String, String>)],
) -> String {
    let mut out = text.to_string();
    for (name, outputs) in needs {
        for (key, value) in outputs {
            let plain = format!("${{{{ needs.{name}.outputs.{key} }}}}");
            out = out.replace(&plain, value);
            let tight = format!("${{{{needs.{name}.outputs.{key}}}}}");
            out = out.replace(&tight, value);
        }
    }
    let mut at = 0;
    while let Some(rel) = out[at..].find("${{") {
        let start = at + rel;
        let Some(rel_end) = out[start..].find("}}") else {
            break;
        };
        let end = start + rel_end + 2;
        let inner = out[start + 3..end - 2].trim();
        let mut parts = inner.split('.');
        let blank = match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some("needs"), Some(name), Some("outputs"), Some(_)) => {
                parts.next().is_none() && needs.iter().any(|(n, _)| n == name)
            }
            _ => false,
        };
        if blank {
            out.replace_range(start..end, "");
            at = start;
        } else {
            at = end;
        }
    }
    out
}

fn resolve_call_outputs(
    store: &ComputeJobStore,
    results: &JobResultStore,
    need_job: &ComputeJob,
) -> Option<(String, std::collections::HashMap<String, String>)> {
    let call_id = need_job.call_id.as_ref()?;
    if need_job.call_outputs.is_empty() {
        return None;
    }
    let all = store.list();
    let mut out = std::collections::HashMap::new();
    for (name, expr) in &need_job.call_outputs {
        let Some((inner, key)) = crate::workflow::parse_call_output_ref(expr) else {
            continue;
        };
        let target = format!("{call_id}/{inner}");
        let mut hits = all.iter().filter(|j| j.workflow_job == target);
        let (Some(hit), None) = (hits.next(), hits.next()) else {
            continue;
        };
        if let Some(value) = results
            .for_job(&hit.id)
            .and_then(|r| r.outputs.get(&key).cloned())
        {
            out.insert(name.clone(), value);
        }
    }
    Some((call_id.clone(), out))
}

fn call_rollup_result(store: &ComputeJobStore, call_id: &str) -> String {
    let prefix = format!("{call_id}/");
    let mut result = "success";
    for job in store
        .list()
        .iter()
        .filter(|j| j.workflow_job.starts_with(&prefix))
    {
        let single = match &job.status {
            JobStatus::Completed { .. } => "success",
            JobStatus::Cancelled => "cancelled",
            _ if job.continue_on_error && matches!(job.status, JobStatus::Failed { .. }) => {
                "success"
            }
            _ => "failure",
        };
        if single == "cancelled" {
            return "cancelled".to_string();
        }
        if single == "failure" {
            result = "failure";
        }
    }
    result.to_string()
}

fn collect_needs_context(
    store: &ComputeJobStore,
    results: &JobResultStore,
    need_ids: &[String],
) -> Vec<(String, std::collections::HashMap<String, String>)> {
    let mut needs: Vec<(String, std::collections::HashMap<String, String>)> = Vec::new();
    let mut calls = std::collections::HashSet::new();
    for need in need_ids {
        let Some(need_job) = store.get(need) else {
            continue;
        };
        if need_job.workflow_job.is_empty() {
            continue;
        }
        let outputs = results
            .for_job(need)
            .map(|r| r.outputs.clone())
            .unwrap_or_default();
        needs.push((need_job.workflow_job.clone(), outputs));
        if let Some((call, mapped)) = resolve_call_outputs(store, results, need_job) {
            if calls.insert(call.clone()) {
                needs.push((call, mapped));
            }
        }
    }
    needs
}

fn apply_needs_context(
    job: &mut ComputeJob,
    needs: &[(String, std::collections::HashMap<String, String>)],
) {
    if needs.is_empty() {
        return;
    }
    for step in &mut job.job_config.steps {
        step.name = substitute_needs(&step.name, needs);
        step.run = substitute_needs(&step.run, needs);
        if let Some(dir) = step.working_directory.take() {
            step.working_directory = Some(substitute_needs(&dir, needs));
        }
    }
    for value in job.job_config.env.values_mut() {
        *value = substitute_needs(value, needs);
    }
    for value in job.job_config.secrets.values_mut() {
        *value = substitute_needs(value, needs);
    }
}

fn job_if_context(
    store: &ComputeJobStore,
    results: &JobResultStore,
    need_ids: &[String],
    job_id: &str,
) -> crate::ifexpr::IfContext {
    let mut ctx = crate::ifexpr::IfContext::default();
    let mut failure = false;
    let mut cancelled = false;
    for need in need_ids {
        let Some(need_job) = store.get(need) else {
            continue;
        };
        if need_job.workflow_job.is_empty() {
            continue;
        }
        let forgiven =
            need_job.continue_on_error && matches!(need_job.status, JobStatus::Failed { .. });
        let result = match &need_job.status {
            JobStatus::Completed { .. } => "success",
            JobStatus::Cancelled => {
                cancelled = true;
                "cancelled"
            }
            _ if forgiven => "success",
            _ => {
                failure = true;
                "failure"
            }
        };
        ctx.results
            .insert(need_job.workflow_job.clone(), result.to_string());
        ctx.needs_outputs.insert(
            need_job.workflow_job.clone(),
            results
                .for_job(need)
                .map(|r| r.outputs.clone())
                .unwrap_or_default(),
        );
        if let Some((call, mapped)) = resolve_call_outputs(store, results, need_job) {
            ctx.needs_outputs.entry(call.clone()).or_insert(mapped);
            ctx.results
                .entry(call.clone())
                .or_insert_with(|| call_rollup_result(store, &call));
        }
    }
    ctx.failure = failure;
    ctx.cancelled = cancelled;
    ctx.success = !failure && !cancelled;
    if let Some(job) = store.get(job_id) {
        ctx.env = job.job_config.env.clone();
        ctx.inputs = job.dispatch_inputs.clone();
        ctx.matrix = job.matrix.clone();
    }
    ctx
}

fn reduce_step_conditions(job: &mut ComputeJob, ctx: &crate::ifexpr::IfContext) {
    for step in &mut job.job_config.steps {
        if let crate::job::RunCondition::Expr(raw) = &step.condition {
            let reduced = crate::ifexpr::substitute_known(raw, ctx);
            step.condition = crate::job::RunCondition::Expr(reduced);
        }
    }
}

pub fn unblock_ready_jobs(
    store: &mut ComputeJobStore,
    results: &JobResultStore,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut now_pending = Vec::new();
    let mut newly_failed = Vec::new();
    let mut skipped = Vec::new();
    let blocked: Vec<(String, Vec<String>, crate::job::RunCondition)> = store
        .list()
        .iter()
        .filter_map(|j| match &j.status {
            JobStatus::Blocked { needs } => Some((j.id.clone(), needs.clone(), j.run_if.clone())),
            _ => None,
        })
        .collect();
    for (id, needs, condition) in blocked {
        let mut bad_need: Option<String> = None;
        let mut unforgiven_bad: Option<String> = None;
        let mut cancelled_need: Option<String> = None;
        let mut waiting = false;
        for need in &needs {
            let forgiven = store.get(need).is_some_and(|j| {
                j.continue_on_error && matches!(j.status, JobStatus::Failed { .. })
            });
            match store.get(need).map(|j| &j.status) {
                Some(JobStatus::Completed { .. }) => {}
                Some(JobStatus::Cancelled) => {
                    bad_need = Some(need.clone());
                    unforgiven_bad = Some(need.clone());
                    cancelled_need = Some(need.clone());
                }
                Some(JobStatus::Failed { .. }) | Some(JobStatus::Disputed) => {
                    bad_need = Some(need.clone());
                    if !forgiven {
                        unforgiven_bad = Some(need.clone());
                    }
                }
                Some(_) => {
                    waiting = true;
                }
                None => {
                    bad_need = Some(need.clone());
                    unforgiven_bad = Some(need.clone());
                }
            }
        }
        if waiting {
            continue;
        }
        let release = match &condition {
            crate::job::RunCondition::Always => true,
            crate::job::RunCondition::Failure => bad_need.is_some(),
            crate::job::RunCondition::Cancelled => cancelled_need.is_some(),
            crate::job::RunCondition::Success => unforgiven_bad.is_none(),
            crate::job::RunCondition::Expr(raw) => {
                let ctx = job_if_context(store, results, &needs, &id);
                crate::ifexpr::eval(raw, &ctx)
            }
        };
        if !release {
            if matches!(
                condition,
                crate::job::RunCondition::Failure
                    | crate::job::RunCondition::Cancelled
                    | crate::job::RunCondition::Expr(_)
            ) {
                if let Some(job) = store.get_mut(&id) {
                    job.status = JobStatus::Cancelled;
                }
                skipped.push(id);
            } else if let Some(need) = unforgiven_bad.or(bad_need) {
                if let Some(job) = store.get_mut(&id) {
                    job.status = JobStatus::Failed {
                        reason: format!("need {need} did not complete"),
                    };
                }
                newly_failed.push(id);
            }
            continue;
        }
        let context = collect_needs_context(store, results, &needs);
        if let Some(job) = store.get_mut(&id) {
            job.status = JobStatus::Pending;
            apply_needs_context(job, &context);
        }
        let if_ctx = job_if_context(store, results, &needs, &id);
        if let Some(job) = store.get_mut(&id) {
            reduce_step_conditions(job, &if_ctx);
        }
        now_pending.push(id);
    }
    (now_pending, newly_failed, skipped)
}

pub fn requeue_stale_matches(store: &mut ComputeJobStore, timeout_secs: i64) -> Vec<String> {
    let now = Utc::now();
    let stale: Vec<String> = store
        .list()
        .iter()
        .filter_map(|j| match &j.status {
            JobStatus::Matched { matched_at, .. } => {
                if (now - *matched_at).num_seconds() > timeout_secs {
                    Some(j.id.clone())
                } else {
                    None
                }
            }
            _ => None,
        })
        .collect();
    for id in &stale {
        if let Some(job) = store.get_mut(id) {
            job.status = JobStatus::Pending;
        }
    }
    stale
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job::{JobConfig, JobStep};
    use crate::result::ResourceUsage;
    use gt_core::identity::Identity;
    use std::collections::HashMap;

    fn sample_job(
        submitter: &Identity,
        repo: &str,
        requirements: ComputeRequirements,
    ) -> ComputeJob {
        let config = JobConfig {
            name: "ci".into(),
            steps: vec![JobStep {
                name: "test".into(),
                run: "cargo test".into(),
                condition: crate::job::RunCondition::Success,
                continue_on_error: false,
                working_directory: None,
                id: None,
            }],
            docker_image: "rust:1.75".into(),
            timeout_secs: 600,
            env: HashMap::new(),
            secrets: HashMap::new(),
            artifacts: vec![],
            outputs: Default::default(),
            services: vec![],
            container: None,
            job_token: String::new(),
        };
        ComputeJob::new(
            submitter,
            submitter.peer_id(),
            repo.into(),
            "abc123".into(),
            config,
            requirements,
            1000,
            "lightning".into(),
        )
    }

    fn sample_result(worker: &Identity, job_id: &str) -> JobResult {
        JobResult::new(
            worker,
            job_id.into(),
            0,
            b"ok",
            b"",
            vec![],
            100,
            ResourceUsage {
                peak_cpu_percent: 10.0,
                peak_ram_mb: 256,
                disk_used_mb: 10,
            },
        )
    }

    fn caps() -> LocalCaps {
        LocalCaps {
            cpu_cores: 4,
            ram_mb: 8192,
            disk_mb: 50000,
            has_gpu: false,
            has_network: false,
            docker: false,
        }
    }

    #[test]
    fn test_caps_match() {
        let c = caps();
        assert!(caps_match(&c, &ComputeRequirements::default()));
        assert!(!caps_match(
            &c,
            &ComputeRequirements {
                min_cpu_cores: 8,
                ..ComputeRequirements::default()
            }
        ));
        assert!(!caps_match(
            &c,
            &ComputeRequirements {
                gpu_required: true,
                ..ComputeRequirements::default()
            }
        ));
        assert!(!caps_match(
            &c,
            &ComputeRequirements {
                network_access: true,
                ..ComputeRequirements::default()
            }
        ));
        let net_caps = LocalCaps {
            has_network: true,
            ..caps()
        };
        assert!(caps_match(
            &net_caps,
            &ComputeRequirements {
                network_access: true,
                ..ComputeRequirements::default()
            }
        ));
        let gpu_caps = LocalCaps {
            has_gpu: true,
            ..caps()
        };
        assert!(caps_match(
            &gpu_caps,
            &ComputeRequirements {
                gpu_required: true,
                ..ComputeRequirements::default()
            }
        ));
    }

    #[test]
    fn test_local_workers_skip_services_jobs() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut job = sample_job(&id, "r", ComputeRequirements::default());
        job.job_config.services = vec![crate::job::JobService {
            name: "db".into(),
            image: "postgres:16".into(),
            env: std::collections::HashMap::new(),
            ports: vec![],
            health: None,
        }];
        let job_id = job.id.clone();
        store.add(job);
        assert!(selectable(&store, &caps(), &|_| true).is_empty());
        let docker_caps = LocalCaps {
            docker: true,
            ..caps()
        };
        let found = selectable(&store, &docker_caps, &|_| true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, job_id);
    }

    #[test]
    fn test_local_workers_skip_container_jobs() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut job = sample_job(&id, "r", ComputeRequirements::default());
        job.job_config.container = Some(crate::job::JobContainer {
            image: "node:20".into(),
            credentials: None,
            env: std::collections::HashMap::new(),
            ports: vec![],
            volumes: vec![],
            resources: crate::job::ContainerResources::default(),
            options: String::new(),
        });
        let job_id = job.id.clone();
        store.add(job);
        assert!(selectable(&store, &caps(), &|_| true).is_empty());
        let docker_caps = LocalCaps {
            docker: true,
            ..caps()
        };
        let found = selectable(&store, &docker_caps, &|_| true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, job_id);
    }

    #[test]
    fn test_selectable_filters() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let small = sample_job(&id, "repo-a", ComputeRequirements::default());
        let big = sample_job(
            &id,
            "repo-a",
            ComputeRequirements {
                min_cpu_cores: 64,
                ..ComputeRequirements::default()
            },
        );
        let remote = sample_job(&id, "repo-b", ComputeRequirements::default());
        let small_id = small.id.clone();
        store.add(small);
        store.add(big);
        store.add(remote);

        let found = selectable(&store, &caps(), &|j| j.repo_name == "repo-a");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, small_id);
    }

    #[test]
    fn test_selectable_gates_gpu_jobs_on_caps() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let gpu_job = sample_job(
            &id,
            "repo-a",
            ComputeRequirements {
                gpu_required: true,
                ..ComputeRequirements::default()
            },
        );
        let gpu_id = gpu_job.id.clone();
        store.add(gpu_job);
        assert!(selectable(&store, &caps(), &|_| true).is_empty());
        let gpu_caps = LocalCaps {
            has_gpu: true,
            ..caps()
        };
        let found = selectable(&store, &gpu_caps, &|_| true);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, gpu_id);
    }

    #[test]
    fn test_selectable_skips_non_pending() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let job = sample_job(&id, "repo-a", ComputeRequirements::default());
        let job_id = job.id.clone();
        store.add(job);
        assert!(fail_job(&mut store, &job_id, "nope".into()));
        let found = selectable(&store, &caps(), &|_| true);
        assert!(found.is_empty());
    }

    fn matrix_pair(fail_fast: bool) -> (ComputeJobStore, String, String, String) {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut first = sample_job(&id, "repo-a", ComputeRequirements::default());
        let mut second = sample_job(&id, "repo-a", ComputeRequirements::default());
        let other = sample_job(&id, "repo-a", ComputeRequirements::default());
        let first_id = first.id.clone();
        let second_id = second.id.clone();
        let other_id = other.id.clone();
        first.matrix_siblings = vec![second_id.clone()];
        second.matrix_siblings = vec![first_id.clone()];
        first.fail_fast = fail_fast;
        second.fail_fast = fail_fast;
        store.add(first);
        store.add(second);
        store.add(other);
        (store, first_id, second_id, other_id)
    }

    #[test]
    fn test_fail_fast_cancels_pending_siblings() {
        let (mut store, first_id, second_id, other_id) = matrix_pair(true);
        let cancelled = fail_job_fail_fast(&mut store, &first_id, "boom".into());
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].0, second_id);
        assert!(cancelled[0].1.is_none());
        assert!(matches!(
            store.get(&first_id).unwrap().status,
            JobStatus::Failed { .. }
        ));
        assert!(matches!(
            store.get(&second_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert!(matches!(
            store.get(&other_id).unwrap().status,
            JobStatus::Pending
        ));
    }

    #[test]
    fn test_fail_fast_opt_out_leaves_siblings() {
        let (mut store, first_id, second_id, _) = matrix_pair(false);
        let cancelled = fail_job_fail_fast(&mut store, &first_id, "boom".into());
        assert!(cancelled.is_empty());
        assert!(matches!(
            store.get(&second_id).unwrap().status,
            JobStatus::Pending
        ));
    }

    #[test]
    fn test_fail_fast_skips_running_and_terminal_siblings() {
        let id = Identity::generate();
        let worker = Identity::generate().peer_id();
        let mut store = ComputeJobStore::new();
        let mut first = sample_job(&id, "repo-a", ComputeRequirements::default());
        let mut running = sample_job(&id, "repo-a", ComputeRequirements::default());
        let mut matched = sample_job(&id, "repo-a", ComputeRequirements::default());
        let first_id = first.id.clone();
        let running_id = running.id.clone();
        let matched_id = matched.id.clone();
        first.matrix_siblings = vec![running_id.clone(), matched_id.clone()];
        first.fail_fast = true;
        running.status = JobStatus::Running {
            worker: worker.clone(),
            started_at: chrono::Utc::now(),
        };
        matched.status = JobStatus::Matched {
            worker: worker.clone(),
            matched_at: chrono::Utc::now(),
        };
        store.add(first);
        store.add(running);
        store.add(matched);
        let cancelled = fail_job_fail_fast(&mut store, &first_id, "boom".into());
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].0, matched_id);
        assert_eq!(cancelled[0].1, Some(worker));
        assert!(matches!(
            store.get(&running_id).unwrap().status,
            JobStatus::Running { .. }
        ));
        assert!(matches!(
            store.get(&matched_id).unwrap().status,
            JobStatus::Cancelled
        ));
    }

    #[test]
    fn test_fail_fast_unknown_job_cancels_nothing() {
        let (mut store, _, _, _) = matrix_pair(true);
        assert!(fail_job_fail_fast(&mut store, "missing", "x".into()).is_empty());
        assert!(cancel_matrix_siblings(&mut store, "missing").is_empty());
    }

    #[test]
    fn test_claim_marks_running() {
        let id = Identity::generate();
        let worker = Identity::generate().peer_id();
        let mut store = ComputeJobStore::new();
        let job = sample_job(&id, "repo-a", ComputeRequirements::default());
        let job_id = job.id.clone();
        store.add(job);

        let claimed = claim_next_job(&mut store, &caps(), worker.clone(), &|_| true).unwrap();
        assert_eq!(claimed.id, job_id);
        assert!(matches!(
            store.get(&job_id).unwrap().status,
            JobStatus::Running { .. }
        ));
        assert_eq!(running_count(&store), 1);
        assert!(claim_next_job(&mut store, &caps(), worker, &|_| true).is_none());
    }

    #[test]
    fn test_complete_and_fail() {
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut jobs = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let job = sample_job(&id, "repo-a", ComputeRequirements::default());
        let job_id = job.id.clone();
        jobs.add(job);

        let result = sample_result(&worker, &job_id);
        let result_id = result.id.clone();
        assert!(complete_job(&mut jobs, &mut results, result));
        assert!(matches!(
            jobs.get(&job_id).unwrap().status,
            JobStatus::Completed { .. }
        ));
        assert_eq!(results.for_job(&job_id).unwrap().id, result_id);
        assert!(!complete_job(
            &mut jobs,
            &mut results,
            sample_result(&worker, "nope")
        ));

        assert!(fail_job(&mut jobs, &job_id, "x".into()));
        assert!(matches!(
            jobs.get(&job_id).unwrap().status,
            JobStatus::Failed { .. }
        ));
        assert!(!fail_job(&mut jobs, "nope", "x".into()));
    }

    #[test]
    fn test_requeue_stale_matches() {
        let id = Identity::generate();
        let worker = Identity::generate().peer_id();
        let mut store = ComputeJobStore::new();
        let mut stale = sample_job(&id, "r", ComputeRequirements::default());
        stale.status = JobStatus::Matched {
            worker: worker.clone(),
            matched_at: Utc::now() - chrono::Duration::hours(2),
        };
        let stale_id = stale.id.clone();
        let mut fresh = sample_job(&id, "r", ComputeRequirements::default());
        fresh.status = JobStatus::Matched {
            worker,
            matched_at: Utc::now(),
        };
        let fresh_id = fresh.id.clone();
        store.add(stale);
        store.add(fresh);
        let out = requeue_stale_matches(&mut store, 1800);
        assert_eq!(out, vec![stale_id.clone()]);
        assert!(matches!(
            store.get(&stale_id).unwrap().status,
            JobStatus::Pending
        ));
        assert!(matches!(
            store.get(&fresh_id).unwrap().status,
            JobStatus::Matched { .. }
        ));
    }

    #[test]
    fn test_blocked_jobs_skip_selection_until_unblocked() {
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let mut root = sample_job(&id, "r", ComputeRequirements::default());
        root.status = JobStatus::Running {
            worker: worker.peer_id(),
            started_at: Utc::now(),
        };
        let root_id = root.id.clone();
        let mut child = sample_job(&id, "r", ComputeRequirements::default());
        child.status = JobStatus::Blocked {
            needs: vec![root_id.clone()],
        };
        let child_id = child.id.clone();
        store.add(root);
        store.add(child);
        assert!(selectable(&store, &caps(), &|_| true).is_empty());
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert!(pending.is_empty());
        assert!(failed.is_empty());
        assert!(skipped.is_empty());
        let result = sample_result(&worker, &root_id);
        assert!(complete_job(&mut store, &mut results, result));
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![child_id.clone()]);
        assert!(failed.is_empty());
        assert!(skipped.is_empty());
        assert_eq!(selectable(&store, &caps(), &|_| true).len(), 1);
    }

    #[test]
    fn test_blocked_job_fails_when_need_fails_or_vanishes() {
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut bad = sample_job(&id, "r", ComputeRequirements::default());
        bad.status = JobStatus::Failed {
            reason: "boom".into(),
        };
        let bad_id = bad.id.clone();
        let mut child = sample_job(&id, "r", ComputeRequirements::default());
        child.status = JobStatus::Blocked {
            needs: vec![bad_id.clone()],
        };
        let child_id = child.id.clone();
        let mut orphan = sample_job(&id, "r", ComputeRequirements::default());
        orphan.status = JobStatus::Blocked {
            needs: vec!["ghost".into()],
        };
        let orphan_id = orphan.id.clone();
        store.add(bad);
        store.add(child);
        store.add(orphan);
        let results = JobResultStore::new();
        let (pending, mut failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert!(pending.is_empty());
        assert!(skipped.is_empty());
        failed.sort();
        let mut expected = vec![child_id.clone(), orphan_id.clone()];
        expected.sort();
        assert_eq!(failed, expected);
        assert!(matches!(
            store.get(&child_id).unwrap().status,
            JobStatus::Failed { .. }
        ));
    }

    #[test]
    fn test_run_conditions_gate_unblock() {
        use crate::job::RunCondition;
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut bad = sample_job(&id, "r", ComputeRequirements::default());
        bad.status = JobStatus::Failed {
            reason: "boom".into(),
        };
        let bad_id = bad.id.clone();
        let mut good = sample_job(&id, "r", ComputeRequirements::default());
        good.status = JobStatus::Completed {
            result_id: "res".into(),
        };
        let good_id = good.id.clone();
        store.add(bad);
        store.add(good);
        let mut on_failure = sample_job(&id, "r", ComputeRequirements::default());
        on_failure.run_if = RunCondition::Failure;
        on_failure.status = JobStatus::Blocked {
            needs: vec![bad_id.clone()],
        };
        let on_failure_id = on_failure.id.clone();
        let mut on_failure_skip = sample_job(&id, "r", ComputeRequirements::default());
        on_failure_skip.run_if = RunCondition::Failure;
        on_failure_skip.status = JobStatus::Blocked {
            needs: vec![good_id.clone()],
        };
        let skip_id = on_failure_skip.id.clone();
        let mut always = sample_job(&id, "r", ComputeRequirements::default());
        always.run_if = RunCondition::Always;
        always.status = JobStatus::Blocked {
            needs: vec![bad_id.clone()],
        };
        let always_id = always.id.clone();
        store.add(on_failure);
        store.add(on_failure_skip);
        store.add(always);
        let results = JobResultStore::new();
        let (mut pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        pending.sort();
        let mut expected = vec![on_failure_id.clone(), always_id.clone()];
        expected.sort();
        assert_eq!(pending, expected);
        assert!(failed.is_empty());
        assert_eq!(skipped, vec![skip_id.clone()]);
        assert!(matches!(
            store.get(&skip_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert_eq!(
            RunCondition::parse("failure()"),
            Some(RunCondition::Failure)
        );
        assert_eq!(RunCondition::parse("Always"), Some(RunCondition::Always));
        assert_eq!(RunCondition::parse("success"), Some(RunCondition::Success));
        assert_eq!(RunCondition::parse("sometimes"), None);
    }

    #[test]
    fn test_unblock_cancelled_condition() {
        use crate::job::RunCondition;
        let id = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut wiped = sample_job(&id, "r", ComputeRequirements::default());
        wiped.status = JobStatus::Cancelled;
        let wiped_id = wiped.id.clone();
        let mut good = sample_job(&id, "r", ComputeRequirements::default());
        good.status = JobStatus::Completed {
            result_id: "res".into(),
        };
        let good_id = good.id.clone();
        store.add(wiped);
        store.add(good);
        let mut on_cancel = sample_job(&id, "r", ComputeRequirements::default());
        on_cancel.run_if = RunCondition::Cancelled;
        on_cancel.status = JobStatus::Blocked {
            needs: vec![wiped_id.clone()],
        };
        let on_cancel_id = on_cancel.id.clone();
        let mut on_cancel_skip = sample_job(&id, "r", ComputeRequirements::default());
        on_cancel_skip.run_if = RunCondition::Cancelled;
        on_cancel_skip.status = JobStatus::Blocked {
            needs: vec![good_id.clone()],
        };
        let skip_id = on_cancel_skip.id.clone();
        store.add(on_cancel);
        store.add(on_cancel_skip);
        let results = JobResultStore::new();
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![on_cancel_id.clone()]);
        assert!(failed.is_empty());
        assert_eq!(skipped, vec![skip_id.clone()]);
        assert!(matches!(
            store.get(&skip_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert_eq!(
            RunCondition::parse("cancelled()"),
            Some(RunCondition::Cancelled)
        );
    }

    #[test]
    fn test_unblock_substitutes_needs_outputs() {
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let mut root = sample_job(&id, "r", ComputeRequirements::default());
        root.workflow_job = "build".to_string();
        let root_id = root.id.clone();
        store.add(root);
        let mut child = sample_job(&id, "r", ComputeRequirements::default());
        child.workflow_job = "test".to_string();
        child.job_config.steps[0].run =
            "echo ${{ needs.build.outputs.SHA }} ${{needs.build.outputs.MISSING}} ${{ needs.ghost.outputs.X }}"
                .to_string();
        child.job_config.env.insert(
            "TAG".to_string(),
            "v-${{ needs.build.outputs.SHA }}".to_string(),
        );
        child.status = JobStatus::Blocked {
            needs: vec![root_id.clone()],
        };
        let child_id = child.id.clone();
        store.add(child);
        let mut result = sample_result(&worker, &root_id);
        result.set_outputs(
            &worker,
            std::collections::HashMap::from([("SHA".to_string(), "abc123".to_string())]),
        );
        assert!(complete_job(&mut store, &mut results, result));
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![child_id.clone()]);
        assert!(failed.is_empty());
        assert!(skipped.is_empty());
        let child = store.get(&child_id).unwrap();
        assert_eq!(
            child.job_config.steps[0].run,
            "echo abc123  ${{ needs.ghost.outputs.X }}"
        );
        assert_eq!(child.job_config.env.get("TAG").unwrap(), "v-abc123");
    }

    #[test]
    fn test_unblock_exposes_call_level_outputs() {
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let mut inner = sample_job(&id, "r", ComputeRequirements::default());
        inner.workflow_job = "hello/greet".to_string();
        inner.call_id = Some("hello".to_string());
        inner.call_outputs = HashMap::from([(
            "digest".to_string(),
            "${{ jobs.greet.outputs.digest }}".to_string(),
        )]);
        let inner_id = inner.id.clone();
        store.add(inner);
        let mut child = sample_job(&id, "r", ComputeRequirements::default());
        child.workflow_job = "consume".to_string();
        child.job_config.steps[0].run = "echo ${{ needs.hello.outputs.digest }}".to_string();
        child.status = JobStatus::Blocked {
            needs: vec![inner_id.clone()],
        };
        let child_id = child.id.clone();
        store.add(child);
        let mut result = sample_result(&worker, &inner_id);
        result.set_outputs(
            &worker,
            std::collections::HashMap::from([("digest".to_string(), "abc123".to_string())]),
        );
        assert!(complete_job(&mut store, &mut results, result));
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![child_id.clone()]);
        assert!(failed.is_empty());
        assert!(skipped.is_empty());
        let child = store.get(&child_id).unwrap();
        assert_eq!(child.job_config.steps[0].run, "echo abc123");
    }

    #[test]
    fn test_expr_conditions_gate_unblock() {
        use crate::job::RunCondition;
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let mut root = sample_job(&id, "r", ComputeRequirements::default());
        root.workflow_job = "build".to_string();
        let root_id = root.id.clone();
        store.add(root);
        let mut deploy = sample_job(&id, "r", ComputeRequirements::default());
        deploy.run_if = RunCondition::Expr("needs.build.outputs.SHA == 'abc123'".to_string());
        deploy.job_config.steps[0].condition =
            RunCondition::Expr("needs.build.result == 'success'".to_string());
        deploy.status = JobStatus::Blocked {
            needs: vec![root_id.clone()],
        };
        let deploy_id = deploy.id.clone();
        let mut skip = sample_job(&id, "r", ComputeRequirements::default());
        skip.run_if = RunCondition::Expr("needs.build.result == 'failure'".to_string());
        skip.status = JobStatus::Blocked {
            needs: vec![root_id.clone()],
        };
        let skip_id = skip.id.clone();
        store.add(deploy);
        store.add(skip);
        let mut result = sample_result(&worker, &root_id);
        result.set_outputs(
            &worker,
            std::collections::HashMap::from([("SHA".to_string(), "abc123".to_string())]),
        );
        assert!(complete_job(&mut store, &mut results, result));
        let (pending, failed, skipped) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![deploy_id.clone()]);
        assert!(failed.is_empty());
        assert_eq!(skipped, vec![skip_id.clone()]);
        assert!(matches!(
            store.get(&skip_id).unwrap().status,
            JobStatus::Cancelled
        ));
        let deploy = store.get(&deploy_id).unwrap();
        assert_eq!(
            deploy.job_config.steps[0].condition,
            RunCondition::Expr("'success' == 'success'".to_string())
        );
    }

    #[test]
    fn test_unblock_substitutes_needs_in_working_directory() {
        let id = Identity::generate();
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut results = JobResultStore::new();
        let mut root = sample_job(&id, "r", ComputeRequirements::default());
        root.workflow_job = "build".to_string();
        let root_id = root.id.clone();
        store.add(root);
        let mut child = sample_job(&id, "r", ComputeRequirements::default());
        child.workflow_job = "test".to_string();
        child.job_config.steps[0].working_directory =
            Some("out/${{ needs.build.outputs.SHA }}".to_string());
        child.status = JobStatus::Blocked {
            needs: vec![root_id.clone()],
        };
        let child_id = child.id.clone();
        store.add(child);
        let mut result = sample_result(&worker, &root_id);
        result.set_outputs(
            &worker,
            std::collections::HashMap::from([("SHA".to_string(), "abc123".to_string())]),
        );
        assert!(complete_job(&mut store, &mut results, result));
        let (pending, _, _) = unblock_ready_jobs(&mut store, &results);
        assert_eq!(pending, vec![child_id.clone()]);
        let child = store.get(&child_id).unwrap();
        assert_eq!(
            child.job_config.steps[0].working_directory,
            Some("out/abc123".to_string())
        );
    }

    #[test]
    fn concurrency_group_serializes_claims() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut first = sample_job(&worker, "repo-a", ComputeRequirements::default());
        first.concurrency_group = "deploy".to_string();
        first.created_at = chrono::DateTime::from_timestamp(100, 0).unwrap();
        let first_id = first.id.clone();
        let mut second = sample_job(&worker, "repo-a", ComputeRequirements::default());
        second.concurrency_group = "deploy".to_string();
        second.created_at = chrono::DateTime::from_timestamp(200, 0).unwrap();
        let second_id = second.id.clone();
        let mut other = sample_job(&worker, "repo-a", ComputeRequirements::default());
        other.concurrency_group = "other".to_string();
        other.created_at = chrono::DateTime::from_timestamp(300, 0).unwrap();
        let other_id = other.id.clone();
        store.add(first);
        store.add(second);
        store.add(other);
        let claimed = claim_next_job(&mut store, &caps(), worker.peer_id(), &|_| true).unwrap();
        assert_eq!(claimed.id, first_id);
        let waiting: Vec<String> = selectable(&store, &caps(), &|_| true)
            .iter()
            .map(|j| j.id.clone())
            .collect();
        assert!(!waiting.contains(&second_id));
        assert!(waiting.contains(&other_id));
        let mut results = JobResultStore::new();
        assert!(complete_job(
            &mut store,
            &mut results,
            sample_result(&worker, &first_id)
        ));
        let freed: Vec<String> = selectable(&store, &caps(), &|_| true)
            .iter()
            .map(|j| j.id.clone())
            .collect();
        assert!(freed.contains(&second_id));
    }

    #[test]
    fn cancel_group_jobs_only_touches_queued_and_unkept() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut pending = sample_job(&worker, "repo-a", ComputeRequirements::default());
        pending.concurrency_group = "g".to_string();
        let pending_id = pending.id.clone();
        let mut blocked = sample_job(&worker, "repo-a", ComputeRequirements::default());
        blocked.concurrency_group = "g".to_string();
        blocked.status = JobStatus::Blocked {
            needs: vec!["missing".to_string()],
        };
        let blocked_id = blocked.id.clone();
        let mut running = sample_job(&worker, "repo-a", ComputeRequirements::default());
        running.concurrency_group = "g".to_string();
        running.status = JobStatus::Running {
            worker: worker.peer_id(),
            started_at: chrono::Utc::now(),
        };
        let running_id = running.id.clone();
        let mut fresh = sample_job(&worker, "repo-a", ComputeRequirements::default());
        fresh.concurrency_group = "g".to_string();
        let fresh_id = fresh.id.clone();
        let mut elsewhere = sample_job(&worker, "repo-a", ComputeRequirements::default());
        elsewhere.concurrency_group = "elsewhere".to_string();
        let elsewhere_id = elsewhere.id.clone();
        for job in [pending, blocked, running, fresh, elsewhere] {
            store.add(job);
        }
        let keep = std::collections::HashSet::from([fresh_id.clone()]);
        let cancelled = cancel_group_jobs(&mut store, "g", &keep);
        let mut ids: Vec<String> = cancelled.iter().map(|(id, _)| id.clone()).collect();
        ids.sort();
        let mut want = vec![blocked_id.clone(), pending_id.clone()];
        want.sort();
        assert_eq!(ids, want);
        assert!(matches!(
            store.get(&pending_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert!(matches!(
            store.get(&blocked_id).unwrap().status,
            JobStatus::Cancelled
        ));
        assert!(matches!(
            store.get(&running_id).unwrap().status,
            JobStatus::Running { .. }
        ));
        assert!(matches!(
            store.get(&fresh_id).unwrap().status,
            JobStatus::Pending
        ));
        assert!(matches!(
            store.get(&elsewhere_id).unwrap().status,
            JobStatus::Pending
        ));
        assert!(cancel_group_jobs(&mut store, "", &keep).is_empty());
    }

    #[test]
    fn approval_pending_jobs_wait_until_approved() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut gated = sample_job(&worker, "repo-a", ComputeRequirements::default());
        gated.environment = "prod".to_string();
        gated.env_approval_pending = true;
        let gated_id = gated.id.clone();
        let mut open = sample_job(&worker, "repo-a", ComputeRequirements::default());
        open.environment = "staging".to_string();
        let open_id = open.id.clone();
        store.add(gated);
        store.add(open);
        let waiting: Vec<String> = selectable(&store, &caps(), &|_| true)
            .iter()
            .map(|j| j.id.clone())
            .collect();
        assert!(!waiting.contains(&gated_id));
        assert!(waiting.contains(&open_id));
        assert!(!approve_job(&mut store, "missing"));
        assert!(!approve_job(&mut store, &open_id));
        assert!(approve_job(&mut store, &gated_id));
        assert!(!approve_job(&mut store, &gated_id));
        let freed: Vec<String> = selectable(&store, &caps(), &|_| true)
            .iter()
            .map(|j| j.id.clone())
            .collect();
        assert!(freed.contains(&gated_id));
    }

    #[test]
    fn failed_need_with_continue_on_error_releases_success_child() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let results = JobResultStore::new();
        let mut need = sample_job(&worker, "repo-a", ComputeRequirements::default());
        need.continue_on_error = true;
        need.status = JobStatus::Failed {
            reason: "boom".to_string(),
        };
        let need_id = need.id.clone();
        let mut child = sample_job(&worker, "repo-a", ComputeRequirements::default());
        child.status = JobStatus::Blocked {
            needs: vec![need_id.clone()],
        };
        child.run_if = crate::job::RunCondition::Success;
        let child_id = child.id.clone();
        let mut strict_child = sample_job(&worker, "repo-a", ComputeRequirements::default());
        strict_child.status = JobStatus::Blocked {
            needs: vec![need_id.clone()],
        };
        strict_child.run_if = crate::job::RunCondition::Failure;
        let strict_id = strict_child.id.clone();
        store.add(need);
        store.add(child);
        store.add(strict_child);
        let (pending, failed, _skipped) = unblock_ready_jobs(&mut store, &results);
        assert!(pending.contains(&child_id));
        assert!(pending.contains(&strict_id));
        assert!(failed.is_empty());
    }

    #[test]
    fn failed_need_without_flag_still_blocks_success_child() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let results = JobResultStore::new();
        let mut need = sample_job(&worker, "repo-a", ComputeRequirements::default());
        need.status = JobStatus::Failed {
            reason: "boom".to_string(),
        };
        let need_id = need.id.clone();
        let mut child = sample_job(&worker, "repo-a", ComputeRequirements::default());
        child.status = JobStatus::Blocked {
            needs: vec![need_id.clone()],
        };
        child.run_if = crate::job::RunCondition::Success;
        let child_id = child.id.clone();
        store.add(need);
        store.add(child);
        let (pending, failed, _skipped) = unblock_ready_jobs(&mut store, &results);
        assert!(pending.is_empty());
        assert_eq!(failed, vec![child_id]);
    }

    #[test]
    fn max_parallel_holds_siblings_until_a_slot_frees() {
        let worker = Identity::generate();
        let mut store = ComputeJobStore::new();
        let mut first = sample_job(&worker, "repo-a", ComputeRequirements::default());
        first.max_parallel = 1;
        first.created_at = chrono::DateTime::from_timestamp(100, 0).unwrap();
        let first_id = first.id.clone();
        let mut second = sample_job(&worker, "repo-a", ComputeRequirements::default());
        second.max_parallel = 1;
        second.created_at = chrono::DateTime::from_timestamp(200, 0).unwrap();
        let second_id = second.id.clone();
        first.matrix_siblings = vec![second_id.clone()];
        second.matrix_siblings = vec![first_id.clone()];
        store.add(first);
        store.add(second);
        let claimed = claim_next_job(&mut store, &caps(), worker.peer_id(), &|_| true).unwrap();
        assert_eq!(claimed.id, first_id);
        assert!(selectable(&store, &caps(), &|_| true).is_empty());
        let mut results = JobResultStore::new();
        assert!(complete_job(
            &mut store,
            &mut results,
            sample_result(&worker, &first_id)
        ));
        let freed: Vec<String> = selectable(&store, &caps(), &|_| true)
            .iter()
            .map(|j| j.id.clone())
            .collect();
        assert_eq!(freed, vec![second_id]);
    }
}
