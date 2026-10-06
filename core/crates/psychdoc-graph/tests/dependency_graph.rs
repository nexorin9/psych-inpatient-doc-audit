//! Task 8 集成测试：文书依赖图 —— Collector → Resolver → 正反向 `ImportMap`。
//!
//! 跑**交付规则集**（`rulesets/hospital.yaml`）与**真实脱敏夹具**
//! （`fixtures/ward-02-restraint-set.json` 等），把归档核齐视图的数据基础钉死：
//!
//! | 用例 | 钉住的行为 |
//! |---|---|
//! | `a_full_restraint_set_draws_the_expected_edges` | 成套 4 件 → 3 条边，节点 4 个（评估件缺住院记录属 `soft_gap`） |
//! | `dependencies_and_dependents_differ_and_are_reproducible` | **正反向答案不同**且两次查询完全一致（源产品那条「方向开关问正反两问题」） |
//! | `a_missing_item_makes_exactly_one_blocking_edge` | 缺 1 件 → **恰好 1 条**阻塞边；补齐后该边消失 |
//! | `ambiguous_items_never_produce_edges` | 跨系统同名件（ward-03）进人工队列，**零条边** |
//! | `a_self_loop_dependency_is_refused` | `from == to` → 收进 `self_loops`，不产生边 |
//! | `a_dangling_dependency_spec_is_recorded_not_dropped` | 引用不存在的文书类型 → `dangling_specs` 有账 |
//! | `duplicate_spec_declarations_collapse_to_one_edge` | 院级 + 科室级各声明同一条边 → 去重成 1 条 |
//! | `edges_never_cross_encounter_aliases` | 边绝不跨住院别名（`check_invariants`） |
//! | `a_dangling_edge_endpoint_is_rejected` | 手工塞一条端点不存在的边 → `E_DEP_DANGLING` |

use std::collections::BTreeSet;

use psychdoc_graph::{
    Collector, Direction, DocGraph, GraphNode, InvariantError, NodeKind,
};
use psychdoc_parse::{parse_package_with, EntityBinder};
use psychdoc_rules::{load_effective, EffectiveRules};

const RULESET: &str = include_str!("../../../../rulesets/hospital.yaml");
const RESTRAINT_SET: &str = include_str!("../../../../fixtures/ward-02-restraint-set.json");
const CROSS_SYSTEM_DUP: &str = include_str!("../../../../fixtures/ward-03-cross-system-dup.json");

fn rules() -> EffectiveRules {
    load_effective(RULESET).expect("交付规则集应可加载")
}

/// 走完整生产路径：解析 → 字典过滤 → 实体绑定 → 建图。
fn graph_of(json: &str) -> DocGraph {
    graph_with_rules(json, &rules())
}

fn graph_with_rules(json: &str, rs: &EffectiveRules) -> DocGraph {
    let reg = rs.registry();
    let pkg = parse_package_with(json, Some(&reg)).expect("脱敏夹具应可解析");
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    DocGraph::build(&bound.confident, rs)
}

fn ids(set: &BTreeSet<String>) -> Vec<&str> {
    set.iter().map(String::as_str).collect()
}

#[test]
fn a_full_restraint_set_draws_the_expected_edges() {
    let g = graph_of(RESTRAINT_SET);
    g.check_invariants().expect("成图应满足不变量");

    // 成套 4 件：约束知情同意 / 约束记录 / 约束解除记录 / 评估报告。
    let present: Vec<&GraphNode> = g.nodes.values().filter(|n| !n.is_missing()).collect();
    assert_eq!(present.len(), 4, "4 件真实件应各入节点表");

    // 院级依赖规范三条：
    //   约束记录 → 约束知情同意（requires）
    //   约束解除记录 → 约束记录（requires）
    //   评估报告 → 住院记录（requires: false，且住院记录不在包里）
    assert!(
        g.blocking_edges().is_empty(),
        "成套包里两条 requires 边的前置件都在，一条阻塞边都不该有；实得：{:?}",
        g.blocking_edges()
    );
    assert_eq!(g.edges.len(), 2, "两条 requires 边：记录→同意、解除→记录");

    assert!(g.soft_gaps.len() == 1, "评估报告的住院记录参考缺件应记 soft_gap");
    let gap = &g.soft_gaps[0];
    assert_eq!(gap.spec_to, "inpatient_record");
    assert!(!gap.requires, "requires: false 的参考关系不该阻断");
    assert!(!gap.blocking);

    assert!(g.manual_queue.is_empty(), "成套包不该有人工队列项：{:?}", g.manual_queue);
}

#[test]
fn dependencies_and_dependents_differ_and_are_reproducible() {
    // 这是源产品那条「一条命令拿整张图、靠方向开关问正反两个问题」的落点。
    let g = graph_of(RESTRAINT_SET);
    let record = "WARD-B-001-RECORD-01";
    let consent = "WARD-B-001-CONSENT-01";

    let (deps, dependents) = g.both_directions(record);
    assert_eq!(
        ids(&deps),
        vec![consent],
        "约束记录在等的应只有约束知情同意"
    );
    assert_eq!(
        ids(&dependents),
        vec!["WARD-B-001-RELEASE-01"],
        "约束记录卡住的是约束解除记录（解除记录要求先有记录）"
    );
    assert_ne!(deps, dependents, "正反向答案必须不同，否则方向开关没意义");

    // 反过来问前置件：谁在等它。
    let (c_deps, c_dependents) = g.both_directions(consent);
    assert!(c_deps.is_empty(), "知情同意不需要别的件");
    assert_eq!(ids(&c_dependents), vec![record], "等它的正是约束记录");

    // 可复现：同一张图、同一个节点，问两次答案完全一致。
    for _ in 0..3 {
        let again = g.query(record, Direction::Dependencies);
        assert_eq!(again, deps, "同一张图重复查询必须给出同一答案");
        assert_eq!(g.query(record, Direction::Dependents), dependents);
    }

    // `ImportMap` 正反两个构造来自**同一批边**（源侧 `ImportMap::dependencies` /
    // `dependents` 的形状），边数守恒。
    let forward = g.dependencies_map().expect("无环图应能出正向表");
    let reverse = g.dependents_map().expect("无环图应能出反向表");
    assert_eq!(forward.edge_count(), reverse.edge_count());
    assert_eq!(forward.get(record).unwrap(), &deps);
    assert_eq!(reverse.get(consent).unwrap(), &c_dependents);
    // 同一批边的两个投影：两表并起来的键集 == 边里出现过的全部端点。
    let endpoints: std::collections::BTreeSet<String> = g
        .edges
        .iter()
        .flat_map(|e| [e.from.clone(), e.to.clone()])
        .collect();
    let union: std::collections::BTreeSet<String> = forward
        .nodes()
        .chain(reverse.nodes())
        .cloned()
        .collect();
    assert_eq!(union, endpoints, "正反两表并起来应恰好覆盖边上的全部端点");
}

#[test]
fn a_missing_item_makes_exactly_one_blocking_edge() {
    // 拿掉约束知情同意这一件，其余三件原样。
    let full = graph_of(RESTRAINT_SET);
    assert_eq!(full.blocking_edges().len(), 0, "成套包本不该有阻塞边");

    // 手工构造「缺知情同意」的包：只留约束记录（依赖方）与另外两件。
    let without_consent: String = {
        let v: serde_json::Value = serde_json::from_str(RESTRAINT_SET).unwrap();
        let entries: Vec<_> = v["entry"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| {
                !e["resource"]["type"]["coding"][0]["code"]
                    .as_str()
                    .unwrap_or("")
                    .contains("restraint_consent")
            })
            .cloned()
            .collect();
        let mut v2 = v.clone();
        v2["entry"] = serde_json::Value::Array(entries);
        serde_json::to_string(&v2).unwrap()
    };

    let g = graph_of(&without_consent);
    g.check_invariants().expect("缺件图仍应满足不变量");

    let blocking = g.blocking_edges();
    assert_eq!(
        blocking.len(),
        1,
        "缺 1 件必须恰好产生 1 条阻塞边；实得：{:?}",
        blocking
    );
    let b = blocking[0];
    assert_eq!(b.spec_to, "restraint_consent");
    assert!(b.blocking && b.requires);
    assert!(
        g.nodes[&b.to].is_missing() && g.nodes[&b.to].kind == NodeKind::Missing,
        "阻塞边应指向一个缺件占位节点"
    );
    assert!(
        g.is_blocked(&b.from),
        "约束记录此刻应显示为被卡住"
    );

    // 补齐后：阻塞边消失，占位节点随之退场，实边连到真实件上。
    let restored = graph_of(RESTRAINT_SET);
    assert!(
        restored.blocking_edges().is_empty(),
        "补齐后阻塞边必须消失"
    );
    assert!(
        !restored.nodes.contains_key(&b.to),
        "补齐后不应再有缺件占位节点"
    );
    let deps = restored.query("WARD-B-001-RECORD-01", Direction::Dependencies);
    assert_eq!(ids(&deps), vec!["WARD-B-001-CONSENT-01"], "补齐后连到真实件");
    assert!(!restored.is_blocked("WARD-B-001-RECORD-01"));
}

#[test]
fn ambiguous_items_never_produce_edges() {
    // ward-03：HIS 与护理系统各一份「约束记录」。绑定层判 ambiguous，
    // 依赖图**不得**把任一份接出去。
    let rs = rules();
    let reg = rs.registry();
    let pkg = parse_package_with(CROSS_SYSTEM_DUP, Some(&reg)).unwrap();
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    assert!(
        bound.confident.is_empty(),
        "跨系统同名件不该有 confident 的：{:?}",
        bound.confident.iter().map(|i| &i.doc_id).collect::<Vec<_>>()
    );

    // 人工队列由绑定层（`EntityBinder`）产出——它认出跨系统同名，
    // 依赖图拿到的就只有 confident 的那批。
    assert_eq!(bound.manual_queue.len(), 2, "两份件都进人工队列");
    for m in &bound.manual_queue {
        assert!(
            m.reason.contains("人工确认"),
            "队列理由要写清为什么没人敢自动处理：{}",
            m.reason
        );
    }

    let g = DocGraph::build(&bound.confident, &rs);
    assert!(
        g.edges.is_empty() && g.soft_gaps.is_empty() && g.nodes.is_empty(),
        "没有 confident 件就一条边、一个节点都不该有；实得：{:?}",
        g
    );
}

#[test]
fn ambiguous_items_reaching_the_resolver_are_queued_and_never_linked() {
    // 依赖图自身还有一道防线：即使上层忘了过滤，`Resolver` 见到
    // `Binding::Ambiguous` 的件也**不连边**，只记人工队列。
    let rs = rules();
    let reg = rs.registry();
    let pkg = parse_package_with(CROSS_SYSTEM_DUP, Some(&reg)).unwrap();
    let items = pkg
        .items
        .into_iter()
        .map(|mut d| {
            // 复刻绑定层给出的结论：两份同名件都判 ambiguous。
            d.binding = psychdoc_model::Binding::Ambiguous;
            d
        })
        .collect::<Vec<_>>();

    let g = DocGraph::build(&items, &rs);
    assert!(g.edges.is_empty(), "ambiguous 件不得连出任何边");
    assert!(g.nodes.is_empty(), "ambiguous 件不得进入节点表");
    assert_eq!(g.manual_queue.len(), 2, "两份件都进依赖图的人工队列");
    for m in &g.manual_queue {
        assert!(
            m.reason.contains("人工确认"),
            "队列理由要写清为什么没人敢自动处理：{}",
            m.reason
        );
    }
}

#[test]
fn a_self_loop_dependency_is_refused() {
    // 院内把「约束记录」写成依赖自己（抄规则集时手滑）。
    let mut yaml = RULESET.to_string();
    yaml.push_str(
        "\ndepartment:\n  dependencies:\n    - from: restraint_record\n      to: restraint_record\n",
    );
    let rs = load_effective(&yaml).expect("自环依赖在加载期是合法的（字典内），要到出图期才拦");
    let g = graph_with_rules(RESTRAINT_SET, &rs);

    assert_eq!(g.self_loops.len(), 1, "约束记录应被认出自环；实得：{:?}", g.self_loops);
    let sl = &g.self_loops[0];
    assert_eq!(sl.template_id, "restraint_record");
    assert_eq!(sl.doc_id, "WARD-B-001-RECORD-01");

    // 自环不产生边：否则「这份在等自己」会让归档核齐永远核不齐。
    for e in g.edges.iter().chain(g.soft_gaps.iter()) {
        assert_ne!(e.from, e.to, "自环边不该被连出来：{e:?}");
    }
    g.check_invariants().expect("含自环记录但无自环边的图仍应满足不变量");
}

#[test]
fn a_dangling_dependency_spec_is_recorded_not_dropped() {
    // 依赖规范引用了一个院内类型字典里没有的类型码。
    let mut yaml = RULESET.to_string();
    yaml.push_str(
        "\ndepartment:\n  dependencies:\n    - from: sign_page\n      to: never_used_form\n",
    );
    // 加载期已经拒绝过一次（E_RULESET_INVALID）；这里直接喂给 Collector，
    // 验证**第二道防线**也会把悬空规范记下来。
    let dep: psychdoc_rules::DependencyDecl = serde_yaml::from_str(
        "from: sign_page\nto: never_used_form\nrequires: true\n",
    )
    .unwrap();
    let doc_types = rules().doc_types;
    let collected = Collector::new().collect_deps(&[], std::slice::from_ref(&dep), &doc_types);

    assert_eq!(collected.dangling_specs.len(), 1, "悬空规范必须记账");
    assert_eq!(collected.dangling_specs[0].to, "never_used_form");
    assert!(
        collected.dangling_specs[0].reason.contains("类型字典"),
        "理由要写清是字典里没有：{}",
        collected.dangling_specs[0].reason
    );
    assert!(collected.edges.is_empty(), "悬空规范不得连出边");
    assert!(yaml.contains("never_used_form"), "本用例另验证加载期亦拒绝该写法");
    assert!(
        load_effective(&yaml).is_err(),
        "加载期应先一步拒绝悬空依赖规范"
    );
}

#[test]
fn duplicate_spec_declarations_collapse_to_one_edge() {
    // 院级与科室级各声明同一条边（很常见：科室照抄院级又改了 requires）。
    let yaml = format!(
        "{RULESET}\ndepartment:\n  dependencies:\n    - from: restraint_release\n      to: restraint_record\n      requires: true\n"
    );
    let rs = load_effective(&yaml).expect("跨层同一条边只是重复，不是配置错误");

    let reg = rs.registry();
    let pkg = parse_package_with(RESTRAINT_SET, Some(&reg)).unwrap();
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    let collected = Collector::new().collect(&bound.confident, &rs);

    // 4 件里只有约束解除记录声明了 3 条依赖，两条里有一条重复 → 2 条边。
    let release_edges: Vec<_> = collected
        .edges
        .iter()
        .filter(|e| e.spec_to == "restraint_record")
        .collect();
    assert_eq!(
        release_edges.len(),
        1,
        "同一条边声明两遍应收敛成一条；实得：{release_edges:?}"
    );

    let g = DocGraph::build(&bound.confident, &rs);
    let blocks = g.blocks("WARD-B-001-RECORD-01");
    assert_eq!(blocks.len(), 1, "反向表里同一前置件只该有一条反向边");
}

#[test]
fn edges_never_cross_encounter_aliases() {
    let rs = rules();
    let reg = rs.registry();

    // 两份不同住院的成套文书拼在同一个包里（一次导出了两个病区）。
    let a: serde_json::Value = serde_json::from_str(RESTRAINT_SET).unwrap();
    let b = retag(a.clone(), "WARD-B-001", "WARD-B-002");
    let mut merged = a.clone();
    let mut entries = a["entry"].as_array().unwrap().clone();
    entries.extend(b["entry"].as_array().unwrap().clone());
    merged["entry"] = serde_json::Value::Array(entries);
    let json = serde_json::to_string(&merged).unwrap();

    let pkg = parse_package_with(&json, Some(&reg)).unwrap();
    let bound = EntityBinder::new(Some(&reg)).bind(pkg.items);
    let g = DocGraph::build(&bound.confident, &rs);
    g.check_invariants()
        .expect("跨住院连边必须被 check_invariants 抓住");

    assert_eq!(
        g.nodes.values().filter(|n| !n.is_missing()).count(),
        8,
        "两次住院各 4 件真实件"
    );
    assert_eq!(
        g.nodes.values().filter(|n| n.is_missing()).count(),
        2,
        "两次住院各缺一份住院记录（requires:false 参考）→ 各一个占位节点"
    );
    for e in g.edges.iter().chain(g.soft_gaps.iter()) {
        let fe = &g.nodes[&e.from].encounter_alias;
        let te = &g.nodes[&e.to].encounter_alias;
        assert_eq!(fe, te, "边跨了住院别名：{e:?}");
    }

    // 手工塞一条跨住院的边（模拟出图期逻辑写错），自检必须报错。
    let mut broken = g.clone();
    broken.edges.push(psychdoc_graph::GraphEdge {
        from: "WARD-B-001-RECORD-01".into(),
        to: "WARD-B-002-RECORD-01".into(),
        requires: true,
        blocking: false,
        spec_from: "restraint_record".into(),
        spec_to: "restraint_record".into(),
    });
    assert!(
        matches!(
            broken.check_invariants(),
            Err(InvariantError::CrossEncounter { .. })
        ),
        "跨住院边必须被 E_DEP_CROSS_ENCOUNTER 拦下"
    );
}

#[test]
fn a_dangling_edge_endpoint_is_rejected() {
    let g = graph_of(RESTRAINT_SET);
    let mut broken = g.clone();
    broken.edges.push(psychdoc_graph::GraphEdge {
        from: "WARD-B-001-RECORD-01".into(),
        to: "<WARD-B-001>::missing::never_collected".into(),
        requires: true,
        blocking: true,
        spec_from: "restraint_record".into(),
        spec_to: "never_collected".into(),
    });
    match broken.check_invariants() {
        Err(InvariantError::Dangling { missing, .. }) => {
            assert_eq!(missing, "<WARD-B-001>::missing::never_collected")
        }
        other => panic!("悬空边应报 E_DEP_DANGLING，实得：{other:?}"),
    }
}

#[test]
fn an_empty_package_yields_an_empty_but_valid_graph() {
    // 单件包（ward-01 只有一份知情同意）：没有任何依赖方 → 空图，但不是错误。
    let g = graph_of(include_str!("../../../../fixtures/ward-01-minimal.json"));
    g.check_invariants().expect("空图也满足不变量");
    assert!(g.edges.is_empty() && g.soft_gaps.is_empty());
    assert_eq!(g.nodes.len(), 1);
}

// ---------------------------------------------------------------------------

/// 把一份包里的住院别名整体换掉（用于造「两次住院」的边界夹具）。
fn retag(v: serde_json::Value, from: &str, to: &str) -> serde_json::Value {
    let mut v = v;
    if let Some(entries) = v["entry"].as_array_mut() {
        for e in entries.iter_mut() {
            let r = &mut e["resource"];
            if let Some(id) = r["id"].as_str() {
                let new = id.replace(from, to);
                r["id"] = serde_json::Value::String(new);
            }
            if let Some(ids) = r["identifier"].as_array_mut() {
                for i in ids.iter_mut() {
                    if i["value"].as_str() == Some(from) {
                        i["value"] = serde_json::Value::String(to.to_string());
                    }
                }
            }
        }
    }
    v
}
