fn main() {
    println!("cargo:rerun-if-changed=Info.plist");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("macos") {
        // Read at run time: `env!` bakes in the checkout that compiled this
        // script, and a shared build cache reuses it from other checkouts.
        let manifest_dir =
            std::env::var_os("CARGO_MANIFEST_DIR").expect("cargo sets CARGO_MANIFEST_DIR");
        let plist = std::path::Path::new(&manifest_dir).join("Info.plist");
        println!(
            "cargo:rustc-link-arg-bins=-Wl,-sectcreate,__TEXT,__info_plist,{}",
            plist.display()
        );
    }

    println!("cargo:rerun-if-changed=../../assets/icons/app/tcode.rc");
    println!("cargo:rerun-if-changed=../../assets/icons/app/tcode.ico");

    if std::env::var_os("CARGO_CFG_WINDOWS").is_some() {
        // GPUI requests, prepaints and paints the element tree recursively on
        // the main thread, so its stack grows with how deeply a window's
        // content nests. Windows reserves 1 MiB for that thread unless the
        // binary asks for more, and a conversation with nested lists and
        // quotes overflows it; macOS and Linux give it 8 MiB.
        if std::env::var("CARGO_CFG_TARGET_ENV").as_deref() == Ok("msvc") {
            println!("cargo:rustc-link-arg-bins=/STACK:{}", 8 * 1024 * 1024);
        }
        embed_resource::compile_for(
            "../../assets/icons/app/tcode.rc",
            ["tcode"],
            embed_resource::NONE,
        )
        .manifest_optional()
        .expect("failed to embed the Tcode application icon");
    }
}
