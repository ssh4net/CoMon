use crate::usage::{
    normalize_project_key, project_identity_from_path, project_identity_from_tool_call,
    PROJECT_IDENTITY_LINE_LIMIT,
};
use anyhow::{Context, Result};
use chrono::{DateTime, Local, Utc};
use serde_json::Value;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_TITLE_CHARS: usize = 96;
const MAX_TURN_PREVIEW_CHARS: usize = 220;

#[derive(Debug, Clone)]
pub(crate) struct Catalog {
    pub(crate) sessions_dir: PathBuf,
    pub(crate) projects: Vec<ProjectRecord>,
    pub(crate) files_scanned: usize,
    pub(crate) files_skipped: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct ProjectRecord {
    pub(crate) display_path: String,
    pub(crate) sessions: Vec<SessionSummary>,
}

#[derive(Debug, Clone)]
pub(crate) struct SessionSummary {
    pub(crate) file_path: PathBuf,
    pub(crate) session_id: String,
    pub(crate) cwd: String,
    pub(crate) title: String,
    pub(crate) started_at_raw: Option<String>,
    pub(crate) started_at_label: String,
    pub(crate) started_at_sort_key_ms: i64,
    pub(crate) git_branch: Option<String>,
    pub(crate) git_commit: Option<String>,
    pub(crate) repo_url: Option<String>,
    pub(crate) model_provider: Option<String>,
    pub(crate) model: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct SessionDetail {
    pub(crate) meaningful_user_turns: Vec<String>,
    pub(crate) all_user_turns: Vec<String>,
    pub(crate) assistant_messages: usize,
    pub(crate) tool_calls: usize,
    pub(crate) tool_outputs: usize,
    pub(crate) input_images: usize,
    pub(crate) total_tokens: Option<i64>,
    pub(crate) input_tokens: Option<i64>,
    pub(crate) output_tokens: Option<i64>,
    pub(crate) reasoning_encrypted: bool,
}

#[derive(Debug, Default)]
struct ProjectBuilder {
    sessions: Vec<SessionSummary>,
}

#[derive(Debug, Default)]
struct SessionSummaryBuilder {
    file_path: PathBuf,
    session_id: Option<String>,
    cwd: Option<String>,
    first_user_text: Option<String>,
    meaningful_title: Option<String>,
    started_at_raw: Option<String>,
    started_at_sort_key_ms: i64,
    git_branch: Option<String>,
    git_commit: Option<String>,
    repo_url: Option<String>,
    model_provider: Option<String>,
    model: Option<String>,
    project_cwds: BTreeMap<String, ProjectPathCandidate>,
}

#[derive(Debug, Clone, Default)]
struct ProjectPathCandidate {
    display_path: String,
    count: u32,
}

pub(crate) fn build_catalog(sessions_dir: &Path) -> Result<Catalog> {
    if !sessions_dir.exists() {
        return Ok(Catalog {
            sessions_dir: sessions_dir.to_path_buf(),
            projects: Vec::new(),
            files_scanned: 0,
            files_skipped: 0,
        });
    }

    let mut candidates = Vec::new();
    collect_session_files(sessions_dir, &mut candidates)?;
    candidates.sort();

    let mut grouped: BTreeMap<String, ProjectBuilder> = BTreeMap::new();
    let mut files_scanned = 0usize;
    let mut files_skipped = 0usize;

    for path in candidates {
        match scan_session_summary(&path) {
            Ok(summary) => {
                files_scanned += 1;
                let key = normalize_project_key(&summary.cwd);
                grouped.entry(key).or_default().sessions.push(summary);
            }
            Err(_) => {
                files_skipped += 1;
            }
        }
    }

    let mut projects = Vec::with_capacity(grouped.len());
    for (_, mut builder) in grouped {
        builder.sessions.sort_by(|left, right| {
            right
                .started_at_sort_key_ms
                .cmp(&left.started_at_sort_key_ms)
                .then_with(|| left.file_path.cmp(&right.file_path))
        });
        let display_path = builder
            .sessions
            .first()
            .map(|session| session.cwd.clone())
            .unwrap_or_else(|| "<unknown>".to_string());
        projects.push(ProjectRecord {
            display_path,
            sessions: builder.sessions,
        });
    }

    projects.sort_by(|left, right| left.display_path.cmp(&right.display_path));

    Ok(Catalog {
        sessions_dir: sessions_dir.to_path_buf(),
        projects,
        files_scanned,
        files_skipped,
    })
}

pub(crate) fn load_session_detail(path: &Path) -> Result<SessionDetail> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut detail = SessionDetail::default();

    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            break;
        }

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").and_then(Value::as_object);

        match entry_type {
            "response_item" => {
                if let Some(payload) = payload {
                    let payload_type = payload.get("type").and_then(Value::as_str).unwrap_or("");
                    match payload_type {
                        "message" => {
                            let role = payload.get("role").and_then(Value::as_str).unwrap_or("");
                            if role == "user" {
                                let texts = extract_message_texts(payload, "input_text");
                                for text in texts {
                                    if is_meaningful_user_text(&text) {
                                        detail.meaningful_user_turns.push(truncate_single_line(
                                            &text,
                                            MAX_TURN_PREVIEW_CHARS,
                                        ));
                                    }
                                    detail
                                        .all_user_turns
                                        .push(truncate_single_line(&text, MAX_TURN_PREVIEW_CHARS));
                                }
                                detail.input_images += count_message_parts(payload, "input_image");
                            } else if role == "assistant"
                                && has_message_part(payload, "output_text")
                            {
                                detail.assistant_messages += 1;
                            }
                        }
                        "function_call" => {
                            detail.tool_calls += 1;
                        }
                        "function_call_output" => {
                            detail.tool_outputs += 1;
                        }
                        "reasoning" => {
                            if payload
                                .get("encrypted_content")
                                .and_then(Value::as_str)
                                .is_some()
                            {
                                detail.reasoning_encrypted = true;
                            }
                        }
                        _ => {}
                    }
                }
            }
            "event_msg" => {
                if let Some(payload) = payload {
                    if payload.get("type").and_then(Value::as_str) == Some("token_count") {
                        if let Some(info) = payload.get("info") {
                            if let Some(total_usage) =
                                find_nested_map(info, &["total_token_usage", "totalTokenUsage"])
                            {
                                detail.total_tokens = read_i64(total_usage.get("total_tokens"))
                                    .or_else(|| read_i64(total_usage.get("totalTokens")));
                                detail.input_tokens = read_i64(total_usage.get("input_tokens"))
                                    .or_else(|| read_i64(total_usage.get("inputTokens")));
                                detail.output_tokens = read_i64(total_usage.get("output_tokens"))
                                    .or_else(|| read_i64(total_usage.get("outputTokens")));
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }

    Ok(detail)
}

fn collect_session_files(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = std::fs::read_dir(&dir)
            .with_context(|| format!("Unable to read directory {}", dir.display()))?;
        for entry_result in entries {
            let entry = entry_result
                .with_context(|| format!("Unable to read entry in {}", dir.display()))?;
            let path = entry.path();
            let meta = std::fs::symlink_metadata(&path)
                .with_context(|| format!("Unable to inspect {}", path.display()))?;
            let file_type = meta.file_type();
            if file_type.is_symlink() {
                continue;
            }
            if file_type.is_dir() {
                stack.push(path);
                continue;
            }
            if !file_type.is_file() {
                continue;
            }
            if path.extension().and_then(|ext| ext.to_str()) == Some("jsonl") {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn scan_session_summary(path: &Path) -> Result<SessionSummary> {
    let file = File::open(path).with_context(|| format!("Unable to open {}", path.display()))?;
    let mut reader = BufReader::new(file);
    let mut line = String::new();
    let mut builder = SessionSummaryBuilder {
        file_path: path.to_path_buf(),
        ..SessionSummaryBuilder::default()
    };

    let mut lines_seen = 0usize;
    loop {
        line.clear();
        if reader
            .read_line(&mut line)
            .with_context(|| format!("Unable to read {}", path.display()))?
            == 0
        {
            break;
        }
        lines_seen += 1;

        let value = match serde_json::from_str::<Value>(&line) {
            Ok(value) => value,
            Err(_) => continue,
        };

        let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
        match entry_type {
            "session_meta" => extract_session_meta(&mut builder, &value),
            "turn_context" => extract_turn_context(&mut builder, &value),
            "response_item" => {
                extract_title_candidate(&mut builder, &value);
                if let Some(project) =
                    project_identity_from_tool_call(&value, builder.cwd.as_deref())
                {
                    builder.note_project_cwd(project);
                }
            }
            _ => {}
        }

        if lines_seen >= PROJECT_IDENTITY_LINE_LIMIT && builder.has_minimum_fields() {
            break;
        }
    }

    builder.finish()
}

fn extract_session_meta(builder: &mut SessionSummaryBuilder, value: &Value) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    if builder.session_id.is_none() {
        builder.session_id = payload
            .get("id")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    if builder.cwd.is_none() {
        builder.cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    if builder.model_provider.is_none() {
        builder.model_provider = payload
            .get("model_provider")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    if builder.started_at_raw.is_none() {
        if let Some(timestamp) = payload.get("timestamp").and_then(Value::as_str) {
            builder.started_at_raw = Some(timestamp.to_string());
            builder.started_at_sort_key_ms = parse_rfc3339_to_epoch_ms(timestamp).unwrap_or(0);
        }
    }

    let git = match payload.get("git").and_then(Value::as_object) {
        Some(git) => git,
        None => return,
    };

    if builder.git_branch.is_none() {
        builder.git_branch = git
            .get("branch")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if builder.git_commit.is_none() {
        builder.git_commit = git
            .get("commit_hash")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
    if builder.repo_url.is_none() {
        builder.repo_url = git
            .get("repository_url")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn extract_turn_context(builder: &mut SessionSummaryBuilder, value: &Value) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    if builder.cwd.is_none() {
        builder.cwd = payload
            .get("cwd")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }

    if builder.model.is_none() {
        builder.model = payload
            .get("model")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
    }
}

fn extract_title_candidate(builder: &mut SessionSummaryBuilder, value: &Value) {
    let payload = match value.get("payload").and_then(Value::as_object) {
        Some(payload) => payload,
        None => return,
    };

    if payload.get("type").and_then(Value::as_str) != Some("message") {
        return;
    }
    if payload.get("role").and_then(Value::as_str) != Some("user") {
        return;
    }

    let texts = extract_message_texts(payload, "input_text");
    for text in texts {
        if builder.first_user_text.is_none() {
            builder.first_user_text = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
        }
        if builder.meaningful_title.is_none() && is_meaningful_user_text(&text) {
            builder.meaningful_title = Some(truncate_single_line(&text, MAX_TITLE_CHARS));
            break;
        }
    }
}

impl SessionSummaryBuilder {
    fn has_minimum_fields(&self) -> bool {
        self.cwd.is_some() && self.session_id.is_some()
    }

    fn note_project_cwd(&mut self, display_path: String) {
        let key = normalize_project_key(&display_path);
        let candidate = self.project_cwds.entry(key).or_default();
        if candidate.display_path.is_empty() {
            candidate.display_path = display_path;
        }
        candidate.count = candidate.count.saturating_add(1);
    }

    fn inferred_project_cwd(&self) -> Option<String> {
        self.project_cwds
            .values()
            .max_by(|left, right| {
                left.count.cmp(&right.count).then_with(|| {
                    project_path_depth(&left.display_path)
                        .cmp(&project_path_depth(&right.display_path))
                })
            })
            .map(|candidate| candidate.display_path.clone())
    }

    fn finish(self) -> Result<SessionSummary> {
        let cwd = self
            .inferred_project_cwd()
            .or_else(|| self.cwd.as_deref().and_then(project_identity_from_path))
            .or_else(|| self.cwd.clone())
            .ok_or_else(|| anyhow::anyhow!("Missing cwd in {}", self.file_path.display()))?;
        let session_id = self
            .session_id
            .unwrap_or_else(|| self.file_path.display().to_string());
        let title = self
            .meaningful_title
            .or(self.first_user_text)
            .unwrap_or_else(|| format!("Session {session_id}"));

        let (started_at_raw, started_at_label, started_at_sort_key_ms) =
            if let Some(raw) = self.started_at_raw {
                let label = format_timestamp_label(&raw);
                (Some(raw), label, self.started_at_sort_key_ms)
            } else {
                let file_time = std::fs::metadata(&self.file_path)
                    .ok()
                    .and_then(|meta| meta.modified().ok())
                    .and_then(system_time_to_epoch_ms)
                    .unwrap_or(0);
                (None, "--".to_string(), file_time)
            };

        Ok(SessionSummary {
            file_path: self.file_path,
            session_id,
            cwd,
            title,
            started_at_raw,
            started_at_label,
            started_at_sort_key_ms,
            git_branch: self.git_branch,
            git_commit: self.git_commit,
            repo_url: self.repo_url,
            model_provider: self.model_provider,
            model: self.model,
        })
    }
}

fn project_path_depth(path: &str) -> usize {
    Path::new(path).components().count()
}

fn extract_message_texts(payload: &serde_json::Map<String, Value>, part_type: &str) -> Vec<String> {
    let mut texts = Vec::new();
    let Some(items) = payload.get("content").and_then(Value::as_array) else {
        return texts;
    };
    for item in items {
        let Some(object) = item.as_object() else {
            continue;
        };
        if object.get("type").and_then(Value::as_str) != Some(part_type) {
            continue;
        }
        if let Some(text) = object.get("text").and_then(Value::as_str) {
            texts.push(text.to_string());
        }
    }
    texts
}

fn count_message_parts(payload: &serde_json::Map<String, Value>, part_type: &str) -> usize {
    let Some(items) = payload.get("content").and_then(Value::as_array) else {
        return 0;
    };
    let mut count = 0usize;
    for item in items {
        if item.get("type").and_then(Value::as_str) == Some(part_type) {
            count += 1;
        }
    }
    count
}

fn has_message_part(payload: &serde_json::Map<String, Value>, part_type: &str) -> bool {
    count_message_parts(payload, part_type) > 0
}

fn find_nested_map<'a>(
    value: &'a Value,
    keys: &[&str],
) -> Option<&'a serde_json::Map<String, Value>> {
    for key in keys {
        if let Some(map) = value.get(*key).and_then(Value::as_object) {
            return Some(map);
        }
    }
    None
}

fn read_i64(value: Option<&Value>) -> Option<i64> {
    value.and_then(|value| {
        value
            .as_i64()
            .or_else(|| value.as_u64().and_then(|raw| i64::try_from(raw).ok()))
    })
}

fn parse_rfc3339_to_epoch_ms(raw: &str) -> Option<i64> {
    let parsed = DateTime::parse_from_rfc3339(raw).ok()?;
    Some(parsed.timestamp_millis())
}

fn format_timestamp_label(raw: &str) -> String {
    let parsed = DateTime::parse_from_rfc3339(raw).ok();
    if let Some(parsed) = parsed {
        let utc: DateTime<Utc> = parsed.with_timezone(&Utc);
        let local = utc.with_timezone(&Local);
        return local.format("%Y-%m-%d %H:%M").to_string();
    }
    raw.to_string()
}

fn system_time_to_epoch_ms(time: SystemTime) -> Option<i64> {
    let duration = time.duration_since(SystemTime::UNIX_EPOCH).ok()?;
    i64::try_from(duration.as_millis()).ok()
}

fn is_meaningful_user_text(text: &str) -> bool {
    let trimmed = text.trim_start();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.starts_with("# AGENTS.md instructions") {
        return false;
    }
    if trimmed.starts_with("<environment_context>") {
        return false;
    }
    if trimmed.starts_with("<skill>") {
        return false;
    }
    true
}

pub(crate) fn truncate_single_line(input: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let compact = input.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut output = String::new();
    for (idx, ch) in compact.chars().enumerate() {
        if idx >= max_chars {
            break;
        }
        output.push(ch);
    }
    if compact.chars().count() > max_chars {
        let suffix = if max_chars >= 3 {
            "..."
        } else if max_chars == 2 {
            ".."
        } else {
            "."
        };
        while output.len() + suffix.len() > max_chars && !output.is_empty() {
            output.pop();
        }
        output.push_str(suffix);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEMP_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn make_temp_dir(prefix: &str) -> PathBuf {
        let unique = format!(
            "{}-{}",
            std::process::id(),
            TEMP_ID_COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let dir = std::env::temp_dir().join(format!("comon-read-{prefix}-{unique}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_session(path: &Path, body: &str) {
        std::fs::write(path, body).expect("write session");
    }

    #[test]
    fn build_catalog_groups_projects_case_insensitively() {
        let root = make_temp_dir("group");
        let sessions = root.join("sessions");
        std::fs::create_dir_all(sessions.join("2026/03/16")).expect("create tree");
        let session_a = sessions.join("2026/03/16/a.jsonl");
        let session_b = sessions.join("2026/03/16/b.jsonl");

        write_session(
            &session_a,
            r##"{"type":"session_meta","payload":{"id":"a","timestamp":"2026-03-16T08:30:22.974Z","cwd":"/mnt/e/GH/oiio-builder","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /mnt/e/GH/oiio-builder"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"show me the session history"}]}}
"##,
        );
        write_session(
            &session_b,
            r##"{"type":"session_meta","payload":{"id":"b","timestamp":"2026-03-16T09:30:22.974Z","cwd":"/mnt/e/gh/oiio-builder","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"list prompts"}]}}
"##,
        );

        let catalog = build_catalog(&sessions).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(catalog.projects[0].sessions.len(), 2);
        assert_eq!(catalog.projects[0].sessions[0].title, "list prompts");
        assert_eq!(
            catalog.projects[0].sessions[1].title,
            "show me the session history"
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn build_catalog_prefers_structured_tool_workdir_git_root() {
        let root = make_temp_dir("tool-workdir");
        let sessions = root.join("sessions");
        let project = root.join("rustadmin-fps-diag");
        let project_child = project.join("flutter");
        std::fs::create_dir_all(sessions.join("2026/06/23")).expect("create sessions");
        std::fs::create_dir_all(root.join(".git")).expect("create parent git marker");
        std::fs::create_dir_all(project.join(".git")).expect("create git marker");
        std::fs::create_dir_all(&project_child).expect("create project child");
        let session = sessions.join("2026/06/23/a.jsonl");
        write_session(
            &session,
            &format!(
                "{}\n{}\n{}\n{}\n{}\n",
                r#"{"type":"session_meta","payload":{"id":"a","timestamp":"2026-06-23T08:30:22.974Z","cwd":"/outside/non-git","model_provider":"openai"}}"#,
                r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"work on the project"}]}}"#,
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": serde_json::json!({"workdir": root}).to_string()
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": serde_json::json!({"workdir": project_child}).to_string()
                    }
                }),
                serde_json::json!({
                    "type": "response_item",
                    "payload": {
                        "type": "function_call",
                        "name": "exec_command",
                        "arguments": serde_json::json!({"workdir": project_child}).to_string()
                    }
                })
            ),
        );

        let catalog = build_catalog(&sessions).expect("catalog");
        assert_eq!(catalog.projects.len(), 1);
        assert_eq!(
            catalog.projects[0].display_path,
            project.display().to_string()
        );

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn load_session_detail_extracts_turns_and_tokens() {
        let root = make_temp_dir("detail");
        let path = root.join("session.jsonl");
        write_session(
            &path,
            r##"{"type":"session_meta","payload":{"id":"s","timestamp":"2026-03-16T08:30:22.974Z","cwd":"/mnt/w/VisualStudio/comon-cli","model_provider":"openai"}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"# AGENTS.md instructions for /mnt/w/VisualStudio/comon-cli"}]}}
{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"show all prompts"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"ok"}]}}
{"type":"response_item","payload":{"type":"function_call","name":"shell_command","arguments":"{}"}}
{"type":"response_item","payload":{"type":"function_call_output","call_id":"1","output":"done"}}
{"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":12,"output_tokens":4,"total_tokens":16}}}}
"##,
        );

        let detail = load_session_detail(&path).expect("detail");
        assert_eq!(detail.meaningful_user_turns, vec!["show all prompts"]);
        assert_eq!(detail.all_user_turns.len(), 2);
        assert_eq!(detail.assistant_messages, 1);
        assert_eq!(detail.tool_calls, 1);
        assert_eq!(detail.tool_outputs, 1);
        assert_eq!(detail.total_tokens, Some(16));

        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn truncate_single_line_collapses_whitespace() {
        let text = "one\n\n two\tthree";
        assert_eq!(truncate_single_line(text, 64), "one two three");
    }
}
