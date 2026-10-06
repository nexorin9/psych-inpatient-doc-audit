//! 审查队列：把一次扫描的结果按**住院别名**聚合成左栏的一列条目。
//!
//! ## 为什么放在 parse 层而不是桌面壳
//!
//! 护士长看到的左栏不是「文件夹里的文件列表」，而是「**哪几次住院、
//! 每次还差几件**」。这个口径有三个���素：
//!
//! - 件数：本次包里解析出多少件（不是文件数——一个文件里可能装 4 件）；
//! - 缺件数：依赖图算出来的、会阻断归档的**缺件占位**数
//!   （Task 21 的 `blocking_missing_ids` 口径，**不含** `requires: false`
//!   的参考缺件）；
//! - 病区：从住院别名的 `WARD-x-nnn` 形态里取中间段。
//!
//! 三个里任何一个放到壳里现算，都会让 GUI 与 CLI 对同一份包给出
//! 不同的缺件数——那是审计上答不了的问题（「导出包没变，为什么昨天说缺 2 件
//! 今天说缺 1 件」）。因此本模块只做**纯数据聚合**：输入件与缺件 id，
//! 输出可序列化、可逐字节比对的队列条目。
//!
//! **本模块不判任何规则**：它不知道 `QC-PSY-nn`，也不数诊断条数。
//! 阻断项数由 `psychdoc-rules` 的 `ReviewResult` 给（见 FFI 层 `encounter_rows`）。

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::discovery::FileSkip;

/// 左栏一条住院条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    /// 住院别名（脱敏），跨件 join 的键。
    pub encounter_alias: String,
    /// 病区码（从别名形态解析；解析不出时为空串，**不编一个**）。
    pub ward: String,
    /// 本次包里解析出的件数。
    pub doc_count: usize,
    /// 会阻断归档的缺件数（只算 `requires: true` 的真缺件）。
    pub missing_count: usize,
    /// 本次包里有哪些件（升序，供选中后聚焦第一条被卡住的件）。
    pub doc_ids: Vec<String>,
    /// 这些件分别来自哪些导出文件（相对扫描根），护士长要能回溯「哪份文件少了什么」。
    pub source_files: Vec<String>,
}

impl QueueEntry {
    /// 左栏那一行：别名 + 件数 + 缺件数徽标。
    pub fn badge_zh(&self) -> String {
        if self.missing_count == 0 {
            format!("{} 件 · 达标", self.doc_count)
        } else {
            format!("{} 件 · 缺件 {}", self.doc_count, self.missing_count)
        }
    }

    /// 可归档？缺件为 0 才算。
    pub fn is_complete(&self) -> bool {
        self.missing_count == 0
    }
}

/// 一次扫描聚合出来的完整队列。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewQueue {
    pub entries: Vec<QueueEntry>,
    /// 打不开的文件（杂项 / 坏 JSON / 不是 Bundle）。
    pub file_skips: Vec<FileSkip>,
    /// 解析到件但被院内类型字典或结构规则过滤掉的 entry 汇总。
    pub filtered_count: usize,
    /// 走过的目录数 / 看到的文件数 / 成功打开的包文件数。
    pub dirs_visited: usize,
    pub files_seen: usize,
    /// 成功打开的导出包文件数（**含**打开后一件都没出的那些）。
    pub files_opened: usize,
}

impl ReviewQueue {
    /// 件数守恒：件总数 == 各条目的 `doc_count` 之和。
    pub fn item_count(&self) -> usize {
        self.entries.iter().map(|e| e.doc_count).sum()
    }

    /// 文件守恒：`files_seen == files_opened + file_skips.len()`。
    ///
    /// 「打开的包文件」必须单独记，不能拿「出件数 ≥ 1」近似：
    /// 一份全被字典过滤掉的包**打开过**，只是没出件。
    /// 护士长看到「扫了 20 个文件、跳过 3 个、审了 0 件」时，
    /// 这条等式能立刻指出少了 17 个文件没记账。
    pub fn is_balanced(&self) -> bool {
        self.files_seen == self.files_opened + self.file_skips.len()
    }

    /// 缺件数合计（底栏「本次共欠 N 件」）。
    pub fn missing_total(&self) -> usize {
        self.entries.iter().map(|e| e.missing_count).sum()
    }

    /// 按住院别名取一条。
    pub fn find(&self, alias: &str) -> Option<&QueueEntry> {
        self.entries.iter().find(|e| e.encounter_alias == alias)
    }
}

/// 序号段的形态：**可选的一个字母 + 数字**。
///
/// 院内脱敏别名有两种写法，两种都要认：
///
/// - `WARD-A-001` —— 病区段 `WARD-A` + 序号段 `001`（`fixtures/` 里的形态）
/// - `SYNTH-W001` —— 病区段 `SYNTH` + 序号段 `W001`
///   （`psychdoc_parse::synth` 的形态，合成基准按它批量生成）
///
/// 纯字母（`A`）**不算**序号段——那样 `WARD-A` 会被误认成「病区 WARD」。
fn is_seq_segment(seg: &str) -> bool {
    let digits = seg
        .strip_prefix(|c: char| c.is_ascii_alphabetic())
        .unwrap_or(seg);
    !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit())
}

/// 从住院别名里取病区码：**末段是序号段时，它之前的那一段就是病区**。
///
/// **形态不认就不猜**：别名没有序号段（`WARD-A`）或压根没有连字符
/// （`WARDX`）就返回空串，让左栏显示「未标注病区」而不是编一个「A 病区」。
///
/// 特意不做的推断：按文件所在目录名倒推病区。两个病区可能共用一个
/// 导出目录，那会给出**看起来合理但错的**病区——比留空更危险。
pub fn ward_of(encounter_alias: &str) -> String {
    let (head, last) = match encounter_alias.rsplit_once('-') {
        Some(pair) => pair,
        None => return String::new(),
    };
    if head.is_empty() || !is_seq_segment(last) {
        return String::new();
    }
    head.to_string()
}

/// 聚合输入：件（已解析）、缺件节点 id、件→源文件的映射、打不开的文件。
///
/// 刻意做成纯数据结构而不是一串函数参数——调用方（CLI 目录模式、
/// FFI `review_batch`、桌面壳扫描）三处都要用同一份聚合，
/// 参数顺序错了编译器会拦，结构体字段错了解析器会拦。
#[derive(Debug, Clone, Default)]
pub struct QueueInput {
    /// 本次扫描解析出的所有件（跨住院别名混在一起）。
    pub items: Vec<psychdoc_model::DocItem>,
    /// 会阻断归档的缺件占位节点 id（来自 `DocGraph::blocking_missing_ids`）。
    pub missing_node_ids: Vec<String>,
    /// 件 id → 它来自哪个导出文件（相对扫描根）。
    pub source_of: BTreeMap<String, String>,
    /// 打不开的文件。
    pub file_skips: Vec<FileSkip>,
    /// 被过滤掉的 entry 数（字典外 / 非文书正文 / 日期坏）。
    pub filtered_count: usize,
    pub dirs_visited: usize,
    pub files_seen: usize,
    /// 成功打开的导出包文件数（含打开后一件都没出的那些）。
    pub files_opened: usize,
}

/// 按住院别名聚合成队列。
///
/// 排序：**缺件数降序 + 件数降序 + 别名升序**。
/// 「最该先看哪份」排在最前，且两次运行逐字节一致（审计导出要能 diff）。
pub fn aggregate(input: QueueInput) -> ReviewQueue {
    let mut groups: BTreeMap<String, QueueEntry> = BTreeMap::new();

    for item in &input.items {
        let entry = groups
            .entry(item.encounter_alias.clone())
            .or_insert_with(|| QueueEntry {
                encounter_alias: item.encounter_alias.clone(),
                ward: ward_of(&item.encounter_alias),
                doc_count: 0,
                missing_count: 0,
                doc_ids: Vec::new(),
                source_files: Vec::new(),
            });
        entry.doc_count += 1;
        entry.doc_ids.push(item.doc_id.clone());
        if let Some(src) = input.source_of.get(&item.doc_id) {
            entry.source_files.push(src.clone());
        }
    }

    // 缺件节点 id 形如 `<WARD-A-001>::missing::restraint_consent`：
    // 住院别名就在 `<>` 里。取不到就**不计入任何条目**——
    // 一条缺件挂错住院，比少显示一个数字更难查。
    let mut missing_by_alias: BTreeMap<String, usize> = BTreeMap::new();
    for node_id in &input.missing_node_ids {
        let Some(alias) = alias_of_missing(node_id) else {
            continue;
        };
        *missing_by_alias.entry(alias.to_string()).or_insert(0) += 1;
    }
    for (alias, n) in missing_by_alias {
        if let Some(entry) = groups.get_mut(&alias) {
            entry.missing_count = n;
        }
    }

    let mut entries: Vec<QueueEntry> = groups.into_values().collect();
    for e in entries.iter_mut() {
        e.doc_ids.sort();
        e.doc_ids.dedup();
        e.source_files.sort();
        e.source_files.dedup();
    }
    entries.sort_by(|a, b| {
        b.missing_count
            .cmp(&a.missing_count)
            .then_with(|| b.doc_count.cmp(&a.doc_count))
            .then_with(|| a.encounter_alias.cmp(&b.encounter_alias))
    });

    ReviewQueue {
        entries,
        file_skips: input.file_skips,
        filtered_count: input.filtered_count,
        dirs_visited: input.dirs_visited,
        files_seen: input.files_seen,
        files_opened: input.files_opened,
    }
}

/// 从缺件占位节点 id 里取住院别名。形如 `<WARD-A-001>::missing::xxx`。
pub fn alias_of_missing(node_id: &str) -> Option<&str> {
    let inner = node_id.strip_prefix('<')?;
    let end = inner.find(">::missing::")?;
    Some(&inner[..end])
}

/// 选中若干条时的批量审查计划：保持左栏的展示顺序，逐条列出。
///
/// 返回的是**别名列表**而不是条目本身：批量审查要在工作线程里逐条跑，
/// 传的应该是「审哪几次住院」这一句指令，而不是把界面对象搬进线程。
pub fn plan_batch(entries: &[QueueEntry], selected: &BTreeSet<String>) -> Vec<String> {
    entries
        .iter()
        .filter(|e| selected.contains(&e.encounter_alias))
        .map(|e| e.encounter_alias.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::DocItem;

    fn item(doc_id: &str, alias: &str) -> DocItem {
        DocItem::new(doc_id, "restraint_record", alias)
    }

    #[test]
    fn ward_is_taken_from_the_alias_shape_and_never_guessed() {
        // fixtures/ 的三段形态与 synth 的两段形态都要认
        assert_eq!(ward_of("WARD-A-001"), "WARD-A");
        assert_eq!(ward_of("SYNTH-W001"), "SYNTH");
        // 形态不认 → 留空，不编一个「A 病区」
        assert_eq!(ward_of("WARD-A"), "", "末段纯字母不是序号段");
        assert_eq!(ward_of("WARDX"), "", "没有连字符就没有病区段可取");
        assert_eq!(ward_of("WARD-A-001-EXTRA"), "");
        assert_eq!(ward_of("WARD-A-ABC"), "");
        assert_eq!(ward_of("WARD-A-"), "", "空段不是序号段");
        assert_eq!(ward_of("-001"), "", "病区段为空");
        assert_eq!(ward_of(""), "");
    }

    #[test]
    fn entries_are_grouped_by_encounter_alias() {
        let q = aggregate(QueueInput {
            items: vec![
                item("D-1", "WARD-A-001"),
                item("D-2", "WARD-A-001"),
                item("D-3", "WARD-B-001"),
            ],
            ..Default::default()
        });
        assert_eq!(q.entries.len(), 2);
        let a = q.find("WARD-A-001").unwrap();
        assert_eq!(a.doc_count, 2);
        assert_eq!(a.ward, "WARD-A");
        assert_eq!(q.item_count(), 3);
    }

    #[test]
    fn missing_count_comes_from_the_node_id_and_only_counts_its_own_encounter() {
        let q = aggregate(QueueInput {
            items: vec![item("D-1", "WARD-A-001"), item("D-2", "WARD-B-001")],
            missing_node_ids: vec![
                "<WARD-A-001>::missing::restraint_consent".into(),
                "<WARD-A-001>::missing::restraint_record".into(),
                // 拼不出住院别名的缺件不计入任何条目，而不是乱挂一份住院
                "garbage-node-id".into(),
            ],
            ..Default::default()
        });
        assert_eq!(q.find("WARD-A-001").unwrap().missing_count, 2);
        assert_eq!(q.find("WARD-B-001").unwrap().missing_count, 0);
        assert_eq!(q.missing_total(), 2);
    }

    #[test]
    fn ordering_puts_the_most_missing_first_and_is_reproducible() {
        let input = QueueInput {
            items: vec![
                item("D-A1", "WARD-A-001"),
                item("D-A2", "WARD-A-001"),
                item("D-B1", "WARD-B-001"),
            ],
            missing_node_ids: vec![
                "<WARD-B-001>::missing::a".into(),
                "<WARD-A-001>::missing::a".into(),
                "<WARD-A-001>::missing::b".into(),
            ],
            ..Default::default()
        };
        let q = aggregate(input.clone());
        // A 缺 2 件、B 缺 1 件 → A 排最前；同为 0 缺件的没有
        assert_eq!(q.entries[0].encounter_alias, "WARD-A-001");
        assert_eq!(q.entries[0].missing_count, 2);
        assert_eq!(q.entries[1].encounter_alias, "WARD-B-001");
        assert_eq!(q.entries[1].missing_count, 1);

        // 件数相同时按别名升序：给 D 补一件（缺 1 件），应排在 B 之后
        let mut more = input.clone();
        more.items.push(item("D-D1", "WARD-D-001"));
        more.missing_node_ids.push("<WARD-D-001>::missing::a".into());
        let q2 = aggregate(more);
        assert_eq!(
            q2.entries.iter().map(|e| e.encounter_alias.as_str()).collect::<Vec<_>>(),
            vec!["WARD-A-001", "WARD-B-001", "WARD-D-001"]
        );

        // 同输入两次聚合逐字段相同（审计导出要能 diff）
        assert_eq!(aggregate(input), q);
    }

    #[test]
    fn badge_says_missing_items_not_violations() {
        let e = QueueEntry {
            encounter_alias: "WARD-A-001".into(),
            ward: "WARD-A".into(),
            doc_count: 4,
            missing_count: 2,
            doc_ids: vec![],
            source_files: vec![],
        };
        assert_eq!(e.badge_zh(), "4 件 · 缺件 2");
        assert!(!e.is_complete());
        let done = QueueEntry { missing_count: 0, ..e.clone() };
        assert_eq!(done.badge_zh(), "4 件 · 达标");
        assert!(done.is_complete());
    }

    #[test]
    fn plan_batch_keeps_the_display_order_of_the_left_pane() {
        let mk = |alias: &str| QueueEntry {
            encounter_alias: alias.into(),
            ward: ward_of(alias),
            doc_count: 1,
            missing_count: 0,
            doc_ids: vec![],
            source_files: vec![],
        };
        let entries = vec![mk("WARD-B-001"), mk("WARD-A-001"), mk("WARD-C-001")];
        let mut sel = BTreeSet::new();
        sel.insert("WARD-C-001".to_string());
        sel.insert("WARD-A-001".to_string());
        assert_eq!(
            plan_batch(&entries, &sel),
            vec!["WARD-B-001".to_string(), "WARD-A-001".to_string(), "WARD-C-001".to_string()]
                .into_iter()
                .filter(|a| sel.contains(a))
                .collect::<Vec<_>>()
        );
    }
}