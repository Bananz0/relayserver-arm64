
extern crate cc;
fn main() {
    println!("cargo:rerun-if-changed=src/c/relay.c");
    println!("cargo:rerun-if-changed=src/c/absdUser.c");
    println!("cargo:rustc-link-lib=dylib=MobileGestalt");
    println!("cargo:rustc-link-lib=framework=CoreFoundation");

    let mut b = cc::Build::new();
    if let Ok(ar) = std::env::var("AR_aarch64_apple_ios") {
        b.archiver(ar);
    } else if let Ok(ar) = std::env::var("AR") {
        b.archiver(ar);
    }

    b.file("src/c/relay.c")
     .file("src/c/absdUser.c")
     .compile("relay");
}