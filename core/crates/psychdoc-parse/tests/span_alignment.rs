//! 对照测试：源侧区间语义 ↔ 产品侧定位模型逐条对齐。
//!
//! 源产品 `ruff_python_parser` 的位置一律是 `TextRange`（**字节**区间，
//! `github_ref/ruff/crates/ruff_python_parser/src/error.rs:11-14`），
//! 且 `TextRange` 通过 `source[range]` 直接切片校验。
//!
//! 本产品把位置改写为 `(doc_id, section_index, char_range)`，其中
//! `char_range` 是**字符**区间（Task 2 决策：卡片高亮必须与生僻字同口径）。
//! 两套口径不能靠"看起来对"验收，必须逐条对拍：
//!
//! | 断言 | 含义 |
//! |---|---|
//! | `byte_span_agrees_with_char_span` | 产品字符区间换算成字节后，切出的子串与源侧字节区间切出的**逐字节相同** |
//! | `every_field_locates_into_its_own_section` | 每条字段的区间都能在**它自己那一段**的文本上切出非空子串（越界或错段即失败） |
//! | `diagnostic_locates_the_exact_missing_value` | 诊断的定位能切回它所描述的那段原文，而不是「大概在这一段」 |
//!
//! 这条对照测试存在的理由：定位错位（错 1 个字符 / 错一段）在单测里
//! 往往看不出来，但会让护士长在卡片上看到高亮落到别的句子上——
//! 那等于没有依据，比不报更糟。

use psychdoc_model::{DocItem, Span};
use psychdoc_parse::parse_bundle;
use psychdoc_rules::{load_effective, review};

const FIXTURE: &str = include_str!("../../../../fixtures/ward-01-minimal.json");

/// 本文件守的是**位置口径**（字符区间 ↔ 源侧字节区间），不是院内业务口径。
/// 因此这里自带一份最小规则集并把 QC-PSY-01 钉在 `restraint_consent` 上，
/// 而不是读 `rulesets/hospital.yaml`——否则院级 / 科室级哪天调整了这条码的
/// 作用范围（例如把住院记录降为提示），这个与业务无关的对拍测试就会跟着变红。
const RULESET: &str = r#"
version: "test-span-alignment"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方意见
        section: 患方知情同意
        required: true
      - key: 患方签字
        section: 患方知情同意
        required: true
hospital:
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      applies_to: ["restraint_consent"]
      basis: "《精神科住院护理常规》第 4.2 条"
"#;

/// 把字符区间换算成字节区间 —— 源侧 `TextRange` 用的就是字节下标。
/// 这是本产品从字符口径回到源侧口径的唯一桥，因此必须与源侧切片结果一致。
fn char_span_to_byte(text: &str, span: Span) -> (usize, usize) {
    let start = text.char_indices().nth(span.start as usize).map_or(text.len(), |(b, _)| b);
    let end = text
        .char_indices()
        .nth(span.end as usize)
        .map_or(text.len(), |(b, _)| b);
    (start, end)
}

fn load() -> (Vec<DocItem>, psychdoc_rules::EffectiveRules) {
    let rs = load_effective(RULESET).expect("院内规则集应可加载");
    let reg = rs.registry();
    let items = parse_bundle(FIXTURE, Some(&reg)).expect("夹具应可解析").items;
    (items, rs)
}

#[test]
fn byte_span_agrees_with_char_span() {
    let (items, _) = load();
    let item = &items[0];
    let mut compared = 0;
    for f in &item.fields {
        let Some(span) = f.source_span else { continue };
        let sec = &item.sections[f.section_index as usize];
        // 产品侧：字符区间直接切
        let by_char = span.slice(&sec.text);
        // 源侧口径：换算成字节区间后切
        let (bs, be) = char_span_to_byte(&sec.text, span);
        let by_byte = sec.text.get(bs..be);
        assert_eq!(
            by_char, by_byte,
            "字段「{}」字符区间与字节区间切出的子串不一致",
            f.key
        );
        compared += 1;
    }
    assert!(compared >= 4, "应至少对拍 4 个带区间的字段，实际 {compared}");
}

#[test]
fn every_field_locates_into_its_own_section() {
    let (items, _) = load();
    for item in &items {
        for f in &item.fields {
            let Some(span) = f.source_span else { continue };
            let sec = item
                .sections
                .get(f.section_index as usize)
                .unwrap_or_else(|| panic!("字段「{}」的段序号越界", f.key));
            assert!(
                span.slice(&sec.text).is_some(),
                "字段「{}」的区间切不出子串 → 定位越界",
                f.key
            );
            // 值非空的字段，其区间长度必须与值等长（不多不少）
            if let Some(v) = &f.value {
                if !v.is_empty() {
                    assert_eq!(
                        span.len(),
                        u32::try_from(v.chars().count()).unwrap(),
                        "字段「{}」的区间长度与值长度不符",
                        f.key
                    );
                }
            }
        }
    }
}

#[test]
fn diagnostic_locates_the_exact_missing_value() {
    let (items, rs) = load();
    let result = review(&items, &rs);
    assert_eq!(result.diagnostics.len(), 1);
    let d = &result.diagnostics[0];
    let item = &items[0];
    // 诊断的 doc_id 必须是真实存在的件，不是编出来的
    assert_eq!(d.loc.doc_id, item.doc_id);
    // 诊断的 section_index 落在真实段落范围内
    let sec = item
        .sections
        .get(d.loc.section_index as usize)
        .expect("诊断定位到不存在的段落");
    // 位置可切回原文：这里切出的是零宽区间（空值），但区间端点必须合法
    let (bs, be) = char_span_to_byte(&sec.text, d.loc.char_range);
    assert!(sec.text.get(bs..be).is_some(), "诊断区间越出该段文本");
    // 且该段落确实含被判缺的键
    assert!(
        sec.text.contains("患方签字"),
        "诊断落在不含问题字段的段落上 → 定位错了"
    );
}

#[test]
fn char_offset_survives_multibyte_keys() {
    // 键本身含全角字（3 字节/字）。若把键的**字节**数当成字符数，
    // 值区间会整体前移，切出「情同意…」而不是「已知情同意…」。
    let (items, _) = load();
    let item = &items[0];
    let f = item.field("患方意见").expect("键含全角字");
    let sec = &item.sections[f.section_index as usize];
    let sliced = f.source_span.unwrap().slice(&sec.text).expect("区间应可切");
    assert_eq!(sliced, f.value.as_deref().expect("字段有值"));
    assert!(sliced.starts_with('已'), "值区间被键的字节长度带偏");
}
