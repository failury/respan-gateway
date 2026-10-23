//! Soniox's native REST transcription and speech APIs.

use respan_gateway_media_http::soniox::{SonioxSpeech, SonioxTranscriptions};
use respan_gateway_vendor_kit::Vendor;

/// Soniox's provider identity.
pub const ID: &str = "soniox";

/// Soniox, with separate REST origins for transcription and speech.
pub const VENDOR: Vendor = Vendor {
    id: ID,
    key_env: &["SONIOX_API_KEY"],
    formats: &[
        (&SonioxTranscriptions, Some("https://api.soniox.com/v1")),
        (&SonioxSpeech, Some("https://tts-rt.soniox.com")),
    ],
    ..Vendor::DEFAULT
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soniox_has_only_its_two_native_rest_formats_at_their_own_origins() {
        assert_eq!(VENDOR.id, "soniox");
        assert_eq!(VENDOR.key_env, ["SONIOX_API_KEY"]);
        assert_eq!(VENDOR.formats.len(), 2);
        for (name, base) in [
            (SonioxTranscriptions::NAME, "https://api.soniox.com/v1"),
            (SonioxSpeech::NAME, "https://tts-rt.soniox.com"),
        ] {
            let (format, origin) = VENDOR.format(name).expect("a native format");
            assert_eq!(origin, Some(base));
            assert_eq!(format.key(), respan_gateway_http::KeyIn::Bearer);
        }
        assert!(VENDOR.format("openai-chat").is_none());
    }
}
