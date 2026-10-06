//! `psychdoc-store`：件级状态与审查结果的本机持久化。
//!
//! 支撑 `spec.md` §4 的三个闭环维度，各由一个可核对的落点承担：
//!
//! - **幂等**：`sha256(住院别名 + 件 id + 内容哈希)` 为幂等键，写回表上建
//!   **唯一约束**。同一件同日重复确认 → 新增 0 行 `writeback_log`，
//!   返回 `CommitOutcome::AlreadyExists` 而非新记录。重复插入**不产生**
//!   第二条 `kind=writeback` 审计事件——「没有新写回」必须是可核对的事实。
//! - **安全审计**：每次**调阅**与每次**写回**各留一条 Provenance，携带规则集
//!   `sha256`、命中码集合、推进人与时刻。
//! - **版本回溯**：给定住院别名 + 时间点，还原当时生效的规则集哈希与命中码
//!   集合，支撑质控办「某份病历当年按哪版审的」。
//!
//! 另含**写回草稿区**：草稿可撤回，撤回**不产生已生效事实**（只把
//! `writeback_drafts.state` 改回 `draft`）。这条对应 `spec.md` 人工确认点
//! 「写回先落本地草稿 → 逐条勾选 → 一次性发 transaction，撤回 = 回草稿」。
//!
//! 库文件落位见 [`paths`]：**用户数据目录**，绝不落在 round 根或项目子目录。

pub mod audit;
pub mod cache;
pub mod claim;
pub mod cursor;
pub mod error;
pub mod export;
pub mod notify;
pub mod observe;
pub mod paths;
pub mod queue;
pub mod schema;

use std::path::{Path, PathBuf};

use psychdoc_model::state::DocState;
use psychdoc_model::{is_legal, Diagnostic, DocItem};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};

pub use audit::AuditKind;
pub use cache::{CacheInvalidation, CacheRow, CacheScope, CacheWrite};
pub use claim::{ClaimRow, ClaimTarget, RefusalReason, ReserveOutcome, Resolution};
pub use cursor::{CursorKey, SyncCursor};
pub use error::{Result, StoreError};
pub use export::{
    archive_to_csv, csv_escape, redact_rows, to_csv, to_json, ArchiveViewRow, AuditBundle,
    AuditRow, ExportQuery, RedactionReport, RunRollup, COLUMNS as AUDIT_COLUMNS,
    UTF8_BOM as AUDIT_UTF8_BOM,
};
pub use notify::{
    backoff_seconds, channel, notify_key, state as notify_state, EnqueueOutcome, OutboxRow,
    MAX_ATTEMPTS_DEFAULT,
};
pub use observe::{
    error_codes_markdown, event, log_dir, log_file_name, Event, Level, RunLogger, RunSummary,
};
pub use queue::{
    clear_reviewed, delete_queue, latest_queue, list_queue_ids, load_queue, mark_reviewed,
    merge_reviewed_at, queue_id, reviewed_progress, save_queue, QueueEntryRow, QueueSession,
    QueueSnapshot,
};
pub use schema::{
    migrate_to_v6, migrate_to_v7, schema_version, Migration, MigrationReport, TableSpec,
    SCHEMA_VERSION_LATEST, SCHEMA_VERSION_V1, SCHEMA_VERSION_V2, SCHEMA_VERSION_V3,
    SCHEMA_VERSION_V4, SCHEMA_VERSION_V5, SCHEMA_VERSION_V6, SCHEMA_VERSION_V7, MIGRATIONS,
};

/// 幂等键 = `sha256(住院别名 + 件 id + 内容哈希)`。
///
/// 三个分量缺一不可：只取内容哈希，换了住院别名会误命中；只取件 id，
/// 补写内容变了会被旧键挡住（那是 Task 28 的「内容变更后重提」语义）。
pub fn idempotency_key(encounter_alias: &str, doc_id: &str, content_sha256: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(encounter_alias.as_bytes());
    hasher.update(b"\x1f"); // 字段分隔，避免 "ab"+"c" 与 "a"+"bc" 撞键
    hasher.update(doc_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(content_sha256.as_bytes());
    format!("{:x}", hasher.finalize())
}

fn content_sha256_of(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 一次写回的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitOutcome {
    /// 全新写回：`keys` 为本次落库的幂等键。
    Committed { keys: Vec<String> },
    /// 幂等命中：这些键此前已写回，本次**新增 0 行**。
    AlreadyExists { keys: Vec<String> },
}

impl CommitOutcome {
    /// 本次真正新增的写回行数。幂等命中恒为 0。
    pub fn new_rows(&self) -> usize {
        match self {
            Self::Committed { keys } => keys.len(),
            Self::AlreadyExists { .. } => 0,
        }
    }

    /// 受影响（新建或幂等复用）的键数。
    pub fn touched(&self) -> usize {
        match self {
            Self::Committed { keys } | Self::AlreadyExists { keys } => keys.len(),
        }
    }
}

/// 一次审查批次的落库参数。
#[derive(Debug, Clone)]
pub struct ReviewRunInput<'a> {
    pub run_id: &'a str,
    pub encounter_alias: &'a str,
    pub ruleset_version: &'a str,
    /// 规则集源文件 sha256（64 位十六进制）。随批次落盘，供事后回溯。
    pub ruleset_sha256: &'a str,
    pub actor: &'a str,
    pub started_at: &'a str,
    pub finished_at: Option<&'a str>,
    pub items_total: usize,
    pub items_reviewed: usize,
    pub items_skipped: usize,
    pub unresolved_count: usize,
    pub waived_count: usize,
    pub diagnostics: &'a [Diagnostic],
    /// 被更具体规则压下、但**未丢弃**的命中（Task 23 起；Task 31 落进证据包）。
    ///
    /// 与 `diagnostics` 合起来等于折叠前的条数。少传这一组，导出包里的
    /// 「当年报了几条」就只是折叠后的数——质控办拿它跟当年的界面截图
    /// 对不上，会以为漏审了。**压下也是事实**，事实进证据包。
    pub suppressed: &'a [Diagnostic],
    /// 每条规则码的生效层级溯源（Task 5 落 `provenance()`，Task 31 落进证据包）。
    ///
    /// `code → "QC-PSY-02（科室级，覆盖 院级）"`。同一码在不同病区可能
    /// 生效依据不同，不带这列就答不了「为什么这个病区报得这么松」。
    pub rule_sources: &'a [(String, String)],
}

/// 一次审查的归档核齐视图快照（Task 21 出视图，Task 31 随批次落库）。
///
/// `view_json` 是 `psychdoc-graph::ArchiveView` 的**原样序列化**。
/// 刻意不拆成列重拼：视图的口径说明（`basis_zh`）是界面上写给人看的，
/// 拆列再拼一遍必然与界面对不上，而对不上的导出比没有导出更糟。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveSnapshot {
    pub view_id: String,
    pub run_id: String,
    pub encounter_alias: String,
    /// 视图以哪个时刻算的（由业务上下文给，不读系统时钟）。
    pub as_of: String,
    /// 能不能归档签字。`ArchiveView::is_ready()` 的结论，随包带走。
    pub is_ready: bool,
    pub view_json: String,
    pub recorded_at: String,
}

impl ArchiveSnapshot {
    /// 幂等键 = `sha256(run_id + as_of)`。
    ///
    /// 不含 `view_json`：同一批次同一时刻反复存**不追加第二条**，否则
    /// 每次打开「归档核齐」都会让证据包多出一份重复视图，质控办
    /// 数不清到底核过几次。
    pub fn view_id(run_id: &str, as_of: &str) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(run_id.as_bytes());
        // 分隔符防字段边界串位：「ab|c」与「a|bc」不得撞成同一个键。
        h.update(b"\x1f");
        h.update(as_of.as_bytes());
        format!("{:x}", h.finalize())
    }
}

/// 往 `diagnostics` 插一行。`disposition` 区分「清单上留下的」与「被压下的」。
fn insert_diagnostic(
    tx: &rusqlite::Transaction<'_>,
    run_id: &str,
    d: &Diagnostic,
    disposition: &str,
    rule_source: &str,
) -> Result<()> {
    tx.execute(
        "INSERT INTO diagnostics
           (run_id, doc_id, section_index, char_start, char_end,
            code, severity, message, basis, fix_availability,
            waived, suppressed_by, disposition, rule_source)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        params![
            run_id,
            d.loc.doc_id,
            d.loc.section_index as i64,
            d.loc.char_range.start as i64,
            d.loc.char_range.end as i64,
            d.code,
            format!("{:?}", d.severity).to_lowercase(),
            d.message.text,
            d.message.basis,
            d.fix_availability.to_string(),
            d.waived,
            d.suppressed_by,
            disposition,
            rule_source,
        ],
    )?;
    Ok(())
}

/// 按当时规则集版本回溯到的结果。
///
/// `Serialize`：导出包要能把它当「版本回溯结论」原样带给质控办
/// （`psychdoc_store::export::AuditBundle::recalled`），不必自己再转一层。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RulesetRecall {
    pub run_id: String,
    pub ruleset_version: String,
    pub ruleset_sha256: String,
    pub finished_at: Option<String>,
    /// 该批次**报出**的规则码集合（去重后升序）。
    pub codes: Vec<String>,
    /// 该批次**被折叠压下**的规则码集合（去重后升序）。
    ///
    /// 与 `codes` 分开：不分开的话，回溯结论里会混进清单上根本没显示的码，
    /// 质控办照着核对会以为少审了——而实际只是被去重层折叠了。
    pub suppressed_codes: Vec<String>,
}

/// 一次写回条目的落库参数。
#[derive(Debug, Clone)]
pub struct WritebackEntry<'a> {
    pub doc_id: &'a str,
    pub content_sha256: &'a str,
    pub run_id: Option<&'a str>,
}

/// 本机 sqlite 存储。
pub struct Store {
    conn: Connection,
    path: Option<PathBuf>,
}

impl Store {
    /// 打开（必要时创建）指定路径的库并迁到 v1。
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| StoreError::DataDirUnresolved)?;
        }
        let conn = Connection::open(&path)?;
        Self::from_conn(conn, Some(path))
    }

    /// 内存库（测试用；不落盘，同样走一遍 schema）。
    pub fn open_in_memory() -> Result<Self> {
        Self::from_conn(Connection::open_in_memory()?, None)
    }

    /// 库文件落用户数据目录（见 [`paths`]）。**不**落在 round 根或项目子目录。
    pub fn open_default() -> Result<Self> {
        Self::open(paths::db_path()?)
    }

    fn from_conn(conn: Connection, path: Option<PathBuf>) -> Result<Self> {
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // 并发写的前提：WAL 让「一个线程写、另一个线程读」互不阻塞
        // （常驻进程的后台批扫不该把 UI 查询卡住）；busy_timeout 让
        // 撞上写锁的读者等 5 秒而不是立刻报 `database is locked`。
        // 内存库不支持这两条 pragma，设了会报错，因此只在有路径时设。
        if path.is_some() {
            let _ = conn.pragma_update(None, "journal_mode", "WAL");
            let _ = conn.busy_timeout(std::time::Duration::from_secs(5));
        }
        // 启动自检 → 版本化迁移链（Task 27）。**自检排在迁移之前**：库比程序
        // 新时必须在任何一条 DDL 执行之前就拒掉，否则旧代码会先建表再发现
        // 版本对不上。
        schema::startup_check(&conn)?;
        schema::migrate(&conn)?;
        Ok(Self { conn, path })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }

    // ---------------------------------------------------------------- 件

    /// 登记一批件（发现即登记）。已存在的件更新 `content_sha256` 与
    /// `last_seen_at`，`first_seen_at` 保持首次登记时刻。
    pub fn register_items(&self, items: &[DocItem], at: &str) -> Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        let mut n = 0usize;
        for item in items {
            let content = content_of(item);
            tx.execute(
                "INSERT INTO doc_items
                   (doc_id, template_id, encounter_alias, source_system,
                    content_sha256, binding, first_seen_at, last_seen_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)
                 ON CONFLICT(doc_id) DO UPDATE SET
                   content_sha256 = excluded.content_sha256,
                   binding        = excluded.binding,
                   last_seen_at   = excluded.last_seen_at",
                params![
                    item.doc_id,
                    item.template_id,
                    item.encounter_alias,
                    item.source_system,
                    content,
                    format!("{:?}", item.binding),
                    at,
                ],
            )?;
            n += 1;
        }
        tx.commit()?;
        Ok(n)
    }

    pub fn item_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM doc_items", [], |r| r.get(0))?)
    }

    // ------------------------------------------------------------ 状态机

    /// 推进件级状态。非法跳变返回 `E_STATE_JUMP` 且**不改变既有状态**
    ///（校验先于写入）。
    pub fn advance_state(
        &self,
        doc_id: &str,
        to: DocState,
        actor: &str,
        at: &str,
    ) -> Result<()> {
        let current: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM doc_states WHERE doc_id = ?1",
                params![doc_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(from_raw) = current else {
            return Err(StoreError::StateUnknown(doc_id.to_string()));
        };
        let from = parse_state(&from_raw);
        if !is_legal(from, to) {
            return Err(StoreError::jump(doc_id, from, to));
        }
        self.conn.execute(
            "UPDATE doc_states SET state = ?2, last_actor = ?3, updated_at = ?4
             WHERE doc_id = ?1",
            params![doc_id, state_name(to), actor, at],
        )?;
        // 首次进入缺失态的时刻**记下来且不清**：欠件天数是从这一刻算的，
        // 拿 `updated_at` 顶替等于每次推状态都把「欠了 5 天」改写成
        // 「今天才发现」——那正是欠件天数这个字段唯一存在的理由。
        // 离开缺失态时置空，重新缺件时从头记。
        if to == DocState::Missing {
            self.conn.execute(
                "UPDATE doc_states SET missing_since = ?2 WHERE doc_id = ?1 AND missing_since IS NULL",
                params![doc_id, at],
            )?;
        } else {
            self.conn.execute(
                "UPDATE doc_states SET missing_since = NULL WHERE doc_id = ?1",
                params![doc_id],
            )?;
        }
        Ok(())
    }

    /// 该件**首次**进入缺失态的时刻（RFC3339）。未进过缺失态则 `None`。
    ///
    /// `None` 不是「今天才发现」——调用方应据此让欠件天数留空并给出口径
    /// 说明，而不是填 0。填 0 会让「欠了 0 天」看着像「刚发现」，
    /// 病案室据此催办就会误判紧迫程度。
    pub fn missing_since(&self, doc_id: &str) -> Result<Option<String>> {
        let row: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT missing_since FROM doc_states WHERE doc_id = ?1",
                params![doc_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(row.flatten())
    }

    pub fn state_of(&self, doc_id: &str) -> Result<Option<DocState>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT state FROM doc_states WHERE doc_id = ?1",
                params![doc_id],
                |r| r.get(0),
            )
            .optional()?;
        Ok(raw.as_deref().map(parse_state))
    }

    /// 初始化件级状态（首次登记时调用）。已存在则原样返回，不覆盖。
    pub fn init_state(&self, doc_id: &str, initial: DocState, actor: &str, at: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO doc_states (doc_id, state, last_actor, updated_at, history_json)
             VALUES (?1, ?2, ?3, ?4, '[]')
             ON CONFLICT(doc_id) DO NOTHING",
            params![doc_id, state_name(initial), actor, at],
        )?;
        Ok(())
    }

    // -------------------------------------------------------------- 审查

    /// 落一次审查批次：批次行 + 全部诊断（含被压下的）+ **恰好一条**
    /// `kind=review` 审计事件。
    pub fn record_review(&self, input: &ReviewRunInput<'_>) -> Result<()> {
        let source_of = |code: &str| -> String {
            input
                .rule_sources
                .iter()
                .find(|(c, _)| c == code)
                .map(|(_, s)| s.clone())
                .unwrap_or_default()
        };
        let mut codes: Vec<String> = input.diagnostics.iter().map(|d| d.code.clone()).collect();
        codes.sort();
        codes.dedup();
        let codes_json = serde_json::to_string(&codes).unwrap_or_else(|_| "[]".into());
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO review_runs
               (run_id, encounter_alias, ruleset_version, ruleset_sha256, actor,
                started_at, finished_at, items_total, items_reviewed, items_skipped,
                unresolved_count, waived_count)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                input.run_id,
                input.encounter_alias,
                input.ruleset_version,
                input.ruleset_sha256,
                input.actor,
                input.started_at,
                input.finished_at,
                input.items_total as i64,
                input.items_reviewed as i64,
                input.items_skipped as i64,
                input.unresolved_count as i64,
                input.waived_count as i64,
            ],
        )?;
        for d in input.diagnostics {
            let src = source_of(&d.code);
            insert_diagnostic(&tx, input.run_id, d, "reported", &src)?;
        }
        // 压下的那组同样落库，只是 `disposition='suppressed'`。
        // 导出时它们单独成列，质控办能同时看到「报了什么」与「压了什么」。
        for d in input.suppressed {
            let src = source_of(&d.code);
            insert_diagnostic(&tx, input.run_id, d, "suppressed", &src)?;
        }
        tx.execute(
            "INSERT INTO audit_events
               (event_at, kind, encounter_alias, doc_id, run_id,
                ruleset_sha256, codes_json, actor, idempotency_key)
             VALUES (?1,'review',?2,NULL,?3,?4,?5,?6,NULL)",
            params![
                input.finished_at.unwrap_or(input.started_at),
                input.encounter_alias,
                input.run_id,
                input.ruleset_sha256,
                codes_json,
                input.actor,
            ],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// 随批次落一份归档核齐视图快照（Task 31）。
    ///
    /// 幂等键 `sha256(run_id + as_of)`：同一批次同一时刻反复存不追加第二条，
    /// 否则每次打开「归档核齐」都会让证据包多出一份重复的视图，
    /// 质控办数不清到底核过几次。
    pub fn record_archive_view(&self, view: &ArchiveSnapshot) -> Result<()> {
        self.conn.execute(
            "INSERT INTO archive_views
               (view_id, run_id, encounter_alias, as_of, is_ready, view_json, recorded_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7)
             ON CONFLICT(view_id) DO NOTHING",
            params![
                view.view_id,
                view.run_id,
                view.encounter_alias,
                view.as_of,
                view.is_ready as i64,
                view.view_json,
                view.recorded_at,
            ],
        )?;
        Ok(())
    }

    pub fn review_run_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM review_runs", [], |r| r.get(0))?)
    }

    pub fn diagnostic_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM diagnostics", [], |r| r.get(0))?)
    }

    /// 给定住院别名 + 时间点，还原**当时生效**的规则集哈希与命中码集合。
    ///
    /// 取 `started_at <= at` 的最近一次审查（同一时刻有多次时按 `run_id`
    /// 升序取最大，保证可复现）。找不到返回 `None`——**不是**返回「最新一版」，
    /// 否则回溯会答非所问。
    pub fn recall_ruleset(&self, encounter_alias: &str, at: &str) -> Result<Option<RulesetRecall>> {
        let row: Option<(String, String, String, Option<String>)> = self
            .conn
            .query_row(
                "SELECT run_id, ruleset_version, ruleset_sha256, finished_at
                   FROM review_runs
                  WHERE encounter_alias = ?1 AND started_at <= ?2
                  ORDER BY started_at DESC, run_id ASC
                  LIMIT 1",
                params![encounter_alias, at],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;
        let Some((run_id, ruleset_version, ruleset_sha256, finished_at)) = row else {
            return Ok(None);
        };
        // 与批次小结**同一个口径**：留下的与压下的分开列，不合并成一个 codes。
        // 回溯要答的是「当年按哪版审的、报了哪些」——把压下的码混进去，
        // 质控办会照着不存在的清单去核对，而那正是导出的用途所在。
        let mut codes = Vec::new();
        let mut suppressed_codes = Vec::new();
        for (disposition, bucket) in [
            ("reported", &mut codes),
            ("suppressed", &mut suppressed_codes),
        ] {
            let mut stmt = self.conn.prepare(
                "SELECT DISTINCT code FROM diagnostics
                  WHERE run_id = ?1 AND COALESCE(disposition, 'reported') = ?2
                  ORDER BY code ASC",
            )?;
            let got = stmt
                .query_map(params![run_id, disposition], |r| r.get::<_, String>(0))?
                .collect::<std::result::Result<Vec<String>, _>>()?;
            *bucket = got;
        }
        Ok(Some(RulesetRecall {
            run_id,
            ruleset_version,
            ruleset_sha256,
            finished_at,
            codes,
            suppressed_codes,
        }))
    }

    /// 某住院别名经历过的全部规则集哈希（按时间升序），供「改版影响面」核对。
    pub fn ruleset_history(&self, encounter_alias: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT started_at, ruleset_sha256 FROM review_runs
              WHERE encounter_alias = ?1 ORDER BY started_at ASC, run_id ASC",
        )?;
        let rows = stmt
            .query_map(params![encounter_alias], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<std::result::Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    // -------------------------------------------------------------- 写回

    /// 建写回草稿。草稿里的件**尚未生效**，只是待人工勾选。
    pub fn create_draft(
        &self,
        draft_id: &str,
        encounter_alias: &str,
        entries: &[WritebackEntry<'_>],
        actor: &str,
        at: &str,
    ) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "INSERT INTO writeback_drafts
               (draft_id, encounter_alias, state, created_at, created_by, updated_at)
             VALUES (?1, ?2, 'draft', ?3, ?4, ?3)",
            params![draft_id, encounter_alias, at, actor],
        )?;
        for e in entries {
            tx.execute(
                "INSERT INTO writeback_draft_items (draft_id, doc_id, content_sha256, selected)
                 VALUES (?1, ?2, ?3, 0)",
                params![draft_id, e.doc_id, e.content_sha256],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// 勾选 / 取消勾选草稿中的一条。草稿不在 `draft` 态时拒绝。
    pub fn set_selected(&self, draft_id: &str, doc_id: &str, selected: bool, at: &str) -> Result<()> {
        self.assert_draft_state(draft_id, "draft", "勾选")?;
        let n = self.conn.execute(
            "UPDATE writeback_draft_items SET selected = ?3
              WHERE draft_id = ?1 AND doc_id = ?2",
            params![draft_id, doc_id, selected as i64],
        )?;
        if n == 0 {
            return Err(StoreError::EntryUnknown {
                draft_id: draft_id.to_string(),
                doc_id: doc_id.to_string(),
            });
        }
        self.touch_draft(draft_id, at)?;
        Ok(())
    }

    pub fn selected_count(&self, draft_id: &str) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM writeback_draft_items WHERE draft_id = ?1 AND selected = 1",
            params![draft_id],
            |r| r.get(0),
        )?)
    }

    /// 草稿里**已勾选**的条目（件 id + 该件内容哈希），按件 id 升序。
    ///
    /// 「已勾选」是唯一能进入 transaction 的来源：调用方不能自己拼一份
    /// selection 绕开勾选状态，否则「未勾选即发送」这条拦截就形同虚设。
    pub fn selected_items(&self, draft_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT doc_id, content_sha256 FROM writeback_draft_items
              WHERE draft_id = ?1 AND selected = 1
              ORDER BY doc_id ASC",
        )?;
        let rows = stmt
            .query_map(params![draft_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    /// 草稿归属的住院别名（幂等键的第二个分量，调用方不该自己记）。
    pub fn draft_alias(&self, draft_id: &str) -> Result<String> {
        self.conn
            .query_row(
                "SELECT encounter_alias FROM writeback_drafts WHERE draft_id = ?1",
                params![draft_id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| StoreError::DraftUnknown(draft_id.to_string()))
    }

    /// 草稿里全部条目（含未勾选），供「撤回要退回哪几条」用。
    pub fn draft_items(&self, draft_id: &str) -> Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT doc_id, content_sha256 FROM writeback_draft_items
              WHERE draft_id = ?1 ORDER BY doc_id ASC",
        )?;
        let rows = stmt
            .query_map(params![draft_id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<Vec<(String, String)>, _>>()?;
        Ok(rows)
    }

    /// 清空草稿的全部勾选（撤回「已勾选但未发送」时用）。
    ///
    /// 返回被清掉的条数——「撤回了几条」必须是可核对的事实，
    /// 而不是 UI 上的一句提示。
    pub fn clear_selection(&self, draft_id: &str, at: &str) -> Result<usize> {
        self.assert_draft_state(draft_id, "draft", "清空勾选")?;
        let cleared = self.selected_count(draft_id)?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE writeback_draft_items SET selected = 0 WHERE draft_id = ?1",
            params![draft_id],
        )?;
        tx.execute(
            "UPDATE writeback_drafts SET updated_at = ?2 WHERE draft_id = ?1",
            params![draft_id, at],
        )?;
        tx.commit()?;
        Ok(cleared)
    }

    /// 该草稿**实际写出过**的件 id（按写回日志，不是按草稿条目）。
    ///
    /// 撤回走反向 transaction 时只能撤「真的发出去过的」那些：
    /// 勾了但没发的件没有服务端资源，撤它等于凭空造一次 DELETE。
    pub fn written_doc_ids(&self, draft_id: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT doc_id FROM writeback_log WHERE draft_id = ?1 ORDER BY doc_id ASC",
        )?;
        let rows = stmt
            .query_map(params![draft_id], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    /// 提交整批写回。**任一键已存在即视为幂等命中**：不新增 `writeback_log`
    /// 行，也不写 `kind=writeback` 审计事件。
    ///
    /// 整批在一个事务里提交：中途失败 0 条部分写入。
    pub fn commit_writeback(
        &self,
        draft_id: &str,
        entries: &[WritebackEntry<'_>],
        actor: &str,
        at: &str,
        ruleset_sha256: &str,
    ) -> Result<CommitOutcome> {
        self.assert_draft_state(draft_id, "draft", "提交")?;
        let draft_alias: String = self.conn.query_row(
            "SELECT encounter_alias FROM writeback_drafts WHERE draft_id = ?1",
            params![draft_id],
            |r| r.get(0),
        )?;
        // 事务开头就拿写锁（IMMEDIATE 而非 DEFERRED）：这个函数是「查已存在
        // → 再写入」的形状，DEFERRED 下两个并发提交会各自查到「不存在」，
        // 等它们去拿写锁时 sqlite **立刻**返回 `SQLITE_BUSY_SNAPSHOT`
        // （它必须如此，否则两个读者都打算写会死锁），`busy_timeout` 在
        // 这一步不生效——现象是护士长点确认，偶尔报一句莫名其妙的
        // `E_DB_IO: database is locked`。
        let tx = rusqlite::Transaction::new_unchecked(
            &self.conn,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        let mut keys: Vec<String> = Vec::with_capacity(entries.len());
        let mut fresh: Vec<(String, &WritebackEntry<'_>)> = Vec::new();
        for e in entries {
            let key = idempotency_key(&draft_alias, e.doc_id, e.content_sha256);
            let exists: Option<i64> = tx
                .query_row(
                    "SELECT id FROM writeback_log WHERE idempotency_key = ?1",
                    params![key],
                    |r| r.get(0),
                )
                .optional()?;
            keys.push(key.clone());
            if exists.is_none() {
                fresh.push((key, e));
            }
        }
        if fresh.is_empty() {
            tx.rollback()?;
            return Ok(CommitOutcome::AlreadyExists { keys });
        }
        let mut written: Vec<String> = Vec::new();
        for (key, e) in &fresh {
            tx.execute(
                "INSERT INTO writeback_log
                   (idempotency_key, encounter_alias, doc_id, draft_id,
                    content_sha256, run_id, committed_at, committed_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    key,
                    draft_alias,
                    e.doc_id,
                    draft_id,
                    e.content_sha256,
                    e.run_id,
                    at,
                    actor,
                ],
            )?;
            written.push(key.clone());
        }
        let doc_ids: Vec<String> = fresh.iter().map(|(_, e)| e.doc_id.to_string()).collect();
        let codes_json = serde_json::to_string(&doc_ids).unwrap_or_else(|_| "[]".into());
        for (key, e) in &fresh {
            tx.execute(
                "INSERT INTO audit_events
                   (event_at, kind, encounter_alias, doc_id, run_id,
                    ruleset_sha256, codes_json, actor, idempotency_key)
                 VALUES (?1,'writeback',?2,?3,?4,?5,?6,?7,?8)",
                params![
                    at,
                    draft_alias,
                    e.doc_id,
                    e.run_id,
                    ruleset_sha256,
                    codes_json,
                    actor,
                    key,
                ],
            )?;
        }
        tx.execute(
            "UPDATE writeback_drafts SET state = 'sent', updated_at = ?2 WHERE draft_id = ?1",
            params![draft_id, at],
        )?;
        tx.commit()?;
        Ok(CommitOutcome::Committed { keys: written })
    }

    /// 撤回草稿：把 `sent` 草稿退回 `draft` 区，**不产生已生效事实**。
    ///
    /// `writeback_log` 里的行不动——那是「已发出过」的历史，撤回只改
    /// 「这件现在能不能再提交」的判断。撤回记一条 `kind=writeback` 事件，
    /// 便于事后看出「发过又撤过」。
    pub fn withdraw_draft(
        &self,
        draft_id: &str,
        actor: &str,
        at: &str,
        ruleset_sha256: &str,
    ) -> Result<()> {
        self.assert_draft_state(draft_id, "sent", "撤回")?;
        let alias: String = self.conn.query_row(
            "SELECT encounter_alias FROM writeback_drafts WHERE draft_id = ?1",
            params![draft_id],
            |r| r.get(0),
        )?;
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE writeback_drafts SET state = 'draft', updated_at = ?2 WHERE draft_id = ?1",
            params![draft_id, at],
        )?;
        tx.execute(
            "UPDATE writeback_draft_items SET selected = 0 WHERE draft_id = ?1",
            params![draft_id],
        )?;
        tx.execute(
            "INSERT INTO audit_events
               (event_at, kind, encounter_alias, doc_id, run_id,
                ruleset_sha256, codes_json, actor, idempotency_key)
             VALUES (?1,'writeback',?2,NULL,NULL,?3,'[]',?4,NULL)",
            params![at, alias, ruleset_sha256, actor],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn draft_state(&self, draft_id: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row(
                "SELECT state FROM writeback_drafts WHERE draft_id = ?1",
                params![draft_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?)
    }

    pub fn writeback_row_count(&self) -> Result<usize> {
        Ok(self
            .conn
            .query_row("SELECT COUNT(*) FROM writeback_log", [], |r| r.get(0))?)
    }

    // -------------------------------------------------------------- 审计

    pub fn audit_event_count(&self, kind: AuditKind) -> Result<usize> {
        Ok(self.conn.query_row(
            "SELECT COUNT(*) FROM audit_events WHERE kind = ?1",
            params![kind.as_str()],
            |r| r.get(0),
        )?)
    }

    /// 某住院别名的审计链（按时间升序）。
    pub fn audit_trail(
        &self,
        encounter_alias: &str,
    ) -> Result<Vec<(String, String, String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT event_at, kind, ruleset_sha256, actor
               FROM audit_events WHERE encounter_alias = ?1
              ORDER BY event_at ASC, id ASC",
        )?;
        let rows = stmt
            .query_map(params![encounter_alias], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<Vec<(String, String, String, String)>, _>>()?;
        Ok(rows)
    }

    /// 审计事件里出现过的全部规则集哈希（去重升序），供版本回溯界面展示。
    pub fn audited_ruleset_hashes(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT ruleset_sha256 FROM audit_events ORDER BY ruleset_sha256")?;
        let rows = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(rows)
    }

    fn assert_draft_state(
        &self,
        draft_id: &str,
        want: &'static str,
        action: &'static str,
    ) -> Result<()> {
        match self.draft_state(draft_id)? {
            None => Err(StoreError::DraftUnknown(draft_id.to_string())),
            Some(state) if state != want => Err(StoreError::DraftState {
                draft_id: draft_id.to_string(),
                state: leak(&state),
                action,
            }),
            Some(_) => Ok(()),
        }
    }

    fn touch_draft(&self, draft_id: &str, at: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE writeback_drafts SET updated_at = ?2 WHERE draft_id = ?1",
            params![draft_id, at],
        )?;
        Ok(())
    }
}

/// `DraftState` 错误里要塞 `&'static str`，而库里的 state 是 `String`。
/// 已知取值只有三个，映射回静态串。
fn leak(state: &str) -> &'static str {
    match state {
        "sent" => "sent",
        "withdrawn" => "withdrawn",
        _ => "draft",
    }
}

fn state_name(s: DocState) -> &'static str {
    match s {
        DocState::Missing => "missing",
        DocState::Draft => "draft",
        DocState::Created => "created",
        DocState::Received => "received",
        DocState::Archivable => "archivable",
    }
}

fn parse_state(raw: &str) -> DocState {
    match raw {
        "missing" => DocState::Missing,
        "draft" => DocState::Draft,
        "created" => DocState::Created,
        "received" => DocState::Received,
        _ => DocState::Archivable,
    }
}

/// 件内容哈希：与 `psychdoc_rules::content_sha256` 同一口径（本层独立实现，
/// 避免存储层反向依赖规则层）。
pub fn content_of(item: &DocItem) -> String {
    let mut parts: Vec<String> = Vec::new();
    parts.push(format!(
        "{}|{}|{}",
        item.doc_id, item.template_id, item.encounter_alias
    ));
    parts.push(item.source_system.clone());
    for s in &item.sections {
        parts.push(format!("#{}|{}|{}", s.index, s.title, s.text));
    }
    for f in &item.fields {
        parts.push(format!(
            "${}|{}|{}|{}|{}",
            f.section_index,
            f.key,
            f.value.as_deref().unwrap_or("<null>"),
            f.required,
            f.source_span
                .map(|s| format!("{},{}", s.start, s.end))
                .unwrap_or_else(|| "-".into())
        ));
    }
    for t in &item.timestamps {
        // 抽不出时刻的时间戳也要参与哈希：只写「23时40分」而没有日期的件，
        // 与根本没写时刻的件，是两件不同的件，不能因为 moment 都是 None 就撞成一个。
        let moment = t
            .moment
            .map(|m| m.to_rfc3339())
            .unwrap_or_else(|| "<no-date>".into());
        parts.push(format!(
            "@{}|{}|{}|{}",
            kind_name(t.kind),
            moment,
            format!("{:?}", t.precision).to_lowercase(),
            t.raw_text.as_deref().unwrap_or("")
        ));
    }
    content_sha256_of(&parts.join("\n"))
}

fn kind_name(k: psychdoc_model::TimestampKind) -> &'static str {
    use psychdoc_model::TimestampKind::*;
    match k {
        AuthoredOn => "authored_on",
        Date => "date",
        BusinessMoment => "business_moment",
    }
}
