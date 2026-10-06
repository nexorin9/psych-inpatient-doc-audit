//! Task 22 集成测试：**解析器容错与部分成功**。
//!
//! 真实院内导出包必然带脏数据。这份文件把七类脏数据各钉一个用例，
//! 主张只有一条：
//!
//! > **能解析的件照常进结果，坏处逐处记账；整包不失败，也不静默丢弃。**
//!
//! 「不失败」容易做到，`read_to_string` 换成容错解码就行；难的是
//! **「不静默丢弃」**——它要求每一件坏数据都能被指名到具体的字节偏移 /
//! 段号 / 原因码，否则底栏那个「未解析 N 件」只是个能被随便填的数字。
//! 因此本文件对每类脏数据都断言三件事：**产出**、**原因码**、**守恒**。

use psychdoc_model::{DocTypeRegistry, TimestampKind};
use psychdoc_parse::tolerance::{self, SourceEncoding, REPLACEMENT_CHAR};
use psychdoc_parse::{parse_code, parse_package_bytes_with, discover_dir, ParsedBundle};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");

fn registry() -> DocTypeRegistry {
    serde_yaml::from_str(RULESET).expect("交付规则集 YAML 合法")
}

/// 读一份脏夹具的**原始字节**——经�� `read_to_string` 的路径
/// 在这份文件里根本走不到（GBK 与非法字节都会让它当场失败）。
fn dirty_bytes(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures/dirty")
        .join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("读不到脏夹具 {}: {e}", p.display()))
}

fn parse_dirty(name: &str) -> psychdoc_parse::discovery::DiscoveredPackage {
    parse_package_bytes_with(&dirty_bytes(name), Some(&registry()))
        .unwrap_or_else(|e| panic!("脏夹具 {name} 不该整包失败：{e}"))
}

/// 整件不可用 = 件没进 `items`。
///
/// **不能**直接数 `errors` 里的 doc_id：段级问题（text 为 null、超长段切分、
/// 编码占位符）也带 doc_id，但那些件照样在 `items` 里。数它们会把守恒
/// 算假，让「未解析 N 件」这个数字比真实情况大。
fn wholly_unresolved(pkg: &psychdoc_parse::discovery::DiscoveredPackage) -> usize {
    let parsed: std::collections::BTreeSet<&str> =
        pkg.items.iter().map(|d| d.doc_id.as_str()).collect();
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for d in &pkg.errors {
        // `<整包>` 是整包级诊断的占位 doc_id（编码问题与某一个件无关），
        // 不对应任何一件文书，不能计入
        if d.loc.doc_id == "<整包>" || parsed.contains(d.loc.doc_id.as_str()) {
            continue;
        }
        // 重复件由调用方按「物理件数」单独计（一个 id 两条诊断 = 两个件），
        // 这里按 id 去重会把它们算成 1，与调用处的条数口径重复
        if d.code == parse_code::DUPLICATE {
            continue;
        }
        seen.insert(d.loc.doc_id.as_str());
    }
    seen.len()
}

/// 守恒：看到的 Composition 件 = 成功件 + 整件不可用件。
fn assert_no_silent_drop(pkg: &psychdoc_parse::discovery::DiscoveredPackage, total: usize) {
    // 重复 doc_id 是**两个物理件共用一个 id**：去重按 id 去重后只算 1，
    // 但守恒要按物理件数算，所以重复诊断的条数要加回来。
    let dups = diags_of(pkg, parse_code::DUPLICATE).len();
    assert_eq!(
        pkg.items.len() + wholly_unresolved(pkg) + dups,
        total,
        "有件被静默吞了：成功 {} + 整件不可用 {} + 重复件 {} != 看到的 {total}",
        pkg.items.len(),
        wholly_unresolved(pkg),
        dups
    );
}

fn diags_of<'a>(pkg: &'a psychdoc_parse::discovery::DiscoveredPackage, code: &str) -> Vec<&'a str> {
    pkg.errors
        .iter()
        .filter(|d| d.code == code)
        .map(|d| d.message.text.as_str())
        .collect()
}

// ---------------------------------------------------------------------------
// 1. GBK 导出
// ---------------------------------------------------------------------------

#[test]
fn a_gbk_export_parses_to_the_same_items_as_its_utf8_twin() {
    let bytes = dirty_bytes("ward-h-001-gbk-export.json");
    // 前置条件：这份包**真的**不是 UTF-8。这条断言是为了防止
    // 「夹具哪天被编辑器重新存成 UTF-8 了，测试还在绿」——
    // 那种情况下本用例已经不再测 GBK 了。
    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "前置条件失效：GBK 夹具已是合法 UTF-8，本用例不再覆盖 GBK 路径"
    );

    let pkg = parse_dirty("ward-h-001-gbk-export.json");
    assert_eq!(pkg.encoding, SourceEncoding::Gbk, "必须判成 GBK 而不是混编");
    assert_eq!(pkg.items.len(), 1, "GBK 包必须真的解析出件，不是「读进来但空的」");
    assert_no_silent_drop(&pkg, 1);

    let d = &pkg.items[0];
    assert_eq!(d.doc_id, "WARD-H-001-CONSENT-01");
    assert_eq!(d.encounter_alias, "WARD-H-001");
    // 中文正文逐字还原（GBK 解码对了才会过）
    let sign = d.field("患方意见").expect("患方意见应被抽成字段");
    assert_eq!(sign.value.as_deref(), Some("已知情同意，同意按护理常规采取保护措施。"));
    // 段内业务时刻也在（中文与时刻都在同一段里）
    assert!(
        d.timestamps
            .iter()
            .any(|t| t.kind == TimestampKind::BusinessMoment && t.moment.is_some()),
        "GBK 段内的「2026-03-02 23:40」必须被抽成时刻"
    );
}

#[test]
fn a_gbk_file_no_longer_crashes_the_directory_scan() {
    // 改用 `read_to_string` 的老写法在这份包上会直接 E_IO 崩掉整次批扫。
    // 这里钉住「一个病区目录里混着 GBK 包与 UTF-8 包时，两类都要出得来」。
    let dir = tempfile::tempdir().expect("临时目录");
    std::fs::create_dir_all(dir.path().join("ward-a")).unwrap();
    std::fs::create_dir_all(dir.path().join("ward-h")).unwrap();
    std::fs::write(
        dir.path().join("ward-a/utf8.json"),
        std::fs::read(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../../fixtures/ward-01-minimal.json"),
        )
        .unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("ward-h/gbk.json"),
        dirty_bytes("ward-h-001-gbk-export.json"),
    )
    .unwrap();

    let found = discover_dir(dir.path()).expect("目录扫描不应因一份 GBK 包整体失败");
    assert_eq!(found.len(), 2, "UTF-8 包与 GBK 包都要被扫到");
    let total_items: usize = found.iter().map(|(_, p)| p.items.len()).sum();
    assert_eq!(total_items, 2, "两类包各出 1 件，GBK 包不得被跳过");
    let gbk = found
        .iter()
        .find(|(p, _)| p.ends_with("gbk.json"))
        .expect("GBK 包在结果里");
    assert_eq!(gbk.1.encoding, SourceEncoding::Gbk);
}

// ---------------------------------------------------------------------------
// 2. UTF-8 BOM
// ---------------------------------------------------------------------------

#[test]
fn a_utf8_bom_is_stripped_without_touching_the_content() {
    let bytes = dirty_bytes("ward-h-002-utf8-bom.json");
    assert!(bytes.starts_with(&[0xEF, 0xBB, 0xBF]), "前置条件：夹具必须带 BOM");

    let pkg = parse_dirty("ward-h-002-utf8-bom.json");
    assert_eq!(pkg.encoding, SourceEncoding::Utf8Bom);
    assert_eq!(pkg.items.len(), 1, "带 BOM 不该让包解析不出来");
    let d = &pkg.items[0];
    assert_eq!(d.doc_id, "WARD-H-002-RECORD-01");
    let sec = &d.sections[0];
    assert!(
        !sec.text.starts_with('\u{FEFF}'),
        "BOM 不得留在段文本里——否则第一个字段的 char_range 会整体偏 1"
    );
    assert!(
        d.field("执行护士")
            .and_then(|f| f.source_span)
            .and_then(|s| s.slice(&sec.text))
            == Some("<EXAMPLE_NURSE>。"),
        "BOM 剥掉后字段区间必须仍能切回原文"
    );
}

// ---------------------------------------------------------------------------
// 3. 混编
// ---------------------------------------------------------------------------

#[test]
fn a_mixed_encoding_file_keeps_both_halves_readable() {
    let bytes = dirty_bytes("ward-h-003-mixed-encoding.json");
    assert!(
        std::str::from_utf8(&bytes).is_err(),
        "前置条件：混编夹具整体不是合法 UTF-8"
    );

    let pkg = parse_dirty("ward-h-003-mixed-encoding.json");
    assert_eq!(pkg.encoding, SourceEncoding::Mixed);
    assert_eq!(pkg.items.len(), 2, "UTF-8 段与 GBK 段所属的两件都要解析出来");
    assert_no_silent_drop(&pkg, 2);

    // 前半是 UTF-8
    let utf8_item = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-H-003-RECORD-01")
        .expect("UTF-8 半边");
    assert_eq!(
        utf8_item.field("约束等级").and_then(|f| f.value.clone()).as_deref(),
        Some("二级保护。")
    );
    // 后半是 GBK —— 这才是混编的意义：整体按 UTF-8 解会在这里变乱码
    let gbk_item = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-H-003-RELEASE-01")
        .expect("GBK 半边");
    assert_eq!(
        gbk_item.field("解除原因").and_then(|f| f.value.clone()).as_deref(),
        Some("病情稳定。"),
        "GBK 半边的中文必须解对，否则护士长看到的是乱码——那和「没写」无法区分"
    );
    assert!(!diags_of(&pkg, parse_code::ENCODING).iter().any(|d| d.contains(REPLACEMENT_CHAR)),
        "两种编码都合法时不该产生占位符诊断");
}

// ---------------------------------------------------------------------------
// 4. 非法码点
// ---------------------------------------------------------------------------

#[test]
fn illegal_bytes_become_placeholders_and_are_located_one_by_one() {
    let pkg = parse_dirty("ward-h-004-illegal-bytes.json");

    // 部分成功：坏字节所在的那一件**照样进结果**
    assert_eq!(pkg.items.len(), 1, "有非法字节不等于整件不可用");
    assert_no_silent_drop(&pkg, 1);

    let enc = diags_of(&pkg, parse_code::ENCODING);
    assert!(!enc.is_empty(), "非法字节必须留下 E_ENCODING 记录");
    let joined = enc.join(" | ");
    assert!(joined.contains("占位符"), "文案要说明发生了什么：{joined}");
    assert!(
        joined.contains("用 UTF-8 重新导出"),
        "文案要给出可执行的处置建议（让信息科用 UTF-8 重导），而不是只说「有乱码」：{joined}"
    );

    // 逐处记录：3 个坏字节 → 3 条带字节偏移的说明 + 1 条汇总
    let with_offset = enc
        .iter()
        .filter(|t| t.contains("字节无法解码"))
        .count();
    assert_eq!(with_offset, 3, "三个坏字节要记三处，不能合并成一句「有乱码」");
    let offsets: Vec<usize> = enc
        .iter()
        .filter_map(|t| {
            t.split("第 ")
                .nth(1)?
                .split(" 字节")
                .next()?
                .parse::<usize>()
                .ok()
        })
        .collect();
    assert_eq!(offsets.len(), 3);
    assert!(
        offsets.windows(2).all(|w| w[0] < w[1]),
        "字节偏移必须升序，便于信息科逐处回查：{offsets:?}"
    );

    // 占位符占住位 → 坏字节**之后**的正文与字段区间都不受影响
    let d = &pkg.items[0];
    let body: String = d.sections.iter().map(|s| s.text.as_str()).collect();
    assert!(body.contains(REPLACEMENT_CHAR), "坏字节处应有占位符");
    assert!(
        body.contains("观察记录：约束期间生命体征平稳。"),
        "坏字节之后的正文必须完好——这是占位符相对删字符的关键差别"
    );
}

// ---------------------------------------------------------------------------
// 5. 结构容错（缺 section / null text / 缺 type.coding / 坏日期）
// ---------------------------------------------------------------------------

#[test]
fn four_structural_defects_in_one_package_do_not_take_the_good_item_down() {
    let pkg = parse_dirty("ward-h-005-missing-section.json");

    // 6 件进去：4 件成功（section 键缺失 / 空数组 / null 段文本都是**段级**问题，
    // 件仍算成功）+ 2 件整件不可用（type.coding 缺失 / 日期写法认不出）。
    assert_eq!(pkg.items.len(), 4, "只有 type.coding 缺失与坏日期两件整件不可用");
    assert_no_silent_drop(&pkg, 6);

    // 好的那一件照常解析出全部字段
    let good = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-H-005-ASSESS-01")
        .expect("好的一件必须在结果里");
    assert_eq!(good.template_id, "assessment_report");
    assert_eq!(
        good.field("风险等级").and_then(|f| f.value.clone()).as_deref(),
        Some("二级。")
    );
}

#[test]
fn a_missing_section_array_is_reported_but_the_item_still_parses() {
    let pkg = parse_dirty("ward-h-005-missing-section.json");
    let consent = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-H-005-CONSENT-01")
        .expect("section 键整个缺失的件仍算成功");
    assert!(consent.sections.is_empty());
    // 一个段都没有 → 字段表为空。必填字段**根本没进字段表**（而不是
    // 进了表但值为空）：这两种在卡片上长得不一样——「表里没有这行」
    // 是「文书里没写」，「表里有这行但值空」是「写了但没填」，
    // 必填检查报出来的整改要求也不一样。
    assert!(consent.fields.is_empty(), "无 section 时不得凭空造出字段行");
    assert!(consent.field("患方签字").is_none());
    assert!(
        diags_of(&pkg, parse_code::STRUCTURE)
            .iter()
            .any(|t| t.contains("没有任何 section")),
        "缺 section 必须显式记诊断，不能静默当成「这文书没内容」"
    );
}

#[test]
fn a_null_section_text_keeps_the_item_and_the_other_section() {
    let pkg = parse_dirty("ward-h-005-missing-section.json");
    let record = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-H-005-RECORD-01")
        .expect("null 段文本所在件仍算成功");
    assert_eq!(record.sections.len(), 2, "null 段保留条目，不丢段");
    assert!(record.sections[1].text.is_empty(), "null 段按空段处理");
    // 另一个段的内容完好
    assert_eq!(
        record.field("约束起始时间").and_then(|f| f.value.clone()).as_deref(),
        Some("2026-03-02 23:40。")
    );
    assert!(
        diags_of(&pkg, parse_code::STRUCTURE)
            .iter()
            .any(|t| t.contains("text 为 null"))
    );
}

#[test]
fn a_missing_type_coding_and_a_bad_date_each_land_in_the_skip_list_with_a_reason() {
    let pkg = parse_dirty("ward-h-005-missing-section.json");

    let sign = pkg
        .filtered
        .iter()
        .find(|f| f.doc_id == "WARD-H-005-SIGN-01")
        .expect("缺 type.coding 的件必须进已跳过清单");
    assert_eq!(sign.reason, psychdoc_parse::SkipReason::NoDocTypeCode);
    assert!(sign.reason.as_zh().contains("缺文书类型码"));

    let release = pkg
        .filtered
        .iter()
        .find(|f| f.doc_id == "WARD-H-005-RELEASE-01")
        .expect("日期坏的件必须进已跳过清单");
    assert_eq!(release.reason, psychdoc_parse::SkipReason::BadDate);
    assert!(release.reason.as_zh().contains("日期写法认不出"));

    // 每条都带人话原因，不出现内部术语
    for f in &pkg.filtered {
        let t = f.reason.as_zh();
        assert!(
            !t.contains("resourceType") && !t.contains("ISO8601") && !t.contains("identifier"),
            "已跳过原因要能直接贴交办单：{t}"
        );
    }
}

// ---------------------------------------------------------------------------
// 6. 重复 doc_id
// ---------------------------------------------------------------------------

#[test]
fn duplicate_ids_all_go_to_the_manual_queue_and_are_never_merged() {
    let pkg = parse_dirty("ward-h-006-duplicate-id.json");

    assert_eq!(pkg.items.len(), 1, "两份重复件都不进 items，唯一 id 的件照常进");
    assert_no_silent_drop(&pkg, 3);
    assert_eq!(pkg.items[0].doc_id, "WARD-H-006-RELEASE-01");

    let dups = diags_of(&pkg, parse_code::DUPLICATE);
    assert_eq!(dups.len(), 2, "两份重复件各出一条，不合并成一条");
    assert!(dups.iter().all(|t| t.contains("WARD-H-006-RECORD-01")));
    assert!(
        dups.iter().all(|t| t.contains("2 份")),
        "文案要说清是几份撞了：{dups:?}"
    );
    assert!(
        dups[0].contains("未做合并也未做取舍"),
        "要写明为什么两条都交人工——程序无权替人决定以哪份为准"
    );
}

#[test]
fn a_duplicate_never_lets_one_copy_silently_win() {
    // 同一个 id 的两份正文不同（23:40 二级 vs 23:55 三级）。
    // 若实现改成「保留第一条」，第二份的「三级保护」会凭空消失——
    // 而三级保护是比二级更重的护理决定，丢掉它等于丢掉一条安全信息。
    let pkg = parse_dirty("ward-h-006-duplicate-id.json");
    let all: String = pkg
        .items
        .iter()
        .flat_map(|d| d.sections.iter())
        .map(|s| s.text.as_str())
        .collect();
    assert!(!all.contains("三级保护"), "重复件不该被合并进来");
    assert!(!all.contains("二级保护"), "重复件不该被取第一条塞进结果");
    // 唯一 id 那件的内容仍在
    assert!(all.contains("解除时间：2026-03-03 06:10。"));
}

#[test]
fn duplicate_detection_is_reproducible_regardless_of_entry_order() {
    let bytes = dirty_bytes("ward-h-006-duplicate-id.json");
    let a = parse_package_bytes_with(&bytes, Some(&registry())).unwrap();
    let b = parse_package_bytes_with(&bytes, Some(&registry())).unwrap();
    let codes = |p: &psychdoc_parse::discovery::DiscoveredPackage| -> Vec<String> {
        p.errors.iter().map(|d| format!("{}|{}", d.loc.doc_id, d.code)).collect()
    };
    assert_eq!(codes(&a), codes(&b), "同一份字节两次解析的诊断序列必须完全相同");
}

// ---------------------------------------------------------------------------
// 7. 超长段
// ---------------------------------------------------------------------------

#[test]
fn an_oversized_section_is_split_without_losing_a_single_character() {
    let pkg = parse_dirty("ward-h-007-oversized-section.json");
    assert_eq!(pkg.items.len(), 1);
    let d = &pkg.items[0];
    assert!(
        d.sections.len() > 2,
        "13 万字的单段必须被切开（实际 {} 段）",
        d.sections.len()
    );

    // 切分只发生在段落边界
    for s in &d.sections {
        assert!(
            s.text.chars().count() <= psychdoc_parse::LONG_SECTION_CHARS + 64,
            "块 {} 仍超限：{} 字",
            s.index,
            s.text.chars().count()
        );
    }
    // 段号连续，卡片按段号取文本才不会取空
    for (i, s) in d.sections.iter().enumerate() {
        assert_eq!(s.index as usize, i, "段号必须连续且从 0 起");
    }
    // 续块标题带序号，人工通读时知道这是同一段的第几块
    assert!(d.sections[1].title.contains("（续 "));

    // **内容零丢失**：按原始段落数核对
    let nursing = &d.sections[0];
    // 各块首尾相接（不是「用换行拼」——切分点落在换行处，换行符归后一块）
    let body: String = d
        .sections
        .iter()
        .filter(|s| s.title.starts_with("护理记录"))
        .map(|s| s.text.as_str())
        .collect();
    assert_eq!(
        body.lines().count(),
        3000,
        "3000 个段落一行不少——切分不得丢段"
    );
    assert!(body.starts_with("0001 观察记录："), "首段开头不变");
    assert!(nursing.text.contains("观察记录："), "内容逐字保留");
    assert_eq!(
        body.lines().last().unwrap().trim_start(),
        "3000 观察记录：约束期间生命体征平稳，患者无不适主诉，情绪较前稳定。",
        "末段结尾不变"
    );
}

#[test]
fn field_spans_stay_inside_their_own_block_after_splitting() {
    // char_range 口径不变：它在**所属块的文本内**。切段后若还用原段坐标，
    // 第二个块之后的字段区间会全部越界或指到别的字上。
    let pkg = parse_dirty("ward-h-007-oversized-section.json");
    let d = &pkg.items[0];
    // 切分块内本身也要能切回原文：找块里带字段的那一块
    let nursing = d
        .field("护理小结")
        .or_else(|| d.field("病情描述"))
        .expect("块内字段必须被抽出来");
    let sec = &d.sections[nursing.section_index as usize];
    assert_eq!(
        nursing.source_span.unwrap().slice(&sec.text),
        Some("约束解除后睡眠可，无跌倒风险。"),
        "切段后字段区间必须仍能切回它自己那一块的原文"
    );
}

#[test]
fn a_split_is_announced_not_done_silently() {
    let pkg = parse_dirty("ward-h-007-oversized-section.json");
    let note = diags_of(&pkg, parse_code::STRUCTURE)
        .into_iter()
        .find(|t| t.contains("超过单段"))
        .expect("切分必须显式记诊断，否则卡片上会凭空多出几段没人解释");
    assert!(note.contains("按段落边界切成"), "要说明按什么规则切的：{note}");
    assert!(note.contains("内容零丢失"), "要说明切分不改内容：{note}");
}

// ---------------------------------------------------------------------------
// 8. 守恒与不改判（回归守卫）
// ---------------------------------------------------------------------------

#[test]
fn parsing_every_dirty_fixture_never_fails_the_whole_package() {
    // 七份脏夹具逐份跑：任何一份「整包失败」都是回归。
    // 部分成功的主张在这条里被钉成可执行断言，而不是一句文档承诺。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/dirty");
    let found = discover_dir(&dir).expect("脏夹具目录扫描不应失败");
    assert_eq!(found.len(), 7, "七份脏夹具都要被扫到");
    for (path, pkg) in &found {
        assert!(
            pkg.items.len() + wholly_unresolved(pkg) > 0,
            "{} 解析出 0 件且 0 条记账——整包被静默吞了",
            path.display()
        );
    }
}

#[test]
fn tolerance_does_not_change_the_verdict_on_clean_packages() {
    // 容错层的最大风险不是「坏包处理不了」，而是**把好包判坏了**。
    // 这条拿干净的 ward-01 打底：走字节路径解析，结论必须与走字符串
    // 路径逐字段相同，且零编码诊断。
    let bytes = std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/ward-01-minimal.json"),
    )
    .unwrap();
    let reg = registry();
    let via_bytes = parse_package_bytes_with(&bytes, Some(&reg)).unwrap();
    let via_text = psychdoc_parse::parse_package_with(
        std::str::from_utf8(&bytes).unwrap(),
        Some(&reg),
    )
    .unwrap();

    assert_eq!(via_bytes.items.len(), via_text.items.len());
    assert_eq!(via_bytes.items[0].doc_id, "WARD-A-001-CONSENT-01");
    assert_eq!(via_bytes.items[0].template_id, via_text.items[0].template_id);
    assert_eq!(via_bytes.items[0].sections.len(), 2);
    assert_eq!(
        via_bytes.items[0].sections.len(),
        via_text.items[0].sections.len()
    );
    // 字段区间在两条路径上必须切出同一个子串
    let f = via_bytes.items[0].field("患方意见").unwrap();
    let sec = &via_bytes.items[0].sections[f.section_index as usize];
    assert_eq!(
        f.source_span.unwrap().slice(&sec.text),
        Some("已知情同意，同意按护理常规采取保护措施。")
    );
    assert!(diags_of(&via_bytes, parse_code::ENCODING).is_empty());
    assert!(diags_of(&via_bytes, parse_code::DUPLICATE).is_empty());
    // 好包不该被记成「已跳过」
    assert!(via_bytes.filtered.is_empty());
}

#[test]
fn the_balanced_invariant_is_available_to_callers() {
    // `is_balanced` 是给底栏「未解析 N 件」用的一致性口径：
    // 调用方能自己查「有没有件被吞了」，而不是只能相信解析层报的数。
    let pkg = parse_dirty("ward-h-005-missing-section.json");
    let parsed = ParsedBundle {
        items: pkg.items.clone(),
        errors: pkg.errors.clone(),
        entries_seen: 6,
    };
    assert!(
        parsed.is_balanced(wholly_unresolved(&pkg)),
        "6 件 = {} 成功 + {} 整件不可用",
        pkg.items.len(),
        wholly_unresolved(&pkg)
    );
    assert_eq!(wholly_unresolved(&pkg), 2, "只有缺 type.coding 与坏日期两件整件不可用");

    // 谎报一个数就会被这条不变量抓住
    let lying = ParsedBundle {
        items: pkg.items.clone(),
        errors: pkg.errors.clone(),
        entries_seen: 9,
    };
    assert!(!lying.is_balanced(wholly_unresolved(&pkg)));
}

#[test]
fn the_encoding_of_each_dirty_fixture_is_what_its_name_says() {
    // 夹具与断言必须对得上：哪天 GBK 夹具被编辑器存成 UTF-8，
    // 这条会先红，测试不会「绿着但已经不测 GBK 了」。
    let cases = [
        ("ward-h-001-gbk-export.json", SourceEncoding::Gbk),
        ("ward-h-002-utf8-bom.json", SourceEncoding::Utf8Bom),
        ("ward-h-003-mixed-encoding.json", SourceEncoding::Mixed),
        // UTF-8 文件里混进坏字节：主体仍是 UTF-8，坏字节逐个替换
        ("ward-h-004-illegal-bytes.json", SourceEncoding::Utf8),
        ("ward-h-005-missing-section.json", SourceEncoding::Utf8),
        ("ward-h-006-duplicate-id.json", SourceEncoding::Utf8),
        ("ward-h-007-oversized-section.json", SourceEncoding::Utf8),
    ];
    for (name, expected) in cases {
        let d = tolerance::decode_export_bytes(&dirty_bytes(name));
        assert_eq!(d.encoding, expected, "{name} 的实际编码与夹具名不符");
    }
}

#[test]
fn no_dirty_fixture_carries_patient_identifiers() {
    // 脱敏守卫：脏夹具最容易在「随手造数据」时混进真名字样式串。
    // 编码是 GBK 的那份先解码再扫，否则扫的是乱码，等于没扫。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures/dirty");
    let banned = ["身份证", "patientName", "\"姓名\"", "住院号", "000000198"];
    for (path, _) in discover_dir(&dir).unwrap() {
        let decoded = tolerance::decode_export_bytes(&std::fs::read(&path).unwrap());
        for b in banned {
            assert!(
                !decoded.text.contains(b),
                "{} 命中禁用标识 {b}",
                path.display()
            );
        }
    }
}
