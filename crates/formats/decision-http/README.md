# Decision formats

`respan-gateway-decision-http` is the decision kind over HTTP.

| Format | Endpoint | Served by |
| --- | --- | --- |
| `openai-decisions` (`OpenAiDecisions`) | `POST /openai/v1/decisions` in the standalone gateway | Azure OpenAI (`vendor = "azure"`) |
| `decisions` (`Decisions`) | `POST /v1/decisions`, `POST /v1/systemone` | Respan (`vendor = "respan"`); TypeSafe (`vendor = "typesafe"`), at `/systemone`; EmpirioLabs AI (`vendor = "empiriolabs"`); or any compatible base URL |

A provider is called at `/decisions`. A vendor that departs from the
contract lays its dialect over the format
(`respan_gateway_http::dialect::Dialected`), a provider format of the
spec's name: TypeSafe's moves the call to `/systemone`.

A request is `{"model", "state", "questions": {id: question}}`, each question
`{"type", "instructions", "criteria"}`: `noul` with criteria
`{"true", "false"}`, `choice` with `{option: description or null}`, `score`
with a list of levels. A response is `{"model", "answers": {id: answer},
"usage": {"input_tokens", "output_tokens"}}`, answered whole in JSON.

A question of another type is refused, 400, before any provider is called.
An answer of a type the kind has no form for is kept and written back where
it was. A refusal is `{"error": {"message", "type", "code"}}`, whose message
TypeSafe's SDKs read.

`OpenAiDecisions` uses the OpenAI `input` and ordered `questions` array
contract. Its reusable client defaults to `/v1/decisions`; applications can
choose a distinct path with `OpenAiDecisions::serving`. The standalone gateway
uses `/openai/v1/decisions` to preserve its existing Decisions endpoint.
The Azure dialect sends the key in `api-key` and calls `/decisions` relative
to the configured v1 base. Boolean choice values remain distinct from strings;
per-question refusals and provider usage retain their meaning.
