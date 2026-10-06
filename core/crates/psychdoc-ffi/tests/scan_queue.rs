//! Task 32 · `scan_queue` 入口的行为测试。
//!
//! 这个入口回答的是护士长拖进一整个病区文件夹之后的三个问题：
//!
//! 1. **哪几次住院、每次几件、还差几件**（左栏那一列）；
//! 2. **哪些文件根本没被打开**（「已跳过」清单，一个都不许静默丢）；
//! 3. **关窗再开，队列还在不在**（落库 + 回读）。
//!
//! 与 `review_batch`（判「哪几件不合规」）的分工是这一轮的核心设计：
//! 判定主体不搬进队列入口，两条路径共享发现链路与类型字典，
//! 因此「拖文件夹」与「点单文件」不会对同一份包给出不同的跳过口径。

use psychdoc_ffi::api::{scan_queue, ApiError, ScanQueueRequest};
use serde_json::Value;

fn root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .to_path_buf()
}

fn ruleset() -> String {
    std::fs::read_to_string(root().join("rulesets/hospital.yaml")).expect("读得到规则集")
}

fn fixture(name: &str) -> String {
    std::fs::read_to_string(root().join("fixtures").join(name))
        .unwrap_or_else(|e| panic!("读得到夹具 {name} — {e}"))
}

/// 临时目录（Drop 时删掉）。
struct TmpDir(std::path::PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("psychdoc-ffi-queue-{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self) -> &std::path::Path {
        &self.0
    }

    fn file(&self, rel: &str, content: &str) {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, content).unwrap();
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn call(
    roots: Vec<String>,
    store_path: Option<&str>,
    persist: bool,
) -> Result<Value, ApiError> {
    scan_queue(&ScanQueueRequest {
        roots,
        ruleset_yaml: ruleset(),
        at: "2026-03-02T08:00:00+08:00".into(),
        store_path: store_path.map(str::to_string),
        persist,
    })
}

fn aliases(v: &Value) -> Vec<String> {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["encounter_alias"].as_str().unwrap().to_string())
        .collect()
}

fn entry<'a>(v: &'a Value, alias: &str) -> &'a Value {
    v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|e| e["encounter_alias"] == alias)
        .unwrap_or_else(|| panic!("队列里应有 {alias}：{:?}", aliases(v)))
}

// ---------------------------------------------------------------------------
// 主路径：一整个 fixtures 目录拖进来
// ---------------------------------------------------------------------------

#[test]
fn the_whole_fixture_folder_becomes_one_queue_grouped_by_encounter() {
    let v = call(
        vec![root().join("fixtures").display().to_string()],
        None,
        false,
    )
    .unwrap();

    let aliases = aliases(&v);
    assert!(aliases.len() >= 8, "应有多个住院条目，实际 {}", aliases.len());
    assert!(aliases.iter().all(|a| a.starts_with("WARD-")), "{aliases:?}");
    // 别名不重复：一次住院只在左栏出现一次
    let mut sorted = aliases.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), aliases.len(), "同一次住院出现了两次");

    // 每条都有件数与徽标；徽标说的是「缺件 N」不是「不合规 N」
    for e in v["entries"].as_array().unwrap() {
        assert!(e["doc_count"].as_u64().unwrap() > 0, "条目应有件数");
        assert!(e["ward"].is_string(), "条目应带病区字段（认不出时是空串）");
        let badge = e["badge_zh"].as_str().unwrap();
        assert!(badge.contains("件"), "{badge}");
        if e["missing_count"].as_u64().unwrap() == 0 {
            assert!(badge.contains("达标"), "{badge}");
        } else {
            assert!(badge.contains("缺件"), "{badge}");
        }
    }

    // 守恒：看到的文件 = 打开的 + 跳过的
    assert_eq!(v["is_balanced"], true, "队列守恒");
    assert_eq!(v["filtered_balanced"], true, "发现守恒");
    let t = &v["totals"];
    assert_eq!(
        t["files_seen"].as_u64().unwrap(),
        t["files_opened"].as_u64().unwrap() + t["skipped_files"].as_u64().unwrap()
    );
    // 件数 = 各条目 doc_count 之和（界面上「共 N 件」不能与逐条对不上）
    let summed: u64 = v["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["doc_count"].as_u64().unwrap())
        .sum();
    assert_eq!(t["items"].as_u64().unwrap(), summed);
    assert_eq!(
        t["missing"].as_u64().unwrap(),
        v["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["missing_count"].as_u64().unwrap())
            .sum::<u64>()
    );
    assert_eq!(v["ruleset_short_sha256"].as_str().unwrap().len(), 12);
}

#[test]
fn the_missing_count_agrees_with_the_archive_view_on_the_same_package() {
    // 左栏徽标的缺件数必须与依赖图同源（blocking_missing_ids），
    // 否则同一次住院在「左栏」和「归档核齐」两个标签页会报出两个数，
    // 而这个数是要发给护士长的。
    let v = call(
        vec![root().join("fixtures").display().to_string()],
        None,
        false,
    )
    .unwrap();
    let c = entry(&v, "WARD-C-001");
    let missing = c["missing_count"].as_u64().unwrap();
    assert!(missing >= 1, "WARD-C-001 应当缺件，实际 {missing}");

    // 同一份包问依赖图：只取**从 WARD-C-001 的件出发**的阻塞边。
    // `blocking_edges` 是整张图的（边绝不跨住院别名），因此别把它
    // 当成某一个住院的清单——那样会拿别的住院的缺件数来对账。
    let g = psychdoc_ffi::api::graph_both_directions(
        &root().join("fixtures").display().to_string(),
        &ruleset(),
        "WARD-C-001-RECORD-HIS",
    )
    .unwrap();
    let from_this_node: Vec<String> = g["blocking_edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["from"] == "WARD-C-001-RECORD-HIS")
        .filter_map(|e| e["to"].as_str().map(str::to_string))
        .collect();
    assert!(
        !from_this_node.is_empty(),
        "WARD-C-001-RECORD-HIS 应有阻塞边"
    );
    for t in &from_this_node {
        assert!(
            t.starts_with("<WARD-C-001>::missing::"),
            "阻塞目标 {t} 应属于 WARD-C-001，徽标才和它对得上"
        );
        // 图上这一住院的缺件数与左栏徽标逐条对得上
        assert!(
            missing >= from_this_node.len() as u64,
            "左栏缺件数 {missing} 少于图上可见的 {} 条阻塞目标",
            from_this_node.len()
        );
    }
    // 图上标了 `requires: false` 的软缺件**不进**徽标：把参考缺件算成
    // 欠件，护士长会收到一条根本不欠的催办。
    let soft: Vec<&Value> = g["blocking_edges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| e["from"] == "WARD-C-001-RECORD-HIS" && e["requires"] == false)
        .collect();
    assert!(
        soft.iter().all(|e| !from_this_node.contains(&e["to"].as_str().unwrap().to_string())),
        "软缺件不得进阻塞目标集"
    );
}

// ---------------------------------------------------------------------------
// 边界：杂项文件逐条入「已跳过」，且一份都没被静默丢
// ---------------------------------------------------------------------------

#[test]
fn misc_files_are_listed_as_skipped_with_a_readable_reason() {
    let dir = TmpDir::new("misc");
    dir.file(
        "WARD-A-001-CONSENT.json",
        &fixture("ward-02-restraint-set.json"),
    );
    dir.file("扫描件.pdf", "%PDF-1.4");
    dir.file("台账.xlsx", "PK");
    dir.file("半截导出.json", "{\"resourceType\":\"Bundle\",\"entry\":[{");
    dir.file("患者清单.json", "{\"resourceType\":\"Patient\",\"id\":\"P-1\"}");

    let v = call(vec![dir.path().display().to_string()], None, false).unwrap();

    let skips = v["file_skips"].as_array().unwrap();
    assert_eq!(skips.len(), 4, "四个打不开的文件都要记账：{skips:?}");
    // 唯一的好包照常打开
    assert_eq!(v["totals"]["packages"].as_u64().unwrap(), 1);
    assert_eq!(v["totals"]["items"].as_u64().unwrap(), 4, "成套 4 件");

    for s in skips {
        let zh = s["reason_zh"].as_str().unwrap();
        assert!(!zh.is_empty());
        assert!(!s["relative_path"].as_str().unwrap().is_empty());
        assert!(s["describe_zh"].as_str().unwrap().contains(zh));
    }
    let by_name = |n: &str| {
        skips
            .iter()
            .find(|s| s["name"] == n)
            .unwrap_or_else(|| panic!("{n} 应在已跳过清单里"))
    };
    assert_eq!(by_name("扫描件.pdf")["reason"], "UnsupportedExtension");
    assert_eq!(by_name("台账.xlsx")["reason"], "UnsupportedExtension");
    assert_eq!(by_name("半截导出.json")["reason"], "MalformedJson");
    assert_eq!(by_name("患者清单.json")["reason"], "NotABundle");
    assert_eq!(by_name("扫描件.pdf")["retryable"], false);
}

#[test]
fn an_empty_folder_says_why_instead_of_reporting_zero_findings() {
    let dir = TmpDir::new("empty");
    let v = call(vec![dir.path().display().to_string()], None, false).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    // 关键：不能是「0 条命中 = 全部合规」，必须明说没找到东西
    let reason = v["empty_reason_zh"].as_str().expect("空文件夹要给原因");
    assert!(reason.contains("没有任何文件"), "{reason}");
    // 空队列**不落库**
    assert_eq!(v["persisted"], false);
    assert_eq!(v["queue_id"], Value::Null);
}

#[test]
fn a_folder_of_only_misc_files_is_reported_as_such_and_still_balances() {
    let dir = TmpDir::new("allmisc");
    dir.file("a.pdf", "%PDF");
    dir.file("b.xlsx", "PK");
    let v = call(vec![dir.path().display().to_string()], None, false).unwrap();
    assert_eq!(v["entries"].as_array().unwrap().len(), 0);
    assert_eq!(v["totals"]["skipped_files"].as_u64().unwrap(), 2);
    assert_eq!(v["filtered_balanced"], true);
    let reason = v["empty_reason_zh"].as_str().unwrap();
    assert!(reason.contains("都不是文书包"), "{reason}");
}

// ---------------------------------------------------------------------------
// 边界：多个文件夹一次拖入
// ---------------------------------------------------------------------------

#[test]
fn several_folders_dropped_at_once_land_in_one_queue_without_double_counting() {
    let a = TmpDir::new("multi-a");
    let b = TmpDir::new("multi-b");
    a.file("WARD-A-001-CONSENT.json", &fixture("ward-02-restraint-set.json"));
    b.file("WARD-B-001-CONSENT.json", &fixture("ward-01-minimal.json"));
    b.file("杂项.pdf", "%PDF");

    let roots = vec![
        a.path().display().to_string(),
        b.path().display().to_string(),
    ];
    let v = call(roots, None, false).unwrap();
    assert!(aliases(&v).contains(&"WARD-B-001".to_string()));
    assert_eq!(v["totals"]["skipped_files"].as_u64().unwrap(), 1);
    assert_eq!(v["filtered_balanced"], true);

    // 同一文件夹拖两次不会把同一次住院数两遍
    let dup = vec![
        a.path().display().to_string(),
        b.path().display().to_string(),
        a.path().display().to_string(),
    ];
    let v2 = call(dup, None, false).unwrap();
    assert_eq!(
        v2["totals"]["items"].as_u64().unwrap(),
        v["totals"]["items"].as_u64().unwrap()
    );
}

#[test]
fn dropping_nothing_is_refused_with_a_code_not_an_empty_success() {
    match call(vec![], None, false) {
        Err(e) => assert_eq!(e.code(), "E_QUEUE_EMPTY"),
        Ok(_) => panic!("没拖任何东西不该返回成功"),
    }
}

// ---------------------------------------------------------------------------
// 持久化：关窗重开队列仍在
// ---------------------------------------------------------------------------

#[test]
fn the_queue_is_persisted_and_can_be_read_back_after_a_restart() {
    let dir = TmpDir::new("persist");
    dir.file("WARD-A-001-CONSENT.json", &fixture("ward-02-restraint-set.json"));
    dir.file("杂项.pdf", "%PDF");
    let db = dir.path().join("store/psychdoc.sqlite3");

    let v = call(
        vec![dir.path().display().to_string()],
        Some(&db.display().to_string()),
        true,
    )
    .unwrap();
    assert_eq!(v["persisted"], true);
    let queue_id = v["queue_id"].as_str().expect("落库后要有队列 id").to_string();

    // 模拟关窗重开：重新打开库读回
    let store = psychdoc_store::Store::open(&db).unwrap();
    let back = psychdoc_store::load_queue(store.conn(), &queue_id)
        .unwrap()
        .expect("队列不能丢");
    assert_eq!(
        back.entries.len(),
        v["entries"].as_array().unwrap().len(),
        "落库的条目数与返回的一致"
    );
    // 左栏顺序原样还原
    assert_eq!(
        back.entries
            .iter()
            .map(|e| e.encounter_alias.as_str())
            .collect::<Vec<_>>(),
        aliases(&v)
    );
    assert_eq!(back.session.missing_total as u64, v["totals"]["missing"].as_u64().unwrap());
    // 「已跳过」清单存的是原样 JSON，重开后能逐字还原
    let stored: serde_json::Value =
        serde_json::from_str(&back.session.file_skips_json).unwrap();
    assert_eq!(
        stored.as_array().unwrap().len(),
        v["file_skips"].as_array().unwrap().len()
    );
    assert!(back.session.file_skips_json.contains("杂项.pdf"));

    // 库文件落在指定的 store 目录里，不在当前工作目录
    assert!(db.canonicalize().unwrap().starts_with(dir.path().canonicalize().unwrap()));
}

#[test]
fn a_scan_without_persist_does_not_touch_the_database() {
    let dir = TmpDir::new("nopersist");
    dir.file("WARD-A-001-CONSENT.json", &fixture("ward-02-restraint-set.json"));
    let db = dir.path().join("store/psychdoc.sqlite3");

    let v = call(
        vec![dir.path().display().to_string()],
        Some(&db.display().to_string()),
        false,
    )
    .unwrap();
    assert_eq!(v["persisted"], false, "试扫不该落库");
    assert_eq!(v["entries"].as_array().unwrap().len(), 1, "但结果照常给");
    assert!(!db.exists(), "试扫不该凭空建出库文件");
}

// ---------------------------------------------------------------------------
// 失败不被降级：规则集坏、路径不存在
// ---------------------------------------------------------------------------

#[test]
fn a_broken_ruleset_is_refused_rather_than_degrading_to_zero_findings() {
    let r = scan_queue(&ScanQueueRequest {
        roots: vec![root().join("fixtures").display().to_string()],
        ruleset_yaml: "templates: [".into(),
        at: "2026-03-02T08:00:00+08:00".into(),
        store_path: None,
        persist: false,
    });
    match r {
        Err(e) => assert_eq!(e.code(), "E_RULESET_INVALID", "坏规则集不能降级为「全部通过」"),
        Ok(_) => panic!("坏规则集不该返回成功"),
    }
}

#[test]
fn a_missing_path_is_refused_with_an_io_code() {
    let r = call(
        vec!["/nonexistent/导出目录".to_string()],
        None,
        false,
    );
    match r {
        Err(e) => assert_eq!(e.code(), "E_IO"),
        Ok(_) => panic!("路径不存在不该返回空队列"),
    }
}