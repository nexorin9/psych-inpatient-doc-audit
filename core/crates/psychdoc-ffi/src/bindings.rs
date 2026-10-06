//! PyO3 绑定：**只做搬运与错误码透传**，业务逻辑全在 [`crate::api`]。
//!
//! 5 个同步入口，形状与源产品「一条命令拿结果」的调用链同形：
//! 输入（Bundle 文本 / 路径 / 规则集 YAML）→ 主函数 → 结构化结果。
//! 核心侧**无内部线程、无内部 tokio runtime**；重活由 PySide6 侧
//! `QtConcurrent` 承担，结果回主线程后再调这些同步函数刷新界面。

use pyo3::create_exception;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use serde_json::{json, Value};

use crate::api::{self, CommitRequest};
use crate::jsonval;

create_exception!(
    psychdoc_ffi,
    PsychdocError,
    pyo3::exceptions::PyException,
    "精神科住院文书审查核心错误（带 .code 稳定错误码）"
);

/// 把 `ApiResult` 的成功值转成 Python dict、失败值转成带 `code` 的异常。
fn finish(py: Python<'_>, result: Result<Value, api::ApiError>) -> PyResult<PyObject> {
    match result {
        Ok(v) => jsonval::to_py(py, &v),
        Err(e) => Err(jsonval::raise(py, e.code(), e.message())),
    }
}

fn ruleset_arg(ruleset_yaml: &str) -> ApiResult2 {
    if ruleset_yaml.trim().is_empty() {
        return Err(api::ApiError::new(
            "E_RULESET_INVALID",
            "规则集 YAML 为空：加载失败时不降级为「无规则 = 全部通过」",
        ));
    }
    Ok(())
}

type ApiResult2 = Result<(), api::ApiError>;

/// 入口 1：一份 Bundle 文本 → 带码、带段落定位、带依据的清单。
#[pyfunction]
#[pyo3(signature = (bundle_text, ruleset_yaml))]
fn check_bundle(py: Python<'_>, bundle_text: &str, ruleset_yaml: &str) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    finish(py, api::check_bundle(bundle_text, ruleset_yaml))
}

/// 入口 2：一个导出目录或文件 → 增量批扫 + 运行摘要（重跑 / 跳过 / 规则集短哈希）。
#[pyfunction]
#[pyo3(signature = (input, ruleset_yaml, cache_path = None))]
fn review_batch(
    py: Python<'_>,
    input: &str,
    ruleset_yaml: &str,
    cache_path: Option<&str>,
) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    finish(py, api::review_batch(input, ruleset_yaml, cache_path))
}

/// 入口 3：补写预览 —— 双栏 diff + 只给依据不给按钮的码集合。
#[pyfunction]
#[pyo3(signature = (bundle_text, ruleset_yaml))]
fn preview_fixback(py: Python<'_>, bundle_text: &str, ruleset_yaml: &str) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    finish(py, api::preview_fixback(bundle_text, ruleset_yaml))
}

/// 入口 4：草稿 → 逐条勾选 → 整批 Bundle transaction → 状态推进。
///
/// 凭据从环境变量读；缺失时抛 `E_CFG_MISSING` 且**零次网络调用**。
#[pyfunction]
#[pyo3(signature = (store_path, draft_id, encounter_alias, actor, at, entries, content_hashes, ruleset_sha256 = "", run_id = None))]
fn commit_writeback(
    py: Python<'_>,
    store_path: Option<&str>,
    draft_id: &str,
    encounter_alias: &str,
    actor: &str,
    at: &str,
    entries: Vec<(String, String)>,
    content_hashes: Vec<(String, String)>,
    ruleset_sha256: &str,
    run_id: Option<&str>,
) -> PyResult<PyObject> {
    let parsed = parse_entries(entries).map_err(|msg| PyValueError::new_err(msg))?;
    let req = CommitRequest {
        store_path: store_path.map(str::to_string),
        draft_id: draft_id.to_string(),
        encounter_alias: encounter_alias.to_string(),
        actor: actor.to_string(),
        at: at.to_string(),
        entries: parsed,
        content_hashes,
        ruleset_sha256: ruleset_sha256.to_string(),
        run_id: run_id.map(str::to_string),
    };
    finish(py, api::commit_writeback(&req))
}

/// 干跑：只构造 transaction 并返回，**不发任何网络请求**。
#[pyfunction]
#[pyo3(signature = (draft_id, encounter_alias, actor, at, entries, content_hashes, ruleset_sha256 = ""))]
fn writeback_dryrun(
    py: Python<'_>,
    draft_id: &str,
    encounter_alias: &str,
    actor: &str,
    at: &str,
    entries: Vec<(String, String)>,
    content_hashes: Vec<(String, String)>,
    ruleset_sha256: &str,
) -> PyResult<PyObject> {
    let parsed = parse_entries(entries).map_err(|msg| PyValueError::new_err(msg))?;
    let req = CommitRequest {
        store_path: None,
        draft_id: draft_id.to_string(),
        encounter_alias: encounter_alias.to_string(),
        actor: actor.to_string(),
        at: at.to_string(),
        entries: parsed,
        content_hashes,
        // 干跑与真发共用同一份 `CommitRequest`，版本字段一并带过来——
        // 两处各写一遍构造、只在一处填版本，迟早漂成「干跑看到的和真发的不一样」。
        ruleset_sha256: ruleset_sha256.to_string(),
        run_id: None,
    };
    finish(py, api::build_writeback_preview(&req, ruleset_sha256))
}

/// 入口 5：归档核齐视图 —— 同一张图的正反向两个答案一次拿全。
///
/// `input` 收路径（导出目录或单份文件），**也**收 Bundle JSON 文本：
/// 归档视图面对的是整个导出目录，收文本会逼调用方先把目录拼成一大坨字符串。
#[pyfunction]
#[pyo3(signature = (input, ruleset_yaml, node_id))]
fn graph_both_directions(
    py: Python<'_>,
    input: &str,
    ruleset_yaml: &str,
    node_id: &str,
) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    finish(py, api::graph_both_directions(input, ruleset_yaml, node_id))
}

/// 入口 6（Task 32）：拖入一个 / 几个导出目录 → 按住院别名聚合的审查队列。
///
/// 与入口 2 的分工：`review_batch` 判「哪几件不合规」，本入口回答
/// 「我拖进来了什么、哪几次住院、每次还差几件、哪些文件根本没被打开」。
#[pyfunction]
#[pyo3(signature = (roots, ruleset_yaml, at, store_path = None, persist = true))]
fn scan_queue(
    py: Python<'_>,
    roots: Vec<String>,
    ruleset_yaml: &str,
    at: &str,
    store_path: Option<&str>,
    persist: bool,
) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    let req = api::ScanQueueRequest {
        roots,
        ruleset_yaml: ruleset_yaml.to_string(),
        at: at.to_string(),
        store_path: store_path.map(str::to_string),
        persist,
    };
    finish(py, api::scan_queue(&req))
}

/// 入口 7（Task 33）：文书卡片 —— 一件件 id → 可渲染的段落 / 诊断 / 预览。
///
/// `input` 与 `graph_both_directions` 同形：收目录 / 单份文件 / Bundle 文本。
/// 找不到该 `doc_id` 时报 `E_DOC_NOT_FOUND`，**不返回空卡片**。
#[pyfunction]
#[pyo3(signature = (input, ruleset_yaml, doc_id))]
fn doc_card(
    py: Python<'_>,
    input: &str,
    ruleset_yaml: &str,
    doc_id: &str,
) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    let req = api::DocCardRequest {
        input: input.to_string(),
        ruleset_yaml: ruleset_yaml.to_string(),
        doc_id: doc_id.to_string(),
    };
    finish(py, api::doc_card(&req))
}

/// 入口 8（Task 34）：归档核齐视图 —— 整个导出目录的一次住院一屏数据。
///
/// 返回节点四态（缺件 / 达标 / 人工待确认 / 被连带阻塞）、边（阻塞边标记）、
/// 欠件天数、正反向两个方向的问题清单与每件的通知状态。
#[pyfunction]
#[pyo3(signature = (input, ruleset_yaml, encounter_alias, as_of, business_day, store_path=None))]
fn archive_view(
    py: Python<'_>,
    input: &str,
    ruleset_yaml: &str,
    encounter_alias: &str,
    as_of: &str,
    business_day: &str,
    store_path: Option<String>,
) -> PyResult<PyObject> {
    ruleset_arg(ruleset_yaml).map_err(|e| jsonval::raise(py, e.code(), e.message()))?;
    let req = api::ArchiveViewRequest {
        input: input.to_string(),
        ruleset_yaml: ruleset_yaml.to_string(),
        encounter_alias: encounter_alias.to_string(),
        as_of: as_of.to_string(),
        store_path,
        business_day: business_day.to_string(),
    };
    finish(py, api::archive_view(&req))
}

/// 入口 9（Task 34）：一键发缺件待办（企业微信群机器人 Webhook）。
///
/// `WECOM_WEBHOOK_URL` 缺失时 `E_CFG_MISSING` 且**零次网络调用**。
/// `dry_run=True` 只构造消息体，同样零次外发。
#[pyfunction]
#[pyo3(signature = (encounter_alias, ward, business_day, at, missing, group_hint, dry_run, store_path=None))]
#[allow(clippy::too_many_arguments)]
fn notify_missing(
    py: Python<'_>,
    encounter_alias: &str,
    ward: &str,
    business_day: &str,
    at: &str,
    missing: Vec<(String, String)>,
    group_hint: &str,
    dry_run: bool,
    store_path: Option<String>,
) -> PyResult<PyObject> {
    let req = api::NotifyRequest {
        store_path,
        encounter_alias: encounter_alias.to_string(),
        ward: ward.to_string(),
        business_day: business_day.to_string(),
        at: at.to_string(),
        missing,
        group_hint: group_hint.to_string(),
        dry_run,
    };
    finish(py, api::notify_missing(&req))
}

/// 启动自检：五项凭据缺一即 `E_CFG_MISSING`，**零次网络调用**。
///
/// 桌面壳在 `main()` 里先调它：缺凭据就在弹窗里说清楚，
/// 而不是让用户对着一个空窗口猜。
#[pyfunction]
fn startup_check(py: Python<'_>) -> PyResult<PyObject> {
    match psychdoc_fhir::startup_check() {
        Ok(cfg) => {
            // 凭据本体**不回传给 Python**（也不该进日志）：只回形状信息。
            finish(
                py,
                Ok(json!({
                    "ok": true,
                    "base_url": cfg.base_url,
                    "tenant_id": cfg.tenant_id,
                    "token_path": cfg.token_path,
                })),
            )
        }
        Err(e) => Err(jsonval::raise(py, e.code(), &e.to_string())),
    }
}

/// 启动自检的**逐项报告**：不抛异常，把五项凭据与非凭据配置的结论全摆出来。
///
/// 与 `startup_check` 的分工：后者严格（缺一即抛），供「一定要写回」的
/// 调用点；本函数只汇报，供桌面壳决定「提示但不拦启动」——护士长没配
/// 密钥时仍要能看到本地文书包清单。两者都**零次网络调用**。
#[pyfunction]
fn startup_report(py: Python<'_>) -> PyResult<PyObject> {
    finish(py, api::startup_report())
}

fn parse_entries(entries: Vec<(String, String)>) -> Result<Vec<(String, Value)>, String> {
    entries
        .into_iter()
        .map(|(doc_id, text)| {
            let parsed = serde_json::from_str::<Value>(&text);
            parsed.map(|v| (doc_id.clone(), v)).map_err(|e| {
                format!("件 {doc_id} 的写回资源不是合法 JSON — {e}（E_FHIR_SHAPE）")
            })
        })
        .collect()
}

/// 核心版本号，供 UI 底栏与 `run.sh` 自检比对，避免壳与核心版本漂移。
#[pyfunction]
fn core_version() -> &'static str {
    crate::CORE_VERSION
}

#[pymodule]
fn psychdoc_ffi(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(check_bundle, m)?)?;
    m.add_function(wrap_pyfunction!(review_batch, m)?)?;
    m.add_function(wrap_pyfunction!(preview_fixback, m)?)?;
    m.add_function(wrap_pyfunction!(commit_writeback, m)?)?;
    m.add_function(wrap_pyfunction!(writeback_dryrun, m)?)?;
    m.add_function(wrap_pyfunction!(graph_both_directions, m)?)?;
    m.add_function(wrap_pyfunction!(scan_queue, m)?)?;
    m.add_function(wrap_pyfunction!(doc_card, m)?)?;
    m.add_function(wrap_pyfunction!(archive_view, m)?)?;
    m.add_function(wrap_pyfunction!(notify_missing, m)?)?;
    m.add_function(wrap_pyfunction!(startup_check, m)?)?;
    m.add_function(wrap_pyfunction!(startup_report, m)?)?;
    m.add_function(wrap_pyfunction!(core_version, m)?)?;
    m.add("PsychdocError", m.py().get_type_bound::<PsychdocError>())?;
    m.add("CORE_VERSION", crate::CORE_VERSION)?;
    Ok(())
}
