//! `psychdoc` CLI —— 无 GUI 回归与运维通道。
//!
//! 存在理由：院内信息科在服务器上做**无人值守跑批**、CI 做**规则回归**，
//! 两个场景都不会有人去点开一个桌面窗口。因此本二进制必须能独立完成
//! 「读包 → 判定 → 出可核对结论」，并把结论交给**只看一个数字**的自动化。
//!
//! ## 源产品主路径的领养位置
//!
//! 源产品（Python linter 家族）的主路径是「一条命令拿全量 → 逐文件解析 →
//! 按稳定编号输出码 + 位置 + 为什么这是问题」，本 CLI 是它的直系改写：
//!
//! | 源侧 | 本产品侧 |
//! |------|----------|
//! | `cli.py` 薄壳 → `main()` 分发子命令 | 本文件 [`run`] 分发五个子命令 |
//! | `check` 子命令逐文件解析并打印 | `check` 读 FHIR R4 导出包（单份或整个目录） |
//! | `Violation(code, range, message)` | `Diagnostic(code, loc, message, basis)` |
//! | `analyze_graph --direction` 正反两向 | `graph --direction both\|dependencies\|dependents` |
//! | 缓存避免重跑未变更文件 | `--cache` 按件内容哈希增量重跑 |
//!
//! 退出码契约在 [`contract`] 里钉死：`0` 成功 / `1` 审查不通过 /
//! `2` 配置或用法问题 / `3` 外部依赖不可达。

mod contract;
mod dryrun_cmd;
mod graph_cmd;
mod sink;

use contract::{CliError, EXIT_FINDINGS, EXIT_OK};
use psychdoc_rules::{load_effective, review, review_incremental, ReviewCache};
use std::process::ExitCode;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => ExitCode::from(code),
        Err(e) => {
            // 错误文案同时上 stderr 与日志；`--json` 下 stdout 保持干净，
            // 让 CI 能从 stderr 抓原因、从退出码判成败。
            sink::say_err(&format!("psychdoc: {}：{}", e.code, e.message));
            if sink::json_mode() && !e.json_reported {
                sink::emit_json(
                    &serde_json::json!({
                        "ok": false,
                        "code": e.code,
                        "message": e.message,
                        "exit_code": contract::exit_for_code(&e.code),
                        "exit_meaning": contract::describe(contract::exit_for_code(&e.code)),
                    })
                    .to_string(),
                );
            }
            ExitCode::from(contract::exit_for_code(&e.code))
        }
    }
}

/// 全局选项（在任何子命令位置都接受），先摘出来再交给子命令解析。
///
/// 摘而不是让每个子命令各认一遍：`--json` 与 `--log-file` 对五个子命令
/// 含义完全一样，让它们各解析一次就等于给了五种可能不一致的行为。
struct GlobalOpts {
    json: bool,
    log_file: Option<String>,
}

fn take_globals(args: &[String]) -> (GlobalOpts, Vec<String>) {
    let mut g = GlobalOpts {
        json: false,
        log_file: None,
    };
    let mut rest = Vec::new();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--json" => {
                g.json = true;
                i += 1;
            }
            "--log-file" => {
                // 取不到值时原样留在 rest 里，交给子命令报「--log-file 需要取值」——
                // 在这里悄悄吞掉会让用户以为日志已经落盘了。
                if let Some(v) = args.get(i + 1) {
                    g.log_file = Some(v.clone());
                    i += 2;
                } else {
                    rest.push(args[i].clone());
                    i += 1;
                }
            }
            other => {
                rest.push(other.to_string());
                i += 1;
            }
        }
    }
    (g, rest)
}

fn run(args: &[String]) -> Result<u8, CliError> {
    let (g, rest) = take_globals(args);
    if let Some(p) = &g.log_file {
        sink::open_log(p)?;
    }
    sink::set_json_mode(g.json);

    let Some(sub) = rest.first().map(String::as_str) else {
        sink::say(&usage());
        return Ok(EXIT_OK);
    };
    let result = match sub {
        "check" => check(&rest[1..], g.json),
        "graph" => graph(&rest[1..], g.json),
        "writeback-dryrun" => dryrun(&rest[1..], g.json),
        "rule-regress" => rule_regress(&rest[1..]),
        "export-audit" => export_audit(&rest[1..]),
        "codes" => codes(&rest[1..], g.json),
        "preflight" => preflight(&rest[1..], g.json),
        "--help" | "-h" | "help" => {
            sink::say(&usage());
            Ok(EXIT_OK)
        }
        other => Err(CliError::usage(format!(
            "未知子命令 {other}\n\n{}",
            usage()
        ))),
    };
    // `--json` 的正文由各子命令自己 emit（它们的结构各不相同，强行统一
    // 只会得到一个把 check 与 graph 硬塞进同一个键的畸形 schema）。
    // 这里只保证 stdout 上**至多一段** JSON。
    result
}

/// `psychdoc preflight`：启动自检。缺一项即退 2，**零次网络调用**。
///
/// 存在的理由是运维的改配置流程：`.env` 改完先跑这一条，
/// 一次就能看到**全部**缺项与形态错，而不是跑一次批扫撞一条、再改一条。
/// 它与 `check` 的分工是硬的——`check` 不碰网络也不读凭据（本地文书包
/// 审查不需要），`preflight` 专门回答「院内对接能不能用」。
fn preflight(args: &[String], json_mode: bool) -> Result<u8, CliError> {
    // preflight 不接任何参数：给了就是用法错，不静默忽略。
    if let Some(extra) = args.first() {
        return Err(CliError::usage(format!(
            "preflight 不接受参数 {extra}\n\n{}",
            usage()
        )));
    }

    let report = psychdoc_fhir::StartupReport::check(|k| std::env::var(k).ok());

    // 严格口径：走一遍完整加载器。形态错、未知日志级别都在这里被拦下，
    // 报码与缺失项分开——两类问题找的人不一样。
    let strict = psychdoc_fhir::StartupConfig::from_env();

    if json_mode {
        let mut v = serde_json::json!({
            "ok": report.writeback_ready,
            "ready": report.ready,
            "missing": report.missing,
            "invalid": report.invalid.iter()
                .map(|(k, why)| serde_json::json!({"key": k, "detail": why}))
                .collect::<Vec<_>>(),
            "notices": report.notices,
            "network_calls": 0,
        });
        match &strict {
            Ok(cfg) => {
                v["ruleset"] = serde_json::json!(cfg.ruleset.display().to_string());
                v["log_level"] = serde_json::json!(cfg.log_level.as_zh());
                v["data_dir"] = match &cfg.data_dir {
                    Some(d) => serde_json::json!(d.display().to_string()),
                    None => serde_json::Value::Null,
                };
            }
            Err(e) => {
                v["code"] = serde_json::json!(e.code());
                v["message"] = serde_json::json!(e.to_string());
            }
        }
        sink::emit_json(&v.to_string());
    } else {
        sink::say("启动自检（本次未发起任何网络请求）：");
        sink::say(&report.render());
        match &strict {
            Ok(cfg) => {
                sink::say(&format!(
                    "  [规则集] {}",
                    cfg.ruleset.display()
                ));
                sink::say(&format!("  [日志级别] {}", cfg.log_level.as_zh()));
                match &cfg.data_dir {
                    Some(d) => sink::say(&format!("  [数据目录] {}", d.display())),
                    None => sink::say("  [数据目录] 未显式设置，按平台用户数据目录规则解析"),
                }
            }
            Err(e) => sink::say(&format!("  [拦截] {e}")),
        }
    }

    match strict {
        Ok(_) => Ok(EXIT_OK),
        // `already_reported`：报告正文（含 code 与 message）刚已写进 stdout，
        // 再让 main 补一段就会破坏「stdout 至多一段 JSON」的契约。
        Err(e) => Err(CliError::from_display(e).already_reported()),
    }
}

fn usage() -> String {
    concat!(
        "用法：\n",
        "  psychdoc check <文书包.json | 目录> --ruleset <规则集.yaml> [--cache <缓存.json>]\n",
        "                         [--record <库路径|default>] [--actor <操作人>]\n",
        "  psychdoc graph <住院别名> --input <文书包.json | 目录> --ruleset <规则集.yaml>\n",
        "                         [--direction both|dependencies|dependents] [--as-of <RFC3339>]\n",
        "                         [--db <库路径>]\n",
        "  psychdoc writeback-dryrun <selection.json> [--out <Bundle.json>]\n",
        "  psychdoc rule-regress [--cases <用例集.yaml>] [--ruleset <规则集.yaml>]\n",
        "                         [--root <根目录>] [--only <子串>] [--json <报告.json>]\n",
        "  psychdoc export-audit [--db <库路径>] [--alias <住院别名>] [--run <批次号>]\n",
        "                         [--since <RFC3339>] [--until <RFC3339>] [--at <RFC3339>]\n",
        "                         [--format csv|json] [--out <文件路径>]\n",
        "  psychdoc codes [--format markdown|json|text] [--code <错误码>]\n",
        "                         [--out <文件路径>]\n",
        "  psychdoc preflight\n",
        "\n",
        "全局选项（五个子命令都接受）：\n",
        "  --json          stdout 只输出一段机器可读 JSON，人类可读过程文字改进日志。\n",
        "  --log-file <路径> 把完整过程写进磁盘日志（覆盖写）。不与 --json 同用也不丢内容。\n",
        "\n",
        "退出码契约：\n",
        "  0  成功：审查零阻断 / 视图已核齐 / 干跑构造成功\n",
        "  1  审查不通过：存在阻断项，或归档核齐未通过（缺件 / 连带阻塞 / 人工待办）\n",
        "  2  配置或用法问题：缺环境变量、规则集非法、参数写错\n",
        "  3  外部依赖不可达：EMR 端点连不上或返回错误\n",
        "\n",
        "check        输入一份 FHIR R4 Bundle 导出文件（或一个导出目录），输出带规则码 + 段落定位 + 规范依据的清单。\n",
        "             带 --cache 时按件内容哈希增量重跑：未变过的件跳过规则执行，底栏给出重跑 / 跳过件数。\n",
        "             带 --record 时把这次审查落进本机库，供 export-audit 导出审计包；同一批重跑不追加第二条记录。\n",
        "graph        输出某次住院的归档核齐视图：件清单、阻塞边、参考缺件、人工待确认队列、欠件天数与口径说明。\n",
        "             --direction 收窄输出：dependencies 只看「这件在等谁」，dependents 只看「这件卡住谁」，\n",
        "             both 两个方向都给（出图前先跑正反向对拍，对不上就拒出视图）。\n",
        "             已核齐退 0，未核齐退 1 —— CI 拿它当「这份病历能不能归档」的闸门。\n",
        "writeback-dryrun  读勾选清单，构造一份 Bundle transaction 并打印报文摘要与幂等键，\n",
        "             **不发起任何网络请求**（不取 token、不连端点、不落库）。CI 在无凭据环境里据此守报文形状。\n",
        "rule-regress 跑规则回归用例集：逐条用例报通过 / 失败，并输出按规则码统计的正例 / 反例 / 边界例覆盖矩阵。\n",
        "             任一用例失败或覆盖矩阵有缺位 → 退出码 1（规则集改坏了就去改规则集，不是去改用例）。\n",
        "export-audit 从本机库导出审计证据包（csv 默认带 UTF-8 BOM，Excel 双击即中文不乱码）。\n",
        "             每行含批次号 / 住院别名 / 件 id / 段落定位 / 规则码 / 处置 / 严重度 / 规范依据 /\n",
        "             豁免原因 / 规则集 sha256 / 规则来源层 / 推进人 / 时刻 / 写回幂等键。\n",
        "             「处置」列区分已报出与已折叠：被更具体规则压下的命中照样在包里，\n",
        "             否则「当年只报了一条」这句追问答不上来。导出前二次脱敏：\n",
        "             标识列命中未脱敏内容 → 拒绝导出（E_EXPORT_LEAK）；自由文本列就地替换并计数。\n",
        "             带 --at 时附带版本回溯：那一刻生效的规则集哈希与命中码集合。\n",
        "             带 --out 落 csv 时，归档核齐视图另写一份同名「-归档核齐.csv」。\n",
        "codes        打印全量错误码表：每个码的触发条件、是否可重试、给使用者的处置建议。\n",
        "             值班的人拿它判断「这个码该找谁」，不必翻源码。带 --code <错误码> 只看一条。\n",
        "             --format markdown 导出表格（README「错误与降级」一节的数据源）。\n",
        "             表由代码生成而非手抄：代码里加了码而表里没加，cargo test 会当场失败。\n",
        "preflight    启动自检：逐项列出五项凭据与非凭据配置的结论，缺一项退 2。\n",
        "             改完 .env 先跑这一条，**一次看到全部缺项**与形态错；\n",
        "             全程零次网络请求（缺配置在发出的第一个包之前就被拦下）。\n",
        "             缺凭据不等于不能审查：check / graph / writeback-dryrun 不读凭据照常可用。"
    )
    .to_string()
}

/// `psychdoc graph <住院别名> --input … --ruleset …`。
fn graph(args: &[String], json_mode: bool) -> Result<u8, CliError> {
    let mut encounter: Option<String> = None;
    let mut input: Option<String> = None;
    let mut ruleset_path: Option<String> = None;
    let mut direction = graph_cmd::DirectionArg::Both;
    let mut db: Option<String> = None;
    let mut as_of: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let key = args[i].clone();
        match key.as_str() {
            "--input" | "--ruleset" | "--direction" | "--db" | "--as-of" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::usage(format!("{key} 后面需要取值\n\n{}", usage())))?
                    .clone();
                match key.as_str() {
                    "--input" => input = Some(v),
                    "--ruleset" => ruleset_path = Some(v),
                    "--direction" => direction = graph_cmd::DirectionArg::parse(&v)?,
                    "--db" => db = Some(v),
                    "--as-of" => as_of = Some(v),
                    _ => unreachable!(),
                }
                i += 2;
            }
            other if other.starts_with("--") => {
                return Err(CliError::usage(format!(
                    "graph 不支持的选项 {other}\n\n{}",
                    usage()
                )));
            }
            other => {
                if encounter.is_some() {
                    return Err(CliError::usage(format!(
                        "graph 多余的位置参数 {other}（住院别名只能给一个）\n\n{}",
                        usage()
                    )));
                }
                encounter = Some(other.to_string());
                i += 1;
            }
        }
    }

    let encounter = encounter.ok_or_else(|| {
        CliError::usage(format!("graph 缺少住院别名\n\n{}", usage()))
    })?;
    let input = input.ok_or_else(|| {
        CliError::usage(format!(
            "graph 缺少 --input <文书包.json | 目录>\n\n{}",
            usage()
        ))
    })?;
    let ruleset_path = ruleset_path.ok_or_else(|| {
        CliError::usage(format!(
            "graph 缺少 --ruleset <规则集.yaml>\n\n{}",
            usage()
        ))
    })?;
    let yaml = std::fs::read_to_string(&ruleset_path)
        .map_err(|e| CliError::new("E_IO", format!("读不到规则集 {ruleset_path} — {e}")))?;

    graph_cmd::run(
        &encounter,
        &input,
        &yaml,
        direction,
        db.as_deref(),
        as_of.as_deref(),
        json_mode,
    )
}

/// `psychdoc writeback-dryrun <selection.json> [--out <Bundle.json>]`。
fn dryrun(args: &[String], json_mode: bool) -> Result<u8, CliError> {
    let mut positional: Vec<String> = Vec::new();
    let mut out: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--out" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| {
                        CliError::usage(format!("--out 后面需要文件路径\n\n{}", usage()))
                    })?
                    .clone();
                out = Some(v);
                i += 2;
            }
            other if other.starts_with("--") => {
                return Err(CliError::usage(format!(
                    "writeback-dryrun 不支持的选项 {other}\n\n{}",
                    usage()
                )));
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }
    let Some(selection) = positional.first() else {
        return Err(CliError::usage(format!(
            "writeback-dryrun 缺少 <selection.json>\n\n{}",
            usage()
        )));
    };
    dryrun_cmd::run(selection, json_mode, out.as_deref())
}

fn check(args: &[String], json_mode: bool) -> Result<u8, CliError> {
    // 先摘掉带值的长选项，剩下的位置参数才是文书包路径。
    let mut positional: Vec<String> = Vec::new();
    let mut ruleset_path: Option<String> = None;
    let mut cache_path: Option<String> = None;
    let mut record_path: Option<String> = None;
    let mut actor: Option<String> = None;
    let mut live: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            // --live：直接从院内 FHIR R4 端点拉这一次住院的文书包。
            // 无人值守跑批的真实入口——不靠人工先导出一份 json 落到盘上。
            "--live" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::usage("--live 后面需要住院别名".to_string()))?;
                live = Some(v.clone());
                i += 2;
            }
            "--cache" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "--cache 后面需要缓存文件路径".to_string())?;
                cache_path = Some(v.clone());
                i += 2;
            }
            "--record" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "--record 后面需要库路径或「default」".to_string())?;
                record_path = Some(v.clone());
                i += 2;
            }
            "--actor" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "--actor 后面需要操作人姓名".to_string())?;
                actor = Some(v.clone());
                i += 2;
            }
            "--ruleset" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| "--ruleset 后面需要规则集路径".to_string())?;
                ruleset_path = Some(v.clone());
                i += 2;
            }
            other if other.starts_with("--") => {
                return Err(CliError::usage(format!("check 不支持的选项 {other}\n\n{}", usage())));
            }
            other => {
                positional.push(other.to_string());
                i += 1;
            }
        }
    }

    let Some(ruleset_path) = ruleset_path else {
        return Err(CliError::usage(format!("check 缺少 --ruleset <规则集.yaml>\n\n{}", usage())));
    };
    // `--live` 自带入料来源（从 EMR 拉），此时不需要位置参数；
    // 其余情况必须给一条包路径或一个导出目录。
    // `--live` 与位置参数互斥：--live 自己从 EMR 拉件，不读本地文件。
    // 两者都给时若不报错，多半是脚本拼错了，而「哪个输入被采用了」
    // 只有到出结果时才知道——所以在这里就拒掉。
    if live.is_some() && !positional.is_empty() {
        return Err(CliError::usage(
            "check --live 与位置参数互斥：--live 自己从 EMR 拉件，不读本地文件",
        ));
    }
    let mut input = positional.first().cloned().unwrap_or_default();
    if input.is_empty() && live.is_none() {
        return Err(CliError::usage(format!("check 缺少文书包路径\n\n{}", usage())));
    }

    let yaml = std::fs::read_to_string(&ruleset_path)
        .map_err(|e| format!("读不到规则集 {ruleset_path} — {e}"))?;

    // 规则集加载 / 校验失败即拒绝启动该批，**不**降级为「无规则 = 全部通过」。
    let ruleset = load_effective(&yaml).map_err(CliError::from_display)?;
    let registry = ruleset.registry();

    // 输入可以是一份 Bundle，也可以是一个导出目录（护士长一次拖一整个文件夹进来）。
    let mut items = Vec::new();
    let mut parse_issues = Vec::new();
    let mut skipped_total = 0usize;
    if live.is_none() {
        let path = std::path::Path::new(&input);
        if path.is_dir() {
            let packages = psychdoc_parse::discover_dir(path).map_err(CliError::from_display)?;
            for (_, pkg) in &packages {
                items.extend(pkg.items.iter().cloned());
                parse_issues.extend(pkg.errors.iter().cloned());
                // 被过滤的 entry 是「读得出来但不是本院文书」的部分，
                // 单独计数并入「未解析」，否则「0 条诊断」会被误读成全部合规。
                skipped_total += pkg.filtered.len();
            }
            sink::say(&format!("自 {input} 发现导出包 {} 份。", packages.len()));
        } else {
            let json = std::fs::read_to_string(&input)
                .map_err(|e| format!("读不到文书包 {input} — {e}"))?;
            let parsed = psychdoc_parse::parse_package_with(&json, Some(&registry))
                .map_err(CliError::from_display)?;
            items.extend(parsed.items);
            parse_issues.extend(parsed.errors);
            skipped_total += parsed.filtered.len();
        }
    }

    // --live：走真实 FHIR R4 读端点，凭据从环境变量读。
    // 这是本产品唯一一条「不打脱敏文件、直接对接院内 EMR」的入料路径，
    // 无人值守跑批靠它；桌面端的拖放审查则走上面的文件/目录分支。
    // 凭据缺失在 `EmrConfig::from_env` 阶段就报 E_CFG_MISSING，
    // **物理上不可能先发出网络请求**——这是退出码 2 与 3 分得开的根据。
    if let Some(encounter) = &live {
        let config = psychdoc_fhir::startup_check()
            .map_err(CliError::from_display)?;
        let transport = psychdoc_fhir::UreqTransport::new(std::time::Duration::from_secs(10));
        let token = psychdoc_fhir::fetch_token(&config, &transport)
            .map_err(CliError::from_display)?;
        let read = psychdoc_fhir::read_package(&config, &token, encounter, &registry, &transport)
            .map_err(CliError::from_display)?;
        sink::say(&format!(
            "自 {} 读取住院 {encounter}：遍历 {} 页 · 已审文书 {} 件 · 未解析 {} 件。",
            config.base_url,
            read.pages,
            read.items.len(),
            read.unresolved.len()
        ));
        for u in &read.unresolved {
            sink::say(&format!(
                "  [未解析] {} · {} — {}",
                u.resource_id, u.code, u.detail
            ));
        }
        items.extend(read.items);
        input = format!("<live:{encounter}@{}>", config.base_url);
    }

    // 增量 vs 全量：带 --cache 走增量（未变过的件跳过规则执行），
    // 不带则全量跑。两者判定结果相同，区别只在「算了多少遍」。
    //
    // 计时从规则执行**开始**算，不含读包：耗时这一栏回答的是
    // 「这台机器审这么多件要多久」，把磁盘读包的时间算进去会让
    // 两次运行之间的差别全被 I/O 抖动吃掉。
    let started = std::time::Instant::now();
    let (result, summary_line) = match &cache_path {
        Some(cp) => {
            let mut cache = ReviewCache::load(std::path::Path::new(cp))?;
            let mut run = review_incremental(&items, &ruleset, &mut cache, skipped_total);
            // 解析期问题与增量无关：跳过重跑不等于「这次不用再报解析问题」。
            run.result.parse_issues.extend(parse_issues.iter().cloned());
            // 规则集变了 → 缓存整表作废。这是「本次一件都没走缓存」的
            // 唯一合理解释，必须打在输出里，否则看起来像工具坏了。
            // 摘要行末尾也带了同一条（`RunSummary::as_zh`），这里只补一句
            // 「旧版结论一条都没复用」——重复整段文案只会让人以为是两件事。
            if run.summary.invalidated.is_some() {
                sink::say("");
                sink::say("⚠ 规则集已变更，本次按新规则集重新审了全部文书，旧版结论一条都没复用。");
            }
            cache.save(std::path::Path::new(cp))?;
            let line = run.summary.as_zh();
            (run.result, Some(line))
        }
        None => {
            let mut r = review(&items, &ruleset);
            r.parse_issues.extend(parse_issues.iter().cloned());
            (r, None)
        }
    };

    // 结构化运行摘要：一条 JSON 行，写进用户数据目录下的 logs/。
    //
    // 与 `--log-file`（人读的自由文本）并存而不是替代：值班的人要的是
    // 「这次到底看了什么、报了什么、有没有件从头到尾没被判」，
    // 这三个问题用 grep 自由文本答不齐，用 jq 读这一行才行。
    let elapsed = started.elapsed();
    let mut run_summary = build_run_summary(&items, skipped_total, &result);
    emit_run_log(&mut run_summary, &result, elapsed.as_millis() as u64);

    print_report(
        &input,
        &ruleset_path,
        &ruleset,
        &result,
        &result.parse_issues,
        summary_line.as_deref(),
    );

    // `--json`：stdout 上只留这一段。清单正文已经在上面打过（`--json`
    // 模式下 sink::say 不写 stdout），所以这里不是「再打一遍」，
    // 而是给 CI 一份字段稳定、可直接 jq 的结论。
    if json_mode {
        emit_check_json(
            &input,
            &ruleset_path,
            &result,
            summary_line.as_deref(),
            Some(&run_summary),
        );
    }

    // --record：把这次审查落进本机库，**导出包才有东西可导**。
    // 刻意做成显式开关而不是默认落库：审查清单是看的东西，审计证据是留的东西，
    // 「看」不该悄悄变成「留」。质控办要的是能回查的痕迹，不是堆出来的流水。
    if let Some(rp) = &record_path {
        // 一次审查只用**一个**时刻：批次行、诊断行与归档视图若各读一次
        // 系统时钟，跨零点时会分属两个时刻，「这一批是几点审的」就答不上了。
        let record_at = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false);
        // 折叠命中的那组也要进证据包：清单上只留了一条，但质控办
        // 对着当年的截图问「就报了一条？」时，答案是压下去的那几条。
        let suppressed: Vec<psychdoc_model::Diagnostic> =
            result.suppressed.iter().map(suppressed_to_diagnostic).collect();
        // 规则来源层：同一个码在不同病区生效依据可能不同。落库后
        // 导出才答得了「为什么这个病区报得这么松」。
        let rule_sources: Vec<(String, String)> = result
            .diagnostics
            .iter()
            .chain(suppressed.iter())
            .filter_map(|d| {
                let p = ruleset.rule(&d.code)?.provenance();
                Some((d.code.clone(), p))
            })
            .collect::<std::collections::BTreeMap<_, _>>()
            .into_iter()
            .collect();
        let noted = record_review_to_store(
            rp,
            actor.as_deref(),
            &result,
            &result.diagnostics,
            &suppressed,
            &rule_sources,
            &record_at,
        )
        .map_err(CliError::from_display)?;
        sink::say("");
        sink::say(&format!(
            "已记入本机审查库：批次 {noted}（报出 {} 条 · 折叠 {} 条）",
            result.diagnostics.len(),
            result.suppressed.len()
        ));
        // 归档核齐视图随批次落库：质控办问「这份当时核齐了没有、欠件几天」，
        // 答案是这一份视图，而不是让对方重跑一遍工具。
        record_archive_views(rp, &items, &ruleset, &record_at)
            .map_err(CliError::from_display)?;
    }

    Ok(if result.blocking_count() > 0 {
        EXIT_FINDINGS
    } else {
        EXIT_OK
    })
}

/// 把这批件涉及的每个住院的**归档核齐视图**随批次落库。
///
/// 视图由 `psychdoc-graph::DocGraph::archive_view` 算——**不重算一遍**。
/// 界面（Task 34）与导出（Task 31）消费的是同一个结构、同一个口径；
/// 各算各的必然漂移，而漂移的导出比没有导出更糟。
///
/// 缺件登记簿此刻从库里已登记的缺失件反推首次发现时刻：库里没有更早
/// 记录时**不填当前时间**（那会把「欠了 5 天」显示成「今天才发现」），
/// 让 `days` 保持 `None` 并由视图自己的口径说明交代。
fn record_archive_views(
    db: &str,
    items: &[psychdoc_model::DocItem],
    ruleset: &psychdoc_rules::EffectiveRules,
    at: &str,
) -> Result<usize, String> {
    let store = if db == "default" {
        psychdoc_store::Store::open_default().map_err(|e| e.to_string())?
    } else {
        psychdoc_store::Store::open(db).map_err(|e| e.to_string())?
    };

    // 一次可能跨多个住院（导出目录），逐个住院各出一份视图。
    let mut aliases: Vec<String> = items
        .iter()
        .map(|i| i.encounter_alias.clone())
        .filter(|a| !a.is_empty())
        .collect();
    aliases.sort();
    aliases.dedup();
    if aliases.is_empty() {
        return Ok(0);
    }

    let now = chrono::DateTime::parse_from_rfc3339(at)
        .map_err(|e| format!("审查时刻 {at} 不是 RFC3339：{e}"))?;

    // 全批一张图，视图按住院过滤：跨住院连边是上一道任务就拦掉的错误，
    // 这里再分图只会让「同一张图里哪些件互相阻塞」这个问题没有答案。
    let graph = psychdoc_graph::DocGraph::build(items, ruleset);

    let mut written = 0usize;
    for alias in aliases {
        let alias_items: Vec<_> = items
            .iter()
            .filter(|i| i.encounter_alias == alias)
            .cloned()
            .collect();

        // 缺件登记簿：只登记**库里已有状态记录**的缺失件，取其首次进入
        // 缺失态的时刻。没有记录就不登记——`days` 返回 None 并附口径说明，
        // 比拿当前时间凑一个「0 天」诚实。
        let mut ledger = psychdoc_graph::MissingLedger::new();
        for it in &alias_items {
            if let Some(state) = store.state_of(&it.doc_id).map_err(|e| e.to_string())? {
                if state == psychdoc_model::state::DocState::Missing {
                    if let Some(at) = store
                        .missing_since(&it.doc_id)
                        .map_err(|e| e.to_string())?
                        .and_then(|t| chrono::DateTime::parse_from_rfc3339(&t).ok())
                    {
                        ledger.record(it.doc_id.clone(), at);
                    }
                }
            }
        }

        let view = graph
            .archive_view(&alias, now, &ledger, ruleset)
            .map_err(|e| format!("{alias} 的归档核齐视图算不出来：{e}"))?;
        let view_json =
            serde_json::to_string(&view).map_err(|e| format!("视图序列化失败：{e}"))?;

        // 视图挂在**最近一次**该住院的审查批次上：它描述的是这次审查
        // 看到的件。挂错批次会让「按批次筛导出」时看不到这份视图。
        let run_id: Option<String> = store
            .conn()
            .query_row(
                "SELECT run_id FROM review_runs WHERE encounter_alias = ?1
                 ORDER BY started_at DESC LIMIT 1",
                [&alias],
                |r| r.get(0),
            )
            .ok();
        let Some(run_id) = run_id else { continue };

        store
            .record_archive_view(&psychdoc_store::ArchiveSnapshot {
                view_id: psychdoc_store::ArchiveSnapshot::view_id(&run_id, at),
                run_id,
                encounter_alias: alias.clone(),
                as_of: at.to_string(),
                is_ready: view.is_ready(),
                view_json,
                recorded_at: at.to_string(),
            })
            .map_err(|e| e.to_string())?;
        written += 1;
    }
    Ok(written)
}

/// 一条被折叠压下的命中 → 可落库的 `Diagnostic`。
///
/// 转换而不是另立一张表：导出的列（码 / 段 / 区间 / 文案 / 依据）在
/// 折叠前后**完全一样**，唯一差别是「这条被谁盖掉了」。拆两张表就得
/// 在导出时把两套列重新对齐一次，而对齐迟早会漏——漏掉的那一列
/// 正好是质控办唯一会追问的那列。
///
/// `severity` 取输家自己的严重度：质控办要看的是「本来有多重」，
/// 而不是赢家那档（赢家可能被抬高过，见 `SuppressedFinding::escalated`）。
/// 抬高与否由 `suppressed_by` 那列交代。
fn suppressed_to_diagnostic(s: &psychdoc_rules::SuppressedFinding) -> psychdoc_model::Diagnostic {
    psychdoc_model::Diagnostic {
        code: s.code.clone(),
        loc: s.loc.clone(),
        message: psychdoc_model::RuleMessage::new(s.message.clone(), s.basis.clone())
            .expect("压下的命中自带文案与依据，构造必定成功"),
        // 折叠是展示层的事，不是「这条能自动改」——沿用不可自动修。
        fix_availability: psychdoc_model::FixAvailability::None,
        severity: s.severity,
        fix: None,
        suppressed_by: Some(format!(
            "{}（{}：{}）",
            s.suppressed_by, s.family_label, s.reason
        )),
        waived: None,
    }
}

/// 把一次审查落库，返回批次号。
///
/// 批次号 = `sha256(住院别名 + 规则集哈希 + 全部件的内容哈希)` 截前 12 位。
/// 内容进键是有意的：同一份文书包、同一版规则集**重跑不产生第二条记录**——
/// 质控办看到两条一模一样的审查记录只会问「到底审了几次」，答不上来比没有更糟。
fn record_review_to_store(
    db: &str,
    actor: Option<&str>,
    result: &psychdoc_rules::ReviewResult,
    diagnostics: &[psychdoc_model::Diagnostic],
    suppressed: &[psychdoc_model::Diagnostic],
    rule_sources: &[(String, String)],
    at: &str,
) -> Result<String, String> {
    // 一个包的件可能跨住院（导出目录），按别名各自成批；单份包则只有一批。
    // 压下的命中也要参与别名判定：否则一次「全是折叠命中」的审查
    // 会落到 `<无诊断，按包记录>` 这个假别名上，导出时查无此批。
    let mut aliases: Vec<String> = Vec::new();
    for d in diagnostics.iter().chain(suppressed.iter()) {
        let alias = d
            .loc
            .doc_id
            .split('-')
            .take(3)
            .collect::<Vec<_>>()
            .join("-");
        if !aliases.contains(&alias) {
            aliases.push(alias);
        }
    }
    if aliases.is_empty() {
        aliases.push("<无诊断，按包记录>".to_string());
    }

    let store = if db == "default" {
        psychdoc_store::Store::open_default().map_err(|e| e.to_string())?
    } else {
        psychdoc_store::Store::open(db).map_err(|e| e.to_string())?
    };
    let at = at.to_string();
    let actor = actor.unwrap_or("CLI 未署名操作人");
    use sha2::Digest;
    let mut last = String::new();
    for alias in aliases {
        let owned: Vec<psychdoc_model::Diagnostic> = diagnostics
            .iter()
            .filter(|d| d.loc.doc_id.starts_with(&alias))
            .cloned()
            .collect();
        let owned_suppressed: Vec<psychdoc_model::Diagnostic> = suppressed
            .iter()
            .filter(|d| d.loc.doc_id.starts_with(&alias))
            .cloned()
            .collect();

        // 折叠命中的码也进批次键：压下与留下是**同一个审查事实的两面**，
        // 键若只算留下那面，改了折叠口径后重跑会被幂等吞掉，
        // 库里留下的是旧口径的「压下了什么」——那与证据包的用途相反。
        let mut hasher = sha2::Sha256::new();
        hasher.update(alias.as_bytes());
        hasher.update(b"\x1f");
        hasher.update(result.ruleset_sha256.as_bytes());
        hasher.update(b"\x1f");
        for d in owned.iter().chain(owned_suppressed.iter()) {
            hasher.update(d.code.as_bytes());
            hasher.update(b"\x1e");
            hasher.update(d.loc.char_range.to_string().as_bytes());
        }
        let run_id = format!("run-{}", &format!("{:x}", hasher.finalize())[..12]);

        // 幂等：同一批已落过就跳过，不追加第二条审查事实。
        let existing: usize = store
            .conn()
            .query_row(
                "SELECT COUNT(*) FROM review_runs WHERE run_id = ?1",
                [&run_id],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if existing > 0 {
            last = run_id;
            continue;
        }

        store
            .register_items(&[], &at)
            .map_err(|e| e.to_string())?;
        store
            .record_review(&psychdoc_store::ReviewRunInput {
                run_id: &run_id,
                encounter_alias: &alias,
                ruleset_version: &result.ruleset_version,
                ruleset_sha256: &result.ruleset_sha256,
                actor,
                started_at: &at,
                finished_at: Some(&at),
                items_total: result.items_reviewed,
                items_reviewed: result.items_reviewed,
                items_skipped: 0,
                unresolved_count: result.parse_issues.len(),
                waived_count: result.waived_count,
                diagnostics: &owned,
                suppressed: &owned_suppressed,
                rule_sources,
            })
            .map_err(|e| e.to_string())?;
        last = run_id;
    }
    Ok(last)
}

/// `check --json` 的机器可读正文。
///
/// 字段选择刻意对齐「CI 要判的三件事」：有没有阻断（`blocking_count`）、
/// 命中了什么（`diagnostics`，带码 + 段落定位 + 依据）、
/// 这份结论是在哪一版规则下产生的（`ruleset.sha256`）。
/// 少给一项，CI 就得去解析人类文字——那正是 `--json` 要消灭的事。
/// 装配一次审查的结构化运行摘要。
///
/// `items_total` 取**进到规则层的件 + 被过滤的件**，而不是
/// 「规则层说审了几件」：后者会让「一半的件没解析出来」这件事在摘要里
/// 完全看不见，而那正是最该被追问的情形。
fn build_run_summary(
    items: &[psychdoc_model::DocItem],
    filtered: usize,
    result: &psychdoc_rules::ReviewResult,
) -> psychdoc_store::RunSummary {
    let mut s = psychdoc_store::RunSummary::new(
        items.len() + filtered,
        &result.ruleset_version,
        &psychdoc_store_short_sha(&result.ruleset_sha256),
    );
    s.items_reviewed = result.items_reviewed;
    s.findings = result.diagnostics.len();
    s.blocking = result.blocking_count();
    s.waived = result.waived_count;
    // 「未判定」取三类互不重叠的未决记录之和：跨文书跳过的检查、
    // 豁免判不出来而没放行的、以及因同名字段歧义未判定的字段。
    // 三者都不进阻断统计，但都要能按住院别名回查。
    s.undecided = result.skipped_checks.len() + result.waiver_skipped.len();
    // 解析期问题单列：那些件**已经被读进来判过**（进的是 `items_reviewed`），
    // 再往 `unresolved` 里加一遍会让「已审 + 未解析」大于输入件数——
    // 账假平正是这条不变量要拦的那类失败。
    s.parse_issues = result.parse_issues.len();
    s.unresolved = filtered;
    s
}

/// 64 位哈希截成 12 位短哈希（与底栏、Provenance 用的是同一口径）。
fn psychdoc_store_short_sha(full: &str) -> String {
    full.chars().take(12).collect()
}

/// 把运行摘要与逐条命中写进用户数据目录下的结构化日志。
///
/// 写不进去**不报错**：审查结论已经算出来了，因为日志盘满了就把整次
/// 审查丢掉是不可接受的反向失败。
///
/// 摘要**只写一次**：`RunSummary::append_to` 与这里的逐条命中共用同一个
/// 日志器，读日志的人才不会在同一天里看到两条口径相同、内容却略有差别的摘要。
fn emit_run_log(
    summary: &mut psychdoc_store::RunSummary,
    result: &psychdoc_rules::ReviewResult,
    elapsed_ms: u64,
) {
    summary.elapsed_ms = elapsed_ms;
    let Some(dir) = psychdoc_store::log_dir().ok() else {
        return;
    };
    let ts = chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false);
    if !summary.append_to(&dir, &ts) {
        return;
    }
    let Some(mut log) = psychdoc_store::RunLogger::new(&dir).ok() else {
        return;
    };
    // 每条命中也各记一行：护士长问「3 床那条为什么报了」时，
    // 一行一条比在整段自由文本里 grep 靠得住得多。
    for d in result.diagnostics.iter().chain(result.parse_issues.iter()) {
        log.write(
            psychdoc_store::event(&ts, psychdoc_store::Level::Warn, "diagnostic")
                .doc(&d.loc.doc_id)
                .code(&d.code)
                .detail("段落", format!("#s{}", d.loc.section_index))
                .detail("问题", &d.message.text)
                .detail("依据", &d.message.basis),
        );
    }
    // 报了什么码各多少次：一份审计包最常被问的三件事之一。
    let mut tally: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
    for d in result.diagnostics.iter() {
        *tally.entry(d.code.as_str()).or_default() += 1;
    }
    for (code, n) in tally {
        log.write(
            psychdoc_store::event(&ts, psychdoc_store::Level::Info, "rule.tally")
                .code(code)
                .detail("命中条数", n.to_string()),
        );
    }
    // `silently_unaccounted` 非零是一条**告警**：有一批件既没被审，
    // 也没被记成读不动。它不该阻断提交（那会把工具自己漏算当成文书问题），
    // 但必须留在日志里等人看见。
    let lost = summary.silently_unaccounted();
    if lost > 0 {
        sink::say(&format!(
            "⚠ 有 {lost} 件既未进入规则层、也未记入未解析清单——请核对导出包。"
        ));
        log.write(
            psychdoc_store::event(&ts, psychdoc_store::Level::Warn, "run.unbalanced")
                .detail("去向不明的件数", lost.to_string()),
        );
    }
    summary.redacted_hits += log.redacted_hits();
    sink::say(&format!(
        "运行日志：{}（本次事件 {} 条 · 脱敏替换 {} 处）",
        summary.log_file,
        log.written() + 1,
        summary.redacted_hits,
    ));
}

fn emit_check_json(
    input: &str,
    ruleset_path: &str,
    result: &psychdoc_rules::ReviewResult,
    summary_line: Option<&str>,
    run_summary: Option<&psychdoc_store::RunSummary>,
) {
    use serde_json::json;
    let diagnostics: Vec<serde_json::Value> = result
        .diagnostics
        .iter()
        .map(|d| {
            json!({
                "code": d.code,
                "severity": format!("{:?}", d.severity).to_lowercase(),
                "doc_id": d.loc.doc_id,
                "section_index": d.loc.section_index,
                "char_start": d.loc.char_range.start,
                "char_end": d.loc.char_range.end,
                "message": d.message.text,
                "basis": d.message.basis,
                "fix_availability": format!("{:?}", d.fix_availability).to_lowercase(),
                "suppressed_by": d.suppressed_by,
                "waived": d.waived,
            })
        })
        .collect();
    let parse_issues: Vec<serde_json::Value> = result
        .parse_issues
        .iter()
        .map(|d| {
            json!({
                "code": d.code,
                "doc_id": d.loc.doc_id,
                "section_index": d.loc.section_index,
                "char_start": d.loc.char_range.start,
                "char_end": d.loc.char_range.end,
                "message": d.message.text,
                "basis": d.message.basis,
            })
        })
        .collect();
    let exit_code = if result.blocking_count() > 0 {
        EXIT_FINDINGS
    } else {
        EXIT_OK
    };
    let body = json!({
        "ok": exit_code == EXIT_OK,
        "subcommand": "check",
        "input": input,
        "ruleset": {
            "path": ruleset_path,
            "version": result.ruleset_version,
            "sha256": result.ruleset_sha256,
        },
        "items_reviewed": result.items_reviewed,
        "blocking_count": result.blocking_count(),
        "notice_count": result.diagnostics.len() - result.blocking_count(),
        "waived_count": result.waived_count,
        "suppressed_count": result.suppressed.len(),
        "diagnostics": diagnostics,
        "parse_issues": parse_issues,
        // 「未判定」与「容差内」也进 JSON：它们不进 blocking，
        // 但 CI / 报表常要回答「这一条为什么没报」。
        "skipped_checks": result.skipped_checks.iter().map(|s| s.as_zh()).collect::<Vec<_>>(),
        "tolerated_checks": result.tolerated_checks.iter().map(|t| t.as_zh()).collect::<Vec<_>>(),
        "run_summary": summary_line,
        // 结构化运行摘要：CI 可以直接 jq 这份「这次到底发生了什么」，
        // 不必从 diagnostics 反推「一共看了几件、有没有件没被看到」。
        "run": run_summary,
        "exit_code": exit_code,
        "exit_meaning": contract::describe(exit_code),
    });
    sink::emit_json(
        &serde_json::to_string_pretty(&body)
            .unwrap_or_else(|e| format!("{{\"ok\":false,\"error\":\"JSON 序列化失败：{e}\"}}")),
    );
}

fn print_report(
    input: &str,
    ruleset_path: &str,
    ruleset: &psychdoc_rules::EffectiveRules,
    result: &psychdoc_rules::ReviewResult,
    parse_issues: &[psychdoc_model::Diagnostic],
    summary_line: Option<&str>,
) {
    sink::say(&format!(
        "文书包：{input}    规则集：{ruleset_path}（版本 {} · sha256 {}）",
        result.ruleset_version,
        ruleset.short_sha256()
    ));
    sink::say(&format!("已审文书 {} 件", result.items_reviewed));
    sink::say("");

    if result.diagnostics.is_empty() {
        sink::say("未发现规则命中项。");
    } else {
        sink::say(&format!("问题清单（{} 条）：", result.diagnostics.len()));
        for d in &result.diagnostics {
            let fix = match d.fix_availability {
                psychdoc_model::FixAvailability::Automatic => "可预览补写",
                psychdoc_model::FixAvailability::Manual => "需人工确认后补写",
                psychdoc_model::FixAvailability::None => "不可自动修",
            };
            let severity = match d.severity {
                psychdoc_model::Severity::Blocking => "阻断",
                psychdoc_model::Severity::Notice => "提示",
            };
            // 「本条命中来自哪一层」：三级级联覆盖后仍要能回答质控办的追问。
            let provenance = ruleset.rule(&d.code).map(|r| r.provenance()).unwrap_or_default();
            sink::say(&format!("  [{severity}] {} · {} · {}", d.code, d.loc, fix));
            sink::say(&format!("        {}", d.message.render()));
            if !provenance.is_empty() {
                sink::say(&format!("        规则来源：{provenance}"));
            }
        }
    }

    if !parse_issues.is_empty() {
        sink::say("");
        sink::say(&format!("解析期问题（{} 条，读不动或需人工核的部分）：", parse_issues.len()));
        for d in parse_issues {
            sink::say(&format!("  [解析] {} · {}", d.code, d.loc));
            sink::say(&format!("        {}", d.message.render()));
        }
    }

    // 「此处另有 N 条更概括的命中」：折叠是为了不让同一条整改刷屏，
    // 但压下去的那条**必须留痕**——护士长追问「为什么只报一条」时，
    // 答案在这里；不打印就等于让工具悄悄吞掉了一条命中。
    if !result.suppressed.is_empty() {
        sink::say("");
        sink::say(&format!("同处另有更概括的命中（{} 条，已折叠，展开可见）：", result.suppressed.len()));
        for s in &result.suppressed {
            sink::say(&format!("  [已折叠] {}", s.as_zh()));
        }
    }

    // 「未判定」不是「通过」：跳过的跨文书检查必须显式列出来，
    // 否则护士长看到清单空了就以为全部合规，而实际上有件没读出时刻。
    // 底栏：增量模式给出重跑 / 跳过件数，供信息科核对「这次到底审了多少」。
    if let Some(line) = summary_line {
        sink::say("");
        sink::say(line);
    }

    if !result.skipped_checks.is_empty() {
        sink::say("");
        sink::say(&format!("未判定的跨文书检查（{} 条，需人工核）：", result.skipped_checks.len()));
        for s in &result.skipped_checks {
            sink::say(&format!("  [未判定] {}", s.as_zh()));
        }
    }

    // 「容差内」与「未判定」是两件事，必须分开列：
    // 前者是**判出来了、结论是不算违规**（规则集里有一条写明理由的容差），
    // 后者是**判不了**。混成一段，护士长问「这一条为什么没报」就答不出来。
    if !result.tolerated_checks.is_empty() {
        sink::say("");
        sink::say(&format!("落在并列容差内、未判违规（{} 条）：", result.tolerated_checks.len()));
        for t in &result.tolerated_checks {
            sink::say(&format!("  [容差内] {} · {}", t.as_zh(), t.detail));
        }
    }

    // 事件时间线：把一次住院的几件按业务时刻排开。跨文书时序规则的判定底座
    // 就是它，因此现场追问「这一天的顺序到底对不对」时可以直接看这一段。
    if !result.timelines.is_empty() {
        sink::say("");
        for tl in &result.timelines {
            sink::say(&format!("事件时间线（{}，{} 条事件）：", tl.encounter_alias, tl.events.len()));
            for row in tl.as_zh_rows() {
                sink::say(&format!("  {row}"));
            }
        }
    }
}

/// 规则回归：把 `rulesets/rule-regress.yaml` 里的每条用例跑一遍，
/// 逐条报通过 / 失败，并输出按规则码统计的覆盖矩阵。
///
/// 退出码复用 `check` 的契约：**审查不通过 = 1**，因此 CI 里
/// 「跑回归」与「跑审查」可以用同一条判定线。
fn rule_regress(args: &[String]) -> Result<u8, CliError> {
    let mut cases_path: Option<String> = None;
    let mut ruleset_path: Option<String> = None;
    let mut root: Option<String> = None;
    let mut only: Option<String> = None;
    let mut json_out: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let key = match args[i].as_str() {
            "--cases" | "--ruleset" | "--root" | "--only" | "--json" => args[i].clone(),
            other => return Err(CliError::usage(format!("rule-regress 不支持的选项 {other}\n\n{}", usage()))),
        };
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{key} 后面需要取值\n\n{}", usage()))?
            .clone();
        match key.as_str() {
            "--cases" => cases_path = Some(value),
            "--ruleset" => ruleset_path = Some(value),
            "--root" => root = Some(value),
            "--only" => only = Some(value),
            "--json" => json_out = Some(value),
            _ => unreachable!(),
        }
        i += 2;
    }

    // 默认用例集可能在 <root>/rulesets/ 下，而人往往在 core/ 或更深处敲命令。
    // 依次试当前目录与 core 的上一级，省得每次都记着带路径。
    let cases_path = match cases_path {
        Some(p) => p,
        None => ["rulesets/rule-regress.yaml", "../rulesets/rule-regress.yaml"]
            .into_iter()
            .find(|p| std::path::Path::new(p).exists())
            .unwrap_or("rulesets/rule-regress.yaml")
            .to_string()
            .to_string(),
    };
    let cases_file = std::path::Path::new(&cases_path);
    if !cases_file.exists() {
        return Err(CliError::usage(format!("找不到用例集 {cases_path}")));
    }
    let root = root.unwrap_or_else(|| {
        // 用例集放在 <root>/rulesets/ 下时，root 是它的上两级；
        // 放在别处（如 /tmp 下的临时用例集）就直接把它所在目录当 root，
        // 免得推算出个根本不存在的路径。
        cases_file
            .parent()
            .filter(|p| p.file_name().is_some_and(|n| n == "rulesets"))
            .and_then(std::path::Path::parent)
            .or_else(|| cases_file.parent())
            .unwrap_or(std::path::Path::new("."))
            .to_string_lossy()
            .into_owned()
    });

    let ruleset_path =
        ruleset_path.unwrap_or_else(|| format!("{}/rulesets/hospital.yaml", root));
    let yaml = std::fs::read_to_string(&ruleset_path)
        .map_err(|e| format!("读不到规则集 {ruleset_path} — {e}"))?;
    // 坏规则集在回归里必须当场失败，不能降级成「无规则 = 全绿」。
    let ruleset = load_effective(&yaml).map_err(CliError::from_display)?;
    let registry = ruleset.registry();

    let case_yaml = std::fs::read_to_string(cases_file)
        .map_err(|e| format!("读不到用例集 {cases_path} — {e}"))?;
    let file = psychdoc_rules::load_cases(&case_yaml).map_err(CliError::from_display)?;

    // 解析走 psychdoc-parse，判定走 psychdoc-rules，与 GUI / CLI check 同一套。
    let root_path = std::path::Path::new(&root);
    let report = psychdoc_rules::run_all(&file, &ruleset, only.as_deref(), |given| {
        let path = psychdoc_rules::fixture_path(root_path, given);
        let json = std::fs::read_to_string(&path).map_err(|e| psychdoc_rules::CaseError {
            code: "E_REGRESS_FIXTURE_MISSING",
            detail: format!("用例引用的夹具读不到：{} — {e}", path.display()),
        })?;
        let parsed = psychdoc_parse::parse_package_with(&json, Some(&registry))
            .map_err(|e| psychdoc_rules::CaseError {
                code: "E_REGRESS_FIXTURE_UNPARSABLE",
                detail: format!("夹具 {} 解析失败：{e}", path.display()),
            })?;
        // 切片留在这里做：解析层不认识用例，只认识件。
        Ok(given.slice_items(parsed.items))
    })
    .map_err(CliError::from_display)?;

    print_regress_report(&cases_path, &report);

    if let Some(out) = json_out {
        let json = serde_json::to_string_pretty(&report)
            .map_err(|e| format!("回归报告序列化失败 — {e}"))?;
        std::fs::write(&out, json).map_err(|e| format!("写不进 {out} — {e}"))?;
        sink::say("");
        sink::say(&format!("机器可读报告已写到 {out}"));
    }

    Ok(if report.is_green() {
        EXIT_OK
    } else {
        EXIT_FINDINGS
    })
}

/// 审计导出：把本机库里已落盘的审查 / 写回摊成**第三方可核对**的证据行。
///
/// 与 `check` 的分工：`check` 回答「现在这份包有没有问题」，本命令回答
/// 「这份病历当时是按哪一版规则、谁在什么时候审的、命中了哪些码」——
/// 质控办月度抽查带走的是后者。
///
/// 退出码复用同一套契约：导出成功 0；用法 / 配置错 2。**导出行数为 0 不算错**
/// （当月真没审过就是没有），但要在输出里说清楚，不能让人以为导出坏了。
/// `psychdoc codes [--format markdown|json|text] [--code <错误码>] [--out <路径>]`。
///
/// **表由代码生成，不是一份手抄的文档**。手抄的那份会在代码加了码之后
/// 悄悄过期，而过期的错误码表比没有更糟——值班的人会照着一张缺了三行的
/// 表判断「这个码不在我们这儿」，于是去查错方向。
/// 反向由 `psychdoc-store/tests/observability.rs` 双向钉死。
fn codes(args: &[String], json_mode: bool) -> Result<u8, CliError> {
    use psychdoc_model::errcodes;

    let mut format = if json_mode { "json" } else { "text" }.to_string();
    let mut only: Option<String> = None;
    let mut out: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        let key = args[i].clone();
        match key.as_str() {
            "--format" | "--code" | "--out" => {
                let v = args
                    .get(i + 1)
                    .ok_or_else(|| CliError::usage(format!("{key} 后面需要取值")))?
                    .clone();
                match key.as_str() {
                    "--format" => format = v,
                    "--code" => only = Some(v),
                    _ => out = Some(v),
                }
                i += 2;
            }
            other => {
                return Err(CliError::usage(format!("codes 不支持的选项 {other}")));
            }
        }
    }
    if !["markdown", "json", "text"].contains(&format.as_str()) {
        return Err(CliError::usage(format!(
            "codes --format 只认 markdown|json|text，收到 {format}"
        )));
    }

    // 只看一条时，查不到就是查不到——不拿近似的那一条糊过去。
    let rows: Vec<psychdoc_model::ErrorCode> = match &only {
        Some(code) => match errcodes::lookup(code) {
            Some(found) => vec![*found],
            None => {
                return Err(CliError::usage(format!(
                    "{code} 不在本产品的错误码表里：{}",
                    errcodes::describe(code)
                )))
            }
        },
        None => errcodes::ERROR_CODES.to_vec(),
    };

    let body = match (format.as_str(), &only) {
        ("markdown", None) => errcodes::markdown_table(),
        (_, Some(_)) => rows
            .iter()
            .map(|c| {
                format!(
                    "{} · {} · {}\n  触发：{}\n  处置：{}",
                    c.code,
                    c.side.as_zh(),
                    if c.retryable {
                        "可重试"
                    } else {
                        "不可重试"
                    },
                    c.trigger,
                    c.remedy
                )
            })
            .collect::<Vec<_>>()
            .join("\n"),
        ("json", None) => {
            let items: Vec<serde_json::Value> = rows
                .iter()
                .map(|c| {
                    serde_json::json!({
                        "code": c.code,
                        "side": c.side.as_zh(),
                        "retryable": c.retryable,
                        "trigger": c.trigger,
                        "remedy": c.remedy,
                        "origin": c.origin,
                    })
                })
                .collect();
            serde_json::to_string_pretty(&serde_json::json!({
                "ok": true,
                "subcommand": "codes",
                "count": items.len(),
                "codes": items,
            }))
            .unwrap_or_default()
        }
        _ => {
            let mut s = String::new();
            for c in &rows {
                s.push_str(&format!(
                    "{:<28} {:<10} {:<8} {}\n",
                    c.code,
                    c.side.as_zh(),
                    if c.retryable { "可重试" } else { "不可重试" },
                    c.trigger
                ));
            }
            s.push_str(&format!("\n共 {} 条。带 --code <错误码> 看单条的处置建议。\n", rows.len()));
            s
        }
    };

    match &out {
        Some(path) => {
            std::fs::write(path, &body).map_err(|e| {
                CliError::new("E_IO", format!("写不进 {path} — {e}"))
            })?;
            sink::say(&format!("错误码表已写入 {path}（{} 条）", rows.len()));
        }
        None if format == "json" || json_mode => sink::emit_json(&body),
        None => print!("{body}"),
    }
    Ok(EXIT_OK)
}

fn export_audit(args: &[String]) -> Result<u8, CliError> {
    let mut q = psychdoc_store::ExportQuery::default();
    let mut db: Option<String> = None;
    let mut format = "csv".to_string();
    let mut out_path: Option<String> = None;

    let mut i = 0;
    while i < args.len() {
        let key = args[i].clone();
        let takes_value = matches!(
            key.as_str(),
            "--db" | "--alias" | "--run" | "--since" | "--until" | "--at" | "--format" | "--out"
        );
        if !takes_value {
            return Err(CliError::usage(format!("export-audit 不支持的选项 {key}\n\n{}", usage())));
        }
        let value = args
            .get(i + 1)
            .ok_or_else(|| format!("{key} 后面需要取值\n\n{}", usage()))?
            .clone();
        match key.as_str() {
            "--db" => db = Some(value),
            "--alias" => q.encounter_alias = Some(value),
            "--run" => q.run_id = Some(value),
            "--since" => q.since = Some(value),
            "--until" => q.until = Some(value),
            "--at" => q.recall_at = Some(value),
            "--format" => {
                if value != "csv" && value != "json" {
                    return Err(CliError::usage(format!("--format 只收 csv / json，收到 {value}")));
                }
                format = value;
            }
            "--out" => out_path = Some(value),
            _ => unreachable!(),
        }
        i += 2;
    }

    // `--at` 是「回溯到那一刻」，必须同时知道查哪份住院；只给时刻不给别名
    // 会导出全库后仍答不出「那一刻用哪版规则」，是个看着能跑其实答非所问的组合。
    if q.recall_at.is_some() && q.encounter_alias.is_none() {
        return Err(CliError::usage(
            "export-audit --at 需要同时带 --alias <住院别名>（要回溯的是那一份住院）\n\n".to_string()
                + &usage(),
        ));
    }

    // 库落用户数据目录（不传 --db 时），与桌面端读的是同一份——
    // 「界面上看到的」和「导出的」必须是同一个事实来源。
    let store = match db {
        Some(p) => psychdoc_store::Store::open(p).map_err(CliError::from_display)?,
        None => psychdoc_store::Store::open_default().map_err(CliError::from_display)?,
    };
    let bundle = store.export_audit(&q).map_err(CliError::from_display)?;

    let body = if format == "json" {
        psychdoc_store::to_json(&bundle)
    } else {
        psychdoc_store::to_csv(&bundle)
    };

    print_export_summary(&bundle, &format, &q);

    match out_path {
        Some(p) => {
            std::fs::write(&p, &body).map_err(|e| format!("写不进 {p} — {e}"))?;
            sink::say("");
            sink::say(&format!("审计导出包已写到 {p}（{format} · {} 行 · {} 批次）", bundle.row_count(), bundle.run_count()));
            if format == "csv" {
                sink::say("csv 带 UTF-8 BOM，院内 Excel 双击打开不乱码。");
            }
            // 归档核齐视图单独一份：列结构与证据行完全不同，合成一张表
            // 只会得到一堆空格。质控办要带走的本来就是两份材料。
            if !bundle.archive_views.is_empty() {
                let ap = archive_sibling_path(&p);
                let abody = psychdoc_store::export::archive_to_csv(&bundle);
                std::fs::write(&ap, &abody).map_err(|e| format!("写不进 {ap} — {e}"))?;
                sink::say(&format!("归档核齐视图已写到 {ap}（{} 份 · 同样带 UTF-8 BOM）。", bundle.archive_views.len()));
            }
        }
        None => {
            // 直接打到 stdout：运维把这条命令接进定时导出、或临时看一眼都方便。
            // csv 前导的 BOM 会混进重定向，但这是 Excel 侧的刻意选择——
            // 落盘文件才是交付物，stdout 只是「先看一眼」。
            if format == "csv" {
                sink::say_err("（以下为 csv 正文，带 UTF-8 BOM；要成文件请加 --out <路径>）");
                sink::say(body.trim_start_matches('\u{FEFF}'));
            } else {
                sink::say(&body);
            }
        }
    }
    Ok(EXIT_OK)
}

/// 归档视图 csv 的落位：证据行同一目录下，加 `-归档核齐` 后缀。
///
/// 不放进子目录：质控员在文件管理器里一眼看见两个文件、并排打开，
/// 是最省事的交付形态。藏在 `archive/` 里反而要他们多点一次。
fn archive_sibling_path(evidence_path: &str) -> String {
    match evidence_path.rfind('.') {
        Some(i) if !evidence_path[i..].contains('/') => {
            format!("{}-归档核齐{}", &evidence_path[..i], &evidence_path[i..])
        }
        _ => format!("{evidence_path}-归档核齐.csv"),
    }
}

fn print_export_summary(
    bundle: &psychdoc_store::AuditBundle,
    format: &str,
    q: &psychdoc_store::ExportQuery,
) {
    sink::say(&format!(
        "审计导出：{} · 批次 {} 次 · 证据行 {} 条（报出 {} · 折叠 {}）",
        format,
        bundle.run_count(),
        bundle.row_count(),
        bundle.reported_count(),
        bundle.suppressed_count()
    ));
    if !bundle.is_disposition_accounting_balanced() {
        // 这条恒等式破了 = 去重层吞掉了条目。导出照发，但必须喊出来：
        // 质控办拿到一份对不上账的证据包，比没拿到更危险。
        sink::say("⚠ 处置计数对不上：报出 + 折叠 != 证据行总数，导出包里可能有条目丢失。");
    }
    sink::say(&format!(
        "筛选：住院别名 {} · 批次 {} · 时间段 {} ~ {}",
        q.encounter_alias.as_deref().unwrap_or("全部"),
        q.run_id.as_deref().unwrap_or("全部"),
        q.since.as_deref().unwrap_or("不限"),
        q.until.as_deref().unwrap_or("不限")
    ));

    for run in &bundle.runs {
        // 折叠的码单独列：清单上「只报一条」时，答案就在这一行。
        let folded = if run.suppressed_codes.is_empty() {
            String::new()
        } else {
            format!(" · 折叠码 [{}]", run.suppressed_codes.join("、"))
        };
        sink::say(&format!(
            "  · {} · {} · 规则集 {}（sha256 {}）· {} 于 {} · 审 {} 件（跳过 {}）· 命中码 [{}]{folded} · 本次导出 {} 行（报出 {} · 折叠 {}）",
            run.review_run_id,
            run.encounter_alias,
            run.ruleset_version,
            &run.ruleset_sha256[..run.ruleset_sha256.len().min(12)],
            run.actor,
            run.started_at,
            run.items_reviewed,
            run.items_skipped,
            run.codes.join("、"),
            run.row_count,
            run.reported_count,
            run.suppressed_count,
        ));
    }

    // 二次脱敏的结果必须报出来：质控办要能回答「这份导出里有没有过未脱敏字段」，
    // 答「没有」得有凭据，而不是靠导出方口头担保。
    if bundle.redaction.replaced_fields > 0 {
        sink::say("");
        sink::say(&format!(
            "导出期二次脱敏：替换自由文本字段 {} 处（{}）",
            bundle.redaction.replaced_fields,
            bundle.redaction.replaced_at.join("、")
        ));
    } else {
        sink::say("");
        sink::say("导出期二次脱敏：未发现未脱敏内容（标识列与自由文本列均已扫过）。");
    }

    // 版本回溯：质控办最常问的那句「这份当年按哪版审的」。
    if let Some(recalled) = &bundle.recalled {
        sink::say("");
        sink::say(&format!(
            "版本回溯（{}）：批次 {} · 规则集 {}（sha256 {}）· 完成于 {}",
            q.recall_at.as_deref().unwrap_or("-"),
            recalled.run_id,
            recalled.ruleset_version,
            recalled.ruleset_sha256,
            recalled.finished_at.as_deref().unwrap_or("未完成"),
        ));
        sink::say(&format!(
            "  当时报出的码：{}",
            if recalled.codes.is_empty() {
                "无（该次审查零命中）".to_string()
            } else {
                recalled.codes.join("、")
            }
        ));
        if !recalled.suppressed_codes.is_empty() {
            sink::say(&format!(
                "  当时折叠的码：{}（清单上未显示，被更具体的规则覆盖）",
                recalled.suppressed_codes.join("、")
            ));
        }
    } else if q.recall_at.is_some() {
        sink::say("");
        sink::say(&format!(
            "版本回溯（{}）：该时刻之前 {} 没有任何审查记录，无从回溯（不是返回「最新一版」）",
            q.recall_at.as_deref().unwrap_or("-"),
            q.encounter_alias.as_deref().unwrap_or("-")
        ));
    }

    if bundle.row_count() == 0 {
        sink::say("");
        sink::say("本次导出 0 行：该范围内没有命中记录（或尚未做过审查），不是导出失败。");
    }
}

fn print_regress_report(cases_path: &str, report: &psychdoc_rules::RunReport) {
    sink::say(&format!(
        "规则回归：{cases_path}    规则集版本 {}（sha256 {}）",
        report.ruleset_version, report.ruleset_sha256
    ));
    sink::say(&format!(
        "用例 {} 条 · 通过 {} · 失败 {}",
        report.outcomes.len(),
        report.passed(),
        report.failed()
    ));
    sink::say("");

    for o in &report.outcomes {
        if o.passed {
            sink::say(&format!("  [通过] {} · {} · {}", o.id, o.code, o.kind.as_zh()));
        } else {
            sink::say(&format!("  [失败] {} · {} · {}", o.id, o.code, o.kind.as_zh()));
            for f in &o.failures {
                sink::say(&format!("         {f}"));
            }
        }
    }

    // 覆盖矩阵：用例全绿不等于回归完整 —— 删掉一条反例，所有用例照样全绿，
    // 但「这条规则会不会乱报」从此没人验证。缺位必须显式报出来。
    sink::say("");
    sink::say("规则码覆盖矩阵（正例 / 反例 / 边界例）：");
    for row in &report.coverage {
        let mark = if row.is_covered() { "齐" } else { "缺" };
        sink::say(&format!(
            "  [{mark}] {} · {} · 正例 {} · 反例 {} · 边界例 {}",
            row.code, row.title, row.positive, row.negative, row.boundary
        ));
    }
    let ratio = psychdoc_rules::coverage_ratio(&report.coverage);
    sink::say(&format!("覆盖率 {:.0}%", ratio * 100.0));
    for row in report.coverage.iter().filter(|r| !r.is_covered()) {
        let mut missing = Vec::new();
        if row.positive == 0 {
            missing.push("正例");
        }
        if row.negative == 0 {
            missing.push("反例");
        }
        if row.boundary == 0 {
            missing.push("边界例");
        }
        sink::say(&format!(
            "  ！{} 缺 {}：没写这类用例，就没人验证它{}",
            row.code,
            missing.join("、"),
            if missing.contains(&"反例") {
                "会不会乱报"
            } else {
                "边界上会怎么判"
            }
        ));
    }

    sink::say("");
    sink::say(&format!(
        "回归{}。",
        if report.is_green() {
            "全绿：规则集与用例集一致"
        } else {
            "未通过：按上面的失败明细改规则集（不要改用例去迁就它）"
        }
    ));
}
