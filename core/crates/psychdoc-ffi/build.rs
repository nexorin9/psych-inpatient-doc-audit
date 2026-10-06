//! 构建脚本：给 cdylib 补上 PyO3 扩展模块在 macOS 上必须的链接参数。
//!
//! Python 扩展模块**不链接** `libpython`（符号由解释器进程在装载时提供），
//! macOS 的链接器因此必须显式允许未定义符号，否则 `cargo build --features
//! extension-module` 会以 `_PyBool_Type undefined` 之类的一串报错收场。
//!
//! 正常打包路径（`maturin develop` / `maturin build`）由 maturin 注入同样的
//! 参数；这里显式写出来，使得**只装 cargo、不装 maturin** 也能本地建出
//! 可 `import` 的 `.so`，让 `run.sh` 的自检与 `pytest tests/test_ffi_bindings.py`
//! 在离线环境里仍然跑得通。
//!
//! 参数只在 `extension-module` feature 打开时加：不开这个 feature 时本 crate
//! 是普通 rlib，不该带任何 Python 链接痕迹。

fn main() {
    #[cfg(feature = "extension-module")]
    pyo3_build_config::add_extension_module_link_args();
    println!("cargo:rerun-if-changed=build.rs");
}
