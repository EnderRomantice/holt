//! Public API surface — `Tree`, `AtomicBatch`, `Record`,
//! `RecordVersion`,
//! `TreeBuilder`, plus the curated [`stats`] module.
//!
//! This module is what users will write `use holt::{...}` for.

pub mod atomic;
pub mod builder;
pub mod config;
pub mod errors;
pub mod stats;
pub mod tree;
