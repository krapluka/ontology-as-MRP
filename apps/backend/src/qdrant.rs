//! The Qdrant driver for the non-authoritative vector projection.
//!
//! A thin async client over Qdrant's REST API, like [`GraphDbClient`] in `ekg-semantic`:
//! it makes sure a collection of the pinned embedding width exists, upserts a point keyed
//! by the changed entity, reads back a point's stored `revision` (for the consumer-side
//! monotonic guard), and deletes on retraction.
//!
//! Reached over `http://` (no TLS locally: `reqwest` `default-features = false`), which
//! keeps the TLS dependency tree out of the `cargo-deny` licence check. The projection is
//! a read model and can be rebuilt by replaying the `RdfChanged` stream.
//!
//! [`GraphDbClient`]: ekg_semantic::GraphDbClient

use ekg_domain::{EngineError, Result, Revision};
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

/// Maps any error carrying a message to a typed backend error.
fn backend<E: std::fmt::Display>(err: E) -> EngineError {
    EngineError::Backend(err.to_string())
}

/// A thin async client over the Qdrant REST API for one collection.
///
/// Every non-2xx response or transport failure surfaces as [`EngineError::Backend`].
pub struct QdrantClient {
    http: reqwest::Client,
    /// Base URL, e.g. `http://localhost:6333` (no trailing slash).
    base_url: String,
    /// The collection the projection upserts into.
    collection: String,
}

impl QdrantClient {
    /// Connects to `base_url` and makes sure `collection` exists with the pinned
    /// embedding `dim` (cosine distance). An existing collection is left as it is, so
    /// its width must already match (it is `EMBED_DIM`, pinned across the engine).
    ///
    /// # Errors
    /// [`EngineError::Backend`] if the client cannot be built, Qdrant is unreachable,
    /// or the collection cannot be created.
    pub async fn connect(
        base_url: impl Into<String>,
        collection: impl Into<String>,
        dim: usize,
    ) -> Result<Self> {
        let http = reqwest::Client::builder().build().map_err(backend)?;
        let client = Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            collection: collection.into(),
        };
        client.ensure_collection(dim).await?;
        Ok(client)
    }

    /// The collection endpoint, e.g. `{base}/collections/{c}`.
    fn collection_url(&self) -> String {
        format!("{}/collections/{}", self.base_url, self.collection)
    }

    /// Creates the collection if it does not already exist.
    async fn ensure_collection(&self, dim: usize) -> Result<()> {
        // A GET on the collection returns 200 when it exists.
        let existing = self
            .http
            .get(self.collection_url())
            .send()
            .await
            .map_err(backend)?;
        if existing.status().is_success() {
            return Ok(());
        }
        let body = json!({ "vectors": { "size": dim, "distance": "Cosine" } });
        let resp = self
            .http
            .put(self.collection_url())
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).map_err(backend)?)
            .send()
            .await
            .map_err(backend)?;
        self.expect_success(resp, "create collection").await
    }

    /// Upserts the projection point for `entity_uri`: the embedding `vector` plus a
    /// payload carrying the source `model`, the event `subject`, and the `revision`
    /// the point was built at (read back by [`Self::revision_of`] for the guard).
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn upsert(
        &self,
        entity_uri: &str,
        model: &str,
        subject: &str,
        revision: Revision,
        vector: Vec<f32>,
    ) -> Result<()> {
        let body = json!({
            "points": [{
                "id": point_id(entity_uri),
                "vector": vector,
                "payload": {
                    "entity_uri": entity_uri,
                    "model": model,
                    "subject": subject,
                    "revision": revision.0,
                },
            }],
        });
        // wait=true so the write is visible on return (the gated test relies on it).
        let resp = self
            .http
            .put(format!("{}/points?wait=true", self.collection_url()))
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).map_err(backend)?)
            .send()
            .await
            .map_err(backend)?;
        self.expect_success(resp, "upsert point").await
    }

    /// Deletes the projection point for `entity_uri`. Deleting a missing point is a
    /// no-op. Used on a `Retracted` change.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure or a non-2xx response.
    pub async fn delete(&self, entity_uri: &str) -> Result<()> {
        let body = json!({ "points": [point_id(entity_uri)] });
        let resp = self
            .http
            .post(format!("{}/points/delete?wait=true", self.collection_url()))
            .header(CONTENT_TYPE, "application/json")
            .body(serde_json::to_vec(&body).map_err(backend)?)
            .send()
            .await
            .map_err(backend)?;
        self.expect_success(resp, "delete point").await
    }

    /// The `revision` stored on `entity_uri`'s point, or `None` if the point does not
    /// exist yet. Drives the consumer's per-entity monotonic-revision guard.
    ///
    /// # Errors
    /// [`EngineError::Backend`] on transport failure, a non-2xx (non-404) response, or
    /// an unparseable body.
    pub async fn revision_of(&self, entity_uri: &str) -> Result<Option<Revision>> {
        let resp = self
            .http
            .get(format!(
                "{}/points/{}",
                self.collection_url(),
                point_id(entity_uri)
            ))
            .send()
            .await
            .map_err(backend)?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        let status = resp.status();
        let text = resp.text().await.map_err(backend)?;
        if !status.is_success() {
            return Err(EngineError::Backend(format!("get point {status}: {text}")));
        }
        let doc: Value = serde_json::from_str(&text).map_err(backend)?;
        Ok(doc
            .pointer("/result/payload/revision")
            .and_then(Value::as_u64)
            .map(Revision))
    }

    /// Fails with a typed backend error unless `resp` is 2xx.
    async fn expect_success(&self, resp: reqwest::Response, what: &str) -> Result<()> {
        let status = resp.status();
        if status.is_success() {
            return Ok(());
        }
        let body = resp.text().await.unwrap_or_default();
        Err(EngineError::Backend(format!(
            "qdrant {what} {status}: {body}"
        )))
    }
}

/// A deterministic point id for an entity URI. Qdrant point ids must be a `u64` or a
/// UUID, not an arbitrary string, so the URI is hashed with FNV-1a (stable, no
/// dependency) and kept in the payload. The same entity maps to the same point across
/// restarts and rebuilds.
#[must_use]
pub fn point_id(entity_uri: &str) -> u64 {
    const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = FNV_OFFSET;
    for byte in entity_uri.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn point_id_is_deterministic_and_uri_sensitive() {
        let a = point_id("https://ekg.example/id/sale.order/1");
        assert_eq!(a, point_id("https://ekg.example/id/sale.order/1"));
        assert_ne!(a, point_id("https://ekg.example/id/sale.order/2"));
    }
}
