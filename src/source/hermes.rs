//! Адаптер источника `hermes` — Hermes Agent (`~/.hermes/state.db`, профили
//! `~/.hermes/profiles/<имя>/state.db`).
//!
//! Hermes не пишет usage на каждый API-вызов: в `session_model_usage` лежат агрегаты
//! по (сессия, модель, провайдер, task), и они мутируют. Поэтому источник — снимок
//! ([`DataSource::snapshot`]): один turn на строку агрегата, при каждом index turn'ы
//! этой БД заменяются (другие корни — зеркала с VPS — не трогаются). БД открываем read-only — Hermes держит её в WAL.
//!
//! Hermes сам нормализует usage (`agent/usage_pricing.py::normalize_usage`):
//! `input_tokens` — без кэша, `output_tokens` уже включает reasoning (`reasoning_tokens` —
//! справочное подмножество, НЕ прибавляем). cost берём из Hermes (`actual` → `estimated`),
//! иначе считаем по прайсингу.

use super::{DataSource, Event, FileBatch, TurnData};
use crate::model::Usage;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::path::{Path, PathBuf};

pub struct HermesSource {
    root: PathBuf,
}

impl HermesSource {
    /// `root` = `HERMES_HOME` (по умолчанию `$HERMES_HOME` или `~/.hermes`).
    pub fn new(root: Option<PathBuf>) -> Self {
        let root = root
            .or_else(|| std::env::var("HERMES_HOME").ok().map(PathBuf::from))
            .unwrap_or_else(|| super::home_join(".hermes"));
        HermesSource { root }
    }
}

impl DataSource for HermesSource {
    fn name(&self) -> &'static str {
        "hermes"
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn owns(&self, path: &Path) -> bool {
        path.file_name().map(|n| n == "state.db").unwrap_or(false)
    }

    fn read_file(&self, path: &Path, _from: u64, _seed: Option<String>) -> Result<FileBatch> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .with_context(|| format!("open {}", path.display()))?;
        if !has_usage_table(&conn) {
            return Ok(FileBatch { events: Vec::new(), bytes: size, last_prompt: None });
        }
        let profile = profile_of(path, &self.root);
        let key = path.to_string_lossy();
        Ok(FileBatch { events: read_usage(&conn, &profile, &key)?, bytes: size, last_prompt: None })
    }

    fn snapshot(&self) -> bool {
        true
    }

    /// `state.db` и `profiles/<имя>/state.db`; глубже — чекаут `hermes-agent/` и кэши.
    fn max_depth(&self) -> usize {
        3
    }
}

/// Потолок правдоподобной цены, $ за токен (= $1000 за 1M). Самая дорогая реальная ставка (output
/// топ-модели) на порядок ниже. Hermes иногда пишет в `estimated_cost_usd` цену за 1M токенов без
/// деления на 1e6 (~$3 за токен): такая стоимость отбрасывается, и turn считается по прайс-листу quickai.
const MAX_COST_PER_TOKEN: f64 = 1e-3;

fn cost_is_plausible(cost: f64, total_tokens: u64) -> bool {
    cost <= total_tokens as f64 * MAX_COST_PER_TOKEN
}

/// Любой файл с именем `state.db` попадает в обход каталога; не-Hermes БД (пустые, чужие) пропускаем,
/// а не роняем весь индекс.
fn has_usage_table(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='session_model_usage'",
        [],
        |_| Ok(()),
    )
    .is_ok()
}

/// Имя профиля по пути БД (`profiles/<имя>/state.db`), корневая БД — "".
fn profile_of(db: &Path, root: &Path) -> String {
    db.parent()
        .and_then(|d| d.strip_prefix(root).ok())
        .and_then(|rel| rel.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default()
}

fn read_usage(conn: &Connection, profile: &str, key: &str) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare(
        "SELECT u.session_id, u.model, u.billing_provider, u.billing_base_url, u.billing_mode, u.task,
                u.input_tokens, u.output_tokens, u.cache_read_tokens, u.cache_write_tokens,
                u.actual_cost_usd, u.estimated_cost_usd,
                COALESCE(u.last_seen, u.first_seen, s.started_at, 0),
                s.source, s.cwd, s.git_branch, s.parent_session_id, s.title, s.end_reason
         FROM session_model_usage u LEFT JOIN sessions s ON s.id = u.session_id",
    )?;
    let rows = stmt.query_map([], |r| {
        let sid: String = r.get(0)?;
        let dims: Vec<String> = (1..6).map(|i| r.get::<_, String>(i)).collect::<rusqlite::Result<_>>()?;
        let tok = |i: usize| r.get::<_, Option<i64>>(i).map(|v| v.unwrap_or(0).max(0) as u64);
        let actual: Option<f64> = r.get(10)?;
        let estimated: Option<f64> = r.get(11)?;
        let ts: f64 = r.get(12)?;
        let src: Option<String> = r.get(13)?;
        let cwd: Option<String> = r.get(14)?;
        let parent: Option<String> = r.get(16)?;
        let title: Option<String> = r.get(17)?;
        let cost = actual.filter(|c| *c > 0.0).or(estimated.filter(|c| *c > 0.0));
        let total_tokens = tok(6)? + tok(7)? + tok(8)? + tok(9)?;
        let cost = cost.filter(|c| cost_is_plausible(*c, total_tokens));
        let root_session = parent.clone().unwrap_or_else(|| sid.clone());
        let project = match (cwd.as_deref().filter(|c| !c.is_empty()), profile) {
            (Some(c), _) => c.to_string(),
            (None, "") => format!("hermes:{}", src.as_deref().unwrap_or("cli")),
            (None, p) => format!("hermes:{p}"),
        };
        let turn = TurnData {
            prompt_id: Some(root_session.clone()),
            project,
            session_id: Some(root_session),
            agent_run_id: parent.as_ref().map(|_| sid.clone()),
            ext_id: Some(format!("{key}|{sid}|{}", dims.join("|"))),
            is_sidechain: parent.is_some(),
            model: dims[0].clone(),
            ts: (ts * 1000.0) as i64,
            usage: Usage {
                input: tok(6)?,
                output: tok(7)?,
                cache_write_5m: tok(9)?,
                cache_write_1h: 0,
                cache_read: tok(8)?,
                web_search: 0,
                web_fetch: 0,
            },
            cost,
            stop_reason: r.get(18)?,
            tool_uses: Vec::new(),
            cwd: cwd.unwrap_or_default(),
            git_branch: r.get::<_, Option<String>>(15)?.unwrap_or_default(),
            version: String::new(),
        };
        Ok((turn, title, sid))
    })?;

    let mut events = Vec::new();
    for row in rows {
        let (turn, title, sid) = row?;
        if let Some(text) = title.filter(|t| !t.trim().is_empty()) {
            events.push(Event::Prompt {
                prompt_id: turn.prompt_id.clone().unwrap_or_default(),
                text: Some(text),
                agent_run_id: turn.agent_run_id.clone(),
            });
        }
        if turn.is_sidechain {
            events.push(Event::AgentLink { id: sid, agent_type: "delegate".into() });
        }
        events.push(Event::Turn(turn));
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, source TEXT NOT NULL, cwd TEXT, git_branch TEXT,
                 parent_session_id TEXT, title TEXT, end_reason TEXT, started_at REAL NOT NULL);
             CREATE TABLE session_model_usage (session_id TEXT NOT NULL, model TEXT NOT NULL,
                 billing_provider TEXT NOT NULL DEFAULT '', billing_base_url TEXT NOT NULL DEFAULT '',
                 billing_mode TEXT NOT NULL DEFAULT '', task TEXT NOT NULL DEFAULT '',
                 input_tokens INTEGER NOT NULL DEFAULT 0, output_tokens INTEGER NOT NULL DEFAULT 0,
                 cache_read_tokens INTEGER NOT NULL DEFAULT 0, cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                 reasoning_tokens INTEGER NOT NULL DEFAULT 0, estimated_cost_usd REAL NOT NULL DEFAULT 0,
                 actual_cost_usd REAL NOT NULL DEFAULT 0, first_seen REAL, last_seen REAL);
             INSERT INTO sessions VALUES ('A','telegram',NULL,NULL,NULL,'deploy bot',NULL,100.0);
             INSERT INTO sessions VALUES ('B','cli','/srv/app','main','A',NULL,'done',101.0);
             INSERT INTO session_model_usage VALUES
                 ('A','anthropic/claude-sonnet-4.6','openrouter','','','',1000,200,5000,300,50,0,0.42,100.0,102.5),
                 ('A','gpt-5-mini','openai','','','compression',10,2,0,0,0,0,0,100.0,101.0),
                 ('B','qwen3:32b','ollama','http://vps:11434','','',70,30,0,0,0,0,0,101.0,103.0);",
        )
        .unwrap();
        c
    }

    #[test]
    fn rows_map_to_turns() {
        let ev = read_usage(&fixture(), "", "/h/state.db").unwrap();
        let t: Vec<&TurnData> =
            ev.iter().filter_map(|e| if let Event::Turn(t) = e { Some(t) } else { None }).collect();
        assert_eq!(t.len(), 3);

        let a = t.iter().find(|t| t.model.contains("sonnet")).unwrap();
        assert_eq!(a.usage.input, 1000);
        assert_eq!(a.usage.output, 200);
        assert_eq!(a.usage.cache_read, 5000);
        assert_eq!(a.usage.cache_write_5m, 300);
        assert_eq!(a.cost, Some(0.42));
        assert_eq!(a.project, "hermes:telegram");
        assert_eq!(a.ts, 102_500);

        let mini = t.iter().find(|t| t.model == "gpt-5-mini").unwrap();
        assert_eq!(mini.cost, None);
        assert_ne!(mini.ext_id, a.ext_id);
        assert!(t.iter().all(|t| t.ext_id.as_deref().unwrap().starts_with("/h/state.db|")));

        let b = t.iter().find(|t| t.model.starts_with("qwen")).unwrap();
        assert!(b.is_sidechain);
        assert_eq!(b.prompt_id.as_deref(), Some("A"));
        assert_eq!(b.agent_run_id.as_deref(), Some("B"));
        assert_eq!(b.project, "/srv/app");
    }

    #[test]
    fn scale_bug_cost_falls_back_to_price_table() {
        let c = fixture();
        c.execute_batch(
            "INSERT INTO session_model_usage VALUES
                 ('A','cc/claude-sonnet-5','custom','','','',15835772,13207,0,0,0,47546937.0,0,100.0,104.0);",
        )
        .unwrap();
        let ev = read_usage(&c, "", "/h/state.db").unwrap();
        let broken = ev
            .iter()
            .find_map(|e| match e {
                Event::Turn(t) if t.model == "cc/claude-sonnet-5" => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(broken.cost, None);
        let ok = ev
            .iter()
            .find_map(|e| match e {
                Event::Turn(t) if t.model.contains("sonnet-4.6") => Some(t),
                _ => None,
            })
            .unwrap();
        assert_eq!(ok.cost, Some(0.42));
    }

    #[test]
    fn profile_from_path() {
        let root = Path::new("/h");
        assert_eq!(profile_of(Path::new("/h/state.db"), root), "");
        assert_eq!(profile_of(Path::new("/h/profiles/work/state.db"), root), "work");
    }
}
