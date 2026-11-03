//! The skill registry: agent skills are authored in git, but versioned and switched at
//! runtime here, in one durable Restate object.
//!
//! - A version is a skill's content digest ([`ekg_cognition::skills::digest`]): immutable,
//!   so publishing content already registered is a no-op.
//! - A skill's first version becomes active; a later one is registered inactive until an
//!   operator activates it, so a deploy never silently changes agent behaviour. Activating
//!   an earlier version is the rollback; no version is ever deleted.
//! - Every activation, the first version's automatic one included, is appended to the skill's
//!   history (who, why, when): the audit trail.
//!
//! The registry is one object instance ([`SKILL_REGISTRY_KEY`]), so publish, activate and
//! reads see one consistent view. State is the index (every skill's versions, active version
//! and history) plus one entry per version holding its files. Publishing and activating are
//! exclusive handlers; reading the index and a version are shared, concurrent handlers.
//!
//! The state transitions are the pure [`apply_publish`] and [`apply_activate`], tested
//! hermetically; the object only loads and persists. [`RegistryClient`] reaches the object
//! over the Restate ingress for tooling (the `skills` binary), for resolving a task's
//! [`active_pins`], and for the tool gateway's [`RegistrySource`]. Skills are behaviour
//! configuration, not governed facts, so none of this touches the world model.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ekg_cognition::skills::{self, ProfileTools, SkillBundle};
use ekg_cognition::tools::{SkillPin, SkillSource};
use restate_sdk::prelude::*;
use serde::{Deserialize, Serialize};

/// The registered name of the skill registry object.
pub const SKILL_REGISTRY_OBJECT: &str = "SkillRegistry";
/// The single key the registry runs under.
pub const SKILL_REGISTRY_KEY: &str = "catalog";
/// Who a first version's automatic activation is recorded as.
pub const PUBLISH_ACTOR: &str = "publish";

/// Restate state key of the index.
const INDEX_STATE: &str = "index";

/// Restate state key of one version's content.
fn content_key(version: &str) -> String {
    format!("content/{version}")
}

/// Every skill in the registry, by name.
pub type SkillIndex = BTreeMap<String, SkillEntry>;

/// One skill: its active version, every version published (in publish order), and its
/// activation history (append-only, in order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SkillEntry {
    /// The version tasks started now are pinned to.
    pub active: String,
    /// Every version published, oldest first.
    pub versions: Vec<VersionInfo>,
    /// Every activation, oldest first.
    pub history: Vec<Activation>,
}

impl SkillEntry {
    /// The published version `version`.
    #[must_use]
    pub fn version(&self, version: &str) -> Option<&VersionInfo> {
        self.versions.iter().find(|info| info.version == version)
    }
}

/// One published version's metadata (its files are kept apart, by version).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionInfo {
    /// The version id: the content digest.
    pub version: String,
    /// The version's one-line description.
    pub description: String,
    /// The profiles the version is written for.
    pub profiles: Vec<String>,
    /// The tools the version relies on.
    pub tools: Vec<String>,
    /// When it was published, in seconds since the Unix epoch.
    pub published_at: u64,
}

/// One activation, as the history records it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Activation {
    /// The version activated.
    pub version: String,
    /// The version active before (none for a first version).
    pub previous: Option<String>,
    /// Who activated it ([`PUBLISH_ACTOR`] for a first version).
    pub by: String,
    /// Why.
    pub reason: String,
    /// When, in seconds since the Unix epoch.
    pub at: u64,
}

/// A skill's files, submitted for publishing under the version they digest to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishRequest {
    /// The skill's name (its directory name).
    pub name: String,
    /// The version the publisher computed: the files' digest.
    pub version: String,
    /// Every file, `SKILL.md` included, by path.
    pub files: BTreeMap<String, String>,
}

impl PublishRequest {
    /// The request publishing `bundle`.
    #[must_use]
    pub fn of(bundle: &SkillBundle) -> Self {
        Self {
            name: bundle.name().to_owned(),
            version: bundle.digest(),
            files: bundle.files().clone(),
        }
    }
}

/// What a publish did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PublishOutcome {
    /// The skill.
    pub skill: String,
    /// The version published.
    pub version: String,
    /// Whether the version was new (`false`: it was already registered, nothing changed).
    pub created: bool,
    /// The skill's active version after the publish.
    pub active: String,
}

/// An explicit activation (or rollback) of a registered version.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateRequest {
    /// The skill.
    pub skill: String,
    /// The full version id to activate.
    pub version: String,
    /// Who activates it.
    pub by: String,
    /// Why.
    pub reason: String,
}

/// What an activation did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActivateOutcome {
    /// The skill.
    pub skill: String,
    /// The version now active.
    pub version: String,
    /// The version active before.
    pub previous: String,
}

/// One version's content, as the registry serves it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VersionContent {
    /// The skill's name.
    pub name: String,
    /// The version id.
    pub version: String,
    /// Every file, `SKILL.md` included, by path.
    pub files: BTreeMap<String, String>,
}

/// Why the registry refused a request, with the HTTP status the ingress answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// 400 (malformed), 404 (unknown skill or version) or 409 (already active).
    pub code: u16,
    /// Why, for the operator.
    pub reason: String,
}

impl Refusal {
    fn new(code: u16, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.reason, self.code)
    }
}

impl From<Refusal> for HandlerError {
    fn from(refusal: Refusal) -> Self {
        TerminalError::new_with_code(refusal.code, refusal.reason).into()
    }
}

/// Register `bundle` (already validated) in `index` at `now`. Idempotent: a version already
/// registered changes nothing. A new version is appended; it becomes active only when it is the
/// skill's first, with a history entry by [`PUBLISH_ACTOR`].
pub fn apply_publish(index: &mut SkillIndex, bundle: &SkillBundle, now: u64) -> PublishOutcome {
    let version = bundle.digest();
    let info = VersionInfo {
        version: version.clone(),
        description: bundle.description().to_owned(),
        profiles: bundle.profiles().to_vec(),
        tools: bundle.tools().to_vec(),
        published_at: now,
    };
    let name = bundle.name().to_owned();
    let (created, active) = match index.get_mut(&name) {
        Some(entry) if entry.version(&version).is_some() => (false, entry.active.clone()),
        Some(entry) => {
            entry.versions.push(info);
            (true, entry.active.clone())
        }
        None => {
            index.insert(
                name.clone(),
                SkillEntry {
                    active: version.clone(),
                    versions: vec![info],
                    history: vec![Activation {
                        version: version.clone(),
                        previous: None,
                        by: PUBLISH_ACTOR.to_owned(),
                        reason: "first version".to_owned(),
                        at: now,
                    }],
                },
            );
            (true, version.clone())
        }
    };
    PublishOutcome {
        skill: name,
        version,
        created,
        active,
    }
}

/// Activate `request.version` of `request.skill` in `index` at `now`, appending the history.
///
/// # Errors
/// A [`Refusal`]: an empty `by` or `reason` (400), an unknown skill or version (404), or the
/// version already active (409). A refused request changes nothing.
pub fn apply_activate(
    index: &mut SkillIndex,
    request: &ActivateRequest,
    now: u64,
) -> Result<ActivateOutcome, Refusal> {
    let (by, reason) = (request.by.trim(), request.reason.trim());
    if by.is_empty() {
        return Err(Refusal::new(
            400,
            "an activation needs who activates it (`by`)",
        ));
    }
    if reason.is_empty() {
        return Err(Refusal::new(400, "an activation needs a reason"));
    }
    let entry = index
        .get_mut(&request.skill)
        .ok_or_else(|| Refusal::new(404, format!("unknown skill {:?}", request.skill)))?;
    if entry.version(&request.version).is_none() {
        return Err(Refusal::new(
            404,
            format!(
                "skill {:?} has no version {:?}",
                request.skill, request.version
            ),
        ));
    }
    if entry.active == request.version {
        return Err(Refusal::new(
            409,
            format!(
                "version {} of skill {:?} is already active",
                request.version, request.skill
            ),
        ));
    }
    let previous = std::mem::replace(&mut entry.active, request.version.clone());
    entry.history.push(Activation {
        version: request.version.clone(),
        previous: Some(previous.clone()),
        by: by.to_owned(),
        reason: reason.to_owned(),
        at: now,
    });
    Ok(ActivateOutcome {
        skill: request.skill.clone(),
        version: request.version.clone(),
        previous,
    })
}

/// Parse and check a publish request against the running engine's profiles: the files form a
/// valid skill named `request.name`, and they digest to `request.version`.
///
/// # Errors
/// A 400 [`Refusal`] naming the skill and every defect.
pub fn check_publish(
    request: &PublishRequest,
    profiles: &ProfileTools,
) -> Result<SkillBundle, Refusal> {
    let bundle = SkillBundle::parse(&request.name, request.files.clone())
        .map_err(|error| Refusal::new(400, error.to_string()))?;
    skills::validate(&bundle, profiles).map_err(|error| Refusal::new(400, error.to_string()))?;
    let digest = bundle.digest();
    if digest != request.version {
        return Err(Refusal::new(
            400,
            format!(
                "skill {:?}: its files digest to {digest}, not the version {} it was published as",
                request.name, request.version
            ),
        ));
    }
    Ok(bundle)
}

/// The skill registry object: built with the running engine's map from profile to tools,
/// which every publish is re-validated against.
#[derive(Clone)]
pub struct SkillRegistry {
    profiles: Arc<ProfileTools>,
}

impl SkillRegistry {
    /// The registry validating against `profiles`.
    #[must_use]
    pub fn new(profiles: ProfileTools) -> Self {
        Self {
            profiles: Arc::new(profiles),
        }
    }
}

/// Seconds since the Unix epoch. A clock read: call only inside `ctx.run`.
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

#[restate_sdk::object]
impl SkillRegistry {
    /// Publish a skill version: re-validate it against this engine's profiles and check its
    /// digest, then register it (a no-op when already registered). A skill's first version
    /// becomes active; a later one stays inactive.
    #[handler]
    async fn publish(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<PublishRequest>,
    ) -> Result<Json<PublishOutcome>, HandlerError> {
        let request = request.into_inner();
        let bundle = check_publish(&request, &self.profiles)?;
        let mut index = ctx
            .get::<Json<SkillIndex>>(INDEX_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        // The clock is journaled, so a replay records the same time.
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("published_at")
            .await?;
        let outcome = apply_publish(&mut index, &bundle, now);
        if outcome.created {
            ctx.set(
                &content_key(&outcome.version),
                Json(VersionContent {
                    name: request.name,
                    version: outcome.version.clone(),
                    files: request.files,
                }),
            );
            ctx.set(INDEX_STATE, Json(index));
        }
        Ok(Json(outcome))
    }

    /// Activate a registered version (an earlier one is the rollback), recording who, why and
    /// when.
    #[handler]
    async fn activate(
        &self,
        ctx: ObjectContext<'_>,
        request: Json<ActivateRequest>,
    ) -> Result<Json<ActivateOutcome>, HandlerError> {
        let request = request.into_inner();
        let mut index = ctx
            .get::<Json<SkillIndex>>(INDEX_STATE)
            .await?
            .map(Json::into_inner)
            .unwrap_or_default();
        let now = ctx
            .run(|| async { Ok::<_, HandlerError>(unix_now()) })
            .name("activated_at")
            .await?;
        let outcome = apply_activate(&mut index, &request, now)?;
        ctx.set(INDEX_STATE, Json(index));
        Ok(Json(outcome))
    }

    /// Every skill's versions, active version and history. Shared: runs concurrently.
    #[handler]
    async fn index(&self, ctx: SharedObjectContext<'_>) -> Result<Json<SkillIndex>, HandlerError> {
        Ok(Json(
            ctx.get::<Json<SkillIndex>>(INDEX_STATE)
                .await?
                .map(Json::into_inner)
                .unwrap_or_default(),
        ))
    }

    /// One version's content, if registered. Shared: runs concurrently.
    #[handler]
    async fn version(
        &self,
        ctx: SharedObjectContext<'_>,
        version: Json<String>,
    ) -> Result<Json<Option<VersionContent>>, HandlerError> {
        let version = version.into_inner();
        // A version id is a hex digest; anything else names nothing (and no state key).
        if version.is_empty() || !version.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(Json(None));
        }
        Ok(Json(
            ctx.get::<Json<VersionContent>>(&content_key(&version))
                .await?
                .map(Json::into_inner),
        ))
    }
}

/// Why a registry call over the ingress failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegistryError {
    /// The ingress could not be reached, or answered something unreadable.
    Unreachable(String),
    /// The registry refused the request: the HTTP status and the ingress's message.
    Refused(u16, String),
}

impl std::fmt::Display for RegistryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(why) => write!(f, "the skill registry is unreachable: {why}"),
            Self::Refused(code, why) => write!(f, "the skill registry refused ({code}): {why}"),
        }
    }
}

impl std::error::Error for RegistryError {}

/// The registry over the Restate ingress (plain HTTP).
#[derive(Debug, Clone)]
pub struct RegistryClient {
    http: reqwest::Client,
    ingress: String,
}

impl RegistryClient {
    /// A client of the registry behind the ingress at `ingress` (e.g. `http://localhost:8080`).
    #[must_use]
    pub fn new(http: reqwest::Client, ingress: impl Into<String>) -> Self {
        Self {
            http,
            ingress: ingress.into(),
        }
    }

    /// Publish `bundle`.
    ///
    /// # Errors
    /// [`RegistryError`].
    pub async fn publish(&self, bundle: &SkillBundle) -> Result<PublishOutcome, RegistryError> {
        self.call("publish", Some(&PublishRequest::of(bundle)))
            .await
    }

    /// Activate a version.
    ///
    /// # Errors
    /// [`RegistryError`].
    pub async fn activate(
        &self,
        request: &ActivateRequest,
    ) -> Result<ActivateOutcome, RegistryError> {
        self.call("activate", Some(request)).await
    }

    /// The registry's index.
    ///
    /// # Errors
    /// [`RegistryError`].
    pub async fn index(&self) -> Result<SkillIndex, RegistryError> {
        self.call::<(), _>("index", None).await
    }

    /// One version's content, if registered.
    ///
    /// # Errors
    /// [`RegistryError`].
    pub async fn version(&self, version: &str) -> Result<Option<VersionContent>, RegistryError> {
        self.call("version", Some(&version)).await
    }

    async fn call<I: Serialize + ?Sized, O: serde::de::DeserializeOwned>(
        &self,
        handler: &str,
        input: Option<&I>,
    ) -> Result<O, RegistryError> {
        let url = format!(
            "{}/{SKILL_REGISTRY_OBJECT}/{SKILL_REGISTRY_KEY}/{handler}",
            self.ingress.trim_end_matches('/')
        );
        let mut request = self.http.post(url);
        if let Some(input) = input {
            let body =
                serde_json::to_vec(input).map_err(|e| RegistryError::Unreachable(e.to_string()))?;
            request = request
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(body);
        }
        let response = request
            .send()
            .await
            .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|e| RegistryError::Unreachable(e.to_string()))?;
        if !status.is_success() {
            // The ingress answers a terminal error as `{"message": …}`; keep the text otherwise.
            let message = serde_json::from_slice::<serde_json::Value>(&body)
                .ok()
                .and_then(|value| value.get("message")?.as_str().map(str::to_owned))
                .unwrap_or_else(|| String::from_utf8_lossy(&body).into_owned());
            return Err(RegistryError::Refused(status.as_u16(), message));
        }
        serde_json::from_slice(&body)
            .map_err(|e| RegistryError::Unreachable(format!("unreadable {handler} answer: {e}")))
    }
}

/// The skill pins a task reading through `profile` gets now: each skill's active version, when
/// that version is written for `profile`. Resolved when a token is minted, so a version
/// switched afterwards never changes the task.
#[must_use]
pub fn active_pins(index: &SkillIndex, profile: &str) -> Vec<SkillPin> {
    index
        .iter()
        .filter_map(|(name, entry)| {
            let info = entry.version(&entry.active)?;
            info.profiles
                .iter()
                .any(|id| id == profile)
                .then(|| SkillPin {
                    name: name.clone(),
                    version: info.version.clone(),
                    profiles: info.profiles.clone(),
                })
        })
        .collect()
}

impl RegistryClient {
    /// The skill pins a task reading through `profile` gets now ([`active_pins`]).
    ///
    /// # Errors
    /// [`RegistryError`].
    pub async fn active_pins(&self, profile: &str) -> Result<Vec<SkillPin>, RegistryError> {
        Ok(active_pins(&self.index().await?, profile))
    }
}

/// A task's skill pins for the agent proposer: the active versions written for its profile,
/// read from the registry when its token is minted.
#[async_trait]
impl ekg_cognition::SkillPins for RegistryClient {
    async fn pins(&self, profile: &str) -> ekg_domain::Result<Vec<SkillPin>> {
        self.active_pins(profile)
            .await
            .map_err(|e| ekg_domain::EngineError::Unavailable(format!("skill pins: {e}")))
    }
}

/// The default number of skill versions the gateway's source holds.
pub const DEFAULT_SKILL_CACHE_CAPACITY: usize = 64;

/// The tool gateway's skill source: versions read from the registry's shared `version`
/// handler over the ingress, cached by version. Versions are immutable, so the cache never
/// goes stale; it is bounded, evicting the oldest entry. A version is checked against its
/// digest before it is served.
pub struct RegistrySource {
    client: RegistryClient,
    capacity: usize,
    cache: Mutex<VersionCache>,
}

#[derive(Default)]
struct VersionCache {
    order: VecDeque<String>,
    bundles: HashMap<String, Arc<SkillBundle>>,
}

impl RegistrySource {
    /// A source over `client`, holding at most `capacity` versions (at least one).
    #[must_use]
    pub fn new(client: RegistryClient, capacity: usize) -> Self {
        Self {
            client,
            capacity: capacity.max(1),
            cache: Mutex::new(VersionCache::default()),
        }
    }

    fn cached(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String> {
        Ok(self.lock()?.bundles.get(version).cloned())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, VersionCache>, String> {
        self.cache
            .lock()
            .map_err(|_| "the skill cache is poisoned".to_owned())
    }
}

#[async_trait]
impl SkillSource for RegistrySource {
    async fn version(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String> {
        if let Some(bundle) = self.cached(version)? {
            return Ok(Some(bundle));
        }
        let Some(content) = self
            .client
            .version(version)
            .await
            .map_err(|e| e.to_string())?
        else {
            return Ok(None);
        };
        let bundle = SkillBundle::parse(&content.name, content.files).map_err(|e| e.to_string())?;
        if bundle.digest() != version {
            return Err(format!(
                "the registry's version {version} of {:?} does not match its digest",
                content.name
            ));
        }
        let bundle = Arc::new(bundle);
        let mut cache = self.lock()?;
        if !cache.bundles.contains_key(version) {
            while cache.order.len() >= self.capacity {
                if let Some(oldest) = cache.order.pop_front() {
                    cache.bundles.remove(&oldest);
                }
            }
            cache.order.push_back(version.to_owned());
            cache
                .bundles
                .insert(version.to_owned(), Arc::clone(&bundle));
        }
        Ok(Some(bundle))
    }
}

/// The repository's skill directory as a task's skills: every skill pinned at the digest of
/// its files as they are on disk. For running the agent on skills that are not published
/// (an offline eval of the working tree); a digest is the version the registry would give
/// the same content.
pub struct DirSkills {
    bundles: Vec<Arc<SkillBundle>>,
}

impl DirSkills {
    /// Read and validate every skill under `root` against `profiles`.
    ///
    /// # Errors
    /// A message naming every skill that failed to read, parse or validate.
    pub fn read(root: &std::path::Path, profiles: &ProfileTools) -> Result<Self, String> {
        let bundles = skills::read_and_validate_all(root, profiles).map_err(|errors| {
            errors
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("; ")
        })?;
        Ok(Self {
            bundles: bundles.into_iter().map(Arc::new).collect(),
        })
    }

    /// Each skill's version, by name.
    #[must_use]
    pub fn versions(&self) -> BTreeMap<String, String> {
        self.bundles
            .iter()
            .map(|bundle| (bundle.name().to_owned(), bundle.digest()))
            .collect()
    }

    /// The pins of a task reading through `profile`: the skills written for it.
    #[must_use]
    pub fn pins_for(&self, profile: &str) -> Vec<SkillPin> {
        self.bundles
            .iter()
            .filter(|bundle| bundle.profiles().iter().any(|id| id == profile))
            .map(|bundle| SkillPin {
                name: bundle.name().to_owned(),
                version: bundle.digest(),
                profiles: bundle.profiles().to_vec(),
            })
            .collect()
    }
}

#[async_trait]
impl ekg_cognition::SkillPins for DirSkills {
    async fn pins(&self, profile: &str) -> ekg_domain::Result<Vec<SkillPin>> {
        Ok(self.pins_for(profile))
    }
}

#[async_trait]
impl SkillSource for DirSkills {
    async fn version(&self, version: &str) -> Result<Option<Arc<SkillBundle>>, String> {
        Ok(self
            .bundles
            .iter()
            .find(|bundle| bundle.digest() == version)
            .cloned())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::*;

    fn bundle(name: &str, body: &str) -> SkillBundle {
        SkillBundle::parse(
            name,
            BTreeMap::from([(
                "SKILL.md".to_owned(),
                format!(
                    "---\nname: {name}\ndescription: d\nprofiles: [supply-risk]\ntools: [ground_entity]\n---\n{body}\n"
                ),
            )]),
        )
        .expect("parse")
    }

    fn activate(skill: &str, version: &str, by: &str, reason: &str) -> ActivateRequest {
        ActivateRequest {
            skill: skill.to_owned(),
            version: version.to_owned(),
            by: by.to_owned(),
            reason: reason.to_owned(),
        }
    }

    #[test]
    fn a_first_publish_registers_and_activates_with_a_publish_history_entry() {
        let mut index = SkillIndex::new();
        let v1 = bundle("plan", "one");
        let outcome = apply_publish(&mut index, &v1, 100);
        assert_eq!(
            outcome,
            PublishOutcome {
                skill: "plan".to_owned(),
                version: v1.digest(),
                created: true,
                active: v1.digest(),
            }
        );
        let entry = &index["plan"];
        assert_eq!(entry.active, v1.digest());
        assert_eq!(entry.versions.len(), 1);
        assert_eq!(entry.versions[0].published_at, 100);
        assert_eq!(entry.versions[0].profiles, vec!["supply-risk".to_owned()]);
        assert_eq!(
            entry.history,
            vec![Activation {
                version: v1.digest(),
                previous: None,
                by: PUBLISH_ACTOR.to_owned(),
                reason: "first version".to_owned(),
                at: 100,
            }]
        );
    }

    #[test]
    fn republishing_the_same_content_is_a_no_op() {
        let mut index = SkillIndex::new();
        let v1 = bundle("plan", "one");
        apply_publish(&mut index, &v1, 100);
        let before = index.clone();
        let outcome = apply_publish(&mut index, &v1, 200);
        assert!(!outcome.created);
        assert_eq!(outcome.active, v1.digest());
        assert_eq!(index, before);
    }

    #[test]
    fn a_second_version_is_registered_inactive() {
        let mut index = SkillIndex::new();
        let (v1, v2) = (bundle("plan", "one"), bundle("plan", "two"));
        apply_publish(&mut index, &v1, 100);
        let outcome = apply_publish(&mut index, &v2, 200);
        assert!(outcome.created);
        assert_eq!(outcome.active, v1.digest(), "the deploy changes nothing");
        let entry = &index["plan"];
        assert_eq!(entry.active, v1.digest());
        assert_eq!(
            entry
                .versions
                .iter()
                .map(|v| v.version.clone())
                .collect::<Vec<_>>(),
            vec![v1.digest(), v2.digest()]
        );
        assert_eq!(entry.history.len(), 1);
    }

    #[test]
    fn activation_and_rollback_switch_the_active_version_and_append_history_in_order() {
        let mut index = SkillIndex::new();
        let (v1, v2) = (bundle("plan", "one"), bundle("plan", "two"));
        apply_publish(&mut index, &v1, 100);
        apply_publish(&mut index, &v2, 200);
        let up = apply_activate(
            &mut index,
            &activate("plan", &v2.digest(), "ops", "better plans"),
            300,
        )
        .expect("activate");
        assert_eq!(up.previous, v1.digest());
        assert_eq!(index["plan"].active, v2.digest());
        let back = apply_activate(
            &mut index,
            &activate("plan", &v1.digest(), "ops", " rollback: v2 loops "),
            400,
        )
        .expect("rollback");
        assert_eq!(back.previous, v2.digest());
        let entry = &index["plan"];
        assert_eq!(entry.active, v1.digest());
        assert_eq!(entry.versions.len(), 2, "no version is deleted");
        let history: Vec<(String, Option<String>, &str, &str, u64)> = entry
            .history
            .iter()
            .map(|a| {
                (
                    a.version.clone(),
                    a.previous.clone(),
                    a.by.as_str(),
                    a.reason.as_str(),
                    a.at,
                )
            })
            .collect();
        assert_eq!(
            history,
            vec![
                (v1.digest(), None, PUBLISH_ACTOR, "first version", 100),
                (v2.digest(), Some(v1.digest()), "ops", "better plans", 300),
                (
                    v1.digest(),
                    Some(v2.digest()),
                    "ops",
                    "rollback: v2 loops",
                    400
                ),
            ]
        );
    }

    #[test]
    fn each_bad_activation_is_refused_and_changes_nothing() {
        let mut index = SkillIndex::new();
        let (v1, v2) = (bundle("plan", "one"), bundle("plan", "two"));
        apply_publish(&mut index, &v1, 100);
        apply_publish(&mut index, &v2, 200);
        let before = index.clone();
        for (request, code, says) in [
            (
                activate("nope", &v2.digest(), "ops", "r"),
                404,
                "unknown skill",
            ),
            (
                activate("plan", "abc123", "ops", "r"),
                404,
                "has no version",
            ),
            (
                activate("plan", &v2.digest(), " ", "r"),
                400,
                "who activates",
            ),
            (activate("plan", &v2.digest(), "ops", ""), 400, "reason"),
            (
                activate("plan", &v1.digest(), "ops", "r"),
                409,
                "already active",
            ),
        ] {
            let refusal = apply_activate(&mut index, &request, 300).expect_err(says);
            assert_eq!(refusal.code, code, "{refusal}");
            assert!(refusal.reason.contains(says), "{refusal}");
            assert_eq!(index, before);
        }
    }

    #[test]
    fn a_publish_is_re_validated_and_its_digest_checked() {
        let profiles: ProfileTools = BTreeMap::from([(
            "supply-risk".to_owned(),
            ["ground_entity".to_owned()].into_iter().collect(),
        )]);
        let v1 = bundle("plan", "one");
        let good = PublishRequest::of(&v1);
        assert_eq!(check_publish(&good, &profiles).expect("valid"), v1);
        let forged = PublishRequest {
            version: bundle("plan", "two").digest(),
            ..good.clone()
        };
        let refusal = check_publish(&forged, &profiles).expect_err("digest");
        assert_eq!(refusal.code, 400);
        assert!(refusal.reason.contains("digest"), "{refusal}");
        let refusal = check_publish(&good, &ProfileTools::new()).expect_err("unknown profile");
        assert!(
            refusal.reason.contains("unknown profile \"supply-risk\""),
            "{refusal}"
        );
    }

    #[test]
    fn active_pins_are_the_active_versions_written_for_the_profile() {
        let mut index = SkillIndex::new();
        let both = SkillBundle::parse(
            "both",
            BTreeMap::from([(
                "SKILL.md".to_owned(),
                "---\nname: both\ndescription: d\nprofiles: [supply-risk, process]\ntools: []\n---\nb\n".to_owned(),
            )]),
        )
        .expect("parse");
        let (v1, v2) = (bundle("plan", "one"), bundle("plan", "two"));
        apply_publish(&mut index, &both, 1);
        apply_publish(&mut index, &v1, 1);
        apply_publish(&mut index, &v2, 2);
        let names = |pins: Vec<SkillPin>| -> Vec<(String, String)> {
            pins.into_iter()
                .map(|pin| (pin.name, pin.version))
                .collect()
        };
        assert_eq!(
            names(active_pins(&index, "supply-risk")),
            vec![
                ("both".to_owned(), both.digest()),
                ("plan".to_owned(), v1.digest()),
            ],
            "the active version, not the newest"
        );
        assert_eq!(
            names(active_pins(&index, "process")),
            vec![("both".to_owned(), both.digest())]
        );
        assert!(active_pins(&index, "other").is_empty());
        let pin = &active_pins(&index, "process")[0];
        assert_eq!(
            pin.profiles,
            vec!["supply-risk".to_owned(), "process".to_owned()]
        );
    }

    /// A fake ingress serving the registry's `version` handler for `bundles`, counting requests.
    async fn fake_ingress(
        bundles: Vec<SkillBundle>,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let hits = Arc::new(AtomicUsize::new(0));
        let contents: HashMap<String, VersionContent> = bundles
            .iter()
            .map(|b| {
                (
                    b.digest(),
                    VersionContent {
                        name: b.name().to_owned(),
                        version: b.digest(),
                        files: b.files().clone(),
                    },
                )
            })
            .collect();
        let route = format!("/{SKILL_REGISTRY_OBJECT}/{SKILL_REGISTRY_KEY}/version");
        let counted = Arc::clone(&hits);
        let app = axum::Router::new().route(
            &route,
            axum::routing::post(move |axum::Json(version): axum::Json<String>| {
                counted.fetch_add(1, Ordering::SeqCst);
                let found = contents.get(&version).cloned();
                async move { axum::Json(found) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind");
        let port = listener.local_addr().expect("addr").port();
        tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
        (format!("http://127.0.0.1:{port}"), hits)
    }

    #[tokio::test]
    async fn the_source_caches_each_version_and_a_second_load_makes_no_request() {
        use std::sync::atomic::Ordering;
        let (v1, v2) = (bundle("plan", "one"), bundle("plan", "two"));
        let (ingress, hits) = fake_ingress(vec![v1.clone(), v2.clone()]).await;
        let source = RegistrySource::new(RegistryClient::new(reqwest::Client::new(), ingress), 1);
        let first = source
            .version(&v1.digest())
            .await
            .expect("load")
            .expect("found");
        assert_eq!(*first, v1);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let again = source
            .version(&v1.digest())
            .await
            .expect("load")
            .expect("found");
        assert_eq!(*again, v1);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "served from the cache");
        // Bounded: loading another version evicts the oldest.
        source
            .version(&v2.digest())
            .await
            .expect("load")
            .expect("found");
        source
            .version(&v1.digest())
            .await
            .expect("load")
            .expect("found");
        assert_eq!(hits.load(Ordering::SeqCst), 3);
        // An unknown version is none, and is not cached.
        assert!(source.version("abcdef").await.expect("load").is_none());
        assert!(source.version("abcdef").await.expect("load").is_none());
        assert_eq!(hits.load(Ordering::SeqCst), 5);
    }

    #[tokio::test]
    async fn the_directory_source_pins_each_skill_at_its_digest_for_its_profiles() {
        use ekg_cognition::tools::SkillSource as _;

        let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../hayhooks/skills"));
        let tools = |names: &[&str]| names.iter().map(|n| (*n).to_owned()).collect();
        let profiles: ProfileTools = BTreeMap::from([
            (
                "supply-risk".to_owned(),
                tools(&["ground_entity", "related_entities", "submit_plan"]),
            ),
            (
                "process".to_owned(),
                tools(&[
                    "ground_entity",
                    "related_entities",
                    "submit_plan",
                    "list_work_centres",
                    "list_work_centre_schedule",
                ]),
            ),
            (
                "portfolio".to_owned(),
                tools(&[
                    "ground_entity",
                    "related_entities",
                    "submit_plan",
                    "list_work_centres",
                    "list_work_centre_schedule",
                ]),
            ),
            (
                "supply-risk-analyst".to_owned(),
                tools(&["ground_entity", "related_entities", "submit_finding"]),
            ),
            (
                "process-analyst".to_owned(),
                tools(&[
                    "ground_entity",
                    "related_entities",
                    "submit_finding",
                    "list_work_centres",
                    "list_work_centre_schedule",
                ]),
            ),
        ]);
        let skills = DirSkills::read(root, &profiles).expect("the repository's skills");
        let names = |profile: &str| -> Vec<String> {
            skills
                .pins_for(profile)
                .into_iter()
                .map(|pin| pin.name)
                .collect()
        };
        assert_eq!(names("process"), ["process-work-centres", "propose-plan"]);
        assert_eq!(names("supply-risk"), ["propose-plan"]);
        assert_eq!(names("supply-risk-analyst"), ["analyse-member"]);
        assert_eq!(
            names("process-analyst"),
            ["analyse-member", "analyse-work-centres"]
        );
        assert_eq!(
            names("portfolio"),
            ["plan-portfolio", "process-work-centres", "propose-plan"]
        );

        let pin = &skills.pins_for("supply-risk")[0];
        let bundle = skills
            .version(&pin.version)
            .await
            .expect("source")
            .expect("the pinned version is served");
        assert_eq!(bundle.digest(), pin.version);
        assert_eq!(skills.versions()["propose-plan"], pin.version);
        assert!(skills.version("0000").await.expect("source").is_none());

        let unknown = DirSkills::read(root, &ProfileTools::new())
            .err()
            .expect("no profile is registered");
        assert!(unknown.contains("propose-plan"), "{unknown}");
    }
}
