#!/usr/bin/env python3
"""生成 Task 22 的脏数据夹具（GBK / 混编 / 非法码点三类必须是**字节**，
不能手写成 UTF-8 文本再改名——那样测的就不是真实场景了）。

用法（在项目子目录下）：
    python3 scripts/make_dirty_fixtures.py

产物写入 fixtures/dirty/。夹具里的标识一律脱敏别名，不含患者姓名/住院号。
"""
import json
import os
import pathlib

HERE = pathlib.Path(__file__).resolve().parent.parent
OUT = HERE / "fixtures" / "dirty"

ENC = "https://his.example-hospital.invalid/fhir/identifier/encounter"
DOC_TYPE = "https://example-hospital.invalid/fhir/CodeSystem/doc-type"


def comp(cid, alias, code, sections, date="2026-03-02T08:30:00+08:00",
         authored="2026-03-02T08:55:00+08:00", encounter_system=ENC):
    r = {
        "resourceType": "Composition",
        "id": cid,
        "status": "preliminary",
        "title": "<脱敏样例文书>",
        "identifier": [{"system": encounter_system, "value": alias}],
        "type": {"coding": [{"system": DOC_TYPE, "code": code}]},
        "author": [{"reference": "Practitioner/<EXAMPLE_PRACTITIONER>"}],
    }
    if date is not None:
        r["date"] = date
    if authored is not None:
        r["authoredOn"] = authored
    r["section"] = sections
    return r


def sec(title, text):
    return {"title": title, "text": text}


def bundle(entries, comment):
    return {
        "_comment": comment,
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [{"fullUrl": f"urn:uuid:{e['id']}", "resource": e} for e in entries],
    }


def write(name, obj, encoding="utf-8", bom=False, raw=None):
    OUT.mkdir(parents=True, exist_ok=True)
    path = OUT / name
    if raw is not None:
        data = raw
    else:
        data = json.dumps(obj, ensure_ascii=False, indent=2).encode(encoding)
    if bom and raw is None:
        data = b"\xef\xbb\xbf" + data
    path.write_bytes(data)
    print(f"  {path.relative_to(HERE)}  ({len(data)} bytes, {encoding}"
          f"{', BOM' if bom else ''})")


def main():
    print("生成 Task 22 脏数据夹具：")

    # 1) GBK 导出：中文 Windows 区域设置下导出工具的默认编码
    write("ward-h-001-gbk-export.json", bundle(
        [comp("WARD-H-001-CONSENT-01", "WARD-H-001", "restraint_consent", [
            sec("约束事由", "约束事由：日间激越，需短时保护。"),
            sec("患方知情同意", "患方意见：已知情同意，同意按护理常规采取保护措施。\n"
                "患方签字：\n约束起始时间：2026-03-02 23:40。"),
        ])],
        "脱敏合成夹具：整份以 GBK 编码导出。read_to_string 在这份包上会直接报错，"
        "解析器必须先嗅探编码；内容应与 UTF-8 版逐字相同。",
    ), encoding="gbk")

    # 2) UTF-8 + BOM
    write("ward-h-002-utf8-bom.json", bundle(
        [comp("WARD-H-002-RECORD-01", "WARD-H-002", "restraint_record", [
            sec("约束经过", "约束起始时间：2026-03-02 23:40。\n约束等级：二级保护。\n"
                "执行护士：<EXAMPLE_NURSE>。"),
        ])],
        "脱敏合成夹具：带 UTF-8 BOM。标记必须被剥掉，正文逐字保留。",
    ), bom=True)

    # 3) 同一文件内 UTF-8 与 GBK 混编
    utf8_part = comp("WARD-H-003-RECORD-01", "WARD-H-003", "restraint_record", [
        sec("约束经过", "约束起始时间：2026-03-02 23:40。\n约束等级：二级保护。"),
    ])
    gbk_part = comp("WARD-H-003-RELEASE-01", "WARD-H-003", "restraint_release", [
        sec("解除经过", "解除时间：2026-03-03 06:10。\n解除原因：病情稳定。\n"
            "执行护士：<EXAMPLE_NURSE>。"),
    ], date="2026-03-03T06:20:00+08:00")
    # 头部与第一条 entry 用 UTF-8 写，第二条 entry 整段用 GBK 写。
    # 中间不加任何 ASCII 分隔的正文（`,` 是 ASCII，会成为段边界）——
    # 真实场景是 UTF-8 的最后一行正文直接接上 GBK 的第一行正文。
    prefix = ('{"_comment":"脱敏合成夹具：同一文件内 UTF-8 段与 GBK 段混编'
              '（院内模板段由 UTF-8 组件写入、自由叙述段由 GBK 组件补录）。'
              '整体按任一单编码解都会坏掉一半，解析器必须逐段判定。",'
              '"resourceType":"Bundle","type":"collection","entry":[')
    mixed = bytearray()
    mixed += prefix.encode("utf-8")
    mixed += json.dumps({"fullUrl": "urn:uuid:WARD-H-003-RECORD-01",
                         "resource": utf8_part}, ensure_ascii=False).encode("utf-8")
    mixed += b","
    # 这一条 entry 刻意整段用 GBK 编码写出
    mixed += json.dumps({"fullUrl": "urn:uuid:WARD-H-003-RELEASE-01",
                         "resource": gbk_part}, ensure_ascii=False).encode("gbk")
    mixed += b"]}"
    write("ward-h-003-mixed-encoding.json", None, raw=bytes(mixed))

    # 4) 非法码点：UTF-8 与 GBK 都解不出来的字节
    dirty = bytearray()
    dirty += json.dumps(bundle(
        [comp("WARD-H-004-CONSENT-01", "WARD-H-004", "restraint_consent", [
            sec("约束事由", "约束事由：日间激越。\n观察记录：约束期间生命体征平稳。"),
        ])],
        "脱敏合成夹具：段内混入 UTF-8 与 GBK 都解不出的非法字节。"
        "解析必须继续（不整包失败），坏字节替换为占位符并逐处记录 E_ENCODING。",
    ), ensure_ascii=False).encode("utf-8")
    # 在第一处 text 的「日间激越。」后面插 3 个非法字节
    marker = "日间激越。".encode("utf-8")
    at = dirty.find(marker) + len(marker)
    dirty[at:at] = b"\xff\xfe\x81"
    write("ward-h-004-illegal-bytes.json", None, raw=bytes(dirty))

    # 5) 缺 section / null text / 缺 type.coding / 坏日期 —— 一包多坏
    no_section = comp("WARD-H-005-CONSENT-01", "WARD-H-005", "restraint_consent", [])
    del no_section["section"]  # section 键整个不存在（不是空数组）
    empty_section = comp("WARD-H-005-SIGNPAGE-01", "WARD-H-005", "sign_page", [])
    write("ward-h-005-missing-section.json", bundle(
        [
            no_section,
            # section[].text 为 null
            comp("WARD-H-005-RECORD-01", "WARD-H-005", "restraint_record", [
                sec("约束经过", "约束起始时间：2026-03-02 23:40。"),
                sec("执行护士", None),
            ]),
            # section 存在但是空数组
            empty_section,
            # type.coding 缺失
            {**comp("WARD-H-005-SIGN-01", "WARD-H-005", "restraint_consent", []),
             "type": {}},
            # 日期写法认不出
            comp("WARD-H-005-RELEASE-01", "WARD-H-005", "restraint_release", [
                sec("解除经过", "解除时间：2026-03-03 06:10。"),
            ], date="2026/03/03 06:20"),
            # 好的那一件：必须照常进结果
            comp("WARD-H-005-ASSESS-01", "WARD-H-005", "assessment_report", [
                sec("评估结论", "风险等级：二级。\n评估时间：2026-03-02 09:00。"),
            ]),
        ],
        "脱敏合成夹具：section 键整个缺失 / section 为空数组 / null 段文本 / "
        "缺 type.coding / 坏日期五种结构问题混在一包里。"
        "只有后两种让件整件不可用，前三种都只是段级问题（件仍算成功），"
        "最后一件好的必须照常解析成功——部分成功不是整包丢。",
    ))

    # 6) 重复 doc_id
    write("ward-h-006-duplicate-id.json", bundle(
        [
            comp("WARD-H-006-RECORD-01", "WARD-H-006", "restraint_record", [
                sec("约束经过", "约束起始时间：2026-03-02 23:40。\n约束等级：二级保护。"),
            ]),
            # 同 id 再来一份，正文不同 —— 真实原因可能是 HIS 与护理系统
            # 都写了同一个 id，也可能是导出工具重试，两种都要人确认
            comp("WARD-H-006-RECORD-01", "WARD-H-006", "restraint_record", [
                sec("约束经过", "约束起始时间：2026-03-02 23:55。\n约束等级：三级保护。"),
            ], authored="2026-03-02T09:10:00+08:00"),
            # 唯一 id 的件不受牵连
            comp("WARD-H-006-RELEASE-01", "WARD-H-006", "restraint_release", [
                sec("解除经过", "解除时间：2026-03-03 06:10。"),
            ], date="2026-03-03T06:20:00+08:00"),
        ],
        "脱敏合成夹具：两条 Composition 用了同一个 id，正文不同。"
        "两条都必须进 unresolved 交人工——既不合并（合并会拼出没人敢归档的文书），"
        "也不取第一条（程序无权替人决定以哪份为准）。",
    ))

    # 7) 超长段：单段 > 10 万字，按段落边界切分
    para = "观察记录：约束期间生命体征平稳，患者无不适主诉，情绪较前稳定。"
    body = "\n".join([f"{i:04d} {para}" for i in range(1, 3001)])
    assert len(body) > 100_000, f"需要 >10 万字，实际 {len(body)}"
    write("ward-h-007-oversized-section.json", bundle(
        [comp("WARD-H-007-NURSING-01", "WARD-H-007", "nursing_record", [
            sec("护理记录", body),
            sec("护理小结", "护理小结：约束解除后睡眠可，无跌倒风险。\n巡视时间：2026-03-03 07:00。"),
        ])],
        "脱敏合成夹具：单段 13 万余字（病程记录里粘了一整篇外文摘要那种量级）。"
        "必须按段落边界切分成多块、内容零丢失、块内 char_range 口径不变。",
    ))


if __name__ == "__main__":
    os.chdir(HERE)
    main()
