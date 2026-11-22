# Providers

A **provider** is the model backend the agent loop talks to. The loop only sees
the `Provider` trait in `destro-core` (`crates/destro-core/src/provider/mod.rs`):

```rust
pub trait Provider: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream;
}
```

`stream` takes one `ChatRequest` and returns a stream of `ProviderItem`s
(`TextDelta`, `ReasoningDelta`, `ToolCall`, `Usage`, `Done`). Everything
transport-specific — HTTP, SSE, retries — lives behind that trait, so adding a
backend never requires touching `agent.rs` or `harness.rs`.

Two implementations ship today:

| Kind | Type | Use |
|---|---|---|
| `mock` | `MockProvider` | Offline demos, tests, and `mock` as a safe default |
| `openaiCompat` | `OpenAiCompatProvider` | Any vendor speaking OpenAI `/chat/completions` streaming: OpenAI, DeepSeek, vLLM, llama.cpp, LM Studio, Ollama |

`ProviderRegistry` owns the catalogue of selectable providers and builds one by
id. It always contains `mock`, `deepseek`, and `openai-compatible`, and merges
an optional config file on top.

---

## Way 1 — config only (`providers.json`)

This is the normal way to point the harness at a new endpoint. No rebuild is
needed.

### Where the file lives

On Windows:

```
%APPDATA%\destro\providers.json
```

Otherwise, the first of:

```
$XDG_CONFIG_HOME/destro/providers.json
~/.config/destro/providers.json
```

`ProviderRegistry::path()` returns the resolved path; `ProviderRegistry::load()`
reads it. A **missing file is not an error** — the built-ins are used as-is. A
malformed file is reported by `ProviderRegistry::load_from(path)` (and ignored by
`load()`, which falls back to the built-ins).

### Shape

One JSON object with a `providers` array. Field names are **camelCase**. Only
`id` and `kind` are required.

```json
{
  "providers": [
    {
      "id": "local-llama",
      "label": "llama.cpp (local)",
      "kind": "openAiCompat",
      "baseUrl": "http://127.0.0.1:8080/v1",
      "defaultModel": "qwen2.5-coder",
      "models": ["qwen2.5-coder", "llama-3.3-70b"]
    },
    {
      "id": "deepseek",
      "label": "DeepSeek via gateway",
      "kind": "openAiCompat",
      "baseUrl": "https://gateway.internal.example/v1",
      "apiKeyEnv": "DEEPSEEK_API_KEY",
      "defaultModel": "deepseek-chat",
      "models": ["deepseek-chat", "deepseek-reasoner"]
    }
  ]
}
```

Fields:

| Field | Required | Meaning |
|---|---|---|
| `id` | yes | Selection id (`provider/build` and `default_id`). Config entries merge **by id** and win over built-ins: the `deepseek` entry above overrides the built-in URL. |
| `kind` | yes | `"mock"` or `"openAiCompat"` (aliases `"openai-compatible"` and `"openai-compat"` are accepted). |
| `label` | no | Display name. |
| `baseUrl` | no\* | API root; the request goes to `{baseUrl}/chat/completions`. \*Required for `openAiCompat`. |
| `apiKeyEnv` | no | Name of an environment variable holding the key. Omit for local servers that need no key. |
| `defaultModel` | no | Model id used when nothing else selects one. |
| `models` | no | Model ids offered to clients. |

### Environment variable convention

Secrets never go in `providers.json`. Put the key in the environment and name
that variable in `apiKeyEnv`:

```
set DEEPSEEK_API_KEY=sk-...
```

- The variable name is uppercase, `SCREAMING_SNAKE_CASE`, and ends in
  `_API_KEY` (`DEEPSEEK_API_KEY`, `OPENAI_API_KEY`, `TOGETHER_API_KEY`, …).
- `requires_key` in `ProviderInfo` is true exactly when `apiKeyEnv` is set.
- `configured` is true when `apiKeyEnv` is absent or the variable is present in
  the environment.
- `ProviderRegistry::build` resolves the variable and returns an error that
  **names the missing variable**; `default_id` picks the first configured
  non-mock provider, else `mock`.
- An empty `apiKeyEnv` variable value is passed through as an empty key (no
  `Authorization` header), which suits local endpoints.

---

## Way 2 — in code (the `Provider` trait)

Use this when a backend does not speak the OpenAI dialect, or needs custom
headers, signing, retries, or a non-HTTP transport.

1. Implement `Provider`:

```rust
use std::sync::Arc;
use destro_core::{
    ChatRequest, Provider, ProviderError, ProviderItem, ProviderStream,
};
use tokio_util::sync::CancellationToken;
use futures_util::StreamExt;

struct MyProvider;

impl Provider for MyProvider {
    fn name(&self) -> &'static str {
        "my-provider"
    }

    fn stream(&self, req: ChatRequest, cancel: CancellationToken) -> ProviderStream {
        // Honour cancellation: stop the request/reader when `cancel` fires.
        let items = futures_util::stream::iter(vec![
            Ok(ProviderItem::TextDelta("hello".into())),
            Ok(ProviderItem::Done { finish_reason: "stop".into() }),
        ]);
        items.boxed()
    }
}
```

2. Register a descriptor so clients can list and select it. A descriptor carries
   everything needed to build the provider; `ProviderKind` says which built-in
   transport it uses.

```rust
use destro_core::{ProviderDescriptor, ProviderKind, ProviderRegistry};

let registry = ProviderRegistry::builtin().with(ProviderDescriptor {
    id: "my-provider".into(),
    label: "My provider".into(),
    kind: ProviderKind::OpenAiCompat, // `Mock` for an offline provider
    base_url: "https://api.example.com/v1".into(),
    api_key_env: Some("MY_PROVIDER_API_KEY".into()),
    default_model: "my-model".into(),
    models: vec!["my-model".into()],
});

let provider = registry.build("my-provider").expect("configured");
```

If you add a brand-new transport, extend `ProviderKind` with a variant and match
on it in `ProviderRegistry::build`; the agent loop still does not change.
`ProviderRegistry::descriptors`, `list`, `build`, and `default_id` are the
client-facing surface for a picker UI.

---

## Model selection

`ChatRequest.model` is simply **whatever the harness passes** — normally the
model id of the agent (`HarnessConfig`, a spawn spec, or a model-swap steering
message). A provider must send that string to its backend unchanged; it must not
substitute its own default.

The registry only supplies **defaults and metadata**:

- `ProviderDescriptor::default_model` / `ProviderInfo::default_model` — the id a
  client may pre-select when the user has not chosen one.
- `ProviderInfo::models` — the ids a picker can offer.
- `ProviderRegistry::default_id()` — which provider id to start with.

So `ChatRequest.model` may be any model the endpoint serves, even one absent
from `models`; the lists are advisory, not a validation gate.

---

## What implementations must guarantee

- **Cancellation.** The returned stream must stop promptly when the
  `CancellationToken` fires. The agent loop selects on cancellation while it
  polls the stream; a provider that ignores the token keeps an HTTP request
  alive after the user interrupted. Map cancellation to
  `ProviderError::Cancelled` (or simply end the stream). Dropping the stream must
  not leak work.
- **Tool calls are complete.** Emit `ProviderItem::ToolCall` only once the id,
  name, and full JSON argument string are known. Providers that stream partial
  arguments (OpenAI does) must accumulate by index before emitting.
- **One `Done` per response.** Emit `Done { finish_reason }` last.
- **Never panic on malformed input.** Skip lines or return `ProviderError::Failed`
  instead.
- **Failures are useful.** `ProviderError::Failed` should include the HTTP status
  and a short response body, so the transcript explains what went wrong.

See `crates/destro-core/src/provider/openai_compat.rs` for the reference
streaming implementation and `crates/destro-core/src/provider/sse.rs` for the
chunk-boundary-safe SSE parser.
