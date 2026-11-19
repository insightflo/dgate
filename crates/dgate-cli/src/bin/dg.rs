//! `dg` 진입 바이너리 — 공유 라이브러리 `dgate_cli::run`에 전적으로 위임한다.
//! exit 직전 stdout/stderr를 명시적으로 flush한다(process::exit은 버퍼를 버린다).
use std::io::Write;

fn main() {
    let code = dgate_cli::run(std::env::args().skip(1));
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}
