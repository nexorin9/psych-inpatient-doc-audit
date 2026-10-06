//! Task 4 集成测试：6 份脱敏夹具 → 件数与解析成功率**可核对**。
//!
//! 每份夹具一个用例，断言「成功几件 / 被跳过几件 / 跳过原因是什么」。
//! 这份文件的作用是把「发现 → 过滤 → 绑定 → 时间序」四步的产出钉死，
//! 避免后续任务改动时把某类件的静默丢弃行为带回来。

use psychdoc_model::{Binding, DocTypeRegistry, TimestampKind};
use psychdoc_parse::{build_timeline, discover_dir, parse_package_with, EntityBinder, SkipReason};

const REGISTRY: &str = include_str!("../../../../rulesets/hospital.yaml");

fn registry() -> DocTypeRegistry {
    serde_yaml::from_str(REGISTRY).expect("院内规则集 YAML 合法")
}

fn fixture(name: &str) -> String {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fixtures")
        .join(name);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("读不到夹具 {}: {e}", p.display()))
}

fn skip_counts(pkg: &psychdoc_parse::discovery::DiscoveredPackage) -> Vec<(SkipReason, usize)> {
    let mut v: Vec<(SkipReason, usize)> = Vec::new();
    for f in &pkg.filtered {
        match v.iter_mut().find(|(r, _)| *r == f.reason) {
            Some((_, n)) => *n += 1,
            None => v.push((f.reason, 1)),
        }
    }
    v.sort_by_key(|(r, _)| format!("{r:?}"));
    v
}

#[test]
fn six_fixtures_parse_with_checkable_item_counts() {
    // 夹具 → (成功件数, 期望的跳过原因分布)
    /// 夹具名 → (成功件数, 期望的跳过原因分布)
    type Expected = (&'static str, usize, Vec<(SkipReason, usize)>);
    let expected: Vec<Expected> = vec![
        ("ward-01-minimal.json", 1, vec![]),
        ("ward-02-restraint-set.json", 4, vec![]),
        // HIS + 护理系统各一份「约束记录」：两件都解析成功，但绑定层判 ambiguous
        ("ward-03-cross-system-dup.json", 2, vec![]),
        ("ward-04-missing-moments.json", 2, vec![]),
        // 1 正常 + 1 字典外类型码 + 1 缺住院别名 + 2 个非文书正文附件
        (
            "ward-05-mixed-entries.json",
            1,
            vec![
                (SkipReason::DocTypeNotInRegistry, 1),
                (SkipReason::NoEncounterAlias, 1),
                (SkipReason::NotADocument, 2),
            ],
        ),
        // 1 件含 null 段文本（部分成功仍计成功）+ 1 件日期坏 + 1 件无 section（仍成功）
        (
            "ward-06-dirty-data.json",
            2,
            vec![(SkipReason::BadDate, 1)],
        ),
        ("ward-07-cross-midnight.json", 3, vec![]),
    ];
    assert_eq!(expected.len(), 7, "夹具清单与本文件须同步");

    let reg = registry();
    for (name, want_items, want_skips) in expected {
        let pkg = parse_package_with(&fixture(name), Some(&reg))
            .unwrap_or_else(|e| panic!("{name} 解析失败: {e}"));
        assert_eq!(pkg.items.len(), want_items, "{name} 成功件数不符");
        assert_eq!(skip_counts(&pkg), want_skips, "{name} 跳过原因分布不符");
    }
}

#[test]
fn cross_system_fixture_binds_to_manual_queue_not_confident() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-03-cross-system-dup.json"), Some(&reg)).unwrap();
    let out = EntityBinder::new(Some(&reg)).bind(pkg.items);
    assert!(out.confident.is_empty(), "跨系统同名文书不得被自动配对");
    assert_eq!(out.manual_queue.len(), 2);
    assert!(out.manual_queue.iter().all(|e| e.binding == Binding::Ambiguous));
    // 人工队列要能直接回答「是哪两个系统撞了」
    let r = &out.manual_queue[0].reason;
    assert!(r.contains("his") && r.contains("nursing"), "原因写明系统：{r}");
}

#[test]
fn full_restraint_set_binds_confidently_and_orders_by_time() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-02-restraint-set.json"), Some(&reg)).unwrap();
    assert_eq!(pkg.items.len(), 4);
    let out = EntityBinder::new(Some(&reg)).bind(pkg.items);
    assert_eq!(out.confident.len(), 4);
    assert!(out.manual_queue.is_empty());

    let t = build_timeline(&out.confident);
    // 评估(09:00) → 同意(23:30) → 约束记录(23:40) → 解除(次日 06:10)
    let order: Vec<&str> = t.entries.iter().map(|e| e.doc_id.as_str()).collect();
    assert_eq!(
        order,
        [
            "WARD-B-001-ASSESS-01",
            "WARD-B-001-CONSENT-01",
            "WARD-B-001-RECORD-01",
            "WARD-B-001-RELEASE-01"
        ]
    );
    assert_eq!(t.missing_count, 0, "完整成套文书不应有缺时刻件");
    // 约束记录早于解除记录，这是业务事实，必须可核对
    let gap = t
        .gap_seconds("WARD-B-001-RECORD-01", "WARD-B-001-RELEASE-01")
        .expect("两件都有时刻");
    assert_eq!(gap, 6 * 3600 + 30 * 60, "23:40 → 次日 06:10 实为 6 小时 30 分");
}

#[test]
fn missing_moment_fixture_flags_every_item_and_keeps_raw_text() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-04-missing-moments.json"), Some(&reg)).unwrap();
    assert_eq!(pkg.items.len(), 2);
    let out = EntityBinder::new(Some(&reg)).bind(pkg.items);
    let t = build_timeline(&out.confident);
    assert_eq!(t.missing_count, 2, "两份都没有任何可定序时刻");
    for e in &t.entries {
        assert!(e.moment.is_none(), "{} 不得被填上当前时间", e.doc_id);
        assert!(e.missing_moment);
        assert!(!e.missing_kinds.is_empty());
    }
    // 「23时45分」这类只写时分无日期的字面量要留在 raw_text 里：
    // 护士长得能看见「原文写了 23:45，只是没日期」
    let sign = out
        .confident
        .iter()
        .find(|d| d.doc_id == "WARD-D-001-SIGN-01")
        .unwrap();
    let raw: Vec<&str> = sign
        .timestamps
        .iter()
        .filter_map(|ts| ts.raw_text.as_deref())
        .collect();
    assert!(
        raw.iter().any(|r| r.contains("23时45分")),
        "原文时刻文本必须保留：{raw:?}"
    );
    assert!(sign
        .timestamps
        .iter()
        .any(|ts| ts.kind == TimestampKind::BusinessMoment && ts.moment.is_none()));
}

#[test]
fn dirty_data_fixture_partially_succeeds_and_explains_each_skip() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-06-dirty-data.json"), Some(&reg)).unwrap();
    // 部分成功：null 段文本与无 section 的件仍在 items 里
    assert_eq!(pkg.items.len(), 2, "坏件不得拖垮好件");
    let ids: Vec<&str> = pkg.items.iter().map(|d| d.doc_id.as_str()).collect();
    assert!(ids.contains(&"WARD-F-001-CONSENT-01"), "null 段文本的件应保留");
    assert!(ids.contains(&"WARD-F-001-RELEASE-01"), "无 section 的件应保留");
    // 日期坏的那件被跳过，且原因可读
    assert_eq!(pkg.filtered.len(), 1);
    assert_eq!(pkg.filtered[0].doc_id, "WARD-F-001-RECORD-01");
    assert_eq!(pkg.filtered[0].reason, SkipReason::BadDate);
    // 解析期诊断同样在 errors 里，没有被吞掉
    assert!(!pkg.errors.is_empty());
}

#[test]
fn cross_midnight_fixture_keeps_real_time_distance() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-07-cross-midnight.json"), Some(&reg)).unwrap();
    let out = EntityBinder::new(Some(&reg)).bind(pkg.items);
    let t = build_timeline(&out.confident);
    // 23:40（3-02）→ 次日 06:10（3-03）：按真实时间差 = 6h30m，
    // 而非「当日 06:10 - 当日 23:40」这种负数。
    let gap = t
        .gap_seconds("WARD-G-001-RECORD-01", "WARD-G-001-RELEASE-01")
        .unwrap();
    assert_eq!(gap, 6 * 3600 + 30 * 60);
    assert!(gap > 0);
}

#[test]
fn discover_dir_over_all_fixtures_is_deterministic() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures");
    let a: Vec<_> = discover_dir(&dir).unwrap().into_iter().map(|(p, _)| p).collect();
    let b: Vec<_> = discover_dir(&dir).unwrap().into_iter().map(|(p, _)| p).collect();
    assert_eq!(a, b);
    // 夹具库随任务增长（Task 6 补了 ward-08/09/10 三份件-字段类夹具）；
    // 断言写成「至少 7 份」而不是固定值，是为了让「加夹具」不必回来改断言，
    // 又仍然挡住「夹具目录空了 / 扫不到」这类真问题。
    assert!(a.len() >= 7, "夹具目录应至少有 7 份可解析 json，实际 {}", a.len());
    assert!(a.iter().all(|p| p.extension().and_then(|s| s.to_str()) == Some("json")));
}

#[test]
fn business_moment_is_extracted_from_section_text_with_locatable_span() {
    let reg = registry();
    let pkg = parse_package_with(&fixture("ward-02-restraint-set.json"), Some(&reg)).unwrap();
    let record = pkg
        .items
        .iter()
        .find(|d| d.doc_id == "WARD-B-001-RECORD-01")
        .unwrap();
    let bm: Vec<_> = record
        .timestamps
        .iter()
        .filter(|t| t.kind == TimestampKind::BusinessMoment)
        .collect();
    assert!(!bm.is_empty(), "段内「2026-03-02 23:40」应被抽成业务时刻");
    let hit = bm.iter().find(|t| t.moment.is_some()).expect("有日期的时刻可定序");
    let sec = &record.sections[hit.section_index as usize];
    assert_eq!(
        hit.char_range.unwrap().slice(&sec.text),
        Some("2026-03-02 23:40"),
        "业务时刻的字面量区间必须能切回原文，卡片高亮要靠它"
    );
    // 同一件 authoredOn 与 date 都在，三类时间戳齐备
    for k in [TimestampKind::Date, TimestampKind::AuthoredOn] {
        assert!(
            record.timestamps.iter().any(|t| t.kind == k && t.moment.is_some()),
            "{k:?} 应被抽出"
        );
    }
}

#[test]
fn all_fixtures_are_free_of_patient_identifiers() {
    // 脱敏守卫：夹具里不得出现姓名/住院号/身份证样式串。
    // 走**字节**路径而不是 `read_to_string`：脏夹具目录里有 GBK 文件，
    // `read_to_string` 会在那里当场失败，等于这个守卫根本没扫到它。
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../fixtures");
    let banned = ["身份证", "patientName", "\"姓名\"", "住院号", "000000198"];
    for (path, _) in discover_dir(&dir).unwrap() {
        let decoded = psychdoc_parse::decode_export_bytes(&std::fs::read(&path).unwrap());
        for b in banned {
            assert!(
                !decoded.text.contains(b),
                "{} 命中禁用标识 {b}",
                path.display()
            );
        }
    }
}
