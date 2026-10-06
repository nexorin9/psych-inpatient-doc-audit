//! OAuth2 `client_credentials` 取 token。
//!
//! 院内 EMR 文档服务普遍走 OAuth2 客户端凭据流：拿 `client_id`/`client_secret`
//! 换 `access_token`，之后每个请求带 `Authorization: Bearer`。
//!
//! 关键行为（对应源产品「解析成实体而非保留字符串」那条纪律的同构延伸）：
//! **token 不可达时不静默继续**。取不到 token 就返回明确错误码，
//! 绝不「不带 Authorization 头也试试」——那种请求要么被拒，要么在
//! 配置错误的端点上匿名成功，属于最坏的一种假成功。

use crate::config::EmrConfig;
use crate::error::{FhirError, Result};
use crate::transport::{HttpMethod, HttpRequest, Transport};

/// 一个已取到的 token。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessToken {
    pub token: String,
    /// 剩余有效秒数（`expires_in`，缺省 300）
    pub expires_in_secs: u64,
}

/// OAuth2 端点缺省有效期。端点不返回 `expires_in` 时用它，
/// 但**仍按短有效期对待**（宁可多换一次 token，不可拿过期 token 撞 401）。
const DEFAULT_EXPIRES_IN: u64 = 300;

/// 请求 token。凭据形态问题已在 `EmrConfig` 阶段拦掉，这里只处理协议层失败。
pub fn fetch_token(
    config: &EmrConfig,
    transport: &dyn Transport,
) -> Result<AccessToken> {
    let url = config.token_url();
    let req = HttpRequest::new(HttpMethod::Post, &url)
        .header("Accept", "application/json")
        .form_body(config.token_form());
    let resp = transport.send(&req)?;
    if !resp.is_success() {
        return Err(FhirError::Status {
            method: "POST",
            url,
            status: resp.status,
            detail: first_line(&resp.body),
        });
    }
    let json: serde_json::Value = serde_json::from_str(&resp.body).map_err(|e| FhirError::Shape {
        detail: format!("token 响应不是合法 JSON：{e}"),
    })?;
    let token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .ok_or_else(|| FhirError::MissingField {
            field: "access_token",
            path: url.clone(),
        })?
        .to_string();
    let expires_in_secs = json
        .get("expires_in")
        .and_then(|v| v.as_u64())
        .unwrap_or(DEFAULT_EXPIRES_IN);
    Ok(AccessToken {
        token,
        expires_in_secs,
    })
}

/// 取 token 失败时给运维看得懂的一句话：优先取 `error_description`。
pub fn first_line(body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for key in ["error_description", "message", "issue"] {
            if let Some(s) = v.get(key).and_then(|x| x.as_str()) {
                return s.chars().take(160).collect();
            }
        }
    }
    trimmed.chars().take(160).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::HttpResponse;

    /// 测试替身：**只**出现在本 crate 的 `#[cfg(test)]` 里。
    struct Stub {
        response: HttpResponse,
    }

    impl Transport for Stub {
        fn send(&self, _req: &HttpRequest) -> Result<HttpResponse> {
            Ok(self.response.clone())
        }
    }

    fn cfg() -> EmrConfig {
        EmrConfig::from_source(|k| match k {
            "EMR_BASE_URL" => Some("https://emr.example-hospital.local/fhir/r4".into()),
            "EMR_CLIENT_ID" => Some("psychdoc-client".into()),
            "EMR_CLIENT_SECRET" => Some("s3cret-value".into()),
            "EMR_TENANT_ID" => Some("ward-a".into()),
            _ => None,
        })
        .unwrap()
    }

    #[test]
    fn missing_access_token_is_a_named_error_not_a_silent_empty_token() {
        let t = Stub {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"token_type":"Bearer","expires_in":600}"#.into(),
            },
        };
        let err = fetch_token(&cfg(), &t).unwrap_err();
        assert_eq!(err.code(), "E_FHIR_FIELD");
        assert!(err.to_string().contains("access_token"));
    }

    #[test]
    fn non_2xx_token_response_carries_the_readable_reason() {
        let t = Stub {
            response: HttpResponse {
                status: 401,
                headers: vec![],
                body: r#"{"error":"invalid_client","error_description":"客户端凭据已停用"}"#
                    .into(),
            },
        };
        let err = fetch_token(&cfg(), &t).unwrap_err();
        assert_eq!(err.code(), "E_FHIR_STATUS");
        assert!(err.to_string().contains("401"));
        assert!(err.to_string().contains("客户端凭据已停用"));
    }

    #[test]
    fn expires_in_defaults_short_when_absent() {
        let t = Stub {
            response: HttpResponse {
                status: 200,
                headers: vec![],
                body: r#"{"access_token":"tok-1"}"#.into(),
            },
        };
        let tok = fetch_token(&cfg(), &t).unwrap();
        assert_eq!(tok.expires_in_secs, DEFAULT_EXPIRES_IN);
    }
}
