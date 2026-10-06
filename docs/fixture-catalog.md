# 夹具清单 —— 按文书类型的最小字段集与注入方式

本文件是**夹具数据层的目录**：院内在用的七类文书各需要哪些字段、生成器按什么
参数造、每类违规在核心眼里从哪条通道能看见。改夹具前先读这里，别直接改 JSON。

- 生成器：`scripts/fixture_kit.py`（可复现，不取当前时间、不用随机数）
- 数据位置：`fixtures/`（手写定向夹具）与 `fixtures/generated/`（生成器产物）
- 生成器产物每份都带 `_fixture_meta`，写明生成参数与**预期观察点**
- 重新生成：`python3 scripts/fixture_kit.py --out fixtures/generated`

## 七类文书类型的最小字段集

与 `rulesets/hospital.yaml` 的 `templates` 段逐条对齐（`tests/test_fixtures.py`
里有断言守着两边不漂移）。括号内为必填性。

| 文书类型码 | 中文名 | 段落 | 最小字段集 |
|---|---|---|---|
| `inpatient_record` | 住院记录 | 入院记录 | 入院时间（必填 · 日期时间）、诊断（必填） |
| `nursing_record` | 护理记录 | 护理记录 | 巡视时间（选填）、病情描述（必填） |
| `restraint_consent` | 约束知情同意 | 约束事由 / 患方知情同意 | 约束事由（必填）、患方意见（必填）、约束起始时间（必填 · 日期时间）、患方签字（必填） |
| `restraint_record` | 约束记录 | 约束经过 | 约束起始时间（必填 · 日期时间）、执行护士（必填）、约束等级（必填 · 限「一级/二级/三级保护」）、观察记录（选填 · 不声明段落，故同键多段走歧义）、约束时长（选填 · 数值 1~720 · 单位「分钟」） |
| `restraint_release` | 约束解除记录 | 解除经过 | 解除时间（必填 · 日期时间）、解除依据（必填）、执行护士（必填） |
| `assessment_report` | 评估报告 | 入院评估 | 评估时间（必填 · 日期时间）、风险等级（必填 · 限「低/中低/中高/高风险」）、评估医师（必填） |
| `sign_page` | 签署页 | 签署页 | 护士签字（必填）、医师签字（必填） |

两份结构性约定，造夹具时必须遵守：

1. **「观察记录」不声明段落**。纸质表单上它落在随手写的空格里，同一个键出现在
   多段时按「字段归属歧义」交人工，**不取第一段当真值**。生成器照此办理。
2. **时间粒度要写实**。`authoredOn` 该到分就到分、该只到日就只到日。粗粒度不是脏
   数据，是业务事实（签署页只印一个日期）；一律写细会让「低精度不得判违规」这条
   降级规则在夹具上测不出来。

## 可注入的四类违规

`--defects` 接受这四个名字，逗号分隔。不给就是全合规包（误报守卫）。

| 违规名 | 注入方式 | 核心从哪条通道看见 | 预期码 / 结构 |
|---|---|---|---|
| `time_inversion` | 约束解除记录的解除时间改写为 09:00（早于约束起始 10:00），`authoredOn` 与 `date` 同步改 | `check_bundle` → `summary.diagnostics` | `QC-PSY-05`，违反的**两件各一条** |
| `signature_missing` | 签署页删掉「医师签字」整行 | `check_bundle` → `summary.diagnostics` | `QC-PSY-01`（件内必填缺失，`FixAvailability::None`） |
| `missing_item` | 整份约束知情同意不生成 | `graph_both_directions` → `blocking_edges` | 恰好 1 条阻塞边指向 `::missing::restraint_consent` 占位节点 |
| `binding_ambiguity` | 约束记录由 HIS 与护理系统各导出一份（两份 `identifier.system` 不同） | `check_bundle` → `summary.skipped_checks` + `graph_both_directions` → `manual_queue` | 零诊断；三条跨文书规则整条跳过并记「绑定不唯一」，同时进人工队列 |

**为什么不产诊断的也算违规**：缺件与绑定歧义在业务上确实是「这份病历交不出去」，
但它们不是某一件填错了，判它得跨到依赖图和绑定层。把它们塞进规则表只会让
规则集多两条永远判不中的码。

## 基准文书包（`fixtures/generated/`）

五次住院，每类违规各一次，外加一次全合规作误报守卫。

| 文件 | 住院别名 | 件数 | 注入 | 期望观察点 |
|---|---|---|---|---|
| `ward-p-001-time_inversion.json` | `WARD-P-001` | 7 | `time_inversion` | `QC-PSY-05` 2 条 |
| `ward-p-002-signature_missing.json` | `WARD-P-002` | 7 | `signature_missing` | `QC-PSY-01` 1 条 |
| `ward-p-003-missing_item.json` | `WARD-P-003` | 6 | `missing_item` | 1 条阻塞边 + `QC-PSY-07` 未判定（对照件不在包内） |
| `ward-p-004-binding_ambiguity.json` | `WARD-P-004` | 8 | `binding_ambiguity` | 零诊断 + 3 条未判定 + 人工队列 1 |
| `ward-p-005.json` | `WARD-P-005` | 7 | —（全合规） | **零诊断**，任何通道都不得报出东西 |

**为什么要拆成五次住院而不是塞一包里**：绑定歧义会让同类型的跨文书配对整对跳过。
歧义和时间序倒挂放同一次住院，后者的结论会被前者盖掉，两条都测不出来。

## 怎么手工验一份

```bash
# 时序倒挂：恰好 2 条 QC-PSY-05，退出码 1
cd core && cargo run -p psychdoc -- check ../fixtures/generated/ward-p-001-time_inversion.json \
  --ruleset ../rulesets/hospital.yaml

# 全合规包：未发现规则命中项，退出码 0
cd core && cargo run -p psychdoc -- check ../fixtures/generated/ward-p-005.json \
  --ruleset ../rulesets/hospital.yaml

# 缺件：未发现规则命中项，但「未判定的跨文书检查」里有一条 QC-PSY-07
cd core && cargo run -p psychdoc -- check ../fixtures/generated/ward-p-003-missing_item.json \
  --ruleset ../rulesets/hospital.yaml

# 重新生成，验证可复现（与 git diff 应当为空）
python3 scripts/fixture_kit.py --out fixtures/generated && git diff --stat
```

## 造新夹具的三条约束

1. **脱敏**：住院别名用 `WARD-X-NNN`，执行者用 `<EXAMPLE_NURSE>` /
   `<EXAMPLE_PRACTITIONER>` / `<EXAMPLE_SIGNATURE>`。**不得**出现姓名、住院号、
   身份证件号——`tests/test_fixtures.py::test_no_fixture_carries_a_patient_identifier`
   与 Rust 侧 `all_fixtures_are_free_of_patient_identifiers` 各扫一遍。
2. **成对**：造一条违规夹具就要顺手造一份同场景的合规夹具，否则没有误报守卫。
   用生成器时这只是一次参数取值。
3. **留参数**：每份夹具写明生成参数（`_fixture_meta`）。质控办复核一份病历时问的
   是「你这批数据怎么来的」，答不上来的批次等于不可信。

**这不是最终对接面**：真实对接按 FHIR R4 规范实现，凭据从环境变量读
（`EMR_BASE_URL` / `EMR_CLIENT_ID` / `EMR_CLIENT_SECRET` / `EMR_TENANT_ID`）。
本目录只服务于测试与基准。
