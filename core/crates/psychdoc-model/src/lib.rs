//! `psychdoc-model`：精神科住院文书审查系统的核心模型层。
//!
//! 本层**不做 IO**，可独立 `cargo test`。承载四组类型：
//!
//! | 模块 | 内容 | 对应源产品主能力 |
//! |---|---|---|
//! | [`span`] | `Span` 半开字符区间 + `DocLoc(doc_id, section_index, char_range)` | `TextRange` 字节区间 → 改写为字符区间 |
//! | [`diagnostic`] | `Diagnostic{code, range, message}` + `FixAvailability` 三态 | 规则诊断三件套；**不可自动修是一等状态** |
//! | [`errcodes`] | 全量错误码表：触发条件 / 可否重试 / 处置建议 | 稳定编号 + 位置 + 「为什么这是问题」在排障侧的延伸 |
//! | [`doc_item`] | 件 → 字段表 → 时间戳集合三层结构 | 解析产物结构 |
//! | [`state`] | 件级状态机 + 批量推进的原子性 | 业务状态生命周期 |
//! | [`template`] | 院内文书类型字典 + 模板必填字段表 | 表单形状（与规则集分开版本化） |

pub mod diagnostic;
pub mod doc_item;
pub mod errcodes;
pub mod preview;
pub mod span;
pub mod state;
pub mod template;

pub use diagnostic::{
    Diagnostic, DiagnosticError, FixAvailability, FixPayload, RuleMessage, Severity,
};
pub use errcodes::{describe as describe_error_code, ErrorCode, Side, ERROR_CODES};
pub use doc_item::{
    Binding, DocItem, Field, FieldPresence, Precision, Section, Timestamp, TimestampKind,
};
pub use preview::{
    diff_lines, preview_fixback, render_skeleton, CandidateBlock, DiffLine, DiffOp, FixbackPreview,
};
pub use span::{DocLoc, Span, SpanError};
pub use state::{is_legal, StateBoard, StateError, StateTracker, Transition};
pub use template::{DocTypeRegistry, FieldSpec, TemplateSpec, ValueType};

/// 院内规范依据的占位样例；真实条款由 `rulesets/*.yaml` 随规则集版本化下发。
pub const EXAMPLE_BASIS: &str = "《精神科住院护理常规》第 4.2 条";
