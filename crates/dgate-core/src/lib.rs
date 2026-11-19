//! dgate-core: 결정 엔진 코어 라이브러리 (v2 Rust).
//!
//! 전체 코어 모듈: `redact` · `db` · `engine` · `verdict` · `telemetry` · `roster` ·
//! `stats` · `checkpoint` · `report` · `adapter` · `hook` · `live_sync` · `setup`.

pub mod adapter;
pub mod checkpoint;
pub mod db;
pub mod engine;
pub mod hook;
pub mod live_sync;
pub mod redact;
pub mod report;
pub mod roster;
pub mod setup;
pub mod stats;
pub mod telemetry;
pub mod verdict;

#[cfg(test)]
mod testutil;

/// 패키지가 정상 빌드·로드되었는지 확인하는 상수.
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanity_core_version() {
        assert_eq!(CORE_VERSION, "2.0.0");
    }
}
