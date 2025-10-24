use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use chrono::Utc;
use reqwest::header::HeaderMap;
use serde_json::Value;
use std::fs::OpenOptions;
use std::io::Write;
use tracing::warn;
use uuid::Uuid;

#[derive(Clone, Debug)]
pub struct ApiDebugLogger {
    inner: Arc<ApiDebugLoggerInner>,
}

#[derive(Debug)]
struct ApiDebugLoggerInner {
    enabled: bool,
    directory: PathBuf,
}

impl ApiDebugLogger {
    pub fn new(base_dir: &Path) -> Self {
        let mut directory = base_dir.to_path_buf();
        directory.push("debug_api_logs");
        let enabled = std::fs::create_dir_all(&directory)
            .map(|_| true)
            .unwrap_or_else(|err| {
                warn!("failed to create debug directory {:?}: {}", directory, err);
                false
            });

        Self {
            inner: Arc::new(ApiDebugLoggerInner { enabled, directory }),
        }
    }

    pub fn disabled() -> Self {
        Self {
            inner: Arc::new(ApiDebugLoggerInner {
                enabled: false,
                directory: PathBuf::new(),
            }),
        }
    }

    pub fn start_request(
        &self,
        method: &str,
        url: &str,
        payload: &Value,
        session_id: Option<String>,
    ) -> RequestLogHandle {
        if !self.inner.enabled {
            return RequestLogHandle::disabled();
        }

        let timestamp = Utc::now();
        let request_id = Uuid::new_v4().to_string();
        let short_id = &request_id[..8];
        let base_name = format!("{}_{short_id}", timestamp.format("%Y%m%dT%H%M%S%.3fZ"));

        let request_path = self
            .inner
            .directory
            .join(format!("{base_name}_request.json"));
        let response_path = self
            .inner
            .directory
            .join(format!("{base_name}_response.jsonl"));

        let usage_path = self.inner.directory.join(format!("{base_name}_usage.json"));

        let entry = serde_json::json!({
            "timestamp": timestamp.to_rfc3339(),
            "request_id": request_id,
            "method": method,
            "url": url,
            "payload": payload,
            "session_id": session_id,
        });

        if let Err(err) = write_pretty_json(&request_path, &entry) {
            warn!("failed to log request {:?}: {}", request_path, err);
            return RequestLogHandle::disabled();
        }

        RequestLogHandle {
            inner: Arc::new(RequestLogInner {
                enabled: true,
                response_path,
                request_id,
                session_id,
                usage_path,
                usage_entries: Mutex::new(Vec::new()),
            }),
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct RequestLogHandle {
    inner: Arc<RequestLogInner>,
}

#[derive(Debug, Default)]
struct RequestLogInner {
    enabled: bool,
    response_path: PathBuf,
    request_id: String,
    session_id: Option<String>,
    usage_path: PathBuf,
    usage_entries: Mutex<Vec<Value>>,
}

impl RequestLogHandle {
    pub(crate) fn disabled() -> Self {
        Self::default()
    }

    pub fn request_id(&self) -> &str {
        &self.inner.request_id
    }

    pub fn log_response_headers(&self, status: u16, headers: &HeaderMap) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "type": "response_headers",
            "request_id": self.request_id(),
            "session_id": self.inner.session_id,
            "status": status,
            "headers": headers_to_json(headers),
        });
        self.append(&entry);
    }

    pub fn log_text_body(&self, label: &str, status: Option<u16>, body: &str) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "type": label,
            "request_id": self.request_id(),
            "session_id": self.inner.session_id,
            "status": status,
            "body": body,
        });
        self.append(&entry);
    }

    pub fn log_sse_event(&self, event: &str, data: &str) {
        let parsed_data =
            serde_json::from_str::<Value>(data).unwrap_or_else(|_| Value::String(data.to_string()));
        let entry_data = parsed_data.clone();
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "type": "sse_event",
            "request_id": self.request_id(),
            "session_id": self.inner.session_id,
            "event": event,
            "data": entry_data,
        });
        self.append(&entry);

        if let Some(usage) = extract_usage(&parsed_data) {
            self.append_usage(&usage);
        }
    }

    pub fn log_retry(&self, attempt: u64, wait_ms: u64) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "type": "retry_scheduled",
            "request_id": self.request_id(),
            "session_id": self.inner.session_id,
            "attempt": attempt,
            "wait_ms": wait_ms,
        });
        self.append(&entry);
    }

    pub fn log_error(&self, message: &str) {
        let entry = serde_json::json!({
            "timestamp": Utc::now().to_rfc3339(),
            "type": "error",
            "request_id": self.request_id(),
            "session_id": self.inner.session_id,
            "message": message,
        });
        self.append(&entry);
    }

    pub fn enabled(&self) -> bool {
        self.inner.enabled
    }

    fn append(&self, value: &Value) {
        if !self.inner.enabled {
            return;
        }

        if let Err(err) = append_json_line(&self.inner.response_path, value) {
            warn!(
                "failed to append response log {:?}: {}",
                self.inner.response_path, err
            );
        }
    }

    fn append_usage(&self, usage: &Value) {
        if !self.inner.enabled {
            return;
        }

        if let Err(err) = self.inner.append_usage(usage) {
            warn!(
                "failed to update usage log {:?}: {}",
                self.inner.usage_path, err
            );
        }
    }
}

impl RequestLogInner {
    fn append_usage(&self, usage: &Value) -> std::io::Result<()> {
        let entries = {
            let mut guard = self.usage_entries.lock().unwrap();
            guard.push(usage.clone());
            guard.clone()
        };

        write_pretty_json(&self.usage_path, &Value::Array(entries))
    }
}

fn extract_usage(value: &Value) -> Option<Value> {
    match value {
        Value::Object(map) => {
            if let Some(usage) = map.get("usage") {
                if !usage.is_null() {
                    return Some(usage.clone());
                }
            }

            if let Some(response) = map.get("response") {
                return extract_usage(response);
            }

            None
        }
        _ => None,
    }
}

fn headers_to_json(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (key, value) in headers.iter() {
        let name = key.as_str().to_string();
        let val = value.to_str().unwrap_or_default().to_string();
        map.insert(name, Value::String(val));
    }
    Value::Object(map)
}

fn write_pretty_json(path: &Path, value: &Value) -> std::io::Result<()> {
    let content = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    std::fs::write(path, content)
}

fn append_json_line(path: &Path, value: &Value) -> std::io::Result<()> {
    let mut file = OpenOptions::new().create(true).append(true).open(path)?;
    let line = serde_json::to_string_pretty(value)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")
}
