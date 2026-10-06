"""pytest 全局配置：把 `bindings/` 与项目根放进 `sys.path`。

`psychdoc_ffi` 是本机构建产物（`maturin develop` 或 `cargo build` 后拷到
`bindings/`），不是一个能 `pip install` 的普通包；测试必须在它缺席时
**明确跳过并说明怎么建**，而不是报一个 `ModuleNotFoundError` 了事。
"""

from __future__ import annotations

import os
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent
for p in (ROOT, ROOT / "bindings"):
    if str(p) not in sys.path:
        sys.path.insert(0, str(p))

# Qt 在无显示器的机器（CI、病区瘦机）上必须走 offscreen，
# 否则 QApplication 构造就 abort，测试无法运行。
os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
