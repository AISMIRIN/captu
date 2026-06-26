// Enable #[coverage(off)] when instrumented by cargo-llvm-cov (nightly only).
// This lets us mark externally-dependent functions (ffmpeg, FFI, server bootstrap)
// as "confirmed separately" without blocking the coverage gate.
#![cfg_attr(coverage_nightly, feature(coverage_attribute))]

pub mod config;
pub mod db;
pub mod ingest;
pub mod media;
pub mod routes;
pub mod scheduler;
pub mod ts;
