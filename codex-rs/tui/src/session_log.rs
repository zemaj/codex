use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use codex_core::config::Config;
use codex_core::protocol::{Event, Op};
use lazy_static::lazy_static;
use serde::Serialize;
use serde_json::json;

use crate::app_event::AppEvent;

lazy_static! {
    static ref LOGGER: SessionLogger = SessionLogger::default();
}

#[derive(Default)]
struct SessionLogger {
    file: Mutex<Option<File>>,
}

impl SessionLogger {
    fn open(&self, path: PathBuf) -> std::io::Result<()> {
        let mut opts = OpenOptions::new();
        opts.create(true).truncate(true).write(true);

        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }

        let file = opts.open(path)?;
        let mut guard = self.file.lock().unwrap();
        *guard = Some(file);
        Ok(())
    }

    fn write_json_line(&self, value: serde_json::Value) {
        if let Some(file) = self.file.lock().unwrap().as_mut() {
            if let Ok(serialized) = serde_json::to_string(&value) {
                let _ = file.write_all(serialized.as_bytes());
                let _ = file.write_all(b"\n");
                let _ = file.flush();
            }
        }
    }
}

fn now_ts() -> String {
    // RFC3339 for readability; consumers can parse as needed.
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

pub(crate) fn maybe_init(config: &Config) {
    let enabled = std::env::var("CODEX_TUI_RECORD_SESSION")
        .map(|v| matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false);
    if !enabled {
        return;
    }

    let path = if let Ok(path) = std::env::var("CODEX_TUI_SESSION_LOG_PATH") {
        PathBuf::from(path)
    } else {
        let mut p = match codex_core::config::log_dir(config) {
            Ok(dir) => dir,
            Err(_) => std::env::temp_dir(),
        };
        let filename = format!("session-{}.jsonl", chrono::Utc::now().format("%Y%m%dT%H%M%SZ"));
        p.push(filename);
        p
    };

    if let Err(e) = LOGGER.open(path.clone()) {
        tracing::error!("failed to open session log {:?}: {}", path, e);
        return;
    }

    // Write a header record so we can attach context.
    let header = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_start",
        "cwd": config.cwd,
        "model": config.model,
        "model_provider_id": config.model_provider_id,
        "model_provider_name": config.model_provider.name,
    });
    LOGGER.write_json_line(header);
}

pub(crate) fn log_inbound_app_event(event: &AppEvent) {
    // Log only if enabled
    if LOGGER.file.lock().unwrap().is_none() {
        return;
    }

    match event {
        AppEvent::CodexEvent(ev) => {
            write_record("to_tui", "codex_event", ev);
        }
        AppEvent::KeyEvent(k) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "key_event",
                "event": format!("{:?}", k),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::Paste(s) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "paste",
                "text": s,
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::DispatchCommand(cmd) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "slash_command",
                "command": format!("{:?}", cmd),
            });
            LOGGER.write_json_line(value);
        }
        // Internal UI events; still log for fidelity, but avoid heavy payloads.
        AppEvent::InsertHistory(lines) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "insert_history",
                "lines": lines.len(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::StartFileSearch(query) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_start",
                "query": query,
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::FileSearchResult { query, matches } => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "file_search_result",
                "query": query,
                "matches": matches.len(),
            });
            LOGGER.write_json_line(value);
        }
        AppEvent::LatestLog(line) => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "log_line",
                "line": line,
            });
            LOGGER.write_json_line(value);
        }
        // Noise or control flow – record variant only
        other => {
            let value = json!({
                "ts": now_ts(),
                "dir": "to_tui",
                "kind": "app_event",
                "variant": format!("{:?}", other).split('(').next().unwrap_or("app_event"),
            });
            LOGGER.write_json_line(value);
        }
    }
}

pub(crate) fn log_outbound_op(op: &Op) {
    if LOGGER.file.lock().unwrap().is_none() {
        return;
    }
    write_record("from_tui", "op", op);
}

pub(crate) fn log_session_end() {
    if LOGGER.file.lock().unwrap().is_none() {
        return;
    }
    let value = json!({
        "ts": now_ts(),
        "dir": "meta",
        "kind": "session_end",
    });
    LOGGER.write_json_line(value);
}

fn write_record<T>(dir: &str, kind: &str, obj: &T)
where
    T: Serialize,
{
    let value = json!({
        "ts": now_ts(),
        "dir": dir,
        "kind": kind,
        "payload": obj,
    });
    LOGGER.write_json_line(value);
}


