//! Who an operator command is recorded as: `EKG_OPERATOR`, else `git config user.name`, else
//! `$USER`. Shared by the internal command binaries (`skills`, `rollout`), whose every governed
//! switch names who made it.

/// Resolves the operator identity from the environment.
///
/// # Errors
/// None of the three is set.
pub fn operator() -> Result<String, String> {
    let set = |value: Option<String>| value.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    set(std::env::var("EKG_OPERATOR").ok())
        .or_else(|| {
            set(std::process::Command::new("git")
                .args(["config", "user.name"])
                .output()
                .ok()
                .filter(|out| out.status.success())
                .map(|out| String::from_utf8_lossy(&out.stdout).into_owned()))
        })
        .or_else(|| set(std::env::var("USER").ok()))
        .ok_or_else(|| {
            "no operator identity: set EKG_OPERATOR (or git user.name, or USER)".to_owned()
        })
}
