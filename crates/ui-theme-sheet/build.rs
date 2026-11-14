// SPDX-License-Identifier: GPL-3.0-or-later

fn main() {
    if !cfg!(feature = "generate") {
        return;
    }
    // The sheet's test finds elements by type name, which needs debug info.
    let config = slint_build::CompilerConfiguration::new()
        .with_include_paths(vec![std::path::PathBuf::from("../ui/ui")])
        .with_debug_info(true);
    slint_build::compile_with_config("../ui/ui/theme_sheet.slint", config).unwrap();
}
