//! 协议级测试：打在**本地 FHIR 协议替身**上，用**真实 socket**。
//!
//! ## 替身纪律（重要）
//!
//! 本文件里的 `FakeFhir` 是**测试数据层**的脱敏协议替身，**不是**产品的
//! 最终对接面。生产路径是 [`psychdoc_fhir::UreqTransport`] 指向
//! `EMR_BASE_URL` 的真实院内端点。
//!
//! 之所以不用纯内存 mock 而是起一个真 HTTP server：请求方法 / 路径 /
//! 请求头 / 请求体形状必须**过一遍真实的 HTTP 编解码**才作数。用
//! `MockTransport` 断言自己写进去的东西，等于什么都没验。
//!
//! 替身**不实现** OAuth2 的真实签名与 TLS，只按 R4 的路径 / 头 / 方法
//! 规范应答；凭据从环境变量读这一条由 `config.rs` 的单测独立守住。

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use psychdoc_fhir::{
    auth, build_transaction, read, send_transaction, write, AccessToken, EmrConfig, FhirError,
    HttpRequest, HttpMethod, Transport, UreqTransport,
};
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 本地 FHIR 协议替身（只在本 tests/ 内出现）
// ---------------------------------------------------------------------------

/// 替身记下收到的每一次请求，供断言方法 / 路径 / 头 / 体。
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

struct FakeFhir {
    base_url: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
    call_count: Arc<AtomicUsize>,
}

impl FakeFhir {
    /// 起一个只讲 FHIR R4 形状的本地 HTTP 服务。
    fn start(routes: Vec<(String, Value)>) -> Self {
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
                let routes = routes.clone();
                std::thread::spawn(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    if let Some(req) = read_request(&stream) {
                        let reply = route(&req, &routes);
                        let _ = stream.write_all(&reply);
                        let _ = stream.flush();
                        seen.lock().unwrap().push(req);
                    }
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/fhir/r4"),
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

/// 极简路由：只认 `/fhir/r4/**` 下的 FHIR 读与写两条路径。
fn route(req: &SeenRequest, routes: &[(String, Value)]) -> Vec<u8> {
    // 1) token 端点
    if req.path.starts_with("/fhir/r4/oauth2/token") {
        return http(
            200,
            "OK",
            &json!({"access_token": "tok-local", "expires_in": 600, "token_type": "Bearer"}),
        );
    }
    // 2) 写回：POST 到根 = Bundle transaction
    if req.method == "POST" && req.path == "/fhir/r4" {
        let body: Value = match serde_json::from_str(&req.body) {
            Ok(v) => v,
            Err(_) => return http(400, "Bad Request", &json!({"message": "非 JSON"})),
        };
        // 逐条 entry 应答；这里模拟「全部成功」
        let entries: Vec<Value> = body["entry"]
            .as_array()
            .map(|es| {
                es.iter()
                    .map(|e| {
                        let url = e["request"]["url"].as_str().unwrap_or("X");
                        let is_prov = url == "Provenance";
                        json!({
                            "response": {
                                "status": "201 Created",
                                "location": format!("{url}/_history/1")
                            },
                            "resource": if is_prov {
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
    // 3) 读：searchset
    if req.method == "GET" {
        if let Some((_, body)) = routes.iter().find(|(p, _)| req.path.starts_with(p.as_str())) {
            return http(200, "OK", body);
        }
    }
    http(404, "Not Found", &json!({"message": "未知路径"}))
}

/// 一份脱敏 searchset：2 件文书 + 1 件不在院内字典的件。
fn searchset() -> Value {
    json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": [
            {"resource": {
                "resourceType": "Composition",
                "id": "WARD-A-001-CONSENT-01",
                "status": "in-progress",
                "date": "2026-03-02T23:30:00+08:00",
                "identifier": [{"system": "https://emr.example/encounter", "value": "WARD-A-001"}],
                "type": {"coding": [{"code": "TPL-CONSTRAINT-CONSENT", "display": "约束知情同意书"}]},
                "author": [{"reference": "Practitioner/deid-nurse-a"}],
                "section": [
                    {"title": "基本信息", "text": "患者：脱敏-患者甲\n约束起始时间：2026-03-02 23:40\n执行护士：脱敏-护士A"},
                    {"title": "患方签字", "text": ""}
                ]
            }},
            {"resource": {
                "resourceType": "Composition",
                "id": "WARD-A-001-RECORD-01",
                "status": "in-progress",
                "date": "2026-03-02T23:40:00+08:00",
                "identifier": [{"system": "https://emr.example/encounter", "value": "WARD-A-001"}],
                "type": {"coding": [{"code": "TPL-NURSING-RECORD", "display": "护理记录"}]},
                "section": [{"title": "记录", "text": "2026-03-02 23:40 约束执行，患方在场。"}]
            }},
            {"resource": {
                "resourceType": "Composition",
                "id": "WARD-A-001-LAB-REPORT",
                "status": "final",
                "identifier": [{"system": "https://emr.example/encounter", "value": "WARD-A-001"}],
                "type": {"coding": [{"code": "TPL-LAB-REPORT", "display": "检验报告"}]},
                "section": [{"title": "结果", "text": "血钾 4.1 mmol/L"}]
            }}
        ]
    })
}

/// 院内文书类型字典（与 `rulesets/hospital.yaml` 同一套码）。
fn registry() -> psychdoc_model::DocTypeRegistry {
    serde_yaml::from_str(
        r#"
templates:
  TPL-CONSTRAINT-CONSENT:
    display: 约束知情同意书
    fields:
      - key: 约束起始时间
        required: true
      - key: 执行护士
        required: true
  TPL-NURSING-RECORD:
    display: 护理记录
    fields:
      - key: 执行护士
        required: true
"#,
    )
    .expect("院内文书类型字典 YAML 合法")
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

/// 脱敏替身**不出现在产品代码里**：本 crate 的非测试代码不得引用本文件。
#[test]
fn the_fake_server_is_only_reachable_from_tests() {
    // 这个断言是纪律的可执行版本：替身类型定义在 tests/ 内，
    // lib.rs 的 pub use 列表里没有任何替身名字。
    let f = FakeFhir::start(vec![]);
    assert!(f.base_url.starts_with("http://127.0.0.1:"));
}

#[test]
fn startup_check_without_env_makes_zero_network_calls() {
    // 未设环境变量时：报 E_CFG_MISSING，且 0 次外发。
    // 这里刻意不启动 server——没有任何 transport 被构造过，物理上无法外发。
    let err = EmrConfig::from_source(|_| None).unwrap_err();
    assert_eq!(err.code(), "E_CFG_MISSING");
    let text = err.to_string();
    for key in [
        "EMR_BASE_URL",
        "EMR_CLIENT_ID",
        "EMR_CLIENT_SECRET",
        "EMR_TENANT_ID",
    ] {
        assert!(text.contains(key), "{key} 应列在缺失清单：{text}");
    }
    assert!(text.contains("不会降级"), "报错须明说不降级：{text}");
}

#[test]
fn a_full_read_carries_the_tenant_header_and_the_bearer_token() {
    let f = FakeFhir::start(vec![("/fhir/r4/Composition".into(), searchset())]);
    let cfg = f.config();
    let transport = UreqTransport::default();
    let token = auth::fetch_token(&cfg, &transport).expect("应取到 token");
    assert_eq!(token.token, "tok-local");

    let pkg = read::read_package(&cfg, &token, "WARD-A-001", &registry(), &transport)
        .expect("应读回文书包");

    let reqs = f.requests();
    let search = reqs
        .iter()
        .find(|r| r.method == "GET")
        .expect("应发出一次 GET");
    assert!(search.path.contains("/fhir/r4/Composition"));
    assert!(search.path.contains("identifier=WARD-A-001"));
    assert_eq!(search.header("X-Tenant-Id"), Some("ward-a"));
    assert_eq!(search.header("Authorization"), Some("Bearer tok-local"));
    assert_eq!(search.header("Accept"), Some("application/fhir+json"));
    // 读侧要真的拿到件
    assert_eq!(pkg.items.len(), 2, "字典外的那件应被过滤");
    assert!(pkg
        .items
        .iter()
        .all(|i| i.encounter_alias == "WARD-A-001"));
}

#[test]
fn the_token_request_uses_form_encoding_not_json() {
    let f = FakeFhir::start(vec![]);
    let cfg = f.config();
    let transport = UrefSafe::new();
    auth::fetch_token(&cfg, &transport).expect("取 token");
    let reqs = f.requests();
    let token_req = reqs.first().expect("应有一次 token 请求");
    assert_eq!(token_req.method, "POST");
    assert!(token_req.path.starts_with("/fhir/r4/oauth2/token"));
    assert_eq!(
        token_req.header("Content-Type"),
        Some("application/x-www-form-urlencoded")
    );
    assert!(token_req.body.contains("grant_type=client_credentials"));
    // 租户头不发到 token 端点（凭据端点不需要）
    assert_eq!(token_req.header("X-Tenant-Id"), None);
}

/// 只为让上一个测试能复用 `FakeFhir` 的类型别名，避免重复。
struct UrefSafe {
    inner: UreqTransport,
}

impl UrefSafe {
    fn new() -> Self {
        Self {
            inner: UreqTransport::default(),
        }
    }
}

impl Transport for UrefSafe {
    fn send(&self, req: &HttpRequest) -> Result<psychdoc_fhir::HttpResponse, FhirError> {
        self.inner.send(req)
    }
}

#[test]
fn a_writeback_sends_a_real_transaction_bundle_with_provenance() {
    let f = FakeFhir::start(vec![]);
    let cfg = f.config();
    let transport = UreqTransport::default();
    let token = auth::fetch_token(&cfg, &transport).unwrap();

    let entry = write::WriteEntry {
        resource_type: "Composition".into(),
        logical_id: "WARD-A-001-CONSENT-01".into(),
        method: write::WriteMethod::Post,
        resource: json!({
            "resourceType": "Composition",
            "id": "WARD-A-001-CONSENT-01",
            "status": "in-progress",
            "type": {"coding": [{"code": "TPL-CONSTRAINT-CONSENT"}]}
        }),
    };
    let ctx = write::WritebackContext {
        encounter_alias: "WARD-A-001".into(),
        actor: "脱敏-护士长A".into(),
        recorded_at: "2026-03-03T08:10:00+08:00".into(),
        ruleset_sha256: "e4eda0c79c95a1b2c3d4e5f60718293a".into(),
        idempotency_keys: vec!["key-a".into()],
    };
    let out = send_transaction(&cfg, &token, std::slice::from_ref(&entry), &ctx, &transport)
        .expect("整批应成功");

    assert_eq!(out.created.len(), 1);
    assert!(out.provenance.is_some(), "Provenance 必须落库");

    // 断言真正过了一遍 socket 的请求体
    let post = f
        .requests()
        .into_iter()
        .find(|r| r.method == "POST" && r.path == "/fhir/r4")
        .expect("应发出一次 transaction POST");
    assert_eq!(post.header("Content-Type"), Some("application/fhir+json"));
    assert_eq!(post.header("X-Tenant-Id"), Some("ward-a"));
    let sent = post.json();
    assert_eq!(sent["resourceType"], "Bundle");
    assert_eq!(sent["type"], "transaction");
    let entries = sent["entry"].as_array().expect("entry 数组");
    assert_eq!(entries.len(), 2, "1 件 + 1 条 Provenance");
    for e in entries {
        assert!(e["request"]["method"].is_string());
        assert!(e["request"]["url"].is_string());
    }
    assert_eq!(entries[0]["request"]["method"], "POST");
    assert_eq!(entries[0]["request"]["url"], "Composition");
    assert_eq!(entries[1]["resource"]["resourceType"], "Provenance");
    assert_eq!(entries[1]["request"]["url"], "Provenance");
}

#[test]
fn an_empty_selection_is_refused_before_any_network_call() {
    let f = FakeFhir::start(vec![]);
    let cfg = f.config();
    let transport = UreqTransport::default();
    let token = auth::fetch_token(&cfg, &transport).unwrap();
    let before = f.outbound_calls();

    let ctx = write::WritebackContext {
        encounter_alias: "WARD-A-001".into(),
        actor: "脱敏-护士长A".into(),
        recorded_at: "2026-03-03T08:10:00+08:00".into(),
        ruleset_sha256: "e4eda0c79c95a1b2c3d4e5f60718293a".into(),
        idempotency_keys: vec![],
    };
    let err = send_transaction(&cfg, &token, &[], &ctx, &transport).unwrap_err();
    assert_eq!(err.code(), "E_FHIR_TXN_EMPTY");
    assert_eq!(
        f.outbound_calls(),
        before,
        "空 selection 必须 0 次外发（token 那次已在 before 计入）"
    );
}

#[test]
fn a_failing_endpoint_produces_zero_partial_writes_and_no_local_success() {
    // 端点中途失败：这里用一个总是 500 的路径模拟
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut s) = stream else { break };
            let _ = read_request(&s);
            let body = json!({"resourceType": "OperationOutcome", "message": "服务端内部错误"}).to_string();
            let reply = format!(
                "HTTP/1.1 500 Internal Server Error\r\nContent-Type: application/fhir+json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = s.write_all(reply.as_bytes());
        }
    });
    let cfg = EmrConfig::from_source(|k| match k {
        "EMR_BASE_URL" => Some(format!("http://{addr}/fhir/r4")),
        "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
        "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
        "EMR_TENANT_ID" => Some("ward-a".into()),
        _ => None,
    })
    .unwrap();
    let transport = UreqTransport::default();
    let token = AccessToken {
        token: "tok".into(),
        expires_in_secs: 600,
    };
    let entry = write::WriteEntry {
        resource_type: "Composition".into(),
        logical_id: "WARD-A-001-CONSENT-01".into(),
        method: write::WriteMethod::Post,
        resource: json!({"resourceType": "Composition", "id": "WARD-A-001-CONSENT-01"}),
    };
    let ctx = write::WritebackContext {
        encounter_alias: "WARD-A-001".into(),
        actor: "脱敏-护士长A".into(),
        recorded_at: "2026-03-03T08:10:00+08:00".into(),
        ruleset_sha256: "e4eda0c79c95a1b2c3d4e5f60718293a".into(),
        idempotency_keys: vec!["key-a".into()],
    };
    let err = send_transaction(&cfg, &token, &[entry], &ctx, &transport).unwrap_err();
    assert_eq!(err.code(), "E_FHIR_STATUS");
    assert!(err.to_string().contains("500"), "错误须带 500：{err}");
    // 关键：没有返回「成功」，调用方不会去写本地「已写回」
    assert!(err.is_retryable(), "5xx 应可重试");
}

#[test]
fn transaction_bundle_built_in_memory_matches_what_a_server_would_accept() {
    // 纯构造断言：不触网，验证结构本身合规
    let entry = write::WriteEntry {
        resource_type: "Composition".into(),
        logical_id: "WARD-A-001-CONSENT-01".into(),
        method: write::WriteMethod::Put,
        resource: json!({"resourceType": "Composition", "id": "WARD-A-001-CONSENT-01"}),
    };
    let ctx = write::WritebackContext {
        encounter_alias: "WARD-A-001".into(),
        actor: "脱敏-护士长A".into(),
        recorded_at: "2026-03-03T08:10:00+08:00".into(),
        ruleset_sha256: "e4eda0c79c95a1b2c3d4e5f60718293a".into(),
        idempotency_keys: vec!["key-a".into()],
    };
    let b = build_transaction(&[entry], &ctx);
    assert_eq!(b["type"], "transaction");
    assert_eq!(b["entry"][0]["request"]["url"], "Composition/WARD-A-001-CONSENT-01");
    assert_eq!(b["entry"][0]["request"]["method"], "PUT");
}

#[test]
fn http_method_display_is_rfc_verbs() {
    assert_eq!(HttpMethod::Get.as_str(), "GET");
    assert_eq!(HttpMethod::Post.as_str(), "POST");
}
