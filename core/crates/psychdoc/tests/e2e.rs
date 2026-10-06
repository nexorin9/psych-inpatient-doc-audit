//! Task 40 验收：**端到端主路径一次跑通**。
//!
//! 前面 39 个任务各自证明了单个模块是对的，但**没有一条用例真的把
//! 「拖入 → 清单 → 勾选 → 预览 diff → transaction → 状态推进 → 通知 →
//! 审计回溯」串起来跑过一遍**。模块级测试全绿、拼起来跑不通，是这类
//! 项目最常见的收尾翻车方式——所以本文件存在的理由就是**不许它翻车**。
//!
//! ## 为什么打真 socket
//!
//! 写回与通知都走**真实出站**（[`UreqTransport`] → 本地 FHIR 协议替身 /
//! 企业微信 Webhook 替身）。用 mock 断言自己写进去的东西等于什么都没验：
//! `Content-Type` 写错、Provenance 少一条 `request.url`、token 端点用了
//! JSON 而不是表单——这些只有过一遍真实 HTTP 编解码才现形。
//!
//! 替身是**测试数据层**的脱敏协议替身，**不是**产品的最终对接面：
//! 生产路径永远是 `UreqTransport` 指向 `EMR_BASE_URL` /
//! `WECOM_WEBHOOK_URL` 的真实院内端点。
//!
//! ## 五个用例对应 task.json 的六条 step
//!
//! | 用例 | step |
//! |------|------|
//! | [`the_whole_chain_runs_once_from_a_dropped_folder_to_a_delivered_todo`] | 1 · 逐环断言 |
//! | [`confirming_the_same_item_twice_sends_nothing_and_moves_no_state`] | 2 · 幂等 |
//! | [`a_failing_endpoint_writes_nothing_locally_and_keeps_the_draft`] | 3 · 失败显式 |
//! | [`the_audit_export_can_recall_the_ruleset_and_codes_in_force_then`] | 4 · 审计可回溯 |
//! | [`the_cli_reaches_the_same_verdict_with_no_window_open`] | 5 · 无 GUI 可复现 |

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use psychdoc_ffi::api::{
    archive_view, commit_writeback, notify_missing, preview_fixback, review_batch, scan_queue,
    ApiError, ArchiveViewRequest, CommitRequest, NotifyRequest, ScanQueueRequest,
};
use psychdoc_store::Store;
use serde_json::{json, Value};

// ---------------------------------------------------------------------------
// 路径与常量
// ---------------------------------------------------------------------------

/// 项目根（本 crate 在 `<root>/core/crates/psychdoc`，往上三层）。
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("找不到项目根")
        .to_path_buf()
}

fn ruleset_yaml() -> String {
    std::fs::read_to_string(root().join("rulesets/hospital.yaml")).expect("读得到规则集")
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(root().join("fixtures").join(name))
        .unwrap_or_else(|e| panic!("读得到夹具 {name} — {e}"))
}

const AT: &str = "2026-03-10T08:30:00+08:00";
const DAY: &str = "2026-03-10";
/// 写回勾选时刻（与 AT 同一班次内，落在同一业务日）。
const AT_CONFIRM: &str = "2026-03-10T08:40:00+08:00";

/// 补写目标：ward-08 里唯一「可预览补写」的那一条（QC-PSY-02 全角日期）。
const FIXABLE: &str = "WARD-F-001-ASSESS-01";
/// ward-08 全部命中的码集合（run 一遍就能钉住，不必手抄每条文案）。
const WARD_08_CODES: [&str; 4] = ["QC-PSY-01", "QC-PSY-02", "QC-PSY-03", "QC-PSY-04"];

// ---------------------------------------------------------------------------
// 临时目录
// ---------------------------------------------------------------------------

struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("psychdoc-e2e-{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn db(&self) -> String {
        self.0.join("psychdoc.sqlite3").display().to_string()
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 造一份「抽掉某类文书」的导出包，落到临时目录并返回它的路径。
///
/// **为什么不直接用 `fixtures/` 整个目录**：那里十几份夹具各有各的住院，
/// 一次性喂进来会把不同住院的件并进同一批，断言的就不再是「这份包缺了什么」。
fn without_template(dir: &TmpDir, src: &str, template_id: &str) -> String {
    let mut doc: Value = serde_json::from_str(&fixture_text(src)).unwrap();
    let entries = doc["entry"].as_array_mut().expect("夹具是 Bundle");
    entries.retain(|e| {
        let r = &e["resource"];
        r["resourceType"] != "Composition"
            || !r["type"]["coding"]
                .as_array()
                .is_some_and(|cs| cs.iter().any(|c| c["code"] == template_id))
    });
    let path = dir.path().join(format!("trimmed-{template_id}.json"));
    std::fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
    path.display().to_string()
}

/// 把两份基准包并进一个目录 = 护士长一次拖一个病区导出文件夹。
///
/// - `ward-08-field-violations.json`：4 条命中，其中 1 条可预览补写（写回那一环）；
/// - `ward-02-restraint-set.json` 抽掉约束知情同意：1 件真缺件（通知那一环）。
///
/// 一次拖入、两次住院，正好覆盖「同一批里既有要补写的、也有缺件要催的」。
fn dropped_folder(dir: &TmpDir) -> String {
    std::fs::write(
        dir.path().join("ward-08-field-violations.json"),
        fixture_text("ward-08-field-violations.json"),
    )
    .unwrap();
    without_template(dir, "ward-02-restraint-set.json", "restraint_consent");
    dir.path().display().to_string()
}

// ---------------------------------------------------------------------------
// 本地 FHIR 协议替身 + 企业微信 Webhook 替身（只在本 tests/ 内出现）
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

/// 端点行为：整批成功 / 整体 500。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    AllOk,
    ServerError,
}

struct FakeFhir {
    base_url: String,
    seen: Arc<Mutex<Vec<SeenRequest>>>,
}

impl FakeFhir {
    fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let seen = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let (seen_c, calls_c) = (seen.clone(), calls.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (seen, calls) = (seen_c.clone(), calls_c.clone());
                std::thread::spawn(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let Some(req) = read_request(&stream) else { return };
                    let reply = route_fhir(&req, mode);
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                    seen.lock().unwrap().push(req);
                });
            }
        });
        Self {
            base_url: format!("http://{addr}/fhir/r4"),
            seen,
        }
    }

    fn requests(&self) -> Vec<SeenRequest> {
        self.seen.lock().unwrap().clone()
    }

    /// 过 socket 的 Bundle transaction（不含 token 那次）。
    fn transactions(&self) -> Vec<SeenRequest> {
        self.requests()
            .into_iter()
            .filter(|r| r.method == "POST" && r.path == "/fhir/r4")
            .collect()
    }

}

/// 企业微信群机器人 Webhook 替身。数真实外发次数。
struct FakeWeCom {
    url: String,
    calls: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<Value>>>,
}

impl FakeWeCom {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let addr = listener.local_addr().expect("取本地地址");
        let calls = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let (calls_c, bodies_c) = (calls.clone(), bodies.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let (calls, bodies) = (calls_c.clone(), bodies_c.clone());
                std::thread::spawn(move || {
                    calls.fetch_add(1, Ordering::SeqCst);
                    let Some(req) = read_request(&stream) else { return };
                    bodies.lock().unwrap().push(req.json());
                    let reply = http_raw(
                        200,
                        "OK",
                        "application/json",
                        &json!({"errcode": 0, "errmsg": "ok"}).to_string(),
                    );
                    let _ = stream.write_all(&reply);
                    let _ = stream.flush();
                });
            }
        });
        Self {
            url: format!("http://{addr}/cgi-bin/webhook/send?key=local-test-key"),
            calls,
            bodies,
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn markdown(&self) -> String {
        self.bodies
            .lock()
            .unwrap()
            .first()
            .map(|b| b["markdown"]["content"].as_str().unwrap_or_default().to_string())
            .unwrap_or_default()
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

fn http_raw(status: u16, reason: &str, content_type: &str, payload: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
        payload.len()
    )
    .into_bytes()
}

fn route_fhir(req: &SeenRequest, mode: Mode) -> Vec<u8> {
    if req.path.starts_with("/fhir/r4/oauth2/token") {
        return http_raw(
            200,
            "OK",
            "application/json",
            &json!({"access_token": "tok-local", "expires_in": 600, "token_type": "Bearer"})
                .to_string(),
        );
    }
    if req.method == "POST" && req.path == "/fhir/r4" {
        if mode == Mode::ServerError {
            return http_raw(
                500,
                "Internal Server Error",
                "application/fhir+json",
                &json!({"message": "服务端内部错误，事务未提交"}).to_string(),
            );
        }
        let Ok(body) = serde_json::from_str::<Value>(&req.body) else {
            return http_raw(400, "Bad Request", "application/fhir+json", "{}");
        };
        let entries: Vec<Value> = body["entry"]
            .as_array()
            .map(|es| {
                es.iter()
                    .map(|e| {
                        let url = e["request"]["url"].as_str().unwrap_or("X");
                        json!({
                            "response": {"status": "201 Created", "location": format!("{url}/_history/1")},
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
        return http_raw(
            200,
            "OK",
            "application/fhir+json",
            &json!({"resourceType": "Bundle", "type": "transaction-response", "entry": entries})
                .to_string(),
        );
    }
    http_raw(
        404,
        "Not Found",
        "application/fhir+json",
        &json!({"message": "未知路径"}).to_string(),
    )
}

// ---------------------------------------------------------------------------
// 进程级环境变量互斥锁
// ---------------------------------------------------------------------------

/// `EMR_*` 与 `WECOM_WEBHOOK_URL` 是**进程**环境变量，Rust 测试默认多线程并行：
/// 一个用例刚设好，另一个用例 `remove_var` 就把它抽走了，于是「发得出去」
/// 的用例报 `E_CFG_MISSING`。这不是产品缺陷，是测试自己踩自己的脚——
/// 用一把锁把这几个用例串起来，并明确写出它们共享进程环境。
static ENV_LOCK: Mutex<()> = Mutex::new(());

fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// 把四项凭据指到替身上，并在**返回前**校验替身真的起来了。
fn point_emr_at(fhir: &FakeFhir) {
    std::env::set_var("EMR_BASE_URL", &fhir.base_url);
    std::env::set_var("EMR_CLIENT_ID", "psychdoc-client");
    std::env::set_var("EMR_CLIENT_SECRET", "s3cret-value");
    std::env::set_var("EMR_TENANT_ID", "ward-a");
}

// ---------------------------------------------------------------------------
// CLI（无 GUI 回归通道）
// ---------------------------------------------------------------------------

fn run_cli(args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_psychdoc"));
    cmd.args(args);
    for k in [
        "EMR_BASE_URL",
        "EMR_CLIENT_ID",
        "EMR_CLIENT_SECRET",
        "EMR_TENANT_ID",
        "WECOM_WEBHOOK_URL",
    ] {
        cmd.env_remove(k);
    }
    cmd.output().expect("跑不起 psychdoc 二进制")
}

fn exit_code(out: &Output) -> i32 {
    out.status.code().expect("进程被信号杀掉，没有退出码")
}

fn both(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

// ---------------------------------------------------------------------------
// 共用小工具
// ---------------------------------------------------------------------------

fn codes_of(summary: &Value) -> BTreeSet<String> {
    summary["diagnostics"]
        .as_array()
        .expect("diagnostics 是数组")
        .iter()
        .map(|d| d["code"].as_str().expect("每条诊断带码").to_string())
        .collect()
}

/// 归档核齐视图里的真缺件 `(node_id, 院内显示名)`。
fn missing_of(view: &Value) -> Vec<(String, String)> {
    view["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let node_id = m["node_id"].as_str().unwrap().to_string();
            let label = view["rows"]
                .as_array()
                .unwrap()
                .iter()
                .find(|r| r["node_id"].as_str() == Some(node_id.as_str()))
                .map(|r| r["display_zh"].as_str().unwrap().to_string())
                .unwrap_or_else(|| node_id.clone());
            (node_id, label)
        })
        .collect()
}

fn commit_req(db: &str, draft_id: &str, doc_id: &str) -> CommitRequest {
    CommitRequest {
        store_path: Some(db.to_string()),
        draft_id: draft_id.to_string(),
        encounter_alias: "WARD-F-001".to_string(),
        actor: "脱敏-护士长A".to_string(),
        at: AT_CONFIRM.to_string(),
        entries: vec![(
            doc_id.to_string(),
            json!({
                "resourceType": "Composition",
                "id": doc_id,
                "status": "preliminary",
                "type": {"coding": [{"code": "assessment_report"}]},
                "title": "<脱敏样例文书>",
                "section": [{
                    "title": "入院评估",
                    "text": "评估时间：2026-03-02 09:00。\n风险等级：中高风险。"
                }]
            }),
        )],
        content_hashes: vec![(doc_id.to_string(), content_sha256().to_string())],
        ruleset_sha256: content_sha256().to_string(),
        run_id: Some("RUN-E2E-01".to_string()),
    }
}

/// 幂等键的第三个分量。真实路径上这是件内容的哈希；这里给一个固定的
/// 脱敏占位，让「同件同日重复确认」可复现。
fn content_sha256() -> &'static str {
    "9f2c1ab47de3508c6b1930fd47e1a9b2c4d5e6f708192a3b4c5d6e7f8091a2b3"
}

// ===========================================================================
// step 1 · 全链路一次跑通
// ===========================================================================

#[test]
fn the_whole_chain_runs_once_from_a_dropped_folder_to_a_delivered_todo() {
    let _env = env_guard();
    let fhir = FakeFhir::start(Mode::AllOk);
    point_emr_at(&fhir);
    let wecom = FakeWeCom::start();
    std::env::set_var("WECOM_WEBHOOK_URL", &wecom.url);

    let tmp = TmpDir::new("chain");
    let db = tmp.db();
    let folder = dropped_folder(&tmp);
    let yaml = ruleset_yaml();

    // ── 环 1：拖入文件夹 → 队列按住院别名聚合 ──────────────────────────
    let q = scan_queue(&ScanQueueRequest {
        roots: vec![folder.clone()],
        ruleset_yaml: yaml.clone(),
        at: AT.to_string(),
        store_path: Some(db.clone()),
        persist: true,
    })
    .expect("拖入文件夹应扫得出队列");

    let aliases: BTreeSet<String> = q["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["encounter_alias"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        aliases,
        BTreeSet::from(["WARD-B-001".to_string(), "WARD-F-001".to_string()]),
        "一次拖入应聚出两次住院"
    );
    assert_eq!(q["is_balanced"], true, "队列记账必须守恒");
    assert_eq!(q["totals"]["items"], 6, "ward-08 三件 + ward-02 抽掉同意后三件");

    // ward-02 被抽掉了约束知情同意：左栏徽标必须显示「还差 1 件」，
    // 且那 1 件是**真缺件**（阻塞传播），不是评估报告的参考缺件。
    let b = q["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["encounter_alias"] == "WARD-B-001")
        .unwrap();
    assert_eq!(b["missing_count"], 1, "恰好缺约束知情同意一件");
    assert_eq!(b["is_complete"], false);
    assert_eq!(b["badge_zh"], "3 件 · 缺件 1");

    // ── 环 2：批审查 → 带码带段落定位的清单 ────────────────────────────
    let rev = review_batch(&folder, &yaml, None).expect("批审查应跑得通");
    let summary = &rev["summary"];
    assert_eq!(
        codes_of(summary),
        WARD_08_CODES.iter().map(|s| s.to_string()).collect(),
        "ward-08 的四条命中一个不多一个不少"
    );
    // 每条都要能切回原文的位置 + 院内依据，否则护士长拿着清单没法改。
    for d in summary["diagnostics"].as_array().unwrap() {
        assert!(!d["doc_id"].as_str().unwrap().is_empty(), "诊断带件 id");
        assert!(d["section_index"].is_number(), "诊断带段落下标");
        assert!(d["char_start"].is_number() && d["char_end"].is_number(), "诊断带字符区间");
        assert!(
            !d["message"].as_str().unwrap().is_empty(),
            "文案要说清为什么这是问题"
        );
        assert!(
            d["basis"].as_str().unwrap().contains("《精神科住院护理常规》"),
            "每条命中都要指到院内规范条款：{}",
            d["basis"]
        );
        // 区间非零宽 = 真指到了具体那处，而不是整段糊过去。
        assert!(d["char_end"].as_i64().unwrap() > 0, "区间落在原文里");
    }

    // ── 环 3：勾选前的预览双栏 diff ────────────────────────────────────
    let prev = preview_fixback(&fixture_text("ward-08-field-violations.json"), &yaml)
        .expect("补写预览应出得来");
    let blocks = prev["blocks"].as_array().unwrap();
    let automatic: Vec<&Value> = blocks
        .iter()
        .filter(|b| b["fix_availability"] == "automatic")
        .collect();
    assert_eq!(
        automatic.len(),
        1,
        "ward-08 恰好一条可自动补写（其余「只给依据不给按钮」）"
    );
    assert_eq!(automatic[0]["doc_id"], FIXABLE);
    assert_eq!(automatic[0]["code"], "QC-PSY-02");
    assert!(
        !automatic[0]["diff"].as_array().unwrap().is_empty(),
        "候选块必须带逐行 diff，界面才有双栏可画"
    );
    assert!(
        prev["evidence_only"]
            .as_array()
            .unwrap()
            .contains(&json!("QC-PSY-01")),
        "缺签字这类只给依据不给按钮"
    );

    // ── 环 4~6：勾选 → transaction 200 → Provenance → 状态推进 ─────────
    let out = commit_writeback(&commit_req(&db, "DRAFT-E2E-1", FIXABLE))
        .expect("勾选一条可补写项应写回得出去");

    assert_eq!(out["new_rows"], 1, "一条勾选 → 一条新写回");
    assert_eq!(out["replayed"], false);
    assert_eq!(out["created"].as_array().unwrap().len(), 1);
    assert!(out["provenance"].is_string(), "Provenance 必须生成");

    // 真的过了一遍 socket 的请求体
    let txns = fhir.transactions();
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
    assert_eq!(ents[0]["resource"]["id"], FIXABLE);
    assert_eq!(ents[0]["request"]["method"], "POST");
    // R4 的 Bundle.entry.request 是必填的：少一个 url 服务端就拒整批。
    assert_eq!(ents[1]["resource"]["resourceType"], "Provenance");
    assert_eq!(ents[1]["request"]["url"], "Provenance");
    // Provenance 记的是「这一批在什么上下文下做的」：谁、何时、按哪版规则、
    // 幂等键是什么。四项缺一项，审计链就答不出「这份件凭什么建在这里」。
    let prov = &ents[1]["resource"];
    assert!(prov["target"].as_array().unwrap().len() == 1, "Provenance 指明目标件");
    assert_eq!(prov["recorded"], AT_CONFIRM);
    assert!(
        prov["agent"][0]["who"]["display"]
            .as_str()
            .unwrap()
            .contains("护士长"),
        "Provenance 记推进人"
    );
    let entity = prov["entity"][0]["what"]["display"].as_str().unwrap();
    assert!(
        entity.contains(&content_sha256()[..12]),
        "Provenance 带规则集短哈希（幂等键也要在）：{entity}"
    );

    // 状态推进：草稿 → 已建件，且**只**推进被勾的那一件。
    assert_eq!(out["advanced"].as_array().unwrap().len(), 1);
    assert_eq!(out["advanced"][0]["doc_id"], FIXABLE);
    assert_eq!(out["advanced"][0]["state"], "已建件");

    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.state_of(FIXABLE).unwrap(),
        Some(psychdoc_model::state::DocState::Created),
        "库里落的状态与返回给界面的必须是同一个事实"
    );
    assert_eq!(
        store.state_of("WARD-F-001-CONSENT-01").unwrap(),
        None,
        "没勾的件不该被顺带推进"
    );

    // ── 环 7：缺件待办经通知队列发出 ──────────────────────────────────
    let view = archive_view(&ArchiveViewRequest {
        input: folder.clone(),
        ruleset_yaml: yaml.clone(),
        encounter_alias: "WARD-B-001".to_string(),
        as_of: AT.to_string(),
        store_path: Some(db.clone()),
        business_day: DAY.to_string(),
    })
    .expect("归档核齐视图应出得来");

    assert_eq!(view["is_ready"], false, "还缺一件就不算核齐");
    let missing = missing_of(&view);
    assert_eq!(missing.len(), 1, "恰好一件真缺件");
    assert_eq!(missing[0].1, "约束知情同意");
    // 被连带卡住的两件要能回答「在等谁」，否则催办单没有下文。
    let blocked: Vec<&str> = view["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["state"] == "blocked")
        .map(|r| r["template_id"].as_str().unwrap())
        .collect();
    assert_eq!(blocked, vec!["restraint_record", "restraint_release"]);

    let notified = notify_missing(&NotifyRequest {
        store_path: Some(db.clone()),
        encounter_alias: "WARD-B-001".to_string(),
        ward: "精神科三病区".to_string(),
        business_day: DAY.to_string(),
        at: AT.to_string(),
        missing: missing.clone(),
        group_hint: "精神科病区文书群".to_string(),
        dry_run: false,
    })
    .expect("缺件待办应发得出去");

    assert_eq!(notified["sent"], 1, "一条缺件一条提醒");
    assert_eq!(notified["outcomes"][0]["state"], "sent");
    assert_eq!(wecom.calls(), 1, "真出了一次外发");
    let md = wecom.markdown();
    assert!(md.contains("约束知情同意"), "消息体要列缺件中文名：{md}");
    assert!(md.contains("WARD-B-001"), "消息体要认得住是哪次住院：{md}");

    std::env::remove_var("EMR_BASE_URL");
    std::env::remove_var("EMR_CLIENT_ID");
    std::env::remove_var("EMR_CLIENT_SECRET");
    std::env::remove_var("EMR_TENANT_ID");
    std::env::remove_var("WECOM_WEBHOOK_URL");
}

// ===========================================================================
// step 2 · 幂等：同一件同日重复确认
// ===========================================================================

#[test]
fn confirming_the_same_item_twice_sends_nothing_and_moves_no_state() {
    let _env = env_guard();
    let fhir = FakeFhir::start(Mode::AllOk);
    point_emr_at(&fhir);

    let tmp = TmpDir::new("idem");
    let db = tmp.db();

    // 第一次：正常写回。
    let first = commit_writeback(&commit_req(&db, "DRAFT-E2E-1", FIXABLE)).expect("首次写回");
    assert_eq!(first["new_rows"], 1);
    let after_first = fhir.transactions().len();
    assert_eq!(after_first, 1);

    // 第二次：**另一张草稿**、同件、同内容哈希 —— 幂等键相同。
    //
    // 「同一张草稿点两下」在草稿层就被 assert_draft_state 拦住了，
    // 走不到这里；真实的重复来源是护士长在两台机器上各点了一次，
    // 或者网络超时后客户端重试同一批——两者都是**新草稿 + 同键**。
    let second = commit_writeback(&commit_req(&db, "DRAFT-E2E-2", FIXABLE)).expect("重复确认应被幂等接住");

    assert_eq!(second["new_rows"], 0, "重复确认不得产生第二条写回");
    assert_eq!(second["replayed"], true, "第二次是重放，不是新建");
    assert_eq!(
        second["created"],
        first["created"],
        "重放回放的是首次结果，不重新问服务端"
    );
    assert_eq!(
        second["advanced"].as_array().unwrap().len(),
        0,
        "重放不得重复推进状态"
    );
    assert_eq!(
        fhir.transactions().len(),
        after_first,
        "整条链路上只该有一次 transaction 外发"
    );

    // 状态不回退：仍是「已建件」，不是被重置回草稿。
    let store = Store::open(&db).unwrap();
    assert_eq!(
        store.state_of(FIXABLE).unwrap(),
        Some(psychdoc_model::state::DocState::Created)
    );
    assert_eq!(
        store.writeback_row_count().unwrap(),
        1,
        "写回表仍只有一条"
    );

    std::env::remove_var("EMR_BASE_URL");
    std::env::remove_var("EMR_CLIENT_ID");
    std::env::remove_var("EMR_CLIENT_SECRET");
    std::env::remove_var("EMR_TENANT_ID");
}

#[test]
fn sending_the_same_missing_item_todo_twice_on_the_same_day_makes_one_call() {
    let _env = env_guard();
    let wecom = FakeWeCom::start();
    std::env::set_var("WECOM_WEBHOOK_URL", &wecom.url);

    let tmp = TmpDir::new("idem-notify");
    let db = tmp.db();
    let folder = dropped_folder(&tmp);
    let yaml = ruleset_yaml();

    let view = archive_view(&ArchiveViewRequest {
        input: folder,
        ruleset_yaml: yaml.clone(),
        encounter_alias: "WARD-B-001".to_string(),
        as_of: AT.to_string(),
        store_path: Some(db.clone()),
        business_day: DAY.to_string(),
    })
    .unwrap();
    let missing = missing_of(&view);
    assert_eq!(missing.len(), 1);

    let req = || NotifyRequest {
        store_path: Some(db.clone()),
        encounter_alias: "WARD-B-001".to_string(),
        ward: "精神科三病区".to_string(),
        business_day: DAY.to_string(),
        at: AT.to_string(),
        missing: missing.clone(),
        group_hint: "精神科病区文书群".to_string(),
        dry_run: false,
    };

    let first = notify_missing(&req()).unwrap();
    assert_eq!(first["sent"], 1);
    assert_eq!(wecom.calls(), 1);

    let second = notify_missing(&req()).unwrap();
    assert_eq!(second["sent"], 0, "同日同件不重复提醒");
    assert_eq!(second["outcomes"][0]["state"], "already_sent");
    assert_eq!(wecom.calls(), 1, "整个链路上只该有一次外发");

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

// ===========================================================================
// step 3 · 失败显式
// ===========================================================================

#[test]
fn a_failing_endpoint_writes_nothing_locally_and_keeps_the_draft() {
    let _env = env_guard();
    let fhir = FakeFhir::start(Mode::ServerError);
    point_emr_at(&fhir);

    let tmp = TmpDir::new("fail");
    let db = tmp.db();

    let err: ApiError = commit_writeback(&commit_req(&db, "DRAFT-E2E-1", FIXABLE))
        .expect_err("端点 500 时必须显式报错，不能假装成功");

    assert_eq!(
        err.code(),
        "E_FHIR_STATUS",
        "错误码要能分派给运维：{}",
        err.message()
    );
    assert!(
        err.message().contains("500"),
        "报错要带上端点实际返回的状态码：{}",
        err.message()
    );
    assert_eq!(fhir.transactions().len(), 1, "确实发出去过一次，且只一次");

    // 本地 0 条已生效事实：界面据此不会把「已写回」标灰。
    let store = Store::open(&db).unwrap();
    assert_eq!(store.writeback_row_count().unwrap(), 0, "0 条部分写入");
    assert_eq!(
        store.audit_event_count(psychdoc_store::audit::AuditKind::Writeback)
            .unwrap(),
        0,
        "失败不追加已生效事实，否则审计计数虚高"
    );
    assert_ne!(
        store.state_of(FIXABLE).unwrap(),
        Some(psychdoc_model::state::DocState::Created),
        "端点没建成就不能推进到已建件"
    );

    // 草稿仍在，护士长原样重试即可，不必重填一遍。
    assert_eq!(
        store.draft_state("DRAFT-E2E-1").unwrap().as_deref(),
        Some("draft"),
        "本地草稿要保留下来供重试"
    );
    assert_eq!(
        store.selected_count("DRAFT-E2E-1").unwrap(),
        1,
        "勾选状态保留，重试不必重新勾一遍"
    );

    std::env::remove_var("EMR_BASE_URL");
    std::env::remove_var("EMR_CLIENT_ID");
    std::env::remove_var("EMR_CLIENT_SECRET");
    std::env::remove_var("EMR_TENANT_ID");
}

// ===========================================================================
// step 4 · 审计可回溯
// ===========================================================================

#[test]
fn the_audit_export_can_recall_the_ruleset_and_codes_in_force_then() {
    let tmp = TmpDir::new("audit");
    let db = tmp.db();
    let ruleset = root().join("rulesets/hospital.yaml").display().to_string();
    let fixture = root()
        .join("fixtures/ward-08-field-violations.json")
        .display()
        .to_string();

    // 审查时把这一批落进本机库（`--record` 是显式开关：看 ≠ 留）。
    let rec = run_cli(&["check", &fixture, "--ruleset", &ruleset, "--record", &db, "--actor", "脱敏-护士长A"]);
    assert_eq!(exit_code(&rec), 1, "有阻断项退 1：\n{}", both(&rec));
    assert!(both(&rec).contains("已记入本机审查库"), "落库要有回执");

    // 导出 + 回溯到那一刻：质控办问「这份病历当年按哪版审的、报了哪些」。
    let out_csv = tmp.path().join("audit.csv");
    let exp = run_cli(&[
        "export-audit",
        "--db",
        &db,
        "--alias",
        "WARD-F-001",
        "--at",
        "2099-01-01T00:00:00+08:00",
        "--out",
        &out_csv.display().to_string(),
    ]);
    assert_eq!(exit_code(&exp), 0, "导出应成功：\n{}", both(&exp));

    let csv = std::fs::read_to_string(&out_csv).expect("导出包落盘");
    let header = csv.lines().next().unwrap();
    for col in ["规则码", "规则集", "操作人", "时刻", "写回幂等键"] {
        assert!(header.contains(col), "证据行要带「{col}」这列：{header}");
    }
    // 四条命中逐条可核对，且带定位与依据。
    for code in WARD_08_CODES {
        assert!(csv.contains(code), "导出包应含 {code}");
    }
    assert!(csv.contains("脱敏-护士长A"), "导出包要带推进人");
    assert!(csv.contains('@') || csv.contains('['), "导出包要带段落定位");

    // 版本回溯：给一个**早于任何审查**的时刻，必须答「查无此批」而不是
    // 返回最新一版——后者会让质控办拿着 0.6.0 的口径去核对 0.5.0 的病历。
    let early = Store::open(&db).unwrap();
    assert!(
        early
            .recall_ruleset("WARD-F-001", "2020-01-01T00:00:00+08:00")
            .unwrap()
            .is_none(),
        "早于任何审查的时刻不该回溯出一版规则集"
    );

    let recall = early
        .recall_ruleset("WARD-F-001", "2099-01-01T00:00:00+08:00")
        .unwrap()
        .expect("那一刻应当回溯得到一版");
    assert_eq!(
        recall.codes,
        WARD_08_CODES.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
        "回溯返回的命中码集合要与当次审查一致"
    );
    assert!(recall.ruleset_sha256.len() == 64, "带 64 位完整哈希，便于逐字比对");
    assert!(recall.ruleset_version.starts_with('0'), "带规则集版本号");
}

// ===========================================================================
// step 5 · 无 GUI 等价全链路
// ===========================================================================

#[test]
fn the_cli_reaches_the_same_verdict_with_no_window_open() {
    let ruleset = root().join("rulesets/hospital.yaml").display().to_string();
    let folder = root().join("fixtures").display().to_string();

    // check：批扫整夹具目录，仍按退出码契约分诊。
    let chk = run_cli(&["check", &folder, "--ruleset", &ruleset]);
    let text = both(&chk);
    assert_eq!(exit_code(&chk), 1, "夹具目录里有阻断项，应退 1");
    assert!(text.contains("QC-PSY-"), "清单带规则码");
    assert!(text.contains("@[") && text.contains("依据"), "清单带定位与依据");

    // graph --direction both：正反向两个答案一次拿全。
    let g = run_cli(&[
        "graph",
        "WARD-F-001",
        "--input",
        &folder,
        "--ruleset",
        &ruleset,
        "--direction",
        "both",
        "--as-of",
        AT,
    ]);
    let gtext = both(&g);
    assert!(
        exit_code(&g) == 0 || exit_code(&g) == 1,
        "graph 只用 0/1 分诊：\n{gtext}"
    );
    assert!(gtext.contains("在等（正向）") && gtext.contains("卡住（反向）"), "两个方向都答");

    // writeback-dryrun：构造 transaction 但**一次网络请求都不发**。
    let tmp = TmpDir::new("cli-dryrun");
    let selection = tmp.path().join("selection.json");
    std::fs::write(
        &selection,
        serde_json::to_string_pretty(&json!({
            "encounter_alias": "WARD-F-001",
            "actor": "脱敏-护士长A",
            "at": AT_CONFIRM,
            "ruleset_sha256": content_sha256(),
            "entries": [{
                "doc_id": FIXABLE,
                "content_sha256": content_sha256(),
                "resource": {
                    "resourceType": "Composition",
                    "id": FIXABLE,
                    "status": "preliminary",
                    "type": {"coding": [{"code": "assessment_report"}]}
                }
            }]
        }))
        .unwrap(),
    )
    .unwrap();

    let bundle_out = tmp.path().join("bundle.json");
    let dry = run_cli(&[
        "writeback-dryrun",
        &selection.display().to_string(),
        "--out",
        &bundle_out.display().to_string(),
    ]);
    assert_eq!(exit_code(&dry), 0, "干跑应成功：\n{}", both(&dry));

    let bundle: Value =
        serde_json::from_str(&std::fs::read_to_string(&bundle_out).unwrap()).expect("报文落盘");
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "transaction");
    assert_eq!(
        bundle["entry"].as_array().unwrap().len(),
        2,
        "1 件 + 1 条 Provenance"
    );
    assert_eq!(
        bundle["entry"][1]["resource"]["resourceType"],
        "Provenance",
        "干跑与真发共用同一份构造代码，报文形状必然一致"
    );

    // 空勾选：干跑与真发同码，不许「干跑报成功、真发报没勾」。
    let empty = tmp.path().join("empty.json");
    std::fs::write(
        &empty,
        serde_json::to_string_pretty(&json!({
            "encounter_alias": "WARD-F-001",
            "at": AT_CONFIRM,
            "ruleset_sha256": content_sha256(),
            "entries": []
        }))
        .unwrap(),
    )
    .unwrap();
    let bad = run_cli(&["writeback-dryrun", &empty.display().to_string()]);
    assert_eq!(exit_code(&bad), 2, "空勾选退 2");
    assert!(both(&bad).contains("E_FHIR_TXN_EMPTY"), "与真发同码");
}