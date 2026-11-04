use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::job::{ComputeRequirements, JobConfig, JobStep};
use crate::permissions::{Permissions, PermissionsDecl};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BranchFilter {
    #[serde(default)]
    pub branches: Vec<String>,
    #[serde(default, rename = "branches-ignore")]
    pub branches_ignore: Vec<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default, rename = "tags-ignore")]
    pub tags_ignore: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDispatchDef {
    #[serde(default)]
    pub inputs: HashMap<String, WorkflowCallInput>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowTriggers {
    #[serde(default)]
    pub push: Option<BranchFilter>,
    #[serde(default)]
    pub pull_request: Option<BranchFilter>,
    #[serde(default)]
    pub workflow_dispatch: Option<WorkflowDispatchDef>,
    #[serde(default)]
    pub workflow_call: Option<WorkflowCallDef>,
    #[serde(default)]
    pub workflow_run: Option<WorkflowRunDef>,
    #[serde(default)]
    pub schedule: Option<Vec<ScheduleDef>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ScheduleDef {
    #[serde(default)]
    pub cron: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowRunDef {
    #[serde(default)]
    pub workflows: Vec<String>,
    #[serde(default)]
    pub types: Vec<String>,
    #[serde(default)]
    pub branches: BranchFilter,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowCallInput {
    #[serde(default)]
    pub required: bool,
    #[serde(default)]
    pub default: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowCallDef {
    #[serde(default)]
    pub inputs: HashMap<String, WorkflowCallInput>,
    #[serde(default)]
    pub secrets: HashMap<String, bool>,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
}

fn is_local_composite_ref(uses: &str) -> bool {
    if uses.starts_with("./") || uses.starts_with("../") {
        return true;
    }
    uses.starts_with('/') && (uses.ends_with(".yml") || uses.ends_with(".yaml"))
}

struct CompositeAction {
    inputs: HashMap<String, WorkflowCallInput>,
    steps: Vec<WorkflowStep>,
}

fn parse_composite_action(text: &str, source: &str) -> Result<CompositeAction> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(text).with_context(|| format!("parse composite action {source}"))?;
    let serde_yaml::Value::Mapping(map) = &doc else {
        bail!("composite action {source} must be a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let runs = get("runs")
        .and_then(|v| v.as_mapping())
        .ok_or_else(|| anyhow::anyhow!("composite action {source} needs a runs block"))?;
    let using = runs
        .get(serde_yaml::Value::String("using".to_string()))
        .and_then(yaml_scalar_string)
        .unwrap_or_default();
    if using != "composite" {
        bail!("composite action {source} has unsupported runs.using {using:?}");
    }
    let mut steps = Vec::new();
    if let Some(serde_yaml::Value::Sequence(items)) =
        runs.get(serde_yaml::Value::String("steps".to_string()))
    {
        for item in items {
            steps.push(
                parse_step(item)
                    .with_context(|| format!("composite action {source} has a bad step"))?,
            );
        }
    }
    if steps.is_empty() {
        bail!("composite action {source} has no steps");
    }
    Ok(CompositeAction {
        inputs: parse_input_specs(&doc),
        steps,
    })
}

fn inline_composite_steps(
    steps: &[WorkflowStep],
    base_dir: Option<&std::path::Path>,
    depth: usize,
    stack: &mut Vec<std::path::PathBuf>,
) -> Result<Vec<WorkflowStep>> {
    if depth > 8 {
        bail!("composite actions nest too deep (over 8)");
    }
    let mut out = Vec::new();
    for step in steps {
        let Some(target) = step.uses.as_deref().filter(|u| is_local_composite_ref(u)) else {
            out.push(step.clone());
            continue;
        };
        if step.run.is_some() {
            bail!("step uses {target} and also sets run; pick one");
        }
        let Some(dir) = base_dir else {
            bail!("step uses {target} but the calling workflow has no file location");
        };
        let target_path = dir.join(target);
        if stack.contains(&target_path) {
            bail!("step uses {target} recursively");
        }
        let text =
            std::fs::read_to_string(&target_path).with_context(|| format!("step uses {target}"))?;
        let action = parse_composite_action(&text, target)?;
        let mut inputs: HashMap<String, String> = HashMap::new();
        for (name, spec) in &action.inputs {
            if let Some(raw) = step.with.get(name) {
                inputs.insert(name.clone(), raw.clone());
            } else if let Some(default) = &spec.default {
                inputs.insert(name.clone(), default.clone());
            } else if spec.required {
                bail!("step uses {target} without required input {name}");
            } else {
                inputs.insert(name.clone(), String::new());
            }
        }
        for name in step.with.keys() {
            if !action.inputs.contains_key(name) {
                bail!("step uses {target} with undeclared input {name}");
            }
        }
        stack.push(target_path.clone());
        let sub_dir = target_path.parent().map(|p| p.to_path_buf());
        let inner = inline_composite_steps(&action.steps, sub_dir.as_deref(), depth + 1, stack);
        stack.pop();
        for mut sub in inner? {
            if let Some(run) = sub.run.take() {
                sub.run = Some(substitute_inputs(&run, &inputs));
            }
            if !sub.name.is_empty() {
                sub.name = substitute_inputs(&sub.name, &inputs);
            }
            for value in sub.with.values_mut() {
                *value = substitute_inputs(value, &inputs);
            }
            for value in sub.env.values_mut() {
                *value = substitute_inputs(value, &inputs);
            }
            if let Some(condition) = sub.condition.take() {
                sub.condition = Some(substitute_inputs(&condition, &inputs));
            }
            if sub.condition.is_none() {
                sub.condition = step.condition.clone();
            }
            for (key, value) in &step.env {
                sub.env.entry(key.clone()).or_insert_with(|| value.clone());
            }
            out.push(sub);
        }
    }
    Ok(out)
}

pub(crate) fn parse_call_output_ref(expr: &str) -> Option<(String, String)> {
    let inner = expr.trim().strip_prefix("${{")?.strip_suffix("}}")?.trim();
    let mut parts = inner.split('.');
    let (jobs, job, outputs, key) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    if jobs.trim() != "jobs" || outputs.trim() != "outputs" {
        return None;
    }
    let (job, key) = (job.trim(), key.trim());
    if job.is_empty() || key.is_empty() {
        return None;
    }
    Some((job.to_string(), key.to_string()))
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStep {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: HashMap<String, String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default, rename = "if")]
    pub condition: Option<String>,
    #[serde(default, rename = "continue-on-error")]
    pub continue_on_error: bool,
    #[serde(default, rename = "working-directory")]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowContainerCredentials {
    pub username: String,
    pub password: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowContainer {
    pub image: String,
    #[serde(default)]
    pub credentials: Option<WorkflowContainerCredentials>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<crate::job::PortBinding>,
    #[serde(default)]
    pub volumes: Vec<crate::job::JobVolume>,
    #[serde(default)]
    pub resources: crate::job::ContainerResources,
    #[serde(default)]
    pub options: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowService {
    pub image: String,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub ports: Vec<crate::job::PortBinding>,
    #[serde(default)]
    pub options: String,
    #[serde(default)]
    pub health: Option<crate::job::ServiceHealth>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowStrategy {
    #[serde(default)]
    pub matrix: HashMap<String, Vec<String>>,
    #[serde(default)]
    pub include: Vec<HashMap<String, String>>,
    #[serde(default)]
    pub exclude: Vec<HashMap<String, String>>,
    #[serde(default = "default_fail_fast", rename = "fail-fast")]
    pub fail_fast: bool,
    #[serde(default, rename = "max-parallel")]
    pub max_parallel: Option<u32>,
}

fn default_fail_fast() -> bool {
    true
}

impl Default for WorkflowStrategy {
    fn default() -> Self {
        WorkflowStrategy {
            matrix: HashMap::new(),
            include: Vec::new(),
            exclude: Vec::new(),
            fail_fast: true,
            max_parallel: None,
        }
    }
}

pub const DEFAULT_JOB_CPU_CORES: u32 = 2;
pub const DEFAULT_JOB_RAM_MB: u64 = 4096;
pub const DEFAULT_JOB_DISK_MB: u64 = 10240;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkflowJob {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "runs-on")]
    pub runs_on: RunsOn,
    #[serde(default)]
    pub needs: Needs,
    #[serde(default)]
    pub strategy: WorkflowStrategy,
    #[serde(default)]
    pub steps: Vec<WorkflowStep>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub secrets: HashMap<String, String>,
    #[serde(default)]
    pub secrets_inherit: bool,
    #[serde(default, rename = "timeout-minutes")]
    pub timeout_minutes: Option<u64>,
    #[serde(default, rename = "docker-image")]
    pub docker_image: Option<String>,
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub network: bool,
    #[serde(default)]
    pub cpu: Option<u32>,
    #[serde(default, rename = "ram-mb")]
    pub ram_mb: Option<u64>,
    #[serde(default, rename = "disk-mb")]
    pub disk_mb: Option<u64>,
    #[serde(default, rename = "if")]
    pub if_condition: Option<String>,
    #[serde(default)]
    pub uses: Option<String>,
    #[serde(default)]
    pub with: HashMap<String, String>,
    #[serde(default)]
    pub artifacts: Vec<String>,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub environment: Option<Environment>,
    #[serde(default, rename = "continue-on-error")]
    pub continue_on_error: bool,
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    #[serde(default)]
    pub outputs: HashMap<String, String>,
    #[serde(default)]
    pub services: HashMap<String, WorkflowService>,
    #[serde(default)]
    pub container: Option<WorkflowContainer>,
    #[serde(default)]
    pub call_id: Option<String>,
    #[serde(default)]
    pub call_outputs: HashMap<String, String>,
    #[serde(default)]
    pub permissions: PermissionsDecl,
    #[serde(skip, default)]
    pub effective_permissions: Option<Permissions>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowDefaults {
    #[serde(default)]
    pub run: WorkflowRunDefaults,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct WorkflowRunDefaults {
    #[serde(default, rename = "working-directory")]
    pub working_directory: Option<String>,
    #[serde(default)]
    pub shell: Option<String>,
}

fn parse_shell(value: Option<&serde_yaml::Value>, owner: &str) -> Result<Option<String>> {
    let Some(raw) = value.and_then(yaml_scalar_string) else {
        return Ok(None);
    };
    let shell = raw.trim();
    if shell == "sh" || shell == "bash" {
        return Ok(Some(shell.to_string()));
    }
    bail!("{owner} has unsupported shell {raw:?}: want sh or bash")
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RunsOn {
    Single(String),
    Labels(Vec<String>),
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Needs {
    Single(String),
    List(Vec<String>),
    #[default]
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Concurrency {
    Simple(String),
    Full {
        group: String,
        #[serde(default, rename = "cancel-in-progress")]
        cancel_in_progress: bool,
    },
}

impl Concurrency {
    pub fn group(&self) -> &str {
        match self {
            Concurrency::Simple(group) => group,
            Concurrency::Full { group, .. } => group,
        }
    }

    pub fn cancel_in_progress(&self) -> bool {
        match self {
            Concurrency::Simple(_) => false,
            Concurrency::Full {
                cancel_in_progress, ..
            } => *cancel_in_progress,
        }
    }

    fn map_group(&mut self, f: impl Fn(&str) -> String) {
        match self {
            Concurrency::Simple(group) => *group = f(group),
            Concurrency::Full { group, .. } => *group = f(group),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Environment {
    Simple(String),
    Full {
        name: String,
        #[serde(default)]
        url: String,
    },
}

impl Environment {
    pub fn name(&self) -> &str {
        match self {
            Environment::Simple(name) => name,
            Environment::Full { name, .. } => name,
        }
    }

    pub fn url(&self) -> &str {
        match self {
            Environment::Simple(_) => "",
            Environment::Full { url, .. } => url,
        }
    }

    fn map_parts(&mut self, f: impl Fn(&str) -> String) {
        match self {
            Environment::Simple(name) => *name = f(name),
            Environment::Full { name, url } => {
                *name = f(name);
                *url = f(url);
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    #[serde(default)]
    pub name: String,
    #[serde(default, rename = "on")]
    pub on: WorkflowTriggers,
    #[serde(default)]
    pub concurrency: Option<Concurrency>,
    #[serde(default)]
    pub jobs: HashMap<String, WorkflowJob>,
    #[serde(skip, default)]
    pub source_dir: Option<std::path::PathBuf>,
    #[serde(default)]
    pub defaults: WorkflowDefaults,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub permissions: PermissionsDecl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEvent {
    Push {
        branch: String,
    },
    PullRequest {
        branch: String,
    },
    WorkflowDispatch {
        inputs: HashMap<String, String>,
    },
    Tag {
        tag: String,
    },
    WorkflowRun {
        workflow: String,
        file: String,
        conclusion: String,
        branch: String,
    },
    Schedule {
        cron: String,
        branch: String,
    },
}

fn cron_field_due(field: &str, value: u32, max: u32) -> bool {
    for part in field.split(',') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.trim().parse::<u32>().ok().filter(|n| *n > 0)),
            None => (part, None),
        };
        if part.contains('/') && step.is_none() {
            continue;
        }
        let step = step.unwrap_or(1);
        let (lo, hi) = if range == "*" || range.is_empty() {
            (0, max)
        } else if let Some((a, b)) = range.split_once('-') {
            match (a.trim().parse::<u32>(), b.trim().parse::<u32>()) {
                (Ok(a), Ok(b)) if a <= b && b <= max => (a, b),
                _ => continue,
            }
        } else {
            match range.trim().parse::<u32>() {
                Ok(n) if n <= max => (n, n),
                _ => continue,
            }
        };
        if value >= lo && value <= hi && (value - lo).is_multiple_of(step) {
            return true;
        }
    }
    false
}

pub fn cron_due(cron: &str, now: &chrono::DateTime<chrono::Utc>) -> bool {
    use chrono::Datelike;
    use chrono::Timelike;
    let fields: Vec<&str> = cron.split_whitespace().collect();
    if fields.len() != 5 {
        return false;
    }
    let monday = now.weekday().number_from_monday();
    let day_of_week = monday % 7;
    let checks = [
        (fields[0], now.minute(), 59),
        (fields[1], now.hour(), 23),
        (fields[2], now.day(), 31),
        (fields[3], now.month(), 12),
    ];
    if !checks
        .iter()
        .all(|(field, value, max)| cron_field_due(field, *value, *max))
    {
        return false;
    }
    if cron_field_due(fields[4], day_of_week, 6) {
        return true;
    }
    day_of_week == 0 && cron_field_due(fields[4], 7, 7)
}

#[derive(Debug, Clone)]
pub struct ExpandedJob {
    pub workflow_name: String,
    pub job_id: String,
    pub display_name: String,
    pub matrix: HashMap<String, String>,
    pub job_config: JobConfig,
    pub requirements: ComputeRequirements,
    pub needs: Vec<String>,
    pub call_id: Option<String>,
    pub call_outputs: HashMap<String, String>,
    pub run_if: crate::job::RunCondition,
    pub matrix_parent: Option<String>,
    pub fail_fast: bool,
    pub concurrency_group: String,
    pub cancel_in_progress: bool,
    pub environment: String,
    pub environment_url: String,
    pub continue_on_error: bool,
    pub max_parallel: u32,
    pub permissions: Permissions,
}

impl Default for RunsOn {
    fn default() -> Self {
        RunsOn::Single("ubuntu-latest".to_string())
    }
}

impl RunsOn {
    pub fn labels(&self) -> Vec<String> {
        match self {
            RunsOn::Single(s) => vec![s.clone()],
            RunsOn::Labels(v) => v.clone(),
        }
    }
}

impl Needs {
    pub fn list(&self) -> Vec<String> {
        match self {
            Needs::Single(s) => vec![s.clone()],
            Needs::List(v) => v.clone(),
            Needs::None => vec![],
        }
    }
}

fn yaml_string_list(value: &serde_yaml::Value) -> Vec<String> {
    match value {
        serde_yaml::Value::Sequence(items) => items.iter().filter_map(yaml_scalar_string).collect(),
        single => yaml_scalar_string(single).into_iter().collect(),
    }
}

fn yaml_scalar_string(value: &serde_yaml::Value) -> Option<String> {
    match value {
        serde_yaml::Value::String(s) => Some(s.clone()),
        serde_yaml::Value::Number(n) => Some(n.to_string()),
        serde_yaml::Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn parse_branch_filter(value: &serde_yaml::Value) -> BranchFilter {
    let mut filter = BranchFilter::default();
    if let serde_yaml::Value::Mapping(map) = value {
        let branches_key = serde_yaml::Value::String("branches".to_string());
        if let Some(v) = map.get(&branches_key) {
            filter.branches = yaml_string_list(v);
        }
        let ignore_key = serde_yaml::Value::String("branches-ignore".to_string());
        if let Some(v) = map.get(&ignore_key) {
            filter.branches_ignore = yaml_string_list(v);
        }
        let tags_key = serde_yaml::Value::String("tags".to_string());
        if let Some(v) = map.get(&tags_key) {
            filter.tags = yaml_string_list(v);
        }
        let tags_ignore_key = serde_yaml::Value::String("tags-ignore".to_string());
        if let Some(v) = map.get(&tags_ignore_key) {
            filter.tags_ignore = yaml_string_list(v);
        }
    }
    filter
}

fn enable_trigger(triggers: &mut WorkflowTriggers, name: &str, value: &serde_yaml::Value) {
    match name {
        "push" => triggers.push = Some(parse_branch_filter(value)),
        "pull_request" => triggers.pull_request = Some(parse_branch_filter(value)),
        "workflow_dispatch" => triggers.workflow_dispatch = Some(parse_workflow_dispatch(value)),
        "workflow_call" => triggers.workflow_call = Some(parse_workflow_call(value)),
        "workflow_run" => triggers.workflow_run = Some(parse_workflow_run(value)),
        "schedule" => triggers.schedule = Some(parse_schedule(value)),
        _ => {}
    }
}

fn parse_input_specs(value: &serde_yaml::Value) -> HashMap<String, WorkflowCallInput> {
    let mut inputs = HashMap::new();
    let serde_yaml::Value::Mapping(map) = value else {
        return inputs;
    };
    let Some(serde_yaml::Value::Mapping(specs)) =
        map.get(serde_yaml::Value::String("inputs".to_string()))
    else {
        return inputs;
    };
    for (k, v) in specs {
        let Some(name) = yaml_scalar_string(k) else {
            continue;
        };
        let mut input = WorkflowCallInput::default();
        if let serde_yaml::Value::Mapping(spec) = v {
            let req_key = serde_yaml::Value::String("required".to_string());
            let def_key = serde_yaml::Value::String("default".to_string());
            input.required = matches!(spec.get(&req_key), Some(serde_yaml::Value::Bool(true)));
            input.default = spec.get(&def_key).and_then(yaml_scalar_string);
        }
        inputs.insert(name, input);
    }
    inputs
}

fn parse_workflow_run(value: &serde_yaml::Value) -> WorkflowRunDef {
    let mut def = WorkflowRunDef::default();
    let serde_yaml::Value::Mapping(map) = value else {
        return def;
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    if let Some(v) = get("workflows") {
        def.workflows = yaml_string_list(v);
    }
    if let Some(v) = get("types") {
        def.types = yaml_string_list(v);
    }
    def.branches = parse_branch_filter(value);
    def
}

fn parse_schedule(value: &serde_yaml::Value) -> Vec<ScheduleDef> {
    let mut out = Vec::new();
    let items: Vec<&serde_yaml::Value> = match value {
        serde_yaml::Value::Sequence(items) => items.iter().collect(),
        mapping @ serde_yaml::Value::Mapping(_) => vec![mapping],
        _ => Vec::new(),
    };
    for item in items {
        if let serde_yaml::Value::Mapping(map) = item {
            if let Some(cron) = map
                .get(serde_yaml::Value::String("cron".to_string()))
                .and_then(yaml_scalar_string)
            {
                out.push(ScheduleDef {
                    cron: cron.trim().to_string(),
                });
            }
        }
    }
    out
}

fn parse_workflow_dispatch(value: &serde_yaml::Value) -> WorkflowDispatchDef {
    WorkflowDispatchDef {
        inputs: parse_input_specs(value),
    }
}

fn parse_workflow_call(value: &serde_yaml::Value) -> WorkflowCallDef {
    let mut def = WorkflowCallDef {
        inputs: parse_input_specs(value),
        ..WorkflowCallDef::default()
    };
    let serde_yaml::Value::Mapping(map) = value else {
        return def;
    };
    if let Some(serde_yaml::Value::Mapping(secrets)) =
        map.get(serde_yaml::Value::String("secrets".to_string()))
    {
        for (k, v) in secrets {
            let Some(name) = yaml_scalar_string(k) else {
                continue;
            };
            let required = match v {
                serde_yaml::Value::Mapping(spec) => {
                    let req_key = serde_yaml::Value::String("required".to_string());
                    matches!(spec.get(&req_key), Some(serde_yaml::Value::Bool(true)))
                }
                _ => false,
            };
            def.secrets.insert(name, required);
        }
    }
    if let Some(outputs) = map.get(serde_yaml::Value::String("outputs".to_string())) {
        for (k, v) in parse_string_map(Some(outputs)) {
            def.outputs.insert(k, v);
        }
    }
    def
}

fn parse_triggers(value: &serde_yaml::Value) -> WorkflowTriggers {
    let mut triggers = WorkflowTriggers::default();
    match value {
        serde_yaml::Value::String(s) => {
            enable_trigger(&mut triggers, s, &serde_yaml::Value::Null);
        }
        serde_yaml::Value::Sequence(items) => {
            for item in items {
                if let Some(name) = yaml_scalar_string(item) {
                    enable_trigger(&mut triggers, &name, &serde_yaml::Value::Null);
                }
            }
        }
        serde_yaml::Value::Mapping(map) => {
            for (k, v) in map {
                if let Some(name) = yaml_scalar_string(k) {
                    enable_trigger(&mut triggers, &name, v);
                }
            }
        }
        _ => {}
    }
    triggers
}

fn parse_runs_on(value: Option<&serde_yaml::Value>) -> RunsOn {
    match value {
        None => RunsOn::default(),
        Some(serde_yaml::Value::Sequence(items)) => {
            let labels: Vec<String> = items.iter().filter_map(yaml_scalar_string).collect();
            if labels.is_empty() {
                RunsOn::default()
            } else {
                RunsOn::Labels(labels)
            }
        }
        Some(single) => match yaml_scalar_string(single) {
            Some(s) => RunsOn::Single(s),
            None => RunsOn::default(),
        },
    }
}

fn parse_needs(value: Option<&serde_yaml::Value>) -> Needs {
    match value {
        None => Needs::None,
        Some(serde_yaml::Value::Sequence(items)) => {
            Needs::List(items.iter().filter_map(yaml_scalar_string).collect())
        }
        Some(single) => match yaml_scalar_string(single) {
            Some(s) => Needs::Single(s),
            None => Needs::None,
        },
    }
}

fn parse_job_secrets(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
) -> Result<HashMap<String, String>> {
    match value {
        None | Some(serde_yaml::Value::Mapping(_)) => Ok(parse_string_map(value)),
        Some(serde_yaml::Value::String(s)) if s == "inherit" => Ok(HashMap::new()),
        Some(_) => bail!("job {job_id} secrets must be a mapping or inherit"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UsesTarget {
    Local(String),
    Remote {
        repo: String,
        path: String,
        git_ref: String,
    },
}

fn valid_repo_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

fn valid_git_ref(git_ref: &str) -> bool {
    if git_ref.is_empty() || git_ref.len() > 256 {
        return false;
    }
    if git_ref.starts_with('-') || git_ref.starts_with('/') || git_ref.contains("..") {
        return false;
    }
    if git_ref.contains("@{") {
        return false;
    }
    git_ref
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '/' | '+'))
}

pub fn parse_uses_target(target: &str) -> Result<UsesTarget> {
    if !target.contains('@') {
        if target.trim().is_empty() {
            bail!("uses target must not be empty");
        }
        return Ok(UsesTarget::Local(target.to_string()));
    }
    let bad = || {
        anyhow::anyhow!(
            "uses must be a repo-relative path like ./reusable.yml or repo/path/to/file.yml@ref, got {target:?}"
        )
    };
    let (left, git_ref) = target.split_once('@').ok_or_else(bad)?;
    if !valid_git_ref(git_ref) {
        return Err(bad());
    }
    let (repo, path) = left.split_once('/').ok_or_else(bad)?;
    if !valid_repo_name(repo) {
        return Err(bad());
    }
    if path.starts_with('/')
        || path.contains("..")
        || !(path.ends_with(".yml") || path.ends_with(".yaml"))
    {
        return Err(bad());
    }
    Ok(UsesTarget::Remote {
        repo: repo.to_string(),
        path: path.to_string(),
        git_ref: git_ref.to_string(),
    })
}

fn parse_string_map(value: Option<&serde_yaml::Value>) -> HashMap<String, String> {
    let mut out = HashMap::new();
    if let Some(serde_yaml::Value::Mapping(map)) = value {
        for (k, v) in map {
            if let (Some(key), Some(val)) = (yaml_scalar_string(k), yaml_scalar_string(v)) {
                out.insert(key, val);
            }
        }
    }
    out
}

fn parse_matrix(value: Option<&serde_yaml::Value>) -> HashMap<String, Vec<String>> {
    let mut out = HashMap::new();
    if let Some(serde_yaml::Value::Mapping(map)) = value {
        for (k, v) in map {
            let Some(key) = yaml_scalar_string(k) else {
                continue;
            };
            if key == "include" || key == "exclude" {
                continue;
            }
            let values = yaml_string_list(v);
            if !values.is_empty() {
                out.insert(key, values);
            }
        }
    }
    out
}

fn valid_service_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 64 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
}

fn parse_port_binding(raw: &str, job_id: &str, svc: &str) -> Result<crate::job::PortBinding> {
    let parts: Vec<&str> = raw.split(':').collect();
    let parse_port = |s: &str| {
        s.parse::<u16>()
            .map_err(|_| anyhow::anyhow!("job {job_id} service {svc} has bad port {raw:?}"))
    };
    match parts.as_slice() {
        [container] => Ok(crate::job::PortBinding {
            container: parse_port(container)?,
            host: None,
            ip: None,
        }),
        [host, container] => Ok(crate::job::PortBinding {
            container: parse_port(container)?,
            host: Some(parse_port(host)?),
            ip: None,
        }),
        [ip, host, container] => {
            if ip.is_empty() {
                bail!("job {job_id} service {svc} has bad port {raw:?}");
            }
            Ok(crate::job::PortBinding {
                container: parse_port(container)?,
                host: Some(parse_port(host)?),
                ip: Some(ip.to_string()),
            })
        }
        _ => bail!("job {job_id} service {svc} has bad port {raw:?}"),
    }
}

fn split_options(options: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;
    for c in options.chars() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
            } else {
                current.push(c);
            }
            continue;
        }
        match c {
            '\'' | '"' => quote = Some(c),
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

fn parse_go_duration(raw: &str) -> Option<u64> {
    if raw.is_empty() {
        return None;
    }
    if let Ok(secs) = raw.parse::<u64>() {
        return Some(secs);
    }
    let mut total_ms: u64 = 0;
    let mut num = String::new();
    let chars: Vec<char> = raw.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_ascii_digit() {
            num.push(c);
            i += 1;
            continue;
        }
        let n: u64 = num.parse().ok()?;
        num.clear();
        let mult = match c {
            'h' => 3_600_000,
            'm' if chars.get(i + 1) == Some(&'s') => {
                i += 1;
                1
            }
            'm' => 60_000,
            's' => 1_000,
            _ => return None,
        };
        total_ms = total_ms.checked_add(n.checked_mul(mult)?)?;
        i += 1;
    }
    if !num.is_empty() || total_ms == 0 {
        return None;
    }
    Some(total_ms.div_ceil(1000))
}

fn parse_service_health(
    options: &str,
    job_id: &str,
    svc: &str,
) -> Result<Option<crate::job::ServiceHealth>> {
    let tokens = split_options(options);
    let mut cmd: Option<String> = None;
    let mut interval = 30u64;
    let mut timeout = 30u64;
    let mut retries = 3u32;
    let mut i = 0;
    let take_value = |tokens: &[String], i: &mut usize, flag: &str| -> Result<String> {
        if let Some((_, v)) = tokens[*i].split_once('=') {
            return Ok(v.to_string());
        }
        *i += 1;
        tokens
            .get(*i)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("job {job_id} service {svc} flag {flag} needs a value"))
    };
    while i < tokens.len() {
        let flag = tokens[i].split('=').next().unwrap_or("");
        match flag {
            "--health-cmd" => cmd = Some(take_value(&tokens, &mut i, flag)?),
            "--health-interval" => {
                let v = take_value(&tokens, &mut i, flag)?;
                interval = parse_go_duration(&v).ok_or_else(|| {
                    anyhow::anyhow!("job {job_id} service {svc} has bad duration {v:?}")
                })?;
            }
            "--health-timeout" => {
                let v = take_value(&tokens, &mut i, flag)?;
                timeout = parse_go_duration(&v).ok_or_else(|| {
                    anyhow::anyhow!("job {job_id} service {svc} has bad duration {v:?}")
                })?;
            }
            "--health-retries" => {
                let v = take_value(&tokens, &mut i, flag)?;
                retries = v.parse().map_err(|_| {
                    anyhow::anyhow!("job {job_id} service {svc} has bad retries {v:?}")
                })?;
            }
            unknown if unknown.starts_with("--") => {
                tracing::warn!("job {job_id} service {svc} ignores unsupported option {unknown}");
                if tokens.get(i + 1).is_some_and(|t| !t.starts_with("--")) {
                    i += 1;
                }
            }
            other => bail!("job {job_id} service {svc} has bad options token {other:?}"),
        }
        i += 1;
    }
    Ok(cmd.map(|cmd| crate::job::ServiceHealth {
        cmd,
        interval_secs: interval,
        timeout_secs: timeout,
        retries,
    }))
}

fn parse_services(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
) -> Result<HashMap<String, WorkflowService>> {
    let mut services = HashMap::new();
    let Some(value) = value else {
        return Ok(services);
    };
    let serde_yaml::Value::Mapping(map) = value else {
        bail!("job {job_id} services must be a mapping");
    };
    for (k, v) in map {
        let Some(name) = yaml_scalar_string(k) else {
            bail!("job {job_id} has a non-string service name");
        };
        if !valid_service_name(&name) {
            bail!("job {job_id} has bad service name {name:?}");
        }
        let serde_yaml::Value::Mapping(spec) = v else {
            bail!("job {job_id} service {name} must be a mapping");
        };
        let get = |key: &str| spec.get(serde_yaml::Value::String(key.to_string()));
        let image = get("image")
            .and_then(yaml_scalar_string)
            .unwrap_or_default();
        if image.is_empty() {
            bail!("job {job_id} service {name} needs an image");
        }
        let mut ports = Vec::new();
        if let Some(serde_yaml::Value::Sequence(items)) = get("ports") {
            for item in items {
                let Some(raw) = yaml_scalar_string(item) else {
                    bail!("job {job_id} service {name} has a bad port entry");
                };
                ports.push(parse_port_binding(&raw, job_id, &name)?);
            }
        } else if get("ports").is_some() {
            bail!("job {job_id} service {name} ports must be a list");
        }
        let options = get("options")
            .and_then(yaml_scalar_string)
            .unwrap_or_default();
        let health = parse_service_health(&options, job_id, &name)?;
        services.insert(
            name,
            WorkflowService {
                image,
                env: parse_string_map(get("env")),
                ports,
                options,
                health,
            },
        );
    }
    Ok(services)
}

fn valid_volume_name(name: &str) -> bool {
    if name.is_empty() || name.len() > 128 {
        return false;
    }
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '-')
}

fn parse_container_volume(raw: &str, job_id: &str) -> Result<crate::job::JobVolume> {
    let bad = || anyhow::anyhow!("job {job_id} has bad container volume {raw:?}");
    let parts: Vec<&str> = raw.split(':').collect();
    let (source, target, mode) = match parts.as_slice() {
        [target] => (None, *target, "rw"),
        [source, target] => (Some(*source), *target, "rw"),
        [source, target, mode] => (Some(*source), *target, *mode),
        _ => return Err(bad()),
    };
    if !target.starts_with('/') || target.contains("..") {
        return Err(bad());
    }
    if let Some(source) = source {
        if !valid_volume_name(source) {
            return Err(bad());
        }
    }
    let read_only = match mode {
        "rw" => false,
        "ro" => true,
        _ => return Err(bad()),
    };
    Ok(crate::job::JobVolume {
        source: source.map(|s| s.to_string()),
        target: target.to_string(),
        read_only,
    })
}

fn parse_mem_bytes(raw: &str) -> Option<i64> {
    let lower = raw.trim().to_lowercase();
    let (digits, mult) = if let Some(n) = lower.strip_suffix('g') {
        (n, 1024 * 1024 * 1024i64)
    } else if let Some(n) = lower.strip_suffix('m') {
        (n, 1024 * 1024i64)
    } else if let Some(n) = lower.strip_suffix('k') {
        (n, 1024i64)
    } else if let Some(n) = lower.strip_suffix('b') {
        (n, 1i64)
    } else {
        (lower.as_str(), 1i64)
    };
    let n: i64 = digits.trim().parse().ok()?;
    if n <= 0 {
        return None;
    }
    n.checked_mul(mult)
}

fn parse_container_options(options: &str, job_id: &str) -> Result<crate::job::ContainerResources> {
    let tokens = split_options(options);
    let mut resources = crate::job::ContainerResources::default();
    let mut i = 0;
    let take_value = |tokens: &[String], i: &mut usize, flag: &str| -> Result<String> {
        if let Some((_, v)) = tokens[*i].split_once('=') {
            return Ok(v.to_string());
        }
        *i += 1;
        tokens
            .get(*i)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("job {job_id} container flag {flag} needs a value"))
    };
    while i < tokens.len() {
        let flag = tokens[i].split('=').next().unwrap_or("");
        match flag {
            "--cpus" => {
                let v = take_value(&tokens, &mut i, flag)?;
                let cpus: f64 = v
                    .parse()
                    .map_err(|_| anyhow::anyhow!("job {job_id} has bad --cpus {v:?}"))?;
                if !cpus.is_finite() || cpus <= 0.0 || cpus > 1024.0 {
                    anyhow::bail!("job {job_id} has bad --cpus {v:?}");
                }
                resources.nano_cpus = Some((cpus * 1_000_000_000.0) as i64);
            }
            "--memory" | "-m" => {
                let v = take_value(&tokens, &mut i, flag)?;
                resources.memory_bytes = Some(
                    parse_mem_bytes(&v)
                        .ok_or_else(|| anyhow::anyhow!("job {job_id} has bad --memory {v:?}"))?,
                );
            }
            "--shm-size" => {
                let v = take_value(&tokens, &mut i, flag)?;
                resources.shm_bytes = Some(
                    parse_mem_bytes(&v)
                        .ok_or_else(|| anyhow::anyhow!("job {job_id} has bad --shm-size {v:?}"))?,
                );
            }
            "--privileged" | "--pid" | "--network" | "--volume" | "-v" | "--mount"
            | "--publish" | "-p" | "--env" | "-e" | "--user" | "-u" | "--cap-add"
            | "--cap-drop" | "--security-opt" | "--device" | "--gpus" => {
                anyhow::bail!("job {job_id} container forbids {flag}: the runner manages it");
            }
            unknown if unknown.starts_with('-') => {
                tracing::warn!("job {job_id} container ignores unsupported option {unknown}");
                if tokens.get(i + 1).is_some_and(|t| !t.starts_with('-')) {
                    i += 1;
                }
            }
            other => anyhow::bail!("job {job_id} container has bad options token {other:?}"),
        }
        i += 1;
    }
    Ok(resources)
}

fn parse_container(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
) -> Result<Option<WorkflowContainer>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if let Some(image) = yaml_scalar_string(value) {
        if image.is_empty() {
            bail!("job {job_id} container needs an image");
        }
        return Ok(Some(WorkflowContainer {
            image,
            credentials: None,
            env: HashMap::new(),
            ports: Vec::new(),
            volumes: Vec::new(),
            resources: crate::job::ContainerResources::default(),
            options: String::new(),
        }));
    }
    let serde_yaml::Value::Mapping(spec) = value else {
        bail!("job {job_id} container must be an image string or a mapping");
    };
    let get = |key: &str| spec.get(serde_yaml::Value::String(key.to_string()));
    let image = get("image")
        .and_then(yaml_scalar_string)
        .unwrap_or_default();
    if image.is_empty() {
        bail!("job {job_id} container needs an image");
    }
    let credentials = match get("credentials") {
        None => None,
        Some(serde_yaml::Value::Mapping(creds)) => {
            let cget = |key: &str| creds.get(serde_yaml::Value::String(key.to_string()));
            let username = cget("username")
                .and_then(yaml_scalar_string)
                .unwrap_or_default();
            let password = cget("password")
                .and_then(yaml_scalar_string)
                .unwrap_or_default();
            if username.is_empty() || password.is_empty() {
                bail!("job {job_id} container credentials need a username and password");
            }
            Some(WorkflowContainerCredentials { username, password })
        }
        Some(_) => bail!("job {job_id} container credentials must be a mapping"),
    };
    let mut ports = Vec::new();
    if let Some(serde_yaml::Value::Sequence(items)) = get("ports") {
        for item in items {
            let Some(raw) = yaml_scalar_string(item) else {
                bail!("job {job_id} container has a bad port entry");
            };
            ports.push(parse_port_binding(&raw, job_id, "container")?);
        }
    } else if get("ports").is_some() {
        bail!("job {job_id} container ports must be a list");
    }
    let mut volumes = Vec::new();
    if let Some(serde_yaml::Value::Sequence(items)) = get("volumes") {
        for item in items {
            let Some(raw) = yaml_scalar_string(item) else {
                bail!("job {job_id} container has a bad volume entry");
            };
            volumes.push(parse_container_volume(&raw, job_id)?);
        }
    } else if get("volumes").is_some() {
        bail!("job {job_id} container volumes must be a list");
    }
    let options = get("options")
        .and_then(yaml_scalar_string)
        .unwrap_or_default();
    let resources = parse_container_options(&options, job_id)?;
    Ok(Some(WorkflowContainer {
        image,
        credentials,
        env: parse_string_map(get("env")),
        ports,
        volumes,
        resources,
        options,
    }))
}

fn check_step_refs(job: &WorkflowJob, job_id: &str) -> Result<()> {
    let mut ids = std::collections::HashSet::new();
    for step in &job.steps {
        if let Some(step_id) = &step.id {
            if !ids.insert(step_id.clone()) {
                bail!("job {job_id} has duplicate step id {step_id:?}");
            }
        }
    }
    let mut texts: Vec<&str> = Vec::new();
    if let Some(cond) = &job.if_condition {
        texts.push(cond);
    }
    for step in &job.steps {
        if let Some(cond) = &step.condition {
            texts.push(cond);
        }
    }
    texts.extend(job.outputs.values().map(|v| v.as_str()));
    for text in texts {
        for name in crate::ifexpr::steps_refs(text) {
            if !ids.contains(&name) {
                bail!("job {job_id} references unknown step {name:?}");
            }
        }
    }
    Ok(())
}

fn valid_step_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_step(value: &serde_yaml::Value) -> Result<WorkflowStep> {
    let serde_yaml::Value::Mapping(map) = value else {
        bail!("each step must be a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let run = get("run").and_then(yaml_scalar_string);
    let uses = get("uses").and_then(yaml_scalar_string);
    if run.is_none() && uses.is_none() {
        bail!("each step needs either run or uses");
    }
    let condition = get("if").and_then(yaml_scalar_string);
    if let Some(raw) = &condition {
        if crate::ifexpr::mentions_cancelled(raw) {
            bail!("step has job-level if condition {raw:?}; cancelled() is only valid on jobs");
        }
        if !crate::ifexpr::is_valid(raw) {
            bail!("step has unparsable if condition {raw:?}");
        }
    }
    let id = get("id").and_then(yaml_scalar_string);
    if let Some(step_id) = &id {
        if !valid_step_id(step_id) {
            bail!("step id {step_id:?} must match [A-Za-z_][A-Za-z0-9_-]*");
        }
    }
    Ok(WorkflowStep {
        name: get("name").and_then(yaml_scalar_string).unwrap_or_default(),
        run,
        uses,
        with: parse_string_map(get("with")),
        env: parse_string_map(get("env")),
        condition,
        continue_on_error: parse_bool_flag(get("continue-on-error")),
        working_directory: parse_working_directory(get("working-directory"), "step")?,
        id,
        shell: parse_shell(get("shell"), "step")?,
    })
}

fn parse_working_directory(
    value: Option<&serde_yaml::Value>,
    owner: &str,
) -> Result<Option<String>> {
    let Some(raw) = value.and_then(yaml_scalar_string) else {
        return Ok(None);
    };
    let dir = raw.trim();
    if dir.is_empty() || dir.starts_with('/') {
        bail!("{owner} has bad working-directory {raw:?}: want a relative path");
    }
    if dir.split('/').any(|c| c == "..") {
        bail!("{owner} has bad working-directory {raw:?}: .. is not allowed");
    }
    if dir.chars().any(|c| c.is_control()) {
        bail!("{owner} has bad working-directory {raw:?}: control characters are not allowed");
    }
    Ok(Some(dir.to_string()))
}

fn parse_defaults_block(
    value: Option<&serde_yaml::Value>,
    owner: &str,
) -> Result<WorkflowDefaults> {
    let mut out = WorkflowDefaults::default();
    let Some(serde_yaml::Value::Mapping(map)) = value else {
        if value.is_some() {
            bail!("{owner} defaults must be a mapping");
        }
        return Ok(out);
    };
    let run_key = serde_yaml::Value::String("run".to_string());
    let Some(run) = map.get(&run_key) else {
        return Ok(out);
    };
    let serde_yaml::Value::Mapping(run_map) = run else {
        bail!("{owner} defaults.run must be a mapping");
    };
    let wd_key = serde_yaml::Value::String("working-directory".to_string());
    out.run.working_directory =
        parse_working_directory(run_map.get(&wd_key), &format!("{owner} defaults.run"))?;
    let shell_key = serde_yaml::Value::String("shell".to_string());
    out.run.shell = parse_shell(run_map.get(&shell_key), &format!("{owner} defaults.run"))?;
    Ok(out)
}

fn parse_bool_flag(value: Option<&serde_yaml::Value>) -> bool {
    match value {
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::String(s)) => s.trim().eq_ignore_ascii_case("true"),
        _ => false,
    }
}

fn parse_job(job_id: &str, value: &serde_yaml::Value) -> Result<WorkflowJob> {
    let serde_yaml::Value::Mapping(map) = value else {
        bail!("job {job_id} must be a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let uses = get("uses").and_then(yaml_scalar_string);
    if let Some(target) = &uses {
        parse_uses_target(target).with_context(|| format!("job {job_id} has a bad uses target"))?;
        if get("steps").is_some() {
            bail!("job {job_id} cannot combine uses with steps");
        }
        if get("runs-on").is_some() {
            bail!("job {job_id} cannot combine uses with runs-on");
        }
    }
    let permissions = parse_permissions_decl(get("permissions"), &format!("job {job_id}"))?;
    if uses.is_none() && permissions.is_inherit() {
        bail!("job {job_id} uses permissions: inherit without a reusable call");
    }
    let steps_value = get("steps");
    let mut steps = Vec::new();
    match steps_value {
        Some(serde_yaml::Value::Sequence(items)) => {
            for item in items {
                steps.push(
                    parse_step(item).with_context(|| format!("job {job_id} has a bad step"))?,
                );
            }
        }
        Some(_) => bail!("job {job_id} steps must be a list"),
        None => {
            if uses.is_none() {
                bail!("job {job_id} has no steps");
            }
        }
    }
    if steps.is_empty() && uses.is_none() {
        bail!("job {job_id} has no steps");
    }
    let strategy = if let Some(serde_yaml::Value::Mapping(strat)) = get("strategy") {
        let matrix_key = serde_yaml::Value::String("matrix".to_string());
        let fail_key = serde_yaml::Value::String("fail-fast".to_string());
        let include_key = serde_yaml::Value::String("include".to_string());
        let exclude_key = serde_yaml::Value::String("exclude".to_string());
        let max_key = serde_yaml::Value::String("max-parallel".to_string());
        let max_parallel = match parse_job_resource(strat.get(&max_key), job_id, "max-parallel")? {
            Some(n) if n <= u32::MAX as u64 => Some(n as u32),
            Some(_) => bail!("job {job_id} has bad max-parallel: want a positive integer"),
            None => None,
        };
        let matrix_value = strat.get(&matrix_key);
        let matrix_map = match matrix_value {
            Some(serde_yaml::Value::Mapping(m)) => Some(m),
            _ => None,
        };
        WorkflowStrategy {
            matrix: parse_matrix(matrix_value),
            include: parse_rule_list(
                matrix_map.and_then(|m| m.get(&include_key)),
                job_id,
                "include",
            )?,
            exclude: parse_rule_list(
                matrix_map.and_then(|m| m.get(&exclude_key)),
                job_id,
                "exclude",
            )?,
            fail_fast: parse_fail_fast(strat.get(&fail_key), job_id)?,
            max_parallel,
        }
    } else {
        WorkflowStrategy::default()
    };
    let timeout_minutes = get("timeout-minutes").and_then(|v| match v {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.parse::<u64>().ok(),
        _ => None,
    });
    let cpu = match parse_job_resource(get("cpu"), job_id, "cpu")? {
        Some(n) if n <= u32::MAX as u64 => Some(n as u32),
        Some(_) => bail!("job {job_id} has bad cpu: want a positive integer"),
        None => None,
    };
    let ram_mb = parse_job_resource(get("ram-mb").or_else(|| get("ram_mb")), job_id, "ram-mb")?;
    let disk_mb = parse_job_resource(get("disk-mb").or_else(|| get("disk_mb")), job_id, "disk-mb")?;
    Ok(WorkflowJob {
        name: get("name").and_then(yaml_scalar_string).unwrap_or_default(),
        runs_on: parse_runs_on(get("runs-on")),
        needs: parse_needs(get("needs")),
        strategy,
        steps,
        env: parse_string_map(get("env")),
        secrets: parse_job_secrets(get("secrets"), job_id)?,
        secrets_inherit: matches!(get("secrets"), Some(serde_yaml::Value::String(s)) if s == "inherit"),
        timeout_minutes,
        docker_image: get("docker-image")
            .or_else(|| get("docker_image"))
            .and_then(yaml_scalar_string),
        gpu: matches!(get("gpu"), Some(serde_yaml::Value::Bool(true))),
        network: matches!(get("network"), Some(serde_yaml::Value::Bool(true))),
        cpu,
        ram_mb,
        disk_mb,
        if_condition: parse_job_if(get("if"), job_id)?,
        uses,
        with: parse_string_map(get("with")),
        artifacts: get("artifacts").map(yaml_string_list).unwrap_or_default(),
        concurrency: parse_concurrency(get("concurrency"), &format!("job {job_id}"))?,
        environment: parse_environment(get("environment"), job_id)?,
        continue_on_error: parse_bool_flag(get("continue-on-error")),
        defaults: parse_defaults_block(get("defaults"), &format!("job {job_id}"))?,
        outputs: parse_string_map(get("outputs")),
        services: parse_services(get("services"), job_id)?,
        container: parse_container(get("container"), job_id)?,
        call_id: None,
        call_outputs: HashMap::new(),
        permissions,
        effective_permissions: None,
    })
}

pub fn parse_permissions_decl(
    value: Option<&serde_yaml::Value>,
    owner: &str,
) -> Result<PermissionsDecl> {
    let Some(value) = value else {
        return Ok(PermissionsDecl::Empty);
    };
    match value {
        serde_yaml::Value::Null => Ok(PermissionsDecl::Empty),
        serde_yaml::Value::String(s) => {
            let decl = PermissionsDecl::Shorthand(s.clone());
            if decl.is_inherit() {
                return Ok(decl);
            }
            decl.resolve()
                .with_context(|| format!("{owner} has bad permissions"))?;
            Ok(decl)
        }
        serde_yaml::Value::Mapping(map) => {
            let mut out = HashMap::new();
            for (k, v) in map {
                let Some(scope) = yaml_scalar_string(k) else {
                    bail!("{owner} has a non-string permission scope");
                };
                let Some(level) = yaml_scalar_string(v) else {
                    bail!("{owner} permission '{scope}' must be none, read, or write");
                };
                out.insert(scope, level);
            }
            let decl = PermissionsDecl::Map(out);
            decl.resolve()
                .with_context(|| format!("{owner} has bad permissions"))?;
            Ok(decl)
        }
        _ => bail!("{owner} permissions must be a mapping, read-all, or write-all"),
    }
}

pub fn effective_job_permissions(
    workflow: &Workflow,
    job: &WorkflowJob,
    incoming: Option<&Permissions>,
) -> Result<Permissions> {
    let base = workflow.permissions.resolve()?;
    if job.permissions.is_inherit() {
        anyhow::bail!("only reusable calls may use permissions: inherit");
    }
    let declared = base.overlay(&job.permissions)?;
    Ok(match incoming {
        Some(grant) => declared.intersect(grant),
        None => declared,
    })
}

pub fn call_grant(caller: &Permissions, call_decl: &PermissionsDecl) -> Result<Permissions> {
    if call_decl.is_empty() || call_decl.is_inherit() {
        return Ok(caller.clone());
    }
    Ok(caller.intersect(&call_decl.resolve()?))
}

fn parse_job_resource(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
    key: &str,
) -> Result<Option<u64>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    let parsed = match raw {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    match parsed {
        Some(n) if n > 0 => Ok(Some(n)),
        _ => bail!("job {job_id} has bad {key}: want a positive integer"),
    }
}

fn parse_concurrency(
    value: Option<&serde_yaml::Value>,
    owner: &str,
) -> Result<Option<Concurrency>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if let Some(group) = yaml_scalar_string(raw) {
        return Ok(Some(Concurrency::Simple(group)));
    }
    let serde_yaml::Value::Mapping(map) = raw else {
        bail!("{owner} concurrency must be a string or a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let Some(group) = get("group").and_then(yaml_scalar_string) else {
        bail!("{owner} concurrency mapping needs a string group");
    };
    let cancel = match get("cancel-in-progress") {
        None => false,
        Some(serde_yaml::Value::Bool(b)) => *b,
        Some(serde_yaml::Value::String(s)) => match s.trim().to_lowercase().as_str() {
            "true" => true,
            "false" => false,
            _ => bail!("{owner} concurrency cancel-in-progress must be true or false"),
        },
        Some(_) => bail!("{owner} concurrency cancel-in-progress must be true or false"),
    };
    Ok(Some(Concurrency::Full {
        group,
        cancel_in_progress: cancel,
    }))
}

fn parse_environment(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
) -> Result<Option<Environment>> {
    let Some(raw) = value else {
        return Ok(None);
    };
    if let Some(name) = yaml_scalar_string(raw) {
        return Ok(Some(Environment::Simple(name)));
    }
    let serde_yaml::Value::Mapping(map) = raw else {
        bail!("job {job_id} environment must be a string or a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let Some(name) = get("name").and_then(yaml_scalar_string) else {
        bail!("job {job_id} environment mapping needs a string name");
    };
    let url = get("url").and_then(yaml_scalar_string).unwrap_or_default();
    Ok(Some(Environment::Full { name, url }))
}

fn parse_fail_fast(value: Option<&serde_yaml::Value>, job_id: &str) -> Result<bool> {
    let Some(raw) = value else {
        return Ok(true);
    };
    match raw {
        serde_yaml::Value::Bool(b) => Ok(*b),
        serde_yaml::Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" => Ok(true),
            "false" => Ok(false),
            _ => bail!("job {job_id} has bad fail-fast: want true or false"),
        },
        _ => bail!("job {job_id} has bad fail-fast: want true or false"),
    }
}

fn parse_job_if(value: Option<&serde_yaml::Value>, job_id: &str) -> Result<Option<String>> {
    let Some(raw) = value.and_then(yaml_scalar_string) else {
        return Ok(None);
    };
    if !crate::ifexpr::is_valid(&raw) {
        bail!("job {job_id} has unparsable if condition {raw:?}");
    }
    Ok(Some(raw))
}

fn parse_rule_list(
    value: Option<&serde_yaml::Value>,
    job_id: &str,
    kind: &str,
) -> Result<Vec<HashMap<String, String>>> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let serde_yaml::Value::Sequence(items) = value else {
        bail!("job {job_id} matrix {kind} must be a list");
    };
    let mut out = Vec::new();
    for item in items {
        let serde_yaml::Value::Mapping(map) = item else {
            bail!("job {job_id} matrix {kind} entries must be mappings");
        };
        let mut rule = HashMap::new();
        for (k, v) in map {
            if let (Some(key), Some(val)) = (yaml_scalar_string(k), yaml_scalar_string(v)) {
                rule.insert(key, val);
            }
        }
        if !rule.is_empty() {
            out.push(rule);
        }
    }
    Ok(out)
}

pub fn parse_workflow(yaml: &str) -> Result<Workflow> {
    let value: serde_yaml::Value = serde_yaml::from_str(yaml)?;
    let serde_yaml::Value::Mapping(map) = &value else {
        bail!("workflow must be a mapping");
    };
    let get = |key: &str| map.get(serde_yaml::Value::String(key.to_string()));
    let on_value = get("on")
        .or_else(|| map.get(serde_yaml::Value::Bool(true)))
        .cloned()
        .unwrap_or(serde_yaml::Value::Null);
    let triggers = parse_triggers(&on_value);
    let jobs_value = get("jobs").cloned().unwrap_or(serde_yaml::Value::Null);
    let serde_yaml::Value::Mapping(jobs_map) = &jobs_value else {
        bail!("workflow needs a jobs mapping");
    };
    if jobs_map.is_empty() {
        bail!("workflow needs at least one job");
    }
    let mut jobs = HashMap::new();
    for (k, v) in jobs_map {
        let Some(job_id) = yaml_scalar_string(k) else {
            bail!("job ids must be strings");
        };
        let job = parse_job(&job_id, v)?;
        check_step_refs(&job, &job_id)?;
        jobs.insert(job_id.clone(), job);
    }
    Ok(Workflow {
        name: get("name").and_then(yaml_scalar_string).unwrap_or_default(),
        on: triggers,
        concurrency: parse_concurrency(get("concurrency"), "workflow")?,
        jobs,
        source_dir: None,
        defaults: parse_defaults_block(get("defaults"), "workflow")?,
        env: parse_string_map(get("env")),
        permissions: parse_permissions_decl(get("permissions"), "workflow").and_then(|decl| {
            if decl.is_inherit() {
                anyhow::bail!("workflow top-level permissions cannot use inherit");
            }
            Ok(decl)
        })?,
    })
}

pub fn branch_matches(filter: &BranchFilter, branch: &str) -> bool {
    for ignored in &filter.branches_ignore {
        if glob_match(ignored, branch) {
            return false;
        }
    }
    if filter.branches.is_empty() {
        return filter.tags.is_empty() && filter.tags_ignore.is_empty();
    }
    filter.branches.iter().any(|p| glob_match(p, branch))
}

pub fn tag_matches(filter: &BranchFilter, tag: &str) -> bool {
    for ignored in &filter.tags_ignore {
        if glob_match(ignored, tag) {
            return false;
        }
    }
    if filter.tags.is_empty() {
        return filter.branches.is_empty() && filter.branches_ignore.is_empty();
    }
    filter.tags.iter().any(|p| glob_match(p, tag))
}

pub fn glob_match(pattern: &str, branch: &str) -> bool {
    if pattern == "*" || pattern == "**" {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix("/**") {
        return branch == prefix || branch.starts_with(&format!("{prefix}/"));
    }
    if let Some(prefix) = pattern.strip_suffix("/*") {
        let rest = branch.strip_prefix(&format!("{prefix}/")).unwrap_or("");
        return !rest.is_empty() && !rest.contains('/');
    }
    if !pattern.contains('*') {
        return pattern == branch;
    }
    let mut parts = pattern.split('*');
    let first = parts.next().unwrap_or("");
    if !branch.starts_with(first) {
        return false;
    }
    let mut rest = &branch[first.len()..];
    let mut middle: Vec<&str> = parts.collect();
    let last = middle.pop().unwrap_or("");
    for part in &middle {
        if part.is_empty() {
            continue;
        }
        match rest.find(part) {
            Some(pos) => rest = &rest[pos + part.len()..],
            None => return false,
        }
    }
    rest.ends_with(last)
}

impl WorkflowTriggers {
    pub fn matches(&self, event: &WorkflowEvent) -> bool {
        match event {
            WorkflowEvent::Push { branch } => self
                .push
                .as_ref()
                .is_some_and(|f| branch_matches(f, branch)),
            WorkflowEvent::PullRequest { branch } => self
                .pull_request
                .as_ref()
                .is_some_and(|f| branch_matches(f, branch)),
            WorkflowEvent::WorkflowDispatch { .. } => self.workflow_dispatch.is_some(),
            WorkflowEvent::Tag { tag } => self.push.as_ref().is_some_and(|f| tag_matches(f, tag)),
            WorkflowEvent::WorkflowRun {
                workflow,
                file,
                branch,
                ..
            } => self.workflow_run.as_ref().is_some_and(|def| {
                (def.workflows.iter().any(|w| w == workflow || w == file))
                    && (def.types.is_empty() || def.types.iter().any(|t| t == "completed"))
                    && branch_matches(&def.branches, branch)
            }),
            WorkflowEvent::Schedule { .. } => self.schedule.is_some(),
        }
    }
}

impl Workflow {
    pub fn matches(&self, event: &WorkflowEvent) -> bool {
        self.on.matches(event)
    }
}

fn expand_matrix(strategy: &WorkflowStrategy) -> Vec<HashMap<String, String>> {
    let mut keys: Vec<&String> = strategy.matrix.keys().collect();
    keys.sort();
    let mut combos: Vec<HashMap<String, String>> = vec![HashMap::new()];
    for key in keys {
        let mut next = Vec::new();
        if let Some(values) = strategy.matrix.get(key) {
            for combo in &combos {
                for value in values {
                    let mut grown = combo.clone();
                    grown.insert(key.clone(), value.clone());
                    next.push(grown);
                }
            }
        }
        combos = next;
    }
    for extra in &strategy.include {
        if !extra.is_empty() {
            combos.push(extra.clone());
        }
    }
    if !strategy.exclude.is_empty() {
        combos.retain(|combo| {
            !strategy.exclude.iter().any(|rule| {
                !rule.is_empty()
                    && rule
                        .iter()
                        .all(|(k, v)| combo.get(k).is_some_and(|got| got == v))
            })
        });
    }
    if combos.is_empty() {
        combos.push(HashMap::new());
    }
    combos
}

pub fn substitute_matrix(text: &str, matrix: &HashMap<String, String>) -> String {
    let mut out = text.to_string();
    for (key, value) in matrix {
        let plain = format!("${{{{ matrix.{key} }}}}");
        out = out.replace(&plain, value);
        let tight = format!("${{{{matrix.{key}}}}}");
        out = out.replace(&tight, value);
    }
    out
}

pub fn substitute_secrets(text: &str, secrets: &HashMap<String, String>) -> String {
    substitute_context(text, secrets, "secrets")
}

pub fn substitute_inputs(text: &str, inputs: &HashMap<String, String>) -> String {
    substitute_context(text, inputs, "inputs")
}

pub fn substitute_github(text: &str, github: &HashMap<String, String>) -> String {
    substitute_context(text, github, "github")
}

fn valid_context_name(name: &str) -> bool {
    !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

fn substitute_context(text: &str, values: &HashMap<String, String>, prefix: &str) -> String {
    let mut out = text.to_string();
    for (key, value) in values {
        let plain = format!("${{{{ {prefix}.{key} }}}}");
        out = out.replace(&plain, value);
        let tight = format!("${{{{{prefix}.{key}}}}}");
        out = out.replace(&tight, value);
    }
    let mut at = 0;
    while let Some(rel) = out[at..].find("${{") {
        let start = at + rel;
        let Some(rel_end) = out[start..].find("}}") else {
            break;
        };
        let end = start + rel_end + 2;
        let inner = out[start + 3..end - 2].trim();
        let is_ours = inner
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('.'))
            .map(valid_context_name)
            .unwrap_or(false);
        if is_ours {
            out.replace_range(start..end, "");
            at = start;
        } else {
            at = end;
        }
    }
    out
}

fn action_step_script(uses: &str, with: &HashMap<String, String>) -> String {
    let action = uses.split('@').next().unwrap_or(uses);
    match action {
        "actions/checkout" => "echo 'repo already present, skipping checkout'".to_string(),
        "gitforest/llm" => crate::llm::llm_step_script(with),
        "gitforest/cache" => crate::cache::cache_restore_step_script(with),
        "gitforest/cache-save" => crate::cache::cache_save_step_script(with),
        _ => format!("echo 'external action {action} has no gitforest runner, skipping'"),
    }
}

fn default_image_for_labels(labels: &[String]) -> String {
    let joined = labels.join(" ").to_lowercase();
    if joined.contains("rust") {
        "rust:1.75".to_string()
    } else if joined.contains("node") {
        "node:20".to_string()
    } else if joined.contains("python") {
        "python:3.12".to_string()
    } else if joined.contains("golang") || joined.contains("go-") {
        "golang:1.22".to_string()
    } else {
        "ubuntu:22.04".to_string()
    }
}

pub fn expand_job(
    workflow: &Workflow,
    job_id: &str,
    job: &WorkflowJob,
) -> Result<Vec<ExpandedJob>> {
    let workflow_name = workflow.name.as_str();
    let combos = expand_matrix(&job.strategy);
    let combo_count = combos.len();
    let run_if = job
        .if_condition
        .as_deref()
        .map(crate::job::RunCondition::parse_full)
        .unwrap_or_default();
    let labels = job.runs_on.labels();
    let image = job
        .container
        .as_ref()
        .map(|c| c.image.clone())
        .or_else(|| job.docker_image.clone())
        .unwrap_or_else(|| default_image_for_labels(&labels));
    let timeout_secs = job.timeout_minutes.unwrap_or(60).saturating_mul(60).max(60);
    let permissions = match &job.effective_permissions {
        Some(pre) => pre.clone(),
        None => effective_job_permissions(workflow, job, None)
            .with_context(|| format!("job {job_id} has bad permissions"))?,
    };
    let llm_gpu = job.steps.iter().any(|step| {
        let action = step
            .uses
            .as_deref()
            .unwrap_or("")
            .split('@')
            .next()
            .unwrap_or("");
        action == "gitforest/llm"
            && step
                .with
                .get("gpu")
                .is_some_and(|v| v.trim().eq_ignore_ascii_case("true"))
    });
    let gpu = job.gpu || llm_gpu || labels.iter().any(|l| l.to_lowercase().contains("gpu"));
    let jobs: Vec<ExpandedJob> = combos
        .into_iter()
        .enumerate()
        .map(|(index, matrix)| {
            let secrets: HashMap<String, String> = job
                .secrets
                .iter()
                .map(|(k, v)| (k.clone(), substitute_matrix(v, &matrix)))
                .collect();
            let fill = |text: &str| substitute_secrets(&substitute_matrix(text, &matrix), &secrets);
            let mut steps = Vec::new();
            let mut env: HashMap<String, String> = workflow
                .env
                .iter()
                .map(|(k, v)| (k.clone(), fill(v)))
                .collect();
            env.extend(job.env.clone());
            for step in &job.steps {
                for (k, v) in &step.env {
                    env.insert(k.clone(), fill(v));
                }
                let name = if step.name.is_empty() {
                    step.uses.clone().unwrap_or_default()
                } else {
                    fill(&step.name)
                };
                let run = match (&step.run, &step.uses) {
                    (Some(script), _) => {
                        let body = fill(script);
                        let shell = step
                            .shell
                            .as_deref()
                            .or(job.defaults.run.shell.as_deref())
                            .or(workflow.defaults.run.shell.as_deref())
                            .unwrap_or("sh");
                        if shell == "bash" {
                            format!("bash -e -c {}", crate::llm::shell_quote(&body))
                        } else {
                            body
                        }
                    }
                    (None, Some(uses)) => {
                        let filled: HashMap<String, String> = step
                            .with
                            .iter()
                            .map(|(k, v)| (k.clone(), fill(v)))
                            .collect();
                        action_step_script(uses, &filled)
                    }
                    (None, None) => continue,
                };
                let condition = step
                    .condition
                    .as_deref()
                    .map(crate::job::RunCondition::parse_full)
                    .unwrap_or_default();
                steps.push(JobStep {
                    name,
                    run,
                    condition,
                    continue_on_error: step.continue_on_error,
                    working_directory: step
                        .working_directory
                        .clone()
                        .or_else(|| job.defaults.run.working_directory.clone())
                        .map(|d| fill(&d)),
                    id: step.id.clone(),
                });
            }
            let base_name = if job.name.is_empty() {
                job_id.to_string()
            } else {
                fill(&job.name)
            };
            let display_name = if matrix.is_empty() {
                base_name.clone()
            } else {
                let mut pairs: Vec<String> =
                    matrix.iter().map(|(k, v)| format!("{k}={v}")).collect();
                pairs.sort();
                format!("{base_name} ({})", pairs.join(", "))
            };
            let expanded_id = if combo_count <= 1 {
                job_id.to_string()
            } else {
                format!("{job_id}-{index}")
            };
            let matrix_parent = if combo_count > 1 {
                Some(job_id.to_string())
            } else {
                None
            };
            let (concurrency_group, cancel_in_progress) = match &job.concurrency {
                Some(concurrency) => (
                    substitute_matrix(concurrency.group(), &matrix),
                    concurrency.cancel_in_progress(),
                ),
                None => (String::new(), false),
            };
            let (environment, environment_url) = match &job.environment {
                Some(environment) => (
                    substitute_matrix(environment.name(), &matrix),
                    substitute_matrix(environment.url(), &matrix),
                ),
                None => (String::new(), String::new()),
            };
            let mapped_outputs: HashMap<String, String> = job
                .outputs
                .iter()
                .map(|(k, v)| (k.clone(), fill(v)))
                .collect();
            let mut service_names: Vec<&String> = job.services.keys().collect();
            service_names.sort();
            let services: Vec<crate::job::JobService> = service_names
                .into_iter()
                .map(|name| {
                    let svc = &job.services[name];
                    crate::job::JobService {
                        name: name.clone(),
                        image: fill(&svc.image),
                        env: svc.env.iter().map(|(k, v)| (k.clone(), fill(v))).collect(),
                        ports: svc.ports.clone(),
                        health: svc.health.clone(),
                    }
                })
                .collect();
            let container = job.container.as_ref().map(|c| crate::job::JobContainer {
                image: fill(&c.image),
                credentials: c.credentials.as_ref().map(|creds| {
                    crate::job::JobContainerCredentials {
                        username: fill(&creds.username),
                        password: fill(&creds.password),
                    }
                }),
                env: c.env.iter().map(|(k, v)| (k.clone(), fill(v))).collect(),
                ports: c.ports.clone(),
                volumes: c.volumes.clone(),
                resources: c.resources.clone(),
                options: c.options.clone(),
            });
            let docker_image = container
                .as_ref()
                .map(|c| c.image.clone())
                .unwrap_or_else(|| image.clone());
            ExpandedJob {
                workflow_name: workflow_name.to_string(),
                job_id: expanded_id,
                call_id: job.call_id.clone(),
                call_outputs: job.call_outputs.clone(),
                display_name,
                matrix,
                matrix_parent,
                fail_fast: job.strategy.fail_fast,
                job_config: JobConfig {
                    name: base_name,
                    steps,
                    docker_image,
                    timeout_secs,
                    env: env.clone(),
                    secrets: secrets.clone(),
                    artifacts: job.artifacts.clone(),
                    outputs: mapped_outputs,
                    services,
                    container,
                    job_token: String::new(),
                },
                requirements: ComputeRequirements {
                    min_cpu_cores: job.cpu.unwrap_or(DEFAULT_JOB_CPU_CORES),
                    min_ram_mb: job.ram_mb.unwrap_or(DEFAULT_JOB_RAM_MB),
                    min_disk_mb: job.disk_mb.unwrap_or(DEFAULT_JOB_DISK_MB),
                    gpu_required: gpu,
                    network_access: job.network,
                },
                needs: job.needs.list(),
                run_if: run_if.clone(),
                concurrency_group,
                cancel_in_progress,
                environment,
                environment_url,
                continue_on_error: job.continue_on_error,
                max_parallel: job.strategy.max_parallel.unwrap_or(0),
                permissions: permissions.clone(),
            }
        })
        .collect::<Vec<ExpandedJob>>();
    Ok(jobs)
}

pub fn expand_workflow(workflow: &Workflow) -> Result<Vec<ExpandedJob>> {
    let mut by_job: HashMap<String, Vec<ExpandedJob>> = HashMap::new();
    for (job_id, job) in &workflow.jobs {
        let mut job = job.clone();
        if job.defaults.run.working_directory.is_none() {
            job.defaults.run.working_directory = workflow.defaults.run.working_directory.clone();
        }
        by_job.insert(
            job_id.clone(),
            expand_job(workflow, job_id, &job).with_context(|| format!("expand job {job_id}"))?,
        );
    }
    let mut out = Vec::new();
    for (job_id, expanded) in &by_job {
        let _ = job_id;
        for job in expanded {
            let mut resolved_needs = Vec::new();
            for need in &job.needs {
                if let Some(deps) = by_job.get(need) {
                    for dep in deps {
                        resolved_needs.push(dep.job_id.clone());
                    }
                } else {
                    let prefix = format!("{need}/");
                    let mut fanned: Vec<String> = by_job
                        .values()
                        .flat_map(|deps| deps.iter().map(|dep| dep.job_id.clone()))
                        .filter(|id| id.starts_with(&prefix))
                        .collect();
                    if fanned.is_empty() {
                        resolved_needs.push(need.clone());
                    } else {
                        fanned.sort();
                        resolved_needs.extend(fanned);
                    }
                }
            }
            let mut owned = job.clone();
            owned.needs = resolved_needs;
            if owned.concurrency_group.is_empty() {
                if let Some(default) = &workflow.concurrency {
                    owned.concurrency_group = substitute_matrix(default.group(), &owned.matrix);
                    owned.cancel_in_progress = default.cancel_in_progress();
                }
            }
            out.push(owned);
        }
    }
    out.sort_by(|a, b| a.job_id.cmp(&b.job_id));
    Ok(out)
}

pub fn order_jobs(jobs: &[ExpandedJob]) -> Result<Vec<ExpandedJob>> {
    let ids: HashMap<&str, &ExpandedJob> = jobs.iter().map(|j| (j.job_id.as_str(), j)).collect();
    for job in jobs {
        for need in &job.needs {
            if !ids.contains_key(need.as_str()) {
                bail!("job {} needs unknown job {need}", job.job_id);
            }
            if need == &job.job_id {
                bail!("job {} depends on itself", job.job_id);
            }
        }
    }
    let mut ordered: Vec<ExpandedJob> = Vec::new();
    let mut done: HashMap<&str, bool> = HashMap::new();
    let mut visiting: Vec<&str> = Vec::new();
    fn visit<'a>(
        id: &'a str,
        ids: &HashMap<&'a str, &'a ExpandedJob>,
        done: &mut HashMap<&'a str, bool>,
        visiting: &mut Vec<&'a str>,
        ordered: &mut Vec<ExpandedJob>,
    ) -> Result<()> {
        if done.get(id).copied().unwrap_or(false) {
            return Ok(());
        }
        if visiting.contains(&id) {
            bail!("dependency cycle at job {id}");
        }
        visiting.push(id);
        if let Some(job) = ids.get(id) {
            for need in &job.needs {
                visit(need.as_str(), ids, done, visiting, ordered)?;
            }
            ordered.push((*job).clone());
        }
        visiting.pop();
        done.insert(id, true);
        Ok(())
    }
    let mut ids_sorted: Vec<&str> = ids.keys().copied().collect();
    ids_sorted.sort();
    for id in ids_sorted {
        visit(id, &ids, &mut done, &mut visiting, &mut ordered)?;
    }
    Ok(ordered)
}

pub fn workflow_files_in(repo_path: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for dir in [
        "gitforest/workflows",
        ".gitforest/workflows",
        ".github/workflows",
    ] {
        let base = repo_path.join(dir);
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        let mut paths: Vec<std::path::PathBuf> = entries
            .filter_map(|e| e.ok().map(|entry| entry.path()))
            .filter(|p| {
                matches!(
                    p.extension().and_then(|e| e.to_str()),
                    Some("yml") | Some("yaml")
                )
            })
            .collect();
        paths.sort();
        out.extend(paths);
    }
    out
}

pub fn parse_workflow_file(path: &std::path::Path) -> Result<Workflow> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let mut workflow = parse_workflow(&text)?;
    workflow.source_dir = path
        .parent()
        .map(|p| p.to_path_buf())
        .filter(|p| !p.as_os_str().is_empty());
    Ok(workflow)
}

pub fn load_workflows(repo_path: &std::path::Path) -> Vec<(std::path::PathBuf, Result<Workflow>)> {
    workflow_files_in(repo_path)
        .into_iter()
        .map(|path| {
            let parsed = parse_workflow_file(&path);
            (path, parsed)
        })
        .collect()
}

pub fn resolve_dispatch_inputs(
    workflow: &Workflow,
    provided: &HashMap<String, String>,
) -> Result<HashMap<String, String>> {
    let mut resolved = HashMap::new();
    let Some(def) = workflow.on.workflow_dispatch.as_ref() else {
        if !provided.is_empty() {
            bail!(
                "workflow dispatch passes inputs to a workflow without a workflow_dispatch trigger"
            );
        }
        return Ok(resolved);
    };
    for (name, spec) in &def.inputs {
        if let Some(value) = provided.get(name) {
            resolved.insert(name.clone(), value.clone());
        } else if let Some(default) = &spec.default {
            resolved.insert(name.clone(), default.clone());
        } else if spec.required {
            bail!("workflow dispatch lacks required input {name}");
        } else {
            resolved.insert(name.clone(), String::new());
        }
    }
    for name in provided.keys() {
        if !def.inputs.contains_key(name) {
            bail!("workflow dispatch passes undeclared input {name}");
        }
    }
    Ok(resolved)
}

fn apply_inputs_to_job(job: &mut WorkflowJob, inputs: &HashMap<String, String>) {
    if !job.name.is_empty() {
        job.name = substitute_inputs(&job.name, inputs);
    }
    if let Some(concurrency) = job.concurrency.as_mut() {
        concurrency.map_group(|group| substitute_inputs(group, inputs));
    }
    if let Some(environment) = job.environment.as_mut() {
        environment.map_parts(|part| substitute_inputs(part, inputs));
    }
    for value in job.env.values_mut() {
        *value = substitute_inputs(value, inputs);
    }
    for svc in job.services.values_mut() {
        svc.image = substitute_inputs(&svc.image, inputs);
        for value in svc.env.values_mut() {
            *value = substitute_inputs(value, inputs);
        }
    }
    for step in &mut job.steps {
        if let Some(run) = step.run.take() {
            step.run = Some(substitute_inputs(&run, inputs));
        }
        if !step.name.is_empty() {
            step.name = substitute_inputs(&step.name, inputs);
        }
        for value in step.env.values_mut() {
            *value = substitute_inputs(value, inputs);
        }
    }
}

pub fn resolve_and_expand_with_secrets(
    workflow: &Workflow,
    extra_secrets: &HashMap<String, String>,
    dispatch_inputs: Option<&HashMap<String, String>>,
) -> Result<Vec<ExpandedJob>> {
    let mut owned = workflow.clone();
    for job in owned.jobs.values_mut() {
        if job.uses.is_some() {
            continue;
        }
        for (key, value) in extra_secrets {
            job.secrets.insert(key.clone(), value.clone());
        }
    }
    let mut scope: Option<HashMap<String, String>> = None;
    if let Some(provided) = dispatch_inputs {
        let resolved = resolve_dispatch_inputs(&owned, provided)?;
        for (id, job) in owned.jobs.iter_mut() {
            check_context_refs(job, id, &resolved, &job.secrets)?;
            apply_inputs_to_job(job, &resolved);
        }
        if let Some(concurrency) = owned.concurrency.as_mut() {
            concurrency.map_group(|group| substitute_inputs(group, &resolved));
        }
        scope = Some(resolved);
    }
    resolve_and_expand_with_scope(&owned, scope.as_ref(), extra_secrets)
}

pub fn resolve_and_expand(workflow: &Workflow) -> Result<Vec<ExpandedJob>> {
    resolve_and_expand_with_scope(workflow, None, &HashMap::new())
}

fn resolve_and_expand_with_scope(
    workflow: &Workflow,
    scope: Option<&HashMap<String, String>>,
    secrets: &HashMap<String, String>,
) -> Result<Vec<ExpandedJob>> {
    let mut stack: Vec<std::path::PathBuf> = Vec::new();
    let level_top = workflow.permissions.resolve()?;
    let jobs = resolve_calls(
        &workflow.jobs,
        workflow.source_dir.as_deref(),
        0,
        &mut stack,
        scope,
        secrets,
        &level_top,
        None,
    )?;
    let mut owned = workflow.clone();
    owned.jobs = jobs;
    expand_workflow(&owned)
}

#[allow(clippy::too_many_arguments)]
fn resolve_calls(
    jobs: &HashMap<String, WorkflowJob>,
    base_dir: Option<&std::path::Path>,
    depth: usize,
    stack: &mut Vec<std::path::PathBuf>,
    scope: Option<&HashMap<String, String>>,
    secrets: &HashMap<String, String>,
    level_top: &Permissions,
    incoming: Option<&Permissions>,
) -> Result<HashMap<String, WorkflowJob>> {
    let level_cap = match incoming {
        Some(grant) => level_top.intersect(grant),
        None => level_top.clone(),
    };
    if depth > 8 {
        bail!("reusable workflow calls nest too deep (over 8)");
    }
    let mut out: HashMap<String, WorkflowJob> = HashMap::new();
    let mut calls: HashMap<String, Vec<String>> = HashMap::new();
    let mut ids: Vec<&String> = jobs.keys().collect();
    ids.sort();
    for id in ids {
        let job = &jobs[id];
        let Some(target) = &job.uses else {
            let mut plain = job.clone();
            plain.steps = inline_composite_steps(&plain.steps, base_dir, depth, stack)?;
            if plain.permissions.is_inherit() {
                bail!("job {id} uses permissions: inherit without a reusable call");
            }
            let declared = level_top
                .overlay(&plain.permissions)
                .with_context(|| format!("job {id} has bad permissions"))?;
            plain.effective_permissions = Some(match incoming {
                Some(grant) => declared.intersect(grant),
                None => declared,
            });
            out.insert(id.clone(), plain);
            continue;
        };
        let Some(dir) = base_dir else {
            bail!("job {id} calls {target} but the calling workflow has no file location");
        };
        let target_path = dir.join(target);
        if stack.contains(&target_path) {
            bail!("job {id} calls {target} recursively");
        }
        let called = parse_workflow_file(&target_path)
            .with_context(|| format!("job {id} calls {target}"))?;
        let Some(def) = &called.on.workflow_call else {
            bail!("job {id} calls {target}, which has no workflow_call trigger");
        };
        let mut inputs: HashMap<String, String> = HashMap::new();
        for (name, spec) in &def.inputs {
            if let Some(raw) = job.with.get(name) {
                let value = match scope {
                    Some(upper) => substitute_inputs(raw, upper),
                    None => {
                        if !context_refs(raw, "inputs").is_empty() {
                            bail!("job {id} input {name} references inputs outside a reusable workflow");
                        }
                        raw.clone()
                    }
                };
                inputs.insert(name.clone(), value);
            } else if let Some(default) = &spec.default {
                inputs.insert(name.clone(), default.clone());
            } else if spec.required {
                bail!("job {id} calls {target} without required input {name}");
            } else {
                inputs.insert(name.clone(), String::new());
            }
        }
        for name in job.with.keys() {
            if !def.inputs.contains_key(name) {
                bail!("job {id} passes undeclared input {name} to {target}");
            }
        }
        for (name, required) in &def.secrets {
            if *required
                && !job.secrets.contains_key(name)
                && !(job.secrets_inherit && secrets.contains_key(name))
            {
                bail!("job {id} calls {target} without required secret {name}");
            }
        }
        for (name, expr) in &def.outputs {
            let Some((inner, _)) = parse_call_output_ref(expr) else {
                bail!("job {id} calls {target} whose output {name} is not a jobs.<job>.outputs.<key> reference");
            };
            if !called.jobs.contains_key(&inner) {
                bail!("job {id} calls {target} whose output {name} references unknown job {inner}");
            }
        }
        let grant = call_grant(&level_cap, &job.permissions)
            .with_context(|| format!("job {id} passes bad permissions to {target}"))?;
        let callee_top = called.permissions.resolve().with_context(|| {
            format!("job {id} calls {target}, which has bad top-level permissions")
        })?;
        stack.push(target_path.clone());
        let inner = resolve_calls(
            &called.jobs,
            called.source_dir.as_deref(),
            depth + 1,
            stack,
            Some(&inputs),
            secrets,
            &callee_top,
            Some(&grant),
        );
        stack.pop();
        let inner = inner?;
        let mut inner_ids: Vec<&String> = inner.keys().collect();
        inner_ids.sort();
        for tid in inner_ids {
            let mut inlined = inner[tid].clone();
            let new_id = format!("{id}/{tid}");
            let mut needs = Vec::new();
            for need in inlined.needs.list() {
                if inner.contains_key(&need) {
                    needs.push(format!("{id}/{need}"));
                } else {
                    needs.push(need);
                }
            }
            if needs.is_empty() {
                needs.extend(job.needs.list());
            }
            inlined.needs = Needs::List(needs);
            for (axis, values) in &job.strategy.matrix {
                inlined
                    .strategy
                    .matrix
                    .entry(axis.clone())
                    .or_insert_with(|| values.clone());
            }
            inlined
                .strategy
                .include
                .extend(job.strategy.include.clone());
            inlined
                .strategy
                .exclude
                .extend(job.strategy.exclude.clone());
            if inlined.if_condition.is_none() {
                inlined.if_condition = job.if_condition.clone();
            }
            if inlined.call_id.is_none() {
                inlined.call_id = Some(id.clone());
                inlined.call_outputs = def.outputs.clone();
            }
            for (key, value) in &job.secrets {
                inlined
                    .secrets
                    .insert(key.clone(), substitute_secrets(value, secrets));
            }
            if job.secrets_inherit {
                for (key, value) in secrets {
                    inlined
                        .secrets
                        .entry(key.clone())
                        .or_insert_with(|| value.clone());
                }
            }
            check_context_refs(&inlined, &new_id, &inputs, &inlined.secrets)?;
            for step in &mut inlined.steps {
                if let Some(run) = step.run.take() {
                    step.run = Some(substitute_inputs(&run, &inputs));
                }
                if !step.name.is_empty() {
                    step.name = substitute_inputs(&step.name, &inputs);
                }
                for value in step.env.values_mut() {
                    *value = substitute_inputs(value, &inputs);
                }
                if let Some(dir) = step.working_directory.take() {
                    step.working_directory = Some(substitute_inputs(&dir, &inputs));
                }
            }
            if let Some(dir) = inlined.defaults.run.working_directory.take() {
                inlined.defaults.run.working_directory = Some(substitute_inputs(&dir, &inputs));
            }
            if !inlined.name.is_empty() {
                inlined.name = substitute_inputs(&inlined.name, &inputs);
            }
            for value in inlined.env.values_mut() {
                *value = substitute_inputs(value, &inputs);
            }
            if out.contains_key(&new_id) {
                bail!("reusable call {id} collides on job id {new_id}");
            }
            calls.entry(id.clone()).or_default().push(new_id.clone());
            out.insert(new_id, inlined);
        }
    }
    for job in out.values_mut() {
        let current = job.needs.list();
        let mut rewritten = Vec::new();
        for need in &current {
            if let Some(members) = calls.get(need) {
                rewritten.extend(members.clone());
            } else {
                rewritten.push(need.clone());
            }
        }
        if rewritten != current {
            job.needs = Needs::List(rewritten);
        }
    }
    Ok(out)
}

fn check_context_refs(
    job: &WorkflowJob,
    job_id: &str,
    inputs: &HashMap<String, String>,
    secrets: &HashMap<String, String>,
) -> Result<()> {
    let mut texts: Vec<&str> = Vec::new();
    texts.push(&job.name);
    texts.extend(job.env.values().map(|v| v.as_str()));
    if let Some(cond) = &job.if_condition {
        texts.push(cond);
    }
    for svc in job.services.values() {
        texts.push(&svc.image);
        texts.extend(svc.env.values().map(|v| v.as_str()));
    }
    if let Some(c) = &job.container {
        texts.push(&c.image);
        texts.extend(c.env.values().map(|v| v.as_str()));
        if let Some(creds) = &c.credentials {
            texts.push(&creds.username);
            texts.push(&creds.password);
        }
    }
    for step in &job.steps {
        texts.push(&step.name);
        if let Some(run) = &step.run {
            texts.push(run);
        }
        texts.extend(step.env.values().map(|v| v.as_str()));
        if let Some(cond) = &step.condition {
            texts.push(cond);
        }
    }
    for text in texts {
        for name in context_refs(text, "inputs") {
            if !inputs.contains_key(&name) {
                bail!("job {job_id} references undeclared input {name}");
            }
        }
        for name in context_refs(text, "secrets") {
            if !secrets.contains_key(&name) {
                bail!("job {job_id} references unknown secret {name}");
            }
        }
    }
    Ok(())
}

fn context_refs(text: &str, prefix: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(rel) = text[at..].find("${{") {
        let start = at + rel;
        let Some(rel_end) = text[start..].find("}}") else {
            break;
        };
        let end = start + rel_end + 2;
        let inner = text[start + 3..end - 2].trim();
        if let Some(name) = inner
            .strip_prefix(prefix)
            .and_then(|rest| rest.strip_prefix('.'))
            .filter(|name| valid_context_name(name))
        {
            out.push(name.to_string());
        }
        at = end;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permissions::{PermissionLevel, PermissionScope};

    const SAMPLE: &str = r#"
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
      - name: Build
        run: cargo build --release
  test:
    needs: build
    runs-on: ubuntu-latest
    strategy:
      matrix:
        rust: [stable, beta]
    steps:
      - run: cargo test --toolchain ${{ matrix.rust }}
"#;

    #[test]
    fn parses_triggers_steps_and_needs() {
        let workflow = parse_workflow(SAMPLE).unwrap();
        assert_eq!(workflow.name, "CI");
        assert!(workflow.on.push.is_some());
        assert!(workflow.on.pull_request.is_some());
        assert!(workflow.on.workflow_dispatch.is_some());
        assert_eq!(workflow.jobs.len(), 2);
        assert_eq!(
            workflow.jobs["test"].needs.list(),
            vec!["build".to_string()]
        );
        assert_eq!(workflow.jobs["build"].steps.len(), 1);
    }

    #[test]
    fn trigger_matching_respects_branches() {
        let workflow = parse_workflow(SAMPLE).unwrap();
        assert!(workflow.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
        assert!(!workflow.matches(&WorkflowEvent::Push {
            branch: "feature".to_string()
        }));
        assert!(workflow.matches(&WorkflowEvent::PullRequest {
            branch: "anything".to_string()
        }));
        assert!(workflow.matches(&WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::new()
        }));
    }

    #[test]
    fn dispatch_inputs_parse_resolve_and_reject() {
        let yaml = r#"
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
        let workflow = parse_workflow(yaml).unwrap();
        let def = workflow.on.workflow_dispatch.as_ref().unwrap();
        assert!(def.inputs["name"].required);
        assert_eq!(def.inputs["greeting"].default.as_deref(), Some("hello"));
        let provided: HashMap<String, String> = [("name".to_string(), "world".to_string())]
            .into_iter()
            .collect();
        let resolved = resolve_dispatch_inputs(&workflow, &provided).unwrap();
        assert_eq!(resolved["name"], "world");
        assert_eq!(resolved["greeting"], "hello");
        let empty: HashMap<String, String> = HashMap::new();
        assert!(resolve_dispatch_inputs(&workflow, &empty).is_err());
        let unknown: HashMap<String, String> = [
            ("name".to_string(), "world".to_string()),
            ("bogus".to_string(), "x".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(resolve_dispatch_inputs(&workflow, &unknown).is_err());
        let bare = parse_workflow(
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        assert!(bare.on.workflow_dispatch.is_none());
        assert!(resolve_dispatch_inputs(&bare, &provided).is_err());
        assert!(resolve_dispatch_inputs(&bare, &empty).is_ok());
    }

    #[test]
    fn dispatch_inputs_substitute_into_expanded_steps() {
        let yaml = r#"
on:
  workflow_dispatch:
    inputs:
      name:
        required: true
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo "hi ${{ inputs.name }}"
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let provided: HashMap<String, String> = [("name".to_string(), "ada".to_string())]
            .into_iter()
            .collect();
        let expanded =
            resolve_and_expand_with_secrets(&workflow, &HashMap::new(), Some(&provided)).unwrap();
        assert_eq!(expanded.len(), 1);
        assert!(
            expanded[0].job_config.steps[0].run.contains("hi ada"),
            "{}",
            expanded[0].job_config.steps[0].run
        );
        let bad_yaml = r#"
on:
  workflow_dispatch:
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo "hi ${{ inputs.name }}"
"#;
        let bad = parse_workflow(bad_yaml).unwrap();
        let empty: HashMap<String, String> = HashMap::new();
        assert!(resolve_and_expand_with_secrets(&bad, &HashMap::new(), Some(&empty)).is_err());
        assert!(resolve_and_expand_with_secrets(&bad, &HashMap::new(), None).is_ok());
    }

    #[test]
    fn workflow_run_trigger_matches() {
        let yaml = r#"
on:
  workflow_run:
    workflows: [CI, release.yml]
    types: [completed]
    branches: [main]
jobs:
  b:
    runs-on: ubuntu-latest
    steps:
      - run: echo b
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let hit = |name: &str, file: &str, branch: &str| {
            workflow.matches(&WorkflowEvent::WorkflowRun {
                workflow: name.to_string(),
                file: file.to_string(),
                conclusion: "success".to_string(),
                branch: branch.to_string(),
            })
        };
        assert!(hit("CI", "ci.yml", "main"));
        assert!(hit("Release", "release.yml", "main"));
        assert!(!hit("Other", "other.yml", "main"));
        assert!(!hit("CI", "ci.yml", "dev"));
        assert!(!workflow.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
    }

    #[test]
    fn cron_due_matches_fields() {
        use chrono::TimeZone;
        let tue = chrono::Utc
            .with_ymd_and_hms(2026, 11, 3, 14, 30, 0)
            .unwrap();
        let sun = chrono::Utc.with_ymd_and_hms(2026, 11, 1, 0, 0, 0).unwrap();
        for cron in [
            "* * * * *",
            "30 14 * * *",
            "*/15 * * * *",
            "30 14 3 11 2",
            "30 9-17 * * 1-5",
            "0,30 14 * * *",
        ] {
            assert!(cron_due(cron, &tue), "{cron}");
        }
        for cron in [
            "31 14 * * *",
            "*/20 * * * *",
            "30 14 3 11 3",
            "0 0 * * 0",
            "61 * * * *",
            "*/0 * * * *",
            "30 14 * *",
            "",
        ] {
            assert!(!cron_due(cron, &tue), "{cron}");
        }
        assert!(cron_due("0 0 * * 0", &sun));
        assert!(cron_due("0 0 * * 7", &sun));
        assert!(!cron_due("0 0 * * 1", &sun));
    }

    #[test]
    fn schedule_trigger_matches() {
        let yaml = r#"
on:
  schedule:
    - cron: '*/5 * * * *'
jobs:
  b:
    runs-on: ubuntu-latest
    steps:
      - run: echo b
"#;
        let workflow = parse_workflow(yaml).unwrap();
        assert_eq!(workflow.on.schedule.as_ref().unwrap().len(), 1);
        assert!(workflow.matches(&WorkflowEvent::Schedule {
            cron: "*/5 * * * *".to_string(),
            branch: "main".to_string(),
        }));
        assert!(!workflow.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
    }

    #[test]
    fn workflow_env_merges_under_job_and_step() {
        let yaml = r#"
on: push
env:
  TOP: wf
  MID: wf
  LEAF: wf
  GREETING: hi-${{ secrets.T }}
jobs:
  build:
    runs-on: ubuntu-latest
    env:
      MID: job
    steps:
      - run: echo hi
        env:
          LEAF: step
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let extras = HashMap::from([("T".to_string(), "s3".to_string())]);
        let expanded = resolve_and_expand_with_secrets(&workflow, &extras, None).unwrap();
        let env = &expanded[0].job_config.env;
        assert_eq!(env.get("TOP").unwrap(), "wf");
        assert_eq!(env.get("MID").unwrap(), "job");
        assert_eq!(env.get("LEAF").unwrap(), "step");
        assert_eq!(env.get("GREETING").unwrap(), "hi-s3");
    }

    #[test]
    fn step_shell_inherits_and_wraps_bash() {
        let yaml = r#"
on: push
defaults:
  run:
    shell: bash
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - run: echo inherited
      - run: echo explicit
        shell: sh
  b:
    runs-on: ubuntu-latest
    defaults:
      run:
        shell: sh
    steps:
      - run: echo job-default
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        let get = |id: &str| {
            expanded
                .iter()
                .find(|j| j.job_id == id)
                .unwrap()
                .job_config
                .steps
                .iter()
                .map(|s| s.run.clone())
                .collect::<Vec<_>>()
        };
        let a = get("a");
        assert!(a[0].starts_with("bash -e -c "), "{}", a[0]);
        assert!(a[0].contains("echo inherited"));
        assert!(!a[1].contains("bash -e -c"), "{}", a[1]);
        let b = get("b");
        assert!(!b[0].contains("bash -e -c"), "{}", b[0]);
        let bad_step = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo x\n        shell: powershell\n";
        assert!(parse_workflow(bad_step).is_err());
        let bad_default = "on: push\ndefaults:\n  run:\n    shell: fish\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo x\n";
        assert!(parse_workflow(bad_default).is_err());
    }

    #[test]
    fn short_trigger_forms_parse() {
        let single = parse_workflow(
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        assert!(single.on.push.is_some());
        let list = parse_workflow("on: [push, pull_request]\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n").unwrap();
        assert!(list.on.push.is_some());
        assert!(list.on.pull_request.is_some());
    }

    #[test]
    fn glob_branches_match() {
        assert!(glob_match("main", "main"));
        assert!(!glob_match("main", "develop"));
        assert!(glob_match("feature/*", "feature/login"));
        assert!(!glob_match("feature/*", "feature/a/b"));
        assert!(glob_match("release/**", "release/2024/may"));
        assert!(glob_match("hotfix-*", "hotfix-123"));
        assert!(glob_match("v*-rc", "v2-rc"));
        assert!(!glob_match("v*-rc", "v1.2.3"));
        assert!(glob_match("release-*-final", "release-2-final"));
        assert!(!glob_match("release-*-final", "release-final"));
        assert!(glob_match("release*-final", "release-final"));
        assert!(!glob_match("release-*-final", "release-2-draft"));
        assert!(glob_match("*hotfix*", "urgent-hotfix-now"));
        assert!(!glob_match("*hotfix*", "urgent-fix-now"));
    }

    #[test]
    fn tag_and_branch_events_match_their_own_filters() {
        let yaml = r#"
on:
  push:
    tags: ['v*']
    tags-ignore: ['v*-rc']
jobs:
  release:
    runs-on: ubuntu-latest
    steps:
      - run: echo release
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let push = workflow.on.push.as_ref().unwrap();
        assert_eq!(push.tags, vec!["v*".to_string()]);
        assert_eq!(push.tags_ignore, vec!["v*-rc".to_string()]);
        assert!(workflow.matches(&WorkflowEvent::Tag {
            tag: "v1.2.3".to_string()
        }));
        assert!(!workflow.matches(&WorkflowEvent::Tag {
            tag: "v2-rc".to_string()
        }));
        assert!(!workflow.matches(&WorkflowEvent::Tag {
            tag: "nightly".to_string()
        }));
        assert!(!workflow.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
        let bare = parse_workflow(
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        assert!(bare.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
        assert!(bare.matches(&WorkflowEvent::Tag {
            tag: "v9".to_string()
        }));
        let branches_only = parse_workflow("on:\n  push:\n    branches: [main]\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n")
            .unwrap();
        assert!(!branches_only.matches(&WorkflowEvent::Tag {
            tag: "v9".to_string()
        }));
    }

    #[test]
    fn matrix_expansion_fans_out_and_substitutes() {
        let workflow = parse_workflow(SAMPLE).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 3);
        let tests: Vec<&ExpandedJob> = expanded
            .iter()
            .filter(|j| j.job_id.starts_with("test"))
            .collect();
        assert_eq!(tests.len(), 2);
        assert!(tests
            .iter()
            .any(|j| j.job_config.steps[0].run.contains("stable")));
        assert!(tests
            .iter()
            .any(|j| j.job_config.steps[0].run.contains("beta")));
        for job in tests {
            assert_eq!(job.needs, vec!["build".to_string()]);
        }
    }

    #[test]
    fn matrix_needs_fan_in_to_all_parents() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [a, b]
    steps:
      - run: echo build
  test:
    needs: build
    runs-on: ubuntu-latest
    steps:
      - run: echo test
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let test = expanded.iter().find(|j| j.job_id == "test").unwrap();
        assert_eq!(test.needs.len(), 2);
        assert!(test.needs.contains(&"build-0".to_string()));
        assert!(test.needs.contains(&"build-1".to_string()));
    }

    #[test]
    fn ordering_respects_needs_and_rejects_cycles() {
        let workflow = parse_workflow(SAMPLE).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let ordered = order_jobs(&expanded).unwrap();
        let build_pos = ordered.iter().position(|j| j.job_id == "build").unwrap();
        for job in &ordered {
            if job.job_id.starts_with("test") {
                let pos = ordered.iter().position(|j| j.job_id == job.job_id).unwrap();
                assert!(pos > build_pos);
            }
        }
        let cyclic = r#"
on: push
jobs:
  a:
    needs: b
    runs-on: ubuntu-latest
    steps:
      - run: echo a
  b:
    needs: a
    runs-on: ubuntu-latest
    steps:
      - run: echo b
"#;
        let workflow = parse_workflow(cyclic).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert!(order_jobs(&expanded).is_err());
    }

    #[test]
    fn missing_need_is_an_error() {
        let yaml = r#"
on: push
jobs:
  a:
    needs: ghost
    runs-on: ubuntu-latest
    steps:
      - run: echo a
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert!(order_jobs(&expanded).is_err());
    }

    #[test]
    fn uses_steps_become_skippable_scripts() {
        let yaml = r#"
on: push
jobs:
  a:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: some/other@v1
      - run: echo done
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].job_config.steps.len(), 3);
        assert!(expanded[0].job_config.steps[0]
            .run
            .contains("already present"));
        assert!(expanded[0].job_config.steps[1]
            .run
            .contains("no gitforest runner"));
    }

    #[test]
    fn llm_uses_step_expands_to_inference_script() {
        let yaml = r#"
on: push
jobs:
  summarize:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        target: [api, cli]
    steps:
      - uses: gitforest/llm@v1
        with:
          model: tiny-llama
          prompt: summarize ${{ matrix.target }}
          max-tokens: "64"
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        for job in &expanded {
            assert_eq!(job.job_config.steps.len(), 1);
            let run = &job.job_config.steps[0].run;
            assert!(run.contains("gitforest llm run"), "{run}");
            assert!(run.contains("--model 'tiny-llama'"), "{run}");
            assert!(run.contains("--max-tokens 64"), "{run}");
            assert!(run.contains("$GF_OUTPUTS"), "{run}");
            assert!(!run.contains("matrix.target"), "{run}");
        }
        let prompts: Vec<&str> = expanded
            .iter()
            .map(|j| j.job_config.steps[0].run.as_str())
            .collect();
        assert!(prompts[0].contains("summarize api") || prompts[1].contains("summarize api"));
        assert!(prompts[0].contains("summarize cli") || prompts[1].contains("summarize cli"));
    }

    #[test]
    fn job_resources_flow_into_requirements() {
        let yaml = r#"
on: push
jobs:
  small:
    runs-on: ubuntu-latest
    cpu: 1
    ram-mb: 1024
    disk-mb: 2048
    steps:
      - run: echo tiny
  plain:
    runs-on: ubuntu-latest
    steps:
      - run: echo default
  altspell:
    runs-on: ubuntu-latest
    ram_mb: "512"
    steps:
      - run: echo alt
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 3);
        let req = |id: &str| {
            expanded
                .iter()
                .find(|j| j.job_id == id)
                .unwrap()
                .requirements
                .clone()
        };
        let small = req("small");
        assert_eq!(small.min_cpu_cores, 1);
        assert_eq!(small.min_ram_mb, 1024);
        assert_eq!(small.min_disk_mb, 2048);
        let plain = req("plain");
        assert_eq!(plain.min_cpu_cores, DEFAULT_JOB_CPU_CORES);
        assert_eq!(plain.min_ram_mb, DEFAULT_JOB_RAM_MB);
        assert_eq!(plain.min_disk_mb, DEFAULT_JOB_DISK_MB);
        let alt = req("altspell");
        assert_eq!(alt.min_ram_mb, 512);
        assert_eq!(alt.min_cpu_cores, DEFAULT_JOB_CPU_CORES);
    }

    #[test]
    fn matrix_expansion_carries_fail_fast_and_parent() {
        let yaml = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        rust: [stable, beta]
    steps:
      - run: echo x
  slow:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [linux]
      fail-fast: false
    steps:
      - run: echo y
  single:
    runs-on: ubuntu-latest
    steps:
      - run: echo z
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 4);
        for job in expanded
            .iter()
            .filter(|j| j.matrix_parent.as_deref() == Some("test"))
        {
            assert!(job.fail_fast);
        }
        assert_eq!(
            expanded
                .iter()
                .filter(|j| j.matrix_parent.as_deref() == Some("test"))
                .count(),
            2
        );
        let slow = expanded.iter().find(|j| j.job_id == "slow").unwrap();
        assert!(!slow.fail_fast);
        assert!(slow.matrix_parent.is_none());
        let single = expanded.iter().find(|j| j.job_id == "single").unwrap();
        assert!(single.fail_fast);
        assert!(single.matrix_parent.is_none());
    }

    #[test]
    fn job_artifacts_flow_into_job_config() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo x > app.bin
    artifacts:
      - app.bin
      - dist/
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(
            expanded[0].job_config.artifacts,
            vec!["app.bin".to_string(), "dist/".to_string()]
        );
    }

    #[test]
    fn bad_fail_fast_fails_parse() {
        let yaml = r#"
on: push
jobs:
  bad:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        x: [1]
      fail-fast: sometimes
    steps:
      - run: echo x
"#;
        assert!(parse_workflow(yaml).is_err());
    }

    #[test]
    fn bad_job_resources_fail_parse() {
        for (key, value) in [
            ("cpu", "0"),
            ("ram-mb", "0"),
            ("disk-mb", "-5"),
            ("cpu", "lots"),
        ] {
            let yaml = format!(
                "on: push\njobs:\n  bad:\n    runs-on: ubuntu-latest\n    {key}: {value}\n    steps:\n      - run: echo x\n"
            );
            assert!(parse_workflow(&yaml).is_err(), "{key}={value}");
        }
    }

    #[test]
    fn llm_uses_step_without_model_fails_fast() {
        let yaml = r#"
on: push
jobs:
  broken:
    runs-on: ubuntu-latest
    steps:
      - uses: gitforest/llm
        with:
          prompt: hello
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        let run = &expanded[0].job_config.steps[0].run;
        assert!(run.contains("exit 1"), "{run}");
    }

    #[test]
    fn gpu_required_flows_from_job_flag_labels_and_llm_steps() {
        let yaml = r#"
on: push
jobs:
  plain:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
  flagged:
    runs-on: ubuntu-latest
    gpu: true
    steps:
      - run: echo hi
  labelled:
    runs-on: [self-hosted, linux-gpu]
    steps:
      - run: echo hi
  infer:
    runs-on: ubuntu-latest
    steps:
      - uses: gitforest/llm@v1
        with:
          model: tiny
          prompt: hi
          gpu: true
  infer_cpu:
    runs-on: ubuntu-latest
    steps:
      - uses: gitforest/llm
        with:
          model: tiny
          prompt: hi
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let required = |id: &str| {
            expanded
                .iter()
                .find(|j| j.job_id == id)
                .unwrap()
                .requirements
                .gpu_required
        };
        assert!(!required("plain"));
        assert!(required("flagged"));
        assert!(required("labelled"));
        assert!(required("infer"));
        assert!(!required("infer_cpu"));
    }

    #[test]
    fn docker_image_defaults_follow_labels() {
        let yaml = r#"
on: push
jobs:
  rust:
    runs-on: rust-latest
    steps:
      - run: echo hi
  custom:
    runs-on: ubuntu-latest
    docker-image: my/image:1
    steps:
      - run: echo hi
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let rust = expanded.iter().find(|j| j.job_id == "rust").unwrap();
        let custom = expanded.iter().find(|j| j.job_id == "custom").unwrap();
        assert_eq!(rust.job_config.docker_image, "rust:1.75");
        assert_eq!(custom.job_config.docker_image, "my/image:1");
    }

    #[test]
    fn bad_workflows_fail_with_context() {
        assert!(parse_workflow("on: push\n").is_err());
        assert!(parse_workflow("on: push\njobs: {}\n").is_err());
        assert!(parse_workflow("on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n").is_err());
        assert!(parse_workflow(
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - name: empty\n"
        )
        .is_err());
    }

    #[test]
    fn workflow_files_cover_both_layouts() {
        let root = std::env::temp_dir().join(format!("gf_workflows_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::create_dir_all(root.join(".gitforest/workflows")).unwrap();
        std::fs::write(
            root.join(".github/workflows/ci.yml"),
            "on: push\njobs:\n  a:\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        std::fs::write(
            root.join(".gitforest/workflows/extra.yaml"),
            "on: push\njobs:\n  b:\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        std::fs::write(root.join(".github/workflows/README.md"), "not a workflow").unwrap();
        let files = workflow_files_in(&root);
        assert_eq!(files.len(), 2);
        let loaded = load_workflows(&root);
        assert_eq!(loaded.len(), 2);
        assert!(loaded.iter().all(|(_, r)| r.is_ok()));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn job_if_conditions_parse_and_flow_through() {
        let yaml = r#"
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
  cleanup:
    needs: build
    if: always()
    runs-on: ubuntu-latest
    steps:
      - run: echo cleanup
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 3);
        let notify = expanded.iter().find(|j| j.job_id == "notify").unwrap();
        let cleanup = expanded.iter().find(|j| j.job_id == "cleanup").unwrap();
        let build = expanded.iter().find(|j| j.job_id == "build").unwrap();
        assert_eq!(notify.run_if, crate::job::RunCondition::Failure);
        assert_eq!(cleanup.run_if, crate::job::RunCondition::Always);
        assert_eq!(build.run_if, crate::job::RunCondition::Success);
        let expr = "on: push\njobs:\n  a:\n    if: needs.build.result == 'success'\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
        let workflow = parse_workflow(expr).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(
            expanded[0].run_if,
            crate::job::RunCondition::Expr("needs.build.result == 'success'".to_string())
        );
        let bad = "on: push\njobs:\n  a:\n    if: 'foo ==='\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad).is_err());
    }

    #[test]
    fn matrix_include_adds_and_exclude_removes() {
        let yaml = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [a, b]
        rust: [stable, beta]
        exclude:
          - os: a
            rust: beta
        include:
          - os: c
            rust: nightly
    steps:
      - run: echo ${{ matrix.os }} ${{ matrix.rust }}
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 4);
        let runs: Vec<String> = expanded
            .iter()
            .map(|j| j.job_config.steps[0].run.clone())
            .collect();
        assert!(!runs.iter().any(|r| r.contains("a beta")));
        assert!(runs.iter().any(|r| r.contains("c nightly")));
        assert!(runs.iter().any(|r| r.contains("a stable")));
    }

    #[test]
    fn step_conditions_carry_into_expansion() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: make
      - run: notify
        if: failure()
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(
            expanded[0].job_config.steps[0].condition,
            crate::job::RunCondition::Success
        );
        assert_eq!(
            expanded[0].job_config.steps[1].condition,
            crate::job::RunCondition::Failure
        );
        let expr = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        if: inputs.env == 'prod'\n";
        let workflow = parse_workflow(expr).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(
            expanded[0].job_config.steps[0].condition,
            crate::job::RunCondition::Expr("inputs.env == 'prod'".to_string())
        );
        let bad = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        if: 'foo ==='\n";
        assert!(parse_workflow(bad).is_err());
        let cancelled = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        if: failure() || cancelled()\n";
        assert!(parse_workflow(cancelled).is_err());
    }

    #[test]
    fn services_parse_expand_and_validate() {
        let yaml = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    services:
      postgres:
        image: postgres:16
        env:
          POSTGRES_PASSWORD: example
        ports:
          - 5432
          - "127.0.0.1:5433:5432"
        options: >-
          --health-cmd "pg_isready -U postgres"
          --health-interval 5s
          --health-timeout 3s
          --health-retries 5
    steps:
      - run: echo hi
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        let services = &expanded[0].job_config.services;
        assert_eq!(services.len(), 1);
        assert_eq!(services[0].name, "postgres");
        assert_eq!(services[0].image, "postgres:16");
        assert_eq!(services[0].env.get("POSTGRES_PASSWORD").unwrap(), "example");
        assert_eq!(services[0].ports.len(), 2);
        assert_eq!(services[0].ports[0].container, 5432);
        assert_eq!(services[0].ports[1].host, Some(5433));
        let health = services[0].health.clone().unwrap();
        assert_eq!(health.cmd, "pg_isready -U postgres");
        assert_eq!(health.interval_secs, 5);
        assert_eq!(health.timeout_secs, 3);
        assert_eq!(health.retries, 5);
        let no_image = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    services:\n      db:\n        ports: ['5432']\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(no_image).is_err());
        let bad_port = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    services:\n      db:\n        image: x:1\n        ports: ['abc']\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad_port).is_err());
    }

    #[test]
    fn container_parse_expand_and_validate() {
        let yaml = r#"
on: push
jobs:
  test:
    runs-on: ubuntu-latest
    container:
      image: node:20
      credentials:
        username: octo
        password: s3cret
      env:
        NODE_ENV: test
      ports:
        - 8080
        - "18081:8080"
      volumes:
        - www:/usr/share/nginx/html:ro
        - /data/scratch
      options: --cpus 2 --memory 512m --shm-size 64m
    steps:
      - run: echo hi
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        let config = &expanded[0].job_config;
        assert_eq!(config.docker_image, "node:20");
        let c = config.container.as_ref().unwrap();
        assert_eq!(c.image, "node:20");
        assert_eq!(c.credentials.as_ref().unwrap().username, "octo");
        assert_eq!(c.env.get("NODE_ENV").unwrap(), "test");
        assert_eq!(c.ports.len(), 2);
        assert_eq!(c.ports[0].container, 8080);
        assert_eq!(c.ports[1].container, 8080);
        assert_eq!(c.ports[1].host, Some(18081));
        assert_eq!(c.volumes.len(), 2);
        assert_eq!(c.volumes[0].source.as_deref(), Some("www"));
        assert_eq!(c.volumes[0].target, "/usr/share/nginx/html");
        assert!(c.volumes[0].read_only);
        assert_eq!(c.volumes[1].source, None);
        assert_eq!(c.resources.nano_cpus, Some(2_000_000_000));
        assert_eq!(c.resources.memory_bytes, Some(512 * 1024 * 1024));
        assert_eq!(c.resources.shm_bytes, Some(64 * 1024 * 1024));
        let short = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container: node:20\n    steps:\n      - run: echo hi\n";
        let expanded = expand_workflow(&parse_workflow(short).unwrap()).unwrap();
        assert_eq!(expanded[0].job_config.docker_image, "node:20");
        assert!(expanded[0]
            .job_config
            .container
            .as_ref()
            .unwrap()
            .ports
            .is_empty());
        let no_image = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      env: {A: b}\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(no_image).is_err());
        let host_path = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      volumes: ['/etc:/evil:ro']\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(host_path).is_err());
        let dotdot = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      volumes: ['data:/x/../y']\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(dotdot).is_err());
        let privileged = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      options: --privileged\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(privileged).is_err());
        let net_opt = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      options: --network host\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(net_opt).is_err());
        let bad_cpus = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      options: --cpus banana\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad_cpus).is_err());
        let unknown_flag = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    container:\n      image: x:1\n      options: --log-driver json-file\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(unknown_flag).is_ok());
    }

    #[test]
    fn step_ids_and_outputs_map_flow_through() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    outputs:
      digest: ${{ steps.emit.outputs.SHA }}
    steps:
      - id: emit
        run: echo "SHA=abc" >> "$GF_OUTPUTS"
      - run: echo next
        if: steps.emit.outcome == 'success'
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded[0].job_config.steps[0].id, Some("emit".to_string()));
        assert_eq!(
            expanded[0].job_config.outputs.get("digest").unwrap(),
            "${{ steps.emit.outputs.SHA }}"
        );
        let dup = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - id: x\n        run: echo hi\n      - id: x\n        run: echo yo\n";
        assert!(parse_workflow(dup).is_err());
        let ghost = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - id: x\n        run: echo hi\n      - run: echo yo\n        if: steps.ghost.outcome == 'success'\n";
        assert!(parse_workflow(ghost).is_err());
        let bad_id = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - id: '9 bad'\n        run: echo hi\n";
        assert!(parse_workflow(bad_id).is_err());
    }

    #[test]
    fn step_working_directory_parses_and_expands() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: make
        working-directory: crates/foo
      - run: make
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(
            expanded[0].job_config.steps[0].working_directory,
            Some("crates/foo".to_string())
        );
        assert_eq!(expanded[0].job_config.steps[1].working_directory, None);
        for bad_dir in ["/abs", "../escape", "a/../../b", "\"\""] {
            let bad = format!(
                "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        working-directory: {bad_dir}\n"
            );
            assert!(
                parse_workflow(&bad).is_err(),
                "working-directory {bad_dir:?} must be rejected"
            );
        }
    }

    #[test]
    fn job_defaults_run_working_directory_applies_unless_overridden() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: crates/foo
    steps:
      - run: make
      - run: make
        working-directory: crates/bar
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(
            expanded[0].job_config.steps[0].working_directory,
            Some("crates/foo".to_string())
        );
        assert_eq!(
            expanded[0].job_config.steps[1].working_directory,
            Some("crates/bar".to_string())
        );
        let bad = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    defaults:\n      run:\n        working-directory: /abs\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad).is_err());
        let bad_shape = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    defaults:\n      run: crates/foo\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad_shape).is_err());
    }

    #[test]
    fn workflow_defaults_apply_below_job_and_step() {
        let yaml = r#"
on: push
defaults:
  run:
    working-directory: crates/root
jobs:
  plain:
    runs-on: ubuntu-latest
    steps:
      - run: make
  scoped:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: crates/job
    steps:
      - run: make
      - run: make
        working-directory: crates/step
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let get = |id: &str, step: usize| {
            expanded
                .iter()
                .find(|j| j.job_id == id)
                .unwrap()
                .job_config
                .steps[step]
                .working_directory
                .clone()
        };
        assert_eq!(get("plain", 0), Some("crates/root".to_string()));
        assert_eq!(get("scoped", 0), Some("crates/job".to_string()));
        assert_eq!(get("scoped", 1), Some("crates/step".to_string()));
        let bad = "on: push\ndefaults:\n  run:\n    working-directory: /abs\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(bad).is_err());
    }

    #[test]
    fn working_directory_substitutes_matrix_and_inputs() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [a, b]
    defaults:
      run:
        working-directory: out/${{ matrix.os }}
    steps:
      - run: make
      - run: make
        working-directory: custom/${{ matrix.os }}
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        for job in &expanded {
            let os = job.matrix.get("os").unwrap();
            assert_eq!(
                job.job_config.steps[0].working_directory,
                Some(format!("out/{os}"))
            );
            assert_eq!(
                job.job_config.steps[1].working_directory,
                Some(format!("custom/{os}"))
            );
        }
        let reusable = r#"
name: Reusable
on:
  workflow_call:
    inputs:
      dir:
        required: true
jobs:
  greet:
    runs-on: ubuntu-latest
    defaults:
      run:
        working-directory: ${{ inputs.dir }}/shared
    steps:
      - run: echo hi
        working-directory: ${{ inputs.dir }}/step
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: ./reusable.yml
    with:
      dir: crates/foo
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", reusable)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        let greet = expanded.iter().find(|j| j.job_id == "hello/greet").unwrap();
        assert_eq!(
            greet.job_config.steps[0].working_directory,
            Some("crates/foo/step".to_string())
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cancelled_condition_is_job_level_only() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo build
  janitor:
    needs: build
    if: cancelled()
    runs-on: ubuntu-latest
    steps:
      - run: echo janitor
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let janitor = expanded.iter().find(|j| j.job_id == "janitor").unwrap();
        assert_eq!(janitor.run_if, crate::job::RunCondition::Cancelled);
        let bad = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n        if: cancelled()\n";
        assert!(parse_workflow(bad).is_err());
    }

    #[test]
    fn secrets_substitute_and_missing_blank() {
        let yaml = r#"
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    secrets:
      TOKEN: hunter2-hunter2
    steps:
      - run: deploy ${{ secrets.TOKEN }} ${{secrets.MISSING}} ${{ matrix.os }}
    strategy:
      matrix:
        os: [linux]
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        let config = &expanded[0].job_config;
        assert_eq!(
            config.steps[0].run,
            "deploy hunter2-hunter2  linux".to_string()
        );
        assert_eq!(config.secrets.get("TOKEN").unwrap(), "hunter2-hunter2");
        let untouched = substitute_secrets("keep ${{ matrix.x }} safe", &config.secrets);
        assert_eq!(untouched, "keep ${{ matrix.x }} safe");
        let unclosed = substitute_secrets("keep ${{ secrets.TOKEN safe", &config.secrets);
        assert_eq!(unclosed, "keep ${{ secrets.TOKEN safe");
    }

    fn reuse_dir(files: &[(&str, &str)]) -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("gf_reuse_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        for (name, text) in files {
            std::fs::write(root.join(name), text).unwrap();
        }
        root
    }

    const REUSABLE: &str = r#"
name: Reusable
on:
  workflow_call:
    inputs:
      greeting:
        required: true
      punctuation:
        default: "!"
    secrets:
      TOKEN:
        required: true
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.greeting }}${{ inputs.punctuation }} ${{ secrets.TOKEN }}
"#;

    #[test]
    fn reusable_call_inlines_with_inputs() {
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: ./reusable.yml
    with:
      greeting: hi
    secrets:
      TOKEN: hunter2
  after:
    needs: hello
    runs-on: ubuntu-latest
    steps:
      - run: echo done
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", REUSABLE)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        let greet = expanded.iter().find(|j| j.job_id == "hello/greet").unwrap();
        assert!(greet.job_config.steps[0].run.contains("echo hi! hunter2"));
        assert_eq!(greet.job_config.secrets.get("TOKEN").unwrap(), "hunter2");
        let after = expanded.iter().find(|j| j.job_id == "after").unwrap();
        assert_eq!(after.needs, vec!["hello/greet".to_string()]);
        let ordered = order_jobs(&expanded).unwrap();
        assert_eq!(ordered[0].job_id, "hello/greet");
        assert_eq!(ordered[1].job_id, "after");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_permissions_narrow_through_grant() {
        let caller = r#"
name: Caller
on: push
permissions:
  contents: read
  workflows: read
jobs:
  hello:
    uses: ./reusable-perms.yml
    permissions:
      contents: read
  wide:
    uses: ./reusable-perms.yml
    permissions: inherit
"#;
        let callee = r#"
name: Reusable
on:
  workflow_call:
permissions:
  contents: write
jobs:
  greet:
    runs-on: ubuntu-latest
    permissions:
      workflows: write
    steps:
      - run: echo hi
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable-perms.yml", callee)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        let narrow = expanded.iter().find(|j| j.job_id == "hello/greet").unwrap();
        assert!(narrow
            .permissions
            .allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!narrow
            .permissions
            .allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(!narrow
            .permissions
            .allows(PermissionScope::Workflows, PermissionLevel::Read));
        let wide = expanded.iter().find(|j| j.job_id == "wide/greet").unwrap();
        assert!(wide
            .permissions
            .allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!wide
            .permissions
            .allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(wide
            .permissions
            .allows(PermissionScope::Workflows, PermissionLevel::Read));
        assert!(!wide
            .permissions
            .allows(PermissionScope::Workflows, PermissionLevel::Write));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn permissions_parse_effective_and_narrow() {
        let wf = parse_workflow(
            "name: P\non: push\npermissions:\n  contents: read\n  workflows: write\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n  b:\n    runs-on: ubuntu-latest\n    permissions: read-all\n    steps:\n      - run: echo hi\n",
        )
        .unwrap();
        let a = effective_job_permissions(&wf, &wf.jobs["a"], None).unwrap();
        assert!(a.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!a.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(a.allows(PermissionScope::Workflows, PermissionLevel::Write));
        let b = effective_job_permissions(&wf, &wf.jobs["b"], None).unwrap();
        assert!(b.allows(PermissionScope::Models, PermissionLevel::Read));
        assert!(!b.allows(PermissionScope::Workflows, PermissionLevel::Write));
        let capped = effective_job_permissions(
            &wf,
            &wf.jobs["a"],
            Some(
                &PermissionsDecl::Map(HashMap::from([(
                    "workflows".to_string(),
                    "read".to_string(),
                )]))
                .resolve()
                .unwrap(),
            ),
        )
        .unwrap();
        assert!(!capped.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(capped.allows(PermissionScope::Workflows, PermissionLevel::Read));
        assert!(!capped.allows(PermissionScope::Workflows, PermissionLevel::Write));
    }

    #[test]
    fn permissions_reject_unknown_and_misplaced_inherit() {
        assert!(parse_workflow(
            "name: P\non: push\npermissions:\n  bogus: read\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
        )
        .is_err());
        assert!(parse_workflow(
            "name: P\non: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    permissions:\n      contents: admin\n    steps:\n      - run: echo hi\n"
        )
        .is_err());
        assert!(parse_workflow(
            "name: P\non: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    permissions: inherit\n    steps:\n      - run: echo hi\n"
        )
        .is_err());
        assert!(parse_workflow(
            "name: P\non: push\npermissions: inherit\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n"
        )
        .is_err());
    }

    #[test]
    fn call_grant_passes_or_narrows() {
        let caller = Permissions::all(PermissionLevel::Write);
        let pass = call_grant(&caller, &PermissionsDecl::default()).unwrap();
        assert!(pass.allows(PermissionScope::Contents, PermissionLevel::Write));
        let inherit =
            call_grant(&caller, &PermissionsDecl::Shorthand("inherit".to_string())).unwrap();
        assert!(inherit.allows(PermissionScope::Models, PermissionLevel::Write));
        let narrow = call_grant(
            &caller,
            &PermissionsDecl::Map(HashMap::from([(
                "contents".to_string(),
                "read".to_string(),
            )])),
        )
        .unwrap();
        assert!(narrow.allows(PermissionScope::Contents, PermissionLevel::Read));
        assert!(!narrow.allows(PermissionScope::Contents, PermissionLevel::Write));
        assert!(!narrow.allows(PermissionScope::Workflows, PermissionLevel::Read));
    }

    #[test]
    fn uses_target_shapes() {
        assert_eq!(
            parse_uses_target("./reusable.yml").unwrap(),
            UsesTarget::Local("./reusable.yml".to_string())
        );
        assert_eq!(
            parse_uses_target("../shared/x.yml").unwrap(),
            UsesTarget::Local("../shared/x.yml".to_string())
        );
        assert_eq!(
            parse_uses_target("shared/.github/workflows/build.yml@abc123").unwrap(),
            UsesTarget::Remote {
                repo: "shared".to_string(),
                path: ".github/workflows/build.yml".to_string(),
                git_ref: "abc123".to_string(),
            }
        );
        assert!(parse_uses_target("shared/.github/workflows/build.yml@feature/x").is_ok());
        assert_eq!(
            parse_uses_target("shared/.github/workflows/build.yml").unwrap(),
            UsesTarget::Local("shared/.github/workflows/build.yml".to_string())
        );
        assert_eq!(
            parse_uses_target("/abs/path.yml").unwrap(),
            UsesTarget::Local("/abs/path.yml".to_string())
        );
        assert_eq!(
            parse_uses_target("shared/.github/actions/echo/action.yml@v1").unwrap(),
            UsesTarget::Remote {
                repo: "shared".to_string(),
                path: ".github/actions/echo/action.yml".to_string(),
                git_ref: "v1".to_string(),
            }
        );
        for bad in [
            "",
            "shared/.github/workflows/build.txt@abc123",
            "shared/.github/workflows/../evil.yml@abc123",
            "shared/.github/workflows/build.yml@",
            "shared/.github/workflows/build.yml@-x",
            "shared/.github/workflows/build.yml@a b",
            "shared/.github/workflows/build.yml@x@{y}",
            "actions/checkout@v4",
        ] {
            assert!(parse_uses_target(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn reusable_call_inherits_and_resolves_secrets() {
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: ./reusable.yml
    with:
      greeting: hi
    secrets: inherit
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", REUSABLE)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(workflow.jobs["hello"].secrets_inherit);
        let extras = HashMap::from([("TOKEN".to_string(), "inh3rited".to_string())]);
        let expanded = resolve_and_expand_with_secrets(&workflow, &extras, None).unwrap();
        let greet = expanded.iter().find(|j| j.job_id == "hello/greet").unwrap();
        assert!(greet.job_config.steps[0].run.contains("inh3rited"));
        assert_eq!(greet.job_config.secrets.get("TOKEN").unwrap(), "inh3rited");
        let explicit = r#"
name: Caller
on: push
jobs:
  hello:
    uses: ./reusable.yml
    with:
      greeting: hi
    secrets:
      TOKEN: ${{ secrets.REAL }}
"#;
        std::fs::write(root.join("caller.yml"), explicit).unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let extras = HashMap::from([("REAL".to_string(), "mapped".to_string())]);
        let expanded = resolve_and_expand_with_secrets(&workflow, &extras, None).unwrap();
        let greet = expanded.iter().find(|j| j.job_id == "hello/greet").unwrap();
        assert!(greet.job_config.steps[0].run.contains("mapped"));
        let garbage =
            "name: C\non: push\njobs:\n  h:\n    uses: ./reusable.yml\n    secrets: sometime\n";
        assert!(parse_workflow(garbage).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_needs_fan_out_to_inner_jobs() {
        let reusable = r#"
name: Reusable
on:
  workflow_call:
    outputs:
      digest: ${{ jobs.greet.outputs.digest }}
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
  other:
    runs-on: ubuntu-latest
    steps:
      - run: echo yo
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    uses: ./reusable.yml
  consume:
    needs: [hello]
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ needs.hello.outputs.digest }}
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", reusable)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        let ordered = order_jobs(&expanded).unwrap();
        assert_eq!(ordered.len(), 3);
        let consume = ordered.iter().find(|j| j.job_id == "consume").unwrap();
        assert_eq!(
            consume.needs,
            vec!["hello/greet".to_string(), "hello/other".to_string()]
        );
        for id in ["hello/greet", "hello/other"] {
            let inner = ordered.iter().find(|j| j.job_id == id).unwrap();
            assert_eq!(inner.call_id.as_deref(), Some("hello"));
            assert_eq!(
                inner.call_outputs.get("digest").unwrap(),
                "${{ jobs.greet.outputs.digest }}"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_output_refs_must_resolve() {
        let good = parse_call_output_ref("${{ jobs.greet.outputs.digest }}").unwrap();
        assert_eq!(good, ("greet".to_string(), "digest".to_string()));
        assert!(parse_call_output_ref("${{jobs.greet.outputs.SHA}}").is_some());
        for bad in [
            "plain",
            "${{ jobs.greet.outputs }}",
            "${{ jobs.greet.outputs.a.b }}",
            "${{ steps.x.outputs.a }}",
            "${{ jobs..outputs.a }}",
        ] {
            assert!(parse_call_output_ref(bad).is_none(), "{bad}");
        }
        let reusable = r#"
name: Reusable
on:
  workflow_call:
    outputs:
      digest: ${{ jobs.ghost.outputs.SHA }}
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo hi
"#;
        let caller = "name: C\non: push\njobs:\n  h:\n    uses: ./reusable.yml\n";
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", reusable)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn composite_steps_inline_with_inputs() {
        let action = r#"
name: Prefix
inputs:
  text:
    required: true
  punct:
    default: "!"
runs:
  using: composite
  steps:
    - run: echo "${{ inputs.text }}${{ inputs.punct }}"
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./action.yml
        with:
          text: hi
      - run: echo done
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("action.yml", action)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        let steps = &expanded[0].job_config.steps;
        assert_eq!(steps.len(), 2);
        assert!(steps[0].run.contains("echo \"hi!\""), "{}", steps[0].run);
        assert!(steps[1].run.contains("echo done"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn composite_steps_validate_contract() {
        let action = r#"
name: Prefix
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "${{ inputs.text }}"
"#;
        for (name, caller) in [
            (
                "missing",
                "name: C\non: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./action.yml\n",
            ),
            (
                "undeclared",
                "name: C\non: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./action.yml\n        with:\n          text: hi\n          bogus: x\n",
            ),
            (
                "run-and-uses",
                "name: C\non: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo x\n        uses: ./action.yml\n        with:\n          text: hi\n",
            ),
            (
                "gone",
                "name: C\non: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./nope.yml\n",
            ),
        ] {
            let root = reuse_dir(&[("caller.yml", caller), ("action.yml", action)]);
            let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
            assert!(resolve_and_expand(&workflow).is_err(), "{name}");
            let _ = std::fs::remove_dir_all(&root);
        }
        let root = reuse_dir(&[(
            "caller.yml",
            "name: C\non: push\njobs:\n  b:\n    runs-on: ubuntu-latest\n    steps:\n      - uses: ./action.yml\n        with:\n          text: hi\n",
        )]);
        std::fs::write(
            root.join("action.yml"),
            "name: X\nruns:\n  using: docker\n  steps:\n    - run: echo x\n",
        )
        .unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn composite_steps_nest_and_inherit_gate() {
        let root = reuse_dir(&[(
            "caller.yml",
            r#"
name: Caller
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - uses: ./outer.yml
        if: github.event_name == 'push'
"#,
        )]);
        std::fs::write(
            root.join("outer.yml"),
            r#"
name: Outer
runs:
  using: composite
  steps:
    - uses: ./inner.yml
      with:
        text: deep
"#,
        )
        .unwrap();
        std::fs::write(
            root.join("inner.yml"),
            r#"
name: Inner
inputs:
  text:
    required: true
runs:
  using: composite
  steps:
    - run: echo "${{ inputs.text }}"
"#,
        )
        .unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        let steps = &expanded[0].job_config.steps;
        assert_eq!(steps.len(), 1);
        assert!(steps[0].run.contains("deep"), "{}", steps[0].run);
        assert!(matches!(
            steps[0].condition,
            crate::job::RunCondition::Expr(_)
        ));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_matrix_fans_out_with_inputs() {
        let reusable = r#"
name: Reusable
on:
  workflow_call:
    inputs:
      version:
        required: true
jobs:
  greet:
    runs-on: ubuntu-latest
    steps:
      - run: echo "${{ inputs.version }}"
"#;
        let caller = r#"
name: Caller
on: push
jobs:
  hello:
    strategy:
      matrix:
        py: [3.11, 3.12]
    uses: ./reusable.yml
    with:
      version: py-${{ matrix.py }}
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("reusable.yml", reusable)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        let mut runs: Vec<String> = expanded
            .iter()
            .map(|j| j.job_config.steps[0].run.clone())
            .collect();
        runs.sort();
        assert_eq!(runs, vec!["echo \"py-3.11\"", "echo \"py-3.12\""]);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_validates_contract() {
        let no_input = "name: C\non: push\njobs:\n  h:\n    uses: ./reusable.yml\n    secrets:\n      TOKEN: x\n";
        let root = reuse_dir(&[("caller.yml", no_input), ("reusable.yml", REUSABLE)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let extra_input = "name: C\non: push\njobs:\n  h:\n    uses: ./reusable.yml\n    with:\n      greeting: hi\n      bogus: 1\n    secrets:\n      TOKEN: x\n";
        std::fs::write(root.join("caller.yml"), extra_input).unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let no_secret = "name: C\non: push\njobs:\n  h:\n    uses: ./reusable.yml\n    with:\n      greeting: hi\n";
        std::fs::write(root.join("caller.yml"), no_secret).unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_call_rejects_bad_targets() {
        let plain = "name: P\non: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo hi\n";
        let caller = "name: C\non: push\njobs:\n  h:\n    uses: ./plain.yml\n";
        let root = reuse_dir(&[("caller.yml", caller), ("plain.yml", plain)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let missing = "name: C\non: push\njobs:\n  h:\n    uses: ./ghost.yml\n";
        std::fs::write(root.join("caller.yml"), missing).unwrap();
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let remote = "name: C\non: push\njobs:\n  h:\n    uses: other/repo/path.txt@v1\n";
        assert!(parse_workflow(remote).is_err());
        let both = "name: C\non: push\njobs:\n  h:\n    uses: ./plain.yml\n    steps:\n      - run: echo hi\n";
        assert!(parse_workflow(both).is_err());
        let with_runs =
            "name: C\non: push\njobs:\n  h:\n    uses: ./plain.yml\n    runs-on: ubuntu-latest\n";
        assert!(parse_workflow(with_runs).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_cycles_and_nesting() {
        let inner = r#"
name: Inner
on:
  workflow_call:
    inputs:
      word:
        required: true
jobs:
  say:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.word }}
"#;
        let middle = r#"
name: Middle
on:
  workflow_call:
    inputs:
      word:
        required: true
jobs:
  inner:
    uses: ./inner.yml
    with:
      word: ${{ inputs.word }}
"#;
        let outer = r#"
name: Outer
on: push
jobs:
  mid:
    uses: ./middle.yml
    with:
      word: deep
"#;
        let root = reuse_dir(&[
            ("outer.yml", outer),
            ("middle.yml", middle),
            ("inner.yml", inner),
        ]);
        let workflow = parse_workflow_file(&root.join("outer.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].job_id, "mid/inner/say");
        assert!(expanded[0].job_config.steps[0].run.contains("echo deep"));
        let cyclic_a =
            "name: A\non:\n  workflow_call:\n    inputs: {}\njobs:\n  b:\n    uses: ./b.yml\n";
        let cyclic_b =
            "name: B\non:\n  workflow_call:\n    inputs: {}\njobs:\n  a:\n    uses: ./a.yml\n";
        let root2 = reuse_dir(&[("a.yml", cyclic_a), ("b.yml", cyclic_b)]);
        let workflow = parse_workflow_file(&root2.join("a.yml")).unwrap();
        assert!(resolve_and_expand(&workflow).is_err());
        let _ = std::fs::remove_dir_all(&root);
        let _ = std::fs::remove_dir_all(&root2);
    }

    #[test]
    fn reusable_call_fans_out_caller_matrix() {
        let target = r#"
name: T
on:
  workflow_call:
    inputs:
      flavor:
        required: false
jobs:
  build:
    runs-on: ubuntu-latest
    steps:
      - run: echo ${{ inputs.flavor }}
"#;
        let caller = r#"
name: C
on: push
jobs:
  build:
    uses: ./target.yml
    with:
      flavor: ${{ matrix.os }}
    strategy:
      matrix:
        os: [a, b]
"#;
        let root = reuse_dir(&[("caller.yml", caller), ("target.yml", target)]);
        let workflow = parse_workflow_file(&root.join("caller.yml")).unwrap();
        let expanded = resolve_and_expand(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        let runs: Vec<String> = expanded
            .iter()
            .map(|j| j.job_config.steps[0].run.clone())
            .collect();
        assert!(runs.iter().any(|r| r.contains("echo a")));
        assert!(runs.iter().any(|r| r.contains("echo b")));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn reusable_files_match_no_event() {
        let workflow = parse_workflow(REUSABLE).unwrap();
        assert!(workflow.on.workflow_call.is_some());
        assert!(!workflow.matches(&WorkflowEvent::WorkflowDispatch {
            inputs: HashMap::new()
        }));
        assert!(!workflow.matches(&WorkflowEvent::Push {
            branch: "main".to_string()
        }));
    }

    #[test]
    fn extra_secrets_merge_and_win() {
        let yaml = r#"
on: push
jobs:
  deploy:
    runs-on: ubuntu-latest
    secrets:
      TOKEN: yaml-value
    steps:
      - run: deploy ${{ secrets.TOKEN }} ${{ secrets.STORE_ONLY }}
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let extra = HashMap::from([
            ("TOKEN".to_string(), "store-value".to_string()),
            ("STORE_ONLY".to_string(), "from-store".to_string()),
        ]);
        let expanded = resolve_and_expand_with_secrets(&workflow, &extra, None).unwrap();
        assert_eq!(expanded.len(), 1);
        let config = &expanded[0].job_config;
        assert_eq!(
            config.steps[0].run,
            "deploy store-value from-store".to_string()
        );
        assert_eq!(config.secrets.get("TOKEN").unwrap(), "store-value");
        assert_eq!(config.secrets.get("STORE_ONLY").unwrap(), "from-store");
    }

    #[test]
    fn concurrency_simple_and_full_forms_parse() {
        let yaml = r#"
on: push
jobs:
  plain:
    runs-on: ubuntu-latest
    concurrency: deploy
    steps:
      - run: echo plain
  full:
    runs-on: ubuntu-latest
    concurrency:
      group: release
      cancel-in-progress: true
    steps:
      - run: echo full
"#;
        let workflow = parse_workflow(yaml).unwrap();
        assert_eq!(
            workflow.jobs["plain"].concurrency.as_ref().unwrap().group(),
            "deploy"
        );
        assert!(!workflow.jobs["plain"]
            .concurrency
            .as_ref()
            .unwrap()
            .cancel_in_progress());
        assert_eq!(
            workflow.jobs["full"].concurrency.as_ref().unwrap().group(),
            "release"
        );
        assert!(workflow.jobs["full"]
            .concurrency
            .as_ref()
            .unwrap()
            .cancel_in_progress());
        let expanded = expand_workflow(&workflow).unwrap();
        let plain = expanded.iter().find(|j| j.job_id == "plain").unwrap();
        assert_eq!(plain.concurrency_group, "deploy");
        assert!(!plain.cancel_in_progress);
        let full = expanded.iter().find(|j| j.job_id == "full").unwrap();
        assert_eq!(full.concurrency_group, "release");
        assert!(full.cancel_in_progress);
    }

    #[test]
    fn concurrency_workflow_default_fills_matrix_and_job_wins() {
        let yaml = r#"
on: push
concurrency: ci-${{ matrix.os }}
jobs:
  build:
    runs-on: ubuntu-latest
    strategy:
      matrix:
        os: [linux, macos]
    steps:
      - run: echo build
  lint:
    runs-on: ubuntu-latest
    concurrency: lint-fixed
    steps:
      - run: echo lint
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded = expand_workflow(&workflow).unwrap();
        let mut groups: Vec<String> = expanded
            .iter()
            .filter(|j| j.job_id.starts_with("build"))
            .map(|j| j.concurrency_group.clone())
            .collect();
        groups.sort();
        assert_eq!(groups, vec!["ci-linux".to_string(), "ci-macos".to_string()]);
        let lint = expanded.iter().find(|j| j.job_id == "lint").unwrap();
        assert_eq!(lint.concurrency_group, "lint-fixed");
    }

    #[test]
    fn concurrency_bad_shapes_rejected() {
        let missing_group = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    concurrency:\n      cancel-in-progress: true\n    steps:\n      - run: echo a\n";
        assert!(parse_workflow(missing_group).is_err());
        let bad_flag = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    concurrency:\n      group: g\n      cancel-in-progress: sometimes\n    steps:\n      - run: echo a\n";
        assert!(parse_workflow(bad_flag).is_err());
    }

    #[test]
    fn concurrency_group_takes_inputs_then_github() {
        let yaml = r#"
on:
  workflow_dispatch:
    inputs:
      env:
        default: staging
jobs:
  deploy:
    runs-on: ubuntu-latest
    concurrency: ${{ inputs.env }}-${{ github.ref_name }}
    steps:
      - run: echo deploy
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let provided = HashMap::new();
        let expanded =
            resolve_and_expand_with_secrets(&workflow, &HashMap::new(), Some(&provided)).unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(
            expanded[0].concurrency_group,
            "staging-${{ github.ref_name }}"
        );
        let github = HashMap::from([("ref_name".to_string(), "main".to_string())]);
        assert_eq!(
            substitute_github(&expanded[0].concurrency_group, &github),
            "staging-main"
        );
    }

    #[test]
    fn substitute_github_blanks_unknown_keys() {
        let github = HashMap::from([("ref".to_string(), "refs/heads/main".to_string())]);
        assert_eq!(
            substitute_github("a-${{ github.sha }}-${{github.ref}}", &github),
            "a--refs/heads/main"
        );
    }

    #[test]
    fn environment_string_and_mapping_parse_with_matrix() {
        let yaml = r#"
on: push
jobs:
  plain:
    runs-on: ubuntu-latest
    environment: staging
    steps:
      - run: echo plain
  full:
    runs-on: ubuntu-latest
    environment:
      name: prod-${{ matrix.region }}
      url: https://example.com/${{ matrix.region }}
    strategy:
      matrix:
        region: [east]
    steps:
      - run: echo full
"#;
        let workflow = parse_workflow(yaml).unwrap();
        assert_eq!(
            workflow.jobs["plain"].environment.as_ref().unwrap().name(),
            "staging"
        );
        assert_eq!(
            workflow.jobs["plain"].environment.as_ref().unwrap().url(),
            ""
        );
        let expanded = expand_workflow(&workflow).unwrap();
        let plain = expanded.iter().find(|j| j.job_id == "plain").unwrap();
        assert_eq!(plain.environment, "staging");
        assert_eq!(plain.environment_url, "");
        let full = expanded.iter().find(|j| j.job_id == "full").unwrap();
        assert_eq!(full.environment, "prod-east");
        assert_eq!(full.environment_url, "https://example.com/east");
    }

    #[test]
    fn environment_mapping_needs_a_name() {
        let yaml = "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    environment:\n      url: https://example.com\n    steps:\n      - run: echo a\n";
        assert!(parse_workflow(yaml).is_err());
    }

    #[test]
    fn environment_takes_dispatch_inputs() {
        let yaml = r#"
on:
  workflow_dispatch:
    inputs:
      target:
        default: staging
jobs:
  deploy:
    runs-on: ubuntu-latest
    environment: ${{ inputs.target }}
    steps:
      - run: echo deploy
"#;
        let workflow = parse_workflow(yaml).unwrap();
        let expanded =
            resolve_and_expand_with_secrets(&workflow, &HashMap::new(), Some(&HashMap::new()))
                .unwrap();
        assert_eq!(expanded.len(), 1);
        assert_eq!(expanded[0].environment, "staging");
    }

    #[test]
    fn continue_on_error_and_max_parallel_expand() {
        let yaml = r#"
on: push
jobs:
  build:
    runs-on: ubuntu-latest
    continue-on-error: true
    strategy:
      max-parallel: 2
      matrix:
        os: [linux, macos]
    steps:
      - run: echo build
      - run: echo flaky
        continue-on-error: "true"
"#;
        let workflow = parse_workflow(yaml).unwrap();
        assert!(workflow.jobs["build"].continue_on_error);
        assert_eq!(workflow.jobs["build"].strategy.max_parallel, Some(2));
        assert!(!workflow.jobs["build"].steps[0].continue_on_error);
        assert!(workflow.jobs["build"].steps[1].continue_on_error);
        let expanded = expand_workflow(&workflow).unwrap();
        assert_eq!(expanded.len(), 2);
        for job in &expanded {
            assert!(job.continue_on_error);
            assert_eq!(job.max_parallel, 2);
            assert!(job.job_config.steps[1].continue_on_error);
        }
    }

    #[test]
    fn max_parallel_rejects_non_positive() {
        for value in ["0", "-1", "many"] {
            let yaml = format!(
                "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    strategy:\n      max-parallel: {value}\n      matrix:\n        os: [linux]\n    steps:\n      - run: echo a\n"
            );
            assert!(parse_workflow(&yaml).is_err(), "value {value}");
        }
        let yaml =
            "on: push\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: echo a\n";
        let workflow = parse_workflow(yaml).unwrap();
        assert_eq!(workflow.jobs["a"].strategy.max_parallel, None);
        assert!(!workflow.jobs["a"].continue_on_error);
    }
}
