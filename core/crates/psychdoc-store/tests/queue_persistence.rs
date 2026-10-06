//! Task 32 · 审查队列的持久化契约。
//!
//! 这一条产品纪律是：**常驻工具关掉再开，护士长不该把整个病区文件夹
//! 重新拖一遍，更不该丢掉「上午已审到第 8 次住院」这个进度。**
//!
//! 因此这里钉死四件事：
//!
//! 1. 队列落本机 sqlite，**关窗重开仍在**（含条目顺序与已审时刻）；
//! 2. 库文件落**用户数据目录**，不落 round 根 / 项目子目录；
//! 3. 同一文件夹二次载入 = **两次独立队列**（进度各留一份，不互相顶掉）；
//! 4. 重扫更新计数与规则集版本，但**不清**已审进度；清进度是另一个显式动作。

use std::path::PathBuf;

use psychdoc_store::{
    clear_reviewed, delete_queue, latest_queue, list_queue_ids, load_queue, mark_reviewed,
    merge_reviewed_at, queue_id, reviewed_progress, save_queue, QueueEntryRow, QueueSession,
    QueueSnapshot, Store, StoreError,
};

/// 每个用例一个独立库文件；返回 (Store, 库路径)。Drop 时删目录。
struct Scratch {
    dir: PathBuf,
    db: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> (Self, Store) {
        let dir = std::env::temp_dir().join(format!("psychdoc-queue-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = dir.join("psychdoc.sqlite3");
        let store = Store::open(&db).unwrap();
        (Scratch { dir, db }, store)
    }

    fn reopen(&self) -> Store {
        Store::open(&self.db).unwrap()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn entry(alias: &str, doc_count: usize, missing: usize, position: usize) -> QueueEntryRow {
    QueueEntryRow {
        encounter_alias: alias.into(),
        ward: psychdoc_parse::ward_of(alias),
        doc_count,
        missing_count: missing,
        reviewed_at: None,
        position,
    }
}

fn snapshot(
    queue_id: &str,
    source: &str,
    loaded_at: &str,
    entries: Vec<QueueEntryRow>,
) -> QueueSnapshot {
    let (doc_total, missing_total) = entries
        .iter()
        .fold((0usize, 0usize), |(d, m), e| (d + e.doc_count, m + e.missing_count));
    QueueSnapshot {
        session: QueueSession {
            queue_id: queue_id.into(),
            source_path: source.into(),
            loaded_at: loaded_at.into(),
            ruleset_short_sha256: "e4eda0c79c95".into(),
            packages_found: entries.len(),
            files_seen: entries.len() + 3,
            items_total: doc_total,
            missing_total,
            file_skips_json: r#"[{"relative_path":"杂项.pdf","reason_zh":"不是文书包文件"}]"#.into(),
            last_scanned_at: loaded_at.into(),
        },
        entries,
    }
}

// ---------------------------------------------------------------------------
// 1. 关窗重开：队列、顺序、已审进度都在
// ---------------------------------------------------------------------------

#[test]
fn the_queue_survives_closing_and_reopening_the_window() {
    let (scratch, store) = Scratch::new("survive");
    let snap = snapshot(
        "Q1",
        "/export/ward-a",
        "2026-03-02T08:00:00+08:00",
        vec![
            entry("WARD-A-001", 4, 2, 0),
            entry("WARD-A-002", 3, 0, 1),
            entry("WARD-A-003", 2, 0, 2),
        ],
    );
    assert_eq!(save_queue(store.conn(), &snap).unwrap(), 3);
    mark_reviewed(
        store.conn(),
        "Q1",
        &["WARD-A-001".into(), "WARD-A-002".into()],
        "2026-03-02T09:30:00+08:00",
    )
    .unwrap();
    drop(store);

    // 重开（模拟关窗再开）
    let store = scratch.reopen();
    let back = load_queue(store.conn(), "Q1").unwrap().expect("队列不能丢");
    assert_eq!(back.entries.len(), 3);
    // 左栏顺序原样还原：护士长已经按这个顺序走了一遍病区
    assert_eq!(
        back.entries.iter().map(|e| e.encounter_alias.as_str()).collect::<Vec<_>>(),
        vec!["WARD-A-001", "WARD-A-002", "WARD-A-003"]
    );
    assert_eq!(back.entries[0].missing_count, 2);
    assert_eq!(back.session.missing_total, 2);
    // 已审进度也在
    assert_eq!(reviewed_progress(store.conn(), "Q1").unwrap(), (2, 3));
    assert_eq!(
        back.entries[0].reviewed_at.as_deref(),
        Some("2026-03-02T09:30:00+08:00")
    );
    assert_eq!(back.entries[2].reviewed_at, None);
    // 「已跳过」清单原样存原样取
    assert!(back.session.file_skips_json.contains("杂项.pdf"));
}

#[test]
fn the_store_file_lands_in_the_given_data_dir_not_the_working_directory() {
    let (scratch, store) = Scratch::new("dbpath");
    let path = store.path().expect("落盘库要有路径").to_path_buf();
    assert!(path.starts_with(&scratch.dir), "库必须落在指定的数据目录：{}", path.display());
    let cwd = std::env::current_dir().unwrap();
    assert!(
        !path.starts_with(&cwd),
        "库不能落在当前工作目录（round 根 / 项目子目录）：{}",
        path.display()
    );
    assert!(path.file_name().unwrap() == "psychdoc.sqlite3");
}

// ---------------------------------------------------------------------------
// 2. 同一文件夹二次载入 = 两次队列（不是覆盖）
// ---------------------------------------------------------------------------

#[test]
fn loading_the_same_folder_twice_creates_two_queues_instead_of_overwriting() {
    let (_scratch, store) = Scratch::new("twoloads");
    let source = "/export/ward-a";
    let morning = queue_id(source, "2026-03-02T08:00:00+08:00");
    let afternoon = queue_id(source, "2026-03-02T14:00:00+08:00");
    assert_ne!(morning, afternoon);

    save_queue(
        store.conn(),
        &snapshot(&morning, source, "2026-03-02T08:00:00+08:00", vec![entry("WARD-A-001", 4, 2, 0)]),
    )
    .unwrap();
    mark_reviewed(store.conn(), &morning, &["WARD-A-001".into()], "2026-03-02T09:00:00+08:00").unwrap();

    // 下午补导出几件再拖一次
    save_queue(
        store.conn(),
        &snapshot(
            &afternoon,
            source,
            "2026-03-02T14:00:00+08:00",
            vec![entry("WARD-A-001", 6, 0, 0), entry("WARD-A-004", 2, 1, 1)],
        ),
    )
    .unwrap();

    // 两条都还在，上午那条的进度没被顶掉
    assert_eq!(load_queue(store.conn(), &morning).unwrap().unwrap().entries[0].missing_count, 2);
    assert_eq!(reviewed_progress(store.conn(), &morning).unwrap(), (1, 1));
    let pm = load_queue(store.conn(), &afternoon).unwrap().unwrap();
    assert_eq!(pm.entries.len(), 2);
    assert_eq!(reviewed_progress(store.conn(), &afternoon).unwrap(), (0, 2));

    // 重开窗口回到**最近**那一次（护士长最后看到的那次）
    let latest = latest_queue(store.conn()).unwrap().expect("应有最近队列");
    assert_eq!(latest.session.queue_id, afternoon);
    assert_eq!(latest.entries.len(), 2);
}

// ---------------------------------------------------------------------------
// 3. 重扫：更新计数与规则集版本，但不清进度
// ---------------------------------------------------------------------------

#[test]
fn a_rescan_updates_the_counts_but_keeps_the_reviewed_progress() {
    let (_scratch, store) = Scratch::new("rescan");
    let source = "/export/ward-a";
    let id = queue_id(source, "2026-03-02T08:00:00+08:00");
    save_queue(
        store.conn(),
        &snapshot(&id, source, "2026-03-02T08:00:00+08:00", vec![
            entry("WARD-A-001", 4, 2, 0),
            entry("WARD-A-002", 3, 0, 1),
        ]),
    )
    .unwrap();
    mark_reviewed(store.conn(), &id, &["WARD-A-001".into()], "2026-03-02T09:00:00+08:00").unwrap();

    // 病区补导出几件后重扫：多了一次住院、缺件补齐了、规则集也换了版本
    let mut rescanned = snapshot(
        &id,
        source,
        "2026-03-02T08:00:00+08:00",
        vec![
            entry("WARD-A-001", 4, 0, 0),
            entry("WARD-A-002", 3, 0, 1),
            entry("WARD-A-005", 2, 1, 2),
        ],
    );
    rescanned.session.ruleset_short_sha256 = "77cf76b8f383".into();
    rescanned.session.last_scanned_at = "2026-03-02T11:00:00+08:00".into();

    let kept = merge_reviewed_at(store.conn(), &mut rescanned).unwrap();
    assert_eq!(kept, 1, "已审过的那次住院要保住进度");
    save_queue(store.conn(), &rescanned).unwrap();

    let back = load_queue(store.conn(), &id).unwrap().unwrap();
    assert_eq!(back.entries.len(), 3, "新出现的住院要进来");
    assert_eq!(back.entries[0].missing_count, 0, "缺件补齐后计数要更新");
    assert_eq!(
        back.entries[0].reviewed_at.as_deref(),
        Some("2026-03-02T09:00:00+08:00"),
        "重扫不是把上午的结论作废"
    );
    assert_eq!(back.entries[2].reviewed_at, None, "新住院没审过");
    assert_eq!(back.session.ruleset_short_sha256, "77cf76b8f383");
    assert_eq!(back.session.last_scanned_at, "2026-03-02T11:00:00+08:00");
    assert_eq!(reviewed_progress(store.conn(), &id).unwrap(), (1, 3));
}

#[test]
fn clearing_the_progress_keeps_the_queue_itself() {
    let (_scratch, store) = Scratch::new("clear");
    let id = "Q1";
    save_queue(
        store.conn(),
        &snapshot(id, "/e", "2026-03-02T08:00:00+08:00", vec![
            entry("WARD-A-001", 4, 2, 0),
            entry("WARD-A-002", 3, 0, 1),
        ]),
    )
    .unwrap();
    mark_reviewed(store.conn(), id, &["WARD-A-001".into()], "2026-03-02T09:00:00+08:00").unwrap();

    assert_eq!(clear_reviewed(store.conn(), id).unwrap(), 1);
    let back = load_queue(store.conn(), id).unwrap().unwrap();
    // 「重来一遍」指的是重审，不是清空住院列表
    assert_eq!(back.entries.len(), 2);
    assert!(back.entries.iter().all(|e| e.reviewed_at.is_none()));
    assert_eq!(reviewed_progress(store.conn(), id).unwrap(), (0, 2));
    // 再清一次是 0 条而不是报错
    assert_eq!(clear_reviewed(store.conn(), id).unwrap(), 0);
}

// ---------------------------------------------------------------------------
// 4. 拒绝静默：空队列、没勾选、别名不存在
// ---------------------------------------------------------------------------

#[test]
fn an_empty_queue_is_refused_rather_than_saved() {
    let (_scratch, store) = Scratch::new("empty");
    let s = snapshot("Q1", "/e", "2026-03-02T08:00:00+08:00", vec![]);
    match save_queue(store.conn(), &s) {
        Err(StoreError::QueueEmpty) => {}
        other => panic!("空队列应被 E_QUEUE_EMPTY 拒住，实际 {other:?}"),
    }
    // 一条都没落：重开后的左栏不会莫名其妙空着
    assert!(latest_queue(store.conn()).unwrap().is_none());
}

#[test]
fn marking_nothing_or_an_unknown_encounter_is_refused_not_silently_skipped() {
    let (_scratch, store) = Scratch::new("refuse");
    save_queue(
        store.conn(),
        &snapshot("Q1", "/e", "2026-03-02T08:00:00+08:00", vec![entry("WARD-A-001", 4, 0, 0)]),
    )
    .unwrap();

    match mark_reviewed(store.conn(), "Q1", &[], "2026-03-02T09:00:00+08:00") {
        Err(StoreError::QueueNothingSelected) => {}
        other => panic!("未勾选任何住院应被拒住，实际 {other:?}"),
    }
    match mark_reviewed(
        store.conn(),
        "Q1",
        &["WARD-A-001".into(), "WARD-Z-999".into()],
        "2026-03-02T09:00:00+08:00",
    ) {
        // 别名不存在 → 整批拒（事务回滚），而不是插一行从没审过的住院
        Err(StoreError::QueueUnknownEncounter(a)) => assert_eq!(a, "WARD-Z-999"),
        other => panic!("未知别名应被 E_QUEUE_UNKNOWN_ENCOUNTER 拒住，实际 {other:?}"),
    }
    // 整批拒 ⇒ 一条都没标上
    assert_eq!(reviewed_progress(store.conn(), "Q1").unwrap(), (0, 1));
}

// ---------------------------------------------------------------------------
// 5. 会话列表与删除
// ---------------------------------------------------------------------------

#[test]
fn queue_ids_are_listed_newest_first_and_deleting_removes_the_entries_too() {
    let (_scratch, store) = Scratch::new("list");
    for (i, at) in [
        "2026-03-02T08:00:00+08:00",
        "2026-03-02T14:00:00+08:00",
        "2026-03-03T09:00:00+08:00",
    ]
    .iter()
    .enumerate()
    {
        let id = queue_id("/export", at);
        save_queue(store.conn(), &snapshot(&id, "/export", at, vec![entry("A-1", 1, 0, 0)])).unwrap();
        assert_eq!(list_queue_ids(store.conn(), 10).unwrap().len(), i + 1);
    }
    let ids = list_queue_ids(store.conn(), 10).unwrap();
    assert_eq!(ids[0], queue_id("/export", "2026-03-03T09:00:00+08:00"), "最新在前");

    // 删除连带条目，不留孤儿行
    let victim = ids[1].clone();
    assert_eq!(delete_queue(store.conn(), &victim).unwrap(), 1);
    assert!(load_queue(store.conn(), &victim).unwrap().is_none());
    let remaining: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM queue_entries WHERE queue_id = ?1",
            [&victim],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(remaining, 0, "删会话不能留下孤儿条目行");
}

// ---------------------------------------------------------------------------
// 6. 落库失败的两种形态都不静默
// ---------------------------------------------------------------------------

#[test]
fn an_entry_cannot_reference_a_queue_that_was_never_saved() {
    let (_scratch, store) = Scratch::new("orphan");
    // 外键硬约束：条目必须挂在已存在的会话上，
    // 否则重开后的左栏会多出一行「不知道哪次载入带来的住院」
    let r = store.conn().execute(
        "INSERT INTO queue_entries
           (queue_id, encounter_alias, ward, doc_count, missing_count, reviewed_at, position)
         VALUES ('GHOST','WARD-A-001','',1,0,NULL,0)",
        [],
    );
    assert!(r.is_err(), "孤儿条目行必须被外键拒住");
}

#[test]
fn a_duplicated_alias_is_refused_by_name_rather_than_silently_last_wins() {
    let (_scratch, store) = Scratch::new("dupe");
    // 聚合按住院别名分组后不该出现重复；真出现了说明上游出了问题。
    // 静默「后一条覆盖前一条」会让左栏少一次住院且没人知道少了哪次。
    match save_queue(
        store.conn(),
        &snapshot("Q1", "/e", "2026-03-02T08:00:00+08:00", vec![
            entry("WARD-A-001", 4, 0, 0),
            entry("WARD-A-001", 9, 0, 1),
        ]),
    ) {
        Err(StoreError::QueueDuplicateEncounter(a)) => assert_eq!(a, "WARD-A-001"),
        other => panic!("重复别名应被显式拒住并点名，实际 {other:?}"),
    }
    // 整批拒 ⇒ 什么都没落
    assert!(latest_queue(store.conn()).unwrap().is_none());
}

#[test]
fn re_saving_the_same_queue_replaces_its_entries_instead_of_appending() {
    let (_scratch, store) = Scratch::new("replace");
    save_queue(
        store.conn(),
        &snapshot("Q1", "/e", "2026-03-02T08:00:00+08:00", vec![
            entry("WARD-A-001", 4, 0, 0),
            entry("WARD-A-002", 3, 0, 1),
        ]),
    )
    .unwrap();
    // 同一 queue_id 再落一次（重扫）：条目整批换掉，不追加
    save_queue(
        store.conn(),
        &snapshot("Q1", "/e", "2026-03-02T08:00:00+08:00", vec![entry("WARD-A-001", 4, 1, 0)]),
    )
    .unwrap();
    let back = load_queue(store.conn(), "Q1").unwrap().unwrap();
    assert_eq!(back.entries.len(), 1, "重扫是替换不是追加");
    assert_eq!(back.entries[0].missing_count, 1);
    assert_eq!(list_queue_ids(store.conn(), 10).unwrap().len(), 1, "会话也只有一条");
}