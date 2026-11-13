# Standalone expression library checks

`tidb_query_expr` uses the original `tidb-datatype` implementation from the fixed
TiDB Git revision in this directory's `Cargo.toml`. Cargo fetches its nested Rust
workspace, sibling leaf crates, build scripts and collation resources. A sibling
TiDB checkout is not required. Keep `Cargo.lock` committed; do not substitute a
second datatype implementation or an external Cargo path override.

Run from the SDK repository root with rustup's normal Cargo proxy. The repository
pins nightly-2026-08-22; its compiler meets the datatype workspace's Rust 1.97
minimum. This is a repository-wide toolchain change, but these checks validate
only the expression library, not all TiKV packages, platforms or CI jobs.

```sh
rustup show active-toolchain
rustc --version --verbose
cargo metadata --locked --format-version 1 > metadata.json
cargo tree --locked -p tidb_query_expr -i tidb-datatype
cargo build --locked -p tidb_query_expr --lib
cargo test --locked -p tidb_query_expr --lib
```

Native dependencies require a C/C++ toolchain, CMake, make, pkg-config, protoc and
OpenSSL development files. On the validated Gentoo host, the existing gRPC/native
source closure was built with GCC/G++14 and these explicit compatibility settings:

```sh
export CC=gcc-14 CXX=g++-14
export CXXFLAGS='-include cstdint'
export CMAKE_POLICY_VERSION_MINIMUM=3.5
export CARGO_BUILD_JOBS=4
```

These are system build prerequisites, not replacements for the declared Rust
toolchain. Other platforms may need their usual native dependency setup.

When building the native TiDB workspace together with this SDK, TiDB's root
`rust/Cargo.toml` patches the same Git source to its local `tidb-datatype` owner.
Check metadata/tree for one datatype package and matching leaf identities before
running shared expression tests. The standalone SDK has no such patch.
