//! 跨文书时间序 join：事件时间线、并列容差、精度降级与缺时刻降级。
//!
//! ## 为什么要有这一层
//!
//! Task 7 的执行器 B 已经能把同一次住院的多件 join 到一起判 `QC-PSY-05`~`07`，
//! 但那版判定是**二元**的：两个时刻一比，要么「顺序成立」，要么「倒挂」。
//! 真实院内的跨文书比对有四类难例，二元判定在其中三类上会给出**错误结论**：
//!
//! | 难例 | 现场样子 | 二元判定会怎样 | 本层的处置 |
//! |---|---|---|---|
//! | **并列容差** | 解除登记与约束起始在同一班次内前后差 3 分钟 | 报「时序倒挂」，但这在纸上就是正常的补记顺序 | 件对可配 0/1/5 分钟容差；容差内**不判违规**，但记 [`ToleratedPair`] 不静默放过 |
//! | **精度不一致** | 签署页只写了「2026-03-11」一个日期，约束起始写到分 | 拿 00:00 去比 08:00，凭空造出「先约束后知情」 | 区间算术：低精度侧**只能用于确认「不早于」成立，不能用来认定倒挂** → `insufficient_precision` 交人工 |
//! | **跨零点** | 约束 23:40 开始、次日 06:10 解除 | 按「当日时分」比会得到 −17 小时 30 分 | 日期与时刻分列存储，比较一律走**真实时间差** |
//! | **缺时刻** | 段里只写了「23时45分」没有日期 | 兜底成当前时间 → 编出一条「看起来正常」的结论 | 任一侧缺时刻即整条跳过并记原因，**不兜底** |
//!
//! ## 「不早于」单向判断（本层最要紧的一条）
//!
//! 院内文书的时间常常只写到日或小时。这类时刻的**真实值落在一个区间里**
//! 而不是某个点上：`2026-03-11`（日精度）真实落在 `[03-11 00:00, 03-12 00:00)`。
//! 因此：
//!
//! - 拿它去确认「b **不早于** a」是安全的——区间下界已经越过了 a 的上界，
//!   无论真实值取在哪里，结论都成立；
//! - 拿它去认定「b **早于** a」是不安全的——那等于替文书作者猜了一个他没写的时刻。
//!
//! 所以 [`judge`] 在**要判违规**的那一侧出现低精度时刻时，一律返回
//! [`Chronology::Undecidable`] / [`SkipReason::InsufficientPrecision`]：
//! **宁可交人工，也不用一条粗时刻冤枉一份病历。**
//!
//! ## 与源产品的关系
//!
//! 源产品在这条主路径上提供的是「按稳定顺序收集 → 解析 → 批量执行 → 逐条给位置」，
//! 本层沿用它的**可观察行为**（顺序稳定、每条判定都带原因、判不出来不静默），
//! 判定内核（区间算术 + 并列容差 + 精度降级）为本轮新建。

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, FixedOffset};
use psychdoc_model::{DocItem, Precision, Span, TimestampKind};
use serde::{Deserialize, Serialize};

use crate::ruleset::{EffectiveRules, LayerId};

/// 时刻精度对应的**区间宽度**（秒）。
///
/// 精度为 `P`、值为 `t` 的时刻，真实值落在半开区间 `[t, t + width(P))` 内。
/// 院内文书实际会出现的最粗粒度是「日」（签署页只印日期），因此这个宽度
/// 决定了「能不能拿它去判违规」。
pub fn precision_width(p: Precision) -> i64 {
    match p {
        Precision::Second => 1,
        Precision::Minute => 60,
        Precision::Hour => 3_600,
        Precision::Day => 86_400,
    }
}

/// 一个时刻及其**可信区间**。
///
/// `at` 是文书里写出来的字面值（也是高亮的落点），
/// `[lo, hi)` 是它真实取值可能落入的区间。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Moment {
    pub at: DateTime<FixedOffset>,
    pub precision: Precision,
}

impl Moment {
    pub fn new(at: DateTime<FixedOffset>, precision: Precision) -> Self {
        Self { at, precision }
    }

    pub fn lo(&self) -> DateTime<FixedOffset> {
        self.at
    }

    pub fn hi(&self) -> DateTime<FixedOffset> {
        self.at + chrono::Duration::seconds(precision_width(self.precision))
    }

    /// 是否**低精度**（粗于分钟）。
    ///
    /// 院内业务时刻（`2026-03-04 08:00`）最细到分，文书级时间戳最细到秒；
    /// 粗于分钟意味着文书作者**没有写下具体时刻**，它只能用来放行，不能用来定罪。
    pub fn is_coarse(&self) -> bool {
        self.precision.is_coarser_than(Precision::Minute)
    }

    /// 真实时间差（`self - other`，秒）。**跨零点走真实时间差**，
    /// 不按「当日时分」相减——那样 23:40 → 次日 06:10 会算成 −17 小时 30 分。
    pub fn real_gap_seconds(&self, other: &Moment) -> i64 {
        (self.at - other.at).num_seconds()
    }

    pub fn as_zh(&self) -> String {
        self.at.format("%Y-%m-%d %H:%M").to_string()
    }
}

/// 时间序判定为什么没结论。**跳过必须带原因**——「这条为什么没报」
/// 是院内最常追问、又最难自己查出来的问题。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkipReason {
    /// 参与判定的对照件不在本次包里（缺件由归档核齐依赖图负责，见 Task 8）。
    NoCounterpart,
    /// 参与判定的任一件绑定不唯一（跨系统同名文书 / 未找到模板）。
    AmbiguousBinding,
    /// 任一侧抽不出时刻：绝不用当前时间或零值兜底。
    MissingTimestamp,
    /// 两个时刻完全相等：分不出先后，不报也不放过，记下来交人工。
    EqualMoment,
    /// 两个时刻的精度区间重叠，或低精度侧被要求去认定倒挂：判了就是在猜。
    InsufficientPrecision,
}

impl SkipReason {
    pub fn as_zh(self) -> &'static str {
        match self {
            Self::NoCounterpart => "对照件不在本次文书包内",
            Self::AmbiguousBinding => "参与判定的文书绑定不唯一",
            Self::MissingTimestamp => "有一侧抽不出时刻",
            Self::EqualMoment => "两个时刻完全相等，分不出先后",
            Self::InsufficientPrecision => "时刻精度不足以分辨先后",
        }
    }

    /// 是否属于「**看起来像**倒挂但其实不能判」的一类——这类最危险，
    /// 因为它们一旦被静默放过，事后复核时看不出这里当初没判。
    pub fn is_inconclusive(self) -> bool {
        matches!(self, Self::EqualMoment | Self::InsufficientPrecision)
    }
}

/// 一次比对的结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub enum Chronology {
    /// 顺序成立：`b` 确实不早于（或不晚于）`a`。
    Clear {
        gap_seconds: i64,
        /// 判定所依据的区间关系，写进跳过/容差记录让人能复核。
        basis: &'static str,
    },
    /// 顺序确实倒了，但差距落在该件对的**并列容差**内 → **不判违规**。
    ///
    /// 仍然记账：容差是规则集里的一个业务决定（「同班次内先填解除、后补起始」
    /// 属正常补记），事后要能回答「这一条为什么没报」。
    Tolerated {
        gap_seconds: i64,
        tolerance_minutes: i64,
        reason: String,
    },
    /// 明确倒挂，且两侧精度都足以支撑这个结论。
    Inverted { gap_seconds: i64 },
    /// 判不了，原因必须带出去。
    Undecidable {
        reason: SkipReason,
        detail: String,
    },
}

impl Chronology {
    pub fn is_violation(&self) -> bool {
        matches!(self, Self::Inverted { .. })
    }

    pub fn is_clear(&self) -> bool {
        matches!(self, Self::Clear { .. })
    }
}

/// `b` 相对 `a` 应满足的先后方向。
///
/// 院内三条跨文书时序规则的口径都是「某件事**不得**早于/晚于某件事」，
/// 因此方向是规则的属性，不是时刻的属性。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// `b` 不得早于 `a`（解除不得早于约束开始、患方签署不得晚于约束开始）。
    NotEarlier,
    /// `b` 不得晚于 `a`（评估不得晚于首次约束）。
    NotLater,
}

impl Direction {
    pub fn as_zh(self) -> &'static str {
        match self {
            Self::NotEarlier => "不得早于",
            Self::NotLater => "不得晚于",
        }
    }
}

/// 一个件对的并列容差。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tolerance {
    pub minutes: i64,
    /// 院内为什么给这个件对开这个口子。没有理由的容差不允许加载。
    pub reason: String,
    /// 由哪一层声明（院 → 科室 → 个人）。
    #[serde(default = "default_tolerance_source")]
    pub source: LayerId,
}

fn default_tolerance_source() -> LayerId {
    LayerId::Hospital
}

impl Tolerance {
    /// 规则集未对该件对配置容差时的默认口径：**0 分钟，从严判定**。
    pub fn strict() -> Self {
        Self {
            minutes: 0,
            reason: "规则集未对该件对配置并列容差，按 0 分钟从严判定".to_string(),
            source: LayerId::Hospital,
        }
    }

    pub fn seconds(&self) -> i64 {
        self.minutes * 60
    }
}

/// 院内只承认 0 / 1 / 5 三档并列容差。
///
/// 档位固定是刻意的：容差一旦可以随手写 30 分钟，跨文书时序判定就等于关掉了，
/// 而且这个决定会藏在规则集里没人发现。超出档位一律
/// [`crate::ruleset::RuleSetError::InvalidTieTolerance`] 拒绝加载。
pub const ALLOWED_TOLERANCE_MINUTES: [i64; 3] = [0, 1, 5];

/// 按**件对**（无序）配置的并列容差表。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToleranceTable {
    /// 未命中任何件对时的兜底容差（交付规则集固定为 0）。
    pub default_minutes: i64,
    entries: BTreeMap<(String, String), Tolerance>,
}

impl ToleranceTable {
    /// 新建一张表，兜底档位由调用方给（加载期校验过必须落在院内档位内）。
    pub fn with_default(default_minutes: i64) -> Self {
        Self { default_minutes, entries: BTreeMap::new() }
    }

    fn key(a: &str, b: &str) -> (String, String) {
        if a <= b {
            (a.to_string(), b.to_string())
        } else {
            (b.to_string(), a.to_string())
        }
    }

    /// 生效容差：命中件对用件对配置，否则用兜底。
    pub fn effective(&self, a_tpl: &str, b_tpl: &str) -> Tolerance {
        self.entries
            .get(&Self::key(a_tpl, b_tpl))
            .cloned()
            .unwrap_or_else(|| Tolerance {
                minutes: self.default_minutes,
                reason: "规则集未对该件对配置并列容差，按默认档从严判定".to_string(),
                source: LayerId::Hospital,
            })
    }

    pub fn declared(&self, a_tpl: &str, b_tpl: &str) -> Option<&Tolerance> {
        self.entries.get(&Self::key(a_tpl, b_tpl))
    }

    pub fn insert(&mut self, a_tpl: &str, b_tpl: &str, tol: Tolerance) {
        self.entries.insert(Self::key(a_tpl, b_tpl), tol);
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 逐条列出，供规则集溯源展示。
    pub fn iter(&self) -> impl Iterator<Item = (&(String, String), &Tolerance)> {
        self.entries.iter()
    }
}

/// 落在并列容差内、因此**未判违规**的一对件。
///
/// 与 [`crate::cross_rules::SkippedCheck`] 的区别：跳过是「判不出来」，
/// 容差内是「判出来了、结论是不算违规」。两者都不得静默消失，
/// 但它们回答的是现场两个不同的问题。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToleratedPair {
    pub code: String,
    pub encounter_alias: String,
    /// 触发判定的件（基准侧）。
    pub primary_doc: String,
    /// 被比较的件。
    pub counterpart_doc: String,
    pub gap_seconds: i64,
    pub tolerance_minutes: i64,
    pub reason: String,
    pub source: LayerId,
    pub detail: String,
}

impl ToleratedPair {
    pub fn as_zh(&self) -> String {
        format!(
            "{} · {} · 两件相差 {} 分钟，在 {} 分钟并列容差内未判倒挂（{}）",
            self.code,
            self.encounter_alias,
            // 取绝对值：现场读的是「差了几分钟」，负号只会让人以为算错了。
            self.gap_seconds.abs() / 60,
            self.tolerance_minutes,
            self.reason
        )
    }
}

/// 一次跨文书时间序判定。`a` 为基准侧，`b` 为被比较侧。
pub fn judge(a: &Moment, b: &Moment, direction: Direction, tolerance: &Tolerance) -> Chronology {
    let gap = b.real_gap_seconds(a);

    // 完全相等：分不出先后。**与容差无关**——并列容差回答的是
    // 「差了几分钟还算不算倒挂」，而完全相等是另一个问题：
    // 同一分钟里的签名与记录谁先谁后，纸面上就是判不出来。
    if gap == 0 {
        return Chronology::Undecidable {
            reason: SkipReason::EqualMoment,
            detail: format!("两个时刻完全相等（{}），分不出先后", a.as_zh()),
        };
    }

    // 区间算术：只有两个区间**完全分开**时，先后才是确定的。
    let b_clearly_later = b.lo() >= a.hi();
    let b_clearly_earlier = b.hi() <= a.lo();
    if !b_clearly_later && !b_clearly_earlier {
        return Chronology::Undecidable {
            reason: SkipReason::InsufficientPrecision,
            detail: format!(
                "两侧精度区间相交（{} 的区间宽 {} 秒，{} 的区间宽 {} 秒），分不出先后",
                a.as_zh(),
                precision_width(a.precision),
                b.as_zh(),
                precision_width(b.precision)
            ),
        };
    }

    let violates = match direction {
        Direction::NotEarlier => b_clearly_earlier,
        Direction::NotLater => b_clearly_later,
    };
    if !violates {
        return Chronology::Clear {
            gap_seconds: gap,
            basis: "两个时刻的精度区间完全分开，先后可判",
        };
    }

    // —— 要判违规了。——
    //
    // 低精度时刻（粗于分钟）只能用于确认「不早于 / 不晚于」成立，
    // 不能用来认定倒挂：那等于替文书作者猜一个他没写下的时刻，
    // 再拿这个猜出来的时刻去给一份病历记一条时序倒挂。
    if a.is_coarse() || b.is_coarse() {
        let coarse_side = if a.is_coarse() { "基准侧" } else { "被比较侧" };
        return Chronology::Undecidable {
            reason: SkipReason::InsufficientPrecision,
            detail: format!(
                "{coarse_side}只写到{}，真实时刻可能落在 {} ~ {} 之间；\
                 低精度时刻只能用于确认「不早于」成立，不能用来认定倒挂，本条交人工",
                if a.is_coarse() { precision_zh(a.precision) } else { precision_zh(b.precision) },
                coarse_lo(a, b),
                coarse_hi(a, b)
            ),
        };
    }

    if tolerance.minutes > 0 && gap.abs() <= tolerance.seconds() {
        return Chronology::Tolerated {
            gap_seconds: gap,
            tolerance_minutes: tolerance.minutes,
            reason: tolerance.reason.clone(),
        };
    }

    Chronology::Inverted { gap_seconds: gap }
}

fn precision_zh(p: Precision) -> &'static str {
    match p {
        Precision::Second => "秒",
        Precision::Minute => "分",
        Precision::Hour => "小时",
        Precision::Day => "日",
    }
}

fn coarse_lo(a: &Moment, b: &Moment) -> String {
    let m = if a.is_coarse() { a } else { b };
    m.lo().format("%Y-%m-%d %H:%M").to_string()
}

fn coarse_hi(a: &Moment, b: &Moment) -> String {
    let m = if a.is_coarse() { a } else { b };
    m.hi().format("%Y-%m-%d %H:%M").to_string()
}

// ---------------------------------------------------------------------------
// 事件时间线
// ---------------------------------------------------------------------------

/// 件类型在「同刻并列」时的排序优先级。数值小者在前。
pub type TypePriority = BTreeMap<String, usize>;

/// 由规则集的文书依赖规范推出的件类型优先级。
///
/// 依赖边 `from → to` 的含义是「`from` 这件事应当发生在 `to` 之前」，
/// 因此对依赖图做一次**确定性拓扑排序**（Kahn + `BTreeSet` 就绪队列）
/// 就得到优先级：任何依赖边两端都满足 `rank(from) < rank(to)`，且同一张图
/// 两次跑出的顺序逐项一致。
///
/// 未被任何依赖边约束的类型与已排序的类型按类型码升序**交错**（不是一律排最后）——
/// 它们之间本来就没有先后关系，唯一的诉求是可复现。
/// 依赖成环时环内类型按类型码升序追加，同样可复现。
pub fn type_priority(ruleset: &EffectiveRules) -> TypePriority {
    let names: Vec<String> = ruleset.doc_types.keys().cloned().collect();
    let mut outgoing: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut indegree: BTreeMap<String, usize> = names.iter().map(|n| (n.clone(), 0)).collect();
    for d in &ruleset.dependencies {
        // 依赖边引用了未定义的文书类型 → 该边不参与排序（加载期已拒绝）。
        if !indegree.contains_key(&d.from) || !indegree.contains_key(&d.to) || d.from == d.to {
            continue;
        }
        if outgoing.entry(d.from.clone()).or_default().insert(d.to.clone()) {
            *indegree.get_mut(&d.to).expect("已判定存在") += 1;
        }
    }

    let mut rank: TypePriority = BTreeMap::new();
    let mut ready: BTreeSet<String> =
        indegree.iter().filter(|(_, &n)| n == 0).map(|(k, _)| k.clone()).collect();
    while !ready.is_empty() {
        let Some(name) = ready.iter().next().cloned() else { break };
        ready.remove(&name);
        if rank.contains_key(&name) {
            continue;
        }
        let next_rank = rank.len();
        rank.insert(name.clone(), next_rank);
        if let Some(kids) = outgoing.get(&name) {
            for kid in kids {
                if let Some(n) = indegree.get_mut(kid) {
                    *n = n.saturating_sub(1);
                    if *n == 0 {
                        ready.insert(kid.clone());
                    }
                }
            }
        }
    }
    // 环内或孤立类型：按类型码升序追加，保证结果可复现。
    let mut next = rank.len();
    for name in names {
        if rank.contains_key(&name) {
            continue;
        }
        rank.insert(name, next);
        next += 1;
    }
    rank
}

/// 时间线上的一条事件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimelineEvent {
    pub doc_id: String,
    pub template_id: String,
    pub encounter_alias: String,
    pub kind: TimestampKind,
    pub at: DateTime<FixedOffset>,
    pub precision: Precision,
    /// 落回原文的定位：段序号 + 段内字符区间（文书级时间戳无段内区间）。
    pub section_index: u32,
    pub char_range: Option<Span>,
    /// 件类型优先级（同刻并列时的排序依据）。
    pub type_rank: usize,
}

/// 一次住院的事件时间线。
///
/// 排序键固定为 `(时刻, 件类型优先级, doc_id, 时间戳类别, 段序号, 段内起点)`，
/// 因此同一批文书两次重建的结果逐字段相同——这是「同一时刻两件怎么排」
/// 能被复核的前提。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EventTimeline {
    pub encounter_alias: String,
    pub events: Vec<TimelineEvent>,
    /// 整件抽不出任何时刻的件：**显式列出**，不静默从时间线上消失。
    pub undated: Vec<String>,
}

impl EventTimeline {
    pub fn build(alias: &str, items: &[&DocItem], priority: &TypePriority) -> Self {
        let rank_of =
            |tpl: &str| priority.get(tpl).copied().unwrap_or(usize::MAX);
        let mut events: Vec<TimelineEvent> = Vec::new();
        let mut undated: Vec<String> = Vec::new();
        for item in items {
            let mut any = false;
            for ts in &item.timestamps {
                let Some(at) = ts.moment else { continue };
                any = true;
                events.push(TimelineEvent {
                    doc_id: item.doc_id.clone(),
                    template_id: item.template_id.clone(),
                    encounter_alias: item.encounter_alias.clone(),
                    kind: ts.kind,
                    at,
                    precision: ts.precision,
                    section_index: ts.section_index,
                    char_range: ts.char_range,
                    type_rank: rank_of(&item.template_id),
                });
            }
            if !any {
                undated.push(item.doc_id.clone());
            }
        }
        events.sort_by(|x, y| {
            x.at
                .cmp(&y.at)
                .then_with(|| x.type_rank.cmp(&y.type_rank))
                .then_with(|| x.doc_id.cmp(&y.doc_id))
                .then_with(|| kind_rank(x.kind).cmp(&kind_rank(y.kind)))
                .then_with(|| x.section_index.cmp(&y.section_index))
                .then_with(|| {
                    x.char_range.map(|s| s.start).unwrap_or(0).cmp(&y.char_range.map(|s| s.start).unwrap_or(0))
                })
        });
        undated.sort();
        Self { encounter_alias: alias.to_string(), events, undated }
    }

    /// 缺时刻的件：供「缺什么」清单直接渲染。
    pub fn items_without_moment(&self) -> &[String] {
        &self.undated
    }

    /// 时间线的一行行中文渲染（CLI 与 UI 共用同一份口径）。
    pub fn as_zh_rows(&self) -> Vec<String> {
        let mut rows: Vec<String> = self
            .events
            .iter()
            .map(|e| {
                format!(
                    "{}  {}  {}  {}",
                    e.at.format("%Y-%m-%d %H:%M"),
                    kind_zh(e.kind),
                    e.doc_id,
                    if e.precision.is_coarser_than(Precision::Minute) {
                        format!("（只写到{}，不参与判违规）", precision_zh(e.precision))
                    } else {
                        String::new()
                    }
                )
            })
            .collect();
        for doc in &self.undated {
            rows.push(format!("（无时刻）  {doc}  整件抽不出任何时刻，时间序相关检查已跳过"));
        }
        rows
    }
}

/// 时间戳类别的排序权重。`TimestampKind` 不派生 `Ord`（它是模型层的公共类型，
/// 排序口径属于本层），因此这里给一个显式的院内顺序：
/// 文书日期 → 业务时刻 → 书写时刻。同一件同一时刻内先看文书本身的日期。
fn kind_rank(k: TimestampKind) -> u8 {
    match k {
        TimestampKind::Date => 0,
        TimestampKind::BusinessMoment => 1,
        TimestampKind::AuthoredOn => 2,
    }
}

fn kind_zh(k: TimestampKind) -> &'static str {
    match k {
        TimestampKind::AuthoredOn => "书写时刻",
        TimestampKind::Date => "文书日期",
        TimestampKind::BusinessMoment => "业务时刻",
    }
}

// ---------------------------------------------------------------------------
// 从件上取参与比对的时刻
// ---------------------------------------------------------------------------

/// 取「落在 `span` 之内的那条段内业务时刻」。
///
/// 刻意**不用**「这件最早的 `BusinessMoment`」：约束记录里除了「约束起始时间」
/// 还可能写着巡视时刻、解除时刻，取最早的那个会让判定落在不相干的字上。
/// 定位与取值必须来自**同一个字段**，护士长点开高亮才看得到被判的那个值。
pub fn moment_within(item: &DocItem, span: Span) -> Option<Moment> {
    item.timestamps
        .iter()
        .filter(|t| t.kind == TimestampKind::BusinessMoment)
        .find(|t| t.char_range.is_some_and(|r| span.start <= r.start && r.end <= span.end))
        .and_then(|t| t.moment.map(|at| (at, t.precision)))
        .map(|(at, precision)| Moment::new(at, precision))
}

/// 该件第一个有时刻的**文书级**时间戳（`authoredOn` 优先，其次 `date`）。
pub fn document_moment(item: &DocItem, kind: TimestampKind) -> Option<Moment> {
    item.timestamps
        .iter()
        .find(|t| t.kind == kind && t.moment.is_some())
        .and_then(|t| t.moment.map(|at| (at, t.precision)))
        .map(|(at, precision)| Moment::new(at, precision))
}

/// `authoredOn` 是文书级时刻，没有段内字符区间。取**最早的有时刻时间戳**所在段，
/// 让定位至少落在该件真实存在内容的地方，而不是凭空指向第 0 段第 0 字。
pub fn document_moment_section(item: &DocItem, kind: TimestampKind) -> u32 {
    item.timestamps
        .iter()
        .filter(|t| t.moment.is_some())
        .filter(|t| t.kind == kind || t.kind == TimestampKind::Date)
        .map(|t| t.section_index)
        .next()
        .unwrap_or(0)
}

/// 供夹具与测试直接构造一个时刻。接受到分（`… 08:00`）与到秒（`… 08:00:30`）
/// 两种字面量；精度由调用方**显式**给出，不从字面量反推——
/// 精度是判定的输入（能不能定罪），必须由声明它的那个模块说清楚。
pub fn moment(raw: &str, precision: Precision) -> Option<Moment> {
    let naive = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M")
        .or_else(|_| chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S"))
        .ok()?;
    let offset = FixedOffset::east_opt(8 * 3600).expect("UTC+8 恒有效");
    chrono::TimeZone::from_local_datetime(&offset, &naive)
        .single()
        .map(|at| Moment::new(at, precision))
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Timestamp;

    const P: Precision = Precision::Minute;

    fn m(raw: &str) -> Moment {
        moment(raw, P).expect("测试时刻必须可解析")
    }

    fn coarse(raw: &str) -> Moment {
        moment(raw, Precision::Day).expect("测试时刻必须可解析")
    }

    // —— 缺时刻：绝不兜底 ——

    #[test]
    fn a_missing_timestamp_never_becomes_a_number() {
        // 解析层把「23时45分」记成 moment=None；本层拿到 None 就该拒绝判定。
        let ts = Timestamp::missing(TimestampKind::BusinessMoment, 0, Some("23时45分".into()));
        assert!(ts.moment.is_none());
        assert!(ts.is_missing());
        // 没有 Moment 可构造 → 上层只能记 skipped。
        let item = DocItem::new("D-1", "restraint_record", "W-1").with_timestamps(vec![ts]);
        assert!(moment_within(&item, Span::unchecked(0, 100)).is_none());
    }

    // —— 跨零点：按真实时间差，不按当日时分 ——

    #[test]
    fn across_midnight_the_gap_is_real_elapsed_time() {
        let start = m("2026-03-02 23:40");
        let release = m("2026-03-03 06:10");
        assert_eq!(release.real_gap_seconds(&start), 23_400, "真实时间差是 6 小时 30 分");
        // 反向查询不得取绝对值。
        assert_eq!(start.real_gap_seconds(&release), -23_400);
    }

    #[test]
    fn a_same_clock_comparison_would_have_given_the_wrong_answer() {
        // 这就是「按当日时分比」会踩的坑：两个时刻的时分差是负的，
        // 真实时间差是正的。守住这个对照，跨零点就不会被改回当日比较。
        let start = m("2026-03-02 23:40");
        let release = m("2026-03-03 06:10");
        let same_clock_minutes = 6 * 60 + 10 - (23 * 60 + 40);
        assert!(same_clock_minutes < 0, "当日时分差为负，是错误口径");
        assert!(release.real_gap_seconds(&start) > 0, "真实时间差为正");
    }

    #[test]
    fn a_cross_midnight_release_is_not_reported_as_inverted() {
        let start = m("2026-03-02 23:40");
        let release = m("2026-03-03 06:10");
        let v = judge(&start, &release, Direction::NotEarlier, &Tolerance::strict());
        assert!(v.is_clear(), "{v:?}");
    }

    // —— 并列容差 ——

    #[test]
    fn a_three_minute_inversion_is_a_violation_without_a_tolerance() {
        let start = m("2026-03-04 08:00");
        let release = m("2026-03-04 07:57");
        let v = judge(&start, &release, Direction::NotEarlier, &Tolerance::strict());
        assert_eq!(v, Chronology::Inverted { gap_seconds: -180 });
    }

    #[test]
    fn the_same_pair_is_tolerated_when_the_ruleset_opens_five_minutes() {
        let start = m("2026-03-04 08:00");
        let release = m("2026-03-04 07:57");
        let tol = Tolerance {
            minutes: 5,
            reason: "同班次内先填解除、后补约束起始属正常补记".into(),
            source: LayerId::Hospital,
        };
        let v = judge(&start, &release, Direction::NotEarlier, &tol);
        assert_eq!(
            v,
            Chronology::Tolerated {
                gap_seconds: -180,
                tolerance_minutes: 5,
                reason: "同班次内先填解除、后补约束起始属正常补记".into(),
            }
        );
        assert!(!v.is_violation(), "容差内不得判违规");
    }

    #[test]
    fn a_tolerance_does_not_swallow_an_inversion_beyond_its_window() {
        let start = m("2026-03-04 08:00");
        let release = m("2026-03-04 07:50");
        let tol = Tolerance { minutes: 5, reason: "容差 5 分钟".into(), source: LayerId::Hospital };
        let v = judge(&start, &release, Direction::NotEarlier, &tol);
        assert_eq!(v, Chronology::Inverted { gap_seconds: -600 }, "超出容差仍是倒挂");
    }

    #[test]
    fn a_zero_tolerance_is_exactly_strict() {
        let start = m("2026-03-04 08:00");
        let release = m("2026-03-04 07:59");
        let strict = Tolerance::strict();
        let zero = Tolerance { minutes: 0, reason: "显式配 0 分钟".into(), source: LayerId::Hospital };
        assert_eq!(judge(&start, &release, Direction::NotEarlier, &strict), judge(&start, &release, Direction::NotEarlier, &zero));
    }

    #[test]
    fn tolerance_is_symmetric_in_the_pair_but_the_direction_still_decides() {
        let mut table = ToleranceTable::with_default(0);
        table_insert(&mut table, 5);
        // 件对无序：正着查、反着查拿到同一条容差（谁写在前面都一样）。
        assert_eq!(table.effective("restraint_record", "restraint_release").minutes, 5);
        assert_eq!(table.effective("restraint_release", "restraint_record").minutes, 5);
        // 但**方向**由规则决定，容差不改方向：把同一对时刻换到「评估不得晚于
        // 约束开始」的口径下，08:00 晚于 07:57 就是违规，且不在 5 分钟容差外——
        // 它是 3 分钟，落在容差内 → 仍不判违规，但记账类型是 Tolerated 而非 Clear。
        let start = m("2026-03-04 07:57");
        let release = m("2026-03-04 08:00");
        let tol = table.effective("restraint_record", "restraint_release");
        assert!(matches!(judge(&start, &release, Direction::NotEarlier, &tol), Chronology::Clear { .. }));
        assert!(matches!(judge(&start, &release, Direction::NotLater, &tol), Chronology::Tolerated { .. }));
        // 同一对时刻、同一份容差，两个方向给出两个不同结论：方向确实是规则的属性。
        assert_ne!(
            judge(&start, &release, Direction::NotEarlier, &tol),
            judge(&start, &release, Direction::NotLater, &tol)
        );
    }

    fn table_insert(table: &mut ToleranceTable, minutes: i64) {
        table.insert(
            "restraint_record",
            "restraint_release",
            Tolerance { minutes, reason: "补记".into(), source: LayerId::Hospital },
        );
    }

    // —— 精度降级：低精度只参与「不早于」单向判断 ——

    #[test]
    fn a_coarse_side_can_never_convict() {
        // 患方签署只写了日期 2026-03-11，约束起始写到分 03-10 08:00。
        // 按字面值 00:00 去比，凭空造出「先约束、后知情」——这正是要避免的。
        let start = m("2026-03-10 08:00");
        let signed = coarse("2026-03-11 00:00");
        let v = judge(&start, &signed, Direction::NotLater, &Tolerance::strict());
        match v {
            Chronology::Undecidable { reason, detail } => {
                assert_eq!(reason, SkipReason::InsufficientPrecision);
                assert!(detail.contains("不能用来认定倒挂"), "{detail}");
            }
            other => panic!("低精度侧不得定罪，实际 {other:?}"),
        }
    }

    #[test]
    fn a_coarse_side_can_still_clear() {
        // 解除只写到日 03-11，约束起始 03-10 08:00：区间下界已越过基准上界，
        // 无论真实时刻落在那天哪一刻，「解除不早于开始」都成立。
        let start = m("2026-03-10 08:00");
        let release = coarse("2026-03-11 00:00");
        assert!(judge(&start, &release, Direction::NotEarlier, &Tolerance::strict()).is_clear());
    }

    #[test]
    fn an_hour_precision_side_is_also_too_coarse_to_convict() {
        let start = m("2026-03-10 08:00");
        let signed = moment("2026-03-11 09:00", Precision::Hour).unwrap();
        let v = judge(&start, &signed, Direction::NotLater, &Tolerance::strict());
        assert!(matches!(v, Chronology::Undecidable { reason: SkipReason::InsufficientPrecision, .. }), "{v:?}");
    }

    #[test]
    fn overlapping_intervals_are_never_judged() {
        // 基准侧只到分（区间 [08:00, 08:01)），被比较侧到秒且落在这一分钟内。
        let start = m("2026-03-10 08:00");
        let signed = moment("2026-03-10 08:00:30", Precision::Second).unwrap();
        let v = judge(&start, &signed, Direction::NotLater, &Tolerance::strict());
        match v {
            Chronology::Undecidable { reason, detail } => {
                assert_eq!(reason, SkipReason::InsufficientPrecision);
                assert!(detail.contains("相交"), "{detail}");
            }
            other => panic!("区间相交时不得下结论，实际 {other:?}"),
        }
    }

    #[test]
    fn identical_moments_are_equal_moment_not_a_tolerance_hit() {
        // 完全相等是「分不出先后」，与容差无关：容差回答的是「差几分钟还算不算倒挂」。
        let start = m("2026-03-10 08:00");
        let release = m("2026-03-10 08:00");
        let tol = Tolerance { minutes: 5, reason: "容差 5 分钟".into(), source: LayerId::Hospital };
        match judge(&start, &release, Direction::NotEarlier, &tol) {
            Chronology::Undecidable { reason, .. } => assert_eq!(reason, SkipReason::EqualMoment),
            other => panic!("完全相等应记 equal_moment，实际 {other:?}"),
        }
    }

    #[test]
    fn minute_precision_pairs_one_minute_apart_are_still_decidable() {
        // 分精度的区间宽度是 60 秒，恰好相邻的两个分钟不重叠 → 判得了。
        let start = m("2026-03-10 08:00");
        let release = m("2026-03-10 08:01");
        assert!(judge(&start, &release, Direction::NotEarlier, &Tolerance::strict()).is_clear());
        assert!(judge(&release, &start, Direction::NotEarlier, &Tolerance::strict()).is_violation());
    }

    #[test]
    fn skip_reasons_keep_their_stable_machine_readable_names() {
        for (reason, want) in [
            (SkipReason::NoCounterpart, "no_counterpart"),
            (SkipReason::AmbiguousBinding, "ambiguous_binding"),
            (SkipReason::MissingTimestamp, "missing_timestamp"),
            (SkipReason::EqualMoment, "equal_moment"),
            (SkipReason::InsufficientPrecision, "insufficient_precision"),
        ] {
            assert_eq!(serde_json::to_string(&reason).unwrap(), format!("\"{want}\""));
            assert!(!reason.as_zh().is_empty());
        }
        assert!(SkipReason::EqualMoment.is_inconclusive());
        assert!(SkipReason::InsufficientPrecision.is_inconclusive());
        assert!(!SkipReason::MissingTimestamp.is_inconclusive());
    }

    #[test]
    fn tolerated_pair_renders_a_line_a_ward_nurse_can_act_on() {
        let p = ToleratedPair {
            code: "QC-PSY-05".into(),
            encounter_alias: "WARD-M-001".into(),
            primary_doc: "REC".into(),
            counterpart_doc: "REL".into(),
            gap_seconds: -180,
            tolerance_minutes: 5,
            reason: "同班次内先填解除、后补约束起始属正常补记".into(),
            source: LayerId::Hospital,
            detail: String::new(),
        };
        let zh = p.as_zh();
        assert!(zh.contains("QC-PSY-05"), "{zh}");
        assert!(zh.contains("3 分钟"), "差距要换算成分钟：{zh}");
        assert!(zh.contains("5 分钟并列容差"), "{zh}");
    }

    // —— 事件时间线 ——

    fn item(id: &str, tpl: &str, ts: Vec<Timestamp>) -> DocItem {
        DocItem::new(id, tpl, "W-1").with_timestamps(ts)
    }

    fn ts_at(id: &str, tpl: &str, kind: TimestampKind, raw: &str, p: Precision) -> (DocItem, Timestamp) {
        let at = moment(raw, p).unwrap().at;
        let t = Timestamp::at(kind, at, p, 0, None);
        (item(id, tpl, vec![t.clone()]), t)
    }

    #[test]
    fn the_timeline_orders_by_moment_then_type_then_doc_id() {
        let (rec, _) = ts_at("REC", "restraint_record", TimestampKind::BusinessMoment, "2026-03-04 08:00", P);
        let (con, _) = ts_at("CON", "restraint_consent", TimestampKind::BusinessMoment, "2026-03-04 08:00", P);
        let (rel, _) = ts_at("REL", "restraint_release", TimestampKind::BusinessMoment, "2026-03-04 06:10", P);
        let items = [rec, con, rel];
        let refs: Vec<&DocItem> = items.iter().collect();

        // 依赖规范：record → consent，因此同刻时 consent 排在 record 之后。
        let priority: TypePriority = [("restraint_record", 0), ("restraint_consent", 1), ("restraint_release", 2)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
        let tl = EventTimeline::build("W-1", &refs, &priority);
        let order: Vec<&str> = tl.events.iter().map(|e| e.doc_id.as_str()).collect();
        assert_eq!(order, ["REL", "REC", "CON"], "先按时刻，再按件类型优先级");
    }

    #[test]
    fn the_timeline_is_byte_for_byte_reproducible() {
        let (a, _) = ts_at("B", "restraint_release", TimestampKind::BusinessMoment, "2026-03-04 08:00", P);
        let (b, _) = ts_at("A", "restraint_record", TimestampKind::BusinessMoment, "2026-03-04 08:00", P);
        let items = [a, b];
        let refs: Vec<&DocItem> = items.iter().collect();
        let tl1 = EventTimeline::build("W-1", &refs, &TypePriority::new());
        let tl2 = EventTimeline::build("W-1", &refs, &TypePriority::new());
        assert_eq!(tl1, tl2);
    }

    #[test]
    fn an_item_with_no_moment_is_listed_not_dropped() {
        let good = item("D-OK", "restraint_record", vec![Timestamp::at(
            TimestampKind::BusinessMoment,
            moment("2026-03-04 08:00", P).unwrap().at,
            P,
            0,
            None,
        )]);
        let bad = item("D-NO-MOMENT", "sign_page", vec![Timestamp::missing(
            TimestampKind::BusinessMoment,
            0,
            Some("23时45分".into()),
        )]);
        let refs = vec![&good, &bad];
        let tl = EventTimeline::build("W-1", &refs, &TypePriority::new());
        assert_eq!(tl.events.len(), 1);
        assert_eq!(tl.items_without_moment(), ["D-NO-MOMENT"]);
        let rows = tl.as_zh_rows();
        assert!(rows.iter().any(|r| r.contains("整件抽不出任何时刻")), "{rows:?}");
    }

    #[test]
    fn type_priority_follows_the_dependency_order_and_is_stable() {
        let yaml = r#"
version: "prio-1"
templates:
  restraint_record: { display: 约束记录, fields: [] }
  restraint_consent: { display: 约束知情同意, fields: [] }
  restraint_release: { display: 约束解除记录, fields: [] }
  assessment_report: { display: 评估报告, fields: [] }
hospital:
  doc_types:
    restraint_record: "约束记录"
    restraint_consent: "约束知情同意"
    restraint_release: "约束解除记录"
    assessment_report: "评估报告"
  dependencies:
    - from: restraint_record
      to: restraint_consent
    - from: restraint_release
      to: restraint_record
  rules:
    - code: "QC-PSY-05"
      severity: blocking
      basis: "《精神科住院护理常规》第 6.1 条"
"#;
        let rs = crate::load_effective(yaml).unwrap();
        let p1 = type_priority(&rs);
        assert_eq!(type_priority(&rs), p1, "两次必须完全一致");
        assert!(p1["restraint_release"] < p1["restraint_record"], "解除先于约束开始");
        assert!(p1["restraint_record"] < p1["restraint_consent"], "约束开始先于患方知情同意");
        // 未被依赖边约束的类型与已排序类型交错，但顺序可复现。
        assert!(p1["assessment_report"] < p1["restraint_release"], "孤立类型按类型码升序入场：{p1:?}");
        // 唯一的不变量是「依赖边两端 rank 递增」，逐条断言。
        for d in &rs.dependencies {
            assert!(p1[&d.from] < p1[&d.to], "{} → {} 必须递增：{p1:?}", d.from, d.to);
        }
    }

    #[test]
    fn a_dependency_cycle_still_yields_a_deterministic_priority() {
        // 加载期本就会拒绝成环的覆盖配置，但依赖边成环时排序不能死循环。
        let yaml = r#"
version: "prio-cycle"
templates:
  a_doc: { display: 甲, fields: [] }
  b_doc: { display: 乙, fields: [] }
hospital:
  doc_types:
    a_doc: "甲"
    b_doc: "乙"
  dependencies:
    - from: a_doc
      to: b_doc
    - from: b_doc
      to: a_doc
  rules:
    - code: "QC-PSY-05"
      severity: blocking
      basis: "《精神科住院护理常规》第 6.1 条"
"#;
        let rs = crate::load_effective(yaml).unwrap();
        let p = type_priority(&rs);
        assert_eq!(p.len(), 2);
        assert_eq!(type_priority(&rs), p, "成环时也必须可复现");
    }

    #[test]
    fn only_the_three_house_tiers_are_accepted_as_tolerances() {
        for minutes in ALLOWED_TOLERANCE_MINUTES {
            assert!(Tolerance { minutes, reason: "r".into(), source: LayerId::Hospital }.seconds() >= 0);
        }
        assert_eq!(ALLOWED_TOLERANCE_MINUTES, [0, 1, 5]);
    }
}
