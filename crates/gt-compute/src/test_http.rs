#![cfg(test)]

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

pub struct RecordedRequest {
    pub path: String,
    pub body: Vec<u8>,
}

impl RecordedRequest {
    pub fn body_str(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

pub struct MockServer {
    pub base_url: String,
    received: Arc<Mutex<Vec<RecordedRequest>>>,
    shutdown: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl MockServer {
    pub fn start(rules: Vec<(String, String)>) -> Self {
        Self::start_with_status(
            rules
                .into_iter()
                .map(|(pat, body)| (pat, 200, body))
                .collect(),
        )
    }

    pub fn start_with_status(rules: Vec<(String, u16, String)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        listener.set_nonblocking(true).unwrap();
        let received: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let shutdown = Arc::new(AtomicBool::new(false));
        let worker_received = received.clone();
        let worker_shutdown = shutdown.clone();
        let rules = Arc::new(rules);
        let handle = std::thread::spawn(move || {
            while !worker_shutdown.load(Ordering::Relaxed) {
                let (stream, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(_) => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                };
                let _ = stream.set_nonblocking(false);
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                if let Some(req) = read_request(stream.try_clone().unwrap()) {
                    let (status, body) = rules
                        .iter()
                        .find(|(pat, _, _)| req.path.contains(pat) || req.body_str().contains(pat))
                        .map(|(_, status, body)| (*status, body.clone()))
                        .unwrap_or((200, r#"{"error":"no mock rule"}"#.to_string()));
                    // Record before responding: a client that got its reply
                    // must always find the request in requests().
                    worker_received.lock().unwrap().push(req);
                    let reason = if status == 200 { "OK" } else { "Error" };
                    let response = format!(
                        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.try_clone().unwrap().write_all(response.as_bytes());
                }
            }
        });
        MockServer {
            base_url,
            received,
            shutdown,
            handle: Some(handle),
        }
    }

    pub fn requests(&self) -> Vec<RecordedRequest> {
        std::mem::take(&mut self.received.lock().unwrap())
    }

    pub fn stop(mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }
}

fn read_request(mut stream: std::net::TcpStream) -> Option<RecordedRequest> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read_exact(&mut byte).is_err() || head.len() > 65536 {
            return None;
        }
        head.push(byte[0]);
    }
    let head_str = String::from_utf8_lossy(&head).into_owned();
    let mut lines = head_str.lines();
    let request_line = lines.next().unwrap_or("");
    let mut parts = request_line.split_whitespace();
    parts.next();
    let path = parts.next().unwrap_or("").to_string();
    let content_len = head_str
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0)
        .min(1 << 20);
    let mut body = vec![0u8; content_len];
    if content_len > 0 && stream.read_exact(&mut body).is_err() {
        return None;
    }
    Some(RecordedRequest { path, body })
}
