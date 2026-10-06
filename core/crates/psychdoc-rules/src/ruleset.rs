//! 规则集加载、三级级联覆盖与 sha256 版本冻结。
//!
//! ## 源产品主能力与本轮处置
//!
//! 级联配置（`pyproject.toml` / `ruff.toml` 逐层覆盖）与 `per-file-ignores`
//! 病区级豁免，其机制证据**仅来自源产品 README**（`spec.md` §6 明确标注
//! 「仅 README，不声称复用源码」）。本模块因此**借鉴设计**而非搬运代码：
//! 保留「后层覆盖前层」「每条生效规则能说清自己来自哪一层配置」这两个可观察行为，
//! 把层名从 `pyproject → user → 目录` 换成院内的
//! **院（hospital） → 科室（department） → 个人（personal）**。
//!
//! ## 三条不可让步的行为
//!
//! 1. **加载失败即拒绝启动该批**：任一校验不过 → 整批 `Err`，`EffectiveRules`
//!    根本不会被构造出来。「无规则 = 全部通过」这条退路在结构上不存在。
//! 2. **生效规则带溯源标记**：`EffectiveRule.source` 指出这一条最终由哪一层决定，
//!    `superseded` 保留被它盖掉的层，质控办能回答「这条为什么是这样」。
//! 3. **豁免必须写理由**：`per-ward-ignores` 的 `reason` 为空即拒绝加载，
//!    杜绝「某条为什么不报」变成查不到出处的问题。

use psychdoc_model::{DocTypeRegistry, Severity, TemplateSpec};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

/// sha256 短哈希长度（与 UI 底栏、审计导出共用同一口径）。
pub const SHA256_SHORT_LEN: usize = 12;

/// 规则集的三个覆盖层。`Ord` 决定同级冲突时的稳定顺序（院 < 科室 < 个人）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LayerId {
    Hospital,
    Department,
    Personal,
}

impl LayerId {
    pub const ALL: [LayerId; 3] = [LayerId::Hospital, LayerId::Department, LayerId::Personal];

    pub fn as_str(self) -> &'static str {
        match self {
            LayerId::Hospital => "hospital",
            LayerId::Department => "department",
            LayerId::Personal => "personal",
        }
    }

    /// 院内口径的层名（写进审计与「这条命中来自哪一层」的溯源展示）。
    pub fn label(self) -> &'static str {
        match self {
            LayerId::Hospital => "院级",
            LayerId::Department => "科室级",
            LayerId::Personal => "个人级",
        }
    }

    /// 未显式声明 `extends` 时的默认父层。医院是根，个人继承科室。
    fn default_parent(self) -> Option<LayerId> {
        match self {
            LayerId::Hospital => None,
            LayerId::Department => Some(LayerId::Hospital),
            LayerId::Personal => Some(LayerId::Department),
        }
    }
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一层配置。三个层用同一个形状，靠字段填充与否区分「这一层管不管」。
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct RuleLayer {
    /// 显式声明覆盖谁；键不写则取默认父层（科室→院，个人→科室）。
    /// 写成一个**已存在的层**才合法，写成环（科室↔个人）会在加载期被拒。
    ///
    /// 「某个病区不按院级口径办」不在这层解决——那是 `per-ward-ignores` 的职责，
    /// 且必须带理由；级联层只表达「谁的规则说了算」。
    #[serde(default)]
    pub extends: Option<LayerId>,
    /// 院内文书类型字典的该层增量。
    #[serde(default)]
    pub doc_types: BTreeMap<String, String>,
    /// 文书依赖规范（归档核齐视图的边来源）。
    #[serde(default)]
    pub dependencies: Vec<DependencyDecl>,
    /// 该层声明的规则。同层内重复声明同一码 → `E_RULESET_INVALID`。
    #[serde(default)]
    pub rules: Vec<RuleDecl>,
}

/// 文书依赖规范的一条边。`requires` 缺省为「需要」，与院内「前置件」口径一致。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct DependencyDecl {
    pub from: String,
    pub to: String,
    #[serde(default = "default_true")]
    pub requires: bool,
}

fn default_true() -> bool {
    true
}

/// 一条规则声明。跨层同码即覆盖（科室盖院、个人盖科室），同层同码是配置错误。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct RuleDecl {
    pub code: String,
    #[serde(default)]
    pub severity: Option<Severity>,
    #[serde(default)]
    pub title: Option<String>,
    /// 院内规范条款依据。留空时回落到顶层 `rule_basis[code]`；两处都没有 → 拒绝加载。
    #[serde(default)]
    pub basis: Option<String>,
    /// 适用的文书类型（`templates` 的键）。空 = 全部文书类型。
    /// 引用未定义文书类型 → `E_RULESET_INVALID`。
    #[serde(default)]
    pub applies_to: Vec<String>,
}

/// 病区级豁免。**必须写理由**——「这条为什么不报」永远是院内最常见的追问。
///
/// 谓词化（Task 18 起）：四类条件 —— 病区 / 文书类型 / 规则码 / 时间窗 —— 写了的
/// 几类按 [`PredicateCombine`] 组合；未写的维度不受约束。`ward` / `codes` 是
/// `wards` / `codes` 的**书写简写**，两者等价，保证既有 YAML 不用改。
///
/// `source` 记录这条豁免由哪一层声明（院 → 科室 → 个人），随豁免结果落盘，
/// 供质控办回答「这条豁免是谁批的」。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct WardIgnore {
    /// 单病区简写。`wards` 非空时以 `wards` 为准。
    #[serde(default)]
    pub ward: String,
    /// 病区码列表（可多个病区共用一条豁免）。
    #[serde(default)]
    pub wards: Vec<String>,
    #[serde(default)]
    pub codes: Vec<String>,
    /// 文书类型（`templates` 的键）。空 = 不限文书类型。
    #[serde(default)]
    pub doc_types: Vec<String>,
    /// 生效时间窗。`None` = 不限时间。
    #[serde(default)]
    pub time_window: Option<crate::waiver::TimeWindow>,
    /// 条件组合方式。
    #[serde(default)]
    pub combine: crate::waiver::PredicateCombine,
    /// 两级语义：整条不适用 / 降级为提示。
    #[serde(default)]
    pub effect: crate::waiver::WaiverEffect,
    /// 声明层级。
    #[serde(default = "default_waiver_source")]
    pub source: LayerId,
    pub reason: String,
}

fn default_waiver_source() -> LayerId {
    LayerId::Hospital
}

impl WardIgnore {
    /// 归一化后的病区列表（合并 `ward` 简写与 `wards`）。
    pub fn ward_list(&self) -> Vec<String> {
        let mut v: Vec<String> = Vec::new();
        if !self.ward.trim().is_empty() {
            v.push(self.ward.trim().to_string());
        }
        for w in &self.wards {
            let w = w.trim();
            if !w.is_empty() && !v.iter().any(|x| x == w) {
                v.push(w.to_string());
            }
        }
        v
    }

    /// 归一化后的谓词：`ward` 简写折进 `wards`。
    pub fn predicate(&self) -> crate::waiver::WaiverPredicate {
        crate::waiver::WaiverPredicate {
            wards: self.ward_list(),
            doc_types: self.doc_types.clone(),
            codes: self.codes.clone(),
            time_window: self.time_window,
            combine: self.combine,
        }
    }
}

/// 跨文书时间序的并列容差声明。
///
/// 容差是**一个业务决定**，不是一条技术参数：「同班次内先填解除、后补约束起始」
/// 在纸面上就是正常的补记顺序，差 3 分钟不该记一条时序倒挂。因此：
///
/// - 按**件对**（无序）配置，理由必填——「这条为什么没报」要能回答；
/// - 档位固定 0 / 1 / 5 分钟：容差一旦能随手写 30 分钟，跨文书时序判定
///   等于关掉了，而这个决定会藏在规则集里没人发现。
#[derive(Debug, Clone, Default, PartialEq, Deserialize, Serialize)]
pub struct ChronologyDecl {
    /// 未命中任何件对时的兜底容差。交付规则集固定 0（从严）。
    #[serde(default)]
    pub default_minutes: i64,
    #[serde(default, rename = "tie-tolerance")]
    pub tie_tolerance: Vec<TieToleranceDecl>,
}

/// 一条件对容差声明。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct TieToleranceDecl {
    /// 件对，两个文书类型码，**顺序无关**。
    pub pair: Vec<String>,
    pub minutes: i64,
    pub reason: String,
    /// 声明层级（院 → 科室 → 个人）。
    #[serde(default = "default_waiver_source")]
    pub source: LayerId,
}

impl TieToleranceDecl {
    pub fn to_tolerance(&self) -> crate::chronology::Tolerance {
        crate::chronology::Tolerance {
            minutes: self.minutes,
            reason: self.reason.clone(),
            source: self.source,
        }
    }
}

/// **根因码族**声明（Task 23）。
///
/// 真实规则集互相重叠：同一个字段既能被判「必填缺失」，也能被判「类型不符」，
/// 而两者的整改动作其实只有一条——把这一栏填对。同一处刷两行会让护士长
/// 以为要改两件事，规则集越长刷得越凶。
///
/// 码族把「**说同一件事**」的规则归到一族，并按**由泛到具体**排好序：
/// 同一处同时命中时只保留最靠后（最具体）的那条，其余记 `suppressed_by`
/// 折叠起来，随时可展开——不丢弃。
///
/// 两条纪律：
///
/// - 映射表写在**规则集里**而不是代码里。哪两条是同一根因，是质控办的业务决定；
///   写死在代码里，下一个维护者只能靠猜。
/// - 一个码只能属于一个族。同一码出现在两族（或一族内出现两次）一律
///   `E_RULESET_INVALID`——那说明规则集本身写错了，**不允许**静默按一条处理。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
pub struct CodeFamilyDecl {
    /// 族标识（kebab-case 短码，进去重键，保证结果可复现）。
    pub family: String,
    /// 院内中文名，进「此处另有 N 条更概括的命中」一句。
    pub label: String,
    /// 成员规则码，**由泛到具体**：越靠后越具体，择优时越优先保留。
    pub codes: Vec<String>,
    /// 为什么要归成一族。空即拒绝加载——「为什么这两条算同一件事」要能回答。
    pub reason: String,
}

/// 规则集源文件（尚未合并）。
///
/// 合并结果与版本哈希在 [`EffectiveRules`]；`RuleSet` 只负责「把 YAML 读成结构」
/// 与「算出版本指纹」，不做覆盖决策。
#[derive(Debug, Clone, Deserialize)]
pub struct RuleSet {
    pub version: String,
    #[serde(default)]
    pub templates: BTreeMap<String, TemplateSpec>,
    /// 规则码 → 规范条款依据。与各层 `rules[].basis` 同源，独立成表便于按码回溯。
    #[serde(default)]
    pub rule_basis: BTreeMap<String, String>,
    #[serde(default)]
    pub hospital: RuleLayer,
    #[serde(default)]
    pub department: RuleLayer,
    #[serde(default)]
    pub personal: RuleLayer,
    /// 病区级豁免。YAML 里用院内习惯的连字符写法 `per-ward-ignores`。
    #[serde(default, rename = "per-ward-ignores")]
    pub per_ward_ignores: Vec<WardIgnore>,
    /// 跨文书时间序的**并列容差声明**（Task 19）。缺省为空 = 一律 0 分钟从严判定。
    #[serde(default)]
    pub chronology: ChronologyDecl,
    /// 根因码族映射表（Task 23）。缺省为空 = 不做任何去重（每条各报各的）。
    #[serde(default, rename = "code-families")]
    pub code_families: Vec<CodeFamilyDecl>,
    /// 规则集**源文件**的 sha256。加载时算一次，随每次审查结果落盘，
    /// 供质控办按当时版本回溯复核（spec.md §5 审计链）。
    #[serde(default)]
    pub source_sha256: String,
}

impl RuleSet {
    pub fn layer(&self, id: LayerId) -> &RuleLayer {
        match id {
            LayerId::Hospital => &self.hospital,
            LayerId::Department => &self.department,
            LayerId::Personal => &self.personal,
        }
    }

    /// 解析 YAML 并冻结版本指纹。**不做覆盖与语义校验**——那属于 [`Self::resolve`]。
    pub fn load(yaml: &str) -> Result<Self, RuleSetError> {
        let mut rs: RuleSet =
            serde_yaml::from_str(yaml).map_err(|e| RuleSetError::Yaml { detail: e.to_string() })?;
        if rs.version.trim().is_empty() {
            return Err(RuleSetError::MissingVersion);
        }
        rs.source_sha256 = sha256_hex(yaml.as_bytes());
        Ok(rs)
    }

    /// 校验并合并出生效规则集。**任一校验失败即整批拒绝，不放行任何一条规则。**
    pub fn resolve(self) -> Result<EffectiveRules, RuleSetError> {
        let order = self.merge_order()?;

        // —— 结构校验：同层重复码。
        for &id in &order {
            let mut seen: BTreeSet<&str> = BTreeSet::new();
            for r in &self.layer(id).rules {
                if !seen.insert(r.code.as_str()) {
                    return Err(RuleSetError::DuplicateCode { code: r.code.clone(), layer: id });
                }
            }
        }

        // —— 结构校验：豁免必须写病区、理由与至少一个约束条件。
        for w in &self.per_ward_ignores {
            if w.ward_list().is_empty() {
                return Err(RuleSetError::MissingWaiverField {
                    ward: w.ward.clone(),
                    field: "ward（病区必填，可写 ward 单值或 wards 列表）",
                });
            }
            if w.reason.trim().is_empty() {
                return Err(RuleSetError::MissingWaiverField {
                    ward: w.ward.clone(),
                    field: "reason（豁免必须写理由，否则「这条为什么不报」查不到出处）",
                });
            }
            if w.codes.is_empty() && w.doc_types.is_empty() && w.time_window.is_none() {
                return Err(RuleSetError::MissingWaiverField {
                    ward: w.ward.clone(),
                    field: "codes / doc_types / time_window（三类约束至少写一类，否则谓词为空 = 什么都豁免）",
                });
            }
            if let Some(window) = &w.time_window {
                if window.from > window.to {
                    return Err(RuleSetError::InvalidWaiverWindow {
                        ward: w.ward.clone(),
                        detail: format!(
                            "生效时间窗起点 {} 晚于终点 {}",
                            window.from.to_rfc3339(),
                            window.to.to_rfc3339()
                        ),
                    });
                }
            }
        }

        // —— 按拓扑序合并：后者覆盖前者。
        let mut doc_types: BTreeMap<String, String> = BTreeMap::new();
        let mut dependencies: Vec<DependencyDecl> = Vec::new();
        let mut rules: BTreeMap<String, EffectiveRule> = BTreeMap::new();
        for &id in &order {
            let layer = self.layer(id);
            doc_types.extend(layer.doc_types.clone());
            dependencies.extend(layer.dependencies.clone());
            for decl in &layer.rules {
                let prev = rules.get(&decl.code);
                // 依据：先看本层写了没，没写就沿用上级；两处都没有才报错。
                let basis = decl
                    .basis
                    .clone()
                    .or_else(|| prev.map(|p| p.basis.clone()))
                    .or_else(|| self.rule_basis.get(&decl.code).cloned())
                    .unwrap_or_default();
                if basis.trim().is_empty() {
                    return Err(RuleSetError::MissingBasis { code: decl.code.clone() });
                }
                let superseded = prev
                    .map(|p| {
                        let mut v = p.superseded.clone();
                        v.push(p.source);
                        v
                    })
                    .unwrap_or_default();
                // 逐字段继承：下层只写它真正要改的那几项，其余沿用上级。
                //
                // `applies_to` 的继承规则要特别小心：没写 = **沿用上级的范围**，
                // 不是「作用于全部」。否则科室只想把住院记录降为提示，
                // 却顺手把约束知情同意的患方签字也一起降级——那是本系统最不该松的一类。
                // 真要扩到全院，显式写 `applies_to: ["*"]`；真要收窄，完整重写一遍列表。
                let applies_to = if !decl.applies_to.is_empty() {
                    decl.applies_to.clone()
                } else {
                    prev.map(|p| p.applies_to.clone()).unwrap_or_default()
                };
                rules.insert(
                    decl.code.clone(),
                    EffectiveRule {
                        code: decl.code.clone(),
                        severity: decl.severity.or(prev.map(|p| p.severity)).unwrap_or_default(),
                        title: decl
                            .title
                            .clone()
                            .or_else(|| prev.map(|p| p.title.clone()))
                            .unwrap_or_else(|| decl.code.clone()),
                        basis,
                        applies_to,
                        source: id,
                        superseded,
                    },
                );
            }
        }

        // —— 语义校验：规则引用的文书类型必须已定义（`*` 是通配，不在此列）。
        for r in rules.values() {
            for tpl in &r.applies_to {
                if tpl != "*" && !self.templates.contains_key(tpl) {
                    return Err(RuleSetError::UndefinedTemplate {
                        code: r.code.clone(),
                        template: tpl.clone(),
                    });
                }
            }
        }
        // —— 语义校验：依赖边两端的文书类型必须已定义。
        for d in &dependencies {
            for tpl in [&d.from, &d.to] {
                if !self.templates.contains_key(tpl) {
                    return Err(RuleSetError::UndefinedTemplate {
                        code: format!("依赖边 {d:?}"),
                        template: tpl.clone(),
                    });
                }
            }
        }
        // —— 语义校验：豁免引用的规则码 / 文书类型必须存在，否则是永远不会生效的死豁免。
        for w in &self.per_ward_ignores {
            for code in &w.codes {
                if !rules.contains_key(code) {
                    return Err(RuleSetError::UndefinedRuleCode {
                        ward: w.ward.clone(),
                        code: code.clone(),
                    });
                }
            }
            for tpl in &w.doc_types {
                if !self.templates.contains_key(tpl) {
                    return Err(RuleSetError::UndefinedTemplate {
                        code: format!("病区 {} 的豁免引用文书类型", w.ward_list().join("、")),
                        template: tpl.clone(),
                    });
                }
            }
        }

        // 容差表在校验阶段就要读 `self.templates`，因此**先**算出来，
        // 再做那几个整体 move（move 之后再借 `self` 就编译不过了）。
        let chronology = self.resolve_chronology()?;
        let code_families = self.resolve_code_families(&rules)?;
        Ok(EffectiveRules {
            version: self.version,
            sha256: self.source_sha256,
            templates: self.templates,
            doc_types,
            dependencies,
            rules,
            chronology,
            code_families,
            ward_ignores: self.per_ward_ignores,
            merge_order: order,
        })
    }

    /// 校验并归一化根因码族表（Task 23）。
    ///
    /// 五类校验，任一不过即整批拒绝：
    ///
    /// 1. 族标识重复；
    /// 2. 族内码重复，或**同一码落在两族**——这是规则集写错了，不是业务决定；
    /// 3. 引用了未定义的规则码（死族成员 = 永不参与去重的空配置）；
    /// 4. 成员不足 2 条（单条族不构成去重，只会让人以为配了）；
    /// 5. 缺院内理由（「为什么这两条算同一件事」要能回答）。
    fn resolve_code_families(
        &self,
        rules: &BTreeMap<String, EffectiveRule>,
    ) -> Result<Vec<CodeFamily>, RuleSetError> {
        let mut seen_family: BTreeSet<&str> = BTreeSet::new();
        let mut owner: BTreeMap<&str, &str> = BTreeMap::new();
        let mut out: Vec<CodeFamily> = Vec::new();

        for decl in &self.code_families {
            let fam = decl.family.trim();
            if fam.is_empty() {
                return Err(RuleSetError::InvalidCodeFamily {
                    family: decl.family.clone(),
                    detail: "族标识为空（用 kebab-case 短码，例如 field-value）".to_string(),
                });
            }
            if !seen_family.insert(fam) {
                return Err(RuleSetError::DuplicateCodeFamily {
                    family: decl.family.clone(),
                });
            }
            if decl.label.trim().is_empty() {
                return Err(RuleSetError::InvalidCodeFamily {
                    family: decl.family.clone(),
                    detail: "缺院内中文名（折叠时要向护士长说明「此处另有哪一族」）".to_string(),
                });
            }
            if decl.reason.trim().is_empty() {
                return Err(RuleSetError::InvalidCodeFamily {
                    family: decl.family.clone(),
                    detail: "缺院内理由（「为什么这两条算同一件事」要能回答）".to_string(),
                });
            }
            if decl.codes.len() < 2 {
                return Err(RuleSetError::InvalidCodeFamily {
                    family: decl.family.clone(),
                    detail: format!(
                        "成员只有 {} 条——单条族不构成去重，配了等于没配",
                        decl.codes.len()
                    ),
                });
            }
            let mut within: BTreeSet<&str> = BTreeSet::new();
            for code in &decl.codes {
                if !within.insert(code.as_str()) {
                    return Err(RuleSetError::DuplicateCodeInFamily {
                        family: decl.family.clone(),
                        code: code.clone(),
                    });
                }
                if let Some(other) = owner.insert(code.as_str(), fam) {
                    return Err(RuleSetError::CodeInTwoFamilies {
                        code: code.clone(),
                        first: other.to_string(),
                        second: decl.family.clone(),
                    });
                }
                if !rules.contains_key(code) {
                    return Err(RuleSetError::UndefinedRuleCodeInFamily {
                        family: decl.family.clone(),
                        code: code.clone(),
                    });
                }
            }
            out.push(CodeFamily {
                family: decl.family.trim().to_string(),
                label: decl.label.trim().to_string(),
                reason: decl.reason.trim().to_string(),
                // 下标即**具体度**：`codes` 由泛到具体，越靠后越优先保留。
                codes: decl.codes.to_vec(),
            });
        }
        // 族序固定按标识升序：同一份 YAML 两次加载的族序必须完全一致，
        // 否则去重结果会随声明顺序抖动，审计导出就没法 diff 了。
        out.sort_by(|a, b| a.family.cmp(&b.family));
        Ok(out)
    }

    /// 并列容差表：校验档位、件对、理由，再归一化成无序件对。
    ///
    /// 任一项不合法即整条拒绝——容差写错的后果是**漏报**，
    /// 而漏报在归档核齐时是查不出来的。
    fn resolve_chronology(&self) -> Result<ChronologyDecl, RuleSetError> {
        let table = self.build_tolerance_table()?;
        Ok(ChronologyDecl {
            default_minutes: table.default_minutes,
            tie_tolerance: self.chronology.tie_tolerance.clone(),
        })
    }

    /// 校验并归一化并列容差。件对**无序**：`a ↔ b` 与 `b ↔ a` 是同一件对，
    /// 因此重复声明（哪怕顺序不同）无法判断哪条说了算，一律拒绝。
    pub(crate) fn build_tolerance_table(&self) -> Result<crate::chronology::ToleranceTable, RuleSetError> {
        if !crate::chronology::ALLOWED_TOLERANCE_MINUTES.contains(&self.chronology.default_minutes) {
            return Err(RuleSetError::InvalidTieTolerance {
                pair: "（默认档）".to_string(),
                detail: format!(
                    "默认并列容差 {} 分钟不在院内档位 {:?} 内",
                    self.chronology.default_minutes,
                    crate::chronology::ALLOWED_TOLERANCE_MINUTES
                ),
            });
        }
        let mut table =
            crate::chronology::ToleranceTable::with_default(self.chronology.default_minutes);
        for decl in &self.chronology.tie_tolerance {
            let pair_label = decl.pair.join(" ↔ ");
            if decl.pair.len() != 2 || decl.pair[0] == decl.pair[1] {
                return Err(RuleSetError::InvalidTieTolerance {
                    pair: pair_label,
                    detail: "并列容差必须写**两个不同**的文书类型码".to_string(),
                });
            }
            for tpl in &decl.pair {
                if !self.templates.contains_key(tpl) {
                    return Err(RuleSetError::UndefinedTemplate {
                        code: format!("并列容差件对 {pair_label}"),
                        template: tpl.clone(),
                    });
                }
            }
            if !crate::chronology::ALLOWED_TOLERANCE_MINUTES.contains(&decl.minutes) {
                return Err(RuleSetError::InvalidTieTolerance {
                    pair: pair_label,
                    detail: format!(
                        "容差 {} 分钟不在院内档位 {:?} 内",
                        decl.minutes,
                        crate::chronology::ALLOWED_TOLERANCE_MINUTES
                    ),
                });
            }
            if decl.reason.trim().is_empty() {
                return Err(RuleSetError::InvalidTieTolerance {
                    pair: pair_label,
                    detail: "并列容差必须写院内理由（「这一条为什么没报」要能回答）".to_string(),
                });
            }
            if table.declared(&decl.pair[0], &decl.pair[1]).is_some() {
                return Err(RuleSetError::InvalidTieTolerance {
                    pair: pair_label,
                    detail: "该件对已声明过并列容差（件对无序，重复声明无法判断哪条说了算）".to_string(),
                });
            }
            table.insert(&decl.pair[0], &decl.pair[1], decl.to_tolerance());
        }
        Ok(table)
    }

    /// 求合并顺序：先校验 `extends` 图（未知层 / 环 / 多重父），再按深度排序。
    ///
    /// 环是**必须拒绝**的：院→科室→个人→院 这样的配置没有任何可解释的「哪层说了算」。
    fn merge_order(&self) -> Result<Vec<LayerId>, RuleSetError> {
        let parent_of = |id: LayerId| -> Option<LayerId> {
            self.layer(id).extends.or_else(|| id.default_parent())
        };

        // 1) extends 只能指向已存在的层。
        for &id in &LayerId::ALL {
            if let Some(p) = self.layer(id).extends {
                if !LayerId::ALL.contains(&p) {
                    return Err(RuleSetError::UnknownLayer { child: id, parent: p });
                }
            }
        }

        // 2) 同一层被两层显式 extends = 覆盖来源有歧义，优于报错。
        let mut children: BTreeMap<LayerId, Vec<LayerId>> = BTreeMap::new();
        for &id in &LayerId::ALL {
            if let Some(p) = self.layer(id).extends {
                children.entry(p).or_default().push(id);
            }
        }
        if let Some((parent, kids)) = children.iter().find(|(_, kids)| kids.len() > 1) {
            return Err(RuleSetError::MultiParent {
                parent: *parent,
                children: RuleSetError::render_layers(kids),
            });
        }

        // 3) 环检测：沿父链上溯，回到走过的层即成环（自环是长度为 1 的环）。
        //    深度 = 到根的步数；无环已证，故每条链必然终止于某个 `parent_of == None` 的层。
        let mut depth: BTreeMap<LayerId, usize> = BTreeMap::new();
        for &start in &LayerId::ALL {
            let mut path: Vec<LayerId> = Vec::new();
            let mut cur = Some(start);
            while let Some(id) = cur {
                if let Some(pos) = path.iter().position(|x| *x == id) {
                    let mut cycle = path[pos..].to_vec();
                    cycle.push(id);
                    return Err(RuleSetError::OverrideCycle {
                        cycle: RuleSetError::render_layers(&cycle),
                    });
                }
                path.push(id);
                cur = parent_of(id);
            }
            // path 是 start → … → 根，根深度 0，故 start 的深度 = path.len() - 1。
            depth.insert(start, path.len() - 1);
        }

        // 4) 深度浅的先合并；同深度按层枚举序，保证结果可复现。
        let mut order: Vec<LayerId> = LayerId::ALL.to_vec();
        order.sort_by_key(|id| (depth[id], LayerId::ALL.iter().position(|x| x == id).unwrap()));
        Ok(order)
    }
}

/// 合并后的生效规则 + 版本冻结。
// `TemplateSpec` 含 `f64`（数值区间上下界），因此本结构**不能**派生 `Eq`；
// 调用方需要「两份生效规则集是否完全一致」时用 `PartialEq` 即可。
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct EffectiveRules {
    pub version: String,
    /// 规则集源文件 sha256（64 位十六进制）。
    pub sha256: String,
    pub templates: BTreeMap<String, TemplateSpec>,
    pub doc_types: BTreeMap<String, String>,
    pub dependencies: Vec<DependencyDecl>,
    /// 规则码 → 生效声明（含溯源）。
    pub rules: BTreeMap<String, EffectiveRule>,
    pub ward_ignores: Vec<WardIgnore>,
    /// 跨文书时间序的并列容差（Task 19）。已通过加载期档位 / 件对 / 理由校验。
    pub chronology: ChronologyDecl,
    /// 根因码族（Task 23）。已通过加载期五类校验；空 = 不做去重。
    pub code_families: Vec<CodeFamily>,
    /// 本次实际生效的合并顺序（根层在前），写进运行摘要便于复核。
    pub merge_order: Vec<LayerId>,
}

/// 一个生效的根因码族：`codes` 的**下标即具体度**，越靠后越具体。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CodeFamily {
    pub family: String,
    pub label: String,
    pub reason: String,
    /// 由泛到具体。
    pub codes: Vec<String>,
}

impl CodeFamily {
    /// 该码在本族内的具体度；不在本族返回 `None`。
    pub fn specificity(&self, code: &str) -> Option<usize> {
        self.codes.iter().position(|c| c == code)
    }

    /// 折叠说明：`字段取值（field-value）：QC-PSY-01 被 QC-PSY-02 覆盖 —— <理由>`。
    pub fn explain_suppression(&self, loser: &str, winner: &str) -> String {
        format!(
            "{}（{}）：{loser} 已被更具体的 {winner} 覆盖 —— {}",
            self.label, self.family, self.reason
        )
    }
}

/// 一条生效规则：值是合并后的结果，`source` 说清它由哪一层说了算。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EffectiveRule {
    pub code: String,
    pub severity: Severity,
    pub title: String,
    /// 院内规范条款依据（构造期保证非空）。
    pub basis: String,
    /// 适用文书类型。
    ///
    /// - 空 = 该层首次声明、无人限定过 → 作用于全部文书类型；
    /// - 下层没写 = **沿用上级的范围**（不是清空，也不是扩到全部）；
    /// - 写 `["*"]` = 显式扩到所有文书类型。
    ///
    /// 收窄范围时必须**完整重写**列表：整体替换，不与上级并入。
    pub applies_to: Vec<String>,
    /// 最终决定这条的层。
    pub source: LayerId,
    /// 被本层盖掉的层（由远及近）。
    pub superseded: Vec<LayerId>,
}

impl EffectiveRule {
    /// 该规则是否适用于某种文书类型。
    pub fn applies_to_doc_type(&self, template_id: &str) -> bool {
        self.applies_to.is_empty() || self.applies_to.iter().any(|t| t == "*" || t == template_id)
    }

    /// 溯源串：`QC-PSY-02（个人级，覆盖 科室级、院级）`。
    pub fn provenance(&self) -> String {
        if self.superseded.is_empty() {
            format!("{}（{}）", self.code, self.source.label())
        } else {
            let names: Vec<&str> = self.superseded.iter().map(|l| l.label()).collect();
            format!("{}（{}，覆盖 {}）", self.code, self.source.label(), names.join("、"))
        }
    }
}

impl EffectiveRules {
    /// 12 位短哈希，供 UI 底栏与运行摘要显示。
    pub fn short_sha256(&self) -> &str {
        let end = SHA256_SHORT_LEN.min(self.sha256.len());
        &self.sha256[..end]
    }

    pub fn registry(&self) -> DocTypeRegistry {
        DocTypeRegistry { templates: self.templates.clone() }
    }

    pub fn rule(&self, code: &str) -> Option<&EffectiveRule> {
        self.rules.get(code)
    }

    /// 规则存在才取依据；不存在即该规则未生效，调用方应跳过而不是回落占位。
    pub fn basis_for(&self, code: &str) -> Option<&str> {
        self.rules.get(code).map(|r| r.basis.as_str())
    }

    pub fn severity_for(&self, code: &str) -> Option<Severity> {
        self.rules.get(code).map(|r| r.severity)
    }

    /// 该码所属的根因码族。**不在任何族里返回 `None`**——
    /// 去重层据此把每条各报各的（不族内不折叠），而不是猜一个族。
    pub fn family_of(&self, code: &str) -> Option<&CodeFamily> {
        self.code_families.iter().find(|f| f.codes.iter().any(|c| c == code))
    }

    /// 该码的族内具体度；未归族返回 `None`。
    pub fn specificity_of(&self, code: &str) -> Option<usize> {
        self.family_of(code).and_then(|f| f.specificity(code))
    }

    /// 两个文书类型之间的**生效并列容差**（Task 19）。
    ///
    /// 未声明即 0 分钟从严判定。件对**无序**：谁写在前面都一样。
    pub fn tie_tolerance(&self, a_tpl: &str, b_tpl: &str) -> crate::chronology::Tolerance {
        self.chronology
            .tie_tolerance
            .iter()
            .find(|d| {
                (d.pair[0] == a_tpl && d.pair[1] == b_tpl) || (d.pair[0] == b_tpl && d.pair[1] == a_tpl)
            })
            .map(|d| d.to_tolerance())
            .unwrap_or_else(|| crate::chronology::Tolerance {
                minutes: self.chronology.default_minutes,
                reason: "规则集未对该件对配置并列容差，按默认档从严判定".to_string(),
                source: LayerId::Hospital,
            })
    }

    /// 该件对是否**显式**声明过并列容差（用于「死容差」记账）。
    pub fn declares_tie_tolerance(&self, a_tpl: &str, b_tpl: &str) -> bool {
        self.chronology.tie_tolerance.iter().any(|d| {
            (d.pair[0] == a_tpl && d.pair[1] == b_tpl) || (d.pair[0] == b_tpl && d.pair[1] == a_tpl)
        })
    }

    /// 某病区是否整条豁免某规则（不带文书类型 / 时间窗约束的那种）。
    ///
    /// 保留这个二元接口是为了让「规则集里配了没配」可一眼核对；
    /// 真正逐条判定的走 [`crate::waiver::apply`]，它看得到文书类型与时刻。
    pub fn is_waived(&self, ward: &str, code: &str) -> bool {
        self.waived_reason(ward, code).is_some()
    }

    /// 最具体的一条豁免理由：病区匹配、码匹配，且**未**收窄文书类型 / 时间窗。
    /// 同条件下取最先声明的一条，保证结果可复现。
    pub fn waived_reason(&self, ward: &str, code: &str) -> Option<&str> {
        self.ward_ignores
            .iter()
            .filter(|w| {
                w.ward_list().iter().any(|x| x == ward)
                    && w.codes.iter().any(|c| c == code)
                    && w.doc_types.is_empty()
                    && w.time_window.is_none()
            })
            .map(|w| w.reason.as_str())
            .next()
    }
}

/// 加载 YAML 并直接得到生效规则集。
pub fn load_effective(yaml: &str) -> Result<EffectiveRules, RuleSetError> {
    RuleSet::load(yaml)?.resolve()
}

/// 加载失败时被放行的规则条数——**恒为 0**。
///
/// 「无规则 = 全部通过」是审查工具最危险的一种退化：批次看起来干干净净，
/// 实际上一条都没查。这条退路在结构上就不该存在，因此把它做成可断言的函数，
/// 让测试能直接守住「任一校验失败 → 零条规则被放行」。
pub fn released_rule_count(yaml: &str) -> usize {
    load_effective(yaml).map(|rs| rs.rules.len()).unwrap_or(0)
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hasher
        .finalize()
        .iter()
        .fold(String::with_capacity(64), |mut acc, b| {
            use fmt::Write as _;
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

/// 规则集加载 / 校验错误。**加载期错误一律带 `E_RULESET_INVALID`**，与运行期
/// 诊断的 `QC-PSY-nn` 分属两条线：前者是「这批根本没开审」，后者是「审了，有问题」。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RuleSetError {
    #[error("E_RULESET_INVALID: 规则集 YAML 解析失败 — {detail}")]
    Yaml { detail: String },
    #[error("E_RULESET_INVALID: 规则集缺少 version 字段")]
    MissingVersion,
    #[error("E_RULESET_INVALID: 规则 {code} 缺少院内规范条款依据，拒绝放行（无依据的诊断没有意义）")]
    MissingBasis { code: String },
    #[error("E_RULESET_INVALID: 规则码在 {layer} 层内重复声明：{code}（跨层同码是覆盖，同层同码是配置错误）")]
    DuplicateCode { code: String, layer: LayerId },
    #[error("E_RULESET_INVALID: 规则 {code} 引用了未定义的文书类型「{template}」")]
    UndefinedTemplate { code: String, template: String },
    #[error("E_RULESET_INVALID: 病区 {ward} 的豁免引用了不存在的规则码 {code}（死豁免）")]
    UndefinedRuleCode { ward: String, code: String },
    #[error("E_RULESET_INVALID: 根因码族标识 {family} 重复声明（族标识是去重键的一部分，重复即无法判断哪一族说了算）")]
    DuplicateCodeFamily { family: String },
    #[error("E_RULESET_INVALID: 根因码族 {family} 声明非法 — {detail}")]
    InvalidCodeFamily { family: String, detail: String },
    #[error("E_RULESET_INVALID: 规则码 {code} 在根因码族 {family} 内重复列出（同一条不能既是根因又是它的具体化）")]
    DuplicateCodeInFamily { family: String, code: String },
    #[error("E_RULESET_INVALID: 规则码 {code} 同时归入根因码族 {first} 与 {second}（一个码只能属于一个族，否则无法判断谁覆盖谁）")]
    CodeInTwoFamilies { code: String, first: String, second: String },
    #[error("E_RULESET_INVALID: 根因码族 {family} 引用了不存在的规则码 {code}（死族成员 = 永不参与去重的空配置）")]
    UndefinedRuleCodeInFamily { family: String, code: String },
    #[error("E_RULESET_INVALID: 病区豁免缺少必填项 {field}（病区：{ward}）")]
    MissingWaiverField { ward: String, field: &'static str },
    #[error("E_RULESET_INVALID: 病区 {ward} 的豁免时间窗非法 — {detail}（时间窗恒为空，这条豁免等于白写）")]
    InvalidWaiverWindow { ward: String, detail: String },
    #[error("E_RULESET_INVALID: 覆盖环：{cycle}（成环时「哪层说了算」无解）")]
    OverrideCycle { cycle: String },
    #[error("E_RULESET_INVALID: 件对「{pair}」的并列容差声明非法 — {detail}（容差写错的后果是漏报，而漏报在归档核齐时查不出来）")]
    InvalidTieTolerance { pair: String, detail: String },
    #[error("E_RULESET_INVALID: {child} 层 extends 了不存在的层 {parent}")]
    UnknownLayer { child: LayerId, parent: LayerId },
    #[error("E_RULESET_INVALID: {parent} 被多层显式 extends（{children}），覆盖来源有歧义")]
    MultiParent { parent: LayerId, children: String },
}

impl RuleSetError {
    /// 环路径 / 子层列表的可读形式：`科室级 → 个人级 → 科室级`。
    fn render_layers(layers: &[LayerId]) -> String {
        layers.iter().map(|l| l.label()).collect::<Vec<_>>().join(" → ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BASE: &str = r#"
version: "1.2.0"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        section: 患方知情同意
        required: true
  sign_page:
    display: 签署页
    fields: []
hospital:
  doc_types:
    restraint_consent: "约束知情同意"
    sign_page: "签署页"
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      basis: "《精神科住院护理常规》第 4.2 条"
"#;

    #[test]
    fn default_merge_order_is_hospital_department_personal() {
        let rs = load_effective(BASE).unwrap();
        assert_eq!(
            rs.merge_order,
            vec![LayerId::Hospital, LayerId::Department, LayerId::Personal]
        );
    }

    #[test]
    fn sha256_is_stable_across_loads_and_flips_on_one_byte() {
        let a = load_effective(BASE).unwrap();
        let b = load_effective(BASE).unwrap();
        assert_eq!(a.sha256, b.sha256);
        assert_eq!(a.sha256.len(), 64);
        assert_eq!(a.short_sha256().len(), SHA256_SHORT_LEN);

        let mutated = BASE.replace("第 4.2 条", "第 4.3 条");
        let c = load_effective(&mutated).unwrap();
        assert_ne!(a.sha256, c.sha256, "改一个字节后哈希必须改变");
    }

    #[test]
    fn override_cycle_is_rejected() {
        let bad = format!("{BASE}\ndepartment:\n  extends: personal\npersonal:\n  extends: department\n");
        let err = load_effective(&bad).unwrap_err();
        assert!(matches!(err, RuleSetError::OverrideCycle { .. }), "{err}");
        assert!(err.to_string().contains("E_RULESET_INVALID"));
        assert_eq!(released_rule_count(&bad), 0, "成环时零条规则被放行");
    }

    #[test]
    fn self_extends_is_a_cycle() {
        let bad = format!("{BASE}\npersonal:\n  extends: personal\n");
        assert!(matches!(
            load_effective(&bad).unwrap_err(),
            RuleSetError::OverrideCycle { .. }
        ));
    }

    #[test]
    fn multi_parent_is_rejected() {
        let bad = format!(
            "{BASE}\ndepartment:\n  extends: hospital\npersonal:\n  extends: hospital\n"
        );
        assert!(matches!(
            load_effective(&bad).unwrap_err(),
            RuleSetError::MultiParent { .. }
        ));
    }

    // ——— 三级级联：每一层各自生效，且能说清「这条来自哪一层」 ———

    /// 三层各声明一条，并把 QC-PSY-01 的 severity 一路从 blocking 改到 notice。
    const CASCADE: &str = r#"
version: "2.0.0"
templates:
  restraint_consent:
    display: 约束知情同意
    fields: []
  sign_page:
    display: 签署页
    fields: []
hospital:
  doc_types:
    restraint_consent: "约束知情同意"
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "院级标题"
      basis: "《精神科住院护理常规》第 4.2 条"
    - code: "QC-PSY-09"
      severity: blocking
      title: "只有院级配了"
      basis: "《病案书写规范》第 7.1 条"
department:
  rules:
    - code: "QC-PSY-02"
      severity: blocking
      title: "科室级独有"
      basis: "《精神科住院护理常规》第 5.3 条"
    - code: "QC-PSY-01"
      severity: notice
      title: "科室级标题"
      basis: "《精神科住院护理常规》第 4.3 条"
personal:
  rules:
    - code: "QC-PSY-03"
      severity: notice
      title: "个人级独有"
      basis: "《精神科住院护理常规》第 6.1 条"
    - code: "QC-PSY-02"
      severity: notice
      title: "个人级盖科室级"
      basis: "《精神科住院护理常规》第 5.4 条"
"#;

    #[test]
    fn each_layer_contributes_its_own_rules() {
        let rs = load_effective(CASCADE).unwrap();
        assert_eq!(rs.rules.len(), 4, "四个码全部生效：{:?}", rs.rules.keys());
        for code in ["QC-PSY-01", "QC-PSY-02", "QC-PSY-03", "QC-PSY-09"] {
            assert!(rs.rule(code).is_some(), "{code} 应存在");
        }
    }

    #[test]
    fn personal_overrides_department_overrides_hospital() {
        let rs = load_effective(CASCADE).unwrap();
        // 院 blocking → 科室 notice →（个人没配 01）停在科室
        let r01 = rs.rule("QC-PSY-01").unwrap();
        assert_eq!(r01.severity, Severity::Notice);
        assert_eq!(r01.source, LayerId::Department);
        assert_eq!(r01.superseded, vec![LayerId::Hospital]);
        assert!(r01.basis.contains("第 4.3 条"), "依据应来自科室级那条声明");
        assert_eq!(r01.title, "科室级标题");

        // 科室 blocking → 个人 notice
        let r02 = rs.rule("QC-PSY-02").unwrap();
        assert_eq!(r02.severity, Severity::Notice);
        assert_eq!(r02.source, LayerId::Personal);
        assert_eq!(r02.superseded, vec![LayerId::Department]);

        // 院级独有：没被盖过
        let r09 = rs.rule("QC-PSY-09").unwrap();
        assert_eq!(r09.source, LayerId::Hospital);
        assert!(r09.superseded.is_empty());
    }

    #[test]
    fn provenance_names_the_winning_layer_and_what_it_covered() {
        let rs = load_effective(CASCADE).unwrap();
        assert_eq!(
            rs.rule("QC-PSY-01").unwrap().provenance(),
            "QC-PSY-01（科室级，覆盖 院级）"
        );
        assert_eq!(
            rs.rule("QC-PSY-09").unwrap().provenance(),
            "QC-PSY-09（院级）"
        );
    }

    #[test]
    fn rule_declared_only_at_hospital_survives_when_upper_layers_are_empty() {
        // 三层里只有院级配了 09：科室 / 个人层为空不应把它冲掉。
        let rs = load_effective(BASE).unwrap();
        assert_eq!(rs.rules.len(), 1);
        assert_eq!(rs.rule(QC).unwrap().source, LayerId::Hospital);
    }

    // ——— per-ward-ignores：豁免只影响指定病区 ———

    const WITH_WAIVER: &str = r#"
version: "2.1.0"
templates:
  restraint_consent:
    display: 约束知情同意
    fields: []
hospital:
  rules:
    - code: "QC-PSY-01"
      severity: blocking
      title: "件内必填字段缺失"
      basis: "《精神科住院护理常规》第 4.2 条"
per-ward-ignores:
  - ward: "WARD-B"
    codes: ["QC-PSY-01"]
    reason: "开放病区不收约束文书，本病区无此业务"
"#;

    #[test]
    fn waiver_only_applies_to_the_named_ward() {
        let rs = load_effective(WITH_WAIVER).unwrap();
        assert!(rs.is_waived("WARD-B", "QC-PSY-01"));
        assert!(!rs.is_waived("WARD-A", "QC-PSY-01"), "别的病区同一码照报");
        assert!(!rs.is_waived("WARD-B", "QC-PSY-99"), "别的码不受影响");
    }

    #[test]
    fn waiver_reason_is_always_recoverable() {
        let rs = load_effective(WITH_WAIVER).unwrap();
        assert_eq!(
            rs.waived_reason("WARD-B", "QC-PSY-01"),
            Some("开放病区不收约束文书，本病区无此业务")
        );
    }

    #[test]
    fn waiver_without_reason_is_rejected() {
        let y = WITH_WAIVER.replace("开放病区不收约束文书，本病区无此业务", "  ");
        let err = load_effective(&y).unwrap_err();
        assert!(matches!(err, RuleSetError::MissingWaiverField { .. }), "{err}");
        assert_eq!(released_rule_count(&y), 0);
    }

    #[test]
    fn waiver_without_codes_is_rejected() {
        let y = WITH_WAIVER.replace("codes: [\"QC-PSY-01\"]", "codes: []");
        assert!(matches!(
            load_effective(&y).unwrap_err(),
            RuleSetError::MissingWaiverField { .. }
        ));
    }

    #[test]
    fn waiver_pointing_at_a_nonexistent_code_is_rejected() {
        // 死豁免：引用了不存在的码 → 永远不生效，却让维护者以为「豁免了」
        let y = WITH_WAIVER.replace("codes: [\"QC-PSY-01\"]", "codes: [\"QC-PSY-77\"]");
        let err = load_effective(&y).unwrap_err();
        assert!(matches!(err, RuleSetError::UndefinedRuleCode { .. }), "{err}");
        assert_eq!(released_rule_count(&y), 0);
    }

    // ——— 强校验：任一失败 → 零条规则被放行 ———

    #[test]
    fn rule_referencing_an_undefined_template_is_rejected() {
        // 用 QC-PSY-09（只在院级声明、无上层覆盖），否则个人层的同名声明会把它整个盖掉。
        let y = CASCADE.replace(
            "    - code: \"QC-PSY-09\"\n      severity: blocking",
            "    - code: \"QC-PSY-09\"\n      applies_to: [\"not_a_doc_type\"]\n      severity: blocking",
        );
        assert_ne!(y, CASCADE, "夹具替换必须真的生效，否则本用例是假绿");
        let err = load_effective(&y).unwrap_err();
        assert!(matches!(err, RuleSetError::UndefinedTemplate { .. }), "{err}");
        assert_eq!(released_rule_count(&y), 0, "引用未定义表单时零条规则被放行");
    }

    #[test]
    fn dependency_edge_referencing_an_undefined_template_is_rejected() {
        let y = format!(
            "{BASE}\ndepartment:\n  dependencies:\n    - from: sign_page\n      to: not_a_doc_type\n"
        );
        let err = load_effective(&y).unwrap_err();
        assert!(matches!(err, RuleSetError::UndefinedTemplate { .. }), "{err}");
    }

    #[test]
    fn every_validation_failure_releases_zero_rules() {
        // 逐条对照 CLAUDE.md 里的三类强校验：未定义表单引用 / 码重复 / 覆盖环。
        let cases: Vec<(&str, String)> = vec![
            (
                "覆盖环",
                format!("{BASE}\ndepartment:\n  extends: personal\npersonal:\n  extends: department\n"),
            ),
            (
                "同层码重复",
                CASCADE.replace(
                    "personal:\n  rules:",
                    "personal:\n  rules:\n    - code: \"QC-PSY-03\"\n      severity: blocking\n      title: \"重复\"\n      basis: \"《规范》第 1 条\"",
                ),
            ),
            (
                "缺规范依据",
                CASCADE.replace("《精神科住院护理常规》第 6.1 条", ""),
            ),
            (
                "豁免无理由",
                WITH_WAIVER.replace("开放病区不收约束文书，本病区无此业务", ""),
            ),
        ];
        for (name, yaml) in cases {
            assert!(
                load_effective(&yaml).is_err(),
                "{name}：应拒绝加载而不是放行"
            );
            assert_eq!(released_rule_count(&yaml), 0, "{name}：应零条规则被放行");
        }
    }

    #[test]
    fn missing_version_is_rejected() {
        let y = CASCADE.replace("version: \"2.0.0\"", "version: \"\"");
        assert!(matches!(
            load_effective(&y).unwrap_err(),
            RuleSetError::MissingVersion
        ));
    }

    #[test]
    fn override_inherits_fields_the_lower_layer_did_not_restate() {
        // 科室只想把 01 从阻断降为提示，没重写作用范围与依据。
        let r = load_effective(CASCADE).unwrap().rule("QC-PSY-01").unwrap().clone();
        assert_eq!(r.severity, Severity::Notice, "科室写了的字段按科室来");
        assert_eq!(r.title, "科室级标题");
        assert!(r.basis.contains("第 4.3 条"), "科室写了依据 → 用科室的");
        // 关键：科室没写 applies_to → 沿用院级范围，不清空也不扩到全院。
        assert!(r.applies_to_doc_type("sign_page"), "作用范围应沿用院级");
        assert!(r.applies_to_doc_type("restraint_consent"));
    }

    #[test]
    fn narrowing_scope_requires_rewriting_the_whole_list() {
        // 科室想把 QC-PSY-01 的适用范围收窄到住院记录 / 护理记录，必须完整重写。
        // 若只写 severity 而不写 applies_to，约束知情同意的患方签字仍会被降级——
        // 那是本系统最不该松的一类，所以「不写」必须是安全的继承，而不是隐式清空。
        let y = CASCADE.replace(
            "    - code: \"QC-PSY-01\"\n      severity: notice\n      title: \"科室级标题\"",
            "    - code: \"QC-PSY-01\"\n      applies_to: [\"sign_page\"]\n      severity: notice\n      title: \"科室级标题\"",
        );
        assert_ne!(y, CASCADE, "夹具替换必须真的生效");
        let r = load_effective(&y).unwrap().rule("QC-PSY-01").unwrap().clone();
        assert!(r.applies_to_doc_type("sign_page"));
        assert!(!r.applies_to_doc_type("restraint_consent"), "收窄后不应再命中约束类");
    }

    #[test]
    fn explicit_wildcard_is_the_way_to_widen_scope() {
        // 真要扩到全部文书类型，必须显式写 `*`。
        let y = CASCADE.replace(
            "    - code: \"QC-PSY-01\"\n      severity: notice",
            "    - code: \"QC-PSY-01\"\n      applies_to: [\"*\"]\n      severity: notice",
        );
        assert_ne!(y, CASCADE, "夹具替换必须真的生效");
        let r = load_effective(&y).unwrap().rule("QC-PSY-01").unwrap().clone();
        assert!(r.applies_to_doc_type("nursing_record"), "写了 * 就该覆盖所有文书类型");
        assert!(r.applies_to_doc_type("restraint_consent"));
    }

    #[test]
    fn lower_layer_without_basis_inherits_the_hospitals_citation() {
        // 下层只写 severity → 依据沿用上级，仍然非空，仍然能进诊断 message。
        let y = format!(
            "{BASE}\ndepartment:\n  rules:\n    - code: \"QC-PSY-01\"\n      severity: notice\n      title: \"科室降级\"\n"
        );
        let r = load_effective(&y).unwrap().rule("QC-PSY-01").unwrap().clone();
        assert!(r.basis.contains("第 4.2 条"), "应沿用院级依据：{}", r.basis);
        assert_eq!(r.source, LayerId::Department);
    }

    #[test]
    fn applies_to_filters_which_doc_types_a_rule_runs_on() {
        // QC-PSY-09 只在院级声明，没人覆盖 → 可以直接改它的 applies_to。
        let y = CASCADE.replace(
            "    - code: \"QC-PSY-09\"\n      severity: blocking",
            "    - code: \"QC-PSY-09\"\n      applies_to: [\"sign_page\"]\n      severity: blocking",
        );
        let rs = load_effective(&y).unwrap();
        let r = rs.rule("QC-PSY-09").unwrap();
        assert!(r.applies_to_doc_type("sign_page"));
        assert!(!r.applies_to_doc_type("restraint_consent"));
    }

    #[test]
    fn empty_applies_to_means_every_doc_type() {
        // 覆盖时若上层没写 applies_to，等于「这条对所有文书类型生效」
        let rs = load_effective(CASCADE).unwrap();
        let r = rs.rule("QC-PSY-01").unwrap();
        assert!(r.applies_to.is_empty());
        assert!(r.applies_to_doc_type("sign_page"));
        assert!(r.applies_to_doc_type("restraint_consent"));
    }

    #[test]
    fn doc_types_and_dependencies_merge_across_layers() {
        let y = format!(
            "{BASE}\ndepartment:\n  doc_types:\n    sign_page: \"科室改过的签署页名\"\n  dependencies:\n    - from: sign_page\n      to: restraint_consent\n"
        );
        let rs = load_effective(&y).unwrap();
        assert_eq!(rs.doc_types["restraint_consent"], "约束知情同意", "院级条目应保留");
        assert_eq!(rs.doc_types["sign_page"], "科室改过的签署页名", "科室应覆盖院级");
        assert_eq!(rs.dependencies.len(), 1);
        assert!(rs.dependencies[0].requires, "requires 缺省应为「需要」");
    }

    const QC: &str = "QC-PSY-01";

    // ——— 根因码族（Task 23）：由泛到具体的下标即具体度 ———

    /// 在 CASCADE 基础上挂一张两成员码族。
    fn with_family(block: &str) -> String {
        format!("{CASCADE}\n{block}")
    }

    const GOOD_FAMILY: &str = r#"
code-families:
  - family: "field-value"
    label: "件内字段取值"
    codes: ["QC-PSY-01", "QC-PSY-02"]
    reason: "同一栏的整改动作只有一个：改成院内规范写法"
"#;

    #[test]
    fn a_code_family_is_loaded_with_its_own_specificity_order() {
        let rs = load_effective(&with_family(GOOD_FAMILY)).unwrap();
        assert_eq!(rs.code_families.len(), 1);
        let fam = &rs.code_families[0];
        assert_eq!(fam.family, "field-value");
        assert_eq!(fam.label, "件内字段取值");
        // 下标即具体度：越靠后越具体。写反了不会加载失败，只会静默挑错赢家。
        assert_eq!(fam.specificity("QC-PSY-01"), Some(0));
        assert_eq!(fam.specificity("QC-PSY-02"), Some(1));
        assert_eq!(fam.specificity("QC-PSY-03"), None, "没列进族的码不参与折叠");
        assert_eq!(rs.specificity_of("QC-PSY-02"), Some(1));
        assert_eq!(rs.specificity_of("QC-PSY-03"), None);
        assert!(rs.family_of("QC-PSY-01").is_some());
        assert!(rs.family_of("QC-PSY-03").is_none());
    }

    #[test]
    fn the_family_explanation_names_the_winner_and_the_in_hospital_reason() {
        let rs = load_effective(&with_family(GOOD_FAMILY)).unwrap();
        let zh = rs.code_families[0].explain_suppression("QC-PSY-01", "QC-PSY-02");
        assert!(zh.contains("件内字段取值"), "折叠说明要能说清是哪一族：{zh}");
        assert!(zh.contains("field-value"));
        assert!(zh.contains("QC-PSY-01 已被更具体的 QC-PSY-02 覆盖"));
        assert!(zh.contains("改成院内规范写法"), "院内理由必须带出来：{zh}");
    }

    #[test]
    fn a_ruleset_without_families_loads_and_declares_no_families() {
        // 缺省为空 = 不做任何去重。旧规则集不该因为多了一张新表就加载失败。
        let rs = load_effective(CASCADE).unwrap();
        assert!(rs.code_families.is_empty());
        assert!(rs.family_of("QC-PSY-01").is_none());
    }

    #[test]
    fn family_order_does_not_depend_on_declaration_order() {
        // 族序固定按标识升序：同一份 YAML 两次加载族序必须一致，
        // 否则去重结果会随声明顺序抖动，审计导出就没法 diff 了。
        // 两种写法都把 `code-families:` 抬头放在同一位置，只换两条目的先后，
        // 排除「只是 YAML 位置不同」这一解释。
        const FIELD_VALUE: &str =
            "  - family: \"field-value\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-02\"]\n    reason: \"同一栏的整改动作只有一个：改成院内规范写法\"\n";
        const ZZ_LATER: &str = "  - family: \"zz-later\"\n    label: \"后声明的族\"\n    codes: [\"QC-PSY-03\", \"QC-PSY-09\"]\n    reason: \"与 field-value 无关的族\"\n";
        let a = load_effective(&with_family(&format!("code-families:\n{FIELD_VALUE}{ZZ_LATER}"))).unwrap();
        let b = load_effective(&with_family(&format!("code-families:\n{ZZ_LATER}{FIELD_VALUE}"))).unwrap();
        let names = |rs: &EffectiveRules| -> Vec<String> {
            rs.code_families.iter().map(|f| f.family.clone()).collect()
        };
        assert_eq!(names(&a), vec!["field-value", "zz-later"]);
        assert_eq!(names(&a), names(&b), "两种声明顺序必须得到同一族序");
    }

    const GOOD_FAMILIES: &str = r#"
code-families:
  - family: "field-value"
    label: "件内字段取值"
    codes: ["QC-PSY-01", "QC-PSY-02"]
    reason: "同一栏的整改动作只有一个：改成院内规范写法"
"#;

    #[test]
    fn every_code_family_validation_failure_releases_zero_rules() {
        // 五类拒绝逐条断言。共同的底线：**不是**「配错了就当没配」——
        // 静默降级等于让质控办以为去重开着，实际上每处都在刷屏。
        type Case = (&'static str, String, fn(&RuleSetError) -> bool);
        let cases: Vec<Case> = vec![
            (
                "族标识重复",
                format!(
                    "{CASCADE}\n{GOOD_FAMILIES}  - family: \"field-value\"\n    label: \"重名\"\n    codes: [\"QC-PSY-02\", \"QC-PSY-03\"]\n    reason: \"族标识重复\"\n"
                ),
                |e| matches!(e, RuleSetError::DuplicateCodeFamily { family } if family == "field-value"),
            ),
            (
                "族内重复列出同一个码",
                with_family(
                    "code-families:\n  - family: \"f\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-01\"]\n    reason: \"同一码列了两次\"\n",
                ),
                |e| matches!(e, RuleSetError::DuplicateCodeInFamily { code, .. } if code == "QC-PSY-01"),
            ),
            (
                "一个码同时属于两族",
                format!(
                    "{CASCADE}\n{GOOD_FAMILIES}  - family: \"other\"\n    label: \"另一族\"\n    codes: [\"QC-PSY-02\", \"QC-PSY-03\"]\n    reason: \"QC-PSY-02 已经在前一族里\"\n"
                ),
                |e| matches!(e, RuleSetError::CodeInTwoFamilies { code, .. } if code == "QC-PSY-02"),
            ),
            (
                "引用了未定义的规则码",
                with_family(
                    "code-families:\n  - family: \"f\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-77\"]\n    reason: \"QC-PSY-77 根本没定义\"\n",
                ),
                |e| matches!(e, RuleSetError::UndefinedRuleCodeInFamily { code, .. } if code == "QC-PSY-77"),
            ),
            (
                "单条族",
                with_family(
                    "code-families:\n  - family: \"f\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\"]\n    reason: \"只有一条\"\n",
                ),
                |e| matches!(e, RuleSetError::InvalidCodeFamily { detail, .. } if detail.contains("单条族")),
            ),
            (
                "缺院内理由",
                with_family(
                    "code-families:\n  - family: \"f\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-02\"]\n    reason: \"\"\n",
                ),
                |e| matches!(e, RuleSetError::InvalidCodeFamily { detail, .. } if detail.contains("缺院内理由")),
            ),
            (
                "缺院内中文名",
                with_family(
                    "code-families:\n  - family: \"f\"\n    label: \"\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-02\"]\n    reason: \"有理由但没名字\"\n",
                ),
                |e| matches!(e, RuleSetError::InvalidCodeFamily { detail, .. } if detail.contains("缺院内中文名")),
            ),
            (
                "族标识为空",
                with_family(
                    "code-families:\n  - family: \"\"\n    label: \"件内字段取值\"\n    codes: [\"QC-PSY-01\", \"QC-PSY-02\"]\n    reason: \"标识空着\"\n",
                ),
                |e| matches!(e, RuleSetError::InvalidCodeFamily { detail, .. } if detail.contains("族标识为空")),
            ),
        ];
        for (name, yaml, pred) in cases {
            let err = load_effective(&yaml).unwrap_err();
            assert!(pred(&err), "{name} 应被对应错误拒住，实际：{err}");
            assert!(
                err.to_string().contains("E_RULESET_INVALID"),
                "{name} 必须报 E_RULESET_INVALID：{err}"
            );
            assert_eq!(released_rule_count(&yaml), 0, "{name} 时零条规则被放行");
        }
    }

    #[test]
    fn a_completely_duplicated_rule_code_is_refused_not_silently_merged() {
        // 「完全同码重复（规则集 bug）」：同一码在同一层写了两遍。
        // 绝不允许静默按一条处理——那会让去重挑出一个不存在的赢家。
        let dup = CASCADE.replace(
            "    - code: \"QC-PSY-09\"\n      severity: blocking",
            "    - code: \"QC-PSY-09\"\n      severity: notice\n      title: \"同码重复声明\"\n      basis: \"《病案书写规范》第 9.9 条\"\n    - code: \"QC-PSY-09\"\n      severity: blocking",
        );
        let err = load_effective(&dup).unwrap_err();
        assert!(
            matches!(err, RuleSetError::DuplicateCode { layer: LayerId::Hospital, .. }),
            "{err}"
        );
        assert_eq!(released_rule_count(&dup), 0, "零条规则被放行，不降级为「无规则=通过」");
    }
}
