//! 企业微信群机器人 Webhook 出站：**符合机器人规范的消息体 + 投递**。
//!
//! ## 凭据纪律（与 `config.rs` 同一把尺）
//!
//! `WECOM_WEBHOOK_URL` **只**从环境变量读，不硬编码、不给默认群地址。
//! 缺失或空白 → `E_CFG_MISSING`，**零次网络调用**——「没配通知」不应该
//! 顺带把审查批扫也拖下水，但也不能假装通知发过了。
//!
//! ## 消息体规范
//!
//! 企业微信群机器人的 markdown 消息体形状固定：
//!
//! ```json
//! { "msgtype": "markdown", "markdown": { "content": "..." } }
//! ```
//!
//! 两条硬约束都来自端点侧，本模块负责落到代码里：
//!
//! - `content` **上限 4096 字节**（超了端点直接拒，不是截断）；
//! - 业务上真正难受的不是字节数而是**条目数**——一份住院缺 30 件时把
//!   30 条全列出来，群里没人看第一行。因此条目先截断并注明
//!   「另有 N 条见归档核齐视图」，剩下的让护士长自己在工具里看。
//!
//! ## 返回体的坑：`errcode == 0` 才算成功
//!
//! 企业微信群机器人**HTTP 200 不代表发送成功**：限流、机器人被移出群、
//! key 失效都回 200 + `{"errcode": 93000, "errmsg": "..."}`。
//! 只看 HTTP 状态码会把这些全记成「已送达」，缺件提醒就此静默丢失。
//! 故 [`WeComAck`] 单独解析业务码，[`deliver`] 在非 0 时返回错误，
//! 由调用方交给发件箱重试。

use crate::error::{FhirError, Result};
use crate::transport::{HttpMethod, HttpRequest, Transport};

use serde_json::{json, Value};

/// Webhook 地址的环境变量名。
pub const ENV_WECOM_WEBHOOK_URL: &str = "WECOM_WEBHOOK_URL";

/// 企业微信 markdown `content` 的字节上限。端点侧硬限制：超了直接拒。
pub const MAX_CONTENT_BYTES: usize = 4096;

/// 一条缺件提醒里列出的单条缺件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingLine {
    /// 文书类型院内显示名（如「约束知情同意」）。
    pub doc_label: String,
    /// 命中规则码（可空——纯缺件没有码）。
    pub code: Option<String>,
    /// 依据 / 提示（可空）。
    pub basis: Option<String>,
}

/// 一次缺件提醒的内容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NoticeContent {
    /// 住院脱敏别名（如 `WARD-A-001`）。
    pub encounter_alias: String,
    pub ward: String,
    /// 业务日（`YYYY-MM-DD`）。跨零点按**交班日**算，不按自然日。
    pub business_day: String,
    pub lines: Vec<MissingLine>,
    /// 收件群名（显示用，不参与消息体）。
    pub group_hint: String,
}

/// 条目截断后追加的一行提示。
pub const OVERFLOW_NOTE: &str = "（其余条目见「归档核齐」视图）";

/// 构造 markdown 正文。
///
/// 两条截断依次生效，顺序不能反：
///
/// 1. **条目数**先截（业务上更难受：三十条没人看）；
/// 2. **字节数**后截（端点侧硬限制）。
///
/// 先截字节的话，一条极长的依据文案就能把后面十几条缺件挤掉，
/// 而护士长关心的恰恰是「还差几件」。
pub fn build_markdown(content: &NoticeContent, max_lines: usize) -> (String, usize) {
    let total = content.lines.len();
    let shown: Vec<&MissingLine> = content.lines.iter().take(max_lines).collect();
    let hidden = total.saturating_sub(shown.len());

    let mut out = String::new();
    out.push_str(&format!(
        "**精神科住院文书缺件提醒**\n> 住院：{}（{}）\n> 业务日：{}\n",
        content.encounter_alias, content.ward, content.business_day
    ));
    if !content.group_hint.is_empty() {
        out.push_str(&format!("> 发送至：{}\n", content.group_hint));
    }
    out.push_str("\n缺件清单：\n");
    for (i, line) in shown.iter().enumerate() {
        out.push_str(&format!("{}. **{}**", i + 1, line.doc_label));
        if let Some(code) = &line.code {
            out.push_str(&format!(" · `{code}`"));
        }
        if let Some(basis) = &line.basis {
            out.push_str(&format!("\n   > 依据：{basis}"));
        }
        out.push('\n');
    }
    if hidden > 0 {
        out.push_str(&format!("\n> 另有 {hidden} 条{OVERFLOW_NOTE}\n"));
    }
    if total == 0 {
        out.push_str("\n> 本次未发现缺件。\n");
    }

    // 字节截断：按 char 边界退，不能切在多字节字符中间（切一半会得到
    // 非法 UTF-8，JSON 序列化直接失败，端点收到的是一段乱码）。
    //
    // 截断位要**先给提示语留出长度**：末尾那句「正文过长已截断」是追加
    // 的，若按 MAX_CONTENT_BYTES 截完再补，整条又会超限——端点直接拒，
    // 而调用方看到的是「明明已经截断了却还是发不出去」。
    const TRUNCATION_NOTE: &str = "\n\n（正文过长已截断，完整清单见「归档核齐」视图）";
    if out.len() > MAX_CONTENT_BYTES {
        let budget = MAX_CONTENT_BYTES.saturating_sub(TRUNCATION_NOTE.len());
        let mut cut = budget;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str(TRUNCATION_NOTE);
    }
    (out, hidden)
}

/// 完整消息体（企业微信群机器人规范形状）。
pub fn build_payload(markdown: &str) -> Value {
    json!({
        "msgtype": "markdown",
        "markdown": { "content": markdown },
    })
}

/// Webhook 端点的应答。**业务码才是成败判据**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComAck {
    pub errcode: i64,
    pub errmsg: String,
}

impl WeComAck {
    pub fn is_ok(&self) -> bool {
        self.errcode == 0
    }

    /// 限流类业务码值得退避后重试；其余（群不存在 / key 失效）重试也没用。
    ///
    /// 企业微信把限流单独给了 45009；机器人不在群里的 93000 属于配置错，
    /// 再发一万次也是同一个错，只会把发件箱刷满。
    pub fn is_rate_limited(&self) -> bool {
        matches!(self.errcode, 45009 | 45011 | 45047)
    }
}

/// 解析应答体。形态不对（不是 JSON / 缺 `errcode`）报 `E_WECOM_SHAPE`。
pub fn parse_ack(body: &str) -> Result<WeComAck> {
    let v: Value = serde_json::from_str(body).map_err(|e| FhirError::WecomShape {
        detail: format!("应答体不是合法 JSON：{e}"),
    })?;
    let obj = v.as_object().ok_or_else(|| FhirError::WecomShape {
        detail: "应答体不是 JSON 对象".into(),
    })?;
    let errcode = obj
        .get("errcode")
        .and_then(|x| x.as_i64())
        .ok_or_else(|| FhirError::WecomShape {
            detail: "应答体缺 errcode 字段".into(),
        })?;
    let errmsg = obj
        .get("errmsg")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    Ok(WeComAck { errcode, errmsg })
}

/// 投递一条 markdown 消息。
///
/// **HTTP 200 + 非 0 业务码判为失败**（`E_WECOM_REJECTED`），不静默当成功。
pub fn deliver(webhook_url: &str, payload: &Value, transport: &dyn Transport) -> Result<WeComAck> {
    // 不走 `HttpRequest::json_body`：那条会把 Content-Type 设成
    // `application/fhir+json`（FHIR 专用）。Webhook 端点不吃这个类型，
    // 严格实现会回 415。
    let body = serde_json::to_string(payload).map_err(|e| FhirError::WecomShape {
        detail: format!("消息体序列化失败：{e}"),
    })?;
    let req = HttpRequest::new(HttpMethod::Post, webhook_url)
        .header("Content-Type", "application/json; charset=utf-8")
        .header("Accept", "application/json")
        .with_body(body);
    let resp = transport.send(&req)?;
    if !resp.is_success() {
        return Err(FhirError::WecomStatus {
            status: resp.status,
            detail: first_line(&resp.body),
        });
    }
    let ack = parse_ack(&resp.body)?;
    if !ack.is_ok() {
        return Err(FhirError::WecomRejected {
            errcode: ack.errcode,
            errmsg: ack.errmsg.clone(),
        });
    }
    Ok(ack)
}

/// 企业微信 Webhook 配置。**只从环境变量读**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeComConfig {
    pub webhook_url: String,
    /// 正文最多列几条（截断后附「另有 N 条」）。
    pub max_lines: usize,
}

impl WeComConfig {
    pub fn from_env() -> Result<Self> {
        Self::from_source(|k| std::env::var(k).ok())
    }

    /// 与 [`WeComConfig::from_env`] 同形，注入取值来源便于测「缺 / 空 / 形态错」。
    /// **不发起任何网络调用。**
    pub fn from_source<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let raw = lookup(ENV_WECOM_WEBHOOK_URL);
        let Some(v) = raw.filter(|s| !s.trim().is_empty()) else {
            return Err(FhirError::CfgMissing {
                count: 1,
                missing: ENV_WECOM_WEBHOOK_URL.to_string(),
            });
        };
        let url = v.trim().to_string();
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Err(FhirError::CfgInvalid {
                key: ENV_WECOM_WEBHOOK_URL,
                detail: "应以 http:// 或 https:// 开头（企业微信群机器人地址形如 https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=…）".into(),
            });
        }
        if !url.contains("key=") {
            return Err(FhirError::CfgInvalid {
                key: ENV_WECOM_WEBHOOK_URL,
                detail: "缺少 key= 参数——企业微信群机器人地址必须带 key，否则端点会回 93000".into(),
            });
        }
        if url.contains(char::is_whitespace) {
            return Err(FhirError::CfgInvalid {
                key: ENV_WECOM_WEBHOOK_URL,
                detail: "不得含空白字符（多半是复制时带上了换行）".into(),
            });
        }
        Ok(Self {
            webhook_url: url,
            max_lines: 10,
        })
    }
}

fn first_line(body: &str) -> String {
    body.lines().next().unwrap_or("").chars().take(200).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content(n: usize) -> NoticeContent {
        NoticeContent {
            encounter_alias: "WARD-A-001".into(),
            ward: "精神科三病区".into(),
            business_day: "2026-03-03".into(),
            group_hint: "精神科病区文书群".into(),
            lines: (0..n)
                .map(|i| MissingLine {
                    doc_label: format!("文书 {i}"),
                    code: Some(format!("QC-PSY-0{}", i % 9 + 1)),
                    basis: Some("《精神科住院护理常规》第 4.2 条".into()),
                })
                .collect(),
        }
    }

    #[test]
    fn the_payload_matches_the_wecom_robot_shape() {
        let p = build_payload("hello");
        assert_eq!(p["msgtype"], "markdown");
        assert_eq!(p["markdown"]["content"], "hello");
    }

    #[test]
    fn lines_are_truncated_and_the_rest_is_named() {
        let (md, hidden) = build_markdown(&content(30), 10);
        assert_eq!(hidden, 20);
        assert!(md.contains("另有 20 条"));
        assert!(md.contains(OVERFLOW_NOTE));
        // 只列前 10 条：第 11 条的标题不该出现
        assert!(md.contains("文书 9"));
        assert!(!md.contains("文书 10"));
    }

    #[test]
    fn nothing_is_hidden_when_it_fits() {
        let (md, hidden) = build_markdown(&content(3), 10);
        assert_eq!(hidden, 0);
        assert!(!md.contains(OVERFLOW_NOTE));
    }

    #[test]
    fn an_oversized_body_is_cut_on_a_char_boundary() {
        let mut c = content(4);
        // 每条依据 ~2000 字节，4 条必然超过 4096
        for line in c.lines.iter_mut() {
            line.basis = Some("依".repeat(1000));
        }
        let (md, _) = build_markdown(&c, 10);
        assert!(md.len() <= MAX_CONTENT_BYTES + 64, "实际 {} 字节", md.len());
        assert!(md.contains("正文过长已截断"));
        // 截断不得切出非法 UTF-8
        assert!(std::str::from_utf8(md.as_bytes()).is_ok());
        assert!(md.chars().count() > 0);
    }

    #[test]
    fn an_empty_package_says_so_instead_of_sending_an_empty_list() {
        let (md, hidden) = build_markdown(&content(0), 10);
        assert_eq!(hidden, 0);
        assert!(md.contains("本次未发现缺件"));
    }

    #[test]
    fn a_zero_errcode_is_the_only_success() {
        let ok = parse_ack(r#"{"errcode":0,"errmsg":"ok"}"#).unwrap();
        assert!(ok.is_ok());
        let limited = parse_ack(r#"{"errcode":45009,"errmsg":"rate limit"}"#).unwrap();
        assert!(!limited.is_ok());
        assert!(limited.is_rate_limited());
        // 机器人不在群里：重试无用
        let gone = parse_ack(r#"{"errcode":93000,"errmsg":"invalid webhook url"}"#).unwrap();
        assert!(!gone.is_rate_limited());
    }

    #[test]
    fn a_malformed_body_is_a_shape_error_not_a_silent_success() {
        assert_eq!(
            parse_ack("not json").unwrap_err().code(),
            "E_WECOM_SHAPE"
        );
        assert_eq!(
            parse_ack(r#"{"errmsg":"ok"}"#).unwrap_err().code(),
            "E_WECOM_SHAPE"
        );
    }

    #[test]
    fn a_missing_env_var_is_reported_before_any_network_call() {
        let err = WeComConfig::from_source(|_| None).unwrap_err();
        assert_eq!(err.code(), "E_CFG_MISSING");
        assert!(err.to_string().contains(ENV_WECOM_WEBHOOK_URL));
        // 空白值同样算缺失
        for bad in ["", "   "] {
            let err = WeComConfig::from_source(|k| {
                (k == ENV_WECOM_WEBHOOK_URL).then(|| bad.to_string())
            })
            .unwrap_err();
            assert_eq!(err.code(), "E_CFG_MISSING");
        }
    }

    #[test]
    fn a_malformed_url_is_cfg_invalid_not_cfg_missing() {
        for bad in [
            "qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send",
        ] {
            let err = WeComConfig::from_source(|k| {
                (k == ENV_WECOM_WEBHOOK_URL).then(|| bad.to_string())
            })
            .unwrap_err();
            assert_eq!(err.code(), "E_CFG_INVALID", "{bad} 应报形态错");
        }
    }

    #[test]
    fn a_valid_url_loads() {
        let cfg = WeComConfig::from_source(|k| {
            (k == ENV_WECOM_WEBHOOK_URL)
                .then(|| "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc123".to_string())
        })
        .unwrap();
        assert_eq!(cfg.max_lines, 10);
    }
}