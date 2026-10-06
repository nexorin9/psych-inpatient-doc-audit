# 输出样例（完整回显）

README 的「输出样例」小节只留每类产物的一小段摘录。这里是**完整**的真实运行回显，
全部用 `fixtures/` 下的脱敏合成文书包跑出来，无患者姓名、无住院号、无真实文书。

复现命令统一在项目根 `core/` 目录下执行：

```bash
cd core
./target/debug/psychdoc <子命令> ...
```

---

## 1. `psychdoc check` — 问题清单

```bash
./target/debug/psychdoc check ../fixtures/ward-11-cross-doc-violations.json \
  --ruleset ../rulesets/hospital.yaml
```

真实 stdout（退出码 1）：

```text
运行日志：psychdoc-2026-10-07.log（本次事件 12 条 · 脱敏替换 0 处）
文书包：../fixtures/ward-11-cross-doc-violations.json    规则集：../rulesets/hospital.yaml（版本 0.6.0 · sha256 2abfac00aa5b）
已审文书 5 件

问题清单（7 条）：
  [阻断] QC-PSY-05 · WARD-I-001-RECORD-01#s0@[7, 24) · 不可自动修
        约束记录的约束起始时间（2026-03-04 08:00）早于约束解除记录的解除时间（2026-03-04 07:00），解除时序倒挂（依据：《精神科住院护理常规》第 6.1 条：约束解除记录填写的解除时间不得早于约束记录填写的约束起始时间，二者倒挂的病历不得直接归档）
        规则来源：QC-PSY-05（院级）
  [阻断] QC-PSY-05 · WARD-I-001-RELEASE-01#s0@[5, 22) · 不可自动修
        约束解除记录的解除时间（2026-03-04 07:00）晚于约束记录的约束起始时间（2026-03-04 08:00），解除时序倒挂（依据：《精神科住院护理常规》第 6.1 条：约束解除记录填写的解除时间不得早于约束记录填写的约束起始时间，二者倒挂的病历不得直接归档）
        规则来源：QC-PSY-05（院级）
  [阻断] QC-PSY-06 · WARD-I-001-RECORD-01#s0@[7, 24) · 不可自动修
        约束记录的约束起始时间（2026-03-04 08:00）早于评估报告的评估时间（2026-03-05 10:00），评估时序倒挂（依据：《精神科住院护理常规》第 6.2 条：首次约束开始前应当已完成入院评估，评估时间晚于首次约束起始时间的须补记说明）
        规则来源：QC-PSY-06（院级）
  [阻断] QC-PSY-06 · WARD-I-001-ASSESS-01#s0@[5, 22) · 不可自动修
        评估报告的评估时间（2026-03-05 10:00）晚于约束记录的约束起始时间（2026-03-04 08:00），评估时序倒挂（依据：《精神科住院护理常规》第 6.2 条：首次约束开始前应当已完成入院评估，评估时间晚于首次约束起始时间的须补记说明）
        规则来源：QC-PSY-06（院级）
  [阻断] QC-PSY-07 · WARD-I-001-RECORD-01#s0@[7, 24) · 不可自动修
        约束记录写明的约束起始时间（2026-03-04 08:00）早于患方知情同意件的签署时刻（2026-03-04 09:10）：先约束、后知情，顺序不成立（依据：《精神科住院护理常规》第 6.3 条：患方知情同意须在约束开始前完成签署，签署时刻晚于约束起始时间的按先约束后知情处理）
        规则来源：QC-PSY-07（院级）
  [阻断] QC-PSY-07 · WARD-I-001-CONSENT-01#s0@[0, 0) · 不可自动修
        患方知情同意件的签署时刻（2026-03-04 09:10）晚于约束记录写明的约束起始时间（2026-03-04 08:00）：先约束、后知情，顺序不成立（依据：《精神科住院护理常规》第 6.3 条：患方知情同意须在约束开始前完成签署，签署时刻晚于约束起始时间的按先约束后知情处理）
        规则来源：QC-PSY-07（院级）
  [阻断] QC-PSY-08 · WARD-I-001-SIGN-01#s0@[29, 45) · 不可自动修
        签署页件的「护士签字」「医师签字」已填写，但该件没有签署时刻（authoredOn 缺失），事后无法核对签署与约束的先后（依据：《精神科住院护理常规》第 4.5 条：签署齐备性含签署人签字与可核对的签署时刻两项；只留签字未留签署时刻的，事后无法核对签署与约束的先后）
        规则来源：QC-PSY-08（院级）

事件时间线（WARD-I-001，13 条事件）：
  2026-03-04 07:00  文书日期  WARD-I-001-RELEASE-01
  2026-03-04 07:00  业务时刻  WARD-I-001-RELEASE-01
  2026-03-04 07:30  书写时刻  WARD-I-001-RELEASE-01
  2026-03-04 07:40  文书日期  WARD-I-001-CONSENT-01
  2026-03-04 08:00  文书日期  WARD-I-001-RECORD-01
  2026-03-04 08:00  业务时刻  WARD-I-001-RECORD-01
  2026-03-04 08:00  业务时刻  WARD-I-001-CONSENT-01
  2026-03-04 08:20  书写时刻  WARD-I-001-RECORD-01
  2026-03-04 09:10  书写时刻  WARD-I-001-CONSENT-01
  2026-03-04 09:30  文书日期  WARD-I-001-SIGN-01
  2026-03-05 10:00  文书日期  WARD-I-001-ASSESS-01
  2026-03-05 10:00  业务时刻  WARD-I-001-ASSESS-01
  2026-03-05 10:20  书写时刻  WARD-I-001-ASSESS-01

退出码 1：审查不通过：存在阻断项，或归档核齐未通过（缺件 / 连带阻塞 / 人工待办）
```

读法：

- 一条跨文书违规给**两件各一条**定位（谁晚、谁早都说清），这是时间序规则的固定形态；
- `#s0@[7, 24)` 是「第 0 段 · 字符 7 到 24」，该区间切回原文正好是被判的那个时刻字面量；
- 「不可自动修」是一等状态：这类条目只给依据，不给写入按钮；
- 合规反例包 `ward-12-cross-doc-compliant.json` 跑同一套规则输出「未发现规则命中项」，退出码 0。

### 缺时刻时走的是「判不了」，不是「通过」

```bash
./target/debug/psychdoc check ../fixtures/ward-04-missing-moments.json --ruleset ../rulesets/hospital.yaml
```

```text
未发现规则命中项

未判定的跨文书检查（3 条，需人工核）：
  [未判定] QC-PSY-05 · WARD-D-001 · 对照件不在本次文书包内：本次文书包内没有 restraint_record 件（缺件由归档核齐视图负责）
  [未判定] QC-PSY-06 · WARD-D-001 · 对照件不在本次文书包内：本次文书包内没有 restraint_record 件（缺件由归档核齐视图负责）
  [未判定] QC-PSY-07 · WARD-D-001 · 对照件不在本次文书包内：本次文书包内没有 restraint_consent 件（缺件由归档核齐视图负责）

事件时间线（WARD-D-001，0 条事件）：
  （无时刻）  WARD-D-001-NURSE-01  整件抽不出任何时刻，时间序相关检查已跳过
  （无时刻）  WARD-D-001-SIGN-01  整件抽不出任何时刻，时间序相关检查已跳过
```

清单为空但**并列**列出「未判定的跨文书检查」，每条写清是「对照件不在包里」还是「抽不出时刻」，
避免把「判不了」读成「没问题」。时间线里这两件标「（无时刻）」，**不按当前时间兜底**。

---

## 2. `psychdoc graph` — 归档核齐依赖图

```bash
./target/debug/psychdoc graph WARD-B-001 --input ../fixtures/ward-02-restraint-set.json \
  --ruleset ../rulesets/hospital.yaml --direction both
```

真实 stdout（退出码 0）：

```text
归档核齐视图：WARD-B-001（核算时刻 2026-10-07T01:21:36.708819+08:00）
结论：已核齐（零缺件 · 零连带阻塞 · 零人工待办）
方向：both

件清单（5 件）：
  [缺件] <WARD-B-001>::missing::inpatient_record · 住院记录
      在等（正向）：无
      卡住（反向）：无
  [在库] WARD-B-001-ASSESS-01 · 评估报告
      在等（正向）：无
      卡住（反向）：无
  [在库] WARD-B-001-CONSENT-01 · 约束知情同意
      在等（正向）：无
      卡住（反向）：WARD-B-001-RECORD-01（约束记录）
  [在库] WARD-B-001-RECORD-01 · 约束记录
      在等（正向）：WARD-B-001-CONSENT-01（约束知情同意）
      卡住（反向）：WARD-B-001-RELEASE-01（约束解除记录）
  [在库] WARD-B-001-RELEASE-01 · 约束解除记录
      在等（正向）：WARD-B-001-RECORD-01（约束记录）
      卡住（反向）：无

参考缺件（1 条，可见但不阻断）：
  WARD-B-001-ASSESS-01（评估报告） → <WARD-B-001>::missing::inpatient_record（住院记录）

口径说明：
  · 阻塞传播只走 requires: true 的依赖边；本次住院 WARD-B-001 缺 0 件，连带阻塞 0 件，最深 0 级
  · requires: false 的参考缺件只列在「参考缺件」里，不阻断归档，也不参与传播。
  · 欠件天数按自然日（日历日差）计算，不按 24 小时整除；从未登记发现时刻的缺件天数显示为「算不出」，不按今天兜底。

退出码 0：成功：审查零阻断 / 视图出得来 / 干跑构造成功
```

查一个根本没有的住院别名，**退 2 而不是返回空视图**：

```bash
./target/debug/psychdoc graph X --input ../fixtures/ward-02-restraint-set.json --ruleset ../rulesets/hospital.yaml
```

```text
psychdoc: E_ENCOUNTER_UNKNOWN：这份输入里没有住院别名 X 的任何一件文书——空视图会被读成「已核齐」
```

---

## 3. `psychdoc writeback-dryrun` — 建件请求摘要

```bash
./target/debug/psychdoc writeback-dryrun selection.json
```

真实 stdout（退出码 0，**0 次网络请求**）：

```text
写回干跑：WARD-A-001 · 1 条 · 时刻 2026-03-05T08:20:00+08:00
Bundle 类型：transaction（transaction = 要么整批生效，要么零条生效）
规则集 sha256：2a97516c354b
Bundle 条目 2 条（其中末条为 Provenance）：
  POST Composition · resource=Composition
  POST Provenance · resource=Provenance
幂等键（同一份内容今天重复提交会被这几条挡下）：
  WARD-A-001-CONSENT-01 · 84b50317895c76e14940b429705308da774db432379d3055093e7a474af33626

本次未发起任何网络请求（干跑：不取 token、不连端点、不落库）。

退出码 0：成功：审查零阻断 / 视图出得来 / 干跑构造成功
```

`selection.json` 的字段规范（只含脱敏别名与件 id，不含患者明细）：

```json
{
  "encounter_alias": "WARD-A-001",
  "actor": "脱敏-护士长A",
  "at": "2026-03-05T08:20:00+08:00",
  "ruleset_sha256": "<64 位>",
  "entries": [
    {
      "doc_id": "WARD-A-001-CONSENT-01",
      "content_sha256": "<64 位>",
      "resource": { "resourceType": "Composition", "status": "preliminary" }
    }
  ]
}
```

空勾选与真发**同码**（不出现「干跑报成功、真发报没勾」）：

```bash
./target/debug/psychdoc writeback-dryrun empty-selection.json   # entries: []
```

```text
psychdoc: E_FHIR_TXN_EMPTY：勾选清单是空的，拒绝构造 transaction（与真发同码 E_FHIR_TXN_EMPTY）
退出码 2
```

---

## 4. `psychdoc export-audit` — 审计导出包

```bash
export PSYCHDOC_DATA_DIR=/tmp/psychdoc_demo_data     # 库落在用户数据目录，不落项目目录
./target/debug/psychdoc check ../fixtures/ward-11-cross-doc-violations.json \
  --ruleset ../rulesets/hospital.yaml --record default --actor "脱敏-护士长A"
# → 已记入本机审查库：批次 run-1a1cca8504e5（报出 7 条 · 折叠 0 条）

./target/debug/psychdoc export-audit --format csv
```

批次摘要（导出前先核对范围）：

```text
审计导出：csv · 批次 1 次 · 证据行 7 条（报出 7 · 折叠 0）
筛选：住院别名 全部 · 批次 全部 · 时间段 不限 ~ 不限
  · run-1a1cca8504e5 · WARD-I-001 · 规则集 0.6.0（sha256 2abfac00aa5b）· 脱敏-护士长A 于 2026-10-07T01:21:58+08:00 · 审 5 件（跳过 0）· 命中码 [QC-PSY-05、QC-PSY-06、QC-PSY-07、QC-PSY-08] · 本次导出 7 行（报出 7 · 折叠 0）

导出期二次脱敏：未发现未脱敏内容（标识列与自由文本列均已扫过）。
（以下为 csv 正文，带 UTF-8 BOM；要成文件请加 --out <路径>）
```

csv 表头与首行（真实回显，字段过长处已折行）：

```csv
审查批次,住院别名,文书件id,段落序号,字符区间,规则码,处置,严重度,规范依据,问题描述,豁免原因,折叠去向,规则集版本,规则集sha256,规则来源层,操作人,时刻,写回幂等键
run-1a1cca8504e5,WARD-I-001,WARD-I-001-RECORD-01,0,"[7, 24)",QC-PSY-05,已报出,blocking,《精神科住院护理常规》第 6.1 条：约束解除记录填写的解除时间不得早于约束记录填写的约束起始时间，二者倒挂的病历不得直接归档,约束记录的约束起始时间（2026-03-04 08:00）早于约束解除记录的解除时间（2026-03-04 07:00），解除时序倒挂,,,0.6.0,2abfac00aa5b7046ceb004f07b4347d59419f9f9c3c7c9893cd103e5a086eb59,QC-PSY-05（院级）,脱敏-护士长A,2026-10-07T01:21:58+08:00,
```

18 列依次是：批次、住院别名、件 id、段落序号、字符区间、规则码、处置、严重度、规范依据、
问题描述、豁免原因、折叠去向、规则集版本、规则集 sha256、规则来源层、操作人、时刻、写回幂等键。
`豁免原因` 与 `折叠去向` 两列让「这条为什么没报 / 为什么被合并」在导出包里说得清。

---

## 5. `psychdoc preflight` — 启动自检

```bash
./target/debug/psychdoc preflight     # 未设任何环境变量
```

```text
启动自检（本次未发起任何网络请求）：
  [缺失] EMR_BASE_URL（未设置或为空白）
  [缺失] EMR_CLIENT_ID（未设置或为空白）
  [缺失] EMR_CLIENT_SECRET（未设置或为空白）
  [缺失] EMR_TENANT_ID（未设置或为空白）
  [缺失] WECOM_WEBHOOK_URL（未设置或为空白）
  [说明] 本次自检未发起任何网络请求；缺项会导致对应链路返回错误，不会静默降级为本地假成功

  [拦截] E_CFG_MISSING: 缺少 5 项必需环境变量：EMR_BASE_URL、EMR_CLIENT_ID、EMR_CLIENT_SECRET、EMR_TENANT_ID、WECOM_WEBHOOK_URL（不会降级为本地替身或离线模式）
psychdoc: E_CFG_MISSING：…
退出码 2
```

---

## 6. `psychdoc rule-regress` — 规则回归与覆盖矩阵

```bash
./target/debug/psychdoc rule-regress
```

```text
  [通过] psy-08-positive-signed-without-moment · QC-PSY-08 · 正例
  [通过] psy-08-negative-signed-with-moment · QC-PSY-08 · 反例（误报守卫）
  [通过] psy-08-boundary-half-signed-page · QC-PSY-08 · 边界例

规则码覆盖矩阵（正例 / 反例 / 边界例）：
  [齐] QC-PSY-01 · 件内必填字段缺失 · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-02 · 字段值形状不符 · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-03 · 取值域越界 · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-04 · 单位 / 量纲错配 · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-05 · 事件与记录时序倒挂（解除早于开始） · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-06 · 评估与医嘱时序矛盾（评估晚于首次约束） · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-07 · 约束与签署时序错误（签署晚于约束开始） · 正例 1 · 反例 1 · 边界例 1
  [齐] QC-PSY-08 · 签署齐备性（已签但无签署时刻） · 正例 1 · 反例 1 · 边界例 1
覆盖率 100%

回归全绿：规则集与用例集一致。
退出码 0
```

24 条用例 = 8 码 × 正例 / 反例 / 边界例。删用例**不算**回归通过——
覆盖矩阵会立刻标出缺位并退 1。