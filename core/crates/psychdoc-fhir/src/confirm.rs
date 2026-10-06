//! 确认写回闭环：**草稿 → 逐条勾选 → 整批 transaction → 状态推进**。
//!
//! 这是 `spec.md` §4「确认回写」「业务状态生命周期」两条闭环维度的落点。
//! 主链路一次跑通的顺序固定为：
//!
//! ```text
//! 建草稿（未生效）
//!   └─ 逐条勾选
//!        └─ 整批发送 Bundle transaction
//!             ├─ 成功 → 落写回日志 + 审计 + 推进件级状态（草稿→已建件）
//!             └─ 失败 → 0 条部分写入，草稿原样保留可重试
//! 撤回
//!   ├─ 已勾选未发送 → 回草稿区（清空勾选，0 次外发）
//!   └─ 已发送       → 反向 Bundle transaction（DELETE）+ 审计事件
//! ```
//!
//! ## 五条不可让步的不变量
//!
//! 1. **未勾选任何条 → 零次外发**。这不是 UI 提示，而是后端拦截：
//!    「没勾就点提交」在真实使用中天天发生，光靠按钮置灰挡不住脚本与竞态。
//! 2. **失败不静默改数据**。transaction 被拒或端点 5xx 时，本地
//!    **不写**「已写回」、**不推进**状态——那是最坏的一种假成功。
//! 3. **状态推进只走合法边**。非法跳变（如 `缺失 → 可归档`）由状态机
//!    拒绝并记 `E_STATE_JUMP`，既有状态**不改变**。
//! 4. **先占键、后外发**（Task 28）。并发确认在**发出请求之前**就被挡住：
//!    抢不到键的第二个进程一次外发都不发，而不是发出去再在本地撞约束。
//! 5. **重放不再外发**。同键重放原样回放首次 transaction 结果，
//!    `replayed=true`；上次结果未知（进程崩溃）时一律 `E_REPLAY_IN_DOUBT`，
//!    **不自动重发**——服务端可能已经建好件。
//!
//! Webhook 出站通知由 Task 30 承担。

use crate::auth::AccessToken;
use crate::config::EmrConfig;
use crate::error::FhirError;
use crate::transport::Transport;
use crate::write::{
    build_reversal, build_transaction, parse_transaction_response, post_bundle, TransactionOutcome,
    WriteEntry, WriteMethod, WritebackContext,
};

use psychdoc_model::state::DocState;
use psychdoc_store::{
    ClaimTarget, CommitOutcome, RefusalReason, ReserveOutcome, Store, StoreError, WritebackEntry,
};

/// 本模块的错误码：存储层与协议层的码原样透传，**不另造一套**——
/// 同一个错误在界面与日志里必须显示同一个码。
#[derive(Debug, thiserror::Error)]
pub enum ConfirmError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Fhir(#[from] FhirError),

    /// 勾选了草稿里不存在的件（草稿被并发改动过）。
    #[error("E_DRAFT_ENTRY_UNKNOWN: 草稿 {draft_id} 里没有件 {doc_id}")]
    DraftEntryUnknown { draft_id: String, doc_id: String },

    /// 撤回时草稿处于 `draft` 态（还没发出去），没有可撤的已生效事实。
    #[error("E_WITHDRAW_NOT_SENT: 草稿 {0} 从未发送过，撤回只能清空勾选")]
    WithdrawNotSent(String),

    /// 同一件正被另一处提交（护士长在两台机器同时点了提交）。
    #[error(
        "E_WRITEBACK_CONFLICT: {key} 已由「{first_actor}」于 {first_at} 占键提交中，本次未外发"
    )]
    WritebackConflict {
        key: String,
        first_actor: String,
        first_at: String,
    },

    /// 上次提交的结果未知（进程崩在外发与落库之间），**不自动重发**。
    #[error(
        "E_REPLAY_IN_DOUBT: {key} 上次由「{first_actor}」于 {first_at} 提交，进程在结果落库前中断，\
         本次**不重发**；请先人工核对院内端点是否已建件，再在「归档核齐」里标记为已写回或未写回"
    )]
    ReplayInDoubt {
        key: String,
        first_actor: String,
        first_at: String,
    },
}

impl ConfirmError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Store(e) => e.code(),
            Self::Fhir(e) => e.code(),
            Self::DraftEntryUnknown { .. } => "E_DRAFT_ENTRY_UNKNOWN",
            Self::WithdrawNotSent(_) => "E_WITHDRAW_NOT_SENT",
            Self::WritebackConflict { .. } => "E_WRITEBACK_CONFLICT",
            Self::ReplayInDoubt { .. } => "E_REPLAY_IN_DOUBT",
        }
    }

    /// 值不值得重试：端点不可达与 5xx 值得，配置与勾选问题不值得；
    /// **并发冲突值得**（对方提交完就放开了），**结果存疑不值得**
    /// ——它要的是人工核对，不是再点一次。
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Fhir(e) => e.is_retryable(),
            Self::WritebackConflict { .. } => true,
            _ => false,
        }
    }
}

pub type ConfirmResult<T> = std::result::Result<T, ConfirmError>;

/// 一次勾选确认的上下文。
#[derive(Debug, Clone)]
pub struct ConfirmContext {
    pub encounter_alias: String,
    /// 推进人（脱敏操作人标识）
    pub actor: String,
    /// RFC3339 时刻，由调用方注入以保证可复现
    pub at: String,
    pub ruleset_sha256: String,
    pub run_id: Option<String>,
}

impl ConfirmContext {
    fn writeback_ctx(&self, keys: Vec<String>) -> WritebackContext {
        WritebackContext {
            encounter_alias: self.encounter_alias.clone(),
            actor: self.actor.clone(),
            recorded_at: self.at.clone(),
            ruleset_sha256: self.ruleset_sha256.clone(),
            idempotency_keys: keys,
        }
    }
}

/// 一次确认的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfirmOutcome {
    /// 端点建成的件
    pub created: Vec<String>,
    /// Provenance 落库位置
    pub provenance: Option<String>,
    /// 落库结果：`Committed`（新增 N 行）或 `AlreadyExists`（幂等命中，0 行）
    pub commit: CommitOutcome,
    /// 状态推进后的件级状态（件 id → 新状态）
    pub advanced: Vec<(String, DocState)>,
    /// 本次是**重放**：键已被占且已提交，0 次外发，返回的是首次结果。
    ///
    /// 与 `is_replay()` 的区别：`is_replay()` 说的是「本地没新增写回行」，
    /// 而这里说的是「这次连请求都没发」。前者可能是并发挡住，后者是
    /// 真的重放——界面上要给两种不同的提示。
    pub replayed: bool,
    /// 本次的外发次数（重放恒为 0）。验收与运行摘要都用它。
    pub outbound: usize,
    /// 重放时首次提交的操作人与时刻（`(操作人, 时刻)`），供界面显示
    /// 「此件已由 王护士长 于 08:10 提交」而不是一句「已存在」。
    pub replayed_from: Option<(String, String)>,
}

impl ConfirmOutcome {
    /// 本次真正产生的新写回数。幂等命中恒为 0。
    pub fn new_rows(&self) -> usize {
        self.commit.new_rows()
    }

    /// 幂等命中：此前已写回过，本次 0 条新写回。
    pub fn is_replay(&self) -> bool {
        self.replayed || matches!(self.commit, CommitOutcome::AlreadyExists { .. })
    }
}

/// 建写回草稿。草稿里的件**尚未生效**，只是待人工勾选。
pub fn create_draft(
    store: &Store,
    draft_id: &str,
    ctx: &ConfirmContext,
    entries: &[WritebackEntry<'_>],
) -> ConfirmResult<()> {
    store.create_draft(draft_id, &ctx.encounter_alias, entries, &ctx.actor, &ctx.at)?;
    Ok(())
}

/// 逐条勾选 / 取消勾选。
pub fn set_selected(
    store: &Store,
    draft_id: &str,
    doc_id: &str,
    selected: bool,
    at: &str,
) -> ConfirmResult<()> {
    store.set_selected(draft_id, doc_id, selected, at)?;
    Ok(())
}

/// 把候选块落成一份草稿（逐段勾选 → 一件一条）。
///
/// 「一件一条」而不是「一段一条」：Bundle transaction 的 entry 粒度是
/// 资源，段是资源内部的内容。同一件的多个候选块在勾选时合并成一条，
/// 免得写出两份同 id 的 `Composition` 让服务端按最后一条赢。
pub fn draft_from_preview(
    store: &Store,
    draft_id: &str,
    ctx: &ConfirmContext,
    blocks: &[psychdoc_model::CandidateBlock],
    content_sha256_of: &dyn Fn(&str) -> String,
) -> ConfirmResult<usize> {
    let mut doc_ids: Vec<&str> = Vec::new();
    for b in blocks {
        if !doc_ids.contains(&b.doc_id.as_str()) {
            doc_ids.push(b.doc_id.as_str());
        }
    }
    // 哈希先算出来落到本地 Vec：WritebackEntry 借的是 &str，
    // 不能借用临时 String。
    let hashes: Vec<String> = doc_ids.iter().map(|id| content_sha256_of(id)).collect();
    let entries: Vec<WritebackEntry<'_>> = doc_ids
        .iter()
        .zip(hashes.iter())
        .map(|(id, sha)| WritebackEntry {
            doc_id: id,
            content_sha256: sha.as_str(),
            run_id: ctx.run_id.as_deref(),
        })
        .collect();
    let n = entries.len();
    create_draft(store, draft_id, ctx, &entries)?;
    Ok(n)
}

/// 确认发送整批写回。
///
/// 顺序是**先占键、再外发、后落库**：
///
/// ```text
/// 读勾选（空 → 零次外发直接拒）
///   └ 占幂等键（BEGIN IMMEDIATE，整批原子）
///        ├ 抢到          → 外发 transaction → 结算 committed → 落本地 + 推进状态
///        ├ 已 committed  → 重放：回放首次结果，0 次外发
///        ├ 被别人占着     → E_WRITEBACK_CONFLICT，0 次外发
///        └ 上次 in_doubt → E_REPLAY_IN_DOUBT，0 次外发，要人工核对
/// ```
///
/// 「先占键」这一步是 Task 28 的核心：不占键就外发，两个进程会各发一份
/// transaction，服务端建两份同 id 的件，本地那份唯一约束拦不住已经发生的
/// 外发。占键在前，冲突就发生在**零次网络调用**之前。
///
/// 外发失败时把键放回 `released`（不是留在 `in_flight`），否则一次网络抖动
/// 会把这件永久锁死成「别人正在提交」。
pub fn confirm(
    store: &Store,
    config: &EmrConfig,
    token: &AccessToken,
    draft_id: &str,
    ctx: &ConfirmContext,
    build_entries: &dyn Fn(&str) -> Option<WriteEntry>,
    transport: &dyn Transport,
) -> ConfirmResult<ConfirmOutcome> {
    // 1) 勾选是唯一入口：selection 一律从库里读，不接受调用方自报
    let selected = store.selected_items(draft_id)?;
    if selected.is_empty() {
        // 零次外发：这一步在任何网络调用之前
        return Err(StoreError::NothingSelected(draft_id.to_string()).into());
    }
    // 2) 组装 transaction 条目。幂等键由 reserve_writeback 算出（同一口径），
    //    Provenance 要带的那份在下面按 keys 重新拼。
    let mut entries: Vec<WriteEntry> = Vec::with_capacity(selected.len());
    for (doc_id, _) in &selected {
        let Some(mut e) = build_entries(doc_id) else {
            return Err(ConfirmError::DraftEntryUnknown {
                draft_id: draft_id.to_string(),
                doc_id: doc_id.clone(),
            });
        };
        // 权威实体键以库里登记的件 id 为准，避免调用方拼错
        e.logical_id = doc_id.clone();
        entries.push(e);
    }

    // 3) 占键：并发防护发生在**任何网络调用之前**
    let targets: Vec<ClaimTarget<'_>> = selected
        .iter()
        .map(|(doc_id, sha)| ClaimTarget {
            doc_id,
            content_sha256: sha,
        })
        .collect();
    let batch_id = format!("BATCH-{draft_id}");
    let reservation = store.reserve_writeback(
        &ctx.encounter_alias,
        &targets,
        &batch_id,
        &ctx.actor,
        &ctx.at,
    )?;
    let keys = match reservation {
        ReserveOutcome::Reserved { keys, .. } => keys,
        ReserveOutcome::Replay {
            keys,
            tx_result_json,
            first_actor,
            first_at,
            ..
        } => {
            // 重放：0 次外发，把首次结果原样回放给界面。
            // 本地不写「已写回」——那一笔早已落库，本次是同一件事。
            let prior = tx_result_json
                .as_deref()
                .and_then(|raw| serde_json::from_str::<StoredTxn>(raw).ok());
            return Ok(ConfirmOutcome {
                created: prior.as_ref().map(|p| p.created.clone()).unwrap_or_default(),
                provenance: prior.and_then(|p| p.provenance),
                commit: CommitOutcome::AlreadyExists { keys },
                advanced: Vec::new(),
                replayed: true,
                outbound: 0,
                replayed_from: Some((first_actor, first_at)),
            });
        }
        ReserveOutcome::Refused { blocker, reason, .. } => {
            // 一条都占不住 → 零次外发
            return Err(match reason {
                RefusalReason::InDoubt => ConfirmError::ReplayInDoubt {
                    key: blocker.short_key().to_string(),
                    first_actor: blocker.claimed_by,
                    first_at: blocker.claimed_at,
                },
                RefusalReason::Conflict => ConfirmError::WritebackConflict {
                    key: blocker.short_key().to_string(),
                    first_actor: blocker.claimed_by,
                    first_at: blocker.claimed_at,
                },
            });
        }
    };

    let bundle_ctx = ctx.writeback_ctx(keys);
    let bundle = build_transaction(&entries, &bundle_ctx);
    // 4) 外发。这一段失败一律把键放回 released，让护士长能原样重试。
    let resp = match post_bundle(config, token, &bundle, transport) {
        Ok(resp) => resp,
        Err(e) => {
            release_claim(store, &batch_id, &e.to_string(), &ctx.actor, &ctx.at);
            return Err(e.into());
        }
    };
    let txn: TransactionOutcome = match parse_transaction_response(&resp, entries.len()) {
        Ok(t) => t,
        Err(e) => {
            release_claim(store, &batch_id, &e.to_string(), &ctx.actor, &ctx.at);
            return Err(e.into());
        }
    };
    // 5) 端点整批成功才落本地：写回日志 + 审计（commit_writeback 内部一个事务）
    let store_entries: Vec<WritebackEntry<'_>> = selected
        .iter()
        .map(|(doc_id, sha)| WritebackEntry {
            doc_id,
            content_sha256: sha,
            run_id: ctx.run_id.as_deref(),
        })
        .collect();
    let commit = store.commit_writeback(
        draft_id,
        &store_entries,
        &ctx.actor,
        &ctx.at,
        &ctx.ruleset_sha256,
    )?;
    // 结算预占：committed + 存首次结果（重放时原样回放，不用再问服务端）
    let stored = StoredTxn {
        status: txn.status,
        created: txn.created.clone(),
        provenance: txn.provenance.clone(),
    };
    let _ = store.settle_committed(
        &batch_id,
        serde_json::to_string(&stored).ok().as_deref(),
        &ctx.actor,
        &ctx.at,
    )?;
    // 6) 状态推进：草稿 → 已建件。非法跳变会被状态机拒（0 条已生效语义见文档）
    let mut advanced = Vec::with_capacity(selected.len());
    for (doc_id, _) in &selected {
        match store.state_of(doc_id)? {
            Some(DocState::Draft) => {
                store.advance_state(doc_id, DocState::Created, &ctx.actor, &ctx.at)?;
                advanced.push((doc_id.clone(), DocState::Created));
            }
            // 已经推进过（幂等重放 / 已收）就不重复推进，也不报错
            Some(_) => {}
            None => {
                return Err(StoreError::StateUnknown(doc_id.clone()).into());
            }
        }
    }
    Ok(ConfirmOutcome {
        created: txn.created,
        provenance: txn.provenance,
        commit,
        advanced,
        replayed: false,
        outbound: 1,
        replayed_from: None,
    })
}

/// 落进预占表的首次 transaction 结果。序列化后存 `tx_result_json`，
/// 重放时原样回放——**不重新问服务端**（问一次就等于把「重放不外发」
/// 这条保证又破了一次）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct StoredTxn {
    status: u16,
    created: Vec<String>,
    provenance: Option<String>,
}

/// 外发失败时把键放回 `released`。失败信息只落在预占表上，**不写
/// `kind=writeback` 审计事件**——失败没有产生已生效事实，写进去会让
/// 「写回了几次」的审计计数虚高。
fn release_claim(store: &Store, batch_id: &str, reason: &str, actor: &str, at: &str) {
    let _ = store.release_batch(batch_id, reason, actor, at);
}

/// 撤回。
///
/// - 草稿仍是 `draft`（勾了但没发）→ 清空勾选回草稿区，**0 次外发**；
/// - 草稿已是 `sent` → 走反向 `Bundle` transaction（`DELETE`）+ 审计事件，
///   件级状态退回 `草稿`。
pub fn withdraw(
    store: &Store,
    config: &EmrConfig,
    token: &AccessToken,
    draft_id: &str,
    ctx: &ConfirmContext,
    build_entries: &dyn Fn(&str) -> Option<WriteEntry>,
    transport: &dyn Transport,
) -> ConfirmResult<usize> {
    match store.draft_state(draft_id)? {
        // 从未发送：只能清勾选，不产生任何已生效事实，也不需要网络
        Some(state) if state == "draft" => {
            let cleared = store.clear_selection(draft_id, &ctx.at)?;
            return Ok(cleared);
        }
        None => return Err(ConfirmError::Store(StoreError::DraftUnknown(draft_id.to_string()))),
        Some(_) => {}
    }
    // 已发送：只撤「真的写出过」的件（按写回日志，不按草稿勾选）
    let written = store.written_doc_ids(draft_id)?;
    if !written.is_empty() {
        let keys: Vec<String> = written
            .iter()
            .map(|doc_id| {
                psychdoc_store::idempotency_key(&ctx.encounter_alias, doc_id, "")
            })
            .collect();
        let mut entries: Vec<WriteEntry> = Vec::with_capacity(written.len());
        for doc_id in &written {
            let Some(mut e) = build_entries(doc_id) else {
                return Err(ConfirmError::DraftEntryUnknown {
                    draft_id: draft_id.to_string(),
                    doc_id: doc_id.clone(),
                });
            };
            e.logical_id = doc_id.clone();
            e.method = WriteMethod::Post; // DELETE 的 url 只看 logical_id，method 无关
            entries.push(e);
        }
        let bundle_ctx = ctx.writeback_ctx(keys);
        let reversal = build_reversal(&entries, &bundle_ctx);
        post_bundle(config, token, &reversal, transport)?;
    }
    // 落库：草稿退回 draft 区 + 一条审计事件（撤回也留痕）
    store.withdraw_draft(draft_id, &ctx.actor, &ctx.at, &ctx.ruleset_sha256)?;
    // 撤销预占：服务端那份件已经被 DELETE 掉了，键必须跟着放开，否则同一件
    // 补写后再提交会命中「重放」而被当成已完成——界面上点提交什么都不发。
    let _ = store.revoke_batch(
        &format!("BATCH-{draft_id}"),
        "已撤回：服务端资源已删除，幂等键放开以备重写",
        &ctx.actor,
        &ctx.at,
    );
    // 状态回退：已建件 → 草稿（合法回退边）
    for doc_id in &written {
        if let Some(DocState::Created) = store.state_of(doc_id)? {
            store.advance_state(doc_id, DocState::Draft, &ctx.actor, &ctx.at)?;
        }
    }
    Ok(written.len())
}