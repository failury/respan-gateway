//! Google's native image API as a vendor dialect of OpenAI Images.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource, Usage};
use respan_gateway_conversation_http::fields::{invalid, kept};
use respan_gateway_core::{Connection, Fault, Format, NativeFields, NativeValue, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::http::request::Parts as RequestHead;
use respan_gateway_http::{Connect, Encoder, KeyIn, ProviderIo, Upstream, Whole};
use respan_gateway_media::image::{Image, Input, Output, Start};
use respan_gateway_media_http::dialect::{bind_provider, post_to, sends};
use respan_gateway_media_http::openai::OpenAiImages;
use serde_json::{Map, Value, json};

/// Gemini's `generateContent` image API, using the canonical Images kind.
#[derive(Debug, Clone, Copy)]
pub struct GeminiImages;

impl Upstream for GeminiImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }
    fn key(&self) -> KeyIn {
        KeyIn::Header("x-goog-api-key")
    }
    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, Output, Input> for GeminiImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(bind_provider(io, OpenAiImages::NAME, call, |_, _| {
            Some(Box::new(Whole::new(decode)))
        }))
    }
}

fn call(head: &mut RequestHead, input: &Input) -> Result<Box<dyn Encoder<Input>>, Fault> {
    let model = input
        .model
        .as_deref()
        .filter(|m| !m.is_empty())
        .ok_or_else(|| invalid("model", "is required for Gemini images"))?;
    if input.prompt.trim().is_empty() {
        return Err(invalid("prompt", "must not be empty"));
    }
    if input.count.is_some_and(|n| n != 1) {
        return Err(invalid("n", "must be 1 for Gemini images"));
    }
    if input.mask.is_some() {
        return Err(invalid("mask", "is not supported by Gemini images"));
    }
    if input.quality.as_deref().is_some_and(|q| q != "auto") {
        return Err(invalid("quality", "must be auto for Gemini images"));
    }
    if input.output_format.is_some() {
        return Err(invalid("output_format", "is chosen by Gemini"));
    }
    if input.user.is_some() {
        return Err(invalid("user", "is not supported by Gemini images"));
    }
    let mut fields = kept(OpenAiImages::NAME, &input.native)?;
    if let Some(format) = fields.remove("response_format").filter(|v| !v.is_null())
        && format != "b64_json"
    {
        return Err(invalid("response_format", "must be b64_json"));
    }
    let mut config = json!({"responseModalities": ["IMAGE"]});
    let size_ratio = match input.size.as_deref() {
        None | Some("auto") => None,
        Some("1024x1024") => Some("1:1"),
        Some("1536x1024") => Some("3:2"),
        Some("1024x1536") => Some("2:3"),
        Some(_) => {
            return Err(invalid(
                "size",
                "must be auto, 1024x1024, 1536x1024 or 1024x1536; use aspect_ratio for native ratios",
            ));
        }
    };
    if let Some(ratio) = fields.remove("aspect_ratio") {
        let ratio = ratio
            .as_str()
            .ok_or_else(|| invalid("aspect_ratio", "must be a string"))?;
        if size_ratio.is_some_and(|s| s != ratio) {
            return Err(invalid("aspect_ratio", "conflicts with size"));
        }
        config["imageConfig"] = json!({"aspectRatio": ratio});
    } else if let Some(ratio) = size_ratio {
        config["imageConfig"] = json!({"aspectRatio": ratio});
    }
    if let Some((key, _)) = fields.iter().next() {
        return Err(invalid(key, "is not supported by Gemini images"));
    }
    let mut parts = vec![json!({"text": input.prompt})];
    for image in &input.images {
        let part = match &image.source {
            MediaSource::Bytes(bytes) => {
                json!({"inlineData": {"mimeType": image.mime_type, "data": STANDARD.encode(bytes)}})
            }
            MediaSource::Url(url) => {
                json!({"fileData": {"mimeType": image.mime_type, "fileUri": url}})
            }
        };
        parts.push(part);
    }
    const PATH_SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
        .remove(b'-')
        .remove(b'.')
        .remove(b'_')
        .remove(b'~');
    let model = percent_encoding::utf8_percent_encode(model, PATH_SEGMENT);
    post_to(
        head,
        &format!("/models/{model}:generateContent"),
        "application/json",
    )?;
    let body = json!({"contents": [{"role": "user", "parts": parts}], "generationConfig": config});
    Ok(sends(Bytes::from(body.to_string())))
}

/// The modality split, preserving absence rather than claiming a measured zero.
fn modalities(value: &Value) -> Map<String, Value> {
    let mut counts = Map::new();
    for item in value.as_array().into_iter().flatten() {
        let key = match item["modality"]
            .as_str()
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("TEXT" | "DOCUMENT") => "text_tokens",
            Some("IMAGE") => "image_tokens",
            Some("AUDIO") => "audio_tokens",
            Some("VIDEO") => "video_tokens",
            _ => continue,
        };
        let count = item["tokenCount"].as_u64().unwrap_or(0);
        let previous = counts.get(key).and_then(Value::as_u64).unwrap_or(0);
        counts.insert(key.into(), previous.saturating_add(count).into());
    }
    counts
}

fn usage(value: &Value) -> Result<(Usage, Map<String, Value>), Fault> {
    let count = |key: &str| match &value[key] {
        Value::Null => Ok(0),
        v => v
            .as_u64()
            .ok_or_else(|| Fault::upstream(format!("Gemini {key} is not a token count"))),
    };
    let input = count("promptTokenCount")?;
    let candidates = count("candidatesTokenCount")?;
    let thoughts = count("thoughtsTokenCount")?;
    let output = if input.saturating_add(candidates) == count("totalTokenCount")? {
        candidates
    } else {
        candidates.saturating_add(thoughts)
    };
    let cached = count("cachedContentTokenCount")?;
    let counts = Usage {
        input_tokens: input,
        output_tokens: output,
        cached_input_tokens: cached,
        reasoning_tokens: thoughts,
        ..Usage::default()
    };
    let mut output_details = modalities(&value["candidatesTokensDetails"]);
    let nontext = ["image_tokens", "audio_tokens", "video_tokens"]
        .iter()
        .fold(0u64, |sum, key| {
            sum.saturating_add(
                output_details
                    .get(*key)
                    .and_then(Value::as_u64)
                    .unwrap_or(0),
            )
        });
    output_details
        .entry("text_tokens")
        .or_insert(candidates.saturating_sub(nontext).into());
    output_details.insert("reasoning_tokens".into(), thoughts.into());
    let mut input_details = modalities(&value["promptTokensDetails"]);
    let cache_details = modalities(&value["cacheTokensDetails"]);
    for (key, count) in &mut input_details {
        let cached = cache_details
            .get(key)
            .and_then(Value::as_u64)
            .unwrap_or_else(|| {
                if key == "text_tokens" && value.get("cacheTokensDetails").is_none() {
                    cached
                } else {
                    0
                }
            });
        *count = count.as_u64().unwrap_or(0).saturating_sub(cached).into();
    }
    input_details.insert("cached_tokens".into(), cached.into());
    let extra =
        json!({"input_tokens_details": input_details, "output_tokens_details": output_details});
    Ok((counts, extra.as_object().cloned().unwrap_or_default()))
}

fn decode(raw: Bytes, out: &mut Vec<Output>) -> Result<(), Fault> {
    let value: Value = serde_json::from_slice(&raw)
        .map_err(|e| Fault::upstream("Gemini returned invalid image JSON").with_source(e))?;
    if !value["promptFeedback"]["blockReason"].is_null() {
        return Err(Fault::upstream("Gemini blocked the image prompt"));
    }
    if !value["candidates"].is_array() {
        return Err(Fault::upstream("Gemini returned no image candidates"));
    }
    let mut events = vec![Output::Start(Start::new(None))];
    for candidate in value["candidates"].as_array().into_iter().flatten() {
        for part in candidate["content"]["parts"]
            .as_array()
            .into_iter()
            .flatten()
        {
            if part["thought"] == true {
                continue;
            }
            let inline = &part["inlineData"];
            if inline.is_null() {
                continue;
            }
            let mime = inline["mimeType"]
                .as_str()
                .filter(|m| m.starts_with("image/"))
                .ok_or_else(|| Fault::upstream("Gemini returned a non-image part"))?;
            let data = inline["data"]
                .as_str()
                .and_then(|d| STANDARD.decode(d).ok())
                .filter(|d| !d.is_empty())
                .ok_or_else(|| Fault::upstream("Gemini returned invalid image bytes"))?;
            let mut image = Image::new(Media::new(mime, MediaSource::Bytes(data.into())));
            image.native = Some(NativeFields::new(
                OpenAiImages::NAME,
                json!({"media_type": mime})
                    .as_object()
                    .cloned()
                    .unwrap_or_default(),
            ));
            events.push(Output::Image(image));
        }
    }
    let (counts, extra) = usage(&value["usageMetadata"])?;
    let has_image = events.iter().any(|e| matches!(e, Output::Image(_)));
    if !has_image
        && let Some(candidate) =
            value["candidates"]
                .as_array()
                .into_iter()
                .flatten()
                .find(|candidate| {
                    matches!(
                        candidate["finishReason"].as_str(),
                        Some("IMAGE_RECITATION" | "IMAGE_SAFETY" | "IMAGE_PROHIBITED_CONTENT")
                    )
                })
    {
        let reason = candidate["finishReason"].as_str().unwrap_or_default();
        let message = candidate["finishMessage"]
            .as_str()
            .unwrap_or("The provider did not return an image.");
        return Err(Fault::upstream(format!(
            "Gemini image generation refused ({reason}): {message}"
        )));
    }
    if !has_image && counts.is_empty() {
        return Err(Fault::upstream(
            "Gemini returned neither an image nor billable usage",
        ));
    }
    if value["usageMetadata"].is_object() {
        events.push(Output::Usage(counts));
        events.push(Output::Native(NativeValue::new(
            OpenAiImages::NAME,
            json!({"usage": extra}),
        )));
    }
    out.extend(events);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_core::{Gateway, futures};
    use respan_gateway_http::{Body, http, http_body_util::BodyExt};
    use std::sync::{Arc, Mutex};

    fn encoded(input: &Input) -> Result<(String, Value), Fault> {
        let mut head = http::Request::new(()).into_parts().0;
        let mut encoder = call(&mut head, input)?;
        let mut chunks = Vec::new();
        encoder.encode(input, &mut chunks)?;
        Ok((
            head.uri.to_string(),
            serde_json::from_slice(&chunks.concat()).expect("JSON body"),
        ))
    }

    fn input() -> Input {
        let mut input = Input::new("a blue circle");
        input.model = Some("gemini-3.1-flash-image".into());
        input.size = Some("1024x1024".into());
        input.quality = Some("auto".into());
        input.count = Some(1);
        input
    }

    fn answer() -> Value {
        json!({"candidates": [{"content": {"parts": [
            {"thought": true, "inlineData": {"mimeType": "image/png", "data": "cHJldmlldw=="}},
            {"text": "Here it is"},
            {"inlineData": {"mimeType": "image/jpeg", "data": "aW1hZ2U="}}
        ]}, "finishReason": "STOP"}], "usageMetadata": {
            "promptTokenCount": 10, "candidatesTokenCount": 100, "thoughtsTokenCount": 5, "totalTokenCount": 115,
            "cachedContentTokenCount": 2,
            "promptTokensDetails": [{"modality": "TEXT", "tokenCount": 4}, {"modality": "IMAGE", "tokenCount": 6}],
            "candidatesTokensDetails": [{"modality": "IMAGE", "tokenCount": 90}, {"modality": "TEXT", "tokenCount": 10}]
        }})
    }

    #[test]
    fn canonical_image_request_uses_native_generate_content_and_inline_edits() {
        let mut input = input();
        input.images.push(Media::new(
            "image/png",
            MediaSource::Bytes(Bytes::from_static(b"source")),
        ));
        let (path, body) = encoded(&input).expect("request");
        assert_eq!(path, "/models/gemini-3.1-flash-image:generateContent");
        assert_eq!(body["contents"][0]["parts"][0]["text"], "a blue circle");
        assert_eq!(
            body["contents"][0]["parts"][1]["inlineData"]["data"],
            "c291cmNl"
        );
        assert_eq!(
            body["generationConfig"],
            json!({"responseModalities": ["IMAGE"], "imageConfig": {"aspectRatio": "1:1"}})
        );
        assert!(body.get("quality").is_none());
        assert!(matches!(
            Upstream::key(&GeminiImages),
            KeyIn::Header("x-goog-api-key")
        ));
    }

    #[test]
    fn unsupported_image_controls_are_refused_before_calling_the_provider() {
        for edit in [0, 1, 2, 3, 4] {
            let mut input = input();
            match edit {
                0 => input.count = Some(2),
                1 => input.mask = Some(Media::new("image/png", MediaSource::Bytes(Bytes::new()))),
                2 => input.quality = Some("high".into()),
                3 => input.output_format = Some("webp".into()),
                _ => {
                    input.native = Some(NativeFields::new(
                        OpenAiImages::NAME,
                        json!({"unrecognized": true})
                            .as_object()
                            .cloned()
                            .unwrap_or_default(),
                    ))
                }
            }
            assert!(encoded(&input).is_err());
        }
    }

    #[test]
    fn final_images_keep_mime_and_usage_without_thought_previews() {
        let mut out = Vec::new();
        decode(Bytes::from(answer().to_string()), &mut out).expect("images");
        let images: Vec<_> = out
            .iter()
            .filter_map(|event| match event {
                Output::Image(image) => Some(image),
                _ => None,
            })
            .collect();
        assert_eq!(images.len(), 1);
        assert_eq!(images[0].media.mime_type, "image/jpeg");
        assert!(
            matches!(&images[0].media.source, MediaSource::Bytes(bytes) if bytes.as_ref() == b"image")
        );
        assert!(out.iter().any(|event| matches!(event, Output::Usage(u) if u.input_tokens == 10 && u.output_tokens == 105 && u.cached_input_tokens == 2 && u.reasoning_tokens == 5)));
        let extra = out
            .iter()
            .find_map(|event| match event {
                Output::Native(native) => Some(&native.value["usage"]),
                _ => None,
            })
            .expect("usage details");
        assert_eq!(extra["input_tokens_details"]["text_tokens"], 2);
        assert_eq!(extra["input_tokens_details"]["image_tokens"], 6);
        assert_eq!(extra["output_tokens_details"]["image_tokens"], 90);
    }

    #[test]
    fn inclusive_thoughts_and_modality_specific_cache_are_not_counted_twice() {
        let mut value = answer()["usageMetadata"].clone();
        value["totalTokenCount"] = 110.into();
        value["cacheTokensDetails"] = json!([{"modality": "IMAGE", "tokenCount": 2}]);
        let (counts, extras) = usage(&value).expect("usage");
        assert_eq!(counts.output_tokens, 100);
        assert_eq!(extras["input_tokens_details"]["image_tokens"], 4);
        assert_eq!(extras["input_tokens_details"]["text_tokens"], 4);
    }

    #[test]
    fn malformed_and_blocked_outputs_fail_but_text_only_usage_survives() {
        let mut broken = answer();
        broken["candidates"][0]["content"]["parts"][2]["inlineData"]["data"] = "!!!".into();
        for value in [
            broken,
            json!({"promptFeedback": {"blockReason": "SAFETY"}}),
            json!({"candidates": []}),
        ] {
            let mut out = Vec::new();
            assert!(decode(Bytes::from(value.to_string()), &mut out).is_err());
            assert!(out.is_empty());
        }
        let mut text = answer();
        text["candidates"][0]["content"]["parts"] = json!([{"text": "No image was made"}]);
        let mut out = Vec::new();
        decode(Bytes::from(text.to_string()), &mut out).expect("billed text answer");
        assert!(!out.iter().any(|o| matches!(o, Output::Image(_))));
        assert!(out.iter().any(|o| matches!(o, Output::Usage(_))));
    }

    #[test]
    fn image_policy_refusals_do_not_succeed_on_prompt_usage_alone() {
        for reason in [
            "IMAGE_RECITATION",
            "IMAGE_SAFETY",
            "IMAGE_PROHIBITED_CONTENT",
        ] {
            let response = json!({
                "candidates": [{"content": {}, "finishReason": reason,
                    "finishMessage": "The image could not be generated. You will not be charged."}],
                "usageMetadata": {"promptTokenCount": 15, "totalTokenCount": 15}
            });
            let mut out = Vec::new();
            let error =
                decode(Bytes::from(response.to_string()), &mut out).expect_err("provider refusal");
            assert!(error.message().contains(reason));
            assert!(error.message().contains("You will not be charged"));
            assert!(out.is_empty());
        }
    }

    #[tokio::test]
    async fn openai_image_client_receives_native_gemini_images_and_usage() {
        let sent = Arc::new(Mutex::new(None));
        let seen = sent.clone();
        let connect = respan_gateway_http::connect(
            move |request: http::Request<Body>| {
                let seen = seen.clone();
                async move {
                    let (head, body) = request.into_parts();
                    let body = body.collect().await?.to_bytes();
                    *seen.lock().expect("capture") = Some((
                        head.uri,
                        serde_json::from_slice::<Value>(&body).expect("request"),
                    ));
                    Ok((
                        http::Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::full(answer().to_string()))
                            .expect("response"),
                        None,
                    ))
                }
            },
            1 << 20,
        );
        let gateway = Gateway::new()
            .provider(Upstream::provider(
                &GeminiImages,
                "gemini".into(),
                Arc::new(connect),
            ))
            .expect("provider")
            .client::<Input, Output, _>(OpenAiImages)
            .expect("client")
            .target("gemini")
            .expect("target");
        let request = http::Request::post("/v1/images/generations").header("content-type", "application/json")
            .body(Body::full(json!({"model": "gemini-3.1-flash-image", "prompt": "a blue circle", "size": "1024x1024", "quality": "auto", "n": 1}).to_string())).expect("request");
        let (response, done) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        let (body, finished) = futures::future::join(response.into_body().collect(), done).await;
        finished.expect("finished");
        let body: Value = serde_json::from_slice(&body.expect("body").to_bytes()).expect("JSON");
        assert_eq!(body["data"][0]["b64_json"], "aW1hZ2U=");
        assert_eq!(body["data"][0]["media_type"], "image/jpeg");
        assert_eq!(body["usage"]["input_tokens"], 10);
        assert_eq!(body["usage"]["output_tokens"], 105);
        assert_eq!(body["usage"]["output_tokens_details"]["image_tokens"], 90);
        assert!(
            sent.lock()
                .expect("capture")
                .as_ref()
                .expect("request")
                .0
                .path()
                .ends_with(":generateContent")
        );
    }
}
