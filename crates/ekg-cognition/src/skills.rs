//! Agent skills: the repetitive procedures an agent follows, as editable text authored in git,
//! versioned by content and served only through the engine.
//!
//! A skill is a directory holding a `SKILL.md` (strict frontmatter, then markdown
//! instructions) and optionally bundled text files:
//!
//! ```text
//! <name>/
//!   SKILL.md          ---
//!                     name: <name>
//!                     description: <one line>
//!                     profiles: [<profile>, ...]
//!                     tools: [<tool>, ...]
//!                     ---
//!                     <markdown instructions>
//!   reference/*.md    optional bundled text files
//! ```
//!
//! This module parses a [`SkillBundle`] from a directory or an in-memory file map, computes
//! its content digest (the version id), and validates it against the tools each profile
//! offers. It names no capability and knows no registry.
//!
//! Skills are guidance; the tools enforce the rules. A skill can only name tools its profiles
//! offer, so a badly written skill gives a worse plan, never an unsafe one.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::{self, Write as _};
use std::path::Path;

use sha2::{Digest, Sha256};

/// The instructions file every skill has.
pub const SKILL_FILE: &str = "SKILL.md";
/// The largest file a skill may bundle, in bytes.
pub const MAX_FILE_BYTES: usize = 64 * 1024;
/// The largest a skill may be in total, in bytes (every version fits durable state).
pub const MAX_SKILL_BYTES: usize = 256 * 1024;
/// The longest skill name.
pub const MAX_NAME_LEN: usize = 64;
/// The shortest version prefix tooling accepts for a version id.
pub const MIN_VERSION_PREFIX: usize = 8;

/// The tools each registered profile offers, by profile id: what a skill is validated against.
pub type ProfileTools = BTreeMap<String, BTreeSet<String>>;

/// Why a skill was refused: the skill and each defect.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillError {
    /// The skill's name (its directory name when it could not be parsed).
    pub skill: String,
    /// Every defect found, each as one sentence.
    pub defects: Vec<String>,
}

impl SkillError {
    fn one(skill: &str, defect: impl Into<String>) -> Self {
        Self {
            skill: skill.to_owned(),
            defects: vec![defect.into()],
        }
    }
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "skill {:?}: {}", self.skill, self.defects.join("; "))
    }
}

impl std::error::Error for SkillError {}

/// A skill's frontmatter. These four keys are required and no other key is allowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillFrontmatter {
    /// The skill's name, equal to its directory name.
    pub name: String,
    /// One line on when to use the skill, shown to the agent up front.
    pub description: String,
    /// The tool profiles the skill is written for (at least one).
    pub profiles: Vec<String>,
    /// The tools the skill relies on.
    pub tools: Vec<String>,
}

/// A parsed skill: its frontmatter, its instructions body and every file (paths relative to
/// the skill root, POSIX-style; `SKILL.md` included), all UTF-8 text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillBundle {
    frontmatter: SkillFrontmatter,
    body: String,
    files: BTreeMap<String, String>,
}

impl SkillBundle {
    /// Parse the skill `name` from its files (path to content). The files must include
    /// `SKILL.md`; every path must be relative POSIX with no `.` or `..` segment; sizes stay
    /// within [`MAX_FILE_BYTES`] and [`MAX_SKILL_BYTES`]; the frontmatter is strict; and its
    /// `name` must equal `name` (a skill's directory name).
    ///
    /// # Errors
    /// [`SkillError`] naming the skill and the first defect found.
    pub fn parse(name: &str, files: BTreeMap<String, String>) -> Result<Self, SkillError> {
        check_name(name).map_err(|defect| SkillError::one(name, defect))?;
        let mut total = 0_usize;
        for (path, content) in &files {
            check_path(path).map_err(|defect| SkillError::one(name, defect))?;
            check_size(path, content.len()).map_err(|defect| SkillError::one(name, defect))?;
            total += content.len();
        }
        if total > MAX_SKILL_BYTES {
            return Err(SkillError::one(
                name,
                format!("the skill is {total} bytes, over the {MAX_SKILL_BYTES}-byte limit"),
            ));
        }
        let skill_md = files
            .get(SKILL_FILE)
            .ok_or_else(|| SkillError::one(name, format!("it has no {SKILL_FILE}")))?;
        let (frontmatter, body) =
            parse_frontmatter(skill_md).map_err(|defect| SkillError::one(name, defect))?;
        if frontmatter.name != name {
            return Err(SkillError::one(
                name,
                format!(
                    "its frontmatter name {:?} differs from its directory name",
                    frontmatter.name
                ),
            ));
        }
        Ok(Self {
            frontmatter,
            body,
            files,
        })
    }

    /// Read and parse the skill directory `dir` (its name is the directory's). Entries whose
    /// name starts with `.` (editor and OS litter) are skipped; anything else must be a UTF-8
    /// text file or a directory of them.
    ///
    /// # Errors
    /// [`SkillError`] naming the skill and the defect: an unreadable entry, a binary file, an
    /// over-size file, or any [`Self::parse`] defect.
    pub fn read_dir(dir: &Path) -> Result<Self, SkillError> {
        let name = dir
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default()
            .to_owned();
        let mut files = BTreeMap::new();
        read_files(dir, "", &name, &mut files)?;
        Self::parse(&name, files)
    }

    /// The skill's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.frontmatter.name
    }

    /// The skill's one-line description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.frontmatter.description
    }

    /// The profiles the skill is written for.
    #[must_use]
    pub fn profiles(&self) -> &[String] {
        &self.frontmatter.profiles
    }

    /// The tools the skill relies on.
    #[must_use]
    pub fn tools(&self) -> &[String] {
        &self.frontmatter.tools
    }

    /// The skill's frontmatter.
    #[must_use]
    pub fn frontmatter(&self) -> &SkillFrontmatter {
        &self.frontmatter
    }

    /// The instructions: `SKILL.md` without its frontmatter.
    #[must_use]
    pub fn body(&self) -> &str {
        &self.body
    }

    /// Every file of the skill, `SKILL.md` included, by path.
    #[must_use]
    pub fn files(&self) -> &BTreeMap<String, String> {
        &self.files
    }

    /// The bundled files' paths (every file but `SKILL.md`), sorted.
    #[must_use]
    pub fn bundled(&self) -> Vec<&str> {
        self.files
            .keys()
            .map(String::as_str)
            .filter(|path| *path != SKILL_FILE)
            .collect()
    }

    /// The bundled file at `path` (never `SKILL.md`, whose body is [`Self::body`]).
    #[must_use]
    pub fn bundled_file(&self, path: &str) -> Option<&str> {
        (path != SKILL_FILE)
            .then(|| self.files.get(path).map(String::as_str))
            .flatten()
    }

    /// The skill's version id: the hex SHA-256 over its files sorted by path, each encoded as
    /// `path \0 len \0 bytes` (`len` in decimal), so any change to any file is a new version.
    #[must_use]
    pub fn digest(&self) -> String {
        digest(&self.files)
    }
}

/// The content digest of `files` (see [`SkillBundle::digest`]).
#[must_use]
pub fn digest(files: &BTreeMap<String, String>) -> String {
    let mut hash = Sha256::new();
    // A BTreeMap iterates in path order, whatever order the files were read in.
    for (path, content) in files {
        hash.update(path.as_bytes());
        hash.update([0]);
        hash.update(content.len().to_string().as_bytes());
        hash.update([0]);
        hash.update(content.as_bytes());
    }
    hash.finalize()
        .iter()
        .fold(String::with_capacity(64), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Validate `bundle` against the registered profiles: it names at least one profile, every
/// profile it names is registered, and each of them offers every tool it names.
///
/// # Errors
/// [`SkillError`] naming the skill and every defect.
pub fn validate(bundle: &SkillBundle, profiles: &ProfileTools) -> Result<(), SkillError> {
    let mut defects = Vec::new();
    for profile in bundle.profiles() {
        match profiles.get(profile) {
            None => defects.push(format!("unknown profile {profile:?}")),
            Some(offered) => {
                for tool in bundle.tools() {
                    if !offered.contains(tool) {
                        defects.push(format!("profile {profile:?} does not offer tool {tool:?}"));
                    }
                }
            }
        }
    }
    if defects.is_empty() {
        Ok(())
    } else {
        Err(SkillError {
            skill: bundle.name().to_owned(),
            defects,
        })
    }
}

/// Read and validate every skill directory under `root` (each subdirectory is one skill;
/// entries starting with `.` are skipped), in name order.
///
/// # Errors
/// Every skill that failed to read, parse or validate, or one error for an unreadable root.
pub fn read_and_validate_all(
    root: &Path,
    profiles: &ProfileTools,
) -> Result<Vec<SkillBundle>, Vec<SkillError>> {
    let root_name = root.display().to_string();
    let entries = std::fs::read_dir(root).map_err(|e| {
        vec![SkillError::one(
            &root_name,
            format!("cannot read the skills directory: {e}"),
        )]
    })?;
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            vec![SkillError::one(
                &root_name,
                format!("cannot read the skills directory: {e}"),
            )]
        })?;
        let hidden = entry.file_name().to_string_lossy().starts_with('.');
        if !hidden && entry.path().is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    let (mut bundles, mut errors) = (Vec::new(), Vec::new());
    for dir in dirs {
        match SkillBundle::read_dir(&dir).and_then(|bundle| {
            validate(&bundle, profiles)?;
            Ok(bundle)
        }) {
            Ok(bundle) => bundles.push(bundle),
            Err(error) => errors.push(error),
        }
    }
    if errors.is_empty() {
        Ok(bundles)
    } else {
        Err(errors)
    }
}

/// Read every file under `dir` into `files`, keyed by its path below the skill root.
fn read_files(
    dir: &Path,
    prefix: &str,
    skill: &str,
    files: &mut BTreeMap<String, String>,
) -> Result<(), SkillError> {
    let unreadable =
        |e: std::io::Error| SkillError::one(skill, format!("cannot read {}: {e}", dir.display()));
    for entry in std::fs::read_dir(dir).map_err(unreadable)? {
        let entry = entry.map_err(unreadable)?;
        let file_name = entry.file_name();
        let Some(file_name) = file_name.to_str() else {
            return Err(SkillError::one(
                skill,
                format!("{} has a non-UTF-8 name", entry.path().display()),
            ));
        };
        if file_name.starts_with('.') {
            continue;
        }
        let path = format!("{prefix}{file_name}");
        let kind = entry.file_type().map_err(unreadable)?;
        if kind.is_dir() {
            read_files(&entry.path(), &format!("{path}/"), skill, files)?;
        } else if kind.is_file() {
            let bytes = std::fs::read(entry.path()).map_err(unreadable)?;
            check_size(&path, bytes.len()).map_err(|defect| SkillError::one(skill, defect))?;
            let text = String::from_utf8(bytes)
                .map_err(|_| SkillError::one(skill, format!("{path} is not UTF-8 text")))?;
            files.insert(path, text);
        } else {
            return Err(SkillError::one(
                skill,
                format!("{path} is not a regular file"),
            ));
        }
    }
    Ok(())
}

/// A skill name is lowercase kebab-case: `[a-z0-9]+(-[a-z0-9]+)*`, at most [`MAX_NAME_LEN`].
fn check_name(name: &str) -> Result<(), String> {
    let kebab = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name.split('-').all(|part| {
            !part.is_empty()
                && part
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        });
    if kebab {
        Ok(())
    } else {
        Err(format!(
            "the name {name:?} is not lowercase kebab-case of at most {MAX_NAME_LEN} characters"
        ))
    }
}

/// A file path is relative POSIX: non-empty segments, none `.` or `..`, no backslash.
fn check_path(path: &str) -> Result<(), String> {
    let relative = !path.is_empty()
        && !path.contains('\\')
        && path
            .split('/')
            .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if relative {
        Ok(())
    } else {
        Err(format!(
            "the path {path:?} is not a relative POSIX path inside the skill"
        ))
    }
}

fn check_size(path: &str, len: usize) -> Result<(), String> {
    if len > MAX_FILE_BYTES {
        Err(format!(
            "{path} is {len} bytes, over the {MAX_FILE_BYTES}-byte file limit"
        ))
    } else {
        Ok(())
    }
}

/// Split `SKILL.md` into its frontmatter and its body: the first line is `---`, then the
/// keys `name`, `description`, `profiles` and `tools` as `key: value` lines (the two lists
/// as `[a, b]`), then a closing `---`. Anything else is refused, naming the line.
fn parse_frontmatter(text: &str) -> Result<(SkillFrontmatter, String), String> {
    let lines: Vec<&str> = text.split('\n').collect();
    if lines.first().map(|line| line.trim_end()) != Some("---") {
        return Err(format!(
            "{SKILL_FILE} must start with a `---` frontmatter line"
        ));
    }
    let closing = lines
        .iter()
        .skip(1)
        .position(|line| line.trim_end() == "---")
        .map(|index| index + 1)
        .ok_or_else(|| "the frontmatter is never closed with a `---` line".to_owned())?;
    let mut values: BTreeMap<&str, &str> = BTreeMap::new();
    for (index, line) in lines.iter().enumerate().take(closing).skip(1) {
        let (number, line) = (index + 1, line.trim_end());
        let (key, value) = line
            .split_once(':')
            .ok_or_else(|| format!("frontmatter line {number} is not `key: value`: {line:?}"))?;
        if !matches!(key, "name" | "description" | "profiles" | "tools") {
            return Err(format!(
                "frontmatter line {number} has an unknown key {key:?}"
            ));
        }
        if values.insert(key, value.trim()).is_some() {
            return Err(format!("frontmatter line {number} repeats the key {key:?}"));
        }
    }
    let scalar = |key: &str| -> Result<String, String> {
        match values.get(key) {
            None => Err(format!("the frontmatter has no {key:?}")),
            Some(&"") => Err(format!("the frontmatter's {key:?} is empty")),
            Some(value) => Ok((*value).to_owned()),
        }
    };
    let list = |key: &str| -> Result<Vec<String>, String> {
        let value = values
            .get(key)
            .ok_or_else(|| format!("the frontmatter has no {key:?}"))?;
        let inner = value
            .strip_prefix('[')
            .and_then(|rest| rest.strip_suffix(']'))
            .ok_or_else(|| format!("the frontmatter's {key:?} must be a list `[a, b]`"))?;
        if inner.trim().is_empty() {
            return Ok(Vec::new());
        }
        let mut items: Vec<String> = Vec::new();
        for item in inner.split(',').map(str::trim) {
            let bare = !item.is_empty()
                && item
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
            if !bare {
                return Err(format!(
                    "the frontmatter's {key:?} has an invalid item {item:?}"
                ));
            }
            if items.iter().any(|seen| seen == item) {
                return Err(format!("the frontmatter's {key:?} repeats {item:?}"));
            }
            items.push(item.to_owned());
        }
        Ok(items)
    };
    let frontmatter = SkillFrontmatter {
        name: scalar("name")?,
        description: scalar("description")?,
        profiles: list("profiles")?,
        tools: list("tools")?,
    };
    if frontmatter.profiles.is_empty() {
        return Err("the frontmatter's \"profiles\" names no profile".to_owned());
    }
    let body = lines[closing + 1..].join("\n");
    Ok((frontmatter, body.trim_start_matches('\n').to_owned()))
}

/// The version of `versions` that `prefix` names: the version itself, or a unique prefix of
/// at least [`MIN_VERSION_PREFIX`] characters.
///
/// # Errors
/// A message when `prefix` is too short, names no version, or names more than one.
pub fn resolve_version<'v>(
    versions: impl IntoIterator<Item = &'v str>,
    prefix: &str,
) -> Result<&'v str, String> {
    let versions: Vec<&str> = versions.into_iter().collect();
    if let Some(exact) = versions.iter().find(|version| **version == prefix) {
        return Ok(exact);
    }
    if prefix.len() < MIN_VERSION_PREFIX {
        return Err(format!(
            "version {prefix:?} is shorter than {MIN_VERSION_PREFIX} characters"
        ));
    }
    let mut matches = versions
        .iter()
        .filter(|version| version.starts_with(prefix));
    match (matches.next(), matches.next()) {
        (Some(version), None) => Ok(version),
        (None, _) => Err(format!("no version starts with {prefix:?}")),
        (Some(_), Some(_)) => Err(format!("more than one version starts with {prefix:?}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used)]

    use super::*;

    const SKILL_MD: &str = "---\nname: plan-it\ndescription: Draft and hand in a plan.\nprofiles: [planner, scheduler]\ntools: [ground_entity, submit_plan]\n---\n\n# Plan it\n\n1. Ground the subject.\n";

    fn files(extra: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut files = BTreeMap::from([(SKILL_FILE.to_owned(), SKILL_MD.to_owned())]);
        for (path, content) in extra {
            files.insert((*path).to_owned(), (*content).to_owned());
        }
        files
    }

    fn with_frontmatter(frontmatter: &str) -> BTreeMap<String, String> {
        BTreeMap::from([(
            SKILL_FILE.to_owned(),
            format!("---\n{frontmatter}\n---\nbody\n"),
        )])
    }

    fn profiles() -> ProfileTools {
        let tools = |names: &[&str]| names.iter().map(|&n| n.to_owned()).collect();
        BTreeMap::from([
            (
                "planner".to_owned(),
                tools(&["ground_entity", "related_entities", "submit_plan"]),
            ),
            ("scheduler".to_owned(), tools(&["ground_entity"])),
        ])
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "ekg-skills-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_skill_parses_its_frontmatter_body_and_files() {
        let bundle = SkillBundle::parse("plan-it", files(&[("reference/verbs.md", "# Verbs\n")]))
            .expect("parse");
        assert_eq!(
            bundle.frontmatter(),
            &SkillFrontmatter {
                name: "plan-it".to_owned(),
                description: "Draft and hand in a plan.".to_owned(),
                profiles: vec!["planner".to_owned(), "scheduler".to_owned()],
                tools: vec!["ground_entity".to_owned(), "submit_plan".to_owned()],
            }
        );
        assert_eq!(bundle.body(), "# Plan it\n\n1. Ground the subject.\n");
        assert_eq!(bundle.bundled(), vec!["reference/verbs.md"]);
        assert_eq!(bundle.bundled_file("reference/verbs.md"), Some("# Verbs\n"));
        assert_eq!(bundle.bundled_file(SKILL_FILE), None);
        // Round trip: re-parsing its own files yields the same bundle.
        assert_eq!(
            SkillBundle::parse("plan-it", bundle.files().clone()).expect("again"),
            bundle
        );
    }

    #[test]
    fn a_skill_directory_round_trips_through_the_file_map() {
        let dir = scratch("roundtrip").join("plan-it");
        std::fs::create_dir_all(dir.join("reference")).expect("mkdir");
        std::fs::write(dir.join(SKILL_FILE), SKILL_MD).expect("write");
        std::fs::write(dir.join("reference/verbs.md"), "# Verbs\n").expect("write");
        std::fs::write(dir.join(".DS_Store"), [0_u8, 159, 146]).expect("write");
        let read = SkillBundle::read_dir(&dir).expect("read");
        assert_eq!(
            read,
            SkillBundle::parse("plan-it", files(&[("reference/verbs.md", "# Verbs\n")]))
                .expect("parse"),
            "the dot-file is skipped"
        );
        let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
    }

    #[test]
    fn an_unknown_or_missing_or_repeated_key_is_refused() {
        let base = "name: plan-it\ndescription: d\nprofiles: [planner]\ntools: []";
        for (frontmatter, expected) in [
            (format!("{base}\nauthor: me"), "unknown key \"author\""),
            (
                "name: plan-it\nprofiles: [planner]\ntools: []".to_owned(),
                "no \"description\"",
            ),
            (format!("{base}\ntools: []"), "repeats the key \"tools\""),
            (
                "name: plan-it\ndescription: d\nprofiles: planner\ntools: []".to_owned(),
                "must be a list",
            ),
            (
                "name: plan-it\ndescription: d\nprofiles: []\ntools: []".to_owned(),
                "names no profile",
            ),
            (format!("{base}\n# a comment"), "not `key: value`"),
        ] {
            let error =
                SkillBundle::parse("plan-it", with_frontmatter(&frontmatter)).expect_err(expected);
            assert_eq!(error.skill, "plan-it");
            assert!(
                error.to_string().contains(expected),
                "{error} != {expected}"
            );
        }
        let unclosed = BTreeMap::from([(SKILL_FILE.to_owned(), "---\nname: plan-it\n".to_owned())]);
        assert!(
            SkillBundle::parse("plan-it", unclosed)
                .expect_err("unclosed")
                .to_string()
                .contains("never closed")
        );
    }

    #[test]
    fn a_name_that_differs_from_the_directory_or_is_not_kebab_case_is_refused() {
        let error = SkillBundle::parse("other-name", files(&[])).expect_err("mismatch");
        assert!(
            error
                .to_string()
                .contains("differs from its directory name"),
            "{error}"
        );
        for bad in ["Plan-It", "plan_it", "plan--it", "-plan", ""] {
            assert!(SkillBundle::parse(bad, files(&[])).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_path_escaping_the_skill_is_refused() {
        for path in [
            "../escape.md",
            "reference/../../x.md",
            "/abs.md",
            "a//b.md",
            "./a.md",
            "a\\b.md",
        ] {
            let error = SkillBundle::parse("plan-it", files(&[(path, "x")])).expect_err(path);
            assert!(
                error.to_string().contains("not a relative POSIX path"),
                "{error}"
            );
        }
    }

    #[test]
    fn a_binary_file_is_refused() {
        let dir = scratch("binary").join("plan-it");
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(SKILL_FILE), SKILL_MD).expect("write");
        std::fs::write(
            dir.join("logo.png"),
            [0x89_u8, 0x50, 0x4e, 0x47, 0xff, 0xfe],
        )
        .expect("write");
        let error = SkillBundle::read_dir(&dir).expect_err("binary");
        assert_eq!(error.skill, "plan-it");
        assert!(
            error.to_string().contains("logo.png is not UTF-8 text"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(dir.parent().expect("parent"));
    }

    #[test]
    fn an_over_size_file_or_skill_is_refused() {
        let big = "x".repeat(MAX_FILE_BYTES + 1);
        let error = SkillBundle::parse("plan-it", files(&[("big.md", &big)])).expect_err("file");
        assert!(error.to_string().contains("file limit"), "{error}");
        let chunk = "x".repeat(MAX_FILE_BYTES);
        let many: Vec<(String, &str)> = (0..4)
            .map(|i| (format!("part{i}.md"), chunk.as_str()))
            .collect();
        let many: Vec<(&str, &str)> = many.iter().map(|(p, c)| (p.as_str(), *c)).collect();
        let error = SkillBundle::parse("plan-it", files(&many)).expect_err("skill");
        assert!(error.to_string().contains("byte limit"), "{error}");
    }

    #[test]
    fn the_digest_is_stable_across_file_order_and_changes_on_a_one_byte_edit() {
        let a = files(&[("a.md", "alpha"), ("b.md", "beta")]);
        let b: BTreeMap<String, String> =
            [("b.md", "beta"), (SKILL_FILE, SKILL_MD), ("a.md", "alpha")]
                .into_iter()
                .map(|(p, c)| (p.to_owned(), c.to_owned()))
                .collect();
        let first = SkillBundle::parse("plan-it", a.clone())
            .expect("a")
            .digest();
        // The documented encoding: each file as `path \0 len \0 bytes`, in path (byte) order.
        let mut framed = Vec::new();
        for (path, content) in [(SKILL_FILE, SKILL_MD), ("a.md", "alpha"), ("b.md", "beta")] {
            framed.extend_from_slice(format!("{path}\0{}\0{content}", content.len()).as_bytes());
        }
        let expected = Sha256::digest(&framed)
            .iter()
            .fold(String::new(), |mut hex, byte| {
                let _ = write!(hex, "{byte:02x}");
                hex
            });
        assert_eq!(first, expected);
        assert_eq!(SkillBundle::parse("plan-it", b).expect("b").digest(), first);
        let mut edited = a;
        edited.insert("b.md".to_owned(), "betA".to_owned());
        assert_ne!(
            SkillBundle::parse("plan-it", edited)
                .expect("edited")
                .digest(),
            first
        );
    }

    #[test]
    fn the_digest_frames_each_file_so_content_cannot_shift_between_files() {
        // Moving a byte from one file's end into the next file's name changes the digest.
        let one = BTreeMap::from([
            ("a".to_owned(), "xb".to_owned()),
            ("c".to_owned(), String::new()),
        ]);
        let two = BTreeMap::from([
            ("a".to_owned(), "x".to_owned()),
            ("bc".to_owned(), String::new()),
        ]);
        assert_ne!(digest(&one), digest(&two));
    }

    #[test]
    fn validation_names_every_unknown_profile_and_missing_tool() {
        let bundle = SkillBundle::parse("plan-it", files(&[])).expect("parse");
        assert_eq!(
            validate(&bundle, &profiles()),
            Err(SkillError {
                skill: "plan-it".to_owned(),
                defects: vec![
                    "profile \"scheduler\" does not offer tool \"submit_plan\"".to_owned()
                ],
            })
        );
        let mut only_planner = profiles();
        only_planner.remove("scheduler");
        let error = validate(&bundle, &only_planner).expect_err("unknown");
        assert_eq!(
            error.defects,
            vec!["unknown profile \"scheduler\"".to_owned()]
        );
        let ok = SkillBundle::parse(
            "plan-it",
            with_frontmatter("name: plan-it\ndescription: d\nprofiles: [planner]\ntools: [ground_entity, submit_plan]"),
        )
        .expect("parse");
        assert_eq!(validate(&ok, &profiles()), Ok(()));
    }

    #[test]
    fn every_skill_under_a_root_is_read_and_validated() {
        let root = scratch("root");
        for (name, tools) in [
            ("good-one", "[ground_entity]"),
            ("bad-one", "[write_order]"),
        ] {
            std::fs::create_dir_all(root.join(name)).expect("mkdir");
            std::fs::write(
                root.join(name).join(SKILL_FILE),
                format!("---\nname: {name}\ndescription: d\nprofiles: [planner]\ntools: {tools}\n---\nbody\n"),
            )
            .expect("write");
        }
        let errors = read_and_validate_all(&root, &profiles()).expect_err("one is bad");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].skill, "bad-one");
        assert!(
            errors[0].to_string().contains("\"write_order\""),
            "{}",
            errors[0]
        );
        std::fs::remove_dir_all(root.join("bad-one")).expect("rm");
        let bundles = read_and_validate_all(&root, &profiles()).expect("valid");
        assert_eq!(
            bundles.iter().map(SkillBundle::name).collect::<Vec<_>>(),
            vec!["good-one"]
        );
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn a_version_is_named_by_itself_or_a_unique_long_prefix() {
        let versions = ["abcdef0123", "abcdef9999", "0123456789"];
        assert_eq!(resolve_version(versions, "0123456789"), Ok("0123456789"));
        assert_eq!(resolve_version(versions, "01234567"), Ok("0123456789"));
        assert!(
            resolve_version(versions, "0123")
                .expect_err("short")
                .contains("shorter")
        );
        assert!(resolve_version(versions, "abcdef01x").is_err());
        assert!(resolve_version(versions, "abcdef").is_err());
        let both = ["abcdef0123", "abcdef0199"];
        assert!(
            resolve_version(both, "abcdef01")
                .expect_err("ambiguous")
                .contains("more than one")
        );
    }
}
