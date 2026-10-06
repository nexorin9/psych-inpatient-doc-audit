//! 退出码契约：`0 / 1 / 2 / 3` 四种情形的唯一裁决点。
//!
//! 无 GUI 通道存在的理由是**接进别人的自动化**：信息科在服务器上跑无人值守
//! 批处理，CI 跑规则回归，两边都只看一个数字。于是「退出码代表什么」不能是
//! 各子命令各写各的，必须在这里钉死一份、并且可被测试逐条钉住。
//!
//! | 码 | 含义 | 判据 | CI 该怎么做 |
//! |----|------|------|-------------|
//! | 0 | 成功 | 审查零阻断 / 视图出得来 / 干跑构造成功 | 通过 |
//! | 1 | 审查不通过 | 存在 `blocking` 诊断 | 业务问题：把清单发给科室，**不要**改 CI |
//! | 2 | 配置或用法问题 | 缺环境变量、规则集非法、参数写错 | 环境问题：修配置后重跑 |
//! | 3 | 外部依赖不可达 | 端点连不上 / 5xx / 事务被拒 | 基础设施问题：退避后重跑 |
//!
//! **1 与 2/3 必须分开**：审查不通过是业务事实（护士长要整改），
//! 配错了是运维事实（谁改了规则集 YAML）。混成一个码，CI 就会把
//! 「规则集被改坏」当成「病历有问题」重跑一百遍，或者反过来把
//! 真实缺件当成配置抖动压过去。

use std::fmt;

/// 退出码：成功。
pub const EXIT_OK: u8 = 0;
/// 退出码：审查不通过（存在阻断项）。
pub const EXIT_FINDINGS: u8 = 1;
/// 退出码：配置缺失 / 形态非法 / 用法错误。
pub const EXIT_CONFIG: u8 = 2;
/// 退出码：外部依赖不可达。
pub const EXIT_UNREACHABLE: u8 = 3;

/// CLI 错误：**必须带稳定错误码**。
///
/// 早先的写法是 `Result<_, String>`，错误只有一个字符串。那样写不出上面的
/// 契约表——退出码只能「一律 2」，于是「端点挂了」和「少填了环境变量」
/// 在 CI 看来完全一样，运维分不清该找网络组还是找改配置的人。
/// 带上码之后，退出码由 [`exit_for_code`] 单点推导，不靠各子命令自觉。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    /// 稳定错误码，如 `E_CFG_MISSING`。进日志、进 `--json` 正文、进 CI 判读。
    pub code: String,
    /// 人读的中文说明。已含该错误码的完整语境，可直接贴进工单。
    pub message: String,
    /// 该子命令**自己已经把这段错误写进 `--json` 正文**了。
    ///
    /// `--json` 的契约是 stdout 上**至多一段** JSON（见 `main.rs`）。
    /// `preflight` 这类要逐项汇报的子命令会先把完整报告吐出来再退 2，
    /// 此时 `main` 的兜底分支若再吐一段，`jq` 会直接解析失败——
    /// 而这恰恰是它最该伺候好的那条命令（CI 里 `jq '.missing | length'`）。
    pub json_reported: bool,
}

impl CliError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            json_reported: false,
        }
    }

    /// 标记「正文已由子命令自己写过了」，让 `main` 不再补一段。
    pub fn already_reported(mut self) -> Self {
        self.json_reported = true;
        self
    }

    /// 用法 / 参数错误。归 2。
    pub fn usage(message: impl Into<String>) -> Self {
        Self::new("E_USAGE", message)
    }

    /// 从 `Display` 文本里**摘出**稳定码，摘不到才落到 `E_USAGE`。
    ///
    /// 各 crate 的错误类型都已经把码写进了 `Display` 首段
    /// （`E_FHIR_UNREACHABLE: …` / `E_RULESET_INVALID: …` / `E_IO: …`），
    /// 因此这里不必让每个调用点手工传一遍码——传错码比不传码更糟：
    /// 它会把网络故障报成配置问题，让运维去查环境变量。
    pub fn from_display(err: impl fmt::Display) -> Self {
        let text = err.to_string();
        match leading_code(&text) {
            Some(code) => Self::new(code, text),
            None => Self::usage(text),
        }
    }
}

/// 取 `Display` 首段的稳定码（形如 `E_FHIR_UNREACHABLE: 后续中文`）。
///
/// 只认下划线大写形态（`E_` + `[A-Z0-9_]+`），且必须紧跟 `:` 或空格收尾，
/// 免得把正文里恰好出现的英文缩写当成码。
pub fn leading_code(text: &str) -> Option<String> {
    let rest = text.trim_start().strip_prefix("E_")?;
    let end = rest
        .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
        .unwrap_or(rest.len());
    if end == 0 {
        return None;
    }
    let code = format!("E_{}", &rest[..end]);
    match rest[end..].chars().next() {
        None => Some(code),
        Some(c) if c == ':' || c.is_whitespace() => Some(code),
        Some(_) => None,
    }
}

impl From<String> for CliError {
    /// 没有码的字符串一律当用法错误。
    ///
    /// 有意保守：给不出稳定码的错误归 2（配置侧）而不是 3（网络侧）。
    /// 归错方向的后果不对称——把网络故障报成配置问题，运维会去反复检查
    /// 环境变量，永远查不出问题；反过来只是多等一轮重试。
    fn from(message: String) -> Self {
        Self::usage(message)
    }
}

/// 错误码 → 退出码。**退出码的唯一来源**。
pub fn exit_for_code(code: &str) -> u8 {
    match code {
        // 配置侧：凭据缺失 / 形态非法 / 规则集与数据有问题 / 用法写错。
        "E_CFG_MISSING" | "E_CFG_INVALID" | "E_RULESET_INVALID" | "E_USAGE" | "E_ARG"
        | "E_BUNDLE_SHAPE" | "E_DATA_DIR" | "E_SCHEMA_EMPTY" | "E_EXPORT_LEAK"
        | "E_DRAFT_UNKNOWN" | "E_DRAFT_STATE" | "E_ENTRY_UNKNOWN" | "E_NOTHING_SELECTED"
        | "E_STATE_JUMP" | "E_STATE_UNKNOWN" | "E_RUN_UNKNOWN" | "E_DEP_CYCLE"
        | "E_DEP_INVARIANT" | "E_ENCOUNTER_UNKNOWN" | "E_DIR_PAIRING" | "E_REGRESS_*"
        // 空勾选归配置侧而不是网络侧：没人勾条目是**用法问题**，
        // 报成「端点不可达」会让 CI 去重试一件重试一万次也不会成功的事。
        | "E_FHIR_TXN_EMPTY" => EXIT_CONFIG,

        // 外部依赖侧：连不上 / 被限流 / 服务端错 / 事务被拒 / 通知发不出去。
        "E_FHIR_UNREACHABLE" | "E_FHIR_STATUS" | "E_FHIR_SHAPE" | "E_FHIR_FIELD"
        | "E_FHIR_TXN_REJECTED" | "E_FHIR_PAGING"
        | "E_FHIR_RETRIES_EXHAUSTED" | "E_WECOM_STATUS" | "E_WECOM_REJECTED"
        | "E_WECOM_SHAPE" | "E_FHIR_READ" | "E_FHIR_BIND" => EXIT_UNREACHABLE,

        // 其余按配置侧处理：宁可让运维多看一眼，也不要把未知错误
        // 报成网络抖动从而触发无意义的重试风暴。
        _ => EXIT_CONFIG,
    }
}

/// 退出码 → 一句中文，供 `--json` 正文与 `docs/cli.md` 共用同一份措辞。
pub fn describe(code: u8) -> &'static str {
    match code {
        EXIT_OK => "成功：审查零阻断 / 视图出得来 / 干跑构造成功",
        EXIT_FINDINGS => "审查不通过：存在阻断项（业务问题，交给科室整改）",
        EXIT_CONFIG => "配置或用法问题：缺环境变量、规则集非法或参数写错（修配置后重跑）",
        EXIT_UNREACHABLE => "外部依赖不可达：端点连不上或返回错误（退避后重跑）",
        _ => "未知退出码",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_four_exit_codes_are_distinct_and_stable() {
        let codes = [EXIT_OK, EXIT_FINDINGS, EXIT_CONFIG, EXIT_UNREACHABLE];
        assert_eq!(codes, [0, 1, 2, 3]);
        // 每个码都有一句说明，且互不相同——CI 的判读文档直接用它。
        let mut seen = std::collections::BTreeSet::new();
        for c in codes {
            assert!(seen.insert(describe(c)), "退出码 {c} 的说明重复");
        }
    }

    #[test]
    fn config_side_and_network_side_codes_never_collide() {
        assert_eq!(exit_for_code("E_CFG_MISSING"), EXIT_CONFIG);
        assert_eq!(exit_for_code("E_CFG_INVALID"), EXIT_CONFIG);
        assert_eq!(exit_for_code("E_RULESET_INVALID"), EXIT_CONFIG);
        assert_eq!(exit_for_code("E_USAGE"), EXIT_CONFIG);
        assert_eq!(exit_for_code("E_FHIR_UNREACHABLE"), EXIT_UNREACHABLE);
        assert_eq!(exit_for_code("E_FHIR_STATUS"), EXIT_UNREACHABLE);
        assert_eq!(exit_for_code("E_FHIR_TXN_REJECTED"), EXIT_UNREACHABLE);
        // 空勾选是用法问题：归到网络侧会让 CI 陷入无意义的重试。
        assert_eq!(exit_for_code("E_FHIR_TXN_EMPTY"), EXIT_CONFIG);
    }

    #[test]
    fn an_unknown_code_falls_back_to_the_config_side() {
        // 归错方向不对称：把未知故障报成网络问题会引发重试风暴，
        // 报成配置问题最多让运维多看一眼。
        assert_eq!(exit_for_code("E_SOMETHING_NEW"), EXIT_CONFIG);
    }

    #[test]
    fn a_display_string_yields_its_leading_code() {
        assert_eq!(
            leading_code("E_FHIR_UNREACHABLE: 无法连接 https://… — timed out").as_deref(),
            Some("E_FHIR_UNREACHABLE")
        );
        assert_eq!(
            leading_code("E_RULESET_INVALID: 规则集 YAML 解析失败 — x").as_deref(),
            Some("E_RULESET_INVALID")
        );
        // 整个字符串就是码（无冒号无正文）也要认。
        assert_eq!(leading_code("E_IO").as_deref(), Some("E_IO"));
    }

    #[test]
    fn a_display_string_without_a_code_falls_back_to_usage() {
        assert_eq!(leading_code("读不到规则集 x.yaml"), None);
        assert_eq!(leading_code(""), None);
        // 小写紧跟不算码：`E_CFGmissing` 不是稳定码。
        assert_eq!(leading_code("E_CFGmissing: …"), None);
        assert_eq!(
            CliError::from_display("读不到规则集 x.yaml").code,
            "E_USAGE"
        );
        assert_eq!(
            CliError::from_display("E_CFG_MISSING: 缺少 4 项").code,
            "E_CFG_MISSING"
        );
    }
}
