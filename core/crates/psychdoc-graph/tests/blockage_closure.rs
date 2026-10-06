//! Task 21 集成测试：阻塞传播闭包、欠件天数与正反向对拍。
//!
//! 归档核齐视图真正要算的两件事，本文件逐条钉死：
//!
//! | 用例 | 钉住的行为 |
//!---|---|
//! | `a_missing_root_propagates_to_every_downstream_item` | 真实脱敏夹具：缺知情同意 → 约束记录（1 级）+ 约束解除记录（2 级）**连带**被卡 |
//! | `a_diamond_dependency_does_not_double_count_the_meeting_point` | 菱形 A→B→D / A→C→D：D **只出现一次**，但 roots 保住两个 |
//! | `a_chain_deeper_than_five_keeps_exact_depths` | 深度 6 的链式依赖，每层深度逐一核对 |
//! | `a_single_node_graph_has_no_outgoing_blockage` | 单点图：无缺件即无阻塞，闭包为空而不是报错 |
//! | `the_blockage_vanishes_as_soon_as_the_item_is_restored` | **补齐后**闭包、阻塞边、角标同时消失 |
//! | `soft_gaps_never_propagate` | `requires: false` 的参考缺件不阻断、不传播 |
//! | `forward_and_backward_union_equals_the_reachable_set` | 正反两面答案逐件一致（全图逐节点），且正反向确实不同 |
//! | `pairing_failure_is_reported_not_silently_accepted` | 端点不在节点表 / 两面打架时对拍必须报 `E_DIR_PAIRING` |
//| `a_cyclic_graph_refuses_to_propagate` | 成环 → `E_DEP_CYCLE_PROPAGATION`，不给假深度 |
//| `missing_days_uses_calendar_days_not_24_hour_blocks` | 跨零点 23:40 → 次日 00:10 是**第 1 天**不是 0 天 |
//| `the_oldest_missing_item_sets_the_encounter_age` | 同住院多条缺件 → 取**最老**一条 |
//| `an_unregistered_missing_item_never_reports_zero_days` | 没登记发现时刻 → 天数 `None` + 口径说明，**不**返回 0 |
//| `the_archive_view_is_exportable_and_carries_its_own_basis` | 视图可序列化、口径自带、不依赖调用方拼装 |

use std::collections::BTreeSet;

use chrono::{DateTime, FixedOffset};
use psychdoc_graph::{
    DocGraph, GraphEdge, GraphNode, MissingLedger, NodeKind,
};
use psychdoc_model::Binding;
use psychdoc_parse::{parse_package_with, EntityBinder};
use psychdoc_rules::{load_effective, EffectiveRules};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const RESTRAINT_SET: &str = include_str!("../../../../fixtures/ward-02-restraint-set.json");

fn rules() -> EffectiveRules {
    load_effective(RULESET).expect("交付规则集应可加载")
}

fn graph_with_rules(json: &str, rs: &EffectiveRules) -> DocGraph {
    let reg = rs.registry();
    let pkg = parse_package_with(json, Some(&reg)).expect("脱敏夹具应可解析");
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    DocGraph::build(&bound.confident, rs)
}

fn graph_of(json: &str) -> DocGraph {
    graph_with_rules(json, &rules())
}

fn at(s: &str) -> DateTime<FixedOffset> {
    DateTime::parse_from_rfc3339(s).unwrap_or_else(|e| panic!("时刻 {s} 应可解析：{e}"))
}

/// 手上工装：直接拼一张图出来，专治真实夹具凑不出菱形/深链的情况。
struct GraphForge {
    encounter: String,
    graph: DocGraph,
}

impl GraphForge {
    fn new(encounter: &str) -> Self {
        Self {
            encounter: encounter.to_string(),
            graph: DocGraph {
                nodes: Default::default(),
                edges: Vec::new(),
                soft_gaps: Vec::new(),
                manual_queue: Vec::new(),
                self_loops: Vec::new(),
                cycles: Vec::new(),
            },
        }
    }

    /// 加一个真实件。件 id 自拟，住院别名一律取本工装持有的那个——
    /// 让 id 自己编码住院别名，测试里的 id 就得长得像真实件，
    /// 换个断言就得连带改一堆字符串。
    fn item(mut self, id: &str, template: &str) -> Self {
        self.graph.nodes.insert(
            id.to_string(),
            GraphNode {
                node_id: id.to_string(),
                encounter_alias: self.encounter.clone(),
                template_id: template.to_string(),
                kind: NodeKind::Present,
                doc_id: Some(id.to_string()),
                source_system: "emr".into(),
                binding: Binding::Confident,
            },
        );
        self
    }

    /// 加一个缺件占位（还没有人建出来的那一份）。
    fn missing(mut self, encounter: &str, template: &str) -> Self {
        let id = GraphNode::missing_id(encounter, template);
        self.graph.nodes.insert(
            id.clone(),
            GraphNode {
                node_id: id,
                encounter_alias: encounter.to_string(),
                template_id: template.to_string(),
                kind: NodeKind::Missing,
                doc_id: None,
                source_system: String::new(),
                binding: Binding::Confident,
            },
        );
        self
    }

    /// `later` 要求先有 `earlier`。
    fn requires(mut self, later: &str, earlier: &str) -> Self {
        self.push_edge(later, earlier, true);
        self
    }

    fn push_edge(&mut self, from: &str, to: &str, requires: bool) {
        let e = GraphEdge {
            from: from.to_string(),
            to: to.to_string(),
            requires,
            blocking: requires && self.graph.nodes.get(to).is_some_and(|n| n.is_missing()),
            spec_from: self
                .graph
                .nodes
                .get(from)
                .map(|n| n.template_id.clone())
                .unwrap_or_default(),
            spec_to: self
                .graph
                .nodes
                .get(to)
                .map(|n| n.template_id.clone())
                .unwrap_or_default(),
        };
        if requires {
            self.graph.edges.push(e);
        } else {
            self.graph.soft_gaps.push(e);
        }
    }

    fn build(self) -> DocGraph {
        self.graph
    }
}

const ENC: &str = "WARD-T-001";

// ---------------------------------------------------------------------------
// 一、闭包传播
// ---------------------------------------------------------------------------

#[test]
fn a_missing_root_propagates_to_every_downstream_item() {
    // 真实脱敏夹具：拿掉约束知情同意这一件，其余三件原样。
    let json = without_template(RESTRAINT_SET, "restraint_consent");
    let g = graph_of(&json);
    g.check_invariants().expect("缺件图仍应满足不变量");

    let consent_missing = GraphNode::missing_id("WARD-B-001", "restraint_consent");
    let b = g.blockage().expect("无环图应能算闭包");

    assert_eq!(
        b.roots,
        vec![consent_missing.clone()],
        "根因只有缺的那一件；连带件不是根因"
    );
    assert_eq!(b.blocked_ids().len(), 2, "约束记录与约束解除记录两件被连带卡住");

    assert_eq!(
        b.depth_of("WARD-B-001-RECORD-01"),
        Some(1),
        "约束记录直接卡在缺件上"
    );
    assert_eq!(
        b.depth_of("WARD-B-001-RELEASE-01"),
        Some(2),
        "约束解除记录被约束记录连带卡住，是第 2 级"
    );
    assert_eq!(b.max_depth, 2);
    assert!(
        !b.is_blocked("WARD-B-001-ASSESS-01"),
        "评估报告与约束链无关，不该被连带"
    );

    // 每一件都能说清「被哪几件缺件卡住」，护士长不用自己顺着图找。
    for id in ["WARD-B-001-RECORD-01", "WARD-B-001-RELEASE-01"] {
        assert_eq!(
            b.roots_of(id),
            BTreeSet::from([consent_missing.clone()]),
            "{id} 的根因应回溯到缺件那一件"
        );
    }
    let explain = b.explain("WARD-B-001-RELEASE-01", |id| {
        g.display_of(id, &rules())
    });
    assert!(
        explain.contains("连带卡住") && explain.contains("约束知情同意"),
        "文案要能直接贴交办单：{explain}"
    );
}

#[test]
fn a_diamond_dependency_does_not_double_count_the_meeting_point() {
    // 菱形：A 缺 → B、C 各要求 A → B、C 各要求 D。
    // D 出现在两条路径上，**只该记一次**；但「被两份缺件一起卡住」这条信息不能丢。
    let (g, a, _d) = diamond_graph();

    let b = g.blockage().expect("无环图应能算闭包");
    assert_eq!(b.roots, vec![a.clone()]);
    assert_eq!(
        b.blocked_ids(),
        vec!["B".to_string(), "C".to_string(), "D".to_string()],
        "D 只出现一次（两条路径都通向它）；同一份数据算两次也必须给出同一个集合"
    );
    assert_eq!(b.depth_of("D"), Some(2), "D 在第 2 级");
    assert_eq!(b.max_depth, 2);

    // 汇聚点保留全部根因（本例只有一个根因，但集合本身是完整的）。
    assert_eq!(b.roots_of("D"), BTreeSet::from([a.clone()]));

    // 真实场景里两份根因会同时卡住一件——把 A 换成另一个缺件 F 验证这一点。
    let two_roots = g_four_item_diamond();
    let b2 = two_roots.blockage().expect("无环图应能算闭包");
    assert_eq!(b2.roots.len(), 2, "两个缺件各自成根");
    assert_eq!(
        b2.roots_of("D"),
        BTreeSet::from([
            GraphNode::missing_id("WARD-T-001", "form_a"),
            GraphNode::missing_id("WARD-T-001", "form_b"),
        ]),
        "D 被两份缺件一起卡住，根因集合要全"
    );
}

#[test]
fn a_chain_deeper_than_five_keeps_exact_depths() {
    // 深度 6 的链：M(缺) → A1 → A2 → A3 → A4 → A5 → A6。
    let head = GraphNode::missing_id(ENC, "head");
    let mut f = GraphForge::new(ENC)
        .missing(ENC, "head")
        .item("A1", "step1")
        .item("A2", "step2")
        .item("A3", "step3")
        .item("A4", "step4")
        .item("A5", "step5")
        .item("A6", "step6");
    for (later, earlier) in [
        ("A1", head.as_str()),
        ("A2", "A1"),
        ("A3", "A2"),
        ("A4", "A3"),
        ("A5", "A4"),
        ("A6", "A5"),
    ] {
        f = f.requires(later, earlier);
    }
    // 补一个跟链无关的孤点：它不该被算进任何一级。
    let g = f.item("LONE", "other").build();

    let b = g.blockage().expect("无环图应能算闭包");
    assert_eq!(b.max_depth, 6, "最深应是第 6 级");
    for i in 1..=6u32 {
        assert_eq!(b.depth_of(&format!("A{i}")), Some(i), "A{i} 应是第 {i} 级");
    }
    assert!(!b.is_blocked("LONE"), "孤点不受缺件影响");
    assert_eq!(b.blocked_ids().len(), 6, "六件全部被卡，一件不多");
}

#[test]
fn a_single_node_graph_has_no_outgoing_blockage() {
    // 单件包（ward-01 只有一份知情同意）：没有任何依赖方 → 无缺件、无阻塞。
    let g = graph_of(include_str!("../../../../fixtures/ward-01-minimal.json"));
    let b = g.blockage().expect("单点图应能算闭包");
    assert!(b.roots.is_empty(), "没有缺件就没有根因");
    assert!(b.blocked.is_empty(), "没有缺件就没有连带阻塞");
    assert_eq!(b.max_depth, 0, "最大深度是 0 而不是 1");
    assert!(
        b.explain("WARD-A-001-CONSENT-01", |id| id.to_string())
            .contains("不受缺件影响"),
        "没被卡时也要有一句可显示的话"
    );
}

#[test]
fn the_blockage_vanishes_as_soon_as_the_item_is_restored() {
    let consent_missing = GraphNode::missing_id("WARD-B-001", "restraint_consent");

    let before = graph_of(&without_template(RESTRAINT_SET, "restraint_consent"));
    let b_before = before.blockage().unwrap();
    assert_eq!(b_before.blocked_ids().len(), 2, "缺件时两件被连带卡住");
    assert_eq!(before.blocking_edges().len(), 1);
    assert!(before.is_blocked("WARD-B-001-RECORD-01"));

    // 补齐：同一份文书包，知情同意回来了。
    let after = graph_of(RESTRAINT_SET);
    let b_after = after.blockage().expect("补齐后仍应能算闭包");
    assert!(
        b_after.blocked.is_empty(),
        "补齐后连带阻塞必须清空；实得：{:?}",
        b_after.blocked
    );
    assert!(after.blocking_edges().is_empty(), "补齐后阻塞边必须清空");
    assert!(!after.is_blocked("WARD-B-001-RECORD-01"));
    assert!(
        !after.nodes.contains_key(&consent_missing),
        "补齐后不应再有缺件占位节点"
    );

    // 可核对产出：视图层面「能不能归档」这个结论也跟着翻。
    let now = at("2026-03-05T09:00:00+08:00");
    let v_after = after
        .archive_view("WARD-B-001", now, &MissingLedger::new(), &rules())
        .expect("补齐后应能出视图");
    assert!(
        v_after.is_ready(),
        "成套齐全的包应判可归档；实得缺件 {:?}",
        v_after.missing
    );
    assert!(v_after.missing.is_empty());
}

#[test]
fn soft_gaps_never_propagate() {
    // ward-02 成套包：评估报告 → 住院记录 是 requires: false 的参考关系，
    // 住院记录不在包里。缺它**不得**阻断任何件，也不得进闭包。
    let g = graph_of(RESTRAINT_SET);
    assert_eq!(g.soft_gaps.len(), 1, "评估报告的住院记录参考缺件应记 soft_gap");

    let b = g.blockage().expect("软缺件不成环");
    assert!(
        b.roots.is_empty() && b.blocked.is_empty(),
        "requires: false 的缺件不产生根因也不产生阻塞；实得：{:?}",
        b
    );

    let view = g
        .archive_view("WARD-B-001", at("2026-03-05T09:00:00+08:00"), &MissingLedger::new(), &rules())
        .expect("应能出视图");
    assert_eq!(view.soft_gaps.len(), 1, "软缺件在视图里可见");
    assert!(view.soft_gaps[0].1.contains("inpatient_record"));
    assert!(
        view.blocking_edges.is_empty(),
        "软缺件不产生阻塞边"
    );
}

#[test]
fn a_cyclic_graph_refuses_to_propagate() {
    // Task 20 造出的成环规则集：约束知情同意 → 约束解除记录 → … → 约束知情同意。
    let yaml = format!(
        "{RULESET}\ndepartment:\n  dependencies:\n    - from: restraint_consent\n      to: restraint_release\n      requires: true\n"
    );
    let rs = load_effective(&yaml).expect("成环在加载期合法（字典内），到出图期才拦");
    let g = graph_with_rules(RESTRAINT_SET, &rs);
    assert!(!g.cycles.is_empty(), "前提：本用例确实造出了环");

    match g.blockage() {
        Err(e) => {
            let msg = e.to_string();
            assert!(
                msg.contains("E_DEP_CYCLE_PROPAGATION") && msg.contains("restraint_consent"),
                "报错要带错误码与环上件，规则集维护者要靠它定位改哪一行：{msg}"
            );
        }
        Ok(b) => panic!("成环图不得给出闭包；实得：{b:?}"),
    }

    // 视图同样拒绝：宁可不出，也不在环上给一份假的「差几级」。
    assert!(g.archive_view("WARD-B-001", at("2026-03-05T09:00:00+08:00"), &MissingLedger::new(), &rs).is_err());
}

// ---------------------------------------------------------------------------
// 二、正反向对拍
// ---------------------------------------------------------------------------

#[test]
fn forward_and_backward_union_equals_the_reachable_set() {
    // 真夹具 + 手工图都跑一遍。对拍是闭包算法的地基：
    // 正反两面合起来必须把整张图走遍，少走一条就意味着漏了一条边。
    for (name, g) in [
        ("ward-02 成套包", graph_of(RESTRAINT_SET)),
        ("ward-01 单件包", graph_of(include_str!("../../../../fixtures/ward-01-minimal.json"))),
        ("缺件包", graph_of(&without_template(RESTRAINT_SET, "restraint_consent"))),
        ("菱形", diamond_graph().0),
        ("深链", deep_chain()),
    ] {
        g.check_direction_pairing_all()
            .unwrap_or_else(|e| panic!("{name} 对拍失败：{e}"));
        // 逐节点再核一遍并集口径，确保不是「恰好全对」而是对拍本身写错了。
        for id in g.nodes.keys() {
            let (deps, dependents) = g.both_directions(id);
            let mut union: BTreeSet<String> = deps.union(&dependents).cloned().collect();
            union.insert(id.clone());
            for other in g.nodes.keys() {
                // 弱连通：与 id 之间存在任一方向的路径。
                let touching = g
                    .edges
                    .iter()
                    .any(|e| e.from == *id && e.to == *other || e.to == *id && e.from == *other);
                if touching {
                    assert!(
                        union.contains(other),
                        "{name}: {id} 与 {other} 有边，但并集里没有 {other}"
                    );
                }
            }
        }
    }

    // 正反向确实**不同**——否则并集不变式毫无意义（守住 Task 8 那条）。
    let g = graph_of(RESTRAINT_SET);
    let (deps, dependents) = g.both_directions("WARD-B-001-RECORD-01");
    assert_ne!(deps, dependents, "正反向答案必须不同");
    assert!(!deps.is_empty() && !dependents.is_empty());
}

#[test]
fn pairing_failure_is_reported_not_silently_accepted() {
    // 对拍是「两个方向的答案必须一致」。这里让它们**真的**不一致：
    // 手工塞一条 `requires: true` 但端点不在节点表里的边——
    // 沿边走的两个方向都会把它算进去，而 UI 画不出这个节点。
    // 真实场景里这就是「手写图时把件 id 拼错了」，
    // 不拦住的话，闭包里会出现一个归档核齐视图渲染不出来的行。
    let mut g = graph_of(RESTRAINT_SET);
    g.edges.push(psychdoc_graph::GraphEdge {
        from: "WARD-B-001-RECORD-01".into(),
        to: "WARD-B-001-CONCENSENT-01".into(), // 拼错的件 id
        requires: true,
        blocking: false,
        spec_from: "restraint_record".into(),
        spec_to: "restraint_consent".into(),
    });

    let err = g
        .check_direction_pairing("WARD-B-001-RECORD-01")
        .expect_err("端点不在节点表里，对拍必须报出来");
    assert!(err.to_string().contains("E_DIR_PAIRING"));
    assert!(
        err.detail.contains("WARD-B-001-CONCENSENT-01") && err.detail.contains("节点表"),
        "报错要指名道姓说哪个节点画不出来，排查才知道该查哪条规范：{}",
        err.detail
    );

    // 视图也必须因此出不来，而不是给一份「看起来齐全」的核齐结论。
    assert!(g
        .archive_view("WARD-B-001", at("2026-03-05T09:00:00+08:00"), &MissingLedger::new(), &rules())
        .is_err());

    // 不在图里的节点也报得清楚，不静默当成「没被卡」。
    assert!(g.check_direction_pairing("NOT-A-NODE").is_err());

    // 另一个方向：让正反两面本身打架。给缺件连一条实边，
    // 但把它从依赖图里摘掉——正向仍能走到，反向闭包却看不见它。
    let mut g2 = graph_of(&without_template(RESTRAINT_SET, "restraint_consent"));
    let phantom = GraphNode::missing_id("WARD-B-001", "restraint_consent");
    g2.edges.push(psychdoc_graph::GraphEdge {
        from: "WARD-B-001-RECORD-01".into(),
        to: "WARD-B-001-ASSESS-01".into(),
        requires: true,
        blocking: false,
        spec_from: "restraint_record".into(),
        spec_to: "assessment_report".into(),
    });
    // 正向：RECORD 仍然只等得到缺件；反向闭包：缺件也卡住了 ASSESS。
    // 两条都是自洽的——把 phantom 直接从节点表删掉，制造真正的不一致。
    g2.nodes.remove(&phantom);
    let err2 = g2
        .check_direction_pairing("WARD-B-001-RECORD-01")
        .expect_err("节点被摘掉后两面必然对不上");
    assert!(err2.to_string().contains("E_DIR_PAIRING"), "{}", err2);
}

// ---------------------------------------------------------------------------
// 三、欠件天数
// ---------------------------------------------------------------------------

#[test]
fn missing_days_uses_calendar_days_not_24_hour_blocks() {
    let g = graph_of(&without_template(RESTRAINT_SET, "restraint_consent"));
    let consent_missing = GraphNode::missing_id("WARD-B-001", "restraint_consent");
    let now = at("2026-03-05T09:00:00+08:00");

    // 跨零点：3-02 23:40 缺、3-03 00:10 查 → 第 1 天。
    let mut ledger = MissingLedger::new();
    ledger.record(&consent_missing, at("2026-03-02T23:40:00+08:00"));
    let age = g.missing_age(&consent_missing, now, &ledger).expect("缺件应有账");
    assert_eq!(
        age.days,
        Some(3),
        "按日历日差：3-02 → 3-05 是 3 天（按 24 小时整除会算成 2，与护士长的「欠了几天」对不上）"
    );
    assert!(age.basis_zh.contains("自然日"), "口径要写明按日历日差：{}", age.basis_zh);

    // 相邻两天只差 20 分钟，仍是 1 天——这正是按 24 小时整除会算错的情形。
    let mut l2 = MissingLedger::new();
    l2.record(&consent_missing, at("2026-03-02T23:50:00+08:00"));
    assert_eq!(
        g.missing_age(&consent_missing, at("2026-03-03T00:10:00+08:00"), &l2)
            .unwrap()
            .days,
        Some(1),
        "20 分钟跨零点仍是第 1 天"
    );

    // 同一天内多次查询恒为 0 天，且稳定可复现。
    let mut l3 = MissingLedger::new();
    l3.record(&consent_missing, at("2026-03-05T00:05:00+08:00"));
    for _ in 0..3 {
        assert_eq!(
            g.missing_age(&consent_missing, now, &l3).unwrap().days,
            Some(0),
            "当天缺当天查是 0 天"
        );
    }

    // 非缺件查不到账（不硬造一条出来）。
    assert!(g.missing_age("WARD-B-001-RECORD-01", now, &l3).is_none());
}

#[test]
fn the_oldest_missing_item_sets_the_encounter_age() {
    // 三条缺件各有一件真实件在等它（缺件本身不会自己成为根因——
    // 根因的定义是「有件在等它且等的是缺件」）。
    let f = GraphForge::new(ENC)
        .missing(ENC, "form_a")
        .missing(ENC, "form_b")
        .missing(ENC, "form_c")
        .item("KEEP", "record")
        .requires("KEEP", &GraphNode::missing_id(ENC, "form_a"))
        .requires("KEEP", &GraphNode::missing_id(ENC, "form_b"))
        .requires("KEEP", &GraphNode::missing_id(ENC, "form_c"));
    let g = f.build();
    let now = at("2026-03-10T08:00:00+08:00");

    let mut ledger = MissingLedger::new();
    ledger.record(GraphNode::missing_id(ENC, "form_a"), at("2026-03-05T10:00:00+08:00"));
    ledger.record(GraphNode::missing_id(ENC, "form_b"), at("2026-03-02T09:00:00+08:00"));
    ledger.record(GraphNode::missing_id(ENC, "form_c"), at("2026-03-08T15:00:00+08:00"));

    let age = g.encounter_age(ENC, now, &ledger);
    assert_eq!(age.missing_count, 3, "三条缺件都算进缺件数");
    assert_eq!(
        age.oldest_missing_node,
        Some(GraphNode::missing_id(ENC, "form_b")),
        "取最老的一条（3-02），因为它才是根因"
    );
    assert_eq!(age.days, Some(8), "3-02 → 3-10 是 8 个自然日");
    assert!(
        age.basis_zh.contains("最老"),
        "口径要写明按哪一条算的：{}",
        age.basis_zh
    );

    // 重复登记取更早的：又扫了一遍不该把「已欠 8 天」改写成「今天才发现」。
    ledger.record(GraphNode::missing_id(ENC, "form_b"), at("2026-03-09T23:00:00+08:00"));
    assert_eq!(g.encounter_age(ENC, now, &ledger).days, Some(8), "重复登记取更早时刻");

    // 并列时取 node_id 最小的一条，保证两次算出来完全一致。
    let mut tie = MissingLedger::new();
    tie.record(GraphNode::missing_id(ENC, "form_a"), at("2026-03-05T10:00:00+08:00"));
    tie.record(GraphNode::missing_id(ENC, "form_b"), at("2026-03-05T10:00:00+08:00"));
    let first = g.encounter_age(ENC, now, &tie);
    let second = g.encounter_age(ENC, now, &tie);
    assert_eq!(first, second, "并列时答案必须可复现");
    assert_eq!(first.days, Some(5));
}

#[test]
fn an_unregistered_missing_item_never_reports_zero_days() {
    let g = graph_of(&without_template(RESTRAINT_SET, "restraint_consent"));
    let consent_missing = GraphNode::missing_id("WARD-B-001", "restraint_consent");
    let now = at("2026-03-05T09:00:00+08:00");

    // 一条都没登记：天数算不出，**不是** 0。
    let empty = MissingLedger::new();
    let age = g.missing_age(&consent_missing, now, &empty).expect("缺件仍有条目");
    assert_eq!(age.days, None, "没登记发现时刻，天数必须是 None");
    assert!(
        age.basis_zh.contains("从未登记") && age.basis_zh.contains("刚缺 0 天"),
        "口径要写清为什么不按今天兜底：{}",
        age.basis_zh
    );

    let enc_age = g.encounter_age("WARD-B-001", now, &empty);
    assert_eq!(enc_age.days, None, "别名级同样算不出");
    assert_eq!(enc_age.missing_count, 1);
    assert!(enc_age.basis_zh.contains("没有任何一件登记过"));

    // 登记时刻晚于核算时刻：也是算不出，且文案要指向这条登记本身。
    let mut future = MissingLedger::new();
    future.record(&consent_missing, at("2026-03-09T10:00:00+08:00"));
    let age2 = g.missing_age(&consent_missing, now, &future).unwrap();
    assert_eq!(age2.days, None, "发现时刻晚于核算时刻不能算出负天数");
    assert!(age2.basis_zh.contains("晚于核算时刻"), "{}", age2.basis_zh);
}

// ---------------------------------------------------------------------------
// 四、视图（Task 34 渲染 / Task 31 导出的数据来源）
// ---------------------------------------------------------------------------

#[test]
fn the_archive_view_is_exportable_and_carries_its_own_basis() {
    let json = without_template(RESTRAINT_SET, "restraint_consent");
    let g = graph_of(&json);
    let consent_missing = GraphNode::missing_id("WARD-B-001", "restraint_consent");
    let now = at("2026-03-05T09:00:00+08:00");

    let mut ledger = MissingLedger::new();
    ledger.record(&consent_missing, at("2026-03-02T23:40:00+08:00"));
    let v = g
        .archive_view("WARD-B-001", now, &ledger, &rules())
        .expect("无环图应能出视图");

    assert_eq!(v.encounter_alias, "WARD-B-001");
    assert_eq!(v.as_of, now, "视图自带核算时刻，导出后不依赖读系统时钟复现");
    assert!(!v.is_ready(), "缺件未补齐不能判可归档");

    // 缺件与欠件天数都在，且天数可核对。
    assert_eq!(v.missing.len(), 1);
    assert_eq!(v.missing[0].node_id, consent_missing);
    assert_eq!(v.missing[0].days, Some(3));
    assert_eq!(v.blocking_edges.len(), 1);

    // 每一行都带「在等谁 / 卡住谁 / 被几级卡」，Task 34 直接按行画。
    let row = |id: &str| v.rows.iter().find(|r| r.node_id == id).expect("行应存在");
    let record = row("WARD-B-001-RECORD-01");
    assert!(record.present);
    assert!(record.directly_blocked, "约束记录直接卡在缺件上");
    assert_eq!(record.blocked_depth, Some(1));
    assert_eq!(record.waiting_on, vec![consent_missing.clone()]);
    assert_eq!(
        record.blocking_others,
        vec!["WARD-B-001-RELEASE-01".to_string()],
        "反向：它卡住了约束解除记录"
    );
    assert_eq!(record.display_zh, "约束记录", "展示名取院内类型字典");

    let release = row("WARD-B-001-RELEASE-01");
    assert_eq!(release.blocked_depth, Some(2), "连带件深度要标出来");
    assert!(!release.directly_blocked, "连带件不是直接卡住");
    let assess = row("WARD-B-001-ASSESS-01");
    assert!(assess.blocked_depth.is_none(), "无关件深度为空");

    // 口径说明自带：视图不依赖调用方再拼一段脚注。
    assert!(v.basis_zh.len() >= 3);
    assert!(v.basis_zh.iter().any(|b| b.contains("requires: true")));
    assert!(v.basis_zh.iter().any(|b| b.contains("自然日")));

    // 可导出：`Serialize` 落盘即 Task 31 的导出行来源，不做二次拼装。
    let dumped = serde_json::to_string_pretty(&v).expect("视图应可序列化");
    assert!(dumped.contains("restraint_consent"), "缺件要出现在导出里");
    assert!(dumped.contains("blocked_depth"));
    // 同一份数据两次导出**逐字节相同**——审计件要能被 diff。
    let again = g
        .archive_view("WARD-B-001", now, &ledger, &rules())
        .unwrap();
    assert_eq!(dumped, serde_json::to_string_pretty(&again).unwrap());

    // 人工队列进视图：成环隔离 / 绑定不唯一的件不能让归档核齐悄悄「齐了」。
    assert_eq!(v.manual_queue, Vec::<String>::new());
}

// ---------------------------------------------------------------------------
// 夹具与工装
// ---------------------------------------------------------------------------

/// 从包里拿掉某一类文书（造缺件包）。
fn without_template(json: &str, template: &str) -> String {
    let mut v: serde_json::Value = serde_json::from_str(json).unwrap();
    let entries: Vec<_> = v["entry"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|e| {
            e["resource"]["type"]["coding"][0]["code"]
                .as_str()
                .unwrap_or("")
                != template
        })
        .cloned()
        .collect();
    v["entry"] = serde_json::Value::Array(entries);
    serde_json::to_string(&v).unwrap()
}

/// A 缺 → B、C → D。
fn diamond_graph() -> (DocGraph, String, String) {
    let a = GraphNode::missing_id(ENC, "head");
    let g = GraphForge::new(ENC)
        .missing(ENC, "head")
        .item("B", "step_b")
        .item("C", "step_c")
        .item("D", "step_d")
        .requires("B", &a)
        .requires("C", &a)
        .requires("D", "B")
        .requires("D", "C")
        .build();
    (g, a, "D".to_string())
}

/// 深度 6 的链。
fn deep_chain() -> DocGraph {
    let head = GraphNode::missing_id(ENC, "head");
    let mut f = GraphForge::new(ENC)
        .missing(ENC, "head")
        .item("A1", "s1")
        .item("A2", "s2")
        .item("A3", "s3")
        .item("A4", "s4")
        .item("A5", "s5")
        .item("A6", "s6");
    for (later, earlier) in [
        ("A1", head.as_str()),
        ("A2", "A1"),
        ("A3", "A2"),
        ("A4", "A3"),
        ("A5", "A4"),
        ("A6", "A5"),
    ] {
        f = f.requires(later, earlier);
    }
    f.build()
}

/// 两份缺件一起卡住 D（验证 `roots` 集合不塌成单值）。
fn g_four_item_diamond() -> DocGraph {
    let a = GraphNode::missing_id(ENC, "form_a");
    let b = GraphNode::missing_id(ENC, "form_b");
    GraphForge::new(ENC)
        .missing(ENC, "form_a")
        .missing(ENC, "form_b")
        .item("D", "merge")
        .requires("D", &a)
        .requires("D", &b)
        .build()
}
