//! 豁免谓词化：把「按路径整条跳过」升级为**带条件的部分放行**。
//!
//! ## 源产品主能力与保留
//!
//! 源产品的 `per-file-ignores`（`github_ref/ruff/crates/ruff_workspace/src/configuration.rs`
//! 与 README 的级联配置一节）解决的是「这个文件我不查」——**二元**、无理由要求之外
//! 的结构约束、粒度固定在路径上。本轮**只借鉴其设计意图**（豁免与规则集同源、
//! 豁免必须写在配置里而不是藏在代码里），不搬源码。
//!
//! 院内场景逼出两级语义，这是源侧没有的：
//!
//! - [`WaiverEffect::NotApplicable`] —— 该病区**真的不收**这类文书（例如开放病区
//!   不收约束知情同意），整条规则不适用，不计入命中统计；
//! - [`WaiverEffect::DowngradeToNotice`] —— 业务还在办，只是本阶段不阻断提交
//!   （例如带教期口径），**仍计入命中**并降为提示。
//!
//! 二者混为一谈的后果很具体：把「不阻断」写成「整条跳过」，质控办按命中数统计
//! 时就会少算，把「真不适用」写成「仅提示」，病区就会收到一条永远改不掉的提示。
//!
//! ## 为什么每条豁免都要留溯源
//!
//! 「这条为什么不报」是院内最高频的追问。因此**豁免不得静默消失**：
//! 每条被放行的诊断都写一条 [`WaivedRecord`]，携带谓词摘要、生效时间窗、
//! 来源规则集层级与理由，可按住院别名回查；被更具体豁免盖掉的记在
//! [`WaivedRecord::superseded`] 里，不丢弃。
//!
//! 判不出来的（谓词带时间窗但件上没有可用时刻）走 [`WaiverSkip`]，
//! **既不豁免也不当成通过**。

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

use psychdoc_model::{DocItem, Severity};

use crate::ruleset::{EffectiveRules, LayerId};

// —————————————————————————————————————————————— 效果与组合

/// 豁免的两级语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WaiverEffect {
    /// 整条规则对本病区**不适用**：不进命中统计，不进待办，不阻断。
    #[default]
    NotApplicable,
    /// **降级为提示**：仍计入命中（可统计、可追责），但不阻断提交。
    DowngradeToNotice,
}

impl WaiverEffect {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NotApplicable => "整条不适用",
            Self::DowngradeToNotice => "降级为提示",
        }
    }
}

/// 多条件之间的组合方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PredicateCombine {
    /// 写了的每一类条件都要满足（默认）。写得越具体，命中面越窄。
    #[default]
    All,
    /// 任一写了的条件满足即放行。
    Any,
}

// —————————————————————————————————————————————— 时间窗

/// 豁免生效时间窗。两端都是**绝对时刻**，因此跨零点（23:40 起、次日 06:10 止）
/// 天然按真实时间比较，不做「当日时分」那种会算反的比较。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    /// 闭区间起点。
    #[serde(deserialize_with = "de_window_bound")]
    pub from: DateTime<FixedOffset>,
    /// 闭区间终点。
    #[serde(deserialize_with = "de_window_bound")]
    pub to: DateTime<FixedOffset>,
}

impl TimeWindow {
    pub fn contains(&self, at: DateTime<FixedOffset>) -> bool {
        at >= self.from && at <= self.to
    }

    /// 跨零点是否成立（`to` 在 `from` 之前即跨日）。
    pub fn crosses_midnight(&self) -> bool {
        self.to.time() < self.from.time()
    }
}

fn de_window_bound<'de, D>(d: D) -> Result<DateTime<FixedOffset>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let raw = String::deserialize(d)?;
    DateTime::parse_from_rfc3339(raw.trim()).map_err(|e| {
        serde::de::Error::custom(format!(
            "时间窗边界 {raw:?} 不是 RFC3339 时刻（例 2026-03-01T00:00:00+08:00）：{e}"
        ))
    })
}

// —————————————————————————————————————————————— 谓词

/// 一条豁免的谓词：四类条件 —— 病区 / 文书类型 / 规则码 / 时间窗。
///
/// 未写的条件 = **不受该维度约束**（不是「等于空」）。`combine` 决定写了的
/// 几类之间是 AND 还是 OR。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaiverPredicate {
    #[serde(default)]
    pub wards: Vec<String>,
    #[serde(default)]
    pub doc_types: Vec<String>,
    #[serde(default)]
    pub codes: Vec<String>,
    #[serde(default)]
    pub time_window: Option<TimeWindow>,
    #[serde(default)]
    pub combine: PredicateCombine,
}

impl Default for WaiverPredicate {
    fn default() -> Self {
        Self {
            wards: Vec::new(),
            doc_types: Vec::new(),
            codes: Vec::new(),
            time_window: None,
            combine: PredicateCombine::All,
        }
    }
}

impl WaiverPredicate {
    /// 写了多少类条件（0–4）。用于「取最具体一条」。
    pub fn class_count(&self) -> usize {
        usize::from(!self.wards.is_empty())
            + usize::from(!self.doc_types.is_empty())
            + usize::from(!self.codes.is_empty())
            + usize::from(self.time_window.is_some())
    }

    /// 写了多少个具体取值。同类数时取值多者更具体。
    pub fn value_count(&self) -> usize {
        self.wards.len() + self.doc_types.len() + self.codes.len()
    }

    /// 人可读的谓词摘要，直接贴进审计导出与 UI 的「为什么没报」。
    pub fn summary(&self, effect: WaiverEffect, source: LayerId) -> String {
        let mut parts: Vec<String> = Vec::new();
        if !self.wards.is_empty() {
            parts.push(self.wards.join("、"));
        }
        if !self.doc_types.is_empty() {
            parts.push(format!("文书类型 {}", self.doc_types.join("、")));
        }
        if !self.codes.is_empty() {
            parts.push(self.codes.join("、"));
        }
        if let Some(w) = &self.time_window {
            let span = format!("{} ~ {}", w.from.to_rfc3339(), w.to.to_rfc3339());
            parts.push(format!("生效 {span}{}", if w.crosses_midnight() { "（跨零点）" } else { "" }));
        }
        if parts.is_empty() {
            return format!("无限定谓词（{}，{}层）", effect.as_str(), source.label());
        }
        let joiner = if self.combine == PredicateCombine::All { " 且 " } else { " 或 " };
        format!("{}（{}，{}层）", parts.join(joiner), effect.as_str(), source.label())
    }

    /// 加载期自检：谓词不能是空的（空谓词 = 全放行，等于没有审查）。
    pub fn is_empty(&self) -> bool {
        self.class_count() == 0
    }

    /// 是否引用了不存在的文书类型（供加载期强校验调用）。
    pub fn undefined_doc_types<'a>(&'a self, known: &'a [&'a str]) -> Vec<&'a str> {
        self.doc_types
            .iter()
            .map(String::as_str)
            .filter(|t| !known.contains(t))
            .collect()
    }
}

// —————————————————————————————————————————————— 判定输入与结论

/// 判定一条诊断是否落在豁免谓词内所需的全部事实。
#[derive(Debug, Clone, PartialEq)]
pub struct WaiverSubject<'a> {
    pub code: &'a str,
    pub doc_id: &'a str,
    pub encounter_alias: &'a str,
    pub ward: &'a str,
    pub doc_type: &'a str,
    /// 该件用于比对时间窗的业务时刻（最早的可用时刻）。
    pub moment: Option<DateTime<FixedOffset>>,
}

/// 谓词判定结论。**三态**：判不出来与判为「不命中」必须分开。
#[derive(Debug, Clone, PartialEq)]
pub enum PredicateVerdict {
    Match,
    NoMatch,
    /// 条件里有时间窗而件上没有可用时刻 → 判不出来。
    /// 既不豁免、也不当成通过。
    Indeterminate(String),
}

impl PredicateVerdict {
    pub fn is_match(&self) -> bool {
        matches!(self, Self::Match)
    }

    pub fn is_indeterminate(&self) -> bool {
        matches!(self, Self::Indeterminate(_))
    }
}

/// 求住院别名所属病区。
///
/// 别名形如 `<病区>-<序号>`（`WARD-A-001` → `WARD-A`）。末段不是纯数字时
/// **不猜**，整体作为病区返回——猜错病区会让豁免套到别的病区上。
pub fn ward_of(encounter_alias: &str) -> String {
    match encounter_alias.rsplit_once('-') {
        Some((head, tail))
            if !head.is_empty()
                && !tail.is_empty()
                && tail.chars().all(|c| c.is_ascii_digit()) =>
        {
            head.to_string()
        }
        _ => encounter_alias.to_string(),
    }
}

/// 一件文书用于比对时间窗的时刻：最早的可用时刻。
///
/// 取「最早」而非「最晚」：豁免窗口一般从文书业务发生时算起，晚的那个时刻
/// 会把一份傍晚补签的件推出窗口，从而让补签件逃过豁免。
pub fn subject_moment(item: &DocItem) -> Option<DateTime<FixedOffset>> {
    item.timestamps.iter().filter_map(|t| t.moment).min()
}

/// 按谓词判定。
pub fn evaluate(predicate: &WaiverPredicate, subject: &WaiverSubject<'_>) -> PredicateVerdict {
    let checks: Vec<(bool, bool, String)> = vec![
        (
            !predicate.wards.is_empty(),
            predicate.wards.iter().any(|w| w == subject.ward),
            format!("病区 {} 不在豁免病区 {:?}", subject.ward, predicate.wards),
        ),
        (
            !predicate.doc_types.is_empty(),
            predicate.doc_types.iter().any(|t| t == subject.doc_type),
            format!("文书类型 {} 不在豁免范围 {:?}", subject.doc_type, predicate.doc_types),
        ),
        (
            !predicate.codes.is_empty(),
            predicate.codes.iter().any(|c| c == subject.code),
            format!("规则码 {} 不在豁免范围 {:?}", subject.code, predicate.codes),
        ),
        (
            predicate.time_window.is_some(),
            match (predicate.time_window, subject.moment) {
                (Some(w), Some(m)) => w.contains(m),
                _ => false,
            },
            match (predicate.time_window, subject.moment) {
                (Some(w), Some(m)) => format!(
                    "件的业务时刻 {} 不在豁免窗口 {} ~ {}",
                    m.to_rfc3339(),
                    w.from.to_rfc3339(),
                    w.to.to_rfc3339()
                ),
                (Some(w), None) => format!(
                    "件上没有可用时刻，无法判断是否落在豁免窗口 {} ~ {}",
                    w.from.to_rfc3339(),
                    w.to.to_rfc3339()
                ),
                (None, _) => "本谓词不带时间窗".to_string(),
            },
        ),
    ];

    let active: Vec<&(bool, bool, String)> = checks.iter().filter(|(present, _, _)| *present).collect();
    if active.is_empty() {
        return PredicateVerdict::Indeterminate("谓词没有任何生效条件".to_string());
    }

    match predicate.combine {
        PredicateCombine::All => {
            // AND：任一不成立即不命中。判不出来的那一项单独记账——
            // 不能因为「时间窗判不了」就把另外两条已成立的条件一并丢掉。
            if active.iter().any(|(_, ok, _)| !ok) {
                let indeterminate = checks
                    .iter()
                    .filter(|(present, ok, _)| *present && !ok)
                    .find(|(_, _, why)| why.contains("没有可用时刻"));
                if let Some((_, _, why)) = indeterminate {
                    return PredicateVerdict::Indeterminate(why.clone());
                }
                return PredicateVerdict::NoMatch;
            }
            PredicateVerdict::Match
        }
        PredicateCombine::Any => {
            if active.iter().any(|(_, ok, _)| *ok) {
                return PredicateVerdict::Match;
            }
            match active.iter().find(|(_, _, why)| why.contains("没有可用时刻")) {
                Some((_, _, why)) => PredicateVerdict::Indeterminate((*why).clone()),
                None => PredicateVerdict::NoMatch,
            }
        }
    }
}

// —————————————————————————————————————————————— 溯源记录

/// 一条被豁免放行的诊断的溯源记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaivedRecord {
    pub code: String,
    pub doc_id: String,
    pub encounter_alias: String,
    pub ward: String,
    pub doc_type: String,
    pub effect: WaiverEffect,
    /// 豁免理由（加载期强制非空）。
    pub reason: String,
    /// 人可读的谓词摘要：条件 + 两级语义 + 来源层级。
    pub predicate: String,
    /// 生效时间窗的可读形式（无时间窗时为空串）。
    pub window: String,
    /// 声明这条豁免的规则集层级。
    pub source: LayerId,
    /// 同时命中、但被更具体的一条盖掉的其它豁免摘要（不丢弃）。
    pub superseded: Vec<String>,
}

/// 判不出来、因而**没有**豁免的记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WaiverSkip {
    pub code: String,
    pub doc_id: String,
    pub encounter_alias: String,
    pub predicate: String,
    /// 为什么判不出来。
    pub detail: String,
}

/// 选定「最具体的一条豁免」。返回值同时给出其余命中项的摘要。
///
/// 具体度 = （条件类数多者优先，取值数少者优先，声明序靠前者优先）：
///
/// - **类数多** = 约束的维度多，命中面必然更窄；
/// - **取值少** = 在同一维度上写得越窄越具体（列了 WARD-A、WARD-B 两个病区的
///   豁免比只列 WARD-A 的**更宽**，不是更具体）；
/// - 同具体度时**先声明的赢**，保证规则集改一次只影响真正变了的那条。
pub fn pick_most_specific(
    candidates: &[crate::ruleset::WardIgnore],
) -> Option<(usize, &crate::ruleset::WardIgnore, Vec<String>)> {
    let mut indexed: Vec<usize> = (0..candidates.len()).collect();
    indexed.sort_by_key(|&i| {
        let p = &candidates[i].predicate();
        (std::cmp::Reverse(p.class_count()), p.value_count(), i)
    });
    let winner = *indexed.first()?;
    let superseded = indexed
        .iter()
        .skip(1)
        .map(|&i| {
            candidates[i]
                .predicate()
                .summary(candidates[i].effect, candidates[i].source)
        })
        .collect();
    Some((winner, &candidates[winner], superseded))
}

/// 把豁免应用到一批已产出的诊断上。
///
/// - [`WaiverEffect::NotApplicable`]：从 `diagnostics` 中**移除**（不计入命中统计）；
/// - [`WaiverEffect::DowngradeToNotice`]：留在 `diagnostics` 里但 `severity` 降为
///   [`Severity::Notice`]，并在 `Diagnostic::waived` 写明理由。
///
/// 两条路都写 [`WaivedRecord`]；判不出来的两条路都写 [`WaiverSkip`]。
pub fn apply(
    items: &[DocItem],
    ruleset: &EffectiveRules,
    diagnostics: &mut Vec<psychdoc_model::Diagnostic>,
    waived: &mut Vec<WaivedRecord>,
    skipped: &mut Vec<WaiverSkip>,
) {
    if ruleset.ward_ignores.is_empty() {
        return;
    }
    // 件索引：诊断只带 doc_id，病区 / 文书类型 / 时刻都要回件里取。
    let mut index: std::collections::BTreeMap<&str, &DocItem> = std::collections::BTreeMap::new();
    for item in items {
        index.entry(item.doc_id.as_str()).or_insert(item);
    }

    let mut kept: Vec<psychdoc_model::Diagnostic> = Vec::with_capacity(diagnostics.len());
    for diagnostic in diagnostics.drain(..) {
        let Some(item) = index.get(diagnostic.loc.doc_id.as_str()).copied() else {
            // 件索引里没有 → 说明这条诊断不是本批判出来的，不动它。
            kept.push(diagnostic);
            continue;
        };
        let subject = WaiverSubject {
            code: &diagnostic.code,
            doc_id: &diagnostic.loc.doc_id,
            encounter_alias: &item.encounter_alias,
            ward: &ward_of(&item.encounter_alias),
            doc_type: &item.template_id,
            moment: subject_moment(item),
        };

        let mut hits: Vec<usize> = Vec::new();
        for (i, w) in ruleset.ward_ignores.iter().enumerate() {
            match evaluate(&w.predicate(), &subject) {
                PredicateVerdict::Match => hits.push(i),
                PredicateVerdict::NoMatch => {}
                PredicateVerdict::Indeterminate(detail) => skipped.push(WaiverSkip {
                    code: diagnostic.code.clone(),
                    doc_id: diagnostic.loc.doc_id.clone(),
                    encounter_alias: item.encounter_alias.clone(),
                    predicate: w.predicate().summary(w.effect, w.source),
                    detail,
                }),
            }
        }
        if hits.is_empty() {
            kept.push(diagnostic);
            continue;
        }

        let candidates: Vec<&crate::ruleset::WardIgnore> =
            hits.iter().map(|&i| &ruleset.ward_ignores[i]).collect();
        let borrowed: Vec<crate::ruleset::WardIgnore> =
            candidates.iter().map(|w| (*w).clone()).collect();
        let (_, winner, superseded) =
            pick_most_specific(&borrowed).expect("hits 非空时必有候选");

        let record = WaivedRecord {
            code: diagnostic.code.clone(),
            doc_id: diagnostic.loc.doc_id.clone(),
            encounter_alias: item.encounter_alias.clone(),
            ward: subject.ward.to_string(),
            doc_type: subject.doc_type.to_string(),
            effect: winner.effect,
            reason: winner.reason.clone(),
            predicate: winner.predicate().summary(winner.effect, winner.source),
            window: winner
                .predicate()
                .time_window
                .map(|w| format!("{} ~ {}", w.from.to_rfc3339(), w.to.to_rfc3339()))
                .unwrap_or_default(),
            source: winner.source,
            superseded,
        };

        match winner.effect {
            WaiverEffect::NotApplicable => {
                // 整条不适用 → 不进命中统计。
                waived.push(record);
            }
            WaiverEffect::DowngradeToNotice => {
                let mut d = diagnostic;
                d.severity = Severity::Notice;
                d.waived = Some(format!("{}：{}", record.predicate, winner.reason));
                kept.push(d);
                waived.push(record);
            }
        }
    }
    *diagnostics = kept;
}

/// 本批里一次都没命中的豁免（死豁免）。
///
/// 「配了但从没生效过」是最容易被忘掉的一类配置问题——维护者以为豁免了，
/// 实际一条都没放行。这里显式列出来，不让它静默躺在配置里。
pub fn dead_waivers(ruleset: &EffectiveRules, waived: &[WaivedRecord]) -> Vec<String> {
    let mut hit: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for r in waived {
        hit.insert(r.predicate.as_str());
    }
    ruleset
        .ward_ignores
        .iter()
        .filter(|w| {
            let s = w.predicate().summary(w.effect, w.source);
            !hit.contains(s.as_str())
        })
        .map(|w| w.predicate().summary(w.effect, w.source))
        .collect()
}
#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::{Precision, Timestamp, TimestampKind};

    fn at(text: &str) -> DateTime<FixedOffset> {
        DateTime::parse_from_rfc3339(text).expect("测试里的时刻都写成 RFC3339")
    }

    fn window(from: &str, to: &str) -> TimeWindow {
        TimeWindow { from: at(from), to: at(to) }
    }

    /// 病区单独传进来：`WaiverSubject` 持借用，自算出来的 `String` 活不过本函数。
    fn subject<'a>(
        ward: &'a str,
        doc_type: &'a str,
        code: &'a str,
        moment: Option<DateTime<FixedOffset>>,
    ) -> WaiverSubject<'a> {
        WaiverSubject {
            code,
            doc_id: "DOC-1",
            encounter_alias: ward,
            ward,
            doc_type,
            moment,
        }
    }

    // ——— 病区推导：猜错病区会让豁免套到别的病区上，宁可不猜 ———

    #[test]
    fn ward_is_the_alias_without_its_trailing_sequence() {
        assert_eq!(ward_of("WARD-A-001"), "WARD-A");
        assert_eq!(ward_of("WARD-B-123"), "WARD-B");
    }

    #[test]
    fn an_alias_without_a_numeric_tail_is_taken_whole_never_guessed() {
        // 末段不是纯数字 → 整体作为病区，不去猜「哪一段才是病区」。
        assert_eq!(ward_of("WARD-A-CONSENT"), "WARD-A-CONSENT");
        assert_eq!(ward_of("WARD-A"), "WARD-A");
    }

    // ——— 四类条件与 AND / OR ———

    #[test]
    fn an_unwritten_dimension_does_not_constrain() {
        // 谓词只写病区：文书类型与时间窗不该拦下别的文书。
        let p = WaiverPredicate { wards: vec!["WARD-A".into()], ..Default::default() };
        let v = evaluate(&p, &subject("WARD-A", "restraint_record", "QC-PSY-09", None));
        assert_eq!(v, PredicateVerdict::Match, "未写的维度不受约束：{:?}", v);
    }

    #[test]
    fn all_requires_every_written_condition() {
        let p = WaiverPredicate {
            wards: vec!["WARD-A".into()],
            doc_types: vec!["restraint_consent".into()],
            codes: vec!["QC-PSY-01".into()],
            time_window: None,
            combine: PredicateCombine::All,
        };
        assert!(evaluate(&p, &subject("WARD-A", "restraint_consent", "QC-PSY-01", None)).is_match());
        assert_eq!(
            evaluate(&p, &subject("WARD-A", "restraint_record", "QC-PSY-01", None)),
            PredicateVerdict::NoMatch,
            "文书类型不成立即不命中"
        );
        assert_eq!(
            evaluate(&p, &subject("WARD-B", "restraint_consent", "QC-PSY-01", None)),
            PredicateVerdict::NoMatch,
            "病区不成立即不命中"
        );
    }

    #[test]
    fn any_fires_on_the_first_condition_that_holds() {
        let p = WaiverPredicate {
            wards: vec!["WARD-A".into()],
            doc_types: vec!["restraint_consent".into()],
            codes: vec!["QC-PSY-04".into()],
            time_window: None,
            combine: PredicateCombine::Any,
        };
        // 病区成立、文书类型不成立 → OR 仍放行。
        assert!(evaluate(&p, &subject("WARD-A", "restraint_record", "QC-PSY-09", None)).is_match());
        // 三条都不成立 → 不命中。
        assert_eq!(
            evaluate(&p, &subject("WARD-B", "restraint_record", "QC-PSY-09", None)),
            PredicateVerdict::NoMatch
        );
    }

    // ——— 时间窗：跨零点按真实时间比，缺时刻判不出来 ———

    #[test]
    fn a_window_spanning_midnight_compares_real_elapsed_time() {
        let w = window("2026-03-02T22:00:00+08:00", "2026-03-03T07:00:00+08:00");
        assert!(w.crosses_midnight(), "止点在次日，窗口应被标为跨零点");
        // 次日 06:10 落在窗口内；若按「当日时分 06:10 < 22:00」比就会被误判为窗口外。
        assert!(w.contains(at("2026-03-03T06:10:00+08:00")));
        assert!(!w.contains(at("2026-03-03T07:00:01+08:00")));
    }

    #[test]
    fn a_window_outside_the_moment_never_matches() {
        let w = window("2026-04-01T00:00:00+08:00", "2026-04-30T23:59:59+08:00");
        assert!(!w.contains(at("2026-03-02T08:30:00+08:00")));
    }

    #[test]
    fn a_window_on_a_document_without_a_moment_is_indeterminate_not_a_match() {
        let p = WaiverPredicate {
            wards: vec!["WARD-A".into()],
            codes: vec!["QC-PSY-01".into()],
            time_window: Some(window("2026-03-01T00:00:00+08:00", "2026-03-31T23:59:59+08:00")),
            ..Default::default()
        };
        let v = evaluate(&p, &subject("WARD-A", "restraint_consent", "QC-PSY-01", None));
        assert!(v.is_indeterminate(), "缺时刻时判不出来，不得当成命中：{v:?}");
        assert!(
            matches!(&v, PredicateVerdict::Indeterminate(why) if why.contains("没有可用时刻")),
            "判不出来的原因要能贴进交办单：{v:?}"
        );
        assert!(!v.is_match());
    }

    #[test]
    fn an_empty_predicate_is_indeterminate_rather_than_a_blanket_pass() {
        let v = evaluate(&WaiverPredicate::default(), &subject("WARD-A", "t", "QC-PSY-01", None));
        assert!(v.is_indeterminate(), "空谓词不等于全放行：{v:?}");
        assert!(WaiverPredicate::default().is_empty());
    }

    // ——— 件时刻取「最早」而非「最晚」 ———

    #[test]
    fn the_subject_moment_is_the_earliest_known_one() {
        let mut item = DocItem::new("DOC-1", "restraint_consent", "WARD-A-001");
        item.timestamps = vec![
            Timestamp::at(
                TimestampKind::AuthoredOn,
                at("2026-03-02T08:52:00+08:00"),
                Precision::Second,
                0,
                None,
            ),
            Timestamp::at(
                TimestampKind::Date,
                at("2026-03-02T08:30:00+08:00"),
                Precision::Second,
                0,
                None,
            ),
        ];
        assert_eq!(
            subject_moment(&item),
            Some(at("2026-03-02T08:30:00+08:00")),
            "取最早时刻：晚签的件不该被推出窗口"
        );
    }

    #[test]
    fn an_item_with_no_timestamps_at_all_has_no_subject_moment() {
        let item = DocItem::new("DOC-1", "restraint_consent", "WARD-A-001");
        assert_eq!(subject_moment(&item), None, "没有时刻就是没有，不填当前时间");
    }

    // ——— 具体度排序 ———

    fn waiver(wards: &[&str], doc_types: &[&str], codes: &[&str], window: Option<TimeWindow>) -> crate::ruleset::WardIgnore {
        crate::ruleset::WardIgnore {
            ward: wards.first().copied().unwrap_or_default().to_string(),
            wards: wards.iter().skip(1).map(|s| s.to_string()).collect(),
            codes: codes.iter().map(|s| s.to_string()).collect(),
            doc_types: doc_types.iter().map(|s| s.to_string()).collect(),
            time_window: window,
            combine: PredicateCombine::All,
            effect: WaiverEffect::NotApplicable,
            source: LayerId::Hospital,
            reason: "测试用".into(),
        }
    }

    #[test]
    fn the_most_specific_waiver_wins_and_the_rest_are_recorded_not_dropped() {
        let w = window("2026-03-01T00:00:00+08:00", "2026-03-03T00:00:00+08:00");
        let candidates = vec![
            waiver(&["WARD-A"], &[], &["QC-PSY-01"], None),                            // 2 类
            waiver(&["WARD-A"], &["restraint_consent"], &["QC-PSY-01"], None),         // 3 类
            waiver(&["WARD-A"], &["restraint_consent"], &["QC-PSY-01"], Some(w)),      // 4 类
        ];
        let (_, winner, superseded) = pick_most_specific(&candidates).unwrap();
        assert_eq!(winner.predicate().class_count(), 4, "条件类数最多的应胜出");
        assert_eq!(superseded.len(), 2, "其余两条记为 superseded，一条都不能丢");
    }

    #[test]
    fn ties_on_specificity_are_broken_by_declaration_order_so_results_are_reproducible() {
        let candidates = vec![
            waiver(&["WARD-A"], &[], &["QC-PSY-01"], None),
            waiver(&["WARD-A", "WARD-B"], &[], &["QC-PSY-01"], None),
        ];
        let first = pick_most_specific(&candidates).unwrap();
        let again = pick_most_specific(&candidates).unwrap();
        assert_eq!(first.0, again.0, "同具体度时先声明的赢，两次结果必须一致");
        assert_eq!(first.1.ward_list().len(), 1, "取值更少的那条胜出");
        assert_eq!(first.2.len(), 1);
    }

    // ——— 简写归一化 ———

    #[test]
    fn the_single_ward_shorthand_folds_into_the_ward_list_without_duplicating() {
        let w = waiver(&["WARD-A"], &[], &["QC-PSY-01"], None);
        assert_eq!(w.ward_list(), vec!["WARD-A".to_string()], "ward: 单值折进 wards");
        let w = waiver(&["WARD-A", "WARD-B"], &[], &["QC-PSY-01"], None);
        assert_eq!(
            w.ward_list(),
            vec!["WARD-A".to_string(), "WARD-B".to_string()],
            "单值 + 列表合并去重"
        );
        let blank = waiver(&["", "  ", "WARD-A", "WARD-A"], &[], &["QC-PSY-01"], None);
        assert_eq!(blank.ward_list(), vec!["WARD-A".to_string()], "空白项跳过、重复项去重");
    }

    #[test]
    fn an_undefined_doc_type_is_reported_for_load_time_validation() {
        let p = WaiverPredicate {
            doc_types: vec!["restraint_consent".into(), "not_a_doc_type".into()],
            ..Default::default()
        };
        let known = ["restraint_consent", "restraint_record"];
        assert_eq!(p.undefined_doc_types(&known), vec!["not_a_doc_type"]);
    }

    #[test]
    fn the_summary_states_the_effect_the_source_layer_and_the_crossing() {
        let p = WaiverPredicate {
            wards: vec!["WARD-A".into()],
            codes: vec!["QC-PSY-01".into()],
            time_window: Some(window("2026-03-02T22:00:00+08:00", "2026-03-03T07:00:00+08:00")),
            ..Default::default()
        };
        let s = p.summary(WaiverEffect::DowngradeToNotice, LayerId::Department);
        assert!(s.contains("WARD-A") && s.contains("QC-PSY-01"), "{s}");
        assert!(s.contains("降级为提示"), "两级语义必须写在摘要里：{s}");
        assert!(s.contains("跨零点"), "跨零点要标出来，否则复核的人会按当日时分读：{s}");
        assert!(s.contains(" 且 "), "AND 组合用「且」：{s}");
        assert!(s.contains("科室级"), "来源层级要写清：{s}");
    }

    #[test]
    fn class_and_value_counts_drive_specificity() {
        let p = WaiverPredicate {
            wards: vec!["WARD-A".into()],
            codes: vec!["QC-PSY-01".into(), "QC-PSY-02".into()],
            ..Default::default()
        };
        assert_eq!(p.class_count(), 2);
        assert_eq!(p.value_count(), 3);
    }

    #[test]
    fn a_malformed_window_bound_is_reported_instead_of_defaulting() {
        let y = r#"
from: "2026年3月1日"
to: "2026-03-31T23:59:59+08:00"
"#;
        let err = serde_yaml::from_str::<TimeWindow>(y).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("RFC3339"), "非法时刻必须点名格式要求：{msg}");
        assert!(msg.contains("2026年3月1日"), "错误信息要带出原值，便于定位：{msg}");
    }

    #[test]
    fn a_well_formed_window_parses_from_yaml_with_a_fallback_offset() {
        let y = r#"
from: "2026-03-02T22:00:00+08:00"
to: "2026-03-03T07:00:00+08:00"
"#;
        let w: TimeWindow = serde_yaml::from_str(y).unwrap();
        assert!(w.contains(at("2026-03-03T06:10:00+08:00")));
        assert!(w.crosses_midnight());
        // 带空白的写法也能吃（YAML 里缩进常把人坑在这）。
        let padded: TimeWindow = serde_yaml::from_str(
            "from: \"  2026-03-02T22:00:00+08:00 \"\nto: \"2026-03-03T07:00:00+08:00\"\n",
        )
        .unwrap();
        assert_eq!(padded.from, w.from, "边界值前后空白应被裁掉");
    }

    #[test]
    fn an_inverted_window_is_not_silently_accepted() {
        // 起点晚于终点 → 窗口恒为空。加载期由 InvalidWaiverWindow 拒掉，
        // 这里守住 TimeWindow 本身不把它悄悄「修正」成合法窗口。
        let w = window("2026-03-31T00:00:00+08:00", "2026-03-01T00:00:00+08:00");
        assert!(!w.contains(at("2026-03-15T00:00:00+08:00")), "起止写反的窗口应恒不命中");
    }
}
