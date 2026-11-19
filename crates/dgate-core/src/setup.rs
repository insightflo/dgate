//! 환경 감지 온보딩 위저드 & 읽기전용 진단 도구 — `scripts/setup.ts` (brief-setup-wizard·
//! setup-v2·v3·v4)의 Rust 포팅.
//!
//! 목적: 첫 사용자 머신(macOS/Linux)의 런타임·코딩 CLI·에이전트 스킬 디렉터리·선택
//! 도구를 자동 감지해, 실제 설치 조합에 맞는 roster 프로필을 결정적으로 추천한다.
//! v2: 설치 CLI의 로컬 설정·카탈로그에서 사용 가능 모델을 발견(0 네트워크/0 토큰)해
//! 역할별 적합 가이드와 함께 보여준다. v3: 동반 스킬(mistology·neverstuck·todo-flow·
//! effort-router)을 설치 여부·현재 환경과 비교해 개별 verdict로 추천한다. v4: 검증된
//! 공식 GitHub 주소(repoSlug·repoUrl) 노출 + opt-in 스타 응원(`star_github_repo`)을
//! 지원한다.
//!
//! 외부 연결:
//! - 생성 `roster.json`은 `roster` 모듈 `load_roster_config` 병합 규약을 따른다
//!   (빈 문자열 필드는 내장 quota-saver 기본 상속을 차단하기 위한 의도적 오버라이드).
//! - 훅 설치 = canonical `.agents/hooks.json` 복사 + `hooks/dg-hook.ts` 심볼릭 링크 +
//!   `.git/info/exclude` 추가.
//!
//! 주의:
//! - `GEMINI_DECISION_API_KEY`는 존재 여부만 보고 — 값은 절대 읽어 저장·출력하지 않는다.
//! - `agy`는 감지되지만 쿼터 한도로 어떤 추천에도 자동 선택되지 않는다(`agy: 0` 고정).
//! - 모든 헬퍼는 `DetectDeps`/homeDir 주입 가능 — 테스트가 실제 `$HOME`을 건드리지 않는다.
//! - `star_github_repo`는 사용자 명시적 동의가 있을 때만 호출되어야 하며(호출측 책임,
//!   opt-in 전용) argv 배열로만 실행한다 — 쉘 문자열 결합·쉘 실행은 절대 금지.
//! - Rust 포팅 적응: TS의 Node 버전·`node:sqlite` 프로브는 런타임 주입값이 있을 때만
//!   판정하고, 미주입 시 러스트 바이너리는 Node 불필요 → `ok = true`로 취급한다.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::roster::{
    RoleSlot, RoleWorkerSpec, RosterConfig, RosterProfile, WorkerCliKind, ROLE_SLOTS,
};

/// 오케스트레이터 저장소 루트 — 컴파일 시점 crate 위치 기준(훅 소스 기본값).
pub fn default_repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")))
}

pub const CLI_IDS: [&str; 8] = ["claude", "codex", "devin", "pi", "gemini", "agy", "hc", "cargo"];

/// 멀티CLI 워커로 인정되는 CLI — agy·hc·cargo는 워커 슬롯에 자동 배정되지 않는다.
pub const WORKER_CLI_IDS: [&str; 5] = ["claude", "codex", "devin", "pi", "gemini"];

pub const COMPANION_IDS: [&str; 6] = [
    "mistology",
    "todo-flow",
    "effort-router",
    "neverstuck",
    "hc",
    "gemini-decision-api-key",
];

pub type CompanionId = &'static str;
pub type CompanionSkillId = &'static str;

/// PATH 미검출 시 추가로 찾는 관례적 설치 위치 (homeDir 기준 상대경로 — 기계 독립).
const HOME_FALLBACK_BIN_DIRS: [&str; 3] = [".local/bin", ".bun/bin", ".cargo/bin"];
const DEFAULT_SYSTEM_BIN_DIRS: [&str; 4] = ["/opt/homebrew/bin", "/usr/local/bin", "/usr/bin", "/bin"];

/// (id, home 상대 디렉터리) — 에이전트 스킬 탐색 대상.
pub const AGENT_SKILL_DIRS: [(&str, &str); 6] = [
    ("claude", ".claude"),
    ("gemini", ".gemini"),
    ("codex", ".codex"),
    ("agents", ".agents"),
    ("pi", ".pi/agent"),
    ("cursor", ".cursor"),
];

/// todo-flow·effort-router·neverstuck 스킬을 찾는 에이전트 부모 디렉터리.
const SKILL_COMPANION_DIRS: [&str; 4] = [".agents", ".gemini", ".claude", ".codex"];
/// mistology 스킬을 찾는 에이전트 부모 디렉터리 (brief §2.5 — .agents/.gemini만).
const MISTOLOGY_SKILL_DIRS: [&str; 2] = [".agents", ".gemini"];

pub const MISTOLOGY_INSTALL_CMD: &str =
    "cargo install --git https://github.com/chldbwnstm/mistology --locked && mistology setup";
pub const MISTOLOGY_GUIDE_URL: &str = "https://github.com/chldbwnstm/mistology";
pub const DECISION_API_KEY_ENV: &str = "GEMINI_DECISION_API_KEY";

/// 본 저장소의 검증된 공식 GitHub 주소 (안내·opt-in 스타 전용).
pub const SELF_REPO_SLUG: &str = "insightflo/dgate";
pub const SELF_REPO_URL: &str = "https://github.com/insightflo/dgate";

/// 동반 스킬별 검증된 공식 GitHub 저장소 — 가짜·미검증 URL 발명 금지.
pub const COMPANION_REPOS: [(CompanionSkillId, &str, &str); 4] = [
    ("mistology", "chldbwnstm/mistology", "https://github.com/chldbwnstm/mistology"),
    ("neverstuck", "chldbwnstm/NeverStuck", "https://github.com/chldbwnstm/NeverStuck"),
    ("todo-flow", "JakeB-5/todo-flow", "https://github.com/JakeB-5/todo-flow"),
    ("effort-router", "acc0mplish/Effort-Router", "https://github.com/acc0mplish/Effort-Router"),
];

const GIT_EXCLUDE_ENTRIES: [&str; 2] = [".agents/hooks.json", ".agents/hooks/"];

// ---------------------------------------------------------------------------
// 타입
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct CliDetection {
    pub installed: bool,
    pub path: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AgentDirStatus {
    pub id: String,
    pub dir: String,
    pub exists: bool,
    /// `<dir>/skills/dgate/SKILL.md` 링크 여부
    pub skill_linked: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CompanionStatus {
    pub id: String,
    pub installed: bool,
    /// 원본 계약상 항상 false — 동반 도구는 필수가 아니다.
    pub required: bool,
    /// 1-line Korean role description
    pub role: String,
    /// 감지된 경로 또는 '미설치'
    pub detail: String,
    pub install_hint: Option<String>,
    pub guide_url: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanionVerdict {
    AlreadyInstalled,
    Recommended,
    Optional,
    SkipOverlap,
}

impl CompanionVerdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            CompanionVerdict::AlreadyInstalled => "already_installed",
            CompanionVerdict::Recommended => "recommended",
            CompanionVerdict::Optional => "optional",
            CompanionVerdict::SkipOverlap => "skip_overlap",
        }
    }
}

/// 동반 스킬 1개의 환경 비교 판정 — 무조건 전부 권하지 않고 실제 환경과 비교한다.
#[derive(Debug, Clone, PartialEq)]
pub struct CompanionAdvice {
    pub id: String,
    pub name: String,
    pub verdict: CompanionVerdict,
    /// 이 사용자 환경과 비교한 1줄 한국어 이유
    pub reason: String,
    /// 설치 후 1줄 한국어 사용법
    pub how_to_use: String,
    /// 위저드가 지금 자동 설치를 실행할 수 있는지 (mistology+cargo만 true)
    pub can_auto_install: bool,
    /// 실제 검증된 설치 명령 — 없으면 None (가짜 URL 발명 금지)
    pub install_command: Option<String>,
    /// 수동 설치 한국어 안내
    pub install_guide: String,
    /// 검증된 공식 GitHub 저장소 `owner/repo` (opt-in 스타·안내 전용)
    pub repo_slug: String,
    /// 검증된 공식 GitHub 저장소 URL
    pub repo_url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SecretsStatus {
    pub env_set: bool,
    pub secrets_file_exists: bool,
    pub configured: bool,
    pub path: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DbStatus {
    pub path: String,
    pub exists: bool,
    pub source: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RosterFileStatus {
    pub path: String,
    pub exists: bool,
    pub valid_json: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RepoHooksStatus {
    pub repo_dir: String,
    pub repo_dir_exists: bool,
    pub hooks_json: bool,
    pub hook_symlink: bool,
    pub excluded: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupMode {
    Standalone,
    MultiCli,
}

impl SetupMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            SetupMode::Standalone => "standalone",
            SetupMode::MultiCli => "multi-cli",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct RosterRecommendation {
    pub mode: SetupMode,
    pub profile_name: Option<String>,
    /// 표시용 역할→CLI 라벨 맵
    pub roles: HashMap<RoleSlot, String>,
    pub reason: String,
    /// 추천 agy 일일 턴 예산 — 항상 0 (agy는 사용자 명시 선택 시에만)
    pub agy: i64,
}

/// 발견 경로 — 100% 로컬 파일(설정·카탈로그).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelSource {
    LocalConfig,
    Catalog,
}

impl ModelSource {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelSource::LocalConfig => "local_config",
            ModelSource::Catalog => "catalog",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DiscoveredCli {
    Pi,
    Claude,
    Codex,
    Devin,
    Gemini,
}

impl DiscoveredCli {
    pub fn as_str(&self) -> &'static str {
        match self {
            DiscoveredCli::Pi => "pi",
            DiscoveredCli::Claude => "claude",
            DiscoveredCli::Codex => "codex",
            DiscoveredCli::Devin => "devin",
            DiscoveredCli::Gemini => "gemini",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelTier {
    Fast,
    Balanced,
    Strong,
    CodingAgent,
}

impl ModelTier {
    pub fn label(&self) -> &'static str {
        match self {
            ModelTier::Fast => "빠름·저비용",
            ModelTier::Balanced => "균형",
            ModelTier::Strong => "고성능·추론",
            ModelTier::CodingAgent => "코딩 에이전트",
        }
    }
}

/// 로컬 설정·카탈로그에서 발견된 사용 가능 모델 (100% 로컬 — 네트워크·토큰 비용 0).
#[derive(Debug, Clone, PartialEq)]
pub struct DiscoveredModel {
    pub cli: DiscoveredCli,
    pub provider: Option<String>,
    pub model: String,
    pub source: ModelSource,
    pub tier: ModelTier,
    pub tier_label: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct NodeStatus {
    pub version: String,
    pub major: u32,
    pub minor: u32,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnvironmentReport {
    pub ok: bool,
    pub node: NodeStatus,
    pub node_sqlite_ok: bool,
    pub git: CliDetection,
    pub clis: BTreeMap<String, CliDetection>,
    pub agents: Vec<AgentDirStatus>,
    pub roster: RosterFileStatus,
    pub db: DbStatus,
    pub secrets: SecretsStatus,
    pub companions: BTreeMap<String, CompanionStatus>,
    pub recommendation: RosterRecommendation,
    pub discovered_models: Vec<DiscoveredModel>,
    /// 동반 스킬 4종 환경 비교 추천 (`--check --json`에도 포함)
    pub companion_advice: Vec<CompanionAdvice>,
    pub repo_hooks: Option<RepoHooksStatus>,
}

/// env 주입형 의존성 — 미설정 시 실제 프로세스 env/홈을 읽는다(운용 경로).
#[derive(Debug, Clone, Default)]
pub struct DetectDeps {
    pub env: Option<BTreeMap<String, String>>,
    pub home_dir: Option<PathBuf>,
    /// 지정 시 PATH 대신 이 디렉터리 목록으로 바이너리를 찾는다 (테스트 주입용).
    pub path_dirs: Option<Vec<PathBuf>>,
    /// 시스템 관례 bin 디렉터리 (기본: homebrew/usr/local/usr/bin/bin).
    pub system_dirs: Option<Vec<PathBuf>>,
    /// Node 버전 문자열(러스트 바이너리는 불필요 — 주입 시에만 >= 22.6 판정).
    pub node_version: Option<String>,
    pub node_sqlite_ok: Option<bool>,
    /// `--repo` 대상 — 지정 시 hooks 설치 상태를 함께 진단한다.
    pub repo_dir: Option<PathBuf>,
    pub orchestrator_root: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// 파일시스템·환경 유틸
// ---------------------------------------------------------------------------

fn env_get(env: Option<&BTreeMap<String, String>>, key: &str) -> Option<String> {
    if let Some(map) = env {
        return map.get(key).cloned();
    }
    std::env::var(key).ok()
}

#[cfg(unix)]
fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(p) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(_p: &Path) -> bool {
    false
}

/// `which`와 동일한 의미론 — 검색 디렉터리를 순서대로 훑어 첫 실행 가능 파일을 반환.
pub fn find_on_path(name: &str, search_dirs: &[PathBuf]) -> Option<String> {
    let mut seen: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    for dir in search_dirs {
        if dir.as_os_str().is_empty() || !seen.insert(dir.clone()) {
            continue;
        }
        let cand = dir.join(name);
        if is_executable_file(&cand) {
            return Some(cand.to_string_lossy().into_owned());
        }
    }
    None
}

fn detect_bin(name: &str, search_dirs: &[PathBuf]) -> CliDetection {
    let found = find_on_path(name, search_dirs);
    CliDetection {
        installed: found.is_some(),
        path: found,
    }
}

fn split_path_list(v: Option<&str>) -> Vec<PathBuf> {
    v.unwrap_or("")
        .split(':')
        .filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .collect()
}

fn parse_node_version(v: &str) -> NodeStatus {
    let trimmed = v.trim();
    let (major, minor) = match re_node_version().captures(trimmed) {
        Some(c) => (
            c.get(1).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
            c.get(2).and_then(|m| m.as_str().parse().ok()).unwrap_or(0),
        ),
        None => (0, 0),
    };
    NodeStatus {
        version: v.to_string(),
        major,
        minor,
        ok: major > 22 || (major == 22 && minor >= 6),
    }
}

fn re_node_version() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^v?(\d+)\.(\d+)").unwrap())
}

fn skill_file_in(home_dir: &Path, skill: &str, parents: &[&str]) -> Option<String> {
    for parent in parents {
        let p = home_dir.join(parent).join("skills").join(skill).join("SKILL.md");
        if p.exists() {
            return Some(p.to_string_lossy().into_owned());
        }
    }
    None
}

/// JSON 파일을 읽어 객체로 반환 — 부재·깨짐·비객체는 None.
fn read_json_safe(p: &Path) -> Option<serde_json::Map<String, Value>> {
    let content = std::fs::read_to_string(p).ok()?;
    let parsed: Value = serde_json::from_str(&content).ok()?;
    parsed.as_object().cloned()
}

// ---------------------------------------------------------------------------
// Step 1: 환경 자동 감지
// ---------------------------------------------------------------------------

/// 환경 자동 감지 — 런타임·CLI·에이전트 디렉터리·설정 상태·추천까지 한 번에 조립.
/// 모든 입력은 `deps`로 주입 가능하며, 시크릿 값은 존재 여부만 판정한다(값 미저장).
pub fn detect_environment(deps: &DetectDeps) -> EnvironmentReport {
    let env = deps.env.as_ref();
    let home_dir = deps
        .home_dir
        .clone()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let orch_root = deps.orchestrator_root.clone().unwrap_or_else(default_repo_root);
    let path_dirs = deps
        .path_dirs
        .clone()
        .unwrap_or_else(|| split_path_list(env_get(env, "PATH").as_deref()));
    let system_dirs = deps
        .system_dirs
        .clone()
        .unwrap_or_else(|| DEFAULT_SYSTEM_BIN_DIRS.iter().map(PathBuf::from).collect());
    let mut search_dirs = path_dirs.clone();
    for d in HOME_FALLBACK_BIN_DIRS {
        search_dirs.push(home_dir.join(d));
    }
    search_dirs.extend(system_dirs);

    // Rust 바이너리는 Node 불필요 — 주입값이 있을 때만 버전 판정(적응 계약).
    let node = match &deps.node_version {
        Some(v) => parse_node_version(v),
        None => NodeStatus {
            version: "rust-native".to_string(),
            major: 0,
            minor: 0,
            ok: true,
        },
    };
    let node_sqlite_ok = deps.node_sqlite_ok.unwrap_or(true);
    let git = detect_bin("git", &search_dirs);

    let mut clis: BTreeMap<String, CliDetection> = BTreeMap::new();
    for id in CLI_IDS {
        clis.insert(id.to_string(), detect_bin(id, &search_dirs));
    }

    let xdg_config = env_get(env, "XDG_CONFIG_HOME")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| home_dir.join(".config").to_string_lossy().into_owned());
    let xdg_state = env_get(env, "XDG_STATE_HOME")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| home_dir.join(".local").join("state").to_string_lossy().into_owned());

    let agents: Vec<AgentDirStatus> = AGENT_SKILL_DIRS
        .iter()
        .map(|(id, dir)| {
            let dir_path = home_dir.join(dir);
            AgentDirStatus {
                id: id.to_string(),
                dir: dir_path.to_string_lossy().into_owned(),
                exists: dir_path.exists(),
                skill_linked: dir_path.join("skills").join("dgate").join("SKILL.md").exists(),
            }
        })
        .collect();

    let roster_path = PathBuf::from(&xdg_config).join("herdr-quad").join("roster.json");
    let roster_exists = roster_path.exists();
    let roster_valid_json = roster_exists
        && std::fs::read_to_string(&roster_path)
            .ok()
            .and_then(|c| serde_json::from_str::<Value>(&c).ok())
            .is_some();

    let dgate_db = env_get(env, "DGATE_DB_PATH")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let env_db = env_get(env, "DECISION_ORCHESTRATOR_DB_PATH")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let legacy_db = env_get(env, "GEMINI_TRICK_DB_PATH")
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let default_db = PathBuf::from(&xdg_state).join("dgate").join("decision.db");
    let db = if let Some(p) = dgate_db {
        DbStatus {
            path: p.clone(),
            exists: Path::new(&p).exists(),
            source: "DGATE_DB_PATH",
        }
    } else if let Some(p) = env_db {
        DbStatus {
            path: p.clone(),
            exists: Path::new(&p).exists(),
            source: "DECISION_ORCHESTRATOR_DB_PATH",
        }
    } else if let Some(p) = legacy_db {
        DbStatus {
            path: p.clone(),
            exists: Path::new(&p).exists(),
            source: "GEMINI_TRICK_DB_PATH",
        }
    } else {
        DbStatus {
            path: default_db.to_string_lossy().into_owned(),
            exists: default_db.exists(),
            source: "default",
        }
    };

    let secrets_path = PathBuf::from(&xdg_config).join("herdr-quad").join("secrets.env");
    // SECURITY: 키 값은 읽지 않는다 — 존재·비공백 여부만.
    let env_set = env_get(env, DECISION_API_KEY_ENV)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    let secrets_file_exists = secrets_path.exists();
    let secrets = SecretsStatus {
        env_set,
        secrets_file_exists,
        configured: env_set || secrets_file_exists,
        path: secrets_path.to_string_lossy().into_owned(),
    };

    let mistology_bin = find_on_path("mistology", &search_dirs);
    let mistology_skill = skill_file_in(&home_dir, "mistology", &MISTOLOGY_SKILL_DIRS);
    let mut companions: BTreeMap<String, CompanionStatus> = BTreeMap::new();
    companions.insert(
        "mistology".to_string(),
        CompanionStatus {
            id: "mistology".to_string(),
            installed: mistology_bin.is_some() || mistology_skill.is_some(),
            required: false,
            role: "저장소별 과거 코드 실수·2-hop 연관 모듈 브리핑 (Rust CLI + 스킬)".to_string(),
            detail: mistology_bin
                .or(mistology_skill)
                .unwrap_or_else(|| "미설치".to_string()),
            install_hint: Some(MISTOLOGY_INSTALL_CMD.to_string()),
            guide_url: Some(MISTOLOGY_GUIDE_URL.to_string()),
        },
    );

    let skill_companions: [(&str, &str, &str, &str); 3] = [
        (
            "todo-flow",
            "todo",
            "요구사항 조사·할 일 트랙·완료조건(Acceptance) 문서 관리 스킬",
            "각 에이전트의 skills/todo/ 에 todo 스킬 폴더를 배치하거나 심볼릭 링크",
        ),
        (
            "effort-router",
            "effort-router",
            "작업 티어(S/M/L/XL) 판정 및 단일 세션 단계·증거번들 계획 스킬",
            "각 에이전트의 skills/effort-router/ 에 스킬 폴더를 배치하거나 심볼릭 링크",
        ),
        (
            "neverstuck",
            "neverstuck",
            "같은 오류 3회+ 반복 시 원인 진단·프롬프트 재설계 탈출 스킬",
            "각 에이전트의 skills/neverstuck/ 에 스킬 폴더를 배치하거나 심볼릭 링크",
        ),
    ];
    for (id, skill, role, hint) in skill_companions {
        let found = skill_file_in(&home_dir, skill, &SKILL_COMPANION_DIRS);
        companions.insert(
            id.to_string(),
            CompanionStatus {
                id: id.to_string(),
                installed: found.is_some(),
                required: false,
                role: role.to_string(),
                detail: found.unwrap_or_else(|| "미설치".to_string()),
                install_hint: Some(hint.to_string()),
                guide_url: None,
            },
        );
    }
    let hc_cli = &clis["hc"];
    companions.insert(
        "hc".to_string(),
        CompanionStatus {
            id: "hc".to_string(),
            installed: hc_cli.installed,
            required: false,
            role: "여러 코딩 CLI 워커를 백그라운드 터미널로 병렬 배차·제어하는 런처".to_string(),
            detail: hc_cli
                .path
                .clone()
                .unwrap_or_else(|| "미설치".to_string()),
            install_hint: Some(
                "Herdr 런처(hc) 설치 — 배포 문서의 설치 절차를 따른 뒤 `hc run`/`hc send`로 워커 배차"
                    .to_string(),
            ),
            guide_url: None,
        },
    );
    companions.insert(
        "gemini-decision-api-key".to_string(),
        CompanionStatus {
            id: "gemini-decision-api-key".to_string(),
            installed: secrets.configured,
            required: false,
            role: "선택적 `gemini-decision-1` Shadow/On 확률 보정 API (미설정 시 로컬 무료 통계로 동작)"
                .to_string(),
            detail: if env_set {
                "환경변수 설정됨".to_string()
            } else if secrets_file_exists {
                "secrets.env 존재".to_string()
            } else {
                "미설정 (기본 off — 완전 무료 로컬 통계)".to_string()
            },
            install_hint: Some(format!(
                "~/.config/herdr-quad/secrets.env 에 {} 저장 후 chmod 600",
                DECISION_API_KEY_ENV
            )),
            guide_url: None,
        },
    );

    let recommendation = recommend_roster_profile(&clis);
    let discovered_models = discover_available_models(&home_dir, &clis);
    let mut report = EnvironmentReport {
        ok: node.ok && node_sqlite_ok && git.installed,
        node,
        node_sqlite_ok,
        git,
        clis,
        agents,
        roster: RosterFileStatus {
            path: roster_path.to_string_lossy().into_owned(),
            exists: roster_exists,
            valid_json: roster_valid_json,
        },
        db,
        secrets,
        companions,
        recommendation,
        discovered_models,
        companion_advice: Vec::new(),
        repo_hooks: None,
    };
    report.companion_advice = recommend_companion_tools(&report);
    if let Some(repo_dir) = &deps.repo_dir {
        report.repo_hooks = Some(inspect_repo_hooks(repo_dir, &orch_root));
    }
    report
}

// ---------------------------------------------------------------------------
// Step 2: 맞춤 roster 추천·생성
// ---------------------------------------------------------------------------

const STANDALONE_REASON: &str =
    "단독 모드 — 추가 CLI 없이 `dg route`/`dg wrap`/`dg stats`/`dg report` + TUI 바로 사용";

/// mergeSpec은 null만 스킵하므로, 파일 프로필이 내장 quota-saver 위에 병합될 때
/// pi 전용 필드(model·provider·thinking)가 비-pi 스펙으로 새어 나가는 것을
/// 빈 문자열 오버라이드로 차단한다.
fn block_inherit(mut spec: RoleWorkerSpec) -> RoleWorkerSpec {
    if spec.model.is_none() {
        spec.model = Some(String::new());
    }
    if spec.provider.is_none() {
        spec.provider = Some(String::new());
    }
    if spec.thinking.is_none() {
        spec.thinking = Some(String::new());
    }
    spec
}

fn pricing(relative_cost: f64) -> Option<crate::roster::RolePricingSpec> {
    Some(crate::roster::RolePricingSpec {
        relative_cost: Some(relative_cost),
        ..Default::default()
    })
}

fn spec_pi_lead() -> RoleWorkerSpec {
    RoleWorkerSpec {
        cli: WorkerCliKind::Pi,
        provider: Some("opencodex".into()),
        model: Some("glm-zcode/glm-5.3".into()),
        thinking: Some("max".into()),
        pricing: pricing(5.0),
        ..Default::default()
    }
}
fn spec_pi_explore() -> RoleWorkerSpec {
    RoleWorkerSpec {
        cli: WorkerCliKind::Pi,
        provider: Some("opencodex".into()),
        model: Some("command-code/deepseek-deepseek-v4.1-flash".into()),
        thinking: Some("medium".into()),
        readonly: Some(true),
        pricing: pricing(1.0),
        ..Default::default()
    }
}
fn spec_pi_verifier() -> RoleWorkerSpec {
    RoleWorkerSpec {
        cli: WorkerCliKind::Pi,
        provider: Some("opencodex".into()),
        model: Some("command-code/deepseek-deepseek-v4.1-flash".into()),
        thinking: Some("max".into()),
        readonly: Some(true),
        pricing: pricing(1.5),
        ..Default::default()
    }
}
fn spec_pi_coder() -> RoleWorkerSpec {
    RoleWorkerSpec {
        cli: WorkerCliKind::Pi,
        provider: Some("opencodex".into()),
        model: Some("glm-zcode/glm-5.3".into()),
        thinking: Some("high".into()),
        pricing: pricing(3.0),
        ..Default::default()
    }
}
fn spec_pi_advisor() -> RoleWorkerSpec {
    // TS 원본은 pricing 미지정 — 내장 quota-saver 상속(merge 규약과 동일).
    RoleWorkerSpec {
        cli: WorkerCliKind::Pi,
        provider: Some("opencodex".into()),
        model: Some("glm-zcode/glm-5.3".into()),
        thinking: Some("high".into()),
        readonly: Some(true),
        on_demand: Some(true),
        quota_bucket: Some("pi".into()),
        ..Default::default()
    }
}

#[derive(Clone, Copy)]
enum SpecRole {
    Lead,
    Coder,
    Ro,
    Advisor,
}

fn spec_claude(role: SpecRole) -> RoleWorkerSpec {
    let base = RoleWorkerSpec {
        cli: WorkerCliKind::Claude,
        permission_mode: Some("auto".into()),
        quota_bucket: Some("claude".into()),
        ..Default::default()
    };
    match role {
        SpecRole::Lead => block_inherit(RoleWorkerSpec { pricing: pricing(5.0), ..base }),
        SpecRole::Coder => block_inherit(RoleWorkerSpec { pricing: pricing(3.0), ..base }),
        SpecRole::Ro => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            pricing: pricing(1.5),
            ..base
        }),
        SpecRole::Advisor => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            on_demand: Some(true),
            pricing: pricing(8.0),
            ..base
        }),
    }
}

fn spec_devin(role: SpecRole) -> RoleWorkerSpec {
    let base = RoleWorkerSpec {
        cli: WorkerCliKind::Devin,
        permission_mode: Some("auto".into()),
        quota_bucket: Some("devin".into()),
        ..Default::default()
    };
    match role {
        SpecRole::Lead => block_inherit(RoleWorkerSpec { pricing: pricing(5.0), ..base }),
        SpecRole::Coder => block_inherit(RoleWorkerSpec { pricing: pricing(3.0), ..base }),
        SpecRole::Ro => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            pricing: pricing(1.5),
            ..base
        }),
        SpecRole::Advisor => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            on_demand: Some(true),
            pricing: pricing(8.0),
            ..base
        }),
    }
}

/// codex·gemini 등 WorkerCliKind에 없는 CLI — roster 규약상 `custom` + 명시 bin.
fn spec_custom_cli(bin: &str, role: SpecRole) -> RoleWorkerSpec {
    let base = RoleWorkerSpec {
        cli: WorkerCliKind::Custom,
        bin: Some(bin.to_string()),
        quota_bucket: Some(bin.to_string()),
        ..Default::default()
    };
    match role {
        SpecRole::Lead => block_inherit(RoleWorkerSpec { pricing: pricing(5.0), ..base }),
        SpecRole::Coder => block_inherit(RoleWorkerSpec { pricing: pricing(3.0), ..base }),
        SpecRole::Ro => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            pricing: pricing(1.5),
            ..base
        }),
        SpecRole::Advisor => block_inherit(RoleWorkerSpec {
            readonly: Some(true),
            on_demand: Some(true),
            pricing: pricing(8.0),
            ..base
        }),
    }
}

fn all_slots(make: fn(SpecRole) -> RoleWorkerSpec) -> HashMap<RoleSlot, RoleWorkerSpec> {
    HashMap::from([
        (RoleSlot::Lead, make(SpecRole::Lead)),
        (RoleSlot::Explore, make(SpecRole::Ro)),
        (RoleSlot::Coder, make(SpecRole::Coder)),
        (RoleSlot::Verifier, make(SpecRole::Ro)),
        (RoleSlot::Advisor, make(SpecRole::Advisor)),
    ])
}

fn zero_agy_budgets() -> Option<BTreeMap<String, i64>> {
    Some(BTreeMap::from([("agy".to_string(), 0)]))
}

fn zero_agy_profile(description: &str, roles: HashMap<RoleSlot, RoleWorkerSpec>) -> RosterProfile {
    RosterProfile {
        description: Some(description.to_string()),
        fallback_profile: None,
        daily_turn_budgets: zero_agy_budgets(),
        roles,
    }
}

fn profile_reason_and_build(name: &str) -> Option<(&'static str, RosterProfile)> {
    match name {
        "zero-agy" => Some((
            "hc + pi + devin 감지 — Lead/Advisor pi, Explore/Verifier pi(readonly), Coder devin (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (zero-agy): Lead·Advisor pi / Explore·Verifier pi readonly / Coder devin, agy=0",
                HashMap::from([
                    (RoleSlot::Lead, spec_pi_lead()),
                    (RoleSlot::Explore, spec_pi_explore()),
                    (RoleSlot::Coder, spec_devin(SpecRole::Coder)),
                    (RoleSlot::Verifier, spec_pi_verifier()),
                    (RoleSlot::Advisor, spec_pi_advisor()),
                ]),
            ),
        )),
        "claude-codex" => Some((
            "hc + claude + codex 감지 — Lead/Verifier claude, Coder/Explore codex (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (claude-codex): Lead·Verifier·Advisor claude / Coder·Explore codex, agy=0",
                HashMap::from([
                    (RoleSlot::Lead, spec_claude(SpecRole::Lead)),
                    (RoleSlot::Explore, spec_custom_cli("codex", SpecRole::Ro)),
                    (RoleSlot::Coder, spec_custom_cli("codex", SpecRole::Coder)),
                    (RoleSlot::Verifier, spec_claude(SpecRole::Ro)),
                    (RoleSlot::Advisor, spec_claude(SpecRole::Advisor)),
                ]),
            ),
        )),
        "claude-only" => Some((
            "hc + claude 감지 — 모든 역할 claude (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (claude-only): 모든 역할 claude, agy=0",
                all_slots(spec_claude),
            ),
        )),
        "codex-only" => Some((
            "hc + codex 감지 — 모든 역할 codex (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (codex-only): 모든 역할 codex, agy=0",
                all_slots(|r| spec_custom_cli("codex", r)),
            ),
        )),
        "devin-only" => Some((
            "hc + devin 감지 — 모든 역할 devin (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (devin-only): 모든 역할 devin, agy=0",
                all_slots(spec_devin),
            ),
        )),
        "pi-only" => Some((
            "hc + pi 감지 — 모든 역할 pi (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (pi-only): 모든 역할 pi, agy=0",
                HashMap::from([
                    (RoleSlot::Lead, spec_pi_lead()),
                    (RoleSlot::Explore, spec_pi_explore()),
                    (RoleSlot::Coder, spec_pi_coder()),
                    (RoleSlot::Verifier, spec_pi_verifier()),
                    (RoleSlot::Advisor, spec_pi_advisor()),
                ]),
            ),
        )),
        "gemini-only" => Some((
            "hc + gemini 감지 — 모든 역할 gemini (agy: 0)",
            zero_agy_profile(
                "설치 CLI 맞춤 (gemini-only): 모든 역할 gemini, agy=0",
                all_slots(|r| spec_custom_cli("gemini", r)),
            ),
        )),
        _ => None,
    }
}

/// 위저드가 만들 수 있는 맞춤 프로필 이름 목록.
pub const TAILORED_PROFILE_NAMES: [&str; 7] = [
    "zero-agy",
    "claude-codex",
    "claude-only",
    "codex-only",
    "devin-only",
    "pi-only",
    "gemini-only",
];

fn spec_label(spec: &RoleWorkerSpec) -> String {
    let base = match spec.cli {
        WorkerCliKind::Custom => spec.bin.clone().unwrap_or_else(|| "custom".to_string()),
        other => other.as_str().to_string(),
    };
    let mut tags: Vec<&str> = Vec::new();
    if spec.readonly == Some(true) {
        tags.push("readonly");
    }
    if spec.on_demand == Some(true) {
        tags.push("onDemand");
    }
    if tags.is_empty() {
        base
    } else {
        format!("{} ({})", base, tags.join("·"))
    }
}

fn standalone_recommendation() -> RosterRecommendation {
    let mut roles = HashMap::new();
    for slot in ROLE_SLOTS {
        roles.insert(slot, "—".to_string());
    }
    RosterRecommendation {
        mode: SetupMode::Standalone,
        profile_name: None,
        roles,
        reason: STANDALONE_REASON.to_string(),
        agy: 0,
    }
}

/// 감지된 CLI 조합으로 최적 설정을 결정적으로 추천한다.
/// 규칙: ① `agy`는 절대 자동 선택되지 않는다 ② 미설치 CLI는 절대 추천되지 않는다
/// ③ `hc` 부재 또는 워커 CLI 0개 → standalone.
pub fn recommend_roster_profile(clis: &BTreeMap<String, CliDetection>) -> RosterRecommendation {
    let has = |id: &str| clis.get(id).map(|c| c.installed).unwrap_or(false);
    let worker_count = WORKER_CLI_IDS.iter().filter(|id| has(id)).count();

    if !has("hc") || worker_count == 0 {
        return standalone_recommendation();
    }

    let profile_name = if has("pi") && has("devin") {
        "zero-agy"
    } else if has("claude") && has("codex") {
        "claude-codex"
    } else if has("claude") {
        "claude-only"
    } else if has("codex") {
        "codex-only"
    } else if has("devin") {
        "devin-only"
    } else if has("pi") {
        "pi-only"
    } else {
        "gemini-only"
    };

    let Some((reason, profile)) = profile_reason_and_build(profile_name) else {
        return standalone_recommendation();
    };
    let mut roles = HashMap::new();
    for slot in ROLE_SLOTS {
        roles.insert(
            slot,
            profile
                .roles
                .get(&slot)
                .map(spec_label)
                .unwrap_or_else(|| "—".to_string()),
        );
    }
    RosterRecommendation {
        mode: SetupMode::MultiCli,
        profile_name: Some(profile_name.to_string()),
        roles,
        reason: reason.to_string(),
        agy: 0,
    }
}

/// 추천(또는 프로필 이름)으로 `load_roster_config` 호환 RosterConfig를 만든다.
pub fn build_tailored_roster_config(rec: &RosterRecommendation) -> Option<RosterConfig> {
    build_tailored_roster_config_by_name(rec.profile_name.as_deref())
}

/// 프로필 이름 직접 지정 변형 — 존재하지 않는 이름은 None.
pub fn build_tailored_roster_config_by_name(name: Option<&str>) -> Option<RosterConfig> {
    let name = name
        .map(str::trim)
        .filter(|s| !s.is_empty())?;
    let (_, profile) = profile_reason_and_build(name)?;
    let mut profiles = BTreeMap::new();
    profiles.insert(name.to_string(), profile);
    Some(RosterConfig {
        default_profile: name.to_string(),
        profiles,
    })
}

// ---------------------------------------------------------------------------
// Step 2b: 로컬 모델 발견·티어 분류 (setup v2)
// ---------------------------------------------------------------------------

fn re_model_fast() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)(?:^|[/_\-_.0-9])(?:flash|mini|haiku|small|lite|nano|exaone)(?:[/_\-_.0-9]|$)")
            .unwrap()
    })
}

fn re_model_strong() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| {
        regex::Regex::new(r"(?i)opus|pro|max|glm-5|gpt-6|o3|r1|reasoning|high").unwrap()
    })
}

fn re_toml_model_line() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r#"^\s*model\s*=\s*"([^"]+)""#).unwrap())
}

/// 모델명 휴리스틱으로 비용·성능 티어를 분류한다 (devin은 항상 코딩 에이전트).
/// fast 키워드는 `/ - _ .`·숫자·문자열 경계로 구분된 토큰일 때만 매칭한다 —
/// 'gemini-2.5-pro'의 'gemini' 안 'mini' 오탐을 막는다.
pub fn classify_model_tier(cli: &str, model: &str) -> (ModelTier, &'static str) {
    let tier = if cli == "devin" {
        ModelTier::CodingAgent
    } else if re_model_fast().is_match(model) {
        ModelTier::Fast
    } else if re_model_strong().is_match(model) {
        ModelTier::Strong
    } else {
        ModelTier::Balanced
    };
    (tier, tier.label())
}

/// 설치된 CLI의 로컬 설정·내장 카탈로그에서 사용 가능 모델을 수집한다.
/// - pi: `~/.pi/agent/models.json`의 `providers[*].models` (문자열·`{id}` 항목)
/// - codex: `~/.codex/config.toml`의 `model = "..."` 라인, 없으면 카탈로그
/// - claude: `~/.claude/settings.json` 최상위 `model` + 표준 카탈로그 3종
/// - devin·gemini: 카탈로그 항목
/// - `agy`는 어떤 경우에도 자동 포함하지 않는다 (zero-agy 기본 보존).
/// SECURITY: apiKey·baseUrl 등 다른 키는 절대 열지 않는다 — models 배열만 추출.
pub fn discover_available_models(
    home_dir: &Path,
    clis: &BTreeMap<String, CliDetection>,
) -> Vec<DiscoveredModel> {
    let mut out: Vec<DiscoveredModel> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let cli_installed = |id: &str| clis.get(id).map(|c| c.installed).unwrap_or(false);
    let mut push = |cli: DiscoveredCli,
                    model: String,
                    source: ModelSource,
                    provider: Option<String>| {
        let key = format!("{} {} {}", cli.as_str(), provider.as_deref().unwrap_or(""), model);
        if seen.contains(&key) {
            return;
        }
        seen.insert(key);
        let (tier, tier_label) = classify_model_tier(cli.as_str(), &model);
        out.push(DiscoveredModel {
            cli,
            provider,
            model,
            source,
            tier,
            tier_label,
        });
    };

    if cli_installed("pi") {
        if let Some(obj) = read_json_safe(&home_dir.join(".pi").join("agent").join("models.json")) {
            if let Some(Value::Object(providers)) = obj.get("providers") {
                for (provider_name, prov_obj) in providers {
                    // apiKey·baseUrl 등 다른 키는 절대 열지 않는다 — models 배열만 추출.
                    let Some(models) = prov_obj.get("models").and_then(Value::as_array) else {
                        continue;
                    };
                    for m in models {
                        let id = match m {
                            Value::String(s) => Some(s.clone()),
                            Value::Object(o) => {
                                o.get("id").and_then(Value::as_str).map(str::to_string)
                            }
                            _ => None,
                        };
                        if let Some(id) = id.filter(|s| !s.is_empty()) {
                            push(DiscoveredCli::Pi, id, ModelSource::LocalConfig, Some(provider_name.clone()));
                        }
                    }
                }
            }
        }
    }

    if cli_installed("codex") {
        let mut found = 0usize;
        if let Ok(text) = std::fs::read_to_string(home_dir.join(".codex").join("config.toml")) {
            for line in text.lines() {
                if let Some(c) = re_toml_model_line().captures(line) {
                    if let Some(m) = c.get(1).map(|g| g.as_str().to_string()) {
                        push(DiscoveredCli::Codex, m, ModelSource::LocalConfig, None);
                        found += 1;
                    }
                }
            }
        }
        // config.toml 부재 — 카탈로그 폴백으로 진행
        if found == 0 {
            push(DiscoveredCli::Codex, "gpt-5-codex".to_string(), ModelSource::Catalog, None);
        }
    }

    if cli_installed("claude") {
        if let Some(j) = read_json_safe(&home_dir.join(".claude").join("settings.json")) {
            if let Some(model) = j.get("model").and_then(Value::as_str).filter(|s| !s.trim().is_empty()) {
                push(DiscoveredCli::Claude, model.to_string(), ModelSource::LocalConfig, None);
            }
        }
        for cat in ["claude-sonnet-4-6", "claude-opus-4-8", "claude-haiku-4-5"] {
            push(DiscoveredCli::Claude, cat.to_string(), ModelSource::Catalog, None);
        }
    }

    if cli_installed("devin") {
        push(DiscoveredCli::Devin, "default".to_string(), ModelSource::Catalog, None);
    }

    if cli_installed("gemini") {
        push(DiscoveredCli::Gemini, "gemini-2.5-flash".to_string(), ModelSource::Catalog, None);
        push(DiscoveredCli::Gemini, "gemini-2.5-pro".to_string(), ModelSource::Catalog, None);
    }

    out
}

/// 역할 슬롯별 적합 모델 가이드 — 피커·리포트에서 티어 힌트로 노출.
pub struct RoleFitGuide {
    pub label: &'static str,
    pub recommended_tier: &'static str,
    pub summary: &'static str,
}

pub fn role_fit_guide(slot: &RoleSlot) -> RoleFitGuide {
    match slot {
        RoleSlot::Lead => RoleFitGuide {
            label: "리드",
            recommended_tier: "고성능·추론 또는 균형",
            summary: "계획·작업 분해·최종 통합 — 고성능·추론 또는 균형 모델 권장",
        },
        RoleSlot::Explore => RoleFitGuide {
            label: "탐색",
            recommended_tier: "빠름·저비용",
            summary: "읽기전용 코드 탐색 — 빠름·저비용 모델 권장",
        },
        RoleSlot::Coder => RoleFitGuide {
            label: "코더",
            recommended_tier: "코딩 에이전트 또는 강한 코딩 CLI",
            summary: "파일 수정·테스트 반복 — 코딩 에이전트(devin) 또는 강한 코딩 CLI 권장",
        },
        RoleSlot::Verifier => RoleFitGuide {
            label: "검증",
            recommended_tier: "리드·코더와 다른 모델 계열",
            summary: "독립 교차검증(읽기전용) — 리드·코더와 다른 모델 계열 권장",
        },
        RoleSlot::Advisor => RoleFitGuide {
            label: "자문",
            recommended_tier: "고성능·추론",
            summary: "고난도 설계·막힘 해결(온디맨드) — 고성능·추론 모델 권장",
        },
    }
}

// ---------------------------------------------------------------------------
// Step 2c: 동반 스킬 환경 비교 추천 (companion advisor)
// ---------------------------------------------------------------------------

const COMPANION_HOW_TO_USE: [(CompanionSkillId, &str); 4] = [
    (
        "mistology",
        "작업 착수 전에 \"과거 실수·연관 모듈 확인해줘\"라고 하면 저장소별 브리핑을 자동 조회하고, 완료 전 결합 파급도 점검합니다.",
    ),
    (
        "neverstuck",
        "같은 오류가 3회 이상 반복될 때 \"neverstuck으로 탈출해줘\"라고 하면 원인 진단·프롬프트 재설계로 막힘 루프를 끊습니다.",
    ),
    (
        "todo-flow",
        "중대형 작업 전에 \"할 일 트랙으로 잡아줘\"라고 하면 요구사항·완료 조건을 문서로 먼저 고정해 방향 이탈을 막습니다.",
    ),
    (
        "effort-router",
        "작업을 맡길 때 \"작업 크기 판정해줘\"라고 하면 S/M/L/XL 티어와 단계별 모델·추론 강도 계획을 세워줍니다.",
    ),
];

fn how_to_use(id: &str) -> &'static str {
    COMPANION_HOW_TO_USE
        .iter()
        .find(|(k, _)| *k == id)
        .map(|(_, v)| *v)
        .unwrap_or("")
}

fn companion_repo(id: &str) -> (&'static str, &'static str) {
    COMPANION_REPOS
        .iter()
        .find(|(k, _, _)| *k == id)
        .map(|(_, slug, url)| (*slug, *url))
        .unwrap_or(("", ""))
}

fn mistology_manual_guide() -> String {
    format!(
        "Rust(rustup)를 먼저 설치한 뒤 다음을 실행하세요: {} — 또는 {} 에서 릴리스 다운로드",
        MISTOLOGY_INSTALL_CMD, MISTOLOGY_GUIDE_URL
    )
}

fn skill_folder_guide(skill: &str, contents: &str, repo_url: &str) -> String {
    format!(
        "{} 스킬 폴더({} 포함)를 감지된 에이전트 스킬 디렉터리(~/.claude/skills, ~/.agents/skills 등)에 배치하거나 심볼릭 링크하세요. 위저드의 동반 스킬 단계에서 로컬 폴더 경로나 Git URL({})을 입력하면 검증 후 자동 연결됩니다.",
        skill, contents, repo_url
    )
}

/// 동반 스킬 4종을 실제 감지 환경과 비교해 개별 verdict로 추천한다.
/// 절대 "전부 설치"를 권하지 않는다 — 설치됨/환경 유리/선택/중복 생략을 구분한다.
pub fn recommend_companion_tools(report: &EnvironmentReport) -> Vec<CompanionAdvice> {
    let cli_installed =
        |id: &str| report.clis.get(id).map(|c: &CliDetection| c.installed).unwrap_or(false);
    // 코딩 에이전트 CLI 수 — hc(런처)·cargo(빌드)는 코딩 CLI로 세지 않는다.
    let coding_cli_count = WORKER_CLI_IDS
        .iter()
        .chain(std::iter::once(&"agy"))
        .filter(|id| cli_installed(id))
        .count();
    let multi_cli = report.recommendation.mode == SetupMode::MultiCli;
    let installed = |id: &str| report.companions.get(id).map(|c| c.installed).unwrap_or(false);
    let mut out: Vec<CompanionAdvice> = Vec::new();

    // 1) mistology — Git 기반 저장소에서 체감 효과 최대, cargo가 있으면 자동 설치 가능
    let (m_slug, m_url) = companion_repo("mistology");
    if installed("mistology") {
        out.push(CompanionAdvice {
            id: "mistology".into(),
            name: "mistology".into(),
            verdict: CompanionVerdict::AlreadyInstalled,
            reason: "이미 설치됨 — 작업 전 과거 코드 실수·연관 모듈을 자동 브리핑합니다.".into(),
            how_to_use: how_to_use("mistology").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: String::new(),
            repo_slug: m_slug.into(),
            repo_url: m_url.into(),
        });
    } else if report.git.installed {
        let cargo = cli_installed("cargo");
        out.push(CompanionAdvice {
            id: "mistology".into(),
            name: "mistology".into(),
            verdict: CompanionVerdict::Recommended,
            reason: "Git 기반 코드 작업 환경에서 반복 실수와 연관 모듈 깨짐을 줄이는 체감 효과가 가장 큽니다.".into(),
            how_to_use: how_to_use("mistology").into(),
            can_auto_install: cargo,
            install_command: cargo.then(|| MISTOLOGY_INSTALL_CMD.to_string()),
            install_guide: if cargo {
                format!("다음 명령으로 설치합니다: {} — 가이드: {}", MISTOLOGY_INSTALL_CMD, MISTOLOGY_GUIDE_URL)
            } else {
                mistology_manual_guide()
            },
            repo_slug: m_slug.into(),
            repo_url: m_url.into(),
        });
    } else {
        out.push(CompanionAdvice {
            id: "mistology".into(),
            name: "mistology".into(),
            verdict: CompanionVerdict::Optional,
            reason: "Git이 감지되지 않아 저장소 연관 추적 효과가 제한적입니다 — Git 기반 작업을 시작하면 도입하세요.".into(),
            how_to_use: how_to_use("mistology").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: mistology_manual_guide(),
            repo_slug: m_slug.into(),
            repo_url: m_url.into(),
        });
    }

    // 2) neverstuck — 코딩 에이전트가 있는 환경에서 막힘 루프 차단
    let (n_slug, n_url) = companion_repo("neverstuck");
    let neverstuck_guide = skill_folder_guide("neverstuck", "SKILL.md·PROTOCOL.md", n_url);
    if installed("neverstuck") {
        out.push(CompanionAdvice {
            id: "neverstuck".into(),
            name: "neverstuck".into(),
            verdict: CompanionVerdict::AlreadyInstalled,
            reason: "이미 설치됨 — 같은 오류 반복 시 막힘 탈출 프로토콜이 적용됩니다.".into(),
            how_to_use: how_to_use("neverstuck").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: String::new(),
            repo_slug: n_slug.into(),
            repo_url: n_url.into(),
        });
    } else {
        out.push(CompanionAdvice {
            id: "neverstuck".into(),
            name: "neverstuck".into(),
            verdict: if coding_cli_count >= 1 {
                CompanionVerdict::Recommended
            } else {
                CompanionVerdict::Optional
            },
            reason: if coding_cli_count >= 1 {
                "코딩 에이전트가 같은 오류를 3회 이상 반복할 때 프롬프트를 재설계해 막힘 루프를 끊어줍니다.".into()
            } else {
                "설치된 코딩 CLI가 없어 아직 필요 없습니다 — 코딩 에이전트 사용을 시작하면 도입하세요.".into()
            },
            how_to_use: how_to_use("neverstuck").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: neverstuck_guide,
            repo_slug: n_slug.into(),
            repo_url: n_url.into(),
        });
    }

    // 3) todo-flow — 멀티 워커·중대형 작업 전 요구사항 문서화
    let (t_slug, t_url) = companion_repo("todo-flow");
    let todo_guide = skill_folder_guide("todo", "SKILL.md", t_url);
    if installed("todo-flow") {
        out.push(CompanionAdvice {
            id: "todo-flow".into(),
            name: "todo-flow".into(),
            verdict: CompanionVerdict::AlreadyInstalled,
            reason: "이미 설치됨 — 할 일 트랙 문서로 요구사항·완료 조건을 관리합니다.".into(),
            how_to_use: how_to_use("todo-flow").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: String::new(),
            repo_slug: t_slug.into(),
            repo_url: t_url.into(),
        });
    } else {
        let useful = multi_cli || coding_cli_count >= 2;
        out.push(CompanionAdvice {
            id: "todo-flow".into(),
            name: "todo-flow".into(),
            verdict: if useful {
                CompanionVerdict::Recommended
            } else {
                CompanionVerdict::Optional
            },
            reason: if useful {
                "멀티 워커·중대형 기능 개발 전에 요구사항과 완료 조건을 문서로 먼저 고정해 방향 이탈을 막아줍니다.".into()
            } else {
                "단독 소규모 작업이면 생략 가능하며, 프로젝트가 커질 때 도입하면 좋습니다.".into()
            },
            how_to_use: how_to_use("todo-flow").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: todo_guide,
            repo_slug: t_slug.into(),
            repo_url: t_url.into(),
        });
    }

    // 4) effort-router — 단일 세션 중심 환경에서 작업 크기·추론 강도 체계화
    let (e_slug, e_url) = companion_repo("effort-router");
    let effort_guide = skill_folder_guide("effort-router", "SKILL.md", e_url);
    if installed("effort-router") {
        out.push(CompanionAdvice {
            id: "effort-router".into(),
            name: "effort-router".into(),
            verdict: CompanionVerdict::AlreadyInstalled,
            reason: "이미 설치됨 — 작업 크기별 모델·추론 강도를 체계화합니다.".into(),
            how_to_use: how_to_use("effort-router").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: String::new(),
            repo_slug: e_slug.into(),
            repo_url: e_url.into(),
        });
    } else if multi_cli {
        out.push(CompanionAdvice {
            id: "effort-router".into(),
            name: "effort-router".into(),
            verdict: CompanionVerdict::Optional,
            reason: "이미 `dgate` 멀티워커 배차가 가능하므로, 단일 세션 단계 계획·증거번들이 추가로 필요할 때만 선택하세요 (중복 라우팅 주의).".into(),
            how_to_use: how_to_use("effort-router").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: effort_guide,
            repo_slug: e_slug.into(),
            repo_url: e_url.into(),
        });
    } else if coding_cli_count >= 1 {
        out.push(CompanionAdvice {
            id: "effort-router".into(),
            name: "effort-router".into(),
            verdict: CompanionVerdict::Recommended,
            reason: "단일 채팅 세션 중심 환경이라 작업 크기(S/M/L/XL)별 모델·추론 강도를 체계화하는 데 유리합니다.".into(),
            how_to_use: how_to_use("effort-router").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: effort_guide,
            repo_slug: e_slug.into(),
            repo_url: e_url.into(),
        });
    } else {
        out.push(CompanionAdvice {
            id: "effort-router".into(),
            name: "effort-router".into(),
            verdict: CompanionVerdict::SkipOverlap,
            reason: "설치된 코딩 CLI가 없어 현재 활용할 세션이 없습니다 — 코딩 CLI 설치 후 다시 확인하세요.".into(),
            how_to_use: how_to_use("effort-router").into(),
            can_auto_install: false,
            install_command: None,
            install_guide: effort_guide,
            repo_slug: e_slug.into(),
            repo_url: e_url.into(),
        });
    }

    out
}

// ---------------------------------------------------------------------------
// 설치 헬퍼 (opt-in, 멱등)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SkillLinkResult {
    pub linked: Vec<String>,
    pub skipped: Vec<SkipEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SkipEntry {
    pub dir: String,
    pub reason: &'static str,
}

/// `install_skill_links` 옵션 — 홈·오케스트레이터 루트·대상 에이전트 id 주입.
#[derive(Debug, Clone, Default)]
pub struct InstallSkillLinksOptions {
    pub home_dir: Option<PathBuf>,
    pub repo_dir: Option<PathBuf>,
    pub only_agent_ids: Option<Vec<String>>,
}

/// `<agent>/skills/dgate` → `<repoDir>/skills/dgate` 심볼릭 링크 생성.
/// 존재하는 에이전트 디렉터리에만 만들고, 기존 경로는 절대 덮어쓰지 않는다.
pub fn install_skill_links(opts: &InstallSkillLinksOptions) -> SkillLinkResult {
    let home_dir = opts
        .home_dir
        .clone()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"));
    let repo_dir = opts.repo_dir.clone().unwrap_or_else(default_repo_root);
    let source = repo_dir.join("skills").join("dgate");
    let mut res = SkillLinkResult::default();

    if !source.join("SKILL.md").exists() {
        for (id, dir) in AGENT_SKILL_DIRS {
            let _ = id;
            res.skipped.push(SkipEntry {
                dir: home_dir.join(dir).to_string_lossy().into_owned(),
                reason: "source-skill-missing",
            });
        }
        return res;
    }

    for (id, dir) in AGENT_SKILL_DIRS {
        if let Some(only) = &opts.only_agent_ids {
            if !only.iter().any(|o| o == id) {
                continue;
            }
        }
        let agent_dir = home_dir.join(dir);
        if !agent_dir.exists() {
            res.skipped.push(SkipEntry {
                dir: agent_dir.to_string_lossy().into_owned(),
                reason: "agent-dir-absent",
            });
            continue;
        }
        let skills_dir = agent_dir.join("skills");
        let _ = std::fs::create_dir_all(&skills_dir);
        let target = skills_dir.join("dgate");
        match std::fs::symlink_metadata(&target) {
            Ok(meta) => {
                let already = meta.file_type().is_symlink()
                    && std::fs::read_link(&target)
                        .map(|p| p == source)
                        .unwrap_or(false);
                res.skipped.push(SkipEntry {
                    dir: agent_dir.to_string_lossy().into_owned(),
                    reason: if already {
                        "already-linked"
                    } else {
                        "conflict-existing-path"
                    },
                });
            }
            Err(_) => {
                // 기존 경로 없음 — 심볼릭 링크 생성(실패는 조용히 스킵 처리)
                match std::os::unix::fs::symlink(&source, &target) {
                    Ok(()) => res.linked.push(agent_dir.to_string_lossy().into_owned()),
                    Err(_) => res.skipped.push(SkipEntry {
                        dir: agent_dir.to_string_lossy().into_owned(),
                        reason: "symlink-failed",
                    }),
                }
            }
        }
    }
    res
}

/// 타깃 프로젝트 훅 상태 진단(읽기전용) — `--repo` 지정 시 리포트에 포함된다.
pub fn inspect_repo_hooks(repo_dir: &Path, orchestrator_root: &Path) -> RepoHooksStatus {
    let agents_dir = repo_dir.join(".agents");
    let hook_link = agents_dir.join("hooks").join("dg-hook.ts");
    let src_hook = orchestrator_root.join(".agents").join("hooks").join("dg-hook.ts");
    let hook_symlink = match std::fs::symlink_metadata(&hook_link) {
        Ok(meta) => {
            meta.file_type().is_symlink()
                && std::fs::read_link(&hook_link)
                    .map(|p| p == src_hook)
                    .unwrap_or(false)
        }
        Err(_) => false,
    };
    let exclude_path = repo_dir.join(".git").join("info").join("exclude");
    let excluded = std::fs::read_to_string(&exclude_path)
        .map(|c| {
            let lines: Vec<&str> = c.split('\n').map(str::trim).collect();
            lines.contains(&GIT_EXCLUDE_ENTRIES[0]) && lines.contains(&GIT_EXCLUDE_ENTRIES[1])
        })
        .unwrap_or(false);
    RepoHooksStatus {
        repo_dir: repo_dir.to_string_lossy().into_owned(),
        repo_dir_exists: repo_dir.exists(),
        hooks_json: agents_dir.join("hooks.json").exists(),
        hook_symlink,
        excluded,
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct RepoHooksResult {
    pub repo_dir: String,
    pub written: Vec<String>,
    pub skipped: Vec<PathSkipEntry>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PathSkipEntry {
    pub path: String,
    pub reason: &'static str,
}

/// 타깃 프로젝트에 `.agents/hooks.json`(canonical 복사) + `hooks/dg-hook.ts`
/// (오케스트레이터 심볼릭 링크)를 설치하고 `.git/info/exclude`에 두 경로를 추가해
/// `git status`를 깨끗하게 유지한다. 기존 파일·다른 내용은 절대 덮어쓰지 않는다.
pub fn install_repo_hooks(repo_dir: &Path, orchestrator_root: Option<&Path>) -> RepoHooksResult {
    let orch = orchestrator_root
        .map(Path::to_path_buf)
        .unwrap_or_else(default_repo_root);
    let mut res = RepoHooksResult {
        repo_dir: repo_dir.to_string_lossy().into_owned(),
        written: Vec::new(),
        skipped: Vec::new(),
    };
    let src_json = orch.join(".agents").join("hooks.json");
    let src_hook = orch.join(".agents").join("hooks").join("dg-hook.ts");
    if !src_json.exists() || !src_hook.exists() {
        res.skipped.push(PathSkipEntry {
            path: repo_dir.to_string_lossy().into_owned(),
            reason: "orchestrator-hook-source-missing",
        });
        return res;
    }

    let hooks_dir = repo_dir.join(".agents").join("hooks");
    if let Err(e) = std::fs::create_dir_all(&hooks_dir) {
        res.skipped.push(PathSkipEntry {
            path: hooks_dir.to_string_lossy().into_owned(),
            reason: "mkdir-failed",
        });
        let _ = e;
        return res;
    }

    let dst_json = repo_dir.join(".agents").join("hooks.json");
    let canonical = std::fs::read_to_string(&src_json).unwrap_or_default();
    if !dst_json.exists() {
        if std::fs::write(&dst_json, &canonical).is_ok() {
            res.written.push(dst_json.to_string_lossy().into_owned());
        }
    } else if std::fs::read_to_string(&dst_json).map(|c| c == canonical).unwrap_or(false) {
        res.skipped.push(PathSkipEntry {
            path: dst_json.to_string_lossy().into_owned(),
            reason: "already-present",
        });
    } else {
        res.skipped.push(PathSkipEntry {
            path: dst_json.to_string_lossy().into_owned(),
            reason: "conflict-different-content",
        });
    }

    let dst_hook = hooks_dir.join("dg-hook.ts");
    match std::fs::symlink_metadata(&dst_hook) {
        Err(_) => {
            match std::os::unix::fs::symlink(&src_hook, &dst_hook) {
                Ok(()) => res.written.push(dst_hook.to_string_lossy().into_owned()),
                Err(_) => res.skipped.push(PathSkipEntry {
                    path: dst_hook.to_string_lossy().into_owned(),
                    reason: "symlink-failed",
                }),
            }
        }
        Ok(meta) => {
            let already = meta.file_type().is_symlink()
                && std::fs::read_link(&dst_hook).map(|p| p == src_hook).unwrap_or(false);
            res.skipped.push(PathSkipEntry {
                path: dst_hook.to_string_lossy().into_owned(),
                reason: if already {
                    "already-linked"
                } else {
                    "conflict-existing-path"
                },
            });
        }
    }

    let git_dir = repo_dir.join(".git");
    let exclude_path = git_dir.join("info").join("exclude");
    if !git_dir.exists() {
        res.skipped.push(PathSkipEntry {
            path: exclude_path.to_string_lossy().into_owned(),
            reason: "not-a-git-repo",
        });
        return res;
    }
    let _ = std::fs::create_dir_all(git_dir.join("info"));
    let existing = std::fs::read_to_string(&exclude_path).unwrap_or_default();
    let lines: Vec<&str> = existing
        .split('\n')
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let missing: Vec<&str> = GIT_EXCLUDE_ENTRIES
        .iter()
        .copied()
        .filter(|e| !lines.contains(e))
        .collect();
    if missing.is_empty() {
        res.skipped.push(PathSkipEntry {
            path: exclude_path.to_string_lossy().into_owned(),
            reason: "already-excluded",
        });
    } else {
        let mut base = existing.clone();
        if !existing.is_empty() && !existing.ends_with('\n') {
            base.push('\n');
        }
        let mut content = base;
        content.push_str(&missing.join("\n"));
        content.push('\n');
        if std::fs::write(&exclude_path, content).is_ok() {
            res.written.push(exclude_path.to_string_lossy().into_owned());
        }
    }
    res
}

// ---------------------------------------------------------------------------
// GitHub 스타 (opt-in 전용)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
pub struct StarRepoResult {
    pub ok: bool,
    pub repo_slug: String,
    /// 실패·거부 시에도 항상 제공되는 수동 안내 URL
    pub fallback_url: String,
    pub reason: Option<String>,
}

/// gh 실행 주입 결과 — 실제 spawn 성공 시 `error = None`·`status`에 exit code.
#[derive(Debug, Clone, Default)]
pub struct GhCommandOutput {
    pub error: Option<String>,
    pub status: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

/// gh 실행 주입 함수 — 테스트가 spawn을 흉내 낸다.
pub type GhExec<'a> = Box<dyn Fn(&str, &[String]) -> GhCommandOutput + 'a>;

fn re_repo_slug() -> &'static regex::Regex {
    use std::sync::OnceLock;
    static RE: OnceLock<regex::Regex> = OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(r"^[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+$").unwrap())
}

/// `gh api -X PUT user/starred/<owner>/<repo>`로 저장소에 스타를 등록한다.
/// 사용자의 명시적 동의가 있을 때만 호출되어야 하며(호출 측 책임 — opt-in 전용),
/// 절대 패닉하지 않고 gh 부재(ENOENT)·비정상 종료·비공개 저장소 404는 깔끔한
/// 폴백 URL로 되돌린다. argv 배열로만 실행한다 — 쉘 문자열 결합·쉘 실행은 절대 금지.
pub fn star_github_repo(repo_slug: &str) -> StarRepoResult {
    let exec: GhExec<'_> = Box::new(|file: &str, args: &[String]| {
        let output = match std::process::Command::new(file).args(args).output() {
            Ok(o) => o,
            Err(e) => {
                let code = match e.kind() {
                    std::io::ErrorKind::NotFound => Some("ENOENT".to_string()),
                    _ => None,
                };
                return GhCommandOutput {
                    error: Some(code.unwrap_or_else(|| e.to_string())),
                    status: None,
                    stdout: String::new(),
                    stderr: String::new(),
                };
            }
        };
        GhCommandOutput {
            error: None,
            status: output.status.code(),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        }
    });
    star_github_repo_with(repo_slug, exec)
}

/// 실행기 주입 변형(테스트용) — 판정 로직의 단일 구현.
pub fn star_github_repo_with(repo_slug: &str, exec: GhExec<'_>) -> StarRepoResult {
    let fallback_url = format!("https://github.com/{}", repo_slug);
    if !re_repo_slug().is_match(repo_slug) {
        return StarRepoResult {
            ok: false,
            repo_slug: repo_slug.to_string(),
            fallback_url,
            reason: Some("invalid-repo-slug".to_string()),
        };
    }
    let args: Vec<String> = vec![
        "api".to_string(),
        "-X".to_string(),
        "PUT".to_string(),
        format!("user/starred/{}", repo_slug),
    ];
    let res = exec("gh", &args);
    if let Some(err) = res.error {
        return StarRepoResult {
            ok: false,
            repo_slug: repo_slug.to_string(),
            fallback_url,
            reason: Some(if err == "ENOENT" {
                "gh-not-installed".to_string()
            } else {
                format!("gh-spawn-error:{}", err)
            }),
        };
    }
    if res.status != Some(0) {
        let err_text = format!("{} {}", res.stderr, res.stdout);
        let status_label = res
            .status
            .map(|s| s.to_string())
            .unwrap_or_else(|| "null".to_string());
        return StarRepoResult {
            ok: false,
            repo_slug: repo_slug.to_string(),
            fallback_url,
            reason: Some(
                if err_text.contains("404") || err_text.to_lowercase().contains("not found") {
                    "repo-not-found-or-private".to_string()
                } else {
                    format!("gh-exit-{}", status_label)
                },
            ),
        };
    }
    StarRepoResult {
        ok: true,
        repo_slug: repo_slug.to_string(),
        fallback_url,
        reason: None,
    }
}

// ---------------------------------------------------------------------------
// 진단 리포트 포맷
// ---------------------------------------------------------------------------

fn mark(ok: bool, optional: bool) -> &'static str {
    if ok {
        "✓"
    } else if optional {
        "○"
    } else {
        "✗"
    }
}

fn pad_end(s: &str, width: usize) -> String {
    let len = s.chars().count();
    if len >= width {
        s.to_string()
    } else {
        format!("{}{}", s, " ".repeat(width - len))
    }
}

/// `--check` 읽기전용 진단 리포트 — 0 writes/0 prompts, 한국어 고정 포맷.
pub fn format_check_report(report: &EnvironmentReport) -> String {
    let mut l: Vec<String> = Vec::new();
    l.push("[setup --check] dgate 환경 진단".to_string());
    l.push(String::new());
    l.push("필수 런타임:".to_string());
    l.push(format!(
        "  {} node {} (>= 22.6 필요)",
        mark(report.node.ok, false),
        report.node.version
    ));
    l.push(format!(
        "  {} node:sqlite (DatabaseSync 로드)",
        mark(report.node_sqlite_ok, false)
    ));
    l.push(format!(
        "  {} git {}",
        mark(report.git.installed, false),
        report.git.path.clone().unwrap_or_else(|| "미설치".to_string())
    ));
    l.push(String::new());
    l.push("코딩 CLI (선택):".to_string());
    for id in CLI_IDS {
        let c = &report.clis[id];
        let note = if id == "agy" {
            " — 감지됨, 쿼터 한도로 기본 미선택"
        } else {
            ""
        };
        l.push(format!(
            "  {} {} {}{}",
            mark(c.installed, true),
            id,
            c.path.clone().unwrap_or_else(|| "(미설치)".to_string()),
            if c.installed { note } else { "" }
        ));
    }
    if !report.discovered_models.is_empty() {
        l.push(String::new());
        l.push(format!(
            "사용 가능 모델 ({}종, 로컬 감지):",
            report.discovered_models.len()
        ));
        const MAX: usize = 12;
        for m in report.discovered_models.iter().take(MAX) {
            let prov = m
                .provider
                .as_ref()
                .map(|p| format!(" · {}", p))
                .unwrap_or_default();
            let src = if m.source == ModelSource::LocalConfig {
                " · 로컬 설정"
            } else {
                ""
            };
            l.push(format!(
                "  [{}] {} — {}{}{}",
                m.cli.as_str(),
                m.model,
                m.tier_label,
                prov,
                src
            ));
        }
        if report.discovered_models.len() > MAX {
            l.push(format!(
                "  … 외 {}종",
                report.discovered_models.len() - MAX
            ));
        }
    }
    l.push(String::new());
    l.push("에이전트 스킬 디렉터리:".to_string());
    for a in &report.agents {
        if !a.exists {
            l.push(format!("  ○ {} (미설치)", a.dir));
        } else {
            l.push(format!(
                "  {} {} {}",
                if a.skill_linked { "✓" } else { "○" },
                a.dir,
                if a.skill_linked {
                    "(dgate 링크됨)"
                } else {
                    "(스킬 미링크)"
                }
            ));
        }
    }
    l.push(String::new());
    l.push("설정·상태:".to_string());
    let roster_mark = if report.roster.exists {
        if report.roster.valid_json {
            "✓"
        } else {
            "✗"
        }
    } else {
        "○"
    };
    let roster_desc = if report.roster.exists {
        if report.roster.valid_json {
            report.roster.path.clone()
        } else {
            format!("{} (JSON 파싱 실패)", report.roster.path)
        }
    } else {
        format!("없음 ({})", report.roster.path)
    };
    l.push(format!("  {} 전역 roster {}", roster_mark, roster_desc));
    l.push(format!(
        "  {} 결정 DB {}",
        mark(report.db.exists, true),
        if report.db.exists {
            report.db.path.clone()
        } else {
            format!("없음 ({}, {})", report.db.path, report.db.source)
        }
    ));
    let secret_state = if report.secrets.env_set {
        "(환경변수 설정됨)"
    } else if report.secrets.secrets_file_exists {
        "(secrets.env 존재)"
    } else {
        "(미설정 — 기본 off, 무료 로컬 통계)"
    };
    l.push(format!(
        "  {} {} {}",
        mark(report.secrets.configured, true),
        DECISION_API_KEY_ENV,
        secret_state
    ));
    l.push(String::new());
    l.push("선택 도구·스킬:".to_string());
    for id in COMPANION_IDS {
        let c = &report.companions[id];
        l.push(format!("  {} {} — {}", mark(c.installed, true), id, c.role));
        if c.installed {
            l.push(format!("      → {}", c.detail));
        }
    }
    if !report.companion_advice.is_empty() {
        l.push(String::new());
        l.push("동반 스킬 환경 비교 추천:".to_string());
        for a in &report.companion_advice {
            let v_label = match a.verdict {
                CompanionVerdict::AlreadyInstalled => "✓ 설치됨",
                CompanionVerdict::Recommended => "◆ 추천",
                CompanionVerdict::Optional => "○ 선택",
                CompanionVerdict::SkipOverlap => "· 생략",
            };
            l.push(format!("  {} {} — {}", v_label, a.id, a.reason));
            l.push(format!("      {}", a.repo_url));
            if a.verdict == CompanionVerdict::Recommended
                && a.can_auto_install
                && a.install_command.is_some()
            {
                l.push(format!(
                    "      → 지금 자동 설치 가능: {}",
                    a.install_command.clone().unwrap_or_default()
                ));
            }
        }
    }
    l.push(String::new());
    let rec = &report.recommendation;
    if rec.mode == SetupMode::Standalone {
        l.push(format!("추천 모드: standalone — {}", rec.reason));
    } else {
        l.push(format!(
            "추천 모드: multi-cli / 프로필 \"{}\" (agy: {})",
            rec.profile_name.clone().unwrap_or_default(),
            rec.agy
        ));
        l.push(format!("  {}", rec.reason));
        for slot in ROLE_SLOTS {
            l.push(format!(
                "    {} → {}  (권장: {})",
                pad_end(slot.as_str(), 8),
                rec.roles.get(&slot).map(String::as_str).unwrap_or("—"),
                role_fit_guide(&slot).recommended_tier
            ));
        }
    }
    if let Some(h) = &report.repo_hooks {
        l.push(String::new());
        l.push(format!("대상 프로젝트 훅 상태 ({}):", h.repo_dir));
        l.push(format!(
            "  {} 디렉터리 {}",
            mark(h.repo_dir_exists, false),
            if h.repo_dir_exists { "존재" } else { "없음" }
        ));
        l.push(format!(
            "  {} .agents/hooks.json {}",
            mark(h.hooks_json, true),
            if h.hooks_json { "설치됨" } else { "미설치" }
        ));
        l.push(format!(
            "  {} .agents/hooks/dg-hook.ts {}",
            mark(h.hook_symlink, true),
            if h.hook_symlink {
                "심볼릭 링크됨"
            } else {
                "미설치"
            }
        ));
        l.push(format!(
            "  {} .git/info/exclude {}",
            mark(h.excluded, true),
            if h.excluded { "항목 있음" } else { "항목 없음" }
        ));
    }
    l.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::{env_lock, temp_dir};
    use std::sync::MutexGuard;

    // ---------- 테스트 픽스처 ----------

    fn make_exec(dir: &Path, name: &str) {
        let p = dir.join(name);
        std::fs::write(&p, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perm = std::fs::metadata(&p).unwrap().permissions();
            perm.set_mode(0o755);
            std::fs::set_permissions(&p, perm).unwrap();
        }
    }

    fn make_non_exec(dir: &Path, name: &str) {
        std::fs::write(dir.join(name), "not executable").unwrap();
    }

    fn fake_home(tag: &str) -> PathBuf {
        let home = temp_dir(tag);
        std::fs::create_dir_all(home.join("bin")).unwrap();
        std::fs::create_dir_all(home.join("sysbin")).unwrap();
        home
    }

    // ---------- find_on_path·parse_node_version ----------

    #[test]
    fn find_on_path_respects_order_and_executability() {
        let tmp = temp_dir("fop");
        std::fs::create_dir_all(tmp.join("a")).unwrap();
        std::fs::create_dir_all(tmp.join("b")).unwrap();
        make_exec(&tmp.join("a"), "tool");
        make_exec(&tmp.join("b"), "tool");
        make_non_exec(&tmp.join("a"), "plain");
        let dirs = vec![tmp.join("a"), tmp.join("b")];
        assert_eq!(
            find_on_path("tool", &dirs),
            Some(tmp.join("a").join("tool").to_string_lossy().into_owned())
        );
        assert_eq!(find_on_path("plain", &dirs), None, "실행 권한 없는 파일은 미검출");
        assert_eq!(find_on_path("missing", &dirs), None);
        // 빈 디렉터리·중복 디렉터리 무시
        assert_eq!(find_on_path("tool", &[]), None);
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn parse_node_version_matches_ts_rules() {
        assert!(parse_node_version("v22.6.0").ok);
        assert!(parse_node_version("v23.1.0").ok);
        assert!(parse_node_version("24.0.0").ok);
        assert!(!parse_node_version("v22.5.9").ok);
        assert!(!parse_node_version("v21.9.0").ok);
        assert!(!parse_node_version("garbage").ok);
        let n = parse_node_version("v22.6.1");
        assert_eq!((n.major, n.minor), (22, 6));
        assert_eq!(n.version, "v22.6.1");
    }

    // ---------- detect_environment ----------

    #[test]
    fn detect_environment_full_matrix() {
        let home = fake_home("detect-full");
        // bin: git, hc, pi, devin / sysbin: claude
        for name in ["git", "hc", "pi", "devin"] {
            make_exec(&home.join("bin"), name);
        }
        make_exec(&home.join("sysbin"), "claude");
        // 에이전트 디렉터리
        std::fs::create_dir_all(home.join(".claude").join("skills").join("dgate")).unwrap();
        std::fs::write(
            home.join(".claude").join("skills").join("dgate").join("SKILL.md"),
            "# dgate",
        )
        .unwrap();
        std::fs::create_dir_all(home.join(".agents").join("skills").join("todo")).unwrap();
        std::fs::write(home.join(".agents").join("skills").join("todo").join("SKILL.md"), "").unwrap();
        // 전역 roster — 유효 JSON
        let xdg = home.join("xdgconfig");
        std::fs::create_dir_all(xdg.join("herdr-quad")).unwrap();
        std::fs::write(xdg.join("herdr-quad").join("roster.json"), "{}").unwrap();
        std::fs::write(xdg.join("herdr-quad").join("secrets.env"), "GEMINI_DECISION_API_KEY=x").unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        env.insert("XDG_CONFIG_HOME".to_string(), xdg.to_string_lossy().into_owned());
        env.insert("GEMINI_DECISION_API_KEY".to_string(), "  ".to_string()); // 공백 → 미설정
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: None,
            node_sqlite_ok: None,
            repo_dir: None,
            orchestrator_root: None,
        };
        let report = detect_environment(&deps);
        assert!(report.ok); // rust-native node + git 설치
        assert!(report.git.installed);
        for id in ["hc", "pi", "devin", "claude"] {
            assert!(report.clis[id].installed, "{}", id);
        }
        assert!(!report.clis["codex"].installed);
        assert!(!report.clis["agy"].installed);
        // 에이전트 스킬
        let claude_dir = report.agents.iter().find(|a| a.id == "claude").unwrap();
        assert!(claude_dir.exists && claude_dir.skill_linked);
        let agents_dir = report.agents.iter().find(|a| a.id == "agents").unwrap();
        assert!(agents_dir.exists && !agents_dir.skill_linked);
        // roster 유효
        assert!(report.roster.exists && report.roster.valid_json);
        // secrets: env 공백 → env_set=false, 파일 존재 → configured=true (값 미저장 확인)
        assert!(!report.secrets.env_set);
        assert!(report.secrets.secrets_file_exists);
        assert!(report.secrets.configured);
        // 동반 스킬: todo 설치됨, neverstuck 미설치
        assert!(report.companions["todo-flow"].installed);
        assert!(!report.companions["neverstuck"].installed);
        assert!(!report.companions["mistology"].installed);
        // 추천: hc + pi + devin → zero-agy
        assert_eq!(report.recommendation.mode, SetupMode::MultiCli);
        assert_eq!(report.recommendation.profile_name.as_deref(), Some("zero-agy"));
        assert_eq!(report.recommendation.agy, 0);
        assert!(report.repo_hooks.is_none());
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn detect_environment_broken_roster_and_missing_pieces() {
        let home = fake_home("detect-broken");
        let xdg = home.join("xdgconfig");
        std::fs::create_dir_all(xdg.join("herdr-quad")).unwrap();
        std::fs::write(xdg.join("herdr-quad").join("roster.json"), "{broken").unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        env.insert("XDG_CONFIG_HOME".to_string(), xdg.to_string_lossy().into_owned());
        env.insert("GEMINI_DECISION_API_KEY".to_string(), "real-key-value".to_string());
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: Some("v20.0.0".to_string()),
            node_sqlite_ok: Some(false),
            repo_dir: None,
            orchestrator_root: None,
        };
        let report = detect_environment(&deps);
        assert!(!report.ok, "node 미달 + nodeSqlite 실패");
        assert!(!report.node.ok);
        assert!(!report.node_sqlite_ok);
        assert!(report.roster.exists && !report.roster.valid_json);
        assert!(report.secrets.env_set && report.secrets.configured);
        // 시크릿 값이 리포트에 남지 않는다 — 경로·불린만 존재
        assert!(!format!("{:?}", report).contains("real-key-value"));
        // hc 없음 → standalone
        assert_eq!(report.recommendation.mode, SetupMode::Standalone);
        assert_eq!(report.recommendation.profile_name, None);
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn detect_environment_db_path_priority() {
        let home = fake_home("detect-db");
        let db1 = home.join("d1.db");
        std::fs::write(&db1, "x").unwrap();

        let mk = |env_extra: Vec<(&str, String)>| {
            let mut env = BTreeMap::new();
            env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
            for (k, v) in env_extra {
                env.insert(k.to_string(), v);
            }
            DetectDeps {
                env: Some(env),
                home_dir: Some(home.clone()),
                path_dirs: None,
                system_dirs: Some(vec![home.join("sysbin")]),
                node_version: None,
                node_sqlite_ok: None,
                repo_dir: None,
                orchestrator_root: None,
            }
        };
        // 1) DGATE_DB_PATH 최우선
        let r = detect_environment(&mk(vec![(
            "DGATE_DB_PATH",
            db1.to_string_lossy().into_owned(),
        )]));
        assert_eq!(r.db.source, "DGATE_DB_PATH");
        assert_eq!(r.db.path, db1.to_string_lossy().into_owned());
        assert!(r.db.exists);
        // 2) DECISION_ORCHESTRATOR_DB_PATH
        let r2 = detect_environment(&mk(vec![(
            "DECISION_ORCHESTRATOR_DB_PATH",
            home.join("d2.db").to_string_lossy().into_owned(),
        )]));
        assert_eq!(r2.db.source, "DECISION_ORCHESTRATOR_DB_PATH");
        // 3) GEMINI_TRICK_DB_PATH (레거시)
        let r3 = detect_environment(&mk(vec![(
            "GEMINI_TRICK_DB_PATH",
            home.join("d3.db").to_string_lossy().into_owned(),
        )]));
        assert_eq!(r3.db.source, "GEMINI_TRICK_DB_PATH");
        // 4) 기본 XDG state
        let r4 = detect_environment(&mk(vec![]));
        assert_eq!(r4.db.source, "default");
        assert!(r4.db.path.ends_with("dgate/decision.db"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn detect_environment_repo_hooks_included_when_repo_dir_given() {
        let home = fake_home("detect-hooks");
        let orch = home.join("orch");
        std::fs::create_dir_all(orch.join(".agents").join("hooks")).unwrap();
        std::fs::write(orch.join(".agents").join("hooks.json"), "{}").unwrap();
        std::fs::write(orch.join(".agents").join("hooks").join("dg-hook.ts"), "// hook").unwrap();
        let target = home.join("target-repo");
        std::fs::create_dir_all(&target).unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: None,
            node_sqlite_ok: None,
            repo_dir: Some(target.clone()),
            orchestrator_root: Some(orch.clone()),
        };
        let report = detect_environment(&deps);
        let h = report.repo_hooks.expect("repo hooks diagnosed");
        assert!(h.repo_dir_exists);
        assert!(!h.hooks_json && !h.hook_symlink && !h.excluded);
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---------- recommend_roster_profile ----------

    fn clis_from(ids: &[&str]) -> BTreeMap<String, CliDetection> {
        let mut m = BTreeMap::new();
        for id in CLI_IDS {
            m.insert(
                id.to_string(),
                CliDetection {
                    installed: ids.contains(&id),
                    path: if ids.contains(&id) {
                        Some(format!("/fake/{}", id))
                    } else {
                        None
                    },
                },
            );
        }
        m
    }

    #[test]
    fn recommend_standalone_without_hc_or_workers() {
        let r = recommend_roster_profile(&clis_from(&["pi", "claude"]));
        assert_eq!(r.mode, SetupMode::Standalone);
        assert_eq!(r.profile_name, None);
        assert!(r.roles.values().all(|v| v == "—"));
        assert_eq!(r.agy, 0);

        let r2 = recommend_roster_profile(&clis_from(&["hc"])); // 워커 0개
        assert_eq!(r2.mode, SetupMode::Standalone);

        let r3 = recommend_roster_profile(&clis_from(&[]));
        assert_eq!(r3.mode, SetupMode::Standalone);
    }

    #[test]
    fn recommend_profile_priority_matrix() {
        let cases: &[(&[&str], &str)] = &[
            (&["hc", "pi", "devin"], "zero-agy"),
            (&["hc", "pi", "devin", "claude", "codex", "agy"], "zero-agy"),
            (&["hc", "claude", "codex"], "claude-codex"),
            (&["hc", "claude"], "claude-only"),
            (&["hc", "codex"], "codex-only"),
            (&["hc", "devin"], "devin-only"),
            (&["hc", "pi"], "pi-only"),
            (&["hc", "gemini"], "gemini-only"),
        ];
        for (ids, expect) in cases {
            let r = recommend_roster_profile(&clis_from(ids));
            assert_eq!(r.mode, SetupMode::MultiCli, "{:?}", ids);
            assert_eq!(r.profile_name.as_deref(), Some(*expect), "{:?}", ids);
            assert_eq!(r.agy, 0, "agy는 절대 자동 추천되지 않는다");
            // 미설치 CLI는 역할에 등장하지 않는다
            let joined = r.roles.values().cloned().collect::<Vec<_>>().join(" ");
            if !ids.contains(&"claude") {
                assert!(!joined.contains("claude"), "{:?} → {:?}", ids, joined);
            }
        }

        // agy만 있는 경우 — 워커 CLI 0개로 간주되어 standalone(TS workerCount 규칙).
        let agy_only = recommend_roster_profile(&clis_from(&["hc", "agy"]));
        assert_eq!(agy_only.mode, SetupMode::Standalone);
    }

    #[test]
    fn recommend_role_labels_carry_readonly_ondemand_tags() {
        let r = recommend_roster_profile(&clis_from(&["hc", "pi", "devin"]));
        assert_eq!(r.roles[&RoleSlot::Lead], "pi");
        assert_eq!(r.roles[&RoleSlot::Coder], "devin");
        assert_eq!(r.roles[&RoleSlot::Explore], "pi (readonly)");
        assert_eq!(r.roles[&RoleSlot::Verifier], "pi (readonly)");
        assert_eq!(r.roles[&RoleSlot::Advisor], "pi (readonly·onDemand)");

        let r2 = recommend_roster_profile(&clis_from(&["hc", "claude"]));
        assert_eq!(r2.roles[&RoleSlot::Coder], "claude");
        assert_eq!(r2.roles[&RoleSlot::Advisor], "claude (readonly·onDemand)");

        let r3 = recommend_roster_profile(&clis_from(&["hc", "codex"]));
        assert_eq!(r3.roles[&RoleSlot::Lead], "codex");
    }

    // ---------- build_tailored_roster_config ----------

    #[test]
    fn tailored_config_builds_known_profiles_only() {
        let rec = recommend_roster_profile(&clis_from(&["hc", "pi", "devin"]));
        let cfg = build_tailored_roster_config(&rec).unwrap();
        assert_eq!(cfg.default_profile, "zero-agy");
        let prof = &cfg.profiles["zero-agy"];
        assert_eq!(prof.daily_turn_budgets.as_ref().unwrap()["agy"], 0);
        assert_eq!(prof.roles[&RoleSlot::Lead].cli, WorkerCliKind::Pi);
        assert_eq!(prof.roles[&RoleSlot::Lead].model.as_deref(), Some("glm-zcode/glm-5.3"));
        assert_eq!(prof.roles[&RoleSlot::Lead].thinking.as_deref(), Some("max"));
        assert_eq!(prof.roles[&RoleSlot::Coder].cli, WorkerCliKind::Devin);
        assert_eq!(prof.roles[&RoleSlot::Coder].permission_mode.as_deref(), Some("auto"));
        assert_eq!(prof.roles[&RoleSlot::Coder].quota_bucket.as_deref(), Some("devin"));

        // block_inherit — 비-pi 스펙의 pi 전용 필드는 빈 문자열로 상속 차단
        let rec2 = recommend_roster_profile(&clis_from(&["hc", "claude", "codex"]));
        let cfg2 = build_tailored_roster_config(&rec2).unwrap();
        let explore = &cfg2.profiles["claude-codex"].roles[&RoleSlot::Explore];
        assert_eq!(explore.cli, WorkerCliKind::Custom);
        assert_eq!(explore.bin.as_deref(), Some("codex"));
        assert_eq!(explore.model.as_deref(), Some(""));
        assert_eq!(explore.provider.as_deref(), Some(""));
        assert_eq!(explore.thinking.as_deref(), Some(""));
        assert_eq!(explore.quota_bucket.as_deref(), Some("codex"));

        // 이름 직접 지정·미지정
        assert!(build_tailored_roster_config_by_name(Some("devin-only")).is_some());
        assert!(build_tailored_roster_config_by_name(Some("no-such-profile")).is_none());
        assert!(build_tailored_roster_config_by_name(None).is_none());
        // standalone 추천은 프로필이 없다
        let sa = recommend_roster_profile(&clis_from(&["pi"]));
        assert!(build_tailored_roster_config(&sa).is_none());
    }

    #[test]
    fn tailored_profile_names_cover_all_builders() {
        for name in TAILORED_PROFILE_NAMES {
            assert!(
                profile_reason_and_build(name).is_some(),
                "builder missing for {}",
                name
            );
        }
        assert!(profile_reason_and_build("bogus").is_none());
    }

    // ---------- classify_model_tier·discover_available_models ----------

    #[test]
    fn model_tier_classification() {
        assert_eq!(classify_model_tier("devin", "anything").0, ModelTier::CodingAgent);
        for fast in ["glm-5.3-flash", "gpt-4o-mini", "claude-haiku-4-5", "nano-x", "exaone-3"] {
            assert_eq!(classify_model_tier("pi", fast).0, ModelTier::Fast, "{}", fast);
        }
        // 경계 — 'gemini' 안 'mini'는 토큰 경계 없어 미매칭
        assert_ne!(classify_model_tier("pi", "gemini-2.5-pro").0, ModelTier::Fast);
        for strong in ["glm-5.3", "gpt-6", "o3-pro", "claude-opus-4", "deepseek-r1", "high-tier"] {
            assert_eq!(classify_model_tier("pi", strong).0, ModelTier::Strong, "{}", strong);
        }
        assert_eq!(classify_model_tier("pi", "some-plain-model").0, ModelTier::Balanced);
        assert_eq!(classify_model_tier("pi", "flash").1, "빠름·저비용");
        assert_eq!(classify_model_tier("claude", "x").1, "균형");
    }

    #[test]
    fn discover_models_reads_local_configs_only() {
        let home = fake_home("models");
        // pi models.json — 문자열·{id} 혼합 + 시크릿 키 혼입(무시되어야 함)
        std::fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        std::fs::write(
            home.join(".pi").join("agent").join("models.json"),
            serde_json::json!({
                "providers": {
                    "opencodex": {
                        "apiKey": "SECRET-VALUE-MUST-NOT-APPEAR",
                        "models": ["glm-zcode/glm-5.3", { "id": "command-code/deepseek-deepseek-v4.1-flash" }, 42]
                    },
                    "empty": { "models": "not-array" },
                    "nullprov": null
                }
            })
            .to_string(),
        )
        .unwrap();
        // codex config.toml
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex").join("config.toml"),
            "# comment\nmodel = \"gpt-5.1-codex\"\nother = 1\nmodel = \"gpt-5.1-codex\"\n",
        )
        .unwrap();
        // claude settings.json
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::write(
            home.join(".claude").join("settings.json"),
            serde_json::json!({ "model": "claude-sonnet-4-6" }).to_string(),
        )
        .unwrap();

        let clis = clis_from(&["pi", "codex", "claude", "devin", "gemini", "agy"]);
        let models = discover_available_models(&home, &clis);
        let find = |cli: DiscoveredCli, model: &str| {
            models
                .iter()
                .find(|m| m.cli == cli && m.model == model)
                .cloned()
        };
        // pi 로컬 설정
        let pi = find(DiscoveredCli::Pi, "glm-zcode/glm-5.3").unwrap();
        assert_eq!(pi.provider.as_deref(), Some("opencodex"));
        assert_eq!(pi.source, ModelSource::LocalConfig);
        assert!(find(DiscoveredCli::Pi, "command-code/deepseek-deepseek-v4.1-flash").is_some());
        // codex toml — 중복 dedupe로 1건
        assert_eq!(
            models
                .iter()
                .filter(|m| m.cli == DiscoveredCli::Codex && m.model == "gpt-5.1-codex")
                .count(),
            1
        );
        // claude 로컬 + 카탈로그 3종 (dedupe: sonnet 로컬=카탈로그 동일 → 1건)
        assert!(find(DiscoveredCli::Claude, "claude-sonnet-4-6").is_some());
        assert!(find(DiscoveredCli::Claude, "claude-opus-4-8").is_some());
        assert!(find(DiscoveredCli::Claude, "claude-haiku-4-5").is_some());
        // devin·gemini 카탈로그
        assert!(find(DiscoveredCli::Devin, "default").is_some());
        assert!(find(DiscoveredCli::Gemini, "gemini-2.5-flash").is_some());
        assert!(find(DiscoveredCli::Gemini, "gemini-2.5-pro").is_some());
        // agy는 절대 포함하지 않는다
        assert!(!models.iter().any(|m| m.model.contains("agy")));
        // 시크릿 값 미노출
        assert!(!format!("{:?}", models).contains("SECRET-VALUE-MUST-NOT-APPEAR"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn discover_models_catalog_fallbacks_when_configs_missing() {
        let home = fake_home("models-fallback");
        let clis = clis_from(&["codex", "claude"]);
        let models = discover_available_models(&home, &clis);
        // config.toml 부재 → 카탈로그 gpt-5-codex
        assert!(
            models
                .iter()
                .any(|m| m.cli == DiscoveredCli::Codex && m.model == "gpt-5-codex" && m.source == ModelSource::Catalog)
        );
        // settings.json 부재 → 카탈로그만
        assert_eq!(
            models
                .iter()
                .filter(|m| m.cli == DiscoveredCli::Claude && m.source == ModelSource::LocalConfig)
                .count(),
            0
        );
        // 미설치 CLI는 0건
        let models2 = discover_available_models(&home, &clis_from(&[]));
        assert!(models2.is_empty());
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---------- recommend_companion_tools ----------

    fn report_for(ids: &[&str], git: bool, companions_installed: &[&str]) -> EnvironmentReport {
        let home = fake_home("companion");
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        if git {
            make_exec(&home.join("bin"), "git");
        }
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: None,
            node_sqlite_ok: None,
            repo_dir: None,
            orchestrator_root: None,
        };
        let mut report = detect_environment(&deps);
        // CLI 감지 결과를 직접 주입(detect는 가짜 bin 없이 standalone이 되므로)
        report.clis = clis_from(ids);
        report.git = CliDetection {
            installed: git,
            path: if git { Some("/usr/bin/git".into()) } else { None },
        };
        report.recommendation = recommend_roster_profile(&report.clis);
        for id in COMPANION_IDS {
            if let Some(c) = report.companions.get_mut(id) {
                c.installed = companions_installed.contains(&id);
            }
        }
        report.companion_advice = recommend_companion_tools(&report);
        let _ = std::fs::remove_dir_all(&home);
        report
    }

    #[test]
    fn companion_advice_verdicts_by_environment() {
        // ① 설치됨 — 전부 already_installed
        let r = report_for(&["hc", "pi", "devin"], true, &["mistology", "neverstuck", "todo-flow", "effort-router"]);
        assert_eq!(r.companion_advice.len(), 4);
        assert!(r.companion_advice.iter().all(|a| a.verdict == CompanionVerdict::AlreadyInstalled));
        assert!(r.companion_advice.iter().all(|a| a.install_command.is_none()));

        // ② git 있음 + 코딩 CLI 있음(단일) — mistology 추천(cargo 없으면 수동)
        let r2 = report_for(&["pi"], true, &[]);
        let mist = r2.companion_advice.iter().find(|a| a.id == "mistology").unwrap();
        assert_eq!(mist.verdict, CompanionVerdict::Recommended);
        assert!(!mist.can_auto_install); // cargo 미설치
        assert!(mist.install_guide.contains("Rust(rustup)"));
        let ns = r2.companion_advice.iter().find(|a| a.id == "neverstuck").unwrap();
        assert_eq!(ns.verdict, CompanionVerdict::Recommended);
        let tf = r2.companion_advice.iter().find(|a| a.id == "todo-flow").unwrap();
        assert_eq!(tf.verdict, CompanionVerdict::Optional); // coding 1개 + 비멀티
        let er = r2.companion_advice.iter().find(|a| a.id == "effort-router").unwrap();
        assert_eq!(er.verdict, CompanionVerdict::Recommended); // 단일 세션 중심

        // ③ git 없음 + 코딩 CLI 없음 — mistology·neverstuck optional, effort-router skip
        let r3 = report_for(&[], false, &[]);
        assert_eq!(
            r3.companion_advice.iter().find(|a| a.id == "mistology").unwrap().verdict,
            CompanionVerdict::Optional
        );
        // 코딩 CLI 없음 — neverstuck optional, effort-router skip_overlap
        assert_eq!(
            r3.companion_advice.iter().find(|a| a.id == "neverstuck").unwrap().verdict,
            CompanionVerdict::Optional
        );
        assert_eq!(
            r3.companion_advice.iter().find(|a| a.id == "effort-router").unwrap().verdict,
            CompanionVerdict::SkipOverlap
        );

        // ④ 멀티 CLI — todo-flow 추천, effort-router는 중복 생략(optional)
        let r4 = report_for(&["hc", "pi", "devin"], true, &[]);
        assert_eq!(
            r4.companion_advice.iter().find(|a| a.id == "todo-flow").unwrap().verdict,
            CompanionVerdict::Recommended
        );
        assert_eq!(
            r4.companion_advice.iter().find(|a| a.id == "effort-router").unwrap().verdict,
            CompanionVerdict::Optional
        );

        // ⑤ git + cargo — mistology 자동 설치 가능
        let r5 = report_for(&["hc", "pi", "cargo"], true, &[]);
        let mist5 = r5.companion_advice.iter().find(|a| a.id == "mistology").unwrap();
        assert!(mist5.can_auto_install);
        assert_eq!(mist5.install_command.as_deref(), Some(MISTOLOGY_INSTALL_CMD));
    }

    #[test]
    fn companion_advice_carries_verified_repo_urls() {
        let r = report_for(&["hc", "pi"], true, &[]);
        let expect: [(&str, &str, &str); 4] = [
            ("mistology", "chldbwnstm/mistology", "https://github.com/chldbwnstm/mistology"),
            ("neverstuck", "chldbwnstm/NeverStuck", "https://github.com/chldbwnstm/NeverStuck"),
            ("todo-flow", "JakeB-5/todo-flow", "https://github.com/JakeB-5/todo-flow"),
            ("effort-router", "acc0mplish/Effort-Router", "https://github.com/acc0mplish/Effort-Router"),
        ];
        for (id, slug, url) in expect {
            let a = r.companion_advice.iter().find(|a| a.id == id).unwrap();
            assert_eq!(a.repo_slug, slug);
            assert_eq!(a.repo_url, url);
        }
        assert_eq!(SELF_REPO_SLUG, "insightflo/dgate");
        assert_eq!(SELF_REPO_URL, "https://github.com/insightflo/dgate");
    }

    // ---------- install_skill_links ----------

    #[test]
    fn skill_links_create_skip_and_conflict() {
        let home = fake_home("skill-links");
        let repo = home.join("orch");
        std::fs::create_dir_all(repo.join("skills").join("dgate")).unwrap();
        std::fs::write(repo.join("skills").join("dgate").join("SKILL.md"), "# dgate").unwrap();
        std::fs::create_dir_all(home.join(".claude")).unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();

        let opts = InstallSkillLinksOptions {
            home_dir: Some(home.clone()),
            repo_dir: Some(repo.clone()),
            only_agent_ids: Some(vec![
                "claude".to_string(),
                "codex".to_string(),
                "gemini".to_string(),
                "pi".to_string(),
            ]),
        };
        let res = install_skill_links(&opts);
        // claude — 링크 생성
        assert!(res.linked.contains(&home.join(".claude").to_string_lossy().into_owned()));
        // codex — 링크 생성
        assert!(res.linked.contains(&home.join(".codex").to_string_lossy().into_owned()));
        // gemini — 에이전트 디렉터리 부재 스킵
        assert!(
            res.skipped
                .iter()
                .any(|s| s.dir == home.join(".gemini").to_string_lossy().into_owned() && s.reason == "agent-dir-absent")
        );
        // 실제 symlink 검증
        let link = std::fs::read_link(home.join(".claude").join("skills").join("dgate")).unwrap();
        assert_eq!(link, repo.join("skills").join("dgate"));

        // 재실행 — already-linked 멱등
        let res2 = install_skill_links(&opts);
        assert!(res2.linked.is_empty());
        assert!(
            res2.skipped
                .iter()
                .any(|s| s.dir == home.join(".claude").to_string_lossy().into_owned() && s.reason == "already-linked")
        );

        // 충돌 — 미사용 에이전트 슬롯에 실제 디렉터리가 이미 있으면 절대 덮어쓰지 않는다
        std::fs::create_dir_all(home.join(".pi").join("agent").join("skills").join("dgate")).unwrap();
        std::fs::write(
            home.join(".pi").join("agent").join("skills").join("dgate").join("SKILL.md"),
            "mine",
        )
        .unwrap();
        let res3 = install_skill_links(&opts);
        assert!(
            res3.skipped.iter().any(|s|
                s.dir == home.join(".pi").join("agent").to_string_lossy().into_owned()
                    && s.reason == "conflict-existing-path")
        );
        assert_eq!(
            std::fs::read_to_string(home.join(".pi").join("agent").join("skills").join("dgate").join("SKILL.md")).unwrap(),
            "mine"
        );
        // 소스 오염 없음 확인
        assert_eq!(
            std::fs::read_to_string(repo.join("skills").join("dgate").join("SKILL.md")).unwrap(),
            "# dgate"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn skill_links_missing_source_skips_all() {
        let home = fake_home("skill-missing");
        let repo = home.join("orch"); // skills/dgate 부재
        std::fs::create_dir_all(&repo).unwrap();
        let res = install_skill_links(&InstallSkillLinksOptions {
            home_dir: Some(home.clone()),
            repo_dir: Some(repo.clone()),
            only_agent_ids: None,
        });
        assert!(res.linked.is_empty());
        assert_eq!(res.skipped.len(), AGENT_SKILL_DIRS.len());
        assert!(res.skipped.iter().all(|s| s.reason == "source-skill-missing"));
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---------- install_repo_hooks·inspect ----------

    fn orch_fixture(home: &Path) -> PathBuf {
        let orch = home.join("orch");
        std::fs::create_dir_all(orch.join(".agents").join("hooks")).unwrap();
        std::fs::write(orch.join(".agents").join("hooks.json"), "{\"canonical\":true}").unwrap();
        std::fs::write(orch.join(".agents").join("hooks").join("dg-hook.ts"), "// hook").unwrap();
        orch
    }

    fn init_git_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(dir)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false));
    }

    #[test]
    fn repo_hooks_install_is_idempotent_and_safe() {
        let home = fake_home("hooks-install");
        let orch = orch_fixture(&home);
        let target = home.join("target");
        init_git_repo(&target);

        let res = install_repo_hooks(&target, Some(&orch));
        assert_eq!(res.written.len(), 3, "hooks.json + symlink + exclude");
        assert!(res.written.iter().any(|p| p.ends_with(".agents/hooks.json")));
        assert!(res.written.iter().any(|p| p.ends_with("dg-hook.ts")));
        assert!(res.written.iter().any(|p| p.ends_with(".git/info/exclude")));
        // 심볼릭 링크 대상
        let link = std::fs::read_link(target.join(".agents").join("hooks").join("dg-hook.ts")).unwrap();
        assert_eq!(link, orch.join(".agents").join("hooks").join("dg-hook.ts"));
        // exclude 내용
        let exclude = std::fs::read_to_string(target.join(".git").join("info").join("exclude")).unwrap();
        assert!(exclude.contains(".agents/hooks.json"));
        assert!(exclude.contains(".agents/hooks/"));

        // 재실행 — 전부 already-* 멱등
        let res2 = install_repo_hooks(&target, Some(&orch));
        assert!(res2.written.is_empty());
        assert!(res2.skipped.iter().any(|s| s.reason == "already-present"));
        assert!(res2.skipped.iter().any(|s| s.reason == "already-linked"));
        assert!(res2.skipped.iter().any(|s| s.reason == "already-excluded"));

        // 다른 내용의 hooks.json은 절대 덮어쓰지 않는다
        std::fs::write(target.join(".agents").join("hooks.json"), "{\"mine\":1}").unwrap();
        let res3 = install_repo_hooks(&target, Some(&orch));
        assert!(res3.skipped.iter().any(|s| s.reason == "conflict-different-content"));
        assert_eq!(
            std::fs::read_to_string(target.join(".agents").join("hooks.json")).unwrap(),
            "{\"mine\":1}"
        );
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn repo_hooks_missing_source_and_non_git() {
        let home = fake_home("hooks-edge");
        // 소스 부재
        let empty_orch = home.join("empty-orch");
        std::fs::create_dir_all(&empty_orch).unwrap();
        let target = home.join("t1");
        std::fs::create_dir_all(&target).unwrap();
        let res = install_repo_hooks(&target, Some(&empty_orch));
        assert_eq!(res.skipped.len(), 1);
        assert_eq!(res.skipped[0].reason, "orchestrator-hook-source-missing");

        // git 저장소 아님 — exclude 스킵
        let orch = orch_fixture(&home);
        let nongit = home.join("t2");
        std::fs::create_dir_all(&nongit).unwrap();
        let res2 = install_repo_hooks(&nongit, Some(&orch));
        assert!(res2.written.iter().any(|p| p.ends_with("hooks.json")));
        assert!(res2.skipped.iter().any(|s| s.reason == "not-a-git-repo"));

        // inspect — 설치 후 상태 true
        let st = inspect_repo_hooks(&nongit, &orch);
        assert!(st.hooks_json && st.hook_symlink && !st.excluded);
        let _ = std::fs::remove_dir_all(&home);
    }

    // ---------- star_github_repo ----------

    #[test]
    fn star_rejects_invalid_slug() {
        let r = star_github_repo_with(
            "not a slug!!",
            Box::new(|_, _| panic!("exec must not run for invalid slug")),
        );
        assert!(!r.ok);
        assert_eq!(r.reason.as_deref(), Some("invalid-repo-slug"));
        assert_eq!(r.fallback_url, "https://github.com/not a slug!!");
        for bad in ["nodash", "a/b/c", "", "a/", "/b"] {
            let r2 = star_github_repo_with(bad, Box::new(|_, _| panic!("no exec")));
            assert!(!r2.ok, "{}", bad);
        }
    }

    #[test]
    fn star_uses_argv_array_put_and_maps_results() {
        use std::sync::{Arc, Mutex};
        let calls: Arc<Mutex<Vec<(String, Vec<String>)>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = calls.clone();

        // 성공 — argv 배열 검증
        let ok = star_github_repo_with(
            "insightflo/dgate",
            Box::new(move |file, args| {
                sink.lock().unwrap().push((file.to_string(), args.to_vec()));
                GhCommandOutput {
                    error: None,
                    status: Some(0),
                    stdout: String::new(),
                    stderr: String::new(),
                }
            }),
        );
        assert!(ok.ok);
        assert_eq!(ok.reason, None);
        assert_eq!(ok.fallback_url, "https://github.com/insightflo/dgate");
        let recorded = calls.lock().unwrap();
        assert_eq!(recorded.len(), 1);
        assert_eq!(recorded[0].0, "gh");
        assert_eq!(
            recorded[0].1,
            vec!["api", "-X", "PUT", "user/starred/insightflo/dgate"]
        );

        // gh 부재(ENOENT)
        let enoent = star_github_repo_with(
            "a/b",
            Box::new(|_, _| GhCommandOutput {
                error: Some("ENOENT".into()),
                status: None,
                stdout: String::new(),
                stderr: String::new(),
            }),
        );
        assert!(!enoent.ok);
        assert_eq!(enoent.reason.as_deref(), Some("gh-not-installed"));

        // 404 — 비공개·미존재 저장소
        let notfound = star_github_repo_with(
            "a/private",
            Box::new(|_, _| GhCommandOutput {
                error: None,
                status: Some(1),
                stdout: String::new(),
                stderr: "404 Not Found".into(),
            }),
        );
        assert!(!notfound.ok);
        assert_eq!(notfound.reason.as_deref(), Some("repo-not-found-or-private"));

        // 기타 비정상 종료
        let fail = star_github_repo_with(
            "a/b",
            Box::new(|_, _| GhCommandOutput {
                error: None,
                status: Some(2),
                stdout: String::new(),
                stderr: "boom".into(),
            }),
        );
        assert!(!fail.ok);
        assert_eq!(fail.reason.as_deref(), Some("gh-exit-2"));

        // 기타 spawn 오류
        let spawn_err = star_github_repo_with(
            "a/b",
            Box::new(|_, _| GhCommandOutput {
                error: Some("EACCES".into()),
                status: None,
                stdout: String::new(),
                stderr: String::new(),
            }),
        );
        assert!(!spawn_err.ok);
        assert_eq!(spawn_err.reason.as_deref(), Some("gh-spawn-error:EACCES"));
    }

    #[test]
    fn star_never_runs_for_invalid_input_and_real_path_is_opt_in() {
        // 실제 spawn은 opt-in 전용 — 단위 테스트는 슬러그 검증 게이트만 확인한다
        // (유효 슬러그로 실 gh 실행·스타 등록 부수효과를 내지 않는다).
        let r = star_github_repo("bad slug!!");
        assert!(!r.ok);
        assert_eq!(r.reason.as_deref(), Some("invalid-repo-slug"));
        assert_eq!(r.fallback_url, "https://github.com/bad slug!!");
    }

    // ---------- format_check_report ----------

    #[test]
    fn check_report_renders_all_sections() {
        let home = fake_home("report");
        let orch = orch_fixture(&home);
        make_exec(&home.join("bin"), "git");
        make_exec(&home.join("bin"), "hc");
        make_exec(&home.join("bin"), "pi");
        make_exec(&home.join("bin"), "devin");
        std::fs::create_dir_all(home.join(".claude").join("skills").join("dgate")).unwrap();
        std::fs::write(home.join(".claude").join("skills").join("dgate").join("SKILL.md"), "").unwrap();
        // pi 로컬 모델 카탈로그 — 발견 모델 섹션 렌더링용
        std::fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        std::fs::write(
            home.join(".pi").join("agent").join("models.json"),
            serde_json::json!({
                "providers": { "opencodex": { "models": ["glm-zcode/glm-5.3"] } }
            })
            .to_string(),
        )
        .unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: None,
            node_sqlite_ok: None,
            repo_dir: Some(home.join("target")),
            orchestrator_root: Some(orch.clone()),
        };
        let report = detect_environment(&deps);
        let text = format_check_report(&report);

        assert!(text.starts_with("[setup --check] dgate 환경 진단"));
        assert!(text.contains("필수 런타임:"));
        assert!(text.contains("node:sqlite"));
        assert!(text.contains("코딩 CLI (선택):"));
        assert!(text.contains(" hc "));
        assert!(text.contains(" — 감지됨, 쿼터 한도로 기본 미선택") || !report.clis["agy"].installed);
        assert!(text.contains("사용 가능 모델"));
        assert!(text.contains("[pi] glm-zcode/glm-5.3 — 고성능·추론 · opencodex · 로컬 설정"));
        assert!(text.contains("에이전트 스킬 디렉터리:"));
        assert!(text.contains("(dgate 링크됨)"));
        assert!(text.contains("설정·상태:"));
        assert!(text.contains("전역 roster"));
        assert!(text.contains("결정 DB"));
        assert!(text.contains(DECISION_API_KEY_ENV));
        assert!(text.contains("(미설정 — 기본 off, 무료 로컬 통계)"));
        assert!(text.contains("선택 도구·스킬:"));
        assert!(text.contains("동반 스킬 환경 비교 추천:"));
        assert!(text.contains("◆ 추천 mistology"));
        assert!(text.contains("https://github.com/chldbwnstm/mistology"));
        assert!(text.contains("→ 지금 자동 설치 가능:") || !report.clis["cargo"].installed);
        assert!(text.contains("추천 모드: multi-cli / 프로필 \"zero-agy\" (agy: 0)"));
        assert!(text.contains("→ pi (readonly)"));
        assert!(text.contains("(권장: 빠름·저비용)"));
        assert!(text.contains("대상 프로젝트 훅 상태"));
        assert!(text.contains(".git/info/exclude 항목 없음"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn check_report_standalone_and_model_overflow() {
        let home = fake_home("report2");
        make_exec(&home.join("bin"), "git");
        make_exec(&home.join("bin"), "pi");
        // discover용 로컬 카탈로그만 있는 gemini
        std::fs::create_dir_all(home.join(".pi").join("agent")).unwrap();
        let providers: serde_json::Map<String, Value> = (0..15)
            .map(|i| {
                (
                    format!("p{}", i),
                    serde_json::json!({ "models": [format!("m-{}-flash", i)] }),
                )
            })
            .collect();
        std::fs::write(
            home.join(".pi").join("agent").join("models.json"),
            serde_json::json!({ "providers": Value::Object(providers) }).to_string(),
        )
        .unwrap();

        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), home.join("bin").to_string_lossy().into_owned());
        let deps = DetectDeps {
            env: Some(env),
            home_dir: Some(home.clone()),
            path_dirs: None,
            system_dirs: Some(vec![home.join("sysbin")]),
            node_version: Some("v20.1.0".to_string()),
            node_sqlite_ok: Some(false),
            repo_dir: None,
            orchestrator_root: None,
        };
        let report = detect_environment(&deps);
        let text = format_check_report(&report);
        assert!(text.contains("✗ node v20.1.0"));
        assert!(text.contains("✗ node:sqlite"));
        assert!(text.contains("추천 모드: standalone —"));
        // 모델 12개 상한 + "… 외 N종"
        assert!(text.contains("… 외 3종"), "{}", text.lines().take(30).collect::<Vec<_>>().join("\n"));
        let _ = std::fs::remove_dir_all(&home);
    }

    #[test]
    fn deps_default_falls_back_to_process_env() {
        // 주입 없이도 크래시 없이 동작(운용 경로)
        let _g: MutexGuard<'_, ()> = env_lock();
        let report = detect_environment(&DetectDeps::default());
        let _ = format_check_report(&report);
        assert!(!report.roster.path.is_empty());
    }
}
