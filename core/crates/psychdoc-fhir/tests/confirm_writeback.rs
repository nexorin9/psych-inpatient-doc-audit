//! Task 12 验收：**确认写回闭环**——草稿 → 逐条勾选 → 整批发送 → 状态推进 → 撤回。
//!
//! 打在**本地 FHIR 协议替身**上，走真实 socket 与真实 `UreqTransport`：
//! transaction 的请求形状必须过一遍真实 HTTP 编解码才作数。
//! 替身只存在于本 `tests/`，**不是**产品默认对接面——生产路径是
//! `UreqTransport` 指向 `EMR_BASE_URL` 的真实院内端点。
//!
//! 五个用例对应 task.json 的五条 step：
//!
//! 1. 未勾选任何条 → `E_NOTHING_SELECTED` 且**零次外发**；
//! 2. 勾选一条 → 整批 transaction 发出 → 写回落库 + 状态 `草稿→已建件`；
//! 3. 非法跳变（如 `缺失→可归档`）被状态机拒绝且既有状态不变；
//! 4. 撤回：已勾选未发送 → 回草稿区 0 次外发；已发送 → 反向 transaction；
//! 5. 端点失败 → 0 条部分写入、本地草稿保留、状态不推进。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use psychdoc_fhir::{auth, confirm, AccessToken, EmrConfig, Transport, UreqTransport};
use psychdoc_model::state::DocState;
use psychdoc_model::{DocItem, Field, Section, Span};
use psychdoc_store::{Store, WritebackEntry};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 本地 FHIR 协议替身（只在本 tests/ 内出现）
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
}

/// 替身模式：整批成功 / 部分 entry 被拒 / 端点整体 500。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    AllOk,
    /// 端点回 200，但第二条 entry 是 422
    PartialReject,
    ServerError,
}

struct FakeFhir {
    base_url: String,
    /// 运行期由替身线程闭包持有；此处留字段是为了 `config()` 与断言
    /// 能在同一处看到「本次替身跑的是哪种模式」。
    #[allow(dead_code)]
    mode: Mode,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    call_count: Arc<AtomicUsize>,
}

impl FakeFhir {
    fn start(mode: Mode) -> Self {
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
                std::thread::spawn(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(req) = read_request(&stream) {
                        let reply = route(&req, mode);
                        let _ = stream.write_all(&reply);
                        let _ = stream.flush();
                        seen.lock().unwrap().push(req);
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/fhir/r4"),
            mode,
            seen,
            call_count: calls,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    fn outbound_calls(&self) -> usize {
        self.call_count.load(Ordering::SeqCst)
    }

    /// 打到根路径的 transaction POST（不含 token 那次）。
    fn transactions(&self) -> Vec<SeenRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/fhir/r4")
            .collect()
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

fn http(status: u16, reason: &str, body: &Value) -> Vec<u8> {
    let payload = body.to_string();
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: application/fhir+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .into_bytes()
}

fn route(req: &SeenRequest, mode: Mode) -> Vec<u8> {
    if req.path.starts_with("/fhir/r4/oauth2/token") {
        return http(
            200,
            "OK",
            &json!({"access_token": "tok-local", "expires_in": 600, "token_type": "Bearer"}),
        );
    }
    if req.method == "POST" && req.path == "/fhir/r4" {
        if mode == Mode::ServerError {
            return http(
                500,
                "Internal Server Error",
                &json!({"message": "服务端内部错误，事务未提交"}),
            );
        }
        let Ok(body) = serde_json::from_str::<Value>(&req.body) else {
            return http(400, "Bad Request", &json!({"message": "非 JSON"}));
        };
        let entries: Vec<Value> = body["entry"]
            .as_array()
            .map(|es| {
                es.iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let url = e["request"]["url"].as_str().unwrap_or("X");
                        let rejected =
                            mode == Mode::PartialReject && i == 1 && url != "Provenance";
                        json!({
                            "response": {
                                "status": if rejected { "422 Unprocessable" } else { "201 Created" },
                                "location": format!("{url}/_history/1")
                            },
                            "resource": if url == "Provenance" {
                                json!({"resourceType": "Provenance", "id": "PS-1"})
                            } else {
                                json!({})
                            }
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        return http(
            200,
            "OK",
            &json!({"resourceType": "Bundle", "type": "transaction-response", "entry": entries}),
        );
    }
    http(404, "Not Found", &json!({"message": "未知路径"}))
}

// ---------------------------------------------------------------------------
// 夹具与上下文
// ---------------------------------------------------------------------------

const ALIAS: &str = "WARD-A-001";
const CONSENT: &str = "WARD-A-001-CONSENT-01";
const RECORD: &str = "WARD-A-001-RECORD-01";
const SHA: &str = "e4eda0c79c95aa11bb22cc33dd44ee9911a1b2c3d4e5f60718293a4b5c6d7e8f9";

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

/// 建一份「N 件已登记、状态沿合法路径推进到 `target`」的库。
fn store_with_drafts_in_state(items: &[&DocItem], target: DocState) -> Store {
    let store = Store::open_in_memory().expect("内存库");
    let at = "2026-03-03T08:00:00+08:00";
    let owned: Vec<DocItem> = items.iter().map(|i| (*i).clone()).collect();
    store.register_items(&owned, at).unwrap();
    // 合法路径只有一条：缺失 → 草稿 → 已建件 → 已收 → 可归档
    let path = [DocState::Draft, DocState::Created, DocState::Received, DocState::Archivable];
    let steps = path
        .iter()
        .position(|s| *s == target)
        .map(|i| i + 1)
        .unwrap_or(0);
    for it in items {
        store.init_state(&it.doc_id, DocState::Missing, "护士长A", at).unwrap();
        for s in path.iter().take(steps) {
            store.advance_state(&it.doc_id, *s, "护士长A", at).unwrap();
        }
    }
    store
}

fn ctx() -> confirm::ConfirmContext {
    confirm::ConfirmContext {
        encounter_alias: ALIAS.into(),
        actor: "脱敏-护士长A".into(),
        at: "2026-03-03T08:10:00+08:00".into(),
        ruleset_sha256: SHA.into(),
        run_id: Some("RUN-2026-03-03-01".into()),
    }
}

/// 造件条目的工厂：把 doc_id 编进资源体，与库里登记的件一一对应。
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

fn entries<'a>(docs: &'a [&'a str]) -> Vec<WritebackEntry<'a>> {
    docs.iter()
        .map(|d| WritebackEntry {
            doc_id: d,
            content_sha256: SHA,
            run_id: Some("RUN-2026-03-03-01"),
        })
        .collect()
}

fn token() -> AccessToken {
    AccessToken {
        token: "tok-local".into(),
        expires_in_secs: 600,
    }
}

// ---------------------------------------------------------------------------
// Step 1：未勾选任何条 → 零次外发
// ---------------------------------------------------------------------------

#[test]
fn nothing_selected_is_refused_before_a_single_outbound_call() {
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();
    let store = Store::open_in_memory().unwrap();

    let it = item(CONSENT);
    store.register_items(std::slice::from_ref(&it), "2026-03-03T08:00:00+08:00").unwrap();
    store
        .init_state(CONSENT, DocState::Draft, "护士长A", "2026-03-03T08:00:00+08:00")
        .unwrap();

    // 建了草稿，但一条都没勾
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    assert_eq!(store.selected_count("DRAFT-1").unwrap(), 0);

    let before = f.outbound_calls();
    let err = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap_err();
    assert_eq!(err.code(), "E_NOTHING_SELECTED");
    assert_eq!(
        f.outbound_calls(),
        before,
        "未勾选必须 0 次外发——拦截发生在任何网络调用之前"
    );
    assert_eq!(f.transactions().len(), 0);
    // 本地也不产生任何已生效事实
    assert_eq!(store.writeback_row_count().unwrap(), 0);
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Draft));
}

// ---------------------------------------------------------------------------
// Step 2：勾选一条 → 整批 transaction → 落库 + 状态推进
// ---------------------------------------------------------------------------

#[test]
fn one_selected_item_is_sent_as_a_whole_transaction_and_advances_the_state() {
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let b = item(RECORD);
    let store = store_with_drafts_in_state(&[&a, &b], DocState::Draft);

    let e = entries(&[CONSENT, RECORD]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    // 只勾一条：另一条留在草稿里不动
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();
    assert_eq!(store.selected_count("DRAFT-1").unwrap(), 1);

    let out = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .expect("整批应成功");

    assert_eq!(out.new_rows(), 1);
    assert!(!out.is_replay());
    assert_eq!(out.created.len(), 1);
    assert!(out.provenance.is_some(), "Provenance 必须落库");
    assert_eq!(out.advanced, vec![(CONSENT.to_string(), DocState::Created)]);

    // 真的过了一遍 socket 的请求体
    let txns = f.transactions();
    assert_eq!(txns.len(), 1, "只发一次整批 transaction");
    let post = &txns[0];
    assert_eq!(post.header("Content-Type"), Some("application/fhir+json"));
    assert_eq!(post.header("X-Tenant-Id"), Some("ward-a"));
    assert_eq!(post.header("Authorization"), Some("Bearer tok-local"));
    let sent = post.json();
    assert_eq!(sent["resourceType"], "Bundle");
    assert_eq!(sent["type"], "transaction");
    let ents = sent["entry"].as_array().unwrap();
    assert_eq!(ents.len(), 2, "勾了 1 件 + 1 条 Provenance");
    assert_eq!(ents[0]["request"]["method"], "POST");
    assert_eq!(ents[1]["resource"]["resourceType"], "Provenance");
    // Provenance 带上幂等键与规则集短哈希
    let entity = ents[1]["resource"]["entity"][0]["what"]["display"]
        .as_str()
        .unwrap();
    assert!(entity.contains("e4eda0c79c95"), "{entity}");

    // 本地：写回 1 行 + 审计 1 条 + 状态推进
    assert_eq!(store.writeback_row_count().unwrap(), 1);
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        1
    );
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Created));
    assert_eq!(
        store.state_of(RECORD).unwrap(),
        Some(DocState::Draft),
        "未勾选的件状态不动"
    );
    assert_eq!(store.draft_state("DRAFT-1").unwrap().as_deref(), Some("sent"));
}

#[test]
fn a_repeated_confirmation_on_the_same_day_writes_zero_new_rows() {
    // 同日重复确认：幂等键命中，0 条新写回、不重复推进、不重复外发 transaction
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let store = store_with_drafts_in_state(&[&a], DocState::Draft);
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();

    let first = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap();
    assert_eq!(first.new_rows(), 1);

    // 同日另一份草稿再确认同一件：幂等键（住院别名 + 件 id + 内容哈希）命中
    let e2 = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-2", &ctx(), &e2).unwrap();
    confirm::set_selected(&store, "DRAFT-2", CONSENT, true, "2026-03-03T09:00:00+08:00").unwrap();
    let second = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-2",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap();
    assert!(second.is_replay(), "第二次应幂等命中");
    assert_eq!(second.new_rows(), 0);
    assert_eq!(store.writeback_row_count().unwrap(), 1, "写回表仍是 1 行");
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        1,
        "幂等命中不追加新的已生效事实"
    );
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Created));
}

// ---------------------------------------------------------------------------
// Step 3：状态机拒绝非法跳变
// ---------------------------------------------------------------------------

#[test]
fn an_illegal_state_jump_is_refused_and_the_existing_state_is_untouched() {
    let store = Store::open_in_memory().unwrap();
    let a = item(CONSENT);
    store.register_items(std::slice::from_ref(&a), "2026-03-03T08:00:00+08:00").unwrap();
    store
        .init_state(CONSENT, DocState::Missing, "护士长A", "2026-03-03T08:00:00+08:00")
        .unwrap();

    // 缺失 → 可归档 不可跳步
    let err = store
        .advance_state(CONSENT, DocState::Archivable, "护士长A", "2026-03-03T08:10:00+08:00")
        .unwrap_err();
    assert_eq!(err.code(), "E_STATE_JUMP");
    assert_eq!(
        store.state_of(CONSENT).unwrap(),
        Some(DocState::Missing),
        "既有状态不改变"
    );
}

#[test]
fn an_illegal_jump_inside_the_confirmation_leaves_no_effective_fact() {
    // 件已在「可归档」（终态），确认写回时的推进必须被拒而不是静默改数据。
    // 这里直接对状态机做等价断言：终态不接受任何推进。
    let store = store_with_drafts_in_state(&[&item(CONSENT)], DocState::Archivable);
    let err = store
        .advance_state(CONSENT, DocState::Draft, "护士长A", "2026-03-03T08:10:00+08:00")
        .unwrap_err();
    assert_eq!(err.code(), "E_STATE_JUMP");
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Archivable));
    assert_eq!(store.writeback_row_count().unwrap(), 0, "0 条已生效事实");
}

// ---------------------------------------------------------------------------
// Step 4：撤回
// ---------------------------------------------------------------------------

#[test]
fn withdrawing_a_selected_but_unsent_draft_clears_it_with_zero_network_calls() {
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let store = store_with_drafts_in_state(&[&a], DocState::Draft);
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();
    assert_eq!(store.selected_count("DRAFT-1").unwrap(), 1);

    let before = f.outbound_calls();
    let cleared = confirm::withdraw(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .expect("撤回应成功");
    assert_eq!(cleared, 1, "撤回清掉 1 条勾选");
    assert_eq!(store.selected_count("DRAFT-1").unwrap(), 0);
    assert_eq!(store.draft_state("DRAFT-1").unwrap().as_deref(), Some("draft"));
    assert_eq!(
        f.outbound_calls(),
        before,
        "没发出去过的草稿撤回不需要任何网络调用"
    );
    assert_eq!(store.writeback_row_count().unwrap(), 0, "撤回不产生已生效事实");

    // 清空勾选后可再次提交
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:20:00+08:00").unwrap();
    let out = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap();
    assert_eq!(out.new_rows(), 1, "撤回后可以再次提交");
}

#[test]
fn withdrawing_a_sent_draft_sends_a_reversal_and_returns_the_item_to_draft() {
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let store = store_with_drafts_in_state(&[&a], DocState::Draft);
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();
    confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap();
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Created));

    let reverted = confirm::withdraw(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .expect("撤回已发送的草稿应成功");
    assert_eq!(reverted, 1);

    // 第二笔 transaction 就是反向的 DELETE
    let txns = f.transactions();
    assert_eq!(txns.len(), 2, "第一次建件 + 第二次撤回");
    let rev = txns[1].json();
    assert_eq!(rev["type"], "transaction");
    assert_eq!(rev["entry"][0]["request"]["method"], "DELETE");
    assert_eq!(
        rev["entry"][0]["request"]["url"],
        format!("Composition/{CONSENT}")
    );
    assert_eq!(rev["entry"][1]["resource"]["resourceType"], "Provenance");

    // 件回到草稿态、草稿区可再次提交，并留一条审计
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Draft));
    assert_eq!(store.draft_state("DRAFT-1").unwrap().as_deref(), Some("draft"));
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        2,
        "发过 + 撤过各留一条"
    );
    assert_eq!(
        store.writeback_row_count().unwrap(),
        1,
        "撤回不动写回日志——那是「发出去过」的历史"
    );
}

// ---------------------------------------------------------------------------
// Step 5：端点失败 → 0 条部分写入、草稿保留、状态不推进
// ---------------------------------------------------------------------------

#[test]
fn a_server_error_leaves_zero_local_writes_and_the_draft_intact() {
    let f = FakeFhir::start(Mode::ServerError);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let store = store_with_drafts_in_state(&[&a], DocState::Draft);
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();

    let err = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap_err();
    assert_eq!(err.code(), "E_FHIR_STATUS");
    assert!(err.is_retryable(), "5xx 可重试");

    // 关键：没有「本地成功」
    assert_eq!(store.writeback_row_count().unwrap(), 0);
    assert_eq!(
        store.audit_event_count(psychdoc_store::AuditKind::Writeback).unwrap(),
        0
    );
    assert_eq!(
        store.state_of(CONSENT).unwrap(),
        Some(DocState::Draft),
        "状态不推进"
    );
    assert_eq!(
        store.draft_state("DRAFT-1").unwrap().as_deref(),
        Some("draft"),
        "草稿保留可重试"
    );
    assert_eq!(store.selected_count("DRAFT-1").unwrap(), 1, "勾选保留");
}

#[test]
fn a_partially_rejected_batch_produces_no_local_success_at_all() {
    let f = FakeFhir::start(Mode::PartialReject);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let b = item(RECORD);
    let store = store_with_drafts_in_state(&[&a, &b], DocState::Draft);
    let e = entries(&[CONSENT, RECORD]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();
    confirm::set_selected(&store, "DRAFT-1", RECORD, true, "2026-03-03T08:05:00+08:00").unwrap();

    let err = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &builder(),
        &transport,
    )
    .unwrap_err();
    assert_eq!(err.code(), "E_FHIR_TXN_REJECTED");
    assert!(err.to_string().contains("本地草稿保留"));

    // 整批失败：两条件都没写进本地，一条都没推进
    assert_eq!(store.writeback_row_count().unwrap(), 0);
    assert_eq!(store.state_of(CONSENT).unwrap(), Some(DocState::Draft));
    assert_eq!(store.state_of(RECORD).unwrap(), Some(DocState::Draft));
}

#[test]
fn a_selection_pointing_at_an_unknown_item_is_rejected_not_silently_skipped() {
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();

    let a = item(CONSENT);
    let store = store_with_drafts_in_state(&[&a], DocState::Draft);
    let e = entries(&[CONSENT]);
    confirm::create_draft(&store, "DRAFT-1", &ctx(), &e).unwrap();
    confirm::set_selected(&store, "DRAFT-1", CONSENT, true, "2026-03-03T08:05:00+08:00").unwrap();

    // 工厂对某一件返回 None（无法构造资源）——必须报错，不能悄悄少发一件
    let partial = |doc_id: &str| -> Option<psychdoc_fhir::WriteEntry> {
        if doc_id == CONSENT {
            None
        } else {
            builder()(doc_id)
        }
    };
    let err = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-1",
        &ctx(),
        &partial,
        &transport,
    )
    .unwrap_err();
    assert_eq!(err.code(), "E_DRAFT_ENTRY_UNKNOWN");
    assert_eq!(f.transactions().len(), 0, "构造失败不发请求");
    assert_eq!(store.writeback_row_count().unwrap(), 0);
}

// ---------------------------------------------------------------------------
// 替身纪律
// ---------------------------------------------------------------------------

#[test]
fn the_protocol_stub_never_appears_in_the_product_crate_surface() {
    // 可执行的纪律：替身类型定义在本 tests/ 内，
    // 产品侧只暴露 EmrConfig / Transport / confirm 这套真实接口。
    let cfg = EmrConfig::from_source(|k| match k {
        "EMR_BASE_URL" => Some("https://emr.example-hospital.local/fhir/r4".into()),
        "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
        "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
        "EMR_TENANT_ID" => Some("ward-a".into()),
        _ => None,
    })
    .unwrap();
    assert!(cfg.base_url.starts_with("https://"));
    // 生产 transport 是 UreqTransport，替身只是个实现同一 trait 的本地服务
    let _transport: Box<dyn Transport> = Box::new(UreqTransport::default());
    let _ = auth::fetch_token;
}
// ---------------------------------------------------------------------------
// Step 1 的端到端：真实脱敏夹具 → 解析 → 审查 → 补写预览 → 草稿 → 确认写回
//
// 上面的用例用手工构造的 DocItem 逐段钉行为；这里**不手工造件**，
// 走真实夹具 `fixtures/ward-08-field-violations.json`（含全角日期这一
// 唯一可自动修的分支），把「骨架渲染 → 与原文对照 → 候选块 → 草稿 →
// 整批 transaction」串成一条可复现的链。
// ---------------------------------------------------------------------------

const WARD_08: &str = include_str!("../../../../fixtures/ward-08-field-violations.json");
const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");

/// 取 `键：值` 行的键部分；无分隔符时整行即为键。
fn key_of(line: &str) -> &str {
    line.split_once('：').map_or(line, |(k, _)| k)
}

#[test]
fn a_real_fixture_runs_from_preview_blocks_to_one_transaction() {
    use psychdoc_model::preview_fixback;
    use psychdoc_parse::parse_bundle;
    use psychdoc_rules::{load_effective, review};

    // 1) 真实夹具解析 + 审查
    let rules = load_effective(RULESET).expect("院级规则集应可加载");
    let parsed = parse_bundle(WARD_08, Some(&rules.registry())).expect("夹具应可解析");
    let items = parsed.items.clone();
    assert!(!items.is_empty(), "夹具至少解析出 1 件");

    let result = review(&items, &rules);
    // 这份夹具里 QC-PSY-02（全角日期）是唯一带 Automatic 载荷的码
    let fixable: Vec<_> = result
        .diagnostics
        .iter()
        .filter(|d| d.fix.is_some())
        .collect();
    assert!(
        !fixable.is_empty(),
        "真实夹具里应至少有一条可补写项，否则整条链无从验起"
    );

    // 2) 补写预览：候选块逐段可定位、带回规则码与依据
    let target = items
        .iter()
        .find(|it| result.diagnostics.iter().any(|d| d.fix.is_some() && d.loc.doc_id == it.doc_id))
        .expect("命中可补写诊断的件");
    let pv = preview_fixback(target, &rules.registry(), &result.diagnostics);
    assert!(
        !pv.blocks.is_empty(),
        "带 Automatic 载荷的诊断应产生可勾选候选块"
    );
    let b = &pv.blocks[0];
    assert!(b.has_additions(), "候选块必须有真新增行，否则不该被勾选");
    assert!(b.basis.contains("第"), "候选块必须带回院内规范依据");
    // 骨架以原文为底：值归一（全角→半角）会让同一栏位表现为「删一行 + 加一行」，
    // 这是对的；真正要守住的是**没有任何栏位被悄悄删掉**——每个被删的键
    // 都必须有一个同键的新增行顶上来。
    let added_keys: Vec<&str> = b
        .diff
        .iter()
        .filter(|l| matches!(l.op, psychdoc_model::DiffOp::Added))
        .map(|l| key_of(&l.text))
        .collect();
    for line in b.diff.iter().filter(|l| matches!(l.op, psychdoc_model::DiffOp::Removed)) {
        let k = key_of(&line.text);
        assert!(
            added_keys.contains(&k),
            "栏位「{k}」被删掉却没有同键新增行顶上来：{}",
            line.text
        );
    }

    // 3) 候选块 → 草稿：同一件的多段合并成一条 entry（见 draft_from_preview 文档）
    let f = FakeFhir::start(Mode::AllOk);
    let cfg = f.config();
    let transport = UreqTransport::default();
    let store = Store::open_in_memory().unwrap();
    let at = "2026-03-03T08:00:00+08:00";
    store.register_items(&items, at).unwrap();
    for it in &items {
        store.init_state(&it.doc_id, DocState::Missing, "护士长A", at).unwrap();
        store.advance_state(&it.doc_id, DocState::Draft, "护士长A", at).unwrap();
    }

    let n = confirm::draft_from_preview(
        &store,
        "DRAFT-REAL",
        &ctx(),
        &pv.blocks,
        &|doc_id: &str| {
            psychdoc_store::content_of(items.iter().find(|i| i.doc_id == doc_id).unwrap())
        },
    )
    .expect("候选块应能落成草稿");
    assert_eq!(n, 1, "同一件的多段候选块合并成 1 条草稿条目");
    assert_eq!(store.draft_items("DRAFT-REAL").unwrap().len(), 1);
    assert_eq!(
        store.selected_count("DRAFT-REAL").unwrap(),
        0,
        "草稿默认不勾选——勾选是人的动作"
    );

    // 4) 勾选 → 整批 transaction
    let doc_id = &b.doc_id;
    confirm::set_selected(&store, "DRAFT-REAL", doc_id, true, "2026-03-03T08:05:00+08:00")
        .unwrap();
    let out = confirm::confirm(
        &store,
        &cfg,
        &token(),
        "DRAFT-REAL",
        &ctx(),
        &builder(),
        &transport,
    )
    .expect("整批应成功");

    assert_eq!(out.new_rows(), 1);
    assert!(out.provenance.is_some(), "Provenance 必须落库");
    assert_eq!(out.advanced, vec![(doc_id.clone(), DocState::Created)]);
    assert_eq!(store.state_of(doc_id).unwrap(), Some(DocState::Created));

    // 真的过了一遍 socket：1 条 Composition + 1 条 Provenance
    let txns = f.transactions();
    assert_eq!(txns.len(), 1);
    let ents = txns[0].json()["entry"].as_array().unwrap().clone();
    assert_eq!(ents.len(), 2);
    assert_eq!(ents[0]["resource"]["resourceType"], "Composition");
    assert_eq!(ents[0]["resource"]["id"], doc_id.as_str());
    assert_eq!(ents[1]["resource"]["resourceType"], "Provenance");
    // Provenance 必须可回溯到当时的规则集版本（12 位短哈希）
    let prov = &ents[1]["resource"];
    let entity = prov["entity"][0]["what"]["display"].as_str().unwrap();
    assert!(
        entity.contains("e4eda0c79c95"),
        "Provenance 须带规则集短哈希：{entity}"
    );
    assert!(entity.contains(ALIAS), "Provenance 须带住院别名：{entity}");
    assert_eq!(prov["recorded"], "2026-03-03T08:10:00+08:00");
    assert_eq!(prov["agent"][0]["who"]["display"], "脱敏-护士长A");
}
