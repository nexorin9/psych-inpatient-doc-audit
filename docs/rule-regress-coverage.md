# 规则码覆盖矩阵

> 本文件由 `rulesets/rule-regress.yaml`（回归用例集）与 `rulesets/hospital.yaml`（交付规则集）
> 对照生成。改规则集后跑 `psychdoc rule-regress`，覆盖缺位会直接非 0 退出——
> 删掉一条反例，剩下所有用例照样全绿，覆盖矩阵是唯一能发现这件事的东西。

| 规则码 | 名称 | 正例 | 反例（误报守卫） | 边界例 | 覆盖 |
|---|---|---|---|---|---|
| `QC-PSY-01` | 件内必填字段缺失 | 1 | 1 | 1 | 齐备 |
| `QC-PSY-02` | 字段值形状不符 | 1 | 1 | 1 | 齐备 |
| `QC-PSY-03` | 取值域越界 | 1 | 1 | 1 | 齐备 |
| `QC-PSY-04` | 单位 / 量纲错配 | 1 | 1 | 1 | 齐备 |
| `QC-PSY-05` | 事件与记录时序倒挂（解除早于开始） | 1 | 1 | 1 | 齐备 |
| `QC-PSY-06` | 评估与医嘱时序矛盾（评估晚于首次约束） | 1 | 1 | 1 | 齐备 |
| `QC-PSY-07` | 约束与签署时序错误（签署晚于约束开始） | 1 | 1 | 1 | 齐备 |
| `QC-PSY-08` | 签署齐备性（已签但无签署时刻） | 1 | 1 | 1 | 齐备 |

合计 8 条规则码 · 24 条用例 · 覆盖率 100%


## 逐码用例明细

| 用例 id | 类别 | 现场 | 夹具 | 期望 |
|---|---|---|---|---|
| `psy-01-positive-signature-blank` | 正例 | 约束知情同意的「患方签字」键在但值为空 —— 护士长把这一栏补上即可解除阻断 | `fixtures/ward-01-minimal.json` | 命中 QC-PSY-01；严重度 blocking；1 条 |
| `psy-01-negative-compliant-package` | 反例 | 同场景全合规包必须零命中 | `fixtures/ward-09-field-compliant.json` | 0 条 |
| `psy-01-boundary-mixed-entries` | 边界例 | 包里混了字典外类型、附件、Provenance、无住院别名的件 —— 只有真正的约束知情同意报，其余走解析期问题，不许混进规则命中 | `fixtures/ward-05-mixed-entries.json` | 命中 QC-PSY-01；1 条 |
| `psy-02-positive-fullwidth-date` | 正例 | 评估时间写成全角数字 —— 全角归一是唯一给补写载荷的情形，护士长点一下预览即可 | `fixtures/ward-08-field-violations.json` | 命中 QC-PSY-02；严重度 blocking；1 条 |
| `psy-02-negative-compliant-date` | 反例 | 合规评估件零命中 | `fixtures/ward-09-field-compliant.json` | 0 条 |
| `psy-02-boundary-value-with-sentence-punct` | 边界例 | 约束记录里的日期与时长都带句号 —— 句号是病区随手写的标点，不是格式错；只有等级越界与单位错配两条真命中 | `fixtures/ward-08-field-violations.json` | 命中 QC-PSY-03、QC-PSY-04；2 条 |
| `psy-03-positive-level-out-of-domain` | 正例 | 约束等级填「四级保护」—— 院内表单只有一/二/三级保护三级勾选项 | `fixtures/ward-08-field-violations.json` | 命中 QC-PSY-03、QC-PSY-04；严重度 blocking；2 条 |
| `psy-03-negative-compliant-record` | 反例 | 二级保护 + 120 分钟的合规记录零命中 | `fixtures/ward-09-field-compliant.json` | 0 条 |
| `psy-03-boundary-folded-by-more-specific` | 边界例 | 「1000 小时」同时命中越界与单位错配 —— 去重后只刷更具体的 04，03 折叠留痕而不是消失 | `fixtures/ward-15-overlapping-findings.json` | 命中 QC-PSY-04；1 条；折叠 QC-PSY-03 |
| `psy-04-positive-wrong-unit` | 正例 | 约束时长按表单标注以分钟计量，填「1000 小时」无法核算 | `fixtures/ward-15-overlapping-findings.json` | 命中 QC-PSY-04；严重度 blocking；1 条 |
| `psy-04-negative-correct-unit` | 反例 | 合规记录「120 分钟」零命中 | `fixtures/ward-09-field-compliant.json` | 0 条 |
| `psy-04-boundary-converted-value-in-range` | 边界例 | 「2 小时」换算后 120 分钟落在允许区间内 —— 只报单位错配，不连带报越界 | `fixtures/ward-08-field-violations.json` | 命中 QC-PSY-03、QC-PSY-04；2 条 |
| `psy-05-positive-release-before-start` | 正例 | 解除时间 07:00 早于约束起始 08:00 —— 违反的两件各报一条，护士长看得出是谁先谁后 | `fixtures/ward-11-cross-doc-violations.json` | 命中 QC-PSY-05；严重度 blocking；2 条 |
| `psy-05-negative-correct-order` | 反例 | 解除 13:10 晚于起始 08:00 的合规包零命中 | `fixtures/ward-12-cross-doc-compliant.json` | 0 条 |
| `psy-05-boundary-inside-tie-tolerance` | 边界例 | 解除早 3 分钟但落在院内声明的 5 分钟并列容差内 —— 判出来了，结论是不算违规，且必须留痕 | `fixtures/ward-13-tie-tolerance.json` | 0 条；容差内 QC-PSY-05（5 分钟档） |
| `psy-06-positive-assessment-after-restraint` | 正例 | 首次约束 03-04 08:00 才开始，评估却写在 03-05 10:00 —— 补记说明才放得过 | `fixtures/ward-11-cross-doc-violations.json` | 命中 QC-PSY-06；严重度 blocking；2 条 |
| `psy-06-negative-assessment-first` | 反例 | 评估早于首次约束的合规包零命中 | `fixtures/ward-12-cross-doc-compliant.json` | 0 条 |
| `psy-06-boundary-moment-unreadable` | 边界例 | 评估时间写成全角抽不出时刻 —— 判不出来，不拿当前时间兜底，记未判定交人工 | `fixtures/ward-08-field-violations.json` | 命中 QC-PSY-02、QC-PSY-03、QC-PSY-04；3 条；未判定 QC-PSY-06 |
| `psy-07-positive-signature-after-restraint` | 正例 | 患方签署时刻 07:40 晚于约束起始 08:00 —— 按先约束后知情处理 | `fixtures/ward-11-cross-doc-violations.json` | 命中 QC-PSY-07；严重度 blocking；2 条 |
| `psy-07-negative-consent-first` | 反例 | 知情同意先于约束开始的合规包零命中 | `fixtures/ward-12-cross-doc-compliant.json` | 0 条 |
| `psy-07-boundary-precision-too-coarse` | 边界例 | 签署时刻只写到日 —— 判了就是在猜，记「精度不足」交人工而不是静悄悄放过 | `fixtures/ward-14-coarse-precision.json` | 0 条；未判定 QC-PSY-07 |
| `psy-08-positive-signed-without-moment` | 正例 | 签署页两个签字都在，但该件没有 authoredOn —— 事后无法核对签署与约束的先后 | `fixtures/ward-11-cross-doc-violations.json` | 命中 QC-PSY-08；严重度 blocking；1 条 |
| `psy-08-negative-signed-with-moment` | 反例 | 签署页带 authoredOn 的合规件零命中 | `fixtures/ward-12-cross-doc-compliant.json` | 0 条 |
| `psy-08-boundary-half-signed-page` | 边界例 | 护士签字写了、医师签字压根没写，且全件无 authoredOn —— 两条不同的码各管各的，不许混成一条 | `fixtures/ward-04-missing-moments.json` | 命中 QC-PSY-01、QC-PSY-08；2 条 |

