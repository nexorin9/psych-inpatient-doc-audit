//! 存储层错误码。**每个码都是一等状态**：调用方据此决定重试、报人工还是直接失败。
//!
//! 命名与 Task 37「全量错误码表」共用同一套前缀（`E_DB_*` / `E_STATE_*`），
//! 避免文档与实现漂移——Task 37 会用「错误码表 ↔ 代码实际返回码一一对应」的
//! 测试守住这一点。

use psychdoc_model::state::DocState;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("E_DB_IO: sqlite 操作失败：{0}")]
    Db(#[from] rusqlite::Error),

    #[error("E_STATE_JUMP: 件 {doc_id} 不能从「{from}」推进到「{to}」")]
    StateJump {
        doc_id: String,
        from: &'static str,
        to: &'static str,
    },

    #[error("E_STATE_UNKNOWN: 库里没有件 {0}，不能推进状态")]
    StateUnknown(String),

    #[error("E_BAD_TIMESTAMP: {field} 不是 RFC3339 时刻（收到 {value}）")]
    BadTimestamp { field: &'static str, value: String },

    #[error("E_DATA_DIR: 无法确定用户数据目录（未设 HOME / APPDATA / XDG_DATA_HOME）")]
    DataDirUnresolved,

    #[error("E_DRAFT_UNKNOWN: 找不到草稿 {0}")]
    DraftUnknown(String),

    #[error("E_DRAFT_STATE: 草稿 {draft_id} 当前为「{state}」，不允许 {action}")]
    DraftState {
        draft_id: String,
        state: &'static str,
        action: &'static str,
    },

    #[error("E_NOTHING_SELECTED: 草稿 {0} 未勾选任何条目，拒绝发送")]
    NothingSelected(String),

    #[error("E_ENTRY_UNKNOWN: 草稿 {draft_id} 里没有件 {doc_id} 这一条")]
    EntryUnknown { draft_id: String, doc_id: String },

    #[error("E_RUN_UNKNOWN: 找不到审查批次 {0}")]
    RunUnknown(String),

    #[error("E_SCHEMA_EMPTY: 库尚未初始化（无任何表），须先跑迁移")]
    SchemaEmpty,

    #[error(
        "E_SCHEMA_TOO_NEW: 库 schema 版本 v{db_version} 高于本程序支持的 v{program_version}，\
         拒绝启动（不退化为只读，也不做任何写入）"
    )]
    SchemaTooNew {
        db_version: i64,
        program_version: i64,
    },

    #[error("E_MIGRATION_FAILED: 迁移 v{version}（{name}）{stage}：{detail}；本条已整体回滚")]
    MigrationFailed {
        version: i64,
        name: &'static str,
        stage: &'static str,
        detail: String,
    },

    #[error(
        "E_WRITEBACK_CONFLICT: 键 {key} 已由「{first_actor}」于 {first_at} 占键提交中（{state}），\
         本次不重复外发；如确认那次已失败，请先在「归档核齐」里释放该键"
    )]
    WritebackConflict {
        key: String,
        first_actor: String,
        first_at: String,
        state: &'static str,
    },

    #[error(
        "E_REPLAY_IN_DOUBT: 键 {key} 上次由「{first_actor}」于 {first_at} 提交，\
         进程在结果落库前中断，本次**不重发**；请先人工核对院内端点是否已建件，\
         再在「归档核齐」里标记为「已写回」或「未写回」"
    )]
    ReplayInDoubt {
        key: String,
        first_actor: String,
        first_at: String,
    },

    #[error("E_CLAIM_UNKNOWN: 批次 {0} 下没有处于 in_flight 的预占键")]
    ClaimUnknown(String),

    #[error(
        "E_CLAIM_STATE: 预占键 {key} 当前为「{state}」，不允许 {action}"
    )]
    ClaimState {
        key: String,
        state: &'static str,
        action: &'static str,
    },

    #[error(
        "E_CURSOR_REWIND: {tenant_id} / {resource_type} 的增量游标当前为 {current}，\
         不允许回退到 {attempted}（回退会让变更区间重复拉取并漏掉区间内的件）"
    )]
    CursorRewind {
        tenant_id: String,
        resource_type: String,
        current: String,
        attempted: String,
    },

    #[error("E_CURSOR_SHAPE: 增量游标 {since} 不是带时区偏移的 RFC3339 时刻（{field}）")]
    CursorShape { field: &'static str, since: String },

    #[error("E_NOTIFY_UNKNOWN: 发件箱里找不到幂等键 {0} 对应的那条通知")]
    NotifyUnknown(String),

    #[error("E_QUEUE_EMPTY: 队列里一条住院都没有，拒绝落库（空队列会让重开后的左栏空着）")]
    QueueEmpty,

    #[error("E_QUEUE_NOTHING_SELECTED: 队列里未勾选任何住院，拒绝标记已审")]
    QueueNothingSelected,

    #[error("E_QUEUE_DUPLICATE_ENCOUNTER: 队列里住院别名 {0} 出现两次，拒绝落库（上游聚合出错）")]
    QueueDuplicateEncounter(String),

    #[error("E_QUEUE_UNKNOWN_ENCOUNTER: 队列里没有住院别名 {0} 这一条，拒绝标记已审")]
    QueueUnknownEncounter(String),

    #[error("E_EXPORT_LEAK: {0}")]
    ExportLeak(String),
}

impl StoreError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Db(_) => "E_DB_IO",
            Self::StateJump { .. } => "E_STATE_JUMP",
            Self::StateUnknown(_) => "E_STATE_UNKNOWN",
            Self::BadTimestamp { .. } => "E_BAD_TIMESTAMP",
            Self::DataDirUnresolved => "E_DATA_DIR",
            Self::DraftUnknown(_) => "E_DRAFT_UNKNOWN",
            Self::DraftState { .. } => "E_DRAFT_STATE",
            Self::NothingSelected(_) => "E_NOTHING_SELECTED",
            Self::EntryUnknown { .. } => "E_ENTRY_UNKNOWN",
            Self::RunUnknown(_) => "E_RUN_UNKNOWN",
            Self::SchemaEmpty => "E_SCHEMA_EMPTY",
            Self::SchemaTooNew { .. } => "E_SCHEMA_TOO_NEW",
            Self::MigrationFailed { .. } => "E_MIGRATION_FAILED",
            Self::WritebackConflict { .. } => "E_WRITEBACK_CONFLICT",
            Self::ReplayInDoubt { .. } => "E_REPLAY_IN_DOUBT",
            Self::ClaimUnknown(_) => "E_CLAIM_UNKNOWN",
            Self::ClaimState { .. } => "E_CLAIM_STATE",
            Self::CursorRewind { .. } => "E_CURSOR_REWIND",
            Self::CursorShape { .. } => "E_CURSOR_SHAPE",
            Self::NotifyUnknown(_) => "E_NOTIFY_UNKNOWN",
            Self::QueueEmpty => "E_QUEUE_EMPTY",
            Self::QueueNothingSelected => "E_QUEUE_NOTHING_SELECTED",
            Self::QueueDuplicateEncounter(_) => "E_QUEUE_DUPLICATE_ENCOUNTER",
            Self::QueueUnknownEncounter(_) => "E_QUEUE_UNKNOWN_ENCOUNTER",
            Self::ExportLeak(_) => "E_EXPORT_LEAK",
        }
    }

    /// 是否值得稍后重试：`E_WRITEBACK_CONFLICT` 值得（对方提交完就放开了），
    /// `E_REPLAY_IN_DOUBT` **不值得**——它要的是人工核对，不是再点一次。
    pub fn is_retryable(&self) -> bool {
        matches!(self, Self::WritebackConflict { .. })
    }

    /// 状态跳变错误的便捷构造（与 `psychdoc_model::StateError::Jump` 同形，
    /// 存储层只补齐「库里没有这件」这一种额外情形）。
    pub fn jump(doc_id: &str, from: DocState, to: DocState) -> Self {
        Self::StateJump {
            doc_id: doc_id.to_string(),
            from: from.as_zh(),
            to: to.as_zh(),
        }
    }
}

pub type Result<T> = std::result::Result<T, StoreError>;
