//! **审计导出包**：把一次审查 / 一次写回摊成**第三方可核对**的证据行。
//!
//! 对应 `spec.md` §4「安全审计」与岗位「质控办」：月度专项抽查要能回答
//! 「这份病历当年是**按哪一版规则**审的、命中了哪些码、谁在什么时候推进的」。
//! 界面上答得出来不算数——质控办要把这份证据**带走**，带走的只能是文件。
//!
//! # 一行是什么
//!
//! 一行 = 一条诊断（或一条写回），并且**带上当次的全部语境**：审查批次号、
//! 住院别名（脱敏）、件 id、段落定位、规则码、严重度、规范依据、豁免原因、
//! 规则集 `sha256`、推进人、时刻、写回幂等键。缺任何一列，质控办回头都得
//! 再问一次系统，问一次就少信一分。
//!
//! **同一次审查的多条诊断必须打同一个 `review_run_id`**——按批次聚合才
//! 能回答「这一轮一共报了几条」，按条看则会把跨件的时序问题算成好幾件事。
//!
//! # 二次脱敏不是重复劳动，是最后一道闸
//!
//! 落库时 [`crate::audit::desensitize`] 已经扫过一遍；导出时再扫一遍，
//! 因为导出是**唯一会被带出这台机器**的产物，而上游多接一个字段
//! （比如把 `message` 换成含姓名的自由文本）就可能把患者信息带走。
//!
//! 两档处置，刻意不同：
//!
//! - **标识列**（住院别名 / 件 id）命中 → **拒绝导出**（`E_EXPORT_LEAK`）。
//!   这两列本来就不该含个人信息，出现了说明上游绑定错了；写个占位符
//!   糊过去等于把「病历对不上号」这个事实一起糊掉。
//! - **自由文本列**（问题描述 / 依据 / 豁免原因）命中 → 就地替换为占位符
//!   并计入 [`RedactionReport`]。整行作废会让一次抽查颗粒无收，得不偿失。
//!
//! # csv 为什么带 BOM
//!
//! 院内 Excel 双击打开 csv 时默认按本地代码页解码，UTF-8 的中文会变成
//! 一串「锟斤拷」。写 BOM 是让质控员**不用做任何设置**就能看到中文，
//! 这是交付的一部分，不是细节。
//!
//! # 「压下的」也要在包里
//!
//! 去重层把更概括的命中压下、只留最具体的那条，是**展示层**的决定。
//! 但导出包是**证据**：质控办拿着它跟当年的界面截图对，「只报一条」
//! 得有个解释。因此每一行都带 `disposition` 列（`reported` / `suppressed`），
//! 压下的那组照样带码、带定位、带依据——只是标明了「这条被谁盖掉了」。
//!
//! # 规则来源层：为什么这个病区报得这么松
//!
//! 三级级联（院 → 科室 → 个人）之后，同一个码在不同病区的生效依据可能
//! 不同。`rule_source` 列落的是 `QC-PSY-02（科室级，覆盖 院级）` 这样的
//! 溯源串：质控办问「为什么 3 床那个病区只报提示」，答案在这一列里，
//! 不必把整份规则集翻出来对。

use rusqlite::params;
use serde::Serialize;

use crate::audit::desensitize;
use crate::error::{Result, StoreError};
use crate::{ArchiveSnapshot, RulesetRecall, Store};

/// 导出行的列顺序。csv 表头、json 字段与本常量三者必须一致——
/// 质控办会把导出当固定格式收，谁改了列序谁就要在群里解释一遍。
pub const COLUMNS: &[&str] = &[
    "审查批次",
    "住院别名",
    "文书件id",
    "段落序号",
    "字符区间",
    "规则码",
    "处置",
    "严重度",
    "规范依据",
    "问题描述",
    "豁免原因",
    "折叠去向",
    "规则集版本",
    "规则集sha256",
    "规则来源层",
    "操作人",
    "时刻",
    "写回幂等键",
];

/// 一次导出覆盖到的时间范围与对象。**空 `encounter_alias` = 全量**：
/// 质控办导出整批是常态，按住院别名筛是抽查时的用法。
#[derive(Debug, Clone, Default)]
pub struct ExportQuery {
    /// 只导这一个住院别名（脱敏形态，如 `WARD-A-001`）。
    pub encounter_alias: Option<String>,
    /// 只导这一个审查批次。
    pub run_id: Option<String>,
    /// 起始时刻（含），RFC3339。
    pub since: Option<String>,
    /// 截止时刻（含），RFC3339。
    pub until: Option<String>,
    /// 版本回溯点：给定时返回**该时刻生效**的规则集与命中码集合。
    pub recall_at: Option<String>,
}

impl ExportQuery {
    pub fn for_alias(alias: &str) -> Self {
        Self {
            encounter_alias: Some(alias.to_string()),
            ..Default::default()
        }
    }

    pub fn with_run(mut self, run_id: &str) -> Self {
        self.run_id = Some(run_id.to_string());
        self
    }

    pub fn with_recall_at(mut self, at: &str) -> Self {
        self.recall_at = Some(at.to_string());
        self
    }
}

/// 一条可核对的证据行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AuditRow {
    /// 审查批次号。同一次审查的所有行共用一个。
    pub review_run_id: String,
    pub encounter_alias: String,
    pub doc_id: String,
    pub section_index: usize,
    /// `[start, end)`，与界面上看到的字符区间同一口径。
    pub char_range: String,
    pub code: String,
    /// `reported`（清单上留下的）/ `suppressed`（被更具体规则压下）。
    ///
    /// 压下的那组**照样导出**：质控办拿导出跟当年的界面截图对，
    /// 「只报一条」得有个解释，而这行就是那个解释。
    pub disposition: String,
    /// `blocking` / `notice`。
    pub severity: String,
    pub basis: String,
    pub message: String,
    /// 非空即表示这条是被病区豁免放行的；空串 = 正常命中。
    pub waived_reason: String,
    /// 非空即表示这条被更具体的规则压下了（`disposition=suppressed`），
    /// 值里写明**被谁盖掉、为什么**。
    ///
    /// 这一列与 `处置` 是一对：只给 `处置=已折叠` 而不给这一列，
    /// 质控办看到「少了一条」仍然答不出「去哪了」。
    pub suppressed_by: String,
    pub ruleset_version: String,
    pub ruleset_sha256: String,
    /// 生效层级溯源，如 `QC-PSY-02（科室级，覆盖 院级）`。
    /// 老库迁移前的行没有这列内容，空串即「当时没记过来源层」。
    pub rule_source: String,
    pub actor: String,
    /// 事件时刻（RFC3339）。
    pub event_at: String,
    /// 该件最近一次写回的幂等键；从未写回为空。
    pub idempotency_key: String,
}

impl AuditRow {
    /// 这条是不是清单上留下来的。
    ///
    /// 只认 `reported` 两个字面量：老库迁移前落下的行走 v6 的
    /// `DEFAULT 'reported'`，与新写的行同口径。不认识的值算「留下的」——
    /// 把不认识的东西当成压下的，会让导出**少报**条目，那比多报严重。
    pub fn is_reported(&self) -> bool {
        self.disposition != "suppressed"
    }

    pub fn is_suppressed(&self) -> bool {
        self.disposition == "suppressed"
    }

    /// 中文处置，直接印在 csv 与 CLI 里。
    pub fn disposition_zh(&self) -> &'static str {
        if self.is_suppressed() {
            "已折叠"
        } else {
            "已报出"
        }
    }
}

/// 按 `review_run_id` 归并出的批次小结。
///
/// 与 [`AuditRow`] 分开是因为「这一轮审了多少件、命中几条、按哪版规则」
/// 是**批次级**问题，逐行看会被同一住院的多轮审查搅在一起。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RunRollup {
    pub review_run_id: String,
    pub encounter_alias: String,
    pub ruleset_version: String,
    pub ruleset_sha256: String,
    pub actor: String,
    pub started_at: String,
    pub finished_at: String,
    pub items_total: usize,
    pub items_reviewed: usize,
    pub items_skipped: usize,
    pub unresolved_count: usize,
    pub waived_count: usize,
    /// 该批次命中码集合（去重升序，**只含留下的**）。
    pub codes: Vec<String>,
    /// 该批次被去重压下的码集合（去重升序）。
    ///
    /// 与 `codes` 分开而不是合成一个列表：合并后质控办分不清
    /// 「报出来的」和「压下去的」，而这个区分正是导出的意义。
    pub suppressed_codes: Vec<String>,
    /// 该批次在本次导出里的行数。
    pub row_count: usize,
    /// 该批次导出的「留下的」条数（`row_count` 减去压下的）。
    pub reported_count: usize,
    /// 该批次导出的「压下的」条数。
    pub suppressed_count: usize,
}

/// 导出期脱敏结果。**必须跟着包一起交出去**：质控办要能回答
/// 「这份导出里有没有过未脱敏字段」，答「没有」要有凭据。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct RedactionReport {
    /// 自由文本列中被替换的字段数。
    pub replaced_fields: usize,
    /// `列名@行标识` 清单，供追查是哪条文案带出来的。
    pub replaced_at: Vec<String>,
    /// 标识列命中导致**拒绝导出**时才有值。
    pub refused: Option<String>,
}

/// 一个完整的导出包。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct AuditBundle {
    pub columns: Vec<String>,
    pub rows: Vec<AuditRow>,
    pub runs: Vec<RunRollup>,
    pub redaction: RedactionReport,
    /// 版本回溯结论：给定时返回当时生效的规则集与命中码集合。
    pub recalled: Option<RulesetRecall>,
    /// 随批次落库的归档核齐视图快照（Task 21 出视图，Task 31 随包带走）。
    ///
    /// 存的是 `ArchiveView` 的原样 JSON：口径说明（欠件天数怎么算、
    /// 软缺件为什么不算阻断）本来就是写给人看的，拆列重拼必然与界面漂移。
    pub archive_views: Vec<ArchiveViewRow>,
}

/// 归档核齐视图的一行摘要。csv 用；json 里另附 `view_json` 全量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ArchiveViewRow {
    pub view_id: String,
    pub run_id: String,
    pub encounter_alias: String,
    pub as_of: String,
    /// `可归档` / `未核齐`。
    pub readiness: String,
    pub missing_count: usize,
    /// 本次住院的欠件天数（最老缺件口径）；`None` = 没有缺件登记过发现时刻。
    pub missing_days: Option<i64>,
    pub manual_queue: Vec<String>,
    /// 口径说明，直接写给人看，导进 csv 不会被当成噪音删掉。
    pub basis_zh: Vec<String>,
    pub view_json: String,
}

impl AuditBundle {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// 导出的「留下的」条数。压下的条数不在其中。
    pub fn reported_count(&self) -> usize {
        self.rows.iter().filter(|r| r.is_reported()).count()
    }

    /// 导出的「压下的」条数。
    pub fn suppressed_count(&self) -> usize {
        self.rows.iter().filter(|r| r.is_suppressed()).count()
    }

    /// 折叠不丢条目的不变量：留下的 + 压下的 == 导出的全部行。
    ///
    /// 这条恒等式破了 = 去重层吞掉了条目，而吞掉比刷屏严重得多。
    pub fn is_disposition_accounting_balanced(&self) -> bool {
        self.reported_count() + self.suppressed_count() == self.rows.len()
    }

    pub fn run_count(&self) -> usize {
        self.runs.len()
    }

    /// 导出的批次号集合（去重升序），「同一次审查打同一个 run id」的可核对凭据。
    pub fn run_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.runs.iter().map(|r| r.review_run_id.clone()).collect();
        ids.sort();
        ids.dedup();
        ids
    }
}

/// utf-8 BOM。Excel 双击打开 csv 时靠它认编码。
pub const UTF8_BOM: &str = "\u{FEFF}";

impl Store {
    /// 按查询条件摊出审计行。**先脱敏再返回**，返回的每一行都可以直接带走。
    pub fn export_audit(&self, q: &ExportQuery) -> Result<AuditBundle> {
        let mut stmt = self.conn().prepare(
            "SELECT d.run_id, r.encounter_alias, d.doc_id, d.section_index,
                    d.char_start, d.char_end, d.code, COALESCE(d.disposition, 'reported'),
                    d.severity, d.basis,
                    d.message, COALESCE(d.waived, ''), COALESCE(d.suppressed_by, ''),
                    r.ruleset_version,
                    r.ruleset_sha256, COALESCE(d.rule_source, ''), r.actor,
                    COALESCE(r.finished_at, r.started_at),
                    COALESCE((
                        SELECT w.idempotency_key FROM writeback_log w
                         WHERE w.doc_id = d.doc_id
                         ORDER BY w.committed_at DESC, w.id DESC LIMIT 1
                    ), '')
               FROM diagnostics d
               JOIN review_runs r ON r.run_id = d.run_id
              WHERE (?1 IS NULL OR r.encounter_alias = ?1)
                AND (?2 IS NULL OR d.run_id = ?2)
                AND (?3 IS NULL OR r.started_at >= ?3)
                AND (?4 IS NULL OR r.started_at <= ?4)
              ORDER BY r.started_at ASC, d.run_id ASC, d.disposition DESC, d.id ASC",
        )?;
        let mut rows = stmt
            .query_map(
                params![
                    q.encounter_alias.as_deref(),
                    q.run_id.as_deref(),
                    q.since.as_deref(),
                    q.until.as_deref(),
                ],
                |r| {
                    let start: i64 = r.get(4)?;
                    let end: i64 = r.get(5)?;
                    Ok(AuditRow {
                        review_run_id: r.get(0)?,
                        encounter_alias: r.get(1)?,
                        doc_id: r.get(2)?,
                        section_index: r.get::<_, i64>(3)?.max(0) as usize,
                        char_range: format!("[{start}, {end})"),
                        code: r.get(6)?,
                        disposition: r.get(7)?,
                        severity: r.get(8)?,
                        basis: r.get(9)?,
                        message: r.get(10)?,
                        waived_reason: r.get(11)?,
                        suppressed_by: r.get(12)?,
                        ruleset_version: r.get(13)?,
                        ruleset_sha256: r.get(14)?,
                        rule_source: r.get(15)?,
                        actor: r.get(16)?,
                        event_at: r.get(17)?,
                        idempotency_key: r.get(18)?,
                    })
                },
            )?
            .collect::<std::result::Result<Vec<AuditRow>, _>>()?;

        // 脱敏在查询之后、返回之前：库里存的是原样，出了这道闸的每一行都可带走。
        let redaction = redact_rows(&mut rows);
        // 标识列命中 = 拒绝导出整包，而不是打个占位符糊过去。
        // 半脱敏的行会比不导出更危险：质控办拿着一个对不上号的住院别名，
        // 既没法核对，也说不清是哪一份病历，只能回头问系统。
        if let Some(detail) = &redaction.refused {
            return Err(StoreError::ExportLeak(detail.clone()));
        }

        let runs = self.rollup_runs(q)?;
        let recalled = match (&q.encounter_alias, &q.recall_at) {
            (Some(alias), Some(at)) => self.recall_ruleset(alias, at)?,
            _ => None,
        };
        let archive_views = self.archive_view_rows(q)?;
        Ok(AuditBundle {
            columns: COLUMNS.iter().map(|s| s.to_string()).collect(),
            rows,
            runs,
            redaction,
            recalled,
            archive_views,
        })
    }

    /// 摊出随批次落库的归档核齐视图。
    ///
    /// 摘要字段从 `view_json` 里读出来是为了让 csv 能平铺；**口径说明
    /// 仍是原样的 `basis_zh`**，不重写措辞——界面上怎么写的，导出就怎么写的。
    fn archive_view_rows(&self, q: &ExportQuery) -> Result<Vec<ArchiveViewRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT view_id, run_id, encounter_alias, as_of, is_ready, view_json
               FROM archive_views a
              WHERE (?1 IS NULL OR encounter_alias = ?1)
                AND (?2 IS NULL OR run_id = ?2)
              ORDER BY as_of ASC, view_id ASC",
        )?;
        let raw = stmt
            .query_map(params![q.encounter_alias.as_deref(), q.run_id.as_deref()], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, i64>(4)? != 0,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;

        let mut out = Vec::with_capacity(raw.len());
        for (view_id, run_id, encounter_alias, as_of, is_ready, view_json) in raw {
            // view_json 读坏了不该让整包导出失败：视图是附加材料，
            // 证据行才是主体。降级成「摘要留空 + 原始 JSON 带走」，
            // 质控办至少还拿得到那份原文。
            let parsed: Option<serde_json::Value> = serde_json::from_str(&view_json).ok();
            let (missing_count, missing_days, manual_queue, basis_zh) = match &parsed {
                Some(v) => {
                    let n = v
                        .get("missing")
                        .and_then(|m| m.as_array())
                        .map(|a| a.len())
                        .unwrap_or(0);
                    let days = v
                        .get("encounter_age")
                        .and_then(|e| e.get("days"))
                        .and_then(|d| d.as_i64());
                    let q = v
                        .get("manual_queue")
                        .and_then(|m| m.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    let b = v
                        .get("basis_zh")
                        .and_then(|m| m.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|x| x.as_str().map(str::to_string))
                                .collect()
                        })
                        .unwrap_or_default();
                    (n, days, q, b)
                }
                None => (0, None, Vec::new(), Vec::new()),
            };
            out.push(ArchiveViewRow {
                view_id,
                run_id,
                encounter_alias,
                as_of,
                readiness: if is_ready { "可归档" } else { "未核齐" }.to_string(),
                missing_count,
                missing_days,
                manual_queue,
                basis_zh,
                view_json,
            });
        }
        Ok(out)
    }

    /// 按批次归并小结。行数取本次导出的实际行数，而不是全库行数——
    /// 导出筛了范围就说筛后的数，否则「本次导出 3 行」会被读成「全库 3 行」。
    fn rollup_runs(&self, q: &ExportQuery) -> Result<Vec<RunRollup>> {
        let mut stmt = self.conn().prepare(
            "SELECT r.run_id, r.encounter_alias, r.ruleset_version, r.ruleset_sha256,
                    r.actor, r.started_at, COALESCE(r.finished_at, ''),
                    r.items_total, r.items_reviewed, r.items_skipped,
                    r.unresolved_count, r.waived_count,
                    (SELECT COUNT(*) FROM diagnostics d
                      WHERE d.run_id = r.run_id
                        AND (?1 IS NULL OR r.encounter_alias = ?1)
                        AND (?2 IS NULL OR r.run_id = ?2)
                        AND (?3 IS NULL OR r.started_at >= ?3)
                        AND (?4 IS NULL OR r.started_at <= ?4))
               FROM review_runs r
              WHERE (?1 IS NULL OR r.encounter_alias = ?1)
                AND (?2 IS NULL OR r.run_id = ?2)
                AND (?3 IS NULL OR r.started_at >= ?3)
                AND (?4 IS NULL OR r.started_at <= ?4)
              ORDER BY r.started_at ASC, r.run_id ASC",
        )?;
        let mut out = Vec::new();
        let rows = stmt.query_map(
            params![
                q.encounter_alias.as_deref(),
                q.run_id.as_deref(),
                q.since.as_deref(),
                q.until.as_deref(),
            ],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                    r.get::<_, i64>(7)? as usize,
                    r.get::<_, i64>(8)? as usize,
                    r.get::<_, i64>(9)? as usize,
                    r.get::<_, i64>(10)? as usize,
                    r.get::<_, i64>(11)? as usize,
                    r.get::<_, i64>(12)? as usize,
                ))
            },
        )?;
        for r in rows {
            let (
                run_id,
                encounter_alias,
                ruleset_version,
                ruleset_sha256,
                actor,
                started_at,
                finished_at,
                items_total,
                items_reviewed,
                items_skipped,
                unresolved_count,
                waived_count,
                row_count,
            ) = r?;
            let codes = self.codes_of_run(&run_id, "reported")?;
            let suppressed_codes = self.codes_of_run(&run_id, "suppressed")?;
            // 压下的条数直接从库里数，而不是 `row_count - 留下的条数`：
            // 导出筛了范围，两个口径一旦不同源就会对不上账。
            let suppressed_count = self.count_of_run(&run_id, "suppressed")?;
            let reported_count = self.count_of_run(&run_id, "reported")?;
            out.push(RunRollup {
                review_run_id: run_id,
                encounter_alias,
                ruleset_version,
                ruleset_sha256,
                actor,
                started_at,
                finished_at,
                items_total,
                items_reviewed,
                items_skipped,
                unresolved_count,
                waived_count,
                codes,
                suppressed_codes,
                row_count,
                reported_count,
                suppressed_count,
            });
        }
        Ok(out)
    }

    /// 某批次在指定处置下的命中码集合。
    ///
    /// `disposition` 走参数而不是拼串：值来自常量，不是外部输入，
    /// 但参数化仍比模板拼接少一处「记得改两遍」的机会。
    fn codes_of_run(&self, run_id: &str, disposition: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn().prepare(
            "SELECT DISTINCT code FROM diagnostics
              WHERE run_id = ?1 AND COALESCE(disposition, 'reported') = ?2
              ORDER BY code ASC",
        )?;
        let codes = stmt
            .query_map(params![run_id, disposition], |r| r.get::<_, String>(0))?
            .collect::<std::result::Result<Vec<String>, _>>()?;
        Ok(codes)
    }

    fn count_of_run(&self, run_id: &str, disposition: &str) -> Result<usize> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM diagnostics
              WHERE run_id = ?1 AND COALESCE(disposition, 'reported') = ?2",
            params![run_id, disposition],
            |r| r.get::<_, i64>(0),
        )? as usize)
    }

    /// 落一份归档核齐视图快照（CLI 与桌面端共用）。
    pub fn save_archive_view(&self, view: &ArchiveSnapshot) -> Result<()> {
        self.record_archive_view(view)
    }
}

/// 对导出行做二次脱敏。**就地改写**并返回替换清单。
///
/// 标识列命中即返回 `E_EXPORT_LEAK`——见模块文档「为什么两档处置不同」。
pub fn redact_rows(rows: &mut [AuditRow]) -> RedactionReport {
    let mut report = RedactionReport::default();
    for row in rows.iter_mut() {
        let tag = format!("{}@{}", row.code, row.doc_id);
        for (field, value) in [
            ("住院别名", &mut row.encounter_alias),
            ("文书件id", &mut row.doc_id),
        ] {
            let (cleaned, hits) = desensitize(value);
            if hits > 0 {
                report.refused = Some(format!(
                    "{field} 列含未脱敏内容（{tag}：{value}），已拒绝导出；\
                     住院别名与件 id 本应是脱敏别名，请先排查上游绑定"
                ));
                return report;
            }
            debug_assert_eq!(cleaned, value.as_str());
        }
        for (field, value) in [
            ("问题描述", &mut row.message),
            ("规范依据", &mut row.basis),
            ("豁免原因", &mut row.waived_reason),
            ("折叠去向", &mut row.suppressed_by),
        ] {
            let (cleaned, hits) = desensitize(value);
            if hits > 0 {
                report.replaced_fields += 1;
                report.replaced_at.push(format!("{field}@{tag}"));
                *value = cleaned;
            }
        }
    }
    report
}

/// 一个字段的 csv 转义（RFC 4180）。
///
/// 只有含分隔符、引号、换行或首尾空白时才加引号——无脑给每格加引号
/// 会让 Excel 里每列都变成「文本型」，日期排序就废了。
pub fn csv_escape(field: &str) -> String {
    let needs = field.contains(',')
        || field.contains('"')
        || field.contains('\n')
        || field.contains('\r')
        || field != field.trim();
    if !needs {
        return field.to_string();
    }
    let mut out = String::with_capacity(field.len() + 2);
    out.push('"');
    for c in field.chars() {
        if c == '"' {
            out.push('"');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// 导出 csv。**带 UTF-8 BOM**，行尾 `\r\n`（Excel 习惯）。
pub fn to_csv(bundle: &AuditBundle) -> String {
    let mut out = String::from(UTF8_BOM);
    out.push_str(&bundle.columns.join(","));
    out.push_str("\r\n");
    for row in &bundle.rows {
        let cells = [
            row.review_run_id.as_str(),
            row.encounter_alias.as_str(),
            row.doc_id.as_str(),
            &row.section_index.to_string(),
            row.char_range.as_str(),
            row.code.as_str(),
            row.disposition_zh(),
            row.severity.as_str(),
            row.basis.as_str(),
            row.message.as_str(),
            row.waived_reason.as_str(),
            row.suppressed_by.as_str(),
            row.ruleset_version.as_str(),
            row.ruleset_sha256.as_str(),
            row.rule_source.as_str(),
            row.actor.as_str(),
            row.event_at.as_str(),
            row.idempotency_key.as_str(),
        ];
        // 临时数组活不过 push_str，先拼一整行再写入。
        let line: Vec<String> = cells.iter().map(|c| csv_escape(c)).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

/// 导出 json（人读缩进 / 机器读皆可）。
pub fn to_json(bundle: &AuditBundle) -> String {
    serde_json::to_string_pretty(bundle).unwrap_or_else(|e| format!("{{\"error\":\"{e}\"}}"))
}

/// 归档核齐视图单独一份 csv。
///
/// **刻意不与证据行合表**：两者的列完全不同（一边是「哪条码报在哪一段」，
/// 一边是「这次能不能归档签字、欠件几天」），硬拼成一张表只会得到一堆
/// 空格。质控办要带走的是两份材料，各自能独立打开、能独立解释。
pub fn archive_to_csv(bundle: &AuditBundle) -> String {
    const ARCHIVE_COLUMNS: &[&str] = &[
        "视图号",
        "审查批次",
        "住院别名",
        "统计时刻",
        "归档核齐",
        "缺件数",
        "欠件天数",
        "人工队列",
        "口径说明",
    ];
    let mut out = String::from(UTF8_BOM);
    out.push_str(&ARCHIVE_COLUMNS.join(","));
    out.push_str("\r\n");
    for v in &bundle.archive_views {
        let cells = [
            v.view_id.as_str(),
            v.run_id.as_str(),
            v.encounter_alias.as_str(),
            v.as_of.as_str(),
            v.readiness.as_str(),
            &v.missing_count.to_string(),
            &v.missing_days
                .map(|d| d.to_string())
                .unwrap_or_else(|| "未登记发现时刻".to_string()),
            &v.manual_queue.join("、"),
            &v.basis_zh.join(" / "),
        ];
        let line: Vec<String> = cells.iter().map(|c| csv_escape(c)).collect();
        out.push_str(&line.join(","));
        out.push_str("\r\n");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_field_is_left_bare_so_excel_keeps_it_typed() {
        assert_eq!(csv_escape("QC-PSY-01"), "QC-PSY-01");
        assert_eq!(csv_escape("[78, 78)"), "\"[78, 78)\"");
    }

    #[test]
    fn quotes_are_doubled_not_dropped() {
        assert_eq!(csv_escape("他说\"已收\""), "\"他说\"\"已收\"\"\"");
    }

    #[test]
    fn newline_and_cjk_survive_round_trip() {
        let raw = "第一行：缺患方签字\n第二行：𠮷护士已核";
        let escaped = csv_escape(raw);
        assert!(escaped.starts_with('"') && escaped.ends_with('"'));
        assert_eq!(escaped.matches('"').count(), 2);
        assert!(escaped.contains('\n'));
        assert!(escaped.contains('𠮷'));
    }

    #[test]
    fn csv_starts_with_the_bom_excel_needs() {
        let bundle = AuditBundle {
            columns: COLUMNS.iter().map(|s| s.to_string()).collect(),
            rows: vec![],
            runs: vec![],
            redaction: RedactionReport::default(),
            recalled: None,
            archive_views: vec![],
        };
        let csv = to_csv(&bundle);
        assert!(csv.starts_with('\u{FEFF}'));
        assert!(csv.contains("\r\n"));
    }
}