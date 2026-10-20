//! OpenRouter's provider dialects of OpenAI Images and Videos. Their canonical
//! names let existing OpenAI clients route to them, with the vendor's own wire.

use crate::edge::{bind_provider, content, sends};
use crate::openai::{self, OpenAiImages, OpenAiVideos, images, videos};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use respan_gateway_conversation::{Media, MediaSource};
use respan_gateway_conversation_http::fields::{kept, merge_missing, set};
use respan_gateway_core::{Connection, Fault, Format, Provider};
use respan_gateway_http::bytes::Bytes;
use respan_gateway_http::content as http_content;
use respan_gateway_http::http::{Method, request::Parts as RequestHead};
use respan_gateway_http::{Connect, Decoder, Encoder, ProviderIo, Sse, Upstream, Whole};
use respan_gateway_media::{image, video};
use serde_json::{Map, Value, json};

/// OpenRouter's `/images`, presented to clients as OpenAI Images.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterImages;

impl Upstream for OpenRouterImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }
    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, image::Output, image::Input> for OpenRouterImages {
    fn name(&self) -> &str {
        OpenAiImages::NAME
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<image::Output, image::Input>, Fault> {
        let stream = openai::streams(&io, &["/images/generations", "/images/edits"], |v| {
            v.get("stream").and_then(Value::as_bool) == Some(true)
        });
        Ok(bind_provider(
            io,
            OpenAiImages::NAME,
            move |head, input| image_call(head, input, stream),
            |head, _| {
                Some(if http_content::is_stream(head) {
                    Box::new(Sse::new(ImageStream::default())) as Box<dyn Decoder<image::Output>>
                } else {
                    Box::new(Whole::new(read_image))
                })
            },
        ))
    }
}

fn image_call(
    head: &mut RequestHead,
    input: &image::Input,
    stream: bool,
) -> Result<Box<dyn Encoder<image::Input>>, Fault> {
    if input.mask.is_some() {
        return Err(Fault::bad_request(
            "OpenRouter images do not accept an OpenAI mask",
        ));
    }
    let mut fields = images::fields(input, stream)?;
    if !input.images.is_empty() {
        fields.insert("input_references".into(), references(&input.images).into());
    }
    openai::post_to(head, "/images", "application/json")?;
    Ok(sends(openai::json_bytes(&fields)?))
}

fn references(media: &[Media]) -> Vec<Value> {
    media
        .iter()
        .map(|media| {
            let url = match &media.source {
                MediaSource::Url(url) => url.clone(),
                MediaSource::Bytes(bytes) => {
                    format!("data:{};base64,{}", media.mime_type, STANDARD.encode(bytes))
                }
            };
            json!({"type": "image_url", "image_url": {"url": url}})
        })
        .collect()
}

fn object(raw: &[u8]) -> Result<Map<String, Value>, Fault> {
    serde_json::from_slice(raw)
        .map_err(|e| Fault::upstream("OpenRouter returned an invalid media object").with_source(e))
}

fn image_fields(fields: &mut Map<String, Value>) {
    if let Some(usage) = fields.get_mut("usage").and_then(Value::as_object_mut) {
        for (from, to) in [
            ("prompt_tokens", "input_tokens"),
            ("completion_tokens", "output_tokens"),
        ] {
            if let Some(value) = usage.get(from).cloned() {
                usage.entry(to).or_insert(value);
            }
        }
    }
    if let Some(format) = fields
        .get("media_type")
        .and_then(Value::as_str)
        .and_then(|mime| mime.strip_prefix("image/"))
    {
        fields.insert("output_format".into(), format.into());
    }
}

fn read_image(raw: Bytes, out: &mut Vec<image::Output>) -> Result<(), Fault> {
    let mut fields = object(&raw)?;
    image_fields(&mut fields);
    let start = out.len();
    images::decode_whole(openai::json_bytes(&fields)?, out)?;
    let mimes = fields
        .get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|item| item.get("media_type").and_then(Value::as_str));
    for (output, mime) in out[start..]
        .iter_mut()
        .filter_map(|o| match o {
            image::Output::Image(i) => Some(i),
            _ => None,
        })
        .zip(mimes)
    {
        if let Some(mime) = mime {
            output.media.mime_type = mime.into();
        }
    }
    Ok(())
}

#[derive(Default)]
struct ImageStream(images::Streamed);

impl Decoder<image::Output> for ImageStream {
    fn decode(&mut self, raw: Bytes, out: &mut Vec<image::Output>) -> Result<(), Fault> {
        let Some(event) = openai::event_json(OpenAiImages::NAME, &raw)? else {
            return Ok(());
        };
        let Value::Object(mut fields) = event else {
            return Err(Fault::upstream(
                "OpenRouter returned an invalid image event",
            ));
        };
        image_fields(&mut fields);
        if let Some(created) = fields.get("created").cloned() {
            fields.entry("created_at").or_insert(created);
        }
        self.0
            .event(openai::sse_event(&Value::Object(fields)).as_bytes(), out)
    }
    fn end(&mut self, end: Result<(), Fault>, _: &mut Vec<image::Output>) -> Result<(), Fault> {
        end?;
        self.0.end()
    }
}

/// OpenRouter's asynchronous Videos API, presented to OpenAI video clients.
#[derive(Debug, Clone, Copy)]
pub struct OpenRouterVideos;

impl Upstream for OpenRouterVideos {
    fn name(&self) -> &str {
        OpenAiVideos::NAME
    }
    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, *self, move |x| connect(x))
    }
}

impl Format<ProviderIo, video::Output, video::Input> for OpenRouterVideos {
    fn name(&self) -> &str {
        OpenAiVideos::NAME
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<video::Output, video::Input>, Fault> {
        Ok(bind_provider(
            io,
            OpenAiVideos::NAME,
            video_call,
            |_, sent| {
                Some(if sent.uri().path().ends_with("/content") {
                    content()
                } else {
                    Box::new(Whole::new(read_video))
                })
            },
        ))
    }
}

fn video_call(
    head: &mut RequestHead,
    input: &video::Input,
) -> Result<Box<dyn Encoder<video::Input>>, Fault> {
    let path = |id: &str| {
        format!(
            "/videos/{}",
            form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>()
        )
    };
    match input {
        video::Input::Create(create) => {
            let mut fields = Map::new();
            set(&mut fields, "model", create.model.clone());
            fields.insert("prompt".into(), create.prompt.clone().into());
            set(&mut fields, "duration", create.seconds);
            set(&mut fields, "size", create.size.clone());
            merge_missing(&mut fields, &kept(OpenAiVideos::NAME, &create.native)?);
            if let Some(reference) = &create.reference {
                let mut frames = references(std::slice::from_ref(reference));
                frames[0]["frame_type"] = "first_frame".into();
                fields.insert("frame_images".into(), frames.into());
            }
            openai::post_to(head, "/videos", "application/json")?;
            Ok(sends(openai::json_bytes(&fields)?))
        }
        video::Input::Retrieve { id } => {
            head.method = Method::GET;
            head.uri = path(id)
                .parse()
                .map_err(|e| Fault::internal("invalid video path").with_source(e))?;
            Ok(sends(Bytes::new()))
        }
        video::Input::Content {
            id,
            variant: video::Variant::Video,
            ..
        } => {
            head.method = Method::GET;
            head.uri = format!("{}/content", path(id))
                .parse()
                .map_err(|e| Fault::internal("invalid video content path").with_source(e))?;
            Ok(sends(Bytes::new()))
        }
        _ => Err(Fault::bad_request(
            "OpenRouter videos support create, retrieve and video content only",
        )),
    }
}

fn read_video(raw: Bytes, out: &mut Vec<video::Output>) -> Result<(), Fault> {
    let mut fields = object(&raw)?;
    if let Some(duration) = fields.get("duration").cloned() {
        fields.entry("seconds").or_insert(duration);
    }
    match fields.get("status").and_then(Value::as_str) {
        Some("pending") => {
            fields.insert("status".into(), "queued".into());
        }
        Some("cancelled" | "expired") => {
            let status = fields["status"].clone();
            if fields.get("error").is_none_or(Value::is_null) {
                fields.insert("error".into(), status.clone());
            }
            fields.insert("status".into(), "failed".into());
        }
        _ => {}
    }
    if let Some(message) = fields.get("error").and_then(Value::as_str) {
        let error = json!({"message": message});
        fields.insert("error".into(), error);
    }
    videos::decode_whole(openai::json_bytes(&fields)?, out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use respan_gateway_conversation::Usage;
    use respan_gateway_core::{Gateway, futures};
    use respan_gateway_http::http;
    use respan_gateway_http::{Body, http_body_util::BodyExt};
    use std::sync::{Arc, Mutex};

    fn body<I: 'static>(mut encoder: Box<dyn Encoder<I>>, input: &I) -> Value {
        let mut chunks = Vec::new();
        encoder.encode(input, &mut chunks).expect("encoded request");
        serde_json::from_slice(&chunks.concat()).expect("JSON request")
    }

    #[test]
    fn image_edits_use_references_and_masks_are_refused() {
        let mut input = image::Input::new("make it blue");
        input.images.push(Media::new(
            "image/jpeg",
            MediaSource::Bytes(Bytes::from_static(b"image")),
        ));
        let mut head = http::Request::new(()).into_parts().0;
        let request = body(
            image_call(&mut head, &input, false).expect("image call"),
            &input,
        );
        assert_eq!(head.uri.path(), "/images");
        assert_eq!(
            request["input_references"][0]["image_url"]["url"],
            "data:image/jpeg;base64,aW1hZ2U="
        );
        assert!(request.get("images").is_none());
        input.mask = input.images.first().cloned();
        assert!(image_call(&mut head, &input, false).is_err());
    }

    #[test]
    fn image_output_keeps_mime_tokens_and_reported_cost() {
        let mut out = Vec::new();
        read_image(
            Bytes::from(
                json!({
                    "created": 42,
                    "data": [{"b64_json": "aW1hZ2U=", "media_type": "image/jpeg"}],
                    "usage": {"prompt_tokens": 2, "completion_tokens": 3, "cost": 0.04}
                })
                .to_string(),
            ),
            &mut out,
        )
        .expect("image reply");
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Start(s) if s.created == Some(42)))
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Image(i) if i.media.mime_type == "image/jpeg"))
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Usage(u) if u == &Usage::new(2, 3)))
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Native(n) if n.value["usage"]["cost"] == 0.04))
        );
    }

    #[test]
    fn image_stream_normalizes_timestamp_usage_and_requires_completion() {
        let mut decoder = ImageStream::default();
        let mut out = Vec::new();
        decoder
            .decode(
                Bytes::from(openai::sse_event(&json!({
                    "type": "image_generation.completed", "created": 42,
                    "b64_json": "aW1hZ2U=", "media_type": "image/webp",
                    "usage": {"prompt_tokens": 2, "completion_tokens": 3, "cost": 0.04}
                }))),
                &mut out,
            )
            .expect("image event");
        decoder
            .decode(Bytes::from_static(b"data: [DONE]\n\n"), &mut out)
            .expect("done");
        decoder.end(Ok(()), &mut out).expect("complete");
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Start(s) if s.created == Some(42)))
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Image(i) if i.media.mime_type == "image/webp"))
        );
        assert!(
            out.iter()
                .any(|o| matches!(o, image::Output::Usage(u) if u == &Usage::new(2, 3)))
        );
        assert!(ImageStream::default().end(Ok(()), &mut Vec::new()).is_err());
    }

    #[test]
    fn video_creation_uses_integer_duration_and_references() {
        let mut create = video::Create::new("a wave");
        create.model = Some("google/veo-3.1-lite".into());
        create.seconds = Some(4);
        create.reference = Some(Media::new(
            "image/png",
            MediaSource::Url("https://example.com/frame.png".into()),
        ));
        let input = video::Input::Create(create);
        let mut head = http::Request::new(()).into_parts().0;
        let request = body(video_call(&mut head, &input).expect("video call"), &input);
        assert_eq!(head.uri.path(), "/videos");
        assert_eq!(request["duration"], 4);
        assert!(request.get("seconds").is_none());
        assert_eq!(request["frame_images"][0]["type"], "image_url");
        assert_eq!(request["frame_images"][0]["frame_type"], "first_frame");
        assert!(request.get("input_references").is_none());
        let input = video::Input::Content {
            id: "job/1".into(),
            variant: video::Variant::Video,
            url: None,
        };
        video_call(&mut head, &input).expect("content call");
        assert_eq!(head.method, Method::GET);
        assert_eq!(head.uri.path(), "/videos/job%2F1/content");
    }

    #[test]
    fn video_status_and_cost_survive_translation() {
        for (status, expected) in [
            ("pending", video::Status::Queued),
            ("completed", video::Status::Completed),
            ("cancelled", video::Status::Failed),
            ("expired", video::Status::Failed),
            ("failed", video::Status::Failed),
        ] {
            let mut out = Vec::new();
            read_video(
                Bytes::from(
                    json!({"id": "job", "status": status, "duration": 4,
                "error": "provider explanation", "usage": {"cost": 0.42}})
                    .to_string(),
                ),
                &mut out,
            )
            .expect("job");
            let video::Output::Job(job) = &out[0] else {
                panic!("job expected")
            };
            assert_eq!(job.status, expected);
            assert_eq!(job.seconds, Some(4));
            assert_eq!(
                job.error.as_ref().map(|e| e.message.as_str()),
                Some("provider explanation")
            );
            assert_eq!(
                job.native.as_ref().expect("native usage").fields["usage"]["cost"],
                0.42
            );
        }
    }

    #[tokio::test]
    async fn image_http_pipeline_calls_openrouter_and_returns_openai_output() {
        let sent = Arc::new(Mutex::new(None));
        let seen = sent.clone();
        let connect = respan_gateway_http::connect(
            move |request: http::Request<Body>| {
                let seen = seen.clone();
                async move {
                    let (head, bytes) = request.into_parts();
                    let bytes = bytes.collect().await?.to_bytes();
                    *seen.lock().expect("capture") = Some((
                        head.uri,
                        serde_json::from_slice::<Value>(&bytes).expect("request"),
                    ));
                    Ok((http::Response::builder().header("content-type", "application/json")
                        .body(Body::full(json!({"created":42,"data":[{"b64_json":"aW1hZ2U=","media_type":"image/jpeg"}],"usage":{"prompt_tokens":2,"completion_tokens":3,"cost":0.04}}).to_string())).expect("response"), None))
                }
            },
            1 << 20,
        );
        let gateway = Gateway::new()
            .provider(Upstream::provider(
                &OpenRouterImages,
                "openrouter".into(),
                Arc::new(connect),
            ))
            .expect("provider")
            .client::<image::Input, image::Output, _>(OpenAiImages)
            .expect("client")
            .target("openrouter")
            .expect("target");
        let request = http::Request::post("/v1/images/generations")
            .header("content-type", "application/json")
            .body(Body::full(
                json!({"model":"google/gemini-2.5-flash-image","prompt":"a wave"}).to_string(),
            ))
            .expect("request");
        let (response, done) = respan_gateway_http::handle(&gateway, request, 1 << 20).await;
        assert_eq!(response.status(), http::StatusCode::OK);
        let (bytes, finished) = futures::future::join(response.into_body().collect(), done).await;
        finished.expect("finished");
        let response: Value =
            serde_json::from_slice(&bytes.expect("body").to_bytes()).expect("response");
        assert_eq!(response["data"][0]["b64_json"], "aW1hZ2U=");
        assert_eq!(response["usage"]["input_tokens"], 2);
        assert_eq!(response["usage"]["cost"], 0.04);
        assert_eq!(
            sent.lock().expect("sent").as_ref().expect("call").0.path(),
            "/images"
        );
    }
}
