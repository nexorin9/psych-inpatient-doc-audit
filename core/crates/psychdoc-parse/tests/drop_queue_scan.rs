//! Task 32 · 拖放与批量队列的**发现侧**契约。
//!
//! 护士长拖进来的是一整个病区文件夹，而不是一份干净的单文件导出。
//! 因此这里钉死的不是「能不能解析一个 Bundle」，而是
//! **「一堆杂七杂八的文件拖进来会怎样」**：
//!
//! - 能不能打开的照常打开；
//! - 打不开的**逐个记账并给中文原因**，一个坏文件不许拖垮整批
//!   （这正是旧 `discover_dir` 的行为：一份坏 JSON 让整个病区审不了）；
//! - 「看到的文件 = 打开的 + 跳过的」这条守恒等式成立。
//!
//! 聚合与徽标口径见 `src/queue.rs` 的单测；本文件只管发现侧。

use std::path::{Path, PathBuf};

use psychdoc_parse::{
    discover_dir, discover_dir_report, discover_dir_report_with_registry,
    discover_paths_report, FileSkipReason,
};

/// 一个临时目录，Drop 时删掉。
struct TmpDir(PathBuf);

impl TmpDir {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!("psychdoc-queue-{tag}"));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        TmpDir(p)
    }

    fn path(&self) -> &Path {
        &self.0
    }

    fn write(&self, rel: &str, content: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }

    fn write_bytes(&self, rel: &str, content: &[u8]) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&p, content).unwrap();
        p
    }
}

impl Drop for TmpDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 一份最小但合法的脱敏导出包。
///
/// 用 `serde_json::json!` 拼而不是手写字符串：手写 JSON 一旦少个引号，
/// 编译器只会报一串看不懂的 token 错误，而这里错位会一路传染到文件末尾。
fn bundle_for(doc_id: &str, alias: &str, code: &str) -> String {
    serde_json::json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [{
            "fullUrl": format!("urn:uuid:{doc_id}"),
            "resource": {
                "resourceType": "Composition",
                "id": doc_id,
                "status": "preliminary",
                "date": "2026-03-02T23:40:00+08:00",
                "identifier": [{
                    "system": "https://his.example-hospital.invalid/fhir/identifier/encounter",
                    "value": alias,
                }],
                "type": {"coding": [{
                    "system": "https://example-hospital.invalid/fhir/CodeSystem/doc-type",
                    "code": code,
                }]},
                "section": [{"title": "约束事由", "text": "约束事由：夜间反复冲动。"}],
            }
        }]
    })
    .to_string()
}

/// 只声明本文件用到的两个类型码，够验证「字典外被过滤并给原因」。
const REG_YAML: &str = r#"
templates:
  restraint_consent:
    display: 约束知情同意
    fields: []
  restraint_record:
    display: 约束记录
    fields: []
"#;

fn registry() -> psychdoc_model::DocTypeRegistry {
    serde_yaml::from_str(REG_YAML).unwrap()
}

// ---------------------------------------------------------------------------
// 主路径：一整个文件夹拖进来
// ---------------------------------------------------------------------------

#[test]
fn a_whole_ward_folder_recurses_and_aggregates_by_encounter() {
    let dir = TmpDir::new("recurse");
    // 按住院别名分目录 —— 护士长拖进来就是这个形态
    dir.write(
        "ward-a/WARD-A-001-CONSENT.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    dir.write(
        "ward-a/WARD-A-001-RECORD.json",
        &bundle_for("WARD-A-001-RECORD-01", "WARD-A-001", "restraint_record"),
    );
    dir.write(
        "ward-a/nested/deep/WARD-A-002-CONSENT.json",
        &bundle_for("WARD-A-002-CONSENT-01", "WARD-A-002", "restraint_consent"),
    );
    dir.write(
        "ward-b/WARD-B-001-CONSENT.json",
        &bundle_for("WARD-B-001-CONSENT-01", "WARD-B-001", "restraint_consent"),
    );

    let r = discover_dir_report_with_registry(dir.path(), Some(&registry())).unwrap();
    assert!(r.is_balanced(), "文件守恒：{:?}", r.file_skips);
    assert_eq!(r.packages.len(), 4);
    assert_eq!(r.items_len(), 4, "四份包各出一件");
    assert!(r.file_skips.is_empty(), "干净目录不该有跳过：{:?}", r.file_skips);
    assert!(r.dirs_visited >= 5, "根 + ward-a + nested + deep + ward-b");
}

// ---------------------------------------------------------------------------
// 边界一：杂项文件逐个入「已跳过」，坏文件不拖垮整批
// ---------------------------------------------------------------------------

#[test]
fn misc_files_land_in_the_skip_list_with_a_reason_and_a_working_bundle_still_opens() {
    let dir = TmpDir::new("misc");
    dir.write(
        "WARD-A-001-CONSENT.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    // 病区文件夹里真实会混进来的东西
    dir.write_bytes("扫描件.pdf", b"%PDF-1.4 ...");
    dir.write("台账.xlsx", "PK\x03\x04");
    dir.write(".DS_Store", "\x00\x01");
    dir.write("README.txt", "这是护士自己放的说明");
    // 上一份没导完的 JSON —— 旧 discover_dir 会让**整批**失败
    dir.write("半截导出.json", "{\"resourceType\":\"Bundle\",\"entry\":[{\"resource\":");

    let r = discover_dir_report(dir.path()).unwrap();

    assert_eq!(
        r.packages.len(),
        1,
        "一份坏 JSON 不该让唯一的好包也打不开（旧 discover_dir 的缺陷）"
    );
    assert_eq!(r.items_len(), 1);
    assert_eq!(r.file_skips.len(), 5, "四个杂项 + 一个坏 JSON");
    assert!(r.is_balanced(), "看到的文件 = 打开的 1 + 跳过的 5");

    // 每个跳过项都有可执行的中文原因，不出现内部术语
    for s in &r.file_skips {
        let zh = s.as_zh();
        assert!(!zh.is_empty());
        assert!(!s.relative_path.is_empty(), "跳过项要带相对路径");
    }
    let by_name = |n: &str| {
        r.file_skips
            .iter()
            .find(|s| s.name == n)
            .unwrap_or_else(|| panic!("{} 应在已跳过清单里：{:?}", n, r.file_skips))
    };
    assert_eq!(by_name("扫描件.pdf").reason, FileSkipReason::UnsupportedExtension);
    assert!(by_name("扫描件.pdf").as_zh().contains("只认 .json"));
    assert_eq!(by_name("台账.xlsx").reason, FileSkipReason::UnsupportedExtension);
    assert_eq!(by_name(".DS_Store").reason, FileSkipReason::UnsupportedExtension);
    assert_eq!(by_name("README.txt").reason, FileSkipReason::UnsupportedExtension);
    assert_eq!(by_name("半截导出.json").reason, FileSkipReason::MalformedJson);
    assert!(
        by_name("半截导出.json").as_zh().contains("没导完"),
        "坏 JSON 的原因要指向可执行动作：{}",
        by_name("半截导出.json").as_zh()
    );
}

#[test]
fn a_valid_json_that_is_not_a_bundle_is_distinguished_from_a_malformed_one() {
    let dir = TmpDir::new("notabundle");
    dir.write("患者清单.json", r#"{"resourceType":"Patient","id":"P-1"}"#);
    dir.write("缺一半.json", r#"{"resourceType":"#);

    let r = discover_dir_report(dir.path()).unwrap();
    assert!(r.packages.is_empty());
    let reasons: Vec<_> = r.file_skips.iter().map(|s| s.reason).collect();
    assert!(
        reasons.contains(&FileSkipReason::NotABundle),
        "合法 JSON 但不是 Bundle 要单独一类：{:?}",
        r.file_skips
    );
    assert!(reasons.contains(&FileSkipReason::MalformedJson));
    let nb = r
        .file_skips
        .iter()
        .find(|s| s.reason == FileSkipReason::NotABundle)
        .unwrap();
    assert!(nb.as_zh().contains("resourceType=Bundle"), "{}", nb.as_zh());
}

#[test]
fn an_empty_folder_says_so_instead_of_reporting_zero_findings() {
    let dir = TmpDir::new("empty");
    let r = discover_dir_report(dir.path()).unwrap();
    assert!(r.packages.is_empty());
    assert_eq!(r.files_seen, 0);
    assert!(r.is_balanced());
    // 关键：空文件夹不能被当成「0 条命中 = 全部合规」
    let reason = r.empty_reason_zh().expect("空文件夹要给出原因");
    assert!(reason.contains("没有任何文件"), "{reason}");
}

#[test]
fn a_folder_of_only_misc_files_says_so_and_stays_balanced() {
    let dir = TmpDir::new("allmisc");
    dir.write("a.pdf", "%PDF");
    dir.write("b.xlsx", "PK");
    let r = discover_dir_report(dir.path()).unwrap();
    assert!(r.packages.is_empty());
    assert_eq!(r.file_skips.len(), 2);
    assert!(r.is_balanced());
    let reason = r.empty_reason_zh().expect("全是杂项也要给出原因");
    assert!(reason.contains("都不是文书包"), "{reason}");
}

#[test]
fn a_directory_that_has_a_package_is_not_given_the_empty_reason() {
    let dir = TmpDir::new("notempty");
    dir.write(
        "WARD-A-001-CONSENT.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    let r = discover_dir_report(dir.path()).unwrap();
    assert_eq!(r.empty_reason_zh(), None, "有包就不该再喊「没找到」");
}

// ---------------------------------------------------------------------------
// 边界二：类型字典过滤在目录批扫上同样生效并给原因
// ---------------------------------------------------------------------------

#[test]
fn the_registry_filter_applies_to_directory_scans_too_with_a_reason_per_item() {
    let dir = TmpDir::new("registry");
    dir.write(
        "ok.json",
        &bundle_for("D-OK", "WARD-A-001", "restraint_consent"),
    );
    dir.write(
        "字典外.json",
        &bundle_for("D-UNKNOWN", "WARD-A-001", "mystery_form"),
    );

    let r = discover_dir_report_with_registry(dir.path(), Some(&registry())).unwrap();
    assert_eq!(r.packages.len(), 2, "两份包都打开了");
    assert_eq!(r.items_len(), 1, "只有字典内那件进审查");
    let filtered: Vec<_> = r
        .packages
        .iter()
        .flat_map(|(_, pkg)| pkg.filtered.iter())
        .collect();
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].doc_id, "D-UNKNOWN");
    assert!(filtered[0].reason.as_zh().contains("类型字典"));
}

// ---------------------------------------------------------------------------
// 多根拖放：一次拖几个病区文件夹
// ---------------------------------------------------------------------------

#[test]
fn several_folders_dropped_at_once_are_scanned_together_without_double_counting() {
    let ward_a = TmpDir::new("multi-a");
    let ward_b = TmpDir::new("multi-b");
    ward_a.write(
        "WARD-A-001-CONSENT.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    ward_b.write(
        "WARD-B-001-CONSENT.json",
        &bundle_for("WARD-B-001-CONSENT-01", "WARD-B-001", "restraint_consent"),
    );
    ward_b.write("杂项.pdf", "%PDF");

    let roots = vec![ward_a.path().to_path_buf(), ward_b.path().to_path_buf()];
    let r = discover_paths_report(&roots, Some(&registry())).unwrap();
    assert_eq!(r.packages.len(), 2);
    assert_eq!(r.file_skips.len(), 1);
    assert!(r.is_balanced());

    // 同一文件夹拖两次只审一遍
    let dup = vec![
        ward_a.path().to_path_buf(),
        ward_b.path().to_path_buf(),
        ward_a.path().to_path_buf(),
    ];
    let r2 = discover_paths_report(&dup, Some(&registry())).unwrap();
    assert_eq!(r2.packages.len(), 2, "重复的根要去重，不能把同一次住院数两遍");
    assert_eq!(r2.file_skips.len(), 1);
    assert!(r2.is_balanced());
}

#[test]
fn a_single_file_dropped_on_its_own_is_scanned_like_a_folder() {
    let dir = TmpDir::new("single");
    let file = dir.write(
        "WARD-A-001-CONSENT.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    dir.write("杂项.pdf", "%PDF");

    // 拖单个文件：根不是目录，走文件根分支
    let r = discover_paths_report(&[file], Some(&registry())).unwrap();
    assert_eq!(r.packages.len(), 1);
    assert_eq!(r.items_len(), 1);
    assert!(r.file_skips.is_empty(), "只扫了被拖的那一个文件，旁边的 pdf 不该跳出来");
    // 单文件根不进 files_seen（它不是「扫到一个文件夹里的文件」）
    assert_eq!(r.files_seen, 0);
}

// ---------------------------------------------------------------------------
// 兼容：旧入口形状不变
// ---------------------------------------------------------------------------

#[test]
fn the_old_discover_dir_entry_point_still_returns_just_the_packages() {
    let dir = TmpDir::new("legacy");
    dir.write(
        "a.json",
        &bundle_for("WARD-A-001-CONSENT-01", "WARD-A-001", "restraint_consent"),
    );
    dir.write("notes.txt", "非导出包");
    let found = discover_dir(dir.path()).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].0.file_name().unwrap(), "a.json");
}

// ---------------------------------------------------------------------------
// 可复现：同一目录两次扫描逐字段相同
// ---------------------------------------------------------------------------

#[test]
fn two_scans_of_the_same_folder_are_byte_for_byte_reproducible() {
    let dir = TmpDir::new("repro");
    for i in 1..=3 {
        dir.write(
            &format!("WARD-A-00{i}-CONSENT.json"),
            &bundle_for(
                &format!("WARD-A-00{i}-CONSENT-01"),
                &format!("WARD-A-00{i}"),
                "restraint_consent",
            ),
        );
    }
    // 故意让杂项文件的**加入顺序**与字典序相反
    dir.write("z.pdf", "%PDF");
    dir.write("a.xlsx", "PK");
    dir.write("m.txt", "note");

    let a = discover_dir_report(dir.path()).unwrap();
    let b = discover_dir_report(dir.path()).unwrap();
    assert_eq!(a.packages.len(), b.packages.len());
    assert_eq!(a.file_skips, b.file_skips, "跳过清单顺序必须可复现，否则审计导出没法 diff");
    assert_eq!(a.file_skips.len(), 3);
    // 字典序而非写入序
    let names: Vec<_> = a.file_skips.iter().map(|s| s.name.as_str()).collect();
    assert_eq!(names, vec!["a.xlsx", "m.txt", "z.pdf"]);
}