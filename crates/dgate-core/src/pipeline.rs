//! 결정적 sublead 파이프라인 상태머신 — `coder → verifier → rework/advisor-g3 → READY_PR`
//! 전이를 상주 LLM 없이(0토큰) 순수 함수로 계산하고, 상태를 `.herdr/pipelines/<run_id>.json`
//! 파일로 영속화한다(paperwork-runtime 실전 피드백: 기계적 전이에 ~70% 토큰 낭비).
//!
//! 외부 연결:
//! - `dg pipeline init|step|status`(`crates/dgate-cli`)가 이 모듈의 상태 파일을 읽고
//!   `advance_pipeline`으로 다음 결정적 액션을 계산한다 — CLI는 스케줄링만 하고 판정은 전부 여기.
//! - verdict 수신은 `crate::verdict::validate_verdict_file`의 `effective_decision`을 그대로
//!   소비한다(fail-closed — 무효 verdict는 Human → 즉시 에스컬레이션).
//!
//! 주의:
//! - 이종 모델 불변식: `normalize_model_family(coder) == normalize_model_family(verifier)`
//!   이면 Verifier 스폰을 거부하고 `Escalated`로 fail-closed한다.
//! - 에스컬레이션(`Escalated`)이 유일한 "사람/sublead LLM 깨우기" 경로다 — 결정적 재시도
//!   소진·human 판정·G3 실패·프로토콜 위반(위상-이벤트 불일치)만 단발 LLM을 깨운다.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::verdict::{normalize_model_family, VerdictDecision};

/// 상태 파일 스키마 버전 — 상위 호환 없는 필드 변경 시 +1.
pub const PIPELINE_SCHEMA_VERSION: u32 = 1;
/// `max_reworks` 기본값 — coder 재작업(재스폰) 허용 횟수.
pub const DEFAULT_MAX_REWORKS: u32 = 2;
/// 기본 상태 디렉터리(저장소 cwd 기준 상대 경로).
pub const DEFAULT_STATE_DIR: &str = ".herdr/pipelines";
/// 기본 사전 탐색 Context Pack 디렉터리(저장소 cwd 기준 상대 경로).
pub const DEFAULT_CONTEXT_DIR: &str = ".herdr/context";

/// fork별 기본 Context Pack 경로(`.herdr/context/<fork_id>-map.md`).
pub fn default_context_map_path(fork_id: &str) -> String {
    format!("{DEFAULT_CONTEXT_DIR}/{fork_id}-map.md")
}

/// 브리프 본문에 코더가 바로 구현에 들어갈 수 있는 코드 앵커(파일 경로·줄번호·Context Pack)가
/// 충분한지 판정한다(순수 함수 — `dg pipeline init`과 `dg dispatch`가 공유).
///
/// 통과 기준(하나라도 충족 시 `true` = 사전 탐색 스킵 가능):
/// 1. `## Context Pack` 또는 `## Target Anchors` 헤딩 존재
/// 2. `파일명.확장자:줄번호` 또는 `파일명.확장자#L줄번호` 형태 앵커 1건 이상
/// 3. `.herdr/` 외 실제 소스/설정 파일 경로 2건 이상
pub fn brief_has_code_anchors(text: &str) -> bool {
    const SOURCE_EXTS: &[&str] = &[
        ".rs", ".ts", ".tsx", ".js", ".jsx", ".mjs", ".cjs", ".vue", ".svelte", ".py", ".go",
        ".java", ".kt", ".swift", ".c", ".cc", ".cpp", ".h", ".hpp", ".rb", ".php", ".cs",
        ".html", ".css", ".scss", ".sql", ".toml", ".yaml", ".yml", ".json",
    ];
    let mut source_paths = std::collections::BTreeSet::new();

    for raw_line in text.lines() {
        let trimmed = raw_line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("## context pack") || lower.starts_with("## target anchors") {
            return true;
        }
        for raw_tok in trimmed.split(|c: char| {
            c.is_whitespace() || matches!(c, '`' | '\'' | '"' | '(' | ')' | '[' | ']' | '<' | '>' | ',' | ';')
        }) {
            let tok = raw_tok.trim_matches(|c: char| matches!(c, '*' | '_' | '.'));
            if tok.is_empty() || tok.starts_with("http://") || tok.starts_with("https://") {
                continue;
            }
            if tok.contains(".herdr/") {
                continue;
            }
            // `file.ext:123` 또는 `file.ext#L123` 형태 앵커
            for ext in SOURCE_EXTS {
                if let Some(idx) = tok.find(ext) {
                    let after = &tok[idx + ext.len()..];
                    if let Some(rest) = after.strip_prefix(':') {
                        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                            return true;
                        }
                    }
                    if let Some(rest) = after.strip_prefix("#L").or_else(|| after.strip_prefix("#l")) {
                        if rest.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                            return true;
                        }
                    }
                    if after.is_empty() && tok.contains('/') {
                        source_paths.insert(tok.to_string());
                    }
                }
            }
        }
    }
    source_paths.len() >= 2
}

/// 파이프라인 위상 — 종결(terminal) 위상은 ReadyPr·Escalated·Failed다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipelinePhase {
    /// 생성 직후(워커 미스폰). init 액션과 함께 ExploreRunning 또는 CoderRunning으로 전이된다.
    Init,
    /// 경량 읽기전용 explore 워커가 Context Pack(`.herdr/context/<fork_id>-map.md`) 생성 중.
    ExploreRunning,
    /// coder 워커 실행 중(첫 스폰).
    CoderRunning,
    /// verifier 워커 실행 중(coder exit 0 이후).
    VerifierRunning,
    /// 재작업 대기 — coder 실패/verdict rework·fail로 `attempt` 소비 후 재스폰 지시 상태.
    /// 재스폰된 coder의 종료 이벤트도 이 위상에서 받는다.
    ReworkPending,
    /// advisor-g3(P0 게이트) 실행 중 — verdict pass 이후.
    AdvisorG3Running,
    /// 종결: PR 준비 완료 — `READY_PR=<num> HEAD=<sha> G3=PASS` 신호 출력.
    ReadyPr,
    /// 종결: 결정적 처리 실패 — sublead LLM 단발 기동 필요.
    Escalated,
    /// 종결(예약): 외부 요인에 의한 강제 중단 표기용 — 현재 전이 규칙은 생산하지 않는다.
    Failed,
}

impl PipelinePhase {
    /// 사람 출력·로그용 snake_case 라벨.
    pub fn as_str(&self) -> &'static str {
        match self {
            PipelinePhase::Init => "init",
            PipelinePhase::ExploreRunning => "explore_running",
            PipelinePhase::CoderRunning => "coder_running",
            PipelinePhase::VerifierRunning => "verifier_running",
            PipelinePhase::ReworkPending => "rework_pending",
            PipelinePhase::AdvisorG3Running => "advisor_g3_running",
            PipelinePhase::ReadyPr => "ready_pr",
            PipelinePhase::Escalated => "escalated",
            PipelinePhase::Failed => "failed",
        }
    }

    /// 종결 위상 여부 — 종결 위상에 도착한 이벤트는 무시(위상 불변)한다.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            PipelinePhase::ReadyPr | PipelinePhase::Escalated | PipelinePhase::Failed
        )
    }
}

/// 단일 전이 이력 — 감사·`dg pipeline status` 출력용 결정적 로그(시각 미포함).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineTransition {
    pub from: PipelinePhase,
    /// 이벤트 라벨(init|coder_exited|verdict_received|advisor_g3_result).
    pub event: String,
    pub to: PipelinePhase,
    /// 액션 라벨(spawn_coder|spawn_verifier|backprop_and_respawn_coder|
    /// spawn_advisor_g3|emit_ready_pr|wake_sublead_escalation|none).
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// 영속 상태 문서 — `.herdr/pipelines/<run_id>.json`으로 저장된다.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PipelineState {
    pub schema_version: u32,
    pub run_id: String,
    pub fork_id: String,
    pub brief_path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    pub branch: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope_glob: Option<String>,
    pub phase: PipelinePhase,
    /// 소비한 재작업 횟수(0부터 시작, 재작업마다 +1). max_reworks 초과 시 에스컬레이션.
    pub attempt: u32,
    pub max_reworks: u32,
    pub coder_model: String,
    pub verifier_model: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explore_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_map_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verdict_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub escalation_reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pr_number: Option<u64>,
    #[serde(default)]
    pub history: Vec<PipelineTransition>,
}

/// `dg pipeline init` 입력 — 상태 파일 초기화에 필요한 최소 계약.
#[derive(Debug, Clone)]
pub struct PipelineInit {
    pub fork_id: String,
    /// 미지정 시 fork_id를 run_id로 쓴다(상태 파일명 = fork_id).
    pub run_id: Option<String>,
    pub brief_path: String,
    pub worktree_path: Option<String>,
    pub branch: String,
    pub scope_glob: Option<String>,
    pub max_reworks: u32,
    pub coder_model: String,
    pub verifier_model: String,
    /// `true`면 코더 투입 전 경량 읽기전용 `explore` 단계를 먼저 실행해 Context Pack을 만든다.
    pub with_explore: bool,
    /// 경량 탐색 워커 모델(미지정 시 로스터 `explore` 슬롯 기본값 사용).
    pub explore_model: Option<String>,
}

/// 상태머신 입력 이벤트 — 외부 실행기(hc/러너)의 관측 결과만 받는다.
#[derive(Debug, Clone, PartialEq)]
pub enum PipelineEvent {
    /// 경량 explore(사전 탐색) 종료 — 성공 시 context_map_path를 코더 프롬프트에 주입하고,
    /// 실패/스톨해도 fail-open으로 바로 CoderRunning으로 전이한다.
    ExploreExited {
        exit_code: i32,
        context_map_path: Option<String>,
        stalled: bool,
    },
    /// coder 프로세스 종료 — `exit_code != 0` 또는 `stalled`면 실패 경로.
    CoderExited {
        exit_code: i32,
        head_sha: Option<String>,
        stalled: bool,
    },
    /// verifier verdict 수신 — decision은 `validate_verdict_file`의 effective_decision
    /// (fail-closed — 무효 verdict는 CLI가 Human으로 넣는다).
    VerdictReceived {
        decision: VerdictDecision,
        verdict_path: String,
    },
    /// advisor-g3 결과 — pass 시 pr_number가 있어야 ReadyPr로 간다(없으면 에스컬레이션).
    AdvisorG3Result {
        passed: bool,
        pr_number: Option<u64>,
        reason: Option<String>,
    },
}

/// 상태머신 출력 — 실행기가 해석해 프로세스를 스폰·알림하는 결정적 지시.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum PipelineAction {
    /// 전이 없음(종결 위상 이벤트 무시 등).
    None,
    /// 경량 읽기전용 explore 워커 스폰 — Context Pack(`.herdr/context/<fork_id>-map.md`) 생성.
    SpawnExplore {
        fork_id: String,
        context_map_path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        explore_model: Option<String>,
    },
    /// coder (재)스폰 — attempt는 이번 실행의 재작업 번호.
    SpawnCoder {
        attempt: u32,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// 이종 verifier 스폰 — coder exit 0 후 fork HEAD를 검증한다.
    SpawnVerifier {
        fork_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_sha: Option<String>,
        verifier_model: String,
    },
    /// `dg backprop` 후 verdict를 첨부해 coder 재스폰.
    BackpropAndRespawnCoder {
        fork_id: String,
        verdict_path: String,
        attempt: u32,
    },
    /// advisor-g3(P0 게이트) 스폰 — verdict pass 후 PR 게이트.
    SpawnAdvisorG3 {
        fork_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_sha: Option<String>,
        verdict_path: String,
    },
    /// `READY_PR=<num> HEAD=<sha> G3=PASS` 신호 — 리드에게 PR 준비 완료 통지.
    EmitReadyPr {
        pr_number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_sha: Option<String>,
    },
    /// sublead LLM 단발 기동 — 결정적 처리 소진/판단 불가 사유.
    WakeSubleadEscalation { reason: String },
}

impl PipelineAction {
    /// 이력·출력용 라벨.
    pub fn label(&self) -> &'static str {
        match self {
            PipelineAction::None => "none",
            PipelineAction::SpawnExplore { .. } => "spawn_explore",
            PipelineAction::SpawnCoder { .. } => "spawn_coder",
            PipelineAction::SpawnVerifier { .. } => "spawn_verifier",
            PipelineAction::BackpropAndRespawnCoder { .. } => "backprop_and_respawn_coder",
            PipelineAction::SpawnAdvisorG3 { .. } => "spawn_advisor_g3",
            PipelineAction::EmitReadyPr { .. } => "emit_ready_pr",
            PipelineAction::WakeSubleadEscalation { .. } => "wake_sublead_escalation",
        }
    }
}

fn event_label(event: &PipelineEvent) -> &'static str {
    match event {
        PipelineEvent::ExploreExited { .. } => "explore_exited",
        PipelineEvent::CoderExited { .. } => "coder_exited",
        PipelineEvent::VerdictReceived { .. } => "verdict_received",
        PipelineEvent::AdvisorG3Result { .. } => "advisor_g3_result",
    }
}

fn escalate(state: &mut PipelineState, from: PipelinePhase, event: &PipelineEvent, reason: String) -> PipelineAction {
    state.escalation_reason = Some(reason.clone());
    state.phase = PipelinePhase::Escalated;
    state.history.push(PipelineTransition {
        from,
        event: event_label(event).to_string(),
        to: PipelinePhase::Escalated,
        action: "wake_sublead_escalation".to_string(),
        note: Some(reason.clone()),
    });
    PipelineAction::WakeSubleadEscalation { reason }
}

/// `dg pipeline init` — 초기 상태 생성 + 첫 결정적 액션(coder 스폰 지시).
/// 위상은 즉시 Init → CoderRunning으로 전이되고 이력에 기록된다.
pub fn init_pipeline(init: &PipelineInit) -> (PipelineState, PipelineAction) {
    let mut state = PipelineState {
        schema_version: PIPELINE_SCHEMA_VERSION,
        run_id: init.run_id.clone().unwrap_or_else(|| init.fork_id.clone()),
        fork_id: init.fork_id.clone(),
        brief_path: init.brief_path.clone(),
        worktree_path: init.worktree_path.clone(),
        branch: init.branch.clone(),
        scope_glob: init.scope_glob.clone(),
        phase: PipelinePhase::Init,
        attempt: 0,
        max_reworks: init.max_reworks,
        coder_model: init.coder_model.clone(),
        verifier_model: init.verifier_model.clone(),
        explore_model: init.explore_model.clone(),
        context_map_path: None,
        head_sha: None,
        last_verdict_path: None,
        escalation_reason: None,
        pr_number: None,
        history: Vec::new(),
    };
    let (next_phase, action) = if init.with_explore {
        let map_path = default_context_map_path(&init.fork_id);
        (
            PipelinePhase::ExploreRunning,
            PipelineAction::SpawnExplore {
                fork_id: init.fork_id.clone(),
                context_map_path: map_path,
                explore_model: init.explore_model.clone(),
            },
        )
    } else {
        (
            PipelinePhase::CoderRunning,
            PipelineAction::SpawnCoder {
                attempt: 0,
                feedback: None,
            },
        )
    };
    state.history.push(PipelineTransition {
        from: PipelinePhase::Init,
        event: "init".to_string(),
        to: next_phase,
        action: action.label().to_string(),
        note: None,
    });
    state.phase = next_phase;
    (state, action)
}

/// coder 실패(비정상 종료·스톨)와 verdict rework/fail의 공통 재작업 분기.
/// `attempt < max_reworks`면 attempt를 소비하고 `rework_action`을, 초과면 에스컬레이션.
fn rework_or_escalate(
    state: &mut PipelineState,
    from: PipelinePhase,
    event: &PipelineEvent,
    rework_action: impl Fn(u32) -> PipelineAction,
    exhausted_reason: String,
) -> PipelineAction {
    if state.attempt < state.max_reworks {
        state.attempt += 1;
        let action = rework_action(state.attempt);
        state.phase = PipelinePhase::ReworkPending;
        state.history.push(PipelineTransition {
            from,
            event: event_label(event).to_string(),
            to: PipelinePhase::ReworkPending,
            action: action.label().to_string(),
            note: None,
        });
        action
    } else {
        escalate(state, from, event, exhausted_reason)
    }
}

/// 순수 전이 함수 — 상태 + 이벤트 → (갱신 상태, 다음 결정적 액션). LLM 호출 0.
///
/// 전이 규칙:
/// 1. `CoderExited{0}` (CoderRunning|ReworkPending) → 이종 모델 검사 → VerifierRunning.
///    동종 계열이면 fail-closed 에스컬레이션.
/// 2. `CoderExited{!=0|stalled}` → attempt < max_reworks면 재스폰, 초과면 에스컬레이션.
/// 3. `VerdictReceived{Pass}` → AdvisorG3Running.
/// 4. `VerdictReceived{Rework|Fail}` → 재작업/에스컬레이션(2와 같은 예산).
/// 5. `VerdictReceived{Human}` → 즉시 에스컬레이션.
/// 6. `AdvisorG3Result{pass, pr_number}` → ReadyPr + EmitReadyPr.
/// 7. `AdvisorG3Result{fail}` 또는 pass인데 pr_number 없음 → 에스컬레이션.
/// - 위상-이벤트 불일치(프로토콜 위반)는 전부 fail-closed 에스컬레이션.
/// - 종결 위상(ReadyPr|Escalated|Failed) 이벤트는 무시 + Action::None(이력만 기록).
pub fn advance_pipeline(
    mut state: PipelineState,
    event: PipelineEvent,
) -> (PipelineState, PipelineAction) {
    let from = state.phase;

    // 종결 위상 — 전이 없음. 이력에는 무시된 이벤트를 남겨 감사를 보존한다.
    if from.is_terminal() {
        state.history.push(PipelineTransition {
            from,
            event: event_label(&event).to_string(),
            to: from,
            action: PipelineAction::None.label().to_string(),
            note: Some(format!(
                "terminal phase {} — event ignored",
                from.as_str()
            )),
        });
        return (state, PipelineAction::None);
    }

    let action: PipelineAction = match (from, &event) {
        // --- 사전 경량 explore 종료 (fail-open: 실패/스톨해도 코더로 직행) ---
        (
            PipelinePhase::ExploreRunning,
            PipelineEvent::ExploreExited {
                exit_code,
                context_map_path,
                stalled,
            },
        ) => {
            let note = if *exit_code == 0 && !*stalled {
                if let Some(p) = context_map_path.as_ref().filter(|s| !s.trim().is_empty()) {
                    state.context_map_path = Some(p.clone());
                    Some(format!("context_map={p}"))
                } else {
                    Some("explore ok without context map".to_string())
                }
            } else if *stalled {
                Some("explore scout stalled — fail-open to coder".to_string())
            } else {
                Some(format!("explore scout exit {exit_code} — fail-open to coder"))
            };
            let action = PipelineAction::SpawnCoder {
                attempt: state.attempt,
                feedback: None,
            };
            state.phase = PipelinePhase::CoderRunning;
            state.history.push(PipelineTransition {
                from,
                event: event_label(&event).to_string(),
                to: PipelinePhase::CoderRunning,
                action: action.label().to_string(),
                note,
            });
            action
        }

        // --- coder 종료 ---
        (
            PipelinePhase::CoderRunning | PipelinePhase::ReworkPending,
            PipelineEvent::CoderExited {
                exit_code: 0,
                head_sha,
                stalled: false,
            },
        ) => {
            if let Some(h) = head_sha {
                state.head_sha = Some(h.clone());
            }
            // 이종 모델 불변식 — 같은 계열이면 검증 독립성이 깨지므로 fail-closed.
            if normalize_model_family(Some(&state.coder_model))
                == normalize_model_family(Some(&state.verifier_model))
            {
                let reason = format!(
                    "coder/verifier 동종 모델 계열 — 이종 불변식 위반 (coder={:?}, verifier={:?})",
                    state.coder_model, state.verifier_model
                );
                escalate(&mut state, from, &event, reason)
            } else {
                let action = PipelineAction::SpawnVerifier {
                    fork_id: state.fork_id.clone(),
                    head_sha: state.head_sha.clone(),
                    verifier_model: state.verifier_model.clone(),
                };
                state.phase = PipelinePhase::VerifierRunning;
                state.history.push(PipelineTransition {
                    from,
                    event: event_label(&event).to_string(),
                    to: PipelinePhase::VerifierRunning,
                    action: action.label().to_string(),
                    note: None,
                });
                action
            }
        }
        (
            PipelinePhase::CoderRunning | PipelinePhase::ReworkPending,
            PipelineEvent::CoderExited {
                exit_code,
                head_sha,
                stalled,
            },
        ) => {
            if let Some(h) = head_sha {
                state.head_sha = Some(h.clone());
            }
            let feedback = if *stalled {
                "coder stalled (no output)".to_string()
            } else {
                format!("coder exited with code {exit_code}")
            };
            let exhausted = format!(
                "coder 실패 재시도 소진 (attempt {} >= max_reworks {})",
                state.attempt, state.max_reworks
            );
            rework_or_escalate(
                &mut state,
                from,
                &event,
                |attempt| PipelineAction::SpawnCoder {
                    attempt,
                    feedback: Some(feedback.clone()),
                },
                exhausted,
            )
        }

        // --- verdict 수신 ---
        (
            PipelinePhase::VerifierRunning,
            PipelineEvent::VerdictReceived {
                decision,
                verdict_path,
            },
        ) => {
            state.last_verdict_path = Some(verdict_path.clone());
            match decision {
                VerdictDecision::Pass => {
                    let action = PipelineAction::SpawnAdvisorG3 {
                        fork_id: state.fork_id.clone(),
                        head_sha: state.head_sha.clone(),
                        verdict_path: verdict_path.clone(),
                    };
                    state.phase = PipelinePhase::AdvisorG3Running;
                    state.history.push(PipelineTransition {
                        from,
                        event: event_label(&event).to_string(),
                        to: PipelinePhase::AdvisorG3Running,
                        action: action.label().to_string(),
                        note: None,
                    });
                    action
                }
                VerdictDecision::Rework | VerdictDecision::Fail => {
                    let fork_id = state.fork_id.clone();
                    let vp = verdict_path.clone();
                    let exhausted = format!(
                        "verdict={} 재작업 소진 (attempt {} >= max_reworks {})",
                        decision.as_str(),
                        state.attempt,
                        state.max_reworks
                    );
                    rework_or_escalate(
                        &mut state,
                        from,
                        &event,
                        move |attempt| PipelineAction::BackpropAndRespawnCoder {
                            fork_id: fork_id.clone(),
                            verdict_path: vp.clone(),
                            attempt,
                        },
                        exhausted,
                    )
                }
                VerdictDecision::Human => escalate(
                    &mut state,
                    from,
                    &event,
                    "verdict=human — 검증자가 사람 판단 요청".to_string(),
                ),
            }
        }

        // --- advisor-g3 결과 ---
        (
            PipelinePhase::AdvisorG3Running,
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number,
                ..
            },
        ) => match pr_number {
            Some(n) => {
                state.pr_number = Some(*n);
                let action = PipelineAction::EmitReadyPr {
                    pr_number: *n,
                    head_sha: state.head_sha.clone(),
                };
                state.phase = PipelinePhase::ReadyPr;
                state.history.push(PipelineTransition {
                    from,
                    event: event_label(&event).to_string(),
                    to: PipelinePhase::ReadyPr,
                    action: action.label().to_string(),
                    note: None,
                });
                action
            }
            None => escalate(
                &mut state,
                from,
                &event,
                "advisor-g3 pass인데 pr_number 없음 — 프로토콜 위반".to_string(),
            ),
        },
        (
            PipelinePhase::AdvisorG3Running,
            PipelineEvent::AdvisorG3Result {
                passed: false,
                reason,
                ..
            },
        ) => escalate(
            &mut state,
            from,
            &event,
            format!(
                "advisor-g3 fail: {}",
                reason.clone().unwrap_or_else(|| "(no reason)".to_string())
            ),
        ),

        // --- 위상-이벤트 불일치 — fail-closed ---
        _ => escalate(
            &mut state,
            from,
            &event,
            format!(
                "unexpected event {} in phase {}",
                event_label(&event),
                from.as_str()
            ),
        ),
    };

    (state, action)
}

// ---------- 영역 스코프 가드 ----------

/// 미니 glob → regex 변환. 지원: `**`(디렉터리 무관 임의 깊이), `*`(한 세그먼트),
/// `?`(한 글자). 그 외 문자는 리터럴로 이스케이프한다.
fn glob_to_regex(pattern: &str) -> Option<regex::Regex> {
    let mut re = String::from("^");
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        match chars[i] {
            '*' => {
                if chars.get(i + 1) == Some(&'*') {
                    // `**/`는 "0개 이상 디렉터리"로 접두 제거 허용, 단독 `**`는 `.*`.
                    if chars.get(i + 2) == Some(&'/') {
                        re.push_str("(?:.*/)?");
                        i += 3;
                    } else {
                        re.push_str(".*");
                        i += 2;
                    }
                } else {
                    re.push_str("[^/]*");
                    i += 1;
                }
            }
            '?' => {
                re.push_str("[^/]");
                i += 1;
            }
            c => {
                re.push_str(&regex::escape(&c.to_string()));
                i += 1;
            }
        }
    }
    re.push('$');
    regex::Regex::new(&re).ok()
}

/// 스코프 매칭 — 패턴 자체 또는 `pattern/**`(하위 전체)에 걸리면 도메인 안.
/// `crates/*` 스코프는 `crates/dgate-core` 뿐 아니라 그 아래 모든 파일도 포함한다.
fn path_in_scope(scope_glob: &str, path: &str) -> bool {
    let matched = glob_to_regex(scope_glob).map(|r| r.is_match(path)).unwrap_or(false);
    if matched {
        return true;
    }
    glob_to_regex(&format!("{scope_glob}/**"))
        .map(|r| r.is_match(path))
        .unwrap_or(false)
}

/// `--scope "<glob>"` 영역 가드 — 대상 경로 중 도메인 밖 파일을 경고 목록으로 반환한다.
/// 반환 항목은 사람이 읽을 수 있는 경고 문장(JSON `scope_warnings` 그대로 사용).
/// 비-glob 문자는 리터럴로 이스케이프되므로 패턴 자체의 컴파일 실패는 사실상 없다.
pub fn check_scope_glob(scope_glob: &str, target_paths: &[&str]) -> Vec<String> {
    let mut warnings = Vec::new();
    for raw in target_paths {
        // `./crates/x` 같은 앞 경로 접두 정규화 — 상대 표기 차이로 오경고하지 않는다.
        let path = raw.strip_prefix("./").unwrap_or(raw);
        if !path_in_scope(scope_glob, path) {
            warnings.push(format!(
                "scope {scope_glob:?} 도메인 밖 경로: {path}"
            ));
        }
    }
    warnings
}

// ---------- 상태 파일 영속화 ----------

/// run_id를 파일명 안전 문자로 변환(경로 구분자만 치환 — fork/run id는 이미 안전 문자 위주).
fn sanitize_run_id(run_id: &str) -> String {
    run_id
        .chars()
        .map(|c| if c == '/' || c == '\\' || c == '\0' { '_' } else { c })
        .collect()
}

/// `<state_dir>/<run_id>.json` 경로.
pub fn state_file_path(state_dir: &Path, run_id: &str) -> PathBuf {
    state_dir.join(format!("{}.json", sanitize_run_id(run_id)))
}

/// 상태 저장 — tmp 파일에 쓴 뒤 rename으로 원자적 교체(부분 파일 잔류 차단).
/// 디렉터리는 필요 시 생성한다.
pub fn save_state(state_dir: &Path, state: &PipelineState) -> Result<PathBuf, String> {
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("state dir 생성 실패 {}: {e}", state_dir.display()))?;
    let path = state_file_path(state_dir, &state.run_id);
    let body = serde_json::to_string_pretty(state)
        .map_err(|e| format!("pipeline state 직렬화 실패: {e}"))?;
    let tmp = state_dir.join(format!(".{}.json.tmp", sanitize_run_id(&state.run_id)));
    std::fs::write(&tmp, body)
        .map_err(|e| format!("state 파일 쓰기 실패 {}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path)
        .map_err(|e| format!("state 파일 교체 실패 {}: {e}", path.display()))?;
    Ok(path)
}

/// 상태 로드 — 파일 부재·파싱 실패·schema_version 불일치는 전부 오류.
pub fn load_state(state_dir: &Path, run_id: &str) -> Result<PipelineState, String> {
    let path = state_file_path(state_dir, run_id);
    let raw = std::fs::read_to_string(&path)
        .map_err(|e| format!("pipeline state 읽기 실패 {}: {e}", path.display()))?;
    let state: PipelineState = serde_json::from_str(&raw)
        .map_err(|e| format!("pipeline state JSON 파싱 실패 {}: {e}", path.display()))?;
    if state.schema_version != PIPELINE_SCHEMA_VERSION {
        return Err(format!(
            "pipeline state schema_version 불일치: {} (지원 {})",
            state.schema_version, PIPELINE_SCHEMA_VERSION
        ));
    }
    Ok(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() -> PipelineInit {
        PipelineInit {
            fork_id: "fk_t1".to_string(),
            run_id: None,
            brief_path: ".herdr/briefs/t1.md".to_string(),
            worktree_path: Some("/tmp/wt-t1".to_string()),
            branch: "feat/t1".to_string(),
            scope_glob: None,
            max_reworks: DEFAULT_MAX_REWORKS,
            coder_model: "codex-gpt5".to_string(),
            verifier_model: "glm-5.3".to_string(),
            with_explore: false,
            explore_model: None,
        }
    }

    fn coder_ok(sha: &str) -> PipelineEvent {
        PipelineEvent::CoderExited {
            exit_code: 0,
            head_sha: Some(sha.to_string()),
            stalled: false,
        }
    }

    fn verdict(d: VerdictDecision) -> PipelineEvent {
        PipelineEvent::VerdictReceived {
            decision: d,
            verdict_path: ".herdr/verify/fk_t1-verdict.json".to_string(),
        }
    }

    // 1) 정상 경로 — Init → coder_ok → pass → g3 pass → ReadyPr + READY_PR 신호
    #[test]
    fn happy_path_ready_pr() {
        let (state, action) = init_pipeline(&init());
        assert_eq!(state.phase, PipelinePhase::CoderRunning);
        assert!(matches!(action, PipelineAction::SpawnCoder { attempt: 0, .. }));

        let (state, action) = advance_pipeline(state, coder_ok("abc1234"));
        assert_eq!(state.phase, PipelinePhase::VerifierRunning);
        assert_eq!(state.head_sha.as_deref(), Some("abc1234"));
        assert!(matches!(
            action,
            PipelineAction::SpawnVerifier { ref fork_id, ref verifier_model, .. }
                if fork_id == "fk_t1" && verifier_model == "glm-5.3"
        ));

        let (state, action) = advance_pipeline(state, verdict(VerdictDecision::Pass));
        assert_eq!(state.phase, PipelinePhase::AdvisorG3Running);
        assert!(matches!(action, PipelineAction::SpawnAdvisorG3 { .. }));

        let (state, action) = advance_pipeline(
            state,
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number: Some(42),
                reason: None,
            },
        );
        assert_eq!(state.phase, PipelinePhase::ReadyPr);
        assert_eq!(state.pr_number, Some(42));
        let (pr_number, head_sha) = match action {
            PipelineAction::EmitReadyPr { pr_number, head_sha } => (pr_number, head_sha),
            other => panic!("expected EmitReadyPr, got {other:?}"),
        };
        let signal = format!(
            "READY_PR={} HEAD={} G3=PASS",
            pr_number,
            head_sha.as_deref().unwrap_or("unknown")
        );
        assert_eq!(signal, "READY_PR=42 HEAD=abc1234 G3=PASS");
        // 종결 후 이벤트는 무시
        let (state2, action2) = advance_pipeline(state.clone(), coder_ok("zzz"));
        assert_eq!(state2.phase, PipelinePhase::ReadyPr);
        assert_eq!(action2, PipelineAction::None);
        assert_eq!(state2.history.len(), state.history.len() + 1);
    }

    // 2) 재작업 루프 — verdict rework 1~2회 → BackpropAndRespawnCoder, 소진 시 에스컬레이션
    #[test]
    fn rework_loop_then_escalate() {
        let (mut state, _) = init_pipeline(&init());
        state = advance_pipeline(state, coder_ok("s1")).0;
        // rework 1회
        let (s, a) = advance_pipeline(state, verdict(VerdictDecision::Rework));
        assert_eq!(s.phase, PipelinePhase::ReworkPending);
        assert_eq!(s.attempt, 1);
        assert!(matches!(
            a,
            PipelineAction::BackpropAndRespawnCoder { attempt: 1, .. }
        ));
        // 재스폰 coder exit 0 → VerifierRunning (ReworkPending에서도 coder 종료 수신)
        let (s, a) = advance_pipeline(s, coder_ok("s2"));
        assert_eq!(s.phase, PipelinePhase::VerifierRunning);
        assert!(matches!(a, PipelineAction::SpawnVerifier { .. }));
        // rework 2회
        let (s, a) = advance_pipeline(s, verdict(VerdictDecision::Fail));
        assert_eq!(s.phase, PipelinePhase::ReworkPending);
        assert_eq!(s.attempt, 2);
        assert!(matches!(
            a,
            PipelineAction::BackpropAndRespawnCoder { attempt: 2, .. }
        ));
        // 3번째 실패 — 예산 소진 → Escalated + WakeSubleadEscalation
        let (s, a) = advance_pipeline(s, coder_ok("s3"));
        let (s, a2) = advance_pipeline(s, verdict(VerdictDecision::Rework));
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert_eq!(s.attempt, 2, "소진 후 attempt 증가 없음");
        assert!(s.escalation_reason.is_some());
        assert!(matches!(a2, PipelineAction::WakeSubleadEscalation { .. }));
        let _ = a;
    }

    // 2b) coder 종료 실패도 같은 재작업 예산을 소비한다
    #[test]
    fn coder_fail_rework_then_escalate() {
        let (state, _) = init_pipeline(&init());
        let (s, a) = advance_pipeline(
            state,
            PipelineEvent::CoderExited {
                exit_code: 1,
                head_sha: None,
                stalled: false,
            },
        );
        assert_eq!(s.phase, PipelinePhase::ReworkPending);
        assert_eq!(s.attempt, 1);
        assert!(matches!(
            a,
            PipelineAction::SpawnCoder { attempt: 1, feedback: Some(_) }
        ));
        // 스톨도 실패 취급
        let (s, _) = advance_pipeline(
            s,
            PipelineEvent::CoderExited {
                exit_code: 0,
                head_sha: None,
                stalled: true,
            },
        );
        assert_eq!(s.phase, PipelinePhase::ReworkPending);
        assert_eq!(s.attempt, 2);
        let (s, a) = advance_pipeline(
            s,
            PipelineEvent::CoderExited {
                exit_code: 137,
                head_sha: None,
                stalled: false,
            },
        );
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(matches!(a, PipelineAction::WakeSubleadEscalation { .. }));
    }

    // 3) 동종 coder/verifier 모델 — 이종 불변식 위반 → fail-closed 에스컬레이션
    #[test]
    fn same_family_models_fail_closed() {
        let mut i = init();
        i.verifier_model = "gpt-5-codex".to_string(); // coder(codex-gpt5 → gpt)와 동종
        let (state, _) = init_pipeline(&i);
        let (s, a) = advance_pipeline(state, coder_ok("s1"));
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(matches!(a, PipelineAction::WakeSubleadEscalation { .. }));
        assert!(s
            .escalation_reason
            .as_deref()
            .unwrap_or("")
            .contains("이종 불변식"));
        // 미지정 모델 두 개(둘 다 None 계열)도 동등 → fail-closed
        let mut i2 = init();
        i2.coder_model = String::new();
        i2.verifier_model = String::new();
        let (state, _) = init_pipeline(&i2);
        let (s, _) = advance_pipeline(state, coder_ok("s1"));
        assert_eq!(s.phase, PipelinePhase::Escalated);
    }

    // 4) human verdict / g3 fail → 즉시 에스컬레이션
    #[test]
    fn human_verdict_and_g3_fail_escalate() {
        let (mut state, _) = init_pipeline(&init());
        state = advance_pipeline(state, coder_ok("s1")).0;
        let (s, a) = advance_pipeline(state, verdict(VerdictDecision::Human));
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(matches!(a, PipelineAction::WakeSubleadEscalation { .. }));

        // g3 pass인데 pr_number 없음 → 프로토콜 위반 에스컬레이션
        let (mut state, _) = init_pipeline(&init());
        state = advance_pipeline(state, coder_ok("s1")).0;
        state = advance_pipeline(state, verdict(VerdictDecision::Pass)).0;
        let (s, a) = advance_pipeline(
            state,
            PipelineEvent::AdvisorG3Result {
                passed: true,
                pr_number: None,
                reason: None,
            },
        );
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(matches!(a, PipelineAction::WakeSubleadEscalation { .. }));

        // g3 fail → 에스컬레이션 + 사유
        let (mut state, _) = init_pipeline(&init());
        state = advance_pipeline(state, coder_ok("s1")).0;
        state = advance_pipeline(state, verdict(VerdictDecision::Pass)).0;
        let (s, a) = advance_pipeline(
            state,
            PipelineEvent::AdvisorG3Result {
                passed: false,
                pr_number: None,
                reason: Some("P0 conflict".to_string()),
            },
        );
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(matches!(
            a,
            PipelineAction::WakeSubleadEscalation { ref reason } if reason.contains("P0 conflict")
        ));
    }

    // 4b) 위상-이벤트 불일치 → fail-closed 에스컬레이션
    #[test]
    fn unexpected_event_escalates() {
        let (state, _) = init_pipeline(&init());
        // CoderRunning에서 verdict 수신 — 순서 위반
        let (s, a) = advance_pipeline(state, verdict(VerdictDecision::Pass));
        assert_eq!(s.phase, PipelinePhase::Escalated);
        assert!(s
            .escalation_reason
            .as_deref()
            .unwrap_or("")
            .contains("unexpected event"));
        let _ = a;
    }

    // 5) 스코프 가드 — 도메인 내 통과·도메인 외 경고
    #[test]
    fn scope_glob_check() {
        // `crates/*` 스코프는 하위 재귀도 도메인 안
        let warns = check_scope_glob(
            "crates/*",
            &[
                "crates/dgate-core/src/pipeline.rs",
                "crates/dgate-cli/src/lib.rs",
                "server/src/app.ts",
                "./crates/dgate-core/src/verdict.rs",
            ],
        );
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("server/src/app.ts"));

        // 세그먼트 한정 패턴
        let warns = check_scope_glob("src/*.ts", &["src/app.ts", "src/deep/x.ts"]);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("src/deep/x.ts"));

        // `**` 패턴
        assert!(check_scope_glob("**/*.rs", &["a/b/c.rs"]).is_empty());
        assert_eq!(
            check_scope_glob("**/*.rs", &["a/b/c.ts"]).len(),
            1
        );

        // 비-glob 문자는 리터럴 — 매칭 안 되면 도메인 밖 경고
        let warns = check_scope_glob("[unclosed", &["anything.rs"]);
        assert_eq!(warns.len(), 1);
        assert!(warns[0].contains("anything.rs"));
    }

    // 6) 상태 파일 저장·회독 라운드트립
    #[test]
    fn state_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dg-pipe-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (mut state, _) = init_pipeline(&init());
        state = advance_pipeline(state, coder_ok("deadbee")).0;
        let path = save_state(&dir, &state).unwrap();
        assert_eq!(path, dir.join("fk_t1.json"));
        let loaded = load_state(&dir, "fk_t1").unwrap();
        assert_eq!(loaded, state);
        // 미존재 run_id → 오류
        assert!(load_state(&dir, "fk_none").is_err());
        // schema_version 불일치 → 오류
        let mut bad = state.clone();
        bad.schema_version = 99;
        std::fs::write(&path, serde_json::to_string(&bad).unwrap()).unwrap();
        assert!(load_state(&dir, "fk_t1").is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    // 7) Pre-Coder 경량 explore 단계 — 성공 시 context_map 주입, 실패 시 fail-open 코더 진행
    #[test]
    fn explore_phase_injects_context_map_and_fails_open() {
        let mut cfg = init();
        cfg.with_explore = true;
        cfg.explore_model = Some("deepseek-v4.1-flash".to_string());
        let (state, action) = init_pipeline(&cfg);
        assert_eq!(state.phase, PipelinePhase::ExploreRunning);
        assert!(matches!(
            action,
            PipelineAction::SpawnExplore {
                ref context_map_path,
                ..
            } if context_map_path == ".herdr/context/fk_t1-map.md"
        ));

        // 성공 → context_map_path 저장 + CoderRunning 전이
        let (next, next_action) = advance_pipeline(
            state.clone(),
            PipelineEvent::ExploreExited {
                exit_code: 0,
                context_map_path: Some(".herdr/context/fk_t1-map.md".to_string()),
                stalled: false,
            },
        );
        assert_eq!(next.phase, PipelinePhase::CoderRunning);
        assert_eq!(
            next.context_map_path.as_deref(),
            Some(".herdr/context/fk_t1-map.md")
        );
        assert!(matches!(next_action, PipelineAction::SpawnCoder { attempt: 0, .. }));

        // 실패/스톨 → 에스컬레이션 없이 fail-open으로 CoderRunning 전이
        let (fallback, fb_action) = advance_pipeline(
            state,
            PipelineEvent::ExploreExited {
                exit_code: 124,
                context_map_path: None,
                stalled: true,
            },
        );
        assert_eq!(fallback.phase, PipelinePhase::CoderRunning);
        assert_eq!(fallback.context_map_path, None);
        assert!(matches!(fb_action, PipelineAction::SpawnCoder { attempt: 0, .. }));
    }

    // 8) 브리프 코드 앵커 판별 — 파일:줄번호 / Context Pack / 소스 파일 2건+
    #[test]
    fn brief_has_code_anchors_distinguishes_high_level_vs_anchored() {
        let vague = "## Standards\n깔끔하게 구현\n## Spec\n홈 검색 UX를 개선한다.\n## Merge Danger\ndoor: two-way\nblast_radius: 홈 검색 화면\n";
        assert!(!brief_has_code_anchors(vague));

        let with_line_anchor = "## Spec\n`HomeCatalogView.vue:120-185` 의 검색 디바운스를 수정한다.\n";
        assert!(brief_has_code_anchors(with_line_anchor));

        let with_context_pack = "## Context Pack\n- 핵심 함수 위치 요약\n";
        assert!(brief_has_code_anchors(with_context_pack));

        let with_two_paths = "## Spec\n수정 대상: `src/views/HomeCatalogView.vue`, `src/composables/useSearch.ts`\n";
        assert!(brief_has_code_anchors(with_two_paths));

        // `.herdr/` 메타 파일 경로는 코드 앵커로 치지 않는다
        let herdr_only = "## Spec\n브리프 `.herdr/briefs/a.md`와 `.herdr/verify/a.json` 참고\n";
        assert!(!brief_has_code_anchors(herdr_only));
    }
}
