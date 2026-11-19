//! Node v1 ↔ Rust v2 SQLite 크로스호환 검증(brief-rust-cli §2.2 — CRITICAL).
//!
//! 1. Node v1(`src/decision/dg.ts`)로 DB 생성·기록(route/record/wrap)
//! 2. Rust v2(`dg`)가 같은 DB를 조회·변형(stats/route/report/close)
//! 3. Node v1이 다시 읽기(stats/report)
//! 4. project_id 해시 100% 동일 · PRAGMA user_version==2 · integrity_check ok ·
//!    total_events 일치 · 사후확률 p 1e-6 일치 · 양방향 스키마 오류 0 을 단언.
//!
//! 실행: `cargo test -p dgate-cli --test node_rust_compat`
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rusqlite::Connection;
use serde_json::Value;

const DG: &str = env!("CARGO_BIN_EXE_dg");
const NODE_DG: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../src/decision/dg.ts"
);

fn temp_base() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dg-compat-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn node_path() -> &'static str {
    // node >= 22.6 필요(네이티브 TS + node:sqlite). 부재 시 즉시 실패 — 실측 크로스호환이 목적.
    which("node").expect("node가 PATH에 있어야 Node v1 크로스호환 검증 가능")
}

fn which(name: &str) -> Option<&'static str> {
    let path = std::env::var("PATH").unwrap_or_default();
    for dir in path.split(':') {
        let cand = Path::new(dir).join(name);
        if cand.is_file() {
            // 정적 문자열로 돌려주기 위해 누적 버퍼 사용
            return Some(Box::leak(cand.to_string_lossy().into_owned().into_boxed_str()));
        }
    }
    None
}

struct RunResult {
    status: i32,
    stdout: String,
    stderr: String,
}

fn run_node_dg(args: &[&str], db: &str) -> RunResult {
    // 주의: `--`(자식 명령 구분자) 뒤에는 어떤 플래그도 붙일 수 없다 —
    // --json/--db는 반드시 `--` 이전에 삽입한다(그렇지 않으면 wrap 자식 argv로 흘러
    // 기본 DB 경로로 오염된다).
    let mut full: Vec<&str> = args.iter().copied().take_while(|a| *a != "--").collect();
    let tail: Vec<&str> = args.iter().copied().skip_while(|a| *a != "--").collect();
    full.push("--json");
    full.push("--db");
    full.push(db);
    full.extend(tail);
    let out = Command::new(node_path())
        .arg(NODE_DG)
        .args(&full)
        .env_remove("DGATE_DB_PATH")
        .env_remove("DECISION_ORCHESTRATOR_DB_PATH")
        .env_remove("GEMINI_TRICK_DB_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("node dg.ts spawn");
    RunResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn run_rust_dg(args: &[&str], db: &str) -> RunResult {
    // run_node_dg와 동일한 `--` 규칙 — 플래그는 구분자 이전에.
    let mut full: Vec<&str> = args.iter().copied().take_while(|a| *a != "--").collect();
    let tail: Vec<&str> = args.iter().copied().skip_while(|a| *a != "--").collect();
    full.push("--json");
    full.push("--db");
    full.push(db);
    full.extend(tail);
    let out = Command::new(DG)
        .args(&full)
        .env_remove("DGATE_DB_PATH")
        .env_remove("DECISION_ORCHESTRATOR_DB_PATH")
        .env_remove("GEMINI_TRICK_DB_PATH")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("dg spawn");
    RunResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn last_json(stdout: &str) -> Value {
    let last = stdout.trim().lines().last().unwrap_or("");
    serde_json::from_str(last).unwrap_or_else(|e| panic!("JSON 아님: {e} ({last})"))
}

fn init_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    for args in [
        vec!["init", "-q"],
        vec!["config", "user.email", "compat@example.com"],
        vec!["config", "user.name", "compat"],
        vec!["config", "commit.gpgsign", "false"],
    ] {
        let out = Command::new("git").arg("-C").arg(&repo).args(&args).output().unwrap();
        assert!(out.status.success(), "git {args:?}");
    }
    std::fs::write(repo.join("seed.txt"), "v0\n").unwrap();
    let out = Command::new("git")
        .arg("-C").arg(&repo).args(["add", "-A"])
        .output().unwrap();
    assert!(out.status.success());
    let out = Command::new("git")
        .arg("-C").arg(&repo).args(["commit", "-qm", "init"])
        .output().unwrap();
    assert!(out.status.success());
    repo
}

fn total_events(db: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("SELECT COUNT(*) FROM decision_events", [], |r| r.get(0)).unwrap()
}

fn user_version(db: &str) -> i64 {
    let conn = Connection::open(db).unwrap();
    conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap()
}

fn integrity_check(db: &str) -> String {
    let conn = Connection::open(db).unwrap();
    conn.query_row("PRAGMA integrity_check", [], |r| r.get(0)).unwrap()
}

#[test]
fn test_node_rust_sqlite_cross_compatibility() {
    let node = which("node").unwrap_or_else(|| panic!("node 미설치 — 크로스호환 검증 불가"));
    let node_version = Command::new(node).arg("--version").output().unwrap();
    let nv = String::from_utf8_lossy(&node_version.stdout).trim().to_string();
    eprintln!("node = {nv} ({node})");
    let major: u32 = nv.trim_start_matches('v').split('.').next().unwrap().parse().unwrap_or(0);
    assert!(major >= 22, "node >= 22.6 필요(현재 {nv})");

    let base = temp_base();
    let repo = init_repo(&base);
    let repo_s = repo.display().to_string();
    let db = base.join("compat.db");
    let dbs = db.display().to_string();

    // ---------- Phase A: Node v1 이 DB 생성·기록 ----------
    let route = run_node_dg(&["route", "--task-kind", "single_edit", "--project", &repo_s], &dbs);
    assert_eq!(route.status, 0, "node route: {}", route.stderr);
    let route_json = last_json(&route.stdout);
    let node_fork_1 = route_json["fork_id"].as_str().unwrap().to_string();
    let node_project_id = route_json["project_id"].as_str().unwrap().to_string();
    assert!(node_project_id.starts_with("proj_"), "project_id 형식: {node_project_id}");

    let record = run_node_dg(
        &["record", "--route", "tool_select", "--tool-class", "shell_exec",
          "--tool", "run_command", "--ok", "1", "--project", &repo_s],
        &dbs,
    );
    assert_eq!(record.status, 0, "node record: {}", record.stderr);
    assert_eq!(last_json(&record.stdout)["recorded"], true);

    let wrap = run_node_dg(
        &["wrap", "--route", "file_select", "--tool-class", "file_edit",
          "--project", &repo_s, "--", "/bin/sh", "-c", "echo compat-ok"],
        &dbs,
    );
    assert_eq!(wrap.status, 0, "node wrap: {}", wrap.stderr);

    let events_after_node = total_events(&dbs);
    assert!(events_after_node >= 2, "Node 기록 후 이벤트 {events_after_node}건");

    // ---------- Phase B: Rust v2 이 같은 DB를 조회·변형 ----------
    // (1) stats — Node가 만든 v2 DB를 읽기전용 조회
    let stats = run_rust_dg(&["stats", "--cost", "--timing"], &dbs);
    assert_eq!(stats.status, 0, "rust stats on node DB: {}", stats.stderr);
    let stats_json = last_json(&stats.stdout);
    assert_eq!(stats_json["cost"]["schema_v1"], false, "Node v1이 만든 DB는 v2 스키마");
    assert_eq!(
        stats_json["cost"]["total_events"].as_i64().unwrap(),
        events_after_node,
        "Rust stats total_events == SQL 직접 카운트"
    );

    // (2) route — project_id 해시 100% 동일 단언
    let rroute = run_rust_dg(&["route", "--task-kind", "code_search", "--project", &repo_s], &dbs);
    assert_eq!(rroute.status, 0, "rust route: {}", rroute.stderr);
    let rroute_json = last_json(&rroute.stdout);
    let rust_project_id = rroute_json["project_id"].as_str().unwrap();
    assert_eq!(
        rust_project_id, node_project_id,
        "CRITICAL: project_id 해시가 Node v1과 Rust v2에서 동일해야 함"
    );
    let rust_fork_2 = rroute_json["fork_id"].as_str().unwrap().to_string();

    // (3) report — 읽기전용
    let rreport = run_rust_dg(&["report"], &dbs);
    assert_eq!(rreport.status, 0, "rust report on node DB: {}", rreport.stderr);

    // (4) close — Rust가 Node가 발행한 fork를 닫는다(교차 생명주기)
    let rclose = run_rust_dg(&["close", "--fork", &node_fork_1, "--outcome", "ok", "--by", "dispatcher"], &dbs);
    assert_eq!(rclose.status, 0, "rust close of node fork: {}", rclose.stderr);
    assert_eq!(last_json(&rclose.stdout)["recorded"], true);

    let events_after_rust = total_events(&dbs);
    assert!(events_after_rust > events_after_node, "Rust close가 표본을 추가 기록");

    // ---------- Phase C: Node v1 이 Rust가 변형한 DB를 다시 읽는다 ----------
    let nstats = run_node_dg(&["stats", "--cost", "--timing"], &dbs);
    assert_eq!(nstats.status, 0, "node stats after rust writes: {}", nstats.stderr);
    let nstats_json = last_json(&nstats.stdout);
    assert_eq!(
        nstats_json["cost"]["total_events"].as_i64().unwrap(),
        events_after_rust,
        "Node 재조회 total_events == Rust 조작 후 SQL 카운트 (양방향 정합)"
    );

    let nreport = run_node_dg(&["report"], &dbs);
    assert_eq!(nreport.status, 0, "node report after rust writes: {}", nreport.stderr);
    let nreport_json = last_json(&nreport.stdout);
    assert_eq!(nreport_json["report"]["schema_version"], 1);

    // ---------- 사후확률 p 동일성(±1e-6) — 같은 키(tool_select/shell_exec)로 양쪽 route ----------
    let probe_node = run_node_dg(&["route", "--task-kind", "static_check", "--project", &repo_s], &dbs);
    assert_eq!(probe_node.status, 0);
    let p_node = last_json(&probe_node.stdout)["p"].as_f64().unwrap();
    let probe_rust = run_rust_dg(&["route", "--task-kind", "static_check", "--project", &repo_s], &dbs);
    assert_eq!(probe_rust.status, 0);
    let p_rust = last_json(&probe_rust.stdout)["p"].as_f64().unwrap();
    assert!(
        (p_node - p_rust).abs() < 1e-6,
        "사후확률 불일치: node p={p_node} rust p={p_rust}"
    );

    // ---------- 스키마·무결성 ----------
    assert_eq!(user_version(&dbs), 2, "PRAGMA user_version == 2 (v2 스키마)");
    assert_eq!(integrity_check(&dbs), "ok", "PRAGMA integrity_check == ok");
    let conn = Connection::open(&dbs).unwrap();
    let fk: i64 = conn
        .query_row("PRAGMA foreign_key_check", [], |r| r.get(0))
        .map(|v: i64| v)
        .unwrap_or(0);
    assert_eq!(fk, 0, "foreign_key_check 위반 0");

    // ---------- Rust가 발행한 fork를 Node도 조작할 수 있는지(교차 방향) ----------
    let ntouch = run_node_dg(&["touch", "--fork", &rust_fork_2], &dbs);
    assert_eq!(ntouch.status, 0, "node touch of rust fork: {}", ntouch.stderr);
    let nclose = run_node_dg(&["close", "--fork", &rust_fork_2, "--outcome", "ok", "--by", "dispatcher"], &dbs);
    assert_eq!(nclose.status, 0, "node close of rust fork: {}", nclose.stderr);
    assert_eq!(last_json(&nclose.stdout)["recorded"], true);

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_project_id_hash_algorithm_identity() {
    // project_id = sha256(realpath).hex[0:16] — Node v1 db.ts와 Rust v2 db.rs가
    // 같은 입력 경로에서 같은 해시를 내는지 알고리즘 차원에서 단언.
    use sha2::{Digest, Sha256};
    let base = temp_base();
    let repo = init_repo(&base);
    let repo_s = repo.display().to_string();

    let rust = run_rust_dg(&["route", "--task-kind", "explore_read", "--project", &repo_s], &(base.join("pid.db").display().to_string()));
    assert_eq!(rust.status, 0, "{}", rust.stderr);
    let pid_rust = last_json(&rust.stdout)["project_id"].as_str().unwrap().to_string();

    let node = run_node_dg(
        &["route", "--task-kind", "explore_read", "--project", &repo_s],
        &(base.join("pid2.db").display().to_string()),
    );
    assert_eq!(node.status, 0, "{}", node.stderr);
    let pid_node = last_json(&node.stdout)["project_id"].as_str().unwrap().to_string();

    // realpath(canonicalize) 기준 해시 재현
    let real = std::fs::canonicalize(&repo).unwrap();
    let digest = Sha256::digest(real.to_string_lossy().as_bytes());
    let hex: String = digest.iter().take(8).map(|b| format!("{b:02x}")).collect();
    let expected = format!("proj_{hex}");
    assert_eq!(pid_rust, expected, "Rust project_id == sha256(realpath)[0:16]");
    assert_eq!(pid_node, expected, "Node project_id == sha256(realpath)[0:16]");
    assert_eq!(pid_rust, pid_node, "양방향 project_id 동일");

    std::fs::remove_dir_all(&base).unwrap();
}
