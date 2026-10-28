//! Адаптер источника `proxy` — нативный JSONL от `quickai proxy` (live-capture).
//!
//! Append-JSONL (как Claude), поэтому дочитываем хвост по байтовому офсету. Одна
//! строка = один захваченный ответ LLM API (см. crate::proxy). cost НЕ задаём —
//! считаем по прайсингу, т.к. модели тут провайдерские (OpenAI, Anthropic и пр.).
//!
//! Нормализация usage уже сделана прокси: `input` без кэша, `output` включает
//! reasoning (поле `reasoning` — только справочное, его НЕ прибавляем), `agent` —
//! тег харнесса из `quickai proxy --tag` (идёт в project). `cache_write` прокси пока
//! не пишет (OpenAI его не отдаёт) — поле читается на будущее, по умолчанию 0.

use super::{DataSource, Event, FileBatch, TurnData};
use crate::model::Usage;
use anyhow::Result;
use serde_json::Value;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

pub struct ProxySource {
    root: PathBuf,
}

impl ProxySource {
    /// `root` = каталог с `<день>.jsonl` (по умолчанию `~/.quickai/proxy`).
    pub fn new(root: Option<PathBuf>) -> Self {
        ProxySource { root: root.unwrap_or_else(|| super::home_join(".quickai/proxy")) }
    }
}

impl DataSource for ProxySource {
    fn name(&self) -> &'static str {
        "proxy"
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
        for line in reader.lines() {
            let line = match line {
                Ok(l) if !l.trim().is_empty() => l,
                _ => continue,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                events.push(Event::Turn(turn_of(&v)));
            }
        }

        Ok(FileBatch { events, bytes: size, last_prompt: None })
    }
}

/// Одна строка прокси → turn. Группировка задачи = сессия-разговор (per-user-turn у прокси нет).
fn turn_of(v: &Value) -> TurnData {
    let u = |k: &str| -> u64 { v.get(k).and_then(|x| x.as_u64()).unwrap_or(0) };
    let s = |k: &str| -> Option<String> { v.get(k).and_then(|x| x.as_str()).map(|s| s.to_string()) };
    let session = s("session_id");
    TurnData {
        prompt_id: session.clone(),
        project: s("agent").unwrap_or_default(),
        session_id: session,
        agent_run_id: None,
        ext_id: s("ext_id"),
        is_sidechain: false,
        model: s("model").unwrap_or_default(),
        ts: v.get("ts").and_then(|x| x.as_i64()).unwrap_or(0),
        usage: Usage {
            input: u("input"),
            output: u("output"),
            cache_write_5m: u("cache_write"),
            cache_write_1h: 0,
            cache_read: u("cache_read"),
            web_search: 0,
            web_fetch: 0,
        },
        cost: None,
        stop_reason: s("stop_reason"),
        tool_uses: Vec::new(),
        skills: Vec::new(),
        cwd: String::new(),
        git_branch: String::new(),
        version: String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasoning_not_double_counted() {
        let v: Value = serde_json::from_str(
            r#"{"ts":1,"model":"gpt-5","input":100,"output":50,"reasoning":30,"cache_read":10,"cache_write":5,"agent":"aider"}"#,
        )
        .unwrap();
        let t = turn_of(&v);
        assert_eq!(t.usage.output, 50);
        assert_eq!(t.usage.cache_write_5m, 5);
        assert_eq!(t.project, "aider");
    }
}
