//! `dgate` 진입 바이너리 — `dg`와 완전 동일한 명령면을 공유 lib로 제공한다.
//! exit 직전 stdout/stderr를 명시적으로 flush한다(process::exit은 버퍼를 버린다).
use std::io::Write;

fn main() {
    let code = dgate_cli::run(std::env::args().skip(1));
    let _ = std::io::stdout().flush();
    let _ = std::io::stderr().flush();
    std::process::exit(code);
}
