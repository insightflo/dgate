//! TS `src/decision/dg.ts`의 `parseArgs`와 완전 동일한 의미론의 인자 파서.
//!
//! - `--key value` / `--key=value` / 단독 불리언 플래그(`BOOL_FLAGS`) 지원.
//! - `--` 이후 인자는 전부 `rest`(자식 명령)로 넘긴다.
//! - 값 누락·중복 플래그·위치 인자는 `UsageError`(exit 2).
//! - 알 수 없는 플래그는 TS와 동일하게 저장만 하고 무시한다(TS 파리티).
use std::collections::BTreeMap;

/// 값 없이 단독으로 쓰는 불리언 플래그 — dg.ts `BOOL_FLAGS` + v2 신규
/// `hook`/`setup`/`dispatch` 전용 플래그(TS dg.ts에는 없는 명령들).
pub const BOOL_FLAGS: [&str; 16] = [
    "json",
    "is-retry",
    "require-stdout-nonempty",
    "require-nonempty",
    "sanitize-env",
    "bump-gen",
    "cost",
    "timing",
    "force",
    "api-eval",
    "guard",
    "collect",
    "check",
    "yes",
    "dry-run",
    "help",
];

/// 사용법 오류 — main에서 exit 2로 변환한다.
#[derive(Debug, Clone)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

#[derive(Debug, Default, Clone)]
pub struct ParsedArgs {
    pub flags: BTreeMap<String, String>,
    pub rest: Vec<String>,
}

pub fn parse_args(argv: &[String]) -> Result<ParsedArgs, UsageError> {
    let mut flags = BTreeMap::new();
    let mut rest = Vec::new();
    let mut i = 0;
    while i < argv.len() {
        let arg = &argv[i];
        if arg == "--" {
            rest.extend_from_slice(&argv[i + 1..]);
            break;
        }
        if !arg.starts_with("--") {
            return Err(UsageError(format!("unexpected positional argument: {arg}")));
        }
        let body = &arg[2..];
        let (key, value) = match body.find('=') {
            Some(eq) => (&body[..eq], Some(body[eq + 1..].to_string())),
            None => (body, None),
        };
        let value = match value {
            Some(v) => v,
            None => {
                if BOOL_FLAGS.contains(&key) {
                    flags.insert(key.to_string(), "1".to_string());
                    i += 1;
                    continue;
                }
                let next = argv.get(i + 1);
                match next {
                    None => return Err(UsageError(format!("missing value for --{key}"))),
                    Some(n) if n.starts_with("--") => {
                        return Err(UsageError(format!("missing value for --{key}")))
                    }
                    Some(n) => {
                        i += 1;
                        n.clone()
                    }
                }
            }
        };
        if flags.contains_key(key) {
            return Err(UsageError(format!("duplicate flag --{key}")));
        }
        flags.insert(key.to_string(), value);
        i += 1;
    }
    Ok(ParsedArgs { flags, rest })
}

pub fn require_flag(args: &ParsedArgs, name: &str) -> Result<String, UsageError> {
    match args.flags.get(name) {
        Some(v) if !v.is_empty() => Ok(v.clone()),
        _ => Err(UsageError(format!("missing required flag --{name}"))),
    }
}

pub fn optional_flag(args: &ParsedArgs, name: &str) -> Option<String> {
    match args.flags.get(name) {
        Some(v) if !v.is_empty() => Some(v.clone()),
        _ => None,
    }
}

pub fn parse_int_flag(args: &ParsedArgs, name: &str, fallback: i64) -> Result<i64, UsageError> {
    match optional_flag(args, name) {
        None => Ok(fallback),
        Some(v) => v
            .parse::<i64>()
            .map_err(|_| UsageError(format!("--{name} must be an integer"))),
    }
}

/// TS `parseFloatFlag` — 양의 유한수만 허용.
pub fn parse_float_flag(args: &ParsedArgs, name: &str, fallback: f64) -> Result<f64, UsageError> {
    match optional_flag(args, name) {
        None => Ok(fallback),
        Some(v) => {
            let n: f64 = v
                .parse()
                .map_err(|_| UsageError(format!("--{name} must be a positive number")))?;
            if !n.is_finite() || n <= 0.0 {
                return Err(UsageError(format!("--{name} must be a positive number")));
            }
            Ok(n)
        }
    }
}

pub fn bool_flag(args: &ParsedArgs, name: &str) -> bool {
    optional_flag(args, name).as_deref() == Some("1")
}
