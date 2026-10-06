//! 合成脱敏文书包生成器 —— 供**规模基准**与回归用例批量取样。
//!
//! ## 为什么需要它
//!
//! 真实规模验收要求「≥200 件文书包批扫」，而院内导出包不可能进代码库。
//! 本模块按**与 `rulesets/hospital.yaml` 完全一致的字段规范**生成
//! 合成 Bundle：字段名、段落标题、值类型、取值域、单位全部取自交付规则集，
//! 因此生成物**能被真实规则命中**，而不是「长得像但永远合规」的玩具数据。
//!
//! ## 三条脱敏纪律
//!
//! 1. **标识一律为占位别名**：`<WARD>-<SEQ>-<TYPE>-<nn>`，形如
//!    `SYN-W007-RECORD-01`。不含姓名、住院号、身份证样式字符串。
//! 2. **签署与执行人一律占位**：`<EXAMPLE_NURSE>` / `<EXAMPLE_PRACTITIONER>`。
//! 3. **诊断字段只写文书类型无关的中性描述**，不写任何具体病情表述。
//!
//! 违规注入是**参数化**的（[`ViolationProfile`]）：调用方声明要注入哪几类违规，
//! 生成器保证同一参数两次生成**字节一致**（无随机、无当前时间）。

use serde_json::{json, Value};

/// 一次住院里要生成哪些文书件。默认 6 件，接近真实约束流程的成套规模。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PackageShape {
    pub inpatient_record: bool,
    pub nursing_record: bool,
    pub assessment_report: bool,
    pub restraint_consent: bool,
    pub restraint_record: bool,
    pub restraint_release: bool,
    pub sign_page: bool,
}

impl Default for PackageShape {
    /// 六件套：住院记录 / 护理记录 / 评估报告 / 约束同意 / 约束记录 / 约束解除。
    ///
    /// 不带签署页：带了就必然触发 `QC-PSY-08`（签署页缺 `authoredOn`），
    /// 会让「合规包零命中」的基准用例被污染。需要签署页的用例显式打开。
    fn default() -> Self {
        Self {
            inpatient_record: true,
            nursing_record: true,
            assessment_report: true,
            restraint_consent: true,
            restraint_record: true,
            restraint_release: true,
            sign_page: false,
        }
    }
}

impl PackageShape {
    /// 这个形状会生成几件。
    pub fn item_count(&self) -> usize {
        [
            self.inpatient_record,
            self.nursing_record,
            self.assessment_report,
            self.restraint_consent,
            self.restraint_record,
            self.restraint_release,
            self.sign_page,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }

    /// 一个文书都不生成的形状。
    ///
    /// 只给「按件算住院数」的除法用：真实形状永远不为空，但那条除法
    /// 仍必须对空形状给出答案而不是 panic——除零是取件数的工具级事故。
    pub fn default_shaped_none() -> Self {
        Self {
            inpatient_record: false,
            nursing_record: false,
            assessment_report: false,
            restraint_consent: false,
            restraint_record: false,
            restraint_release: false,
            sign_page: false,
        }
    }
}

/// 要注入哪几类违规。全部为 `false` 时是**合规包**（基准的零误报对照组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ViolationProfile {
    /// 约束知情同意的患方签字留空 → `QC-PSY-01`
    pub missing_consent_signature: bool,
    /// 评估时间写成全角 → `QC-PSY-02`
    pub fullwidth_assessment_time: bool,
    /// 约束等级写成勾选项外的值 → `QC-PSY-03`
    pub out_of_domain_level: bool,
    /// 约束时长写成非分钟单位 → `QC-PSY-04`
    pub wrong_duration_unit: bool,
    /// 解除时间早于约束起始 → `QC-PSY-05`（两件各一条）
    pub release_before_start: bool,
    /// 评估时间晚于首次约束 → `QC-PSY-06`（两件各一条）
    pub assessment_after_restraint: bool,
    /// 签署页缺 `authoredOn` → `QC-PSY-08`
    pub sign_page_without_moment: bool,
}

impl ViolationProfile {
    /// 声明的违规种类数（用于基准里算「应当命中多少条」的期望值上界）。
    pub fn declared_count(&self) -> usize {
        [
            self.missing_consent_signature,
            self.fullwidth_assessment_time,
            self.out_of_domain_level,
            self.wrong_duration_unit,
            self.release_before_start,
            self.assessment_after_restraint,
            self.sign_page_without_moment,
        ]
        .iter()
        .filter(|b| **b)
        .count()
    }
}

/// 生成器的确定性参数。
///
/// `base_day` 是**天序号**（自 1970-01-01 起的天数），不是日期字符串：
/// 用整数才能保证「同参数两次生成字节一致」——传日期字符串会引入解析差异，
/// 传「今天」则会随时间漂移，基准就无法复现。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SynthParams {
    pub shape: PackageShape,
    pub violations: ViolationProfile,
    /// 住院别名序号（`WARD` 为固定前缀，见 [`SYNTH_WARD`]）。
    pub encounter_seq: u32,
    /// 基准日序号（每次生成整体偏移一天，避免所有件时刻雷同）。
    pub base_day: u32,
    /// 住院别名前缀。默认 `SYNTH`。
    pub ward: &'static str,
}

/// 默认病区前缀。带 `SYNTH` 是为了让测试产物一眼可辨为合成数据，
/// 不会被误当成真实病历导出。
pub const SYNTH_WARD: &str = "SYNTH";

impl Default for SynthParams {
    fn default() -> Self {
        Self {
            shape: PackageShape::default(),
            violations: ViolationProfile::default(),
            encounter_seq: 1,
            // 2026-03-02 的天序号（1970-01-01 起 20514 天）
            base_day: 20514,
            ward: SYNTH_WARD,
        }
    }
}

impl SynthParams {
    pub fn encounter_seq(mut self, seq: u32) -> Self {
        self.encounter_seq = seq;
        self
    }

    pub fn violations(mut self, v: ViolationProfile) -> Self {
        self.violations = v;
        self
    }

    pub fn shape(mut self, s: PackageShape) -> Self {
        self.shape = s;
        self
    }

    pub fn ward(mut self, w: &'static str) -> Self {
        self.ward = w;
        self
    }
}

/// 生成一个住院别名。
pub fn synth_alias(params: &SynthParams) -> String {
    format!("{}-W{:03}", params.ward, params.encounter_seq)
}

/// 天序号 → `YYYY-MM-DD`。不依赖 chrono，纯算法，任何机器结果一致。
fn civil_from_days(z: u32) -> (i64, u32, u32) {
    let z = z as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

fn date_str(day: u32) -> String {
    let (y, m, d) = civil_from_days(day);
    format!("{y:04}-{m:02}-{d:02}")
}

fn datetime_str(day: u32, hour: u32, minute: u32) -> String {
    format!("{} {hour:02}:{minute:02}", date_str(day))
}

/// `"YYYY-MM-DD HH:MM"` → `(hour, minute)`。取不出时回落到 08:00。
fn hour_minute(dt: &str) -> (u32, u32) {
    dt.rsplit_once(' ')
        .map(|(_, hm)| match hm.split_once(':') {
            Some((h, m)) => (h.parse().unwrap_or(8), m.parse().unwrap_or(0)),
            None => (8, 0),
        })
        .unwrap_or((8, 0))
}

/// 把数字转成**全角**字符（`QC-PSY-02` 的触发条件之一）。
fn to_fullwidth(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '0'..='9' => char::from_u32(u32::from(c) + 0xFF10).unwrap_or(c),
            '-' => '－',
            other => other,
        })
        .collect()
}

/// 生成一份 Bundle（`serde_json::Value`）。
///
/// 字段规范严格对齐 `rulesets/hospital.yaml` 的模板表与
/// `spec.md §2` 的 FHIR R4 最小集：只写产品真正读取的字段。
pub fn synth_package(params: &SynthParams) -> Value {
    let alias = synth_alias(params);
    let day = params.base_day;
    let v = params.violations;

    // 各件的时刻安排。合规包的时序是「评估 → 签署同意 → 约束 → 解除」，
    // 违规包按 ViolationProfile 逐项打乱对应那一对。
    let assess_time = if v.assessment_after_restraint {
        datetime_str(day, 15, 0)
    } else {
        datetime_str(day, 8, 0)
    };
    let start_time = datetime_str(day, 9, 0);
    let release_day = day + if v.release_before_start { 0 } else { 1 };
    let release_time = if v.release_before_start {
        datetime_str(day, 7, 0)
    } else {
        datetime_str(release_day, 13, 10)
    };

    let mut entries: Vec<Value> = Vec::new();

    if params.shape.inpatient_record {
        entries.push(comp(
            &doc_id(params, "INPATIENT"),
            &alias,
            "inpatient_record",
            DocMoment::at(day, 7, 30),
            json!([{
                "title": "入院记录",
                "text": format!(
                    "入院时间：{}。\n诊断：<EXAMPLE_DIAGNOSIS_CATEGORY>。",
                    datetime_str(day, 7, 30)
                )
            }]),
        ));
    }

    if params.shape.nursing_record {
        entries.push(comp(
            &doc_id(params, "NURSING"),
            &alias,
            "nursing_record",
            DocMoment::at(day, 8, 10),
            json!([{
                "title": "护理记录",
                "text": format!(
                    "巡视时间：{}。\n病情描述：<EXAMPLE_NURSING_NOTE>。",
                    datetime_str(day, 8, 10)
                )
            }]),
        ));
    }

    if params.shape.assessment_report {
        // 全角写法只影响**段内业务时刻**（触发 QC-PSY-02）；
        // `Composition.date` 仍是合法 RFC3339，否则解析层会先拒收这件，
        // 变成「解析失败」而不是「值形状不符」——那测的就不是同一条规则了。
        let shown_time = if v.fullwidth_assessment_time {
            to_fullwidth(&assess_time)
        } else {
            assess_time.clone()
        };
        let (ah, am) = hour_minute(&assess_time);
        entries.push(comp(
            &doc_id(params, "ASSESS"),
            &alias,
            "assessment_report",
            DocMoment::at(day, ah, am),
            json!([{
                "title": "入院评估",
                "text": format!(
                    "评估时间：{shown_time}。\n风险等级：中高风险。\n评估医师：<EXAMPLE_PRACTITIONER>。"
                )
            }]),
        ));
    }

    if params.shape.restraint_consent {
        // 患方签字留空 → QC-PSY-01。写成空串而不是「键缺席」：
        // 键缺席落模板声明的段起点，值区间是零宽，两种形态都要能测。
        let signature = if v.missing_consent_signature {
            String::new()
        } else {
            "<EXAMPLE_SIGNATURE>".to_string()
        };
        entries.push(comp(
            &doc_id(params, "CONSENT"),
            &alias,
            "restraint_consent",
            DocMoment::at(day, 8, 20),
            json!([
                {
                    "title": "约束事由",
                    "text": "约束事由：夜间反复冲动，需短时保护。\n保护等级：二级保护。"
                },
                {
                    "title": "患方知情同意",
                    "text": format!(
                        "患方意见：已知情同意，同意按护理常规采取保护措施。\n约束起始时间：{start_time}。\n执行护士：<EXAMPLE_NURSE>。\n患方签字：{signature}"
                    )
                }
            ]),
        ));
    }

    if params.shape.restraint_record {
        let level = if v.out_of_domain_level { "四级保护" } else { "二级保护" };
        let duration = if v.wrong_duration_unit { "2 小时" } else { "120 分钟" };
        entries.push(comp(
            &doc_id(params, "RECORD"),
            &alias,
            "restraint_record",
            DocMoment::at(day, 9, 0),
            json!([{
                "title": "约束经过",
                "text": format!(
                    "约束起始时间：{start_time}。\n约束等级：{level}。\n执行护士：<EXAMPLE_NURSE>。\n观察记录：约束期间生命体征平稳。\n约束时长：{duration}。"
                )
            }]),
        ));
    }

    if params.shape.restraint_release {
        let (rh, rm) = hour_minute(&release_time);
        entries.push(comp(
            &doc_id(params, "RELEASE"),
            &alias,
            "restraint_release",
            DocMoment::at(release_day, rh, rm),
            json!([{
                "title": "解除经过",
                "text": format!(
                    "解除时间：{release_time}。\n解除依据：症状缓解。\n执行护士：<EXAMPLE_NURSE>。"
                )
            }]),
        ));
    }

    if params.shape.sign_page {
        // 缺 authoredOn → 该件「签了名却没有签署时刻」→ QC-PSY-08。
        // 字段**整个不写**，不留空串：空串仍会被解析成 Date 时刻。
        let authored_on = (!v.sign_page_without_moment)
            .then(|| format!("{}T09:35:00+08:00", date_str(day)));
        entries.push(comp_with(
            &doc_id(params, "SIGN"),
            &alias,
            "sign_page",
            DocMoment::at(day, 9, 30),
            json!([{
                "title": "签署页",
                "text": "签署页说明：本页由当班护士与值班医师共同签署。\n护士签字：<EXAMPLE_NURSE>。\n医师签字：<EXAMPLE_PRACTITIONER>。"
            }]),
            authored_on,
        ));
    }

    json!({
        "_comment": "合成脱敏夹具：按院内文书类型字典与字段规范生成，不含任何真实患者标识。",
        "resourceType": "Bundle",
        "type": "collection",
        "entry": entries,
    })
}

/// 生成 `n` 个住院的合成包（住院别名序号从 `start_seq` 连续递增）。
pub fn synth_corpus(start_seq: u32, n: u32, params: SynthParams) -> Vec<Value> {
    (0..n)
        .map(|i| {
            let mut p = params;
            p.encounter_seq = start_seq + i;
            // 每个住院整体偏移一天，保证件间时刻不雷同、时间线不退化。
            p.base_day = params.base_day + i;
            synth_package(&p)
        })
        .collect()
}

/// 一档规模基准的目标**件数**。
///
/// 按件而不是按住院数定档，是因为现场真正会被卡住的是**件**：
/// 护士长一次拖进来的文件夹是几百件文书，不是几百次住院。
/// 三档（200 / 500 / 1000）覆盖「一个病区一天」到「全院一个上午」的量级。
pub const SCALE_TIERS: [usize; 3] = [200, 500, 1000];

/// 按目标**件数**推出住院数（向上取整，保证件数 ≥ 目标）。
///
/// 用 `div_ceil` 而不是整除再补零：宁可多生成一整个住院，也不要为了凑整数
/// 去截断——截断会让「最后一件正好是被改动的那件」这类断言变得不可复现。
pub fn encounters_for_items(target_items: usize, shape: PackageShape) -> u32 {
    let per_encounter = shape.item_count().max(1);
    target_items.div_ceil(per_encounter) as u32
}

/// 生成「件数 ≥ `target_items`」的一批合成包，返回 `(包列表, 实际件数)`。
///
/// 实际件数**显式返回**而不是让调用方自己数：基准要断言的是「这批真的有
/// 这么多件」，件数算错时基准必须立刻红，而不是拿着一个偏小的批次
/// 报出一份好看的耗时。
pub fn synth_corpus_by_items(
    start_seq: u32,
    target_items: usize,
    params: SynthParams,
) -> (Vec<Value>, usize) {
    let encounters = encounters_for_items(target_items, params.shape);
    let corpus = synth_corpus(start_seq, encounters, params);
    let items = corpus
        .iter()
        .map(|p| p["entry"].as_array().map(|e| e.len()).unwrap_or(0))
        .sum();
    (corpus, items)
}

/// 该住院里预期会命中的规则码（按注入的违规推算）。
///
/// 这是**基准的期望值来源**：基准测试断言「实际命中数 == 期望命中数」，
/// 而不是「> 0」。这样规则被改坏（漏报或多报）时基准会立刻红。
pub fn expected_codes(params: &SynthParams) -> Vec<&'static str> {
    let v = params.violations;
    let mut codes = Vec::new();
    if params.shape.restraint_consent && v.missing_consent_signature {
        codes.push("QC-PSY-01");
    }
    if params.shape.assessment_report && v.fullwidth_assessment_time {
        codes.push("QC-PSY-02");
    }
    if params.shape.restraint_record && v.out_of_domain_level {
        codes.push("QC-PSY-03");
    }
    if params.shape.restraint_record && v.wrong_duration_unit {
        codes.push("QC-PSY-04");
    }
    if params.shape.restraint_record
        && params.shape.restraint_release
        && v.release_before_start
    {
        // 违反的两件各自一条
        codes.push("QC-PSY-05");
        codes.push("QC-PSY-05");
    }
    if params.shape.assessment_report
        && params.shape.restraint_record
        && v.assessment_after_restraint
    {
        codes.push("QC-PSY-06");
        codes.push("QC-PSY-06");
    }
    if params.shape.sign_page && v.sign_page_without_moment {
        codes.push("QC-PSY-08");
    }
    codes
}

// ---------------------------------------------------------------------------
// 组装 Composition
// ---------------------------------------------------------------------------

fn doc_id(params: &SynthParams, tag: &str) -> String {
    format!(
        "{}-W{:03}-{}-01",
        params.ward, params.encounter_seq, tag
    )
}

/// 造一份 Composition。
///
/// `date` 是文书业务日期（`Composition.date`），`authored_on` 是**签署时刻**
/// （`Composition.authoredOn`）。`authored_on` 传 `None` 时**不写该字段**——
/// 这正是 `QC-PSY-08` 要抓的「签了名却没有签署时刻」，必须靠「字段缺席」
/// 表达，不能靠留空串冒充。
fn comp_with(
    id: &str,
    alias: &str,
    doc_type: &str,
    when: DocMoment,
    section: Value,
    authored_on: Option<String>,
) -> Value {
    let mut resource = json!({
        "resourceType": "Composition",
        "id": id,
        "status": "preliminary",
        "title": "<脱敏样例文书>",
        "identifier": [{
            "system": "https://his.example-hospital.invalid/fhir/identifier/encounter",
            "value": alias,
        }],
        // 解析层用 RFC3339 读 date，因此这里给完整时间戳而不是「08:00」。
        "date": format!(
            "{}T{:02}:{:02}:00+08:00",
            date_str(when.day),
            when.hour,
            when.minute
        ),
        "type": {
            "coding": [{
                "system": "https://example-hospital.invalid/fhir/CodeSystem/doc-type",
                "code": doc_type,
            }]
        },
        "author": [{ "reference": "Practitioner/<EXAMPLE_PRACTITIONER>" }],
        "section": section,
    });
    if let Some(a) = authored_on {
        resource["authoredOn"] = json!(a);
    }
    json!({ "fullUrl": format!("urn:uuid:{id}"), "resource": resource })
}

/// 一件文书的时间锚点：业务日期 + 时刻。
///
/// 单独抽成结构体而不是继续摊成参数：`day / hour / minute` 三个数并排传
/// 很容易在调用处看串（09 与 09:30 差一位，肉眼极难发现），成组后调用点是
/// `DocMoment::at(day, 9, 30)`，含义自明。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DocMoment {
    day: u32,
    hour: u32,
    minute: u32,
}

impl DocMoment {
    fn at(day: u32, hour: u32, minute: u32) -> Self {
        Self { day, hour, minute }
    }

    /// 业务日期 + 20 分钟后作为签署时刻（模拟「写完文书二十分钟内签」）。
    fn authored(self) -> String {
        format!(
            "{}T{:02}:{:02}:00+08:00",
            date_str(self.day),
            self.hour,
            (self.minute + 20).min(59)
        )
    }
}

/// 业务日期与时刻齐全的常规件（`authoredOn` = 业务日期 20 分钟后）。
fn comp(
    id: &str,
    alias: &str,
    doc_type: &str,
    when: DocMoment,
    section: Value,
) -> Value {
    comp_with(
        id,
        alias,
        doc_type,
        when,
        section,
        Some(when.authored()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn generation_is_byte_identical_for_identical_params() {
        let p = SynthParams::default();
        let a = serde_json::to_string(&synth_package(&p)).unwrap();
        let b = serde_json::to_string(&synth_package(&p)).unwrap();
        assert_eq!(a, b, "同参数两次生成必须字节一致");
    }

    #[test]
    fn different_encounter_seq_yields_different_aliases() {
        let a = synth_alias(&SynthParams::default().encounter_seq(1));
        let b = synth_alias(&SynthParams::default().encounter_seq(2));
        assert_ne!(a, b);
        assert!(a.starts_with("SYNTH-W001"));
    }

    #[test]
    fn civil_date_conversion_matches_known_days() {
        // 天序号（1970-01-01 起）→ 日期。用 Python 的
        // `(date(2026,3,2)-date(1970,1,1)).days` 独立核对过，避免自己验自己。
        assert_eq!(date_str(0), "1970-01-01");
        assert_eq!(date_str(20514), "2026-03-02");
        assert_eq!(date_str(20541), "2026-03-29");
        // 闰年与世纪边界
        assert_eq!(date_str(59), "1970-03-01"); // 1970 非闰年
        assert_eq!(date_str(11323), "2001-01-01");
    }

    #[test]
    fn default_shape_emits_six_documents() {
        let pkg = synth_package(&SynthParams::default());
        assert_eq!(pkg["entry"].as_array().unwrap().len(), 6);
        assert_eq!(PackageShape::default().item_count(), 6);
    }

    #[test]
    fn generated_package_carries_no_real_patient_identifiers() {
        let pkg = synth_package(&SynthParams::default().violations(ViolationProfile {
            missing_consent_signature: true,
            ..Default::default()
        }));
        let text = serde_json::to_string(&pkg).unwrap();
        for banned in ["姓名", "住院号", "身份证", "patient_name", "mrn"] {
            assert!(!text.contains(banned), "生成物不得出现 {banned}");
        }
        assert!(text.contains("<EXAMPLE_"), "占位符应保留");
    }

    #[test]
    fn encounters_for_items_rounds_up_and_never_truncates() {
        // 6 件/住院：200 → 34（204 件），501 → 84（504 件）
        assert_eq!(encounters_for_items(200, PackageShape::default()), 34);
        assert_eq!(encounters_for_items(204, PackageShape::default()), 34);
        assert_eq!(encounters_for_items(205, PackageShape::default()), 35);
        // 除零保护：空形状也必须给出一个可用的住院数，而不是 panic。
        assert_eq!(encounters_for_items(10, PackageShape::default_shaped_none()), 10);
    }

    #[test]
    fn every_scale_tier_yields_at_least_its_target_item_count() {
        // 三档规模各生成一次，逐档核对「实际件数 ≥ 目标件数」——
        // 件数是基准后面所有断言的分母，分母错了后面全错。
        for target in SCALE_TIERS {
            let (corpus, items) =
                synth_corpus_by_items(1, target, SynthParams::default());
            assert!(
                items >= target,
                "目标 {target} 件，实际只生成 {items} 件（{} 个住院）",
                corpus.len()
            );
            // 逐包核对，别信总数口径。
            let per_package: usize = corpus
                .iter()
                .map(|p| p["entry"].as_array().unwrap().len())
                .sum();
            assert_eq!(per_package, items, "包内 entry 数之和必须与实际件数一致");
        }
    }

    #[test]
    fn scale_tiers_build_byte_identical_corpora() {
        // 规模基准要能被复现：同一目标件数两次生成必须逐字节相同，
        // 否则两次跑出来的耗时根本没法互相比较。
        let a = synth_corpus_by_items(1, 200, SynthParams::default()).0;
        let b = synth_corpus_by_items(1, 200, SynthParams::default()).0;
        assert_eq!(
            serde_json::to_string(&a).unwrap(),
            serde_json::to_string(&b).unwrap()
        );
    }

    #[test]
    fn scale_corpus_keeps_cross_document_time_relations_non_degenerate() {
        // 「图非退化」这条要求不能靠肉眼。要钉住的是两件事：
        //
        //  1. **住院内**六件各自落在不同时刻上，构成一条
        //     入院 → 护理 → 评估 → 同意 → 约束 → 解除 的有序链——
        //     跨文书时间序规则（QC-PSY-05/06/07）要有东西可比；
        //  2. **住院之间**整体按天错开——否则 1000 件全挤在同一天，
        //     规模一大时间线就退化成一条平线，join 出的组全一样，
        //     基准等于在测一条退化路径。
        let (corpus, items) = synth_corpus_by_items(1, 200, SynthParams::default());
        assert!(items >= 200);
        assert_eq!(corpus.len(), 34);

        let mut encounter_days: BTreeSet<String> = BTreeSet::new();
        for pkg in &corpus {
            let entries = pkg["entry"].as_array().unwrap();
            let moments: Vec<String> = entries
                .iter()
                .map(|e| e["resource"]["date"].as_str().unwrap().to_string())
                .collect();
            assert_eq!(moments.len(), 6);
            let unique = {
                let mut m = moments.clone();
                m.sort();
                m.dedup();
                m.len()
            };
            assert_eq!(
                unique, 6,
                "同一住院内 6 件应各有不同时刻，实际只有 {unique} 个：{moments:?}"
            );
            // 住院别名上的日期整体错开：34 个住院至少落在 30 个不同日期上。
            let first_date = moments[0][..10].to_string();
            encounter_days.insert(first_date);
        }
        assert!(
            encounter_days.len() >= 30,
            "34 个住院只落在 {} 个日期上，住院间时刻已退化",
            encounter_days.len()
        );
    }
}