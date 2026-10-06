//! `psychdoc-parse`：文书包解析 —— 输入一份 FHIR R4 `Bundle`（JSON），
//! 输出**件 → 字段表 → 时间戳集合**三层结构。
//!
//! ## 源产品主能力与改写
//!
//! 源产品的主路径是 `parse(source, options) -> Result<Parsed<Mod>, ParseError>`
//! （`github_ref/ruff/crates/ruff_python_parser/src/lib.rs:281`），
//! 关键行为是**解析失败也返回带位置的错误**：
//! `ParseError { error: ParseErrorType, location: TextRange }`
//! （`error.rs:11-14`，`Display` 打印 `"{error} at byte range {location}"`）。
//!
//! 本任务把这条语义**改写**为文书包解析：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | `Result<Parsed<Mod>, ParseError>` | `ParsedBundle { items, errors }`——**部分成功** |
//! | `TextRange` 字节区间 | `Span` 字符区间 + `DocLoc(doc_id, section_index, char_range)` |
//! | `ParseError` 在整文件解析失败时返回 | 字段/段级问题也产出带 `Span` 的诊断，**不静默丢字段** |
//! | 字节下标（`ruff_text_size`） | 字符下标，保证生僻字与卡片高亮同口径 |
//!
//! 调用方（`psychdoc-rules` / CLI）拿到的永远是 `ParsedBundle`：
//! 能解析的件照常进 `items`，坏处进 `errors`，**整批不失败**。

use psychdoc_model::{
    Binding, Diagnostic, DocItem, DocLoc, DocTypeRegistry, Field, RuleMessage, Section, Span,
    Timestamp, TimestampKind, Precision,
};
use serde::Deserialize;

pub mod discovery;
pub mod profile;
pub mod queue;
pub mod synth;
pub mod timeline;
pub mod tolerance;

pub use discovery::{
    discover_dir, discover_dir_report, discover_dir_report_with_registry, discover_paths_report,
    filter_by_registry, parse_package, parse_package_bytes, parse_package_bytes_with,
    parse_package_with, BindingOutcome, DiscoverError, DiscoveredPackage, DiscoveryReport,
    EntityBinder, FileSkip, FileSkipReason, FilteredEntry, ManualQueueEntry, SkipReason,
};
pub use profile::{
    peak_rss_bytes, ScaleMeasurement, SegmentProfile, Stage, StageTimer,
};
pub use queue::{
    aggregate, alias_of_missing, plan_batch, ward_of, QueueEntry, QueueInput, ReviewQueue,
};
pub use timeline::{build_timeline, TimelineEntry, TimelineOrder};
pub use tolerance::{
    decode_export_bytes, detect_duplicate_ids, parse_export, split_long_section, Decoded,
    EncodingNote, SourceEncoding, LONG_SECTION_CHARS, REPLACEMENT_CHAR,
};

/// 解析产物：成功的件 + 失败处（部分成功）。
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct ParsedBundle {
    pub items: Vec<DocItem>,
    pub errors: Vec<Diagnostic>,
    /// 进入解析的 `Composition` entry 总数。
    ///
    /// 记它是为了让 [`ParsedBundle::is_balanced`] 能查「有没有件被静默吞掉」。
    /// 少了这个数，「未解析 N 件」就只是一个能被随便填的数字。
    #[serde(default)]
    pub entries_seen: usize,
}

impl ParsedBundle {
    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.errors.is_empty()
    }

    /// 零静默丢弃的不变量。
    ///
    /// `成功件数 + 整件不可用件数 == 看到的 entry 数`。注意
    /// 「整件不可用」只能数**整件**的失败（类型码缺失 / 无住院别名 /
    /// 日期写错 / id 重复），不能数 `errors.len()`——段级问题（text 为 null、
    /// 超长段切分）不会让件离开 `items`，把它们算进去会把这个不变量算假。
    pub fn is_balanced(&self, wholly_unresolved: usize) -> bool {
        self.entries_seen == self.items.len() + wholly_unresolved
    }
}

// ---------------------------------------------------------------------------
// FHIR R4 子集：只声明本产品实际读取的字段（spec.md §2 最小集）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
pub struct Bundle {
    #[serde(default, rename = "resourceType")]
    pub resource_type: String,
    #[serde(default)]
    pub entry: Vec<BundleEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BundleEntry {
    #[serde(default)]
    pub resource: serde_json::Value,
}

/// 本产品处理的资源类型：文书正文是 `Composition`。
#[derive(Debug, Clone, Deserialize)]
pub struct Composition {
    pub id: String,
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default)]
    pub identifier: Vec<Identifier>,
    #[serde(rename = "type")]
    pub doc_type: CodeableConcept,
    #[serde(default)]
    pub author: Vec<Reference>,
    #[serde(default)]
    pub title: Option<String>,
    /// FHIR R4 `Composition` 无 `authoredOn` 字段，但院内导出普遍在
    /// **业务侧**带上书写时刻（本产品按院内约定放在 resource 顶层，
    /// 也接受 `DocumentReference.date` 形式）。抽不出即 missing，不兜底。
    #[serde(default, rename = "authoredOn")]
    pub authored_on: Option<String>,
    #[serde(default)]
    pub section: Vec<CompositionSection>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Identifier {
    #[serde(default)]
    pub system: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CodeableConcept {
    #[serde(default)]
    pub coding: Vec<Coding>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Coding {
    #[serde(default)]
    pub code: Option<String>,
    #[serde(default)]
    pub display: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Reference {
    #[serde(default)]
    pub reference: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CompositionSection {
    #[serde(default)]
    pub title: Option<String>,
    /// `null` 文本是真实存在的脏数据（Task 22 覆盖）：此处退化为空串，
    /// 保留条目并由调用方的必填类规则按「未填」处理，而不是 panic。
    #[serde(default)]
    pub text: Option<String>,
}

/// 解析错误码。全部可读，无一是静默丢弃。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParseError {
    #[error("E_BUNDLE_JSON: 文书包不是合法 JSON — {detail}")]
    BundleJson { detail: String },
    #[error("E_BUNDLE_SHAPE: 顶层不是 FHIR Bundle（resourceType={got}）")]
    BundleShape { got: String },
    #[error("E_DOC_TYPE_MISSING: 文书 {doc_id} 缺 type.coding，无法判定文书类型")]
    DocTypeMissing { doc_id: String },
    #[error("E_UNKNOWN_DOC_TYPE: 文书 {doc_id} 的类型码 {code} 不在院内文书类型字典内")]
    UnknownDocType { doc_id: String, code: String },
    #[error("E_ENCOUNTER_ALIAS_MISSING: 文书 {doc_id} 无住院别名（identifier），无法跨件 join")]
    EncounterAliasMissing { doc_id: String },
    #[error("E_DATE_PARSE: 文书 {doc_id} 的 date 非 ISO8601 — {value}")]
    DateParse { doc_id: String, value: String },
}

/// 解析期诊断码。**与规则码（QC-PSY-nn）分开**：解析期问题说的是
/// 「这份包读不动」，规则码说的是「这份文书不合规」，二者不得混用。
pub mod parse_code {
    /// 结构性问题（类型码缺失、段文本为 null、非 Composition entry）。
    pub const STRUCTURE: &str = "PARSE-STRUCTURE";
    /// 编码问题（非法字节已替换为占位符，见 `tolerance`）。
    pub const ENCODING: &str = "E_ENCODING";
    /// 同一份包内 `doc_id` 重复，全部交人工。
    pub const DUPLICATE: &str = crate::tolerance::DUPLICATE_CODE;
}

/// 解析期诊断的「依据」：它不是院内规范问题，而是接口规范问题。
const PARSE_BASIS: &str = "FHIR R4 Composition 资源结构规范（spec.md §2 字段要求）";

pub(crate) fn parse_diag(code: &str, loc: DocLoc, text: impl Into<String>) -> Diagnostic {
    let msg = RuleMessage::new(text, PARSE_BASIS).expect("依据为非空常量");
    // 解析期问题一律不可自动补写：结构错了要人改包，不是程序替他补。
    Diagnostic::new(code, loc, msg).without_fix()
}

// ---------------------------------------------------------------------------
// 主入口
// ---------------------------------------------------------------------------

/// 解析一份文书包。`registry` 为院内文书类型字典，传入则按模板把段内
/// `键：值` 行回填为带必填性的字段表；不传则只回填 `required=false` 的字段。
pub fn parse_bundle(
    json: &str,
    registry: Option<&DocTypeRegistry>,
) -> Result<ParsedBundle, ParseError> {
    let bundle: Bundle = serde_json::from_str(json)
        .map_err(|e| ParseError::BundleJson { detail: e.to_string() })?;
    if bundle.resource_type != "Bundle" {
        return Err(ParseError::BundleShape { got: bundle.resource_type });
    }
    let mut out = ParsedBundle::default();
    for (i, entry) in bundle.entry.iter().enumerate() {
        // 非 Composition 的 entry（如 DocumentReference / Provenance）本轮
        // 显式跳过并记账，不静默丢弃。
        if entry.resource.get("resourceType").and_then(|v| v.as_str()) != Some("Composition") {
            let doc_id = entry
                .resource
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("<无 id>")
                .to_string();
            out.errors.push(parse_diag(
                parse_code::STRUCTURE,
                DocLoc::new(doc_id, i as u32, Span::unchecked(0, 0)),
                format!("entry[{i}] 资源类型非 Composition，本轮只解析文书正文件"),
            ));
            continue;
        }
        out.entries_seen += 1;
        let comp: Composition = serde_json::from_value(entry.resource.clone()).map_err(|e| {
            ParseError::BundleJson {
                detail: format!("entry[{i}] Composition 解析失败 — {e}"),
            }
        })?;
        match parse_composition(&comp, registry) {
            Ok((item, diags)) => {
                out.items.push(item);
                out.errors.extend(diags);
            }
            Err(e) => {
                // 单件解析失败不拖垮整批：产出带位置的诊断，件不进入 items。
                out.errors.push(parse_diag(
                    parse_code::STRUCTURE,
                    DocLoc::new(comp.id.clone(), 0, Span::unchecked(0, 0)),
                    e.to_string(),
                ));
            }
        }
    }
    // 重复 doc_id：全部交人工，**不合并也不取舍**（Task 22）。
    // 必须在整批 entry 都进 `items` 之后做——`Composition.id` 是本产品的
    // 权威实体键，跨 entry 比对才有意义。
    let (kept, dup_diags) = detect_duplicate_ids(std::mem::take(&mut out.items));
    out.items = kept;
    out.errors.extend(dup_diags);
    Ok(out)
}

fn parse_composition(
    comp: &Composition,
    registry: Option<&DocTypeRegistry>,
) -> Result<(DocItem, Vec<Diagnostic>), ParseError> {
    let template_id = comp
        .doc_type
        .coding
        .iter()
        .find_map(|c| c.code.clone())
        .ok_or_else(|| ParseError::DocTypeMissing { doc_id: comp.id.clone() })?;

    if let Some(r) = registry {
        if !r.is_known(&template_id) {
            return Err(ParseError::UnknownDocType {
                doc_id: comp.id.clone(),
                code: template_id,
            });
        }
    }

    // 住院别名：identifier 中 system 指向 encounter 的那条。
    // **禁止按文件名或标题推断**——权威键只从 identifier 来。
    let encounter_alias = comp
        .identifier
        .iter()
        .find(|i| i.system.as_deref().is_some_and(|s| s.ends_with("/encounter")))
        .and_then(|i| i.value.clone())
        .ok_or_else(|| ParseError::EncounterAliasMissing { doc_id: comp.id.clone() })?;

    let mut diags = Vec::new();
    if comp.section.is_empty() {
        diags.push(parse_diag(
            parse_code::STRUCTURE,
            DocLoc::new(comp.id.clone(), 0, Span::unchecked(0, 0)),
            "文书没有任何 section，字段表为空",
        ));
    }

    // 权威系统：住院别名那条 identifier 的 system 主机段（如 `example-hospital`）。
    // 同一文书在 HIS / EMR / 护理系统各有一份导出时，靠它判「同名不同源」。
    let source_system = comp
        .identifier
        .iter()
        .find(|i| i.system.as_deref().is_some_and(|s| s.ends_with("/encounter")))
        .and_then(|i| i.system.as_deref())
        .map(system_authority)
        .unwrap_or_default();

    // 段 → 字段表
    //
    // 段号在**切分之后**才定：超长段会按段落边界拆成多块（Task 22），
    // 拆出来的块各自占一个段号，字段区间在块内重新计算——char_range 口径
    // 始终是「在它所属段的文本内的字符下标」，拆段不改变这个约定。
    let mut sections: Vec<Section> = Vec::new();
    let mut fields = Vec::new();
    for (idx, sec) in comp.section.iter().enumerate() {
        let origin = u32::try_from(idx).unwrap_or(u32::MAX);
        let text = sec.text.clone().unwrap_or_default();
        if sec.text.is_none() {
            diags.push(parse_diag(
                parse_code::STRUCTURE,
                DocLoc::new(comp.id.clone(), origin, Span::unchecked(0, 0)),
                format!(
                    "第 {} 段「{}」text 为 null，按空段处理并保留条目",
                    origin,
                    sec.title.clone().unwrap_or_default()
                ),
            ));
        }
        let title = sec.title.clone().unwrap_or_else(|| format!("段落{origin}"));
        let chunks = split_long_section(&title, &text, LONG_SECTION_CHARS);
        if chunks.len() > 1 {
            diags.push(parse_diag(
                parse_code::STRUCTURE,
                DocLoc::new(comp.id.clone(), origin, Span::unchecked(0, 0)),
                format!(
                    "第 {} 段「{}」共 {} 字，超过单段 {} 字上限，已按段落边界切成 {} 块；\
                     内容零丢失，各块段号顺延",
                    origin,
                    title,
                    text.chars().count(),
                    LONG_SECTION_CHARS,
                    chunks.len()
                ),
            ));
        }
        for (chunk_title, chunk_text) in chunks {
            let index = u32::try_from(sections.len()).unwrap_or(u32::MAX);
            fields.extend(extract_fields(index, &chunk_text, registry, &template_id));
            sections.push(Section {
                index,
                title: chunk_title,
                text: chunk_text,
            });
        }
    }

    // 三类时间戳。**抽不出就标 missing，绝不填当前时间或零值** ——
    // 兜底会制造出「看起来有时间序、其实是我们编的」结论。
    let mut timestamps = Vec::new();
    match comp.date.as_deref() {
        Some(raw) => {
            let (dt, precision) = parse_date_time(raw).ok_or_else(|| ParseError::DateParse {
                doc_id: comp.id.clone(),
                value: raw.to_string(),
            })?;
            timestamps.push(Timestamp::at(TimestampKind::Date, dt, precision, 0, None));
        }
        None => {
            diags.push(parse_diag(
                parse_code::STRUCTURE,
                DocLoc::new(comp.id.clone(), 0, Span::unchecked(0, 0)),
                "文书无 date，时间序无法建立，保留条目交人工",
            ));
            timestamps.push(Timestamp::missing(TimestampKind::Date, 0, None));
        }
    }

    // 书写时刻 authoredOn：院内导出常带，抽不出即 missing。
    let authored = comp.authored_on.as_deref().and_then(|raw| {
        parse_date_time(raw).map(|(dt, precision)| {
            Timestamp::at(TimestampKind::AuthoredOn, dt, precision, 0, None)
        })
    });
    timestamps.push(authored.unwrap_or_else(|| {
        Timestamp::missing(TimestampKind::AuthoredOn, 0, comp.authored_on.clone())
    }));

    // 段落内业务时刻：只认**带日期**的时刻（`2026-03-02 23:40` 这类）。
    // 只写了「23时40分」的段落文本**不算时刻**——没有日期就无法定序，
    // 按 missing 记（Task 19 的跨零点与精度降级在此只做取样，不下结论）。
    for sec in &sections {
        for hit in scan_business_moments(&sec.text) {
            let ok = chrono::NaiveDateTime::parse_from_str(&hit.raw, "%Y-%m-%d %H:%M")
                .or_else(|_| chrono::NaiveDateTime::parse_from_str(&hit.raw, "%Y-%m-%d %H:%M:%S"));
            let ts = match ok {
                Ok(naive) => {
                    // 院内业务时刻按东八区解释；固定偏移无夏令时歧义。
                    let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 恒有效");
                    let dt = chrono::TimeZone::from_local_datetime(&offset, &naive).single();
                    match dt {
                        Some(dt) => Timestamp::at(
                            TimestampKind::BusinessMoment,
                            dt,
                            if hit.raw.len() == 16 { Precision::Minute } else { Precision::Second },
                            sec.index,
                            Some(Span::unchecked(hit.start, hit.end)),
                        ),
                        None => Timestamp::missing(
                            TimestampKind::BusinessMoment,
                            sec.index,
                            Some(hit.raw.clone()),
                        ),
                    }
                }
                Err(_) => Timestamp::missing(
                    TimestampKind::BusinessMoment,
                    sec.index,
                    Some(hit.raw.clone()),
                ),
            };
            timestamps.push(ts);
        }
        // 只有时分的写法（「23时45分」「23:45」）——**没有日期就不能定序**。
        // 但原文必须留下：护士长要看到「这段写了 23:45，只是没日期」，
        // 而不是这条时间信息在解析时凭空消失。
        for hit in scan_time_only_moments(&sec.text) {
            timestamps.push(Timestamp::missing(
                TimestampKind::BusinessMoment,
                sec.index,
                Some(hit.raw),
            ));
        }
    }

    let item = DocItem::new(&comp.id, &template_id, &encounter_alias)
        .with_source_system(source_system)
        .with_binding(Binding::Confident)
        .with_sections(sections)
        .with_fields(fields)
        .with_timestamps(timestamps);
    Ok((item, diags))
}

/// 从 `identifier.system` URL 抽权威系统名（`https://h.invalid/fhir/...` → `h`）。
fn system_authority(system: &str) -> String {
    let host = system
        .split("://")
        .nth(1)
        .unwrap_or(system)
        .split('/')
        .next()
        .unwrap_or_default();
    // 只取第一段标签：`<authority>.example-hospital.invalid` → `<authority>`
    host.split('.').next().unwrap_or_default().to_string()
}

/// 解析 FHIR `dateTime`，返回 `(时刻, 精度)`。
///
/// FHIR R4 的 `dateTime` 允许**只到日**（`2026-03-11`）与**只到小时**
/// （`2026-03-11T08`）。院内导出确实会出现这两种：签署页只印一个日期、
/// 值班表只写「8 时」。因此这里不是把它们当坏值丢掉，而是**照实标出精度**
/// —— 粗粒度是业务事实，规则层据此决定这个时刻能不能用来判违规
/// （见 `psychdoc_rules::chronology` 的「低精度只参与单向判断」）。
///
/// 时区按东八区解释（院内业务时刻都是本地时间）；固定偏移无夏令时歧义。
fn parse_date_time(raw: &str) -> Option<(chrono::DateTime<chrono::FixedOffset>, Precision)> {
    use chrono::TimeZone;
    let offset = chrono::FixedOffset::east_opt(8 * 3600).expect("UTC+8 恒有效");
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(raw) {
        return Some((dt, precision_of_rfc3339(raw)));
    }
    if let Ok(naive) = chrono::NaiveDate::parse_from_str(raw, "%Y-%m-%d") {
        let at = offset.from_local_datetime(&naive.and_hms_opt(0, 0, 0)?).single()?;
        return Some((at, Precision::Day));
    }
    if let Ok(naive) = chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%dT%H") {
        let at = offset.from_local_datetime(&naive).single()?;
        return Some((at, Precision::Hour));
    }
    None
}

/// RFC3339 字面量的实际精度：只到分 / 只到秒。
fn precision_of_rfc3339(raw: &str) -> Precision {
    let t = raw.split('T').nth(1).unwrap_or("");
    let (hms, _) = t.split_once(['+', '-']).unwrap_or((t, ""));
    if hms.split(':').count() >= 3 {
        Precision::Second
    } else {
        Precision::Minute
    }
}

/// 段内一次业务时刻命中。`start` / `end` 为**字符**下标，可直接高亮。
struct MomentHit {
    raw: String,
    start: u32,
    end: u32,
}

/// 扫出段内形如 `2026-03-02 23:40[:SS]` 的时刻字面量。
/// **只认带日期的**：无日期的时刻无法定序，记 missing 而非硬凑。
fn scan_business_moments(text: &str) -> Vec<MomentHit> {
    const DATE_CHARS: usize = 10; // YYYY-MM-DD
    const MIN_FORM: usize = 16; // + ' HH:MM'
    const SEC_FORM: usize = 19; // + ':SS'
    // 形态判定按字符序列走，但**产出的区间必须是字符下标**——
    // 卡片高亮按 char_range 在段文本上切，用字节下标会切到半个生僻字。
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i + MIN_FORM <= chars.len() {
        let head: String = chars[i..i + DATE_CHARS].iter().collect();
        if !is_date_shaped(&head) {
            i += 1;
            continue;
        }
        let tail: String = chars[i + DATE_CHARS..i + MIN_FORM].iter().collect();
        if !is_min_shaped(&tail) {
            i += 1;
            continue;
        }
        let (len, next_ok) = match chars.get(i + MIN_FORM) {
            Some(':') => match chars.get(i + SEC_FORM) {
                Some(':') => (SEC_FORM, true),
                _ => (MIN_FORM, false),
            },
            _ => (MIN_FORM, true),
        };
        // 秒位存在时必须形如 `HH:MM:SS`，否则（如 `23:40:3`）不认。
        if len == SEC_FORM {
            let secs: String = chars[i + MIN_FORM + 1..i + SEC_FORM].iter().collect();
            if !is_sec_shaped(&secs) {
                i += 1;
                continue;
            }
        }
        out.push(MomentHit {
            raw: chars[i..i + len].iter().collect(),
            start: u32::try_from(i).unwrap_or(u32::MAX),
            end: u32::try_from(i + len).unwrap_or(u32::MAX),
        });
        i += len;
        if !next_ok {
            i += 1;
        }
    }
    out
}

fn is_date_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
        && b[7] == b'-'
        && b[8..10].iter().all(u8::is_ascii_digit)
}

fn is_min_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 6
        && (b[0] == b' ' || b[0] == b'T')
        && b[1..3].iter().all(u8::is_ascii_digit)
        && b[3] == b':'
        && b[4..6].iter().all(u8::is_ascii_digit)
}

fn is_sec_shaped(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 2 && b.iter().all(u8::is_ascii_digit)
}

/// 扫出**只有时分、没有日期**的时刻写法：中文「23时45分」「2时10分」
/// 与半角「23:45」。这类字面量无法定序（跨零点判定属 Task 19），
/// 本轮只把原文留下来，不造一个假时刻。
///
/// 会跳过已经被 [`scan_business_moments`] 认领的区间（带日期的那批）。
fn scan_time_only_moments(text: &str) -> Vec<MomentHit> {
    let chars: Vec<char> = text.chars().collect();
    let taken: Vec<(usize, usize)> = scan_business_moments(text)
        .into_iter()
        .map(|h| (h.start as usize, h.end as usize))
        .collect();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < chars.len() {
        // 跳到已认领区间的末尾（带日期的批次由上面的扫描负责）
        if let Some((_, e)) = taken.iter().find(|(s, e)| i >= *s && i < *e).copied() {
            i = e;
            continue;
        }
        // 中文式：`23时45分`
        if let Some(len) = zh_time_len(&chars, i) {
            out.push(MomentHit {
                raw: chars[i..i + len].iter().collect(),
                start: u32::try_from(i).unwrap_or(u32::MAX),
                end: u32::try_from(i + len).unwrap_or(u32::MAX),
            });
            i += len;
            continue;
        }
        // 半角式：`23:45`（后面不再跟秒位——带秒的会被带日期的扫描先吃掉）
        if i + 5 <= chars.len()
            && chars[i].is_ascii_digit()
            && chars[i + 1].is_ascii_digit()
            && chars[i + 2] == ':'
            && chars[i + 3].is_ascii_digit()
            && chars[i + 4].is_ascii_digit()
            && chars.get(i + 5).is_none_or(|c| *c != ':')
        {
            out.push(MomentHit {
                raw: chars[i..i + 5].iter().collect(),
                start: u32::try_from(i).unwrap_or(u32::MAX),
                end: u32::try_from(i + 5).unwrap_or(u32::MAX),
            });
            i += 5;
            continue;
        }
        i += 1;
    }
    out
}

/// 中文时刻 `23时45分` / `2时10分` / `23时` 的字符长度；不匹配返回 `None`。
fn zh_time_len(chars: &[char], i: usize) -> Option<usize> {
    let h1 = *chars.get(i)?;
    let h2 = *chars.get(i + 1)?;
    if !(h1.is_ascii_digit() && h2.is_ascii_digit()) {
        return None;
    }
    if chars.get(i + 2) != Some(&'时') {
        return None;
    }
    match chars.get(i + 5) {
        Some('分') => Some(6), // 23时45分
        None => Some(3),        // 23时（只到时）
        _ => None,
    }
}

/// 段内 `键：值` 行 → 字段表。字段值区间为**字符**下标（与 Span 口径一致）。
///
/// 识别两种分隔符：`全角冒号 ：`（院内手写模板的常见写法）与半角 `:`。
/// 值区间只覆盖值本身，不含首尾空白；空值 → 零宽区间落在冒号之后。
fn extract_fields(
    section_index: u32,
    text: &str,
    registry: Option<&DocTypeRegistry>,
    template_id: &str,
) -> Vec<Field> {
    let mut out = Vec::new();
    // 行首在**整段文本**中的字符下标：字段区间必须是段内定位，不是行内定位
    // （卡片高亮按 DocLoc.char_range 在段文本上切，行内下标会高亮到错误位置）。
    let mut line_start: u32 = 0;
    for line in text.split('\n') {
        let line_chars = u32::try_from(line.chars().count()).unwrap_or(u32::MAX);
        match find_key_sep(line) {
            Some(colon) => {
                // 分隔符同时需要两个数：字节数（用于切片）与**字符数**（用于区间换算）。
                // 全角「：」占 3 字节但只算 1 个字符——混用会把高亮推到错的字上。
                let sep_bytes = line[colon..].chars().next().map(char::len_utf8).unwrap_or(0);
                let sep_chars = 1u32;
                let key = line[..colon].trim();
                let rest = &line[colon + sep_bytes..];
                if key.is_empty() {
                    // 空键行不是字段，但仍要留痕，否则文书内容凭空消失。
                    out.push(untitled(section_index, rest.trim()));
                    line_start += line_chars + 1; // +1 换行符
                    continue;
                }
                let value = rest.trim();
                // 值起始 = 行首 + 键长 + 分隔符字符宽 + 值的前导空白
                let lead_chars = u32::try_from(
                    rest[..rest.len() - rest.trim_start().len()].chars().count(),
                )
                .unwrap_or(0);
                let key_chars = u32::try_from(line[..colon].chars().count()).unwrap_or(0);
                let value_start = line_start + key_chars + sep_chars + lead_chars;
                let value_len = u32::try_from(value.chars().count()).unwrap_or(0);
                let required = registry
                    .and_then(|r| r.template(template_id))
                    .and_then(|t| t.field(key))
                    .map(|f| f.required)
                    .unwrap_or(false);
                out.push(Field {
                    key: key.to_string(),
                    value: Some(value.to_string()),
                    required,
                    source_span: Some(Span::unchecked(value_start, value_start + value_len)),
                    section_index,
                });
            }
            None => {
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    // 自由叙述行不是字段，但它仍是文书内容：保留为无来源区间的条目，
                    // 不静默吞掉（否则卡片渲染会缺段）。
                    out.push(untitled(section_index, trimmed));
                }
            }
        }
        line_start += line_chars + 1; // +1 换行符
    }
    out
}

/// 非键值行的占位条目：无 `source_span`（= 字段表意义上的 Missing），
/// 语义上区别于「键存在但值为空」。
fn untitled(section_index: u32, text: &str) -> Field {
    Field {
        key: format!("自由文本@{section_index}"),
        value: (!text.is_empty()).then(|| text.to_string()),
        required: false,
        source_span: None,
        section_index,
    }
}

/// 找 `键：值` 的分隔符：优先全角 `：`，其次半角 `:`。返回**字节**下标。
fn find_key_sep(line: &str) -> Option<usize> {
    if let Some(i) = line.find('：') {
        return Some(i);
    }
    line.char_indices().find(|(_, c)| *c == ':').map(|(i, _)| i)
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::FieldPresence;

    const FIXTURE: &str = include_str!("../../../../fixtures/ward-01-minimal.json");

    fn registry() -> DocTypeRegistry {
        serde_yaml::from_str(
            r#"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 约束事由
        section: 约束事由
        required: true
      - key: 患方意见
        section: 患方知情同意
        required: true
      - key: 约束起始时间
        section: 患方知情同意
        required: true
      - key: 患方签字
        section: 患方知情同意
        required: true
"#,
        )
        .unwrap()
    }

    #[test]
    fn parses_fixture_into_single_doc_item() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        assert_eq!(parsed.items.len(), 1, "夹具只有 1 件文书正文");
        assert!(parsed.errors.is_empty(), "样例夹具不应有解析错误: {:?}", parsed.errors);
        let d = &parsed.items[0];
        assert_eq!(d.doc_id, "WARD-A-001-CONSENT-01");
        assert_eq!(d.template_id, "restraint_consent");
        assert_eq!(d.encounter_alias, "WARD-A-001");
        assert_eq!(d.sections.len(), 2);
        assert_eq!(d.sections[1].title, "患方知情同意");
    }

    #[test]
    fn fields_carry_char_ranges_into_their_own_section() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        let d = &parsed.items[0];
        let f = d.field("患方意见").expect("患方意见应被解析出来");
        assert_eq!(f.section_index, 1);
        let sec = &d.sections[f.section_index as usize];
        // 字符区间切出的子串必须正好是字段值 —— 定位错位会让卡片高亮到错处。
        assert_eq!(
            f.source_span.unwrap().slice(&sec.text),
            Some("已知情同意，同意按护理常规采取保护措施。")
        );
        let key = d.field("约束事由").unwrap();
        assert_eq!(
            key.source_span.unwrap().slice(&d.sections[0].text),
            Some("夜间反复冲动，需短时保护。")
        );
    }

    #[test]
    fn required_flag_comes_from_registry_not_guessed() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        let d = &parsed.items[0];
        assert!(d.field("患方签字").unwrap().required);
        assert!(!d.field("保护等级").unwrap().required, "字典外的键不得自带必填性");
    }

    #[test]
    fn empty_value_is_empty_not_missing() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        let d = &parsed.items[0];
        // 「患方签字：」在包里存在但值为空 —— 语义是 Empty，不是 Missing
        let sign = d.field("患方签字").expect("键存在");
        assert_eq!(sign.presence(), FieldPresence::Empty);
        assert!(sign.is_unfilled());
    }

    #[test]
    fn non_key_value_lines_are_kept_not_dropped() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        let d = &parsed.items[0];
        // 保护等级等非模板字段仍要留在字段表里，否则文书内容凭空消失
        assert!(d.field("保护等级").is_some());
    }

    #[test]
    fn date_timestamp_is_extracted_and_authored_on_is_not_faked() {
        let parsed = parse_bundle(FIXTURE, Some(&registry())).unwrap();
        let d = &parsed.items[0];
        assert_eq!(d.timestamps[0].kind, TimestampKind::Date);
        assert!(d.timestamps[0].moment.is_some());
        // authoredOn 不在最小集内 → missing，**不得**兜底成当前时间
        assert_eq!(d.missing_timestamps().len(), 1);
    }

    #[test]
    fn missing_type_coding_is_explicit_error_not_silent_skip() {
        let json = r#"{"resourceType":"Bundle","entry":[{"resource":{
            "resourceType":"Composition","id":"X-1",
            "identifier":[{"system":"https://h.invalid/fhir/identifier/encounter","value":"WARD-X"}],
            "type":{},"section":[]}}]}"#;
        // 单件坏 → 该件不进 items，但整批不失败，错误带码落进 errors
        let parsed = parse_bundle(json, None).unwrap();
        assert!(parsed.items.is_empty());
        assert_eq!(parsed.errors.len(), 1);
        assert!(parsed.errors[0].message.text.contains("E_DOC_TYPE_MISSING"));
        assert_eq!(parsed.errors[0].loc.doc_id, "X-1");
    }

    #[test]
    fn unknown_doc_type_is_rejected_when_registry_known() {
        let json = r#"{"resourceType":"Bundle","entry":[{"resource":{
            "resourceType":"Composition","id":"X-2",
            "identifier":[{"system":"https://h.invalid/fhir/identifier/encounter","value":"WARD-X"}],
            "type":{"coding":[{"code":"mystery_doc"}]},"section":[]}}]}"#;
        let parsed = parse_bundle(json, Some(&registry())).unwrap();
        assert!(parsed.items.is_empty());
        assert!(parsed.errors[0].message.text.contains("E_UNKNOWN_DOC_TYPE"));
    }

    #[test]
    fn encounter_alias_never_guessed_from_filename_or_title() {
        let json = r#"{"resourceType":"Bundle","entry":[{"resource":{
            "resourceType":"Composition","id":"X-3","title":"WARD-Y 住院记录",
            "identifier":[],
            "type":{"coding":[{"code":"restraint_consent"}]},"section":[]}}]}"#;
        let parsed = parse_bundle(json, None).unwrap();
        assert!(parsed.items.is_empty());
        assert!(parsed.errors[0].message.text.contains("E_ENCOUNTER_ALIAS_MISSING"));
    }

    #[test]
    fn bad_date_is_explicit_parse_error() {
        let json = r#"{"resourceType":"Bundle","entry":[{"resource":{
            "resourceType":"Composition","id":"X-4","date":"03/02/2026",
            "identifier":[{"system":"https://h.invalid/fhir/identifier/encounter","value":"WARD-X"}],
            "type":{"coding":[{"code":"restraint_consent"}]},"section":[]}}]}"#;
        let parsed = parse_bundle(json, None).unwrap();
        assert!(parsed.items.is_empty());
        assert!(parsed.errors[0].message.text.contains("E_DATE_PARSE"));
    }

    #[test]
    fn non_composition_entry_is_recorded_not_dropped() {
        let json = r#"{"resourceType":"Bundle","entry":[
            {"resource":{"resourceType":"DocumentReference","id":"DR-1"}},
            {"resource":{"resourceType":"Composition","id":"X-5",
             "identifier":[{"system":"https://h.invalid/fhir/identifier/encounter","value":"WARD-X"}],
             "type":{"coding":[{"code":"restraint_consent"}]},"section":[]}}]}"#;
        let parsed = parse_bundle(json, None).unwrap();
        assert_eq!(parsed.items.len(), 1);
        // DocumentReference 被显式记账；另有「无 section」「无 date」两条结构诊断
        assert!(parsed.errors.iter().any(|d| d.loc.doc_id == "DR-1"));
        assert!(parsed.errors.iter().all(|d| d.code == parse_code::STRUCTURE));
    }

    #[test]
    fn null_section_text_yields_diagnostic_but_keeps_item() {
        let json = r#"{"resourceType":"Bundle","entry":[{"resource":{
            "resourceType":"Composition","id":"X-6",
            "identifier":[{"system":"https://h.invalid/fhir/identifier/encounter","value":"WARD-X"}],
            "type":{"coding":[{"code":"restraint_consent"}]},
            "section":[{"title":"患方知情同意","text":null}]}}]}"#;
        let parsed = parse_bundle(json, None).unwrap();
        assert_eq!(parsed.items.len(), 1, "部分成功：坏段不拖垮整件");
        let null_diag = parsed
            .errors
            .iter()
            .find(|d| d.message.text.contains("text 为 null"))
            .expect("null 段文本必须显式记诊断");
        assert_eq!(null_diag.loc.section_index, 0);
    }

    #[test]
    fn full_width_and_half_width_separators_both_parse() {
        assert_eq!(find_key_sep("约束事由：夜间冲动"), Some("约束事由".len()));
        assert_eq!(find_key_sep("约束事由: 夜间冲动"), Some("约束事由".len()));
    }

    #[test]
    fn char_range_is_character_based_not_byte_based() {
        // 生僻字占 3 字节但只算 1 个字符：区间切出的必须是字本身
        let text = "㊙事项：已核实";
        let colon = find_key_sep(text).unwrap();
        let sep_bytes = text[colon..].chars().next().map(char::len_utf8).unwrap();
        let rest = &text[colon + sep_bytes..];
        let value = rest.trim();
        let start = 4u32; // "㊙事项" 4 个字符
        let span = Span::unchecked(start, start + value.chars().count() as u32);
        assert_eq!(span.slice(text), Some(value));
    }
}
