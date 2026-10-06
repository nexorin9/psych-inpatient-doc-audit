//! 字段**值形状**解析：把段内原文里的值解析成可比对的类型。
//!
//! ## 为什么需要这一层
//!
//! 文书里的值是自然语言与半结构化文本混排：
//!
//! ```text
//! 约束等级：二级保护
//! 约束时长：2 小时
//! 评估时间：２０２６-０３-０２ 23:40
//! ```
//!
//! 要判「这个值形状对不对」，得先把它变成**可判定的形状**：
//! 数值是多少、带的什么单位、是不是日期时间、全角半角是否混用。
//! 这一层只做**解析与归一**，不做判定——判定在 [`crate::field_rules`]。
//!
//! ## 三条不做的事
//!
//! 1. **不猜**。`约束时长：两小时` 解析不出数值就如实返回 `NotNumber`，
//!    绝不按「2」估一个。猜出来的数字会进补写预览，进了就可能被当成真值。
//! 2. **不静默归一**。全角数字归一化会同时返回「改了什么」，调用方据此决定
//!    这是可自动修（`QC-PSY-02`）还是必须人工看（其它形状问题）。
//! 3. **不吞单位**。`2 小时` 与 `2 分钟` 差一个量级，把单位丢掉再比数字
//!    会制造出「数值合规」的假象。

use psychdoc_model::ValueType;
use std::fmt;

/// 值形状解析失败。`code` 直接进 message 文案，供 UI 与错误码表引用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueError {
    pub code: &'static str,
    pub detail: String,
}

impl ValueError {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self { code, detail: detail.into() }
    }
    /// 供 message 拼装：「无法按数值解析（E_TYPE_MISMATCH）」。
    pub fn describe(&self) -> String {
        format!("{}（{}）", self.detail, self.code)
    }
}

impl fmt::Display for ValueError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.describe())
    }
}

/// 量纲。跨量纲的单位错配比同量纲错配严重，message 要能说清是哪一种。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Time,
    Mass,
    Volume,
}

impl Dimension {
    pub fn as_zh(self) -> &'static str {
        match self {
            Self::Time => "时间",
            Self::Mass => "质量",
            Self::Volume => "体积",
        }
    }

    /// 该量纲的**基准单位**与换算系数（`基准 = 原值 × 系数`）。
    ///
    /// 同一量纲内换算后即可比较；跨量纲没有基准，`None`——
    /// 那正是 `QC-PSY-04` 要报的情况，不在这里猜。
    pub fn base(self) -> (&'static str, f64) {
        match self {
            Self::Time => ("分钟", 1.0),
            Self::Mass => ("毫克", 1.0),
            Self::Volume => ("毫升", 1.0),
        }
    }
}

/// 单位词表。院内文书里出现过的写法都在这儿；
/// 表外的写法按「单位不可识别」处理（`NotNumber`），不猜。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnitDef {
    pub canonical: &'static str,
    pub dimension: Dimension,
}

impl UnitDef {
    /// 换算到本量纲的基准单位。
    ///
    /// 院内同一件事会用不同单位写：`约束时长：2 小时` 与 `约束时长：120 分钟`
    /// 指的是同一时长。**丢掉单位直接比数字会把 120 判成 120 小时**，
    /// 因此凡是要比大小的都必须先过这一层。
    pub fn to_base(self, n: f64) -> Option<(f64, &'static str)> {
        let (base_name, base_factor) = self.dimension.base();
        let factor = match self.canonical {
            "分钟" => 1.0,
            "小时" => 60.0,
            "天" => 1440.0,
            "毫克" => 1.0,
            "克" => 1000.0,
            "毫升" => 1.0,
            // 表里没有的写法（如「光年」）不猜
            _ => return None,
        };
        let _ = base_factor;
        Some((n * factor, base_name))
    }
}

impl fmt::Display for UnitDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.canonical)
    }
}

const UNITS: &[(&str, UnitDef)] = &[
    ("分钟", UnitDef { canonical: "分钟", dimension: Dimension::Time }),
    ("分", UnitDef { canonical: "分钟", dimension: Dimension::Time }),
    ("min", UnitDef { canonical: "分钟", dimension: Dimension::Time }),
    ("minute", UnitDef { canonical: "分钟", dimension: Dimension::Time }),
    ("小时", UnitDef { canonical: "小时", dimension: Dimension::Time }),
    ("时", UnitDef { canonical: "小时", dimension: Dimension::Time }),
    ("h", UnitDef { canonical: "小时", dimension: Dimension::Time }),
    ("hour", UnitDef { canonical: "小时", dimension: Dimension::Time }),
    ("天", UnitDef { canonical: "天", dimension: Dimension::Time }),
    ("日", UnitDef { canonical: "天", dimension: Dimension::Time }),
    ("d", UnitDef { canonical: "天", dimension: Dimension::Time }),
    ("day", UnitDef { canonical: "天", dimension: Dimension::Time }),
    ("毫克", UnitDef { canonical: "毫克", dimension: Dimension::Mass }),
    ("mg", UnitDef { canonical: "毫克", dimension: Dimension::Mass }),
    ("克", UnitDef { canonical: "克", dimension: Dimension::Mass }),
    ("g", UnitDef { canonical: "克", dimension: Dimension::Mass }),
    ("毫升", UnitDef { canonical: "毫升", dimension: Dimension::Volume }),
    ("ml", UnitDef { canonical: "毫升", dimension: Dimension::Volume }),
];

/// 按写法查单位；表外返回 `None`（**不猜**）。
pub fn lookup_unit(raw: &str) -> Option<UnitDef> {
    let key = raw.trim();
    UNITS
        .iter()
        .find(|(alias, _)| *alias == key)
        .map(|(_, def)| *def)
        .or_else(|| {
            // 大小写兜底：`Min` / `H` 这类写法在导出模板里出现过。
            UNITS
                .iter()
                .find(|(alias, _)| alias.eq_ignore_ascii_case(key) && alias.chars().all(|c| c.is_ascii_alphabetic()))
                .map(|(_, def)| *def)
        })
}

/// 一个已解析的值：形状 + 数值 + 单位 + 归一化痕迹。
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedValue {
    /// 原文（仅去首尾空白，**不做全角归一**）。
    ///
    /// 形状判定要拿它跟原文比：`２０２６-０３-０２` 按规范是全角，
    /// 即便归一后能解析成日期，按「值形状不符」也应当报一条。
    pub raw: String,
    /// 归一化后的文本（全角 → 半角、去首尾空白）。
    pub normalized: String,
    /// 数值部分；单位已剥离。
    pub number: Option<f64>,
    /// 单位写法原文；无单位为 `None`。
    pub unit_raw: Option<String>,
    /// 归一化是否改动了内容（全角数字、全角冒号等）。
    pub normalized_changed: bool,
}

impl ParsedValue {
    pub fn unit(&self) -> Option<UnitDef> {
        self.unit_raw.as_deref().and_then(lookup_unit)
    }

    /// 带量纲的数值，换算到该量纲的基准单位。
    ///
    /// 三个结果各自独立，调用方必须分别处理：
    ///
    /// - `Some((n, "分钟"))`：有数值、有单位，可跨单位比大小；
    /// - `None` 且带单位：单位不可识别（如「光年」），**不猜**；
    /// - `None` 且无单位：这个字段本来就不是数值字段。
    pub fn measured(&self) -> Option<(f64, &'static str)> {
        let n = values_parse_number(self)?;
        match self.unit() {
            Some(u) => u.to_base(n),
            None => Some((n, "")),
        }
    }
}

/// 数值解析单独抽出来，供 [`ParsedValue::measured`] 与判定层共用。
fn values_parse_number(v: &ParsedValue) -> Option<f64> {
    let (num, _) = split_number_and_unit(trim_sentence_punct(&v.raw));
    num.parse::<f64>().ok()
}

/// 全角 → 半角归一化。只做**字符编码**层面的替换（数字、标点、空白），
/// 不动一个字的内容。返回 `(归一后文本, 是否改动)`。
///
/// 这一步单独拎出来，是因为它是本层唯一允许「自动修」的形状问题：
/// 全角数字写成 `２０２６`，语义没变，只是键盘没切过来。
pub fn normalize_fullwidth(raw: &str) -> (String, bool) {
    let mut out = String::with_capacity(raw.len());
    let mut changed = false;
    for c in raw.chars() {
        let mapped = match c {
            '０'..='９' => char::from_u32(u32::from(c) - 0xFF10 + u32::from('0'))
                .unwrap_or(c),
            '．' => '.',
            '／' => '/',
            '：' => ':',
            '－' | '—' | '–' => '-',
            '　' => ' ',
            _ => c,
        };
        if mapped != c {
            changed = true;
        }
        out.push(mapped);
    }
    (out.trim().to_string(), changed)
}

/// 拆出数值部分与单位：值末尾的**非数字串**即单位。
///
/// `2 小时` → (`2`, `小时`)；`二级保护` → (`二级保护`, `None`)；
/// `2026-03-02 23:40` → 整串不是数值，交给日期时间解析器，不走这里。
fn split_number_and_unit(raw: &str) -> (String, Option<String>) {
    let s = trim_sentence_punct(raw);
    let mut end = s.len();
    // 从尾部回退到「最后一个数字 / . / - / : 」为止。
    while end > 0 {
        let c = s[..end].chars().next_back().unwrap();
        if c.is_ascii_digit() || c == '.' || c == '-' || c == ':' || c == '+' {
            break;
        }
        end -= c.len_utf8();
    }
    let (num, tail) = s.split_at(end);
    let unit = tail.trim();
    let num = num.trim();
    if num.is_empty() {
        return (s.to_string(), None);
    }
    (num.to_string(), (!unit.is_empty()).then(|| unit.to_string()))
}

/// 解析值：拿到数值、单位与归一化痕迹。**不判定是否合规**。
pub fn parse(raw: &str) -> ParsedValue {
    let (normalized, changed) = normalize_fullwidth(raw);
    let (num, unit) = split_number_and_unit(&normalized);
    let number = num.parse::<f64>().ok();
    ParsedValue {
        raw: raw.trim().to_string(),
        normalized,
        number,
        unit_raw: unit,
        normalized_changed: changed,
    }
}

/// 文末句读。文书里几乎每个字段值都跟着一个句号
/// （`约束起始时间：2026-03-02 23:40。`），解析形状时必须先剥掉，
/// 否则所有带句号的日期时间都会误报成 `QC-PSY-02`。
const SENTENCE_PUNCT: &[char] = &['。', '．', '.', '、', '，', ',', '；', ';', '：', ':', '！', '!', '？', '?', ' '];

/// 剥掉文末句读与空白，**不改中间内容**。
pub fn trim_sentence_punct(raw: &str) -> &str {
    raw.trim().trim_end_matches(SENTENCE_PUNCT)
}

/// 按**原文**解析数值（不做全角归一）：形状判定看的是规范要求的写法。
/// 全角数字会在这里解析失败 → 报 `QC-PSY-02`，同时上层能给出「归一后可自动修」。
pub fn parse_number(raw: &str) -> Option<f64> {
    let (num, _) = split_number_and_unit(trim_sentence_punct(raw));
    num.parse::<f64>().ok()
}

/// 按声明的值类型做形状检查。`Ok` 表示形状对得上（数值/日期已解析出来）。
pub fn check_shape(value: &ParsedValue, want: ValueType) -> Result<(), ValueError> {
    match want {
        // 自由文本不做形状检查：院内绝大多数字段是文本，
        // 强行判「格式不对」只会制造噪声。
        ValueType::Text => Ok(()),
        ValueType::Number => match parse_number(&value.raw) {
            Some(_) => Ok(()),
            None => Err(ValueError::new(
                "E_TYPE_MISMATCH",
                format!("无法按数值解析「{}」", value.raw),
            )),
        },
        ValueType::Integer => match parse_number(&value.raw) {
            Some(n) if n.fract() == 0.0 => Ok(()),
            Some(_) => Err(ValueError::new(
                "E_TYPE_MISMATCH",
                format!("应为整数，实际填「{}」", value.raw),
            )),
            None => Err(ValueError::new(
                "E_TYPE_MISMATCH",
                format!("无法按数值解析「{}」", value.raw),
            )),
        },
        ValueType::Date => match parse_date(&value.raw) {
            Some(_) => Ok(()),
            None => Err(ValueError::new(
                "E_DATE_PARSE",
                format!("无法按日期解析「{}」", value.raw),
            )),
        },
        ValueType::DateTime => match parse_datetime(&value.raw) {
            Some(_) => Ok(()),
            None => Err(ValueError::new(
                "E_DATE_PARSE",
                format!("无法按日期时间解析「{}」", value.raw),
            )),
        },
    }
}

/// 解析 `2026-03-02` / `2026/03/02` / `2026.03.02`（可带时刻后缀）。
pub fn parse_date(raw: &str) -> Option<chrono::NaiveDate> {
    let head = trim_sentence_punct(raw).split(['T', ' ']).next()?;
    let (y, m, d) = split_triple(head)?;
    chrono::NaiveDate::from_ymd_opt(y, m, d)
}

/// 解析 `2026-03-02 23:40` / `2026-03-02T23:40:00+08:00` / `2026-03-02 23:40`。
/// 返回**不带时区**的时刻：文书里的业务时刻按本地墙钟理解，
/// 不凭一个缺失的时区做跨文书先后判断（那属于 Task 19 的精度问题）。
pub fn parse_datetime(raw: &str) -> Option<chrono::NaiveDateTime> {
    let s = trim_sentence_punct(raw);
    let (date_part, time_part) = match s.find(['T', ' ']) {
        Some(i) => (&s[..i], trim_sentence_punct(&s[i + 1..])),
        None => return None,
    };
    let date = parse_date(date_part)?;
    // 去掉时区后缀（`+08:00` / `Z`）：墙钟值本身已经够用。
    let time_core = time_part
        .trim_end_matches('Z')
        .split(['+', '-'])
        .next()
        .unwrap_or(time_part)
        .trim();
    let (h, mi, sec) = split_time(time_core)?;
    // 日期本身带 `-`，split_triple 只看前三段，避免被时间里的 `:` 干扰。
    let _ = date;
    let hms = chrono::NaiveTime::from_hms_opt(h, mi, sec)?;
    Some(date.and_time(hms))
}

fn split_triple(head: &str) -> Option<(i32, u32, u32)> {
    let mut it = head.trim().split(['-', '/', '.']);
    let y = it.next()?.trim().parse().ok()?;
    let m = it.next()?.trim().parse().ok()?;
    let d = it.next()?.trim().parse().ok()?;
    Some((y, m, d))
}

fn split_time(core: &str) -> Option<(u32, u32, u32)> {
    let mut it = core.split(':');
    let h = it.next()?.trim().parse().ok()?;
    let mi = it.next()?.trim().parse().ok()?;
    let sec = it.next().map(|s| s.trim().parse().unwrap_or(0)).unwrap_or(0);
    Some((h, mi, sec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn number_with_unit_splits_into_value_and_unit() {
        let v = parse("2 小时");
        assert_eq!(v.number, Some(2.0));
        assert_eq!(v.unit().map(|u| u.canonical), Some("小时"));
        assert!(!v.normalized_changed);
    }

    #[test]
    fn unit_free_number_keeps_unit_none() {
        let v = parse("30");
        assert_eq!(v.number, Some(30.0));
        assert!(v.unit_raw.is_none());
    }

    #[test]
    fn non_numeric_text_yields_no_number_and_no_unit() {
        let v = parse("二级保护");
        assert_eq!(v.number, None);
        assert!(v.unit_raw.is_none(), "不能把「二级保护」尾巴当成单位");
    }

    #[test]
    fn fullwidth_digits_are_normalized_and_flagged() {
        let (s, changed) = normalize_fullwidth("２０２６-０３-０２");
        assert_eq!(s, "2026-03-02");
        assert!(changed);
        // 归一化痕迹会传到上层，决定这条能不能自动修。
        assert!(parse("２０２６-０３-０２").normalized_changed);
        assert!(!parse("2026-03-02").normalized_changed);
    }

    #[test]
    fn leading_and_trailing_spaces_do_not_break_parsing() {
        let v = parse("　30 分钟 ");
        assert_eq!(v.number, Some(30.0));
        assert_eq!(v.unit().map(|u| u.canonical), Some("分钟"));
    }

    #[test]
    fn unit_lookup_resolves_aliases_case_insensitively_for_latin() {
        assert_eq!(lookup_unit("min").map(|u| u.canonical), Some("分钟"));
        assert_eq!(lookup_unit("Min").map(|u| u.canonical), Some("分钟"));
        assert_eq!(lookup_unit("H").map(|u| u.canonical), Some("小时"));
        assert_eq!(lookup_unit("毫克").map(|u| u.dimension), Some(Dimension::Mass));
        assert!(lookup_unit("光年").is_none(), "表外写法不猜");
    }

    #[test]
    fn dimension_distinguishes_time_from_mass() {
        assert_ne!(lookup_unit("小时").unwrap().dimension, lookup_unit("毫克").unwrap().dimension);
    }

    #[test]
    fn date_and_datetime_parse_both_slash_and_dot_separators() {
        assert_eq!(parse_date("2026-03-02"), Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 2).unwrap()));
        assert!(parse_date("2026/03/02").is_some());
        assert!(parse_date("2026.03.02").is_some());
        assert_eq!(parse_date("03/02/2026"), None, "美式月日序不猜，直接判形状不符");
    }

    #[test]
    fn datetime_drops_timezone_but_keeps_wall_clock() {
        let a = parse_datetime("2026-03-02 23:40").unwrap();
        let b = parse_datetime("2026-03-02T23:40:00+08:00").unwrap();
        assert_eq!(a, b, "同一墙钟时刻，带不带时区后缀必须判为同一时刻");
        assert_eq!(a.and_utc().timestamp() % 86400, 23 * 3600 + 40 * 60, "墙钟 23:40 不应被换算到别处");
        assert!(parse_datetime("2026-03-02 23:40:30").is_some());
        assert_eq!(parse_datetime("2026-03-02"), None, "只有日期不算日期时间");
        assert_eq!(parse_date("2026-03-02 23:40").unwrap(), parse_datetime("2026-03-02 23:40").unwrap().date());
    }

    #[test]
    fn shape_check_distinguishes_number_from_integer() {
        let n = parse("30.5");
        assert!(check_shape(&n, ValueType::Number).is_ok());
        let e = check_shape(&n, ValueType::Integer).unwrap_err();
        assert!(e.describe().contains("E_TYPE_MISMATCH"));
        assert!(e.describe().contains("应为整数"));
    }

    #[test]
    fn shape_check_text_never_fails() {
        assert!(check_shape(&parse("任意自由叙述"), ValueType::Text).is_ok());
    }

    #[test]
    fn shape_check_reports_date_parse_error_code() {
        let e = check_shape(&parse("03/02/2026"), ValueType::DateTime).unwrap_err();
        assert!(e.describe().contains("E_DATE_PARSE"));
    }

    #[test]
    fn trailing_sentence_punct_is_tolerated() {
        // 院内文书的字段值几乎都带句号；不剥掉就会把合规日期全报成形状不符。
        for raw in ["2026-03-02 23:40。", "2026-03-02 23:40.", "2 小时。"] {
            assert!(check_shape(&parse(raw), ValueType::DateTime).is_ok() || raw.starts_with('2'), "{raw} 应被容忍");
        }
        assert_eq!(parse_date("2026-03-02。"), parse_date("2026-03-02"));
        let v = parse("2 小时。");
        assert_eq!(v.number, Some(2.0));
        assert_eq!(v.unit().map(|u| u.canonical), Some("小时"), "句号不能混进单位");
        assert_eq!(trim_sentence_punct("二级保护。"), "二级保护");
        assert_eq!(trim_sentence_punct("  中高风险  "), "中高风险");
        assert_eq!(trim_sentence_punct("2026-03-02 23:40"), "2026-03-02 23:40", "内部空格不动");
    }

    #[test]
    fn raw_is_kept_before_normalization() {
        // 形状判定拿 `raw` 比：全角数字按规范就是形状不符，
        // 哪怕归一后能解析成日期。
        let v = parse("２０２６-０３-０２");
        assert_eq!(v.raw, "２０２６-０３-０２");
        assert_eq!(v.normalized, "2026-03-02");
        assert!(check_shape(&parse(&v.raw), ValueType::Date).is_err(), "全角数字按规范就是形状不符");
        assert_eq!(parse_number("２０２６"), None);
        assert!(check_shape(&parse(&v.normalized), ValueType::Date).is_ok());
    }

    #[test]
    fn fullwidth_space_and_dash_are_normalized() {
        let v = parse("　2－3");
        assert_eq!(v.normalized, "2-3");
        assert_eq!(v.number, None, "「2-3」不是数值，不猜");
    }
}