//! Task 20 集成测试：依赖图成环检测（Tarjan SCC）与「拒绝出图」。
//!
//! 院内文书依赖规范里若写着「约束记录 → 约束知情同意」+「约束知情同意 → 约束记录」，
//! 阻塞传播（Task 21 的多级闭包）在这张图上**无解**：两件互为前置，谁都不是
//! 「最先」的一份。源产品（import 环上照样出图、只报一条提示）在这里等于给出一份
//! 永远「核不齐」的假结论，因此本项目必须**显式报 `E_DEP_CYCLE` 并拒绝出图**。
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `a_three_node_cycle_is_detected_with_a_readable_path` | 三件成环 → 环路径 `A → B → C → A` 可复现，且带规范来源行 |
//! | `cycle_nodes_are_quarantined_and_never_produce_an_edge` | 环内件整件标 ambiguous、零条边、进人工队列 |
//! | `the_graph_refuses_to_emit_its_import_maps_when_cyclic` | `dependencies_map` / `dependents_map` / `import_maps` 全返回 `E_DEP_CYCLE` |
//! | `nodes_outside_the_cycle_still_get_their_figure` | **环外**节点照常解析、照常留在图里，不因局部错误全盘失败 |
//! | `two_nodes_depending_on_each_other_are_a_cycle` | 两节点互依的最小用例 |
//! | `two_independent_cycles_are_reported_separately` | 两个不相干的环各报一次，不合并成一条 |
//! | `a_self_loop_spec_is_never_counted_as_a_cycle_twice` | `from == to` 归 `self_loops`，不进环清单（同一问题不记两笔） |
//! | `an_acyclic_graph_is_untouched` | 交付规则集 + 成套夹具：无环、正反表照常出、`cycles` 为空 |
//! | `the_cycle_error_text_names_the_nodes_and_the_spec_lines` | 报错文案要够直接贴进交办单：环路径 + 规范来源 |

use std::collections::BTreeMap;

use psychdoc_graph::{
    cycle, CycleAudit, DependencyCycle, DocGraph, GraphEdge, GraphNode, InvariantError, NodeKind,
};
use psychdoc_model::Binding;
use psychdoc_parse::{parse_package_with, EntityBinder};
use psychdoc_rules::{load_effective, EffectiveRules};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const RESTRAINT_SET: &str = include_str!("../../../../fixtures/ward-02-restraint-set.json");

const CONSENT: &str = "WARD-B-001-CONSENT-01";
const RECORD: &str = "WARD-B-001-RECORD-01";
const RELEASE: &str = "WARD-B-001-RELEASE-01";
const ASSESS: &str = "WARD-B-001-ASSESS-01";

/// 在交付规则集之上追加一条科室级依赖规范。
fn rules_with_extra_dep(extra: &str) -> EffectiveRules {
    let yaml = format!("{RULESET}\ndepartment:\n  dependencies:\n{extra}");
    load_effective(&yaml).expect("类型码都在字典内，加载期合法；成环要到出图期才拦")
}

fn graph_with_rules(json: &str, rs: &EffectiveRules) -> DocGraph {
    let reg = rs.registry();
    let pkg = parse_package_with(json, Some(&reg)).expect("脱敏夹具应可解析");
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    DocGraph::build(&bound.confident, rs)
}

// ---------------------------------------------------------------------------
// 1. 环检得出来、路径可读
// ---------------------------------------------------------------------------

/// 补上「约束知情同意 → 约束解除记录」这一行，成环：
///
/// ```text
/// 约束记录 → 约束知情同意 → 约束解除记录 → 约束记录
/// ```
#[test]
fn a_three_node_cycle_is_detected_with_a_readable_path() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_release\n      requires: true\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    assert_eq!(g.cycles.len(), 1, "应恰好报出一个环；实得：{:?}", g.cycles);
    let c = &g.cycles[0];

    // 环上就是互为前置的三件；评估报告在环外。
    let mut members = c.nodes.clone();
    members.sort();
    assert_eq!(members, vec![CONSENT, RECORD, RELEASE], "环内三件");
    assert!(!c.nodes.contains(&ASSESS.to_string()), "评估报告不在环上");
    assert_eq!(c.encounter_alias, "WARD-B-001", "边不跨住院，环必属一次住院");

    // 环路径首尾同节点，能直接照着去改规则集。
    assert_eq!(c.path.len(), c.nodes.len() + 1, "路径比节点多一个回到起点的尾");
    assert_eq!(c.path.first(), c.path.last(), "环路径必须首尾同节点");
    assert_eq!(c.edges.len(), c.path.len() - 1, "每段路径配一条边");
    for (i, e) in c.edges.iter().enumerate() {
        assert_eq!(e.from, c.path[i], "第 {i} 条边的起点应与路径第 {i} 段对齐");
        assert_eq!(e.to, c.path[i + 1], "第 {i} 条边的终点应与路径第 {} 段对齐", i + 1);
    }
}

#[test]
fn cycle_paths_are_reproducible() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_release\n      requires: true\n");

    // 同一张图建两次，环路径逐字相同——报错文案要能 diff，
    // 否则每次报错都得人工比对是哪条环。
    let a = graph_with_rules(RESTRAINT_SET, &rs);
    let b = graph_with_rules(RESTRAINT_SET, &rs);
    assert_eq!(a.cycles, b.cycles, "同一输入两次建图必得同一环路径");
    assert_eq!(a.audit_cycles().describe_zh(), b.audit_cycles().describe_zh());

    // 再问一次也一致（`audit_cycles` 是只读的）。
    assert_eq!(a.audit_cycles(), a.audit_cycles());
    assert_eq!(
        cycle::detect_cycles(&a.nodes, &a.edges).len(),
        0,
        "隔离已在 build 期完成，单独再检这张图不该再找到环"
    );
}

// ---------------------------------------------------------------------------
// 2. 拒绝出图
// ---------------------------------------------------------------------------

#[test]
fn cycle_nodes_are_quarantined_and_never_produce_an_edge() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_release\n      requires: true\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    // 环内件一条边都不许有——留着就等于告诉病案室「这份在等那份」。
    for e in g.edges.iter().chain(g.soft_gaps.iter()) {
        assert!(
            !g.is_in_cycle(&e.from) && !g.is_in_cycle(&e.to),
            "环内件不得连出任何边（含参考边）：{e:?}"
        );
    }
    for id in [CONSENT, RECORD, RELEASE] {
        assert!(g.is_in_cycle(id), "{id} 应在环上");
        assert_eq!(
            g.nodes[id].binding,
            Binding::Ambiguous,
            "{id} 应整件标 ambiguous"
        );
        let q = g
            .manual_queue
            .iter()
            .find(|m| m.doc_id == id)
            .unwrap_or_else(|| panic!("{id} 应进人工队列"));
        assert!(
            q.reason.contains("成环") && q.reason.contains("人工确认"),
            "人工队列理由要写清是规则集成环、要人工改规范：{}",
            q.reason
        );
    }
}

#[test]
fn the_graph_refuses_to_emit_its_import_maps_when_cyclic() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_release\n      requires: true\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    // 自检直接给环码，并带上人可读路径。
    match g.check_invariants() {
        Err(InvariantError::Cycle(text)) => {
            assert!(text.contains("成环"), "报错要说明是成环：{text}");
            assert!(
                text.contains("restraint_consent") && text.contains("restraint_release"),
                "报错要带依赖规范来源行（类型码），否则维护者不知道改哪一行：{text}"
            );
        }
        other => panic!("成环图应报 E_DEP_CYCLE，实得：{other:?}"),
    }

    // 正向表、反向表、合并取表——三个入口一起拒绝，不给「只出一半方向」的中间态。
    for label in ["dependencies_map", "dependents_map", "import_maps"] {
        let got = match label {
            "dependencies_map" => g.dependencies_map().map(|_| ()),
            "dependents_map" => g.dependents_map().map(|_| ()),
            _ => g.import_maps().map(|_| ()),
        };
        assert!(
            matches!(got, Err(InvariantError::Cycle(_))),
            "{label} 在成环图上必须拒绝出图"
        );
    }
}

#[test]
fn the_cycle_error_text_names_the_nodes_and_the_spec_lines() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_release\n      requires: true\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);
    let text = g.audit_cycles().describe_zh();

    // 这段文案会直接进交办单：环路径 + 规范来源 + 为什么无解。
    assert!(text.contains("→"), "要给出环路径：{text}");
    assert!(text.contains("互为前置"), "要说清环上为什么核不齐：{text}");
    assert!(text.contains("需人工确认先后顺序"), "要写清要人工做什么：{text}");
    // 规范来源写成「类型码 A → 类型码 B（必前置）」，维护者能照着定位。
    assert!(
        text.contains("restraint_record → restraint_consent"),
        "要带上原有那条规范来源：{text}"
    );
    assert!(
        text.contains("restraint_consent → restraint_release"),
        "要带上新补那条规范来源：{text}"
    );
}

// ---------------------------------------------------------------------------
// 3. 环外节点照常出图（局部错误不连坐）
// ---------------------------------------------------------------------------

#[test]
fn nodes_outside_the_cycle_still_get_their_figure() {
    // 只造一个两节点环：约束记录 ↔ 约束知情同意。评估报告（soft_gap）与
    // 约束解除记录都在环外，用来证明它们没被连坐。
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_record\n      requires: true\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    let mut members: Vec<&str> = g.cycles[0].nodes.iter().map(String::as_str).collect();
    members.sort();
    assert_eq!(members, vec![CONSENT, RECORD], "只该这两个成环");

    // 环外的两件仍在节点表里，且维持 Confident。
    for id in [RELEASE, ASSESS] {
        assert!(g.nodes.contains_key(id), "环外节点 {id} 不该被赶出节点表");
        assert_eq!(g.nodes[id].binding, Binding::Confident, "{id} 不该被连坐成 ambiguous");
        assert!(!g.is_in_cycle(id), "{id} 不该被标成环内");
        assert!(
            !g.manual_queue.iter().any(|m| m.doc_id == id),
            "{id} 不该被塞进人工队列"
        );
    }

    // 环外的缺件占位节点与 soft_gap 照常保留：评估报告的「参考住院记录」与环无关。
    assert_eq!(g.soft_gaps.len(), 1, "环外的 soft_gap 不该被一起清掉");
    assert_eq!(g.soft_gaps[0].spec_from, "assessment_report");
    assert!(
        g.nodes.values().any(|n| n.is_missing() && n.template_id == "inpatient_record"),
        "环外的缺件占位节点照常建出来"
    );
}

// ---------------------------------------------------------------------------
// 4. 最小用例：两节点互依
// ---------------------------------------------------------------------------

#[test]
fn two_nodes_depending_on_each_other_are_a_cycle() {
    let rs = rules_with_extra_dep("    - from: restraint_consent\n      to: restraint_record\n      requires: true\n");
    let c = cycle_of(&graph_with_rules(RESTRAINT_SET, &rs));

    assert_eq!(c.nodes, vec![CONSENT, RECORD]);
    assert_eq!(c.path.len(), 3, "两节点环的路径是 3 段");
    assert_eq!(c.path[0], c.path[2], "首尾同节点");
    assert_eq!(c.edges.len(), 2, "两条边配成环");
}

// ---------------------------------------------------------------------------
// 5. 两个不相干的环各报一次
// ---------------------------------------------------------------------------

#[test]
fn two_independent_cycles_are_reported_separately() {
    // 手工拼一张图：{1,2} 一个环、{3,4} 另一个环，5 挂在 3 上但不成环。
    let (nodes, edges) = hand_made_graph();
    let audit = cycle::detect_cycles(&nodes, &edges);

    assert_eq!(audit.len(), 2, "两个不相干的环各报一次，不合并：{:?}", audit.cycles);
    assert_eq!(audit.cycles[0].nodes, vec!["N1", "N2"]);
    assert_eq!(audit.cycles[1].nodes, vec!["N3", "N4"]);
    // 环按节点 id 排序，报错顺序稳定。
    assert_eq!(audit.node_ids().len(), 4, "四个节点在环上");
    // N5 参与了一个强连通分量吗？没有——它在环外。
    assert!(!audit.node_ids().contains("N5"));
    assert!(audit.describe_zh().lines().count() == 2, "两个环 = 两行说明");
}

// ---------------------------------------------------------------------------
// 6. 自环不重复记账
// ---------------------------------------------------------------------------

#[test]
fn a_self_loop_spec_is_never_counted_as_a_cycle_twice() {
    // 院内把「约束记录」写成依赖自己。收集期已把它收进 `self_loops` 且不连边。
    let rs = rules_with_extra_dep("    - from: restraint_record\n      to: restraint_record\n");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    assert_eq!(g.self_loops.len(), 1, "自环应被收进 self_loops");
    assert!(
        g.cycles.is_empty(),
        "自环不进环清单：同一问题记两笔，护士长会收到两条重复的整改要求；实得 {:?}",
        g.cycles
    );
    assert!(g.check_invariants().is_ok(), "只有自环记录、无自环边的图仍合法");
    // 原图的正反表照常出：自环没有连边，不影响别的件。
    assert!(g.dependencies_map().is_ok() && g.dependents_map().is_ok());
}

// ---------------------------------------------------------------------------
// 7. 无环图不受影响
// ---------------------------------------------------------------------------

#[test]
fn an_acyclic_graph_is_untouched() {
    let g = graph_with_rules(RESTRAINT_SET, &load_effective(RULESET).unwrap());

    assert!(g.cycles.is_empty(), "交付规则集不该有成环");
    assert!(g.audit_cycles().is_clean());
    assert_eq!(g.audit_cycles().describe_zh(), "", "无环不给空文案以外的东西");
    g.check_invariants().expect("无环图应满足全部不变量");
    for id in g.nodes.keys() {
        assert!(!g.is_in_cycle(id), "{id} 不该被标成环内");
    }
    let (fwd, rev) = g.import_maps().expect("无环图正反两表照常出");
    assert_eq!(fwd.edge_count(), rev.edge_count());
    assert_eq!(fwd.edge_count(), g.edges.len());
}

// ---------------------------------------------------------------------------
// 8. 纯函数层：环检测算法本身
// ---------------------------------------------------------------------------

#[test]
fn a_dag_has_no_cycle() {
    let (nodes, edges) = hand_made_graph();
    // 去掉两条回边（N2→N1、N4→N3）后剩下的就是一张 DAG。
    let acyclic: Vec<GraphEdge> = edges
        .iter()
        .filter(|e| !((e.from == "N2" && e.to == "N1") || (e.from == "N4" && e.to == "N3")))
        .cloned()
        .collect();
    let audit = cycle::detect_cycles(&nodes, &acyclic);
    assert!(audit.is_clean(), "去掉两条回边后是 DAG，不该报环：{:?}", audit.cycles);

    // 只去掉一条回边仍应报**剩下那一个**环：算法不是「发现第一个就收工」。
    let one_back_edge_left: Vec<GraphEdge> = edges
        .iter()
        .filter(|e| !(e.from == "N2" && e.to == "N1"))
        .cloned()
        .collect();
    let audit = cycle::detect_cycles(&nodes, &one_back_edge_left);
    assert_eq!(audit.len(), 1, "N3↔N4 仍成环");
    assert_eq!(audit.cycles[0].nodes, vec!["N3", "N4"]);
}

#[test]
fn soft_gap_only_relations_never_form_a_reported_cycle() {
    // `requires: false` 的参考关系不成环：参考边缺了只提示不阻断，
    // 环在这种边上不会让归档核齐无解，报出来只会淹掉真正该报的行。
    let mut nodes = BTreeMap::new();
    for id in ["A", "B"] {
        nodes.insert(
            id.to_string(),
            node(id, "restraint_record", NodeKind::Present),
        );
    }
    let edges = vec![
        soft_edge("A", "B"),
        soft_edge("B", "A"),
    ];
    let audit = cycle::detect_cycles(&nodes, &edges);
    assert!(audit.is_clean(), "只有 soft_gap 边时不该报环：{:?}", audit.cycles);
}

#[test]
fn a_dangling_endpoint_never_shows_up_in_a_cycle() {
    // 手工拼的坏图：边指向节点表里没有的 id。它该由 E_DEP_DANGLING 报，
    // 不该混进环清单（两件事分开报，才知道是数据坏还是规范错）。
    let (mut nodes, mut edges) = hand_made_graph();
    edges.push(hard_edge("N5", "NOT-A-NODE"));
    let audit = cycle::detect_cycles(&nodes, &edges);
    assert_eq!(audit.len(), 2, "悬空端点不改变已检出的两个环");
    assert!(!audit.node_ids().contains("NOT-A-NODE"));
    nodes.clear();
    assert!(
        cycle::detect_cycles(&nodes, &edges).is_clean(),
        "节点表全空时不该凭空报环"
    );
}

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

fn cycle_of(g: &DocGraph) -> DependencyCycle {
    assert_eq!(g.cycles.len(), 1, "应恰好一个环；实得 {:?}", g.cycles);
    g.cycles[0].clone()
}

fn node(id: &str, template: &str, kind: NodeKind) -> GraphNode {
    GraphNode {
        node_id: id.to_string(),
        encounter_alias: "WARD-T-001".to_string(),
        template_id: template.to_string(),
        kind,
        doc_id: Some(id.to_string()),
        source_system: "nursing".to_string(),
        binding: Binding::Confident,
    }
}

fn hard_edge(from: &str, to: &str) -> GraphEdge {
    GraphEdge {
        from: from.to_string(),
        to: to.to_string(),
        requires: true,
        blocking: false,
        spec_from: from.to_string(),
        spec_to: to.to_string(),
    }
}

fn soft_edge(from: &str, to: &str) -> GraphEdge {
    GraphEdge {
        requires: false,
        ..hard_edge(from, to)
    }
}

/// `N1 ↔ N2` 一个环、`N3 ↔ N4` 另一个环，`N5` 挂在 `N3` 上（在环外）。
fn hand_made_graph() -> (BTreeMap<String, GraphNode>, Vec<GraphEdge>) {
    let mut nodes = BTreeMap::new();
    for id in ["N1", "N2", "N3", "N4", "N5"] {
        nodes.insert(id.to_string(), node(id, "restraint_record", NodeKind::Present));
    }
    let edges = vec![
        hard_edge("N1", "N2"),
        hard_edge("N2", "N1"),
        hard_edge("N3", "N4"),
        hard_edge("N4", "N3"),
        hard_edge("N5", "N3"),
    ];
    (nodes, edges)
}

/// 让 `CycleAudit` 的空判断有测试覆盖（不是所有 `len()==0` 都值得单独一条，
/// 但公开 API 的边界行为要有）。
#[test]
fn an_empty_audit_is_clean() {
    let a = CycleAudit::default();
    assert!(a.is_clean() && a.is_empty());
    assert_eq!(a.len(), 0);
    assert!(a.node_ids().is_empty());
    assert_eq!(a.describe_zh(), "");
}
