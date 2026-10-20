//! Format-neutral HTTP building blocks for vendor dialects.
//! Vendor-specific wire translation belongs in the vendor crate.

pub use crate::edge::{bind_provider, sends};
pub use crate::openai::post_to;
