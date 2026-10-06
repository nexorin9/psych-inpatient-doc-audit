//! Task 28 验收：**写回幂等键与并发确认防护**。
//!
//! 这一组的验收口径只有一句话：**并发与重放都不会让外发多发生一次**。
//! 因此大部分用例直接断言「外发计数」与「写回行数」，而不是只看返回值。
//!
//! 六个用例对应 task.json 的六条 step：
//!
//! 1. 幂等键 = `sha256(住院别名 + 件 id + 内容哈希)`，字段分隔防撞；
//! 2. 同键并发 → 恰好一个占到，第二个拿到 `E_WRITEBACK_CONFLICT` 且带首次
//!    操作人与时刻，**0 条第二条写回**；
//! 3. 重放 → 原样回放首次结果，`replayed=true`，**0 次外发**；
//! 4. 内容变更 → 新键 → 允许再次写回（不被旧键误挡）；
//! 5. 外发与落库之间崩溃 → 重启后升级为 `in_doubt`，重放**被拒**且 0 次外发；
//! 6. 50 线程 × 同键并发 → 外发恰好 1 次。
//!
//! **替身纪律**：并发用例打的是**真实的 sqlite 文件库**（`OpenFlags::read_write`
//! + WAL），不是内存库——内存库根本没有第二个连接，测不到并发。

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};

use psychdoc_store::{
    idempotency_key, ClaimTarget, RefusalReason, ReserveOutcome, Resolution, Store, StoreError,
};

const ALIAS: &str = "WARD-A-001";
const CONSENT: &str = "WARD-A-001-CONSENT-01";
const SHA_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SHA_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
const AT: &str = "2026-03-03T08:10:00+08:00";

/// 一个临时目录 + 库文件路径。Drop 时删掉，避免 `logs/` 与项目目录留垃圾。
struct TempDb(PathBuf);

impl TempDb {
    fn new(tag: &str) -> Self {
        let mut dir = std::env::temp_dir();
        // 用测试名 + 进程 pid 隔离：同一测试跑两次也不会撞
        dir.push(format!("psychdoc-t28-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("建临时目录");
        Self(dir)
    }

    fn path(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn target<'a>(doc_id: &'a str, sha: &'a str) -> ClaimTarget<'a> {
    ClaimTarget {
        doc_id,
        content_sha256: sha,
    }
}

// ---------------------------------------------------------------------------
// Step 1：幂等键形状
// ---------------------------------------------------------------------------

#[test]
fn the_key_is_stable_and_field_separated() {
    let k1 = idempotency_key(ALIAS, CONSENT, SHA_A);
    let k2 = idempotency_key(ALIAS, CONSENT, SHA_A);
    assert_eq!(k1, k2, "同一分量必须算出同一个键");
    assert_eq!(k1.len(), 64);

    // 三个分量各换一个就变
    assert_ne!(k1, idempotency_key("WARD-A-002", CONSENT, SHA_A));
    assert_ne!(k1, idempotency_key(ALIAS, "WARD-A-001-CONSENT-02", SHA_A));
    assert_ne!(k1, idempotency_key(ALIAS, CONSENT, SHA_B));

    // 字段分隔：把分隔挪一位不撞键
    assert_ne!(
        idempotency_key("WARD-A-001X", "CONSENT-01", SHA_A),
        idempotency_key("WARD-A-001", "XCONSENT-01", SHA_A),
        "没有字段分隔时这两个键会撞在一起"
    );
}

// ---------------------------------------------------------------------------
// Step 2：同键并发 → 一个占到，另一个拿到可读的冲突
// ---------------------------------------------------------------------------

#[test]
fn a_second_concurrent_claim_is_refused_with_a_readable_conflict() {
    let tmp = TempDb::new("conflict");
    let store = Store::open(tmp.path("conflict.sqlite3")).unwrap();

    let first = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    assert!(matches!(first, ReserveOutcome::Reserved { .. }));

    // 另一个人（另一台机器）在同一件上点提交
    let second = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长B", "2026-03-03T08:11:00+08:00")
        .unwrap();
    let ReserveOutcome::Refused { blocker, reason, .. } = second else {
        panic!("第二次必须被拒");
    };
    assert_eq!(reason, RefusalReason::Conflict);
    assert_eq!(blocker.claimed_by, "护士长A");
    assert_eq!(blocker.claimed_at, AT);
    assert_eq!(blocker.doc_id, CONSENT);
    assert_eq!(blocker.short_key().len(), 12);

    // 报错文案要能直接贴进交办单：带键、带人、带时刻
    let err = StoreError::WritebackConflict {
        key: blocker.short_key().to_string(),
        first_actor: blocker.claimed_by.clone(),
        first_at: blocker.claimed_at.clone(),
        state: "in_flight",
    };
    let text = err.to_string();
    assert_eq!(err.code(), "E_WRITEBACK_CONFLICT");
    assert!(text.contains("护士长A"), "{text}");
    assert!(text.contains(AT), "{text}");
    assert!(err.is_retryable(), "对方提交完就放开了，值得稍后重试");
}

#[test]
fn a_partially_overlapping_batch_reserves_nothing_at_all() {
    // 整批原子：一批里两件，其中一件被别人占着 → **两件都占不住**。
    // 半占住的批次等于「服务端可能建了 2 件中的 1 件」，那是最难收拾的状态。
    let tmp = TempDb::new("atomic");
    let store = Store::open(tmp.path("atomic.sqlite3")).unwrap();
    let other = "WARD-A-001-RECORD-01";

    store
        .reserve_writeback(ALIAS, &[target(other, SHA_A)], "BATCH-OTHER", "护士长B", AT)
        .unwrap();

    let out = store
        .reserve_writeback(
            ALIAS,
            &[target(CONSENT, SHA_A), target(other, SHA_A)],
            "BATCH-MINE",
            "护士长A",
            AT,
        )
        .unwrap();
    assert!(matches!(out, ReserveOutcome::Refused { .. }));

    // 全新那一件**没有**被占上：占住了就等于说「A 也在提交这一批」，
    // 而这一批根本没发出去。
    assert!(
        store.claim_of(&idempotency_key(ALIAS, CONSENT, SHA_A)).unwrap().is_none(),
        "被拒的批次不该留下半个占键"
    );
}

#[test]
fn an_empty_selection_is_refused_before_any_claim() {
    let tmp = TempDb::new("empty");
    let store = Store::open(tmp.path("empty.sqlite3")).unwrap();
    let err = store
        .reserve_writeback(ALIAS, &[], "BATCH-0", "护士长A", AT)
        .unwrap_err();
    assert_eq!(err.code(), "E_NOTHING_SELECTED");
}

// ---------------------------------------------------------------------------
// Step 3 + 4：重放回放首次结果；内容变更产生新键
// ---------------------------------------------------------------------------

#[test]
fn a_replay_returns_the_first_result_without_a_second_outbound() {
    let tmp = TempDb::new("replay");
    let store = Store::open(tmp.path("replay.sqlite3")).unwrap();
    let key = idempotency_key(ALIAS, CONSENT, SHA_A);

    let reserved = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    let ReserveOutcome::Reserved { keys, .. } = reserved else {
        panic!("首次应占到")
    };
    // 外发一次，落库结果存进预占表
    let n = store
        .settle_committed("BATCH-1", Some(r#"{"created":["Composition/WARD-A-001-CONSENT-01"]}"#), "护士长A", AT)
        .unwrap();
    assert_eq!(n, 1);

    // 网络超时后客户端重试同一 transaction
    let again = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长A", "2026-03-03T08:20:00+08:00")
        .unwrap();
    let ReserveOutcome::Replay {
        keys: replay_keys,
        tx_result_json,
        first_actor,
        first_at,
        ..
    } = again
    else {
        panic!("已提交过的键必须走重放而不是冲突")
    };
    assert_eq!(replay_keys, keys, "回放的键与首次完全一致");
    assert_eq!(first_actor, "护士长A");
    assert_eq!(first_at, AT);
    // 「重放不外发」的凭据：结果直接从库里读出来，不必再问服务端
    let stored: serde_json::Value =
        serde_json::from_str(tx_result_json.as_deref().unwrap()).unwrap();
    assert_eq!(stored["created"][0], "Composition/WARD-A-001-CONSENT-01");

    // 重放键的落库状态没被动过
    assert_eq!(store.claim_of(&key).unwrap().unwrap().state, "committed");
}

#[test]
fn a_changed_content_hash_produces_a_new_key_and_may_be_written_again() {
    // 内容变了（护士长改了知情同意模板）→ 新的幂等键 → 允许再次写回。
    // 若幂等键只取件 id，这里会被旧键挡住，补写永远发不出去。
    let tmp = TempDb::new("rehash");
    let store = Store::open(tmp.path("rehash.sqlite3")).unwrap();

    store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    store.settle_committed("BATCH-1", None, "护士长A", AT).unwrap();

    // 同件、同别名、内容不同
    let out = store
        .reserve_writeback(
            ALIAS,
            &[target(CONSENT, SHA_B)],
            "BATCH-2",
            "护士长A",
            "2026-03-03T09:00:00+08:00",
        )
        .unwrap();
    let ReserveOutcome::Reserved { keys, .. } = out else {
        panic!("内容变更后必须能重新占键，否则补写永远发不出去")
    };
    assert_ne!(keys[0], idempotency_key(ALIAS, CONSENT, SHA_A));
}

#[test]
fn a_released_claim_can_be_reserved_again_but_a_committed_one_cannot() {
    let tmp = TempDb::new("release");
    let store = Store::open(tmp.path("release.sqlite3")).unwrap();

    // 外发失败 → 键被放开 → 可以重占
    store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    assert_eq!(store.release_batch("BATCH-1", "端点 503", "护士长A", AT).unwrap(), 1);
    let retry = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长A", AT)
        .unwrap();
    assert!(matches!(retry, ReserveOutcome::Reserved { .. }), "网络抖动不该把这件永久锁死");

    // 提交过 → 占住，撤回之外只有人工核销能放开
    store.settle_committed("BATCH-2", None, "护士长A", AT).unwrap();
    let again = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-3", "护士长A", AT)
        .unwrap();
    assert!(matches!(
        again,
        ReserveOutcome::Replay { .. } | ReserveOutcome::Refused { .. }
    ));
}

#[test]
fn revoking_a_batch_frees_the_keys_so_the_item_can_be_written_again() {
    // 撤回走 DELETE，服务端那份件已不存在；键必须跟着放开，
    // 否则同一件补写后会命中重放，界面上点提交什么都不发。
    let tmp = TempDb::new("revoke");
    let store = Store::open(tmp.path("revoke.sqlite3")).unwrap();
    store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    store.settle_committed("BATCH-1", None, "护士长A", AT).unwrap();

    assert_eq!(store.revoke_batch("BATCH-1", "已撤回", "护士长A", AT).unwrap(), 1);
    let out = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长A", AT)
        .unwrap();
    assert!(matches!(out, ReserveOutcome::Reserved { .. }), "撤回后必须能重写");
}

// ---------------------------------------------------------------------------
// Step 5：外发与落库之间崩溃 → in_doubt，重放被拒
// ---------------------------------------------------------------------------

#[test]
fn a_crash_between_outbound_and_commit_becomes_in_doubt_and_is_never_resent() {
    let tmp = TempDb::new("crash");
    let path = tmp.path("crash.sqlite3");

    // 第一次运行：占键 → 外发发出 → **在落库前崩了**（没有 settle）
    {
        let store = Store::open(&path).unwrap();
        store
            .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
            .unwrap();
        // 进程在这里崩掉：库里只剩一行 in_flight
    }

    // 第二次运行：启动自检把上次遗留的 in_flight 升级成 in_doubt
    let store = Store::open(&path).unwrap();
    assert_eq!(store.in_flight_claims(ALIAS).unwrap().len(), 1);
    assert_eq!(
        store.recover_orphans("2026-03-03T09:00:00+08:00").unwrap(),
        1
    );
    assert_eq!(store.in_doubt_count().unwrap(), 1);

    // 重试：拒绝，且是「结果未知」而不是「并发冲突」——
    // 两者对用户的行动建议完全不同（前者要人工核对，后者等一下再点）
    let retry = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长A", "2026-03-03T09:05:00+08:00")
        .unwrap();
    let ReserveOutcome::Refused { blocker, reason, .. } = retry else {
        panic!("结果未知时绝不能放行重发")
    };
    assert_eq!(reason, RefusalReason::InDoubt);
    assert_eq!(blocker.claimed_by, "护士长A");

    // 关键：全程没有任何「已写回」被凭空记下来
    assert_eq!(store.writeback_row_count().unwrap(), 0);
}

#[test]
fn recovery_only_upgrades_claims_from_an_earlier_session() {
    // `cutoff` 由调用方注入：本次运行自己占的键不能被误升级成 in_doubt，
    // 否则一个正在提交中的进程会被另一个进程「宣判」为崩溃。
    let tmp = TempDb::new("cutoff");
    let store = Store::open(tmp.path("cutoff.sqlite3")).unwrap();
    store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();

    assert_eq!(store.recover_orphans("2026-03-03T08:05:00+08:00").unwrap(), 0);
    assert_eq!(store.in_flight_claims(ALIAS).unwrap().len(), 1, "本次会话的键不动");
    assert_eq!(store.recover_orphans("2026-03-03T09:00:00+08:00").unwrap(), 1);
}

#[test]
fn a_human_can_settle_an_in_doubt_claim_either_way_but_never_without_a_reason() {
    let tmp = TempDb::new("settle");
    let store = Store::open(tmp.path("settle.sqlite3")).unwrap();
    let key = idempotency_key(ALIAS, CONSENT, SHA_A);
    store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-1", "护士长A", AT)
        .unwrap();
    store.recover_orphans("2026-03-03T09:00:00+08:00").unwrap();

    // 没写理由 = 没核对
    let err = store
        .resolve_claim(&key, Resolution::NotWritten, "  ", "护士长B", AT)
        .unwrap_err();
    assert_eq!(err.code(), "E_CLAIM_STATE");

    // 核对「没建」→ 放开，可重发
    store
        .resolve_claim(&key, Resolution::NotWritten, "查院内端点：该件不存在", "护士长B", AT)
        .unwrap();
    assert_eq!(store.claim_of(&key).unwrap().unwrap().state, "released");
    let out = store
        .reserve_writeback(ALIAS, &[target(CONSENT, SHA_A)], "BATCH-2", "护士长B", AT)
        .unwrap();
    assert!(matches!(out, ReserveOutcome::Reserved { .. }));

    // 另一件走「核对为已建」这条：占键 → 崩溃遗留 → 人工核销为已写回
    let other = "WARD-A-001-RECORD-01";
    let key2 = idempotency_key(ALIAS, other, SHA_A);
    store
        .reserve_writeback(ALIAS, &[target(other, SHA_A)], "BATCH-3", "护士长A", AT)
        .unwrap();
    store.recover_orphans("2026-03-04T09:00:00+08:00").unwrap();
    store
        .resolve_claim(
            &key2,
            Resolution::AlreadyWritten,
            "查院内端点：Composition 已存在",
            "护士长B",
            AT,
        )
        .unwrap();
    assert_eq!(store.claim_of(&key2).unwrap().unwrap().state, "committed");
    let after = store
        .reserve_writeback(ALIAS, &[target(other, SHA_A)], "BATCH-4", "护士长B", AT)
        .unwrap();
    assert!(matches!(after, ReserveOutcome::Replay { .. }), "已写回的不该再发一次");
}

// ---------------------------------------------------------------------------
// Step 6：50 线程 × 同键 → 外发恰好 1 次
// ---------------------------------------------------------------------------

#[test]
fn fifty_threads_racing_the_same_key_produce_exactly_one_outbound() {
    // 打真实文件库 + 真实 `BEGIN IMMEDIATE`：内存库没有第二个连接，
    // 那种「并发」测不到任何东西。
    let tmp = TempDb::new("race");
    let path = tmp.path("race.sqlite3");
    Store::open(&path).unwrap(); // 先建好 schema

    const THREADS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));
    let outbound = Arc::new(AtomicUsize::new(0));

    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            let outbound = outbound.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                let out = store
                    .reserve_writeback(
                        ALIAS,
                        &[target(CONSENT, SHA_A)],
                        &format!("BATCH-{i}"),
                        &format!("护士长{i}"),
                        AT,
                    )
                    .unwrap();
                match out {
                    ReserveOutcome::Reserved { batch_id, .. } => {
                        // 抢到的那一个才允许外发
                        outbound.fetch_add(1, Ordering::SeqCst);
                        store.settle_committed(&batch_id, Some(r#"{"created":["x"]}"#), "护士长", AT).unwrap();
                        "reserved"
                    }
                    ReserveOutcome::Replay { .. } => "replayed",
                    ReserveOutcome::Refused { reason, blocker, .. } => {
                        // 冲突文案必须可读：带首次操作人，不是一句「已存在」
                        assert!(!blocker.claimed_by.is_empty(), "冲突必须指出是谁占着");
                        assert_eq!(blocker.claimed_at, AT);
                        match reason {
                            RefusalReason::Conflict => "conflict",
                            RefusalReason::InDoubt => panic!("并发不该产生 in_doubt"),
                        }
                    }
                }
            })
        })
        .collect();

    let results: Vec<&str> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let reserved = results.iter().filter(|r| **r == "reserved").count();
    let others = THREADS - reserved;

    assert_eq!(
        outbound.load(Ordering::SeqCst),
        1,
        "50 个线程抢同一个键，外发必须恰好 1 次（结果分布：{results:?}）"
    );
    assert_eq!(reserved, 1, "恰好一个占到");
    assert!(
        results
            .iter()
            .filter(|r| **r == "conflict" || **r == "replayed")
            .count()
            == others,
        "其余 {others} 个要么撞冲突要么命中重放"
    );

    // 落库侧同样只有一行：唯一约束没有被并发打穿
    let store = Store::open(&path).unwrap();
    assert_eq!(store.in_flight_claims(ALIAS).unwrap().len(), 0);
    assert_eq!(store.in_doubt_count().unwrap(), 0);
    assert_eq!(store.writeback_row_count().unwrap(), 0);
}

#[test]
fn concurrent_claims_on_distinct_keys_all_succeed() {
    // 反向守卫：并发防护不能把**不同**的件也挡掉——50 件各写各的，
    // 50 个占键都要成。写死了全局锁的实现会在这里卡死或大面积失败。
    let tmp = TempDb::new("distinct");
    let path = tmp.path("distinct.sqlite3");
    Store::open(&path).unwrap();

    const THREADS: usize = 50;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                barrier.wait();
                let doc = format!("WARD-A-001-CONSENT-{i:02}");
                let t = ClaimTarget {
                    doc_id: &doc,
                    content_sha256: SHA_A,
                };
                matches!(
                    store
                        .reserve_writeback(ALIAS, &[t], &format!("BATCH-{i}"), "护士长A", AT)
                        .unwrap(),
                    ReserveOutcome::Reserved { .. }
                )
            })
        })
        .collect();

    let ok = handles.into_iter().map(|h| h.join().unwrap()).filter(|ok| *ok).count();
    assert_eq!(ok, THREADS, "不同件的并发占键不该互相挡");
}
// ---------------------------------------------------------------------------
// `commit_writeback` 自身也要能扛并发（Task 25 交接单点名的尾巴）
// ---------------------------------------------------------------------------

#[test]
fn concurrent_commit_writeback_produces_no_busy_error_and_exactly_one_row() {
    // Task 25 实测到：并发同键写会撞 `SQLITE_BUSY_SNAPSHOT`，而
    // `busy_timeout` 在「读事务升级成写事务」这一步**不生效**——
    // sqlite 必须立刻失败，否则两个读者都打算写时会死锁。
    // 现象是护士长点「确认写回」，偶尔蹦一句莫名其妙的
    // `E_DB_IO: database is locked`。
    //
    // `commit_writeback` 是「查已存在 → 再写入」的形状，正是这个坑的典型。
    // 改成 `BEGIN IMMEDIATE` 后，并发提交要么等锁、要么直接撞唯一约束走
    // `AlreadyExists`，**不再出现 `E_DB_IO`**。
    let tmp = TempDb::new("commit");
    let path = tmp.path("commit.sqlite3");
    {
        let store = Store::open(&path).unwrap();
        // 每线程一份草稿：护士长各开了一个标签页。共用一份草稿的话，
        // 第一个提交成功就把草稿推进 `sent`，其余线程在 `E_DRAFT_STATE`
        // 就被挡住了，压根到不了本用例要盯的那段「查已存在 → 再写入」。
        for i in 0..24 {
            store
                .create_draft(&format!("DRAFT-{i}"), ALIAS, &[], "护士长A", AT)
                .unwrap();
        }
    }

    const THREADS: usize = 24;
    let barrier = Arc::new(Barrier::new(THREADS));
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let path = path.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let store = Store::open(&path).unwrap();
                let e = [psychdoc_store::WritebackEntry {
                    doc_id: CONSENT,
                    content_sha256: SHA_A,
                    run_id: None,
                }];
                barrier.wait();
                store.commit_writeback(
                    &format!("DRAFT-{i}"),
                    &e,
                    &format!("护士长{i}"),
                    AT,
                    SHA_A,
                )
            })
        })
        .collect();

    let mut committed = 0usize;
    let mut already = 0usize;
    for h in handles {
        match h.join().unwrap() {
            Ok(psychdoc_store::CommitOutcome::Committed { .. }) => committed += 1,
            Ok(psychdoc_store::CommitOutcome::AlreadyExists { .. }) => already += 1,
            Err(e) => panic!("并发提交不该报 {e}（{}）", e.code()),
        }
    }
    assert!(committed >= 1, "至少一个提交成功");
    assert_eq!(committed + already, THREADS);

    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.writeback_row_count().unwrap(),
        1,
        "唯一约束没被打穿：写回表仍然一行"
    );
}
