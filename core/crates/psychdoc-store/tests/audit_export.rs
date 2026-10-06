//! Task 31 验收：审计导出包（csv / json）+ 版本回溯 + 二次脱敏。
//!
//! 六组断言对应 task.json 的六个 step：
//!
//! 1. 导出行 schema 齐全（批次 / 别名 / 件 id / 码 / 豁免 / 严重度 / sha256 /
//!    推进人 / 时刻 / 幂等键）；
//! 2. csv 与 json 两种形态，csv 带 UTF-8 BOM；
//! 3. 同一次审查的多条诊断打同一 `review_run_id`，可按次聚合；
//! 4. 二次脱敏：标识列命中 → 拒绝导出（`E_EXPORT_LEAK`）；自由文本列 → 就地替换；
//! 5. 版本回溯查询入口：住院别名 + 时间点 → 当时 sha256 与命中码集合；
//! 6. 生僻字 / 换行字段在 csv 里正确转义，导出行数 == 诊断数。

use psychdoc_model::{Diagnostic, DocItem, DocLoc, FixAvailability, RuleMessage, Severity, Span};
use psychdoc_store::export::{csv_escape, to_csv, to_json};
use psychdoc_store::{
    ExportQuery, ReviewRunInput, Store, StoreError, WritebackEntry, AUDIT_UTF8_BOM,
};

const ALIAS: &str = "WARD-A-001";
const OTHER_ALIAS: &str = "WARD-B-002";
const V1: &str = "e4eda0c79c95aa11bb22cc33dd44ee9911a1b2c3d4e5f60718293a4b5c6d7e8f";
const V2: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9001122334455667788990aabbccddeeff0";

fn item(doc_id: &str) -> DocItem {
    let mut it = DocItem::new(doc_id, "CONSENT_FORM", ALIAS);
    it.source_system = "emr".into();
    it
}

fn diag(doc_id: &str, code: &str, section: usize, message: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        loc: DocLoc::new(doc_id, section as u32, Span::new(9, 18).unwrap()),
        message: RuleMessage::new(
            message.to_string(),
            "《精神科住院护理常规》第 4.2 条".to_string(),
        )
        .unwrap(),
        fix_availability: FixAvailability::None,
        severity: Severity::Blocking,
        fix: None,
        suppressed_by: None,
        waived: None,
    }
}

fn review<'a>(
    run_id: &'a str,
    sha: &'a str,
    started_at: &'a str,
    actor: &'a str,
    diags: &'a [Diagnostic],
) -> ReviewRunInput<'a> {
    review_with(run_id, sha, started_at, actor, diags, &[], &[])
}

/// 与 [`review`] 相同，但带上「被压下的那组」与规则来源层。
fn review_with<'a>(
    run_id: &'a str,
    sha: &'a str,
    started_at: &'a str,
    actor: &'a str,
    diags: &'a [Diagnostic],
    suppressed: &'a [Diagnostic],
    rule_sources: &'a [(String, String)],
) -> ReviewRunInput<'a> {
    ReviewRunInput {
        run_id,
        encounter_alias: ALIAS,
        ruleset_version: "0.6.0",
        ruleset_sha256: sha,
        actor,
        started_at,
        finished_at: Some(started_at),
        items_total: 6,
        items_reviewed: 6,
        items_skipped: 0,
        unresolved_count: 0,
        waived_count: 0,
        diagnostics: diags,
        suppressed,
        rule_sources,
    }
}

/// 落一个「三次审查」的库：run-001 两条诊断（V1）、run-002 一条（V2），
/// 外加 run-003 零命中（合规包也要留痕，否则「没报」无从追问）。
fn seeded() -> Store {
    let store = Store::open_in_memory().expect("开内存库");
    let items = vec![
        item("WARD-A-001-CONSENT-01"),
        item("WARD-A-001-RECORD-01"),
        item("WARD-A-001-ASSESS-01"),
    ];
    store
        .register_items(&items, "2026-03-02T09:00:00+08:00")
        .expect("登记件");

    let d1 = vec![
        diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "知情同意书患方签字未填写"),
        diag("WARD-A-001-RECORD-01", "QC-PSY-07", 0, "约束解除记录晚于约束开始记录"),
    ];
    store
        .record_review(&review(
            "run-001",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &d1,
        ))
        .expect("落 run-001");

    let d2 = vec![diag(
        "WARD-A-001-ASSESS-01",
        "QC-PSY-08",
        2,
        "评估量表缺少第二签名",
    )];
    store
        .record_review(&review(
            "run-002",
            V2,
            "2026-03-05T09:00:00+08:00",
            "文书专员 李静",
            &d2,
        ))
        .expect("落 run-002");

    store
        .record_review(&ReviewRunInput {
            run_id: "run-003",
            encounter_alias: ALIAS,
            ruleset_version: "0.6.0",
            ruleset_sha256: V2,
            actor: "文书专员 李静",
            started_at: "2026-03-06T09:00:00+08:00",
            finished_at: Some("2026-03-06T09:05:00+08:00"),
            items_total: 3,
            items_reviewed: 3,
            items_skipped: 0,
            unresolved_count: 0,
            waived_count: 0,
            diagnostics: &[],
            suppressed: &[],
            rule_sources: &[],
        })
        .expect("落 run-003（零命中也要留痕）");
    store
}

/// csv 按行拆（去掉 BOM 与行尾空白），并把引号字段解回原值。
fn csv_rows(text: &str) -> Vec<Vec<String>> {
    parse_csv(text.trim_start_matches('\u{FEFF}'))
        .into_iter()
        .map(|r| r.into_iter().map(|c| unescape(&c)).collect())
        .collect()
}

fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    quoted = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => {
                row.push(std::mem::take(&mut field));
            }
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.into_iter().filter(|r| !r.is_empty()).collect()
}

fn unescape(field: &str) -> String {
    if field.len() >= 2 && field.starts_with('"') && field.ends_with('"') {
        field[1..field.len() - 1].replace("\"\"", "\"")
    } else {
        field.to_string()
    }
}

// ---------------------------------------------------------------- step 1

#[test]
fn every_exported_row_carries_the_whole_context() {
    let store = seeded();
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.row_count(), 3, "三次审查共 3 条诊断");

    let row = &bundle.rows[0];
    // 批次 / 别名 / 件 id
    assert_eq!(row.review_run_id, "run-001");
    assert_eq!(row.encounter_alias, ALIAS);
    assert_eq!(row.doc_id, "WARD-A-001-CONSENT-01");
    // 段落定位：不能只有「哪条码」，还得能翻回原文哪一段
    assert_eq!(row.section_index, 1);
    assert_eq!(row.char_range, "[9, 18)");
    // 码 / 严重度 / 依据
    assert_eq!(row.code, "QC-PSY-01");
    assert_eq!(row.severity, "blocking");
    assert!(row.basis.contains("第 4.2 条"));
    assert!(row.message.contains("患方签字"));
    // 规则集版本 + 完整 sha256
    assert_eq!(row.ruleset_version, "0.6.0");
    assert_eq!(row.ruleset_sha256.len(), 64);
    // 推进人 + 时刻
    assert_eq!(row.actor, "护士长 王敏");
    assert!(row.event_at.starts_with("2026-03-02"));
    // 幂等键：没写回过的件为空，但**列必须在**（写回后才有值）
    assert_eq!(row.idempotency_key, "");
    assert!(bundle.columns.contains(&"写回幂等键".to_string()));
    assert!(bundle.columns.contains(&"豁免原因".to_string()));
    assert_eq!(bundle.columns.len(), 18, "原 15 列 + 处置 + 折叠去向 + 规则来源层");
}

#[test]
fn a_written_back_item_carries_its_idempotency_key_into_the_export() {
    let store = seeded();
    let es = [WritebackEntry {
        doc_id: "WARD-A-001-CONSENT-01",
        content_sha256: "content-sha-1",
        run_id: Some("run-001"),
    }];
    store
        .create_draft("draft-1", ALIAS, &es, "文书专员 李静", "2026-03-07T09:00:00+08:00")
        .expect("建草稿");
    store
        .set_selected("draft-1", "WARD-A-001-CONSENT-01", true, "2026-03-07T09:01:00+08:00")
        .expect("勾选");
    store
        .commit_writeback("draft-1", &es, "文书专员 李静", "2026-03-07T09:02:00+08:00", V1)
        .expect("提交写回");

    let bundle = store
        .export_audit(&ExportQuery::for_alias(ALIAS))
        .expect("导出");
    let row = bundle
        .rows
        .iter()
        .find(|r| r.doc_id == "WARD-A-001-CONSENT-01")
        .expect("找到该件的行");
    assert_eq!(row.idempotency_key.len(), 64, "幂等键是 64 位 sha256");
    // 别的件不沾这个键
    let other = bundle
        .rows
        .iter()
        .find(|r| r.doc_id == "WARD-A-001-ASSESS-01")
        .expect("另一件");
    assert_eq!(other.idempotency_key, "");
}

#[test]
fn a_waived_finding_keeps_its_reason_in_the_export() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(&[item("WARD-A-001-CONSENT-01")], "2026-03-02T09:00:00+08:00")
        .expect("登记");
    let mut d = diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写");
    d.waived = Some("病区豁免：WARD-A 不收约束知情同意（第 4.3 条）".into());
    let ds = vec![d];
    store
        .record_review(&review(
            "run-w",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &ds,
        ))
        .expect("落库");
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.rows.len(), 1);
    assert!(bundle.rows[0].waived_reason.contains("病区豁免"));
    assert!(bundle.rows[0].waived_reason.contains("第 4.3 条"));
    // 豁免的条目**仍然导出**：质控办要能看见「报了但被豁免」，而不是看不见。
    assert_eq!(bundle.rows[0].code, "QC-PSY-01");
}

// ---------------------------------------------------------------- step 2

#[test]
fn csv_starts_with_a_bom_and_carries_the_fifteen_columns() {
    let store = seeded();
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    let csv = to_csv(&bundle);
    assert!(csv.starts_with(AUDIT_UTF8_BOM), "缺 UTF-8 BOM，Excel 打开会乱码");

    let rows = csv_rows(&csv);
    assert_eq!(rows[0].len(), 18, "表头 18 列");
    assert_eq!(rows[0][0], "审查批次");
    assert_eq!(rows[0][6], "处置");
    assert_eq!(rows[0][11], "折叠去向");
    assert_eq!(rows[0][13], "规则集sha256");
    assert_eq!(rows[0][14], "规则来源层");
    // 表头 + 3 条证据行
    assert_eq!(rows.len(), 4);
    assert!(csv.contains("\r\n"), "csv 行尾用 CRLF");
}

#[test]
fn json_export_is_machine_readable_and_keeps_the_rollup_and_recall() {
    let store = seeded();
    let q = ExportQuery::for_alias(ALIAS).with_recall_at("2026-03-03T09:00:00+08:00");
    let bundle = store.export_audit(&q).expect("导出");
    let json = to_json(&bundle);
    let back: psychdoc_store::AuditBundle = serde_json::from_str(&json).expect("json 可回读");
    assert_eq!(back.row_count(), bundle.row_count());
    assert_eq!(back.run_count(), bundle.run_count());
    assert_eq!(back.rows[0].code, "QC-PSY-01");
    // 批次小结也在包里：按次聚合不用再回库
    assert_eq!(back.runs.len(), 3, "run-003 零命中也是一次审查，也要在包里");
    let r1 = back.runs.iter().find(|r| r.review_run_id == "run-001").unwrap();
    assert_eq!(r1.row_count, 2);
    assert_eq!(r1.codes, vec!["QC-PSY-01", "QC-PSY-07"]);
    // 版本回溯结论随包走
    assert_eq!(back.recalled.unwrap().ruleset_sha256, V1);
}

// ---------------------------------------------------------------- step 3

#[test]
fn one_review_shares_one_run_id_so_rows_can_be_aggregated_per_run() {
    let store = seeded();
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");

    let run1: Vec<_> = bundle
        .rows
        .iter()
        .filter(|r| r.review_run_id == "run-001")
        .collect();
    assert_eq!(run1.len(), 2, "同一次审查的两条诊断共用一个 run id");
    assert_ne!(
        run1[0].doc_id, run1[1].doc_id,
        "跨件的时序问题必须能看出是两条不同的件，否则按条统计会算重"
    );
    // 按批次聚合出来的批次数与 run_ids 一致
    assert_eq!(bundle.run_ids(), vec!["run-001", "run-002", "run-003"]);
    assert_eq!(bundle.run_count(), 3);

    let rollup = bundle.runs.iter().find(|r| r.review_run_id == "run-003").unwrap();
    assert_eq!(rollup.row_count, 0);
    assert!(rollup.codes.is_empty());
    assert_eq!(rollup.items_reviewed, 3, "零命中的批次仍记录审了几件");
}

#[test]
fn filtering_by_run_id_narrows_the_bundle() {
    let store = seeded();
    let bundle = store
        .export_audit(&ExportQuery::default().with_run("run-002"))
        .expect("导出");
    assert_eq!(bundle.row_count(), 1);
    assert_eq!(bundle.run_count(), 1);
    assert_eq!(bundle.rows[0].code, "QC-PSY-08");
}

#[test]
fn a_time_window_keeps_only_the_runs_inside_it() {
    let store = seeded();
    let mut q = ExportQuery::for_alias(ALIAS);
    q.since = Some("2026-03-05T00:00:00+08:00".into());
    q.until = Some("2026-03-06T23:59:59+08:00".into());
    let bundle = store.export_audit(&q).expect("导出");
    assert_eq!(
        bundle.run_ids(),
        vec!["run-002", "run-003"],
        "run-001 在窗口外"
    );
    assert_eq!(bundle.row_count(), 1);
}

// ---------------------------------------------------------------- step 4

#[test]
fn an_alias_column_carrying_a_raw_id_refuses_the_whole_export() {
    let store = Store::open_in_memory().expect("开库");
    // 绕过脱敏直接写库，模拟「上游多接了一个带住院号的字段」——这正是
    // 导出器要独立再扫一遍的理由：它不能假设上游永远是对的。
    store
        .conn()
        .execute(
            "INSERT INTO review_runs (run_id, encounter_alias, ruleset_version, ruleset_sha256,
                                      actor, started_at, finished_at, items_total, items_reviewed,
                                      items_skipped, unresolved_count, waived_count)
             VALUES ('run-x','WARD-A-001','0.6.0',?1,'护士长 王敏','2026-03-02T09:00:00+08:00',
                     '2026-03-02T09:05:00+08:00',6,6,0,0,0)",
            [V1],
        )
        .expect("插批次");
    store
        .conn()
        .execute(
            "INSERT INTO diagnostics (run_id, doc_id, section_index, char_start, char_end,
                                       code, severity, message, basis, fix_availability,
                                       waived, suppressed_by)
             VALUES ('run-x','WARD-A-001-CONSENT-01',1,9,18,'QC-PSY-01','blocking',
                     '患方签字未填写','第 4.2 条','none',NULL,NULL)",
            [],
        )
        .expect("插诊断");
    store
        .conn()
        .execute(
            "UPDATE doc_items SET encounter_alias = '住院号 1234567' WHERE doc_id = ?1",
            ["WARD-A-001-CONSENT-01"],
        )
        .ok();
    store
        .conn()
        .execute(
            "UPDATE diagnostics SET doc_id = '住院号 1234567' WHERE run_id = 'run-x'",
            [],
        )
        .expect("把件 id 换成未脱敏值");

    match store.export_audit(&ExportQuery::default()) {
        Err(StoreError::ExportLeak(detail)) => {
            assert!(detail.contains("已拒绝导出"), "文案要说清是拒导出：{detail}");
            assert!(detail.contains("文书件id"), "要点名是哪一列：{detail}");
        }
        other => panic!("应当以 E_EXPORT_LEAK 拒绝导出，实际 {other:?}"),
    }
}

#[test]
fn a_leaking_free_text_field_is_replaced_in_place_and_counted() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(&[item("WARD-A-001-CONSENT-01")], "2026-03-02T09:00:00+08:00")
        .expect("登记");
    // 自由文本列带出姓名样式串：整行作废会让一次抽查颗粒无收，
    // 就地替换 + 计数才是代价与收益相称的处置。
    let ds = vec![diag(
        "WARD-A-001-CONSENT-01",
        "QC-PSY-01",
        1,
        "家属：张丽娟代签，患方签字未填写",
    )];
    store
        .record_review(&review(
            "run-n",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &ds,
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.row_count(), 1, "替换而不是丢行");
    assert_eq!(bundle.redaction.replaced_fields, 1);
    assert_eq!(bundle.redaction.replaced_at.len(), 1);
    assert!(bundle.redaction.replaced_at[0].contains("问题描述"));
    assert!(bundle.redaction.refused.is_none());
    assert!(
        !bundle.rows[0].message.contains("张丽娟"),
        "姓名字样串必须已被替换：{}",
        bundle.rows[0].message
    );
    assert!(bundle.rows[0].message.contains("<REDACTED-NAME>"));
    // 被替换的字段不影响同行的其余列
    assert_eq!(bundle.rows[0].code, "QC-PSY-01");
    assert!(bundle.rows[0].basis.contains("第 4.2 条"));
}

// ---------------------------------------------------------------- step 5

#[test]
fn the_recall_entry_returns_the_ruleset_in_force_at_that_moment() {
    let store = seeded();
    let q = ExportQuery::for_alias(ALIAS).with_recall_at("2026-03-03T09:00:00+08:00");
    let bundle = store.export_audit(&q).expect("导出");
    let recalled = bundle.recalled.expect("03-02 那次审查在 03-03 之前");
    assert_eq!(recalled.ruleset_sha256, V1, "当时是 V1，不是后来的 V2");
    assert_eq!(recalled.codes, vec!["QC-PSY-01", "QC-PSY-07"]);

    // 规则集改版之后再问同一时刻，仍答 V1：回溯的是「那一刻」，不是「最新版」
    let later = store
        .export_audit(
            &ExportQuery::for_alias(ALIAS).with_recall_at("2026-03-07T09:00:00+08:00"),
        )
        .expect("导出");
    assert_eq!(later.recalled.unwrap().ruleset_sha256, V2);
}

#[test]
fn recalling_before_the_first_review_returns_nothing_rather_than_the_latest() {
    let store = seeded();
    let bundle = store
        .export_audit(
            &ExportQuery::for_alias(ALIAS).with_recall_at("2026-01-01T00:00:00+08:00"),
        )
        .expect("导出");
    assert!(
        bundle.recalled.is_none(),
        "早于任何一次审查的时刻不能返回「最新一版」"
    );
}

#[test]
fn recalling_another_encounter_does_not_leak_this_one() {
    let store = seeded();
    let bundle = store
        .export_audit(
            &ExportQuery::for_alias(OTHER_ALIAS).with_recall_at("2026-03-07T09:00:00+08:00"),
        )
        .expect("导出");
    assert!(bundle.recalled.is_none(), "换住院别名不该串到别人的结论");
    assert_eq!(bundle.row_count(), 0);
}

// ---------------------------------------------------------------- step 6

#[test]
fn newline_and_rare_character_fields_survive_a_csv_round_trip() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(&[item("WARD-A-001-CONSENT-01")], "2026-03-02T09:00:00+08:00")
        .expect("登记");
    // 生僻字 + 内嵌换行 + 内嵌引号 + 全角逗号：四样一起上，
    // 任何一处转义写错都会让 Excel 里的列错位。
    let nasty = "第一行：𠮷夜班值守，缺患方签字\n第二行：护士说\"已口头告知\"，另行补签（全角逗号，）";
    let ds = vec![diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, nasty)];
    store
        .record_review(&review(
            "run-csv",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &ds,
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    let csv = to_csv(&bundle);
    let rows = csv_rows(&csv);
    assert_eq!(rows.len(), 2, "内嵌换行不该把一条记录拆成两行");
    assert_eq!(rows[1].len(), 18, "内嵌逗号不该把一列拆成两列");

    // 逐字回到原文：转义只发生在分隔符与引号上，生僻字与换行原样保留。
    let message = rows[1][9].clone();
    assert_eq!(message, nasty);
    assert!(message.contains('𠮷'), "生僻字按原字符往返，不被截断或多字节拆分");
    assert!(message.contains('\n'), "换行保留在引号内");
    assert!(message.contains("\"已口头告知\""), "引号被还原");
    // 列序没有被转义挤歪
    assert_eq!(rows[1][0], "run-csv");
    assert_eq!(rows[1][5], "QC-PSY-01");
}

#[test]
fn the_escape_helper_only_quotes_when_it_has_to() {
    assert_eq!(csv_escape("QC-PSY-01"), "QC-PSY-01");
    assert_eq!(csv_escape("[9, 18)"), "\"[9, 18)\"");
    assert_eq!(csv_escape("a\"b"), "\"a\"\"b\"");
    assert_eq!(csv_escape("line1\nline2"), "\"line1\nline2\"");
    assert_eq!(csv_escape(" 前后空格  "), "\" 前后空格  \"");
}

#[test]
fn the_row_count_always_equals_the_diagnostic_count_it_exports() {
    let store = seeded();
    for q in [
        ExportQuery::default(),
        ExportQuery::for_alias(ALIAS),
        ExportQuery::default().with_run("run-001"),
        ExportQuery::for_alias(OTHER_ALIAS),
    ] {
        let bundle = store.export_audit(&q).expect("导出");
        let expected = store.diagnostic_count().expect("数诊断");
        let filtered = match (&q.encounter_alias, &q.run_id) {
            (Some(a), None) if a != ALIAS => 0,
            (_, Some(_)) => bundle.rows.len(),
            _ => expected,
        };
        assert_eq!(bundle.row_count(), filtered, "查询 {q:?}");
        // 行数 = 各批次 row_count 之和（批次小结与明细永远对得上）
        assert_eq!(
            bundle.rows.len(),
            bundle.runs.iter().map(|r| r.row_count).sum::<usize>()
        );
    }
}
// ------------------------------------------- Task 21/22/5 交接的三个补口
//
// 前面六组守住 task.json 明写的六个 step。下面这组守的是**别的任务
// 在交接里点名要 Task 31 落**的三处：折叠命中的证据、规则来源层、
// 归档核齐视图。少了它们，导出包答不上质控办最常问的三句话。

/// Task 22 交接：「导出要带折叠前后两组命中码，只导折叠后的会被
/// 「当年这一栏到底报了哪几条」问倒。」
#[test]
fn a_suppressed_finding_is_exported_too_and_says_which_rule_beat_it() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(
            &[item("WARD-A-001-RECORD-01")],
            "2026-03-02T09:00:00+08:00",
        )
        .expect("登记");

    let kept = vec![diag(
        "WARD-A-001-RECORD-01",
        "QC-PSY-04",
        0,
        "约束时长应按分钟计量，实际填「2 小时。」",
    )];
    let mut folded = diag(
        "WARD-A-001-RECORD-01",
        "QC-PSY-03",
        0,
        "约束等级取值「四级保护」不在院内允许范围内",
    );
    folded.severity = Severity::Notice;
    folded.suppressed_by = Some(
        "QC-PSY-04（约束量纲码族：QC-PSY-03 是同族的更概括表述）".into(),
    );
    let suppressed = vec![folded];

    store
        .record_review(&review_with(
            "run-fold",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &kept,
            &suppressed,
            &[(
                "QC-PSY-04".into(),
                "QC-PSY-04（科室级，覆盖 院级）".into(),
            )],
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    // 两条都在包里：清单上只留了一条，证据包不能也只留一条
    assert_eq!(bundle.row_count(), 2);
    assert!(bundle.is_disposition_accounting_balanced());

    let folded_row = bundle
        .rows
        .iter()
        .find(|r| r.code == "QC-PSY-03")
        .expect("被压下的那条必须在包里");
    assert!(folded_row.is_suppressed());
    assert_eq!(folded_row.disposition_zh(), "已折叠");
    // 压下的那条**保留输家自己的严重度**：质控办要看「本来有多重」
    assert_eq!(folded_row.severity, "notice");
    // 且能看出被谁盖掉了
    let by = folded_row.suppressed_by.as_str();
    assert!(by.contains("QC-PSY-04"), "折叠行要写明被谁盖掉：{by}");
    assert!(by.contains("更概括"), "折叠理由不能只是一串码：{by}");
    // 压下不是「不算问题」：定位与依据照样带全
    assert_eq!(folded_row.char_range, "[9, 18)");
    assert!(folded_row.basis.contains("第 4.2 条"));
}

/// 折叠前后两组码在批次小结里**分开**列，合并就答不出「哪些是报出来的」。
#[test]
fn the_rollup_keeps_the_folded_codes_apart_from_the_reported_ones() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(
            &[item("WARD-A-001-RECORD-01")],
            "2026-03-02T09:00:00+08:00",
        )
        .expect("登记");
    let kept = vec![diag(
        "WARD-A-001-RECORD-01",
        "QC-PSY-04",
        0,
        "量纲错配",
    )];
    let mut folded = diag("WARD-A-001-RECORD-01", "QC-PSY-03", 0, "取值域越界");
    folded.suppressed_by = Some("QC-PSY-04（同族更具体）".into());
    let suppressed = vec![folded];

    store
        .record_review(&review_with(
            "run-fold",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &kept,
            &suppressed,
            &[],
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    let rollup = &bundle.runs[0];
    assert_eq!(rollup.codes, vec!["QC-PSY-04"], "留下的只有赢家");
    assert_eq!(rollup.suppressed_codes, vec!["QC-PSY-03"], "压下的是输家");
    assert_eq!(rollup.reported_count, 1);
    assert_eq!(rollup.suppressed_count, 1);
    assert_eq!(rollup.row_count, 2);
}

/// 零折叠的批次不会被塞一个假的 `suppressed_codes`。
#[test]
fn a_run_without_folding_reports_an_empty_folded_group_not_a_placeholder() {
    let store = seeded();
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    for run in &bundle.runs {
        assert!(
            run.suppressed_codes.is_empty(),
            "{} 没有折叠却有折叠码：{:?}",
            run.review_run_id,
            run.suppressed_codes
        );
        assert_eq!(run.suppressed_count, 0);
        assert_eq!(run.reported_count, run.row_count);
    }
    assert_eq!(bundle.suppressed_count(), 0);
}

/// Task 5 交接：「审计导出时携带 ruleset_sha256 与规则来源层」。
#[test]
fn every_row_says_which_layer_the_winning_rule_came_from() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(
            &[item("WARD-A-001-CONSENT-01")],
            "2026-03-02T09:00:00+08:00",
        )
        .expect("登记");
    let ds = vec![diag(
        "WARD-A-001-CONSENT-01",
        "QC-PSY-02",
        1,
        "评估时间格式不符",
    )];
    store
        .record_review(&review_with(
            "run-src",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &ds,
            &[],
            &[(
                "QC-PSY-02".into(),
                "QC-PSY-02（科室级，覆盖 院级）".into(),
            )],
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    let row = &bundle.rows[0];
    // sha256 与来源层是两列：哈希答「按哪一版」，来源层答「这一版里谁说了算」
    assert_eq!(row.ruleset_sha256, V1);
    assert_eq!(row.rule_source, "QC-PSY-02（科室级，覆盖 院级）");
}

/// 老库迁移前的行没有来源层：空串即「当时没记过」，**不得**回填一个占位的
/// 「院级」——那等于替历史编一个没发生过的依据。
#[test]
fn a_row_recorded_before_the_layer_column_reports_an_empty_source_not_a_guess() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(
            &[item("WARD-A-001-CONSENT-01")],
            "2026-03-02T09:00:00+08:00",
        )
        .expect("登记");
    // 只传码，不传 rule_sources —— 等价于迁移前的老行。
    let ds = vec![diag(
        "WARD-A-001-CONSENT-01",
        "QC-PSY-01",
        1,
        "患方签字未填写",
    )];
    store
        .record_review(&review(
            "run-legacy",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &ds,
        ))
        .expect("落库");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.rows[0].rule_source, "");
    // 但处置列有 v6 的 DEFAULT：老行确实留在清单上
    assert!(bundle.rows[0].is_reported());
}

/// Task 21 交接：「`ArchiveView` 直接 `Serialize`，导出行不要再拼一次」。
#[test]
fn an_archive_view_travels_with_the_bundle_and_keeps_its_own_wording() {
    let store = Store::open_in_memory().expect("开库");
    store
        .record_review(&review(
            "run-ar",
            V1,
            "2026-03-02T09:00:00+08:00",
            "病案室 质控员",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写")],
        ))
        .expect("落库");

    // 造一份与 `ArchiveView` 同形的视图 JSON（含口径说明）。
    let view_json = serde_json::json!({
        "encounter_alias": ALIAS,
        "as_of": "2026-03-05T09:00:00+08:00",
        "rows": [],
        "missing": [
            {"node_id": "WARD-A-001-RELEASE-01", "encounter_alias": ALIAS,
             "template_id": "RESTRAINT_RELEASE", "first_seen": "2026-03-01T09:00:00+08:00",
             "days": 4, "basis_zh": "按首次登记为缺失的自然日差计算"}
        ],
        "encounter_age": {"encounter_alias": ALIAS, "days": 4,
                          "oldest_missing_node": "WARD-A-001-RELEASE-01",
                          "missing_count": 1,
                          "basis_zh": "取本住院最老的一条缺件；并列时取件 id 最小的一条"},
        "blocking_edges": [["WARD-A-001-RECORD-01", "WARD-A-001-RELEASE-01"]],
        "soft_gaps": [],
        "manual_queue": [],
        "basis_zh": [
            "欠件天数按自然日差，不按 24 小时整除",
            "软缺件（requires: false）可见但不阻断归档",
            "成环导致算不出闭包时一律记「未核齐」，不给「已核齐」"
        ]
    })
    .to_string();

    store
        .record_archive_view(&psychdoc_store::ArchiveSnapshot {
            view_id: psychdoc_store::ArchiveSnapshot::view_id(
                "run-ar",
                "2026-03-05T09:00:00+08:00",
            ),
            run_id: "run-ar".into(),
            encounter_alias: ALIAS.into(),
            as_of: "2026-03-05T09:00:00+08:00".into(),
            is_ready: false,
            view_json: view_json.clone(),
            recorded_at: "2026-03-05T09:00:00+08:00".into(),
        })
        .expect("落视图");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.archive_views.len(), 1);
    let v = &bundle.archive_views[0];
    assert_eq!(v.encounter_alias, ALIAS);
    assert_eq!(v.readiness, "未核齐");
    assert_eq!(v.missing_count, 1);
    assert_eq!(v.missing_days, Some(4));
    // 口径说明是**原样**带走的，不重写措辞
    assert_eq!(v.basis_zh.len(), 3);
    assert!(v.basis_zh[0].contains("自然日差"));
    assert!(v.basis_zh[2].contains("未核齐"));
    // 全量视图也在包里（json 用）
    assert_eq!(v.view_json, view_json);
}

/// 视图幂等：同一批次同一时刻反复存不追加第二条，否则质控办数不清核过几次。
#[test]
fn saving_the_same_archive_view_twice_still_yields_one_row() {
    let store = Store::open_in_memory().expect("开库");
    store
        .record_review(&review(
            "run-ar",
            V1,
            "2026-03-02T09:00:00+08:00",
            "病案室 质控员",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写")],
        ))
        .expect("落批次");
    let snap = psychdoc_store::ArchiveSnapshot {
        view_id: psychdoc_store::ArchiveSnapshot::view_id("run-ar", "2026-03-05T09:00:00+08:00"),
        run_id: "run-ar".into(),
        encounter_alias: ALIAS.into(),
        as_of: "2026-03-05T09:00:00+08:00".into(),
        is_ready: true,
        view_json: "{\"missing\":[]}".into(),
        recorded_at: "2026-03-05T09:00:00+08:00".into(),
    };
    store.record_archive_view(&snap).expect("第一次");
    store.record_archive_view(&snap).expect("第二次");
    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.archive_views.len(), 1, "同一 (run_id, as_of) 只留一份");
}

/// 视图 id 用分隔符隔字段：「ab|c」与「a|bc」不得撞成同一个键。
#[test]
fn the_archive_view_id_separates_its_two_fields() {
    let a = psychdoc_store::ArchiveSnapshot::view_id("ab", "c");
    let b = psychdoc_store::ArchiveSnapshot::view_id("a", "bc");
    assert_ne!(a, b, "字段边界串位会让两次不同的核齐共用一个视图号");
    assert_eq!(a.len(), 64);
    assert_eq!(a, psychdoc_store::ArchiveSnapshot::view_id("ab", "c"), "稳定");
}

/// 视图单独出一份 csv，同样带 BOM；口径说明不因带分隔符而错列。
#[test]
fn the_archive_csv_carries_its_own_bom_and_survives_commas_in_the_basis() {
    let store = Store::open_in_memory().expect("开库");
    store
        .record_review(&review(
            "run-ar",
            V1,
            "2026-03-02T09:00:00+08:00",
            "病案室 质控员",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写")],
        ))
        .expect("落批次");
    let view_json = serde_json::json!({
        "encounter_alias": ALIAS,
        "as_of": "2026-03-05T09:00:00+08:00",
        "rows": [],
        "missing": [{"node_id": "N1", "encounter_alias": ALIAS, "template_id": "T",
                     "first_seen": "2026-03-01T09:00:00+08:00", "days": 4,
                     "basis_zh": "自然日差，不按 24 小时整除"}],
        "encounter_age": {"encounter_alias": ALIAS, "days": 4,
                          "oldest_missing_node": "N1", "missing_count": 1,
                          "basis_zh": "取最老的一条"},
        "blocking_edges": [], "soft_gaps": [],
        "manual_queue": ["WARD-A-001-CONSENT-01"],
        // 口径说明里带逗号与顿号：csv 转义写错就会错列
        "basis_zh": ["欠件天数按自然日差，不按 24 小时整除；软缺件可见但不阻断"]
    })
    .to_string();
    store
        .record_archive_view(&psychdoc_store::ArchiveSnapshot {
            view_id: psychdoc_store::ArchiveSnapshot::view_id("run-ar", "2026-03-05T09:00:00+08:00"),
            run_id: "run-ar".into(),
            encounter_alias: ALIAS.into(),
            as_of: "2026-03-05T09:00:00+08:00".into(),
            is_ready: false,
            view_json,
            recorded_at: "2026-03-05T09:00:00+08:00".into(),
        })
        .expect("落视图");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    let csv = psychdoc_store::archive_to_csv(&bundle);
    assert!(csv.starts_with(AUDIT_UTF8_BOM), "归档 csv 也带 BOM");
    let rows = csv_rows(&csv);
    assert_eq!(rows.len(), 2, "表头 + 1 份视图");
    assert_eq!(rows[0].len(), 9, "归档 csv 自己的列数");
    assert_eq!(rows[1].len(), 9, "口径说明里的逗号不该把一列拆成两列");
    assert_eq!(rows[1][4], "未核齐");
    assert_eq!(rows[1][5], "1");
    assert_eq!(rows[1][6], "4");
    assert_eq!(rows[1][7], "WARD-A-001-CONSENT-01");
    assert!(rows[1][8].contains("自然日差"), "口径说明原样保留");
}

/// 欠件天数算不出来时写「未登记发现时刻」，**不填 0**。
/// 填 0 会让「欠了 0 天」看着像刚发现，病案室据此催办就会误判紧迫程度。
#[test]
fn an_unknown_missing_age_is_written_as_such_rather_than_zero_days() {
    let store = Store::open_in_memory().expect("开库");
    store
        .record_review(&review(
            "run-ar",
            V1,
            "2026-03-02T09:00:00+08:00",
            "病案室 质控员",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写")],
        ))
        .expect("落批次");
    // encounter_age 为 null = 没有任何一条缺件登记过发现时刻
    let view_json = serde_json::json!({
        "encounter_alias": ALIAS, "as_of": "2026-03-05T09:00:00+08:00",
        "rows": [], "missing": [], "encounter_age": null,
        "blocking_edges": [], "soft_gaps": [], "manual_queue": [],
        "basis_zh": ["没有登记过发现时刻的缺件，欠件天数留空"]
    })
    .to_string();
    store
        .record_archive_view(&psychdoc_store::ArchiveSnapshot {
            view_id: psychdoc_store::ArchiveSnapshot::view_id("run-ar", "2026-03-05T09:00:00+08:00"),
            run_id: "run-ar".into(),
            encounter_alias: ALIAS.into(),
            as_of: "2026-03-05T09:00:00+08:00".into(),
            is_ready: false,
            view_json,
            recorded_at: "2026-03-05T09:00:00+08:00".into(),
        })
        .expect("落视图");

    let bundle = store.export_audit(&ExportQuery::default()).expect("导出");
    assert_eq!(bundle.archive_views[0].missing_days, None);
    let csv = psychdoc_store::archive_to_csv(&bundle);
    assert!(
        csv.contains("未登记发现时刻"),
        "算不出来的天数要说明为什么，不能填 0"
    );
    // 断言只针对欠件天数那一列：缺件数本来就是 0，混进来断言就没意义了。
    let row = &csv_rows(&csv)[1];
    assert_eq!(row[6], "未登记发现时刻", "算不出来的天数要说明为什么，不能填 0");
}

/// 视图 JSON 读坏时不炸掉整包导出：摘要是附加材料，证据行才是主体。
#[test]
fn a_corrupt_view_json_does_not_take_the_whole_export_down() {
    let store = Store::open_in_memory().expect("开库");
    store
        .record_review(&review(
            "run-ar",
            V1,
            "2026-03-02T09:00:00+08:00",
            "病案室 质控员",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01", 1, "患方签字未填写")],
        ))
        .expect("落批次");
    store
        .record_archive_view(&psychdoc_store::ArchiveSnapshot {
            view_id: psychdoc_store::ArchiveSnapshot::view_id("run-ar", "2026-03-05T09:00:00+08:00"),
            run_id: "run-ar".into(),
            encounter_alias: ALIAS.into(),
            as_of: "2026-03-05T09:00:00+08:00".into(),
            is_ready: false,
            view_json: "{这不是合法 JSON".into(),
            recorded_at: "2026-03-05T09:00:00+08:00".into(),
        })
        .expect("落坏视图");

    let bundle = store
        .export_audit(&ExportQuery::default())
        .expect("证据行照样导出");
    assert_eq!(bundle.row_count(), 1, "证据行不因视图坏掉而丢");
    // 坏视图的原文仍带走，供质控办或排障人工看
    assert_eq!(bundle.archive_views[0].view_json, "{这不是合法 JSON");
    assert_eq!(bundle.archive_views[0].missing_count, 0);
}

/// 缺件首次进入缺失态的时刻一旦记下就不被后续推进覆盖——欠件天数
/// 正是靠这个才不是「今天才发现」。
#[test]
fn the_first_missing_moment_is_kept_across_later_transitions() {
    let store = Store::open_in_memory().expect("开库");
    let it = item("WARD-A-001-CONSENT-01");
    store.register_items(&[it], "2026-03-01T09:00:00+08:00").expect("登记");
    store
        .init_state("WARD-A-001-CONSENT-01", psychdoc_model::state::DocState::Missing, "护士长 王敏", "2026-03-01T09:00:00+08:00")
        .expect("初始化为缺失");
    store
        .advance_state("WARD-A-001-CONSENT-01", psychdoc_model::state::DocState::Draft, "文书专员 李静", "2026-03-03T09:00:00+08:00")
        .expect("补写后转草稿");
    store
        .advance_state("WARD-A-001-CONSENT-01", psychdoc_model::state::DocState::Missing, "护士长 王敏", "2026-03-05T09:00:00+08:00")
        .expect("再次缺失");

    // 再缺一次的时刻是 03-05，但这一轮欠件天数从 03-01 起算才说得通吗？
    // 不说得通——件已经补过又丢了。所以离开缺失态时清空、重新缺时从头记。
    let since = store.missing_since("WARD-A-001-CONSENT-01").expect("读时刻");
    assert_eq!(
        since.as_deref(),
        Some("2026-03-05T09:00:00+08:00"),
        "补齐过的件再缺，从这一轮重新记；沿用 03-01 会把 4 天说成 0 天"
    );
}

/// 从没进过缺失态的件，`missing_since` 是 None 而不是「今天」。
#[test]
fn an_item_that_was_never_missing_has_no_missing_moment() {
    let store = Store::open_in_memory().expect("开库");
    let it = item("WARD-A-001-CONSENT-01");
    store.register_items(&[it], "2026-03-01T09:00:00+08:00").expect("登记");
    store
        .init_state("WARD-A-001-CONSENT-01", psychdoc_model::state::DocState::Draft, "护士长 王敏", "2026-03-01T09:00:00+08:00")
        .expect("初始化为草稿");
    assert_eq!(
        store.missing_since("WARD-A-001-CONSENT-01").expect("读时刻"),
        None,
        "没进过缺失态就是没有，不填当前时间"
    );
}

/// 回溯结论与批次小结**必须同一个口径**：压下的码不许混进「报出的」。
/// 两处若各写一遍 filter，早晚会对不上——而对不上的那一份正��证据包
/// 唯一存在的理由被破坏。
#[test]
fn the_recall_split_matches_the_rollup_split_for_the_same_run() {
    let store = Store::open_in_memory().expect("开库");
    store
        .register_items(
            &[item("WARD-A-001-RECORD-01")],
            "2026-03-02T09:00:00+08:00",
        )
        .expect("登记");
    let kept = vec![diag("WARD-A-001-RECORD-01", "QC-PSY-04", 0, "量纲错配")];
    let mut folded = diag("WARD-A-001-RECORD-01", "QC-PSY-03", 0, "取值域越界");
    folded.suppressed_by = Some("QC-PSY-04（同族更具体）".into());
    let suppressed = vec![folded];
    store
        .record_review(&review_with(
            "run-x",
            V1,
            "2026-03-02T09:00:00+08:00",
            "护士长 王敏",
            &kept,
            &suppressed,
            &[],
        ))
        .expect("落库");

    let bundle = store
        .export_audit(
            &ExportQuery::for_alias(ALIAS).with_recall_at("2026-03-03T09:00:00+08:00"),
        )
        .expect("导出");
    let rollup = &bundle.runs[0];
    let recalled = bundle.recalled.as_ref().expect("回溯得出结论");
    assert_eq!(recalled.run_id, rollup.review_run_id);
    assert_eq!(recalled.codes, rollup.codes, "报出的码两处必须一致");
    assert_eq!(
        recalled.suppressed_codes, rollup.suppressed_codes,
        "压下的码两处必须一致"
    );
    assert_eq!(recalled.codes, vec!["QC-PSY-04"]);
    assert_eq!(recalled.suppressed_codes, vec!["QC-PSY-03"]);
}

/// 没有折叠时，回溯的压下组是空数组而不是空串占位。
#[test]
fn a_run_without_folding_recalls_an_empty_folded_group() {
    let store = seeded();
    // 03-06 当天那次（run-003）是零命中的那次，回溯到它两组都是空的；
    // 这里刻意回溯到 03-05 的 run-002——有条命中、没折叠，
    // 才能验「有报出但压下组为空」这个真正要守的形状。
    let bundle = store
        .export_audit(
            &ExportQuery::for_alias(ALIAS).with_recall_at("2026-03-05T18:00:00+08:00"),
        )
        .expect("导出");
    let recalled = bundle.recalled.expect("回溯得出结论");
    assert_eq!(recalled.run_id, "run-002");
    assert!(recalled.suppressed_codes.is_empty(), "没折叠就没有压下组");
    assert_eq!(recalled.codes, vec!["QC-PSY-08"]);
}
