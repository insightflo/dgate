//! `dg setup` — 환경 감지 진단 + 대화형 위저드(scripts/setup.ts CLI 대응).
//!
//! - `--check`(또는 비TTY) → 읽기전용 진단 리포트. 0 writes/0 prompts.
//! - `--check --json` → 기계판독 JSON(TS EnvironmentReport camelCase 키 패리티).
//! - TTY에서 `--check` 없이 실행 → opt-in 대화형 위저드(roster.json 기록·
//!   스킬 링크·훅 설치 — 각 단계 y/N 확인, 비TTY에서는 절대 쓰지 않는다).
use std::io::{BufRead, Write};
use std::path::PathBuf;

use serde_json::{json, Map, Value};

use crate::args::{optional_flag, ParsedArgs};
use crate::util::prelude::*;
use dgate_core::roster::global_roster_path;
use dgate_core::setup::{
    self, build_tailored_roster_config_by_name, detect_environment, format_check_report,
    DetectDeps, InstallSkillLinksOptions,
};

const USAGE: &str = "Usage:
  dg setup                 대화형 온보딩 위저드 (비TTY → 진단 요약 폴백)
  dg setup --check         읽기전용 진단 리포트 (0 writes/0 prompts)
  dg setup --check --json  기계판독 JSON 진단
  dg setup --repo <dir>    훅 설치 대상 프로젝트 지정
  dg setup --yes           대화형 프롬프트 기본값 자동 수락
  dg setup --help          이 도움말";

/// TS `EnvironmentReport` camelCase JSON 직렬화 — 값 판독만 하고 비밀은 절대 넣지 않는다.
fn report_json(rep: &setup::EnvironmentReport) -> Value {
    fn cli_detection(c: &setup::CliDetection) -> Value {
        json!({"installed": c.installed, "path": c.path})
    }
    let mut clis = Map::new();
    for (k, v) in &rep.clis {
        clis.insert(k.clone(), cli_detection(v));
    }
    let mut companions = Map::new();
    for (k, v) in &rep.companions {
        companions.insert(
            k.clone(),
            json!({
                "id": v.id,
                "installed": v.installed,
                "required": v.required,
                "role": v.role,
                "detail": v.detail,
                "installHint": v.install_hint,
                "guideUrl": v.guide_url,
            }),
        );
    }
    let mut roles = Map::new();
    for (slot, label) in &rep.recommendation.roles {
        roles.insert(slot.as_str().to_string(), json!(label));
    }
    let discovered: Vec<Value> = rep
        .discovered_models
        .iter()
        .map(|m| {
            json!({
                "cli": m.cli.as_str(),
                "provider": m.provider,
                "model": m.model,
                "source": m.source.as_str(),
                "tier": m.tier.label(),
                "tierLabel": m.tier_label,
            })
        })
        .collect();
    let advice: Vec<Value> = rep
        .companion_advice
        .iter()
        .map(|a| {
            json!({
                "id": a.id,
                "name": a.name,
                "verdict": a.verdict.as_str(),
                "reason": a.reason,
                "howToUse": a.how_to_use,
                "canAutoInstall": a.can_auto_install,
                "installCommand": a.install_command,
                "installGuide": a.install_guide,
                "repoSlug": a.repo_slug,
                "repoUrl": a.repo_url,
            })
        })
        .collect();
    json!({
        "ok": rep.ok,
        "node": {
            "version": rep.node.version,
            "major": rep.node.major,
            "minor": rep.node.minor,
            "ok": rep.node.ok,
        },
        "nodeSqlite": {"ok": rep.node_sqlite_ok},
        "git": cli_detection(&rep.git),
        "clis": Value::Object(clis),
        "agents": rep.agents.iter().map(|a| json!({
            "id": a.id,
            "dir": a.dir,
            "exists": a.exists,
            "skillLinked": a.skill_linked,
        })).collect::<Vec<_>>(),
        "roster": {
            "path": rep.roster.path,
            "exists": rep.roster.exists,
            "validJson": rep.roster.valid_json,
        },
        "db": {"path": rep.db.path, "exists": rep.db.exists, "source": rep.db.source},
        "secrets": {
            "envSet": rep.secrets.env_set,
            "secretsFileExists": rep.secrets.secrets_file_exists,
            "configured": rep.secrets.configured,
            "path": rep.secrets.path,
        },
        "companions": Value::Object(companions),
        "recommendation": {
            "mode": rep.recommendation.mode.as_str(),
            "profileName": rep.recommendation.profile_name,
            "roles": Value::Object(roles),
            "reason": rep.recommendation.reason,
            "agy": rep.recommendation.agy,
        },
        "discoveredModels": discovered,
        "companionAdvice": advice,
        "repoHooks": rep.repo_hooks.as_ref().map(|h| json!({
            "repoDir": h.repo_dir,
            "repoDirExists": h.repo_dir_exists,
            "hooksJson": h.hooks_json,
            "hookSymlink": h.hook_symlink,
            "excluded": h.excluded,
        })),
    })
}

fn stdin_is_tty() -> bool {
    unsafe { libc::isatty(0) == 1 }
}

fn prompt_yes(question: &str, default_yes: bool) -> bool {
    let mut out = std::io::stdout().lock();
    let suffix = if default_yes { "[Y/n]" } else { "[y/N]" };
    let _ = writeln!(out, "{question} {suffix}");
    let _ = out.flush();
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).is_err() {
        return false;
    }
    let t = line.trim().to_lowercase();
    if t.is_empty() {
        return default_yes;
    }
    t == "y" || t == "yes"
}

/// 최소 대화형 위저드 — 모든 쓰기는 y/N opt-in. 비TTY에서는 호출되지 않는다.
fn run_wizard(rep: &setup::EnvironmentReport, yes: bool, repo: Option<&str>) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(
        out,
        "[setup] 추천 프로필: {} — {}",
        rep.recommendation.profile_name.as_deref().unwrap_or("(builtin)"),
        rep.recommendation.reason
    );
    let _ = writeln!(out, "[setup] 진단 전체는 `dg setup --check`로 확인할 수 있다.");
    drop(out);

    // 1) 맞춤 roster.json 기록(opt-in)
    let roster_path = global_roster_path();
    let do_roster = yes
        || prompt_yes(
            &format!("맞춤 roster.json을 {}에 기록할까요?", roster_path.display()),
            false,
        );
    if do_roster {
        match build_tailored_roster_config_by_name(rep.recommendation.profile_name.as_deref()) {
            Some(cfg) => {
                let body = serde_json::to_string_pretty(&cfg).unwrap_or_default();
                if let Some(parent) = roster_path.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match std::fs::write(&roster_path, format!("{body}\n")) {
                    Ok(()) => {
                        let mut out = std::io::stdout().lock();
                        let _ = writeln!(out, "[setup] roster.json 기록 → {}", roster_path.display());
                    }
                    Err(e) => crate::util::eprint_line(&format!("[setup] roster.json 기록 실패: {e}")),
                }
            }
            None => crate::util::eprint_line("[setup] 맞춤 roster 구성을 만들 수 없어 기록을 건너뜁니다"),
        }
    }

    // 2) 에이전트 스킬 심볼릭 링크(opt-in)
    let do_skills = yes || prompt_yes("발견된 에이전트 디렉터리에 dgate 스킬 링크를 설치할까요?", false);
    if do_skills {
        let res = setup::install_skill_links(&InstallSkillLinksOptions {
            home_dir: None,
            repo_dir: None,
            only_agent_ids: None,
        });
        let mut out = std::io::stdout().lock();
        for l in &res.linked {
            let _ = writeln!(out, "[setup] 스킬 링크 → {l}");
        }
        for s in &res.skipped {
            let _ = writeln!(out, "[setup] 스킬 링크 건너뜀({}): {}", s.dir, s.reason);
        }
    }

    // 3) 타깃 프로젝트 훅 설치(opt-in, --repo 지정 시)
    if let Some(repo_dir) = repo {
        let do_hooks = yes || prompt_yes(&format!("{repo_dir}에 훅(hooks.json + dg-hook)을 설치할까요?"), false);
        if do_hooks {
            let res = setup::install_repo_hooks(std::path::Path::new(repo_dir), None);
            let mut out = std::io::stdout().lock();
            for w in &res.written {
                let _ = writeln!(out, "[setup] 훅 설치 → {w}");
            }
            for s in &res.skipped {
                let _ = writeln!(out, "[setup] 훅 건너뜀({}): {}", s.path, s.reason);
            }
        }
    }
}

pub fn cmd_setup(args: &ParsedArgs, _json_top: bool) -> CmdResult {
    // setup 전용 인자 검증 — 알 수 없는 옵션은 exit 2(TS parseArgs 패리티)
    for key in args.flags.keys() {
        if !["check", "json", "yes", "repo", "help"].contains(&key.as_str()) {
            return Err(DgError::Usage(crate::args::UsageError(format!(
                "unknown option: --{key}\n{USAGE}"
            ))));
        }
    }
    if crate::args::bool_flag(args, "help") {
        println!("{USAGE}");
        return Ok(0);
    }
    let check = crate::args::bool_flag(args, "check");
    let json = crate::args::bool_flag(args, "json");
    let yes = crate::args::bool_flag(args, "yes");
    let repo = optional_flag(args, "repo");

    let deps = DetectDeps {
        repo_dir: repo.as_deref().map(PathBuf::from),
        ..Default::default()
    };
    let report = detect_environment(&deps);

    if json {
        let mut out = std::io::stdout().lock();
        let _ = writeln!(
            out,
            "{}",
            serde_json::to_string_pretty(&report_json(&report)).unwrap_or_default()
        );
        return Ok(0);
    }
    if check || !stdin_is_tty() {
        let footer = if !check {
            "\n(비대화형 모드 — 쓰기 없이 진단만 수행했습니다. 대화형 설치는 TTY에서 `dg setup`을 실행하세요.)"
        } else {
            ""
        };
        println!("{}{footer}", format_check_report(&report));
        let _ = std::io::stdout().flush();
        return Ok(0);
    }
    run_wizard(&report, yes, repo.as_deref());
    Ok(0)
}
