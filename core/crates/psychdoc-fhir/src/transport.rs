//! HTTP 传输层：一个可替换的 [`Transport`]  trait + 真实实现 [`UreqTransport`]。
//!
//! 为什么要抽一层 trait：院内对接代码里有**两件**必须被分别钉死的事——
//!
//! 1. **真实协议**：请求方法 / 头 / 请求体必须符合 FHIR R4 规范，由
//!    [`UreqTransport`] 打真实 socket 实现（Task 11 的协议级测试就打在它上面）；
//! 2. **「缺配置时 0 次外发」这条不变量**：它要求能在**不触网**的前提下
//!    数清外发次数。生产路径走 `UreqTransport`，测试路径走 [`Transport`] 的
//!    记录实现，两者跑的是**同一套**上层代码。
//!
//! 脱敏协议替身（本地 FHIR 协议替身）只允许出现在 `tests/` 层，
//! **不是**产品默认路径；产品默认路径永远是 `UreqTransport` 指向
//! `EMR_BASE_URL` 的真实端点。

use crate::error::{FhirError, Result};

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpMethod {
    Get,
    Post,
}

impl HttpMethod {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Post => "POST",
        }
    }
}

/// 一次出站请求。字段与 FHIR R4 规范一一对应，不做「万能请求对象」。
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: HttpMethod,
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Option<String>,
}

impl HttpRequest {
    pub fn new(method: HttpMethod, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            headers: Vec::new(),
            body: None,
        }
    }

    pub fn header(mut self, name: &str, value: impl Into<String>) -> Self {
        self.headers.push((name.to_string(), value.into()));
        self
    }

    pub fn json_body(mut self, body: impl Into<String>) -> Self {
        self.headers
            .push(("Content-Type".to_string(), "application/fhir+json".into()));
        self.body = Some(body.into());
        self
    }

    pub fn form_body(mut self, body: impl Into<String>) -> Self {
        self.headers.push((
            "Content-Type".to_string(),
            "application/x-www-form-urlencoded".into(),
        ));
        self.body = Some(body.into());
        self
    }

    /// 只挂请求体、**不动**已有请求头。
    ///
    /// 存在的理由：出站不只 FHIR 一种协议。企业微信 Webhook 端点不吃
    /// `application/fhir+json`（`json_body` 硬设的那个类型），要的是
    /// `application/json; charset=utf-8`。若为了换 content-type 去改
    /// `json_body`，等于让所有协议共用一个会互相覆盖的默认值——
    /// 「能发出去」被改成一个和协议无关的猜测。
    pub fn with_body(mut self, body: impl Into<String>) -> Self {
        self.body = Some(body.into());
        self
    }

    /// 头值查询（大小写不敏感，HTTP 头名本来就不区分大小写）。
    pub fn header_value(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 出站传输。**生产实现见 [`UreqTransport`]**。
pub trait Transport: Send + Sync {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse>;
}

/// 真实 HTTP 客户端（`ureq`，启用 TLS）。这是产品的**唯一**出站通道。
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new(timeout: Duration) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_secs(5))
            .timeout(timeout)
            .build();
        Self { agent }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new(Duration::from_secs(15))
    }
}

impl Transport for UreqTransport {
    fn send(&self, req: &HttpRequest) -> Result<HttpResponse> {
        let mut call = match req.method {
            HttpMethod::Get => self.agent.get(&req.url),
            HttpMethod::Post => self.agent.post(&req.url),
        };
        for (name, value) in &req.headers {
            call = call.set(name, value);
        }
        let sent = match &req.body {
            Some(b) => call.send_string(b),
            None => call.call(),
        };
        match sent {
            Ok(resp) => {
                let status = resp.status();
                let headers = resp
                    .headers_names()
                    .iter()
                    .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
                    .collect();
                let body = resp.into_string().unwrap_or_default();
                Ok(HttpResponse {
                    status,
                    headers,
                    body,
                })
            }
            Err(ureq::Error::Status(status, resp)) => {
                // 4xx / 5xx 仍把响应体与响应头一起带回来，两者都不能丢：
                // - 体：FHIR 的 OperationOutcome 里有可读的拒绝原因，
                //   直接丢掉等于把「为什么被拒」扔了；
                // - 头：429 的 `Retry-After` 决定退避多久。丢头就退化成
                //   盲目重试，把服务端明确说的「等 30 秒」变成「立刻再来」，
                //   限流只会越撞越紧。
                let headers = resp
                    .headers_names()
                    .iter()
                    .filter_map(|n| resp.header(n).map(|v| (n.clone(), v.to_string())))
                    .collect();
                let body = resp.into_string().unwrap_or_default();
                Ok(HttpResponse {
                    status,
                    headers,
                    body,
                })
            }
            Err(ureq::Error::Transport(t)) => Err(FhirError::Unreachable {
                url: req.url.clone(),
                detail: t.to_string(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_lookup_is_case_insensitive() {
        let req = HttpRequest::new(HttpMethod::Get, "https://x/fhir")
            .header("X-Tenant-Id", "ward-a")
            .json_body("{}");
        assert_eq!(req.header_value("x-tenant-id"), Some("ward-a"));
        assert_eq!(req.header_value("Content-Type"), Some("application/fhir+json"));
    }

    #[test]
    fn fhir_content_type_is_used_not_plain_json() {
        // FHIR R4 要求 application/fhir+json；发 application/json 的端点
        // 在严格实现上会 415。
        let req = HttpRequest::new(HttpMethod::Post, "https://x/fhir").json_body("{}");
        assert_eq!(req.header_value("Content-Type"), Some("application/fhir+json"));
    }

    #[test]
    fn success_range_is_2xx() {
        let mk = |status| HttpResponse {
            status,
            headers: vec![],
            body: String::new(),
        };
        assert!(mk(200).is_success());
        assert!(mk(201).is_success());
        assert!(!mk(400).is_success());
        assert!(!mk(500).is_success());
    }
}
