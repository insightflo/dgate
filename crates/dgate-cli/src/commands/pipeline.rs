//! `dg pipeline init|step|status|run` — 결정적 sublead 파이프라인 상태머신(0토큰).
//!
//! `dgate_core::pipeline`의 순수 상태머신을 파일 영속 상태(`.herdr/pipelines/<run_id>.json`)
//! 위에서 구동한다 — 상주 sublead LLM 없이 `coder → verifier → rework/advisor-g3 → READY_PR`
//! 전이를 계산하고, 재시도 소진·human 판정 등 에스컬레이션만 sublead LLM 단발 기동으로 넘긴다.
//!
//! 종료 코드:
//! - 0: 정상 전이(status 포함) / 1: 상태 파일 입출력·실행기 오류
//! - 2: 사용법 오류(필수 플래그 누락·알 수 없는 이벤트/서브커맨드·상태 파일 부재)
//! - 3: 파이프라인이 `Escalated`에 도달(fail-closed — sublead 단발 기동 필요 신호)
use std::path::{Path, PathBuf};
use std::process::Command as ProcCommand;

use serde_json::{json, Value};

use crate::args::{bool_flag, optional_flag, parse_int_flag, require_flag, ParsedArgs, UsageError};
use crate::util::eprint_line;
use crate::util::prelude::*;
use dgate_core::pipeline::{
    self, check_scope_glob, PipelineAction, PipelineEvent, PipelineInit, PipelinePhase,
    PipelineState,
};
use dgate_core::roster;
use dgate_core::verdict::{self, ValidateVerdictOptions};

pub fn cmd_pipeline(sub: Option<&str>, args: &ParsedArgs, json: bool) -> CmdResult {
    match sub {
        Some("init") => cmd_init(args, json),
        Some("step") => cmd_step(args, json),
        Some("status") => cmd_status(args, json),
        Some("run") => cmd_run(args, json),
        other => Err(DgError::Usage(UsageError(format!(
            "unknown pipeline subcommand: {} (allowed: init|step|status|run)",
            other.unwrap_or("(missing)")
        )))),
    }
}

/// `--state-dir` 또는 기본 `.herdr/pipelines`(cwd 상대).
fn state_dir(args: &ParsedArgs) -> PathBuf {
    optional_flag(args, "state-dir")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(pipeline::DEFAULT_STATE_DIR))
}

fn cmd_init(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let brief = require_flag(args, "brief")?;
    let branch = require_flag(args, "branch")?;
    let worktree = optional_flag(args, "worktree");
    let scope = optional_flag(args, "scope");
    let max_reworks = parse_int_flag(args, "max-reworks", pipeline::DEFAULT_MAX_REWORKS as i64)?;
    if max_reworks < 0 {
        return Err(DgError::Usage(UsageError(
            "--max-reworks must be >= 0".to_string(),
        )));
    }
    // 모델 계약 — 플래그 > 환경변수 > 빈 문자열(미지정은 이종 검사에서 fail-closed).
    let coder_model = optional_flag(args, "coder-model")
        .or_else(|| std::env::var("HERDR_CODER_MODEL").ok())
        .unwrap_or_default();
    let verifier_model = optional_flag(args, "verifier-model")
        .or_else(|| std::env::var("HERDR_VERIFIER_MODEL").ok())
        .unwrap_or_default();
    let explore_model = optional_flag(args, "explore-model")
        .or_else(|| std::env::var("HERDR_EXPLORE_MODEL").ok())
        .filter(|s| !s.trim().is_empty());

    // 사전 경량 탐색(Pre-Coder Explore) 결정:
    // `--skip-explore` → 끔, `--with-explore` → 켬, 미지정 시 브리프에 코드 앵커(파일:줄번호)가
    // 없을 때만 자동 활성화해 중량 코더가 전체 레포 탐색에 시간을 태우는 사고를 예방한다.
    let skip_explore = bool_flag(args, "skip-explore");
    let force_explore = bool_flag(args, "with-explore");
    let auto_explore = if skip_explore {
        false
    } else if force_explore {
        true
    } else {
        std::fs::read_to_string(&brief)
            .map(|text| !pipeline::brief_has_code_anchors(&text))
            .unwrap_or(false)
    };
    if auto_explore && !force_explore {
        crate::util::eprint_line(
            "[dg pipeline] 브리프에 파일·줄번호 앵커 없음 — 경량 explore 사전 탐색 자동 활성화(--skip-explore로 생략 가능)",
        );
    }

    let (state, action) = pipeline::init_pipeline(&PipelineInit {
        fork_id: fork_id.clone(),
        run_id: None,
        brief_path: brief.clone(),
        worktree_path: worktree.clone(),
        branch: branch.clone(),
        scope_glob: scope.clone(),
        max_reworks: max_reworks as u32,
        coder_model,
        verifier_model,
        with_explore: auto_explore,
        explore_model,
    });

    let dir = state_dir(args);
    let state_file = match pipeline::save_state(&dir, &state) {
        Ok(p) => p,
        Err(e) => {
            emit(
                json,
                format!("[pipeline init] {fork_id} state 저장 실패: {e}"),
                &json!({
                    "action": "init",
                    "fork_id": fork_id,
                    "saved": false,
                    "errors": [e],
                }),
            );
            return Ok(1);
        }
    };

    // 스코프 가드 — 브리프/워크트리 등 선언된 대상 경로가 도메인 밖이면 경고(비치명적).
    let mut targets = vec![brief.as_str()];
    if let Some(w) = &worktree {
        targets.push(w.as_str());
    }
    let scope_warnings = scope
        .as_deref()
        .map(|s| check_scope_glob(s, &targets))
        .unwrap_or_default();
    for w in &scope_warnings {
        crate::util::eprint_line(&format!("[dg pipeline] scope warning: {w}"));
    }

    emit(
        json,
        format!(
            "[pipeline init] {fork_id} → {} next={}",
            state.phase.as_str(),
            action.label()
        ),
        &json!({
            "action": "init",
            "fork_id": fork_id,
            "run_id": state.run_id,
            "phase": state.phase.as_str(),
            "state_file": state_file,
            "next_action": action,
            "scope_warnings": scope_warnings,
        }),
    );
    Ok(0)
}

fn cmd_step(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let event_name = require_flag(args, "event")?;
    let dir = state_dir(args);
    let state = pipeline::load_state(&dir, &fork_id)
        .map_err(|e| DgError::Usage(UsageError(e)))?;

    let head = optional_flag(args, "head");
    let reason = optional_flag(args, "reason");
    let mut verdict_errors: Vec<String> = Vec::new();
    let event = match event_name.as_str() {
        "explore-ok" => PipelineEvent::ExploreExited {
            exit_code: 0,
            context_map_path: optional_flag(args, "context-map")
                .or_else(|| Some(pipeline::default_context_map_path(&state.fork_id))),
            stalled: false,
        },
        "explore-fail" => PipelineEvent::ExploreExited {
            exit_code: 1,
            context_map_path: None,
            stalled: bool_flag(args, "stalled"),
        },
        "coder-ok" => PipelineEvent::CoderExited {
            exit_code: 0,
            head_sha: head.clone(),
            stalled: false,
        },
        "coder-fail" => PipelineEvent::CoderExited {
            exit_code: 1,
            head_sha: head.clone(),
            stalled: bool_flag(args, "stalled"),
        },
        "verdict" => {
            let vf = require_flag(args, "verdict-file")?;
            // fail-closed — 무효 verdict는 effective_decision=Human → 즉시 에스컬레이션.
            // HEAD 결속은 --head > 상태 파일의 head_sha 순으로 적용한다.
            let v = verdict::validate_verdict_file(
                Path::new(&vf),
                &ValidateVerdictOptions {
                    expected_fork_id: state.fork_id.clone(),
                    expected_head: head.clone().or_else(|| state.head_sha.clone()),
                },
            );
            verdict_errors = v.errors.clone();
            PipelineEvent::VerdictReceived {
                decision: v.effective_decision,
                verdict_path: vf,
            }
        }
        "g3-pass" => {
            let pr = match optional_flag(args, "pr") {
                Some(raw) => Some(raw.parse::<u64>().map_err(|_| {
                    DgError::Usage(UsageError(format!("--pr must be an integer: {raw}")))
                })?),
                None => None,
            };
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number: pr,
                reason: reason.clone(),
            }
        }
        "g3-fail" => PipelineEvent::AdvisorG3Result {
            passed: false,
            pr_number: None,
            reason: reason.clone(),
        },
        other => {
            return Err(DgError::Usage(UsageError(format!(
                "invalid --event \"{other}\" (allowed: explore-ok|explore-fail|coder-ok|coder-fail|verdict|g3-pass|g3-fail)"
            ))))
        }
    };

    let (next_state, action) = pipeline::advance_pipeline(state, event);
    let state_file = match pipeline::save_state(&dir, &next_state) {
        Ok(p) => p,
        Err(e) => {
            emit(
                json,
                format!("[pipeline step] {fork_id} state 저장 실패: {e}"),
                &json!({
                    "action": "step",
                    "fork_id": fork_id,
                    "saved": false,
                    "errors": [e],
                }),
            );
            return Ok(1);
        }
    };

    // READY_PR 신호 — EmitReadyPr 액션일 때만 `READY_PR=<num> HEAD=<sha> G3=PASS`.
    let ready_pr_signal = match &action {
        PipelineAction::EmitReadyPr {
            pr_number,
            head_sha,
        } => Some(format!(
            "READY_PR={} HEAD={} G3=PASS",
            pr_number,
            head_sha.as_deref().unwrap_or("unknown")
        )),
        _ => None,
    };

    let mut human = format!(
        "[pipeline step] {fork_id} {event_name} → {} next={}",
        next_state.phase.as_str(),
        action.label()
    );
    if let PipelineAction::WakeSubleadEscalation { reason } = &action {
        human.push_str(&format!(" reason={reason}"));
    }
    if let Some(sig) = &ready_pr_signal {
        human.push_str(&format!("\n{sig}"));
    }

    emit(
        json,
        human,
        &json!({
            "action": "step",
            "fork_id": fork_id,
            "event": event_name,
            "phase": next_state.phase.as_str(),
            "attempt": next_state.attempt,
            "max_reworks": next_state.max_reworks,
            "state_file": state_file,
            "next_action": action,
            "ready_pr_signal": ready_pr_signal,
            "verdict_errors": verdict_errors,
            "escalation_reason": next_state.escalation_reason,
        }),
    );
    // Escalated 도달은 sublead 단발 기동 신호 — verdict fail-closed(exit 3)와 같은 계약.
    Ok(if next_state.phase == PipelinePhase::Escalated {
        3
    } else {
        0
    })
}

fn cmd_status(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let dir = state_dir(args);
    let state = pipeline::load_state(&dir, &fork_id)
        .map_err(|e| DgError::Usage(UsageError(e)))?;

    emit(
        json,
        format!(
            "[pipeline] {fork_id} phase={} attempt={}/{} transitions={}",
            state.phase.as_str(),
            state.attempt,
            state.max_reworks,
            state.history.len()
        ),
        &json!({
            "action": "status",
            "state_file": pipeline::state_file_path(&dir, &state.run_id),
            "state": state,
        }),
    );
    Ok(0)
}

// ============================================================================
// `dg pipeline run` — 0토큰 결정적 파이프라인 자동 실행 러너
// ============================================================================
//
// `dg pipeline init`이 만든 상태 파일을 로드해, 현재 위상에 대응하는 결정적
// 액션을 `PipelineExecutor`에 위임해 `PipelineEvent`를 얻고 `advance_pipeline`으로
// 전이·영속화하는 루프를 돌린다. 전이 판정은 전부 `dgate_core::pipeline`에 있고
// 여기서는 스케줄링(워커 스폰·대기·결과 수집)만 한다 — 상태 전이 중복 구현 없음.
//
// 종료 코드: 0 = ReadyPr 완주 · 1 = 실행기/상태 파일 오류 · 2 = 사용법 오류 ·
// 3 = Escalated/Failed 도달(sublead 단발 기동 필요).

/// 러너 스텝 상한 기본값 — 무한 루프 방지.
pub const RUN_DEFAULT_MAX_STEPS: i64 = 20;
/// 워커 `dg wrap` 스톨·최대 시간(관측된 코더 스폰 계약과 동일 값).
const WRAP_STALL_MS: &str = "600000";
const WRAP_MAX_MS: &str = "1500000";
/// 경량 사전 탐색(explore) 워커는 빠르게 Context Pack만 뽑고 종료한다(최대 5분).
const EXPLORE_STALL_MS: &str = "180000";
const EXPLORE_MAX_MS: &str = "300000";
const EXPLORE_READ_LINES: &str = "240";
/// advisor 세션 화면 스캔 범위 — `[advisor:…]` 마커 검출용 `hc read` 꼬리 길이.
const ADVISOR_READ_LINES: &str = "200";

/// 파이프라인 실행 계약 — 결정적 액션 1건을 받아 상태머신 이벤트를 생산한다.
/// 출력/알림성 액션(`EmitReadyPr`·`WakeSubleadEscalation`·`None`)은 이벤트가
/// 없으므로 `Ok(None)`을 반환한다. 테스트는 Mock으로 전체 루프를 결정적 검증한다.
pub trait PipelineExecutor {
    fn execute(
        &mut self,
        action: &PipelineAction,
        state: &PipelineState,
    ) -> Result<Option<PipelineEvent>, String>;
}

/// 실제 실행기 설정 — hc/dg 실행 파일과 프로젝트 루트(verdict·gh 기준 디렉터리).
#[derive(Debug, Clone)]
pub struct ExecConfig {
    pub hc_bin: String,
    pub dg_bin: String,
    pub repo_dir: PathBuf,
}

impl ExecConfig {
    fn from_args(args: &ParsedArgs) -> Self {
        ExecConfig {
            // send.rs/reap.rs와 동일한 오버라이드 계약(HC_BIN).
            hc_bin: std::env::var("HC_BIN").unwrap_or_else(|_| "hc".to_string()),
            // 자기 자신(dg)을 백프랍·wrap 자식 명령으로 쓴다(dispatch.rs와 동일 패턴).
            dg_bin: std::env::current_exe()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|_| "dg".to_string()),
            repo_dir: optional_flag(args, "repo")
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from(".")),
        }
    }
}

/// 워커 작업 디렉터리 — state.worktree_path 우선, 없으면 repo_dir.
fn worker_cwd(cfg: &ExecConfig, state: &PipelineState) -> PathBuf {
    state
        .worktree_path
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.repo_dir.clone())
}

/// hc `--name` 계약(`^[a-z0-9_-]{1,32}$`)으로 세션명을 만든다 —
/// 역할 접두 + fork + 재작업 번호(재스폰 시 신규 세션, 크래시 재개 시 동일 세션 재사용).
fn session_name(role: &str, fork_id: &str, attempt: Option<u32>) -> String {
    let mut n = format!("{role}-{fork_id}");
    if let Some(a) = attempt {
        n = format!("{n}-a{a}");
    }
    n.to_lowercase()
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
                c
            } else {
                '-'
            }
        })
        .take(32)
        .collect()
}

/// `sh -c` 페이로드 안에 넣을 argv 단일 인용 부호 이스케이프.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// 현재 위상 → 실행할 결정적 액션 재구성.
/// `rework_pending`은 마지막 전이의 액션 라벨로 `dg backprop` 경유 여부를 복원한다
/// (전이 시 이미 계산된 액션을 재실행하는 것이지 새 판정이 아님 — 0토큰).
fn action_for_phase(state: &PipelineState) -> Result<PipelineAction, String> {
    match state.phase {
        PipelinePhase::ExploreRunning => Ok(PipelineAction::SpawnExplore {
            fork_id: state.fork_id.clone(),
            context_map_path: state
                .context_map_path
                .clone()
                .unwrap_or_else(|| pipeline::default_context_map_path(&state.fork_id)),
            explore_model: state.explore_model.clone(),
        }),
        PipelinePhase::Init | PipelinePhase::CoderRunning | PipelinePhase::ReworkPending => {
            let last = state
                .history
                .last()
                .map(|t| t.action.as_str())
                .unwrap_or_default();
            if state.phase == PipelinePhase::ReworkPending && last == "backprop_and_respawn_coder" {
                let verdict_path = state.last_verdict_path.clone().ok_or_else(|| {
                    "rework_pending(backprop)인데 last_verdict_path 없음".to_string()
                })?;
                Ok(PipelineAction::BackpropAndRespawnCoder {
                    fork_id: state.fork_id.clone(),
                    verdict_path,
                    attempt: state.attempt,
                })
            } else {
                Ok(PipelineAction::SpawnCoder {
                    attempt: state.attempt,
                    feedback: None,
                })
            }
        }
        PipelinePhase::VerifierRunning => Ok(PipelineAction::SpawnVerifier {
            fork_id: state.fork_id.clone(),
            head_sha: state.head_sha.clone(),
            verifier_model: state.verifier_model.clone(),
        }),
        PipelinePhase::AdvisorG3Running => Ok(PipelineAction::SpawnAdvisorG3 {
            fork_id: state.fork_id.clone(),
            head_sha: state.head_sha.clone(),
            verdict_path: state.last_verdict_path.clone().unwrap_or_default(),
        }),
        terminal => Err(format!(
            "terminal phase {} — 실행할 액션 없음",
            terminal.as_str()
        )),
    }
}

// ---------- 워커 프롬프트·argv (dry-run 계획과 실제 실행이 같은 빌더를 공유) ----------

fn explore_prompt(state: &PipelineState) -> String {
    format!(
        "읽기전용 사전 탐색(Context Pack 생성) — 브리프 {brief}의 요구사항을 구현하기 전에 \
         중량 코더가 불필요한 전체 레포 탐색을 하지 않도록 아래 4가지만 간결한 Markdown으로 출력하라 \
         (파일 수정·커밋 절대 금지): \
         1) ## Target Anchors: 수정할 파일 경로와 정확한 줄번호 범위(`path:start-end`) 및 핵심 함수·컴포넌트명, \
         2) ## Related Tests: 기존 테스트 파일 경로와 실행 명령, \
         3) ## Constraints & Edge Cases: 놓치기 쉬운 타입·상태·호출부 제약, \
         4) ## Minimal Plan: 3~5줄 구현 순서 요약.",
        brief = state.brief_path,
    )
}

/// 경량 읽기전용 사전 탐색 워커 내부 argv — 로스터 `explore` 슬롯(기본 pi read-only)을 쓰되
/// `--tools read,grep,find,ls --exclude-tools edit,write`로 쓰기를 물리 차단한다.
fn explore_inner_argv(cfg: &ExecConfig, state: &PipelineState) -> Vec<String> {
    let prompt = explore_prompt(state);
    let loaded = roster::load_roster_config_quiet(None, Some(&cfg.repo_dir));
    let (_name, profile, _fb) =
        roster::resolve_roster_profile(&loaded.config, &loaded.config.default_profile);
    let model = state.explore_model.clone().or_else(|| {
        profile
            .roles
            .get(&roster::RoleSlot::Explore)
            .and_then(|s| s.model.clone())
    });
    let mut a = vec![
        roster::resolve_worker_bin(roster::WorkerCliKind::Pi, None),
        "--provider".to_string(),
        "opencodex".to_string(),
    ];
    if let Some(m) = model.filter(|m| !m.trim().is_empty()) {
        a.push("--model".to_string());
        a.push(m);
    }
    a.extend(
        [
            "--tools",
            "read,grep,find,ls",
            "--exclude-tools",
            "edit,write",
            "--no-session",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "-p",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    a.push(prompt);
    a
}

fn coder_prompt(
    state: &PipelineState,
    attempt: u32,
    feedback: Option<&str>,
    verdict_path: Option<&str>,
) -> String {
    let mut p = format!(
        "브리프 파일 {} 를 읽고 Standards/Spec/Merge Danger을 모두 준수해 구현한 뒤 \
         이 워크트리 브랜치에 로컬 git commit까지 완료하라(git push·gh pr create·merge는 금지). \
         완료 시 변경 요약을 최종 메시지로 출력하라",
        state.brief_path
    );
    if let Some(cm) = state.context_map_path.as_deref().filter(|s| !s.trim().is_empty()) {
        p.push_str(&format!(
            " — 사전 탐색 Context Pack({cm})을 먼저 읽고 기록된 파일·줄번호 앵커부터 바로 구현에 착수하라(불필요한 전체 레포 재탐색 금지)"
        ));
    }
    if let Some(vp) = verdict_path {
        p.push_str(&format!(" — 이전 검증 verdict {vp} 의 finding을 모두 해소하라"));
    }
    if let Some(fb) = feedback.filter(|f| !f.trim().is_empty()) {
        p.push_str(&format!(" — 재작업 사유: {fb}"));
    }
    if attempt > 0 {
        p.push_str(&format!(" (rework #{attempt})"));
    }
    p
}

/// 코더 내부 argv — spec 계약: `devin --permission-mode dangerous
/// --respect-workspace-trust false -p <prompt>`.
fn coder_inner_argv(
    state: &PipelineState,
    attempt: u32,
    feedback: Option<&str>,
    verdict_path: Option<&str>,
) -> Vec<String> {
    let mut a = vec![
        roster::resolve_worker_bin(roster::WorkerCliKind::Devin, None),
        "--permission-mode".to_string(),
        "dangerous".to_string(),
        "--respect-workspace-trust".to_string(),
        "false".to_string(),
    ];
    if !state.coder_model.is_empty() {
        a.push("--model".to_string());
        a.push(state.coder_model.clone());
    }
    a.push("-p".to_string());
    a.push(coder_prompt(state, attempt, feedback, verdict_path));
    a
}

fn verifier_prompt(state: &PipelineState, verdict_path: &Path) -> String {
    let head = state.head_sha.as_deref().unwrap_or("현재 HEAD");
    format!(
        "fork {fk} 구현(HEAD {head})을 브리프 {brief}의 Standards/Spec 두 축과 Merge Danger으로 \
         검증하라. verdict JSON(schema_version 1, fork_id \"{fk}\", target_head \"{head}\", \
         decision pass|rework|fail|human, reason, findings, merge_danger)을 {vp} 에 기록하라 \
         (디렉터리가 없으면 생성). 엄격 스키마: findings[].severity는 반드시 P0|P1|P2|info 4종만 \
         사용(low/medium/high 금지), merge_danger는 {{\"door\":\"one-way|two-way\",\"blast_radius\":\"<=500자\"}} 형식 준수",
        fk = state.fork_id,
        head = head,
        brief = state.brief_path,
        vp = verdict_path.display(),
    )
}

/// 검증자 내부 argv — spec 계약: `pi --provider opencodex --model <verifier_model>
/// --no-session --no-extensions --no-mcp --no-skills --mode json -p <prompt>`
/// (`pi`는 `--no-approve` 미지원, `--mode json` 스트리밍으로 무출력 stall 오탐 방지).
fn verifier_inner_argv(state: &PipelineState, verdict_path: &Path) -> Vec<String> {
    let mut a = vec![
        roster::resolve_worker_bin(roster::WorkerCliKind::Pi, None),
        "--provider".to_string(),
        "opencodex".to_string(),
    ];
    if !state.verifier_model.is_empty() {
        a.push("--model".to_string());
        a.push(state.verifier_model.clone());
    }
    a.extend(
        [
            "--no-session",
            "--no-extensions",
            "--no-mcp",
            "--no-skills",
            "--mode",
            "json",
            "-p",
        ]
        .iter()
        .map(|s| s.to_string()),
    );
    a.push(verifier_prompt(state, verdict_path));
    a
}

fn advisor_prompt(state: &PipelineState) -> String {
    let head = state.head_sha.as_deref().unwrap_or("현재 HEAD");
    let vp = state.last_verdict_path.as_deref().unwrap_or("(없음)");
    format!(
        "G3 자문 — fork {fk} 구현(HEAD {head}, verdict {vp})을 브리프 {brief}의 Merge Danger 기준 \
         최종 머지 관문으로 검토하라. 판정은 마커 하나만 출력하라: [advisor:pass] 또는 \
         [advisor:challenge:<이유>]",
        fk = state.fork_id,
        head = head,
        vp = vp,
        brief = state.brief_path,
    )
}

/// G3 자문 내부 argv — 로스터 advisor 슬롯(SKILL.md "로스터 advisor 모델")을 쓰고,
/// 슬롯이 없으면 spec 패턴과 같은 pi/opencodex 폴백으로 내린다.
fn advisor_inner_argv(cfg: &ExecConfig, state: &PipelineState) -> Vec<String> {
    let prompt = advisor_prompt(state);
    let loaded = roster::load_roster_config_quiet(None, Some(&cfg.repo_dir));
    let (_name, profile, _fb) =
        roster::resolve_roster_profile(&loaded.config, &loaded.config.default_profile);
    if let Some(spec) = profile.roles.get(&roster::RoleSlot::Advisor) {
        roster::build_worker_command(
            spec,
            &roster::BuildWorkerCommandOptions {
                prompt: &prompt,
                role_brief_path: None,
                interactive: false,
            },
        )
    } else {
        vec![
            roster::resolve_worker_bin(roster::WorkerCliKind::Pi, None),
            "--provider".to_string(),
            "opencodex".to_string(),
            "--no-session".to_string(),
            "--no-extensions".to_string(),
            "--no-mcp".to_string(),
            "-p".to_string(),
            prompt,
        ]
    }
}

/// `cmd /C` 라인에서 셸 메타문자를 보호하는 이스케이프 — 공백·인용을 포함하는
/// 인자는 Rust가 `"…"`로 감싸며 그 인용이 cmd를 그대로 통과해 자식 argv를 묶어
/// 주므로 그대로 두고, 공백·인용이 없는 인자의 `&|<>`·`^`만 `^`로 이스케이프한다
/// (Windows 실측: bare `a&b`는 명령 분리되고 `a^&b`는 리터럴로 도달). `%`는
/// cmd에서 신뢰성 있게 이스케이프할 수 없어 건드리지 않는다(희귀 잔여 엣지).
fn cmd_escape_token(s: &str) -> String {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '"') {
        return s.to_string();
    }
    s.chars()
        .flat_map(|c| {
            if "&|<>^".contains(c) {
                vec!['^', c]
            } else {
                vec![c]
            }
        })
        .collect()
}

/// `dg wrap --` 뒤에 넘길 OS별 워커 스폰 argv (순수 — macOS에서도 양 분기 단위 검증).
/// Unix: 기존 계약 그대로 `sh -c 'exec env HERDR_ROLE=<role> HERDR_DELEGATE_MODE=hc
/// <inner…> </dev/null'`. Windows: `sh`/`env`/`/dev/null`이 없으므로
/// `cmd /C set HERDR_ROLE=<role>&& set HERDR_DELEGATE_MODE=hc&& <inner> < NUL` —
/// `dg wrap`에 `--env`가 없어 `set`으로 env를 심고 `< NUL`로 stdin을 닫아
/// 워커의 stdin 대기(SIGTTIN 유사 정지)를 차단한다.
/// 각 토큰을 별도 argv 요소로 넘기는 이유: 단일 스크립트 문자열에 손수 넣은 `"`는
/// Rust가 `\"`로 재인용해 cmd의 프로그램 해석·공백 인자 묶기가 깨진다(Windows 실측).
/// `set VAR=v&&`는 `&&` 앞 공백이 값에 섞이는 것을 막기 위해 반드시 한 토큰이다.
fn wrap_inner_command(is_windows: bool, role: &str, inner: &[String]) -> Vec<String> {
    if is_windows {
        let mut argv = vec![
            "cmd".to_string(),
            "/C".to_string(),
            "set".to_string(),
            format!("HERDR_ROLE={role}&&"),
            "set".to_string(),
            "HERDR_DELEGATE_MODE=hc&&".to_string(),
        ];
        if role == "verifier" {
            argv.push("set".to_string());
            argv.push(r"PI_CODING_AGENT_DIR=%USERPROFILE%\.pi\agent-verify&&".to_string());
        }
        argv.extend(inner.iter().map(|a| cmd_escape_token(a)));
        argv.push("<".to_string());
        argv.push("NUL".to_string());
        argv
    } else {
        let verifier_env = if role == "verifier" {
            r#" PI_CODING_AGENT_DIR="${PI_CODING_AGENT_DIR:-$HOME/.pi/agent-verify}""#
        } else {
            ""
        };
        vec![
            "sh".to_string(),
            "-c".to_string(),
            format!(
                "exec env HERDR_ROLE={role} HERDR_DELEGATE_MODE=hc{verifier_env} {} </dev/null",
                inner
                    .iter()
                    .map(|a| sh_quote(a))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        ]
    }
}

/// `dg wrap … -- <OS별 워커 스폰 argv>` — 관측된 코더 스폰 계약(`hc get t30f`)과 같은 형태.
/// `--role`/`--label`은 현재 wrap 파서에서 미사용 필드지만 spec이 요구하는 명시 계약이다.
fn wrap_spawn_argv(
    cfg: &ExecConfig,
    sess: &str,
    role: &str,
    tool_class: &str,
    fork_id: Option<&str>,
    require_artifact: Option<&Path>,
    inner: &[String],
) -> Vec<String> {
    let mut argv = vec![
        cfg.dg_bin.clone(),
        "wrap".to_string(),
        "--route".to_string(),
        "file_select".to_string(),
        "--tool-class".to_string(),
        tool_class.to_string(),
        "--role".to_string(),
        role.to_string(),
        "--label".to_string(),
        sess.to_string(),
    ];
    if let Some(fk) = fork_id {
        argv.push("--fork".to_string());
        argv.push(fk.to_string());
    }
    argv.extend([
        "--project".to_string(),
        cfg.repo_dir.display().to_string(),
        "--stall-ms".to_string(),
        WRAP_STALL_MS.to_string(),
        "--max-ms".to_string(),
        WRAP_MAX_MS.to_string(),
    ]);
    if let Some(art) = require_artifact {
        argv.push("--require-artifact".to_string());
        argv.push(art.display().to_string());
    }
    argv.push("--".to_string());
    argv.extend(wrap_inner_command(cfg!(windows), role, inner));
    argv
}

/// `hc run --bg --name <sess> --cwd <dir> -- <worker argv…>` 전체 argv.
fn hc_run_argv(cfg: &ExecConfig, sess: &str, cwd: &Path, worker_argv: &[String]) -> Vec<String> {
    let mut a = vec![
        cfg.hc_bin.clone(),
        "run".to_string(),
        "--bg".to_string(),
        "--name".to_string(),
        sess.to_string(),
        "--cwd".to_string(),
        cwd.display().to_string(),
        "--".to_string(),
    ];
    a.extend(worker_argv.iter().cloned());
    a
}

fn coder_commands(
    cfg: &ExecConfig,
    state: &PipelineState,
    attempt: u32,
    feedback: Option<&str>,
    verdict_path: Option<&str>,
) -> Vec<Vec<String>> {
    let sess = session_name("coder", &state.fork_id, Some(attempt));
    let cwd = worker_cwd(cfg, state);
    let inner = coder_inner_argv(state, attempt, feedback, verdict_path);
    // attempt 0만 파이프라인 루트 fork에 결속하고, rework(attempt>0)는 이미 닫힌 루트 fork
    // 재사용 오류(`fork not open`)를 막기 위해 `dg wrap`이 새 스텝 fork를 자동 발급하게 둔다.
    let step_fork = (attempt == 0).then_some(state.fork_id.as_str());
    let spawn = wrap_spawn_argv(cfg, &sess, "coder", "file_edit", step_fork, None, &inner);
    vec![
        hc_run_argv(cfg, &sess, &cwd, &spawn),
        vec![cfg.hc_bin.clone(), "wait".to_string(), sess.clone(), "--json".to_string()],
        vec![cfg.hc_bin.clone(), "get".to_string(), sess.clone(), "--json".to_string()],
        vec!["git".to_string(), "-C".to_string(), cwd.display().to_string(), "rev-parse".to_string(), "HEAD".to_string()],
        vec![cfg.hc_bin.clone(), "rm".to_string(), sess],
    ]
}

/// dry-run·실행 공용 커맨드 빌더 — 액션을 외부 명령 argv 리스트로 펼친다(순수, 실행 없음).
/// 실제 실행기는 이 목록과 동일한 명령을 돌린다(dry-run 출력 = 실제 실행 명령).
fn plan_commands(
    action: &PipelineAction,
    state: &PipelineState,
    cfg: &ExecConfig,
) -> Vec<Vec<String>> {
    let cwd = worker_cwd(cfg, state);
    match action {
        PipelineAction::SpawnExplore { fork_id, .. } => {
            let sess = session_name("explore", fork_id, None);
            let inner = explore_inner_argv(cfg, state);
            let mut spawn = wrap_spawn_argv(cfg, &sess, "explore", "file_read", None, None, &inner);
            for i in 0..spawn.len().saturating_sub(1) {
                if spawn[i] == "--stall-ms" {
                    spawn[i + 1] = EXPLORE_STALL_MS.to_string();
                } else if spawn[i] == "--max-ms" {
                    spawn[i + 1] = EXPLORE_MAX_MS.to_string();
                }
            }
            vec![
                hc_run_argv(cfg, &sess, &cwd, &spawn),
                vec![cfg.hc_bin.clone(), "wait".to_string(), sess.clone(), "--json".to_string()],
                vec![
                    cfg.hc_bin.clone(),
                    "read".to_string(),
                    sess.clone(),
                    "--lines".to_string(),
                    EXPLORE_READ_LINES.to_string(),
                ],
                vec![cfg.hc_bin.clone(), "rm".to_string(), sess],
            ]
        }
        PipelineAction::SpawnCoder { attempt, feedback } => {
            coder_commands(cfg, state, *attempt, feedback.as_deref(), None)
        }
        PipelineAction::BackpropAndRespawnCoder {
            fork_id,
            verdict_path,
            attempt,
        } => {
            let mut bp = vec![
                cfg.dg_bin.clone(),
                "backprop".to_string(),
                "--fork".to_string(),
                fork_id.clone(),
                "--verdict-file".to_string(),
                verdict_path.clone(),
            ];
            if let Some(h) = &state.head_sha {
                bp.push("--expected-head".to_string());
                bp.push(h.clone());
            }
            let mut cmds = vec![bp];
            cmds.extend(coder_commands(
                cfg,
                state,
                *attempt,
                Some("verdict rework"),
                Some(verdict_path),
            ));
            cmds
        }
        PipelineAction::SpawnVerifier { fork_id, .. } => {
            let sess = session_name("verifier", fork_id, Some(state.attempt));
            let vp = verdict::resolve_verdict_path(&cfg.repo_dir, fork_id);
            let inner = verifier_inner_argv(state, &vp);
            // coder(a0)가 종료되며 루트 fork를 닫으므로 verifier는 --fork 없이 자체 스텝 fork를 연다.
            // verdict JSON 내부 fork_id는 여전히 state.fork_id에 결속된다.
            let spawn = wrap_spawn_argv(cfg, &sess, "verifier", "file_read", None, Some(&vp), &inner);
            vec![
                hc_run_argv(cfg, &sess, &cwd, &spawn),
                vec![cfg.hc_bin.clone(), "wait".to_string(), sess.clone(), "--json".to_string()],
                vec![cfg.hc_bin.clone(), "get".to_string(), sess.clone(), "--json".to_string()],
                vec![cfg.hc_bin.clone(), "rm".to_string(), sess],
            ]
        }
        PipelineAction::SpawnAdvisorG3 { fork_id, .. } => {
            let sess = session_name("advisor-g3", fork_id, None);
            let inner = advisor_inner_argv(cfg, state);
            let spawn = wrap_spawn_argv(cfg, &sess, "advisor", "file_read", None, None, &inner);
            vec![
                hc_run_argv(cfg, &sess, &cwd, &spawn),
                vec![cfg.hc_bin.clone(), "wait".to_string(), sess.clone(), "--json".to_string()],
                vec![
                    cfg.hc_bin.clone(),
                    "read".to_string(),
                    sess.clone(),
                    "--lines".to_string(),
                    ADVISOR_READ_LINES.to_string(),
                ],
                vec![cfg.hc_bin.clone(), "rm".to_string(), sess],
                // [advisor:pass] 마커 확인 후에만 실행 — git push + gh pr create(ci-full 라벨 미부착)
                vec![
                    "git".to_string(),
                    "-C".to_string(),
                    cwd.display().to_string(),
                    "push".to_string(),
                    "-u".to_string(),
                    "origin".to_string(),
                    state.branch.clone(),
                ],
                vec![
                    "gh".to_string(),
                    "pr".to_string(),
                    "create".to_string(),
                    "--head".to_string(),
                    state.branch.clone(),
                    "--fill".to_string(),
                ],
            ]
        }
        PipelineAction::WakeSubleadEscalation { reason } => vec![vec![
            cfg.hc_bin.clone(),
            "send".to_string(),
            "<parent>".to_string(),
            format!("[pipeline] {} ESCALATED: {reason}", state.fork_id),
        ]],
        PipelineAction::EmitReadyPr { .. } | PipelineAction::None => Vec::new(),
    }
}

// ---------- hc 헬퍼 ----------

/// `hc get <t> --json` — 터미널 없음·실패는 None(스폰 필요 신호로 사용).
fn hc_get(hc: &str, term: &str) -> Option<Value> {
    let out = ProcCommand::new(hc).args(["get", term, "--json"]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    serde_json::from_slice(&out.stdout).ok()
}

/// `hc wait <t> --timeout 300 --json` — 워커가 "일을 멈출 때"까지 블록한다(sleep·폴링 금지 계약).
const HC_WAIT_TIMEOUT_SECS: &str = "300";
/// `hc_run_to_exit` 대기 라운드 상한 — 6 × 300s = 1800s(30분) > `WRAP_MAX_MS`(1500s).
/// 기본 3×180s(9분)에서 조기 exit 1되어 `die_with_parent`로 정상 워커를 SIGHUP 종료하는 사고 방지.
const HC_WAIT_MAX_ROUNDS: usize = 6;

fn hc_wait(hc: &str, term: &str) -> Result<Value, String> {
    let out = ProcCommand::new(hc)
        .args(["wait", term, "--timeout", HC_WAIT_TIMEOUT_SECS, "--json"])
        .output()
        .map_err(|e| format!("hc 실행 불가: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hc wait {term} 실패: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    serde_json::from_slice(&out.stdout).map_err(|e| format!("hc wait JSON 파싱 실패: {e}"))
}

/// `hc run --bg` 스폰 — 세션명은 호출자의 결정적 이름을 쓴다(재개 식별용).
fn hc_spawn(hc: &str, name: &str, cwd: &Path, argv: &[String]) -> Result<(), String> {
    let cwd_s = cwd.display().to_string();
    let mut args: Vec<&str> = vec!["run", "--bg", "--name", name, "--cwd", &cwd_s, "--"];
    args.extend(argv.iter().map(String::as_str));
    let out = ProcCommand::new(hc)
        .args(&args)
        .output()
        .map_err(|e| format!("hc 실행 불가: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "hc run --bg {name} 실패: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(())
}

/// `hc read <t> --lines <n>` — 종료 세션의 화면 꼬리(wrap 요약·advisor 마커 검출용).
fn hc_read(hc: &str, term: &str, lines: &str) -> String {
    ProcCommand::new(hc)
        .args(["read", term, "--lines", lines])
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default()
}

/// 종료된 워커 터미널 정리(`hc rm`) — 최선노력, 실패 무시.
fn hc_rm(hc: &str, term: &str) {
    let _ = ProcCommand::new(hc).args(["rm", term]).output();
}

/// 세션 상태 문자열(live/finished/…) — 없으면 None.
fn hc_session_state(info: Option<&Value>) -> Option<&str> {
    info.and_then(|v| v.get("state")).and_then(Value::as_str)
}

/// 워커 스폰-or-재개-or-대기 후 종료 정보를 반환한다.
/// finished면 결과 재사용, live면 대기만(크래시 재개), 없거나 dead/gone이면
/// 잔여 세션을 정리하고 새로 스폰한다 — 종결적 세션명 결정성의 핵심.
fn hc_run_to_exit(hc: &str, sess: &str, cwd: &Path, argv: &[String]) -> Result<Value, String> {
    let info = hc_get(hc, sess);
    match hc_session_state(info.as_ref()) {
        Some("finished") => return Ok(info.expect("checked")),
        Some("live") => {} // 재개 — 대기만 한다
        _ => {
            if info.is_some() {
                hc_rm(hc, sess); // dead/gone 잔여 세션 — 이름 충돌 방지로 선제 정리
            }
            hc_spawn(hc, sess, cwd, argv)?;
        }
    }
    // 비종료 사유(message·idle·300s 창 만료 등)로 wait가 조기 반환될 수 있어
    // `WRAP_MAX_MS`(1500s)를 충분히 덮는 상한(6×300s=1800s)까지 대기한다.
    for _ in 0..HC_WAIT_MAX_ROUNDS {
        let _ = hc_wait(hc, sess)?;
        let after = hc_get(hc, sess);
        if hc_session_state(after.as_ref()) == Some("finished") {
            return Ok(after.expect("checked"));
        }
    }
    Err(format!("hc 세션 {sess}이(가) 종료되지 않음"))
}

/// 종료 세션 정보 → 종료 코드(exit_code 우선, signal이면 128+sig, 둘 다 없으면 -1).
fn session_exit_code(info: &Value) -> i32 {
    info.get("exit_code")
        .and_then(Value::as_i64)
        .or_else(|| info.get("signal").and_then(Value::as_i64).map(|s| 128 + s))
        .unwrap_or(-1) as i32
}

/// 종료 코더 세션 → CoderExited. stalled는 `dg wrap` 요약 JSON을 터미널 화면에서 best-effort로 읽는다.
fn coder_event_from_session(hc: &str, sess: &str, info: &Value) -> PipelineEvent {
    let screen = hc_read(hc, sess, "60");
    let stalled = screen.contains("\"stalled\":true") || screen.contains("\"stalled\": true");
    PipelineEvent::CoderExited {
        exit_code: session_exit_code(info),
        head_sha: None, // 호출자가 git rev-parse로 채운다
        stalled,
    }
}

/// advisor 출력에서 판정 마커 추출 — `[advisor:pass]` 또는 `[advisor:challenge:<이유>]`.
fn advisor_marker(screen: &str) -> (bool, String) {
    if screen.contains("[advisor:pass]") {
        return (true, String::new());
    }
    if let Some(pos) = screen.find("[advisor:challenge") {
        let tail = &screen[pos + "[advisor:challenge".len()..];
        let reason = tail
            .trim_start_matches(':')
            .split(']')
            .next()
            .unwrap_or("")
            .trim();
        let reason = if reason.is_empty() { "(사유 없음)" } else { reason };
        return (false, format!("advisor challenge: {reason}"));
    }
    (false, "advisor 마커 없음([advisor:pass] 미검출)".to_string())
}

/// `gh pr create` 출력에서 PR 번호 추출 — `…/pull/<num>` 꼬리.
fn parse_pr_number(text: &str) -> Option<u64> {
    for token in text.split_whitespace() {
        if let Some(pos) = token.find("/pull/") {
            let digits: String = token[pos + 6..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            if !digits.is_empty() {
                return digits.parse().ok();
            }
        }
    }
    None
}

// ---------- 실제 실행기 ----------

/// 코더가 exit 0으로 종료했는데 로컬 커밋을 빠뜨린 경우 안전망으로 자동 스테이징·로컬 커밋해
/// 다음 verifier 단계가 변경분이 포함된 실제 `HEAD`를 검증하도록 보장한다(clean이면 no-op).
fn ensure_worktree_committed(cwd: &Path, fork_id: &str, attempt: u32) {
    let cwd_s = cwd.display().to_string();
    let Ok(status) = ProcCommand::new("git")
        .args(["-C", &cwd_s, "status", "--porcelain"])
        .output()
    else {
        return;
    };
    if !status.status.success() || String::from_utf8_lossy(&status.stdout).trim().is_empty() {
        return;
    }
    let _ = ProcCommand::new("git")
        .args(["-C", &cwd_s, "add", "-A"])
        .output();
    let msg = format!("chore(pipeline): auto-commit {fork_id} attempt {attempt}");
    let _ = ProcCommand::new("git")
        .args(["-C", &cwd_s, "commit", "-m", &msg])
        .output();
}

/// 실제 실행기 — hc+`dg wrap` 워커 스폰·대기·결과 수집. 모든 외부 명령은
/// `plan_commands`와 동일한 argv로 실행된다(dry-run 출력 = 실제 실행 명령).
pub struct RealPipelineExecutor {
    pub cfg: ExecConfig,
}

impl RealPipelineExecutor {
    pub fn new(cfg: ExecConfig) -> Self {
        Self { cfg }
    }

    /// 경량 읽기전용 explore 세션 실행 → 출력 캡처 → `.herdr/context/<fork_id>-map.md` 저장 → ExploreExited.
    /// 탐색이 실패하거나 스톨해도 fail-open으로 ExploreExited를 반환해 코더 진행을 막지 않는다.
    fn run_explore(
        &self,
        state: &PipelineState,
        context_map_path: &str,
    ) -> Result<Option<PipelineEvent>, String> {
        let sess = session_name("explore", &state.fork_id, None);
        let cwd = worker_cwd(&self.cfg, state);
        let inner = explore_inner_argv(&self.cfg, state);
        let mut argv = wrap_spawn_argv(&self.cfg, &sess, "explore", "file_read", None, None, &inner);
        for i in 0..argv.len().saturating_sub(1) {
            if argv[i] == "--stall-ms" {
                argv[i + 1] = EXPLORE_STALL_MS.to_string();
            } else if argv[i] == "--max-ms" {
                argv[i + 1] = EXPLORE_MAX_MS.to_string();
            }
        }
        let info = match hc_run_to_exit(&self.cfg.hc_bin, &sess, &cwd, &argv) {
            Ok(i) => i,
            Err(e) => {
                eprint_line(&format!("[pipeline run] explore 스폰/대기 실패(fail-open): {e}"));
                hc_rm(&self.cfg.hc_bin, &sess);
                return Ok(Some(PipelineEvent::ExploreExited {
                    exit_code: 1,
                    context_map_path: None,
                    stalled: false,
                }));
            }
        };
        let exit_code = session_exit_code(&info);
        let screen = hc_read(&self.cfg.hc_bin, &sess, EXPLORE_READ_LINES);
        let stalled = screen.contains("\"stalled\":true") || screen.contains("\"stalled\": true");
        hc_rm(&self.cfg.hc_bin, &sess);

        let saved_path = if exit_code == 0 && !stalled && !screen.trim().is_empty() {
            let full_path = if Path::new(context_map_path).is_absolute() {
                PathBuf::from(context_map_path)
            } else {
                cwd.join(context_map_path)
            };
            if let Some(parent) = full_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let header = format!(
                "# Context Pack — {} (auto-generated by lightweight explore scout)\n\n",
                state.fork_id
            );
            let content = format!("{header}{}\n", screen.trim());
            match std::fs::write(&full_path, content) {
                Ok(()) => Some(context_map_path.to_string()),
                Err(e) => {
                    eprint_line(&format!("[pipeline run] context map 저장 실패(fail-open): {e}"));
                    None
                }
            }
        } else {
            None
        };

        Ok(Some(PipelineEvent::ExploreExited {
            exit_code,
            context_map_path: saved_path,
            stalled,
        }))
    }

    /// coder 세션 실행 → CoderExited. 세션명은 attempt와 결속해 재스폰을 구분한다.
    fn run_coder(
        &self,
        state: &PipelineState,
        attempt: u32,
        feedback: Option<&str>,
        verdict_path: Option<&str>,
    ) -> Result<Option<PipelineEvent>, String> {
        let sess = session_name("coder", &state.fork_id, Some(attempt));
        let cwd = worker_cwd(&self.cfg, state);
        let inner = coder_inner_argv(state, attempt, feedback, verdict_path);
        let step_fork = (attempt == 0).then_some(state.fork_id.as_str());
        let argv = wrap_spawn_argv(&self.cfg, &sess, "coder", "file_edit", step_fork, None, &inner);
        let info = hc_run_to_exit(&self.cfg.hc_bin, &sess, &cwd, &argv)?;
        let mut event = coder_event_from_session(&self.cfg.hc_bin, &sess, &info);
        if let PipelineEvent::CoderExited {
            exit_code,
            stalled,
            ref mut head_sha,
        } = event
        {
            if exit_code == 0 && !stalled {
                ensure_worktree_committed(&cwd, &state.fork_id, attempt);
            }
            *head_sha = verdict::resolve_git_head(&cwd);
        }
        hc_rm(&self.cfg.hc_bin, &sess);
        Ok(Some(event))
    }

    /// verifier 세션 실행 → VerdictReceived. verdict 판정은 `validate_verdict_file`의
    /// fail-closed `effective_decision`을 그대로 쓴다(무효·stale → Human → 에스컬레이션).
    fn run_verifier(&self, state: &PipelineState) -> Result<Option<PipelineEvent>, String> {
        let sess = session_name("verifier", &state.fork_id, Some(state.attempt));
        let cwd = worker_cwd(&self.cfg, state);
        let vp = verdict::resolve_verdict_path(&self.cfg.repo_dir, &state.fork_id);
        if let Some(dir) = vp.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let inner = verifier_inner_argv(state, &vp);
        let argv = wrap_spawn_argv(&self.cfg, &sess, "verifier", "file_read", None, Some(&vp), &inner);
        let _info = hc_run_to_exit(&self.cfg.hc_bin, &sess, &cwd, &argv)?;
        hc_rm(&self.cfg.hc_bin, &sess);
        let v = verdict::validate_verdict_file(
            &vp,
            &ValidateVerdictOptions {
                expected_fork_id: state.fork_id.clone(),
                expected_head: state.head_sha.clone(),
            },
        );
        // PASS 판정도 decision.db에 자동 역귀속(closed_by='verifier', neon_verdict='pass')해
        // 교차검증 실적이 누락되지 않게 보장한다(rework는 BackpropAndRespawnCoder에서 처리).
        if matches!(v.effective_decision, dgate_core::verdict::VerdictDecision::Pass) {
            let mut bp_args = vec![
                "backprop".to_string(),
                "--fork".to_string(),
                state.fork_id.clone(),
                "--verdict-file".to_string(),
                vp.display().to_string(),
            ];
            if let Some(h) = &state.head_sha {
                bp_args.push("--expected-head".to_string());
                bp_args.push(h.clone());
            }
            let _ = ProcCommand::new(&self.cfg.dg_bin)
                .args(&bp_args)
                .current_dir(&self.cfg.repo_dir)
                .output();
        }
        Ok(Some(PipelineEvent::VerdictReceived {
            decision: v.effective_decision,
            verdict_path: vp.display().to_string(),
        }))
    }

    /// advisor-g3 세션 실행 → `[advisor:pass]` 마커 확인 → push + `gh pr create`
    /// → AdvisorG3Result. pass인데 pr_number를 못 얻으면 상태머신이 프로토콜 위반으로
    /// 에스컬레이션한다(fail-closed — 여기서 추정하지 않는다).
    fn run_advisor_g3(&self, state: &PipelineState) -> Result<Option<PipelineEvent>, String> {
        let sess = session_name("advisor-g3", &state.fork_id, None);
        let cwd = worker_cwd(&self.cfg, state);
        let inner = advisor_inner_argv(&self.cfg, state);
        let argv = wrap_spawn_argv(&self.cfg, &sess, "advisor", "file_read", None, None, &inner);
        let _info = hc_run_to_exit(&self.cfg.hc_bin, &sess, &cwd, &argv)?;
        let screen = hc_read(&self.cfg.hc_bin, &sess, ADVISOR_READ_LINES);
        hc_rm(&self.cfg.hc_bin, &sess);
        let (passed, marker_reason) = advisor_marker(&screen);
        if !passed {
            return Ok(Some(PipelineEvent::AdvisorG3Result {
                passed: false,
                pr_number: None,
                reason: Some(marker_reason),
            }));
        }
        let cwd_s = cwd.display().to_string();
        let push = ProcCommand::new("git")
            .args(["-C", &cwd_s, "push", "-u", "origin", &state.branch])
            .output()
            .map_err(|e| format!("git 실행 불가: {e}"))?;
        if !push.status.success() {
            return Ok(Some(PipelineEvent::AdvisorG3Result {
                passed: false,
                pr_number: None,
                reason: Some(format!(
                    "git push 실패: {}",
                    String::from_utf8_lossy(&push.stderr).trim()
                )),
            }));
        }
        let gh = ProcCommand::new("gh")
            .args(["pr", "create", "--head", &state.branch, "--fill"])
            .current_dir(&cwd)
            .output()
            .map_err(|e| format!("gh 실행 불가: {e}"))?;
        if !gh.status.success() {
            return Ok(Some(PipelineEvent::AdvisorG3Result {
                passed: false,
                pr_number: None,
                reason: Some(format!(
                    "gh pr create 실패: {}",
                    String::from_utf8_lossy(&gh.stderr).trim()
                )),
            }));
        }
        let pr_number = parse_pr_number(&String::from_utf8_lossy(&gh.stdout));
        Ok(Some(PipelineEvent::AdvisorG3Result {
            passed: true,
            pr_number,
            reason: None,
        }))
    }

    /// 에스컬레이션 단발 알림 — stderr 출력 + `HC_TERMINAL_ID` 부모가 있으면 `hc send` 보고.
    fn notify_escalation(&self, state: &PipelineState, reason: &str) {
        let msg = format!("[pipeline] {} ESCALATED: {reason}", state.fork_id);
        eprint_line(&format!("[pipeline run] {msg}"));
        let Ok(tid) = std::env::var("HC_TERMINAL_ID") else {
            return;
        };
        let Some(info) = hc_get(&self.cfg.hc_bin, &tid) else {
            return;
        };
        let parent = info.get("parent").and_then(Value::as_str).unwrap_or("");
        if parent.is_empty() {
            return;
        }
        let _ = ProcCommand::new(&self.cfg.hc_bin)
            .args(["send", parent, &msg])
            .output();
    }
}

impl PipelineExecutor for RealPipelineExecutor {
    fn execute(
        &mut self,
        action: &PipelineAction,
        state: &PipelineState,
    ) -> Result<Option<PipelineEvent>, String> {
        match action {
            PipelineAction::SpawnExplore {
                context_map_path, ..
            } => self.run_explore(state, context_map_path),
            PipelineAction::SpawnCoder { attempt, feedback } => {
                self.run_coder(state, *attempt, feedback.as_deref(), None)
            }
            PipelineAction::BackpropAndRespawnCoder {
                fork_id,
                verdict_path,
                attempt,
            } => {
                // `dg backprop --fork <id> --verdict-file <path>` — verdict 역귀속 후 재스폰.
                // backprop 실패는 경고만 남기고 진행한다(verdict는 이미 fail-closed 검증됨).
                let mut bp_args = vec![
                    "backprop".to_string(),
                    "--fork".to_string(),
                    fork_id.clone(),
                    "--verdict-file".to_string(),
                    verdict_path.clone(),
                ];
                if let Some(h) = &state.head_sha {
                    bp_args.push("--expected-head".to_string());
                    bp_args.push(h.clone());
                }
                match ProcCommand::new(&self.cfg.dg_bin).args(&bp_args).output() {
                    Ok(o) if !o.status.success() => eprint_line(&format!(
                        "[pipeline run] dg backprop 실패(exit {:?}): {}",
                        o.status.code(),
                        String::from_utf8_lossy(&o.stderr).trim()
                    )),
                    Err(e) => eprint_line(&format!("[pipeline run] dg backprop 실행 불가: {e}")),
                    _ => {}
                }
                self.run_coder(state, *attempt, Some("verdict rework"), Some(verdict_path))
            }
            PipelineAction::SpawnVerifier { .. } => self.run_verifier(state),
            PipelineAction::SpawnAdvisorG3 { .. } => self.run_advisor_g3(state),
            PipelineAction::EmitReadyPr { .. } | PipelineAction::None => Ok(None),
            PipelineAction::WakeSubleadEscalation { reason } => {
                self.notify_escalation(state, reason);
                Ok(None)
            }
        }
    }
}

/// `dg pipeline run` 결과 — exit_code·READY_PR 신호·에스컬레이션 사유를 담는다.
#[derive(Debug)]
pub struct RunOutcome {
    pub exit_code: i32,
    pub steps: u32,
    pub final_phase: PipelinePhase,
    pub ready_pr_signal: Option<String>,
    pub escalation_reason: Option<String>,
    pub max_steps_exceeded: bool,
}

/// 결정적 러너 루프 — `위상→액션→실행→이벤트→전이→원자적 저장`을 max_steps까지 반복.
/// 종결 위상 도달 시 ReadyPr는 exit 0, Escalated/Failed는 exit 3. 스텝 소진은
/// 결정적 한계 도달로 보고 에스컬레이션 알림 후 exit 3(상태 파일은 그대로 둔다).
pub fn run_pipeline(
    mut state: PipelineState,
    state_dir: &Path,
    executor: &mut dyn PipelineExecutor,
    max_steps: u32,
) -> Result<RunOutcome, String> {
    let mut steps = 0u32;
    let mut signal: Option<String> = None;
    loop {
        if state.phase.is_terminal() {
            return Ok(RunOutcome {
                exit_code: if state.phase == PipelinePhase::ReadyPr { 0 } else { 3 },
                steps,
                final_phase: state.phase,
                ready_pr_signal: signal,
                escalation_reason: state.escalation_reason.clone(),
                max_steps_exceeded: false,
            });
        }
        if steps >= max_steps {
            let reason = format!("max_steps {max_steps} 초과 — 무한 루프 방지로 강제 에스컬레이션");
            let action = PipelineAction::WakeSubleadEscalation {
                reason: reason.clone(),
            };
            let _ = executor.execute(&action, &state);
            return Ok(RunOutcome {
                exit_code: 3,
                steps,
                final_phase: state.phase,
                ready_pr_signal: signal,
                escalation_reason: Some(reason),
                max_steps_exceeded: true,
            });
        }
        let action = action_for_phase(&state)?;
        let event = executor
            .execute(&action, &state)?
            .ok_or_else(|| format!("executor가 액션 {}의 이벤트를 반환하지 않음", action.label()))?;
        let (next, out_action) = pipeline::advance_pipeline(state, event);
        pipeline::save_state(state_dir, &next)?;
        steps += 1;
        state = next;
        match &out_action {
            PipelineAction::EmitReadyPr { pr_number, head_sha } => {
                signal = Some(format!(
                    "READY_PR={} HEAD={} G3=PASS",
                    pr_number,
                    head_sha.as_deref().unwrap_or("unknown")
                ));
                let _ = executor.execute(&out_action, &state);
            }
            PipelineAction::WakeSubleadEscalation { .. } => {
                let _ = executor.execute(&out_action, &state);
            }
            _ => {}
        }
    }
}

/// `dg pipeline run --fork <id> [--state-dir <dir>] [--repo <dir>] [--max-steps 20]
/// [--dry-run] [--json]` — 상태 파일을 로드해 파이프라인을 자동 구동한다.
fn cmd_run(args: &ParsedArgs, json: bool) -> CmdResult {
    let fork_id = require_flag(args, "fork")?;
    let dir = state_dir(args);
    let state =
        pipeline::load_state(&dir, &fork_id).map_err(|e| DgError::Usage(UsageError(e)))?;
    let max_steps = parse_int_flag(args, "max-steps", RUN_DEFAULT_MAX_STEPS)?;
    if max_steps < 1 {
        return Err(DgError::Usage(UsageError(
            "--max-steps must be >= 1".to_string(),
        )));
    }
    let cfg = ExecConfig::from_args(args);

    // --dry-run — 외부 프로세스 0회: 현재 위상의 예정 명령 목록만 출력하고 종료.
    if bool_flag(args, "dry-run") {
        if state.phase.is_terminal() {
            emit(
                json,
                format!(
                    "[pipeline run] {fork_id} — 이미 종결 위상 {}",
                    state.phase.as_str()
                ),
                &json!({
                    "action": "run",
                    "dry_run": true,
                    "fork_id": fork_id,
                    "phase": state.phase.as_str(),
                    "commands": Vec::<Vec<String>>::new(),
                }),
            );
            return Ok(0);
        }
        let action = action_for_phase(&state).map_err(|e| DgError::Usage(UsageError(e)))?;
        let commands = plan_commands(&action, &state, &cfg);
        let mut human = format!(
            "[pipeline run] {fork_id} dry-run — {} (action={})",
            state.phase.as_str(),
            action.label()
        );
        for c in &commands {
            human.push_str(&format!("\n  $ {}", c.join(" ")));
        }
        emit(
            json,
            human,
            &json!({
                "action": "run",
                "dry_run": true,
                "fork_id": fork_id,
                "phase": state.phase.as_str(),
                "planned_action": action,
                "commands": commands,
            }),
        );
        return Ok(0);
    }

    let mut executor = RealPipelineExecutor::new(cfg);
    match run_pipeline(state, &dir, &mut executor, max_steps as u32) {
        Ok(outcome) => {
            let mut human = format!(
                "[pipeline run] {fork_id} → {} steps={}",
                outcome.final_phase.as_str(),
                outcome.steps
            );
            if let Some(sig) = &outcome.ready_pr_signal {
                human.push_str(&format!("\n{sig}"));
            }
            if let Some(r) = &outcome.escalation_reason {
                human.push_str(&format!("\nESCALATED: {r}"));
            }
            emit(
                json,
                human,
                &json!({
                    "action": "run",
                    "fork_id": fork_id,
                    "final_phase": outcome.final_phase.as_str(),
                    "steps": outcome.steps,
                    "exit_code": outcome.exit_code,
                    "ready_pr_signal": outcome.ready_pr_signal,
                    "escalation_reason": outcome.escalation_reason,
                    "max_steps_exceeded": outcome.max_steps_exceeded,
                }),
            );
            Ok(outcome.exit_code)
        }
        Err(e) => {
            emit(
                json,
                format!("[pipeline run] {fork_id} 실행 오류: {e}"),
                &json!({
                    "action": "run",
                    "fork_id": fork_id,
                    "errors": [e],
                }),
            );
            Ok(1)
        }
    }
}

#[cfg(test)]
mod run_tests {
    use super::*;
    use std::collections::VecDeque;

    use dgate_core::pipeline::{self, PipelineTransition};
    use dgate_core::verdict::VerdictDecision;

    /// 스크립트된 이벤트 큐 + 호출 기록 Mock 실행기 — 외부 프로세스 없이
    /// 전체 루프(`Init → … → ReadyPr`/`Escalated` 탈출)를 결정적으로 검증한다.
    struct MockExecutor {
        calls: Vec<String>,
        events: VecDeque<PipelineEvent>,
    }

    impl MockExecutor {
        fn new(events: Vec<PipelineEvent>) -> Self {
            Self {
                calls: Vec::new(),
                events: events.into(),
            }
        }
        fn count(&self, label: &str) -> usize {
            self.calls.iter().filter(|c| c.as_str() == label).count()
        }
    }

    impl PipelineExecutor for MockExecutor {
        fn execute(
            &mut self,
            action: &PipelineAction,
            _state: &PipelineState,
        ) -> Result<Option<PipelineEvent>, String> {
            self.calls.push(action.label().to_string());
            Ok(self.events.pop_front())
        }
    }

    fn tmp_state_dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dg-run-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn init_state(fork: &str, max_reworks: u32) -> PipelineState {
        pipeline::init_pipeline(&PipelineInit {
            fork_id: fork.to_string(),
            run_id: None,
            brief_path: ".herdr/briefs/t.md".to_string(),
            worktree_path: Some("/tmp/wt".to_string()),
            branch: "feat/t".to_string(),
            scope_glob: None,
            max_reworks,
            coder_model: "codex-gpt5".to_string(),
            verifier_model: "glm-5.3".to_string(),
            with_explore: false,
            explore_model: None,
        })
        .0
    }

    fn coder_ok() -> PipelineEvent {
        PipelineEvent::CoderExited {
            exit_code: 0,
            head_sha: Some("abc1234".to_string()),
            stalled: false,
        }
    }

    fn verdict(d: VerdictDecision) -> PipelineEvent {
        PipelineEvent::VerdictReceived {
            decision: d,
            verdict_path: ".herdr/verify/fk_r-verdict.json".to_string(),
        }
    }

    // 1) 정상 완주 — Init→Coder(0)→Verifier(Pass)→G3(Pass,PR=77)→ReadyPr, exit 0 + READY_PR 신호
    #[test]
    fn run_happy_path_ready_pr() {
        let dir = tmp_state_dir("happy");
        let state = init_state("fk_r", 2);
        let mut mock = MockExecutor::new(vec![
            coder_ok(),
            verdict(VerdictDecision::Pass),
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number: Some(77),
                reason: None,
            },
        ]);
        let outcome = run_pipeline(state, &dir, &mut mock, 20).unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(outcome.final_phase, PipelinePhase::ReadyPr);
        assert_eq!(
            outcome.ready_pr_signal.as_deref(),
            Some("READY_PR=77 HEAD=abc1234 G3=PASS")
        );
        assert_eq!(
            mock.calls,
            vec!["spawn_coder", "spawn_verifier", "spawn_advisor_g3", "emit_ready_pr"]
        );
        // 매 스텝 영속화 — 마지막 상태 파일이 ready_pr
        let saved = pipeline::load_state(&dir, "fk_r").unwrap();
        assert_eq!(saved.phase, PipelinePhase::ReadyPr);
        assert_eq!(saved.pr_number, Some(77));
        std::fs::remove_dir_all(&dir).ok();
    }

    // 2) 1회 rework 후 2회차 PASS 완주 — backprop 호출 정확히 1회
    #[test]
    fn run_rework_once_then_pass() {
        let dir = tmp_state_dir("rework");
        let state = init_state("fk_r", 2);
        let mut mock = MockExecutor::new(vec![
            coder_ok(),
            verdict(VerdictDecision::Rework),
            coder_ok(),
            verdict(VerdictDecision::Pass),
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number: Some(7),
                reason: None,
            },
        ]);
        let outcome = run_pipeline(state, &dir, &mut mock, 20).unwrap();
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(mock.count("backprop_and_respawn_coder"), 1);
        assert_eq!(mock.count("spawn_coder"), 1);
        assert_eq!(mock.count("spawn_verifier"), 2);
        let saved = pipeline::load_state(&dir, "fk_r").unwrap();
        assert_eq!(saved.attempt, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    // 3) rework 예산 소진 → Escalated + WakeSubleadEscalation 호출 + exit 3
    #[test]
    fn run_rework_budget_exhausted_escalates() {
        let dir = tmp_state_dir("esc");
        let state = init_state("fk_r", 1); // max_reworks=1
        let mut mock = MockExecutor::new(vec![
            coder_ok(),
            verdict(VerdictDecision::Rework), // attempt 1 → backprop 재스폰
            coder_ok(),
            verdict(VerdictDecision::Fail), // 예산 소진 → escalate
        ]);
        let outcome = run_pipeline(state, &dir, &mut mock, 20).unwrap();
        assert_eq!(outcome.exit_code, 3);
        assert_eq!(outcome.final_phase, PipelinePhase::Escalated);
        assert_eq!(mock.count("backprop_and_respawn_coder"), 1);
        assert_eq!(mock.count("wake_sublead_escalation"), 1);
        assert!(outcome.escalation_reason.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    // 4) --dry-run — 외부 프로세스 0회(plan_commands는 순수 빌더, executor 미호출) +
    //    예정 명령 리스트(hc run/wait/get·dg backprop·dg wrap·devin·pi·gh pr create) 반환
    #[test]
    fn run_dry_run_lists_commands_without_spawning() {
        let cfg = ExecConfig {
            hc_bin: "hc".to_string(),
            dg_bin: "dg".to_string(),
            repo_dir: PathBuf::from("/repo"),
        };
        // coder_running — hc run(dg wrap + devin … </dev/null) + hc wait/get + git rev-parse
        let state = init_state("fk_r", 2);
        let action = action_for_phase(&state).unwrap();
        assert!(matches!(action, PipelineAction::SpawnCoder { attempt: 0, .. }));
        let cmds = plan_commands(&action, &state, &cfg);
        let joined = cmds.iter().map(|c| c.join(" ")).collect::<Vec<_>>().join("\n");
        assert!(cmds.iter().any(|c| c[0] == "hc" && c[1] == "run"));
        assert!(joined.contains("dg wrap") || joined.contains("dg\" wrap") || joined.contains(" wrap "));
        let stdin_redir = if cfg!(windows) { "< NUL" } else { "</dev/null" };
        assert!(joined.contains("devin") && joined.contains(stdin_redir));
        assert!(joined.contains("로컬 git commit까지 완료하라"));
        assert!(!joined.contains("git commit/push는 금지"));
        assert!(cmds.iter().any(|c| c[0] == "hc" && c[1] == "wait"));
        assert!(cmds.iter().any(|c| c[0] == "git" && c.join(" ").contains("rev-parse")));

        // verifier_running — dg wrap --role verifier + pi --provider opencodex --model glm-5.3
        let mut s2 = state.clone();
        s2.phase = PipelinePhase::VerifierRunning;
        s2.history.push(PipelineTransition {
            from: PipelinePhase::CoderRunning,
            event: "coder_exited".to_string(),
            to: PipelinePhase::VerifierRunning,
            action: "spawn_verifier".to_string(),
            note: None,
        });
        let action = action_for_phase(&s2).unwrap();
        let cmds = plan_commands(&action, &s2, &cfg);
        let joined = cmds.iter().map(|c| c.join(" ")).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("--role verifier"));
        assert!(joined.contains("--tool-class file_read"));
        assert!(!joined.contains("--fork fk_r"));
        assert!(joined.contains("opencodex") && joined.contains("glm-5.3"));
        assert!(joined.contains("--no-session"));
        assert!(joined.contains("--no-skills"));
        assert!(joined.contains("--mode json") || joined.contains("--mode' 'json"));
        assert!(joined.contains("-p"));
        assert!(joined.contains("PI_CODING_AGENT_DIR="));
        assert!(!joined.contains("--no-approve"));

        // advisor_g3_running — dg wrap --role advisor + gh pr create 계획 포함
        let mut s3 = state.clone();
        s3.phase = PipelinePhase::AdvisorG3Running;
        let action = action_for_phase(&s3).unwrap();
        let cmds = plan_commands(&action, &s3, &cfg);
        let joined = cmds.iter().map(|c| c.join(" ")).collect::<Vec<_>>().join("\n");
        assert!(joined.contains("--role advisor"));
        assert!(joined.contains("--tool-class file_read"));
        assert!(!joined.contains("--fork fk_r"));
        assert!(cmds.iter().any(|c| c[0] == "gh" && c.join(" ").contains("pr create")));

        // backprop 경유 rework_pending — dg backprop --verdict-file 선행, rework 코더는 닫힌 루트 --fork 미재사용
        let mut s4 = state.clone();
        s4.phase = PipelinePhase::ReworkPending;
        s4.attempt = 1;
        s4.last_verdict_path = Some(".herdr/verify/fk_r-verdict.json".to_string());
        s4.history.push(PipelineTransition {
            from: PipelinePhase::VerifierRunning,
            event: "verdict_received".to_string(),
            to: PipelinePhase::ReworkPending,
            action: "backprop_and_respawn_coder".to_string(),
            note: None,
        });
        let action = action_for_phase(&s4).unwrap();
        assert!(matches!(action, PipelineAction::BackpropAndRespawnCoder { attempt: 1, .. }));
        let cmds = plan_commands(&action, &s4, &cfg);
        assert_eq!(cmds[0][0], "dg");
        assert_eq!(cmds[0][1], "backprop");
        assert!(cmds[0].join(" ").contains("--verdict-file"));
        assert!(!cmds[1].join(" ").contains("--fork fk_r"));
    }

    // 부록) 유틸 — 세션명 계약·마커·PR 번호 파싱
    #[test]
    fn run_helpers() {
        assert_eq!(
            session_name("coder", "fk_abc123", Some(0)),
            "coder-fk_abc123-a0"
        );
        let long = session_name("verifier", "FK_Upper.Case/Slug", Some(2));
        assert!(long.len() <= 32 && long.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_'));

        let (pass, _) = advisor_marker("log… [advisor:pass] done");
        assert!(pass);
        let (fail, reason) = advisor_marker("[advisor:challenge: P0 보안 문제]");
        assert!(!fail && reason.contains("P0 보안 문제"));
        let (none, reason) = advisor_marker("no marker");
        assert!(!none && reason.contains("마커 없음"));

        assert_eq!(
            parse_pr_number("Creating pull request\nhttps://github.com/o/r/pull/77\n"),
            Some(77)
        );
        assert_eq!(parse_pr_number("no url"), None);
    }

    // Windows 스폰 분기 — is_windows 플래그로 양 OS argv를 macOS에서 검증한다.
    // Unix 경로는 기존 계약(sh -c exec env … </dev/null) 불변, Windows 경로는
    // cmd /C + set env + < NUL이며 sh//dev/null 의존이 없어야 한다.
    #[test]
    fn wrap_inner_command_os_branch() {
        let inner = vec![
            "devin".to_string(),
            "--permission-mode".to_string(),
            "dangerous".to_string(),
            "-p".to_string(),
            "브리프 파일 x.md 를 읽고 구현하라".to_string(),
        ];

        // is_windows=false — 기존 Unix 계약 보존
        let u = wrap_inner_command(false, "coder", &inner);
        assert_eq!(u.len(), 3);
        assert_eq!(u[0], "sh");
        assert_eq!(u[1], "-c");
        assert!(u[2].contains("HERDR_ROLE=coder"));
        assert!(u[2].contains("HERDR_DELEGATE_MODE=hc"));
        assert!(u[2].ends_with("</dev/null"));
        assert!(u[2].starts_with("exec env "));
        assert!(u[2].contains("devin"));

        // is_windows=true — cmd /C + set env + < NUL, sh//dev/null 미포함
        let w = wrap_inner_command(true, "verifier", &inner);
        assert_eq!(w[0], "cmd");
        assert_eq!(w[1], "/C");
        assert!(!w.iter().any(|a| a == "sh"));
        assert!(!w.join(" ").contains("/dev/null"));
        // set `VAR=v&&`는 `&&` 앞 공백이 env 값에 섞이지 않도록 한 토큰이어야 한다.
        assert!(w.iter().any(|a| a == "HERDR_ROLE=verifier&&"));
        assert!(w.iter().any(|a| a == "HERDR_DELEGATE_MODE=hc&&"));
        assert!(w.join(" ").contains("< NUL"));
        // 내부 워커 argv는 뒤쪽에 그대로 이어진다(공백 인자는 Rust 인용으로 묶인다).
        let pos = w.iter().position(|a| a == "devin").unwrap();
        assert_eq!(&w[pos..], ["devin", "--permission-mode", "dangerous", "-p", "브리프 파일 x.md 를 읽고 구현하라", "<", "NUL"]);
    }

    // cmd /C 메타문자 이스케이프 — 공백·인용 없는 인자만 `^`로 보호하고,
    // 공백/인용/빈 인자는 Rust 인용 계층에 맡긴다.
    #[test]
    fn cmd_escape_token_metachars() {
        assert_eq!(cmd_escape_token("a&b"), "a^&b");
        assert_eq!(cmd_escape_token("x|y"), "x^|y");
        assert_eq!(cmd_escape_token("a<b>c"), "a^<b^>c");
        assert_eq!(cmd_escape_token("x^y"), "x^^y");
        // 공백이 있으면 Rust가 "…"로 감싸므로 ^ 이스케이프를 넣으면 오히려 리터럴 ^가 샌다.
        assert_eq!(cmd_escape_token("a & b"), "a & b");
        assert_eq!(cmd_escape_token("a\"b"), "a\"b");
        assert_eq!(cmd_escape_token(""), "");
        assert_eq!(cmd_escape_token("glm-zcode/glm-5.3"), "glm-zcode/glm-5.3");
    }

    #[test]
    fn explore_commands_are_readonly_and_coder_prompt_injects_context_map() {
        let (mut state, action) = pipeline::init_pipeline(&PipelineInit {
            fork_id: "fk_exp1".to_string(),
            run_id: None,
            brief_path: ".herdr/briefs/ux.md".to_string(),
            worktree_path: Some("/tmp/wt-ux".to_string()),
            branch: "feat/ux".to_string(),
            scope_glob: None,
            max_reworks: 2,
            coder_model: "glm-5.4".to_string(),
            verifier_model: "deepseek-v4.1-flash".to_string(),
            with_explore: true,
            explore_model: Some("deepseek-v4.1-flash".to_string()),
        });
        assert_eq!(state.phase, PipelinePhase::ExploreRunning);
        let cfg = ExecConfig {
            hc_bin: "hc".to_string(),
            dg_bin: "dg".to_string(),
            repo_dir: PathBuf::from("/tmp/repo"),
        };
        let cmds = plan_commands(&action, &state, &cfg);
        assert_eq!(cmds.len(), 4);
        let joined = cmds[0].join(" ");
        assert!(joined.contains("--tools") && joined.contains("read,grep,find,ls"));
        assert!(joined.contains("--exclude-tools") && joined.contains("edit,write"));
        assert!(joined.contains("--max-ms 300000"));

        state.context_map_path = Some(".herdr/context/fk_exp1-map.md".to_string());
        let cp = coder_prompt(&state, 0, None, None);
        assert!(cp.contains(".herdr/context/fk_exp1-map.md"));
        assert!(cp.contains("파일·줄번호 앵커"));
    }
}
