//! 多规则命中同一处的**去重与择优**（Task 23）。
//!
//! ## 要解决的问题
//!
//! 真实规则集互相重叠。同一个字段既能被判「必填缺失」（`QC-PSY-01`），
//! 又能被判「值形状不符」（`QC-PSY-02`）——而这两条的整改动作其实**只有一条**：
//! 把这一栏填对。同一处刷两行，护士长会以为要改两件事；规则集越长，刷得越凶。
//!
//! 源产品在这一点上给的是**抑制注释**（`github_ref/ruff/crates/ty_python_semantic/
//! src/suppression/parser.rs`：`# ty: ignore[code1, code2]`——一条抑制声明可以
//! 点名若干个码，并被解析成带 `TextRange` 的 `SuppressionComment`）。本项目复用
//! 的是它的**形状**：被抑制的条目**带着抑制它的码留下来**，而不是消失。
//! 差别在于抑制的来源：源侧由人写注释，本项目由**码族择优**自动判定——
//! 因为院内没人会为了消掉重复行去给病历加注释。
//!
//! ## 三条不可让步的行为
//!
//! 1. **折叠不丢弃**。被压下去的那条进 [`SuppressedFinding`]，`suppressed_by`
//!    指明是谁覆盖了它，清单上可展开。压下去就消失等于让护士长以为工具漏报了。
//! 2. **严重度只上不下**。赢家若比输家轻，赢家的严重度被**抬到输家那一档**。
//!    「为去重而降级」等于用界面整洁换漏报，这是本系统最不能出的错。
//! 3. **未归族的码不折叠**。[`crate::ruleset::EffectiveRules::family_of`] 返回
//!    `None` 时，该码自成一族——宁可同处多报一行，也不替质控办猜一个族。
//!
//! ## 去重键
//!
//! `(doc_id, section_index, char_range, family)`。前三项就是位置三元组
//! （`psychdoc_model::DocLoc`），第四项保证**只折叠说同一件事的规则**。
//! 跨件的两条时间序诊断各自带自己的位置三元组，本来就落不到同一个键上。

use std::collections::BTreeMap;

use psychdoc_model::{Diagnostic, DocLoc, Severity};
use serde::Serialize;

use crate::ruleset::EffectiveRules;

/// 去重键。四项全等才可能折叠。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct DedupKey {
    pub doc_id: String,
    pub section_index: u32,
    pub start: u32,
    pub end: u32,
    /// 族标识；未归族时用 `"<独立码:{code}>"`，保证它永远不会与任何别的码同键。
    pub family: String,
}

impl DedupKey {
    pub fn of(d: &Diagnostic, ruleset: &EffectiveRules) -> Self {
        let family = match ruleset.family_of(&d.code) {
            Some(f) => f.family.clone(),
            None => format!("<独立码:{}>", d.code),
        };
        Self {
            doc_id: d.loc.doc_id.clone(),
            section_index: d.loc.section_index,
            start: d.loc.char_range.start,
            end: d.loc.char_range.end,
            family,
        }
    }

    /// 清单上给护士长看的一行。
    pub fn describe_zh(&self) -> String {
        format!("{}#s{}@[{}, {}) · 码族 {}", self.doc_id, self.section_index, self.start, self.end, self.family)
    }
}

/// 一条被更具体规则压下去的诊断。**不丢弃**，可展开。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SuppressedFinding {
    /// 被压下去的码。
    pub code: String,
    /// 压它的那条（赢家）的码。
    pub suppressed_by: String,
    /// 赢家的完整文案——展开时要看清楚「真正该改的是哪一条」。
    pub winner_message: String,
    /// 输家自己的文案与依据，同样保留。
    pub message: String,
    pub basis: String,
    pub loc: DocLoc,
    /// 输家原本的严重度。用于核对「压下去的那条本来有多重」。
    pub severity: Severity,
    /// 族内中文名 + 院内理由。
    pub family_label: String,
    pub reason: String,
    /// 折叠是否把这一处的严重度**抬高**了（赢家比输家轻时）。
    pub escalated: bool,
}

impl SuppressedFinding {
    /// 折叠说明一行。
    pub fn as_zh(&self) -> String {
        let mark = if self.escalated { "（并按更重的一档计入阻断）" } else { "" };
        format!(
            "{loc} · {code} 已被更具体的 {win} 覆盖{mark}：{family} —— {reason}",
            loc = self.loc,
            code = self.code,
            win = self.suppressed_by,
            family = self.family_label,
            reason = self.reason,
        )
    }
}

/// 去重结果：赢家按原序留在 `kept`，输家按发现顺序进 `suppressed`。
#[derive(Debug, Clone, Default)]
pub struct DedupOutcome {
    pub kept: Vec<Diagnostic>,
    pub suppressed: Vec<SuppressedFinding>,
    /// 折叠掉的条数。`kept.len() + suppressed.len() == 输入条数` 恒成立。
    pub suppressed_count: usize,
}

/// 对一批诊断做「同键择优」。
///
/// 具体度 = **族内下标**（`ruleset` 里 `codes` 由泛到具体，越靠后越具体），
/// 规则集未归族时该码自成一族（键不同，永不折叠）。
///
/// 平手时按 `(具体度, 严重度, 码升序)` 决胜，保证同一份输入两次运行结果逐字节相同。
pub fn dedup(diagnostics: Vec<Diagnostic>, ruleset: &EffectiveRules) -> DedupOutcome {
    // 保序：先记下每条在输入中的位置，输出时赢家站在**该组第一条**的位置上。
    let mut groups: BTreeMap<DedupKey, Vec<(usize, Diagnostic)>> = BTreeMap::new();
    let mut slots: Vec<Option<Diagnostic>> = Vec::with_capacity(diagnostics.len());
    for (i, d) in diagnostics.into_iter().enumerate() {
        let key = DedupKey::of(&d, ruleset);
        slots.push(None);
        groups.entry(key).or_default().push((i, d));
    }

    let total = slots.len();
    let mut suppressed: Vec<SuppressedFinding> = Vec::new();

    for (key, members) in groups {
        if members.len() == 1 {
            let (i, d) = members.into_iter().next().expect("长度已判为 1");
            slots[i] = Some(d);
            continue;
        }
        // 择优：**具体度大的先排**（`sort_by` 是升序，`Ordering::Reverse` 把
        // 「最具体」放到队首）。其次严重度重的先排，最后按码升序 + 输入序兜底。
        // 三者全等才会落到同一码，而同一码在同一处出现两次是**规则集写重了**，
        // 由 `E_RULESET_INVALID` 在加载期拦掉，不在这里静默按一条处理。
        let mut ordered = members;
        ordered.sort_by(|(ia, a), (ib, b)| {
            spec(ruleset, b)
                .cmp(&spec(ruleset, a))
                .then_with(|| {
                    b.severity
                        .rank()
                        .cmp(&a.severity.rank())
                        .then_with(|| a.code.cmp(&b.code))
                        .then_with(|| ia.cmp(ib))
                })
        });
        let (winner_slot, winner) = ordered[0].clone();
        let losers = &ordered[1..];

        // 严重度只上不下：赢家若比任何一条输家都轻，抬到最重的那一档。
        let mut winner = winner;
        let mut escalated = false;
        for (_, l) in losers {
            if l.severity.rank() > winner.severity.rank() {
                winner.severity = l.severity;
                escalated = true;
            }
        }

        let family = ruleset.family_of(&winner.code);
        for (_, l) in losers {
            suppressed.push(SuppressedFinding {
                code: l.code.clone(),
                suppressed_by: winner.code.clone(),
                winner_message: winner.message.render(),
                message: l.message.render(),
                basis: l.message.basis.clone(),
                loc: l.loc.clone(),
                severity: l.severity,
                family_label: family.map(|f| f.label.clone()).unwrap_or_else(|| "未归族".to_string()),
                reason: family.map(|f| f.reason.clone()).unwrap_or_else(|| {
                    format!("{} 同一处命中同一族规则", key.describe_zh())
                }),
                escalated,
            });
        }
        slots[winner_slot] = Some(winner);
    }

    let kept: Vec<Diagnostic> = slots.into_iter().flatten().collect();
    let suppressed_count = suppressed.len();
    debug_assert_eq!(kept.len() + suppressed_count, total, "折叠不丢弃：总数必须守恒");
    DedupOutcome { kept, suppressed, suppressed_count }
}

/// 族内具体度；未归族给 `0`（但它的键含码名，不会与别人同组）。
fn spec(ruleset: &EffectiveRules, d: &Diagnostic) -> usize {
    ruleset.specificity_of(&d.code).unwrap_or(0)
}
