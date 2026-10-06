//! `psychdoc-fhir`：**正式对接层** —— FHIR R4 REST 读 + Bundle transaction 写。
//!
//! ## 位置：为什么本项目不是只读旁路
//!
//! 业务上确实要**新建缺件骨架**。护士长在受控文书包里补一份约束知情同意，
//! 这一步在业务系统里就是「建件」；只读旁路意味着护士长仍要在 EMR 里手工
//! 建一遍，那本产品就只是多了一层看图。因此本层实现**真实写协议**：
//! `Bundle{type=transaction}` + `Bundle.entry.request` + `Provenance`。
//!
//! ## 凭据纪律（`spec.md` §4「正式对接层」这条闭环维度的落点）
//!
//! - 五项环境变量**只**从环境变量读，**不提供任何默认凭据**；
//! - 缺任一项 → [`FhirError::CfgMissing`]，**一次列全**缺失项；
//! - 形态错 → [`FhirError::CfgInvalid`]，与缺失**分开报码**；
//! - 加载与校验**不发起任何网络调用**——「缺配置」必须在 0 次外发的前提下被检出，
//!   否则「静默降级」就藏在校验的网络往返里。
//!
//! ## 替身纪律
//!
//! 脱敏 FHIR 协议替身（`StubTransport` / `FakeFhirServer`）**只允许出现在
//! `tests/` 与 `#[cfg(test)]` 里**，不是产品默认路径。产品默认路径永远是
//! [`UreqTransport`] 指向 `EMR_BASE_URL` 的真实端点。
//!
//! ## 源产品主路径的迁移位置
//!
//! 本层对应源产品 `analyze_graph.rs` 的「一条命令拿整张图」里的**取数**环节：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | `resolve_default_files` / `project_files_in_path` | [`read::read_package`] 按住院别名拉 searchset |
//! | `paths.retain(...)` 类型过滤 | [`read::filter_by_registry`] 按院内文书类型字典过滤 |
//! | 逐件解析 `Parsed<Mod>` | 逐件 `parse_bundle` → 件-字段-时间序三层 |
//! | 循环 `SourceOrderVisitor` 收集 | [`read::read_all_pages`] 跟随 `Bundle.link[next]` 有界遍历 |
//!
//! 源仓库**没有任何网络协议与业务实体**，故本层全部为新建（见 `spec.md` §7
//! 诚实声明）。借用的只是「一条命令拿全量、按类型过滤、逐件解析」这条形状。

pub mod auth;
pub mod config;
pub mod error;
pub mod notify;
pub mod read;
pub mod confirm;
pub mod startup;
pub mod sync;
pub mod transport;
pub mod wecom;
pub mod write;

pub use auth::{fetch_token, AccessToken};
pub use config::{EmrConfig, ENV_BASE_URL, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_TENANT_ID};
pub use confirm::{
    confirm as confirm_writeback, create_draft, draft_from_preview, set_selected,
    withdraw as withdraw_writeback, ConfirmContext, ConfirmError, ConfirmOutcome,
};
pub use error::{FhirError, Result};
pub use notify::{
    drain_pending, manual_queue, notify_once, sent_today, state_of, undelivered_summary,
    BackoffPolicy, DrainSummary, NotifyOutcome,
};
pub use read::{read_all_pages, read_package, PackageRead, UnresolvedItem, MAX_PAGES};
pub use startup::{
    resolve_optional, LogLevel, StartupConfig, StartupReport, ALL_CREDENTIAL_VARS, ENV_DATA_DIR,
    ENV_LOG_LEVEL, ENV_RULESET,
};
pub use sync::{
    get_with_retry, parse_retry_after_ms, since_url, sync_since, RetryLedger, RetryPolicy,
    Sleeper, SyncOutcome, ThreadSleeper,
};
pub use transport::{HttpMethod, HttpRequest, HttpResponse, Transport, UreqTransport};
pub use wecom::{
    build_markdown, build_payload, deliver as deliver_wecom, parse_ack, MissingLine, NoticeContent,
    WeComAck, WeComConfig, ENV_WECOM_WEBHOOK_URL, MAX_CONTENT_BYTES,
};
pub use write::{
    build_reversal, build_transaction, parse_transaction_response, post_bundle, send_transaction,
    TransactionOutcome, WriteEntry, WriteMethod, WritebackContext,
};

/// 启动自检：加载并校验凭据。
///
/// 返回 `Ok` 之前**零次网络调用**——这是 Task 11 proof 里的硬指标。
///
/// Task 38 起这一步读的是**五项**凭据（EMR 四项 + 通知 Webhook），
/// 由 [`startup::StartupConfig`] 一次列全缺失项，而不是分两次撞。
pub fn startup_check() -> Result<EmrConfig> {
    EmrConfig::from_env()
}
