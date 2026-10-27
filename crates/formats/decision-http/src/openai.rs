//! The OpenAI Decisions JSON contract.
use super::*;
use respan_gateway_core::NativeFields;
use std::borrow::Cow;

/// OpenAI Decisions, with a host-selected client path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenAiDecisions {
    path: Cow<'static, str>,
}
impl Default for OpenAiDecisions {
    fn default() -> Self {
        Self::DEFAULT
    }
}
impl OpenAiDecisions {
    /// Default client path.
    pub const DEFAULT: Self = Self {
        path: Cow::Borrowed("/v1/decisions"),
    };
    /// Format identity used by routing and native fields.
    pub const NAME: &'static str = "openai-decisions";
    /// Serve clients at this exact path.
    pub fn serving(path: impl Into<String>) -> Self {
        Self {
            path: Cow::Owned(path.into()),
        }
    }
}
impl Format<ClientIo, Input, Output> for OpenAiDecisions {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn matches(&self, io: &ClientIo) -> bool {
        io.request().method() == Method::POST && io.request().uri().path() == self.path
    }
    fn bind(&self, io: ClientIo) -> Result<Connection<Input, Output>, Fault> {
        Ok(io.with_error(refusal).bind(
            |_| {
                Box::new(|raw: Bytes, out: &mut Vec<Input>| {
                    out.push(request(&raw)?);
                    Ok(())
                })
            },
            |reply| {
                let mut head = reply.head();
                head.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                Ok(reply.write(head, Box::new(All::new(response))))
            },
        ))
    }
}
impl Upstream for OpenAiDecisions {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn provider(&self, name: String, connect: Connect) -> Provider {
        Provider::new(name, self.clone(), move |x| connect(x))
    }
}
impl Format<ProviderIo, Output, Input> for OpenAiDecisions {
    fn name(&self) -> &str {
        Self::NAME
    }
    fn bind(&self, io: ProviderIo) -> Result<Connection<Output, Input>, Fault> {
        Ok(io.with_refusal(read_provider_error).bind(
            |head, input: Option<&Input>| {
                let input = input.ok_or_else(|| {
                    Fault::config("openai-decisions does not accept streaming inputs")
                })?;
                head.method = Method::POST;
                set_path(head, "/decisions")?;
                head.headers
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
                let body = to_bytes(&call(input)?)?;
                Ok(Box::new(move |_: &Input, out: &mut Vec<Bytes>| {
                    out.push(body.clone());
                    Ok(())
                }) as Box<dyn Encoder<Input>>)
            },
            |_, req| {
                let questions = serde_json::from_slice::<Value>(req.body())
                    .ok()
                    .and_then(|v| v["questions"].as_array().cloned())
                    .unwrap_or_default();
                Some(
                    Box::new(Whole::new(move |raw: Bytes, out: &mut Vec<Output>| {
                        answer(&raw, &questions, out)
                    })) as Box<dyn Decoder<Output>>,
                )
            },
        ))
    }
}
fn tag(map: Map<String, Value>) -> Option<NativeFields> {
    Some(NativeFields::new(OpenAiDecisions::NAME, map))
}
fn own(n: &Option<NativeFields>) -> Result<Map<String, Value>, Fault> {
    match n {
        Some(n) if &*n.format == OpenAiDecisions::NAME => Ok(n.fields.clone()),
        _ => Err(Fault::bad_request("foreign or untagged decision evidence")),
    }
}
fn choice(v: &Value) -> Option<ChoiceValue> {
    match v {
        Value::String(s) => Some(ChoiceValue::String(s.clone())),
        Value::Bool(b) => Some(ChoiceValue::Boolean(*b)),
        _ => None,
    }
}
fn choice_json(v: &ChoiceValue) -> Value {
    match v {
        ChoiceValue::String(s) => s.clone().into(),
        ChoiceValue::Boolean(b) => (*b).into(),
    }
}
fn request(raw: &[u8]) -> Result<Input, Fault> {
    let mut obj = serde_json::from_slice::<Value>(raw)
        .ok()
        .and_then(|v| v.as_object().cloned())
        .ok_or_else(|| invalid("body", "must be an object"))?;
    if obj.get("stream").is_some_and(|v| v != &Value::Bool(false)) {
        return Err(invalid("stream", "is not supported"));
    }
    if obj.contains_key("state") {
        return Err(invalid("state", "is not supported"));
    }
    let state = obj
        .remove("input")
        .ok_or_else(|| invalid("input", "is required"))?;
    validate_input(&state)?;
    let qs = obj
        .remove("questions")
        .and_then(|v| v.as_array().cloned())
        .ok_or_else(|| invalid("questions", "must be an array"))?;
    if qs.is_empty() || qs.len() > 400 {
        return Err(invalid("questions", "must contain 1 to 400 questions"));
    }
    let mut questions = Vec::new();
    for (i, v) in qs.into_iter().enumerate() {
        let mut q = v
            .as_object()
            .cloned()
            .ok_or_else(|| invalid("questions", "must contain objects"))?;
        if !matches!(
            q.get("name"),
            None | Some(Value::Null) | Some(Value::String(_))
        ) {
            return Err(invalid("name", "must be a string or null"));
        }
        let instructions = q
            .remove("instructions")
            .ok_or_else(|| invalid("instructions", "is required"))?;
        if !(instructions.is_string() || instructions.is_array()) {
            return Err(invalid("instructions", "must be text or parts"));
        }
        let ask = match q.remove("type").as_ref().and_then(Value::as_str) {
            Some("predicate") => Ask::Noul {
                yes: None,
                no: None,
            },
            Some("choice") => {
                let choices = q
                    .remove("choices")
                    .and_then(|v| v.as_array().cloned())
                    .ok_or_else(|| invalid("choices", "must be an array"))?;
                let mut options = Vec::new();
                q.insert("choices".into(), choices.clone().into());
                for c in choices {
                    let o = c
                        .as_object()
                        .ok_or_else(|| invalid("choices", "must contain objects"))?;
                    let value = o
                        .get("value")
                        .and_then(choice)
                        .ok_or_else(|| invalid("value", "must be a string or boolean"))?;
                    if options.iter().any(|(v, _)| v == &value) {
                        return Err(invalid("choices", "must be unique"));
                    }
                    if o.get("description").is_some_and(|v| !v.is_string()) {
                        return Err(invalid("description", "must be a string"));
                    }
                    options.push((value, o.get("description").cloned()));
                }
                if !(2..=255).contains(&options.len()) {
                    return Err(invalid("choices", "must contain 2 to 255 choices"));
                }
                Ask::Choice { options }
            }
            Some("score") => {
                let levels = q
                    .remove("levels")
                    .and_then(|v| v.as_array().cloned())
                    .ok_or_else(|| invalid("levels", "must be an array"))?;
                if levels.iter().any(|v| !v["label"].is_string()) {
                    return Err(invalid("levels", "must have string labels"));
                }
                Ask::Score { levels }
            }
            _ => return Err(invalid("type", "must be predicate, choice or score")),
        };
        let mut question = Question::new(i.to_string(), ask);
        question.instructions = Some(instructions);
        question.native = tag(q);
        questions.push(question);
    }
    let mut input = Input::new(state, questions);
    input.model = match obj.remove("model") {
        Some(Value::String(s)) => Some(s),
        _ => return Err(invalid("model", "must be a string")),
    };
    input.native = tag(obj);
    Ok(input)
}
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Evidence {
    Text(String),
    Messages(Vec<UserMessage>),
}
#[derive(serde::Deserialize)]
struct UserMessage {
    role: UserRole,
    content: Content,
    #[serde(rename = "type")]
    kind: Option<MessageType>,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum UserRole {
    User,
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "snake_case")]
enum MessageType {
    Message,
}
#[derive(serde::Deserialize)]
#[serde(untagged)]
enum Content {
    Text(String),
    Parts(Vec<Part>),
}
#[derive(serde::Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    InputText {
        text: String,
    },
    InputImage {
        image_url: Option<String>,
        file_id: Option<String>,
    },
}
fn validate_input(v: &Value) -> Result<(), Fault> {
    let evidence: Evidence = serde_json::from_value(v.clone()).map_err(|_| {
        invalid(
            "input",
            "must be text or user messages with text or image parts",
        )
    })?;
    if let Evidence::Messages(messages) = evidence {
        for m in messages {
            let _ = (m.role, m.kind);
            match m.content {
                Content::Text(text) => {
                    let _ = text;
                }
                Content::Parts(parts) => {
                    for part in parts {
                        match part {
                            Part::InputText { text } => {
                                let _ = text;
                            }
                            Part::InputImage { image_url, file_id }
                                if image_url.is_some() && file_id.is_none() => {}
                            _ => {
                                return Err(invalid(
                                    "input_image",
                                    "must have an image_url and no file_id",
                                ));
                            }
                        }
                    }
                }
            }
        }
    } else if let Evidence::Text(text) = evidence {
        let _ = text;
    }
    Ok(())
}
fn call(input: &Input) -> Result<Map<String, Value>, Fault> {
    let mut map = own(&input.native)?;
    validate_input(&input.state)?;
    map.insert("input".into(), input.state.clone());
    set(&mut map, "model", input.model.clone());
    let mut qs = Vec::new();
    for q in &input.questions {
        let mut v = own(&q.native)?;
        set(&mut v, "instructions", q.instructions.clone());
        match &q.ask {
            Ask::Noul {
                yes: None,
                no: None,
            } => {
                v.insert("type".into(), "predicate".into());
            }
            Ask::Choice { options } => {
                v.insert("type".into(), "choice".into());
                let original = v
                    .remove("choices")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                v.insert(
                    "choices".into(),
                    options
                        .iter()
                        .enumerate()
                        .map(|(i, (c, d))| {
                            let mut m = original
                                .get(i)
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            m.insert("value".into(), choice_json(c));
                            set(&mut m, "description", d.clone());
                            Value::Object(m)
                        })
                        .collect::<Vec<_>>()
                        .into(),
                );
            }
            Ask::Score { levels } => {
                v.insert("type".into(), "score".into());
                v.insert("levels".into(), levels.clone().into());
            }
            _ => return Err(Fault::bad_request("unsupported decision question")),
        }
        qs.push(Value::Object(v));
    }
    map.insert("questions".into(), qs.into());
    Ok(map)
}
fn probability(v: &Value) -> Option<f64> {
    v.as_f64()
        .filter(|p| p.is_finite() && (0.0..=1.0).contains(p))
}
fn answer(raw: &[u8], questions: &[Value], out: &mut Vec<Output>) -> Result<(), Fault> {
    let bad = || Fault::upstream("invalid openai-decisions response");
    let v: Value = serde_json::from_slice(raw).map_err(|_| bad())?;
    let answers = v["answers"].as_array().ok_or_else(bad)?;
    if answers.len() != questions.len() {
        return Err(bad());
    }
    let model = v["model"].as_str().ok_or_else(bad)?;
    let mut start = Start::new(Some(model.to_owned()));
    let mut extra = v.as_object().cloned().ok_or_else(bad)?;
    extra.remove("model");
    extra.remove("answers");
    extra.remove("usage");
    start.native = tag(extra);
    out.push(Output::Start(start));
    for (i, a) in answers.iter().enumerate() {
        if !matches!(
            a.get("name"),
            None | Some(Value::Null) | Some(Value::String(_))
        ) {
            return Err(bad());
        }
        let expected_name = questions[i].get("name").unwrap_or(&Value::Null);
        let actual_name = a.get("name").unwrap_or(&Value::Null);
        if expected_name != actual_name {
            return Err(bad());
        }
        let t = a["type"].as_str().ok_or_else(bad)?;
        if t != "refusal" && Some(t) != questions[i]["type"].as_str() {
            return Err(bad());
        }
        let verdict = match t {
            "refusal" => Verdict::Refusal,
            "predicate" => Verdict::Noul(probability(&a["probability"]).ok_or_else(bad)?),
            "choice" => Verdict::Choice {
                choice: choice(&a["choice"]).ok_or_else(bad)?,
                probabilities: a["probabilities"]
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|p| {
                        Ok((
                            choice(&p["value"]).ok_or_else(bad)?,
                            probability(&p["probability"]).ok_or_else(bad)?,
                        ))
                    })
                    .collect::<Result<_, Fault>>()?,
            },
            "score" => Verdict::Score {
                score: a["score"]
                    .as_f64()
                    .filter(|n| n.is_finite())
                    .ok_or_else(bad)?,
                probabilities: a["probabilities"]
                    .as_array()
                    .ok_or_else(bad)?
                    .iter()
                    .map(|p| probability(&p["probability"]).ok_or_else(bad))
                    .collect::<Result<_, _>>()?,
            },
            _ => return Err(bad()),
        };
        match &verdict {
            Verdict::Choice {
                choice: selected,
                probabilities,
            } => {
                let expected = questions[i]["choices"].as_array().ok_or_else(bad)?;
                let values = expected
                    .iter()
                    .map(|x| choice(&x["value"]).ok_or_else(bad))
                    .collect::<Result<Vec<_>, _>>()?;
                if probabilities.len() != values.len()
                    || !values.contains(selected)
                    || probabilities
                        .iter()
                        .zip(&values)
                        .any(|((v, _), expected)| v != expected)
                {
                    return Err(bad());
                }
            }
            Verdict::Score { probabilities, .. } => {
                let expected = questions[i]["levels"].as_array().ok_or_else(bad)?;
                let actual = a["probabilities"].as_array().ok_or_else(bad)?;
                if probabilities.len() != expected.len()
                    || actual.iter().enumerate().any(|(j, p)| {
                        p["value"].as_u64() != Some(j as u64)
                            || p["label"].as_str() != expected[j]["label"].as_str()
                    })
                {
                    return Err(bad());
                }
            }
            _ => {}
        }
        let mut result = Answer::new(i.to_string(), verdict);
        result.confidence = if t == "choice" || t == "score" {
            Some(probability(&a["confidence"]).ok_or_else(bad)?)
        } else {
            None
        };
        let mut extra = a.as_object().cloned().ok_or_else(bad)?;
        extra.remove("type");
        extra.remove("confidence");
        match &result.verdict {
            Verdict::Noul(_) => {
                extra.remove("probability");
            }
            Verdict::Choice { .. } => {
                extra.remove("choice");
            }
            Verdict::Score { .. } => {
                extra.remove("score");
            }
            _ => {}
        }
        result.native = tag(extra);
        out.push(Output::Answer(result));
    }
    if let Some(u) = v.get("usage") {
        let count = |key| u.get(key).and_then(Value::as_u64).ok_or_else(bad);
        let mut usage = Usage::new(count("input_tokens")?, count("output_tokens")?);
        if u.get("total_tokens").is_some() {
            count("total_tokens")?;
        }
        for (key, target) in [
            ("input_tokens_details", &mut usage.cached_input_tokens),
            ("output_tokens_details", &mut usage.reasoning_tokens),
        ] {
            if let Some(d) = u.get(key) {
                let sub = if key == "input_tokens_details" {
                    "cached_tokens"
                } else {
                    "reasoning_tokens"
                };
                *target = d.get(sub).and_then(Value::as_u64).ok_or_else(bad)?;
            }
        }
        if let Some(x) = u["input_tokens_details"].get("cache_write_tokens") {
            usage.cache_write_input_tokens = x.as_u64().ok_or_else(bad)?;
        }
        let mut extra = u.as_object().cloned().ok_or_else(bad)?;
        extra.remove("input_tokens");
        extra.remove("output_tokens");
        if extra.contains_key("total_tokens") {
            extra.insert("total_tokens".into(), Value::Null);
        }
        for (key, fields) in [
            (
                "input_tokens_details",
                &["cached_tokens", "cache_write_tokens"][..],
            ),
            ("output_tokens_details", &["reasoning_tokens"][..]),
        ] {
            if let Some(Value::Object(details)) = extra.get_mut(key) {
                for field in fields {
                    if details.contains_key(*field) {
                        details.insert((*field).into(), Value::Null);
                    }
                }
            }
        }
        out.push(Output::Usage(usage));
        out.push(Output::native(
            OpenAiDecisions::NAME,
            json!({"usage": extra}),
        ));
    }
    Ok(())
}
impl OpenAiDecisions {
    /// Serialize an answer using the OpenAI Decisions contract.
    pub fn answer_value(a: &Answer) -> Result<Value, Fault> {
        let mut v = kept(OpenAiDecisions::NAME, &a.native)?;
        match &a.verdict {
            Verdict::Noul(p) => {
                v.insert("type".into(), "predicate".into());
                v.insert("probability".into(), (*p).into());
            }
            Verdict::Refusal => {
                v.insert("type".into(), "refusal".into());
            }
            Verdict::Choice {
                choice,
                probabilities,
            } => {
                v.insert("type".into(), "choice".into());
                v.insert("choice".into(), choice_json(choice));
                let original = v
                    .remove("probabilities")
                    .and_then(|v| v.as_array().cloned())
                    .unwrap_or_default();
                v.insert(
                    "probabilities".into(),
                    probabilities
                        .iter()
                        .enumerate()
                        .map(|(i, (c, p))| {
                            let mut item = original
                                .get(i)
                                .and_then(Value::as_object)
                                .cloned()
                                .unwrap_or_default();
                            item.insert("value".into(), choice_json(c));
                            item.insert("probability".into(), (*p).into());
                            Value::Object(item)
                        })
                        .collect::<Vec<_>>()
                        .into(),
                );
            }
            Verdict::Score {
                score,
                probabilities,
            } => {
                v.insert("type".into(), "score".into());
                v.insert("score".into(), (*score).into());
                let mut parts = v
                    .remove("probabilities")
                    .and_then(|v| v.as_array().cloned())
                    .ok_or_else(|| Fault::bad_request("score labels are missing"))?;
                if parts.len() != probabilities.len() {
                    return Err(Fault::bad_request(
                        "score labels do not match probabilities",
                    ));
                }
                for (part, p) in parts.iter_mut().zip(probabilities) {
                    part.as_object_mut()
                        .ok_or_else(|| Fault::bad_request("invalid score label"))?
                        .insert("probability".into(), (*p).into());
                }
                v.insert("probabilities".into(), parts.into());
            }
            _ => return Err(Fault::bad_request("unsupported decision answer")),
        }
        v.remove("confidence");
        set(&mut v, "confidence", a.confidence);
        Ok(Value::Object(v))
    }
}
fn response(events: &[Output]) -> Result<Bytes, Fault> {
    let mut map = Map::new();
    let mut answers = Vec::new();
    let mut usage = None;
    let mut native_usage = Map::new();
    for e in events {
        match e {
            Output::Start(s) => {
                merge_missing(&mut map, &kept(OpenAiDecisions::NAME, &s.native)?);
                set(&mut map, "model", s.model.clone());
            }
            Output::Answer(a) => {
                answers.push(OpenAiDecisions::answer_value(a)?);
            }
            Output::Usage(u) => {
                *usage.get_or_insert_with(Usage::default) += *u;
            }
            Output::Native(n) if &*n.format == OpenAiDecisions::NAME => {
                if let Some(extra) = n.value.get("usage").and_then(Value::as_object) {
                    merge_missing(&mut native_usage, extra);
                }
            }
            Output::Native(_) => return Err(Fault::bad_request("foreign decision event")),
            _ => (),
        }
    }
    map.insert("answers".into(), answers.into());
    if let Some(u) = usage {
        let mut body = Map::new();
        body.insert("input_tokens".into(), u.input_tokens.into());
        body.insert("output_tokens".into(), u.output_tokens.into());
        if native_usage.contains_key("total_tokens") {
            body.insert(
                "total_tokens".into(),
                u.input_tokens.saturating_add(u.output_tokens).into(),
            );
        }
        for (key, fields) in [
            (
                "input_tokens_details",
                &[
                    ("cached_tokens", u.cached_input_tokens),
                    ("cache_write_tokens", u.cache_write_input_tokens),
                ][..],
            ),
            (
                "output_tokens_details",
                &[("reasoning_tokens", u.reasoning_tokens)][..],
            ),
        ] {
            let original = native_usage.get(key).and_then(Value::as_object);
            let mut details = Map::new();
            for (field, value) in fields {
                if *value > 0 || original.is_some_and(|o| o.contains_key(*field)) {
                    details.insert((*field).into(), (*value).into());
                }
            }
            if !details.is_empty() {
                body.insert(key.into(), details.into());
            }
        }
        merge_missing(&mut body, &native_usage);
        map.insert("usage".into(), body.into());
    }
    to_bytes(&map)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn typed_plugin_reply_needs_no_native_fields() {
        let answer = Answer::new("0", Verdict::Noul(0.7));
        let expected = json!({"type":"predicate","probability":0.7});
        assert_eq!(
            OpenAiDecisions::answer_value(&answer).expect("typed answer"),
            expected
        );
        let events = vec![
            Output::Start(Start::new(Some("m".into()))),
            Output::Answer(answer),
            Output::Usage(Usage::new(0, 0)),
        ];
        let actual: Value =
            serde_json::from_slice(&response(&events).expect("typed response")).expect("JSON");
        assert_eq!(
            actual,
            json!({"model":"m","answers":[expected],"usage":{"input_tokens":0,"output_tokens":0}})
        );
    }

    #[test]
    fn foreign_native_output_is_rejected() {
        let native = Some(NativeFields::new(
            "foreign",
            Map::from_iter([("extra".into(), true.into())]),
        ));
        let mut answer = Answer::new("0", Verdict::Noul(0.7));
        answer.native = native.clone();
        assert!(OpenAiDecisions::answer_value(&answer).is_err());
        let mut start = Start::new(Some("m".into()));
        start.native = native;
        assert!(response(&[Output::Start(start)]).is_err());
    }

    #[test]
    fn typed_choices_and_order_round_trip() {
        let raw = json!({"model":"deployment","input":[{"role":"user","content":[{"type":"input_text","text":"hello"},{"type":"input_image","image_url":"https://example.com/a.png"}]}],"questions":[{"type":"predicate","instructions":"a"},{"type":"choice","name":"same","instructions":"b","choices":[{"value":true},{"value":"true"}]},{"type":"score","name":"same","instructions":"c","levels":[{"label":"low"},{"label":"high"}]}]});
        let input = request(raw.to_string().as_bytes()).expect("valid fixture");
        assert_eq!(Value::Object(call(&input).expect("valid fixture")), raw);
        assert!(
            matches!(&input.questions[1].ask, Ask::Choice { options } if options[0].0 != options[1].0)
        );
        assert!(super::super::encode_request(&input).is_err());
    }
    #[test]
    fn mixed_refusal_and_zero_usage_round_trip() {
        let qs = vec![
            json!({"type":"predicate"}),
            json!({"type":"choice","name":"x","choices":[{"value":true},{"value":"true"}]}),
        ];
        let raw = json!({"model":"m","answers":[{"type":"refusal","name":null},{"type":"choice","name":"x","choice":true,"confidence":0.8,"probabilities":[{"value":true,"probability":0.8},{"value":"true","probability":0.2}]}],"usage":{"input_tokens":0,"output_tokens":0,"total_tokens":0}});
        let mut events = Vec::new();
        answer(raw.to_string().as_bytes(), &qs, &mut events).expect("valid fixture");
        assert!(matches!(&events[1], Output::Answer(a) if matches!(a.verdict, Verdict::Refusal)));
        assert!(events.contains(&Output::Usage(Usage::default())));
        assert_eq!(
            serde_json::from_slice::<Value>(&response(&events).expect("valid fixture"))
                .expect("valid fixture"),
            raw
        );
    }
    #[test]
    fn typed_usage_and_cleared_confidence_override_native_fields() {
        let qs = vec![json!({"type":"choice","name":"x","choices":[{"value":true}]})];
        let raw = json!({"model":"m","answers":[{"type":"choice","name":"x","choice":true,"confidence":0.8,"probabilities":[{"value":true,"probability":1.0,"extra":"kept"}],"extra":true}],"usage":{"input_tokens":1,"output_tokens":2,"total_tokens":3,"extra":"kept","input_tokens_details":{"cached_tokens":0,"cache_write_tokens":0,"extra":"input"},"output_tokens_details":{"reasoning_tokens":0,"extra":"output"}}});
        let mut events = Vec::new();
        answer(raw.to_string().as_bytes(), &qs, &mut events).expect("valid response");
        for event in &mut events {
            match event {
                Output::Answer(a) => a.confidence = None,
                Output::Usage(u) => {
                    u.input_tokens = 10;
                    u.output_tokens = 20;
                    u.cached_input_tokens = 3;
                    u.cache_write_input_tokens = 4;
                    u.reasoning_tokens = 5;
                }
                _ => {}
            }
        }
        events.push(Output::Usage(Usage::new(1, 2)));
        let encoded: Value =
            serde_json::from_slice(&response(&events).expect("response")).expect("json");
        assert_eq!(
            encoded["usage"],
            json!({"input_tokens":11,"output_tokens":22,"total_tokens":33,"extra":"kept","input_tokens_details":{"cached_tokens":3,"cache_write_tokens":4,"extra":"input"},"output_tokens_details":{"reasoning_tokens":5,"extra":"output"}})
        );
        assert!(encoded["answers"][0].get("confidence").is_none());
        assert_eq!(encoded["answers"][0]["extra"], true);
        assert_eq!(encoded["answers"][0]["probabilities"][0]["extra"], "kept");
    }

    #[test]
    fn rejects_bad_count_type_probability_and_foreign_provenance() {
        let qs = vec![json!({"type":"predicate"})];
        for raw in [
            json!({"model":"m","answers":[]}),
            json!({"model":"m","answers":[{"type":"choice"}]}),
            json!({"model":"m","answers":[{"type":"predicate","probability":1.1}]}),
            json!({"model":"m","answers":[{"type":"predicate","probability":0.5}],"usage":{"input_tokens":-1,"output_tokens":0}}),
        ] {
            assert!(answer(raw.to_string().as_bytes(), &qs, &mut Vec::new()).is_err());
        }
        let mut input = Input::new("x", vec![]);
        input.native = Some(NativeFields::new("foreign", Map::new()));
        assert!(call(&input).is_err());
        assert!(super::super::encode_request(&input).is_err());
    }
    #[test]
    fn score_and_absent_usage_round_trip() {
        let qs = vec![json!({"type":"score","levels":[{"label":"low"},{"label":"high"}]})];
        let raw = json!({"model":"m","extra":null,"answers":[{"type":"score","name":null,"score":0.8,"confidence":0.9,"probabilities":[{"label":"low","value":0,"probability":0.2,"extra":null},{"label":"high","value":1,"probability":0.8}]}]});
        let mut events = Vec::new();
        answer(raw.to_string().as_bytes(), &qs, &mut events).expect("valid score");
        assert!(!events.iter().any(|e| matches!(e, Output::Usage(_))));
        assert_eq!(
            serde_json::from_slice::<Value>(&response(&events).expect("response")).expect("json"),
            raw
        );
    }
    #[test]
    fn answer_names_must_match_their_slots() {
        let questions = vec![
            json!({"type":"predicate","name":"first"}),
            json!({"type":"predicate","name":"second"}),
        ];
        let swapped = json!({"model":"m","answers":[{"type":"predicate","name":"second","probability":0.2},{"type":"predicate","name":"first","probability":0.8}]});
        assert!(answer(swapped.to_string().as_bytes(), &questions, &mut Vec::new()).is_err());
        let missing = json!({"model":"m","answers":[{"type":"predicate","probability":0.2},{"type":"predicate","name":"second","probability":0.8}]});
        assert!(answer(missing.to_string().as_bytes(), &questions, &mut Vec::new()).is_err());
        let questions = vec![
            json!({"type":"predicate","name":"same"}),
            json!({"type":"predicate","name":"same"}),
            json!({"type":"predicate"}),
            json!({"type":"predicate","name":null}),
        ];
        let valid = json!({"model":"m","answers":[{"type":"predicate","name":"same","probability":0.1},{"type":"predicate","name":"same","probability":0.2},{"type":"predicate","name":null,"probability":0.3},{"type":"predicate","probability":0.4}]});
        answer(valid.to_string().as_bytes(), &questions, &mut Vec::new())
            .expect("duplicate and unnamed slots");
    }
}
