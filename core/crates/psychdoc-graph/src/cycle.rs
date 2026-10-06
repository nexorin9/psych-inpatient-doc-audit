//! 依赖图环检测：Tarjan 强连通分量 + 成环拒绝出图。
//!
//! ## 源产品主能力与本产品的分歧
//!
//! 源产品（`analyze_graph`）在 import 环上是**诊断上报**：环照样出图，
//! 只是给一条「这个 import 形成了环」的提示。图能不能画出来，与环无关。
//!
//! 本产品**不能**这样。院内文书依赖规范里如果写着
//!
//! ```text
//! 约束记录      → 约束知情同意
//! 约束解除记录  → 约束记录
//! 约束知情同意  → 约束解除记录      # ← 抄规则集时补的一行
//! ```
//!
//! 阻塞传播（Task 21 的多级闭包）在这张图上**无解**：三件都「在等对方」，
//! 病案室看到的归档核齐视图会永远显示「还差 2 件」，而真实情况是规则集写错了。
//! 源侧的「照出图 + 报一条提示」在这里等于**给出一个假的核齐结论**。
//!
//! 因此本模块的策略是三件事同时做：
//!
//! 1. **算**：Tarjan 求强连通分量，取出所有成环节点；
//! 2. **说**：给出可复现的环路径（`A → B → A`）与每条边的规范来源行，
//!    让规则集维护者能直接照着改（`E_DEP_CYCLE`）；
//! 3. **拒**：环内节点**整件标 ambiguous**、**不产生任何边**、进人工队列；
//!    `check_invariants` 见到环直接返回错误，`ImportMap` 构造被拒绝。
//!
//! 环**外**的节点照常解析、照常连边——局部配置错误不该让整张图报废。
//!
//! ## 为什么用 Tarjan 而不是拓扑排序
//!
//! 拓扑排序只能回答「有没有环」，答不出「哪些件在环上」。归档核齐视图要做的
//! 恰恰是后者：把环内三件一起标成「规则集写错，需人工确认先后」，
//! 而把环外的件照常显示成缺件/已收。Tarjan 一次遍历同时给出这两件事。
//!
//! ## 复杂度
//!
//! 递归深度 = 图上最长依赖链。边**绝不跨住院别名**（`check_invariants` 硬约束），
//! 一次住院的文书类型数由院内类型字典决定（个位到十位数），故递归深度有界，
//! 不需要为「1000 件批扫」（Task 25）改写成显式栈。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{GraphEdge, GraphNode};

/// 环上的一条边。带规范来源，便于规则集维护者照着改。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleEdge {
    pub from: String,
    pub to: String,
    /// 依赖规范声明的 `from` 类型码。
    pub spec_from: String,
    /// 依赖规范声明的 `to` 类型码。
    pub spec_to: String,
    /// 该规范是否 `requires: true`。
    pub requires: bool,
}

impl CycleEdge {
    /// 人可读的一行规范来源，进 `E_DEP_CYCLE` 的报错文案。
    pub fn spec_line(&self) -> String {
        format!(
            "{} → {}（{} → {}，{}）",
            display_id(&self.from),
            display_id(&self.to),
            self.spec_from,
            self.spec_to,
            if self.requires { "必前置" } else { "仅参考" }
        )
    }
}

/// 一个强连通分量，且它确实成环（`nodes.len() >= 2`，或单点自环）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DependencyCycle {
    /// 环所在的住院别名。边不跨住院，故一个环必然只在一个住院内。
    pub encounter_alias: String,
    /// 环上全部节点（按 id 字典序，稳定可复现）。
    pub nodes: Vec<String>,
    /// 环路径，**首尾同一节点**：`[A, B, C, A]`。长度为 `nodes.len() + 1`。
    pub path: Vec<String>,
    /// 与 `path` 一一对应的边（`path.len() - 1` 条，含最后一条回到起点）。
    pub edges: Vec<CycleEdge>,
}

impl DependencyCycle {
    /// 一行中文说明：环路径 + 规范来源。进 `E_DEP_CYCLE` 与人工队列理由。
    pub fn describe(&self) -> String {
        let path = self
            .path
            .iter()
            .map(|id| display_id(id))
            .collect::<Vec<_>>()
            .join(" → ");
        let specs = self
            .edges
            .iter()
            .map(CycleEdge::spec_line)
            .collect::<Vec<_>>()
            .join("；");
        format!(
            "文书依赖规范成环：{path}。环上 {} 件互为前置，归档核齐无解（谁都不是「最先」的一份），\
             需人工确认先后顺序后修正规范。规范来源：{specs}",
            self.nodes.len()
        )
    }
}

/// 环检测结果。`cycles` 为空 = 这张图无环。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CycleAudit {
    pub cycles: Vec<DependencyCycle>,
}

impl CycleAudit {
    pub fn is_clean(&self) -> bool {
        self.cycles.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self.cycles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.cycles.len()
    }

    /// 全部环上节点。`DocGraph` 靠它决定哪些件要整件标 ambiguous。
    pub fn node_ids(&self) -> BTreeSet<String> {
        self.cycles.iter().flat_map(|c| c.nodes.iter().cloned()).collect()
    }

    /// 全部成环节点，一行一个。多个环时按节点 id 排序。
    pub fn describe_zh(&self) -> String {
        if self.cycles.is_empty() {
            return String::new();
        }
        self.cycles
            .iter()
            .map(DependencyCycle::describe)
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// 在 `nodes` + `edges` 上做环检测。
///
/// 只吃 [`GraphEdge`]（`requires: true` 的实边 + 指向缺件占位的阻塞边）。
/// `soft_gaps`（`requires: false` 的参考关系）**不参与**：参考关系缺了只提示
/// 不阻断，环在这种边上不会让归档核齐无解，把它算进来只会淹掉真正该报的行。
pub fn detect_cycles(
    nodes: &BTreeMap<String, GraphNode>,
    edges: &[GraphEdge],
) -> CycleAudit {
    // 邻接表。`BTreeSet` 天然按 id 排序 —— 下面找环路径时遍历顺序因此确定，
    // 同一张图跑两次必得同一条路径（用例 `cycle_paths_are_reproducible` 守住）。
    //
    // `requires: false` 的参考边在这里**硬过滤**，不靠调用方传对切片：
    // 契约写在文档里不够，规则集里多声明一条 `requires: false` 的双向参考
    // 关系就够淹掉真正该报的那一行。过滤放在函数内，调用方传什么都一样。
    let mut adj: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for e in edges.iter().filter(|e| e.requires) {
        adj.entry(e.from.clone()).or_default().insert(e.to.clone());
        adj.entry(e.to.clone()).or_default();
    }
    // 悬空端点（手工拼的坏图）不参与：它们不构成环，且 `check_invariants`
    // 会用 E_DEP_DANGLING 单独报，两件事不混。
    adj.retain(|id, _| nodes.contains_key(id));

    let components = tarjan(&adj);
    let mut cycles: Vec<DependencyCycle> = Vec::new();
    let mut seen_components: BTreeSet<Vec<String>> = BTreeSet::new();

    for comp in components {
        if comp.len() < 2 {
            // 单点分量只有自环才成环。自环在收集期就被收进 `self_loops` 不连边，
            // 但手工拼的图里可能仍有，`E_DEP_SELF_LOOP` 由 `check_invariants` 报；
            // 这里不重复计入环清单，避免同一条问题记两笔。
            continue;
        }
        let mut members = comp.clone();
        members.sort();
        if !seen_components.insert(members.clone()) {
            continue;
        }
        let (path, cycle_edges) = trace_cycle(&adj, &members, edges);
        let encounter_alias = nodes
            .get(&members[0])
            .map(|n| n.encounter_alias.clone())
            .unwrap_or_default();
        cycles.push(DependencyCycle {
            encounter_alias,
            nodes: members,
            path,
            edges: cycle_edges,
        });
    }
    cycles.sort_by(|a, b| a.nodes.cmp(&b.nodes));
    CycleAudit { cycles }
}

/// Tarjan 强连通分量（迭代序由 `BTreeSet` 邻接决定，结果稳定可复现）。
fn tarjan(adj: &BTreeMap<String, BTreeSet<String>>) -> Vec<Vec<String>> {
    struct State {
        index: BTreeMap<String, usize>,
        lowlink: BTreeMap<String, usize>,
        on_stack: BTreeSet<String>,
        stack: Vec<String>,
        next_index: usize,
        out: Vec<Vec<String>>,
    }

    fn visit(
        adj: &BTreeMap<String, BTreeSet<String>>,
        v: &str,
        s: &mut State,
    ) {
        s.index.insert(v.to_string(), s.next_index);
        s.lowlink.insert(v.to_string(), s.next_index);
        s.next_index += 1;
        s.stack.push(v.to_string());
        s.on_stack.insert(v.to_string());

        for w in adj.get(v).into_iter().flatten() {
            if !s.index.contains_key(w) {
                visit(adj, w, s);
                let lw = s.lowlink[w];
                let lv = s.lowlink[v];
                s.lowlink.insert(v.to_string(), lv.min(lw));
            } else if s.on_stack.contains(w) {
                let iw = s.index[w];
                let lv = s.lowlink[v];
                s.lowlink.insert(v.to_string(), lv.min(iw));
            }
        }

        let lv = s.lowlink[v];
        if lv == s.index[v] {
            let mut comp = Vec::new();
            while let Some(w) = s.stack.pop() {
                s.on_stack.remove(&w);
                let is_root = w == v;
                comp.push(w);
                if is_root {
                    break;
                }
            }
            s.out.push(comp);
        }
    }

    let mut s = State {
        index: BTreeMap::new(),
        lowlink: BTreeMap::new(),
        on_stack: BTreeSet::new(),
        stack: Vec::new(),
        next_index: 0,
        out: Vec::new(),
    };
    for v in adj.keys() {
        if !s.index.contains_key(v) {
            visit(adj, v, &mut s);
        }
    }
    s.out
}

/// 在一个强连通分量内追出一条**确定性**的环路径。
///
/// 起点取分量内最小的节点 id，邻居按 id 升序深搜，第一次回到起点即成环。
/// 「最小起点 + 升序邻居」两条固定住遍历顺序，因此同一张图两次调用
/// 必得同一条路径，报错文案才可复现、可 diff。
fn trace_cycle(
    adj: &BTreeMap<String, BTreeSet<String>>,
    members: &[String],
    edges: &[GraphEdge],
) -> (Vec<String>, Vec<CycleEdge>) {
    let in_comp: BTreeSet<&str> = members.iter().map(String::as_str).collect();
    let start = members[0].as_str();

    // 分量内的邻居已按 id 升序（`BTreeSet` 天然有序），遍历顺序因此确定。
    let neighbours_of = |node: &str| -> Vec<String> {
        adj.get(node)
            .map(|s| s.iter().filter(|n| in_comp.contains(n.as_str())).cloned().collect())
            .unwrap_or_default()
    };

    // 显式栈深搜：帧 = (节点, 还没试过的邻居, 下一个要试的下标)。
    let mut path: Vec<String> = vec![start.to_string()];
    let mut on_path: BTreeSet<String> = BTreeSet::from([start.to_string()]);
    let mut frames: Vec<(String, Vec<String>, usize)> = vec![(start.to_string(), neighbours_of(start), 0)];

    while let Some((_node, pending, cursor)) = frames.last_mut() {
        if *cursor >= pending.len() {
            frames.pop();
            if path.len() > 1 {
                let gone = path.pop().expect("路径非空");
                on_path.remove(&gone);
            }
            continue;
        }
        let next = pending[*cursor].clone();
        *cursor += 1;
        if next == start {
            path.push(next);
            let cycle_edges = path_edges(&path, edges);
            return (path, cycle_edges);
        }
        if on_path.contains(&next) {
            // 走到自己身上的另一条边：还没绕回起点，试试下一个邻居。
            continue;
        }
        path.push(next.clone());
        on_path.insert(next.clone());
        let child_pending = neighbours_of(&next);
        frames.push((next, child_pending, 0));
    }

    // Tarjan 说这些点在同一个强连通分量里，理论上一定能绕回起点。
    // 万一图被手工改坏（邻接表与 edges 不一致），也不 panic：退化成
    // 「按成员顺序首尾相接」，并在路径上只保留真实存在的边。
    let mut fallback = members.to_vec();
    if let Some(first) = fallback.first().cloned() {
        fallback.push(first);
    }
    let cycle_edges = path_edges(&fallback, edges);
    (fallback, cycle_edges)
}

/// 把一条节点路径翻译成边序列（`path.len() - 1` 条，最后一条回到起点）。
fn path_edges(path: &[String], edges: &[GraphEdge]) -> Vec<CycleEdge> {
    path.windows(2)
        .filter_map(|w| {
            edges
                .iter()
                .find(|e| e.from == w[0] && e.to == w[1])
                .map(|e| CycleEdge {
                    from: e.from.clone(),
                    to: e.to.clone(),
                    spec_from: e.spec_from.clone(),
                    spec_to: e.spec_to.clone(),
                    requires: e.requires,
                })
        })
        .collect()
}

/// 节点 id → 人读得懂的名字。
///
/// 缺件占位节点的 id 形如 `<WARD-B-001>::missing::restraint_consent`，
/// 直接印出来是一串尖括号噪音，这里压成「缺件·约束知情同意」。
pub fn display_id(id: &str) -> String {
    if let Some(rest) = id.strip_prefix('<') {
        if let Some((alias, template)) = rest.split_once(">::missing::") {
            return format!("缺件·{template}（{alias}）");
        }
    }
    id.to_string()
}
