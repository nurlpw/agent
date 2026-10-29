fn main() {
    println!(
        "cargo:rustc-env=NURL_TARGET={}",
        std::env::var("TARGET").expect("cargo sets TARGET")
    );
    println!("cargo:rerun-if-changed=build.rs");
}
