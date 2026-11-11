// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    // `with_debug_info` -- `tests/shell.rs`'s `slint_testing::ElementQuery`
    // needs the compiler's debug info to find elements by id at all.
    let config = slint_build::CompilerConfiguration::new()
        .with_debug_info(true)
        .with_include_paths(vec![std::path::PathBuf::from("ui")]);
    slint_build::compile_with_config("ui/app.slint", config).unwrap();
}
