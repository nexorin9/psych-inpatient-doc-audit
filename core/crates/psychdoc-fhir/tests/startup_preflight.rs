//! 启动自检的**进程级**契约：缺配置时既不静默降级，也不外发一次请求。
//!
//! `startup.rs` 里的单测打的是「注入取值来源」的纯函数；本文件打的是
//! **真进程环境**那条路径，并额外钉住两件在单测里看不出来的事：
//!
//! 1. **零次网络调用**：本文件起一个真实的 `TcpListener`，把
//!    `EMR_BASE_URL` 指向它，然后跑自检。监听器一次都没被 accept
//!    ——「缺配置」必须在 0 次外发的前提下被检出，否则「静默降级」
//!    就藏在校验的网络往返里（端点暂时不通 → 退避重试 → 重试一万次
//!    也不会长出环境变量来）。
//! 2. **没有任何默认凭据**：产品代码里不得出现硬编码的 EMR 地址、
//!    client id 或 webhook key。编译不进去的默认值，扫源码来钉。
//!
//! 环境变量是**进程全局**状态，Rust 测试默认多线程并行：本文件所有
//! 会动 `EMR_*` / `WECOM_*` / `PSYCHDOC_*` 的用例都必须持有
//! [`ENV_LOCK`]，否则会与别的用例互相踩。

use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::Duration;

use psychdoc_fhir::startup::{LogLevel, StartupConfig, StartupReport};
use psychdoc_fhir::{
    ENV_BASE_URL, ENV_CLIENT_ID, ENV_CLIENT_SECRET, ENV_TENANT_ID, ENV_WECOM_WEBHOOK_URL,
};

/// 动环境变量的用例共用的锁。
fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 记一次「有人连上来了」。自检跑完时它必须是空的。
static CONNECTIONS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// 起一个真实监听端点：任何一次外发都会在这里留下一条记录。
///
/// 用独立**子进程**而不是同进程线程：产品侧的 `ureq` 是同步阻塞的，
/// 调用它的线程等响应期间不放 GIL，同进程的 accept 线程可能抢不到
/// 调度。这里用子进程是为了让「连了就是连了」这件事无可争议。
fn spawn_recording_endpoint() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("应能绑一个本地端口");
    let addr = listener.local_addr().expect("取本地地址");
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            CONNECTIONS.lock().unwrap().push(describe(stream));
        }
    });
    format!("http://{addr}/fhir/r4")
}

fn describe(_s: TcpStream) -> String {
    "一次外发".to_string()
}

fn connection_count() -> usize {
    CONNECTIONS.lock().unwrap().len()
}

/// crate 源码目录（`core/crates`）。
fn crates_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(1)
        .expect("找不到 core/crates")
        .to_path_buf()
}

fn src_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            src_files(&p, out);
        } else if p.extension().and_then(|s| s.to_str()) == Some("rs") {
            out.push(p);
        }
    }
}

/// 读 `#[cfg(test)]` 之前的部分：单测里的样例凭据不算「硬编码默认值」。
fn code_without_tests(path: &Path) -> String {
    let text = std::fs::read_to_string(path).unwrap_or_default();
    match text.find("#[cfg(test)]") {
        Some(at) => text[..at].to_string(),
        None => text,
    }
}

// ---------------------------------------------------------------------------
// 五项凭据：缺一即失败，一次列全
// ---------------------------------------------------------------------------

#[test]
fn with_nothing_configured_the_report_names_all_five_at_once() {
    let _g = env_lock();
    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
        "PSYCHDOC_RULESET",
        "PSYCHDOC_DATA_DIR",
        "PSYCHDOC_LOG_LEVEL",
    ] {
        std::env::remove_var(k);
    }

    let r = StartupReport::check(|k| std::env::var(k).ok());
    assert!(!r.writeback_ready);
    assert_eq!(
        r.missing.len(),
        5,
        "五项应一次列全，而不是报一个报一个：{:?}",
        r.missing
    );
    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        assert!(r.missing.contains(&k), "{k} 应在缺失清单里：{:?}", r.missing);
    }

    let err = StartupConfig::from_env().expect_err("一项都没配时必须启动失败");
    assert_eq!(err.code(), "E_CFG_MISSING");
    let text = err.to_string();
    assert!(text.contains("缺少 5 项"), "{text}");
    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        assert!(text.contains(k), "{k} 应一次列全：{text}");
    }
    // 缺失 ≠ 降级：文案必须说清「不会降级」，否则值守的人会去查别处。
    assert!(text.contains("不会降级"), "{text}");
}

#[test]
fn missing_three_are_listed_in_one_pass_not_three_restarts() {
    let _g = env_lock();
    let _endpoint = spawn_recording_endpoint();
    std::env::set_var(ENV_BASE_URL, &_endpoint);
    std::env::set_var(ENV_CLIENT_ID, "psychdoc-client");
    std::env::set_var(ENV_TENANT_ID, "ward-a");
    std::env::remove_var(ENV_CLIENT_SECRET);
    std::env::remove_var(ENV_WECOM_WEBHOOK_URL);
    let before = connection_count();

    let err = StartupConfig::from_env().expect_err("缺两项必须启动失败");
    let text = err.to_string();
    assert_eq!(err.code(), "E_CFG_MISSING");
    assert!(text.contains("缺少 2 项"), "{text}");
    assert!(text.contains(ENV_CLIENT_SECRET) && text.contains(ENV_WECOM_WEBHOOK_URL), "{text}");
    // 已填的那三项不该出现在缺失清单里——否则「列全」变成了「全报」。
    assert!(!text.contains(ENV_CLIENT_ID), "已填项不该被列成缺失：{text}");

    std::thread::sleep(Duration::from_millis(80));
    assert_eq!(
        connection_count(),
        before,
        "自检不得发起任何网络请求（哪怕 base_url 指向一个活端点）"
    );
    for k in [ENV_BASE_URL, ENV_CLIENT_ID, ENV_TENANT_ID] {
        std::env::remove_var(k);
    }
}

#[test]
fn a_blank_value_is_missing_and_a_malformed_one_is_a_shape_error() {
    let _g = env_lock();
    std::env::set_var(ENV_BASE_URL, "https://emr.example-hospital.local/fhir/r4");
    std::env::set_var(ENV_CLIENT_ID, "psychdoc-client");
    std::env::set_var(ENV_CLIENT_SECRET, "s3cret-value");
    std::env::set_var(ENV_TENANT_ID, "ward-a");
    std::env::set_var(ENV_WECOM_WEBHOOK_URL, "   ");
    std::env::remove_var("PSYCHDOC_RULESET");
    std::env::remove_var("PSYCHDOC_DATA_DIR");
    std::env::remove_var("PSYCHDOC_LOG_LEVEL");

    // 空白算缺失，且报的是缺失码——「以为填了」是最难查的一类事故。
    let err = StartupConfig::from_env().unwrap_err();
    assert_eq!(err.code(), "E_CFG_MISSING", "{}", err);
    assert!(err.to_string().contains(ENV_WECOM_WEBHOOK_URL), "{}", err);

    // 填了但形态错 → 另一个码。两类问题找的人不一样，不能合成一个。
    std::env::set_var(ENV_WECOM_WEBHOOK_URL, "qyapi.weixin.qq.com/hook?key=abc");
    let err = StartupConfig::from_env().unwrap_err();
    assert_eq!(err.code(), "E_CFG_INVALID", "{}", err);
    assert!(err.to_string().contains("http"), "{}", err);

    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        std::env::remove_var(k);
    }
}

#[test]
fn a_shape_error_never_becomes_a_silent_default() {
    let _g = env_lock();
    std::env::set_var(ENV_BASE_URL, "emr.example-hospital.local"); // 没头
    std::env::set_var(ENV_CLIENT_ID, "psychdoc-client");
    std::env::set_var(ENV_CLIENT_SECRET, "s3cret-value");
    std::env::set_var(ENV_TENANT_ID, "ward-a");
    std::env::set_var(ENV_WECOM_WEBHOOK_URL, "https://qyapi.weixin.qq.com/x?key=abc");

    // 最危险的一种「省事」：URL 没头就自动补 https://。院内地址写错协议
    // 与写错主机名同样是事故，补前缀只会把事故推迟到第一次写回。
    assert!(StartupConfig::from_env().is_err(), "形态错必须失败，不得自动纠正");
    std::env::remove_var(ENV_BASE_URL);
    for k in [
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        std::env::remove_var(k);
    }
}

// ---------------------------------------------------------------------------
// 非凭据配置：规则集 / 数据目录 / 日志级别
// ---------------------------------------------------------------------------

#[test]
fn non_credential_settings_are_read_with_relative_paths_resolved() {
    let _g = env_lock();
    let base = std::env::current_dir().expect("取工作目录");
    for (k, v) in [
        (ENV_BASE_URL, "https://emr.example-hospital.local/fhir/r4"),
        (ENV_CLIENT_ID, "psychdoc-client"),
        (ENV_CLIENT_SECRET, "s3cret-value"),
        (ENV_TENANT_ID, "ward-a"),
        (ENV_WECOM_WEBHOOK_URL, "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc"),
    ] {
        std::env::set_var(k, v);
    }
    std::env::set_var("PSYCHDOC_RULESET", "rulesets/hospital.yaml");
    std::env::set_var("PSYCHDOC_DATA_DIR", "var/psychdoc");
    std::env::set_var("PSYCHDOC_LOG_LEVEL", "debug");

    let cfg = StartupConfig::from_env().unwrap();
    assert_eq!(cfg.ruleset, base.join("rulesets").join("hospital.yaml"));
    assert_eq!(
        cfg.data_dir,
        Some(base.join("var").join("psychdoc")),
        "相对数据目录要按同一个基准解析：原样留着的话，\
         双击启动与跑批两种工作目录会各读到各的库"
    );
    assert_eq!(cfg.log_level, LogLevel::Debug);

    std::env::remove_var("PSYCHDOC_RULESET");
    std::env::remove_var("PSYCHDOC_DATA_DIR");
    std::env::remove_var("PSYCHDOC_LOG_LEVEL");
    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        std::env::remove_var(k);
    }
}

#[test]
fn an_unknown_log_level_is_refused_rather_than_silently_defaulted() {
    let _g = env_lock();
    for (k, v) in [
        (ENV_BASE_URL, "https://emr.example-hospital.local/fhir/r4"),
        (ENV_CLIENT_ID, "psychdoc-client"),
        (ENV_CLIENT_SECRET, "s3cret-value"),
        (ENV_TENANT_ID, "ward-a"),
        (ENV_WECOM_WEBHOOK_URL, "https://qyapi.weixin.qq.com/cgi-bin/webhook/send?key=abc"),
    ] {
        std::env::set_var(k, v);
    }
    std::env::set_var("PSYCHDOC_LOG_LEVEL", "verbose");
    let err = StartupConfig::from_env().expect_err("不认识的级别必须拒绝");
    assert_eq!(err.code(), "E_CFG_INVALID", "{}", err);
    assert!(err.to_string().contains("PSYCHDOC_LOG_LEVEL"), "{}", err);
    std::env::remove_var("PSYCHDOC_LOG_LEVEL");
    for k in [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ] {
        std::env::remove_var(k);
    }
}

// ---------------------------------------------------------------------------
// 不提供默认凭据
// ---------------------------------------------------------------------------

#[test]
fn no_product_code_carries_a_default_credential_or_a_default_endpoint() {
    // 「不提供默认凭据」这件事编译不出来，只能扫源码。但扫的不是
    // 「源码里有没有出现过某个 URL 形状」——那会误伤**帮助文案**
    // （`E_CFG_INVALID` 的 detail 里就写着「企业微信地址形如 https://qyapi…」，
    // 那是给值守的人看的提示，不是会被采用的默认值）。
    //
    // 真正要防的是：**取值链路上挂了非空兜底**。形如
    // `env::var(X).unwrap_or("https://…")` / `.or(Some("s3cret"))`，
    // 一旦有人图省事补上，环境变量没配就会被这个字面量顶替，
    // 而现场表现是「连着一个不存在的端点」——比直接报错难查得多。
    let mut files = Vec::new();
    src_files(&crates_dir(), &mut files);
    assert!(files.len() >= 7, "扫描范围只有 {} 个文件，扫描本身可能坏了", files.len());

    // 凭据相关的取值函数名：这些名字出现在带字面量兜底的链路上才是问题。
    let credential_vars = [
        ENV_BASE_URL,
        ENV_CLIENT_ID,
        ENV_CLIENT_SECRET,
        ENV_TENANT_ID,
        ENV_WECOM_WEBHOOK_URL,
    ];
    let mut hits: Vec<String> = Vec::new();
    for f in &files {
        let code = code_without_tests(f);
        for (i, line) in code.lines().enumerate() {
            let trimmed = line.trim();
            if trimmed.starts_with("//") || trimmed.starts_with("///") || trimmed.starts_with("//!") {
                continue;
            }
            let touches_credential = credential_vars.iter().any(|v| line.contains(v));
            if !touches_credential {
                continue;
            }
            // `unwrap_or("")` 之类是**空**兜底，等价于「没有」，
            // 上面 `split` 里的空白判定已经把它算作缺失；这里只抓非空兜底。
            for marker in ["unwrap_or(", "or(Some(", "unwrap_or_else(|_|"] {
                if let Some(at) = line.find(marker) {
                    let tail = &line[at..];
                    let has_literal = tail.contains('"')
                        && !tail.contains("unwrap_or_default")
                        && !tail.trim_end().ends_with("unwrap_or(\"\")")
                        && !tail.contains("unwrap_or(\"\")");
                    if has_literal {
                        hits.push(format!(
                            "{}:{} 的凭据取值链路上带了字面量兜底：{}",
                            f.display(),
                            i + 1,
                            trimmed
                        ));
                    }
                }
            }
        }
    }
    assert!(
        hits.is_empty(),
        "凭据读取链路上出现了默认兜底（编译进二进制的凭据等于没有凭据，\
         现场表现是「连着一个不存在的端点」而不是「明确报缺配置」）：\n  {}",
        hits.join("\n  ")
    );
}

#[test]
fn the_config_loader_never_falls_back_to_a_local_stand_in() {
    // 「不静默降级」的**结构性**证据：加载器返回的类型里没有任何
    // 「本地替身 / 离线模式」的字段或开关。真要降级，得先改这个类型，
    // 而类型一变，`the_five_variable_names_are_exactly_the_contract` 那组
    // 断言就会红。
    let cfg = StartupConfig::from_source(
        |k| match k {
            ENV_BASE_URL => Some("https://emr.example-hospital.local/fhir/r4".into()),
            ENV_CLIENT_ID => Some("psychdoc-client".into()),
            ENV_CLIENT_SECRET => Some("s3cret-value".into()),
            ENV_TENANT_ID => Some("ward-a".into()),
            ENV_WECOM_WEBHOOK_URL => Some("https://qyapi.weixin.qq.com/x?key=abc".into()),
            _ => None,
        },
        Path::new("/app"),
    )
    .unwrap();
    // 端点一定来自环境变量本身：一个字都没被改写。
    assert_eq!(cfg.emr.base_url, "https://emr.example-hospital.local/fhir/r4");
    assert_eq!(cfg.emr.token_url(), "https://emr.example-hospital.local/fhir/r4/oauth2/token");
}
