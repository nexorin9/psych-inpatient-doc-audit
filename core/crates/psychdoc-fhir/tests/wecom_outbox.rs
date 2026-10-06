//! 缺件通知的端到端用例：幂等、重试队列、重启续发、凭据校验。
//!
//! ## 替身纪律
//!
//! 本文件里的 `FakeWeCom` 是**测试数据层**的脱敏协议替身，**不是**产品的
//! 最终对接面。生产路径是 [`psychdoc_fhir::UreqTransport`] 指向
//! `WECOM_WEBHOOK_URL` 的真实企业微信群机器人地址。
//!
//! 之所以起真 socket 而不用内存 mock：请求方法 / 路径 / Content-Type /
//! 请求体形状必须**过一遍真实 HTTP 编解码**才作数。用 mock 断言自己
//! 写进去的东西，等于什么都没验——尤其企业微信对 Content-Type 很挑，
//! 发了 `application/fhir+json` 会被拒。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use psychdoc_fhir::notify::{self, BackoffPolicy, NotifyOutcome};
use psychdoc_fhir::transport::{HttpRequest, Transport, UreqTransport};
use psychdoc_fhir::wecom::{self, MissingLine, NoticeContent, WeComConfig, ENV_WECOM_WEBHOOK_URL};
use psychdoc_fhir::{FhirError, HttpResponse};

use psychdoc_store::notify::{notify_key, state as outbox_state};
use psychdoc_store::Store;

use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 本地企业微信协议替身（只在本 tests/ 内出现）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SeenRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl SeenRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).expect("请求体应为合法 JSON")
    }

    /// 消息体里的 markdown 正文。
    fn markdown(&self) -> String {
        self.json()["markdown"]["content"]
            .as_str()
            .unwrap_or_default()
            .to_string()
    }
}

/// 替身按脚本回放应答：第 N 次调用返回第 N 个应答（用尽后重复最后一个）。
struct FakeWeCom {
    webhook_url: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    calls: Arc<AtomicUsize>,
}

impl FakeWeCom {
    fn start(replies: Vec<(u16, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let seen_c = seen.clone();
        let calls_c = calls.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let seen = seen_c.clone();
                let calls = calls_c.clone();
                let replies = replies.clone();
                std::thread::spawn(move || {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    let Some(req) = read_request(&stream) else { return };
                    let (status, body) = replies
                        .get(n)
                        .cloned()
                        .unwrap_or_else(|| replies.last().cloned().expect("至少一条应答"));
                    let reply = http(status, &body);
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    seen.lock().unwrap().push(req);
                });
            }
        });
        Self {
            webhook_url: format!("http://{addr}/cgi-bin/webhook/send?key=local-test-key"),
            seen,
            calls,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn outbound_calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn config(&self) -> WeComConfig {
        WeComConfig::from_source(|k| {
            (k == ENV_WECOM_WEBHOOK_URL).then(|| self.webhook_url.clone())
        })
        .expect("替身配置应合法")
    }
}

fn read_request(stream: &TcpStream) -> Option<SeenRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut parts = start.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            let (k, v) = (k.trim().to_string(), v.trim().to_string());
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.parse().unwrap_or(0);
            }
            headers.push((k, v));
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some(SeenRequest {
        method,
        path,
        headers,
        body: String::from_utf8_lossy(&body).to_string(),
    })
}

fn http(status: u16, body: &Value) -> Vec<u8> {
    let payload = body.to_string();
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .into_bytes()
}

fn ok_reply() -> (u16, Value) {
    (200, json!({"errcode": 0, "errmsg": "ok"}))
}

fn rate_limited() -> (u16, Value) {
    (200, json!({"errcode": 45009, "errmsg": "rate limit"}))
}

fn gone_from_group() -> (u16, Value) {
    (200, json!({"errcode": 93000, "errmsg": "invalid webhook url"}))
}

/// 一个**不触网**的 transport，用来断言「缺配置时 0 次外发」。
#[derive(Default)]
struct CountingTransport {
    calls: AtomicUsize,
}

impl Transport for CountingTransport {
    fn send(&self, _req: &HttpRequest) -> Result<HttpResponse, FhirError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(HttpResponse {
            status: 200,
            headers: vec![],
            body: ok_reply().1.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn notice(n_lines: usize) -> NoticeContent {
    NoticeContent {
        encounter_alias: "WARD-A-001".into(),
        ward: "精神科三病区".into(),
        business_day: "2026-03-03".into(),
        group_hint: "精神科病区文书群".into(),
        lines: (0..n_lines)
            .map(|i| MissingLine {
                doc_label: ["约束知情同意", "约束记录", "解除约束记录", "住院评估报告"][i % 4].into(),
                code: Some(format!("QC-PSY-0{}", i % 9 + 1)),
                basis: Some("《精神科住院护理常规》第 4.2 条".into()),
            })
            .collect(),
    }
}

fn transport() -> UreqTransport {
    UreqTransport::new(std::time::Duration::from_secs(5))
}

const T0: &str = "2026-03-03T09:00:00+08:00";

// ---------------------------------------------------------------------------
// 1. 幂等：同一天同一件只外发一次
// ---------------------------------------------------------------------------

#[test]
fn the_same_item_on_the_same_business_day_is_sent_exactly_once() {
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    let tr = transport();

    // 常驻进程同一天会因 EMR 增量变更重扫这份住院十几次
    for _ in 0..12 {
        let out = notify::notify_once(
            &store,
            &cfg,
            &tr,
            &notice(3),
            "CONSENT-01",
            BackoffPolicy::immediate(),
            T0,
        )
        .unwrap();
        assert!(out.is_sent() || out == NotifyOutcome::AlreadySent);
    }

    assert_eq!(fake.outbound_calls(), 1, "同键同业务日只应有 1 次外发");
    assert_eq!(store.notification_count().unwrap(), 1);
    assert!(notify::sent_today(&store, "WARD-A-001", "CONSENT-01", "2026-03-03").unwrap());
}

#[test]
fn a_new_business_day_may_send_again() {
    // 跨零点后是新的交班日：护士长期望第二天还欠件时再被提醒一次
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    let tr = transport();

    let mut day1 = notice(1);
    day1.business_day = "2026-03-03".into();
    let out = notify::notify_once(
        &store,
        &cfg,
        &tr,
        &day1,
        "CONSENT-01",
        BackoffPolicy::immediate(),
        "2026-03-03T23:50:00+08:00",
    )
    .unwrap();
    assert!(out.is_sent());

    let mut day2 = notice(1);
    day2.business_day = "2026-03-04".into();
    let out = notify::notify_once(
        &store,
        &cfg,
        &tr,
        &day2,
        "CONSENT-01",
        BackoffPolicy::immediate(),
        "2026-03-04T07:10:00+08:00",
    )
    .unwrap();
    assert!(out.is_sent());
    assert_eq!(fake.outbound_calls(), 2);
    assert_eq!(store.notification_count().unwrap(), 2);
}

#[test]
fn two_missing_items_of_one_encounter_are_two_separate_reminders() {
    // 粒度：护士长逐条勾掉时，一条提醒只该对应一件
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    let tr = transport();

    for doc in ["CONSENT-01", "RECORD-01"] {
        let out = notify::notify_once(
            &store, &cfg, &tr, &notice(2), doc, BackoffPolicy::immediate(), T0,
        )
        .unwrap();
        assert!(out.is_sent(), "{doc} 应送达");
    }
    assert_eq!(fake.outbound_calls(), 2);
    assert_eq!(store.notification_count().unwrap(), 2);
}

// ---------------------------------------------------------------------------
// 2. 消息体规范
// ---------------------------------------------------------------------------

#[test]
fn the_request_carries_the_wecom_robot_shape_over_a_real_socket() {
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    notify::notify_once(
        &store,
        &fake.config(),
        &transport(),
        &notice(2),
        "CONSENT-01",
        BackoffPolicy::immediate(),
        T0,
    )
    .unwrap();

    let reqs = fake.requests();
    assert_eq!(reqs.len(), 1);
    let r = &reqs[0];
    assert_eq!(r.method, "POST");
    assert!(r.path.starts_with("/cgi-bin/webhook/send"));
    // 端点对 Content-Type 很挑：发 FHIR 专用的 application/fhir+json 会被拒
    let ct = r.header("Content-Type").unwrap_or_default();
    assert!(
        ct.starts_with("application/json"),
        "Content-Type 应为 application/json，实际 {ct}"
    );
    let body = r.json();
    assert_eq!(body["msgtype"], "markdown");
    let md = r.markdown();
    assert!(md.contains("WARD-A-001"));
    assert!(md.contains("2026-03-03"));
    assert!(md.contains("约束知情同意"));
}

#[test]
fn a_long_list_is_truncated_and_the_remainder_is_named() {
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    notify::notify_once(
        &store,
        &fake.config(),
        &transport(),
        &notice(30),
        "CONSENT-01",
        BackoffPolicy::immediate(),
        T0,
    )
    .unwrap();

    let md = fake.requests()[0].markdown();
    assert!(md.contains("另有 20 条"), "{md}");
    // 端点侧硬限制：正文不得超过 4096 字节，否则整条被拒
    assert!(md.len() <= wecom::MAX_CONTENT_BYTES, "正文 {} 字节", md.len());
}

#[test]
fn an_oversized_basis_does_not_push_the_body_over_the_limit() {
    // 一条极长的依据若排在前面，不能把后面的缺件条目整条挤掉
    let mut n = notice(4);
    n.lines[0].basis = Some("依".repeat(3000));
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    notify::notify_once(
        &store,
        &fake.config(),
        &transport(),
        &n,
        "CONSENT-01",
        BackoffPolicy::immediate(),
        T0,
    )
    .unwrap();
    let md = fake.requests()[0].markdown();
    assert!(md.len() <= wecom::MAX_CONTENT_BYTES + 64, "实际 {} 字节", md.len());
    assert!(std::str::from_utf8(md.as_bytes()).is_ok());
}

// ---------------------------------------------------------------------------
// 3. 业务码：HTTP 200 ≠ 送达
// ---------------------------------------------------------------------------

#[test]
fn a_two_hundred_with_a_non_zero_business_code_is_treated_as_failure() {
    // 限流 / 机器人不在群里都回 200。只看 HTTP 状态码会把这些记成
    // 「已送达」，缺件提醒就此静默丢失。
    let fake = FakeWeCom::start(vec![rate_limited()]);
    let store = Store::open_in_memory().unwrap();
    let out = notify::notify_once(
        &store,
        &fake.config(),
        &transport(),
        &notice(1),
        "CONSENT-01",
        BackoffPolicy::immediate(),
        T0,
    )
    .unwrap();

    assert!(!out.is_sent(), "errcode 非 0 不得判为已送达：{out:?}");
    assert!(out.as_zh().contains("未送达"), "{}", out.as_zh());

    // 状态仍是 pending，且失败原因可回查
    let key = notify_key("WARD-A-001", "CONSENT-01", "2026-03-03");
    let row = store.notification(&key).unwrap().expect("行应在");
    assert!(row.is_pending());
    assert!(row.attempts == 1);
    assert!(row.last_error.as_deref().unwrap().contains("45009"));
    assert!(row.last_error.as_deref().unwrap().contains("未送达"));
    assert!(!notify::sent_today(&store, "WARD-A-001", "CONSENT-01", "2026-03-03").unwrap());

    // 底栏「待发 N 条」必须看得见这一条：界面上「已发」与「没发」长得一样，
    // 不显式计数就等于把这条提醒静默丢了
    let (undelivered, manual) = notify::undelivered_summary(&store).unwrap();
    assert_eq!(undelivered, 1);
    assert_eq!(manual, 0, "还没到重试耗尽，不该进人工队列");
    assert!(row.as_zh().contains("待发送"), "{}", row.as_zh());
    assert!(row.as_zh().contains("45009"), "失败原因要能在界面上回查");
}

// ---------------------------------------------------------------------------
// 4. 重试队列：失败不丢、重启续发
// ---------------------------------------------------------------------------

#[test]
fn a_failed_notification_stays_in_the_queue_across_a_process_restart() {
    let dir = std::env::temp_dir().join("psychdoc-wecom-restart");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let db = dir.join("psychdoc.sqlite3");

    // 第一次进程：端点限流，发送失败
    {
        let fake = FakeWeCom::start(vec![rate_limited()]);
        let store = Store::open(&db).unwrap();
        let out = notify::notify_once(
            &store,
            &fake.config(),
            &transport(),
            &notice(2),
            "CONSENT-01",
            BackoffPolicy::default(),
            T0,
        )
        .unwrap();
        assert!(!out.is_sent());
        assert_eq!(fake.outbound_calls(), 1);
        assert_eq!(store.notification_count().unwrap(), 1);
    } // 进程在此「退出」，提醒留在库里

    // 第二次进程：同一个库文件，端点恢复正常
    {
        let fake = FakeWeCom::start(vec![ok_reply()]);
        let store = Store::open(&db).unwrap();

        // 重启时不该自动把未到期的行发出去
        let early = notify::drain_pending(
            &store,
            &fake.config(),
            &transport(),
            BackoffPolicy::immediate(),
            T0,
        )
        .unwrap();
        assert_eq!(early.sent, 0, "退避未到期的行不该被提前重发");
        assert_eq!(fake.outbound_calls(), 0, "重启本身不该产生外发");

        // 退避到期后（60 秒后）续发
        let later = notify::drain_pending(
            &store,
            &fake.config(),
            &transport(),
            BackoffPolicy::immediate(),
            "2026-03-03T09:05:00+08:00",
        )
        .unwrap();
        assert_eq!(later.sent, 1, "{later:?}");
        assert_eq!(later.still_pending, 0);
        assert_eq!(later.needs_manual, 0);
        assert_eq!(fake.outbound_calls(), 1);
        assert!(notify::sent_today(&store, "WARD-A-001", "CONSENT-01", "2026-03-03").unwrap());

        // 队列里没有未送达的了
        let (undelivered, manual) = notify::undelivered_summary(&store).unwrap();
        assert_eq!(undelivered, 0);
        assert_eq!(manual, 0);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_retry_sends_the_very_same_payload_that_was_queued() {
    // 重试期间文书若已被补齐，第二条消息不能改口——群里已经有人说
    // 「我这就补」，紧接着一条说「不用补了」比第一条更让人困惑。
    let fake = FakeWeCom::start(vec![rate_limited(), ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    let tr = transport();

    let n1 = notice(2);
    notify::notify_once(
        &store, &cfg, &tr, &n1, "CONSENT-01", BackoffPolicy::immediate(), T0,
    )
    .unwrap();

    // 第二次触发时缺件已经变了（只 1 条）
    let mut n2 = notice(1);
    n2.lines.truncate(1);
    n2.lines[0].doc_label = "补齐后的另一件".into();
    let out = notify::notify_once(
        &store, &cfg, &tr, &n2, "CONSENT-01", BackoffPolicy::immediate(), T0,
    )
    .unwrap();
    assert!(!out.is_sent(), "同键仍在队列中，不该改口重发");

    // 退避到期续发
    notify::drain_pending(
        &store,
        &cfg,
        &tr,
        BackoffPolicy::immediate(),
        "2026-03-03T09:05:00+08:00",
    )
    .unwrap();

    let reqs = fake.requests();
    assert_eq!(reqs.len(), 2);
    assert_eq!(
        reqs[0].markdown(),
        reqs[1].markdown(),
        "重试必须逐字节复用入队时定稿的消息体"
    );
    assert!(reqs[1].markdown().contains("约束知情同意"));
    assert!(!reqs[1].markdown().contains("补齐后的另一件"));
}

#[test]
fn retries_stop_after_the_configured_attempts_and_go_to_a_manual_queue() {
    let fake = FakeWeCom::start(vec![gone_from_group()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    let tr = transport();
    let policy = BackoffPolicy {
        base_seconds: 0,
        max_attempts: 3,
    };

    // 首次触发：入队 + 发送失败（端点回 93000）
    let first = notify::notify_once(
        &store, &cfg, &tr, &notice(1), "CONSENT-01", policy, T0,
    )
    .unwrap();
    assert!(!first.is_sent());

    // 零退避下，余下的重试全部在 due 集合里；每次 drain 只发一条
    let mut outcomes = vec![0usize];
    for _ in 0..2 {
        outcomes.push(
            notify::drain_pending(&store, &cfg, &tr, policy, T0)
                .unwrap()
                .needs_manual,
        );
    }

    assert_eq!(fake.outbound_calls(), 3, "应恰好尝试 3 次");
    assert_eq!(outcomes[2], 1, "第 3 次失败后转人工");
    // 转人工之后不再自动重发
    let summary = notify::drain_pending(&store, &cfg, &tr, policy, T0).unwrap();
    assert_eq!(summary.sent, 0);
    assert_eq!(summary.still_pending, 0);
    assert_eq!(summary.needs_manual, 0);
    assert_eq!(fake.outbound_calls(), 3, "转人工后不该再有第 4 次外发");

    let manual = notify::manual_queue(&store).unwrap();
    assert_eq!(manual.len(), 1);
    assert!(manual[0].needs_manual());
    assert!(manual[0].as_zh().contains("待人工"), "{}", manual[0].as_zh());
    assert_eq!(manual[0].attempts, 3);
    assert!(manual[0].last_error.as_deref().unwrap().contains("93000"));
}

#[test]
fn a_notified_item_that_later_fails_again_does_not_resend_from_scratch() {
    // 已 sent 的行不该被重试逻辑捡回来
    let fake = FakeWeCom::start(vec![ok_reply()]);
    let store = Store::open_in_memory().unwrap();
    let cfg = fake.config();
    notify::notify_once(
        &store, &cfg, &transport(), &notice(1), "CONSENT-01", BackoffPolicy::immediate(), T0,
    )
    .unwrap();

    let summary =
        notify::drain_pending(&store, &cfg, &transport(), BackoffPolicy::immediate(), T0).unwrap();
    assert_eq!(summary.sent, 0);
    assert_eq!(fake.outbound_calls(), 1, "已送达的不该再发");
    let key = notify_key("WARD-A-001", "CONSENT-01", "2026-03-03");
    assert_eq!(
        store.notification_state(&key).unwrap().as_deref(),
        Some(outbox_state::SENT)
    );
}

// ---------------------------------------------------------------------------
// 5. 凭据：缺失时 0 次网络调用
// ---------------------------------------------------------------------------

#[test]
fn a_missing_webhook_env_var_makes_zero_network_calls() {
    // 刻意用计数 transport：物理上不可能外发成功
    let counting = CountingTransport::default();
    let store = Store::open_in_memory().unwrap();

    // 构造配置这一步就该失败（不触网）
    let cfg_err = WeComConfig::from_source(|_| None).unwrap_err();
    assert_eq!(cfg_err.code(), "E_CFG_MISSING");
    assert!(cfg_err.to_string().contains(ENV_WECOM_WEBHOOK_URL));
    assert!(cfg_err.to_string().contains("不会降级"));

    // 空白同样算缺失
    for bad in ["", "  ", "\t"] {
        let err = WeComConfig::from_source(|k| {
            (k == ENV_WECOM_WEBHOOK_URL).then(|| bad.to_string())
        })
        .unwrap_err();
        assert_eq!(err.code(), "E_CFG_MISSING");
    }

    assert_eq!(counting.calls.load(Ordering::SeqCst), 0);
    // 没有通知就不该往发件箱塞行——否则底栏「待发 N 条」永久顶不起来
    assert_eq!(store.notification_count().unwrap(), 0);
}

#[test]
fn a_malformed_webhook_url_is_cfg_invalid_and_still_silent() {
    let counting = CountingTransport::default();
    for bad in [
        "qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc",
        "https://qyapi.weixin.qq.com/cgi-bin/webhook/send",
    ] {
        let err = WeComConfig::from_source(|k| {
            (k == ENV_WECOM_WEBHOOK_URL).then(|| bad.to_string())
        })
        .unwrap_err();
        assert_eq!(err.code(), "E_CFG_INVALID", "{bad} 应报形态错");
        assert!(err.to_string().contains(ENV_WECOM_WEBHOOK_URL));
    }
    assert_eq!(counting.calls.load(Ordering::SeqCst), 0);
}

#[test]
fn an_http_error_is_not_swallowed_into_a_success() {
    let fake = FakeWeCom::start(vec![(500, json!({"errcode": -1, "errmsg": "server"}))]);
    let store = Store::open_in_memory().unwrap();
    let out = notify::notify_once(
        &store,
        &fake.config(),
        &transport(),
        &notice(1),
        "CONSENT-01",
        BackoffPolicy::immediate(),
        T0,
    )
    .unwrap();
    assert!(!out.is_sent());
    let key = notify_key("WARD-A-001", "CONSENT-01", "2026-03-03");
    let row = store.notification(&key).unwrap().unwrap();
    assert!(row.is_pending());
    assert!(row.last_error.as_deref().unwrap().contains("E_WECOM_STATUS"));
}

// ---------------------------------------------------------------------------
// 6. 替身纪律
// ---------------------------------------------------------------------------

#[test]
fn the_fake_server_is_only_reachable_from_tests() {
    // lib 的 pub use 列表里不得出现任何替身类型名——生产代码只能走
    // UreqTransport + 环境变量。
    let lib = include_str!("../src/lib.rs");
    assert!(!lib.contains("FakeWeCom"));
    assert!(!lib.contains("CountingTransport"));
    assert!(!lib.contains("127.0.0.1"));
}