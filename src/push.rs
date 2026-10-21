//! `quickai push` — отправка дневных агрегатов индекса на удалённый приёмник
//! (agent-sync-hub), чтобы сводить аналитику с нескольких машин.
//!
//! Контракт `quickai.push/v1` (см. `docs/push.md`): документ — снимок окна последних
//! `days` UTC-дней хоста. Приёмник для `(host, day)` из окна ЗАМЕНЯЕТ строки `daily`,
//! а `tools`/`agents`/`tasks` — снимок окна целиком, тоже заменяется по `host`. Поэтому
//! повторная отправка идемпотентна, а пересчитанные снимки (Hermes, ZCode) не задваиваются.
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
        "SELECT date(ts/1000,'unixepoch'), source, project, model, COUNT(*), SUM(is_sidechain),
                SUM(input_tokens), SUM(output_tokens), SUM(cache_read), SUM(cache_write_5m+cache_write_1h),
                SUM(cost_usd), SUM(stop_reason IN ('error','cancelled'))
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
        "SELECT tc.source, tc.name, COUNT(*), SUM(tc.is_error)
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
        "SELECT source, agent_type, COUNT(*), SUM(turns), SUM(out_tokens), SUM(cost_usd)
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
        "SELECT prompt_id, source, project, first_ts, wall_ms, cost_usd, out_tokens, total_tokens,
                agent_count, text
         FROM tasks WHERE last_ts >= ?1 ORDER BY cost_usd DESC LIMIT 50",
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
                "text": if o.with_text { text } else { None },
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
                    ('h','s','/w','hermes','GLM-5.3',{old},1,1,0,0,0,1.0,0,'stop');",
            old = now - 40 * 86_400_000
        ))
        .unwrap();
        let doc = build(&conn, &Opts { host: "h".into(), days: 7, with_text: false }).unwrap();
        let all = doc["daily"].as_array().unwrap();
        assert_eq!(all.len(), 2);
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
}
