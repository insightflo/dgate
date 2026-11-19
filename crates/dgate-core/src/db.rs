//! Decision 게이트 SQLite WAL SSOT 연결·마이그레이션·프로젝트 식별 모듈 —
//! `src/decision/db.ts` (plan-r2.1 §3, PB-1 DDL v2)의 Rust 포팅.
//!
//! 목적: 단일 SQLite 구현으로 CLI·훅·매퍼·GUI가 동일한 통계 엔진을 공유하기 위한
//! 물리적 기반을 제공한다(Split-Brain 원천 차단 — §3.1).
//!
//! 주의:
//! - 읽기전용 구독자는 반드시 `read_only: true` + `PRAGMA query_only=ON`을 쓴다.
//! - DDL은 §3.3·PB-1 원문과 문자열 단위로 일치한다(CHECK 제약 오타 금지).
//! - 마이그레이션 핫패스는 `user_version >= 2` 읽기 프로브로 먼저 빠져나가
//!   매 open마다 WAL 쓰기 잠금을 점유하지 않는다.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use sha2::{Digest, Sha256};

/// 전역 사전확률 풀용 예약 프로젝트 ID (§3.4)
pub const GLOBAL_PROJECT_ID: &str = "__GLOBAL__";

/// 로컬 타임존 기준 `YYYY-MM-DD` — agy 일일 예산 키 (TS `localDay`와 동일).
fn local_day(now_ms: i64) -> String {
    let secs = now_ms.div_euclid(1000);
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let t: libc::time_t = secs as libc::time_t;
    unsafe {
        libc::localtime_r(&t, &mut tm);
    }
    format!("{:04}-{:02}-{:02}", tm.tm_year + 1900, tm.tm_mon + 1, tm.tm_mday)
}

/// `$XDG_STATE_HOME`(비빈 값) 또는 `~/.local/state` + `/dgate/decision.db`.
pub fn default_db_path() -> PathBuf {
    let xdg = std::env::var("XDG_STATE_HOME")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let base = match xdg {
        Some(x) => PathBuf::from(x),
        None => dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("/"))
            .join(".local")
            .join("state"),
    };
    base.join("dgate").join("decision.db")
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// DB 경로 우선순위: explicit(`--watch-db` 등) > `DGATE_DB_PATH`
/// > `DECISION_ORCHESTRATOR_DB_PATH` > `GEMINI_TRICK_DB_PATH`(레거시 별칭) > XDG 기본 경로.
pub fn resolve_db_path(explicit: Option<&str>) -> PathBuf {
    if let Some(p) = explicit.map(|s| s.trim()).filter(|s| !s.is_empty()) {
        return PathBuf::from(p);
    }
    for env_key in [
        "DGATE_DB_PATH",
        "DECISION_ORCHESTRATOR_DB_PATH",
        "GEMINI_TRICK_DB_PATH",
    ] {
        if let Some(v) = non_empty_env(env_key) {
            return PathBuf::from(v);
        }
    }
    default_db_path()
}

#[derive(Debug, Clone, Default)]
pub struct OpenDbOptions {
    /// 읽기전용 구독(관제) — `PRAGMA query_only=ON` 함께 적용. 파일을 절대 생성하지 않고 마이그레이션도 돌지 않는다.
    pub read_only: bool,
    /// busy 타임아웃(ms). 일반 writer 5000, reader 1000 기본.
    pub busy_timeout_ms: Option<u32>,
}

fn es(e: rusqlite::Error) -> String {
    e.to_string()
}

/// 연결·PRAGMA·(writer) 마이그레이션까지 수행하는 단일 진입점.
pub fn open_db(db_path: Option<&str>, opts: OpenDbOptions) -> Result<Connection, String> {
    let resolved = resolve_db_path(db_path);
    let busy = opts.busy_timeout_ms.unwrap_or(if opts.read_only { 1000 } else { 5000 });
    let conn = if !opts.read_only {
        if let Some(parent) = resolved.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
            }
        }
        Connection::open_with_flags(&resolved, OpenFlags::default())
            .map_err(|e| format!("open {}: {}", resolved.display(), e))?
    } else {
        // 읽기전용 모드는 파일을 절대 생성하지 않는다.
        if !resolved.exists() {
            return Err(format!("decision db not found: {}", resolved.display()));
        }
        Connection::open_with_flags(&resolved, OpenFlags::SQLITE_OPEN_READ_ONLY)
            .map_err(|e| format!("open {}: {}", resolved.display(), e))?
    };
    conn.busy_timeout(std::time::Duration::from_millis(busy as u64))
        .map_err(es)?;
    if !opts.read_only {
        // busy_timeout을 먼저 잡은 뒤 WAL로 전환한다(잠금 경합 완충).
        // journal_mode 전환 시도는 busy handler를 오래 점유하며 부분 잠금을 드잡을
        // 수 있어(동시 최초 생성 경합에서 타 writer까지 굶김) 전환 동안은 짧은
        // 타임아웃으로 빠르게 재시도하고 성공 후 원래 값으로 복원한다.
        conn.execute_batch(&format!("PRAGMA busy_timeout = {};", busy)).map_err(es)?;
        conn.execute_batch("PRAGMA busy_timeout = 150;").map_err(es)?;
        let mut wal_err: Option<String> = None;
        for _ in 0..80 {
            match conn.query_row::<String, _, _>("PRAGMA journal_mode = WAL", [], |r| r.get(0)) {
                Ok(mode) if mode.eq_ignore_ascii_case("wal") => {
                    wal_err = None;
                    break;
                }
                Ok(mode) => wal_err = Some(format!("journal_mode = {} (wal 아님)", mode)),
                Err(e) => wal_err = Some(es(e)),
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        conn.execute_batch(&format!("PRAGMA busy_timeout = {};", busy)).map_err(es)?;
        if let Some(e) = wal_err {
            return Err(format!("journal_mode WAL 전환 실패: {}", e));
        }
        conn.execute_batch("PRAGMA synchronous = NORMAL; PRAGMA foreign_keys = ON;")
            .map_err(es)?;
        // migrate 동안은 busy_timeout을 짧게 — 핫패스(v2 완료 DB)는 BEGIN 자체를
        // 하지 않으므로 기존 DB 성능에 영향 없고, 신규 생성 경합만 빠르게 순환한다.
        conn.execute_batch("PRAGMA busy_timeout = 800;").map_err(es)?;
        // migrate는 멱등(BEGIN IMMEDIATE + 재판정)이라 BUSY 시 전체 재실행이 안전하다.
        // 동시 최초 생성 경합에서 BEGIN IMMEDIATE의 busy 대기가 잠금 해방 후에도
        // 깨어나지 못하는 기현상(WAL 전환 직후)이 실측되어 짧은 재시도로 수렴시킨다.
        let mut migrate_err: Option<String> = None;
        for attempt in 0..10 {
            match migrate(&conn, Some(&resolved)) {
                Ok(()) => {
                    migrate_err = None;
                    break;
                }
                Err(err) => {
                    let locked = err.contains("database is locked") || err.contains("database is busy");
                    if locked && attempt < 9 {
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        migrate_err = Some(err);
                        continue;
                    }
                    let _ = conn.close();
                    return Err(format!("migrate({}): {}", resolved.display(), err));
                }
            }
        }
        if let Some(err) = migrate_err {
            let _ = conn.close();
            return Err(format!("migrate({}): {}", resolved.display(), err));
        }
        conn.execute_batch(&format!("PRAGMA busy_timeout = {};", busy)).map_err(es)?;
    } else {
        conn.execute_batch(&format!("PRAGMA busy_timeout = {};", busy)).map_err(es)?;
        conn.execute_batch("PRAGMA query_only = ON;").map_err(es)?;
    }
    Ok(conn)
}

/// 안전 폴백 연결: DB 미존재·권한 오류·손상·잠금 시 예외 대신 `None`을 반환한다(§3.5).
pub fn open_db_safe(db_path: Option<&str>, opts: OpenDbOptions) -> Option<Connection> {
    open_db(db_path, opts).ok()
}

// ---------- 스키마 버전·오브젝트 감지 헬퍼 (PB-1: DDL v2) ----------

pub fn get_schema_version(db: &Connection) -> i64 {
    db.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap_or(0)
}

pub fn has_table(db: &Connection, name: &str) -> bool {
    db.query_row(
        "SELECT 1 AS one FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|o| o.is_some())
    .unwrap_or(false)
}

fn is_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// `table`는 내부 상수만 전달된다 — `table_info`는 파라미터 바인딩 미지원이므로
/// 식별자 화이트리스트 검증 후 실행한다.
pub fn has_column(db: &Connection, table: &str, col: &str) -> bool {
    if !is_identifier(table) {
        return false;
    }
    let Ok(mut stmt) = db.prepare(&format!("PRAGMA table_info({})", table)) else {
        return false;
    };
    let names: Vec<String> = match stmt.query_map([], |r| r.get::<_, String>(1)) {
        Ok(rows) => rows.flatten().collect(),
        Err(_) => return false,
    };
    names.iter().any(|name| name == col)
}

/// §3.3 v1 DDL 전체(멱등). v1 픽스처 생성·검증 테스트와 마이그레이션의
/// "기타 테이블" 재사용을 위해 export한다.
pub fn migrate_v1_tables(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS stats_summary (
  project_id         TEXT NOT NULL,
  project_label      TEXT NOT NULL,
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_class         TEXT NOT NULL,
  ewma_success       REAL NOT NULL DEFAULT 0.0,
  ewma_fail          REAL NOT NULL DEFAULT 0.0,
  raw_samples        INTEGER NOT NULL DEFAULT 0,
  consecutive_errors INTEGER NOT NULL DEFAULT 0,
  last_target_hash   TEXT,
  last_outcome       INTEGER,
  updated_at_ms      INTEGER NOT NULL,
  PRIMARY KEY (project_id, route, tool_class)
);

CREATE TABLE IF NOT EXISTS forks (
  fork_id            TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  project_label      TEXT NOT NULL,
  conversation_id    TEXT,
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_class         TEXT NOT NULL,
  decision           TEXT NOT NULL CHECK (decision IN ('sharp','split')),
  forced_split       INTEGER NOT NULL DEFAULT 0,
  p_at_gate          REAL NOT NULL,
  basis_at_gate      TEXT NOT NULL CHECK (basis_at_gate IN ('prior','measured')),
  status             TEXT NOT NULL CHECK (status IN ('open','ok','fail','abandoned','rerouted','backprop_fail')),
  closed_by          TEXT CHECK (closed_by IN ('dispatcher','lead','worker','ttl','verifier')),
  neon_verdict       TEXT CHECK (neon_verdict IN ('pass','fail')),
  ttl_ms             INTEGER NOT NULL DEFAULT 600000,
  created_at_ms      INTEGER NOT NULL,
  last_touch_ms      INTEGER NOT NULL,
  closed_at_ms       INTEGER,
  metadata_json      TEXT,
  CHECK (status = 'open' OR (closed_by IS NOT NULL AND closed_at_ms IS NOT NULL))
);
CREATE INDEX IF NOT EXISTS idx_forks_open_ttl ON forks(status, last_touch_ms);
CREATE INDEX IF NOT EXISTS idx_forks_conv ON forks(conversation_id, created_at_ms);

CREATE TABLE IF NOT EXISTS decision_events (
  event_id           INTEGER PRIMARY KEY AUTOINCREMENT,
  dedupe_key         TEXT UNIQUE,
  fork_id            TEXT,
  project_id         TEXT NOT NULL,
  conversation_id    TEXT,
  source             TEXT NOT NULL CHECK (source IN ('hook','agy_stream','transcript','rest_api','cli','wrap','ttl_orphan','neon_backprop')),
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_name          TEXT NOT NULL,
  tool_class         TEXT NOT NULL,
  ok                 INTEGER NOT NULL CHECK (ok IN (0, 1)),
  weight             REAL NOT NULL DEFAULT 1.0,
  is_retry           INTEGER NOT NULL DEFAULT 0,
  target_hash        TEXT,
  error_snippet      TEXT,
  created_at_ms      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_events_proj_time ON decision_events(project_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_events_conv_source ON decision_events(conversation_id, source);

CREATE TABLE IF NOT EXISTS session_cursor (
  conversation_id    TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  has_hook_coverage  INTEGER NOT NULL DEFAULT 0,
  last_failed_tool   TEXT,
  last_failed_target TEXT,
  last_failed_step   INTEGER,
  session_consec_err INTEGER NOT NULL DEFAULT 0,
  updated_at_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS agy_budget (
  day                TEXT PRIMARY KEY,
  turns_used         INTEGER NOT NULL DEFAULT 0,
  sessions_started   INTEGER NOT NULL DEFAULT 0,
  updated_at_ms      INTEGER NOT NULL
);
"#,
    )
    .map_err(es)
}

// ---------- DDL v2 (PB-1): forks 리빌드 · decision_events 확장 · fork_intervals ----------

/// v2 `forks` DDL — `name`은 forks_v2(리빌드)/forks(신규) 두 위치에서 재사용 (§1.2-a)
fn forks_v2_ddl(name: &str) -> String {
    format!(
        r#"CREATE TABLE IF NOT EXISTS {} (
  fork_id            TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  project_label      TEXT NOT NULL,
  conversation_id    TEXT,
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_class         TEXT NOT NULL,
  decision           TEXT NOT NULL CHECK (decision IN ('sharp','split')),
  forced_split       INTEGER NOT NULL DEFAULT 0,
  p_at_gate          REAL NOT NULL,
  basis_at_gate      TEXT NOT NULL CHECK (basis_at_gate IN ('prior','measured')),
  generation         INTEGER NOT NULL DEFAULT 1 CHECK (generation >= 1),
  attempt            INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
  lease_owner        TEXT,
  lease_expires_ms   INTEGER,
  status             TEXT NOT NULL CHECK (status IN
                       ('open','ok','fail','abandoned','rerouted','backprop_fail','interrupted','waiting_human')),
  closed_by          TEXT CHECK (closed_by IN ('dispatcher','lead','worker','ttl','verifier')),
  neon_verdict       TEXT CHECK (neon_verdict IN ('pass','fail')),
  ttl_ms             INTEGER NOT NULL DEFAULT 600000,
  created_at_ms      INTEGER NOT NULL,
  last_touch_ms      INTEGER NOT NULL,
  closed_at_ms       INTEGER,
  metadata_json      TEXT,
  CHECK (status IN ('open','waiting_human') OR (closed_by IS NOT NULL AND closed_at_ms IS NOT NULL)),
  CHECK ((lease_owner IS NULL) = (lease_expires_ms IS NULL))
);"#,
        name
    )
}

fn create_forks_v2_indexes(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        r#"
CREATE INDEX IF NOT EXISTS idx_forks_open_ttl ON forks(status, last_touch_ms);
CREATE INDEX IF NOT EXISTS idx_forks_conv ON forks(conversation_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_forks_lease ON forks(status, lease_expires_ms);
"#,
    )
    .map_err(es)
}

fn create_fork_intervals(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS fork_intervals (
  interval_id    INTEGER PRIMARY KEY AUTOINCREMENT,
  fork_id        TEXT NOT NULL,
  attempt        INTEGER NOT NULL DEFAULT 1 CHECK (attempt >= 1),
  phase          TEXT NOT NULL CHECK (phase IN ('exec','wait','human')),
  started_at_ms  INTEGER NOT NULL,
  ended_at_ms    INTEGER,
  CHECK (ended_at_ms IS NULL OR ended_at_ms >= started_at_ms)
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_fork_intervals_one_open
  ON fork_intervals(fork_id, attempt) WHERE ended_at_ms IS NULL;
CREATE INDEX IF NOT EXISTS idx_fork_intervals_fork
  ON fork_intervals(fork_id, started_at_ms);
CREATE INDEX IF NOT EXISTS idx_fork_intervals_phase
  ON fork_intervals(phase, started_at_ms);"#,
    )
    .map_err(es)
}

/// v2 `decision_events` 컬럼 목록 — 신규 생성·ALTER 추가 두 경로에서 동일 정의 재사용
const DECISION_EVENTS_V2_COLUMNS: [(&str, &str); 7] = [
    (
        "cause",
        "cause TEXT CHECK (cause IS NULL OR cause IN\n      ('ok_pass','transient_network','tool_error','max_turns','no_result','artifact_missing',\n       'gate_fail','stall','timeout','config_error','interrupted'))",
    ),
    ("input_tokens", "input_tokens INTEGER CHECK (input_tokens IS NULL OR input_tokens >= 0)"),
    ("output_tokens", "output_tokens INTEGER CHECK (output_tokens IS NULL OR output_tokens >= 0)"),
    ("cache_read_tokens", "cache_read_tokens INTEGER CHECK (cache_read_tokens IS NULL OR cache_read_tokens >= 0)"),
    ("cache_write_tokens", "cache_write_tokens INTEGER CHECK (cache_write_tokens IS NULL OR cache_write_tokens >= 0)"),
    ("cost_usd", "cost_usd REAL CHECK (cost_usd IS NULL OR cost_usd >= 0)"),
    (
        "cost_status",
        "cost_status TEXT NOT NULL DEFAULT 'unknown' CHECK (cost_status IN ('reported','estimated','unknown'))",
    ),
];

fn decision_events_v2_ddl() -> String {
    let extra: String = DECISION_EVENTS_V2_COLUMNS
        .iter()
        .map(|(_, ddl)| format!("  {}", ddl))
        .collect::<Vec<_>>()
        .join(",\n");
    format!(
        r#"CREATE TABLE IF NOT EXISTS decision_events (
  event_id           INTEGER PRIMARY KEY AUTOINCREMENT,
  dedupe_key         TEXT UNIQUE,
  fork_id            TEXT,
  project_id         TEXT NOT NULL,
  conversation_id    TEXT,
  source             TEXT NOT NULL CHECK (source IN ('hook','agy_stream','transcript','rest_api','cli','wrap','ttl_orphan','neon_backprop')),
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_name          TEXT NOT NULL,
  tool_class         TEXT NOT NULL,
  ok                 INTEGER NOT NULL CHECK (ok IN (0, 1)),
  weight             REAL NOT NULL DEFAULT 1.0,
  is_retry           INTEGER NOT NULL DEFAULT 0,
  target_hash        TEXT,
  error_snippet      TEXT,
  created_at_ms      INTEGER NOT NULL,
{}
);
CREATE INDEX IF NOT EXISTS idx_events_proj_time ON decision_events(project_id, created_at_ms);
CREATE INDEX IF NOT EXISTS idx_events_conv_source ON decision_events(conversation_id, source);"#,
        extra
    )
}

fn alter_decision_events(db: &Connection) -> Result<(), String> {
    for (name, ddl) in DECISION_EVENTS_V2_COLUMNS {
        if !has_column(db, "decision_events", name) {
            db.execute_batch(&format!("ALTER TABLE decision_events ADD COLUMN {};", ddl))
                .map_err(es)?;
        }
    }
    Ok(())
}

/// v1 `forks` → v2 리빌드(§1.4): 18컬럼 + generation/attempt/lease_* 승격 백필.
/// metadata_json → 컬럼 백필은 `json_valid` + `typeof` 가드를 지킨다 —
/// 손상 JSON에서 `json_extract`는 NULL이 아니라 예외를 던지므로 조건식의
/// 첫 피연산자를 항상 `json_valid(metadata_json)`로 둔다(SQLite AND 단락 평가).
/// 리스는 owner/expires "쌍 단위" — 절반만 유효하면 둘 다 NULL로 정규화(CHECK 보호).
fn rebuild_forks(db: &Connection) -> Result<(), String> {
    db.execute_batch(&forks_v2_ddl("forks_v2")).map_err(es)?;
    db.execute_batch(
        r#"
INSERT INTO forks_v2 (
  fork_id, project_id, project_label, conversation_id, route, tool_class, decision,
  forced_split, p_at_gate, basis_at_gate, generation, attempt, lease_owner, lease_expires_ms,
  status, closed_by, neon_verdict, ttl_ms, created_at_ms, last_touch_ms, closed_at_ms, metadata_json
)
SELECT
  fork_id, project_id, project_label, conversation_id, route, tool_class, decision,
  forced_split, p_at_gate, basis_at_gate,
  CASE WHEN json_valid(metadata_json)
        AND typeof(json_extract(metadata_json, '$.gen')) = 'integer'
        AND json_extract(metadata_json, '$.gen') >= 1
       THEN json_extract(metadata_json, '$.gen') ELSE 1 END,
  CASE WHEN json_valid(metadata_json)
        AND typeof(json_extract(metadata_json, '$.attempt')) = 'integer'
        AND json_extract(metadata_json, '$.attempt') >= 1
       THEN json_extract(metadata_json, '$.attempt') ELSE 1 END,
  CASE WHEN json_valid(metadata_json)
        AND typeof(json_extract(metadata_json, '$.lease_owner')) = 'text'
        AND json_extract(metadata_json, '$.lease_owner') <> ''
        AND typeof(json_extract(metadata_json, '$.lease_until_ms')) = 'integer'
       THEN json_extract(metadata_json, '$.lease_owner') ELSE NULL END,
  CASE WHEN json_valid(metadata_json)
        AND typeof(json_extract(metadata_json, '$.lease_owner')) = 'text'
        AND json_extract(metadata_json, '$.lease_owner') <> ''
        AND typeof(json_extract(metadata_json, '$.lease_until_ms')) = 'integer'
       THEN json_extract(metadata_json, '$.lease_until_ms') ELSE NULL END,
  CASE WHEN status = 'open' AND json_valid(metadata_json)
        AND json_extract(metadata_json, '$.waiting_human') = 1
       THEN 'waiting_human' ELSE status END,
  closed_by, neon_verdict, ttl_ms, created_at_ms, last_touch_ms, closed_at_ms, metadata_json
FROM forks;"#,
    )
    .map_err(es)?;
    db.execute_batch("DROP TABLE forks;").map_err(es)?;
    db.execute_batch("ALTER TABLE forks_v2 RENAME TO forks;").map_err(es)?;
    create_forks_v2_indexes(db)
}

/// `cause=` 접두 규약이 박힌 레거시 error_snippet에서 유효 enum만 백필 —
/// CHECK 위반 시 전체 실패이므로 루프로 필터링한다.
const BACKFILL_CAUSES: [&str; 11] = [
    "ok_pass",
    "transient_network",
    "tool_error",
    "max_turns",
    "no_result",
    "artifact_missing",
    "gate_fail",
    "stall",
    "timeout",
    "config_error",
    "interrupted",
];

fn backfill_event_cause(db: &Connection) -> Result<(), String> {
    if !has_column(db, "decision_events", "cause") {
        return Ok(());
    }
    let rows: Vec<(i64, String)> = db
        .prepare(
            "SELECT event_id, error_snippet FROM decision_events
              WHERE cause IS NULL AND error_snippet LIKE 'cause=%'",
        )
        .map_err(es)?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))
        .map_err(es)?
        .collect::<Result<Vec<_>, _>>()
        .map_err(es)?;
    for (event_id, snippet) in rows {
        let head = &snippet["cause=".len()..];
        let v = match head.find(';') {
            Some(semi) => &head[..semi],
            None => head,
        };
        if BACKFILL_CAUSES.contains(&v) {
            db.execute("UPDATE decision_events SET cause = ?1 WHERE event_id = ?2", rusqlite::params![v, event_id])
                .map_err(es)?;
        }
    }
    Ok(())
}

/// 파일 DB 백업(§1.3): `VACUUM INTO`는 트랜잭션 밖에서만 가능하므로 BEGIN 전에 수행.
/// `${dbPath}.v1.bak.<ts>` 명명 — 이미 `.v1.bak.*`가 존재하면 재생성하지 않는다.
fn backup_once_v1(db: &Connection, db_path: &Path) -> Option<String> {
    let path_str = db_path.to_string_lossy();
    if path_str == ":memory:" {
        return None;
    }
    let dir = db_path.parent()?;
    let base = db_path.file_name()?.to_string_lossy().into_owned();
    let prefix = format!("{}.v1.bak.", base);
    let existing = fs::read_dir(dir).ok()?;
    if existing.flatten().any(|e| e.file_name().to_string_lossy().starts_with(&prefix)) {
        return None;
    }
    let dest = format!("{}.v1.bak.{}", path_str, now_ms());
    let safe_dest = dest.replace('\'', "''");
    db.execute_batch(&format!("VACUUM INTO '{}'", safe_dest)).ok()?;
    Some(dest)
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// 신규 DB용 v2 DDL — forks·decision_events는 처음부터 v2로 생성한다
fn create_v2_tables(db: &Connection) -> Result<(), String> {
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS stats_summary (
  project_id         TEXT NOT NULL,
  project_label      TEXT NOT NULL,
  route              TEXT NOT NULL CHECK (route IN ('file_select','tool_select','retry_or_abort','delegate_swarm')),
  tool_class         TEXT NOT NULL,
  ewma_success       REAL NOT NULL DEFAULT 0.0,
  ewma_fail          REAL NOT NULL DEFAULT 0.0,
  raw_samples        INTEGER NOT NULL DEFAULT 0,
  consecutive_errors INTEGER NOT NULL DEFAULT 0,
  last_target_hash   TEXT,
  last_outcome       INTEGER,
  updated_at_ms      INTEGER NOT NULL,
  PRIMARY KEY (project_id, route, tool_class)
);
"#,
    )
    .map_err(es)?;
    db.execute_batch(&forks_v2_ddl("forks")).map_err(es)?;
    create_forks_v2_indexes(db)?;
    db.execute_batch(&decision_events_v2_ddl()).map_err(es)?;
    db.execute_batch(
        r#"
CREATE TABLE IF NOT EXISTS session_cursor (
  conversation_id    TEXT PRIMARY KEY,
  project_id         TEXT NOT NULL,
  has_hook_coverage  INTEGER NOT NULL DEFAULT 0,
  last_failed_tool   TEXT,
  last_failed_target TEXT,
  last_failed_step   INTEGER,
  session_consec_err INTEGER NOT NULL DEFAULT 0,
  updated_at_ms      INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS agy_budget (
  day                TEXT PRIMARY KEY,
  turns_used         INTEGER NOT NULL DEFAULT 0,
  sessions_started   INTEGER NOT NULL DEFAULT 0,
  updated_at_ms      INTEGER NOT NULL
);
"#,
    )
    .map_err(es)?;
    create_fork_intervals(db)
}

/// 스키마 마이그레이션 진입점(§3.3 + PB-1 §1.3):
/// - `forks` 부재(신규 DB) → v2 스키마를 바로 생성하고 `user_version = 2`.
/// - v1 DB(`forks` 존재 AND `generation` 컬럼 부재) → VACUUM INTO 백업 후
///   단일 `BEGIN IMMEDIATE ... COMMIT` 내부에서 재판정·리빌드·백필·`user_version = 2`.
///   오류 시 ROLLBACK으로 v1을 그대로 보존한다.
/// - 핫패스(`user_version >= 2` AND `generation` 존재)는 비용 0으로 반환.
/// - `db_path`는 백업 파일명에만 쓰인다(생략 시 백업 생략).
pub fn migrate(db: &Connection, db_path: Option<&Path>) -> Result<(), String> {
    // 신규 DB(forks 부재)는 처음부터 v2 — v1 테이블을 먼저 만들면 판정을 오도한다
    if !has_table(db, "forks") {
        // 단일 IMMEDIATE Tx + 재판정: 다중 프로세스 동시 최초 생성 경합에서
        // 나중 진입자는 user_version을 보고 no-op (v1 승격 경로와 동일 계약)
        db.execute_batch("BEGIN IMMEDIATE").map_err(|e| format!("BEGIN: {}", e))?;
        let result = (|| -> Result<(), String> {
            if get_schema_version(db) >= 2 {
                return Ok(());
            }
            create_v2_tables(db).map_err(|e| format!("CREATE_V2: {}", e))?;
            alter_decision_events(db).map_err(|e| format!("ALTER_EVENTS: {}", e))?; // 부분 존재 DB: 기존 v1 decision_events에는 컬럼만 추가
            db.execute_batch("PRAGMA user_version = 2").map_err(|e| format!("USERVER: {}", e))?;
            db.execute_batch("COMMIT").map_err(|e| format!("COMMIT: {}", e))?;
            Ok(())
        })();
        match result {
            Ok(()) => return Ok(()),
            Err(err) => {
                let _ = db.execute_batch("ROLLBACK");
                return Err(err);
            }
        }
    }
    // 핫패스(읽기 전용 프로브): v2 완료 DB는 즉시 반환. 매 open마다 CREATE IF NOT EXISTS를
    // 실행하면 WAL 쓰기 잠금을 점유해 동시 writer 경합 시 SQLITE_BUSY를 유발한다(§3.3).
    if get_schema_version(db) >= 2 && has_column(db, "forks", "generation") {
        return Ok(());
    }
    // v1 또는 부분 스키마 DB: 나머지 v1 테이블을 멱등 생성으로 채운 뒤 승격 경로 진입
    migrate_v1_tables(db)?;

    // v1 → v2: 백업은 BEGIN IMMEDIATE 이전에 수행해야 한다(VACUUM INTO는 tx 불가)
    if let Some(p) = db_path {
        backup_once_v1(db, p);
    }
    db.execute_batch("BEGIN IMMEDIATE").map_err(es)?;
    let result = (|| -> Result<(), String> {
        // 재판정(경합 안전 핵심): 다른 프로세스가 먼저 승격했으면 no-op
        if get_schema_version(db) >= 2 && has_column(db, "forks", "generation") {
            return Ok(());
        }
        if !has_column(db, "forks", "generation") {
            rebuild_forks(db)?;
        }
        if !has_table(db, "decision_events") {
            db.execute_batch(&decision_events_v2_ddl()).map_err(es)?;
        } else {
            alter_decision_events(db)?;
        }
        create_fork_intervals(db)?;
        backfill_event_cause(db)?;
        db.execute_batch("PRAGMA user_version = 2").map_err(es)?;
        db.execute_batch("COMMIT").map_err(es)?;
        Ok(())
    })();
    match result {
        Ok(()) => Ok(()),
        Err(err) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(err)
        }
    }
}

// ---------- 프로젝트 식별 (§3.4) ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectIdentity {
    pub project_id: String,
    pub project_label: String,
}

fn canonical(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

fn identity_for_path(real_root: &Path) -> ProjectIdentity {
    let s = real_root.to_string_lossy();
    // TS: sha256(realRoot).hex().slice(0, 16) — hex 16자(=앞 8바이트)
    let digest = Sha256::digest(s.as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{:02x}", b)).collect();
    let label = real_root
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| s.into_owned());
    ProjectIdentity {
        project_id: format!("proj_{}", hex),
        project_label: label,
    }
}

/// 프로젝트 스코핑(§3.4): git 루트 realpath sha256 앞 16자리. 비-git이면 cwd realpath 기준.
/// 해시 입력은 realpath 문자열 그 자체다(Node v1과 프로젝트별 project_id 동일성 보장).
pub fn resolve_project_identity(dir: &Path) -> ProjectIdentity {
    let root = match Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(dir)
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.is_empty() {
                canonical(dir)
            } else {
                PathBuf::from(s)
            }
        }
        _ => canonical(dir),
    };
    identity_for_path(&canonical(&root))
}

// ---------- agy 일일 예산 계측 (B2 — §6.4 강등 규칙용) ----------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgyBudgetRow {
    pub day: String,
    pub turns_used: i64,
    pub sessions_started: i64,
}

/// agy 일일 턴 예산 upsert — 단일 트랜잭션.
pub fn increment_agy_budget(
    db: &Connection,
    delta_turns: i64,
    delta_sessions: i64,
    now_ms_value: i64,
) -> Result<(), String> {
    let day = local_day(now_ms_value);
    db.execute_batch("BEGIN IMMEDIATE").map_err(es)?;
    let result = db.execute(
        "INSERT INTO agy_budget (day, turns_used, sessions_started, updated_at_ms)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(day) DO UPDATE SET
           turns_used = turns_used + excluded.turns_used,
           sessions_started = sessions_started + excluded.sessions_started,
           updated_at_ms = excluded.updated_at_ms",
        rusqlite::params![day, delta_turns, delta_sessions, now_ms_value],
    );
    match result {
        Ok(_) => db.execute_batch("COMMIT").map_err(es),
        Err(e) => {
            let _ = db.execute_batch("ROLLBACK");
            Err(es(e))
        }
    }
}

pub fn get_agy_budget(db: &Connection, now_ms_value: i64) -> Result<Option<AgyBudgetRow>, String> {
    let day = local_day(now_ms_value);
    let row = db
        .query_row(
            "SELECT day, turns_used, sessions_started FROM agy_budget WHERE day = ?1",
            [day],
            |r| {
                Ok(AgyBudgetRow {
                    day: r.get(0)?,
                    turns_used: r.get(1)?,
                    sessions_started: r.get(2)?,
                })
            },
        )
        .optional()
        .map_err(es)?;
    Ok(row)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, temp_dir};
    use std::sync::MutexGuard;

    fn mem_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.busy_timeout(std::time::Duration::from_millis(5000)).unwrap();
        conn
    }

    #[test]
    fn default_db_path_uses_xdg_state_home() {
        let _g = env_lock();
        std::env::set_var("XDG_STATE_HOME", "/tmp/xdg-state-abc");
        let p = default_db_path();
        assert_eq!(p, PathBuf::from("/tmp/xdg-state-abc/dgate/decision.db"));
        std::env::remove_var("XDG_STATE_HOME");
        let p2 = default_db_path();
        assert!(p2.ends_with(".local/state/dgate/decision.db") || p2.ends_with(".local/state/dgate/decision.db"));
        assert!(p2.is_absolute());
    }

    #[test]
    fn default_db_path_ignores_blank_xdg() {
        let _g = env_lock();
        std::env::set_var("XDG_STATE_HOME", "   ");
        let p = default_db_path();
        assert!(p.ends_with("dgate/decision.db"));
        assert!(p.to_string_lossy().contains(".local/state"));
        std::env::remove_var("XDG_STATE_HOME");
    }

    #[test]
    fn resolve_db_path_priority_explicit_envs_default() {
        let _g: MutexGuard<'_, ()> = env_lock();
        std::env::remove_var("DGATE_DB_PATH");
        std::env::remove_var("DECISION_ORCHESTRATOR_DB_PATH");
        std::env::remove_var("GEMINI_TRICK_DB_PATH");
        std::env::remove_var("XDG_STATE_HOME");

        // 1) explicit 최우선 (공백 explicit는 무시)
        assert_eq!(resolve_db_path(Some("/tmp/a.db")), PathBuf::from("/tmp/a.db"));
        assert_eq!(resolve_db_path(Some("  /tmp/b.db  ")), PathBuf::from("/tmp/b.db"));
        assert_ne!(resolve_db_path(Some("   ")), PathBuf::from("   "));
        // 2) DGATE_DB_PATH
        std::env::set_var("DGATE_DB_PATH", "/tmp/dgate.db");
        assert_eq!(resolve_db_path(None), PathBuf::from("/tmp/dgate.db"));
        assert_eq!(resolve_db_path(Some("/tmp/a.db")), PathBuf::from("/tmp/a.db"));
        // 3) DECISION_ORCHESTRATOR_DB_PATH
        std::env::set_var("DECISION_ORCHESTRATOR_DB_PATH", "/tmp/orch.db");
        assert_eq!(resolve_db_path(None), PathBuf::from("/tmp/dgate.db"));
        std::env::remove_var("DGATE_DB_PATH");
        assert_eq!(resolve_db_path(None), PathBuf::from("/tmp/orch.db"));
        // 4) GEMINI_TRICK_DB_PATH (레거시 별칭)
        std::env::set_var("GEMINI_TRICK_DB_PATH", "/tmp/legacy.db");
        assert_eq!(resolve_db_path(None), PathBuf::from("/tmp/orch.db"));
        std::env::remove_var("DECISION_ORCHESTRATOR_DB_PATH");
        assert_eq!(resolve_db_path(None), PathBuf::from("/tmp/legacy.db"));
        // 5) 기본 경로
        std::env::remove_var("GEMINI_TRICK_DB_PATH");
        assert_eq!(resolve_db_path(None), default_db_path());
    }

    #[test]
    fn project_identity_matches_git_root_and_hash_format() {
        // 이 저장소 자체(git repo)에서 호출 — git 루트 기준 식별
        let cwd = std::env::current_dir().unwrap();
        let ident = resolve_project_identity(&cwd);
        assert!(ident.project_id.starts_with("proj_"));
        assert_eq!(ident.project_id.len(), 5 + 16);
        assert!(ident.project_id[5..].chars().all(|c| c.is_ascii_hexdigit()));
        // 라벨은 git 루트 basename
        let root = canonical(
            &PathBuf::from(
                String::from_utf8_lossy(
                    &std::process::Command::new("git")
                        .args(["rev-parse", "--show-toplevel"])
                        .output()
                        .unwrap()
                        .stdout,
                )
                .trim()
                .to_string(),
            ),
        );
        assert_eq!(ident.project_label, root.file_name().unwrap().to_string_lossy());
        // 해시 입력은 realpath 문자열 — TS와 동일 계약 재계산 검증
        let expected = {
            let s = root.to_string_lossy();
            let d = Sha256::digest(s.as_bytes());
            d.iter().take(8).map(|b| format!("{:02x}", b)).collect::<String>()
        };
        assert_eq!(ident.project_id, format!("proj_{}", expected));
        // 결정론성
        assert_eq!(resolve_project_identity(&cwd), ident);
    }

    #[test]
    fn project_identity_non_git_dir_uses_realpath() {
        let tmp = temp_dir("non-git-ident");
        std::fs::create_dir_all(&tmp).unwrap();
        let ident = resolve_project_identity(&tmp);
        assert!(ident.project_id.starts_with("proj_"));
        // macOS /tmp → /private/tmp canonicalize 후에도 동일 식별
        let real = tmp.canonicalize().unwrap();
        let s = real.to_string_lossy();
        let d = Sha256::digest(s.as_bytes());
        let hex: String = d.iter().take(8).map(|b| format!("{:02x}", b)).collect();
        assert_eq!(ident.project_id, format!("proj_{}", hex));
        assert_eq!(ident.project_label, real.file_name().unwrap().to_string_lossy());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn open_db_writer_creates_dirs_and_v2_schema() {
        let tmp = temp_dir("writer-init");
        let db_path = tmp.join("nested/dir/decision.db");
        let conn = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        assert!(db_path.exists());
        assert_eq!(get_schema_version(&conn), 2);
        for table in [
            "stats_summary",
            "forks",
            "decision_events",
            "fork_intervals",
            "session_cursor",
            "agy_budget",
        ] {
            assert!(has_table(&conn, table), "table {} missing", table);
        }
        for col in ["generation", "attempt", "lease_owner", "lease_expires_ms"] {
            assert!(has_column(&conn, "forks", col), "forks.{} missing", col);
        }
        for col in ["cause", "cost_status", "cost_usd", "input_tokens", "output_tokens", "cache_read_tokens", "cache_write_tokens"] {
            assert!(has_column(&conn, "decision_events", col), "decision_events.{} missing", col);
        }
        // WAL 모드
        let mode: String = conn.query_row("PRAGMA journal_mode", [], |r| r.get(0)).unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        // fork_intervals 부분 유니크 인덱스
        let idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_fork_intervals_one_open'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(idx, 1);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn open_db_read_only_never_creates_and_enforces_query_only() {
        let tmp = temp_dir("ro-open");
        let db_path = tmp.join("decision.db");
        // 파일 없으면 명확한 오류
        let err = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions { read_only: true, busy_timeout_ms: None });
        assert!(err.is_err());
        assert!(!db_path.exists());
        // 생성 후 읽기전용 오픈 — 쓰기 거부
        {
            let w = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
            w.execute("INSERT INTO agy_budget (day, turns_used, sessions_started, updated_at_ms) VALUES ('2026-01-01', 1, 1, 0)", []).unwrap();
        }
        let ro = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions { read_only: true, busy_timeout_ms: None }).unwrap();
        let n: i64 = ro.query_row("SELECT turns_used FROM agy_budget", [], |r| r.get(0)).unwrap();
        assert_eq!(n, 1);
        assert!(ro.execute("INSERT INTO agy_budget (day) VALUES ('x')", []).is_err()); // NOT NULL 위반이든 query_only든 거부
        assert!(ro.execute("UPDATE agy_budget SET turns_used = 9", []).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn open_db_read_only_on_v1_db_does_not_migrate() {
        let tmp = temp_dir("ro-v1");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("v1.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            migrate_v1_tables(&conn).unwrap();
        }
        let ro = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions { read_only: true, busy_timeout_ms: None }).unwrap();
        assert_eq!(get_schema_version(&ro), 0);
        assert!(!has_column(&ro, "forks", "generation"));
        assert!(has_table(&ro, "forks"));
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn fresh_in_memory_db_is_v2_and_idempotent() {
        let conn = mem_db();
        migrate(&conn, None).unwrap();
        assert_eq!(get_schema_version(&conn), 2);
        assert!(has_column(&conn, "forks", "generation"));
        // 재마이그레이션 no-op
        migrate(&conn, None).unwrap();
        assert_eq!(get_schema_version(&conn), 2);
    }

    #[test]
    fn v1_to_v2_migration_is_lossless_with_backup() {
        let tmp = temp_dir("v1-migrate");
        std::fs::create_dir_all(&tmp).unwrap();
        let db_path = tmp.join("legacy.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            migrate_v1_tables(&conn).unwrap();
            conn.execute(
                "INSERT INTO forks (fork_id, project_id, project_label, conversation_id, route, tool_class, decision,
                 forced_split, p_at_gate, basis_at_gate, status, closed_by, neon_verdict, ttl_ms, created_at_ms,
                 last_touch_ms, closed_at_ms, metadata_json)
                 VALUES
                 ('fk_plain', 'proj_a', 'A', NULL, 'file_select', 'file_read', 'split', 0, 0.5, 'prior', 'ok', 'worker', NULL, 600000, 100, 200, 300, NULL),
                 ('fk_meta', 'proj_a', 'A', 'c1', 'tool_select', 'shell_exec', 'sharp', 1, 0.8, 'measured', 'open', NULL, NULL, 600000, 10, 20, NULL,
                  '{\"gen\":3,\"attempt\":2,\"lease_owner\":\"owner-1\",\"lease_until_ms\":99999}'),
                 ('fk_halflease', 'proj_a', 'A', NULL, 'tool_select', 'shell_exec', 'split', 0, 0.5, 'prior', 'open', NULL, NULL, 600000, 10, 20, NULL,
                  '{\"gen\":1,\"attempt\":1,\"lease_owner\":\"owner-2\"}'),
                 ('fk_wait', 'proj_a', 'A', NULL, 'file_select', 'file_edit', 'split', 0, 0.5, 'prior', 'open', NULL, NULL, 600000, 10, 20, NULL,
                  '{\"gen\":1,\"attempt\":1,\"waiting_human\":true,\"waiting_since_ms\":50}'),
                 ('fk_badmeta', 'proj_a', 'A', NULL, 'file_select', 'file_read', 'split', 0, 0.5, 'prior', 'open', NULL, NULL, 600000, 10, 20, NULL, 'not-json')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO decision_events (dedupe_key, fork_id, project_id, conversation_id, source, route,
                 tool_name, tool_class, ok, weight, is_retry, target_hash, error_snippet, created_at_ms)
                 VALUES
                 ('e1', NULL, 'proj_a', NULL, 'cli', 'tool_select', 'bash', 'shell_exec', 0, 1.0, 0, NULL, 'cause=tool_error; boom', 1000),
                 ('e2', NULL, 'proj_a', NULL, 'cli', 'tool_select', 'bash', 'shell_exec', 0, 1.0, 0, NULL, 'cause=not_a_cause; x', 1001),
                 ('e3', NULL, 'proj_a', NULL, 'cli', 'tool_select', 'bash', 'shell_exec', 0, 1.0, 0, NULL, 'plain failure', 1002)",
                [],
            )
            .unwrap();
        }
        // writer 오픈 → v1→v2 마이그레이션 자동 수행
        let conn = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        assert_eq!(get_schema_version(&conn), 2);
        assert!(has_column(&conn, "forks", "generation"));
        // 백업 파일 생성 — v1 원본 보존
        let mut found_backup = false;
        for e in std::fs::read_dir(&tmp).unwrap().flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with("legacy.db.v1.bak.") {
                found_backup = true;
            }
        }
        assert!(found_backup, "v1 backup must exist");

        // 손실 없는 리빌드 검증
        let count: i64 = conn.query_row("SELECT COUNT(*) FROM forks", [], |r| r.get(0)).unwrap();
        assert_eq!(count, 5);
        let get = |fk: &str, sql: &str| -> String {
            conn.query_row(&format!("SELECT {} FROM forks WHERE fork_id = ?", sql), [fk], |r| r.get::<_, String>(0))
                .unwrap_or_default()
        };
        let geti = |fk: &str, sql: &str| -> i64 {
            conn.query_row(&format!("SELECT {} FROM forks WHERE fork_id = ?", sql), [fk], |r| r.get::<_, i64>(0))
                .unwrap()
        };
        // fk_plain: 기본값 백필
        assert_eq!(geti("fk_plain", "generation"), 1);
        assert_eq!(geti("fk_plain", "attempt"), 1);
        assert!(conn.query_row("SELECT lease_owner IS NULL FROM forks WHERE fork_id='fk_plain'", [], |r| r.get::<_, i64>(0)).unwrap() == 1);
        assert_eq!(get("fk_plain", "status"), "ok");
        // fk_meta: 메타 승격
        assert_eq!(geti("fk_meta", "generation"), 3);
        assert_eq!(geti("fk_meta", "attempt"), 2);
        assert_eq!(get("fk_meta", "lease_owner"), "owner-1");
        assert_eq!(geti("fk_meta", "lease_expires_ms"), 99999);
        assert_eq!(get("fk_meta", "status"), "open");
        // fk_halflease: 절반 리스는 NULL 쌍으로 정규화
        assert!(conn.query_row("SELECT lease_owner IS NULL AND lease_expires_ms IS NULL FROM forks WHERE fork_id='fk_halflease'", [], |r| r.get::<_, i64>(0)).unwrap() == 1);
        // fk_wait: open + waiting_human 메타 → waiting_human 승격
        assert_eq!(get("fk_wait", "status"), "waiting_human");
        // fk_badmeta: 손상 JSON은 가드 통과(기본값)
        assert_eq!(geti("fk_badmeta", "generation"), 1);
        assert_eq!(get("fk_badmeta", "status"), "open");

        // cause 백필: 유효 enum만
        let c1: Option<String> = conn.query_row("SELECT cause FROM decision_events WHERE dedupe_key='e1'", [], |r| r.get(0)).unwrap();
        assert_eq!(c1.as_deref(), Some("tool_error"));
        let c2: Option<String> = conn.query_row("SELECT cause FROM decision_events WHERE dedupe_key='e2'", [], |r| r.get(0)).unwrap();
        assert_eq!(c2, None);
        let c3: Option<String> = conn.query_row("SELECT cause FROM decision_events WHERE dedupe_key='e3'", [], |r| r.get(0)).unwrap();
        assert_eq!(c3, None);

        // v2 컬럼·구간 테이블·유니크 인덱스
        assert!(has_table(&conn, "fork_intervals"));
        // 재오픈(핫패스) — 백업 추가 생성 없음
        drop(conn);
        let conn2 = open_db(Some(db_path.to_str().unwrap()), OpenDbOptions::default()).unwrap();
        assert_eq!(get_schema_version(&conn2), 2);
        let backups = std::fs::read_dir(&tmp)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with("legacy.db.v1.bak."))
            .count();
        assert_eq!(backups, 1, "backup must be created exactly once");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn concurrent_new_db_init_8_threads_single_v2_schema() {
        let tmp = temp_dir("concurrent-init");
        let db_path = tmp.join("race.db");
        let path_str = db_path.to_str().unwrap().to_string();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let p = path_str.clone();
            handles.push(std::thread::spawn(move || {
                open_db(Some(&p), OpenDbOptions::default())
            }));
        }
        let mut ok = 0;
        for h in handles {
            match h.join().unwrap() {
                Ok(conn) => {
                    assert_eq!(get_schema_version(&conn), 2);
                    ok += 1;
                }
                Err(e) => panic!("thread open failed: {}", e),
            }
        }
        assert_eq!(ok, 8);
        let conn = open_db(Some(&path_str), OpenDbOptions { read_only: true, busy_timeout_ms: None }).unwrap();
        assert_eq!(get_schema_version(&conn), 2);
        // 테이블이 정확히 1회 생성(스키마 이상 없음)
        let forks: i64 = conn
            .query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='forks'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(forks, 1);
        let cols: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_table_info('forks')", [], |r| r.get(0))
            .unwrap();
        assert_eq!(cols, 22, "v2 forks column count");
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn agy_budget_upsert_accumulates_per_day() {
        let conn = mem_db();
        migrate(&conn, None).unwrap();
        let now = 1_700_000_000_000i64;
        assert!(get_agy_budget(&conn, now).unwrap().is_none());
        increment_agy_budget(&conn, 3, 1, now).unwrap();
        increment_agy_budget(&conn, 2, 0, now).unwrap();
        let b = get_agy_budget(&conn, now).unwrap().unwrap();
        assert_eq!(b.turns_used, 5);
        assert_eq!(b.sessions_started, 1);
        // 날짜 키 형식
        assert!(regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(&b.day));
        // 다른 날(가까운 미래=다른 날짜 가능성 있음)은 별도 행 — 최소한 크래시 없음
        increment_agy_budget(&conn, 1, 0, now + 90 * 86_400_000).unwrap();
        assert!(get_agy_budget(&conn, now + 90 * 86_400_000).unwrap().is_some());
    }

    #[test]
    fn has_column_rejects_non_identifier() {
        let conn = mem_db();
        migrate(&conn, None).unwrap();
        assert!(!has_column(&conn, "forks; DROP TABLE forks", "x"));
        assert!(!has_column(&conn, "9bad", "x"));
        assert!(has_column(&conn, "forks", "fork_id"));
    }

    #[test]
    fn local_day_matches_local_time_format() {
        let d = local_day(1_700_000_000_000);
        assert!(regex::Regex::new(r"^\d{4}-\d{2}-\d{2}$").unwrap().is_match(&d), "{}", d);
    }
}
