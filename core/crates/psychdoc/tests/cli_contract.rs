//! 退出码契约测试：四个码各钉一个真实场景，**打的是编译出来的真二进制**。
//!
//! 为什么不直接调 `run(&args)`：退出码是本产品的**对外接口**，
//! CI 读的是 `process.exit_code`。函数返回值对得上而进程码对不上，
//! 契约就等于没钉。这里用 `env!("CARGO_BIN_EXE_psychdoc")` 拿真二进制，
//! 断言的是运维真正会看到的那一个数字。
//!
//! 四个场景各自对应一种「谁该被通知」：
//!
//! | 场景 | 退出码 | 谁该处理 |
//! |------|--------|----------|
//! | 合规文书包 | 0 | 无人，CI 通过 |
//! | 有阻断项的文书包 | 1 | 护士长 / 质控办（业务整改） |
//! | 参数写错 | 2 | 调脚本的人（改命令） |
//! | 端点不可达 | 3 | 信息科 / 网络组（退避重试） |

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 项目根（本 crate 在 `<root>/core/crates/psychdoc`，往上三层）。
fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(3)
        .expect("找不到项目根")
        .to_path_buf()
}

fn fixture(name: &str) -> String {
    project_root().join("fixtures").join(name).display().to_string()
}

fn ruleset() -> String {
    project_root()
        .join("rulesets")
        .join("hospital.yaml")
        .display()
        .to_string()
}

/// 跑一次真二进制。`env` 里的每一项都会**清空同名变量**后再设置，
/// 保证测试不受开发机上遗留的 `EMR_*` 影响。
fn run_cli(args: &[&str], env: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_psychdoc"));
    cmd.args(args);
    // 四项凭据默认一律清掉：没有它们时 `--live` 必须在 0 次网络调用下报 2。
    for k in [
        "EMR_BASE_URL",
        "EMR_CLIENT_ID",
        "EMR_CLIENT_SECRET",
        "EMR_TENANT_ID",
        "WECOM_WEBHOOK_URL",
    ] {
        cmd.env_remove(k);
    }
    for (k, v) in env {
        cmd.env(k, v);
    }
    cmd.output().expect("跑不起 psychdoc 二进制")
}

fn code(out: &Output) -> i32 {
    out.status.code().expect("进程被信号杀掉，没有退出码")
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).to_string()
}

fn both(out: &Output) -> String {
    format!("{}{}", stdout(out), stderr(out))
}

#[test]
fn exit_0_when_a_package_passes_review() {
    let out = run_cli(
        &[
            "check",
            &fixture("ward-09-field-compliant.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    let text = both(&out);
    assert_eq!(code(&out), 0, "合规包应退 0：\n{text}");
    assert!(text.contains("未发现规则命中项"), "{text}");
    // 「0 条」必须同时带上「看过几件」——否则 CI 分不清
    // 「全部合规」与「一件都没读到」。
    assert!(text.contains("已审文书 3 件"), "{text}");
}

#[test]
fn exit_1_when_a_blocking_finding_exists() {
    let out = run_cli(
        &[
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    let text = both(&out);
    assert_eq!(code(&out), 1, "有阻断项应退 1：\n{text}");
    assert!(text.contains("QC-PSY-01"), "{text}");
    // 阻断项必须带段落定位与依据，否则护士长拿着清单回去找不到地方改。
    assert!(text.contains("@[") && text.contains("依据"), "{text}");
}

#[test]
fn exit_2_on_a_usage_error_and_makes_no_network_call() {
    // 参数写错（少 --ruleset）→ 2。
    let out = run_cli(&["check", &fixture("ward-01-minimal.json")], &[]);
    let text = both(&out);
    assert_eq!(code(&out), 2, "缺参数应退 2：\n{text}");
    assert!(text.contains("E_USAGE"), "{text}");

    // `--live` 且四项凭据全缺 → 2，且一次网络调用都不该发生。
    // 「0 次外发」在无凭据情形下是结构性保证：凭据校验在构造 transport 之前。
    let live = run_cli(
        &["check", "--live", "WARD-A-001", "--ruleset", &ruleset()],
        &[],
    );
    let text = both(&live);
    assert_eq!(code(&live), 2, "缺凭据应退 2：\n{text}");
    assert!(text.contains("E_CFG_MISSING"), "{text}");
    // 一次列全，而不是报一个跑一次。
    for k in ["EMR_BASE_URL", "EMR_CLIENT_ID", "EMR_CLIENT_SECRET", "EMR_TENANT_ID"] {
        assert!(text.contains(k), "缺项清单里没有 {k}：\n{text}");
    }
    assert!(text.contains("不会降级"), "{text}");
}

#[test]
fn exit_3_when_the_endpoint_is_unreachable() {
    // 指向一个**保证没人监听**的回环端口：凭据齐备、形态合法，
    // 于是流程会真的走到网络层并失败——这正是要验的那条路径。
    let out = run_cli(
        &["check", "--live", "WARD-A-001", "--ruleset", &ruleset()],
        &[
            // 1..1024 是系统保留段，实际部署不会有服务监听 127.0.0.1:9。
            ("EMR_BASE_URL", "http://127.0.0.1:9/fhir/r4"),
            ("EMR_CLIENT_ID", "ci-runner"),
            ("EMR_CLIENT_SECRET", "ci-runner-secret"),
            ("EMR_TENANT_ID", "ward-a"),
        ],
    );
    let text = both(&out);
    assert_eq!(code(&out), 3, "端点不可达应退 3：\n{text}");
    assert!(
        text.contains("E_FHIR_UNREACHABLE") || text.contains("E_FHIR_STATUS"),
        "应带外部依赖侧错误码：\n{text}"
    );
    // 关键的一条：不可达**不能**降级成「没读到件 = 全部通过」。
    assert!(!text.contains("未发现规则命中项"), "{text}");
}

#[test]
fn exit_3_is_not_the_same_bucket_as_exit_1() {
    // 1 与 3 的分界：审查结论 vs 基础设施故障。两者文案不应互相冒名。
    let findings = run_cli(
        &[
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    let unreachable = run_cli(
        &["check", "--live", "WARD-A-001", "--ruleset", &ruleset()],
        &[
            ("EMR_BASE_URL", "http://127.0.0.1:9/fhir/r4"),
            ("EMR_CLIENT_ID", "ci-runner"),
            ("EMR_CLIENT_SECRET", "ci-runner-secret"),
            ("EMR_TENANT_ID", "ward-a"),
        ],
    );
    assert_eq!(code(&findings), 1);
    assert_eq!(code(&unreachable), 3);
    assert!(!both(&findings).contains("E_FHIR_UNREACHABLE"));
    assert!(!both(&unreachable).contains("QC-PSY-01"));
}

#[test]
fn graph_exit_1_when_the_encounter_is_not_archived_ready() {
    // 拿掉约束知情同意：约束记录与解除记录会被连带阻塞，归档核齐不通过。
    let dir = tempdir("graph-not-ready");
    let pkg = serde_json::json!({
        "resourceType": "Bundle",
        "type": "collection",
        "entry": [
            { "fullUrl": "urn:uuid:1", "resource": constraint_record() },
            { "fullUrl": "urn:uuid:2", "resource": restraint_release() },
        ]
    });
    let path = dir.join("pkg.json");
    std::fs::write(&path, serde_json::to_vec_pretty(&pkg).unwrap()).unwrap();

    let out = run_cli(
        &[
            "graph",
            "WARD-B-001",
            "--input",
            &path.display().to_string(),
            "--ruleset",
            &ruleset(),
            "--as-of",
            "2026-03-10T09:00:00+08:00",
        ],
        &[],
    );
    let text = both(&out);
    assert_eq!(code(&out), 1, "未核齐应退 1：\n{text}");
    assert!(text.contains("未核齐"), "{text}");
    assert!(text.contains("阻塞边"), "{text}");
    assert!(text.contains("连带阻塞"), "{text}");
}

#[test]
fn graph_exit_0_when_the_encounter_is_complete() {
    let out = run_cli(
        &[
            "graph",
            "WARD-B-001",
            "--input",
            &fixture("ward-02-restraint-set.json"),
            "--ruleset",
            &ruleset(),
            "--as-of",
            "2026-03-10T09:00:00+08:00",
        ],
        &[],
    );
    let text = both(&out);
    assert_eq!(code(&out), 0, "成套包应退 0：\n{text}");
    assert!(text.contains("已核齐"), "{text}");
}

#[test]
fn graph_forward_and_backward_answers_are_different_questions() {
    let run = |dir: &str| {
        let out = run_cli(
            &[
                "graph",
                "WARD-B-001",
                "--input",
                &fixture("ward-02-restraint-set.json"),
                "--ruleset",
                &ruleset(),
                "--as-of",
                "2026-03-10T09:00:00+08:00",
                "--direction",
                dir,
                "--json",
            ],
            &[],
        );
        assert_eq!(code(&out), 0, "{dir} 方向应退 0：\n{}", both(&out));
        serde_json::from_str::<serde_json::Value>(&stdout(&out)).expect("stdout 应是一段纯 JSON")
    };

    let deps = run("dependencies");
    let dependents = run("dependents");

    // 「正反向答案必须不同」是这条命令存在的理由：正问问「在等谁」，
    // 反问问「卡住谁」。两者若相同，说明方向参数没生效。
    assert_ne!(deps, dependents, "正反向 JSON 完全相同，--direction 没生效");

    let rows = deps["rows"].as_array().expect("rows 应是数组");
    let record = rows
        .iter()
        .find(|r| r["node_id"] == "WARD-B-001-RECORD-01")
        .expect("应能看到约束记录");
    // 正向：约束记录在等约束知情同意。
    assert_eq!(
        record["waiting_on"],
        serde_json::json!(["WARD-B-001-CONSENT-01"])
    );
    // 反向：约束知情同意卡住约束记录（看的是 consent 那行）。
    let consent = dependents["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["node_id"] == "WARD-B-001-CONSENT-01")
        .unwrap();
    assert_eq!(
        consent["blocking_others"],
        serde_json::json!(["WARD-B-001-RECORD-01"])
    );
}

#[test]
fn writeback_dryrun_exits_0_and_makes_zero_outbound_calls() {
    // 真实 `TcpListener` 数连接数：**任何**一次外发都会被数到，
    // 不靠「代码里看起来没有 send」这种自证。
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("起监听");
    let port = listener.local_addr().unwrap().port();
    listener
        .set_nonblocking(true)
        .expect("非阻塞，才能断言「一个连接都没有」");

    let dir = tempdir("dryrun-zero-egress");
    let sel = dir.join("selection.json");
    std::fs::write(&sel, selection_json()).unwrap();

    let out = run_cli(
        &[
            "writeback-dryrun",
            &sel.display().to_string(),
            "--out",
            &dir.join("bundle.json").display().to_string(),
        ],
        // 凭据**故意给全**：让「没发请求」不是因为发不出去，
        // 而是因为这条路径压根不碰网络。
        &[
            ("EMR_BASE_URL", &format!("http://127.0.0.1:{port}/fhir/r4")),
            ("EMR_CLIENT_ID", "ci-runner"),
            ("EMR_CLIENT_SECRET", "ci-runner-secret"),
            ("EMR_TENANT_ID", "ward-b"),
        ],
    );
    let text = both(&out);
    assert_eq!(code(&out), 0, "干跑成功应退 0：\n{text}");
    assert!(text.contains("未发起任何网络请求"), "{text}");

    // 数连接：一个都没有。
    assert!(
        matches!(
            listener.accept(),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock
        ),
        "干跑期间有连接打到了监听端口——这条路径不该碰网络"
    );

    // 报文形状仍要守：与真发同一个 build_transaction 出来的。
    let bundle: serde_json::Value = serde_json::from_slice(
        &std::fs::read(dir.join("bundle.json")).expect("Bundle 应已落盘"),
    )
    .unwrap();
    assert_eq!(bundle["resourceType"], "Bundle");
    assert_eq!(bundle["type"], "transaction");
    let entries = bundle["entry"].as_array().expect("entry 应是数组");
    assert_eq!(entries.len(), 3, "2 件 + 1 条 Provenance");
    for e in &entries[..2] {
        assert_eq!(e["request"]["method"], "POST");
        assert_eq!(e["request"]["url"], "Composition");
    }
    assert_eq!(entries[2]["resource"]["resourceType"], "Provenance");
    // 幂等键实算进 Provenance：同一天重复提交要靠它挡下。
    // 这也顺带证明干跑算的是**真**键（64 位十六进制），不是占位串。
    let prov = entries[2]["resource"].to_string();
    assert!(prov.contains("2abfac00aa5b"), "Provenance 没带规则集短哈希：{prov}");
}

#[test]
fn writeback_dryrun_refuses_an_empty_selection_with_the_config_exit_code() {
    // 空勾选是**用法问题**不是网络问题：报 3 会让 CI 陷入无意义重试。
    let dir = tempdir("dryrun-empty");
    let sel = dir.join("selection.json");
    let mut v: serde_json::Value = serde_json::from_str(&selection_json()).unwrap();
    v["entries"] = serde_json::json!([]);
    std::fs::write(&sel, serde_json::to_vec_pretty(&v).unwrap()).unwrap();

    let out = run_cli(&["writeback-dryrun", &sel.display().to_string()], &[]);
    let text = both(&out);
    assert_eq!(code(&out), 2, "空勾选应退 2：\n{text}");
    assert!(text.contains("E_FHIR_TXN_EMPTY"), "{text}");
}

#[test]
fn json_mode_puts_one_json_document_on_stdout_and_nothing_else() {
    let out = run_cli(
        &[
            "--json",
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    assert_eq!(code(&out), 1, "{}", both(&out));
    let body = stdout(&out);
    // stdout 上**至多一段** JSON：人类可读的过程文字一条都不许混进来。
    assert!(
        !body.contains("问题清单（"),
        "stdout 混进了人类可读文字：\n{body}"
    );
    let v: serde_json::Value = serde_json::from_str(&body).expect("stdout 应是纯 JSON");
    assert_eq!(v["ok"], false);
    assert_eq!(v["exit_code"], 1);
    assert_eq!(v["blocking_count"], 1);
    assert_eq!(v["subcommand"], "check");
    assert_eq!(v["diagnostics"][0]["code"], "QC-PSY-01");
    // 段落定位三件套 + 依据：CI 要能把它贴进工单。
    assert_eq!(v["diagnostics"][0]["doc_id"], "WARD-A-001-CONSENT-01");
    assert!(v["diagnostics"][0]["section_index"].is_number());
    assert!(v["diagnostics"][0]["char_end"].is_number());
    assert!(v["diagnostics"][0]["basis"].as_str().unwrap().contains("第 4.2 条"));
    assert!(!v["ruleset"]["sha256"].as_str().unwrap().is_empty());
}

#[test]
fn log_file_captures_the_process_and_json_mode_still_keeps_stdout_clean() {
    let dir = tempdir("log-file");
    let log = dir.join("run.log");
    let out = run_cli(
        &[
            "--json",
            "--log-file",
            &log.display().to_string(),
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    assert_eq!(code(&out), 1, "{}", both(&out));
    // stdout 仍然是纯 JSON。
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("stdout 应是纯 JSON");
    assert_eq!(v["subcommand"], "check");
    // 日志里能看到人类可读的过程。
    let logged = std::fs::read_to_string(&log).expect("日志应已落盘");
    assert!(logged.contains("问题清单"), "日志里没有过程文字：\n{logged}");
    assert!(logged.contains("QC-PSY-01"), "{logged}");
}

#[test]
fn the_four_documented_codes_match_what_the_binary_actually_returns() {
    // 契约文档（`docs/cli.md`）与真二进制的一致性由本条兜底：
    // 四个场景各跑一次，把实际退出码与 `contract::exit_for_code` 的语义对齐。
    let compliant = fixture("ward-09-field-compliant.json");
    let violating = fixture("ward-01-minimal.json");
    let rs = ruleset();
    let cases: Vec<(&str, Vec<&str>, i32)> = vec![
        (
            "合规包",
            vec!["check", compliant.as_str(), "--ruleset", rs.as_str()],
            0,
        ),
        (
            "有阻断项",
            vec!["check", violating.as_str(), "--ruleset", rs.as_str()],
            1,
        ),
        ("参数写错", vec!["check"], 2),
        (
            "勾选文件不存在",
            vec!["writeback-dryrun", "/nonexistent/selection.json"],
            2,
        ),
    ];
    for (name, args, want) in cases {
        let out = run_cli(&args, &[]);
        assert_eq!(code(&out), want, "{name} 应退 {want}：\n{}", both(&out));
    }
}

/// `codes` 把错误码表打成 markdown，供 README「错误与降级」一节引用。
///
/// 打的是真二进制而不是直接调函数：这张表会被抄进文档、被贴在工单上，
/// 抄错一行的代价是值班的人照着查错方向。
#[test]
fn codes_prints_a_table_someone_can_act_on() {
    let out = run_cli(&["codes", "--format", "markdown"], &[]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let body = stdout(&out);
    assert!(body.contains("| 错误码 | 处置侧 | 可重试 | 触发条件 | 处置建议 |"), "{body}");
    // 表要够厚才有排障价值：十行以下的表答不了「这个码归谁」。
    let rows = body.lines().filter(|l| l.starts_with("| `E_")).count();
    assert!(rows >= 10, "错误码表只有 {rows} 行：\n{body}");
    // 抽查两行：一行配置侧不可重试、一行外部依赖侧可重试——
    // 两类都在，才说明「谁该处理」这一栏真的有区分度。
    assert!(body.contains("| `E_CFG_MISSING` | 配置 / 用法 | 否 |"), "{body}");
    assert!(body.contains("| `E_FHIR_UNREACHABLE` | 外部依赖 | 是 |"), "{body}");
    // 每一行都要有可执行的处置建议，不能是「见文档」这类空话。
    for l in body.lines().filter(|l| l.starts_with("| `E_")) {
        let cells: Vec<&str> = l.split('|').collect();
        assert!(
            cells.len() >= 7 && cells[5].len() > 8,
            "这一行没有像样的处置建议：{l}"
        );
        assert!(!l.contains("见文档") && !l.contains("TBD"), "{l}");
    }
}

#[test]
fn codes_can_explain_a_single_code_and_refuses_an_unknown_one() {
    let hit = run_cli(&["codes", "--code", "E_STATE_JUMP"], &[]);
    assert_eq!(code(&hit), 0, "{}", both(&hit));
    let body = stdout(&hit);
    assert!(body.contains("E_STATE_JUMP"), "{body}");
    assert!(body.contains("不可重试"), "{body}");
    // 处置建议要能贴进工单：讲清怎么改，不只是复述码名。
    assert!(body.contains("处置："), "{body}");

    // 查不到就说查不到——不拿近似的那一条糊过去，那会让值班的人去查错方向。
    let miss = run_cli(&["codes", "--code", "E_NOT_A_REAL_CODE"], &[]);
    assert_eq!(code(&miss), 2, "未知码应退 2：\n{}", both(&miss));
    assert!(both(&miss).contains("不在本产品的错误码表里"), "{}", both(&miss));
}

#[test]
fn codes_json_is_machine_readable_and_countable() {
    let out = run_cli(&["--json", "codes"], &[]);
    assert_eq!(code(&out), 0, "{}", both(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("stdout 应是纯 JSON");
    assert_eq!(v["subcommand"], "codes");
    let rows = v["codes"].as_array().expect("codes 应是数组");
    assert_eq!(rows.len() as u64, v["count"].as_u64().unwrap());
    assert!(rows.len() >= 10);
    // 每条都要能被 CI 直接分流：重试还是不重试，必须是布尔。
    for r in rows {
        assert!(r["retryable"].is_boolean(), "{r}");
        assert!(r["side"].is_string(), "{r}");
        assert!(!r["origin"].as_str().unwrap().is_empty(), "{r}");
    }
}

/// 一次审查在结构化日志里留下可核对的痕迹。
///
/// 这一条钉的是「事后可复现」：光有一条人类可读的 `--log-file` 不够——
/// 值班的人要能 `jq` 出「这次看了几件、报了什么码、有没有件没被判」。
#[test]
fn check_leaves_a_machine_readable_run_log_and_summary() {
    let dir = tempdir("run-log");
    let out = run_cli(
        &[
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[("PSYCHDOC_DATA_DIR", &dir.display().to_string())],
    );
    assert_eq!(code(&out), 1, "{}", both(&out));

    // 按日切分的日志落在用户数据目录下，不是 round 根也不是项目子目录。
    let logs = dir.join("logs");
    assert!(logs.is_dir(), "结构化日志目录应已建出：{}", dir.display());
    let files: Vec<_> = std::fs::read_dir(&logs)
        .expect("日志目录可读")
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(files.len(), 1, "一次运行只应产生一个日志文件：{files:?}");
    assert!(
        files[0].starts_with("psychdoc-") && files[0].ends_with(".log"),
        "日志文件名应形如 psychdoc-<日期>.log：{files:?}"
    );

    let body = std::fs::read_to_string(logs.join(&files[0])).expect("日志应可读");
    let events: Vec<serde_json::Value> = body
        .lines()
        .map(|l| serde_json::from_str::<serde_json::Value>(l).expect("每行都应是合法 JSON"))
        .collect();

    // 摘要：一行，回答「这次到底发生了什么」。
    let summary: Vec<_> = events.iter().filter(|e| e["event"] == "run.summary").collect();
    assert_eq!(summary.len(), 1, "摘要应恰好一行（重复写会让人分不清哪条为准）");
    let s = &summary[0];
    assert_eq!(s["detail"]["items_total"], "1");
    assert_eq!(s["detail"]["findings"], "1");
    assert_eq!(s["detail"]["blocking"], "1");
    // 规则集短哈希随摘要落盘：事后回溯「当年按哪版审的」靠它。
    assert!(s["detail"]["summary"].as_str().unwrap().contains("规则集 v"));

    // 逐条命中：带件 id 与码，护士长问「3 床那条为什么报了」能直接查到。
    let diags: Vec<_> = events.iter().filter(|e| e["event"] == "diagnostic").collect();
    assert_eq!(diags.len(), 1);
    assert_eq!(diags[0]["code"], "QC-PSY-01");
    assert_eq!(diags[0]["doc_id"], "WARD-A-001-CONSENT-01");
    // 依据行也带上——只报一个码的整改要求没人能执行。
    assert!(diags[0]["detail"]["依据"].as_str().unwrap().contains("条"));

    // 按码统计：一份审计包最常被问的三件事之一。
    let tally: Vec<_> = events.iter().filter(|e| e["event"] == "rule.tally").collect();
    assert_eq!(tally.len(), 1);
    assert_eq!(tally[0]["code"], "QC-PSY-01");
    assert_eq!(tally[0]["detail"]["命中条数"], "1");
}

/// 结构化日志写盘前要再脱敏一次：日志是唯一会被整段贴进群里的东西。
#[test]
fn the_run_log_never_carries_a_patient_identifier() {
    let dir = tempdir("run-log-redact");
    // 打一整批真实脱敏夹具：里面出现过「家属：<姓名样式>」这类自由文本，
    // 正是最容易把患者信息带出去的地方。
    let out = run_cli(
        &[
            "check",
            &project_root().join("fixtures").display().to_string(),
            "--ruleset",
            &ruleset(),
        ],
        &[("PSYCHDOC_DATA_DIR", &dir.display().to_string())],
    );
    let logs = dir.join("logs");
    let files: Vec<_> = std::fs::read_dir(&logs)
        .expect("日志目录可读")
        .filter_map(|e| e.ok())
        .collect();
    assert!(!files.is_empty(), "批扫后应有结构化日志：{}", both(&out));

    let mut total_lines = 0usize;
    for f in files {
        let body = std::fs::read_to_string(f.path()).expect("日志应可读");
        for line in body.lines() {
            serde_json::from_str::<serde_json::Value>(line).expect("每行都应是合法 JSON");
            total_lines += 1;
        }
        // 6 位以上连续数字（住院号样式）不该出现在日志里。
        assert!(
            !contains_long_digit_run(&body),
            "日志里出现了疑似住院号的数字串：\n{body}"
        );
    }
    assert!(total_lines >= 10, "批扫的日志行数太少：{total_lines}");
}

/// 正文里有没有 6 位以上连续数字。住院别名（`WARD-A-001`）与
/// 规则集短哈希（12 位十六进制）**故意不算**：哈希不是住院号，
/// 拿它当泄漏信号会让这条测试恒红而失去意义。
fn contains_long_digit_run(text: &str) -> bool {
    let mut run = 0usize;
    for ch in text.chars() {
        if ch.is_ascii_digit() {
            run += 1;
            // 12 位短哈希允许；超过就不像哈希了，更像住院号。
            if run > 12 {
                return true;
            }
        } else {
            run = 0;
        }
    }
    false
}

/// `--json` 的 `run` 段是同一份摘要的机器可读形态：CI 不必从
/// diagnostics 反推「一共看了几件、有没有件根本没被看到」。
#[test]
fn json_mode_carries_the_run_summary_as_a_first_class_object() {
    let out = run_cli(
        &[
            "--json",
            "check",
            &fixture("ward-01-minimal.json"),
            "--ruleset",
            &ruleset(),
        ],
        &[],
    );
    assert_eq!(code(&out), 1, "{}", both(&out));
    let v: serde_json::Value = serde_json::from_str(&stdout(&out)).expect("stdout 应是纯 JSON");
    let run = &v["run"];
    assert_eq!(run["items_total"], 1);
    assert_eq!(run["items_reviewed"], 1);
    assert_eq!(run["findings"], 1);
    assert_eq!(run["blocking"], 1);
    assert_eq!(run["unresolved"], 0);
    assert!(run["ruleset_short_sha256"].as_str().is_some());
    // 账要平：已审 + 未解析 == 输入。这条断言让「静默跳过」变成
    // 一个 CI 能自己查出来的条件，而不是靠人肉比对。
    assert_eq!(
        run["items_reviewed"].as_u64().unwrap() + run["unresolved"].as_u64().unwrap(),
        run["items_total"].as_u64().unwrap(),
        "运行摘要的账不平：{run}"
    );
}

// ---------------------------------------------------------------------------
// 夹具与工具
// ---------------------------------------------------------------------------

/// 一个临时目录。用 `std::env::temp_dir` 拼名字而非引入 tempfile 依赖：
/// 本 crate 是纯 CLI 通道，为两条断言拖一个新依赖不值得。
fn tempdir(tag: &str) -> PathBuf {
    // 用进程 id + 测试名做后缀：并行跑时互不覆盖，且不依赖系统时钟
    // （测试名已由 `#[test]` 保证唯一）。
    let dir = std::env::temp_dir().join(format!("psychdoc-cli-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("建临时目录");
    dir
}

fn constraint_record() -> serde_json::Value {
    serde_json::json!({
        "resourceType": "Composition",
        "id": "WARD-B-001-RECORD-01",
        "status": "preliminary",
        "title": "<脱敏样例文书>",
        "identifier": [{
            "system": "https://his.example-hospital.invalid/fhir/identifier/encounter",
            "value": "WARD-B-001"
        }],
        "date": "2026-03-02T23:40:00+08:00",
        "authoredOn": "2026-03-03T00:30:00+08:00",
        "type": { "coding": [{
            "system": "https://example-hospital.invalid/fhir/CodeSystem/doc-type",
            "code": "restraint_record"
        }] },
        "author": [{ "reference": "Practitioner/<EXAMPLE_PRACTITIONER>" }],
        "section": [{
            "title": "约束经过",
            "text": "约束起始时间：2026-03-02 23:40。\n约束等级：二级保护。\n执行护士：<EXAMPLE_NURSE>。\n观察记录：约束期间生命体征平稳。"
        }]
    })
}

fn restraint_release() -> serde_json::Value {
    serde_json::json!({
        "resourceType": "Composition",
        "id": "WARD-B-001-RELEASE-01",
        "status": "preliminary",
        "title": "<脱敏样例文书>",
        "identifier": [{
            "system": "https://his.example-hospital.invalid/fhir/identifier/encounter",
            "value": "WARD-B-001"
        }],
        "date": "2026-03-03T06:10:00+08:00",
        "authoredOn": "2026-03-03T06:20:00+08:00",
        "type": { "coding": [{
            "system": "https://example-hospital.invalid/fhir/CodeSystem/doc-type",
            "code": "restraint_release"
        }] },
        "author": [{ "reference": "Practitioner/<EXAMPLE_PRACTITIONER>" }],
        "section": [{
            "title": "解除经过",
            "text": "解除时间：2026-03-03 06:10。\n解除原因：病情稳定。\n执行护士：<EXAMPLE_NURSE>。"
        }]
    })
}

fn selection_json() -> String {
    serde_json::json!({
        "encounter_alias": "WARD-B-001",
        "actor": "病区文书专员",
        "at": "2026-03-03T07:00:00+08:00",
        "ruleset_sha256": "2abfac00aa5bd0dc0b0b1cf0e2f2f1a3b4c5d6e7f8091a2b3c4d5e6f708192a3",
        "entries": [
            {
                "doc_id": "WARD-B-001-CONSENT-01",
                "content_sha256": "aaaa1111bbbb2222cccc3333dddd4444eeee5555ffff6666aaaa7777bbbb8888",
                "resource": { "resourceType": "Composition", "id": "WARD-B-001-CONSENT-01", "status": "preliminary" }
            },
            {
                "doc_id": "WARD-B-001-RECORD-01",
                "content_sha256": "1111aaaa2222bbbb3333cccc4444dddd5555eeee6666ffff7777aaaa8888bbbb",
                "resource": { "resourceType": "Composition", "id": "WARD-B-001-RECORD-01", "status": "preliminary" }
            }
        ]
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// preflight：启动自检的退出码契约（Task 38）
// ---------------------------------------------------------------------------

/// 五个变量齐备时用的样例值。测试里的凭据是**断言材料**，
/// 产品代码里一个都没有——`psychdoc-fhir` 的 `startup_preflight.rs`
/// 会扫源码把这件事钉住。
fn cred_env() -> Vec<(&'static str, &'static str)> {
    vec![
        ("EMR_BASE_URL", "https://emr.example-hospital.local/fhir/r4"),
        ("EMR_CLIENT_ID", "psychdoc-client"),
        ("EMR_CLIENT_SECRET", "s3cret-value"),
        ("EMR_TENANT_ID", "ward-a"),
        (
            "WECOM_WEBHOOK_URL",
            "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc-123",
        ),
    ]
}

#[test]
fn preflight_exits_0_when_all_five_credentials_are_present() {
    let out = run_cli(&["preflight"], &cred_env());
    let text = both(&out);
    assert_eq!(code(&out), 0, "五项齐备应退 0：\n{text}");
    assert!(text.contains("EMR_BASE_URL") && text.contains("WECOM_WEBHOOK_URL"), "{text}");
    assert!(text.contains("未发起任何网络请求"), "自检必须说清没外发：{text}");
}

#[test]
fn preflight_exits_2_and_names_every_missing_credential_in_one_pass() {
    // 五项全缺：一次列全，而不是让运维改一条跑一次。
    let out = run_cli(&["preflight"], &[]);
    let text = both(&out);
    assert_eq!(code(&out), 2, "缺配置应退 2（配置侧，不是网络侧）：\n{text}");
    for key in [
        "EMR_BASE_URL",
        "EMR_CLIENT_ID",
        "EMR_CLIENT_SECRET",
        "EMR_TENANT_ID",
        "WECOM_WEBHOOK_URL",
    ] {
        assert!(text.contains(key), "{key} 应一次列全：\n{text}");
    }
    assert!(text.contains("缺少 5 项"), "{text}");
    // 「缺配置」不能被读成「已降级到本地」。
    assert!(text.contains("不会降级"), "{text}");
    // 缺的变量不该同时又被报成「形态错」——那是让值守的人去查一个
    // 他还没填的值的格式。
    assert!(!text.contains("应以 http://"), "缺失项不该重复报形态错：\n{text}");
}

#[test]
fn preflight_reports_a_malformed_value_as_a_shape_error_not_as_missing() {
    let mut env = cred_env();
    env[0].1 = "emr.example-hospital.local"; // 没头
    let out = run_cli(&["preflight"], &env);
    let text = both(&out);
    assert_eq!(code(&out), 2, "{text}");
    assert!(text.contains("E_CFG_INVALID"), "形态错与缺失分开报码：\n{text}");
    assert!(text.contains("应以 http://"), "{text}");
    // 形态错不得被悄悄纠正：没自动补 https://。
    assert!(!text.contains("emr.example-hospital.local/fhir"), "{text}");
}

#[test]
fn preflight_json_carries_the_report_and_proves_zero_network_calls() {
    let out = run_cli(&["preflight", "--json"], &[]);
    let text = stdout(&out);
    let v: serde_json::Value = serde_json::from_str(text.trim())
        .unwrap_or_else(|e| panic!("--json 应输出可解析 JSON — {e}\n{text}"));
    assert_eq!(v["ok"], false);
    assert_eq!(v["network_calls"], 0, "自检不得外发：{text}");
    assert_eq!(
        v["missing"].as_array().map(|a| a.len()),
        Some(5),
        "五项一次列全：{text}"
    );
    assert_eq!(v["code"], "E_CFG_MISSING");
}

#[test]
fn a_missing_writeback_credential_does_not_stop_local_review() {
    // 关键分工：缺凭据只让**院内对接**不可用，本地文书包审查照常跑。
    // 护士长没配密钥时仍要能看到清单，否则这产品对他毫无用处。
    let out = run_cli(
        &["check", &fixture("ward-01-minimal.json"), "--ruleset", &ruleset()],
        &[],
    );
    let text = both(&out);
    assert_eq!(code(&out), 1, "本地审查不该被凭据拦住：\n{text}");
    assert!(text.contains("QC-PSY-01"), "{text}");
}
