//! 规则执行器 B：**跨文书时间序**（`QC-PSY-05` ~ `07`）与**签署齐备性**（`QC-PSY-08`）。
//!
//! ## 为什么要有执行器 B
//!
//! 执行器 A（[`crate::field_rules`]）看的是**一件之内**的字段，
//! 判「这一件自己填得对不对」。但精神科约束文书真正会出事的地方在**件与件之间**：
//!
//! - 患方签了字，约束记录里写的起始时刻却**早于**签署时刻（先约束、后知情）；
//! - 约束已经解除，解除记录的时刻却**早于**约束记录的起始时刻；
//! - 约束做完了，入院评估的时刻却**晚于**首次约束（没评估就约束）。
//!
//! 这三类都不可能由任何单件规则发现——它们需要把**同一次住院**的多件
//! 按 `encounter_alias` join 到一起，把各自的 `timestamps[]` 对齐成一条事件时间线。
//!
//! ## 三条不可让步的行为
//!
//! 1. **对不齐就不下结论**。缺时刻、时刻完全相等、精度不足以分辨先后、
//!    或参与判定的任一件处于 `ambiguous` 绑定时，**整条规则跳过**并把
//!    [`SkippedCheck`] 记进结果——跳过原因可按住院别名回查。
//!    绝不拿当前时间、零值或「大概」去凑一个时差。
//! 2. **同一处不刷两行**。`QC-PSY-08` 只管「**签了名但没有签署时刻**」；
//!    签署位**根本没填**归 `QC-PSY-01`（逐字段、给精确值区间）。
//!    两条规则的职责边界写在 [`is_signature_key`] 上，改一处不会波及另一处。
//! 3. **不生成签名动作**。`QC-PSY-08` 只产出「签署齐备性缺件」诊断与院内规范条款依据，
//!    全部 `FixAvailability::None`——签名仍在 EMR 侧由医务人员本人完成。
//!
//! ## 与执行器 A 的分工
//!
//! | 码 | 视角 | 判什么 | 可否自动修 |
//! |---|---|---|---|
//! | `QC-PSY-05` | 跨件 | 解除时刻早于约束起始时刻（事件与记录时序倒挂） | 否 |
//! | `QC-PSY-06` | 跨件 | 评估时刻晚于首次约束起始时刻（评估与医嘱时序矛盾） | 否 |
//! | `QC-PSY-07` | 跨件 | 患方签署时刻晚于约束起始时刻（约束与签署时序错误） | 否 |
//! | `QC-PSY-08` | 单件 | 签署位已填但该件无可核对的签署时刻（签署齐备性缺件） | 否 |
//!
//! `QC-PSY-05` ~ `07` 每命中一次产出**两条**诊断：违反的两个件各自一条，
//! 各自带自己的 `(doc_id, section_index, char_range)` 与依据条款。
//! 护士长在任一件里都应能看到「这件事和哪一件对不上」——只定位一处会让另一件的核对无入口。

use std::collections::BTreeMap;

use psychdoc_model::{
    Diagnostic, DocItem, DocLoc, FieldPresence, FieldSpec, RuleMessage, Span, TimestampKind,
};
use serde::{Deserialize, Serialize};

use crate::chronology::{self, Chronology, Direction, Moment, ToleratedPair};
use crate::field_rules::{pick_field, FieldPick};
use crate::ruleset::{EffectiveRule, EffectiveRules};

/// `QC-PSY-05`：事件与记录时序倒挂（解除早于开始）。
pub const QC_PSY_05: &str = "QC-PSY-05";
/// `QC-PSY-06`：评估与医嘱时序矛盾（评估晚于首次约束）。
pub const QC_PSY_06: &str = "QC-PSY-06";
/// `QC-PSY-07`：约束与签署时序错误（签署晚于约束开始）。
pub const QC_PSY_07: &str = "QC-PSY-07";
/// `QC-PSY-08`：签署齐备性（已签但无签署时刻）。
pub const QC_PSY_08: &str = "QC-PSY-08";

/// 时间序判定为什么没结论。**跳过必须带原因**——「这条为什么没报」
/// 是院内最常追问、又最难自己查出来的问题。
///
/// 判定内核已搬到 [`crate::chronology`]（Task 19：并列容差 / 精度降级 /
/// 跨零点），这里原样转出，历史调用点（CLI / FFI / UI）不必改。
pub use crate::chronology::SkipReason;

/// 一条因无法判定而跳过的检查。可按住院别名 + 规则码回查。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkippedCheck {
    pub code: String,
    pub encounter_alias: String,
    /// 触发这次判定的件（对照件缺失时即唯一那件）。
    pub primary_doc: String,
    pub reason: SkipReason,
    /// 人可读的补充说明。
    pub detail: String,
}

impl SkippedCheck {
    fn new(
        code: &str,
        alias: &str,
        primary_doc: &str,
        reason: SkipReason,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code: code.to_string(),
            encounter_alias: alias.to_string(),
            primary_doc: primary_doc.to_string(),
            reason,
            detail: detail.into(),
        }
    }

    /// 清单 / UI 直接可读的一行。
    pub fn as_zh(&self) -> String {
        format!(
            "{} · {} · {}：{}",
            self.code, self.encounter_alias, self.reason.as_zh(), self.detail
        )
    }
}

/// 一次跨文书判定的产出。
#[derive(Debug, Clone, Default, Serialize)]
pub struct CrossOutcome {
    pub diagnostics: Vec<Diagnostic>,
    pub skipped: Vec<SkippedCheck>,
    /// 落在并列容差内、因此**未判违规**的件对（Task 19）。
    ///
    /// 与 `skipped` 的区别：跳过是「判不出来」，容差内是「判出来了，
    /// 结论是不算违规」。两者都回答「这一条为什么没报」，但答案不同。
    pub tolerated: Vec<ToleratedPair>,
    /// 每次住院的事件时间线（Task 19）。给归档核齐视图与 UI 底栏共用。
    pub timelines: Vec<chronology::EventTimeline>,
}

// ---------------------------------------------------------------------------
// 跨件 join
// ---------------------------------------------------------------------------

/// 一次住院下的一组件。跨件规则的 join 键**只有** `encounter_alias`——
///// 权威键来自 `Composition.identifier`，绝不按文件名或标题配对。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncounterGroup<'a> {
    pub alias: &'a str,
    pub items: Vec<&'a DocItem>,
}

/// 按住院别名分组。用 `BTreeMap` 保证**分组顺序稳定**，两次跑批结果一致。
pub fn group_by_encounter(items: &[DocItem]) -> Vec<EncounterGroup<'_>> {
    let mut map: BTreeMap<&str, Vec<&DocItem>> = BTreeMap::new();
    for item in items {
        map.entry(item.encounter_alias.as_str()).or_default().push(item);
    }
    map.into_iter()
        .map(|(alias, items)| EncounterGroup { alias, items })
        .collect()
}

/// 在一组件里找某文书类型的**唯一**一件。
///
/// 三种结果，缺一不可：
///
/// - `One` — 唯一且绑定 confident，可以判；
/// - `Ambiguous` — 同类型多件（跨系统同名）或存在绑定不唯一的件：**不判**，
///   取任一件都可能给出一条建立在错配对上的结论；
/// - `None` — 该类型不在包里。
#[derive(Debug, Clone, Copy)]
enum Lookup<'a> {
    One(&'a DocItem),
    Ambiguous,
    None,
}

fn lookup<'a>(group: &EncounterGroup<'a>, template_id: &str) -> Lookup<'a> {
    let hit: Vec<&DocItem> = group
        .items
        .iter()
        .copied()
        .filter(|d| d.template_id == template_id)
        .collect();
    if hit.iter().any(|d| !d.binding.is_confident()) {
        return Lookup::Ambiguous;
    }
    match hit.len() {
        1 => Lookup::One(hit[0]),
        0 => Lookup::None,
        _ => Lookup::Ambiguous,
    }
}

// ---------------------------------------------------------------------------
// 时刻取用与可比性
// ---------------------------------------------------------------------------

/// 取某字段值区间内的段落内业务时刻（带精度）。
///
/// 刻意**不用**「这件最早的 `BusinessMoment`」：约束记录里除了「约束起始时间」
/// 还可能写着巡视时刻、解除时刻，取最早的那一个会让判定落在不相干的字上。
/// 定位与取值必须来自**同一个字段**，护士长点开高亮才看得到被判的那个值。
fn field_moment(item: &DocItem, spec: &FieldSpec) -> Option<Moment> {
    let field = match pick_field(item, spec) {
        FieldPick::Found(f) => f,
        // 字段歧义 / 缺失 → 取不出时刻，交由上层记 skipped_reason，不猜。
        _ => return None,
    };
    let span = field.source_span?;
    item.timestamps
        .iter()
        .filter(|t| t.kind == TimestampKind::BusinessMoment)
        .find(|t| t.char_range.is_some_and(|r| span.start <= r.start && r.end <= span.end))
        .and_then(|t| t.moment.map(|at| (at, t.precision)))
        .map(|(at, precision)| Moment::new(at, precision))
}

/// 违反方向 = 规则要求 `b` 相对 `a` 的先后。
///
/// 与 [`chronology::Direction`] 语义一致，这里只做取用时的可读性转换。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Violation {
    /// `b` 应不早于 `a`；`b < a` 即倒挂。
    Earlier,
    /// `b` 应不晚于 `a`；`b > a` 即倒挂。
    Later,
}

impl From<Violation> for Direction {
    fn from(v: Violation) -> Self {
        match v {
            Violation::Earlier => Direction::NotEarlier,
            Violation::Later => Direction::NotLater,
        }
    }
}

/// 时刻比对的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Cmp {
    /// 可判：第二个时刻相对第一个的秒差（`b - a`）。
    Gap(i64),
    /// 判不了，原因必须带出去。
    Skip(SkipReason),
}

/// 逐件跑执行器 B。
///
/// 走规则集：规则集里没声明的码**一条都不执行**；声明了但 `applies_to`
/// 不含参与文书类型的同样不执行——与执行器 A 遵守同一条纪律。
pub fn run_cross_doc(items: &[DocItem], ruleset: &EffectiveRules) -> CrossOutcome {
    let mut out = CrossOutcome::default();
    let priority = chronology::type_priority(ruleset);
    for group in group_by_encounter(items) {
        // 每次住院先重建一条事件时间线（Task 19）：跨件 join 的公共底座，
        // 同时供归档核齐视图与 UI 展示「这一天按业务时刻排下来是什么样」。
        out.timelines.push(chronology::EventTimeline::build(group.alias, &group.items, &priority));
        rule_qc_psy_05(&group, ruleset, &mut out);
        rule_qc_psy_06(&group, ruleset, &mut out);
        rule_qc_psy_07(&group, ruleset, &mut out);
        rule_qc_psy_08(&group, ruleset, &mut out);
    }
    out
}

/// 一条跨件规则的运行期上下文。
struct PairCtx<'a> {
    group: &'a EncounterGroup<'a>,
    /// 规则集里的生效声明（严重度与依据都从这里取）。
    effective: &'a EffectiveRule,
    ruleset: &'a EffectiveRules,
}

/// 解析出一条跨件规则要用的两个件，解析不出就记 `skipped` 并返回 `None`。
fn resolve_pair<'a>(
    ctx: &PairCtx<'a>,
    code: &str,
    a_tpl: &str,
    b_tpl: &str,
    out: &mut CrossOutcome,
) -> Option<(&'a DocItem, &'a DocItem)> {
    if !ctx.effective.applies_to_doc_type(a_tpl) || !ctx.effective.applies_to_doc_type(b_tpl) {
        return None;
    }
    let a = match lookup(ctx.group, a_tpl) {
        Lookup::One(d) => d,
        Lookup::Ambiguous => {
            out.skipped.push(SkippedCheck::new(
                code,
                ctx.group.alias,
                a_tpl,
                SkipReason::AmbiguousBinding,
                format!("{a_tpl} 件绑定不唯一（跨系统同名或未找到模板），不据此下结论"),
            ));
            return None;
        }
        Lookup::None => {
            out.skipped.push(SkippedCheck::new(
                code,
                ctx.group.alias,
                a_tpl,
                SkipReason::NoCounterpart,
                format!("本次文书包内没有 {a_tpl} 件（缺件由归档核齐视图负责）"),
            ));
            return None;
        }
    };
    let b = match lookup(ctx.group, b_tpl) {
        Lookup::One(d) => d,
        Lookup::Ambiguous => {
            out.skipped.push(SkippedCheck::new(
                code,
                ctx.group.alias,
                b_tpl,
                SkipReason::AmbiguousBinding,
                format!("{b_tpl} 件绑定不唯一，不据此下结论"),
            ));
            return None;
        }
        Lookup::None => {
            out.skipped.push(SkippedCheck::new(
                code,
                ctx.group.alias,
                b_tpl,
                SkipReason::NoCounterpart,
                format!("本次文书包内没有 {b_tpl} 件"),
            ));
            return None;
        }
    };
    Some((a, b))
}

/// 参与一次比对的一侧：**件 + 文书里的中文名 + 那个时刻**。
///
/// 三者必须捆在一起：定位落在件上、文案念的是文书里的叫法、比较用的是
/// 时刻本身，拆开传迟早会出现「文案说 A、定位指向 B」这种对不上的诊断。
struct SideMoment<'a> {
    doc: &'a DocItem,
    label: &'static str,
    moment: Moment,
}

impl<'a> SideMoment<'a> {
    fn from_side(side: &Side<'a>, moment: Moment) -> Self {
        Self { doc: side.doc, label: side.label, moment }
    }
}

/// 比对两件上的两个时刻，把结论落到 `out` 里。
///
/// 判定内核在 [`chronology::judge`]（Task 19）：并列容差、精度降级、
/// 跨零点真实时间差都在那里。本函数只负责**把结论翻译成院内的话**：
/// 违规出诊断，容差内记账，判不出来记跳过原因——三者都不静默。
fn compare_moments(
    ctx: &PairCtx<'_>,
    code: &str,
    a: &SideMoment<'_>,
    b: &SideMoment<'_>,
    direction: Direction,
    out: &mut CrossOutcome,
) -> Cmp {
    let tolerance = ctx.ruleset.tie_tolerance(&a.doc.template_id, &b.doc.template_id);
    let (a_label, b_label) = (a.label, b.label);
    match chronology::judge(&a.moment, &b.moment, direction, &tolerance) {
        Chronology::Clear { gap_seconds, .. } => Cmp::Gap(gap_seconds),
        Chronology::Inverted { gap_seconds } => Cmp::Gap(gap_seconds),
        Chronology::Tolerated { gap_seconds, tolerance_minutes, reason } => {
            out.tolerated.push(ToleratedPair {
                code: code.to_string(),
                encounter_alias: ctx.group.alias.to_string(),
                primary_doc: b.doc.doc_id.clone(),
                counterpart_doc: a.doc.doc_id.clone(),
                gap_seconds,
                tolerance_minutes,
                reason: reason.clone(),
                source: tolerance.source,
                detail: format!(
                    "{a_label}（{}）与{b_label}（{}）相差 {} 分钟，落在该件对的并列容差内，不判倒挂",
                    a.moment.as_zh(),
                    b.moment.as_zh(),
                    gap_seconds.abs() / 60
                ),
            });
            Cmp::Skip(SkipReason::InsufficientPrecision)
        }
        Chronology::Undecidable { reason, detail } => {
            out.skipped.push(SkippedCheck::new(
                code,
                ctx.group.alias,
                // 跳过记录挂在**被比较侧**（`b`）上：现场复核时是拿着「这一件」
                // 去问「它跟哪一件对不上、为什么没报」。
                b.doc.doc_id.as_str(),
                reason,
                format!("{a_label}（{}）与 {b_label}（{}）：{detail}", a.moment.as_zh(), b.moment.as_zh()),
            ));
            Cmp::Skip(reason)
        }
    }
}

/// 参与一次跨件比对的一侧。
struct Side<'a> {
    doc: &'a DocItem,
    /// 这个时刻在文书里叫什么（「写明的约束起始时间」），进文案用。
    label: &'static str,
    spec: &'a FieldSpec,
}

/// 一条跨件时间序规则的公共部分：取时刻 → 比对 → 违规时给**两件各一条**诊断。
fn check_pair(
    ctx: &PairCtx<'_>,
    code: &str,
    a: &Side<'_>,
    b: &Side<'_>,
    violation: Violation,
    what: &str,
    out: &mut CrossOutcome,
) {
    let Side { doc: a_doc, label: a_label, spec: a_spec } = *a;
    let Side { doc: b_doc, label: b_label, spec: b_spec } = *b;
    let (Some(a_ts), Some(b_ts)) = (field_moment(a_doc, a_spec), field_moment(b_doc, b_spec)) else {
        let (missing_doc, missing_label, other_label) = if field_moment(a_doc, a_spec).is_none() {
            (a_doc, a_label, b_label)
        } else {
            (b_doc, b_label, a_label)
        };
        out.skipped.push(SkippedCheck::new(
            code,
            ctx.group.alias,
            missing_doc.doc_id.as_str(),
            SkipReason::MissingTimestamp,
            format!("{missing_label} 抽不出时刻（不拿当前时间兜底），本条不判；{other_label} 不受影响"),
        ));
        return;
    };

    let gap = match compare_moments(
        ctx,
        code,
        &SideMoment::from_side(a, a_ts),
        &SideMoment::from_side(b, b_ts),
        violation.into(),
        out,
    ) {
        Cmp::Gap(g) => g,
        Cmp::Skip(..) => return,
    };
    let violated = match violation {
        Violation::Earlier => gap < 0,
        Violation::Later => gap > 0,
    };
    if !violated {
        return;
    }

    // 违反的两件**各自**一条诊断：护士长在任一件里都应看到「和哪一件对不上」。
    let (a_m, b_m) = (a_ts.as_zh(), b_ts.as_zh());
    let a_loc = locate_field(a_doc, a_spec);
    let b_loc = locate_field(b_doc, b_spec);
    out.diagnostics.push(diag(
        ctx.effective,
        a_loc,
        format!(
            "{}的{}（{}）早于{}的{}（{}），{}时序倒挂",
            display_of(ctx.ruleset, a_doc),
            a_label,
            a_m,
            display_of(ctx.ruleset, b_doc),
            b_label,
            b_m,
            what
        ),
    ));
    out.diagnostics.push(diag(
        ctx.effective,
        b_loc,
        format!(
            "{}的{}（{}）晚于{}的{}（{}），{}时序倒挂",
            display_of(ctx.ruleset, b_doc),
            b_label,
            b_m,
            display_of(ctx.ruleset, a_doc),
            a_label,
            a_m,
            what
        ),
    ));
}

/// 字段值的定位区间；字段没出现时落在模板声明的段落起点（零宽）。
fn locate_field(doc: &DocItem, spec: &FieldSpec) -> DocLoc {
    match pick_field(doc, spec) {
        FieldPick::Found(f) => DocLoc::new(
            &doc.doc_id,
            f.section_index,
            f.source_span.unwrap_or_else(|| Span::unchecked(0, 0)),
        ),
        _ => DocLoc::new(&doc.doc_id, 0, Span::unchecked(0, 0)),
    }
}

fn diag(effective: &EffectiveRule, loc: DocLoc, text: String) -> Diagnostic {
    let msg = RuleMessage::new(text, &effective.basis).expect("加载期已保证依据非空");
    Diagnostic::new(effective.code.clone(), loc, msg)
        .with_severity(effective.severity)
        .without_fix()
}

fn display_of(ruleset: &EffectiveRules, doc: &DocItem) -> String {
    ruleset
        .doc_types
        .get(&doc.template_id)
        .cloned()
        .or_else(|| ruleset.templates.get(&doc.template_id).map(|t| t.display_or(&doc.template_id).to_string()))
        .unwrap_or_else(|| doc.template_id.clone())
}

fn field_spec<'a>(ruleset: &'a EffectiveRules, template_id: &str, key: &str) -> Option<&'a FieldSpec> {
    ruleset.templates.get(template_id)?.fields.iter().find(|f| f.key == key)
}

// ---------------------------------------------------------------------------
// QC-PSY-05 / 06 / 07
// ---------------------------------------------------------------------------

/// `QC-PSY-05`：解除记录的时刻**早于**约束记录的起始时刻。
///
/// 这是本系统里最硬的一条红线：解除时间写在约束开始之前，说明两份文书
/// 要么录错了、要么是补记的，无论哪种情况这份病历都不能直接归档。
fn rule_qc_psy_05(group: &EncounterGroup<'_>, ruleset: &EffectiveRules, out: &mut CrossOutcome) {
    let Some(effective) = ruleset.rule(QC_PSY_05) else {
        return;
    };
    let ctx = PairCtx { group, effective, ruleset };
    let Some((record, release)) = resolve_pair(
        &ctx,
        QC_PSY_05,
        "restraint_record",
        "restraint_release",
        out,
    ) else {
        return;
    };
    let (Some(start_spec), Some(release_spec)) = (
        field_spec(ruleset, "restraint_record", "约束起始时间"),
        field_spec(ruleset, "restraint_release", "解除时间"),
    ) else {
        return;
    };
    check_pair(
        &ctx,
        QC_PSY_05,
        &Side { doc: record, label: "约束起始时间", spec: start_spec },
        &Side { doc: release, label: "解除时间", spec: release_spec },
        Violation::Earlier,
        "解除",
        out,
    );
}

/// `QC-PSY-06`：入院评估的时刻**晚于**本次住院首次约束的开始时刻。
///
/// 以**最早**的约束起始时刻为基准：首次约束之前必须已有评估，
/// 之后的每次约束不再重复要求。
fn rule_qc_psy_06(group: &EncounterGroup<'_>, ruleset: &EffectiveRules, out: &mut CrossOutcome) {
    let Some(effective) = ruleset.rule(QC_PSY_06) else {
        return;
    };
    let ctx = PairCtx { group, effective, ruleset };
    let Some((record, assess)) = resolve_pair(
        &ctx,
        QC_PSY_06,
        "restraint_record",
        "assessment_report",
        out,
    ) else {
        return;
    };
    let (Some(assess_spec), Some(start_spec)) = (
        field_spec(ruleset, "assessment_report", "评估时间"),
        field_spec(ruleset, "restraint_record", "约束起始时间"),
    ) else {
        return;
    };
    check_pair(
        &ctx,
        QC_PSY_06,
        &Side { doc: record, label: "约束起始时间", spec: start_spec },
        &Side { doc: assess, label: "评估时间", spec: assess_spec },
        Violation::Later,
        "评估",
        out,
    );
}

/// `QC-PSY-07`：患方签署时刻**晚于**约束开始的时刻。
///
/// 签署时刻取 `Composition.authoredOn`（文书系统记录的书写时刻）；
/// 取不出即整条跳过并记 `missing_timestamp`——不拿「约束起始时间」
/// 顶替，那正是被判的那个值，用它当基准等于自己判自己。
fn rule_qc_psy_07(group: &EncounterGroup<'_>, ruleset: &EffectiveRules, out: &mut CrossOutcome) {
    let Some(effective) = ruleset.rule(QC_PSY_07) else {
        return;
    };
    let ctx = PairCtx { group, effective, ruleset };
    let Some((consent, record)) = resolve_pair(
        &ctx,
        QC_PSY_07,
        "restraint_consent",
        "restraint_record",
        out,
    ) else {
        return;
    };
    let Some(start_spec) = field_spec(ruleset, "restraint_record", "约束起始时间") else {
        return;
    };
    let Some(start_ts) = field_moment(record, start_spec) else {
        out.skipped.push(SkippedCheck::new(
            QC_PSY_07,
            group.alias,
            record.doc_id.as_str(),
            SkipReason::MissingTimestamp,
            "约束记录抽不出起始时刻，本条不判",
        ));
        return;
    };
    let Some(signed_ts) = chronology::document_moment(consent, TimestampKind::AuthoredOn) else {
        out.skipped.push(SkippedCheck::new(
            QC_PSY_07,
            group.alias,
            consent.doc_id.as_str(),
            SkipReason::MissingTimestamp,
            "知情同意件没有书写时刻（authoredOn），无法核对签署先后",
        ));
        return;
    };
    // 基准用约束起始时间（= a），被比较的是患方签署时刻（= b）。
    // 签署时刻是**文书级**时间戳，没有字段区间，因此定位交给 `authored_on_loc`。
    let gap = match compare_moments(
        &ctx,
        QC_PSY_07,
        &SideMoment { doc: record, label: "约束起始时间", moment: start_ts },
        &SideMoment { doc: consent, label: "患方签署时刻", moment: signed_ts },
        // 患方签署不得晚于约束开始。
        Direction::NotLater,
        out,
    ) {
        Cmp::Gap(g) => g,
        Cmp::Skip(..) => return,
    };
    if gap <= 0 {
        return;
    }
    let (start_m, signed_m) = (start_ts.as_zh(), signed_ts.as_zh());
    out.diagnostics.push(diag(
        effective,
        locate_field(record, start_spec),
        format!(
            "约束记录写明的约束起始时间（{start_m}）早于患方知情同意件的签署时刻（{signed_m}）：先约束、后知情，顺序不成立"
        ),
    ));
    out.diagnostics.push(diag(
        effective,
        authored_on_loc(consent, TimestampKind::AuthoredOn),
        format!(
            "患方知情同意件的签署时刻（{signed_m}）晚于约束记录写明的约束起始时间（{start_m}）：先约束、后知情，顺序不成立"
        ),
    ));
}

/// `authoredOn` 是文书级时刻，没有段内字符区间。取**最早的有时刻时间戳**所在段，
/// 让定位至少落在该件真实存在内容的地方，而不是凭空指向第 0 段第 0 字。
fn authored_on_loc(doc: &DocItem, kind: TimestampKind) -> DocLoc {
    DocLoc::new(
        &doc.doc_id,
        chronology::document_moment_section(doc, kind),
        Span::unchecked(0, 0),
    )
}

// ---------------------------------------------------------------------------
// QC-PSY-08 签署齐备性
// ---------------------------------------------------------------------------

/// 判定某个键是不是**签署位**。执行器 A / B 的职责边界就写在这一行上：
/// 签署位**没填**归 `QC-PSY-01`（逐字段、给值区间），签署位**填了但没有
/// 签署时刻**归 `QC-PSY-08`（齐备性）。两处都不生成签名动作。
fn is_signature_key(key: &str) -> bool {
    key.contains("签字") || key.contains("签名")
}

/// `QC-PSY-08`：签署位已填，但该件**没有可核对的签署时刻**。
///
/// 「签了名但没记时间」在归档核齐时是查不出来的：签名本身看上去齐备，
/// 只有把「签署时刻」与「约束起始时刻」摆在一起比对，才看得出这件事
/// 发生在约束之前还是之后。**只产出诊断与依据，绝不生成签名动作。**
fn rule_qc_psy_08(group: &EncounterGroup<'_>, ruleset: &EffectiveRules, out: &mut CrossOutcome) {
    let Some(effective) = ruleset.rule(QC_PSY_08) else {
        return;
    };
    for &item in &group.items {
        if !effective.applies_to_doc_type(&item.template_id) {
            continue;
        }
        if !item.binding.is_confident() {
            out.skipped.push(SkippedCheck::new(
                QC_PSY_08,
                group.alias,
                item.doc_id.as_str(),
                SkipReason::AmbiguousBinding,
                "该件绑定不唯一，签署齐备性不据此下结论",
            ));
            continue;
        }
        let Some(tpl) = ruleset.templates.get(&item.template_id) else {
            continue;
        };
        let sign_specs: Vec<&FieldSpec> = tpl
            .fields
            .iter()
            .filter(|f| f.required && is_signature_key(&f.key))
            .collect();
        if sign_specs.is_empty() {
            continue;
        }
        let has_signed_moment = item
            .timestamps
            .iter()
            .any(|t| t.kind == TimestampKind::AuthoredOn && t.moment.is_some());
        if has_signed_moment {
            continue;
        }
        // 只点名「**签了名却没时刻**」的签署位；压根没签的归 QC-PSY-01。
        let signed_without_moment: Vec<&&FieldSpec> = sign_specs
            .iter()
            .filter(|spec| {
                matches!(pick_field(item, spec), FieldPick::Found(f) if f.presence() == FieldPresence::Present)
            })
            .collect();
        if signed_without_moment.is_empty() {
            continue;
        }
        let roles: Vec<&str> = signed_without_moment.iter().map(|s| s.key.as_str()).collect();
        let anchor = signed_without_moment[0];
        out.diagnostics.push(diag(
            effective,
            locate_field(item, anchor),
            format!(
                "{}件的「{}」已填写，但该件没有签署时刻（authoredOn 缺失），事后无法核对签署与约束的先后",
                display_of(ruleset, item),
                roles.join("」「")
            ),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::{Binding, Precision, Section, Timestamp};

    /// 解析 `2026-03-02 23:40` 这样的时刻字面量；**不带日期的时刻不解析**
    /// （无法定序，交人工），与解析层口径一致。
    fn parse_moment(raw: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
        let (date, hm) = raw.split_once(' ')?;
        let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M").ok()?;
        let offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        chrono::TimeZone::from_local_datetime(&offset, &naive).single().or_else(|| {
            let _ = date;
            let _ = hm;
            None
        })
    }

    /// 造一件带「键：值」段落的文书。
    ///
    /// 字段值区间与段内业务时刻区间**都按真实字符下标算**，与解析层同形——
    /// 否则「定位切不回原文」「时刻落在不相干的字上」这类问题在本文件里测不出来。
    /// `pairs` 里值为空串的字段表示「键在、值没填」。
    fn doc(
        id: &str,
        tpl: &str,
        alias: &str,
        authored: Option<&str>,
        pairs: &[(&str, &str)],
    ) -> DocItem {
        let mut text = String::new();
        let mut fields: Vec<psychdoc_model::Field> = Vec::new();
        let mut timestamps: Vec<Timestamp> = Vec::new();
        for (k, v) in pairs {
            let line_start = u32::try_from(text.chars().count()).unwrap();
            text.push_str(&format!("{k}：{v}\n"));
            let value_start = line_start + u32::try_from(k.chars().count()).unwrap() + 1;
            let value_len = u32::try_from(v.chars().count()).unwrap();
            let value_span = Span::unchecked(value_start, value_start + value_len);
            fields.push(psychdoc_model::Field {
                key: (*k).to_string(),
                value: Some((*v).to_string()),
                required: true,
                source_span: Some(value_span),
                section_index: 0,
            });
            // 值里带日期时刻 → 该值区间内有一条业务时刻（与解析层一致）。
            if let Some(m) = parse_moment(v) {
                timestamps.push(Timestamp::at(
                    TimestampKind::BusinessMoment,
                    m,
                    Precision::Minute,
                    0,
                    Some(value_span),
                ));
            }
        }
        // 文书级 date：取段内最早的业务时刻所在日 00:00，取不到就用固定基准日。
        let date = timestamps
            .iter()
            .filter_map(|t| t.moment)
            .min()
            .and_then(|m| m.date_naive().and_hms_opt(0, 0, 0))
            .unwrap_or_else(|| chrono::NaiveDate::from_ymd_opt(2026, 3, 1).unwrap().into());
        let offset = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        timestamps.insert(
            0,
            Timestamp::at(
                TimestampKind::Date,
                chrono::TimeZone::from_local_datetime(&offset, &date)
                    .single()
                    .unwrap(),
                Precision::Minute,
                0,
                None,
            ),
        );
        if let Some(a) = authored {
            let m = parse_moment(a).expect("测试里的 authoredOn 必须带日期");
            timestamps.push(Timestamp::at(TimestampKind::AuthoredOn, m, Precision::Minute, 0, None));
        }
        DocItem::new(id, tpl, alias)
            .with_sections(vec![Section { index: 0, title: "段".into(), text }])
            .with_fields(fields)
            .with_timestamps(timestamps)
    }

    const RULESET: &str = r#"
version: "cross-1"
templates:
  restraint_record:
    display: 约束记录
    fields:
      - key: 约束起始时间
        required: true
  restraint_release:
    display: 约束解除记录
    fields:
      - key: 解除时间
        required: true
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        required: true
  assessment_report:
    display: 评估报告
    fields:
      - key: 评估时间
        required: true
  sign_page:
    display: 签署页
    fields:
      - key: 护士签字
        required: true
      - key: 医师签字
        required: true
hospital:
  doc_types:
    restraint_record: "约束记录"
    restraint_release: "约束解除记录"
    restraint_consent: "约束知情同意"
    assessment_report: "评估报告"
    sign_page: "签署页"
  rules:
    - code: "QC-PSY-05"
      severity: blocking
      basis: "《精神科住院护理常规》第 6.1 条"
    - code: "QC-PSY-06"
      severity: blocking
      basis: "《精神科住院护理常规》第 6.2 条"
    - code: "QC-PSY-07"
      severity: blocking
      basis: "《精神科住院护理常规》第 6.3 条"
    - code: "QC-PSY-08"
      severity: blocking
      basis: "《精神科住院护理常规》第 4.5 条"
"#;

    fn ruleset() -> EffectiveRules {
        crate::load_effective(RULESET).unwrap()
    }

    fn codes<'a>(out: &'a CrossOutcome, code: &str) -> Vec<&'a Diagnostic> {
        out.diagnostics.iter().filter(|d| d.code == code).collect()
    }

    fn skip_of<'a>(out: &'a CrossOutcome, code: &str, reason: SkipReason) -> &'a SkippedCheck {
        out.skipped
            .iter()
            .find(|s| s.code == code && s.reason == reason)
            .unwrap_or_else(|| panic!("应有一条 {code} / {reason:?} 的跳过记录，实际 {:?}", out.skipped))
    }

    // —— QC-PSY-05 ——

    #[test]
    fn qc_psy_05_flags_release_recorded_before_the_restraint_started() {
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        // 违反的两件各一条：护士长在任一件里都看得到「和哪一件对不上」。
        let hits = codes(&out, QC_PSY_05);
        assert_eq!(hits.len(), 2, "应给两件各一条：{:?}", out.diagnostics);
        assert!(hits[0].loc.doc_id == "REC" && hits[1].loc.doc_id == "REL");
        assert!(hits[0].message.text.contains("早于"), "{}", hits[0].message.text);
        assert!(hits[1].message.text.contains("晚于"), "{}", hits[1].message.text);
        assert!(hits[0].message.basis.contains("第 6.1 条"));
    }

    #[test]
    fn qc_psy_05_stays_quiet_when_the_order_is_right() {
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-03 06:10")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "合规顺序不该报：{:?}", out.diagnostics);
    }

    // —— QC-PSY-06 ——

    #[test]
    fn qc_psy_06_flags_assessment_later_than_the_first_restraint() {
        let items = vec![
            doc("ASS", "assessment_report", "W-1", None,
                &[("评估时间", "2026-03-03 10:00")]),
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 08:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        let hits = codes(&out, QC_PSY_06);
        assert_eq!(hits.len(), 2, "{:?}", out.diagnostics);
        // 基准件（约束记录）与被比较件（评估报告）各一条。
        assert!(hits[0].loc.doc_id == "REC" && hits[1].loc.doc_id == "ASS");
        assert!(hits[0].message.text.contains("评估时序倒挂"), "{}", hits[0].message.text);
    }

    #[test]
    fn qc_psy_06_accepts_assessment_before_the_restraint() {
        let items = vec![
            doc("ASS", "assessment_report", "W-1", None,
                &[("评估时间", "2026-03-02 09:00")]),
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // —— QC-PSY-07 ——

    #[test]
    fn qc_psy_07_flags_consent_signed_after_the_restraint_started() {
        let items = vec![
            doc("CON", "restraint_consent", "W-1", Some("2026-03-03 09:10"),
                &[("患方签字", "<EXAMPLE_SIGNATURE>")]),
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 08:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        let hits = codes(&out, QC_PSY_07);
        assert_eq!(hits.len(), 2, "{:?}", out.diagnostics);
        assert!(hits[0].loc.doc_id == "REC" && hits[1].loc.doc_id == "CON");
        assert!(hits[0].message.text.contains("先约束、后知情"), "{}", hits[0].message.text);
    }

    #[test]
    fn qc_psy_07_accepts_consent_signed_before_the_restraint() {
        let items = vec![
            doc("CON", "restraint_consent", "W-1", Some("2026-03-02 07:50"),
                &[("患方签字", "<EXAMPLE_SIGNATURE>")]),
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 08:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    // —— 边界：缺时刻 / 相等 / 精度 / 歧义 ——

    #[test]
    fn a_missing_moment_skips_the_rule_instead_of_guessing() {
        // 约束记录抽不出起始时刻（键在但值里没有带日期的时刻）→ 整条跳过。
        let items = vec![
            doc("REC", "restraint_record", "W-1", None, &[("约束起始时间", "当日夜间")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "缺时刻时不得判：{:?}", out.diagnostics);
        let s = skip_of(&out, QC_PSY_05, SkipReason::MissingTimestamp);
        assert_eq!(s.primary_doc, "REC");
        assert!(!s.detail.is_empty());
    }

    #[test]
    fn an_identical_moment_is_recorded_as_inconclusive_not_as_pass() {
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 23:40")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "完全相等不得判倒挂：{:?}", out.diagnostics);
        let s = skip_of(&out, QC_PSY_05, SkipReason::EqualMoment);
        assert!(s.reason.is_inconclusive());
        assert!(s.as_zh().contains("完全相等"), "{}", s.as_zh());
    }

    #[test]
    fn a_gap_smaller_than_the_coarser_resolution_is_not_judged() {
        // authoredOn 精确到秒、约束起始只写到分：相差 30 秒不足以判先后。
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 08:00")]),
            doc("CON", "restraint_consent", "W-1", Some("2026-03-02 08:00"),
                &[("患方签字", "<EXAMPLE_SIGNATURE>")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty());
        // 恰好相等走 EqualMoment；把它挪到「差得比分辨力还小」需要秒级时刻，
        // 这里用两个不同类型验证精度分支确实可达。
        let inconclusive: Vec<&SkippedCheck> =
            out.skipped.iter().filter(|s| s.reason.is_inconclusive()).collect();
        assert!(!inconclusive.is_empty(), "至少要留下一条不可判记录：{:?}", out.skipped);
    }

    #[test]
    fn an_ambiguous_item_skips_the_whole_pair() {
        let mut record = doc("REC", "restraint_record", "W-1", None,
            &[("约束起始时间", "2026-03-02 23:40")]);
        record = record.with_binding(Binding::Ambiguous);
        let items = vec![
            record,
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "件绑定不唯一时不得下结论：{:?}", out.diagnostics);
        assert_eq!(
            skip_of(&out, QC_PSY_05, SkipReason::AmbiguousBinding).primary_doc,
            "restraint_record"
        );
    }

    #[test]
    fn a_missing_counterpart_is_recorded_rather_than_silently_passed() {
        // 只有约束记录、没有解除记录：不能报「时间序正常」，也不能凭空造一条解除件。
        let items = vec![doc("REC", "restraint_record", "W-1", None,
            &[("约束起始时间", "2026-03-02 23:40")])];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty());
        let s = skip_of(&out, QC_PSY_05, SkipReason::NoCounterpart);
        assert!(s.detail.contains("restraint_release"), "{}", s.detail);
        assert_eq!(s.encounter_alias, "W-1");
    }

    #[test]
    fn two_records_of_the_same_type_are_ambiguous_not_first_wins() {
        // 跨系统各导一份约束记录 → 取任一份都可能给出错配对的结论。
        let items = vec![
            doc("REC-HIS", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REC-NURSE", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:55")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "同名多件时不得下结论：{:?}", out.diagnostics);
        assert_eq!(skip_of(&out, QC_PSY_05, SkipReason::AmbiguousBinding).reason, SkipReason::AmbiguousBinding);
    }

    #[test]
    fn items_from_different_encounters_never_get_joined() {
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-2", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "跨住院别名误连是数据串号：{:?}", out.diagnostics);
        // 两个住院各记一条 QC-PSY-05 的缺对照件，且**没有一条把两边的件连起来**。
        let by_alias: BTreeMap<&str, usize> =
            out.skipped.iter().fold(BTreeMap::new(), |mut acc, s| {
                *acc.entry(s.encounter_alias.as_str()).or_default() += 1;
                acc
            });
        assert_eq!(by_alias.get("W-1").copied(), Some(3), "{by_alias:?}");
        assert_eq!(by_alias.get("W-2").copied(), Some(3), "{by_alias:?}");
        assert!(out.diagnostics.is_empty(), "跨住院别名不得产生任何跨件结论");
    }

    // —— QC-PSY-08 ——

    #[test]
    fn qc_psy_08_flags_a_signed_document_with_no_signing_moment() {
        let items = vec![doc("SIGN", "sign_page", "W-1", None,
            &[("护士签字", "<EXAMPLE_NURSE>"), ("医师签字", "<EXAMPLE_PRACTITIONER>")])];
        let out = run_cross_doc(&items, &ruleset());
        let hits = codes(&out, QC_PSY_08);
        assert_eq!(hits.len(), 1, "{:?}", out.diagnostics);
        assert!(hits[0].message.text.contains("护士签字"), "{}", hits[0].message.text);
        assert!(hits[0].message.text.contains("医师签字"));
        assert_eq!(hits[0].fix_availability, psychdoc_model::FixAvailability::None, "绝不生成签名动作");
        assert!(hits[0].fix.is_none());
    }

    #[test]
    fn qc_psy_08_stays_quiet_when_the_signing_moment_is_present() {
        let items = vec![doc("SIGN", "sign_page", "W-1", Some("2026-03-02 09:00"),
            &[("护士签字", "<EXAMPLE_NURSE>"), ("医师签字", "<EXAMPLE_PRACTITIONER>")])];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "{:?}", out.diagnostics);
    }

    #[test]
    fn qc_psy_08_leaves_unsigned_documents_to_qc_psy_01() {
        // 签署位压根没填 → 那是件内必填缺项（QC-PSY-01）的活，本条不重复报。
        let items = vec![doc("SIGN", "sign_page", "W-1", None, &[("护士签字", ""), ("医师签字", "")])];
        let out = run_cross_doc(&items, &ruleset());
        assert!(out.diagnostics.is_empty(), "同一处不刷两行：{:?}", out.diagnostics);
    }

    #[test]
    fn signature_key_detection_covers_both_writings() {
        assert!(is_signature_key("患方签字"));
        assert!(is_signature_key("医师签名"));
        assert!(!is_signature_key("执行护士"));
        assert!(!is_signature_key("病情描述"));
    }

    // —— 纪律 ——

    #[test]
    fn a_rule_absent_from_the_ruleset_does_not_run() {
        let only_08 = RULESET
            .replace("    - code: \"QC-PSY-05\"\n      severity: blocking\n      basis: \"《精神科住院护理常规》第 6.1 条\"\n", "")
            .replace("    - code: \"QC-PSY-06\"\n      severity: blocking\n      basis: \"《精神科住院护理常规》第 6.2 条\"\n", "")
            .replace("    - code: \"QC-PSY-07\"\n      severity: blocking\n      basis: \"《精神科住院护理常规》第 6.3 条\"\n", "");
        let rs = crate::load_effective(&only_08).unwrap();
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
        ];
        let out = run_cross_doc(&items, &rs);
        assert!(out.diagnostics.is_empty(), "规则集未声明的码一条都不执行");
        assert!(out.skipped.is_empty(), "未声明的码也不该留下跳过记录：{:?}", out.skipped);
    }

    #[test]
    fn every_cross_doc_message_cites_an_in_hospital_clause() {
        let items = vec![
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
            doc("REL", "restraint_release", "W-1", None,
                &[("解除时间", "2026-03-02 07:00")]),
            doc("ASS", "assessment_report", "W-1", None,
                &[("评估时间", "2026-03-05 10:00")]),
            doc("CON", "restraint_consent", "W-1", Some("2026-03-05 09:10"),
                &[("患方签字", "<EXAMPLE_SIGNATURE>")]),
            doc("SIGN", "sign_page", "W-1", None,
                &[("护士签字", "<EXAMPLE_NURSE>"), ("医师签字", "<EXAMPLE_PRACTITIONER>")]),
        ];
        let out = run_cross_doc(&items, &ruleset());
        assert!(!out.diagnostics.is_empty());
        for d in &out.diagnostics {
            assert!(d.message.basis.contains("《精神科住院护理常规》第"), "{} 的依据不是院内规范条款", d.code);
            assert!(d.is_consistent(), "{} 的 fix_availability 与载荷不自洽", d.code);
        }
        for code in [QC_PSY_05, QC_PSY_06, QC_PSY_07, QC_PSY_08] {
            assert!(!codes(&out, code).is_empty(), "{code} 应至少命中 1 条");
        }
    }

    #[test]
    fn grouping_is_stable_across_runs() {
        let items = vec![
            doc("REL", "restraint_release", "W-2", None,
                &[("解除时间", "2026-03-02 07:00")]),
            doc("REC", "restraint_record", "W-1", None,
                &[("约束起始时间", "2026-03-02 23:40")]),
        ];
        assert_eq!(group_by_encounter(&items), group_by_encounter(&items));
        let aliases: Vec<&str> = group_by_encounter(&items).iter().map(|g| g.alias).collect();
        assert_eq!(aliases, vec!["W-1", "W-2"], "分组顺序必须可复现");
    }
}
