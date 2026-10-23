//! What shapes the model's context: the system prompt, context files and skills.
//!
//! Loaders never fail: problems become [`Diagnostic`]s that the session logs.

pub mod files;
pub mod prompt;
pub mod skills;

use std::fmt;
use std::path::{Path, PathBuf};

use axo_agent::AgentMessage;
use axo_ai::{Message, UserMessage};

pub use skills::{parse_command_args, substitute_args};

/// A non-fatal problem found while loading context resources.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    /// The file the problem is about, if any.
    pub path: Option<PathBuf>,
    pub message: String,
}

impl fmt::Display for Diagnostic {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.path {
            Some(path) => write!(f, "{}: {}", path.display(), self.message),
            None => f.write_str(&self.message),
        }
    }
}

/// The nearest ancestor of `cwd` (inclusive) holding a `.git` entry (a directory, or a
/// file in worktrees and submodules). Context-file, skill and flake walks stop there.
pub fn git_root(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors()
        .find(|dir| dir.join(".git").exists())
        .map(Path::to_path_buf)
}

/// The git root, or `cwd` itself outside a repository.
pub fn project_root(cwd: &Path) -> PathBuf {
    git_root(cwd).unwrap_or_else(|| cwd.to_path_buf())
}

pub(crate) fn project_dirs<'a>(root: &Path, dir: &'a Path) -> Vec<&'a Path> {
    let mut dirs: Vec<_> = dir
        .ancestors()
        .take_while(|dir| dir.starts_with(root))
        .collect();
    dirs.reverse();
    dirs
}

/// `$HOME/.agents`, the folder other agent tools share; read only with `homeAgentsDir`.
pub fn home_agents_dir() -> Option<PathBuf> {
    crate::config::home_dir().map(|home| home.join(".agents"))
}

/// The shared lowering used by ordinary requests and summary requests.
pub(crate) fn lower_messages(messages: Vec<AgentMessage>) -> Vec<Message> {
    messages
        .into_iter()
        .map(|message| match message {
            AgentMessage::Llm(message) => message,
            AgentMessage::Custom(custom) => Message::User(UserMessage {
                content: custom.content,
                timestamp: custom.timestamp,
            }),
        })
        .collect()
}
