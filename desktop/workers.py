"""把核心的同步调用丢进线程池，结果回主线程再碰控件。

**线程契约**（本项目最重要的一条界面纪律）：

- 核心 5 个入口**全部同步、无内部线程**。在主线程直接调，200 件的包
  会把窗口冻住；
- 因此重活一律走 `QThreadPool` + `QRunnable`，工作线程里**只**调核心、
  **只**返回普通 Python 对象（dict / list / None），
  **不碰任何 QWidget、不碰 QPixmap、不碰 QApplication 状态**；
- 完成的回调用 `Signal`（Qt 的跨线程队列连接）发回主线程，
  界面刷新只发生在那里。

用 `QRunnable` + `Signal` 而不是 `QtConcurrent.run` + `QFutureWatcher`：
PySide6 6.11 已移除 `QtConcurrent.run` 与泛型 `QFutureWatcher`
（只剩 `QFutureVoid` 等特化类），而这条纪律本来就该由
「信号 + 队列连接」显式承担——`QFutureWatcher.finished` 的主线程保证
是隐式的，出了问题很难查。

`is_main_thread` / `assert_main_thread_only` 把契约变成可测的：
任何在工作线程里刷新界面的写法都会立刻炸，而不是等用户点第二下才崩。
"""

from __future__ import annotations

from typing import Any, Callable

from PySide6.QtCore import (
    QCoreApplication,
    QObject,
    QRunnable,
    QThread,
    QThreadPool,
    Signal,
)

from . import core_bridge


def is_main_thread() -> bool:
    """当前是否在 GUI 主线程。

    没有 `QApplication` 时（纯单元测试里 import 本模块）一律当作主线程：
    此时根本没有事件循环，"工作线程"这个概念不成立。
    """
    app = QCoreApplication.instance()
    if app is None:
        return True
    return QThread.currentThread() == app.thread()


def assert_main_thread_only(what: str) -> None:
    """断言当前在 GUI 主线程。工作线程里调用即抛错。"""
    if not is_main_thread():
        raise RuntimeError(
            f"{what} 试图在工作线程里执行。核心调用可以，但任何界面刷新必须"
            " 回到 done/failed 回调（主线程）。"
        )


class _Emitter(QObject):
    """跨线程回调用的中转站。

    `Signal` 声明在这个**独立 QObject** 上、连接到主线程的槽，
    Qt 会自动用队列连接投递，因此槽函数一定在主线程执行。
    """

    ok = Signal(object)
    err = Signal(object)

    def __init__(self) -> None:
        super().__init__()
        # 挂在当前线程（主线程）上，槽因此总在主线程跑。
        if QCoreApplication.instance() is not None:
            self.moveToThread(QThread.currentThread())


class _Task(QRunnable):
    """一次核心调用。`run()` 只跑核心、只搬结果，绝不碰界面。"""

    def __init__(self, emitter: _Emitter, fn: Callable[..., Any], args: tuple, kwargs: dict) -> None:
        super().__init__()
        self._emitter = emitter
        self._fn = fn
        self._args = args
        self._kwargs = kwargs
        self.setAutoDelete(True)

    def run(self) -> None:  # pragma: no cover - 由 Qt 线程池调用
        try:
            payload: Any = self._fn(*self._args, **self._kwargs)
        except core_bridge.CoreError as exc:
            # 错误在**工作线程**里捕获再带回主线程：
            # 让异常跨线程冒泡到 Qt 的事件循环是不受控的。
            self._emitter.err.emit(exc)
        except Exception as exc:  # noqa: BLE001 - 兜底也必须带码
            self._emitter.err.emit(core_bridge.CoreError("E_INTERNAL", str(exc)))
        else:
            self._emitter.ok.emit(payload)


class _BaseJob(QObject):
    """`CoreJob` / `ArchiveViewJob` / `PreviewJob` 的共同骨架。

    三个 job 分开而不是加个 `kind` 参数：两个标签页与预览面板的失败文案、
    刷新路径完全不同，合成一个 job 只会让 `if/else` 越写越长。
    """

    done = Signal(object)
    failed = Signal(object)

    def __init__(self, fn: Callable[..., Any], *args: Any, **kwargs: Any) -> None:
        super().__init__()
        self._emitter = _Emitter()
        # 队列连接：工作线程 emit → 主线程槽。
        self._emitter.ok.connect(self._on_ok)
        self._emitter.err.connect(self._on_err)
        self.finished = False
        self._pool = QThreadPool.globalInstance()
        self._pool.start(_Task(self._emitter, fn, args, kwargs))

    def _on_ok(self, payload: Any) -> None:
        assert_main_thread_only(type(self).__name__)
        self.finished = True
        self.done.emit(payload)

    def _on_err(self, exc: Any) -> None:
        assert_main_thread_only(type(self).__name__)
        self.finished = True
        self.failed.emit(exc)


class CoreJob(_BaseJob):
    """文书包审查用的一次核心调用。"""


class ArchiveViewJob(_BaseJob):
    """归档核齐视图的数据加载（第二个标签页）。"""

    ready = Signal(dict)
    failed = Signal(object)

    def __init__(self, bundle_text: str, ruleset_yaml: str, node_id: str) -> None:
        super().__init__(
            core_bridge.graph_both_directions, bundle_text, ruleset_yaml, node_id
        )

    def _on_ok(self, payload: Any) -> None:
        super()._on_ok(payload)
        self.ready.emit(payload)

    def _on_err(self, exc: Any) -> None:
        assert_main_thread_only("ArchiveViewJob")
        self.finished = True
        self.failed.emit(exc)


class PreviewJob(_BaseJob):
    """补写预览（双栏 diff）。

    与前两个分开的原因同上：预览要按 `fix_availability` 决定渲不渲染
    补写按钮，失败时还要把「只给依据」的码单独显示出来。
    """

    ready = Signal(dict)
    failed = Signal(object)

    def __init__(self, bundle_text: str, ruleset_yaml: str) -> None:
        super().__init__(core_bridge.preview_fixback, bundle_text, ruleset_yaml)

    def _on_ok(self, payload: Any) -> None:
        super()._on_ok(payload)
        self.ready.emit(payload)

    def _on_err(self, exc: Any) -> None:
        assert_main_thread_only("PreviewJob")
        self.finished = True
        self.failed.emit(exc)


class DocCardJob(_BaseJob):
    """文书卡片的数据加载（Task 33）。

    与 `PreviewJob` 分开：预览面板按 `preview.blocks` 渲染**全部**候选块，
    卡片按**当前选中的一条诊断**聚焦，两者刷新时机不同。
    合在一个 job 里会让「换一条诊断」把整份预览重算一遍。
    """

    ready = Signal(dict)
    failed = Signal(object)

    def __init__(self, input_path: str, ruleset_yaml: str, doc_id: str) -> None:
        super().__init__(core_bridge.doc_card, input_path, ruleset_yaml, doc_id)

    def _on_ok(self, payload: Any) -> None:
        super()._on_ok(payload)
        self.ready.emit(payload)

    def _on_err(self, exc: Any) -> None:
        assert_main_thread_only("DocCardJob")
        self.finished = True
        self.failed.emit(exc)


def run_core_job(fn: Callable[..., Any], *args: Any, **kwargs: Any) -> CoreJob:
    """起一个核心调用并返回 job（不阻塞调用方）。"""
    return CoreJob(fn, *args, **kwargs)


__all__ = [
    "ArchiveViewJob",
    "CoreJob",
    "DocCardJob",
    "PreviewJob",
    "assert_main_thread_only",
    "is_main_thread",
    "run_core_job",
]
