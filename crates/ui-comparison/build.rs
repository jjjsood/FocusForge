// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    // Element lookup by id (tests, the inspection endpoint) needs the compiler's
    // debug info; `SLINT_EMIT_DEBUG_INFO=1` enables it on any build.
    let mut config = slint_build::CompilerConfiguration::new()
        .with_include_paths(vec![std::path::PathBuf::from("../ui/ui")]);
    if cfg!(feature = "element-debug-info") {
        config = config.with_debug_info(true);
    }
    slint_build::compile_with_config("../ui/ui/comparison_window.slint", config).unwrap();
}
