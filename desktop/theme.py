"""QSS 样式表与配色常量。

四个主色来自 `spec.md` §8 视觉目标，与 `docs/concepts/visual-target.md` 一致：

- ``#1F6FEB`` 可操作（主按钮、可点条目）
- ``#D64545`` 缺件 / 阻断
- ``#2DA44E`` 达标
- ``#F6F8FA`` 窗口底

**约束**：只用 Qt 现有控件 + QSS 能做出的效果。不引 WebGL、不引动画曲线，
也不引第三方主题库——护士长科室的机器上装不了那些，而界面必须能跑。
"""

from __future__ import annotations

#: 可操作（主按钮、可点条目、选中态）
PRIMARY = "#1F6FEB"
#: 缺件 / 阻断
DANGER = "#D64545"
#: 达标 / 无阻断
OK = "#2DA44E"
#: 窗口底
BG = "#F6F8FA"

#: 次级文字（依据文案、灰字提示）
MUTED = "#57606A"
#: 卡片描边
BORDER = "#D0D7DE"

#: 被连带阻塞的件（自己没缺，但它下游的前置件缺了）。
#: **不是**缺件红：缺件是「你要去补」，连带阻塞是「等上游补齐就好」，
#: 两种颜色混用会让护士长对着一件并不欠自己的文书去催人。
BLOCKED = "#E3A008"
#: 绑定不唯一 / 未绑定 —— 需要**人工确认是哪一份**，工具不猜。
AMBIGUOUS = "#8B949E"
#: 归档核齐视图里「已核齐」的绿，比 ``OK`` 略深一档（底色上要压得住节点框）。
ARCHIVE_OK = "#1F7A3D"

#: 节点四态 → 颜色。**唯一**的映射表：界面各处上色都查这一张，
#: 散落各处的 if/else 迟早让「缺件红」在某个角落变成别的颜色。
NODE_COLORS = {
    "missing": DANGER,
    "blocked": BLOCKED,
    "ambiguous": AMBIGUOUS,
    "ok": ARCHIVE_OK,
}

#: 节点四态 → 中文标签（tooltip 与图例用）。
NODE_STATE_ZH = {
    "missing": "缺件",
    "blocked": "被连带阻塞",
    "ambiguous": "需人工确认",
    "ok": "已收齐",
}

#: 违规段落底色：`#D64545` 的低饱和版本，长段落不刺眼。
VIOLATION_BG = "#FCE8E8"

#: 码与版本哈希用等宽字体，正文用系统无衬线。
MONO_FAMILY = "Menlo, Consolas, monospace"

QSS = f"""
QWidget {{
    background: {BG};
    color: #1F2328;
    font-size: 13px;
}}
QTabWidget::pane {{
    border: 1px solid {BORDER};
    background: #FFFFFF;
}}
QTabBar::tab {{
    background: transparent;
    padding: 8px 20px;
    margin-right: 4px;
    border-bottom: 2px solid transparent;
}}
QTabBar::tab:selected {{
    color: {PRIMARY};
    border-bottom: 2px solid {PRIMARY};
    font-weight: 600;
}}
QListWidget {{
    background: #FFFFFF;
    border: 1px solid {BORDER};
    /* 行高留宽松：护士长连着看十几行时挤在一起会漏看徽标 */
    outline: none;
}}
QListWidget::item {{
    padding: 10px 12px;
    border-bottom: 1px solid #EAEEF2;
}}
QListWidget::item:selected {{
    background: #EAF1FD;
    color: #1F2328;
}}
/* 达标徽标：左栏「这份没有阻断项」时用绿色，缺件时用 DANGER */
QLabel#badge-ok {{
    color: {OK};
    font-size: 12px;
}}
QLabel#badge-danger {{
    color: {DANGER};
    font-size: 12px;
    font-weight: 600;
}}
/* 已审角标：灰色 —— 它是**进度**不是**结论**，不能跟缺件红一个视觉重量 */
QLabel#badge-done {{
    color: {MUTED};
    font-size: 11px;
}}
QLabel#dropzone {{
    border: 2px dashed {BORDER};
    border-radius: 6px;
    padding: 24px;
}}
QPushButton {{
    background: {PRIMARY};
    color: #FFFFFF;
    border: none;
    border-radius: 4px;
    padding: 7px 16px;
}}
QPushButton:hover {{ background: #1A5FD0; }}
QPushButton:disabled {{
    background: #AFB8C1;
    color: #F0F3F6;
}}
QLabel#statusbar {{
    background: #FFFFFF;
    border-top: 1px solid {BORDER};
    padding: 6px 12px;
    color: {MUTED};
}}
QLabel#code {{
    font-family: {MONO_FAMILY};
    color: {DANGER};
    font-weight: 600;
}}
QLabel#basis {{
    color: {MUTED};
    font-size: 12px;
}}
QTextEdit#doccard, QListWidget#findings {{
    background: #FFFFFF;
    border: 1px solid {BORDER};
}}
"""
