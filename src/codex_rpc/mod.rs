use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::env;
#[cfg(windows)]
use std::ffi::OsStr;
use std::path::Path;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio::time::timeout;

const MAX_RPC_LINE_BYTES: usize = 1024 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
const MAX_PENDING_REQUESTS: usize = 64;

#[derive(Debug, Clone)]
pub struct RateLimitWindow {
    pub used_percent: Option<f64>,
    pub window_duration_mins: Option<f64>,
    pub resets_at: Option<i64>,
}

#[derive(Debug, Clone)]
pub struct CreditsSnapshot {
    pub has_credits: bool,
    pub unlimited: bool,
    pub balance: Option<String>,
}

/// Detail rows are retained for the planned reset-details screen. The compact summary currently
/// consumes only `expires_at`.
#[allow(dead_code)]
#[derive(Debug, Clone)]
pub struct RateLimitResetCredit {
    pub id: Option<String>,
    pub reset_type: Option<String>,
    pub status: Option<String>,
    pub granted_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub title: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SpendControlLimitSnapshot {
    pub limit: Option<String>,
    pub remaining_percent: Option<f64>,
    pub resets_at: Option<i64>,
    pub used: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RateLimitSnapshot {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
}

#[derive(Debug, Clone)]
pub struct AccountRateLimits {
    pub limit_id: Option<String>,
    pub limit_name: Option<String>,
    pub individual_limit: Option<SpendControlLimitSnapshot>,
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
    pub buckets: Vec<RateLimitSnapshot>,
    pub reset_credits_available: Option<i64>,
    /// `None` means the backend did not return the optional credit detail rows.
    pub reset_credits: Option<Vec<RateLimitResetCredit>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetCreditOutcome {
    Reset,
    NothingToReset,
    NoCredit,
    AlreadyRedeemed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsageSummary {
    pub lifetime_tokens: Option<u64>,
    pub peak_daily_tokens: Option<u64>,
    pub longest_running_turn_sec: Option<u64>,
    pub current_streak_days: Option<u64>,
    pub longest_streak_days: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DailyUsageBucket {
    pub start_date: String,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountUsage {
    pub summary: AccountUsageSummary,
    /// `None` means the backend did not return the optional daily activity series.
    pub daily_usage_buckets: Option<Vec<DailyUsageBucket>>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppServerCommand {
    program: PathBuf,
    args: Vec<String>,
    explicit: bool,
}

impl AppServerCommand {
    fn codex(program: PathBuf, explicit: bool) -> Self {
        Self {
            program,
            args: vec!["app-server".to_string()],
            explicit,
        }
    }

    fn standalone(program: PathBuf, explicit: bool) -> Self {
        Self {
            program,
            args: Vec::new(),
            explicit,
        }
    }

    pub fn display_command(&self) -> String {
        let mut text = self.program.display().to_string();
        for arg in &self.args {
            text.push(' ');
            text.push_str(arg);
        }
        text
    }

    pub fn is_explicit(&self) -> bool {
        self.explicit
    }
}

pub fn resolve_app_server_command(
    codex_bin: Option<String>,
    app_server_bin: Option<PathBuf>,
) -> Option<AppServerCommand> {
    if let Some(path) = non_empty_path(app_server_bin) {
        return Some(AppServerCommand::standalone(path, true));
    }

    if let Some(bin) = codex_bin.and_then(non_empty_string) {
        return Some(AppServerCommand::codex(PathBuf::from(bin), true));
    }

    if let Some(path) = find_command_in_path("codex") {
        return Some(AppServerCommand::codex(path, false));
    }

    find_native_app_server_command()
}

pub fn normalize_epoch_millis(value: i64) -> i64 {
    if value > 0 && value < 1_000_000_000_000 {
        value * 1000
    } else {
        value
    }
}

pub struct CodexRpc {
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    pending: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    notifications: Mutex<mpsc::Receiver<Value>>,
    next_id: Mutex<u64>,
}

impl CodexRpc {
    pub async fn spawn(app_server: AppServerCommand, cwd: PathBuf) -> Result<Arc<Self>> {
        let display_command = app_server.display_command();
        let mut command = Command::new(&app_server.program);
        command.current_dir(cwd);
        command.args(&app_server.args);
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn `{display_command}`"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("missing stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("missing stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("missing stderr"))?;

        let (notification_tx, notification_rx) = mpsc::channel(MAX_PENDING_REQUESTS);
        let rpc = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
            notifications: Mutex::new(notification_rx),
            next_id: Mutex::new(1),
        });

        // stdout reader (line-length limited)
        {
            let rpc = Arc::clone(&rpc);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                let mut buf = Vec::<u8>::with_capacity(8 * 1024);
                let mut tmp = [0u8; 8 * 1024];

                loop {
                    match reader.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            buf.extend_from_slice(&tmp[..n]);

                            // Process complete lines.
                            while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                                let mut line = buf.drain(..=pos).collect::<Vec<u8>>();
                                if let Some(b'\n') = line.last() {
                                    line.pop();
                                }
                                if let Some(b'\r') = line.last() {
                                    line.pop();
                                }

                                if line.is_empty() {
                                    continue;
                                }
                                if line.len() > MAX_RPC_LINE_BYTES {
                                    // Protocol violation / malicious server: stop processing.
                                    let mut child = rpc.child.lock().await;
                                    let _ = child.kill().await;
                                    return;
                                }

                                let value: Value = match serde_json::from_slice(&line) {
                                    Ok(v) => v,
                                    Err(_) => continue,
                                };
                                if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                                    let has_result_or_error = value.get("result").is_some()
                                        || value.get("error").is_some();
                                    if has_result_or_error {
                                        if let Some(tx) = rpc.pending.lock().await.remove(&id) {
                                            let _ = tx.send(value);
                                        }
                                    } else if value.get("method").is_some() {
                                        let _ = notification_tx.try_send(value);
                                    }
                                } else if value.get("method").is_some() {
                                    let _ = notification_tx.try_send(value);
                                }
                            }

                            // Protect against unbounded growth if a malicious server omits newlines.
                            if buf.len() > MAX_RPC_LINE_BYTES * 2 {
                                let mut child = rpc.child.lock().await;
                                let _ = child.kill().await;
                                return;
                            }
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // stderr reader (best-effort; no UI surface yet)
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut tmp = [0u8; 8 * 1024];
            loop {
                if reader.read(&mut tmp).await.unwrap_or(0) == 0 {
                    break;
                }
            }
        });

        rpc.initialize().await?;
        Ok(rpc)
    }

    async fn initialize(self: &Arc<Self>) -> Result<()> {
        let init_params = json!({
            "clientInfo": {
                "name": "comon",
                "title": "comon",
                "version": env!("CARGO_PKG_VERSION")
            }
        });
        let init = timeout(
            Duration::from_secs(15),
            self.send_request("initialize", init_params),
        )
        .await
        .map_err(|_| anyhow!("Codex app-server did not respond to initialize within 15s"))??;

        if init.get("error").is_some() {
            return Err(anyhow!(
                "Codex app-server initialize returned error: {init}"
            ));
        }

        self.send_notification("initialized", None).await?;
        Ok(())
    }

    async fn write_message(&self, value: Value) -> Result<()> {
        let mut stdin = self.stdin.lock().await;
        let mut line = serde_json::to_string(&value)?;
        line.push('\n');
        stdin.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub async fn send_request(&self, method: &str, params: Value) -> Result<Value> {
        let id = {
            let mut next = self.next_id.lock().await;
            let id = *next;
            *next = next.saturating_add(1);
            id
        };

        {
            let pending_len = self.pending.lock().await.len();
            if pending_len >= MAX_PENDING_REQUESTS {
                return Err(anyhow!("too many pending RPC requests"));
            }
        }

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        if let Err(err) = self
            .write_message(json!({ "id": id, "method": method, "params": params }))
            .await
        {
            let _ = self.pending.lock().await.remove(&id);
            return Err(err);
        }

        match timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(v)) => Ok(v),
            Ok(Err(_)) => Err(anyhow!("request canceled")),
            Err(_) => {
                let _ = self.pending.lock().await.remove(&id);
                Err(anyhow!(
                    "RPC request `{method}` timeout after {:?}",
                    REQUEST_TIMEOUT
                ))
            }
        }
    }

    pub async fn send_notification(&self, method: &str, params: Option<Value>) -> Result<()> {
        let value = if let Some(params) = params {
            json!({ "method": method, "params": params })
        } else {
            json!({ "method": method })
        };
        self.write_message(value).await
    }

    pub async fn read_account_rate_limits(&self) -> Result<AccountRateLimits> {
        let value = self
            .send_request("account/rateLimits/read", Value::Null)
            .await?;

        if let Some(err) = value.get("error") {
            return Err(anyhow!("account/rateLimits/read error: {err}"));
        }

        let result = value.get("result").cloned().unwrap_or(Value::Null);
        parse_account_rate_limits_result(&result)
    }

    pub async fn read_account_usage(&self) -> Result<AccountUsage> {
        let value = self.send_request("account/usage/read", Value::Null).await?;

        if let Some(err) = value.get("error") {
            return Err(anyhow!("account/usage/read error: {err}"));
        }

        let result = value.get("result").cloned().unwrap_or(Value::Null);
        parse_account_usage_result(&result)
    }

    pub async fn consume_account_rate_limit_reset_credit(
        &self,
        idempotency_key: &str,
    ) -> Result<ResetCreditOutcome> {
        let value = self
            .send_request(
                "account/rateLimitResetCredit/consume",
                json!({ "idempotencyKey": idempotency_key }),
            )
            .await?;

        if let Some(err) = value.get("error") {
            return Err(anyhow!("account/rateLimitResetCredit/consume error: {err}"));
        }

        let result = value.get("result").cloned().unwrap_or(Value::Null);
        parse_reset_credit_outcome(&result)
    }

    pub async fn recv_notification(&self) -> Option<Value> {
        self.notifications.lock().await.recv().await
    }

    pub async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

pub fn is_account_rate_limits_updated_notification(value: &Value) -> bool {
    value.get("method").and_then(|v| v.as_str()) == Some("account/rateLimits/updated")
}

fn parse_reset_credit_outcome(result: &Value) -> Result<ResetCreditOutcome> {
    match result.get("outcome").and_then(Value::as_str) {
        Some("reset") => Ok(ResetCreditOutcome::Reset),
        Some("nothingToReset") => Ok(ResetCreditOutcome::NothingToReset),
        Some("noCredit") => Ok(ResetCreditOutcome::NoCredit),
        Some("alreadyRedeemed") => Ok(ResetCreditOutcome::AlreadyRedeemed),
        Some(other) => Err(anyhow!("unknown reset-credit outcome: {other}")),
        None => Err(anyhow!("reset-credit outcome missing")),
    }
}

fn non_empty_string(value: String) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn non_empty_path(path: Option<PathBuf>) -> Option<PathBuf> {
    path.filter(|value| !value.as_os_str().is_empty())
}

fn find_command_in_path(command: &str) -> Option<PathBuf> {
    let command_path = Path::new(command);
    if command_path.components().count() > 1 || command_path.is_absolute() {
        return executable_file_exists(command_path).then(|| command_path.to_path_buf());
    }

    let path_value = env::var_os("PATH")?;
    for dir in env::split_paths(&path_value) {
        for candidate in command_candidates(&dir, command) {
            if executable_file_exists(&candidate) {
                return Some(candidate);
            }
        }
    }
    None
}

fn executable_file_exists(path: &Path) -> bool {
    std::fs::metadata(path)
        .map(|meta| meta.is_file())
        .unwrap_or(false)
}

#[cfg(windows)]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    let command_path = Path::new(command);
    if command_path.extension().is_some() {
        return vec![dir.join(command)];
    }

    let mut extensions = Vec::<String>::new();
    if let Some(pathext) = env::var_os("PATHEXT") {
        for ext in pathext.to_string_lossy().split(';') {
            let trimmed = ext.trim();
            if !trimmed.is_empty() {
                extensions.push(trimmed.to_string());
            }
        }
    }
    if extensions.is_empty() {
        extensions.extend(
            [".exe", ".cmd", ".bat", ".com"]
                .iter()
                .map(|ext| ext.to_string()),
        );
    }

    extensions
        .into_iter()
        .map(|ext| dir.join(format!("{command}{ext}")))
        .collect()
}

#[cfg(not(windows))]
fn command_candidates(dir: &Path, command: &str) -> Vec<PathBuf> {
    vec![dir.join(command)]
}

#[cfg(windows)]
fn find_native_app_server_command() -> Option<AppServerCommand> {
    for path in native_app_candidate_roots() {
        if let Some(command) = find_app_server_under_root(&path) {
            return Some(command);
        }
    }
    None
}

#[cfg(not(windows))]
fn find_native_app_server_command() -> Option<AppServerCommand> {
    None
}

#[cfg(windows)]
fn native_app_candidate_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    push_env_child_roots(
        &mut roots,
        "LOCALAPPDATA",
        &[
            "Codex",
            "OpenAI\\Codex",
            "Programs\\Codex",
            "Programs\\Codex App",
            "Programs\\OpenAI Codex",
            "Programs\\OpenAI\\Codex",
        ],
    );
    push_env_child_roots(
        &mut roots,
        "ProgramFiles",
        &["Codex", "Codex App", "OpenAI Codex", "OpenAI\\Codex"],
    );
    push_env_child_roots(
        &mut roots,
        "ProgramFiles(x86)",
        &["Codex", "Codex App", "OpenAI Codex", "OpenAI\\Codex"],
    );
    roots
}

#[cfg(windows)]
fn push_env_child_roots(roots: &mut Vec<PathBuf>, env_key: &str, children: &[&str]) {
    let Some(base) = env::var_os(env_key) else {
        return;
    };
    if base.is_empty() {
        return;
    }
    let base = PathBuf::from(base);
    for child in children {
        roots.push(base.join(child));
    }
}

#[cfg(windows)]
fn find_app_server_under_root(root: &Path) -> Option<AppServerCommand> {
    if !root.exists() {
        return None;
    }

    const DIRECT_CODEX_REL: &[&str] = &[
        "bin\\codex.exe",
        "resources\\codex.exe",
        "resources\\bin\\codex.exe",
        "resources\\app\\codex.exe",
        "resources\\app\\bin\\codex.exe",
        "resources\\app.asar.unpacked\\codex.exe",
        "resources\\app.asar.unpacked\\bin\\codex.exe",
    ];
    const DIRECT_STANDALONE_REL: &[&str] = &[
        "app-server.exe",
        "codex-app-server.exe",
        "bin\\app-server.exe",
        "bin\\codex-app-server.exe",
        "resources\\app-server.exe",
        "resources\\codex-app-server.exe",
        "resources\\bin\\app-server.exe",
        "resources\\bin\\codex-app-server.exe",
        "resources\\app.asar.unpacked\\app-server.exe",
        "resources\\app.asar.unpacked\\codex-app-server.exe",
        "resources\\app.asar.unpacked\\bin\\app-server.exe",
        "resources\\app.asar.unpacked\\bin\\codex-app-server.exe",
    ];

    for rel in DIRECT_CODEX_REL {
        let path = root.join(rel);
        if executable_file_exists(&path) {
            return Some(AppServerCommand::codex(path, false));
        }
    }
    for rel in DIRECT_STANDALONE_REL {
        let path = root.join(rel);
        if executable_file_exists(&path) {
            return Some(AppServerCommand::standalone(path, false));
        }
    }

    find_app_server_recursive(root, 0, &mut 0)
}

#[cfg(windows)]
fn find_app_server_recursive(
    dir: &Path,
    depth: usize,
    visited_entries: &mut usize,
) -> Option<AppServerCommand> {
    const MAX_DEPTH: usize = 6;
    const MAX_ENTRIES: usize = 2048;
    if depth > MAX_DEPTH || *visited_entries >= MAX_ENTRIES {
        return None;
    }

    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        *visited_entries = (*visited_entries).saturating_add(1);
        if *visited_entries >= MAX_ENTRIES {
            return None;
        }
        let path = entry.path();
        let file_name = path.file_name().and_then(OsStr::to_str).unwrap_or("");
        if file_name == "codex.exe" && executable_file_exists(&path) {
            return Some(AppServerCommand::codex(path, false));
        }
        if (file_name.eq_ignore_ascii_case("app-server.exe")
            || file_name.eq_ignore_ascii_case("codex-app-server.exe"))
            && executable_file_exists(&path)
        {
            return Some(AppServerCommand::standalone(path, false));
        }
        if path.is_dir() {
            if let Some(found) = find_app_server_recursive(&path, depth + 1, visited_entries) {
                return Some(found);
            }
        }
    }
    None
}

fn parse_account_rate_limits_result(result: &Value) -> Result<AccountRateLimits> {
    let rate_limits = result
        .get("rateLimits")
        .or_else(|| result.get("rate_limits"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut limits = parse_rate_limits(&rate_limits)?;

    limits.reset_credits_available = result
        .get("rateLimitResetCredits")
        .or_else(|| result.get("rate_limit_reset_credits"))
        .and_then(|v| v.as_object())
        .and_then(|v| {
            v.get("availableCount")
                .or_else(|| v.get("available_count"))
                .and_then(as_i64_maybe_float)
        });

    limits.reset_credits = result
        .get("rateLimitResetCredits")
        .or_else(|| result.get("rate_limit_reset_credits"))
        .and_then(|v| v.as_object())
        .and_then(|v| v.get("credits"))
        .and_then(|v| v.as_array())
        .map(|credits| {
            credits
                .iter()
                .filter_map(parse_rate_limit_reset_credit)
                .collect()
        });

    if let Some(by_limit_id) = result
        .get("rateLimitsByLimitId")
        .or_else(|| result.get("rate_limits_by_limit_id"))
        .and_then(|v| v.as_object())
    {
        let mut buckets = Vec::<RateLimitSnapshot>::with_capacity(by_limit_id.len());
        for (key, value) in by_limit_id {
            if let Some(mut snapshot) = parse_rate_limit_snapshot(value) {
                if snapshot.limit_id.is_none() {
                    snapshot.limit_id = Some(key.clone());
                }
                buckets.push(snapshot);
            }
        }
        buckets.sort_by(|a, b| a.limit_id.cmp(&b.limit_id));
        limits.buckets = buckets;
    }

    Ok(limits)
}

fn parse_account_usage_result(result: &Value) -> Result<AccountUsage> {
    let result = result
        .as_object()
        .ok_or_else(|| anyhow!("account usage result missing"))?;
    let summary = result.get("summary").and_then(Value::as_object);
    let read_summary_u64 = |camel: &str, snake: &str| {
        summary
            .and_then(|value| value.get(camel).or_else(|| value.get(snake)))
            .and_then(as_u64_maybe_float)
    };

    let mut daily_usage_buckets = result
        .get("dailyUsageBuckets")
        .or_else(|| result.get("daily_usage_buckets"))
        .and_then(Value::as_array)
        .map(|buckets| {
            buckets
                .iter()
                .filter_map(parse_daily_usage_bucket)
                .collect::<Vec<_>>()
        });
    if let Some(buckets) = daily_usage_buckets.as_mut() {
        buckets.sort_by(|left, right| left.start_date.cmp(&right.start_date));
    }

    Ok(AccountUsage {
        summary: AccountUsageSummary {
            lifetime_tokens: read_summary_u64("lifetimeTokens", "lifetime_tokens"),
            peak_daily_tokens: read_summary_u64("peakDailyTokens", "peak_daily_tokens"),
            longest_running_turn_sec: read_summary_u64(
                "longestRunningTurnSec",
                "longest_running_turn_sec",
            ),
            current_streak_days: read_summary_u64("currentStreakDays", "current_streak_days"),
            longest_streak_days: read_summary_u64("longestStreakDays", "longest_streak_days"),
        },
        daily_usage_buckets,
    })
}

fn parse_daily_usage_bucket(value: &Value) -> Option<DailyUsageBucket> {
    let value = value.as_object()?;
    let start_date = read_string(value.get("startDate").or_else(|| value.get("start_date")))?;
    let tokens = value.get("tokens").and_then(as_u64_maybe_float)?;
    Some(DailyUsageBucket { start_date, tokens })
}

fn parse_rate_limits(value: &Value) -> Result<AccountRateLimits> {
    let snapshot =
        parse_rate_limit_snapshot(value).ok_or_else(|| anyhow!("rate limits missing"))?;

    Ok(AccountRateLimits {
        limit_id: snapshot.limit_id.clone(),
        limit_name: snapshot.limit_name.clone(),
        individual_limit: snapshot.individual_limit.clone(),
        primary: snapshot.primary.clone(),
        secondary: snapshot.secondary.clone(),
        credits: snapshot.credits.clone(),
        buckets: Vec::new(),
        reset_credits_available: None,
        reset_credits: None,
    })
}

fn parse_rate_limit_reset_credit(value: &Value) -> Option<RateLimitResetCredit> {
    let obj = value.as_object()?;
    Some(RateLimitResetCredit {
        id: read_string(obj.get("id")),
        reset_type: read_string(obj.get("resetType").or_else(|| obj.get("reset_type"))),
        status: read_string(obj.get("status")),
        granted_at: obj
            .get("grantedAt")
            .or_else(|| obj.get("granted_at"))
            .and_then(as_i64_maybe_float),
        expires_at: obj
            .get("expiresAt")
            .or_else(|| obj.get("expires_at"))
            .and_then(as_i64_maybe_float),
        title: read_string(obj.get("title")),
        description: read_string(obj.get("description")),
    })
}

fn parse_rate_limit_snapshot(value: &Value) -> Option<RateLimitSnapshot> {
    let obj = value.as_object().filter(|_| !value.is_null())?;

    let limit_id = read_string(obj.get("limitId").or_else(|| obj.get("limit_id")));
    let limit_name = read_string(obj.get("limitName").or_else(|| obj.get("limit_name")));
    let individual_limit = obj
        .get("individualLimit")
        .or_else(|| obj.get("individual_limit"))
        .and_then(parse_individual_limit);
    let primary = obj.get("primary").and_then(parse_window);
    let secondary = obj.get("secondary").and_then(parse_window);

    let credits = obj.get("credits").and_then(|v| v.as_object()).map(|c| {
        let has_credits = c
            .get("hasCredits")
            .or_else(|| c.get("has_credits"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let unlimited = c
            .get("unlimited")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let balance = c.get("balance").and_then(|v| {
            v.as_str()
                .map(|s| s.to_string())
                .or_else(|| v.as_i64().map(|n| n.to_string()))
                .or_else(|| v.as_f64().map(|n| n.to_string()))
        });
        CreditsSnapshot {
            has_credits,
            unlimited,
            balance,
        }
    });

    Some(RateLimitSnapshot {
        limit_id,
        limit_name,
        individual_limit,
        primary,
        secondary,
        credits,
    })
}

fn parse_individual_limit(value: &Value) -> Option<SpendControlLimitSnapshot> {
    let obj = value.as_object()?;
    Some(SpendControlLimitSnapshot {
        limit: read_scalar_string(obj.get("limit")),
        remaining_percent: obj
            .get("remainingPercent")
            .or_else(|| obj.get("remaining_percent"))
            .and_then(as_f64),
        resets_at: obj
            .get("resetsAt")
            .or_else(|| obj.get("resets_at"))
            .and_then(as_i64_maybe_float),
        used: read_scalar_string(obj.get("used")),
    })
}

fn parse_window(value: &Value) -> Option<RateLimitWindow> {
    let obj = value.as_object()?;
    let used_percent = obj
        .get("usedPercent")
        .or_else(|| obj.get("used_percent"))
        .and_then(as_f64);
    let window_duration_mins = obj
        .get("windowDurationMins")
        .or_else(|| obj.get("window_duration_mins"))
        .or_else(|| obj.get("window_minutes"))
        .and_then(as_f64);
    let resets_at = obj
        .get("resetsAt")
        .or_else(|| obj.get("resets_at"))
        .and_then(as_i64_maybe_float);

    Some(RateLimitWindow {
        used_percent,
        window_duration_mins,
        resets_at,
    })
}

fn read_string(value: Option<&Value>) -> Option<String> {
    let value = value?.as_str()?.trim();
    (!value.is_empty()).then(|| value.to_string())
}

fn read_scalar_string(value: Option<&Value>) -> Option<String> {
    let value = value?;
    value
        .as_str()
        .map(|s| s.trim().to_string())
        .or_else(|| value.as_i64().map(|n| n.to_string()))
        .or_else(|| value.as_f64().map(|n| n.to_string()))
        .filter(|s| !s.is_empty())
}

fn as_f64(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_i64().map(|v| v as f64))
        .or_else(|| value.as_str().and_then(|s| s.parse::<f64>().ok()))
}

fn as_i64_maybe_float(value: &Value) -> Option<i64> {
    value
        .as_i64()
        .or_else(|| value.as_f64().map(|v| v as i64))
        .or_else(|| value.as_str().and_then(|s| s.parse::<i64>().ok()))
}

fn as_u64_maybe_float(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_str().and_then(|number| number.parse::<u64>().ok()))
        .or_else(|| {
            value.as_f64().and_then(|number| {
                (number.is_finite() && number >= 0.0 && number <= u64::MAX as f64)
                    .then(|| number.round() as u64)
            })
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_rate_limits_accepts_expected_shapes() {
        let v = json!({
            "limitId": "codex",
            "primary": { "usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1770118764 },
            "secondary": { "used_percent": 1.0, "window_minutes": "10080", "resets_at": "1770684472" },
            "credits": { "hasCredits": true, "unlimited": false, "balance": 900.735 },
        });

        let limits = parse_rate_limits(&v).expect("parse_rate_limits");
        assert_eq!(limits.limit_id.as_deref(), Some("codex"));
        let p = limits.primary.expect("primary");
        assert_eq!(p.used_percent, Some(0.0));
        assert_eq!(p.window_duration_mins, Some(300.0));
        assert_eq!(p.resets_at, Some(1770118764));

        let s = limits.secondary.expect("secondary");
        assert_eq!(s.used_percent, Some(1.0));
        assert_eq!(s.window_duration_mins, Some(10080.0));
        assert_eq!(s.resets_at, Some(1770684472));

        let c = limits.credits.expect("credits");
        assert!(c.has_credits);
        assert!(!c.unlimited);
        assert_eq!(c.balance.as_deref(), Some("900.735"));
    }

    #[test]
    fn parse_account_rate_limits_result_accepts_multi_bucket_response() {
        let v = json!({
            "rateLimitResetCredits": {
                "availableCount": 2,
                "credits": [{
                    "id": "credit-1",
                    "resetType": "codexRateLimits",
                    "status": "available",
                    "grantedAt": 1781742782,
                    "expiresAt": 1784334782,
                    "title": "Full reset (Weekly + 5 hr)",
                    "description": "Restores rolling limits"
                }]
            },
            "rateLimits": {
                "limitId": "codex",
                "individualLimit": {
                    "limit": "60000",
                    "remainingPercent": 99,
                    "resetsAt": 1785523200,
                    "used": "564"
                }
            },
            "rateLimitsByLimitId": {
                "codex_extra": {
                    "limitName": "GPT-5.3-Codex-Spark",
                    "primary": { "usedPercent": 30, "windowDurationMins": 300 },
                    "secondary": { "usedPercent": 40, "windowDurationMins": 10080 }
                },
                "codex": {
                    "individualLimit": {
                        "limit": "60000",
                        "remainingPercent": 99,
                        "resetsAt": 1785523200,
                        "used": "564"
                    }
                }
            }
        });

        let limits =
            parse_account_rate_limits_result(&v).expect("parse_account_rate_limits_result");
        assert_eq!(limits.limit_id.as_deref(), Some("codex"));
        let individual_limit = limits.individual_limit.expect("individual limit");
        assert_eq!(individual_limit.limit.as_deref(), Some("60000"));
        assert_eq!(individual_limit.remaining_percent, Some(99.0));
        assert_eq!(individual_limit.resets_at, Some(1785523200));
        assert_eq!(individual_limit.used.as_deref(), Some("564"));
        assert_eq!(limits.reset_credits_available, Some(2));
        let credit = limits
            .reset_credits
            .as_deref()
            .and_then(|credits| credits.first())
            .expect("reset credit details");
        assert_eq!(credit.id.as_deref(), Some("credit-1"));
        assert_eq!(credit.reset_type.as_deref(), Some("codexRateLimits"));
        assert_eq!(credit.status.as_deref(), Some("available"));
        assert_eq!(credit.granted_at, Some(1781742782));
        assert_eq!(credit.expires_at, Some(1784334782));
        assert_eq!(credit.title.as_deref(), Some("Full reset (Weekly + 5 hr)"));
        assert_eq!(
            credit.description.as_deref(),
            Some("Restores rolling limits")
        );
        assert_eq!(limits.buckets.len(), 2);
        assert_eq!(limits.buckets[0].limit_id.as_deref(), Some("codex"));
        assert_eq!(limits.buckets[1].limit_id.as_deref(), Some("codex_extra"));
        assert_eq!(
            limits.buckets[1].limit_name.as_deref(),
            Some("GPT-5.3-Codex-Spark")
        );
    }

    #[test]
    fn parses_all_reset_credit_outcomes() {
        for (raw, expected) in [
            ("reset", ResetCreditOutcome::Reset),
            ("nothingToReset", ResetCreditOutcome::NothingToReset),
            ("noCredit", ResetCreditOutcome::NoCredit),
            ("alreadyRedeemed", ResetCreditOutcome::AlreadyRedeemed),
        ] {
            assert_eq!(
                parse_reset_credit_outcome(&json!({ "outcome": raw })).expect("outcome"),
                expected
            );
        }
    }

    #[test]
    fn rejects_unknown_or_missing_reset_credit_outcome() {
        assert!(parse_reset_credit_outcome(&json!({ "outcome": "futureValue" })).is_err());
        assert!(parse_reset_credit_outcome(&json!({})).is_err());
    }

    #[test]
    fn parse_account_usage_result_accepts_summary_and_sorts_daily_buckets() {
        let value = json!({
            "summary": {
                "lifetimeTokens": 8_206_591_409_u64,
                "peakDailyTokens": 773_124_581_u64,
                "longestRunningTurnSec": 42_168,
                "currentStreakDays": 2,
                "longestStreakDays": 21
            },
            "dailyUsageBuckets": [
                { "startDate": "2026-07-23", "tokens": 97_465_847 },
                { "startDate": "2026-07-22", "tokens": 215_392_194 }
            ]
        });

        let usage = parse_account_usage_result(&value).expect("parse account usage");
        assert_eq!(usage.summary.lifetime_tokens, Some(8_206_591_409));
        assert_eq!(usage.summary.peak_daily_tokens, Some(773_124_581));
        assert_eq!(usage.summary.longest_running_turn_sec, Some(42_168));
        assert_eq!(usage.summary.current_streak_days, Some(2));
        assert_eq!(usage.summary.longest_streak_days, Some(21));
        let buckets = usage.daily_usage_buckets.expect("daily buckets");
        assert_eq!(buckets.len(), 2);
        assert_eq!(buckets[0].start_date, "2026-07-22");
        assert_eq!(buckets[1].tokens, 97_465_847);
    }

    #[test]
    fn parse_account_usage_result_preserves_nullable_optional_fields() {
        let value = json!({
            "summary": {
                "lifetimeTokens": null,
                "peak_daily_tokens": 1234
            },
            "dailyUsageBuckets": null
        });

        let usage = parse_account_usage_result(&value).expect("parse account usage");
        assert_eq!(usage.summary.lifetime_tokens, None);
        assert_eq!(usage.summary.peak_daily_tokens, Some(1234));
        assert_eq!(usage.daily_usage_buckets, None);
    }

    #[test]
    fn recognizes_account_rate_limits_updated_notification() {
        let v = json!({
            "method": "account/rateLimits/updated",
            "params": { "rateLimits": { "primary": { "usedPercent": 3 } } }
        });
        assert!(is_account_rate_limits_updated_notification(&v));
    }

    #[test]
    fn parse_rate_limits_rejects_non_object() {
        let err = parse_rate_limits(&Value::Null).unwrap_err().to_string();
        assert!(err.contains("rate limits missing"));
    }

    #[test]
    fn explicit_codex_bin_uses_app_server_subcommand() {
        let command =
            resolve_app_server_command(Some("custom-codex".to_string()), None).expect("command");
        assert_eq!(command.program, PathBuf::from("custom-codex"));
        assert_eq!(command.args, vec!["app-server".to_string()]);
        assert!(command.is_explicit());
    }

    #[test]
    fn explicit_app_server_bin_is_standalone() {
        let path = PathBuf::from("custom-app-server");
        let command = resolve_app_server_command(None, Some(path.clone())).expect("command");
        assert_eq!(command.program, path);
        assert!(command.args.is_empty());
        assert!(command.is_explicit());
    }
}
