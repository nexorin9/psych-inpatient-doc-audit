//! 对**实际交付的** `rulesets/hospital.yaml` 做的回归。
//!
//! 单元测试用的是 `#[cfg(test)]` 里的内联夹具；那份夹具和真实规则集迟早会漂移。
//! 本文件直接读仓库里那份 YAML，守住三条：
//!   1. 它能通过全部加载期强校验（否则 `psychdoc check` 一启动就退 2）；
//!   2. 三级级联真的生效（QC-PSY-01 的最终来源是科室级，且盖掉了院级）；
//!   3. 改一字节 → sha256 变（审计回溯的前提）。

use psychdoc_rules::{load_effective, released_rule_count, LayerId, Severity};

const HOSPITAL_YAML: &str = include_str!("../../../../rulesets/hospital.yaml");

#[test]
fn shipped_ruleset_passes_all_load_time_validation() {
    let rs = load_effective(HOSPITAL_YAML).expect("交付规则集必须能加载");
    assert_eq!(rs.version, "0.6.0", "规则集版本随根因码族表（Task 23）一并升版");
    assert!(rs.sha256.len() == 64, "必须冻结 64 位 sha256");
    assert_eq!(rs.short_sha256(), &rs.sha256[..12]);
}

#[test]
fn shipped_ruleset_leaves_department_and_personal_layers_empty() {
    // 交付规则集**不预置**任何科室 / 个人级放宽口径。
    // 「哪些文书可以先提示后补」是质控办与护理部的业务决定，工具的默认配置
    // 不该替他们做。因此 QC-PSY-01 的生效来源必须仍是院级。
    let rs = load_effective(HOSPITAL_YAML).unwrap();
    let r = rs.rule("QC-PSY-01").expect("QC-PSY-01 应生效");
    assert_eq!(r.severity, Severity::Blocking, "约束类缺项必须始终是阻断");
    assert_eq!(r.source, LayerId::Hospital);
    assert!(r.superseded.is_empty());
    assert_eq!(r.provenance(), "QC-PSY-01（院级）");

    assert_eq!(
        rs.merge_order,
        vec![LayerId::Hospital, LayerId::Department, LayerId::Personal],
        "三层仍须参与合并（空层不改变顺序，只是不产出规则）"
    );
    assert_eq!(rs.dependencies.len(), 3, "三条文书依赖规范应保留");
    assert!(rs.rule_basis_len_at_least_one());
}

#[test]
fn a_department_override_actually_takes_effect_when_enabled() {
    // 交付文件里那段科室级覆盖是注释掉的；这里只把**那一段**取消注释，
    // 证明「取消注释即可生效」这句话是真的，而不是只在注释里成立。
    let mut out: Vec<&str> = Vec::new();
    let mut in_block = false;
    for line in HOSPITAL_YAML.lines() {
        if line.starts_with("# department:") {
            in_block = true;
        } else if in_block && !line.starts_with('#') {
            in_block = false; // 注释块结束（后面是 personal: / per-ward-ignores:）
        }
        out.push(if in_block { line.trim_start_matches("# ") } else { line });
    }
    let enabled = out.join("\n");
    assert!(
        enabled.contains("\ndepartment:\n"),
        "科室级注释块必须真的被启用，形状与本用例假设不一致"
    );

    let rs = load_effective(&enabled).unwrap();
    let r = rs.rule("QC-PSY-01").unwrap();
    assert_eq!(r.severity, Severity::Notice, "科室级启用后应降为提示");
    assert_eq!(r.source, LayerId::Department);
    assert_eq!(r.superseded, vec![LayerId::Hospital]);
    assert_eq!(r.provenance(), "QC-PSY-01（科室级，覆盖 院级）");
    assert!(r.basis.contains("带教期口径"), "依据应来自科室级那条声明");
    // 科室完整重写了 applies_to → 作用范围从院级的五类收窄到两类。
    assert_eq!(r.applies_to, vec!["inpatient_record", "nursing_record"]);
    assert!(!r.applies_to_doc_type("restraint_consent"), "约束知情同意应已移出该码的作用范围");
}

#[test]
fn shipped_ruleset_has_no_waivers_so_nothing_is_silently_dropped() {
    let rs = load_effective(HOSPITAL_YAML).unwrap();
    assert!(rs.ward_ignores.is_empty(), "交付规则集当前不启用病区豁免");
    assert!(!rs.is_waived("WARD-A", "QC-PSY-01"));
}

#[test]
fn the_shipped_code_family_is_ordered_from_generic_to_specific() {
    // 交付的码族表：四码由泛到具体，**下标即具体度**。
    // 顺序写反不会导致加载失败，只会静默让去重挑错赢家——所以在这里钉住。
    let rs = load_effective(HOSPITAL_YAML).unwrap();
    let fam = rs
        .family_of("QC-PSY-04")
        .expect("交付规则集应声明 field-value 码族");
    assert_eq!(fam.family, "field-value");
    assert_eq!(
        fam.codes,
        vec!["QC-PSY-01", "QC-PSY-02", "QC-PSY-03", "QC-PSY-04"],
        "族内必须由泛到具体：单位错配（04）最具体，应排在最后"
    );
    assert_eq!(fam.specificity("QC-PSY-01"), Some(0));
    assert_eq!(fam.specificity("QC-PSY-04"), Some(3));
    assert_eq!(fam.specificity("QC-PSY-99"), None, "未列出的码不在本族");
    assert!(!fam.reason.is_empty(), "族必须带院内理由，否则「为什么算同一件事」答不出");

    // 归族是质控办的业务决定，交付配置不该把跨文书的时间序码也塞进件内字段族：
    // 那两类的整改动作完全不同（改时间 vs 改写法），合成一族会压掉真问题。
    for cross in ["QC-PSY-05", "QC-PSY-06", "QC-PSY-07", "QC-PSY-08"] {
        assert!(rs.family_of(cross).is_none(), "{cross} 不应并进件内字段族");
    }
}

#[test]
fn one_byte_change_flips_the_version_fingerprint() {
    let before = load_effective(HOSPITAL_YAML).unwrap().sha256;
    let after = load_effective(&HOSPITAL_YAML.replace("第 4.2 条", "第 4.3 条")).unwrap().sha256;
    assert_ne!(before, after, "改一个字节后哈希必须改变，否则审计回溯形同虚设");
    // 两次加载同一份文件 → 哈希一致（可复现，不随时间变化）。
    assert_eq!(before, load_effective(HOSPITAL_YAML).unwrap().sha256);
}

#[test]
fn restraint_documents_always_run_under_the_hospital_severity() {
    // 红线守卫：约束 / 解除 / 评估 / 签署这几类文书的必填缺项必须始终是**阻断**，
    // 且都在本规则的作用范围内。哪天真要让科室放宽，也只能放宽住院记录 / 护理记录。
    let rs = load_effective(HOSPITAL_YAML).unwrap();
    let r = rs.rule("QC-PSY-01").unwrap();
    for tpl in [
        "restraint_consent",
        "restraint_record",
        "restraint_release",
        "assessment_report",
        "sign_page",
    ] {
        assert!(r.applies_to_doc_type(tpl), "{tpl} 应在本规则作用范围内");
    }
    assert_eq!(r.severity, Severity::Blocking);
    assert!(!r.applies_to_doc_type("nursing_record"), "护理记录当前不在本规则作用范围内");
}

#[test]
fn a_broken_shipped_ruleset_would_be_rejected_not_silently_passed() {
    // 守卫：万一有人把校验删了，这里会红。改一条 basis 为空 → 必须拒绝加载。
    let broken = HOSPITAL_YAML.replace(
        "《精神科住院护理常规》第 4.2 条：约束类文书须逐项填写并由患方签字确认",
        "",
    );
    assert_eq!(released_rule_count(&broken), 0, "校验失效时这条会红");
    assert!(load_effective(&broken).is_err());
}

/// 小扩展：让上面的断言读起来像业务断言而不是容器断言。
trait RuleBasisLen {
    fn rule_basis_len_at_least_one(&self) -> bool;
}

impl RuleBasisLen for psychdoc_rules::EffectiveRules {
    fn rule_basis_len_at_least_one(&self) -> bool {
        self.rule("QC-PSY-01").is_some_and(|r| !r.basis.is_empty())
    }
}
