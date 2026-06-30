//! Generates prost message types for the outbox markers from `proto/outbox.proto`.
//!
//! `protox` parses the schema in pure Rust (no `protoc` binary required) and
//! produces a `FileDescriptorSet`, which `prost-build` turns into Rust code in
//! `OUT_DIR`.  This avoids depending on a system `protoc` while still driving
//! codegen from a real `.proto` schema.

use std::path::Path;

const PROTO: &str = "proto/outbox.proto";
const PROTO_INCLUDE: &str = "proto";

fn main() {
    println!("cargo:rerun-if-changed={PROTO}");

    let file_descriptors =
        protox::compile([PROTO], [PROTO_INCLUDE]).expect("failed to compile proto/outbox.proto");

    prost_build::Config::new()
        .compile_fds(file_descriptors)
        .expect("failed to generate prost types from descriptor set");

    // Fail fast if codegen silently produced nothing for the expected package.
    let generated = Path::new(&std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"))
        .join("nitinol.saga.outbox.rs");
    assert!(
        generated.exists(),
        "expected generated file {} was not produced",
        generated.display()
    );
}
