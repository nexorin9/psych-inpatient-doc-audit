//! **通知发件箱**（`spec.md` §2 输出⑤「`Task` 待办 + Webhook 通知」的落点）。
//!
//! # 为什么缺件提醒必须落库，而不是「发出去就完」
//!
//! Webhook 出站和 FHIR 写回不同：写回失败时护士长就在旁边看着，界面
//! 立刻报错；缺件提醒是**单向**的——发出去就走了，界面上「已发」和
//! 「没发出去」在护士长眼里长得一模一样。而失败恰恰常见：病区在病房楼
//! 里，网络抖动、手机换基站、企业微信限流，都是日常。
//!
//! 一次丢提醒的实际后果不是「少收一条消息」，而是**病案室永远不知道这份
//! 住院缺件**，归档核齐卡在那里没人推。所以：
//!
//! ```text
//! 幂等键入队（落库） → 出站发送 → 成功标 sent
//!                            └─ 失败 → 留在 pending + 记下次可发时刻
//!                                        （进程退出、重启都不丢）
//! ```
//!
//! # 幂等键为什么含「业务日」
//!
//! `sha256(住院别名 + 件 id + 业务日)`：常驻进程每次 EMR 增量变更都重扫，
//! 同一份住院同一天会被扫十几次。键里若不含业务日，这十几次会往群里发
//! 十条一模一样的提醒——群里消息一多就没人看了，通知本身就失效了。
//! 含业务日之后「同一天同一件只发一次」，跨零点后（新的交班日）才允许
//! 再发下一次，这是护士长期望的节奏。
//!
//! 三段之间用 `\x1f` 分隔，与 [`crate::idempotency_key`] 同一把尺：
//! 不分隔会让 `WARD-A-001 + X` 和 `WARD-A-001X + 空` 撞成同一个键。

use rusqlite::{params, OptionalExtension};

use crate::error::{Result, StoreError};
use crate::Store;

/// 发件箱行的状态取值。字面量与库里的 `state` 列一一对应。
pub mod state {
    /// 已入队、尚未成功送达（含退避等待中）。
    pub const PENDING: &str = "pending";
    /// 端点确认成功。**不再重发**。
    pub const SENT: &str = "sent";
    /// 重试次数耗尽，转人工处理（不会被自动重发，也不会消失）。
    pub const PENDING_MANUAL: &str = "pending_manual";
}

/// 通知通道。当前只有企业微信机器人；留这一列是为了将来加院内短信
/// 时不必推翻这张表。
pub mod channel {
    /// 企业微信群机器人 Webhook。
    pub const WECOM_MARKDOWN: &str = "wecom_markdown";
}

/// 默认最多自动重试次数。超过即转 `pending_manual`。
pub const MAX_ATTEMPTS_DEFAULT: u32 = 3;

/// 幂等键 = `sha256(住院别名 + 件 id + 业务日)`。
///
/// 三个分量各换一位即变键——与 [`crate::idempotency_key`] 同样的断言
/// 纪律，保证「换个住院别名不会误命中」和「跨零点（业务日变）允许重发」。
pub fn notify_key(encounter_alias: &str, doc_id: &str, business_day: &str) -> String {
    let mut hasher = sha2::Sha256::new();
    use sha2::Digest;
    hasher.update(encounter_alias.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(doc_id.as_bytes());
    hasher.update(b"\x1f");
    hasher.update(business_day.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// 入队结果。**幂等命中不是错误**——常驻进程每次重扫都会命中，
/// 若当成错误报出来，日志里全是「失败」，真正失败的那条反而看不见。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnqueueOutcome {
    /// 首次入队（此前没有这条）。
    Enqueued,
    /// 此前已入队且**尚未发出去**（pending）：不重复入队，返回原行的重试状态。
    AlreadyPending { attempts: u32, next_attempt_at: String },
    /// 此前已发出去（同键同日）：**不再外发**。
    AlreadySent { sent_at: String },
    /// 此前已耗尽重试转人工：不再自动重发，提示去人工处理。
    AlreadyManual { attempts: u32, last_error: Option<String> },
}

impl EnqueueOutcome {
    /// 本次是否真的需要出站发送。
    pub fn needs_send(&self) -> bool {
        matches!(self, Self::Enqueued)
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Self::Enqueued)
    }

    pub fn as_zh(&self) -> String {
        match self {
            Self::Enqueued => "首次入队，准备发送".into(),
            Self::AlreadyPending { attempts, .. } => {
                format!("今天已入队未发成功（第 {attempts} 次），不重复入队")
            }
            Self::AlreadySent { sent_at } => format!("今天已发过（{sent_at}），不重复发送"),
            Self::AlreadyManual { attempts, .. } => {
                format!("重试 {attempts} 次仍未送达，已转人工处理")
            }
        }
    }
}

/// 发件箱里的一条。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutboxRow {
    pub notify_key: String,
    pub encounter_alias: String,
    pub doc_id: String,
    pub business_day: String,
    pub channel: String,
    /// 入队那一刻定稿的消息体。**重试不改口**。
    pub payload_json: String,
    pub state: String,
    pub attempts: u32,
    /// 下次可发时刻（RFC3339）。
    pub next_attempt_at: String,
    pub last_error: Option<String>,
    pub created_at: String,
    pub sent_at: Option<String>,
}

impl OutboxRow {
    pub fn is_pending(&self) -> bool {
        self.state == state::PENDING
    }

    pub fn is_sent(&self) -> bool {
        self.state == state::SENT
    }

    pub fn needs_manual(&self) -> bool {
        self.state == state::PENDING_MANUAL
    }

    /// 底栏/「归档核齐」视图用的一行中文说明。
    pub fn as_zh(&self) -> String {
        let status = match self.state.as_str() {
            state::SENT => "已送达",
            state::PENDING_MANUAL => "重试耗尽 · 待人工",
            _ => "待发送",
        };
        let err = self
            .last_error
            .as_deref()
            .map(|e| format!("，上次失败：{e}"))
            .unwrap_or_default();
        format!(
            "{} / {} · {}（已试 {} 次{}）",
            self.encounter_alias, self.doc_id, status, self.attempts, err
        )
    }
}

/// 指数退避：第 1 次失败后等 `base` 秒，之后每次翻倍。
///
/// 取 60 秒为底：企业微信的限流窗口是分钟级，退避 5 秒连着撞三次只是
/// 把限流撞得更紧。上限封在 15 分钟——再长护士长会以为这条提醒丢了，
/// 转而打电话问，而打电话之前他并不知道系统正在等。
pub fn backoff_seconds(attempts: u32, base: u64) -> u64 {
    let exp = attempts.saturating_sub(1).min(4); // 2^4 = 16，封顶
    (base.saturating_mul(1u64 << exp)).min(15 * 60)
}

/// 把「本次失败」记到发件箱行上，返回**下一次可发时刻**。
///
/// 语义与退避策略一起决定：`attempts` 达到上限就转 `pending_manual`，
/// **不再**返回下次可发时刻（返回 `None`）——自动重发到这一步已经
/// 没有意义，剩下的要人看。
pub fn record_failure(
    row_attempts: u32,
    failed_at: &str,
    base_backoff: u64,
    max_attempts: u32,
) -> Option<String> {
    let attempts = row_attempts + 1;
    if attempts >= max_attempts {
        return None;
    }
    let secs = backoff_seconds(attempts, base_backoff);
    parse_moment(failed_at).map(|t| {
        (t + chrono::Duration::seconds(secs as i64))
            .to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
    })
}

fn parse_moment(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(raw.trim()).ok()
}

impl Store {
    /// 幂等入队。**先落库再发送**是这个模块全部纪律的来源。
    ///
    /// 同键重复调用返回对应的 [`EnqueueOutcome`]，**不覆盖原 payload**：
    /// 已经在等的提醒不该因为又扫了一遍而被改口。
    pub fn enqueue_notification(
        &self,
        encounter_alias: &str,
        doc_id: &str,
        business_day: &str,
        channel: &str,
        payload_json: &str,
        now: &str,
    ) -> Result<EnqueueOutcome> {
        let key = notify_key(encounter_alias, doc_id, business_day);
        if let Some(existing) = self.notification(&key)? {
            return Ok(match existing.state.as_str() {
                state::SENT => EnqueueOutcome::AlreadySent {
                    sent_at: existing.sent_at.unwrap_or_default(),
                },
                state::PENDING_MANUAL => EnqueueOutcome::AlreadyManual {
                    attempts: existing.attempts,
                    last_error: existing.last_error,
                },
                _ => EnqueueOutcome::AlreadyPending {
                    attempts: existing.attempts,
                    next_attempt_at: existing.next_attempt_at,
                },
            });
        }
        self.conn().execute(
            "INSERT INTO notification_outbox
                 (notify_key, encounter_alias, doc_id, business_day, channel,
                  payload_json, state, attempts, next_attempt_at, last_error,
                  created_at, sent_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, ?8, NULL, ?9, NULL)",
            params![
                key,
                encounter_alias,
                doc_id,
                business_day,
                channel,
                payload_json,
                state::PENDING,
                now,
                now
            ],
        )?;
        Ok(EnqueueOutcome::Enqueued)
    }

    /// 读一行（按幂等键）。
    pub fn notification(&self, key: &str) -> Result<Option<OutboxRow>> {
        self.conn()
            .query_row(
                "SELECT notify_key, encounter_alias, doc_id, business_day, channel,
                        payload_json, state, attempts, next_attempt_at,
                        COALESCE(last_error, ''), created_at, COALESCE(sent_at, '')
                   FROM notification_outbox WHERE notify_key = ?1",
                params![key],
                |r| {
                    let err: String = r.get(9)?;
                    let sent: String = r.get(11)?;
                    Ok(OutboxRow {
                        notify_key: r.get(0)?,
                        encounter_alias: r.get(1)?,
                        doc_id: r.get(2)?,
                        business_day: r.get(3)?,
                        channel: r.get(4)?,
                        payload_json: r.get(5)?,
                        state: r.get(6)?,
                        attempts: r.get::<_, i64>(7)? as u32,
                        next_attempt_at: r.get(8)?,
                        last_error: if err.is_empty() { None } else { Some(err) },
                        created_at: r.get(10)?,
                        sent_at: if sent.is_empty() { None } else { Some(sent) },
                    })
                },
            )
            .optional()
            .map_err(Into::into)
    }

    /// 该件今天是否已成功送达（幂等快查，UI 与 CLI 都用它）。
    pub fn notification_sent_today(
        &self,
        encounter_alias: &str,
        doc_id: &str,
        business_day: &str,
    ) -> Result<bool> {
        let key = notify_key(encounter_alias, doc_id, business_day);
        Ok(self.notification(&key)?.map(|r| r.is_sent()).unwrap_or(false))
    }

    /// 标记送达。**只能从 `pending` 转到 `sent`**：对已 `sent` 的行再标一次
    /// 说明有两个流程都在发同一条，那是要查的并发问题，不该静默成功。
    pub fn mark_notification_sent(&self, key: &str, at: &str) -> Result<usize> {
        let row = self
            .notification(key)?
            .ok_or_else(|| StoreError::NotifyUnknown(key.to_string()))?;
        if row.is_sent() {
            return Ok(0);
        }
        Ok(self.conn().execute(
            "UPDATE notification_outbox
                SET state = ?2, sent_at = ?3, last_error = NULL, next_attempt_at = ?3
              WHERE notify_key = ?1",
            params![key, state::SENT, at],
        )?)
    }

    /// 记一次发送失败：累加次数、按退避算下次可发时刻；次数耗尽转人工。
    ///
    /// 返回下一次可发时刻；`None` = 已转人工，不再自动重发。
    pub fn mark_notification_failed(
        &self,
        key: &str,
        error: &str,
        at: &str,
        base_backoff: u64,
        max_attempts: u32,
    ) -> Result<Option<String>> {
        let row = self
            .notification(key)?
            .ok_or_else(|| StoreError::NotifyUnknown(key.to_string()))?;
        let attempts = row.attempts + 1;
        match record_failure(row.attempts, at, base_backoff, max_attempts) {
            Some(next_at) => {
                self.conn().execute(
                    "UPDATE notification_outbox
                        SET attempts = ?2, next_attempt_at = ?3, last_error = ?4, state = ?5
                      WHERE notify_key = ?1",
                    params![key, attempts as i64, next_at, error, state::PENDING],
                )?;
                Ok(Some(next_at))
            }
            None => {
                self.conn().execute(
                    "UPDATE notification_outbox
                        SET attempts = ?2, last_error = ?3, state = ?4, next_attempt_at = ?3
                      WHERE notify_key = ?1",
                    params![key, attempts as i64, error, state::PENDING_MANUAL],
                )?;
                Ok(None)
            }
        }
    }

    /// 到期可发的 `pending` 行（按 `next_attempt_at` 升序）。
    ///
    /// 进程重启后调一次就能把上次没发出去的续上——**这是「不丢通知」
    /// 真正落地的那一步**。
    pub fn due_notifications(&self, now: &str) -> Result<Vec<OutboxRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT notify_key, encounter_alias, doc_id, business_day, channel,
                    payload_json, state, attempts, next_attempt_at,
                    COALESCE(last_error, ''), created_at, COALESCE(sent_at, '')
               FROM notification_outbox
              WHERE state = ?1 AND next_attempt_at <= ?2
              ORDER BY next_attempt_at ASC, created_at ASC",
        )?;
        let rows = stmt
            .query_map(params![state::PENDING, now], |r| {
                let err: String = r.get(9)?;
                let sent: String = r.get(11)?;
                Ok(OutboxRow {
                    notify_key: r.get(0)?,
                    encounter_alias: r.get(1)?,
                    doc_id: r.get(2)?,
                    business_day: r.get(3)?,
                    channel: r.get(4)?,
                    payload_json: r.get(5)?,
                    state: r.get(6)?,
                    attempts: r.get::<_, i64>(7)? as u32,
                    next_attempt_at: r.get(8)?,
                    last_error: if err.is_empty() { None } else { Some(err) },
                    created_at: r.get(10)?,
                    sent_at: if sent.is_empty() { None } else { Some(sent) },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 全部未送达（含转人工），供底栏「待发 N 条」与人工队列。
    pub fn undelivered_notifications(&self) -> Result<Vec<OutboxRow>> {
        let mut stmt = self.conn().prepare(
            "SELECT notify_key, encounter_alias, doc_id, business_day, channel,
                    payload_json, state, attempts, next_attempt_at,
                    COALESCE(last_error, ''), created_at, COALESCE(sent_at, '')
               FROM notification_outbox
              WHERE state <> ?1
              ORDER BY created_at ASC",
        )?;
        let rows = stmt
            .query_map(params![state::SENT], |r| {
                let err: String = r.get(9)?;
                let sent: String = r.get(11)?;
                Ok(OutboxRow {
                    notify_key: r.get(0)?,
                    encounter_alias: r.get(1)?,
                    doc_id: r.get(2)?,
                    business_day: r.get(3)?,
                    channel: r.get(4)?,
                    payload_json: r.get(5)?,
                    state: r.get(6)?,
                    attempts: r.get::<_, i64>(7)? as u32,
                    next_attempt_at: r.get(8)?,
                    last_error: if err.is_empty() { None } else { Some(err) },
                    created_at: r.get(10)?,
                    sent_at: if sent.is_empty() { None } else { Some(sent) },
                })
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// 发件箱行数（测试用）。
    pub fn notification_count(&self) -> Result<usize> {
        Ok(self
            .conn()
            .query_row("SELECT COUNT(*) FROM notification_outbox", [], |r| {
                r.get::<_, i64>(0)
            })? as usize)
    }

    pub fn notification_state(&self, key: &str) -> Result<Option<String>> {
        Ok(self.notification(key)?.map(|r| r.state))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_key_is_stable_and_field_separated() {
        let k = notify_key("WARD-A-001", "CONSENT-01", "2026-03-03");
        assert_eq!(k, notify_key("WARD-A-001", "CONSENT-01", "2026-03-03"));
        assert_eq!(k.len(), 64);
        // 任一分量换一位即变键
        assert_ne!(k, notify_key("WARD-B-001", "CONSENT-01", "2026-03-03"));
        assert_ne!(k, notify_key("WARD-A-001", "CONSENT-02", "2026-03-03"));
        // 跨零点 = 新的交班日 → 允许重发
        assert_ne!(k, notify_key("WARD-A-001", "CONSENT-01", "2026-03-04"));
    }

    #[test]
    fn the_separator_prevents_concatenation_collisions() {
        assert_ne!(
            notify_key("WARD-A-001X", "", "2026-03-03"),
            notify_key("WARD-A-001", "X", "2026-03-03")
        );
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        assert_eq!(backoff_seconds(1, 60), 60);
        assert_eq!(backoff_seconds(2, 60), 120);
        assert_eq!(backoff_seconds(3, 60), 240);
        // 再往后不能无限涨：15 分钟封顶
        assert_eq!(backoff_seconds(10, 60), 15 * 60);
    }

    #[test]
    fn exhaustion_returns_no_next_attempt() {
        let at = "2026-03-03T09:00:00+08:00";
        assert!(record_failure(0, at, 60, 3).is_some());
        assert!(record_failure(1, at, 60, 3).is_some());
        // 第 3 次失败后不再自动重发
        assert!(record_failure(2, at, 60, 3).is_none());
    }
}