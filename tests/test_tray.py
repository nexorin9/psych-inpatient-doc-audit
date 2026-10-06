"""系统托盘常驻：角标一致性、变更合并、不并发重入与底栏增量口径。

GUI 部分一律 `offscreen`（`conftest.py` 已设），不做像素级断言；
断言的是「契约在不在」：角标与窗口同源、连来多条只跑一轮、跳过数来自
真实缓存命中。渲染好不好看是 Task 14 截图人工核的事。

三条最要紧的：

- `test_the_badge_and_the_window_agree`：托盘角标与窗口队列**同一口径**。
  两处各算一遍时，「托盘说 3 份、窗口说 5 份」几乎是必然的漂移。
- `test_bursts_of_changes_collapse_into_one_rerun` +
  `test_a_change_during_a_rerun_goes_to_the_next_round`：合并与不重入。
- `test_the_status_bar_skipped_count_is_the_real_cache_hit_count`：
  底栏的「缓存跳过 M 件」不是「总数 - 重跑数」编出来的。
"""

from __future__ import annotations

import json
import os
from pathlib import Path

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

pytest.importorskip("PySide6", reason="未安装 PySide6，桌面壳用例跳过")
core_bridge = pytest.importorskip("desktop.core_bridge")

from desktop import tray as tray_mod  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent


@pytest.fixture(scope="session")
def app():
    from PySide6.QtWidgets import QApplication

    existing = QApplication.instance()
    return existing or QApplication([])


@pytest.fixture
def env(tmp_path, monkeypatch):
    """库与缓存都指到临时目录：常驻用例不该往用户数据目录里写东西。"""
    monkeypatch.setenv("PSYCHDOC_DATA_DIR", str(tmp_path / "data"))
    monkeypatch.setenv("PSYCHDOC_CACHE", str(tmp_path / "cache.json"))
    return tmp_path


def _settle(window):
    """等界面上**没有待收的 job**（转事件循环）。

    必须循环而不是「等一轮」：载入先扫队列（job 1），扫完才起审查
    （job 2），批量审查又是**依次**跑的。等一轮只等到第一个 job。
    """
    from PySide6.QtCore import QEventLoop, QTimer

    def pending():
        return list(window._jobs) + list(window.findings._jobs)

    while pending():
        loop = QEventLoop()
        jobs = pending()
        for job in jobs:
            job.done.connect(lambda _p: loop.quit())
            job.failed.connect(lambda _e: loop.quit())
        QTimer.singleShot(10_000, loop.quit)
        loop.exec()
        if pending() == jobs:
            break


def _load(window, path=None):
    """载入夹具（或指定路径）并等所有 job 收完。"""
    window.load_path(str(path or ROOT / "fixtures"))
    _settle(window)


# ---------------------------------------------------------------------------
# 角标口径（纯函数）
# ---------------------------------------------------------------------------


def test_the_badge_counts_packages_not_items():
    """角标数的是**份**（几次住院没收齐），不是**件**。

    11 个缺件可能只落在 3 次住院上：护士长先认「还有 3 份包没收」，
    再进窗口看「这 3 份里差 11 件」。两个数字混用会让人以为少算了。
    """
    entries = [
        {"encounter_alias": "WARD-A-001", "doc_count": 6, "missing_count": 5},
        {"encounter_alias": "WARD-A-002", "doc_count": 4, "missing_count": 4},
        {"encounter_alias": "WARD-A-003", "doc_count": 6, "missing_count": 2},
        {"encounter_alias": "WARD-A-004", "doc_count": 5, "missing_count": 0},
    ]
    state = tray_mod.badge_from_entries(entries)
    assert state["count"] == 3
    assert state["missing_items"] == 11
    assert state["aliases"] == ["WARD-A-001", "WARD-A-002", "WARD-A-003"]
    assert "3 份" in state["basis_zh"] and "11 件" in state["basis_zh"]


def test_a_fully_collected_ward_shows_no_badge_at_all():
    entries = [{"encounter_alias": "WARD-B-001", "doc_count": 6, "missing_count": 0}]
    state = tray_mod.badge_from_entries(entries)
    assert state["count"] == 0
    assert tray_mod.badge_text(0) == ""
    assert "已全部收齐" in state["basis_zh"]


def test_the_badge_number_is_capped_rather_than_written_out_in_full():
    """角标画不下 4 位数：给 99+，tooltip 里仍是真数。"""
    entries = [
        {"encounter_alias": f"WARD-C-{i:03d}", "doc_count": 1, "missing_count": 1}
        for i in range(120)
    ]
    state = tray_mod.badge_from_entries(entries)
    assert state["count"] == 120
    assert tray_mod.badge_text(120) == "99+"
    assert "120 份" in state["basis_zh"]


def test_entries_without_missing_key_do_not_raise_or_invent_a_number():
    """缺 `missing_count` 键按 0 算，不当成 1——否则凭空多一份待提交。"""
    state = tray_mod.badge_from_entries([{"encounter_alias": "WARD-D-001"}])
    assert state["count"] == 0
    assert tray_mod.badge_from_entries([])["count"] == 0
    assert tray_mod.badge_from_entries(None)["count"] == 0


# ---------------------------------------------------------------------------
# 变更合并：不并发重入
# ---------------------------------------------------------------------------


def test_the_first_change_starts_a_round_immediately():
    c = tray_mod.ChangeCoalescer()
    assert c.running is False
    assert c.submit(["WARD-A-001", "WARD-A-002"]) == ["WARD-A-001", "WARD-A-002"]
    c.mark_running()
    assert c.running is True


def test_a_change_arriving_mid_rerun_waits_for_the_next_round():
    c = tray_mod.ChangeCoalescer()
    c.submit(["WARD-A-001"])
    c.mark_running()
    # 重跑途中来的变更：本轮不变，进下一轮
    assert c.submit(["WARD-B-001"]) is None
    assert c.running is True
    assert c.pending == ("WARD-A-001",)
    assert c.queued == ("WARD-B-001",)


def test_bursts_of_changes_collapse_into_one_rerun():
    """连来 5 条变更合并成**一轮**，不是 5 轮。"""
    c = tray_mod.ChangeCoalescer()
    c.submit(["WARD-A-001"])
    c.mark_running()
    for alias in ("WARD-B-001", "WARD-C-001", "WARD-B-001", "WARD-D-001", "WARD-C-001"):
        c.submit([alias])
    nxt = c.finish()
    assert nxt == ["WARD-B-001", "WARD-C-001", "WARD-D-001"], "别名应去重且保序"
    # 5 条变更 → 下一轮一次
    assert c.stats()["rounds_merged"] == 5
    assert c.stats()["rounds_started"] == 2


def test_finishing_with_nothing_queued_unlocks_the_coalescer():
    c = tray_mod.ChangeCoalescer()
    c.submit(["WARD-A-001"])
    c.mark_running()
    assert c.finish() is None
    assert c.running is False
    assert c.pending == ()
    # 解锁后又能立刻起新一轮
    assert c.submit(["WARD-E-001"]) == ["WARD-E-001"]


def test_an_empty_change_is_never_treated_as_a_rerun():
    """空变更不排队也不起轮——否则每次心跳都会起一次空跑。"""
    c = tray_mod.ChangeCoalescer()
    assert c.submit([]) is None
    assert c.submit([""]) is None
    assert c.submit(None or []) is None
    assert c.running is False
    c.submit(["WARD-A-001"])
    c.mark_running()
    c.finish()
    assert c.queued == ()


# ---------------------------------------------------------------------------
# 目录指纹与变更反查（纯函数）
# ---------------------------------------------------------------------------


def test_only_the_files_that_really_changed_come_back(tmp_path):
    a = tmp_path / "a.json"
    b = tmp_path / "b.json"
    a.write_text('{"resourceType":"Bundle"}', encoding="utf-8")
    b.write_text('{"resourceType":"Bundle"}', encoding="utf-8")

    before = tray_mod.file_fingerprints([str(a), str(b)])
    assert tray_mod.changed_files(before, before) == []

    # b 不动、a 变：只报 a，且顺序稳定
    b_before = before[str(b)]
    a.write_text('{"resourceType":"Bundle","x":1}', encoding="utf-8")
    os.utime(a, (before[str(a)][0] + 10, before[str(a)][0] + 10))
    after = tray_mod.file_fingerprints([str(a), str(b)])
    assert tray_mod.changed_files(before, after) == [str(a)]
    assert after[str(b)] == b_before


def test_an_unreadable_file_is_recorded_rather_than_skipped(tmp_path):
    """打不开的文件记 ``size=-1``：下一轮仍看得见它，不会被当成「没这文件」。"""
    missing = str(tmp_path / "gone.json")
    prints = tray_mod.file_fingerprints([missing])
    assert prints[missing][1] == -1.0
    # 仍然是「存在且变了」，不是被静默丢掉
    assert tray_mod.changed_files({}, prints) == [missing]


def test_changed_files_are_reported_in_a_stable_order(tmp_path):
    paths = []
    for name in ("c.json", "a.json", "b.json"):
        p = tmp_path / name
        p.write_text("{}", encoding="utf-8")
        paths.append(str(p))
    changed = tray_mod.changed_files({}, tray_mod.file_fingerprints(paths))
    assert changed == sorted(paths), "两次比对必须给出同一个顺序，否则导出 diff 不掉"


def test_changed_files_map_back_to_encounter_aliases_not_guessed_names():
    """反查走核心给的 `source_files`，不在壳里按 doc_id / 文件名猜住院。"""
    entries = [
        {
            "encounter_alias": "WARD-C-001",
            "source_files": ["ward-03-cross-system-dup.json"],
        },
        {
            "encounter_alias": "WARD-J-001",
            "source_files": ["ward-12-cross-doc-compliant.json"],
        },
    ]
    got = tray_mod.aliases_of_changed_files(
        entries, ["/abs/path/to/ward-03-cross-system-dup.json"]
    )
    assert got == ["WARD-C-001"]
    # 按左栏显示顺序返回，不按字典序
    got = tray_mod.aliases_of_changed_files(
        [
            {"encounter_alias": "WARD-Z-001", "source_files": ["z.json"]},
            {"encounter_alias": "WARD-A-001", "source_files": ["a.json"]},
        ],
        ["/x/z.json", "/y/a.json"],
    )
    assert got == ["WARD-Z-001", "WARD-A-001"]


def test_a_changed_file_that_matches_no_queue_entry_reruns_nothing():
    """对不上任何队列条目的文件**不**触发全量重审。

    病区导出目录里会落一堆无关文件（台账、扫描件）；每一次无关文件落地
    都触发一次全病区重审，护士长会直接把轮询关掉——那才是真正的损失。
    """
    entries = [{"encounter_alias": "WARD-A-001", "source_files": ["a.json"]}]
    assert tray_mod.aliases_of_changed_files(entries, ["/tmp/台账.xlsx"]) == []
    assert tray_mod.aliases_of_changed_files(entries, []) == []


def test_only_json_export_files_are_watched(tmp_path):
    (tmp_path / "a.json").write_text("{}", encoding="utf-8")
    (tmp_path / "扫描件.pdf").write_bytes(b"%PDF")
    (tmp_path / "台账.xlsx").write_bytes(b"PK")
    sub = tmp_path / "ward-b"
    sub.mkdir()
    (sub / "b.json").write_text("{}", encoding="utf-8")
    got = [Path(p).name for p in tray_mod.iter_export_files([str(tmp_path)])]
    assert sorted(got) == ["a.json", "b.json"]


# ---------------------------------------------------------------------------
# 游标落盘
# ---------------------------------------------------------------------------


def test_the_cursor_round_trips_through_the_user_data_dir(env):
    saved = tray_mod.save_cursor({"/x/a.json": [1.5, 128.0]})
    assert saved is not None
    # 游标不落项目子目录（否则会被误提交进仓库）
    assert ROOT not in saved.parents
    assert tray_mod.load_cursor() == {"/x/a.json": [1.5, 128.0]}


def test_a_corrupt_cursor_reads_as_no_cursor_instead_of_crashing(env):
    tray_mod.cursor_path().parent.mkdir(parents=True, exist_ok=True)
    tray_mod.cursor_path().write_text("{ 不是 json", encoding="utf-8")
    assert tray_mod.load_cursor() == {}
    tray_mod.cursor_path().write_text("[1, 2, 3]", encoding="utf-8")
    assert tray_mod.load_cursor() == {}


def test_the_poll_interval_is_off_by_default_and_env_can_turn_it_on(monkeypatch):
    monkeypatch.delenv("PSYCHDOC_POLL_SECONDS", raising=False)
    assert tray_mod.resolve_poll_seconds() == 0, "没人要求它盯盘时不该自己转起来"
    assert tray_mod.DEFAULT_POLL_SECONDS == 0
    monkeypatch.setenv("PSYCHDOC_POLL_SECONDS", "45")
    assert tray_mod.resolve_poll_seconds() == 45
    # 显式参数压过环境变量；坏值退回「不轮询」而不是抛错打断启动
    assert tray_mod.resolve_poll_seconds(10) == 10
    monkeypatch.setenv("PSYCHDOC_POLL_SECONDS", "abc")
    assert tray_mod.resolve_poll_seconds() == 0
    assert tray_mod.resolve_poll_seconds(-5) == 0


# ---------------------------------------------------------------------------
# Qt：托盘图标与控制器（offscreen）
# ---------------------------------------------------------------------------


def test_the_tray_menu_offers_the_three_actions(app):
    from desktop.tray import TrayController

    class _Win:
        pass

    icon = tray_mod.TrayController(_Win()).icon
    labels = [a.text() for a in icon.menu().actions() if a.text()]
    assert labels == ["打开主窗口", "立即重审", "退出"]


def test_the_icon_paints_a_badge_and_a_no_badge_state(app):
    """角标画在图标上：有数走红、无数走绿。视觉形态由 Qt 原生绘制。"""
    danger = tray_mod.render_icon(3).pixmap(64, 64)
    ok = tray_mod.render_icon(0).pixmap(64, 64)
    assert not danger.isNull() and not ok.isNull()
    assert danger.toImage() != ok.toImage(), "有角标与无角标必须画得出来区别"
    assert tray_mod.badge_text(0) == "" and tray_mod.badge_text(7) == "7"


def test_the_badge_and_the_window_agree_on_the_real_fixture_queue(
    app, env, tmp_path
):
    """角标数与窗口队列**同一口径**：真实夹具，两个数必须一致。

    这条是常驻工具最容易出的错：托盘自己再扫一遍目录 → 两个口径分头
    长偏，护士长看着托盘说「还剩 3 份」，点进窗口却看到「缺件 11」，
    两边都得解释一遍，而且**两边都有理**。
    """
    from PySide6.QtWidgets import QApplication

    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller
    _load(window)
    QApplication.processEvents()

    rows = window.encounter_rows()
    assert rows, "夹具队列应有住院条目"
    from_rows = tray_mod.badge_from_entries(rows)
    from_controller = controller.refresh_badge()

    assert from_rows == from_controller, "角标必须只有一个口径"
    assert from_rows["count"] > 0, "夹具里确有缺件的住院"
    # 与窗口自己的统计同源：份数 = 缺件 > 0 的条目数
    assert from_rows["count"] == sum(1 for r in rows if r["missing_count"] > 0)
    # 标题也挂同一个数（同一口径，不是各算一遍）
    assert f"待提交 {from_rows['count']} 份" in window.windowTitle()
    assert from_rows["basis_zh"] in controller.icon.toolTip()
    window.close()


def test_starting_the_tray_takes_a_baseline_so_an_unchanged_queue_never_reruns(
    app, env
):
    """启动那一刻先记指纹：否则首轮会把整目录当成「全变了」。

    护士长刚打开窗口就看着它自动重跑一遍没动过的包——这比不轮询更糟。
    """
    from PySide6.QtWidgets import QApplication

    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller
    _load(window)
    QApplication.processEvents()

    controller.start()
    assert controller._fingerprints, "启动时应先记下当前导出根的指纹"
    before = dict(controller._fingerprints)

    # 目录没动：这一轮什么都不该发生
    assert controller.poll_now() == []
    assert controller._fingerprints == before
    assert controller.coalescer.running is False
    window.close()


def test_a_touched_export_file_triggers_exactly_one_incremental_rerun(
    app, env, tmp_path
):
    """改了哪个文件 → 只重跑那几次住院，且底栏跳过数是真实缓存命中数。

    「只重跑变过的件」这条靠增量缓存兑现：第二轮 run_summary 里的
    ``items_skipped`` 必须等于上一轮的件数——这是核心真给的，不是
    「总数 - 重跑数」编出来的。
    """
    from PySide6.QtWidgets import QApplication

    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller

    export_dir = tmp_path / "export"
    export_dir.mkdir()
    # 放**两份不同住院**的夹具：只动其中一份，才能证明「只重跑变过的件」
    # 而不是「整个目录重扫一遍、恰好只有一份变了」。
    src = ROOT / "fixtures" / "ward-01-minimal.json"
    other = ROOT / "fixtures" / "ward-09-field-compliant.json"
    dst = export_dir / "ward-01-minimal.json"
    dst.write_text(src.read_text(encoding="utf-8"), encoding="utf-8")
    (export_dir / "ward-09-field-compliant.json").write_text(
        other.read_text(encoding="utf-8"), encoding="utf-8"
    )

    rounds: list[dict] = []
    # 走托盘自己的回调（`runFinished`）而不是私有回调：
    # `request_rerun` 会把 `_rerun_done` 覆盖成托盘那个，测试再塞一个
    # 自己的只会让人以为收到了、其实收的是上一轮。
    controller.runFinished.connect(rounds.append)
    _load(window, export_dir)
    QApplication.processEvents()

    controller.start()
    assert controller.coalescer.running is False
    rounds.clear()

    # 改其中一个文件的**真实文书内容**（改 JSON 里的一个段内时刻）。
    # 只在文件尾追加一个换行不算：解析出来的件没变，核心判「重跑 0 件」
    # 是对的——拿它当「变了」会把这个口径测歪。
    doc = json.loads(dst.read_text(encoding="utf-8"))
    for entry in doc.get("entry", []):
        resource = entry.get("resource", {})
        for section in resource.get("section", []) or []:
            if isinstance(section, dict) and isinstance(section.get("text"), str):
                section["text"] = section["text"] + "（补记：巡视一次）"
    dst.write_text(json.dumps(doc, ensure_ascii=False), encoding="utf-8")
    os.utime(dst, (1_800_000_000, 1_800_000_000))

    aliases = controller.poll_now()
    assert aliases == ["WARD-A-001"], f"只该重跑这一个住院，实得 {aliases}"
    assert controller.coalescer.running is True, "重跑期间必须处在运行态"

    # 这一轮真正的重跑由窗口的批量审查跑完：等它收尾。
    _settle(window)
    QApplication.processEvents()

    assert rounds, "托盘那一轮的收尾回调必须被调到"
    run = rounds[-1]
    assert "items_reviewed" in run and "items_skipped" in run
    # 「只重跑变过的件」靠增量缓存兑现：没动的那次住院全部命中缓存。
    assert run["items_reviewed"] > 0, f"改了的件应重跑，实得 {run}"
    assert run["items_skipped"] > 0, f"未变件应命中缓存，实得 {run}"
    assert run["items_reviewed"] < run["items_total"], "不该整目录全量重跑"
    assert run["ruleset_short_sha256"], "规则集短哈希要跟着底栏一起出来"

    status = window.status_label.text()
    assert "重跑" in status and "缓存跳过" in status
    reviewed = int(status.split("重跑")[1].split("件")[0].strip())
    skipped = int(status.split("缓存跳过")[1].split("件")[0].strip())
    assert reviewed == run["items_reviewed"], "底栏重跑数必须逐个等于核心给的数"
    assert skipped == run["items_skipped"], "底栏跳过数必须逐个等于核心给的数"
    assert reviewed + skipped == run["items_total"], "两个数加起来应等于总件数"
    window.close()


def test_changes_arriving_mid_rerun_are_merged_into_one_more_round(
    app, env, tmp_path
):
    """连来多条变更合并成**一轮**重跑；本轮不变。"""
    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller

    calls: list[list[str]] = []
    window.request_rerun = lambda aliases, on_done=None: calls.append(list(aliases)) or True
    controller._on_round_done = lambda payload: tray_mod.TrayController._on_round_done(
        controller, payload
    )

    controller.notify_change(["WARD-A-001"])
    assert controller.coalescer.running is True
    for alias in ("WARD-B-001", "WARD-C-001", "WARD-B-001"):
        assert controller.notify_change([alias]) is None, "重跑期间的变更进下一轮"

    stats = controller.coalescer.stats()
    assert stats["running"] is True
    assert stats["pending"] == ["WARD-A-001"]
    assert stats["queued"] == ["WARD-B-001", "WARD-C-001"], "同别名去重"
    assert stats["rounds_merged"] == 3

    controller._on_round_done({"items_reviewed": 1, "items_skipped": 5})
    assert calls[-1] == ["WARD-B-001", "WARD-C-001"], "合并后只起一轮"
    window.close()


def test_shutting_down_stops_polling_persists_and_leaves_no_tray(
    app, env, tmp_path
):
    """退出 = 停轮询 + 落盘游标 + 通知窗口落库 + 隐藏托盘。"""
    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=1)
    window.tray = controller
    controller.start()
    assert controller.poll_seconds == 1

    persisted: list[bool] = []
    window.persist_before_exit = lambda: persisted.append(True)
    controller.shutdown()

    assert controller.poll_seconds == 1
    assert persisted == [True], "退出前必须让窗口把进度落库"
    assert tray_mod.cursor_path().exists(), "游标要落盘，否则下次启动全量当变了"
    assert controller.icon.isVisible() is False
    window.close()


def test_closing_the_window_is_the_same_as_quitting(app, env):
    """关窗口 = 真退出（缩到托盘是另一种产品）。

    缩到托盘会让护士长以为程序没起来：进程还在、端口还占着，下次点图标
    才发现。两条退出路（关窗口 / 托盘→退出）必须同义。
    """
    from desktop.main_window import MainWindow

    window = MainWindow()
    persisted: list[bool] = []
    window.persist_before_exit = lambda: persisted.append(True)
    window.close()
    assert persisted == [True], "关窗也要落库"
    assert window.isVisible() is False


def test_the_badge_is_refreshed_after_a_scan_but_not_on_every_click(
    app, env
):
    """角标只在队列真变了的三处刷：一屏一变，护士长就学会不信它。"""
    from desktop.main_window import MainWindow

    window = MainWindow()
    refreshes: list[str] = []

    class _Tray:
        """只提供 `refresh_badge` 的托盘替身（并带一个 icon 供关窗路径用）。"""

        class icon:  # noqa: N801 - 替身属性名
            @staticmethod
            def hide():
                pass

        @staticmethod
        def refresh_badge():
            refreshes.append("scan")

    window.tray = _Tray()
    _load(window)
    assert refreshes, "扫完队列必须刷一次角标"
    # 载入与首轮审完各刷一次（队列真变了的两处），不是每个 job 刷一次。
    assert len(refreshes) <= 3, f"角标刷得太勤，护士长会学不会信它：{refreshes}"

    # 点一条住院只是**切换右栏内容**，队列本身没变：不该刷。
    # 这一步只看「选中动作本身」，不等随后的审查收尾（那是另一处该刷的）。
    before = len(refreshes)
    window.encounter_list.setCurrentRow(0)
    assert len(refreshes) == before, "选中一条住院不该立刻刷角标"
    window.close()


def test_the_menu_review_action_replays_the_queue_as_one_change_event(
    app, env
):
    """托盘「立即重审」= 一次全量别名变更事件，走同一条合并队列。

    不另开一条「菜单专用重审」路径：两条路迟早在「跳过数怎么算」上分叉，
    而分叉出来的那个数正是底栏给护士长看的那个。
    """
    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller
    _load(window)

    seen: list[list[str]] = []
    controller.notify_change = lambda aliases: seen.append(list(aliases)) or list(aliases)

    controller._on_review_requested()
    assert seen, "菜单动作应产生一次全量别名变更"
    assert seen[0] == [r["encounter_alias"] for r in window.encounter_rows()]
    window.close()


def test_the_open_action_brings_the_window_back_from_the_tray(app, env):
    """菜单「打开主窗口」= 取消最小化 + 置前 + 激活。"""
    called: list[str] = []

    class _Win:
        def show_from_tray(self):
            called.append("show_from_tray")

    controller = tray_mod.TrayController(_Win())
    controller._on_open_requested()
    assert called == ["show_from_tray"]


def test_no_change_never_reruns_even_with_a_baseline_intact(app, env, tmp_path):
    """边界：导出根不存在 / 未载入时不重跑，也不报错。"""
    from desktop.main_window import MainWindow

    window = MainWindow()
    controller = tray_mod.TrayController(window, poll_seconds=0)
    window.tray = controller
    controller._roots = lambda: []
    controller.start()
    assert controller.poll_now() == []
    assert controller.coalescer.running is False
    window.close()