//! Read-only URLs for discovered skills and their supporting files.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use axo_agent::ToolError;

use super::{Scheme, SchemeContext, SchemeUrl, Target, join_contained};
use crate::context::definitions::LiveDefinitions;

/// Resolves every loaded skill, including skills hidden from model invocation.
pub struct SkillScheme {
    definitions: Arc<LiveDefinitions>,
}

impl SkillScheme {
    pub(crate) fn new(definitions: Arc<LiveDefinitions>) -> Self {
        Self { definitions }
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
        let defs = self.definitions.get();
        let skill = defs
            .skills
            .iter()
            .filter(|skill| {
                let name = &skill.name;
                url.path == *name
                    || url
                        .path
                        .strip_prefix(name.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            })
            .max_by_key(|skill| skill.name.len())
            .ok_or_else(|| {
                let name = url
                    .path
                    .split_once('/')
                    .map_or(url.path.as_str(), |(name, _)| name);
                ToolError(format!(
                    "Unknown skill: {name}. Available: {}",
                    super::available(defs.skills.iter().map(|skill| skill.name.clone()).collect())
                ))
            })?;
        let path = if url.path == skill.name {
            skill.path.clone()
        } else {
            let rest = url.path[skill.name.len()..].trim_start_matches('/');
            let root = skill.path.parent().unwrap_or_else(|| Path::new("."));
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
    use crate::context::definitions::ResolvedDefinitions;
    use crate::context::skills::Skill;
    use std::path::PathBuf;

    #[tokio::test]
    async fn nested_skill_names_resolve_to_the_longest_match() {
        let skills = [
            ("team", "/s/team/SKILL.md"),
            ("team/deploy", "/s/deploy/SKILL.md"),
        ]
        .into_iter()
        .map(|(name, path)| Skill {
            name: name.into(),
            description: String::new(),
            path: path.into(),
            disable_model_invocation: false,
            argument_hint: None,
        })
        .collect();
        let scheme = SkillScheme::new(Arc::new(LiveDefinitions::new(ResolvedDefinitions {
            skills,
            ..Default::default()
        })));
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
