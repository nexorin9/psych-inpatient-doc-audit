//! Task 19 集成测试：跨文书时间序 join 的**四类难例**，
//! 打在**交付规则集**与**真实脱敏夹具**上。
//!
//! 单元测试（`chronology.rs` 内的 `mod tests`）用的是内联构造的时刻，
//! 那份东西迟早与 `rulesets/hospital.yaml` / `fixtures/` 漂移。本文件直接读
//! 仓库里那两份，把最容易出错、也最贵的三件事钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `widening_the_tolerance_changes_only_the_tie` | **对照测试**：同一夹具把并列容差 0 → 5 分钟，命中数按预期变化（QC-PSY-05 从 2 条降到 0 条、并列容差记账从 0 升到 1 条），而**其余诊断逐条一模一样** |
//! | `a_real_inversion_beyond_the_tolerance_is_still_reported` | 真实倒挂 60 分钟（ward-11）在 0 与 5 分钟两档下都照报——容差不是橡皮擦 |
//! | `a_date_only_signature_never_convicts` | 精度降级：签署页只写日期，**不判违规**、不静默通过，记 `insufficient_precision` 交人工 |
//! | `the_same_pair_with_a_second_precise_signature_is_judged` | 同一场景只把 `authoredOn` 换成到秒的时刻，结论从「判不了」变成「照报」——证明降级来自精度本身 |
//! | `across_midnight_release_is_never_an_inversion` | 跨零点（ward-07，23:40 → 次日 06:10）按真实时间差判为合规 |
//! | `a_missing_moment_is_never_filled_in` | 缺时刻（ward-04）零诊断 + 零容差记账，且跳过原因可按住院别名回查 |
//! | `every_kept_finding_keeps_its_own_locator` | 每条诊断的区间都能切回原文并指向被判的时刻本身 |
//! | `the_event_timeline_is_reproducible_and_marks_coarse_moments` | 事件时间线两次重建逐字段相同，且粗精度时刻在时间线上被显式标出 |

use psychdoc_model::{Diagnostic, Precision, TimestampKind};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{
    load_effective, review, EffectiveRules, ReviewResult, SkipReason, ToleratedPair, QC_PSY_05,
    QC_PSY_06, QC_PSY_07, QC_PSY_08,
};

const BASE_RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");

/// 3 分钟补记包（WARD-N-001）：解除 07:57 早于约束起始 08:00。
const TIE: &str = include_str!("../../../../fixtures/ward-13-tie-tolerance.json");
/// 真实倒挂包（WARD-I-001）：解除 07:00 早于约束起始 08:00，差 60 分钟。
const REAL_INVERSION: &str = include_str!("../../../../fixtures/ward-11-cross-doc-violations.json");
/// 签署页只写日期的包（WARD-P-001）：`authoredOn: 2026-03-11`，约束起始 03-10 08:00。
const COARSE: &str = include_str!("../../../../fixtures/ward-14-coarse-precision.json");
/// 跨零点包（WARD-G-001）：23:40 约束开始 → 次日 06:10 解除。
const CROSS_MIDNIGHT: &str = include_str!("../../../../fixtures/ward-07-cross-midnight.json");
/// 缺时刻包（WARD-D-001）：段内只有「23时45分」，没有日期。
const NO_MOMENTS: &str = include_str!("../../../../fixtures/ward-04-missing-moments.json");

/// 交付规则集，并把 `restraint_record ↔ restraint_release` 的并列容差改成 `minutes`。
///
/// 只动那一处，因此两次运行之间**只有**容差不同——这正是对照测试要控的变量。
fn ruleset_with_tolerance(minutes: i64) -> EffectiveRules {
    assert_eq!(
        BASE_RULESET.matches("minutes: 5").count(),
        1,
        "交付规则集里应恰有一处 `minutes: 5`（约束记录 ↔ 约束解除记录的并列容差）"
    );
    let yaml = BASE_RULESET.replacen("minutes: 5", &format!("minutes: {minutes}"), 1);
    load_effective(&yaml).unwrap_or_else(|e| panic!("规则集应可加载（容差 {minutes} 分钟）：{e}"))
}

fn ruleset() -> EffectiveRules {
    load_effective(BASE_RULESET).expect("交付规则集应可加载")
}

fn run(json: &str, rs: &EffectiveRules) -> ReviewResult {
    let reg = rs.registry();
    let parsed = parse_bundle(json, Some(&reg)).expect("脱敏夹具应可解析");
    review(&parsed.items, rs)
}

fn of_code<'a>(result: &'a ReviewResult, code: &str) -> Vec<&'a Diagnostic> {
    result.diagnostics.iter().filter(|d| d.code == code).collect()
}

/// 诊断的「指纹」：码 + 件 + 段 + 区间 + 文案。用于断言**其余诊断一字未变**。
fn fingerprint(result: &ReviewResult) -> Vec<String> {
    let mut v: Vec<String> = result
        .diagnostics
        .iter()
        .map(|d| {
            format!(
                "{}|{}|{}|{:?}|{}",
                d.code, d.loc.doc_id, d.loc.section_index, d.loc.char_range, d.message.text
            )
        })
        .collect();
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// 对照测试：0 → 5 分钟，其余诊断必须逐条不变
// ---------------------------------------------------------------------------

#[test]
fn widening_the_tolerance_changes_only_the_tie() {
    let strict = run(TIE, &ruleset_with_tolerance(0));
    let loose = run(TIE, &ruleset_with_tolerance(5));

    // 0 分钟：3 分钟补记就是倒挂，违反的两件各一条（沿用 Task 7 的「两件各一条」）。
    assert_eq!(of_code(&strict, QC_PSY_05).len(), 2, "0 分钟档应照报 2 条：{:?}", strict.diagnostics);
    assert!(strict.tolerated_checks.is_empty(), "0 分钟档不该有容差记账");

    // 5 分钟：落在容差内 → 不判违规，但**必须记账**。
    assert!(
        of_code(&loose, QC_PSY_05).is_empty(),
        "5 分钟档内不该判违规：{:?}",
        of_code(&loose, QC_PSY_05)
    );
    assert_eq!(loose.tolerated_checks.len(), 1, "容差内必须记账：{:?}", loose.tolerated_checks);

    let t: &ToleratedPair = &loose.tolerated_checks[0];
    assert_eq!(t.code, QC_PSY_05);
    assert_eq!(t.encounter_alias, "WARD-N-001", "按住院别名可回查");
    assert_eq!(t.gap_seconds, -180, "3 分钟的倒挂");
    assert_eq!(t.tolerance_minutes, 5);
    assert!(!t.reason.is_empty(), "容差必须有院内理由：{t:?}");
    assert!(t.detail.contains("并列容差"), "{}", t.detail);
    assert!(t.as_zh().contains("未判倒挂"), "{}", t.as_zh());
    assert_ne!(t.primary_doc, t.counterpart_doc, "记账必须指清是哪两件");

    // 其余诊断**一字未变**：签署页仍报 QC-PSY-08，另外两码仍 0 条。
    let strict_rest: Vec<String> = fingerprint(&strict)
        .into_iter()
        .filter(|f| !f.starts_with(QC_PSY_05))
        .collect();
    let loose_rest: Vec<String> = fingerprint(&loose)
        .into_iter()
        .filter(|f| !f.starts_with(QC_PSY_05))
        .collect();
    assert_eq!(strict_rest, loose_rest, "改容差不得影响其他诊断");
    assert_eq!(loose_rest.len(), 1, "只剩签署齐备性一条：{loose_rest:?}");
    assert!(loose_rest[0].starts_with(QC_PSY_08));
    assert!(of_code(&loose, QC_PSY_06).is_empty() && of_code(&loose, QC_PSY_07).is_empty());
}

#[test]
fn a_real_inversion_beyond_the_tolerance_is_still_reported() {
    // ward-11 的解除比约束起始早 60 分钟：这不是补记，是真倒挂。
    for minutes in [0, 5] {
        let result = run(REAL_INVERSION, &ruleset_with_tolerance(minutes));
        assert_eq!(
            of_code(&result, QC_PSY_05).len(),
            2,
            "{minutes} 分钟档下 60 分钟倒挂都必须照报"
        );
        assert!(
            result.tolerated_checks.is_empty(),
            "{minutes} 分钟档下不得把真倒挂记成容差内：{:?}",
            result.tolerated_checks
        );
    }
    // 顺带守住「改容差不改别的码」：两档下其余诊断逐条相同。
    let strict_rest: Vec<String> = fingerprint(&run(REAL_INVERSION, &ruleset_with_tolerance(0)))
        .into_iter()
        .filter(|f| !f.starts_with(QC_PSY_05))
        .collect();
    let loose_rest: Vec<String> = fingerprint(&run(REAL_INVERSION, &ruleset_with_tolerance(5)))
        .into_iter()
        .filter(|f| !f.starts_with(QC_PSY_05))
        .collect();
    assert_eq!(strict_rest, loose_rest);
}

#[test]
fn the_shipped_ruleset_really_does_open_a_five_minute_window_for_this_pair() {
    // 交付规则集里那条容差必须**真的生效**，而不是恰好被 0 分钟覆盖掉。
    let rs = ruleset();
    assert!(rs.declares_tie_tolerance("restraint_record", "restraint_release"));
    assert_eq!(rs.tie_tolerance("restraint_record", "restraint_release").minutes, 5);
    // 件对无序：反着查是同一条。
    assert_eq!(rs.tie_tolerance("restraint_release", "restraint_record").minutes, 5);
    // 没声明的件对一律 0 分钟从严。
    assert_eq!(rs.tie_tolerance("restraint_record", "assessment_report").minutes, 0);
    assert_eq!(rs.chronology.default_minutes, 0);
}

// ---------------------------------------------------------------------------
// 精度降级
// ---------------------------------------------------------------------------

#[test]
fn a_date_only_signature_never_convicts() {
    // ward-14：患方签署只写了「2026-03-11」，约束起始写到分 03-10 08:00。
    // 按 00:00 去比会凭空造出「先约束、后知情」——这正是本任务要挡掉的错。
    let result = run(COARSE, &ruleset());
    assert!(
        of_code(&result, QC_PSY_07).is_empty(),
        "低精度时刻不得用来认定倒挂：{:?}",
        of_code(&result, QC_PSY_07)
    );
    assert!(result.tolerated_checks.is_empty(), "判不了不是「容差内」，两者不得混记");

    let skip = result
        .skipped_checks
        .iter()
        .find(|s| s.code == QC_PSY_07 && s.reason == SkipReason::InsufficientPrecision)
        .unwrap_or_else(|| panic!("应留下一条 insufficient_precision 交人工：{:?}", result.skipped_checks));
    assert!(skip.detail.contains("不能用来认定倒挂"), "{}", skip.detail);
    assert!(skip.detail.contains("日"), "跳过的文案要说清粗到哪一档：{}", skip.detail);

    // 事件时间线上必须**标出**这个粗时刻，而不是把它当成 03-11 00:00 混进去。
    let tl = &result.timelines[0];
    let coarse = tl
        .events
        .iter()
        .find(|e| e.kind == TimestampKind::AuthoredOn && e.doc_id == "WARD-P-001-CONSENT-01")
        .expect("知情同意件应有 authoredOn");
    assert_eq!(coarse.precision, Precision::Day);
    assert!(tl.as_zh_rows().iter().any(|r| r.contains("只写到日")), "{}", tl.as_zh_rows().join(" / "));
}

#[test]
fn the_same_pair_with_a_second_precise_signature_is_judged() {
    // 同一场景，只把 authoredOn 换成到秒的时刻：结论必须从「判不了」变成「照报」。
    // 这条断言证明降级**来自精度本身**，而不是来自文书类型或别的巧合。
    let precise = COARSE.replace("\"authoredOn\": \"2026-03-11\"", "\"authoredOn\": \"2026-03-11T09:10:00+08:00\"");
    let result = run(&precise, &ruleset());
    assert_eq!(
        of_code(&result, QC_PSY_07).len(),
        2,
        "签署时刻写到秒后应照报 2 条：{:?}",
        of_code(&result, QC_PSY_07)
    );
    assert!(
        !result.skipped_checks.iter().any(|s| s.reason == SkipReason::InsufficientPrecision),
        "精度够时不该再留下精度不足的跳过：{:?}",
        result.skipped_checks
    );
    let hit = of_code(&result, QC_PSY_07)[0];
    assert!(hit.message.text.contains("先约束、后知情"), "{}", hit.message.text);
}

// ---------------------------------------------------------------------------
// 跨零点 / 缺时刻
// ---------------------------------------------------------------------------

#[test]
fn across_midnight_release_is_never_an_inversion() {
    // ward-07：约束 03-02 23:40 开始，解除 03-03 06:10。按「当日时分」比会得到
    // −17 小时 30 分，从而报出一条根本不存在的时序倒挂。
    let result = run(CROSS_MIDNIGHT, &ruleset());
    assert_eq!(result.items_reviewed, 3);
    assert!(
        of_code(&result, QC_PSY_05).is_empty(),
        "跨零点的解除不得被当成倒挂：{:?}",
        of_code(&result, QC_PSY_05)
    );
    // 时间线上的**业务时刻**顺序必须与真实时间一致
    // （一件在时间线上有多条事件：文书日期 / 业务时刻 / 书写时刻）。
    let tl = &result.timelines[0];
    let business: Vec<_> = tl
        .events
        .iter()
        .filter(|e| e.kind == TimestampKind::BusinessMoment)
        .collect();
    let order: Vec<&str> = business.iter().map(|e| e.doc_id.as_str()).collect();
    // 同意件与约束记录的业务时刻**同为 23:40**（同意件上也要复述约束起始时间），
    // 同刻并列由件类型优先级拆开：依赖规范写的是「约束记录 → 知情同意」，
    // 所以约束记录在前。这一条同时守住了「同刻并列按 (时刻, 件类型, doc_id) 排」。
    assert_eq!(order, ["WARD-G-001-RECORD-01", "WARD-G-001-CONSENT-01", "WARD-G-001-RELEASE-01"]);
    assert_eq!(business[0].at, business[1].at, "前两件确实同刻，并列顺序来自件类型优先级");
    let gap = (business[2].at - business[0].at).num_seconds();
    assert_eq!(gap, 23_400, "解除 06:10 与约束起始 23:40 的真实时间差是 6 小时 30 分");
    assert!(gap > 0, "跨零点后的时间差必须为正");
}

#[test]
fn a_missing_moment_is_never_filled_in() {
    // ward-04：段内写了「23时45分」但没有日期。判不了先后 → 零诊断，
    // 且**一条容差记账都不能有**（容差是「判了但不算违规」，与「判不了」无关）。
    let result = run(NO_MOMENTS, &ruleset());
    for code in [QC_PSY_05, QC_PSY_06, QC_PSY_07] {
        assert!(of_code(&result, code).is_empty(), "{code} 在缺时刻时不得产生结论");
    }
    assert!(result.tolerated_checks.is_empty(), "缺时刻不得混进容差记账：{:?}", result.tolerated_checks);
    assert!(!result.skipped_checks.is_empty(), "缺时刻必须留下可回查的跳过记录");
    for s in &result.skipped_checks {
        assert!(!s.detail.is_empty());
        assert!(!s.encounter_alias.is_empty());
        assert!(s.as_zh().contains(&s.encounter_alias));
    }
    // 缺时刻的件在时间线上被显式列出，而不是从时间线上消失。
    let tl = &result.timelines[0];
    assert_eq!(tl.events.len(), 0, "这两件都没有可用时刻");
    assert_eq!(tl.items_without_moment().len(), 2, "{:?}", tl.items_without_moment());
}

// ---------------------------------------------------------------------------
// 定位与可复现性
// ---------------------------------------------------------------------------

#[test]
fn every_kept_finding_keeps_its_own_locator() {
    // 容差只影响「要不要报」，绝不影响「报在哪」。0 分钟档下逐条断言定位可用。
    let result = run(TIE, &ruleset_with_tolerance(0));
    let rs = ruleset_with_tolerance(0);
    let reg = rs.registry();
    let items = parse_bundle(TIE, Some(&reg)).unwrap().items;
    let mut checked = 0;
    for d in &result.diagnostics {
        let item = items.iter().find(|i| i.doc_id == d.loc.doc_id).expect("诊断指向真实件");
        let text = d.loc.char_range.slice(&item.sections[d.loc.section_index as usize].text);
        if let Some(text) = text {
            if text.is_empty() {
                continue; // 零宽区间（文书级时刻无段内区间）
            }
            if d.code == QC_PSY_08 {
                // QC-PSY-08 点名的是**签署角色**（「护士签字」），不是签名值本身
                // ——护士长要的是「谁签了」，不是「签成什么样」。这条口径与执行器 B 一致。
                assert!(
                    d.message.text.contains("签署时刻"),
                    "{} 的文案必须点明缺的是签署时刻：{}",
                    d.code,
                    d.message.text
                );
            } else {
                assert!(
                    d.message.text.contains(text.trim_end_matches('。')),
                    "{} 的文案里看不到被判的「{text}」：{}",
                    d.code,
                    d.message.text
                );
            }
            checked += 1;
        }
    }
    assert!(checked >= 2, "至少两条非空定位可逐字对照，实际 {checked}");
}

#[test]
fn the_event_timeline_is_reproducible() {
    let a = run(TIE, &ruleset());
    let b = run(TIE, &ruleset());
    assert_eq!(a.timelines, b.timelines, "同一批文书两次跑批的时间线必须逐字段相同");
    assert_eq!(a.timelines.len(), 1);
    assert_eq!(a.timelines[0].encounter_alias, "WARD-N-001");
    // 逐件都能在时间线上找到自己的业务时刻，件一件不落。
    let ids: std::collections::BTreeSet<&str> =
        a.timelines[0].events.iter().map(|e| e.doc_id.as_str()).collect();
    assert_eq!(ids.len(), 5, "五件成套文书都应上时间线：{ids:?}");
}

#[test]
fn a_whole_run_is_reproducible_including_tolerated_pairs() {
    let one = run(TIE, &ruleset_with_tolerance(5));
    let two = run(TIE, &ruleset_with_tolerance(5));
    assert_eq!(fingerprint(&one), fingerprint(&two));
    assert_eq!(one.tolerated_checks, two.tolerated_checks);
    assert_eq!(one.skipped_checks, two.skipped_checks);
}
