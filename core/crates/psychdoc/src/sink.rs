//! 输出去向：`--log-file` 落盘 + `--json` 时 stdout 只留机器可读正文。
//!
//! 院内信息科把这条 CLI 接进**无人值守跑批**与 CI 之后，会同时要两样东西：
//!
//! - 人看的完整过程（这次审了什么、为什么这么判）→ 磁盘日志；
//! - 机器读的结论（命中几条、退出码几）→ stdout 上**唯一**一段 JSON。
//!
//! 两样东西混在一条 stdout 上，`jq` 就再也解析不动了。因此这里给一条硬规矩：
//! **开了 `--json` 之后，人类可读的过程文字只进日志文件，不再上 stdout**。
//! 没给 `--log-file` 时这些文字直接丢弃——宁可少一段，也不让 stdout 变成
//! 「前半段中文、后半段 JSON」的混合物，让下游脚本自己猜边界。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);
/// 开了 `--json` 之后，人类可读文字是否停止上 stdout。
static SUPPRESS_HUMAN: AtomicBool = AtomicBool::new(false);

/// 打开磁盘日志。父目录不存在时一并建出来。
///
/// 覆盖写（`truncate`）而不是追加：跑批日志按「一次跑批一个文件」组织，
/// 追加只会让第二次跑批的结论接在第一次的判定后面——CI 抓取最后一次
/// 退出码对应的是哪一段，没人说得清。
pub fn open_log(path: &str) -> Result<(), String> {
    let p = std::path::Path::new(path);
    if let Some(parent) = p.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("建不出日志目录 {} — {e}", parent.display()))?;
        }
    }
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(p)
        .map_err(|e| format!("写不进日志文件 {path} — {e}"))?;
    *LOG_FILE.lock().expect("日志锁中毒") = Some(f);
    Ok(())
}

/// 进入 `--json` 模式。此后 [`say`] 不再写 stdout。
pub fn set_json_mode(on: bool) {
    SUPPRESS_HUMAN.store(on, Ordering::SeqCst);
}

/// 当前是否处于 `--json` 模式（供子命令决定「这行算正文还是过程」）。
pub fn json_mode() -> bool {
    SUPPRESS_HUMAN.load(Ordering::SeqCst)
}

/// 一行人类可读文字：stdout + 日志文件（`--json` 模式下只进日志文件）。
pub fn say(s: &str) {
    if !json_mode() {
        println!("{s}");
    }
    to_log(s);
}

/// 一行错误文字：**始终**上 stderr（CI 的错误采集抓的是 stderr），
/// 同时进日志文件——`--json` 模式下 stdout 必须是干净的。
pub fn say_err(s: &str) {
    eprintln!("{s}");
    to_log(s);
}

/// `--json` 模式下 stdout 的正文出口：只写 stdout，**不**进日志文件。
///
/// 写进日志反而有害：日志是给人读的进程记录，里面混一段机器正文后，
/// 「这次跑批打印了什么」和「结论是什么」就得分两处找。
pub fn emit_json(body: &str) {
    println!("{body}");
}

fn to_log(s: &str) {
    let mut guard = LOG_FILE.lock().expect("日志锁中毒");
    if let Some(f) = guard.as_mut() {
        // 写日志失败**不**影响主流程：审查结论已经算出来了，
        // 因为日志盘满了就丢掉整次审查是不可接受的反向失败。
        let _ = writeln!(f, "{s}");
        let _ = f.flush();
    }
}
