//! Адаптер источника `kagent` — агенты Dada Cloud на движке kagent (Google ADK).
//!
//! Производитель — сам под агента: рантайм (kagent-app) дописывает каждое ADK-событие в
//! `~/.quickai/kagent/<день>.jsonl` и сам запускает `quickai push --index`. Одна строка =
//! одно событие: `{id, session_id, user_id, agent_id, model, created_at_ms, data}`, где
//! `data` — ADK `Event` как есть. Файлы читаются как append-JSONL (по байтовому офсету).
//!
//! Разбор события:
//! - `usage_metadata` или `error_code` → assistant-turn; `invocation_id` = задача (promptId);
//!   `prompt_token_count` включает кэш, поэтому input = prompt − cached; output = candidates +
//!   thoughts (рассуждения тарифицируются как выход); `error_code` → stop_reason `error`.
//! - `author == "user"` → промпт задачи (текст уходит в push только с `--with-text`).
//! - `function_call` / `function_response` → тулзы; ошибка = MCP `isError` или поле `error`.
//! - `author`, отличный от самого агента (и не `user`/`system`) → субагент: turn в сайдчейне.
//!
//! project = имя агента в k8s-виде (`tg-exchange-support`): по нему стыкуются карточки
//! агентов в agent-sync-hub и Langfuse.

use super::{DataSource, Event, FileBatch, TurnData};
use crate::model::Usage;
use anyhow::Result;
use serde_json::Value;
use std::collections::HashSet;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct KagentSource {
    root: PathBuf,
}

impl KagentSource {
    /// `root` = каталог с `<день>.jsonl` (по умолчанию `~/.quickai/kagent`).
    pub fn new(root: Option<PathBuf>) -> Self {
        KagentSource { root: root.unwrap_or_else(default_root) }
    }
}

pub fn default_root() -> PathBuf {
    super::home_join(".quickai/kagent")
}

impl DataSource for KagentSource {
    fn name(&self) -> &'static str {
        "kagent"
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn owns(&self, path: &Path) -> bool {
        path.extension().map(|e| e == "jsonl").unwrap_or(false)
    }

    fn read_file(&self, path: &Path, from: u64, _seed: Option<String>) -> Result<FileBatch> {
        let mut f = std::fs::File::open(path)?;
        let size = f.metadata().map(|m| m.len()).unwrap_or(from);
        f.seek(SeekFrom::Start(from))?;
        let reader = BufReader::new(&mut f as &mut dyn Read);
        let mut events = Vec::new();
        let mut linked = HashSet::new();
        for line in reader.lines() {
            let line = match line {
                Ok(l) if !l.trim().is_empty() => l,
                _ => continue,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                events_of(&v, &mut linked, &mut events);
            }
        }
        Ok(FileBatch { events, bytes: size, last_prompt: None })
    }
}

/// `kagent__NS__tg_exchange_support` → `tg_exchange_support` (имя автора событий агента).
fn agent_author(agent_id: &str) -> &str {
    agent_id.rsplit("__NS__").next().unwrap_or(agent_id)
}

/// Имя агента в k8s-виде — так его зовут ManagedAgent, Langfuse и карточки.
pub fn agent_name(agent_id: &str) -> String {
    agent_author(agent_id).replace('_', "-")
}

fn parts(data: &Value) -> impl Iterator<Item = &Value> {
    data.pointer("/content/parts").and_then(|p| p.as_array()).into_iter().flatten()
}

fn tool_failed(response: &Value) -> bool {
    response.get("isError").and_then(|x| x.as_bool()).unwrap_or(false)
        || response.get("error").map(|e| !e.is_null()).unwrap_or(false)
}

fn stop_reason(data: &Value) -> Option<String> {
    if data.get("error_code").map(|e| !e.is_null()).unwrap_or(false) {
        return Some("error".into());
    }
    data.get("finish_reason").and_then(|x| x.as_str()).map(|r| match r {
        "STOP" => "stop".to_string(),
        "MAX_TOKENS" => "max_tokens".to_string(),
        other => other.to_ascii_lowercase(),
    })
}

/// Одна строка, записанная подом агента → события индексатора.
fn events_of(line: &Value, linked: &mut HashSet<String>, out: &mut Vec<Event>) {
    let data = match line.get("data") {
        Some(d) if d.is_object() => d,
        _ => return,
    };
    let s = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let agent_id = s(line, "agent_id");
    let own = agent_author(&agent_id).to_string();
    let author = s(data, "author");
    let invocation = data.get("invocation_id").and_then(|x| x.as_str()).map(|x| x.to_string());

    if author == "user" {
        if let Some(prompt_id) = invocation.clone() {
            let text: Vec<&str> = parts(data).filter_map(|p| p.get("text").and_then(|t| t.as_str())).collect();
            let text = text.join("\n").trim().to_string();
            out.push(Event::Prompt { prompt_id, text: (!text.is_empty()).then_some(text), agent_run_id: None });
        }
    }

    for p in parts(data) {
        if let Some(fr) = p.get("function_response").filter(|x| x.is_object()) {
            if let Some(id) = fr.get("id").and_then(|x| x.as_str()) {
                let failed = fr.get("response").map(tool_failed).unwrap_or(false);
                out.push(Event::ToolResult { tool_use_id: id.to_string(), is_error: failed });
            }
        }
    }

    let usage = data.get("usage_metadata").filter(|u| u.is_object());
    let errored = data.get("error_code").map(|e| !e.is_null()).unwrap_or(false);
    if usage.is_none() && !errored {
        return;
    }
    let n = |k: &str| usage.and_then(|u| u.get(k)).and_then(|x| x.as_u64()).unwrap_or(0);
    let cached = n("cached_content_token_count");
    let sidechain = !author.is_empty() && author != own && author != "user" && author != "system";
    let agent_run_id = match (&invocation, sidechain) {
        (Some(inv), true) => {
            let id = format!("{inv}:{author}");
            if linked.insert(id.clone()) {
                out.push(Event::AgentLink { id: id.clone(), agent_type: author.clone() });
            }
            Some(id)
        }
        _ => None,
    };
    let tool_uses = parts(data)
        .filter_map(|p| p.get("function_call").filter(|x| x.is_object()))
        .filter_map(|fc| {
            Some((fc.get("id")?.as_str()?.to_string(), fc.get("name")?.as_str()?.to_string()))
        })
        .collect();
    let ts = data
        .get("timestamp")
        .and_then(|x| x.as_f64())
        .map(|sec| (sec * 1000.0) as i64)
        .or_else(|| line.get("created_at_ms").and_then(|x| x.as_i64()))
        .unwrap_or(0);
    out.push(Event::Turn(TurnData {
        prompt_id: invocation,
        project: agent_name(&agent_id),
        session_id: Some(s(line, "session_id")).filter(|x| !x.is_empty()),
        agent_run_id,
        ext_id: Some(s(line, "id")).filter(|x| !x.is_empty()),
        is_sidechain: sidechain,
        model: s(line, "model"),
        ts,
        usage: Usage {
            input: n("prompt_token_count").saturating_sub(cached),
            output: n("candidates_token_count") + n("thoughts_token_count"),
            cache_write_5m: 0,
            cache_write_1h: 0,
            cache_read: cached,
            web_search: 0,
            web_fetch: 0,
        },
        cost: None,
        stop_reason: stop_reason(data),
        tool_uses,
        skills: Vec::new(),
        cwd: String::new(),
        git_branch: String::new(),
        version: String::new(),
    }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn line(data: Value) -> Value {
        json!({"id": "e1", "session_id": "s1", "user_id": "u1", "agent_id": "kagent__NS__tg_exchange_support",
               "created_at_ms": 5, "model": "glm-5.3-flash", "data": data})
    }

    fn run(v: Value) -> Vec<Event> {
        let mut out = Vec::new();
        events_of(&v, &mut HashSet::new(), &mut out);
        out
    }

    #[test]
    fn llm_call_becomes_turn_with_cache_split_and_tools() {
        let ev = run(line(json!({
            "author": "tg_exchange_support", "invocation_id": "inv1", "timestamp": 1.5, "finish_reason": "STOP",
            "usage_metadata": {"prompt_token_count": 1000, "cached_content_token_count": 600,
                               "candidates_token_count": 40, "thoughts_token_count": 10},
            "content": {"parts": [{"function_call": {"id": "c1", "name": "kb_search"}}]}
        })));
        let t = match &ev[..] {
            [Event::Turn(t)] => t,
            _ => panic!("one turn expected"),
        };
        assert_eq!((t.usage.input, t.usage.cache_read, t.usage.output), (400, 600, 50));
        assert_eq!(t.project, "tg-exchange-support");
        assert_eq!(t.prompt_id.as_deref(), Some("inv1"));
        assert_eq!(t.stop_reason.as_deref(), Some("stop"));
        assert_eq!(t.ts, 1500);
        assert_eq!(t.model, "glm-5.3-flash");
        assert!(!t.is_sidechain);
        assert_eq!(t.tool_uses, vec![("c1".to_string(), "kb_search".to_string())]);
    }

    #[test]
    fn api_error_without_usage_is_a_failed_turn() {
        let ev = run(line(json!({"author": "tg_exchange_support", "invocation_id": "inv1", "error_code": "API_ERROR",
                                 "usage_metadata": null})));
        match &ev[..] {
            [Event::Turn(t)] => assert_eq!(t.stop_reason.as_deref(), Some("error")),
            _ => panic!("failed turn expected"),
        }
    }

    #[test]
    fn user_prompt_tool_errors_and_subagents() {
        let ev = run(line(json!({"author": "user", "invocation_id": "inv1",
                                 "content": {"parts": [{"text": " hello "}]}})));
        assert!(matches!(&ev[..], [Event::Prompt { prompt_id, text: Some(t), .. }] if prompt_id == "inv1" && t == "hello"));

        let ev = run(line(json!({"author": "tg_exchange_support", "content": {"parts": [
            {"function_response": {"id": "c1", "response": {"isError": true, "content": []}}},
            {"function_response": {"id": "c2", "response": {"isError": false}}},
            {"function_response": {"id": "c3", "response": {"error": "boom"}}}]}})));
        let failed: Vec<(String, bool)> = ev
            .iter()
            .filter_map(|e| match e {
                Event::ToolResult { tool_use_id, is_error } => Some((tool_use_id.clone(), *is_error)),
                _ => None,
            })
            .collect();
        assert_eq!(failed, vec![("c1".into(), true), ("c2".into(), false), ("c3".into(), true)]);

        let mut linked = HashSet::new();
        let mut out = Vec::new();
        let sub = line(json!({"author": "researcher", "invocation_id": "inv1",
                              "usage_metadata": {"prompt_token_count": 10, "candidates_token_count": 1}}));
        events_of(&sub, &mut linked, &mut out);
        events_of(&sub, &mut linked, &mut out);
        let links = out.iter().filter(|e| matches!(e, Event::AgentLink { agent_type, .. } if agent_type == "researcher")).count();
        assert_eq!(links, 1, "one link per sub-agent run");
        assert!(out.iter().any(|e| matches!(e, Event::Turn(t) if t.is_sidechain && t.agent_run_id.as_deref() == Some("inv1:researcher"))));
    }
}
