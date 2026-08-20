use std::{env, fs, path::PathBuf, process::Command};

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
}

fn sdk_is_at_least_26(version: &str) -> bool {
    version
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .is_some_and(|major| major >= 26)
}

fn main() {
    println!("cargo:rerun-if-changed=swift/shim.swift");
    println!("cargo:rerun-if-env-changed=TCODE_VOICE_FORCE_STUB");

    if env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("macos")
        || env::var("TCODE_VOICE_FORCE_STUB").as_deref() == Ok("1")
    {
        return;
    }

    let Some(sdk_version) = command_output("xcrun", &["--sdk", "macosx", "--show-sdk-version"])
    else {
        return;
    };
    if !sdk_is_at_least_26(&sdk_version) {
        return;
    }

    let sdk_path = command_output("xcrun", &["--sdk", "macosx", "--show-sdk-path"])
        .expect("macOS SDK path is unavailable");
    let swiftc =
        command_output("xcrun", &["--find", "swiftc"]).expect("Swift compiler is unavailable");
    let arch = match env::var("CARGO_CFG_TARGET_ARCH").as_deref() {
        Ok("aarch64") => "arm64",
        Ok("x86_64") => "x86_64",
        Ok(other) => panic!("unsupported macOS architecture for tcode-voice: {other}"),
        Err(error) => panic!("CARGO_CFG_TARGET_ARCH is unavailable: {error}"),
    };
    let target = format!("{arch}-apple-macosx26.0");
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("OUT_DIR is unavailable"));
    let library = out_dir.join("libtcode_voice_shim.a");
    let module_cache = out_dir.join("swift-module-cache");

    // The SDK's concurrency text stub contains `$ld$previous` directives that
    // rewrite its install name to @rpath when the final Rust executable keeps
    // an older deployment target. The shim itself is macOS 26-only and needs
    // the current Swift concurrency ABI, which macOS 26 provides in the dyld
    // cache at /usr/lib/swift. Keep the final app's deployment target intact,
    // but make its Swift autolink resolve to that system install name.
    let concurrency_stub_source =
        PathBuf::from(&sdk_path).join("usr/lib/swift/libswift_Concurrency.tbd");
    let mut concurrency_stub = fs::read_to_string(&concurrency_stub_source)
        .expect("failed to read the Swift concurrency text stub");
    while let Some(start) = concurrency_stub.find("'$ld$previous") {
        let end = concurrency_stub[start..]
            .find("',")
            .map(|offset| start + offset + 2)
            .expect("malformed $ld$previous directive in Swift concurrency text stub");
        concurrency_stub.replace_range(start..end, "");
    }
    fs::write(out_dir.join("libswift_Concurrency.tbd"), concurrency_stub)
        .expect("failed to write the adjusted Swift concurrency text stub");

    let status = Command::new(&swiftc)
        .args([
            "-parse-as-library",
            "-swift-version",
            "5",
            "-target",
            &target,
            "-sdk",
            &sdk_path,
            "-module-cache-path",
        ])
        .arg(&module_cache)
        .args(["-static", "-emit-library", "swift/shim.swift", "-o"])
        .arg(&library)
        .status()
        .expect("failed to invoke swiftc for tcode-voice");
    assert!(
        status.success(),
        "swiftc failed to compile swift/shim.swift"
    );

    println!("cargo:rustc-link-search=native={}", out_dir.display());
    println!("cargo:rustc-link-lib=static=tcode_voice_shim");
    for framework in ["Speech", "AVFoundation", "Foundation", "CoreMedia"] {
        println!("cargo:rustc-link-lib=framework={framework}");
    }

    println!("cargo:rustc-link-search=native=/usr/lib/swift");
    println!("cargo:rustc-link-search=native={sdk_path}/usr/lib/swift");
    println!("cargo:rustc-link-search=native={sdk_path}/usr/lib/swift/macosx");
    if let Some(toolchain) = PathBuf::from(swiftc).parent().and_then(|bin| bin.parent()) {
        println!(
            "cargo:rustc-link-search=native={}",
            toolchain.join("lib/swift/macosx").display()
        );
    }
    println!("cargo:rustc-cfg=voice_shim");
}
