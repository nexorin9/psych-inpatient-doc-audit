//! 凭据加载器：**五项环境变量逐一校验，一次列全缺失项**。
//!
//! 这是 `spec.md` §4「正式对接层」这条闭环维度的落点，也是全项目
//! **唯一**读 `EMR_*` 凭据的地方。纪律：
//!
//! - 凭据**只**从环境变量读，**不提供任何默认凭据**；
//! - 缺任一项 → `E_CFG_MISSING`，把**全部**缺失项一次列全（运维一次改完，
//!   而不是跑三次报三遍）；
//! - 空串与纯空白**算缺失**（复制 `.env.example` 后忘了填是最常见的事故）；
//! - 形态不合法与缺失**分开报码**（`E_CFG_INVALID`）；
//! - 加载**本身不发起任何网络调用**——「缺配置」必须在 0 次外发的前提下被检出，
//!   否则「静默降级」就藏在校验的网络往返里。
//!
//! 替身纪律：脱敏协议替身只允许出现在 `tests/` 层，**不是**产品默认路径。

use crate::error::{FhirError, Result};

/// 五项凭据的环境变量名。顺序即校验与报错顺序，固定下来便于测试断言。
pub const ENV_BASE_URL: &str = "EMR_BASE_URL";
pub const ENV_CLIENT_ID: &str = "EMR_CLIENT_ID";
pub const ENV_CLIENT_SECRET: &str = "EMR_CLIENT_SECRET";
pub const ENV_TENANT_ID: &str = "EMR_TENANT_ID";

/// 全部必需的凭据变量（企业微信 Webhook 属通知出站，由 Task 30 单独校验，
/// 不属于「读 EMR」这条链路的启动前提）。
pub const REQUIRED_VARS: [&str; 4] = [ENV_BASE_URL, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_TENANT_ID];

/// 院内 EMR 对接配置。字段名与 `.env.example` 逐字对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EmrConfig {
    /// FHIR R4 服务根地址，形如 `https://emr.example-hospital.local/fhir/r4`
    pub base_url: String,
    pub client_id: String,
    pub client_secret: String,
    /// 院内租户标识，写入 `X-Tenant-Id` 请求头
    pub tenant_id: String,
    /// OAuth2 token 端点路径（拼在 `base_url` 之后）
    pub token_path: String,
}

impl EmrConfig {
    /// 从环境变量加载并校验。**不发起任何网络调用。**
    pub fn from_env() -> Result<Self> {
        fn lookup(key: &str) -> Option<String> {
            std::env::var(key).ok()
        }
        Self::from_source(lookup)
    }

    /// 与 [`EmrConfig::from_env`] 同形，但注入取值的来源。
    ///
    /// 存在的理由是可测：环境变量是**进程全局**状态，测试里改它再改回来
    /// 会在并行测试下互相踩。注入一个纯函数来源，测试就能对
    /// 「缺 1 项 / 缺 3 项 / 全空 / 形态错」四种情形各断言一次，
    /// 且不碰全局状态。
    pub fn from_source<F>(lookup: F) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        let (missing, raw) = split(lookup);
        if !missing.is_empty() {
            return Err(FhirError::CfgMissing {
                count: missing.len(),
                missing: missing.join("、"),
            });
        }
        // 形态问题**一次列全**再报：只报第一个会让运维改一个跑一次。
        let problems = shape_problems(&raw);
        if let Some((key, _)) = problems.first() {
            return Err(cfg_invalid(key, &problems));
        }
        let get = |k: &str| -> String {
            raw.iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
                .expect("缺失项已在上方排除")
        };
        Ok(Self {
            base_url: get(ENV_BASE_URL).trim_end_matches('/').to_string(),
            client_id: get(ENV_CLIENT_ID),
            client_secret: get(ENV_CLIENT_SECRET),
            tenant_id: get(ENV_TENANT_ID),
            token_path: "/oauth2/token".to_string(),
        })
    }

    /// OAuth2 token 端点完整 URL。
    pub fn token_url(&self) -> String {
        format!("{}{}", self.base_url, self.token_path)
    }

    /// 请求头：`X-Tenant-Id` + `Authorization`。
    ///
    /// 租户头是**院内多租户部署**的实际需求：同一台 EMR 服务器按病区/院区
    /// 分租户，缺这个头会拉到另一个租户的数据——静默串租户比报错危险得多。
    pub fn headers(&self, access_token: &str) -> Vec<(String, String)> {
        vec![
            ("X-Tenant-Id".to_string(), self.tenant_id.clone()),
            (
                "Authorization".to_string(),
                format!("Bearer {access_token}"),
            ),
        ]
    }

    /// OAuth2 `client_credentials` 授予的表单体。
    pub fn token_form(&self) -> String {
        format!(
            "grant_type=client_credentials&client_id={}&client_secret={}",
            urlencode(&self.client_id),
            urlencode(&self.client_secret)
        )
    }
}

/// 把四项凭据按 `REQUIRED_VARS` 顺序拆成 **缺失清单** 与 **已填原始值**。
///
/// 空串与纯空白都算「没填」——`.env.example` 复制出来后忘填就是这个形状，
/// 而「以为填了其实没填」正是启动失败最难查的一类现场事故。
pub fn split<F>(lookup: F) -> (Vec<&'static str>, Vec<(&'static str, String)>)
where
    F: Fn(&str) -> Option<String>,
{
    let mut missing: Vec<&'static str> = Vec::new();
    let mut raw: Vec<(&'static str, String)> = Vec::with_capacity(REQUIRED_VARS.len());
    for key in REQUIRED_VARS {
        match lookup(key) {
            Some(v) if !v.trim().is_empty() => raw.push((key, v.trim().to_string())),
            _ => missing.push(key),
        }
    }
    (missing, raw)
}

/// 四项凭据的形态校验。返回**全部**问题，不是第一个。
///
/// **只校验 `raw` 里实际出现的项**。没出现的（缺失）一律跳过：缺失已经由
/// [`split`] 归到 `E_CFG_MISSING`，在这里再报一条「URL 形态错」会让值守的人
/// 去查一个他还没填的值的格式——而他该做的是把那行填上。
///
/// 一次列全的理由很实际：值守的人是在改 `.env`，每报一条就重跑一次启动
/// 自检要花十几秒，还容易漏掉剩下那条。
pub fn shape_problems(raw: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
    let has = |k: &str| raw.iter().any(|(key, _)| *key == k);
    let get = |k: &str| -> &str {
        raw.iter()
            .find(|(key, _)| *key == k)
            .map(|(_, v)| v.as_str())
            .unwrap_or_default()
    };
    let mut out: Vec<(&'static str, String)> = Vec::new();
    let base_url = get(ENV_BASE_URL);
    let tenant_id = get(ENV_TENANT_ID);
    let client_id = get(ENV_CLIENT_ID);
    let client_secret = get(ENV_CLIENT_SECRET);

    if has(ENV_BASE_URL) {
        if !(base_url.starts_with("http://") || base_url.starts_with("https://")) {
            out.push((ENV_BASE_URL, "应以 http:// 或 https:// 开头".into()));
        } else if base_url.contains(char::is_whitespace) {
            out.push((
                ENV_BASE_URL,
                "不得含空白字符（多半是复制时带上了换行）".into(),
            ));
        }
    }
    if has(ENV_TENANT_ID) && tenant_id.contains(char::is_whitespace) {
        out.push((ENV_TENANT_ID, "租户标识不得含空白".into()));
    }
    if has(ENV_CLIENT_ID) && client_id.len() < 4 {
        out.push((
            ENV_CLIENT_ID,
            "形态可疑：短于 4 字符，多半是没填".into(),
        ));
    }
    if has(ENV_CLIENT_SECRET) && client_secret.len() < 8 {
        out.push((
            ENV_CLIENT_SECRET,
            "形态可疑：短于 8 字符，多半是没填".into(),
        ));
    }
    out
}

/// 把一组形态问题收成一条 `E_CFG_INVALID`，文案里逐条列全。
pub fn cfg_invalid(first_key: &'static str, problems: &[(&'static str, String)]) -> FhirError {
    FhirError::CfgInvalid {
        key: first_key,
        detail: problems
            .iter()
            .map(|(k, d)| format!("{k}：{d}"))
            .collect::<Vec<_>>()
            .join("；"),
    }
}

/// 极简百分号编码：只处理 OAuth2 表单需要的 `+ / & = ? #` 与空格。
/// 不用外部 crate 是为了不为一处编码引入依赖——凭据字符集本就很窄。
fn urlencode(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for b in raw.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn full() -> Vec<(&'static str, String)> {
        vec![
            (ENV_BASE_URL, "https://emr.example-hospital.local/fhir/r4".into()),
            (ENV_CLIENT_ID, "psychdoc-client".into()),
            (ENV_CLIENT_SECRET, "s3cret-value".into()),
            (ENV_TENANT_ID, "ward-a".into()),
        ]
    }

    fn source<'a>(entries: &'a [(&'static str, String)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |k| {
            entries
                .iter()
                .find(|(key, _)| *key == k)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn all_present_loads_and_trims_trailing_slash() {
        let cfg = EmrConfig::from_source(source(&full())).expect("应加载成功");
        assert_eq!(cfg.base_url, "https://emr.example-hospital.local/fhir/r4");
        assert_eq!(cfg.token_url(), "https://emr.example-hospital.local/fhir/r4/oauth2/token");
    }

    #[test]
    fn missing_one_is_listed_with_its_name() {
        let mut e = full();
        e.retain(|(k, _)| *k != ENV_TENANT_ID);
        let err = EmrConfig::from_source(source(&e)).unwrap_err();
        assert_eq!(err.code(), "E_CFG_MISSING");
        assert!(err.to_string().contains(ENV_TENANT_ID));
        assert!(err.to_string().contains("缺少 1 项"));
    }

    #[test]
    fn missing_three_are_all_listed_at_once() {
        let mut e = full();
        e.retain(|(k, _)| *k != ENV_TENANT_ID && *k != ENV_CLIENT_ID && *k != ENV_BASE_URL);
        let err = EmrConfig::from_source(source(&e)).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("缺少 3 项"), "{text}");
        for key in [ENV_BASE_URL, ENV_CLIENT_ID, ENV_TENANT_ID] {
            assert!(text.contains(key), "{key} 应在缺失清单里：{text}");
        }
    }

    #[test]
    fn blank_and_whitespace_only_count_as_missing() {
        for bad in ["", "   ", "\t"] {
            let mut e = full();
            for (k, v) in e.iter_mut() {
                if *k == ENV_CLIENT_SECRET {
                    *v = bad.to_string();
                }
            }
            let err = EmrConfig::from_source(source(&e)).unwrap_err();
            assert_eq!(err.code(), "E_CFG_MISSING", "空白值应算缺失：{bad:?}");
        }
    }

    #[test]
    fn malformed_values_are_cfg_invalid_not_cfg_missing() {
        let cases: [(&str, &str, &str); 4] = [
            (ENV_BASE_URL, "emr.example.local", "E_CFG_INVALID"),
            (ENV_BASE_URL, "https://emr .local", "E_CFG_INVALID"),
            (ENV_TENANT_ID, "ward a", "E_CFG_INVALID"),
            (ENV_CLIENT_ID, "ab", "E_CFG_INVALID"),
        ];
        for (key, bad, expect) in cases {
            let mut e = full();
            for (k, v) in e.iter_mut() {
                if *k == key {
                    *v = bad.to_string();
                }
            }
            let err = EmrConfig::from_source(source(&e)).unwrap_err();
            assert_eq!(err.code(), expect, "{key}={bad:?} 应报形态错");
            assert!(err.to_string().contains(key));
        }
    }

    #[test]
    fn headers_carry_tenant_and_bearer() {
        let cfg = EmrConfig::from_source(source(&full())).unwrap();
        let h = cfg.headers("tok-123");
        assert!(h.iter().any(|(k, v)| k == "X-Tenant-Id" && v == "ward-a"));
        assert!(h
            .iter()
            .any(|(k, v)| k == "Authorization" && v == "Bearer tok-123"));
    }

    #[test]
    fn token_form_percent_encodes_reserved_chars_in_the_secret() {
        // 真实 client_secret 常含 `& = ? # / +` 等保留字符：
        // 不编码就会把表单体的字段边界打乱，token 请求静默拿到错误凭据
        let mut e = full();
        for (k, v) in e.iter_mut() {
            if *k == ENV_CLIENT_SECRET {
                *v = "a&b=c/d e".into();
            }
        }
        let cfg = EmrConfig::from_source(source(&e)).unwrap();
        let form = cfg.token_form();
        assert!(form.starts_with("grant_type=client_credentials&"));
        assert!(form.contains("client_id=psychdoc-client"));
        // 三个 `&` 只应出现两个（分隔 client_id 与 client_secret）
        assert_eq!(form.matches('&').count(), 2, "字段边界被打乱：{form}");
        assert!(form.contains("client_secret=a%26b%3Dc%2Fd+e"), "{form}");
        assert!(!form.contains(' '), "空格应编码为 +：{form}");
    }
}
