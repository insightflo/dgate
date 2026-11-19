//! Git-Native 워크트리 체크포인트 + 삭제 게이트 — `src/decision/checkpoint.ts` (PB-2B) 포팅.
//!
//! 목적: `git worktree` 기반 병렬 워커의 작업 손실 0을 보장한다. staged/unstaged
//! `git diff --binary` 패치와 untracked 원본 복사를 `.herdr/checkpoints/<run_id>/<wt_name>/`
//! 에 스냅숏하고, sha256 manifest로 무결성을 검증한다. [`safe_remove_worktree`]는
//! "체크포인트 저장 성공 + 검증 valid"를 통과하기 전에는 `git worktree remove` 호출에
//! 물리적으로 도달할 수 없는 삭제 게이트다(우회 플래그 없음 — 절대 불변식).
//!
//! 주의:
//! - 외부 의존성 0: git CLI·sha2·std fs만 사용. 압축 아카이브 금지 — Git 네이티브 패치 + 원본 파일.
//! - untracked 파일당·총합 용량이 [`MAX_CHECKPOINT_BYTES`](12MiB)를 초과하면 저장 실패를
//!   반환한다(조용한 건너뛰기 금지 — 무손실 복원 보장이 우선).
//! - untracked 경로 순회(`..`)·제외 대상(`.git`,`node_modules`,`.herdr`,`.verify`,`*.db*`)는
//!   save·verify·restore 양쪽에서 동일 규칙으로 엄격 차단한다.
//! - 심볼릭 링크는 링크 대상 경로 문자열을 텍스트로 저장한다(따라가서 복사하지 않는다).
//! - manifest 파일 항목은 언제나 일반 파일로 저장하며, verify는 lstat으로 일반 파일
//!   여부까지 대조한다(검증 대상 파일을 심볼릭 링크로 바꿔치기하는 변조 차단).

use std::fs;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// untracked 복사 용량 상한 — 파일당·총합 공용 (brief §3.A.1)
pub const MAX_CHECKPOINT_BYTES: u64 = 12 * 1024 * 1024;

/// 체크포인트 디렉터리명(run_id·wt_name) 허용 문자셋 — 경로 탈출 원천 차단
const SAFE_NAME_RE: &str = r"^[A-Za-z0-9][A-Za-z0-9._-]*$";

/// untracked 제외 세그먼트 — save·verify·restore가 같은 목록을 쓴다
const EXCLUDED_SEGMENTS: [&str; 4] = [".git", "node_modules", ".herdr", ".verify"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointFileKind {
    IndexPatch,
    WorktreePatch,
    Untracked,
}

impl CheckpointFileKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            CheckpointFileKind::IndexPatch => "index_patch",
            CheckpointFileKind::WorktreePatch => "worktree_patch",
            CheckpointFileKind::Untracked => "untracked",
        }
    }
    pub fn parse(v: &str) -> Option<Self> {
        match v {
            "index_patch" => Some(CheckpointFileKind::IndexPatch),
            "worktree_patch" => Some(CheckpointFileKind::WorktreePatch),
            "untracked" => Some(CheckpointFileKind::Untracked),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointFileEntry {
    /// 체크포인트 디렉터리 기준 상대 경로 (`index.patch` | `worktree.patch` | `untracked/<rel>`)
    pub relpath: String,
    pub kind: CheckpointFileKind,
    pub bytes: u64,
    pub sha256: String,
    /// 복원 시 chmod용 권한(8진수를 10진수로). 심볼릭 링크는 None
    #[serde(default)]
    pub mode: Option<u32>,
    /// true면 저장 내용이 심볼릭 링크 대상 경로 문자열이다
    #[serde(default)]
    pub symlink: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckpointManifest {
    pub schema_version: i64,
    pub created_at_ms: i64,
    pub run_id: String,
    pub worktree_name: String,
    pub worktree_path: String,
    pub branch: String,
    pub head_sha: String,
    pub files: Vec<CheckpointFileEntry>,
    /// files 정규화 JSON(경로 오름차순·키 고정)의 sha256 — manifest 자기 자신은 제외
    pub manifest_digest: String,
}

pub struct CreateCheckpointInput<'a> {
    pub repo_dir: &'a Path,
    pub worktree_dir: &'a Path,
    pub run_id: &'a str,
    /// 미지정 시 worktree_dir의 basename
    pub worktree_name: Option<&'a str>,
    /// 기본 `<repo_dir>/.herdr/checkpoints`
    pub checkpoint_root: Option<&'a Path>,
}

#[derive(Debug, Clone, Default)]
pub struct CheckpointResult {
    pub saved: bool,
    pub checkpoint_dir: Option<PathBuf>,
    pub manifest: Option<CheckpointManifest>,
    pub errors: Vec<String>,
}

#[derive(Debug, Clone, Default)]
pub struct VerifyCheckpointResult {
    pub valid: bool,
    pub errors: Vec<String>,
    pub manifest: Option<CheckpointManifest>,
}

#[derive(Debug, Clone, Default)]
pub struct RestoreCheckpointResult {
    pub restored: bool,
    pub errors: Vec<String>,
}

pub struct SafeRemoveInput<'a> {
    pub repo_dir: &'a Path,
    pub worktree_dir: &'a Path,
    pub run_id: &'a str,
    pub worktree_name: Option<&'a str>,
    pub checkpoint_root: Option<&'a Path>,
    /// true면 `git worktree remove --force`(dirty worktree 허용). 체크포인트 검증에는 영향 없다
    pub force_git_remove: bool,
}

#[derive(Debug, Clone, Default)]
pub struct SafeRemoveResult {
    pub removed: bool,
    /// 게이트 거부는 4, 정상 삭제는 0, git 실패는 git 종료코드
    pub exit_code: i32,
    pub checkpoint_dir: Option<PathBuf>,
    pub errors: Vec<String>,
}

// ---------- 경로 안전성 헬퍼 (save·verify·restore 공용) ----------

fn safe_name_re() -> &'static regex::Regex {
    static RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| regex::Regex::new(SAFE_NAME_RE).expect("SAFE_NAME_RE 컴파일 실패"))
}

/// 상대 경로 구조 검증 — 빈 세그먼트·`.`·`..`·백슬래시·NUL·절대경로 거부
fn is_safe_rel_path(rel: &str) -> bool {
    if rel.is_empty() || rel.len() > 1024 {
        return false;
    }
    if rel.starts_with('/') || rel.contains('\\') || rel.contains('\0') {
        return false;
    }
    rel.split('/').all(|s| !s.is_empty() && s != "." && s != "..")
}

/// 제외 대상: 금지 세그먼트(`.git` 등) 또는 `*.db*` basename
fn is_excluded_untracked_rel(rel: &str) -> bool {
    let segs: Vec<&str> = rel.split('/').collect();
    if segs.iter().any(|s| EXCLUDED_SEGMENTS.contains(s)) {
        return true;
    }
    segs.last().map(|b| b.contains(".db")).unwrap_or(false)
}

fn sha256_bytes(buf: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(buf);
    h.finalize().iter().map(|b| format!("{:02x}", b)).collect()
}

/// JSON 문자열 리터럴 — `JSON.stringify`와 동일 이스케이프(serde_json 위임)
fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".to_string())
}

/// manifest digest SSOT — files를 relpath·kind 오름차순 정렬 + 키 고정 직렬화 후 sha256.
/// TS `computeManifestDigest`와 바이트 단위 동일 결과를 만든다(교차 검증 호환).
pub fn compute_manifest_digest(files: &[CheckpointFileEntry]) -> String {
    let mut canonical: Vec<&CheckpointFileEntry> = files.iter().collect();
    canonical.sort_by(|a, b| a.relpath.cmp(&b.relpath).then_with(|| a.kind.as_str().cmp(b.kind.as_str())));
    let parts: Vec<String> = canonical
        .iter()
        .map(|f| {
            format!(
                "{{\"relpath\":{},\"kind\":{},\"bytes\":{},\"sha256\":{},\"mode\":{},\"symlink\":{}}}",
                json_str(&f.relpath),
                json_str(f.kind.as_str()),
                f.bytes,
                json_str(&f.sha256),
                match f.mode {
                    Some(m) => m.to_string(),
                    None => "null".to_string(),
                },
                f.symlink
            )
        })
        .collect();
    sha256_bytes(format!("[{}]", parts.join(",")).as_bytes())
}

struct GitOut {
    ok: bool,
    stdout: Vec<u8>,
    stderr: String,
}

fn git_run(cwd: &Path, args: &[&str]) -> GitOut {
    let res = Command::new("git").arg("-C").arg(cwd).args(args).output();
    match res {
        Ok(out) => GitOut {
            ok: out.status.success(),
            stdout: out.stdout,
            stderr: format!(
                "{}{}",
                String::from_utf8_lossy(&out.stderr),
                ""
            )
            .trim()
            .to_string(),
        },
        Err(e) => GitOut { ok: false, stdout: Vec::new(), stderr: e.to_string() },
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------- B4 저장: create_worktree_checkpoint ----------

/// 워크트리 체크포인트 저장. 실패 시 부분 산출물을 남기지 않는다(tmp 정리 + errors 반환).
/// 성공은 "manifest까지 기록 + 자기검증(verify) 통과 + tmp→최종 rename 완료"를 의미한다.
pub fn create_worktree_checkpoint(input: &CreateCheckpointInput) -> CheckpointResult {
    let run_id = input.run_id;
    if !safe_name_re().is_match(run_id) || run_id == "." || run_id == ".." {
        return CheckpointResult {
            saved: false,
            errors: vec![format!("invalid run_id (charset [A-Za-z0-9._-] required): {}", run_id)],
            ..Default::default()
        };
    }
    let worktree_dir = fs::canonicalize(input.worktree_dir).unwrap_or_else(|_| input.worktree_dir.to_path_buf());
    let worktree_meta = match fs::metadata(&worktree_dir) {
        Ok(m) => m,
        Err(_) => {
            return CheckpointResult {
                saved: false,
                errors: vec![format!("worktree not found: {}", worktree_dir.display())],
                ..Default::default()
            }
        }
    };
    if !worktree_meta.is_dir() {
        return CheckpointResult {
            saved: false,
            errors: vec![format!("worktree is not a directory: {}", worktree_dir.display())],
            ..Default::default()
        };
    }
    let wt_name = input
        .worktree_name
        .map(str::to_string)
        .unwrap_or_else(|| worktree_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
    if !safe_name_re().is_match(&wt_name) || wt_name == "." || wt_name == ".." {
        return CheckpointResult {
            saved: false,
            errors: vec![format!("invalid worktree_name (charset [A-Za-z0-9._-] required): {}", wt_name)],
            ..Default::default()
        };
    }
    let repo_dir = fs::canonicalize(input.repo_dir).unwrap_or_else(|_| input.repo_dir.to_path_buf());
    let root = match input.checkpoint_root {
        Some(r) => fs::canonicalize(r).unwrap_or_else(|_| r.to_path_buf()),
        None => repo_dir.join(".herdr").join("checkpoints"),
    };
    let final_dir = root.join(run_id).join(&wt_name);
    let mut rnd = [0u8; 6];
    rand::thread_rng().fill_bytes(&mut rnd);
    let tmp_suffix: String = rnd.iter().map(|b| format!("{:02x}", b)).collect();
    let tmp_dir = root.join(run_id).join(format!(".tmp-{}", tmp_suffix));

    let outcome = create_checkpoint_inner(&worktree_dir, &repo_dir, &root, run_id, &wt_name, &final_dir, &tmp_dir);
    if outcome.saved {
        outcome
    } else {
        // 실패 경로의 부분 산출물만 정리 — rename 성공 시 tmp는 이미 없다
        let _ = fs::remove_dir_all(&tmp_dir);
        outcome
    }
}

#[allow(clippy::too_many_arguments)]
fn create_checkpoint_inner(
    worktree_dir: &Path,
    _repo_dir: &Path,
    _root: &Path,
    run_id: &str,
    wt_name: &str,
    final_dir: &Path,
    tmp_dir: &Path,
) -> CheckpointResult {
    let head = git_run(worktree_dir, &["rev-parse", "HEAD"]);
    if !head.ok {
        return CheckpointResult { saved: false, errors: vec![format!("git rev-parse HEAD failed: {}", head.stderr)], ..Default::default() };
    }
    let head_sha = String::from_utf8_lossy(&head.stdout).trim().to_string();
    let branch_res = git_run(worktree_dir, &["rev-parse", "--abbrev-ref", "HEAD"]);
    if !branch_res.ok {
        return CheckpointResult {
            saved: false,
            errors: vec![format!("git rev-parse --abbrev-ref failed: {}", branch_res.stderr)],
            ..Default::default()
        };
    }
    let branch = String::from_utf8_lossy(&branch_res.stdout).trim().to_string();

    let index_res = git_run(worktree_dir, &["diff", "--cached", "--binary"]);
    if !index_res.ok {
        return CheckpointResult { saved: false, errors: vec![format!("git diff --cached failed: {}", index_res.stderr)], ..Default::default() };
    }
    let worktree_res = git_run(worktree_dir, &["diff", "--binary"]);
    if !worktree_res.ok {
        return CheckpointResult { saved: false, errors: vec![format!("git diff failed: {}", worktree_res.stderr)], ..Default::default() };
    }
    let index_patch = index_res.stdout;
    let worktree_patch = worktree_res.stdout;

    // untracked 원본 수집 계획 — 전부 검증한 뒤에만 디스크에 쓴다(부분 산출물 방지)
    let ls = git_run(worktree_dir, &["ls-files", "--others", "--exclude-standard", "-z"]);
    if !ls.ok {
        return CheckpointResult { saved: false, errors: vec![format!("git ls-files --others failed: {}", ls.stderr)], ..Default::default() };
    }
    let untracked_raw = String::from_utf8_lossy(&ls.stdout).into_owned();
    let untracked_rels: Vec<String> = untracked_raw.split('\0').filter(|s| !s.is_empty()).map(str::to_string).collect();

    let mut errors: Vec<String> = Vec::new();
    struct CopyItem {
        rel: String,
        symlink: bool,
        content: Vec<u8>,
        mode: Option<u32>,
    }
    let mut copy_plan: Vec<CopyItem> = Vec::new();
    let mut total_bytes = index_patch.len() as u64 + worktree_patch.len() as u64;
    for rel in &untracked_rels {
        if !is_safe_rel_path(rel) {
            errors.push(format!("unsafe untracked path rejected: {}", rel));
            continue;
        }
        if is_excluded_untracked_rel(rel) {
            errors.push(format!("excluded untracked path rejected (checkpoint would be lossy): {}", rel));
            continue;
        }
        let src_abs = worktree_dir.join(rel);
        let st = match fs::symlink_metadata(&src_abs) {
            Ok(s) => s,
            Err(e) => {
                errors.push(format!("untracked lstat failed ({}): {}", rel, e));
                continue;
            }
        };
        let (symlink, content, mode) = if st.file_type().is_symlink() {
            let target = fs::read_link(&src_abs)
                .map_err(|e| format!("untracked readlink failed ({}): {}", rel, e))
                .map(|p| p.to_string_lossy().into_owned().into_bytes());
            match target {
                Ok(c) => (true, c, None::<u32>),
                Err(e) => {
                    errors.push(e);
                    continue;
                }
            }
        } else if st.is_file() {
            if st.len() > MAX_CHECKPOINT_BYTES {
                errors.push(format!("untracked file exceeds MAX_CHECKPOINT_BYTES ({} > {}): {}", st.len(), MAX_CHECKPOINT_BYTES, rel));
                continue;
            }
            match fs::read(&src_abs) {
                Ok(c) => (false, c, Some(st.mode() & 0o777)),
                Err(e) => {
                    errors.push(format!("untracked read failed ({}): {}", rel, e));
                    continue;
                }
            }
        } else {
            // ls-files은 파일만 열거하므로 여기엔 도달하지 않는다 — 이상 상태는 건너뛰지 않고 오류로
            errors.push(format!("untracked entry is not a regular file/symlink: {}", rel));
            continue;
        };
        if content.len() as u64 > MAX_CHECKPOINT_BYTES {
            errors.push(format!("untracked file exceeds MAX_CHECKPOINT_BYTES ({} > {}): {}", content.len(), MAX_CHECKPOINT_BYTES, rel));
            continue;
        }
        total_bytes += content.len() as u64;
        if total_bytes > MAX_CHECKPOINT_BYTES {
            errors.push(format!("checkpoint total size exceeds MAX_CHECKPOINT_BYTES ({} > {})", total_bytes, MAX_CHECKPOINT_BYTES));
            break;
        }
        copy_plan.push(CopyItem { rel: rel.clone(), symlink, content, mode });
    }
    if index_patch.len() as u64 > MAX_CHECKPOINT_BYTES {
        errors.push(format!("index.patch exceeds MAX_CHECKPOINT_BYTES ({})", index_patch.len()));
    }
    if worktree_patch.len() as u64 > MAX_CHECKPOINT_BYTES {
        errors.push(format!("worktree.patch exceeds MAX_CHECKPOINT_BYTES ({})", worktree_patch.len()));
    }
    if !errors.is_empty() {
        return CheckpointResult { saved: false, errors, ..Default::default() };
    }

    // tmp 디렉터리에 전부 기록 → manifest → 자기검증 → 원자적 rename
    if let Err(e) = fs::create_dir_all(tmp_dir) {
        return CheckpointResult { saved: false, errors: vec![format!("checkpoint failed: {}", e)], ..Default::default() };
    }
    let written = (|| -> Result<CheckpointManifest, CheckpointResult> {
        fs::write(tmp_dir.join("index.patch"), &index_patch).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?;
        fs::write(tmp_dir.join("worktree.patch"), &worktree_patch).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?;
        let mut entries = vec![
            CheckpointFileEntry {
                relpath: "index.patch".to_string(),
                kind: CheckpointFileKind::IndexPatch,
                bytes: index_patch.len() as u64,
                sha256: sha256_bytes(&index_patch),
                mode: None,
                symlink: false,
            },
            CheckpointFileEntry {
                relpath: "worktree.patch".to_string(),
                kind: CheckpointFileKind::WorktreePatch,
                bytes: worktree_patch.len() as u64,
                sha256: sha256_bytes(&worktree_patch),
                mode: None,
                symlink: false,
            },
        ];
        for item in &copy_plan {
            let dest = tmp_dir.join("untracked").join(&item.rel);
            if let Some(parent) = dest.parent() {
                fs::create_dir_all(parent).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?;
            }
            fs::write(&dest, &item.content).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?;
            entries.push(CheckpointFileEntry {
                relpath: format!("untracked/{}", item.rel),
                kind: CheckpointFileKind::Untracked,
                bytes: item.content.len() as u64,
                sha256: sha256_bytes(&item.content),
                mode: item.mode,
                symlink: item.symlink,
            });
        }
        let mut manifest = CheckpointManifest {
            schema_version: 1,
            created_at_ms: now_ms(),
            run_id: run_id.to_string(),
            worktree_name: wt_name.to_string(),
            worktree_path: worktree_dir.to_string_lossy().into_owned(),
            branch,
            head_sha,
            files: entries.clone(),
            manifest_digest: String::new(),
        };
        manifest.manifest_digest = compute_manifest_digest(&entries);
        let pretty = format!("{}\n", serde_json::to_string_pretty(&manifest).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?);
        fs::write(tmp_dir.join("manifest.json"), pretty).map_err(|e| cp_err(format!("checkpoint failed: {}", e)))?;

        let self_check = verify_worktree_checkpoint(tmp_dir);
        if !self_check.valid {
            let mut errs = vec!["self-verification failed after write".to_string()];
            errs.extend(self_check.errors);
            return Err(cp_errs(errs));
        }
        Ok(manifest)
    })();
    let manifest = match written {
        Ok(m) => m,
        Err(failed) => return failed,
    };
    if let Err(e) = fs::remove_dir_all(final_dir) {
        if final_dir.exists() {
            return CheckpointResult { saved: false, errors: vec![format!("checkpoint failed: {}", e)], ..Default::default() };
        }
    }
    if let Err(e) = fs::rename(tmp_dir, final_dir) {
        return CheckpointResult { saved: false, errors: vec![format!("checkpoint failed: {}", e)], ..Default::default() };
    }
    CheckpointResult {
        saved: true,
        checkpoint_dir: Some(final_dir.to_path_buf()),
        manifest: Some(manifest),
        errors: Vec::new(),
    }
}

fn cp_err(msg: String) -> CheckpointResult {
    CheckpointResult { saved: false, errors: vec![msg], ..Default::default() }
}

fn cp_errs(msgs: Vec<String>) -> CheckpointResult {
    CheckpointResult { saved: false, errors: msgs, ..Default::default() }
}

// ---------- B4 검증: verify_worktree_checkpoint ----------

fn is_valid_sha256_hex(v: &str) -> bool {
    v.len() == 64 && v.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 체크포인트 무결성 검증: manifest 파싱 → manifest_digest 재계산 대조 →
/// files 각 항목의 존재·일반 파일 여부·bytes·sha256 전건 대조.
/// manifest에 안전하지 않은 relpath(`..` 순회 등)가 하나라도 있으면 invalid다.
pub fn verify_worktree_checkpoint(checkpoint_dir: &Path) -> VerifyCheckpointResult {
    let dir = fs::canonicalize(checkpoint_dir).unwrap_or_else(|_| checkpoint_dir.to_path_buf());
    let mut errors: Vec<String> = Vec::new();
    let manifest: Option<CheckpointManifest> = (|| {
        let raw = match fs::read_to_string(dir.join("manifest.json")) {
            Ok(r) => r,
            Err(e) => {
                errors.push(format!("manifest.json unreadable: {}", e));
                return None;
            }
        };
        let parsed: serde_json::Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(e) => {
                errors.push(format!("manifest.json unreadable: {}", e));
                return None;
            }
        };
        if parsed.get("schema_version").and_then(|v| v.as_i64()) != Some(1) {
            errors.push(format!(
                "unsupported schema_version: {}",
                parsed.get("schema_version").map(|v| v.to_string()).unwrap_or_default()
            ));
        }
        let run_id = parsed.get("run_id").and_then(|v| v.as_str()).unwrap_or("");
        if run_id.is_empty() {
            errors.push("manifest run_id missing".to_string());
        }
        let worktree_name = parsed.get("worktree_name").and_then(|v| v.as_str()).unwrap_or("");
        if worktree_name.is_empty() {
            errors.push("manifest worktree_name missing".to_string());
        }
        let head_sha = parsed.get("head_sha").and_then(|v| v.as_str()).unwrap_or("");
        if head_sha.is_empty() {
            errors.push("manifest head_sha missing".to_string());
        }
        let files = match parsed.get("files").and_then(|v| v.as_array()) {
            Some(f) => f.clone(),
            None => {
                errors.push("manifest files is not an array".to_string());
                return None;
            }
        };
        let mut digest_input: Vec<CheckpointFileEntry> = Vec::new();
        for f in &files {
            let rel = match f.get("relpath").and_then(|v| v.as_str()) {
                Some(r) => r,
                None => {
                    errors.push("manifest file entry missing relpath/kind".to_string());
                    continue;
                }
            };
            let kind_str = match f.get("kind").and_then(|v| v.as_str()) {
                Some(k) => k,
                None => {
                    errors.push("manifest file entry missing relpath/kind".to_string());
                    continue;
                }
            };
            let Some(kind) = CheckpointFileKind::parse(kind_str) else {
                errors.push(format!("manifest file entry has invalid kind: {}", kind_str));
                continue;
            };
            if rel == "index.patch" && kind != CheckpointFileKind::IndexPatch {
                errors.push(format!("kind mismatch for {}: {}", rel, kind_str));
                continue;
            }
            if rel == "worktree.patch" && kind != CheckpointFileKind::WorktreePatch {
                errors.push(format!("kind mismatch for {}: {}", rel, kind_str));
                continue;
            }
            if let Some(inner) = rel.strip_prefix("untracked/") {
                if kind != CheckpointFileKind::Untracked || !is_safe_rel_path(inner) || is_excluded_untracked_rel(inner) {
                    errors.push(format!("unsafe or mismatched untracked manifest path: {}", rel));
                    continue;
                }
            } else if rel != "index.patch" && rel != "worktree.patch" {
                errors.push(format!("unsafe manifest relpath rejected: {}", rel));
                continue;
            }
            let Some(bytes) = f.get("bytes").and_then(|v| v.as_u64()) else {
                errors.push(format!("manifest bytes invalid for {}", rel));
                continue;
            };
            let Some(sha) = f.get("sha256").and_then(|v| v.as_str()) else {
                errors.push(format!("manifest sha256 invalid for {}", rel));
                continue;
            };
            if !is_valid_sha256_hex(sha) {
                errors.push(format!("manifest sha256 invalid for {}", rel));
                continue;
            }
            digest_input.push(CheckpointFileEntry {
                relpath: rel.to_string(),
                kind,
                bytes,
                sha256: sha.to_string(),
                mode: f.get("mode").and_then(|v| v.as_u64()).map(|m| m as u32),
                symlink: matches!(f.get("symlink"), Some(serde_json::Value::Bool(true))),
            });
        }
        if digest_input.len() == files.len() {
            let recomputed = compute_manifest_digest(&digest_input);
            let stored = parsed.get("manifest_digest").and_then(|v| v.as_str());
            if stored != Some(recomputed.as_str()) {
                errors.push("manifest_digest mismatch".to_string());
            }
        }
        if !digest_input.is_empty() || files.is_empty() {
            Some(CheckpointManifest {
                schema_version: 1,
                created_at_ms: parsed.get("created_at_ms").and_then(|v| v.as_i64()).unwrap_or(0),
                run_id: run_id.to_string(),
                worktree_name: worktree_name.to_string(),
                worktree_path: parsed.get("worktree_path").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                branch: parsed.get("branch").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                head_sha: head_sha.to_string(),
                files: digest_input,
                manifest_digest: parsed.get("manifest_digest").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })
        } else {
            None
        }
    })();

    // 파일 실체 대조 — 저장 시점에는 언제나 일반 파일이므로 lstat으로 심볼릭 링크 바꿔치기 차단
    if let Some(manifest) = &manifest {
        for entry in &manifest.files {
            let abs = dir.join(&entry.relpath);
            if abs.strip_prefix(&dir).map(|p| p.to_string_lossy() != entry.relpath).unwrap_or(true) {
                errors.push(format!("path escapes checkpoint dir: {}", entry.relpath));
                continue;
            }
            let st = match fs::symlink_metadata(&abs) {
                Ok(s) => s,
                Err(_) => {
                    errors.push(format!("file missing: {}", entry.relpath));
                    continue;
                }
            };
            if !st.is_file() {
                errors.push(format!("not a regular file (symlink substitution?): {}", entry.relpath));
                continue;
            }
            if st.len() != entry.bytes {
                errors.push(format!("size mismatch for {}: {} != {}", entry.relpath, st.len(), entry.bytes));
                continue;
            }
            match fs::read(&abs) {
                Ok(data) => {
                    if sha256_bytes(&data) != entry.sha256 {
                        errors.push(format!("sha256 mismatch for {}", entry.relpath));
                    }
                }
                Err(e) => errors.push(format!("file missing: {} ({})", entry.relpath, e)),
            }
        }
    }

    VerifyCheckpointResult {
        valid: errors.is_empty(),
        errors,
        manifest,
    }
}

// ---------- B4 복원: restore_worktree_checkpoint ----------

/// 검증 통과 체크포인트를 대상 워크트리에 무손실 재적용한다.
/// - verify 실패 시 복원을 시작하지 않는다(원문 그대로 거부).
/// - 패치 적용 순서: index.patch(`git apply --index`) → worktree.patch(워크트리만).
/// - untracked는 manifest 검증을 통과한 경로만 복사하며, 심볼릭 링크는 대상 문자열로 재생성.
/// - 실패 시 타깃을 지우지 않는다(부분 복구 상태를 사람이 검사할 수 있게 남긴다).
pub fn restore_worktree_checkpoint(checkpoint_dir: &Path, target_worktree_dir: &Path) -> RestoreCheckpointResult {
    let dir = fs::canonicalize(checkpoint_dir).unwrap_or_else(|_| checkpoint_dir.to_path_buf());
    let target = fs::canonicalize(target_worktree_dir).unwrap_or_else(|_| target_worktree_dir.to_path_buf());
    let verified = verify_worktree_checkpoint(&dir);
    let Some(manifest) = verified.manifest.clone() else {
        let mut errs = vec!["checkpoint verification failed".to_string()];
        errs.extend(verified.errors);
        return RestoreCheckpointResult { restored: false, errors: errs };
    };
    if !verified.valid {
        let mut errs = vec!["checkpoint verification failed".to_string()];
        errs.extend(verified.errors);
        return RestoreCheckpointResult { restored: false, errors: errs };
    }
    if !target.is_dir() {
        return RestoreCheckpointResult {
            restored: false,
            errors: vec![format!("target worktree not found: {}", target.display())],
        };
    }
    let mut errors: Vec<String> = Vec::new();
    let index_path = dir.join("index.patch");
    let worktree_path = dir.join("worktree.patch");
    let index_size = fs::metadata(&index_path).map(|m| m.len()).unwrap_or(0);
    let worktree_size = fs::metadata(&worktree_path).map(|m| m.len()).unwrap_or(0);
    if index_size > 0 {
        let r = git_run(&target, &["apply", "--index", "--binary", &index_path.to_string_lossy()]);
        if !r.ok {
            return RestoreCheckpointResult { restored: false, errors: vec![format!("git apply --index failed: {}", r.stderr)] };
        }
    }
    if worktree_size > 0 {
        let r = git_run(&target, &["apply", "--binary", &worktree_path.to_string_lossy()]);
        if !r.ok {
            return RestoreCheckpointResult { restored: false, errors: vec![format!("git apply failed: {}", r.stderr)] };
        }
    }
    for entry in &manifest.files {
        if entry.kind != CheckpointFileKind::Untracked {
            continue;
        }
        let Some(rel) = entry.relpath.strip_prefix("untracked/") else {
            continue;
        };
        if !is_safe_rel_path(rel) || is_excluded_untracked_rel(rel) {
            errors.push(format!("unsafe untracked manifest path refused during restore: {}", entry.relpath));
            continue;
        }
        let src_abs = dir.join(&entry.relpath);
        let dest = target.join(rel);
        if dest.strip_prefix(&target).map(|p| p.to_string_lossy() != rel).unwrap_or(true) {
            errors.push(format!("path escapes target worktree: {}", rel));
            continue;
        }
        if let Some(parent) = dest.parent() {
            if let Err(e) = fs::create_dir_all(parent) {
                errors.push(format!("restore failed (target kept for inspection): {}", e));
                continue;
            }
        }
        if entry.symlink {
            let link_target = match fs::read_to_string(&src_abs) {
                Ok(t) => t,
                Err(e) => {
                    errors.push(format!("restore failed (target kept for inspection): {}", e));
                    continue;
                }
            };
            if dest.exists() || fs::symlink_metadata(&dest).is_ok() {
                let _ = fs::remove_file(&dest);
            }
            if let Err(e) = std::os::unix::fs::symlink(&link_target, &dest) {
                errors.push(format!("restore failed (target kept for inspection): {}", e));
                continue;
            }
        } else if let Err(e) = fs::copy(&src_abs, &dest) {
            errors.push(format!("restore failed (target kept for inspection): {}", e));
            continue;
        } else if let Some(mode) = entry.mode {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
        }
    }
    RestoreCheckpointResult { restored: errors.is_empty(), errors }
}

// ---------- B4 삭제 게이트: safe_remove_worktree (절대 불변식) ----------

/// 체크포인트 게이트 경유 워크트리 삭제 — 이 함수가 저장소 내 `git worktree remove`를
/// 호출하는 유일한 지점이다. 저장([`create_worktree_checkpoint`])과 검증
/// ([`verify_worktree_checkpoint`]) 양쪽 모두 통과하기 전에는 삭제 호출에 도달할 수
/// 없다(우회 플래그 없음). 게이트 거부는 `{ removed: false, exit_code: 4 }`로 즉시
/// 반환하며 워크트리를 건드리지 않는다.
pub fn safe_remove_worktree(input: &SafeRemoveInput) -> SafeRemoveResult {
    let ck = create_worktree_checkpoint(&CreateCheckpointInput {
        repo_dir: input.repo_dir,
        worktree_dir: input.worktree_dir,
        run_id: input.run_id,
        worktree_name: input.worktree_name,
        checkpoint_root: input.checkpoint_root,
    });
    if !ck.saved {
        let mut errs = vec!["checkpoint save failed — removal refused".to_string()];
        errs.extend(ck.errors);
        return SafeRemoveResult { removed: false, exit_code: 4, checkpoint_dir: None, errors: errs };
    }
    let Some(checkpoint_dir) = ck.checkpoint_dir.clone() else {
        let mut errs = vec!["checkpoint save failed — removal refused".to_string()];
        errs.extend(ck.errors);
        return SafeRemoveResult { removed: false, exit_code: 4, checkpoint_dir: None, errors: errs };
    };
    let verified = verify_worktree_checkpoint(&checkpoint_dir);
    if !verified.valid {
        let mut errs = vec!["checkpoint verification failed — removal refused".to_string()];
        errs.extend(verified.errors);
        return SafeRemoveResult {
            removed: false,
            exit_code: 4,
            checkpoint_dir: Some(checkpoint_dir),
            errors: errs,
        };
    }
    let worktree_abs = fs::canonicalize(input.worktree_dir).unwrap_or_else(|_| input.worktree_dir.to_path_buf());
    let repo_abs = fs::canonicalize(input.repo_dir).unwrap_or_else(|_| input.repo_dir.to_path_buf());
    let mut args: Vec<String> = vec!["worktree".into(), "remove".into()];
    if input.force_git_remove {
        args.push("--force".into());
    }
    args.push(worktree_abs.to_string_lossy().into_owned());
    let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
    let res = Command::new("git").arg("-C").arg(&repo_abs).args(&arg_refs).output();
    match res {
        Ok(out) if out.status.success() => SafeRemoveResult {
            removed: true,
            exit_code: 0,
            checkpoint_dir: Some(checkpoint_dir),
            errors: Vec::new(),
        },
        Ok(out) => SafeRemoveResult {
            removed: false,
            exit_code: out.status.code().unwrap_or(1),
            checkpoint_dir: Some(checkpoint_dir),
            errors: vec![format!(
                "git worktree remove failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )],
        },
        Err(e) => SafeRemoveResult {
            removed: false,
            exit_code: 1,
            checkpoint_dir: Some(checkpoint_dir),
            errors: vec![format!("git worktree remove failed: {}", e)],
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::temp_dir;
    use std::os::unix::fs::PermissionsExt;

    fn git_ok(cwd: &Path, args: &[&str]) -> String {
        let r = git_run(cwd, args);
        assert!(r.ok, "git {} failed: {}", args.join(" "), r.stderr);
        String::from_utf8_lossy(&r.stdout).into_owned()
    }

    fn init_repo(base: &Path) -> PathBuf {
        let repo = base.join("repo");
        let no_hooks = base.join("nohooks");
        fs::create_dir_all(&repo).unwrap();
        fs::create_dir_all(&no_hooks).unwrap();
        git_ok(&repo, &["init", "-q"]);
        git_ok(&repo, &["config", "user.email", "pb2b@example.com"]);
        git_ok(&repo, &["config", "user.name", "pb2b-test"]);
        git_ok(&repo, &["config", "commit.gpgsign", "false"]);
        git_ok(&repo, &["config", "core.hooksPath", &no_hooks.to_string_lossy()]);
        fs::write(repo.join("base.txt"), "v0\n").unwrap();
        fs::write(repo.join("tracked.md"), "# hello\n").unwrap();
        git_ok(&repo, &["add", "-A"]);
        git_ok(&repo, &["commit", "-qm", "init"]);
        repo
    }

    /// staged(base.txt→v2-idx) + unstaged(→v2-wt) + untracked(초안/실행파일/심볼릭 링크)
    fn add_worktree_with_dirty_state(repo: &Path, wt_name: &str) -> PathBuf {
        let wt = repo.parent().unwrap().join(wt_name);
        git_ok(repo, &["worktree", "add", "-b", wt_name, &wt.to_string_lossy()]);
        fs::write(wt.join("base.txt"), "v2-idx\n").unwrap();
        git_ok(&wt, &["add", "base.txt"]);
        fs::write(wt.join("base.txt"), "v2-wt\n").unwrap();
        fs::create_dir_all(wt.join("notes")).unwrap();
        fs::write(wt.join("notes").join("draft.txt"), "draft-초안\n").unwrap();
        fs::write(wt.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
        fs::set_permissions(&wt.join("run.sh"), fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("notes/draft.txt", wt.join("link.txt")).unwrap();
        wt
    }

    #[test]
    fn t14_checkpoint_save_verify_tamper_restore_roundtrip() {
        let base = temp_dir("ckpt-t14");
        let repo = init_repo(&base);
        let wt = add_worktree_with_dirty_state(&repo, "coder-1");
        let head = git_ok(&wt, &["rev-parse", "HEAD"]).trim().to_string();
        let ckpt_root = base.join("ckpt");

        let res = create_worktree_checkpoint(&CreateCheckpointInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-1",
            worktree_name: None,
            checkpoint_root: Some(&ckpt_root),
        });
        assert!(res.saved, "save failed: {:?}", res.errors);
        let ckpt_dir = res.checkpoint_dir.clone().unwrap();
        assert_eq!(ckpt_dir, ckpt_root.join("run-1").join("coder-1"));
        let manifest = res.manifest.clone().unwrap();
        assert_eq!(manifest.schema_version, 1);
        assert_eq!(manifest.branch, "coder-1");
        assert_eq!(manifest.head_sha, head);
        let relpaths: Vec<String> = manifest
            .files
            .iter()
            .map(|f| format!("{}|{}{}", f.relpath, f.kind.as_str(), if f.symlink { "|symlink" } else { "" }))
            .collect();
        for expected in [
            "index.patch|index_patch",
            "worktree.patch|worktree_patch",
            "untracked/link.txt|untracked|symlink",
            "untracked/notes/draft.txt|untracked",
            "untracked/run.sh|untracked",
        ] {
            assert!(relpaths.iter().any(|r| r == expected), "manifest missing entry: {} (got {:?})", expected, relpaths);
        }

        let v1 = verify_worktree_checkpoint(&ckpt_dir);
        assert!(v1.valid, "verify failed: {:?}", v1.errors);
        assert_eq!(v1.manifest.unwrap().manifest_digest, manifest.manifest_digest);

        // 1바이트 변조 → invalid
        let patch_path = ckpt_dir.join("index.patch");
        let orig = fs::read(&patch_path).unwrap();
        let mut tampered = orig.clone();
        tampered.push(b'X');
        fs::write(&patch_path, &tampered).unwrap();
        let v_bad = verify_worktree_checkpoint(&ckpt_dir);
        assert!(!v_bad.valid, "tampered checkpoint must be invalid");
        assert!(v_bad.errors.iter().any(|e| e.contains("mismatch") || e.contains("size")), "{:?}", v_bad.errors);
        fs::write(&patch_path, &orig).unwrap();
        assert!(verify_worktree_checkpoint(&ckpt_dir).valid, "restored checkpoint must verify again");

        // 새 worktree에 복원 → staged/unstaged/untracked/symlink/실행비트 전부 일치
        let target = base.join("coder-1-restore");
        git_ok(&repo, &["worktree", "add", "-b", "coder-1-restore", &target.to_string_lossy(), &head]);
        let rr = restore_worktree_checkpoint(&ckpt_dir, &target);
        assert!(rr.restored, "restore failed: {:?}", rr.errors);

        assert_eq!(fs::read_to_string(target.join("base.txt")).unwrap(), "v2-wt\n", "unstaged 내용 일치");
        assert_eq!(git_ok(&target, &["show", ":base.txt"]), "v2-idx\n", "staged(index) 내용 일치");
        assert_eq!(fs::read_to_string(target.join("notes").join("draft.txt")).unwrap(), "draft-초안\n");
        let mode = fs::metadata(target.join("run.sh")).unwrap().mode() & 0o777;
        assert_eq!(mode, 0o755, "실행 비트 보존");
        assert_eq!(fs::read_link(target.join("link.txt")).unwrap().to_string_lossy(), "notes/draft.txt", "심볼릭 링크 대상 일치");

        // 원본 워크트리와 전체 diff 상태 동등성(패치 적용 결과 = 원본 상태)
        let orig_index = git_ok(&wt, &["diff", "--cached", "--binary"]);
        let rest_index = git_ok(&target, &["diff", "--cached", "--binary"]);
        assert_eq!(rest_index, orig_index, "복원 worktree의 staged diff가 원본과 동일");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn t15_removal_gate_hard_invariants() {
        let base = temp_dir("ckpt-t15");
        let repo = init_repo(&base);
        let wt = add_worktree_with_dirty_state(&repo, "coder-2");
        let ckpt_root = base.join("ckpt");
        fs::create_dir_all(&ckpt_root).unwrap();

        // (a) 읽기전용 checkpointRoot — 체크포인트 저장 자체가 실패
        fs::set_permissions(&ckpt_root, fs::Permissions::from_mode(0o500)).unwrap();
        let rm_a = safe_remove_worktree(&SafeRemoveInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-2",
            worktree_name: None,
            checkpoint_root: Some(&ckpt_root),
            force_git_remove: false,
        });
        assert!(!rm_a.removed, "읽기전용 root에서는 삭제 거부");
        assert_eq!(rm_a.exit_code, 4, "게이트 거부 exitCode는 4 (got {})", rm_a.exit_code);
        assert!(!rm_a.errors.is_empty());
        assert!(wt.join("base.txt").exists(), "워크트리가 그대로 보존되어야 한다");
        assert!(wt.join("notes").join("draft.txt").exists(), "untracked 파일도 보존");
        fs::set_permissions(&ckpt_root, fs::Permissions::from_mode(0o755)).unwrap();

        // (b) 용량 초과(12MiB+) untracked — 저장 실패 → 삭제 거부 + 파일 보존
        let big = wt.join("big.bin");
        fs::write(&big, vec![7u8; (MAX_CHECKPOINT_BYTES + 4096) as usize]).unwrap();
        let rm_b = safe_remove_worktree(&SafeRemoveInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-2b",
            worktree_name: None,
            checkpoint_root: Some(&ckpt_root),
            force_git_remove: false,
        });
        assert!(!rm_b.removed);
        assert_eq!(rm_b.exit_code, 4);
        assert!(rm_b.errors.iter().any(|e| e.contains("big.bin")), "용량 초과 에러에 파일명 포함 (got: {:?})", rm_b.errors);
        assert!(big.exists(), "초과 파일도 삭제되지 않고 보존");
        fs::remove_file(&big).unwrap();

        // (c) 정상 체크포인트 저장+검증 통과 → 삭제 성공
        let rm_c = safe_remove_worktree(&SafeRemoveInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-2c",
            worktree_name: None,
            checkpoint_root: Some(&ckpt_root),
            force_git_remove: true,
        });
        assert!(rm_c.removed, "정상 경로 삭제 실패: {:?}", rm_c.errors);
        assert_eq!(rm_c.exit_code, 0);
        let ck_dir = rm_c.checkpoint_dir.clone().unwrap();
        assert!(ck_dir.exists(), "삭제 후에도 체크포인트는 보존(복구 가능)");
        assert!(!wt.exists(), "워크트리 디렉터리가 삭제되어야 한다");
        let list = git_ok(&repo, &["worktree", "list", "--porcelain"]);
        assert!(!list.contains(wt.to_string_lossy().as_ref()), "git worktree list에서 제거되어야 한다");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn t16_path_traversal_manifest_tamper_rejected() {
        let base = temp_dir("ckpt-t16");
        let repo = init_repo(&base);
        let wt = add_worktree_with_dirty_state(&repo, "coder-3");
        let ckpt_root = base.join("ckpt");
        let res = create_worktree_checkpoint(&CreateCheckpointInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-3",
            worktree_name: None,
            checkpoint_root: Some(&ckpt_root),
        });
        assert!(res.saved, "{:?}", res.errors);
        let ckpt_dir = res.checkpoint_dir.clone().unwrap();

        // digest까지 정합하게 재계산한 "완전한" 변조 manifest — 오직 경로 검증만이 걸러낸다
        let manifest_path = ckpt_dir.join("manifest.json");
        let mut m: serde_json::Value = serde_json::from_str(&fs::read_to_string(&manifest_path).unwrap()).unwrap();
        let esc_abs = base.join("escape.txt");
        fs::write(&esc_abs, "escaped-payload\n").unwrap();
        let esc_sha = sha256_bytes(&fs::read(&esc_abs).unwrap());
        m["files"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({
                "relpath": "../escape.txt",
                "kind": "untracked",
                "bytes": fs::metadata(&esc_abs).unwrap().len(),
                "sha256": esc_sha,
                "mode": 0o644,
                "symlink": false,
            }));
        // 변조 후 digest 재계산 — TS computeManifestDigest와 동일 규칙
        let tampered_files: Vec<CheckpointFileEntry> = m["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| serde_json::from_value(f.clone()).unwrap())
            .collect();
        m["manifest_digest"] = serde_json::Value::String(compute_manifest_digest(&tampered_files));
        fs::write(&manifest_path, serde_json::to_string_pretty(&m).unwrap()).unwrap();

        let v = verify_worktree_checkpoint(&ckpt_dir);
        assert!(!v.valid, "순회 경로가 포함된 manifest는 invalid");
        assert!(v.errors.iter().any(|e| e.contains("escape") || e.contains("unsafe")), "{:?}", v.errors);

        let target = base.join("coder-3-target");
        git_ok(&repo, &["worktree", "add", "-b", "coder-3-target", &target.to_string_lossy()]);
        let rr = restore_worktree_checkpoint(&ckpt_dir, &target);
        assert!(!rr.restored, "변조 체크포인트 복원 거부");
        assert!(!base.join("escape.txt").exists() || fs::read_to_string(&base.join("escape.txt")).unwrap() == "escaped-payload\n");
        assert!(!target.join("escape.txt").exists(), "타깃 워크트리 내부에도 escape.txt 없음");
        assert!(wt.join("base.txt").exists(), "원본 워크트리는 무사");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn excluded_db_untracked_rejected_at_save() {
        let base = temp_dir("ckpt-db");
        let repo = init_repo(&base);
        let wt = add_worktree_with_dirty_state(&repo, "coder-db");
        fs::write(wt.join("cache.db"), b"sqlite bytes").unwrap();
        let res = create_worktree_checkpoint(&CreateCheckpointInput {
            repo_dir: &repo,
            worktree_dir: &wt,
            run_id: "run-db",
            worktree_name: None,
            checkpoint_root: Some(&base.join("ckpt")),
        });
        assert!(!res.saved, "*.db* untracked는 무손실 보장 위반 → 저장 거부");
        assert!(res.errors.iter().any(|e| e.contains("cache.db")), "{:?}", res.errors);
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn invalid_run_id_and_missing_worktree_rejected() {
        let base = temp_dir("ckpt-invalid");
        let repo = init_repo(&base);
        let r1 = create_worktree_checkpoint(&CreateCheckpointInput {
            repo_dir: &repo,
            worktree_dir: &repo,
            run_id: "../evil",
            worktree_name: None,
            checkpoint_root: Some(&base.join("ckpt")),
        });
        assert!(!r1.saved);
        assert!(r1.errors[0].contains("invalid run_id"));

        let r2 = create_worktree_checkpoint(&CreateCheckpointInput {
            repo_dir: &repo,
            worktree_dir: &base.join("nope"),
            run_id: "run-ok",
            worktree_name: None,
            checkpoint_root: Some(&base.join("ckpt")),
        });
        assert!(!r2.saved);
        assert!(r2.errors[0].contains("worktree not found"));
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn manifest_digest_matches_ts_canonical_form() {
        // TS computeManifestDigest 교차 검증 — 고정 입력의 sha256이 JSON.stringify 정규형과 일치
        let files = vec![
            CheckpointFileEntry {
                relpath: "index.patch".into(),
                kind: CheckpointFileKind::IndexPatch,
                bytes: 42,
                sha256: "a".repeat(64),
                mode: None,
                symlink: false,
            },
            CheckpointFileEntry {
                relpath: "untracked/z.txt".into(),
                kind: CheckpointFileKind::Untracked,
                bytes: 7,
                sha256: "b".repeat(64),
                mode: Some(0o755),
                symlink: true,
            },
        ];
        let canonical = format!(
            "[{{\"relpath\":\"index.patch\",\"kind\":\"index_patch\",\"bytes\":42,\"sha256\":\"{}\",\"mode\":null,\"symlink\":false}},{{\"relpath\":\"untracked/z.txt\",\"kind\":\"untracked\",\"bytes\":7,\"sha256\":\"{}\",\"mode\":493,\"symlink\":true}}]",
            "a".repeat(64),
            "b".repeat(64)
        );
        assert_eq!(compute_manifest_digest(&files), sha256_bytes(canonical.as_bytes()));
        // 입력 순서와 무관하게 정렬 후 digest가 동일하다
        let mut reversed = files.clone();
        reversed.reverse();
        assert_eq!(compute_manifest_digest(&reversed), compute_manifest_digest(&files));
    }
}
