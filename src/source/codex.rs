//! Адаптер источника `codex` — rollout'ы OpenAI Codex (`~/.codex/sessions/**/rollout-*.jsonl`
//! и `~/.codex/archived_sessions/`).
//!
//! Формат append-JSONL, но контекст (модель, turn, мета сессии) живёт в начале файла,
//! поэтому изменившийся файл перечитываем целиком, а дубли режет `(source, ext_id)`.
//!
//! Usage на каждый API-вызов:
//! - `token_usage_record` (новые CLI) — `ext_id` = `response_id`;
//! - иначе `event_msg/token_count` — `last_token_usage`, `ext_id` = `<thread>:<кумулятивный total>`
//!   (Codex пишет повторные token_count с тем же total — они схлопываются).
//!
//! Семантика OpenAI: `input_tokens` включает `cached_input_tokens`, `output_tokens` включает
//! `reasoning_output_tokens`. Субагент = отдельный rollout с `source.subagent` в
//! `session_meta`: его turn'ы идут в задачу родителя (`root_turn_id`) как sidechain.

use super::{DataSource, Event, FileBatch, TurnData};
use crate::model::Usage;
use crate::parse::ts_to_ms;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

pub struct CodexSource {
    root: PathBuf,
}

impl CodexSource {
    /// `root` = `CODEX_HOME` (по умолчанию `$CODEX_HOME` или `~/.codex`).
    pub fn new(root: Option<PathBuf>) -> Self {
        let root = root
            .or_else(|| std::env::var("CODEX_HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| super::home_join(".codex"));
        CodexSource { root }
    }
}

impl DataSource for CodexSource {
    fn name(&self) -> &'static str {
        "codex"
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn owns(&self, path: &Path) -> bool {
        path.extension().map(|e| e == "jsonl").unwrap_or(false)
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.starts_with("rollout-"))
                .unwrap_or(false)
    }

    fn read_file(&self, path: &Path, _from: u64, _seed: Option<String>) -> Result<FileBatch> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let rows: Vec<Value> = BufReader::new(std::fs::File::open(path)?)
            .lines()
            .map_while(|l| l.ok())
            .filter_map(|l| serde_json::from_str(&l).ok())
            .collect();
        Ok(FileBatch { events: parse_rollout(&rows), bytes: size, last_prompt: None })
    }
}

/// Контекст сессии из первого `session_meta` (последующие — копия родителя у форков).
#[derive(Default)]
struct Meta {
    session_id: Option<String>,
    thread_id: String,
    cwd: String,
    version: String,
    /// Субагент: его роль (`agent_role` → `agent_nickname`).
    subagent: Option<String>,
}

fn s(v: &Value, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(|x| x.as_str()).map(|x| x.to_string())
}

fn n(v: &Value, key: &str) -> u64 {
    v.get(key).and_then(|x| x.as_u64()).unwrap_or(0)
}

fn usage_of(u: &Value) -> Usage {
    let cached = n(u, "cached_input_tokens");
    let cache_write = n(u, "cache_write_input_tokens");
    Usage {
        input: n(u, "input_tokens").saturating_sub(cached),
        output: n(u, "output_tokens"),
        cache_write_5m: cache_write,
        cache_write_1h: 0,
        cache_read: cached,
        web_search: 0,
        web_fetch: 0,
    }
}

/// Текст пользовательского промпта. Служебные вставки (`<...>`, AGENTS.md) — шум.
fn prompt_text(text: &str) -> Option<String> {
    let t = text.trim();
    if t.is_empty() || t.starts_with('<') || t.starts_with("# AGENTS.md") {
        return None;
    }
    Some(t.split_whitespace().collect::<Vec<_>>().join(" "))
}

fn parse_rollout(rows: &[Value]) -> Vec<Event> {
    let mut meta: Option<Meta> = None;
    let precise = rows.iter().any(|r| r["type"] == "token_usage_record");

    let mut events = Vec::new();
    let mut model = String::new();
    let mut task: Option<String> = None;
    let mut prompted: Option<String> = None;
    let mut tool_uses: Vec<(String, String)> = Vec::new();

    for r in rows {
        let p = &r["payload"];
        let ts = r.get("timestamp").and_then(|x| x.as_str()).map(ts_to_ms).unwrap_or(0);
        match (r["type"].as_str().unwrap_or(""), p["type"].as_str().unwrap_or("")) {
            ("session_meta", _) if meta.is_none() => {
                let subagent = p.pointer("/source/subagent").map(|sa| {
                    s(sa, "/thread_spawn/agent_role")
                        .or_else(|| s(sa, "/thread_spawn/agent_nickname"))
                        .unwrap_or_else(|| "subagent".into())
                });
                let m = Meta {
                    session_id: s(p, "/session_id").or_else(|| s(p, "/id")),
                    thread_id: s(p, "/id").unwrap_or_default(),
                    cwd: s(p, "/cwd").unwrap_or_default(),
                    version: s(p, "/cli_version").unwrap_or_default(),
                    subagent,
                };
                if let Some(m) = s(p, "/model").or_else(|| s(p, "/base_instructions/provenance/model")) {
                    model = m;
                }
                if let Some(t) = &m.subagent {
                    events.push(Event::AgentLink { id: m.thread_id.clone(), agent_type: t.clone() });
                }
                meta = Some(m);
            }
            ("turn_context", _) | ("event_msg", "task_started") => {
                if let Some(m) = s(p, "/model") {
                    model = m;
                }
                if let Some(t) = s(p, "/root_turn_id").or_else(|| s(p, "/turn_id")) {
                    task = Some(t);
                }
            }
            ("event_msg", "user_message") => {
                push_prompt(&mut events, &meta, &task, &mut prompted, p["message"].as_str().unwrap_or(""));
            }
            ("response_item", "message") if p["role"] == "user" => {
                let text = p["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c["text"].as_str())
                    .collect::<Vec<_>>()
                    .join(" ");
                push_prompt(&mut events, &meta, &task, &mut prompted, &text);
            }
            ("response_item", "function_call") | ("response_item", "custom_tool_call") => {
                if let (Some(id), Some(name)) = (s(p, "/call_id").or_else(|| s(p, "/id")), s(p, "/name")) {
                    tool_uses.push((id, name));
                }
            }
            ("token_usage_record", _) if precise => {
                if let Some(t) = s(p, "/root_turn_id").or_else(|| s(p, "/turn_id")) {
                    task = Some(t);
                }
                let ext = s(p, "/response_id");
                events.push(turn(&meta, &task, &model, ts, ext, usage_of(&p["usage"]), &mut tool_uses));
            }
            ("event_msg", "token_count") if !precise => {
                let (Some(last), Some(total)) =
                    (p.pointer("/info/last_token_usage"), p.pointer("/info/total_token_usage"))
                else {
                    continue;
                };
                let thread = meta.as_ref().map(|m| m.thread_id.as_str()).unwrap_or("");
                let ext = Some(format!("{thread}:{}", n(total, "total_tokens")));
                events.push(turn(&meta, &task, &model, ts, ext, usage_of(last), &mut tool_uses));
            }
            _ => {}
        }
    }
    events
}

fn push_prompt(
    events: &mut Vec<Event>,
    meta: &Option<Meta>,
    task: &Option<String>,
    prompted: &mut Option<String>,
    raw: &str,
) {
    let Some(pid) = task.clone() else { return };
    if prompted.as_ref() == Some(&pid) {
        return;
    }
    let Some(text) = prompt_text(raw) else { return };
    *prompted = Some(pid.clone());
    let agent_run_id = meta.as_ref().filter(|m| m.subagent.is_some()).map(|m| m.thread_id.clone());
    events.push(Event::Prompt { prompt_id: pid, text: Some(text), agent_run_id });
}

fn turn(
    meta: &Option<Meta>,
    task: &Option<String>,
    model: &str,
    ts: i64,
    ext_id: Option<String>,
    usage: Usage,
    tool_uses: &mut Vec<(String, String)>,
) -> Event {
    let m = meta.as_ref();
    let sub = m.map(|m| m.subagent.is_some()).unwrap_or(false);
    Event::Turn(TurnData {
        prompt_id: task.clone(),
        project: m.map(|m| m.cwd.clone()).unwrap_or_default(),
        session_id: m.and_then(|m| m.session_id.clone()),
        agent_run_id: if sub { m.map(|m| m.thread_id.clone()) } else { None },
        ext_id,
        is_sidechain: sub,
        model: model.to_string(),
        ts,
        usage,
        cost: None,
        stop_reason: None,
        tool_uses: std::mem::take(tool_uses),
        skills: Vec::new(),
        cwd: m.map(|m| m.cwd.clone()).unwrap_or_default(),
        git_branch: String::new(),
        version: m.map(|m| m.version.clone()).unwrap_or_default(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(lines: &[&str]) -> Vec<Value> {
        lines.iter().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    fn turns(ev: &[Event]) -> Vec<&TurnData> {
        ev.iter().filter_map(|e| if let Event::Turn(t) = e { Some(t) } else { None }).collect()
    }

    #[test]
    fn token_count_dedup_and_cache_split() {
        let ev = parse_rollout(&rows(&[
            r#"{"timestamp":"2026-10-04T10:00:00Z","type":"session_meta","payload":{"id":"T1","session_id":"T1","cwd":"/w","cli_version":"0.1","source":"cli","base_instructions":{"provenance":{"model":"gpt-5.1-codex"}}}}"#,
            r#"{"timestamp":"2026-10-04T10:00:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"U1"}}"#,
            r#"{"timestamp":"2026-10-04T10:00:01Z","type":"event_msg","payload":{"type":"user_message","message":"fix the bug"}}"#,
            r#"{"timestamp":"2026-10-04T10:00:02Z","type":"response_item","payload":{"type":"function_call","call_id":"c1","name":"shell"}}"#,
            r#"{"timestamp":"2026-10-04T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":150},"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":50,"reasoning_output_tokens":20,"total_tokens":150}}}}"#,
            r#"{"timestamp":"2026-10-04T10:00:03Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":150},"last_token_usage":{"input_tokens":100,"cached_input_tokens":40,"output_tokens":50,"total_tokens":150}}}}"#,
            r#"{"timestamp":"2026-10-04T10:00:04Z","type":"event_msg","payload":{"type":"token_count","info":null}}"#,
        ]));
        let t = turns(&ev);
        assert_eq!(t.len(), 2);
        assert_eq!(t[0].ext_id, t[1].ext_id);
        assert_eq!(t[0].usage.input, 60);
        assert_eq!(t[0].usage.cache_read, 40);
        assert_eq!(t[0].usage.output, 50);
        assert_eq!(t[0].prompt_id.as_deref(), Some("U1"));
        assert_eq!(t[0].model, "gpt-5.1-codex");
        assert_eq!(t[0].tool_uses, vec![("c1".to_string(), "shell".to_string())]);
        assert!(t[1].tool_uses.is_empty());
        assert!(ev.iter().any(|e| matches!(e, Event::Prompt { prompt_id, .. } if prompt_id == "U1")));
    }

    #[test]
    fn usage_record_wins_and_subagent_rolls_up() {
        let ev = parse_rollout(&rows(&[
            r#"{"timestamp":"2026-10-04T10:00:00Z","type":"session_meta","payload":{"id":"S2","session_id":"ROOT","cwd":"/w","source":{"subagent":{"thread_spawn":{"parent_thread_id":"ROOT","agent_role":"reviewer"}}}}}"#,
            r#"{"timestamp":"2026-10-04T10:00:00Z","type":"session_meta","payload":{"id":"ROOT","session_id":"ROOT","cwd":"/parent"}}"#,
            r#"{"timestamp":"2026-10-04T10:00:01Z","type":"turn_context","payload":{"turn_id":"X","root_turn_id":"R","model":"gpt-6"}}"#,
            r#"{"timestamp":"2026-10-04T10:00:02Z","type":"token_usage_record","payload":{"response_id":"resp_1","root_turn_id":"R","usage":{"input_tokens":10,"cached_input_tokens":0,"output_tokens":5}}}"#,
            r#"{"timestamp":"2026-10-04T10:00:02Z","type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"total_tokens":15},"last_token_usage":{"input_tokens":10,"output_tokens":5}}}}"#,
        ]));
        let t = turns(&ev);
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].ext_id.as_deref(), Some("resp_1"));
        assert_eq!(t[0].prompt_id.as_deref(), Some("R"));
        assert_eq!(t[0].session_id.as_deref(), Some("ROOT"));
        assert_eq!(t[0].agent_run_id.as_deref(), Some("S2"));
        assert!(t[0].is_sidechain);
        assert_eq!(t[0].project, "/w");
        assert!(ev.iter().any(|e| matches!(e, Event::AgentLink { id, agent_type } if id == "S2" && agent_type == "reviewer")));
    }
}
