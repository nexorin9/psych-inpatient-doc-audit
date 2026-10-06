//! 位置模型：`(doc_id, section_index, char_range)`。
//!
//! 源产品主能力对应：`ruff_python_parser::parse()` 失败时返回
//! `ParseError { error, location: TextRange }`——**解析失败也带着位置**，
//! 绝不静默丢字段。本项目把字节区间语义改写为字符区间 `Span`，
//! 并在其外层再包一层 `DocLoc`（件 id + 段序号），使一条诊断能被
//! 文书卡片直接定位到具体段落。
//!
//! 口径约定：全文使用 **字符（Unicode scalar）下标**，不是字节下标，
//! 保证生僻字与全角字符的 `char_range` 与卡片高亮一致（见 task 17）。

use serde::{Deserialize, Serialize};
use std::fmt;

/// 半开区间 `[start, end)`，与源产品 `TextRange::new(start, end)` 同语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

/// `Span` 构造与越界错误码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SpanError {
    #[error("E_SPAN_REVERSED: 区间起点 {start} 大于终点 {end}")]
    Reversed { start: u32, end: u32 },
    #[error("E_SPAN_OUT_OF_BOUNDS: 区间 [{start}, {end}) 越出文本长度 {len}")]
    OutOfBounds { start: u32, end: u32, len: u32 },
}

impl Span {
    pub fn new(start: u32, end: u32) -> Result<Self, SpanError> {
        if start > end {
            return Err(SpanError::Reversed { start, end });
        }
        Ok(Self { start, end })
    }

    /// 不校验越界的便捷构造（仅用于测试内部已知合法的场景）。
    pub fn unchecked(start: u32, end: u32) -> Self {
        Self { start, end }
    }

    /// 对 `text` 校验区间合法；越界或倒置均拒绝。
    pub fn checked(text: &str, start: u32, end: u32) -> Result<Self, SpanError> {
        let span = Self::new(start, end)?;
        let len = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
        if end > len {
            return Err(SpanError::OutOfBounds { start, end, len });
        }
        Ok(span)
    }

    pub fn len(&self) -> u32 {
        self.end.saturating_sub(self.start)
    }

    pub fn is_empty(&self) -> bool {
        self.start == self.end
    }

    /// 按字符切出区间对应的子串；越界返回 `None`，**不静默截断**
    /// （截断会让文书卡片高亮到错误的区间，比报错更难发现）。
    pub fn slice<'a>(&self, text: &'a str) -> Option<&'a str> {
        let len = u32::try_from(text.chars().count()).unwrap_or(u32::MAX);
        if self.end > len {
            return None;
        }
        let start_byte = text.char_indices().nth(self.start as usize).map_or(text.len(), |(b, _)| b);
        let rest = &text[start_byte..];
        // 终点必须真实存在：char_indices().nth(len) 为 None 说明越界，
        // 除非该区间正好覆盖到文本末尾。
        match rest.char_indices().nth(self.len() as usize) {
            Some((end_byte, _)) => Some(&rest[..end_byte]),
            None if rest.chars().count() == self.len() as usize => Some(rest),
            None => None,
        }
    }
}

impl fmt::Display for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[{}, {})", self.start, self.end)
    }
}

/// 一条诊断在整份文书包里的位置：`件 id + 段序号 + 段内字符区间`。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct DocLoc {
    pub doc_id: String,
    pub section_index: u32,
    pub char_range: Span,
}

impl DocLoc {
    pub fn new(doc_id: impl Into<String>, section_index: u32, char_range: Span) -> Self {
        Self {
            doc_id: doc_id.into(),
            section_index,
            char_range,
        }
    }
}

impl fmt::Display for DocLoc {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}#s{}@{}",
            self.doc_id, self.section_index, self.char_range
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_construction_and_len() {
        let s = Span::new(3, 9).unwrap();
        assert_eq!(s.len(), 6);
        assert!(!s.is_empty());
        assert_eq!(s.to_string(), "[3, 9)");
    }

    #[test]
    fn reversed_span_rejected() {
        let err = Span::new(9, 3).unwrap_err();
        assert!(matches!(err, SpanError::Reversed { start: 9, end: 3 }));
        assert!(err.to_string().contains("E_SPAN_REVERSED"));
    }

    #[test]
    fn out_of_bounds_rejected() {
        let text = "约束知情同意";
        // 6 个字符，区间终点 7 越界
        let err = Span::checked(text, 0, 7).unwrap_err();
        assert!(matches!(err, SpanError::OutOfBounds { len: 6, .. }));
        assert!(err.to_string().contains("E_SPAN_OUT_OF_BOUNDS"));
        assert!(Span::checked(text, 0, 6).is_ok());
    }

    #[test]
    fn slice_uses_char_not_byte_offsets() {
        // 生僻字「㊙」在 UTF-8 占 3 字节；字符下标 1..2 应只切出它本身
        let text = "甲㊙乙";
        let s = Span::checked(text, 1, 2).unwrap();
        assert_eq!(s.slice(text), Some("㊙"));
    }

    #[test]
    fn slice_out_of_bounds_returns_none() {
        let text = "abc";
        let s = Span::unchecked(1, 9);
        assert_eq!(s.slice(text), None);
    }

    #[test]
    fn slice_covers_span_ending_at_text_end() {
        let text = "约束起始 23:40";
        let s = Span::checked(text, 0, u32::try_from(text.chars().count()).unwrap()).unwrap();
        assert_eq!(s.slice(text), Some(text));
    }

    #[test]
    fn slice_of_zero_width_span_at_text_end_is_empty_not_none() {
        // 回归：段落最后一行是「患方签字：」这种空值时，区间是**文本末尾的零宽区间**。
        // 早前实现用 `char_indices().nth(start)?`，start 落在末尾时直接 None，
        // 导致「有键无值」这类最常见的命中在卡片上取不到子串。零宽区间应切出空串。
        let text = "患方签字：";
        let s = Span::checked(text, 5, 5).unwrap();
        assert_eq!(s.slice(text), Some(""));
    }

    #[test]
    fn slice_still_rejects_genuinely_out_of_bounds() {
        // 越界仍须报 None，不能因为上面的修正被放行
        assert_eq!(Span::unchecked(0, 99).slice("abc"), None);
    }

    #[test]
    fn docloc_display_is_parseable() {
        let loc = DocLoc::new("WARD-A-001", 2, Span::unchecked(4, 8));
        assert_eq!(loc.to_string(), "WARD-A-001#s2@[4, 8)");
    }
}
