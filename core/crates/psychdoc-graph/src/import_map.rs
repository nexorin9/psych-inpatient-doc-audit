//! 正反向依赖表：源产品 `crates/ruff_graph/src/lib.rs::ImportMap` 的产品侧改写。
//!
//! 源侧 `ImportMap` 只有两个构造函数，来自**同一份** `(path, imports)` 输入：
//!
//! - `dependencies`（`lib.rs:111-118`）：原样插入，`file -> 它 import 的模块`；
//! - `dependents`（`lib.rs:122-131`）：每条边**反向再插一次**，
//!   `被 import 的模块 -> import 它的 file`。
//!
//! 配合 `analyze_graph.rs` 里的 `Direction` 开关，用户一条命令拿到整张图，
//! 换方向即问出「这份缺件卡住谁」「这份未收谁在等」。
//!
//! 本产品保留这个形状：正反两个答案来自**同一批边**，
//! 因此「正反向并集 = 可达集」这条不变量天然可测（Task 21 用它守住闭包算法）。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// 方向开关。保留源产品同名同形的枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// 「这件在等谁」——件 → 它要求的前置件。
    Dependencies,
    /// 「这件卡住谁」——前置件 → 要求它的件。
    Dependents,
    /// 两个方向并起来。
    Both,
}

/// 一张正/反向依赖表：`节点 id → 相邻节点 id 集合`。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImportMap(BTreeMap<String, BTreeSet<String>>);

impl ImportMap {
    /// 正向：件 → 它要求的前置件。
    pub fn dependencies<I, S>(edges: I) -> Self
    where
        I: IntoIterator<Item = (S, S)>,
        S: Into<String>,
    {
        let mut map = ImportMap::default();
        for (from, to) in edges {
            map.0.entry(from.into()).or_default().insert(to.into());
        }
        map
    }

    /// 反向：前置件 → 要求它的件。每条边反向再插一次。
    pub fn dependents<I, S>(edges: I) -> Self
    where
        I: IntoIterator<Item = (S, S)>,
        S: Into<String>,
    {
        let mut reverse = ImportMap::default();
        for (from, to) in edges {
            reverse.0.entry(to.into()).or_default().insert(from.into());
        }
        reverse
    }

    /// 取某个节点的邻居集合。节点不存在时返回空集（源侧 `BTreeMap::get` 语义）。
    pub fn get(&self, node: &str) -> Option<&BTreeSet<String>> {
        self.0.get(node)
    }

    /// 表里的节点总数（含只出现在某一侧、另一侧没有邻居的节点）。
    pub fn node_count(&self) -> usize {
        self.0.len()
    }

    /// 全部边数（邻居对数）。
    pub fn edge_count(&self) -> usize {
        self.0.values().map(BTreeSet::len).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn nodes(&self) -> impl Iterator<Item = &String> {
        self.0.keys()
    }
}
