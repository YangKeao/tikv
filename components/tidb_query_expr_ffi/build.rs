// Copyright 2026 TiKV Project Authors. Licensed under Apache-2.0.

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        // rustc's cdylib export list exposes only our no_mangle API. Additionally
        // localize every native archive definition, including OpenSSL and gRPC,
        // so they cannot interpose on TiFlash's unrelated C++/TLS dependencies.
        // This does not isolate undefined imports: audit the finished ELF too.
        println!("cargo:rustc-cdylib-link-arg=-Wl,--exclude-libs,ALL");
    } else {
        panic!("tidb_query_expr_ffi isolation currently requires Linux ELF");
    }
}
