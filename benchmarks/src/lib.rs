//! Reproducible retrieval-quality and performance measurements for SolaScope.
//!
//! The harness only drives the public [`solascope_engine::Engine`] API; it does
//! not reimplement retrieval. [`quality`] scores an annotated query set once per
//! retrieval variant the loaded configuration enables. [`perf`] measures
//! indexing, index size, peak resident memory and in-process search latency
//! against a throw-away index directory, leaving the project's own index alone.
//!
//! Pure logic lives in [`metrics`] (matching rule, rank metrics, nearest-rank
//! percentiles), [`queries`] (annotation parsing) and [`table`] (rendering).

pub mod metrics;
pub mod perf;
pub mod quality;
pub mod queries;
pub mod report;
pub mod table;
pub mod variants;

/// Receives progress lines and skip notices. The binary prints them to stderr
/// so that stdout carries nothing but the report.
pub type Progress<'a> = &'a mut dyn FnMut(&str);
