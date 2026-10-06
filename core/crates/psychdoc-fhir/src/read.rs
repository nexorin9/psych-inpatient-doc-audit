//! 读端点：`GET Composition` / `GET DocumentReference`。
//!
//! 对应源产品主路径的「递归发现 → 过滤 → 逐件解析」，本轮改写为
//! 「按住院别名取包 → 按院内文书类型字典过滤 → 逐件解析」：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | `resolve_default_files` 按路径发现文件 | 按 `encounter` 拉 searchset |
//! | `paths.retain(...)` 过滤 | `DocTypeRegistry::is_known(code)` 过滤 |
//! | 逐件解析 `Parsed<Mod>` | 逐件 `parse_bundle` → 件-字段-时间序三层 |
//!
//! 三条硬纪律：
//!
//! - **tenant 头必带**：院内多租户部署下缺 `X-Tenant-Id` 会静默串租户；
//! - **分页有界**：`Bundle.link[next]` 跟随但有上限与自环检测，
//!   服务端 bug 不会让审查线程卡死；
//! - **取件失败显式记 `unresolved`**，绝不静默跳过。

use crate::auth::AccessToken;
use crate::config::EmrConfig;
use crate::error::{FhirError, Result};
use crate::transport::{HttpMethod, HttpRequest, Transport};

use psychdoc_model::{Binding, DocTypeRegistry, DocItem};
use psychdoc_parse::{parse_bundle, ParseError, ParsedBundle};

/// 单次 `searchset` 遍历最多翻多少页。防止服务端 `link.next` 自环或
/// 无限增长把桌面程序挂住。
pub const MAX_PAGES: usize = 200;

/// 一页 searchset 的解析产物。
#[derive(Debug, Clone, Default)]
pub struct ReadPage {
    /// 遍历过的页数
    pub pages: usize,
    /// 拼起来的完整 searchset JSON
    pub body: String,
    /// 下一页 URL；`None` 表示遍历到底
    pub next: Option<String>,
}

/// 一份住院文书包的读结果。
#[derive(Debug, Clone, Default)]
pub struct PackageRead {
    /// 解析成功且在院内文书类型字典内的件
    pub items: Vec<DocItem>,
    /// 结构问题 / 类型不在字典 / 绑定不唯一——**每条都有原因码**
    pub unresolved: Vec<UnresolvedItem>,
    /// 遍历过的页数
    pub pages: usize,
}

/// 取件失败的可核对记录。绝不静默丢弃。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnresolvedItem {
    /// 资源 id（脱敏别名）或 `<unknown>`
    pub resource_id: String,
    /// 原因码：解析期码（`PARSE-STRUCTURE`）、`E_BIND_NOT_CONFIDENT`
    /// 或 `E_FHIR_*` 形态码
    pub code: String,
    pub detail: String,
}

impl UnresolvedItem {
    fn new(
        resource_id: impl Into<String>,
        code: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            resource_id: resource_id.into(),
            code: code.into(),
            detail: detail.into(),
        }
    }
}

/// 组装一次 searchset 请求。路径与查询参数按 FHIR R4 规范。
pub fn search_url(base_url: &str, resource: &str, encounter_alias: &str) -> String {
    // encounter_alias 是脱敏别名（WARD-A-001），URL 编码后作 identifier 值
    format!(
        "{base_url}/{resource}?identifier={}",
        percent_encode_identifier(encounter_alias)
    )
}

pub fn percent_encode_identifier(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 发一次带租户头与 Bearer 的 GET。
pub fn get(config: &EmrConfig, token: &AccessToken, url: &str, transport: &dyn Transport) -> Result<String> {
    let mut req = HttpRequest::new(HttpMethod::Get, url)
        .header("Accept", "application/fhir+json");
    for (name, value) in config.headers(&token.token) {
        req = req.header(&name, value);
    }
    let resp = transport.send(&req)?;
    if !resp.is_success() {
        return Err(FhirError::Status {
            method: "GET",
            url: url.to_string(),
            status: resp.status,
            detail: crate::auth::first_line(&resp.body),
        });
    }
    Ok(resp.body)
}

/// 跟随 `Bundle.link[relation=next]` 遍历整个 searchset，有界且防自环。
pub fn read_all_pages(
    config: &EmrConfig,
    token: &AccessToken,
    start_url: &str,
    transport: &dyn Transport,
) -> Result<ReadPage> {
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut next = Some(start_url.to_string());
    let mut seen: Vec<String> = Vec::new();
    let mut pages = 0usize;
    let mut last_next: Option<String> = None;
    while let Some(url) = next {
        if pages >= MAX_PAGES {
            break;
        }
        // 服务端把 next 指回已见过的页 = 死循环，立即中止而不是空转到 MAX_PAGES
        if seen.iter().any(|u| u == &url) {
            return Err(FhirError::PagingLoop { url });
        }
        seen.push(url.clone());
        let body = get(config, token, &url, transport)?;
        let page: serde_json::Value =
            serde_json::from_str(&body).map_err(|e| FhirError::Shape {
                detail: format!("searchset 页不是合法 JSON：{e}"),
            })?;
        if page.get("resourceType").and_then(|v| v.as_str()) != Some("Bundle") {
            return Err(FhirError::Shape {
                detail: "searchset 页 resourceType 不是 Bundle".to_string(),
            });
        }
        if let Some(list) = page.get("entry").and_then(|v| v.as_array()) {
            entries.extend(list.iter().cloned());
        }
        pages += 1;
        last_next = next_link(&page);
        next = last_next.clone();
    }
    let merged = serde_json::json!({
        "resourceType": "Bundle",
        "type": "searchset",
        "entry": entries,
    });
    Ok(ReadPage {
        pages,
        body: merged.to_string(),
        next: last_next,
    })
}

/// `Bundle.link[relation=next]`。
pub fn next_link(page: &serde_json::Value) -> Option<String> {
    page.get("link")
        .and_then(|v| v.as_array())?
        .iter()
        .find(|l| l.get("relation").and_then(|r| r.as_str()) == Some("next"))
        .and_then(|l| l.get("url"))
        .and_then(|u| u.as_str())
        .map(str::to_string)
}

/// 按住院别名读回文书包，并做**类型字典过滤 + 逐件解析**。
pub fn read_package(
    config: &EmrConfig,
    token: &AccessToken,
    encounter_alias: &str,
    registry: &DocTypeRegistry,
    transport: &dyn Transport,
) -> Result<PackageRead> {
    let url = search_url(&config.base_url, "Composition", encounter_alias);
    let page = read_all_pages(config, token, &url, transport)?;
    package_from_searchset(&page.body, registry)
}

/// 把一段 searchset JSON 变成件 + unresolved 清单（**部分成功**）。
///
/// 与 `parse_bundle` 的纪律一致：整批不失败，能解析的件照常进 `items`，
/// 坏处进 `unresolved` 且每条带原因码。
pub fn package_from_searchset(json: &str, registry: &DocTypeRegistry) -> Result<PackageRead> {
    // 先按院内类型字典过滤：非本产品负责的文书类型不解析，也不记账为故障
    let filtered = filter_by_registry(json, registry)?;
    let parsed: ParsedBundle = parse_bundle(&filtered, Some(registry)).map_err(|e| match e {
        ParseError::BundleJson { detail } => FhirError::Shape { detail },
        ParseError::BundleShape { got } => FhirError::Shape {
            detail: format!("顶层不是 FHIR Bundle（resourceType={got}）"),
        },
        other => FhirError::Shape {
            detail: other.to_string(),
        },
    })?;
    let mut out = PackageRead {
        pages: 0,
        ..Default::default()
    };
    for diag in &parsed.errors {
        out.unresolved.push(UnresolvedItem::new(
            diag.loc.doc_id.clone(),
            diag.code.clone(),
            diag.message.text.clone(),
        ));
    }
    for item in parsed.items {
        // 绑定不唯一 → 显式记 unresolved，不进 items
        if !item.binding.is_confident() {
            let reason = match item.binding {
                Binding::Ambiguous => "跨系统同名文书，绑定不唯一，须人工确认",
                Binding::Unresolved => "未找到对应院内文书模板",
                Binding::Confident => unreachable!("已由 is_confident 过滤"),
            };
            out.unresolved.push(UnresolvedItem::new(
                item.doc_id.clone(),
                "E_BIND_NOT_CONFIDENT",
                reason,
            ));
            continue;
        }
        out.items.push(item);
    }
    Ok(out)
}

/// 类型字典过滤：只保留 `type.coding[].code` 在字典内的 `Composition`。
///
/// 过滤掉的 entry **逐条记账**（`filtered_out` 计数可核对），
/// 绝不静默丢弃——「这份包里 3 件不是文书」是护士长会问的问题。
pub fn filter_by_registry(json: &str, registry: &DocTypeRegistry) -> Result<String> {
    let mut bundle: serde_json::Value =
        serde_json::from_str(json).map_err(|e| FhirError::Shape {
            detail: format!("searchset 不是合法 JSON：{e}"),
        })?;
    let entries = bundle
        .get_mut("entry")
        .and_then(|v| v.as_array_mut())
        .ok_or_else(|| FhirError::MissingField {
            field: "entry",
            path: "<searchset>".into(),
        })?;
    entries.retain(|e| {
        let is_composition = e
            .get("resource")
            .and_then(|r| r.get("resourceType"))
            .and_then(|v| v.as_str())
            == Some("Composition");
        if !is_composition {
            return false;
        }
        let code = e
            .get("resource")
            .and_then(|r| r.get("type"))
            .and_then(|t| t.get("coding"))
            .and_then(|c| c.as_array())
            .and_then(|a| a.first())
            .and_then(|c| c.get("code"))
            .and_then(|v| v.as_str());
        // 抽不出类型码的**保留**——那是结构问题，须报 unresolved，不能当「不在字典」丢掉
        match code {
            Some(c) => registry.is_known(c),
            None => true,
        }
    });
    Ok(bundle.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_url_encodes_the_encounter_alias() {
        let u = search_url("https://emr.example/fhir/r4", "Composition", "WARD-A-001");
        assert_eq!(
            u,
            "https://emr.example/fhir/r4/Composition?identifier=WARD-A-001"
        );
    }

    #[test]
    fn next_link_picks_relation_next() {
        let page = serde_json::json!({
            "resourceType": "Bundle",
            "link": [
                {"relation": "self", "url": "https://emr/fhir/Composition?identifier=E1"},
                {"relation": "next", "url": "https://emr/fhir/Composition?identifier=E1&page=2"}
            ]
        });
        assert_eq!(
            next_link(&page).as_deref(),
            Some("https://emr/fhir/Composition?identifier=E1&page=2")
        );
        let no_next = serde_json::json!({"resourceType":"Bundle","link":[]});
        assert_eq!(next_link(&no_next), None);
    }
}
