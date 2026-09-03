use std::env;

fn main() {
    let icons_dir = env::var("DEP_GPUI_KIT_DEFAULT_ICONS_ICONS_DIR")
        .expect("DEP_GPUI_KIT_DEFAULT_ICONS_ICONS_DIR is set by gpui-kit-assets");

    println!("cargo:rustc-env=GPUI_COMPONENT_DEFAULT_ICONS_DIR={icons_dir}");
    println!("cargo:rerun-if-changed={icons_dir}");
    println!("cargo:rerun-if-changed=build.rs");
}
