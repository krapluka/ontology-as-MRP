//! Shared Odoo client: one source of truth for the Odoo API surface.
//!
//! Talks to Odoo 19's `json2` external API (`POST /json/2/<model>/<method>`,
//! `Authorization: Bearer <api_key>`), the successor to the `/jsonrpc` +
//! `object.execute_kw` path deprecated in Odoo 19 and scheduled for removal in Odoo
//! 22. json2 binds the JSON body to the method signature by parameter name, with the
//! recordset addressed by a special `ids` key, so the surface is a small set of typed
//! methods (`search`, `search_read`, `read`, `create`, `write`, `unlink`) rather than
//! a generic positional `execute_kw`. On top of those sit the seeder helpers: xmlid
//! resolution, a many2one read, and an `ensure` upsert-by-external-ID. Consumed by both
//! `ekg-odoo-seed` (writes) and the ingestion driver in `ekg-semantic` (reads).

use std::time::Duration;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

/// The external-ID module namespace every seeded record is registered under.
/// Stable across runs, so re-running upserts instead of duplicating.
pub const SEED_MODULE: &str = "ekg_seed";

/// An Odoo `json2` session bound to one database, authenticated by API key.
pub struct Odoo {
    agent: ureq::Agent,
    base_url: String,
    db: String,
    api_key: String,
}

impl Odoo {
    /// Connect to the `json2` API and fail loud if the API key is rejected.
    ///
    /// `api_key` is an Odoo API key (`res.users.apikeys`, scope `rpc`) sent as a
    /// Bearer token, not the login password. `login` is only used in the error
    /// message; under Bearer auth the key identifies the user server-side.
    ///
    /// # Errors
    /// Returns an error if the transport fails or the key is rejected (a cheap
    /// authenticated probe runs here so a bad key surfaces at connect, not first use).
    pub fn connect(base_url: &str, db: &str, login: &str, api_key: &str) -> Result<Self> {
        // A non-2xx reply stays a response: its body carries the json2 error (see `call`).
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(60)))
            .http_status_as_error(false)
            // Odoo is reached directly; a proxy set in the environment is not for this client.
            .proxy(None)
            .build()
            .into();
        let odoo = Self {
            agent,
            base_url: base_url.trim_end_matches('/').to_owned(),
            db: db.to_owned(),
            api_key: api_key.to_owned(),
        };
        // Any authenticated call proves the Bearer key, so a bad one fails here.
        odoo.call("res.users", "search_count", json!({ "domain": [] }))
            .with_context(|| {
                format!(
                    "authenticating to Odoo db {db:?} as {login:?} \
                     (check ODOO_API_KEY is a valid `rpc`-scoped API key)"
                )
            })?;
        Ok(odoo)
    }

    /// Runs `search` on `model` and returns the matching record ids.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side error.
    #[allow(clippy::needless_pass_by_value)] // owned JSON handed to the transport
    pub fn search(&self, model: &str, domain: Value) -> Result<Vec<i64>> {
        let rows = self.call(model, "search", json!({ "domain": domain }))?;
        Ok(rows
            .as_array()
            .map(|a| a.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default())
    }

    /// Runs `search_read` on `model` and returns the matching records as JSON objects.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side error.
    #[allow(clippy::needless_pass_by_value)] // owned JSON handed to the transport
    pub fn search_read(&self, model: &str, domain: Value, fields: Value) -> Result<Value> {
        self.call(
            model,
            "search_read",
            json!({ "domain": domain, "fields": fields }),
        )
    }

    /// Reads the records addressed by `ids`. Pass [`Value::Null`] for `fields` to
    /// read every field.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side error.
    #[allow(clippy::needless_pass_by_value)] // owned JSON handed to the transport
    pub fn read(&self, model: &str, ids: Value, fields: Value) -> Result<Value> {
        let mut body = json!({ "ids": ids });
        if !fields.is_null() {
            body["fields"] = fields;
        }
        self.call(model, "read", body)
    }

    /// Creates a record and returns its id. json2 `create` returns `[id]`; this
    /// unwraps it to the single id.
    ///
    /// # Errors
    /// Returns an error on RPC failure or if `create` returns no id.
    // The owned JSON payload is handed to the transport.
    #[allow(clippy::needless_pass_by_value)]
    pub fn create(&self, model: &str, values: Value) -> Result<i64> {
        let res = self.call(model, "create", json!({ "vals_list": [values] }))?;
        created_id(&res).with_context(|| format!("create {model} did not return an id: {res}"))
    }

    /// Sets `values` on the records addressed by `ids`.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn write(&self, model: &str, ids: Value, values: Value) -> Result<()> {
        self.call(model, "write", json!({ "ids": ids, "vals": values }))?;
        Ok(())
    }

    /// Hard-deletes the records addressed by `ids`.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side error.
    #[allow(clippy::needless_pass_by_value)]
    pub fn unlink(&self, model: &str, ids: Value) -> Result<()> {
        self.call(model, "unlink", json!({ "ids": ids }))?;
        Ok(())
    }

    /// Resolve a static external ID (`module.name`) to its record id.
    ///
    /// # Errors
    /// Returns an error on RPC failure or if the external id is not found.
    pub fn xmlid(&self, module: &str, name: &str) -> Result<i64> {
        let rows = self.search_read(
            "ir.model.data",
            json!([["module", "=", module], ["name", "=", name]]),
            json!(["res_id"]),
        )?;
        rows.as_array()
            .and_then(|r| r.first())
            .and_then(|r| r["res_id"].as_i64())
            .with_context(|| format!("external id {module}.{name} not found in this database"))
    }

    /// Upsert a record addressed by the seeder's external ID.
    ///
    /// If `ekg_seed.<name>` already resolves, `write` the values onto the
    /// existing record; otherwise `create` it and register the external ID.
    /// This makes the seed re-runnable: change a value, re-run, and the same
    /// record is updated in place.
    ///
    /// # Errors
    /// Returns an error on RPC failure or if `create` returns no id.
    #[allow(clippy::needless_pass_by_value)] // owned payload handed to the RPC layer
    pub fn ensure(&self, name: &str, model: &str, values: Value) -> Result<i64> {
        let existing = self.search_read(
            "ir.model.data",
            json!([["module", "=", SEED_MODULE], ["name", "=", name]]),
            json!(["res_id"]),
        )?;

        if let Some(res_id) = existing
            .as_array()
            .and_then(|r| r.first())
            .and_then(|r| r["res_id"].as_i64())
        {
            self.write(model, json!([res_id]), values)?;
            return Ok(res_id);
        }

        let res_id = self.create(model, values)?;

        self.create(
            "ir.model.data",
            json!({
                "module": SEED_MODULE,
                "name": name,
                "model": model,
                "res_id": res_id,
                "noupdate": false,
            }),
        )?;
        Ok(res_id)
    }

    /// Upsert the record addressed by the seeder's external ID, adopting the record the
    /// source system already created for it instead of creating a second one.
    ///
    /// - If `ekg_seed.<name>` resolves, write the values onto that record (as [`Self::ensure`]).
    /// - Else, if `domain` matches records (e.g. the work order Odoo generated for an
    ///   operation when its manufacturing order was created), register the external ID on the
    ///   first one and write the values onto it.
    /// - Else create it and register the external ID.
    ///
    /// Afterwards every other record matching `domain` is unlinked, so a duplicate an
    /// earlier seed created (or the source's own, when the external ID already named a
    /// duplicate) is removed and one record remains.
    ///
    /// # Errors
    /// Returns an error on RPC failure or if `create` returns no id.
    #[allow(clippy::needless_pass_by_value)] // owned payloads handed to the RPC layer
    pub fn adopt(&self, name: &str, model: &str, domain: Value, values: Value) -> Result<i64> {
        let registered = self.search_read(
            "ir.model.data",
            json!([["module", "=", SEED_MODULE], ["name", "=", name]]),
            json!(["res_id"]),
        )?;
        let registered = registered
            .as_array()
            .and_then(|r| r.first())
            .and_then(|r| r["res_id"].as_i64());
        let matching = self.search(model, domain.clone())?;
        let res_id = match (registered, matching.first()) {
            (Some(res_id), _) => {
                self.write(model, json!([res_id]), values)?;
                res_id
            }
            (None, Some(&own)) => {
                self.create(
                    "ir.model.data",
                    json!({
                        "module": SEED_MODULE,
                        "name": name,
                        "model": model,
                        "res_id": own,
                        "noupdate": false,
                    }),
                )?;
                self.write(model, json!([own]), values)?;
                own
            }
            (None, None) => self.ensure(name, model, values)?,
        };
        let others: Vec<i64> = self
            .search(model, domain)?
            .into_iter()
            .filter(|id| *id != res_id)
            .collect();
        if !others.is_empty() {
            self.unlink(model, json!(others))?;
        }
        Ok(res_id)
    }

    /// Read a single many2one field, returning the referenced record id.
    ///
    /// # Errors
    /// Returns an error on RPC failure or if the field is empty.
    pub fn read_m2o(&self, model: &str, id: i64, field: &str) -> Result<i64> {
        let rows = self.read(model, json!([id]), json!([field]))?;
        // many2one reads back as `[id, "display name"]`.
        rows.as_array()
            .and_then(|r| r.first())
            .and_then(|r| r[field].as_array())
            .and_then(|pair| pair.first())
            .and_then(Value::as_i64)
            .with_context(|| format!("{model}.{field} was empty for id {id}"))
    }

    /// Call an arbitrary model method over `json2`: `POST /json/2/<model>/<method>`
    /// with `body` as the named-kwargs payload (the recordset addressed by the special
    /// `ids` key). Escape hatch for workflow methods the typed helpers don't cover
    /// (e.g. `purchase.order.button_confirm` to let Odoo generate the inbound picking).
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side json2 error.
    #[allow(clippy::needless_pass_by_value)] // body is moved into the request
    pub fn call_method(&self, model: &str, method: &str, body: Value) -> Result<Value> {
        self.call(model, method, body)
    }

    /// One `json2` round-trip: `POST /json/2/<model>/<method>?db=<db>` with the Bearer
    /// key and `body` as the named-kwargs payload. Returns the raw method result;
    /// a non-2xx json2 error body becomes an error.
    ///
    /// # Errors
    /// Returns an error on transport failure or an Odoo-side json2 error.
    #[allow(clippy::needless_pass_by_value)] // body is moved into the request
    fn call(&self, model: &str, method: &str, body: Value) -> Result<Value> {
        let url = json2_url(&self.base_url, model, method, &self.db);
        let mut resp = self
            .agent
            .post(&url)
            .header("Authorization", format!("Bearer {}", self.api_key))
            .send_json(body)
            .with_context(|| format!("POST {url} ({model}.{method})"))?;
        let status = resp.status();
        // `with_config` reads without the default 10 MB cap: a large `read` is one reply.
        let body = resp.body_mut().with_config();
        if status.is_success() {
            body.read_json()
                .with_context(|| format!("decoding json2 response ({model}.{method})"))
        } else {
            let payload = body.read_json::<Value>().unwrap_or(Value::Null);
            bail!("{}", json2_error(model, method, status.as_u16(), &payload));
        }
    }
}

/// Build the `json2` endpoint URL for a model method, selecting the database
/// explicitly via `?db=` (safe under a multi-database Odoo; ignored when single-db).
#[must_use]
fn json2_url(base_url: &str, model: &str, method: &str, db: &str) -> String {
    format!("{base_url}/json/2/{model}/{method}?db={db}")
}

/// The id from a json2 `create` result (`[id]`), if present.
#[must_use]
fn created_id(result: &Value) -> Option<i64> {
    result
        .as_array()
        .and_then(|a| a.first())
        .and_then(Value::as_i64)
}

/// Render a json2 error body (`{name, message, arguments:[msg, code], ...}`) as a
/// one-line failure, falling back to the HTTP status when the body is unexpected.
#[must_use]
fn json2_error(model: &str, method: &str, code: u16, body: &Value) -> String {
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("HTTPError");
    let message = body
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("(no message)");
    format!("Odoo json2 error ({model}.{method}) HTTP {code}: {name}: {message}")
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    use super::*;

    /// A local HTTP server that answers one request per canned `(status, body)` reply, in
    /// order, and returns the requests it read. The first reply always goes to the probe
    /// `Odoo::connect` sends.
    fn serve(replies: Vec<(u16, &'static str)>) -> (String, JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind a local port");
        let base_url = format!("http://{}", listener.local_addr().expect("local address"));
        let server = std::thread::spawn(move || {
            let mut requests = Vec::new();
            for (status, body) in replies {
                let (mut stream, _) = listener.accept().expect("accept a connection");
                let mut reader = BufReader::new(stream.try_clone().expect("clone the stream"));
                let mut head = String::new();
                let mut length = 0;
                loop {
                    let mut line = String::new();
                    reader.read_line(&mut line).expect("read a header line");
                    if line == "\r\n" {
                        break;
                    }
                    if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                        length = value.trim().parse().expect("a numeric content length");
                    }
                    head.push_str(&line);
                }
                let mut payload = vec![0; length];
                reader.read_exact(&mut payload).expect("read the body");
                requests.push(format!("{head}\r\n{}", String::from_utf8_lossy(&payload)));
                write!(
                    stream,
                    "HTTP/1.1 {status} Reply\r\nContent-Type: application/json\r\n\
                     Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write the reply");
            }
            requests
        });
        (base_url, server)
    }

    #[test]
    fn a_call_sends_the_bearer_key_and_returns_the_result() {
        let (base_url, server) = serve(vec![(200, "3"), (200, r#"[{"id": 7, "name": "Acme"}]"#)]);
        let odoo = Odoo::connect(&base_url, "ekg", "admin", "secret-key").expect("connect");

        let rows = odoo
            .call_method("res.partner", "search_read", json!({ "domain": [] }))
            .expect("a 200 reply is the method result");

        assert_eq!(rows, json!([{ "id": 7, "name": "Acme" }]));
        let requests = server.join().expect("the server thread");
        let call = requests[1].to_ascii_lowercase();
        assert!(
            call.starts_with("post /json/2/res.partner/search_read?db=ekg "),
            "posts to the json2 endpoint: {call}"
        );
        assert!(
            call.contains("authorization: bearer secret-key\r\n"),
            "sends the API key as a Bearer token: {call}"
        );
        let (_, sent) = requests[1]
            .split_once("\r\n\r\n")
            .expect("a head and a body");
        let sent: Value = serde_json::from_str(sent).expect("a JSON body");
        assert_eq!(sent, json!({ "domain": [] }), "sends the body");
    }

    #[test]
    fn a_rejected_call_reports_the_odoo_error_from_the_reply_body() {
        let error = r#"{"name": "builtins.ValueError", "message": "Invalid field 'nope' on 'res.partner'", "arguments": ["Invalid field 'nope' on 'res.partner'", 422]}"#;
        let (base_url, server) = serve(vec![(200, "3"), (422, error)]);
        let odoo = Odoo::connect(&base_url, "ekg", "admin", "secret-key").expect("connect");

        let failure = odoo
            .call_method("res.partner", "search_read", json!({ "fields": ["nope"] }))
            .expect_err("a 422 reply is an error");

        assert_eq!(
            failure.to_string(),
            "Odoo json2 error (res.partner.search_read) HTTP 422: builtins.ValueError: \
             Invalid field 'nope' on 'res.partner'"
        );
        server.join().expect("the server thread");
    }

    #[test]
    fn json2_url_targets_the_model_method_with_db() {
        assert_eq!(
            json2_url("http://localhost:8069", "res.partner", "search_read", "ekg"),
            "http://localhost:8069/json/2/res.partner/search_read?db=ekg"
        );
    }

    #[test]
    fn created_id_unwraps_the_single_element_list() {
        assert_eq!(created_id(&json!([20])), Some(20));
        assert_eq!(created_id(&json!([])), None, "empty list has no id");
        assert_eq!(
            created_id(&json!(20)),
            None,
            "a bare scalar is not the json2 shape"
        );
    }

    #[test]
    fn json2_error_renders_name_and_message() {
        let body = json!({
            "name": "builtins.ValueError",
            "message": "Invalid field 'nope' on 'res.partner'",
            "arguments": ["Invalid field 'nope' on 'res.partner'", 422],
        });
        let rendered = json2_error("res.partner", "search_read", 422, &body);
        assert!(
            rendered.contains("res.partner.search_read"),
            "names the call"
        );
        assert!(rendered.contains("HTTP 422"), "carries the status");
        assert!(
            rendered.contains("Invalid field 'nope'"),
            "carries the message"
        );
    }

    #[test]
    fn json2_error_falls_back_when_body_is_unexpected() {
        let rendered = json2_error("res.partner", "read", 500, &Value::Null);
        assert!(rendered.contains("HTTP 500"));
        assert!(
            rendered.contains("HTTPError"),
            "falls back to a generic name"
        );
    }
}
