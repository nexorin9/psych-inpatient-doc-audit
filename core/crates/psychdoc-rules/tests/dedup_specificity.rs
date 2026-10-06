//! Task 23 集成测试：多规则命中同一处的**去重与择优**。
//!
//! 真实规则集互相重叠。同一个字段能同时被判「取值域越界」和「单位错配」，
//! 而整改动作其实只有一个：把这一栏改成院内规范写法。同一处刷两行，
//! 护士长会以为要改两件事；规则集越长，刷得越凶。
//!
//! 但折叠是**最容易把漏报做出来的**一步——压下去的那条若直接消失，
//! 等于让工具少报了一条。因此本文件把三件事钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `two_rules_hitting_the_same_place_collapse_into_one` | 同键收敛为 1 条 + 1 条 suppressed |
//! | `the_most_specific_rule_wins_not_the_first_one` | 单位错配（04）胜取值域越界（03） |
//! | `the_folded_entry_is_kept_not_discarded` | 压下去那条的码/依据/定位全留痕 |
//! | `folding_never_lowers_the_severity` | 严重度只上不下 |
//! | `folding_never_changes_the_verdict_on_a_clean_package` | 误报守卫 |
//! | `codes_outside_every_family_are_never_folded` | 未归族的码宁可多报一行 |
//! | `the_same_package_always_folds_the_same_way` | 可复现（审计导出能 diff） |
//!
//! 全部打在**交付规则集**与**脱敏夹具**上，不用内联替身。

use psychdoc_model::{Diagnostic, Severity};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{dedup, load_effective, review, DedupKey, EffectiveRules, ReviewResult};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
/// 同一处同时命中 QC-PSY-03（1000 分钟越界）与 QC-PSY-04（单位写成小时）。
const OVERLAP: &str = include_str!("../../../../fixtures/ward-15-overlapping-findings.json");
/// 四类违规但**各处不重叠**的包：用于守住「不误折叠」。
const SCATTERED: &str = include_str!("../../../../fixtures/ward-08-field-violations.json");
/// 全部合规的包。
const COMPLIANT: &str = include_str!("../../../../fixtures/ward-09-field-compliant.json");

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

// ——— 折叠：收敛为 1 条 + 1 条压下 ———

#[test]
fn two_rules_hitting_the_same_place_collapse_into_one() {
    let rs = ruleset();
    let result = run(OVERLAP, &rs);

    assert_eq!(
        result.diagnostics_before_dedup, 2,
        "折叠前 2 条：取值域越界（03）与单位错配（04）落在同一处同一区间"
    );
    assert_eq!(
        result.suppressed.len(), 1,
        "只应有 1 条被折叠：{:?}",
        result.suppressed.iter().map(|s| &s.code).collect::<Vec<_>>()
    );
    assert_eq!(
        result.diagnostics.len(), 1,
        "清单上只留最具体的那一条，不刷屏：{:?}",
        result.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert!(result.is_dedup_accounting_balanced(), "折叠不丢弃：留下 + 压下 == 折叠前");
    assert_eq!(
        result.diagnostics.len() + result.suppressed.len(),
        result.diagnostics_before_dedup
    );
}

#[test]
fn the_most_specific_rule_wins_not_the_first_one() {
    let rs = ruleset();
    let result = run(OVERLAP, &rs);

    // 03（取值域越界）是**派生**结论：数字看着越界是因为单位填错了。
    // 04 点名了真正的错处，护士长照着改一次就够。
    assert!(
        of_code(&result, "QC-PSY-04").len() == 1,
        "赢家应是单位错配 04：{:?}",
        result.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert_eq!(of_code(&result, "QC-PSY-03").len(), 0, "03 应被折进 04，不是反过来");
    assert_eq!(result.suppressed[0].code, "QC-PSY-03");
    assert_eq!(result.suppressed[0].suppressed_by, "QC-PSY-04");
}

#[test]
fn the_folded_entry_is_kept_not_discarded() {
    let rs = ruleset();
    let result = run(OVERLAP, &rs);
    let s = &result.suppressed[0];

    // 「为什么只报一条」是护士长最高频的追问；答不出来就等于工具在偷偷少报。
    assert!(!s.message.is_empty(), "输家自己的文案要留着");
    assert!(s.basis.contains("第 5.3 条"), "输家的院内依据要留着：{}", s.basis);
    assert!(s.winner_message.contains("第 5.4 条"), "赢家的依据要留着：{}", s.winner_message);
    assert!(!s.family_label.is_empty(), "要能说清是哪一族：{}", s.family_label);
    assert!(!s.reason.is_empty(), "院内理由要留着：{}", s.reason);

    let zh = s.as_zh();
    assert!(zh.contains("QC-PSY-03"), "折叠说明要含被压的码：{zh}");
    assert!(zh.contains("QC-PSY-04"), "折叠说明要含压它的码：{zh}");

    // 定位也留着：卡片高亮「此处另有 N 条」时要点得回去。
    assert!(!s.loc.doc_id.is_empty());
    assert_eq!(s.severity, Severity::Blocking, "压下去那条本来有多重要得记着");
}

#[test]
fn the_collapsed_entry_shares_the_exact_location_with_the_winner() {
    // 两码指向的是**同一处**文字。位置若有一格之差，护士长点开折叠项
    // 会看到高亮落在别的地方，「其实说的是同一栏」就说不通了。
    let rs = ruleset();
    let result = run(OVERLAP, &rs);
    let winner = of_code(&result, "QC-PSY-04")[0].clone();
    let loser = &result.suppressed[0];
    assert_eq!(loser.loc.doc_id, winner.loc.doc_id);
    assert_eq!(loser.loc.section_index, winner.loc.section_index);
    assert_eq!(loser.loc.char_range, winner.loc.char_range);
    // 区间能切回原文，且切出来的正是被判的那一栏。
    let source = "约束起始时间：2026-03-08 09:00。\n约束等级：一级保护。\n执行护士：<EXAMPLE_NURSE>。\n观察记录：约束期间生命体征平稳。\n约束时长：1000 小时。";
    let text = loser
        .loc
        .char_range
        .slice(source)
        .expect("区间必须能切得回本段原文");
    assert!(text.contains("1000"), "区间必须切得回被改的那一栏，实际：{text}");
}

#[test]
fn the_dedup_key_carries_the_position_triple_plus_the_family() {
    // 去重键四项全等才折叠。第四项保证**只折叠说同一件事的规则**：
    // 跨件的时间序诊断各自带自己的位置，本来就落不到同一个键上。
    let rs = ruleset();
    let result = run(OVERLAP, &rs);
    let w = of_code(&result, "QC-PSY-04")[0];
    let key = DedupKey::of(w, &rs);
    assert_eq!(key.doc_id, w.loc.doc_id);
    assert_eq!(key.section_index, w.loc.section_index);
    assert_eq!(key.start, w.loc.char_range.start);
    assert_eq!(key.end, w.loc.char_range.end);
    assert_eq!(key.family, "field-value");
    assert!(key.describe_zh().contains("field-value"), "键要能被人读出来");
}

// ——— 严重度分层：只上不下 ———

#[test]
fn folding_never_lowers_the_severity() {
    // 赢家若比输家轻，严重度被**抬**到最重的那一档。
    // 「为去重而降级」等于用界面整洁换漏报——这是本系统最不能出的错。
    let rs = ruleset();
    let result = run(OVERLAP, &rs);
    // 真人复核时偶有科室把「单位错配」降成提示的诉求（院里 QA 抽查看这个）。
    // 这时同一处还会命中阻断的「取值域越界」——折叠若把这条的阻断一起压掉，
    // 提交闸门就形同虚设。因此造一组「更具体但更轻」的同键对来钉住只上不下。
    let mut light = of_code(&result, "QC-PSY-04")[0].clone();
    light.severity = Severity::Notice;
    let mut heavy = light.clone();
    heavy.code = "QC-PSY-03".to_string();
    heavy.severity = Severity::Blocking;

    let outcome = dedup::dedup(vec![light, heavy], &rs);
    assert_eq!(outcome.kept.len(), 1);
    assert_eq!(outcome.kept[0].code, "QC-PSY-04", "更具体的一条仍是赢家");
    assert_eq!(
        outcome.kept[0].severity,
        Severity::Blocking,
        "严重度必须被抬到输家那一档，不能因为赢家「更具体」就让它变轻"
    );
    assert!(outcome.suppressed[0].escalated, "折叠说明要写明这一处的严重度被抬高了");
    assert!(outcome.suppressed[0].as_zh().contains("更重的一档"), "{}", outcome.suppressed[0].as_zh());
}

#[test]
fn severity_counts_have_exactly_one_source() {
    // CLI、FFI、UI 底栏都只许调 counts_by_severity。写两遍 filter 迟早
    // 出现「清单说有 3 条阻断、底栏显示 2 条」。
    let rs = ruleset();
    let result = run(OVERLAP, &rs);
    let c = result.counts_by_severity();
    assert_eq!(c.total(), result.diagnostics.len());
    assert_eq!(c.blocking, result.blocking_count());
    assert_eq!(c.blocking + c.notice, result.diagnostics.len());
    // 分区取用与计数必须对得上。
    assert_eq!(result.by_severity(Severity::Blocking).len(), c.blocking);
    assert_eq!(result.by_severity(Severity::Notice).len(), c.notice);
    // 折叠后剩下的每条都还是原来的严重度，没有被改成阻断或改成提示。
    for d in &result.diagnostics {
        let rule_sev = rs.rule(&d.code).expect("生效规则").severity;
        assert_eq!(d.severity, rule_sev, "{} 的严重度必须来自规则集", d.code);
    }
}

// ——— 误报守卫与边界 ———

#[test]
fn findings_at_different_places_are_never_folded_together() {
    // 键含位置：两处不同位置的同族命中必须各报各的。
    // 把它们折成一条，等于把「这一栏」和「那一栏」说成同一件事。
    let rs = ruleset();
    let result = run(SCATTERED, &rs);
    assert!(result.diagnostics_before_dedup >= 4, "该包有多处不重叠的命中");
    let mut seen = std::collections::BTreeSet::new();
    for d in &result.diagnostics {
        let k = DedupKey::of(d, &rs);
        assert!(
            seen.insert(k.clone()),
            "同一去重键上不得留下两条：{}",
            k.describe_zh()
        );
    }
    assert_eq!(
        result.diagnostics.len() + result.suppressed.len(),
        result.diagnostics_before_dedup,
        "总条数守恒"
    );
}

#[test]
fn folding_never_changes_the_verdict_on_a_clean_package() {
    // 误报守卫：合规包跑出 0 条，且没有「被折叠掉的东西」。
    // 如果折叠层在这里吞了条目，这条会红。
    let rs = ruleset();
    let result = run(COMPLIANT, &rs);
    assert_eq!(result.diagnostics.len(), 0, "合规包跑出 0 条：{:?}", result.diagnostics);
    assert!(result.suppressed.is_empty(), "合规包不该有被折叠的条目：{:?}", result.suppressed);
    assert_eq!(result.diagnostics_before_dedup, 0);
}

#[test]
fn codes_outside_every_family_are_never_folded() {
    // 族是质控办的业务决定。未归族的码自成一族——宁可同处多报一行，
    // 也不替质控办猜一个族。跨文书的 05~08 交付配置刻意不并进件内字段族。
    let rs = ruleset();
    for code in ["QC-PSY-05", "QC-PSY-06", "QC-PSY-07", "QC-PSY-08"] {
        assert!(rs.family_of(code).is_none(), "{code} 不应归族");
    }
    // 人工造两条同处同码的未归族诊断：键不同，永不折叠。
    let base = run(OVERLAP, &rs).diagnostics[0].clone();
    let mut a = base.clone();
    a.code = "QC-PSY-05".to_string();
    let mut b = base;
    b.code = "QC-PSY-06".to_string();
    assert_ne!(DedupKey::of(&a, &rs), DedupKey::of(&b, &rs), "未归族时键必须含码名");
    let outcome = dedup::dedup(vec![a, b], &rs);
    assert_eq!(outcome.kept.len(), 2, "未归族不折叠");
    assert!(outcome.suppressed.is_empty());
}

#[test]
fn the_same_package_always_folds_the_same_way() {
    // 审计导出要能 diff：同一份输入两次跑，折叠结果必须逐字段相同。
    let rs = ruleset();
    let first = run(OVERLAP, &rs);
    for _ in 0..3 {
        let again = run(OVERLAP, &rs);
        assert_eq!(
            again.diagnostics.iter().map(|d| (&d.code, &d.loc)).collect::<Vec<_>>(),
            first.diagnostics.iter().map(|d| (&d.code, &d.loc)).collect::<Vec<_>>()
        );
        assert_eq!(
            again.suppressed.iter().map(|s| (&s.code, &s.suppressed_by)).collect::<Vec<_>>(),
            first.suppressed.iter().map(|s| (&s.code, &s.suppressed_by)).collect::<Vec<_>>()
        );
        assert_eq!(again.diagnostics_before_dedup, first.diagnostics_before_dedup);
    }
    // 输入顺序变了也不能改变结果：赢家永远是最具体的那条。
    let reg = rs.registry();
    let items = parse_bundle(OVERLAP, Some(&reg)).unwrap().items;
    let mut swapped = items;
    swapped.reverse();
    let reversed = review(&swapped, &rs);
    assert_eq!(
        reversed.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>(),
        first.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert_eq!(reversed.suppressed.len(), first.suppressed.len());
}

#[test]
fn disabling_the_family_table_brings_both_findings_back() {
    // 对照：同一份夹具、同一份规则，去掉 code-families 段后两条都报。
    // 这条证明「只报一条」确实是折叠的结果，而不是 QC-PSY-04 恰好没命中。
    let yaml = RULESET.trim_end();
    let cut = yaml.find("\ncode-families:").expect("交付规则集应含 code-families 段");
    let without = load_effective(&yaml[..cut]).expect("去掉码族表后仍应可加载");
    assert!(without.code_families.is_empty());
    let with = run(OVERLAP, &without);
    assert!(
        of_code(&with, "QC-PSY-03").len() == 1 && of_code(&with, "QC-PSY-04").len() == 1,
        "无码族表时两码各报一条：{:?}",
        with.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
    assert!(with.suppressed.is_empty(), "无码族表时不折叠");

    // 配上码族表 → 收敛为 1 条 + 1 条压下。
    let folded = run(OVERLAP, &with_ruleset());
    assert_eq!(folded.suppressed.len(), 1);
    assert_eq!(of_code(&folded, "QC-PSY-04").len(), 1);
}

fn with_ruleset() -> EffectiveRules {
    ruleset()
}
