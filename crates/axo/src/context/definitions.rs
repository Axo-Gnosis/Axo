use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use super::agents::{AgentDef, AgentSpec, TeamSpec, load_agents, merge_agents};
use super::keywords::{KeywordSpec, load_keywords, merge_keywords};
use super::modes::{ModeSpec, load_modes, merge_modes};
use super::skills::{Skill, load_skills, merge_skills};
use super::{Diagnostic, home_agents_dir};
use crate::config::{EffectiveConfig, ProjectAccess};

/// Definitions supplied in code rather than read from definition files.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Definitions {
    pub agents: Vec<AgentSpec>,
    pub teams: Vec<TeamSpec>,
    pub modes: Vec<ModeSpec>,
    pub keywords: Vec<KeywordSpec>,
    pub skills: Vec<Skill>,
}

/// What a session read from definition files at start; never re-read.
#[derive(Debug, Clone, Default)]
pub(crate) struct FileDefinitions {
    pub agents: Vec<AgentDef>,
    pub modes: Vec<ModeSpec>,
    pub keywords: Vec<KeywordSpec>,
    pub skills: Vec<Skill>,
}

/// File definitions merged with code-supplied ones: the definitions in force.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct ResolvedDefinitions {
    pub agents: Vec<AgentDef>,
    pub modes: Vec<Arc<ModeSpec>>,
    pub keywords: Vec<Arc<KeywordSpec>>,
    pub skills: Vec<Skill>,
}

/// The definitions in force for one session, shared with its `skill://` scheme.
#[derive(Default)]
pub(crate) struct LiveDefinitions(Mutex<Arc<ResolvedDefinitions>>);

impl LiveDefinitions {
    pub(crate) fn new(resolved: ResolvedDefinitions) -> Self {
        Self(Mutex::new(Arc::new(resolved)))
    }

    pub(crate) fn get(&self) -> Arc<ResolvedDefinitions> {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    pub(crate) fn set(&self, resolved: ResolvedDefinitions) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Arc::new(resolved);
    }
}

/// Blocking file I/O.
pub(crate) fn load_files(
    config: &EffectiveConfig,
    cwd: &Path,
) -> (FileDefinitions, Vec<Diagnostic>) {
    let (agents, mut diagnostics) = load_agents(&config.agent_roots);
    let (modes, mode_diagnostics) = load_modes(&config.mode_roots);
    let (keywords, keyword_diagnostics) = load_keywords(&config.keyword_roots);
    let (skills, skill_diagnostics) = skill_files(config, cwd);
    diagnostics.extend(mode_diagnostics);
    diagnostics.extend(keyword_diagnostics);
    diagnostics.extend(skill_diagnostics);
    (
        FileDefinitions {
            agents,
            modes,
            keywords,
            skills,
        },
        diagnostics,
    )
}

/// Blocking file I/O.
pub(crate) fn skill_files(config: &EffectiveConfig, cwd: &Path) -> (Vec<Skill>, Vec<Diagnostic>) {
    let settings = &config.settings;
    let home_agents = settings.home_agents_dir.then(home_agents_dir).flatten();
    load_skills(
        &config.skill_roots,
        home_agents.as_deref(),
        cwd,
        config.access != ProjectAccess::Ignore && settings.project.files.skills,
    )
}

/// `code` in precedence order (first wins). No I/O.
pub(crate) fn merge(
    files: &FileDefinitions,
    code: &[&Definitions],
) -> (ResolvedDefinitions, Vec<Diagnostic>) {
    let mut agents = Vec::new();
    for source in code {
        agents.extend(source.agents.iter().cloned().map(AgentDef::Agent));
        agents.extend(source.teams.iter().cloned().map(AgentDef::Team));
    }
    let (agents, mut diagnostics) = merge_agents(&files.agents, &agents);
    let (modes, mode_diagnostics) = merge_modes(&files.modes, &joined(code, |d| &d.modes));
    let (keywords, keyword_diagnostics) =
        merge_keywords(&files.keywords, &joined(code, |d| &d.keywords));
    let (skills, skill_diagnostics) = merge_skills(&files.skills, &joined(code, |d| &d.skills));
    diagnostics.extend(mode_diagnostics);
    diagnostics.extend(keyword_diagnostics);
    diagnostics.extend(skill_diagnostics);
    (
        ResolvedDefinitions {
            agents,
            modes: modes.into_iter().map(Arc::new).collect(),
            keywords: keywords.into_iter().map(Arc::new).collect(),
            skills,
        },
        diagnostics,
    )
}

// Borrow the list when only one source contributes definitions of this kind.
fn joined<'a, T: Clone>(
    code: &[&'a Definitions],
    list: impl Fn(&'a Definitions) -> &'a [T],
) -> Cow<'a, [T]> {
    let mut lists = code.iter().copied().map(list).filter(|s| !s.is_empty());
    let mut joined = Cow::Borrowed(lists.next().unwrap_or_default());
    for items in lists {
        joined.to_mut().extend_from_slice(items);
    }
    joined
}
