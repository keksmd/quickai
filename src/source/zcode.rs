//! Адаптер источника `zcode` — ZCode (Z.ai), `~/.zcode/cli/db/db.sqlite`.
//!
//! Схема — форк OpenCode (session/message/part) плюс телеметрия: `model_usage` — строка
//! на каждый API-вызов, `turn_usage` — на пользовательский turn, `tool_usage` — на вызов
//! тулзы. Строки мутируют (`running` → `completed`), поэтому источник — снимок
//! ([`DataSource::snapshot`]): turn'ы этой БД заменяются при каждом index.
//!
//! Usage в стиле AI SDK: `input_tokens` включает `cache_read_input_tokens` (проверено на
//! живой БД) и, по конвенции AI SDK, `cache_creation_input_tokens` (не наблюдалось — всегда 0)
//! — вычитаем; `output_tokens` финальный, reasoning отдельно НЕ прибавляем. Субагент = дочерняя сессия (`session.parent_id`): её вызовы
//! идут в задачу родителя (turn родителя, активный на момент вызова) как sidechain.
//!
//! Читаем обычным read-only. Если ZCode держит лок и open/чтение не проходит — fallback
//! `immutable=1` (без локов): читаем, пока размер/mtime файла до и после чтения совпадают
//! (до 3 попыток), иначе ошибка — рваный снимок не индексируем.

use super::{DataSource, Event, FileBatch, TurnData};
use crate::model::Usage;
use anyhow::{Context, Result};
use rusqlite::{Connection, OpenFlags};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub struct ZcodeSource {
    root: PathBuf,
}

impl ZcodeSource {
    /// `root` = каталог с `db.sqlite` (по умолчанию `$ZCODE_HOME/cli/db` или `~/.zcode/cli/db`).
    pub fn new(root: Option<PathBuf>) -> Self {
        let root = root
            .or_else(|| std::env::var("ZCODE_HOME").ok().map(|h| PathBuf::from(h).join("cli/db")))
            .unwrap_or_else(|| super::home_join(".zcode/cli/db"));
        ZcodeSource { root }
    }
}

impl DataSource for ZcodeSource {
    fn name(&self) -> &'static str {
        "zcode"
    }

    fn root(&self) -> PathBuf {
        self.root.clone()
    }

    fn owns(&self, path: &Path) -> bool {
        path.file_name().map(|n| n == "db.sqlite").unwrap_or(false)
    }

    fn read_file(&self, path: &Path, _from: u64, _seed: Option<String>) -> Result<FileBatch> {
        let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        let key = path.to_string_lossy();
        let events = match open_ro(path).and_then(|c| read_db(&c, &key)) {
            Ok(ev) => ev,
            Err(_) => read_stable(path, &key)?,
        };
        Ok(FileBatch { events, bytes: size, last_prompt: None })
    }

    fn snapshot(&self) -> bool {
        true
    }

    fn max_depth(&self) -> usize {
        1
    }
}

fn open_ro(path: &Path) -> Result<Connection> {
    Ok(Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX)?)
}

fn stamp(path: &Path) -> Option<(u64, std::time::SystemTime)> {
    std::fs::metadata(path).ok().and_then(|m| Some((m.len(), m.modified().ok()?)))
}

fn read_stable(path: &Path, key: &str) -> Result<Vec<Event>> {
    for _ in 0..3 {
        let before = stamp(path);
        let events = read_db(&open_immutable(path)?, key);
        if before.is_some() && before == stamp(path) {
            return events;
        }
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    anyhow::bail!("{} меняется во время чтения — повторите index", path.display())
}

fn open_immutable(path: &Path) -> Result<Connection> {
    let p = path.to_string_lossy().replace('%', "%25").replace(' ', "%20").replace('?', "%3f").replace('#', "%23");
    Connection::open_with_flags(
        format!("file:{p}?immutable=1"),
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("open {}", path.display()))
}

struct Call {
    id: String,
    session: String,
    turn: Option<String>,
    model: String,
    agent: String,
    status: String,
    started: i64,
    finish: Option<String>,
    input: u64,
    output: u64,
    cache_write: u64,
    cache_read: u64,
    parent: Option<String>,
    dir: String,
    version: String,
    parent_turn: Option<String>,
}

fn read_db(conn: &Connection, key: &str) -> Result<Vec<Event>> {
    let mut tools: HashMap<(String, String), Vec<(String, String, bool)>> = HashMap::new();
    {
        let mut st = conn.prepare(
            "SELECT tool_call_id, tool_name, session_id, COALESCE(turn_id,''), status FROM tool_usage",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?, r.get::<_, String>(4)?))
        })?;
        for row in rows {
            let (id, name, sid, turn, status) = row?;
            tools.entry((sid, turn)).or_default().push((id, name, status == "error"));
        }
    }

    let mut events = Vec::new();
    {
        let mut st = conn.prepare(
            "SELECT tu.turn_id,
                    (SELECT group_concat(json_extract(p.data,'$.text'), ' ') FROM part p
                      WHERE p.message_id = tu.user_message_id AND json_extract(p.data,'$.type') = 'text')
             FROM turn_usage tu JOIN session s ON s.id = tu.session_id
             WHERE s.parent_id IS NULL",
        )?;
        let rows = st.query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })?;
        for row in rows {
            let (turn, text) = row?;
            let clean = text.map(|t| t.split_whitespace().collect::<Vec<_>>().join(" ")).filter(|t| !t.is_empty());
            events.push(Event::Prompt { prompt_id: turn, text: clean, agent_run_id: None });
        }
    }

    let mut st = conn.prepare(
        "SELECT m.id, m.session_id, m.turn_id, m.model_id, COALESCE(m.agent,''), m.status, m.started_at,
                m.finish_reason, m.input_tokens, m.output_tokens, m.cache_creation_input_tokens,
                m.cache_read_input_tokens, s.parent_id, COALESCE(s.directory,''), COALESCE(s.version,''),
                (SELECT tu.turn_id FROM turn_usage tu
                  WHERE tu.session_id = s.parent_id AND tu.started_at <= m.started_at
                  ORDER BY tu.started_at DESC LIMIT 1)
         FROM model_usage m LEFT JOIN session s ON s.id = m.session_id
         ORDER BY m.started_at",
    )?;
    let calls = st.query_map([], |r| {
        let tok = |i: usize| r.get::<_, Option<i64>>(i).map(|v| v.unwrap_or(0).max(0) as u64);
        Ok(Call {
            id: r.get(0)?,
            session: r.get(1)?,
            turn: r.get(2)?,
            model: r.get(3)?,
            agent: r.get(4)?,
            status: r.get(5)?,
            started: r.get(6)?,
            finish: r.get(7)?,
            input: tok(8)?,
            output: tok(9)?,
            cache_write: tok(10)?,
            cache_read: tok(11)?,
            parent: r.get(12)?,
            dir: r.get(13)?,
            version: r.get(14)?,
            parent_turn: r.get(15)?,
        })
    })?;

    let mut linked: HashSet<String> = HashSet::new();
    for c in calls {
        let c = c?;
        let sub = c.parent.is_some();
        if sub && linked.insert(c.session.clone()) {
            let agent_type = c.agent.strip_prefix("zcode-").unwrap_or(&c.agent).to_string();
            events.push(Event::AgentLink { id: c.session.clone(), agent_type });
        }
        let tool_list = tools.remove(&(c.session.clone(), c.turn.clone().unwrap_or_default())).unwrap_or_default();
        let stop_reason = if c.status == "completed" { c.finish.clone() } else { Some(c.status.clone()) };
        events.push(Event::Turn(TurnData {
            prompt_id: if sub { c.parent_turn.clone().or(c.turn.clone()) } else { c.turn.clone() },
            project: c.dir.clone(),
            session_id: Some(c.parent.clone().unwrap_or_else(|| c.session.clone())),
            agent_run_id: if sub { Some(c.session.clone()) } else { None },
            ext_id: Some(format!("{key}|{}", c.id)),
            is_sidechain: sub,
            model: c.model,
            ts: c.started,
            usage: Usage {
                input: c.input.saturating_sub(c.cache_read + c.cache_write),
                output: c.output,
                cache_write_5m: c.cache_write,
                cache_write_1h: 0,
                cache_read: c.cache_read,
                web_search: 0,
                web_fetch: 0,
            },
            cost: None,
            stop_reason,
            tool_uses: tool_list.iter().map(|(id, name, _)| (id.clone(), name.clone())).collect(),
            cwd: c.dir,
            git_branch: String::new(),
            version: c.version,
        }));
        for (id, _, err) in tool_list {
            if err {
                events.push(Event::ToolResult { tool_use_id: id, is_error: true });
            }
        }
    }
    Ok(events)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Connection {
        let c = Connection::open_in_memory().unwrap();
        c.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, parent_id TEXT, directory TEXT, version TEXT);
             CREATE TABLE part (id TEXT, message_id TEXT, data TEXT);
             CREATE TABLE turn_usage (session_id TEXT, turn_id TEXT, user_message_id TEXT, started_at INTEGER);
             CREATE TABLE tool_usage (tool_call_id TEXT, tool_name TEXT, session_id TEXT, turn_id TEXT, status TEXT);
             CREATE TABLE model_usage (id TEXT, session_id TEXT, turn_id TEXT, model_id TEXT, agent TEXT,
                 status TEXT, started_at INTEGER, finish_reason TEXT, input_tokens INTEGER, output_tokens INTEGER,
                 cache_creation_input_tokens INTEGER, cache_read_input_tokens INTEGER);
             INSERT INTO session VALUES ('S','', '/w','3.12'), ('C','S','/w','3.12');
             UPDATE session SET parent_id=NULL WHERE id='S';
             INSERT INTO part VALUES ('p1','u1','{\"type\":\"text\",\"text\":\"fix  the bug\"}');
             INSERT INTO turn_usage VALUES ('S','T1','u1',100), ('C','CT','u2',150);
             INSERT INTO tool_usage VALUES ('k1','bash','S','T1','error'), ('k2','read','S','T1','completed');
             INSERT INTO model_usage VALUES
               ('m1','S','T1','GLM-5.3','zcode-agent','completed',110,'tool-calls',1000,50,0,900),
               ('m2','S','T1','GLM-5.3','zcode-agent','completed',120,'stop',1100,60,0,1000),
               ('m3','C','CT','GLM-5.3-Flash','zcode-Explore','error',160,NULL,300,10,0,0);",
        )
        .unwrap();
        c
    }

    #[test]
    fn calls_map_to_turns() {
        let ev = read_db(&fixture(), "/z/db.sqlite").unwrap();
        let t: Vec<&TurnData> = ev.iter().filter_map(|e| if let Event::Turn(t) = e { Some(t) } else { None }).collect();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].usage.input, 100);
        assert_eq!(t[0].usage.cache_read, 900);
        assert_eq!(t[0].prompt_id.as_deref(), Some("T1"));
        assert_eq!(t[0].tool_uses.len(), 2);
        assert!(t[1].tool_uses.is_empty());
        assert_eq!(t[0].ext_id.as_deref(), Some("/z/db.sqlite|m1"));

        let sub = t[2];
        assert!(sub.is_sidechain);
        assert_eq!(sub.prompt_id.as_deref(), Some("T1"));
        assert_eq!(sub.session_id.as_deref(), Some("S"));
        assert_eq!(sub.agent_run_id.as_deref(), Some("C"));
        assert_eq!(sub.stop_reason.as_deref(), Some("error"));

        assert!(ev.iter().any(|e| matches!(e, Event::AgentLink { id, agent_type } if id == "C" && agent_type == "Explore")));
        assert!(ev.iter().any(|e| matches!(e, Event::ToolResult { tool_use_id, is_error: true } if tool_use_id == "k1")));
        assert!(ev.iter().any(|e| matches!(e, Event::Prompt { prompt_id, text: Some(x), .. } if prompt_id == "T1" && x == "fix the bug")));
    }
}
