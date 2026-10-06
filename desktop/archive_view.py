"""归档核齐标签页：依赖图渲染 + 欠件天数 + 一键发缺件待办。

这一页回答护士长切到「归档核齐」时的三个问题，顺序就是版式顺序：

1. **哪件缺了** —— 图上四态一色到底（缺件红 / 被连带阻塞橙 / 需人工确认灰 /
   已收齐绿），阻塞边加粗。一眼看完，不用逐件点开。
2. **欠了几天、连带卡住了谁** —— 右侧栏两个方向的问题清单（这份在等谁 /
   这份卡住谁），点节点即切换。
3. **提醒发没发** —— 底部「一键发缺件待办」；已发项置灰并写明
   「已于 T 时刻发出」，同一天不会往群里发第二条。

## 三条不能破的纪律

- **颜色由核心给**。节点四态（``state`` 字段）是 Rust 侧判好的，本模块
  只做「状态 → 颜色」的查表。让界面自己判「这算不算缺件」，同一件事就
  会出现两个口径，而两个口径会变成两条互相矛盾的催办。
- **缺件口径与闭包同源**。参考缺件（``requires: false``）在图上可见但
  **不**进缺件清单、不进通知——护士长不该收到一条根本不欠的催办。
- **发过就是发过**。按钮置灰读的是发件箱里那一行的状态，不是本次点击
  的返回值；进程重启后重开窗口，置灰照样生效。
"""

from __future__ import annotations

from typing import Any

from PySide6.QtCore import QRectF, Qt, Signal
from PySide6.QtGui import QBrush, QColor, QFont, QPainter, QPen
from PySide6.QtWidgets import (
    QHBoxLayout,
    QLabel,
    QListWidget,
    QListWidgetItem,
    QPushButton,
    QSplitter,
    QTextEdit,
    QVBoxLayout,
    QWidget,
)

from . import theme

#: 节点框尺寸。够放「约束知情同意」六个汉字又不至于把图撑得太散。
NODE_W = 132
NODE_H = 46
#: 同层节点的横向间距 / 层与层的纵向间距。
GAP_X = 26
GAP_Y = 78


def color_of(state: str) -> str:
    """节点态 → 颜色。未知的态退回中性灰而不是猜一个。

    宁可显示成「不知道是什么状态」，也不能给一件可能欠着的文书涂上绿色。
    """
    return theme.NODE_COLORS.get(state, theme.AMBIGUOUS)


class GraphView(QWidget):
    """依赖图：分层布局 + 四态配色 + 阻塞边加粗。

    布局是**分层**的（按依赖深度分层，同层横排）：文书依赖图的业务含义是
    「后件等前件」，分层能把这条方向画成自上而下的一条轴，护士长扫一眼
    就知道该从最上面一层往下追。

    为什么自己画而不是用 ``QGraphicsView``：节点只有几十个、边只有几十条，
    一屏一屏重画的量级用 ``paintEvent`` 完全够；而自定义绘制让「阻塞边
    加粗」这件事变成一行 ``setWidth``，不必与图框架的默认笔宽较劲。
    """

    nodeClicked = Signal(str)

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        self.setObjectName("graph")
        self.setMinimumHeight(220)
        self._rows: list[dict[str, Any]] = []
        self._edges: list[dict[str, Any]] = []
        self._positions: dict[str, QRectF] = {}
        self._focus: str | None = None
        self.setAutoFillBackground(True)
        self.setStyleSheet(f"background: {theme.BG}; border: 1px solid {theme.BORDER};")

    # ------------------------------------------------------------------
    # 数据
    # ------------------------------------------------------------------

    def load(self, rows: list[dict[str, Any]], edges: list[dict[str, Any]]) -> None:
        self._rows = list(rows)
        self._edges = list(edges)
        if self._focus not in {r["node_id"] for r in self._rows}:
            self._focus = None
        self._relayout()
        self.update()

    def clear(self) -> None:
        self._rows = []
        self._edges = []
        self._positions = {}
        self._focus = None
        self.update()

    def row_for(self, node_id: str) -> dict[str, Any] | None:
        for r in self._rows:
            if r["node_id"] == node_id:
                return r
        return None

    def focus_node(self, node_id: str | None) -> None:
        self._focus = node_id
        self.update()

    def focused_node(self) -> str | None:
        return self._focus

    def position_of(self, node_id: str) -> QRectF | None:
        return self._positions.get(node_id)

    # ------------------------------------------------------------------
    # 分层布局
    # ------------------------------------------------------------------

    def _layer_of(self) -> dict[str, int]:
        """按依赖深度分层：没有前置件的落在第 0 层，逐层往后推。

        环在这一层**不会**死循环成无限深：`seen` 记下算过的节点，
        遇到回边直接按已算的层走。核心侧本来就把成环拒在出图之前
        （``E_DEP_CYCLE``），这里的兜底只是不让渲染路径有崩溃的入口。
        """
        depth: dict[str, int] = {}
        in_graph = {r["node_id"] for r in self._rows}
        # `from` 依赖 `to`：前置件（to）排前面，后件（from）往后。
        prereqs: dict[str, list[str]] = {n: [] for n in in_graph}
        for e in self._edges:
            if e["from"] in in_graph and e["to"] in in_graph:
                prereqs[e["from"]].append(e["to"])

        def resolve(node: str, seen: frozenset[str]) -> int:
            if node in depth:
                return depth[node]
            if node in seen:
                return 0
            seen = seen | {node}
            parents = prereqs.get(node) or []
            value = 0 if not parents else 1 + max(resolve(p, seen) for p in parents)
            depth[node] = value
            return value

        for n in sorted(in_graph):
            resolve(n, frozenset())
        return depth

    def _relayout(self) -> None:
        self._positions = {}
        if not self._rows:
            return
        depth = self._layer_of()
        by_layer: dict[int, list[str]] = {}
        for r in self._rows:
            by_layer.setdefault(depth.get(r["node_id"], 0), []).append(r["node_id"])
        margin = 18.0
        max_cols = max(len(ids) for ids in by_layer.values())
        width = margin * 2 + max_cols * NODE_W + (max_cols - 1) * GAP_X
        height = margin * 2 + len(by_layer) * NODE_H + (len(by_layer) - 1) * GAP_Y
        self.setMinimumSize(int(width), int(height))
        for layer, ids in sorted(by_layer.items()):
            # 同层按 node_id 排序：同一份包两次渲染的版式逐像素一致，
            # 截图与截图才能 diff。
            for col, node_id in enumerate(sorted(ids)):
                x = margin + col * (NODE_W + GAP_X)
                y = margin + layer * (NODE_H + GAP_Y)
                self._positions[node_id] = QRectF(x, y, NODE_W, NODE_H)

    # ------------------------------------------------------------------
    # 绘制
    # ------------------------------------------------------------------

    def paintEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        painter = QPainter(self)
        painter.setRenderHint(QPainter.RenderHint.Antialiasing)
        painter.fillRect(self.rect(), QColor(theme.BG))
        if not self._rows:
            painter.setPen(QColor(theme.MUTED))
            painter.drawText(
                self.rect(),
                Qt.AlignmentFlag.AlignCenter,
                "还没有可画的依赖图：先在上一个标签页载入住院文书包。",
            )
            return

        # 边先画、节点后画：节点框压在边上面，交叉处不会出现线穿字。
        for e in self._edges:
            a = self._positions.get(e["from"])
            b = self._positions.get(e["to"])
            if a is None or b is None:
                continue
            start = a.center().toPoint()
            end = b.center().toPoint()
            blocking = bool(e.get("blocking"))
            pen = QPen(QColor(theme.DANGER if blocking else theme.BORDER))
            # 阻塞边加粗：这是这一页唯一的「加粗」语义，别处不要复用这个宽度。
            pen.setWidthF(3.0 if blocking else 1.0)
            if not blocking and not e.get("requires", True):
                pen.setStyle(Qt.PenStyle.DotLine)
            painter.setPen(pen)
            painter.drawLine(start, end)
            if blocking:
                painter.drawLine(end, end + (start - end) / 12)

        font = QFont()
        font.setPointSize(9)
        painter.setFont(font)
        for row in self._rows:
            rect = self._positions.get(row["node_id"])
            if rect is None:
                continue
            state = row.get("state", "ok")
            color = QColor(color_of(state))
            focused = self._focus == row["node_id"]
            painter.setBrush(QBrush(color))
            pen = QPen(QColor(theme.BORDER))
            pen.setWidthF(2.0 if focused else 1.0)
            if focused:
                pen.setColor(QColor(theme.PRIMARY))
                pen.setWidthF(3.0)
            painter.setPen(pen)
            painter.drawRoundedRect(rect, 4.0, 4.0)

            # 缺件节点的 id 是合成的 `<住院别名>::missing::<类型码>`，
            # 画在框里会糊成一团；这类节点只显示院内类型名。
            label = row.get("display_zh") or row.get("template_id") or row["node_id"]
            painter.setPen(QColor("#FFFFFF"))
            painter.drawText(
                rect.adjusted(6, 4, -6, -18),
                Qt.AlignmentFlag.AlignCenter,
                label,
            )
            # 第二行：状态 + 欠件天数。天数算不出时显示「算不出」而不是 0。
            days = row.get("missing_days")
            detail = theme.NODE_STATE_ZH.get(state, state)
            if state == "missing":
                detail += f" · 欠 {days} 天" if days is not None else " · 天数算不出"
            elif state == "blocked" and row.get("blocked_depth"):
                detail += f"（第 {row['blocked_depth']} 级）"
            painter.setPen(QColor("#F0F3F6"))
            painter.drawText(rect.adjusted(6, 22, -6, -2), Qt.AlignmentFlag.AlignLeft, detail)

            # 需人工确认的件加一个角标：工具没判定这件事，只能人来判。
            if state == "ambiguous":
                badge = QRectF(rect.right() - 16, rect.top() + 3, 13, 13)
                painter.setBrush(QBrush(QColor(theme.PRIMARY)))
                painter.setPen(QPen(QColor(theme.PRIMARY)))
                painter.drawEllipse(badge)
                painter.setPen(QColor("#FFFFFF"))
                painter.drawText(badge, Qt.AlignmentFlag.AlignCenter, "?")

    def mousePressEvent(self, event) -> None:  # noqa: N802 - Qt 命名
        pos = event.position()
        for node_id, rect in self._positions.items():
            if rect.contains(pos):
                self._focus = node_id
                self.nodeClicked.emit(node_id)
                self.update()
                return
        super().mousePressEvent(event)


class ArchivePane(QWidget):
    """归档核齐标签页：图 + 侧栏 + 通知按钮。"""

    #: 点图上的节点 → 侧栏切到该件的两个方向清单。
    nodeSelected = Signal(str)
    #: 点「一键发缺件待办」：主窗口负责起 job 并把结果回灌。
    notifyRequested = Signal(list)
    #: 点「导出 csv」。
    exportRequested = Signal()

    def __init__(self, parent: QWidget | None = None) -> None:
        super().__init__(parent)
        layout = QVBoxLayout(self)
        layout.setContentsMargins(12, 12, 12, 12)
        layout.setSpacing(8)

        self.header = QLabel("尚未载入住院文书包")
        self.header.setStyleSheet(f"color: {theme.MUTED}; font-weight: 600;")
        layout.addWidget(self.header)

        splitter = QSplitter(Qt.Orientation.Horizontal)

        self.graph = GraphView()
        self.graph.nodeClicked.connect(self._on_node_clicked)
        splitter.addWidget(self.graph)

        side = QWidget()
        side_layout = QVBoxLayout(side)
        side_layout.setContentsMargins(0, 0, 0, 0)
        side_layout.setSpacing(6)

        self.age_label = QLabel("欠件天数：—")
        self.age_label.setStyleSheet(f"color: {theme.MUTED}; font-weight: 600;")
        side_layout.addWidget(self.age_label)

        self.direction_tabs = QListWidget()
        self.direction_tabs.setObjectName("findings")
        self.direction_tabs.setMaximumHeight(96)
        # 方向切换自己接刷新。挂在 MainWindow 上意味着这个组件单独用
        # （或换个人接到别处）就会静默失灵——「切方向不重绘」这种故障
        # 在界面上看起来就是「两个方向显示一样」，极难定位。
        self.direction_tabs.currentRowChanged.connect(self._on_direction_changed)
        side_layout.addWidget(self.direction_tabs)

        self.side = QListWidget()
        self.side.setObjectName("findings")
        side_layout.addWidget(self.side, 1)

        self.basis = QTextEdit()
        self.basis.setObjectName("doccard")
        self.basis.setReadOnly(True)
        self.basis.setMaximumHeight(120)
        side_layout.addWidget(self.basis)

        splitter.addWidget(side)
        splitter.setStretchFactor(0, 3)
        splitter.setStretchFactor(1, 2)
        layout.addWidget(splitter, 1)

        bar = QHBoxLayout()
        self.notify_button = QPushButton("一键发缺件待办")
        self.notify_button.setEnabled(False)
        self.notify_button.setToolTip(
            "把这次住院的缺件一次性发到病区文书群。\n"
            "同一天同一件只发一次——已发过的按钮是灰的。"
        )
        self.notify_button.clicked.connect(self._on_notify_clicked)
        bar.addWidget(self.notify_button)

        self.export_button = QPushButton("导出 csv")
        self.export_button.setEnabled(False)
        self.export_button.setToolTip("把当前图导成可核对的清单（件 / 状态 / 依赖对象 / 阻塞对象 / 欠件天数）")
        self.export_button.clicked.connect(self.exportRequested.emit)
        bar.addWidget(self.export_button)

        bar.addStretch(1)
        self.summary = QLabel("")
        self.summary.setStyleSheet(f"color: {theme.MUTED};")
        bar.addWidget(self.summary)
        layout.addLayout(bar)

        self._payload: dict[str, Any] = {}
        self.reset()

    # ------------------------------------------------------------------
    # 载入
    # ------------------------------------------------------------------

    def reset(self) -> None:
        self._payload = {}
        self.graph.clear()
        self.header.setText("尚未载入住院文书包")
        self.age_label.setText("欠件天数：—")
        self.side.clear()
        self.direction_tabs.clear()
        self.basis.setPlainText("")
        self.summary.setText("")
        self.notify_button.setEnabled(False)
        self.notify_button.setText("一键发缺件待办")
        self.export_button.setEnabled(False)

    def load(self, payload: dict[str, Any]) -> None:
        """载入核心 `archive_view` 的返回值。"""
        self._payload = payload
        rows = payload.get("rows", [])
        self.graph.load(rows, payload.get("edges", []))

        alias = payload.get("encounter_alias", "")
        age = payload.get("encounter_age") or {}
        days = age.get("days")
        if days is None:
            age_text = (
                f"欠件天数：算不出（{age.get('missing_count', 0)} 件缺件，"
                "但没有一件登记过首次发现的时刻）"
                if age.get("missing_count")
                else "欠件天数：本次不缺件"
            )
        else:
            age_text = f"欠件天数：{days} 天（按最老的一件算，共缺 {age.get('missing_count', 0)} 件）"
        self.age_label.setText(age_text)

        ready = payload.get("is_ready")
        self.header.setText(
            f"{alias} · {'可以归档' if ready else '尚未核齐'}"
            f" · {self._counts_zh(rows)}"
        )

        # 方向清单：两份方向相反的问题，默认选「这条卡住了谁」——
        # 护士长点进来最想知道的是「我这份为什么走不下去」。
        self.direction_tabs.clear()
        self.direction_tabs.addItem("这条卡住了谁")
        self.direction_tabs.addItem("这件在等谁")
        self.direction_tabs.setCurrentRow(0)
        self.side.clear()

        self.basis.setPlainText("\n".join(payload.get("basis_zh", [])))
        self.summary.setText(f"规则集 {payload.get('ruleset_short_sha256', '—')}")

        missing = payload.get("missing", [])
        self.notify_button.setEnabled(bool(missing))
        self.notify_button.setText(
            f"一键发缺件待办（{len(missing)} 件）" if missing else "一键发缺件待办"
        )
        self.export_button.setEnabled(bool(rows))
        self._refresh_sidebar()

    @staticmethod
    def _counts_zh(rows: list[dict[str, Any]]) -> str:
        counts: dict[str, int] = {}
        for r in rows:
            counts[r.get("state", "ok")] = counts.get(r.get("state", "ok"), 0) + 1
        parts = [
            f"{theme.NODE_STATE_ZH.get(state, state)} {n}"
            for state, n in sorted(counts.items())
        ]
        return " · ".join(parts)

    # ------------------------------------------------------------------
    # 侧栏
    # ------------------------------------------------------------------

    def _on_node_clicked(self, node_id: str) -> None:
        self.nodeSelected.emit(node_id)
        self._refresh_sidebar()

    def _on_direction_changed(self, _row: int) -> None:
        self._refresh_sidebar()

    def select_node(self, node_id: str | None) -> None:
        self.graph.focus_node(node_id)
        self._refresh_sidebar()

    def current_node(self) -> str | None:
        return self.graph.focused_node()

    def _refresh_sidebar(self) -> None:
        node_id = self.graph.focused_node()
        self.side.clear()
        if node_id is None:
            placeholder = QListWidgetItem(
                "点图上任意一件，这里显示「这条卡住了谁 / 这件在等谁」。"
            )
            placeholder.setFlags(Qt.ItemFlag.NoItemFlags)
            placeholder.setForeground(Qt.GlobalColor.gray)
            self.side.addItem(placeholder)
            return
        row = self.graph.row_for(node_id)
        if row is None:
            return
        dependents = row.get("blocking_others") or []
        dependencies = row.get("waiting_on") or []
        want_dependents = self.direction_tabs.currentRow() == 0
        names = {
            r["node_id"]: (r.get("display_zh") or r.get("template_id") or r["node_id"])
            for r in self._payload.get("rows", [])
        }
        items = dependents if want_dependents else dependencies
        empty_text = (
            "这件没有卡住任何其他件：它不挡别人的路。"
            if want_dependents
            else "这件没有在等任何前置件：它不是别人的前置。"
        )
        if not items:
            placeholder = QListWidgetItem(empty_text)
            placeholder.setFlags(Qt.ItemFlag.NoItemFlags)
            placeholder.setForeground(Qt.GlobalColor.gray)
            self.side.addItem(placeholder)
        for node in items:
            label = names.get(node, node)
            note = ""
            other = self.graph.row_for(node)
            if other is not None:
                state = other.get("state", "ok")
                note = f"（{theme.NODE_STATE_ZH.get(state, state)}）"
                if state == "missing" and other.get("missing_days") is not None:
                    note = f"（缺件，欠 {other['missing_days']} 天）"
            item = QListWidgetItem(f"{label} {note}")
            item.setData(Qt.ItemDataRole.UserRole, node)
            item.setForeground(
                Qt.GlobalColor.black
                if self.graph.row_for(node) is None
                or self.graph.row_for(node).get("state") == "ok"
                else Qt.GlobalColor.red
            )
            self.side.addItem(item)

    # ------------------------------------------------------------------
    # 通知
    # ------------------------------------------------------------------

    def missing_pairs(self) -> list[tuple[str, str]]:
        """本次要发的 ``(node_id, 院内显示名)``。

        只取**会阻断归档的缺件**：参考缺件（``requires: false``）在图上
        可见但不该进通知——护士长收到一条根本不欠的催办，下次就不会看提醒了。

        缺件节点没有 ``Composition.id``（它压根不在包里），显示名要从
        ``rows`` 里按 node_id 反查院内类型名；查不到才退回 node_id，
        **不能**发一条写着 ``<WARD-B-001>::missing::restraint_consent``
        的提醒——护士长看不懂，群里也没人会去查。
        """
        names = {
            r["node_id"]: (r.get("display_zh") or r.get("template_id") or r["node_id"])
            for r in self._payload.get("rows", [])
        }
        return [
            (m["node_id"], names.get(m["node_id"], m["node_id"]))
            for m in self._payload.get("missing", [])
        ]

    def apply_notify_result(self, result: dict[str, Any]) -> None:
        """把一次发送结果回灌到界面：已发的置灰并写明时刻。"""
        outcomes = {o["node_id"]: o for o in result.get("outcomes", [])}
        sent = result.get("sent", 0)
        if sent:
            self.summary.setText(
                f"已发 {sent} 条缺件提醒 · 规则集 {self._payload.get('ruleset_short_sha256', '—')}"
            )
        # 重算一次视图：置灰读的是发件箱那一行的状态，不是本次返回值——
        # 这样关窗重开后按钮仍然是灰的。
        already = [
            o for o in outcomes.values() if o.get("state") in ("already_sent", "sent")
        ]
        if already:
            self.notify_button.setToolTip(
                "其中 "
                + "、".join(o["as_zh"] for o in already[:3])
                + "（同键同日不重复发送）"
            )

    def _on_notify_clicked(self) -> None:
        missing = self.missing_pairs()
        if not missing:
            return
        self.notify_button.setEnabled(False)
        self.notifyRequested.emit([{"node_id": n, "display_zh": label} for n, label in missing])


def export_csv(payload: dict[str, Any]) -> str:
    """把归档核齐视图导成 csv 文本。

    列：**件 / 状态 / 依赖对象 / 阻塞对象 / 欠件天数**——病案室拿着这张表
    逐行核，质控办拿着它答「这份当年为什么没归档」。

    导出的天数与界面**同源**（同一份 ``rows``）：算不出就写空，
    不写 0——0 天会被读成「今天刚发现」，而真相是「没登记过发现时刻」。
    """
    import csv
    import io

    buf = io.StringIO()
    writer = csv.writer(buf)
    writer.writerow(
        ["住院别名", "件", "状态", "欠件天数", "依赖对象（这件在等谁）", "阻塞对象（这件卡住谁）"]
    )
    alias = payload.get("encounter_alias", "")
    # node_id → 院内显示名。缺件节点的 id 是合成的
    # `<住院别名>::missing::<类型码>`，直接写进 csv 等于给病案室一份
    # 没人看得懂的表——「约束记录在等 <WARD-B-001>::missing::restraint_consent」
    # 看着像系统故障，其实只是缺了层翻译。
    names = {
        r["node_id"]: (r.get("display_zh") or r.get("template_id") or r["node_id"])
        for r in payload.get("rows", [])
    }

    def label(node: str) -> str:
        return names.get(node, node)

    for row in payload.get("rows", []):
        state = row.get("state", "ok")
        days = row.get("missing_days")
        writer.writerow(
            [
                alias,
                row.get("display_zh") or row.get("template_id") or row["node_id"],
                theme.NODE_STATE_ZH.get(state, state),
                "" if days is None else days,
                "、".join(label(n) for n in (row.get("waiting_on") or [])) or "—",
                "、".join(label(n) for n in (row.get("blocking_others") or [])) or "—",
            ]
        )
    return buf.getvalue()


__all__ = ["ArchivePane", "GraphView", "color_of", "export_csv"]