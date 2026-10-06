fn main() {
    println!("cargo:rerun-if-changed=Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        let plist = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }

    println!("cargo:rerun-if-changed=../../assets/icons/app/tcode.rc");
    println!("cargo:rerun-if-changed=../../assets/icons/app/tcode.ico");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        embed_resource::compile_for(
            "../../assets/icons/app/tcode.rc",
            ["tcode"],
            embed_resource::NONE,
        )
        .manifest_optional()
        .expect("failed to embed the Tcode application icon");
    }
}
