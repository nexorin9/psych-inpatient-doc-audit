//! **版本化迁移链**：本机 sqlite 库随版本演进，`PRAGMA user_version` 记版本号，
//! 每一步迁移是一个事务，迁移前后对关键表列做断言，不符即整条回滚。
//!
//! # 为什么要链，而不是一次性 `CREATE TABLE`
//!
//! 质控办归档库在护士长机器上跑几个月不动，某次升级加了张缓存表。若代码里
//! 只留一句 `CREATE TABLE IF NOT EXISTS`，**老库根本没有这张表**——因为那句
//! 只在建新库时跑过；而开发者本地每次都是空库，永远测不到「老库升级」这条路。
//! 更糟的是 `user_version` 停在 0，看起来像全新库，于是所有 `IF NOT EXISTS`
//! 全部静默跳过，程序拿着一张缺列的库跑起来，直到某次查询才炸出一句
//! `no such column`，护士长看到的是「今天早上就报错了」。
//!
//! # 三条硬规则
//!
//! 1. **每条迁移一个事务**：DDL 与 `user_version` 一起提交或一起回滚。
//!    sqlite 的 DDL 是事务性的，回滚后连半张表都不留。
//! 2. **迁移前后都有断言**：前断言保证「上一步真的做完了」（否则是在一份
//!    被手工改坏的库上继续建表）；后断言保证「这一步真的建成了」，不符即回滚，
//!    不让一个半成品 schema 留在磁盘上等下次启动。
//! 3. **库比程序新就拒绝启动**（`E_SCHEMA_TOO_NEW`）：拿旧代码打开新库，
//!    旧代码不认识新表里的新列，写回去就把数据写坏了。**降级只读也不做**
//!    ——只读同样会让人以为「今天没问题」，然后第二天才发现漏审。
//!
//! # 加一条迁移怎么做
//!
//! 在 [`MIGRATIONS`] 末尾追加 [`Migration`]，`version` 必须紧接上一条（缺号会
//! 在 `apply_migrations` 的链自检里报 `E_MIGRATION_FAILED`，宁可启动失败也不要
//! 静默跳号），并补上 `expect_before` / `expect_after` 两段表列断言。
//!
//! 六张表的分工与 `spec.md` §4 闭环维度一一对应：

//! | 表 | 承载的闭环维度 | 关键约束 |
//! |---|---|---|
//! | `doc_items` | 权威实体绑定 | `doc_id` 主键（= `Composition.id` 脱敏别名） |
//! | `doc_states` | 业务状态生命周期 | 状态只能经状态机推进，非法跳变在写入前被拒 |
//! | `review_runs` | 按当时规则集版本回溯 | `ruleset_sha256` 随批次落盘 |
//! | `diagnostics` | 命中码可回查 | 同批次的诊断全部挂同一 `run_id` |
//! | `writeback_drafts` | 确认回写 | 草稿可撤回，撤回**不产生已生效事实** |
//! | `writeback_log` | 幂等 | `idempotency_key` 唯一约束 |
//! | `writeback_claims` | 并发 / 重放 | 占键在**外发之前**，四态 `in_flight`/`committed`/`released`/`in_doubt` |
//! | `audit_events` | 安全审计 | 每次调阅与写回各一条 Provenance |

use rusqlite::{Connection, Transaction, TransactionBehavior};

use crate::error::{Result, StoreError};

/// v1 迁移的 DDL。全部 `IF NOT EXISTS`，重复执行幂等。
pub const V1_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS doc_items (
    doc_id            TEXT PRIMARY KEY,
    template_id       TEXT NOT NULL,
    encounter_alias   TEXT NOT NULL,
    source_system     TEXT NOT NULL DEFAULT '',
    content_sha256    TEXT NOT NULL,
    binding           TEXT NOT NULL,
    first_seen_at     TEXT NOT NULL,
    last_seen_at      TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_doc_items_alias ON doc_items(encounter_alias);

CREATE TABLE IF NOT EXISTS doc_states (
    doc_id        TEXT PRIMARY KEY REFERENCES doc_items(doc_id),
    state         TEXT NOT NULL,
    last_actor    TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    history_json  TEXT NOT NULL DEFAULT '[]'
);

CREATE TABLE IF NOT EXISTS review_runs (
    run_id            TEXT PRIMARY KEY,
    encounter_alias   TEXT NOT NULL,
    ruleset_version   TEXT NOT NULL,
    ruleset_sha256    TEXT NOT NULL,
    actor             TEXT NOT NULL,
    started_at        TEXT NOT NULL,
    finished_at       TEXT,
    items_total       INTEGER NOT NULL,
    items_reviewed    INTEGER NOT NULL,
    items_skipped     INTEGER NOT NULL,
    unresolved_count  INTEGER NOT NULL,
    waived_count      INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_review_runs_alias
    ON review_runs(encounter_alias, started_at);
CREATE INDEX IF NOT EXISTS idx_review_runs_ruleset
    ON review_runs(ruleset_sha256);

CREATE TABLE IF NOT EXISTS diagnostics (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id          TEXT NOT NULL REFERENCES review_runs(run_id),
    doc_id          TEXT NOT NULL,
    section_index   INTEGER NOT NULL,
    char_start      INTEGER NOT NULL,
    char_end        INTEGER NOT NULL,
    code            TEXT NOT NULL,
    severity        TEXT NOT NULL,
    message         TEXT NOT NULL,
    basis           TEXT NOT NULL,
    fix_availability TEXT NOT NULL,
    waived          TEXT,
    suppressed_by   TEXT
);
CREATE INDEX IF NOT EXISTS idx_diagnostics_run ON diagnostics(run_id);
CREATE INDEX IF NOT EXISTS idx_diagnostics_code ON diagnostics(code);

CREATE TABLE IF NOT EXISTS writeback_drafts (
    draft_id         TEXT PRIMARY KEY,
    encounter_alias  TEXT NOT NULL,
    state            TEXT NOT NULL,          -- draft | sent | withdrawn
    created_at       TEXT NOT NULL,
    created_by       TEXT NOT NULL,
    updated_at       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_drafts_alias ON writeback_drafts(encounter_alias);

CREATE TABLE IF NOT EXISTS writeback_draft_items (
    draft_id         TEXT NOT NULL REFERENCES writeback_drafts(draft_id),
    doc_id           TEXT NOT NULL,
    content_sha256   TEXT NOT NULL,
    selected         INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (draft_id, doc_id)
);

CREATE TABLE IF NOT EXISTS writeback_log (
    id                 INTEGER PRIMARY KEY AUTOINCREMENT,
    idempotency_key    TEXT NOT NULL UNIQUE,
    encounter_alias    TEXT NOT NULL,
    doc_id             TEXT NOT NULL,
    draft_id           TEXT NOT NULL,
    content_sha256     TEXT NOT NULL,
    run_id             TEXT,
    committed_at       TEXT NOT NULL,
    committed_by       TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_writeback_doc ON writeback_log(doc_id);

CREATE TABLE IF NOT EXISTS audit_events (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    event_at        TEXT NOT NULL,
    kind            TEXT NOT NULL,        -- review | writeback
    encounter_alias TEXT NOT NULL,
    doc_id          TEXT,
    run_id          TEXT,
    ruleset_sha256  TEXT NOT NULL,
    codes_json      TEXT NOT NULL,
    actor           TEXT NOT NULL,
    idempotency_key TEXT
);
CREATE INDEX IF NOT EXISTS idx_audit_alias ON audit_events(encounter_alias, event_at);
"#;

/// v2 迁移的 DDL：**增量缓存表**（Task 26）。
///
/// 这张表回答的是「哪些件的判定结果可以复用」，主键 `(doc_id, 内容哈希)`
/// 与内存缓存 [`ReviewCache`](crate::cache) 的键形状一致，因此「进程重启后
/// 仍能命中」与「同一进程内命中」走的是同一套键规则，不会出现两份缓存各说
/// 各话。
///
/// **`ruleset_sha256` 是这张表存在的理由**。若主键只有 `(doc_id, 内容哈希)`，
/// 院内改一次规则集后，内容没变的件会继续命中旧版结论——清单上写着
/// 「已按 0.6.0 审过」，实际那份结论是 0.5.0 判的。质控办回溯时按
/// `review_runs.ruleset_sha256` 一查就会发现对不上。因此每条缓存都记着
/// 产出它时的规则集哈希，规则集一变即整表作废。
///
/// `scope` 区分件级（`item`）与住院组级（`group`）缓存——两者的键第二段
/// 分别是内容哈希与组指纹，混在一张表里不加列的话无法区分语义。
pub const V2_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS review_cache (
    scope           TEXT NOT NULL,          -- item | group
    doc_id          TEXT NOT NULL,          -- 件 id 或住院别名
    content_sha256  TEXT NOT NULL,          -- 件内容哈希 / 组指纹
    ruleset_sha256  TEXT NOT NULL,          -- 产出该结论时的规则集版本
    review_run_id   TEXT NOT NULL,          -- 产出该结论的审查批次
    payload_json    TEXT NOT NULL,          -- 判定结果（JSON）
    written_at      TEXT NOT NULL,
    PRIMARY KEY (scope, doc_id, content_sha256)
);
CREATE INDEX IF NOT EXISTS idx_review_cache_ruleset ON review_cache(ruleset_sha256);
CREATE INDEX IF NOT EXISTS idx_review_cache_doc ON review_cache(doc_id);
"#;

/// v3 迁移的 DDL：**外发预占表**（Task 28）。
///
/// 这张表存在的理由是：**光有 `writeback_log.idempotency_key` 唯一约束
/// 挡不住并发**。那条唯一约束约束的是「已落库的写回」，可 transaction
/// 是在**落库之前**就发出去的——两个进程同时确认同一件时，两边都会发出
/// 一份 Bundle transaction，服务端按 `POST` 建两份 `Composition`，之后
/// 第二个才在本地撞上唯一约束。那时已经晚了：外发已经发生，撤不回来。
///
/// 因此把幂等键的占用提前到**外发之前**：
///
/// ```text
/// reserve（占键，唯一约束） → 外发 transaction → settle（确认结果）
///              ↑ 撞键的人在这里就被挡住，一次外发都不会多
/// ```
///
/// `state` 四态各自对应一条现场处置口径：
///
/// | state | 含义 | 撞上它的人拿到什么 |
/// |---|---|---|
/// | `in_flight` | 已占键，外发进行中（正常只存在几秒） | `E_WRITEBACK_CONFLICT`，带首次操作人与时刻 |
/// | `committed` | 外发成功且结果已落库 | 重放：返回首次结果，`replayed=true`，**不外发** |
/// | `released` | 占键后外发失败，已释放，可重发 | 整批重新预占（不是冲突——上次确实没发出去） |
/// | `in_doubt` | 进程在外发与落库之间崩了，**结果未知** | `E_REPLAY_IN_DOUBT`：**禁止自动重发**，要人工核对 |
///
/// `in_doubt` 是这张表里最重要的一态。「崩溃后重试」如果不带它，就是把
/// 「服务端可能已经建好件」当成「没发过」再发一次——护士长那边会看到两份
/// 同 id 的知情同意。
///
/// `tx_result_json` 存首次外发返回的 transaction 结果（已建件列表 +
/// Provenance 位置）。重放时原样回放，调用方不必再问服务端一次。
pub const V3_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS writeback_claims (
    idempotency_key TEXT PRIMARY KEY,   -- 幂等键；占键即生效
    encounter_alias TEXT NOT NULL,
    doc_id          TEXT NOT NULL,
    content_sha256  TEXT NOT NULL,
    batch_id        TEXT NOT NULL,      -- 同批 transaction 的键共用一个批次号
    state           TEXT NOT NULL,      -- in_flight | committed | released | in_doubt
    claimed_by      TEXT NOT NULL,      -- 首次占键的人
    claimed_at      TEXT NOT NULL,
    settled_at      TEXT,
    settled_by      TEXT,
    tx_result_json  TEXT,               -- 首次外发结果，供重放原样回放
    release_reason  TEXT
);
CREATE INDEX IF NOT EXISTS idx_claims_batch ON writeback_claims(batch_id);
CREATE INDEX IF NOT EXISTS idx_claims_state ON writeback_claims(state);
CREATE INDEX IF NOT EXISTS idx_claims_alias ON writeback_claims(encounter_alias, state);
"#;

/// v4 迁移的 DDL：**增量读游标表**（Task 29）。
///
/// 每次全量拉院内文书包会把「凌晨 2 点的变更」和「三天前就定稿的件」一起
/// 重新拉一遍。FHIR R4 的做法是 `_since=<上一次成功落库的时刻>`——但**光有
/// `_since` 参数不够**：中断之后从哪儿续？如果游标只存在内存里，进程一崩，
/// 下次要么从零全量（拉了几万条），要么从上次启动点续（漏掉崩溃到重启之间的
/// 变更）。护士长那边看到的都是「今天的文书好像少了几件」。
///
/// 所以游标必须落库，且**只在整批成功落库之后才推进**：
///
/// ```text
/// 读游标 → 分页拉取 → 解析落库（事务）→ 推进游标到本批最后一页的 lastUpdated
///            ↑ 任一步失败，游标原地不动，下次从同一点重来
/// ```
///
/// 推进的那一步和落库那一步必须**同一个事务**（[`crate::cursor`] 的
/// `commit_page`）。分两次提交会出现「游标已推进、件没落库」——那等于
/// 永久丢失一批文书，而且没有任何报错可查。
///
/// 键是 `(tenant_id, resource_type)`：多租户部署下同一个 `Composition`
/// 端点在不同病区各有各的变更流，混一条游标会把 A 病区的增量推进到
/// B 病区之后，B 病区的变更就再也拉不到了。
pub const V4_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS sync_cursors (
    tenant_id     TEXT NOT NULL,         -- 多租户部署下按租户各拉各的
    resource_type TEXT NOT NULL,         -- Composition / DocumentReference
    since         TEXT NOT NULL,         -- 下次拉取的 _since 下界（RFC3339）
    last_success_at TEXT,                -- 最近一次整批成功落库的时刻
    pages_done    INTEGER NOT NULL DEFAULT 0,
    run_id        TEXT,                  -- 推进该游标的那次审查批次
    PRIMARY KEY (tenant_id, resource_type)
);
"#;

/// v5 迁移的 DDL：**通知发件箱**（Task 30）。
///
/// 缺件提醒走企业微信 Webhook 出站，网络抖动是常态而不是异常——护士长
/// 在病房楼里，手机信号比机房差得多。这张表回答两个问题：
///
/// 1. **「这条今天发过了吗？」** —— `notify_key` 为主键，
///    `sha256(住院别名 + 件 id + 业务日)`。同一天同一件重复触发审查
///    （常驻进程每次增量变更都重扫）**不会**再发一遍；护士长群里同一份
///    住院被 @ 七八次，第二天就没人看这条消息了。
/// 2. **「上次没发出去的那条还在吗？」** —— 发送失败**先落库再返回**，
///    进程退出、机器重启都不丢。缺一条提醒的直接后果是病案室永远收不到，
///    而界面上「已发」与「没发」长得一模一样。
///
/// | state | 含义 | 下次谁来动 |
/// |---|---|---|
/// | `pending` | 已入队、尚未成功送达（含退避等待中） | 退避到期后由下一次 `drain()` 重发 |
/// | `sent` | 端点确认成功 | 不再动（幂等键命中直接跳过） |
/// | `pending_manual` | 重试次数耗尽，转人工 | 只在「归档核齐」里由人处理 |
///
/// `pending_manual` 是刻意分出来的第三态：把耗尽重试的通知继续留在
/// `pending` 里，界面上的「待发 N 条」就永远降不下来，变成一个没人看的
/// 常驻数字；删掉更糟——那条提醒就此消失。
///
/// `payload_json` 存**渲染好的消息体**而不是「待渲染的输入」：入队时
/// 缺件清单是什么就发什么，重试期间文书已被补齐也不该改口（群里已经
/// 有人说「我这就补」，第二条却说「不用补了」比第一条更让人困惑）。
pub const V5_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS notification_outbox (
    notify_key       TEXT PRIMARY KEY,  -- sha256(住院别名 + 件 id + 业务日)
    encounter_alias  TEXT NOT NULL,
    doc_id           TEXT NOT NULL,
    business_day     TEXT NOT NULL,     -- 业务日 YYYY-MM-DD；跨零点按交班日
    channel          TEXT NOT NULL,     -- wecom_markdown（后续可加别的通道）
    payload_json     TEXT NOT NULL,     -- 入队那一刻定稿的消息体，重试不改口
    state            TEXT NOT NULL,     -- pending | sent | pending_manual
    attempts         INTEGER NOT NULL DEFAULT 0,
    next_attempt_at  TEXT NOT NULL,     -- 下次可发时刻（指数退避）
    last_error       TEXT,
    created_at       TEXT NOT NULL,
    sent_at          TEXT
);
CREATE INDEX IF NOT EXISTS idx_outbox_state ON notification_outbox(state, next_attempt_at);
CREATE INDEX IF NOT EXISTS idx_outbox_alias ON notification_outbox(encounter_alias, business_day);
"#;

/// v6：审计证据的三个补口（Task 31）。
///
/// **为什么补**：`diagnostics` 表原来只装「清单上留下的那几条」。质控办
/// 拿导出包追问「当年这一栏到底报了哪几条」时，会问倒在这三处——
///
/// 1. `disposition`：去重**压下**的命中此前根本没落库。只导留下来的那组，
///    「只报一条」会被读成「当年只查出这一条」。折叠是展示层的决定，
///    但「压下了什么」是**事实**，必须跟着进证据包。
/// 2. `rule_source`：三级级联覆盖后，这条命中究竟出自院级 / 科室级 /
///    个人级哪一层。同一码在不同病区生效依据可能不同，不带这列的导出
///    答不了「为什么这个病区报得这么松」。
/// 3. `archive_views` 表：归档核齐视图（缺件、欠件天数、阻塞边、口径说明）
///    随批次落库。视图已经在 `psychdoc-graph` 里 `Serialize` 过，
///    这里存**原样 JSON** 而不是拆列重拼——重拼就会与界面口径漂移。
///
/// 三列都有 `DEFAULT`，因此老库迁移后既有行不会变成空值以外的怪东西：
/// 迁移前落库的审查，导出时 `disposition` 是 `reported`（当时清单上确实
/// 留着它）、`rule_source` 为空（确实没记过来源层），如实反映历史。
pub const V6_DDL: &str = r#"
ALTER TABLE diagnostics ADD COLUMN disposition TEXT NOT NULL DEFAULT 'reported';
ALTER TABLE diagnostics ADD COLUMN rule_source TEXT NOT NULL DEFAULT '';

/* 首次进入缺失态的时刻。欠件天数从这里算，不是从 updated_at ——
   updated_at 每次推状态都会变，拿它顶替等于把「欠了 5 天」改写成
   「今天才发现」。可空 = 未进过缺失态（不是「今天」）。 */
ALTER TABLE doc_states ADD COLUMN missing_since TEXT;

CREATE TABLE IF NOT EXISTS archive_views (
    view_id         TEXT PRIMARY KEY,     -- sha256(run_id + as_of)
    run_id          TEXT NOT NULL REFERENCES review_runs(run_id),
    encounter_alias TEXT NOT NULL,
    as_of           TEXT NOT NULL,        -- 视图以哪个时刻算的（不读系统时钟）
    is_ready        INTEGER NOT NULL,     -- 能不能归档签字
    view_json       TEXT NOT NULL,        -- ArchiveView 原样序列化，不拆列重拼
    recorded_at     TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_archive_views_run ON archive_views(run_id);
CREATE INDEX IF NOT EXISTS idx_archive_views_alias ON archive_views(encounter_alias, as_of);
"#;

/// v7：审查队列表（Task 32：拖放与批量队列）。
///
/// **为什么补**：常驻工具关掉再开，护士长不该把整个病区文件夹重新拖一遍。
/// 更要紧的是——**关窗时队列的进度会丢**：早上审到第 8 次住院，
/// 中午去查房，回来发现窗口记的是「尚未载入」，等于从头再来。
///
/// 两张表：
///
/// - `review_queues`：**一次拖入**。`source_path` 存拖进来的那个根路径，
///   重开后据此一键重扫；`ruleset_short_sha256` 存当次用的规则集短哈希——
///   规则集改版后重扫，队列里的诊断会按新版判，而**队列本身**（哪几次住院、
///   几件、缺几件）不受规则集影响，因此两件事必须分开存。
/// - `queue_entries`：这一次拖入里的**每一次住院**。`position` 存左栏顺序
///   （缺件数降序），重开后要还原成同一个顺序——护士长已经按这个顺序
///   走了一遍病区，打乱顺序等于让他重新找。
///
/// **刻意不存的东西**：诊断明细。诊断已经在 `diagnostics` 表里按
/// `review_runs` 归档；队列表只回答「有哪些次住院、排第几、缺几件」。
/// 两处重复存同一批诊断，重扫一次就会对不上。
pub const V7_DDL: &str = r#"
CREATE TABLE IF NOT EXISTS review_queues (
    queue_id             TEXT PRIMARY KEY,  -- sha256(source_path + loaded_at)
    source_path          TEXT NOT NULL,     -- 拖进来的根（目录或单份文件）
    loaded_at            TEXT NOT NULL,
    ruleset_short_sha256 TEXT NOT NULL,     -- 当次规则集版本；重扫会更新
    packages_found       INTEGER NOT NULL,
    files_seen           INTEGER NOT NULL,
    items_total          INTEGER NOT NULL,
    missing_total        INTEGER NOT NULL,
    file_skips_json      TEXT NOT NULL,     -- 「已跳过」清单原样 JSON
    last_scanned_at      TEXT NOT NULL      -- 最近一次重扫时刻
);
CREATE INDEX IF NOT EXISTS idx_review_queues_loaded ON review_queues(loaded_at);

CREATE TABLE IF NOT EXISTS queue_entries (
    queue_id        TEXT NOT NULL REFERENCES review_queues(queue_id),
    encounter_alias TEXT NOT NULL,
    ward            TEXT NOT NULL DEFAULT '',
    doc_count       INTEGER NOT NULL,
    missing_count   INTEGER NOT NULL,
    reviewed_at     TEXT,                  -- NULL = 本次尚未审；非空 = 已审时刻
    position        INTEGER NOT NULL,      -- 左栏顺序（缺件数降序）
    PRIMARY KEY (queue_id, encounter_alias)
);
CREATE INDEX IF NOT EXISTS idx_queue_entries_queue ON queue_entries(queue_id, position);
"#;

/// v1：件、件级状态、审查批次、诊断、草稿、写回日志与审计链。
pub const SCHEMA_VERSION_V1: i64 = 1;
/// v2：加入增量缓存表（Task 26）。
pub const SCHEMA_VERSION_V2: i64 = 2;
/// v3：加入外发预占表（Task 28：并发确认与重放防护）。
pub const SCHEMA_VERSION_V3: i64 = 3;
/// v4：加入增量读游标表（Task 29：`_since` 增量拉取与断点续拉）。
pub const SCHEMA_VERSION_V4: i64 = 4;
/// v5：加入通知发件箱表（Task 30：缺件提醒幂等与失败重试）。
pub const SCHEMA_VERSION_V5: i64 = 5;
/// v6：诊断补 `disposition` / `rule_source` 两列，加入归档核齐视图表（Task 31）。
pub const SCHEMA_VERSION_V6: i64 = 6;
/// v7：加入审查队列表（Task 32：拖放与批量队列的持久化）。
pub const SCHEMA_VERSION_V7: i64 = 7;
/// 本 crate 当前能建到的最高版本。**程序能打开的库不得高于此值**。
pub const SCHEMA_VERSION_LATEST: i64 = SCHEMA_VERSION_V7;

/// 一张表及其关键列的期望形状。迁移前后拿它做断言。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSpec {
    pub table: &'static str,
    /// 关键列（不是全部列）：审计导出与主路径查询真正依赖的那几列。
    pub columns: &'static [&'static str],
}

impl TableSpec {
    pub const fn new(table: &'static str, columns: &'static [&'static str]) -> Self {
        Self { table, columns }
    }
}

/// 一条有序迁移。`version` 从 1 起连续递增，与 `PRAGMA user_version` 对齐。
#[derive(Debug, Clone, Copy)]
pub struct Migration {
    pub version: i64,
    /// 中文迁移名，进报错文案（护士长截图给信息科时能看出卡在哪一步）。
    pub name: &'static str,
    pub ddl: &'static str,
    /// 迁移**前**必须已成立的表列。空库起步的 v1 不需要前置断言。
    pub expect_before: &'static [TableSpec],
    /// 迁移**后**必须成立的表列。不符即回滚本条迁移。
    pub expect_after: &'static [TableSpec],
}

/// 一条已成功应用的迁移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AppliedMigration {
    pub version: i64,
    pub name: &'static str,
}

/// 一次迁移的结果。`applied` 为空表示库已是当前版本（幂等命中）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    pub from_version: i64,
    pub to_version: i64,
    pub applied: Vec<AppliedMigration>,
}

impl MigrationReport {
    /// 一条都没跑：库本来就在当前版本。
    pub fn is_noop(&self) -> bool {
        self.applied.is_empty()
    }

    /// 本次实际应用的迁移中文名（启动日志与进度条用）。
    pub fn applied_names_zh(&self) -> Vec<&'static str> {
        self.applied.iter().map(|m| m.name).collect()
    }
}

const V1_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "doc_items",
        &[
            "doc_id",
            "template_id",
            "encounter_alias",
            "source_system",
            "content_sha256",
            "binding",
            "first_seen_at",
            "last_seen_at",
        ],
    ),
    TableSpec::new(
        "doc_states",
        &[
            "doc_id",
            "state",
            "last_actor",
            "updated_at",
            "history_json",
        ],
    ),
    TableSpec::new(
        "review_runs",
        &[
            "run_id",
            "encounter_alias",
            "ruleset_version",
            "ruleset_sha256",
            "actor",
            "started_at",
            "finished_at",
            "items_total",
            "items_reviewed",
            "items_skipped",
            "unresolved_count",
            "waived_count",
        ],
    ),
    TableSpec::new(
        "diagnostics",
        &[
            "id",
            "run_id",
            "doc_id",
            "section_index",
            "char_start",
            "char_end",
            "code",
            "severity",
            "message",
            "basis",
            "fix_availability",
            "waived",
            "suppressed_by",
        ],
    ),
    TableSpec::new(
        "writeback_drafts",
        &[
            "draft_id",
            "encounter_alias",
            "state",
            "created_at",
            "created_by",
            "updated_at",
        ],
    ),
    TableSpec::new(
        "writeback_draft_items",
        &["draft_id", "doc_id", "content_sha256", "selected"],
    ),
    TableSpec::new(
        "writeback_log",
        &[
            "id",
            "idempotency_key",
            "encounter_alias",
            "doc_id",
            "draft_id",
            "content_sha256",
            "run_id",
            "committed_at",
            "committed_by",
        ],
    ),
    TableSpec::new(
        "audit_events",
        &[
            "id",
            "event_at",
            "kind",
            "encounter_alias",
            "doc_id",
            "run_id",
            "ruleset_sha256",
            "codes_json",
            "actor",
            "idempotency_key",
        ],
    ),
];

const V2_EXPECT_BEFORE: &[TableSpec] = &[
    TableSpec::new("doc_items", &["doc_id", "encounter_alias"]),
    TableSpec::new("review_runs", &["run_id", "ruleset_sha256"]),
];

const V2_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "review_cache",
        &[
            "scope",
            "doc_id",
            "content_sha256",
            "ruleset_sha256",
            "review_run_id",
            "payload_json",
            "written_at",
        ],
    ),
];

const V3_EXPECT_BEFORE: &[TableSpec] = &[
    TableSpec::new("writeback_log", &["idempotency_key", "doc_id"]),
];

const V3_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "writeback_claims",
        &[
            "idempotency_key",
            "encounter_alias",
            "doc_id",
            "content_sha256",
            "batch_id",
            "state",
            "claimed_by",
            "claimed_at",
            "settled_at",
            "settled_by",
            "tx_result_json",
            "release_reason",
        ],
    ),
];

const V4_EXPECT_BEFORE: &[TableSpec] = &[
    TableSpec::new("doc_items", &["doc_id", "encounter_alias"]),
    TableSpec::new("review_runs", &["run_id", "ruleset_sha256"]),
];

const V4_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "sync_cursors",
        &[
            "tenant_id",
            "resource_type",
            "since",
            "last_success_at",
            "pages_done",
            "run_id",
        ],
    ),
];

const V5_EXPECT_BEFORE: &[TableSpec] = &[TableSpec::new("doc_items", &["doc_id", "encounter_alias"])];

const V5_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "notification_outbox",
        &[
            "notify_key",
            "encounter_alias",
            "doc_id",
            "business_day",
            "channel",
            "payload_json",
            "state",
            "attempts",
            "next_attempt_at",
            "last_error",
            "created_at",
            "sent_at",
        ],
    ),
];

const V6_EXPECT_BEFORE: &[TableSpec] = &[TableSpec::new(
    "diagnostics",
    &["id", "run_id", "doc_id", "code", "severity", "waived", "suppressed_by"],
)];

const V7_EXPECT_BEFORE: &[TableSpec] = &[TableSpec::new(
    "review_runs",
    &["run_id", "encounter_alias", "ruleset_sha256", "started_at"],
)];

const V7_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "review_queues",
        &[
            "queue_id",
            "source_path",
            "loaded_at",
            "ruleset_short_sha256",
            "packages_found",
            "files_seen",
            "items_total",
            "missing_total",
            "file_skips_json",
            "last_scanned_at",
        ],
    ),
    TableSpec::new(
        "queue_entries",
        &[
            "queue_id",
            "encounter_alias",
            "ward",
            "doc_count",
            "missing_count",
            "reviewed_at",
            "position",
        ],
    ),
];

const V6_EXPECT_AFTER: &[TableSpec] = &[
    TableSpec::new(
        "diagnostics",
        &[
            "id",
            "run_id",
            "doc_id",
            "code",
            "severity",
            "waived",
            "suppressed_by",
            "disposition",
            "rule_source",
        ],
    ),
    TableSpec::new(
        "doc_states",
        &["doc_id", "state", "updated_at", "history_json", "missing_since"],
    ),
    TableSpec::new(
        "archive_views",
        &[
            "view_id",
            "run_id",
            "encounter_alias",
            "as_of",
            "is_ready",
            "view_json",
            "recorded_at",
        ],
    ),
];

/// 有序迁移链。**追加迁移只能加在末尾**。
pub const MIGRATIONS: &[Migration] = &[
    Migration {
        version: SCHEMA_VERSION_V1,
        name: "v1 初始 schema：件、状态、审查批次、诊断、草稿、写回与审计链",
        ddl: V1_DDL,
        expect_before: &[],
        expect_after: V1_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V2,
        name: "v2 增量缓存表：件级与住院组级判定复用",
        ddl: V2_DDL,
        expect_before: V2_EXPECT_BEFORE,
        expect_after: V2_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V3,
        name: "v3 外发预占表：并发确认占键、重放拒绝与崩溃存疑",
        ddl: V3_DDL,
        expect_before: V3_EXPECT_BEFORE,
        expect_after: V3_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V4,
        name: "v4 增量读游标表：_since 下界按租户与资源类型各存一条",
        ddl: V4_DDL,
        expect_before: V4_EXPECT_BEFORE,
        expect_after: V4_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V5,
        name: "v5 通知发件箱：缺件提醒幂等键与失败重试落库",
        ddl: V5_DDL,
        expect_before: V5_EXPECT_BEFORE,
        expect_after: V5_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V6,
        name: "v6 审计证据补口：诊断 disposition / rule_source 与归档核齐视图表",
        ddl: V6_DDL,
        expect_before: V6_EXPECT_BEFORE,
        expect_after: V6_EXPECT_AFTER,
    },
    Migration {
        version: SCHEMA_VERSION_V7,
        name: "v7 审查队列：一次拖入与每次住院的条目、顺序与已审时刻",
        ddl: V7_DDL,
        expect_before: V7_EXPECT_BEFORE,
        expect_after: V7_EXPECT_AFTER,
    },
];

/// 读当前库版本（`PRAGMA user_version`）。
pub fn schema_version(conn: &Connection) -> Result<i64> {
    Ok(conn.pragma_query_value(None, "user_version", |row| row.get(0))?)
}

/// 启动自检（**只读，不写库**）：库版本高于程序版本即拒绝启动。
///
/// 不降级为只读模式：只读也会让界面显示「今天没有缺件」，实际是根本没读
/// 到新表里的那批件——这种「看起来正常」比直接报错危险得多。
pub fn startup_check(conn: &Connection) -> Result<()> {
    check_not_too_new(schema_version(conn)?, SCHEMA_VERSION_LATEST)
}

fn check_not_too_new(db_version: i64, program_version: i64) -> Result<()> {
    if db_version > program_version {
        return Err(StoreError::SchemaTooNew {
            db_version,
            program_version,
        });
    }
    Ok(())
}

/// 按序应用迁移链，返回实际应用了哪几步。已是当前版本时一条不跑。
pub fn apply_migrations(conn: &Connection, migrations: &[Migration]) -> Result<MigrationReport> {
    let from_version = schema_version(conn)?;
    let program_version = migrations.iter().map(|m| m.version).max().unwrap_or(0);
    check_not_too_new(from_version, program_version)?;
    assert_chain_is_contiguous(migrations)?;

    let mut ordered: Vec<&Migration> = migrations.iter().collect();
    ordered.sort_by_key(|m| m.version);

    let mut report = MigrationReport {
        from_version,
        to_version: from_version,
        applied: Vec::new(),
    };
    for m in ordered {
        if m.version <= from_version {
            continue;
        }
        if apply_one(conn, m)? {
            report.applied.push(AppliedMigration {
                version: m.version,
                name: m.name,
            });
            report.to_version = m.version;
        } else {
            // 别的进程刚把这条迁完了。库已是目标状态，如实记为「已到这里」，
            // 不能报成「本进程执行了它」——迁移报告是要给人看的事实。
            report.to_version = schema_version(conn)?;
        }
    }
    Ok(report)
}

/// 应用单条迁移：前置断言 → 事务（DDL + `user_version`）→ 后置断言。
/// 任一环节失败即丢弃事务（回滚），返回 `E_MIGRATION_FAILED`。
///
/// 返回值是「本进程是否真的执行了它」。拿到写锁后若发现别的进程已把这条
/// 迁完，返回 `false` 且**不做任何 DDL**——这正是 `ALTER TABLE ADD COLUMN`
/// 这类非幂等 DDL 在并发启动下不被撞坏的原因。
fn apply_one(conn: &Connection, m: &Migration) -> Result<bool> {
    assert_specs(conn, m.expect_before, m, "前置断言不通过")?;

    // `Transaction::new_unchecked(.., Immediate)` 而非 `unchecked_transaction()`
    //（后者是 DEFERRED）：
    //
    // 1. 迁移要跑的正是会失败的 DDL，`transaction()` 那种 Result 型事务要求
    //    先知道会不会失败——这里就是要让它失败，所以要用 unchecked 那一族；
    // 2. 但**必须是 IMMEDIATE**：DEFERRED 下事务开头不拿写锁，等到
    //    `PRAGMA user_version` 要写库头页时才升级锁。两个进程同时启动时，
    //    sqlite 对「把读事务升级成写事务」会**立刻**返回 `SQLITE_BUSY`
    //    （它必须这么做，否则两个读者都打算写时会死锁），`busy_timeout`
    //    在这一条上**不生效**。IMMEDIATE 开头就拿写锁，撞上的那个进程走
    //    busy_timeout 等，而不是当场失败。
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .map_err(|e| migration_failed(m, "开事务失败", e))?;

    // **拿到写锁之后再确认一次版本。**
    //
    // 版本是在开事务**之前**读的，两个进程同时启动时会都读到 5、都决定
    // 跑 v6。此前 v1~v5 全是 `CREATE TABLE IF NOT EXISTS`，重复执行无害，
    // 撞上了也就撞上了；v6 起用了 `ALTER TABLE ADD COLUMN`，它**不幂等**——
    // 第二个进程会拿到 `duplicate column name: disposition` 而整批启动失败。
    //
    // 因此：进事务 → 拿写锁 → 重读 `user_version`。谁先提交，另一个
    // 在这里就看见版本已经追上，直接跳过本条。
    let locked_version = tx
        .pragma_query_value(None, "user_version", |row| row.get::<_, i64>(0))
        .map_err(|e| migration_failed(m, "读版本号失败", e))?;
    if locked_version >= m.version {
        return Ok(false);
    }

    tx.execute_batch(m.ddl)
        .map_err(|e| migration_failed(m, "执行失败", e))?;

    // `user_version` 写在同一事务里：sqlite 把它存进数据库头页，随事务
    // 一起回滚，因此「DDL 成功但版本号没跟上」这种半截状态不存在。
    tx.pragma_update(None, "user_version", m.version)
        .map_err(|e| migration_failed(m, "写版本号失败", e))?;

    assert_specs(conn, m.expect_after, m, "后置断言不通过")?;

    // 走到这里才提交；上面任何一步返回错误都会 drop 事务 → ROLLBACK。
    tx.commit()?;
    Ok(true)
}

fn assert_specs(
    conn: &Connection,
    specs: &[TableSpec],
    m: &Migration,
    stage: &'static str,
) -> Result<()> {
    for spec in specs {
        assert_table(conn, spec).map_err(|detail| StoreError::MigrationFailed {
            version: m.version,
            name: m.name,
            stage,
            detail,
        })?;
    }
    Ok(())
}

/// 迁移各阶段的统一报错构造（三处调用点共用，避免 `stage` 写岔）。
fn migration_failed(m: &Migration, stage: &'static str, e: rusqlite::Error) -> StoreError {
    StoreError::MigrationFailed {
        version: m.version,
        name: m.name,
        stage,
        detail: e.to_string(),
    }
}

/// 断言一张表存在且关键列齐备。返回人可读的差异描述（不是布尔）。
fn assert_table(conn: &Connection, spec: &TableSpec) -> std::result::Result<(), String> {
    let exists: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            [spec.table],
            |r| r.get(0),
        )
        .map_err(|e| format!("查 sqlite_master 失败：{e}"))?;
    if exists == 0 {
        return Err(format!("缺表 {}", spec.table));
    }
    let mut stmt = conn
        .prepare(&format!("PRAGMA table_info({})", spec.table))
        .map_err(|e| format!("读 {} 的表结构失败：{e}", spec.table))?;
    let actual = stmt
        .query_map([], |r| r.get::<_, String>(1))
        .map_err(|e| format!("读 {} 的列清单失败：{e}", spec.table))?;
    let mut present: Vec<String> = Vec::new();
    for name in actual {
        present.push(name.map_err(|e| format!("读 {} 的列名失败：{e}", spec.table))?);
    }
    let missing: Vec<&str> = spec
        .columns
        .iter()
        .copied()
        .filter(|c| !present.iter().any(|p| p == c))
        .collect();
    if missing.is_empty() {
        Ok(())
    } else {
        Err(format!("表 {} 缺关键列 {}", spec.table, missing.join("、")))
    }
}

/// 迁移链自检：版本号必须从 1 起连续无缺口。
///
/// 加了 v4 忘了补 v3 时，链里的 `user_version` 会对不上——库停在 3 就永远
/// 升不到 4，而升到 2 的迁移又是安全的。缺号必须在启动时就炸出来。
fn assert_chain_is_contiguous(migrations: &[Migration]) -> Result<()> {
    let mut ordered: Vec<&Migration> = migrations.iter().collect();
    ordered.sort_by_key(|m| m.version);
    for (i, m) in ordered.iter().enumerate() {
        let expected = i as i64 + 1;
        if m.version != expected {
            return Err(StoreError::MigrationFailed {
                version: m.version,
                name: m.name,
                stage: "迁移链定义不连续",
                detail: format!("第 {} 条迁移应为 v{expected}，实际写成 v{}", i + 1, m.version),
            });
        }
    }
    Ok(())
}

/// 升到当前最高版本（走完整迁移链）。
pub fn migrate(conn: &Connection) -> Result<MigrationReport> {
    apply_migrations(conn, MIGRATIONS)
}

/// 升到不超过 `target` 的版本；已是该版本或更高时直接返回（幂等）。
///
/// 保留 `migrate_to_v1` / `migrate_to_v2` 这两个入口，是因为「只补某一步」
/// 是排障动作（老库出问题时要单独试一步），但**实现一律走迁移链**，
/// 不再各写一份 `execute_batch`——那正是版本号与实际表结构会走散的起点。
pub fn migrate_to(conn: &Connection, target: i64) -> Result<MigrationReport> {
    let current = schema_version(conn)?;
    if current >= target {
        // 已经在这条（或更后面）——「补一步」是空操作，不是错误。
        return Ok(MigrationReport {
            from_version: current,
            to_version: current,
            applied: Vec::new(),
        });
    }
    let mut chain: Vec<Migration> = MIGRATIONS
        .iter()
        .copied()
        .filter(|m| m.version <= target)
        .collect();
    chain.sort_by_key(|m| m.version);
    apply_migrations(conn, &chain)
}

/// 升到 v1。
pub fn migrate_to_v1(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V1)
}

/// 升到 v2。
pub fn migrate_to_v2(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V2)
}

/// 升到 v3。
pub fn migrate_to_v3(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V3)
}

/// 升到 v4（增量读游标表）。
pub fn migrate_to_v4(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V4)
}

/// 升到 v5（通知发件箱表）。
pub fn migrate_to_v5(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V5)
}

/// 升到 v6（审计证据补口）。
pub fn migrate_to_v6(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V6)
}

/// 升到 v7（审查队列）。
pub fn migrate_to_v7(conn: &Connection) -> Result<MigrationReport> {
    migrate_to(conn, SCHEMA_VERSION_V7)
}
