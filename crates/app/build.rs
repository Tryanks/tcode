fn main() {
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
