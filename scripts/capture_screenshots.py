"""采集**真实运行**截图到 `docs/screenshots/`（Task 14）。

四张图全部是本脚本真跑出来的，不是设计稿：

1. `01-起始态.png` —— 刚起窗口、还没拖文书包时的空壳 + 拖放区；
2. `02-问题清单与段落高亮.png` —— 载入含 4 类违规的脱敏文书包后，左栏住院条目、
   右栏问题清单、点开一条后的违规段落高亮与依据行；
3. `03-归档核齐依赖图.png` —— 切到「归档核齐」标签的依赖图 + 阻塞边 + 底栏运行摘要；
4. `04-cli-check.png` —— `psychdoc check` 子命令的**真实**终端回显渲染成图。

抓图用 `QScreen.grabWindow(winId)`：抓的是这个窗口自己的绘制结果，
因此窗口标题栏不出现（那是系统装饰），但控件一像素不差——
比 `QWidget.grab()` 多走一步「窗口已映射到屏幕」的真实路径。

用法（在项目根 `psych-inpatient-doc-audit/` 下）：

```bash
python3 scripts/capture_screenshots.py
```

不需要显示器的机器上也能出图（`QT_QPA_PLATFORM=offscreen` 时
`grabWindow` 拿到的是离屏渲染结果，同样是真实控件树）。
"""

from __future__ import annotations

import json
import os
import subprocess
import sys
from pathlib import Path

# 截图默认走 offscreen：真实显示器上抓屏会把系统弹窗（macOS 的
# 「Terminal 请求屏幕录制」授权框、输入法候选窗）一起框进来，
# 那不是产品界面。offscreen 下 `grabWindow` 抓的仍是**同一个窗口**
# 经 Qt 真实布局与 QSS 绘制出来的结果，只是没有系统装饰层。
# 想要带窗口标题栏的本机截图：`QT_QPA_PLATFORM=cocoa python3 scripts/capture_screenshots.py`
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "docs" / "screenshots"
FIXTURES = ROOT / "fixtures"
RULESET = ROOT / "rulesets" / "hospital.yaml"

#: 工作目录：截图期间所有 sqlite / 缓存都落到这里，不污染用户数据目录。
WORK = ROOT / ".shot-work"


def _pump(app, predicate, timeout_s: float = 60.0) -> bool:
    """转事件循环直到 `predicate()` 为真。

    核心调用全在 `QThreadPool` 上跑，界面刷新靠信号回主线程，
    所以截图前必须真的把事件循环转起来让 job 收完——不等就是空壳窗口。
    """
    import time

    deadline = time.monotonic() + timeout_s
    while time.monotonic() < deadline:
        app.processEvents()
        if predicate():
            app.processEvents()
            return True
        time.sleep(0.02)
    return False


def _grab(window, screen, name: str, note: str) -> Path:
    """抓窗口本身并落盘。返回落盘路径。"""
    window.repaint()
    # 传 `winId()` 而不是 widget：`grabWindow` 只认窗口句柄，
    # 传 widget 会走到「按坐标抓整屏」的分支，截回来的是桌面。
    screen.grabWindow(int(window.winId())).save(str(OUT / name))
    path = OUT / name
    print(f"  抓到 {name}（{path.stat().st_size} 字节）— {note}")
    return path


def _synthetic_fixtures() -> Path:
    """拼一份「一次装下 4 类违规」的输入目录。

    直接吃 `fixtures/` 整个目录也行，但那是 20 份包、几百件，
    截图上左栏会挤成一小条，反而看不出「左栏条目 + 右栏清单」的关系。
    这里取三份覆盖不同违规面的：跨文书时间序倒挂（ward-11）、
    件-字段类（ward-08）、归档核齐缺件（ward-02 去掉知情同意）。
    """
    WORK.mkdir(parents=True, exist_ok=True)
    stage = WORK / "input"
    stage.mkdir(exist_ok=True)

    for name in ("ward-11-cross-doc-violations.json", "ward-08-field-violations.json"):
        (stage / name).write_text(
            (FIXTURES / name).read_text(encoding="utf-8"), encoding="utf-8"
        )

    # 归档核齐那一屏要看得见「真缺件 + 连带阻塞」，所以拿一份抽掉
    # 约束知情同意的成套包：约束记录与解除记录会各欠一天、连带卡住下游。
    doc = json.loads((FIXTURES / "ward-02-restraint-set.json").read_text(encoding="utf-8"))
    doc["entry"] = [
        e
        for e in doc["entry"]
        if not (
            e["resource"].get("resourceType") == "Composition"
            and any(
                c.get("code") == "restraint_consent"
                for c in e["resource"].get("type", {}).get("coding", [])
            )
        )
    ]
    (stage / "ward-B-001-missing-consent.json").write_text(
        json.dumps(doc, ensure_ascii=False, indent=2), encoding="utf-8"
    )
    return stage


def capture_window_states() -> list[Path]:
    """抓三态窗口。返回落盘路径列表。"""
    from PySide6.QtCore import Qt
    from PySide6.QtGui import QGuiApplication
    from PySide6.QtWidgets import QApplication

    from desktop import core_bridge, theme
    from desktop.main_window import MainWindow
    from desktop.tray import TrayController

    app = QApplication.instance() or QApplication(sys.argv[:1])
    app.setApplicationName("精神科住院文书审查系统")
    app.setStyleSheet(theme.QSS)

    screen = QGuiApplication.primaryScreen()
    stage = _synthetic_fixtures()

    window = MainWindow()
    window.resize(1280, 820)
    window.show()
    window.raise_()
    app.processEvents()

    shots: list[Path] = []

    # ---- ① 起始态：空窗口 + 拖放区 ------------------------------------
    _pump(app, lambda: window.isVisible())
    shots.append(_grab(window, screen, "01-起始态.png", "刚起窗口、未载入文书包"))

    # ---- 载入文书包（走真实的拖放汇入路径 load_paths）------------------
    window.load_paths([str(stage)])
    ok = _pump(
        app,
        lambda: not window._jobs and window.encounter_list.count() > 0,
        timeout_s=180.0,
    )
    if not ok:
        raise SystemExit("扫描队列没跑完就截图——截到的会是空壳，拒绝出图")

    # ---- ② 首个可用瞬间：左栏条目 + 右栏清单 + 段落高亮 + 依据行 --------
    # 逐条点一遍左栏，挑**真有命中项**的那次住院：三份包里有一份是合规包，
    # 硬抓第一条会得到一张空清单的图——那不是「首个可用瞬间」，是没跑完。
    # 挑完再点回它，把违规段落高亮与依据行一并截进来。
    best_row, best_count = -1, -1
    for row in range(window.encounter_list.count()):
        window.encounter_list.setCurrentRow(row)
        if not _pump(
            app, lambda: not window._jobs and not window._reviewing, timeout_s=180.0
        ):
            raise SystemExit(f"第 {row} 条住院没审完就截图——截到的是空壳，拒绝出图")
        count = window.findings.list.count()
        if count > best_count:
            best_row, best_count = row, count
    if best_count <= 0:
        raise SystemExit("这批夹具一条都没命中规则，截不到问题清单")

    window.encounter_list.setCurrentRow(best_row)
    _pump(app, lambda: not window._jobs and not window._reviewing, timeout_s=180.0)
    window.findings.list.setCurrentRow(0)
    _pump(app, lambda: not window.findings._jobs, timeout_s=60.0)
    _pump(app, lambda: window.findings.diff.isVisible(), timeout_s=30.0)
    alias = (
        window.encounter_list.item(best_row).data(Qt.ItemDataRole.UserRole) or {}
    ).get("encounter_alias", "?")
    shots.append(
        _grab(
            window,
            screen,
            "02-问题清单与段落高亮.png",
            f"{alias}：左栏 {window.encounter_list.count()} 条住院 · 右栏 {best_count} 条问题",
        )
    )

    # ---- ③ 成形态：托盘角标 + 归档核齐依赖图 + 底栏运行摘要 -------------
    tray = TrayController(window, poll_seconds=999)
    window.tray = tray
    tray.refresh_badge()

    # 归档核齐要看**缺件**那一屏，所以切到缺了知情同意的那次住院。
    target = 0
    for row in range(window.encounter_list.count()):
        item = window.encounter_list.item(row)
        data = item.data(Qt.ItemDataRole.UserRole) or {}
        if data.get("encounter_alias") == "WARD-B-001":
            target = row
            break
    window.encounter_list.setCurrentRow(target)
    _pump(app, lambda: not window._jobs and not window._reviewing, timeout_s=180.0)
    window.tabs.setCurrentIndex(1)
    _pump(app, lambda: not window._jobs, timeout_s=60.0)
    _pump(app, lambda: "依赖" in window.archive_pane.basis.toPlainText(), timeout_s=30.0)
    shots.append(
        _grab(
            window,
            screen,
            "03-归档核齐依赖图.png",
            f"窗口标题「{window.windowTitle()}」· 底栏「{window.status_label.text()}」",
        )
    )

    tray.shutdown()
    return shots


def capture_cli_output() -> Path:
    """跑一次**真的** `psychdoc check`，把它的真实回显渲染成图。

    不重新「排版」一遍输出：命令怎么打就怎么截，退出码也印在图上。
    渲染成 PNG 而不是贴终端滚屏，是为了让 README 里能直接引用一张图。
    """
    binary = Path("core/target/debug/psychdoc")
    if not (ROOT / binary).is_file():
        binary = Path("core/target/release/psychdoc")
    if not (ROOT / binary).is_file():
        raise SystemExit("找不到 psychdoc 二进制，先 cd core && cargo build")

    # 用**相对路径**跑：绝对路径有 100 多字符，会把渲染出来的终端图
    # 拉成一条几千像素宽的横幅，缩到 README 里谁也读不清字。
    argv = [
        str(binary),
        "check",
        "fixtures/ward-11-cross-doc-violations.json",
        "--ruleset",
        "rulesets/hospital.yaml",
    ]
    proc = subprocess.run(argv, capture_output=True, text=True, cwd=ROOT)

    text = proc.stdout + proc.stderr
    lines = text.splitlines()
    # 全量太长就留头尾——截图要能一眼看完，完整回显在 logs/ 里。
    if len(lines) > 30:
        skipped = len(lines) - 26
        lines = lines[:24] + [f"…（省略 {skipped} 行，完整输出见 logs/task14-cli-check.log）"] + lines[-2:]

    body = ["$ " + " ".join(argv)] + ["  " + ln for ln in lines]
    body.append("")
    body.append(f"$ echo $?\n{proc.returncode}")

    from PySide6.QtGui import QColor, QFont, QFontMetrics, QImage, QPainter
    from PySide6.QtWidgets import QApplication

    app = QApplication.instance() or QApplication(sys.argv[:1])
    mono = QFont("Menlo")
    mono.setStyleHint(QFont.StyleHint.Monospace)
    mono.setPixelSize(13)
    app.setFont(mono)

    metrics = QFontMetrics(mono)
    # 回显里的绝对路径收窄成相对路径，并把超长行**折行**而不是截断——
    # 依据文案一截断，截图就恰好丢掉「为什么判它违规」那半句，
    # 那正是这张图要证明的东西。
    MAX_COLS = 150
    wrapped: list[str] = []
    for ln in body:
        clean = ln.replace(str(ROOT) + "/", "")
        while len(clean) > MAX_COLS:
            wrapped.append(clean[:MAX_COLS])
            clean = "　　" + clean[MAX_COLS:]
        wrapped.append(clean)

    width = max(metrics.horizontalAdvance(ln) for ln in wrapped) + 48
    line_h = metrics.height() + 4
    height = line_h * len(wrapped) + 54

    img = QImage(width, height, QImage.Format.Format_RGB32)
    img.fill(QColor("#0D1117"))
    painter = QPainter(img)
    # 标题栏：写清这条命令是真跑出来的，不是画上去的假终端。
    painter.fillRect(0, 0, width, 30, QColor("#161B22"))
    painter.setPen(QColor("#8B949E"))
    painter.drawText(16, 20, f"真实执行： {' '.join(argv)}（退出码 {proc.returncode}）")
    painter.setPen(QColor("#D8DEE4"))
    y = 30 + 24
    for ln in wrapped:
        painter.drawText(16, y, ln)
        y += line_h
    painter.end()

    path = OUT / "04-cli-check.png"
    img.save(str(path))
    print(f"  抓到 {path.name}（{path.stat().st_size} 字节）— 真实退出码 {proc.returncode}")
    (ROOT / ".." / "logs").mkdir(exist_ok=True)
    (ROOT / ".." / "logs" / "task14-cli-check.log").write_text(
        f"$ {' '.join(argv)}\n{text}\n退出码 {proc.returncode}\n", encoding="utf-8"
    )
    return path


def main() -> int:
    # 截图期间所有落库/缓存都指到工作目录，不写用户数据目录。
    os.environ.setdefault("PSYCHDOC_DATA_DIR", str(WORK / "data"))
    os.environ.setdefault("PSYCHDOC_CACHE", str(WORK / "cache.json"))
    OUT.mkdir(parents=True, exist_ok=True)

    print("采集真实运行截图（真实窗口 + 真实 CLI 回显）…")
    shots = capture_window_states()
    shots.append(capture_cli_output())

    print("\n落盘清单：")
    for p in shots:
        print(f"  {p.relative_to(ROOT)}  {p.stat().st_size} 字节")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())