//! 规则条件的**声明式表达式求值器**。
//!
//! ## 为什么需要这一层
//!
//! Task 6 / 7 的规则条件写死在 `if` 里：`if !f.is_unfilled() { … }`、
//! `if n > spec.max { … }`。这类条件有两个绕不过去的问题：
//!
//! 1. **改条件要改代码、重新发版**。病案室想把「约束时长 > 4 小时」改成
//!    「> 3 小时」，得让信息科排期发版；而这条口径本来就在规则集 YAML 里。
//! 2. **判不了要说清「为什么没判」**。硬编码 `if` 走不下去时只有一条路：
//!    什么都不报。于是「这条为什么没报」就成了查不到出处的问题——而这正是
//!    本项目从 Task 5 起反复守的那条纪律。
//!
//! [`EvalOutcome`] 的四态就是为第 2 点存在的：**`Error` 不是「通过」**。
//!
//! ## 源产品主能力与本轮处置
//!
//! 源产品的规则执行是一个**遍历语法树、逐个变体匹配**的递归函数
//! （`github_ref/ruff/crates/ruff_linter/src/rules/airflow/rules/runtime_value_in_dag_or_task.rs:99`
//! `find_runtime_varying_call`）：它 `match` 遍 `Expr` 的全部变体，
//! 命中就 `checker.report_diagnostic(…, expr.range())` 上报，
//! 变体列表与 [`ast::Expr`] 枚举**逐条对齐**，少一个变体就等于静默漏判。
//!
//! 保留的是那个形状——**判定条件穷举、每条产出带位置的结论**；
//! 改写的是判定**依据**：把写死的 `match` 换成可从 YAML 声明的
//! [`Expr`]，把「变体漏了就静默不报」换成 [`EvalOutcome::Error`] 显式报出。
//! 位置语义同源：源侧 `expr.range()` 出 `TextRange`，本模块出
//! `(section_index, char_range)`，与源侧一样是**字符口径**而非字节口径。
//!
//! ## 三条不做的事
//!
//! 1. **不把「判不了」当「通过」**。类型不匹配返回 `Error(E_TYPE_MISMATCH)`，
//!    日期解析不了返回 `Error(E_DATE_PARSE)`，正则编译不过返回
//!    `Error(E_REGEX_INVALID)`——三者都**不是** `Pass`。
//! 2. **不合并三态**。字段「未出现」/「值为 null」/「值为空串」是三件不同的事，
//!    `eq ""` 对空串是 `Pass`，对 null 与未出现是 `Skip` + 不同原因。
//! 3. **不按字节算长度**。全文 `char_range` 用 Unicode 标量下标，
//!    与文书卡片高亮同口径；非 UTF-8 字节按替换字符处理，**不 panic、不越界**。

use std::fmt;

use chrono::NaiveDate;
use chrono::NaiveDateTime;
use psychdoc_model::{DocItem, Field, Span};
use regex::Regex;

use crate::values;

/// 数值字段遇到非数字文本。
pub const E_TYPE_MISMATCH: &str = "E_TYPE_MISMATCH";
/// 日期 / 日期时间字段遇到非法格式。
pub const E_DATE_PARSE: &str = "E_DATE_PARSE";
/// 正则表达式编译失败（规则集写错，不是文书的问题）。
pub const E_REGEX_INVALID: &str = "E_REGEX_INVALID";
/// 字段路径写不出三级形态。
pub const E_PATH_INVALID: &str = "E_PATH_INVALID";
/// 算子与操作数形态对不上（如 `between` 配了文本操作数）。
pub const E_OPERAND_MISMATCH: &str = "E_OPERAND_MISMATCH";
/// 数值比较跨了量纲（如字段是「小时」、条件写的是「毫克」）——**不换算、不猜**。
pub const E_DIMENSION_MISMATCH: &str = "E_DIMENSION_MISMATCH";

// ---------------------------------------------------------------------------
// 字段路径（三级）
// ---------------------------------------------------------------------------

/// 字段路径：`件` / `段` / `段内键` 三级。
///
/// 路径文法（病案室手写规则集时能直接看懂）：
///
/// | 写法 | 形态 | 含义 |
/// |---|---|---|
/// | `患方签字` | [`FieldPath::Doc`] | 件级：整件里唯一出现的这个键 |
/// | `s1` 或 `段1` | [`FieldPath::SectionText`] | 段级：整段正文 |
/// | `s1.患方签字` | [`FieldPath::SectionKey`] | 段内键：只看第 1 段的这个键 |
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldPath {
    /// 件级：按键在整件里找。同名键出现多次 → [`Resolved::Ambiguous`]，不取第一个。
    Doc(String),
    /// 段级：整段正文。段序号不存在 → [`Resolved::NoSection`]。
    SectionText(u32),
    /// 段内键：只看指定段的那个键。
    SectionKey(u32, String),
}

impl fmt::Display for FieldPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Doc(k) => f.write_str(k),
            Self::SectionText(i) => write!(f, "s{i}"),
            Self::SectionKey(i, k) => write!(f, "s{i}.{k}"),
        }
    }
}

impl FieldPath {
    /// 解析路径字符串。段前缀接受 `s3` / `段3` 两种写法。
    ///
    /// 写错（如 `s` 后面没数字、段号为负、空路径）一律 `Err(E_PATH_INVALID)`——
    /// 规则集加载期就该拦住，不能等到运行时才发现整批条件判不了。
    pub fn parse(raw: &str) -> Result<Self, ExprError> {
        let s = raw.trim();
        if s.is_empty() {
            return Err(ExprError::new(E_PATH_INVALID, "字段路径为空"));
        }
        // 没有段前缀 → 件级
        let rest = s
            .strip_prefix('s')
            .or_else(|| s.strip_prefix('段'))
            .map(str::trim);
        let Some(rest) = rest else {
            return Ok(Self::Doc(s.to_string()));
        };
        let (num, key) = match rest.split_once('.') {
            Some((n, k)) => (n.trim(), Some(k.trim())),
            None => (rest, None),
        };
        if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
            return Err(ExprError::new(
                E_PATH_INVALID,
                format!("段序号不是非负整数：{raw:?}"),
            ));
        }
        let index: u32 = num.parse().map_err(|_| {
            ExprError::new(E_PATH_INVALID, format!("段序号越界：{raw:?}"))
        })?;
        match key {
            // `s1.` 后面是空的 → 段内键缺键名，不当成段级（那会判错对象）
            Some("") => Err(ExprError::new(
                E_PATH_INVALID,
                format!("段内键缺少键名：{raw:?}"),
            )),
            Some(k) => Ok(Self::SectionKey(index, k.to_string())),
            None => Ok(Self::SectionText(index)),
        }
    }

    /// 在一件文书里解析出值。**不判定**，只定位。
    ///
    /// 定位不出来时返回的每一个变体都带着**为什么定位不出来**：
    /// 「没这一段」与「这一段里没这个键」是院内交办时最常被追问的两句。
    pub fn resolve<'a>(&self, item: &'a DocItem) -> Resolved<'a> {
        match self {
            Self::SectionText(i) => match item.sections.iter().find(|s| s.index == *i) {
                Some(sec) => Resolved::SectionText { text: &sec.text, index: *i },
                None => Resolved::NoSection(*i),
            },
            Self::SectionKey(i, key) => {
                if !item.sections.iter().any(|s| s.index == *i) {
                    return Resolved::NoSection(*i);
                }
                let hits: Vec<&Field> = item
                    .fields
                    .iter()
                    .filter(|f| f.key == *key && f.section_index == *i)
                    .collect();
                classify(&hits)
            }
            Self::Doc(key) => {
                let hits: Vec<&Field> = item.fields.iter().filter(|f| f.key == *key).collect();
                classify(&hits)
            }
        }
    }
}

/// 把命中的字段按**三态**分类。零命中 / 多命中都不判定。
fn classify<'a>(hits: &[&'a Field]) -> Resolved<'a> {
    match hits {
        [] => Resolved::Missing,
        [one] => match one.presence() {
            psychdoc_model::FieldPresence::Present => Resolved::Present {
                text: one.value.as_deref().unwrap_or_default(),
                span: one.source_span,
                section_index: one.section_index,
            },
            psychdoc_model::FieldPresence::Empty => Resolved::Empty {
                span: one.source_span,
                section_index: one.section_index,
            },
            psychdoc_model::FieldPresence::Null => Resolved::Null {
                span: one.source_span,
                section_index: one.section_index,
            },
            // 模板声明了但结构里没有这条记录——按「未出现」记账，与 Missing 同义。
            psychdoc_model::FieldPresence::Missing => Resolved::Missing,
        },
        many => Resolved::Ambiguous(many.len()),
    }
}

/// 一次定位的结果。三态与「定位不出来」各自独立，不合并。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Resolved<'a> {
    /// 键有值。
    Present {
        text: &'a str,
        span: Option<Span>,
        section_index: u32,
    },
    /// 键存在，值为空串。
    Empty {
        span: Option<Span>,
        section_index: u32,
    },
    /// 键存在，值为 `null`。
    Null {
        span: Option<Span>,
        section_index: u32,
    },
    /// 段级路径：整段正文。
    SectionText { text: &'a str, index: u32 },
    /// 键未出现。
    Missing,
    /// 同名键出现在多处，无法唯一定位。
    Ambiguous(usize),
    /// 段序号不存在。
    NoSection(u32),
}

impl Resolved<'_> {
    /// 可用于判定的文本。空串 / null / 定位不出来都返回 `None`。
    pub fn text(&self) -> Option<&str> {
        match self {
            Self::Present { text, .. } | Self::SectionText { text, .. } => Some(text),
            _ => None,
        }
    }

    /// 值在原文里的字符区间。定位不出来时为 `None`——
    /// **不编造一个零宽区间**，那会让卡片高亮到错误的位置。
    pub fn span(&self) -> Option<Span> {
        match self {
            Self::Present { span, .. } | Self::Empty { span, .. } | Self::Null { span, .. } => {
                *span
            }
            Self::SectionText { text, .. } => {
                Some(Span::unchecked(0, u32::try_from(text.chars().count()).unwrap_or(u32::MAX)))
            }
            Self::Missing | Self::Ambiguous(_) | Self::NoSection(_) => None,
        }
    }

    /// 文本的**字符**长度（不是字节长度）。
    ///
    /// 生僻字与全角字符下，这一个数与文书卡片高亮用的口径必须一致：
    /// 按字节算会让 `𠮷` 记成 4 而高亮区间只走 1 格。
    pub fn char_len(&self) -> Option<usize> {
        self.text().map(|t| t.chars().count())
    }
}

// ---------------------------------------------------------------------------
// 算子与操作数
// ---------------------------------------------------------------------------

/// 判定算子。**穷举**这十个，与源侧遍历全部 `Expr` 变体是同一个纪律：
/// 少一个就是一类文书静默漏判。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    /// 键有值（非空、非 null）。空串与 null 各自有独立文案。
    Exists,
    /// 键未出现。已出现（含空串、null）即不满足。
    NotExists,
    /// 相等。操作数为数值时按数值比，为文本时按文本比。
    Eq,
    /// 不等。
    Ne,
    /// 大于。操作数为数值或时刻。
    Gt,
    /// 落在闭区间 `[low, high]`。
    Between,
    /// 匹配正则。**编译失败返回 `Error`，不抛异常**。
    Regex,
    /// 属于集合。
    InSet,
    /// 早于操作数时刻。
    Before,
    /// 晚于操作数时刻。
    After,
}

impl fmt::Display for Operator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Exists => "exists",
            Self::NotExists => "not_exists",
            Self::Eq => "eq",
            Self::Ne => "ne",
            Self::Gt => "gt",
            Self::Between => "between",
            Self::Regex => "regex",
            Self::InSet => "in_set",
            Self::Before => "before",
            Self::After => "after",
        })
    }
}

/// 操作数。形态必须与算子对得上，对不上即 `Error(E_OPERAND_MISMATCH)`。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "value", rename_all = "snake_case")]
pub enum Operand {
    /// 无操作数（`exists` / `not_exists`）。也是 `#[serde(default)]` 的落点。
    #[default]
    None,
    /// 文本。
    Text(String),
    /// 数值（**不带单位**）。字段带单位时按该量纲的基准单位比。
    Number(f64),
    /// 数值 + 单位（如 `约束时长 <= 4 小时`）。
    ///
    /// 写单位不是为了好看：`2 小时` 与 `120 分钟` 是同一个量纲的两个写法，
    /// 换算后才比得了；跨量纲（小时 vs 毫克）一律报错，不换算。
    Quantity { value: f64, unit: String },
    /// 闭区间（`between`），**不带单位**。
    Range(f64, f64),
    /// 带单位的闭区间（`between`），如「0.5~4 小时」。
    QuantityRange { low: f64, high: f64, unit: String },
    /// 候选集合（`in_set`）。
    Set(Vec<String>),
    /// 字面时刻（`before` / `after`）。
    Moment(String),
    /// 另一条字段路径（`before` / `after` 的「与另一件字段比」形态）。
    Path(FieldPathSerde),
}

/// `FieldPath` 的 YAML/JSON 形态：一律写字符串。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct FieldPathSerde(pub String);

impl fmt::Display for FieldPathSerde {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// 一条声明式条件。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Expr {
    pub path: FieldPathSerde,
    pub op: Operator,
    #[serde(default)]
    pub operand: Operand,
}

impl Expr {
    /// 从 YAML 片段读一条条件。
    ///
    /// 规则集里写法示例：
    ///
    /// ```yaml
    /// - path: "约束时长"
    ///   op: between
    ///   operand: { type: range, value: [0.5, 4] }
    /// ```
    pub fn from_yaml(yaml: &str) -> Result<Self, ExprError> {
        serde_yaml::from_str(yaml)
            .map_err(|e| ExprError::new(E_PATH_INVALID, format!("条件声明解析失败：{e}")))
    }

    /// 解析出的字段路径。路径非法即 `Err`，**不在运行时才发现整批判不了**。
    pub fn field_path(&self) -> Result<FieldPath, ExprError> {
        FieldPath::parse(&self.path.0)
    }

    /// 求值。
    pub fn eval(&self, item: &DocItem) -> EvalOutcome {
        match self.field_path() {
            Err(e) => EvalOutcome::Error { code: e.code, detail: e.detail },
            Ok(path) => eval(&path, self.op, &self.operand, item),
        }
    }
}

// ---------------------------------------------------------------------------
// 求值结果（四态）
// ---------------------------------------------------------------------------

/// 一次求值的结果。**四态互不合并**：
///
/// | 态 | 含义 | 上层怎么用 |
/// |---|---|---|
/// | [`Pass`](Self::Pass) | 条件成立 | —— |
/// | [`Fail`](Self::Fail) | 条件不成立且**判得出来** | 产诊断 |
/// | [`Skip`](Self::Skip) | 判不了，但**说得清为什么** | 记 `skipped_reason`，交人工 |
/// | [`Error`](Self::Error) | 判不了，且**原因在规则集这边** | 阻断该批 / 报配置错 |
///
/// `Skip` 与 `Error` 的分界是本模块最重要的一条：文书缺数据是 [`Skip`]，
/// 规则集写错是 [`Error`]。两者都**不是** `Pass`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EvalOutcome {
    Pass,
    Fail { detail: String },
    Skip { reason: SkipCause },
    Error { code: &'static str, detail: String },
}

impl EvalOutcome {
    pub fn is_pass(&self) -> bool {
        matches!(self, Self::Pass)
    }
    pub fn is_fail(&self) -> bool {
        matches!(self, Self::Fail { .. })
    }
    pub fn is_skip(&self) -> bool {
        matches!(self, Self::Skip { .. })
    }
    pub fn is_error(&self) -> bool {
        matches!(self, Self::Error { .. })
    }
    /// 稳定错误码；非 `Error` 态为 `None`。
    pub fn error_code(&self) -> Option<&'static str> {
        match self {
            Self::Error { code, .. } => Some(*code),
            _ => None,
        }
    }
    /// 交给 `skipped_checks` / 错误码表的一句话。
    pub fn describe(&self) -> String {
        match self {
            Self::Pass => "条件成立".to_string(),
            Self::Fail { detail } => detail.clone(),
            Self::Skip { reason } => reason.describe(),
            Self::Error { code, detail } => format!("{detail}（{code}）"),
        }
    }
}

/// 跳过的原因。**每一种都带得走**：交人工时护士长问「这条为什么没报」，
/// 答案必须能从这里直接读到。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipCause {
    /// 键未出现。
    FieldMissing,
    /// 键存在但值为 `null`（与空串、未出现是三件不同的事）。
    FieldNull,
    /// 键存在但值为空串。
    FieldEmpty,
    /// 同名键出现在多处，取哪一处都不对。
    AmbiguousField(usize),
    /// 段序号不存在。
    NoSection(u32),
    /// 操作数形态与算子对不上（如 `before` 配了数值）。
    OperandKind,
    /// 另一侧（`Operand::Path`）定位不出来或不是时刻。
    OtherSide,
}

impl SkipCause {
    pub fn describe(self) -> String {
        match self {
            Self::FieldMissing => "字段未出现，本条未判定（需人工核）".to_string(),
            Self::FieldNull => "字段存在但值为 null，本条未判定（需人工核）".to_string(),
            Self::FieldEmpty => "字段存在但为空串，本条未判定（需人工核）".to_string(),
            Self::AmbiguousField(n) => format!("同名键出现在 {n} 处，无法唯一定位，本条未判定"),
            Self::NoSection(i) => format!("文书内没有第 {i} 段，本条未判定"),
            Self::OperandKind => "操作数形态与算子不匹配，本条未判定".to_string(),
            Self::OtherSide => "比对侧定位不到或不是时刻，本条未判定".to_string(),
        }
    }
}

/// 表达式层的错误。只在**规则集写错**时产生，文书数据问题一律走 `Skip`。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}（{code}）")]
pub struct ExprError {
    pub code: &'static str,
    pub detail: String,
}

impl ExprError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }
}

// ---------------------------------------------------------------------------
// 时刻
// ---------------------------------------------------------------------------

/// 一个可比较的时刻，保留**日 / 秒**两档精度。
///
/// 院内文书里「约束起始时间」可能只写到日（「2026-03-02」），
/// 也可能写到分秒。两侧精度不同档时统一降到日再比——
/// 逐秒精度降级与并列容差由 Task 19 专门处理，本模块只保证**不比错**。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MomentValue {
    Day(NaiveDate),
    DateTime(NaiveDateTime),
}

impl MomentValue {
    fn parse(raw: &str) -> Option<Self> {
        if let Some(dt) = values::parse_datetime(raw) {
            return Some(Self::DateTime(dt));
        }
        values::parse_date(raw).map(Self::Day)
    }

    /// 与另一时刻比大小；任一侧为日精度则按日比。
    fn cmp_to(self, other: Self) -> std::cmp::Ordering {
        match (self, other) {
            (Self::Day(a), Self::Day(b)) => a.cmp(&b),
            (Self::DateTime(a), Self::DateTime(b)) => a.cmp(&b),
            (Self::Day(a), Self::DateTime(b)) => a.cmp(&b.date()),
            (Self::DateTime(a), Self::Day(b)) => a.date().cmp(&b),
        }
    }

    fn as_zh(self) -> &'static str {
        match self {
            Self::Day(_) => "日期",
            Self::DateTime(_) => "日期时间",
        }
    }
}

// ---------------------------------------------------------------------------
// 求值
// ---------------------------------------------------------------------------

/// 求值一条条件。
///
/// 与源侧 `find_runtime_varying_call` 同形状：把输入**逐个变体**走一遍，
/// 命中即给结论。差别只有一处，且是本项目刻意要的：走不通时给
/// [`EvalOutcome`] 的显式一态，而不是返回 `None` 了事。
pub fn eval(path: &FieldPath, op: Operator, operand: &Operand, item: &DocItem) -> EvalOutcome {
    let resolved = path.resolve(item);

    // —— 存在性算子：唯一会把三态**判成结论**的两种。
    // 其余算子一律要求「有值」，三态各走各的 Skip。
    match op {
        Operator::Exists | Operator::NotExists => return eval_presence(op, resolved),
        _ => {}
    }

    // —— 其余算子：定位不出值就跳过，**不当作空值参与比较**。
    //
    // 空串是唯一例外：它**确实是一个可比较的文本值**，所以
    // `eq ""` 对空串判 `Pass`（真相等），对 `null` 与「未出现」判 `Skip`。
    // 这正是三态不能合并的理由——合并后这条就没法判了。
    // 但空串进不了数值/时刻算子：`""` 不是一个数，也不是一个时刻。
    let (text, is_empty) = match resolved {
        Resolved::Present { text, .. } | Resolved::SectionText { text, .. } => (text, false),
        Resolved::Empty { .. } => ("", true),
        Resolved::Null { .. } => return skip(SkipCause::FieldNull),
        Resolved::Missing => return skip(SkipCause::FieldMissing),
        Resolved::Ambiguous(n) => return skip(SkipCause::AmbiguousField(n)),
        Resolved::NoSection(i) => return skip(SkipCause::NoSection(i)),
    };
    // 数值 / 时刻算子看到空串只能跳过——拿空串去比大小是编造事实。
    if is_empty && !matches!(op, Operator::Eq | Operator::Ne | Operator::Regex | Operator::InSet) {
        return skip(SkipCause::FieldEmpty);
    }

    match op {
        Operator::Eq | Operator::Ne => {
            let want_equal = op == Operator::Eq;
            let equal = match operand {
                Operand::Text(want) => cmp_text(text, want),
                Operand::Number(want) => match measure(text) {
                    Some((n, _)) => n == *want,
                    None => {
                        if is_empty {
                            return skip(SkipCause::FieldEmpty);
                        }
                        return error(E_TYPE_MISMATCH, format!("无法按数值解析「{text}」"));
                    }
                },
                Operand::Quantity { value, unit } => {
                    match compare_measured(text, *value, unit) {
                        Ok(ord) => ord.is_eq(),
                        Err(code) => return error(code, measured_detail(text, unit, code)),
                    }
                }
                _ => return skip(SkipCause::OperandKind),
            };
            outcome(equal == want_equal, || {
                format!("实际填「{text}」，与条件{}一致", if want_equal { "" } else { "不" })
            })
        }
        Operator::Gt => {
            // 量纲先核对：跨量纲的比较是「配置错」，不是「不成立」。
            let ord = match operand {
                Operand::Number(bound) => match measure(text) {
                    Some((n, _)) => n.partial_cmp(bound),
                    None => return error(E_TYPE_MISMATCH, format!("无法按数值解析「{text}」")),
                },
                Operand::Quantity { value, unit } => {
                    match compare_measured(text, *value, unit) {
                        Ok(ord) => Some(ord),
                        Err(code) => return error(code, measured_detail(text, unit, code)),
                    }
                }
                // 「早于 / 晚于」也能用 gt 表达时刻比较，此处不做别名：
                // 规则集写错形态比写别名更容易被发现。
                _ => return skip(SkipCause::OperandKind),
            };
            outcome(
                ord.is_some_and(|o| o.is_gt()),
                || format!("实际填「{text}」，应不大于 {}", bound_text(operand)),
            )
        }
        Operator::Between => {
            // `range` 形态不带单位；`quantities` 形态带单位（病案室要按「小时」写口径时用）。
            let (low, high, unit) = match operand {
                Operand::Range(a, b) => (*a, *b, ""),
                Operand::QuantityRange { low, high, unit } => (*low, *high, unit.as_str()),
                _ => return skip(SkipCause::OperandKind),
            };
            let Some((n, base)) = measure(text) else {
                return error(E_TYPE_MISMATCH, format!("无法按数值解析「{text}」"));
            };
            // 量纲核对在区间判定**之前**：跨量纲时「在区间内」是毫无意义的结论。
            // 条件侧同样要换算到基准单位，否则「0.5~4 小时」会拿 0.5~4 分钟去比。
            let (low, high) = if unit.is_empty() {
                (low, high)
            } else {
                let Some(u) = values::lookup_unit(unit) else {
                    return error(E_DIMENSION_MISMATCH, measured_detail(text, unit, E_DIMENSION_MISMATCH));
                };
                if u.dimension.base().0 != base {
                    return error(E_DIMENSION_MISMATCH, measured_detail(text, unit, E_DIMENSION_MISMATCH));
                }
                match (u.to_base(low), u.to_base(high)) {
                    (Some((a, _)), Some((b, _))) => (a, b),
                    _ => return error(E_DIMENSION_MISMATCH, measured_detail(text, unit, E_DIMENSION_MISMATCH)),
                }
            };
            outcome(
                n >= low && n <= high,
                || format!("实际填「{text}」，应在 {low}~{high}{} 之间", unit_suffix(unit)),
            )
        }
        Operator::InSet => {
            let candidates = match operand {
                Operand::Set(xs) => xs,
                _ => return skip(SkipCause::OperandKind),
            };
            let hit = candidates.iter().any(|c| cmp_text(text, c));
            outcome(hit, || format!("实际填「{text}」，不在允许取值内"))
        }
        Operator::Regex => {
            let pattern = match operand {
                Operand::Text(p) => p,
                _ => return skip(SkipCause::OperandKind),
            };
            // 编译失败是**规则集写错**，不是文书的问题 → Error，不 panic。
            let re = match Regex::new(pattern) {
                Ok(re) => re,
                Err(e) => return error(E_REGEX_INVALID, format!("正则 {pattern:?} 编译失败：{e}")),
            };
            outcome(re.is_match(text), || format!("实际填「{text}」，不匹配 {pattern}"))
        }
        Operator::Before | Operator::After => eval_order(op, operand, text, item),
        Operator::Exists | Operator::NotExists => unreachable!("已在上方提前返回"),
    }
}

/// 存在性算子。**三态在这里各给各的文案**——
/// 「写了但是空的」和「压根没写」在交办时是两句话。
fn eval_presence(op: Operator, resolved: Resolved<'_>) -> EvalOutcome {
    let (present, detail): (bool, String) = match resolved {
        Resolved::Present { text, .. } | Resolved::SectionText { text, .. } => {
            (true, format!("已填写「{text}」"))
        }
        Resolved::Empty { .. } => (false, "字段存在但值为空串".to_string()),
        Resolved::Null { .. } => (false, "字段存在但值为 null".to_string()),
        Resolved::Missing => (false, "字段未出现".to_string()),
        Resolved::Ambiguous(n) => {
            return skip(SkipCause::AmbiguousField(n));
        }
        Resolved::NoSection(i) => return skip(SkipCause::NoSection(i)),
    };
    let want_present = op == Operator::Exists;
    outcome(present == want_present, || detail)
}

/// `before` / `after`：值是时刻，操作数是字面时刻或另一条字段路径。
fn eval_order(op: Operator, operand: &Operand, text: &str, item: &DocItem) -> EvalOutcome {
    let Some(lhs) = MomentValue::parse(text) else {
        return error(E_DATE_PARSE, format!("无法按日期时间解析「{text}」"));
    };
    let rhs = match operand {
        Operand::Moment(raw) => match MomentValue::parse(raw) {
            Some(m) => m,
            None => {
                return error(E_DATE_PARSE, format!("条件里的时刻 {raw:?} 解析不了"))
            }
        },
        Operand::Path(other) => {
            let path = match FieldPath::parse(&other.0) {
                Ok(p) => p,
                Err(e) => return EvalOutcome::Error { code: e.code, detail: e.detail },
            };
            match path.resolve(item).text().and_then(MomentValue::parse) {
                Some(m) => m,
                None => return skip(SkipCause::OtherSide),
            }
        }
        _ => return skip(SkipCause::OperandKind),
    };
    let ord = lhs.cmp_to(rhs);
    let want_earlier = op == Operator::Before;
    let ok = if want_earlier { ord.is_lt() } else { ord.is_gt() };
    outcome(ok, || {
        format!(
            "实际填「{text}」（{}），与 {}「{}」不符",
            lhs.as_zh(),
            if want_earlier { "早于" } else { "晚于" },
            rhs_text(rhs, operand)
        )
    })
}

fn rhs_text(rhs: MomentValue, operand: &Operand) -> String {
    match operand {
        Operand::Path(other) => other.0.clone(),
        _ => match rhs {
            MomentValue::Day(d) => d.to_string(),
            MomentValue::DateTime(t) => t.to_string(),
        },
    }
}

/// 文本比较：**剥文末句读 + 去首尾空白**，不静默做全角归一。
///
/// 全角数字（`２０２６`）与半写的 `2026` 在这里判为不等——
/// 那是值形状问题，归 `QC-PSY-02` 管；在相等性上把它们抹平会让那条规则永远不报。
fn cmp_text(actual: &str, expect: &str) -> bool {
    values::trim_sentence_punct(actual).trim() == values::trim_sentence_punct(expect).trim()
}

/// 数值比较按**原文**解析（不做全角归一），与 [`crate::values::check_shape`] 同口径，
/// 并**连单位一起换算**到该量纲的基准单位。
///
/// 跨量纲比较是最容易出事的一处：院内同一件事会写 `2 小时` 也写 `120 分钟`，
/// 丢单位比数字会把 120 判成「120 小时」。因此这里把基准单位一并带出来，
/// 由调用方核对量纲是否一致——**不一致就报错，不换算**。
fn measure(raw: &str) -> Option<(f64, &'static str)> {
    values::parse(raw).measured()
}

/// 按量纲核对并比较。基准单位不同即 [`E_DIMENSION_MISMATCH`]。
fn compare_measured(
    raw: &str,
    bound: f64,
    bound_unit: &str,
) -> Result<std::cmp::Ordering, &'static str> {
    let Some((n, base)) = measure(raw) else {
        return Err(E_TYPE_MISMATCH);
    };
    // 条件侧的单位：没写单位就视为「与字段同一量纲」，由字段说了算，
    // 此时 bound 直接就是基准单位下的数。
    let bound_in_base = if bound_unit.is_empty() {
        bound
    } else {
        let u = match values::lookup_unit(bound_unit) {
            Some(u) => u,
            // 条件里写了表外单位（如「光年」）→ 配置错，不当成裸数字
            None => return Err(E_DIMENSION_MISMATCH),
        };
        // 跨量纲没有共同基准，「小时 vs 毫克」比出来的任何数字都没有意义。
        if u.dimension.base().0 != base {
            return Err(E_DIMENSION_MISMATCH);
        }
        // **两侧都换算到基准单位再比**——只换算一侧会把 2 小时判成 2 分钟。
        u.to_base(bound).map(|(v, _)| v).ok_or(E_DIMENSION_MISMATCH)?
    };
    Ok(n.partial_cmp(&bound_in_base).unwrap_or(std::cmp::Ordering::Equal))
}

/// 量纲不匹配时的文案：说清「字段是什么单位、条件和它差在哪」。
fn measured_detail(raw: &str, unit: &str, code: &'static str) -> String {
    let field_unit = measure(raw).map(|(_, base)| base).unwrap_or("");
    format!(
        "实际填「{raw}」按{}计，条件按「{unit}」计，量纲对不上（{code}）",
        if field_unit.is_empty() { "无单位" } else { field_unit }
    )
}

fn bound_text(operand: &Operand) -> String {
    match operand {
        Operand::Number(n) => format!("{n}"),
        Operand::Quantity { value, unit } => format!("{value} {unit}"),
        _ => "—".to_string(),
    }
}

fn unit_suffix(unit: &str) -> String {
    if unit.is_empty() { String::new() } else { format!(" {unit}") }
}

fn outcome(ok: bool, detail: impl FnOnce() -> String) -> EvalOutcome {
    if ok {
        EvalOutcome::Pass
    } else {
        EvalOutcome::Fail { detail: detail() }
    }
}

fn skip(reason: SkipCause) -> EvalOutcome {
    EvalOutcome::Skip { reason }
}

fn error(code: &'static str, detail: String) -> EvalOutcome {
    EvalOutcome::Error { code, detail }
}

/// 把一段**原始字节**解成可判定的文本。
///
/// 院内导出包确实会出现 GBK 与 UTF-8 混编。这一层是**唯一**允许非 UTF-8
/// 字节进入求值器的地方：非法字节按 Unicode 替换字符处理并如实计数，
/// 既不 panic 也不越界；返回的 `bool` 供调用方记 `E_ENCODING`
/// （编码嗅探与逐件替换由 Task 22 负责，本函数只保证求值器这一侧安全）。
pub fn decode_bytes(bytes: &[u8]) -> (String, bool) {
    match std::str::from_utf8(bytes) {
        Ok(s) => (s.to_string(), false),
        Err(_) => {
            let owned = String::from_utf8_lossy(bytes).into_owned();
            let had_invalid = owned.contains('\u{FFFD}');
            (owned, had_invalid)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Section;

    fn field(key: &str, value: Option<&str>, span: Option<Span>, sec: u32) -> Field {
        Field { key: key.into(), value: value.map(str::to_string), required: true, source_span: span, section_index: sec }
    }

    fn item() -> DocItem {
        DocItem::new("WARD-A-001-CONSENT-01", "restraint_consent", "WARD-A-001")
            .with_sections(vec![
                Section { index: 0, title: "约束事由".into(), text: "约束事由：夜间反复冲动，需短时保护。".into() },
                Section {
                    index: 1,
                    title: "患方知情同意".into(),
                    text: "患方意见：已知情同意。\n约束起始时间：2026-03-02 23:40。\n约束时长：2 小时。".into(),
                },
            ])
            .with_fields(vec![
                field("约束事由", Some("夜间反复冲动，需短时保护"), Some(Span::unchecked(5, 15)), 0),
                field("患方意见", Some("已知情同意"), Some(Span::unchecked(5, 10)), 1),
                field("约束起始时间", Some("2026-03-02 23:40"), Some(Span::unchecked(7, 22)), 1),
                field("约束时长", Some("2 小时"), Some(Span::unchecked(5, 8)), 1),
            ])
    }

    fn eval_path(raw: &str, op: Operator, operand: &Operand) -> EvalOutcome {
        let path = FieldPath::parse(raw).expect("路径应可解析");
        eval(&path, op, operand, &item())
    }

    // —— 算子集：十个各一例 ---------------------------------------------

    #[test]
    fn exists_passes_on_a_filled_field() {
        assert_eq!(eval_path("患方意见", Operator::Exists, &Operand::None), EvalOutcome::Pass);
    }

    #[test]
    fn not_exists_passes_only_when_the_key_is_absent() {
        assert_eq!(eval_path("护士签名", Operator::NotExists, &Operand::None), EvalOutcome::Pass);
        assert!(eval_path("患方意见", Operator::NotExists, &Operand::None).is_fail());
    }

    #[test]
    fn eq_compares_text_and_number() {
        assert_eq!(
            eval_path("患方意见", Operator::Eq, &Operand::Text("已知情同意".into())),
            EvalOutcome::Pass
        );
        assert!(eval_path("患方意见", Operator::Eq, &Operand::Text("已知情".into())).is_fail());
        // 文本操作数 + 数值字段：按文本比，不做强制
        assert!(eval_path("约束时长", Operator::Eq, &Operand::Text("2 小时".into())).is_pass());
        // 数值操作数走量纲换算：2 小时 = 120 分钟
        assert!(eval_path("约束时长", Operator::Eq, &Operand::Quantity { value: 120.0, unit: "分钟".into() }).is_pass());
    }

    #[test]
    fn ne_is_the_negation_of_eq() {
        assert!(eval_path("患方意见", Operator::Ne, &Operand::Text("未知情".into())).is_pass());
        assert!(eval_path("患方意见", Operator::Ne, &Operand::Text("已知情同意".into())).is_fail());
    }

    #[test]
    fn gt_compares_numbers_against_a_bound() {
        // 约束时长 = 2 小时 = 120 分钟（基准单位）
        assert!(eval_path("约束时长", Operator::Gt, &Operand::Number(60.0)).is_pass());
        assert!(eval_path("约束时长", Operator::Gt, &Operand::Number(240.0)).is_fail());
        assert!(eval_path("约束时长", Operator::Gt, &Operand::Quantity { value: 1.0, unit: "小时".into() }).is_pass());
        assert!(eval_path("约束时长", Operator::Gt, &Operand::Quantity { value: 4.0, unit: "小时".into() }).is_fail());
    }

    #[test]
    fn values_in_different_units_are_compared_after_conversion() {
        // 同量纲：2 小时 = 120 分钟，判「等于 2 小时」成立
        assert!(
            eval_path("约束时长", Operator::Eq, &Operand::Quantity { value: 2.0, unit: "小时".into() }).is_pass()
        );
        // 差一个数量级：120 ≠ 2
        assert!(
            eval_path("约束时长", Operator::Ne, &Operand::Quantity { value: 2.0, unit: "分钟".into() }).is_pass()
        );
    }

    #[test]
    fn a_comparison_across_dimensions_is_an_error_not_a_verdict() {
        // 「小时」与「毫克」没有共同基准——报错，绝不拿数字硬比
        let out = eval_path("约束时长", Operator::Gt, &Operand::Quantity { value: 100.0, unit: "毫克".into() });
        assert_eq!(out.error_code(), Some(E_DIMENSION_MISMATCH));
        assert!(!out.is_pass());
        // 条件侧写了表外单位，同样是配置错
        let bogus = eval_path("约束时长", Operator::Gt, &Operand::Quantity { value: 1.0, unit: "光年".into() });
        assert_eq!(bogus.error_code(), Some(E_DIMENSION_MISMATCH));
        // 带单位区间同理
        let r = Operand::QuantityRange { low: 0.0, high: 4.0, unit: "毫克".into() };
        assert_eq!(
            eval_path("约束时长", Operator::Between, &r).error_code(),
            Some(E_DIMENSION_MISMATCH)
        );
    }

    #[test]
    fn between_is_a_closed_interval() {
        // 约束时长 = 2 小时 = 120 分钟；`range` 形态不带单位，按基准单位比
        let r = Operand::Range(60.0, 240.0);
        assert!(eval_path("约束时长", Operator::Between, &r).is_pass());
        // 闭区间端点必须算在内
        let edge = Operand::Range(120.0, 240.0);
        assert!(eval_path("约束时长", Operator::Between, &edge).is_pass());
        let narrow = Operand::Range(150.0, 240.0);
        assert!(eval_path("约束时长", Operator::Between, &narrow).is_fail());
        // 带单位区间按医院口径写：0.5~4 小时
        let with_unit = Operand::QuantityRange { low: 0.5, high: 4.0, unit: "小时".into() };
        assert!(eval_path("约束时长", Operator::Between, &with_unit).is_pass());
    }

    #[test]
    fn regex_matches_the_value() {
        let p = Operand::Text(r"^二级保护$".into());
        assert!(eval_path("保护等级", Operator::Regex, &p).is_skip(), "键不存在时应跳过而非判失败");
        let mut it = item();
        it.fields.push(field("保护等级", Some("二级保护"), None, 0));
        let path = FieldPath::parse("保护等级").unwrap();
        assert_eq!(eval(&path, Operator::Regex, &p, &it), EvalOutcome::Pass);
        let other = Operand::Text(r"^一级保护$".into());
        assert!(eval(&path, Operator::Regex, &other, &it).is_fail());
    }

    #[test]
    fn in_set_checks_membership() {
        let s = Operand::Set(vec!["一级保护".into(), "二级保护".into()]);
        let mut it = item();
        it.fields.push(field("保护等级", Some("二级保护"), None, 0));
        let path = FieldPath::parse("保护等级").unwrap();
        assert_eq!(eval(&path, Operator::InSet, &s, &it), EvalOutcome::Pass);
        let other = Operand::Set(vec!["三级保护".into()]);
        assert!(eval(&path, Operator::InSet, &other, &it).is_fail());
    }

    #[test]
    fn before_and_after_compare_moments() {
        let one = FieldPath::parse("约束起始时间").unwrap();
        let two = FieldPath::parse("解除时间").unwrap();
        let mut it = item();
        it.fields.push(field("解除时间", Some("2026-03-03 06:10"), None, 1));
        let rel = Operand::Path(FieldPathSerde("解除时间".into()));
        assert!(eval(&one, Operator::Before, &rel, &it).is_pass(), "开始早于解除应成立");
        assert!(eval(&two, Operator::Before, &rel, &it).is_fail());

        let lit = Operand::Moment("2026-03-03 00:00".into());
        assert!(eval(&one, Operator::Before, &lit, &it).is_pass());
        assert!(eval(&one, Operator::After, &lit, &it).is_fail());
    }

    // —— 三态区分 --------------------------------------------------------

    #[test]
    fn the_three_states_never_collapse_into_equals_empty() {
        let eq_empty = Operand::Text(String::new());
        let eq = Operator::Eq;

        // 空串：真的就是空串 → 能判
        let mut empty = item();
        empty.fields.push(field("患方签字", Some(""), Some(Span::unchecked(0, 0)), 1));
        let path = FieldPath::parse("患方签字").unwrap();
        assert_eq!(eval(&path, eq, &eq_empty, &empty), EvalOutcome::Pass, "空串确实等于空串");

        // null：判不了
        let mut nulled = item();
        nulled.fields.push(field("患方签字", None, Some(Span::unchecked(0, 0)), 1));
        assert_eq!(
            eval(&path, eq, &eq_empty, &nulled),
            EvalOutcome::Skip { reason: SkipCause::FieldNull }
        );

        // 未出现：同样判不了，但原因必须能说出区别
        assert_eq!(
            eval(&path, eq, &eq_empty, &item()),
            EvalOutcome::Skip { reason: SkipCause::FieldMissing }
        );
    }

    #[test]
    fn exists_gives_a_different_wording_for_each_of_the_three_states() {
        let mut empty = item();
        empty.fields.push(field("患方签字", Some(""), None, 1));
        let mut nulled = item();
        // null 态：键在结构里但值为 null —— 必须带 source_span，
        // 否则 Field::presence() 按「未出现」记账，三态就又混回去了。
        nulled.fields.push(field("患方签字", None, Some(Span::unchecked(0, 0)), 1));
        let op = Operator::Exists;
        let path = FieldPath::parse("患方签字").unwrap();
        let w_empty = eval(&path, op, &Operand::None, &empty).describe();
        let w_null = eval(&path, op, &Operand::None, &nulled).describe();
        let w_missing = eval(&path, op, &Operand::None, &item()).describe();
        // 三态都判「不成立」，但文案必须能让人一眼分出是哪一种
        assert!(eval(&path, op, &Operand::None, &empty).is_fail());
        assert!(eval(&path, op, &Operand::None, &nulled).is_fail());
        assert!(eval(&path, op, &Operand::None, &item()).is_fail());
        assert!(w_empty.contains("空串"), "{w_empty}");
        assert!(w_null.contains("null"), "{w_null}");
        assert!(w_missing.contains("未出现"), "{w_missing}");
        assert_ne!(w_empty, w_null);
        assert_ne!(w_null, w_missing);
    }

    // —— 失败码 ----------------------------------------------------------

    #[test]
    fn a_non_numeric_value_on_a_numeric_operator_is_a_type_mismatch() {
        let out = eval_path("患方意见", Operator::Gt, &Operand::Number(1.0));
        assert_eq!(out.error_code(), Some(E_TYPE_MISMATCH));
        assert!(!out.is_pass(), "类型不匹配绝不能算通过");
        assert!(out.describe().contains("无法按数值解析"));
    }

    #[test]
    fn a_bad_date_is_reported_as_a_date_parse_error() {
        let mut it = item();
        it.fields.push(field("解除时间", Some("次日清晨"), None, 1));
        let p = FieldPath::parse("解除时间").unwrap();
        let out = eval(&p, Operator::Before, &Operand::Moment("2026-03-03 00:00".into()), &it);
        assert_eq!(out.error_code(), Some(E_DATE_PARSE));
        assert!(!out.is_pass());
    }

    #[test]
    fn an_invalid_regex_returns_an_error_instead_of_panicking() {
        let out = eval_path("患方意见", Operator::Regex, &Operand::Text("([未闭合".into()));
        assert_eq!(out.error_code(), Some(E_REGEX_INVALID));
        assert!(!out.is_pass(), "规则集写错不能静默变成「通过」");
    }

    #[test]
    fn a_malformed_path_is_rejected_at_parse_time_not_at_run_time() {
        for bad in ["", "  ", "s", "s1.", "段", "s-1", "sX.键"] {
            let err = FieldPath::parse(bad).unwrap_err();
            assert_eq!(err.code, E_PATH_INVALID, "{bad:?} 应在解析期被拒");
        }
        assert_eq!(FieldPath::parse("s1.患方签字").unwrap(), FieldPath::SectionKey(1, "患方签字".into()));
        assert_eq!(FieldPath::parse("段2").unwrap(), FieldPath::SectionText(2));
        assert_eq!(FieldPath::parse("患方意见").unwrap(), FieldPath::Doc("患方意见".into()));
    }

    // —— 定位不到：交人工，不是判失败 ------------------------------------

    #[test]
    fn an_absent_key_is_skipped_rather_than_judged() {
        let out = eval_path("护士签名", Operator::Eq, &Operand::Text("某护士".into()));
        assert_eq!(out, EvalOutcome::Skip { reason: SkipCause::FieldMissing });
    }

    #[test]
    fn an_ambiguous_key_is_skipped_with_the_hit_count() {
        let mut it = item();
        it.fields.push(field("约束时长", Some("3 小时"), None, 0));
        let p = FieldPath::parse("约束时长").unwrap();
        let out = eval(&p, Operator::Gt, &Operand::Number(1.0), &it);
        assert_eq!(out, EvalOutcome::Skip { reason: SkipCause::AmbiguousField(2) });
        assert!(out.describe().contains("2 处"));
    }

    #[test]
    fn a_missing_section_is_reported_with_its_index() {
        let out = eval_path("s9.患方意见", Operator::Exists, &Operand::None);
        assert_eq!(out, EvalOutcome::Skip { reason: SkipCause::NoSection(9) });
        assert!(out.describe().contains("第 9 段"));
    }

    #[test]
    fn a_section_scoped_path_looks_only_inside_that_section() {
        // 「患方意见」在第 1 段；写 s0 就该找不到，而不是回退到整件。
        // `exists` 是存在性算子：找不到就是「不成立」，判 Fail 而不是 Skip。
        let out = eval_path("s0.患方意见", Operator::Exists, &Operand::None);
        assert!(out.is_fail(), "段内键写错段号时必须判不成立：{out:?}");
        assert!(out.describe().contains("未出现"), "{out:?}");
        assert!(eval_path("s1.患方意见", Operator::Exists, &Operand::None).is_pass());
        // 比较类算子在同一情形下则是「判不了」——两条路径的差别是可核对的
        assert_eq!(
            eval_path("s0.患方意见", Operator::Eq, &Operand::Text("已知情同意".into())),
            EvalOutcome::Skip { reason: SkipCause::FieldMissing }
        );
    }

    #[test]
    fn a_section_level_path_resolves_to_the_whole_paragraph() {
        let out = eval_path("s1", Operator::Regex, &Operand::Text("约束起始时间".into()));
        assert!(out.is_pass());
    }

    #[test]
    fn an_operand_of_the_wrong_shape_is_skipped_not_guessed() {
        assert_eq!(
            eval_path("约束时长", Operator::Between, &Operand::Text("2".into())),
            EvalOutcome::Skip { reason: SkipCause::OperandKind }
        );
        assert_eq!(
            eval_path("患方意见", Operator::InSet, &Operand::Number(1.0)),
            EvalOutcome::Skip { reason: SkipCause::OperandKind }
        );
    }

    // —— 字符口径与脏字节 ------------------------------------------------

    #[test]
    fn lengths_and_spans_are_counted_in_characters_not_bytes() {
        let mut it = item();
        // 𠮷 是 4 字节、1 字符；全角数字 3 字节 1 字符
        it.fields.push(field("护士备注", Some("𠮷夜班　值守"), Some(Span::unchecked(0, 6)), 1));
        let p = FieldPath::parse("护士备注").unwrap();
        let r = p.resolve(&it);
        assert_eq!(r.char_len(), Some(6), "6 个字符");
        assert_eq!(r.text().unwrap().len(), 19, "同一个值占 19 字节：口径不同结果就不同");
        let span = r.span().expect("有值就应带区间");
        assert_eq!(span.len(), 6);
    }

    #[test]
    fn gbk_mixed_bytes_do_not_panic_or_run_out_of_bounds() {
        // 「小时」的 GBK 编码：非法 UTF-8 字节
        let gbk: &[u8] = &[0xB2, 0xE2, 0xCA, 0xD4];
        let (text, had_invalid) = decode_bytes(gbk);
        assert!(had_invalid, "非 UTF-8 字节必须被如实标记");
        assert!(text.contains('\u{FFFD}'));
        // 替换后仍是合法文本，可参与判定；长度按**替换后的字符**算，
        // 而不是按原始字节数——否则卡片高亮会越界。
        let n_chars = text.chars().count();
        assert!(n_chars > 0 && n_chars <= 4, "替换后字符数 {n_chars} 不得多于原始字节数 4");
        let mut it = item();
        it.fields.retain(|f| f.key != "约束时长");
        let n_chars = u32::try_from(text.chars().count()).unwrap();
        it.fields.push(field("约束时长", Some(&text), Some(Span::unchecked(0, n_chars)), 1));
        let p = FieldPath::parse("约束时长").unwrap();
        let r = p.resolve(&it);
        assert_eq!(r.char_len(), Some(n_chars as usize));
        // 数值判定走 Error 而不是 panic
        assert_eq!(eval(&p, Operator::Gt, &Operand::Number(1.0), &it).error_code(), Some(E_TYPE_MISMATCH));
        // 区间仍然切得回文本
        assert_eq!(r.span().unwrap().slice(&text), Some(text.as_str()));
    }

    #[test]
    fn valid_utf8_bytes_report_no_encoding_loss() {
        let (text, had_invalid) = decode_bytes("2 小时".as_bytes());
        assert!(!had_invalid);
        assert_eq!(text, "2 小时");
    }

    // —— 声明形态 --------------------------------------------------------

    #[test]
    fn a_condition_can_be_declared_in_yaml() {
        let e = Expr::from_yaml(
            r#"
path: "约束时长"
op: between
operand: { type: quantity_range, value: { low: 0.5, high: 4, unit: "小时" } }
"#,
        )
        .expect("条件声明应可解析");
        assert_eq!(e.op, Operator::Between);
        assert_eq!(
            e.operand,
            Operand::QuantityRange { low: 0.5, high: 4.0, unit: "小时".into() }
        );
        assert_eq!(e.field_path().unwrap(), FieldPath::Doc("约束时长".into()));
        assert!(e.eval(&item()).is_pass());
    }

    #[test]
    fn a_declared_condition_with_a_bad_path_errors_before_it_runs() {
        let e = Expr::from_yaml("path: \"s1.\"\nop: exists\n").unwrap();
        assert_eq!(e.eval(&item()).error_code(), Some(E_PATH_INVALID));
    }

    #[test]
    fn day_precision_moments_compare_against_datetimes_at_day_granularity() {
        let mut it = item();
        it.fields.push(field("解除时间", Some("2026-03-03"), None, 1));
        let p = FieldPath::parse("解除时间").unwrap();
        let cut = Operand::Moment("2026-03-03 06:00".into());
        // 解除日 = 03-03，晚于 03-03 06:00 之前？按日精度比：同一天不算「早于」
        assert!(eval(&p, Operator::Before, &cut, &it).is_fail());
        assert!(eval(&p, Operator::After, &cut, &it).is_fail());
        let next = Operand::Moment("2026-03-02 23:59".into());
        assert!(eval(&p, Operator::After, &next, &it).is_pass());
    }

    #[test]
    fn text_comparison_tolerates_trailing_punctuation_but_not_fullwidth_digits() {
        assert!(cmp_text("已知情同意。", "已知情同意"));
        assert!(!cmp_text("２０２６-０３-０２", "2026-03-02"), "全角是形状问题，不在相等性上抹平");
    }

    #[test]
    fn an_absent_other_side_is_skipped_rather_than_judged() {
        let out = eval_path("约束起始时间", Operator::Before, &Operand::Path(FieldPathSerde("解除时间".into())));
        assert_eq!(out, EvalOutcome::Skip { reason: SkipCause::OtherSide });
    }
}
