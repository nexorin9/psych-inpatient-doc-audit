//! 增量读游标：`_since` 下界的落库、单调推进与断点续拉。
//!
//! ## 一条不可让步的不变量
//!
//! **游标推进与件落库必须同一个事务**（[`Store::commit_page`]）。分两次提交
//! 会出现两种都无法排查的事故：
//!
//! | 分两次提交的后果 | 现场表现 |
//! |---|---|
//! | 先推进游标、后落件，中间崩了 | 这批件**永久丢失**，且没有任何报错 |
//! | 先落件、后推进游标，中间崩了 | 下次重拉这批件，靠 `idempotency_key` 去重 |
//!
//! 第二种是**可以接受的**（宁可重拉不可漏拉）。所以本模块的顺序固定为
//! 「写件 → 推进游标」同事务，而不是反过来。
//!
//! ## 为什么不按住院别名存游标
//!
//! 拉的是「本病区自上次以来变了哪些件」，粒度是**端点 + 租户**，不是某次
//! 住院。按住院别名存会让「A 住院拉过 → 游标推进 → B 住院接着从 A 的点拉」
//! 成立，而 A 住院那些没被 B 拉到的件就再也进不来了。粒度错一格就是漏件。
//!
//! ## 时刻比较一律用真实时间
//!
//! `_since` 是 RFC3339 带偏移量的时刻。比较时**按绝对时刻比**，不按字面串比：
//! `2026-03-02T23:40:00+08:00` 与 `2026-03-02T15:40:00Z` 是同一刻，按串比会
//! 得出「倒退了一截」的错误结论，把已拉过的区间重拉一遍。

use crate::error::{Result, StoreError};
use crate::Store;

use chrono::{DateTime, FixedOffset};
use rusqlite::{params, OptionalExtension, Transaction, TransactionBehavior};

/// 一条游标由「租户 + 资源类型」两个坐标唯一定位。
///
/// 把它们收成一个结构体而不是两个散参数，是为了在
/// [`Store::commit_page`] 那种已有 8 个参数的地方不再靠调用方记忆
/// 「哪个是租户哪个是资源类型」——传反了不会报错，只会静默拉错端点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CursorKey {
    pub tenant_id: String,
    pub resource_type: String,
}

impl CursorKey {
    pub fn new(tenant_id: impl Into<String>, resource_type: impl Into<String>) -> Self {
        Self {
            tenant_id: tenant_id.into(),
            resource_type: resource_type.into(),
        }
    }
}

/// 游标当前指向的一处位置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncCursor {
    pub tenant_id: String,
    pub resource_type: String,
    /// 下次拉取的 `_since` 下界（RFC3339）。首次拉取为空串，表示「全量」。
    pub since: String,
    /// 最近一次**整批成功落库**的时刻。空串表示从未成功过。
    pub last_success_at: String,
    /// 上次那批翻了几页（供运行摘要显示「增量 3 页 / 全量 40 页」）。
    pub pages_done: i64,
    pub run_id: Option<String>,
}

impl SyncCursor {
    pub fn key(&self) -> CursorKey {
        CursorKey::new(self.tenant_id.clone(), self.resource_type.clone())
    }
}

impl SyncCursor {
    /// 是否已有可续拉的位置。`false` 表示这是一次全量拉。
    pub fn is_incremental(&self) -> bool {
        !self.since.trim().is_empty()
    }

    pub fn as_zh(&self) -> String {
        if self.is_incremental() {
            format!(
                "{} / {} 增量自 {}{}",
                self.tenant_id,
                self.resource_type,
                self.since,
                if self.last_success_at.is_empty() {
                    String::new()
                } else {
                    format!("（上次成功 {}）", self.last_success_at)
                }
            )
        } else {
            format!(
                "{} / {} 尚无游标，本次为全量拉取",
                self.tenant_id, self.resource_type
            )
        }
    }
}

impl Store {
    /// 读当前游标。**没有游标不是错误**——那是第一次拉，返回 `None`，
    /// 调用方按全量处理。
    pub fn load_cursor(&self, tenant_id: &str, resource_type: &str) -> Result<Option<SyncCursor>> {
        let row = self.conn().query_row(
            "SELECT tenant_id, resource_type, since, COALESCE(last_success_at, ''),
                    pages_done, run_id
               FROM sync_cursors
              WHERE tenant_id = ?1 AND resource_type = ?2",
            params![tenant_id, resource_type],
            |r| {
                Ok(SyncCursor {
                    tenant_id: r.get(0)?,
                    resource_type: r.get(1)?,
                    since: r.get(2)?,
                    last_success_at: r.get(3)?,
                    pages_done: r.get(4)?,
                    run_id: r.get(5)?,
                })
            },
        )
        .optional()?;
        Ok(row)
    }

    /// 当前 `_since` 下界；没有游标时返回空串（= 全量）。
    pub fn current_since(&self, tenant_id: &str, resource_type: &str) -> Result<String> {
        Ok(self
            .load_cursor(tenant_id, resource_type)?
            .map(|c| c.since)
            .unwrap_or_default())
    }

    /// 推进游标。**只在整批成功落库后调用**。
    ///
    /// 推进是**单调**的：新时刻早于当前游标时**拒绝**并报 `E_CURSOR_REWIND`。
    /// 这一条挡住的是真实会发生的一种情形——回滚一批数据后重拉，服务端
    /// 按 `_since` 过滤时会把旧版本件排除在外，游标若跟着回退，重拉到的
    /// 是空集，看起来「今天一件变更都没有」。
    pub fn advance_cursor(
        &self,
        key: &CursorKey,
        new_since: &str,
        at: &str,
        pages_done: usize,
        run_id: Option<&str>,
    ) -> Result<()> {
        if let Some(cur) = self.load_cursor(&key.tenant_id, &key.resource_type)? {
            if cur.is_incremental() && is_before(new_since, &cur.since) {
                return Err(StoreError::CursorRewind {
                    tenant_id: key.tenant_id.clone(),
                    resource_type: key.resource_type.clone(),
                    current: cur.since,
                    attempted: new_since.to_string(),
                });
            }
        }
        self.conn().execute(
            "INSERT INTO sync_cursors
                 (tenant_id, resource_type, since, last_success_at, pages_done, run_id)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(tenant_id, resource_type) DO UPDATE SET
                 since = excluded.since,
                 last_success_at = excluded.last_success_at,
                 pages_done = excluded.pages_done,
                 run_id = excluded.run_id",
            params![
                key.tenant_id,
                key.resource_type,
                new_since,
                at,
                pages_done as i64,
                run_id
            ],
        )?;
        Ok(())
    }

    /// 「件落库 + 游标推进」同事务。**这是断点续拉不漏件的唯一保证**。
    ///
    /// `items` 为本批新拉到的件（`DocItem` 已在 v1 表登记）；`new_since` 为
    /// 本批各件 `meta.lastUpdated` 的最大值。任一步失败 → 整体回滚，
    /// 游标原地不动，下次从同一点重来。
    pub fn commit_page(
        &self,
        key: &CursorKey,
        items: &[psychdoc_model::DocItem],
        new_since: &str,
        at: &str,
        pages_done: usize,
        run_id: Option<&str>,
    ) -> Result<()> {
        if let Some(c) = self.load_cursor(&key.tenant_id, &key.resource_type)? {
            if c.is_incremental() && is_before(new_since, &c.since) {
                return Err(StoreError::CursorRewind {
                    tenant_id: key.tenant_id.clone(),
                    resource_type: key.resource_type.clone(),
                    current: c.since,
                    attempted: new_since.to_string(),
                });
            }
        }
        let tx = Transaction::new_unchecked(self.conn(), TransactionBehavior::Immediate)?;
        for item in items {
            upsert_item(&tx, item, at)?;
        }
        upsert_cursor_tx(&tx, key, new_since, at, pages_done, run_id)?;
        tx.commit()?;
        Ok(())
    }

    /// 游标条数（测试与运行摘要用）。
    pub fn cursor_count(&self) -> Result<usize> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM sync_cursors", [], |r| {
                r.get::<_, i64>(0)
            })? as usize)
    }

    /// 清掉某租户某资源的游标，回到全量拉取。**须留痕**：
    /// 这是一次「明知会全量重拉」的操作，护士长会问为什么今天拉这么久。
    pub fn reset_cursor(&self, tenant_id: &str, resource_type: &str, at: &str) -> Result<()> {
        self.conn().execute(
            "INSERT INTO sync_cursors
                 (tenant_id, resource_type, since, last_success_at, pages_done, run_id)
             VALUES (?1, ?2, '', ?3, 0, 'reset')
             ON CONFLICT(tenant_id, resource_type) DO UPDATE SET
                 since = '',
                 last_success_at = excluded.last_success_at,
                 pages_done = 0,
                 run_id = 'reset'",
            params![tenant_id, resource_type, at],
        )?;
        Ok(())
    }
}

/// 同一事务内 upsert 游标行。[`Store::advance_cursor`] 与
/// [`Store::commit_page`] 共用这一句 SQL——两份写法迟早会漂移成
/// 「推进」和「记录」各写各的。
fn upsert_cursor_tx(
    tx: &Transaction<'_>,
    key: &CursorKey,
    new_since: &str,
    at: &str,
    pages_done: usize,
    run_id: Option<&str>,
) -> Result<()> {
    tx.execute(
        "INSERT INTO sync_cursors
             (tenant_id, resource_type, since, last_success_at, pages_done, run_id)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)
         ON CONFLICT(tenant_id, resource_type) DO UPDATE SET
             since = excluded.since,
             last_success_at = excluded.last_success_at,
             pages_done = excluded.pages_done,
             run_id = excluded.run_id",
        params![
            key.tenant_id,
            key.resource_type,
            new_since,
            at,
            pages_done as i64,
            run_id
        ],
    )?;
    Ok(())
}

/// 同一事务内 upsert 一件。与 [`crate::Store::register_items`] 的语义一致，
/// 差别只在于这里复用调用方已经开好的事务。
///
/// `content_sha256` 直接存 [`crate::content_of`] 的规范化文本，与
/// `register_items` 保持同一把尺——两处算法若不同，件级增量缓存
/// （Task 26）会把同一件判成「内容变了」而无谓重审。
fn upsert_item(tx: &Transaction<'_>, item: &psychdoc_model::DocItem, at: &str) -> Result<()> {
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
            crate::content_of(item),
            format!("{:?}", item.binding),
            at,
        ],
    )?;
    Ok(())
}

/// `a` 是否**真实早于** `b`。
///
/// 形态非法（不是 RFC3339、缺偏移量）时返回 `false`——即「不比、不判倒退」。
/// 宁可放过一次可能的回退（下次多拉一点），也不因解析失败把正常推进
/// 误判成回退并拒绝。
fn is_before(a: &str, b: &str) -> bool {
    match (parse_moment(a), parse_moment(b)) {
        (Some(x), Some(y)) => x < y,
        _ => false,
    }
}

/// 解析带偏移量的 RFC3339 时刻。
pub fn parse_moment(raw: &str) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(raw.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_instant_written_in_two_offsets_is_the_same_moment() {
        // `+08:00` 与 `Z` 只是写法不同；按串比会得出「倒退 8 小时」。
        assert!(!is_before(
            "2026-03-02T15:40:00Z",
            "2026-03-02T23:40:00+08:00"
        ));
        assert!(!is_before(
            "2026-03-02T23:40:00+08:00",
            "2026-03-02T15:40:00Z"
        ));
    }

    #[test]
    fn a_genuinely_earlier_moment_is_detected() {
        assert!(is_before(
            "2026-03-02T23:30:00+08:00",
            "2026-03-02T23:40:00+08:00"
        ));
    }

    #[test]
    fn an_unparsable_moment_is_never_treated_as_a_rewind() {
        // 形态坏掉时应放行（宁可多拉），而不是把正常推进误判成回退并拒绝。
        assert!(!is_before("昨天", "2026-03-02T23:40:00+08:00"));
        assert!(!is_before("2026-03-02T23:40:00", "2026-03-02T23:39:00+08:00"));
    }

    #[test]
    fn an_incremental_cursor_reads_back_as_incremental() {
        let c = SyncCursor {
            tenant_id: "ward-a".into(),
            resource_type: "Composition".into(),
            since: String::new(),
            last_success_at: String::new(),
            pages_done: 0,
            run_id: None,
        };
        assert!(!c.is_incremental());
        assert!(c.as_zh().contains("全量"));

        let c = SyncCursor {
            since: "2026-03-02T23:40:00+08:00".into(),
            ..c
        };
        assert!(c.is_incremental());
        assert!(c.as_zh().contains("2026-03-02T23:40:00+08:00"));
    }
}