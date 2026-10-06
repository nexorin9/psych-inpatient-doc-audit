@echo off
REM 精神科住院文书审查系统 —— Windows 启动脚本
REM 语义与 run.sh 保持一致：本地审查不拦启动，缺写回凭据只提示；退出码 0/1/2/3 同 CLI。
setlocal enabledelayedexpansion
cd /d "%~dp0"
set "PY=python"

REM ---------------------------------------------------------------- 1) Python
%PY% -c "import sys; raise SystemExit(0 if sys.version_info>=(3,10) else 1)" >nul 2>&1 || goto :badpy
%PY% -c "import PySide6" >nul 2>&1 || goto :badsx

REM ---------------------------------------------------------------- 2) 核心绑定
REM 没构建过就现场构建：装了 maturin 走 maturin develop，失败（常见于没建虚拟环境）
REM 或没装 maturin 时走 cargo build --features extension-module 再拷进 bindings\。
%PY% -c "import psychdoc_ffi" >nul 2>&1 && goto :haveffi

where maturin >nul 2>&1
if not errorlevel 1 (
  echo [构建中] maturin develop（首次较慢，请稍候）...
  maturin develop --manifest-path core\crates\psychdoc-ffi\Cargo.toml >nul 2>&1
  %PY% -c "import psychdoc_ffi" >nul 2>&1 && goto :haveffi
)

where cargo >nul 2>&1 || goto :badffi
echo [构建中] cargo build -p psychdoc-ffi --features extension-module（首次较慢，请稍候）...
pushd core
cargo build -p psychdoc-ffi --features extension-module
popd
if errorlevel 1 goto :badffi
if not exist bindings mkdir bindings
for %%L in (core\target\debug\libpsychdoc_ffi.dll core\target\debug\libpsychdoc_ffi.dylib) do (
  if exist %%L (
    copy /y %%L bindings\psychdoc_ffi.pyd >nul
    copy /y %%L bindings\psychdoc_ffi.dll >nul
    goto :haveffi
  )
)
:badffi
echo [启动失败] 核心绑定 psychdoc_ffi 未构建，且构建失败。
echo            装其一即可： pip install maturin，或安装 Rust 工具链。
exit /b 1

:haveffi
REM ---------------------------------------------------------------- 3) 入口齐全性
REM 少一个入口就说明绑定与壳不同步，宁可启动失败也不要半残窗口。
%PY% -c "import psychdoc_ffi,sys; need=('check_bundle','review_batch','preview_fixback','commit_writeback','graph_both_directions','scan_queue','doc_card','startup_report'); miss=[n for n in need if not hasattr(psychdoc_ffi,n)]; sys.exit(1 if miss else 0)" >nul 2>&1 || goto :badentry

REM ---------------------------------------------------------------- 4) 规则集
REM 规则集读不到就**不启动**：没有规则时给出「全部通过」比不给结论更危险。
if not exist rulesets\hospital.yaml (
  echo [启动失败] 读不到规则集 rulesets\hospital.yaml（E_RULESET_INVALID）。
  echo            系统不会在没有规则的情况下给出「全部通过」的结论。
  exit /b 2
)

REM ---------------------------------------------------------------- 5) 自检模式
if "%~1"=="--selfcheck" (
  %PY% -c "from desktop.main import preflight; ok,msg,code=preflight(); print(msg); raise SystemExit(0 if ok else code)"
  exit /b %errorlevel%
)

REM ---------------------------------------------------------------- 6) 凭据提示
REM 写回凭据缺失**不拦启动**：只想看本地文书包清单的人不必先配密钥。
set "MISSING="
for %%K in (EMR_BASE_URL EMR_CLIENT_ID EMR_CLIENT_SECRET EMR_TENANT_ID WECOM_WEBHOOK_URL) do (
  if "%%%K%%"=="" set "MISSING=!MISSING! %%K"
)
if not "!MISSING!"=="" (
  echo [提示] 未配置：!MISSING!
  echo        本次可照常审查本地文书包；写回与缺件通知会返回 E_CFG_MISSING，系统不会静默降级。
)

echo [启动中] 精神科住院文书审查系统 ...
%PY% -c "import sys; from desktop.main import main; sys.exit(main(sys.argv[1:]))" %*
exit /b %errorlevel%

:badpy
echo [启动失败] Python 版本过低，需要 3.10 及以上。
exit /b 1
:badsx
echo [启动失败] 未安装 PySide6，请先执行： pip install -e .
exit /b 1
:badentry
echo [启动失败] 核心绑定缺少入口，绑定与桌面壳版本不同步，请重新构建。
exit /b 1