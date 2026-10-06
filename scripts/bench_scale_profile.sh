#!/usr/bin/env bash
# 规模基准与性能剖析 —— 可复现脚本。
#
# README「验收与自检」表与 docs/bench/scale-profile.md 都引用本脚本。
# 跑法：
#
#   ./scripts/bench_scale_profile.sh              # 三档规模 + 增量命中率 + 规模敏感度
#   ./scripts/bench_scale_profile.sh --tier-only  # 只跑三档规模的正确性/耗时/内存
#
# 产出：
#   docs/bench/scale-profile.md   分段耗时报告（解析 / 求值 / 建图与阻塞传播 / 写库）
#
# 全部夹具为**合成脱敏数据**，由 psychdoc_parse::synth 按交付规则集的字段规范
# 生成，不含任何真实患者标识。峰值内存取进程级 getrusage(RUSAGE_SELF).ru_maxrss。

set -euo pipefail

cd "$(dirname "$0")/.."
# workspace 根在 core/ 下（PySide6 壳不在 workspace 内）
CARGO=(cargo test --manifest-path core/Cargo.toml)

export CARGO_TERM_COLOR=never

MODE="${1:-all}"

echo "== 1/4 三档规模：正确性 + 耗时上限 + 峰值内存上限 =="
# 逐个跑：cargo test 只接**一个**名字过滤器。
for t in \
  scale_tier_of_200_items_meets_its_time_and_correctness_budget \
  scale_tier_of_500_items_meets_its_time_and_correctness_budget \
  scale_tier_of_1000_items_meets_its_time_and_correctness_budget \
  the_conforming_corpus_stays_clean_at_every_scale \
  every_generated_scale_tier_carries_no_real_patient_identifier
do
  echo "--- $t"
  "${CARGO[@]}" --release -p psychdoc-parse --test scale_profile -- --nocapture --exact "$t"
done

if [ "$MODE" = "--tier-only" ]; then
  echo
  echo "三档规模全部通过（--tier-only：跳过增量与敏感度）。"
  exit 0
fi

echo
echo "== 2/4 增量命中率：改 1 件 → 只重跑 1 件 =="
for t in \
  changing_one_item_reruns_exactly_one_at_every_scale \
  the_second_run_with_no_change_at_every_scale_hits_a_full_cache
do
  echo "--- $t"
  "${CARGO[@]}" --release -p psychdoc-parse --test scale_profile -- --nocapture --exact "$t"
done

echo
echo "== 3/4 规模敏感度：规则条数翻倍与件数增长的近线性 =="
for t in \
  the_doubled_ruleset_really_doubles_and_still_loads \
  doubling_the_rule_count_keeps_the_batch_time_near_linear \
  review_time_grows_sub_linearly_in_the_item_count
do
  echo "--- $t"
  "${CARGO[@]}" --release -p psychdoc-parse --test scale_profile -- --nocapture --exact "$t"
done

echo
echo "== 4/4 生成 profile 报告 =="
"${CARGO[@]}" --release -p psychdoc-parse --test scale_profile -- --nocapture --exact write_the_profile_report

echo
echo "分段耗时报告已写到 docs/bench/scale-profile.md"
echo
echo "规模基准全部通过。"
