//! 绑定层的**业务实现**：5 个同步入口的真正逻辑。
//!
//! 这一层刻意不依赖 pyo3：PyO3 只做「Rust 值 → Python 对象」的搬运与
//! 异常码透传，规则集加载、解析、审查、依赖图、写回确认全在这里。
//! 好处是 `cargo test -p psychdoc-ffi`（不开 `extension-module` feature）
//! 就能对这 5 个入口做真实断言——不需要先 `maturin develop` 才能测。
//!
//! **全部同步、无内部线程**：调用方（PySide6 壳）负责把重活丢进
//! `QtConcurrent`，核心侧不自己起线程池，也不持有跨线程状态。

use serde_json::{json, Value};

use psychdoc_fhir::{
    build_transaction, confirm_writeback, create_draft, fetch_token, set_selected, ConfirmContext,
    EmrConfig, Transport, UreqTransport, WeComConfig, WriteEntry, WriteMethod, WritebackContext,
};
use psychdoc_store::WritebackEntry;
use psychdoc_graph::{Direction, DocGraph, InvariantError};
use psychdoc_model::{DocItem, FixAvailability, Severity};
use psychdoc_parse::{discover_dir, parse_package_bytes_with, parse_package_with};
use psychdoc_rules::{load_effective, review, review_incremental, EffectiveRules, ReviewCache};
use psychdoc_store::Store;

/// 绑定层错误：**必须**能翻译成稳定的错误码，UI 不得去匹配中文文案。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApiError {
    #[error("{message}")]
    Coded { code: &'static str, message: String },
}

impl ApiError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self::Coded {
            code,
            message: message.into(),
        }
    }

    pub fn code(&self) -> &'static str {
        match self {
            Self::Coded { code, .. } => code,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            Self::Coded { message, .. } => message,
        }
    }

    /// 从 `Display` 里取出 `E_XXX` 前缀。
    ///
    /// 解析层 / 规则集层的错误码写在 `thiserror` 的 `#[error(...)]` 首段里
    /// （如 `E_RULESET_INVALID: …`），它们没有独立的 `code()` 方法。绑定层
    /// 统一在这里收口：拿不到前缀就落 `E_INTERNAL`，**不**把文案当错误码。
    pub fn from_display(message: String) -> Self {
        let code = message
            .split_once(':')
            .map(|(head, _)| head.trim().to_string())
            .filter(|h| h.starts_with("E_") && !h.contains(' '))
            .unwrap_or_else(|| "E_INTERNAL".to_string());
        let leaked: &'static str = Box::leak(code.into_boxed_str());
        Self::new(leaked, message)
    }
}

type ApiResult<T> = Result<T, ApiError>;

// ---------------------------------------------------------------------------
// 装配：规则集 + 解析
// ---------------------------------------------------------------------------

fn load_rules(yaml: &str) -> ApiResult<EffectiveRules> {
    load_effective(yaml).map_err(|e| ApiError::from_display(e.to_string()))
}

/// 解析输入（一份 Bundle 文件或一个导出目录），返回件 + 被过滤件数。
///
/// 与 CLI `check` 走**同一条**发现链路（`discover_dir` / `parse_package_with`），
/// 不另造一套 GUI 专属解析：GUI 与 CLI 对同一份包给出不同结论是不可接受的。
fn load_input(input: &str, rules: &EffectiveRules) -> ApiResult<(Vec<DocItem>, Vec<psychdoc_model::Diagnostic>, usize, usize)> {
    let registry = rules.registry();
    let path = std::path::Path::new(input);
    if path.is_dir() {
        let packages = discover_dir(path).map_err(|e| ApiError::new("E_IO", e.to_string()))?;
        let mut items = Vec::new();
        let mut issues = Vec::new();
        let mut skipped = 0usize;
        for (_, pkg) in &packages {
            items.extend(pkg.items.iter().cloned());
            issues.extend(pkg.errors.iter().cloned());
            skipped += pkg.filtered.len();
        }
        Ok((items, issues, skipped, packages.len()))
    } else {
        // 走**字节**路径（`parse_package_bytes_with`）而不是 `read_to_string`：
        // 单份文件与目录两条路径必须能读同一批包——护士长拖进来一个 GBK 文件、
        // 批扫同一个目录得出不同结果，是不可接受的。编码问题会作为
        // `E_ENCODING` 诊断进 `errors`，在 GUI 的解析期问题面板里可见。
        let bytes = std::fs::read(path)
            .map_err(|e| ApiError::new("E_IO", format!("读不到文书包 {input} — {e}")))?;
        let parsed = parse_package_bytes_with(&bytes, Some(&registry))
            .map_err(|e| ApiError::from_display(e.to_string()))?;
        let skipped = parsed.filtered.len();
        Ok((parsed.items, parsed.errors, skipped, 1))
    }
}

fn load_input_json(text: &str, rules: &EffectiveRules) -> ApiResult<(Vec<DocItem>, Vec<psychdoc_model::Diagnostic>, usize)> {
    let registry = rules.registry();
    let parsed =
        parse_package_with(text, Some(&registry)).map_err(|e| ApiError::from_display(e.to_string()))?;
    let skipped = parsed.filtered.len();
    Ok((parsed.items, parsed.errors, skipped))
}

// ---------------------------------------------------------------------------
// 序列化：诊断 / 摘要 / 图
// ---------------------------------------------------------------------------

fn diagnostic_json(d: &psychdoc_model::Diagnostic) -> Value {
    json!({
        "code": d.code,
        "doc_id": d.loc.doc_id,
        "section_index": d.loc.section_index,
        "char_start": d.loc.char_range.start,
        "char_end": d.loc.char_range.end,
        "message": d.message.text,
        "basis": d.message.basis,
        "severity": match d.severity { Severity::Blocking => "blocking", Severity::Notice => "notice" },
        // 不可自动修是一等状态：UI 依此**不渲染**补写按钮。
        "fix_availability": match d.fix_availability {
            FixAvailability::Automatic => "automatic",
            FixAvailability::Manual => "manual",
            FixAvailability::None => "none",
        },
        "can_fix": d.fix_availability.allows_fix_ui(),
        "fix_summary": d.fix.as_ref().map(|f| f.summary.clone()),
        "suppressed_by": d.suppressed_by,
        "waived": d.waived,
    })
}

fn review_json(result: &psychdoc_rules::ReviewResult, skipped: usize) -> Value {
    json!({
        "items_reviewed": result.items_reviewed,
        "blocking_count": result.blocking_count(),
        "diagnostics": result.diagnostics.iter().map(diagnostic_json).collect::<Vec<_>>(),
        "parse_issues": result.parse_issues.iter().map(diagnostic_json).collect::<Vec<_>>(),
        "ambiguous_fields": result.ambiguous_fields.len(),
        "skipped_checks": result.skipped_checks.len(),
        // 「容差内未判违规」与「未判定」分列：前者是判出来了结论豁免，
        // 后者是判不了。底栏与逐条清单都得能分开答这两问。
        "tolerated_checks": result.tolerated_checks.len(),
        "waived_count": result.waived_count,
        // 「同处另有更概括的命中」：UI 清单只显示 kept，这里是展开源。
        // 折叠是为了不让同一条整改刷屏，但压下去的那条必须留痕——
        // 否则护士长追问「为什么只报一条」时，工具答不出来。
        "suppressed_count": result.suppressed.len(),
        "suppressed": result
            .suppressed
            .iter()
            .map(|s| {
                json!({
                    "code": s.code,
                    "suppressed_by": s.suppressed_by,
                    "winner_message": s.winner_message,
                    "message": s.message,
                    "basis": s.basis,
                    "doc_id": s.loc.doc_id,
                    "section_index": s.loc.section_index,
                    "char_start": s.loc.char_range.start,
                    "char_end": s.loc.char_range.end,
                    "severity": match s.severity {
                        Severity::Blocking => "blocking",
                        Severity::Notice => "notice",
                    },
                    "family_label": s.family_label,
                    "reason": s.reason,
                    "escalated": s.escalated,
                })
            })
            .collect::<Vec<_>>(),
        "ruleset_version": result.ruleset_version,
        "ruleset_sha256": result.ruleset_sha256,
        "ruleset_short_sha256": psychdoc_rules::sha256_hex(result.ruleset_sha256.as_bytes())
            .chars()
            .take(psychdoc_rules::SHA256_SHORT_LEN)
            .collect::<String>(),
        // 未解析 / 被过滤件数：0 条诊断**不等于**全部合规，底栏必须看得见。
        "unresolved_count": skipped,
        // 每次住院的事件时间线（Task 19）：左栏选中一次住院后，右侧
        // 「这一天按业务时刻排下来是什么样」直接消费这一份，不必前端重排。
        "timelines": result.timelines.iter().map(timeline_json).collect::<Vec<_>>(),
        // 容差内未判违规的逐条明细：底栏给计数，展开给「为什么没报 + 谁的批准」。
        "tolerated": result
            .tolerated_checks
            .iter()
            .map(|t| {
                json!({
                    "code": t.code,
                    "encounter_alias": t.encounter_alias,
                    "primary_doc": t.primary_doc,
                    "counterpart_doc": t.counterpart_doc,
                    "gap_minutes": t.gap_seconds / 60,
                    "tolerance_minutes": t.tolerance_minutes,
                    "reason": t.reason,
                    "source": t.source.as_str(),
                    "detail": t.detail,
                })
            })
            .collect::<Vec<_>>(),
    })
}

/// 一次住院的事件时间线（Task 19）。粗精度时刻带 `coarse: true`，
/// UI 依此**不**把它画成可定罪的时间点，只标成「只写到日/时，需人工核」。
fn timeline_json(tl: &psychdoc_rules::chronology::EventTimeline) -> Value {
    json!({
        "encounter_alias": tl.encounter_alias,
        "events": tl.events.iter().map(|e| json!({
            "doc_id": e.doc_id,
            "template_id": e.template_id,
            "kind": match e.kind {
                psychdoc_model::TimestampKind::AuthoredOn => "authored_on",
                psychdoc_model::TimestampKind::Date => "date",
                psychdoc_model::TimestampKind::BusinessMoment => "business_moment",
            },
            "at": e.at.to_rfc3339(),
            "precision": match e.precision {
                psychdoc_model::Precision::Second => "second",
                psychdoc_model::Precision::Minute => "minute",
                psychdoc_model::Precision::Hour => "hour",
                psychdoc_model::Precision::Day => "day",
            },
            "coarse": e.precision.is_coarser_than(psychdoc_model::Precision::Minute),
            "section_index": e.section_index,
            "char_start": e.char_range.map(|s| s.start),
            "char_end": e.char_range.map(|s| s.end),
        })).collect::<Vec<_>>(),
        // 整件抽不出任何时刻的件：显式带上，不让它们从时间线上凭空消失。
        "undated": tl.undated.clone(),
    })
}

/// 按住院别名聚合，供左栏住院条目列表区消费。
///
/// 顺序按「缺件数降序 + 别名升序」，与「最该先看哪份」一致；
/// 科室名取件上可得的脱敏来源，字典里没有就留空串而不是编一个。
fn encounter_rows(items: &[DocItem], result: &psychdoc_rules::ReviewResult) -> Value {
    let mut rows: std::collections::BTreeMap<String, Value> = std::collections::BTreeMap::new();
    for item in items {
        let entry = rows
            .entry(item.encounter_alias.clone())
            .or_insert_with(|| {
                json!({
                    "encounter_alias": item.encounter_alias,
                    "doc_count": 0,
                    "blocking_count": 0,
                    "doc_ids": Vec::<String>::new(),
                })
            });
        entry["doc_count"] = json!(entry["doc_count"].as_u64().unwrap_or(0) + 1);
        entry["doc_ids"].as_array_mut().unwrap().push(json!(item.doc_id));
    }
    for d in &result.diagnostics {
        let Some(item) = items.iter().find(|i| i.doc_id == d.loc.doc_id) else {
            continue;
        };
        let Some(entry) = rows.get_mut(&item.encounter_alias) else {
            continue;
        };
        if d.severity == Severity::Blocking {
            entry["blocking_count"] = json!(entry["blocking_count"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut list: Vec<Value> = rows.into_values().collect();
    list.sort_by(|a, b| {
        b["blocking_count"]
            .as_u64()
            .unwrap_or(0)
            .cmp(&a["blocking_count"].as_u64().unwrap_or(0))
            .then_with(|| {
                a["encounter_alias"]
                    .as_str()
                    .unwrap_or("")
                    .cmp(b["encounter_alias"].as_str().unwrap_or(""))
            })
    });
    Value::Array(list)
}

// ---------------------------------------------------------------------------
// 入口 7（Task 33）：doc_card —— 一件文书的「可渲染卡片」数据
// ---------------------------------------------------------------------------

/// 卡片入口的入参。`doc_id` 是权威实体键（`Composition.id` 的脱敏别名），
/// **不是**文件名——界面上「点哪条问题就显示哪件」靠的就是它，
/// 按文件名配对是 Task 4 明令禁止的形状。
pub struct DocCardRequest {
    /// 一份 Bundle 文本 / 单份导出文件 / 整个导出目录（与 `graph_both_directions` 同形）。
    pub input: String,
    pub ruleset_yaml: String,
    pub doc_id: String,
}

/// 返回**一件**文书的完整卡片数据：段落序列 + 字段表 + 该件的诊断 + 补写预览。
///
/// 为什么不复用 `check_bundle`：那份入口回的是**整包**的诊断与全部件，
/// 而窗口右栏在任一时刻只显示**一件**。把整包数据搬进界面再筛，
/// 一屏 1000 件的包会把文书卡片挤成一次上万行的字符串拼接。
///
/// 这个入口就是源产品「一条命令拿结构化结果」那条主路径的卡片版：
/// **输入（一件件 id + 定位）→ 主函数 → 带位置的可渲染片段**。
/// 段落顺序、字段顺序、诊断顺序三者都稳定，界面按顺序渲染即可 diff。
pub fn doc_card(req: &DocCardRequest) -> ApiResult<Value> {
    let rules = load_rules(&req.ruleset_yaml)?;
    let registry = rules.registry();
    let (items, issues, skipped) = load_input_any(&req.input, &rules)?;
    let Some(item) = items.iter().find(|i| i.doc_id == req.doc_id) else {
        // 找不到件时报**未知件**，不给空卡片：空卡片会让护士长以为
        // 「这份真的没问题」，而真相是这份根本没被打开。
        return Err(ApiError::new(
            "E_DOC_NOT_FOUND",
            format!(
                "这份导出包里没有件 {}——请确认它是否属于本次载入的目录，或是否已被类型字典过滤",
                req.doc_id
            ),
        ));
    };
    let mut result = review(&items, &rules);
    result.parse_issues.extend(issues);

    let diags: Vec<psychdoc_model::Diagnostic> = result
        .diagnostics
        .iter()
        .filter(|d| d.loc.doc_id == req.doc_id)
        .cloned()
        .collect();
    let parse_issues: Vec<psychdoc_model::Diagnostic> = result
        .parse_issues
        .iter()
        .filter(|d| d.loc.doc_id == req.doc_id)
        .cloned()
        .collect();
    let preview = psychdoc_model::preview_fixback(item, &registry, &result.diagnostics);

    Ok(json!({
        "ok": true,
        "doc": doc_item_json(item),
        // 该件自己的诊断 + 解析期问题。合并两类是因为护士长看到的
        // 是同一张卡片：「这段为什么要标红」对两者是同一个问题。
        "diagnostics": diags.iter().map(diagnostic_json).collect::<Vec<_>>(),
        "parse_issues": parse_issues.iter().map(diagnostic_json).collect::<Vec<_>>(),
        "preview": {
            "blocks": preview.blocks.iter().map(candidate_block_json).collect::<Vec<_>>(),
            // 这些码**只给依据不给按钮**。UI 必须在按钮位置写清原因，
            // 而不是把按钮藏起来让人以为工具漏报了。
            "evidence_only": preview.evidence_only.clone(),
            "summary_line": preview.summary(),
        },
        "binding": match item.binding {
            psychdoc_model::Binding::Confident => "confident",
            psychdoc_model::Binding::Ambiguous => "ambiguous",
            psychdoc_model::Binding::Unresolved => "unresolved",
        },
        "unresolved_count": skipped,
        "ruleset_short_sha256": short_sha(&rules),
    }))
}

/// 候选块 → JSON。与 `preview_fixback` 共用同一份形状，
/// 两处各写一遍会让「界面上看到的 diff」与「预览面板里的 diff」悄悄分叉。
fn candidate_block_json(b: &psychdoc_model::CandidateBlock) -> Value {
    json!({
        "doc_id": b.doc_id,
        "section_index": b.section_index,
        "section_title": b.section_title,
        "original": b.original,
        "skeleton": b.skeleton,
        "code": b.code,
        "basis": b.basis,
        "fix_availability": match b.fix_availability {
            FixAvailability::Automatic => "automatic",
            FixAvailability::Manual => "manual",
            FixAvailability::None => "none",
        },
        "added_lines": b.added_lines,
        "diff": b.diff.iter().map(|l| json!({
            "op": match l.op {
                psychdoc_model::DiffOp::Same => "same",
                psychdoc_model::DiffOp::Added => "added",
                psychdoc_model::DiffOp::Removed => "removed",
            },
            "text": l.text,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// 入口 1：check_bundle —— 一份 Bundle 文本 → 带码带定位的清单
// ---------------------------------------------------------------------------

pub fn check_bundle(bundle_text: &str, ruleset_yaml: &str) -> ApiResult<Value> {
    let rules = load_rules(ruleset_yaml)?;
    let (items, issues, skipped) = load_input_json(bundle_text, &rules)?;
    let mut result = review(&items, &rules);
    result.parse_issues.extend(issues);
    Ok(json!({
        "ok": true,
        "summary": review_json(&result, skipped),
        "encounters": encounter_rows(&items, &result),
        "items": items.iter().map(doc_item_json).collect::<Vec<_>>(),
    }))
}

// ---------------------------------------------------------------------------
// 入口 2：review_batch —— 一个目录 / 文件 → 增量批扫 + 运行摘要
// ---------------------------------------------------------------------------

pub fn review_batch(
    input: &str,
    ruleset_yaml: &str,
    cache_path: Option<&str>,
) -> ApiResult<Value> {
    let rules = load_rules(ruleset_yaml)?;
    let (items, issues, skipped, packages) = load_input(input, &rules)?;
    let mut summary_json = Value::Null;
    let result = match cache_path {
        Some(cp) => {
            let mut cache = ReviewCache::load(std::path::Path::new(cp))
                .map_err(|e| ApiError::new("E_CACHE", e))?;
            let mut run = review_incremental(&items, &rules, &mut cache, skipped);
            run.result.parse_issues.extend(issues.iter().cloned());
            cache.save(std::path::Path::new(cp))
                .map_err(|e| ApiError::new("E_CACHE", e))?;
            let s = &run.summary;
            summary_json = json!({
                "items_total": s.items_total,
                "items_reviewed": s.items_reviewed,
                "items_skipped": s.items_skipped,
                "groups_reviewed": s.groups_reviewed,
                "groups_skipped": s.groups_skipped,
                "ruleset_short_sha256": s.ruleset_short_sha256,
                "unresolved_count": s.unresolved_count,
                "unmatched_type_count": s.unmatched_type_count,
                // 规则集变更导致整表作废时，底栏要能区分「本来就没缓存可省」
                // 与「规则集改了所以全量重审」——后者必须显式提示，
                // 否则护士长看到「重跑 204 件 / 跳过 0 件」会以为工具坏了。
                "invalidated": s.invalidated.as_ref().map(|r| json!({
                    "from_ruleset_short_sha256": r.from_ruleset_short_sha256,
                    "to_ruleset_short_sha256": r.to_ruleset_short_sha256,
                    "dropped_items": r.dropped_items,
                    "dropped_groups": r.dropped_groups,
                    "as_zh": r.as_zh(),
                })),
                "as_zh": s.as_zh(),
            });
            run.result
        }
        None => {
            let mut r = review(&items, &rules);
            r.parse_issues.extend(issues);
            r
        }
    };
    Ok(json!({
        "ok": true,
        "summary": review_json(&result, skipped),
        "run_summary": summary_json,
        "encounters": encounter_rows(&items, &result),
        "packages_found": packages,
    }))
}

fn doc_item_json(item: &DocItem) -> Value {
    json!({
        "doc_id": item.doc_id,
        "template_id": item.template_id,
        "encounter_alias": item.encounter_alias,
        "source_system": item.source_system,
        "sections": item.sections.iter().map(|s| json!({
            "index": s.index,
            "title": s.title,
            "text": s.text,
        })).collect::<Vec<_>>(),
    })
}

// ---------------------------------------------------------------------------
// 入口 3：preview_fixback —— 补写预览双栏 diff
// ---------------------------------------------------------------------------

pub fn preview_fixback(bundle_text: &str, ruleset_yaml: &str) -> ApiResult<Value> {
    let rules = load_rules(ruleset_yaml)?;
    let registry = rules.registry();
    let (items, _issues, _skipped) = load_input_json(bundle_text, &rules)?;
    let result = review(&items, &rules);
    let mut blocks = Vec::new();
    let mut evidence_only: Vec<String> = Vec::new();
    for item in &items {
        let preview = psychdoc_model::preview_fixback(item, &registry, &result.diagnostics);
        for b in &preview.blocks {
            // 复用 `candidate_block_json`：卡片面板与预览面板消费的是
            // 同一种块，两处各写一遍 JSON 早晚会漂成两套形状。
            blocks.push(candidate_block_json(b));
        }
        for code in preview.evidence_only {
            if !evidence_only.contains(&code) {
                evidence_only.push(code);
            }
        }
    }
    let new_sections = blocks
        .iter()
        .filter(|b| b["section_index"].is_null())
        .count();
    Ok(json!({
        "ok": true,
        "blocks": blocks,
        // 这些码**只给依据不给按钮**：UI 必须在按钮位置显式说明原因。
        "evidence_only": evidence_only,
        "summary_line": format!("新建 {new_sections} 段 / 修改 {} 段", blocks.len() - new_sections),
    }))
}

// ---------------------------------------------------------------------------
// 入口 5：graph_both_directions —— 归档核齐视图的正反向数据
// ---------------------------------------------------------------------------

/// 三态输入：Bundle 文本 / 单份文件 / 导出目录。
///
/// 判定顺序是「先看是不是路径」：`review_batch` 收路径、`check_bundle`
/// 收文本，两个入口的入参类型不同是既有契约，不在这里强行统一。
fn load_input_any(
    input: &str,
    rules: &EffectiveRules,
) -> ApiResult<(Vec<DocItem>, Vec<psychdoc_model::Diagnostic>, usize)> {
    let path = std::path::Path::new(input);
    if path.is_dir() || path.is_file() {
        let (items, issues, skipped, _) = load_input(input, rules)?;
        return Ok((items, issues, skipped));
    }
    // 不是路径：按 Bundle 文本解析。首字符不是 `{` 时直接给形态错误，
    // 而不是把一段路径当 JSON 解析后报一句看不懂的 serde 错误。
    if !input.trim_start().starts_with('{') {
        return Err(ApiError::new(
            "E_BUNDLE_SHAPE",
            format!("既不是可读的路径（{input}），也不是 FHIR Bundle JSON 文本"),
        ));
    }
    load_input_json(input, rules)
}

/// `input` 可以是一份 Bundle **文本**，也可以是一个导出目录 / 单份文件的路径。
///
/// 归档核齐视图面对的是「整个导出目录」（护士长一次拖一整个文件夹进来），
/// 而依赖图的边**绝不跨住院别名**（`DocGraph::check_invariants` 硬约束），
/// 因此把目录里各包的件并到一张图上再问某个节点，正反向答案与只看那一次
/// 住院时**完全一致**——不需要、也不应该在 UI 层先做一次按住院过滤。
pub fn graph_both_directions(input: &str, ruleset_yaml: &str, node_id: &str) -> ApiResult<Value> {
    let rules = load_rules(ruleset_yaml)?;
    let (items, _issues, _skipped) = load_input_any(input, &rules)?;
    let graph = DocGraph::build(&items, &rules);
    graph
        .check_invariants()
        .map_err(|e| match e {
            // 成环单独给一个码：UI 要据此把「规则集写错了」和「这份图数据坏了」
            // 区分开，前者交护士长/质控办改规范，后者交信息科查数据。
            InvariantError::Cycle(_) => ApiError::new("E_DEP_CYCLE", e.to_string()),
            other => ApiError::new("E_DEP_INVARIANT", other.to_string()),
        })?;
    let (deps, dependents) = graph.both_directions(node_id);
    let node = graph.nodes.get(node_id);
    // 先把「节点 id → 中文展示名」算成一张表，再交给 `describe` 的闭包：
    // `describe` 要的是 `Fn(&str) -> Option<String>`，而 `display_of` 需要
    // `&EffectiveRules`；预先制表避免在闭包里借用 `graph` 自身。
    let names: std::collections::HashMap<String, String> = graph
        .nodes
        .keys()
        .map(|id| (id.clone(), graph.display_of(id, &rules)))
        .collect();
    let blocking: Vec<Value> = graph
        .blocking_edges()
        .iter()
        .map(|e| {
            json!({
                "from": e.from,
                "to": e.to,
                "requires": e.requires,
                "spec_from": e.spec_from,
                "spec_to": e.spec_to,
                "describe": e.describe(|id| names.get(id).cloned()),
            })
        })
        .collect();
    Ok(json!({
        "ok": true,
        "node_id": node_id,
        "exists": node.is_some(),
        "node": node.map(|n| json!({
            "template_id": n.template_id,
            "encounter_alias": n.encounter_alias,
            "doc_id": n.doc_id,
            "is_missing": n.is_missing(),
            "binding": format!("{:?}", n.binding),
        })),
        "display_name": graph.display_of(node_id, &rules),
        "dependencies": deps.iter().cloned().collect::<Vec<_>>(),
        "dependents": dependents.iter().cloned().collect::<Vec<_>>(),
        "both": graph.query(node_id, Direction::Both).into_iter().collect::<Vec<_>>(),
        "is_blocked": graph.is_blocked(node_id),
        "blocking_edges": blocking,
        "manual_queue": graph.manual_queue.len(),
        "self_loops": graph.self_loops.len(),
        "soft_gaps": graph.soft_gaps.len(),
        // 环：有环时本入口已经报 E_DEP_CYCLE 了（见上），这里恒为 0；
        // 保留这个字段是为了让 UI 侧代码形状稳定（Task 34 渲染环内件灰底角标）。
        "cycles": graph.cycles.len(),
    }))
}

// ---------------------------------------------------------------------------
// 入口 6（Task 32）：scan_queue —— 拖入一个 / 几个导出目录 → 审查队列
// ---------------------------------------------------------------------------

/// `scan_queue` 的入参。
///
/// 三个字段都是「调用方注入」而不是核心自己取：时刻来自调用方是为了
/// 结果可复现（同参数两次跑出同一份快照），库路径是为了让测试落在临时
/// 目录而不是用户数据目录。
pub struct ScanQueueRequest {
    /// 拖进来的根：一个或多个目录 / 单份导出文件。
    pub roots: Vec<String>,
    pub ruleset_yaml: String,
    /// RFC3339 时刻（载入时刻）。核心**不读系统时钟**。
    pub at: String,
    /// 本机 sqlite 库路径；`None` 落用户数据目录。
    pub store_path: Option<String>,
    /// 是否把这次扫描落库（关窗重开后据此还原队列）。`false` = 只看不存，
    /// 用于「试扫一个目录看看能不能认」。
    pub persist: bool,
}

/// 扫描一个 / 几个导出根，产出按住院别名聚合的审查队列。
///
/// 与入口 2（`review_batch`）的分工：
///
/// - `review_batch` 回答「这些包里**哪几件不合规**」——规则判定的主体；
/// - `scan_queue` 回答「我拖进来了什么、**哪几次住院、每次还差几件**、
///   有哪些文件根本没被打开」——**队列与发现**。
///
/// 两者共享同一条发现链路（`discover_paths_report`）与同一份类型字典，
/// 因此「拖一个文件夹」与「逐个点开文件」不会给出不同的跳过口径。
///
/// 依赖图的缺件口径来自 `DocGraph::blocking_missing_ids`（只算 `requires: true`
/// 的真缺件），与归档核齐视图同源——左栏徽标与「归档核齐」标签页
/// 不能对同一次住院报出两个不同的缺件数。
pub fn scan_queue(req: &ScanQueueRequest) -> ApiResult<Value> {
    let rules = load_rules(&req.ruleset_yaml)?;
    let registry = rules.registry();
    let root_paths: Vec<std::path::PathBuf> = req.roots.iter().map(std::path::PathBuf::from).collect();
    if root_paths.is_empty() {
        return Err(ApiError::new(
            "E_QUEUE_EMPTY",
            "没有指定要扫描的路径——请拖入一个病区导出目录，或用「选择文件夹」。",
        ));
    }

    let report = psychdoc_parse::discover_paths_report(&root_paths, Some(&registry))
        .map_err(|e| ApiError::from_display(e.to_string()))?;

    // 件 → 源文件（相对路径）：护士长要能回答「这次住院的件是哪份文件带来的」。
    let mut source_of: std::collections::BTreeMap<String, String> = Default::default();
    let mut items: Vec<DocItem> = Vec::new();
    let mut parse_issues: Vec<psychdoc_model::Diagnostic> = Vec::new();
    let mut filtered_count = 0usize;
    for (path, pkg) in &report.packages {
        let rel = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        filtered_count += pkg.filtered.len();
        parse_issues.extend(pkg.errors.iter().cloned());
        for item in &pkg.items {
            source_of.insert(item.doc_id.clone(), rel.clone());
            items.push(item.clone());
        }
    }

    // 缺件数：走依赖图，与归档核齐视图同源。**成环时图拒绝出缺件口径**
    // （阻塞传播在环上无解），此时整批报 E_DEP_CYCLE，而不是给一个假数字。
    let graph = DocGraph::build(&items, &rules);
    graph.check_invariants().map_err(|e| match e {
        InvariantError::Cycle(_) => ApiError::new("E_DEP_CYCLE", e.to_string()),
        other => ApiError::new("E_DEP_INVARIANT", other.to_string()),
    })?;
    let missing_ids = graph.blocking_missing_ids();

    let queue = psychdoc_parse::aggregate(psychdoc_parse::QueueInput {
        items: items.clone(),
        missing_node_ids: missing_ids,
        source_of,
        file_skips: report.file_skips.clone(),
        filtered_count,
        dirs_visited: report.dirs_visited,
        files_seen: report.files_seen,
        files_opened: report.packages.len(),
    });

    // 落库（可选）。空队列**不落**：落一个空队列会让重开后的左栏空着，
    // 而用户真正需要知道的是「这个文件夹里没有能认的文书包」。
    let mut queue_id: Option<String> = None;
    if req.persist && !queue.entries.is_empty() {
        let store = open_store(req.store_path.as_deref())?;
        let conn = store.conn();
        let sid = psychdoc_store::queue_id(&req.roots.join("\x1f"), &req.at);
        let snap = psychdoc_store::QueueSnapshot {
            session: psychdoc_store::QueueSession {
                queue_id: sid.clone(),
                source_path: req.roots.join("\x1f"),
                loaded_at: req.at.clone(),
                ruleset_short_sha256: short_sha(&rules),
                packages_found: report.packages.len(),
                files_seen: report.files_seen,
                items_total: queue.item_count(),
                missing_total: queue.missing_total(),
                // 存原样 JSON：重开后逐字还原这份「已跳过」清单，
                // 拆列重拼会与当次扫描的实际文案漂移。
                file_skips_json: serde_json::to_string(
                    &report.file_skips.iter().map(file_skip_json).collect::<Vec<_>>(),
                )
                .unwrap_or_else(|_| "[]".into()),
                last_scanned_at: req.at.clone(),
            },
            entries: queue
                .entries
                .iter()
                .enumerate()
                .map(|(i, e)| psychdoc_store::QueueEntryRow {
                    encounter_alias: e.encounter_alias.clone(),
                    ward: e.ward.clone(),
                    doc_count: e.doc_count,
                    missing_count: e.missing_count,
                    reviewed_at: None,
                    position: i,
                })
                .collect(),
        };
        psychdoc_store::save_queue(conn, &snap)
            .map_err(|e| ApiError::from_display(e.to_string()))?;
        queue_id = Some(sid);
    }

    Ok(json!({
        "ok": true,
        "queue_id": queue_id,
        "persisted": req.persist && queue_id.is_some(),
        "roots": req.roots,
        "entries": queue.entries.iter().map(queue_entry_json).collect::<Vec<_>>(),
        "file_skips": report.file_skips.iter().map(file_skip_json).collect::<Vec<_>>(),
        "parse_issues": parse_issues.iter().map(diagnostic_json).collect::<Vec<_>>(),
        "totals": {
            "packages": report.packages.len(),
            "files_seen": report.files_seen,
            "files_opened": report.packages.len(),
            "dirs_visited": report.dirs_visited,
            "items": queue.item_count(),
            "missing": queue.missing_total(),
            "filtered": filtered_count,
            "encounters": queue.entries.len(),
            "skipped_files": report.file_skips.len(),
        },
        // 两条守恒等式让调用方自查有没有漏记账（0 条命中 ≠ 全合规）
        "is_balanced": queue.is_balanced(),
        "filtered_balanced": report.is_balanced(),
        // 一份包都没打开时给一句人话，而不是让界面显示「0 条命中」
        "empty_reason_zh": report.empty_reason_zh(),
        "ruleset_short_sha256": short_sha(&rules),
    }))
}

/// 规则集的 12 位短哈希：底栏与队列落库都用它标版本。
///
/// 取自**规则集源文件的** sha256（`EffectiveRules.sha256`），不是重新
/// 对 YAML 字节算一遍——加载期已经算过，复用它保证「界面上显示的版本」
/// 与「审查结果里记的版本」是同一个值。
fn short_sha(rules: &EffectiveRules) -> String {
    psychdoc_rules::sha256_hex(rules.sha256.as_bytes())
        .chars()
        .take(psychdoc_rules::SHA256_SHORT_LEN)
        .collect()
}

/// 打开库：给了路径就用它，否则落用户数据目录。凭据与迁移链都在 `Store::open` 里。
fn open_store(path: Option<&str>) -> ApiResult<Store> {
    match path {
        Some(p) => Store::open(p).map_err(|e| ApiError::from_display(e.to_string())),
        None => Store::open_default().map_err(|e| ApiError::from_display(e.to_string())),
    }
}

fn queue_entry_json(e: &psychdoc_parse::QueueEntry) -> Value {
    json!({
        "encounter_alias": e.encounter_alias,
        "ward": e.ward,
        "doc_count": e.doc_count,
        "missing_count": e.missing_count,
        "doc_ids": e.doc_ids,
        "source_files": e.source_files,
        "badge_zh": e.badge_zh(),
        "is_complete": e.is_complete(),
    })
}

fn file_skip_json(s: &psychdoc_parse::FileSkip) -> Value {
    json!({
        "path": s.path,
        "name": s.name,
        "relative_path": s.relative_path,
        "reason": format!("{:?}", s.reason),
        "reason_zh": s.reason.as_zh(),
        "retryable": s.reason.retryable(),
        "describe_zh": s.as_zh(),
    })
}

// ---------------------------------------------------------------------------
// 入口 4：commit_writeback —— 草稿 → 勾选 → 整批 transaction → 状态推进
// ---------------------------------------------------------------------------

/// 一次写回确认的入参。字段名与 spec.md §4「确认回写」一致。
pub struct CommitRequest {
    /// 本机 sqlite 库路径。**不传**时落用户数据目录（`Store::open_default`）。
    pub store_path: Option<String>,
    pub draft_id: String,
    pub encounter_alias: String,
    /// 推进人（脱敏操作人标识）
    pub actor: String,
    /// RFC3339 时刻，由调用方注入以保证可复现
    pub at: String,
    /// 待勾选的件：`(doc_id, 写回资源 JSON)`
    pub entries: Vec<(String, Value)>,
    /// 每件的内容哈希，写进幂等键
    pub content_hashes: Vec<(String, String)>,
    /// 当时生效的规则集 sha256，进 Provenance 与写回审计事件。
    ///
    /// 审计链要能回答「这次建件是哪一版规则下的判断」，而这个值只有
    /// 调用方才知道（界面上就在上一屏的底栏）。留空会让 Provenance 里
    /// 的版本字段空着，质控办事后无从判断这份件是按哪版口径建的。
    pub ruleset_sha256: String,
    /// 产出这次勾选的审查批次号，写进 `writeback_log.run_id`。
    ///
    /// 有了它，一条写回才能与「是哪一次审查发现了这个问题」对起来；
    /// 否则审计导出里写回行与诊断行只能靠住院别名与时刻猜。
    pub run_id: Option<String>,
}

/// 走**真实**出站链路：凭据从环境变量读 → 取 token → POST transaction。
///
/// 凭据缺失时在 `EmrConfig::from_env()` 就返回 `E_CFG_MISSING`，
/// 物理上不可能先发出网络请求——这正是 Task 11 已验证的行为，
/// 绑定层原样透传，不加任何「测试模式」后门。
pub fn commit_writeback(req: &CommitRequest) -> ApiResult<Value> {
    let config = EmrConfig::from_env().map_err(|e| ApiError::new(e.code(), e.to_string()))?;
    let store = match &req.store_path {
        Some(p) => Store::open(p).map_err(|e| ApiError::new(e.code(), e.to_string()))?,
        None => Store::open_default().map_err(|e| ApiError::new(e.code(), e.to_string()))?,
    };
    // 10 秒超时：院内 FHIR 端点在同一园区内，超过 10 秒基本等于不可达，
    // 早失败比让界面空转 30 秒更好。
    let transport = UreqTransport::new(std::time::Duration::from_secs(10));
    let token = fetch_token(&config, &transport).map_err(|e| ApiError::new(e.code(), e.to_string()))?;

    let ctx = ConfirmContext {
        encounter_alias: req.encounter_alias.clone(),
        actor: req.actor.clone(),
        at: req.at.clone(),
        ruleset_sha256: req.ruleset_sha256.clone(),
        run_id: req.run_id.clone(),
    };
    // 待写回的件必须**先**在库里落在合法起点上，confirm 的第 6 步才推得动。
    //
    // 这一步放在 `create_draft` 之前、**且在任何网络调用之前**：反过来的话，
    // confirm 会先把 transaction 发出去、把 `writeback_log` 落库，然后才在
    // 状态推进处撞上 `E_STATE_UNKNOWN` 返回错误——护士长看到的是「写回失败」，
    // 而服务端那份件其实已经建好了，本地写回日志也已经记上了。
    // 那是最坏的一种状态：界面说没成，事实已经成了。
    ensure_draftable(&store, req)?;
    // 草稿 → 勾选：selection 只经由草稿，调用方自报的勾选一律不认。
    let writeback_entries: Vec<WritebackEntry<'_>> = req
        .content_hashes
        .iter()
        .map(|(doc_id, sha)| WritebackEntry {
            doc_id: doc_id.as_str(),
            content_sha256: sha.as_str(),
            run_id: req.run_id.as_deref(),
        })
        .collect();
    create_draft(&store, &req.draft_id, &ctx, &writeback_entries)
        .map_err(|e| ApiError::new("E_DRAFT_STATE", e.to_string()))?;
    for (doc_id, _) in &req.entries {
        set_selected(&store, &req.draft_id, doc_id, true, &req.at)
            .map_err(|e| ApiError::new("E_DRAFT_STATE", e.to_string()))?;
    }

    let by_id: std::collections::HashMap<&str, &Value> = req
        .entries
        .iter()
        .map(|(id, v)| (id.as_str(), v))
        .collect();
    let outcome = confirm_writeback(
        &store,
        &config,
        &token,
        &req.draft_id,
        &ctx,
        &|doc_id: &str| {
            by_id.get(doc_id).map(|resource| WriteEntry {
                resource_type: "Composition".to_string(),
                logical_id: doc_id.to_string(),
                method: WriteMethod::Post,
                resource: (*resource).clone(),
            })
        },
        &transport as &dyn Transport,
    )
    .map_err(|e| ApiError::new(e.code(), e.to_string()))?;

    Ok(json!({
        "ok": true,
        "created": outcome.created,
        "provenance": outcome.provenance,
        "new_rows": outcome.new_rows(),
        // 幂等命中：本次 0 条新写回，UI 应显示「此前已由 X 于 T 提交」。
        "replayed": outcome.is_replay(),
        "advanced": outcome.advanced.iter().map(|(id, s)| json!({
            "doc_id": id,
            "state": s.as_zh(),
        })).collect::<Vec<_>>(),
    }))
}

/// 把待写回的件放到状态机允许的**草稿**起点上，且只做这件事。
///
/// `confirm` 的第 6 步只认「当前态是草稿」这一种推进，件没登记或状态不在
/// 草稿上，它就报 `E_STATE_UNKNOWN` —— 而那一步在 transaction 外发**之后**。
/// 因此登记必须在这里、在任何网络调用之前做完。
///
/// 三种情形：
///
/// - **未登记**（建件场景，护士长首次补一份缺件）：登记件 + `缺失` → `草稿`。
///   建件本来就该从「缺失」起步，直接落「已建件」等于凭空跳过起草那一态。
/// - **已登记且在草稿**：什么都不做。
/// - **已登记但在别的态**：**不动**。已建件 / 已收 / 可归档的件不该被一次
///   写回偷偷改状态，那属于越权改业务事实，宁可让 confirm 如实报错。
fn ensure_draftable(store: &Store, req: &CommitRequest) -> ApiResult<()> {
    for (doc_id, sha) in &req.content_hashes {
        if store.state_of(doc_id).map_err(|e| ApiError::new(e.code(), e.to_string()))?.is_none() {
            // 建件：先按「缺失」登记，再沿唯一合法边 缺失 → 草稿。
            // template_id 从待写资源里取权威字典码；取不到就留空串而不是
            // 编一个——件类型编错会让后续按类型聚合的视图说错话。
            let template_id = req
                .entries
                .iter()
                .find(|(id, _)| id == doc_id)
                .and_then(|(_, r)| {
                    r["type"]["coding"]
                        .as_array()
                        .and_then(|cs| cs.first())
                        .and_then(|c| c["code"].as_str())
                })
                .unwrap_or("")
                .to_string();
            let item = DocItem::new(
                doc_id.clone(),
                template_id,
                req.encounter_alias.clone(),
            )
            .with_source_system("")
            .with_binding(psychdoc_model::Binding::Confident);
            store
                .register_items(std::slice::from_ref(&item), &req.at)
                .map_err(|e| ApiError::new(e.code(), e.to_string()))?;
            store
                .init_state(
                    doc_id,
                    psychdoc_model::state::DocState::Missing,
                    &req.actor,
                    &req.at,
                )
                .map_err(|e| ApiError::new(e.code(), e.to_string()))?;
        }
        let current = store
            .state_of(doc_id)
            .map_err(|e| ApiError::new(e.code(), e.to_string()))?;
        if current == Some(psychdoc_model::state::DocState::Missing) {
            store
                .advance_state(
                    doc_id,
                    psychdoc_model::state::DocState::Draft,
                    &req.actor,
                    &req.at,
                )
                .map_err(|e| ApiError::new(e.code(), e.to_string()))?;
        }
        let _ = sha;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 入口 9（Task 34）：archive_view —— 归档核齐标签页的一屏数据
// ---------------------------------------------------------------------------

/// `archive_view` 的入参。
///
/// `as_of` 由调用方注入而不是核心读系统时钟：同一份导出目录在两次审查之间
/// 可能被补进件，欠件天数必须能由「按哪一刻算」复现，否则审计导出 diff 不动。
pub struct ArchiveViewRequest {
    /// 导出目录 / 单份文件 / Bundle 文本（与 `graph_both_directions` 同形）。
    pub input: String,
    pub ruleset_yaml: String,
    /// 住院脱敏别名。一次只看一次住院——依赖图绝不跨住院别名。
    pub encounter_alias: String,
    /// RFC3339 时刻，本视图以它为「今天」算欠件天数。
    pub as_of: String,
    /// 本机 sqlite 库路径。`None` 落用户数据目录。
    ///
    /// 缺件登记簿从库里读；库读不到时**不**按今天兜底，欠件天数给
    /// `None` + 口径说明（与 `MissingAge` 的既有口径一致）。
    pub store_path: Option<String>,
    /// 业务日（`YYYY-MM-DD`），通知幂等键的第三个分量。
    pub business_day: String,
}

/// 一件文书的**节点态**：界面据此上色，四态互斥且穷尽。
///
/// 为什么不在 Python 侧判：同一个「橙」既可能来自「被连带阻塞」也可能
/// 来自「软缺件」，两者的业务含义完全不同。让 Rust 侧给一个稳定枚举，
/// 界面只做颜色映射，色与义就不会漂。
fn node_state(
    node: Option<&psychdoc_graph::GraphNode>,
    row: &psychdoc_graph::ArchiveRow,
    graph: &DocGraph,
) -> &'static str {
    // 顺序即优先级：缺件最重，其次人工待确认，再次连带阻塞，最后达标。
    if row.blocked_depth.is_some() {
        return "blocked";
    }
    if graph.is_blocking_missing(&row.node_id) {
        return "missing";
    }
    if node.is_some_and(|n| !n.binding.is_confident()) {
        return "ambiguous";
    }
    "ok"
}

/// 产出**一屏**归档核齐视图数据：节点四态、边（阻塞边加粗标记）、
/// 欠件天数、正反向两个方向的问题清单、通知状态与口径说明。
///
/// 与 `graph_both_directions` 的分工：那个入口回答「某一个节点的两个方向」，
/// 本入口回答「一整张图长什么样、哪些件欠了几天、提醒发没发」——
/// 界面切到「归档核齐」标签时需要的是后者。
pub fn archive_view(req: &ArchiveViewRequest) -> ApiResult<Value> {
    let rules = load_rules(&req.ruleset_yaml)?;
    let (items, _issues, _skipped) = load_input_any(&req.input, &rules)?;
    let now = chrono::DateTime::parse_from_rfc3339(&req.as_of)
        .map_err(|e| ApiError::new("E_ARG", format!("核算时刻 {} 不是 RFC3339：{e}", req.as_of)))?;

    let graph = DocGraph::build(&items, &rules);
    // 不变量先跑：成环 / 悬空 / 跨住院一律拒出图，绝不给「看起来齐全」的视图。
    graph.check_invariants().map_err(|e| match e {
        InvariantError::Cycle(_) => ApiError::new("E_DEP_CYCLE", e.to_string()),
        other => ApiError::new("E_DEP_INVARIANT", other.to_string()),
    })?;

    // 住院别名必须真的在这份输入里。返回一张空图会被读成「这次住院已核齐」，
    // 而真相是「你点的是一条不存在的住院」——两种读法差着一次误归档。
    if !graph
        .nodes
        .values()
        .any(|n| n.encounter_alias == req.encounter_alias)
    {
        return Err(ApiError::new(
            "E_ENCOUNTER_UNKNOWN",
            format!(
                "这份文书包里没有住院别名 {} 的任何一件，无法出归档核齐视图",
                req.encounter_alias
            ),
        ));
    }

    let view = graph
        .archive_view(&req.encounter_alias, now, &ledger_from_store(req.store_path.as_deref())?, &rules)
        .map_err(|e| ApiError::from_display(e.to_string()))?;

    let store = open_store_opt(req.store_path.as_deref())?;

    // 边：整张图按住院过滤后交给界面画。`blocking` 是界面加粗的唯一依据。
    let in_scope = |id: &str| {
        graph
            .nodes
            .get(id)
            .is_some_and(|n| n.encounter_alias == req.encounter_alias)
    };
    let names: std::collections::HashMap<String, String> = graph
        .nodes
        .keys()
        .map(|id| (id.clone(), graph.display_of(id, &rules)))
        .collect();
    let edges: Vec<Value> = graph
        .edges
        .iter()
        .chain(graph.soft_gaps.iter())
        .filter(|e| in_scope(&e.from) && in_scope(&e.to))
        .map(|e| {
            json!({
                "from": e.from,
                "to": e.to,
                "requires": e.requires,
                "blocking": e.blocking,
                "describe": e.describe(|id| names.get(id).cloned()),
            })
        })
        .collect();

    // 节点：四态 + 两个方向的问题清单（前端只画，不自己判）。
    //
    // 先把每行的通知状态查成 `(node_id, Option<OutboxRow>)` 的表，再逐行映射：
    // 查库要返回 `Result`，而 `json!` 闭包里用不了 `?`。
    let notifications: std::collections::BTreeMap<String, Option<Value>> = view
        .rows
        .iter()
        .map(|r| {
            let notify = store
                .notification(&psychdoc_store::notify::notify_key(
                    &req.encounter_alias,
                    &r.node_id,
                    &req.business_day,
                ))
                .map_err(|e| ApiError::from_display(e.to_string()))?;
            let v = notify.as_ref().map(|n| {
                json!({
                    "state": n.state,
                    "sent_at": n.sent_at,
                    "attempts": n.attempts,
                    "next_attempt_at": n.next_attempt_at,
                    "last_error": n.last_error,
                    "as_zh": n.as_zh(),
                })
            });
            Ok((r.node_id.clone(), v))
        })
        .collect::<ApiResult<_>>()?;

    let rows: Vec<Value> = view
        .rows
        .iter()
        .map(|r| {
            let node = graph.nodes.get(&r.node_id);
            let state = node_state(node, r, &graph);
            let notify = notifications.get(&r.node_id).cloned().unwrap_or(None);
            json!({
                "node_id": r.node_id,
                "template_id": r.template_id,
                "display_zh": r.display_zh,
                "state": state,
                "present": r.present,
                "binding": node.map(|n| format!("{:?}", n.binding)),
                "directly_blocked": r.directly_blocked,
                "blocked_depth": r.blocked_depth,
                "blocked_by": r.blocked_by,
                "waiting_on": r.waiting_on,
                "blocking_others": r.blocking_others,
                "missing_days": r.missing_days,
                "notification": notify,
            })
        })
        .collect();

    let age = view.encounter_age.as_ref();
    Ok(json!({
        "ok": true,
        "encounter_alias": view.encounter_alias,
        "as_of": view.as_of.to_rfc3339(),
        "is_ready": view.is_ready(),
        "rows": rows,
        "edges": edges,
        // 阻塞边单列一栏：界面加粗只认这一份，`edges` 里那条
        // `blocking: true` 是同一条边的渲染依据，两处对不上就会画出
        // 一根凭空多出来的红线。
        "blocking_edges": view.blocking_edges,
        "missing": view.missing.iter().map(|m| json!({
            "node_id": m.node_id,
            "template_id": m.template_id,
            "days": m.days,
            "first_seen": m.first_seen.to_rfc3339(),
            "basis_zh": m.basis_zh,
        })).collect::<Vec<_>>(),
        "encounter_age": age.map(|a| json!({
            "days": a.days,
            "oldest_missing_node": a.oldest_missing_node,
            "missing_count": a.missing_count,
            "basis_zh": a.basis_zh,
        })),
        "soft_gaps": view.soft_gaps,
        "manual_queue": view.manual_queue,
        "basis_zh": view.basis_zh,
        "ruleset_short_sha256": short_sha(&rules),
    }))
}

/// 缺件登记簿：从本机库里读**首次发现缺失的时刻**。
///
/// 读不到库 / 库里没有登记 → 空登记簿 → 欠件天数给 `None` + 口径说明。
/// **绝不**按 `as_of` 兜底：那样会把「刚发现缺 3 件」显示成「已欠 0 天」。
fn ledger_from_store(path: Option<&str>) -> ApiResult<psychdoc_graph::MissingLedger> {
    let mut ledger = psychdoc_graph::MissingLedger::new();
    let Ok(store) = open_store_opt(path) else {
        return Ok(ledger);
    };
    let Ok(mut stmt) = store.conn().prepare(
        "SELECT doc_id, missing_since FROM doc_states WHERE state = 'missing' AND missing_since IS NOT NULL",
    ) else {
        return Ok(ledger);
    };
    let collected = stmt
        .query_map([], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?))
        })
        .map_err(|e| ApiError::from_display(e.to_string()))?;
    for row in collected {
        // 单行读坏只丢这一行，不让整张视图算不出来——其余件的欠件天数
        // 仍然有值，比整张报错对护士长更有用。
        let Ok((doc_id, at)) = row else { continue };
        if let Some(at) = at.and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok()) {
            ledger.record(doc_id, at);
        }
    }
    Ok(ledger)
}

/// 打开库；给不出可用的库时返回 `Err` 而不是造一个空壳。
///
/// `ledger_from_store` 用的是「打不开就算了」的宽松版（欠件天数退化成
/// `None`），而提交写回这一侧必须严格：打不开库就不能记「已发」。
fn open_store_opt(path: Option<&str>) -> ApiResult<Store> {
    match path {
        Some(p) => Store::open(p).map_err(|e| ApiError::from_display(e.to_string())),
        None => Store::open_default().map_err(|e| ApiError::from_display(e.to_string())),
    }
}

/// 「一键发缺件待办」的入参。
pub struct NotifyRequest {
    pub store_path: Option<String>,
    pub encounter_alias: String,
    /// 病区名（进消息体抬头）。
    pub ward: String,
    /// 业务日 `YYYY-MM-DD`；跨零点按交班日算。
    pub business_day: String,
    /// RFC3339 时刻，由调用方注入以保证可复现。
    pub at: String,
    /// 缺件行：`(node_id, 院内显示名)`。
    pub missing: Vec<(String, String)>,
    /// 收件群名（显示用）。
    pub group_hint: String,
    /// 干跑：只构造消息体，**零次网络调用**。
    pub dry_run: bool,
}

/// 走**真实**出站链路的一键发缺件待办。
///
/// 凭据纪律与写回一致：`WECOM_WEBHOOK_URL` 只从环境变量读，缺失时
/// `E_CFG_MISSING` 且**零次网络调用**——「没配通知」不该把归档核齐视图
/// 拖下水，但也绝不假装「已发」。
pub fn notify_missing(req: &NotifyRequest) -> ApiResult<Value> {
    if req.missing.is_empty() {
        return Err(ApiError::new(
            "E_NOTHING_SELECTED",
            "本次没有缺件，无可发送的待办",
        ));
    }
    let config = WeComConfig::from_env()
        .map_err(|e| ApiError::new(e.code(), e.to_string()))?;

    let content = psychdoc_fhir::wecom::NoticeContent {
        encounter_alias: req.encounter_alias.clone(),
        ward: req.ward.clone(),
        business_day: req.business_day.clone(),
        lines: req
            .missing
            .iter()
            .map(|(_node_id, label)| psychdoc_fhir::wecom::MissingLine {
                doc_label: label.clone(),
                code: None,
                // 依据行写的是**合成节点 id**。护士长看不懂
                // `<WARD-B-001>::missing::restraint_consent`，群里也没人
                // 会去查那条 id——这一行在这里没有任何信息量，不如不写。
                basis: None,
            })
            .collect(),
        group_hint: req.group_hint.clone(),
    };

    // 干跑分支：消息体在这里就定稿，与真实路径**同一份代码**构造，
    // 因此「干跑看到的」与「真发出去的」不可能是两张脸。
    if req.dry_run {
        let (markdown, hidden) = psychdoc_fhir::wecom::build_markdown(&content, config.max_lines);
        return Ok(json!({
            "ok": true,
            "dry_run": true,
            "channel": "wecom_markdown",
            "hidden_lines": hidden,
            "markdown": markdown,
            "payload": psychdoc_fhir::wecom::build_payload(&markdown),
            "sent": 0,
        }));
    }

    let store = open_store_opt(req.store_path.as_deref())?;
    let transport = UreqTransport::new(std::time::Duration::from_secs(10));
    let policy = psychdoc_fhir::BackoffPolicy::default();
    let mut outcomes: Vec<Value> = Vec::new();
    let mut sent = 0usize;
    for (node_id, _label) in &req.missing {
        let outcome =
            psychdoc_fhir::notify_once(&store, &config, &transport, &content, node_id, policy, &req.at)
                .map_err(|e| ApiError::from_display(e.to_string()))?;
        if outcome.is_sent() {
            sent += 1;
        }
        outcomes.push(json!({
            "node_id": node_id,
            "state": notify_state_zh(&outcome),
            "as_zh": outcome.as_zh(),
            "is_terminal": outcome.is_terminal(),
        }));
    }
    Ok(json!({
        "ok": true,
        "dry_run": false,
        "business_day": req.business_day,
        "sent": sent,
        "total": req.missing.len(),
        "outcomes": outcomes,
    }))
}

/// `NotifyOutcome` 的稳定枚举值。界面按它上色，**不**匹配中文文案。
fn notify_state_zh(o: &psychdoc_fhir::NotifyOutcome) -> &'static str {
    use psychdoc_fhir::NotifyOutcome::*;
    match o {
        Sent => "sent",
        AlreadySent => "already_sent",
        Queued { .. } => "queued",
        NeedsManual { .. } => "needs_manual",
    }
}

/// 只构造 transaction、不发送（干跑）。**零次网络调用**，
/// 供「点提交前先看一眼要发什么」与无 GUI 回归通道使用。
pub fn build_writeback_preview(req: &CommitRequest, ruleset_sha256: &str) -> ApiResult<Value> {
    let keys: Vec<String> = req
        .content_hashes
        .iter()
        .map(|(doc_id, sha)| {
            psychdoc_store::idempotency_key(&req.encounter_alias, doc_id, sha)
        })
        .collect();
    let entries: Vec<WriteEntry> = req
        .entries
        .iter()
        .map(|(id, resource)| WriteEntry {
            resource_type: "Composition".to_string(),
            logical_id: id.clone(),
            method: WriteMethod::Post,
            resource: resource.clone(),
        })
        .collect();
    if entries.is_empty() {
        return Err(ApiError::new("E_NOTHING_SELECTED", "勾选为空，未构造 transaction"));
    }
    let ctx = WritebackContext {
        encounter_alias: req.encounter_alias.clone(),
        actor: req.actor.clone(),
        recorded_at: req.at.clone(),
        ruleset_sha256: ruleset_sha256.to_string(),
        idempotency_keys: keys.clone(),
    };
    let bundle = build_transaction(&entries, &ctx);
    Ok(json!({
        "ok": true,
        "entry_count": entries.len(),
        "idempotency_keys": keys,
        "bundle": bundle,
    }))
}

/// 启动自检的**完整报告**（Task 38）。
///
/// 与旧的 `startup_check` 的分工：
///
/// - [`startup_check`] 是**严格**的：缺一项就抛 `E_CFG_MISSING`，
///   供「一定要写回」的调用点用；
/// - 本函数**不抛**，把五项凭据逐项摆出来（就绪 / 缺失 / 形态错），
///   供桌面壳决定「弹窗提示但不拦启动」。
///
/// 两者都**零次网络调用**。凭据本体不进返回值也不进日志——
/// 这份报告会显示在窗口上，也可能被人截图发到工作群里。
pub fn startup_report() -> ApiResult<Value> {
    let report = psychdoc_fhir::StartupReport::check(|k| std::env::var(k).ok());
    let strict = psychdoc_fhir::StartupConfig::from_env();

    let (ruleset, log_level, data_dir, code, message) = match &strict {
        Ok(cfg) => (
            Value::from(cfg.ruleset.display().to_string()),
            Value::from(cfg.log_level.as_zh()),
            match &cfg.data_dir {
                Some(d) => Value::from(d.display().to_string()),
                None => Value::Null,
            },
            Value::Null,
            Value::Null,
        ),
        Err(e) => (
            Value::Null,
            Value::Null,
            Value::Null,
            Value::from(e.code()),
            Value::from(e.to_string()),
        ),
    };

    Ok(json!({
        "writeback_ready": report.writeback_ready,
        "ready": report.ready,
        "missing": report.missing,
        "invalid": report.invalid.iter()
            .map(|(k, why)| json!({"key": k, "detail": why}))
            .collect::<Vec<_>>(),
        "notices": report.notices,
        "render": report.render(),
        "ruleset": ruleset,
        "log_level": log_level,
        "data_dir": data_dir,
        "code": code,
        "message": message,
        "network_calls": 0,
    }))
}
