//! Task 24 集成测试：规则回归套件跑**交付用例集 × 交付规则集**。
//!
//! 单元测试用内联 YAML 与内联规则集；那份东西迟早会与
//! `rulesets/hospital.yaml` / `rulesets/rule-regress.yaml` 漂移。
//! 本文件直接读仓库里那三份，把回归套件本身钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `the_shipped_suite_is_green_with_full_coverage` | 交付用例集全绿 + 8 条码三类齐备 |
//! | `a_ruleset_change_that_downgrades_a_severity_is_caught` | 阻断 → 提示的改动跑不掉（码不变） |
//! | `a_ruleset_change_that_narrows_a_domain_is_caught_as_a_false_positive` | 合规件被多报 = 误报守卫触发 |
//! | `deleting_a_boundary_case_is_reported_as_a_coverage_gap` | 删用例不能骗过回归 |
//! | `every_shipped_case_actually_sees_the_fixture_it_names` | 切片写错件名会当场暴露 |
//! | `a_case_never_asserts_against_an_empty_slice` | 空切片的用例是假通过，必须拒 |

use std::collections::BTreeSet;

use psychdoc_parse::parse_bundle;
use psychdoc_rules::regress::{self, CaseError};
use psychdoc_rules::{load_effective, EffectiveRules};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const CASES: &str = include_str!("../../../../rulesets/rule-regress.yaml");

fn ruleset() -> EffectiveRules {
    load_effective(RULESET).expect("交付规则集应可加载")
}

fn cases() -> regress::CaseFile {
    regress::load_cases(CASES).expect("交付用例集应通过加载期校验")
}

/// 用例集里的 fixture 路径相对仓库项目根（本文件在 core/crates/psychdoc-rules/tests/）。
fn repo_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../..")
        .canonicalize()
        .expect("项目根应可解析")
}

fn resolve(rs: &EffectiveRules) -> impl FnMut(&regress::Given) -> Result<Vec<psychdoc_model::DocItem>, CaseError> + '_ {
    let root = repo_root();
    let registry = rs.registry();
    move |given: &regress::Given| {
        let path = regress::fixture_path(&root, given);
        let json = std::fs::read_to_string(&path).map_err(|e| CaseError {
            code: "E_REGRESS_FIXTURE_MISSING",
            detail: format!("{} — {e}", path.display()),
        })?;
        let parsed = parse_bundle(&json, Some(&registry)).map_err(|e| CaseError {
            code: "E_REGRESS_FIXTURE_UNPARSABLE",
            detail: e.to_string(),
        })?;
        Ok(given.slice_items(parsed.items))
    }
}

#[test]
fn the_shipped_suite_is_green_with_full_coverage() {
    let rs = ruleset();
    let file = cases();
    let report = regress::run_all(&file, &rs, None, resolve(&rs)).expect("夹具应全部可解析");

    let failed: Vec<String> = report
        .outcomes
        .iter()
        .filter(|o| !o.passed)
        .map(|o| format!("{}：{}", o.id, o.failures.join(" / ")))
        .collect();
    assert!(failed.is_empty(), "回归未通过：\n{}", failed.join("\n"));
    assert_eq!(report.failed(), 0);
    assert!(report.is_green(), "用例全绿但覆盖矩阵不该有缺位");

    // 覆盖率是「三类齐备的码占比」，不是「跑过的用例数 / 用例数」。
    assert_eq!(report.coverage.len(), 8, "交付规则集有 8 条规则码");
    assert!(report.coverage.iter().all(|r| r.is_covered()));
    assert_eq!(regress::coverage_ratio(&report.coverage), 1.0);
    assert_eq!(
        report.outcomes.len(),
        report.coverage.len() * 3,
        "每条码应当恰好三条用例（正例 / 反例 / 边界例），共 {} 条，实际 {} 条",
        report.coverage.len() * 3,
        report.outcomes.len()
    );
}

#[test]
fn every_shipped_case_actually_sees_the_fixture_it_names() {
    // 空切片会让「期望零命中」的用例变成**假通过**——夹具没读到、什么都没跑，
    // 当然零命中。用例集里有大量切片用例，这条专门把它们逐个验一遍。
    let rs = ruleset();
    for case in cases().cases {
        if case.given.doc_ids.is_empty() {
            continue;
        }
        let items = resolve(&rs)(&case.given).unwrap_or_else(|e| panic!("{}：{e}", case.id));
        let seen: BTreeSet<&str> = items.iter().map(|i| i.doc_id.as_str()).collect();
        for wanted in &case.given.doc_ids {
            assert!(
                seen.contains(wanted.as_str()),
                "用例 {} 的切片点名了 {wanted}，但夹具里没有这一件（实际只有 {:?}）——\
                 件名写错会让这条用例悄悄跑成空转",
                case.id,
                seen
            );
        }
    }
}

#[test]
fn a_ruleset_change_that_downgrades_a_severity_is_caught() {
    // 科室级把某条规则从阻断改成提示时，**命中码集合一个字都没变**。
    // 只比对码的回归套件会让这种改动静悄悄通过，而对护士长来说
    // 「被拦下」变成「只提示」是一次实实在在的口径变更。
    let rs = load_effective(&RULESET.replace("severity: blocking", "severity: notice"))
        .expect("改严重度不应让规则集加载失败");
    let file = cases();
    let report = regress::run_all(&file, &rs, None, resolve(&rs)).unwrap();

    let severity_failures = report
        .outcomes
        .iter()
        .filter(|o| o.failures.iter().any(|f| f.contains("严重度不符")))
        .count();
    assert!(
        severity_failures >= 8,
        "把 8 条码全降级后，8 条正例都该报严重度不符，实际 {severity_failures} 条"
    );
    assert!(!report.is_green(), "严重度整体降级不能算回归通过");
}

#[test]
fn a_ruleset_change_that_narrows_a_domain_is_caught_as_a_false_positive() {
    // 从约束等级的允许值里拿掉「二级保护」——院内表单上明明印着这一项。
    // 结果是每份合规包都被报一条越界：典型的误报，且是最伤信任的那一类
    // （护士长会开始觉得「这工具老乱报」，然后不再看它的清单）。
    let narrowed = RULESET.replace(
        r#"allowed: ["一级保护", "二级保护", "三级保护"]"#,
        r#"allowed: ["一级保护", "三级保护"]"#,
    );
    assert_ne!(narrowed, RULESET, "测试前提：规则集里确实有那个允许值列表");
    let rs = load_effective(&narrowed).expect("收窄允许值不应让规则集加载失败");
    let file = cases();
    let report = regress::run_all(&file, &rs, None, resolve(&rs)).unwrap();

    let guarded = report
        .outcomes
        .iter()
        .filter(|o| o.failures.iter().any(|f| f.contains("误报守卫")))
        .count();
    assert!(
        guarded >= 5,
        "收窄允许值后至少 5 条反例该触发误报守卫，实际 {guarded} 条"
    );
    // 反例一旦跑出命中，报出来的必须是**带定位**的那一条，
    // 否则维护者只知道「多了一条」，不知道多在哪一栏。
    let sample = report
        .outcomes
        .iter()
        .find(|o| o.failures.iter().any(|f| f.contains("误报守卫")))
        .expect("应有触发误报守卫的反例");
    let detail = sample
        .failures
        .iter()
        .find(|f| f.contains("误报守卫"))
        .unwrap();
    assert!(detail.contains('@') && detail.contains("#s"), "误报明细要给定位：{detail}");
    assert!(!report.is_green());
}

#[test]
fn deleting_a_boundary_case_is_reported_as_a_coverage_gap() {
    // 删掉一条反例或边界例，**所有剩下的用例照样全绿**。
    // 覆盖矩阵是唯一能发现这件事的东西，因此它必须参与 is_green()。
    let rs = ruleset();
    let file = cases();
    let trimmed: regress::CaseFile = regress::CaseFile {
        version: file.version.clone(),
        cases: file
            .cases
            .iter()
            .filter(|c| c.kind != regress::CaseKind::Boundary)
            .cloned()
            .collect(),
    };
    let report = regress::run_all(&trimmed, &rs, None, resolve(&rs)).unwrap();

    assert_eq!(report.failed(), 0, "只剩正反例时用例本身全绿");
    assert!(!report.is_green(), "覆盖矩阵缺位不能让回归算通过");
    let gaps: Vec<&str> = report
        .coverage
        .iter()
        .filter(|r| !r.is_covered())
        .map(|r| r.code.as_str())
        .collect();
    assert_eq!(gaps.len(), 8, "每条码都缺边界例：{gaps:?}");
    assert_eq!(regress::coverage_ratio(&report.coverage), 0.0);
}

#[test]
fn a_case_pointing_at_a_missing_fixture_fails_the_run_instead_of_being_skipped() {
    let rs = ruleset();
    let mut file = cases();
    file.cases[0].given.fixture = "fixtures/does-not-exist.json".into();
    let err = regress::run_all(&file, &rs, None, resolve(&rs)).unwrap_err();
    assert_eq!(err.code, "E_REGRESS_FIXTURE_MISSING", "{err}");
}

#[test]
fn the_only_filter_selects_a_subset_without_hiding_the_coverage_gaps() {
    // `--only` 是给「我刚改了 05 这一族，只想跑这几条」用的。
    // 它必须真的少跑用例，但覆盖矩阵仍按**全量用例集**统计——
    // 否则只跑一条用例就能报「覆盖率 100%」，那比没有矩阵更糟。
    let rs = ruleset();
    let file = cases();
    let all = regress::run_all(&file, &rs, None, resolve(&rs)).unwrap();
    let only = regress::run_all(&file, &rs, Some("QC-PSY-05"), resolve(&rs)).unwrap();

    assert_eq!(all.outcomes.len(), 24);
    assert_eq!(only.outcomes.len(), 3, "QC-PSY-05 的正例 / 反例 / 边界例共三条");
    assert!(only.outcomes.iter().all(|o| o.code == "QC-PSY-05"));
    assert_eq!(only.coverage.len(), all.coverage.len(), "覆盖矩阵仍按全量用例集统计");
    assert_eq!(regress::coverage_ratio(&only.coverage), 1.0);
}

#[test]
fn the_machine_readable_report_serializes_for_audit() {
    // 质控办要把「这次回归跑了什么、覆盖到没有」存进等保材料，
    // 因此报告必须能落成 json 且两次跑逐字节相同。
    let rs = ruleset();
    let report = regress::run_all(&cases(), &rs, None, resolve(&rs)).unwrap();
    let a = serde_json::to_string_pretty(&report).unwrap();
    let b = serde_json::to_string_pretty(&regress::run_all(&cases(), &rs, None, resolve(&rs)).unwrap())
        .unwrap();
    assert_eq!(a, b, "同一份规则集 + 同一份用例集的回归报告应逐字节可复现");
    assert!(a.contains("QC-PSY-01"));
    assert!(a.contains("ruleset_sha256"));
}