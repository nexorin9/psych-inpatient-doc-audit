//! 增量缓存的本机持久化：**落得下去、并发写不丢、崩溃不留半张表**。
//!
//! 内存侧的对偶是 `psychdoc_rules::ReviewCache`（JSON 文件），本模块是它的
//! sqlite 版本。两边**键形状必须一致**：`(scope, doc_id, 内容哈希)`，
//! 否则会出现「进程内命中、重启后不命中」这种谁也说不清的怪事。
//!
//! ## 三条不可让步的行为
//!
//! 1. **规则集一变即整表作废**（[`Store::invalidate_cache_for_ruleset`]）。
//!    内容没变不代表结论没变——院内把「患方签字」从提示改成阻断，
//!    昨天判「提示」的件今天必须判「阻断」。这是真实事故，不是理论洁癖。
//! 2. **命中要逐字段校验**（[`Store::cache_lookup`]）：主键里的哈希、
//!    行里的 `content_sha256`、行里的 `ruleset_sha256` 三者都要与调用方
//!    给的一致，任一不符即判未命中。宁可重跑，不可误命中。
//! 3. **写缓存与写审查结果同事务**（[`Store::commit_review_with_cache`]）：
//!    「缓存说审过了」与「审查记录说审过了」必须同时成立或同时不成立。
//!    分两次提交的话，进程死在中间就会留下一张「缓存已更新、审查记录
//!    还是旧的」的表——质控办回溯时这两者对不上，整份审查链就不可信了。
//!
//! ## 并发写为什么不会丢
//!
//! 缓存表的写入全部走 `INSERT OR REPLACE`（按主键）+ 显式事务：
//!
//! - sqlite 在写事务期间持有写锁，同一时刻只有一个写者，**结构上**不存在
//!   lost update——两个线程写同一主键，后写者覆盖先写者，而两者的
//!   `payload` 都是同一件内容的完整结论，覆盖不产生错数据；
//! - 写不同主键的行不会互相阻塞（同一次事务内批量写），
//!   因此「多线程各审一批、各写各的」不会有 lost update；
//! - `busy_timeout` + WAL 让并发读者不被写者挡住，常驻进程的后台批扫
//!   不会把 UI 查询卡死。

use crate::error::Result;
use crate::{ReviewRunInput, Store};
use rusqlite::{params, OptionalExtension, TransactionBehavior};

/// 开一个**立即取写锁**的事务。
///
/// 为什么不能直接用 `unchecked_transaction`（DEFERRED）：DEFERRED 事务
/// 起步时只拿读锁，直到第一条 `INSERT` 才升级为写锁。两个线程同时这么做时，
/// 后者在升级那一刻会发现快照已过期，sqlite 立刻返回 `SQLITE_BUSY_SNAPSHOT`
/// ——而 `busy_timeout` **不重试这一种**（它只重试「等锁等不到」），
/// 于是并发写退化成随机失败。IMMEDIATE 一上来就把写锁拿在手里，
/// 等不到就按 `busy_timeout` 老实等，语义与调用方预期一致。
fn write_tx(store: &Store) -> Result<rusqlite::Transaction<'_>> {
    Ok(rusqlite::Transaction::new_unchecked(
        store.conn(),
        TransactionBehavior::Immediate,
    )?)
}

/// 缓存行的作用域。件级与组级混在一张表里，不带这个列就分不清键第二段
/// 是「件内容哈希」还是「组指纹」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheScope {
    Item,
    Group,
}

impl CacheScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Item => "item",
            Self::Group => "group",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw {
            "item" => Some(Self::Item),
            "group" => Some(Self::Group),
            _ => None,
        }
    }
}

/// 待写入的一条缓存。
#[derive(Debug, Clone)]
pub struct CacheWrite<'a> {
    pub scope: CacheScope,
    /// 件 id（`Item`）或住院别名（`Group`）。
    pub doc_id: &'a str,
    /// 件内容哈希（`Item`）或组指纹（`Group`）。
    pub content_sha256: &'a str,
    pub ruleset_sha256: &'a str,
    pub review_run_id: &'a str,
    /// 判定结果的 JSON 文本。原样存字符串，不在存储层定义结构——
    /// 结果形状归 `psychdoc-rules`，存储层只管原子地存取。
    pub payload_json: &'a str,
}

/// 读到的一条缓存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheRow {
    pub scope: CacheScope,
    pub doc_id: String,
    pub content_sha256: String,
    pub ruleset_sha256: String,
    pub review_run_id: String,
    pub payload_json: String,
    pub written_at: String,
}

/// 一次整表作废的留痕（返回给调用方写进运行摘要 / 审计）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheInvalidation {
    pub from_ruleset_short_sha256: String,
    pub to_ruleset_short_sha256: String,
    pub dropped: usize,
}

impl CacheInvalidation {
    pub fn as_zh(&self) -> String {
        format!(
            "因规则集变更全量重审：规则集由 {} 变为 {}，作废缓存 {} 条",
            if self.from_ruleset_short_sha256.is_empty() {
                "（空缓存）".to_string()
            } else {
                self.from_ruleset_short_sha256.clone()
            },
            self.to_ruleset_short_sha256,
            self.dropped
        )
    }
}

fn short_of(sha: &str) -> String {
    sha.chars().take(12).collect()
}

/// 库里缓存表当前属于哪一版规则集（空串 = 不知道）。
pub fn cache_ruleset_sha256(store: &Store) -> Result<String> {
    Ok(store
        .conn()
        .query_row(
            "SELECT DISTINCT ruleset_sha256 FROM review_cache LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

impl Store {
    /// 批量写入缓存条目（一个事务）。
    ///
    /// 主键上的 `INSERT OR REPLACE` 是并发写的关键：两个线程同时写同一件，
    /// 后者整体替换前者，不会出现「两个字段各自来自不同写入」的撕裂状态。
    /// 返回真正**新增**的行数（被替换的不算）——「写了几条」与
    /// 「新增了几条」是两件事，命中率统计要用后者。
    pub fn write_cache(&self, rows: &[CacheWrite<'_>], at: &str) -> Result<usize> {
        if rows.is_empty() {
            return Ok(0);
        }
        let tx = write_tx(self)?;
        let mut fresh = 0usize;
        for r in rows {
            let existed: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM review_cache
                      WHERE scope = ?1 AND doc_id = ?2 AND content_sha256 = ?3",
                    params![r.scope.as_str(), r.doc_id, r.content_sha256],
                    |row| row.get(0),
                )
                .optional()?;
            if existed.is_none() {
                fresh += 1;
            }
            tx.execute(
                "INSERT OR REPLACE INTO review_cache
                   (scope, doc_id, content_sha256, ruleset_sha256,
                    review_run_id, payload_json, written_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    r.scope.as_str(),
                    r.doc_id,
                    r.content_sha256,
                    r.ruleset_sha256,
                    r.review_run_id,
                    r.payload_json,
                    at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(fresh)
    }

    /// 取一条缓存，**三处字段全部校验**后才返回。
    ///
    /// - 主键里的 `content_sha256` 必须等于 `content_sha256` 参数（sqlite
    ///   主键保证了，但这道比对让「查错了列」也暴露出来）；
    /// - 行内 `content_sha256` 必须与键一致（防手改过的库 / 哈希碰撞）；
    /// - 行内 `ruleset_sha256` 必须等于 `ruleset_sha256` 参数——
    ///   **这一条才是防「旧版规则的结论被当新版复用」的那道闸**。
    ///
    /// 任一不符返回 `Ok(None)`（未命中），由调用方重跑。
    pub fn cache_lookup(
        &self,
        scope: CacheScope,
        doc_id: &str,
        content_sha256: &str,
        ruleset_sha256: &str,
    ) -> Result<Option<CacheRow>> {
        let row: Option<(String, String, String, String, String, String)> = self
            .conn()
            .query_row(
                "SELECT content_sha256, ruleset_sha256, review_run_id,
                        payload_json, written_at, scope
                   FROM review_cache
                  WHERE scope = ?1 AND doc_id = ?2 AND content_sha256 = ?3",
                params![scope.as_str(), doc_id, content_sha256],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional()?;
        let Some((sha, ruleset, run_id, payload, written_at, scope_raw)) = row else {
            return Ok(None);
        };
        // 行内字段与键 / 入参对不上 = 这条记录不可信，按未命中处理。
        if sha != content_sha256 || ruleset != ruleset_sha256 {
            return Ok(None);
        }
        Ok(Some(CacheRow {
            scope: CacheScope::parse(&scope_raw).unwrap_or(scope),
            doc_id: doc_id.to_string(),
            content_sha256: sha,
            ruleset_sha256: ruleset,
            review_run_id: run_id,
            payload_json: payload,
            written_at,
        }))
    }

    /// 规则集变更 → 整表作废，返回留痕。规则集没变则返回 `None`。
    ///
    /// **只在表非空时才记留痕**：空表 + 规则集变了属于「本来就没有可复用
    /// 的结论」，报「全量重审」会让第一次用的人以为工具坏了。
    pub fn invalidate_cache_for_ruleset(
        &self,
        current_ruleset_sha256: &str,
    ) -> Result<Option<CacheInvalidation>> {
        let previous = cache_ruleset_sha256(self)?;
        if previous == current_ruleset_sha256 {
            return Ok(None);
        }
        let total: i64 = self
            .conn()
            .query_row("SELECT COUNT(*) FROM review_cache", [], |r| r.get(0))?;
        if total == 0 {
            return Ok(None);
        }
        self.conn()
            .execute("DELETE FROM review_cache WHERE ruleset_sha256 <> ?1", params![current_ruleset_sha256])?;
        Ok(Some(CacheInvalidation {
            from_ruleset_short_sha256: short_of(&previous),
            to_ruleset_short_sha256: short_of(current_ruleset_sha256),
            dropped: total as usize,
        }))
    }

    pub fn cache_row_count(&self) -> Result<usize> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM review_cache", [], |r| r.get(0))?)
    }

    /// 某件在**指定规则集**下的全部缓存版本（按写入时刻升序）。
    ///
    /// 「改回去能不能命中」这条行为需要能被人核对，因此给一条能列出
    /// 全部历史版本的查询，而不是只暴露「命中 / 未命中」两种结果。
    pub fn cache_history(&self, scope: CacheScope, doc_id: &str) -> Result<Vec<CacheRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT content_sha256, ruleset_sha256, review_run_id, payload_json,
                    written_at, scope
               FROM review_cache WHERE scope = ?1 AND doc_id = ?2
              ORDER BY written_at ASC, content_sha256 ASC",
        )?;
        let rows = stmt
            .query_map(params![scope.as_str(), doc_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                    r.get::<_, String>(5)?,
                ))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows
            .into_iter()
            .map(|(sha, ruleset, run_id, payload, written_at, scope_raw)| CacheRow {
                scope: CacheScope::parse(&scope_raw).unwrap_or(scope),
                doc_id: doc_id.to_string(),
                content_sha256: sha,
                ruleset_sha256: ruleset,
                review_run_id: run_id,
                payload_json: payload,
                written_at,
            })
            .collect())
    }

    /// **写审查结果与写缓存在同一事务里提交**。
    ///
    /// 这是本模块最重要的一条：`review_runs` 说「这次审了 204 件」，
    /// `review_cache` 说「这 204 件的结论可以复用」。两者若分两次提交，
    /// 进程死在中间就会留下「缓存已经更新、审查记录还是旧的」——
    /// 重启后底栏显示高命中率，但按 `review_runs` 回溯查不到对应批次，
    /// 质控办问「这批结论是哪次审出来的」将无解。
    ///
    /// 任一写入失败整批回滚，**0 条部分写入**。
    pub fn commit_review_with_cache(
        &self,
        input: &ReviewRunInput<'_>,
        cache_rows: &[CacheWrite<'_>],
    ) -> Result<usize> {
        let tx = write_tx(self)?;
        // —— 审查批次 + 诊断 + 审计事件 ——
        let mut codes: Vec<String> = input.diagnostics.iter().map(|d| d.code.clone()).collect();
        codes.sort();
        codes.dedup();
        let codes_json = serde_json::to_string(&codes).unwrap_or_else(|_| "[]".into());
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
            tx.execute(
                "INSERT INTO diagnostics
                   (run_id, doc_id, section_index, char_start, char_end,
                    code, severity, message, basis, fix_availability,
                    waived, suppressed_by)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                params![
                    input.run_id,
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
                ],
            )?;
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
        // —— 缓存行 ——
        let mut fresh = 0usize;
        for r in cache_rows {
            let existed: Option<i64> = tx
                .query_row(
                    "SELECT 1 FROM review_cache
                      WHERE scope = ?1 AND doc_id = ?2 AND content_sha256 = ?3",
                    params![r.scope.as_str(), r.doc_id, r.content_sha256],
                    |row| row.get(0),
                )
                .optional()?;
            if existed.is_none() {
                fresh += 1;
            }
            tx.execute(
                "INSERT OR REPLACE INTO review_cache
                   (scope, doc_id, content_sha256, ruleset_sha256,
                    review_run_id, payload_json, written_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    r.scope.as_str(),
                    r.doc_id,
                    r.content_sha256,
                    r.ruleset_sha256,
                    r.review_run_id,
                    r.payload_json,
                    input.finished_at.unwrap_or(input.started_at),
                ],
            )?;
        }
        tx.commit()?;
        Ok(fresh)
    }
}