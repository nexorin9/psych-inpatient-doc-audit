# 端到端主路径复现手册

> 本文件是 Task 40 的可复现记录。README「验收与自检」一节引用这里。
> 全部命令在项目根 `psych-inpatient-doc-audit/` 下执行，**不需要**打开窗口。

## 跑完整条链

```bash
cd core
cargo test -p psychdoc --test e2e -- --nocapture
```

一条用例覆盖九环，任一环断掉即整条红：

```
拖入文件夹 → 队列聚合 → 批审查清单 → 补写预览 diff → 勾选
   → Bundle transaction（真 socket）→ Provenance → 状态推进 → 缺件通知 → 审计回溯
```

| 用例 | 钉住的不变量 |
|------|--------------|
| `the_whole_chain_runs_once_from_a_dropped_folder_to_a_delivered_todo` | 九环逐环断言；transaction 只发一次；只推进被勾的那一件 |
| `confirming_the_same_item_twice_sends_nothing_and_moves_no_state` | 同件同日重复确认 → 0 次外发、0 条新写回、状态不回退 |
| `sending_the_same_missing_item_todo_twice_on_the_same_day_makes_one_call` | 同日同件通知只发一次 |
| `a_failing_endpoint_writes_nothing_locally_and_keeps_the_draft` | 端点 500 → 本地 0 条已生效事实、草稿与勾选状态保留 |
| `the_audit_export_can_recall_the_ruleset_and_codes_in_force_then` | 导出包带齐定位/依据/推进人/版本；早于任何审查的时刻回溯「查无此批」 |
| `the_cli_reaches_the_same_verdict_with_no_window_open` | 无 GUI 时 `check` / `graph` / `writeback-dryrun` 给出同一结论 |

## 无 GUI 手工复现（同一批夹具）

```bash
B=core/target/debug/psychdoc
R=rulesets/hospital.yaml

# 1) 批审查：有阻断项退 1，清单带码 + 段落定位 + 规范依据
$B check fixtures/ward-08-field-violations.json --ruleset $R ; echo "退出码 $?"

# 2) 归档核齐视图：正反向各答一遍，未核齐退 1
$B graph WARD-F-001 --input fixtures --ruleset $R \
   --direction both --as-of 2026-03-10T09:00:00+08:00 ; echo "退出码 $?"

# 3) 审查落库（--record 是显式开关：看 ≠ 留）
$B check fixtures/ward-08-field-violations.json --ruleset $R \
   --record /tmp/psychdoc-demo.sqlite3 --actor 脱敏-护士长A

# 4) 审计导出 + 版本回溯
$B export-audit --db /tmp/psychdoc-demo.sqlite3 --alias WARD-F-001 \
   --at 2099-01-01T00:00:00+08:00 --out /tmp/audit.csv
```

## Task 40 修掉的主路径缺陷

首轮 `cargo test --test e2e` 有 1 例红，报
`E_STATE_UNKNOWN: 库里没有件 WARD-F-001-ASSESS-01，不能推进状态`。
这不是测试写错，是**产品缺陷**，且是最难在模块级测试里发现的那一类：

- `psychdoc_ffi::api::commit_writeback` 从不调用 `register_items` /
  `init_state`，直接建草稿就发 transaction；
- `confirm()` 的第 6 步要推进状态时，`state_of` 返回 `None` → 报
  `E_STATE_UNKNOWN`；
- **而这一步在 transaction 外发之后、在 `writeback_log` 落库之后。**

后果是：护士长在界面上点确认 → 服务端**已经建好了件** → 本地写回日志
**已经记上了** → 界面弹一个「写回失败」。界面说没成，事实已经成了，
护士长多半会再点一次，于是同一件事在服务端留下两份件。

修法：`ensure_draftable()` 在 `create_draft` 之前、**任何网络调用之前**
把待写回的件放到状态机允许的草稿起点上：

- 未登记 → 按「缺失」登记，再走唯一合法边 `缺失 → 草稿`（建件本就应从
  缺失起步，直接落「已建件」等于跳过起草那一态）；
- 已在草稿 → 不动；
- 在别的态（已建件 / 已收 / 可归档）→ **不动**，让 `confirm` 如实报错。
  一次写回不该偷偷改已在流转中的业务事实，那属于越权。

顺带补齐审计链的两个空档：`CommitRequest` 增加 `ruleset_sha256` 与
`run_id`。此前 `ConfirmContext.ruleset_sha256` 被写死成空串、批次号恒为
`None`，于是 Provenance 里的版本字段永远是空的、`writeback_log.run_id`
永远为空——「这份件是按哪版规则、由哪一次审查发现的」答不出来。
两个字段一路透传到 `bindings.rs` 与 `desktop/core_bridge.py`。

### 反向验证

临时撤掉 `ensure_draftable(&store, req)?;` 后重跑：

```
test result: FAILED. 4 passed; 2 failed
  the_whole_chain_runs_once_from_a_dropped_folder_to_a_delivered_todo
  confirming_the_same_item_twice_sends_nothing_and_moves_no_state
  ← E_STATE_UNKNOWN: 库里没有件 WARD-F-001-ASSESS-01，不能推进状态
```

恢复后 6 例全绿。修复确属承重，不是让测试变绿的空改。

## 替身纪律

`core/crates/psychdoc/tests/e2e.rs` 里的 `FakeFhir` / `FakeWeCom` 是
**测试数据层**的脱敏协议替身，不是产品的最终对接面。两者都起**真
socket**，请求过真实 HTTP 编解码——`Content-Type` 写错、Provenance 少一条
`request.url`、token 端点用 JSON 而非表单，这些只有真过一遍才现形。

生产路径永远是 `UreqTransport` 指向 `EMR_BASE_URL` / `WECOM_WEBHOOK_URL`
的院内真实端点，五项凭据从环境变量读。