//! 协议级测试：增量读 + 分页 + 限流退避 + 断点续拉。
//!
//! ## 替身纪律
//!
//! 本文件的 [`ScriptedFhir`] 是**测试数据层**的脱敏协议替身，不是产品的最终
//! 对接面。生产路径是 `UreqTransport` 指向 `EMR_BASE_URL` 的真实院内端点。
//!
//! 与 `fhir_protocol.rs` 的差别在于：这里要**按脚本**应答（先 429 再 200、
//! 先 500 再 500 再 200），所以替身维护一个「待应答脚本队列」，按到达顺序
//! 出队。请求仍然过一遍**真实 socket 与真实 HTTP 编解码**——
//! `Retry-After` 头能不能真的读到、状态码会不会被 ureq 吞掉，都得这么验。
//!
//! **游标替身不是替身**：断点续拉那一节用的是**真的 sqlite 库**
//! （`psychdoc_store::Store::open_in_memory`），只有端点是替身。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use psychdoc_fhir::{
    get_with_retry, since_url, sync_since, AccessToken, EmrConfig, FhirError, RetryLedger,
    RetryPolicy, Sleeper, UreqTransport,
};
use psychdoc_store::{CursorKey, Store};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 脚本化 FHIR 协议替身（只在本 tests/ 内出现）
// ---------------------------------------------------------------------------

/// 一次脚本应答。
#[derive(Clone)]
enum Reply {
    /// 正常 searchset 页。
    Page(Value),
    /// 限流，带 `Retry-After`（秒）。
    Throttled { retry_after_secs: Option<&'static str> },
    /// 瞬时服务端错误。
    ServerError(u16),
}

#[derive(Debug, Clone)]
struct SeenRequest {
    path: String,
    headers: Vec<(String, String)>,
}

impl SeenRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// URL 里的查询参数值（已按 `%XX` 解码）。
    fn query_param(&self, key: &str) -> Option<String> {
        let q = self.path.split_once('?')?.1;
        q.split('&')
            .find_map(|kv| kv.split_once('=').filter(|(k, _)| *k == key).map(|(_, v)| decode(v)))
    }
}

fn decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&raw[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

struct ScriptedFhir {
    base_url: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    script: Arc<Mutex<Vec<Reply>>>,
    calls: Arc<AtomicUsize>,
}

impl ScriptedFhir {
    /// `script` 按到达顺序出队；出队���了还有请求就返回空 searchset
    /// （等价于「服务端没东西可给」，是合法应答而非失败）。
    fn start(script: Vec<Reply>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(script));
        let calls = Arc::new(AtomicUsize::new(0));

        let (seen_c, script_c, calls_c) = (seen.clone(), script.clone(), calls.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (seen, script, calls) = (seen_c.clone(), script_c.clone(), calls_c.clone());
                std::thread::spawn(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let Some(req) = read_request(&stream) else { return };
                    seen.lock().unwrap().push(req);
                    // 脚本按到达顺序出队；走空了就回落空 searchset
                    // （= 服务端没东西可给，合法应答而非失败）
                    let reply = {
                        let mut q = script.lock().unwrap();
                        match q.first().cloned() {
                            Some(r) => {
                                q.remove(0);
                                r
                            }
                            None => Reply::Page(empty_page()),
                        }
                    };
                    let _ = stream.write_all(&render(reply));
                    let _ = stream.flush();
                });
            }
        });

        Self {
            base_url: format!("http://{addr}/fhir/r4"),
            seen,
            script,
            calls,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// 到达替身的请求总数（含被拒后重试的那几次）。
    fn hits(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn remaining_script(&self) -> usize {
        self.script.lock().unwrap().len()
    }

    /// 装入应答脚本（`next` 链接要引用 base_url，所以脚本可以后填）。
    fn set_script(&self, script: Vec<Reply>) {
        *self.script.lock().unwrap() = script;
    }

    fn config(&self) -> EmrConfig {
        EmrConfig::from_source(|k| match k {
            "EMR_BASE_URL" => Some(self.base_url.clone()),
            "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
            "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
            "EMR_TENANT_ID" => Some("ward-a".into()),
            _ => None,
        })
        .unwrap()
    }
}

fn render(reply: Reply) -> Vec<u8> {
    match reply {
        Reply::Page(v) => raw_http(200, "OK", &[], &v.to_string()),
        Reply::ServerError(code) => raw_http(
            code,
            "Service Unavailable",
            &[],
            &json!({"resourceType": "OperationOutcome", "message": "网关忙"}).to_string(),
        ),
        Reply::Throttled { retry_after_secs } => {
            let mut headers: Vec<(&str, String)> = vec![
                ("Content-Type", "application/fhir+json".into()),
                ("Retry-After", retry_after_secs.unwrap_or("2").to_string()),
            ];
            if retry_after_secs.is_none() {
                // 刻意不发该头，逼调用方退回本地指数退避
                headers.retain(|(k, _)| *k != "Retry-After");
            }
            raw_http(
                429,
                "Too Many Requests",
                &headers,
                &json!({"message": "请求过于频繁"}).to_string(),
            )
        }
    }
}

fn raw_http(status: u16, reason: &str, headers: &[(&str, String)], body: &str) -> Vec<u8> {
    let mut head = format!("HTTP/1.1 {status} {reason}\r\n");
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str(&format!(
        "Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    ));
    format!("{head}{body}").into_bytes()
}

fn read_request(stream: &TcpStream) -> Option<SeenRequest> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut parts = start.split_whitespace();
    let _method = parts.next()?;
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
    Some(SeenRequest { path, headers })
}

fn empty_page() -> Value {
    json!({"resourceType": "Bundle", "type": "searchset", "link": [], "entry": []})
}

/// 一页 searchset。`base` 是拼 `next` 链接用的绝对前缀。
fn page(base: &str, entries: Vec<Value>, next: Option<usize>, page_no: usize) -> Value {
    let mut link = vec![json!({"relation": "self", "url": format!("{base}/p{page_no}")})];
    if let Some(n) = next {
        link.push(json!({"relation": "next", "url": format!("{base}/p{n}")}));
    }
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "link": link,
        "entry": entries,
    })
}

fn composition(id: &str, last_updated: &str) -> Value {
    json!({"resource": {
        "resourceType": "Composition",
        "id": id,
        "meta": {"lastUpdated": last_updated},
        "status": "in-progress",
        "date": last_updated,
        "identifier": [{"system": "https://emr.example/encounter", "value": "WARD-A-001"}],
        "type": {"coding": [{"code": "TPL-NURSING-RECORD", "display": "护理记录"}]},
        "section": [{"title": "记录", "text": "脱敏样例记录"}]
    }})
}

/// 本组测试共用的游标坐标：ward-a 租户的 Composition 端点。
fn ward_a_composition() -> CursorKey {
    CursorKey::new("ward-a", "Composition")
}

fn token() -> AccessToken {
    AccessToken {
        token: "tok-local".into(),
        expires_in_secs: 600,
    }
}

// ---------------------------------------------------------------------------
// 测试替身：记录式 sleeper（只在本 tests/ 内出现）
// ---------------------------------------------------------------------------

#[derive(Default)]
struct RecordingSleeper {
    waits: Arc<Mutex<Vec<u64>>>,
}

impl RecordingSleeper {
    fn new() -> Self {
        Self::default()
    }
    fn waits(&self) -> Vec<u64> {
        self.waits.lock().unwrap().clone()
    }
}

impl Sleeper for RecordingSleeper {
    fn sleep_ms(&self, ms: u64) {
        self.waits.lock().unwrap().push(ms);
        // 真的不睡：退避时长会被断言，但不该让测试套件真的等 30 秒。
    }
}

// ===========================================================================
// 1) _since 增量读
// ===========================================================================

#[test]
fn an_incremental_pull_carries_the_since_parameter_on_the_wire() {
    let f = ScriptedFhir::start(vec![Reply::Page(empty_page())]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    let body = get_with_retry(
        &cfg,
        &token(),
        &since_url(&cfg.base_url, "Composition", "2026-03-02T23:40:00+08:00"),
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &sleeper,
        &mut ledger,
    )
    .expect("应拉到一页");

    assert!(body.contains("searchset"));
    let req = f.requests().remove(0);
    assert_eq!(
        req.query_param("_since").as_deref(),
        Some("2026-03-02T23:40:00+08:00"),
        "`_since` 里的 `+` 必须百分号编码，否则 `+` 会被解成空格"
    );
    assert_eq!(req.header("X-Tenant-Id"), Some("ward-a"));
    assert_eq!(req.header("Authorization"), Some("Bearer tok-local"));
    assert_eq!(req.header("Accept"), Some("application/fhir+json"));
}

#[test]
fn a_first_pull_without_a_cursor_omits_since_entirely() {
    let f = ScriptedFhir::start(vec![Reply::Page(empty_page())]);
    let cfg = f.config();
    let mut ledger = RetryLedger::default();
    get_with_retry(
        &cfg,
        &token(),
        &since_url(&cfg.base_url, "Composition", ""),
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &RecordingSleeper::new(),
        &mut ledger,
    )
    .unwrap();
    let req = f.requests().remove(0);
    assert!(!req.path.contains("_since"), "首次全量不该带空 _since");
}

// ===========================================================================
// 2) 分页遍历
// ===========================================================================

#[test]
fn paging_walks_every_page_and_neither_drops_nor_repeats_an_entry() {
    let f = ScriptedFhir::start(vec![]);
    let base = format!("{}/Composition", f.base_url);
    // 3 页；第 2 页刻意含一个第 1 页已给过的件（服务端在翻页期间改了数据）
    f.set_script(vec![
        Reply::Page(page(
            &base,
            vec![composition("WARD-A-001-RECORD-01", "2026-03-03T09:00:00+08:00")],
            Some(2),
            1,
        )),
        Reply::Page(page(
            &base,
            vec![
                composition("WARD-A-001-RECORD-01", "2026-03-03T09:00:00+08:00"),
                composition("WARD-A-001-RECORD-02", "2026-03-03T09:05:00+08:00"),
            ],
            Some(3),
            2,
        )),
        Reply::Page(page(
            &base,
            vec![
                composition("WARD-A-001-RECORD-03", "2026-03-03T08:30:00+08:00"),
                composition("WARD-A-001-CONSENT-01", "2026-03-03T09:20:00+08:00"),
            ],
            None,
            3,
        )),
    ]);
    let cfg = f.config();

    let out = sync_since(
        &cfg,
        &token(),
        "Composition",
        "2026-03-03T08:00:00+08:00",
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &RecordingSleeper::new(),
    )
    .expect("应走完分页");

    assert_eq!(out.pages, 3, "三页都要走到");
    assert_eq!(out.entries, 5, "服务端给了 5 条 entry");
    assert_eq!(out.unique_resources, 4, "去重后 4 件：重复的那件只算一次");
    assert_eq!(
        out.new_since, "2026-03-03T09:20:00+08:00",
        "新游标取各件 lastUpdated 的最大值，不是最后看到的那一件"
    );
    assert_eq!(f.remaining_script(), 0, "脚本应刚好走完");
}

#[test]
fn a_server_that_points_next_back_at_itself_is_stopped_not_looped() {
    let f = ScriptedFhir::start(vec![]);
    let start = format!("{}/Composition", f.base_url);
    // 服务端把 next 指回本页 —— 自环
    f.set_script(vec![Reply::Page(json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "link": [{"relation": "next", "url": start}],
        "entry": [composition("WARD-A-001-RECORD-01", "2026-03-03T09:00:00+08:00")]
    }))]);
    let cfg = f.config();

    let err = psychdoc_fhir::read::read_all_pages(
        &cfg,
        &token(),
        &start,
        &UreqTransport::default(),
    )
    .expect_err("自环必须被中止而不是空转到 MAX_PAGES");

    assert!(matches!(err, FhirError::PagingLoop { .. }));
    assert_eq!(err.code(), "E_FHIR_PAGING");
    assert_eq!(
        f.requests().len(),
        1,
        "自环在第二次请求**发出之前**就被识破，不该白跑一趟"
    );
}

// ===========================================================================
// 3) 429 限流退避
// ===========================================================================

#[test]
fn a_429_is_backed_off_by_the_retry_after_header_the_server_sent() {
    let f = ScriptedFhir::start(vec![
        Reply::Throttled {
            retry_after_secs: Some("7"),
        },
        Reply::Throttled {
            retry_after_secs: Some("3"),
        },
        Reply::Page(empty_page()),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect("第 3 次应成功");

    assert_eq!(
        sleeper.waits(),
        vec![7_000, 3_000],
        "退避时长必须**按服务端说的**来，不是本地指数档"
    );
    assert_eq!(ledger.rate_limited_hits, 2);
    assert_eq!(ledger.retry_after_honoured, 2);
    assert_eq!(ledger.attempts, 3);
    assert_eq!(ledger.waited_ms, 10_000);
    assert_eq!(f.hits(), 3, "三次请求都真的过了 socket");
    assert_eq!(f.requests().len(), 3);
}

#[test]
fn a_429_without_a_retry_after_header_falls_back_to_capped_exponential_backoff() {
    let f = ScriptedFhir::start(vec![
        Reply::Throttled {
            retry_after_secs: None,
        },
        Reply::Throttled {
            retry_after_secs: None,
        },
        Reply::Page(empty_page()),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect("第 3 次应成功");

    assert_eq!(
        sleeper.waits(),
        vec![500, 1_000],
        "无 `Retry-After` 时按 500ms / 1s 指数退避"
    );
    assert_eq!(ledger.rate_limited_hits, 2);
    assert_eq!(ledger.retry_after_honoured, 0);
}

#[test]
fn a_retry_after_far_beyond_the_cap_is_not_obeyed_forever() {
    let f = ScriptedFhir::start(vec![
        Reply::Throttled {
            retry_after_secs: Some("3600"),
        },
        Reply::Page(empty_page()),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect("第 2 次应成功");

    assert_eq!(
        sleeper.waits(),
        vec![500],
        "服务端说等 1 小时时不该干等；退回本地首档退避"
    );
    assert_eq!(ledger.retry_after_honoured, 0);
}

// ===========================================================================
// 4) 5xx 与不可达的有界重试
// ===========================================================================

#[test]
fn transient_5xx_is_retried_three_times_then_gives_up_loudly() {
    let f = ScriptedFhir::start(vec![
        Reply::ServerError(502),
        Reply::ServerError(503),
        Reply::ServerError(500),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    let err = get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect_err("重试耗尽必须报错，绝不能返回空集");

    assert_eq!(err.code(), "E_FHIR_RETRIES_EXHAUSTED");
    let msg = err.to_string();
    assert!(msg.contains("3 次"), "{msg}");
    assert!(msg.contains("游标不推进"), "{msg}");
    assert!(
        msg.contains("不会当作「没有变更」"),
        "空集与失败在界面上长得一样，必须说清：{msg}"
    );
    assert_eq!(ledger.attempts, 3, "最多 3 次尝试");
    assert_eq!(ledger.server_error_hits, 3);
    assert_eq!(sleeper.waits(), vec![500, 1_000], "两次退避，不在最后一次失败后空等");
    assert_eq!(f.requests().len(), 3);
}

#[test]
fn a_5xx_that_clears_midway_still_succeeds_and_is_visible_in_the_summary() {
    let f = ScriptedFhir::start(vec![
        Reply::ServerError(503),
        Reply::Page(page("", vec![composition("WARD-A-001-RECORD-01", "2026-03-03T09:00:00+08:00")], None, 1)),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    let body = get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect("第 2 次应成功");

    assert!(body.contains("WARD-A-001-RECORD-01"));
    assert_eq!(ledger.server_error_hits, 1);
    assert_eq!(ledger.attempts, 2);
    assert!(ledger.as_zh().contains("服务端错误 1 次"), "{}", ledger.as_zh());
}

#[test]
fn a_404_is_not_retried_at_all() {
    // 脚本给足 3 条 404：若实现误把它当可重试，第 2 次仍会拿到 404，
    // 若实现正确，第 1 次就该收手，剩下两条根本用不上。
    let f = ScriptedFhir::start(vec![
        Reply::ServerError(404),
        Reply::ServerError(404),
        Reply::ServerError(404),
    ]);
    let cfg = f.config();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    let err = get_with_retry(
        &cfg,
        &token(),
        &format!("{}/Composition", f.base_url),
        &UreqTransport::default(),
        &RetryPolicy::default(),
        &sleeper,
        &mut ledger,
    )
    .expect_err("404 不可重试");

    assert_eq!(err.code(), "E_FHIR_RETRIES_EXHAUSTED");
    assert!(err.to_string().contains("404"), "最后一次失败的原因要带出来");
    assert_eq!(ledger.attempts, 1, "404 重试没有意义，一次就够");
    assert!(sleeper.waits().is_empty(), "一次都不该等");
    assert_eq!(f.requests().len(), 1, "只该发出一次请求");
    assert_eq!(f.remaining_script(), 2, "剩下两条应答根本没被消费");
}

#[test]
fn an_unreachable_endpoint_is_retried_and_then_reported_as_exhausted() {
    // 端口 1 上不会有服务在听
    let cfg = EmrConfig::from_source(|k| match k {
        "EMR_BASE_URL" => Some("http://127.0.0.1:1/fhir/r4".to_string()),
        "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
        "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
        "EMR_TENANT_ID" => Some("ward-a".into()),
        _ => None,
    })
    .unwrap();
    let sleeper = RecordingSleeper::new();
    let mut ledger = RetryLedger::default();

    let err = get_with_retry(
        &cfg,
        &token(),
        "http://127.0.0.1:1/fhir/r4/Composition",
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &sleeper,
        &mut ledger,
    )
    .expect_err("连不上必须报错");

    assert_eq!(err.code(), "E_FHIR_RETRIES_EXHAUSTED");
    assert!(err.to_string().contains("连不上端点"));
    assert_eq!(ledger.attempts, 3);
    assert_eq!(ledger.server_error_hits, 3);
}

// ===========================================================================
// 5) 断点续拉：游标只在整批成功后推进
// ===========================================================================

#[test]
fn a_cursor_is_written_and_the_next_pull_starts_exactly_where_it_stopped() {
    let store = Store::open_in_memory().unwrap();
    assert!(store.load_cursor("ward-a", "Composition").unwrap().is_none());

    store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T09:20:00+08:00",
            "2026-03-03T09:21:00+08:00",
            3,
            Some("RUN-1"),
        )
        .unwrap();

    let cur = store.load_cursor("ward-a", "Composition").unwrap().unwrap();
    assert!(cur.is_incremental());
    assert_eq!(cur.since, "2026-03-03T09:20:00+08:00");
    assert_eq!(cur.pages_done, 3);
    assert_eq!(cur.run_id.as_deref(), Some("RUN-1"));
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T09:20:00+08:00"
    );
}

#[test]
fn a_second_sync_starts_from_the_persisted_cursor_and_lands_on_the_new_one() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T09:00:00+08:00",
            "2026-03-03T09:01:00+08:00",
            1,
            None,
        )
        .unwrap();

    let f = ScriptedFhir::start(vec![Reply::Page(empty_page())]);
    let cfg = f.config();

    let since = store.current_since("ward-a", "Composition").unwrap();
    assert_eq!(since, "2026-03-03T09:00:00+08:00");

    let out = sync_since(
        &cfg,
        &token(),
        "Composition",
        &since,
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &RecordingSleeper::new(),
    )
    .expect("应拉到");

    assert!(out.start_url.contains("_since=2026-03-03T09%3A00%3A00"));
    let req = f.requests().remove(0);
    assert_eq!(
        req.query_param("_since").as_deref(),
        Some("2026-03-03T09:00:00+08:00"),
        "续拉必须从落库的游标起，不是从进程启动时刻起"
    );

    // 本批没有任何变更 → 游标**不推进**（推进到空串等于下次又全量拉）
    assert_eq!(out.new_since, "");
    store
        .advance_cursor(&ward_a_composition(), &out.new_since, "2026-03-03T10:00:00+08:00", 1, None)
        .ok();
}

#[test]
fn a_cursor_never_moves_backwards_even_when_the_server_suggests_it() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T09:20:00+08:00",
            "2026-03-03T09:21:00+08:00",
            3,
            None,
        )
        .unwrap();

    let err = store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T09:00:00+08:00",
            "2026-03-03T09:30:00+08:00",
            1,
            None,
        )
        .expect_err("游标回退必须被拒");

    assert_eq!(err.code(), "E_CURSOR_REWIND");
    assert!(err.to_string().contains("不允许回退"));
    // 被拒后游标原地不动
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T09:20:00+08:00"
    );
}

#[test]
fn one_cursor_per_tenant_so_two_wards_never_share_a_watermark() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(&ward_a_composition(), "2026-03-03T09:00:00+08:00", "t", 1, None)
        .unwrap();
    store
        .advance_cursor(
            &psychdoc_store::CursorKey::new("ward-b", "Composition"),
            "2026-03-02T23:00:00+08:00",
            "t",
            1,
            None,
        )
        .unwrap();

    assert_eq!(store.cursor_count().unwrap(), 2);
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T09:00:00+08:00"
    );
    assert_eq!(
        store.current_since("ward-b", "Composition").unwrap(),
        "2026-03-02T23:00:00+08:00",
        "B 病区的游标不能被 A 病区推进"
    );
}

#[test]
fn committing_items_and_moving_the_cursor_are_one_transaction() {
    use psychdoc_model::DocItem;

    let store = Store::open_in_memory().unwrap();
    let mut item = DocItem::new("WARD-A-001-RECORD-01", "TPL-NURSING-RECORD", "WARD-A-001");
    item.source_system = "emr".into();

    store
        .commit_page(
            &ward_a_composition(),
            std::slice::from_ref(&item),
            "2026-03-03T09:20:00+08:00",
            "2026-03-03T09:21:00+08:00",
            2,
            Some("RUN-2"),
        )
        .unwrap();

    assert_eq!(store.item_count().unwrap(), 1, "件要落库");
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T09:20:00+08:00",
        "游标要一起推进"
    );

    // 回退被拒 → 整个事务回滚，件也不该多落
    let before = store.item_count().unwrap();
    item.doc_id = "WARD-A-001-RECORD-02".into();
    let err = store
        .commit_page(
            &ward_a_composition(),
            std::slice::from_ref(&item),
            "2026-03-03T09:00:00+08:00",
            "2026-03-03T09:40:00+08:00",
            3,
            None,
        )
        .unwrap_err();
    assert_eq!(err.code(), "E_CURSOR_REWIND");
    assert_eq!(
        store.item_count().unwrap(),
        before,
        "游标推进被拒时同事务里的件也不能落下——否则会留下没进过审查链的件"
    );
}

#[test]
fn the_same_instant_written_in_another_offset_is_not_treated_as_a_rewind() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T09:20:00+08:00",
            "t",
            1,
            None,
        )
        .unwrap();
    // 同一绝对时刻的 Z 写法
    store
        .advance_cursor(
            &ward_a_composition(),
            "2026-03-03T01:20:00Z",
            "t",
            1,
            None,
        )
        .expect("同一刻的另一种写法不该被判成回退");
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T01:20:00Z"
    );
}

#[test]
fn resetting_a_cursor_goes_back_to_a_full_pull_and_leaves_a_trace() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(&ward_a_composition(), "2026-03-03T09:20:00+08:00", "t", 3, None)
        .unwrap();
    store.reset_cursor("ward-a", "Composition", "2026-03-04T08:00:00+08:00").unwrap();

    let cur = store.load_cursor("ward-a", "Composition").unwrap().unwrap();
    assert!(!cur.is_incremental(), "重置后回到全量");
    assert_eq!(cur.run_id.as_deref(), Some("reset"), "重置须留痕");
    assert_eq!(cur.pages_done, 0);
    assert_eq!(store.cursor_count().unwrap(), 1, "重置不是删除");
}

#[test]
fn a_failed_pull_leaves_the_cursor_exactly_where_it_was() {
    let store = Store::open_in_memory().unwrap();
    store
        .advance_cursor(&ward_a_composition(), "2026-03-03T09:00:00+08:00", "t", 1, None)
        .unwrap();

    let f = ScriptedFhir::start(vec![
        Reply::ServerError(500),
        Reply::ServerError(500),
        Reply::ServerError(500),
    ]);
    let cfg = f.config();
    let since = store.current_since("ward-a", "Composition").unwrap();

    let err = sync_since(
        &cfg,
        &token(),
        "Composition",
        &since,
        &UreqTransport::default(),
        &RetryPolicy::instant(),
        &RecordingSleeper::new(),
    )
    .expect_err("三连 5xx 必须报错");

    assert_eq!(err.code(), "E_FHIR_RETRIES_EXHAUSTED");
    assert_eq!(
        store.current_since("ward-a", "Composition").unwrap(),
        "2026-03-03T09:00:00+08:00",
        "拉取失败时游标原地不动，下次从同一点重来"
    );
    assert_eq!(store.cursor_count().unwrap(), 1);
}

// ===========================================================================
// 6) 替身纪律
// ===========================================================================

#[test]
fn the_scripted_double_is_only_reachable_from_tests() {
    // 替身类型定义在 tests/ 内；lib.rs 的 pub use 里没有任何替身名字。
    let f = ScriptedFhir::start(vec![]);
    assert!(f.base_url.starts_with("http://127.0.0.1:"));
    // 记录式 sleeper 同理：产品默认路径是 ThreadSleeper（真睡）
    let real = psychdoc_fhir::ThreadSleeper;
    real.sleep_ms(0); // 0 毫秒不睡，只为证明它在产品侧可用
}