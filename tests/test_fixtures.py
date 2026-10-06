"""夹具库与夹具生成器（Task 39）。

三件事在这里被钉住，缺一条这套夹具就只是一堆好看的 JSON：

1. **可复现** —— 同一份参数两次生成逐字节相同。质控办复核一份病历问的是
   「这批数据怎么来的」，答不上来的批次等于不可信。因此生成器不取当前时间、
   不用随机数；测试逐字节比对把这条钉死。
2. **脱敏** —— 生成物里不出现姓名 / 住院号 / 身份证件号。
3. **基准包每类违规都真的能被核心看见** —— 而且是**从各自该走的通道**看见：
   缺件与绑定歧义本就不产规则诊断，硬塞进规则表只会多两条永远判不中的码。

另外守着生成器与交付规则集**不漂移**：夹具的字段集必须与
`rulesets/hospital.yaml` 的 `templates` 段一致，否则夹具失去「外部标尺」的意义
（改规则集就会悄悄改掉夹具，于是测的永远是「规则集和自己」）。
"""

from __future__ import annotations

import json
import pathlib
import re
import sys

import pytest

ROOT = pathlib.Path(__file__).resolve().parent.parent
if str(ROOT) not in sys.path:
    sys.path.insert(0, str(ROOT))

from scripts import fixture_kit  # noqa: E402

pytest.importorskip(
    "psychdoc_ffi", reason="核心绑定 psychdoc_ffi 未构建，跳过（见 conftest.py）"
)
import psychdoc_ffi as core  # noqa: E402

RULESET = (ROOT / "rulesets" / "hospital.yaml").read_text(encoding="utf-8")
GENERATED = ROOT / "fixtures" / "generated"

#: 脱敏守卫：命中任一即视为夹具带出了真实标识。
#: 形状比字面更宽——「住院号：」后面跟什么都算，光扫「住院号」三个字会漏。
BANNED_PATTERNS = [
    ("姓名", re.compile(r"姓名\s*[:：]")),
    ("住院号", re.compile(r"住院号\s*[:：]?\s*\S")),
    ("身份证", re.compile(r"身份证\s*[:：]?\s*\S")),
    ("证件号", re.compile(r"证件号码?\s*[:：]?\s*\S")),
    ("18位身份证样式", re.compile(r"\b\d{17}[\dXx]\b")),
]


def _review(text: str) -> dict:
    return core.check_bundle(text, RULESET)["summary"]


def _graph(text: str, node_id: str) -> dict:
    return core.graph_both_directions(text, RULESET, node_id)


def _baseline_file(alias: str) -> pathlib.Path:
    candidates = sorted(GENERATED.glob(f"{alias.lower()}*.json"))
    assert candidates, f"{alias} 的基准夹具不在 {GENERATED}"
    return candidates[0]


# ——— 可复现 ———


def test_the_same_parameters_produce_byte_identical_output():
    """同参数两次生成必须逐字节相同：可复现是审计的前提，不是加分项。"""
    spec = fixture_kit.EncounterSpec(
        alias="WARD-T-001",
        defect=(fixture_kit.DEFECT_TIME_INVERSION, fixture_kit.DEFECT_MISSING_ITEM),
        note="可复现性用例",
    )
    first = fixture_kit.serialize(fixture_kit.build_bundle(spec))
    second = fixture_kit.serialize(fixture_kit.build_bundle(spec))
    assert first == second
    assert len(first) > 0


def test_a_regenerated_baseline_pack_matches_what_is_on_disk():
    """落盘的基准包必须就是当前生成器产出的那份。

    换了生成器却忘了重新生成，夹具库就会名不副实——质控办照着 `_fixture_meta`
    里的参数复现，得到的却是另一批数据。这条比「可复现」更要命。
    """
    for spec, bundle in zip(fixture_kit.BASELINE_PACK, fixture_kit.build_baseline()):
        path = _baseline_file(spec.alias)
        on_disk = fixture_kit.serialize(bundle)
        assert path.read_bytes() == on_disk, (
            f"{path.name} 与生成器当前产物不一致；"
            f"改过 scripts/fixture_kit.py 的话执行 python3 scripts/fixture_kit.py --out fixtures/generated"
        )


def test_no_generated_fixture_embeds_a_wall_clock_timestamp():
    """生成器不得取当前时间——那会让「同参数两次一致」在某一天之后悄悄失效。

    这里不写死某个日期，而是断言生成时刻与 `_fixture_meta` 里记录的业务日期
    不在同一年的可能太大；改成断言：全文里出现的日期都落在业务窗口内。
    """
    spec = fixture_kit.EncounterSpec(alias="WARD-T-002", defect=(), note="无当前时间")
    text = fixture_kit.serialize(fixture_kit.build_bundle(spec)).decode("utf-8")
    found = set(re.findall(r"\b20\d{2}-\d{2}-\d{2}\b", text))
    assert found == {"2026-03-04"}, f"出现了预期业务窗口之外的日期：{sorted(found)}"


# ——— 脱敏 ———


def test_no_fixture_carries_a_patient_identifier():
    """脱敏守卫：手写夹具与生成夹具一起扫。

    走字节读取而不是 `read()`：`fixtures/dirty/` 里有 GBK 导出，用文本模式读
    会在那里当场抛异常，等于这个守卫根本没扫到它——和 Rust 侧那条测试同样的坑。
    """
    scanned = 0
    for path in sorted((ROOT / "fixtures").rglob("*.json")):
        raw = path.read_bytes()
        try:
            text = raw.decode("utf-8")
        except UnicodeDecodeError:
            text = _decode_export_bytes(raw)
        for label, pattern in BANNED_PATTERNS:
            hit = pattern.search(text)
            assert hit is None, f"{path.relative_to(ROOT)} 命中禁用标识「{label}」：{hit.group(0)!r}"
        scanned += 1
    assert scanned >= 20, f"只扫到 {scanned} 份夹具，守卫形同虚设"


def _decode_export_bytes(raw: bytes) -> str:
    """与核心同口径的编码嗅探，供脏夹具扫描复用（dirty/ 里有 GBK）。"""
    for encoding in ("utf-8", "gbk"):
        try:
            return raw.decode(encoding)
        except UnicodeDecodeError:
            continue
    return raw.decode("utf-8", errors="replace")


def test_identifiers_use_the_desensitized_alias_shape():
    """住院别名必须是 `WARD-X-NNN` 形状；执行者一律 `<EXAMPLE_*>` 占位。"""
    for spec in fixture_kit.BASELINE_PACK:
        assert re.fullmatch(r"WARD-[A-Z]-\d{3}", spec.alias), spec.alias
        text = fixture_kit.serialize(fixture_kit.build_bundle(spec)).decode("utf-8")
        for role in ("<EXAMPLE_NURSE>", "<EXAMPLE_PRACTITIONER>", "<EXAMPLE_SIGNATURE>"):
            assert role in text, f"{spec.alias} 缺脱敏占位 {role}"
        assert "Practitioner/<EXAMPLE" in text


def test_each_fixture_declares_its_own_generation_parameters():
    """每份生成夹具都要自带 `_fixture_meta`：这批数据怎么来的，必须写在数据里。"""
    for path in sorted(GENERATED.glob("*.json")):
        meta = json.loads(path.read_text(encoding="utf-8"))["_fixture_meta"]
        assert meta["generator"] == fixture_kit.GENERATOR_NAME
        assert meta["generator_version"] == fixture_kit.GENERATOR_VERSION
        assert re.fullmatch(r"WARD-[A-Z]-\d{3}", meta["encounter_alias"])
        assert meta["ward"] == meta["encounter_alias"].split("-")[0]
        assert isinstance(meta["doc_types"], list) and meta["doc_types"]
        # 预期观察点与注入的违规一一对应，且都写成人话
        for defect in meta["defects"]:
            assert defect in meta["expectation_zh"]
            assert meta["expectation_zh"][defect].strip()
        assert meta["note"].strip(), f"{path.name} 的 note 是空的"


# ——— 基准包：四类违规各走各的通道 ———


def test_the_baseline_pack_has_exactly_four_defect_classes_each_used_once():
    """基准包是「四类违规各一次 + 一次全合规」，不多不少。

    多一次就分不清是哪条在起作用；少一次就有一条没被覆盖。合规那次是
    误报守卫，混进违规计数里会让「零诊断」这条断言失去意义。
    """
    defects: list[str] = []
    for spec in fixture_kit.BASELINE_PACK:
        defects.extend(spec.defect)
    assert sorted(defects) == sorted(fixture_kit.ALL_DEFECTS), (
        f"基准包注入的违规是 {sorted(defects)}，应为 {sorted(fixture_kit.ALL_DEFECTS)} 各一次"
    )
    assert sum(1 for s in fixture_kit.BASELINE_PACK if not s.defect) == 1, "缺全合规那一次"


def test_a_time_inversion_is_reported_on_both_documents_that_disagree():
    """时序倒挂：违反的两件**各出一条**，且都不可自动修。

    只报一件的话，护士长会去改没写错的那一件——被约束解除记录反过来指认起始时间。
    """
    text = _baseline_file("WARD-P-001").read_text(encoding="utf-8")
    summary = _review(text)
    codes = [d["code"] for d in summary["diagnostics"]]
    assert codes == ["QC-PSY-05", "QC-PSY-05"]
    docs = {d["doc_id"] for d in summary["diagnostics"]}
    assert docs == {"WARD-P-001-RECORD-01", "WARD-P-001-RELEASE-01"}
    for d in summary["diagnostics"]:
        assert d["fix_availability"] == "none" and d["can_fix"] is False
        assert "第 6.1 条" in d["basis"]
        assert d["char_end"] > d["char_start"], "诊断必须带可定位的区间"


def test_a_missing_signature_points_at_the_signed_line_not_at_the_whole_document():
    """签署缺失：定位落在签署页上，文案点名缺的是哪一项。"""
    text = _baseline_file("WARD-P-002").read_text(encoding="utf-8")
    summary = _review(text)
    assert len(summary["diagnostics"]) == 1
    d = summary["diagnostics"][0]
    assert d["code"] == "QC-PSY-01"
    assert d["doc_id"] == "WARD-P-002-SIGN-01"
    assert "医师签字" in d["message"]
    assert d["fix_availability"] == "none", "缺签字只能提示，不能自动补一个签名"


def test_a_missing_item_becomes_exactly_one_blocking_edge_not_a_diagnostic():
    """缺件走依赖图通道，**不产规则诊断**。

    这里刻意断言零诊断：缺的那件不是「填错了」，判它得看依赖边。
    把它塞进规则表只会多一条永远判不中的码，还会让人以为病历本身合规。
    """
    text = _baseline_file("WARD-P-003").read_text(encoding="utf-8")
    summary = _review(text)
    assert summary["diagnostics"] == [], "缺件不该由规则层出诊断"
    graph = _graph(text, "WARD-P-003-RECORD-01")
    assert len(graph["blocking_edges"]) == 1
    edge = graph["blocking_edges"][0]
    assert edge["spec_to"] == "restraint_consent" and edge["requires"] is True
    assert "missing::restraint_consent" in edge["to"]
    assert "约束知情同意" in edge["describe"]
    assert graph["is_blocked"] is True


def test_binding_ambiguity_yields_no_verdict_and_lands_in_the_manual_queue():
    """绑定歧义：一条结论都不下，整对进人工队列。

    这是本项目最容易写错的一类——两份同名约束记录「取第一件」看上去无害，
    但那会让整份病历的时序结论建立在一个未经核对的来源上。
    """
    text = _baseline_file("WARD-P-004").read_text(encoding="utf-8")
    summary = _review(text)
    assert summary["diagnostics"] == [], "绑定不唯一时不得下结论"
    assert summary["skipped_checks"] == 3, "三条跨文书规则应当整条跳过并留记录"
    for node in ("WARD-P-004-RECORD-01", "WARD-P-004-RECORD-02"):
        graph = _graph(text, node)
        assert graph["manual_queue"] == 1, f"{node} 未进人工队列"
        assert graph["blocking_edges"] == [], "绑定不唯一的件不产生阻塞边"
    items = json.loads(text)["entry"]
    systems = {
        e["resource"]["identifier"][0]["system"]
        for e in items
        if e["resource"]["type"]["coding"][0]["code"] == "restraint_record"
    }
    assert len(systems) == 2, "两份约束记录必须来自不同来源系统，否则构不成歧义"


def test_the_compliant_package_reports_nothing_on_any_channel():
    """误报守卫：同场景全合规的那一次，**任何**通道都不得报出东西。

    只断言「零诊断」是不够的——缺件与歧义根本不产诊断，
    真正要守住的是「没有阻塞边、没有人工队列、没有被跳过的检查」。
    """
    text = _baseline_file("WARD-P-005").read_text(encoding="utf-8")
    summary = _review(text)
    assert summary["diagnostics"] == []
    assert summary["blocking_count"] == 0
    assert summary["skipped_checks"] == 0
    assert summary["unresolved_count"] == 0
    assert summary["parse_issues"] == []
    assert summary["ambiguous_fields"] == 0
    for doc_id in ("WARD-P-005-RECORD-01", "WARD-P-005-RELEASE-01", "WARD-P-005-SIGN-01"):
        graph = _graph(text, doc_id)
        assert graph["blocking_edges"] == []
        assert graph["manual_queue"] == 0
        assert graph["cycles"] == 0


def test_the_baseline_pack_covers_all_seven_document_types():
    """七类文书型别漏。少一类就意味着那条模板的字段约束没被夹具覆盖到。"""
    seen: set[str] = set()
    for spec in fixture_kit.BASELINE_PACK:
        bundle = fixture_kit.build_bundle(spec)
        seen.update(bundle["_fixture_meta"]["doc_types"])
    assert seen == set(fixture_kit.DISPLAY), f"缺 {set(fixture_kit.DISPLAY) - seen}"


# ——— 生成器与交付规则集不漂移 ———


def test_the_generator_field_sets_match_the_shipped_ruleset():
    """生成器里的字段集必须与 `rulesets/hospital.yaml` 的 templates 段一致。

    两边各存一份是刻意的：夹具要是从规则集读出来的，改规则集就会悄悄改掉
    夹具，夹具也就失去了「外部标尺」的意义。这条测试是让漂移**当场暴露**的唯一手段。
    """
    templates = _ruleset_templates()
    assert set(templates) == set(fixture_kit.BODY), (
        f"文书类型对不上：规则集 {sorted(templates)}，生成器 {sorted(fixture_kit.BODY)}"
    )
    for template_id, fields in templates.items():
        declared = {f["key"] for f in fields}
        generated = {k for pairs in fixture_kit.BODY[template_id].values() for k, _ in pairs}
        missing = declared - generated
        # 选填字段可以不造（造了也不判），必填字段缺一个这份文书包就白造了
        required = {f["key"] for f in fields if f.get("required")}
        assert not (missing & required), (
            f"{template_id} 的必填字段 {sorted(missing & required)} 在生成器里没有"
        )
        assert not (generated - declared), (
            f"{template_id} 生成了规则集未声明的字段 {sorted(generated - declared)}"
        )


def _ruleset_templates() -> dict[str, list[dict]]:
    """从交付规则集里抠出 templates 段（不引 pyyaml，够用即可）。"""
    import re as _re

    text = RULESET
    start = text.index("\ntemplates:")
    end = text.index("\nhospital:")
    block = text[start:end]
    out: dict[str, list[dict]] = {}
    current: str | None = None
    for line in block.splitlines():
        m = _re.match(r"^  ([a-z_]+):\s*$", line)
        if m:
            current = m.group(1)
            out[current] = []
            continue
        m = _re.match(r"^\s+- key:\s*(\S+)\s*$", line)
        if m and current:
            out[current].append({"key": m.group(1), "required": False})
            continue
        m = _re.match(r"^\s+required:\s*(\S+)\s*$", line)
        if m and current and out[current]:
            out[current][-1]["required"] = m.group(1) == "true"
    return out


def test_unknown_defect_names_are_rejected_rather_than_silently_ignored():
    """写错违规名要报错退出，不能当成「没注入」照常生成一份全合规包。

    后者是最坏的一种失败：包看起来正常、审查跑起来也正常，只是要测的东西根本没进来。
    """
    rc = fixture_kit.main([
        "--out", str(ROOT / "fixtures" / "_tmp_rejected"),
        "--alias", "WARD-T-003",
        "--defects", "typo_defect",
    ])
    assert rc == 2, "未知违规名必须返回非 0"
    assert not (ROOT / "fixtures" / "_tmp_rejected").exists(), "拒绝时不得留下任何产物"
