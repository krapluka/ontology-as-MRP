//! Minimum-Rust drift check.
//!
//! The workspace declares one `rust-version` in the root `Cargo.toml`, inherited by every
//! crate. This test reads the resolved dependency graph from the committed lockfile
//! (`cargo metadata --locked --offline`, no network) and fails when any package in it (normal,
//! build or dev dependency, on any target) declares a higher minimum than the workspace, or
//! when the workspace crates disagree on one. So the declared minimum cannot silently fall
//! behind a dependency bump. It needs no running stack.
//!
//! ```text
//! cargo test -p ekg-backend --test it workspace_rust_version
//! ```

#![allow(clippy::expect_used)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::Command;

use serde_json::Value;

/// A Rust version as (major, minor, patch); a missing part reads as 0, so `1.90` == `1.90.0`.
type Version = (u64, u64, u64);

/// Parse a `rust-version` value (`1.90`, `1.90.0`), or `None` if it is not one.
fn parse_version(raw: &str) -> Option<Version> {
    let mut parts = raw.trim().split('.').map(str::parse::<u64>);
    let major = parts.next()?.ok()?;
    let minor = parts.next().unwrap_or(Ok(0)).ok()?;
    let patch = parts.next().unwrap_or(Ok(0)).ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

/// One package as `cargo metadata` describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Package {
    name: String,
    version: String,
    rust_version: Option<String>,
}

fn package(name: &str, version: &str, rust_version: Option<&str>) -> Package {
    Package {
        name: name.to_owned(),
        version: version.to_owned(),
        rust_version: rust_version.map(str::to_owned),
    }
}

/// Check the workspace `members` share one minimum and that no `resolved` package needs a
/// newer one. `Ok` carries the shared minimum; `Err` is the full, sorted report.
fn check(members: &[Package], resolved: &[Package]) -> Result<String, String> {
    let declared: BTreeSet<Option<&str>> = members
        .iter()
        .map(|member| member.rust_version.as_deref())
        .collect();
    let minimum = if let [Some(minimum)] = declared.iter().collect::<Vec<_>>().as_slice() {
        (*minimum).to_owned()
    } else {
        let lines: BTreeSet<String> = members
            .iter()
            .map(|member| {
                format!(
                    "  {} declares {}",
                    member.name,
                    member.rust_version.as_deref().unwrap_or("no rust-version")
                )
            })
            .collect();
        return Err(format!(
            "the workspace crates must share one rust-version (inherit it with \
             `rust-version.workspace = true`):\n{}",
            lines.into_iter().collect::<Vec<_>>().join("\n")
        ));
    };
    let floor = parse_version(&minimum)
        .ok_or_else(|| format!("the workspace rust-version {minimum:?} is not a version"))?;
    let offenders: BTreeSet<String> = resolved
        .iter()
        .filter_map(|package| {
            let needs = package.rust_version.as_deref()?;
            (parse_version(needs)? > floor)
                .then(|| format!("  {} {} needs {needs}", package.name, package.version))
        })
        .collect();
    if offenders.is_empty() {
        return Ok(minimum);
    }
    Err(format!(
        "the workspace declares rust-version {minimum}, but these resolved packages need a newer \
         Rust:\n{}\nfix: raise `rust-version` in the root Cargo.toml to the highest of these (and \
         fix the lints it enables), or pin the dependency to a version that supports {minimum}",
        offenders.into_iter().collect::<Vec<_>>().join("\n")
    ))
}

/// The workspace members and every resolved package, read from `cargo metadata`.
fn metadata() -> (Vec<Package>, Vec<Package>) {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version", "1", "--locked", "--offline"])
        .current_dir(&root)
        .output()
        .expect("run cargo metadata");
    assert!(
        output.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).expect("metadata json");
    let by_id: BTreeMap<&str, Package> = json["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .map(|p| {
            (
                p["id"].as_str().expect("id"),
                package(
                    p["name"].as_str().expect("name"),
                    p["version"].as_str().expect("version"),
                    p["rust_version"].as_str(),
                ),
            )
        })
        .collect();
    let lookup = |id: &Value| {
        by_id
            .get(id.as_str().expect("package id"))
            .cloned()
            .expect("a listed package")
    };
    let members = json["workspace_members"]
        .as_array()
        .expect("workspace members")
        .iter()
        .map(lookup)
        .collect();
    let resolved = json["resolve"]["nodes"]
        .as_array()
        .expect("resolve nodes")
        .iter()
        .map(|node| lookup(&node["id"]))
        .collect();
    (members, resolved)
}

#[test]
fn the_declared_minimum_covers_every_resolved_dependency() {
    let (members, resolved) = metadata();
    assert!(!members.is_empty() && !resolved.is_empty());
    if let Err(report) = check(&members, &resolved) {
        panic!("{report}");
    }
}

// --- the comparison ---

fn members(rust_version: &str) -> Vec<Package> {
    vec![
        package("ekg-domain", "0.1.0", Some(rust_version)),
        package("ekg-backend", "0.1.0", Some(rust_version)),
    ]
}

#[test]
fn versions_compare_numerically_with_a_missing_patch_as_zero() {
    assert_eq!(parse_version("1.90"), parse_version("1.90.0"));
    assert!(parse_version("1.100") > parse_version("1.90"));
    assert!(parse_version("1.89.9") < parse_version("1.90"));
    assert_eq!(parse_version("1.9x"), None);
    assert_eq!(parse_version("1.90.0.1"), None);
}

#[test]
fn a_dependency_needing_a_newer_rust_is_named() {
    let resolved = [
        package("restate-sdk", "0.12.0", Some("1.90.0")),
        package("async-graphql", "7.2.1", Some("1.89.0")),
        package("serde", "1.0.0", Some("1.61")),
    ];
    let report = check(&members("1.85"), &resolved).expect_err("too old");
    assert!(report.contains("declares rust-version 1.85"), "{report}");
    assert!(
        report.contains("  async-graphql 7.2.1 needs 1.89.0\n  restate-sdk 0.12.0 needs 1.90.0"),
        "sorted, one line each: {report}"
    );
    assert!(!report.contains("serde"), "{report}");
}

#[test]
fn an_equal_or_lower_minimum_passes() {
    let resolved = [
        package("restate-sdk", "0.12.0", Some("1.90.0")),
        package("time", "0.3.47", Some("1.88")),
    ];
    assert_eq!(check(&members("1.90"), &resolved), Ok("1.90".to_owned()));
}

#[test]
fn a_package_without_a_minimum_is_ignored() {
    let resolved = [package("legacy", "0.1.0", None)];
    assert!(check(&members("1.90"), &resolved).is_ok());
}

#[test]
fn a_workspace_crate_with_its_own_minimum_is_named() {
    let mut crates = members("1.90");
    crates.push(package("ekg-odoo-seed", "0.1.0", Some("1.85")));
    let report = check(&crates, &[]).expect_err("disagreement");
    assert!(report.contains("ekg-odoo-seed declares 1.85"), "{report}");
    let mut crates = members("1.90");
    crates.push(package("ekg-odoo-seed", "0.1.0", None));
    let report = check(&crates, &[]).expect_err("missing");
    assert!(
        report.contains("ekg-odoo-seed declares no rust-version"),
        "{report}"
    );
}
