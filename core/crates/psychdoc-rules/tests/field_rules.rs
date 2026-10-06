//! Task 6 集成测试：执行器 A（件-字段类 `QC-PSY-01`~`04`）跑**交付规则集**。
//!
//! 单元测试用的是 `#[cfg(test)]` 里的内联夹具与内联规则集；那份东西迟早会
//! 与 `rulesets/hospital.yaml` 漂移。本文件直接读仓库里那两份，把「四条规则
//! 在真实模板 + 真实脱敏文书包上各命中 ≥1 条」与「合规包零命中」钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `four_field_rules_each_hit_at_least_once` | 四码各 ≥1 条，且**零误报** |
//! | `every_message_cites_an_in_hospital_clause` | 每条 message 都带规范条款号 |
//! | `only_the_fullwidth_case_offers_a_fix` | 只有全角归一给补写载荷，其余 `None` |
//! | `diagnostic_spans_slice_back_to_the_offending_value` | 定位能切回原文（不是「大概在这一段」） |
//! | `compliant_package_produces_zero_findings` | 误报守卫 |
//! | `a_duplicate_field_key_is_recorded_not_judged` | 同名字段跨段 → 不判定但记账 |
//!
//! 「零误报」在本文件里是可核对的数字，不是形容词：`ward-09` 跑出来就是 0 条。

use psychdoc_model::{Diagnostic, FixAvailability, Severity};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{load_effective, review, EffectiveRules, ReviewResult};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const VIOLATIONS: &str = include_str!("../../../../fixtures/ward-08-field-violations.json");
const COMPLIANT: &str = include_str!("../../../../fixtures/ward-09-field-compliant.json");
const AMBIGUOUS: &str = include_str!("../../../../fixtures/ward-10-ambiguous-field.json");

fn ruleset() -> EffectiveRules {
    load_effective(RULESET).expect("交付规则集应可加载")
}

fn run(json: &str, rs: &EffectiveRules) -> ReviewResult {
    let reg = rs.registry();
    let items = parse_bundle(json, Some(&reg)).expect("脱敏夹具应可解析").items;
    review(&items, rs)
}

fn of_code<'a>(result: &'a ReviewResult, code: &str) -> Vec<&'a Diagnostic> {
    result.diagnostics.iter().filter(|d| d.code == code).collect()
}

#[test]
fn four_field_rules_each_hit_at_least_once() {
    let rs = ruleset();
    let result = run(VIOLATIONS, &rs);
    assert_eq!(result.items_reviewed, 3);

    for code in ["QC-PSY-01", "QC-PSY-02", "QC-PSY-03", "QC-PSY-04"] {
        let hits = of_code(&result, code);
        assert!(!hits.is_empty(), "{code} 应至少命中 1 条，实际清单：{:?}", result.diagnostics);
    }

    // 「≥1 命中」不等于「乱命中」：这份包恰好四类各一条，逐条可核对。
    assert_eq!(
        result.diagnostics.len(),
        4,
        "四类违规各一条，不多不少：{:?}",
        result.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert_eq!(result.blocking_count(), 4);
    assert!(result.diagnostics.iter().all(|d| d.severity == Severity::Blocking));
}

#[test]
fn every_message_cites_an_in_hospital_clause() {
    let rs = ruleset();
    for d in &run(VIOLATIONS, &rs).diagnostics {
        assert!(
            d.message.basis.contains("《精神科住院护理常规》第"),
            "{} 的依据不是院内规范条款：{}",
            d.code,
            d.message.basis
        );
        // 依据来自生效声明，而不是代码里写死的：三级级联改一层，依据跟着变。
        assert_eq!(d.message.basis, rs.rule(&d.code).unwrap().basis);
    }
}

#[test]
fn only_the_fullwidth_case_offers_a_fix() {
    let rs = ruleset();
    let result = run(VIOLATIONS, &rs);
    for d in &result.diagnostics {
        match d.code.as_str() {
            // 全角数字 → 归一化即可，内容不变，给「预览补写」按钮
            "QC-PSY-02" => {
                assert_eq!(d.fix_availability, FixAvailability::Automatic);
                let fix = d.fix.as_ref().expect("可自动修的条目必须带补写载荷");
                assert_eq!(fix.replacement, "评估时间：2026-03-02 09:00。");
                assert!(fix.summary.contains("全角 → 半角"));
            }
            // 缺项、越界、单位错配：只给依据，不给按钮
            other => {
                assert_eq!(
                    d.fix_availability,
                    FixAvailability::None,
                    "{other} 不该给补写按钮：签字、等级、单位都不是工具能替人决定的"
                );
                assert!(d.fix.is_none());
            }
        }
        assert!(d.is_consistent(), "{} 的 fix_availability 与载荷不自洽", d.code);
    }
}

#[test]
fn diagnostic_spans_slice_back_to_the_offending_value() {
    // 定位必须能切回原文里那个**值**，而不是「大概在这一段」。
    // 切错了等于没给依据——护士长看到高亮落到别的句子上，比不报更糟。
    let rs = ruleset();
    let items = parse_bundle(VIOLATIONS, Some(&rs.registry())).unwrap().items;
    let result = review(&items, &rs);

    let mut checked = 0;
    for d in &result.diagnostics {
        let item = items.iter().find(|i| i.doc_id == d.loc.doc_id).expect("诊断指向真实件");
        let section = item
            .sections
            .get(d.loc.section_index as usize)
            .expect("诊断指向真实段落");
        let text = d.loc.char_range.slice(&section.text);
        assert!(text.is_some(), "{} 的定位切不回原文", d.code);
        let text = text.unwrap();
        if !text.is_empty() {
            // 报出来的每个值，文案里必须能看到它（值带句号时含句号）
            let shown = text.trim_end_matches('。');
            assert!(
                d.message.text.contains(shown) || shown.is_empty(),
                "{} 的文案里看不到被判的取值「{text}」：{}",
                d.code,
                d.message.text
            );
            checked += 1;
        }
    }
    assert!(checked >= 3, "至少三条非空定位可逐字对照，实际 {checked}");
}

#[test]
fn compliant_package_produces_zero_findings() {
    // 误报守卫：与违规包同场景、四类都填合规，必须一条不报。
    let rs = ruleset();
    let result = run(COMPLIANT, &rs);
    assert_eq!(result.items_reviewed, 3);
    assert!(
        result.diagnostics.is_empty(),
        "合规包跑出 {} 条：{:?}",
        result.diagnostics.len(),
        result.diagnostics
    );
    assert_eq!(result.blocking_count(), 0);
}

#[test]
fn a_duplicate_field_key_is_recorded_not_judged() {
    // 同一件里「观察记录」出现在两段（模板未声明段落）：取任一段都可能给出
    // 一条建立在错数据上的结论，因此**不判定**——但也不许悄悄消失，
    // 要进人工队列可回查的记账。
    let rs = ruleset();
    let result = run(AMBIGUOUS, &rs);
    assert_eq!(result.items_reviewed, 1);
    assert!(
        result.diagnostics.is_empty(),
        "歧义字段不该出诊断（更不该出错的）：{:?}",
        result.diagnostics
    );
    let amb = &result.ambiguous_fields;
    assert_eq!(amb.len(), 1, "应恰好记账一条歧义：{amb:?}");
    assert_eq!(amb[0].key, "观察记录");
    assert_eq!(amb[0].doc_id, "WARD-H-001-RECORD-01");
    assert_eq!(amb[0].sections, vec![0, 1], "歧义要记清落在哪几段，人工才知道去核对哪里");
}

#[test]
fn a_rule_absent_from_the_ruleset_does_not_run() {
    // 注册表里有 QC-PSY-02~04，但院内只启用了 QC-PSY-01 的规则集里，
    // 没声明的码**一条都不该跑**（否则改规则集改不动行为）。
    let only_01 = r#"
version: "only-01"
templates:
  assessment_report:
    display: 评估报告
    fields:
      - key: 评估时间
        section: 入院评估
        required: true
        value_type: datetime
      - key: 评估医师
        section: 入院评估
        required: true
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        section: 患方知情同意
        required: true
        value_type: text
hospital:
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      applies_to: ["assessment_report", "restraint_consent"]
      basis: "《精神科住院护理常规》第 4.2 条"
"#;
    let rs = load_effective(only_01).unwrap();
    // 违规包里：评估时间是全角（QC-PSY-02 会命中）、约束等级越界（QC-PSY-03）、
    // 时长单位错（QC-PSY-04）；这份规则集只声明了 QC-PSY-01 → 只该出 QC-PSY-01。
    let result = run(VIOLATIONS, &rs);
    let codes: Vec<&str> = result.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(codes, vec!["QC-PSY-01"], "规则集未声明的码一条都不许执行");
    assert!(rs.rule("QC-PSY-02").is_none());
    assert!(rs.rule("QC-PSY-03").is_none());
    assert!(rs.rule("QC-PSY-04").is_none());
}

#[test]
fn a_rule_narrowed_by_department_stops_firing_on_its_own_type() {
    // 「科室把住院记录降为提示」这一类覆盖，作用范围由 applies_to 决定；
    // 这里验的是执行器侧：applies_to 不含该文书类型时，一条都不跑。
    let narrow = RULESET.replace(
        "    - code: \"QC-PSY-03\"\n      severity: blocking",
        "    - code: \"QC-PSY-03\"\n      severity: notice",
    )
    .replace(
        "      applies_to:\n        - restraint_record\n        - assessment_report\n      basis: \"《精神科住院护理常规》第 5.3 条",
        "      applies_to:\n        - assessment_report\n      basis: \"《精神科住院护理常规》第 5.3 条",
    );
    let rs = load_effective(&narrow).expect("收窄后的规则集应可加载");
    assert!(!rs.rule("QC-PSY-03").unwrap().applies_to_doc_type("restraint_record"));
    let result = run(VIOLATIONS, &rs);
    assert!(
        of_code(&result, "QC-PSY-03").is_empty(),
        "作用范围外的文书类型不该命中 QC-PSY-03"
    );
    assert!(!of_code(&result, "QC-PSY-04").is_empty(), "别的码不受影响");
}