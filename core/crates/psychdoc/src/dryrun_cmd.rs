//! `psychdoc writeback-dryrun` —— 构造 Bundle transaction，**一次网络请求都不发**。
//!
//! ## 这条命令存在的理由
//!
//! 真实写回要凭据、要 token、要打 `Bundle transaction`——在 CI 上没有凭据，
//! 在生产上没人敢拿一份真文书包反复试。于是「这台机器上打算发出去的报文长什么样」
//! 就没人能回答，直到某天真的发出去、发现少了一条 `request.url`。
//!
//! 干跑正是补这个洞：**用同一套 [`build_transaction`] 构造同一份 Bundle**，
//! 打出来、落盘、供人核对，而网络层压根不进。CI 因此能在无凭据环境里
//! 守住报文形状；信息科也能在真发之前先把报文过一遍法务与信息科的眼光。
//!
//! ## 纪律
//!
//! - **零外发**是结构保证而不是自觉：这条路径上不存在 `Transport` 实例，
//!   也不调用 [`psychdoc_fhir`] 的任何发送函数。集成测试用一个真实
//!   `TcpListener` 数连接数来钉住这一点（见 `tests/cli_contract.rs`）。
//! - 空勾选**拒绝构造**，与真发同码 `E_FHIR_TXN_EMPTY`：干跑报成功、
//!   真发报「什么都没勾」，那是最坏的组合。
//! - 幂等键照 [`psychdoc_store::idempotency_key`] 实算并打印出来，
//!   让人在干跑阶段就能看出「这一件今天会不会被幂等吞掉」。

use crate::contract::{CliError, EXIT_OK};
use crate::sink;
use psychdoc_fhir::write::{build_transaction, WriteEntry, WriteMethod, WritebackContext};
use serde_json::{json, Value};

/// `selection.json` 的形状。
///
/// 与 `psychdoc_fhir::confirm` 的 `WritebackEntry` 同名同义，但**刻意不带**
/// `run_id`：干跑的用途是核对报文形状，批次号是审查侧的东西，
/// 在这里填一个假的批次号只会让人误以为干跑产物可以直接发出去。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    pub encounter_alias: String,
    /// 推进人（脱敏操作人标识）。
    #[serde(default)]
    pub actor: String,
    /// RFC3339 时刻。**由文件给**而不是读系统时钟：CI 要能逐字节复现同一份报文。
    pub at: String,
    /// 当时生效的规则集 sha256；缺则报形态错——审计追溯要靠它，
    /// 留空等于让这次写回在 Provenance 里查不到是哪一版规则下的判断。
    pub ruleset_sha256: String,
    pub entries: Vec<SelectedEntry>,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SelectedEntry {
    pub doc_id: String,
    pub content_sha256: String,
    /// 待写的 FHIR R4 资源（`Composition` 等），整段进 `Bundle.entry.resource`。
    pub resource: Value,
}

/// 跑一次干跑。返回退出码（成功恒为 0；失败经 `Err` 带稳定码出去）。
pub fn run(
    selection_path: &str,
    json_mode: bool,
    bundle_out: Option<&str>,
) -> Result<u8, CliError> {
    let raw = std::fs::read_to_string(selection_path).map_err(|e| {
        CliError::new("E_IO", format!("读不到 selection 文件 {selection_path} — {e}"))
    })?;
    let sel: Selection = serde_json::from_str(&raw).map_err(|e| {
        CliError::new(
            "E_BUNDLE_SHAPE",
            format!("selection 文件 {selection_path} 不是合法的勾选清单 — {e}"),
        )
    })?;

    // 时刻与规则集哈希在这里先卡住：它们进 Provenance，形态不对时
    // 让它一路构造成 Bundle 再在下游炸，报错会指向报文而不是配置。
    chrono::DateTime::parse_from_rfc3339(&sel.at).map_err(|e| {
        CliError::new("E_BAD_TIMESTAMP", format!("at {} 不是 RFC3339：{e}", sel.at))
    })?;
    if sel.ruleset_sha256.trim().is_empty() {
        return Err(CliError::new(
            "E_ARG",
            "selection 缺 ruleset_sha256：Provenance 要靠它回答「这次写回是哪一版规则下的判断」",
        ));
    }
    // 空勾选与真发同码：干跑报成功、真发报「什么都没勾」，是最坏的组合。
    if sel.entries.is_empty() {
        return Err(CliError::new(
            "E_FHIR_TXN_EMPTY",
            "勾选清单是空的，拒绝构造 transaction（与真发同码 E_FHIR_TXN_EMPTY）",
        ));
    }

    let entries: Vec<WriteEntry> = sel
        .entries
        .iter()
        .map(|e| WriteEntry {
            resource_type: resource_type_of(&e.resource).to_string(),
            logical_id: e.doc_id.clone(),
            method: WriteMethod::Post,
            resource: e.resource.clone(),
        })
        .collect();

    let idempotency_keys: Vec<String> = sel
        .entries
        .iter()
        .map(|e| {
            psychdoc_store::idempotency_key(&sel.encounter_alias, &e.doc_id, &e.content_sha256)
        })
        .collect();

    let ctx = WritebackContext {
        encounter_alias: sel.encounter_alias.clone(),
        actor: if sel.actor.trim().is_empty() {
            "CLI 干跑未署名操作人".to_string()
        } else {
            sel.actor.clone()
        },
        recorded_at: sel.at.clone(),
        ruleset_sha256: sel.ruleset_sha256.clone(),
        idempotency_keys: idempotency_keys.clone(),
    };

    // 真实出站链路在这一行之后**根本不存在**：本模块不 import 任何发送函数，
    // 也不构造 Transport。报文与真发同源（同一个 build_transaction），
    // 但网络层不进。
    let bundle = build_transaction(&entries, &ctx);

    print_summary(&sel, &bundle, &idempotency_keys);

    let body = serde_json::to_string_pretty(&bundle)
        .map_err(|e| CliError::new("E_BUNDLE_SHAPE", format!("Bundle 序列化失败：{e}")))?;
    if let Some(p) = bundle_out {
        if let Some(parent) = std::path::Path::new(p).parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    CliError::new("E_IO", format!("建不出目录 {} — {e}", parent.display()))
                })?;
            }
        }
        std::fs::write(p, &body)
            .map_err(|e| CliError::new("E_IO", format!("写不进 {p} — {e}")))?;
        sink::say(&format!("Bundle transaction 已写到 {p}"));
    }
    if json_mode {
        sink::emit_json(
            &serde_json::to_string_pretty(&json!({
                "ok": true,
                "dry_run": true,
                "outbound_calls": 0,
                "encounter_alias": sel.encounter_alias,
                "entry_count": sel.entries.len(),
                "idempotency_keys": idempotency_keys,
                "bundle": bundle,
            }))
            .map_err(|e| CliError::new("E_BUNDLE_SHAPE", e.to_string()))?,
        );
    }

    sink::say("");
    sink::say(&format!(
        "退出码 {EXIT_OK}：{}",
        crate::contract::describe(EXIT_OK)
    ));
    Ok(EXIT_OK)
}

/// 从资源里取 `resourceType`；取不到按 `Composition` 兜底。
///
/// 兜底而不是报错：FHIR 报文里 `resourceType` 本来就是资源自身的字段，
/// 勾选清单里没写多半是手抄漏了——但**拦下来更糟**，会让整批写回在报文阶段
/// 就失败，而真发时服务端是按 entry.request.url 认资源类型的。
/// 真正被核对的是 URL 与 Provenance，不是这里。
fn resource_type_of(resource: &Value) -> &str {
    resource
        .get("resourceType")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Composition")
}

fn print_summary(sel: &Selection, bundle: &Value, keys: &[String]) {
    sink::say(&format!(
        "写回干跑：{} · {} 条 · 时刻 {}",
        sel.encounter_alias,
        sel.entries.len(),
        sel.at
    ));
    sink::say(&format!(
        "Bundle 类型：{}（transaction = 要么整批生效，要么零条生效）",
        bundle
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("<缺>")
    ));
    sink::say(&format!(
        "规则集 sha256：{}",
        &sel.ruleset_sha256[..sel.ruleset_sha256.len().min(12)]
    ));

    let bundle_entries = bundle
        .get("entry")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    sink::say(&format!(
        "Bundle 条目 {} 条（其中末条为 Provenance）：",
        bundle_entries.len()
    ));
    for e in &bundle_entries {
        let req = e.get("request");
        let resource_type = e
            .get("resource")
            .and_then(|r| r.get("resourceType"))
            .and_then(|v| v.as_str())
            .unwrap_or("-");
        match req {
            Some(r) => sink::say(&format!(
                "  {} {} · resource={}",
                r.get("method").and_then(|v| v.as_str()).unwrap_or("-"),
                r.get("url").and_then(|v| v.as_str()).unwrap_or("-"),
                resource_type
            )),
            // Provenance 是**资源**不是请求：它是这次写回的证据，
            // 不走 request，所以「没有 request.method」是正确的形状。
            None => sink::say(&format!("  （无 request，证据资源）· resource={resource_type}")),
        }
    }

    sink::say("幂等键（同一份内容今天重复提交会被这几条挡下）：");
    for (doc_id, key) in sel.entries.iter().map(|e| &e.doc_id).zip(keys) {
        sink::say(&format!("  {doc_id} · {key}"));
    }

    sink::say("");
    sink::say("本次未发起任何网络请求（干跑：不取 token、不连端点、不落库）。");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_resource_without_resource_type_falls_back_to_composition() {
        assert_eq!(resource_type_of(&json!({"id": "X"})), "Composition");
        assert_eq!(resource_type_of(&json!({"resourceType": "  "})), "Composition");
        assert_eq!(
            resource_type_of(&json!({"resourceType": "DocumentReference"})),
            "DocumentReference"
        );
    }

    #[test]
    fn an_unknown_key_in_the_selection_is_refused_rather_than_ignored() {
        // 勾选清单被当成配置读：多打一个字段多半是抄错了模板，
        // 静默忽略等于让「我明明写了 actor」这类误解在真发时才暴露。
        let bad = r#"{"encounter_alias":"A","at":"2026-03-02T23:50:00+08:00",
            "ruleset_sha256":"abc","actor":"x","entries":[],"typo_field":1}"#;
        assert!(serde_json::from_str::<Selection>(bad).is_err());
    }
}