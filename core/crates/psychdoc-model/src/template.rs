//! 院内文书类型字典与模板必填字段表。
//!
//! 这不是「规则」，而是**文书本身的形状**：`restraint_consent` 这类文书类型
//! 有哪些段落、每个段落必须有哪些字段、每个字段该填什么形状的值。
//! 解析层用它把段内 `键：值` 行回填成带来源区间的字段表；
//! 规则层用它判定「必填字段是否真的没填」「这个值形状对不对」。
//!
//! 与规则集（`psychdoc-rules`）分开建模的原因：模板改了不等于规则改了，
//! 两者在院内是两条不同的维护线（病案室改表单 vs 质控办改判定口径）。
//!
//! Task 6 起，字段形状不止「必填 / 非必填」，还带上
//! **值类型 / 取值域 / 数值区间 / 计量单位**四项声明——它们同样是**表单形状**
//! （纸质表单上就印着「风险等级：□低 □中 □高」「约束时长：___ 分钟」），
//! 不是判定口径，因此仍归本模块，规则层只读不改。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 字段应填的**值类型**。对应纸质表单上「日期」「时长（分钟）」这类栏目。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ValueType {
    /// 自由文本：不做形状检查（院内绝大多数字段是这一类）。
    #[default]
    #[serde(rename = "text")]
    Text,
    /// 数值（可带单位后缀，见 `FieldSpec::unit`）。
    #[serde(rename = "number")]
    Number,
    /// 整数。
    #[serde(rename = "integer")]
    Integer,
    /// 日期（`2026-03-02`）。
    #[serde(rename = "date")]
    Date,
    /// 日期 + 时刻（`2026-03-02 23:40`）。
    ///
    /// YAML 里写作 `datetime`（不是一个词，不拆成 `date_time`）：
    /// 规则集是病案室手写的，`datetime` 是表单上印的说法，
    /// 写成 `date_time` 反而会让维护的人以为有两种类型。
    #[serde(rename = "datetime", alias = "date_time")]
    DateTime,
}

impl ValueType {
    /// 中文名，进 message 文案（`应为日期时间，实际填「…」`）。
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Text => "文本",
            Self::Number => "数值",
            Self::Integer => "整数",
            Self::Date => "日期",
            Self::DateTime => "日期时间",
        }
    }
}

/// 一条字段的表单定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FieldSpec {
    /// 段内 `键：值` 的键（去除首尾空白后的原文）。
    pub key: String,
    /// 该字段应落在哪个段落标题；`None` 表示允许落在任意段落。
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub required: bool,
    /// 值形状；缺省 `Text`（不做形状检查）。
    #[serde(default)]
    pub value_type: ValueType,
    /// 取值域（枚举）：院内勾选项的合集，如 `["一级保护", "二级保护", "三级保护"]`。
    #[serde(default)]
    pub allowed: Vec<String>,
    /// 数值区间的下界 / 上界（闭区间）。仅在 `value_type` 为数值时生效。
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    /// 该字段按哪个单位计量（表单上印的单位），如「分钟」「毫克」。
    #[serde(default)]
    pub unit: Option<String>,
}

impl FieldSpec {
    /// 是否声明了取值域或数值区间——决定 `QC-PSY-03` 是否需要看这个字段。
    pub fn has_domain(&self) -> bool {
        !self.allowed.is_empty() || self.min.is_some() || self.max.is_some()
    }
}

/// 一种文书类型的模板。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TemplateSpec {
    #[serde(default)]
    pub display: Option<String>,
    #[serde(default)]
    pub fields: Vec<FieldSpec>,
}

impl TemplateSpec {
    pub fn field(&self, key: &str) -> Option<&FieldSpec> {
        self.fields.iter().find(|f| f.key == key)
    }

    /// 展示名，回退到类型码本身（不让 UI 出现空白标题）。
    pub fn display_or<'a>(&'a self, code: &'a str) -> &'a str {
        self.display.as_deref().unwrap_or(code)
    }
}

/// 院内文书类型字典：`doc_type 码 → 模板`。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DocTypeRegistry {
    #[serde(default)]
    pub templates: BTreeMap<String, TemplateSpec>,
}

impl DocTypeRegistry {
    pub fn template(&self, code: &str) -> Option<&TemplateSpec> {
        self.templates.get(code)
    }

    pub fn is_known(&self, code: &str) -> bool {
        self.templates.contains_key(code)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn registry() -> DocTypeRegistry {
        serde_yaml_min::parse(
            r#"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方意见
        section: 患方知情同意
        required: true
      - key: 患方签字
        section: 患方知情同意
        required: true
"#,
        )
    }

    mod serde_yaml_min {
        pub fn parse(s: &str) -> super::DocTypeRegistry {
            serde_yaml::from_str(s).expect("夹具 YAML 合法")
        }
    }

    #[test]
    fn registry_looks_up_template_and_field() {
        let r = registry();
        assert!(r.is_known("restraint_consent"));
        let t = r.template("restraint_consent").unwrap();
        assert_eq!(t.display_or("restraint_consent"), "约束知情同意");
        assert!(t.field("患方签字").unwrap().required);
        assert!(t.field("患方意见").unwrap().required);
    }

    #[test]
    fn unknown_doc_type_is_not_invented() {
        let r = registry();
        assert!(!r.is_known("restraint_release"));
        assert!(r.template("restraint_release").is_none());
    }

    #[test]
    fn display_falls_back_to_code_when_absent() {
        let t = TemplateSpec {
            display: None,
            fields: vec![],
        };
        assert_eq!(t.display_or("sign_page"), "sign_page");
    }

    #[test]
    fn omitted_shape_fields_default_to_text_without_domain() {
        // 老模板（只写 key/section/required）必须继续可用：形状检查默认不启用。
        let t = registry().template("restraint_consent").unwrap().clone();
        let f = t.field("患方签字").unwrap();
        assert_eq!(f.value_type, ValueType::Text);
        assert!(f.allowed.is_empty());
        assert!(f.min.is_none() && f.max.is_none());
        assert!(f.unit.is_none());
        assert!(!f.has_domain());
    }

    #[test]
    fn shape_fields_round_trip_through_yaml() {
        let r: DocTypeRegistry = serde_yaml_min::parse(
            r#"
templates:
  restraint_record:
    display: 约束记录
    fields:
      - key: 约束等级
        value_type: text
        allowed: ["一级保护", "二级保护", "三级保护"]
        required: true
      - key: 约束时长
        value_type: number
        min: 1
        max: 720
        unit: 分钟
        required: true
"#,
        );
        let t = r.template("restraint_record").unwrap();
        assert_eq!(t.field("约束等级").unwrap().allowed.len(), 3);
        assert!(t.field("约束等级").unwrap().has_domain());
        let d = t.field("约束时长").unwrap();
        assert_eq!(d.value_type, ValueType::Number);
        assert_eq!(d.min, Some(1.0));
        assert_eq!(d.max, Some(720.0));
        assert_eq!(d.unit.as_deref(), Some("分钟"));
        assert_eq!(d.value_type.as_zh(), "数值");
    }
}