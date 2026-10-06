//! Task 7 集成测试：执行器 B（跨文书时间序与签署齐备性 `QC-PSY-05`~`08`）
//! 跑**交付规则集**与**真实脱敏夹具**。
//!
//! 单元测试用的是 `#[cfg(test)]` 里的内联夹具与内联规则集，那份东西迟早会与
//! `rulesets/hospital.yaml` / `fixtures/` 漂移。本文件直接读仓库里那两份，
//! 把执行器 B 的关键行为钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `four_cross_doc_rules_each_hit_at_least_once` | 4 类跨文书违规各命中 ≥2 条（违反的两件各一条），且执行器 A 零命中 |
//! | `each_violating_document_gets_its_own_locator` | 违反的两件各自带自己的 `(doc_id, section_index, char_range)`，且能切回原文 |
//! | `every_cross_doc_message_cites_an_in_hospital_clause` | 每条 message 都引用院内规范条款（来自生效声明，不是代码写死） |
//! | `no_cross_doc_rule_ever_offers_a_fix` | 四码全部 `FixAvailability::None`——绝不生成签名动作 |
//! | `compliant_package_produces_zero_findings` | 误报守卫：同场景顺序全对 → 0 条 |
//! | `missing_moments_are_recorded_not_judged` | 抽不出时刻的包零诊断，但每条跳过都带原因可回查 |
//! | `ambiguous_items_skip_the_whole_pair` | 跨系统同名文书 → 整对跳过，不取第一件当答案 |
//! | `cross_system_dup_fixture_never_joins_two_copies` | 真实夹具 ward-03 的两份约束记录不得被配成一对 |
//! | `a_rule_absent_from_the_ruleset_does_not_run` | 规则集未声明的码一条都不执行 |

use psychdoc_model::{Diagnostic, FixAvailability, Severity};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{
    load_effective, review, EffectiveRules, ReviewResult, SkipReason, QC_PSY_05, QC_PSY_06,
    QC_PSY_07, QC_PSY_08,
};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const VIOLATIONS: &str = include_str!("../../../../fixtures/ward-11-cross-doc-violations.json");
const COMPLIANT: &str = include_str!("../../../../fixtures/ward-12-cross-doc-compliant.json");
const MISSING_MOMENTS: &str = include_str!("../../../../fixtures/ward-04-missing-moments.json");
const CROSS_SYSTEM_DUP: &str = include_str!("../../../../fixtures/ward-03-cross-system-dup.json");

fn ruleset() -> EffectiveRules {
    load_effective(RULESET).expect("交付规则集应可加载")
}

fn run(json: &str, rs: &EffectiveRules) -> ReviewResult {
    let reg = rs.registry();
    let parsed = parse_bundle(json, Some(&reg)).expect("脱敏夹具应可解析");
    review(&parsed.items, rs)
}

fn of_code<'a>(result: &'a ReviewResult, code: &str) -> Vec<&'a Diagnostic> {
    result.diagnostics.iter().filter(|d| d.code == code).collect()
}

const CROSS_CODES: [&str; 4] = [QC_PSY_05, QC_PSY_06, QC_PSY_07, QC_PSY_08];

#[test]
fn four_cross_doc_rules_each_hit_at_least_once() {
    let rs = ruleset();
    let result = run(VIOLATIONS, &rs);
    assert_eq!(result.items_reviewed, 5, "违规包 5 件成套文书");

    // 三条跨文书时序规则各给**违反的两件各一条**（护士长在任一件里都看得到
    // 「和哪一件对不上」）；QC-PSY-08 是单件规则，两个签署角色在一条里点名。
    for code in [QC_PSY_05, QC_PSY_06, QC_PSY_07] {
        let hits = of_code(&result, code);
        assert_eq!(hits.len(), 2, "{code} 应给违反的两件各一条；清单：{:?}", result.diagnostics);
    }
    let sign_hits = of_code(&result, QC_PSY_08);
    assert_eq!(sign_hits.len(), 1, "签署齐备性是单件规则，两个签署角色在一条里点名");
    assert!(sign_hits[0].message.text.contains("护士签字") && sign_hits[0].message.text.contains("医师签字"));

    // 「≥2 条」不等于「乱报」：执行器 A（件-字段类）在这份包里必须**零命中**——
    // 这四类违规全部只在件与件之间才看得见，件内字段都是合规的。
    let field_codes: Vec<&str> = result
        .diagnostics
        .iter()
        .map(|d| d.code.as_str())
        .filter(|c| !CROSS_CODES.contains(c))
        .collect();
    assert!(
        field_codes.is_empty(),
        "违规包里件内字段全部合规，执行器 A 不该报；多出：{field_codes:?}"
    );
    // 恰好 4 类：3×2 + 1 = 7 条，不多不少。
    assert_eq!(result.diagnostics.len(), 7, "{:?}", result.diagnostics);
    assert_eq!(result.blocking_count(), 7);
    assert!(result.diagnostics.iter().all(|d| d.severity == Severity::Blocking));
}

#[test]
fn each_violating_document_gets_its_own_locator() {
    // 跨文书违规最容易被做成「只给一个位置」——那样护士长在另一件里就没有
    // 核对入口。逐条断言：每条诊断指向的区间都能切回原文，且切出来的是
    // **被比对的时刻本身**。
    let rs = ruleset();
    let reg = rs.registry();
    let items = parse_bundle(VIOLATIONS, Some(&reg)).unwrap().items;
    let result = review(&items, &rs);

    let mut checked = 0;
    for d in &result.diagnostics {
        let item = items.iter().find(|i| i.doc_id == d.loc.doc_id).expect("诊断指向真实件");
        let section = item
            .sections
            .get(d.loc.section_index as usize)
            .expect("诊断指向真实段落");
        let text = d.loc.char_range.slice(&section.text);
        let text = text.expect("定位必须能切回原文");
        if text.is_empty() {
            // 零宽区间（文书级时刻无段内区间）跳过，但件 id 仍必须是真的。
            assert!(items.iter().any(|i| i.doc_id == d.loc.doc_id));
            continue;
        }
        if d.code == QC_PSY_08 {
            // QC-PSY-08 的定位指向**第一个已填的签署位**，文案点名的是签署角色
            // 而不是签名值本身——护士长要的是「谁签了」，不是「签成什么样」。
            assert!(
                d.message.text.contains("签署时刻"),
                "{} 的文案必须点明缺的是签署时刻：{}",
                d.code,
                d.message.text
            );
        } else {
            assert!(
                d.message.text.contains(text.trim_end_matches('。')),
                "{} 的文案里看不到被判的时刻「{text}」：{}",
                d.code,
                d.message.text
            );
        }
        checked += 1;
    }
    assert!(checked >= 4, "至少四条非空定位可逐字对照，实际 {checked}");

    // 同一码下两个 doc_id 必须不同——「两件各一条」不是同一件报两遍。
    for code in [QC_PSY_05, QC_PSY_06, QC_PSY_07] {
        let ids: Vec<&str> = of_code(&result, code).iter().map(|d| d.loc.doc_id.as_str()).collect();
        assert_eq!(ids.len(), 2, "{code} 应两条：{ids:?}");
        assert_ne!(ids[0], ids[1], "{code} 的两条必须落在两个不同的件上：{ids:?}");
    }
}

#[test]
fn every_cross_doc_message_cites_an_in_hospital_clause() {
    let rs = ruleset();
    for d in &run(VIOLATIONS, &rs).diagnostics {
        assert!(
            d.message.basis.contains("《精神科住院护理常规》第"),
            "{} 的依据不是院内规范条款：{}",
            d.code,
            d.message.basis
        );
        // 依据来自生效声明而非代码写死：三级级联改一层，依据跟着变。
        assert_eq!(d.message.basis, rs.rule(&d.code).unwrap().basis);
    }
}

#[test]
fn no_cross_doc_rule_ever_offers_a_fix() {
    // 时序倒挂与签署齐备性都不是工具能替人决定的：
    // 补一个时刻等于篡改病历，替人签名更越界。四码一律只给依据不给按钮。
    let rs = ruleset();
    for d in &run(VIOLATIONS, &rs).diagnostics {
        assert_eq!(
            d.fix_availability,
            FixAvailability::None,
            "{} 不该给补写按钮：{}",
            d.code,
            d.message.text
        );
        assert!(d.fix.is_none());
        assert!(d.is_consistent());
    }
}

#[test]
fn compliant_package_produces_zero_findings() {
    // 误报守卫：与违规包同场景，评估在约束前、签署在约束前、解除在约束后、
    // 签署页带签署时刻 → 一条都不该报。
    let rs = ruleset();
    let result = run(COMPLIANT, &rs);
    assert_eq!(result.items_reviewed, 5);
    assert!(
        result.diagnostics.is_empty(),
        "合规包跑出 {} 条：{:?}",
        result.diagnostics.len(),
        result.diagnostics
    );
    assert_eq!(result.blocking_count(), 0);
    // 四码在合规包里**一条都判不了**的理由只能是「对照件不在包里」——
    // 这一包里五类件齐全，跨件规则应真正跑过并给出「无违规」。
    for code in CROSS_CODES {
        assert!(
            !result.skipped_checks.iter().any(|s| s.code == code),
            "{code} 在件齐全的合规包里不该留下跳过记录：{:?}",
            result.skipped_checks
        );
    }
}

#[test]
fn missing_moments_are_recorded_not_judged() {
    // 真实夹具 ward-04：两件都只有「23时45分」这种**没有日期**的时刻。
    // 判不了先后 → 三条时间序规则零诊断，但每条跳过都必须带原因，
    // 可按住院别名回查。绝不拿当前时间兜底后判一个「看起来正常」的结论。
    let rs = ruleset();
    let result = run(MISSING_MOMENTS, &rs);
    for code in [QC_PSY_05, QC_PSY_06, QC_PSY_07] {
        assert!(
            of_code(&result, code).is_empty(),
            "{code} 在缺时刻时不得产生任何跨文书结论：{:?}",
            of_code(&result, code)
        );
    }
    assert!(
        !result.skipped_checks.is_empty(),
        "缺时刻必须留下可回查的跳过记录，不能静默放过"
    );
    for s in &result.skipped_checks {
        assert!(!s.detail.is_empty(), "跳过记录必须写清为什么：{s:?}");
        assert!(!s.encounter_alias.is_empty());
        assert!(!s.as_zh().is_empty());
    }
    assert!(
        result
            .skipped_checks
            .iter()
            .all(|s| s.reason == SkipReason::NoCounterpart || s.reason == SkipReason::MissingTimestamp),
        "跳过原因只能是缺对照件或缺时刻：{:?}",
        result.skipped_checks
    );
    // 这一份里签署页的护士签字已填、医师签字空且无 authoredOn：
    // 「签了名却没时刻」是真问题（QC-PSY-08 命中），「压根没签」归 QC-PSY-01。
    // 两条同时出现正是职责边界分对了的表现，不是重复报。
    assert_eq!(of_code(&result, QC_PSY_08).len(), 1, "{:?}", of_code(&result, QC_PSY_08));
    assert_eq!(of_code(&result, "QC-PSY-01").len(), 1, "{:?}", of_code(&result, "QC-PSY-01"));
}

#[test]
fn ambiguous_items_skip_the_whole_pair() {
    // ward-03：HIS 与护理系统各导了一份「约束记录」。取任一份都可能给出
    // 一条建立在错配对上的结论 → 整对跳过，绝不取第一件当答案。
    let rs = ruleset();
    let result = run(CROSS_SYSTEM_DUP, &rs);
    let cross: Vec<&Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| CROSS_CODES.contains(&d.code.as_str()))
        .collect();
    assert!(cross.is_empty(), "同名多件时不得下结论：{cross:?}");
    assert!(
        result
            .skipped_checks
            .iter()
            .any(|s| s.reason == SkipReason::NoCounterpart || s.reason == SkipReason::AmbiguousBinding),
        "至少留一条可回查的跳过记录：{:?}",
        result.skipped_checks
    );
}

#[test]
fn cross_system_dup_fixture_never_joins_two_copies() {
    // 明确守住「跨系统同名不得静默配对」：两份约束记录一件都不该
    // 被当成另一件的对端去连边。
    let rs = ruleset();
    let reg = rs.registry();
    let items = parse_bundle(CROSS_SYSTEM_DUP, Some(&reg)).unwrap().items;
    let result = review(&items, &rs);
    let docs_in_cross: Vec<&str> = result
        .diagnostics
        .iter()
        .filter(|d| CROSS_CODES.contains(&d.code.as_str()))
        .map(|d| d.loc.doc_id.as_str())
        .collect();
    assert!(
        docs_in_cross.is_empty(),
        "同名多件时不该产出任何跨件诊断：{docs_in_cross:?}"
    );
}

#[test]
fn a_rule_absent_from_the_ruleset_does_not_run() {
    // 院内只启用了件-字段类规则时，跨文书四码一条都不该跑——
    // 否则「改规则集改不动行为」这条纪律在执行器 B 上就破了。
    let minimal = r#"
version: "field-only"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        section: 患方知情同意
        required: true
  restraint_record:
    display: 约束记录
    fields: []
  restraint_release:
    display: 约束解除记录
    fields: []
  assessment_report:
    display: 评估报告
    fields: []
  sign_page:
    display: 签署页
    fields:
      - key: 护士签字
        section: 签署页
        required: true
hospital:
  doc_types:
    restraint_consent: "约束知情同意"
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      applies_to: ["restraint_consent"]
      basis: "《精神科住院护理常规》第 4.2 条"
"#;
    let rs = load_effective(minimal).unwrap();
    let result = run(VIOLATIONS, &rs);
    for code in CROSS_CODES {
        assert!(rs.rule(code).is_none(), "{code} 不应存在于这份规则集");
        assert!(of_code(&result, code).is_empty(), "{code} 未声明却执行了");
    }
    // 未声明的码也不该留下跳过记录——它压根没跑。
    assert!(
        !result
            .skipped_checks
            .iter()
            .any(|s| CROSS_CODES.contains(&s.code.as_str())),
        "未声明的码不该留下跳过记录：{:?}",
        result.skipped_checks
    );
}

#[test]
fn skip_reasons_are_stable_and_machine_readable() {
    // 跳过原因会被 UI 与错误码表引用，因此序列化后的名字必须固定。
    for (reason, want) in [
        (SkipReason::NoCounterpart, "no_counterpart"),
        (SkipReason::AmbiguousBinding, "ambiguous_binding"),
        (SkipReason::MissingTimestamp, "missing_timestamp"),
        (SkipReason::EqualMoment, "equal_moment"),
        (SkipReason::InsufficientPrecision, "insufficient_precision"),
    ] {
        let json = serde_json::to_string(&reason).unwrap();
        assert_eq!(json, format!("\"{want}\""));
        assert!(!reason.as_zh().is_empty());
    }
    // 「看起来像倒挂但其实不能判」的两类必须能被单独识别出来。
    assert!(SkipReason::EqualMoment.is_inconclusive());
    assert!(SkipReason::InsufficientPrecision.is_inconclusive());
    assert!(!SkipReason::MissingTimestamp.is_inconclusive());
    assert!(!SkipReason::NoCounterpart.is_inconclusive());
}
