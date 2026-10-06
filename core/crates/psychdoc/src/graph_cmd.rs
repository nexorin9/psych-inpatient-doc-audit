//! `psychdoc graph` —— 归档核齐视图的无 GUI 出口。
//!
//! 界面上的「归档核齐」标签页由桌面端渲染（Task 34），但**病案室不会为跑批
//! 去开一个窗口**：夜间归档核齐由定时任务做，质控办的月度抽查由脚本做。
//! 这个子命令就是把同一份 `DocGraph::archive_view` 打到 stdout 上，
//! 与界面读的是同一个函数、同一份数据、同一套口径——
//! 「界面说还差 2 件，脚本说齐了」这种事一旦发生，核齐签字就没依据了。
//!
//! ## `--direction` 为什么必须真的分两个方向
//!
//! 正向问的是「这件在等谁」，反向问的是「这件卡住了谁」。这两个答案
//! **必须不同**，且正反向并集要等于可达集减去自身。`check_direction_pairing_all`
//! 在出图前先跑这道对拍，对不上就拒出视图——给一份「看起来齐全」的视图去签字，
//! 比报错糟得多。
//!
//! 退出码：`0` = 已核齐；`1` = 还没核齐（缺件 / 连带阻塞 / 人工待办）。
//! CI 拿它当「这份病历能不能归档」的闸门，与 `check` 的「有没有阻断项」同族。

use crate::contract::{CliError, EXIT_FINDINGS, EXIT_OK};
use crate::sink;
use chrono::DateTime;
use psychdoc_graph::{DocGraph, MissingLedger};
use serde_json::json;
use std::path::Path;

/// `--direction` 的三个取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectionArg {
    Both,
    Dependencies,
    Dependents,
}

impl DirectionArg {
    pub fn parse(s: &str) -> Result<Self, CliError> {
        match s {
            "both" => Ok(Self::Both),
            "dependencies" => Ok(Self::Dependencies),
            "dependents" => Ok(Self::Dependents),
            other => Err(CliError::usage(format!(
                "--direction 只收 both / dependencies / dependents，收到 {other}"
            ))),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Both => "both",
            Self::Dependencies => "dependencies",
            Self::Dependents => "dependents",
        }
    }

    fn shows_dependencies(&self) -> bool {
        matches!(self, Self::Both | Self::Dependencies)
    }

    fn shows_dependents(&self) -> bool {
        matches!(self, Self::Both | Self::Dependents)
    }
}

/// 跑一次 `graph`。`json_mode` 决定人类文字是上 stdout 还是只进日志。
pub fn run(
    encounter: &str,
    input: &str,
    ruleset_yaml: &str,
    direction: DirectionArg,
    db: Option<&str>,
    as_of: Option<&str>,
    json_mode: bool,
) -> Result<u8, CliError> {
    let ruleset = psychdoc_rules::load_effective(ruleset_yaml)
        .map_err(|e| CliError::new("E_RULESET_INVALID", e.to_string()))?;
    let registry = ruleset.registry();

    // 输入既可以是一份导出包，也可以是整个病区目录——与 `check` 同一套
    // 发现 / 类型过滤链路，两条命令对「哪些件算文书」必须给同一个答案。
    let path = Path::new(input);
    let mut items = Vec::new();
    if path.is_dir() {
        let report = psychdoc_parse::discover_dir_report_with_registry(path, Some(&registry))
            .map_err(|e| CliError::new("E_IO", e.to_string()))?;
        for (_, pkg) in &report.packages {
            items.extend(pkg.items.iter().cloned());
        }
    } else {
        let bytes = std::fs::read(path)
            .map_err(|e| CliError::new("E_IO", format!("读不到文书包 {input} — {e}")))?;
        let pkg = psychdoc_parse::parse_package_bytes_with(&bytes, Some(&registry))
            .map_err(|e| CliError::new("E_BUNDLE_SHAPE", e.to_string()))?;
        items.extend(pkg.items);
    }

    let graph = DocGraph::build(&items, &ruleset);
    // 不变量先跑：成环 / 悬空 / 跨住院一律拒出图。
    graph.check_invariants().map_err(|e| match e {
        psychdoc_graph::InvariantError::Cycle(_) => CliError::new("E_DEP_CYCLE", e.to_string()),
        other => CliError::new("E_DEP_INVARIANT", other.to_string()),
    })?;

    // 住院别名必须真在这份输入里。空视图会被读成「这次住院已核齐」，
    // 而真相是「你问的是一次不存在的住院」——两种读法差着一次误归档。
    if !graph
        .nodes
        .values()
        .any(|n| n.encounter_alias == encounter)
    {
        return Err(CliError::new(
            "E_ENCOUNTER_UNKNOWN",
            format!("这份输入里没有住院别名 {encounter} 的任何一件文书——空视图会被读成「已核齐」"),
        ));
    }

    let now = match as_of {
        Some(t) => DateTime::parse_from_rfc3339(t)
            .map_err(|e| CliError::usage(format!("--as-of {t} 不是 RFC3339 时刻：{e}")))?,
        None => chrono::Local::now().fixed_offset(),
    };

    let ledger = load_ledger(db, &items)?;
    let view = graph
        .archive_view(encounter, now, &ledger, &ruleset)
        .map_err(|e| CliError::new("E_DIR_PAIRING", e.to_string()))?;

    print_view(&graph, &view, &ruleset, direction);

    if json_mode {
        let mut body = serde_json::to_value(&view).map_err(|e| {
            CliError::new("E_DIR_PAIRING", format!("视图序列化失败：{e}"))
        })?;
        // 方向收窄是**呈现**选择，不改数据：JSON 里也只给被问的那一侧，
        // 否则 CI 会拿 `both` 的答案去判「只查了反向」的脚本。
        narrow_json(&mut body, direction);
        body["direction"] = json!(direction.as_str());
        body["is_ready"] = json!(view.is_ready());
        sink::emit_json(
            &serde_json::to_string_pretty(&body)
                .map_err(|e| CliError::new("E_DIR_PAIRING", e.to_string()))?,
        );
    }

    Ok(if view.is_ready() { EXIT_OK } else { EXIT_FINDINGS })
}

/// 按 `--direction` 收窄 JSON：没被问的那一侧置空数组，**不删键**。
///
/// 删键会让下游脚本写 `row.get("waiting_on", [])` 然后永远拿到空列表，
/// 且没有任何报错。留一个空数组，缺什么一眼可见。
fn narrow_json(body: &mut serde_json::Value, direction: DirectionArg) {
    let Some(rows) = body.get_mut("rows").and_then(|r| r.as_array_mut()) else {
        return;
    };
    for row in rows {
        if !direction.shows_dependencies() {
            row["waiting_on"] = json!([]);
        }
        if !direction.shows_dependents() {
            row["blocking_others"] = json!([]);
        }
    }
}

/// 缺件登记簿：从本机库里读「首次进入缺失态的时刻」。
///
/// 库读不到就交**空簿**，让 `days` 保持 `None` 并由视图自己的口径说明交代。
/// 拿当前时间兜底会把「已欠 5 天」显示成「今天才发现」——那不是保守，
/// 那是在审计件上写一个假数字。
fn load_ledger(
    db: Option<&str>,
    items: &[psychdoc_model::DocItem],
) -> Result<MissingLedger, CliError> {
    let mut ledger = MissingLedger::new();
    let Some(db) = db else {
        return Ok(ledger);
    };
    let store = psychdoc_store::Store::open(db).map_err(|e| CliError::new(e.code(), e.to_string()))?;
    for it in items {
        let Some(state) = store
            .state_of(&it.doc_id)
            .map_err(|e| CliError::new(e.code(), e.to_string()))?
        else {
            continue;
        };
        if state != psychdoc_model::state::DocState::Missing {
            continue;
        }
        let Some(at) = store
            .missing_since(&it.doc_id)
            .map_err(|e| CliError::new(e.code(), e.to_string()))?
            .and_then(|t| DateTime::parse_from_rfc3339(&t).ok())
        else {
            continue;
        };
        ledger.record(it.doc_id.clone(), at);
    }
    Ok(ledger)
}

fn print_view(
    graph: &DocGraph,
    view: &psychdoc_graph::ArchiveView,
    rules: &psychdoc_rules::EffectiveRules,
    direction: DirectionArg,
) {
    sink::say(&format!(
        "归档核齐视图：{}（核算时刻 {}）",
        view.encounter_alias,
        view.as_of.to_rfc3339()
    ));
    sink::say(&format!(
        "结论：{}",
        if view.is_ready() {
            "已核齐（零缺件 · 零连带阻塞 · 零人工待办）"
        } else {
            "未核齐"
        }
    ));
    sink::say(&format!("方向：{}", direction.as_str()));
    sink::say("");

    sink::say(&format!("件清单（{} 件）：", view.rows.len()));
    for row in &view.rows {
        let state = if !row.present {
            "缺件"
        } else if row.blocked_depth.is_some() {
            "连带阻塞"
        } else {
            "在库"
        };
        let mut line = format!("  [{state}] {} · {}", row.node_id, row.display_zh);
        if let Some(days) = row.missing_days {
            line.push_str(&format!(" · 已欠 {days} 天"));
        }
        sink::say(&line);
        if direction.shows_dependencies() {
            let waiting = describe_set(graph, rules, &row.waiting_on);
            sink::say(&format!("      在等（正向）：{waiting}"));
        }
        if direction.shows_dependents() {
            let blocking = describe_set(graph, rules, &row.blocking_others);
            sink::say(&format!("      卡住（反向）：{blocking}"));
        }
        if let Some(depth) = row.blocked_depth {
            let root_ids: Vec<String> = row.blocked_by.iter().cloned().collect();
            let roots = describe_set(graph, rules, &root_ids);
            sink::say(&format!(
                "      被连带阻塞（第 {depth} 级）· 根因：{roots}"
            ));
        }
    }

    if !view.blocking_edges.is_empty() {
        sink::say("");
        sink::say(&format!(
            "阻塞边（{} 条，缺件直接卡住下游）：",
            view.blocking_edges.len()
        ));
        for (from, to) in &view.blocking_edges {
            sink::say(&format!(
                "  {} → {}",
                describe_one(graph, rules, from),
                describe_one(graph, rules, to)
            ));
        }
    }

    if !view.soft_gaps.is_empty() {
        sink::say("");
        sink::say(&format!(
            "参考缺件（{} 条，可见但不阻断）：",
            view.soft_gaps.len()
        ));
        for (from, to) in &view.soft_gaps {
            sink::say(&format!(
                "  {} → {}",
                describe_one(graph, rules, from),
                describe_one(graph, rules, to)
            ));
        }
    }

    if !view.manual_queue.is_empty() {
        sink::say("");
        sink::say(&format!(
            "需人工确认（{} 件，绑定不唯一 / 成环隔离）：",
            view.manual_queue.len()
        ));
        for id in &view.manual_queue {
            sink::say(&format!("  {}", describe_one(graph, rules, id)));
        }
    }

    if !view.basis_zh.is_empty() {
        sink::say("");
        sink::say("口径说明：");
        for b in &view.basis_zh {
            sink::say(&format!("  · {b}"));
        }
    }

    // 正反向必须真的不同：只在一个方向有边时（链式依赖的端点），
    // 两栏会一模一样，那不是 bug，是图本来就是那样。此处只在**两栏都有内容
    // 且完全相同**时提示——那种情况通常意味着方向参数写反了。
    if direction == DirectionArg::Both {
        let deps: Vec<&String> = view.rows.iter().flat_map(|r| r.waiting_on.iter()).collect();
        let dependents: Vec<&String> =
            view.rows.iter().flat_map(|r| r.blocking_others.iter()).collect();
        if !deps.is_empty() && !dependents.is_empty() && deps == dependents {
            sink::say("");
            sink::say("⚠ 正反向答案完全相同，通常意味着依赖规范写成了双向自指，请核对规则集。");
        }
    }

    // 退出码语义在这里再打一遍：脚本作者常常只看 stdout 不看文档。
    sink::say("");
    sink::say(&format!(
        "退出码 {}：{}",
        if view.is_ready() { EXIT_OK } else { EXIT_FINDINGS },
        crate::contract::describe(if view.is_ready() { EXIT_OK } else { EXIT_FINDINGS })
    ));
}

fn describe_one(
    graph: &DocGraph,
    rules: &psychdoc_rules::EffectiveRules,
    id: &str,
) -> String {
    format!("{id}（{}）", graph.display_of(id, rules))
}

fn describe_set(
    graph: &DocGraph,
    rules: &psychdoc_rules::EffectiveRules,
    ids: &[String],
) -> String {
    if ids.is_empty() {
        return "无".to_string();
    }
    ids.iter()
        .map(|id| describe_one(graph, rules, id))
        .collect::<Vec<_>>()
        .join("、")
}