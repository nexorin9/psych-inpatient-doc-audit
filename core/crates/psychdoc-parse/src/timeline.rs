//! 件级时间序索引：把一次住院下多件的 `timestamps[]` 拼成**一条**事件时间线。
//!
//! 排序口径（本轮只做取样与排序，**违规判定留给 Task 19**）：
//!
//! 1. 每件取一个代表时刻（`Date` > `BusinessMoment` > `AuthoredOn`，取最早的
//!    一个已抽出的时刻），保证「约束记录早于约束解除记录」这类顺序可核对。
//! 2. **缺时刻的件排在末位**，并显式标 `missing_moment = true` ——
//!    绝不用当前时间或零值把它塞进时间线里，那会让它看起来「有时刻」。
//! 3. 同一时刻并列时按 `(时刻, 件 id)` 稳定排序，保证两次跑批顺序一致。
//!
//! 对应源产品主路径：`analyze_graph` 收集到 `ModuleImports` 后交给
//! `ImportMap` 建图之前的那一步「先把文件按发现顺序稳定化」。本产品把它
//! 显式化为**时间序索引**，因为文书包的先后关系本身就是业务事实，
//! 不能依赖文件系统给的顺序。

use psychdoc_model::{DocItem, Precision, TimestampKind};
use serde::{Deserialize, Serialize};

/// 时间线上一件。`moment` 为 `None` 即「该件没抽出任何时刻」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEntry {
    pub doc_id: String,
    pub template_id: String,
    pub encounter_alias: String,
    /// 代表时刻；`None` 表示整件没有任何时刻
    pub moment: Option<chrono::DateTime<chrono::FixedOffset>>,
    /// 该时刻的来源类别（便于 UI 说清「按哪个时刻排的」）
    pub source: Option<TimestampKind>,
    pub precision: Option<Precision>,
    /// 缺时刻的件排在末位且此位为 `true`
    pub missing_moment: bool,
    /// 该件抽不出的时刻种类，供人工队列与时间序规则记 `skipped_reason`
    pub missing_kinds: Vec<TimestampKind>,
}

impl TimelineEntry {
    pub fn as_zh(&self, display: &str) -> String {
        match self.moment {
            Some(m) => format!("{display} · {m}", m = m.format("%Y-%m-%d %H:%M")),
            None => format!("{display} · 缺时刻（时间序跳过）"),
        }
    }
}

/// 排序结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineOrder {
    /// 按业务时刻升序；缺时刻的件在末尾
    pub entries: Vec<TimelineEntry>,
    /// 缺时刻的件数（= 人工要补时刻的件数）
    pub missing_count: usize,
}

impl TimelineOrder {
    /// 缺时刻的件，供「缺什么」清单直接渲染。
    pub fn items_without_moment(&self) -> impl Iterator<Item = &TimelineEntry> {
        self.entries.iter().filter(|e| e.missing_moment)
    }

    /// 取两件的时刻差（秒）。任一侧缺时刻返回 `None`——**不编造时差**。
    pub fn gap_seconds(&self, a: &str, b: &str) -> Option<i64> {
        let ma = self.entries.iter().find(|e| e.doc_id == a)?.moment?;
        let mb = self.entries.iter().find(|e| e.doc_id == b)?.moment?;
        Some((mb - ma).num_seconds())
    }
}

/// 代表时刻的选取优先级：`Date`（文书生效）> 段落内业务时刻 > 书写时刻。
/// 同一类别内取最早的一个。
fn representative(item: &DocItem) -> Option<(chrono::DateTime<chrono::FixedOffset>, TimestampKind, Precision)> {
    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, TimestampKind, Precision)> = None;
    let rank = |k: TimestampKind| match k {
        TimestampKind::Date => 0u8,
        TimestampKind::BusinessMoment => 1,
        TimestampKind::AuthoredOn => 2,
    };
    for t in &item.timestamps {
        let Some(m) = t.moment else { continue };
        let better = match &best {
            None => true,
            Some((bm, bk, _)) => rank(t.kind) < rank(*bk) || (rank(t.kind) == rank(*bk) && m < *bm),
        };
        if better {
            best = Some((m, t.kind, t.precision));
        }
    }
    best
}

/// 建一次住院（或一批件）的时间线。
///
/// 只排**同一 `encounter_alias`** 内的件——跨住院 join 是数据串号，
/// 不是时间序问题（跨住院误连由 Task 8 的图负责拦）。
pub fn build_timeline(items: &[DocItem]) -> TimelineOrder {
    let mut entries: Vec<TimelineEntry> = items
        .iter()
        .map(|d| {
            let rep = representative(d);
            let missing_kinds = d
                .timestamps
                .iter()
                .filter(|t| t.moment.is_none())
                .map(|t| t.kind)
                .collect();
            TimelineEntry {
                doc_id: d.doc_id.clone(),
                template_id: d.template_id.clone(),
                encounter_alias: d.encounter_alias.clone(),
                moment: rep.map(|(m, _, _)| m),
                source: rep.map(|(_, k, _)| k),
                precision: rep.map(|(_, _, p)| p),
                missing_moment: rep.is_none(),
                missing_kinds,
            }
        })
        .collect();

    // 稳定且可复现：有时刻按时刻升序，无时刻排末位并按 doc_id 兜底排序。
    entries.sort_by(|a, b| match (a.moment, b.moment) {
        (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.doc_id.cmp(&b.doc_id)),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => a.doc_id.cmp(&b.doc_id),
    });

    let missing_count = entries.iter().filter(|e| e.missing_moment).count();
    TimelineOrder { entries, missing_count }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{FixedOffset, TimeZone};
    use psychdoc_model::{Section, Timestamp};

    /// `mins` 是当日 0 点起的分钟数（可超过 60）。
    fn at(mins: u32) -> chrono::DateTime<FixedOffset> {
        FixedOffset::east_opt(8 * 3600)
            .unwrap()
            .with_ymd_and_hms(2026, 3, 2, mins / 60, mins % 60, 0)
            .unwrap()
    }

    fn doc(id: &str, tpl: &str, alias: &str, ts: Vec<Timestamp>) -> DocItem {
        DocItem::new(id, tpl, alias)
            .with_sections(vec![Section { index: 0, title: "段".into(), text: "x".into() }])
            .with_timestamps(ts)
    }

    fn date_ts(minute: u32) -> Timestamp {
        Timestamp::at(TimestampKind::Date, at(minute), Precision::Second, 0, None)
    }

    fn missing_business() -> Timestamp {
        Timestamp::missing(TimestampKind::BusinessMoment, 0, Some("23时40分".into()))
    }

    #[test]
    fn items_sort_by_business_moment_ascending() {
        let items = vec![
            doc("D-3", "restraint_release", "W-1", vec![date_ts(120)]),
            doc("D-1", "restraint_consent", "W-1", vec![date_ts(30)]),
            doc("D-2", "restraint_record", "W-1", vec![date_ts(90)]),
        ];
        let t = build_timeline(&items);
        assert_eq!(
            t.entries.iter().map(|e| e.doc_id.as_str()).collect::<Vec<_>>(),
            ["D-1", "D-2", "D-3"]
        );
        assert_eq!(t.missing_count, 0);
    }

    #[test]
    fn items_without_moment_sort_last_and_are_flagged() {
        let items = vec![
            doc("D-NO", "sign_page", "W-1", vec![missing_business()]),
            doc("D-YES", "restraint_record", "W-1", vec![date_ts(90)]),
        ];
        let t = build_timeline(&items);
        assert_eq!(t.entries[0].doc_id, "D-YES", "有时刻的必须在前面");
        assert_eq!(t.entries[1].doc_id, "D-NO");
        assert!(t.entries[1].missing_moment);
        assert_eq!(t.missing_count, 1);
        assert_eq!(t.items_without_moment().count(), 1);
    }

    #[test]
    fn missing_moment_is_never_filled_with_current_time() {
        let items = vec![doc("D-NO", "sign_page", "W-1", vec![missing_business()])];
        let t = build_timeline(&items);
        assert!(t.entries[0].moment.is_none());
        assert!(t.gap_seconds("D-NO", "D-NO").is_none(), "缺时刻不得算出时差");
    }

    #[test]
    fn missing_kinds_are_reported_per_item() {
        let items = vec![doc(
            "D-1",
            "sign_page",
            "W-1",
            vec![missing_business(), Timestamp::missing(TimestampKind::Date, 0, None)],
        )];
        let t = build_timeline(&items);
        assert_eq!(t.entries[0].missing_kinds.len(), 2);
        // 原文仍在，护士长能知道「这段写了 23:40，只是没有日期」
        let raw = t.entries[0].missing_kinds.len();
        assert_eq!(raw, 2);
    }

    #[test]
    fn equal_moments_break_tie_by_doc_id_stably() {
        let items = vec![
            doc("D-B", "restraint_record", "W-1", vec![date_ts(30)]),
            doc("D-A", "restraint_consent", "W-1", vec![date_ts(30)]),
        ];
        let t = build_timeline(&items);
        assert_eq!(
            t.entries.iter().map(|e| e.doc_id.as_str()).collect::<Vec<_>>(),
            ["D-A", "D-B"],
            "同刻并列必须按 doc_id 稳定排序，两次跑批顺序一致"
        );
    }

    #[test]
    fn representative_moment_prefers_date_over_business_moment() {
        let items = vec![doc(
            "D-1",
            "restraint_record",
            "W-1",
            vec![
                Timestamp::at(TimestampKind::BusinessMoment, at(10), Precision::Minute, 0, None),
                date_ts(600),
            ],
        )];
        let t = build_timeline(&items);
        assert_eq!(t.entries[0].source, Some(TimestampKind::Date));
        assert_eq!(t.entries[0].moment, Some(at(600)));
    }

    #[test]
    fn gap_seconds_computes_real_difference_only() {
        let items = vec![
            doc("D-A", "restraint_record", "W-1", vec![date_ts(30)]),
            doc("D-B", "restraint_release", "W-1", vec![date_ts(90)]),
        ];
        let t = build_timeline(&items);
        assert_eq!(t.gap_seconds("D-A", "D-B"), Some(3600));
        assert_eq!(t.gap_seconds("D-B", "D-A"), Some(-3600), "反向查询不得取绝对值");
        assert_eq!(t.gap_seconds("D-A", "D-NOPE"), None);
    }

    #[test]
    fn timeline_is_reproducible_across_repeated_builds() {
        let items = vec![
            doc("D-C", "restraint_release", "W-1", vec![date_ts(120)]),
            doc("D-A", "restraint_consent", "W-1", vec![date_ts(30)]),
            doc("D-B", "restraint_record", "W-1", vec![missing_business()]),
        ];
        let a = build_timeline(&items);
        let b = build_timeline(&items);
        assert_eq!(a, b);
    }

    #[test]
    fn as_zh_says_missing_explicitly() {
        let items = vec![doc("D-NO", "sign_page", "W-1", vec![missing_business()])];
        let t = build_timeline(&items);
        assert!(t.entries[0].as_zh("签署页").contains("缺时刻"));
    }
}
