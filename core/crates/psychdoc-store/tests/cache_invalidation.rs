//! Task 26 验收：增量缓存的**并发写与失效**。
//!
//! Task 9 做的是单线程增量（内容没变就跳过）。本任务补上缓存的**正确性边界**
//! —— 那几条边界一旦破了，工具不会报错，只会给出一份**看起来正常但其实是
//! 旧版规则审出来的**清单：
//!
//! | 组 | 守住什么 |
//! |---|---|
//! | 一、规则集变更失效 | 改一个字节 → 全量重审；旧版缓存一条都不许命中 |
//! | 二、边界用例 | 内容回滚不误命中过期结果；哈希碰撞靠字段比对挡住 |
//! | 三、并发写 | 多线程各写各的 → 无 lost update；同键并发 → 整体替换不撕裂 |
//! | 四、崩溃安全 | 写缓存与写审查结果同事务；失败整批回滚，0 条部分写入 |
//! | 五、端到端 | 真实脱敏文书包上跑一遍「改规则集 → 全量重审 → 命中率恢复」 |

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use psychdoc_model::Diagnostic;
use psychdoc_store::cache::{cache_ruleset_sha256, CacheInvalidation};
use psychdoc_store::{
    CacheScope, CacheWrite, ReviewRunInput, Store,
};

const RULESET_V1: &str = include_str!("../../../../rulesets/hospital.yaml");
/// 与 V1 只差一个字节的第二版规则集（改版本号里的一个字符）。
fn ruleset_v2() -> String {
    let s = RULESET_V1.to_string();
    // 升 patch 版本：与「内容变了但语义没变」区分开——本任务要测的正是
    // 「哈希变了就重审」，而这不代表两者语义必然不同。
    let needle = "version: \"0.6.0\"";
    assert!(
        s.contains(needle),
        "交付规则集的版本行形状变了，本用例的构造前提不成立"
    );
    s.replace(needle, "version: \"0.6.1\"")
}

fn rules_v1() -> psychdoc_rules::EffectiveRules {
    psychdoc_rules::load_effective(RULESET_V1).unwrap()
}
fn rules_v2() -> psychdoc_rules::EffectiveRules {
    psychdoc_rules::load_effective(&ruleset_v2()).unwrap()
}

fn row<'a>(
    doc_id: &'a str,
    sha: &'a str,
    ruleset: &'a str,
    run: &'a str,
    payload: &'a str,
) -> CacheWrite<'a> {
    CacheWrite {
        scope: CacheScope::Item,
        doc_id,
        content_sha256: sha,
        ruleset_sha256: ruleset,
        review_run_id: run,
        payload_json: payload,
    }
}

/// 一个稳定的 64 位哈希（`sha2` 是本 crate 已有依赖，不必另找）。
fn sha_of(tag: &str) -> String {
    use sha2::Digest;
    let digest = sha2::Sha256::digest(tag.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

// ===========================================================================
// 一、规则集变更失效
// ===========================================================================

#[test]
fn changing_the_ruleset_by_one_byte_invalidates_the_whole_table() {
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let v2 = rules_v2();
    assert_ne!(
        v1.sha256, v2.sha256,
        "两份规则集必须真的不同，否则本用例测不到失效"
    );

    // 先用 v1 跑一轮，写入 3 条缓存
    let h = [sha_of("a"), sha_of("b"), sha_of("c")];
    store
        .write_cache(
            &[
                row("WARD-A-001-CONSENT-01", &h[0], &v1.sha256, "run-001", "{\"n\":0}"),
                row("WARD-A-001-RECORD-01", &h[1], &v1.sha256, "run-001", "{\"n\":1}"),
                row("WARD-A-001-RELEASE-01", &h[2], &v1.sha256, "run-001", "{\"n\":2}"),
            ],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    assert_eq!(store.cache_row_count().unwrap(), 3);

    // 换成 v2 → 整表作废
    let rec = store
        .invalidate_cache_for_ruleset(&v2.sha256)
        .unwrap()
        .expect("规则集变了必须作废");
    assert_eq!(rec.dropped, 3);
    assert_eq!(rec.from_ruleset_short_sha256, v1.short_sha256());
    assert_eq!(rec.to_ruleset_short_sha256, v2.short_sha256());
    assert_eq!(store.cache_row_count().unwrap(), 0, "旧版缓存必须一条不剩");
    assert!(
        rec.as_zh().contains("因规则集变更全量重审"),
        "{}",
        rec.as_zh()
    );
}

#[test]
fn an_entry_from_the_previous_ruleset_never_hits() {
    // 上面的整表作废是第一道闸。这条是第二道：**即便**作废被绕过
    // （比如调用方忘了调，或缓存从别的进程写进来），
    // 逐字段校验也必须挡住「拿 v1 的结论当 v2 的用」。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let v2 = rules_v2();
    let h = sha_of("a");
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-001", "{\"n\":0}")],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();

    assert!(
        store
            .cache_lookup(CacheScope::Item, "WARD-A-001-CONSENT-01", &h, &v2.sha256)
            .unwrap()
            .is_none(),
        "v1 产出的结论绝不能被 v2 命中"
    );
    assert!(
        store
            .cache_lookup(CacheScope::Item, "WARD-A-001-CONSENT-01", &h, &v1.sha256)
            .unwrap()
            .is_some(),
        "同版本内应当命中"
    );
}

#[test]
fn the_same_ruleset_leaves_the_cache_untouched() {
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-001", "{\"n\":0}")],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    assert!(store.invalidate_cache_for_ruleset(&v1.sha256).unwrap().is_none());
    assert_eq!(store.cache_row_count().unwrap(), 1, "规则集没变就不该动缓存");
}

#[test]
fn an_empty_cache_does_not_claim_a_ruleset_change() {
    // 空表 + 规则集变了 = 「本来就没有可复用的结论」，不是「变更」。
    // 报「因规则集变更全量重审」会让第一次用的人以为工具坏了。
    let store = Store::open_in_memory().unwrap();
    let v2 = rules_v2();
    assert!(store.invalidate_cache_for_ruleset(&v2.sha256).unwrap().is_none());
    assert_eq!(store.cache_row_count().unwrap(), 0);
}

// ===========================================================================
// 二、边界用例
// ===========================================================================

#[test]
fn rolling_a_document_back_to_an_older_hash_does_not_hit_a_stale_result() {
    // 内容回滚（改回去）本身**应该**命中——那是 Task 9 定的行为，撤销一次
    // 误操作不必等全量重审。本用例守住的是它的边界：**跨规则集版本的
    // 回滚不许命中**。护士长改回旧内容，但期间院内把规则改了，
    // 那条旧内容必须用新规则重审。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let v2 = rules_v2();
    let old = sha_of("v1 内容");
    let new = sha_of("v2 内容");

    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &old, &v1.sha256, "run-001", "{\"v\":1}")],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    // 内容改了一版 → 新哈希，v1 规则
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &new, &v1.sha256, "run-002", "{\"v\":2}")],
            "2026-03-02T10:00:00+08:00",
        )
        .unwrap();
    assert_eq!(store.cache_history(CacheScope::Item, "WARD-A-001-CONSENT-01").unwrap().len(), 2);

    // 规则集升版 → 两条一起作废
    store.invalidate_cache_for_ruleset(&v2.sha256).unwrap().unwrap();
    assert_eq!(store.cache_row_count().unwrap(), 0);

    // 内容改回旧哈希（撤销那次修改）——在 v2 下必须重审，不能命中 v1 的结论
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &old, &v2.sha256, "run-003", "{\"v\":3}")],
            "2026-03-02T11:00:00+08:00",
        )
        .unwrap();
    let hit = store
        .cache_lookup(CacheScope::Item, "WARD-A-001-CONSENT-01", &old, &v2.sha256)
        .unwrap()
        .expect("同规则集下改回去应当命中");
    assert_eq!(
        hit.payload_json, "{\"v\":3}",
        "命中的必须是 v2 重审后的结论，不是 v1 那条"
    );
    assert_eq!(hit.review_run_id, "run-003");
}

#[test]
fn a_row_whose_own_hash_field_disagrees_with_the_key_never_hits() {
    // 哈希碰撞 / 库被手改：主键里的 content_sha256 是 h，行里也是 h，
    // 但行里的 ruleset_sha256 被改成别的版本 → 判未命中。
    // 三道校验里最容易被忽略的是「行内字段 vs 入参」这道——
    // 只按主键查的话，这行会被当成有效缓存用。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-001", "{\"n\":0}")],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    // 绕过 API 直接改库，模拟「有人手工改过 / 迁移脚本写错了列」
    store
        .conn()
        .execute(
            "UPDATE review_cache SET ruleset_sha256 = 'deadbeef' WHERE doc_id = 'WARD-A-001-CONSENT-01'",
            [],
        )
        .unwrap();
    assert!(
        store
            .cache_lookup(CacheScope::Item, "WARD-A-001-CONSENT-01", &h, &v1.sha256)
            .unwrap()
            .is_none(),
        "行内规则集指纹与入参不符 → 必须判未命中"
    );
}

#[test]
fn item_and_group_scopes_never_see_each_other() {
    // 件级与组级混在一张表：同 doc_id + 同哈希在两个 scope 下必须是两条
    // 互不相干的记录。少了 scope 列就会互相顶掉。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("same");
    store
        .write_cache(
            &[
                CacheWrite {
                    scope: CacheScope::Item,
                    doc_id: "WARD-A-001",
                    content_sha256: &h,
                    ruleset_sha256: &v1.sha256,
                    review_run_id: "run-001",
                    payload_json: "{\"lvl\":\"item\"}",
                },
                CacheWrite {
                    scope: CacheScope::Group,
                    doc_id: "WARD-A-001",
                    content_sha256: &h,
                    ruleset_sha256: &v1.sha256,
                    review_run_id: "run-001",
                    payload_json: "{\"lvl\":\"group\"}",
                },
            ],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    assert_eq!(store.cache_row_count().unwrap(), 2);
    let item = store
        .cache_lookup(CacheScope::Item, "WARD-A-001", &h, &v1.sha256)
        .unwrap()
        .unwrap();
    let group = store
        .cache_lookup(CacheScope::Group, "WARD-A-001", &h, &v1.sha256)
        .unwrap()
        .unwrap();
    assert_eq!(item.payload_json, "{\"lvl\":\"item\"}");
    assert_eq!(group.payload_json, "{\"lvl\":\"group\"}");
}

// ===========================================================================
// 三、并发写
// ===========================================================================

#[test]
fn concurrent_writers_to_distinct_keys_lose_nothing() {
    // 4 个线程 × 25 条各写各的 → 库里必须正好 100 条，一条不少。
    // 少了就是 lost update：某个线程的写被另一个线程的整批覆盖掉了。
    let dir = std::env::temp_dir().join("psychdoc-store-cache-concurrent");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("psychdoc.sqlite3");

    const THREADS: usize = 4;
    const PER_THREAD: usize = 25;
    let v1 = rules_v1();
    let done = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for t in 0..THREADS {
        let path = path.clone();
        let sha = v1.sha256.clone();
        let done = Arc::clone(&done);
        handles.push(std::thread::spawn(move || {
            // 每线程一个独立连接：这正是真实部署形态（多个常驻进程 / 线程
            // 各自 `Store::open` 同一份库）。共享一个 `Connection` 反而测不到
            // 跨连接的事务互斥。
            let store = Store::open(&path).unwrap();
            for i in 0..PER_THREAD {
                let doc = format!("WARD-{t}-ITEM-{i:02}");
                let h = sha_of(&doc);
                let payload = format!("{{\"t\":{t},\"i\":{i}}}");
                store
                    .write_cache(
                        &[CacheWrite {
                            scope: CacheScope::Item,
                            doc_id: &doc,
                            content_sha256: &h,
                            ruleset_sha256: &sha,
                            review_run_id: "run-concurrent",
                            payload_json: &payload,
                        }],
                        "2026-03-02T09:00:00+08:00",
                    )
                    .unwrap();
                done.fetch_add(1, Ordering::SeqCst);
            }
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(done.load(Ordering::SeqCst), THREADS * PER_THREAD);
    let store = Store::open(&path).unwrap();
    assert_eq!(
        store.cache_row_count().unwrap(),
        THREADS * PER_THREAD,
        "并发写之后行数必须一条不少——少了就是 lost update"
    );
    // 逐条回查：不能只看总数。总数对但内容串了同样算坏。
    for t in 0..THREADS {
        for i in 0..PER_THREAD {
            let doc = format!("WARD-{t}-ITEM-{i:02}");
            let h = sha_of(&doc);
            let got = store
                .cache_lookup(CacheScope::Item, &doc, &h, &v1.sha256)
                .unwrap()
                .unwrap_or_else(|| panic!("{doc} 应有缓存"));
            assert_eq!(got.payload_json, format!("{{\"t\":{t},\"i\":{i}}}"), "{doc} 的内容串了");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn concurrent_writers_to_the_same_key_replace_wholesale_and_never_interleave() {
    // 同一主键并发写：后写者**整体**替换先写者，不产生
    // 「一半来自 A、一半来自 B」的撕裂状态。这是 `INSERT OR REPLACE`
    // 而不是「先 SELECT 再 UPDATE」的关键区别。
    let dir = std::env::temp_dir().join("psychdoc-store-cache-samekey");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("psychdoc.sqlite3");
    let v1 = rules_v1();
    let sha = v1.sha256.clone();
    let doc = "WARD-A-001-CONSENT-01".to_string();
    let h = sha_of(&doc);

    const WRITERS: usize = 8;
    // 每个写入者的 payload 都是一段 400 字节、只在**首字节**不同的文本。
    // 若发生撕裂（逐字段更新），读回来的 400 字节里会混进两个写入者的内容。
    let mut handles = Vec::new();
    for w in 0..WRITERS {
        let path = path.clone();
        let (doc, h, sha) = (doc.clone(), h.clone(), sha.clone());
        handles.push(std::thread::spawn(move || {
            let store = Store::open(&path).unwrap();
            let marker = (b'A' + w as u8) as char;
            let payload = format!("{{\"m\":\"{marker}\",\"pad\":\"{}\"}}", "x".repeat(400));
            store
                .write_cache(
                    &[CacheWrite {
                        scope: CacheScope::Item,
                        doc_id: &doc,
                        content_sha256: &h,
                        ruleset_sha256: &sha,
                        review_run_id: &format!("run-{w}"),
                        payload_json: &payload,
                    }],
                    "2026-03-02T09:00:00+08:00",
                )
                .unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    let store = Store::open(&path).unwrap();
    assert_eq!(store.cache_row_count().unwrap(), 1, "同一主键只应留下一行");
    let got = store
        .cache_lookup(CacheScope::Item, &doc, &h, &v1.sha256)
        .unwrap()
        .unwrap();
    // 整段 payload 必须来自**同一个**写入者：首标记与 run 编号要对得上，
    // 且 400 个填充字符必须完整（少一个就说明是被截断/撕裂过的）。
    // payload 形状固定为 `{"m":"X","pad":"xxx…"}`，标记字符在第 6 字节。
    let marker = got.payload_json.as_bytes()[6];
    assert!(
        (b'A'..b'A' + WRITERS as u8).contains(&marker),
        "读回的首标记字节 {marker:?} 不是任何一个写入者写的"
    );
    assert_eq!(
        got.review_run_id,
        format!("run-{}", (marker - b'A') as usize),
        "payload 与 review_run_id 必须来自同一次写入"
    );
    assert_eq!(
        got.payload_json.matches('x').count(),
        400,
        "payload 被撕裂了：填充段应完整 400 个字符"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn repeated_writes_of_the_same_key_report_only_the_new_rows() {
    // 「写了几条」与「新增了几条」是两件事：命中率统计必须用后者。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    let rows = [row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-001", "{\"n\":1}")];
    assert_eq!(
        store.write_cache(&rows, "2026-03-02T09:00:00+08:00").unwrap(),
        1
    );
    assert_eq!(
        store.write_cache(&rows, "2026-03-02T10:00:00+08:00").unwrap(),
        0,
        "第二次写同一主键不算新增"
    );
    assert_eq!(store.cache_row_count().unwrap(), 1);
}

// ===========================================================================
// 四、崩溃安全：写缓存与写审查结果同事务
// ===========================================================================

fn diag(doc_id: &str, code: &str) -> Diagnostic {
    Diagnostic {
        code: code.to_string(),
        loc: psychdoc_model::DocLoc {
            doc_id: doc_id.to_string(),
            section_index: 0,
            char_range: psychdoc_model::Span::unchecked(0, 3),
        },
        // 依据条款非空是硬要求：RuleMessage::new 返回 Result 是因为
        // 「无依据的诊断」是本项目明确禁止的形状。
        message: psychdoc_model::RuleMessage::new("测试诊断", "《精神科住院护理常规》第 4.2 条")
            .expect("依据条款非空"),
        severity: psychdoc_model::Severity::Blocking,
        fix_availability: psychdoc_model::FixAvailability::None,
        fix: None,
        waived: None,
        suppressed_by: None,
    }
}

#[test]
fn the_cache_and_the_review_record_land_in_the_same_transaction() {
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    let d = diag("WARD-A-001-CONSENT-01", "QC-PSY-01");
    let input = ReviewRunInput {
        run_id: "run-tx-001",
        encounter_alias: "WARD-A-001",
        ruleset_version: &v1.version,
        ruleset_sha256: &v1.sha256,
        actor: "nurse-lead-01",
        started_at: "2026-03-02T09:00:00+08:00",
        finished_at: Some("2026-03-02T09:00:05+08:00"),
        items_total: 1,
        items_reviewed: 1,
        items_skipped: 0,
        unresolved_count: 0,
        waived_count: 0,
        diagnostics: std::slice::from_ref(&d),
        suppressed: &[],
        rule_sources: &[],
    };
    let fresh = store
        .commit_review_with_cache(
            &input,
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-tx-001", "{\"codes\":[\"QC-PSY-01\"]}")],
        )
        .unwrap();
    assert_eq!(fresh, 1);
    assert_eq!(store.review_run_count().unwrap(), 1);
    assert_eq!(store.cache_row_count().unwrap(), 1);
    assert_eq!(store.diagnostic_count().unwrap(), 1);
    // 缓存行记着「哪一次审查产出了它」——这条链断了，回溯就答不出
    // 「这批结论是哪次审出来的」。
    let got = store
        .cache_lookup(CacheScope::Item, "WARD-A-001-CONSENT-01", &h, &v1.sha256)
        .unwrap()
        .unwrap();
    assert_eq!(got.review_run_id, "run-tx-001");
    assert_eq!(got.written_at, "2026-03-02T09:00:05+08:00", "写入时刻取批次完成时刻");
}

#[test]
fn a_failing_batch_leaves_zero_rows_in_both_tables() {
    // 模拟「写到一半失败」：同批里有一条诊断挂在**不存在的** run_id 上
    // 会触发外键失败。用一个已存在的 run_id 重复插入来触发主键冲突，
    // 效果一样：事务中途出错，整批必须回滚。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    let d = diag("WARD-A-001-CONSENT-01", "QC-PSY-01");
    let input = ReviewRunInput {
        run_id: "run-tx-002",
        encounter_alias: "WARD-A-001",
        ruleset_version: &v1.version,
        ruleset_sha256: &v1.sha256,
        actor: "nurse-lead-01",
        started_at: "2026-03-02T09:00:00+08:00",
        finished_at: Some("2026-03-02T09:00:05+08:00"),
        items_total: 1,
        items_reviewed: 1,
        items_skipped: 0,
        unresolved_count: 0,
        waived_count: 0,
        diagnostics: std::slice::from_ref(&d),
        suppressed: &[],
        rule_sources: &[],
    };
    store
        .commit_review_with_cache(
            &input,
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-tx-002", "{\"ok\":1}")],
        )
        .unwrap();
    assert_eq!(store.review_run_count().unwrap(), 1);
    assert_eq!(store.cache_row_count().unwrap(), 1);

    // 第二批用**同一个 run_id**（主键冲突）→ 整批失败
    let clash = ReviewRunInput {
        run_id: "run-tx-002",
        ..input
    };
    let err = store
        .commit_review_with_cache(
            &clash,
            &[row("WARD-A-001-RECORD-01", &h, &v1.sha256, "run-tx-002", "{\"ok\":2}")],
        )
        .unwrap_err();
    assert_eq!(err.code(), "E_DB_IO", "写入失败必须是可识别的错误码");
    assert_eq!(
        store.review_run_count().unwrap(),
        1,
        "失败批次不得留下任何审查记录"
    );
    assert_eq!(
        store.cache_row_count().unwrap(),
        1,
        "失败批次不得留下任何缓存行——缓存说「审过了」而审查记录里没有，"
    );
    assert!(
        store
            .cache_lookup(CacheScope::Item, "WARD-A-001-RECORD-01", &h, &v1.sha256)
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_cache_row_never_outlives_the_review_run_that_produced_it() {
    // 「缓存说有、审查记录没有」是最难查的一种坏数据：底栏显示高命中率，
    // 但按 run_id 回溯查不到对应批次。因此给一条可查询的不变量测试。
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let h = sha_of("a");
    let input = ReviewRunInput {
        run_id: "run-orphan",
        encounter_alias: "WARD-A-001",
        ruleset_version: &v1.version,
        ruleset_sha256: &v1.sha256,
        actor: "qc-01",
        started_at: "2026-03-02T09:00:00+08:00",
        finished_at: None,
        items_total: 1,
        items_reviewed: 1,
        items_skipped: 0,
        unresolved_count: 0,
        waived_count: 0,
        diagnostics: &[],
        suppressed: &[],
        rule_sources: &[],
    };
    store
        .commit_review_with_cache(
            &input,
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-orphan", "{}")],
        )
        .unwrap();
    // 手工查一遍「缓存指向的 run_id 是否都存在于 review_runs」
    let dangling: i64 = store
        .conn()
        .query_row(
            "SELECT COUNT(*) FROM review_cache c
              WHERE NOT EXISTS (SELECT 1 FROM review_runs r WHERE r.run_id = c.review_run_id)",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(dangling, 0, "缓存不得指向不存在的审查批次");
}

// ===========================================================================
// 五、端到端：真实脱敏文书包上「改规则集 → 全量重审 → 命中率恢复」
// ===========================================================================

#[test]
fn a_changed_ruleset_forces_a_full_rereview_and_the_hit_rate_recovers() {
    use psychdoc_parse::synth::{self, SynthParams, ViolationProfile};

    let reg = rules_v1().registry();
    let params = SynthParams::default().violations(ViolationProfile {
        missing_consent_signature: true,
        ..Default::default()
    });
    let mut items = Vec::new();
    for pkg in synth::synth_corpus(1, 3, params) {
        let json = serde_json::to_string(&pkg).unwrap();
        items.extend(psychdoc_parse::parse_bundle(&json, Some(&reg)).unwrap().items);
    }
    assert!(items.len() >= 12, "样本太小说明生成器形状变了");

    // 第一轮：v1 全量审，缓存写进库
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    let mut cache = psychdoc_rules::ReviewCache::new();
    let r1 = psychdoc_rules::review_incremental(&items, &v1, &mut cache, 0);
    assert_eq!(r1.summary.items_reviewed, items.len(), "冷缓存全量重跑");
    assert!(r1.summary.items_skipped == 0);
    assert!(
        r1.summary.invalidated.is_none(),
        "首轮不该报「因规则集变更全量重审」"
    );

    // `CacheWrite` 借用 `&str`，而哈希是现算出来的——先把它们收集进
    // `Vec<String>` 撑住生命周期，再借用构造行。
    let owned_keys: Vec<(String, String)> = items
        .iter()
        .map(|i| (i.doc_id.clone(), psychdoc_rules::content_sha256(i)))
        .collect();
    let cache_rows: Vec<CacheWrite<'_>> = owned_keys
        .iter()
        .map(|(doc_id, sha)| CacheWrite {
            scope: CacheScope::Item,
            doc_id,
            content_sha256: sha,
            ruleset_sha256: &v1.sha256,
            review_run_id: "run-e2e-001",
            payload_json: "{}",
        })
        .collect();
    assert_eq!(
        store
            .write_cache(&cache_rows, "2026-03-02T09:00:00+08:00")
            .unwrap(),
        items.len()
    );

    // 第二轮：规则集没变 → 全命中
    let r2 = psychdoc_rules::review_incremental(&items, &v1, &mut cache, 0);
    assert_eq!(r2.summary.items_reviewed, 0, "规则集没变就该全命中");
    assert_eq!(r2.summary.items_skipped, items.len());
    assert!((r2.summary.hit_rate() - 1.0).abs() < 1e-9);

    // 第三轮：规则集升版 → 库侧与内存侧都整表作废，全量重审
    let v2 = rules_v2();
    let rec: CacheInvalidation = store
        .invalidate_cache_for_ruleset(&v2.sha256)
        .unwrap()
        .expect("规则集变了必须作废");
    assert_eq!(rec.dropped, items.len());
    let r3 = psychdoc_rules::review_incremental(&items, &v2, &mut cache, 0);
    assert_eq!(
        r3.summary.items_reviewed, items.len(),
        "规则集改一个字节 → 每一件都必须重审，不能有漏网的"
    );
    assert_eq!(r3.summary.items_skipped, 0);
    let inv = r3.summary.invalidated.as_ref().expect("应留一条失效记录");
    assert_eq!(inv.from_ruleset_short_sha256, v1.short_sha256());
    assert_eq!(inv.to_ruleset_short_sha256, v2.short_sha256());
    let zh = r3.summary.as_zh();
    assert!(zh.contains("因规则集变更全量重审"), "{zh}");

    // 第四轮：新规则集下再跑一次 → 命中率恢复到 100%
    let r4 = psychdoc_rules::review_incremental(&items, &v2, &mut cache, 0);
    assert_eq!(r4.summary.items_reviewed, 0, "新规则集下第二次应全命中");
    assert_eq!(r4.summary.items_skipped, items.len());
    assert!(
        r4.summary.invalidated.is_none(),
        "规则集已对齐，不该再报变更"
    );

    // 关键正确性：v1 与 v2 的结论必须逐条相等——
    // 规则集只改了版本号，语义没变，全量重审后结果就该一样。
    // 若不等，说明「重审」这条路径本身算出了别的东西。
    let full_v2 = psychdoc_rules::review(&items, &v2);
    let fp = |r: &psychdoc_rules::ReviewResult| -> std::collections::BTreeSet<String> {
        r.diagnostics
            .iter()
            .map(|d| format!("{}|{}|{:?}|{}", d.code, d.loc.doc_id, d.loc.char_range, d.message.text))
            .collect()
    };
    assert_eq!(fp(&full_v2), fp(&r3.result), "全量重审结果必须与全量跑逐条相等");
    assert_eq!(fp(&full_v2), fp(&r4.result), "缓存命中后的结果也必须逐条相等");
    assert!(!r3.result.diagnostics.is_empty(), "样本应确实有命中项");
}

#[test]
fn the_library_reads_back_which_ruleset_owns_the_cache() {
    let store = Store::open_in_memory().unwrap();
    let v1 = rules_v1();
    assert_eq!(
        cache_ruleset_sha256(&store).unwrap(),
        "",
        "空库没有规则集归属"
    );
    let h = sha_of("a");
    store
        .write_cache(
            &[row("WARD-A-001-CONSENT-01", &h, &v1.sha256, "run-001", "{}")],
            "2026-03-02T09:00:00+08:00",
        )
        .unwrap();
    assert_eq!(cache_ruleset_sha256(&store).unwrap(), v1.sha256);
}