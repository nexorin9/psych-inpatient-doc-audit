//! Task 18 集成测试：豁免谓词化打在**真实脱敏文书包**上。
//!
//! `waiver.rs` 的单元测试用内联 `WaiverPredicate` 验证判定语义；本文件把两级
//! 语义、时间窗边界与溯源记录放到 `fixtures/` 的真实文书包上跑，钉死它作为
//! 「病区能不能不交这份文书」这一业务动作的落点：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `not_applicable_removes_the_finding_from_the_statistics` | 整条不适用 → 不进命中统计，`waived` 留溯源 |
//! | `downgrade_keeps_the_finding_but_stops_it_from_blocking` | 降级为提示 → 仍计入命中，`blocking_count` 归零 |
//! | `the_two_levels_are_not_interchangeable` | 两级语义互不等价（各有 1 条各自独有的断言） |
//! | `the_window_opens_and_closes_on_its_own` | 时间窗外自动失效；窗口内仍生效 |
//! | `a_window_across_midnight_is_compared_by_real_elapsed_time` | 跨零点按真实时间比较，不按当日时分 |
//! | `a_predicate_pointing_at_a_nonexistent_ward_code_never_fires` | 病区码写错 → 一条都不放行（不静默全放行） |
//! | `an_illegal_window_is_rejected_at_load_time` | 时间窗格式非法 → `E_RULESET_INVALID`，零条规则放行 |
//! | `the_most_specific_waiver_wins_and_the_others_are_recorded_as_superseded` | 多条命中 → 取最具体，其余记 `superseded` 不丢弃 |
//! | `every_waiver_can_be_answered_by_encounter_alias` | 逐条豁免可按住院别名回查 |

use psychdoc_model::{DocItem, Severity};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{
    dead_waivers, load_effective, released_rule_count, review, EffectiveRules, PredicateCombine,
    WaiverEffect,
};

const BASE_RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");

/// 违规包：WARD-F-001（`ward-08`），3 件，含 QC-PSY-01~04 四类违规。
const VIOLATIONS: &str = include_str!("../../../../fixtures/ward-08-field-violations.json");
/// 合规包：WARD-G-001（`ward-09`），同场景全合规 → 零命中。
const COMPLIANT: &str = include_str!("../../../../fixtures/ward-09-field-compliant.json");
/// 缺时刻包：WARD-D-001（`ward-04`），件上没有任何可用时刻。
const NO_MOMENTS: &str = include_str!("../../../../fixtures/ward-04-missing-moments.json");
/// 跨零点包：WARD-G-001（`ward-07`），23:30 / 23:40 / 次日 06:10。
const CROSS_MIDNIGHT: &str = include_str!("../../../../fixtures/ward-07-cross-midnight.json");

fn items(json: &str) -> Vec<DocItem> {
    let rs = load_effective(BASE_RULESET).expect("交付规则集应可加载");
    let reg = rs.registry();
    parse_bundle(json, Some(&reg)).expect("脱敏夹具应可解析").items
}

/// 在交付规则集上替换 `per-ward-ignores: []` 这一行，其余保持不变。
///
/// 刻意**就地替换**而不是「截掉尾部再追加」：交付规则集尾部还带着
/// `chronology`（并列容差）段，按尾部截断会把它一起吃掉，
/// 那这些用例跑的就不再是真实交付规则集了。
fn ruleset_with(waivers_yaml: &str) -> String {
    const ANCHOR: &str = "per-ward-ignores: []";
    assert_eq!(
        BASE_RULESET.matches(ANCHOR).count(),
        1,
        "交付规则集里应有且只有一处 {ANCHOR}，本用例的替换假设需要同步更新"
    );
    BASE_RULESET.replacen(ANCHOR, waivers_yaml.trim_end(), 1)
}

fn effective(yaml: &str) -> EffectiveRules {
    load_effective(yaml).unwrap_or_else(|e| panic!("规则集应可加载：{e}"))
}

/// 一条豁免 YAML（缩进已按 `per-ward-ignores:` 下的列表项写好）。
const W: &str = r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    reason: "该病区约束知情同意由护士长统一签署，患方签字栏不随件走"
"#;

// ---------------------------------------------------------------------------
// 两级语义
// ---------------------------------------------------------------------------

#[test]
fn not_applicable_removes_the_finding_from_the_statistics() {
    let yaml = ruleset_with(W);
    let rs = effective(&yaml);
    let items = items(VIOLATIONS);
    let baseline = review(&items, &effective(BASE_RULESET));
    assert!(
        baseline.diagnostics.iter().any(|d| d.code == "QC-PSY-01"),
        "未加豁免时 WARD-F-001 应命中 QC-PSY-01，否则本用例是假绿"
    );

    let result = review(&items, &rs);
    assert!(
        !result.diagnostics.iter().any(|d| d.code == "QC-PSY-01"),
        "整条不适用后 QC-PSY-01 不应再出现在诊断清单里"
    );
    // 不计入命中统计：其余码照报，命中数恰好少一条。
    assert_eq!(
        result.diagnostics.len(),
        baseline.diagnostics.len() - 1,
        "整条不适用只应抹掉被豁免的那一条，其余照报"
    );
    // 但溯源必须留着。
    assert_eq!(result.waived_count, 1);
    assert!(result.is_waiver_accounting_balanced(), "计数与明细必须一致");
    let w = &result.waived[0];
    assert_eq!(w.code, "QC-PSY-01");
    assert_eq!(w.ward, "WARD-F");
    assert_eq!(w.encounter_alias, "WARD-F-001");
    assert_eq!(w.doc_type, "restraint_consent");
    assert_eq!(w.effect, WaiverEffect::NotApplicable);
    assert!(w.reason.contains("护士长统一签署"), "理由必须可回查：{}", w.reason);
    assert!(w.predicate.contains("WARD-F"), "谓词摘要应含病区：{}", w.predicate);
    assert!(w.predicate.contains("QC-PSY-01"), "谓词摘要应含规则码：{}", w.predicate);
    assert!(w.predicate.contains("整条不适用"), "谓词摘要应说明两级语义：{}", w.predicate);
}

#[test]
fn downgrade_keeps_the_finding_but_stops_it_from_blocking() {
    let yaml = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    effect: downgrade_to_notice
    reason: "带教期口径：患方签字本轮只提示不阻断"
"#,
    );
    let rs = effective(&yaml);
    let result = review(&items(VIOLATIONS), &rs);

    // 仍计入命中统计：诊断还在。
    let d = result
        .diagnostics
        .iter()
        .find(|d| d.code == "QC-PSY-01")
        .unwrap_or_else(|| panic!("降级为提示后 QC-PSY-01 仍应在清单里，只是严重度变了"));
    assert_eq!(d.severity, Severity::Notice, "降级后不得再阻断提交");
    assert!(
        d.waived.as_deref().unwrap_or_default().contains("带教期口径"),
        "降级条目必须带上豁免理由，UI 上点开就能看到为什么不阻断：{:?}",
        d.waived
    );

    let baseline = review(&items(VIOLATIONS), &effective(BASE_RULESET));
    assert_eq!(
        result.diagnostics.len(),
        baseline.diagnostics.len(),
        "降级为提示不改变命中条数——这是它与「整条不适用」的分水岭"
    );
    assert_eq!(result.waived_count, 1);
    assert_eq!(result.waived[0].effect, WaiverEffect::DowngradeToNotice);
    assert!(
        result.blocking_count() < baseline.blocking_count(),
        "降级后阻断数必须真的下降（{} → {}）",
        baseline.blocking_count(),
        result.blocking_count()
    );
}

#[test]
fn the_two_levels_are_not_interchangeable() {
    let not_applicable = review(&items(VIOLATIONS), &effective(&ruleset_with(W)));
    let downgraded = review(
        &items(VIOLATIONS),
        &effective(&ruleset_with(
            r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    effect: downgrade_to_notice
    reason: "带教期口径"
"#,
        )),
    );
    // 命中数不同：整条不适用少一条，降级一条不少。
    assert_eq!(
        not_applicable.diagnostics.len() + 1,
        downgraded.diagnostics.len(),
        "两级语义必须给出不同的命中条数"
    );
    // 溯源里的效果标记也不同，否则审计导出分不出「真不适用」与「仅提示」。
    assert_ne!(not_applicable.waived[0].effect, downgraded.waived[0].effect);
    // 两者都不阻断——但原因不同，所以产物里留下的凭据必须不同。
    assert_ne!(not_applicable.waived[0].predicate, downgraded.waived[0].predicate);
}

// ---------------------------------------------------------------------------
// 时间窗
// ---------------------------------------------------------------------------

#[test]
fn the_window_opens_and_closes_on_its_own() {
    // WARD-F-001 的最早时刻是 2026-03-02T08:30（知情同意的 date）。
    let inside = review(
        &items(VIOLATIONS),
        &effective(&ruleset_with(
            r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    time_window:
      from: "2026-03-01T00:00:00+08:00"
      to: "2026-03-03T00:00:00+08:00"
    reason: "试点期口径（3 月 1 日至 3 月 3 日）"
"#,
        )),
    );
    assert_eq!(inside.waived_count, 1, "件落在窗口内应放行");
    assert_eq!(
        inside.waived[0].window,
        "2026-03-01T00:00:00+08:00 ~ 2026-03-03T00:00:00+08:00",
        "溯源里必须原样写清生效窗口，供事后按当时口径核对"
    );

    // 同一份规则集，窗口挪到件之后 → 豁免自动失效，诊断照报。
    let outside = review(
        &items(VIOLATIONS),
        &effective(&ruleset_with(
            r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    time_window:
      from: "2026-04-01T00:00:00+08:00"
      to: "2026-04-30T23:59:59+08:00"
    reason: "四月试点期口径"
"#,
        )),
    );
    assert_eq!(outside.waived_count, 0, "窗口外必须自动失效");
    assert!(
        outside.diagnostics.iter().any(|d| d.code == "QC-PSY-01"),
        "窗口外照报，不能因为「配过豁免」就整条吞掉"
    );
    assert!(outside.is_waiver_accounting_balanced());
}

#[test]
fn a_window_across_midnight_is_compared_by_real_elapsed_time() {
    // ward-07：知情同意 2026-03-02T23:30、约束记录 23:40、解除记录次日 06:10。
    let yaml = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-G"
    codes: ["QC-PSY-08"]
    time_window:
      from: "2026-03-02T22:00:00+08:00"
      to: "2026-03-03T07:00:00+08:00"
    reason: "跨零点值守时段口径"
"#,
    );
    let rs = effective(&yaml);
    // 先确认这批件确实会报 QC-PSY-08，否则「豁免了 0 条」说明不了任何事。
    let baseline = review(&items(CROSS_MIDNIGHT), &effective(BASE_RULESET));
    let result = review(&items(CROSS_MIDNIGHT), &rs);
    assert!(
        result.waived_count >= baseline.diagnostics.iter().filter(|d| d.code == "QC-PSY-08").count(),
        "跨零点窗口内 QC-PSY-08 应全部放行（baseline={} waived={}）",
        baseline.diagnostics.iter().filter(|d| d.code == "QC-PSY-08").count(),
        result.waived_count
    );
    // 窗口写法是「起 22:00 → 止 次日 07:00」，若按当日时分比会算反。
    let any_window = result.waived.iter().find(|w| w.code == "QC-PSY-08");
    if let Some(w) = any_window {
        assert!(
            w.predicate.contains("2026-03-03T07:00:00+08:00"),
            "溯源里必须留原始窗口边界供核对：{}",
            w.predicate
        );
    }
}

#[test]
fn a_predicate_that_cannot_be_judged_is_recorded_not_silently_dropped() {
    // ward-04 的件没有任何可用时刻。谓词带时间窗 → 判不出来。
    let result = review(
        &items(NO_MOMENTS),
        &effective(&ruleset_with(
            r#"per-ward-ignores:
  - ward: "WARD-D"
    codes: ["QC-PSY-01"]
    time_window:
      from: "2026-03-01T00:00:00+08:00"
      to: "2026-03-31T23:59:59+08:00"
    reason: "带时间窗的口径"
"#,
        )),
    );
    // 既没放行（没有 waived），也没当成通过——判不出来必须留下痕迹。
    assert_eq!(result.waived_count, 0, "无时刻的件不得被时间窗豁免放行");
    assert!(
        result.waiver_skipped.iter().any(|s| s.detail.contains("没有可用时刻")),
        "判不出来必须记 waiver_skipped 并说明原因：{:?}",
        result.waiver_skipped
    );
    assert!(
        result.waiver_skipped.iter().all(|s| s.encounter_alias == "WARD-D-001"),
        "跳过记录要能按住院别名回查"
    );
}

// ---------------------------------------------------------------------------
// 加载期强校验与病区码
// ---------------------------------------------------------------------------

#[test]
fn a_predicate_pointing_at_a_nonexistent_ward_code_never_fires() {
    // 拼错的病区码：`WARD-K` 在本夹具里不存在。
    let rs = effective(&ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-K"
    codes: ["QC-PSY-01"]
    reason: "病区码写错了"
"#,
    ));
    let result = review(&items(VIOLATIONS), &rs);
    assert_eq!(result.waived_count, 0, "病区对不上的豁免一条都不该放行");
    assert!(
        result.diagnostics.iter().any(|d| d.code == "QC-PSY-01"),
        "WARD-F 的件照报 QC-PSY-01"
    );
    // 配了却一次都没命中 → 列为死豁免，不让它躺在配置里装样子。
    assert_eq!(result.dead_waivers.len(), 1, "死豁免必须显式列出：{:?}", result.dead_waivers);
    assert!(result.dead_waivers[0].contains("WARD-K"));
}

#[test]
fn an_illegal_window_is_rejected_at_load_time() {
    // 1) 格式非法：不是 RFC3339 时刻。
    let bad_format = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    time_window:
      from: "2026年3月1日"
      to: "2026-03-31T23:59:59+08:00"
    reason: "格式写错"
"#,
    );
    let err = load_effective(&bad_format).unwrap_err();
    assert!(err.to_string().contains("E_RULESET_INVALID"), "{err}");
    assert_eq!(released_rule_count(&bad_format), 0, "时间窗非法时零条规则被放行");

    // 2) 起点晚于终点：窗口恒为空 = 白写。
    let inverted = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    time_window:
      from: "2026-03-31T00:00:00+08:00"
      to: "2026-03-01T00:00:00+08:00"
    reason: "起止写反"
"#,
    );
    let err = load_effective(&inverted).unwrap_err();
    assert!(matches!(err, psychdoc_rules::RuleSetError::InvalidWaiverWindow { .. }), "{err}");
    assert_eq!(released_rule_count(&inverted), 0, "起止写反时零条规则被放行");
}

#[test]
fn a_waiver_with_no_constraint_at_all_is_rejected() {
    // 只写病区与理由 = 什么都不约束 = 等于关掉审查。
    let y = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    reason: "忘了写豁免哪条"
"#,
    );
    let err = load_effective(&y).unwrap_err();
    assert!(matches!(err, psychdoc_rules::RuleSetError::MissingWaiverField { .. }), "{err}");
    assert_eq!(released_rule_count(&y), 0);
}

// ---------------------------------------------------------------------------
// 多条豁免命中同一条诊断
// ---------------------------------------------------------------------------

#[test]
fn the_most_specific_waiver_wins_and_the_others_are_recorded_as_superseded() {
    // 三条同时命中 WARD-F / QC-PSY-01：
    //   A：病区 + 码（2 类）          → 最不具体
    //   B：病区 + 文书类型 + 码（3 类）→ 中
    //   C：病区 + 文书类型 + 码 + 时间窗（4 类）→ 最具体
    let yaml = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    codes: ["QC-PSY-01"]
    reason: "A：只按病区整条不适用"
  - ward: "WARD-F"
    doc_types: ["restraint_consent"]
    codes: ["QC-PSY-01"]
    effect: downgrade_to_notice
    reason: "B：按文书类型降为提示"
  - ward: "WARD-F"
    doc_types: ["restraint_consent"]
    codes: ["QC-PSY-01"]
    effect: downgrade_to_notice
    time_window:
      from: "2026-03-01T00:00:00+08:00"
      to: "2026-03-03T00:00:00+08:00"
    reason: "C：试点期窗口内降为提示"
"#,
    );
    let result = review(&items(VIOLATIONS), &effective(&yaml));
    assert_eq!(result.waived_count, 1, "三条命中同一条诊断时只放行一次");
    let w = &result.waived[0];
    assert!(w.reason.starts_with('C'), "条件类数最多的 C 应胜出，实际胜出：{}", w.reason);
    assert_eq!(w.effect, WaiverEffect::DowngradeToNotice);
    assert_eq!(w.superseded.len(), 2, "其余两条必须记为 superseded，不丢弃：{:?}", w.superseded);
    assert!(
        w.superseded.iter().any(|s| s.contains("WARD-F")) && w.superseded.len() == 2,
        "superseded 应逐条可读：{:?}",
        w.superseded
    );
    // 诊断只被放行一次，且严重度由胜出的那条决定。
    let d = result.diagnostics.iter().find(|d| d.code == "QC-PSY-01").expect("降级后仍在清单里");
    assert_eq!(d.severity, Severity::Notice);
    assert_eq!(
        result.diagnostics.iter().filter(|d| d.code == "QC-PSY-01").count(),
        1,
        "降级不应复制诊断"
    );
    assert!(result.is_waiver_accounting_balanced());
}

// ---------------------------------------------------------------------------
// 文书类型维度与溯源回查
// ---------------------------------------------------------------------------

#[test]
fn a_doc_type_constraint_narrows_the_waiver_to_that_document_type() {
    let yaml = ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    doc_types: ["restraint_record"]
    codes: ["QC-PSY-03"]
    reason: "约束等级的纸质表单口径不同，本轮不按字典判"
"#,
    );
    let result = review(&items(VIOLATIONS), &effective(&yaml));
    assert_eq!(result.waived_count, 1);
    let w = &result.waived[0];
    assert_eq!(w.doc_type, "restraint_record");
    assert_eq!(w.doc_id, "WARD-F-001-RECORD-01");
    assert!(w.predicate.contains("文书类型 restraint_record"), "{}", w.predicate);
}

#[test]
fn every_waiver_can_be_answered_by_encounter_alias() {
    let rs = effective(&ruleset_with(W));
    let result = review(&items(VIOLATIONS), &rs);
    let by_alias = result.waived_for_encounter("WARD-F-001");
    assert_eq!(by_alias.len(), 1, "按住院别名应能回查到那条豁免");
    assert_eq!(by_alias[0].code, "QC-PSY-01");
    assert!(
        result.waived_for_encounter("WARD-A-001").is_empty(),
        "别的住院别名不得串到这条豁免上"
    );
    assert!(result.dead_waivers.is_empty(), "本用例里豁免确实命中过，不该被判死豁免");
    assert_eq!(dead_waivers(&rs, &result.waived).len(), 0);
}

#[test]
fn an_or_combination_fires_when_any_written_condition_holds() {
    let rs = effective(&ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    doc_types: ["restraint_consent"]
    codes: ["QC-PSY-04"]
    combine: any
    reason: "OR 口径：命中任一条件即放行"
"#,
    ));
    let result = review(&items(VIOLATIONS), &rs);
    // 病区条件本身就成立 → OR 下 WARD-F 的全部命中一并放行。
    assert_eq!(
        result.waived_count, 4,
        "OR 下仅病区成立即放行本病区全部命中：{:?}",
        result.waived.iter().map(|w| &w.code).collect::<Vec<_>>()
    );
    assert!(
        result.waived.iter().all(|w| w.predicate.contains(" 或 ")),
        "OR 组合的可读摘要要体现出来：{:?}",
        result.waived.iter().map(|w| &w.predicate).collect::<Vec<_>>()
    );
    assert!(
        result.diagnostics.iter().all(|d| !result.waived.iter().any(|w| w.code == d.code)),
        "被放行的码不应同时留在诊断清单里"
    );
    assert!(result.is_waiver_accounting_balanced());
}

#[test]
fn an_and_combination_requires_every_written_condition() {
    let or_rs = effective(&ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    doc_types: ["restraint_consent"]
    codes: ["QC-PSY-04"]
    combine: any
    reason: "OR 口径"
"#,
    ));
    let and_rs = effective(&ruleset_with(
        r#"per-ward-ignores:
  - ward: "WARD-F"
    doc_types: ["restraint_consent"]
    codes: ["QC-PSY-04"]
    combine: all
    reason: "AND 口径"
"#,
    ));
    // QC-PSY-04 落在约束记录件上，而谓词把文书类型收窄到约束知情同意，
    // 因此文书类型条件不成立：OR 靠病区放行，AND 三条缺一不可 → 不放行。
    let or_result = review(&items(VIOLATIONS), &or_rs);
    let and_result = review(&items(VIOLATIONS), &and_rs);
    assert_eq!(
        or_result.waived.len(),
        4,
        "OR 下仅病区成立即放行本病区全部命中"
    );
    assert_eq!(
        and_result.waived_count, 0,
        "AND 下文书类型条件不成立，零条放行"
    );
    assert!(
        and_result.diagnostics.iter().any(|d| d.code == "QC-PSY-04"),
        "AND 未放行时诊断必须照报"
    );
    assert!(and_result.is_waiver_accounting_balanced());
}

// ---------------------------------------------------------------------------
// 合规包（误报守卫）：豁免不该凭空造出命中
// ---------------------------------------------------------------------------

#[test]
fn a_compliant_package_stays_clean_under_every_waiver() {
    let yaml = ruleset_with(W);
    let result = review(&items(COMPLIANT), &effective(&yaml));
    assert_eq!(
        result.diagnostics.len(),
        0,
        "合规包（WARD-G-001）加上任何豁免都不该冒出新命中：{:?}",
        result.diagnostics
    );
    assert_eq!(result.waived_count, 0, "WARD-F 的豁免不该波及 WARD-G-001");
    assert!(result.dead_waivers.len() <= 1, "至多把「本批没命中」标一次死豁免，不重复计数");
    assert_eq!(PredicateCombine::default(), PredicateCombine::All);
}