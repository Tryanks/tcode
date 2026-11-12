//! Turns the GTK stack's link-time dependencies into run-time ones.
//!
//! The `-sys` crates name their libraries in `#[link]` attributes, so every
//! binary that reaches them would list `libwebkit2gtk-4.1.so.0`, `libgtk-3.so.0`
//! and the rest as `DT_NEEDED`, and the dynamic loader would refuse to start it
//! where they are missing. This script answers each of those `-l` flags with an
//! empty static archive of the same name, found ahead of the system's shared
//! library, and defines every function the system library exports as a
//! trampoline that jumps through a slot `gtk_dlopen::load` fills from
//! `dlsym`.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

use object::{Object as _, ObjectSymbol as _, SymbolKind};

/// The libraries the binary otherwise links, in the order they load: a library
/// after the ones it needs. Each is the `-l` name, the pkg-config module whose
/// `libdir` holds it, and the soname it is loaded by.
const LIBRARIES: &[(&str, &str, &str)] = &[
    ("glib-2.0", "glib-2.0", "libglib-2.0.so.0"),
    ("gobject-2.0", "gobject-2.0", "libgobject-2.0.so.0"),
    ("gio-2.0", "gio-2.0", "libgio-2.0.so.0"),
    ("cairo", "cairo", "libcairo.so.2"),
    ("gdk-3", "gdk-3.0", "libgdk-3.so.0"),
    ("gtk-3", "gtk+-3.0", "libgtk-3.so.0"),
    ("soup-3.0", "libsoup-3.0", "libsoup-3.0.so.0"),
    (
        "javascriptcoregtk-4.1",
        "javascriptcoregtk-4.1",
        "libjavascriptcoregtk-4.1.so.0",
    ),
    ("webkit2gtk-4.1", "webkit2gtk-4.1", "libwebkit2gtk-4.1.so.0"),
];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let out = PathBuf::from(env::var_os("OUT_DIR").expect("cargo sets OUT_DIR"));
    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();
    if target_os != "linux" {
        return;
    }

    let archives = out.join("link");
    fs::create_dir_all(&archives).expect("create the archive directory");
    // Searched before the linker's own directories, so `-lgtk-3` resolves to
    // this empty archive and leaves no DT_NEEDED behind.
    println!("cargo:rustc-link-search=native={}", archives.display());

    let mut names: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut libraries = String::new();
    for (link_name, module, soname) in LIBRARIES {
        fs::write(archives.join(format!("lib{link_name}.a")), b"!<arch>\n")
            .expect("write an empty archive");
        let path = shared_library(module, link_name);
        println!("cargo:rerun-if-changed={}", path.display());
        let first = names.len();
        for name in exported_functions(&path) {
            if seen.insert(name.clone()) {
                names.push(name);
            }
        }
        writeln!(libraries, "    (c\"{soname}\", {first}..{}),", names.len()).unwrap();
    }

    let mut rust = String::new();
    writeln!(rust, "pub(crate) const COUNT: usize = {};", names.len()).unwrap();
    writeln!(
        rust,
        "pub(crate) static LIBRARIES: &[(&core::ffi::CStr, core::ops::Range<usize>)] = &[\n{libraries}];"
    )
    .unwrap();
    rust.push_str("pub(crate) static NAMES: [&core::ffi::CStr; COUNT] = [\n");
    for name in &names {
        writeln!(rust, "    c\"{name}\",").unwrap();
    }
    rust.push_str("];\n");
    fs::write(out.join("symbols.rs"), rust).expect("write symbols.rs");

    let mut asm = String::new();
    for (index, name) in names.iter().enumerate() {
        let offset = index * 8;
        writeln!(asm, ".pushsection .text.gtk_dlopen.{name},\"ax\",%progbits").unwrap();
        writeln!(asm, ".globl {name}\n.hidden {name}\n.type {name},%function").unwrap();
        writeln!(asm, ".p2align 2\n{name}:").unwrap();
        match arch.as_str() {
            "x86_64" => writeln!(asm, "    jmp qword ptr [rip + {{slots}} + {offset}]").unwrap(),
            "aarch64" => writeln!(
                asm,
                "    adrp x16, {{slots}} + {offset}\n    ldr x16, [x16, :lo12:{{slots}} + {offset}]\n    br x16"
            )
            .unwrap(),
            other => panic!("gtk-dlopen has no trampoline for {other}"),
        }
        writeln!(asm, ".size {name}, . - {name}\n.popsection").unwrap();
    }
    fs::write(out.join("trampolines.s"), asm).expect("write trampolines.s");
}

/// The shared library `lib<link_name>.so` in `module`'s pkg-config `libdir`.
fn shared_library(module: &str, link_name: &str) -> PathBuf {
    let libdir = pkg_config::get_variable(module, "libdir")
        .unwrap_or_else(|error| panic!("pkg-config has no libdir for {module}: {error}"));
    let path = Path::new(&libdir).join(format!("lib{link_name}.so"));
    assert!(
        path.exists(),
        "{} is missing; install {module}'s development files",
        path.display()
    );
    path
}

/// The functions `path` exports by a C name. Names that begin with `_` are C++
/// or toolchain symbols no binding calls.
fn exported_functions(path: &Path) -> Vec<String> {
    let data = fs::read(path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()));
    let file = object::File::parse(&*data)
        .unwrap_or_else(|error| panic!("parse {}: {error}", path.display()));
    let mut names: Vec<String> = file
        .dynamic_symbols()
        .filter(|symbol| {
            symbol.kind() == SymbolKind::Text && symbol.is_definition() && symbol.is_global()
        })
        .filter_map(|symbol| symbol.name().ok().map(str::to_owned))
        .filter(|name| {
            !name.starts_with('_') && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        })
        .collect();
    names.sort();
    names.dedup();
    names
}
