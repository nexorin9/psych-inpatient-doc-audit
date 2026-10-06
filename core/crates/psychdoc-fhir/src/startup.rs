//! **启动自检**：一个入口读全部配置，缺一项即失败，**一次列全**。
//!
//! ## 为什么要有这一层（而不是各自 `from_env()`）
//!
//! 早先的形状是「谁用到谁读」：[`crate::config::EmrConfig`] 读四项 EMR 凭据、
//! [`crate::wecom::WeComConfig`] 读一项 Webhook。结果是**同一个启动动作要跑两次
//! 才能知道缺了什么**：跑批脚本先撞 `E_CFG_MISSING`（四项里的哪几项），
//! 补完再跑，又撞一次（Webhook 那项）。运维改一次配置要跑两轮自检，
//! 而第二轮的报错还会让人以为第一轮漏报了。
//!
//! 这一层把**五项凭据 + 三项非凭据配置**一次性读齐：
//!
//! | 类别 | 变量 | 缺了会怎样 |
//! |------|------|-----------|
//! | 凭据 | `EMR_BASE_URL` / `EMR_CLIENT_ID` / `EMR_CLIENT_SECRET` / `EMR_TENANT_ID` | 写回与增量拉取不可用 |
//! | 凭据 | `WECOM_WEBHOOK_URL` | 缺件提醒发不出去 |
//! | 非凭据 | `PSYCHDOC_RULESET` | 审查用哪套规则；缺则用交付规则集 |
//! | 非凭据 | `PSYCHDOC_DATA_DIR` | 库与日志落哪；缺则用平台用户数据目录 |
//! | 非凭据 | `PSYCHDOC_LOG_LEVEL` | 结构化日志级别；缺则 `info` |
//!
//! ## 三条硬纪律
//!
//! 1. **不提供任何默认凭据**。宁可启动失败，也不让「没配」看起来像
//!    「配了但端点恰好还没上线」——后者会一路走到写回才炸，而那时
//!    草稿已经建了。
//! 2. **缺失与形态错分开报码**：缺变量是运维去 `.env` 补，形态错
//!    （URL 没头、secret 短到像没填）是配置内容被改坏了。合成一个码，
//!    两边的人都会去查错的地方。
//! 3. **加载本身零次网络调用**。这是「不静默降级」能被断言的前提：
//!    若校验要靠一次外发才能完成，「没配」就会被「端点暂时不通」掩盖，
//!    而后者会触发退避重试——重试一万次也不会长出环境变量来。
//!
//! ## 与「不拦启动」的关系
//!
//! 桌面壳**允许**在缺写回凭据时启动（护士长仍要能看本地文书包清单，
//! 见 `desktop/main.py::writeback_hint`）。那是**呈现层**的决定：
//! 本模块仍然是严格的——它报出来的 `E_CFG_MISSING` 就是写回与通知
//! 这两条链路当时的真实状态，调用方拿它决定拦不拦，而不是让它
//! 自己悄悄降级成「本地假成功」。

use std::path::{Path, PathBuf};

use crate::config::{cfg_invalid, shape_problems, split, EmrConfig};
use crate::error::{FhirError, Result};
use crate::wecom::WeComConfig;

/// 通知出站的环境变量名（与 [`REQUIRED_VARS`] 一起构成五项凭据）。
pub const ENV_WECOM: &str = crate::wecom::ENV_WECOM_WEBHOOK_URL;

/// 规则集路径。
pub const ENV_RULESET: &str = "PSYCHDOC_RULESET";
/// 数据目录（sqlite 库与结构化日志的落位）。
pub const ENV_DATA_DIR: &str = "PSYCHDOC_DATA_DIR";
/// 结构化日志级别。
pub const ENV_LOG_LEVEL: &str = "PSYCHDOC_LOG_LEVEL";

/// **五项**必需凭据。顺序固定，测试与报错文案都按它断言。
pub const ALL_CREDENTIAL_VARS: [&str; 5] = [
    crate::config::ENV_BASE_URL,
    crate::config::ENV_CLIENT_ID,
    crate::config::ENV_CLIENT_SECRET,
    crate::config::ENV_TENANT_ID,
    ENV_WECOM,
];

/// 结构化日志级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LogLevel {
    /// 只记错误。
    Error,
    /// 错误 + 告警（告警是「要人看」的最小集合）。
    Warn,
    /// 常规运行摘要（默认级别：跑批时 `warn` 只在出问题时才有输出，
    /// 看不出「今天看了几件、命中几条」）。
    #[default]
    Info,
    /// 逐件事件，排障用。
    Debug,
}

impl LogLevel {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warn => "warn",
            Self::Info => "info",
            Self::Debug => "debug",
        }
    }

    /// 解析环境变量取值。**不认识的字面量不静默回落**——
    /// 回落成 `info` 会让「我明明设了 debug 却什么都看不到」无从查起。
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "error" => Some(Self::Error),
            "warn" | "warning" => Some(Self::Warn),
            "info" => Some(Self::Info),
            "debug" => Some(Self::Debug),
            _ => None,
        }
    }
}

/// 启动自检通过后的完整配置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupConfig {
    /// EMR 对接配置（四项凭据均已校验形态）。
    pub emr: EmrConfig,
    /// 企业微信通知配置。
    pub wecom: WeComConfig,
    /// 规则集路径（相对路径已按 `base` 解析）。
    pub ruleset: PathBuf,
    /// 数据目录（相对路径同样按 `base` 解析）。`None` 表示未显式设置，
    /// 由 `psychdoc_store::paths::data_dir` 按平台规则解析。
    pub data_dir: Option<PathBuf>,
    /// 结构化日志级别。
    pub log_level: LogLevel,
}

impl StartupConfig {
    /// 从**进程环境**加载并校验。**零次网络调用。**
    pub fn from_env() -> Result<Self> {
        // 相对路径按**真实工作目录**解析，不是 `Path::new(".")`：
        // 后者会让 `rulesets/hospital.yaml` 变成 `./rulesets/hospital.yaml`，
        // 界面上看着对，实际读文件时可能被解析到别处（Windows 上尤其）。
        let base = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self::from_source(|k| std::env::var(k).ok(), &base)
    }

    /// 注入取值来源与相对路径基准，便于对四种情形各断言一次。
    ///
    /// `base` 的存在理由：环境变量里写相对路径（`rulesets/hospital.yaml`）
    /// 在跑批机上没问题、在双击启动的桌面端就随工作目录漂掉了。统一按
    /// 同一个基准解析，两种启动方式读到的是同一个文件。
    pub fn from_source<F>(lookup: F, base: &Path) -> Result<Self>
    where
        F: Fn(&str) -> Option<String>,
    {
        // --- 五项凭据：缺失优先，且**一次列全** ---
        let (missing_emr, raw_emr) = split(&lookup);
        let wecom_raw = lookup(ENV_WECOM);
        let mut missing: Vec<&'static str> = missing_emr;
        if wecom_raw.as_deref().is_none_or(|v| v.trim().is_empty()) {
            missing.push(ENV_WECOM);
        }
        if !missing.is_empty() {
            return Err(FhirError::CfgMissing {
                count: missing.len(),
                missing: missing.join("、"),
            });
        }

        // --- 形态校验：缺失已排除后才做，且一次列全 ---
        let problems = shape_problems(&raw_emr);
        if let Some(first) = problems.first() {
            return Err(cfg_invalid(first.0, &problems));
        }
        // 缺失与形态问题已在上方排掉，这两次构造**只可能成功**。
        // 仍用 `?` 而不是 `expect`：真出现第三条路径时，报错比 panic 可查。
        let emr = EmrConfig::from_source(&lookup)?;
        let wecom = WeComConfig::from_source(&lookup)?;

        // --- 非凭据配置 ---
        let ruleset = resolve_optional(&lookup, ENV_RULESET, base)
            .unwrap_or_else(|| base.join("rulesets").join("hospital.yaml"));
        let data_dir = resolve_optional(&lookup, ENV_DATA_DIR, base);
        let log_level = match lookup(ENV_LOG_LEVEL) {
            None => LogLevel::default(),
            Some(v) if v.trim().is_empty() => LogLevel::default(),
            Some(v) => LogLevel::parse(&v).ok_or_else(|| FhirError::CfgInvalid {
                key: ENV_LOG_LEVEL,
                detail: format!(
                    "只认 error / warn / info / debug，收到 {:?}；不静默回落到默认级别",
                    v.trim()
                ),
            })?,
        };

        Ok(Self {
            emr,
            wecom,
            ruleset,
            data_dir,
            log_level,
        })
    }
}

/// 相对路径按基准解析；绝对路径原样返回；未设置或纯空白返回 `None`。
pub fn resolve_optional<F>(lookup: &F, key: &str, base: &Path) -> Option<PathBuf>
where
    F: Fn(&str) -> Option<String>,
{
    let raw = lookup(key)?;
    let v = raw.trim();
    if v.is_empty() {
        return None;
    }
    let p = PathBuf::from(v);
    Some(if p.is_absolute() { p } else { base.join(p) })
}

/// 启动自检的**逐项报告**。
///
/// 与 [`StartupConfig`] 的区别：这一版**不因为缺凭据而失败**，而是把
/// 每一项的结论都摆出来。给「要不要拦启动」这件事用——桌面壳要用它把
/// 「缺哪几项」一次性印在窗口上，而不是只报第一条。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StartupReport {
    /// 已就绪的项：**既填了、形态又对**。
    ///
    /// 「填了但形态错」不在这里——它归 [`Self::invalid`]。把两者混进
    /// 一栏会让窗口上出现「`EMR_BASE_URL` 就绪」而点提交就报错的情形。
    pub ready: Vec<&'static str>,
    /// 缺失（未设置或纯空白）的项。
    pub missing: Vec<&'static str>,
    /// 设置了但形态不合法：`变量 → 原因`。
    pub invalid: Vec<(&'static str, String)>,
    /// 不阻断启动、只作提示的项。
    pub notices: Vec<String>,
    /// 凭据齐备时为 `true`。**只读**（凭据本体不外传，见 [`Self::render`]）。
    pub writeback_ready: bool,
}

impl StartupReport {
    /// 跑一遍自检，把五项凭据的结论逐条收齐。**零次网络调用。**
    pub fn check<F>(lookup: F) -> Self
    where
        F: Fn(&str) -> Option<String>,
    {
        let (missing_emr, raw_emr) = split(&lookup);
        let wecom_ok = lookup(ENV_WECOM).is_some_and(|v| !v.trim().is_empty());
        let mut missing: Vec<&'static str> = missing_emr;
        if !wecom_ok {
            missing.push(ENV_WECOM);
        }
        // `shape_problems` 只校验 `raw` 里实际出现的项，缺失项不会在这里重复出现。
        let invalid = shape_problems(&raw_emr);
        let ready: Vec<&'static str> = ALL_CREDENTIAL_VARS
            .iter()
            .copied()
            .filter(|k| !missing.contains(k) && !invalid.iter().any(|(bad, _)| bad == k))
            .collect();
        let writeback_ready = missing.is_empty() && invalid.is_empty();

        let mut notices = Vec::new();
        if writeback_ready {
            notices.push(
                "五项凭据齐备：写回院内 EMR 与缺件通知均可用（本次自检未发起任何网络请求）".into(),
            );
        } else {
            notices.push(
                "本次自检未发起任何网络请求；缺项会导致对应链路返回错误，不会静默降级为本地假成功"
                    .into(),
            );
        }
        Self {
            ready,
            missing,
            invalid,
            notices,
            writeback_ready,
        }
    }

    /// 人读的一行式摘要。**不含任何凭据值**——这份报告要进窗口、
    /// 进日志、进工单，凭据本体不该跟着走。
    pub fn render(&self) -> String {
        let mut s = String::new();
        for k in &self.ready {
            s.push_str(&format!("  [就绪] {k}\n"));
        }
        for k in &self.missing {
            s.push_str(&format!("  [缺失] {k}（未设置或为空白）\n"));
        }
        for (k, why) in &self.invalid {
            s.push_str(&format!("  [形态错] {k}：{why}\n"));
        }
        for n in &self.notices {
            s.push_str(&format!("  [说明] {n}\n"));
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        ENV_BASE_URL, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_TENANT_ID,
    };

    fn full() -> Vec<(&'static str, String)> {
        vec![
            (ENV_BASE_URL, "https://emr.example-hospital.local/fhir/r4".into()),
            (ENV_CLIENT_ID, "psychdoc-client".into()),
            (ENV_CLIENT_SECRET, "s3cret-value".into()),
            (ENV_TENANT_ID, "ward-a".into()),
            (
                ENV_WECOM,
                "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123".into(),
            ),
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
    fn all_five_present_load_in_one_pass() {
        let cfg = StartupConfig::from_source(source(&full()), Path::new("/app")).unwrap();
        assert_eq!(cfg.emr.base_url, "https://emr.example-hospital.local/fhir/r4");
        assert!(cfg.wecom.webhook_url.contains("key=abc-123"));
        assert_eq!(cfg.log_level, LogLevel::Info);
    }

    #[test]
    fn missing_one_lists_it_and_the_four_ready_ones_are_not_errors() {
        let mut e = full();
        e.retain(|(k, _)| *k != ENV_TENANT_ID);
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        let text = err.to_string();
        assert_eq!(err.code(), "E_CFG_MISSING");
        assert!(text.contains(ENV_TENANT_ID) && text.contains("缺少 1 项"), "{text}");
    }

    #[test]
    fn missing_three_including_the_webhook_are_listed_at_once() {
        let mut e = full();
        e.retain(|(k, _)| *k != ENV_TENANT_ID && *k != ENV_CLIENT_ID && *k != ENV_WECOM);
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        let text = err.to_string();
        assert!(text.contains("缺少 3 项"), "{text}");
        for key in [ENV_CLIENT_ID, ENV_TENANT_ID, ENV_WECOM] {
            assert!(text.contains(key), "{key} 应一次列全：{text}");
        }
    }

    #[test]
    fn a_blank_webhook_counts_as_missing_not_as_a_shape_error() {
        let mut e = full();
        for (k, v) in e.iter_mut() {
            if *k == ENV_WECOM {
                *v = "   ".into();
            }
        }
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        assert_eq!(err.code(), "E_CFG_MISSING", "空白算缺失：{}", err);
    }

    #[test]
    fn a_malformed_url_is_cfg_invalid_and_not_cfg_missing() {
        let mut e = full();
        for (k, v) in e.iter_mut() {
            if *k == ENV_BASE_URL {
                *v = "emr.example-hospital.local".into();
            }
        }
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        assert_eq!(err.code(), "E_CFG_INVALID", "{}", err);
        assert!(err.to_string().contains(ENV_BASE_URL));
    }

    #[test]
    fn several_malformed_values_are_all_listed_in_one_go() {
        let mut e = full();
        for (k, v) in e.iter_mut() {
            match *k {
                ENV_BASE_URL => *v = "emr.example.local".into(),
                ENV_TENANT_ID => *v = "ward a".into(),
                ENV_CLIENT_ID => *v = "ab".into(),
                _ => {}
            }
        }
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        let text = err.to_string();
        for key in [ENV_BASE_URL, ENV_TENANT_ID, ENV_CLIENT_ID] {
            assert!(text.contains(key), "{key} 应一次列全：{text}");
        }
    }

    #[test]
    fn a_webhook_without_the_key_parameter_is_a_shape_error() {
        let mut e = full();
        for (k, v) in e.iter_mut() {
            if *k == ENV_WECOM {
                *v = "https://qyapi.weixin.qq.com/cgi-bin/webhook/send".into();
            }
        }
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        assert_eq!(err.code(), "E_CFG_INVALID", "{}", err);
        assert!(err.to_string().contains(ENV_WECOM));
    }

    /// 五项凭据 + 若干非凭据项。
    fn with(extra: Vec<(&'static str, String)>) -> Vec<(&'static str, String)> {
        let mut e = full();
        e.extend(extra);
        e
    }

    #[test]
    fn relative_paths_resolve_against_the_given_base() {
        let e = with(vec![
            (ENV_RULESET, "rulesets/hospital.yaml".to_string()),
            (ENV_DATA_DIR, "var/psychdoc".to_string()),
        ]);
        let cfg = StartupConfig::from_source(source(&e), Path::new("/opt/psychdoc")).unwrap();
        assert_eq!(cfg.ruleset, PathBuf::from("/opt/psychdoc/rulesets/hospital.yaml"));
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/opt/psychdoc/var/psychdoc")));
    }

    #[test]
    fn an_absolute_path_is_left_alone_and_no_ruleset_means_the_shipped_one() {
        let cfg = StartupConfig::from_source(source(&full()), Path::new("/opt/psychdoc")).unwrap();
        assert_eq!(
            cfg.ruleset,
            PathBuf::from("/opt/psychdoc/rulesets/hospital.yaml"),
            "未设规则集时用交付的那套，不是空规则集"
        );
        assert!(cfg.data_dir.is_none(), "未设数据目录时交由平台规则解析");

        let e = with(vec![(ENV_DATA_DIR, "/var/lib/psychdoc".to_string())]);
        let cfg = StartupConfig::from_source(source(&e), Path::new("/opt/psychdoc")).unwrap();
        assert_eq!(cfg.data_dir, Some(PathBuf::from("/var/lib/psychdoc")));
    }

    #[test]
    fn log_level_is_read_and_an_unknown_value_is_refused_rather_than_silently_defaulted() {
        let e = with(vec![(ENV_LOG_LEVEL, "debug".to_string())]);
        let cfg = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap();
        assert_eq!(cfg.log_level, LogLevel::Debug);

        let e = with(vec![(ENV_LOG_LEVEL, "verbose".to_string())]);
        let err = StartupConfig::from_source(source(&e), Path::new("/app")).unwrap_err();
        assert_eq!(err.code(), "E_CFG_INVALID", "{}", err);
        assert!(err.to_string().contains("verbose"), "{}", err);
    }

    #[test]
    fn the_report_separates_ready_missing_and_invalid_without_failing() {
        let mut e = full();
        e.retain(|(k, _)| *k != ENV_TENANT_ID && *k != ENV_WECOM);
        for (k, v) in e.iter_mut() {
            if *k == ENV_CLIENT_ID {
                *v = "ab".into();
            }
        }
        let r = StartupReport::check(source(&e));
        assert!(!r.writeback_ready);
        assert_eq!(r.missing, vec![ENV_TENANT_ID, ENV_WECOM]);
        assert_eq!(r.invalid.len(), 1);
        assert_eq!(r.invalid[0].0, ENV_CLIENT_ID);
        assert_eq!(r.ready.len(), 2, "余下两项已就绪");
    }

    #[test]
    fn the_report_never_prints_a_credential_value() {
        let r = StartupReport::check(source(&full()));
        let text = r.render();
        assert!(text.contains("写回院内 EMR"), "{text}");
        for secret in ["s3cret-value", "psychdoc-client", "abc-123"] {
            assert!(!text.contains(secret), "报告里漏出了凭据值：{text}");
        }
    }
}
