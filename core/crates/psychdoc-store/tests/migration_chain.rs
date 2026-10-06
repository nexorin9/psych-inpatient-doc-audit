//! 迁移链的端到端用例：空库起步、重复迁移幂等、中断回滚、版本过高校验。
//!
//! 这里的每个失败用例都在构造**真实会发生的院内事故**，不是为覆盖率凑的：
//!
//! - 「迁移执行到一半断掉」= 迁移脚本第 2 条 DDL 报错（磁盘满、SQL 写错）；
//! - 「后置断言不通过」= DDL 跑通了但少建了一列（有人改了 DDL 忘了改断言）；
//! - 「前置断言不通过」= 库被手工改坏或从备份恢复不完整；
//! - 「库版本高于程序」= 护士长先升级了新版本，第二天回滚装了旧版本。
//!
//! 三种失败都必须回答同一个问题：**磁盘上那份库还能不能用？**
//! 答案恒为「能，且版本号没动」——用旧版本打开不该丢数据，用旧版本
//! 打开不该拿到半张表。

use psychdoc_model::DocItem;
use psychdoc_store::schema::{self, V1_DDL, V2_DDL};
use psychdoc_store::{Store, StoreError, SCHEMA_VERSION_LATEST};

fn sample_item(doc_id: &str) -> DocItem {
    let mut it = DocItem::new(doc_id, "CONSENT_FORM", "WARD-A-001");
    it.source_system = "emr".into();
    it
}

/// 测试用的表形状（`Migration` 的断言字段是 `&'static`，只能引用常量）。
const DOC_ID_SPEC: &[schema::TableSpec] = &[schema::TableSpec::new("doc_items", &["doc_id"])];
const HALF_BUILT_SPEC: &[schema::TableSpec] = &[schema::TableSpec::new("half_built", &["x"])];
const NARROW_CACHE_SPEC: &[schema::TableSpec] =
    &[schema::TableSpec::new("review_cache", &["payload_json_narrow"])];

fn table_count(conn: &rusqlite::Connection, table: &str) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
        [table],
        |r| r.get(0),
    )
    .unwrap()
}

fn tmp_db(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("psychdoc-migrate-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("psychdoc.sqlite3")
}

// ------------------------------------------------------------------ 正常路径

#[test]
fn an_empty_database_walks_the_chain_one_step_at_a_time() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    assert_eq!(schema::schema_version(&conn).unwrap(), 0, "空库版本号应为 0");

    let report = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();

    assert_eq!(report.from_version, 0);
    assert_eq!(report.to_version, SCHEMA_VERSION_LATEST);
    assert_eq!(
        report.applied.iter().map(|m| m.version).collect::<Vec<_>>(),
        (1..=SCHEMA_VERSION_LATEST).collect::<Vec<_>>(),
        "每一步都要按序应用，不能跳"
    );
    for table in [
        "doc_items",
        "doc_states",
        "review_runs",
        "diagnostics",
        "writeback_drafts",
        "writeback_draft_items",
        "writeback_log",
        "audit_events",
        "review_cache",
        "writeback_claims",
        "sync_cursors",
    ] {
        assert_eq!(table_count(&conn, table), 1, "缺表 {table}");
    }
}

#[test]
fn the_report_says_what_it_did_in_words_a_clerk_can_read() {
    // 迁移报错最终会出现在护士长截图里，名字得是人话。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let report = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    let names = report.applied_names_zh();
    assert_eq!(names.len() as i64, SCHEMA_VERSION_LATEST);
    for name in names {
        assert!(
            name.chars().any(|c| c > '\u{7f}'),
            "迁移名应为中文，便于截图排障：{name}"
        );
    }
}

#[test]
fn migrating_twice_applies_nothing_the_second_time() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();

    let before = table_count(&conn, "doc_items");
    let again = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();

    assert!(again.is_noop(), "已是当前版本就不该再跑一遍");
    assert_eq!(again.from_version, SCHEMA_VERSION_LATEST);
    assert_eq!(again.to_version, SCHEMA_VERSION_LATEST);
    assert_eq!(
        schema::schema_version(&conn).unwrap(),
        SCHEMA_VERSION_LATEST,
        "重复迁移不得改版本号"
    );
    assert_eq!(table_count(&conn, "doc_items"), before);
}

#[test]
fn asking_for_a_step_the_database_already_passed_is_a_noop_not_an_error() {
    // 排障时会说「先单独跑一下 v1 试试」——库已在 v2，这句话不能变成报错。
    let store = Store::open_in_memory().unwrap();
    let report = schema::migrate_to_v1(store.conn()).unwrap();
    assert!(report.is_noop());
    let report = schema::migrate_to_v2(store.conn()).unwrap();
    assert!(report.is_noop());
    assert_eq!(
        psychdoc_store::schema_version(store.conn()).unwrap(),
        SCHEMA_VERSION_LATEST
    );
}

#[test]
fn the_reports_of_two_identical_runs_are_identical() {
    // 启动日志要能 diff：同一个库、同一套迁移，两次跑出来的报告逐字段相同。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    let a = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    let b = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    assert_eq!(a, b);
}

// ------------------------------------------------------------------ 回滚路径

/// 建一条「v1 正常、v2 中途失败」的链，用来验证回滚。
fn chain_where_v2_ddl_fails() -> Vec<schema::Migration> {
    vec![
        schema::Migration {
            version: 1,
            name: "v1 初始",
            ddl: V1_DDL,
            expect_before: &[],
            expect_after: DOC_ID_SPEC,
        },
        schema::Migration {
            version: 2,
            name: "v2 迁移到一半失败",
            // 第 1 条 DDL 能建出半张表，第 2 条必然报错——这就是磁盘满 /
            // SQL 写错时的现场：一条批处理执行到中间停下。
            ddl: "CREATE TABLE half_built (x INTEGER);\nCREATE TABLE half_built (x INTEGER);",
            expect_before: DOC_ID_SPEC,
            expect_after: HALF_BUILT_SPEC,
        },
    ]
}

#[test]
fn an_interrupted_migration_leaves_the_previous_schema_usable() {
    // 空库起步：v1 建成功、v2 执行到一半炸掉。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let err = schema::apply_migrations(&conn, &chain_where_v2_ddl_fails()).unwrap_err();

    assert_eq!(err.code(), "E_MIGRATION_FAILED");
    let msg = err.to_string();
    assert!(msg.contains("v2"), "报错要点名是第几步：{msg}");
    assert!(msg.contains("迁移到一半失败"), "报错要带中文迁移名：{msg}");
    assert!(msg.contains("已整体回滚"), "报错要说明已回滚：{msg}");

    assert_eq!(
        table_count(&conn, "half_built"),
        0,
        "回滚后连半张表都不该留在磁盘上"
    );
    assert_eq!(
        schema::schema_version(&conn).unwrap(),
        1,
        "失败的那一步不得推进版本号"
    );
}

#[test]
fn the_schema_that_survived_a_failed_migration_still_takes_writes() {
    // 「回滚了」不等于「还能用」：这里真写一行进去，证明 v1 的库没有被
    // 回滚搞成只读或半损坏。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let _ = schema::apply_migrations(&conn, &chain_where_v2_ddl_fails()).unwrap_err();

    conn.execute(
        "INSERT INTO doc_items
           (doc_id, template_id, encounter_alias, source_system,
            content_sha256, binding, first_seen_at, last_seen_at)
         VALUES ('WARD-A-001-CONSENT-01','restraint_consent','WARD-A-001','his',
                 'h1','Confident','2026-03-02T09:00:00+08:00','2026-03-02T09:00:00+08:00')",
        [],
    )
    .unwrap();
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM doc_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "回滚后的旧库仍可正常写入");

    // 重跑同一条坏迁移仍是失败且仍不动数据——不是「失败一次就放行」。
    let err = schema::apply_migrations(&conn, &chain_where_v2_ddl_fails()).unwrap_err();
    assert_eq!(err.code(), "E_MIGRATION_FAILED");
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM doc_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1);
}

#[test]
fn a_post_migration_assertion_mismatch_rolls_the_whole_step_back() {
    // DDL 跑通了，但少建了一列（有人改了 DDL 忘了同步断言）。
    // 这种半成品库比「迁移失败」更危险：下一次启动会认为已经是最新版。
    let conn = rusqlite::Connection::open_in_memory().unwrap();

    let chain = vec![
        schema::Migration {
            version: 1,
            name: "v1 初始",
            ddl: V1_DDL,
            expect_before: &[],
            expect_after: DOC_ID_SPEC,
        },
        schema::Migration {
            version: 2,
            name: "v2 建表却漏了一列",
            ddl: V2_DDL,
            expect_before: DOC_ID_SPEC,
            // review_cache 真实列里没有 payload_json_narrow 这一列。
            expect_after: NARROW_CACHE_SPEC,
        },
    ];

    let err = schema::apply_migrations(&conn, &chain).unwrap_err();
    assert_eq!(err.code(), "E_MIGRATION_FAILED");
    let msg = err.to_string();
    assert!(msg.contains("后置断言不通过"), "{msg}");
    assert!(msg.contains("缺关键列"), "{msg}");
    assert!(msg.contains("payload_json_narrow"), "要说清缺哪一列：{msg}");

    assert_eq!(
        schema::schema_version(&conn).unwrap(),
        1,
        "断言不过就不许推进版本号"
    );
    assert_eq!(
        table_count(&conn, "review_cache"),
        0,
        "后置断言不过，已建出来的表也必须一起回滚"
    );
}

#[test]
fn a_pre_migration_assertion_stops_a_step_on_a_broken_database() {
    // 备份恢复不完整 / 有人手工删过列：版本号写着 v1，实际表是残的。
    // 这时继续建 v2 只会把残缺的库固化下来。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch(
        "CREATE TABLE doc_items (doc_id TEXT PRIMARY KEY);
         CREATE TABLE review_runs (run_id TEXT PRIMARY KEY);
         PRAGMA user_version = 1;",
    )
    .unwrap();

    let err = schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap_err();
    assert_eq!(err.code(), "E_MIGRATION_FAILED");
    let msg = err.to_string();
    assert!(msg.contains("前置断言不通过"), "{msg}");
    assert!(msg.contains("encounter_alias"), "要说清缺哪一列：{msg}");
    assert_eq!(
        table_count(&conn, "review_cache"),
        0,
        "前置断言不过，v2 一条 DDL 都不许跑"
    );
    assert_eq!(schema::schema_version(&conn).unwrap(), 1);
}

// ------------------------------------------------------------------ 版本过新

#[test]
fn a_database_newer_than_the_program_is_refused_and_left_untouched() {
    // 护士长先升级到新版，第二天机器回滚装了旧版。旧代码不认识新表，
    // 照写会把数据写坏——所以必须直接拒绝启动。
    let path = tmp_db("too-new");
    {
        let store = Store::open(&path).unwrap();
        store
            .register_items(
                &[sample_item("WARD-A-001-CONSENT-01")],
                "2026-03-02T09:00:00+08:00",
            )
            .unwrap();
        store.conn().pragma_update(None, "user_version", 99).unwrap();
    }

    let err = match Store::open(&path) {
        Ok(_) => panic!("库版本高于程序时必须拒绝启动"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "E_SCHEMA_TOO_NEW");
    let msg = err.to_string();
    assert!(msg.contains("v99"), "{msg}");
    assert!(
        msg.contains(&format!("v{SCHEMA_VERSION_LATEST}")),
        "要同时说出程序支持到哪一版：{msg}"
    );
    assert!(
        msg.contains("不退化为只读"),
        "只读也会假装「今天没缺件」，必须说清不退化：{msg}"
    );

    // 拒绝启动 = 一个字节都不许写：版本号、表、行数全部原样。
    let conn = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(schema::schema_version(&conn).unwrap(), 99);
    assert_eq!(table_count(&conn, "review_cache"), 1);
    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM doc_items", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 1, "拒启不得影响既有数据");
    drop(conn);
    let _ = std::fs::remove_dir_all(path.parent().unwrap());
}

#[test]
fn the_startup_check_alone_already_refuses_before_any_write_happens() {
    // 只读的自检（不碰 DDL）就要能拒掉：调用方拿它做「先问一句」。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    schema::startup_check(&conn).unwrap();

    conn.pragma_update(None, "user_version", 100).unwrap();
    let err = schema::startup_check(&conn).unwrap_err();
    assert!(matches!(err, StoreError::SchemaTooNew { .. }));
    assert_eq!(err.code(), "E_SCHEMA_TOO_NEW");
    // 已经在 v1/v2 的库不该被自检拦下。
    conn.pragma_update(None, "user_version", 1).unwrap();
    schema::startup_check(&conn).unwrap();
}

// ------------------------------------------------------------------ 链本身

#[test]
fn the_shipped_chain_has_no_gap_in_its_version_numbers() {
    // 加了 v4 忘了补 v3，链里就会缺号；启动时必须炸，而不是停在旧版。
    let chain = vec![
        *schema::MIGRATIONS.first().unwrap(),
        schema::Migration {
            version: 4,
            name: "跳号的那一条",
            ddl: V1_DDL,
            expect_before: &[],
            expect_after: &[],
        },
    ];
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let err = schema::apply_migrations(&conn, &chain).unwrap_err();
    assert_eq!(err.code(), "E_MIGRATION_FAILED");
    assert!(err.to_string().contains("迁移链定义不连续"), "{err}");
    assert_eq!(
        schema::schema_version(&conn).unwrap(),
        0,
        "链定义不过，一条迁移都不许跑"
    );
}

#[test]
fn every_shipped_migration_asserts_the_tables_it_claims_to_create() {
    // 断言与 DDL 漂移是这类代码最常见的腐化：改了 DDL 忘了加断言，
    // 于是「后置断言」永远是一句空话。这里把两条对起来。
    for m in schema::MIGRATIONS {
        assert!(
            !m.expect_after.is_empty(),
            "v{} 没写后置断言，等于没断言",
            m.version
        );
        for spec in m.expect_after {
            assert!(
                m.ddl.contains(spec.table),
                "v{} 断言了表 {}，但 DDL 里没有它",
                m.version,
                spec.table
            );
        }
        if m.version > 1 {
            assert!(
                !m.expect_before.is_empty(),
                "v{} 不是首步，必须写前置断言",
                m.version
            );
        }
    }
}

// ------------------------------------------------------------------ v4 增量读游标

#[test]
fn an_existing_v3_database_gains_the_cursor_table_on_upgrade() {
    // 真实场景：护士长的机器上已经跑了几个月 v3，本轮升级后才第一次出现
    // `sync_cursors`。老库必须有这张表，否则增量拉取一启动就 "no such table"。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let report = schema::migrate_to_v3(&conn).unwrap();
    assert_eq!(report.to_version, 3);
    assert_eq!(table_count(&conn, "sync_cursors"), 0, "v3 库里不该有这张表");

    let report = schema::migrate(&conn).unwrap();
    assert_eq!(report.to_version, SCHEMA_VERSION_LATEST);
    assert_eq!(table_count(&conn, "sync_cursors"), 1);
    assert!(
        report.applied_names_zh().iter().any(|n| n.contains("游标")),
        "迁移名要能在排障截图里看出加了游标表：{report:?}"
    );

    // 升级后老数据仍完好：v1 建的表还能写
    let store = Store::open_in_memory().unwrap();
    store
        .register_items(
            &[sample_item("WARD-A-001-RECORD-01")],
            "2026-03-03T09:00:00+08:00",
        )
        .unwrap();
    assert_eq!(store.item_count().unwrap(), 1);
}

#[test]
fn the_cursor_table_refuses_a_second_cursor_for_the_same_tenant_and_resource() {
    // 主键是 (tenant_id, resource_type)：同租户同端点只能有一条水位线。
    // 出现两条就意味着「推进」与「记录」走的是两条路径，早晚会互相覆盖。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    conn.execute(
        "INSERT INTO sync_cursors (tenant_id, resource_type, since, pages_done)
         VALUES ('ward-a', 'Composition', '2026-03-03T09:01:00+08:00', 1)",
        [],
    )
    .unwrap();

    let err = conn.execute(
        "INSERT INTO sync_cursors (tenant_id, resource_type, since, pages_done)
         VALUES ('ward-a', 'Composition', '2026-03-03T09:02:00+08:00', 1)",
        [],
    );
    assert!(
        matches!(
            err,
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "同租户同端点插第二条水位线必须被唯一约束挡下：{err:?}"
    );

    // 换个租户则可以共存——多租户部署下各拉各的
    conn.execute(
        "INSERT INTO sync_cursors (tenant_id, resource_type, since, pages_done)
         VALUES ('ward-b', 'Composition', '2026-03-02T23:00:00+08:00', 1)",
        [],
    )
    .unwrap();

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM sync_cursors", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}

// ------------------------------------------------------------------ v5 通知发件箱

#[test]
fn an_existing_v4_database_gains_the_outbox_table_on_upgrade() {
    // 真实场景：护士长机器上已跑了几个月 v4，本轮升级后才第一次出现
    // `notification_outbox`。老库必须有这张表，否则缺件提醒一入队就
    // "no such table"——而这条报错发生在发送之前，提醒直接丢了。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    let report = schema::migrate_to_v4(&conn).unwrap();
    assert_eq!(report.to_version, 4);
    assert_eq!(table_count(&conn, "notification_outbox"), 0, "v4 库里不该有这张表");

    let report = schema::migrate(&conn).unwrap();
    assert_eq!(report.to_version, SCHEMA_VERSION_LATEST);
    assert_eq!(table_count(&conn, "notification_outbox"), 1);
    assert!(
        report.applied_names_zh().iter().any(|n| n.contains("发件箱")),
        "迁移名要能在排障截图里看出加了发件箱表：{report:?}"
    );

    // 升级后老数据仍完好
    let store = Store::open_in_memory().unwrap();
    store
        .register_items(
            &[sample_item("WARD-A-001-CONSENT-01")],
            "2026-03-03T09:00:00+08:00",
        )
        .unwrap();
    assert_eq!(store.item_count().unwrap(), 1);
}

#[test]
fn the_outbox_key_refuses_a_second_row_for_the_same_item_and_business_day() {
    // 主键是 notify_key：同一天同一件只能有一条。出现两条就意味着入队
    // 走了两条路径，群里会收到两条一模一样的提醒。
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    schema::apply_migrations(&conn, schema::MIGRATIONS).unwrap();
    conn.execute(
        "INSERT INTO notification_outbox
            (notify_key, encounter_alias, doc_id, business_day, channel, payload_json,
             state, attempts, next_attempt_at, created_at)
         VALUES ('k1', 'WARD-A-001', 'CONSENT-01', '2026-03-03', 'wecom_markdown',
                 '{}', 'pending', 0, '2026-03-03T09:00:00+08:00', '2026-03-03T09:00:00+08:00')",
        [],
    )
    .unwrap();

    let err = conn.execute(
        "INSERT INTO notification_outbox
            (notify_key, encounter_alias, doc_id, business_day, channel, payload_json,
             state, attempts, next_attempt_at, created_at)
         VALUES ('k1', 'WARD-A-001', 'CONSENT-01', '2026-03-03', 'wecom_markdown',
                 '{}', 'pending', 0, '2026-03-03T09:00:00+08:00', '2026-03-03T09:00:00+08:00')",
        [],
    );
    assert!(
        matches!(
            err,
            Err(rusqlite::Error::SqliteFailure(e, _))
                if e.code == rusqlite::ErrorCode::ConstraintViolation
        ),
        "同键同业务日插第二条必须被唯一约束挡下：{err:?}"
    );

    // 换业务日则可以共存——跨零点是新交班日，护士长期望再被提醒一次
    conn.execute(
        "INSERT INTO notification_outbox
            (notify_key, encounter_alias, doc_id, business_day, channel, payload_json,
             state, attempts, next_attempt_at, created_at)
         VALUES ('k2', 'WARD-A-001', 'CONSENT-01', '2026-03-04', 'wecom_markdown',
                 '{}', 'pending', 0, '2026-03-04T07:00:00+08:00', '2026-03-04T07:00:00+08:00')",
        [],
    )
    .unwrap();

    let n: i64 = conn
        .query_row("SELECT COUNT(*) FROM notification_outbox", [], |r| r.get(0))
        .unwrap();
    assert_eq!(n, 2);
}
