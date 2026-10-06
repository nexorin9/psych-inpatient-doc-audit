# 命令行通道（`psychdoc`）

> 给不点窗口的人：信息科的服务器跑批、CI 回归、cron 夜间核齐。

桌面窗口是给护士长和病案室用的；这条 CLI 是给**只看一个数字**的自动化用的。
两者读的是同一套核心（`psychdoc-parse` / `psychdoc-rules` / `psychdoc-graph`），
不存在「界面说有问题、脚本说没问题」的第二套判断。

---

## 退出码契约

这是本文件最重要的一张表。CI 只读退出码，所以**每个码对应谁该被通知**必须钉死。

| 退出码 | 含义 | 触发条件（逐条） | 谁该处理 | 能不能重试 |
|--------|------|------------------|----------|-----------|
| **0** | 成功 | `check` 零阻断诊断；`graph` 该住院已核齐（零缺件 + 零连带阻塞 + 零人工待办）；`writeback-dryrun` 报文构造完成 | 无人，流水线通过 | — |
| **1** | 审查不通过 | `check` 存在 `severity=blocking` 的诊断；`graph` 视图未核齐；`rule-regress` 有用例失败或覆盖矩阵缺位；`export-audit` 导出包存在处置对账不平 | **护士长 / 质控办**（业务整改） | **不要重试**。重试一百遍还是这些缺件 |
| **2** | 配置或用法问题 | 缺 `EMR_*` 环境变量（`E_CFG_MISSING`）；环境变量形态非法（`E_CFG_INVALID`）；规则集 YAML 非法（`E_RULESET_INVALID`）；参数写错（`E_USAGE`）；勾选清单为空（`E_FHIR_TXN_EMPTY`）；输入不是合法 Bundle（`E_BUNDLE_SHAPE`）；依赖图成环或对拍不上（`E_DEP_CYCLE` / `E_DIR_PAIRING`） | **调脚本的人 / 改规则集的人** | **不要重试**。这是配置，重试只是刷日志 |
| **3** | 外部依赖不可达 | EMR 端点连不上或超时（`E_FHIR_UNREACHABLE`）；端点返回非 2xx（`E_FHIR_STATUS`）；分页游标循环（`E_FHIR_PAGING`）；退避后仍失败（`E_FHIR_RETRIES_EXHAUSTED`）；写回事务被拒（`E_FHIR_TXN_REJECTED`）；企业微信通知失败（`E_WECOM_*`） | **信息科 / 网络组** | **可以重试**，配合退避 |

### 为什么 1 与 2/3 必须分开

审查不通过是**业务事实**：这份病历确实缺件，护士长要整改。
配错了 / 连不上是**运维事实**：谁改了规则集 YAML、哪段网络断了。
合成一个码，CI 就会把「规则集被改坏」当成「病历有问题」重跑一百遍，
或者反过来把真实缺件当成配置抖动压过去 —— 后一种更糟，因为它**永远不会有人去改**。

### 判读速查（贴进运维手册）

```bash
psychdoc check exports/ --ruleset rulesets/hospital.yaml
case $? in
  0) echo "合规，本批归档" ;;
  1) echo "有阻断项：把清单发给对应病区，不要重试" >&2; exit 1 ;;
  2) echo "配置或规则集有问题：查 E_ 开头错误码，不要重试" >&2; exit 2 ;;
  3) echo "EMR 端点不可达：退避后重试" >&2; exit 3 ;;
esac
```

---

## 子命令

| 子命令 | 用途 | 主要选项 |
|--------|------|----------|
| `check` | 审一份 / 一目录文书包，打带码带段落定位的清单 | `--ruleset` `--cache` `--record` `--actor` `--live` |
| `graph` | 输出某次住院的归档核齐视图（件清单 / 阻塞边 / 欠件天数 / 人工队列） | `--input` `--ruleset` `--direction` `--as-of` `--db` |
| `writeback-dryrun` | 构造 Bundle transaction 并打印报文摘要，**零网络请求** | `--out` |
| `rule-regress` | 跑规则回归用例集 + 覆盖率矩阵 | `--cases` `--root` `--only` `--json` |
| `export-audit` | 从本机库导出审计证据包（csv / json） | `--db` `--alias` `--run` `--at` `--format` `--out` |

全局选项（五个子命令都接受）：`--json`、`--log-file <路径>`。

### `check`

```bash
# 从本地导出目录审
psychdoc check exports/ward-a/ --ruleset rulesets/hospital.yaml

# 直接从院内 FHIR R4 端点拉一次住院的件（无人值守跑批的真实入口）
psychdoc check --live WARD-A-001 --ruleset rulesets/hospital.yaml

# 增量：按件内容哈希跳过未变过的件
psychdoc check exports/ --ruleset rulesets/hospital.yaml --cache /var/lib/psychdoc/cache.json

# 落进本机库，供 export-audit 导出审计包
psychdoc check exports/ --ruleset rulesets/hospital.yaml --record default --actor 张护士长
```

`--live` 与位置参数**互斥**：前者自己从 EMR 拉件，不读本地文件。
两者都给会报错而不是「静默用其中一个」——那会让「我明明传了路径」这类
误解一直拖到出结果时才暴露。

### `graph`

```bash
# 这份病历能不能归档？
psychdoc graph WARD-B-001 --input fixtures/ward-02-restraint-set.json \
  --ruleset rulesets/hospital.yaml --as-of 2026-03-10T09:00:00+08:00

# 只问「这件在等谁」（正向）
psychdoc graph WARD-B-001 --input exports/ --ruleset rulesets/hospital.yaml --direction dependencies

# 只问「这件卡住谁」（反向）
psychdoc graph WARD-B-001 --input exports/ --ruleset rulesets/hospital.yaml --direction dependents
```

| `--direction` | 回答的问题 |
|---------------|------------|
| `dependencies` | 这件**在等**谁（件 → 它要求的前置件） |
| `dependents` | 这件**卡住**了谁（前置件 → 要求它的件） |
| `both` | 两个方向都给 |

两个方向必须给出**不同**的答案，且正反向并集等于可达集减去自身。
出图前先跑这道对拍（`check_direction_pairing_all`），对不上就**拒绝出视图**
并报 `E_DIR_PAIRING` —— 给一份「看起来齐全」的视图去归档签字，比报错糟得多。

`--as-of` 给的是「以哪一刻算欠件天数」。不传就用当前时间；
**传了就以它为准**，这样同一份数据两次跑出的天数可以逐字节复现。

### `writeback-dryrun`

```bash
psychdoc writeback-dryrun selection.json --out /tmp/bundle.json
```

读一份勾选清单，**用与真实写回同一个构造函数**构造 `Bundle{type=transaction}`
并打印报文摘要与幂等键，**一次网络请求都不发**——不取 token、不连端点、不落库。

存在的理由：CI 上没有凭据，生产上没人敢拿真文书包反复试，于是「这台机器上
打算发出去的报文长什么样」没人能回答，直到某天真发出去才发现少了一条
`request.url`。干跑把这个洞补上。

集成测试用**真实 `TcpListener` 数连接数**来钉住「零外发」这条性质
（`core/crates/psychdoc/tests/cli_contract.rs`），不是靠「代码里看起来没有 send」。

`selection.json` 的形状（`deny_unknown_fields`：多打一个字段直接报错，
不静默忽略）：

```json
{
  "encounter_alias": "WARD-B-001",
  "actor": "病区文书专员",
  "at": "2026-03-03T07:00:00+08:00",
  "ruleset_sha256": "2abfac00aa5bd…",
  "entries": [
    {
      "doc_id": "WARD-B-001-CONSENT-01",
      "content_sha256": "aaaa1111bbbb2222…",
      "resource": { "resourceType": "Composition", "id": "WARD-B-001-CONSENT-01", "status": "preliminary" }
    }
  ]
}
```

---

## `--json`：stdout 上只有一段 JSON

```bash
psychdoc --json check exports/ --ruleset rulesets/hospital.yaml | jq '.blocking_count'
```

| 键 | 含义 |
|----|------|
| `ok` | 是否通过（等价于退出码 0） |
| `subcommand` | `check` / `graph` / `writeback-dryrun` |
| `exit_code` / `exit_meaning` | 退出码与其中文说明 |
| `blocking_count` / `notice_count` | 阻断项与提示项条数 |
| `diagnostics[]` | 每条含 `code` `doc_id` `section_index` `char_start` `char_end` `message` `basis` `fix_availability` |
| `parse_issues[]` | 解析期问题（读不动 / 类型不在字典 / 重复 id） |
| `skipped_checks[]` / `tolerated_checks[]` | 「判不出来」与「判出来不算违规」——两者都不是「通过」 |
| `ruleset.sha256` | 当时生效的规则集版本，供事后按版本回溯 |

**硬规矩**：开了 `--json` 之后，人类可读的过程文字**只进日志文件，不再上 stdout**。
两样东西混在一条 stdout 上，`jq` 就再也解析不动了。

---

## `--log-file`：完整过程落盘

```bash
psychdoc --log-file logs/nightly-check.log check exports/ --ruleset rulesets/hospital.yaml
```

- **覆盖写**（`truncate`）而不是追加：跑批日志按「一次跑批一个文件」组织。
  追加只会让第二次跑批的结论接在第一次的判定后面，CI 抓取到的退出码
  对应哪一段就说不清了。
- 父目录不存在时自动创建。
- 与 `--json` 同用时，日志收人类可读过程，stdout 收 JSON 正文。
- **写日志失败不影响主流程**：审查结论已经算出来了，
  因为日志盘满了就丢掉整次审查是不可接受的反向失败。

---

## 关键配置项（环境变量）

| 变量 | 用途 | 缺失时 |
|------|------|--------|
| `EMR_BASE_URL` | FHIR R4 服务根地址 | `E_CFG_MISSING` → 退出 2，**0 次网络调用** |
| `EMR_CLIENT_ID` | OAuth2 客户端 id | 同上 |
| `EMR_CLIENT_SECRET` | OAuth2 客户端密钥 | 同上 |
| `EMR_TENANT_ID` | 院内租户标识（写入 `X-Tenant-Id` 头） | 同上 |
| `WECOM_WEBHOOK_URL` | 企业微信缺件通知出站 | 仅通知链路需要，不阻塞审查 |

四项 `EMR_*` **一次列全**缺失项，而不是报一个跑一次。
凭据校验发生在构造 HTTP 客户端之前，因此「缺配置」在物理上就不可能
先发出网络请求 —— 这也是退出码 2 与 3 分得开的根据。

`check` / `graph` / `writeback-dryrun` / `rule-regress` / `export-audit`
五个子命令里，**只有 `check --live` 需要凭据**。其余四个纯本地。

---

## 源产品主路径的领养位置

本 CLI 改写自源产品（Python linter 家族）的「一条命令拿全量 → 逐文件解析 →
按稳定编号输出码 + 位置 + 为什么这是问题」这条主路径：

| 源侧 | 本产品侧 |
|------|----------|
| `cli.py` 薄壳 → `main()` 分发子命令 | `core/crates/psychdoc/src/main.rs` → `run()` 分发五个子命令 |
| `check` 逐文件解析并打印 | `check` 读 FHIR R4 导出包（单份 / 整个目录 / `--live` 直连 EMR） |
| `Violation(code, range, message)` | `Diagnostic(code, loc, message, basis, fix_availability)` |
| `analyze_graph --direction` 正反两向 | `graph --direction both\|dependencies\|dependents` |
| 缓存避免重跑未变更文件 | `check --cache` 按件内容哈希增量重跑 |
| 稳定的规则编号供 CI 判读 | `QC-PSY-nn` + `E_*` 错误码 + `0/1/2/3` 退出码 |

---

## 自检

```bash
cd core && cargo test -p psychdoc --test cli_contract
```

13 条契约测试，四种退出码各钉一个真实场景，打的是**编译出来的真二进制**
（`env!("CARGO_BIN_EXE_psychdoc")`）——不是函数返回值，因为 CI 读的是
`process.exit_code`，函数对得上而进程码对不上，契约就等于没钉。