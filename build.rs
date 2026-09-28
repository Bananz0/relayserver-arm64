
extern crate cc;
fn main() {
    println!("cargo:rerun-if-changed=src/c/relay.c");
    println!("cargo:rerun-if-changed=src/c/absdUser.c");
    let target = std::env::var("TARGET").unwrap_or_default();
    let is_ios = target.contains("apple-ios");

    if is_ios {
        println!("cargo:rustc-link-lib=dylib=MobileGestalt");
        println!("cargo:rustc-link-lib=framework=CoreFoundation");

        let mut b = cc::Build::new();
        if let Ok(clang) = std::env::var(format!("CC_{}", target.replace('-', "_"))) {
            b.compiler(clang);
        } else if let Ok(clang) = std::env::var("CC") {
            b.compiler(clang);
        }

        if let Ok(ar) = std::env::var(format!("AR_{}", target.replace('-', "_"))) {
            b.archiver(ar);
        } else if let Ok(ar) = std::env::var("AR") {
            b.archiver(ar);
        }

        b.file("src/c/relay.c")
         .file("src/c/absdUser.c")
         .compile("relay");
    }
}