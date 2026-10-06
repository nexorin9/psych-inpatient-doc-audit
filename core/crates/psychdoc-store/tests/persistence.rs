//! Task 10 验收：sqlite 持久化、幂等键、审计 Provenance 与版本回溯。
//!
//! 六组断言对应 task.json 的六个 step：
//!
//! 1. schema 迁移与七张表可用；
//! 2. 幂等键唯一约束（重复插入返回已存在而非新记录）；
//! 3. 写回草稿可撤回（撤回不产生已生效事实）；
//! 4. 审计链：调阅与写回各一条 Provenance，携带规则集 sha256 + 命中码 + 推进人 + 时刻；
//! 5. 按当时规则集版本回溯；
//! 6. 端到端：同日重复确认 0 条新写回；回溯在规则集变更前后返回不同哈希；
//!    库文件落用户数据目录而非 round 根。

use psychdoc_model::state::DocState;
use psychdoc_model::{Diagnostic, DocItem, DocLoc, FixAvailability, RuleMessage, Severity, Span};
use psychdoc_store::{
    idempotency_key, paths, AuditKind, CommitOutcome, ReviewRunInput, RulesetRecall, Store,
    StoreError, WritebackEntry,
};

const ALIAS: &str = "WARD-A-001";
const RULESET_V1: &str = "e4eda0c79c95aa11bb22cc33dd44ee9911a1b2c3d4e5f60718293a4b5c6d7e8f9";
const RULESET_V2: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9001122334455667788990aabbccddeeff0";

fn item(doc_id: &str) -> DocItem {
    let mut it = DocItem::new(doc_id, "CONSENT_FORM", ALIAS);
    it.source_system = "emr".into();
    it
}

fn diag(doc_id: &str, code: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        loc: DocLoc::new(doc_id, 1, Span::new(9, 18).unwrap()),
        message: RuleMessage::new(
            format!("字段 {code} 未填写"),
            "《精神科住院护理常规》第 4.2 条",
        )
        .unwrap(),
        fix_availability: FixAvailability::None,
        severity: Severity::Blocking,
        fix: None,
        suppressed_by: None,
        waived: None,
    }
}

fn review_input<'a>(
    run_id: &'a str,
    sha: &'a str,
    at: &'a str,
    diags: &'a [Diagnostic],
) -> ReviewRunInput<'a> {
    ReviewRunInput {
        run_id,
        encounter_alias: ALIAS,
        ruleset_version: "v2026.03",
        ruleset_sha256: sha,
        actor: "护士长A",
        started_at: at,
        finished_at: Some(at),
        items_total: 4,
        items_reviewed: 4,
        items_skipped: 0,
        unresolved_count: 0,
        waived_count: 0,
        diagnostics: diags,
        suppressed: &[],
        rule_sources: &[],
    }
}

fn entries<'a>(docs: &'a [&'a str], sha: &'a str) -> Vec<WritebackEntry<'a>> {
    docs.iter()
        .map(|d| WritebackEntry {
            doc_id: d,
            content_sha256: sha,
            run_id: Some("run-001"),
        })
        .collect()
}

// ---------------------------------------------------------------- step 1

#[test]
fn schema_migrates_to_the_latest_version_and_every_table_exists() {
    // 版本号随 Task 26 的缓存表升到 v2，因此这里断言「等于当前最高版本」
    // 而不是钉死 v1——钉死版本号会让「加了张表」这种正常演进变成红灯。
    let store = Store::open_in_memory().unwrap();
    assert_eq!(
        psychdoc_store::schema_version(store.conn()).unwrap(),
        psychdoc_store::SCHEMA_VERSION_LATEST
    );
    for table in [
        "doc_items",
        "doc_states",
        "review_runs",
        "diagnostics",
        "writeback_drafts",
        "writeback_log",
        "audit_events",
        // Task 26：增量缓存表
        "review_cache",
    ] {
        let n: i64 = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "缺表 {table}");
    }
}

#[test]
fn migrating_twice_is_idempotent() {
    let store = Store::open_in_memory().unwrap();
    psychdoc_store::schema::migrate(store.conn()).unwrap();
    psychdoc_store::schema::migrate(store.conn()).unwrap();
    psychdoc_store::schema::migrate_to_v1(store.conn()).unwrap();
    psychdoc_store::schema::migrate_to_v2(store.conn()).unwrap();
    assert_eq!(
        psychdoc_store::schema_version(store.conn()).unwrap(),
        psychdoc_store::SCHEMA_VERSION_LATEST,
        "重复迁移（含只调 v1 的那一步）都不该把版本号改回去"
    );
}

#[test]
fn a_v1_library_upgrades_to_v2_without_losing_rows() {
    // 真实升级路径：库里已是 v1 的老库被新代码打开，必须补上缓存表，
    // 且原有七张表的行数一条不少。
    let dir = std::env::temp_dir().join("psychdoc-store-v1-upgrade");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("psychdoc.sqlite3");
    {
        // 裸连接不像 Store::open 那样自己建父目录，这里显式建一次。
        std::fs::create_dir_all(&dir).unwrap();
        let conn = rusqlite::Connection::open(&path).unwrap();
        psychdoc_store::schema::migrate_to_v1(&conn).unwrap();
        assert_eq!(
            psychdoc_store::schema_version(&conn).unwrap(),
            psychdoc_store::SCHEMA_VERSION_V1
        );
        conn.execute(
            "INSERT INTO doc_items
               (doc_id, template_id, encounter_alias, source_system,
                content_sha256, binding, first_seen_at, last_seen_at)
             VALUES ('WARD-A-001-CONSENT-01','restraint_consent','WARD-A-001','his',
                     'h1','Confident','2026-03-02T09:00:00+08:00','2026-03-02T09:00:00+08:00')",
            [],
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(
        psychdoc_store::schema_version(store.conn()).unwrap(),
        psychdoc_store::SCHEMA_VERSION_LATEST
    );
    assert_eq!(
        store.item_count().unwrap(),
        1,
        "升级不能动老库的既有数据"
    );
    let cache_tables: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='review_cache'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(cache_tables, 1, "v1 → v2 应补上缓存表");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn registering_items_keeps_first_seen_at_and_updates_content_hash() {
    let store = Store::open_in_memory().unwrap();
    let it = item("WARD-A-001-CONSENT-01");
    store.register_items(std::slice::from_ref(&it), "2026-03-02T09:00:00+08:00").unwrap();
    let first: String = store
        .conn()
        .query_row(
            "SELECT first_seen_at FROM doc_items WHERE doc_id='WARD-A-001-CONSENT-01'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    store
        .register_items(std::slice::from_ref(&it), "2026-03-03T09:00:00+08:00")
        .unwrap();
    let (first_again, last): (String, String) = store
        .conn()
        .query_row(
            "SELECT first_seen_at, last_seen_at FROM doc_items WHERE doc_id='WARD-A-001-CONSENT-01'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(first, first_again, "首次登记时刻不得被覆盖");
    assert_eq!(last, "2026-03-03T09:00:00+08:00");
    assert_eq!(store.item_count().unwrap(), 1);
}

#[test]
fn state_advances_legally_and_rejects_an_illegal_jump_without_writing() {
    let store = Store::open_in_memory().unwrap();
    let doc = "WARD-A-001-CONSENT-01";
    store
        .register_items(&[item(doc)], "2026-03-02T09:00:00+08:00")
        .unwrap();
    store
        .init_state(doc, DocState::Missing, "护士长A", "2026-03-02T09:00:00+08:00")
        .unwrap();
    for to in [
        DocState::Draft,
        DocState::Created,
        DocState::Received,
        DocState::Archivable,
    ] {
        store
            .advance_state(doc, to, "护士长A", "2026-03-02T10:00:00+08:00")
            .unwrap();
    }
    assert_eq!(store.state_of(doc).unwrap(), Some(DocState::Archivable));

    // 终态不可再跳
    let err = store
        .advance_state(doc, DocState::Draft, "护士长A", "2026-03-02T11:00:00+08:00")
        .unwrap_err();
    assert_eq!(err.code(), "E_STATE_JUMP");
    assert_eq!(store.state_of(doc).unwrap(), Some(DocState::Archivable));

    // 库里没登记过的件不能推进
    let err = store
        .advance_state("NOT-THERE", DocState::Draft, "X", "t")
        .unwrap_err();
    assert_eq!(err.code(), "E_STATE_UNKNOWN");
}

#[test]
fn a_state_row_cannot_reference_an_unregistered_item() {
    // 外键是硬的：没有登记过的件拿不到状态行。否则库里会出现「有个状态，
    // 但查不到这是什么件」的孤儿记录，审计导出时说不清。
    let store = Store::open_in_memory().unwrap();
    let err = store
        .init_state(
            "GHOST-01",
            DocState::Missing,
            "护士长A",
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap_err();
    assert_eq!(err.code(), "E_DB_IO");
    assert!(store.state_of("GHOST-01").unwrap().is_none());
}

// ---------------------------------------------------------------- step 2

#[test]
fn idempotency_key_is_stable_and_field_separated() {
    let a = idempotency_key(ALIAS, "DOC-1", "hash-1");
    assert_eq!(a, idempotency_key(ALIAS, "DOC-1", "hash-1"), "同输入必须同键");
    assert_eq!(a.len(), 64);
    // 字段分隔：别名与件 id 的拼接方式不同不得撞键
    assert_ne!(a, idempotency_key("WARD-A-001X", "", "DOC-1hash-1"));
    // 三个分量各换一个键就变
    assert_ne!(a, idempotency_key("WARD-B-001", "DOC-1", "hash-1"));
    assert_ne!(a, idempotency_key(ALIAS, "DOC-2", "hash-1"));
    assert_ne!(a, idempotency_key(ALIAS, "DOC-1", "hash-2"));
}

#[test]
fn a_repeated_confirmation_on_the_same_day_writes_zero_new_rows() {
    let store = Store::open_in_memory().unwrap();
    let docs = ["WARD-A-001-CONSENT-01"];
    let sha = "hash-v1";
    let es = entries(&docs, sha);

    store
        .create_draft("draft-1", ALIAS, &es, "护士长A", "2026-03-02T09:00:00+08:00")
        .unwrap();
    store
        .set_selected("draft-1", docs[0], true, "2026-03-02T09:05:00+08:00")
        .unwrap();
    assert_eq!(store.selected_count("draft-1").unwrap(), 1);

    let first = store
        .commit_writeback(
            "draft-1",
            &es,
            "护士长A",
            "2026-03-02T09:10:00+08:00",
            RULESET_V1,
        )
        .unwrap();
    assert_eq!(first, CommitOutcome::Committed { keys: vec![idempotency_key(ALIAS, docs[0], sha)] });
    assert_eq!(first.new_rows(), 1);
    assert_eq!(store.writeback_row_count().unwrap(), 1);
    assert_eq!(store.audit_event_count(AuditKind::Writeback).unwrap(), 1);

    // 撤回后同日再提交：同键命中 → 0 条新写回、0 条新审计
    store
        .withdraw_draft("draft-1", "护士长A", "2026-03-02T09:20:00+08:00", RULESET_V1)
        .unwrap();
    store
        .set_selected("draft-1", docs[0], true, "2026-03-02T09:21:00+08:00")
        .unwrap();
    let second = store
        .commit_writeback(
            "draft-1",
            &es,
            "护士长A",
            "2026-03-02T09:30:00+08:00",
            RULESET_V1,
        )
        .unwrap();
    assert_eq!(
        second,
        CommitOutcome::AlreadyExists {
            keys: vec![idempotency_key(ALIAS, docs[0], sha)]
        }
    );
    assert_eq!(second.new_rows(), 0, "幂等命中必须新增 0 行");
    assert_eq!(store.writeback_row_count().unwrap(), 1, "写回表仍只有 1 行");
    // 写回审计事件 1 条来自首次提交 + 1 条来自撤回，重复提交不再追加
    assert_eq!(store.audit_event_count(AuditKind::Writeback).unwrap(), 2);
}

#[test]
fn a_changed_content_hash_produces_a_new_key_and_may_be_written_again() {
    let store = Store::open_in_memory().unwrap();
    let docs = ["WARD-A-001-CONSENT-01"];
    for (draft, sha) in [("draft-a", "hash-v1"), ("draft-b", "hash-v2")] {
        let es = entries(&docs, sha);
        store
            .create_draft(draft, ALIAS, &es, "护士长A", "2026-03-02T09:00:00+08:00")
            .unwrap();
        store
            .set_selected(draft, docs[0], true, "2026-03-02T09:05:00+08:00")
            .unwrap();
        let out = store
            .commit_writeback(draft, &es, "护士长A", "2026-03-02T09:10:00+08:00", RULESET_V1)
            .unwrap();
        assert_eq!(out.new_rows(), 1, "内容变了应放行新写回");
    }
    assert_eq!(store.writeback_row_count().unwrap(), 2);
}

// ---------------------------------------------------------------- step 3

#[test]
fn a_draft_can_be_withdrawn_without_producing_an_effective_fact() {
    let store = Store::open_in_memory().unwrap();
    let docs = ["WARD-A-001-CONSENT-01"];
    let sha = "hash-v1";
    let es = entries(&docs, sha);
    store
        .create_draft("draft-1", ALIAS, &es, "护士长A", "2026-03-02T09:00:00+08:00")
        .unwrap();
    store
        .set_selected("draft-1", docs[0], true, "2026-03-02T09:05:00+08:00")
        .unwrap();

    // 未发送的草稿不能撤回（还没发过可撤的）
    let err = store
        .withdraw_draft("draft-1", "护士长A", "2026-03-02T09:06:00+08:00", RULESET_V1)
        .unwrap_err();
    assert_eq!(err.code(), "E_DRAFT_STATE");

    store
        .commit_writeback("draft-1", &es, "护士长A", "2026-03-02T09:10:00+08:00", RULESET_V1)
        .unwrap();
    assert_eq!(store.draft_state("draft-1").unwrap().as_deref(), Some("sent"));

    store
        .withdraw_draft("draft-1", "护士长A", "2026-03-02T09:20:00+08:00", RULESET_V1)
        .unwrap();
    assert_eq!(store.draft_state("draft-1").unwrap().as_deref(), Some("draft"));
    assert_eq!(store.selected_count("draft-1").unwrap(), 0, "撤回后勾选清空");
}

#[test]
fn operating_on_a_missing_draft_reports_a_specific_code() {
    let store = Store::open_in_memory().unwrap();
    let err = store
        .set_selected("nope", "DOC", true, "t")
        .unwrap_err();
    assert_eq!(err.code(), "E_DRAFT_UNKNOWN");
}

// ---------------------------------------------------------------- step 4

#[test]
fn every_review_writes_exactly_one_provenance_with_codes_and_actor() {
    let store = Store::open_in_memory().unwrap();
    let diags = vec![
        diag("WARD-A-001-CONSENT-01", "QC-PSY-01"),
        diag("WARD-A-001-CONSENT-01", "QC-PSY-02"),
        diag("WARD-A-001-RECORD-01", "QC-PSY-05"),
    ];
    store
        .record_review(&review_input("run-001", RULESET_V1, "2026-03-02T09:00:00+08:00", &diags))
        .unwrap();

    assert_eq!(store.review_run_count().unwrap(), 1);
    assert_eq!(store.diagnostic_count().unwrap(), 3);
    assert_eq!(store.audit_event_count(AuditKind::Review).unwrap(), 1);

    let (sha, codes, actor, at): (String, String, String, String) = store
        .conn()
        .query_row(
            "SELECT ruleset_sha256, codes_json, actor, event_at
               FROM audit_events WHERE kind='review'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .unwrap();
    assert_eq!(sha, RULESET_V1);
    assert_eq!(
        codes,
        r#"["QC-PSY-01","QC-PSY-02","QC-PSY-05"]"#,
        "Provenance 必须带上当时命中的码集合"
    );
    assert_eq!(actor, "护士长A");
    assert_eq!(at, "2026-03-02T09:00:00+08:00");
}

#[test]
fn review_and_writeback_each_leave_their_own_provenance_in_one_trail() {
    let store = Store::open_in_memory().unwrap();
    let diags = vec![diag("WARD-A-001-CONSENT-01", "QC-PSY-01")];
    store
        .record_review(&review_input("run-001", RULESET_V1, "2026-03-02T09:00:00+08:00", &diags))
        .unwrap();

    let docs = ["WARD-A-001-CONSENT-01"];
    let es = entries(&docs, "hash-v1");
    store
        .create_draft("draft-1", ALIAS, &es, "护士长A", "2026-03-02T09:05:00+08:00")
        .unwrap();
    store
        .set_selected("draft-1", docs[0], true, "2026-03-02T09:06:00+08:00")
        .unwrap();
    store
        .commit_writeback("draft-1", &es, "护士长A", "2026-03-02T09:10:00+08:00", RULESET_V1)
        .unwrap();

    let trail = store.audit_trail(ALIAS).unwrap();
    assert_eq!(trail.len(), 2, "调阅 + 写回各一条");
    assert_eq!(trail[0].1, "review");
    assert_eq!(trail[1].1, "writeback");
    assert_eq!(trail[1].3, "护士长A");

    // 审计链里出现过的规则集哈希可去重回查
    let hashes = store.audited_ruleset_hashes().unwrap();
    assert_eq!(hashes, vec![RULESET_V1.to_string()]);
}

#[test]
fn the_diagnostic_row_keeps_its_location_and_basis_clause() {
    let store = Store::open_in_memory().unwrap();
    let diags = vec![diag("WARD-A-001-CONSENT-01", "QC-PSY-01")];
    store
        .record_review(&review_input("run-001", RULESET_V1, "2026-03-02T09:00:00+08:00", &diags))
        .unwrap();
    let (doc, sec, start, end, basis, fix): (String, i64, i64, i64, String, String) = store
        .conn()
        .query_row(
            "SELECT doc_id, section_index, char_start, char_end, basis, fix_availability
               FROM diagnostics",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .unwrap();
    assert_eq!(doc, "WARD-A-001-CONSENT-01");
    assert_eq!((sec, start, end), (1, 9, 18));
    assert!(basis.contains("第 4.2 条"));
    assert_eq!(fix, "none", "不可自动修是一等状态，落库后仍是 none");
}

// ---------------------------------------------------------------- step 5

#[test]
fn recalling_a_time_point_returns_the_ruleset_and_codes_in_force_then() {
    let store = Store::open_in_memory().unwrap();
    let v1 = vec![diag("WARD-A-001-CONSENT-01", "QC-PSY-01")];
    let v2 = vec![
        diag("WARD-A-001-CONSENT-01", "QC-PSY-01"),
        diag("WARD-A-001-CONSENT-01", "QC-PSY-02"),
    ];
    store
        .record_review(&review_input(
            "run-001",
            RULESET_V1,
            "2026-03-01T09:00:00+08:00",
            &v1,
        ))
        .unwrap();
    store
        .record_review(&review_input(
            "run-002",
            RULESET_V2,
            "2026-03-05T09:00:00+08:00",
            &v2,
        ))
        .unwrap();

    let before: RulesetRecall = store
        .recall_ruleset(ALIAS, "2026-03-03T00:00:00+08:00")
        .unwrap()
        .expect("改版前应能回溯");
    assert_eq!(before.ruleset_sha256, RULESET_V1);
    assert_eq!(before.codes, vec!["QC-PSY-01"]);

    let after = store
        .recall_ruleset(ALIAS, "2026-03-06T00:00:00+08:00")
        .unwrap()
        .expect("改版后应能回溯");
    assert_eq!(after.ruleset_sha256, RULESET_V2);
    assert_eq!(after.codes, vec!["QC-PSY-01", "QC-PSY-02"]);
    assert_ne!(before.ruleset_sha256, after.ruleset_sha256);

    // 早于任何一次审查的时刻答「没有」，而不是返回最新一版
    assert!(store
        .recall_ruleset(ALIAS, "2026-02-28T00:00:00+08:00")
        .unwrap()
        .is_none());
    // 换个住院别名不串
    assert!(store
        .recall_ruleset("WARD-B-999", "2026-03-06T00:00:00+08:00")
        .unwrap()
        .is_none());
}

#[test]
fn the_ruleset_history_shows_the_change_over_time() {
    let store = Store::open_in_memory().unwrap();
    let d = vec![diag("WARD-A-001-CONSENT-01", "QC-PSY-01")];
    store
        .record_review(&review_input("run-001", RULESET_V1, "2026-03-01T09:00:00+08:00", &d))
        .unwrap();
    store
        .record_review(&review_input("run-002", RULESET_V2, "2026-03-05T09:00:00+08:00", &d))
        .unwrap();
    let hist = store.ruleset_history(ALIAS).unwrap();
    assert_eq!(hist.len(), 2);
    assert_eq!(hist[0].1, RULESET_V1);
    assert_eq!(hist[1].1, RULESET_V2);
}

// ---------------------------------------------------------------- step 6

#[test]
fn the_db_file_lands_in_the_user_data_dir_not_the_round_root() {
    // 显式环境变量（测试与运维的优先入口）指向临时目录
    let dir = std::env::temp_dir().join("psychdoc-store-path-test");
    std::env::set_var("PSYCHDOC_DATA_DIR", &dir);
    let resolved = paths::db_path().unwrap();
    std::env::remove_var("PSYCHDOC_DATA_DIR");

    assert!(resolved.starts_with(&dir));
    assert_eq!(resolved.file_name().unwrap(), "psychdoc.sqlite3");

    // 关键不变量：绝不落在 round 根 / 项目子目录
    let cwd = std::env::current_dir().unwrap();
    assert!(!resolved.starts_with(&cwd), "库文件不得落在当前工作目录下");
    assert!(resolved
        .to_string_lossy()
        .contains("psychdoc.sqlite3"));

    // 落盘后能重新打开，数据仍在
    let store = Store::open(&resolved).unwrap();
    store
        .record_review(&review_input(
            "run-001",
            RULESET_V1,
            "2026-03-01T09:00:00+08:00",
            &[diag("WARD-A-001-CONSENT-01", "QC-PSY-01")],
        ))
        .unwrap();
    assert_eq!(store.path().unwrap(), resolved.as_path());
    drop(store);

    let reopened = Store::open(&resolved).unwrap();
    assert_eq!(reopened.review_run_count().unwrap(), 1);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_desensitized_alias_survives_the_round_trip_untouched() {
    let store = Store::open_in_memory().unwrap();
    let docs = ["WARD-A-001-CONSENT-01"];
    let es = entries(&docs, "hash-v1");
    store
        .create_draft("draft-1", ALIAS, &es, "护士长A", "2026-03-02T09:00:00+08:00")
        .unwrap();
    store
        .set_selected("draft-1", docs[0], true, "2026-03-02T09:05:00+08:00")
        .unwrap();
    store
        .commit_writeback("draft-1", &es, "护士长A", "2026-03-02T09:10:00+08:00", RULESET_V1)
        .unwrap();
    let (alias, doc): (String, String) = store
        .conn()
        .query_row("SELECT encounter_alias, doc_id FROM writeback_log", [], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })
        .unwrap();
    assert_eq!(alias, ALIAS);
    assert_eq!(doc, docs[0]);
}

#[test]
fn every_error_code_is_stable_for_the_error_table() {
    let store = Store::open_in_memory().unwrap();
    // 错误码是 Task 37「错误码表 ↔ 代码实际返回码一一对应」的测试基线，
    // 这里先钉住存储层自己产出的那几类。
    let cases: Vec<(StoreError, &str)> = vec![
        (
            StoreError::StateUnknown("x".into()),
            "E_STATE_UNKNOWN",
        ),
        (
            StoreError::jump("d", DocState::Missing, DocState::Archivable),
            "E_STATE_JUMP",
        ),
        (StoreError::DraftUnknown("d".into()), "E_DRAFT_UNKNOWN"),
        (
            StoreError::NothingSelected("d".into()),
            "E_NOTHING_SELECTED",
        ),
        (StoreError::DataDirUnresolved, "E_DATA_DIR"),
    ];
    for (err, code) in cases {
        assert_eq!(err.code(), code);
        assert!(
            err.to_string().starts_with(code),
            "错误文案必须自带错误码：{err}"
        );
    }
    // 真跑一次拿一个
    let err = store
        .commit_writeback(
            "ghost",
            &[],
            "x",
            "t",
            RULESET_V1,
        )
        .unwrap_err();
    assert_eq!(err.code(), "E_DRAFT_UNKNOWN");
}
