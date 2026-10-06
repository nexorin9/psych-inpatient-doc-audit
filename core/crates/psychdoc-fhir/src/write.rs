//! 写端点：`Bundle` transaction 建件 + `Provenance` 留痕。
//!
//! ## 为什么必须是真的写回，不是只读旁路
//!
//! 业务上确实要**新建缺件骨架**：护士长在受控文书包里补一份约束知情同意，
//! 这一步在业务系统里就是「建件」。只读旁路意味着护士长仍要在 EMR 里
//! 手工建一遍——那本产品就只是多了一层看图。所以本模块实现**真实写协议**：
//! `Bundle`（`type=transaction`）+ 每个 entry 的 `Bundle.entry.request`，
//! 外加一条 `Provenance` 记录「谁、哪一刻、依据哪版规则集建了这件」。
//!
//! ## 事务原子性
//!
//! FHIR `transaction` Bundle 本身是服务端原子提交，但**客户端这一侧也必须**：
//!
//! - 端点返回 200 但某条 entry 的 `response.status` 非 2xx → 判**整批失败**
//!   （`E_FHIR_TXN_REJECTED`），0 条已生效，本地草稿保留；
//! - 传输层失败（连接断开 / 5xx）→ 同样整批失败，草稿保留可重试；
//! - **绝不**在部分成功后写本地「已写回」——那是最坏的一种假成功：
//!   本地说写成功了，服务端其实没建件。
//!
//! 对应 `spec.md` §4「幂等 / 并发 / 重试」：**transaction 外发恰好一次**
//! 由 Task 28 的幂等键在**落库前**拦截，本模块只负责「发出去的那一次」
//! 要么整批生效要么什么都不生效。

use crate::auth::AccessToken;
use crate::config::EmrConfig;
use crate::error::{FhirError, Result};
use crate::transport::{HttpMethod, HttpRequest, Transport};

use serde_json::{json, Value};

/// 一件待建 / 待改的资源。
#[derive(Debug, Clone, PartialEq)]
pub struct WriteEntry {
    /// 资源类型，如 `Composition` / `DocumentReference`
    pub resource_type: String,
    /// 业务逻辑 id（脱敏别名）；`full_url` 用 `urn:uuid` 避免与真实 id 冲突
    pub logical_id: String,
    /// `POST`（新建）/ `PUT`（改）
    pub method: WriteMethod,
    pub resource: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteMethod {
    /// 新建件
    Post,
    /// 改已有件
    Put,
}

impl WriteMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Post => "POST",
            Self::Put => "PUT",
        }
    }
}

/// 一次写回批次的上下文（Provenance 需要）。
#[derive(Debug, Clone)]
pub struct WritebackContext {
    pub encounter_alias: String,
    /// 推进人（脱敏操作人标识）
    pub actor: String,
    /// RFC3339 时刻
    pub recorded_at: String,
    /// 当时生效的规则集 sha256
    pub ruleset_sha256: String,
    /// 幂等键集合（`sha256(住院别名 + 件 id + 内容哈希)`）
    pub idempotency_keys: Vec<String>,
}

/// 构造 `Bundle{type=transaction}`。
///
/// 结构严格按 FHIR R4：每个 entry 必须带
/// `request{method,url}`，`url` 是**资源类型相对路径**（`Composition`）
/// 或 `Composition/<id>`（PUT）。缺 `request` 的 entry 服务端会拒。
pub fn build_transaction(entries: &[WriteEntry], ctx: &WritebackContext) -> Value {
    let mut bundle_entries: Vec<Value> = Vec::with_capacity(entries.len() + 1);
    for (i, e) in entries.iter().enumerate() {
        // full_url 必须是绝对 URL 或 urn:uuid；同一批内唯一
        let full_url = format!("urn:uuid:00000000-0000-4000-8000-{:012x}", i + 1);
        let url = match e.method {
            WriteMethod::Post => e.resource_type.clone(),
            WriteMethod::Put => format!("{}/{}", e.resource_type, e.logical_id),
        };
        bundle_entries.push(json!({
            "fullUrl": full_url,
            "resource": e.resource,
            "request": {
                "method": e.method.as_str(),
                "url": url,
            }
        }));
    }
    // Provenance 随批提交：它记录的就是**这一批**做了什么
    bundle_entries.push(provenance_entry(ctx, entries));

    json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": bundle_entries,
    })
}

/// `Provenance` entry。R4 必填：`target` / `recorded` / `agent` / `activity`。
fn provenance_entry(ctx: &WritebackContext, entries: &[WriteEntry]) -> Value {
    let targets: Vec<Value> = entries
        .iter()
        .map(|e| {
            json!({
                "reference": format!("{}/{}", e.resource_type, e.logical_id)
            })
        })
        .collect();
    json!({
        "fullUrl": "urn:uuid:ffffffff-0000-4000-8000-000000000001",
        "resource": {
            "resourceType": "Provenance",
            "target": targets,
            "recorded": ctx.recorded_at,
            "activity": {
                "coding": [{
                    "system": "http://terminology.hl7.org/CodeSystem/v3-DataOperation",
                    "code": "CREATE",
                    "display": "建件 / 补写"
                }],
                "text": "精神科住院文书审查系统 · 建件回写"
            },
            "agent": [{
                "type": {
                    "coding": [{
                        "system": "http://terminology.hl7.org/CodeSystem/provenance-participant-type",
                        "code": "author",
                        "display": "人工确认后由本工具提交"
                    }]
                },
                "who": { "display": ctx.actor }
            }],
            "entity": [{
                "role": "source",
                "what": {
                    "display": format!(
                        "规则集 {} · 住院别名 {} · 幂等键 {}",
                        short_hash(&ctx.ruleset_sha256),
                        ctx.encounter_alias,
                        ctx.idempotency_keys.join(",")
                    )
                }
            }]
        },
        "request": { "method": "POST", "url": "Provenance" }
    })
}

fn short_hash(full: &str) -> String {
    full.chars().take(12).collect()
}

/// 一次 transaction 的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransactionOutcome {
    /// 端点整体状态码（整批提交成功为 200）
    pub status: u16,
    /// 成功建的件（`Composition/<id>` 等）
    pub created: Vec<String>,
    /// Provenance 落库结果
    pub provenance: Option<String>,
}

/// 发送整批 transaction。
///
/// **空 selection 直接拒绝**（`E_FHIR_TXN_EMPTY`），零次网络调用——
/// 「没勾选任何条就点提交」是 UI 上真实会发生的操作，后端必须拦。
pub fn send_transaction(
    config: &EmrConfig,
    token: &AccessToken,
    entries: &[WriteEntry],
    ctx: &WritebackContext,
    transport: &dyn Transport,
) -> Result<TransactionOutcome> {
    if entries.is_empty() {
        return Err(FhirError::TxnEmpty);
    }
    let bundle = build_transaction(entries, ctx);
    let url = config.base_url.clone();
    let mut req = HttpRequest::new(HttpMethod::Post, &url)
        .header("Accept", "application/fhir+json")
        .json_body(serde_json::to_string(&bundle).map_err(|e| FhirError::Shape {
            detail: format!("transaction 序列化失败：{e}"),
        })?);
    for (name, value) in config.headers(&token.token) {
        req = req.header(&name, value);
    }
    let resp = transport.send(&req)?;
    if !resp.is_success() {
        return Err(FhirError::Status {
            method: "POST",
            url,
            status: resp.status,
            detail: crate::auth::first_line(&resp.body),
        });
    }
    parse_transaction_response(&resp.body, entries.len())
}

/// 解析 `transaction-response` Bundle，逐条 entry 判定成败。
///
/// 端点回 200 但 entry 里混着 4xx 的情况真实存在（部分资源被业务规则拒），
/// 那时**整批判失败**：本地不得写「已写回」。
pub fn parse_transaction_response(body: &str, expected: usize) -> Result<TransactionOutcome> {
    let v: Value = serde_json::from_str(body).map_err(|e| FhirError::Shape {
        detail: format!("transaction 响应不是合法 JSON：{e}"),
    })?;
    if v.get("resourceType").and_then(|t| t.as_str()) != Some("Bundle") {
        return Err(FhirError::Shape {
            detail: "transaction 响应 resourceType 不是 Bundle".to_string(),
        });
    }
    let entries = v
        .get("entry")
        .and_then(|e| e.as_array())
        .ok_or_else(|| FhirError::MissingField {
            field: "entry",
            path: "<transaction-response>".into(),
        })?;
    let mut created = Vec::new();
    let mut provenance = None;
    let mut failed = 0usize;
    for entry in entries {
        let status = entry
            .get("response")
            .and_then(|r| r.get("status"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        let location = entry
            .get("response")
            .and_then(|r| r.get("location"))
            .and_then(|l| l.as_str())
            .unwrap_or("<无 location>");
        let is_ok = status.starts_with('2');
        if !is_ok {
            failed += 1;
            continue;
        }
        let is_prov = entry
            .get("resource")
            .and_then(|r| r.get("resourceType"))
            .and_then(|t| t.as_str())
            == Some("Provenance");
        if is_prov {
            provenance = Some(location.to_string());
        } else {
            created.push(location.to_string());
        }
    }
    // 期望件 + 1 条 Provenance
    let total = expected + 1;
    if failed > 0 || created.len() != expected || provenance.is_none() {
        return Err(FhirError::TxnRejected {
            failed: failed.max(total - created.len()),
            total,
            detail: format!(
                "期望建 {} 件 + 1 条 Provenance，实际成功 {} 件、失败 {failed} 条；本地草稿保留未动",
                expected,
                created.len()
            ),
        });
    }
    Ok(TransactionOutcome {
        status: 200,
        created,
        provenance,
    })
}

/// 把已构造好的 `Bundle` POST 出去并返回原始响应体。
///
/// [`send_transaction`] 是「构造 + 发送」的一体版本；确认回写闭环
/// （`confirm` / `withdraw`）需要先构造、**再**做本地的勾选与状态判定，
/// 所以这里把发送拆成可复用的两半——**请求形状与 [`send_transaction`]
/// 完全一致**（同一套 tenant / Bearer / FHIR Content-Type 头）。
pub fn post_bundle(
    config: &EmrConfig,
    token: &AccessToken,
    bundle: &Value,
    transport: &dyn Transport,
) -> Result<String> {
    let url = config.base_url.clone();
    let mut req = HttpRequest::new(HttpMethod::Post, &url)
        .header("Accept", "application/fhir+json")
        .json_body(serde_json::to_string(bundle).map_err(|e| FhirError::Shape {
            detail: format!("transaction 序列化失败：{e}"),
        })?);
    for (name, value) in config.headers(&token.token) {
        req = req.header(&name, value);
    }
    let resp = transport.send(&req)?;
    if !resp.is_success() {
        return Err(FhirError::Status {
            method: "POST",
            url,
            status: resp.status,
            detail: crate::auth::first_line(&resp.body),
        });
    }
    Ok(resp.body)
}

/// 反向 transaction：撤回已建件（Task 12 用）。
///
/// 用 `DELETE` 而非「再造一条空件」——撤回的语义是「回到没建之前」。
pub fn build_reversal(entries: &[WriteEntry], ctx: &WritebackContext) -> Value {
    let mut bundle_entries: Vec<Value> = entries
        .iter()
        .enumerate()
        .map(|(i, e)| {
            json!({
                "fullUrl": format!("urn:uuid:00000000-0000-4000-8000-{:012x}", i + 1),
                "request": {
                    "method": "DELETE",
                    "url": format!("{}/{}", e.resource_type, e.logical_id)
                }
            })
        })
        .collect();
    bundle_entries.push(provenance_entry(ctx, entries));
    json!({
        "resourceType": "Bundle",
        "type": "transaction",
        "entry": bundle_entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> WritebackContext {
        WritebackContext {
            encounter_alias: "WARD-A-001".into(),
            actor: "脱敏-护士长A".into(),
            recorded_at: "2026-03-03T08:10:00+08:00".into(),
            ruleset_sha256: "e4eda0c79c95a1b2c3d4e5f60718293a".into(),
            idempotency_keys: vec!["k1".into(), "k2".into()],
        }
    }

    fn entry() -> WriteEntry {
        WriteEntry {
            resource_type: "Composition".into(),
            logical_id: "WARD-A-001-CONSENT-01".into(),
            method: WriteMethod::Post,
            resource: json!({"resourceType": "Composition", "id": "WARD-A-001-CONSENT-01"}),
        }
    }

    #[test]
    fn every_transaction_entry_carries_a_request_block() {
        let b = build_transaction(&[entry()], &ctx());
        let entries = b["entry"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "1 件 + 1 条 Provenance");
        for e in entries {
            assert!(e.get("request").is_some(), "entry 缺 request 块：{e}");
            assert!(e["request"]["method"].is_string());
            assert!(e["request"]["url"].is_string());
        }
    }

    #[test]
    fn post_uses_bare_resource_type_and_put_uses_the_logical_id() {
        let mut e = entry();
        e.method = WriteMethod::Put;
        let b = build_transaction(&[e], &ctx());
        assert_eq!(b["entry"][0]["request"]["url"], "Composition/WARD-A-001-CONSENT-01");
        assert_eq!(b["entry"][0]["request"]["method"], "PUT");
    }

    #[test]
    fn provenance_is_r4_shaped_and_names_the_ruleset_and_idempotency_keys() {
        let b = build_transaction(&[entry()], &ctx());
        let prov = &b["entry"][1]["resource"];
        assert_eq!(prov["resourceType"], "Provenance");
        assert!(prov["target"].as_array().unwrap().len() == 1);
        assert!(prov["recorded"].is_string());
        assert!(prov["activity"]["coding"].is_array());
        assert!(prov["agent"].as_array().unwrap().len() == 1);
        let entity = prov["entity"][0]["what"]["display"].as_str().unwrap();
        assert!(entity.contains("e4eda0c79c95"), "Provenance 须带规则集短哈希");
        assert!(entity.contains("k1") && entity.contains("k2"), "须带幂等键");
    }

    #[test]
    fn full_urls_are_unique_within_one_batch() {
        let b = build_transaction(&[entry(), entry(), entry()], &ctx());
        let urls: Vec<&str> = b["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|e| e["fullUrl"].as_str())
            .collect();
        let mut uniq = urls.clone();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(urls.len(), uniq.len(), "同批 fullUrl 必须唯一");
    }

    #[test]
    fn a_partially_rejected_response_fails_the_whole_batch() {
        // 端点回 200，但第二条 entry 是 422 —— 业务规则拒了这一件
        let body = json!({
            "resourceType": "Bundle",
            "type": "transaction-response",
            "entry": [
                {"response": {"status": "201 Created", "location": "Composition/WARD-A-001-CONSENT-01/_history/1"}},
                {"response": {"status": "422 Unprocessable", "location": "Composition"}}
            ]
        })
        .to_string();
        let err = parse_transaction_response(&body, 1).unwrap_err();
        assert_eq!(err.code(), "E_FHIR_TXN_REJECTED");
        assert!(err.to_string().contains("本地草稿保留"));
    }

    #[test]
    fn a_response_without_provenance_fails_the_batch() {
        let body = json!({
            "resourceType": "Bundle",
            "type": "transaction-response",
            "entry": [
                {"response": {"status": "201 Created", "location": "Composition/X/_history/1"}}
            ]
        })
        .to_string();
        let err = parse_transaction_response(&body, 1).unwrap_err();
        assert_eq!(err.code(), "E_FHIR_TXN_REJECTED");
    }

    #[test]
    fn a_fully_successful_response_reports_created_and_provenance() {
        let body = json!({
            "resourceType": "Bundle",
            "type": "transaction-response",
            "entry": [
                {"response": {"status": "201 Created", "location": "Composition/WARD-A-001-CONSENT-01/_history/1"}},
                {"response": {"status": "201 Created", "location": "Provenance/PS-1/_history/1"},
                 "resource": {"resourceType": "Provenance"}}
            ]
        })
        .to_string();
        let out = parse_transaction_response(&body, 1).unwrap();
        assert_eq!(out.created.len(), 1);
        assert_eq!(out.provenance.as_deref(), Some("Provenance/PS-1/_history/1"));
    }

    #[test]
    fn reversal_uses_delete_and_still_carries_provenance() {
        let b = build_reversal(&[entry()], &ctx());
        assert_eq!(b["entry"][0]["request"]["method"], "DELETE");
        assert_eq!(
            b["entry"][0]["request"]["url"],
            "Composition/WARD-A-001-CONSENT-01"
        );
        assert_eq!(b["entry"][1]["resource"]["resourceType"], "Provenance");
    }
}
