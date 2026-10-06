"""精神科住院文书审查系统 —— PySide6 桌面壳。

壳只做三件事：摆控件、把重活丢给 `QtConcurrent`、把核心返回的结构化结果
画成界面。**所有判定都在 Rust 侧**（`psychdoc_ffi` 的 5 个同步入口），
壳里没有任何规则、没有任何阈值，也没有任何"顺手判断一下"。

这一层刻意不引入业务状态：件级状态机、幂等键、依赖图正反向全在核心，
界面只是它们的一张可点开的视图。
"""

from __future__ import annotations

__all__ = ["__version__", "DISPLAY_NAME"]

__version__ = "0.1.0"

#: 窗口标题与托盘 tooltip 用的中文产品名。
DISPLAY_NAME = "精神科住院文书审查系统"
