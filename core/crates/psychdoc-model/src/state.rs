//! 件级业务状态机：`缺失 → 草稿 → 已建件 → 已收 → 可归档`。
//!
//! 对应 `spec.md` §4「业务状态生命周期」：不可跳步，每步有推进人；
//! 非法跳变（如 `缺失 → 可归档`）**必须**被拒并记 `E_STATE_JUMP`，
//! 绝不静默接受。撤回是显式回退边，不是「新开一条路径」。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, PartialOrd, Ord)]
pub enum DocState {
    Missing,
    Draft,
    Created,
    Received,
    Archivable,
}

impl DocState {
    pub fn as_zh(&self) -> &'static str {
        match self {
            Self::Missing => "缺失",
            Self::Draft => "草稿",
            Self::Created => "已建件",
            Self::Received => "已收",
            Self::Archivable => "可归档",
        }
    }
}

impl fmt::Display for DocState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_zh())
    }
}

/// 合法迁移表。键为当前态，值为允许的下一态。
fn allowed_next(from: DocState) -> &'static [DocState] {
    match from {
        // 缺失 → 可直接进草稿（先起草），或由系统建件直接落「已建件」
        DocState::Missing => &[DocState::Draft, DocState::Created],
        DocState::Draft => &[DocState::Created, DocState::Missing],
        // 撤回 = 回草稿
        DocState::Created => &[DocState::Received, DocState::Draft],
        DocState::Received => &[DocState::Archivable, DocState::Draft],
        // 终态：只能整包回退，不允许单件回退
        DocState::Archivable => &[],
    }
}

pub fn is_legal(from: DocState, to: DocState) -> bool {
    allowed_next(from).contains(&to)
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StateError {
    #[error("E_STATE_JUMP: 件 {doc_id} 不能从「{from}」跳到「{to}」")]
    Jump {
        doc_id: String,
        from: &'static str,
        to: &'static str,
    },
    #[error("E_STATE_UNKNOWN: 未知的件状态 {0}")]
    Unknown(String),
}

impl StateError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Jump { .. } => "E_STATE_JUMP",
            Self::Unknown(_) => "E_STATE_UNKNOWN",
        }
    }
}

/// 单件状态跟踪：当前态 + 谁在什么时候推进的。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StateTracker {
    pub doc_id: String,
    pub state: DocState,
    /// 最近一次推进人（脱敏操作人标识）
    pub last_actor: String,
    /// 迁移历史，供审计链回放
    pub history: Vec<Transition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transition {
    pub from: DocState,
    pub to: DocState,
    pub actor: String,
    /// 由调用方注入（CLI 传 --at，测试传固定值），保证可复现
    pub at: String,
}

impl StateTracker {
    pub fn new(doc_id: impl Into<String>, initial: DocState) -> Self {
        Self {
            doc_id: doc_id.into(),
            state: initial,
            last_actor: String::new(),
            history: Vec::new(),
        }
    }

    /// 推进状态。非法跳变返回 `E_STATE_JUMP` 且**不改变任何既有状态**。
    pub fn transition(
        &mut self,
        to: DocState,
        actor: impl Into<String>,
        at: impl Into<String>,
    ) -> Result<(), StateError> {
        if !is_legal(self.state, to) {
            return Err(StateError::Jump {
                doc_id: self.doc_id.clone(),
                from: self.state.as_zh(),
                to: to.as_zh(),
            });
        }
        let from = self.state;
        let actor = actor.into();
        self.state = to;
        self.last_actor = actor.clone();
        self.history.push(Transition {
            from,
            to,
            actor,
            at: at.into(),
        });
        Ok(())
    }
}

/// 一批件的跟踪器；批量推进时**逐件校验**，任一非法跳变即整批拒绝，
/// 保证「0 条部分写入」。
#[derive(Debug, Clone, Default)]
pub struct StateBoard {
    trackers: BTreeMap<String, StateTracker>,
}

impl StateBoard {
    pub fn insert(&mut self, tracker: StateTracker) {
        self.trackers.insert(tracker.doc_id.clone(), tracker);
    }

    pub fn get(&self, doc_id: &str) -> Option<&StateTracker> {
        self.trackers.get(doc_id)
    }

    pub fn state_of(&self, doc_id: &str) -> Option<DocState> {
        self.trackers.get(doc_id).map(|t| t.state)
    }

    /// 先校验全部，再逐件提交；任一非法则整体不动。
    pub fn transition_batch(
        &mut self,
        items: &[(String, DocState)],
        actor: &str,
        at: &str,
    ) -> Result<(), StateError> {
        for (doc_id, to) in items {
            let cur = self
                .trackers
                .get(doc_id)
                .ok_or_else(|| StateError::Unknown(doc_id.clone()))?;
            if !is_legal(cur.state, *to) {
                return Err(StateError::Jump {
                    doc_id: doc_id.clone(),
                    from: cur.state.as_zh(),
                    to: to.as_zh(),
                });
            }
        }
        for (doc_id, to) in items {
            if let Some(t) = self.trackers.get_mut(doc_id) {
                let _ = t.transition(*to, actor, at);
            }
        }
        Ok(())
    }

    pub fn len(&self) -> usize {
        self.trackers.len()
    }

    pub fn is_empty(&self) -> bool {
        self.trackers.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tracker(state: DocState) -> StateTracker {
        StateTracker::new("WARD-A-001-约束记录", state)
    }

    #[test]
    fn legal_path_missing_to_archivable_is_rejected() {
        let mut t = tracker(DocState::Missing);
        let err = t.transition(DocState::Archivable, "护士长A", "2026-03-02T08:00:00+08:00").unwrap_err();
        assert_eq!(err.code(), "E_STATE_JUMP");
        assert!(err.to_string().contains("不能从"));
        // 状态未被改变
        assert_eq!(t.state, DocState::Missing);
        assert!(t.history.is_empty());
    }

    #[test]
    fn full_legal_path_walks_forward() {
        let mut t = tracker(DocState::Missing);
        for to in [DocState::Draft, DocState::Created, DocState::Received, DocState::Archivable] {
            t.transition(to, "护士长A", "2026-03-02T08:00:00+08:00").unwrap();
        }
        assert_eq!(t.state, DocState::Archivable);
        assert_eq!(t.history.len(), 4);
        assert_eq!(t.history[0].from, DocState::Missing);
    }

    #[test]
    fn archivable_is_terminal() {
        let mut t = tracker(DocState::Archivable);
        assert!(t.transition(DocState::Draft, "X", "t").is_err());
        assert!(t.transition(DocState::Archivable, "X", "t").is_err());
    }

    #[test]
    fn withdraw_returns_to_draft() {
        let mut t = tracker(DocState::Created);
        t.transition(DocState::Draft, "护士长A", "t").unwrap();
        assert_eq!(t.state, DocState::Draft);
    }

    #[test]
    fn batch_rejects_all_when_one_is_illegal() {
        let mut b = StateBoard::default();
        b.insert(tracker(DocState::Missing));
        let mut t2 = tracker(DocState::Created);
        t2.doc_id = "WARD-A-001-评估报告".into();
        b.insert(t2);

        let items = vec![
            ("WARD-A-001-约束记录".to_string(), DocState::Draft),
            ("WARD-A-001-评估报告".to_string(), DocState::Archivable), // 非法
        ];
        let err = b.transition_batch(&items, "护士长A", "t").unwrap_err();
        assert_eq!(err.code(), "E_STATE_JUMP");
        // 0 条部分写入：第一条也没动
        assert_eq!(b.state_of("WARD-A-001-约束记录"), Some(DocState::Missing));
    }

    #[test]
    fn unknown_doc_id_is_rejected() {
        let mut b = StateBoard::default();
        let err = b
            .transition_batch(&[("不存在".to_string(), DocState::Draft)], "X", "t")
            .unwrap_err();
        assert_eq!(err.code(), "E_STATE_UNKNOWN");
    }
}
