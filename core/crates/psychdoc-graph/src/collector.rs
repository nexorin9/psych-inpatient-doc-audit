//! 依赖边收集：源产品 `crates/ruff_graph/src/collector.rs` 的产品侧改写。
//!
//! 源侧 [`Collector`] 用 `SourceOrderVisitor` 逐个 AST 节点走一遍，把
//! `import` / `from … import` 语句收集成 [`CollectedImport`]（`collector.rs:49-55`）。
//! 本产品没有 AST，改为**按住院别名逐件遍历** `DocItem`：每件按 `template_id`
//! 匹配规则集声明的依赖规范 `from`，把「这件要求哪一类前置件」收集成
//! [`CollectedEdge`]。
//!
//! 收集期就处理掉三件事，**不留给出图时**：
//!
//! 1. **去重**：三层级联可能把同一条边声明两遍（院级 + 科室级各写一次），
//!    收集时按 `(住院别名, from 件 id, to 类型码)` 收敛成一条。
//! 2. **自环检测**：`from == to` 的边（约束记录依赖约束记录）在收集期即被
//!    拒绝——源侧的自环 import 是合法的（本模块自己 import 自己），
//!    但院内文书依赖**自环意味着这份永远等不到**，必须拦下。
//! 3. **悬空规范**：依赖规范引用了院内类型字典里没有的类型码 →
//!    [`DanglingSpec`]，显式记账，不静默丢弃。

use std::collections::{BTreeMap, BTreeSet};

use psychdoc_model::DocItem;
use psychdoc_rules::{DependencyDecl, EffectiveRules};
use serde::{Deserialize, Serialize};

/// 收集到的一条边：**边已定位到具体件，尚未解析前置件是否存在**。
///
/// 刻意分「收集」与「解析」两步（对齐源侧 `Collector` → `Resolver`）：
/// 收集只回答「谁要求什么」，解析才回答「要求的东西在不在、是不是唯一」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectedEdge {
    pub encounter_alias: String,
    /// 依赖方（后件）的 `Composition.id`。
    pub from_doc_id: String,
    /// 依赖方的类型码（边的规范来源侧）。
    pub spec_from: String,
    /// 被依赖方（前置件）的**类型码**——此时还不知道具体是哪一件。
    pub spec_to: String,
    pub requires: bool,
}

impl CollectedEdge {
    /// 去重键。同一条边被院级与科室级各声明一次时收敛成一条。
    pub fn dedup_key(&self) -> (&str, &str, &str) {
        (&self.encounter_alias, &self.from_doc_id, &self.spec_to)
    }
}

/// 引用了院内类型字典里没有的类型码的依赖规范。
///
/// 规则集加载期已经拦过一次（`E_RULESET_INVALID`），这里是**第二道防线**：
/// 出图路径可能拿到非加载器产出的依赖声明，静默丢弃等于把文书依赖搞错。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DanglingSpec {
    pub from: String,
    pub to: String,
    pub reason: String,
}

/// 自环依赖（`from == to`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelfLoop {
    pub encounter_alias: String,
    pub doc_id: String,
    pub template_id: String,
}

/// 收集结果。`edges` 之外的每个字段都是「被拒绝的东西」，**都必须可见**。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collected {
    pub edges: Vec<CollectedEdge>,
    pub self_loops: Vec<SelfLoop>,
    pub dangling_specs: Vec<DanglingSpec>,
    /// 每条边收集时命中的规范行号，供「这条边是哪条规范来的」回查。
    pub encounters: BTreeSet<String>,
}

/// 依赖边收集器。
#[derive(Debug, Clone, Default)]
pub struct Collector;

impl Collector {
    pub fn new() -> Self {
        Self
    }

    /// 从解析结果 + 生效规则集收集依赖边。
    pub fn collect(&self, items: &[DocItem], rules: &EffectiveRules) -> Collected {
        self.collect_deps(items, &rules.dependencies, &rules.doc_types)
    }

    /// 收集的底层入口。`doc_types` 是院内类型字典（码 → 中文名），
    /// 用来判断依赖规范是否悬空。
    pub fn collect_deps(
        &self,
        items: &[DocItem],
        deps: &[DependencyDecl],
        doc_types: &BTreeMap<String, String>,
    ) -> Collected {
        let mut out = Collected::default();
        // 同一条依赖规范会被三层级联各声明一次；按 from 侧类型码建索引，
        // 免得每件都全量扫一遍规范表。
        let mut by_from: BTreeMap<&str, Vec<&DependencyDecl>> = BTreeMap::new();
        for d in deps {
            // 悬空规范：任一侧类型码不在字典内 → 记账并整条跳过。
            if !doc_types.contains_key(&d.from) {
                out.dangling_specs.push(DanglingSpec {
                    from: d.from.clone(),
                    to: d.to.clone(),
                    reason: format!("依赖来源类型码 {} 不在本院文书类型字典内", d.from),
                });
                continue;
            }
            if !doc_types.contains_key(&d.to) {
                out.dangling_specs.push(DanglingSpec {
                    from: d.from.clone(),
                    to: d.to.clone(),
                    reason: format!("依赖目标类型码 {} 不在本院文书类型字典内", d.to),
                });
                continue;
            }
            by_from.entry(&d.from).or_default().push(d);
        }

        let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
        for item in items {
            out.encounters.insert(item.encounter_alias.clone());
            let Some(decls) = by_from.get(item.template_id.as_str()) else {
                continue;
            };
            for d in decls {
                if d.from == d.to {
                    // 自环：这份文书要求「先有自己」，永远等不到。
                    out.self_loops.push(SelfLoop {
                        encounter_alias: item.encounter_alias.clone(),
                        doc_id: item.doc_id.clone(),
                        template_id: item.template_id.clone(),
                    });
                    continue;
                }
                let edge = CollectedEdge {
                    encounter_alias: item.encounter_alias.clone(),
                    from_doc_id: item.doc_id.clone(),
                    spec_from: d.from.clone(),
                    spec_to: d.to.clone(),
                    requires: d.requires,
                };
                let key = (
                    edge.encounter_alias.clone(),
                    edge.from_doc_id.clone(),
                    edge.spec_to.clone(),
                );
                if seen.insert(key) {
                    out.edges.push(edge);
                }
            }
        }
        out
    }
}
