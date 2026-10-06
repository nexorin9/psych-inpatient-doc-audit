//! `serde_json::Value` → Python 对象的最小转换层。
//!
//! 绑定层刻意**不**逐字段手写 `PyDict`：`check_bundle` 要回的诊断条数
//! 随文书包规模变化，手写映射会在加一条规则时静默漏字段。这里只做
//! 「值 → 值」的一一递归映射；结构由 [`crate::api`] 里的 `serde_json::json!`
//! 单点定义，Rust 侧与 Python 侧的字段名因此不会漂移。
//!
//! 转换是**只读快照**：核心侧全部同步、无内部线程，Python 拿到的是
//! 已完成的普通 Python 对象，不存在跨线程借用。

use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyList, PyString};

use crate::bindings::PsychdocError;

/// 把一个 JSON 值转成对应的 Python 对象。
pub fn to_py(py: Python<'_>, value: &serde_json::Value) -> PyResult<PyObject> {
    Ok(match value {
        serde_json::Value::Null => py.None(),
        serde_json::Value::Bool(b) => PyBool::new_bound(py, *b).to_owned().into_any().unbind(),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                i.into_py(py)
            } else if let Some(u) = n.as_u64() {
                u.into_py(py)
            } else {
                n.as_f64().unwrap_or(f64::NAN).into_py(py)
            }
        }
        serde_json::Value::String(s) => PyString::new_bound(py, s).into(),
        serde_json::Value::Array(items) => {
            let list = PyList::empty_bound(py);
            for item in items {
                list.append(to_py(py, item)?)?;
            }
            list.into()
        }
        serde_json::Value::Object(map) => {
            let dict = PyDict::new_bound(py);
            for (k, v) in map {
                dict.set_item(k, to_py(py, v)?)?;
            }
            dict.into()
        }
    })
}

/// 把「错误码 + 文案」包成 Python 异常抛出。
///
/// 业务代码看到的是**错误码**（`E_CFG_MISSING` / `E_RULESET_INVALID` …）而不是
/// 一句异常文本：桌面壳据此决定「弹窗 / 底栏红字 / 直接退出」，
/// 只 `str(e)` 会让 UI 层去匹配中文文案，那是最容易漂移的做法。
pub fn raise(py: Python<'_>, code: &str, message: &str) -> PyErr {
    let err = PsychdocError::new_err(message.to_string());
    // 附加属性 `code` / `message`：Python 侧 `except PsychdocError as e: e.code`。
    let inst = err.value_bound(py);
    let _ = inst.setattr("code", code);
    let _ = inst.setattr("message", message);
    err
}
