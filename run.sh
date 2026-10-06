#!/usr/bin/env bash
# 精神科住院文书审查系统 —— Linux / macOS 启动脚本
#
# 用法：
#   ./run.sh                       启动桌面窗口
#   ./run.sh <导出目录|文书包>       启动并直接载入一份文书包
#   ./run.sh --selfcheck           只做启动自检（不建窗口，供服务器 / CI 无人值守跑）
#
# 退出码契约与 CLI 一致：0 正常 / 1 审查不通过 / 2 配置缺失 / 3 外部依赖不可达。
set -euo pipefail

cd "$(dirname "$0")"
export PYTHONPATH="$PWD:$PWD/bindings:${PYTHONPATH:-}"

fail() { echo "[启动失败] $*" >&2; exit "${2:-2}"; }

SELFCHECK_ONLY=0
PRESET=""
for arg in "$@"; do
  case "$arg" in
    --selfcheck) SELFCHECK_ONLY=1 ;;
    *)           PRESET="$arg" ;;
  esac
done

# ---------------------------------------------------------------- 1) Python
command -v python3 >/dev/null 2>&1 || fail "未找到 python3，请先安装 Python 3.10 及以上版本。"
python3 - <<'PY' || exit 2
import sys
if sys.version_info < (3, 10):
    sys.stderr.write(
        f"[启动失败] Python 版本过低：{sys.version.split()[0]}，需要 3.10 及以上。\n"
    )
    raise SystemExit(1)
PY

# ---------------------------------------------------------------- 2) PySide6
python3 -c "import PySide6" 2>/dev/null \
  || fail "未安装 PySide6，请先执行： pip install PySide6"

# ---------------------------------------------------------------- 3) 核心绑定
# 两条构建路径都认：装了 maturin 走 maturin develop；只装了 cargo 就用
# cargo build --features extension-module，再把产物拷到 bindings/。
# 两条都不可用时给中文提示并以非 0 退出，**不**降级成一个点开就报错的空窗口。
#
# rustup 默认把 cargo 装在 ~/.cargo/bin。交互式 shell 一般由 .zshrc 把它加进
# PATH，但脚本、CI 与「从桌面图标启动」拿不到这份 profile —— 于是这里明明装了
# Rust 工具链却报「未找到 cargo」。补进 PATH，来源不存在时是无害的空操作。
case ":$PATH:" in
  *":$HOME/.cargo/bin:"*) ;;
  *) if [ -d "$HOME/.cargo/bin" ]; then PATH="$HOME/.cargo/bin:$PATH"; fi ;;
esac
export PATH

if ! python3 -c "import psychdoc_ffi" 2>/dev/null; then
  # maturin develop 需要一个虚拟环境（VIRTUAL_ENV / CONDA_PREFIX / .venv）。
  # 装了 maturin 但没建虚拟环境时它会失败，此时**必须**接着走 cargo 路径，
  # 不能直接失败：否则「从零 clone 后第一次跑」会被一个可选工具卡死。
  if command -v maturin >/dev/null 2>&1 \
     && maturin develop --manifest-path core/crates/psychdoc-ffi/Cargo.toml; then
    echo "[构建中] maturin develop 完成。"
  else
    command -v cargo >/dev/null 2>&1 \
      || fail "核心绑定 psychdoc_ffi 未构建，且未找到 maturin / cargo。\n       装其一即可： pip install maturin，或安装 Rust 工具链。"
    echo "[构建中] cargo build -p psychdoc-ffi --features extension-module（首次较慢，请稍候）…"
    ( cd core && cargo build -p psychdoc-ffi --features extension-module ) \
      || fail "核心绑定构建失败（cargo）。"
    mkdir -p bindings
    EXT="$(python3 -c 'import sysconfig; print(sysconfig.get_config_var("EXT_SUFFIX"))')"
    for lib in core/target/debug/libpsychdoc_ffi.dylib core/target/debug/libpsychdoc_ffi.so; do
      if [ -f "$lib" ]; then cp "$lib" "bindings/psychdoc_ffi${EXT}"; break; fi
    done
    [ -f "bindings/psychdoc_ffi${EXT}" ] \
      || fail "构建完成但找不到 libpsychdoc_ffi 产物，请检查 core/target/debug/。"
    echo "[构建中] 已安装到 bindings/psychdoc_ffi${EXT}"
  fi
fi

# 入口齐全性：少一个入口就说明绑定与壳不同步，宁可启动失败也不要半残窗口。
python3 - <<'PY' || exit 2
import sys
import psychdoc_ffi
need = ("check_bundle", "review_batch", "preview_fixback",
        "commit_writeback", "graph_both_directions",
        "scan_queue", "doc_card", "startup_report")
missing = [n for n in need if not hasattr(psychdoc_ffi, n)]
if missing:
    sys.stderr.write("[启动失败] 核心绑定缺少入口：" + "、".join(missing) + "\n")
    sys.stderr.write("           绑定与桌面壳版本不同步，请重新构建。\n")
    raise SystemExit(2)
PY

# ---------------------------------------------------------------- 4) 规则集
# 规则集读不到就**不启动**：没有规则时给出「全部通过」比不给结论更危险。
[ -f rulesets/hospital.yaml ] \
  || fail "读不到规则集 rulesets/hospital.yaml（E_RULESET_INVALID）。\n       系统不会在没有规则的情况下给出「全部通过」的结论。"

# ---------------------------------------------------------------- 5) 自检模式
if [ "$SELFCHECK_ONLY" -eq 1 ]; then
  exec python3 - <<'PY'
from desktop.main import preflight, writeback_hint

ok, message, code = preflight()
print(f"[自检] 规则集：{'OK（' + __import__('desktop.core_bridge', fromlist=['x']).default_ruleset_path().name + '）' if ok else message}")
hint = writeback_hint()
print("[自检] 写回凭据：OK" if not hint else f"[自检] {hint.splitlines()[0]}")
raise SystemExit(0 if ok else code)
PY
fi

# ---------------------------------------------------------------- 6) 凭据提示
# 写回凭据缺失**不拦启动**：护士长仍可只做本地文书包审查，点提交时才会拿到
# E_CFG_MISSING。把「启动」和「能写回」绑死，会让只想看清单的人也被迫配密钥。
MISSING=""
for k in EMR_BASE_URL EMR_CLIENT_ID EMR_CLIENT_SECRET EMR_TENANT_ID WECOM_WEBHOOK_URL; do
  [ -n "$(printenv "$k" 2>/dev/null || true)" ] || MISSING="$MISSING $k"
done
if [ -n "$MISSING" ]; then
  echo "[提示] 未配置：$MISSING"
  echo "       本次可照常审查本地文书包；写回与缺件通知会返回 E_CFG_MISSING，系统不会静默降级。"
fi

echo "[启动中] 精神科住院文书审查系统 …"
if [ -n "$PRESET" ]; then
  exec python3 -c "import sys; from desktop.main import main; sys.exit(main([sys.argv[1]]))" "$PRESET"
else
  exec python3 -c "import sys; from desktop.main import main; sys.exit(main())"
fi
