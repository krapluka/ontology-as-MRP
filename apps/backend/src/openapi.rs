//! The code-generated OpenAPI 3.1 contract for the `/api` surface.
//!
//! A `#[derive(OpenApi)]` document assembled from the `#[utoipa::path]` annotations on
//! the handlers, served as JSON at `GET /api/openapi.json`. There is no Swagger-UI and no
//! bundled viewer (bring your own client). The neutral core stays utoipa-free:
//! action-point response bodies are documented as opaque JSON objects at the boundary,
//! while the governed request inputs and the error shape carry real schemas.

// Docs name products (OpenAPI, Odoo, Restate).
#![allow(clippy::doc_markdown)]

use axum::{Json, Router, routing::get};
use utoipa::OpenApi;

use crate::{action_point, health, ingest, portfolio_command};

/// The OpenAPI document for the public `/api` surface. Version is taken from the crate
/// (utoipa reads `CARGO_PKG_VERSION`).
#[derive(OpenApi)]
#[openapi(
    info(
        title = "ekg-backend API",
        description = "The governed /api surface. Writes are classic REST commands (this \
                       document): the Odoo ingestion webhook and human oversight of action \
                       points (list / read / promote / abort / undo) and of portfolios \
                       (promote / undo / abort, each one governed step across every member). \
                       Reads \
                       are served by \
                       the separate read-only GraphQL surface at POST /api/graphql, which \
                       defines no mutation root: every governed mutation stays an explicit \
                       REST command here. Plus operational liveness + readiness."
    ),
    paths(
        ingest::ingest_odoo,
        action_point::list,
        action_point::get_one,
        action_point::promote,
        action_point::abort,
        action_point::undo,
        portfolio_command::promote,
        portfolio_command::undo,
        portfolio_command::abort,
        health::health,
        health::ready,
    ),
    components(schemas(
        ingest::OdooNotification,
        action_point::PromoteInput,
        action_point::AbortInput,
        action_point::UndoInput,
        portfolio_command::PortfolioPromoteInput,
        portfolio_command::PortfolioUndoInput,
        portfolio_command::PortfolioAbortInput,
        action_point::Problem,
        health::HealthStatus,
        health::ReadinessStatus,
    )),
    tags(
        (name = "ingest", description = "Governed source ingestion"),
        (name = "action-points", description = "Human oversight: list / read / promote / abort"),
        (name = "portfolios", description = "Human oversight of portfolios: promote / undo / \
         dismiss a joint plan as one governed step"),
        (name = "meta", description = "Operational: liveness + readiness")
    )
)]
pub struct ApiDoc;

/// `GET /api/openapi.json`: the generated OpenAPI document as JSON.
pub async fn openapi_json() -> Json<utoipa::openapi::OpenApi> {
    Json(ApiDoc::openapi())
}

/// Mount the openapi route (stateless).
pub fn router() -> Router {
    Router::new().route("/api/openapi.json", get(openapi_json))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use axum::http::{StatusCode, header};
    use axum::response::IntoResponse;

    use super::*;

    #[test]
    fn the_document_lists_every_api_path_with_a_version() {
        let json = serde_json::to_value(ApiDoc::openapi()).expect("serialize openapi");
        assert!(
            json.get("openapi")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.starts_with("3.")),
            "openapi version field present, got {:?}",
            json.get("openapi")
        );
        let paths = json.get("paths").expect("paths object");
        for p in [
            "/api/ingest/odoo",
            "/api/action-points",
            "/api/action-points/one",
            "/api/action-points/promote",
            "/api/action-points/abort",
            "/api/action-points/undo",
            "/api/portfolios/promote",
            "/api/portfolios/undo",
            "/api/portfolios/abort",
            "/api/health",
            "/api/ready",
        ] {
            assert!(
                paths.get(p).is_some(),
                "openapi document is missing path {p}"
            );
        }
    }

    /// Every operation's `operationId`, as `(path, method, id)`.
    fn operation_ids(json: &serde_json::Value) -> Vec<(String, String, String)> {
        let mut ids = Vec::new();
        for (path, item) in json["paths"].as_object().expect("paths") {
            for (method, operation) in item.as_object().expect("path item") {
                if let Some(id) = operation.get("operationId").and_then(|v| v.as_str()) {
                    ids.push((path.clone(), method.clone(), id.to_owned()));
                }
            }
        }
        ids
    }

    #[test]
    fn every_operation_id_is_unique() {
        // A client generated from the document has one entry per operation, so no two
        // operations may share an id (the point and portfolio command families both have
        // a `promote` and an `undo`).
        let json = serde_json::to_value(ApiDoc::openapi()).expect("serialize openapi");
        let ids = operation_ids(&json);
        let mut seen = std::collections::BTreeMap::new();
        for (path, method, id) in &ids {
            if let Some(other) = seen.insert(id.clone(), format!("{method} {path}")) {
                panic!("operationId {id:?} is shared by {other} and {method} {path}");
            }
        }
        let find = |path: &str| {
            ids.iter()
                .find(|(p, _, _)| p == path)
                .map(|(_, _, id)| id.as_str())
        };
        assert_eq!(
            find("/api/action-points/promote"),
            Some("promote_action_point")
        );
        assert_eq!(find("/api/portfolios/promote"), Some("promote_portfolio"));
        assert_eq!(find("/api/portfolios/abort"), Some("abort_portfolio"));
        assert_eq!(
            ids.len(),
            json["paths"].as_object().map_or(0, serde_json::Map::len)
        );
    }

    #[tokio::test]
    async fn openapi_route_serves_application_json() {
        let resp = openapi_json().await.into_response();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            resp.headers()
                .get(header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
    }
}
