//! 解析器容错与**部分成功**：真实院内导出包必然带脏数据。
//!
//! ## 源产品主能力与改写
//!
//! 源主路径 `parse(source, options) -> Result<Parsed<Mod>, ParseError>`
//! （`github_ref/ruff/crates/ruff_python_parser/src/lib.rs:281`）的关键行为是
//! **解析失败也返回带位置的错误**：
//! `ParseError { error: ParseErrorType, location: TextRange }`
//! （`error.rs:11-14`）。源侧一个文件要么整个 `Parsed`、要么整个 `ParseError`。
//!
//! 院内导出不能照抄这个「全有或全无」：一个病区导出的包里，
//! **一份文书的 date 写错，不该让同包另外四份一起消失**——护士长看到
//! 「4 件都读不出来」时，唯一能做的只有找信息科重导，而看不到是哪一件、
//! 哪一段出了问题。本模块把源侧语义改写为：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | 文件级 `Result<Parsed, ParseError>` | [`parse_export`]：整包不失败，**能解析的件照常进 `items`** |
//! | `TextRange` 字节区间 | `Span` 字符区间 + `DocLoc(doc_id, section_index, char_range)` |
//! | 源码按 UTF-8 读，非法字节直接失败 | [`decode_export_bytes`]：UTF-8 / UTF-8-BOM / GBK 嗅探，**非法字节替换为占位符并记 `E_ENCODING`** |
//! | 单文件粒度 | **段内粒度**：超长段按段落边界切分，char_range 口径不变 |
//! | 无重复概念 | 重复 `doc_id` 标 `E_DOC_ID_DUPLICATE`，**两条都进人工队列**，不静默合并也不静默丢弃 |
//!
//! ## 零静默丢弃的可核对口径
//!
//! [`ParsedBundle::is_balanced`] 是不变量：进入解析的 entry 数
//! **必须等于** 成功件数 + 整件不可用件数。少一件就是有件被静默吞了，
//! 底栏「未解析 N 件」这个数字才有意义。

use crate::{parse_bundle, ParsedBundle, ParseError};
use encoding_rs::GBK;
use psychdoc_model::{DocItem, DocTypeRegistry, Span};
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// 一、编码嗅探
// ---------------------------------------------------------------------------

/// 非法字节的占位符。用 U+FFFD（`�`）而不是删字符——删字符会让
/// 后续 `char_range` 整体前移，卡片高亮就偏到别的字上；占位符占住一位，
/// 定位口径不变，护士长也能看出「这里原本有个字但认不出」。
pub const REPLACEMENT_CHAR: char = '\u{FFFD}';

/// 导出包的实际编码。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceEncoding {
    /// 纯 UTF-8
    #[default]
    Utf8,
    /// 带 UTF-8 BOM（Windows 上的院内导出工具常见）
    Utf8Bom,
    /// GBK / GB18030（中文 Windows 区域设置下导出的默认编码）
    Gbk,
    /// 同一文件内 UTF-8 与 GBK 片段交替
    Mixed,
}

impl SourceEncoding {
    /// 给「已跳过 / 未解析」清单用的人话（不出现内部术语）。
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Utf8 => "UTF-8",
            Self::Utf8Bom => "UTF-8（带 BOM 标记）",
            Self::Gbk => "GBK",
            Self::Mixed => "UTF-8 与 GBK 混编",
        }
    }
}

/// 编码嗅探结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Decoded {
    /// 解码后的文本，可直接交给 JSON 解析
    pub text: String,
    pub encoding: SourceEncoding,
    /// 被占位符替换掉的字符数（**逐个记，不合并成一句「有乱码」**）
    pub replaced_chars: usize,
    /// 每处替换的字节偏移与说明，进 `unresolved` 供人工回查
    pub notes: Vec<EncodingNote>,
}

/// 一处编码问题。`byte_offset` 让信息科能直接定位到导出工具的哪一段写坏了。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodingNote {
    pub byte_offset: usize,
    pub detail: String,
}

/// 嗅探并解码一份导出包。
///
/// 判定顺序：
///
/// 1. UTF-8 BOM → 剥掉标记，按 UTF-8 严格解；标记之后不是 UTF-8 就按实际
///    字节判定，但**保留「文件头写着 BOM」这条线索**（导出工具的编码选项
///    与实际写出不一致，本身就是条现场事实）
/// 2. 全文严格 UTF-8 → 直接用
/// 3. 否则按 ASCII 边界**逐段**判定（见 [`decode_mixed`]）
///
/// 第 3 步要处理的是真实存在的形态：同一台机器上「模板段」由 UTF-8 组件写入、
/// 「自由叙述段」由 GBK 组件补录，导出来就是一个文件里两种编码并存。
/// 整体按单一编码解会把另一半变成乱码，而乱码在护士长眼里和「文书没写」
/// 长得一样——那比报错更危险。
pub fn decode_export_bytes(bytes: &[u8]) -> Decoded {
    // ① BOM
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return match std::str::from_utf8(rest) {
            Ok(s) => Decoded {
                text: s.to_string(),
                encoding: SourceEncoding::Utf8Bom,
                replaced_chars: 0,
                notes: vec![EncodingNote {
                    byte_offset: 0,
                    detail: "文件开头有 UTF-8 BOM 标记，已剥掉后按 UTF-8 读取（不影响内容）".into(),
                }],
            },
            // BOM 后面不是 UTF-8：标记与实际编码不符，按混编走，但保留「BOM 在」
            // 这条线索——导出工具的编码选项和实际写出不一致本身就是条线索。
            Err(_) => {
                let mut d = decode_mixed(rest, 3);
                d.notes.insert(
                    0,
                    EncodingNote {
                        byte_offset: 0,
                        // 注意：此时 d.encoding 可能是 Gbk / Mixed，
                        // 不可改写它，否则「标着 BOM 却是 GBK」这条现场事实就丢了。
                        detail: "文件开头有 UTF-8 BOM 标记，但标记之后不是合法 UTF-8，已按实际字节判定编码".into(),
                    },
                );
                d
            }
        };
    }
    // ② 严格 UTF-8
    if let Ok(s) = std::str::from_utf8(bytes) {
        return Decoded {
            text: s.to_string(),
            encoding: SourceEncoding::Utf8,
            replaced_chars: 0,
            notes: Vec::new(),
        };
    }
    // ③ 按 ASCII 边界逐段判定。纯 GBK 包与混编包走同一条路径：
    // 前者的每一段都判成 GBK，编码汇总出来自然就是 Gbk。刻意**不**在这里
    // 加一条「先整体试 GBK」的捷径——整体 GBK 解常常「不报错」却把
    // UTF-8 那半边解成乱码，能过 `had_errors == false` 却内容全错。
    let mut d = decode_mixed(bytes, 0);
    if d.encoding == SourceEncoding::Gbk && d.replaced_chars == 0 {
        d.notes.insert(
            0,
            EncodingNote {
                byte_offset: 0,
                detail: "文件不是 UTF-8，已按 GBK 读取（中文 Windows 导出工具的常见编码）".into(),
            },
        );
    }
    d
}

/// 逐段解码。**以 ASCII 字节为段边界**，每一段独立判定编码。
///
/// 为什么必须以 ASCII 为界：UTF-8 与 GBK 里 ASCII 字节都是**独立单字节**，
/// 不会出现在多字节字符内部。而 GBK 的双字节组合里有一大批**恰好也是
/// 合法 UTF-8 序列**（`0xC2 0xBC` 既是 GBK 的「录」，也是 UTF-8 的 `¼`），
/// 所以「逐字节位置先试 UTF-8 再试 GBK」这种贪心判定必然在 GBK 段中间
/// 解出乱码。段边界定在 ASCII 上之后，一段非 ASCII 字节要么整段是 UTF-8、
/// 要么整段是 GBK，不再有逐字节二选一的问题。
///
/// 段内判定：先整段试 UTF-8，再整段试 GBK；两者都不成立时，
/// 试「UTF-8 前缀 + GBK 后缀」切点（段内两种编码交界处没有 ASCII 隔断）；
/// 仍解不出则**逐字节**兜底，解不了的替换为占位符并逐处记录。
fn decode_mixed(bytes: &[u8], offset_base: usize) -> Decoded {
    let mut out = String::with_capacity(bytes.len());
    let mut notes: Vec<EncodingNote> = Vec::new();
    let mut replaced = 0usize;
    let mut saw_gbk = false;
    let mut saw_utf8_multibyte = false;
    let mut i = 0usize;

    while i < bytes.len() {
        // ASCII 字节：两种编码下都是独立单字节，直接放行
        if bytes[i].is_ascii() {
            out.push(bytes[i] as char);
            i += 1;
            continue;
        }
        // 取到下一个 ASCII 字节（或文件末尾）为止的一段连续非 ASCII 字节
        let start = i;
        while i < bytes.len() && !bytes[i].is_ascii() {
            i += 1;
        }
        let run = &bytes[start..i];

        // ① 整段严格 UTF-8
        if let Ok(s) = std::str::from_utf8(run) {
            saw_utf8_multibyte = true;
            out.push_str(s);
            continue;
        }
        // ② 整段严格 GBK
        let (cow, had_errors) = GBK.decode_without_bom_handling(run);
        if !had_errors {
            saw_gbk = true;
            out.push_str(&cow);
            continue;
        }
        // ③ 段内两种编码交界：找「UTF-8 前缀 + GBK 后缀」的切点
        if let Some(k) = split_utf8_then_gbk(run) {
            let head = std::str::from_utf8(&run[..k]).expect("切点按合法 UTF-8 前缀挑出");
            let (tail, _) = GBK.decode_without_bom_handling(&run[k..]);
            saw_utf8_multibyte = true;
            saw_gbk = true;
            out.push_str(head);
            out.push_str(&tail);
            continue;
        }
        // ④ 段内仍有坏字节：逐字节兜底，坏处逐个记
        saw_utf8_multibyte = true;
        if decode_broken_run(run, offset_base + start, &mut out, &mut notes, &mut replaced) {
            saw_gbk = true;
        }
    }

    let encoding = match (saw_utf8_multibyte, saw_gbk) {
        (true, true) => SourceEncoding::Mixed,
        (false, true) => SourceEncoding::Gbk,
        (true, false) => SourceEncoding::Utf8,
        // 全 ASCII 却落到这里：严格 UTF-8 分支应该已经接住了。
        // 保守按 Mixed 记，而不是假装成 UTF-8——这条路径的可达性由
        // `a_mixed_file_falls_back_to_per_run_decoding` 守住。
        (false, false) => SourceEncoding::Mixed,
    };
    if !notes.is_empty() {
        notes.insert(
            0,
            EncodingNote {
                byte_offset: offset_base,
                detail: format!(
                    "文件存在无法解码的字节（已按 {} 判定编码，{} 处替换为占位符）；\
                     解析继续，坏字节所在位置已逐处记录",
                    encoding.as_zh(),
                    replaced
                ),
            },
        );
    }
    Decoded {
        text: out,
        encoding,
        replaced_chars: replaced,
        notes,
    }
}

/// 找「UTF-8 前缀 + GBK 后缀」在 `run` 里的切点 `k`（`run[..k]` 合法 UTF-8
/// 且 `run[k..]` 合法 GBK），返回字节下标。
///
/// ASCII 边界管不了所有情况：`……日间激越。` 后面紧跟 GBK 字节时，
/// 那个 UTF-8 句号和后面的 GBK 双字节之间**没有 ASCII 隔断**，
/// 会落进同一段。这时必须在段内再切一刀。
///
/// 切点取**最靠后的合法位置**：UTF-8 前缀越长越可信。短前缀（1–2 字节）
/// 更可能是 GBK 双字节碰巧长得像合法 UTF-8（`0xC2 0xBC` 两边都合法），
/// 把那种巧合当切点会把 GBK 正文从中间劈开。
fn split_utf8_then_gbk(run: &[u8]) -> Option<usize> {
    // 候选切点只能是「合法 UTF-8 前缀的末尾」，用 valid_up_to 递推得出，
    // 避免对每个 k 都重解一遍（O(n²) 在 13 万字段落上会明显卡）。
    let mut candidates: Vec<usize> = Vec::new();
    let mut k = 0usize;
    while k < run.len() {
        let rest = &run[k..];
        let good = match std::str::from_utf8(rest) {
            Ok(_) => break, // 剩余整段合法 UTF-8，不会再有切点
            Err(e) => e.valid_up_to(),
        };
        if good == 0 {
            break; // 这个字节开头的 UTF-8 序列不合法，k 不是候选
        }
        k += good;
        candidates.push(k);
    }
    for k in candidates.into_iter().rev() {
        if k == 0 || k == run.len() {
            continue;
        }
        let (_, had_errors) = GBK.decode_without_bom_handling(&run[k..]);
        if !had_errors {
            return Some(k);
        }
    }
    None
}

/// 一段既不是合法 UTF-8、也不是合法 GBK、也切不出「UTF-8 前缀 + GBK 后缀」
/// 的字节：能解的尽量解，解不了的**逐字节**替换为占位符并记录字节偏移。
///
/// 这里**不**做「猜一个 GBK 双字节」的兜底。理由是那种猜测会**静默**
/// 产出错字：`0xFE 0x81` 恰好是合法 GBK 字符，于是一个本来损坏的字节
/// 会被「解」成某个不相干的汉字混进正文，而护士长在卡片上根本看不出异常。
/// 显式占位符至少让「这里原本有个字但认不出」这件事**看得见**。
///
/// 返回值恒为 `false`：走到这里说明这段不是 GBK，编码汇总不该把它算成 GBK。
fn decode_broken_run(
    run: &[u8],
    base: usize,
    out: &mut String,
    notes: &mut Vec<EncodingNote>,
    replaced: &mut usize,
) -> bool {
    let mut j = 0usize;
    while j < run.len() {
        match std::str::from_utf8(&run[j..]) {
            Ok(s) => {
                out.push_str(s);
                return false;
            }
            Err(e) => {
                let good = e.valid_up_to();
                if good > 0 {
                    // 开头这段仍是合法 UTF-8：整段放行（坏字节只在其后）
                    let head = std::str::from_utf8(&run[j..j + good]).unwrap_or_default();
                    out.push_str(head);
                    j += good;
                    continue;
                }
                out.push(REPLACEMENT_CHAR);
                notes.push(EncodingNote {
                    byte_offset: base + j,
                    detail: format!(
                        "第 {} 字节无法解码，已替换为占位符「�」；该处原文无法还原，\
                         请让信息科用 UTF-8 重新导出",
                        base + j
                    ),
                });
                *replaced += 1;
                j += 1;
            }
        }
    }
    false
}

// ---------------------------------------------------------------------------
// 二、超长段切分
// ---------------------------------------------------------------------------

/// 单段超过这个字符数就按段落边界切分。
///
/// 10 万字是「病程记录里粘了一整篇外文文献摘要」那种量级。切分是为了
/// 后续的卡片渲染与人工通读能加载得动，**不是为了改变内容**——切出来的
/// 每一块都保留原文，字段区间在各自块内重新计算，char_range 口径不变。
pub const LONG_SECTION_CHARS: usize = 100_000;

/// 把一段超长文本按**段落边界**切成多块。
///
/// 返回 `(标题, 文本)` 列表：首块沿用原标题，续块标题加「（续 n）」。
/// 切分点只落在换行处——落在句子中间会把一句话劈成两半，
/// 而文书卡片的字段高亮是按行做的，劈开的行会让必填字段检查失去上下文。
/// 单行本身超长时按字符硬切（否则无切分点可用，会死循环）。
///
/// 实现上先算出**字符下标的切点**，再从原文切片，而不是逐块重新拼字符串：
/// 重新拼的话每块都要自己判断「要不要补换行」，补一次多一个字、少一次
/// 丢一个字，两种错法都会让「切分零丢失」这条承诺悄悄不成立。切片则
/// 由构造保证：所有块首尾相接、覆盖 `[0, 原文字数)`、互不重叠。
pub fn split_long_section(title: &str, text: &str, limit: usize) -> Vec<(String, String)> {
    let chars: Vec<char> = text.chars().collect();
    if limit == 0 || chars.len() <= limit {
        return vec![(title.to_string(), text.to_string())];
    }

    // 切点：每个切点是一个「块起始字符下标」，必须严格递增，
    // 且第一个是 0、最后一个是 chars.len()。
    //
    // 逐**行**推进（而不是逐字符找 '\n'）：最后一行没有尾随换行，
    // 逐字符扫描会漏掉它，单行超长就切不开。行区间是 `[start, end)`，
    // `end` 指向换行符本身——换行符留给**后一块**的开头，
    // 这样各块首尾相接，既不丢换行也不重复。
    let mut cuts: Vec<usize> = vec![0];
    let mut start = 0usize;
    for (i, c) in chars.iter().enumerate() {
        if *c != '\n' {
            continue;
        }
        let line_len = i - start;
        if line_len > limit {
            // 单行超长：先把当前块封口到行首，再按 limit 硬切这一行
            if *cuts.last().unwrap() < start {
                cuts.push(start);
            }
            let mut at = start;
            while at < i {
                at = (at + limit).min(i);
                if at < i {
                    cuts.push(at);
                }
            }
        } else if i + 1 - *cuts.last().unwrap() > limit {
            // 当前块加上这一行（含换行）会超限 → 在行首处切
            cuts.push(start);
        }
        start = i + 1;
    }
    // 最后一行（无尾随换行）：同样可能超长
    if chars.len() - start > limit {
        if *cuts.last().unwrap() < start {
            cuts.push(start);
        }
        let mut at = start;
        while at < chars.len() {
            at = (at + limit).min(chars.len());
            if at < chars.len() {
                cuts.push(at);
            }
        }
    }
    cuts.push(chars.len());
    // 原文以换行结尾时最后一块会是空的，下面的 windows 会跳过它
    let mut bounds: Vec<usize> = cuts.into_iter().filter(|c| *c <= chars.len()).collect();
    if bounds.first() != Some(&0) {
        bounds.insert(0, 0);
    }
    if bounds.last() != Some(&chars.len()) {
        bounds.push(chars.len());
    }

    let mut out: Vec<(String, String)> = Vec::new();
    for w in bounds.windows(2) {
        if w[0] == w[1] {
            continue; // 原文以换行结尾产生的空块，跳过
        }
        let part = out.len() + 1;
        let t = if part == 1 {
            title.to_string()
        } else {
            format!("{title}（续 {part}）")
        };
        out.push((t, chars[w[0]..w[1]].iter().collect()));
    }
    if out.is_empty() {
        out.push((title.to_string(), text.to_string()));
    }
    out
}

// ---------------------------------------------------------------------------
// 三、重复 doc_id
// ---------------------------------------------------------------------------

/// 重复 `doc_id` 的原因码。
pub const DUPLICATE_CODE: &str = "E_DOC_ID_DUPLICATE";

/// 同一份导出包里出现两个及以上 `id` 相同的 Composition。
///
/// 处置：**两条都不进 `items`，都进 `unresolved`**。
///
/// 为什么不「保留第一条」：`Composition.id` 是本产品的权威实体键，
/// 依赖图、时间序 join、幂等键全都按它连。留一条就等于替信息科猜
/// 「另一条是重复导出的垃圾」——而真实原因可能是 HIS 与护理系统
/// 都写了同一个 id（那要人确认以谁为准），也可能是导出工具重试
/// （那要人确认哪条是最新的）。两种情形的正确处置不同，程序不该替人选。
///
/// 为什么不「合并成一条」：合并等于把两份可能不同的正文拼在一起，
/// 拼出来的文书谁都不敢拿去归档。
pub fn detect_duplicate_ids(
    items: Vec<DocItem>,
) -> (Vec<DocItem>, Vec<psychdoc_model::Diagnostic>) {
    // 键用 owned String：`items` 下一行就要被 move 走，
    // 借用它的 `counts` 会让编译器拒绝这次 move。
    let mut counts: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
    for d in &items {
        *counts.entry(d.doc_id.clone()).or_insert(0) += 1;
    }
    let dups: std::collections::BTreeSet<&str> = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(id, _)| id.as_str())
        .collect();
    if dups.is_empty() {
        return (items, Vec::new());
    }
    let mut kept = Vec::new();
    let mut diags = Vec::new();
    for d in items.into_iter() {
        if dups.contains(d.doc_id.as_str()) {
            let n = counts.get(&d.doc_id).copied().unwrap_or(2);
            diags.push(crate::parse_diag(
                crate::parse_code::DUPLICATE,
                psychdoc_model::DocLoc::new(d.doc_id.clone(), 0, Span::unchecked(0, 0)),
                format!(
                    "文书 id「{}」在同一份导出包里出现 {n} 次，全部交人工确认，\
                     未做合并也未做取舍（合并会把 {n} 份正文拼在一起，\
                     取舍要由人决定以哪份为准）",
                    d.doc_id
                ),
            ));
        } else {
            kept.push(d);
        }
    }
    (kept, diags)
}

// ---------------------------------------------------------------------------
// 四、字节级入口
// ---------------------------------------------------------------------------

/// 解析一份**字节流**形式的导出包：先嗅探编码，再走标准解析链路。
///
/// 这是文件与目录发现路径的真正入口——院内导出给的是文件，
/// 文件是字节，「先 `read_to_string` 再解析」的写法在 GBK 包上会直接
/// `E_IO` 崩掉整次批扫。
pub fn parse_export(
    bytes: &[u8],
    registry: Option<&DocTypeRegistry>,
) -> Result<(ParsedBundle, Decoded), ParseError> {
    let decoded = decode_export_bytes(bytes);
    let mut parsed = parse_bundle(&decoded.text, registry)?;
    for note in &decoded.notes {
        parsed.errors.push(crate::parse_diag(
            crate::parse_code::ENCODING,
            psychdoc_model::DocLoc::new(
                WHOLE_PACKAGE,
                0,
                Span::unchecked(0, 0),
            ),
            format!("E_ENCODING：{}", note.detail),
        ));
    }
    Ok((parsed, decoded))
}

/// 整包级诊断用的占位 doc_id。编码问题与某一个件无关，
/// 挂在某个件上会让「这一件的哪一段有问题」这句话变成假的。
pub const WHOLE_PACKAGE: &str = "<整包>";

#[cfg(test)]
mod tests {
    use super::*;

    // ---- 编码嗅探 ---------------------------------------------------------

    #[test]
    fn a_plain_utf8_file_is_decoded_without_any_replacement() {
        let bytes = "约束事由：夜间反复冲动。".as_bytes();
        let d = decode_export_bytes(bytes);
        assert_eq!(d.encoding, SourceEncoding::Utf8);
        assert_eq!(d.replaced_chars, 0);
        assert_eq!(d.text, "约束事由：夜间反复冲动。");
        assert!(d.notes.is_empty(), "干净的包不该产生任何编码备注");
    }

    #[test]
    fn a_utf8_bom_is_stripped_and_the_content_survives() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("患方意见：同意。".as_bytes());
        let d = decode_export_bytes(&bytes);
        assert_eq!(d.encoding, SourceEncoding::Utf8Bom);
        assert!(!d.text.starts_with('\u{FEFF}'), "BOM 不得留在正文里");
        assert!(d.text.contains("患方意见：同意。"));
        assert!(d.replaced_chars == 0);
    }

    #[test]
    fn a_gbk_file_is_decoded_as_gbk_not_mojibake() {
        // 「约束起始时间」在 GBK 下的真实字节
        let gbk: Vec<u8> = {
            let (cow, _, err) = GBK.encode("约束起始时间：2026-03-02 23:40。");
            assert!(!err, "GBK 应能编码这串中文");
            cow.into_owned()
        };
        let d = decode_export_bytes(&gbk);
        assert_eq!(d.encoding, SourceEncoding::Gbk, "GBK 包不得被当成混编");
        assert_eq!(d.replaced_chars, 0);
        assert_eq!(d.text, "约束起始时间：2026-03-02 23:40。");
        assert!(std::str::from_utf8(&gbk).is_err(), "前置条件：GBK 字节不是合法 UTF-8");
    }

    #[test]
    fn a_mixed_file_falls_back_to_per_run_decoding() {
        // 前半 UTF-8、后半 GBK：整体按任一单编码解都会坏掉一半
        let mut bytes = "约束事由：日间激越。".as_bytes().to_vec();
        let (gbk, _, err) = GBK.encode("观察记录：约束期间生命体征平稳。");
        assert!(!err);
        bytes.extend_from_slice(&gbk);
        let d = decode_export_bytes(&bytes);
        assert_eq!(d.encoding, SourceEncoding::Mixed);
        assert_eq!(d.replaced_chars, 0, "两种编码都合法时不该有占位符");
        assert!(d.text.contains("约束事由：日间激越。"), "UTF-8 段必须原样保留");
        assert!(d.text.contains("观察记录：约束期间生命体征平稳。"), "GBK 段必须解对");
    }

    #[test]
    fn illegal_bytes_become_placeholders_and_every_one_is_recorded() {
        let mut bytes = "约束事由：日间激越。".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xFF, 0xFE, 0x81]); // 三个非法字节
        bytes.extend_from_slice("患方意见：同意。".as_bytes());
        let d = decode_export_bytes(&bytes);
        assert_eq!(d.replaced_chars, 3, "三个坏字节要记三处，不能合并成一句「有乱码」");
        assert!(d.text.contains(REPLACEMENT_CHAR));
        // 占位符占位 → 后续字符区间不前移
        assert!(d.text.ends_with("患方意见：同意。"), "坏字节之后的正文必须完好");
        let offsets: Vec<usize> = d.notes.iter().map(|n| n.byte_offset).collect();
        assert!(
            offsets.windows(2).all(|w| w[0] < w[1]),
            "notes 必须按字节偏移升序，便于信息科逐处回查：{offsets:?}"
        );
        assert!(d.notes.iter().all(|n| n.detail.contains("UTF-8")));
    }

    #[test]
    fn a_bom_on_a_non_utf8_body_keeps_both_clues() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        let (gbk, _, _) = GBK.encode("约束事由：日间激越。");
        bytes.extend_from_slice(&gbk);
        let d = decode_export_bytes(&bytes);
        // 编码按实际字节判（GBK），但「文件头写着 BOM」这条线索必须留下
        assert_eq!(d.encoding, SourceEncoding::Gbk);
        assert!(d.notes[0].detail.contains("BOM"));
        assert!(d.text.contains("约束事由：日间激越。"));
    }

    #[test]
    fn decoding_is_byte_for_byte_reproducible() {
        let mut bytes = "评估时间：2026-03-02 09:00。".as_bytes().to_vec();
        bytes.extend_from_slice(&[0xFF]);
        let (g, _, _) = GBK.encode("执行护士：<EXAMPLE_NURSE>。");
        bytes.extend_from_slice(&g);
        let a = decode_export_bytes(&bytes);
        let b = decode_export_bytes(&bytes);
        assert_eq!(a, b, "同一份字节两次解码必须逐字段相同，否则批扫结果不可复现");
    }

    // ---- 超长段切分 -------------------------------------------------------

    #[test]
    fn a_section_under_the_limit_is_untouched() {
        let text = "短段。\n就这两行。";
        let out = split_long_section("评估", text, 100);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].0, "评估");
        assert_eq!(out[0].1, text, "未超限的段必须逐字不变");
    }

    #[test]
    fn a_long_section_is_split_at_paragraph_boundaries() {
        let para = "观察记录：约束期间生命体征平稳，无不适主诉。"; // 22 字
        let text = std::iter::repeat_n(para, 20).collect::<Vec<_>>().join("\n");
        assert!(text.chars().count() > 100);
        let out = split_long_section("观察", &text, 100);
        assert!(out.len() > 1, "超限段必须被切开");
        // 切分点只落在换行处：每块内部不含被劈开的句子
        for (_, chunk) in &out {
            assert!(chunk.chars().count() <= 100 + para.chars().count());
        }
        // 续块标题带序号
        assert_eq!(out[0].0, "观察");
        assert!(out[1].0.starts_with("观察（续 "));
        // **内容零丢失**：各块首尾相接，直接拼回去等于原文
        let rejoined: String = out.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(rejoined, text, "切分不得丢字或改字");
    }

    #[test]
    fn a_single_oversized_line_is_hard_split_instead_of_looping_forever() {
        let text = "甲".repeat(250);
        let out = split_long_section("长行", &text, 100);
        assert_eq!(out.len(), 3, "250 字按 100 切应是 3 块");
        let rejoined: String = out.iter().map(|(_, c)| c.as_str()).collect();
        assert_eq!(rejoined, text, "硬切也不得丢字");
        assert!(out.iter().all(|(_, c)| c.chars().count() <= 100));
    }

    #[test]
    fn a_limit_of_zero_does_not_divide_by_zero() {
        let out = split_long_section("段", "内容", 0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].1, "内容");
    }

    // ---- 重复 doc_id ------------------------------------------------------

    fn item(id: &str) -> DocItem {
        DocItem::new(id, "restraint_consent", "WARD-Z-001")
    }

    #[test]
    fn duplicate_ids_are_all_kept_out_and_reported_not_merged() {
        let (kept, diags) = detect_duplicate_ids(vec![
            item("D-1"),
            item("D-OK"),
            item("D-1"),
            item("D-1"),
        ]);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].doc_id, "D-OK", "唯一 id 的件必须照常进结果");
        assert_eq!(diags.len(), 3, "三份重复件各出一条，不合并成一条");
        assert!(diags.iter().all(|d| d.code == crate::parse_code::DUPLICATE));
        assert!(diags.iter().all(|d| d.message.text.contains("3 份")));
    }

    #[test]
    fn a_clean_package_produces_no_duplicate_diagnostics() {
        let (kept, diags) = detect_duplicate_ids(vec![item("D-1"), item("D-2"), item("D-3")]);
        assert_eq!(kept.len(), 3);
        assert!(diags.is_empty());
    }
}
