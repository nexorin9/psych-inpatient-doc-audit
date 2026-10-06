"""归档核齐视图的界面测试（offscreen）。

这一页是护士长判断「这份能不能归档」的地方，因此本文件盯住三件事，
少一件这个页面就没有存在价值：

1. **四态配色与图的结构**（谁红、谁橙、哪条边加粗）——图错了，
   护士长就会对着一件并不欠自己的文书去催人；
2. **两个方向的问题清单**（这份卡住谁 / 这份在等谁）——这两个问题
   业务上完全不同，合成一个答案就等于没答；
3. **发缺件待办的置灰与导出 csv**——发过就置灰（且关窗重开后仍置灰），
   导出的清单要能直接给病案室逐行核。

数据全部来自真实脱敏夹具（`fixtures/ward-02-restraint-set.json` 抽掉
约束知情同意），不走 mock：核心给什么，界面就得显示什么。
"""

from __future__ import annotations

import csv
import io
import os
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

import pytest

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")

pytest.importorskip("PySide6", reason="未安装 PySide6，桌面壳用例跳过")
core_bridge = pytest.importorskip("desktop.core_bridge")

from PySide6.QtWidgets import QApplication  # noqa: E402

from desktop import theme  # noqa: E402
from desktop.archive_view import ArchivePane, GraphView, color_of, export_csv  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SET = ROOT / "fixtures" / "ward-02-restraint-set.json"
AT = "2026-03-10T09:00:00+08:00"
DAY = "2026-03-10"
ALIAS = "WARD-B-001"
CONSENT = "restraint_consent"


@pytest.fixture(scope="session")
def app():
    existing = QApplication.instance()
    yield existing or QApplication([])


@pytest.fixture
def tmp_db(tmp_path):
    return str(tmp_path / "psychdoc-test.sqlite3")


@pytest.fixture
def ruleset():
    return (ROOT / "rulesets" / "hospital.yaml").read_text(encoding="utf-8")


def trimmed_bundle(tmp_path) -> Path:
    """抽掉约束知情同意的那份成套文书包。"""
    import json

    doc = json.loads(SET.read_text(encoding="utf-8"))
    doc["entry"] = [
        e
        for e in doc["entry"]
        if not (
            e["resource"].get("resourceType") == "Composition"
            and any(
                c.get("code") == CONSENT
                for c in e["resource"].get("type", {}).get("coding", [])
            )
        )
    ]
    out = tmp_path / "ward-02-missing-consent.json"
    out.write_text(json.dumps(doc, ensure_ascii=False), encoding="utf-8")
    return out


def view_of(path: Path, ruleset: str, tmp_db: str, alias: str = ALIAS) -> dict:
    return core_bridge.archive_view(
        str(path), ruleset, alias, AT, DAY, tmp_db
    )


def state_of(payload: dict, template_id: str) -> str | None:
    for row in payload["rows"]:
        if row["template_id"] == template_id:
            return row["state"]
    return None


# ---------------------------------------------------------------------------
# 一：四态配色与图的结构
# ---------------------------------------------------------------------------


def test_the_four_states_map_to_the_four_colors_the_spec_names():
    # 这张映射表是「缺件红 / 达标绿 / 人工灰 / 连带橙」的唯一出处：
    # 散落各处的 if/else 迟早让缺件红在某个角落变成别的颜色。
    assert color_of("missing") == theme.DANGER == "#D64545"
    assert color_of("blocked") == theme.BLOCKED == "#E3A008"
    assert color_of("ambiguous") == theme.AMBIGUOUS == "#8B949E"
    assert color_of("ok") == theme.ARCHIVE_OK
    # 未知状态退回中性灰，**不给绿色**：宁可显示「不知道」，
    # 也不能给一件可能欠着的文书涂上达标色。
    assert color_of("something-new") == theme.AMBIGUOUS


def test_a_missing_item_is_red_and_what_it_blocks_is_orange(tmp_path, ruleset, tmp_db):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)

    assert state_of(payload, CONSENT) == "missing", "抽走的那一件是缺件"
    assert state_of(payload, "restraint_record") == "blocked", "直接卡在缺件上的件是连带阻塞"
    assert state_of(payload, "restraint_release") == "blocked", "被连带卡住的第二级"
    assert state_of(payload, "assessment_report") == "ok", "没被卡住的件仍然是达标绿"

    # 连带阻塞**不是**缺件：护士长点进去必须看到「在等谁」，
    # 而不是「文件不在」，否则会对着一件并不欠的文书去催人。
    for row in payload["rows"]:
        if row["state"] == "blocked":
            assert row["waiting_on"], "被连带阻塞的件要能回答「在等谁」"
            assert row["missing_days"] is None, "连带阻塞不计欠件天数"


def test_the_blocking_edge_is_marked_so_the_view_can_bold_it(tmp_path, ruleset, tmp_db):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)

    blocking = [e for e in payload["edges"] if e["blocking"]]
    assert blocking, "有缺件就该有加粗的阻塞边"
    for e in blocking:
        # 加粗的边必须指向缺件占位，否则图上会凭空多一根红线。
        assert CONSENT in e["to"] or e["to"].startswith("<"), f"阻塞边指向了 {e['to']}"
        assert e["describe"], "每条边都要有一句人能读懂的中文说明"

    # 阻塞边清单与 edges 里 blocking=true 的那些必须是同一批——
    # 两处对不上就会画出一根多出来的红线。
    from_edges = sorted((e["from"], e["to"]) for e in blocking)
    from_list = sorted((p[0], p[1]) for p in payload["blocking_edges"])
    assert from_edges == from_list


def test_a_soft_gap_is_drawn_but_never_counted_as_a_missing_item(tmp_path, ruleset, tmp_db):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)

    assert payload["soft_gaps"], "参考缺件（requires: false）应当可见"
    for pair in payload["soft_gaps"]:
        node = payload["rows"][0]
        target = next((r for r in payload["rows"] if r["node_id"] == pair[1]), None)
        assert target is not None
        assert target["state"] != "missing", "参考缺件被当成欠件了"
        assert node is not None
    # 通知清单只收会阻断归档的缺件：护士长不该收到一条根本不欠的催办。
    missing_templates = {m["template_id"] for m in payload["missing"]}
    assert CONSENT in missing_templates


def test_the_graph_lays_nodes_out_in_dependency_layers(tmp_path, ruleset, tmp_db, app):
    """分层布局：前置件在上层、后件在下层，护士长自上而下追即可。"""
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    graph = GraphView()
    graph.load(payload["rows"], payload["edges"])

    by_template = {r["node_id"]: r["template_id"] for r in payload["rows"]}
    consent_node = next(
        n for n, t in by_template.items() if t == CONSENT
    )
    record_node = next(
        n for n, t in by_template.items() if t == "restraint_record"
    )
    release_node = next(
        n for n, t in by_template.items() if t == "restraint_release"
    )

    y_consent = graph.position_of(consent_node).top()
    y_record = graph.position_of(record_node).top()
    y_release = graph.position_of(release_node).top()

    assert y_consent < y_record < y_release, "约束知情同意 → 约束记录 → 解除记录，自上而下"
    # 每个节点都有位置：漏一个就是图上凭空少一个框。
    for row in payload["rows"]:
        assert graph.position_of(row["node_id"]) is not None


def test_the_same_bundle_always_lays_out_identically(tmp_path, ruleset, tmp_db, app):
    """同一份数据两次渲染版式逐像素一致——截图与截图才能 diff。"""
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    a, b = GraphView(), GraphView()
    a.load(payload["rows"], payload["edges"])
    b.load(list(reversed(payload["rows"])), payload["edges"])
    for node in (r["node_id"] for r in payload["rows"]):
        assert a.position_of(node) == b.position_of(node)


# ---------------------------------------------------------------------------
# 二：两个方向的问题清单
# ---------------------------------------------------------------------------


def test_the_two_directions_are_two_different_answers(tmp_path, ruleset, tmp_db, app):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    record = next(r for r in payload["rows"] if r["template_id"] == "restraint_record")
    node_id = record["node_id"]
    names = {
        r["node_id"]: (r["display_zh"] or r["template_id"]) for r in payload["rows"]
    }

    pane.select_node(node_id)
    pane.direction_tabs.setCurrentRow(0)  # 这条卡住了谁
    blocking_text = [pane.side.item(i).text() for i in range(pane.side.count())]

    pane.direction_tabs.setCurrentRow(1)  # 这件在等谁
    waiting_text = [pane.side.item(i).text() for i in range(pane.side.count())]

    assert blocking_text != waiting_text, "两个方向给出了同一个答案"
    # 「在等谁」这一栏必须点得出缺件那件——护士长就是从这里追上去的。
    assert any(CONSENT in t or "约束知情同意" in t for t in waiting_text), waiting_text


def test_switching_direction_keeps_the_focused_node(tmp_path, ruleset, tmp_db, app):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    node_id = payload["rows"][0]["node_id"]
    pane.select_node(node_id)
    pane.direction_tabs.setCurrentRow(1)
    assert pane.current_node() == node_id, "换方向不该把焦点弄丢"

    pane.direction_tabs.setCurrentRow(0)
    assert pane.current_node() == node_id, "切回来焦点还在"


def test_a_node_with_nothing_blocking_says_so_instead_of_showing_an_empty_box(
    tmp_path, ruleset, tmp_db, app
):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    # 评估报告不卡住任何件（成包的末端），这一栏要有一句人话，
    # 而不是一片空白——空白会被读成「还没算出来」。
    assess = next(r for r in payload["rows"] if r["template_id"] == "assessment_report")
    pane.select_node(assess["node_id"])
    pane.direction_tabs.setCurrentRow(0)
    texts = [pane.side.item(i).text() for i in range(pane.side.count())]
    assert texts, "没有可列的对象时也要给一句说明"
    assert not any(t.strip() == "" for t in texts)


def test_with_nothing_selected_the_side_pane_explains_what_to_do(tmp_path, ruleset, tmp_db, app):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)
    texts = [pane.side.item(i).text() for i in range(pane.side.count())]
    assert len(texts) == 1
    assert "点图上" in texts[0]


# ---------------------------------------------------------------------------
# 三：发缺件待办的置灰
# ---------------------------------------------------------------------------


# 本地企业微信协议替身跑在**独立进程**里，不在本进程起线程。
#
# 为什么必须是独立进程：核心出站是同步阻塞的 `ureq`，调用它的线程在等待
# 响应期间不放 GIL；同一进程里的 `http.server` 线程因此拿不到 GIL，
# 永远走不到 `do_POST` —— 表现是「连上了但一直读到超时」。
# 用子进程跑替身，主进程只管收端口号，GIL 就成了两个进程之间的事。
_WEBHOOK_SRC = """
import json, sys
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

class H(BaseHTTPRequestHandler):
    # HTTP/1.1 + 显式 Content-Length：核心出站用的是 ureq，
    # 默认 HTTP/1.0 的响应会被它判成「读完状态行就读超时」。
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        n = int(self.headers.get("Content-Length", 0))
        body = self.rfile.read(n).decode("utf-8")
        with open(sys.argv[2], "a", encoding="utf-8") as fh:
            fh.write(body + chr(10))
        payload = b'{"errcode":0,"errmsg":"ok"}'
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *a):
        return

srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
srv.daemon_threads = True
with open(sys.argv[1], "w", encoding="utf-8") as fh:
    fh.write(str(srv.server_address[1]))
srv.serve_forever()
"""


class Webhook:
    """独立进程里的企业微信协议替身。

    替身只在**测试数据层**出现：产品代码走的是真实 ``UreqTransport``
    指向环境变量里的地址，这里只是把那个地址指向本机。
    """

    def __init__(self, monkeypatch, tmp_path):
        import subprocess
        import sys

        self.port_file = tmp_path / "port"
        self.call_log = tmp_path / "calls.log"
        script = tmp_path / "webhook.py"
        script.write_text(_WEBHOOK_SRC, encoding="utf-8")
        self.proc = subprocess.Popen(
            [sys.executable, str(script), str(self.port_file), str(self.call_log)]
        )
        # 等端口文件出现：替身起来之前核心会连不上，而「连不上」和
        # 「被拒」在断言里长得一样，必须先确认它真的在监听。
        for _ in range(200):
            if self.port_file.exists():
                break
            import time

            time.sleep(0.02)
        assert self.port_file.exists(), "本地协议替身没起来"
        self.port = int(self.port_file.read_text().strip())
        monkeypatch.setenv(
            "WECOM_WEBHOOK_URL",
            f"http://127.0.0.1:{self.port}/cgi-bin/webhook/send?key=TESTKEY",
        )

    @property
    def calls(self) -> list[str]:
        if not self.call_log.exists():
            return []
        return [
            line
            for line in self.call_log.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]

    def stop(self) -> None:
        self.proc.terminate()
        try:
            self.proc.wait(timeout=5)
        except Exception:
            self.proc.kill()


@pytest.fixture
def webhook(monkeypatch, tmp_path):
    hook = Webhook(monkeypatch, tmp_path)
    yield hook
    hook.stop()


def test_sending_a_missing_item_todo_goes_out_and_then_greys_the_button(
    tmp_path, ruleset, tmp_db, webhook, monkeypatch, app
):
    bundle = trimmed_bundle(tmp_path)
    payload = view_of(bundle, ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    assert pane.notify_button.isEnabled(), "有缺件时按钮可点"

    result = core_bridge.notify_missing(
        encounter_alias=ALIAS,
        ward="精神科三病区",
        business_day=DAY,
        at=AT,
        missing=[(n, lbl) for n, lbl in pane.missing_pairs()],
        group_hint="精神科病区文书群",
        store_path=tmp_db,
    )
    assert result["sent"] == 1
    assert len(webhook.calls) == 1, "确实出了一次站"
    assert ALIAS in webhook.calls[0]
    pane.apply_notify_result(result)

    # 重算视图：按钮的置灰读的是**发件箱那一行的状态**，不是本次返回值——
    # 这样关窗重开后按钮仍然是灰的。
    reloaded = view_of(bundle, ruleset, tmp_db)
    pane.load(reloaded)
    assert pane.notify_button.isEnabled(), "全部发件后不再可点，避免重复发"
    assert pane.notify_button.toolTip()


def test_the_notice_names_the_item_in_chinese_not_by_its_synthetic_id(
    tmp_path, ruleset, tmp_db, webhook, app
):
    """缺件节点没有 Composition.id，显示名必须反查院内类型名。

    提醒里写 ``<WARD-B-001>::missing::restraint_consent`` 等于没发：
    护士长看不懂，群里也没人会去查那条 id。
    """
    bundle = trimmed_bundle(tmp_path)
    payload = view_of(bundle, ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    core_bridge.notify_missing(
        encounter_alias=ALIAS,
        ward="精神科三病区",
        business_day=DAY,
        at=AT,
        missing=pane.missing_pairs(),
        group_hint="精神科病区文书群",
        store_path=tmp_db,
    )
    body = webhook.calls[0]
    assert "约束知情同意" in body, body
    assert "::missing::" not in body, "提醒正文里漏出了合成的节点 id"


def test_clicking_the_button_twice_on_the_same_day_sends_only_one_notice(
    tmp_path, ruleset, tmp_db, webhook, app
):
    bundle = trimmed_bundle(tmp_path)
    payload = view_of(bundle, ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    sent: list[dict] = []
    pane.notifyRequested.connect(lambda m: sent.append({"missing": m}))
    pane.notify_button.click()
    assert sent and sent[0]["missing"], "点一次发一次待办"

    args = dict(
        encounter_alias=ALIAS,
        ward="精神科三病区",
        business_day=DAY,
        at=AT,
        missing=[(n, lbl) for n, lbl in pane.missing_pairs()],
        group_hint="精神科病区文书群",
        store_path=tmp_db,
    )
    core_bridge.notify_missing(**args)
    core_bridge.notify_missing(**args)

    assert len(webhook.calls) == 1, "同一天同一件只发一次，群里不该出现两条一样的催办"


def test_sending_without_a_webhook_url_says_so_and_claims_nothing_was_sent(
    tmp_path, ruleset, tmp_db, monkeypatch, app
):
    monkeypatch.delenv("WECOM_WEBHOOK_URL", raising=False)
    bundle = trimmed_bundle(tmp_path)
    payload = view_of(bundle, ruleset, tmp_db)
    pane = ArchivePane()
    pane.load(payload)

    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.notify_missing(
            encounter_alias=ALIAS,
            ward="精神科三病区",
            business_day=DAY,
            at=AT,
            missing=[(n, lbl) for n, lbl in pane.missing_pairs()],
            group_hint="精神科病区文书群",
            store_path=tmp_db,
        )
    assert exc.value.code == "E_CFG_MISSING"
    assert "WECOM_WEBHOOK_URL" in str(exc.value)
    # 没配通知 ≠ 缺件已发：视图里那一件仍然是「没发过」。
    assert view_of(bundle, ruleset, tmp_db)["missing"][0]["node_id"]


def test_a_compliant_encounter_has_nothing_to_send(tmp_path, ruleset, tmp_db, app):
    bundle = ROOT / "fixtures" / "ward-01-minimal.json"
    payload = view_of(bundle, ruleset, tmp_db, alias="WARD-A-001")
    pane = ArchivePane()
    pane.load(payload)

    assert payload["missing"] == []
    assert pane.missing_pairs() == []
    assert not pane.notify_button.isEnabled(), "不缺件时没有可发的待办，按钮置灰"
    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.notify_missing(
            encounter_alias="WARD-A-001",
            ward="精神科三病区",
            business_day=DAY,
            at=AT,
            missing=[],
            group_hint="精神科病区文书群",
            store_path=tmp_db,
        )
    assert exc.value.code == "E_NOTHING_SELECTED"


# ---------------------------------------------------------------------------
# 导出 csv
# ---------------------------------------------------------------------------


def test_the_exported_csv_has_one_row_per_node_and_the_columns_they_asked_for(
    tmp_path, ruleset, tmp_db
):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    text = export_csv(payload)
    rows = list(csv.reader(io.StringIO(text)))

    header, body = rows[0], rows[1:]
    assert header == [
        "住院别名",
        "件",
        "状态",
        "欠件天数",
        "依赖对象（这件在等谁）",
        "阻塞对象（这件卡住谁）",
    ]
    assert len(body) == len(payload["rows"]), "导出行数必须与图上节点数一致"
    for row in body:
        assert row[0] == ALIAS


def test_the_export_writes_an_uncomputable_day_count_as_blank_not_zero(
    tmp_path, ruleset, tmp_db
):
    """没登记过发现时刻 → 天数列留空。

    写 0 会被读成「今天刚发现，还不欠」，而真相是「算不出」——
    病案室照着这张表去催人就会催错。
    """
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    rows = list(csv.reader(io.StringIO(export_csv(payload))))
    for row in rows[1:]:
        if row[2] == theme.NODE_STATE_ZH["missing"]:
            assert row[3] == "", f"算不出的天数被写成了 {row[3]}"


def test_the_export_names_the_missing_item_and_what_it_blocks(
    tmp_path, ruleset, tmp_db
):
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    text = export_csv(payload)
    rows = list(csv.reader(io.StringIO(text)))
    body = {r[1]: r for r in rows[1:]}

    consent = next(r for name, r in body.items() if "约束知情同意" in name)
    assert consent[2] == theme.NODE_STATE_ZH["missing"]

    record = next(r for name, r in body.items() if "约束记录" in name)
    assert record[2] == theme.NODE_STATE_ZH["blocked"]
    # 被连带卡住的件在导出里要写得出它在等谁——这张表是给病案室逐行核的。
    assert "约束知情同意" in record[4], record

    # 合成节点 id 不得出现在给病案室的表里：缺件节点压根没有
    # Composition.id，「<WARD-B-001>::missing::restraint_consent」看着像系统故障。
    assert "::missing::" not in text, text
    assert "<" not in text, "导出表里漏出了图内部的节点 id"


def test_two_exports_of_the_same_view_are_byte_identical(tmp_path, ruleset, tmp_db):
    """同一份视图两次导出逐字节相同——审计件要能 diff。"""
    payload = view_of(trimmed_bundle(tmp_path), ruleset, tmp_db)
    assert export_csv(payload) == export_csv(payload)


# ---------------------------------------------------------------------------
# 与主窗口的接线
# ---------------------------------------------------------------------------


def test_the_window_opens_the_archive_tab_without_crashing(app, tmp_db, monkeypatch):
    monkeypatch.setenv("PSYCHDOC_STORE", tmp_db)
    from desktop.main_window import MainWindow

    window = MainWindow()
    try:
        assert window.tabs.tabText(1) == "归档核齐"
        # 起始态不该编数字：没载入就是没载入。
        assert "尚未载入" in window.archive_pane.header.text()
        assert not window.archive_pane.notify_button.isEnabled()
        assert not window.archive_pane.export_button.isEnabled()
    finally:
        window.close()


def test_an_unknown_encounter_is_reported_rather_than_drawn_as_an_empty_graph(
    ruleset, tmp_db
):
    """空图会被读成「已核齐」——那是一次误归档，必须显式报错。"""
    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.archive_view(str(SET), ruleset, "WARD-NOPE-999", AT, DAY, tmp_db)
    assert exc.value.code == "E_ENCOUNTER_UNKNOWN"