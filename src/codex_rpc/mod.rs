use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::{oneshot, Mutex};
use tokio::time::timeout;

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

        // stdout reader
        {
            let rpc = Arc::clone(&rpc);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    if line.trim().is_empty() {
                        continue;
                    }
                    let value: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };
                    if let Some(id) = value.get("id").and_then(|v| v.as_u64()) {
                        let has_result_or_error =
                            value.get("result").is_some() || value.get("error").is_some();
                        if has_result_or_error {
                            if let Some(tx) = rpc.pending.lock().await.remove(&id) {
                                let _ = tx.send(value);
                            }
                        }
                    }
                }
            });
        }

        // stderr reader (best-effort; no UI surface yet)
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(_line)) = lines.next_line().await {
                // ignore; could be forwarded to UI later
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

        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);

        self.write_message(json!({ "id": id, "method": method, "params": params }))
            .await?;

        rx.await.map_err(|_| anyhow!("request canceled"))
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
