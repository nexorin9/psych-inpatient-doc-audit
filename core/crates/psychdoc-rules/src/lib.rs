//! `psychdoc-rules`：规则集加载与规则执行。
//!
//! ## 源产品主能力与保留
//!
//! 源产品每条规则带**稳定编号**、通过 `impl Violation::message()` 说清
//! 「为什么这是问题」、并用 `const FIX_AVAILABILITY` 决定能否自动修
//! （`github_ref/ruff/crates/ruff_linter/src/rules/airflow/rules/runtime_value_in_dag_or_task.rs:44`）。
//! 本项目把编号换成院内 `QC-PSY-nn`，把 `message()` 的依据换成**院内规范条款**
//! （`RuleMessage` 构造期强制非空，否则 `E_NO_RULE_BASIS`）。
//!
//! 模块分工：
//!
//! - [`ruleset`]：规则集加载、三级级联覆盖与 sha256 版本冻结（Task 5）；
//! - [`field_rules`]：**执行器 A · 件-字段类** `QC-PSY-01`~`04`（Task 6）；
//! - [`cross_rules`]：**执行器 B · 跨文书时间序与签署齐备性** `QC-PSY-05`~`08`（Task 7）；
//! - [`values`]：字段值形状解析（全角归一 / 数值 / 日期 / 单位），不判定只解析；
//! - [`expr`]：**声明式条件表达式求值器**，把规则条件从硬编码 `if` 提出来（Task 17）；
//! - [`waiver`]：**豁免谓词化**——病区 / 文书类型 / 规则码 / 时间窗四类条件，
//!   两级语义（整条不适用 / 降级为提示）与逐条溯源（Task 18）；
//! - [`chronology`]：**跨文书时间序 join 判定内核**——事件时间线、并列容差、
//!   精度降级与缺时刻降级（Task 19）。

pub mod chronology;
pub mod cross_rules;
pub mod dedup;
pub mod expr;
pub mod field_rules;
pub mod incremental;
pub mod regress;
pub mod ruleset;
pub mod values;
pub mod waiver;

pub use chronology::{
    judge as judge_chronology, precision_width, type_priority, Chronology, Direction, EventTimeline,
    Moment, SkipReason, TimelineEvent, ToleratedPair, Tolerance, ToleranceTable, TypePriority,
    ALLOWED_TOLERANCE_MINUTES,
};

pub use waiver::{
    dead_waivers, evaluate, pick_most_specific, subject_moment, ward_of, PredicateCombine,
    PredicateVerdict, TimeWindow, WaiverEffect, WaiverPredicate, WaiverSkip, WaiverSubject,
    WaivedRecord,
};

pub use expr::{
    decode_bytes, eval, EvalOutcome, Expr, ExprError, FieldPath, Operand, Operator, Resolved,
    SkipCause, E_DATE_PARSE, E_OPERAND_MISMATCH, E_PATH_INVALID, E_REGEX_INVALID,
    E_TYPE_MISMATCH,
};

pub use cross_rules::{
    group_by_encounter, CrossOutcome, EncounterGroup, SkippedCheck, QC_PSY_05,
    QC_PSY_06, QC_PSY_07, QC_PSY_08,
};
pub use field_rules::{
    registry, AmbiguousField, FieldPick, Rule, RuleCtx, QC_PSY_01, QC_PSY_02, QC_PSY_03, QC_PSY_04,
};
pub use incremental::{
    content_sha256, group_fingerprint, review_incremental, GroupCacheEntry, IncrementalRun,
    InvalidationRecord, ItemCacheEntry, ReviewCache, RunSummary,
};
pub use ruleset::{
    load_effective, released_rule_count, sha256_hex, CodeFamily, CodeFamilyDecl, DependencyDecl,
    EffectiveRule, EffectiveRules, LayerId, RuleDecl, RuleLayer, RuleSet, RuleSetError, WardIgnore,
    SHA256_SHORT_LEN,
};

pub use dedup::{dedup, DedupKey, DedupOutcome, SuppressedFinding};
pub use regress::{
    coverage, coverage_ratio, finding_line, fixture_path, fix_label, load_cases, run_all, run_case,
    Case, CaseError, CaseFile, CaseKind, CaseOutcome, CoverageRow, Expect, Given, LocAssert,
    RunReport, SkipAssert, ToleratedAssert, REGRESS_FORMAT_VERSION,
};

use psychdoc_model::{Diagnostic, DocItem};

// `Severity` 出现在 `EffectiveRule` 的公开字段上，调用方（CLI / FFI / UI）要读它，
// 因此从本 crate 一并转出，免得每个下游都得直接依赖 `psychdoc-model`。
pub use psychdoc_model::Severity;

/// 一次审查的完整结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReviewResult {
    /// 规则码命中的诊断（可按业务阻断/提示分层）。
    pub diagnostics: Vec<Diagnostic>,
    /// 解析期问题（与规则诊断分开记账，便于区分「包读不动」与「文书不合规」）。
    pub parse_issues: Vec<Diagnostic>,
    pub items_reviewed: usize,
    pub ruleset_version: String,
    /// 本次审查所用规则集源文件的 sha256。随结果落盘，供事后按当时版本回溯复核。
    pub ruleset_sha256: String,
    /// 被病区豁免而未上报的条目（Task 18 起填 waived 详情）。
    pub waived_count: usize,
    /// 被豁免放行的逐条溯源（Task 18 起）。
    ///
    /// 与 `waived_count` 是同一次豁免的两种视图：计数给底栏与统计，
    /// 明细给「这条为什么不报」的现场追问。
    pub waived: Vec<WaivedRecord>,
    /// 判不出来因而**没有**豁免的记录（Task 18 起）。
    ///
    /// 与 `skipped_checks` 同一个道理：判不出来不是「通过」，
    /// 必须能按住院别名回查。
    pub waiver_skipped: Vec<WaiverSkip>,
    /// 配了但本批一次都没命中的豁免（死豁免，Task 18 起）。
    pub dead_waivers: Vec<String>,
    /// 因「同名字段跨段歧义」而**未判定**的字段（Task 6 起）。
    ///
    /// 单独记账而不是塞进 diagnostics：未判定不等于不合规，不该阻断提交；
    /// 但也绝不能悄悄消失——否则「这条为什么没报」就查不到出处了。
    pub ambiguous_fields: Vec<AmbiguousField>,
    /// 跨文书时间序 / 签署齐备性因「对不齐」而跳过的检查（Task 7 起）。
    ///
    /// 与 `ambiguous_fields` 同一个道理：缺时刻、时刻相等、精度不足、
    /// 件绑定不唯一、对照件不在包里——这些都**不是**「通过」，
    /// 必须能按住院别名回查，否则「这条为什么没报」就成了查不到出处的问题。
    pub skipped_checks: Vec<SkippedCheck>,
    /// 落在并列容差内、因此**未判违规**的跨文书件对（Task 19）。
    ///
    /// 与 `skipped_checks` 一样不进阻断统计，但回答的是另一个问题：
    /// 「这两件只差 3 分钟，为什么没报时序倒挂」——答案是规则集里
    /// 有一条写明理由的 5 分钟并列容差。少这一条，护士长会以为工具漏了。
    pub tolerated_checks: Vec<ToleratedPair>,
    /// 每次住院的事件时间线（Task 19）：按业务时刻排好的多件事件。
    /// 归档核齐视图（Task 34）与「这一天的时间顺序对不对得上」的追问共用它。
    pub timelines: Vec<chronology::EventTimeline>,
    /// 被更具体规则压下、但**未丢弃**的条目（Task 23）。
    ///
    /// 清单默认只显示 [`Self::diagnostics`]，这一栏是「此处另有 N 条更概括的
    /// 命中」的展开源。护士长追问「为什么只报一条」时，这里答得出来。
    pub suppressed: Vec<SuppressedFinding>,
    /// 进入去重层之前的诊断条数（Task 23）。
    ///
    /// 只为一条恒等式服务：`diagnostics.len() + suppressed.len() == 该值`。
    /// 这条恒等式破了 = 去重层吞掉了诊断，而吞掉比刷屏严重得多。
    pub diagnostics_before_dedup: usize,
}

impl ReviewResult {
    pub fn blocking_count(&self) -> usize {
        self.diagnostics
            .iter()
            .filter(|d| d.severity == Severity::Blocking)
            .count()
    }

    /// 严重度分层的**唯一**计数口径。
    ///
    /// CLI、FFI、UI 底栏都只许调这一个方法——三处各写一遍 `filter` 迟早会
    /// 出现「清单说有 3 条阻断、底栏显示 2 条」。
    pub fn counts_by_severity(&self) -> SeverityCounts {
        let mut c = SeverityCounts::default();
        for d in &self.diagnostics {
            c.add(d.severity);
        }
        c
    }

    /// 按严重度分区取诊断（UI 的两栏列表直接消费）。
    pub fn by_severity(&self, s: Severity) -> Vec<&Diagnostic> {
        self.diagnostics.iter().filter(|d| d.severity == s).collect()
    }

    /// 折叠不丢弃的不变量：留下的 + 压下的 == 折叠前条数。
    ///
    /// 这条恒等式一旦破了，就说明去重层吞掉了诊断——那比刷屏严重得多。
    pub fn is_dedup_accounting_balanced(&self) -> bool {
        self.diagnostics.len() + self.suppressed.len() == self.diagnostics_before_dedup
    }

    /// 按住院别名回查豁免溯源（现场最高频的追问：「这条为什么不报」）。
    pub fn waived_for_encounter(&self, alias: &str) -> Vec<&WaivedRecord> {
        self.waived.iter().filter(|w| w.encounter_alias == alias).collect()
    }

    /// 核对不变量：计数与明细必须一致。
    ///
    /// 「计数说豁免了 3 条、明细只有 2 条」是审查工具最不能出的错——
    /// 前者让病区以为规则没跑，后者让质控办拿不出依据。
    pub fn is_waiver_accounting_balanced(&self) -> bool {
        self.waived_count == self.waived.len()
    }
}

/// 严重度分层计数。字段名即 UI 的两栏标题，不另造口径。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct SeverityCounts {
    pub blocking: usize,
    pub notice: usize,
}

impl SeverityCounts {
    pub fn add(&mut self, s: Severity) {
        match s {
            Severity::Blocking => self.blocking += 1,
            Severity::Notice => self.notice += 1,
        }
    }

    pub fn total(&self) -> usize {
        self.blocking + self.notice
    }
}

/// 对一批文书件跑规则。
///
/// 两个执行器按各自的粒度跑：
///
/// - **执行器 A**（件-字段类）逐件跑，不需要看到别的件；
/// - **执行器 B**（跨文书时间序 / 签署齐备性）必须把同一次住院的多件
///   join 到一起才判得出来，因此走 `encounter_alias` 分组整组跑。
///
/// 两者都走规则集：规则集里没声明的码不执行，声明了但 `applies_to`
/// 不含相应文书类型的不执行。
pub fn review(items: &[DocItem], ruleset: &EffectiveRules) -> ReviewResult {
    let mut result = ReviewResult {
        items_reviewed: items.len(),
        ruleset_version: ruleset.version.clone(),
        ruleset_sha256: ruleset.sha256.clone(),
        ..Default::default()
    };
    for item in items {
        // 件的文书类型不在字典内 → 解析层已记 unresolved，这里不重复造诊断。
        let Some(tpl) = ruleset.templates.get(&item.template_id) else {
            continue;
        };
        let (diagnostics, ambiguous) = field_rules::run_item(item, tpl, ruleset);
        result.diagnostics.extend(diagnostics);
        result.ambiguous_fields.extend(ambiguous);
    }
    let cross = cross_rules::run_cross_doc(items, ruleset);
    result.diagnostics.extend(cross.diagnostics);
    result.skipped_checks.extend(cross.skipped);
    result.tolerated_checks.extend(cross.tolerated);
    result.timelines = cross.timelines;

    // —— 豁免谓词化（Task 18）：在两个执行器都出完诊断之后统一应用。
    // 放在最后是刻意的：豁免判的是「这条诊断是否该放行」，
    // 前提是这条诊断已经真的被判出来了。
    waiver::apply(
        items,
        ruleset,
        &mut result.diagnostics,
        &mut result.waived,
        &mut result.waiver_skipped,
    );
    result.waived_count = result.waived.len();
    result.dead_waivers = waiver::dead_waivers(ruleset, &result.waived);

    // —— 去重择优（Task 23）：放在豁免**之后**。
    //
    // 顺序是刻意的：豁免判的是「这条该不该报」，去重判的是「报哪一条」。
    // 先豁免后去重，被整条豁免掉的诊断不会占着赢家位置把同族的具体条目挤掉。
    apply_dedup(&mut result, ruleset);
    result
}

/// 去重择优（Task 23）。抽成独立函数是为了让全量与增量**走同一条**：
/// 增量只该改变「算了多少遍」，不该改变「算出什么」。
pub fn apply_dedup(result: &mut ReviewResult, ruleset: &EffectiveRules) {
    result.diagnostics_before_dedup = result.diagnostics.len();
    let outcome = dedup::dedup(std::mem::take(&mut result.diagnostics), ruleset);
    result.diagnostics = outcome.kept;
    result.suppressed = outcome.suppressed;
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Span;

    const RULESET: &str = r#"
version: "0.1.0"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 约束事由
        section: 约束事由
        required: true
      - key: 患方意见
        section: 患方知情同意
        required: true
      - key: 约束起始时间
        section: 患方知情同意
        required: true
      - key: 患方签字
        section: 患方知情同意
        required: true
hospital:
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      basis: "《精神科住院护理常规》第 4.2 条"
"#;

    const FIXTURE: &str = include_str!("../../../../fixtures/ward-01-minimal.json");

    fn effective() -> EffectiveRules {
        load_effective(RULESET).unwrap()
    }

    fn parse_fixture() -> Vec<DocItem> {
        let reg = effective().registry();
        psychdoc_parse::parse_bundle(FIXTURE, Some(&reg)).unwrap().items
    }

    #[test]
    fn fixture_hits_exactly_one_qc_psy_01() {
        let result = review(&parse_fixture(), &effective());
        assert_eq!(result.diagnostics.len(), 1, "应恰好命中 1 条：{:?}", result.diagnostics);
        assert_eq!(result.diagnostics[0].code, QC_PSY_01);
        assert_eq!(result.diagnostics[0].severity, Severity::Blocking);
    }

    #[test]
    fn diagnostic_message_cites_the_basis() {
        let d = &review(&parse_fixture(), &effective()).diagnostics[0];
        assert!(d.message.text.contains("患方签字"));
        assert!(d.message.basis.contains("第 4.2 条"), "message 必须引用院内规范条款");
    }

    #[test]
    fn diagnostic_carries_section_localization() {
        let d = &review(&parse_fixture(), &effective()).diagnostics[0];
        // 「患方签字：」出现在第 1 段（患方知情同意）且值区间为零宽
        assert_eq!(d.loc.section_index, 1);
        assert_eq!(d.loc.doc_id, "WARD-A-001-CONSENT-01");
        assert!(d.loc.char_range.is_empty(), "空值 → 零宽区间落在冒号后");
    }

    #[test]
    fn required_missing_is_not_auto_fixable() {
        let d = &review(&parse_fixture(), &effective()).diagnostics[0];
        assert_eq!(d.fix_availability, psychdoc_model::FixAvailability::None);
        assert!(d.fix.is_none());
        assert!(d.is_consistent());
    }

    #[test]
    fn compliant_doc_produces_zero_diagnostics() {
        // 反例零命中（误报守卫）：把「患方签字」填上后必须一条不报
        let filled = FIXTURE.replace("患方签字：", "患方签字：<EXAMPLE_SIGNATURE>");
        let reg = effective().registry();
        let items = psychdoc_parse::parse_bundle(&filled, Some(&reg)).unwrap().items;
        let result = review(&items, &effective());
        assert_eq!(result.diagnostics.len(), 0, "合规件跑出 0 条：{:?}", result.diagnostics);
    }

    #[test]
    fn ruleset_without_basis_is_rejected_not_loaded_as_pass() {
        let bad = RULESET.replace("《精神科住院护理常规》第 4.2 条", "");
        let err = load_effective(&bad).unwrap_err();
        assert!(matches!(err, RuleSetError::MissingBasis { .. }));
        assert!(err.to_string().contains("E_RULESET_INVALID"));
        assert_eq!(released_rule_count(&bad), 0, "缺依据时零条规则被放行");
    }

    #[test]
    fn duplicate_rule_code_in_same_layer_is_rejected() {
        let dup = RULESET.replace(
            "hospital:\n  rules:",
            "hospital:\n  rules:\n    - code: \"QC-PSY-01\"\n      severity: notice\n      title: \"重复声明\"\n      basis: \"《精神科住院护理常规》第 9.9 条\"",
        );
        let err = load_effective(&dup).unwrap_err();
        assert!(
            matches!(err, RuleSetError::DuplicateCode { ref layer, .. } if *layer == LayerId::Hospital),
            "{err}"
        );
    }

    #[test]
    fn field_absent_entirely_locates_at_expected_section_start() {
        // 键根本没出现 → 定位到模板声明的段落起点，而不是编造一个值区间
        let stripped = FIXTURE.replace("患方签字：", "");
        let reg = effective().registry();
        let items = psychdoc_parse::parse_bundle(&stripped, Some(&reg)).unwrap().items;
        let result = review(&items, &effective());
        assert_eq!(result.diagnostics.len(), 1);
        let d = &result.diagnostics[0];
        assert!(d.message.text.contains("未填写"));
        assert_eq!(d.loc.section_index, 1, "应落在模板声明的「患方知情同意」段");
        assert_eq!(d.loc.char_range, Span::unchecked(0, 0));
    }

    #[test]
    fn review_result_carries_ruleset_sha256() {
        let result = review(&parse_fixture(), &effective());
        assert_eq!(result.ruleset_sha256.len(), 64);
        assert_eq!(result.ruleset_sha256, effective().sha256);
    }
}
