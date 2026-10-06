//! 增量读：`_since` 游标 + 分页遍历 + 429/5xx 有界退避重试。
//!
//! 院内对接里最常见的三种卡点，都在这里处理：
//!
//! | 现场 | 本模块的处理 |
//! |---|---|
//! | 每天全量拉几万条，护士长等不起 | [`sync_since`] 带 `_since` 只拉变更 |
//! | 拉一半崩了，重启不知道从哪续 | 游标只在整批成功落库后推进（`psychdoc_store::cursor`） |
//! | 服务端限流 / 网关抽风 | 429 按 `Retry-After` 退避；5xx 与超时按指数退避有界重试 |
//!
//! ## 三条硬纪律
//!
//! 1. **重试耗尽后必须报错，不能返回空集**。返回空集在界面上是
//!    「今天没有缺件」——和「拉取失败」长得一模一样，而护士长会照着
//!    空结果做交接。宁可红字报错让人重试，也不能让一次网络抖动
//!    变成一次「已核对无缺件」。
//! 2. **`Retry-After` 优先于本地退避策略**。服务端说了等 30 秒就等 30 秒；
//!    自己按 1 秒后重试等于在限流上再顶一脚。
//! 3. **退避有上限**。指数退避不封顶会在服务端长时间故障时把
//!    桌面程序挂到天亮；封顶 30 秒，最多重试 [`RetryPolicy::max_attempts`]
//!    次，耗尽即 [`FhirError::RetriesExhausted`]。
//!
//! ## 时间不靠注入
//!
//! 退避要真睡，但测试不该真睡。[`Sleeper`] 把「睡多久」抽出来，
//! 生产用 [`ThreadSleeper`]，测试用记录式的 [`RecordingSleeper`]
//! ——**替身只在 `tests/` 层出现**，生产代码里没有任何假时钟。

use crate::auth::AccessToken;
use crate::config::EmrConfig;
use crate::error::{FhirError, Result};
use crate::transport::{HttpMethod, HttpRequest, Transport};

use serde_json::Value;

/// 有界重试策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// 单个请求最多尝试几次（含首次）。3 = 首次 + 2 次重试。
    pub max_attempts: u32,
    /// 指数退避首档（毫秒）。
    pub base_delay_ms: u64,
    /// 指数退避封顶（毫秒）。服务端让等更久时以 `Retry-After` 为准。
    pub max_delay_ms: u64,
    /// 愿意照 `Retry-After` 等多久的上限（毫秒）。超过则按封顶退避——
    /// 服务端说「等 1 小时」时，常驻程序应该先收工而不是干等。
    pub max_retry_after_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 500,
            max_delay_ms: 8_000,
            max_retry_after_ms: 30_000,
        }
    }
}

impl RetryPolicy {
    /// 第 `attempt`（从 1 起）次失败后的等待时长，封顶 [`Self::max_delay_ms`]。
    pub fn backoff_for(&self, attempt: u32) -> u64 {
        let exp = self
            .base_delay_ms
            .checked_shl(attempt.saturating_sub(1).min(16))
            .unwrap_or(self.max_delay_ms);
        exp.min(self.max_delay_ms)
    }

    /// 测试用：全部等待压到 0/极小值，不真的睡。
    pub fn instant() -> Self {
        Self {
            max_attempts: 3,
            base_delay_ms: 0,
            max_delay_ms: 0,
            max_retry_after_ms: 0,
        }
    }
}

/// 「睡多久」的可替换点。生产用 [`ThreadSleeper`]。
pub trait Sleeper: Send + Sync {
    fn sleep_ms(&self, ms: u64);
}

/// 生产退避：真的睡。
#[derive(Debug, Clone, Copy, Default)]
pub struct ThreadSleeper;

impl Sleeper for ThreadSleeper {
    fn sleep_ms(&self, ms: u64) {
        if ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(ms));
        }
    }
}

/// 一次有界重试的结果记账。供运行摘要与 README 的「错误与降级」表引用。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RetryLedger {
    /// 实际发出的请求次数（含首次）。
    pub attempts: usize,
    /// 因 429 退避的次数。
    pub rate_limited_hits: usize,
    /// 因 5xx / 网络不可达退避的次数。
    pub server_error_hits: usize,
    /// 累计等待毫秒。
    pub waited_ms: u64,
    /// 按 `Retry-After` 退避的次数（其余为本地指数退避）。
    pub retry_after_honoured: usize,
}

impl RetryLedger {
    pub fn as_zh(&self) -> String {
        let mut s = format!("请求 {} 次", self.attempts);
        if self.rate_limited_hits > 0 {
            s.push_str(&format!("，限流 {} 次", self.rate_limited_hits));
        }
        if self.server_error_hits > 0 {
            s.push_str(&format!("，服务端错误 {} 次", self.server_error_hits));
        }
        if self.waited_ms > 0 {
            s.push_str(&format!("，退避等待 {} ms", self.waited_ms));
        }
        s
    }
}

/// 带 429 / 5xx 有界重试的 GET。
///
/// 429 读 `Retry-After`（秒数或 HTTP-date，取秒数形态）并按其退避；缺该头
/// 或形态不认识时退回本地指数退避。5xx 与网络不可达按指数退避。
/// **耗尽后返回 `Err`**——绝不返回空集。
pub fn get_with_retry(
    config: &EmrConfig,
    token: &AccessToken,
    url: &str,
    transport: &dyn Transport,
    policy: &RetryPolicy,
    sleeper: &dyn Sleeper,
    ledger: &mut RetryLedger,
) -> Result<String> {
    let mut attempt = 0u32;
    let mut last: Option<FhirError> = None;
    while attempt < policy.max_attempts.max(1) {
        attempt += 1;
        ledger.attempts += 1;

        let outcome = attempt_get(config, token, url, transport);

        match outcome {
            Ok(body) => return Ok(body),
            Err((err, retry_after_ms)) => {
                let retryable = err.is_retryable();
                if retryable {
                    if matches!(err, FhirError::Status { status: 429, .. }) {
                        ledger.rate_limited_hits += 1;
                    } else {
                        ledger.server_error_hits += 1;
                    }
                }
                last = Some(err);
                if !retryable || attempt >= policy.max_attempts {
                    break;
                }
                // `Retry-After` 优先；超过愿意等的上限则退回本地封顶退避
                let wait = match retry_after_ms {
                    Some(ms) if ms <= policy.max_retry_after_ms => {
                        ledger.retry_after_honoured += 1;
                        ms
                    }
                    _ => policy.backoff_for(attempt),
                };
                ledger.waited_ms += wait;
                sleeper.sleep_ms(wait);
            }
        }
    }
    Err(retry_exhausted(url, attempt, last))
}

/// 发一次 GET，把响应级信息（状态码、`Retry-After`）一并带回。
fn attempt_get(
    config: &EmrConfig,
    token: &AccessToken,
    url: &str,
    transport: &dyn Transport,
) -> std::result::Result<String, (FhirError, Option<u64>)> {
    let mut req = HttpRequest::new(HttpMethod::Get, url)
        .header("Accept", "application/fhir+json");
    for (name, value) in config.headers(&token.token) {
        req = req.header(&name, value);
    }
    let resp = match transport.send(&req) {
        Ok(r) => r,
        // 网络层失败（连不上、超时）：可重试，但服务端没给 `Retry-After`
        Err(e) => return Err((e, None)),
    };
    if resp.is_success() {
        return Ok(resp.body);
    }
    let retry_after = resp.header("Retry-After").and_then(parse_retry_after_ms);
    Err((
        FhirError::Status {
            method: "GET",
            url: url.to_string(),
            status: resp.status,
            detail: crate::auth::first_line(&resp.body),
        },
        retry_after,
    ))
}

fn retry_exhausted(url: &str, attempts: u32, last: Option<FhirError>) -> FhirError {
    let detail = match last {
        Some(FhirError::Status { status, detail, .. }) => format!("最后一次返回 {status} — {detail}"),
        Some(FhirError::Unreachable { detail, .. }) => format!("最后一次连不上端点 — {detail}"),
        Some(other) => format!("最后一次失败 — {other}"),
        None => "没有发出任何请求".to_string(),
    };
    FhirError::RetriesExhausted {
        url: url.to_string(),
        attempts,
        detail,
    }
}

/// `Retry-After` 的**秒数**形态。HTTP-date 形态不认（缺时钟依赖且院内端点
/// 极少发该形态）；认不出来就退回本地指数退避，不猜。
pub fn parse_retry_after_ms(raw: &str) -> Option<u64> {
    let t = raw.trim();
    if t.is_empty() {
        return None;
    }
    if let Ok(secs) = t.parse::<u64>() {
        return Some(secs.saturating_mul(1000));
    }
    // 有些端点写成 "5.0" 或 "0.5"
    if let Ok(secs) = t.parse::<f64>() {
        if secs.is_finite() && secs >= 0.0 {
            return Some((secs * 1000.0) as u64);
        }
    }
    None
}

/// 增量拉取的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SyncOutcome {
    /// 翻过的页数。
    pub pages: usize,
    /// 本批收到的 entry 总数（去重前）。
    pub entries: usize,
    /// entry 去重后**唯一资源数**（按 `resource.id`）。分页不漏不重的核对基准。
    pub unique_resources: usize,
    /// 本批各资源 `meta.lastUpdated` 的最大值，可直接作下次的 `_since` 下界。
    pub new_since: String,
    /// 起点游标（空串 = 全量）。
    pub resumed_from: String,
    /// 起点 URL，便于复现「这次到底拉了什么」。
    pub start_url: String,
    pub ledger: RetryLedger,
}

impl SyncOutcome {
    pub fn as_zh(&self) -> String {
        let mode = if self.resumed_from.is_empty() {
            "全量拉取".to_string()
        } else {
            format!("增量拉取（自 {}）", self.resumed_from)
        };
        format!(
            "{}：{} 页 / {} 条（去重后 {} 件），下批游标推进到 {}",
            mode,
            self.pages,
            self.entries,
            self.unique_resources,
            if self.new_since.is_empty() {
                "（本批无变更，不推进）"
            } else {
                &self.new_since
            }
        )
    }
}

/// 增量拉取一个资源类型的全部变更。
///
/// 遍历完整且不漏页不重复：由 [`crate::read::MAX_PAGES`] 封顶 + `next` 自环
/// 检测 + entry 按 `resource.id` 去重三道一起守。
pub fn sync_since(
    config: &EmrConfig,
    token: &AccessToken,
    resource: &str,
    since: &str,
    transport: &dyn Transport,
    policy: &RetryPolicy,
    sleeper: &dyn Sleeper,
) -> Result<SyncOutcome> {
    let url = since_url(&config.base_url, resource, since);
    let mut outcome = SyncOutcome {
        resumed_from: since.to_string(),
        start_url: url.clone(),
        ..Default::default()
    };

    let mut next = Some(url);
    let mut seen_pages: Vec<String> = Vec::new();
    let mut seen_entries: Vec<String> = Vec::new();
    let mut latest = String::new();

    while let Some(page_url) = next {
        if outcome.pages >= crate::read::MAX_PAGES {
            break;
        }
        if seen_pages.iter().any(|u| u == &page_url) {
            return Err(FhirError::PagingLoop { url: page_url });
        }
        seen_pages.push(page_url.clone());

        let body = get_with_retry(
            config,
            token,
            &page_url,
            transport,
            policy,
            sleeper,
            &mut outcome.ledger,
        )?;
        let page: Value = serde_json::from_str(&body).map_err(|e| FhirError::Shape {
            detail: format!("searchset 页不是合法 JSON：{e}"),
        })?;
        if page.get("resourceType").and_then(|v| v.as_str()) != Some("Bundle") {
            return Err(FhirError::Shape {
                detail: "searchset 页 resourceType 不是 Bundle".into(),
            });
        }
        if let Some(list) = page.get("entry").and_then(|v| v.as_array()) {
            for e in list {
                outcome.entries += 1;
                let id = entry_identity(e);
                if !id.is_empty() && !seen_entries.contains(&id) {
                    seen_entries.push(id);
                    outcome.unique_resources += 1;
                }
                if let Some(lu) = last_updated(e) {
                    if later_than(&lu, &latest) {
                        latest = lu;
                    }
                }
            }
        }
        outcome.pages += 1;
        next = crate::read::next_link(&page);
    }

    outcome.new_since = latest;
    Ok(outcome)
}

/// 组装带 `_since` 的 search URL。`since` 为空则不带该参数（全量）。
pub fn since_url(base_url: &str, resource: &str, since: &str) -> String {
    let s = since.trim();
    if s.is_empty() {
        return format!("{base_url}/{resource}");
    }
    format!(
        "{base_url}/{resource}?_since={}",
        crate::read::percent_encode_identifier(s)
    )
}

/// 一个 entry 的稳定身份：`resource.id`；缺 id 时退回 `fullUrl`。
fn entry_identity(e: &Value) -> String {
    e.get("resource")
        .and_then(|r| r.get("id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .or_else(|| {
            e.get("fullUrl")
                .and_then(|v| v.as_str())
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// `resource.meta.lastUpdated`。FHIR R4 的 `_since` 就是按它过滤的。
fn last_updated(e: &Value) -> Option<String> {
    e.get("resource")
        .and_then(|r| r.get("meta"))
        .and_then(|m| m.get("lastUpdated"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

fn later_than(candidate: &str, current: &str) -> bool {
    if current.is_empty() {
        return !candidate.is_empty();
    }
    match (parse_moment(candidate), parse_moment(current)) {
        (Some(a), Some(b)) => a > b,
        // 形态不认识时保守取字面较大者：宁可多拉一点，不漏。
        _ => candidate > current,
    }
}

fn parse_moment(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    chrono::DateTime::parse_from_rfc3339(raw.trim()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_then_stops_at_the_cap() {
        let p = RetryPolicy {
            max_attempts: 5,
            base_delay_ms: 500,
            max_delay_ms: 4_000,
            max_retry_after_ms: 30_000,
        };
        assert_eq!(p.backoff_for(1), 500);
        assert_eq!(p.backoff_for(2), 1_000);
        assert_eq!(p.backoff_for(3), 2_000);
        assert_eq!(p.backoff_for(4), 4_000);
        assert_eq!(p.backoff_for(9), 4_000, "封顶后不再增长");
    }

    #[test]
    fn retry_after_is_read_as_seconds() {
        assert_eq!(parse_retry_after_ms("30"), Some(30_000));
        assert_eq!(parse_retry_after_ms(" 5 "), Some(5_000));
        assert_eq!(parse_retry_after_ms("0"), Some(0));
        assert_eq!(parse_retry_after_ms("0.5"), Some(500));
    }

    #[test]
    fn an_unknown_retry_after_shape_falls_back_to_local_backoff() {
        // HTTP-date 形态不猜：猜错就是「该等 30 秒时只等了 0.5 秒」。
        assert_eq!(parse_retry_after_ms("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after_ms(""), None);
        assert_eq!(parse_retry_after_ms("-5"), None);
    }

    #[test]
    fn since_url_omits_the_parameter_on_a_full_pull() {
        assert_eq!(
            since_url("https://emr.example/fhir/r4", "Composition", ""),
            "https://emr.example/fhir/r4/Composition"
        );
        assert_eq!(
            since_url("https://emr.example/fhir/r4", "Composition", "2026-03-02T23:40:00+08:00"),
            "https://emr.example/fhir/r4/Composition?_since=2026-03-02T23%3A40%3A00%2B08%3A00"
        );
    }

    #[test]
    fn entries_are_identified_by_resource_id_then_full_url() {
        let e = serde_json::json!({"resource": {"id": "WARD-A-001-CONSENT-01"}});
        assert_eq!(entry_identity(&e), "WARD-A-001-CONSENT-01");
        let no_id = serde_json::json!({"fullUrl": "https://emr/fhir/Composition/x"});
        assert_eq!(entry_identity(&no_id), "https://emr/fhir/Composition/x");
        assert_eq!(entry_identity(&serde_json::json!({})), "");
    }

    #[test]
    fn the_new_cursor_is_the_latest_last_updated_not_the_last_one_seen() {
        // 分页顺序不等于时间顺序：第 2 页可能含更早的件。
        assert!(later_than(
            "2026-03-02T23:50:00+08:00",
            "2026-03-02T23:40:00+08:00"
        ));
        assert!(!later_than(
            "2026-03-02T23:30:00+08:00",
            "2026-03-02T23:40:00+08:00"
        ));
        // 同一刻的两种偏移写法不算「更晚」
        assert!(!later_than(
            "2026-03-02T15:40:00Z",
            "2026-03-02T23:40:00+08:00"
        ));
        assert!(later_than("2026-03-02T23:40:00+08:00", ""));
    }

    #[test]
    fn the_run_summary_says_whether_the_cursor_moves() {
        let o = SyncOutcome {
            pages: 3,
            entries: 12,
            unique_resources: 11,
            new_since: String::new(),
            resumed_from: "2026-03-02T23:40:00+08:00".into(),
            start_url: "u".into(),
            ledger: RetryLedger::default(),
        };
        assert!(o.as_zh().contains("增量拉取"));
        assert!(o.as_zh().contains("不推进"));
    }
}