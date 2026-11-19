//! 시크릿 방어 3중 경계 공용 리댁션 유틸 — `src/decision/redact.ts` (PR-A1) 포팅.
//!
//! 목적: 로그·오류 스니펫·DB `error_snippet`/`command_line`·자식 워커 env 상속
//! 경로로 평문 시크릿이 유출되는 것을 원천 차단한다.
//!
//! 주의:
//! - [`redact_secrets`]는 무예외(Zero-Throw) 순수 함수다 — 어떤 입력·환경에서도
//!   패닉하지 않으므로 fail-open 수집 경로 어디에나 안전하게 삽입할 수 있다.
//! - 리댁션은 절단(slice) **이전**에 적용해야 한다. 절단 후 적용하면 시크릿이
//!   잘린 경계에 걸쳐 패턴 매칭이 무효화된다.
//!
//! TS 원본 대비 차이: 정규식 역참조(`\2`)를 지원하지 않는 `regex` 크레이트 제약로
//! ENV_ASSIGN 패턴을 인용구 3분지(작은따옴표/큰따옴표/무인용) 정규식으로
//! 재구성했다. 치환 결과(키 보존 + 인용구 보존 + `<redacted>`)는 원본과 동일하다.

use std::collections::BTreeMap;
use std::sync::OnceLock;

use regex::{Captures, Regex};

const REDACTED: &str = "<redacted>";
const MIN_SECRET_LEN: usize = 8;

/// 규칙 1: 런타임 env 스크러빙 대상 키 — TS `ENV_SECRET_KEY_RE`와 동치.
/// 값 길이 >= 8만 수집(짧은 더미 오치환 방지).
fn is_env_secret_key(key: &str) -> bool {
    const SUFFIXES: [&str; 5] = ["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_PRIVATE_KEY"];
    let upper = key.to_uppercase();
    upper == "GEMINI_API_KEY"
        || upper == "GOOGLE_API_KEY"
        || SUFFIXES.iter().any(|s| upper.ends_with(s))
}

/// 현재 프로세스 env에서 스크러빙 대상 시크릿 값 수집 (길이 >= 8, 긴 것 우선).
/// 부분 문자열 관계(한 시크릿이 다른 시크릿의 접두)일 때 긴 값부터 치환해야
/// 짧은 값이 먼저 잘라내어 긴 값 매칭이 깨지는 일이 없다.
fn collect_env_secret_values(extra_values: &[String]) -> Vec<String> {
    let mut values: Vec<String> = Vec::new();
    // env 열거 실패 시 env 스크러빙만 생략 — extraValues·패턴 규칙은 유지
    for (key, value) in std::env::vars() {
        if value.chars().count() < MIN_SECRET_LEN {
            continue;
        }
        if !is_env_secret_key(&key) {
            continue;
        }
        values.push(value);
    }
    for v in extra_values {
        if v.chars().count() >= MIN_SECRET_LEN {
            values.push(v.clone());
        }
    }
    values.sort_by(|a, b| b.chars().count().cmp(&a.chars().count()));
    values.dedup();
    values
}

/// JS `encodeURIComponent`와 동일한 퍼센트 인코딩 — 예약 외 `A-Za-z0-9-_.!~*'()`만 무이스케이프.
fn encode_uri_component(s: &str) -> String {
    const UNRESERVED: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        if UNRESERVED.contains(&b) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 규칙 2: 정규식 패턴 세트 — 최초 사용 시 1회 컴파일(모듈 로드 시 1회 컴파일과 동일 효과).
// 컴파일 실패(정적 패턴이라 사실상 불가능) 시 해당 규칙만 건너뛴다(Zero-Throw).

fn env_assign_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        // TS: /\b([A-Z0-9_]*(?:API_KEY|TOKEN|SECRET|PASSWORD|PRIVATE_KEY))\s*[:=]\s*(['"]?)([^\s'"&;]+)\2/g
        // 역참조 \2 대신 인용구 3분지 재구성 — 치환 결과 동일.
        Regex::new(
            r#"\b([A-Z0-9_]*(?:API_KEY|TOKEN|SECRET|PASSWORD|PRIVATE_KEY))\s*[:=]\s*(?:'([^'\s&;]+)'|"([^"\s&;]+)"|([^\s'\"&;]+))"#,
        )
        .ok()
    })
    .as_ref()
}

fn auth_header_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)(\b(?:Authorization|x-goog-api-key|x-api-key)\s*[:=]\s*(?:Bearer\s+|Basic\s+)?)([^\s,'"]+)"#)
            .ok()
    })
    .as_ref()
}

fn bearer_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"\bBearer\s+[A-Za-z0-9._\-+/=]{8,}").ok())
        .as_ref()
}

fn url_query_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r#"(?i)([?&](?:key|api_key|apikey|token|access_token|secret|auth)=)([^&\s'"]+)"#)
            .ok()
    })
    .as_ref()
}

fn userinfo_re() -> Option<&'static Regex> {
    static RE: OnceLock<Option<Regex>> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)(https?://[^:@/\s]+:)([^@/\s]+)(@)").ok())
        .as_ref()
}

fn known_prefix_res() -> &'static Vec<Option<Regex>> {
    static RES: OnceLock<Vec<Option<Regex>>> = OnceLock::new();
    RES.get_or_init(|| {
        [
            r"AIza[0-9A-Za-z\-_]{20,}",
            r"sk-[A-Za-z0-9\-_]{16,}",
            r"gh[pousr]_[A-Za-z0-9]{20,}",
            r"AKIA[0-9A-Z]{16}",
        ]
        .iter()
        .map(|p| Regex::new(p).ok())
        .collect()
    })
}

/// 문자열 내 시크릿을 `<redacted>`로 치환한다. 절대 패닉하지 않는다.
/// 적용 규칙: ① 런타임 env 실측값(raw + encodeURIComponent 형태) ② 패턴 정규식 세트.
pub fn redact_secrets(text: &str) -> String {
    redact_secrets_with_extra(text, &[])
}

/// [`redact_secrets`]에 추가 실측 시크릿 값을 함께 스크러빙하는 확장 진입점.
pub fn redact_secrets_with_extra(text: &str, extra_values: &[String]) -> String {
    if text.is_empty() {
        return text.to_string();
    }
    let mut out = text.to_string();

    // 규칙 1 — 런타임 실측 env 값: 원문과 URL 인코딩 형태 모두 치환.
    // split/join 치환은 임의 문자(정규식 메타 포함)도 안전하게 다룬다.
    for secret in collect_env_secret_values(extra_values) {
        if out.contains(&secret) {
            out = out.replace(&secret, REDACTED);
        }
        let encoded = encode_uri_component(&secret);
        if encoded != secret && out.contains(&encoded) {
            out = out.replace(&encoded, REDACTED);
        }
    }

    // 규칙 2 — 패턴 세트
    if let Some(re) = env_assign_re() {
        out = re
            .replace_all(&out, |c: &Captures| {
                let key = c.get(1).map(|m| m.as_str()).unwrap_or("");
                if c.get(2).is_some() {
                    format!("{}='{}'", key, REDACTED)
                } else if c.get(3).is_some() {
                    format!("{}=\"{}\"", key, REDACTED)
                } else {
                    format!("{}={}", key, REDACTED)
                }
            })
            .into_owned();
    }
    if let Some(re) = auth_header_re() {
        out = re
            .replace_all(&out, "${1}<redacted>")
            .into_owned();
    }
    if let Some(re) = bearer_re() {
        out = re.replace_all(&out, "Bearer <redacted>").into_owned();
    }
    if let Some(re) = url_query_re() {
        out = re.replace_all(&out, "${1}<redacted>").into_owned();
    }
    if let Some(re) = userinfo_re() {
        // TS 치환식 `$1${REDACTED}$3`(후미 @ 보존)과 동일
        out = re
            .replace_all(&out, |c: &Captures| {
                format!("{}{}{}", c.get(1).map(|m| m.as_str()).unwrap_or(""), REDACTED, c.get(3).map(|m| m.as_str()).unwrap_or(""))
            })
            .into_owned();
    }
    for re_opt in known_prefix_res() {
        if let Some(re) = re_opt {
            out = re.replace_all(&out, REDACTED).into_owned();
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Layer B — 자식 워커 env 최소 격리

/// 자식 env에서 제거할 민감 키 (TS `CHILD_SENSITIVE_KEY_RE`와 동치).
fn is_child_sensitive_key(key: &str) -> bool {
    const SUFFIXES: [&str; 5] = ["_API_KEY", "_TOKEN", "_SECRET", "_PASSWORD", "_PRIVATE_KEY"];
    const EXACT: [&str; 8] = [
        "GEMINI_API_KEY",
        "GOOGLE_API_KEY",
        "OPENAI_API_KEY",
        "ANTHROPIC_API_KEY",
        "GH_TOKEN",
        "GITHUB_TOKEN",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
    ];
    let upper = key.to_uppercase();
    SUFFIXES.iter().any(|s| upper.ends_with(s)) || EXACT.iter().any(|k| upper == *k)
}

/// 민감 패턴에 걸려도 유지하는 안전 기본 키(정확 일치, 대소문자 무시).
fn is_safe_env_key(key: &str) -> bool {
    const SAFE_EXACT: [&str; 15] = [
        "PATH",
        "HOME",
        "USER",
        "SHELL",
        "TERM",
        "LANG",
        "TMPDIR",
        "DGATE_DB_PATH",
        "DECISION_ORCHESTRATOR_DB_PATH",
        "GEMINI_TRICK_DB_PATH",
        "PI_BIN",
        "AGY_BIN",
        "DEVIN_BIN",
        "HC_BIN",
        "CLAUDE_BIN",
    ];
    const SAFE_PREFIXES: [&str; 4] = ["LC_", "XDG_", "HERDR_", "DECISION_"];
    let upper = key.to_uppercase();
    if SAFE_EXACT.contains(&upper.as_str()) {
        return true;
    }
    SAFE_PREFIXES.iter().any(|p| upper.starts_with(p))
}

/// 현재 프로세스 env에서 자식 워커에 상속할 env를 구성한다.
/// 민감 키를 제거하되 안전 기본 키와 `allow_extra`에 명시된 키는 유지한다.
pub fn build_sanitized_child_env(allow_extra: &[String]) -> BTreeMap<String, String> {
    build_sanitized_child_env_from(std::env::vars(), allow_extra)
}

/// [`build_sanitized_child_env`]의 주입형 변형(테스트·호출자 제공 env용).
pub fn build_sanitized_child_env_from<I>(base_env: I, allow_extra: &[String]) -> BTreeMap<String, String>
where
    I: IntoIterator<Item = (String, String)>,
{
    let allow: Vec<String> = allow_extra.iter().map(|k| k.to_uppercase()).collect();
    let mut out = BTreeMap::new();
    for (key, value) in base_env {
        let keep = is_safe_env_key(&key)
            || allow.contains(&key.to_uppercase())
            || !is_child_sensitive_key(&key);
        if keep {
            out.insert(key, value);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn redacts_extra_secret_values_raw_and_url_encoded() {
        let secret = "SUPERSECRETVALUE12345";
        let text = format!("token={}&url=https://x/?t={}", secret, secret);
        let out = redact_secrets_with_extra(&text, &[secret.to_string()]);
        assert!(!out.contains(secret), "raw secret must be gone: {}", out);
        assert_eq!(out.matches("<redacted>").count() >= 2, true);
    }

    #[test]
    fn redacts_longer_secret_first_when_prefix_overlap() {
        let short = "ABCDEFGHIJ"; // 10자
        let long = "ABCDEFGHIJLONGSECRET"; // short을 접두로 포함
        let text = format!("a={} b={}", long, short);
        let out = redact_secrets_with_extra(&text, &[short.to_string(), long.to_string()]);
        assert!(out.contains("<redacted> b=<redacted>") || out.matches("<redacted>").count() == 2);
        assert!(!out.contains(long));
        assert!(!out.contains(short));
    }

    #[test]
    fn ignores_short_extra_values() {
        let out = redact_secrets_with_extra("k=abc123", &["abc123".to_string()]);
        assert_eq!(out, "k=abc123");
    }

    #[test]
    fn redacts_env_assignment_patterns() {
        // TS 치환식 `$1=$2${REDACTED}$2` — 구분자(:)도 =로 정규화하고 인용구는 보존한다.
        assert_eq!(redact_secrets("MY_API_KEY=abcdef123456"), "MY_API_KEY=<redacted>");
        assert_eq!(redact_secrets("FOO_TOKEN: value99887766"), "FOO_TOKEN=<redacted>");
        assert_eq!(redact_secrets("DB_PASSWORD='pw1234567890'"), "DB_PASSWORD='<redacted>'");
        assert_eq!(redact_secrets("SSH_PRIVATE_KEY=\"key123456789\""), "SSH_PRIVATE_KEY=\"<redacted>\"");
    }

    #[test]
    fn redacts_authorization_headers() {
        let out = redact_secrets("Authorization: Bearer ya29.a0AfH6SMBx1234567890");
        assert!(out.starts_with("Authorization: Bearer <redacted>") || out.contains("Authorization: <redacted>"));
        assert!(!out.contains("ya29"));
        let out2 = redact_secrets("x-goog-api-key: AIzaSyA1234567890abcdefghij");
        assert!(out2.contains("x-goog-api-key: <redacted>") || out2.contains("x-goog-api-key:"));
        assert!(!out2.contains("AIzaSy"));
    }

    #[test]
    fn redacts_bearer_tokens() {
        // TS BEARER_RE는 대소문자 구분 — "Bearer"만 매칭
        let out = redact_secrets("header Bearer tok_1234567890abcd tail");
        assert!(out.contains("Bearer <redacted>"), "{}", out);
        assert!(!out.contains("tok_1234567890abcd"));
        let lower = redact_secrets("header bearer tok_1234567890abcd tail");
        assert!(lower.contains("bearer tok_"), "소문자 bearer는 별도 규칙 없음: {}", lower);
    }

    #[test]
    fn redacts_url_query_secrets() {
        let out = redact_secrets("https://api.example.com/v1?key=ABCD1234EFGH5678&x=1");
        assert!(out.contains("key=<redacted>"));
        assert!(out.contains("&x=1"));
        let out2 = redact_secrets("https://e.com/v1?y=2&access_token=tok_9876543210xyz");
        assert!(out2.contains("access_token=<redacted>"));
    }

    #[test]
    fn redacts_userinfo_url_credentials() {
        let out = redact_secrets("https://user:passw0rd123@example.com/path");
        assert!(out.contains("user:<redacted>@example.com"));
    }

    #[test]
    fn redacts_known_prefix_tokens() {
        assert!(redact_secrets("key AIzaSyB1234567890abcdefghijklmnopqr").contains("<redacted>"));
        assert!(!redact_secrets("key AIzaSyB1234567890abcdefghijklmnopqr").contains("AIza"));
        assert!(redact_secrets("sk-abc123def456ghi789jkl").contains("<redacted>"));
        assert!(redact_secrets("ghp_abcdefghijklmnopqrstuvwxyz1234").contains("<redacted>"));
        assert!(redact_secrets("AKIAIOSFODNN7EXAMPLE").contains("<redacted>"));
        // 너무 짧은 것은 매칭하지 않는다(오탐 방지)
        assert!(!redact_secrets("sk-short123").contains("<redacted>"));
    }

    #[test]
    fn never_panics_on_weird_input() {
        // 그냥 호출해서 패닉 없이 완결되는지만 확인한다.
        let _ = redact_secrets("");
        let _ = redact_secrets("가나다 🚀 \u{0}\u{1} \\ $ ^ . * + ? ( ) [ ] { } |");
        let _ = redact_secrets(&"x".repeat(100_000));
        let _ = redact_secrets_with_extra("\u{10FFFF}_valid?", &["\u{FFFD}12345".to_string()]);
    }

    #[test]
    fn keeps_plain_text_untouched() {
        assert_eq!(redact_secrets("hello world 123"), "hello world 123");
        assert_eq!(redact_secrets("cargo test -p dgate-core"), "cargo test -p dgate-core");
    }

    #[test]
    fn child_env_strips_sensitive_and_keeps_safe() {
        let base = vec![
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("HOME".to_string(), "/Users/kwak".to_string()),
            ("LANG".to_string(), "ko_KR.UTF-8".to_string()),
            ("LC_ALL".to_string(), "C".to_string()),
            ("XDG_STATE_HOME".to_string(), "/tmp/xdg".to_string()),
            ("HERDR_SESSION".to_string(), "s1".to_string()),
            ("DECISION_ORCHESTRATOR_DB_PATH".to_string(), "/tmp/x.db".to_string()),
            ("DGATE_DB_PATH".to_string(), "/tmp/d.db".to_string()),
            ("GEMINI_API_KEY".to_string(), "secretvalue123".to_string()),
            ("MY_SERVICE_TOKEN".to_string(), "tok123456".to_string()),
            ("OPENAI_API_KEY".to_string(), "sk-abcdefgh12345678".to_string()),
            ("AWS_SESSION_TOKEN".to_string(), "aws123456".to_string()),
            ("GITHUB_TOKEN".to_string(), "ghp_abcdefghijklmnopqrst".to_string()),
            ("TERM_PROGRAM".to_string(), "iTerm".to_string()), // 민감 아님 → 유지
        ];
        let out = build_sanitized_child_env_from(base, &[]);
        assert_eq!(out.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert!(out.contains_key("HOME"));
        assert!(out.contains_key("LANG"));
        assert!(out.contains_key("LC_ALL"));
        assert!(out.contains_key("XDG_STATE_HOME"));
        assert!(out.contains_key("HERDR_SESSION"));
        assert!(out.contains_key("DECISION_ORCHESTRATOR_DB_PATH"));
        assert!(out.contains_key("DGATE_DB_PATH"));
        assert!(!out.contains_key("GEMINI_API_KEY"));
        assert!(!out.contains_key("MY_SERVICE_TOKEN"));
        assert!(!out.contains_key("OPENAI_API_KEY"));
        assert!(!out.contains_key("AWS_SESSION_TOKEN"));
        assert!(!out.contains_key("GITHUB_TOKEN"));
        assert!(out.contains_key("TERM_PROGRAM")); // 민감 패턴 밖 일반 키는 통과
    }

    #[test]
    fn child_env_allow_extra_overrides_sensitive_keys() {
        let base = vec![("MY_SERVICE_TOKEN".to_string(), "tok123456".to_string())];
        let out = build_sanitized_child_env_from(base, &["my_service_token".to_string()]);
        assert_eq!(out.get("MY_SERVICE_TOKEN").map(String::as_str), Some("tok123456"));
    }

    #[test]
    fn encode_uri_component_matches_js() {
        assert_eq!(encode_uri_component("abcXYZ-_.!~*'()"), "abcXYZ-_.!~*'()");
        assert_eq!(encode_uri_component("a b/c?d=e&f"), "a%20b%2Fc%3Fd%3De%26f");
        assert_eq!(encode_uri_component("한글"), "%ED%95%9C%EA%B8%80");
    }
}
