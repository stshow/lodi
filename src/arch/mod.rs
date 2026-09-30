//! Arch Linux repository metadata and package-version semantics.
//!
//! The database parser, comparator, bounded resolver, the base layer's first two lock-time
//! family operations and the image half of the seam are present, but the family has no complete
//! runtime implementation yet: nothing joins the lock-time half to the image half, and the later
//! convergence package is the only package allowed to make Arch reachable from the CLI.

pub mod base;
pub mod db;
pub mod image;
pub mod resolve;
pub mod version;
