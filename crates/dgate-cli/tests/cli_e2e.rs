//! CLI 종단간 검증(brief-rust-cli §2.1) — 컴파일된 `dg` 바이너리를 직접 호출해
//! 전체 생명주기·읽기전용 불변성·게이트 종료코드·훅·setup을 검증한다.
//!
//! 실행: `cargo test -p dgate-cli --test cli_e2e`
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;
use sha2::{Digest, Sha256};

const DG: &str = env!("CARGO_BIN_EXE_dg");

fn temp_base(prefix: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "dg-e2e-{}-{}-{}",
        prefix,
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn sha256_file(p: &Path) -> String {
    let data = std::fs::read(p).unwrap();
    let mut h = Sha256::new();
    h.update(&data);
    h.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

struct DgResult {
    status: i32,
    stdout: String,
    stderr: String,
}

fn dg_with_env(args: &[&str], envs: &[(&str, &str)], stdin: Option<&str>, cwd: Option<&Path>) -> DgResult {
    let mut cmd = Command::new(DG);
    cmd.args(args);
    // DB 격리 — 어떤 테스트도 사용자 기본 DB를 건드리지 않는다
    cmd.env_remove("DGATE_DB_PATH");
    cmd.env_remove("DECISION_ORCHESTRATOR_DB_PATH");
    cmd.env_remove("GEMINI_TRICK_DB_PATH");
    for (k, v) in envs {
        cmd.env(k, v);
    }
    if let Some(dir) = cwd {
        cmd.current_dir(dir);
    }
    if stdin.is_some() {
        cmd.stdin(std::process::Stdio::piped());
    }
    // spawn()은 기본적으로 stdout/stderr을 상속한다 — wait_with_output로 읽으려면
    // 명시적으로 파이프해야 한다.
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("dg spawn");
    if let Some(input) = stdin {
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(input.as_bytes())
            .unwrap();
    }
    let out = child.wait_with_output().expect("dg wait");
    DgResult {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn dg(args: &[&str], envs: &[(&str, &str)]) -> DgResult {
    dg_with_env(args, envs, None, None)
}

/// exit 0 + 마지막 stdout 줄을 JSON로 파싱
fn dg_json(args: &[&str], envs: &[(&str, &str)]) -> Value {
    let mut all = args.to_vec();
    all.push("--json");
    let r = dg(&all, envs);
    assert_eq!(r.status, 0, "dg {} exit={} stderr={}", all.join(" "), r.status, r.stderr);
    let last = r.stdout.trim().lines().last().unwrap_or("");
    serde_json::from_str(last).unwrap_or_else(|e| panic!("invalid JSON from dg {}: {e} ({last})", all.join(" ")))
}

fn git_ok(cwd: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .expect("git spawn");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn init_repo(base: &Path) -> PathBuf {
    let repo = base.join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git_ok(&repo, &["init", "-q"]);
    git_ok(&repo, &["config", "user.email", "e2e@example.com"]);
    git_ok(&repo, &["config", "user.name", "e2e-test"]);
    git_ok(&repo, &["config", "commit.gpgsign", "false"]);
    std::fs::write(repo.join("base.txt"), "v0\n").unwrap();
    git_ok(&repo, &["add", "-A"]);
    git_ok(&repo, &["commit", "-qm", "init"]);
    repo
}

/// 스테이지드+언스테이지드+언트랙 dirty 워크트리
fn add_worktree(repo: &Path, name: &str) -> PathBuf {
    let wt = repo.parent().unwrap().join(name);
    git_ok(repo, &["worktree", "add", "-q", "-b", name, &wt.display().to_string()]);
    std::fs::write(wt.join("base.txt"), "v2-idx\n").unwrap();
    git_ok(&wt, &["add", "base.txt"]);
    std::fs::write(wt.join("base.txt"), "v2-wt\n").unwrap();
    std::fs::create_dir_all(wt.join("notes")).unwrap();
    std::fs::write(wt.join("notes/draft.txt"), "draft\n").unwrap();
    wt
}

// ---------------------------------------------------------------------------

#[test]
fn test_01_route_wrap_lifecycle_stats_report_readonly() {
    let base = temp_base("lifecycle");
    let repo = init_repo(&base);
    let db = base.join("lifecycle.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];
    let repo_s = repo.display().to_string();

    // ---------- route → fork 발행 ----------
    let route = dg_json(
        &["route", "--task-kind", "single_edit", "--project", &repo_s],
        &envs,
    );
    let fork_id = route["fork_id"].as_str().unwrap().to_string();
    assert!(fork_id.starts_with("fk_"), "fork id 형식: {fork_id}");
    assert_eq!(route["task_kind"], "single_edit");
    assert_eq!(route["decision"], "split", "초기 p<θ → split");

    // ---------- wrap 성공(산출물 AND 게이트 통과) ----------
    let artifact = base.join("out.txt");
    let r = dg(
        &[
            "wrap", "--route", "file_select", "--tool-class", "file_edit",
            "--fork", &fork_id, "--project", &repo_s,
            "--require-artifact", &artifact.display().to_string(),
            "--require-nonempty", "--require-stdout-nonempty",
            "--stall-ms", "8000",
            "--", "/bin/sh", "-c", &format!("echo working; echo result > '{}'; echo done", artifact.display()),
        ],
        &envs,
    );
    assert_eq!(r.status, 0, "wrap success exit=0 (stderr={})", r.stderr);
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["ok"], 1, "AND 게이트 통과: {summary}");
    assert_eq!(summary["outcome"], "ok");
    assert_eq!(summary["committed"], true);
    assert_eq!(summary["closed"], true);

    // ---------- wrap 스톨 타임아웃 — 프로세스 그룹 전체 킬 ----------
    let gate2 = dg_json(&["gate", "--route", "tool_select", "--tool-class", "shell_exec", "--project", &repo_s], &envs);
    let fork2 = gate2["fork_id"].as_str().unwrap().to_string();
    let started = std::time::Instant::now();
    let r = dg(
        &[
            "wrap", "--route", "tool_select", "--tool-class", "shell_exec",
            "--fork", &fork2, "--project", &repo_s,
            "--stall-ms", "800", "--kill-grace-ms", "300", "--max-ms", "60000",
            "--", "/bin/sh", "-c", "echo start; /bin/sh -c 'sleep 60' ; echo never",
        ],
        &envs,
    );
    let elapsed = started.elapsed();
    assert_eq!(r.status, 143, "SIGTERM 그룹 킬 → 128+15 (got {})", r.status);
    assert!(elapsed.as_secs() < 10, "워치독이 60s 슬립을 조기 킬해야 함: {elapsed:?}");
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["stalled"], true);
    assert_eq!(summary["cause"], "stall");
    assert_eq!(summary["outcome"], "fail");
    assert_eq!(summary["committed"], true, "스톨 실패도 단일 Tx 커밋");
    // 손자 sleep 60도 그룹 킬로 소멸 확인
    let leftover = Command::new("pgrep").args(["-f", "sleep 60"]).output().unwrap();
    assert!(
        String::from_utf8_lossy(&leftover.stdout).trim().is_empty(),
        "프로세스 그룹 킬 후 sleep 60 잔존 없음"
    );

    // ---------- wrap 산출물 누락 → fail 기록(exit은 자식 종료코드) ----------
    let gate3 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let fork3 = gate3["fork_id"].as_str().unwrap().to_string();
    let r = dg(
        &[
            "wrap", "--route", "file_select", "--tool-class", "file_edit",
            "--fork", &fork3, "--project", &repo_s,
            "--require-artifact", &base.join("missing.txt").display().to_string(),
            "--", "/bin/sh", "-c", "echo clean-but-no-artifact",
        ],
        &envs,
    );
    assert_eq!(r.status, 0, "자식 exit 0 → wrap exit 0(계약 위반은 DB에 fail)");
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["ok"], 0);
    assert_eq!(summary["cause"], "artifact_missing");
    assert_eq!(summary["outcome"], "fail");

    // ---------- wrap rerouted — 첫 줄 "rerouted" → 표본·실패 아님 ----------
    let gate4 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let fork4 = gate4["fork_id"].as_str().unwrap().to_string();
    let r = dg(
        &[
            "wrap", "--route", "file_select", "--tool-class", "file_edit",
            "--fork", &fork4, "--project", &repo_s,
            "--require-stdout-nonempty",
            "--", "/bin/sh", "-c", "echo rerouted",
        ],
        &envs,
    );
    assert_eq!(r.status, 0);
    let summary: Value = serde_json::from_str(r.stderr.trim().lines().last().unwrap()).unwrap();
    assert_eq!(summary["outcome"], "rerouted");
    assert_eq!(summary["cause"], "interrupted");

    // ---------- 생명주기 나머지: touch/attempt/close/retro/settle/compact/record ----------
    let gate5 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_read", "--project", &repo_s], &envs);
    let fork5 = gate5["fork_id"].as_str().unwrap().to_string();
    let touch = dg_json(&["touch", "--fork", &fork5, "--ttl-ms", "900000"], &envs);
    assert_eq!(touch["ttl_ms"], 900000);
    let att = dg_json(&["attempt", "--fork", &fork5, "--bump-gen"], &envs);
    assert_eq!(att["generation"], 2);
    assert_eq!(att["attempt"], 2);
    let rec = dg_json(
        &["record", "--route", "tool_select", "--tool-class", "shell_exec", "--tool", "run_command",
          "--ok", "1", "--project", &repo_s],
        &envs,
    );
    assert_eq!(rec["recorded"], true);
    let close = dg_json(&["close", "--fork", &fork5, "--outcome", "ok", "--by", "dispatcher"], &envs);
    assert_eq!(close["status"], "ok");
    let retro = dg_json(&["retro", "--fork", &fork5], &envs);
    assert!(retro["markdown"].as_str().unwrap().contains("Loop Retro"));
    let settle = dg_json(&["settle-orphans"], &envs);
    assert_eq!(settle["settled"], 0);
    let compact = dg_json(&["compact"], &envs);
    assert!(compact["remaining_events"].as_i64().unwrap() > 0);

    // ---------- close 책임 매트릭스: ttl은 ok/fail 불가 ----------
    let gate6 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_read", "--project", &repo_s], &envs);
    let fork6 = gate6["fork_id"].as_str().unwrap().to_string();
    let r = dg(&["close", "--fork", &fork6, "--outcome", "fail", "--by", "ttl", "--json"], &envs);
    assert_eq!(r.status, 2, "close by=ttl outcome=fail → UsageError exit 2");

    // ---------- stats: 읽기전용 — DB sha256 불변 ----------
    let hash_before = sha256_file(&db);
    let stats = dg_json(&["stats", "--cost", "--timing", "--project", &repo_s], &envs);
    assert_eq!(stats["cmd"], "stats");
    assert_eq!(stats["cost"]["schema_v1"], false);
    let total = stats["cost"]["total_events"].as_i64().unwrap();
    assert!(total >= 5, "wrap/record/close 이벤트 집계: {total}");
    assert_eq!(stats["timing"]["intervals_unavailable"], false);
    assert_eq!(sha256_file(&db), hash_before, "stats 실행 후 DB 파일 sha256 불변");

    // ---------- report: allowlist 거부 exit 2 + 파일 0개 생성 + DB 불변 ----------
    for denied in [repo.join("src/evil.md"), repo.join(".agents/evil.md")] {
        let r = dg(
            &["report", "--db", &dbs, "--repo", &repo_s, "--out", &denied.display().to_string()],
            &envs,
        );
        assert_eq!(r.status, 2, "거부 경로 {} → exit 2", denied.display());
        assert!(!denied.exists(), "거부 경로 파일 미생성");
        assert!(
            !denied.parent().map(|p| p.join(".gitkeep").exists()).unwrap_or(false)
        );
    }
    let allowed = repo.join(".herdr/report/r.md");
    let rep = dg_json(&["report", "--repo", &repo_s, "--out", &allowed.display().to_string()], &envs);
    assert_eq!(rep["written"], true);
    assert!(allowed.exists(), ".herdr/report 허용 경로 생성");
    assert_eq!(sha256_file(&db), hash_before, "report 실행 후에도 DB 불변");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_02_backprop_verdict_pipeline_fail_closed() {
    let base = temp_base("verdict");
    let repo = init_repo(&base);
    let db = base.join("v.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];
    let repo_s = repo.display().to_string();

    let mk_verdict = |fork: &str, decision: &str, findings: Value, path: &str| {
        let v = serde_json::json!({
            "schema_version": 1,
            "fork_id": fork,
            "decision": decision,
            "reason": "e2e verdict",
            "findings": findings,
        });
        std::fs::write(path, v.to_string()).unwrap();
    };

    // (a) pass verdict → exit 0
    let g1 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f1 = g1["fork_id"].as_str().unwrap().to_string();
    mk_verdict(&f1, "pass", serde_json::json!([]), &base.join("v-ok.json").display().to_string());
    let r = dg(
        &["backprop", "--fork", &f1, "--verdict-file", &base.join("v-ok.json").display().to_string()],
        &envs,
    );
    assert_eq!(r.status, 0, "pass verdict → exit 0 (stderr={})", r.stderr);

    // (b) 모순 verdict(pass + P0 finding) → fail-closed exit 3
    let g2 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f2 = g2["fork_id"].as_str().unwrap().to_string();
    mk_verdict(
        &f2,
        "pass",
        serde_json::json!([{"severity": "P0", "title": "broken", "evidence": "hard fail"}]),
        &base.join("v-bad.json").display().to_string(),
    );
    let r = dg(&["backprop", "--fork", &f2, "--verdict-file", &base.join("v-bad.json").display().to_string(), "--json"], &envs);
    assert_eq!(r.status, 3, "모순 verdict → fail-closed exit 3");
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["valid"], false);
    assert_eq!(parsed["effective_decision"], "human");
    assert_eq!(parsed["exit_code"], 3);

    // (c) stale HEAD — expected-head 불일치 → exit 3
    let g3 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f3 = g3["fork_id"].as_str().unwrap().to_string();
    mk_verdict(&f3, "pass", serde_json::json!([]), &base.join("v-stale.json").display().to_string());
    let r = dg(
        &[
            "backprop", "--fork", &f3, "--verdict-file", &base.join("v-stale.json").display().to_string(),
            "--expected-head", "0123456789abcdef0123456789abcdef01234567",
        ],
        &envs,
    );
    assert_eq!(r.status, 3, "stale verdict → exit 3");

    // (d) verdict 파일 부재 → exit 3 (exit 2가 아님 — fail-closed)
    let r = dg(
        &["backprop", "--fork", &f3, "--verdict-file", &base.join("nope.json").display().to_string()],
        &envs,
    );
    assert_eq!(r.status, 3, "missing verdict file → exit 3");

    // (e) fail verdict + evidence → backprop_fail exit 0
    let g4 = dg_json(&["gate", "--route", "file_select", "--tool-class", "file_edit", "--project", &repo_s], &envs);
    let f4 = g4["fork_id"].as_str().unwrap().to_string();
    mk_verdict(
        &f4,
        "fail",
        serde_json::json!([{"severity": "P1", "title": "wrong logic", "evidence": "line 42"}]),
        &base.join("v-fail.json").display().to_string(),
    );
    let r = dg(&["backprop", "--fork", &f4, "--verdict-file", &base.join("v-fail.json").display().to_string(), "--json"], &envs);
    assert_eq!(r.status, 0, "fail verdict → backprop_fail exit 0");
    let parsed: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["action"], "backprop_fail");

    // (f) 수동 backprop — backprop_fail 상태 재역귀속 거부(f1은 ok라 재개방 허용됨)
    let r = dg(&["backprop", "--fork", &f4, "--penalty", "2.0", "--json"], &envs);
    assert_eq!(r.status, 2, "backprop_fail 상태 재역귀속 → UsageError 2");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_03_checkpoint_worktree_gates() {
    let base = temp_base("ckpt");
    let repo = init_repo(&base);
    let wt = add_worktree(&repo, "coder-e2e");
    let ckpt_root = base.join("ckpt");
    let repo_s = repo.display().to_string();
    let envs: [(&str, &str); 0] = [];

    // create → verify → restore 왕복
    let created = dg_json(
        &[
            "checkpoint", "create", "--worktree", &wt.display().to_string(),
            "--run-id", "run-e2e", "--repo", &repo_s,
            "--checkpoint-root", &ckpt_root.display().to_string(),
        ],
        &envs,
    );
    assert_eq!(created["saved"], true, "create: {:?}", created["errors"]);
    let ckpt_dir = created["checkpoint_dir"].as_str().unwrap().to_string();
    assert_eq!(Path::new(&ckpt_dir), ckpt_root.join("run-e2e/coder-e2e"));

    let verified = dg_json(&["checkpoint", "verify", "--dir", &ckpt_dir], &envs);
    assert_eq!(verified["valid"], true);

    let target = base.join("restore-wt");
    git_ok(&repo, &["worktree", "add", "-q", "-b", "restore-wt", &target.display().to_string()]);
    let restored = dg_json(
        &["checkpoint", "restore", "--dir", &ckpt_dir, "--target", &target.display().to_string()],
        &envs,
    );
    assert_eq!(restored["restored"], true, "restore: {:?}", restored["errors"]);
    assert_eq!(std::fs::read_to_string(target.join("base.txt")).unwrap(), "v2-wt\n", "unstaged 복원");
    assert!(target.join("notes/draft.txt").exists(), "untracked 복원");

    // 1바이트 변조 → verify exit 1
    let mut patch = std::fs::read(Path::new(&ckpt_dir).join("index.patch")).unwrap();
    patch.push(b'X');
    std::fs::write(Path::new(&ckpt_dir).join("index.patch"), patch).unwrap();
    let r = dg(&["checkpoint", "verify", "--dir", &ckpt_dir, "--json"], &envs);
    assert_eq!(r.status, 1, "변조 체크포인트 verify → exit 1");

    // worktree remove — 읽기전용 ckpt root → 게이트 거부 exit 4 + 워크트리 보존
    let ckpt_ro = base.join("ckpt-ro");
    std::fs::create_dir_all(&ckpt_ro).unwrap();
    let mut perm = std::fs::metadata(&ckpt_ro).unwrap().permissions();
    use std::os::unix::fs::PermissionsExt;
    perm.set_mode(0o500);
    std::fs::set_permissions(&ckpt_ro, perm).unwrap();
    let denied = dg(
        &[
            "worktree", "remove", "--worktree", &wt.display().to_string(),
            "--repo", &repo_s, "--run-id", "rm-fail",
            "--checkpoint-root", &ckpt_ro.display().to_string(),
        ],
        &envs,
    );
    assert_eq!(denied.status, 4, "체크포인트 게이트 거부 → exit 4");
    assert!(wt.join("base.txt").exists(), "워크트리 보존(거부 시)");
    let mut perm2 = std::fs::metadata(&ckpt_ro).unwrap().permissions();
    perm2.set_mode(0o755);
    std::fs::set_permissions(&ckpt_ro, perm2).unwrap();

    // 정상 경로 → exit 0 + 삭제 + 체크포인트 보존
    let removed = dg(
        &[
            "worktree", "remove", "--worktree", &wt.display().to_string(),
            "--repo", &repo_s, "--run-id", "rm-ok",
            "--checkpoint-root", &ckpt_ro.display().to_string(), "--force", "--json",
        ],
        &envs,
    );
    assert_eq!(removed.status, 0);
    let parsed: Value = serde_json::from_str(removed.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(parsed["removed"], true);
    assert!(!wt.exists(), "워크트리 삭제됨");
    let ck = parsed["checkpoint_dir"].as_str().unwrap();
    assert!(Path::new(ck).exists(), "삭제 후 체크포인트 보존");
    let list = git_ok(&repo, &["worktree", "list", "--porcelain"]);
    assert!(!list.contains(&wt.display().to_string()), "git worktree list에서 제거");

    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_04_hook_guard_collect() {
    let envs_ro = [("HERDR_READONLY", "1")];

    // fail-closed: 잘못된 JSON → deny
    let r = dg_with_env(&["hook", "--guard"], &envs_ro, Some("not-json"), None);
    assert_eq!(r.status, 0);
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "deny");
    assert!(v["reason"].as_str().unwrap().contains("fail-closed"));

    // write_file → deny
    let r = dg_with_env(
        &["hook", "--guard"],
        &envs_ro,
        Some(r#"{"toolCall":{"id":"1","name":"write_file","args":{"path":"/tmp/x"}},"conversationId":"c1","cwd":"/tmp"}"#),
        None,
    );
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "deny");

    // read_file → allow
    let r = dg_with_env(
        &["hook", "--guard"],
        &envs_ro,
        Some(r#"{"toolCall":{"id":"1","name":"read_file","args":{"path":"/tmp/x"}},"conversationId":"c1","cwd":"/tmp"}"#),
        None,
    );
    let v: Value = serde_json::from_str(r.stdout.trim()).unwrap();
    assert_eq!(v["decision"], "allow");

    // collect — fail-open {}
    let r = dg_with_env(&["hook", "--collect"], &[], Some("garbage"), None);
    assert_eq!(r.status, 0);
    assert_eq!(r.stdout.trim(), "{}");
}

#[test]
fn test_05_setup_check_json() {
    let base = temp_base("setup");
    let r = dg(&["setup", "--check", "--json"], &[]);
    assert_eq!(r.status, 0, "setup --check --json exit 0 (stderr={})", r.stderr);
    let v: Value = serde_json::from_str(&r.stdout).expect("유효한 JSON 출력");
    assert!(v["ok"].is_boolean(), "ok 불린 필드");
    assert!(v["node"].is_object());
    assert!(v["clis"].is_object());
    assert!(v["secrets"].is_object());
    // 비밀 값 누출 없음 — 존재 여부 불린만 존재
    let secrets = v["secrets"].to_string();
    assert!(!secrets.contains("AIza"), "API 키 값 미포함");
    assert!(v["secrets"]["envSet"].is_boolean());
    std::fs::remove_dir_all(&base).unwrap();
}

#[test]
fn test_06_usage_errors_and_safe_fallback() {
    // 인증 오류 — exit 2
    let cases: Vec<(&str, Vec<&str>, i32)> = vec![
        ("invalid route enum", vec!["gate", "--route", "bogus", "--tool-class", "x"], 2),
        ("invalid risk", vec!["route", "--task-kind", "explore_read", "--risk", "extreme"], 2),
        ("unknown command", vec!["nope"], 2),
        ("missing value", vec!["gate", "--route"], 2),
        ("duplicate flag", vec!["gate", "--route", "file_select", "--route", "tool_select", "--tool-class", "x"], 2),
        ("positional arg", vec!["stats", "oops"], 2),
        ("invalid source", vec!["record", "--route", "tool_select", "--tool-class", "c", "--tool", "t", "--ok", "1", "--source", "bogus"], 2),
    ];
    for (label, args, expected) in cases {
        let r = dg(&args, &[]);
        assert_eq!(r.status, expected, "{label} → exit {expected} (got {})", r.status);
        if label != "unknown command" {
            // UsageError 계열만 [dg] 접두 — unknown command는 help 본문 출력(TS 패리티)
            assert!(r.stderr.contains("[dg]"), "{label} stderr에 [dg] 오류 라인: {}", r.stderr);
        }
    }

    // DB 폴백: 존재 불가 경로 → 경고 + 안전 JSON + exit 0
    let bad_db = "/nonexistent-dir-xyz/dg-e2e/none.db";
    let r = dg(&["route", "--task-kind", "explore_read", "--db", bad_db, "--json"], &[]);
    assert_eq!(r.status, 0, "DB 폴백 경로는 exit 0");
    assert!(r.stderr.contains("db fallback"), "stderr에 폴백 경고: {}", r.stderr);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["fallback"], true);
    assert_eq!(v["decision"], "split");
    assert!(v["fork_id"].as_str().unwrap().starts_with("fk_fallback_"));

    let r = dg(&["gate", "--route", "file_select", "--tool-class", "file_read", "--db", bad_db], &[]);
    assert_eq!(r.status, 0);

    let r = dg(&["record", "--route", "file_select", "--tool-class", "file_read", "--tool", "t", "--ok", "1", "--db", bad_db, "--json"], &[]);
    assert_eq!(r.status, 0);
    let v: Value = serde_json::from_str(r.stdout.trim().lines().last().unwrap()).unwrap();
    assert_eq!(v["recorded"], false);
}

#[test]
fn test_07_theta_and_exit_code_matrix() {
    let base = temp_base("theta");
    let repo = init_repo(&base);
    let repo_s = repo.display().to_string();
    let db = base.join("theta.db");
    let dbs = db.display().to_string();
    let envs = [("DGATE_DB_PATH", dbs.as_str())];

    // 기본 θ=0.70
    let r = dg_json(&["route", "--task-kind", "explore_read", "--project", &repo_s], &envs);
    assert_eq!(r["sharp_threshold"], 0.7);

    // checkpoint 서브커맨드 오류 — exit 2
    let r = dg(&["checkpoint", "bogus"], &[]);
    assert_eq!(r.status, 2);
    let r = dg(&["worktree"], &[]);
    assert_eq!(r.status, 2);

    // help exit 0
    let r = dg(&["--help"], &[]);
    assert_eq!(r.status, 0);
    assert!(r.stdout.contains("dg — decision gate CLI"));
    // 무인자 exit 2 + help
    let out = Command::new(DG).output().unwrap();
    assert_eq!(out.status.code(), Some(2));

    std::fs::remove_dir_all(&base).unwrap();
}
