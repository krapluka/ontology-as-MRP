//! Gated live e2e for the skill registry: the running stack's durable `SkillRegistry` (over
//! the Restate ingress) and the containerized backend's tool gateway at
//! `http://localhost:8091`.
//!
//! It works on a uniquely named test skill in a temp dir, never `propose-plan`, so the stack's
//! active skills stay untouched (a leftover test skill disappears with the next wipe):
//!
//! - publish: new and active; republish: unchanged;
//! - a task token minted with the stack's `EKG_TOOL_TOKEN_SECRET`, pinning the active version
//!   ([`active_pins`]);
//! - publish an edited copy: registered inactive; activate it: active;
//! - the token still loads the first version from the gateway;
//! - roll back: the first version is active again;
//! - the history lists every activation in order, with who, why and when.
//!
//! Ignored by default. Skips if the ingress, the gateway or the secret is unavailable, unless
//! `EKG_E2E_REQUIRE_INFRA=1` is set (as `just test-e2e` does), when that fails the run.
//!
//! ```text
//! just podman-local-up
//! set -a; source .env; set +a
//! cargo test -p ekg-e2e --test it skill_registry_e2e_it -- --ignored --nocapture
//! ```

#![allow(clippy::expect_used, clippy::doc_markdown)]

use crate::common;

use std::sync::Arc;
use std::time::Duration;

use ekg_backend::propose::{SUPPLY_RISK_PROFILE, supply_risk_profile};
use ekg_backend::skills::{
    ActivateRequest, PUBLISH_ACTOR, RegistryClient, RegistryError, SkillEntry,
};
use ekg_cognition::skills::SkillBundle;
use ekg_cognition::tools::{TaskGrant, TaskTokens};
use ekg_domain::fakes::InMemoryWorldModel;
use ekg_domain::{EntityUri, WorldModel};
use serde_json::{Value, json};

const TRACE: &str = "7d3c1b2a4e5f40718293a4b5c6d7e8f9";

/// Write the test skill `name` into `dir` with instructions `body`, and read it back.
fn write_skill(dir: &std::path::Path, name: &str, body: &str) -> SkillBundle {
    std::fs::create_dir_all(dir).expect("skill dir");
    std::fs::write(
        dir.join("SKILL.md"),
        format!(
            "---\nname: {name}\ndescription: A throwaway skill of the registry e2e.\nprofiles: [{SUPPLY_RISK_PROFILE}]\ntools: [ground_entity]\n---\n{body}\n"
        ),
    )
    .expect("write SKILL.md");
    SkillBundle::read_dir(dir).expect("read skill")
}

fn activation(skill: &str, version: &str, reason: &str) -> ActivateRequest {
    ActivateRequest {
        skill: skill.to_owned(),
        version: version.to_owned(),
        by: "skill-registry-e2e".to_owned(),
        reason: reason.to_owned(),
    }
}

/// A task token pinning `skill`'s active version for the supply-risk profile, minted as the
/// engine would with the stack's `secret` (the world is not read to mint).
async fn mint_pinning_active(registry: &RegistryClient, secret: &str, skill: &str) -> String {
    let pins: Vec<_> = registry
        .active_pins(SUPPLY_RISK_PROFILE)
        .await
        .expect("active pins")
        .into_iter()
        .filter(|pin| pin.name == skill)
        .collect();
    assert_eq!(pins.len(), 1, "{pins:?}");
    let profile = supply_risk_profile(
        Arc::new(InMemoryWorldModel::default()) as Arc<dyn WorldModel>,
        ekg_cognition::DEFAULT_MAX_PLAN_ACTIONS,
    );
    TaskTokens::new(secret.trim().as_bytes())
        .expect("secret")
        .mint(
            &profile,
            TaskGrant::new(
                vec![EntityUri("https://ekg.example/id/sale.order/1".to_owned())],
                vec![],
                Duration::from_secs(120),
                TRACE,
            )
            .with_skills(pins),
        )
        .expect("mint")
}

/// `GET <gateway><path>` with the task token: the status and the JSON body.
async fn gateway_get(gateway: &str, path: &str, token: &str) -> (u16, Value) {
    let response = reqwest::Client::new()
        .get(format!("{gateway}{path}"))
        .header("authorization", format!("Bearer {token}"))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .expect("gateway request");
    let status = response.status().as_u16();
    let body = response.bytes().await.expect("body");
    (status, serde_json::from_slice(&body).unwrap_or(Value::Null))
}

/// The history lists every activation in order (the first version's, the activation of `d2`,
/// the rollback to `d1`), each with who, why and when.
fn assert_history(entry: &SkillEntry, d1: &str, d2: &str) {
    let history: Vec<(&str, Option<&str>, &str, &str)> = entry
        .history
        .iter()
        .map(|a| {
            (
                a.version.as_str(),
                a.previous.as_deref(),
                a.by.as_str(),
                a.reason.as_str(),
            )
        })
        .collect();
    assert_eq!(
        history,
        vec![
            (d1, None, PUBLISH_ACTOR, "first version"),
            (d2, Some(d1), "skill-registry-e2e", "try version two"),
            (
                d1,
                Some(d2),
                "skill-registry-e2e",
                "roll back to version one"
            ),
        ]
    );
    let times: Vec<u64> = entry.history.iter().map(|a| a.at).collect();
    assert!(
        times[0] > 0 && times.windows(2).all(|w| w[0] <= w[1]),
        "{times:?}"
    );
}

#[tokio::test]
#[ignore = "requires the live stack with the skill registry (run with --ignored)"]
async fn a_skill_is_published_activated_rolled_back_and_served_at_its_pinned_version() {
    let ingress = common::env_or("RESTATE_INGRESS_URL", "http://localhost:8080");
    let gateway = common::env_or("EKG_TOOL_GATEWAY_URL", "http://localhost:8091/mcp")
        .trim_end_matches("/mcp")
        .to_owned();
    let Some(secret) = std::env::var("EKG_TOOL_TOKEN_SECRET")
        .ok()
        .filter(|secret| !secret.trim().is_empty())
    else {
        common::infra_unavailable(
            "EKG_TOOL_TOKEN_SECRET unset (run `just podman-local-up`, then `set -a; source .env`)",
        );
        return;
    };
    let registry = RegistryClient::new(reqwest::Client::new(), ingress.clone());
    match registry.index().await {
        Ok(_) => {}
        Err(RegistryError::Unreachable(why)) => {
            common::infra_unavailable(&format!("Restate ingress unreachable at {ingress}: {why}"));
            return;
        }
        Err(error) => {
            panic!("the stack has no SkillRegistry (re-run `just podman-local-up`): {error}")
        }
    }

    let name = format!("e2e-skill-{}", common::unique_base());
    let dir = std::env::temp_dir().join(&name);
    let v1 = write_skill(&dir, &name, "Version one: ground first.");

    // Publish: new and active (its first version). Republish: unchanged.
    let first = registry.publish(&v1).await.expect("publish v1");
    assert!(first.created, "{first:?}");
    assert_eq!(first.active, v1.digest());
    let again = registry.publish(&v1).await.expect("republish v1");
    assert!(!again.created, "{again:?}");
    assert_eq!(again.active, v1.digest());

    // A task started now pins the active version.
    let token = mint_pinning_active(&registry, &secret, &name).await;

    // An edited copy is a new version, registered inactive: the deploy changes nothing.
    let v2 = write_skill(&dir, &name, "Version two: ground, then follow the cause.");
    assert_ne!(v1.digest(), v2.digest());
    let second = registry.publish(&v2).await.expect("publish v2");
    assert!(second.created, "{second:?}");
    assert_eq!(second.active, v1.digest(), "a new version stays inactive");

    // Activate it: active for tasks started from now on.
    let up = registry
        .activate(&activation(&name, &v2.digest(), "try version two"))
        .await
        .expect("activate v2");
    assert_eq!(up.previous, v1.digest());

    // The running task still loads the version its token pins.
    let (status, list) = gateway_get(&gateway, "/skills", &token).await;
    assert_eq!(status, 200, "{list}");
    assert_eq!(
        list,
        json!([{ "name": name, "description": "A throwaway skill of the registry e2e." }])
    );
    let (status, loaded) = gateway_get(&gateway, &format!("/skills/{name}"), &token).await;
    assert_eq!(status, 200, "{loaded}");
    assert_eq!(loaded["version"], json!(v1.digest()), "{loaded}");
    assert_eq!(
        loaded["body"],
        json!("Version one: ground first.\n"),
        "{loaded}"
    );
    let (status, _) = gateway_get(&gateway, "/skills/propose-plan", &token).await;
    assert_eq!(status, 404, "an unpinned skill is not served");

    // Roll back: the first version is active again, and no version was deleted.
    let back = registry
        .activate(&activation(&name, &v1.digest(), "roll back to version one"))
        .await
        .expect("rollback");
    assert_eq!(back.previous, v2.digest());
    let index = registry.index().await.expect("index");
    let entry = index.get(&name).expect("the test skill is registered");
    assert_eq!(entry.active, v1.digest());
    assert_eq!(entry.versions.len(), 2);

    assert_history(entry, &v1.digest(), &v2.digest());

    let _ = std::fs::remove_dir_all(&dir);
}
