//! Gated live TLS smoke test for the cognition provider.
//!
//! Ignored by default. It confirms the provider can negotiate TLS and reach the
//! default HTTPS OpenAI-compatible endpoint (OpenRouter) over the streamed
//! `async-openai` boundary, which is what the rustls backend (ring provider, OS trust
//! store) on `ekg-cognition`'s transport is for. Run with a real key:
//!
//! ```text
//! MODEL_API_KEY=sk-... cargo test -p ekg-cognition --test it tls_smoke_it -- --ignored
//! ```
//!
//! It reads `MODEL_BASE_URL` (default `https://openrouter.ai/api/v1`) and
//! `CHAT_MODEL` (default `openai/gpt-oss-120b`, the checked-in default). If
//! `MODEL_API_KEY` is unset or empty it prints a message and skips rather than failing.

// `expect` and `panic` are the assertion mechanism here; the prose names products
// (OpenRouter).
#![allow(clippy::expect_used, clippy::doc_markdown)]

use ekg_cognition::{
    CognitionConfig, CognitionLimits, CognitionProvider, CognitionRoute, OpenAiCompatCognition,
};

#[tokio::test]
#[ignore = "requires MODEL_API_KEY + a reachable HTTPS provider (run with --ignored)"]
async fn provider_negotiates_tls_against_https_endpoint() {
    let Ok(api_key) = std::env::var("MODEL_API_KEY") else {
        eprintln!("SKIP: MODEL_API_KEY unset, cannot reach the real HTTPS provider");
        return;
    };
    if api_key.trim().is_empty() {
        eprintln!("SKIP: MODEL_API_KEY empty, cannot reach the real HTTPS provider");
        return;
    }

    let base_url = std::env::var("MODEL_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_owned());
    let chat_model =
        std::env::var("CHAT_MODEL").unwrap_or_else(|_| "openai/gpt-oss-120b".to_owned());

    // Fail if the endpoint is not HTTPS: this test exists to exercise TLS.
    assert!(
        base_url.starts_with("https://"),
        "MODEL_BASE_URL must be an https:// endpoint to exercise TLS (got {base_url})"
    );

    let provider = OpenAiCompatCognition::new(CognitionConfig {
        // The direct route: this test exercises the engine's own TLS to the provider.
        route: CognitionRoute::Direct,
        base_url,
        api_key: Some(api_key),
        chat_model,
        // embed_* unused by generate; pin something valid.
        embed_model: "unused".to_owned(),
        embed_dim: 1,
        limits: CognitionLimits::default(),
    })
    .expect("provider");

    // A trivial prompt: a successful round-trip means the TLS handshake and the
    // HTTPS request/response both worked. Any non-2xx surfaces as an error here.
    let reply = provider
        .generate("Reply with the single word: ok")
        .await
        .expect("live HTTPS generate over TLS");
    assert!(
        !reply.trim().is_empty(),
        "provider returned empty content over the live HTTPS path"
    );
}
