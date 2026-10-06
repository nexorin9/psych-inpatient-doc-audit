//! 文书包**发现 → 过滤 → 实体绑定**：从一堆导出文件里认出「哪些是文书」、
//! 「哪件绑哪个住院别名」、「哪件绑定不唯一要人来看」。
//!
//! ## 源产品主能力与改写
//!
//! 源主路径在 `ruff analyze graph` 的命令入口
//! （`github_ref/ruff/crates/ruff/src/commands/analyze_graph.rs:23-52`）：
//!
//! ```text
//! resolve_default_files(args.files)      // ① 发现
//!   → project_files_in_path(...)         // ② 解析 + 配置归属
//!   → paths.retain(|p| SourceType::from(p) == Python)  // ③ 类型过滤
//!   → resolve_module_confident(...)      // ④ 实体解析（confident 才继续）
//!   → ImportMap::dependencies/dependents // ⑤ 出图
//! ```
//!
//! 三条行为被逐条改写进本产品：
//!
//! | 源侧 | 本产品侧 |
//! |---|---|
//! | ① `resolve_default_files` 递归发现 `.py` | [`discover_dir`] 递归发现 `*.json` / `*.ndjson`，其余显式列为「已跳过」并给原因 |
//! | ③ `paths.retain(SourceType::Python)` **静默剔除**非 Python 文件 | [`filter_by_registry`] 按院内文书类型字典过滤，**每件被过滤的都记原因**，前端「已跳过」清单可见 |
//! | ④ `resolve_module_confident` 返回 confident 才连边，否则该模块不入图 | [`EntityBinder`] 只给 `Confident` 的件产出依赖边；`Ambiguous` / `Unresolved` 进 [`ManualQueueEntry`] 人工队列，**绝不静默配对** |
//!
//! 「同名文书」的医院语义：HIS、EMR、护理系统各导一份「约束记录」，
//! `template_id` 相同但 `source_system` 不同 —— 此时**不知道以哪一份为准**，
//! 必须标 `Ambiguous`（对应源产品同名模块解析到多个路径时的 `None`）。

use crate::tolerance::{decode_export_bytes, parse_export, SourceEncoding};
use crate::{parse_bundle, ParsedBundle, ParseError};
use psychdoc_model::{Binding, DocItem, DocTypeRegistry};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 一个导出包在发现阶段的处理结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiscoveredPackage {
    /// 成功解析出的文书件
    pub items: Vec<DocItem>,
    /// 解析期诊断（结构问题、部分成功等）
    pub errors: Vec<psychdoc_model::Diagnostic>,
    /// 被过滤掉的 entry 与原因（**显式记，不静默跳过**）
    pub filtered: Vec<FilteredEntry>,
    /// 实际判定的编码（Task 22：院内导出给的是文件，文件是字节）
    #[serde(default)]
    pub encoding: SourceEncoding,
}

/// `DiscoveredPackage` 的空壳，供「读都读不动」时记账。
impl Default for DiscoveredPackage {
    fn default() -> Self {
        Self {
            items: Vec::new(),
            errors: Vec::new(),
            filtered: Vec::new(),
            encoding: SourceEncoding::Utf8,
        }
    }
}

/// 被过滤的 entry。`reason` 决定它进「已跳过」清单时给用户看的那句话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilteredEntry {
    pub doc_id: String,
    pub resource_type: String,
    pub reason: SkipReason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum SkipReason {
    /// 资源类型不是 Composition（非文书正文，如 DocumentReference / Provenance）
    NotADocument,
    /// `type.coding` 缺失，无法判定文书类型
    NoDocTypeCode,
    /// 类型码不在院内文书类型字典内
    DocTypeNotInRegistry,
    /// 无住院别名（identifier），跨件 join 不了
    NoEncounterAlias,
    /// date 非 ISO8601
    BadDate,
}

impl SkipReason {
    /// 给「已跳过」清单用的人话（不出现内部术语）。
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::NotADocument => "非文书正文（附件/索引类资源）",
            Self::NoDocTypeCode => "缺文书类型码，无法判定是哪类文书",
            Self::DocTypeNotInRegistry => "文书类型不在本院类型字典内",
            Self::NoEncounterAlias => "缺住院别名，无法归到某次住院",
            Self::BadDate => "文书日期写法认不出（应为 2026-03-02 08:30 这类）",
        }
    }
}

/// **文件级**跳过原因：连一份导出包都不是的文件。
///
/// 与 [`SkipReason`] 是两层：那条说的是「包**里面**的某一件为什么没进审查」，
/// 这条说的是「这个**文件**为什么没被当成导出包打开」。护士长拖进来一整个
/// 病区的文件夹，里面混着 `.pdf` 扫描件、`.xlsx` 台账、`.DS_Store`——
///
/// 这些必须逐个出现在「已跳过」清单里并给出原因。静默丢掉的话，
/// 护士长看到「审了 12 件、0 条问题」会以为全合规，而实际上有一份
/// 导出包压根没被打开过。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FileSkipReason {
    /// 扩展名不是 `.json` / `.ndjson`（pdf / xlsx / 图片 / 系统文件等）
    UnsupportedExtension,
    /// 扩展名对，但内容不是合法 JSON
    MalformedJson,
    /// 合法 JSON，但不是 FHIR `Bundle`（或 Bundle 没有 entry 数组）
    NotABundle,
    /// 读不到（权限 / 是目录 / 中途被删）
    Unreadable,
}

impl FileSkipReason {
    /// 给「已跳过」清单用的人话（不出现内部术语）。
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::UnsupportedExtension => "不是文书包文件（只认 .json / .ndjson 导出包）",
            Self::MalformedJson => "扩展名对但内容不是合法 JSON，文件可能没导完",
            Self::NotABundle => "是合法 JSON 但不是 FHIR 文书包（缺 resourceType=Bundle）",
            Self::Unreadable => "读不到这个文件（权限不足或已被移动）",
        }
    }

    /// 可重试？只有 `Unreadable` 值得：换一次权限或等文件同步完可能就好了。
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Unreadable)
    }
}

/// 一个被跳过的**文件**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSkip {
    pub path: String,
    /// 文件名（不含目录），清单里给用户看的那一列。
    pub name: String,
    pub reason: FileSkipReason,
    /// 相对扫描根目录的路径，跨机器看同一份审计导出时能对上。
    pub relative_path: String,
}

impl FileSkip {
    /// 给清单用的一行中文。
    pub fn as_zh(&self) -> String {
        format!("{} —— {}", self.relative_path, self.reason.as_zh())
    }
}

/// 一次递归扫描的完整结果：**能打开的包** + **打不开的文件**。
///
/// 两个数组加起来才是这次拖进来的全部文件。
/// `is_balanced()` 把这条不变量做成可查的：调用方（CLI / 桌面壳）
/// 能自己核对「看到的文件 = 打开的 + 跳过的」，漏记当场暴露。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DiscoveryReport {
    pub packages: Vec<(PathBuf, DiscoveredPackage)>,
    pub file_skips: Vec<FileSkip>,
    /// 递归走过的目录数（含根）。
    pub dirs_visited: usize,
    /// 走到的普通文件总数（打开的 + 跳过的）。
    pub files_seen: usize,
}

impl DiscoveryReport {
    /// 文件守恒：看到的文件 == 打开的包文件 + 跳过的文件。
    pub fn is_balanced(&self) -> bool {
        self.files_seen == self.packages.len() + self.file_skips.len()
    }

    /// 这次扫描一共解析出多少件（跨所有包）。
    pub fn items_len(&self) -> usize {
        self.packages.iter().map(|(_, pkg)| pkg.items.len()).sum()
    }

    /// 包内被过滤掉的 entry 合计。
    pub fn filtered_len(&self) -> usize {
        self.packages.iter().map(|(_, pkg)| pkg.filtered.len()).sum()
    }

    /// 只有跳过、没打开任何包时给一句人话（空文件夹 / 全是杂项）。
    pub fn empty_reason_zh(&self) -> Option<&'static str> {
        if !self.packages.is_empty() {
            return None;
        }
        if self.files_seen == 0 {
            Some("这个文件夹里没有任何文件——确认拖的是病区导出目录，而不是上级目录。")
        } else {
            Some("文件夹里的文件都不是文书包导出件（只认 .json / .ndjson）。")
        }
    }
}

/// 发现阶段的错误：**整包级**失败（JSON 不合法 / 不是 Bundle）才走这里。
/// 单件问题一律进 `FilteredEntry` 或 `errors`，不拖垮整包。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DiscoverError {
    #[error("{0}")]
    Parse(#[from] ParseError),
    #[error("E_IO: 读不到路径 {path} — {detail}")]
    Io { path: String, detail: String },
}

/// 递归发现导出包。返回 `(包, 源文件路径)`，按路径字典序排列以保证可复现。
///
/// 扩展名只认 `.json` / `.ndjson`；其余文件**不静默丢弃**——由
/// [`discover_dir_report`] 逐个记进 `file_skips` 并给中文原因。
///
/// 读文件走**字节**而不是 `read_to_string`：院内导出给的是 GBK 文件，
/// `read_to_string` 遇到非 UTF-8 字节会整包 `E_IO`，一个病区导出的
/// 几十份包里只要有一份是 GBK，整次批扫就都没了。字节路径由
/// [`parse_export`] 嗅探编码后逐处记账。
pub fn discover_dir(root: &Path) -> Result<Vec<(PathBuf, DiscoveredPackage)>, DiscoverError> {
    Ok(discover_dir_report(root)?.packages)
}

/// 递归扫描并**同时**记账打不开的文件（Task 32：拖放与批量队列）。
///
/// 与 [`discover_dir`] 的差别只有一处，但那一处是业务性的：
/// **打开不了的包不再让整次扫描失败**，而是进 `file_skips` 并给原因。
///
/// 护士长拖进来的是一整个病区文件夹，里面混着 pdf 扫描件、xlsx 台账，
/// 上一份没导完的 `.json`、还有 macOS 的 `.DS_Store`。旧写��里，
/// 一个坏 JSON 会让 `discover_dir` 整批返回 `Err` —— 一次手滑
/// 换来「整个病区都审不了」。现在：**能打开的照常进结果，坏件逐个记账**，
/// 与 Task 22「部分成功」在包级的同一条纪律。
///
/// `registry` 传 `None` 表示只做形状检查，不按院内文书类型字典过滤
/// （生产路径传字典，CLI / GUI 都用后者）。
pub fn discover_dir_report(
    root: &Path,
) -> Result<DiscoveryReport, DiscoverError> {
    discover_paths_report(&[root.to_path_buf()], None)
}

/// 同 [`discover_dir_report`]，但**显式带院内文书类型字典**做过滤。
///
/// 目录批扫与单份文件解析必须走同一条过滤链路（`parse_package_bytes_with`），
/// 否则「拖一个文件夹」与「点开一份文件」会对同一份包给出不同的跳过口径。
pub fn discover_dir_report_with_registry(
    root: &Path,
    registry: Option<&DocTypeRegistry>,
) -> Result<DiscoveryReport, DiscoverError> {
    discover_paths_report(&[root.to_path_buf()], registry)
}

/// 一次扫描接受**多个根**（护士长一次拖几个病区文件夹进来）。
///
/// 根之间可以是目录，也可以是单份导出文件；重复的路径按绝对化后的
/// 字典序去重，因此「同一文件夹拖两次」只审一遍。
pub fn discover_paths_report(
    roots: &[PathBuf],
    registry: Option<&DocTypeRegistry>,
) -> Result<DiscoveryReport, DiscoverError> {
    let mut report = DiscoveryReport::default();
    // 去重后的根（同一文件夹拖两次只算一次）。先分「目录根」与「文件根」：
    // 文件根绝不能进目录栈——`read_dir` 一个文件会报 `Not a directory`，
    // 拖单个 .json 进来就整批失败。
    let deduped: std::collections::BTreeSet<PathBuf> = roots
        .iter()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.clone()))
        .collect();
    // 相对路径必须相对**绝对化后**的根：`read_dir` 返回的是绝对路径，
    // 拿它去 strip 一个相对根（"fixtures"）永远失败，退化成绝对路径。
    // 「已跳过」清单里那一条会变成一长串机器绝对路径（…/README.md），
    // 护士长根本看不出跳过了什么。
    let abs_roots: Vec<PathBuf> = deduped.iter().cloned().collect();

    // 待走的目录栈。按字典序**逆序**入栈：弹出时才是升序，
    // 因此最终 `packages` 的顺序只取决于路径，不取决于 readdir 的返回序。
    let mut stack: Vec<PathBuf> = deduped.iter().filter(|p| p.is_dir()).cloned().rev().collect();

    // 文件根（拖进来的是单份 .json 而不是文件夹）：先收进包列表。
    //
    // 这些是**用户点名要审的文件**，不是「扫到一个文件夹里顺带看到的」，
    // 因此**不**计入 `files_seen`——底栏那句「扫了 N 个文件」说的是
    // 递归走过的范围，把一个主动拖进来的单文件算进去会虚高。
    for p in deduped.iter().filter(|p| !p.is_dir()) {
        // 路径压根不存在 = 调用方给错了（手输路径、导出目录被删）。
        // 这**不是**「打开失败」：把它降级成 file_skip 的话，界面会显示
        // 「这个文件夹里没有能认的文书包」——护士长于是以为文件夹是空的，
        // 而真实情况是路径打错了。路径错误必须原样上抛。
        if !p.exists() {
            return Err(DiscoverError::Io {
                path: p.display().to_string(),
                detail: "路径不存在（拼错了？或者导出目录已被移动/删除）".into(),
            });
        }
        match take_file(p, p, registry, &mut report) {
            Ok(Some(pkg)) => report.packages.push((p.clone(), pkg)),
            Ok(None) => {}
            Err(e) => return Err(e),
        }
    }

    while let Some(dir) = stack.pop() {
        report.dirs_visited += 1;
        let entries = std::fs::read_dir(&dir).map_err(|e| DiscoverError::Io {
            path: dir.display().to_string(),
            detail: e.to_string(),
        })?;
        // 先按路径名排一遍再处理：`read_dir` 的返回序在 ext4 / APFS / NTFS 上
        // 各不相同，不排的话同一份目录在两台机器上会给出不同的 `file_skips` 顺序，
        // 审计导出就没法 diff 了。
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in paths {
            if p.is_dir() {
                stack.insert(0, p);
                continue;
            }
            report.files_seen += 1;
            let rel = relative_to(&p, &abs_roots);
            match take_file(&p, &rel, registry, &mut report) {
                Ok(Some(pkg)) => report.packages.push((p, pkg)),
                Ok(None) => {}
                Err(e) => return Err(e),
            }
        }
    }

    report.packages.sort_by(|a, b| a.0.cmp(&b.0));
    report
        .file_skips
        .sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(report)
}

/// 打开一个文件：能打开就把包推进 `out`，打不开就记账进 `report.file_skips`。
///
/// 返回 `Ok(None)` = 已记账的跳过；`Err` 只留给「连读都没法读」以外的
/// 结构性问题——但即便是这些，本函数也**尽量**降级成记账而不是炸掉整批，
/// 只有根目录本身读不到才上抛（那是调用方的路径给错了）。
fn take_file(
    path: &Path,
    rel: &Path,
    registry: Option<&DocTypeRegistry>,
    report: &mut DiscoveryReport,
) -> Result<Option<DiscoveredPackage>, DiscoverError> {
    let name = path
        .file_name()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let relative_path = rel.to_string_lossy().into_owned();

    let mut skip = |reason: FileSkipReason| {
        report.file_skips.push(FileSkip {
            path: path.display().to_string(),
            name: name.clone(),
            reason,
            relative_path: relative_path.clone(),
        });
        Ok(None)
    };

    if !matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("json" | "ndjson")
    ) {
        return skip(FileSkipReason::UnsupportedExtension);
    }

    let bytes = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return skip(FileSkipReason::Unreadable),
    };

    // 形状检查放在解析之前：`parse_package_bytes` 遇到坏 JSON 会返回
    // `Err(DiscoverError::Parse)`，那正是「记一笔、别拖垮整批」的场景。
    // 这里先探一次，好把「不是合法 JSON」与「不是 Bundle」分成两个原因码，
    // 用户看到的清单才有指向（前者多半是没导完，后者多半是拖错了文件）。
    //
    // **必须先经 `decode_export_bytes` 解码再判形状**：直接
    // `serde_json::from_slice` 只吃 UTF-8，一份 GBK 导出（Task 22 覆盖的
    // 院内真实形态）会被误判成「不是合法 JSON」而整份跳过——
    // 那是 Task 22 刚修好的退化，不能在这里重新引入。
    let decoded = decode_export_bytes(&bytes);
    match serde_json::from_str::<serde_json::Value>(&decoded.text) {
        Ok(v) => {
            if v.get("resourceType").and_then(|x| x.as_str()) != Some("Bundle") {
                return skip(FileSkipReason::NotABundle);
            }
        }
        Err(_) => return skip(FileSkipReason::MalformedJson),
    }

    match parse_package_bytes_with(&bytes, registry) {
        Ok(pkg) => Ok(Some(pkg)),
        // 走到这里说明 JSON 合法且是 Bundle，但内部结构把解析器打倒了
        // （例如 entry 不是数组）。仍然降级成记账：一份坏包不该让
        // 整个病区审不了，理由由 `MalformedJson` 承担。
        Err(_) => skip(FileSkipReason::MalformedJson),
    }
}

/// 相对路径：优先相对**某一个**扫描根，跨机器看同一份清单时能对上。
fn relative_to(path: &Path, roots: &[PathBuf]) -> PathBuf {
    for root in roots {
        if let Ok(rel) = path.strip_prefix(root) {
            if !rel.as_os_str().is_empty() {
                return rel.to_path_buf();
            }
        }
    }
    path.to_path_buf()
}

/// 解析一份导出包，并把每件被跳过的 entry 记账。
/// `registry` 传 `None` 表示只做形状检查，不按字典过滤。
pub fn parse_package(json: &str) -> Result<DiscoveredPackage, DiscoverError> {
    parse_package_with(json, None)
}

/// 同上，但按院内文书类型字典过滤（生产路径用这个）。
pub fn parse_package_with(
    json: &str,
    registry: Option<&DocTypeRegistry>,
) -> Result<DiscoveredPackage, DiscoverError> {
    let ParsedBundle { items, errors, .. } = parse_bundle(json, registry)?;
    Ok(assemble_package(items, errors, json, SourceEncoding::Utf8))
}

/// 解析一份**字节流**形式的导出包（文件与目录发现路径用这个）。
///
/// 与 [`parse_package_with`] 的唯一差别是入口：先嗅探编码再走同一条链路。
/// 之后的行为逐字相同——不另造一套「容错解析」，否则 GUI 与 CLI 会对
/// 同一份脏包给出不同结论。
pub fn parse_package_bytes(bytes: &[u8]) -> Result<DiscoveredPackage, DiscoverError> {
    parse_package_bytes_with(bytes, None)
}

/// 同上，但按院内文书类型字典过滤（生产路径用这个）。
pub fn parse_package_bytes_with(
    bytes: &[u8],
    registry: Option<&DocTypeRegistry>,
) -> Result<DiscoveredPackage, DiscoverError> {
    let (parsed, decoded) = parse_export(bytes, registry)?;
    let ParsedBundle { items, errors, .. } = parsed;
    Ok(assemble_package(items, errors, &decoded.text, decoded.encoding))
}

/// 把 `parse_bundle` 的产物整理成带「已跳过」清单的 [`DiscoveredPackage`]。
fn assemble_package(
    items: Vec<DocItem>,
    errors: Vec<psychdoc_model::Diagnostic>,
    json: &str,
    encoding: SourceEncoding,
) -> DiscoveredPackage {
    let mut filtered = collect_filtered(json).unwrap_or_default();
    // parse_bundle 里单件被拒的（类型未知 / 缺别名 / 日期坏）已在 errors 中，
    // 这里把同一批原因再结构化一份，供「已跳过」清单直接渲染。
    for d in &errors {
        let t = d.message.text.as_str();
        // 非文书正文 entry 已由 collect_filtered 从原始 JSON 全量扫出
        // （它能看到真实 resourceType），这里不再重复登记，否则同一件会被数两次。
        let reason = if t.contains("E_DOC_TYPE_MISSING") {
            Some(SkipReason::NoDocTypeCode)
        } else if t.contains("E_UNKNOWN_DOC_TYPE") {
            Some(SkipReason::DocTypeNotInRegistry)
        } else if t.contains("E_ENCOUNTER_ALIAS_MISSING") {
            Some(SkipReason::NoEncounterAlias)
        } else if t.contains("E_DATE_PARSE") {
            Some(SkipReason::BadDate)
        } else {
            None
        };
        if let Some(reason) = reason {
            filtered.push(FilteredEntry {
                doc_id: d.loc.doc_id.clone(),
                resource_type: "Composition".into(),
                reason,
            });
        }
    }
    filtered.sort_by(|a, b| (&a.doc_id, a.reason.as_zh()).cmp(&(&b.doc_id, b.reason.as_zh())));
    filtered.dedup();
    DiscoveredPackage {
        items,
        errors,
        filtered,
        encoding,
    }
}

fn collect_filtered(json: &str) -> Result<Vec<FilteredEntry>, DiscoverError> {
    // 复用 parse_bundle 已产出的诊断即可：这里只补上「结构合法但被跳过」的
    // 资源类型记录。为了不重复实现一遍 Bundle 解析，直接看原始 JSON。
    let v: serde_json::Value = serde_json::from_str(json)
        .map_err(|e| DiscoverError::Parse(ParseError::BundleJson { detail: e.to_string() }))?;
    let entries = v.get("entry").and_then(|e| e.as_array()).cloned().unwrap_or_default();
    let mut out = Vec::new();
    for e in entries {
        let r = e.get("resource").cloned().unwrap_or(serde_json::Value::Null);
        let rt = r
            .get("resourceType")
            .and_then(|x| x.as_str())
            .unwrap_or("<无 resourceType>");
        if rt != "Composition" {
            out.push(FilteredEntry {
                doc_id: r
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or("<无 id>")
                    .to_string(),
                resource_type: rt.to_string(),
                reason: SkipReason::NotADocument,
            });
        }
    }
    Ok(out)
}

/// 按院内文书类型字典过滤，**并保留每件的过滤原因**。
/// 这是 `analyze_graph.rs:43` 的 `paths.retain(...)` 的产品侧改写：
/// 源侧静默剔除，本侧必须可核对。
pub fn filter_by_registry(
    items: Vec<DocItem>,
    registry: Option<&DocTypeRegistry>,
) -> (Vec<DocItem>, Vec<FilteredEntry>) {
    let Some(reg) = registry else {
        return (items, Vec::new());
    };
    let (kept, dropped): (Vec<DocItem>, Vec<DocItem>) = items
        .into_iter()
        .partition(|d| reg.is_known(&d.template_id));
    let filtered = dropped
        .into_iter()
        .map(|d| FilteredEntry {
            doc_id: d.doc_id,
            resource_type: "Composition".into(),
            reason: SkipReason::DocTypeNotInRegistry,
        })
        .collect();
    (kept, filtered)
}

/// 实体绑定结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingOutcome {
    /// 绑定唯一，可进入依赖图与规则执行
    pub confident: Vec<DocItem>,
    /// 绑定不唯一 / 未绑定 → 人工队列
    pub manual_queue: Vec<ManualQueueEntry>,
}

/// 人工队列一行：把「哪件、为什么没人敢自动处理」写成人能读的话。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManualQueueEntry {
    pub doc_id: String,
    pub template_id: String,
    pub encounter_alias: String,
    pub source_system: String,
    pub binding: Binding,
    pub reason: String,
}

/// 实体绑定器。**禁止按文件名或标题配对**——权威键只有
/// `template_id` + `Composition.id` + `identifier(system=encounter)`。
///
/// 判定规则：
///
/// 1. `template_id` 不在字典内 → `Unresolved`，进人工队列
/// 2. 同一 `(encounter_alias, template_id)` 下出现**两个及以上不同
///    `source_system`** 的件 → 全部标 `Ambiguous`，进人工队列
///    （跨系统同名文书：HIS 与护理系统各有一份「约束记录」，不知以谁为准）
/// 3. 其余 → `Confident`
#[derive(Debug, Clone, Default)]
pub struct EntityBinder<'a> {
    registry: Option<&'a DocTypeRegistry>,
}

impl<'a> EntityBinder<'a> {
    pub fn new(registry: Option<&'a DocTypeRegistry>) -> Self {
        Self { registry }
    }

    pub fn bind(&self, items: Vec<DocItem>) -> BindingOutcome {
        // 先按 (住院别名, 类型码) 聚类，跨系统同名在此暴露
        let mut groups: BTreeMap<(String, String), Vec<usize>> = BTreeMap::new();
        for (i, d) in items.iter().enumerate() {
            groups
                .entry((d.encounter_alias.clone(), d.template_id.clone()))
                .or_default()
                .push(i);
        }

        let mut out: Vec<DocItem> = items;
        let mut manual_queue = Vec::new();

        for ((alias, template), idxs) in groups {
            let known = self.registry.map(|r| r.is_known(&template)).unwrap_or(true);
            let mut distinct: Vec<String> = idxs
                .iter()
                .map(|i| out[*i].source_system.clone())
                .collect();
            distinct.sort();
            distinct.dedup();
            let cross_system = distinct.len() > 1;

            for i in idxs {
                let (binding, reason) = if !known {
                    (
                        Binding::Unresolved,
                        format!("类型码 {template} 不在本院文书类型字典内，需病案室确认归属"),
                    )
                } else if cross_system {
                    (
                        Binding::Ambiguous,
                        format!(
                            "同一住院下 {template} 有 {} 个系统各一份（{}），需人工确认以哪份为准",
                            distinct.len(),
                            distinct.join("、")
                        ),
                    )
                } else {
                    (Binding::Confident, String::new())
                };
                out[i].binding = binding;
                if !binding.is_confident() {
                    manual_queue.push(ManualQueueEntry {
                        doc_id: out[i].doc_id.clone(),
                        template_id: template.clone(),
                        encounter_alias: alias.clone(),
                        source_system: out[i].source_system.clone(),
                        binding,
                        reason,
                    });
                }
            }
        }

        manual_queue.sort_by(|a, b| (&a.encounter_alias, &a.doc_id).cmp(&(&b.encounter_alias, &b.doc_id)));
        let confident = out.into_iter().filter(|d| d.binding.is_confident()).collect();
        BindingOutcome { confident, manual_queue }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Span;

    const REG: &str = r#"
templates:
  restraint_consent:
    display: 约束知情同意
    fields:
      - key: 患方签字
        required: true
  restraint_record:
    display: 约束记录
    fields:
      - key: 约束起始时间
        required: true
"#;

    fn registry() -> DocTypeRegistry {
        serde_yaml::from_str(REG).unwrap()
    }

    fn doc(id: &str, tpl: &str, alias: &str, system: &str) -> DocItem {
        DocItem::new(id, tpl, alias)
            .with_source_system(system)
            .with_sections(vec![psychdoc_model::Section {
                index: 0,
                title: "段".into(),
                text: "内容".into(),
            }])
    }

    fn bundle(entries: &str) -> String {
        format!(r#"{{"resourceType":"Bundle","entry":[{entries}]}}"#)
    }

    fn comp(id: &str, code: &str, alias: &str, date: Option<&str>, extra: &str) -> String {
        let d = date.map(|d| format!("\"date\":\"{d}\",")).unwrap_or_default();
        format!(
            r#"{{"resource":{{"resourceType":"Composition","id":"{id}",{d}
            "identifier":[{{"system":"https://{alias}.example-hospital.invalid/fhir/identifier/encounter","value":"{alias}"}}],
            "type":{{"coding":[{{"code":"{code}"}}]}},"section":[{{"title":"段","text":"内容"}}]{extra}}}}}"#
        )
    }

    // ---- 发现 -------------------------------------------------------------

    #[test]
    fn discover_dir_finds_json_and_ignores_other_extensions() {
        let dir = std::env::temp_dir().join("psychdoc-discover-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ward-a")).unwrap();
        std::fs::write(
            dir.join("ward-a/a.json"),
            bundle(&comp("D-1", "restraint_consent", "WARD-A-001", Some("2026-03-02T08:30:00+08:00"), "")),
        )
        .unwrap();
        std::fs::write(dir.join("ward-a/notes.txt"), "非导出包").unwrap();

        let found = discover_dir(&dir).unwrap();
        assert_eq!(found.len(), 1, "只认 .json/.ndjson，txt 不进发现结果");
        assert_eq!(found[0].0.file_name().unwrap(), "a.json");
        assert_eq!(found[0].1.items.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn discover_dir_is_deterministic_across_runs() {
        let dir = std::env::temp_dir().join("psychdoc-discover-order");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for id in ["c", "a", "b"] {
            std::fs::write(
                dir.join(format!("{id}.json")),
                bundle(&comp(&format!("D-{id}"), "restraint_consent", "WARD-A-001", Some("2026-03-02T08:30:00+08:00"), "")),
            )
            .unwrap();
        }
        let first: Vec<_> = discover_dir(&dir).unwrap().into_iter().map(|(p, _)| p).collect();
        let second: Vec<_> = discover_dir(&dir).unwrap().into_iter().map(|(p, _)| p).collect();
        assert_eq!(first, second, "同一目录两次发现顺序必须一致，否则批扫结果不可复现");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 类型过滤 ---------------------------------------------------------

    #[test]
    fn filtered_entries_carry_a_readable_reason_not_silent_skip() {
        let json = bundle(&format!(
            "{},{}",
            comp("DR-1", "x", "WARD-A-001", None, "").replace("\"resourceType\":\"Composition\"", "\"resourceType\":\"DocumentReference\""),
            comp("D-1", "restraint_consent", "WARD-A-001", Some("2026-03-02T08:30:00+08:00"), "")
        ));
        let pkg = parse_package(&json).unwrap();
        assert_eq!(pkg.items.len(), 1);
        assert_eq!(pkg.filtered.len(), 1);
        assert_eq!(pkg.filtered[0].doc_id, "DR-1");
        assert_eq!(pkg.filtered[0].reason, SkipReason::NotADocument);
        assert!(pkg.filtered[0].reason.as_zh().contains("非文书正文"));
    }

    #[test]
    fn unknown_doc_type_is_filtered_with_reason() {
        let json = bundle(&comp("D-9", "mystery_form", "WARD-A-001", Some("2026-03-02T08:30:00+08:00"), ""));
        let pkg = parse_package_with(&json, Some(&registry())).unwrap();
        assert!(pkg.items.is_empty());
        assert!(pkg.filtered.iter().any(|f| f.reason == SkipReason::DocTypeNotInRegistry));
    }

    #[test]
    fn registry_filter_partitions_and_explains_drops() {
        let kept = doc("D-1", "restraint_consent", "WARD-A-001", "his");
        let dropped = doc("D-2", "unknown_form", "WARD-A-001", "his");
        let (kept, filtered) = filter_by_registry(vec![kept, dropped], Some(&registry()));
        assert_eq!(kept.len(), 1);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].reason, SkipReason::DocTypeNotInRegistry);
    }

    // ---- 实体绑定 ---------------------------------------------------------

    #[test]
    fn cross_system_same_name_doc_is_ambiguous_not_silently_paired() {
        // 同一住院、同一类型码，HIS 与护理系统各导一份 → 不知以谁为准
        let items = vec![
            doc("D-HIS-1", "restraint_record", "WARD-A-002", "his"),
            doc("D-NURSE-1", "restraint_record", "WARD-A-002", "nursing"),
        ];
        let out = EntityBinder::new(Some(&registry())).bind(items);
        assert!(out.confident.is_empty(), "跨系统同名不得被配成 confident");
        assert_eq!(out.manual_queue.len(), 2);
        assert!(out.manual_queue.iter().all(|e| e.binding == Binding::Ambiguous));
        assert!(
            out.manual_queue[0].reason.contains("his") && out.manual_queue[0].reason.contains("nursing"),
            "人工队列要写清是哪几个系统撞了：{}",
            out.manual_queue[0].reason
        );
    }

    #[test]
    fn same_system_multiple_docs_stay_confident() {
        // 同一系统下同类型多件是正常的（多次约束），不是歧义
        let items = vec![
            doc("D-1", "restraint_record", "WARD-A-003", "his"),
            doc("D-2", "restraint_record", "WARD-A-003", "his"),
        ];
        let out = EntityBinder::new(Some(&registry())).bind(items);
        assert_eq!(out.confident.len(), 2);
        assert!(out.manual_queue.is_empty());
    }

    #[test]
    fn unknown_type_goes_unresolved_to_manual_queue() {
        let items = vec![doc("D-1", "not_in_dict", "WARD-A-004", "his")];
        let out = EntityBinder::new(Some(&registry())).bind(items);
        assert!(out.confident.is_empty());
        assert_eq!(out.manual_queue[0].binding, Binding::Unresolved);
    }

    #[test]
    fn ambiguity_is_scoped_to_same_encounter() {
        // 两个不同住院各有一份同名文书 → 互不影响
        let items = vec![
            doc("D-1", "restraint_record", "WARD-A-005", "his"),
            doc("D-2", "restraint_record", "WARD-A-006", "nursing"),
        ];
        let out = EntityBinder::new(Some(&registry())).bind(items);
        assert_eq!(out.confident.len(), 2, "不同住院不应互相触发歧义");
    }

    #[test]
    fn manual_queue_is_sorted_for_reproducible_export() {
        let items = vec![
            doc("D-2", "not_in_dict", "WARD-A-009", "his"),
            doc("D-1", "not_in_dict", "WARD-A-008", "his"),
        ];
        let out = EntityBinder::new(Some(&registry())).bind(items);
        let aliases: Vec<_> = out.manual_queue.iter().map(|e| &e.encounter_alias).collect();
        assert_eq!(aliases, vec!["WARD-A-008", "WARD-A-009"]);
    }

    #[test]
    fn binder_does_not_depend_on_file_order() {
        let a = doc("D-HIS-1", "restraint_record", "WARD-A-002", "his");
        let b = doc("D-NURSE-1", "restraint_record", "WARD-A-002", "nursing");
        let r1 = EntityBinder::new(Some(&registry())).bind(vec![a.clone(), b.clone()]);
        let r2 = EntityBinder::new(Some(&registry())).bind(vec![b, a]);
        assert_eq!(
            r1.manual_queue.iter().map(|e| &e.doc_id).collect::<Vec<_>>(),
            r2.manual_queue.iter().map(|e| &e.doc_id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn filtered_entry_zh_text_has_no_internal_jargon() {
        for r in [
            SkipReason::NotADocument,
            SkipReason::NoDocTypeCode,
            SkipReason::DocTypeNotInRegistry,
            SkipReason::NoEncounterAlias,
            SkipReason::BadDate,
        ] {
            let t = r.as_zh();
            assert!(!t.contains("resourceType") && !t.contains("ISO8601") && !t.contains("identifier"));
        }
    }

    #[test]
    fn doc_without_section_still_binds_confidently() {
        let d = DocItem::new("D-1", "restraint_consent", "WARD-A-007")
            .with_source_system("his")
            .with_sections(vec![]);
        let out = EntityBinder::new(Some(&registry())).bind(vec![d]);
        assert_eq!(out.confident.len(), 1);
        let _ = Span::unchecked(0, 0); // 位置模型由 model 负责，此处不重复断言
    }
}
