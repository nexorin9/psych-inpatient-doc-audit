//! 缺件通知的**投递编排**：幂等入队 → 出站 → 失败留在队列 → 重启续发。
//!
//! ## 为什么不把「入队 + 发送」写成一个函数
//!
//! 因为这两件事的失败后果完全不同：
//!
//! - **入队失败**（sqlite 坏了）：什么都没发生，护士长不会收到提醒，
//!   但界面上会明确报错，可以立刻处理；
//! - **发送失败**（网络抖动 / 限流）：入队已经成功，提醒躺在队列里等重试。
//!   此时**不能**把它当成一次失败往上抛然后丢掉——那正是「病案室永远
//!   不知道这份住院缺件」的原因。
//!
//! 因此这里的 API 分成 [`notify_once`]（一次尝试，失败不丢）与
//! [`drain_pending`]（把上次没发出去的续上），后者在进程启动时调一次。
//!
//! ## 幂等键的位置
//!
//! 键在**入队**时占用，不是在发送时。因此常驻进程同一天第 12 次重扫
//! 这份住院时，`enqueue` 直接命中 `AlreadySent`，**0 次外发**。
//! 若把键放在发送成功后记，重复扫描会连发十几条——群里的通知一多就
//! 没人看了，通知本身就失效了。

use crate::error::FhirError;
use crate::transport::Transport;
use crate::wecom::{self, NoticeContent, WeComConfig};

use psychdoc_store::notify::{
    channel, notify_key, EnqueueOutcome, OutboxRow, MAX_ATTEMPTS_DEFAULT,
};
use psychdoc_store::Store as StoreHandle;
use psychdoc_store::StoreError;

use serde_json::Value;

/// 本模块的错误：**存储侧**（发件箱落库）与**出站侧**（Webhook 投递）
/// 两类都要在这里返回。合成一个枚举而不是二选一，是因为调用方面对的
/// 就是一件事——「这条提醒发出去了吗」，两种失败对它一视同仁。
#[derive(Debug, thiserror::Error)]
pub enum NotifyError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Fhir(#[from] FhirError),
}

impl From<NotifyError> for FhirError {
    /// 存储侧失败并到 `FhirError` 时保留原始码：调用方（CLI 退出码、
    /// UI 底栏）按码分处置，`E_NOTIFY_UNKNOWN` 不能被压成一个泛化码。
    fn from(e: NotifyError) -> Self {
        match e {
            NotifyError::Store(s) => FhirError::Store { code: s.code(), detail: s.to_string() },
            NotifyError::Fhir(f) => f,
        }
    }
}

pub type Result<T> = std::result::Result<T, NotifyError>;

/// 一次通知尝试的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotifyOutcome {
    /// 已成功送达。
    Sent,
    /// 今天已发过（幂等命中），未外发。
    AlreadySent,
    /// 已入队但本次没发出去，仍在队列里等退避重试。
    Queued {
        /// 下次可发时刻；`None` = 重试耗尽已转人工。
        next_attempt_at: Option<String>,
        /// 失败原因（中文，可直接贴交办单）。
        error: String,
    },
    /// 重试耗尽，本条转人工处理。
    NeedsManual { attempts: u32, error: String },
}

impl NotifyOutcome {
    pub fn is_sent(&self) -> bool {
        matches!(self, Self::Sent)
    }

    /// 是否已经进入「系统不再自动处理」的终态。
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Sent | Self::AlreadySent | Self::NeedsManual { .. })
    }

    pub fn as_zh(&self) -> String {
        match self {
            Self::Sent => "已送达".into(),
            Self::AlreadySent => "今天已发过，未重复发送".into(),
            Self::Queued {
                next_attempt_at: Some(at),
                error,
            } => format!("未送达，已入队待重试（下次 {at}）：{error}"),
            Self::Queued {
                next_attempt_at: None,
                error,
            } => format!("未送达，已入队：{error}"),
            Self::NeedsManual { attempts, error } => {
                format!("重试 {attempts} 次仍未送达，转人工：{error}")
            }
        }
    }
}

/// 退避参数。抽成结构体是为了测试能注入「零退避」跑完整个重试链，
/// 而不必真的等 60 秒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BackoffPolicy {
    /// 第一次失败后等多少秒（之后翻倍，15 分钟封顶）。
    pub base_seconds: u64,
    /// 最多自动重试几次。
    pub max_attempts: u32,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            base_seconds: 60,
            max_attempts: MAX_ATTEMPTS_DEFAULT,
        }
    }
}

impl BackoffPolicy {
    pub fn immediate() -> Self {
        Self {
            base_seconds: 0,
            max_attempts: MAX_ATTEMPTS_DEFAULT,
        }
    }
}

/// 一次通知尝试（幂等入队 + 出站）。
///
/// `doc_id` 是幂等键的第二个分量：**一件一提醒**。一份住院缺三件，
/// 就是三条独立提醒、三个独立键——护士长逐条勾掉时粒度才对得上。
pub fn notify_once(
    store: &StoreHandle,
    config: &WeComConfig,
    transport: &dyn Transport,
    content: &NoticeContent,
    doc_id: &str,
    policy: BackoffPolicy,
    now: &str,
) -> Result<NotifyOutcome> {
    // 凭据校验在**入队之前**：没配通知就不该往发件箱塞一条永远发不出去的
    // 提醒，把「待发 N 条」这个底栏数字永久顶起来。
    let (markdown, hidden) = wecom::build_markdown(content, config.max_lines);
    let payload = wecom::build_payload(&markdown);
    let payload_json = serde_json::to_string(&payload).map_err(|e| FhirError::WecomShape {
        detail: format!("消息体序列化失败：{e}"),
    })?;

    let key = notify_key(&content.encounter_alias, doc_id, &content.business_day);
    let enqueued = store.enqueue_notification(
        &content.encounter_alias,
        doc_id,
        &content.business_day,
        channel::WECOM_MARKDOWN,
        &payload_json,
        now,
    )?;

    match &enqueued {
        EnqueueOutcome::AlreadySent { .. } => return Ok(NotifyOutcome::AlreadySent),
        EnqueueOutcome::AlreadyPending { .. } => {
            // 已在队列里等着：这轮不重复入队，但**仍然可以试着现在发**
            // （常驻进程每轮重扫都调本函数，不试就永远等不到退避到期）。
            // 由 drain_pending 负责按 next_attempt_at 判断，这次直接跳过。
            return Ok(NotifyOutcome::Queued {
                next_attempt_at: None,
                error: "同键通知已在队列中等待，本次不重复入队".into(),
            });
        }
        EnqueueOutcome::AlreadyManual { attempts, last_error } => {
            return Ok(NotifyOutcome::NeedsManual {
                attempts: *attempts,
                error: last_error
                    .clone()
                    .unwrap_or_else(|| "此前重试已耗尽".into()),
            })
        }
        EnqueueOutcome::Enqueued => {}
    }

    debug_assert!(
        content.lines.len() >= hidden,
        "隐藏条数不应超过总条数"
    );
    attempt_send(store, &key, config, transport, &payload, policy, now)
}

/// 发一条已入队的通知。成功标 `sent`，失败记次数并算下次可发时刻。
fn attempt_send(
    store: &StoreHandle,
    key: &str,
    config: &WeComConfig,
    transport: &dyn Transport,
    payload: &Value,
    policy: BackoffPolicy,
    now: &str,
) -> Result<NotifyOutcome> {
    match wecom::deliver(&config.webhook_url, payload, transport) {
        Ok(_) => {
            store.mark_notification_sent(key, now)?;
            Ok(NotifyOutcome::Sent)
        }
        Err(e) => {
            // 失败**先落库再返回**：进程此刻退出，这条提醒也不会丢。
            let err_zh = e.to_string();
            let next = store.mark_notification_failed(
                key,
                &err_zh,
                now,
                policy.base_seconds,
                policy.max_attempts,
            )?;
            Ok(match next {
                Some(at) => NotifyOutcome::Queued {
                    next_attempt_at: Some(at),
                    error: err_zh,
                },
                None => {
                    let row = store.notification(key)?;
                    NotifyOutcome::NeedsManual {
                        attempts: row.map(|r| r.attempts).unwrap_or(policy.max_attempts),
                        error: err_zh,
                    }
                }
            })
        }
    }
}

/// 一次批量 drain 的结果（启动时与托盘常驻的定时 tick 各调一次）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DrainSummary {
    /// 到期且本次成功送达的条数。
    pub sent: usize,
    /// 到期但这次仍没发出去、继续排队的条数。
    pub still_pending: usize,
    /// 本次耗尽重试转人工的条数。
    pub needs_manual: usize,
}

impl DrainSummary {
    /// 到期条数守恒：每条到期通知要么送达、要么留在队列、要么转人工，
    /// 三者之和必须等于本轮实际处理的条数。
    ///
    /// 传 `due_count`（[`Store::due_notifications`] 的长度）进来断言，
    /// 避免出现「处理了 N 条但账面上看不出去了哪」的静默丢失。
    pub fn is_balanced(&self, due_count: usize) -> bool {
        self.sent + self.still_pending + self.needs_manual == due_count
    }

    pub fn as_zh(&self) -> String {
        if self.sent == 0 && self.still_pending == 0 && self.needs_manual == 0 {
            return "无待发通知".into();
        }
        format!(
            "送达 {} 条、仍在队列 {} 条、转人工 {} 条",
            self.sent, self.still_pending, self.needs_manual
        )
    }
}

/// **进程重启后续发**：把上次没发出去、且退避已到期的 `pending` 行重发一遍。
///
/// 这是「通知不丢」真正落地的那一步。启动时调一次，托盘常驻时定时调。
/// 传替身协议不算生产对接面——生产路径永远是 [`crate::UreqTransport`]
/// 指向 `WECOM_WEBHOOK_URL` 的真实端点。
pub fn drain_pending(
    store: &StoreHandle,
    config: &WeComConfig,
    transport: &dyn Transport,
    policy: BackoffPolicy,
    now: &str,
) -> Result<DrainSummary> {
    let mut summary = DrainSummary::default();
    let due = store.due_notifications(now)?;
    let due_count = due.len();
    for row in due {
        let payload: Value = serde_json::from_str(&row.payload_json).map_err(|e| {
            FhirError::WecomShape {
                detail: format!("发件箱里第 {} 条的消息体坏了：{e}", row.notify_key),
            }
        })?;
        match attempt_send(store, &row.notify_key, config, transport, &payload, policy, now)? {
            NotifyOutcome::Sent => summary.sent += 1,
            NotifyOutcome::NeedsManual { .. } => summary.needs_manual += 1,
            _ => summary.still_pending += 1,
        }
    }
    debug_assert!(
        summary.is_balanced(due_count),
        "到期 {due_count} 条，账面上只记了 {} 条",
        summary.sent + summary.still_pending + summary.needs_manual
    );
    Ok(summary)
}

/// 底栏「待发 N 条」用的计数（`sent` 不计入）。
pub fn undelivered_summary(store: &StoreHandle) -> Result<(usize, usize)> {
    let rows = store.undelivered_notifications()?;
    let manual = rows.iter().filter(|r| r.needs_manual()).count();
    Ok((rows.len(), manual))
}

/// 人工队列（重试耗尽的通知），供「归档核齐」视图列出。
pub fn manual_queue(store: &StoreHandle) -> Result<Vec<OutboxRow>> {
    Ok(store
        .undelivered_notifications()?
        .into_iter()
        .filter(|r| r.needs_manual())
        .collect())
}

/// 某条通知当前的状态（UI/CLI 查询用）。
pub fn state_of(
    store: &StoreHandle,
    encounter_alias: &str,
    doc_id: &str,
    business_day: &str,
) -> Result<Option<String>> {
    let key = notify_key(encounter_alias, doc_id, business_day);
    Ok(store.notification_state(&key)?)
}

/// 便捷判定：这条今天是否已成功送达。
pub fn sent_today(
    store: &StoreHandle,
    encounter_alias: &str,
    doc_id: &str,
    business_day: &str,
) -> Result<bool> {
    Ok(store.notification_sent_today(encounter_alias, doc_id, business_day)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outcomes_classify_correctly() {
        assert!(NotifyOutcome::Sent.is_terminal());
        assert!(NotifyOutcome::AlreadySent.is_terminal());
        assert!(NotifyOutcome::NeedsManual {
            attempts: 3,
            error: "x".into()
        }
        .is_terminal());
        assert!(!NotifyOutcome::Queued {
            next_attempt_at: Some("t".into()),
            error: "x".into()
        }
        .is_terminal());
    }

    #[test]
    fn the_zh_wording_says_whether_it_was_delivered() {
        // 「已送达」与「未送达」在护士长眼里必须一眼分得开
        assert!(NotifyOutcome::Sent.as_zh().contains("已送达"));
        assert!(NotifyOutcome::AlreadySent.as_zh().contains("未重复"));
        assert!(NotifyOutcome::Queued {
            next_attempt_at: Some("2026-03-03T09:01:00+08:00".into()),
            error: "限流".into()
        }
        .as_zh()
        .contains("未送达"));
        assert!(NotifyOutcome::NeedsManual {
            attempts: 3,
            error: "限流".into()
        }
        .as_zh()
        .contains("转人工"));
    }

    #[test]
    fn an_empty_drain_says_so_instead_of_printing_zeros() {
        let s = DrainSummary::default();
        assert_eq!(s.as_zh(), "无待发通知");
        assert!(s.is_balanced(0));
    }

    #[test]
    fn the_drain_accounting_must_add_up_to_the_due_count() {
        let s = DrainSummary {
            sent: 2,
            still_pending: 1,
            needs_manual: 1,
        };
        assert!(s.is_balanced(4));
        // 少记一条 = 有一条通知去向不明，必须被发现
        assert!(!s.is_balanced(5));
        assert!(!s.is_balanced(3));
    }
}