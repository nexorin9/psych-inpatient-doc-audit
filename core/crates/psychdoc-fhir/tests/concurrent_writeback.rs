//! Task 28 的**协议级**验收：并发确认时外发恰好一次。
//!
//! `psychdoc-store/tests/writeback_concurrency.rs` 证的是「占键这一层」；
//! 这一组证的是**真的过了 socket 的 Bundle transaction 恰好一份**——
//! 那才是护士长在院内端点上看到的东西。
//!
//! 打在**本地 FHIR 协议替身**上，走真实 socket 与真实 `UreqTransport`。
//! 替身只存在于本 `tests/`，**不是**产品默认对接面——生产路径是
//! `UreqTransport` 指向 `EMR_BASE_URL` 的真实院内端点。
//!
//! 四个用例：
//!
//! 1. 50 线程 × 同键并发 → 外发恰好 1 次，冲突响应可读（带首次操作人）；
//! 2. 同键重试 → 0 次外发，回放首次结果，`replayed=true`；
//! 3. 外发失败 → 键放开，重试能再发（网络抖动不锁死）；
//! 4. 上次结果未知 → 拒绝重发，0 次外发。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use psychdoc_fhir::{confirm, AccessToken, EmrConfig, UreqTransport};
use psychdoc_model::state::DocState;
use psychdoc_model::{DocItem, Field, Section, Span};
use psychdoc_store::{Store, WritebackEntry};
use serde_json::{json, Value};

const ALIAS: &str = "WARD-A-001";
const CONSENT: &str = "WARD-A-001-CONSENT-01";
const SHA: &str = "e4eda0c79c95aa11bb22cc33dd44ee9911a1b2c3d4e5f60718293a4b5c6d7e8f9";
const AT: &str = "2026-03-03T08:10:00+08:00";

/// 替身模式：整批成功 / 端点整体 500。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    AllOk,
    ServerError,
}

/// 极简 FHIR 协议替身：只统计 transaction 的次数，不解析业务内容。
/// 业务形状由 `fhir_protocol.rs` 那组用例守，这里只管「发了几次」。
struct FakeFhir {
    base_url: String,
    transactions: Arc<AtomicUsize>,
    mode: Mode,
}

impl FakeFhir {
    fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let transactions = Arc::new(AtomicUsize::new(0));
        let t = transactions.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let t = t.clone();
                std::thread::spawn(move || {
                    let Some(req) = read_request(&stream) else { return };
                    if req.0 == "POST" && req.1 == "/fhir/r4" {
                        t.fetch_add(1, Ordering::SeqCst);
                    }
                    let body = if mode == Mode::ServerError {
                        http(500, "Internal Server Error", &json!({"message": "服务端内部错误，事务未提交"}))
                    } else if req.0 == "POST" && req.1 == "/fhir/r4" {
                        transaction_response()
                    } else if req.1.starts_with("/fhir/r4/oauth2/token") {
                        http(200, "OK", &json!({"access_token": "tok-local", "expires_in": 600, "token_type": "Bearer"}))
                    } else {
                        http(404, "Not Found", &json!({"message": "未知路径"}))
                    };
                    let _ = stream.write_all(&body);
                    let _ = stream.flush();
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/fhir/r4"),
            transactions,
            mode,
        }
    }

    fn outbound_transactions(&self) -> usize {
        self.transactions.load(Ordering::SeqCst)
    }

    #[allow(dead_code)]
    fn mode(&self) -> Mode {
        self.mode
    }

    fn config(&self) -> EmrConfig {
        EmrConfig::from_source(|k| match k {
            "EMR_BASE_URL" => Some(self.base_url.clone()),
            "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
            "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
            "EMR_TENANT_ID" => Some("ward-a".into()),
            _ => None,
        })
        .expect("替身配置应合法")
    }
}

fn read_request(stream: &TcpStream) -> Option<(String, String)> {
    let mut reader = BufReader::new(stream.try_clone().ok()?);
    let mut start = String::new();
    reader.read_line(&mut start).ok()?;
    let mut parts = start.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).ok()? == 0 {
            break;
        }
        let line = line.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        reader.read_exact(&mut body).ok()?;
    }
    Some((method, path))
}

fn http(status: u16, reason: &str, body: &Value) -> Vec<u8> {
    let payload = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/fhir+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .into_bytes()
}

/// 回一个「N 条 entry 全 201 + 1 条 Provenance」的 transaction-response。
/// 实际条数由外发请求体决定，这里固定 1 件 + 1 条 Provenance（与用例一致）。
fn transaction_response() -> Vec<u8> {
    http(
        200,
        "OK",
        &json!({
            "resourceType": "Bundle",
            "type": "transaction-response",
            "entry": [
                {"response": {"status": "201 Created", "location": "Composition/WARD-A-001-CONSENT-01/_history/1"},
                 "resource": {"resourceType": "Composition", "id": CONSENT}},
                {"response": {"status": "201 Created", "location": "Provenance/PS-1/_history/1"},
                 "resource": {"resourceType": "Provenance", "id": "PS-1"}}
            ]
        }),
    )
}

// ---------------------------------------------------------------------------
// 夹具
// ---------------------------------------------------------------------------

fn item(doc_id: &str) -> DocItem {
    DocItem::new(doc_id, "CONSENT_FORM", ALIAS)
        .with_source_system("emr")
        .with_sections(vec![Section {
            index: 0,
            title: "患方知情同意".into(),
            text: "患方意见：已知晓约束目的与风险".into(),
        }])
        .with_fields(vec![Field {
            key: "患方意见".into(),
            value: Some("已知晓约束目的与风险".into()),
            required: true,
            source_span: Some(Span::new(0, 13).unwrap()),
            section_index: 0,
        }])
}

fn entries() -> Vec<WritebackEntry<'static>> {
    vec![WritebackEntry {
        doc_id: CONSENT,
        content_sha256: SHA,
        run_id: Some("RUN-2026-03-03-01"),
    }]
}

fn token() -> AccessToken {
    AccessToken {
        token: "tok-local".into(),
        expires_in_secs: 600,
    }
}

fn builder() -> impl Fn(&str) -> Option<psychdoc_fhir::WriteEntry> {
    |doc_id: &str| {
        Some(psychdoc_fhir::WriteEntry {
            resource_type: "Composition".into(),
            logical_id: doc_id.to_string(),
            method: psychdoc_fhir::WriteMethod::Post,
            resource: json!({
                "resourceType": "Composition",
                "id": doc_id,
                "status": "in-progress",
                "type": {"coding": [{"code": "CONSENT_FORM"}]}
            }),
        })
    }
}

/// 临时目录（放真实 sqlite 文件库）。
struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let mut dir = std::env::temp_dir();
        dir.push(format!("psychdoc-t28-fhir-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self) -> PathBuf {
        self.0.join("db.sqlite3")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 建一份「件已登记、状态已推进到草稿」的库。
fn store_ready(path: &PathBuf) -> Store {
    let store = Store::open(path).unwrap();
    let it = item(CONSENT);
    store.register_items(std::slice::from_ref(&it), AT).unwrap();
    store.init_state(CONSENT, DocState::Missing, "护士长A", AT).unwrap();
    store
        .advance_state(CONSENT, DocState::Draft, "护士长A", AT)
        .unwrap();
    store
}

// ---------------------------------------------------------------------------
// 用例 1：50 线程同键并发 → 外发恰好 1 次
// ---------------------------------------------------------------------------

#[test]
fn fifty_threads_confirming_the_same_item_send_exactly_one_transaction() {
    // 真实场景：护士长在两台机器上同时点了提交，或网络卡住后客户端
    // 自动重试。**服务端只能收到一份 Bundle transaction**——否则同一件
    // 知情同意在院内端点上会有两份，护士长看到的是「系统重复建了件」。
    let tmp = TempDb::new("race");
    let path = tmp.path();
    store_ready(&path); // 先建 schema
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();

    const THREADS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                let draft = format!("DRAFT-{i}");
                let ctx = confirm::ConfirmContext {
                    encounter_alias: ALIAS.into(),
                    actor: format!("脱敏-护士长{i}"),
                    at: AT.into(),
                    ruleset_sha256: SHA.into(),
                    run_id: Some(format!("RUN-{i}")),
                };
                confirm::create_draft(&store, &draft, &ctx, &entries()).unwrap();
                confirm::set_selected(&store, &draft, CONSENT, true, AT).unwrap();
                barrier.wait();
                let r = confirm::confirm(
                    &store,
                    &cfg,
                    &token(),
                    &draft,
                    &ctx,
                    &builder(),
                    &UreqTransport::default(),
                );
                match r {
                    Ok(out) => {
                        if out.replayed {
                            assert_eq!(out.outbound, 0, "重放必须 0 次外发");
                            assert!(out.replayed_from.is_some(), "重放要带首次操作人与时刻");
                            "replayed"
                        } else {
                            assert_eq!(out.outbound, 1);
                            assert_eq!(out.new_rows(), 1);
                            "committed"
                        }
                    }
                    Err(e) => {
                        // 冲突必须可读：带键、带首次操作人、带时刻
                        match e.code() {
                            "E_WRITEBACK_CONFLICT" => {
                                assert!(e.is_retryable());
                                let text = e.to_string();
                                assert!(text.contains("护士长"), "{text}");
                                assert!(text.contains("占键"), "{text}");
                                "conflict"
                            }
                            "E_FHIR_STATUS" => panic!("端点替身是 AllOk，不该 5xx：{e}"),
                            other => panic!("不该出现的错误码 {other}：{e}"),
                        }
                    }
                }
            })
        })
        .collect();

    let results: Vec<&str> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let committed = results.iter().filter(|r| **r == "committed").count();

    assert_eq!(
        f.outbound_transactions(),
        1,
        "50 个线程抢同一个件，外发必须恰好 1 次（分布：{:?}）",
        tally(&results)
    );
    assert_eq!(committed, 1, "恰好一个真正提交成功");
    assert_eq!(
        results
            .iter()
            .filter(|r| **r == "conflict" || **r == "replayed")
            .count(),
        THREADS - 1
    );

    // 本地：写回 1 行、审计 1 条、状态推进到已建件
    let store = Store::open(&path).unwrap();
    assert_eq!(store.writeback_row_count().unwrap(), 1, "写回表只有一行");
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        1,
        "只产生一条已生效的写回审计"
    );
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Created));
    assert_eq!(store.in_doubt_count().unwrap(), 0);
}

fn tally<'a>(results: &'a [&'a str]) -> Vec<(&'a str, usize)> {
    let mut out: Vec<(&str, usize)> = Vec::new();
    for r in results {
        match out.iter_mut().find(|(k, _)| k == r) {
            Some((_, n)) => *n += 1,
            None => out.push((r, 1)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 用例 2：同键重试 → 0 次外发，原样回放首次结果
// ---------------------------------------------------------------------------

#[test]
fn a_retry_after_a_timeout_replays_the_first_result_without_sending_again() {
    let tmp = TempDb::new("replay");
    let store = store_ready(&tmp.path());
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let ctx1 = confirm::ConfirmContext {
        encounter_alias: ALIAS.into(),
        actor: "脱敏-护士长A".into(),
        at: AT.into(),
        ruleset_sha256: SHA.into(),
        run_id: Some("RUN-1".into()),
    };
    confirm::create_draft(&store, "DRAFT-1", &ctx1, &entries()).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, AT).unwrap();
    let first = confirm::confirm(&store, &cfg, &token(), "DRAFT-1", &ctx1, &builder(), &transport)
        .expect("首次应成功");
    assert_eq!(first.outbound, 1);
    assert_eq!(first.new_rows(), 1);
    let created_first = first.created.clone();
    assert!(!created_first.is_empty());

    // 网络超时后客户端重试（另一份草稿、同一件同一内容）
    let ctx2 = confirm::ConfirmContext {
        encounter_alias: ALIAS.into(),
        actor: "脱敏-护士长A".into(),
        at: "2026-03-03T08:20:00+08:00".into(),
        ruleset_sha256: SHA.into(),
        run_id: Some("RUN-2".into()),
    };
    confirm::create_draft(&store, "DRAFT-2", &ctx2, &entries()).unwrap();
    confirm::set_selected(&store, "DRAFT-2", CONSENT, true, AT).unwrap();
    let second =
        confirm::confirm(&store, &cfg, &token(), "DRAFT-2", &ctx2, &builder(), &transport)
            .expect("重试应命中重放而不是报错");

    assert!(second.replayed, "第二次是重放");
    assert_eq!(second.outbound, 0, "重放 0 次外发");
    assert_eq!(second.new_rows(), 0, "0 条新写回");
    assert_eq!(
        second.created, created_first,
        "回放的是首次的结果，不是重新问服务端拿的"
    );
    let (actor, at) = second.replayed_from.clone().expect("带首次操作人与时刻");
    assert_eq!(actor, "脱敏-护士长A");
    assert_eq!(at, AT);

    assert_eq!(
        f.outbound_transactions(),
        1,
        "整个过程只发过一份 Bundle transaction"
    );
    assert_eq!(store.writeback_row_count().unwrap(), 1);
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        1,
        "幂等命中不追加已生效事实"
    );
}

// ---------------------------------------------------------------------------
// 用例 3：外发失败 → 键放开，重试能再发
// ---------------------------------------------------------------------------

#[test]
fn a_failed_transaction_releases_the_keys_so_a_retry_can_go_out() {
    // 端点 503 之后，这件不能被永久锁死成「别人正在提交」——
    // 那会让护士长看着一个永远冲突的件却无路可走。
    let tmp = TempDb::new("release");
    let store = store_ready(&tmp.path());
    let ctx = confirm::ConfirmContext {
        encounter_alias: ALIAS.into(),
        actor: "脱敏-护士长A".into(),
        at: AT.into(),
        ruleset_sha256: SHA.into(),
        run_id: Some("RUN-1".into()),
    };
    confirm::create_draft(&store, "DRAFT-1", &ctx, &entries()).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, AT).unwrap();

    let down = FakeFhir::start(Mode::ServerError);
    let err = confirm::confirm(
        &store,
        &down.config(),
        &token(),
        "DRAFT-1",
        &ctx,
        &builder(),
        &UreqTransport::default(),
    )
    .unwrap_err();
    assert_eq!(err.code(), "E_FHIR_STATUS");
    assert_eq!(store.writeback_row_count().unwrap(), 0, "失败不产生已写回");
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        0,
        "失败不追加已生效事实——否则审计里的「写回了几次」会虚高"
    );
    assert_eq!(store.in_flight_claims(ALIAS).unwrap().len(), 0, "键已放开");

    // 端点恢复后重试：这一次真的发出去
    let up = FakeFhir::start(Mode::AllOk);
    let out = confirm::confirm(
        &store,
        &up.config(),
        &token(),
        "DRAFT-1",
        &ctx,
        &builder(),
        &UreqTransport::default(),
    )
    .expect("端点恢复后应能重试成功");
    assert_eq!(out.outbound, 1);
    assert_eq!(out.new_rows(), 1);
    assert_eq!(up.outbound_transactions(), 1);
}

// ---------------------------------------------------------------------------
// 用例 4：上次结果未知 → 拒绝重发，0 次外发
// ---------------------------------------------------------------------------

#[test]
fn an_in_doubt_batch_is_never_resent_and_the_outbound_count_stays_zero() {
    // 上一次进程崩在外发与落库之间：服务端可能已经建好了件。
    // 「再点一次提交」是最自然的操作，也是最危险的操作——那会让护士长
    // 看到两份同 id 的知情同意。所以必须拒绝，且**一次都不许发**。
    let tmp = TempDb::new("indoubt");
    let path = tmp.path();
    store_ready(&path);
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();

    let ctx = confirm::ConfirmContext {
        encounter_alias: ALIAS.into(),
        actor: "脱敏-护士长A".into(),
        at: AT.into(),
        ruleset_sha256: SHA.into(),
        run_id: Some("RUN-1".into()),
    };
    confirm::create_draft(&store_prepare(&path), "DRAFT-CRASH", &ctx, &entries()).unwrap();
    // 模拟崩在外发之后：只占键，不 settle
    {
        let store = Store::open(&path).unwrap();
        let targets = [psychdoc_store::ClaimTarget {
            doc_id: CONSENT,
            content_sha256: SHA,
        }];
        let out = store
            .reserve_writeback(ALIAS, &targets, "BATCH-DRAFT-CRASH", "脱敏-护士长A", AT)
            .unwrap();
        assert!(matches!(
            out,
            psychdoc_store::ReserveOutcome::Reserved { .. }
        ));
    }

    // 重启：启动自检把上次遗留的 in_flight 升级为 in_doubt
    let store = Store::open(&path).unwrap();
    assert_eq!(store.recover_orphans("2026-03-04T09:00:00+08:00").unwrap(), 1);
    assert_eq!(store.in_doubt_count().unwrap(), 1);

    // 护士长看到「上次没提交成功」，再点一次
    let ctx2 = confirm::ConfirmContext {
        actor: "脱敏-护士长A".into(),
        at: "2026-03-04T09:10:00+08:00".into(),
        run_id: Some("RUN-2".into()),
        ..ctx.clone()
    };
    confirm::create_draft(&store, "DRAFT-2", &ctx2, &entries()).unwrap();
    confirm::set_selected(&store, "DRAFT-2", CONSENT, true, AT).unwrap();
    let err = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-2",
        &ctx2,
        &builder(),
        &UreqTransport::default(),
    )
    .unwrap_err();

    assert_eq!(err.code(), "E_REPLAY_IN_DOUBT");
    assert!(!err.is_retryable(), "再点一次解决不了「结果未知」，它要的是人工核对");
    let text = err.to_string();
    assert!(text.contains("不重发"), "{text}");
    assert!(text.contains("人工核对"), "{text}");
    assert_eq!(
        f.outbound_transactions(),
        0,
        "结果未知时一次都不能发"
    );
    assert_eq!(store.writeback_row_count().unwrap(), 0);
}

/// 借用一个建好 schema 的连接（草稿要挂在真实件上）。
fn store_prepare(path: &PathBuf) -> Store {
    Store::open(path).unwrap()
}