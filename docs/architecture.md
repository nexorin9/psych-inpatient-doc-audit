# 架构与不变量

本文是 README「架构与数据流」小节的展开：件级状态机、幂等键、依赖图正反向、
审计链四条主链路的细节。README 只留一张 ASCII 流程图与不变量清单，需要逐条核对口径时看这里。

---

## 1. crate 划分与调用边界

| crate | 职责 | 不做什么 |
|------|------|----------|
| `psychdoc-model` | `Span` / `Section` / `Field` / `DocItem` / `Timestamp` / `Diagnostic` / `DocState` | 不触 IO，不引 serde 之外的运行时依赖 |
| `psychdoc-parse` | 文书包发现、类型过滤、实体绑定、三层解析、时间戳抽取、增量缓存 | 不判合规，不认识规则集 |
| `psychdoc-rules` | 规则集加载与三级级联、豁免谓词、表达式求值器、三类规则执行器、跨文书时间序 join | 不知道 sqlite，不发网络请求 |
| `psychdoc-graph` | Collector → Resolver、Tarjan SCC、正反向 `ImportMap`、阻塞闭包与欠件天数 | 不解析文书正文 |
| `psychdoc-store` | sqlite 迁移、幂等键、草稿区、审计 `Provenance`、规则集版本回溯 | 不解析 FHIR 报文 |
| `psychdoc-fhir` | R4 读（`_since` + 分页 + 退避）、`Bundle` transaction 写、`Provenance` 生成、企业微信 Webhook | 不判合规，不写 sqlite |
| `psychdoc-ffi` | PyO3 绑定，5 个同步入口（api 业务层 + bindings 搬运 + jsonval 转换） | **不含**任何内部线程或 tokio runtime |
| `psychdoc`（bin） | `check` / `graph` / `writeback-dryrun` / `rule-regress` / `export-audit` / `codes` / `preflight` 七个子命令 | — |

依赖方向单向：`parse → rules → graph`、`fhir → model`、`store → model`，
`ffi` 与 bin 在最外层。核心侧**全同步**；重活交 QtConcurrent + `QFutureWatcher`
回主线程刷新，绑定层不得在工作线程外直接碰控件。

---

## 2. 件级状态机

```
        ┌──────────┐
        │  缺失     │◀── 发现缺件 / 成套件未收齐
        └────┬─────┘
             │ 建件（人工勾选后提交）
        ┌────▼─────┐
        │  草稿     │◀── 补写预览已生成，未提交
        └────┬─────┘
             │ 确认写回成功（Bundle transaction 返回 200）
        ┌────▼─────┐
        │  已建件   │
        └────┬─────┘
             │ 病区收件确认
        ┌────▼─────┐
        │  已收     │
        └────┬─────┘
             │ 依赖闭包全部满足
        ┌────▼─────┐
        │  可归档   │
        └──────────┘
```

- 合法路径只有上述五条相邻边。
- **非法跳变**（如 `缺失 → 可归档`）由 `transition()` 返回 `E_STATE_JUMP`，
  且**不改变既有状态**、不产生任何已生效事实。
- 未登记的件请求推进返回 `E_STATE_UNKNOWN`。
- `doc_states` 行以外键挂到已登记的 `doc_items` 上，孤儿状态行被硬约束拒绝——
  否则审计导出说不清这一行说的是哪一件文书。

## 3. 幂等键

```
idempotency_key = sha256(住院别名 ‖ 件 id ‖ 内容哈希)
```

- 三个分量之间用 `|` 分隔且长度前缀固定，防止
  `WARD-A-001X|""|hash-1` 与 `WARD-A-001|DOC-1|hash-1` 撞键。
- `writeback_log` 建唯一约束。重复提交返回 `CommitOutcome::AlreadyExists`，
  并回填首次提交的**操作人与时刻**，**0 条新写回、0 条新审计事实**。
- 内容哈希一变即产生新键，允许在内容确实改过之后再写一次——
  这与「同日重复确认被挡下」是两条不同语义，不互相吞掉。
- 撤回走反向 transaction，件退回 `草稿`；草稿撤回不产生已生效事实，
  `writeback_log` 行不动。

## 4. 依赖图正反向

```
Collector（按住院别名收集节点与依赖规范）
    │  去重 · 检测自环 → self_loops
    ▼
Resolver（实体解析；confident 才连边）
    │  ambiguous → 人工队列，**不产生任何边**
    ▼
Tarjan SCC（环检测）
    │  有环 → E_DEP_CYCLE，整图拒绝出图，环内节点全标 ambiguous
    │          环外节点照常出图（局部错误不连坐全盘）
    ▼
ImportMap { dependencies, dependents }  ← --direction 开关
    │
    ▼
依赖闭包（BFS/DFS）+ 欠件天数
```

- `dependencies(x)`（正向：这件在等谁）与 `dependents(x)`（反向：这件卡住谁）
  对同一节点必须给出**不同**答案，且两者并集 = 全图可达集减去自身。
  这条不变量由测试强制，菱形依赖不得重复计数。
- 阻塞传播**只走 `requires: true` 的依赖边**；`requires: false` 的参考缺件
  只列在「参考缺件」里，不阻断归档也不参与传播。
- **欠件天数按自然日（日历日差）计算**，不按 24 小时整除；按住院别名聚合取
  该别名下最老的一条缺失件。从未登记发现时刻的缺件显示「天数算不出」，
  **不按今天兜底**（否则会显示成刚缺 0 天，把真缺件读成刚发生）。
- 自环归 `self_loops`，既不成边也不重复报成环——同一问题记两笔会让护士长
  收到两条重复整改要求。

## 5. 审计链

```
一次 check --record
      │
      ├── review_runs   批次号 · 规则集版本+sha256 · 操作人 · 时刻 · 已审件数
      ├── diagnostics   每条一行：码 / 段落 / 区间 / 严重度 / 规范依据 / 豁免原因
      └── audit_events  kind=review，携带当次命中码集合

一次确认写回
      │
      ├── writeback_log   幂等键 + Bundle transaction 摘要
      └── audit_events    kind=writeback，同样带规则集 sha256
```

- 调阅与写回各留一条 `Provenance`，落在**同一条**审计链里，顺序可查。
- **版本回溯**：给定住院别名 + 时间点，还原当时生效的规则集哈希与命中码集合。
  早于任何一次审查的时刻返回 `None`（**不返回最新一版**）——返回最新一版
  会让质控办把今天的口径当成当年的口径。
- `export-audit` 导出时做**二次脱敏扫描**（标识列与自由文本列都扫），
  csv 带 UTF-8 BOM，Excel 直接打开不乱码。

## 6. 解析的部分成功语义

- 逐件解析、单件失败不拖垮整批：失败件进 `unresolved` 清单并附原因码
  （如 `BadDate`、类型不在字典）。
- 运行摘要带不变量 `总数 == 重跑 + 跳过 + 未解析 + 类型不在字典`
  （`RunSummary::is_balanced()`），把「静默跳过」变成**可查询条件**而不是
  一句承诺。
- 区间一律按**字符**计（不是字节），生僻字与全角字符的 `char_range`
  切回原文仍能命中同一个值；GBK 混入字节不 panic、不越界。
- 三态不合并：字段**缺失** / **空串** / **null** 产生三个不同的
  `EvalOutcome`，禁止都当成「等于空」。

## 7. 增量重跑

- 双层缓存：件级 `(doc_id, 内容哈希)` + 住院级 `(住院别名, 组指纹)`。
- 跨件时间序必须按住院组整体失效，故在件级缓存之外另设组指纹。
- 缓存键存**内容哈希**而非只存「最新值」，使「改回去」也能命中；
  `HISTORY_DEPTH = 2` 钉住体积。
- 命中率分母取「进入规则层的件数」而非总件数，否则非文书件会凭空拉低命中率。
- 增量结果与全量结果**逐条相等**（比对码 + 件 + 段 + 区间 + 文案指纹），
  不只比计数。