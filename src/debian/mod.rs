//! Debian support for container bases: version ordering, control data, repository indexes, the
//! dependency closure and the pinned base (M-Spike S-2, LD-17).

pub mod base;
pub mod control;
pub mod gitblob;
pub mod index;
pub mod resolve;
pub mod version;
