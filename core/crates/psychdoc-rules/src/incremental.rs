//! 增量重跑：**件级内容哈希 → 只重跑变过的件**。
//!
//! ## 源产品主能力与保留
//!
//! 源产品在 README 与缓存层的设计意图是「Built-in caching, to avoid
//! re-analyzing unchanged files」：同一批文件再次扫描时，**内容没变的不再
//! 重新分析**。本轮只采纳这条**设计意图与数据结构**（键 = 稳定身份 + 内容摘要，
//! 值 = 上一次的判定结果），不搬源仓库缓存层源码——那一层依赖 `salsa`
//! 的增量查询引擎与文件监听，与本项目零交集。
//!
//! ## 为什么不能简单地「按件缓存」
//!
//! 本项目的判定分两层，两层的失效条件不同：
//!
//! | 层 | 粒度 | 失效条件 |
//! |---|---|---|
//! | 执行器 A（件-字段类） | **单件** | 该件内容哈希变了 |
//! | 执行器 B（跨文书时间序 / 签署齐备性） | **一次住院的全部件** | 组内任一件哈希变了 |
//!
//! 执行器 B 要把同一次住院的多件 join 成一条事件时间线，**只看一件的哈希
//! 是不够的**：护士长改了约束记录的起始时刻，解除记录的时序结论也跟着变。
//! 因此本模块同时维护两张缓存：
//!
//! - [`ReviewCache::items`]：件级，键 `doc_id`，值含该件内容哈希；
//! - [`ReviewCache::groups`]：住院级，键 `encounter_alias`，值是**组指纹**
//!   （组内全部 `(doc_id, 内容哈希)` 排序后取 sha256）。
//!
//! 组指纹变了 → 整组重跑。组指纹没变 → 复用上一次的跨件结论。
//!
//! ## 第三层失效条件：规则集本身变了（Task 26）
//!
//! 上面两层失效条件都只看**文书内容**。但同一份没动过的文书，规则集一改
//! 结论就变：院内把「患方签字」从提示改成阻断，昨天判「提示」的件今天必须
//! 判「阻断」。若缓存只认内容，就会出现真实事故——
//! **按旧版规则审的结果被当新版复用**，护士长拿着一份已经不合规的清单
//! 去交班。
//!
//! 因此每条缓存条目与整张表都记着产出它时的规则集 `sha256`，
//! 规则集一变即整表作废，并在运行摘要里显式记一条
//! [`InvalidationRecord`]（「因规则集变更全量重审」）。取用时还要**逐字段
//! 校验**（[`ReviewCache::item_hit`]）：任一字段对不上即判未命中。
//!
//! ## 三条不可让步的行为
//!
//! 1. **缓存只影响「算不算」，不影响「算得对不对」**：增量结果与全量结果
//!    必须逐条相等（由 `tests/incremental_batch.rs` 的对照测试守住）。
//! 2. **跳过数必须是真的**：命中数来自实际比对，不是估算；底栏显示的
//!    「跳过 N 件」是缓存真实命中的件数。
//! 3. **哈希覆盖判定所需的全部输入**：`DocItem` 全文序列化后取 sha256，
//!    包含字段值、来源区间、时间戳与绑定态——任一处改动都会让哈希变。
//! 4. **宁可重跑，不可误命中**：任何一道校验（哈希、字段、规则集）过不去，
//!    一律按未命中处理。重跑的代价是几秒，误命中的代价是把不合规的文书
//!    放过去。

use std::collections::BTreeMap;
use std::path::Path;

use psychdoc_model::{Diagnostic, DocItem};
use serde::{Deserialize, Serialize};

use crate::cross_rules::{run_cross_doc, CrossOutcome};
use crate::field_rules::{run_item, AmbiguousField};
use crate::ruleset::{sha256_hex, EffectiveRules};
use crate::ReviewResult;

/// 件级缓存条目：某件在某份内容下判出来的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ItemCacheEntry {
    /// 该件的内容哈希（与键里的第二段冗余存一份，便于人工排查缓存内容）。
    ///
    /// **这份冗余不是装饰**：命中时逐字段比对，键与字段对不上就作废（见
    /// [`ReviewCache::item_hit`]）。只信键的话，一次哈希碰撞或一份被手改过的
    /// 缓存文件就能让「甲件的结论」被当成「乙件的结论」发出去。
    pub content_sha256: String,
    /// 产出这条结果时生效的**规则集 sha256**。
    ///
    /// 这是「按旧版规则审的结果被当新版复用」这一真实事故的唯一拦路石：
    /// 规则集一改，结论就变，内容没变也必须重审（Task 26）。
    ///
    /// `#[serde(default)]` 是为了读**旧版缓存文件**：那份文件里没有这个字段，
    /// 反序列化出空串。空串同样判为「不是当前规则集产出的」，照样作废——
    /// 宁可多跑一遍，不可拿来源不明的结论当合规。
    #[serde(default)]
    pub ruleset_sha256: String,
    pub diagnostics: Vec<Diagnostic>,
    pub ambiguous_fields: Vec<AmbiguousField>,
}

/// 住院级缓存条目：某次住院在其全部件内容下的跨件判定结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupCacheEntry {
    /// 组指纹：组内全部 `(doc_id, 内容哈希)` 排序后取 sha256。
    pub fingerprint: String,
    /// 产出这条结果时生效的规则集 sha256（同 [`ItemCacheEntry::ruleset_sha256`]）。
    #[serde(default)]
    pub ruleset_sha256: String,
    pub diagnostics: Vec<Diagnostic>,
    pub skipped_checks: Vec<crate::cross_rules::SkippedCheck>,
    /// 落在并列容差内、未判违规的件对（Task 19）。必须随缓存一起存：
    /// 缓存命中的那一次运行，护士长看到的「为什么没报」不能凭空少一条。
    #[serde(default)]
    pub tolerated: Vec<crate::chronology::ToleratedPair>,
    /// 该次住院的事件时间线（Task 19）。同上，缓存命中时原样回放。
    #[serde(default)]
    pub timelines: Vec<crate::chronology::EventTimeline>,
}

/// 每个 `doc_id` / `encounter_alias` 保留几个历史版本。
///
/// 保留 >1 个不是为了「命中率好看」，而是为了**撤销一次修改不必等全量重跑**：
/// 护士长误改了一份护理记录又改回来，内容哈希回到改之前的值——
/// 只存「最新一个」的话这一步会白白重跑一遍。存 2 个刚好覆盖
/// 「改 → 改回」这个最常见的往返，同时把缓存体积钉住（再多就是历史包袱）。
const HISTORY_DEPTH: usize = 2;

/// 一次「因规则集变更而整表作废」的留痕。
///
/// 「为什么这次一件都没走缓存」必须在结果里答得出来：护士长看到
/// 「重跑 204 件 / 跳过 0 件」会以为工具坏了，实际上是院内上午刚改了
/// 规则集第 4.2 条。没有这条记录，这次全量重审就无法与「缓存坏了」
/// 区分开。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InvalidationRecord {
    /// 作废前缓存整表所属的规则集短哈希（首轮或空表时为空串）。
    pub from_ruleset_short_sha256: String,
    /// 作废后（即本次运行所用）的规则集短哈希。
    pub to_ruleset_short_sha256: String,
    pub dropped_items: usize,
    pub dropped_groups: usize,
}

impl InvalidationRecord {
    pub fn as_zh(&self) -> String {
        format!(
            "因规则集变更全量重审：规则集由 {} 变为 {}，作废件级缓存 {} 条 / 住院组级缓存 {} 条",
            if self.from_ruleset_short_sha256.is_empty() {
                "（空缓存）".to_string()
            } else {
                self.from_ruleset_short_sha256.clone()
            },
            self.to_ruleset_short_sha256,
            self.dropped_items,
            self.dropped_groups
        )
    }
}

/// 增量缓存表。可整体序列化到磁盘，供常驻进程跨次运行复用。
///
/// **键的形状**：件级 `(doc_id, 内容哈希)`、住院级 `(alias, 组指纹)`。
/// 内容进了键而不是只存一个「当前值」，是为了让「改回去」也能命中
/// （见 [`HISTORY_DEPTH`]）。
///
/// **但键只回答了「内容变没变」**。判定结果还取决于**规则集**：院内上午把
/// 「患方签字」从提示改成阻断，同一份没动过的文书，结论就从「提示」变成
/// 「阻断」。因此每条条目与整张表都记着产出它时的规则集 sha256，
/// 规则集一变即整表作废（[`ReviewCache::invalidate_for_ruleset`]）。
///
/// **本轮不做**的事：写缓存与写审查结果的同事务提交（sqlite 侧由
/// `psychdoc-store` 承担，见 Task 26 的 `commit_review_with_cache`）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReviewCache {
    pub items: BTreeMap<(String, String), ItemCacheEntry>,
    pub groups: BTreeMap<(String, String), GroupCacheEntry>,
    /// 本表**整表**是在哪一版规则集下产生的。空串 = 不知道（空缓存或旧版文件）。
    #[serde(default)]
    pub ruleset_sha256: String,
    /// 历次整表作废的留痕（随缓存一起落盘，重启后仍能回答「当时为什么全量重审」）。
    #[serde(default)]
    pub invalidations: Vec<InvalidationRecord>,
}

impl ReviewCache {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.items.len()
    }

    pub fn is_empty(&self) -> bool {
        self.items.is_empty() && self.groups.is_empty()
    }

    /// 从磁盘读缓存。文件不存在 = 首次运行，返回空缓存（**不是错误**）。
    pub fn load(path: &Path) -> Result<Self, String> {
        if !path.exists() {
            return Ok(Self::new());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("E_IO: 读不到增量缓存 {} — {e}", path.display()))?;
        if text.trim().is_empty() {
            return Ok(Self::new());
        }
        let disk: CacheOnDisk = serde_json::from_str(&text)
            .map_err(|e| format!("E_CACHE_CORRUPT: 增量缓存 {} 解析失败 — {e}", path.display()))?;
        Ok(Self {
            items: disk.items.into_iter().map(|e| ((e.doc_id, e.hash), e.entry)).collect(),
            groups: disk
                .groups
                .into_iter()
                .map(|e| ((e.alias, e.hash), e.entry))
                .collect(),
            ruleset_sha256: disk.ruleset_sha256,
            invalidations: disk.invalidations,
        })
    }

    /// 落盘。写入**先写临时文件再改名**，避免进程被杀时留下半张缓存表。
    ///
    /// 改名只是原子地换文件名，**不保证内容已落盘**——断电时可能得到
    /// 一个长度完整但内容全零的文件。因此临时文件写完先 `sync_all`
    /// 让数据真的进设备，再改名，最后把目录项也刷一次。
    /// 三步各挡一种「进程被杀 / 断电」：不刷文件 = 内容还在页缓存里；
    /// 不刷目录 = 改名本身没落盘，重启后回到旧文件。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("E_IO: 建不了缓存目录 {} — {e}", parent.display()))?;
        }
        let disk = CacheOnDisk {
            ruleset_sha256: self.ruleset_sha256.clone(),
            invalidations: self.invalidations.clone(),
            items: self
                .items
                .iter()
                .map(|((doc_id, hash), entry)| ItemOnDisk {
                    doc_id: doc_id.clone(),
                    hash: hash.clone(),
                    entry: entry.clone(),
                })
                .collect(),
            groups: self
                .groups
                .iter()
                .map(|((alias, hash), entry)| GroupOnDisk {
                    alias: alias.clone(),
                    hash: hash.clone(),
                    entry: entry.clone(),
                })
                .collect(),
        };
        let body = serde_json::to_string_pretty(&disk)
            .map_err(|e| format!("E_CACHE_SERIALIZE: 增量缓存序列化失败 — {e}"))?;
        let tmp = path.with_extension("json.tmp");
        {
            let mut f = std::fs::File::create(&tmp)
                .map_err(|e| format!("E_IO: 写不了临时缓存 {} — {e}", tmp.display()))?;
            use std::io::Write;
            f.write_all(body.as_bytes())
                .map_err(|e| format!("E_IO: 写临时缓存 {} 失败 — {e}", tmp.display()))?;
            f.sync_all()
                .map_err(|e| format!("E_IO: 临时缓存 {} 落盘失败 — {e}", tmp.display()))?;
        }
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("E_IO: 缓存落盘失败 {} — {e}", path.display()))?;
        // 目录项本身也要落盘，否则改名可能在断电后丢失。
        if let Some(parent) = path.parent() {
            if let Ok(dir) = std::fs::File::open(if parent.as_os_str().is_empty() {
                std::path::Path::new(".")
            } else {
                parent
            }) {
                let _ = dir.sync_all();
            }
        }
        Ok(())
    }

    /// 规则集变更时整表作废，并留一条可核对的记录。
    ///
    /// 返回 `None` 表示规则集没变、缓存原样保留。
    ///
    /// **为什么必须整表作废而不是逐条比对**：条目里虽然记着产出它时的
    /// 规则集哈希，但「同一份规则集文件在两次运行之间是否语义等价」无法
    /// 从哈希判断——哈希变了就是变了，宁可多审一次。半吊子的做法（只重审
    /// 命中过新规则的件）会在「规则集新增了一条规则」时把旧结论原样留下，
    /// 那正是「按旧版规则审的结果被当新版复用」的事故。
    pub fn invalidate_for_ruleset(&mut self, ruleset_sha256: &str) -> Option<InvalidationRecord> {
        // 空表：本次没有任何结论会被复用，也就没什么可作废的。此时只补上
        // 规则集指纹，不记一条「因规则集变更全量重审」——那会让第一次用的人
        // 以为工具出了问题。底栏不该为「本来就没有缓存可省」制造告警。
        if self.items.is_empty() && self.groups.is_empty() {
            self.ruleset_sha256 = ruleset_sha256.to_string();
            return None;
        }
        if self.ruleset_sha256 == ruleset_sha256 {
            return None;
        }
        let record = InvalidationRecord {
            from_ruleset_short_sha256: short_of(&self.ruleset_sha256),
            to_ruleset_short_sha256: short_of(ruleset_sha256),
            dropped_items: self.items.len(),
            dropped_groups: self.groups.len(),
        };
        self.items.clear();
        self.groups.clear();
        self.ruleset_sha256 = ruleset_sha256.to_string();
        self.invalidations.push(record.clone());
        // 留痕只留最近若干次：它是排障线索，不是可以无限增长的日志。
        // 一条缓存失效记录就已经能回答「当时为什么全量重审」了。
        const KEEP: usize = 8;
        if self.invalidations.len() > KEEP {
            let cut = self.invalidations.len() - KEEP;
            self.invalidations.drain(0..cut);
        }
        Some(record)
    }

    /// 取件级缓存条目，**逐字段校验**后才认。
    ///
    /// 三道校验，缺一不可：
    ///
    /// 1. 键里的哈希与条目自带的 `content_sha256` 必须一致——防哈希碰撞、
    ///    防手改过的缓存文件把甲件的结论塞进乙件的键下；
    /// 2. 条目的 `ruleset_sha256` 必须等于本次所用规则集——防旧版规则的
    ///    结论被当新版复用；
    /// 3. 条目自带的 `content_sha256` 必须等于调用方算出来的哈希——
    ///    同一道碰撞保护的另一半：键对、字段对，但两者都不是「这份内容」的
    ///    哈希时（人为构造的夹具），一律判未命中。
    ///
    /// 任何一道不符都返回 `None`（未命中），由调用方重跑。**重跑永远
    /// 是安全的，误命中永远不是**——这是本函数唯一的设计原则。
    pub fn item_hit(&self, doc_id: &str, content_sha256: &str, ruleset_sha256: &str) -> Option<&ItemCacheEntry> {
        let entry = self.items.get(&(doc_id.to_string(), content_sha256.to_string()))?;
        if entry.content_sha256 != content_sha256 || entry.ruleset_sha256 != ruleset_sha256 {
            return None;
        }
        Some(entry)
    }

    /// 组级取用，校验口径与 [`Self::item_hit`] 相同。
    pub fn group_hit(&self, alias: &str, fingerprint: &str, ruleset_sha256: &str) -> Option<&GroupCacheEntry> {
        let entry = self.groups.get(&(alias.to_string(), fingerprint.to_string()))?;
        if entry.fingerprint != fingerprint || entry.ruleset_sha256 != ruleset_sha256 {
            return None;
        }
        Some(entry)
    }
}

/// 取 12 位短哈希；空串原样返回（空串在文案里表示「不知道」而不是「哪一版」）。
fn short_of(sha: &str) -> String {
    sha.chars().take(crate::ruleset::SHA256_SHORT_LEN).collect()
}

/// 缓存的**磁盘形态**：把 `(身份, 哈希) → 结果` 摊平成条目列表。
///
/// 内存里用元组键是为了查找直接；JSON 的对象键只能是字符串，元组键写不进去。
/// 因此这里显式定义磁盘结构，而不是把内部结构直接 `serde` 出去——
/// 顺带让缓存文件在人工排障时**一眼可读**（`doc_id` / `hash` 是两个独立字段，
/// 不需要人去拆拼接出来的键）。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheOnDisk {
    /// 整表所属规则集（Task 26 起有；旧版文件读出空串 → 触发整表作废）。
    #[serde(default)]
    ruleset_sha256: String,
    #[serde(default)]
    invalidations: Vec<InvalidationRecord>,
    items: Vec<ItemOnDisk>,
    groups: Vec<GroupOnDisk>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ItemOnDisk {
    doc_id: String,
    hash: String,
    entry: ItemCacheEntry,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GroupOnDisk {
    alias: String,
    hash: String,
    entry: GroupCacheEntry,
}

/// 件内容哈希：`DocItem` 全文的稳定序列化取 sha256。
///
/// 序列化顺序由结构体字段顺序决定（无 `HashMap`），因此同一份件在任何机器上
/// 哈希一致——这条性质是「换台机器接着审」的必要条件。
pub fn content_sha256(item: &DocItem) -> String {
    match serde_json::to_vec(item) {
        Ok(bytes) => sha256_hex(&bytes),
        // `DocItem` 的字段都是可序列化的标量/Vec/BTreeMap，序列化失败属于
        // 编程错误而不是数据问题；退回「必然不同的哈希」而不是 panic，
        // 保证一次异常不会把整批审查带崩。
        Err(_) => sha256_hex(format!("<unserializable:{}>", item.doc_id).as_bytes()),
    }
}

/// 组指纹：组内全部 `(doc_id, 内容哈希)` 排序后取 sha256。
///
/// 排序保证与件在包里的出现顺序无关——同一组件换个导出顺序，指纹不变。
pub fn group_fingerprint<'a>(items: impl IntoIterator<Item = &'a DocItem>) -> String {
    let mut parts: Vec<String> = items
        .into_iter()
        .map(|i| format!("{}={}", i.doc_id, content_sha256(i)))
        .collect();
    parts.sort();
    sha256_hex(parts.join("\n").as_bytes())
}

/// 一次增量批扫的运行摘要（UI 底栏 / CLI 输出共用同一份）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunSummary {
    /// 本次传入的件总数。
    pub items_total: usize,
    /// 真正重跑的件数（缓存未命中）。
    pub items_reviewed: usize,
    /// 件级缓存命中、直接跳过的件数。
    pub items_skipped: usize,
    pub groups_total: usize,
    /// 组指纹变化、需要重跑跨件规则的就诊组数。
    pub groups_reviewed: usize,
    /// 组指纹未变、复用上次跨件结论的就诊组数。
    pub groups_skipped: usize,
    pub ruleset_version: String,
    pub ruleset_sha256: String,
    pub ruleset_short_sha256: String,
    /// 未解析 / 被过滤的件数。**永远显式带上**，否则「0 条诊断」会被误读成全合规。
    pub unresolved_count: usize,
    /// 件数够、但类型码不在本院文书类型字典内的件数（见 `review_incremental`）。
    pub unmatched_type_count: usize,
    /// 本次是否因规则集变更整表作废了缓存（Task 26）。
    ///
    /// 底栏要能回答「为什么这次一件都没走缓存」——答案是「规则集改了」，
    /// 而不是让护士长以为工具坏了。
    #[serde(default)]
    pub invalidated: Option<InvalidationRecord>,
}

impl RunSummary {
    /// 件级缓存命中率（0.0–1.0）。首次运行为 0。
    ///
    /// 分母用 [`Self::items_adjudicated`] 而不是 `items_total`：类型不在字典内的件
    /// 压根没进规则层，把它算进分母会让命中率**凭空低下去**——护士长看到
    /// 「命中率 94%」会以为有 6% 的件没审到，实际那 6% 是转诊单之类的非文书件。
    /// 命中率要回答的是「进了规则层的件里，有多少走了缓存」。
    pub fn hit_rate(&self) -> f64 {
        let judged = self.items_adjudicated();
        if judged == 0 {
            return 0.0;
        }
        self.items_skipped as f64 / judged as f64
    }

    /// 底栏一行（UI 与 CLI 同文，避免两处说法不一致）。
    /// 走规则层的件数 = 重跑 + 跳过。
    pub fn items_adjudicated(&self) -> usize {
        self.items_reviewed + self.items_skipped
    }

    /// 三项之和必须等于传入总件数——**账要平**。
    ///
    /// 「总数 ≠ 重跑 + 跳过 + 未解析 + 类型不匹配」意味着有件在系统里
    /// 既没被判、也没被记账：这正是「静默跳过」的定义。UI 与 CI 都应能查这条。
    pub fn is_balanced(&self) -> bool {
        self.items_total
            == self.items_adjudicated() + self.unresolved_count + self.unmatched_type_count
    }

    pub fn as_zh(&self) -> String {
        let head = format!(
            "重跑 {} 件 / 跳过 {} 件（命中率 {:.0}%）· 住院组 重跑 {}/跳过 {} · 规则集 v{}（{}）· 未解析 {} 件 · 类型不在字典 {} 件",
            self.items_reviewed,
            self.items_skipped,
            self.hit_rate() * 100.0,
            self.groups_reviewed,
            self.groups_skipped,
            self.ruleset_version,
            self.ruleset_short_sha256,
            self.unresolved_count,
            self.unmatched_type_count,
        );
        match &self.invalidated {
            Some(rec) => format!("{head} · {}", rec.as_zh()),
            None => head,
        }
    }
}

/// 一次增量批扫的结果：判定结果 + 运行摘要 + 就地更新的缓存。
#[derive(Debug, Clone)]
pub struct IncrementalRun {
    pub result: ReviewResult,
    pub summary: RunSummary,
}

/// 同一 `doc_id` 下只保留最近 [`HISTORY_DEPTH`] 个内容版本。
///
/// 保留历史是为了「改回去能命中」；**丢弃更老的版本**是为了不让缓存无限增长
/// ——一个被反复编辑十次的件不该在缓存表里留下十份历史结果。
///
/// 判据用**哈希字典序**而不是写入时间：内容哈希是无意义的十六进制串，
/// 没有「更旧」可言，唯一的稳定排序就是它。这条取舍的代价是：被裁掉的
/// 未必是时间上最老的那份。但代价可接受——淘汰后最多让那一次编辑退化成
/// 全量重跑，不会算错；而若保留全部历史，缓存体积会随编辑次数无界增长，
/// 那是常驻工具真正会踩的坑。
fn prune_item_history(cache: &mut ReviewCache, doc_id: &str) {
    let mut keys: Vec<(String, String)> = cache
        .items
        .keys()
        .filter(|(d, _)| d == doc_id)
        .cloned()
        .collect();
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    let cut = keys.len().saturating_sub(HISTORY_DEPTH);
    for k in keys.into_iter().take(cut) {
        cache.items.remove(&k);
    }
}

/// 同一 `encounter_alias` 下只保留最近 [`HISTORY_DEPTH`] 个组指纹。
fn prune_group_history(cache: &mut ReviewCache, alias: &str) {
    let mut keys: Vec<(String, String)> = cache
        .groups
        .keys()
        .filter(|(a, _)| a == alias)
        .cloned()
        .collect();
    keys.sort_by(|a, b| a.1.cmp(&b.1));
    let cut = keys.len().saturating_sub(HISTORY_DEPTH);
    for k in keys.into_iter().take(cut) {
        cache.groups.remove(&k);
    }
}

/// 对一批件跑规则，**未变过的件走缓存**。
///
/// `unresolved_count` 由调用方传入（解析层已知的未解析 / 被过滤件数），
/// 写进摘要以免「缓存跳过」被当成「全部合规」。
///
/// 结果与 [`crate::review`]（全量）在诊断集合上**逐条相等**——
/// 增量只改变「算了多少遍」，不改变「算出什么」。
pub fn review_incremental(
    items: &[DocItem],
    ruleset: &EffectiveRules,
    cache: &mut ReviewCache,
    unresolved_count: usize,
) -> IncrementalRun {
    // 规则集一变，先整表作废再谈命中。放在最前面是刻意的：
    // 若先跑命中、后作废，本次运行会拿旧版结论凑出一份「高命中率」，
    // 而底栏的命中率是护士长判断「要不要信这次结果」的依据。
    let invalidated = cache.invalidate_for_ruleset(&ruleset.sha256);

    let mut result = ReviewResult {
        items_reviewed: items.len(),
        ruleset_version: ruleset.version.clone(),
        ruleset_sha256: ruleset.sha256.clone(),
        ..Default::default()
    };

    let mut items_reviewed = 0usize;
    let mut items_skipped = 0usize;
    let mut unmatched_type_count = 0usize;

    // —— 执行器 A：件级 ——
    for item in items {
        // 类型码不在字典内的件解析层已记账，这里不重复造诊断，直接跳过。
        // 它既不算「重跑」也不算「跳过」：它压根没进规则层——但**必须计数**，
        // 否则「总数 ≠ 重跑 + 跳过」会让底栏的账对不上（见 `is_balanced`）。
        let Some(tpl) = ruleset.templates.get(&item.template_id) else {
            unmatched_type_count += 1;
            continue;
        };
        let sha = content_sha256(item);
        let entry = match cache.item_hit(&item.doc_id, &sha, &ruleset.sha256) {
            Some(e) => {
                items_skipped += 1;
                e.clone()
            }
            None => {
                items_reviewed += 1;
                let (diagnostics, ambiguous_fields) = run_item(item, tpl, ruleset);
                let fresh = ItemCacheEntry {
                    content_sha256: sha.clone(),
                    ruleset_sha256: ruleset.sha256.clone(),
                    diagnostics,
                    ambiguous_fields,
                };
                cache.items.insert((item.doc_id.clone(), sha), fresh.clone());
                prune_item_history(cache, &item.doc_id);
                fresh
            }
        };
        result.diagnostics.extend(entry.diagnostics);
        result.ambiguous_fields.extend(entry.ambiguous_fields);
    }

    // —— 执行器 B：住院组级 ——
    let mut groups_reviewed = 0usize;
    let mut groups_skipped = 0usize;
    for group in crate::cross_rules::group_by_encounter(items) {
        let group_items: Vec<DocItem> = group.items.iter().map(|i| (*i).clone()).collect();
        let fingerprint = group_fingerprint(group_items.iter());
        let outcome = match cache.group_hit(group.alias, &fingerprint, &ruleset.sha256) {
            Some(e) => {
                groups_skipped += 1;
                CrossOutcome {
                    diagnostics: e.diagnostics.clone(),
                    skipped: e.skipped_checks.clone(),
                    tolerated: e.tolerated.clone(),
                    timelines: e.timelines.clone(),
                }
            }
            None => {
                groups_reviewed += 1;
                let fresh = run_cross_doc(&group_items, ruleset);
                cache.groups.insert(
                    (group.alias.to_string(), fingerprint.clone()),
                    GroupCacheEntry {
                        fingerprint,
                        ruleset_sha256: ruleset.sha256.clone(),
                        diagnostics: fresh.diagnostics.clone(),
                        skipped_checks: fresh.skipped.clone(),
                        tolerated: fresh.tolerated.clone(),
                        timelines: fresh.timelines.clone(),
                    },
                );
                prune_group_history(cache, group.alias);
                fresh
            }
        };
        result.diagnostics.extend(outcome.diagnostics);
        result.skipped_checks.extend(outcome.skipped);
        // 「容差内」与「事件时间线」同样必须回填。早先这里只 extend 了
        // 前两项，于是**带 --cache 的批扫比不带 --cache 的全量少一整段输出**：
        // 护士长看到的「在 5 分钟并列容差内、未判倒挂」与事件时间线都消失了，
        // 而「为什么这两件只差 3 分钟却没报时序倒挂」正是靠容差那条回答的。
        // 少这一段 = 增量结果与全量结果不相等，与本模块头部的第一不变量冲突。
        result.tolerated_checks.extend(outcome.tolerated);
        // 时间线按住院别名累计：缓存命中的组也要把它上次的原样回放，
        // 否则「这一次批扫的时间线」会因为命中了几组而少几段。
        result.timelines.extend(outcome.timelines);
    }

    // 去重择优与全量走同一个函数：增量只改变「算了多少遍」，
    // 不改变「算出什么」，因此折叠结果也必须逐条相等。
    crate::apply_dedup(&mut result, ruleset);

    let summary = RunSummary {
        items_total: items.len(),
        items_reviewed,
        items_skipped,
        groups_total: groups_reviewed + groups_skipped,
        groups_reviewed,
        groups_skipped,
        ruleset_version: ruleset.version.clone(),
        ruleset_sha256: ruleset.sha256.clone(),
        ruleset_short_sha256: ruleset.short_sha256().to_string(),
        unresolved_count,
        unmatched_type_count,
        invalidated,
    };
    IncrementalRun { result, summary }
}

#[cfg(test)]
mod tests {
    use super::*;
    use psychdoc_model::Section;

    /// 造一个「12 位短哈希就能区分版本」的 64 位串：`sha_n(7) != sha_n(8)`。
    ///
    /// 用 `format!("{i:0>64}")` 也能造出不同的 64 位串，但数字落在**末尾**，
    /// 12 位短哈希全是 0 —— 那就测不出「按短哈希区分版本」这件事。
    fn sha_n(n: u32) -> String {
        format!("{n:0>12}{}", "a".repeat(52))
    }

    fn item(id: &str, alias: &str, text: &str) -> DocItem {
        DocItem::new(id, "restraint_consent", alias)
            .with_sections(vec![Section {
                index: 0,
                title: "患方知情同意".into(),
                text: text.into(),
            }])
    }

    #[test]
    fn content_hash_is_stable_for_identical_items() {
        let a = item("A-1", "W-1", "患方意见：同意。");
        let b = item("A-1", "W-1", "患方意见：同意。");
        assert_eq!(content_sha256(&a), content_sha256(&b));
        assert_eq!(content_sha256(&a).len(), 64);
    }

    #[test]
    fn content_hash_flips_on_any_field_change() {
        let a = item("A-1", "W-1", "患方意见：同意。");
        let b = item("A-1", "W-1", "患方意见：不同意。");
        assert_ne!(content_sha256(&a), content_sha256(&b));
    }

    #[test]
    fn group_fingerprint_ignores_item_order() {
        let a = item("A-1", "W-1", "患方意见：同意。");
        let b = item("A-2", "W-1", "患方意见：同意。");
        assert_eq!(
            group_fingerprint([&a, &b]),
            group_fingerprint([&b, &a]),
            "组指纹不应随导出顺序变化"
        );
        let c = item("A-2", "W-1", "患方意见：不同意。");
        assert_ne!(group_fingerprint([&a, &b]), group_fingerprint([&a, &c]));
    }

    #[test]
    fn cache_round_trips_through_disk() {
        let mut cache = ReviewCache::new();
        cache.ruleset_sha256 = "f".repeat(64);
        cache.items.insert(
            ("A-1".into(), "abc".into()),
            ItemCacheEntry {
                content_sha256: "abc".into(),
                ruleset_sha256: "f".repeat(64),
                diagnostics: vec![],
                ambiguous_fields: vec![],
            },
        );
        let dir = std::env::temp_dir().join("psychdoc-cache-roundtrip");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("cache.json");
        cache.save(&path).unwrap();
        let loaded = ReviewCache::load(&path).unwrap();
        assert_eq!(loaded, cache);
        assert!(!dir.join("cache.json.tmp").exists(), "落盘不应留下临时文件");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_cache_file_is_first_run_not_an_error() {
        let p = std::env::temp_dir().join("psychdoc-cache-absent-dir/x.json");
        let c = ReviewCache::load(&p).unwrap();
        assert!(c.is_empty());
    }

    // ------------------------------------------------------------------
    // 规则集变更失效（Task 26）
    // ------------------------------------------------------------------

    #[test]
    fn a_changed_ruleset_invalidates_the_whole_table() {
        let mut cache = ReviewCache::new();
        cache.ruleset_sha256 = "a".repeat(64);
        for i in 0..5 {
            cache.items.insert(
                (format!("A-{i}"), "h".into()),
                ItemCacheEntry {
                    content_sha256: "h".into(),
                    ruleset_sha256: "a".repeat(64),
                    diagnostics: vec![],
                    ambiguous_fields: vec![],
                },
            );
        }
        cache.groups.insert(
            ("W-1".into(), "g".into()),
            GroupCacheEntry {
                fingerprint: "g".into(),
                ruleset_sha256: "a".repeat(64),
                diagnostics: vec![],
                skipped_checks: vec![],
                tolerated: vec![],
                timelines: vec![],
            },
        );

        let rec = cache
            .invalidate_for_ruleset(&"b".repeat(64))
            .expect("规则集变了必须整表作废");
        assert_eq!(rec.dropped_items, 5);
        assert_eq!(rec.dropped_groups, 1);
        assert_eq!(rec.from_ruleset_short_sha256, "a".repeat(12));
        assert_eq!(rec.to_ruleset_short_sha256, "b".repeat(12));
        assert!(cache.items.is_empty() && cache.groups.is_empty());
        assert_eq!(cache.ruleset_sha256, "b".repeat(64));
        let zh = rec.as_zh();
        assert!(zh.contains("因规则集变更全量重审"), "{zh}");
        assert!(zh.contains("作废件级缓存 5 条"), "{zh}");
    }

    #[test]
    fn the_same_ruleset_keeps_the_cache_and_records_nothing() {
        let mut cache = ReviewCache::new();
        let sha = "a".repeat(64);
        cache.ruleset_sha256 = sha.clone();
        cache.items.insert(
            ("A-1".into(), "h".into()),
            ItemCacheEntry {
                content_sha256: "h".into(),
                ruleset_sha256: sha.clone(),
                diagnostics: vec![],
                ambiguous_fields: vec![],
            },
        );
        assert!(cache.invalidate_for_ruleset(&sha).is_none());
        assert_eq!(cache.items.len(), 1, "规则集没变就不该动缓存");
        assert!(cache.invalidations.is_empty());
    }

    #[test]
    fn a_cold_cache_does_not_claim_a_ruleset_change() {
        // 首轮运行缓存是空的。若这时也报「因规则集变更全量重审」，
        // 第一次用的人会以为工具出了问题——底栏不该为「本来就没有缓存可省」
        // 制造一条告警。
        let mut cache = ReviewCache::new();
        assert!(cache.invalidate_for_ruleset(&"a".repeat(64)).is_none());
        assert!(cache.invalidations.is_empty());
    }

    #[test]
    fn a_legacy_cache_file_without_a_ruleset_stamp_is_invalidated() {
        // 旧版缓存文件没有 ruleset_sha256 字段 → 反序列化出空串 → 不认 →
        // 整表作废。真实形状：文件里**有**条目（空表谈不上复用旧结论）。
        let legacy = r#"{
            "items": [{"doc_id": "A-1", "hash": "h",
                       "entry": {"content_sha256": "h", "diagnostics": [], "ambiguous_fields": []}}],
            "groups": []
        }"#;
        let disk: CacheOnDisk = serde_json::from_str(legacy).unwrap();
        assert_eq!(disk.ruleset_sha256, "", "旧版文件读出的规则集指纹必须是空串");
        let mut cache = ReviewCache {
            items: disk
                .items
                .into_iter()
                .map(|e| ((e.doc_id, e.hash), e.entry))
                .collect(),
            groups: disk
                .groups
                .into_iter()
                .map(|e| ((e.alias, e.hash), e.entry))
                .collect(),
            ruleset_sha256: disk.ruleset_sha256,
            invalidations: disk.invalidations,
        };
        assert_eq!(cache.items.len(), 1, "旧版文件里的条目应被读进来");
        let rec = cache
            .invalidate_for_ruleset(&sha_n(1))
            .expect("来源不明的缓存不能被当成本次规则集的产物");
        assert_eq!(rec.dropped_items, 1);
        assert!(cache.items.is_empty());
        assert!(rec.as_zh().contains("（空缓存）"), "{}", rec.as_zh());
    }

    #[test]
    fn an_entry_from_another_ruleset_never_hits() {
        let sha_old = "a".repeat(64);
        let mut cache = ReviewCache::new();
        cache.ruleset_sha256 = sha_old.clone();
        cache.items.insert(
            ("A-1".into(), "h".into()),
            ItemCacheEntry {
                content_sha256: "h".into(),
                ruleset_sha256: sha_old,
                diagnostics: vec![],
                ambiguous_fields: vec![],
            },
        );
        // 绕过 invalidate_for_ruleset，直接问「新规则集能不能用这条缓存」——
        // 这是碰撞保护与误命中防护的最后一层，不能依赖调用方记得先作废。
        assert!(cache.item_hit("A-1", "h", &"b".repeat(64)).is_none());
        assert!(cache.item_hit("A-1", "h", &cache.ruleset_sha256).is_some());
    }

    #[test]
    fn an_entry_whose_own_field_disagrees_with_its_key_never_hits() {
        // 哈希碰撞 / 手改缓存文件：键里的哈希是 h，条目自带的却是别的。
        // 只信键的话，这条就会被当成「这份内容已审过」而跳过——等于把
        // 别的件的结论发出去。
        let sha = "a".repeat(64);
        let mut cache = ReviewCache::new();
        cache.ruleset_sha256 = sha.clone();
        cache.items.insert(
            ("A-1".into(), "h".into()),
            ItemCacheEntry {
                content_sha256: " somebody-elses-hash".into(),
                ruleset_sha256: sha.clone(),
                diagnostics: vec![],
                ambiguous_fields: vec![],
            },
        );
        assert!(cache.item_hit("A-1", "h", &sha).is_none());
    }

    #[test]
    fn a_group_entry_is_verified_the_same_way() {
        let sha = "a".repeat(64);
        let mut cache = ReviewCache::new();
        cache.ruleset_sha256 = sha.clone();
        cache.groups.insert(
            ("W-1".into(), "fp".into()),
            GroupCacheEntry {
                fingerprint: "fp".into(),
                ruleset_sha256: "b".repeat(64),
                diagnostics: vec![],
                skipped_checks: vec![],
                tolerated: vec![],
                timelines: vec![],
            },
        );
        assert!(cache.group_hit("W-1", "fp", &sha).is_none());
    }

    #[test]
    fn the_invalidation_trail_is_bounded() {
        let mut cache = ReviewCache::new();
        for i in 1..=12u32 {
            // 每次都得先有缓存可作废：空表走的是「首轮」分支，不记留痕。
            let sha = sha_n(i);
            cache.items.insert(
                (format!("A-{i}"), sha.clone()),
                ItemCacheEntry {
                    content_sha256: sha.clone(),
                    ruleset_sha256: sha,
                    diagnostics: vec![],
                    ambiguous_fields: vec![],
                },
            );
            assert!(
                cache.invalidate_for_ruleset(&sha_n(i + 100)).is_some(),
                "第 {i} 轮规则集确实变了，应记一条留痕"
            );
        }
        assert_eq!(
            cache.invalidations.len(),
            8,
            "失效留痕应有上限，否则缓存文件会随运行次数无限增长"
        );
        // 留的是**最近**若干次，不是最早的
        assert_eq!(
            cache.invalidations.last().unwrap().to_ruleset_short_sha256,
            short_of(&sha_n(112)),
            "最后一条应记第 12 轮"
        );
        assert_eq!(
            cache.invalidations.first().unwrap().to_ruleset_short_sha256,
            short_of(&sha_n(105)),
            "最早的 4 条应已被裁掉"
        );
    }

    #[test]
    fn summary_as_zh_carries_hit_rate_and_ruleset_fingerprint() {
        // items_total 只数「成功解析成 DocItem 的件」；未解析的件从未进入
        // 这个列表，因此它们单列而不是混进 items_total。
        let s = RunSummary {
            items_total: 203,
            items_reviewed: 1,
            items_skipped: 199,
            groups_total: 40,
            groups_reviewed: 1,
            groups_skipped: 39,
            ruleset_version: "0.4.0".into(),
            ruleset_sha256: "a".repeat(64),
            ruleset_short_sha256: "aaaaaaaaaaaa".into(),
            unresolved_count: 2,
            unmatched_type_count: 1,
            invalidated: None,
        };
        // 命中率按「进入规则层的件」算：200 件参与，199 件走缓存
        assert!((s.hit_rate() - 199.0 / 200.0).abs() < 1e-9);
        // 账要平：203 = 1 + 199 + 2 + 1
        assert!(s.is_balanced(), "203 件应能拆成重跑 1 + 跳过 199 + 未解析 2 + 类型不匹配 1");
        let zh = s.as_zh();
        assert!(zh.contains("重跑 1 件"));
        assert!(zh.contains("跳过 199 件"));
        assert!(zh.contains("未解析 2 件"), "未解析件数必须显式出现：{zh}");
        assert!(zh.contains("类型不在字典 1 件"), "类型不匹配也必须有账：{zh}");
    }

    #[test]
    fn unbalanced_summary_is_detectable() {
        // 「总数对不上」是静默跳过的定义，必须有一条可查询的不变量。
        let mut s = RunSummary {
            items_total: 10,
            items_reviewed: 3,
            items_skipped: 4,
            groups_total: 1,
            groups_reviewed: 1,
            groups_skipped: 0,
            ruleset_version: "0.4.0".into(),
            ruleset_sha256: "a".repeat(64),
            ruleset_short_sha256: "aaaaaaaaaaaa".into(),
            unresolved_count: 1,
            unmatched_type_count: 0,
            invalidated: None,
        };
        assert!(!s.is_balanced(), "10 ≠ 3 + 4 + 1 + 0，账不平");
        s.unmatched_type_count = 2;
        assert!(s.is_balanced());
    }
}