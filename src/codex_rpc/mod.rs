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
use tokio::sync::{oneshot, Mutex};
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

#[derive(Debug, Clone)]
pub struct AccountRateLimits {
    pub primary: Option<RateLimitWindow>,
    pub secondary: Option<RateLimitWindow>,
    pub credits: Option<CreditsSnapshot>,
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

        let rpc = Arc::new(Self {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending: Mutex::new(HashMap::new()),
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
                                    }
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
        let rate_limits = result
            .get("rateLimits")
            .or_else(|| result.get("rate_limits"))
            .cloned()
            .unwrap_or(Value::Null);

        parse_rate_limits(&rate_limits)
    }

    pub async fn kill(&self) {
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
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

fn parse_rate_limits(value: &Value) -> Result<AccountRateLimits> {
    let obj = value
        .as_object()
        .ok_or_else(|| anyhow!("rate limits missing"))?;

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

    Ok(AccountRateLimits {
        primary,
        secondary,
        credits,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_rate_limits_accepts_expected_shapes() {
        let v = json!({
            "primary": { "usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1770118764 },
            "secondary": { "used_percent": 1.0, "window_duration_mins": "10080", "resets_at": "1770684472" },
            "credits": { "hasCredits": true, "unlimited": false, "balance": 900.735 },
        });

        let limits = parse_rate_limits(&v).expect("parse_rate_limits");
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
