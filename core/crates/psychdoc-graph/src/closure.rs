//! 阻塞传播与欠件天数：归档核齐视图真正要算的两件事。
//!
//! Task 8 把「这份卡住谁」「这件在等谁」两个**相邻**问题答清楚了，Task 20 又把
//! 成环拒了。但病案室真正要看的不是相邻的一跳，而是：
//!
//! - **缺 1 件会连带卡住几件？** 约束知情同意没签，约束记录出不来，
//!   约束解除记录自然也出不来——护士长要一次看到这 3 件，而不是自己顺着看。
//! - **欠了几天？** 归档核齐的排序依据。一个住院别名下以**最老**的那条缺件为准，
//!   因为最老的那条是整串的根因。
//!
//! 两者都建立在同一份**正反向 `ImportMap`** 上（源产品
//! `crates/ruff_graph/src/lib.rs::ImportMap`）：`dependents` 侧做 BFS 就是
//! 阻塞传播的传播方向，`dependencies` 侧做 BFS 就是「这件在等谁」。
//!
//! ## 三条硬口径
//!
//! 1. **闭包只走 `requires: true` 的边**。`soft_gaps`（参考关系）不传播：
//!    评估报告缺住院记录，不影响任何别的件归档。
//! 2. **成环图拒绝出闭包**。环上谁都不是「最先」的一份，传播深度无解
//!    （与 [`crate::DocGraph::import_maps`] 同一个理由，但错误码不同：
//!    出图用 `E_DEP_CYCLE`，这里额外说明是闭包传播不了）。
//! 3. **欠件天数不拿当前时间兜底**。一件缺件如果从来没登记过「首次发现缺失
//!    的时刻」，天数返回 `None` 并记 anomaly，**不**返回 0，也**不**返回
//!    「从今天算起」——后者会让护士长看到一份假的「刚缺 0 天」。
//!    自然日按日期差算，不按 24 小时整除：3-02 23:40 缺、3-03 00:10 查，
//!    是第 1 天，不是 0 天。

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use chrono::{DateTime, FixedOffset};
use serde::{Deserialize, Serialize};

/// 缺件「首次发现缺失」时刻的登记簿。
///
/// 由调用方从持久化历史喂进来（生产上来自 `psychdoc-store` 的状态历史，
/// 测试里直接构造）。本 crate **不**自己取当前时间，也不自己写这份账——
/// 取当前时间等于把「什么时候发现的」和「什么时候算的」混成一个量。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingLedger {
    /// 缺件占位节点 id → 首次发现缺失的时刻。
    first_seen: BTreeMap<String, DateTime<FixedOffset>>,
}

impl MissingLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一件缺件的首次发现时刻。重复登记取**更早**的那个：
    /// 后一次调用通常来自「又扫了一遍」，不该把已经欠了 5 天的件改成今天才发现。
    pub fn record(&mut self, node_id: impl Into<String>, at: DateTime<FixedOffset>) -> &mut Self {
        let id = node_id.into();
        self.first_seen
            .entry(id)
            .and_modify(|prev| {
                if at < *prev {
                    *prev = at;
                }
            })
            .or_insert(at);
        self
    }

    /// 批量登记。用于把一次归档核齐里新发现的缺件并进旧账。
    pub fn extend_from<I: IntoIterator<Item = (String, DateTime<FixedOffset>)>>(
        &mut self,
        entries: I,
    ) -> &mut Self {
        for (id, at) in entries {
            self.record(id, at);
        }
        self
    }

    pub fn get(&self, node_id: &str) -> Option<DateTime<FixedOffset>> {
        self.first_seen.get(node_id).copied()
    }

    pub fn len(&self) -> usize {
        self.first_seen.len()
    }

    pub fn is_empty(&self) -> bool {
        self.first_seen.is_empty()
    }
}

/// 闭包传播的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Blockage {
    /// 根因缺件（`NodeKind::Missing` 且有阻塞边指向它），升序。
    pub roots: Vec<String>,
    /// 被连带阻塞的件：node_id → 阻塞详情。**每个件只出现一次**
    /// （菱形依赖 A→B→D、A→C→D 里 D 不记两笔）。
    pub blocked: BTreeMap<String, BlockedEntry>,
    /// 最深传播层数（直接被缺件卡住的是 1）。
    pub max_depth: u32,
    /// 传播过程里发现、但不改变结论的异常（时刻晚于 `now`、登记了非缺件等）。
    pub anomalies: Vec<String>,
}

impl Blockage {
    /// 这件被连带阻塞了吗（含直接被卡）。
    pub fn is_blocked(&self, node_id: &str) -> bool {
        self.blocked.contains_key(node_id)
    }

    /// 这件被几级连带卡住。`None` = 没被卡。
    pub fn depth_of(&self, node_id: &str) -> Option<u32> {
        self.blocked.get(node_id).map(|e| e.depth)
    }

    /// 这件是被哪几件缺件卡住的（沿路径回溯到的全部根因）。
    pub fn roots_of(&self, node_id: &str) -> BTreeSet<String> {
        self.blocked.get(node_id).map(|e| e.roots.clone()).unwrap_or_default()
    }

    /// 被连带阻塞的件（不含根因缺件本身）。
    pub fn blocked_ids(&self) -> Vec<String> {
        self.blocked.keys().cloned().collect()
    }

    /// 一句话给护士长：这份为什么出不来。
    pub fn explain(&self, node_id: &str, name_of: impl Fn(&str) -> String) -> String {
        let Some(entry) = self.blocked.get(node_id) else {
            return format!("{} 不受缺件影响", name_of(node_id));
        };
        let roots: Vec<String> = entry.roots.iter().map(|r| name_of(r)).collect();
        let level = match entry.depth {
            1 => "直接卡住".to_string(),
            d => format!("被连带卡住（第 {d} 级）"),
        };
        format!(
            "{} {level}：缺 {}",
            name_of(node_id),
            roots.join("、")
        )
    }
}

/// 一件被阻塞的件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockedEntry {
    /// 传播层数：1 = 直接被某件缺件卡住，2 = 被 1 级连带件的下游卡住，依此类推。
    pub depth: u32,
    /// 沿路径回溯到的**全部**根因缺件（菱形依赖时不止一个）。
    pub roots: BTreeSet<String>,
}

/// 闭包传播无法进行时的错误码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ClosureError {
    /// 图成环，传播深度无解。与 [`crate::InvariantError::Cycle`] 同源不同码：
    /// 出图阶段拒过一次，这里是**兜底**——调用方若绕过 `import_maps` 直接来算闭包，
    /// 仍然不能拿到一份假深度。
    #[error("E_DEP_CYCLE_PROPAGATION: 依赖图成环，阻塞传播无环上解，拒绝出闭包；环：{0}")]
    Cycle(String),
}

/// 正反向对拍失败时的错误码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("E_DIR_PAIRING: 节点 {node_id} 的正反向并集与可达集对不上：{detail}")]
pub struct DirectionPairingError {
    pub node_id: String,
    pub detail: String,
}

/// 单件缺件的欠件天数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissingAge {
    pub node_id: String,
    pub encounter_alias: String,
    pub template_id: String,
    /// 首次发现缺失的时刻。**一定来自登记簿**，不是 `now`。
    pub first_seen: DateTime<FixedOffset>,
    /// 自然日差。`None` = 登记簿里没有这件缺件的发现时刻（见模块头口径 3）。
    pub days: Option<i64>,
    /// 口径说明，直接进「欠件天数」侧栏的脚注。
    pub basis_zh: String,
}

/// 一次住院的欠件天数（按别名聚合，取最老的一条缺件）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncounterAge {
    pub encounter_alias: String,
    /// 自然日差。`None` = 该别名下**没有一条缺件登记过发现时刻**。
    pub days: Option<i64>,
    /// 最老的那条缺件（并列时取 node_id 最小的一条，保证可复现）。
    pub oldest_missing_node: Option<String>,
    /// 该别名下的缺件数。
    pub missing_count: usize,
    pub basis_zh: String,
}

/// 归档核齐视图的一行。Task 34 渲染依赖图时按行画，Task 31 导出审计时按行导出。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveRow {
    pub node_id: String,
    pub template_id: String,
    /// 院内类型字典里的中文名。
    pub display_zh: String,
    pub present: bool,
    /// 是否直接被缺件卡住（有指向缺件占位节点的阻塞边）。
    pub directly_blocked: bool,
    /// 被连带阻塞时的层数；`None` = 没被卡。
    pub blocked_depth: Option<u32>,
    /// 被卡时回溯到的根因缺件 node_id 集合。
    pub blocked_by: BTreeSet<String>,
    /// 这件在等谁（正向，`dependencies`）。
    pub waiting_on: Vec<String>,
    /// 这件卡住了谁（反向，`dependents`）。
    pub blocking_others: Vec<String>,
    /// 缺件才有：本件的欠件天数。
    pub missing_days: Option<i64>,
}

/// 归档核齐视图数据。**可导出**：`Serialize` 落盘即 Task 34 的渲染输入、
/// Task 31 的导出行来源，不做二次拼装。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveView {
    pub encounter_alias: String,
    /// 本视图以哪个时刻为「今天」算的（由调用方给，不读系统时钟）。
    pub as_of: DateTime<FixedOffset>,
    pub rows: Vec<ArchiveRow>,
    /// 缺件及其欠件天数（含天数算不出来的，附口径说明）。
    pub missing: Vec<MissingAge>,
    /// 本次住院的欠件天数（最老缺件口径）。
    pub encounter_age: Option<EncounterAge>,
    /// 阻塞边（`from → to`，`to` 是缺件占位）。
    pub blocking_edges: Vec<(String, String)>,
    /// `requires: false` 的参考缺件：可见但不阻断。
    pub soft_gaps: Vec<(String, String)>,
    /// 需要人工确认的件（绑定不唯一 / 未绑定 / 成环隔离）。
    pub manual_queue: Vec<String>,
    /// 口径说明。多行，直接显示在视图脚注。
    pub basis_zh: Vec<String>,
}

impl ArchiveView {
    /// 这个住院能不能归档：零缺件、零连带阻塞、零人工待办。
    ///
    /// 成环导致没算出闭包时**返回 false**——宁可说「还没核齐」，也不给
    /// 一份在环上瞎算出来的「已核齐」。
    pub fn is_ready(&self) -> bool {
        self.missing.is_empty()
            && self.rows.iter().all(|r| r.blocked_depth.is_none())
            && self.manual_queue.is_empty()
    }
}

/// 从缺件出发做 `dependents` 方向的 BFS，得到被连带阻塞的件与传播深度。
///
/// 只走 `requires: true` 的边：软缺件（`requires: false`）不传播。
/// 每个件**只记一次**——BFS 用 `depth` 最短优先，菱形汇合点不会被计两笔；
/// 但 `roots` 保留全部根因，「这份是两份缺件一起卡住的」这条信息不能丢。
pub(crate) fn propagate_blockage(
    roots: &[String],
    edges: &[(String, String)],
) -> Blockage {
    // dependents 反向邻接表：前置件 → 依赖它的件。
    let mut dependents: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (from, to) in edges {
        dependents.entry(to.as_str()).or_default().push(from.as_str());
    }

    let mut out = Blockage {
        roots: roots.to_vec(),
        ..Default::default()
    };

    for root in roots {
        let mut queue: VecDeque<(&str, u32)> = VecDeque::new();
        queue.push_back((root.as_str(), 0));
        while let Some((node, depth)) = queue.pop_front() {
            let Some(children) = dependents.get(node) else {
                continue;
            };
            for child in children {
                let next_depth = depth + 1;
                let child_str = child.to_string();
                match out.blocked.get_mut(&child_str) {
                    Some(entry) => {
                        entry.roots.insert(root.clone());
                        // 更短的路径才更新深度；等长或更长时保留先到的（更近）那条。
                        if next_depth < entry.depth {
                            entry.depth = next_depth;
                            queue.push_back((child, next_depth));
                        }
                    }
                    None => {
                        let mut rootset = BTreeSet::new();
                        rootset.insert(root.clone());
                        out.blocked.insert(
                            child_str,
                            BlockedEntry {
                                depth: next_depth,
                                roots: rootset,
                            },
                        );
                        queue.push_back((child, next_depth));
                    }
                }
            }
        }
    }

    out.max_depth = out.blocked.values().map(|e| e.depth).max().unwrap_or(0);
    out
}

/// 自然日差：按**日期**差，不按 24 小时整除。
///
/// 3-02 23:40 缺、3-03 00:10 查 → 1 天（不是 0 天）。
/// 跨零点、跨月、跨年走 `chrono::NaiveDate` 的减法，不做时区换算。
pub fn natural_days_between(
    from: DateTime<FixedOffset>,
    to: DateTime<FixedOffset>,
) -> i64 {
    (to.date_naive() - from.date_naive()).num_days()
}
