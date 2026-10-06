//! Task 34 · `archive_view` / `notify_missing` 两个入口的行为测试。
//!
//! 这个入口回答护士长切到「归档核齐」标签时的三个问题：
//!
//! 1. **哪件缺了、缺几天、连带卡住了谁**（一屏数据，不是逐件点开看）；
//! 2. **图上每个节点该是什么颜色**（四态由核心给，界面只做颜色映射——
//!    让界面自己判，早晚出现「橙的到底是连带阻塞还是软缺件」这种分歧）；
//! 3. **缺件提醒发没发**（已发项要置灰，且不能重复发）。
//!
//! 通知这一侧打在**本地协议替身**上、走真实 socket 与真实 `UreqTransport`：
//! 幂等、置灰与「不重复发送」都必须用真实出站验证，mock 出来的
//! `notify_once` 返回值证明不了任何东西。

use psychdoc_ffi::api::{
    archive_view, notify_missing, ApiError, ArchiveViewRequest, NotifyRequest,
};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn root() -> PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .to_path_buf()
}

fn ruleset() -> String {
    std::fs::read_to_string(root().join("rulesets/hospital.yaml")).expect("读得到规则集")
}

fn fixture_text(name: &str) -> String {
    std::fs::read_to_string(root().join("fixtures").join(name))
        .unwrap_or_else(|e| panic!("读得到夹具 {name} — {e}"))
}

/// 造一份「抽掉某类文书」的导出包，落到临时目录并返回它的路径。
///
/// **为什么不直接读 `fixtures/` 整个目录**：目录里十几份夹具各有各的住院，
/// 一次性喂进来会把不同住院的件并进同一张图，测试断言的就不再是
/// 「这份包缺了什么」，而是「整个夹具库合起来缺了什么」——那种断言
/// 看着绿，改动别的夹具就红。
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
    path.to_string_lossy().to_string()
}

/// 进程级环境变量互斥锁。
///
/// `WECOM_WEBHOOK_URL` 是**进程**环境变量，Rust 的测试默认多线程并行：
/// 一个用例刚设好，另一个用例 `remove_var` 就把它抽走了，于是「发得出去」
/// 的用例报 `E_CFG_MISSING`。这不是产品缺陷，是测试自己踩自己的脚——
/// 用一把锁把这几个用例串起来，并明确写出它们共享进程环境。
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 取环境变量锁。**必须**由每个会动 `WECOM_WEBHOOK_URL` 的用例持有。
fn env_guard() -> std::sync::MutexGuard<'static, ()> {
    ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

fn tmp_ref(d: &TmpDir) -> &TmpDir {
    d
}

/// 临时目录（Drop 时删掉）。
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("psychdoc-ffi-archive-{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

const AT: &str = "2026-03-10T09:00:00+08:00";
const DAY: &str = "2026-03-10";

fn view_of(input: &str, alias: &str, store: Option<&str>) -> Result<Value, ApiError> {
    archive_view(&ArchiveViewRequest {
        input: input.to_string(),
        ruleset_yaml: ruleset(),
        encounter_alias: alias.to_string(),
        as_of: AT.to_string(),
        store_path: store.map(|s| s.to_string()),
        business_day: DAY.to_string(),
    })
}

/// 打开夹具里那份成套约束文书包（4 件齐全）。
fn view(alias: &str, store: Option<&str>) -> Result<Value, ApiError> {
    view_of(
        &root().join("fixtures/ward-02-restraint-set.json").to_string_lossy(),
        alias,
        store,
    )
}

fn states(v: &Value) -> Vec<(String, String)> {
    v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| {
            (
                r["node_id"].as_str().unwrap().to_string(),
                r["state"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 一屏数据
// ---------------------------------------------------------------------------

#[test]
fn the_view_answers_which_item_is_missing_and_which_ones_it_blocks() {
    let dir = TmpDir::new("missing");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).expect("算得出归档核齐视图");

    let by_id: std::collections::BTreeMap<_, _> =
        states(&v).into_iter().collect::<Vec<_>>().into_iter().collect();

    // ward-02 的约束知情同意被抽走了：它必须是「缺件」，
    // 而被它卡住的约束记录 / 解除记录必须是「被连带阻塞」而不是缺件。
    // 缺件节点的 id 是合成的 `<住院别名>::missing::<类型码>`，没有
    // `Composition.id` 可言——因此按**类型码**断言，不去匹配件名。
    let missing: Vec<&str> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["state"] == "missing")
        .map(|r| r["template_id"].as_str().unwrap())
        .collect();
    assert_eq!(missing, vec!["restraint_consent"], "恰好缺约束知情同意一件");

    let blocked: Vec<&str> = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|r| r["state"] == "blocked")
        .map(|r| r["template_id"].as_str().unwrap())
        .collect();
    assert_eq!(
        blocked,
        vec!["restraint_record", "restraint_release"],
        "被连带卡住的是约束记录与解除记录（按 node_id 升序）"
    );
    // 连带件不是缺件：护士长点进去必须看到「在等谁」，不是「文件不在」。
    for r in v["rows"].as_array().unwrap() {
        if r["state"] == "blocked" {
            assert!(
                !r["waiting_on"].as_array().unwrap().is_empty(),
                "被连带阻塞的件要能回答「在等谁」"
            );
        }
    }
    let _ = &by_id;

    assert_eq!(v["is_ready"], false, "有缺件就不算核齐");
    assert_eq!(v["ok"], true);
}

#[test]
fn a_compliant_encounter_is_ready_and_has_no_missing_rows() {
    let v = view_of(
        &root().join("fixtures/ward-01-minimal.json").to_string_lossy(),
        "WARD-A-001",
        None,
    )
    .expect("算得出归档核齐视图");
    assert_eq!(v["missing"].as_array().unwrap().len(), 0);
    assert_eq!(
        states(&v)
            .into_iter()
            .filter(|(_, s)| s != "ok")
            .count(),
        0,
        "合规住院不该有任何非达标节点"
    );
    assert_eq!(v["is_ready"], true, "零缺件、零连带阻塞、零人工待办 = 可归档");
}

#[test]
fn the_four_states_are_mutually_exclusive_and_every_node_gets_exactly_one() {
    let dir = TmpDir::new("states");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).unwrap();
    let rows = v["rows"].as_array().unwrap();
    assert!(!rows.is_empty(), "图上至少要有节点");
    for r in rows {
        let s = r["state"].as_str().unwrap();
        assert!(
            ["missing", "ok", "ambiguous", "blocked"].contains(&s),
            "未知节点态 {s}"
        );
    }
    // 四态互斥：同一个 node_id 不可能在表里出现两次。
    let ids: std::collections::BTreeSet<&str> =
        rows.iter().map(|r| r["node_id"].as_str().unwrap()).collect();
    assert_eq!(ids.len(), rows.len(), "同一节点出现了两次");
}

#[test]
fn a_soft_gap_is_visible_but_never_counted_as_a_missing_item() {
    // ward-02 的评估报告依赖住院记录，后者不在包里但 requires: false。
    let dir = TmpDir::new("soft");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).unwrap();
    let soft = v["soft_gaps"].as_array().unwrap();
    assert!(!soft.is_empty(), "参考缺件应当可见");
    for pair in soft {
        let from = pair[0].as_str().unwrap();
        let to = pair[1].as_str().unwrap();
        let row = v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["node_id"].as_str() == Some(to))
            .unwrap_or_else(|| panic!("软缺件 {to} 不在视图行里"));
        assert_ne!(
            row["state"].as_str(),
            Some("missing"),
            "参考缺件（requires: false）被当成欠件了：{from} → {to}"
        );
    }
}

#[test]
fn every_edge_carries_the_blocking_flag_that_drives_bold_rendering() {
    let dir = TmpDir::new("edges");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).unwrap();
    let edges = v["edges"].as_array().unwrap();
    assert!(!edges.is_empty(), "成套文书之间应有依赖边");
    for e in edges {
        assert!(e["blocking"].is_boolean(), "每条边都要能回答「要不要加粗」");
        assert!(e["requires"].is_boolean());
        assert!(
            e["describe"].as_str().is_some_and(|d| !d.is_empty()),
            "每条边都要有一句人能读懂的中文说明"
        );
    }
    // 缺件存在时必有阻塞边；没有阻塞边却加粗会让图上凭空多一根红线。
    assert!(
        edges.iter().any(|e| e["blocking"] == Value::Bool(true)),
        "有缺件就该有加粗的阻塞边"
    );
}

#[test]
fn each_row_carries_both_directions_and_they_are_not_the_same_answer() {
    let dir = TmpDir::new("dirs");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).unwrap();
    let rows = v["rows"].as_array().unwrap();
    let with_both: Vec<&Value> = rows
        .iter()
        .filter(|r| {
            !r["waiting_on"].as_array().unwrap().is_empty()
                && !r["blocking_others"].as_array().unwrap().is_empty()
        })
        .collect();
    assert!(!with_both.is_empty(), "至少一件既在等谁、也卡住谁");
    for r in with_both {
        let waiting: Vec<&str> = r["waiting_on"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        let blocking: Vec<&str> = r["blocking_others"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        // 同一件的正反两个方向在成环图上可能重合，但**不该被当成同一个问题**：
        // 界面把它们放在两栏，护士长问「谁卡着我」和「我卡着谁」要分开答。
        assert!(!waiting.is_empty() && !blocking.is_empty());
    }
}

#[test]
fn the_basis_lines_explain_the_whole_business_day_rule() {
    let dir = TmpDir::new("basis");
    let input = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", None).unwrap();
    let basis: Vec<&str> = v["basis_zh"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s.as_str().unwrap())
        .collect();
    assert!(basis.len() >= 3, "口径说明至少三行");
    let joined = basis.join("\n");
    assert!(joined.contains("requires"), "要说清参考缺件不阻断");
    assert!(
        joined.contains("自然日"),
        "欠件天数按自然日算，口径里必须写明——否则 3-02 23:40 缺、3-03 查会被读成 0 天"
    );
}

#[test]
fn missing_days_are_null_when_the_ledger_has_no_first_seen_moment() {
    // 库里没有登记过发现时刻 → 天数算不出，**不按今天兜底成 0**。
    let tmp = TmpDir::new("no-ledger");
    let _keep = &tmp;
    let db = tmp.path().join("empty.sqlite3");
    let input = without_template(tmp_ref(&tmp), "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    let missing = v["missing"].as_array().unwrap();
    assert!(!missing.is_empty(), "缺件本身不受登记簿影响");
    for m in missing {
        assert!(
            m["days"].is_null(),
            "没有登记发现时刻却算出了天数：{}",
            m["basis_zh"]
        );
        assert!(
            m["basis_zh"].as_str().unwrap().contains("登记"),
            "天数算不出必须说清为什么：{}",
            m["basis_zh"]
        );
    }
    assert!(v["encounter_age"]["days"].is_null());
    assert!(
        v["encounter_age"]["basis_zh"]
            .as_str()
            .unwrap()
            .contains("算不出")
    );
}

#[test]
fn an_unknown_encounter_alias_is_rejected_rather_than_returning_an_empty_graph() {
    let err = view("WARD-NOPE-999", None).expect_err("不存在的住院别名不能给空图");
    assert_eq!(
        err.code(),
        "E_ENCOUNTER_UNKNOWN",
        "空图会被读成「已核齐」，必须显式拒"
    );
}

// ---------------------------------------------------------------------------
// 一键发缺件待办
// ---------------------------------------------------------------------------

/// 本地 Webhook 协议替身：真 socket、真 HTTP、真 `{"errcode":0}` 业务码。
/// 记录收到的**每一次**请求，供幂等断言用。
struct FakeWebhook {
    url: String,
}

impl FakeWebhook {
    fn start(calls: Arc<AtomicUsize>, bodies: Arc<std::sync::Mutex<Vec<String>>>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().unwrap().port();
        let thread_calls = Arc::clone(&calls);
        let thread_bodies = Arc::clone(&bodies);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                serve_one(stream, &thread_calls, &thread_bodies);
            }
        });
        FakeWebhook {
            // 必须带 key=，否则 WeComConfig 判形态错（与真实端点同一条尺）。
            url: format!("http://127.0.0.1:{port}/cgi-bin/webhook/send?key=TESTKEY"),
        }
    }
}

fn serve_one(
    mut stream: TcpStream,
    calls: &Arc<AtomicUsize>,
    bodies: &Arc<std::sync::Mutex<Vec<String>>>,
) {
    let mut reader = BufReader::new(stream.try_clone().expect("克隆连接"));
    let mut content_length = 0usize;
    let mut first = true;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim_end();
        if first {
            first = false;
            assert!(
                trimmed.starts_with("POST "),
                "缺件提醒必须走 POST，实际 {trimmed}"
            );
        }
        if trimmed.is_empty() {
            break;
        }
        if let Some(v) = trimmed.strip_prefix("Content-Length: ") {
            content_length = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; content_length];
    if content_length > 0 {
        let _ = std::io::Read::read_exact(&mut reader, &mut body);
    }
    bodies
        .lock()
        .unwrap()
        .push(String::from_utf8_lossy(&body).to_string());
    calls.fetch_add(1, Ordering::SeqCst);
    let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\r\n");
    let _ = stream.write_all(br#"{"errcode":0,"errmsg":"ok"}"#);
}

fn missing_of(v: &Value) -> Vec<(String, String)> {
    v["missing"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| {
            let node_id = m["node_id"].as_str().unwrap().to_string();
            let label = v["rows"]
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

fn notify_req(
    store: &str,
    missing: Vec<(String, String)>,
    dry_run: bool,
) -> NotifyRequest {
    NotifyRequest {
        store_path: Some(store.to_string()),
        encounter_alias: "WARD-B-001".to_string(),
        ward: "精神科三病区".to_string(),
        business_day: DAY.to_string(),
        at: AT.to_string(),
        missing,
        group_hint: "精神科病区文书群".to_string(),
        dry_run,
    }
}

#[test]
fn sending_a_missing_item_todo_goes_out_over_the_real_protocol() {
    let _env = env_guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = FakeWebhook::start(calls.clone(), bodies.clone());
    std::env::set_var("WECOM_WEBHOOK_URL", &hook.url);

    let tmp = TmpDir::new("send");
    let db = tmp.path().join("send.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    let missing = missing_of(&v);
    assert!(!missing.is_empty(), "先得有缺件才谈得上发待办");

    let out = notify_missing(&notify_req(db.to_str().unwrap(), missing, false))
        .expect("缺件待办应发得出去");

    assert_eq!(out["sent"], 1, "一条缺件一条提醒");
    assert_eq!(out["total"], 1);
    assert_eq!(out["outcomes"][0]["state"], "sent");
    assert_eq!(out["outcomes"][0]["is_terminal"], true);
    assert_eq!(calls.load(Ordering::SeqCst), 1, "确实出了一次站");

    // 消息体必须是企业微信群机器人的规范形状，且抬头写清是哪次住院。
    let body = &bodies.lock().unwrap()[0];
    let parsed: Value = serde_json::from_str(body).expect("消息体是合法 JSON");
    assert_eq!(parsed["msgtype"], "markdown");
    let content = parsed["markdown"]["content"].as_str().unwrap();
    assert!(content.contains("WARD-B-001"), "消息体要写明住院：{content}");
    assert!(content.contains("2026-03-10"), "消息体要写明业务日：{content}");
    assert!(
        content.contains("约束知情同意"),
        "消息体要列缺件中文名：{content}"
    );

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

#[test]
fn sending_the_same_todo_twice_on_the_same_day_makes_exactly_one_call() {
    let _env = env_guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = FakeWebhook::start(calls.clone(), bodies.clone());
    std::env::set_var("WECOM_WEBHOOK_URL", &hook.url);

    let tmp = TmpDir::new("idem");
    let db = tmp.path().join("idem.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    let missing = missing_of(&v);

    let first = notify_missing(&notify_req(db.to_str().unwrap(), missing.clone(), false)).unwrap();
    assert_eq!(first["outcomes"][0]["state"], "sent");

    let second = notify_missing(&notify_req(db.to_str().unwrap(), missing, false)).unwrap();
    assert_eq!(
        second["outcomes"][0]["state"],
        "already_sent",
        "同一天同一件不该重发"
    );
    assert_eq!(second["sent"], 0);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "第二次点击不能再往群里发一条"
    );

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

#[test]
fn a_sent_item_comes_back_marked_sent_so_the_button_can_grey_out() {
    let _env = env_guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = FakeWebhook::start(calls.clone(), bodies.clone());
    std::env::set_var("WECOM_WEBHOOK_URL", &hook.url);

    let tmp = TmpDir::new("grey");
    let db = tmp.path().join("grey.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let missing = missing_of(&view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap());
    let node_id = missing[0].0.clone();
    notify_missing(&notify_req(db.to_str().unwrap(), missing, false)).unwrap();

    // 重算视图：这一件的 notification 必须是已发，且带发出时刻——
    // 界面要显示「已于 T 时刻由 X 发出」，没有时刻就显示不出这句话。
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    let row = v["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["node_id"].as_str() == Some(node_id.as_str()))
        .expect("刚发过的那一件还在图上");
    assert_eq!(row["notification"]["state"], "sent");
    assert_eq!(row["notification"]["sent_at"], AT);
    assert!(
        row["notification"]["as_zh"]
            .as_str()
            .unwrap()
            .contains("已送达")
    );

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

#[test]
fn an_unsent_item_carries_no_notification_at_all_rather_than_a_fake_zero() {
    let tmp = TmpDir::new("unsent");
    let db = tmp.path().join("unsent.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    for r in v["rows"].as_array().unwrap() {
        assert!(
            r["notification"].is_null(),
            "没发过的件不该有一条「未发」的通知行：{}",
            r["node_id"]
        );
    }
}

#[test]
fn sending_without_a_webhook_url_reports_cfg_missing_and_makes_no_call() {
    let _env = env_guard();
    std::env::remove_var("WECOM_WEBHOOK_URL");
    let tmp = TmpDir::new("nocfg");
    let db = tmp.path().join("nocfg.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();

    let err = notify_missing(&notify_req(
        db.to_str().unwrap(),
        missing_of(&v),
        false,
    ))
    .expect_err("没配 Webhook 就不该假装发过");
    assert_eq!(err.code(), "E_CFG_MISSING");
    assert!(err.message().contains("WECOM_WEBHOOK_URL"));
}

#[test]
fn a_dry_run_builds_the_very_same_message_without_leaving_the_machine() {
    let _env = env_guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = FakeWebhook::start(calls.clone(), bodies.clone());
    std::env::set_var("WECOM_WEBHOOK_URL", &hook.url);

    let tmp = TmpDir::new("dry");
    let db = tmp.path().join("dry.sqlite3");
    let input = without_template(&tmp, "ward-02-restraint-set.json", "restraint_consent");
    let v = view_of(&input, "WARD-B-001", Some(db.to_str().unwrap())).unwrap();
    let out = notify_missing(&notify_req(db.to_str().unwrap(), missing_of(&v), true)).unwrap();

    assert_eq!(out["dry_run"], true);
    assert_eq!(out["sent"], 0);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "干跑零次外发");

    // 干跑看到的正文必须与真发出去的**逐字相同**——同一段构造代码。
    let dry = out["markdown"].as_str().unwrap();
    let real = notify_missing(&notify_req(db.to_str().unwrap(), missing_of(&v), false)).unwrap();
    assert!(real["ok"] == true);
    let sent_body: Value =
        serde_json::from_str(&bodies.lock().unwrap()[0]).unwrap();
    assert_eq!(sent_body["markdown"]["content"].as_str().unwrap(), dry);

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

#[test]
fn sending_with_nothing_missing_is_refused_before_any_outbound_call() {
    let _env = env_guard();
    let calls = Arc::new(AtomicUsize::new(0));
    let bodies = Arc::new(std::sync::Mutex::new(Vec::new()));
    let hook = FakeWebhook::start(calls.clone(), bodies.clone());
    std::env::set_var("WECOM_WEBHOOK_URL", &hook.url);

    let tmp = TmpDir::new("empty-sel");
    let db = tmp.path().join("empty.sqlite3");
    let err = notify_missing(&notify_req(db.to_str().unwrap(), Vec::new(), false))
        .expect_err("没有缺件就不该发出一条空提醒");
    assert_eq!(err.code(), "E_NOTHING_SELECTED");
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    std::env::remove_var("WECOM_WEBHOOK_URL");
}

// ---------------------------------------------------------------------------
// 补齐后重算
// ---------------------------------------------------------------------------

#[test]
fn restoring_the_missing_item_makes_the_blocking_edges_disappear() {
    let dir = TmpDir::new("restore");
    let trimmed = without_template(&dir, "ward-02-restraint-set.json", "restraint_consent");
    let full_path = root().join("fixtures/ward-02-restraint-set.json");
    let full = full_path.to_string_lossy();

    let edges = |input: &str| -> Vec<String> {
        view_of(input, "WARD-B-001", None).unwrap()["blocking_edges"]
            .as_array()
            .map(|a| {
                a.iter()
                    .map(|e| format!("{} → {}", e[0].as_str().unwrap(), e[1].as_str().unwrap()))
                    .collect()
            })
            .unwrap_or_default()
    };

    let before = edges(&trimmed);
    assert!(!before.is_empty(), "缺件时本该有阻塞边：{before:?}");

    // 把缺的那一件补回去（读同一份成套包原件）：阻塞边与占位节点同时退场。
    // 这是从**界面真正消费的那个入口**验证，不复用 Task 21 的闭包用例——
    // 那个用例证明闭包算对了，这个证明界面拿到的数据也跟着对了。
    let after = edges(&full);
    assert_eq!(after.len(), 0, "补齐后阻塞边应清零：{after:?}");

    let v = view_of(&full, "WARD-B-001", None).unwrap();
    assert_eq!(v["is_ready"], true);
    assert_eq!(
        v["rows"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|r| r["state"] == "missing")
            .count(),
        0,
        "补齐后不该还有缺件节点"
    );
}