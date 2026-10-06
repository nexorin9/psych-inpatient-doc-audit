//! 文书三层结构：**件 → 字段表 → 时间戳集合**。
//!
//! 对应 `spec.md` §2：一件 `Composition` 解析出段落下标、字段表
//! （键 / 值 / 必填性 / 来源段落区间）与三类时间戳
//! （`authoredOn` / `date` / 段落内业务时刻）。
//!
//! 实体绑定纪律：件按 `template_id` + `Composition.id` 绑定，
//! **禁止按文件名配对**；`binding` 非 `Confident` 时上层必须转人工。

use crate::span::Span;
use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

/// 实体绑定结果。对应源产品 `resolver::resolve_module_confident`：
/// 只有 confident 才允许继续，ambiguous 必须显式交人工，**绝不静默配对**。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Binding {
    /// 按 template_id 唯一确定
    #[default]
    Confident,
    /// 跨系统同名文书，绑定不唯一 → 人工队列
    Ambiguous,
    /// 未找到对应模板 → 人工队列
    Unresolved,
}

impl Binding {
    pub fn is_confident(&self) -> bool {
        matches!(self, Self::Confident)
    }
}

/// 段落下标与正文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Section {
    pub index: u32,
    pub title: String,
    pub text: String,
}

impl Section {
    /// 段内字符区间换算为全局定位。
    pub fn locate(&self, doc_id: &str, char_range: Span) -> crate::span::DocLoc {
        crate::span::DocLoc::new(doc_id, self.index, char_range)
    }
}

/// 字段表的一行：键 / 值 / 必填性 / 来源段落区间。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Field {
    pub key: String,
    /// `None` 表示字段在结构中显式为 null；与空串、缺失三者语义不同（Task 17）。
    pub value: Option<String>,
    pub required: bool,
    /// 值在源段内的字符区间；`None` 表示字段根本没出现（缺失）
    pub source_span: Option<Span>,
    pub section_index: u32,
}

impl Field {
    /// 三态：缺失（无 source_span）/ null / 空串 —— **不得**合并为「等于空」。
    pub fn presence(&self) -> FieldPresence {
        match &self.value {
            // 键在结构中不存在：值与来源区间都没有
            None if self.source_span.is_none() => FieldPresence::Missing,
            // 键存在但显式为 null
            None => FieldPresence::Null,
            Some(v) if v.is_empty() => FieldPresence::Empty,
            Some(_) => FieldPresence::Present,
        }
    }

    /// 必填字段是否真的没填（null / 空串 / 缺失都算没填）。
    pub fn is_unfilled(&self) -> bool {
        self.required && self.presence() != FieldPresence::Present
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FieldPresence {
    /// 键在结构中不存在
    Missing,
    /// 键存在但值为 null
    Null,
    /// 键存在但值为空串
    Empty,
    /// 键存在且有值
    Present,
}

/// 时间戳来源类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TimestampKind {
    /// 文档系统记录的书写时刻
    AuthoredOn,
    /// 业务生效日期
    Date,
    /// 段落内抽取的业务时刻
    BusinessMoment,
}

/// 时间戳精度：低精度只能参与「不早于」单向判断（Task 19）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum Precision {
    #[default]
    Second,
    Minute,
    Hour,
    Day,
}

impl Precision {
    pub fn is_coarser_than(&self, other: Precision) -> bool {
        self.rank() > other.rank()
    }

    fn rank(&self) -> u8 {
        match self {
            Self::Second => 0,
            Self::Minute => 1,
            Self::Hour => 2,
            Self::Day => 3,
        }
    }
}

/// 一条时间戳。`moment` 为 `None` 表示**抽不出**——
/// 绝不用当前时间或零值兜底（否则会制造假的时间序结论）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Timestamp {
    pub kind: TimestampKind,
    pub moment: Option<DateTime<FixedOffset>>,
    pub precision: Precision,
    pub section_index: u32,
    pub char_range: Option<Span>,
    /// 段内原文时刻（未规范化前的字面量）。
    /// 时刻文本常常**没有日期**（只写「23时40分」），此时 `moment` 为 `None`
    /// 而 `raw_text` 仍在——文案要能告诉护士长「这段里写了 23:40，但没有日期，
    /// 所以时间序这条规则跳过了」，而不是让信息凭空消失。
    #[serde(default)]
    pub raw_text: Option<String>,
}

impl Timestamp {
    pub fn moment(&self) -> Option<DateTime<FixedOffset>> {
        self.moment
    }

    pub fn is_missing(&self) -> bool {
        self.moment.is_none()
    }

    /// 有时刻的时间戳。
    pub fn at(
        kind: TimestampKind,
        moment: DateTime<FixedOffset>,
        precision: Precision,
        section_index: u32,
        char_range: Option<Span>,
    ) -> Self {
        Self {
            kind,
            moment: Some(moment),
            precision,
            section_index,
            char_range,
            raw_text: None,
        }
    }

    /// 抽不出时刻的占位：**`moment` 必须为 `None`**，
    /// 绝不填当前时间或零值——那会制造假的时间序结论。
    pub fn missing(kind: TimestampKind, section_index: u32, raw_text: Option<String>) -> Self {
        Self {
            kind,
            moment: None,
            precision: Precision::Second,
            section_index,
            char_range: None,
            raw_text,
        }
    }
}

/// 一份文书件的三层结构。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DocItem {
    /// 权威实体键：Composition.id 的脱敏别名
    pub doc_id: String,
    pub template_id: String,
    /// 住院别名（脱敏），跨件 join 的键
    pub encounter_alias: String,
    /// 权威系统（脱敏），从 `identifier.system` 的主机段抽取。
    /// 院内同一份文书可能由 HIS、EMR、护理系统各导一份：**跨系统的同名文书
    /// 绑定不唯一**，必须标 `Ambiguous` 交人工（见 `EntityBinder`）。
    pub source_system: String,
    pub binding: Binding,
    pub sections: Vec<Section>,
    pub fields: Vec<Field>,
    pub timestamps: Vec<Timestamp>,
}

impl DocItem {
    pub fn new(
        doc_id: impl Into<String>,
        template_id: impl Into<String>,
        encounter_alias: impl Into<String>,
    ) -> Self {
        Self {
            doc_id: doc_id.into(),
            template_id: template_id.into(),
            encounter_alias: encounter_alias.into(),
            source_system: String::new(),
            binding: Binding::Confident,
            sections: Vec::new(),
            fields: Vec::new(),
            timestamps: Vec::new(),
        }
    }

    pub fn with_source_system(mut self, system: impl Into<String>) -> Self {
        self.source_system = system.into();
        self
    }

    pub fn with_sections(mut self, sections: Vec<Section>) -> Self {
        self.sections = sections;
        self
    }

    pub fn with_fields(mut self, fields: Vec<Field>) -> Self {
        self.fields = fields;
        self
    }

    pub fn with_timestamps(mut self, timestamps: Vec<Timestamp>) -> Self {
        self.timestamps = timestamps;
        self
    }

    pub fn with_binding(mut self, binding: Binding) -> Self {
        self.binding = binding;
        self
    }

    pub fn field(&self, key: &str) -> Option<&Field> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// 该件未填的必填字段（Task 6 的 QC-PSY-01 依据）。
    pub fn unfilled_required(&self) -> Vec<&Field> {
        self.fields.iter().filter(|f| f.is_unfilled()).collect()
    }

    /// 抽不出时刻的条目：交人工时必须显式列出，不用兜底值掩盖。
    pub fn missing_timestamps(&self) -> Vec<&Timestamp> {
        self.timestamps.iter().filter(|t| t.is_missing()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Timelike};

    fn field(key: &str, value: Option<&str>, required: bool, span: Option<Span>) -> Field {
        Field {
            key: key.into(),
            value: value.map(str::to_string),
            required,
            source_span: span,
            section_index: 0,
        }
    }

    fn ts(minute: u32) -> Timestamp {
        Timestamp::at(
            TimestampKind::BusinessMoment,
            chrono::FixedOffset::east_opt(0)
                .unwrap()
                .timestamp_opt(2026, 3)
                .unwrap()
                .with_minute(minute)
                .unwrap(),
            Precision::Minute,
            0,
            None,
        )
    }

    #[test]
    fn field_presence_distinguishes_three_states() {
        assert_eq!(field("k", Some("值"), true, Some(Span::unchecked(0, 1))).presence(), FieldPresence::Present);
        assert_eq!(field("k", Some(""), true, Some(Span::unchecked(0, 0))).presence(), FieldPresence::Empty);
        assert_eq!(field("k", None, true, Some(Span::unchecked(0, 0))).presence(), FieldPresence::Null);
        assert_eq!(field("k", None, false, None).presence(), FieldPresence::Missing);
    }

    #[test]
    fn unfilled_required_collects_empty_and_null() {
        let doc = DocItem::new("D-1", "TPL-CONSTRAINT", "WARD-A-001").with_fields(vec![
            field("约束起始时间", Some(""), true, Some(Span::unchecked(0, 0))),
            field("解除时间", None, true, Some(Span::unchecked(0, 0))),
            field("执行护士", Some("脱敏-护士A"), true, Some(Span::unchecked(2, 6))),
            field("备注", None, false, None),
        ]);
        let unfilled = doc.unfilled_required();
        assert_eq!(unfilled.len(), 2);
        assert!(unfilled.iter().all(|f| f.required));
    }

    #[test]
    fn missing_timestamp_is_not_filled_with_zero_value() {
        let mut missing = ts(10);
        missing.moment = None;
        let doc = DocItem::new("D-1", "TPL", "E-1").with_timestamps(vec![ts(10), missing]);
        assert_eq!(doc.timestamps.len(), 2);
        assert_eq!(doc.missing_timestamps().len(), 1);
        assert!(doc.missing_timestamps()[0].moment.is_none());
    }

    #[test]
    fn ambiguous_binding_is_not_confident() {
        let doc = DocItem::new("D-1", "TPL-CONSTRAINT", "E-1").with_binding(Binding::Ambiguous);
        assert!(!doc.binding.is_confident());
    }

    #[test]
    fn section_locate_produces_docloc() {
        let sec = Section { index: 3, title: "约束经过".into(), text: "约束于当日23时40分开始".into() };
        let loc = sec.locate("D-1", Span::unchecked(0, 3));
        assert_eq!(loc.doc_id, "D-1");
        assert_eq!(loc.section_index, 3);
    }

    #[test]
    fn precision_ordering() {
        assert!(Precision::Hour.is_coarser_than(Precision::Minute));
        assert!(!Precision::Second.is_coarser_than(Precision::Second));
    }
}
