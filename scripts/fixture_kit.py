#!/usr/bin/env python3
"""夹具生成器：按住院别名批量产出**成套**的脱敏文书包（Task 39）。

为什么要有生成器而不只是一堆手写 JSON：

1. **跨文书规则只有在成套包里才测得出来**。QC-PSY-05~07 判的是「同一次住院的
   几件之间对不对得上」；散落的单件夹具根本构不成时序对照。
2. **每造一条违规夹具就得顺手造一份同场景的合规夹具**，否则没有误报守卫。
   生成器把「成对」变成参数的一次取值，而不是靠人记得。
3. **产物必须可复现**。质控办复核一份病历时问的是「你这批数据是怎么来的」，
   答不上来的批次等于不可信。因此本模块**不取当前时间、不用随机数**：
   同一份参数两次生成逐字节相同，且每份夹具自带 `_fixture_meta` 写明参数。

**脱敏纪律**：住院别名形如 `WARD-P-001`，执行者一律 `<EXAMPLE_NURSE>` /
`<EXAMPLE_PRACTITIONER>` / `<EXAMPLE_SIGNATURE>`。生成物里不出现姓名、住院号、
身份证件号——`tests/test_fixtures.py` 与 Rust 侧
`all_fixtures_are_free_of_patient_identifiers` 各扫一遍，双保险。

用法（项目子目录下）：

    python3 scripts/fixture_kit.py --out fixtures/generated
    python3 scripts/fixture_kit.py --out /tmp/x --alias WARD-P-001 --defects time_inversion

**这不是最终对接面**：真实对接按 FHIR R4 规范实现，凭据从环境变量读。
本模块只产出测试数据层。
"""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
from dataclasses import dataclass, field, replace
from typing import Any, Iterable, Sequence

# —— 与 rulesets/hospital.yaml 的 templates 段一一对应 ——
#
# 这里是**抄一份**而不是运行时读 YAML：夹具的字段集必须与交付规则集独立，
# 否则改规则集就会悄悄改掉夹具，夹具失去「外部标尺」的意义。
# `tests/test_fixtures.py` 里有断言守着两边不漂移。

ENC_HIS = "https://his.example-hospital.invalid/fhir/identifier/encounter"
ENC_NURSING = "https://nursing.example-hospital.invalid/fhir/identifier/encounter"
DOC_TYPE_SYS = "https://example-hospital.invalid/fhir/CodeSystem/doc-type"

GENERATOR_NAME = "psychdoc-fixture-kit"
GENERATOR_VERSION = "1.0.0"

#: 文书类型 → doc_id 后缀。改这里会改所有生成物的 id，读之前先想清楚。
SUFFIX = {
    "inpatient_record": "INPATIENT",
    "nursing_record": "NURSING",
    "restraint_consent": "CONSENT",
    "restraint_record": "RECORD",
    "restraint_release": "RELEASE",
    "assessment_report": "ASSESS",
    "sign_page": "SIGN",
}

#: 文书类型中文名（与规则集 doc_types 同源，供清单与日志用）
DISPLAY = {
    "inpatient_record": "住院记录",
    "nursing_record": "护理记录",
    "restraint_consent": "约束知情同意",
    "restraint_record": "约束记录",
    "restraint_release": "约束解除记录",
    "assessment_report": "评估报告",
    "sign_page": "签署页",
}

#: 七种文书类型的最小字段集：段落标题 → 该段必填的 (键, 值)。
#: 值里已写死「合规形态」，注入违规时由 `_defect_*` 覆写其中若干项。
BODY: dict[str, dict[str, list[tuple[str, str]]]] = {
    "inpatient_record": {
        "入院记录": [
            ("入院时间", "2026-03-04 09:00"),
            ("诊断", "精神分裂症缓解期伴冲动风险"),
        ]
    },
    "nursing_record": {
        "护理记录": [
            ("巡视时间", "2026-03-04 10:30"),
            ("病情描述", "情绪较入院时平稳，夜间未再激越。"),
        ]
    },
    "restraint_consent": {
        "约束事由": [("约束事由", "日间反复冲动，需短时保护。")],
        "患方知情同意": [
            ("患方意见", "已知情同意，同意按护理常规采取保护措施。"),
            ("约束起始时间", "2026-03-04 10:00"),
            ("患方签字", "<EXAMPLE_SIGNATURE>"),
        ],
    },
    "restraint_record": {
        "约束经过": [
            ("约束起始时间", "2026-03-04 10:00"),
            ("约束等级", "二级保护"),
            ("执行护士", "<EXAMPLE_NURSE>"),
            ("观察记录", "约束期间生命体征平稳。"),
            ("约束时长", "120 分钟"),
        ]
    },
    "restraint_release": {
        "解除经过": [
            ("解除时间", "2026-03-04 11:30"),
            ("解除依据", "症状缓解，经护士长查房同意解除。"),
            ("执行护士", "<EXAMPLE_NURSE>"),
        ]
    },
    "assessment_report": {
        "入院评估": [
            ("评估时间", "2026-03-04 09:30"),
            ("风险等级", "中高风险"),
            ("评估医师", "<EXAMPLE_PRACTITIONER>"),
        ]
    },
    "sign_page": {
        "签署页": [
            ("护士签字", "<EXAMPLE_SIGNATURE>"),
            ("医师签字", "<EXAMPLE_SIGNATURE>"),
        ]
    },
}

#: 各文书类型上的 authoredOn（FHIR 签署时刻）——合规形态必须与业务时刻同序。
AUTHORED_ON = {
    "inpatient_record": "2026-03-04T09:20:00+08:00",
    "nursing_record": "2026-03-04T10:50:00+08:00",
    "restraint_consent": "2026-03-04T09:40:00+08:00",  # 必须在约束开始 10:00 之前
    "restraint_record": "2026-03-04T10:20:00+08:00",
    "restraint_release": "2026-03-04T11:40:00+08:00",
    "assessment_report": "2026-03-04T09:50:00+08:00",  # 必须在首次约束 10:00 之前
    "sign_page": "2026-03-04T11:45:00+08:00",
}

#: Composition.date：文书本身的业务日期（比 authoredOn 略早，接近真实归档习惯）
DOC_DATE = {
    "inpatient_record": "2026-03-04T09:00:00+08:00",
    "nursing_record": "2026-03-04T10:30:00+08:00",
    "restraint_consent": "2026-03-04T09:35:00+08:00",
    "restraint_record": "2026-03-04T10:00:00+08:00",
    "restraint_release": "2026-03-04T11:30:00+08:00",
    "assessment_report": "2026-03-04T09:30:00+08:00",
    "sign_page": "2026-03-04T11:40:00+08:00",
}

#: 可注入的四类违规。名字就是测试里断言用的键，改名会连带改测试。
DEFECT_TIME_INVERSION = "time_inversion"      # 时间序倒挂（QC-PSY-05）
DEFECT_SIGNATURE_MISSING = "signature_missing"  # 签署缺失（QC-PSY-01）
DEFECT_MISSING_ITEM = "missing_item"          # 成套件缺一件（依赖图阻塞边）
DEFECT_BINDING_AMBIGUITY = "binding_ambiguity"  # 跨系统同名导出，绑定不唯一

ALL_DEFECTS = (
    DEFECT_TIME_INVERSION,
    DEFECT_SIGNATURE_MISSING,
    DEFECT_MISSING_ITEM,
    DEFECT_BINDING_AMBIGUITY,
)

#: 每类违规「在核心眼里」怎么被看见。测试按这张表逐类断言，
#: 而不是笼统地断言「有诊断」——缺件与绑定歧义**本就不产规则诊断**，
#: 它们走的是依赖图与人工队列通道。
DEFECT_EXPECTATION = {
    DEFECT_TIME_INVERSION: {
        "channel": "diagnostics",
        "zh": "约束解除记录填的解除时间早于约束记录的起始时间",
    },
    DEFECT_SIGNATURE_MISSING: {
        "channel": "diagnostics",
        "zh": "签署页缺医师签字，必填字段缺失",
    },
    DEFECT_MISSING_ITEM: {
        "channel": "graph",
        "zh": "约束记录的前置件「约束知情同意」不在包里，产生阻塞边",
    },
    DEFECT_BINDING_AMBIGUITY: {
        "channel": "skipped",
        "zh": "HIS 与护理系统各一份「约束记录」，绑定不唯一，跨文书规则整条跳过",
    },
}


@dataclass(frozen=True)
class EncounterSpec:
    """一次住院要生成哪些件、注入哪几类违规。全部字段参与可复现性。"""

    alias: str
    """住院别名，形如 `WARD-P-001`。"""

    defect: tuple[str, ...] = ()
    """要注入的违规，取值见 `ALL_DEFECTS`。空元组 = 全合规（误报守卫）。"""

    note: str = ""
    """一句话说明这包拿来干什么；原样写进 `_fixture_meta`，供 README 与截图注明来源。"""

    def ward(self) -> str:
        """病区码 = 别名的第一段（`WARD-P-001` → `WARD-P`）。"""
        return self.alias.split("-", 1)[0]


def _section_text(pairs: Sequence[tuple[str, str]]) -> str:
    """把 (键, 值) 列表渲染成一段正文。

    刻意保留中文句末的「。」：真实文书就是这个样子，解析层会剥掉文末句读。
    为了好解析而把句号去掉，等于把夹具调成比现实好看，跨文书规则就测不到真实口径。
    """
    return "\n".join(f"{k}：{v}。" for k, v in pairs)


def _with(pairs: Sequence[tuple[str, str]], overrides: dict[str, str | None]) -> list[tuple[str, str]]:
    """按 `overrides` 改写 (键, 值) 列表；值为 None 表示**整行删掉**（模拟未填写）。"""
    out: list[tuple[str, str]] = []
    for key, value in pairs:
        if key not in overrides:
            out.append((key, value))
            continue
        new = overrides[key]
        if new is None:
            continue
        out.append((key, new))
    return out


def _composition(
    doc_id: str,
    alias: str,
    template_id: str,
    sections: list[dict[str, str]],
    *,
    authored_on: str | None,
    date: str | None,
    encounter_system: str = ENC_HIS,
) -> dict[str, Any]:
    """拼一份 Composition（FHIR R4 最小集）。"""
    comp: dict[str, Any] = {
        "resourceType": "Composition",
        "id": doc_id,
        "status": "preliminary",
        "title": f"<脱敏样例文书·{DISPLAY[template_id]}>",
        "identifier": [{"system": encounter_system, "value": alias}],
        "type": {"coding": [{"system": DOC_TYPE_SYS, "code": template_id}]},
        "author": [{"reference": "Practitioner/<EXAMPLE_PRACTITIONER>"}],
    }
    if date is not None:
        comp["date"] = date
    if authored_on is not None:
        comp["authoredOn"] = authored_on
    comp["section"] = sections
    return comp


def build_items(spec: EncounterSpec) -> list[dict[str, Any]]:
    """按 spec 生成一次住院的全部 Composition。"""
    has = set(spec.defect)
    body = {t: {s: list(p) for s, p in secs.items()} for t, secs in BODY.items()}
    authored = dict(AUTHORED_ON)
    doc_date = dict(DOC_DATE)

    # —— 违规 1：时间序倒挂。解除记录写 09:00，早于约束起始 10:00。
    # 刻意改的是**正文里的业务时刻**与 authoredOn 两处：真实倒挂的病历就是
    # 两处一起错的，只改一处会变成「同一件事两个时间」而不是「先后反了」。
    if DEFECT_TIME_INVERSION in has:
        body["restraint_release"]["解除经过"] = _with(
            body["restraint_release"]["解除经过"], {"解除时间": "2026-03-04 09:00"}
        )
        authored["restraint_release"] = "2026-03-04T09:10:00+08:00"
        doc_date["restraint_release"] = "2026-03-04T09:00:00+08:00"

    # —— 违规 2：签署缺失。签署页少医师签字这一行。
    if DEFECT_SIGNATURE_MISSING in has:
        body["sign_page"]["签署页"] = _with(
            body["sign_page"]["签署页"], {"医师签字": None}
        )

    # —— 违规 3：缺件。整份约束知情同意不生成（成对地补进下面的生成物清单）。
    if DEFECT_MISSING_ITEM in has:
        body.pop("restraint_consent", None)

    # —— 违规 4：绑定歧义。同一份约束记录由 HIS 与护理系统各导出一遍。
    # 注意这不是「重了两遍」而是**两个来源系统各认各的**；绑定层必须拒绝取第一件。
    duplicated = DEFECT_BINDING_AMBIGUITY in has

    order = [
        "inpatient_record",
        "assessment_report",
        "restraint_consent",
        "restraint_record",
        "restraint_release",
        "sign_page",
        "nursing_record",
    ]
    items: list[dict[str, Any]] = []
    for template_id in order:
        if template_id not in body:
            continue
        sections = [
            {"title": title, "text": _section_text(pairs)}
            for title, pairs in body[template_id].items()
        ]
        base_id = f"{spec.alias}-{SUFFIX[template_id]}-01"
        items.append(
            _composition(
                base_id,
                spec.alias,
                template_id,
                sections,
                authored_on=authored[template_id],
                date=doc_date[template_id],
            )
        )
        if duplicated and template_id == "restraint_record":
            twin_id = f"{spec.alias}-{SUFFIX[template_id]}-02"
            items.append(
                _composition(
                    twin_id,
                    spec.alias,
                    template_id,
                    sections,
                    authored_on="2026-03-04T10:35:00+08:00",
                    date=doc_date[template_id],
                    encounter_system=ENC_NURSING,
                )
            )
    return items


def build_bundle(spec: EncounterSpec) -> dict[str, Any]:
    """把一次住院的若干件包成一份 FHIR R4 Bundle。"""
    items = build_items(spec)
    defects = sorted(set(spec.defect))
    expectation = {d: DEFECT_EXPECTATION[d]["zh"] for d in defects}
    return {
        "_comment": (
            f"脱敏合成夹具（{GENERATOR_NAME} {GENERATOR_VERSION} 生成）："
            f"{spec.alias}，{len(items)} 件，注入违规 {defects or '无'}。{spec.note}"
        ),
        # 生成参数原样留档：这份数据怎么来的，审计时必须答得上来。
        "_fixture_meta": {
            "generator": GENERATOR_NAME,
            "generator_version": GENERATOR_VERSION,
            "encounter_alias": spec.alias,
            "ward": spec.ward(),
            "defects": defects,
            "note": spec.note,
            "doc_types": sorted({i["type"]["coding"][0]["code"] for i in items}),
            "expectation_zh": expectation,
        },
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [
            {"fullUrl": f"urn:uuid:{i['id']}", "resource": i} for i in items
        ],
    }


def serialize(bundle: dict[str, Any]) -> bytes:
    """确定性序列化：同参数两次生成必须逐字节相同。"""
    return (json.dumps(bundle, ensure_ascii=False, indent=2) + "\n").encode("utf-8")


#: 基准文书包：五次住院，每类违规各一次，外加一次全合规作误报守卫。
#: 分住院而不是全塞一包里——绑定歧义会让同类型的跨文书配对整对跳过，
#: 和时间序倒挂放在同一次住院里会互相盖住，两条都测不出来。
BASELINE_PACK = (
    EncounterSpec(
        alias="WARD-P-001",
        defect=(DEFECT_TIME_INVERSION,),
        note="时间序倒挂：解除记录 09:00 早于约束起始 10:00（QC-PSY-05）。",
    ),
    EncounterSpec(
        alias="WARD-P-002",
        defect=(DEFECT_SIGNATURE_MISSING,),
        note="签署缺失：签署页少医师签字一行（QC-PSY-01）。",
    ),
    EncounterSpec(
        alias="WARD-P-003",
        defect=(DEFECT_MISSING_ITEM,),
        note="成套缺件：有约束记录无约束知情同意，依赖图出一条阻塞边。",
    ),
    EncounterSpec(
        alias="WARD-P-004",
        defect=(DEFECT_BINDING_AMBIGUITY,),
        note="绑定歧义：HIS 与护理系统各一份约束记录，绑定不唯一交人工。",
    ),
    EncounterSpec(
        alias="WARD-P-005",
        defect=(),
        note="同场景全合规：任何规则都不得命中（误报守卫）。",
    ),
)


def build_baseline() -> list[dict[str, Any]]:
    return [build_bundle(spec) for spec in BASELINE_PACK]


def write_packs(bundles: Iterable[dict[str, Any]], out_dir: pathlib.Path) -> list[pathlib.Path]:
    out_dir.mkdir(parents=True, exist_ok=True)
    written: list[pathlib.Path] = []
    for bundle in bundles:
        alias = bundle["_fixture_meta"]["encounter_alias"]
        defects = bundle["_fixture_meta"]["defects"]
        name = f"{alias.lower()}"
        if defects:
            name += "-" + "-".join(defects)
        path = out_dir / f"{name}.json"
        path.write_bytes(serialize(bundle))
        written.append(path)
    return written


def main(argv: Sequence[str] | None = None) -> int:
    ap = argparse.ArgumentParser(description="生成脱敏成套文书包夹具")
    ap.add_argument(
        "--out",
        default="fixtures/generated",
        help="输出目录（默认 fixtures/generated）",
    )
    ap.add_argument(
        "--alias",
        help="只生成单次住院；与 --defects 搭配使用。不给则生成基准包",
    )
    ap.add_argument(
        "--defects",
        default="",
        help=f"逗号分隔，可选：{', '.join(ALL_DEFECTS)}。空串 = 全合规",
    )
    ap.add_argument("--note", default="", help="写进 _fixture_meta 的一句话说明")
    args = ap.parse_args(argv)

    out = pathlib.Path(args.out)
    if args.alias:
        defects = tuple(d.strip() for d in args.defects.split(",") if d.strip())
        unknown = [d for d in defects if d not in ALL_DEFECTS]
        if unknown:
            print(f"未知违规名：{', '.join(unknown)}；可选 {', '.join(ALL_DEFECTS)}",
                  file=sys.stderr)
            return 2
        spec = EncounterSpec(alias=args.alias, defect=defects, note=args.note)
        written = write_packs([build_bundle(spec)], out)
    else:
        written = write_packs(build_baseline(), out)

    for path in written:
        print(f"  {path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
