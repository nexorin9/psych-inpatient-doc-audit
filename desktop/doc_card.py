"""文书卡片：诊断定位 → 段落高亮 → 依据行 → 双栏补写 diff。

数据来自核心入口 `doc_card`（一件件 id → 段落 / 字段 / 诊断 / 预览），
本模块只做**渲染**：把 `(doc_id, section_index, char_range)` 映射成
「哪一段、哪一段内的哪几个字符」要划出来，再把结果变成 HTML 与
双栏 diff 的行模型。

## 三条不能破的渲染纪律

1. **高亮偏移必须按字符、不是按字节。** 核心的 `Span` 是字符区间；
   拿它去切 Python 字符串之前不能做任何字节换算，否则一份带生僻字的
   文书会把「患方签字」四个字高亮到「签字」上去——而界面上看不出错，
   只是高亮位置莫名其妙。
2. **区间越界不许静默夹取。** 越界说明核心与界面拿到的不是同一份文本
   （规则集改了、件被重新导出），此时宁可把该段标成「定位对不上」，
   也不画一个看起来正常、其实指错位置的底色。
3. **不可自动修是一等状态。** `FixAvailability::None` 的诊断
   **不渲染补写按钮**，并在按钮位置显式写清「只给依据」。
   源产品在同一处也是这么分的：能自动修的才给替换建议，
   不能的只留定位与依据。按钮位置留白会让护士长以为工具漏报了。
"""

from __future__ import annotations

import html
from dataclasses import dataclass, field
from typing import Any, Iterable

from . import theme

#: 高亮区间的左闭右开口径，与 Rust 侧 `Span` 一致。
Span = tuple[int, int]


class LocateError(ValueError):
    """定位对不上：区间越界 / 负数 / 起止颠倒。

    界面据此把该段标成「定位对不上」而不是画一个假的高亮——
    假高亮比不高亮危险得多，因为它看起来是可信的。
    """


def clamp_span(span: Span, length: int) -> Span:
    """把区间夹进 ``[0, length]``，**只在明确知道越界多少时才用**。

    调用方一般不该用这个函数：夹取会让「定位错位」变成
    「定位看起来正常」。卡片渲染走 :func:`check_span`。
    """
    start = max(0, min(span[0], length))
    end = max(start, min(span[1], length))
    return start, end


def check_span(span: Span, length: int) -> Span:
    """校验区间落在文本内，越界即抛 :class:`LocateError`。"""
    start, end = span
    if start < 0 or end < 0:
        raise LocateError(f"区间含负数：[{start}, {end})")
    if end < start:
        raise LocateError(f"区间起止颠倒：[{start}, {end})")
    if end > length:
        raise LocateError(
            f"区间 [{start}, {end}) 越出该段长度 {length}——"
            "界面与核心拿到的不是同一份文本，不画高亮"
        )
    return start, end


def merge_spans(spans: Iterable[Span]) -> list[Span]:
    """合并重叠 / 相邻区间，避免同一处被套两层底色。

    输入不必有序；输出按起点升序，且互不重叠。
    """
    ordered = sorted((s for s in spans), key=lambda s: (s[0], s[1]))
    out: list[Span] = []
    for start, end in ordered:
        if out and start <= out[-1][1]:
            out[-1] = (out[-1][0], max(out[-1][1], end))
        else:
            out.append((start, end))
    return out


@dataclass
class SectionView:
    """一个段落在卡片上的渲染形态。

    ``fragments`` 是切好的片段列表：``(text, highlight)``。
    切成片段而不是拼 HTML，是为了让「高亮偏移是否正确」这件事
    可以在**没有 Qt 的环境**里被断言（见 tests/test_doc_card.py）。
    """

    index: int
    title: str
    text: str
    fragments: list[tuple[str, bool]]
    codes: list[str] = field(default_factory=list)
    diagnostics: list[dict[str, Any]] = field(default_factory=list)
    #: 定位对不上的原因。非空时界面显示这句话而不是画高亮。
    locate_error: str | None = None

    @property
    def is_highlighted(self) -> bool:
        return any(flag for _, flag in self.fragments)

    def highlighted_text(self) -> str:
        """只拼被划掉的部分——用来断言「高亮正好落在被判的那几个字上」。"""
        return "".join(t for t, flag in self.fragments if flag)


def build_sections(
    sections: list[dict[str, Any]], diagnostics: list[dict[str, Any]]
) -> list[SectionView]:
    """把诊断定位映射到段落视图。

    多条诊断落在同一段时区间合并；落在不存在的段上时**不静默丢弃**——
    该诊断仍然留在视图里，由调用方决定怎么显示（卡片底部会列出来）。
    """
    by_section: dict[int, list[dict[str, Any]]] = {}
    for d in diagnostics:
        by_section.setdefault(int(d.get("section_index", 0)), []).append(d)

    views: list[SectionView] = []
    for raw in sections:
        index = int(raw.get("index", 0))
        text = raw.get("text") or ""
        diags = by_section.get(index, [])
        spans: list[Span] = []
        error: str | None = None
        try:
            for d in diags:
                spans.append(
                    check_span(
                        (int(d["char_start"]), int(d["char_end"])),
                        len(text),
                    )
                )
        except LocateError as exc:
            error = str(exc)
            spans = []
        views.append(
            SectionView(
                index=index,
                title=raw.get("title") or f"第 {index} 段",
                text=text,
                fragments=split_fragments(text, merge_spans(spans)),
                codes=[d["code"] for d in diags],
                diagnostics=list(diags),
                locate_error=error,
            )
        )
    return views


def split_fragments(text: str, spans: list[Span]) -> list[tuple[str, bool]]:
    """把文本按高亮区间切成 ``(片段, 是否高亮)`` 序列。

    不高亮的部分与高亮的部分严格覆盖全文且不重叠——
    ``"".join(片段) == text`` 恒成立，测试逐条钉这一条。
    """
    out: list[tuple[str, bool]] = []
    cursor = 0
    for start, end in spans:
        if start > cursor:
            out.append((text[cursor:start], False))
        if end > start:
            out.append((text[start:end], True))
        cursor = max(cursor, end)
    if cursor < len(text):
        out.append((text[cursor:], False))
    return out


# ---------------------------------------------------------------------------
# 渲染
# ---------------------------------------------------------------------------


def render_card(
    doc: dict[str, Any],
    sections: list[SectionView],
    *,
    focus: dict[str, Any] | None = None,
    preview: dict[str, Any] | None = None,
) -> str:
    """整张卡片的 HTML。

    信息层级（`docs/concepts/visual-target.md`）：码最显眼 → 段落正文 →
    依据行最弱。依据行固定在卡片**底部**而不是跟在每段后面，
    因为护士长的动作是「看一眼就翻页」，依据要落在同一条视线上。
    """
    parts: list[str] = []
    doc_id = doc.get("doc_id", "")
    template_id = doc.get("template_id", "")
    parts.append(
        f"<p><span style='font-family:{theme.MONO_FAMILY};font-size:15px;"
        f"font-weight:600'>{_esc(doc_id)}</span> "
        f"<span style='color:{theme.MUTED};font-size:12px'>"
        f"{_esc(template_id)} · {len(sections)} 段</span></p>"
    )

    for view in sections:
        parts.append(_render_section(view, focus))

    parts.append(_render_basis_row(sections, preview))
    return "".join(parts)


def _render_section(view: SectionView, focus: dict[str, Any] | None) -> str:
    head_bits = [f"第 {view.index} 段"]
    if view.title:
        head_bits.append(view.title)
    codes_html = ""
    if view.codes:
        badges = "".join(
            f"<span style='font-family:{theme.MONO_FAMILY};color:{theme.DANGER};"
            f"font-weight:600;margin-right:6px'>{_esc(c)}</span>"
            for c in dict.fromkeys(view.codes)
        )
        codes_html = f"<span style='margin-left:8px'>{badges}</span>"

    if view.locate_error:
        # 定位对不上：明说，不画高亮，**但原文照常显示**。
        # 把原文一起藏掉，护士长就既看不到问题、也看不到内容，
        # 只剩一句「定位对不上」——那等于把工具坏掉这件事藏起来了。
        return (
            f"<p style='margin:10px 0'><span style='color:{theme.MUTED};font-size:12px'>"
            f"{' · '.join(_esc(b) for b in head_bits)}{codes_html}</span></p>"
            f"<p style='background:{theme.VIOLATION_BG};border-left:3px solid {theme.DANGER};"
            f"padding:8px;color:{theme.MUTED};font-size:12px'>定位对不上，未标出具体位置："
            f"{_esc(view.locate_error)}</p>"
            f"<p style='white-space:pre-wrap'>{_esc(view.text)}</p>"
        )

    body = _render_fragments(view, focus)
    if not view.is_highlighted:
        bg, border = "#FFFFFF", "transparent"
    else:
        bg, border = theme.VIOLATION_BG, f"border-left:3px solid {theme.DANGER};"
    return (
        f"<p style='margin:10px 0'><span style='color:{theme.MUTED};font-size:12px'>"
        f"{' · '.join(_esc(b) for b in head_bits)}{codes_html}</span></p>"
        f"<p style='background:{bg};{border}padding:8px;white-space:pre-wrap'>"
        f"{body}</p>"
    )


def _render_fragments(view: SectionView, focus: dict[str, Any] | None) -> str:
    """把片段渲染成 HTML；焦点段内的那几段再加一层下划线。

    段内局部高亮：区间不是整段时，只有那几段字符带下划线，
    其余段落文字保持正常——整段划掉会让 3000 字的护理记录一整块变红，
    真正出问题的那 6 个字反而找不到了。

    两级标记是叠加的：**先按焦点区间再切一刀**。
    直接在高亮片段外面套 `<u>` 会把整条高亮片段一起划上，
    「这一处里有更具体的位置」这个信息就丢了。
    """
    focus_span: Span | None = None
    if focus is not None and int(focus.get("section_index", -1)) == view.index:
        try:
            focus_span = check_span(
                (int(focus["char_start"]), int(focus["char_end"])), len(view.text)
            )
        except LocateError:
            focus_span = None

    cursor = 0
    out: list[str] = []
    for text, highlighted in view.fragments:
        start, end = cursor, cursor + len(text)
        cursor = end
        if highlighted:
            inner = _mark_focus(text, start, focus_span)
            out.append(
                f"<span style='background:{theme.DANGER};color:#FFFFFF;"
                f"border-radius:2px'>{inner}</span>"
            )
        else:
            out.append(_mark_focus(text, start, focus_span))
    return "".join(out)


def _mark_focus(text: str, start: int, focus_span: Span | None) -> str:
    """在片段内部把焦点区间单独加下划线，其余原样。

    焦点区间与片段不相交时**不套任何标签**——给整段加下划线
    会让「焦点在哪」变得比不标还难找。
    """
    if focus_span is None or not text:
        return _esc(text)
    lo = max(start, focus_span[0])
    hi = min(start + len(text), focus_span[1])
    if lo >= hi:
        return _esc(text)
    return (
        _esc(text[: lo - start])
        + "<u>"
        + _esc(text[lo - start : hi - start])
        + "</u>"
        + _esc(text[hi - start :])
    )


def _render_basis_row(
    sections: list[SectionView], preview: dict[str, Any] | None
) -> str:
    """卡片底部固定一行：**依据 + 补写入口 + 汇总**。

    这一行是本任务与源产品「行号 + 划线 + 提示」三件套的第三件：
    源产品在每条诊断下面挂一行脚注说明，本产品挂在卡片底部，
    因为一张卡片可能同时标出好几处，需要一个固定的落点。
    """
    codes: list[str] = []
    for view in sections:
        for d in view.diagnostics:
            codes.append(d["code"])
    codes = list(dict.fromkeys(codes))
    if not codes:
        return (
            f"<p style='color:{theme.MUTED};font-size:12px;border-top:1px solid "
            f"{theme.BORDER};padding-top:6px;margin-top:10px'>"
            f"本件未发现规则命中项。</p>"
        )

    basis = next(
        (d["basis"] for v in sections for d in v.diagnostics if d.get("basis")), ""
    )
    fix_zh, has_fix = fix_entry_state(sections, preview)
    parts = [
        f"<p style='color:{theme.MUTED};font-size:12px;border-top:1px solid "
        f"{theme.BORDER};padding-top:6px;margin-top:10px'>"
        f"依据：{_esc(basis)}</p>",
        _render_fix_row(fix_zh, has_fix),
    ]
    if preview:
        parts.append(
            f"<p style='color:{theme.MUTED};font-size:12px'>"
            f"补写预览：{_esc(preview.get('summary_line', ''))}</p>"
        )
    return "".join(parts)


def _render_fix_row(fix_zh: str, has_fix: bool) -> str:
    """补写入口那一行。

    `has_fix=False` 时**仍然渲染这一行**，写清为什么没有按钮。
    把整行藏掉 = 让护士长以为工具漏报了，而不是「这条按规矩不能自动补」。
    """
    if has_fix:
        return (
            f"<p style='font-size:12px'><span style='color:{theme.PRIMARY};"
            f"font-weight:600'>▸ 预览补写</span>"
            f"<span style='color:{theme.MUTED}'>（{_esc(fix_zh)}）</span></p>"
        )
    return (
        f"<p style='font-size:12px'><span style='color:{theme.MUTED}'>"
        f"不提供补写按钮：{_esc(fix_zh)}——本条只给依据，须人工处理。</span></p>"
    )


def fix_entry_state(
    sections: list[SectionView], preview: dict[str, Any] | None
) -> tuple[str, bool]:
    """`(补写状态文案, 是否给按钮)`。

    规则：
    - 全部命中都不可自动修 → 不给按钮，写清是哪些码；
    - 只要有**可自动修**的命中 → 给按钮，同时说明还有几条只能人工；
    - 一个命中都没有 → 无可补写（卡片不会走到这里，但函数不假设）。
    """
    diags = [d for v in sections for d in v.diagnostics]
    if not diags:
        return "无可补写内容", False
    fixable = [d for d in diags if d.get("fix_availability") == "automatic"]
    evidence_only = [d for d in diags if d.get("fix_availability") != "automatic"]
    if not fixable:
        only = "、".join(dict.fromkeys(d["code"] for d in evidence_only))
        return f"{only} 不可自动修", False
    if evidence_only:
        only = "、".join(dict.fromkeys(d["code"] for d in evidence_only))
        return f"另有 {only} 不可自动修，只给依据", True
    return "可预览补写", True


# ---------------------------------------------------------------------------
# 双栏 diff
# ---------------------------------------------------------------------------


@dataclass
class DiffRow:
    """双栏 diff 的一行。左右各自一段文字 + 各自的标注。"""

    left: str
    right: str
    op: str  # same / changed / added / removed


def build_diff_rows(block: dict[str, Any]) -> list[DiffRow]:
    """候选块 → 双栏 diff 行模型。

    **只做配对，不改文案**：左栏逐行是原文，右栏逐行是骨架渲染结果。
    配对规则是「一段连续的 removed 后面紧跟的 added 逐行对齐」：
    ``removed`` 与 ``added`` 交替出现时不能各自成行，否则同一处改动
    会在两栏里各渲染一遍，护士长数出来的行数对不上原文。

    配对出来的一行 ``op`` 取 ``removed``（表示「这一行将被替换」），
    只有多出来的删行或增行才单独成行——那才是真正的单边增删。
    """
    diff = block.get("diff") or []
    rows: list[DiffRow] = []
    pending_removed: list[str] = []
    pending_added: list[str] = []

    def flush() -> None:
        """把攒下的 removed / added 逐行配成双栏行，然后清空。

        配对后按两栏都有没有分成三档：``changed``（两边都有，这一行将被
        替换）、``removed``（只有原文，单边删行）、``added``（只有补写稿，
        单边增行）。分成三档而不是一律记 ``removed``，是因为界面上
        「这一行是换掉」与「这一行是删掉」的整改动作不一样。
        """
        for i in range(max(len(pending_removed), len(pending_added))):
            left = pending_removed[i] if i < len(pending_removed) else ""
            right = pending_added[i] if i < len(pending_added) else ""
            op = "changed" if left and right else ("removed" if left else "added")
            rows.append(DiffRow(left=left, right=right, op=op))
        pending_removed.clear()
        pending_added.clear()

    for line in diff:
        if line["op"] == "same":
            # 遇到未变行：先把前面攒下的增删配完，再放这一行。
            # 不先 flush 的话，相隔两段的改动会被错配成一对。
            flush()
            rows.append(DiffRow(left=line["text"], right=line["text"], op="same"))
        elif line["op"] == "removed":
            pending_removed.append(line["text"])
        else:
            pending_added.append(line["text"])
    flush()
    return rows


def render_diff_html(block: dict[str, Any]) -> str:
    """双栏 diff 的 HTML：左「原文」、右「补写稿」，逐行标增删。"""
    rows = build_diff_rows(block)
    head = (
        f"<p><span style='font-family:{theme.MONO_FAMILY};color:{theme.DANGER};"
        f"font-weight:600'>{_esc(block.get('code', ''))}</span> "
        f"<span style='color:{theme.MUTED};font-size:12px'>"
        f"{_esc(block.get('section_title', ''))} · 新增 {block.get('added_lines', 0)} 行"
        f"</span></p>"
    )
    body = [
        "<table width='100%' cellspacing='0' cellpadding='2'>",
        f"<tr><td width='50%' style='color:{theme.MUTED};font-size:12px'>原文</td>"
        f"<td width='50%' style='color:{theme.MUTED};font-size:12px'>补写稿</td></tr>",
    ]
    for row in rows:
        left_html = _esc(row.left)
        right_html = _esc(row.right)
        if row.op == "same":
            left_html, right_html = _esc(row.left), _esc(row.right)
        elif row.op == "removed":
            left_html = (
                f"<span style='background:{theme.VIOLATION_BG};color:{theme.DANGER}'>"
                f"− {left_html}</span>"
            )
        elif row.op == "added":
            right_html = (
                f"<span style='background:#E6F4EA;color:{theme.OK}'>"
                f"+ {right_html}</span>"
            )
        else:  # changed：两边都改，逐行核对「换成哪一行」
            left_html = (
                f"<span style='background:{theme.VIOLATION_BG};color:{theme.DANGER}'>"
                f"− {left_html}</span>"
            )
            right_html = (
                f"<span style='background:#E6F4EA;color:{theme.OK}'>"
                f"+ {right_html}</span>"
            )
        body.append(
            f"<tr><td style='white-space:pre-wrap;font-size:12px'>{left_html}</td>"
            f"<td style='white-space:pre-wrap;font-size:12px'>{right_html}</td></tr>"
        )
    body.append("</table>")
    tail = (
        f"<p style='color:{theme.MUTED};font-size:12px;border-top:1px solid "
        f"{theme.BORDER};padding-top:6px'>依据：{_esc(block.get('basis', ''))}</p>"
    )
    return head + "".join(body) + tail


def render_evidence_only_html(preview: dict[str, Any]) -> str:
    """「只给依据不给按钮」那些码的说明段。

    **显式渲染**，即使列表为空也渲染（空时写一句「本次没有这类码」），
    让护士长知道工具把这个口子显式想过，而不是漏了。
    """
    codes = preview.get("evidence_only") or []
    if not codes:
        return (
            f"<p style='color:{theme.MUTED};font-size:12px'>"
            f"本次没有「只给依据」的码。</p>"
        )
    listed = "、".join(_esc(c) for c in codes)
    return (
        f"<p style='color:{theme.MUTED};font-size:12px'>"
        f"只给依据不给补写入口：{listed}——这些要人工核对，"
        f"工具不代填，也不代签。</p>"
    )


def card_focus_target(diagnostics: list[dict[str, Any]]) -> dict[str, Any] | None:
    """卡片该聚焦哪条：取第一条**阻断**（没有阻断就取第一条）。

    只给一条而不是全部：护士长点开的是「一条问题」，
    卡片要能回答「就是这里」，全部划出来等于一条都没划。
    """
    if not diagnostics:
        return None
    blocking = [d for d in diagnostics if d.get("severity") == "blocking"]
    return blocking[0] if blocking else diagnostics[0]


def _esc(text: Any) -> str:
    return html.escape(str(text), quote=True)


__all__ = [
    "DiffRow",
    "LocateError",
    "SectionView",
    "build_diff_rows",
    "build_sections",
    "card_focus_target",
    "check_span",
    "clamp_span",
    "fix_entry_state",
    "merge_spans",
    "render_card",
    "render_diff_html",
    "render_evidence_only_html",
    "split_fragments",
]