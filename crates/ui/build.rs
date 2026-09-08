use std::{env, fs, path::PathBuf};

fn main() {
    let icons_dir = env::var("DEP_GPUI_KIT_DEFAULT_ICONS_ICONS_DIR")
        .expect("DEP_GPUI_KIT_DEFAULT_ICONS_ICONS_DIR is set by gpui-kit-assets");

    // Browser assets must be available synchronously, including on an offline LAN.
    // Use the same dependency-owned directory that generates IconName.
    let mut icons = fs::read_dir(&icons_dir)
        .expect("read component icons")
        .map(|entry| entry.expect("read icon entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "svg"))
        .collect::<Vec<_>>();
    icons.sort();
    let mut source = String::from("const COMPONENT_ICONS: &[(&str, &[u8])] = &[\n");
    for icon in icons {
        let name = icon.file_name().expect("icon filename").to_string_lossy();
        source.push_str(&format!(
            "(\"icons/{name}\", include_bytes!({:?})),\n",
            icon.to_str().expect("UTF-8 icon path")
        ));
    }
    source.push_str("];\n");
    fs::write(
        PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR")).join("component_icons.rs"),
        source,
    )
    .expect("write embedded component icon table");

    println!("cargo:rustc-env=GPUI_COMPONENT_DEFAULT_ICONS_DIR={icons_dir}");
    println!("cargo:rerun-if-changed={icons_dir}");
    println!("cargo:rerun-if-changed=build.rs");
}
