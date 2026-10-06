//! Task 9 的验收测试：**增量重跑**行为 + **≥200 件**真实规模批扫基准。
//!
//! 两条主线，各自独立可核对：
//!
//! 1. **增量正确性**——增量结果与全量结果逐条相等；改 1 件只重跑 1 件；
//!    恢复原内容 0 件重跑。**比对的是诊断集合，不是计数**——只比计数的话，
//!    「重跑 1 件但结果错了」这类回归会被漏过去。
//! 2. **真实规模**——≥200 件合成脱敏文书包批扫，断言耗时上限、命中数与
//!    夹具预期一致、**零静默跳过**（解析失败的件必须全部出现在
//!    `unresolved` 清单里，而不是悄悄消失）。

use std::collections::BTreeSet;
use std::time::Instant;

use psychdoc_parse::synth::{self, PackageShape, SynthParams, ViolationProfile};
use psychdoc_rules::{
    load_effective, review, review_incremental, ReviewCache, RunSummary,
};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");

/// 规模基准：≥200 件（34 次住院 × 6 件 = 204 件）。
///
/// 34 不是整数好看才选的——它让「≥200」这条硬指标成立，
/// 且批扫耗时与命中数都能被逐件核对到具体住院。
const BENCH_ENCOUNTERS: u32 = 34;
const BENCH_MIN_ITEMS: usize = 200;

/// 批扫耗时上限（release 构建，debug 下会显著更慢）。
///
/// 选 30 秒不是「随手写个大数」：合成包走的是**完整解析 + 两级规则**
/// 的生产主路径，200 件在 release 下正常是百毫秒级。给到 30 秒意味着
/// 「慢了 100 倍才红」，足以抓住真正的性能事故，又不会在慢机器上误报。
const BENCH_TIME_LIMIT_MS: u128 = 30_000;

fn rules() -> psychdoc_rules::EffectiveRules {
    load_effective(RULESET).expect("交付规则集必须可加载")
}

/// 生成一批文书包 → 解析 → 返回 `(件, 解析期问题数, 被过滤件数)`。
fn build_corpus(
    start_seq: u32,
    n: u32,
    params: SynthParams,
) -> (Vec<psychdoc_model::DocItem>, usize, usize) {
    let rules = rules();
    let reg = rules.registry();
    let mut items = Vec::new();
    let mut errors = 0usize;
    let mut filtered = 0usize;
    for pkg in synth::synth_corpus(start_seq, n, params) {
        let json = serde_json::to_string(&pkg).unwrap();
        let parsed = psychdoc_parse::parse_bundle(&json, Some(&reg)).unwrap();
        items.extend(parsed.items);
        errors += parsed.errors.len();
        // 被过滤的 entry 单独记账（parse_bundle 只把它们记进 errors，
        // 这里按「非 Composition」再数一遍，用作「零静默跳过」的比对基数）。
        filtered += pkg["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["resource"]["resourceType"] != "Composition")
            .count();
    }
    (items, errors, filtered)
}

/// 把诊断集合压成可比较的指纹串（码 + 件 + 段 + 区间 + 文案）。
fn fingerprint(result: &psychdoc_rules::ReviewResult) -> BTreeSet<String> {
    result
        .diagnostics
        .iter()
        .map(|d| {
            format!(
                "{}|{}|{}|{:?}|{}",
                d.code,
                d.loc.doc_id,
                d.loc.section_index,
                d.loc.char_range,
                d.message.text
            )
        })
        .collect()
}

fn skipped_fingerprint(result: &psychdoc_rules::ReviewResult) -> BTreeSet<String> {
    result
        .skipped_checks
        .iter()
        .map(|s| format!("{}|{}|{}|{}", s.code, s.encounter_alias, s.primary_doc, s.detail))
        .collect()
}

// ---------------------------------------------------------------------------
// 一、增量重跑：行为正确性
// ---------------------------------------------------------------------------

#[test]
fn incremental_result_is_identical_to_full_review() {
    // 这是整个增量的**根本不变量**：增量只改变「算了多少遍」，
    // 不改变「算出什么」。少算一次就说明缓存逻辑有洞。
    let params = SynthParams::default().violations(ViolationProfile {
        missing_consent_signature: true,
        out_of_domain_level: true,
        release_before_start: true,
        ..Default::default()
    });
    let (items, errors, filtered) = build_corpus(1, 5, params);
    assert!(!items.is_empty());

    let full = review(&items, &rules());
    let mut cache = ReviewCache::new();
    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    assert_eq!(
        fingerprint(&full),
        fingerprint(&inc.result),
        "首次增量跑（缓存全冷）必须与全量结果逐条相等"
    );
    assert_eq!(skipped_fingerprint(&full), skipped_fingerprint(&inc.result));
    assert_eq!(inc.summary.items_reviewed, items.len(), "冷缓存应全量重跑");
    assert_eq!(inc.summary.items_skipped, 0);
}

#[test]
fn second_run_with_no_change_reruns_nothing() {
    let (items, errors, filtered) = build_corpus(1, 4, SynthParams::default());
    let mut cache = ReviewCache::new();
    let _ = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    assert_eq!(
        inc.summary.items_reviewed, 0,
        "内容未变时一件都不该重跑，实际重跑 {} 件",
        inc.summary.items_reviewed
    );
    assert_eq!(inc.summary.items_skipped, items.len());
    assert_eq!(inc.summary.groups_reviewed, 0);
    assert_eq!(inc.summary.groups_skipped, 4);
    assert!((inc.summary.hit_rate() - 1.0).abs() < 1e-9, "命中率应为 100%");
}

#[test]
fn changing_one_item_reruns_exactly_one_item() {
    // 关键行为：改 1 件 → 只重跑 1 件。底栏的「跳过 N 件」必须是真的。
    let (items, errors, filtered) = build_corpus(1, 6, SynthParams::default());
    let mut cache = ReviewCache::new();
    let _ = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    assert_eq!(items.len(), 36);

    // 只改一个住院里的一件：把护理记录的病情描述换掉（不影响任何规则）。
    let mut changed = items.clone();
    let target = changed
        .iter()
        .position(|i| i.doc_id.ends_with("-NURSING-01") && i.encounter_alias.ends_with("W003"))
        .expect("应能定位到 W003 的护理记录");
    if let Some(sec) = changed[target].sections.first_mut() {
        sec.text = sec.text.replace("<EXAMPLE_NURSING_NOTE>", "<EXAMPLE_NURSING_NOTE_UPDATED>");
    }

    let inc = review_incremental(&changed, &rules(), &mut cache, errors + filtered);
    assert_eq!(
        inc.summary.items_reviewed, 1,
        "改 1 件只应重跑 1 件，实际 {} 件",
        inc.summary.items_reviewed
    );
    assert_eq!(inc.summary.items_skipped, changed.len() - 1);
    assert_eq!(
        inc.summary.groups_reviewed, 1,
        "该件所属的住院组指纹变了 → 该组跨件规则必须重跑"
    );
    assert_eq!(inc.summary.groups_skipped, 5, "其余住院组应整组跳过");
    // 改的内容不改变任何判定 → 结果仍与全量相等
    assert_eq!(fingerprint(&review(&changed, &rules())), fingerprint(&inc.result));
}

#[test]
fn restoring_the_original_content_reruns_nothing_again() {
    // 「改回去」不该退化成全量重跑——否则撤销一次误操作就要等半天。
    let (items, errors, filtered) = build_corpus(1, 3, SynthParams::default());
    let mut cache = ReviewCache::new();
    let _ = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    let mut changed = items.clone();
    changed[0].sections[0].text.push_str("（补记）");
    let _ = review_incremental(&changed, &rules(), &mut cache, errors + filtered);

    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    assert_eq!(
        inc.summary.items_reviewed, 0,
        "恢复原内容后应命中旧缓存，实际重跑 {} 件",
        inc.summary.items_reviewed
    );
    assert_eq!(inc.summary.items_skipped, items.len());
}

#[test]
fn cache_survives_a_process_restart_via_disk() {
    // 常驻工具的关键性质：托盘常驻进程重启后不该把昨天审过的件重审一遍。
    let (items, errors, filtered) = build_corpus(1, 3, SynthParams::default());
    let mut cache = ReviewCache::new();
    let _ = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    let dir = std::env::temp_dir().join("psychdoc-task9-cache");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("review-cache.json");
    cache.save(&path).unwrap();

    let mut reloaded = ReviewCache::load(&path).unwrap();
    let inc = review_incremental(&items, &rules(), &mut reloaded, errors + filtered);
    assert_eq!(
        inc.summary.items_reviewed, 0,
        "从磁盘读回的缓存应让本次零重跑"
    );
    assert_eq!(inc.summary.items_skipped, items.len());
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_incremental_run_carries_everything_a_full_run_carries() {
    // 增量与全量必须**逐条相等**，不只诊断相等。
    //
    // 早先 `review_incremental` 只回填了诊断与「未判定」两类，漏掉了
    // 「容差内」与「事件时间线」。后果不是少报一条，而是**带 --cache 的
    // 批扫比不带 --cache 的全量少一整段输出**：护士长问「这两件只差 3 分钟
    // 为什么没报时序倒挂」，靠的正是那条容差记录；时间线则是查「这一天顺序
    // 对不对得上」的底稿。两段一起消失 = 增量结果与全量结果不相等，
    // 与本模块头部的第一不变量直接冲突。
    let params = SynthParams::default().violations(ViolationProfile {
        missing_consent_signature: true,
        ..Default::default()
    });
    let (items, errors, filtered) = build_corpus(1, 4, params);
    let full = review(&items, &rules());

    let mut cache = ReviewCache::new();
    // 第一轮冷缓存、第二轮全命中——两条路径都要验，避免只测到其中一种。
    let cold = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    let warm = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    assert_eq!(warm.summary.items_skipped, items.len(), "第二轮应全命中");

    for (label, inc) in [("冷缓存增量", &cold.result), ("全命中增量", &warm.result)] {
        assert_eq!(
            inc.tolerated_checks.len(),
            full.tolerated_checks.len(),
            "{label} 的「容差内」条数应与全量相等"
        );
        assert_eq!(
            inc.timelines.len(),
            full.timelines.len(),
            "{label} 的事件时间线数应与全量相等（实得 {}，全量 {}）",
            inc.timelines.len(),
            full.timelines.len()
        );
        assert!(
            !full.timelines.is_empty(),
            "样本应确实产出时间线，否则本用例测不到漏回填"
        );
        // 逐条也比一遍：只比条数的话，「补了别的、漏了这条」仍能蒙混过关。
        let fp = |v: &[psychdoc_rules::ToleratedPair]| -> BTreeSet<String> {
            v.iter().map(|t| t.as_zh()).collect()
        };
        assert_eq!(
            fp(&inc.tolerated_checks),
            fp(&full.tolerated_checks),
            "{label} 的容差记录应与全量逐条相等"
        );
        let tl = |v: &[psychdoc_rules::EventTimeline]| -> BTreeSet<String> {
            v.iter().map(|t| format!("{}|{}", t.encounter_alias, t.as_zh_rows().join("/"))).collect()
        };
        assert_eq!(
            tl(&inc.timelines),
            tl(&full.timelines),
            "{label} 的时间线应与全量逐条相等"
        );
    }
}

#[test]
fn run_summary_reports_ruleset_fingerprint_and_unresolved_count() {
    // 底栏三要素：重跑 / 跳过 / 规则集短哈希，外加未解析件数。
    let (items, errors, filtered) = build_corpus(1, 2, SynthParams::default());
    let mut cache = ReviewCache::new();
    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    let zh = inc.summary.as_zh();
    assert!(zh.contains(&inc.summary.ruleset_short_sha256), "{zh}");
    assert!(zh.contains("未解析"), "{zh}");
    assert_eq!(inc.summary.ruleset_sha256, rules().sha256);
    // 未解析件数由调用方传入，必须原样透出（不能被吞掉）
    let unresolved = errors + filtered + 7;
    let inc2 = review_incremental(&items, &rules(), &mut cache, unresolved);
    assert_eq!(inc2.summary.unresolved_count, unresolved);
}

// ---------------------------------------------------------------------------
// 二、真实规模基准：≥200 件批扫
// ---------------------------------------------------------------------------

#[test]
fn two_hundred_item_batch_scan_meets_time_and_correctness_budget() {
    let params = SynthParams::default();
    let (items, errors, filtered) = build_corpus(1, BENCH_ENCOUNTERS, params);
    assert!(
        items.len() >= BENCH_MIN_ITEMS,
        "基准应至少 {} 件，实际 {} 件",
        BENCH_MIN_ITEMS,
        items.len()
    );

    let mut cache = ReviewCache::new();
    let started = Instant::now();
    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    let elapsed = started.elapsed();

    let summary: &RunSummary = &inc.summary;
    assert_eq!(summary.items_reviewed, items.len(), "冷缓存应全量重跑");
    // 账要平：每一件要么进规则层、要么进未解析清单，不允许凭空消失
    assert!(
        summary.is_balanced(),
        "账不平：总数 {} ≠ 重跑 {} + 跳过 {} + 未解析 {} + 类型不匹配 {}",
        summary.items_total,
        summary.items_reviewed,
        summary.items_skipped,
        summary.unresolved_count,
        summary.unmatched_type_count
    );
    assert_eq!(summary.unmatched_type_count, 0, "合成包类型应全在字典内");
    assert_eq!(
        summary.groups_reviewed, BENCH_ENCOUNTERS as usize,
        "每次住院各成一组"
    );

    // —— 正确性：命中数与生成器声明的违规严格对上 ——
    // 合规包必须**零命中**（误报守卫）。这是规模基准里最容易崩的一环：
    // 件一多，某个「只在单件上恰好不触发」的越界条件就会大面积漏进来。
    assert_eq!(
        inc.result.diagnostics.len(),
        0,
        "合规合成包批扫必须零命中，实际命中：{:?}",
        inc.result
            .diagnostics
            .iter()
            .map(|d| format!("{}@{}", d.code, d.loc.doc_id))
            .collect::<Vec<_>>()
    );

    // —— 零静默跳过：解析期问题 + 被过滤件必须全部有账 ——
    assert_eq!(
        summary.unresolved_count,
        errors + filtered,
        "未解析件数必须等于解析期问题数与被过滤件数之和"
    );
    let accounted = inc.result.items_reviewed + summary.unresolved_count;
    assert_eq!(
        accounted,
        items.len() + errors + filtered,
        "每一件要么进了结果、要么进了未解析清单，不允许凭空消失"
    );

    // —— 耗时上限 ——
    assert!(
        elapsed.as_millis() < BENCH_TIME_LIMIT_MS,
        "{} 件批扫耗时 {} ms，超过上限 {} ms",
        items.len(),
        elapsed.as_millis(),
        BENCH_TIME_LIMIT_MS
    );

    // 把这次实测写进基准输出，供 docs/ 下的可复现脚本引用
    println!(
        "[bench] 件数 {} · 住院组 {} · 命中 {} 条 · 未解析 {} 件 · 耗时 {} ms · 缓存条目 {} · 账平 {}",
        summary.items_total,
        summary.groups_total,
        inc.result.diagnostics.len(),
        summary.unresolved_count,
        elapsed.as_millis(),
        cache.len(),
        summary.is_balanced()
    );
}

#[test]
fn large_batch_with_violations_hits_exactly_the_expected_codes() {
    // 注入 5 类违规的规模批扫：命中码集合必须与生成器声明的**完全一致**。
    // 用「集合相等」而不是「命中数相等」——多报一条和少报一条同样致命，
    // 只比数量会把「QC-PSY-01 报成 QC-PSY-02」这种错误放过。
    let violations = ViolationProfile {
        missing_consent_signature: true,
        fullwidth_assessment_time: true,
        out_of_domain_level: true,
        wrong_duration_unit: true,
        release_before_start: true,
        ..Default::default()
    };
    let params = SynthParams::default().violations(violations);
    let (items, errors, filtered) = build_corpus(1, 20, params);

    let expected: BTreeSet<String> = synth::expected_codes(&params)
        .into_iter()
        .map(String::from)
        .collect();
    assert!(expected.contains("QC-PSY-05"), "本用例应注入 QC-PSY-05");

    let mut cache = ReviewCache::new();
    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    let actual: BTreeSet<String> = inc
        .result
        .diagnostics
        .iter()
        .map(|d| d.code.clone())
        .collect();
    assert_eq!(actual, expected, "命中码集合必须与注入的违规严格一致");

    // 每个住院应命中 6 条（QC-PSY-01~05，其中 QC-PSY-05 占两件各一条）
    let per_encounter = 20 * 6;
    assert_eq!(
        inc.result.diagnostics.len(),
        per_encounter,
        "{} 个住院 × 6 条 = {}，实际 {}",
        20,
        per_encounter,
        inc.result.diagnostics.len()
    );
    assert_eq!(inc.result.blocking_count(), inc.result.diagnostics.len());
}

#[test]
fn incremental_pass_over_a_large_batch_matches_full_review_exactly() {
    // 规模越大，增量与全量分叉的机会越多。在 ≥200 件上做一次逐条比对，
    // 比在 6 件上比对更能说明缓存键（件哈希 / 组指纹）选对了。
    let params = SynthParams::default().violations(ViolationProfile {
        missing_consent_signature: true,
        assessment_after_restraint: true,
        ..Default::default()
    });
    let (items, errors, filtered) = build_corpus(1, BENCH_ENCOUNTERS, params);
    assert!(items.len() >= BENCH_MIN_ITEMS);

    let full = review(&items, &rules());
    let mut cache = ReviewCache::new();
    let inc = review_incremental(&items, &rules(), &mut cache, errors + filtered);
    assert_eq!(
        fingerprint(&full),
        fingerprint(&inc.result),
        "≥200 件规模下增量与全量结果必须逐条相等"
    );
    assert_eq!(skipped_fingerprint(&full), skipped_fingerprint(&inc.result));
}

#[test]
fn a_changed_encounter_reruns_only_that_encounter_at_scale() {
    // 大包上的增量命中率：改 1 件 → 只重跑 1 件 + 1 个住院组。
    let (items, errors, filtered) = build_corpus(1, BENCH_ENCOUNTERS, SynthParams::default());
    let mut cache = ReviewCache::new();
    let _ = review_incremental(&items, &rules(), &mut cache, errors + filtered);

    let mut changed = items.clone();
    let idx = changed
        .iter()
        .position(|i| i.encounter_alias.ends_with("W017"))
        .expect("应能定位到 W017");
    changed[idx].sections[0].text.push_str("（补记）");

    let inc = review_incremental(&changed, &rules(), &mut cache, errors + filtered);
    assert_eq!(inc.summary.items_reviewed, 1);
    assert_eq!(inc.summary.items_skipped, changed.len() - 1);
    assert_eq!(inc.summary.groups_reviewed, 1);
    assert_eq!(
        inc.summary.groups_skipped,
        BENCH_ENCOUNTERS as usize - 1,
        "大包上其余住院组应整组跳过"
    );
    assert!(
        inc.summary.hit_rate() > 0.99,
        "大包上改 1 件的命中率应 >99%，实际 {:.2}%",
        inc.summary.hit_rate() * 100.0
    );
}

#[test]
fn signature_page_shape_keeps_the_compliant_corpus_clean() {
    // 反例守卫的反例：打开签署页且带 authoredOn 时，仍必须零命中。
    // 有了这个用例，上面「合规包零命中」才不是靠「根本没生成签署页」
    // 才成立的——签署页一旦带上，最容易漏的是 QC-PSY-08。
    let shape = PackageShape {
        sign_page: true,
        ..PackageShape::default()
    };
    let params = SynthParams::default().shape(shape);
    let (items, _errors, _filtered) = build_corpus(1, 3, params);
    assert!(
        items.iter().any(|i| i.template_id == "sign_page"),
        "本用例应确实生成了签署页"
    );
    let result = review(&items, &rules());
    assert_eq!(
        result.diagnostics.len(),
        0,
        "带签署时刻的签署页不应触发 QC-PSY-08：{:?}",
        result
            .diagnostics
            .iter()
            .map(|d| d.message.text.clone())
            .collect::<Vec<_>>()
    );

    // 反过来：抽掉 authoredOn → 恰好命中 QC-PSY-08
    let broken = SynthParams::default()
        .shape(shape)
        .violations(ViolationProfile {
            sign_page_without_moment: true,
            ..Default::default()
        });
    let (items2, _, _) = build_corpus(1, 3, broken);
    let result2 = review(&items2, &rules());
    let codes: Vec<&str> = result2.diagnostics.iter().map(|d| d.code.as_str()).collect();
    assert_eq!(codes, vec!["QC-PSY-08", "QC-PSY-08", "QC-PSY-08"]);
}