//! A throwaway world: a repository of its own that source records are materialized into, as
//! the ingest path would, and that is dropped again whatever its user did.
//!
//! The repository enforces the write-shapes like the production one, so a fixture the shapes
//! refuse fails when it is loaded. Its id carries a prefix, the process id and the mint time
//! (`{prefix}{pid}_{nanos}_{n}`), so a repository a killed process left behind can be told
//! apart and reclaimed by [`sweep_stale`].
//!
//! GraphDB Free caps a server at five repositories: hold one fixture world at a time.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use ekg_domain::{ReadPolicy, Result, SourceRecord, WorldModel};

use crate::mapping::MappingContract;
use crate::ontology;
use crate::world_model::{GraphDbClient, GraphDbWorldModel};

/// Distinguishes the fixture worlds one process mints in the same nanosecond.
static COUNTER: AtomicU64 = AtomicU64::new(0);

fn epoch_nanos(at: SystemTime) -> u128 {
    at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos())
}

/// When a fixture repository with `prefix` was minted, in nanoseconds since the Unix epoch.
/// `None` for any id that is not of the form `{prefix}{pid}_{nanos}_{n}`.
fn minted_at(prefix: &str, repo_id: &str) -> Option<u128> {
    let mut parts = repo_id.strip_prefix(prefix)?.split('_');
    let (pid, nanos, n) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some() {
        return None;
    }
    pid.parse::<u32>().ok()?;
    n.parse::<u64>().ok()?;
    nanos.parse::<u128>().ok()
}

/// The fixture repositories with `prefix` among `repo_ids` minted more than `max_age` before
/// `now`. A younger one may belong to another process, and an id that does not parse is not
/// ours to judge: both are left out.
fn stale(prefix: &str, repo_ids: &[String], now: SystemTime, max_age: Duration) -> Vec<String> {
    let now = epoch_nanos(now);
    repo_ids
        .iter()
        .filter(|id| {
            minted_at(prefix, id)
                .is_some_and(|minted| now.saturating_sub(minted) > max_age.as_nanos())
        })
        .cloned()
        .collect()
}

/// Drop the fixture repositories with `prefix` on `server` that were minted more than
/// `max_age` ago, and return their ids. One that cannot be dropped is logged and left.
///
/// # Errors
/// [`ekg_domain::EngineError::Backend`] if the server's repositories cannot be listed.
pub async fn sweep_stale(
    server: &GraphDbClient,
    prefix: &str,
    max_age: Duration,
) -> Result<Vec<String>> {
    let repo_ids = server.repository_ids().await?;
    let mut reclaimed = Vec::new();
    for repo in stale(prefix, &repo_ids, SystemTime::now(), max_age) {
        match server.drop_repo(&repo).await {
            Ok(()) => reclaimed.push(repo),
            Err(err) => {
                eprintln!("warning: fixture: stale repository {repo} was not dropped: {err}");
            }
        }
    }
    Ok(reclaimed)
}

/// One throwaway world and the owner of its repository. It owns the repository from before
/// the create request is sent, so whatever ends it (a discard, a failed load, a dropped
/// future, a panic) the repository is dropped.
pub struct FixtureWorld {
    server: Arc<GraphDbClient>,
    /// The repository's id; empty once [`FixtureWorld::discard`] dropped it.
    repo: String,
}

impl FixtureWorld {
    /// Create a repository `{prefix}{pid}_{nanos}_{n}` on `server`'s GraphDB, materialize
    /// `records` into it through `contract`, and run the structural derivations.
    ///
    /// # Errors
    /// [`ekg_domain::EngineError`] if the repository cannot be created, a record is refused
    /// (by the mapping or the write-shapes), or a derivation fails. The repository is dropped
    /// before the error is returned.
    pub async fn load(
        server: &GraphDbClient,
        prefix: &str,
        contract: MappingContract,
        records: &[SourceRecord],
    ) -> Result<Self> {
        let nanos = epoch_nanos(SystemTime::now());
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let world = Self {
            server: Arc::new(GraphDbClient::new(server.base_url(), server.repo_id())?),
            repo: format!("{prefix}{}_{nanos}_{n}", std::process::id()),
        };
        match world.fill(contract, records).await {
            Ok(()) => Ok(world),
            Err(err) => {
                world.discard().await;
                Err(err)
            }
        }
    }

    async fn fill(&self, contract: MappingContract, records: &[SourceRecord]) -> Result<()> {
        self.server.create_shacl_repo(&self.repo).await?;
        let writer = GraphDbWorldModel::new(self.client()?, contract);
        for record in records {
            writer.materialize(record).await?;
        }
        let client = self.client()?;
        for derivation in ontology::STRUCTURAL_DERIVATIONS {
            client.update(derivation.sparql).await?;
        }
        Ok(())
    }

    /// The repository's id.
    #[must_use]
    pub fn repo_id(&self) -> &str {
        &self.repo
    }

    /// A client on the world's repository.
    ///
    /// # Errors
    /// [`ekg_domain::EngineError::Backend`] if the HTTP client cannot be built.
    pub fn client(&self) -> Result<GraphDbClient> {
        GraphDbClient::new(self.server.base_url(), self.repo.as_str())
    }

    /// The world as a world model read under `read_policy`.
    ///
    /// # Errors
    /// As [`Self::client`].
    pub fn world_model(
        &self,
        contract: MappingContract,
        read_policy: ReadPolicy,
    ) -> Result<GraphDbWorldModel> {
        Ok(GraphDbWorldModel::new_with_read_policy(
            self.client()?,
            contract,
            read_policy,
        ))
    }

    /// Drop the repository now. If this future is itself dropped before the repository is
    /// gone, [`Drop`] still drops it.
    pub async fn discard(mut self) {
        drop_repo(&self.server, &self.repo).await;
        self.repo.clear();
    }
}

async fn drop_repo(server: &GraphDbClient, repo: &str) {
    if let Err(err) = server.drop_repo(repo).await {
        eprintln!("warning: fixture: repository {repo} was not dropped: {err}");
    }
}

impl Drop for FixtureWorld {
    fn drop(&mut self) {
        if self.repo.is_empty() {
            return;
        }
        // Async drop is not expressible, so the repository drop is spawned on the current
        // runtime. Outside a runtime it cannot be dropped here: it is logged, and the stale
        // sweep reclaims it.
        let repo = std::mem::take(&mut self.repo);
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                let server = Arc::clone(&self.server);
                handle.spawn(async move { drop_repo(&server, &repo).await });
            }
            Err(_) => eprintln!("warning: fixture: repository {repo} was not dropped: no runtime"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_old_repository_with_the_prefix_is_stale() {
        let now = SystemTime::now();
        let at = |ago: u64| {
            let minted = epoch_nanos(now) - u128::from(ago) * 1_000_000_000;
            format!("eval_4242_{minted}_7")
        };
        let ids = vec![
            at(7200),
            at(60),
            "eval_4242_soon_7".to_owned(),
            "eval_4242_1000".to_owned(),
            at(7200).replace("eval_", "sim_"),
            "ekg".to_owned(),
        ];
        assert_eq!(
            stale("eval_", &ids, now, Duration::from_secs(3600)),
            vec![at(7200)]
        );
    }
}
