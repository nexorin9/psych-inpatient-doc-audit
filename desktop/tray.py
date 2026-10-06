"""系统托盘常驻：待提交角标、变更事件触发增量重跑、干净退出。

这一模块把「常驻工具」的两件事补齐，其余都已在主窗口里：

1. **角标随时可答** —— 托盘图标上的数字 = 队列里**还没收齐的文书包份数**
   （本病区待提交且不达标）。口径由 `badge_from_entries` 一处算，
   窗口标题、托盘 tooltip、角标三处**读同一个函数**。
   角标与窗口内状态分头各算一遍，是常驻工具最典型的漂移：护士长看着
   托盘说「还剩 3 份」，点进窗口却看到「缺件 11」，两边都得解释一遍。
2. **变更来了自己动** —— EMR 侧有变更就**只重跑受影响的住院别名**，
   底栏刷新「重跑 N 件 / 缓存跳过 M 件 / 规则集短哈希」，其中跳过数
   只来自核心返回的真实缓存命中，不估算。

## 变更事件的入口只有一个

`TrayController.notify_change(aliases)` 是**唯一**的变更入口，
三条路都汇到它：

- **导出目录轮询**（本轮交付）：`ExportRootWatcher` 按可配间隔
  （`PSYCHDOC_POLL_SECONDS`）比对已载入导出根的文件指纹，只把**真的变了**
  的文件反查成住院别名交给它。轮询间隔默认 0（不轮询）——没人要求它
  盯着磁盘时，就不该在护士长审一份包的时候把 CPU 拉起来。
- **EMR 变更推送 / 轮询**（`EMR_BASE_URL` 等凭据就位后接）：院内文档服务的
  增量读拿到变更件后调**同一个** `notify_change`，不另开一条路。
  壳不自己发外站轮询：凭据、端点与退避策略属于 `psychdoc-fhir` 的地盘，
  壳只消费结果。
- **托盘菜单「立即重审」**：等价于一次全量别名变更事件，走同一条合并队列。

## 三条不能破的纪律

- **不并发重入**。正在重跑时来的变更进**下一轮队列**（`ChangeCoalescer`），
  等本轮跑完再起下一轮；绝不并发起第二个 `review_batch`——那会让底栏的
  「已审 3/12」乱跳，也会把同一份包扫两遍。
- **连续变更合并成一次**。一轮重跑途中连来 5 条变更，合并成**一轮**重跑
  （别名去重、保序），而不是 5 轮。逐条响应会让护士长看着窗口反复闪。
- **退出前落盘**。队列进度由主窗口在每轮审查收尾时落库；托盘另落一份
  **轮询游标**（上次看到的文件指纹），下次启动不把整目录当成「全变了」。
"""

from __future__ import annotations

import json
import os
from pathlib import Path
from typing import Any, Iterable, Sequence

from PySide6.QtCore import QObject, Qt, QTimer, Signal
from PySide6.QtGui import QAction, QColor, QFont, QIcon, QPainter, QPixmap
from PySide6.QtWidgets import QMenu, QSystemTrayIcon

from . import DISPLAY_NAME, core_bridge, theme

#: 轮询间隔默认值：**不轮询**。写 0 是刻意的——常驻不等于一直干活，
#: 没人要求它盯盘时不该在护士长审包时把 CPU 拉起来。
DEFAULT_POLL_SECONDS = 0

#: 托盘角标数字大于这个值时的展示口径（避免 tooltip 里塞一大串）。
_BADGE_MAX = 99


# ---------------------------------------------------------------------------
# 纯函数：角标口径
# ---------------------------------------------------------------------------


def badge_from_entries(entries: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """队列条目 → 待提交角标。

    **口径**：队列里 ``missing_count > 0`` 的住院条目数 = 还没收齐的文书包
    份数。已经收齐的不进角标（它们没有「待提交」这回事）。

    与底栏那行的「缺件 N」**不是同一个数**：底栏数的是**件**（这一病区
    一共还差多少份文书），角标数的是**份**（还有几次住院的包没收齐）。
    11 个缺件可能只落在 3 次住院上——护士长先认「还有 3 份包没收」，
    再进窗口看「这 3 份里差 11 件」。两个数字混用会让人以为少算了。
    """
    entries = list(entries or [])
    pending = [
        e
        for e in entries
        if int(e.get("missing_count") or 0) > 0
    ]
    missing_items = sum(int(e.get("missing_count") or 0) for e in pending)
    count = len(pending)
    aliases = [e.get("encounter_alias", "") for e in pending]
    if count == 0:
        basis_zh = "本病区待提交的文书包已全部收齐。"
    elif count > _BADGE_MAX:
        basis_zh = f"本病区还有 {count} 份文书包没收齐，共差 {missing_items} 件。"
    else:
        basis_zh = f"本病区还有 {count} 份文书包没收齐，共差 {missing_items} 件：{'、'.join(a for a in aliases if a)}"
    return {
        "count": count,
        "missing_items": missing_items,
        "aliases": aliases,
        "basis_zh": basis_zh,
    }


def badge_text(count: int) -> str:
    """角标数字 → 图标上画的那个字符串。0 就是空串（没有角标）。"""
    if count <= 0:
        return ""
    if count > _BADGE_MAX:
        return "99+"
    return str(count)


# ---------------------------------------------------------------------------
# 纯函数：变更合并（不重入）
# ---------------------------------------------------------------------------


class ChangeCoalescer:
    """变更事件的合并与排队——**不并发重入**那条纪律的落点。

    用法固定三步：来变更调 `submit`，跑完调 `finish`。

    - 空闲时来的第一��变更**立即**返回本轮要跑��别名（`list[str]`）；
    - 正在跑时来的变更一律进**下一轮队列**，本轮不变；
    - 连续多条变更在同一轮里**合并成一次**（别名去重、保序）；
    - `finish()` 返回下一轮要跑的别名，没有待跑时返回 `None`。

    全部是纯状态机：不起线程、不碰 Qt、不读时钟，所以「连来 5 条只跑一轮」
    这条能用 pytest 直接断言，不用等真事件。
    """

    def __init__(self) -> None:
        self._running = False
        self._pending: list[str] = []
        self._next_round: list[str] = []
        #: 已起的轮数 / 被合并掉的变更条数——只作运行摘要，不参与判定。
        self._rounds_started = 0
        self._rounds_merged = 0

    # -- 状态 ---------------------------------------------------------------

    @property
    def running(self) -> bool:
        """当前是否有重跑正在进行。"""
        return self._running

    @property
    def pending(self) -> tuple[str, ...]:
        """本轮已提交但还没跑完的别名。"""
        return tuple(self._pending)

    @property
    def queued(self) -> tuple[str, ...]:
        """排在下一轮、还没轮的别名。"""
        return tuple(self._next_round)

    def stats(self) -> dict[str, Any]:
        """运行摘要用的小计（不给界面编数字，给界面读真状态）。"""
        return {
            "running": self._running,
            "pending": list(self._pending),
            "queued": list(self._next_round),
            "rounds_started": self._rounds_started,
            "rounds_merged": self._rounds_merged,
        }

    # -- 变更入口 -----------------------------------------------------------

    def submit(self, aliases: Iterable[str]) -> list[str] | None:
        """来了变更。返回「本轮该跑哪些别名」，`None` 表示进下一轮队列。

        空闲时把待跑队列清空并返回它；正在跑时只入队，返回 `None`。
        """
        incoming = [a for a in dict.fromkeys(aliases) if a]
        if not incoming:
            return None
        if not self._running:
            self._pending = list(incoming)
            self._rounds_started += 1
            return list(self._pending)
        self._next_round = list(dict.fromkeys(self._next_round + incoming))
        self._rounds_merged += 1
        return None

    def finish(self) -> list[str] | None:
        """本轮跑完。返回下一轮该跑的别名；没有待跑时返回 `None`。

        下一轮**不立刻开**：返回列表给调用方，由调用方在自己的线程里起 job。
        壳拿到 `None` 时应当把 `running` 落回 `False`。
        """
        if self._next_round:
            nxt = self._next_round
            self._next_round = []
            self._pending = list(nxt)
            self._rounds_started += 1
            return list(nxt)
        self._running = False
        self._pending = []
        return None

    def mark_running(self) -> None:
        """调用方确认已把 job 丢进线程池后置位（防止 finish 提前解锁）。"""
        self._running = True


# ---------------------------------------------------------------------------
# 纯函数：导出目录指纹
# ---------------------------------------------------------------------------


def file_fingerprints(paths: Iterable[str]) -> dict[str, list[float]]:
    """一组文件路径 → ``{路径: [mtime, size]}`` 指纹。

    打不开的文件**不静默跳过**：记 ``[0.0, -1]``，让下一轮仍然看见它变了。
    病区导出目录里经常有正在写的半截文件，真出问题时「那个文件打不开」
    本身就是答案。
    """
    out: dict[str, list[float]] = {}
    for p in paths:
        try:
            st = os.stat(p)
        except OSError:
            out[p] = [0.0, -1.0]
            continue
        out[p] = [round(st.st_mtime, 3), float(st.st_size)]
    return out


def iter_export_files(roots: Iterable[str]) -> list[str]:
    """展开导出根 → 全部 json 导出文件（递归）。

    只收 `.json`：导出目录里混着 pdf 扫描件与 xlsx 台账，那些是
    `scan_queue` 记进「已跳过」的东西，不该占着轮询的指纹表。
    """
    out: list[str] = []
    for root in roots:
        p = Path(root)
        if p.is_file() and p.suffix.lower() == ".json":
            out.append(str(p))
        elif p.is_dir():
            for f in sorted(p.rglob("*.json")):
                if f.is_file():
                    out.append(str(f))
    return out


def changed_files(
    before: dict[str, list[float]], after: dict[str, list[float]]
) -> list[str]:
    """两份指纹之差 → 变了（或新增 / 消失）的文件，保持稳定顺序。

    顺序按路径名排：同一批变更两次轮询给出的顺序必须一致，
    审计导出与截图才 diff 得动。
    """
    keys = set(before) | set(after)
    return sorted(k for k in keys if before.get(k) != after.get(k))


def aliases_of_changed_files(
    entries: Sequence[dict[str, Any]], files: Iterable[str]
) -> list[str]:
    """变了哪些文件 → 受影响的住院别名（按左栏显示顺序、去重）。

    反查走队列条目里核心给的 `source_files`（**不**在壳里按 doc_id 猜别名
    —— 那正是 Task 4 明令禁止的「按名字配对」）。

    比对按**文件名**做：核心给的 `source_files` 是相对导出根的文件名
    （如 `ward-02-restraint-set.json`），而轮询拿到的是绝对路径，两者
    只有 basename 相同。文件对不上任何一条队列条目时返回空列表：
    这份新文件属于哪次住院只有核心知道，壳猜出来的别名会让错误的住院
    被重跑一遍，而正确的那个反而没跑——那比漏跑一次更难解释。

    basename 撞名（两个病区各有 `bundle.json`）时本函数会**多**报一个别名：
    多跑一遍的代价远小于漏跑，而真要消除它得让核心给全路径，那是
    队列 schema 的一次改动，不该在这里猜。
    """
    changed_names = {Path(f).name for f in files}
    if not changed_names:
        return []
    out: list[str] = []
    for e in entries or []:
        src_names = {Path(s).name for s in (e.get("source_files") or [])}
        alias = e.get("encounter_alias")
        if alias and not changed_names.isdisjoint(src_names):
            out.append(alias)
    return out


# ---------------------------------------------------------------------------
# 游标落盘
# ---------------------------------------------------------------------------


def cursor_path() -> Path:
    """轮询游标的落盘位置（用户数据目录，不落项目子目录）。

    与 sqlite 队列库同目录：两者都是「本机的状态」，都不该被提交进仓库。
    """
    return Path(core_bridge.default_store_path()).parent / "tray-cursor.json"


def save_cursor(fingerprints: dict[str, list[float]]) -> Path | None:
    """落盘游标。**不**吞异常成「假装存了」——落盘失败要看得见。

    返回落盘路径；目录不可写时返回 `None` 并由调用方把失败显式报出来。
    """
    path = cursor_path()
    try:
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(
            json.dumps(fingerprints, ensure_ascii=False, sort_keys=True),
            encoding="utf-8",
        )
    except OSError:
        return None
    return path


def load_cursor() -> dict[str, list[float]]:
    """读回游标。读不到 / 坏了都当「没有游标」——第一轮会全量扫一遍。

    这里**不**报错打断启动：游标只是省一次全量比对，
    丢一个游标的代价是首轮多跑一遍，不是不能启动。
    """
    path = cursor_path()
    try:
        raw = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return {}
    if not isinstance(raw, dict):
        return {}
    return {k: [float(v[0]), float(v[1])] for k, v in raw.items() if isinstance(v, list) and len(v) == 2}


# ---------------------------------------------------------------------------
# Qt 部分：托盘图标 + 轮询
# ---------------------------------------------------------------------------


class TrayIcon(QSystemTrayIcon):
    """托盘图标 + 右键菜单（打开主窗口 / 立即重审 / 退出）。

    菜单项走 `Signal` 抛给 `TrayController`，图标本身不碰业务：
    托盘是护士长唯一能看见的常驻表面，它该做的只是「报数」与「接三个动作」。
    """

    openRequested = Signal()
    reviewNowRequested = Signal()
    quitRequested = Signal()

    def __init__(self, parent: QObject | None = None) -> None:
        super().__init__(parent)
        self.setToolTip(DISPLAY_NAME)

        menu = QMenu()
        self._open_action = QAction("打开主窗口", menu)
        self._open_action.triggered.connect(self.openRequested.emit)
        menu.addAction(self._open_action)

        self._review_action = QAction("立即重审", menu)
        self._review_action.triggered.connect(self.reviewNowRequested.emit)
        menu.addAction(self._review_action)

        menu.addSeparator()
        self._quit_action = QAction("退出", menu)
        self._quit_action.triggered.connect(self.quitRequested.emit)
        menu.addAction(self._quit_action)

        self.setContextMenu(menu)
        self._menu = menu

    def menu(self) -> QMenu:
        """右键菜单（供测试断言三个动作都在）。"""
        return self._menu

    def apply_badge(self, count: int, tooltip: str) -> None:
        """刷角标：图标数字 + tooltip 一句话口径。

        Qt 没有跨平台的原生 badge API（macOS 的角标要 AppKit、Windows 的
        要 Shell 扩展），所以这里走 Qt 自己能画的两处：**图标上叠数字**
        （QPainter 画在 QPixmap 上）与 **tooltip 给一句人话**。tooltip
        是唯一一定送达用户眼睛的一处，数字之外必须把口径写清楚。
        """
        self.setIcon(render_icon(count))
        self.setToolTip(f"{DISPLAY_NAME} — {tooltip}")


def render_icon(count: int) -> QIcon:
    """角标数 → 托盘图标（达标绿 / 缺件红 + 右上角数字）。

    只用 Qt 原生绘制能力（QPixmap + QPainter），不引第三方图标库——
    病区机器上装不了那些，而常驻图标必须能画出来。
    """
    danger = count > 0
    pix = QPixmap(64, 64)
    pix.fill(QColor(0, 0, 0, 0))
    p = QPainter(pix)
    p.setRenderHint(QPainter.RenderHint.Antialiasing, True)
    p.setBrush(QColor(theme.DANGER if danger else theme.OK))
    p.setPen(QColor("#FFFFFF"))
    p.drawEllipse(6, 6, 52, 52)
    text = badge_text(count)
    if text:
        p.setFont(QFont(theme.MONO_FAMILY.split(",")[0], 18, QFont.Weight.Bold))
        p.drawText(pix.rect(), Qt.AlignmentFlag.AlignCenter, text)
    else:
        p.setFont(QFont(theme.MONO_FAMILY.split(",")[0], 20, QFont.Weight.Bold))
        p.drawText(pix.rect(), Qt.AlignmentFlag.AlignCenter, "✓")
    p.end()
    return QIcon(pix)


class TrayController(QObject):
    """把托盘图标、轮询与变更合并接成一条链。

    唯一的变更入口是 `notify_change`；三条路（目录轮询 / EMR 推送 /
    菜单「立即重审」）都汇到它，因此「连来 5 条只跑一轮」只有一处实现。
    """

    #: 本轮重跑跑完（主窗口每审完一轮就发一个）。带一个可读摘要。
    runFinished = Signal(object)
    #: 变更被合并进了下一轮（不是丢弃）。
    changeQueued = Signal(object)

    def __init__(self, window: Any, poll_seconds: int = DEFAULT_POLL_SECONDS) -> None:
        # 不把 `window` 当 QObject 父：它只是个「提供四个口子」的协作者，
        # 强制成父子会让测试替身（非 QObject）直接构造不出来。
        # 生命周期由 `start` / `shutdown` 显式管。
        super().__init__()
        self._window = window
        self.coalescer = ChangeCoalescer()
        self.icon = TrayIcon(self)
        self.icon.openRequested.connect(self._on_open_requested)
        self.icon.reviewNowRequested.connect(self._on_review_requested)
        self.icon.quitRequested.connect(self.shutdown)

        self._poll_seconds = max(0, int(poll_seconds))
        self._timer = QTimer(self)
        self._timer.setInterval(max(1000, self._poll_seconds * 1000))
        self._timer.timeout.connect(self._poll_once)
        self._fingerprints: dict[str, list[float]] = {}

    # -- 生命周期 -----------------------------------------------------------

    def start(self) -> None:
        """进入常驻：托盘可见；间隔 > 0 时起轮询。

        首轮把当前导出根的指纹**先记下来**再开始计时——
        否则启动那一刻的第一次比对会把整个目录都当成「变了」，
        护士长刚打开窗口就看着它自动重跑一遍没动过的包。
        """
        self.icon.show()
        self._fingerprints = file_fingerprints(iter_export_files(self._roots()))
        if self._poll_seconds > 0:
            self._timer.start()
        self.refresh_badge()

    def shutdown(self) -> None:
        """退出：停轮询、落盘游标、通知窗口落库、隐藏托盘。"""
        self._timer.stop()
        save_cursor(self._fingerprints)
        self.icon.hide()
        try:
            self._window.persist_before_exit()
        except Exception:  # noqa: BLE001 - 退出路径不允许抛
            pass
        from PySide6.QtWidgets import QApplication

        app = QApplication.instance()
        if app is not None:
            app.quit()

    @property
    def poll_seconds(self) -> int:
        """轮询间隔（秒）。0 = 不轮询。"""
        return self._poll_seconds

    def set_poll_seconds(self, seconds: int) -> None:
        """运行时改间隔（配置热改；<=0 立刻停轮询）。"""
        self._poll_seconds = max(0, int(seconds))
        self._timer.setInterval(max(1000, self._poll_seconds * 1000))
        if self._poll_seconds > 0:
            self._timer.start()
        else:
            self._timer.stop()

    # -- 角标 ---------------------------------------------------------------

    def refresh_badge(self) -> dict[str, Any]:
        """按**主窗口队列**重算角标，刷新图标 / tooltip / 窗口标题。

        数据源只有一个：``window.encounter_rows()``。角标自己另扫一遍目录
        的话，「托盘说 3 份、窗口说 5 份」几乎是必然发生的漂移。
        """
        state = badge_from_entries(self._window.encounter_rows())
        self.icon.apply_badge(state["count"], state["basis_zh"])
        try:
            self._window.set_title_suffix(state)
        except Exception:  # noqa: BLE001 - 标题刷新失败不该影响角标
            pass
        return state

    # -- 变更入口 -----------------------------------------------------------

    def notify_change(self, aliases: Iterable[str]) -> list[str] | None:
        """唯一的变更入口。返回本轮该跑的别名，`None` 表示进下一轮。

        调用方（目录轮询 / EMR 推送 / 菜单）都调它，因此「合并 + 不重入」
        只有这一份实现，三条路不会再各写一遍。
        """
        now = self.coalescer.submit(aliases)
        if now is None:
            self.changeQueued.emit(self.coalescer.stats())
            return None
        self._start_round(now)
        return now

    def _start_round(self, aliases: list[str]) -> None:
        self.coalescer.mark_running()
        self._window.request_rerun(aliases, on_done=self._on_round_done)

    def _on_round_done(self, payload: dict[str, Any]) -> None:
        """一轮跑完。有下一轮就立刻接着起，没有就解锁并刷新角标。"""
        nxt = self.coalescer.finish()
        self.runFinished.emit(payload)
        if nxt is None:
            self.refresh_badge()
        else:
            self._start_round(nxt)

    # -- 轮询 ---------------------------------------------------------------

    def _roots(self) -> list[str]:
        return list(getattr(self._window, "input_roots", []) or [])

    def poll_now(self) -> list[str]:
        """跑一轮比对（不等定时器）。返回这一轮受影响���住院别名。

        定时器只是「什么时候调它」；比对本身是纯函数式的
        「旧指纹 vs 新指纹」，所以测试可以直接调 `poll_now` 而不必等。
        """
        current = file_fingerprints(iter_export_files(self._roots()))
        changed = changed_files(self._fingerprints, current)
        self._fingerprints = current
        if not changed:
            return []
        aliases = aliases_of_changed_files(self._window.encounter_rows(), changed)
        if not aliases:
            # 文件变了但对不上任何队列条目（新文件 / 与队列无关的文件）：
            # **不**把整个队列当成变了去全量重跑——那会让每一次无关的文件
            # 落地都触发一次全病区重审，护士长会直接把轮询关掉。
            return []
        self.notify_change(aliases)
        return aliases

    def _poll_once(self) -> None:
        self.poll_now()

    # -- 菜单动作 -----------------------------------------------------------

    def _on_open_requested(self) -> None:
        self._window.show_from_tray()

    def _on_review_requested(self) -> None:
        """菜单「立即重审」= 一次全量别名变更事件，走同一条合并队列。"""
        aliases = [e["encounter_alias"] for e in self._window.encounter_rows() if e.get("encounter_alias")]
        if aliases:
            self.notify_change(aliases)


def resolve_poll_seconds(explicit: int | None = None) -> int:
    """轮询间隔：命令行 > 环境变量 > 默认 0（不轮询）。

    写死成一个正数会让常驻工具变成后台常驻扫盘；间隔必须由部署方明确开。
    """
    if explicit is not None:
        return max(0, int(explicit))
    raw = os.environ.get("PSYCHDOC_POLL_SECONDS", "").strip()
    if not raw:
        return DEFAULT_POLL_SECONDS
    try:
        return max(0, int(raw))
    except ValueError:
        return DEFAULT_POLL_SECONDS


__all__ = [
    "ChangeCoalescer",
    "DEFAULT_POLL_SECONDS",
    "TrayController",
    "TrayIcon",
    "aliases_of_changed_files",
    "badge_from_entries",
    "badge_text",
    "changed_files",
    "cursor_path",
    "file_fingerprints",
    "iter_export_files",
    "load_cursor",
    "render_icon",
    "resolve_poll_seconds",
    "save_cursor",
]