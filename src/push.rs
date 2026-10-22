//! `quickai push` — отправка дневных агрегатов индекса на удалённый приёмник
//! (agent-sync-hub), чтобы сводить аналитику с нескольких машин.
//!
//! Контракт `quickai.push/v1` (см. `docs/push.md`): документ — снимок окна последних
//! `days` UTC-дней хоста. Приёмник для `(host, day)` из окна ЗАМЕНЯЕТ строки `daily`,
//! а `tools`/`agents`/`tasks`/`stop_reasons`/`latency` — снимок окна целиком, тоже заменяется
//! по `host`. Поэтому повторная отправка идемпотентна, а пересчитанные снимки (Hermes, ZCode)
//! не задваиваются.
//! Текст промптов уходит только с `--with-text`.

use anyhow::{bail, Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};

pub const SCHEMA: &str = "quickai.push/v1";

/// Источники-агрегаты: строка = накопительный итог сессии, её день (last_seen) «переезжает».
/// Их `daily` шлём за всю историю, приёмник заменяет все строки `(host, source)` целиком —
/// иначе после паузы длиннее окна старый день остаётся и итог задваивается.
pub const FULL_HISTORY: &[&str] = &["hermes"];

pub struct Opts {
    pub host: String,
    pub days: u32,
    pub with_text: bool,
}

/// Имя хоста: `--host`/`QUICKAI_HOST`, иначе `hostname`.
pub fn default_host() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into())
}

/// Начало окна: полночь UTC (`days`-1) дней назад, epoch ms.
fn since_ms(days: u32) -> i64 {
    let today = chrono::Utc::now().date_naive();
    let start = today - chrono::Duration::days(days.max(1) as i64 - 1);
    start.and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}

fn day_of(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|d| d.format("%Y-%m-%d").to_string()).unwrap_or_default()
}

pub fn build(conn: &Connection, o: &Opts) -> Result<Value> {
    let since = since_ms(o.days);

    let full: Vec<String> = FULL_HISTORY.iter().map(|s| format!("'{s}'")).collect();
    let mut st = conn.prepare(&format!(
        "SELECT date(ts/1000,'unixepoch'), source, project, model, COUNT(*), COALESCE(SUM(is_sidechain),0),
                COALESCE(SUM(input_tokens),0), COALESCE(SUM(output_tokens),0), COALESCE(SUM(cache_read),0),
                COALESCE(SUM(COALESCE(cache_write_5m,0)+COALESCE(cache_write_1h,0)),0),
                TOTAL(cost_usd), COALESCE(SUM(stop_reason IN ('error','cancelled')),0)
         FROM turns WHERE ts >= ?1 OR source IN ({}) GROUP BY 1,2,3,4 ORDER BY 1,2,3,4",
        full.join(",")
    ))?;
    let daily: Vec<Value> = st
        .query_map([since], |r| {
            Ok(json!({
                "day": r.get::<_, String>(0)?,
                "source": r.get::<_, String>(1)?,
                "project": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "model": r.get::<_, Option<String>>(3)?.unwrap_or_default(),
                "turns": r.get::<_, i64>(4)?,
                "sidechain_turns": r.get::<_, i64>(5)?,
                "input_tokens": r.get::<_, i64>(6)?,
                "output_tokens": r.get::<_, i64>(7)?,
                "cache_read_tokens": r.get::<_, i64>(8)?,
                "cache_write_tokens": r.get::<_, i64>(9)?,
                "cost_usd": r.get::<_, f64>(10)?,
                "failed_turns": r.get::<_, i64>(11)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut st = conn.prepare(
        "SELECT tc.source, tc.name, COUNT(*), COALESCE(SUM(tc.is_error),0)
         FROM tool_calls tc JOIN sessions s ON s.session_id = tc.session_id
         WHERE s.last_ts >= ?1 GROUP BY 1,2 ORDER BY 3 DESC",
    )?;
    let tools: Vec<Value> = st
        .query_map([since], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "name": r.get::<_, String>(1)?,
                "calls": r.get::<_, i64>(2)?,
                "errors": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut st = conn.prepare(
        "SELECT source, agent_type, COUNT(*), COALESCE(SUM(turns),0), COALESCE(SUM(out_tokens),0), TOTAL(cost_usd)
         FROM agent_runs WHERE last_ts >= ?1 GROUP BY 1,2 ORDER BY 6 DESC",
    )?;
    let agents: Vec<Value> = st
        .query_map([since], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "agent_type": r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                "runs": r.get::<_, i64>(2)?,
                "turns": r.get::<_, i64>(3)?,
                "output_tokens": r.get::<_, i64>(4)?,
                "cost_usd": r.get::<_, f64>(5)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut st = conn.prepare(
        "WITH t AS (SELECT * FROM tasks WHERE last_ts >= ?1),
         gaps AS (
           SELECT prompt_id, ts - LAG(ts) OVER (PARTITION BY prompt_id ORDER BY ts) AS gap
           FROM turns WHERE ts > 0 AND prompt_id IN (SELECT prompt_id FROM t)
         ),
         gs AS (
           SELECT prompt_id,
                  SUM(CASE WHEN gap > 60000 THEN gap ELSE 0 END) AS idle,
                  SUM(CASE WHEN gap > 60000 AND gap < 1800000 THEN gap ELSE 0 END) AS wait,
                  SUM(CASE WHEN gap >= 1800000 THEN gap ELSE 0 END) AS away,
                  MAX(gap) AS max_gap
           FROM gaps GROUP BY prompt_id
         ),
         st AS (
           SELECT t.prompt_id, t.cost_usd, t.wall_ms,
                  (SELECT COUNT(*) FROM turns WHERE prompt_id = t.prompt_id) AS n,
                  COALESCE(gs.idle, 0) AS idle, COALESCE(gs.wait, 0) AS wait,
                  COALESCE(gs.away, 0) AS away, COALESCE(gs.max_gap, 0) AS max_gap
           FROM t LEFT JOIN gs ON gs.prompt_id = t.prompt_id
         ),
         sel AS (
           SELECT prompt_id FROM (SELECT prompt_id FROM st ORDER BY COALESCE(cost_usd,0) DESC, prompt_id LIMIT 50)
           UNION SELECT prompt_id FROM (SELECT prompt_id FROM st ORDER BY COALESCE(wall_ms,0) DESC, prompt_id LIMIT 20)
           UNION SELECT prompt_id FROM (SELECT prompt_id FROM st ORDER BY n DESC, prompt_id LIMIT 20)
         )
         SELECT t.prompt_id, t.source, t.project, COALESCE(t.first_ts,t.last_ts), COALESCE(t.wall_ms,0), COALESCE(t.cost_usd,0),
                COALESCE(t.out_tokens,0), COALESCE(t.total_tokens,0), COALESCE(t.agent_count,0), t.text,
                st.n, st.idle, st.max_gap, st.wait, st.away
         FROM t JOIN st ON st.prompt_id = t.prompt_id
         WHERE t.prompt_id IN (SELECT prompt_id FROM sel)
         ORDER BY COALESCE(t.cost_usd,0) DESC, t.prompt_id",
    )?;
    let tasks: Vec<Value> = st
        .query_map([since], |r| {
            let text: Option<String> = r.get(9)?;
            Ok(json!({
                "id": r.get::<_, String>(0)?,
                "source": r.get::<_, String>(1)?,
                "project": r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                "day": day_of(r.get::<_, i64>(3)?),
                "wall_ms": r.get::<_, i64>(4)?,
                "cost_usd": r.get::<_, f64>(5)?,
                "output_tokens": r.get::<_, i64>(6)?,
                "total_tokens": r.get::<_, i64>(7)?,
                "agent_count": r.get::<_, i64>(8)?,
                "turns": r.get::<_, i64>(10)?,
                "idle_ms": r.get::<_, i64>(11)?,
                "max_gap_ms": r.get::<_, i64>(12)?,
                "wait_ms": r.get::<_, i64>(13)?,
                "away_ms": r.get::<_, i64>(14)?,
                "text": if o.with_text { text } else { None },
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut st = conn.prepare(
        "SELECT COALESCE(source,''), COALESCE(model,''), substr(COALESCE(stop_reason,'(null)'),1,200), COUNT(*)
         FROM turns WHERE ts >= ?1 GROUP BY 1,2,3 ORDER BY 4 DESC",
    )?;
    let stop_reasons: Vec<Value> = st
        .query_map([since], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "model": r.get::<_, String>(1)?,
                "reason": r.get::<_, String>(2)?,
                "turns": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut st = conn.prepare(
        "WITH g AS (
           SELECT source, model, ts,
                  ts - LAG(ts) OVER (PARTITION BY session_id, COALESCE(agent_run_id,'') ORDER BY ts) AS gap
           FROM turns WHERE ts >= ?1 - 120000 AND session_id IS NOT NULL
         )
         SELECT COALESCE(source,''), COALESCE(model,''), COUNT(*), CAST(AVG(gap) AS INTEGER)
         FROM g WHERE ts >= ?1 AND gap > 0 AND gap < 120000 GROUP BY 1,2 ORDER BY 3 DESC",
    )?;
    let latency: Vec<Value> = st
        .query_map([since], |r| {
            Ok(json!({
                "source": r.get::<_, String>(0)?,
                "model": r.get::<_, String>(1)?,
                "samples": r.get::<_, i64>(2)?,
                "avg_gap_ms": r.get::<_, i64>(3)?,
            }))
        })?
        .collect::<rusqlite::Result<_>>()?;

    Ok(json!({
        "schema": SCHEMA,
        "host": o.host,
        "quickai_version": env!("CARGO_PKG_VERSION"),
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "window": { "since_day": day_of(since), "until_day": day_of(chrono::Utc::now().timestamp_millis()), "days": o.days },
        "full_history_sources": FULL_HISTORY,
        "daily": daily,
        "tools": tools,
        "agents": agents,
        "tasks": tasks,
        "stop_reasons": stop_reasons,
        "latency": latency,
    }))
}

/// POST документа на `url` (Bearer `token`). Не-2xx → ошибка с телом ответа.
pub fn send(url: &str, token: Option<&str>, doc: &Value) -> Result<String> {
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let client = reqwest::Client::builder().timeout(std::time::Duration::from_secs(60)).build()?;
        let mut req = client.post(url).json(doc);
        if let Some(t) = token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.with_context(|| format!("POST {url}"))?;
        let status = resp.status();
        let body: String = resp.text().await.unwrap_or_default().chars().take(500).collect();
        if !status.is_success() {
            bail!("{url} ответил {status}: {body}");
        }
        Ok(format!("{status} {body}"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_and_rollup() {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::init(&conn).unwrap();
        let now = chrono::Utc::now().timestamp_millis();
        conn.execute_batch(&format!(
            "INSERT INTO turns(prompt_id,session_id,project,source,model,ts,input_tokens,output_tokens,cache_read,
                               cache_write_5m,cache_write_1h,cost_usd,is_sidechain,stop_reason)
             VALUES ('p','s','/w','zcode','GLM-5.3',{now},10,2,100,0,0,0.5,0,'stop'),
                    ('p','s','/w','zcode','GLM-5.3',{now},20,3,0,5,0,0.25,1,'error'),
                    ('q','s','/w','zcode','GLM-5.3',{old},999,999,0,0,0,9.0,0,'stop'),
                    ('h','s','/w','hermes','GLM-5.3',{old},1,1,0,0,0,1.0,0,'stop'),
                    ('r','s','/w','codex','gpt-5',{now},1,1,0,0,0,0.1,0,NULL);",
            old = now - 40 * 86_400_000
        ))
        .unwrap();
        let doc = build(&conn, &Opts { host: "h".into(), days: 7, with_text: false }).unwrap();
        let all = doc["daily"].as_array().unwrap();
        assert_eq!(all.len(), 3);
        let c: Vec<&Value> = all.iter().filter(|r| r["source"] == "codex").collect();
        assert_eq!(c[0]["failed_turns"], 0, "NULL stop_reason group must not abort the push");
        assert!(all.iter().any(|r| r["source"] == "hermes"));
        let d: Vec<&Value> = all.iter().filter(|r| r["source"] == "zcode").collect();
        assert_eq!(d.len(), 1);
        assert_eq!(d[0]["turns"], 2);
        assert_eq!(d[0]["input_tokens"], 30);
        assert_eq!(d[0]["cache_write_tokens"], 5);
        assert_eq!(d[0]["sidechain_turns"], 1);
        assert_eq!(d[0]["failed_turns"], 1);
        assert!((d[0]["cost_usd"].as_f64().unwrap() - 0.75).abs() < 1e-9);
        assert_eq!(doc["schema"], SCHEMA);
    }

    #[test]
    fn insights_snapshots() {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::init(&conn).unwrap();
        let t0 = chrono::Utc::now().timestamp_millis() - 3_600_000;
        conn.execute_batch(&format!(
            "INSERT INTO turns(prompt_id,session_id,project,source,model,ts,stop_reason) VALUES
                ('o','sa','/w','claude','m1',{old},'error'),
                ('a','sa','/w','claude','m1',{t0},'stop'),
                ('a','sa','/w','claude','m1',{t0}+10000,'stop'),
                ('a','sa','/w','claude','m1',{t0}+100000,'stop'),
                ('a','sa','/w','claude','m1',{t0}+130000,NULL),
                ('zz','sb','/w','codex',NULL,{t0},'stop'),
                ('zz','sb','/w','codex',NULL,{t0}+200000,'stop');
             INSERT INTO tasks(prompt_id,session_id,project,source,first_ts,last_ts,wall_ms,cost_usd)
             VALUES ('a','sa','/w','claude',{t0},{t0}+130000,130000,2.0),
                    ('zz','sb','/w','codex',{t0},{t0}+200000,200000,0.01);
             WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<59)
             INSERT INTO tasks(prompt_id,session_id,project,source,first_ts,last_ts,wall_ms,cost_usd)
             SELECT printf('f%02d',i),printf('fs%02d',i),'/w','filler',{t0},{t0}+1000,1000,1.0 FROM n;
             WITH RECURSIVE n(i) AS (SELECT 0 UNION ALL SELECT i+1 FROM n WHERE i<59)
             INSERT INTO turns(prompt_id,session_id,project,source,model,ts,stop_reason)
             SELECT printf('f%02d',i),printf('fs%02d',i),'/w','filler','m3',{t0}+d,'stop'
             FROM n, (SELECT 0 AS d UNION ALL SELECT 1000);",
            old = t0 - 40 * 86_400_000
        ))
        .unwrap();
        let doc = build(&conn, &Opts { host: "h".into(), days: 7, with_text: false }).unwrap();

        let tasks = doc["tasks"].as_array().unwrap();
        assert_eq!(tasks.len(), 51, "50 by cost (a + 49 fillers) + zz by wall_ms only");
        let a = tasks.iter().find(|t| t["id"] == "a").unwrap();
        assert_eq!(a["turns"], 4);
        assert_eq!(a["idle_ms"], 90_000);
        assert_eq!(a["max_gap_ms"], 90_000);
        assert_eq!((a["wait_ms"].as_i64(), a["away_ms"].as_i64()), (Some(90_000), Some(0)));
        let zz = tasks.iter().find(|t| t["id"] == "zz").expect("top by wall_ms must be included");
        assert_eq!(zz["turns"], 2);
        assert_eq!(zz["idle_ms"], 200_000);
        assert_eq!(zz["max_gap_ms"], 200_000);
        assert!(zz["text"].is_null());
        let f = tasks.iter().find(|t| t["id"] == "f00").unwrap();
        assert_eq!((f["turns"].as_i64(), f["idle_ms"].as_i64(), f["max_gap_ms"].as_i64()), (Some(2), Some(0), Some(1000)));

        let sr = doc["stop_reasons"].as_array().unwrap();
        let n = |src: &str, model: &str, reason: &str| {
            sr.iter()
                .find(|r| r["source"] == src && r["model"] == model && r["reason"] == reason)
                .map(|r| r["turns"].as_i64().unwrap())
        };
        assert_eq!(n("claude", "m1", "stop"), Some(3));
        assert_eq!(n("claude", "m1", "(null)"), Some(1));
        assert_eq!(n("claude", "m1", "error"), None, "turn outside the window is excluded");
        assert_eq!(n("codex", "", "stop"), Some(2), "NULL model -> empty string");

        let lat = doc["latency"].as_array().unwrap();
        let m1 = lat.iter().find(|r| r["model"] == "m1").unwrap();
        assert_eq!(m1["samples"], 3);
        assert_eq!(m1["avg_gap_ms"], 43_333);
        assert!(lat.iter().all(|r| r["source"] != "codex"), "gap >= 120s is excluded");
        let m3 = lat.iter().find(|r| r["model"] == "m3").unwrap();
        assert_eq!((m3["samples"].as_i64(), m3["avg_gap_ms"].as_i64()), (Some(60), Some(1000)));
    }

    #[test]
    fn task_waits_split_from_walk_aways() {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::init(&conn).unwrap();
        let t0 = chrono::Utc::now().timestamp_millis() - 7_200_000;
        conn.execute_batch(&format!(
            "INSERT INTO turns(prompt_id,session_id,project,source,model,ts,stop_reason) VALUES
                ('w','s','/w','claude','m',{t0},'stop'),
                ('w','s','/w','claude','m',{t0}+120000,'stop'),
                ('w','s','/w','claude','m',{t0}+150000,'stop'),
                ('w','s','/w','claude','m',{t0}+2550000,'stop');
             INSERT INTO tasks(prompt_id,session_id,project,source,first_ts,last_ts,wall_ms,cost_usd)
             VALUES ('w','s','/w','claude',{t0},{t0}+2550000,2550000,1.0);"
        ))
        .unwrap();
        let doc = build(&conn, &Opts { host: "h".into(), days: 7, with_text: false }).unwrap();
        let w = &doc["tasks"][0];
        assert_eq!(w["idle_ms"], 2_520_000);
        assert_eq!(w["wait_ms"], 120_000, "1-30 min gaps are waits");
        assert_eq!(w["away_ms"], 2_400_000, "30 min+ gaps are walk-aways");
    }


    #[test]
    fn latency_keeps_agent_runs_apart_and_long_reasons_are_cut() {
        let conn = Connection::open_in_memory().unwrap();
        crate::index::schema::init(&conn).unwrap();
        let t0 = chrono::Utc::now().timestamp_millis() - 600_000;
        let long = "x".repeat(300);
        conn.execute_batch(&format!(
            "INSERT INTO turns(prompt_id,session_id,agent_run_id,project,source,model,ts,stop_reason) VALUES
                ('p','s','a1','/w','claude','m',{t0},'stop'),
                ('p','s','a2','/w','claude','m',{t0}+1000,'stop'),
                ('p','s','a1','/w','claude','m',{t0}+20000,'stop'),
                ('p','s','a2','/w','claude','m',{t0}+21000,'{long}'),
                ('p',NULL,NULL,'/w','claude','m',{t0}+22000,'stop');"
        ))
        .unwrap();
        let doc = build(&conn, &Opts { host: "h".into(), days: 7, with_text: false }).unwrap();
        let lat = doc["latency"].as_array().unwrap();
        assert_eq!(lat.len(), 1);
        assert_eq!(lat[0]["samples"], 2, "one gap per agent run, none across runs or without a session");
        assert_eq!(lat[0]["avg_gap_ms"], 20_000);
        let reasons: Vec<&str> = doc["stop_reasons"].as_array().unwrap().iter().map(|r| r["reason"].as_str().unwrap()).collect();
        assert!(reasons.iter().all(|r| r.chars().count() <= 200));
        assert!(reasons.contains(&"x".repeat(200).as_str()));
    }

}
