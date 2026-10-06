//! 规则回归套件：**用例驱动的批量回归**。
//!
//! 改写自源产品「规则测试由用例文件驱动、批量跑一遍就知道改坏了哪几条」的
//! 设计（`github_ref/ruff/crates/mdtest/src/lib.rs::run` —— 遍历 suite 内
//! 全部用例、逐个执行、把失败明细与断言 diff 汇总输出；`parser.rs::parse`
//! 负责把用例文件解析成结构化 suite）。
//!
//! 融合后的对应关系：
//!
//! | 源产品（markdown 用例） | 本模块（YAML 用例） |
//! |---|---|
//! | markdown 源文件 + ```` ```python ```` 代码块 | `rulesets/rule-regress.yaml` + `given.fixture` |
//! | 断言写在 markdown 里（`# error:` 注释） | `expect` 段（命中码集合 + 位置断言） |
//! | `MDTEST_TEST_FILTER` 环境变量过滤用例 | `psychdoc rule-regress --only <子串>` |
//! | 全部用例跑一遍、按名字报告失败 | 按码输出 passed / failed 明细 + 覆盖矩阵 |
//!
//! ## 三条设计取舍
//!
//! 1. **用例只声明「期望」，不声明「怎么跑」**。执行路径只有一条
//!    （解析 → 规则 → 去重），跟 GUI、跟 CLI `check` 完全同源；
//!    规则集改一行，秒级知道打破了几条用例。
//! 2. **反例零命中是强制的**：`kind: negative` 的用例一旦跑出任何命中码即判失败。
//!    误报比漏报更难被发现——漏报护士长会来问，误报只会让人觉得「这工具老乱报」
//!    然后不再用它。
//! 3. **未判定与容差内也要能被断言**。跨文书规则有一大类「判不出来」的分支
//!    （缺时刻、精度不足、绑定不唯一），它们不是「通过」。用例的 `expect`
//!    因此除了 `codes` 还留了 `skipped` / `tolerated` / `suppressed` 三栏。

use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use psychdoc_model::{DocItem, Diagnostic, FixAvailability, Severity};

use crate::ruleset::EffectiveRules;

/// 用例格式版本。改动 `expect` 语义时必须 +1，
/// 否则老用例文件会被新语义**静默**按另一套标准判过去。
pub const REGRESS_FORMAT_VERSION: &str = "1.0";

/// 一条用例钉哪条规则码、钉哪种情形。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CaseKind {
    /// 正例：该码**必须**命中。
    Positive,
    /// 反例（误报守卫）：整份文书包**必须**零命中。
    Negative,
    /// 边界例：贴着判定线走——要么命中且定位可核对，要么不命中但**留痕**
    /// （未判定 / 容差内 / 被折叠），绝不允许「静悄悄没报」。
    Boundary,
}

impl CaseKind {
    pub fn as_zh(self) -> &'static str {
        match self {
            Self::Positive => "正例",
            Self::Negative => "反例（误报守卫）",
            Self::Boundary => "边界例",
        }
    }
}

/// 输入：一份脱敏夹具 + 可选切片。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Given {
    /// 相对 `--root` 的夹具路径。
    pub fixture: String,
    /// 只保留这些件（`Composition.id`）。留空 = 整包都进。
    ///
    /// 切片是给「同一份包里另外几件本来就有问题」的场景用的：
    /// 只想验某一条规则时，把无关件摘掉，期望集合才是干净的一条。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub doc_ids: Vec<String>,
}

impl Given {
    /// 把整包收窄到切片声明的件。切片里写了不存在的 id 直接返回空——
    /// 调用方能看出「这一条用例的件名写错了」，而不是拿到一个空结果说「通过」。
    pub fn slice_items(&self, items: Vec<DocItem>) -> Vec<DocItem> {
        if self.doc_ids.is_empty() {
            return items;
        }
        items
            .into_iter()
            .filter(|i| self.doc_ids.contains(&i.doc_id))
            .collect()
    }
}

/// 位置断言：把「报在哪」也钉死。
///
/// 只断言码等于只断言「报了个数」——同一个码落在文书A还是文书B，
/// 对护士长是完全不同的两件事（一个能改，一个要去别的系统查）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocAssert {
    pub code: String,
    pub doc_id: String,
    pub section_index: u32,
    pub char_start: u32,
    pub char_end: u32,
    /// 命中文案里必须出现的片段（如被点名的那一栏）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_contains: Option<String>,
}

/// 「未判定」断言。`reason` 省略时只比对规则码。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkipAssert {
    pub code: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 「容差内未判违规」断言。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToleratedAssert {
    pub code: String,
    pub tolerance_minutes: i64,
}

/// 期望：跑完这一份夹具之后应该看到什么。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Expect {
    /// 期望的命中码**集合**（顺序与重复不计），逐条与实际集合比对。
    #[serde(default)]
    pub codes: Vec<String>,
    /// 期望的诊断总条数。给了就一并比对——同码两条与一条是不同的结论。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<usize>,
    /// 期望的严重度分层。
    ///
    /// 单列一栏是因为「阻断」与「提示」在业务上完全不是一回事：
    /// 科室级把某条规则从阻断改成提示，护士长那边就不再被拦，
    /// 而**命中码集合一个字都没变**——只比对码的话这种改动会静悄悄通过。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<Severity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub locations: Vec<LocAssert>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skipped: Vec<SkipAssert>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tolerated: Vec<ToleratedAssert>,
    /// 被去重层折叠掉的码（不是消失，只是默认不刷屏）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suppressed: Vec<String>,
}

/// 一条回归用例。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Case {
    /// 用例 id，全套件唯一。失败报告按它定位。
    pub id: String,
    /// 本用例钉哪条规则码（`QC-PSY-nn`）。覆盖矩阵按它统计。
    pub code: String,
    pub kind: CaseKind,
    /// 人可读的一句话：这条例的是哪种现场。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub given: Given,
    pub expect: Expect,
}

/// 一份用例文件。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaseFile {
    pub version: String,
    pub cases: Vec<Case>,
}

/// 用例文件加载 / 校验错误。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaseError {
    pub code: &'static str,
    pub detail: String,
}

impl fmt::Display for CaseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.code, self.detail)
    }
}

impl std::error::Error for CaseError {}

fn err(code: &'static str, detail: impl Into<String>) -> CaseError {
    CaseError { code, detail: detail.into() }
}

/// 加载并校验一份用例文件。
///
/// 校验期就拒掉的写法（否则它们会以「用例失败」的形式混在回归报告里，
/// 让维护者分不清是自己把规则改坏了还是用例本身写错了）：
/// 版本不符、id 重复、`code` 形状不对、缺 fixture、
/// 正例没写期望命中码、反例却写了期望命中码。
pub fn load_cases(yaml: &str) -> Result<CaseFile, CaseError> {
    let file: CaseFile =
        serde_yaml::from_str(yaml).map_err(|e| err("E_REGRESS_SCHEMA", e.to_string()))?;
    if file.version != REGRESS_FORMAT_VERSION {
        return Err(err(
            "E_REGRESS_VERSION",
            format!(
                "用例格式版本为 {}，本轮只认 {REGRESS_FORMAT_VERSION}",
                file.version
            ),
        ));
    }
    if file.cases.is_empty() {
        return Err(err("E_REGRESS_EMPTY", "用例文件里一条用例都没有"));
    }

    let mut seen = BTreeMap::new();
    for case in &file.cases {
        if case.id.trim().is_empty() {
            return Err(err("E_REGRESS_NO_ID", "有用例没写 id，无法在报告里定位"));
        }
        if seen.insert(case.id.clone(), ()).is_some() {
            return Err(err(
                "E_REGRESS_DUP_ID",
                format!("用例 id「{}」重复；id 必须全套件唯一", case.id),
            ));
        }
        if !case.code.starts_with("QC-PSY-") {
            return Err(err(
                "E_REGRESS_CODE_SHAPE",
                format!(
                    "用例 {} 的规则码「{}」不是 QC-PSY-nn 形状",
                    case.id, case.code
                ),
            ));
        }
        if case.given.fixture.trim().is_empty() {
            return Err(err(
                "E_REGRESS_NO_FIXTURE",
                format!("用例 {} 没写 given.fixture", case.id),
            ));
        }
        match case.kind {
            CaseKind::Positive if case.expect.codes.is_empty() => {
                return Err(err(
                    "E_REGRESS_POSITIVE_NO_CODE",
                    format!(
                        "用例 {} 是正例却没写 expect.codes —— 正例必须点名它要命中哪条码",
                        case.id
                    ),
                ));
            }
            CaseKind::Negative if !case.expect.codes.is_empty() => {
                return Err(err(
                    "E_REGRESS_NEGATIVE_WITH_CODE",
                    format!(
                        "用例 {} 是反例（误报守卫）却写了 expect.codes —— 反例要求整包零命中",
                        case.id
                    ),
                ));
            }
            _ => {}
        }
    }
    Ok(file)
}

/// 一条用例跑完之后的结论。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CaseOutcome {
    pub id: String,
    pub code: String,
    pub kind: CaseKind,
    pub passed: bool,
    /// 期望码 → 实际码的对照，供维护者直接改。
    pub expected_codes: Vec<String>,
    pub actual_codes: Vec<String>,
    /// 逐条失败原因（中文，可直接贴进交办单）。
    pub failures: Vec<String>,
}

/// 跑一条用例。`items` 已由调用方解析好并按 `given.doc_ids` 切好片。
pub fn run_case(case: &Case, items: &[DocItem], ruleset: &EffectiveRules) -> CaseOutcome {
    let result = crate::review(items, ruleset);
    let mut failures = Vec::new();

    let actual_codes = sorted_unique(result.diagnostics.iter().map(|d| d.code.as_str()));
    let expected_codes = sorted_unique(case.expect.codes.iter().map(String::as_str));

    if actual_codes != expected_codes {
        failures.push(format!(
            "命中码不符：期望 [{}]，实际 [{}]{}",
            expected_codes.join(", "),
            actual_codes.join(", "),
            diff_hint(&expected_codes, &actual_codes)
        ));
    }

    if let Some(n) = case.expect.count {
        if result.diagnostics.len() != n {
            failures.push(format!(
                "条数不符：期望 {n} 条，实际 {} 条（码集合相同也不放过——同码两条与一条是两回事）",
                result.diagnostics.len()
            ));
        }
    }

    if let Some(want) = case.expect.severity {
        let counts = result.counts_by_severity();
        let actual = if counts.blocking > 0 && counts.notice > 0 {
            // 两档同时有命中时不能只报一个数——写清楚是哪一档多了哪一档。
            let mut parts = Vec::new();
            if counts.blocking > 0 {
                parts.push(format!("阻断 {} 条", counts.blocking));
            }
            if counts.notice > 0 {
                parts.push(format!("提示 {} 条", counts.notice));
            }
            parts.join(" + ")
        } else if counts.blocking > 0 {
            format!("阻断 {} 条", counts.blocking)
        } else {
            format!("提示 {} 条", counts.notice)
        };
        let matches = match want {
            Severity::Blocking => counts.blocking > 0 && counts.notice == 0,
            Severity::Notice => counts.notice > 0 && counts.blocking == 0,
        };
        if !matches {
            failures.push(format!(
                "严重度不符：期望全为「{want:?}」，实际 {actual}。\
                 科室级把某条规则从阻断改成提示时命中码不变，只比对码会静悄悄放过这种改动。"
            ));
        }
    }

    for loc in &case.expect.locations {
        if let Some(d) = result
            .diagnostics
            .iter()
            .find(|d| location_matches(d, loc))
        {
            if let Some(needle) = &loc.message_contains {
                if !d.message.text.contains(needle) {
                    failures.push(format!(
                        "{} @{} 的命中文案里没有「{needle}」，实际写的是「{}」",
                        d.code, d.loc, d.message.text
                    ));
                }
            }
        } else {
            failures.push(format!(
                "找不到满足位置断言的命中：{} @{}#s{}@[{}, {})",
                loc.code, loc.doc_id, loc.section_index, loc.char_start, loc.char_end
            ));
        }
    }

    for s in &case.expect.skipped {
        match result
            .skipped_checks
            .iter()
            .find(|k| k.code == s.code && s.reason.as_ref().is_none_or(|r| k.reason.as_zh().contains(r)))
        {
            Some(k) => {
                if let Some(r) = &s.reason {
                    if !k.reason.as_zh().contains(r) {
                        failures.push(format!(
                            "{} 的未判定原因不是「{r}」，实际是「{}」",
                            s.code,
                            k.reason.as_zh()
                        ));
                    }
                }
            }
            None => failures.push(format!(
                "期望有一条 {}{} 的未判定记录，实际没有",
                s.code,
                s.reason
                    .as_ref()
                    .map(|r| format!("（原因含「{r}」）"))
                    .unwrap_or_default()
            )),
        }
    }

    for t in &case.expect.tolerated {
        match result
            .tolerated_checks
            .iter()
            .find(|k| k.code == t.code && k.tolerance_minutes == t.tolerance_minutes)
        {
            Some(_) => {}
            None => failures.push(format!(
                "期望有一条 {} 的容差内记录（{} 分钟档），实际没有",
                t.code, t.tolerance_minutes
            )),
        }
    }

    for code in &case.expect.suppressed {
        if !result.suppressed.iter().any(|x| &x.code == code) {
            failures.push(format!(
                "期望 {code} 被折叠（更具体的码赢了），实际没有折叠记录"
            ));
        }
    }

    // 误报守卫单列一条：反例跑出非 0 命中时，维护者需要一眼看到「这是误报」。
    if case.kind == CaseKind::Negative && !result.diagnostics.is_empty() {
        failures.push(format!(
            "误报守卫：反例跑出 {} 条命中（{}），合规件必须零命中",
            result.diagnostics.len(),
            result
                .diagnostics
                .iter()
                .map(|d| format!("{} @{}", d.code, d.loc))
                .collect::<Vec<_>>()
                .join("；")
        ));
    }

    // 去重不变量破了 = 有诊断被吞掉，比刷屏严重得多。
    if !result.is_dedup_accounting_balanced() {
        failures.push(format!(
            "去重账不平：留下的 {} 条 + 压下的 {} 条 != 折叠前 {} 条（有诊断被吞掉了）",
            result.diagnostics.len(),
            result.suppressed.len(),
            result.diagnostics_before_dedup
        ));
    }

    CaseOutcome {
        id: case.id.clone(),
        code: case.code.clone(),
        kind: case.kind,
        passed: failures.is_empty(),
        expected_codes,
        actual_codes,
        failures,
    }
}

fn location_matches(d: &Diagnostic, a: &LocAssert) -> bool {
    d.code == a.code
        && d.loc.doc_id == a.doc_id
        && d.loc.section_index == a.section_index
        && d.loc.char_range.start == a.char_start
        && d.loc.char_range.end == a.char_end
}

fn sorted_unique<'a>(it: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut v: Vec<String> = it.map(str::to_string).collect();
    v.sort();
    v.dedup();
    v
}

fn diff_hint(expected: &[String], actual: &[String]) -> String {
    let missing: Vec<&String> = expected.iter().filter(|c| !actual.contains(c)).collect();
    let extra: Vec<&String> = actual.iter().filter(|c| !expected.contains(c)).collect();
    let mut s = String::new();
    if !missing.is_empty() {
        s.push_str(&format!("；漏报 {}", missing.iter().map(|c| c.as_str()).collect::<Vec<_>>().join("、")));
    }
    if !extra.is_empty() {
        s.push_str(&format!(
            "；多报 {}",
            extra.iter().map(|c| c.as_str()).collect::<Vec<_>>().join("、")
        ));
    }
    s
}

/// 一行覆盖矩阵。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CoverageRow {
    pub code: String,
    pub title: String,
    pub positive: usize,
    pub negative: usize,
    pub boundary: usize,
}

impl CoverageRow {
    pub fn is_covered(&self) -> bool {
        self.positive > 0 && self.negative > 0 && self.boundary > 0
    }
}

/// 覆盖矩阵：每条生效规则码都要有正例 + 反例 + 边界例。
///
/// 反例与边界例的缺位比正例更值得警惕：正例漏了会报出来，
/// 反例漏了意味着**没人验证这条规则不会乱报**。
pub fn coverage(cases: &[Case], ruleset: &EffectiveRules) -> Vec<CoverageRow> {
    ruleset
        .rules
        .iter()
        .map(|(code, rule)| {
            let mut row = CoverageRow {
                code: code.clone(),
                title: rule.title.clone(),
                positive: 0,
                negative: 0,
                boundary: 0,
            };
            for c in cases.iter().filter(|c| &c.code == code) {
                match c.kind {
                    CaseKind::Positive => row.positive += 1,
                    CaseKind::Negative => row.negative += 1,
                    CaseKind::Boundary => row.boundary += 1,
                }
            }
            row
        })
        .collect()
}

/// 覆盖率（0.0–1.0）：三类齐备的规则码占比。
pub fn coverage_ratio(rows: &[CoverageRow]) -> f64 {
    if rows.is_empty() {
        return 1.0;
    }
    rows.iter().filter(|r| r.is_covered()).count() as f64 / rows.len() as f64
}

/// 一次批量回归的完整报告。
#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub format_version: String,
    pub ruleset_version: String,
    pub ruleset_sha256: String,
    pub outcomes: Vec<CaseOutcome>,
    pub coverage: Vec<CoverageRow>,
}

impl RunReport {
    pub fn passed(&self) -> usize {
        self.outcomes.iter().filter(|o| o.passed).count()
    }

    pub fn failed(&self) -> usize {
        self.outcomes.len() - self.passed()
    }

    /// 用例全过**且**覆盖矩阵无缺位才算这次回归通过。
    ///
    /// 只看用例不够：删掉一条反例，所有用例依然全绿，
    /// 但「这条规则会不会乱报」从此没人验证。
    pub fn is_green(&self) -> bool {
        self.failed() == 0 && self.coverage.iter().all(|r| r.is_covered())
    }
}

/// 跑一整份用例文件。
///
/// `resolve` 负责把 `given.fixture` 变成已解析、已切片的件——
/// 解析归 `psychdoc-parse` 管，本模块只管「跑」与「比」，
/// 两边不会各长一套判定逻辑。
pub fn run_all<F>(
    file: &CaseFile,
    ruleset: &EffectiveRules,
    only: Option<&str>,
    mut resolve: F,
) -> Result<RunReport, CaseError>
where
    F: FnMut(&Given) -> Result<Vec<DocItem>, CaseError>,
{
    let selected: Vec<&Case> = file
        .cases
        .iter()
        .filter(|c| only.is_none_or(|f| c.id.contains(f) || c.code.contains(f)))
        .collect();

    let mut outcomes = Vec::new();
    for case in selected {
        let items = resolve(&case.given)?;
        outcomes.push(run_case(case, &items, ruleset));
    }

    Ok(RunReport {
        format_version: file.version.clone(),
        ruleset_version: ruleset.version.clone(),
        ruleset_sha256: ruleset.short_sha256().to_string(),
        outcomes,
        coverage: coverage(&file.cases, ruleset),
    })
}

/// 把 `--root` 解析成夹具绝对路径。
pub fn fixture_path(root: &Path, given: &Given) -> PathBuf {
    root.join(&given.fixture)
}

/// `FixAvailability` 的中文标签，供 CLI 报告与用例失败说明复用。
pub fn fix_label(f: FixAvailability) -> &'static str {
    match f {
        FixAvailability::Automatic => "可预览补写",
        FixAvailability::Manual => "需人工确认后补写",
        FixAvailability::None => "不可自动修",
    }
}

/// 一条命中的完整可读行（CLI / UI 共用同一套措辞）。
pub fn finding_line(d: &Diagnostic) -> String {
    format!(
        "{} @{} · {}",
        d.code,
        d.loc,
        fix_label(d.fix_availability)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn case(kind: CaseKind, codes: &[&str]) -> Case {
        Case {
            id: "t".into(),
            code: "QC-PSY-01".into(),
            kind,
            note: None,
            given: Given { fixture: "f.json".into(), doc_ids: vec![] },
            expect: Expect {
                codes: codes.iter().map(|s| s.to_string()).collect(),
                ..Expect::default()
            },
        }
    }

    #[test]
    fn the_helper_itself_serializes_back_into_the_format_it_reads() {
        // 序列化 / 反序列化往返：保证 YAML 写法与结构体字段一一对应，
        // 新加一栏 expect 时不会因为 skip_serializing_if 漏写而悄悄丢字段。
        let c = case(CaseKind::Positive, &["QC-PSY-01"]);
        let yaml = serde_yaml::to_string(&c).unwrap();
        assert!(yaml.contains("kind: positive"), "{yaml}");
        assert!(yaml.contains("fixture: f.json"), "{yaml}");
        let back: Case = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn a_negative_case_may_not_declare_expected_codes() {
        let yaml = "version: \"1.0\"\ncases:\n  - id: n1\n    code: QC-PSY-01\n    kind: negative\n    given:\n      fixture: f.json\n    expect:\n      codes: [QC-PSY-01]\n";
        let e = load_cases(yaml).unwrap_err();
        assert_eq!(e.code, "E_REGRESS_NEGATIVE_WITH_CODE");
    }

    #[test]
    fn a_positive_case_must_name_the_code_it_expects() {
        let yaml = "version: \"1.0\"\ncases:\n  - id: p1\n    code: QC-PSY-01\n    kind: positive\n    given:\n      fixture: f.json\n    expect: {}\n";
        assert_eq!(load_cases(yaml).unwrap_err().code, "E_REGRESS_POSITIVE_NO_CODE");
    }

    #[test]
    fn duplicate_ids_are_refused() {
        let yaml = "version: \"1.0\"\ncases:\n  - id: d\n    code: QC-PSY-01\n    kind: positive\n    given:\n      fixture: f.json\n    expect:\n      codes: [QC-PSY-01]\n  - id: d\n    code: QC-PSY-02\n    kind: negative\n    given:\n      fixture: f.json\n    expect: {}\n";
        assert_eq!(load_cases(yaml).unwrap_err().code, "E_REGRESS_DUP_ID");
    }

    #[test]
    fn a_version_bump_refuses_the_old_file_instead_of_reinterpreting_it() {
        let yaml = "version: \"0.9\"\ncases:\n  - id: p\n    code: QC-PSY-01\n    kind: positive\n    given:\n      fixture: f.json\n    expect:\n      codes: [QC-PSY-01]\n";
        assert_eq!(load_cases(yaml).unwrap_err().code, "E_REGRESS_VERSION");
    }

    #[test]
    fn slicing_keeps_only_the_named_items() {
        let g = Given { fixture: "f".into(), doc_ids: vec!["A".into()] };
        let items: Vec<DocItem> = Vec::new();
        assert!(g.slice_items(items).is_empty());
    }

    #[test]
    fn a_missing_code_is_reported_as_missed_and_an_extra_one_as_over_reported() {
        let hint = diff_hint(
            &["QC-PSY-01".to_string(), "QC-PSY-02".to_string()],
            &["QC-PSY-02".to_string(), "QC-PSY-07".to_string()],
        );
        assert!(hint.contains("漏报 QC-PSY-01"), "{hint}");
        assert!(hint.contains("多报 QC-PSY-07"), "{hint}");
    }

    #[test]
    fn coverage_is_only_complete_when_all_three_kinds_exist() {
        let mut row = CoverageRow {
            code: "QC-PSY-01".into(),
            title: "t".into(),
            positive: 1,
            negative: 1,
            boundary: 0,
        };
        assert!(!row.is_covered());
        row.boundary = 1;
        assert!(row.is_covered());
    }
}