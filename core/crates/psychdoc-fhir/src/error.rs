//! 正式对接层的错误码。**缺配置不是「本地假成功」的理由**。
//!
//! 与 `psychdoc_store::StoreError` 共用「每个码都是一等状态」的纪律：
//! 调用方据此决定重试、报人工还是直接失败。`E_CFG_MISSING` 与
//! `E_CFG_INVALID` **分开报**——「没配」与「配错了」是两种处置：
//! 前者是运维去补环境变量，后者是运维去查配置内容。

use thiserror::Error;

#[derive(Debug, Error)]
pub enum FhirError {
    /// 启动自检：一次列全所有缺失项，而不是报一个报一个。
    #[error("E_CFG_MISSING: 缺少 {count} 项必需环境变量：{missing}（不会降级为本地替身或离线模式）")]
    CfgMissing { count: usize, missing: String },

    /// 配置存在但形态不合法（URL 没头、tenant 含空白、secret 全空格等）。
    #[error("E_CFG_INVALID: 配置项 {key} 形态不合法 — {detail}")]
    CfgInvalid { key: &'static str, detail: String },

    /// 端点不可达 / 连接失败 / 超时。
    #[error("E_FHIR_UNREACHABLE: 无法连接 {url} — {detail}")]
    Unreachable { url: String, detail: String },

    /// 端点返回非 2xx。
    #[error("E_FHIR_STATUS: {method} {url} 返回 {status} — {detail}")]
    Status {
        method: &'static str,
        url: String,
        status: u16,
        detail: String,
    },

    /// 响应体不是合法 JSON / 不是 FHIR 资源形状。
    #[error("E_FHIR_SHAPE: 响应体不符合 FHIR R4 结构 — {detail}")]
    Shape { detail: String },

    /// 响应体字段缺失或形态不对（如缺 `access_token`、缺 `entry`）。
    #[error("E_FHIR_FIELD: 响应缺字段 {field}（{path}）")]
    MissingField { field: &'static str, path: String },

    /// 写回 transaction 被端点拒绝或未完整成功。
    ///
    /// **绝不允许出现「部分成功」**：要么整批生效（`TransactionOutcome`），
    /// 要么零条生效、本地草稿保留。
    #[error("E_FHIR_TXN_REJECTED: Bundle transaction 未整批成功（{failed}/{total} 条失败）— {detail}；本地草稿保留，未产生任何已生效事实")]
    TxnRejected {
        failed: usize,
        total: usize,
        detail: String,
    },

    /// 写回请求根本没发出去（构造阶段就失败），零次网络调用。
    #[error("E_FHIR_TXN_EMPTY: 写回 selection 为空，transaction 拒绝发送")]
    TxnEmpty,

    /// 分页游标自环（服务端 `Bundle.link[next]` 指回本页），有界遍历防死循环。
    #[error("E_FHIR_PAGING: 分页游标自环（{url}），已中止遍历")]
    PagingLoop { url: String },

    /// 有界重试耗尽。**绝不降级为空集**——空集在界面上是「今天没有缺件」，
    /// 和「拉取失败」长得一模一样，而护士长会照着空结果做交接。
    #[error(
        "E_FHIR_RETRIES_EXHAUSTED: GET {url} 重试 {attempts} 次仍失败 — {detail}；\
         本次增量拉取作废、游标不推进（下次从上次成功点重来），不会当作「没有变更」"
    )]
    RetriesExhausted {
        url: String,
        attempts: u32,
        detail: String,
    },

    /// 企业微信 Webhook 返回非 2xx。
    #[error("E_WECOM_STATUS: 企业微信 Webhook 返回 {status} — {detail}")]
    WecomStatus { status: u16, detail: String },

    /// **HTTP 200 但业务码非 0**：限流 / 机器人不在群里 / key 失效。
    ///
    /// 这一条最要紧：只看 HTTP 状态码会把这些全记成「已送达」，
    /// 缺件提醒就此静默丢失，而界面上看不出任何区别。
    #[error("E_WECOM_REJECTED: 企业微信返回 errcode={errcode}（{errmsg}）——本条**未送达**，已留在发件箱待重试")]
    WecomRejected { errcode: i64, errmsg: String },

    /// Webhook 应答体不是企业微信规范形状（缺 `errcode` / 非 JSON）。
    #[error("E_WECOM_SHAPE: 企业微信应答体不符合群机器人规范 — {detail}")]
    WecomShape { detail: String },

    /// 存储侧失败（发件箱落库）在并入本层时**保留原始码**。
    ///
    /// 不压成 `E_DB_IO`：调用方按码分处置，`E_NOTIFY_UNKNOWN` 与
    /// 「sqlite 磁盘满」要走的路完全不同。
    #[error("{code}: {detail}")]
    Store { code: &'static str, detail: String },
}

impl FhirError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::CfgMissing { .. } => "E_CFG_MISSING",
            Self::CfgInvalid { .. } => "E_CFG_INVALID",
            Self::Unreachable { .. } => "E_FHIR_UNREACHABLE",
            Self::Status { .. } => "E_FHIR_STATUS",
            Self::Shape { .. } => "E_FHIR_SHAPE",
            Self::MissingField { .. } => "E_FHIR_FIELD",
            Self::TxnRejected { .. } => "E_FHIR_TXN_REJECTED",
            Self::TxnEmpty => "E_FHIR_TXN_EMPTY",
            Self::PagingLoop { .. } => "E_FHIR_PAGING",
            Self::RetriesExhausted { .. } => "E_FHIR_RETRIES_EXHAUSTED",
            Self::WecomStatus { .. } => "E_WECOM_STATUS",
            Self::WecomRejected { .. } => "E_WECOM_REJECTED",
            Self::WecomShape { .. } => "E_WECOM_SHAPE",
            Self::Store { code, .. } => code,
        }
    }

    /// 端点不可达 / 429 / 5xx 这类错误值得重试；配置与形状问题重试没有意义。
    ///
    /// 注意 `500..` 是**半开区间**的下界写法：`status: 5..` 会把 404 也
    /// 算成「服务端错误」而重试——404 是「这个地址没这个资源」，再试十次
    /// 也是 404，只会白白等完退避才报错。429 单列：它是限流而非故障，
    /// 但**恰恰是最该退避后重试**的那一种。
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Unreachable { .. } | Self::Status { status: 429 | 500.., .. }
        )
    }
}

pub type Result<T> = std::result::Result<T, FhirError>;
