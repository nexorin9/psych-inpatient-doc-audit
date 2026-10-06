"""Task 32 · 拖放与批量队列的 Python 侧契约。

Rust 侧的行为（发现 / 聚合 / 落库）在
`core/crates/psychdoc-parse/tests/drop_queue_scan.rs` 与
`core/crates/psychdoc-store/tests/queue_persistence.rs` 里已经钉死。
本文件补上**壳这一层**的三件事：

1. `scan_queue` 的返回值确实落成 Python 的 dict / list / int / str；
2. 拖放事件走的是真实 `QDropEvent`（不是直接调回调），且四条入口
   （拖放 / 按钮 / 菜单 / 命令行）汇到同一个 `load_paths`；
3. 关窗重开后队列与已审进度还在——这是常驻工具最该守住的一条。

GUI 用例走 `QT_QPA_PLATFORM=offscreen`（`conftest.py` 已设），
不做像素级断言：渲染好不好看是 Task 14 截图人工核的事。
"""

from __future__ import annotations

import json
import os
import shutil
import tempfile
from pathlib import Path

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

core_bridge = pytest.importorskip(
    "desktop.core_bridge",
    reason=(
        "核心绑定未构建。构建方式："
        "cargo build -p psychdoc-ffi --features extension-module，"
        "再把 core/target/debug/libpsychdoc_ffi.{dylib,so} 拷到 bindings/ 下"
    ),
)
pytest.importorskip("PySide6", reason="未安装 PySide6，桌面壳用例跳过")

from PySide6.QtCore import (  # noqa: E402
    QDeadlineTimer,
    QEventLoop,
    QMimeData,
    QPoint,
    Qt,
    QTimer,
    QUrl,
)
from PySide6.QtGui import QDropEvent  # noqa: E402
from PySide6.QtWidgets import QApplication, QLabel  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent

#: 固定的注入时刻：核心不读系统时钟，测试也不该让它读到真实时间，
#: 否则「同一份目录两次扫描落库记录一致」这类断言没法写。
AT = "2026-03-02T08:00:00+08:00"


@pytest.fixture(scope="session")
def app():
    existing = QApplication.instance()
    yield existing or QApplication([])


@pytest.fixture
def ruleset_yaml() -> str:
    return core_bridge.default_ruleset_path().read_text(encoding="utf-8")


@pytest.fixture
def store_path(tmp_path, monkeypatch) -> str:
    """每个用例一个独立库：不许往真实用户数据目录里写东西。"""
    p = tmp_path / "store" / "psychdoc.sqlite3"
    monkeypatch.setenv("PSYCHDOC_STORE", str(p))
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    return str(p)


def scan(roots, ruleset_yaml, store_path, *, at=AT, persist=True):
    return core_bridge.scan_queue(roots, ruleset_yaml, at, store_path, persist)


def drain(window, timeout_ms: int = 60_000) -> None:
    """转事件循环直到窗口上**没有待收的 job**（或超时）。

    必须循环而不是「等一轮」：批量审查是**依次**跑的（见
    `MainWindow._start_next_in_batch`），一条收完才起下一条。等一轮
    只等到第一条，界面上后面 17 条还没开始。
    """
    deadline = QDeadlineTimer(timeout_ms)
    while window._jobs:
        loop = QEventLoop()
        jobs = list(window._jobs)
        for job in jobs:
            job.done.connect(lambda _p: loop.quit())
            job.failed.connect(lambda _e: loop.quit())
        # 每轮给一个上限，防止某个 job 永不收完时整个用例挂死
        QTimer.singleShot(10_000, loop.quit)
        loop.exec()
        if not deadline.hasExpired() is False:  # pragma: no cover - 超时兜底
            break
        if window._jobs == jobs:
            # 这一轮一个都没收（超时），再等下去也是白等
            break


@pytest.fixture
def window(app):
    from desktop.main_window import MainWindow

    w = MainWindow()
    yield w
    w.close()


# ---------------------------------------------------------------------------
# 1. scan_queue 的返回类型（Rust → Python 的搬运）
# ---------------------------------------------------------------------------


def test_scan_queue_returns_plain_python_objects(store_path, ruleset_yaml):
    v = scan([str(ROOT / "fixtures")], ruleset_yaml, store_path)

    assert isinstance(v, dict) and v["ok"] is True
    assert isinstance(v["entries"], list) and v["entries"]
    e = v["entries"][0]
    for key, typ in [
        ("encounter_alias", str),
        ("ward", str),
        ("doc_count", int),
        ("missing_count", int),
        ("doc_ids", list),
        ("source_files", list),
        ("badge_zh", str),
        ("is_complete", bool),
    ]:
        assert isinstance(e[key], typ), f"{key} 应是 {typ.__name__}，实际 {type(e[key])}"
    assert isinstance(v["file_skips"], list)
    assert isinstance(v["totals"], dict)
    assert isinstance(v["is_balanced"], bool)
    assert len(v["ruleset_short_sha256"]) == 12


def test_scan_queue_refuses_a_bad_ruleset_with_a_code_not_a_degraded_pass(store_path):
    with pytest.raises(core_bridge.CoreError) as exc:
        scan([str(ROOT / "fixtures")], "templates: [", store_path)
    assert exc.value.code == "E_RULESET_INVALID"
    # 关键：不降级为「0 条命中 = 全部通过」
    assert exc.value.retryable is False


def test_scan_queue_refuses_dropping_nothing(store_path, ruleset_yaml):
    with pytest.raises(core_bridge.CoreError) as exc:
        scan([], ruleset_yaml, store_path)
    assert exc.value.code == "E_QUEUE_EMPTY"


def test_scan_queue_refuses_a_mistyped_path_with_an_io_code(store_path, ruleset_yaml):
    """路径打错 ≠ 文件夹是空的。

    降级成「这个文件夹里没有能认的文书包」的话，护士长会以为该导的东西
    没导，而不是「路径打错了」。
    """
    with pytest.raises(core_bridge.CoreError) as exc:
        scan(["/不存在的导出目录"], ruleset_yaml, store_path)
    assert exc.value.code == "E_IO"


def test_misc_files_come_back_as_a_skip_list_not_as_a_silent_drop(tmp_path, store_path, ruleset_yaml):
    src = tmp_path / "ward-a"
    src.mkdir()
    (src / "WARD-A-001-CONSENT.json").write_text(
        (ROOT / "fixtures" / "ward-02-restraint-set.json").read_text(encoding="utf-8"),
        encoding="utf-8",
    )
    (src / "扫描件.pdf").write_bytes(b"%PDF-1.4")
    (src / "台账.xlsx").write_text("PK")
    (src / "半截导出.json").write_text('{"resourceType":"Bundle","entry":[{')

    v = scan([str(src)], ruleset_yaml, store_path)
    assert v["totals"]["items"] == 4, "成套 4 件照常解析出来"
    names = {s["name"] for s in v["file_skips"]}
    assert names == {"扫描件.pdf", "台账.xlsx", "半截导出.json"}
    for s in v["file_skips"]:
        assert s["reason_zh"], "每个跳过项都要有中文原因"
        assert isinstance(s["retryable"], bool)


def test_an_empty_folder_reports_why_instead_of_zero_findings(tmp_path, store_path, ruleset_yaml):
    empty = tmp_path / "空文件夹"
    empty.mkdir()
    v = scan([str(empty)], ruleset_yaml, store_path)
    assert v["entries"] == []
    assert v["empty_reason_zh"], "空文件夹必须给出原因，不能当成「0 条命中 = 全部合规」"
    assert "没有任何文件" in v["empty_reason_zh"]
    # 空队列不落库
    assert v["persisted"] is False
    assert v["queue_id"] is None


# ---------------------------------------------------------------------------
# 2. 拖放事件（真 QDropEvent，不是直接调回调）
# ---------------------------------------------------------------------------


def test_the_drop_zone_accepts_a_real_folder_drop(window, app):
    mime = QMimeData()
    mime.setUrls([QUrl.fromLocalFile(str(ROOT / "fixtures"))])
    event = QDropEvent(
        QPoint(10, 10),
        Qt.DropAction.CopyAction,
        mime,
        Qt.MouseButton.LeftButton,
        Qt.KeyboardModifier.NoModifier,
    )

    received: list[list[str]] = []
    window.drop.pathsDropped.connect(received.append)

    assert window.drop.acceptDrops() is True
    window.drop.dragEnterEvent(event)
    assert event.isAccepted(), "有本地路径悬进来就该接受"

    window.drop.dropEvent(event)
    assert received == [[str(ROOT / "fixtures")]], "拖放要走 load_paths 那条路"


def test_the_drop_zone_refuses_a_drag_that_carries_no_local_file(window):
    """远程 URL / 纯文本拖进来要明确拒绝，而不是收下一个空列表。"""
    mime = QMimeData()
    mime.setUrls([QUrl("https://example.invalid/export.zip")])
    event = QDropEvent(
        QPoint(10, 10),
        Qt.DropAction.CopyAction,
        mime,
        Qt.MouseButton.LeftButton,
        Qt.KeyboardModifier.NoModifier,
    )
    received: list[list[str]] = []
    window.drop.pathsDropped.connect(received.append)
    window.drop.dragEnterEvent(event)
    assert event.isAccepted() is False
    assert received == []


def test_the_drop_zone_accepts_several_folders_at_once(window, app, store_path, monkeypatch, ruleset_yaml):
    a = ROOT / "fixtures"
    b = Path(tempfile.mkdtemp()) / "另一个病区"
    b.mkdir(parents=True, exist_ok=True)
    shutil.copy(ROOT / "fixtures" / "ward-01-minimal.json", b / "WARD-B-001-CONSENT.json")
    monkeypatch.setattr(
        "desktop.main_window._now_rfc3339", lambda: AT, raising=False
    )
    try:
        window.load_paths([str(a), str(b)])
        drain(window)
        app.processEvents()
        aliases = [
            window.encounter_list.item(i).data(0x0100)["encounter_alias"]
            for i in range(window.encounter_list.count())
        ]
        assert "WARD-B-001" in aliases, "一次拖两个文件夹，两边都要进队列"
        assert len(aliases) > 1
    finally:
        shutil.rmtree(b.parent, ignore_errors=True)


def test_collapsing_the_drop_zone_turns_drag_accept_off(window):
    """收起后还留着 acceptDrops 会让拖放**看起来**能用却毫无反应。"""
    assert window.drop.acceptDrops() is True
    window.drop.collapse()
    assert window.drop.acceptDrops() is False


def test_a_failed_load_puts_the_drop_zone_back(window, monkeypatch):
    """载入失败后不能留一个收起着的空窗口——护士长不知道该重试还是求助。"""
    monkeypatch.setattr(
        "desktop.main_window.QMessageBox.critical", lambda *a, **k: None
    )
    monkeypatch.setattr(
        "desktop.core_bridge.scan_queue",
        lambda *a, **k: (_ for _ in ()).throw(
            core_bridge.CoreError("E_IO", "读不到导出目录")
        ),
    )
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app = QApplication.instance()
    app.processEvents()
    assert window.drop.isVisibleTo(window) is True
    assert window.drop.acceptDrops() is True


# ---------------------------------------------------------------------------
# 3. 队列：左栏 + 缺件徽标 + 已跳过清单
# ---------------------------------------------------------------------------


def test_loading_a_folder_fills_the_queue_the_badges_and_the_skip_list(
    window, app, store_path, monkeypatch, ruleset_yaml
):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()

    assert window.encounter_list.count() > 0, "左栏应有住院条目"
    rows = [window.encounter_list.item(i).data(0x0100) for i in range(window.encounter_list.count())]
    assert all(r["encounter_alias"].startswith("WARD-") for r in rows)

    # 徽标文案说的是「缺件 N」，不是「不合规 N」；病区认不出时留空不编
    for r in rows:
        assert "件" in r["badge_zh"]
        if r["missing_count"] == 0:
            assert "达标" in r["badge_zh"]
        else:
            assert "缺件" in r["badge_zh"]

    # 缺件最多的排最前（与核心给���顺序一致，界面不自己重排）
    missing = [r["missing_count"] for r in rows]
    assert missing == sorted(missing, reverse=True)

    # 底栏：住院次数 / 件数 / 缺件数 / 规则集短哈希。
    # 载入后会自动审当前选中的那一条，所以此刻底栏可能已经是批量进度行
    # （`批量审查 · 已审 k / 共 N 次住院`）——那一行同样带住院次数与总数，
    # 但不带规则集短哈希，因此先验总数、再单独验摘要行的形态。
    status = window.status_label.text()
    assert "次住院" in status

    window.encounter_list.clearSelection()
    window._on_selection_changed()
    window._update_status(loaded=True, totals={"encounters": 18, "items": 52,
                                                "missing": 4, "skipped_files": 1,
                                                "ruleset_short_sha256": "e4eda0c79c95"})
    summary_line = window.status_label.text()
    assert "18 次住院" in summary_line and "共 52 件" in summary_line
    assert "缺件 4" in summary_line
    assert "已跳过 1 个文件" in summary_line, summary_line
    assert len(summary_line.split("规则集")[1].strip()) == 12

    # 「已跳过」清单可见且非空（fixtures/README.md 认不出来）
    assert window.skipped.list.count() >= 1
    text = " ".join(window.skipped.list.item(i).text() for i in range(window.skipped.list.count()))
    assert "README.md" in text


def test_the_queue_survives_closing_and_reopening_the_window(window, app, store_path, monkeypatch, ruleset_yaml):
    """关窗重开：队列还在，已审进度还在。

    这是常驻工具最该守住的一条：护士长上午审到第 8 次住院，中午去查房，
    回来发现窗口记的是「尚未载入」，等于从头再来。
    """
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()
    before = [window.encounter_list.item(i).data(0x0100) for i in range(window.encounter_list.count())]
    assert before, "先得有队列"
    window.close()

    from desktop.main_window import MainWindow

    again = MainWindow()
    try:
        assert again.restore_last_queue() is True, "重开窗口应还原上次的队列"
        restored = [
            again.encounter_list.item(i).data(0x0100)["encounter_alias"]
            for i in range(again.encounter_list.count())
        ]
        assert restored == [r["encounter_alias"] for r in before], "左栏顺序也要原样还原"
        # 队列骨架不冒充旧诊断：右栏仍是空的，等用户点开某一次住院
        assert again.findings.list.count() == 0
        assert "已恢复上次队列" in again.status_label.text()
    finally:
        again.close()


def test_restoring_with_no_saved_queue_stays_in_the_starting_state(window, store_path):
    """没落过队列是首次启动的正常情形，不是错误——窗口留在起始态的拖放区。"""
    assert window.restore_last_queue() is False
    assert window.encounter_list.count() == 0
    assert window.drop.isVisibleTo(window) is True
    assert window.drop.acceptDrops() is True


# ---------------------------------------------------------------------------
# 4. 多选批量审查：选中 N 条 → 逐条审 → 进度 (已审/总数)
# ---------------------------------------------------------------------------


def _select_rows(window, rows: list[int]) -> None:
    window.encounter_list.clearSelection()
    for r in rows:
        window.encounter_list.item(r).setSelected(True)


def test_the_batch_button_is_disabled_until_something_is_selected(window, app, store_path, monkeypatch, ruleset_yaml):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()

    window.encounter_list.clearSelection()
    window._on_selection_changed()
    assert window.batch_button.isEnabled() is False, "没选中就不该能点批量审查"

    _select_rows(window, [0])
    window._on_selection_changed()
    assert window.batch_button.isEnabled() is True
    assert "1" in window.batch_button.text()


def test_selecting_several_encounters_reviews_them_one_by_one_with_progress(
    window, app, store_path, monkeypatch, ruleset_yaml
):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()

    # 取三次住院：前两条各有诊断，第三条挑个后面排的
    n = window.encounter_list.count()
    assert n >= 3
    _select_rows(window, [0, 1, 2])
    assert len(window._selected_aliases()) == 3

    seen: list[str] = []
    original = window._update_progress

    def spy(done, total):
        seen.append(f"{done}/{total}")
        original(done, total)

    monkeypatch.setattr(window, "_update_progress", spy)

    window._on_batch_review()
    drain(window)
    app.processEvents()

    # 进度是逐条推进的真实值，不是最后一次性从 0 跳到 3
    assert seen[:3] == ["0/3", "1/3", "2/3"], f"应逐条推进，实际 {seen}"
    assert "3/3" in seen


def test_reviewing_one_encounter_shows_only_that_encounters_findings(
    window, app, store_path, monkeypatch, ruleset_yaml
):
    """审第 3 次住院，右栏不该躺着全病区的问题——这是这个工具最不该犯的错。"""
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()

    # 找一条确实有诊断的住院：直接用核心核一遍，避免界面里猜
    rs = ruleset_yaml
    batch = core_bridge.review_batch(
        str(ROOT / "fixtures"), rs, str(core_bridge.default_cache_path())
    )
    per_alias: dict[str, set[str]] = {}
    doc_alias = {}
    for row in window._encounters:
        for d in row["doc_ids"]:
            doc_alias[d] = row["encounter_alias"]
    for d in batch["summary"]["diagnostics"]:
        alias = doc_alias.get(d["doc_id"])
        if alias:
            per_alias.setdefault(alias, set()).add(d["code"])

    target = next(a for a, codes in per_alias.items() if codes)
    idx = next(i for i in range(window.encounter_list.count())
               if window.encounter_list.item(i).data(0x0100)["encounter_alias"] == target)

    _select_rows(window, [idx])
    window._on_batch_review()
    drain(window)
    app.processEvents()

    shown = [
        window.findings.list.item(i).data(0x0100)
        for i in range(window.findings.list.count())
    ]
    assert shown, "选中的那次住院应当有诊断"
    codes = {d["code"] for d in shown}
    assert codes <= per_alias[target], f"右栏混进了别的住院：{codes - per_alias[target]}"
    # 别名反查走的是队列里的 doc_ids，不是按 doc_id 前缀猜
    for d in shown:
        assert doc_alias.get(d["doc_id"]) == target


def test_reviewing_marks_the_queue_as_reviewed_and_survives_a_restart(
    window, app, store_path, monkeypatch, ruleset_yaml
):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    app.processEvents()
    _select_rows(window, [0])
    window._on_batch_review()
    drain(window)
    app.processEvents()

    p = window._progress()
    assert p["reviewed"] >= 1, f"审完应落进度，实际 {p}"
    assert "已审" in window.status_label.text()

    window.close()
    from desktop.main_window import MainWindow

    again = MainWindow()
    try:
        assert again.restore_last_queue() is True
        reviewed = [
            e["encounter_alias"] for e in again._encounters if e.get("reviewed_at")
        ]
        assert reviewed, "关窗重开后已审进度还在"
        assert len(reviewed) == p["reviewed"]
        # 已审的条目左栏要有一枚灰色角标，一眼认得出来
        item = next(
            again.encounter_list.item(i)
            for i in range(again.encounter_list.count())
            if again.encounter_list.item(i).data(0x0100)["encounter_alias"] == reviewed[0]
        )
        widget = again.encounter_list.itemWidget(item)
        assert widget is not None, "已审条目应仍带徽标控件"
        badge = widget.findChild(QLabel, "badge-done")
        assert badge is not None, "已审条目应有灰色角标"
        assert badge.text().startswith("已审")
    finally:
        again.close()


def test_marking_an_encounter_that_is_not_in_the_queue_is_refused(
    window, store_path, monkeypatch, ruleset_yaml
):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    QApplication.instance().processEvents()
    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.mark_queue_reviewed(window._queue_id, ["不存在的住院"], AT)
    assert exc.value.code == "E_QUEUE_UNKNOWN_ENCOUNTER"


def test_marking_nothing_is_refused(window, store_path, monkeypatch, ruleset_yaml):
    monkeypatch.setattr("desktop.main_window._now_rfc3339", lambda: AT, raising=False)
    window.load_paths([str(ROOT / "fixtures")])
    drain(window)
    QApplication.instance().processEvents()
    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.mark_queue_reviewed(window._queue_id, [], AT)
    assert exc.value.code == "E_QUEUE_NOTHING_SELECTED"


# ---------------------------------------------------------------------------
# 5. 「已跳过」清单存的是原样 JSON（重开后逐字还原）
# ---------------------------------------------------------------------------


def test_the_skip_list_is_stored_verbatim_and_restored(store_path, ruleset_yaml):
    v = scan([str(ROOT / "fixtures")], ruleset_yaml, store_path)
    assert v["file_skips"], "fixtures 里应有认不出来的文件"
    snap = core_bridge.load_queue()
    assert snap is not None
    assert snap["file_skips"] == v["file_skips"], "落库的必须是原样 JSON，逐字还原"
    assert snap["totals"] == {
        k: v["totals"][k] for k in snap["totals"]
    }