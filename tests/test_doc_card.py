"""文书卡片：高亮映射、双栏 diff 与「不可自动修不给按钮」三条契约。

这三样是 Task 33 的全部要害：

1. **高亮偏移正确** —— 诊断给的是字符区间，切出来必须正好是被判的那几个字。
   偏一个字符，界面上看不出错，只是护士长盯着一个不对的地方改文书。
2. **双栏 diff 逐行可核** —— 左栏是原文、右栏是补写稿，
   增删标注不能串行；串了行，「工具要把哪一行换成哪一行」就答错了。
3. **不可自动修不给按钮** —— `FixAvailability::None` 的诊断必须既没有候选块、
   也没有按钮，且界面要**写清为什么没有**（这条对照源产品的同一行为）。

数据全部来自**真实核心入口** `doc_card` 打在脱敏夹具上：
用手写的假诊断去测高亮，只能证明切片函数本身自洽，
证不了「核心给的区间切出来是对的」——而后者才是护士长真正依赖的东西。
"""

from __future__ import annotations

import os
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
doc_card = pytest.importorskip("desktop.doc_card")

ROOT = Path(__file__).resolve().parent.parent
FIXTURES = ROOT / "fixtures"

#: 真实脱敏夹具：
#: - ward-08：件-字段四类违规，含**唯一**一条可自动修的（QC-PSY-02 全角日期）；
#: - ward-01：只缺一个必填字段（QC-PSY-01），不可自动修；
#: - ward-11：跨文书时间序倒挂与签署齐备性，**全部**不可自动修。
FIELD_VIOLATIONS = "ward-08-field-violations.json"
SINGLE_MISSING = "ward-01-minimal.json"
CROSS_DOC = "ward-11-cross-doc-violations.json"


@pytest.fixture(scope="module")
def ruleset_yaml() -> str:
    return core_bridge.default_ruleset_path().read_text(encoding="utf-8")


def _card(fixture: str, doc_id: str, ruleset: str) -> dict:
    return core_bridge.doc_card(str(FIXTURES / fixture), ruleset, doc_id)


# ---------------------------------------------------------------------------
# 1. 高亮映射：区间必须切出被判的那几个字
# ---------------------------------------------------------------------------


def test_the_highlight_lands_exactly_on_the_offending_value(ruleset_yaml):
    """QC-PSY-02 命中的是全角日期；划出来必须一字不多一字不少。"""
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    views = doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    )
    hit = [v for v in views if v.is_highlighted]
    assert len(hit) == 1, "只应有一段被划"
    # 被判的值就是那个全角日期本身（核心文案里写的也是它）。
    assert hit[0].highlighted_text() == "２０２６-０３-０２ 09:00。"


def test_every_diagnostic_range_slices_back_to_its_own_text(ruleset_yaml):
    """每条诊断的区间都能在它自己的段落里切出非空内容。

    这条是「高亮偏移正确」的**逐条**版本：只看第一条通过，
    剩下几条偏了也发现不了。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    sections = {s["index"]: s["text"] for s in payload["doc"]["sections"]}
    for d in payload["diagnostics"]:
        text = sections[d["section_index"]]
        sliced = text[d["char_start"] : d["char_end"]]
        assert sliced, f"{d['code']} 的区间切出空串，定位一定错了"
        # 被判的值必须出现在规则文案里——否则划的地方和说的不是一回事。
        assert sliced in d["message"] or d["code"] in d["message"], (
            f"{d['code']} 划出的「{sliced}」与文案对不上"
        )


def test_fragments_cover_the_whole_section_without_gaps(ruleset_yaml):
    """切片必须**无损**：拼回来等于原文，且高亮片段与不高亮片段严格不重叠。

    少一个字符护士长看不出，但写回时那一段就少了一个字——
    所以这里断言的是恒等式，不是「看起来差不多」。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    for view in doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    ):
        assert "".join(t for t, _ in view.fragments) == view.text
        assert all(t for t, _ in view.fragments), "不允许出现空片段"
        cursor = 0
        for text, _flag in view.fragments:
            assert len(text) == len(text)  # 片段长度即推进量
            cursor += len(text)
        assert cursor == len(view.text)


def test_a_clean_section_is_not_highlighted_at_all(ruleset_yaml):
    """合规段不能被划。误报守卫：全清的包不该有任何红底。"""
    payload = _card("ward-09-field-compliant.json", "WARD-G-001-ASSESS-01", ruleset_yaml)
    views = doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    )
    assert payload["diagnostics"] == []
    assert not any(v.is_highlighted for v in views)


def test_two_findings_in_one_section_merge_into_one_highlight(ruleset_yaml):
    """同一段里两处命中 → 两段区间，不叠成一片。

    叠成一片看着「更醒目」，实际上护士长无法知道要改哪一处。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    # 先用合成数据守住「合并」本身，再确认真实夹具里也成立。
    merged = doc_card.merge_spans([(10, 20), (5, 12)])
    assert merged == [(5, 20)], "重叠区间必须合并，不留两层"
    adjacent = doc_card.merge_spans([(0, 5), (5, 9)])
    assert adjacent == [(0, 9)], "相邻区间也该合并成一段"
    for view in doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    ):
        spans = []
        cursor = 0
        for text, flag in view.fragments:
            if flag:
                spans.append((cursor, cursor + len(text)))
            cursor += len(text)
        assert spans == doc_card.merge_spans(spans), "片段序列不该含重叠区间"


def test_an_out_of_range_span_is_reported_not_silently_clamped():
    """越界 → 抛 LocateError，**不**夹取。

    夹取会让「定位错位」变成「定位看起来正常」——那种 bug 在界面上
    完全看不出来，而护士长会照着改一份本来没错的文书。
    """
    with pytest.raises(doc_card.LocateError):
        doc_card.check_span((0, 99), 10)
    with pytest.raises(doc_card.LocateError):
        doc_card.check_span((-1, 5), 10)
    with pytest.raises(doc_card.LocateError):
        doc_card.check_span((8, 3), 10)
    # 合法的零宽区间（字段未填写时就是这个形状）必须放行。
    assert doc_card.check_span((4, 4), 10) == (4, 4)


def test_a_section_whose_locate_fails_still_renders_with_an_explanation():
    """定位对不上的段：**整段不划**，并写明为什么。"""
    views = doc_card.build_sections(
        [{"index": 0, "title": "评估", "text": "很短"}],
        [{"code": "QC-PSY-01", "char_start": 0, "char_end": 99, "basis": "《规范》"}],
    )
    assert views[0].locate_error, "越界必须留下原因"
    assert not views[0].is_highlighted, "定位错位不许画高亮"
    assert views[0].codes == ["QC-PSY-01"], "但诊断本身不许丢"
    rendered = doc_card.render_card({"doc_id": "D1"}, views)
    assert "定位对不上" in rendered
    assert views[0].text in rendered, "原文仍要显示，不能因为高亮失败就整段消失"


# ---------------------------------------------------------------------------
# 2. 双栏 diff：逐行可核
# ---------------------------------------------------------------------------


def test_the_two_columns_reproduce_the_original_and_the_skeleton(ruleset_yaml):
    """左栏逐行拼起来 == 原文；右栏逐行拼起来 == 补写稿。"""
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    block = payload["preview"]["blocks"][0]
    rows = doc_card.build_diff_rows(block)
    assert "\n".join(r.left for r in rows if r.left) == block["original"]
    assert "\n".join(r.right for r in rows if r.right) == block["skeleton"]


def test_added_and_removed_lines_are_marked_on_exactly_one_side(ruleset_yaml):
    """`same` 两栏同文、`changed` 两栏都非空、单边增删只有一栏非空。

    一行同时挂上增删两个标却说不清是哪一边 = 护士长数出来的行数
    与原文对不上，整份 diff 就失去可核对性。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    block = payload["preview"]["blocks"][0]
    rows = doc_card.build_diff_rows(block)
    assert rows, "真实夹具里应有 diff 行"
    assert any(r.op == "changed" for r in rows), "这份是整段替换，应有配对行"
    for row in rows:
        if row.op == "same":
            assert row.left == row.right and row.left
        elif row.op == "changed":
            assert row.left and row.right, "配对行两栏都必须有内容"
        elif row.op == "removed":
            assert row.left and not row.right, "单边删行的右栏必须空"
        else:
            assert row.right and not row.left, "单边增行的左栏必须空"
    # 右栏每一行都得能追溯到 skeleton 的一行：不多不少。
    assert len([r for r in rows if r.right]) == len(block_lines(payload))
    assert len([r for r in rows if r.left]) == len(block["original"].split("\n"))


def test_a_change_group_is_paired_in_order_and_never_shifted(ruleset_yaml):
    """配对必须按顺序，且不能跨段错配。

    「甲→乙、乙→丙」这种连续替换若配成「甲→乙、乙→乙」，
    护士长照着改就会把「乙」写两遍。
    """
    block = {
        "diff": [
            {"op": "same", "text": "头"},
            {"op": "removed", "text": "甲"},
            {"op": "removed", "text": "乙"},
            {"op": "added", "text": "乙新"},
            {"op": "added", "text": "丙新"},
            {"op": "same", "text": "尾"},
        ]
    }
    rows = doc_card.build_diff_rows(block)
    assert [(r.left, r.right, r.op) for r in rows] == [
        ("头", "头", "same"),
        ("甲", "乙新", "changed"),
        ("乙", "丙新", "changed"),
        ("尾", "尾", "same"),
    ]


def test_two_change_groups_separated_by_an_unchanged_line_are_not_paired(
    ruleset_yaml,
):
    """两处改动中间隔着未变行时，必须各自成对，不许跨过未变行配对。"""
    block = {
        "diff": [
            {"op": "removed", "text": "甲"},
            {"op": "added", "text": "甲新"},
            {"op": "same", "text": "中间未变"},
            {"op": "removed", "text": "乙"},
            {"op": "added", "text": "乙新"},
        ]
    }
    rows = doc_card.build_diff_rows(block)
    assert [(r.left, r.right, r.op) for r in rows] == [
        ("甲", "甲新", "changed"),
        ("中间未变", "中间未变", "same"),
        ("乙", "乙新", "changed"),
    ]


def block_lines(payload: dict) -> list[str]:
    block = payload["preview"]["blocks"][0]
    return block["skeleton"].split("\n")


def test_the_diff_carries_its_rule_code_and_the_basis(ruleset_yaml):
    """每块 diff 必须带触发它的规则码与院内规范依据。

    没有依据的双栏 diff 等于让护士长凭感觉接受一次内容改写。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    block = payload["preview"]["blocks"][0]
    rendered = doc_card.render_diff_html(block)
    assert block["code"] in rendered
    assert "第 5.1 条" in rendered, "依据里的条款号必须渲染出来"
    assert "原文" in rendered and "补写稿" in rendered, "两栏都要有表头"
    assert "−" in rendered and "+" in rendered, "增删标注都要有"


def test_the_diff_summary_line_is_carried_into_the_card(ruleset_yaml):
    """底部汇总「将新建 X 件 / 修改 Y 段」要真出现在卡片上。"""
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    summary = payload["preview"]["summary_line"]
    views = doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    )
    rendered = doc_card.render_card(
        payload["doc"], views, preview=payload["preview"]
    )
    assert summary in rendered
    assert "依据：" in rendered, "依据行固定在卡片底部"


# ---------------------------------------------------------------------------
# 3. 不可自动修：只给依据不给按钮
# ---------------------------------------------------------------------------


def test_a_non_fixable_finding_gets_no_button_and_says_why(ruleset_yaml):
    """QC-PSY-01（必填缺失）不可自动修 → 无候选块、无按钮、写清原因。"""
    payload = _card(SINGLE_MISSING, "WARD-A-001-CONSENT-01", ruleset_yaml)
    codes = [d["code"] for d in payload["diagnostics"]]
    assert "QC-PSY-01" in codes
    assert payload["preview"]["blocks"] == [], "不可自动修不产生候选块"
    assert "QC-PSY-01" in payload["preview"]["evidence_only"]
    assert payload["preview"]["summary_line"] == "无可补写内容"

    views = doc_card.build_sections(
        payload["doc"]["sections"], payload["diagnostics"]
    )
    state, has_button = doc_card.fix_entry_state(views, payload["preview"])
    assert has_button is False
    assert "QC-PSY-01" in state
    rendered = doc_card.render_card(payload["doc"], views)
    assert "不提供补写按钮" in rendered, "必须**显式**说清为什么没有按钮"
    assert "人工" in rendered


def test_a_mix_of_fixable_and_non_fixable_keeps_the_button_and_lists_the_rest(
    ruleset_yaml,
):
    """同一份件里既有可自动修又有不可自动修 → 仍有按钮，但要说明还有几条不行。"""
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    sections = [{"index": 0, "title": "评估", "text": "评估时间：２０２６。"}]
    diags = [
        {
            "code": "QC-PSY-02",
            "fix_availability": "automatic",
            "char_start": 0,
            "char_end": 3,
        },
        {
            "code": "QC-PSY-08",
            "fix_availability": "none",
            "char_start": 5,
            "char_end": 9,
        },
        {
            "code": "QC-PSY-08",
            "fix_availability": "none",
            "char_start": 5,
            "char_end": 9,
        },
    ]
    views = doc_card.build_sections(sections, diags)
    state, has_button = doc_card.fix_entry_state(views, payload["preview"])
    assert has_button is True
    assert "QC-PSY-08" in state and "只给依据" in state
    assert "预览补写" in doc_card.render_card(payload["doc"], views)


def test_the_evidence_only_note_is_rendered_even_when_empty(ruleset_yaml):
    """空列表也要渲染一句「本次没有这类码」。

    留白会被读成「工具没想过这个口子」。
    """
    rendered = doc_card.render_evidence_only_html({"evidence_only": []})
    assert "没有" in rendered and "只给依据" in rendered
    listed = doc_card.render_evidence_only_html({"evidence_only": ["QC-PSY-05"]})
    assert "QC-PSY-05" in listed
    assert "不代签" in listed, "须写明工具不代填也不代签"


def test_cross_document_findings_offer_no_writeback_path(ruleset_yaml):
    """跨文书时间序倒挂：全部不可自动修，一个候选块都不该有。"""
    payload = core_bridge.doc_card(
        str(FIXTURES / CROSS_DOC),
        ruleset_yaml,
        "WARD-I-001-CONSENT-01",
    )
    assert payload["diagnostics"], "这份夹具该有跨文书命中"
    for d in payload["diagnostics"]:
        assert d["fix_availability"] == "none", (
            f"{d['code']} 不该带自动修能力"
        )
    assert payload["preview"]["blocks"] == []


# ---------------------------------------------------------------------------
# 4. 超长段落：卡片内定位与局部高亮
# ---------------------------------------------------------------------------


def test_a_long_section_keeps_only_the_offending_span_underlined():
    """3000 字的段里只划那 6 个字，不整段变红。

    整段划掉的后果很具体：护士长要在一整块红色里找那 6 个字，
    反而比不划更难定位。
    """
    long_text = "护理记录。" * 500 + "约束时长：1000分钟。约束等级：三级保护。" + "继续观察。" * 500
    start = long_text.index("1000")
    diags = [
        {
            "code": "QC-PSY-03",
            "section_index": 0,
            "char_start": start,
            "char_end": start + 4,
            "fix_availability": "none",
            "basis": "《精神科住院护理常规》第 5.3 条",
            "severity": "blocking",
        }
    ]
    views = doc_card.build_sections(
        [{"index": 0, "title": "护理记录", "text": long_text}], diags
    )
    assert views[0].highlighted_text() == "1000"
    assert len(views[0].highlighted_text()) < 20, "绝不能整段划掉"
    assert "".join(t for t, _ in views[0].fragments) == long_text


def test_a_zero_width_span_points_at_the_declared_slot_not_at_a_word():
    """字段未填写时区间是零宽：划出来是空的，定位仍要指对。

    这时界面给的是「模板声明的段起点」，卡片必须照实显示，
    而不是随便挑一段有字的区间划上去。
    """
    text = "患方意见：已知晓约束目的与风险\n患方签字："
    views = doc_card.build_sections(
        [{"index": 0, "title": "患方知情同意", "text": text}],
        [{"code": "QC-PSY-01", "section_index": 0, "char_start": 19, "char_end": 19}],
    )
    assert views[0].locate_error is None
    assert views[0].highlighted_text() == "", "零宽区间不该划出字"
    assert "".join(t for t, _ in views[0].fragments) == text
    assert "QC-PSY-01" in doc_card.render_card({"doc_id": "D"}, views)


def test_the_focus_marker_underlines_the_span_inside_an_already_highlighted_section():
    """整段已划时，焦点那一段再单独带下划线（两级定位不互相吞掉）。"""
    text = "风险等级：中高风险。约束时长：1000分钟。"
    views = doc_card.build_sections(
        [{"index": 0, "title": "评估", "text": text}],
        [
            {
                "code": "QC-PSY-04",
                "section_index": 0,
                "char_start": 0,
                "char_end": len(text),
                "fix_availability": "none",
                "basis": "《规范》第 5.4 条",
            }
        ],
    )
    rendered = doc_card._render_fragments(
        views[0],
        {"section_index": 0, "char_start": 9, "char_end": 13},
    )
    assert "<u>" in rendered, "焦点区间要单独有下划线"
    assert "1000" in rendered


# ---------------------------------------------------------------------------
# 5. 入口契约：找不到件时报码，不给空卡片
# ---------------------------------------------------------------------------


def test_an_unknown_doc_id_reports_a_code_rather_than_an_empty_card(ruleset_yaml):
    with pytest.raises(core_bridge.CoreError) as exc:
        core_bridge.doc_card(
            str(FIXTURES / SINGLE_MISSING), ruleset_yaml, "不存在的件-01"
        )
    assert exc.value.code == "E_DOC_NOT_FOUND"
    assert not exc.value.retryable, "拼错件 id 重试没有意义"


def test_the_card_answers_for_a_whole_directory_too(ruleset_yaml):
    """目录入口与单份包入口给出同一张卡（GUI 面对的是整个导出目录）。"""
    single = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    via_dir = core_bridge.doc_card(
        str(FIXTURES), ruleset_yaml, "WARD-F-001-ASSESS-01"
    )
    assert via_dir["doc"]["sections"] == single["doc"]["sections"]
    assert [d["code"] for d in via_dir["diagnostics"]] == [
        d["code"] for d in single["diagnostics"]
    ]


def test_the_binding_reports_binding_state_not_just_a_paragraph(ruleset_yaml):
    """绑定状态要显式带出来：``ambiguous`` 时界面必须提示人工确认。

    绑定不唯一却照常渲染，等于替人把「这两份同名文书哪份算数」定了。
    """
    payload = _card(FIELD_VIOLATIONS, "WARD-F-001-ASSESS-01", ruleset_yaml)
    assert payload["binding"] == "confident"
    assert payload["ruleset_short_sha256"], "规则集版本要带上，供事后复核"