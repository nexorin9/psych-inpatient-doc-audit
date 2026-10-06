//! 节点实体解析：源产品 `crates/ruff_graph/src/resolver.rs` 的产品侧改写。
//!
//! 源侧 [`Resolver`] 逐条 `CollectedImport` 调 `ty_module_resolver` 的
//! `resolve_module` / `resolve_module_confident`，把「模块名」落成「文件路径」
//! （`resolver.rs:40-52`）。confident 变体只在**唯一命中**时给答案，
//! 命中多个则整个解析失败——源侧靠 `File` 本身携带唯一性，本产品靠
//! [`Binding`]。
//!
//! 本产品的解析回答两个问题：
//!
//! 1. **前置件在不在？** → 在：连一条实边；不在：造一个
//!    [`NodeKind::Missing`] 占位节点。`requires: true` 时这条边是**阻塞边**。
//! 2. **前置件是不是唯一？** → 不唯一（`Binding::Ambiguous`）：
//!    **不连边**，进人工队列。理由写在队列里，不静默取第一份。
//!
//! 另外：**边绝不跨住院别名**。源侧文件路径天然全局唯一，本产品如果把
//! A 住院的约束记录连到 B 住院的知情同意上，病案室看到的「已核齐」就是假的。

use std::collections::BTreeMap;

use psychdoc_model::{Binding, DocItem};
use serde::{Deserialize, Serialize};

use crate::collector::{Collected, SelfLoop};
use crate::{GraphEdge, GraphNode, NodeKind};

/// 绑定不唯一 / 未绑定的件。它们**不产生任何边**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualNode {
    pub doc_id: String,
    pub template_id: String,
    pub encounter_alias: String,
    pub source_system: String,
    pub binding: Binding,
    pub reason: String,
}

/// 解析结果。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedGraph {
    pub nodes: BTreeMap<String, GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// `requires: false` 的依赖所缺的前置件：可见、记原因，但**不阻断**。
    pub soft_gaps: Vec<GraphEdge>,
    pub manual_queue: Vec<ManualNode>,
    pub self_loops: Vec<SelfLoop>,
}

/// 节点解析器。
#[derive(Debug, Clone, Default)]
pub struct Resolver;

impl Resolver {
    pub fn new() -> Self {
        Self
    }

    /// 把 [`Collected`] 的边解析成带具体节点的 [`ResolvedGraph`]。
    ///
    /// `items` 必须与收集时用的是同一批件——解析要靠 `template_id` 反查
    /// 「这次住院里有没有这一类前置件」，只能看实际存在的件。
    pub fn resolve(&self, collected: &Collected) -> ResolvedGraph {
        self.resolve_with(collected, &[])
    }

    pub fn resolve_with(
        &self,
        collected: &Collected,
        items: &[DocItem],
    ) -> ResolvedGraph {
        let mut out = ResolvedGraph {
            self_loops: collected.self_loops.clone(),
            ..Default::default()
        };

        // 索引：只索引 `Binding::Confident` 的件。ambiguous / unresolved
        // 一律走人工队列——这是「绝不取第一份当答案」的落点。
        let mut confident: BTreeMap<(&str, &str), Vec<&DocItem>> = BTreeMap::new();
        for item in items {
            if !item.binding.is_confident() {
                out.manual_queue.push(ManualNode {
                    doc_id: item.doc_id.clone(),
                    template_id: item.template_id.clone(),
                    encounter_alias: item.encounter_alias.clone(),
                    source_system: item.source_system.clone(),
                    binding: item.binding,
                    reason: manual_reason(item),
                });
                continue;
            }
            confident
                .entry((&item.encounter_alias, &item.template_id))
                .or_default()
                .push(item);
        }

        // 真实件入节点表。
        for items in confident.values() {
            for item in items {
                out.nodes.insert(
                    item.doc_id.clone(),
                    GraphNode {
                        node_id: item.doc_id.clone(),
                        encounter_alias: item.encounter_alias.clone(),
                        template_id: item.template_id.clone(),
                        kind: NodeKind::Present,
                        doc_id: Some(item.doc_id.clone()),
                        source_system: item.source_system.clone(),
                        binding: item.binding,
                    },
                );
            }
        }

        for edge in &collected.edges {
            // 依赖方自己绑定不唯一 → 这条边不成立。
            //
            // 收集期是**逐件**按 template_id 走规范表的，因此一件
            // `Ambiguous` 的约束记录照样会收集到「它要求约束知情同意」；
            // 但它到底以哪一份为准都没定，连边就是拿一份猜的。
            // 缺件占位同样不许建——「找不到」和「不敢认」是两回事。
            if !out.nodes.contains_key(&edge.from_doc_id) {
                if !out.manual_queue.iter().any(|m| m.doc_id == edge.from_doc_id) {
                    let reason = format!(
                        "依赖方 {} 绑定不唯一，其对 {} 的依赖边不成立，需人工确认以哪份为准",
                        edge.from_doc_id, edge.spec_to
                    );
                    out.manual_queue.push(ManualNode {
                        doc_id: edge.from_doc_id.clone(),
                        template_id: edge.spec_from.clone(),
                        encounter_alias: edge.encounter_alias.clone(),
                        source_system: String::new(),
                        binding: Binding::Ambiguous,
                        reason,
                    });
                }
                continue;
            }
            // 只在**同一次住院**内找前置件：跨住院连边等于伪造归档核齐。
            let key = (edge.encounter_alias.as_str(), edge.spec_to.as_str());
            let found = confident.get(&key).map(Vec::as_slice).unwrap_or(&[]);

            match found.len() {
                0 => {
                    // 缺件：造占位节点。requires 决定这条边阻不阻断归档。
                    let missing_id = GraphNode::missing_id(&edge.encounter_alias, &edge.spec_to);
                    out.nodes.entry(missing_id.clone()).or_insert_with(|| GraphNode {
                        node_id: missing_id.clone(),
                        encounter_alias: edge.encounter_alias.clone(),
                        template_id: edge.spec_to.clone(),
                        kind: NodeKind::Missing,
                        doc_id: None,
                        source_system: String::new(),
                        binding: Binding::Confident,
                    });
                    let e = GraphEdge {
                        from: edge.from_doc_id.clone(),
                        to: missing_id,
                        requires: edge.requires,
                        blocking: edge.requires,
                        spec_from: edge.spec_from.clone(),
                        spec_to: edge.spec_to.clone(),
                    };
                    if edge.requires {
                        out.edges.push(e);
                    } else {
                        out.soft_gaps.push(e);
                    }
                }
                1 => {
                    let target = &found[0];
                    let e = GraphEdge {
                        from: edge.from_doc_id.clone(),
                        to: target.doc_id.clone(),
                        requires: edge.requires,
                        // 前置件在 → 不阻塞。
                        blocking: false,
                        spec_from: edge.spec_from.clone(),
                        spec_to: edge.spec_to.clone(),
                    };
                    if edge.requires {
                        out.edges.push(e);
                    } else {
                        out.soft_gaps.push(e);
                    }
                }
                n => {
                    // 理论上到不了这里：binder 见到同 (住院, 类型) 多系统已
                    // 全标 Ambiguous。留着是为了**不静默取第一份**——
                    // 真出现 n>1，宁可这条边不成立并记人工。
                    out.manual_queue.push(ManualNode {
                        doc_id: found[0].doc_id.clone(),
                        template_id: edge.spec_to.clone(),
                        encounter_alias: edge.encounter_alias.clone(),
                        source_system: n.to_string(),
                        binding: Binding::Ambiguous,
                        reason: format!(
                            "前置件 {} 有 {n} 份候选，依赖边不成立，需人工确认以哪份为准",
                            edge.spec_to
                        ),
                    });
                }
            }
        }
        out
    }
}

/// 人工队列的理由。措辞与绑定层（`EntityBinder`）保持一致：
/// 都是「哪件、为什么没人敢自动处理、要人工确认什么」三件事，缺一不可。
fn manual_reason(item: &DocItem) -> String {
    match item.binding {
        Binding::Ambiguous => format!(
            "同一住院下 {} 有多个系统各一份（如 {}），绑定不唯一，需人工确认以哪份为准",
            item.template_id, item.source_system
        ),
        Binding::Unresolved => format!(
            "类型码 {} 未在本院文书类型字典内确认，需人工确认归属后再出图",
            item.template_id
        ),
        Binding::Confident => String::new(),
    }
}
