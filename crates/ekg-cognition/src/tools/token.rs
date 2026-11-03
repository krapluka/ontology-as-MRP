//! Task tokens: the short-lived credential a reasoning task carries to the tool gateway.
//!
//! The format is fixed: `v1.<b64url(claims)>.<b64url(HMAC-SHA256(secret, "v1." +
//! b64url(claims)))>`, unpadded base64url, so there is no algorithm header to confuse.
//! The engine is the only issuer and the only verifier, with the same secret; the runtime
//! only carries the token as its bearer.
//!
//! A token may also pin skill versions, resolved from the active versions when it is minted,
//! so a version switched mid-run never changes a running task.

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ekg_domain::EntityUri;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use super::profile::ToolProfile;

/// The longest a task token may live. A longer requested lifetime is clamped to this.
pub const MAX_TOKEN_TTL: Duration = Duration::from_secs(600);

/// The shortest secret the engine signs with (256 bits, the HMAC-SHA256 block of entropy).
pub const MIN_SECRET_LEN: usize = 32;

/// The clock skew tolerated when checking a token's expiry, in seconds.
const CLOCK_SKEW_SECS: u64 = 5;

/// The format version every token starts with.
const VERSION: &str = "v1";

type HmacSha256 = Hmac<Sha256>;

/// What a task token names. Serialized as snake_case JSON inside the token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskClaims {
    /// The token id: random, unique per minted token (the scope cache key).
    pub jti: String,
    /// The opaque id of the tool profile the task reads through.
    pub profile: String,
    /// The task's subjects: the entities its scope is built from.
    pub subjects: Vec<EntityUri>,
    /// The tools the task may list and call.
    pub tools: Vec<String>,
    /// The expiry, in seconds since the Unix epoch.
    pub exp: u64,
    /// The task's trace id, for correlating every call with the runtime's spans.
    pub trace_id: String,
    /// The skill versions the task may load, by skill name. Serialized only when non-empty,
    /// so a token without pins carries no `skills` claim; a token without the key verifies
    /// as pinning none.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub skills: BTreeMap<String, String>,
}

/// A skill version to pin on a task token, with the profiles that version is written for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillPin {
    /// The skill's name.
    pub name: String,
    /// The version to pin.
    pub version: String,
    /// The profiles the version is written for: the token's profile must be one of them.
    pub profiles: Vec<String>,
}

/// What a task is granted: the claims a token is minted with, besides its profile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskGrant {
    /// The task's subjects: the entities its scope is built from (at least one).
    pub subjects: Vec<EntityUri>,
    /// The tools the task may list and call (each offered by the profile).
    pub tools: Vec<String>,
    /// The skill versions the task may load (each written for the profile).
    pub skills: Vec<SkillPin>,
    /// How long the token lives (clamped to [`MAX_TOKEN_TTL`]).
    pub ttl: Duration,
    /// The task's trace id.
    pub trace_id: String,
}

impl TaskGrant {
    /// A grant of `tools` over `subjects`, living `ttl`, traced as `trace_id`, pinning no skill.
    #[must_use]
    pub fn new(
        subjects: Vec<EntityUri>,
        tools: Vec<String>,
        ttl: Duration,
        trace_id: impl Into<String>,
    ) -> Self {
        Self {
            subjects,
            tools,
            skills: Vec::new(),
            ttl,
            trace_id: trace_id.into(),
        }
    }

    /// Also pin `skills`.
    #[must_use]
    pub fn with_skills(mut self, skills: Vec<SkillPin>) -> Self {
        self.skills = skills;
        self
    }
}

/// Why a token could not be minted, or was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenError {
    /// The configured secret is shorter than [`MIN_SECRET_LEN`] bytes.
    SecretTooShort(usize),
    /// A token must name at least one subject.
    NoSubjects,
    /// The named tool is not offered by the token's profile.
    ToolNotOffered(String),
    /// The named skill's pinned version is not written for the token's profile.
    SkillNotForProfile(String),
    /// The named skill is pinned twice.
    DuplicateSkill(String),
    /// The token is not `v1.<claims>.<mac>`, or its claims do not parse.
    Malformed,
    /// The MAC does not match the claims: the token was altered or signed elsewhere.
    BadSignature,
    /// The token has expired.
    Expired,
    /// The system clock or random source failed.
    Unavailable(String),
}

impl fmt::Display for TokenError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::SecretTooShort(len) => write!(
                f,
                "the task-token secret is {len} bytes; at least {MIN_SECRET_LEN} are required"
            ),
            Self::NoSubjects => f.write_str("a task token must name at least one subject"),
            Self::ToolNotOffered(tool) => {
                write!(f, "the profile does not offer the tool {tool:?}")
            }
            Self::SkillNotForProfile(skill) => {
                write!(f, "the skill {skill:?} is not written for the profile")
            }
            Self::DuplicateSkill(skill) => write!(f, "the skill {skill:?} is pinned twice"),
            Self::Malformed => f.write_str("malformed task token"),
            Self::BadSignature => f.write_str("task token signature mismatch"),
            Self::Expired => f.write_str("task token expired"),
            Self::Unavailable(why) => write!(f, "task token unavailable: {why}"),
        }
    }
}

impl std::error::Error for TokenError {}

impl TokenError {
    /// A short, claim-free reason for the gateway's refusal log line.
    #[must_use]
    pub fn reason(&self) -> &'static str {
        match self {
            Self::SecretTooShort(_) => "secret-too-short",
            Self::NoSubjects => "no-subjects",
            Self::ToolNotOffered(_) => "tool-not-offered",
            Self::SkillNotForProfile(_) => "skill-not-for-profile",
            Self::DuplicateSkill(_) => "duplicate-skill",
            Self::Malformed => "malformed",
            Self::BadSignature => "bad-signature",
            Self::Expired => "expired",
            Self::Unavailable(_) => "unavailable",
        }
    }
}

/// The task-token issuer and verifier: one secret, held only by the engine.
#[derive(Clone)]
pub struct TaskTokens {
    secret: Vec<u8>,
}

impl fmt::Debug for TaskTokens {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // Never print the secret.
        f.debug_struct("TaskTokens").finish_non_exhaustive()
    }
}

impl TaskTokens {
    /// Sign and verify with `secret`.
    ///
    /// # Errors
    /// [`TokenError::SecretTooShort`] when `secret` is shorter than [`MIN_SECRET_LEN`].
    pub fn new(secret: &[u8]) -> Result<Self, TokenError> {
        if secret.len() < MIN_SECRET_LEN {
            return Err(TokenError::SecretTooShort(secret.len()));
        }
        Ok(Self {
            secret: secret.to_vec(),
        })
    }

    /// Sign and verify with a random per-process secret: a single engine that both mints
    /// and verifies needs no shared secret.
    ///
    /// # Errors
    /// [`TokenError::Unavailable`] when the OS random source fails.
    pub fn random() -> Result<Self, TokenError> {
        let mut secret = vec![0_u8; MIN_SECRET_LEN];
        getrandom::fill(&mut secret).map_err(|e| TokenError::Unavailable(e.to_string()))?;
        Ok(Self { secret })
    }

    /// Mint a token for a task reading through `profile` with `grant`, living `grant.ttl`
    /// (clamped to [`MAX_TOKEN_TTL`]) from now.
    ///
    /// # Errors
    /// [`TokenError::NoSubjects`] for an empty subject set; [`TokenError::ToolNotOffered`]
    /// for a tool `profile` does not offer; [`TokenError::SkillNotForProfile`] for a skill
    /// pin not written for `profile`; [`TokenError::DuplicateSkill`] for a skill pinned twice;
    /// [`TokenError::Unavailable`] on a clock or random-source failure.
    pub fn mint(&self, profile: &ToolProfile, grant: TaskGrant) -> Result<String, TokenError> {
        self.mint_at(profile, grant, now_secs()?)
    }

    /// [`Self::mint`] against an explicit clock (`now`, seconds since the Unix epoch).
    ///
    /// # Errors
    /// As [`Self::mint`].
    pub fn mint_at(
        &self,
        profile: &ToolProfile,
        grant: TaskGrant,
        now: u64,
    ) -> Result<String, TokenError> {
        let TaskGrant {
            subjects,
            tools,
            skills,
            ttl,
            trace_id,
        } = grant;
        if subjects.is_empty() {
            return Err(TokenError::NoSubjects);
        }
        if let Some(tool) = tools.iter().find(|tool| profile.tool(tool).is_none()) {
            return Err(TokenError::ToolNotOffered(tool.clone()));
        }
        let mut pinned = BTreeMap::new();
        for pin in skills {
            if !pin.profiles.iter().any(|id| id == profile.id()) {
                return Err(TokenError::SkillNotForProfile(pin.name));
            }
            if pinned.contains_key(&pin.name) {
                return Err(TokenError::DuplicateSkill(pin.name));
            }
            pinned.insert(pin.name, pin.version);
        }
        let claims = TaskClaims {
            jti: random_id()?,
            profile: profile.id().to_owned(),
            subjects,
            tools,
            exp: now.saturating_add(ttl.min(MAX_TOKEN_TTL).as_secs()),
            trace_id,
            skills: pinned,
        };
        let json = serde_json::to_vec(&claims).map_err(|_| TokenError::Malformed)?;
        let signed = format!("{VERSION}.{}", URL_SAFE_NO_PAD.encode(json));
        Ok(format!("{signed}.{}", self.tag(&signed)))
    }

    /// The unpadded base64url MAC of `signed`.
    fn tag(&self, signed: &str) -> String {
        URL_SAFE_NO_PAD.encode(self.mac(signed).finalize().into_bytes())
    }

    /// Verify `token` against the clock `now` (seconds since the Unix epoch) and return
    /// its claims. Checks, in order: the version prefix, the MAC (constant-time), that the
    /// claims parse, and that the token has not expired (with a small clock skew).
    ///
    /// # Errors
    /// [`TokenError::Malformed`], [`TokenError::BadSignature`] or [`TokenError::Expired`].
    pub fn verify(&self, token: &str, now: u64) -> Result<TaskClaims, TokenError> {
        let rest = token
            .strip_prefix(VERSION)
            .and_then(|rest| rest.strip_prefix('.'))
            .ok_or(TokenError::Malformed)?;
        let (claims_b64, tag_b64) = rest.split_once('.').ok_or(TokenError::Malformed)?;
        if tag_b64.contains('.') {
            return Err(TokenError::Malformed);
        }
        let tag = URL_SAFE_NO_PAD
            .decode(tag_b64)
            .map_err(|_| TokenError::Malformed)?;
        let signed = &token[..VERSION.len() + 1 + claims_b64.len()];
        self.mac(signed)
            .verify_slice(&tag)
            .map_err(|_| TokenError::BadSignature)?;
        let json = URL_SAFE_NO_PAD
            .decode(claims_b64)
            .map_err(|_| TokenError::Malformed)?;
        let claims: TaskClaims =
            serde_json::from_slice(&json).map_err(|_| TokenError::Malformed)?;
        if now >= claims.exp.saturating_add(CLOCK_SKEW_SECS) {
            return Err(TokenError::Expired);
        }
        Ok(claims)
    }

    /// [`Self::verify`] against the system clock.
    ///
    /// # Errors
    /// As [`Self::verify`], or [`TokenError::Unavailable`] on a clock failure.
    pub fn verify_now(&self, token: &str) -> Result<TaskClaims, TokenError> {
        self.verify(token, now_secs()?)
    }

    fn mac(&self, signed: &str) -> HmacSha256 {
        // HMAC accepts a key of any length; the constructor already enforced the minimum.
        let mut mac = <HmacSha256 as KeyInit>::new_from_slice(&self.secret)
            .unwrap_or_else(|_| unreachable!("HMAC takes keys of any length"));
        mac.update(signed.as_bytes());
        mac
    }
}

/// Seconds since the Unix epoch, from the system clock.
///
/// # Errors
/// [`TokenError::Unavailable`] when the clock reads before the epoch.
pub fn now_secs() -> Result<u64, TokenError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .map_err(|e| TokenError::Unavailable(format!("system clock: {e}")))
}

/// A random 128-bit token id, as hex.
fn random_id() -> Result<String, TokenError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|e| TokenError::Unavailable(e.to_string()))?;
    Ok(bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use ekg_domain::ReadContext;

    use super::*;

    const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
    const NOW: u64 = 1_790_000_000;

    fn profile() -> ToolProfile {
        ToolProfile::new(
            "test-profile",
            ReadContext {
                role: "planner".to_owned(),
                purpose: "test".to_owned(),
            },
            None,
        )
    }

    fn subject() -> EntityUri {
        EntityUri("https://ekg.example/id/sale.order/7".to_owned())
    }

    fn grant(ttl: Duration) -> TaskGrant {
        TaskGrant::new(
            vec![subject()],
            vec!["ground_entity".to_owned()],
            ttl,
            "4bf92f3577b34da6a3ce929d0e0e4736",
        )
    }

    fn mint(ttl: Duration) -> String {
        TaskTokens::new(SECRET)
            .expect("secret")
            .mint_at(&profile(), grant(ttl), NOW)
            .expect("mint")
    }

    fn pin(name: &str, version: &str, profiles: &[&str]) -> SkillPin {
        SkillPin {
            name: name.to_owned(),
            version: version.to_owned(),
            profiles: profiles.iter().map(|&p| p.to_owned()).collect(),
        }
    }

    #[test]
    fn a_minted_token_round_trips_its_claims() {
        let token = mint(Duration::from_secs(60));
        let claims = TaskTokens::new(SECRET)
            .expect("secret")
            .verify(&token, NOW)
            .expect("verify");
        assert_eq!(claims.profile, "test-profile");
        assert_eq!(claims.subjects, vec![subject()]);
        assert_eq!(claims.tools, vec!["ground_entity".to_owned()]);
        assert_eq!(claims.exp, NOW + 60);
        assert_eq!(claims.trace_id, "4bf92f3577b34da6a3ce929d0e0e4736");
        assert_eq!(claims.jti.len(), 32);
        assert!(token.starts_with("v1."));
    }

    #[test]
    fn each_token_gets_its_own_id() {
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let a = tokens
            .verify(&mint(Duration::from_secs(60)), NOW)
            .expect("a");
        let b = tokens
            .verify(&mint(Duration::from_secs(60)), NOW)
            .expect("b");
        assert_ne!(a.jti, b.jti);
    }

    #[test]
    fn an_altered_claims_segment_is_refused() {
        let token = mint(Duration::from_secs(60));
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let mut claims = tokens.verify(&token, NOW).expect("verify");
        // Widen the scope: add another subject, keep the original MAC.
        claims
            .subjects
            .push(EntityUri("https://ekg.example/id/sale.order/8".to_owned()));
        let forged_claims = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims).expect("json"));
        let tag = token.rsplit('.').next().expect("tag");
        let forged = format!("v1.{forged_claims}.{tag}");
        assert_eq!(tokens.verify(&forged, NOW), Err(TokenError::BadSignature));
    }

    #[test]
    fn an_altered_mac_is_refused() {
        let token = mint(Duration::from_secs(60));
        let (signed, tag) = token.rsplit_once('.').expect("tag");
        let mut bytes = URL_SAFE_NO_PAD.decode(tag).expect("b64");
        bytes[0] ^= 1;
        let forged = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(bytes));
        let tokens = TaskTokens::new(SECRET).expect("secret");
        assert_eq!(tokens.verify(&forged, NOW), Err(TokenError::BadSignature));
        // A truncated MAC is refused too, not compared on its prefix.
        let full = URL_SAFE_NO_PAD.decode(tag).expect("b64");
        let truncated = format!("{signed}.{}", URL_SAFE_NO_PAD.encode(&full[..16]));
        assert_eq!(
            tokens.verify(&truncated, NOW),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn an_altered_prefix_is_refused() {
        let token = mint(Duration::from_secs(60));
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let forged = format!("v2{}", &token[2..]);
        assert_eq!(tokens.verify(&forged, NOW), Err(TokenError::Malformed));
        assert_eq!(tokens.verify("", NOW), Err(TokenError::Malformed));
        assert_eq!(tokens.verify("v1.only", NOW), Err(TokenError::Malformed));
        assert_eq!(
            tokens.verify(&format!("{token}.extra"), NOW),
            Err(TokenError::Malformed)
        );
    }

    #[test]
    fn a_token_signed_with_another_secret_is_refused() {
        let token = mint(Duration::from_secs(60));
        let other = TaskTokens::new(b"another-secret-of-at-least-32-bytes!").expect("secret");
        assert_eq!(other.verify(&token, NOW), Err(TokenError::BadSignature));
    }

    #[test]
    fn an_expired_token_is_refused() {
        let token = mint(Duration::from_secs(60));
        let tokens = TaskTokens::new(SECRET).expect("secret");
        // Within the skew it still passes; at the skew's end it is refused.
        assert!(
            tokens
                .verify(&token, NOW + 60 + CLOCK_SKEW_SECS - 1)
                .is_ok()
        );
        assert_eq!(
            tokens.verify(&token, NOW + 60 + CLOCK_SKEW_SECS),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn a_lifetime_over_the_cap_is_clamped() {
        let token = mint(Duration::from_secs(3600));
        let claims = TaskTokens::new(SECRET)
            .expect("secret")
            .verify(&token, NOW)
            .expect("verify");
        assert_eq!(claims.exp, NOW + MAX_TOKEN_TTL.as_secs());
    }

    #[test]
    fn a_too_short_secret_is_refused() {
        assert_eq!(
            TaskTokens::new(b"short").map(|_| ()),
            Err(TokenError::SecretTooShort(5))
        );
        assert!(TaskTokens::new(&[0_u8; MIN_SECRET_LEN]).is_ok());
    }

    #[test]
    fn a_random_secret_mints_and_verifies() {
        let tokens = TaskTokens::random().expect("random");
        let token = tokens
            .mint(
                &profile(),
                TaskGrant::new(vec![subject()], vec![], Duration::from_secs(30), "t"),
            )
            .expect("mint");
        assert!(tokens.verify_now(&token).is_ok());
    }

    #[test]
    fn minting_without_subjects_is_refused() {
        let result = TaskTokens::new(SECRET).expect("secret").mint_at(
            &profile(),
            TaskGrant::new(vec![], vec![], Duration::from_secs(60), "t"),
            NOW,
        );
        assert_eq!(result, Err(TokenError::NoSubjects));
    }

    #[test]
    fn minting_a_tool_the_profile_does_not_offer_is_refused() {
        let result = TaskTokens::new(SECRET).expect("secret").mint_at(
            &profile(),
            TaskGrant::new(
                vec![subject()],
                vec!["ground_entity".to_owned(), "write_order".to_owned()],
                Duration::from_secs(60),
                "t",
            ),
            NOW,
        );
        assert_eq!(
            result,
            Err(TokenError::ToolNotOffered("write_order".to_owned()))
        );
    }

    #[test]
    fn pinned_skills_round_trip_in_the_claims() {
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let token = tokens
            .mint_at(
                &profile(),
                grant(Duration::from_secs(60)).with_skills(vec![
                    pin("propose-plan", "aa11", &["other", "test-profile"]),
                    pin("explain", "bb22", &["test-profile"]),
                ]),
                NOW,
            )
            .expect("mint");
        let claims = tokens.verify(&token, NOW).expect("verify");
        assert_eq!(
            claims.skills,
            BTreeMap::from([
                ("explain".to_owned(), "bb22".to_owned()),
                ("propose-plan".to_owned(), "aa11".to_owned()),
            ])
        );
    }

    #[test]
    fn a_token_without_pins_carries_no_skills_claim_and_still_verifies() {
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let token = mint(Duration::from_secs(60));
        let claims_b64 = token.split('.').nth(1).expect("claims");
        let json =
            String::from_utf8(URL_SAFE_NO_PAD.decode(claims_b64).expect("b64")).expect("utf8");
        assert!(!json.contains("skills"), "{json}");
        // A token with no `skills` key at all verifies, pinning none.
        let legacy = serde_json::json!({
            "jti": "j", "profile": "test-profile", "subjects": [subject()],
            "tools": ["ground_entity"], "exp": NOW + 60, "trace_id": "t"
        });
        let signed = format!("v1.{}", URL_SAFE_NO_PAD.encode(legacy.to_string()));
        let legacy = format!("{signed}.{}", tokens.tag(&signed));
        let claims = tokens.verify(&legacy, NOW).expect("legacy verifies");
        assert!(claims.skills.is_empty());
    }

    #[test]
    fn a_pin_not_written_for_the_profile_or_pinned_twice_is_refused() {
        let tokens = TaskTokens::new(SECRET).expect("secret");
        let other = grant(Duration::from_secs(60)).with_skills(vec![pin(
            "propose-plan",
            "aa11",
            &["supply-risk", "process"],
        )]);
        assert_eq!(
            tokens.mint_at(&profile(), other, NOW),
            Err(TokenError::SkillNotForProfile("propose-plan".to_owned()))
        );
        let twice = grant(Duration::from_secs(60)).with_skills(vec![
            pin("propose-plan", "aa11", &["test-profile"]),
            pin("propose-plan", "bb22", &["test-profile"]),
        ]);
        assert_eq!(
            tokens.mint_at(&profile(), twice, NOW),
            Err(TokenError::DuplicateSkill("propose-plan".to_owned()))
        );
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let shown = format!("{:?}", TaskTokens::new(SECRET).expect("secret"));
        assert!(!shown.contains("0123456789abcdef"), "{shown}");
    }
}
