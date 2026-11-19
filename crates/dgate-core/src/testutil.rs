//! 테스트 전용 공용 헬퍼 — `#[cfg(test)]` 에서만 컴파일된다.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};

static COUNTER: AtomicU64 = AtomicU64::new(0);

/// 프로세스 고유 임시 디렉터리(자동 생성하지 않는다 — 호출자가 create_dir_all).
pub fn temp_dir(tag: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "dgate-core-{}-{}-{}-{}",
        tag,
        std::process::id(),
        nanos,
        n
    ))
}

/// env 변형 테스트의 직렬화 락 — Rust 테스트는 프로세스 내 병렬이므로
/// `std::env::set_var` 경합을 막으려면 이 락을 잡고 env을 다뤄야 한다.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static M: OnceLock<Mutex<()>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
