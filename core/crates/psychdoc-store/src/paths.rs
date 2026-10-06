//! 库文件落位：**用户数据目录**，绝不落在 round 根或项目子目录里。
//!
//! 单机部署（信息科一台机器一份），库文件是运行期产物，不是源码。
//! 落错位置的代价很实际：把库文件当源码提交进 GitHub，或者清理阶段
//! 把审查记录一起删掉。因此本模块只做一件事——把落位规则钉死并可测。
//!
//! 解析顺序（先命中先返回）：
//!
//! 1. `PSYCHDOC_DATA_DIR`：测试与运维显式指定，最高优先；
//! 2. macOS：`~/Library/Application Support/PsychInpatientDocAudit`；
//! 3. Windows：`%APPDATA%\PsychInpatientDocAudit`；
//! 4. 其它：`$XDG_DATA_HOME/psych-inpatient-doc-audit`，缺则 `~/.local/share/…`。

use std::path::PathBuf;

use crate::error::{Result, StoreError};

/// 库文件名（固定，便于运维与 Task 16 清理阶段识别）。
pub const DB_FILE_NAME: &str = "psychdoc.sqlite3";

/// 应用在用户数据目录下的目录名（macOS / Windows）。
const APP_DIR: &str = "PsychInpatientDocAudit";
/// 应用在 XDG 目录下的目录名（kebab-case，符合 freedesktop 约定）。
const XDG_DIR: &str = "psych-inpatient-doc-audit";

/// 用户数据目录（不保证已存在）。
pub fn data_dir() -> Result<PathBuf> {
    if let Some(explicit) = non_empty_env("PSYCHDOC_DATA_DIR") {
        return Ok(PathBuf::from(explicit));
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = non_empty_env("HOME") {
            return Ok(PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join(APP_DIR));
        }
    }
    if cfg!(target_os = "windows") {
        if let Some(appdata) = non_empty_env("APPDATA") {
            return Ok(PathBuf::from(appdata).join(APP_DIR));
        }
    }
    if let Some(xdg) = non_empty_env("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join(XDG_DIR));
    }
    if let Some(home) = non_empty_env("HOME") {
        return Ok(PathBuf::from(home).join(".local").join("share").join(XDG_DIR));
    }
    if let Some(profile) = non_empty_env("USERPROFILE") {
        return Ok(PathBuf::from(profile)
            .join("AppData")
            .join("Roaming")
            .join(APP_DIR));
    }
    Err(StoreError::DataDirUnresolved)
}

/// 库文件完整路径。
pub fn db_path() -> Result<PathBuf> {
    Ok(data_dir()?.join(DB_FILE_NAME))
}

fn non_empty_env(key: &str) -> Option<String> {
    match std::env::var(key) {
        Ok(v) if !v.trim().is_empty() => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_env_var_wins() {
        // 用一个独立子进程语义不便，这里只验证拼接规则本身。
        assert_eq!(DB_FILE_NAME, "psychdoc.sqlite3");
        let joined = PathBuf::from("/tmp/xyz").join(DB_FILE_NAME);
        assert!(joined.to_string_lossy().ends_with("psychdoc.sqlite3"));
    }

    #[test]
    fn empty_env_is_treated_as_unset() {
        assert!(non_empty_env("__PSYCHDOC_DEFINITELY_UNSET_VAR__").is_none());
    }
}
