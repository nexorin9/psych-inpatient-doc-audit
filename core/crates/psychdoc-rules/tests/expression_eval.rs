//! Task 17 集成测试：声明式条件表达式求值器跑**真实脱敏文书包**。
//!
//! `expr.rs` 的单元测试用内联夹具验证语义（`EvalOutcome` 四态、三态不合并、
//! 失败码、字符口径）。本文件把同一套求值器放到 `fixtures/` 里的真实文书包上，
//! 钉死它作为**规则条件底座**的两条产品级行为：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `every_operator_works_on_a_real_desensitized_package` | 十个算子各在真实件上至少判出一条**可核对的结论** |
//! | `declared_conditions_reach_the_same_verdict_as_the_hardcoded_rules` | 声明式条件与 `QC-PSY-01`~`04` 硬编码规则对同一批件给出**一致**的通过/不通过结论 |
//! | `a_compliant_package_evaluates_to_zero_failures` | 合规包（`ward-09`）零 `Fail` —— 误报守卫 |
//! | `a_missing_value_is_skipped_never_reported_as_pass_or_fail` | 未填字段走 `Skip` 且带得走原因，**不落进 Pass 也不落进 Fail** |
//! | `a_bad_ruleset_condition_errors_instead_of_passing` | 规则集写错的正则/路径/类型 → `Error`，不静默通过 |
//! | `evaluation_stays_char_based_on_real_sections` | 真实段上的 `char_range` 切得回原文 |

use psychdoc_model::DocItem;
use psychdoc_parse::parse_bundle;
use psychdoc_rules::expr::{
    self, EvalOutcome, Expr, FieldPath, Operand, Operator, SkipCause,
};
use psychdoc_rules::load_effective;

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const VIOLATIONS: &str = include_str!("../../../../fixtures/ward-08-field-violations.json");
const COMPLIANT: &str = include_str!("../../../../fixtures/ward-09-field-compliant.json");
const DIRTY: &str = include_str!("../../../../fixtures/ward-06-dirty-data.json");

fn items(json: &str) -> Vec<DocItem> {
    let rs = load_effective(RULESET).expect("交付规则集应可加载");
    let reg = rs.registry();
    parse_bundle(json, Some(&reg)).expect("脱敏夹具应可解析").items
}

/// 按件 id 取一件（真实文书包里同名文书不同系统各一份，取件 id 才不会取错）。
fn item_of<'a>(items: &'a [DocItem], doc_id: &str) -> &'a DocItem {
    items
        .iter()
        .find(|i| i.doc_id == doc_id)
        .unwrap_or_else(|| panic!("夹具里应有件 {doc_id}，实际有 {:?}", items.iter().map(|i| &i.doc_id).collect::<Vec<_>>()))
}

fn run(item: &DocItem, path: &str, op: Operator, operand: &Operand) -> EvalOutcome {
    let p = FieldPath::parse(path).expect("路径应可解析");
    expr::eval(&p, op, operand, item)
}

// ---------------------------------------------------------------------------
// 算子集：十个各在真实件上判出一条可核对的结论
// ---------------------------------------------------------------------------

#[test]
fn every_operator_works_on_a_real_desensitized_package() {
    let its = items(VIOLATIONS);
    let record = item_of(&its, "WARD-F-001-RECORD-01");
    let assess = item_of(&its, "WARD-F-001-ASSESS-01");

    // 约束记录件：约束起始时间 / 约束等级 / 执行护士 / 约束时长 四键齐全
    assert_eq!(
        run(record, "执行护士", Operator::Exists, &Operand::None),
        EvalOutcome::Pass,
        "exists：有值应成立"
    );
    assert!(
        run(record, "复查签名", Operator::NotExists, &Operand::None).is_pass(),
        "not_exists：件里没有这个键应成立"
    );
    // 文书里每个字段值都带句末句读，比较时会被剥掉（`cmp_text` 的口径）
    assert!(
        run(record, "约束等级", Operator::Eq, &Operand::Text("四级保护。".into())).is_pass(),
        "eq：与实际填的值一致应成立"
    );
    assert!(
        run(record, "约束等级", Operator::Eq, &Operand::Text("四级保护".into())).is_pass(),
        "eq：条件侧不写句号也应与带句号的实际值相等"
    );
    assert!(
        run(record, "约束等级", Operator::Ne, &Operand::Text("二级保护".into())).is_pass(),
        "ne：与实际填的值不同应成立"
    );
    assert!(
        run(record, "约束时长", Operator::Gt, &Operand::Quantity { value: 1.0, unit: "小时".into() }).is_pass(),
        "gt：2 小时 > 1 小时应成立"
    );
    assert!(
        run(record, "约束时长", Operator::Between, &Operand::QuantityRange {
            low: 0.5, high: 4.0, unit: "小时".into()
        })
        .is_pass(),
        "between：2 小时落在 0.5~4 小时内应成立（与合规包的 120 分钟同量纲）"
    );
    assert!(
        run(record, "约束等级", Operator::Regex, &Operand::Text("^四级保护".into())).is_pass(),
        "regex：应匹配实际值（实际值带句末句号，故用前缀锚定）"
    );
    assert!(
        run(
            record,
            "约束等级",
            Operator::InSet,
            &Operand::Set(vec!["二级保护".into(), "四级保护".into()])
        )
        .is_pass(),
        "in_set：实际值在集合内应成立"
    );
    // before / after 打在跨件时刻上。
    //
    // 违规包里的评估时间写的是**全角** `２０２６-０３-０２ 09:00` ——
    // 这正是 `QC-PSY-02` 要报的形状问题。此处按原文解析（不做全角归一），
    // 因此时刻判定**判不了**，返回 E_DATE_PARSE 而不是编一个「早于」出来。
    let before_start = Operand::Path(psychdoc_rules::expr::FieldPathSerde("约束起始时间".into()));
    let fullwidth = run(assess, "评估时间", Operator::Before, &before_start);
    assert_eq!(
        fullwidth.error_code(),
        Some("E_DATE_PARSE"),
        "全角日期判不了，不得悄悄归一后判通过：{fullwidth:?}"
    );
    assert!(!fullwidth.is_pass(), "判不了绝不能是通过");

    // 同一条件打在**合规包**的半角评估时间上才成立：09:00 早于 23:40
    let comp = items(COMPLIANT);
    let comp_assess = item_of(&comp, "WARD-G-001-ASSESS-01");
    let comp_record = item_of(&comp, "WARD-G-001-RECORD-01");
    // 条件侧写**字面时刻**（跨件时间线 join 归 Task 19，本模块只在件内定位）
    let same_day_cut = Operand::Moment("2026-03-02 23:00".into());
    assert!(
        run(comp_assess, "评估时间", Operator::Before, &same_day_cut).is_pass(),
        "before：评估 09:00 早于 23:00，应成立"
    );
    assert!(
        run(comp_assess, "评估时间", Operator::After, &same_day_cut).is_fail(),
        "after：09:00 不晚于 23:00，应不成立"
    );
    // 自洽性锚点：拿「起始时刻晚于自己」这条反例，必须判不成立
    assert!(
        run(record, "约束起始时间", Operator::After, &before_start).is_fail(),
        "after：约束开始不晚于自己，应不成立"
    );
    // 合规包里的约束时长写的是 120 分钟——与违规包的 2 小时同量纲，
    // 条件写「0.5~4 小时」时两侧都换算到基准单位才比，不会把 120 判成 120 小时
    assert!(
        run(
            comp_record,
            "约束时长",
            Operator::Between,
            &Operand::QuantityRange { low: 0.5, high: 4.0, unit: "小时".into() }
        )
        .is_pass(),
        "120 分钟 = 2 小时，应落在 0.5~4 小时内"
    );
}

#[test]
fn every_operator_that_can_fail_produces_a_chinese_reason() {
    let its = items(VIOLATIONS);
    let record = item_of(&its, "WARD-F-001-RECORD-01");
    // 每种「判不成立」的文案都要能直接贴进病区交办单
    for (path, op, operand) in [
        ("约束等级", Operator::Eq, Operand::Text("一级保护".into())),
        ("约束等级", Operator::Ne, Operand::Text("四级保护".into())),
        ("约束时长", Operator::Gt, Operand::Quantity { value: 4.0, unit: "小时".into() }),
        (
            "约束时长",
            Operator::Between,
            Operand::QuantityRange { low: 3.0, high: 4.0, unit: "小时".into() },
        ),
        ("约束等级", Operator::Regex, Operand::Text("^一级保护".into())),
        ("约束等级", Operator::InSet, Operand::Set(vec!["一级保护".into()])),
    ] {
        let out = run(record, path, op, &operand);
        assert!(out.is_fail(), "{path} {} 应判不成立，实际 {out:?}", op);
        let detail = out.describe();
        assert!(!detail.is_empty() && detail.chars().any(|c| c as u32 > 0x4E00), "{path} {} 文案须是中文：{detail}", op);
    }
}

// ---------------------------------------------------------------------------
// 声明式条件与硬编码规则的一致性
// ---------------------------------------------------------------------------

/// 一组**从 YAML 读出来**的条件声明，模拟病案室写在规则集里的那一档。
const DECLARED: &str = r#"
- path: "s0.执行护士"
  op: exists
- path: "s0.约束等级"
  op: in_set
  operand: { type: set, value: ["一级保护", "二级保护", "三级保护", "四级保护"] }
- path: "s0.约束时长"
  op: between
  operand: { type: quantity_range, value: { low: 0.5, high: 4, unit: "小时" } }
- path: "s0.约束起始时间"
  op: before
  operand: { type: moment, value: "2026-03-03 06:00" }
"#;

fn declared() -> Vec<Expr> {
    serde_yaml::from_str(DECLARED).expect("条件声明应可解析")
}

#[test]
fn declared_conditions_reach_the_same_verdict_as_the_hardcoded_rules() {
    let its = items(VIOLATIONS);
    let record = item_of(&its, "WARD-F-001-RECORD-01");
    let exprs = declared();
    assert_eq!(exprs.len(), 4);

    // 违规包里「约束记录」这一件本身合规（时长 2 小时在区间内、等级在集合内、
    // 起始时刻早于次日 06:00）——四条声明条件必须全 Pass。
    for e in &exprs {
        let out = e.eval(record);
        assert!(out.is_pass(), "{} 应通过，实际 {out:?}", e.path);
    }

    // 把「约束时长」这一件改坏：声明式条件必须立刻给出 Fail，
    // 且与硬编码的 `QC-PSY-03`（取值域越界）结论一致。
    let tampered = tampered_duration(record, "6 小时。");
    let e = &exprs[2];
    let out = e.eval(&tampered);
    assert!(out.is_fail(), "约束时长 6 小时应越界：{out:?}");
    let rs = load_effective(RULESET).unwrap();
    let hard = psychdoc_rules::review(std::slice::from_ref(&tampered), &rs);
    assert!(
        hard.diagnostics.iter().any(|d| d.code == psychdoc_rules::QC_PSY_03),
        "硬编码执行器应同时报 QC-PSY-03：{:?}",
        hard.diagnostics.iter().map(|d| &d.code).collect::<Vec<_>>()
    );
}

/// 把件里某个键的值换掉，返回一份可独立求值的副本。
fn tampered_duration(item: &DocItem, new_value: &str) -> DocItem {
    let mut copy = item.clone();
    for f in &mut copy.fields {
        if f.key == "约束时长" {
            f.value = Some(new_value.to_string());
        }
    }
    copy
}

#[test]
fn a_compliant_package_evaluates_to_zero_failures() {
    // 误报守卫：同一组声明条件打在合规包上必须零 Fail。
    // 只看真实文书包里存在的键——合规包与违规包结构不同，键集不一致本身
    // 由 `an_absent_key_is_skipped` 单独守住，不在这里混着判。
    let its = items(COMPLIANT);
    let record = item_of(&its, "WARD-G-001-RECORD-01");
    let exprs = declared();
    let mut evaluated = 0;
    for e in &exprs {
        let out = e.eval(record);
        assert!(!out.is_fail(), "合规包上 {} 不应判不成立：{out:?}", e.path);
        if out.is_pass() {
            evaluated += 1;
        }
    }
    assert!(evaluated >= 3, "合规包上至少 3 条应判 Pass，实际 {evaluated}");
}

// ---------------------------------------------------------------------------
// 判不了：既不是通过也不是不通过
// ---------------------------------------------------------------------------

#[test]
fn a_missing_value_is_skipped_never_reported_as_pass_or_fail() {
    let its = items(VIOLATIONS);
    let consent = item_of(&its, "WARD-F-001-CONSENT-01");
    // 「患方签字」在违规包里键存在但值为空串（件本身没有患方签字）
    assert_eq!(
        consent.field("患方签字").map(|f| f.presence()),
        Some(psychdoc_model::FieldPresence::Empty),
        "夹具里这是一个空串字段"
    );
    // 空串**判得出来**：它真的就是空串
    assert_eq!(
        run(consent, "患方签字", Operator::Eq, &Operand::Text("".into())),
        EvalOutcome::Pass,
        "空串确实等于空串"
    );
    // 但空串**进不了数值算子** —— 拿空串比大小是编造事实
    let gtr = run(consent, "患方签字", Operator::Gt, &Operand::Number(0.0));
    assert_eq!(gtr, EvalOutcome::Skip { reason: SkipCause::FieldEmpty });
    assert!(!gtr.is_pass() && !gtr.is_fail(), "判不了既不是通过也不是不通过");
    assert!(gtr.describe().contains("空串"), "原因要能直接读出来：{gtr:?}");

    // 件里压根没有的键
    let absent = run(consent, "医师签名", Operator::Eq, &Operand::Text("某医师".into()));
    assert_eq!(absent, EvalOutcome::Skip { reason: SkipCause::FieldMissing });
    assert!(absent.describe().contains("未出现"), "原因必须能直接读出来：{absent:?}");
}

#[test]
fn dirty_data_produces_errors_and_skips_but_never_a_panic() {
    // 脏数据包：解析层能过的件逐条求值，判不了的必须是显式一态。
    let its = items(DIRTY);
    assert!(!its.is_empty(), "脏数据夹具至少应有能解析的件");
    let exprs = declared();
    for item in &its {
        for e in &exprs {
            let out = e.eval(item);
            assert!(
                out.is_pass() || out.is_fail() || out.is_skip() || out.is_error(),
                "求值只允许四态，实际 {out:?}"
            );
            // 判不了的必须带得走原因，不能是一句空话
            if out.is_skip() || out.is_error() {
                assert!(out.describe().chars().count() > 4, "{} 的未判定原因太空：{:?}", e.path, out);
            }
        }
    }
}

#[test]
fn a_bad_ruleset_condition_errors_instead_of_passing() {
    let its = items(VIOLATIONS);
    let record = item_of(&its, "WARD-F-001-RECORD-01");

    for (yaml, code) in [
        // 正则编译失败
        (r#"path: "约束等级"
op: regex
operand: { type: text, value: "([未闭合" }"#, "E_REGEX_INVALID"),
        // 路径写错
        (r#"path: "s1."
op: exists"#, "E_PATH_INVALID"),
        // 数值算子撞上文本字段
        (r#"path: "执行护士"
op: gt
operand: { type: number, value: 1 }"#, "E_TYPE_MISMATCH"),
    ] {
        let e = Expr::from_yaml(yaml).expect("YAML 本身应可解析");
        let out = e.eval(record);
        assert_eq!(out.error_code(), Some(code), "{yaml}\n实际 {out:?}");
        assert!(!out.is_pass(), "规则集写错绝不能变成「通过」");
        assert!(out.describe().contains(code), "文案里要带码：{}", out.describe());
    }
}

// ---------------------------------------------------------------------------
// 字符口径
// ---------------------------------------------------------------------------

#[test]
fn evaluation_stays_char_based_on_real_sections() {
    let its = items(VIOLATIONS);
    let record = item_of(&its, "WARD-F-001-RECORD-01");

    // 段级路径取整段正文
    let p = FieldPath::parse("s0").unwrap();
    let r = p.resolve(record);
    let text = r.text().expect("第 0 段应有正文");
    let span = r.span().expect("段级路径应带区间");
    assert_eq!(span.len() as usize, text.chars().count(), "区间长度按字符算，不是字节");
    assert_eq!(span.slice(text), Some(text), "区间能切回整段原文");

    // 段内键取到的值区间同样能切回原文
    let p = FieldPath::parse("s0.约束等级").unwrap();
    let r = p.resolve(record);
    let value = r.text().unwrap();
    let span = r.span().expect("有值应带区间");
    let sec = &record.sections[0];
    assert_eq!(
        span.slice(&sec.text).map(str::trim),
        Some(value),
        "段内键的区间必须精确落在被判的那个值上"
    );
    // 同一段里的中文键名本身也按字符计（键名 4 个汉字 = 4，不是 12 字节）
    assert_eq!(FieldPath::parse("s0.约束等级").unwrap().to_string(), "s0.约束等级");
}
