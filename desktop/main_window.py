"""主窗口：双标签 + 左栏审查队列 + 底栏运行摘要。

版式对应 `docs/concepts/visual-target.md` 的「起始态」与「首个可用瞬间」：

- 「文书包审查」标签：左栏是**审查队列**（病区 + 住院别名 + 件数 +
  缺件徽标 + 已审角标），下方是「已跳过」清单；右栏是问题清单与文书卡片；
- 「归档核齐」标签：依赖图（四态配色 + 阻塞边加粗）、欠件天数与两个方向
  的问题清单、一键发缺件待办（实现见 `desktop/archive_view.py`）；
- 底栏常驻运行摘要：住院次数 / 件数 / 缺件数 / 规则集短哈希，
  批量审查时切成 `(已审 / 总数)` 真实进度。

## 这一版（Task 34）接的入口

归档核齐标签页走核心的 `archive_view`（一屏数据：节点四态 / 边 / 欠件天数 /
通知状态）与 `notify_missing`（一键发缺件待办，真实出站 + 发件箱幂等）。

## 队列扫描（Task 32）接的入口

拖放、菜单「打开文书包…」、「选择文件夹」、命令行预设路径
**四条入口汇到同一个 `load_paths`**，再由核心 `scan_queue` 扫描成队列。
让它们各走各的，早晚会出现「拖进来的能审、按钮选的扫不到」这种分叉。

## 托盘（Task 35）接的入口

本文件只暴露四个小口子给 `desktop/tray.py`：`encounter_rows`（队列真源）、
`input_roots`、`request_rerun`、`persist_before_exit`。托盘**自己**不重扫目录、
不重算缺件——两边各算一遍角标，迟早出现「托盘说 3 份、窗口说 5 份」。

## 仍然不在本文件

无 GUI 回归通道（Task 36）。分开的理由：把它和「窗口能不能起来」塞进同一轮，
会让「起不来」与「渲染不对」互相掩盖，真出问题时定位不到层。
"""

from __future__ import annotations

from datetime import datetime
from pathlib import Path
from typing import Any

from PySide6.QtCore import QSize, Qt, Signal
from PySide6.QtGui import QAction
from PySide6.QtWidgets import (
    QAbstractItemView,
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QMainWindow,
    QMessageBox,
    QPushButton,
    QSplitter,
    QStatusBar,
    QTabWidget,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)

from . import DISPLAY_NAME, core_bridge, doc_card, theme
from .archive_view import ArchivePane as ArchiveViewPane
from .archive_view import export_csv as export_archive_csv
from .workers import (
    ArchiveViewJob,
    CoreJob,
    DocCardJob,
    PreviewJob,
    run_core_job,
)


def _now_rfc3339() -> str:
    """当前时刻，带时区偏移。

    时刻由**壳**注入、核心不读系统时钟：队列落库的 `queue_id` 与
    `loaded_at` 都从这里来，同一份目录两次扫描要能给出可复现的落库记录，
    审计导出才 diff 得动。这里仍然取真实时间——界面要显示「已审 14:32」。
    """
    return datetime.now().astimezone().replace(microsecond=0).isoformat()


def _business_day() -> str:
    """业务日（`YYYY-MM-DD`），缺件提醒的幂等键用它当第三个分量。

    按**交班日**算而不是自然日：跨零点那一班的人不该在自己新的一天里
    又被同一件事追一次。这也是发件箱「同一天同一件只发一次」的口径。
    """
    return datetime.now().astimezone().strftime("%Y-%m-%d")


class DropZone(QWidget):
    """起始态的中央拖放区（同时也是拖放的落点）。

    拖放**不是唯一入口**：「选择文件夹」按钮与菜单「打开文书包…」走同一条
    `load_paths`，三条路汇到同一个扫描入口。把拖放当唯一入口的话，
    病区瘦机上的远控 / 部分虚拟机传不了拖拽，护士长就只能对着空窗口发呆。

    接受的输入：**一个或多个文件夹**、**一份或多份导出文件**，
    混着拖也行。文件夹会递归扫描（按院内文书类型字典过滤），
    打不开的文件逐条进「已跳过」清单——一个都不静默丢。
    """

    #: 拖进来时发的信号，参数是路径列表（文件夹与文件混在一起）。
    pathsDropped = Signal(list)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QVBoxLayout(self)
        layout.setAlignment(Qt.AlignmentFlag.AlignCenter)
        layout.setSpacing(12)

        self.hint = QLabel("把住院文书包拖进来 / 或点下面的按钮选文件夹")
        self.hint.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.hint.setStyleSheet(f"color: {theme.MUTED}; font-size: 15px;")
        layout.addWidget(self.hint)

        self.detail = QLabel(
            "支持一个 FHIR R4 Bundle 导出文件，或一个按住院别名分目录的导出目录；\n"
            "也可以一次拖几个病区的文件夹。文件夹里的 pdf / xlsx 等杂项会被列进「已跳过」，不会静默丢掉。"
        )
        self.detail.setAlignment(Qt.AlignmentFlag.AlignCenter)
        self.detail.setStyleSheet(f"color: {theme.MUTED}; font-size: 12px;")
        layout.addWidget(self.detail)

        self.button = QPushButton("选择文件夹…")
        self.button.setFixedWidth(180)
        row = QHBoxLayout()
        row.addStretch(1)
        row.addWidget(self.button)
        row.addStretch(1)
        layout.addLayout(row)

        # 拖放必须显式开 acceptDrops：Qt 默认不接受，开了之后
        # dragEnterEvent 才会收到事件，而不是直接穿透。
        self.setAcceptDrops(True)
        self.setObjectName("dropzone")

    # -- 拖放 ---------------------------------------------------------------

    def _offered_paths(self, event) -> list[str]:
        """从拖放事件里取出本地路径（只认本地文件，远程 URL 一律拒绝）。"""
        if not event.mimeData().hasUrls():
            return []
        return [u.toLocalFile() for u in event.mimeData().urls() if u.isLocalFile()]

    def dragEnterEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        """有本地路径悬进来就接受，光标变「可放」；否则明确拒绝。"""
        if self._offered_paths(event):
            event.acceptProposedAction()
            self.hint.setStyleSheet(f"color: {theme.PRIMARY}; font-size: 15px;")
        else:
            event.ignore()

    def dragLeaveEvent(self, event) -> None:  # noqa: N802
        """拖走了就把提示色还原，别留一个「能放」的样子。"""
        self.hint.setStyleSheet(f"color: {theme.MUTED}; font-size: 15px;")

    def dropEvent(self, event) -> None:  # noqa: N802
        paths = self._offered_paths(event)
        if not paths:
            event.ignore()
            return
        event.acceptProposedAction()
        self.dragLeaveEvent(event)
        self.pathsDropped.emit(paths)

    # -- 状态 ---------------------------------------------------------------

    def show_result(self, text: str) -> None:
        """扫描**开始**时：把提示换成进度文案，按钮置灰防重复载入。"""
        self.hint.setText(text)
        self.button.setEnabled(False)
        self.button.setText("已载入")

    def collapse(self) -> None:
        """扫描**完成**后：整块收起，把竖向空间全让给左栏 + 右栏。

        起始态那块提示文字在有内容之后就是纯占位，留着只会把清单挤到
        下面去，护士长第一眼看到的是一句已经完成的话。

        收起后停掉拖放：收起的是一个 `setVisible(False)` 的控件，
        它收不到事件，但留着 `acceptDrops` 会让拖放**看起来**仍然可用——
        拖到窗口别处没有任何反应，那比一开始就不支持更让人困惑。
        """
        self.setVisible(False)
        self.setAcceptDrops(False)

    def restore(self, text: str) -> None:
        """载入失败 / 队列被清空时把拖放区放回来，并换一句说明为什么。"""
        self.setVisible(True)
        self.setAcceptDrops(True)
        self.hint.setText(text)
        self.button.setEnabled(True)
        self.button.setText("选择文件夹…")


class EncounterBadge(QWidget):
    """左栏一条住院条目：病区 + 脱敏别名 + 件数 + 达标 / 缺件徽标。

    徽标颜色就是 spec.md §8 的四色语义：缺件 `DANGER` 红、达标 `OK` 绿。
    「缺件 N」而不是「不合规 N」——护士长看到的是"还差几件没交"，
    不是"系统判我有错"。

    「已审」另起一枚灰色角标：它是**进度**，不是**结论**。
    上午审过的那几次住院要一眼认出来，否则重开窗口后又要从头点一遍。
    """

    def __init__(self, row: dict[str, Any]) -> None:
        super().__init__()
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(2)

        # QListWidget 不会为 setItemWidget 里的控件自动撑高行，
        # 不给最小高度的话两行文字会被裁掉下半截。三行（有已审角标时）
        # 比两行高，这里一次给足，省得勾选后行高跳一下。
        self.setMinimumHeight(62)

        ward = row.get("ward") or ""
        alias = QLabel(f"{row['encounter_alias']}　{ward}" if ward else row["encounter_alias"])
        alias.setStyleSheet("font-weight: 600;")
        layout.addWidget(alias)

        missing = row.get("missing_count", 0)
        # 徽标文案由核心给（`badge_zh`）；这里只在拿不到时退回本地口径，
        # 免得「缺件 1」与「缺件 2」两个数并存——那种情况比少一个角标更糟。
        badge = QLabel(row.get("badge_zh") or f"{row.get('doc_count', 0)} 件 · 达标")
        badge.setObjectName("badge-danger" if missing else "badge-ok")
        layout.addWidget(badge)

        if row.get("reviewed_at"):
            done = QLabel(f"已审 {row['reviewed_at'][:16].replace('T', ' ')}")
            done.setObjectName("badge-done")
            layout.addWidget(done)


class FindingsPane(QWidget):
    """右栏：问题清单 + 文书卡片（段落高亮 / 依据行 / 双栏 diff）。

    **卡片的数据来自核心入口 `doc_card`，不在界面里重解析文书包。**
    点一条问题 → 起一个 `DocCardJob` → 回来渲染那一份件的段落。
    界面只负责「把已经定位好的区间画出来」，不做第二次解析：
    两处各解析一次，迟早会在规则集改版那天对同一份文书给出两个结论。
    """

    #: 卡片渲染完成后要跳到的那一段（超长段落的定位滚动由它驱动）。
    card_loaded = Signal(dict)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(8)

        self.list = QListWidget()
        self.list.setObjectName("findings")
        self.list.setAlternatingRowColors(False)
        self.list.currentRowChanged.connect(self._on_row_changed)
        layout.addWidget(self.list, 3)

        self.card = QTextEdit()
        self.card.setObjectName("doccard")
        self.card.setReadOnly(True)
        self.card.setPlaceholderText("选中左侧一条问题，这里显示该件的段落与依据。")
        layout.addWidget(self.card, 2)

        self.diff = QTextEdit()
        self.diff.setObjectName("doccard")
        self.diff.setReadOnly(True)
        self.diff.setVisible(False)
        self.diff.setPlaceholderText("双栏 diff：左原文、右补写稿。")
        layout.addWidget(self.diff, 2)

        #: 载入路径与规则集：由 `MainWindow` 在载入时注入。
        self._input_path: str | None = None
        self._ruleset_path: Path = core_bridge.default_ruleset_path()
        self._jobs: list[Any] = []
        self._pending_doc: str | None = None

    def bind(self, input_path: str | None, ruleset_path: Path) -> None:
        """告诉卡片去哪儿取数据。载入路径变了就要重新设。"""
        self._input_path = input_path
        self._ruleset_path = ruleset_path

    def _on_row_changed(self, row: int) -> None:
        item = self.list.item(row)
        if item is None:
            return
        diag = item.data(Qt.ItemDataRole.UserRole)
        if diag:
            self.show_diagnostic(diag)

    def show_diagnostic(self, diag: dict[str, Any]) -> None:
        """选中一条诊断 → 取那一份件的卡片数据。

        没载入路径时**不降级**为渲染一段拼好的假卡片：直接说清
        「还没载入文书包」，而不是给一张没有段落、没有依据的空壳。
        """
        doc_id = diag.get("doc_id")
        if not self._input_path:
            self.card.setPlainText(
                "尚未载入文书包，先拖一个病区导出目录进来再点问题。"
            )
            return
        if not doc_id:
            self.card.setPlainText("这条问题没有件 id，无法定位到具体文书。")
            return

        self._pending_doc = doc_id
        self.card.setPlainText(f"正在取 {doc_id} 的段落…")
        self.diff.setVisible(False)
        try:
            ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        except OSError as exc:
            self.card.setPlainText(f"E_RULESET_INVALID：{exc}")
            return
        job = DocCardJob(self._input_path, ruleset_yaml, doc_id)
        job.ready.connect(self._on_card_ready)
        job.failed.connect(self._on_card_failed)
        self._retain(job)

    def _on_card_ready(self, payload: dict[str, Any]) -> None:
        doc = payload.get("doc", {})
        diags = list(payload.get("diagnostics", [])) + list(
            payload.get("parse_issues", [])
        )
        focus = doc_card.card_focus_target(diags)
        views = doc_card.build_sections(doc.get("sections", []), diags)
        preview = payload.get("preview", {})
        self.card.setHtml(
            doc_card.render_card(doc, views, focus=focus, preview=preview)
        )
        self.card_loaded.emit(payload)
        self._render_diff(preview, focus)

    def _render_diff(
        self, preview: dict[str, Any], focus: dict[str, Any] | None
    ) -> None:
        """双栏 diff：只渲染**当前聚焦那条**对应的候选块。

        一张卡片里有好几处命中时，护士长点的是哪一条，diff 就该是哪一条；
        一次把全部候选块铺开，diff 会和上面选中的那条对不上。
        """
        blocks = preview.get("blocks") or []
        chosen = None
        if focus is not None:
            for b in blocks:
                if b["code"] == focus.get("code") and (
                    b.get("section_index") == focus.get("section_index")
                ):
                    chosen = b
                    break
        if chosen is None and blocks:
            chosen = blocks[0]
        if chosen is None:
            # 没有候选块不是错误：显式说清「只给依据」而不是留空白。
            self.diff.setHtml(doc_card.render_evidence_only_html(preview))
            self.diff.setVisible(True)
            return
        html_text = doc_card.render_diff_html(chosen)
        if preview.get("evidence_only"):
            html_text += doc_card.render_evidence_only_html(preview)
        self.diff.setHtml(html_text)
        self.diff.setVisible(True)

    def _on_card_failed(self, exc: object) -> None:
        code = getattr(exc, "code", "E_INTERNAL")
        message = getattr(exc, "message", str(exc))
        self.card.setPlainText(f"{code}：{message}")
        self.diff.setVisible(False)

    def _retain(self, job: Any) -> None:
        """留住 job 直到收完。

        不留会被 Python GC 掉 Qt 侧的连接，表现为「第一次点有反应、
        第二次点没反应」——那种 bug 极难从代码上看出来。
        """
        self._jobs.append(job)
        job.done.connect(lambda _: self._forget(job))
        job.failed.connect(lambda _: self._forget(job))

    def _forget(self, job: Any) -> None:
        if job in self._jobs:
            self._jobs.remove(job)

    def load(self, diagnostics: list[dict[str, Any]]) -> None:
        self.list.clear()
        for d in diagnostics:
            severity = d.get("severity", "blocking")
            marker = "阻断" if severity == "blocking" else "提示"
            text = (
                f"{d['code']} · {marker} · {d['doc_id']} 第 {d['section_index']} 段\n"
                f"{d['message']}"
            )
            item = QListWidgetItem(text)
            item.setData(Qt.ItemDataRole.UserRole, d)
            self.list.addItem(item)
        if self.list.count() == 0:
            self.card.setPlaceholderText("本次审查未发现规则命中项。")

    def reset(self) -> None:
        self.list.clear()
        self.card.clear()
        self.card.setPlaceholderText("选中左侧一条问题，这里显示该件的段落与依据。")
        self.diff.setVisible(False)


class ArchivePane(ArchiveViewPane):
    """归档核齐标签页。

    实现见 `desktop/archive_view.py`；这里只保留一个同名薄壳，
    让主窗口的历史引用（`self.archive_pane.load(...)`）继续可用。
    """


class SkippedPane(QWidget):
    """「已跳过」清单：**每一个没被打开的文件**与它没被打开的原因。

    为什么这块必须在界面上：护士长把整个病区文件夹拖进来，文件夹里
    混着 pdf 扫描件、xlsx 台账、上一份没导完的 json。如果这些被静默丢掉，
    底栏会显示「审了 52 件、0 条问题」，护士长看到的是**全部合规**——
    而实际上有一份导出包压根没被打开过。这条清单就是那个缺口。
    """

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(0, 0, 0, 0)
        layout.setSpacing(4)

        self.title = QLabel("已跳过")
        self.title.setStyleSheet(f"color: {theme.MUTED}; font-weight: 600; font-size: 12px;")
        layout.addWidget(self.title)

        self.list = QListWidget()
        self.list.setObjectName("findings")
        self.list.setToolTip("这些文件没有被当作文书包打开，及其原因")
        layout.addWidget(self.list, 1)

    def load(self, file_skips: list[dict[str, Any]], empty_reason: str | None) -> None:
        self.list.clear()
        if not file_skips:
            if empty_reason:
                self._add(f"⚠ {empty_reason}", theme.DANGER)
            else:
                self._add("本次没有跳过的文件（每个文件都被当作文书包打开过）。", theme.MUTED)
            self.title.setText("已跳过")
            return
        self.title.setText(f"已跳过 {len(file_skips)} 个文件")
        for s in file_skips:
            retry = "（可重试）" if s.get("retryable") else ""
            self._add(f"{s['relative_path']} —— {s['reason_zh']}{retry}", theme.MUTED)

    def _add(self, text: str, color: str) -> None:
        item = QListWidgetItem(text)
        item.setForeground(Qt.GlobalColor.gray if color == theme.MUTED else Qt.GlobalColor.red)
        item.setFlags(Qt.ItemFlag.NoItemFlags)  # 只读清单，不给勾选
        self.list.addItem(item)


class MainWindow(QMainWindow):
    """精神科住院文书审查系统 —— 主窗口。"""

    def __init__(self) -> None:
        super().__init__()
        self.setWindowTitle(DISPLAY_NAME)
        self.resize(1180, 760)

        # 当前载入的输入与规则集：所有 job 的入参都从这里取，
        # 不散落在各回调里（散落会让"这次审的是哪一份"查不清）。
        self._input_path: str | None = None
        self._input_roots: list[str] = []
        self._ruleset_path: Path = core_bridge.default_ruleset_path()
        self._encounters: list[dict[str, Any]] = []
        self._current_node: str | None = None
        #: 当前选中的**住院别名**（归档核齐视图按它出图，依赖图绝不跨住院）
        self._current_alias: str | None = None
        self._jobs: list[CoreJob] = []
        #: 上次落库的队列 id（关窗重开后据此还原队列与进度）。
        self._queue_id: str | None = None
        #: 本轮批量审查里**还没审到**的住院别名。全部审完后剩下的为空，
        #: 与 `_encounters` 的差集就是已审的那几条（落库用）。
        self._reviewing: set[str] = set()
        #: 本轮**确实跑过**的住院别名（批量审查的已审集合）。
        #: 刻意与 `_reviewing`（待审集合）分开存：已审 = 待审的补集
        #: 在「一条都没跑成」时等于整个队列，会把没审过的住院标成已审。
        self._reviewed_this_round: set[str] = set()
        #: 本轮待判的件数（底栏进度行里的「本轮 N 件待判」）。
        #: 从队列的 `doc_count` 累加，不另算一遍。
        self._pending: int = 0
        #: 批量审查的待跑队列 / 计数。**依次**跑，见 `_review_aliases`。
        self._batch_queue: list[str] = []
        self._batch_done: int = 0
        self._batch_total: int = 0
        #: 最近一次 `review_batch` 的真实 `run_summary`（重跑 / 跳过 / 规则集短哈希）。
        #: 底栏的「重跑 N 件 · 缓存跳过 M 件」只读它，不另算一遍。
        self._last_run_summary: dict[str, Any] | None = None
        #: 托盘触发的一轮重跑的收尾回调（Task 35）。`None` = 不是托盘那一轮。
        self._rerun_done = None
        #: 窗口标题上的角标后缀（「· 待提交 3 份」），与托盘角标同源。
        self._title_suffix: str = ""
        #: 本次扫描没被打开的文件（托盘重审后 totals 里的「已跳过 N 个」）。
        self._file_skips: list[dict[str, Any]] = []
        #: 常驻控制器（Task 35）。`None` = 本次只跑窗口不跑托盘。
        self.tray: Any = None

        self._build_ui()
        self._build_menu()
        self._update_status(loaded=False)

    # ------------------------------------------------------------------
    # 关窗重开：还原上一次的队列
    # ------------------------------------------------------------------

    def restore_last_queue(self) -> bool:
        """启动时读回上一次落库的队列。

        返回是否真的还原了。**还原的只是队列骨架**（哪几次住院、几件、
        缺几件、已审到哪），诊断清单要等用户点开某一次住院再跑一遍规则——
        规则集可能已经改版，拿旧诊断冒充新结论才是真的错。

        没落过队列时返回 ``False``，界面留在起始态的拖放区。
        """
        try:
            snap = core_bridge.load_queue()
        except core_bridge.CoreUnavailable:
            return False  # 首次启动：本机还没建过库，留在起始态
        except core_bridge.CoreError as exc:
            self.status_label.setText(f"{exc.code}：{exc.message}")
            return False
        if not snap:
            return False

        self._queue_id = snap["queue_id"]
        self._input_roots = [snap["source_path"]]
        self._input_path = snap["source_path"].split("\x1f")[0]
        self.findings.bind(self._input_path, self._ruleset_path)
        self._fill_queue(snap["entries"], snap.get("file_skips", []), None)
        self.drop.collapse()
        t = snap["totals"]
        p = snap["progress"]
        self.status_label.setText(
            f"已恢复上次队列 · 已审 {p['reviewed']} / 共 {p['total']} 次住院 · "
            f"规则集 {snap['ruleset_short_sha256']}"
        )
        self.tray_summary.setText(f"共 {t['items']} 件 · 缺件 {t['missing']}")
        self._update_skip_pane(snap.get("file_skips", []), None)
        self._refresh_tray_badge()
        return True

    # ------------------------------------------------------------------
    # 版式
    # ------------------------------------------------------------------

    def _build_ui(self) -> None:
        self.tabs = QTabWidget()
        self.tabs.addTab(self._build_review_tab(), "文书包审查")
        self.tabs.addTab(self._build_archive_tab(), "归档核齐")
        self.tabs.currentChanged.connect(self._on_tab_changed)
        self.setCentralWidget(self.tabs)

        # 底栏运行摘要：重跑件数 / 跳过件数 / 规则集短哈希。
        status = QStatusBar()
        self.setStatusBar(status)
        self.status_label = QLabel()
        self.status_label.setObjectName("statusbar")
        status.addWidget(self.status_label, 1)

        self.tray_summary = QLabel()
        self.tray_summary.setObjectName("statusbar")
        status.addPermanentWidget(self.tray_summary)

    def _build_review_tab(self) -> QWidget:
        page = QWidget()
        outer = QVBoxLayout(page)
        outer.setContentsMargins(12, 12, 12, 12)

        self.drop = DropZone()
        self.drop.button.clicked.connect(self._on_pick_folder)
        # 拖放与「选择文件夹」走同一条路：三条入口一个出口。
        self.drop.pathsDropped.connect(self.load_paths)
        outer.addWidget(self.drop)

        splitter = QSplitter(Qt.Orientation.Horizontal)

        # 左栏：住院条目列表（病区 + 别名 + 件数 + 缺件徽标 + 已审角标）
        left = QWidget()
        left_layout = QVBoxLayout(left)
        left_layout.setContentsMargins(0, 0, 0, 0)
        left_layout.setSpacing(8)

        queue_bar = QHBoxLayout()
        queue_bar.addWidget(QLabel("住院条目"))
        self.batch_button = QPushButton("批量审查选中")
        self.batch_button.setToolTip(
            "审查左栏选中的几次住院。\n"
            "按住 Ctrl 多选、Shift 连选；未选中任何一条时按钮置灰。"
        )
        self.batch_button.setEnabled(False)
        self.batch_button.clicked.connect(self._on_batch_review)
        queue_bar.addStretch(1)
        queue_bar.addWidget(self.batch_button)
        left_layout.addLayout(queue_bar)

        self.encounter_list = QListWidget()
        # 多选：批量审查的前提。单选也能跑（就一次），但护士长一次要
        # 走一个病区的十几��住院，单选会让人点到手废。
        self.encounter_list.setSelectionMode(
            QAbstractItemView.SelectionMode.ExtendedSelection
        )
        self.encounter_list.currentRowChanged.connect(self._on_encounter_selected)
        self.encounter_list.itemSelectionChanged.connect(self._on_selection_changed)
        left_layout.addWidget(self.encounter_list, 3)

        self.skipped = SkippedPane()
        left_layout.addWidget(self.skipped, 1)

        splitter.addWidget(left)

        # 右栏：问题清单 + 文书卡片
        self.findings = FindingsPane()
        splitter.addWidget(self.findings)
        splitter.setStretchFactor(0, 1)
        splitter.setStretchFactor(1, 3)
        outer.addWidget(splitter, 1)
        return page

    def _build_archive_tab(self) -> QWidget:
        self.archive_pane = ArchivePane()
        self.archive_pane.notifyRequested.connect(self._on_notify_requested)
        self.archive_pane.exportRequested.connect(self._on_export_requested)
        return self.archive_pane

    def _build_menu(self) -> None:
        file_menu = self.menuBar().addMenu("文件")
        open_action = QAction("打开文书包…", self)
        open_action.triggered.connect(self._on_pick_folder)
        file_menu.addAction(open_action)

        rescan = QAction("重新审查", self)
        rescan.triggered.connect(self._on_rescan)
        file_menu.addAction(rescan)

        file_menu.addSeparator()
        quit_action = QAction("退出", self)
        quit_action.triggered.connect(self.close)
        file_menu.addAction(quit_action)

    # ------------------------------------------------------------------
    # 载入与扫描
    # ------------------------------------------------------------------

    def _on_pick_folder(self) -> None:
        from PySide6.QtWidgets import QFileDialog

        path = QFileDialog.getExistingDirectory(self, "选择住院文书包导出目录")
        if not path:
            path, _ = QFileDialog.getOpenFileName(
                self, "选择一个 FHIR R4 Bundle 导出文件", "", "JSON 文书包 (*.json)"
            )
        if not path:
            return
        self.load_path(path)

    def load_path(self, path: str) -> None:
        """载入一个导出目录或单份 Bundle（`python -m desktop.main <路径>` 用）。"""
        self.load_paths([path])

    def load_paths(self, paths: list[str]) -> None:
        """载入一个 / 几个导出目录或文件 → 扫描成审查队列。

        拖放、菜单「打开文书包…」、「选择文件夹」、命令行预设路径，
        四条入口都汇到这里——**只有这一条**。让它们各走各的，早晚会出现
        「拖进来的能审、按钮选的扫不到」这种查起来极费劲的分叉。

        规则集读不到时**不**降级为「无规则 = 全部通过」：直接报
        ``E_RULESET_INVALID`` 并把空窗口留给用户。
        """
        paths = [p for p in paths if p]
        if not paths:
            return
        try:
            ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        except OSError as exc:
            QMessageBox.critical(
                self,
                "读不到规则集",
                f"E_RULESET_INVALID：{self._ruleset_path}\n{exc}\n\n"
                "系统不会在没有规则的情况下给出「全部通过」的结论。",
            )
            return

        self._input_roots = list(paths)
        # 归档核齐视图与预览仍按「一个路径」工作（依赖图绝不跨住院别名，
        # 目录入口已经同时收多个根）；这里存第一个作为代表路径。
        self._input_path = paths[0]
        # 卡片要自己去核心取这一件的段落，路径一变就得重新绑定。
        self.findings.bind(self._input_path, self._ruleset_path)

        names = "、".join(Path(p).name for p in paths[:2])
        if len(paths) > 2:
            names += f" 等 {len(paths)} 个"
        self.drop.show_result(f"正在扫描 {names} …")
        self.setCursor(Qt.CursorShape.WaitCursor)
        job = run_core_job(
            core_bridge.scan_queue,
            paths,
            ruleset_yaml,
            # 时刻由壳注入而不是核心读系统时钟：同一份目录两次扫描要能
            # 给出可复现的落库记录，审计导出才 diff 得动。
            _now_rfc3339(),
            core_bridge.default_store_path(),
            True,
        )
        job.done.connect(self._on_scan_done)
        job.failed.connect(self._on_batch_failed)
        self._retain(job)

    def _on_scan_done(self, payload: dict[str, Any]) -> None:
        """扫描完成：填队列、填「已跳过」，并对选中的住院发起批审查。"""
        self.unsetCursor()
        self._queue_id = payload.get("queue_id")
        entries = payload.get("entries", [])
        self._fill_queue(entries, payload.get("file_skips", []), payload.get("empty_reason_zh"))

        totals = payload.get("totals", {})
        if not entries:
            # 一份包都没打开：**明说为什么**，而不是让界面停在「0 条命中」。
            reason = payload.get("empty_reason_zh") or "没有扫描到任何文书包。"
            self.drop.restore(reason)
            self.findings.reset()
            self.status_label.setText(reason)
            self.tray_summary.setText("队列为空")
            self._refresh_tray_badge()
            QMessageBox.information(self, "没扫描到文书包", reason)
            return

        self.drop.collapse()
        self._update_skip_pane(payload.get("file_skips", []), payload.get("empty_reason_zh"))
        self._update_status(loaded=True, totals=totals)
        self._refresh_tray_badge()

        # 载入后自动审查**当前选中**的那几条（默认第一条）。
        # 没有默认选中就审 0 条，而不是擅自把整个病区都跑一遍——
        # 后者在 200 件的包上会把界面冻住几十秒。
        selected = self._selected_aliases()
        if not selected and self.encounter_list.count():
            self.encounter_list.setCurrentRow(0)
            selected = self._selected_aliases()
        if selected:
            self._review_aliases(selected)

    def _fill_queue(
        self,
        entries: list[dict[str, Any]],
        file_skips: list[dict[str, Any]],
        empty_reason: str | None,
    ) -> None:
        """把队列条目铺进左栏（顺序由核心给，不在这里重排）。"""
        self._encounters = list(entries)
        self.encounter_list.clear()
        for row in self._encounters:
            item = QListWidgetItem()
            item.setData(Qt.ItemDataRole.UserRole, row)
            # setItemWidget 的控件**不会**把行撑高：行高只由 item 自己的
            # sizeHint 决定。不显式给，行会被裁成一行高，徽标与「已审」
            # 角标只在选中那一条看得见——正是最不该藏起来的进度信息。
            item.setSizeHint(QSize(0, 62))
            item.setToolTip(
                f"{row['encounter_alias']}\n"
                f"{row.get('badge_zh', '')}\n"
                f"来源文件：{'、'.join(row.get('source_files', [])) or '—'}"
            )
            self.encounter_list.addItem(item)
            self.encounter_list.setItemWidget(item, EncounterBadge(row))
        self._update_selection_button()

    def _update_skip_pane(self, file_skips: list[dict[str, Any]], empty_reason: str | None) -> None:
        # 记一份：托盘重审后底栏还要报「已跳过 N 个文件」，那不能靠重扫目录。
        self._file_skips = list(file_skips)
        self.skipped.load(file_skips, empty_reason)

    # ------------------------------------------------------------------
    # 多选批量审查
    # ------------------------------------------------------------------

    def _selected_aliases(self) -> list[str]:
        """当前选中的住院别名（按左栏显示顺序）。

        返回的是**别名列表**而不是控件对象：批审查要在工作线程里逐条跑，
        搬界面对象进线程是这条纪律里明确禁止的。
        """
        rows = sorted(
            (self.encounter_list.row(item) for item in self.encounter_list.selectedItems())
        )
        out: list[str] = []
        for row in rows:
            data = self.encounter_list.item(row).data(Qt.ItemDataRole.UserRole)
            if data:
                out.append(data["encounter_alias"])
        return out

    def _on_selection_changed(self) -> None:
        self._update_selection_button()

    def _update_selection_button(self) -> None:
        n = len(self.encounter_list.selectedItems())
        self.batch_button.setEnabled(n > 0)
        self.batch_button.setText(f"批量审查选中（{n}）" if n else "批量审查选中")

    def _on_batch_review(self) -> None:
        aliases = self._selected_aliases()
        if not aliases:
            return  # 按钮在未选中时已置灰；这里只是不让人绕过
        self._review_aliases(aliases)

    def _review_aliases(self, aliases: list[str]) -> None:
        """审查选中的几次住院。逐条跑，底栏显示 (已审 / 总数) 进度。

        「批量」不是把 N 次住院拼成一个请求——而是**依次**审：这样底栏
        能给出真实进度（第 3 / 共 12），而不是在最后一次性从 0 跳到 12。
        """
        try:
            ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        except OSError as exc:
            self.status_label.setText(f"E_RULESET_INVALID：{exc}")
            return
        if not self._input_path:
            self.status_label.setText("尚未载入文书包，先拖一个文件夹进来。")
            return

        self._reviewing = set(aliases)
        self._reviewed_this_round.clear()
        self._pending = sum(
            r.get("doc_count", 0)
            for r in self._encounters
            if r["encounter_alias"] in self._reviewing
        )
        self._update_progress(0, len(aliases))
        self.setCursor(Qt.CursorShape.WaitCursor)
        # **依次**审，不并发。
        #
        # 两个理由，第二个更要紧：
        #   1) 并发下完成顺序不保证，底栏的「已审 3/12」会倒着跳；
        #   2) 每个 job 都对**整个目录**跑一遍 `review_batch`，并发 12 个
        #      等于把同一份包扫 12 遍。依次跑，第 2 次起增量缓存会把没变的
        #      件全部跳过，代价才线性。
        self._batch_queue = list(aliases)
        self._batch_total = len(aliases)
        self._batch_done = 0
        self._start_next_in_batch()

    def _start_next_in_batch(self) -> None:
        """起下一条住院的审查；队列空了就把光标放开并落库。"""
        if not self._batch_queue:
            self.unsetCursor()
            self._persist_reviewed()
            self._finish_rerun_round()
            return
        alias = self._batch_queue.pop(0)
        ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        job = run_core_job(
            core_bridge.review_batch,
            self._input_path,
            ruleset_yaml,
            str(core_bridge.default_cache_path()),
        )
        job.done.connect(lambda p, a=alias: self._on_one_reviewed(p, a))
        job.failed.connect(self._on_batch_failed)
        self._retain(job)

    def _on_one_reviewed(self, payload: dict[str, Any], alias: str) -> None:
        """一条住院审完：右栏换成**这一条**的诊断，推进进度，起下一条。

        过滤到这一条是必须的：`review_batch` 对整个目录跑一遍，返回的是
        全院诊断。不按住院切分的话，护士长审第 3 次住院，右栏里却躺着
        全病区的问题——那是这个工具最该犯的错里最容易犯的一个。
        """
        diagnostics = [
            d
            for d in payload.get("summary", {}).get("diagnostics", [])
            if self._alias_of_doc(d.get("doc_id")) == alias
        ]
        self.findings.load(diagnostics)
        self._reviewing.discard(alias)
        self._reviewed_this_round.add(alias)
        self._batch_done += 1
        # 增量重跑（托盘触发）要看真实缓存命中数：只记**最后一次**返回的
        # run_summary，并在轮末刷底栏。不估算、不用「大概跳过了多少」。
        self._last_run_summary = payload.get("run_summary") or self._last_run_summary
        self._update_progress(self._batch_done, self._batch_total)
        self._start_next_in_batch()

    def _alias_of_doc(self, doc_id: str | None) -> str | None:
        """件 id → 住院别名（左栏队列里已有一张反查表）。

        核心的诊断 JSON 不带住院别名（那是队列入口才有的字段），
        这里用左栏已聚合好的 `doc_ids` 反查，**不在界面里重新解析 doc_id**
        ——按字符串前缀猜别名，正是 Task 4 明令禁止的「按名字配对」。
        """
        if not doc_id:
            return None
        for row in self._encounters:
            if doc_id in row.get("doc_ids", []):
                return row["encounter_alias"]
        return None

    def _update_progress(self, done: int, total: int) -> None:
        """底栏的批量审查进度：`(已审 k / 共 N 次住院)`。

        数字**只来自真实完成的条数**（`_batch_done`），不是「已发起的条数」。
        并发时后者会先跳到 N 再逐条落回来，护士长看到的是进度条倒退。
        """
        if total <= 0:
            return
        extra = f" · 本轮 {self._pending} 件待判" if self._pending else ""
        self.status_label.setText(f"批量审查 · 已审 {done} / 共 {total} 次住院{extra}")
        self.tray_summary.setText(f"{done}/{total}")

    def _persist_reviewed(self) -> None:
        """把这一轮审过的住院标记落库（关窗重开后进度还在）。

        记的是 `_reviewed_this_round`（本轮**确实跑完**的那几条），
        **不是**「队列里减去 `_reviewing` 的补集」——补集在一条都没跑成时
        等于整个队列，会把没审过的住院一并标成已审。那是常驻工具最不该
        出的错：重开窗口后护士长以为全院都走完了。
        """
        if not self._queue_id:
            return
        done = sorted(self._reviewed_this_round)
        if not done:
            return
        try:
            core_bridge.mark_queue_reviewed(self._queue_id, done, _now_rfc3339())
        except core_bridge.CoreError as exc:
            # 标记失败不吞：进度没落库，用户重开窗口会以为没审过。
            self.status_label.setText(f"{exc.code}：{exc.message}")
            return
        now = _now_rfc3339()
        for row in self._encounters:
            if row["encounter_alias"] in done:
                row["reviewed_at"] = now
        self._refresh_badges()
        self._reviewed_this_round.clear()
        p = self._progress()
        self.status_label.setText(
            f"批量审查完成 · 已审 {p['reviewed']} / 共 {p['total']} 次住院 · "
            f"队列已落库"
        )
        self._refresh_tray_badge()

    def _progress(self) -> dict[str, int]:
        reviewed = sum(1 for r in self._encounters if r.get("reviewed_at"))
        return {"reviewed": reviewed, "total": len(self._encounters)}

    def _refresh_badges(self) -> None:
        for i, row in enumerate(self._encounters):
            item = self.encounter_list.item(i)
            if item is not None:
                self.encounter_list.setItemWidget(item, EncounterBadge(row))

    def _refresh_tray_badge(self) -> None:
        """队列变了就刷托盘角标。

        只在**队列状态**真变了的三处调（扫完、审完落库、载入失败），
        别的路径不碰——角标一屏一变，护士长就学会不信它了。
        """
        if self.tray is not None:
            self.tray.refresh_badge()

    def _retain(self, job: Any) -> None:
        """把 job 挂到窗口上，直到它收完。

        Qt 的 `QRunnable` 在 `run()` 结束后就 `autoDelete` 了，承载回调信号的
        `QObject` 随之析构——如果只把它写成 `_load_archive` 的局部变量，
        PySide 侧一回收 Python 包装对象，信号就永远送不到，界面停在空白。
        收完即从列表里摘掉，避免窗口开久了积累几十个死 job。
        """
        self._jobs.append(job)
        # 只挂「完成」那一路：CoreJob 有 done/failed，ArchiveViewJob 与
        # PreviewJob 只有 ready。全部属性化一遍太啰嗦，收完即摘也保证不会漏。
        for name in ("done", "ready"):
            signal = getattr(job, name, None)
            if signal is not None:
                signal.connect(lambda *a, j=job: self._forget(j))

    def _forget(self, job: Any) -> None:
        if job in self._jobs:
            self._jobs.remove(job)

    def _on_batch_failed(self, exc: object) -> None:
        self.unsetCursor()
        self._reviewing.clear()
        code = getattr(exc, "code", "E_INTERNAL")
        message = getattr(exc, "message", str(exc))
        self.status_label.setText(f"{code}：{message}")
        self.tray_summary.setText(code)
        # 载入失败要把拖放区放回来：否则护士长面对一个收了起来的空窗口，
        # 不知道该重试还是该找信息科。
        if self._queue_id is None:
            self.drop.restore("载入没成功——把文件夹再拖进来，或点按钮重选。")
        QMessageBox.critical(self, f"审查未完成（{code}）", message)

    def _on_encounter_selected(self, row: int) -> None:
        item = self.encounter_list.item(row)
        if item is None:
            return
        data = item.data(Qt.ItemDataRole.UserRole) or {}
        doc_ids = data.get("doc_ids", [])
        if not doc_ids:
            return
        # 默认聚焦第一条被卡住的件：护士长点进来就想知道"缺哪个"。
        self._current_node = doc_ids[0]
        # 归档核齐视图是**按住院别名**算的（依赖图绝不跨住院），
        # 所以另存一份别名；拿 node_id 去问 archive_view 会得到
        # E_ENCOUNTER_UNKNOWN——件 id 与住院别名是两把不同的钥匙。
        self._current_alias = data.get("encounter_alias", "")
        self._load_archive()
        # 点一条住院，右栏就该是**那一条**的问题——这是左栏存在的意义。
        # 批量审查进行中不另起 job：那会让 N 条并发扫同一份包（见
        # `_review_aliases` 里「依次跑」的理由），也会让进度条乱跳。
        if not self._reviewing and self._queue_id:
            self._review_aliases([data["encounter_alias"]])

    def _on_rescan(self) -> None:
        # 重扫走的是**队列**入口而不是逐条审查：护士长点「重新审查」的
        # 意思是「病区又导出���新文书了，把队列刷新一遍」，而不是
        # 「把上午那十几条再跑一次」。队列刷新保留已审进度。
        if self._input_roots:
            self.load_paths(self._input_roots)

    def _on_tab_changed(self, index: int) -> None:
        if index == 1:
            self._load_archive()

    def _load_archive(self) -> None:
        if not self._input_path or not self._current_alias:
            return
        try:
            ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        except OSError as exc:
            self.archive_pane.basis.setPlainText(f"E_RULESET_INVALID：{exc}")
            return
        # 直接把路径交给核心：入口 8 同时收目录、单份文件与 Bundle 文本，
        # 壳里不自己拼字符串——那会让「归档视图算的是哪一份」查不清。
        job = CoreJob(
            core_bridge.archive_view,
            self._input_path,
            ruleset_yaml,
            self._current_alias or "",
            _now_rfc3339(),
            _business_day(),
            core_bridge.default_store_path(),
        )
        job.done.connect(self._on_archive_loaded)
        job.failed.connect(self._on_archive_failed)
        self._retain(job)

    def _on_archive_loaded(self, payload: dict[str, Any]) -> None:
        self.archive_pane.load(payload)
        # 默认聚焦「正在被卡住」的那一件：护士长点进归档核齐，
        # 最想知道的是「我这份为什么走不下去」。
        focus = None
        for row in payload.get("rows", []):
            if row.get("state") in ("missing", "blocked"):
                focus = row["node_id"]
                break
        if focus is not None:
            self.archive_pane.select_node(focus)

    def _on_archive_failed(self, exc: object) -> None:
        self.archive_pane.basis.setPlainText(
            f"{getattr(exc, 'code', 'E_INTERNAL')}：{getattr(exc, 'message', str(exc))}"
        )

    # ------------------------------------------------------------------
    # 归档核齐：发缺件待办 / 导出 csv
    # ------------------------------------------------------------------

    def _on_notify_requested(self, missing: list[dict[str, str]]) -> None:
        """一键发缺件待办：走核心的出站链路，结果回灌到归档视图。"""
        pairs = [(m["node_id"], m["display_zh"]) for m in missing]
        job = CoreJob(
            core_bridge.notify_missing,
            encounter_alias=self._current_alias or "",
            ward=self._current_ward(),
            business_day=_business_day(),
            at=_now_rfc3339(),
            missing=pairs,
            group_hint="精神科病区文书群",
            store_path=core_bridge.default_store_path(),
        )
        job.done.connect(self._on_notify_done)
        job.failed.connect(self._on_archive_failed)
        self._retain(job)

    def _on_notify_done(self, payload: dict[str, Any]) -> None:
        self.archive_pane.apply_notify_result(payload)
        # 重算一次：按钮的置灰读的是发件箱那一行的状态，不是本次返回值，
        # 这样关窗重开后按钮仍然是灰的（护士长关一次窗口是常事）。
        self._load_archive()

    def _current_ward(self) -> str:
        row = self._current_encounter()
        return str(row.get("ward", "")) if row else ""

    def _current_encounter(self) -> dict[str, Any] | None:
        row = self.encounter_list.currentRow()
        if row < 0:
            return None
        item = self.encounter_list.item(row)
        return item.data(Qt.ItemDataRole.UserRole) if item else None

    def _on_export_requested(self) -> None:
        from PySide6.QtWidgets import QFileDialog

        payload = getattr(self.archive_pane, "_payload", None)
        if not payload:
            return
        default = f"归档核齐-{payload.get('encounter_alias', '')}-{_business_day()}.csv"
        path, _ = QFileDialog.getSaveFileName(
            self, "导出归档核齐清单", str(Path.home() / default), "CSV 文件 (*.csv)"
        )
        if not path:
            return
        # 用 UTF-8 BOM 写：Excel 不认无 BOM 的 UTF-8，中文全是乱码，
        # 病案室打开第一眼就会以为工具坏了。
        Path(path).write_text(export_archive_csv(payload), encoding="utf-8-sig")
        self.status_label.setText(f"已导出归档核齐清单：{path}")

    def open_preview(self) -> dict[str, Any] | None:
        """补写预览（Task 33 的双栏 diff 消费这个入口）。

        返回 `None` 表示**异步结果还没回来**；Task 33 接上时应改为等
        `PreviewJob.ready` 信号，而不是在这里同步等——同步等会把窗口冻住。
        """
        if not self._input_path:
            return None
        ruleset_yaml = self._ruleset_path.read_text(encoding="utf-8")
        bundle_text = Path(self._input_path).read_text(encoding="utf-8")
        result: dict[str, Any] = {}

        def on_ready(payload: dict[str, Any]) -> None:
            result.update(payload)

        job = PreviewJob(bundle_text, ruleset_yaml)
        job.ready.connect(on_ready)
        job.failed.connect(self._on_archive_failed)
        self._retain(job)
        return result or None

    # ------------------------------------------------------------------
    # 底栏
    # ------------------------------------------------------------------

    def _update_status(
        self,
        *,
        loaded: bool,
        run: dict[str, Any] | None = None,
        totals: dict[str, Any] | None = None,
    ) -> None:
        """底栏一行。

        两个形态，对应两种「当前在看什么」：

        - **未载入**：规则集名 + 尚未载入。**不写任何件数**——没扫过就是
          不知道，不拿 0 充数。
        - **队列已载入**：住院次数 / 件数 / 缺件数 / 规则集短哈希，
          以及「已跳过 N 个文件」（有跳过时必须显眼，否则「0 条问题」
          会被读成「全合规」）。
        - **批量审查进行中**：`(已审 / 总数)` 真实进度（见 `_update_progress`）。

        「跳过件数」（增量缓存命中）只在跑完一次审查后才出现，且只来自
        **真实缓存命中**，不写死、不估算。
        """
        if not loaded:
            self.status_label.setText(
                f"规则集：{self._ruleset_path.name} · 尚未载入文书包"
            )
            self.tray_summary.setText(f"核心 {core_bridge.core_version()}")
            return

        totals = totals or {}
        short = totals.get("ruleset_short_sha256") or "—"
        parts: list[str] = []
        if totals:
            parts.append(f"{totals.get('encounters', 0)} 次住院")
            parts.append(f"共 {totals.get('items', 0)} 件")
            missing = totals.get("missing", 0)
            parts.append(f"缺件 {missing}" if missing else "缺件 0")
            skipped_files = totals.get("skipped_files", 0)
            # 有跳过就必须显式说出来：这是「0 条命中 ≠ 全合规」的唯一提示
            if skipped_files:
                parts.append(f"已跳过 {skipped_files} 个文件")
        run = run or {}
        if run.get("items_reviewed") is not None:
            parts.append(f"重跑 {run['items_reviewed']} 件")
        if run.get("items_skipped") is not None:
            parts.append(f"缓存跳过 {run['items_skipped']} 件")
        parts.append(f"规则集 {short}")
        self.status_label.setText(" · ".join(parts))
        self.tray_summary.setText(f"核心 {core_bridge.core_version()}")

    # ------------------------------------------------------------------
    # 托盘（Task 35）用的四个口子
    #
    # 托盘**自己**不重扫目录、不重算缺件：数据一律从这里取。
    # 两处各算一遍角标，「托盘说 3 份、窗口说 5 份」几乎是必然的漂移。
    # ------------------------------------------------------------------

    def encounter_rows(self) -> list[dict[str, Any]]:
        """左栏队列的当前条目（托盘角标的唯一数据源）。

        返回的是 `_encounters` 的**浅拷贝**：托盘拿它算角标时不能顺手改到
        窗口内部状态，反过来也一样。
        """
        return [dict(row) for row in self._encounters]

    @property
    def input_roots(self) -> list[str]:
        """已载入的导出根（托盘轮询的监视对象）。未载入时为空。"""
        return list(self._input_roots)

    def set_title_suffix(self, badge_state: dict[str, Any]) -> None:
        """把角标挂到窗口标题：`精神科住院文书审查系统 · 待提交 3 份`。

        标题与托盘**同一口径**（`badge_from_entries` 一处算）：护士长在
        任务栏上也能看见还剩几份没收齐，不必先点开窗口。
        """
        count = int(badge_state.get("count") or 0)
        self._title_suffix = f" · 待提交 {count} 份" if count else ""
        self.setWindowTitle(DISPLAY_NAME + self._title_suffix)

    def request_rerun(
        self, aliases: list[str], on_done: Any | None = None
    ) -> bool:
        """托盘触发的一次增量重跑。返回是否真的起了。

        走的是**批量审查同一条路**（`_review_aliases`），不另开一条：
        两套重跑逻辑迟早会在「跳过数怎么算」上分叉，而分叉出来的那个数
        正是底栏给护士长看的那个。

        `on_done` 在这一轮收尾时于主线程回调，参数是本轮真实
        `run_summary`；不传就是一次普通的用户触发重审。
        """
        if not self._input_path:
            return False
        if self._reviewing:
            # 正在审：不并发重入（见 `tray.ChangeCoalescer`）。调用方
            # （托盘）本来就会把这种变更并进下一轮，这里只是把「没起」
            # 这件事说清楚，而不是静默什么都不做。
            return False
        self._rerun_done = on_done
        self._last_run_summary = None
        self._review_aliases(list(dict.fromkeys(aliases)))
        return True

    def _finish_rerun_round(self) -> None:
        """一轮重跑跑完：刷底栏 + 回调托盘。

        **只对托盘触发的那一轮做这件事**（`request_rerun` 带 `on_done` 时）。
        用户自己点的那次「批量审查」不刷：那一轮底栏停在
        「已审 k / 共 N 次住院 · 队列已落库」上，那才是用户点的动作对应的
        结论；被后来一次自动刷新换成「重跑 0 件 · 缓存跳过 52 件」，
        等于把用户刚点的「批量审查」给抹掉了。
        """
        callback, self._rerun_done = self._rerun_done, None
        if callback is None:
            return
        run = self._last_run_summary
        if run is None:
            return
        # 底栏切到增量口径：重跑 N 件 / 缓存跳过 M 件 / 规则集短哈希。
        # 跳过数直接来自核心返回的 run_summary，不是「总件数 - 重跑件数」。
        self._update_status(loaded=True, run=run, totals=self._totals())
        callback(run)

    def _totals(self) -> dict[str, Any]:
        """由队列现算的 totals（与扫描入口返回的同口径）。"""
        items = sum(int(r.get("doc_count") or 0) for r in self._encounters)
        missing = sum(int(r.get("missing_count") or 0) for r in self._encounters)
        return {
            "encounters": len(self._encounters),
            "items": items,
            "missing": missing,
            "skipped_files": len(getattr(self, "_file_skips", []) or []),
            "ruleset_short_sha256": (self._last_run_summary or {}).get(
                "ruleset_short_sha256"
            ),
        }

    def persist_before_exit(self) -> None:
        """退出前把本轮进度落库（托盘「退出」与窗口关闭都走这里）。

        只落**确实审完**的那几条——队列进度落不上去，重开窗口时护士长
        会以为这一早上白审了。
        """
        if self._reviewed_this_round:
            self._persist_reviewed()

    def show_from_tray(self) -> None:
        """托盘菜单「打开主窗口」：置前 + 取消最小化。"""
        self.showNormal()
        self.raise_()
        self.activateWindow()

    def closeEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        """关窗 = 退出（不是缩到托盘）。

        缩到托盘是另一种产品：护士长按了关闭却发现进程还在、还在占端口，
        下次以为程序没起来。这里让「关窗口」与「托盘 → 退出」**同义**，
        两条路都能真退出，也都走 `persist_before_exit` 落库。

        托盘常驻的价值在**没打开窗口的时候**：通知、增量重跑、角标。
        """
        self.persist_before_exit()
        tray = getattr(self, "tray", None)
        if tray is not None:
            tray.icon.hide()
        super().closeEvent(event)
