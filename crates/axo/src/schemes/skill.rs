//! Read-only URLs for discovered skills and their supporting files.

use std::path::{Path, PathBuf};

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target, join_contained};
use crate::context::skills::Skill;

/// Resolves every loaded skill, including skills hidden from model invocation.
pub struct SkillScheme {
    skills: Vec<(String, PathBuf)>,
}

impl SkillScheme {
    /// Keeps each skill's name and `SKILL.md` location in discovery order.
    pub fn new(skills: &[Skill]) -> Self {
        Self {
            skills: skills
                .iter()
                .map(|skill| (skill.name.clone(), skill.path.clone()))
                .collect(),
        }
    }
}

#[async_trait]
impl Scheme for SkillScheme {
    fn name(&self) -> &str {
        "skill"
    }

    fn prompt_doc(&self) -> &str {
        "`skill://<name>`: a skill's instructions; `skill://<name>/` is its directory (list it with `find`), `skill://<name>/<path>` its other files; read-only."
    }

    async fn resolve(&self, url: &SchemeUrl, _cx: &SchemeContext) -> Result<Target, ToolError> {
        let (name, skill_md) = self
            .skills
            .iter()
            .filter(|(name, _)| {
                url.path == *name
                    || url
                        .path
                        .strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|(name, _)| name.len())
            .ok_or_else(|| {
                let name = url
                    .path
                    .split_once('/')
                    .map_or(url.path.as_str(), |(name, _)| name);
                ToolError(format!(
                    "Unknown skill: {name}. Available: {}",
                    super::available(self.skills.iter().map(|(name, _)| name.clone()).collect())
                ))
            })?;
        let path = if url.path.len() == name.len() {
            skill_md.clone()
        } else {
            let rest = url.path[name.len()..].trim_start_matches('/');
            let root = skill_md.parent().unwrap_or_else(|| Path::new("."));
            join_contained(root, url, rest, false)?
        };
        Ok(Target::File {
            path,
            writable: false,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn nested_skill_names_resolve_to_the_longest_match() {
        let scheme = SkillScheme {
            skills: vec![
                ("team".into(), PathBuf::from("/s/team/SKILL.md")),
                ("team/deploy".into(), PathBuf::from("/s/deploy/SKILL.md")),
            ],
        };
        let context = SchemeContext {
            session_id: "t".into(),
            session_dir: "/x".into(),
            cwd: "/x".into(),
        };
        for (path, expected) in [
            ("team/deploy", Ok("/s/deploy/SKILL.md")),
            ("team/", Ok("/s/team")),
            ("team/deploy/", Ok("/s/deploy")),
            ("team/deploy/ref.md", Ok("/s/deploy/ref.md")),
            ("team/notes.md", Ok("/s/team/notes.md")),
            (
                "nope/x",
                Err("Unknown skill: nope. Available: team, team/deploy"),
            ),
        ] {
            let url = SchemeUrl {
                scheme: "skill".into(),
                path: path.into(),
            };
            let expected = expected
                .map(|path| Target::File {
                    path: PathBuf::from(path),
                    writable: false,
                })
                .map_err(str::to_owned);
            assert_eq!(
                scheme
                    .resolve(&url, &context)
                    .await
                    .map_err(|error| error.0),
                expected,
                "{url}"
            );
        }
    }
}
