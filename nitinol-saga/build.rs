//! Generates prost message types for the saga marker events from the
//! `proto/*.proto` schemas (outbox markers and schedule markers).
//!
//! `protox` parses the schema in pure Rust (no `protoc` binary required) and
//! produces a `FileDescriptorSet`, which `prost-build` turns into Rust code in
//! `OUT_DIR`.  This avoids depending on a system `protoc` while still driving
//! codegen from a real `.proto` schema.

use std::path::Path;

const PROTOS: [&str; 2] = ["proto/outbox.proto", "proto/schedule.proto"];
const PROTO_INCLUDE: &str = "proto";
const GENERATED: [&str; 2] = ["nitinol.saga.outbox.rs", "nitinol.saga.schedule.rs"];

fn main() {
    for proto in PROTOS {
        println!("cargo:rerun-if-changed={proto}");
    }

    let file_descriptors = protox::compile(PROTOS, [PROTO_INCLUDE])
        .expect("failed to compile the saga proto schemas");

    prost_build::Config::new()
        .compile_fds(file_descriptors)
        .expect("failed to generate prost types from descriptor set");

    // Fail fast if codegen silently produced nothing for an expected package.
    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo");
    for generated in GENERATED {
        let path = Path::new(&out_dir).join(generated);
        assert!(
            path.exists(),
            "expected generated file {} was not produced",
            path.display()
        );
    }
}
