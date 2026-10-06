"""Rust 核心（`psychdoc_ffi`）的 Python 侧薄壳。

职责只有三件：

1. **定位模块** —— 先 `import psychdoc_ffi`；找不到时回落到
   `bindings/` 下的本机构建产物，让 `run.sh` 与 pytest 在只装了 cargo
   的机器上也能起来。
2. **错误码归一** —— 核心抛的 `PsychdocError` 带 `.code`；这里统一转成
   `CoreError`，让界面按错误码决定「弹窗 / 底栏红字」，而不是去匹配中文。
3. **线程契约** —— 5 个入口全部同步，调用方（`workers.py`）必须放到
   `QtConcurrent` 里跑；本模块只提供纯函数，不自己起线程。
"""

from __future__ import annotations

import json
import os
import sqlite3
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable

PROJECT_ROOT = Path(__file__).resolve().parent.parent


class CoreUnavailable(RuntimeError):
    """核心绑定没构建出来。**不**降级为「只看不做」的假界面。"""


def _load_core():
    """导入 `psychdoc_ffi`，必要时把 `bindings/` 加进 `sys.path`。"""
    try:
        import psychdoc_ffi as core  # type: ignore
    except ImportError:
        bindings = PROJECT_ROOT / "bindings"
        if bindings.is_dir() and str(bindings) not in sys.path:
            sys.path.insert(0, str(bindings))
        try:
            import psychdoc_ffi as core  # type: ignore
        except ImportError as exc:  # pragma: no cover - 环境缺依赖时才走
            raise CoreUnavailable(
                "核心绑定 psychdoc_ffi 未构建。请先执行：\n"
                "  cargo build -p psychdoc-ffi --features extension-module\n"
                "再把 core/target/debug/libpsychdoc_ffi.{dylib,so} 拷到 bindings/ 下，"
                "或安装 maturin 后执行 maturin develop。"
            ) from exc
    return core


core = _load_core()

#: 桌面壳依赖的同步入口。与 Rust 侧 `ENTRY_POINTS` 一一对应。
ENTRY_POINTS = (
    "check_bundle",
    "review_batch",
    "preview_fixback",
    "commit_writeback",
    "graph_both_directions",
    # Task 32：拖入一个 / 几个导出目录 → 按住院别名聚合的审查队列。
    # 判定主体仍是 review_batch；本入口回答「拖进来了什么、哪几次住院、
    # 每次还差几件、哪些文件根本没被打开」。
    "scan_queue",
    # Task 33：文书卡片。一件件 id → 该件的段落 / 字段 / 诊断 / 补写预览。
    "doc_card",
)

#: 与 `spec.md` §5 一致：五项凭据全部从环境变量读，缺一即启动失败。
REQUIRED_ENV = (
    "EMR_BASE_URL",
    "EMR_CLIENT_ID",
    "EMR_CLIENT_SECRET",
    "EMR_TENANT_ID",
    "WECOM_WEBHOOK_URL",
)

#: 写回 / 通知类操作才需要的凭据。读取文书包做本地审查**不需要**。
WRITE_ENV = ("EMR_BASE_URL", "EMR_CLIENT_ID", "EMR_CLIENT_SECRET", "EMR_TENANT_ID")

#: 退出码契约（Task 36 钉死；本模块先按同一份语义实现，供壳引用）。
EXIT_OK = 0
EXIT_FINDINGS = 1
EXIT_CONFIG = 2
EXIT_UNREACHABLE = 3


@dataclass(frozen=True)
class CoreError(Exception):
    """带稳定错误码的核心异常。

    ``code`` 是 UI 唯一该匹配的东西。``message`` 只给人看，
    任何按文案分支的代码都会在规则集改版后悄悄失效。
    """

    code: str
    message: str
    retryable: bool = False

    def __str__(self) -> str:  # pragma: no cover - 仅用于日志
        return f"{self.code}: {self.message}"


def _wrap(fn: Callable[..., Any], *args: Any, **kwargs: Any) -> Any:
    """调一个核心入口，把异常统一成 `CoreError`。"""
    try:
        return fn(*args, **kwargs)
    except core.PsychdocError as exc:  # type: ignore[attr-defined]
        code = getattr(exc, "code", "E_INTERNAL")
        # 端点不可达与 5xx 值得重试；配置与勾选问题重试没有意义。
        retryable = code in ("E_FHIR_UNREACHABLE", "E_FHIR_STATUS")
        raise CoreError(code, str(exc), retryable) from exc
    except ValueError as exc:
        raise CoreError("E_ARG", str(exc)) from exc


# ---------------------------------------------------------------------------
# 5 个同步入口
# ---------------------------------------------------------------------------


def check_bundle(bundle_text: str, ruleset_yaml: str) -> dict[str, Any]:
    """入口 1：一份 Bundle 文本 → 带码、带段落定位、带依据的清单。"""
    return _wrap(core.check_bundle, bundle_text, ruleset_yaml)


def review_batch(
    input_path: str, ruleset_yaml: str, cache_path: str | None = None
) -> dict[str, Any]:
    """入口 2：一个导出目录或文件 → 增量批扫 + 运行摘要。"""
    return _wrap(core.review_batch, input_path, ruleset_yaml, cache_path)


def preview_fixback(bundle_text: str, ruleset_yaml: str) -> dict[str, Any]:
    """入口 3：补写预览（双栏 diff + 只给依据不给按钮的码）。"""
    return _wrap(core.preview_fixback, bundle_text, ruleset_yaml)


def commit_writeback(
    *,
    store_path: str | None,
    draft_id: str,
    encounter_alias: str,
    actor: str,
    at: str,
    entries: list[tuple[str, str]],
    content_hashes: list[tuple[str, str]],
    ruleset_sha256: str = "",
    run_id: str | None = None,
) -> dict[str, Any]:
    """入口 4：草稿 → 逐条勾选 → 整批 Bundle transaction → 状态推进。

    凭据缺失时核心在构造期就抛 ``E_CFG_MISSING``，**零次网络调用**；
    这里不做任何兜底。

    ``ruleset_sha256`` 与 ``run_id`` 决定审计链答不答得出「这次建件是
    按哪一版规则、由哪一次审查发现的」——底栏上就有这两个值，勾选时带上。
    """
    return _wrap(
        core.commit_writeback,
        store_path,
        draft_id,
        encounter_alias,
        actor,
        at,
        entries,
        content_hashes,
        ruleset_sha256,
        run_id,
    )


def graph_both_directions(bundle_text: str, ruleset_yaml: str, node_id: str) -> dict[str, Any]:
    """入口 5：同一张图的正反向两个答案一次拿全。"""
    return _wrap(core.graph_both_directions, bundle_text, ruleset_yaml, node_id)


def scan_queue(
    roots: list[str],
    ruleset_yaml: str,
    at: str,
    store_path: str | None = None,
    persist: bool = True,
) -> dict[str, Any]:
    """入口 6：拖入一个 / 几个导出目录 → 按住院别名聚合的审查队列。

    ``roots`` 可以是文件夹，也可以是单份导出文件（护士长一次拖几个
    病区文件夹也走这里）。返回值给左栏那一列与「已跳过」清单：

    - ``entries``：每次住院一条（别名 / 病区 / 件数 / 缺件数 / 徽标文案）；
    - ``file_skips``：**每一个没被打开的文件**与它的中文原因；
    - ``totals`` 与 ``is_balanced`` / ``filtered_balanced``：两条守恒等式，
      界面上「扫了 N 个文件 / 跳过 M 个」不是编出来的。

    ``persist=True`` 时落本机 sqlite，关窗重开后队列仍在（含已审进度）。
    路径不存在报 ``E_IO``、规则集坏报 ``E_RULESET_INVALID``——两者都不
    降级为「0 条命中 = 全部合规」。
    """
    return _wrap(core.scan_queue, roots, ruleset_yaml, at, store_path, persist)


def doc_card(input_path: str, ruleset_yaml: str, doc_id: str) -> dict[str, Any]:
    """入口 7：文书卡片数据 —— 一件件 id → 可渲染的段落 / 诊断 / 预览。

    ``input_path`` 与 ``graph_both_directions`` 同形：收导出目录、收单份
    文件、也收 Bundle 文本。返回值分四块：

    - ``doc``：段落序列（``sections[].text`` 是高亮的底稿）；
    - ``diagnostics`` / ``parse_issues``：这一件自己的问题；
    - ``preview``：双栏 diff 的候选块 + 只给依据的码 + 底部汇总；
    - ``binding``：绑定状态，``ambiguous`` 时界面要显式提示人工确认。

    找不到 ``doc_id`` 时核心报 ``E_DOC_NOT_FOUND``，**不返回空卡片**——
    空卡片会被读成「这份没问题」。
    """
    return _wrap(core.doc_card, input_path, ruleset_yaml, doc_id)


def archive_view(
    input_path: str,
    ruleset_yaml: str,
    encounter_alias: str,
    as_of: str,
    business_day: str,
    store_path: str | None = None,
) -> dict[str, Any]:
    """入口 8：归档核齐标签页的一屏数据。

    与 ``graph_both_directions``（入口 5）的分工：那个入口回答「某一个节点
    的两个方向」，本入口回答「一整张图长什么样」——切到归档核齐标签时
    护士长要的就是后者。

    返回值分四块：

    - ``rows``：每件一个节点，带**四态** ``state``
      （``missing`` 缺件 / ``ok`` 达标 / ``ambiguous`` 人工待确认 /
      ``blocked`` 被连带阻塞）与正反两个方向的问题清单；
    - ``edges``：每条依赖边带 ``blocking`` 标记（界面据此加粗）；
    - ``missing`` / ``encounter_age``：欠件天数（算不出时是 ``None``
      + 口径说明，**不按今天兜底**）；
    - ``notification``（在每行里）：该件今天的提醒发没发、发出时刻。

    四态由**核心**判定：让界面自己判，早晚出现「橙的到底是连带阻塞
    还是软缺件」这种分歧，而这种分歧会变成两条互相矛盾的催办。
    """
    return _wrap(
        core.archive_view,
        input_path,
        ruleset_yaml,
        encounter_alias,
        as_of,
        business_day,
        store_path,
    )


def notify_missing(
    *,
    encounter_alias: str,
    ward: str,
    business_day: str,
    at: str,
    missing: list[tuple[str, str]],
    group_hint: str,
    dry_run: bool = False,
    store_path: str | None = None,
) -> dict[str, Any]:
    """入口 9：一键发缺件待办（企业微信群机器人 Webhook）。

    ``WECOM_WEBHOOK_URL`` 缺失时核心报 ``E_CFG_MISSING`` 且**零次网络
    调用**——「没配通知」不该把归档核齐视图拖下水，但也绝不假装「已发」。

    ``dry_run=True`` 只构造消息体（同样零次外发），且用的是与真实路径
    **同一段构造代码**，因此干跑看到的正文与真发出去的逐字相同。

    ``missing`` 是 ``(node_id, 院内显示名)`` 列表；空列表报
    ``E_NOTHING_SELECTED``，不发出一条空提醒。
    """
    return _wrap(
        core.notify_missing,
        encounter_alias,
        ward,
        business_day,
        at,
        missing,
        group_hint,
        dry_run,
        store_path,
    )


def load_queue(queue_id: str | None = None) -> dict[str, Any] | None:
    """读回一次落库的队列；``queue_id=None`` 取最近一次载入的。

    返回 ``None`` 表示本机还没落过队列（首次启动的正常情形），
    **不是**错误——界面据此显示起始态的拖放区。
    """
    store_path = default_store_path()
    conn = _sqlite_connect(store_path)
    try:
        if queue_id is None:
            row = conn.execute(
                "SELECT queue_id FROM review_queues "
                "ORDER BY loaded_at DESC, queue_id DESC LIMIT 1"
            ).fetchone()
            if row is None:
                return None
            queue_id = row[0]
        session = conn.execute(
            "SELECT queue_id, source_path, loaded_at, ruleset_short_sha256, "
            "packages_found, files_seen, items_total, missing_total, "
            "file_skips_json, last_scanned_at FROM review_queues WHERE queue_id = ?",
            (queue_id,),
        ).fetchone()
        if session is None:
            return None
        entries = conn.execute(
            "SELECT encounter_alias, ward, doc_count, missing_count, reviewed_at, position "
            "FROM queue_entries WHERE queue_id = ? ORDER BY position ASC",
            (queue_id,),
        ).fetchall()
    finally:
        conn.close()

    file_skips = json.loads(session[8])
    return {
        "queue_id": session[0],
        "source_path": session[1],
        "loaded_at": session[2],
        "ruleset_short_sha256": session[3],
        "entries": [
            {
                "encounter_alias": e[0],
                "ward": e[1],
                "doc_count": e[2],
                "missing_count": e[3],
                "reviewed_at": e[4],
                "position": e[5],
                "badge_zh": (
                    f"{e[2]} 件 · 达标" if e[3] == 0 else f"{e[2]} 件 · 缺件 {e[3]}"
                ),
                "is_complete": e[3] == 0,
            }
            for e in entries
        ],
        "file_skips": file_skips,
        "totals": {
            "packages": session[4],
            "files_seen": session[5],
            "items": session[6],
            "missing": session[7],
            "encounters": len(entries),
            "skipped_files": len(file_skips),
        },
        "progress": {
            "reviewed": sum(1 for e in entries if e[4]),
            "total": len(entries),
        },
    }


def mark_queue_reviewed(queue_id: str, aliases: list[str], at: str) -> dict[str, Any]:
    """把若干次住院标记为「本次已审」，进度落库。

    空列表报 ``E_QUEUE_NOTHING_SELECTED``、未知别名报
    ``E_QUEUE_UNKNOWN_ENCOUNTER``，两者都由核心拒——界面不自己判断
    「这个别名在不在队列里」，那份名单只有核心知道。
    """
    if not aliases:
        raise CoreError(
            "E_QUEUE_NOTHING_SELECTED",
            "未勾选任何住院，拒绝标记已审（否则「已审 N 条」会混进从没审过的）",
        )
    store_path = default_store_path()
    conn = _sqlite_connect(store_path)
    try:
        marked = 0
        for alias in aliases:
            cur = conn.execute(
                "UPDATE queue_entries SET reviewed_at = ? "
                "WHERE queue_id = ? AND encounter_alias = ?",
                (at, queue_id, alias),
            )
            if cur.rowcount == 0:
                raise CoreError(
                    "E_QUEUE_UNKNOWN_ENCOUNTER",
                    f"队列里没有住院别名 {alias} 这一条，拒绝标记已审",
                )
            marked += cur.rowcount
        conn.commit()
    finally:
        conn.close()
    return {"marked": marked}


def _sqlite_connect(store_path: str):
    """打开本机 sqlite 库（迁移已由核心侧 `Store::open` 落过）。

    库不存在时**不**凭空建一个空的：没落过队列就是「还没落过」，
    建一个空壳会让 `load_queue` 分不清「首次启动」与「队列被清空了」。
    """
    path = Path(store_path)
    if not path.exists():
        raise CoreUnavailable(
            f"本机还没有审查队列库：{path}\n"
            "第一次用请先拖入一个病区导出目录，落库后关窗重开队列就会一直在。"
        )
    conn = sqlite3.connect(store_path)
    conn.execute("PRAGMA foreign_keys = ON")
    return conn


def writeback_dryrun(
    *,
    draft_id: str,
    encounter_alias: str,
    actor: str,
    at: str,
    entries: list[tuple[str, str]],
    content_hashes: list[tuple[str, str]],
    ruleset_sha256: str = "",
) -> dict[str, Any]:
    """干跑：只构造 transaction，**不发任何网络请求**。"""
    return _wrap(
        core.writeback_dryrun,
        draft_id,
        encounter_alias,
        actor,
        at,
        entries,
        content_hashes,
        ruleset_sha256,
    )


def startup_check() -> dict[str, Any]:
    """启动自检（严格）：五项凭据缺一即 `E_CFG_MISSING`，零次网络调用。"""
    return _wrap(core.startup_check)


def startup_report() -> dict[str, Any]:
    """启动自检（逐项）：**不抛异常**，把结论全摆出来。

    判定逻辑在 Rust 侧（`psychdoc_fhir::StartupReport`），这里只做搬运——
    壳里再实现一份判定，两边迟早会漂移，而漂移的方向通常是
    「壳里那份更宽松」，于是缺配置被悄悄放过。

    返回值里只有凭据的**变量名**，没有取值：这份报告要显示在窗口上。
    """
    return _wrap(core.startup_report)


def core_version() -> str:
    """核心版本号。绑定的 `CORE_VERSION` 是常量，不是可调用的。"""
    return str(core.CORE_VERSION)


# ---------------------------------------------------------------------------
# 非凭据配置
# ---------------------------------------------------------------------------


def default_ruleset_path() -> Path:
    """规则集默认位置。可用 ``PSYCHDOC_RULESET`` 覆盖。"""
    override = os.environ.get("PSYCHDOC_RULESET", "").strip()
    return Path(override) if override else PROJECT_ROOT / "rulesets" / "hospital.yaml"


def default_cache_path() -> Path:
    """增量缓存落**用户数据目录**，不落在项目子目录（否则会被误提交）。"""
    override = os.environ.get("PSYCHDOC_CACHE", "").strip()
    if override:
        return Path(override)
    base = os.environ.get("XDG_DATA_HOME") or os.path.join(
        os.path.expanduser("~"), ".local", "share"
    )
    return Path(base) / "psychdoc" / "review-cache.json"


def default_store_path() -> str:
    """审查队列的 sqlite 库路径（用户数据目录，不落项目子目录）。

    落位规则与 Rust 侧 `psychdoc_store::paths` 一致：``PSYCHDOC_DATA_DIR``
    优先，其次按平台落到用户数据目录。之所以让 Python 侧也认这个变量，
    是为了测试与运维能把库指到临时目录——否则每跑一次用例就在真实用户
    目录里留一个库文件。
    """
    override = os.environ.get("PSYCHDOC_STORE", "").strip()
    if override:
        return override
    explicit = os.environ.get("PSYCHDOC_DATA_DIR", "").strip()
    if explicit:
        base = Path(explicit)
    elif sys.platform == "darwin":
        base = Path(os.path.expanduser("~")) / "Library" / "Application Support" / "PsychInpatientDocAudit"
    elif os.name == "nt":
        base = Path(os.environ.get("APPDATA", Path.home() / "AppData/Roaming")) / "PsychInpatientDocAudit"
    else:
        base = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "psych-inpatient-doc-audit"
    return str(base / "psychdoc.sqlite3")


def queue_data_dir() -> Path:
    """队列库所在目录（供界面在「还没落库」时给出可执行的提示）。"""
    return Path(default_store_path()).parent


def missing_write_env() -> list[str]:
    """列出写回所需的、当前缺失的凭据名。**只读，不外发**。"""
    return [k for k in WRITE_ENV if not os.environ.get(k, "").strip()]


def missing_required_env() -> list[str]:
    """列出全部缺失的凭据名（含通知 webhook），供启动自检一次列全。"""
    return [k for k in REQUIRED_ENV if not os.environ.get(k, "").strip()]
