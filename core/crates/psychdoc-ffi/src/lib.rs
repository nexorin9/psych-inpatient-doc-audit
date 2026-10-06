//! `psychdoc-ffi`：Rust 核心与 Python（PySide6 壳）之间的绑定层。
//!
//! 分三块：
//!
//! - [`api`]：5 个同步入口的**业务实现**，不依赖 pyo3 —— `cargo test` 直接可测；
//! - `bindings`（feature `extension-module`）：PyO3 外壳，只做「值 → Python 对象」
//!   搬运与错误码透传；
//! - `jsonval`（同上 feature）：`serde_json::Value` → Python 值的递归映射。
//!
//! 不开 `extension-module` feature 时本 crate 仍可 `cargo build` / `cargo test`
//! ——CI 与本轮回归都走这条路径，不要求先装 Python 绑定。
//!
//! 源码侧对应关系见 `spec.md` §6 / §7：源产品的「一条命令拿结果、
//! 结构化返回、错误分诊断与运行期」这条主路径被原样保留，
//! 只是把「模块 / 函数」换成了「文书件 / 规则码」。
//!
//! **核心侧全部同步、无内部线程、无内部 tokio runtime。**
//! 重活由 PySide6 侧 `QtConcurrent` 承担；不在 Rust 里另起线程池，
//! 是为了保证「一次调用」的行为完全可复现（截图与回归都靠它）。

pub mod api;

#[cfg(feature = "extension-module")]
mod bindings;
#[cfg(feature = "extension-module")]
mod jsonval;

pub use psychdoc_model as model;
pub use psychdoc_parse as parse;
pub use psychdoc_rules as rules;

/// 核心版本号：PyO3 绑定与 CLI 共用，避免壳与核心版本漂移。
pub const CORE_VERSION: &str = env!("CARGO_PKG_VERSION");

/// 桌面壳依赖的同步入口名。
///
/// 写成常量是为了让测试与 `run.sh` 自检都能断言「入口齐全」，
/// 而不是靠 grep 函数名——加入口忘了接 UI 时能立刻发现。
pub const ENTRY_POINTS: [&str; 8] = [
    "check_bundle",
    "review_batch",
    "preview_fixback",
    "commit_writeback",
    "graph_both_directions",
    // Task 32：拖放与批量队列。审查队列的发现与聚合（判定主体仍是 review_batch）。
    "scan_queue",
    // Task 33：文书卡片。一件件 id → 该件的段落 / 字段 / 诊断 / 补写预览。
    "doc_card",
    // Task 38：启动自检的逐项报告（不抛异常，桌面壳据此决定提示还是拦截）。
    "startup_report",
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_re_exports_are_linked() {
        assert!(!CORE_VERSION.is_empty());
        let _ = (model::EXAMPLE_BASIS, parse::parse_code::STRUCTURE, rules::QC_PSY_01);
    }

    #[test]
    fn every_entry_point_is_named() {
        assert_eq!(ENTRY_POINTS.len(), 8);
        for name in [
            "check_bundle",
            "review_batch",
            "preview_fixback",
            "commit_writeback",
            "graph_both_directions",
            "scan_queue",
            "doc_card",
            "startup_report",
        ] {
            assert!(ENTRY_POINTS.contains(&name), "缺少入口 {name}");
        }
        // 入口名不许重复：重名会让自检「看起来齐全」而实际漏接一个
        let mut sorted = ENTRY_POINTS.to_vec();
        sorted.sort_unstable();
        let before = sorted.len();
        sorted.dedup();
        assert_eq!(before, sorted.len(), "ENTRY_POINTS 里有重名");
    }
}
