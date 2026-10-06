//! **外发预占与重放防护**（`spec.md` §4「幂等 / 并发 / 重试」的深挖落点）。
//!
//! # 为什么 `writeback_log` 的唯一约束不够
//!
//! `writeback_log.idempotency_key` 上的 `UNIQUE` 只约束**已落库**的写回。
//! 可 Bundle transaction 是在**落库之前**就发出去的：
//!
//! ```text
//! 进程 A                          进程 B（另一台机器 / 另一个标签页）
//! ├ SELECT 幂等键 → 没有
//! │                            ├ SELECT 幂等键 → 没有
//! ├ POST Bundle transaction ──► 服务端建好 Composition（201）
//! │                            ├ POST Bundle transaction ──► 服务端再建一份
//! ├ INSERT writeback_log  ✔    ├ INSERT writeback_log  ✗ 撞唯一约束
//! ```
//!
//! 第二个进程确实被挡住了，但**外发已经发生**：服务端按 `POST` 建了两份同
//! id 的 `Composition`，撤不回来。「不产生第二条写回」在本机成立，在院内
//! 端点上不成立——而院内端点上那一份才是护士长看到的东西。
//!
//! 所以把占键提前到**外发之前**：谁先把键占住，谁才允许发 transaction。
//!
//! ```text
//! reserve（BEGIN IMMEDIATE + 主键唯一） → 外发 → settle（committed / released）
//! ```
//!
//! # 四态与各自的现场口径
//!
//! | state | 什么时候 | 撞上它的人拿到什么 |
//! |---|---|---|
//! | `in_flight` | 占键成功，外发进行中 | `E_WRITEBACK_CONFLICT`（带首次操作人与时刻，可重试） |
//! | `committed` | 外发成功、结果已落库 | 重放：原样回放首次结果，`replayed=true`，**0 次外发** |
//! | `released` | 占键后外发失败，已放开 | 重新预占（不是冲突——上次确实没发出去） |
//! | `in_doubt` | 进程在外发与落库之间崩了，**结果未知** | `E_REPLAY_IN_DOUBT`：**禁止自动重发**，要人工核对 |
//!
//! `in_doubt` 是这张表里最重要的一态。「崩溃后重试」若不带它，等于把
//! 「服务端可能已经建好件」当成「没发过」再发一次，护士长那边就会看到
//! 两份同 id 的知情同意。因此 [`Store::recover_orphans`] 把上次进程遗留的
//! `in_flight` 升级成 `in_doubt`，而不是悄悄删掉让它重新发。
//!
//! # 整批语义
//!
//! 预占是**整批原子的**：一个 transaction 的多条 entry 要么全部占住、
//! 要么一条都不占。半占住的批次等于「服务端可能建了 3 件中的 1 件」，
//! 那是最难收拾的一种状态。调用方拿到的 [`ReserveOutcome`] 因此永远是
//! 三态之一，不存在部分成功。

use std::time::Duration;

use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};

use crate::error::{Result, StoreError};
use crate::Store;

/// 预占键的状态取值。字面量与库里的 `state` 列一一对应。
pub mod state {
    /// 已占键，外发进行中。
    pub const IN_FLIGHT: &str = "in_flight";
    /// 外发成功且结果已落库。
    pub const COMMITTED: &str = "committed";
    /// 占键后外发失败，已放开，可重发。
    pub const RELEASED: &str = "released";
    /// 进程崩在外发与落库之间，结果未知。
    pub const IN_DOUBT: &str = "in_doubt";
}

/// 一条预占的键。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimRow {
    pub idempotency_key: String,
    pub encounter_alias: String,
    pub doc_id: String,
    pub batch_id: String,
    pub state: String,
    pub claimed_by: String,
    pub claimed_at: String,
    pub tx_result_json: Option<String>,
}

impl ClaimRow {
    /// 键的短形式（12 位），进报错文案与界面用。
    pub fn short_key(&self) -> &str {
        let end = self.idempotency_key.len().min(12);
        &self.idempotency_key[..end]
    }
}

/// 要预占的一件（件 id + 该件内容哈希，键由二者与住院别名算出）。
#[derive(Debug, Clone)]
pub struct ClaimTarget<'a> {
    pub doc_id: &'a str,
    pub content_sha256: &'a str,
}

/// 预占结果。三态，**没有部分成功**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReserveOutcome {
    /// 整批占住，调用方**可以**外发 transaction。
    Reserved { batch_id: String, keys: Vec<String> },
    /// 同键此前已成功写回：原样回放首次结果，**不外发**。
    Replay {
        batch_id: String,
        keys: Vec<String>,
        /// 首次外发返回的 transaction 结果（JSON），可能为 `None`
        /// （首次那笔没有记录结果，例如人工核销过的 in_doubt）。
        tx_result_json: Option<String>,
        first_actor: String,
        first_at: String,
    },
    /// 整批被拒：至少一个键已被占或结果未知。**一条都占不住**。
    Refused {
        batch_id: String,
        /// 决定本次结果的键（冲突 / 存疑），界面据此提示用户去看哪一条。
        blocker: ClaimRow,
        reason: RefusalReason,
    },
}

/// 整批被拒的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalReason {
    /// 别人正在提交（`in_flight`）：对方提交完即可重试。
    Conflict,
    /// 上次结果未知（`in_doubt`）：必须人工核对，不能自动重发。
    InDoubt,
}

impl RefusalReason {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Conflict => "并发提交中",
            Self::InDoubt => "上次提交结果未知",
        }
    }
}

/// 人工核销 `in_doubt` 后的处置。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolution {
    /// 人工已核对院内端点**确实建好了件**：补记为已写回（`committed`），
    /// 不再外发。
    AlreadyWritten,
    /// 人工已核对**没建**：放开该键（`released`），允许正常重发。
    NotWritten,
}

impl Store {
    // ------------------------------------------------------------ 预占

    /// 整批预占幂等键。**只有 [`ReserveOutcome::Reserved`] 才允许外发**。
    ///
    /// 整批原子的实现方式是一把 `BEGIN IMMEDIATE` 事务：sqlite 在
    /// `IMMEDIATE` 下立刻拿写锁，于是「查是否已占」与「写入占键」之间
    /// 没有窗口——两个进程同时进来时，第二个一定在拿锁之后才查，因而看得见
    /// 第一个写下的那一行。写成 `DEFERRED` 就会在两处 `SELECT` 之间留下
    /// 升级锁失败（`SQLITE_BUSY`）或双写的缝隙。
    pub fn reserve_writeback(
        &self,
        encounter_alias: &str,
        targets: &[ClaimTarget<'_>],
        batch_id: &str,
        actor: &str,
        at: &str,
    ) -> Result<ReserveOutcome> {
        if targets.is_empty() {
            return Err(StoreError::NothingSelected(batch_id.to_string()));
        }
        let keys: Vec<String> = targets
            .iter()
            .map(|t| crate::idempotency_key(encounter_alias, t.doc_id, t.content_sha256))
            .collect();

        let tx = immediate(self.conn())?;
        // 逐键查已存在的预占记录
        let mut blockers: Vec<ClaimRow> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT idempotency_key, encounter_alias, doc_id, batch_id, state,
                        claimed_by, claimed_at, tx_result_json
                   FROM writeback_claims WHERE idempotency_key = ?1",
            )?;
            for key in &keys {
                let row: Option<ClaimRow> = stmt
                    .query_row(params![key], row_mapper)
                    .optional()?;
                // 只保留有约束力的记录：released 的键不算占着（上次的
                // 外发确实失败了，可以重发），走下面的 upsert 重新占上。
                if let Some(row) = row.filter(|r| r.state != state::RELEASED) {
                    blockers.push(row);
                }
            }
        }
        if !blockers.is_empty() {
            tx.rollback()?;
            // 优先级：存疑 > 冲突 > 重放。
            //
            // - 存疑与冲突都要用户先动一动（前者人工核对、后者等一下），
            //   把两者都指出来只会让提示变长，反而更没用；
            // - 全部 `committed` 才是纯重放：直接回放首次结果，让护士长
            //   在两台机器上双击也只建一份件。
            let blocker = blockers
                .iter()
                .find(|r| r.state == state::IN_DOUBT)
                .or_else(|| blockers.iter().find(|r| r.state == state::IN_FLIGHT));
            match blocker {
                Some(b) => {
                    let reason = if b.state == state::IN_DOUBT {
                        RefusalReason::InDoubt
                    } else {
                        RefusalReason::Conflict
                    };
                    return Ok(ReserveOutcome::Refused {
                        batch_id: batch_id.to_string(),
                        blocker: b.clone(),
                        reason,
                    });
                }
                None => {
                    // 整批都已 committed → 重放。取第一条的结果回放即可：
                    // 同一个 transaction 的 Provenance 与件列表是一份。
                    let first = &blockers[0];
                    return Ok(ReserveOutcome::Replay {
                        batch_id: batch_id.to_string(),
                        keys,
                        tx_result_json: first.tx_result_json.clone(),
                        first_actor: first.claimed_by.clone(),
                        first_at: first.claimed_at.clone(),
                    });
                }
            }
        }

        // 全新批次（或全部是 released 的重发）：逐键占住。
        // 一句 upsert 同时覆盖「首次占键」与「重新占键」两种情形——
        // 分成两条语句时，「UPDATE 命中 0 行」和「键不存在」会长得一模一样，
        // 得靠 `changes()` 反推，正是最容易写错的地方。
        for (key, t) in keys.iter().zip(targets) {
            tx.execute(
                "INSERT INTO writeback_claims
                   (idempotency_key, encounter_alias, doc_id, content_sha256,
                    batch_id, state, claimed_by, claimed_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
                 ON CONFLICT(idempotency_key) DO UPDATE SET
                    encounter_alias = excluded.encounter_alias,
                    doc_id          = excluded.doc_id,
                    content_sha256  = excluded.content_sha256,
                    batch_id        = excluded.batch_id,
                    state           = excluded.state,
                    claimed_by      = excluded.claimed_by,
                    claimed_at      = excluded.claimed_at,
                    settled_at      = NULL,
                    settled_by      = NULL,
                    tx_result_json  = NULL,
                    release_reason  = NULL",
                params![
                    key,
                    encounter_alias,
                    t.doc_id,
                    t.content_sha256,
                    batch_id,
                    state::IN_FLIGHT,
                    actor,
                    at,
                ],
            )?;
        }
        tx.commit()?;
        Ok(ReserveOutcome::Reserved {
            batch_id: batch_id.to_string(),
            keys,
        })
    }

    // ------------------------------------------------------------ 结算

    /// 外发成功：把该批次全部预占键结算为 `committed`，并记下首次结果。
    ///
    /// 结果 JSON 落库是「重放不再外发」的前提：重放时要能原样回放首次
    /// transaction 的结果（已建件列表 + Provenance 位置），而不是再问一次
    /// 服务端。
    pub fn settle_committed(
        &self,
        batch_id: &str,
        tx_result_json: Option<&str>,
        actor: &str,
        at: &str,
    ) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE writeback_claims
                SET state = ?2, settled_at = ?3, settled_by = ?4, tx_result_json = ?5
              WHERE batch_id = ?1 AND state = ?6",
            params![
                batch_id,
                state::COMMITTED,
                at,
                actor,
                tx_result_json,
                state::IN_FLIGHT
            ],
        )?;
        Ok(n)
    }

    /// 外发失败：把该批次预占键放开为 `released`，允许重发。
    ///
    /// **不写 `kind=writeback` 审计事件**：失败这次没有产生已生效事实，
    /// 写进去会让「写回了几次」的审计计数虚高。失败痕迹落在预占表的
    /// `release_reason` 与 `settled_at` 上，够排障用。
    pub fn release_batch(
        &self,
        batch_id: &str,
        reason: &str,
        actor: &str,
        at: &str,
    ) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE writeback_claims
                SET state = ?2, settled_at = ?3, settled_by = ?4, release_reason = ?5
              WHERE batch_id = ?1 AND state = ?6",
            params![
                batch_id,
                state::RELEASED,
                at,
                actor,
                reason,
                state::IN_FLIGHT
            ],
        )?;
        Ok(n)
    }

    // ------------------------------------------------------------ 查询

    /// 撤回后撤销预占：把该批次 `committed` 的键退回 `released`。
    ///
    /// 撤回走的是反向 transaction（DELETE），服务端那份件**已经不存在**了，
    /// 键必须跟着放开——否则同一件补写后会命中 `Replay` 被当成「已完成」，
    /// 界面上点提交却什么都不会发出去。
    ///
    /// `in_doubt` 的键不受影响：那种情况服务端到底有没有件还没核实，
    /// 由 [`Store::resolve_claim`] 走人工，不许撤回顺手「顺手放开」。
    pub fn revoke_batch(&self, batch_id: &str, reason: &str, actor: &str, at: &str) -> Result<usize> {
        let n = self.conn().execute(
            "UPDATE writeback_claims
                SET state = ?2, settled_at = ?3, settled_by = ?4, release_reason = ?5
              WHERE batch_id = ?1 AND state = ?6",
            params![
                batch_id,
                state::RELEASED,
                at,
                actor,
                reason,
                state::COMMITTED
            ],
        )?;
        Ok(n)
    }

    /// 查一条预占记录（按幂等键）。
    pub fn claim_of(&self, idempotency_key: &str) -> Result<Option<ClaimRow>> {
        Ok(self
            .conn()
            .query_row(
                "SELECT idempotency_key, encounter_alias, doc_id, batch_id, state,
                        claimed_by, claimed_at, tx_result_json
                   FROM writeback_claims WHERE idempotency_key = ?1",
                params![idempotency_key],
                row_mapper,
            )
            .optional()?)
    }

    /// 某住院别名下全部处于 `in_doubt` 的键（崩溃遗留，待人工核对）。
    pub fn in_doubt_claims(&self, encounter_alias: &str) -> Result<Vec<ClaimRow>> {
        self.claims_where(
            "encounter_alias = ?1 AND state = ?2",
            params![encounter_alias, state::IN_DOUBT],
        )
    }

    /// 某住院别名下仍占着（`in_flight`）的键。
    pub fn in_flight_claims(&self, encounter_alias: &str) -> Result<Vec<ClaimRow>> {
        self.claims_where(
            "encounter_alias = ?1 AND state = ?2",
            params![encounter_alias, state::IN_FLIGHT],
        )
    }

    /// 全部 `in_doubt` 键（跨住院），供启动自检报「上次有 N 笔结果未知」。
    pub fn all_in_doubt_claims(&self) -> Result<Vec<ClaimRow>> {
        self.claims_where("state = ?1", params![state::IN_DOUBT])
    }

    fn claims_where(&self, where_clause: &str, args: impl rusqlite::Params) -> Result<Vec<ClaimRow>> {
        let sql = format!(
            "SELECT idempotency_key, encounter_alias, doc_id, batch_id, state,
                    claimed_by, claimed_at, tx_result_json
               FROM writeback_claims WHERE {where_clause}
              ORDER BY encounter_alias ASC, claimed_at ASC, idempotency_key ASC"
        );
        let mut stmt = self.conn().prepare(&sql)?;
        let rows = stmt
            .query_map(args, row_mapper)?
            .collect::<std::result::Result<Vec<ClaimRow>, _>>()?;
        Ok(rows)
    }

    // ------------------------------------------------------ 崩溃恢复

    /// 启动自检：把上次进程遗留的 `in_flight` 升级为 `in_doubt`。
    ///
    /// `cutoff` 由调用方注入（界面上一般取「本次进程启动时刻」），因此
    /// 本次运行自己占的键**不会**被误升级——只有上一次会话遗留的才是。
    ///
    /// **不删行、不自动重发**。理由写在模块文档里：崩在外发与落库之间时，
    /// 服务端可能已经建好件；当成「没发过」再发一次，护士长就会看到两份
    /// 同 id 的知情同意。升级成 `in_doubt` 后，这条路只能由人走。
    pub fn recover_orphans(&self, cutoff: &str) -> Result<usize> {
        Ok(self.conn().execute(
            "UPDATE writeback_claims
                SET state = ?1,
                    release_reason = '上次进程在提交过程中退出，结果未知，须人工核对'
              WHERE state = ?2 AND claimed_at < ?3",
            params![state::IN_DOUBT, state::IN_FLIGHT, cutoff],
        )?)
    }

    /// 人工核销一条 `in_doubt` 键（见 [`Resolution`]）。
    pub fn resolve_claim(
        &self,
        idempotency_key: &str,
        resolution: Resolution,
        reason: &str,
        actor: &str,
        at: &str,
    ) -> Result<()> {
        let row = self
            .claim_of(idempotency_key)?
            .ok_or_else(|| StoreError::ClaimUnknown(idempotency_key.to_string()))?;
        if row.state != state::IN_DOUBT {
            return Err(StoreError::ClaimState {
                key: idempotency_key.to_string(),
                state: leak_state(&row.state),
                action: "人工核销",
            });
        }
        if reason.trim().is_empty() {
            return Err(StoreError::ClaimState {
                key: idempotency_key.to_string(),
                state: state::IN_DOUBT,
                action: "人工核销（缺理由）",
            });
        }
        let new_state = match resolution {
            Resolution::AlreadyWritten => state::COMMITTED,
            Resolution::NotWritten => state::RELEASED,
        };
        self.conn().execute(
            "UPDATE writeback_claims
                SET state = ?2, settled_at = ?3, settled_by = ?4, release_reason = ?5
              WHERE idempotency_key = ?1",
            params![idempotency_key, new_state, at, actor, reason],
        )?;
        Ok(())
    }

    /// `in_doubt` 键计数（托盘角标与启动摘要用）。
    pub fn in_doubt_count(&self) -> Result<usize> {
        Ok(self.conn().query_row(
            "SELECT COUNT(*) FROM writeback_claims WHERE state = ?1",
            params![state::IN_DOUBT],
            |r| r.get(0),
        )?)
    }
}

fn row_mapper(r: &rusqlite::Row<'_>) -> rusqlite::Result<ClaimRow> {
    Ok(ClaimRow {
        idempotency_key: r.get(0)?,
        encounter_alias: r.get(1)?,
        doc_id: r.get(2)?,
        batch_id: r.get(3)?,
        state: r.get(4)?,
        claimed_by: r.get(5)?,
        claimed_at: r.get(6)?,
        tx_result_json: r.get(7)?,
    })
}

/// 拿写锁再开事务。`IMMEDIATE` 是并发正确性的关键，见
/// [`Store::reserve_writeback`] 的说明。
fn immediate(conn: &Connection) -> Result<Transaction<'_>> {
    let _ = conn.busy_timeout(Duration::from_secs(10));
    Ok(Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?)
}

/// 把库里的 state 字符串映射回静态串（错误枚举要求 `&'static str`）。
fn leak_state(raw: &str) -> &'static str {
    match raw {
        state::COMMITTED => state::COMMITTED,
        state::RELEASED => state::RELEASED,
        state::IN_DOUBT => state::IN_DOUBT,
        _ => state::IN_FLIGHT,
    }
}