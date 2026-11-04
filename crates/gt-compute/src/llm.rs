use anyhow::{bail, Context, Result};
use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;

pub const DEFAULT_BACKEND: &str = "llama-cli";
pub const BACKEND_ENV_VAR: &str = "GF_LLM_BACKEND";
pub const MAX_PROMPT_BYTES: usize = 32 * 1024;
pub const MAX_COMPLETION_BYTES: usize = 256 * 1024;
pub const MAX_TOKENS_LIMIT: u32 = 8192;
pub const DEFAULT_MAX_TOKENS: u32 = 256;
pub const DEFAULT_TEMPERATURE: f32 = 0.7;
pub const DEFAULT_TIMEOUT_SECS: u64 = 300;
pub const MAX_TIMEOUT_SECS: u64 = 3600;
pub const STREAM_CHUNK_BYTES: usize = 4096;
pub const STREAM_QUEUE_LEN: usize = 32;
const STDERR_CAPTURE_BYTES: usize = 65536;

#[derive(Debug, Clone)]
pub struct LlmRequest {
    pub model: String,
    pub prompt: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub system: Option<String>,
    pub timeout_secs: u64,
}

#[derive(Debug, Clone)]
pub struct LlmOutcome {
    pub completion: String,
    pub backend: String,
    pub truncated: bool,
    pub elapsed_ms: u64,
}

impl LlmRequest {
    pub fn from_with(with: &HashMap<String, String>) -> Result<Self> {
        let model = with
            .get("model")
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        if model.is_empty() {
            bail!("llm step needs a model");
        }
        if model.len() > 256 {
            bail!("llm model name is too long");
        }
        let prompt = with.get("prompt").cloned().unwrap_or_default();
        if prompt.is_empty() {
            bail!("llm step needs a prompt");
        }
        if prompt.len() > MAX_PROMPT_BYTES {
            bail!("llm prompt exceeds 32KiB");
        }
        let max_tokens = match with.get("max-tokens") {
            Some(raw) => raw
                .trim()
                .parse::<u32>()
                .map_err(|_| anyhow::anyhow!("bad max-tokens"))?,
            None => DEFAULT_MAX_TOKENS,
        };
        if max_tokens == 0 || max_tokens > MAX_TOKENS_LIMIT {
            bail!("max-tokens must be 1..=8192");
        }
        let temperature = match with.get("temperature") {
            Some(raw) => raw
                .trim()
                .parse::<f32>()
                .map_err(|_| anyhow::anyhow!("bad temperature"))?,
            None => DEFAULT_TEMPERATURE,
        };
        if !temperature.is_finite() || !(0.0..=2.0).contains(&temperature) {
            bail!("temperature must be 0..=2");
        }
        let system = with
            .get("system")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if system.as_ref().is_some_and(|s| s.len() > MAX_PROMPT_BYTES) {
            bail!("llm system prompt exceeds 32KiB");
        }
        let timeout_secs = match with.get("timeout-secs") {
            Some(raw) => raw
                .trim()
                .parse::<u64>()
                .map_err(|_| anyhow::anyhow!("bad timeout-secs"))?,
            None => DEFAULT_TIMEOUT_SECS,
        };
        if timeout_secs == 0 || timeout_secs > MAX_TIMEOUT_SECS {
            bail!("timeout-secs must be 1..=3600");
        }
        Ok(LlmRequest {
            model,
            prompt,
            max_tokens,
            temperature,
            system,
            timeout_secs,
        })
    }
}

pub fn resolve_backend(configured: Option<&str>) -> String {
    if let Ok(env) = std::env::var(BACKEND_ENV_VAR) {
        let trimmed = env.trim().to_string();
        if !trimmed.is_empty() {
            return trimmed;
        }
    }
    configured
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_BACKEND.to_string())
}

pub fn shell_quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

pub fn llm_step_script(with: &HashMap<String, String>) -> String {
    let request = match LlmRequest::from_with(with) {
        Ok(request) => request,
        Err(e) => return format!("echo 'gitforest/llm: {e}' >&2; exit 1"),
    };
    let mut args = format!(
        "llm run --model {} --prompt {} --max-tokens {} --temperature {} --timeout {}",
        shell_quote(&request.model),
        shell_quote(&request.prompt),
        request.max_tokens,
        request.temperature,
        request.timeout_secs,
    );
    if let Some(system) = &request.system {
        args.push_str(&format!(" --system {}", shell_quote(system)));
    }
    format!(
        "_gf_completion=$(gitforest {args}) || exit 1\nprintf 'completion<<GF_EOF\\n%s\\nGF_EOF\\n' \"$_gf_completion\" >> \"$GF_OUTPUTS\""
    )
}

fn spawn_backend(
    backend: &str,
    weights: &Path,
    request: &LlmRequest,
    work_dir: &Path,
) -> Result<tokio::process::Child> {
    if backend.is_empty() {
        bail!("no llm backend configured");
    }
    let prompt_path = work_dir.join("prompt.txt");
    std::fs::write(&prompt_path, request.prompt.as_bytes())?;
    let mut cmd = tokio::process::Command::new(backend);
    cmd.arg("-m")
        .arg(weights)
        .arg("-f")
        .arg(&prompt_path)
        .arg("-n")
        .arg(request.max_tokens.to_string())
        .arg("--temp")
        .arg(request.temperature.to_string())
        .arg("--no-display-prompt")
        .arg("--no-warmup")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(system) = &request.system {
        cmd.arg("--system-prompt").arg(system);
    }
    cmd.spawn().context("failed to spawn llm backend")
}

pub async fn run_inference(
    backend: &str,
    weights: &Path,
    request: &LlmRequest,
    work_dir: &Path,
) -> Result<LlmOutcome> {
    let child = spawn_backend(backend, weights, request, work_dir)?;
    let start = std::time::Instant::now();
    let timeout = tokio::time::Duration::from_secs(request.timeout_secs);
    let mut waiter = tokio::spawn(async move { child.wait_with_output().await });
    let output = match tokio::time::timeout(timeout, &mut waiter).await {
        Ok(Ok(Ok(output))) => output,
        Ok(Ok(Err(e))) => bail!("llm backend wait failed: {e}"),
        Ok(Err(e)) => bail!("llm backend join failed: {e}"),
        Err(_) => {
            waiter.abort();
            bail!("llm backend timed out");
        }
    };
    let elapsed_ms = start.elapsed().as_millis() as u64;
    if !output.status.success() {
        let text = String::from_utf8_lossy(&output.stderr);
        let clipped: String = text.chars().take(500).collect();
        bail!("llm backend exited {}: {clipped}", output.status);
    }
    let mut completion = output.stdout;
    let truncated = completion.len() > MAX_COMPLETION_BYTES;
    completion.truncate(MAX_COMPLETION_BYTES);
    let completion = String::from_utf8_lossy(&completion).into_owned();
    Ok(LlmOutcome {
        completion,
        backend: backend.to_string(),
        truncated,
        elapsed_ms,
    })
}

#[derive(Debug, Clone)]
pub enum StreamItem {
    Chunk(String),
    Finished { truncated: bool, elapsed_ms: u64 },
    Error(String),
}

struct Utf8Chunker {
    pending: Vec<u8>,
}

impl Utf8Chunker {
    fn new() -> Self {
        Self {
            pending: Vec::new(),
        }
    }

    fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut out = String::new();
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(valid) => {
                    out.push_str(valid);
                    self.pending.clear();
                    return out;
                }
                Err(e) => {
                    let valid_up_to = e.valid_up_to();
                    if valid_up_to > 0 {
                        if let Ok(valid) = std::str::from_utf8(&self.pending[..valid_up_to]) {
                            out.push_str(valid);
                        }
                        self.pending.drain(..valid_up_to);
                        continue;
                    }
                    match e.error_len() {
                        Some(len) => {
                            out.push('\u{FFFD}');
                            self.pending.drain(..len);
                        }
                        None => return out,
                    }
                }
            }
        }
    }

    fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

pub async fn run_inference_stream(
    backend: &str,
    weights: &Path,
    request: &LlmRequest,
    work_dir: &Path,
) -> Result<tokio::sync::mpsc::Receiver<StreamItem>> {
    let mut child = spawn_backend(backend, weights, request, work_dir)?;
    let stdout = child
        .stdout
        .take()
        .context("llm backend stdout unavailable")?;
    let stderr = child
        .stderr
        .take()
        .context("llm backend stderr unavailable")?;
    let (tx, rx) = tokio::sync::mpsc::channel(STREAM_QUEUE_LEN);
    let timeout_secs = request.timeout_secs;
    tokio::spawn(async move {
        drive_stream(child, stdout, stderr, tx, timeout_secs).await;
    });
    Ok(rx)
}

async fn drain_capped(mut stderr: tokio::process::ChildStderr) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    let mut captured = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match stderr.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if captured.len() < STDERR_CAPTURE_BYTES {
                    let room = STDERR_CAPTURE_BYTES - captured.len();
                    captured.extend_from_slice(&buf[..n.min(room)]);
                }
            }
            Err(_) => break,
        }
    }
    captured
}

async fn drive_stream(
    mut child: tokio::process::Child,
    mut stdout: tokio::process::ChildStdout,
    stderr: tokio::process::ChildStderr,
    tx: tokio::sync::mpsc::Sender<StreamItem>,
    timeout_secs: u64,
) {
    use tokio::io::AsyncReadExt;
    let start = std::time::Instant::now();
    let deadline = start + std::time::Duration::from_secs(timeout_secs);
    let stderr_task = tokio::spawn(drain_capped(stderr));
    let mut chunker = Utf8Chunker::new();
    let mut total: usize = 0;
    let mut truncated = false;
    let mut buf = [0u8; STREAM_CHUNK_BYTES];
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        match tokio::time::timeout(remaining, stdout.read(&mut buf)).await {
            Ok(Ok(0)) => {
                let tail = chunker.finish();
                if !tail.is_empty() {
                    let _ = tx.send(StreamItem::Chunk(tail)).await;
                }
                break;
            }
            Ok(Ok(n)) => {
                total += n;
                if total > MAX_COMPLETION_BYTES {
                    truncated = true;
                    continue;
                }
                let text = chunker.push(&buf[..n]);
                if !text.is_empty() && tx.send(StreamItem::Chunk(text)).await.is_err() {
                    let _ = child.kill().await;
                    stderr_task.abort();
                    return;
                }
            }
            Ok(Err(e)) => {
                let _ = tx
                    .send(StreamItem::Error(format!("llm backend stdout failed: {e}")))
                    .await;
                let _ = child.kill().await;
                stderr_task.abort();
                return;
            }
            Err(_) => {
                let _ = tx
                    .send(StreamItem::Error("llm backend timed out".to_string()))
                    .await;
                let _ = child.kill().await;
                stderr_task.abort();
                return;
            }
        }
    }
    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
    let status = match tokio::time::timeout(remaining, child.wait()).await {
        Ok(Ok(status)) => status,
        Ok(Err(e)) => {
            let _ = tx
                .send(StreamItem::Error(format!("llm backend wait failed: {e}")))
                .await;
            stderr_task.abort();
            return;
        }
        Err(_) => {
            let _ = tx
                .send(StreamItem::Error("llm backend timed out".to_string()))
                .await;
            let _ = child.kill().await;
            stderr_task.abort();
            return;
        }
    };
    let stderr_bytes = stderr_task.await.unwrap_or_default();
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr_bytes);
        let clipped: String = text.chars().take(500).collect();
        let _ = tx
            .send(StreamItem::Error(format!(
                "llm backend exited {status}: {clipped}"
            )))
            .await;
        return;
    }
    let _ = tx
        .send(StreamItem::Finished {
            truncated,
            elapsed_ms: start.elapsed().as_millis() as u64,
        })
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn with_pairs(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn request_defaults_and_bounds() {
        let ok =
            LlmRequest::from_with(&with_pairs(&[("model", "llama-7b"), ("prompt", "hi")])).unwrap();
        assert_eq!(ok.max_tokens, DEFAULT_MAX_TOKENS);
        assert_eq!(ok.temperature, DEFAULT_TEMPERATURE);
        assert_eq!(ok.timeout_secs, DEFAULT_TIMEOUT_SECS);
        assert!(ok.system.is_none());
        let bad_tokens = with_pairs(&[("model", "m"), ("prompt", "p"), ("max-tokens", "0")]);
        assert!(LlmRequest::from_with(&bad_tokens).is_err());
        let bad_temp = with_pairs(&[("model", "m"), ("prompt", "p"), ("temperature", "3")]);
        assert!(LlmRequest::from_with(&bad_temp).is_err());
        let missing = with_pairs(&[("prompt", "p")]);
        assert!(LlmRequest::from_with(&missing).is_err());
        let big = with_pairs(&[
            ("model", "m"),
            ("prompt", &"x".repeat(MAX_PROMPT_BYTES + 1)),
        ]);
        assert!(LlmRequest::from_with(&big).is_err());
    }

    #[test]
    fn backend_env_overrides_config() {
        let key = BACKEND_ENV_VAR;
        let prior = std::env::var(key).ok();
        std::env::set_var(key, "/tmp/fake-llama");
        assert_eq!(resolve_backend(Some("llama-server")), "/tmp/fake-llama");
        std::env::remove_var(key);
        assert_eq!(resolve_backend(Some("llama-server")), "llama-server");
        assert_eq!(resolve_backend(None), DEFAULT_BACKEND);
        if let Some(value) = prior {
            std::env::set_var(key, value);
        }
    }

    #[test]
    fn shell_quote_escapes_single_quotes() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("don't"), "'don'\\''t'");
        let script = llm_step_script(&with_pairs(&[
            ("model", "llama-7b"),
            ("prompt", "say 'hi'"),
        ]));
        assert!(script.contains("--prompt 'say '\\''hi'\\'''"), "{script}");
        assert!(script.contains("$GF_OUTPUTS"), "{script}");
    }

    #[test]
    fn step_script_fails_fast_without_model() {
        let script = llm_step_script(&with_pairs(&[("prompt", "p")]));
        assert!(script.contains("exit 1"), "{script}");
    }

    #[cfg(unix)]
    fn stub_backend(dir: &Path, unix_body: &str, _windows_body: &str) -> PathBuf {
        let path = dir.join("fake-llama");
        let script = format!("#!/bin/sh\n{unix_body}\n");
        std::fs::write(&path, script).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).unwrap();
        }
        path
    }

    #[cfg(windows)]
    fn stub_backend(dir: &Path, _unix_body: &str, windows_body: &str) -> PathBuf {
        let path = dir.join("fake-llama.bat");
        let script = format!("@echo off\n{windows_body}\n");
        std::fs::write(&path, script).unwrap();
        path
    }

    fn stub_request(prompt: &str) -> LlmRequest {
        LlmRequest {
            model: "tiny".to_string(),
            prompt: prompt.to_string(),
            max_tokens: 16,
            temperature: 0.0,
            system: None,
            timeout_secs: 30,
        }
    }

    #[tokio::test]
    async fn inference_captures_backend_stdout() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(&dir, "echo \"canned from $4\"", "echo canned from %4");
        let outcome = run_inference(
            backend.to_str().unwrap(),
            &weights,
            &stub_request("hello"),
            &dir,
        )
        .await
        .unwrap();
        assert!(outcome.completion.contains("canned from"), "{outcome:?}");
        assert!(!outcome.truncated);
        assert_eq!(outcome.backend, backend.to_str().unwrap());
        assert!(dir.join("prompt.txt").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn inference_reports_backend_failure() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(&dir, "echo 'boom' >&2; exit 3", "echo boom 1>&2\nexit /b 3");
        let err = run_inference(
            backend.to_str().unwrap(),
            &weights,
            &stub_request("hello"),
            &dir,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("boom"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn inference_enforces_timeout() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(&dir, "sleep 30", "ping -n 31 127.0.0.1 >nul");
        let mut request = stub_request("hello");
        request.timeout_secs = 1;
        let err = run_inference(backend.to_str().unwrap(), &weights, &request, &dir)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    async fn collect_stream(
        rx: &mut tokio::sync::mpsc::Receiver<StreamItem>,
    ) -> (String, Option<StreamItem>) {
        let mut text = String::new();
        let mut terminal = None;
        while let Some(item) = rx.recv().await {
            match item {
                StreamItem::Chunk(part) => text.push_str(&part),
                done => {
                    terminal = Some(done);
                    break;
                }
            }
        }
        (text, terminal)
    }

    #[tokio::test]
    async fn stream_delivers_first_chunk_before_backend_exits() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(
            &dir,
            "printf 'one\\n'; sleep 2; printf 'two\\n'",
            "echo one\nping -n 4 127.0.0.1 >nul\necho two",
        );
        let started = std::time::Instant::now();
        let mut rx = run_inference_stream(
            backend.to_str().unwrap(),
            &weights,
            &stub_request("hello"),
            &dir,
        )
        .await
        .unwrap();
        let mut text = String::new();
        let mut first_chunk_at: Option<std::time::Instant> = None;
        let mut terminal = None;
        while let Some(item) = rx.recv().await {
            match item {
                StreamItem::Chunk(part) => {
                    if first_chunk_at.is_none() {
                        first_chunk_at = Some(std::time::Instant::now());
                    }
                    text.push_str(&part);
                }
                done => {
                    terminal = Some(done);
                    break;
                }
            }
        }
        #[cfg(unix)]
        assert_eq!(text, "one\ntwo\n");
        #[cfg(windows)]
        assert_eq!(text, "one\r\ntwo\r\n");
        let first_latency = first_chunk_at.expect("no chunks streamed") - started;
        assert!(
            first_latency < std::time::Duration::from_millis(1500),
            "first chunk arrived late: {first_latency:?}"
        );
        match terminal {
            Some(StreamItem::Finished {
                truncated,
                elapsed_ms,
            }) => {
                assert!(!truncated);
                assert!(elapsed_ms >= 1900, "elapsed_ms={elapsed_ms}");
            }
            other => panic!("expected Finished, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_reports_backend_failure() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(&dir, "echo 'boom' >&2; exit 3", "echo boom 1>&2\nexit /b 3");
        let mut rx = run_inference_stream(
            backend.to_str().unwrap(),
            &weights,
            &stub_request("hello"),
            &dir,
        )
        .await
        .unwrap();
        let (_, terminal) = collect_stream(&mut rx).await;
        match terminal {
            Some(StreamItem::Error(message)) => {
                assert!(message.contains("boom"), "{message}");
                assert!(message.contains("exit"), "{message}");
            }
            other => panic!("expected Error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_enforces_timeout() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(&dir, "sleep 30", "ping -n 31 127.0.0.1 >nul");
        let mut request = stub_request("hello");
        request.timeout_secs = 1;
        let mut rx = run_inference_stream(backend.to_str().unwrap(), &weights, &request, &dir)
            .await
            .unwrap();
        let (_, terminal) = collect_stream(&mut rx).await;
        match terminal {
            Some(StreamItem::Error(message)) => {
                assert!(message.contains("timed out"), "{message}");
            }
            other => panic!("expected timeout Error, got {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stream_truncates_huge_output() {
        let dir = std::env::temp_dir().join(format!("gf-llm-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let weights = dir.join("tiny.gguf");
        std::fs::write(&weights, b"GGUFfake").unwrap();
        let backend = stub_backend(
            &dir,
            "head -c 300000 /dev/zero | tr '\\0' 'x'",
            "powershell -NoProfile -Command \"[Console]::Out.Write('x' * 300000)\"",
        );
        let mut rx = run_inference_stream(
            backend.to_str().unwrap(),
            &weights,
            &stub_request("hello"),
            &dir,
        )
        .await
        .unwrap();
        let (text, terminal) = collect_stream(&mut rx).await;
        match terminal {
            Some(StreamItem::Finished {
                truncated,
                elapsed_ms: _,
            }) => assert!(truncated, "300KB output must truncate"),
            other => panic!("expected truncated Finished, got {other:?}"),
        }
        assert!(text.len() <= MAX_COMPLETION_BYTES, "len={}", text.len());
        assert!(
            text.len() > MAX_COMPLETION_BYTES - 8192,
            "len={}",
            text.len()
        );
        assert!(text.bytes().all(|b| b == b'x'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn utf8_chunker_holds_incomplete_sequences() {
        let mut chunker = Utf8Chunker::new();
        assert_eq!(chunker.push("h".as_bytes()), "h");
        assert_eq!(chunker.push(&[0xC3]), "");
        assert_eq!(chunker.push(&[0xA9, b'!']), "é!");
        assert_eq!(chunker.finish(), "");
        let mut chunker = Utf8Chunker::new();
        assert_eq!(chunker.push(&[0xFF]), "\u{FFFD}");
        let mut chunker = Utf8Chunker::new();
        assert_eq!(chunker.push(&[0xE2, 0x82]), "");
        assert_eq!(chunker.finish(), "�");
    }
}
