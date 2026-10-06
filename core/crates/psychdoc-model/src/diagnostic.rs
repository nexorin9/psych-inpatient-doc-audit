//! 诊断三件套：`code` / `range` / `message`，外加 `FixAvailability` 分流。
//!
//! 源产品主能力对应：每条规则带稳定编号（`AIR304`），通过
//! `impl Violation::message()` 给出「为什么这是问题」，
//! `const FIX_AVAILABILITY: FixAvailability` 决定该条能否自动修。
//! 关键行为**保留**：不可自动修是一等状态（`FixAvailability::None`），
//! 此时只给依据、不给补写按钮，且不得携带任何 fix 载荷。
//!
//! 本项目新增：`RuleMessage` 强制携带院内规范条款依据——没有依据的
//! `message` 在构造期即被拒绝（`E_NO_RULE_BASIS`），避免「只报错不给依据」。

use crate::span::DocLoc;
use serde::{Deserialize, Serialize};
use std::fmt;

/// 是否可自动补写。对应源产品 `FixAvailability::{Always, Sometimes, None}`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FixAvailability {
    /// 总能自动补写
    Automatic,
    /// 视上下文而定（本项目用于「人工确认后可补写」）
    Manual,
    /// 不可自动补写：一等状态，只给依据不给按钮
    #[default]
    None,
}

impl FixAvailability {
    /// UI 契约：只有 `Automatic` 才渲染「预览补写」按钮。
    /// Task 33 用对照测试守住这一条，防止不可修条目被误当成可修。
    pub fn allows_fix_ui(&self) -> bool {
        matches!(self, Self::Automatic)
    }
}

impl fmt::Display for FixAvailability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Automatic => "automatic",
            Self::Manual => "manual",
            Self::None => "none",
        };
        write!(f, "{s}")
    }
}

/// 严重度：决定是否阻断提交。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    /// 阻断提交
    #[default]
    Blocking,
    /// 仅提示，不阻断
    Notice,
}

impl Severity {
    /// 全部档位。新增一档时这里必须同步，否则分层视图会漏掉它。
    pub const ALL: [Severity; 2] = [Severity::Blocking, Severity::Notice];

    /// 排序权重：越大越「重」。去重择优与严重度分层共用这一份口径。
    pub fn rank(self) -> u8 {
        match self {
            Severity::Blocking => 1,
            Severity::Notice => 0,
        }
    }

    pub fn max_of(a: Self, b: Self) -> Self {
        if a.rank() >= b.rank() {
            a
        } else {
            b
        }
    }

    /// **唯一**的协议字面量（JSON / Python 侧消费）。UI 与 CLI 都不许自己再写一份映射。
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Blocking => "blocking",
            Severity::Notice => "notice",
        }
    }

    /// **唯一**的中文展示词。清单、卡片、CLI 共用，避免同一个档位三处叫法不一样。
    pub fn as_zh(self) -> &'static str {
        match self {
            Severity::Blocking => "阻断",
            Severity::Notice => "提示",
        }
    }

    /// 是否阻断提交。写回闸门只认这一个函数。
    pub fn blocks_submission(self) -> bool {
        matches!(self, Severity::Blocking)
    }

    /// 解析严重度文案（审计导出、跨进程回读用）。
    ///
    /// 实现标准 [`FromStr`] 而不是自造一个同名固有方法：`from_str` 这个名字
    /// 一旦返回 `Option` 而不是 `Result`，就与标准 trait 的约定相左——
    /// 调用方 `s.parse()` 拿到的会是能编译但读不懂的类型。
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "blocking" => Some(Severity::Blocking),
            "notice" => Some(Severity::Notice),
            _ => None,
        }
    }
}

impl std::str::FromStr for Severity {
    type Err = UnknownSeverity;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Severity::parse(s).ok_or_else(|| UnknownSeverity(s.to_string()))
    }
}

/// 严重度文案认不出时的错误。单独一个类型而不是 `String`：
/// 这样上层能在 `match` 里穷举，而不必去比对错误文案。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("认不出严重度文案 {0}（只接受 blocking / notice）")]
pub struct UnknownSeverity(pub String);

/// 一条规则消息：**必须**携带院内规范条款依据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuleMessage {
    pub text: String,
    /// 院内规范条款依据，如「《精神科住院护理常规》第 4.2 条」。
    /// 为空即拒绝构造——没有依据的诊断没有意义。
    pub basis: String,
}

impl RuleMessage {
    pub fn new(
        text: impl Into<String>,
        basis: impl Into<String>,
    ) -> Result<Self, DiagnosticError> {
        let text = text.into();
        let basis = basis.into();
        if basis.trim().is_empty() {
            return Err(DiagnosticError::NoRuleBasis { text });
        }
        if text.trim().is_empty() {
            return Err(DiagnosticError::EmptyMessage { code_basis: basis });
        }
        Ok(Self { text, basis })
    }

    /// 完整呈现：`文案（依据：条款）`，供 UI 卡片底部固定依据行直接使用。
    pub fn render(&self) -> String {
        format!("{}（依据：{}）", self.text, self.basis)
    }
}

impl fmt::Display for RuleMessage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.render())
    }
}

/// 诊断：码 + 位置 + 为什么是问题 + 依据 + 可否自动修。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Diagnostic {
    /// 稳定编号 `QC-PSY-nn`
    pub code: String,
    pub loc: DocLoc,
    pub message: RuleMessage,
    pub fix_availability: FixAvailability,
    pub severity: Severity,
    /// 可自动补写时的补写载荷；`fix_availability != Automatic` 时**必须**为 `None`。
    pub fix: Option<FixPayload>,
    /// 被更具体规则抑制时记录来源码（Task 23 填充）
    pub suppressed_by: Option<String>,
    /// 规则被豁免时记录理由（Task 18 填充）
    pub waived: Option<String>,
}

/// 自动补写载荷。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FixPayload {
    /// 补写后的整段文本（骨架模板渲染结果）
    pub replacement: String,
    /// 预计新建件数 / 修改段数，供 diff 底部汇总
    pub summary: String,
}

impl Diagnostic {
    pub fn new(code: impl Into<String>, loc: DocLoc, message: RuleMessage) -> Self {
        Self {
            code: code.into(),
            loc,
            message,
            fix_availability: FixAvailability::None,
            severity: Severity::Blocking,
            fix: None,
            suppressed_by: None,
            waived: None,
        }
    }

    /// 标记为不可自动修（一等状态）。构造时即断言不携带 fix 载荷。
    pub fn without_fix(mut self) -> Self {
        self.fix_availability = FixAvailability::None;
        self.fix = None;
        self
    }

    pub fn with_fix(mut self, payload: FixPayload) -> Self {
        self.fix_availability = FixAvailability::Automatic;
        self.fix = Some(payload);
        self
    }

    pub fn with_severity(mut self, severity: Severity) -> Self {
        self.severity = severity;
        self
    }

    /// 自洽性检查：不可修却带载荷 → 视为违反契约，调用方须在测试中断言。
    pub fn is_consistent(&self) -> bool {
        self.fix.is_some() == self.fix_availability.allows_fix_ui()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiagnosticError {
    #[error("E_NO_RULE_BASIS: 规则消息缺少院内规范条款依据 — {text}")]
    NoRuleBasis { text: String },
    #[error("E_EMPTY_MESSAGE: 规则消息正文为空（依据：{code_basis}）")]
    EmptyMessage { code_basis: String },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::span::Span;

    fn loc() -> DocLoc {
        DocLoc::new("WARD-A-001", 0, Span::unchecked(0, 4))
    }

    fn basis() -> RuleMessage {
        RuleMessage::new("缺少签署齐备性", "《精神科住院护理常规》第 4.2 条").unwrap()
    }

    #[test]
    fn fix_availability_none_produces_no_fix_payload() {
        let d = Diagnostic::new("QC-PSY-08", loc(), basis()).without_fix();
        assert_eq!(d.fix_availability, FixAvailability::None);
        assert!(d.fix.is_none());
        assert!(!d.fix_availability.allows_fix_ui());
        assert!(d.is_consistent());
    }

    #[test]
    fn automatic_fix_carries_payload_and_enables_ui() {
        let d = Diagnostic::new("QC-PSY-01", loc(), basis()).with_fix(FixPayload {
            replacement: "约束起始时间：<EXAMPLE_TIME>".into(),
            summary: "将修改 1 段".into(),
        });
        assert_eq!(d.fix_availability, FixAvailability::Automatic);
        assert!(d.fix.is_some());
        assert!(d.fix_availability.allows_fix_ui());
        assert!(d.is_consistent());
    }

    #[test]
    fn manual_fix_availability_still_offers_no_button() {
        // Manual = 人工确认后可补写，UI 仍不自动渲染补写按钮
        let mut d = Diagnostic::new("QC-PSY-05", loc(), basis());
        d.fix_availability = FixAvailability::Manual;
        assert!(!d.fix_availability.allows_fix_ui());
    }

    #[test]
    fn message_requires_rule_basis() {
        let err = RuleMessage::new("时间序倒挂", "  ").unwrap_err();
        assert!(matches!(err, DiagnosticError::NoRuleBasis { .. }));
        assert!(err.to_string().contains("E_NO_RULE_BASIS"));
    }

    #[test]
    fn message_rejects_empty_text() {
        let err = RuleMessage::new("  ", "《规范》第 1 条").unwrap_err();
        assert!(matches!(err, DiagnosticError::EmptyMessage { .. }));
    }

    #[test]
    fn message_render_includes_basis() {
        let m = basis();
        assert_eq!(m.render(), "缺少签署齐备性（依据：《精神科住院护理常规》第 4.2 条）");
    }

    #[test]
    fn diagnostic_serializes_all_three_legs() {
        let d = Diagnostic::new("QC-PSY-01", loc(), basis());
        let json = serde_json::to_string(&d).unwrap();
        assert!(json.contains("QC-PSY-01"));
        assert!(json.contains("WARD-A-001"));
        assert!(json.contains("第 4.2 条"));
    }
}
