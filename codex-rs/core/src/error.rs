use crate::codex::ProcessedResponseItem;
use crate::exec::ExecToolCallOutput;
use crate::token_data::KnownPlan;
use crate::token_data::PlanType;
use crate::truncate::truncate_middle;
use chrono::DateTime;
use chrono::Utc;
use codex_async_utils::CancelErr;
use codex_protocol::ConversationId;
use codex_protocol::protocol::RateLimitSnapshot;
use reqwest::StatusCode;
use serde_json;
use std::io;
use std::time::Duration;
use thiserror::Error;
use tokio::task::JoinError;

pub type Result<T> = std::result::Result<T, CodexErr>;

/// Limit UI error messages to a reasonable size while keeping useful context.
const ERROR_MESSAGE_UI_MAX_BYTES: usize = 2 * 1024; // 4 KiB

#[derive(Error, Debug)]
pub enum SandboxErr {
    /// Error from sandbox execution
    #[error(
        "sandbox denied exec error, exit code: {}, stdout: {}, stderr: {}",
        .output.exit_code, .output.stdout.text, .output.stderr.text
    )]
    Denied { output: Box<ExecToolCallOutput> },

    /// Error from linux seccomp filter setup
    #[cfg(target_os = "linux")]
    #[error("seccomp setup error")]
    SeccompInstall(#[from] seccompiler::Error),

    /// Error from linux seccomp backend
    #[cfg(target_os = "linux")]
    #[error("seccomp backend error")]
    SeccompBackend(#[from] seccompiler::BackendError),

    /// Command timed out
    #[error("command timed out")]
    Timeout { output: Box<ExecToolCallOutput> },

    /// Command was killed by a signal
    #[error("command was killed by a signal")]
    Signal(i32),

    /// Error from linux landlock
    #[error("Landlock was not able to fully enforce all sandbox rules")]
    LandlockRestrict,
}

#[derive(Error, Debug)]
pub enum CodexErr {
    // todo(aibrahim): git rid of this error carrying the dangling artifacts
    #[error("turn aborted. Something went wrong? Hit `/feedback` to report the issue.")]
    TurnAborted {
        dangling_artifacts: Vec<ProcessedResponseItem>,
    },

    /// Returned by ResponsesClient when the SSE stream disconnects or errors out **after** the HTTP
    /// handshake has succeeded but **before** it finished emitting `response.completed`.
    ///
    /// The Session loop treats this as a transient error and will automatically retry the turn.
    ///
    /// Optionally includes the requested delay before retrying the turn.
    #[error("stream disconnected before completion: {0}")]
    Stream(String, Option<Duration>),

    #[error(
        "Codex ran out of room in the model's context window. Start a new conversation or clear earlier history before retrying."
    )]
    ContextWindowExceeded,

    #[error("no conversation with id: {0}")]
    ConversationNotFound(ConversationId),

    #[error("session configured event was not the first event in the stream")]
    SessionConfiguredNotFirstEvent,

    /// Returned by run_command_stream when the spawned child process timed out (10s).
    #[error("timeout waiting for child process to exit")]
    Timeout,

    /// Returned by run_command_stream when the child could not be spawned (its stdout/stderr pipes
    /// could not be captured). Analogous to the previous `CodexError::Spawn` variant.
    #[error("spawn failed: child stdout/stderr not captured")]
    Spawn,

    /// Returned by run_command_stream when the user pressed Ctrl‑C (SIGINT). Session uses this to
    /// surface a polite FunctionCallOutput back to the model instead of crashing the CLI.
    #[error("interrupted (Ctrl-C). Something went wrong? Hit `/feedback` to report the issue.")]
    Interrupted,

    /// Unexpected HTTP status code.
    #[error("{0}")]
    UnexpectedStatus(UnexpectedResponseError),

    #[error("{0}")]
    UsageLimitReached(UsageLimitReachedError),

    #[error("{0}")]
    ResponseStreamFailed(ResponseStreamFailed),

    #[error("{0}")]
    ConnectionFailed(ConnectionFailedError),

    #[error(
        "To use Codex with your ChatGPT plan, upgrade to Plus: https://openai.com/chatgpt/pricing."
    )]
    UsageNotIncluded,

    #[error("We're currently experiencing high demand, which may cause temporary errors.")]
    InternalServerError,

    /// Retry limit exceeded.
    #[error("{0}")]
    RetryLimit(RetryLimitReachedError),

    /// Agent loop died unexpectedly
    #[error("internal error; agent loop died unexpectedly")]
    InternalAgentDied,

    /// Sandbox error
    #[error("sandbox error: {0}")]
    Sandbox(#[from] SandboxErr),

    #[error("codex-linux-sandbox was required but not provided")]
    LandlockSandboxExecutableNotProvided,

    #[error("unsupported operation: {0}")]
    UnsupportedOperation(String),

    #[error("Fatal error: {0}")]
    Fatal(String),

    // -----------------------------------------------------------------
    // Automatic conversions for common external error types
    // -----------------------------------------------------------------
    #[error(transparent)]
    Io(#[from] io::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),

    #[cfg(target_os = "linux")]
    #[error(transparent)]
    LandlockRuleset(#[from] landlock::RulesetError),

    #[cfg(target_os = "linux")]
    #[error(transparent)]
    LandlockPathFd(#[from] landlock::PathFdError),

    #[error(transparent)]
    TokioJoin(#[from] JoinError),

    #[error("{0}")]
    EnvVar(EnvVarError),
}

impl From<CancelErr> for CodexErr {
    fn from(_: CancelErr) -> Self {
        CodexErr::TurnAborted {
            dangling_artifacts: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct ConnectionFailedError {
    pub source: reqwest::Error,
}

impl std::fmt::Display for ConnectionFailedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Connection failed: {}", self.source)
    }
}

#[derive(Debug)]
pub struct ResponseStreamFailed {
    pub source: reqwest::Error,
    pub request_id: Option<String>,
}

impl std::fmt::Display for ResponseStreamFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Error while reading the server response: {}{}",
            self.source,
            self.request_id
                .as_ref()
                .map(|id| format!(", request id: {id}"))
                .unwrap_or_default()
        )
    }
}

#[derive(Debug)]
pub struct UnexpectedResponseError {
    pub status: StatusCode,
    pub body: String,
    pub request_id: Option<String>,
}

impl std::fmt::Display for UnexpectedResponseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "unexpected status {}: {}{}",
            self.status,
            self.body,
            self.request_id
                .as_ref()
                .map(|id| format!(", request id: {id}"))
                .unwrap_or_default()
        )
    }
}

impl std::error::Error for UnexpectedResponseError {}
#[derive(Debug)]
pub struct RetryLimitReachedError {
    pub status: StatusCode,
    pub request_id: Option<String>,
}

impl std::fmt::Display for RetryLimitReachedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "exceeded retry limit, last status: {}{}",
            self.status,
            self.request_id
                .as_ref()
                .map(|id| format!(", request id: {id}"))
                .unwrap_or_default()
        )
    }
}

#[derive(Debug)]
pub struct UsageLimitReachedError {
    pub(crate) plan_type: Option<PlanType>,
    pub(crate) resets_at: Option<DateTime<Utc>>,
    pub(crate) rate_limits: Option<RateLimitSnapshot>,
}

impl std::fmt::Display for UsageLimitReachedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let message = match self.plan_type.as_ref() {
            Some(PlanType::Known(KnownPlan::Plus)) => format!(
                "You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing){}",
                retry_suffix_after_or(self.resets_at.as_ref())
            ),
            Some(PlanType::Known(KnownPlan::Team)) | Some(PlanType::Known(KnownPlan::Business)) => {
                format!(
                    "You've hit your usage limit. To get more access now, send a request to your admin{}",
                    retry_suffix_after_or(self.resets_at.as_ref())
                )
            }
            Some(PlanType::Known(KnownPlan::Free)) => {
                "You've hit your usage limit. Upgrade to Plus to continue using Codex (https://openai.com/chatgpt/pricing)."
                    .to_string()
            }
            Some(PlanType::Known(KnownPlan::Pro))
            | Some(PlanType::Known(KnownPlan::Enterprise))
            | Some(PlanType::Known(KnownPlan::Edu)) => format!(
                "You've hit your usage limit.{}",
                retry_suffix(self.resets_at.as_ref())
            ),
            Some(PlanType::Unknown(_)) | None => format!(
                "You've hit your usage limit.{}",
                retry_suffix(self.resets_at.as_ref())
            ),
        };

        write!(f, "{message}")
    }
}

fn retry_suffix(resets_at: Option<&DateTime<Utc>>) -> String {
    if let Some(secs) = remaining_seconds(resets_at) {
        let reset_duration = format_reset_duration(secs);
        format!(" Try again in {reset_duration}.")
    } else {
        " Try again later.".to_string()
    }
}

fn retry_suffix_after_or(resets_at: Option<&DateTime<Utc>>) -> String {
    if let Some(secs) = remaining_seconds(resets_at) {
        let reset_duration = format_reset_duration(secs);
        format!(" or try again in {reset_duration}.")
    } else {
        " or try again later.".to_string()
    }
}

fn remaining_seconds(resets_at: Option<&DateTime<Utc>>) -> Option<u64> {
    let resets_at = resets_at.cloned()?;
    let now = now_for_retry();
    let secs = resets_at.signed_duration_since(now).num_seconds();
    Some(if secs <= 0 { 0 } else { secs as u64 })
}

#[cfg(test)]
thread_local! {
    static NOW_OVERRIDE: std::cell::RefCell<Option<DateTime<Utc>>> =
        const { std::cell::RefCell::new(None) };
}

fn now_for_retry() -> DateTime<Utc> {
    #[cfg(test)]
    {
        if let Some(now) = NOW_OVERRIDE.with(|cell| *cell.borrow()) {
            return now;
        }
    }
    Utc::now()
}

fn format_reset_duration(total_secs: u64) -> String {
    let days = total_secs / 86_400;
    let hours = (total_secs % 86_400) / 3_600;
    let minutes = (total_secs % 3_600) / 60;

    let mut parts: Vec<String> = Vec::new();
    if days > 0 {
        let unit = if days == 1 { "day" } else { "days" };
        parts.push(format!("{days} {unit}"));
    }
    if hours > 0 {
        let unit = if hours == 1 { "hour" } else { "hours" };
        parts.push(format!("{hours} {unit}"));
    }
    if minutes > 0 {
        let unit = if minutes == 1 { "minute" } else { "minutes" };
        parts.push(format!("{minutes} {unit}"));
    }

    if parts.is_empty() {
        return "less than a minute".to_string();
    }

    match parts.len() {
        1 => parts[0].clone(),
        2 => format!("{} {}", parts[0], parts[1]),
        _ => format!("{} {} {}", parts[0], parts[1], parts[2]),
    }
}

#[derive(Debug)]
pub struct EnvVarError {
    /// Name of the environment variable that is missing.
    pub var: String,

    /// Optional instructions to help the user get a valid value for the
    /// variable and set it.
    pub instructions: Option<String>,
}

impl std::fmt::Display for EnvVarError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Missing environment variable: `{}`.", self.var)?;
        if let Some(instructions) = &self.instructions {
            write!(f, " {instructions}")?;
        }
        Ok(())
    }
}

impl CodexErr {
    /// Minimal shim so that existing `e.downcast_ref::<CodexErr>()` checks continue to compile
    /// after replacing `anyhow::Error` in the return signature. This mirrors the behavior of
    /// `anyhow::Error::downcast_ref` but works directly on our concrete enum.
    pub fn downcast_ref<T: std::any::Any>(&self) -> Option<&T> {
        (self as &dyn std::any::Any).downcast_ref::<T>()
    }
}

pub fn get_error_message_ui(e: &CodexErr) -> String {
    let message = match e {
        CodexErr::Sandbox(SandboxErr::Denied { output }) => {
            let aggregated = output.aggregated_output.text.trim();
            if !aggregated.is_empty() {
                output.aggregated_output.text.clone()
            } else {
                let stderr = output.stderr.text.trim();
                let stdout = output.stdout.text.trim();
                match (stderr.is_empty(), stdout.is_empty()) {
                    (false, false) => format!("{stderr}\n{stdout}"),
                    (false, true) => output.stderr.text.clone(),
                    (true, false) => output.stdout.text.clone(),
                    (true, true) => format!(
                        "command failed inside sandbox with exit code {}",
                        output.exit_code
                    ),
                }
            }
        }
        // Timeouts are not sandbox errors from a UX perspective; present them plainly
        CodexErr::Sandbox(SandboxErr::Timeout { output }) => {
            format!(
                "error: command timed out after {} ms",
                output.duration.as_millis()
            )
        }
        _ => e.to_string(),
    };

    truncate_middle(&message, ERROR_MESSAGE_UI_MAX_BYTES).0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::StreamOutput;
    use chrono::DateTime;
    use chrono::Duration as ChronoDuration;
    use chrono::TimeZone;
    use chrono::Utc;
    use codex_protocol::protocol::RateLimitWindow;
    use pretty_assertions::assert_eq;

    fn rate_limit_snapshot() -> RateLimitSnapshot {
        let primary_reset_at = Utc
            .with_ymd_and_hms(2024, 1, 1, 1, 0, 0)
            .unwrap()
            .timestamp();
        let secondary_reset_at = Utc
            .with_ymd_and_hms(2024, 1, 1, 2, 0, 0)
            .unwrap()
            .timestamp();
        RateLimitSnapshot {
            primary: Some(RateLimitWindow {
                used_percent: 50.0,
                window_minutes: Some(60),
                resets_at: Some(primary_reset_at),
            }),
            secondary: Some(RateLimitWindow {
                used_percent: 30.0,
                window_minutes: Some(120),
                resets_at: Some(secondary_reset_at),
            }),
        }
    }

    fn with_now_override<T>(now: DateTime<Utc>, f: impl FnOnce() -> T) -> T {
        NOW_OVERRIDE.with(|cell| {
            *cell.borrow_mut() = Some(now);
            let result = f();
            *cell.borrow_mut() = None;
            result
        })
    }

    #[test]
    fn usage_limit_reached_error_formats_plus_plan() {
        let err = UsageLimitReachedError {
            plan_type: Some(PlanType::Known(KnownPlan::Plus)),
            resets_at: None,
            rate_limits: Some(rate_limit_snapshot()),
        };
        assert_eq!(
            err.to_string(),
            "You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing) or try again later."
        );
    }

    #[test]
    fn sandbox_denied_uses_aggregated_output_when_stderr_empty() {
        let output = ExecToolCallOutput {
            exit_code: 77,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new("aggregate detail".to_string()),
            duration: Duration::from_millis(10),
            timed_out: false,
        };
        let err = CodexErr::Sandbox(SandboxErr::Denied {
            output: Box::new(output),
        });
        assert_eq!(get_error_message_ui(&err), "aggregate detail");
    }

    #[test]
    fn sandbox_denied_reports_both_streams_when_available() {
        let output = ExecToolCallOutput {
            exit_code: 9,
            stdout: StreamOutput::new("stdout detail".to_string()),
            stderr: StreamOutput::new("stderr detail".to_string()),
            aggregated_output: StreamOutput::new(String::new()),
            duration: Duration::from_millis(10),
            timed_out: false,
        };
        let err = CodexErr::Sandbox(SandboxErr::Denied {
            output: Box::new(output),
        });
        assert_eq!(get_error_message_ui(&err), "stderr detail\nstdout detail");
    }

    #[test]
    fn sandbox_denied_reports_stdout_when_no_stderr() {
        let output = ExecToolCallOutput {
            exit_code: 11,
            stdout: StreamOutput::new("stdout only".to_string()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new(String::new()),
            duration: Duration::from_millis(8),
            timed_out: false,
        };
        let err = CodexErr::Sandbox(SandboxErr::Denied {
            output: Box::new(output),
        });
        assert_eq!(get_error_message_ui(&err), "stdout only");
    }

    #[test]
    fn sandbox_denied_reports_exit_code_when_no_output_available() {
        let output = ExecToolCallOutput {
            exit_code: 13,
            stdout: StreamOutput::new(String::new()),
            stderr: StreamOutput::new(String::new()),
            aggregated_output: StreamOutput::new(String::new()),
            duration: Duration::from_millis(5),
            timed_out: false,
        };
        let err = CodexErr::Sandbox(SandboxErr::Denied {
            output: Box::new(output),
        });
        assert_eq!(
            get_error_message_ui(&err),
            "command failed inside sandbox with exit code 13"
        );
    }

    #[test]
    fn usage_limit_reached_error_formats_free_plan() {
        let err = UsageLimitReachedError {
            plan_type: Some(PlanType::Known(KnownPlan::Free)),
            resets_at: None,
            rate_limits: Some(rate_limit_snapshot()),
        };
        assert_eq!(
            err.to_string(),
            "You've hit your usage limit. Upgrade to Plus to continue using Codex (https://openai.com/chatgpt/pricing)."
        );
    }

    #[test]
    fn usage_limit_reached_error_formats_default_when_none() {
        let err = UsageLimitReachedError {
            plan_type: None,
            resets_at: None,
            rate_limits: Some(rate_limit_snapshot()),
        };
        assert_eq!(
            err.to_string(),
            "You've hit your usage limit. Try again later."
        );
    }

    #[test]
    fn usage_limit_reached_error_formats_team_plan() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resets_at = base + ChronoDuration::hours(1);
        with_now_override(base, move || {
            let err = UsageLimitReachedError {
                plan_type: Some(PlanType::Known(KnownPlan::Team)),
                resets_at: Some(resets_at),
                rate_limits: Some(rate_limit_snapshot()),
            };
            assert_eq!(
                err.to_string(),
                "You've hit your usage limit. To get more access now, send a request to your admin or try again in 1 hour."
            );
        });
    }

    #[test]
    fn usage_limit_reached_error_formats_business_plan_without_reset() {
        let err = UsageLimitReachedError {
            plan_type: Some(PlanType::Known(KnownPlan::Business)),
            resets_at: None,
            rate_limits: Some(rate_limit_snapshot()),
        };
        assert_eq!(
            err.to_string(),
            "You've hit your usage limit. To get more access now, send a request to your admin or try again later."
        );
    }

    #[test]
    fn usage_limit_reached_error_formats_default_for_other_plans() {
        let err = UsageLimitReachedError {
            plan_type: Some(PlanType::Known(KnownPlan::Pro)),
            resets_at: None,
            rate_limits: Some(rate_limit_snapshot()),
        };
        assert_eq!(
            err.to_string(),
            "You've hit your usage limit. Try again later."
        );
    }

    #[test]
    fn usage_limit_reached_includes_minutes_when_available() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resets_at = base + ChronoDuration::minutes(5);
        with_now_override(base, move || {
            let err = UsageLimitReachedError {
                plan_type: None,
                resets_at: Some(resets_at),
                rate_limits: Some(rate_limit_snapshot()),
            };
            assert_eq!(
                err.to_string(),
                "You've hit your usage limit. Try again in 5 minutes."
            );
        });
    }

    #[test]
    fn usage_limit_reached_includes_hours_and_minutes() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resets_at = base + ChronoDuration::hours(3) + ChronoDuration::minutes(32);
        with_now_override(base, move || {
            let err = UsageLimitReachedError {
                plan_type: Some(PlanType::Known(KnownPlan::Plus)),
                resets_at: Some(resets_at),
                rate_limits: Some(rate_limit_snapshot()),
            };
            assert_eq!(
                err.to_string(),
                "You've hit your usage limit. Upgrade to Pro (https://openai.com/chatgpt/pricing) or try again in 3 hours 32 minutes."
            );
        });
    }

    #[test]
    fn usage_limit_reached_includes_days_hours_minutes() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resets_at =
            base + ChronoDuration::days(2) + ChronoDuration::hours(3) + ChronoDuration::minutes(5);
        with_now_override(base, move || {
            let err = UsageLimitReachedError {
                plan_type: None,
                resets_at: Some(resets_at),
                rate_limits: Some(rate_limit_snapshot()),
            };
            assert_eq!(
                err.to_string(),
                "You've hit your usage limit. Try again in 2 days 3 hours 5 minutes."
            );
        });
    }

    #[test]
    fn usage_limit_reached_less_than_minute() {
        let base = Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap();
        let resets_at = base + ChronoDuration::seconds(30);
        with_now_override(base, move || {
            let err = UsageLimitReachedError {
                plan_type: None,
                resets_at: Some(resets_at),
                rate_limits: Some(rate_limit_snapshot()),
            };
            assert_eq!(
                err.to_string(),
                "You've hit your usage limit. Try again in less than a minute."
            );
        });
    }
}
