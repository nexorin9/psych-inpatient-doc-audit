# fixtures —— 脱敏测试数据层

存放合成脱敏的住院文书包（FHIR R4 Bundle），仅用于测试与基准。
**不是最终对接面**：真实对接按 FHIR R4 规范实现，凭据从环境变量读。
所有标识一律用脱敏别名（如 `<WARD>-<SEQ>`），不含姓名 / 住院号 / 身份证明文。
`psychdoc-parse/tests/discovery_timeline.rs::all_fixtures_are_free_of_patient_identifiers`
对本目录逐份扫描（含 `generated/` 子目录），改夹具前先看那条测试。

## 两个来源

| 位置 | 谁造的 | 用途 |
|---|---|---|
| 本目录 `ward-*.json` | 手写 | 定向靶子：一条夹具只钉一个行为（跨零点、低精度容差、GBK 脏数据……） |
| `generated/` | `scripts/fixture_kit.py` | **成套**文书包：七类文书齐全、彼此有真实时序，供跨文书规则与端到端链路用 |

生成物的七类字段最小集、四类可注入违规、基准包逐份预期，见
**`docs/fixture-catalog.md`**。重新生成（产物逐字节可复现）：

```bash
python3 scripts/fixture_kit.py --out fixtures/generated
```

## 夹具清单（手写定向靶子）

| 夹具 | 件数 | 覆盖什么 |
|---|---|---|
| `ward-01-minimal.json` | 1 | 最窄主路径：一份约束知情同意，缺必填「患方签字」（`QC-PSY-01`）。CLI `check` 的回归基准 |
| `ward-02-restraint-set.json` | 4 | 一次完整约束流程成套（同意 / 记录 / 解除 / 评估），跨零点时刻，用于时间序索引 |
| `ward-03-cross-system-dup.json` | 2 | HIS + 护理系统各一份「约束记录」→ 实体绑定歧义，交人工而非静默配对 |
| `ward-04-missing-moments.json` | 2 | 段内写了时刻但无日期 → 时间戳缺失，不兜底成当前时间 |
| `ward-05-mixed-entries.json` | 1 + 4 跳过 | 字典外类型码 / 缺住院别名 / 非文书附件，各自给出可核对的跳过原因 |
| `ward-06-dirty-data.json` | 2 + 1 跳过 | `section[].text` 为 null、日期坏、无 section：部分成功仍计成功 |
| `ward-07-cross-midnight.json` | 3 | 跨零点的真实时间差（不得按当日时分比） |
| `ward-08-field-violations.json` | 3 | **件-字段类四类违规各一处**：缺必填 / 全角日期 / 等级越界 / 单位错配（`QC-PSY-01`~`04`） |
| `ward-09-field-compliant.json` | 3 | 与 ward-08 同场景但全部合规 → **必须跑出 0 条**（误报守卫） |
| `ward-10-ambiguous-field.json` | 1 | 同一件内同一键出现在两段且模板未声明段落 → 不判定，但必须记账 |
| `ward-11-cross-doc-violations.json` | 5 | **跨文书四类违规各 1 处**（`QC-PSY-05`~`08`），件内字段全合规 → 执行器 A 零命中 |
| `ward-12-cross-doc-compliant.json` | 5 | 与 ward-11 同场景但全合规 → **必须跑出 0 条**（跨文书误报守卫） |
| `ward-13-tie-tolerance.json` | 5 | 同班次补记：解除时间比约束起始早 **3 分钟**。0 分钟容差下判倒挂，5 分钟并列容差下不判违规但记账（`chronology.tie-tolerance` 对照测试的靶子） |
| `ward-14-coarse-precision.json` | 2 | 精度不一致：患方签署 `authoredOn` 只写了 `2026-03-11`（FHIR dateTime 允许只到日），约束起始写到分 → 低精度时刻**不得**用来认定倒挂，交人工 |

## 怎么手工验一份

```bash
cd core
cargo run -p psychdoc -- check ../fixtures/ward-08-field-violations.json --ruleset ../rulesets/hospital.yaml
# 预期：4 条，四码各一条，退出码 1
cargo run -p psychdoc -- check ../fixtures/ward-09-field-compliant.json --ruleset ../rulesets/hospital.yaml
# 预期：未发现规则命中项，退出码 0
cargo run -p psychdoc -- check ../fixtures/ward-13-tie-tolerance.json --ruleset ../rulesets/hospital.yaml
# 预期：QC-PSY-05 不报（3 分钟落在 5 分钟并列容差内），另有一段「落在并列容差内、未判违规」+ 一段事件时间线
cargo run -p psychdoc -- check ../fixtures/ward-14-coarse-precision.json --ruleset ../rulesets/hospital.yaml
# 预期：未发现规则命中项，退出码 0；「未判定」里有一条 insufficient_precision，写明签署时刻只写到日
```

## 造新夹具的三条约束

1. **脱敏**：住院别名用 `WARD-X-00N`，执行者用 `<EXAMPLE_NURSE>` / `<EXAMPLE_PRACTITIONER>`。
2. **成对**：造一条违规夹具就要顺手造一份同场景的合规夹具，否则没有误报守卫。
3. **交班口气**：字段值带中文句号（`约束起始时间：2026-03-02 23:40。`）——
   解析层会剥文末句读，这是真实文书的样子，不要为了好解析而把句号去掉。
4. **时间粒度要写实**：`authoredOn` / `date` 该到分就到分，**该只到日就只到日**。
   粗粒度不是脏数据，是业务事实（签署页只印一个日期）；写细了会让
   「低精度不得判违规」这条降级规则在夹具上测不出来。