//! 审查队列的持久化（Task 32：拖放与批量队列）。
//!
//! ## 要解决的问题
//!
//! 护士长早上把整个病区的文件夹拖进来，关窗去查房，回来发现队列没了。
//! 于是要么重新拖一遍，要么——更糟——以为「上午那份已经审完了」。
//! **常驻工具必须记得自己上一次载入了什么。**
//!
//! ## 三条设计取舍
//!
//! 1. **队列 id = `sha256(source_path + loaded_at)`**，不按路径去重。
//!    同一个文件夹上午拖一次、下午补几件再拖一次，是**两次不同的载入**：
//!    后一次要保留「上午已审到第 8 次住院」这个进度，前一次也要留着供回查。
//!    按路径去重会把下午那次直接顶掉上午的进度。
//! 2. **落库的是队列骨架，不是诊断明细**。诊断归 `review_runs` / `diagnostics`。
//!    两处各存一份，重扫一次就对不上——而「对不上」正是审计最不能容忍的。
//! 3. **重扫不清进度**。`rescan` 只更新计数与规则集哈希，
//!    `reviewed_at` 原样保留：重扫是「病区又导出���几件新文书」，
//!    不是「把上午的结论作废重来」。

use rusqlite::{params, OptionalExtension};

use crate::error::{Result, StoreError};

/// 一次拖入（`review_queues` 一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSession {
    pub queue_id: String,
    /// 拖进来的根路径（目录或单份文件）。
    pub source_path: String,
    pub loaded_at: String,
    pub ruleset_short_sha256: String,
    pub packages_found: usize,
    pub files_seen: usize,
    pub items_total: usize,
    pub missing_total: usize,
    /// 「已跳过」清单原样 JSON（存原样是为了重开后逐字还原，
    /// 拆列重拼会与当次扫描的实际文案漂移）。
    pub file_skips_json: String,
    pub last_scanned_at: String,
}

/// 队列里的一次住院（`queue_entries` 一行）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueEntryRow {
    pub encounter_alias: String,
    pub ward: String,
    pub doc_count: usize,
    pub missing_count: usize,
    /// `None` = 本次尚未审；`Some(时刻)` = 已审。
    pub reviewed_at: Option<String>,
    /// 左栏顺序（缺件数降序）。
    pub position: usize,
}

/// 队列 id：路径 + 载入时刻。**刻意不含内容哈希**——同一文件夹二次拖入
/// 是新的一次载入（见模块文档第 1 条）。
pub fn queue_id(source_path: &str, loaded_at: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    // 分隔符防止 `a|b` 与 `a` + `b` 两种切分撞出同一个键。
    h.update(source_path.as_bytes());
    h.update(b"\x1f");
    h.update(loaded_at.as_bytes());
    format!("{:x}", h.finalize())
}

/// 一次落库的完整队列。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueSnapshot {
    pub session: QueueSession,
    pub entries: Vec<QueueEntryRow>,
}

/// 落一次队列。**整批一个事务**：队列的会话行与条目行要么都成、要么都不成，
/// 半截队列会让重开后的左栏少一半住院，且没人知道少的是哪一半。
///
/// 同一 `queue_id` 重复落库是**覆盖**而非追加：重扫时走这条路。
pub fn save_queue(conn: &rusqlite::Connection, snap: &QueueSnapshot) -> Result<usize> {
    if snap.entries.is_empty() {
        return Err(StoreError::QueueEmpty);
    }
    // 同一快照里出现重复别名 = 上游聚合出了问题（按住院别名分组后
    // 不该再有两条同名的）。**显式拒掉并点名**，而不是让主键约束在
    // 事务中间炸出一个看不懂的 UNIQUE constraint failed。
    let mut seen: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    for e in &snap.entries {
        if !seen.insert(e.encounter_alias.as_str()) {
            return Err(StoreError::QueueDuplicateEncounter(e.encounter_alias.clone()));
        }
    }
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO review_queues
           (queue_id, source_path, loaded_at, ruleset_short_sha256, packages_found,
            files_seen, items_total, missing_total, file_skips_json, last_scanned_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
         ON CONFLICT(queue_id) DO UPDATE SET
           ruleset_short_sha256 = excluded.ruleset_short_sha256,
           packages_found       = excluded.packages_found,
           files_seen           = excluded.files_seen,
           items_total          = excluded.items_total,
           missing_total        = excluded.missing_total,
           file_skips_json      = excluded.file_skips_json,
           last_scanned_at      = excluded.last_scanned_at",
        params![
            snap.session.queue_id,
            snap.session.source_path,
            snap.session.loaded_at,
            snap.session.ruleset_short_sha256,
            snap.session.packages_found as i64,
            snap.session.files_seen as i64,
            snap.session.items_total as i64,
            snap.session.missing_total as i64,
            snap.session.file_skips_json,
            snap.session.last_scanned_at,
        ],
    )?;
    // 条目整批换掉。`reviewed_at` 不在这里重建——重扫保留进度，
    // 由 [`merge_reviewed_at`] 单独处理（见模块文档第 3 条）。
    tx.execute(
        "DELETE FROM queue_entries WHERE queue_id = ?1",
        params![snap.session.queue_id],
    )?;
    for e in &snap.entries {
        tx.execute(
            "INSERT INTO queue_entries
               (queue_id, encounter_alias, ward, doc_count, missing_count, reviewed_at, position)
             VALUES (?1,?2,?3,?4,?5,?6,?7)",
            params![
                snap.session.queue_id,
                e.encounter_alias,
                e.ward,
                e.doc_count as i64,
                e.missing_count as i64,
                e.reviewed_at,
                e.position as i64,
            ],
        )?;
    }
    tx.commit()?;
    Ok(snap.entries.len())
}

/// 重扫时把**已有的** `reviewed_at` 并回新条目。
///
/// 病区补导出几件新文书后重扫，住院条目集合可能变（多了一次住院），
/// 但已审过的那几次住院不该被清零——护士长上午审过的还在。
///
/// 返回被保住的条数，调用方可以据此在界面上说清
/// 「保留 N 条已审进度」而不是让用户自己发现。
pub fn merge_reviewed_at(conn: &rusqlite::Connection, snap: &mut QueueSnapshot) -> Result<usize> {
    let mut kept = 0usize;
    for e in snap.entries.iter_mut() {
        // 闭包返回类型必须显式标成 `Option<String>`：不标的话 `optional()`
        // 会把 `T` 推成 `String`，读 NULL 的 `reviewed_at` 直接
        // `InvalidColumnType` —— 恰好在「这一条本来就没审过」的分支上炸。
        let prior: Option<Option<String>> = conn
            .query_row(
                "SELECT reviewed_at FROM queue_entries WHERE queue_id = ?1 AND encounter_alias = ?2",
                params![snap.session.queue_id, e.encounter_alias],
                |r| r.get::<_, Option<String>>(0),
            )
            .optional()?;
        if let Some(Some(at)) = prior {
            e.reviewed_at = Some(at);
            kept += 1;
        }
    }
    Ok(kept)
}

/// 按 `queue_id` 读回一次队列，条目按左栏顺序返回。
pub fn load_queue(conn: &rusqlite::Connection, queue_id: &str) -> Result<Option<QueueSnapshot>> {
    let session = conn
        .query_row(
            "SELECT queue_id, source_path, loaded_at, ruleset_short_sha256, packages_found,
                    files_seen, items_total, missing_total, file_skips_json, last_scanned_at
             FROM review_queues WHERE queue_id = ?1",
            params![queue_id],
            |r| {
                Ok(QueueSession {
                    queue_id: r.get(0)?,
                    source_path: r.get(1)?,
                    loaded_at: r.get(2)?,
                    ruleset_short_sha256: r.get(3)?,
                    packages_found: r.get::<_, i64>(4)? as usize,
                    files_seen: r.get::<_, i64>(5)? as usize,
                    items_total: r.get::<_, i64>(6)? as usize,
                    missing_total: r.get::<_, i64>(7)? as usize,
                    file_skips_json: r.get(8)?,
                    last_scanned_at: r.get(9)?,
                })
            },
        )
        .optional()?;
    let Some(session) = session else {
        return Ok(None);
    };
    let mut stmt = conn.prepare(
        "SELECT encounter_alias, ward, doc_count, missing_count, reviewed_at, position
         FROM queue_entries WHERE queue_id = ?1 ORDER BY position ASC",
    )?;
    let entries = stmt
        .query_map(params![queue_id], |r| {
            Ok(QueueEntryRow {
                encounter_alias: r.get(0)?,
                ward: r.get(1)?,
                doc_count: r.get::<_, i64>(2)? as usize,
                missing_count: r.get::<_, i64>(3)? as usize,
                reviewed_at: r.get(4)?,
                position: r.get::<_, i64>(5)? as usize,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(Some(QueueSnapshot { session, entries }))
}

/// 最近一次载入的队列（重开窗口时取它）。
///
/// 「最近」按 `loaded_at` **倒序**：同一个文件夹上午拖一次、下午再拖一次，
/// 重开窗口要回到**下午那次**（那才是护士长最后看到的）。
pub fn latest_queue(conn: &rusqlite::Connection) -> Result<Option<QueueSnapshot>> {
    let id: Option<String> = conn
        .query_row(
            "SELECT queue_id FROM review_queues ORDER BY loaded_at DESC, queue_id DESC LIMIT 1",
            [],
            |r| r.get(0),
        )
        .optional()?;
    match id {
        Some(id) => load_queue(conn, &id),
        None => Ok(None),
    }
}

/// 一次载入的会话 id 列表（倒序），供「最近载入」菜单用。
pub fn list_queue_ids(conn: &rusqlite::Connection, limit: usize) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT queue_id FROM review_queues ORDER BY loaded_at DESC, queue_id DESC LIMIT ?1",
    )?;
    let ids = stmt
        .query_map(params![limit as i64], |r| r.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(ids)
}

/// 标记若干次住院「本次已审」，时刻取调用方给的 `at`。
///
/// **不存在于该队列的住院别名一律被拒**（返回 `QueueUnknownEncounter`），
/// 而不是静默插一行——后者会让「已审 12 次」里混进一个从没审过的别名。
/// 返回真正标记成功的条数。
pub fn mark_reviewed(
    conn: &rusqlite::Connection,
    queue_id: &str,
    aliases: &[String],
    at: &str,
) -> Result<usize> {
    if aliases.is_empty() {
        return Err(StoreError::QueueNothingSelected);
    }
    let tx = conn.unchecked_transaction()?;
    let mut n = 0usize;
    for alias in aliases {
        let affected = tx.execute(
            "UPDATE queue_entries SET reviewed_at = ?3
             WHERE queue_id = ?1 AND encounter_alias = ?2",
            params![queue_id, alias, at],
        )?;
        if affected == 0 {
            return Err(StoreError::QueueUnknownEncounter(alias.clone()));
        }
        n += affected;
    }
    tx.commit()?;
    Ok(n)
}

/// 清空某次队列的已审标记（重新开始走一遍）。
///
/// 刻意保留队列本身：护士长说「重来一遍」指的是重审，不是清空住院列表。
pub fn clear_reviewed(conn: &rusqlite::Connection, queue_id: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE queue_entries SET reviewed_at = NULL WHERE queue_id = ?1 AND reviewed_at IS NOT NULL",
        params![queue_id],
    )?;
    Ok(n)
}

/// 已审条数 / 总条数（底栏那一句「已审 8 / 共 14」）。
pub fn reviewed_progress(conn: &rusqlite::Connection, queue_id: &str) -> Result<(usize, usize)> {
    let (done, total): (i64, i64) = conn.query_row(
        "SELECT
           COALESCE(SUM(CASE WHEN reviewed_at IS NOT NULL THEN 1 ELSE 0 END), 0),
           COUNT(*)
         FROM queue_entries WHERE queue_id = ?1",
        params![queue_id],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?;
    Ok((done as usize, total as usize))
}

/// 删掉某次队列（连带条目）。
pub fn delete_queue(conn: &rusqlite::Connection, queue_id: &str) -> Result<usize> {
    conn.execute(
        "DELETE FROM queue_entries WHERE queue_id = ?1",
        params![queue_id],
    )?;
    let n = conn.execute(
        "DELETE FROM review_queues WHERE queue_id = ?1",
        params![queue_id],
    )?;
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Store;

    fn snap(id: &str, path: &str, entries: &[(&str, usize, usize)]) -> QueueSnapshot {
        QueueSnapshot {
            session: QueueSession {
                queue_id: id.into(),
                source_path: path.into(),
                loaded_at: "2026-03-02T08:00:00+08:00".into(),
                ruleset_short_sha256: "abc123abc123".into(),
                packages_found: entries.len(),
                files_seen: entries.len() + 2,
                items_total: entries.iter().map(|(_, d, _)| *d).sum(),
                missing_total: entries.iter().map(|(_, _, m)| *m).sum(),
                file_skips_json: "[]".into(),
                last_scanned_at: "2026-03-02T08:00:00+08:00".into(),
            },
            entries: entries
                .iter()
                .enumerate()
                .map(|(i, (a, d, m))| QueueEntryRow {
                    encounter_alias: (*a).into(),
                    ward: "WARD-A".into(),
                    doc_count: *d,
                    missing_count: *m,
                    reviewed_at: None,
                    position: i,
                })
                .collect(),
        }
    }

    #[test]
    fn the_queue_id_separates_the_same_folder_loaded_twice() {
        // 同一文件夹上午拖一次、下午再拖一次 = 两次载入，进度各留一份
        let morning = queue_id("/export/ward-a", "2026-03-02T08:00:00+08:00");
        let afternoon = queue_id("/export/ward-a", "2026-03-02T14:00:00+08:00");
        assert_ne!(morning, afternoon);
        assert_eq!(morning, queue_id("/export/ward-a", "2026-03-02T08:00:00+08:00"));
        // 分隔符防撞：路径与时刻的切分不能混淆
        assert_ne!(queue_id("/a|b", "c"), queue_id("/a", "b|c"));
    }

    #[test]
    fn a_queue_survives_reopening_the_database() {
        let dir = std::env::temp_dir().join("psychdoc-queue-persist");
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("psychdoc.sqlite3");
        {
            let store = Store::open(&db).unwrap();
            let s = snap("Q1", "/export/ward-a", &[("WARD-A-001", 4, 2), ("WARD-A-002", 3, 0)]);
            assert_eq!(save_queue(store.conn(), &s).unwrap(), 2);
        }
        // 重开（模拟关窗再开）
        let store = Store::open(&db).unwrap();
        let back = load_queue(store.conn(), "Q1").unwrap().expect("队列不该丢");
        assert_eq!(back.entries.len(), 2);
        assert_eq!(back.entries[0].encounter_alias, "WARD-A-001");
        assert_eq!(back.entries[0].missing_count, 2);
        assert_eq!(back.session.source_path, "/export/ward-a");
        assert!(back.entries.iter().all(|e| e.reviewed_at.is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marking_reviewed_survives_a_reopen_and_reports_progress() {
        let dir = std::env::temp_dir().join("psychdoc-queue-reviewed");
        let _ = std::fs::remove_dir_all(&dir);
        let db = dir.join("psychdoc.sqlite3");
        {
            let store = Store::open(&db).unwrap();
            save_queue(store.conn(), &snap("Q1", "/e", &[("A-1", 1, 0), ("A-2", 1, 1)])).unwrap();
            mark_reviewed(store.conn(), "Q1", &["A-1".into()], "2026-03-02T09:00:00+08:00").unwrap();
        }
        let store = Store::open(&db).unwrap();
        assert_eq!(reviewed_progress(store.conn(), "Q1").unwrap(), (1, 2));
        let back = load_queue(store.conn(), "Q1").unwrap().unwrap();
        assert_eq!(back.entries[0].reviewed_at.as_deref(), Some("2026-03-02T09:00:00+08:00"));
        assert_eq!(back.entries[1].reviewed_at, None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}