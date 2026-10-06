"""桌面壳入口：自检 → 建窗口 → 起事件循环。

启动顺序刻意是「先自检、后建窗」：

- 核心绑定没构建 → 中文提示 + 非 0 退出，不给一个点开就报错的空窗口；
- 规则集读不到 → 同上（**不**降级为「无规则 = 全部通过」）；
- 写回凭据缺失 → **弹窗告知但不拦启动**：护士长仍可以只做本地审查，
  只是点提交时会拿到 `E_CFG_MISSING`。把「启动」和「能写回」绑死在一起，
  会让只想看清单的人也被迫配一套密钥。

退出码沿用 CLI 的契约（0 正常 / 2 配置缺失 / 3 依赖不可达），
方便信息科在服务器上用 `run.sh` 跑无人值守自检。
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parent.parent

#: CLI 与桌面壳共用的退出码契约（Task 36 钉死）。
EXIT_OK = 0
EXIT_FINDINGS = 1
EXIT_CONFIG = 2
EXIT_UNREACHABLE = 3


def _fail(message: str, code: int = EXIT_CONFIG) -> int:
    sys.stderr.write(f"[启动失败] {message}\n")
    return code


def preflight() -> tuple[bool, str, int]:
    """启动自检。返回 `(是否通过, 提示文案, 退出码)`。

    纯函数：不建 `QApplication`、不建窗口，方便 pytest 直接断言。
    """
    try:
        from . import core_bridge
    except Exception as exc:  # noqa: BLE001 - 任何导入失败都要给中文提示
        return False, f"核心绑定不可用：{exc}", EXIT_CONFIG

    ruleset = core_bridge.default_ruleset_path()
    if not ruleset.is_file():
        return (
            False,
            f"E_RULESET_INVALID：读不到规则集 {ruleset}。\n"
            "           系统不会在没有规则的情况下给出「全部通过」的结论。",
            EXIT_CONFIG,
        )
    try:
        ruleset.read_text(encoding="utf-8")
    except OSError as exc:
        return False, f"E_RULESET_INVALID：规则集读取失败 — {exc}", EXIT_CONFIG

    return True, "", EXIT_OK


def writeback_hint() -> str:
    """写回 / 通知凭据缺失时的提示。**不拦启动**，只告知。

    缺项清单取自核心的 `startup_report`（Rust 侧判定），不在壳里重算一遍：
    两份判定迟早漂移，而漂移的方向通常是壳里那份更宽松。
    绑定不可用时退回 `missing_required_env`——提示可以粗糙，
    但不能因为绑定没建好就一个字都不说。
    """
    from . import core_bridge

    try:
        report = core_bridge.startup_report()
    except Exception:  # noqa: BLE001 - 绑定没建好时退回纯 Python 判定
        missing = core_bridge.missing_required_env()
        bad = []
    else:
        missing = list(report.get("missing", []))
        bad = [f"{item['key']}（{item['detail']}）" for item in report.get("invalid", [])]

    lines = []
    if missing:
        lines.append("、".join(missing) + " 未配置")
    if bad:
        lines.append("；".join(bad) + " 形态不合法")
    if not lines:
        return ""
    return (
        "提示：院内对接所需的 " + "，".join(lines) + "。\n"
        "     本次可以照常审查本地文书包；点「提交写回」或发缺件提醒时会返回 "
        "E_CFG_MISSING，系统不会静默降级。"
    )


def _tray_available() -> bool:
    """本机有没有可用的系统托盘。

    精简版 Windows Server、多数 SSH 会话、部分 Linux 桌面会话都**没有**
    托盘。那里 `QSystemTrayIcon.show()` 是个静默的空操作：窗口能起、
    托盘却什么都没发生——护士长会以为程序崩了。起不来就明说。
    """
    try:
        from PySide6.QtWidgets import QApplication, QSystemTrayIcon
    except ImportError:
        return False
    if QApplication.instance() is None:
        return False
    return bool(QSystemTrayIcon.isSystemTrayAvailable())


def main(argv: list[str] | None = None) -> int:
    """建窗口并进入事件循环。返回进程退出码。"""
    argv = list(sys.argv[1:] if argv is None else argv)

    ok, message, code = preflight()
    if not ok:
        return _fail(message, code)

    # 允许 `python -m desktop.main <导出目录>` 直接载入一份包，
    # 免得信息科为了看一眼清单还得先点一次文件对话框。
    preset = argv[0] if argv else os.environ.get("PSYCHDOC_INPUT", "")

    from PySide6.QtWidgets import QApplication, QMessageBox

    from . import core_bridge, theme
    from .main_window import MainWindow
    from .tray import TrayController, resolve_poll_seconds

    app = QApplication(sys.argv[:1])
    app.setApplicationName("精神科住院文书审查系统")
    app.setStyleSheet(theme.QSS)

    window = MainWindow()
    window.show()

    # 常驻（Task 35）：托盘图标 + 变更触发的增量重跑。
    #
    # 托盘建不起来（没有系统托盘的精简环境）**不**拦启动——那台机器上
    # 本来就没人会去看托盘，让整个程序起不来才是真的坏。这里显式
    # 警告一次，窗口照常用。
    tray = None
    if _tray_available():
        tray = TrayController(window, poll_seconds=resolve_poll_seconds())
        window.tray = tray
        tray.start()
    else:
        sys.stderr.write(
            "[提示] 当前环境没有可用的系统托盘，仅以窗口模式运行"
            "（角标、变更触发增量重跑不可用）。\n"
        )

    hint = writeback_hint()
    if hint:
        QMessageBox.information(window, "写回未启用", hint)

    if preset and Path(preset).exists():
        window.load_path(preset)

    return app.exec()


if __name__ == "__main__":  # pragma: no cover
    raise SystemExit(main())
