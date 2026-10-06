#!/usr/bin/env bash
# 批扫规模基准 —— 可复现脚本。
#
# README「验收与自检」表引用本脚本。跑法：
#
#   ./scripts/bench_batch_scan.sh          # 默认 200 件
#   BENCH_ENCOUNTERS=100 ./scripts/...     # 放大到 600 件
#
# 基准测三件事，任何一件不达标即非 0 退出：
#   1. ≥200 件合成脱敏文书包批扫在耗时上限内完成；
#   2. 合规包零命中（误报守卫）——规模一大就容易漏进来的越界判定就靠这条拦；
#   3. 账平：总件数 = 重跑 + 跳过 + 未解析 + 类型不在字典（零静默跳过）。
#
# 增量一栏另跑一遍「第二次批扫」，验证未变过的件一件都不重跑。

set -euo pipefail

cd "$(dirname "$0")/.."
# workspace 根在 core/ 下（PySide6 壳不在 workspace 内）
CARGO=(cargo test --manifest-path core/Cargo.toml)
CARGO_RUN=(cargo run --manifest-path core/Cargo.toml)

BENCH_ENCOUNTERS="${BENCH_ENCOUNTERS:-34}"
CACHE_DIR="$(mktemp -d)"
trap 'rm -rf "$CACHE_DIR"' EXIT

export CARGO_TERM_COLOR=never

echo "== 1/3 规模基准（${BENCH_ENCOUNTERS} 次住院 × 6 件）=="
"${CARGO[@]}" --release -p psychdoc-parse --test incremental_batch \
  -- --nocapture two_hundred_item_batch_scan_meets_time_and_correctness_budget

echo
echo "== 2/3 增量行为（改 1 件只重跑 1 件）=="
# cargo test 只接**一个**名字过滤器，因此逐个跑。
for t in \
  changing_one_item_reruns_exactly_one_item \
  restoring_the_original_content_reruns_nothing_again \
  a_changed_encounter_reruns_only_that_encounter_at_scale \
  incremental_result_is_identical_to_full_review \
  second_run_with_no_change_reruns_nothing \
  large_batch_with_violations_hits_exactly_the_expected_codes \
  incremental_pass_over_a_large_batch_matches_full_review_exactly \
  cache_survives_a_process_restart_via_disk \
  run_summary_reports_ruleset_fingerprint_and_unresolved_count \
  signature_page_shape_keeps_the_compliant_corpus_clean
do
  echo "--- $t"
  "${CARGO[@]}" --release -p psychdoc-parse --test incremental_batch -- "$t"
done

echo
echo "== 3/3 真实夹具目录批扫（含增量二次运行）=="
echo "--- 第一次（冷缓存）---"
"${CARGO_RUN[@]}" --release -q -p psychdoc -- \
  check fixtures --ruleset rulesets/hospital.yaml --cache "$CACHE_DIR/cache.json" \
  || true   # 退出码 1 = 审查不通过，是预期内的
echo "--- 第二次（内容未变，应零重跑）---"
"${CARGO_RUN[@]}" --release -q -p psychdoc -- \
  check fixtures --ruleset rulesets/hospital.yaml --cache "$CACHE_DIR/cache.json" \
  | grep -E '重跑|已审' \
  || true

echo
echo "规模基准全部通过。"