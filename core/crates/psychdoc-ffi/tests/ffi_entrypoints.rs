//! 绑定层 5 个同步入口的行为测试。
//!
//! 这些用例**不开** `extension-module` feature（不建 `.so`）就能跑：
//! 业务逻辑全在 `api` 里，PyO3 只做搬运。Python 侧的 `tests/test_ffi_bindings.py`
//! 在 `maturin develop` 之后补上「返回值确实落到 Python 对象」那一层。

use psychdoc_ffi::api::{
    build_writeback_preview, check_bundle, graph_both_directions, preview_fixback, review_batch,
    ApiError, CommitRequest,
};
use serde_json::Value;

fn root() -> std::path::PathBuf {
    // CARGO_MANIFEST_DIR = core/crates/psychdoc-ffi
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

fn diag_codes(v: &Value) -> Vec<String> {
    v["summary"]["diagnostics"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["code"].as_str().unwrap_or_default().to_string())
        .collect()
}

// ---------------------------------------------------------------------------
// 入口 1：check_bundle
// ---------------------------------------------------------------------------

#[test]
fn check_bundle_returns_coded_located_diagnostics() {
    let out = check_bundle(&fixture("ward-01-minimal.json"), &ruleset()).expect("审查成功");
    assert_eq!(out["ok"], true);
    let codes = diag_codes(&out);
    assert_eq!(codes, vec!["QC-PSY-01".to_string()], "恰好 1 条必填缺失");
    let d = &out["summary"]["diagnostics"][0];
    // 三件套齐全：码 / 位置 / 为什么是问题（+ 院内规范依据）
    assert_eq!(d["doc_id"], "WARD-A-001-CONSENT-01");
    assert!(d["section_index"].as_u64().is_some());
    assert!(d["char_end"].as_u64().unwrap() >= d["char_start"].as_u64().unwrap());
    assert!(d["message"].as_str().unwrap().contains("患方签字"));
    assert!(d["basis"].as_str().unwrap().contains("第"), "message 必须带规范条款依据");
    // 不可自动修是一等状态：UI 依此不渲染补写按钮。
    assert_eq!(d["fix_availability"], "none");
    assert_eq!(d["can_fix"], false);
    // 底栏运行摘要要能显示规则集短哈希
    assert_eq!(out["summary"]["ruleset_short_sha256"].as_str().unwrap().len(), 12);
}

#[test]
fn check_bundle_groups_items_by_encounter_for_the_left_pane() {
    let out = check_bundle(&fixture("ward-11-cross-doc-violations.json"), &ruleset()).expect("审查成功");
    let rows = out["encounters"].as_array().expect("encounters 是数组");
    assert!(!rows.is_empty());
    // 左栏条目：脱敏别名 + 件数 + 阻断数
    assert!(rows[0]["encounter_alias"].as_str().unwrap().starts_with("WARD-"));
    assert!(rows[0]["doc_count"].as_u64().unwrap() >= 1);
    // 排序契约：阻断数多的排前面
    for pair in rows.windows(2) {
        assert!(pair[0]["blocking_count"].as_u64().unwrap() >= pair[1]["blocking_count"].as_u64().unwrap());
    }
}

#[test]
fn a_compliant_package_produces_no_findings_but_still_reports_counts() {
    let out = check_bundle(&fixture("ward-09-field-compliant.json"), &ruleset()).expect("审查成功");
    assert!(diag_codes(&out).is_empty());
    assert_eq!(out["summary"]["blocking_count"], 0);
    // 「0 条诊断」不等于「没看过」：件数必须仍在摘要里。
    assert!(out["summary"]["items_reviewed"].as_u64().unwrap() >= 1);
}

#[test]
fn an_invalid_ruleset_is_refused_with_a_stable_code_not_a_silent_pass() {
    // 规则引用了未定义的文书类型 → 加载期强校验拒绝。
    let broken = r#"
version: "1.0.0"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        section: 患方知情同意
        required: true
hospital:
  doc_types:
    restraint_consent: "约束知情同意"
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "必填缺失"
      basis: "《精神科住院护理常规》第 4.2 条"
      applies_to: ["never_defined_form"]
    - code: "QC-PSY-01"
      severity: blocking
      title: "同层重复码"
      basis: "《精神科住院护理常规》第 4.3 条"
      applies_to: ["restraint_consent"]
"#;
    let e = check_bundle(&fixture("ward-01-minimal.json"), broken).unwrap_err();
    assert_eq!(e.code(), "E_RULESET_INVALID");
    // 绝不降级为「无规则 = 全部通过」：一条诊断都不该被放行。
    assert!(e.message().contains("E_RULESET_INVALID"));
}

#[test]
fn an_empty_ruleset_string_is_refused_rather_than_treated_as_no_findings() {
    // 绑定层自己先拦一道空串：桌面壳把路径读空了时不该「静默 0 条通过」。
    let e = check_bundle(&fixture("ward-01-minimal.json"), "   ").unwrap_err();
    assert_eq!(e.code(), "E_RULESET_INVALID");
}

// ---------------------------------------------------------------------------
// 入口 2：review_batch
// ---------------------------------------------------------------------------

#[test]
fn review_batch_over_a_directory_reports_run_summary_for_the_status_bar() {
    let out = review_batch(&root().join("fixtures").to_string_lossy(), &ruleset(), None)
        .expect("批扫成功");
    assert!(out["packages_found"].as_u64().unwrap() >= 1);
    let s = &out["summary"];
    assert!(s["items_reviewed"].as_u64().unwrap() >= 1);
    assert_eq!(s["ruleset_short_sha256"].as_str().unwrap().len(), 12);
    assert!(s["unresolved_count"].is_number(), "未解析件数必须显式带上");
}

#[test]
fn review_batch_second_run_with_a_cache_skips_everything() {
    let cache = std::env::temp_dir().join(format!("psychdoc-ffi-cache-{}.json", std::process::id()));
    let _ = std::fs::remove_file(&cache);
    let dir = root().join("fixtures").to_string_lossy().to_string();
    let cp = cache.to_string_lossy().to_string();

    let first = review_batch(&dir, &ruleset(), Some(&cp)).expect("首次批扫");
    let rs = &first["run_summary"];
    assert!(rs["items_reviewed"].as_u64().unwrap() > 0, "首次应重跑");
    assert!(rs["as_zh"].as_str().unwrap().contains("重跑"));

    let second = review_batch(&dir, &ruleset(), Some(&cp)).expect("二次批扫");
    let rs2 = &second["run_summary"];
    assert_eq!(rs2["items_reviewed"].as_u64().unwrap(), 0, "内容未变应全部跳过");
    assert!(rs2["items_skipped"].as_u64().unwrap() > 0);
    assert_eq!(
        diag_codes(&second),
        diag_codes(&first),
        "增量与全量结论必须逐条相同"
    );
    let _ = std::fs::remove_file(&cache);
}

#[test]
fn review_batch_on_a_missing_path_reports_an_io_error() {
    let e = review_batch("/no/such/export/dir", &ruleset(), None).unwrap_err();
    assert_eq!(e.code(), "E_IO");
}

// ---------------------------------------------------------------------------
// 入口 3：preview_fixback
// ---------------------------------------------------------------------------

#[test]
fn preview_fixback_splits_selectable_blocks_from_evidence_only_codes() {
    let out = preview_fixback(&fixture("ward-08-field-violations.json"), &ruleset())
        .expect("预览生成成功");
    assert_eq!(out["ok"], true);
    // ward-08 有一条可自动修（全角日期），其余三条只给依据。
    assert_eq!(out["blocks"].as_array().unwrap().len(), 1);
    let b = &out["blocks"][0];
    assert_eq!(b["code"], "QC-PSY-02");
    assert_eq!(b["fix_availability"], "automatic");
    assert!(!b["diff"].as_array().unwrap().is_empty(), "双栏 diff 必须有逐行对照");
    assert!(b["basis"].as_str().unwrap().contains("第"));

    let evidence = out["evidence_only"].as_array().unwrap();
    let codes: Vec<&str> = evidence.iter().map(|c| c.as_str().unwrap()).collect();
    assert!(codes.contains(&"QC-PSY-01"), "必填缺失只给依据不给按钮");
    assert!(!codes.contains(&"QC-PSY-02"));
    assert!(out["summary_line"].as_str().unwrap().contains("段"));
}

#[test]
fn preview_fixback_of_a_clean_package_is_empty_not_an_error() {
    let out = preview_fixback(&fixture("ward-09-field-compliant.json"), &ruleset())
        .expect("预览生成成功");
    assert!(out["blocks"].as_array().unwrap().is_empty());
    assert!(out["evidence_only"].as_array().unwrap().is_empty());
}

// ---------------------------------------------------------------------------
// 入口 5：graph_both_directions
// ---------------------------------------------------------------------------

#[test]
fn graph_both_directions_answers_two_different_questions_about_the_same_graph() {
    let bundle = fixture("ward-02-restraint-set.json");
    let node = "WARD-B-001-RECORD-01";
    let out = graph_both_directions(&bundle, &ruleset(), node).expect("出图成功");
    assert_eq!(out["exists"], true);
    assert_eq!(out["node_id"], node);
    let deps: Vec<&str> = out["dependencies"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    let dependents: Vec<&str> = out["dependents"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    assert!(!deps.is_empty(), "应有前置件");
    assert!(!dependents.is_empty(), "应有后继件");
    assert_ne!(deps, dependents, "正反向必须给出不同答案");
    // 并集 = both 方向（自身不重复计入）
    let mut union: Vec<&str> = deps.iter().chain(dependents.iter()).copied().collect();
    union.sort_unstable();
    union.dedup();
    let mut both: Vec<&str> = out["both"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
    both.sort_unstable();
    assert_eq!(union, both);
}

#[test]
fn graph_on_an_unknown_node_reports_absence_instead_of_panicking() {
    let out = graph_both_directions(&fixture("ward-02-restraint-set.json"), &ruleset(), "NOPE-1")
        .expect("出图成功");
    assert_eq!(out["exists"], false);
    assert!(out["dependencies"].as_array().unwrap().is_empty());
    assert!(out["node"].is_null());
}

// ---------------------------------------------------------------------------
// 入口 4：commit_writeback / 干跑
// ---------------------------------------------------------------------------

fn commit_req() -> CommitRequest {
    CommitRequest {
        store_path: None,
        draft_id: "DRAFT-1".into(),
        encounter_alias: "WARD-A-001".into(),
        actor: "护士长A".into(),
        at: "2026-03-03T08:00:00+08:00".into(),
        entries: vec![(
            "WARD-A-001-CONSENT-01".into(),
            serde_json::json!({
                "resourceType": "Composition",
                "id": "WARD-A-001-CONSENT-01",
                "status": "preliminary",
                "type": {"coding": [{"code": "restraint_consent"}]}
            }),
        )],
        content_hashes: vec![("WARD-A-001-CONSENT-01".into(), "hash-1".into())],
        ruleset_sha256: "ruleset-v1".into(),
        run_id: Some("RUN-1".into()),
    }
}

#[test]
fn commit_writeback_without_credentials_reports_cfg_missing_and_never_reaches_the_network() {
    // 本用例**刻意不设**任何 EMR_* 环境变量：EmrConfig::from_env 在构造期
    // 就返回 E_CFG_MISSING，物理上不可能先发出请求。
    let e = psychdoc_ffi::api::commit_writeback(&commit_req()).unwrap_err();
    assert_eq!(e.code(), "E_CFG_MISSING");
    assert!(e.message().contains("不会降级"));
}

#[test]
fn the_dryrun_builds_a_transaction_with_provenance_and_makes_no_network_call() {
    let out = build_writeback_preview(&commit_req(), "e4eda0c79c95e4eda").expect("干跑成功");
    assert_eq!(out["entry_count"], 1);
    // 幂等键 = sha256(住院别名 + 件 id + 内容哈希)，随 Provenance 一起出
    let keys = out["idempotency_keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0].as_str().unwrap().len(), 64);
    let bundle = &out["bundle"];
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "transaction");
    let entries = bundle["entry"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "1 件 + 1 条 Provenance");
    assert!(entries.iter().all(|e| e["request"].is_object()), "每条 entry 必须带 request");
    assert_eq!(entries[1]["resource"]["resourceType"], "Provenance");
}

#[test]
fn the_dryrun_refuses_an_empty_selection_with_a_readable_code() {
    let mut req = commit_req();
    req.entries.clear();
    req.content_hashes.clear();
    let e = build_writeback_preview(&req, "x").unwrap_err();
    assert_eq!(e.code(), "E_NOTHING_SELECTED");
}

// ---------------------------------------------------------------------------
// 错误码收口
// ---------------------------------------------------------------------------

#[test]
fn api_errors_carry_a_stable_code_never_a_chinese_phrase() {
    let e = ApiError::from_display("E_RULESET_INVALID: 规则码在 院级 层内重复声明：QC-PSY-01".into());
    assert_eq!(e.code(), "E_RULESET_INVALID");
    // 没有 E_ 前缀的文案不得被当成错误码
    let e2 = ApiError::from_display("读不到文书包 — 没有那个文件".into());
    assert_eq!(e2.code(), "E_INTERNAL");
}

// ---------------------------------------------------------------------------
// 入口 5：三态输入（目录 / 单份文件 / Bundle 文本）
// ---------------------------------------------------------------------------

#[test]
fn graph_accepts_a_directory_and_agrees_with_the_single_bundle_answer() {
    // 归档视图面对的是整个导出目录；边不跨住院别名，因此并图后
    // 同一节点的答案必须与只看那一份包时**完全一致**。
    let dir = root().join("fixtures").to_string_lossy().to_string();
    let from_dir = graph_both_directions(&dir, &ruleset(), "WARD-B-001-RECORD-01").expect("目录出图");
    let from_file = graph_both_directions(
        &root().join("fixtures/ward-02-restraint-set.json").to_string_lossy(),
        &ruleset(),
        "WARD-B-001-RECORD-01",
    )
    .expect("单份出图");
    assert_eq!(from_dir["dependencies"], from_file["dependencies"]);
    assert_eq!(from_dir["dependents"], from_file["dependents"]);
    assert_eq!(from_dir["is_blocked"], from_file["is_blocked"]);
}

#[test]
fn graph_still_accepts_raw_bundle_text() {
    let out = graph_both_directions(&fixture("ward-02-restraint-set.json"), &ruleset(), "WARD-B-001-RECORD-01")
        .expect("文本出图");
    assert_eq!(out["exists"], true);
}

#[test]
fn graph_rejects_a_path_that_is_neither_a_file_nor_json() {
    let e = graph_both_directions("/绝对不存在的导出目录", &ruleset(), "X").unwrap_err();
    assert_eq!(e.code(), "E_BUNDLE_SHAPE");
    assert!(e.message().contains("既不是可读的路径"));
}
