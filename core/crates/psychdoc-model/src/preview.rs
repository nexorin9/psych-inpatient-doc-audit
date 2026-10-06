//! 补写预览：**骨架模板渲染 → 与原文双栏对照 → 可勾选候选块**。
//!
//! 业务现场对应 `spec.md` 人工确认点「写回先落本地草稿 → 逐条勾选 →
//! 一次性发 transaction」的前半段：护士长在真正写回之前，必须先看清
//! 「工具打算往这份文书里加哪几行」。所以本模块**只生成候选块**，
//! 不产生任何已生效事实——Task 33 负责把候选块渲染成双栏 diff 界面，
//! 本层保证候选块本身逐段可定位、可回溯到触发它的规则码与依据。
//!
//! ## 为什么骨架来自模板字典而不是硬编码文案
//!
//! 约束知情同意的段落与必填栏位印在**院内表单**上，来源是
//! [`crate::template::DocTypeRegistry`]（病案室维护）。本模块只按字典渲染
//! `键：<待填写>` 的空骨架，**不替护士长填内容**：填什么值是人的判断，
//! 工具只负责把「该有哪些栏位」摆出来。
//!
//! ## 不可自动修是一等状态（对照保留）
//!
//! 候选块只从**携带 fix 载荷**的诊断生成。`FixAvailability::None` 的诊断
//! （跨文书时间序倒挂、签署齐备性缺件等）**不产生候选块**——它只带依据，
//! 界面上也不该出现补写入口。UI 侧由 Task 33 用对照测试守住同一行为。

use crate::doc_item::DocItem;
use crate::diagnostic::{Diagnostic, FixAvailability, FixPayload};
use crate::template::DocTypeRegistry;

/// 一行 diff。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffOp {
    /// 原文与骨架都有
    Same,
    /// 骨架新增（补写预览里只可能出现这一类 `Added`）
    Added,
    /// 原文有、骨架没有（本预览不做删改，一律显式标出）
    Removed,
}

/// 一行对照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    pub op: DiffOp,
    pub text: String,
}

impl DiffLine {
    fn same(text: impl Into<String>) -> Self {
        Self {
            op: DiffOp::Same,
            text: text.into(),
        }
    }
    fn added(text: impl Into<String>) -> Self {
        Self {
            op: DiffOp::Added,
            text: text.into(),
        }
    }
    fn removed(text: impl Into<String>) -> Self {
        Self {
            op: DiffOp::Removed,
            text: text.into(),
        }
    }
}

/// 逐行 LCS 对照。规模是文书段落（几十行），O(n·m) 完全够用，
/// 且**保留原文行序**，让「哪一行是新增的」在界面上可逐行核对。
pub fn diff_lines(original: &[String], skeleton: &[String]) -> Vec<DiffLine> {
    let n = original.len();
    let m = skeleton.len();
    // lcs[i][j] = original[i..] 与 skeleton[j..] 的最长公共子序列长度
    let mut lcs = vec![vec![0usize; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if original[i] == skeleton[j] {
                lcs[i + 1][j + 1] + 1
            } else {
                lcs[i + 1][j].max(lcs[i][j + 1])
            };
        }
    }
    let mut out = Vec::with_capacity(n + m);
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if original[i] == skeleton[j] {
            out.push(DiffLine::same(original[i].clone()));
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            out.push(DiffLine::removed(original[i].clone()));
            i += 1;
        } else {
            out.push(DiffLine::added(skeleton[j].clone()));
            j += 1;
        }
    }
    while i < n {
        out.push(DiffLine::removed(original[i].clone()));
        i += 1;
    }
    while j < m {
        out.push(DiffLine::added(skeleton[j].clone()));
        j += 1;
    }
    out
}

/// 骨架里未填写栏位的占位符。**刻意不是空白**：界面上要能一眼看出
/// 「这一栏还没填」，而不是让护士长以为工具已经替她填好了。
pub const BLANK: &str = "<待填写>";

/// 一个可逐段勾选的补写候选块。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateBlock {
    pub doc_id: String,
    /// 目标段落下标；新建件（原文无此段）为 `None`
    pub section_index: Option<u32>,
    pub section_title: String,
    /// 该段原文（可能为空串 = 原文这一段没内容）
    pub original: String,
    /// 骨架渲染结果
    pub skeleton: String,
    /// 逐行对照
    pub diff: Vec<DiffLine>,
    /// 触发本候选块的规则码
    pub code: String,
    /// 院内规范依据
    pub basis: String,
    pub fix_availability: FixAvailability,
    /// 新增行数，供底部「将新建 X 件 / 修改 Y 段」汇总
    pub added_lines: usize,
}

impl CandidateBlock {
    /// 该块是否真的新增了内容。零新增的候选块不该被勾选。
    pub fn has_additions(&self) -> bool {
        self.added_lines > 0
    }

    /// 勾选后落进 transaction 的实际文本：骨架全文。
    ///
    /// **不做字段级合并**：整段以骨架为准写回。理由是院内表单按段落
    /// 签署，部分覆盖的段落会让「签过字的段」与实际写回内容对不上。
    pub fn replacement(&self) -> String {
        self.skeleton.clone()
    }

    pub fn to_fix_payload(&self) -> FixPayload {
        FixPayload {
            replacement: self.replacement(),
            summary: format!(
                "{} 段新增 {} 行 · 依据 {}",
                self.section_title, self.added_lines, self.basis
            ),
        }
    }
}

/// 一份文书包的补写预览。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FixbackPreview {
    /// 可逐段勾选的候选块
    pub blocks: Vec<CandidateBlock>,
    /// 因不可自动修而**只给依据不给候选块**的诊断码（界面须显式说明原因）
    pub evidence_only: Vec<String>,
}

impl FixbackPreview {
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// 底部汇总行：`将新建 N 件 / 修改 M 段`。
    pub fn summary(&self) -> String {
        let new_sections = self
            .blocks
            .iter()
            .filter(|b| b.section_index.is_none())
            .count();
        let edited = self.blocks.len() - new_sections;
        let mut parts = Vec::new();
        if new_sections > 0 {
            parts.push(format!("新建 {new_sections} 段"));
        }
        if edited > 0 {
            parts.push(format!("修改 {edited} 段"));
        }
        if parts.is_empty() {
            return "无可补写内容".to_string();
        }
        parts.join(" / ")
    }
}

/// 按模板字典渲染骨架：`【段落】` + 每个栏位一行 `键：<待填写>`。
///
/// 已存在于原文中的栏位按原值渲染，这样 diff 里只有**真缺的**那几行
/// 标 `Added`，护士长不会看到一屏全是新增。
pub fn render_skeleton(
    registry: &DocTypeRegistry,
    template_id: &str,
    item: Option<&DocItem>,
) -> Vec<(String, String)> {
    let Some(tpl) = registry.template(template_id) else {
        return Vec::new();
    };
    let mut out: Vec<(String, String)> = Vec::new();
    for field in &tpl.fields {
        let title = field.section.clone().unwrap_or_else(|| "其他信息".to_string());
        let filled = item
            .and_then(|it| it.field(&field.key))
            .and_then(|f| f.value.clone())
            .filter(|v| !v.trim().is_empty());
        let value = filled.unwrap_or_else(|| BLANK.to_string());
        let line = format!("{}：{}", field.key, value);
        match out.last_mut() {
            Some((t, body)) if *t == title => {
                body.push('\n');
                body.push_str(&line);
            }
            _ => out.push((title, line)),
        }
    }
    out
}

/// 生成补写预览：按诊断逐条找到目标段，渲染骨架并做双栏对照。
///
/// 候选块来源规则（**与 UI 的补写按钮契约同源**）：
///
/// - 诊断带 `FixPayload` 且 `fix_availability == Automatic` → 生成候选块；
/// - `fix_availability` 为 `Manual` / `None` → **不生成候选块**，
///   其码记入 `evidence_only`，让界面能写清「这条只给依据」。
pub fn preview_fixback(
    item: &DocItem,
    registry: &DocTypeRegistry,
    diagnostics: &[Diagnostic],
) -> FixbackPreview {
    let mut out = FixbackPreview::default();
    for d in diagnostics {
        if d.loc.doc_id != item.doc_id {
            continue;
        }
        match (&d.fix, d.fix_availability) {
            (Some(_), FixAvailability::Automatic) => {}
            _ => {
                if !out.evidence_only.iter().any(|c| c == &d.code) {
                    out.evidence_only.push(d.code.clone());
                }
                continue;
            }
        }
        let section = item.sections.iter().find(|s| s.index == d.loc.section_index);
        let (title, original) = match section {
            Some(s) => (s.title.clone(), s.text.clone()),
            None => ("新建段落".to_string(), String::new()),
        };
        let skeleton = skeleton_for_section(registry, item.template_id.as_str(), &title);
        let original_lines = split_lines(&original);
        let skeleton_lines = split_lines(&skeleton);
        let diff = diff_lines(&original_lines, &skeleton_lines);
        let added_lines = diff.iter().filter(|l| l.op == DiffOp::Added).count();
        out.blocks.push(CandidateBlock {
            doc_id: item.doc_id.clone(),
            section_index: section.map(|s| s.index),
            section_title: title,
            original,
            skeleton,
            diff,
            code: d.code.clone(),
            basis: d.message.basis.clone(),
            fix_availability: d.fix_availability,
            added_lines,
        });
    }
    out
}

/// 渲染指定段落标题下的骨架正文（找不到该段时返回空串）。
fn skeleton_for_section(
    registry: &DocTypeRegistry,
    template_id: &str,
    title: &str,
) -> String {
    render_skeleton(registry, template_id, None)
        .into_iter()
        .find(|(t, _)| t == title)
        .map(|(_, body)| body)
        .unwrap_or_default()
}

fn split_lines(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(str::to_string).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diagnostic::RuleMessage;
    use crate::doc_item::Field;
    use crate::span::{DocLoc, Span};

    fn registry() -> DocTypeRegistry {
        serde_yaml::from_str(
            r#"
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
      - key: 患方签字
        section: 患方知情同意
        required: true
"#,
        )
        .unwrap()
    }

    fn item() -> DocItem {
        DocItem::new("WARD-A-001-CONSENT-01", "restraint_consent", "WARD-A-001")
            .with_sections(vec![crate::doc_item::Section {
                index: 0,
                title: "患方知情同意".into(),
                text: "患方意见：已知晓约束目的与风险".into(),
            }])
            .with_fields(vec![Field {
                key: "患方意见".into(),
                value: Some("已知晓约束目的与风险".into()),
                required: true,
                source_span: Some(Span::new(0, 13).unwrap()),
                section_index: 0,
            }])
    }

    fn loc(section_index: u32) -> DocLoc {
        DocLoc::new("WARD-A-001-CONSENT-01", section_index, Span::new(0, 0).unwrap())
    }

    fn diag(code: &str, section_index: u32, fix: Option<FixPayload>) -> Diagnostic {
        let d = Diagnostic::new(
            code,
            loc(section_index),
            RuleMessage::new(format!("{code} 命中"), "《精神科住院护理常规》第 4.2 条").unwrap(),
        );
        match fix {
            Some(p) => d.with_fix(p),
            None => d.without_fix(),
        }
    }

    fn payload(replacement: &str) -> FixPayload {
        FixPayload {
            replacement: replacement.into(),
            summary: "将修改 1 段".into(),
        }
    }

    #[test]
    fn skeleton_marks_unfilled_slots_and_keeps_filled_values() {
        let it = item();
        let sk = render_skeleton(&registry(), "restraint_consent", Some(&it));
        let body = sk
            .iter()
            .find(|(t, _)| t == "患方知情同意")
            .map(|(_, b)| b.clone())
            .unwrap();
        assert!(body.contains("患方意见：已知晓约束目的与风险"), "已填的照原样渲染");
        assert!(body.contains("患方签字：<待填写>"), "未填的显式占位");
        assert!(!body.contains("患方意见：<待填写>"), "不该把已填值又标成待填写");
    }

    #[test]
    fn skeleton_sections_follow_the_template_order() {
        let sk = render_skeleton(&registry(), "restraint_consent", None);
        assert_eq!(sk.len(), 2);
        assert_eq!(sk[0].0, "约束事由");
        assert_eq!(sk[1].0, "患方知情同意");
    }

    #[test]
    fn diff_marks_only_the_genuinely_missing_line_as_added() {
        let original = vec!["患方意见：已知晓约束目的与风险".to_string()];
        let skeleton = vec![
            "患方意见：已知晓约束目的与风险".to_string(),
            "患方签字：<待填写>".to_string(),
        ];
        let d = diff_lines(&original, &skeleton);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0].op, DiffOp::Same);
        assert_eq!(d[1].op, DiffOp::Added);
        assert_eq!(d[1].text, "患方签字：<待填写>");
    }

    #[test]
    fn diff_keeps_original_order_and_flags_dropped_lines() {
        let original = vec!["甲".to_string(), "乙".to_string()];
        let skeleton = vec!["甲".to_string()];
        let d = diff_lines(&original, &skeleton);
        assert_eq!(
            d.iter().map(|l| (l.op.clone(), l.text.clone())).collect::<Vec<_>>(),
            vec![
                (DiffOp::Same, "甲".to_string()),
                (DiffOp::Removed, "乙".to_string())
            ]
        );
    }

    #[test]
    fn only_automatic_fixes_produce_a_selectable_block() {
        let it = item();
        let diags = vec![
            diag("QC-PSY-01", 0, Some(payload("患方签字："))),
            diag("QC-PSY-05", 0, None),
        ];
        let pv = preview_fixback(&it, &registry(), &diags);
        assert_eq!(pv.blocks.len(), 1, "只有 Automatic 带载荷的诊断生成候选块");
        assert_eq!(pv.blocks[0].code, "QC-PSY-01");
        assert_eq!(pv.blocks[0].section_index, Some(0));
        assert!(pv.blocks[0].has_additions());
        assert_eq!(pv.evidence_only, vec!["QC-PSY-05".to_string()]);
    }

    #[test]
    fn a_non_fixable_diagnostic_yields_no_button_and_only_the_basis() {
        let it = item();
        let pv = preview_fixback(&it, &registry(), &[diag("QC-PSY-08", 0, None)]);
        assert!(pv.is_empty(), "不可自动修不产生候选块");
        assert_eq!(pv.evidence_only, vec!["QC-PSY-08".to_string()]);
        assert_eq!(pv.summary(), "无可补写内容");
    }

    #[test]
    fn every_candidate_carries_its_rule_code_and_in_hospital_clause() {
        let it = item();
        let pv = preview_fixback(&it, &registry(), &[diag("QC-PSY-02", 0, Some(payload("x")))]);
        let b = &pv.blocks[0];
        assert_eq!(b.code, "QC-PSY-02");
        assert!(b.basis.contains("第 4.2 条"));
        assert!(b.to_fix_payload().summary.contains("第 4.2 条"));
    }

    #[test]
    fn candidates_of_other_documents_are_not_mixed_in() {
        let it = item();
        let other = Diagnostic::new(
            "QC-PSY-01",
            DocLoc::new("WARD-A-002-RECORD-01", 0, Span::new(0, 0).unwrap()),
            RuleMessage::new("别的件", "《规范》第 1 条").unwrap(),
        )
        .with_fix(payload("y"));
        let pv = preview_fixback(&it, &registry(), &[other]);
        assert!(pv.is_empty(), "别件的诊断不进本件的预览");
    }

    #[test]
    fn summary_counts_new_and_edited_sections_separately() {
        let it = item();
        let pv = preview_fixback(&it, &registry(), &[diag("QC-PSY-01", 0, Some(payload("z")))]);
        assert_eq!(pv.summary(), "修改 1 段");
    }
}