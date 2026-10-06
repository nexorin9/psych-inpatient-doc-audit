"""PyO3 绑定的 Python 侧契约测试。

`core/crates/psychdoc-ffi/tests/ffi_entrypoints.rs` 已经用 Rust 断言了
5 个入口的**业务行为**。本文件补上 Rust 测不到的那一层：

- 返回值确实落成 Python 的 `dict` / `list` / `int` / `str`（不是 `PyO3` 私有类型）；
- 错误**带 `.code` 属性**透传，而不是靠匹配中文文案；
- 缺凭据时 `commit_writeback` 抛 `E_CFG_MISSING` 且**零次网络调用**。

绑定缺席时整组跳过，并在跳过原因里写清怎么构建 —— 让「没跑」和
「跑过了」在 CI 输出里可区分。
"""

from __future__ import annotations

import json
from pathlib import Path

import pytest

ROOT = Path(__file__).resolve().parent.parent

core_bridge = pytest.importorskip(
    "desktop.core_bridge",
    reason=(
        "核心绑定未构建。构建方式："
        "cargo build -p psychdoc-ffi --features extension-module，"
        "再把 core/target/debug/libpsychdoc_ffi.{dylib,so} 拷到 bindings/ 下"
    ),
)


@pytest.fixture(scope="module")
def ruleset_yaml() -> str:
    return core_bridge.default_ruleset_path().read_text(encoding="utf-8")


@pytest.fixture(scope="module")
def fixtures_dir() -> Path:
    return ROOT / "fixtures"


def _read(fixtures_dir: Path, name: str) -> str:
    return (fixtures_dir / name).read_text(encoding="utf-8")


# ---------------------------------------------------------------------------
# 模块契约
# ---------------------------------------------------------------------------


def test_the_five_entry_points_are_exported():
    import psychdoc_ffi

    for name in core_bridge.ENTRY_POINTS:
        assert hasattr(psychdoc_ffi, name), f"绑定缺少入口 {name}"
    assert hasattr(psychdoc_ffi, "PsychdocError")
    assert psychdoc_ffi.CORE_VERSION


def test_core_version_is_a_non_empty_string():
    assert isinstance(core_bridge.core_version(), str)
    assert core_bridge.core_version()


# ---------------------------------------------------------------------------
# 入口 1：check_bundle —— 返回值是真正的 Python 对象
# ---------------------------------------------------------------------------


def test_check_bundle_returns_plain_python_types(fixtures_dir, ruleset_yaml):
    out = core_bridge.check_bundle(_read(fixtures_dir, "ward-01-minimal.json"), ruleset_yaml)

    assert out["ok"] is True
    assert isinstance(out, dict)
    assert isinstance(out["summary"], dict)
    diagnostics = out["summary"]["diagnostics"]
    assert isinstance(diagnostics, list)
    assert len(diagnostics) == 1

    d = diagnostics[0]
    assert isinstance(d, dict)
    # 三件套：码 / 位置 / 为什么；外加院内规范依据与可否自动修
    assert d["code"] == "QC-PSY-01"
    assert isinstance(d["doc_id"], str)
    assert isinstance(d["section_index"], int)
    assert isinstance(d["char_start"], int) and isinstance(d["char_end"], int)
    assert isinstance(d["message"], str) and d["message"]
    assert isinstance(d["basis"], str) and d["basis"]
    assert d["fix_availability"] == "none"
    assert d["can_fix"] is False
    assert d["severity"] == "blocking"


def test_check_bundle_encounter_rows_are_the_left_pane_contract(fixtures_dir, ruleset_yaml):
    out = core_bridge.check_bundle(_read(fixtures_dir, "ward-11-cross-doc-violations.json"), ruleset_yaml)
    rows = out["encounters"]
    assert rows, "左栏必须有住院条目"
    row = rows[0]
    assert row["encounter_alias"].startswith("WARD-")
    assert isinstance(row["doc_count"], int) and row["doc_count"] >= 1
    assert isinstance(row["blocking_count"], int)
    assert isinstance(row["doc_ids"], list)


def test_check_bundle_on_a_compliant_package_is_quiet_but_not_empty(fixtures_dir, ruleset_yaml):
    out = core_bridge.check_bundle(_read(fixtures_dir, "ward-09-field-compliant.json"), ruleset_yaml)
    assert out["summary"]["diagnostics"] == []
    assert out["summary"]["blocking_count"] == 0
    assert out["summary"]["items_reviewed"] >= 1


def test_check_bundle_reports_the_ruleset_short_hash_for_the_status_bar(fixtures_dir, ruleset_yaml):
    out = core_bridge.check_bundle(_read(fixtures_dir, "ward-01-minimal.json"), ruleset_yaml)
    short = out["summary"]["ruleset_short_sha256"]
    assert isinstance(short, str) and len(short) == 12


# ---------------------------------------------------------------------------
# 入口 2：review_batch —— 增量 + 运行摘要
# ---------------------------------------------------------------------------


def test_review_batch_over_a_directory_gives_a_status_bar_summary(fixtures_dir, ruleset_yaml, tmp_path):
    out = core_bridge.review_batch(str(fixtures_dir), ruleset_yaml, str(tmp_path / "cache.json"))
    assert out["packages_found"] >= 1
    run = out["run_summary"]
    assert run["items_reviewed"] > 0
    assert isinstance(run["items_skipped"], int)
    assert len(run["ruleset_short_sha256"]) == 12
    assert "重跑" in run["as_zh"]


def test_review_batch_second_run_skips_everything_and_agrees(fixtures_dir, ruleset_yaml, tmp_path):
    cache = str(tmp_path / "cache.json")
    first = core_bridge.review_batch(str(fixtures_dir), ruleset_yaml, cache)
    second = core_bridge.review_batch(str(fixtures_dir), ruleset_yaml, cache)

    assert second["run_summary"]["items_reviewed"] == 0
    assert second["run_summary"]["items_skipped"] > 0
    # 增量与全量结论必须逐条相同，否则「跳过」就等于「漏审」
    assert second["summary"]["diagnostics"] == first["summary"]["diagnostics"]


# ---------------------------------------------------------------------------
# 入口 3：preview_fixback
# ---------------------------------------------------------------------------


def test_preview_fixback_separates_selectable_blocks_from_evidence_only(fixtures_dir, ruleset_yaml):
    out = core_bridge.preview_fixback(_read(fixtures_dir, "ward-08-field-violations.json"), ruleset_yaml)
    blocks = out["blocks"]
    assert len(blocks) == 1
    b = blocks[0]
    assert b["code"] == "QC-PSY-02"
    assert b["fix_availability"] == "automatic"
    assert isinstance(b["diff"], list) and b["diff"]
    assert all(line["op"] in ("same", "added", "removed") for line in b["diff"])

    # 只给依据不给按钮的码：UI 必须在按钮位置显式说明原因
    assert "QC-PSY-01" in out["evidence_only"]
    assert "QC-PSY-02" not in out["evidence_only"]
    assert "段" in out["summary_line"]


def test_preview_fixback_of_a_clean_package_is_empty(fixtures_dir, ruleset_yaml):
    out = core_bridge.preview_fixback(_read(fixtures_dir, "ward-09-field-compliant.json"), ruleset_yaml)
    assert out["blocks"] == []
    assert out["evidence_only"] == []


# ---------------------------------------------------------------------------
# 入口 5：graph_both_directions
# ---------------------------------------------------------------------------


def test_graph_answers_two_different_questions_about_one_graph(fixtures_dir, ruleset_yaml):
    bundle = _read(fixtures_dir, "ward-02-restraint-set.json")
    out = core_bridge.graph_both_directions(bundle, ruleset_yaml, "WARD-B-001-RECORD-01")

    assert out["exists"] is True
    deps = out["dependencies"]
    dependents = out["dependents"]
    assert deps and dependents
    assert deps != dependents, "正反向必须给出不同答案"
    assert sorted(set(deps) | set(dependents)) == sorted(out["both"])


def test_graph_on_an_unknown_node_reports_absence(fixtures_dir, ruleset_yaml):
    bundle = _read(fixtures_dir, "ward-02-restraint-set.json")
    out = core_bridge.graph_both_directions(bundle, ruleset_yaml, "不存在的件")
    assert out["exists"] is False
    assert out["dependencies"] == []


def test_graph_accepts_a_directory_and_agrees_with_the_single_file(fixtures_dir, ruleset_yaml):
    """归档视图面对的是整个导出目录：并图后答案必须与单份包时一致。"""
    from_dir = core_bridge.graph_both_directions(
        str(fixtures_dir), ruleset_yaml, "WARD-B-001-RECORD-01"
    )
    from_file = core_bridge.graph_both_directions(
        str(fixtures_dir / "ward-02-restraint-set.json"),
        ruleset_yaml,
        "WARD-B-001-RECORD-01",
    )
    assert from_dir["dependencies"] == from_file["dependencies"]
    assert from_dir["dependents"] == from_file["dependents"]


def test_graph_rejects_something_that_is_neither_a_path_nor_json(ruleset_yaml):
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.graph_both_directions("/绝对不存在的导出目录", ruleset_yaml, "X")
    assert ei.value.code == "E_BUNDLE_SHAPE"


# ---------------------------------------------------------------------------
# 入口 4：commit_writeback —— 缺凭据即 E_CFG_MISSING
# ---------------------------------------------------------------------------


def _writeback_kwargs():
    return {
        "store_path": None,
        "draft_id": "DRAFT-TEST",
        "encounter_alias": "WARD-A-001",
        "actor": "护士长A",
        "at": "2026-03-03T08:00:00+08:00",
        "entries": [
            (
                "WARD-A-001-CONSENT-01",
                json.dumps(
                    {
                        "resourceType": "Composition",
                        "id": "WARD-A-001-CONSENT-01",
                        "status": "preliminary",
                        "type": {"coding": [{"code": "restraint_consent"}]},
                    },
                    ensure_ascii=False,
                ),
            )
        ],
        "content_hashes": [("WARD-A-001-CONSENT-01", "hash-1")],
    }


def test_commit_writeback_without_credentials_raises_cfg_missing(monkeypatch):
    # 清空全部凭据：核心在构造 EmrConfig 时就拒，物理上不可能先发请求。
    for key in ("EMR_BASE_URL", "EMR_CLIENT_ID", "EMR_CLIENT_SECRET", "EMR_TENANT_ID"):
        monkeypatch.delenv(key, raising=False)
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.commit_writeback(**_writeback_kwargs())
    assert ei.value.code == "E_CFG_MISSING"
    assert "不会降级" in ei.value.message
    assert ei.value.retryable is False, "配置问题重试没有意义"


def test_startup_check_also_reports_cfg_missing(monkeypatch):
    for key in core_bridge.REQUIRED_ENV:
        monkeypatch.delenv(key, raising=False)
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.startup_check()
    assert ei.value.code == "E_CFG_MISSING"


def test_the_dryrun_builds_a_transaction_and_never_touches_the_network(fixtures_dir):
    out = core_bridge.writeback_dryrun(
        draft_id="DRAFT-TEST",
        encounter_alias="WARD-A-001",
        actor="护士长A",
        at="2026-03-03T08:00:00+08:00",
        entries=_writeback_kwargs()["entries"],
        content_hashes=_writeback_kwargs()["content_hashes"],
        ruleset_sha256="e4eda0c79c95e4eda",
    )
    assert out["entry_count"] == 1
    keys = out["idempotency_keys"]
    assert len(keys) == 1 and len(keys[0]) == 64
    bundle = out["bundle"]
    assert bundle["resourceType"] == "Bundle"
    assert bundle["type"] == "transaction"
    entries = bundle["entry"]
    assert len(entries) == 2, "1 件 + 1 条 Provenance"
    assert all("request" in e for e in entries), "每条 entry 必须带 request"
    assert entries[1]["resource"]["resourceType"] == "Provenance"


def test_the_dryrun_refuses_an_empty_selection():
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.writeback_dryrun(
            draft_id="D",
            encounter_alias="WARD-A-001",
            actor="护士长A",
            at="2026-03-03T08:00:00+08:00",
            entries=[],
            content_hashes=[],
        )
    assert ei.value.code == "E_NOTHING_SELECTED"


# ---------------------------------------------------------------------------
# 错误码透传
# ---------------------------------------------------------------------------


def test_a_broken_ruleset_surfaces_as_a_stable_code_not_a_chinese_match(fixtures_dir):
    broken = (
        'version: "1.0.0"\n'
        "templates:\n"
        "  restraint_consent:\n"
        "    display: 约束知情同意\n"
        "    fields:\n"
        "      - key: 患方签字\n"
        "        section: 患方知情同意\n"
        "        required: true\n"
        "hospital:\n"
        "  doc_types:\n"
        '    restraint_consent: "约束知情同意"\n'
        "  rules:\n"
        '    - code: "QC-PSY-01"\n'
        "      severity: blocking\n"
        '      title: "必填缺失"\n'
        '      basis: "《精神科住院护理常规》第 4.2 条"\n'
        '      applies_to: ["never_defined_form"]\n'
    )
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.check_bundle(_read(fixtures_dir, "ward-01-minimal.json"), broken)
    assert ei.value.code == "E_RULESET_INVALID"


def test_a_malformed_bundle_surfaces_its_own_code(fixtures_dir, ruleset_yaml):
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.check_bundle('{"resourceType": "Patient"}', ruleset_yaml)
    assert ei.value.code == "E_BUNDLE_SHAPE"


def test_every_core_error_carries_a_code_starting_with_e(fixtures_dir, ruleset_yaml):
    """UI 只按 `E_` 前缀的错误码分支；拿不到码就必须落 `E_INTERNAL`。"""
    with pytest.raises(core_bridge.CoreError) as ei:
        core_bridge.review_batch("/no/such/export/dir", ruleset_yaml, None)
    assert ei.value.code == "E_IO"
    assert ei.value.code.startswith("E_")
    assert ei.value.message
