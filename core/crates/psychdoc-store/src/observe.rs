//! **结构化运行日志与运行摘要**：一次审查发生了什么，事后可复现。
//!
//! 院现场排障靠日志，不靠猜。信息科接到一句「今天早上护士长说少报了一条」，
//! 要能在几分钟内回答三个问题：
//!
//! 1. **那次跑批到底看了什么**（几件、哪几个住院、重跑还是走缓存）；
//! 2. **为什么是那个结论**（命中码、段落定位、规则集版本、豁免与未判定）；
//! 3. **中途有没有出问题**（哪个环节、什么码、可不可以重试）。
//!
//! 人类可读的 `psychdoc --log-file` 只回答第 3 个问题的一部分——它是一段
//! 自由文本，`grep` 得动但 `jq` 解析不了，也没法回答「那批里有几条阻断」。
//! 因此这里另起一份 **JSON 行**日志：字段固定、一行一事件、可被机器消费。
//!
//! # 字段为什么是这七个
//!
//! `ts / level / event / encounter_alias / doc_id / code / detail`
//! ——不多不少。多一个字段就要有人填，字段一多就有人瞎填；
//! 少一个字段就会出现「这条查不出来」，而查不出来就等于没记。
//!
//! # 二次脱敏不是重复劳动
//!
//! 落库时 [`crate::audit`] 已经扫过一遍；写日志时**再扫一遍**，因为日志是
//! 唯一会被整段 `cat` 出来贴进企业微信群的东西。上游多接一个字段
//! （比如把 `detail` 换成含姓名的自由文本）就可能把患者信息带走。
//! 命中即替换并计数——`redacted_hits` 挂在运行摘要里，
//! 「这份导出里有没有过未脱敏字段」因此是有凭据的回答，不是口头承诺。
//!
//! # 日志落在哪
//!
//! [`crate::paths::data_dir`] 下的 `logs/`，**按日切分**（`psychdoc-2026-03-02.log`）。
//! 按日切而不是覆盖写：常驻工具一天可能跑几十批，混在一个文件里
//! 「昨天那条是不是同一次运行」就答不上来了。

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::audit::desensitize;

/// 日志文件名里的日期部分。调用方注入，测试可固定。
pub fn log_file_name(day: &str) -> String {
    format!("psychdoc-{day}.log")
}

/// 日志目录（用户数据目录下的 `logs/`）。
pub fn log_dir() -> crate::error::Result<PathBuf> {
    Ok(crate::paths::data_dir()?.join("logs"))
}

/// 级别。刻意只三档：再细分（trace/debug）在本产品里没人看，
/// 而多一档就多一处「这条到底算不算异常」的争论。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Info,
    Warn,
    Error,
}

impl Level {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Info => "常规",
            Self::Warn => "注意",
            Self::Error => "出错",
        }
    }
}

/// 一条结构化日志事件。
///
/// 字段刻意**全部可选**：缺 `doc_id` 的事件（跑批开始 / 规则集载入）
/// 写 `null` 而不是空串——空串在 `jq` 里会被当成「有一个空的 doc_id」，
/// 而「这条根本没有件」和「这件的 id 是空的」是两回事。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Event {
    /// RFC 3339 时刻。
    pub ts: String,
    pub level: Level,
    /// 事件名。用固定词表而不是自由字符串，避免日志里出现拼错的同义词。
    pub event: String,
    /// 住院别名（脱敏形态，如 `WARD-A-001`）。写盘前再脱敏一次。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encounter_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doc_id: Option<String>,
    /// 规则码 `QC-PSY-nn` 或稳定错误码 `E_XXX`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// 本次耗时（毫秒）。跨文书 / 批扫这类量级的差别全在这一栏。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    /// 补充字段。**值一律是字符串**且必须已脱敏——
    /// 让调用方塞进结构体是最容易把患者信息带出去的口子。
    #[serde(skip_serializing_if = "BTreeMap::is_empty", default)]
    pub detail: BTreeMap<String, String>,
}

/// 构造一条事件。`ts` 由调用方传入而不是内部取时钟：
/// 断言日志内容时必须能钉住时刻，否则同一份夹具两次跑出来的日志无法逐字节比对。
pub fn event(ts: impl Into<String>, level: Level, name: &str) -> Event {
    Event {
        ts: ts.into(),
        level,
        event: name.to_string(),
        encounter_alias: None,
        doc_id: None,
        code: None,
        elapsed_ms: None,
        detail: BTreeMap::new(),
    }
}

impl Event {
    pub fn encounter(mut self, alias: impl Into<String>) -> Self {
        self.encounter_alias = Some(alias.into());
        self
    }
    pub fn doc(mut self, doc_id: impl Into<String>) -> Self {
        self.doc_id = Some(doc_id.into());
        self
    }
    pub fn code(mut self, code: impl Into<String>) -> Self {
        self.code = Some(code.into());
        self
    }
    pub fn elapsed_ms(mut self, ms: u64) -> Self {
        self.elapsed_ms = Some(ms);
        self
    }
    pub fn detail(mut self, key: &str, value: impl Into<String>) -> Self {
        self.detail.insert(key.to_string(), value.into());
        self
    }

    /// 写盘前的**二次脱敏**。返回替换次数。
    ///
    /// 住院别名与件 id 按脱敏别名原样保留（它们本来就是 `WARD-A-001` 这类
    /// 形态）；真正要拦的是 `detail` 里被上游塞进来的自由文本——
    /// 那才是患者信息漏得最多的地方。
    pub fn redact_in_place(&mut self) -> usize {
        let mut hits = 0usize;
        for v in self.detail.values_mut() {
            let (out, n) = desensitize(v);
            *v = out;
            hits += n;
        }
        hits
    }

    /// 一行 JSON。字段顺序由结构体定义固定，便于 `diff` 两次运行的日志。
    pub fn to_json_line(&self) -> String {
        serde_json::to_string(self).expect("事件字段都是可序列化的简单类型")
    }
}

/// 结构化日志器：按日切分写 JSON 行。
///
/// 写日志失败**不**让主流程失败：审查结论已经算出来了，
/// 因为日志盘满了就把整次审查丢掉是不可接受的反向失败。
#[derive(Debug, Clone)]
pub struct RunLogger {
    dir: PathBuf,
    /// 当前打开的文件与其日期。日期变了才换文件。
    open: BTreeMap<String, String>,
    written: usize,
    redacted_hits: usize,
}

impl RunLogger {
    /// 写进指定目录。目录不存在时一并建出来。
    pub fn new(dir: impl Into<PathBuf>) -> std::io::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        Ok(Self {
            dir,
            open: BTreeMap::new(),
            written: 0,
            redacted_hits: 0,
        })
    }

    /// 默认目录（用户数据目录下的 `logs/`）。定不出目录时返回 `None`
    /// ——不因为写不了日志就让整个程序起不来。
    pub fn default_dir() -> Option<Self> {
        let dir = log_dir().ok()?;
        Self::new(dir).ok()
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// 已写行数。
    pub fn written(&self) -> usize {
        self.written
    }

    /// 累计脱敏替换次数（`redacted_hits` 的来源）。
    pub fn redacted_hits(&self) -> usize {
        self.redacted_hits
    }

    /// 写一条事件。返回是否真的落盘。
    pub fn write(&mut self, mut ev: Event) -> bool {
        self.redacted_hits += ev.redact_in_place();
        let day = ev.ts.split('T').next().unwrap_or("unknown").to_string();
        let path = self.dir.join(log_file_name(&day));
        let line = ev.to_json_line();
        let ok = append_line(&path, &line);
        if ok {
            self.open.insert(day.clone(), log_file_name(&day));
            self.written += 1;
        }
        ok
    }

    /// 某一天写过哪些文件（供测试断言「按日切分真的切了」）。
    pub fn files_written(&self) -> Vec<String> {
        self.open.values().cloned().collect()
    }
}

fn append_line(path: &Path, line: &str) -> bool {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    match std::fs::OpenOptions::new().create(true).append(true).open(path) {
        Ok(mut f) => writeln!(f, "{line}").is_ok() && f.flush().is_ok(),
        Err(_) => false,
    }
}

/// 一次审查的**运行摘要**。
///
/// 与 [`psychdoc_rules::RunSummary`]（增量缓存那一层）不同：那份回答
/// 「这次重算了几件」，这份回答「这次审查发生了什么」——给排障与交办看的。
///
/// **账要平**是这里唯一的不变量，且**按件算**：
/// `已审 + 未解析 = 输入件数`。
/// 少了它，「0 条阻断」就会被读成「全部合规」，
/// 而真相可能是一半的件根本没解析出来。
///
/// 注意不变量不是拿诊断条数去减：一件可以带三条诊断，拿条数算账会把
/// 一个报了三条的住院算成负数；而「零诊断」本身也是结论（全合规），
/// 同样不该被当成漏网。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    /// 传进来的件总数。
    pub items_total: usize,
    /// 真正走过规则层的件数（含缓存命中的）。
    pub items_reviewed: usize,
    /// 报出的诊断条数。
    pub findings: usize,
    /// 其中阻断提交（severity=Blocking）的条数。退出码 1 看的就是它。
    pub blocking: usize,
    /// 因豁免而未上报的条数。
    pub waived: usize,
    /// 判不出来因而既没报也没放行的条数（缺时刻、绑定不唯一、精度不足）。
    pub undecided: usize,
    /// 未解析 / 被类型字典过滤掉的件数。
    ///
    /// **只算「压根没进规则层」的件**。解析期问题（`E_ENCODING`、缺 section 等）
    /// 记在 [`Self::parse_issues`] 而不是这里：那些件本身已经被读进来并判过，
    /// 再往 `unresolved` 里加一遍会让「已审 + 未解析」大于输入件数，
    /// 账就假平了——而账假平正是这条不变量要防的那种失败。
    pub unresolved: usize,
    /// 解析期问题条数（编码、结构、重复 id 等）。与规则诊断分开记账。
    pub parse_issues: usize,
    /// 本次审查的规则集版本与短哈希——事后回溯「当年按哪版审的」的锚。
    pub ruleset_version: String,
    pub ruleset_short_sha256: String,
    /// 总耗时（毫秒）。
    pub elapsed_ms: u64,
    /// 日志脱敏替换次数。**0 不是默认值，是断言出来的。**
    pub redacted_hits: usize,
    /// 本次日志落在哪个文件。写盘失败时为空串（不静默假装落了）。
    pub log_file: String,
}

impl RunSummary {
    pub fn new(items_total: usize, ruleset_version: &str, ruleset_short_sha256: &str) -> Self {
        Self {
            items_total,
            items_reviewed: 0,
            findings: 0,
            blocking: 0,
            waived: 0,
            undecided: 0,
            unresolved: 0,
            parse_issues: 0,
            ruleset_version: ruleset_version.to_string(),
            ruleset_short_sha256: ruleset_short_sha256.to_string(),
            elapsed_ms: 0,
            redacted_hits: 0,
            log_file: String::new(),
        }
    }

    /// 账平：进来的每一件都有明确去向——被审过，或被显式记成未解析。
    ///
    /// **按件算，不按诊断条数算**。一件可以带三条诊断，若拿 `findings`
    /// 去减 `items_reviewed`，一个报了三条的住院就会把账算成负数；
    /// 而「零诊断」本身也是结论（全合规），同样不该被当成漏网。
    pub fn is_balanced(&self) -> bool {
        self.items_reviewed + self.unresolved == self.items_total
    }

    /// 「有件从头到尾没被判过」——最该被追问的那一类。
    ///
    /// 既没进规则层，也没进未解析清单：它既没有被审，也不曾被记为读不动。
    /// 数字非零就说明有一批件在系统里消失了，而这正是「静默跳过」的定义。
    pub fn silently_unaccounted(&self) -> usize {
        self.items_total
            .saturating_sub(self.items_reviewed)
            .saturating_sub(self.unresolved)
    }

    pub fn as_zh(&self) -> String {
        format!(
            "输入 {} 件 · 已审 {} 件 · 报出 {} 条（阻断 {}）· 豁免 {} 条 · 未判定 {} 条 · 未解析 {} 件 · 解析期问题 {} 条 · 规则集 v{}（{}）· 耗时 {} ms",
            self.items_total,
            self.items_reviewed,
            self.findings,
            self.blocking,
            self.waived,
            self.undecided,
            self.unresolved,
            self.parse_issues,
            self.ruleset_version,
            self.ruleset_short_sha256,
            self.elapsed_ms,
        )
    }

    /// 追加到指定日志文件（不建日志器也能记一条摘要）。
    pub fn append_to(&mut self, dir: &Path, ts: &str) -> bool {
        let mut log = match RunLogger::new(dir) {
            Ok(l) => l,
            Err(_) => return false,
        };
        let path = dir.join(log_file_name(ts.split('T').next().unwrap_or("unknown")));
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let ok = log.write(
            event(ts, Level::Info, "run.summary")
                .detail("summary", self.as_zh())
                .detail("items_total", self.items_total.to_string())
                .detail("findings", self.findings.to_string())
                .detail("blocking", self.blocking.to_string()),
        );
        if ok {
            self.redacted_hits += log.redacted_hits();
            self.log_file = name;
        }
        ok
    }
}

/// 错误码表导出为 markdown 片段。README「错误与降级」一节直接引这段。
///
/// 刻意做成**函数**而不是一份手抄的 `docs/error-codes.md`：
/// 手抄的那份一旦代码里加了码就会悄悄过期，而过期的错误码表比没有更糟——
/// 值班的人会照着一张缺了三行的表判断「这个码不在我们这儿」。
pub fn error_codes_markdown() -> String {
    psychdoc_model::errcodes::markdown_table()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_event_is_one_json_line_with_fixed_fields() {
        let ev = event("2026-03-02T09:00:00+08:00", Level::Warn, "diagnostic")
            .encounter("WARD-A-001")
            .doc("WARD-A-001-CONSENT-01")
            .code("QC-PSY-01")
            .elapsed_ms(12)
            .detail("段落", "第 1 件第 2 段");
        let line = ev.to_json_line();
        assert!(!line.contains('\n'), "事件必须是一行");
        let v: serde_json::Value = serde_json::from_str(&line).expect("必须是合法 JSON");
        assert_eq!(v["ts"], "2026-03-02T09:00:00+08:00");
        assert_eq!(v["level"], "warn");
        assert_eq!(v["event"], "diagnostic");
        assert_eq!(v["encounter_alias"], "WARD-A-001");
        assert_eq!(v["code"], "QC-PSY-01");
        assert_eq!(v["elapsed_ms"], 12);
        assert_eq!(v["detail"]["段落"], "第 1 件第 2 段");
    }

    #[test]
    fn absent_fields_are_omitted_rather_than_written_as_empty_strings() {
        let line = event("2026-03-02T09:00:00+08:00", Level::Info, "run.start").to_json_line();
        let v: serde_json::Value = serde_json::from_str(&line).unwrap();
        // 「这条没有件」与「这件的 id 是空串」在 jq 里必须分得开。
        assert!(v.get("doc_id").is_none());
        assert!(v.get("code").is_none());
        assert!(v.get("detail").is_none());
    }

    #[test]
    fn the_same_input_produces_byte_identical_lines() {
        let mk = || {
            event("2026-03-02T09:00:00+08:00", Level::Info, "run.summary")
                .encounter("WARD-A-001")
                .detail("命中", "3")
        };
        assert_eq!(mk().to_json_line(), mk().to_json_line());
    }

    #[test]
    fn detail_is_redacted_a_second_time_before_it_hits_the_disk() {
        let mut ev = event("2026-03-02T09:00:00+08:00", Level::Info, "run.summary")
            .detail("原文", "住院号 12345678 家属：张丽娟代签");
        let hits = ev.redact_in_place();
        assert_eq!(hits, 2, "住院号与姓名各应计一次");
        let text = ev.detail.get("原文").unwrap();
        assert!(!text.contains("12345678"));
        assert!(!text.contains("张丽娟"));
        // 标签留着：审核员要看得懂这条说的是患者还是家属。
        assert!(text.contains("家属："));
    }

    #[test]
    fn logs_are_split_by_day_not_appended_into_one_file() {
        let dir = std::env::temp_dir().join(format!(
            "psychdoc-observe-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let mut log = RunLogger::new(&dir).expect("建得出日志目录");

        log.write(event("2026-03-02T09:00:00+08:00", Level::Info, "run.start"));
        log.write(event("2026-03-02T23:59:00+08:00", Level::Info, "run.end"));
        // 跨零点：同一次常驻进程的第二批落到另一个文件。
        log.write(event("2026-03-03T00:05:00+08:00", Level::Info, "run.start"));

        assert_eq!(log.written(), 3);
        assert!(dir.join("psychdoc-2026-03-02.log").exists());
        assert!(dir.join("psychdoc-2026-03-03.log").exists());
        let files = log.files_written();
        assert_eq!(files.len(), 2, "应按两个日期切分：{files:?}");

        // 第二天那份里不该出现第一天的事件——否则「昨天那次跑了什么」答不上来。
        let d3 = std::fs::read_to_string(dir.join("psychdoc-2026-03-03.log")).unwrap();
        assert_eq!(d3.lines().count(), 1);
        assert!(!d3.contains("23:59:00"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_summary_reports_every_count_it_promises() {
        let s = RunSummary {
            items_total: 10,
            items_reviewed: 8,
            findings: 5,
            blocking: 3,
            waived: 1,
            undecided: 2,
            unresolved: 2,
            parse_issues: 4,
            ruleset_version: "0.5.0".into(),
            ruleset_short_sha256: "e4eda0c79c95".into(),
            elapsed_ms: 87,
            redacted_hits: 0,
            log_file: "psychdoc-2026-03-02.log".into(),
        };
        let zh = s.as_zh();
        for frag in ["输入 10 件", "已审 8 件", "报出 5 条", "阻断 3", "豁免 1", "未判定 2", "未解析 2 件", "解析期问题 4 条", "e4eda0c79c95", "87 ms"] {
            assert!(zh.contains(frag), "摘要缺「{frag}」：{zh}");
        }
        // 可核对：摘要本身要能被机器读。
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(v["items_total"], 10);
        assert_eq!(v["ruleset_short_sha256"], "e4eda0c79c95");
    }

    #[test]
    fn an_item_that_was_never_judged_is_visible_not_smoothed_over() {
        // 10 件进来、只审了 8 件、剩下 2 件既不在规则层也没记成未解析：
        // 它们既没有被审，也不曾被记为读不动——这正是「静默跳过」的定义。
        let s = RunSummary {
            items_total: 10,
            items_reviewed: 8,
            unresolved: 0,
            findings: 0,
            ..RunSummary::new(10, "0.5.0", "abc")
        };
        assert_eq!(s.silently_unaccounted(), 2);
        assert!(!s.is_balanced());
        // 「0 条」本身不是结论——它必须能被人追问。
        assert!(s.as_zh().contains("已审 8 件"));

        // 把那 2 件显式记成未解析之后账就平了：去向写下来才算数。
        let mut fixed = s.clone();
        fixed.unresolved = 2;
        assert!(fixed.is_balanced());
        assert_eq!(fixed.silently_unaccounted(), 0);
    }

    #[test]
    fn a_single_item_with_three_findings_does_not_break_the_accounting() {
        // 不变量按件算：一件带三条诊断不该把账算成负数。
        let s = RunSummary {
            items_total: 4,
            items_reviewed: 4,
            findings: 3,
            blocking: 3,
            ..RunSummary::new(4, "0.5.0", "abc")
        };
        assert!(s.is_balanced());
        assert_eq!(s.silently_unaccounted(), 0);
    }

    #[test]
    fn a_summary_append_records_the_redaction_count_and_the_file_name() {
        let dir = std::env::temp_dir().join(format!("psychdoc-observe-sum-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let mut s = RunSummary::new(3, "0.5.0", "e4eda0c79c95");
        s.findings = 1;
        s.blocking = 1;
        assert!(s.append_to(&dir, "2026-03-02T09:00:00+08:00"));
        assert_eq!(s.log_file, "psychdoc-2026-03-02.log");
        assert_eq!(s.redacted_hits, 0, "摘要里没有患者信息，应为 0 次替换");

        let body = std::fs::read_to_string(dir.join("psychdoc-2026-03-02.log")).unwrap();
        let v: serde_json::Value = serde_json::from_str(body.trim()).expect("摘要行须是合法 JSON");
        assert_eq!(v["event"], "run.summary");
        assert!(v["detail"]["summary"].as_str().unwrap().contains("输入 3 件"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_balance_survives_parse_issues_being_counted_separately() {
        // 解析期问题挂在**已经审过的件**上。若把它们也算进 unresolved，
        // 「已审 + 未解析」就会大于输入件数——账假平。
        let s = RunSummary {
            items_total: 59,
            items_reviewed: 53,
            unresolved: 6,
            parse_issues: 22,
            findings: 32,
            blocking: 32,
            ..RunSummary::new(59, "0.6.0", "2abfac00aa5b")
        };
        assert!(s.is_balanced(), "53 已审 + 6 未解析 应等于 59 件");
        assert_eq!(s.silently_unaccounted(), 0);
        // 解析期问题仍要被看得见——它只是不参与「件」的账。
        assert!(s.as_zh().contains("解析期问题 22 条"));
    }

    #[test]
    fn the_error_code_markdown_names_at_least_ten_codes() {
        let md = error_codes_markdown();
        let rows = md.lines().filter(|l| l.starts_with("| `E_")).count();
        assert!(rows >= 10, "错误码表只导出了 {rows} 行");
        assert!(md.contains("处置建议"));
        assert!(md.contains("可重试"));
    }
}
