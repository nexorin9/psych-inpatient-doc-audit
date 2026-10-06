//! 规则执行器 A：**件-字段类**（`QC-PSY-01` ~ `QC-PSY-04`）。
//!
//! ## 源产品主能力与本轮处置
//!
//! 源产品每条规则是一个带稳定编号的类型（`AIR304`），通过
//! `impl Violation::message()` 说清「为什么这是问题」，并用
//! `const FIX_AVAILABILITY` 决定能否自动修
//! （`github_ref/ruff/crates/ruff_linter/src/rules/airflow/rules/runtime_value_in_dag_or_task.rs:44`）。
//!
//! 本模块把那个形状落成 [`Rule`] trait + 注册表：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | 一条规则 = 一个类型 | [`Rule`] trait 的一个实现 + 稳定码 `QC-PSY-nn` |
//! | `Violation::message()` | [`Rule::check`] 里构造的文案，**每条必须引用院内规范条款** |
//! | `FIX_AVAILABILITY` 常量 | 每条诊断的 `FixAvailability`：`None` 只给依据不给载荷 |
//! | 规则按文件类型注册 | 按院内文书类型声明 `applies_to`，注册表只放院内认的码 |
//!
//! 保留的关键行为有两条：
//!
//! 1. **不可自动修是一等状态**：涉及患方签字、临床数值、单位的一律
//!    `FixAvailability::None`——UI 只渲染依据行，不渲染补写按钮。
//! 2. **`message()` 必须说清为什么**：本项目把「为什么」落到**院内规范条款**
//!    （由 `RuleMessage` 构造期强制，缺依据即 `E_NO_RULE_BASIS`）。
//!
//! 四条规则的分工：
//!
//! | 码 | 判什么 | 可否自动修 |
//! |---|---|---|
//! | `QC-PSY-01` | 必填字段缺失（缺失 / null / 空串三态） | 否 |
//! | `QC-PSY-02` | 值形状不符（数值 / 整数 / 日期 / 日期时间） | **仅全角半角归一**可 |
//! | `QC-PSY-03` | 取值域越界（枚举不在域内 / 数值越界） | 否 |
//! | `QC-PSY-04` | 单位或量纲错配 | 否 |

use psychdoc_model::{
    Diagnostic, DocItem, DocLoc, Field, FieldSpec, FieldPresence, FixAvailability, FixPayload,
    RuleMessage, Severity, Span, TemplateSpec, ValueType,
};

use crate::ruleset::{EffectiveRule, EffectiveRules};
use crate::values::{self, ParsedValue};

pub use crate::values::ValueError;

/// `QC-PSY-01`：件内必填字段缺失。
pub const QC_PSY_01: &str = "QC-PSY-01";
/// `QC-PSY-02`：字段值形状不符（类型 / 全半角）。
pub const QC_PSY_02: &str = "QC-PSY-02";
/// `QC-PSY-03`：取值域越界（枚举或数值区间）。
pub const QC_PSY_03: &str = "QC-PSY-03";
/// `QC-PSY-04`：单位 / 量纲错配。
pub const QC_PSY_04: &str = "QC-PSY-04";

/// 一个字段在同一件里被解析出的结果。
///
/// 「同一字段在多段落出现」是真实情况（约束起始时间既写在「患方知情同意」
/// 段、又写在「约束经过」段），此时**不能随便挑一段**——挑错了会给出一条
/// 建立在错数据上的结论。因此这种情况走 [`FieldPick::Ambiguous`]，
/// 由上层显式记账交人工，而不是悄悄取第一个。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FieldPick<'a> {
    /// 字段根本没出现。
    Missing,
    /// 唯一命中，可判定。
    Found(&'a Field),
    /// 同名字段出现在多个段落，且模板未把范围收窄到某一段 → 不判定。
    /// 载荷是命中次数：够上层记「这件的这个键有歧义」，
    /// 又不至于让调用方拿到一个指向临时切片的引用。
    Ambiguous(usize),
}

impl<'a> FieldPick<'a> {
    pub fn field(&self) -> Option<&'a Field> {
        match self {
            Self::Found(f) => Some(f),
            _ => None,
        }
    }
    pub fn is_ambiguous(&self) -> bool {
        matches!(self, Self::Ambiguous(_))
    }
}

/// 按模板声明定位一个字段。
///
/// 模板写明了 `section` 就优先只在那一段里找（那一段唯一命中即可判定，
/// 落在别的段的同名件不算数）；该段没写或没命中，才退回整件查找——
/// 退回后出现多次即为歧义，**不取第一个**。
pub fn pick_field<'a>(item: &'a DocItem, spec: &FieldSpec) -> FieldPick<'a> {
    let all: Vec<&'a Field> = item.fields.iter().filter(|f| f.key == spec.key).collect();
    if let Some(title) = spec.section.as_deref() {
        let in_sec: Vec<&'a Field> = all
            .iter()
            .copied()
            .filter(|f| {
                item.sections
                    .get(f.section_index as usize)
                    .is_some_and(|s| s.title == title)
            })
            .collect();
        match in_sec.len() {
            1 => return FieldPick::Found(in_sec[0]),
            0 => {} // 模板声明的段里没写 → 退回整件
            n => return FieldPick::Ambiguous(n),
        }
    }
    match all.len() {
        0 => FieldPick::Missing,
        1 => FieldPick::Found(all[0]),
        n => FieldPick::Ambiguous(n),
    }
}

/// 执行一条规则时能看到的上下文。
pub struct RuleCtx<'a, 'e> {
    pub item: &'a DocItem,
    pub spec: &'a FieldSpec,
    pub pick: FieldPick<'a>,
    /// 文书类型中文名（进文案：「约束知情同意件…」）。
    pub display: &'a str,
    /// 该码的生效声明：严重度与规范条款依据都从这里取，
    /// **不从代码里写死**——否则三级级联覆盖改的就只是 severity，而依据改不动。
    pub effective: &'e EffectiveRule,
}

impl RuleCtx<'_, '_> {
    /// 字段值区间（用于高亮）；字段缺出处时零宽区间落在段首。
    fn value_span(&self) -> Span {
        self.pick
            .field()
            .and_then(|f| f.source_span)
            .unwrap_or_else(|| Span::unchecked(0, 0))
    }

    fn section_index(&self) -> u32 {
        self.pick
            .field()
            .map(|f| f.section_index)
            .unwrap_or_else(|| expected_section_index(self.item, self.spec.section.as_deref()))
    }

    fn loc(&self) -> DocLoc {
        DocLoc::new(&self.item.doc_id, self.section_index(), self.value_span())
    }

    /// 字段有值时的解析结果；没值返回 `None`（空值由 `QC-PSY-01` 负责）。
    fn parsed_value(&self) -> Option<ParsedValue> {
        let f = self.pick.field()?;
        if f.presence() != FieldPresence::Present {
            return None;
        }
        Some(values::parse(f.value.as_deref().unwrap_or_default()))
    }

    /// 三态的中文说法，进文案（护士长看得懂，而不是 Missing/Empty）。
    fn presence_zh(&self) -> &'static str {
        match self.pick.field().map(Field::presence) {
            Some(FieldPresence::Empty) => "有键无值",
            Some(FieldPresence::Null) => "值为 null",
            Some(FieldPresence::Present) => "已填",
            _ => "未填写",
        }
    }

    fn diagnostic(&self, text: String) -> Diagnostic {
        let msg = RuleMessage::new(text, &self.effective.basis)
            .expect("加载期已保证依据非空");
        Diagnostic::new(self.effective.code.clone(), self.loc(), msg).with_severity(self.effective.severity)
    }
}

/// 该字段**应当**落在哪个段落下标：按模板声明的段标题匹配；
/// 模板没声明段落（或该段不存在）→ 落在 0 段起点，不编造段号。
fn expected_section_index(item: &DocItem, section_title: Option<&str>) -> u32 {
    section_title
        .and_then(|title| item.sections.iter().find(|s| s.title == title))
        .map(|s| s.index)
        .unwrap_or(0)
}

/// 一条院内规则。
///
/// 实现者只需回答两件事：**这个字段声明我要不要看**（[`Rule::applies`]）、
/// **看出了问题怎么说**（[`Rule::check`]）。严重度、依据、可否自动修
/// 由基类统一从生效声明里取，避免各条规则各写各的。
pub trait Rule: Send + Sync {
    /// 稳定编号 `QC-PSY-nn`。
    fn code(&self) -> &'static str;

    /// 规则中文名（运行摘要与规则表用）。
    fn title(&self) -> &'static str;

    /// 这条规则是否对某个字段声明有意义。
    /// 形状不匹配就返回 `false`（例如没声明取值域的字段不看 `QC-PSY-03`）。
    fn applies(&self, spec: &FieldSpec) -> bool;

    /// 判定并产出诊断；字段没被填时返回空（那是 `QC-PSY-01` 的活）。
    fn check(&self, ctx: &RuleCtx<'_, '_>) -> Vec<Diagnostic>;
}

/// `QC-PSY-01`：必填字段缺失。三态区分决定定位语义：
/// **键出现但为空** → 指到值区间；**键根本没出现** → 指到应落段落起点。
#[derive(Debug, Default, Clone, Copy)]
pub struct RequiredFieldRule;

impl Rule for RequiredFieldRule {
    fn code(&self) -> &'static str {
        QC_PSY_01
    }
    fn title(&self) -> &'static str {
        "件内必填字段缺失"
    }
    fn applies(&self, spec: &FieldSpec) -> bool {
        spec.required
    }
    fn check(&self, ctx: &RuleCtx<'_, '_>) -> Vec<Diagnostic> {
        // 歧义字段不判定：取任一段都可能给出错误结论，交人工。
        if ctx.pick.is_ambiguous() {
            return Vec::new();
        }
        // 判「有没有填」看 `presence()`，不看解析时抄进来的 `required` 标记：
        // 那个标记来自解析当时的字典快照，字典一改就会与规则集脱节。
        let unfilled = match ctx.pick {
            FieldPick::Missing => true,
            FieldPick::Found(f) => f.presence() != FieldPresence::Present,
            FieldPick::Ambiguous(_) => return Vec::new(),
        };
        if !unfilled {
            return Vec::new();
        }
        let text = format!(
            "{}件缺少必填项「{}」（{}）",
            ctx.display,
            ctx.spec.key,
            ctx.presence_zh()
        );
        // 缺项永远不可自动补写：签字、临床数值不是工具能代填的东西。
        vec![ctx.diagnostic(text).without_fix()]
    }
}

/// `QC-PSY-02`：值形状不符。
///
/// 唯一**可自动修**的分支是「全角数字 / 全角标点」——语义未变，只是键盘没切，
/// 归一化后逐字节可对照。这类条目给 `FixAvailability::Automatic` + 载荷，
/// 其余（值本身读不懂、整数写成小数、日期写成月日序）一律 `None`。
#[derive(Debug, Default, Clone, Copy)]
pub struct ValueShapeRule;

impl Rule for ValueShapeRule {
    fn code(&self) -> &'static str {
        QC_PSY_02
    }
    fn title(&self) -> &'static str {
        "字段值形状不符"
    }
    fn applies(&self, spec: &FieldSpec) -> bool {
        spec.value_type != ValueType::Text
    }
    fn check(&self, ctx: &RuleCtx<'_, '_>) -> Vec<Diagnostic> {
        let Some(parsed) = ctx.parsed_value() else {
            return Vec::new();
        };
        // 拿**原文**判形状：全角数字按规范就是形状不符，
        // 哪怕归一之后能解析成合法日期。
        if values::check_shape(&parsed, ctx.spec.value_type).is_ok() {
            return Vec::new();
        }
        let err = values::check_shape(&parsed, ctx.spec.value_type).expect_err("上面已判为不符");
        let text = format!(
            "{}件「{}」应为{}，实际填「{}」：{}",
            ctx.display,
            ctx.spec.key,
            ctx.spec.value_type.as_zh(),
            parsed.raw,
            err.describe()
        );
        // 唯一可自动修的分支：**仅**当归一化（全角 → 半角）之后形状真的对得上。
        // 这类改动不动一个字的内容，diff 可逐字节对照；其余一律交人工。
        let auto_fixable = parsed.normalized_changed
            && values::check_shape(
                &values::parse(&parsed.normalized),
                ctx.spec.value_type,
            )
            .is_ok();
        let d = ctx.diagnostic(text);
        if auto_fixable {
            vec![d.with_fix(FixPayload {
                replacement: format!("{}：{}", ctx.spec.key, parsed.normalized),
                summary: format!("将修改 1 段（{} 全角 → 半角，内容不变）", ctx.spec.key),
            })]
        } else {
            vec![d.without_fix()]
        }
    }
}

/// `QC-PSY-03`：取值域越界（枚举不在域内，或数值越出闭区间）。
#[derive(Debug, Default, Clone, Copy)]
pub struct DomainRule;

impl Rule for DomainRule {
    fn code(&self) -> &'static str {
        QC_PSY_03
    }
    fn title(&self) -> &'static str {
        "取值域越界"
    }
    fn applies(&self, spec: &FieldSpec) -> bool {
        spec.has_domain()
    }
    fn check(&self, ctx: &RuleCtx<'_, '_>) -> Vec<Diagnostic> {
        let Some(parsed) = ctx.parsed_value() else {
            return Vec::new();
        };
        let spec = ctx.spec;

        // 枚举域优先：表单上印的是勾选项，填了域外的字就是填错了。
        if !spec.allowed.is_empty() {
            // 两侧都剥文末句读：文书里「二级保护。」带句号，表单勾选项不带。
            let allowed: Vec<&str> = spec
                .allowed
                .iter()
                .map(|a| values::trim_sentence_punct(a).trim())
                .collect();
            let got = values::trim_sentence_punct(&parsed.normalized).trim();
            if allowed.contains(&got) {
                return Vec::new();
            }
            let text = format!(
                "{}件「{}」取值「{}」不在院内允许范围内：{}",
                ctx.display,
                spec.key,
                got,
                allowed.join(" / ")
            );
            return vec![ctx.diagnostic(text).without_fix()];
        }

        // 数值区间。解析不出数值说明这是形状问题，归 `QC-PSY-02` 管，
        // 这里不重复报（同一处刷两行正是规则重叠的典型失败模式）。
        let Some(n) = parsed.number else {
            return Vec::new();
        };
        let below = spec.min.is_some_and(|lo| n < lo);
        let above = spec.max.is_some_and(|hi| n > hi);
        if !(below || above) {
            return Vec::new();
        }
        let range = match (spec.min, spec.max) {
            (Some(lo), Some(hi)) => format!("{lo} ~ {hi}"),
            (Some(lo), None) => format!("不小于 {lo}"),
            (None, Some(hi)) => format!("不大于 {hi}"),
            (None, None) => String::new(),
        };
        let unit = spec.unit.clone().map(|u| format!(" {u}")).unwrap_or_default();
        let text = format!(
            "{}件「{}」填 {}{}，越出允许范围（{range}）",
            ctx.display,
            spec.key,
            trim_float(n),
            unit
        );
        vec![ctx.diagnostic(text).without_fix()]
    }
}

/// `QC-PSY-04`：单位 / 量纲错配。
///
/// 三种错法分别说清：写了别的单位（`2 小时` vs 表单要的 `分钟`）、
/// 跨量纲（时长字段填了 `500 毫克`）、以及压根没写单位。
/// 跨量纲最危险——数值看着正常，实际完全不是一回事，所以文案里点名量纲。
#[derive(Debug, Default, Clone, Copy)]
pub struct UnitRule;

impl Rule for UnitRule {
    fn code(&self) -> &'static str {
        QC_PSY_04
    }
    fn title(&self) -> &'static str {
        "单位 / 量纲错配"
    }
    fn applies(&self, spec: &FieldSpec) -> bool {
        spec.unit.is_some()
    }
    fn check(&self, ctx: &RuleCtx<'_, '_>) -> Vec<Diagnostic> {
        let Some(parsed) = ctx.parsed_value() else {
            return Vec::new();
        };
        let Some(spec) = ctx.spec.unit.as_deref() else {
            return Vec::new();
        };
        let Some(want) = values::lookup_unit(spec) else {
            // 表单上写的单位不在词表内：不能判「错配」——
            // 不知道词表里没有的单位该跟谁比。这一条由加载期外的配置问题负责，
            // 执行期只跳过，不猜。
            return Vec::new();
        };
        if parsed.number.is_none() {
            return Vec::new();
        }
        match parsed.unit() {
            // 单位正确（写法可能是 `min` / `Min`），不报。
            Some(got) if got.canonical == want.canonical => Vec::new(),
            // 跨量纲：数值看着正常，含义完全不是一回事。
            Some(got) if got.dimension != want.dimension => vec![ctx
                .diagnostic(format!(
                    "{}件「{}」按「{}」（{}）计量，实际填的是「{}」（{}）",
                    ctx.display,
                    ctx.spec.key,
                    want.canonical,
                    want.dimension.as_zh(),
                    parsed.raw,
                    got.dimension.as_zh()
                ))
                .without_fix()],
            // 同量纲不同单位：可换算，但要人确认换算后是否本意，绝不自动改。
            Some(got) => vec![ctx
                .diagnostic(format!(
                    "{}件「{}」应按「{want}」计量，实际填「{}」（单位：{}）",
                    ctx.display,
                    ctx.spec.key,
                    parsed.raw,
                    got.canonical
                ))
                .without_fix()],
            // 数值 + 应有单位却没写：数值本身无意义（「30」是 30 分钟还是 30 小时？）。
            None => vec![ctx
                .diagnostic(format!(
                    "{}件「{}」应按「{want}」计量，实际填「{}」未注明单位",
                    ctx.display,
                    ctx.spec.key,
                    parsed.raw
                ))
                .without_fix()],
        }
    }
}

/// 规则注册表：院内认的全部件-字段类规则。
/// 执行器 B（跨文书时间序 / 签署齐备性，Task 7）会在同一个注册表上追加。
pub fn registry() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(RequiredFieldRule),
        Box::new(ValueShapeRule),
        Box::new(DomainRule),
        Box::new(UnitRule),
    ]
}

/// 一条因「同名字段跨段歧义」而未判定的记录。
///
/// 不判定是对的，但**必须留痕**：否则「这条为什么没报」就成了查不到出处的问题。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AmbiguousField {
    pub doc_id: String,
    pub key: String,
    /// 出现在哪几个段落下标（升序）。
    pub sections: Vec<u32>,
}

/// 对一件跑全部件-字段类规则。
///
/// 返回 `(诊断, 歧义记录)`。歧义不是诊断（不该阻断提交），
/// 但要能按住院别名回查，因此单独返回。
pub fn run_item(
    item: &DocItem,
    tpl: &TemplateSpec,
    ruleset: &EffectiveRules,
) -> (Vec<Diagnostic>, Vec<AmbiguousField>) {
    let display = tpl.display_or(&item.template_id);
    let mut out = Vec::new();
    let mut ambiguous = Vec::new();
    for spec in tpl.fields.iter() {
        let pick = pick_field(item, spec);
        if pick.is_ambiguous() {
            let sections = item
                .fields
                .iter()
                .filter(|f| f.key == spec.key)
                .map(|f| f.section_index)
                .collect::<Vec<_>>();
            let mut sections = sections;
            sections.sort_unstable();
            sections.dedup();
            ambiguous.push(AmbiguousField {
                doc_id: item.doc_id.clone(),
                key: spec.key.clone(),
                sections,
            });
            continue;
        }
        for rule in registry() {
            let Some(effective) = ruleset.rule(rule.code()) else {
                continue;
            };
            if !effective.applies_to_doc_type(&item.template_id) || !rule.applies(spec) {
                continue;
            }
            let ctx = RuleCtx { item, spec, pick, display, effective };
            out.extend(rule.check(&ctx));
        }
    }
    (out, ambiguous)
}

/// 数值展示：整数不带小数点，非整数最多保留两位（够护士长看懂，又不制造假精度）。
fn trim_float(n: f64) -> String {
    if n.fract() == 0.0 {
        format!("{n:.0}")
    } else {
        let s = format!("{n:.2}");
        s.trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

/// 供上层复用的严重度读取（`Rule` 实现不需要）。
pub fn severity_of(ruleset: &EffectiveRules, code: &str) -> Severity {
    ruleset.severity_for(code).unwrap_or_default()
}

/// 供上层复用：`FixAvailability` 默认即 `None`（不可自动修是一等状态）。
pub fn default_fix() -> FixAvailability {
    FixAvailability::None
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Section;

    fn spec(key: &str, section: Option<&str>) -> FieldSpec {
        FieldSpec {
            key: key.into(),
            section: section.map(str::to_string),
            required: false,
            value_type: ValueType::Text,
            allowed: vec![],
            min: None,
            max: None,
            unit: None,
        }
    }

    fn field(key: &str, value: Option<&str>, section_index: u32) -> Field {
        Field {
            key: key.into(),
            value: value.map(str::to_string),
            required: false,
            source_span: Some(Span::unchecked(0, 1)),
            section_index,
        }
    }

    fn item(fields: Vec<Field>, sections: Vec<Section>) -> DocItem {
        let mut d = DocItem::new("D-1", "restraint_record", "WARD-A-001").with_fields(fields);
        d = d.with_sections(sections);
        d
    }

    fn sections() -> Vec<Section> {
        vec![
            Section { index: 0, title: "约束经过".into(), text: "约束经过：x".into() },
            Section { index: 1, title: "补充说明".into(), text: "补充说明：y".into() },
            Section { index: 2, title: "患方知情同意".into(), text: "患方知情同意：z".into() },
        ]
    }

    fn effective(code: &str) -> EffectiveRule {
        EffectiveRule {
            code: code.into(),
            severity: Severity::Blocking,
            title: "t".into(),
            basis: "《精神科住院护理常规》第 5.1 条".into(),
            applies_to: vec![],
            source: crate::ruleset::LayerId::Hospital,
            superseded: vec![],
        }
    }

    /// 构造规则上下文。`EffectiveRule` 由调用方持有（不做 `Box::leak`，
    /// 测试里也不该为省事而泄漏内存）。
    fn ctx_for<'a, 'e>(
        item: &'a DocItem,
        spec: &'a FieldSpec,
        effective: &'e EffectiveRule,
    ) -> RuleCtx<'a, 'e> {
        let pick = pick_field(item, spec);
        RuleCtx { item, spec, pick, display: "约束记录", effective }
    }

    // —— 字段定位 ——

    #[test]
    fn single_occurrence_in_declared_section_is_found() {
        let d = item(vec![field("约束等级", Some("二级保护"), 0)], sections());
        assert!(matches!(pick_field(&d, &spec("约束等级", Some("约束经过"))), FieldPick::Found(_)));
    }

    #[test]
    fn same_key_in_two_sections_is_ambiguous_not_first_wins() {
        let d = item(
            vec![field("约束起始时间", Some("23:40"), 0), field("约束起始时间", Some("23:50"), 1)],
            sections(),
        );
        let pick = pick_field(&d, &spec("约束起始时间", None));
        assert_eq!(pick, FieldPick::Ambiguous(2), "取第一个就可能给出一条建立在错数据上的结论");
        assert!(pick.field().is_none());
    }

    #[test]
    fn declared_section_narrows_away_a_duplicate_elsewhere() {
        // 同名字段出现在两段，但模板写明了落在哪一段 → 那一段唯一即可判定。
        let d = item(
            vec![field("约束起始时间", Some("23:40"), 0), field("约束起始时间", Some("23:50"), 1)],
            sections(),
        );
        let pick = pick_field(&d, &spec("约束起始时间", Some("补充说明")));
        match pick {
            FieldPick::Found(f) => assert_eq!(f.value.as_deref(), Some("23:50")),
            other => panic!("应唯一命中补充说明段，实际 {other:?}"),
        }
    }

    #[test]
    fn declared_section_with_two_hits_is_still_ambiguous() {
        let d = item(
            vec![field("约束起始时间", Some("23:40"), 0), field("约束起始时间", Some("23:50"), 0)],
            sections(),
        );
        assert_eq!(pick_field(&d, &spec("约束起始时间", Some("约束经过"))), FieldPick::Ambiguous(2));
    }

    #[test]
    fn absent_field_is_missing_not_empty() {
        let d = item(vec![], sections());
        assert!(matches!(pick_field(&d, &spec("患方签字", None)), FieldPick::Missing));
    }

    // —— QC-PSY-01 ——

    #[test]
    fn qc_psy_01_reports_unfilled_with_presence_wording() {
        let mut s = spec("患方签字", Some("患方知情同意"));
        s.required = true;
        // 键出现但值为 null / 空串 → 定位到值区间，文案说清是哪种空
        for (value, want) in [(None, "值为 null"), (Some(""), "有键无值")] {
            let d = item(vec![field("患方签字", value, 1)], sections());
            let eff = effective(QC_PSY_01);
            let ds = RequiredFieldRule.check(&ctx_for(&d, &s, &eff));
            assert_eq!(ds.len(), 1, "值={value:?}");
            assert!(ds[0].message.text.contains(want), "值={value:?} → {}", ds[0].message.text);
            assert_eq!(ds[0].fix_availability, FixAvailability::None, "缺项不自动补写");
        }
        // 键根本没出现 → 定位到应落段落起点，文案说「未填写」
        let d = item(vec![], sections());
        let eff = effective(QC_PSY_01);
        let ds = RequiredFieldRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("未填写"));
        assert_eq!(ds[0].loc.section_index, 2, "应落在模板声明的「患方知情同意」段");
        assert!(ds[0].loc.char_range.is_empty(), "字段没出现 → 零宽区间，不编造值位置");
    }

    #[test]
    fn qc_psy_01_message_always_cites_the_basis() {
        let mut s = spec("患方签字", None);
        s.required = true;
        let d = item(vec![], sections());
        let eff = effective(QC_PSY_01);
            let ds = RequiredFieldRule.check(&ctx_for(&d, &s, &eff));
        assert!(ds[0].message.basis.contains("第 5.1 条"));
    }

    // —— QC-PSY-02 ——

    #[test]
    fn qc_psy_02_flags_non_numeric_for_number_field_without_fix() {
        let mut s = spec("约束时长", None);
        s.value_type = ValueType::Number;
        let d = item(vec![field("约束时长", Some("两小时"), 0)], sections());
        let eff = effective(QC_PSY_02);
            let ds = ValueShapeRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("应为数值"));
        assert!(ds[0].message.text.contains("E_TYPE_MISMATCH"));
        assert_eq!(ds[0].fix_availability, FixAvailability::None, "读不懂的值不能自动改");
    }

    #[test]
    fn qc_psy_02_fullwidth_digits_are_the_only_auto_fixable_case() {
        let mut s = spec("评估时间", None);
        s.value_type = ValueType::Date;
        let d = item(vec![field("评估时间", Some("２０２６-０３-０２"), 0)], sections());
        let eff = effective(QC_PSY_02);
            let ds = ValueShapeRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].fix_availability, FixAvailability::Automatic);
        assert!(ds[0].is_consistent());
        assert_eq!(ds[0].fix.as_ref().unwrap().replacement, "评估时间：2026-03-02");
    }

    #[test]
    fn qc_psy_02_stays_quiet_when_shape_is_fine() {
        let mut s = spec("约束时长", None);
        s.value_type = ValueType::Number;
        let d = item(vec![field("约束时长", Some("30 分钟"), 0)], sections());
        let eff = effective(QC_PSY_02);
        assert!(ValueShapeRule.check(&ctx_for(&d, &s, &eff)).is_empty());
    }

    #[test]
    fn qc_psy_02_rejects_month_day_order_without_guessing() {
        let mut s = spec("评估时间", None);
        s.value_type = ValueType::Date;
        let d = item(vec![field("评估时间", Some("03/02/2026"), 0)], sections());
        let eff = effective(QC_PSY_02);
            let ds = ValueShapeRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("E_DATE_PARSE"));
        assert_eq!(ds[0].fix_availability, FixAvailability::None);
    }

    // —— QC-PSY-03 ——

    #[test]
    fn qc_psy_03_flags_value_outside_declared_enum() {
        let mut s = spec("约束等级", None);
        s.allowed = vec!["一级保护".into(), "二级保护".into(), "三级保护".into()];
        let d = item(vec![field("约束等级", Some("四级保护"), 0)], sections());
        let eff = effective(QC_PSY_03);
            let ds = DomainRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("四级保护"));
        assert!(ds[0].message.text.contains("一级保护 / 二级保护 / 三级保护"));
        assert_eq!(ds[0].fix_availability, FixAvailability::None);
    }

    #[test]
    fn qc_psy_03_accepts_in_domain_value_with_trailing_period() {
        // 院内文书几乎每个值都带句号，带句号的合规取值不能被报成越界。
        let mut s = spec("约束等级", None);
        s.allowed = vec!["一级保护".into(), "二级保护".into()];
        for raw in ["二级保护", "二级保护。", " 二级保护 。"] {
            let d = item(vec![field("约束等级", Some(raw), 0)], sections());
            let eff = effective(QC_PSY_03);
            assert!(DomainRule.check(&ctx_for(&d, &s, &eff)).is_empty(), "{raw} 应判为在域内");
        }
    }

    #[test]
    fn qc_psy_03_numeric_bounds_are_inclusive_on_both_ends() {
        let mut s = spec("约束时长", None);
        s.value_type = ValueType::Number;
        s.min = Some(1.0);
        s.max = Some(720.0);
        for ok in ["1", "720", "360"] {
            let d = item(vec![field("约束时长", Some(ok), 0)], sections());
            let eff = effective(QC_PSY_03);
        assert!(DomainRule.check(&ctx_for(&d, &s, &eff)).is_empty(), "{ok} 应在区间内");
        }
        for bad in ["0", "721"] {
            let d = item(vec![field("约束时长", Some(bad), 0)], sections());
            let eff = effective(QC_PSY_03);
            let ds = DomainRule.check(&ctx_for(&d, &s, &eff));
            assert_eq!(ds.len(), 1, "{bad} 应越界");
            assert!(ds[0].message.text.contains("越出允许范围"));
        }
    }

    #[test]
    fn qc_psy_03_does_not_double_report_a_shape_problem() {
        // 值根本不是数值 → 归 QC-PSY-02，这里不重复报（同一处刷两行是典型失败模式）
        let mut s = spec("约束时长", None);
        s.value_type = ValueType::Number;
        s.min = Some(1.0);
        let d = item(vec![field("约束时长", Some("两小时"), 0)], sections());
        let eff = effective(QC_PSY_03);
        assert!(DomainRule.check(&ctx_for(&d, &s, &eff)).is_empty());
    }

    // —— QC-PSY-04 ——

    #[test]
    fn qc_psy_04_accepts_alias_unit_writing() {
        let mut s = spec("约束时长", None);
        s.unit = Some("分钟".into());
        let d = item(vec![field("约束时长", Some("30 min"), 0)], sections());
        let eff = effective(QC_PSY_04);
        assert!(UnitRule.check(&ctx_for(&d, &s, &eff)).is_empty());
    }

    #[test]
    fn qc_psy_04_flags_same_dimension_different_unit() {
        let mut s = spec("约束时长", None);
        s.unit = Some("分钟".into());
        let d = item(vec![field("约束时长", Some("2 小时"), 0)], sections());
        let eff = effective(QC_PSY_04);
            let ds = UnitRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("应按「分钟」计量"));
        assert!(ds[0].message.text.contains("2 小时"), "文案要带原值：{}", ds[0].message.text);
        assert_eq!(ds[0].fix_availability, FixAvailability::None, "换算要人确认，不自动改");
    }

    #[test]
    fn qc_psy_04_names_the_dimension_on_cross_dimension_mismatch() {
        let mut s = spec("约束时长", None);
        s.unit = Some("分钟".into());
        let d = item(vec![field("约束时长", Some("500 毫克"), 0)], sections());
        let eff = effective(QC_PSY_04);
            let ds = UnitRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("（时间）"), "{}", ds[0].message.text);
        assert!(ds[0].message.text.contains("（质量）"));
        assert!(ds[0].message.text.contains("500 毫克"), "{}", ds[0].message.text);
    }

    #[test]
    fn qc_psy_04_flags_missing_unit() {
        let mut s = spec("约束时长", None);
        s.unit = Some("分钟".into());
        let d = item(vec![field("约束时长", Some("30"), 0)], sections());
        let eff = effective(QC_PSY_04);
            let ds = UnitRule.check(&ctx_for(&d, &s, &eff));
        assert_eq!(ds.len(), 1);
        assert!(ds[0].message.text.contains("未注明单位"));
        assert!(ds[0].message.text.contains("填「30」"), "{}", ds[0].message.text);
    }

    #[test]
    fn qc_psy_04_skips_unknown_unit_in_the_form_definition() {
        // 表单上写了一个词表里没有的单位 → 不判错配（不知道该跟谁比）
        let mut s = spec("约束时长", None);
        s.unit = Some("华氏度".into());
        let d = item(vec![field("约束时长", Some("2 小时"), 0)], sections());
        let eff = effective(QC_PSY_04);
        assert!(UnitRule.check(&ctx_for(&d, &s, &eff)).is_empty());
    }

    // —— 注册表 / applies_to ——

    #[test]
    fn registry_exposes_the_four_field_rule_codes() {
        let codes: Vec<&str> = registry().iter().map(|r| r.code()).collect();
        assert_eq!(codes, vec![QC_PSY_01, QC_PSY_02, QC_PSY_03, QC_PSY_04]);
    }

    #[test]
    fn each_rule_declares_its_own_applicability() {
        let mut s = spec("k", None);
        let mut required = s.clone();
        required.required = true;
        assert!(RequiredFieldRule.applies(&required));
        assert!(!RequiredFieldRule.applies(&s));
        assert!(!ValueShapeRule.applies(&s));
        assert!(!DomainRule.applies(&s));
        assert!(!UnitRule.applies(&s));
        s.value_type = ValueType::Number;
        assert!(ValueShapeRule.applies(&s));
        s.allowed = vec!["x".into()];
        assert!(DomainRule.applies(&s));
        s.unit = Some("分钟".into());
        assert!(UnitRule.applies(&s));
    }

    #[test]
    fn trim_float_keeps_integers_clean() {
        assert_eq!(trim_float(30.0), "30");
        assert_eq!(trim_float(30.5), "30.5");
        assert_eq!(trim_float(30.125), "30.12", "展示值不制造原文没有的精度");
    }
}