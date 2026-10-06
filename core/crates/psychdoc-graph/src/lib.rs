//! `psychdoc-graph`：文书依赖图 —— 归档核齐视图的数据基础。
//!
//! ## 源产品主能力与改写
//!
//! 源产品的主路径是「一条命令拿整张依赖图，靠方向开关问正反两个问题」：
//!
//! ```text
//! analyze_graph(args, config)
//!   ├─ resolve_default_files / project_files_in_path / paths.retain   发现→排除→过滤
//!   ├─ ModuleImports::detect(...)          ruff_graph/src/lib.rs:26
//!   │    ├─ parse(source, options)         解析
//!   │    ├─ Collector::new(..).collect()   crates/ruff_graph/src/collector.rs:49
//!   │    └─ Resolver::new(..).resolve()     crates/ruff_graph/src/resolver.rs:40
//!   └─ match direction {                   crates/ruff/src/commands/analyze_graph.rs
//!        Direction::Dependencies => ImportMap::dependencies(imports),
//!        Direction::Dependents   => ImportMap::dependents(imports),
//!     }
//! ```
//!
//! `ImportMap::dependents` 的实现只有一段——**把每条边反向插一次**
//! （`ruff_graph/src/lib.rs:122-131`），源产品靠 `Direction` 开关复用同一份
//! `ModuleImports` 得出正反两个答案。本产品完整保留这条形状，只把三处换掉：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | 节点 = `.py` 文件路径 | 节点 = **文书件**（`Composition.id` 脱敏别名） |
//! | 边 = AST 里 `import a.b` 语句 | 边 = **院内文书依赖规范**（规则集 `dependencies` 声明） |
//! | `Resolver::resolve` 返回命中的模块路径 | `Resolver` 只对 **`Binding::Confident`** 的件连边；`Ambiguous` 进人工队列且**不产生任何边**（绝不取第一份当答案） |
//!
//! 节点额外带一层**缺件占位**（`NodeKind::Missing`）：院内「约束记录」在、
//! 「约束知情同意」不在时，病案室要看到的是「这份卡在谁」而不是一张空图。
//! 缺件占位只由 `requires: true` 的依赖规范产生，且**恰好一条阻塞边**；
//! `requires: false` 的缺失记 [`soft_gaps`]，可见但不阻断。
//!
//! [Task 20](https://example.invalid) 起在本文件上叠加 Tarjan 强连通分量与
//! `E_DEP_CYCLE`；本任务只落自环检测，环的传播见后续任务。

pub mod closure;
pub mod collector;
pub mod cycle;
pub mod import_map;
pub mod resolver;

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset};
use psychdoc_model::Binding;
use serde::{Deserialize, Serialize};

pub use closure::{
    ArchiveRow, ArchiveView, Blockage, BlockedEntry, ClosureError,
    DirectionPairingError, EncounterAge, MissingAge, MissingLedger,
};
pub use collector::{Collected, CollectedEdge, Collector, DanglingSpec, SelfLoop};
pub use cycle::{CycleAudit, CycleEdge, DependencyCycle};
pub use import_map::{Direction, ImportMap};
pub use resolver::{ManualNode, ResolvedGraph, Resolver};

/// 一个节点的来源。缺件占位没有 `Composition.id`，靠合成 id 区分。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    /// 包内真实存在的件。
    Present,
    /// 依赖规范要求存在、但本次包内没有的件（缺件占位）。
    Missing,
}

/// 依赖图节点 = 一件文书。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNode {
    /// 节点 id。真实件 = `Composition.id` 脱敏别名；缺件占位 = [`GraphNode::missing_id`]。
    pub node_id: String,
    /// 归到哪次住院。**边绝不跨住院别名**（见 [`DocGraph::check_invariants`]）。
    pub encounter_alias: String,
    /// 院内文书类型码。
    pub template_id: String,
    pub kind: NodeKind,
    /// 真实件才有：权威实体键。
    pub doc_id: Option<String>,
    /// 真实件才有：权威系统（脱敏）。
    pub source_system: String,
    /// 真实件才有：实体绑定结论。
    pub binding: Binding,
}

impl GraphNode {
    /// 缺件占位的合成 id。带 `<>` 前缀，肉眼即可与真实件区分，
    /// 且不会与任何 `Composition.id` 撞上。
    pub fn missing_id(encounter_alias: &str, template_id: &str) -> String {
        format!("<{encounter_alias}>::missing::{template_id}")
    }

    pub fn is_missing(&self) -> bool {
        self.kind == NodeKind::Missing
    }
}

/// 一条依赖边：`from` 依赖（要求先有）`to`。
///
/// 方向与「谁卡住谁」相反是刻意的：`from` 是后件、`to` 是前置件，
/// 与规则集 `dependencies: [{from, to}]` 的书写方向一致，避免二次翻译出错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdge {
    /// 依赖方（后件）的节点 id。
    pub from: String,
    /// 被依赖方（前置件）的节点 id；可能指向缺件占位。
    pub to: String,
    /// 依赖规范里的 `requires`。`false` = 参考关系，缺了不阻断。
    pub requires: bool,
    /// 该前置件缺失且 `requires: true` → 阻塞边（归档核齐视图加粗的那条）。
    pub blocking: bool,
    /// 边的规范来源：依赖规范声明的 `from` 类型码。
    pub spec_from: String,
    /// 边的规范来源：依赖规范声明的 `to` 类型码。
    pub spec_to: String,
}

impl GraphEdge {
    /// 边的一句话说明，进「这条为什么卡住」的面板。
    pub fn describe(&self, node_of: impl Fn(&str) -> Option<String>) -> String {
        let name = |id: &str| node_of(id).unwrap_or_else(|| id.to_string());
        if self.blocking {
            format!("{} 需要的前置件「{}」不在本次文书包里", name(&self.from), name(&self.to))
        } else {
            format!("{} 参考的前置件「{}」", name(&self.from), name(&self.to))
        }
    }
}

/// 图不变量被破坏时的错误码。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum InvariantError {
    /// 边的端点不在节点表里（悬空引用）。
    #[error("E_DEP_DANGLING: 边 {from} → {to} 的端点 {missing} 不在节点表中")]
    Dangling { from: String, to: String, missing: String },
    /// 边跨了住院别名。
    #[error("E_DEP_CROSS_ENCOUNTER: 边 {from}（{from_enc}）→ {to}（{to_enc}）跨了住院别名")]
    CrossEncounter {
        from: String,
        from_enc: String,
        to: String,
        to_enc: String,
    },
    /// 同一个节点 id 出现了两次。
    #[error("E_DEP_DUP_NODE: 节点 id {0} 重复")]
    DuplicateNode(String),
    /// 文书依赖规范成环。阻塞传播在环上无解，**拒绝出图**（环外节点照常出图）。
    #[error("E_DEP_CYCLE: {0}")]
    Cycle(String),
}

/// 文书依赖图。`dependencies` 与 `dependents` 是同一批边的两个方向，
/// 与源产品 [`ImportMap::dependencies`] / [`ImportMap::dependents`] 同形。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DocGraph {
    pub nodes: std::collections::BTreeMap<String, GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// `requires: false` 的依赖所缺的前置件：可见、记原因，但**不阻断**。
    pub soft_gaps: Vec<GraphEdge>,
    /// 绑定不唯一 / 未绑定的件。**它们不产生任何边**。
    pub manual_queue: Vec<ManualNode>,
    /// 自环依赖（`from == to`）。可见但不连边——「这份在等自己」会让
    /// 归档核齐永远核不齐，必须让人看见是规则集写错了。
    pub self_loops: Vec<SelfLoop>,
    /// 检出并已隔离的依赖环（Task 20）。非空即表示 [`DocGraph::check_invariants`]
    /// 会报 `E_DEP_CYCLE`：环内节点已整件标 ambiguous 且不连任何边。
    pub cycles: Vec<DependencyCycle>,
}

impl DocGraph {
    /// 用解析结果 + 生效规则集的依赖规范建图。
    ///
    /// 出图后立刻做**环检测**：检出成环分量就把环内件整件隔离
    /// （标 ambiguous、不连边、进人工队列），环**外**的件照常解析照常连边。
    /// 「局部配置错误不该让整张图报废」与「环上不能出假核齐结论」两条同时守住。
    pub fn build(items: &[psychdoc_model::DocItem], rules: &psychdoc_rules::EffectiveRules) -> Self {
        let collected = Collector::new().collect(items, rules);
        let resolved = Resolver::new().resolve_with(&collected, items);
        let mut graph = Self {
            nodes: resolved.nodes,
            edges: resolved.edges,
            soft_gaps: resolved.soft_gaps,
            manual_queue: resolved.manual_queue,
            self_loops: resolved.self_loops,
            cycles: Vec::new(),
        };
        graph.quarantine_cycles();
        graph
    }

    /// 环检测 + 环内件隔离。幂等：已隔离的图再跑一次结果不变。
    fn quarantine_cycles(&mut self) {
        let audit = cycle::detect_cycles(&self.nodes, &self.edges);
        if audit.is_clean() {
            return;
        }
        let in_cycle = audit.node_ids();

        // 环内件的边整条撤掉：一条都不留，也不留 `soft_gaps`。
        // 留着就等于告诉病案室「这份在等那份」——而环上谁都不是「最先」的一份。
        self.edges
            .retain(|e| !in_cycle.contains(&e.from) && !in_cycle.contains(&e.to));
        self.soft_gaps
            .retain(|e| !in_cycle.contains(&e.from) && !in_cycle.contains(&e.to));

        for id in &in_cycle {
            let Some(node) = self.nodes.get_mut(id) else {
                continue;
            };
            // 整件标 ambiguous：UI 侧渲染成灰底 + 角标，与「跨系统同名需人工」
            // 同一套视觉，语义也都是「这条没人敢自动处理」。
            node.binding = Binding::Ambiguous;
            let reason = audit
                .cycles
                .iter()
                .find(|c| c.nodes.contains(id))
                .map(|c| c.describe())
                .unwrap_or_else(|| "文书依赖规范成环".to_string());
            if !self.manual_queue.iter().any(|m| &m.doc_id == id) {
                self.manual_queue.push(ManualNode {
                    doc_id: id.clone(),
                    template_id: node.template_id.clone(),
                    encounter_alias: node.encounter_alias.clone(),
                    source_system: node.source_system.clone(),
                    binding: Binding::Ambiguous,
                    reason,
                });
            }
        }
        self.cycles = audit.cycles;
    }

    /// 环检测入口（供 UI / CLI 单独问一次，不改图）。
    pub fn audit_cycles(&self) -> CycleAudit {
        CycleAudit {
            cycles: self.cycles.clone(),
        }
    }

    /// 「一条命令拿整张图」的正向表：节点 → 它要求的前置件。
    ///
    /// **成环时拒绝出图**（`E_DEP_CYCLE`）：环上的阻塞传播无解，硬画出一张
    /// 正向表只会让病案室看到一份永远「核不齐」的假结论。环外节点照常可查。
    pub fn dependencies_map(&self) -> Result<ImportMap, InvariantError> {
        self.reject_if_cyclic()?;
        Ok(ImportMap::dependencies(
            self.edges.iter().map(|e| (e.from.clone(), e.to.clone())),
        ))
    }

    /// 反向表：前置件 → 谁在等它。与正向表来自**同一批边**。
    pub fn dependents_map(&self) -> Result<ImportMap, InvariantError> {
        self.reject_if_cyclic()?;
        Ok(ImportMap::dependents(
            self.edges.iter().map(|e| (e.from.clone(), e.to.clone())),
        ))
    }

    /// 正反两表一次拿全（源产品「靠方向开关问正反两问题」）。
    /// 两表同生共死：有环时一起拒绝，不给「只出一半方向」的中间态。
    pub fn import_maps(&self) -> Result<(ImportMap, ImportMap), InvariantError> {
        Ok((self.dependencies_map()?, self.dependents_map()?))
    }

    fn reject_if_cyclic(&self) -> Result<(), InvariantError> {
        if self.cycles.is_empty() {
            return Ok(());
        }
        Err(InvariantError::Cycle(self.audit_cycles().describe_zh()))
    }

    /// 方向开关：正反两个问题问同一张图。
    pub fn query(&self, node_id: &str, direction: Direction) -> BTreeSet<String> {
        match direction {
            Direction::Dependencies => self
                .edges
                .iter()
                .filter(|e| e.from == node_id)
                .map(|e| e.to.clone())
                .collect(),
            Direction::Dependents => self
                .edges
                .iter()
                .filter(|e| e.to == node_id)
                .map(|e| e.from.clone())
                .collect(),
            Direction::Both => {
                let mut out: BTreeSet<String> = self.query(node_id, Direction::Dependencies);
                out.extend(self.query(node_id, Direction::Dependents));
                out
            }
        }
    }

    /// 正反两个方向一次拿全（对应源产品「靠方向开关问正反两问题」）。
    pub fn both_directions(&self, node_id: &str) -> (BTreeSet<String>, BTreeSet<String>) {
        (
            self.query(node_id, Direction::Dependencies),
            self.query(node_id, Direction::Dependents),
        )
    }

    /// 成环的节点是不是这些之一。归档核齐视图用它把环内件画成灰底 + 角标。
    pub fn is_in_cycle(&self, node_id: &str) -> bool {
        self.cycles.iter().any(|c| c.nodes.iter().any(|n| n == node_id))
    }

    pub fn blocking_edges(&self) -> Vec<&GraphEdge> {
        self.edges.iter().filter(|e| e.blocking).collect()
    }

    /// 这件被什么卡着（缺件占位节点用它回答）。
    pub fn is_blocked(&self, node_id: &str) -> bool {
        self.edges.iter().any(|e| e.from == node_id && e.blocking)
    }

    /// 这件卡住了谁（归档核齐视图左侧的清单）。
    pub fn blocks(&self, node_id: &str) -> Vec<&GraphEdge> {
        self.edges.iter().filter(|e| e.to == node_id).collect()
    }

    /// 节点的中文展示名：优先院内类型字典，回落到节点 id。
    pub fn display_of(&self, node_id: &str, rules: &psychdoc_rules::EffectiveRules) -> String {
        match self.nodes.get(node_id) {
            Some(n) => rules
                .doc_types
                .get(&n.template_id)
                .cloned()
                .unwrap_or_else(|| n.template_id.clone()),
            None => node_id.to_string(),
        }
    }

    /// 根因缺件：有阻塞边指向它的 [`NodeKind::Missing`] 节点，升序。
    ///
    /// 「根因」的定义刻意收窄成**直接缺件**——不是「所有最终要补的件」。
    /// 一串里真正的根因只有缺的那几件：知情同意没签，后面两件是被连带卡住的，
    /// 派活要派给知情同意那一环，不是派给连带件。
    pub fn missing_roots(&self) -> Vec<String> {
        self.blocking_missing_ids()
    }

    /// **会阻断归档**的缺件占位节点 id（升序）。
    ///
    /// 与 [`DocGraph::missing_roots`] 的区别在 `soft_gaps`：`requires: false`
    /// 的参考缺件同样会造出 [`NodeKind::Missing`] 占位节点（要让病案室看见
    /// 「评估报告参考的住院记录不在包里」），但它**不阻断、不是欠件**。
    /// 欠件天数、缺件数、「能不能归档」三处都必须用这一个口径，
    /// 否则会把参考缺件算成欠件，护士长收到一条根本不欠的催办。
    pub fn blocking_missing_ids(&self) -> Vec<String> {
        let blocking_targets: BTreeSet<String> = self
            .edges
            .iter()
            .filter(|e| e.blocking)
            .map(|e| e.to.clone())
            .collect();
        blocking_targets
            .into_iter()
            .filter(|id| self.nodes.get(id).is_some_and(|n| n.is_missing()))
            .collect()
    }

    /// 阻塞传播：从缺件出发做 `dependents` 方向的 BFS，算出被**连带**阻塞的件
    /// 与传播深度。只走 `requires: true` 的边——`soft_gaps`（参考关系）不传播。
    ///
    /// 成环图**拒绝出闭包**（`E_DEP_CYCLE_PROPAGATION`）：环上谁都不是「最先」
    /// 的一份，传播深度无解，硬算出来的层数是假的。
    pub fn blockage(&self) -> Result<Blockage, ClosureError> {
        self.reject_if_cyclic_for_propagation()?;
        let roots = self.missing_roots();
        let edges: Vec<(String, String)> = self
            .edges
            .iter()
            .filter(|e| e.requires)
            .map(|e| (e.from.clone(), e.to.clone()))
            .collect();
        Ok(closure::propagate_blockage(&roots, &edges))
    }

    /// 正反向对拍：闭包算法算的「被卡住」，必须等于**另一个方向的独立算法**
    /// 算出来的「等到了缺件」。
    ///
    /// 闭包从缺件出发、沿 `dependents`（反向）往下走；这里从**每一件自己**
    /// 出发、沿 `dependencies`（正向）往上走，停在第一个缺件占位上。
    /// 两条路走的是相反方向、不同的遍历入口，答案必须逐件一致——
    /// 这才是「正反两面对拍」真正能拦住的东西。
    ///
    /// 顺带守住两件闭包算法自己管不了的事：
    ///
    /// 1. 传递可达的邻居必须**都在节点表里**。正反两面都是沿边走的，
    ///    一条端点不存在的边照样能被走到——那算出来的闭包里会出现一个
    ///    UI 画不出来的节点，归档核齐视图就多了一行空气。
    /// 2. 正反并集不必等于弱连通分量。菱形里 B 与 C 只经由 D 相连，
    ///    C 在 B 的弱连通分量里，却不在 B 的任何单向闭包里；
    ///    把这两者写成等式是一条**恒假**的断言，写进不变量只会在有人
    ///    改坏算法时给出误导性的报错。真正要对拍的是两个方向的答案。
    pub fn check_direction_pairing(&self, node_id: &str) -> Result<(), DirectionPairingError> {
        if !self.nodes.contains_key(node_id) {
            return Err(DirectionPairingError {
                node_id: node_id.to_string(),
                detail: "节点不在图中".to_string(),
            });
        }
        let forward = self.reachable(node_id, Direction::Dependencies);
        let backward = self.reachable(node_id, Direction::Dependents);

        for (label, set) in [("正向", &forward), ("反向", &backward)] {
            for neighbor in set {
                if !self.nodes.contains_key(neighbor) {
                    return Err(DirectionPairingError {
                        node_id: node_id.to_string(),
                        detail: format!(
                            "{label}可达的 {neighbor} 不在节点表中，闭包里会出现一个画不出来的节点"
                        ),
                    });
                }
            }
        }

        // 反面：闭包（从缺件往下）说它被卡了吗？
        // 正面：这条依赖链（从它往上）是不是真的撞上了缺件？
        let from_roots = self
            .blockage()
            .map(|b| b.is_blocked(node_id))
            .unwrap_or(false);
        let from_chain = self.depends_on_a_missing_item(node_id);
        if from_roots != from_chain {
            return Err(DirectionPairingError {
                node_id: node_id.to_string(),
                detail: format!(
                    "正向说「等到了缺件」={from_chain}，反向闭包说「被连带卡住」={from_roots}，两面不一致"
                ),
            });
        }
        Ok(())
    }

    /// 沿 `dependencies` 往上走，看这件最终是不是等到了某件**会阻断**的缺件。
    ///
    /// 与 [`DocGraph::blockage`] 方向相反、算法独立：闭包是「从缺件往下数谁被卡」，
    /// 这里是「从这件往上找缺件」。同一条边集，两个方向各自走一遍。
    /// `soft_gaps` 不参与（参考缺件不是欠件）。
    pub fn depends_on_a_missing_item(&self, node_id: &str) -> bool {
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::new();
        queue.push_back(node_id.to_string());
        while let Some(cur) = queue.pop_front() {
            for edge in self.edges.iter().filter(|e| e.requires && e.from == cur) {
                if self.is_blocking_missing(&edge.to) {
                    return true;
                }
                if seen.insert(edge.to.clone()) {
                    queue.push_back(edge.to.clone());
                }
            }
        }
        false
    }

    /// 全图逐节点跑一遍对拍。归档核齐视图出图前跑一次。
    pub fn check_direction_pairing_all(&self) -> Result<(), DirectionPairingError> {
        for id in self.nodes.keys() {
            self.check_direction_pairing(id)?;
        }
        Ok(())
    }

    /// 沿指定方向做 BFS，拿到**传递**可达集（不是一跳）。
    fn reachable(&self, node_id: &str, direction: Direction) -> BTreeSet<String> {
        let neighbors = |id: &str| -> Vec<String> {
            match direction {
                Direction::Dependencies => self
                    .edges
                    .iter()
                    .filter(|e| e.from == id)
                    .map(|e| e.to.clone())
                    .collect(),
                Direction::Dependents => self
                    .edges
                    .iter()
                    .filter(|e| e.to == id)
                    .map(|e| e.from.clone())
                    .collect(),
                // `Both` 走无向：正反各一遍并起来就是弱连通分量。
                Direction::Both => {
                    let mut v: Vec<String> = self
                        .edges
                        .iter()
                        .filter(|e| e.from == id || e.to == id)
                        .flat_map(|e| [e.from.clone(), e.to.clone()])
                        .collect();
                    v.retain(|x| x != id);
                    v
                }
            }
        };

        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut queue: std::collections::VecDeque<String> =
            std::collections::VecDeque::new();
        queue.push_back(node_id.to_string());
        while let Some(cur) = queue.pop_front() {
            for next in neighbors(&cur) {
                if seen.insert(next.clone()) {
                    queue.push_back(next);
                }
            }
        }
        seen
    }

    /// 单件缺件的欠件天数。**没有登记过发现时刻就返回 `None`**，
    /// 不用 `now` 兜底（见 [`closure`] 模块头口径 3）。
    pub fn missing_age(
        &self,
        node_id: &str,
        now: DateTime<FixedOffset>,
        ledger: &MissingLedger,
    ) -> Option<MissingAge> {
        let node = self.nodes.get(node_id)?;
        // 参考缺件（requires: false）不欠件，不进天数账。
        if !self.is_blocking_missing(node_id) {
            return None;
        }
        Some(Self::age_of(node, now, ledger))
    }

    /// 这个节点是不是**会阻断归档**的缺件占位。
    ///
    /// 公开是因为渲染侧（Task 34 的归档核齐视图）必须与闭包、欠件天数
    /// **用同一条口径**判「缺件」：界面上把参考缺件（`requires: false`）
    /// 染成缺件红，护士长就会收到一条根本不欠的催办。
    pub fn is_blocking_missing(&self, node_id: &str) -> bool {
        self.nodes
            .get(node_id)
            .is_some_and(|n| n.is_missing() && self.edges.iter().any(|e| e.blocking && e.to == node_id))
    }

    fn age_of(
        node: &GraphNode,
        now: DateTime<FixedOffset>,
        ledger: &MissingLedger,
    ) -> MissingAge {
        let name = node.template_id.clone();
        match ledger.get(&node.node_id) {
            Some(first_seen) if first_seen <= now => {
                let days = closure::natural_days_between(first_seen, now);
                MissingAge {
                    node_id: node.node_id.clone(),
                    encounter_alias: node.encounter_alias.clone(),
                    template_id: name.clone(),
                    first_seen,
                    days: Some(days),
                    basis_zh: format!(
                        "「{name}」自 {} 起缺，到 {} 为第 {days} 个自然日（按日历日差，不按 24 小时整除）",
                        first_seen.format("%Y-%m-%d %H:%M"),
                        now.format("%Y-%m-%d")
                    ),
                }
            }
            Some(first_seen) => MissingAge {
                node_id: node.node_id.clone(),
                encounter_alias: node.encounter_alias.clone(),
                template_id: name.clone(),
                first_seen,
                days: None,
                basis_zh: format!(
                    "「{name}」登记的发现时刻 {} 晚于核算时刻，天数算不出，需人工核对这条登记",
                    first_seen.format("%Y-%m-%d %H:%M")
                ),
            },
            None => MissingAge {
                node_id: node.node_id.clone(),
                encounter_alias: node.encounter_alias.clone(),
                template_id: name.clone(),
                first_seen: now,
                days: None,
                basis_zh: format!(
                    "「{name}」从未登记过首次发现的时刻，欠件天数算不出；不按今天兜底，以免显示成「刚缺 0 天」"
                ),
            },
        }
    }

    /// 按住院别名聚合的欠件天数：取该别名下**最老**的一条缺件。
    ///
    /// 为什么取最老：连带阻塞的一串里，最老那条才是根因；护士长按天数排序
    /// 催办时，催的必须是根因那一件。并列时取 `node_id` 最小的一条，
    /// 保证同一份数据两次算出来的答案完全一致。
    pub fn encounter_age(
        &self,
        encounter_alias: &str,
        now: DateTime<FixedOffset>,
        ledger: &MissingLedger,
    ) -> EncounterAge {
        let missing: Vec<&GraphNode> = self
            .nodes
            .values()
            .filter(|n| n.encounter_alias == encounter_alias && self.is_blocking_missing(&n.node_id))
            .collect();

        // 只在**有登记时刻且不晚于 now** 的缺件里挑最老的一条。
        let oldest = missing
            .iter()
            .filter_map(|n| ledger.get(&n.node_id).map(|at| (at, n)))
            .filter(|(at, _)| *at <= now)
            .min_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.node_id.cmp(&b.1.node_id)));

        match oldest {
            Some((_at, node)) => {
                let age = Self::age_of(node, now, ledger);
                EncounterAge {
                    encounter_alias: encounter_alias.to_string(),
                    days: age.days,
                    oldest_missing_node: Some(node.node_id.clone()),
                    missing_count: missing.len(),
                    basis_zh: format!(
                        "本住院共缺 {} 件，欠件天数按最老的一件（{}，{}）算：{}",
                        missing.len(),
                        node.template_id,
                        node.node_id,
                        age.basis_zh
                    ),
                }
            }
            None => EncounterAge {
                encounter_alias: encounter_alias.to_string(),
                days: None,
                oldest_missing_node: None,
                missing_count: missing.len(),
                basis_zh: format!(
                    "本住院共缺 {} 件，但没有任何一件登记过首次发现的时刻，欠件天数算不出",
                    missing.len()
                ),
            },
        }
    }

    /// 落成**可导出**的归档核齐视图数据结构。Task 34 渲染、Task 31 导出都用它。
    ///
    /// 出图前先跑对拍不变量：对不上就返回错误，而不是给出一份「看起来齐全」
    /// 的视图——归档核齐是给病案室签字用的，宁可报错。
    pub fn archive_view(
        &self,
        encounter_alias: &str,
        now: DateTime<FixedOffset>,
        ledger: &MissingLedger,
        rules: &psychdoc_rules::EffectiveRules,
    ) -> Result<ArchiveView, ArchiveViewError> {
        self.check_direction_pairing_all()
            .map_err(|e| ArchiveViewError::Pairing(e.to_string()))?;
        let blockage = self
            .blockage()
            .map_err(|e| ArchiveViewError::Closure(e.to_string()))?;

        let is_directly_blocked = |id: &str| -> bool {
            self.edges.iter().any(|e| e.from == id && e.blocking)
        };

        let mut rows: Vec<ArchiveRow> = self
            .nodes
            .values()
            .filter(|n| n.encounter_alias == encounter_alias)
            .map(|n| {
                let (deps, dependents) = self.both_directions(&n.node_id);
                let entry = blockage.blocked.get(&n.node_id);
                ArchiveRow {
                    node_id: n.node_id.clone(),
                    template_id: n.template_id.clone(),
                    display_zh: rules
                        .doc_types
                        .get(&n.template_id)
                        .cloned()
                        .unwrap_or_else(|| n.template_id.clone()),
                    present: !n.is_missing(),
                    directly_blocked: is_directly_blocked(&n.node_id),
                    blocked_depth: entry.map(|e| e.depth),
                    blocked_by: entry.map(|e| e.roots.clone()).unwrap_or_default(),
                    waiting_on: deps.into_iter().collect(),
                    blocking_others: dependents.into_iter().collect(),
                    missing_days: if self.is_blocking_missing(&n.node_id) {
                        Self::age_of(n, now, ledger).days
                    } else {
                        None
                    },
                }
            })
            .collect();
        rows.sort_by(|a, b| a.node_id.cmp(&b.node_id));

        let missing: Vec<MissingAge> = self
            .nodes
            .values()
            .filter(|n| n.encounter_alias == encounter_alias && self.is_blocking_missing(&n.node_id))
            .map(|n| Self::age_of(n, now, ledger))
            .collect();

        let encounter_age = self.encounter_age(encounter_alias, now, ledger);

        let in_scope = |id: &str| -> bool {
            self.nodes.get(id).is_some_and(|n| n.encounter_alias == encounter_alias)
        };

        Ok(ArchiveView {
            encounter_alias: encounter_alias.to_string(),
            as_of: now,
            rows,
            missing,
            encounter_age: Some(encounter_age),
            blocking_edges: self
                .edges
                .iter()
                .filter(|e| e.blocking && in_scope(&e.from) && in_scope(&e.to))
                .map(|e| (e.from.clone(), e.to.clone()))
                .collect(),
            soft_gaps: self
                .soft_gaps
                .iter()
                .filter(|e| in_scope(&e.from) && in_scope(&e.to))
                .map(|e| (e.from.clone(), e.to.clone()))
                .collect(),
            manual_queue: self
                .manual_queue
                .iter()
                .filter(|m| m.encounter_alias == encounter_alias)
                .map(|m| m.doc_id.clone())
                .collect(),
            basis_zh: vec![
                format!(
                    "阻塞传播只走 requires: true 的依赖边；本次住院 {encounter_alias} 缺 {missing_count} 件，连带阻塞 {blocked_count} 件，最深 {max_depth} 级",
                    missing_count = self
                        .missing_roots()
                        .iter()
                        .filter(|id| in_scope(id))
                        .count(),
                    blocked_count = blockage
                        .blocked
                        .keys()
                        .filter(|id| in_scope(id))
                        .count(),
                    max_depth = blockage
                        .blocked
                        .iter()
                        .filter(|(id, _)| in_scope(id))
                        .map(|(_, e)| e.depth)
                        .max()
                        .unwrap_or(0),
                ),
                "requires: false 的参考缺件只列在「参考缺件」里，不阻断归档，也不参与传播。".to_string(),
                "欠件天数按自然日（日历日差）计算，不按 24 小时整除；从未登记发现时刻的缺件天数显示为「算不出」，不按今天兜底。".to_string(),
            ],
        })
    }

    fn reject_if_cyclic_for_propagation(&self) -> Result<(), ClosureError> {
        if self.cycles.is_empty() {
            return Ok(());
        }
        Err(ClosureError::Cycle(
            self.audit_cycles().describe_zh(),
        ))
    }

    /// 图不变量自检。**每次出图前跑**，而不是留给 UI 崩溃时才发现。
    ///
    /// 环检测排在最前：成环时后面的不变量（悬空 / 跨住院）已经没有意义了，
    /// 先报 `E_DEP_CYCLE` 并把环路径原文带出去——规则集维护者要靠它定位改哪一行。
    pub fn check_invariants(&self) -> Result<(), InvariantError> {
        if !self.cycles.is_empty() {
            return Err(InvariantError::Cycle(self.audit_cycles().describe_zh()));
        }
        for (id, node) in &self.nodes {
            if id != &node.node_id {
                return Err(InvariantError::DuplicateNode(id.clone()));
            }
        }
        for e in self.edges.iter().chain(self.soft_gaps.iter()) {
            for endpoint in [&e.from, &e.to] {
                if !self.nodes.contains_key(endpoint) {
                    return Err(InvariantError::Dangling {
                        from: e.from.clone(),
                        to: e.to.clone(),
                        missing: endpoint.clone(),
                    });
                }
            }
            let (a, b) = (&self.nodes[&e.from], &self.nodes[&e.to]);
            if a.encounter_alias != b.encounter_alias {
                return Err(InvariantError::CrossEncounter {
                    from: e.from.clone(),
                    from_enc: a.encounter_alias.clone(),
                    to: e.to.clone(),
                    to_enc: b.encounter_alias.clone(),
                });
            }
        }
        Ok(())
    }
}

/// 出归档核齐视图失败的错误码。
///
/// 单独一个类型而不是复用 [`InvariantError`]：视图出不来和图不满足不变量是
/// 两回事——图可能完全合法（无环、无悬空、边不跨住院），但正反向对拍对不上，
/// 那说明**闭包算法**有问题，出图本身没错。这两种必须分开报，
/// 否则排查时会被引到「规则集写错了」这个错误方向上去。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ArchiveViewError {
    /// 正反向并集 ≠ 可达集，闭包算法或图本身有问题。
    #[error("E_DIR_PAIRING: {0}")]
    Pairing(String),
    /// 成环，闭包传播无解。
    #[error("E_DEP_CYCLE_PROPAGATION: {0}")]
    Closure(String),
}
