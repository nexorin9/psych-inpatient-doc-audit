"""主窗口骨架与线程契约的 GUI 测试（offscreen）。

GUI 用例在 CI 与无显示器的病区瘦机上都要能跑，因此 `conftest.py` 把
`QT_QPA_PLATFORM` 设成 `offscreen`；这里**不**做像素级断言，只断言
「控件在、契约在」——渲染好不好看是 Task 14 截图人工核的事。

最要紧的一条是 `test_the_worker_callback_runs_on_the_main_thread`：
把「重活在工作线程、刷新在主线程」这条纪律变成可测的，而不是靠注释。
"""

from __future__ import annotations

import os
from pathlib import Path

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

pytest.importorskip("PySide6", reason="未安装 PySide6，桌面壳用例跳过")
core_bridge = pytest.importorskip("desktop.core_bridge")

from PySide6.QtCore import QEventLoop, QRunnable, QThread, QTimer  # noqa: E402
from PySide6.QtWidgets import QApplication  # noqa: E402

from desktop import theme  # noqa: E402
from desktop.main import preflight, writeback_hint  # noqa: E402
from desktop.workers import (  # noqa: E402
    CoreJob,
    assert_main_thread_only,
    is_main_thread,
)

ROOT = Path(__file__).resolve().parent.parent


class _ProbeTask(QRunnable):
    """最小 QRunnable：跑一个无参函数并置位，供线程断言用。

    PySide6 的 `QSemaphore` 只暴露 `acquire(n)` / `tryAcquire(n)` / `release(n)`
    ——没有 `wait()`。这里用 `tryAcquire(1, 30_000)` 带超时的轮询来等，
    免得测试在 worker 卡死时永远挂着。
    """

    def __init__(self, fn) -> None:
        super().__init__()
        self._fn = fn
        from PySide6.QtCore import QSemaphore

        self._sem = QSemaphore(0)
        self.setAutoDelete(True)

    def run(self) -> None:
        self._fn()
        self._sem.release()

    def wait_done(self, timeout_ms: int = 30_000) -> bool:
        return bool(self._sem.tryAcquire(1, timeout_ms))


@pytest.fixture(scope="session")
def app():
    existing = QApplication.instance()
    yield existing or QApplication([])


@pytest.fixture
def window(app):
    from desktop.main_window import MainWindow

    w = MainWindow()
    yield w
    w.close()


# ---------------------------------------------------------------------------
# 启动自检（纯函数，不建窗口）
# ---------------------------------------------------------------------------


def test_preflight_passes_with_a_shipped_ruleset():
    ok, message, code = preflight()
    assert ok is True, message
    assert code == 0


def test_preflight_refuses_to_start_without_a_ruleset(monkeypatch, tmp_path):
    monkeypatch.setenv("PSYCHDOC_RULESET", str(tmp_path / "nope.yaml"))
    ok, message, code = preflight()
    assert ok is False
    assert code == 2
    # 关键：缺规则集时明说「不会给全部通过」，不静默降级
    assert "E_RULESET_INVALID" in message
    assert "全部通过" in message


def test_writeback_hint_names_every_missing_credential_but_does_not_block(monkeypatch):
    for key in core_bridge.WRITE_ENV:
        monkeypatch.delenv(key, raising=False)
    hint = writeback_hint()
    for key in core_bridge.WRITE_ENV:
        assert key in hint
    assert "E_CFG_MISSING" in hint
    # 提示而不是拦截：只想看清单的护士长不该被逼着配密钥
    assert "不会静默降级" in hint


# ---------------------------------------------------------------------------
# 启动自检的逐项报告（Task 38）
# ---------------------------------------------------------------------------


def test_the_report_names_all_five_credentials_at_once(monkeypatch):
    for key in core_bridge.REQUIRED_ENV:
        monkeypatch.delenv(key, raising=False)
    report = core_bridge.startup_report()
    assert report["writeback_ready"] is False
    # 一次列全，而不是让运维改一条跑一次
    assert sorted(report["missing"]) == sorted(core_bridge.REQUIRED_ENV)
    assert report["network_calls"] == 0
    assert report["code"] == "E_CFG_MISSING"


def test_the_report_never_carries_a_credential_value(monkeypatch):
    # 这份报告要显示在窗口上，也可能被人截图发进工作群。
    secrets = {
        "EMR_BASE_URL": "https://emr.example-hospital.local/fhir/r4",
        "EMR_CLIENT_ID": "psychdoc-client",
        "EMR_CLIENT_SECRET": "s3cret-value",
        "EMR_TENANT_ID": "ward-a",
        "WECOM_WEBHOOK_URL": "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123",
    }
    for key, value in secrets.items():
        monkeypatch.setenv(key, value)
    rendered = core_bridge.startup_report()["render"]
    for value in secrets.values():
        assert value not in rendered, f"报告里漏出了凭据取值：{rendered}"
    # 但变量名要在——值守的人要照着它去改 .env
    for key in core_bridge.REQUIRED_ENV:
        assert key in rendered


def test_a_malformed_value_is_reported_as_a_shape_error_not_as_missing(monkeypatch):
    values = {
        "EMR_BASE_URL": "emr.example-hospital.local",  # 没头
        "EMR_CLIENT_ID": "psychdoc-client",
        "EMR_CLIENT_SECRET": "s3cret-value",
        "EMR_TENANT_ID": "ward-a",
        "WECOM_WEBHOOK_URL": "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123",
    }
    for key, value in values.items():
        monkeypatch.setenv(key, value)
    report = core_bridge.startup_report()
    # 填了但形态错 ≠ 没填：两类问题找的人不一样
    assert "EMR_BASE_URL" not in report["missing"]
    assert [item["key"] for item in report["invalid"]] == ["EMR_BASE_URL"]
    assert "http" in report["invalid"][0]["detail"]
    assert report["code"] == "E_CFG_INVALID"
    # 不得被悄悄纠正：没自动补 https://
    assert "应以 http://" in report["render"]


def test_a_blank_value_counts_as_missing_and_is_not_also_called_malformed(monkeypatch):
    # 复制 .env.example 之后忘了填，最常见的事故形状。
    monkeypatch.setenv("EMR_BASE_URL", "   ")
    report = core_bridge.startup_report()
    assert "EMR_BASE_URL" in report["missing"]
    assert not any(
        item["key"] == "EMR_BASE_URL" for item in report["invalid"]
    ), "缺失项不该同时又被报成形态错——那会让值守的人去查一个他还没填的值"


def test_the_hint_names_the_webhook_too_because_it_is_part_of_the_same_startup(monkeypatch):
    # 通知 webhook 与 EMR 凭据同属启动自检：漏报它，护士长会以为
    # 「配好了」，却在归档当天才发现缺件提醒一条也没发出去。
    for key in core_bridge.REQUIRED_ENV:
        monkeypatch.delenv(key, raising=False)
    hint = writeback_hint()
    assert "WECOM_WEBHOOK_URL" in hint
    assert "不会静默降级" in hint


def test_a_ready_environment_produces_no_hint_at_all(monkeypatch):
    for key, value in {
        "EMR_BASE_URL": "https://emr.example-hospital.local/fhir/r4",
        "EMR_CLIENT_ID": "psychdoc-client",
        "EMR_CLIENT_SECRET": "s3cret-value",
        "EMR_TENANT_ID": "ward-a",
        "WECOM_WEBHOOK_URL": "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123",
    }.items():
        monkeypatch.setenv(key, value)
    assert writeback_hint() == ""


# ---------------------------------------------------------------------------
# 窗口骨架
# ---------------------------------------------------------------------------


def test_the_window_has_the_two_tabs_the_spec_asks_for(window):
    titles = [window.tabs.tabText(i) for i in range(window.tabs.count())]
    assert titles == ["文书包审查", "归档核齐"]


def test_the_status_bar_starts_without_inventing_numbers(window):
    text = window.status_label.text()
    assert "尚未载入文书包" in text
    # 「跳过件数」只来自真实缓存命中：没载入时不能出现好看的假数字
    assert "跳过" not in text
    assert core_bridge.core_version() in window.tray_summary.text()


def test_the_stylesheet_uses_the_four_spec_colors():
    for color in (theme.PRIMARY, theme.DANGER, theme.OK, theme.BG):
        assert color in theme.QSS
    assert theme.PRIMARY == "#1F6FEB"
    assert theme.DANGER == "#D64545"
    assert theme.OK == "#2DA44E"
    assert theme.BG == "#F6F8FA"


def test_a_missing_ruleset_leaves_the_window_untouched(window, monkeypatch, tmp_path):
    """载入时规则集读不到：弹窗 + 不产出任何诊断（不静默「全部通过」）。"""
    monkeypatch.setattr(
        "desktop.main_window.QMessageBox.critical", lambda *a, **k: None
    )
    missing = tmp_path / "gone.yaml"
    window._ruleset_path = missing
    window.load_path(str(ROOT / "fixtures"))
    assert window._encounters == []
    assert window.encounter_list.count() == 0
    assert window.findings.list.count() == 0


# ---------------------------------------------------------------------------
# 端到端：载入夹具 → 左栏 + 清单 + 底栏
# ---------------------------------------------------------------------------


def _select_row(window, row: int) -> None:
    """选中左栏第 `row` 条（并清掉其余选中）。"""
    window.encounter_list.clearSelection()
    window.encounter_list.setCurrentRow(row)
    window.encounter_list.item(row).setSelected(True)


def _pending_jobs(window) -> list:
    """窗口上所有待收的 job：主窗口的 + 文书卡片的。

    Task 33 起卡片自己持有 job（`FindingsPane._jobs`）——点一条问题会
    起一个 `DocCardJob` 去核心取那一件的段落。只等主窗口的 job 会在
    卡片还没回来时就断言它的内容，测到的是「正在取…」那行占位文案。
    """
    return list(window._jobs) + list(window.findings._jobs)


def _wait_for_jobs(window, timeout_ms: int = 30_000) -> None:
    """转事件循环直到界面上**没有待收的 job**（或超时）。

    必须循环而不是「等一轮」：Task 32 起，载入先扫队列（job 1），
    扫完才起审查（job 2），批量审查又是**依次**跑的。等一轮只等到
    第一个 job，界面上的后续 job 一个都还没开始。
    """
    while _pending_jobs(window):
        loop = QEventLoop()
        jobs = _pending_jobs(window)
        for job in jobs:
            job.done.connect(lambda _p: loop.quit())
            job.failed.connect(lambda _e: loop.quit())
        QTimer.singleShot(10_000, loop.quit)
        loop.exec()
        if _pending_jobs(window) == jobs:
            # 这一轮一个都没收（超时），再等也是白等
            break


def test_loading_a_fixture_fills_the_left_pane_the_findings_and_the_status_bar(
    window, app, tmp_path, monkeypatch
):
    # 缓存落临时目录：测试不该往用户数据目录里写东西
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    window.load_path(str(ROOT / "fixtures"))
    _wait_for_jobs(window)
    app.processEvents()

    assert window.encounter_list.count() > 0, "左栏应有住院条目"

    first = window.encounter_list.item(0).data(0x0100)  # Qt.ItemDataRole.UserRole
    assert first["encounter_alias"].startswith("WARD-")

    # Task 32 起右栏**按住院切分**：载入后自动审的是当前选中的那一条。
    # 左栏按缺件数降序，首位可能恰好是干净的（0 条诊断），所以这里显式
    # 选一条确实有问题的住院，再断言清单非空。
    doc_alias = {
        d: r["encounter_alias"] for r in window._encounters for d in r.get("doc_ids", [])
    }
    batch = core_bridge.review_batch(
        str(ROOT / "fixtures"),
        core_bridge.default_ruleset_path().read_text(encoding="utf-8"),
        str(tmp_path / "cache.json"),
    )
    hit_alias = next(
        doc_alias[d["doc_id"]]
        for d in batch["summary"]["diagnostics"]
        if d["doc_id"] in doc_alias
    )
    row = next(
        i
        for i in range(window.encounter_list.count())
        if window.encounter_list.item(i).data(0x0100)["encounter_alias"] == hit_alias
    )
    _select_row(window, row)
    _wait_for_jobs(window)
    app.processEvents()

    assert window.findings.list.count() > 0, "右栏应有带码的清单"

    # 底栏：Task 32 起，**未载入 / 队列已载入 / 批量审查中**是三种形态。
    # 此刻在批量审查的收尾行上，先验进度形态，再验队列摘要形态。
    assert "已审" in window.status_label.text()

    window._update_status(
        loaded=True,
        totals={
            "encounters": 18,
            "items": 52,
            "missing": 4,
            "skipped_files": 1,
            "ruleset_short_sha256": "e4eda0c79c95",
        },
    )
    status = window.status_label.text()
    assert "18 次住院" in status and "共 52 件" in status and "缺件 4" in status
    short = status.split("规则集")[1].strip()
    assert len(short) == 12

    # 清单第一条必须带码、带段落定位、带依据
    diag = window.findings.list.item(0).data(0x0100)
    assert diag["code"].startswith("QC-PSY-")
    assert "section_index" in diag and "basis" in diag
    window.findings.list.setCurrentRow(0)
    # 卡片走 DocCardJob 异步取段落（Task 33），要转事件循环等它回来。
    _wait_for_jobs(window)
    card = window.findings.card.toPlainText()
    assert diag["code"] in card
    assert "依据" in card
    # 卡片必须显示**这一件的真实段落**，而不是「正在取…」那行占位。
    assert "正在取" not in card, "卡片数据没回来就断言，等于测了个空"
    assert diag["doc_id"] in card, "卡片要显示当前这一件的件 id"


# ---------------------------------------------------------------------------
# 线程契约
# ---------------------------------------------------------------------------


def test_is_main_thread_is_true_on_the_gui_thread(app):
    assert is_main_thread() is True
    assert_main_thread_only("测试")


def test_the_callback_runs_on_the_main_thread_not_the_worker(app):
    """工作线程里跑核心，主线程里收结果——这条是本项目的界面纪律。"""
    seen: list[bool] = []
    got: list[object] = []

    ruleset = core_bridge.default_ruleset_path().read_text(encoding="utf-8")
    bundle = (ROOT / "fixtures" / "ward-01-minimal.json").read_text(encoding="utf-8")

    job = CoreJob(core_bridge.check_bundle, bundle, ruleset)

    def on_done(payload):
        seen.append(is_main_thread())
        got.append(payload)

    job.done.connect(on_done)
    job.failed.connect(lambda e: got.append(e))

    loop = QEventLoop()
    job.done.connect(lambda _p: loop.quit())
    job.failed.connect(lambda _e: loop.quit())
    QTimer.singleShot(30_000, loop.quit)
    loop.exec()

    assert seen == [True], "done 回调必须在主线程"
    assert got and isinstance(got[0], dict), got
    assert got[0]["summary"]["diagnostics"][0]["code"] == "QC-PSY-01"


def test_touching_widgets_from_a_worker_thread_is_refused(app):
    """在线程池里调 `assert_main_thread_only` 必须抛错。"""
    from PySide6.QtCore import QThreadPool

    class _Probe:
        def __init__(self) -> None:
            self.in_worker = None
            self.refused = None

        def probe(self) -> None:
            self.in_worker = is_main_thread()
            try:
                assert_main_thread_only("工作线程里的界面刷新")
            except RuntimeError:
                self.refused = True

    probe = _Probe()
    runnable = _ProbeTask(probe.probe)
    QThreadPool.globalInstance().start(runnable)
    assert runnable.wait_done(30_000), "工作线程没在 30 秒内跑完"
    assert probe.in_worker is False, "工作线程里 is_main_thread() 必须是 False"
    assert probe.refused is True, "工作线程里碰界面必须被 assert_main_thread_only 拦下"


# ---------------------------------------------------------------------------
# 第二个标签页 + job 生命周期
# ---------------------------------------------------------------------------


def test_switching_to_the_archive_tab_draws_the_graph_with_real_node_states(
    window, app, tmp_path, monkeypatch
):
    """归档核齐标签页要画**真的**图，不是占位。

    断言走核心给的四态而不是界面临时编的颜色：图错了护士长会对着一件
    并不欠自己的文书去催人。
    """
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    window.load_path(str(ROOT / "fixtures"))
    _wait_for_jobs(window)
    app.processEvents()

    # 选一份有真缺件的住院
    target = next(
        i
        for i in range(window.encounter_list.count())
        if window.encounter_list.item(i).data(0x0100)["encounter_alias"] == "WARD-C-001"
    )
    window.encounter_list.setCurrentRow(target)
    _wait_for_jobs(window)
    window.tabs.setCurrentIndex(1)
    _wait_for_jobs(window)
    app.processEvents()

    pane = window.archive_pane
    rows = pane.graph._rows
    assert rows, "归档核齐视图没有节点"

    # 四态由核心判定；界面只做颜色映射
    from desktop.archive_view import color_of
    from desktop import theme

    states = {r["state"] for r in rows}
    assert states <= {"missing", "ok", "ambiguous", "blocked"}, states
    assert color_of("missing") == theme.DANGER

    # 阻塞边要有，且图上每条边两端都得能定位到节点框
    blocking = [e for e in pane.graph._edges if e["blocking"]]
    assert blocking, "缺件的住院必须有加粗的阻塞边"
    for e in blocking:
        assert pane.graph.position_of(e["from"]) is not None
        assert pane.graph.position_of(e["to"]) is not None

    # 默认聚焦正在被卡住的那一件，两个方向清单随之有内容
    assert pane.current_node() is not None, "进归档核齐就该默认聚焦一件"
    assert pane.direction_tabs.count() == 2
    assert [pane.direction_tabs.item(i).text() for i in range(2)] == [
        "这条卡住了谁",
        "这件在等谁",
    ]


def test_finished_jobs_are_released_so_the_window_does_not_accumulate_them(window, app, tmp_path, monkeypatch):
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    window.load_path(str(ROOT / "fixtures"))
    assert len(window._jobs) == 1, "起 job 时应被挂住"
    _wait_for_jobs(window)
    app.processEvents()
    # 收完即摘：否则窗口开一整天会积累几千个死 job
    assert window._jobs == []


def test_the_drop_zone_collapses_once_content_arrives(window, app, tmp_path, monkeypatch):
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    window.show()
    window.load_path(str(ROOT / "fixtures"))
    assert window.drop.isVisibleTo(window) is True, "审查中仍显示进度提示"
    _wait_for_jobs(window)
    app.processEvents()
    assert window.drop.isVisibleTo(window) is False, "有内容后拖放区必须收起让位"
