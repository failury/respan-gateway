//! Building blocks for vendor media dialects. Vendor-specific wire changes
//! belong in `crates/vendors`; these helpers retain the spec codecs, provider
//! error handling and stream framing without naming a vendor.

pub use crate::edge::{bind_provider, content, sends};
pub use crate::openai::images::{
    Streamed as ImageStream, decode_whole as decode_image, fields as image_fields,
};
pub use crate::openai::videos::decode_whole as decode_video;
pub use crate::openai::{event_json, json_bytes, post_to, sse_event, streams};
