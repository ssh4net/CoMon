use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
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
    pub async fn spawn(codex_bin: Option<String>, cwd: PathBuf) -> Result<Arc<Self>> {
        let bin = codex_bin
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "codex".to_string());

        let mut command = Command::new(bin);
        command.current_dir(cwd);
        command.arg("app-server");
        command.stdin(Stdio::piped());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());

        let mut child = command
            .spawn()
            .context("Failed to spawn `codex app-server`")?;
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

        let result = value.get("result").cloned().unwrap_or_else(|| Value::Null);
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
}
