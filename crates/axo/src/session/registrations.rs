use std::sync::{Arc, MutexGuard, PoisonError};

use axo_agent::Agent;
use axo_macros::expose;

use super::{Session, SessionHooks, SpawnHook, ToolPolicy, detached};
use crate::commands::{self, RegisteredCommand};
use crate::context::Diagnostic;
use crate::context::definitions::Definitions;
use crate::error::Error;
use crate::schemes::Scheme;
use crate::tools::HostTool;

/// One set of live registrations, for a host or a composition layer.
#[derive(Clone, Default)]
pub struct Registrations {
    /// Agents, modes, keywords, and skills supplied in code; the first name wins.
    pub definitions: Definitions,
    /// Tools built per session (and per fork) from its `SessionTools`; a tool named like a built-in replaces it.
    pub tools: Vec<HostTool>,
    /// Ordered tool policies, inherited by forks and children.
    pub tool_policies: Vec<Arc<dyn ToolPolicy>>,
    /// Ordered spawn policies, inherited by children.
    pub spawn_hooks: Vec<Arc<dyn SpawnHook>>,
    /// A scheme named like a built-in replaces it; artifact, file, and tool are reserved.
    pub schemes: Vec<Arc<dyn Scheme>>,
    /// Prompt sections, applied in order; `None` removes a section.
    pub sections: Vec<(String, Option<String>)>,
    /// Commands listed after modes; forks inherit them.
    pub commands: Vec<RegisteredCommand>,
}

/// The host's own registrations and the layer applied after them.
#[derive(Clone, Default)]
pub(crate) struct Layers {
    pub host: Registrations,
    pub layer: Registrations,
}

impl Layers {
    /// The host's entries of one list followed by the layer's.
    pub(crate) fn joined<T: Clone>(&self, of: impl Fn(&Registrations) -> &Vec<T>) -> Vec<T> {
        of(&self.host)
            .iter()
            .chain(of(&self.layer))
            .cloned()
            .collect()
    }
}

pub(super) fn check_section_name(name: &str) -> Result<(), String> {
    if crate::context::prompt::valid_section_name(name) {
        Ok(())
    } else {
        Err(format!("Invalid system prompt section name: {name}"))
    }
}

fn check_sections(sections: &[(String, Option<String>)]) -> Result<(), Error> {
    sections
        .iter()
        .try_for_each(|(name, _)| check_section_name(name))
        .map_err(Error::InvalidInput)
}

impl SessionHooks {
    pub(super) fn registrations(&self) -> MutexGuard<'_, Layers> {
        self.registrations
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts the joined policies, spawn hooks and commands of `layers` in their cells;
    /// returns the commands' diagnostics. Callers hold the `registrations` lock, so these
    /// writes land in the order the layers were stored.
    fn apply_registrations(&self, agent: &Agent, layers: &Layers) -> Vec<Diagnostic> {
        self.install_policies(agent, layers.joined(|r| &r.tool_policies));
        *self
            .spawn_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = layers.joined(|r| &r.spawn_hooks);
        self.install_commands(layers)
    }

    fn install_commands(&self, layers: &Layers) -> Vec<Diagnostic> {
        let commands = layers.joined(|r| &r.commands);
        let defs = self.definitions.get();
        let diagnostics = commands::registered_diagnostics(&commands, &defs.modes, &defs.skills);
        *self.commands.lock().unwrap_or_else(PoisonError::into_inner) = commands;
        diagnostics
    }
}

#[expose(block = SESSION_REGISTRATIONS, object = "session")]
impl Session {
    /// Replaces the host's registered commands; returns the diagnostics of every registered
    /// command, layer included (invalid, hidden or replaced names, hidden skills). Hosts pass
    /// commands again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_commands(&self, commands: Vec<RegisteredCommand>) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.commands = commands;
        Ok(self.hooks.install_commands(&layers))
    }

    /// Changes a host section from the next request. Hosts pass sections again on reopen.
    /// This is cache-heavy on models without mid-conversation system messages.
    #[ext(skip = "edits the host's own registrations; extensions register through their manifest")]
    pub async fn set_section(&self, name: &str, body: Option<String>) -> Result<(), Error> {
        self.reject_child()?;
        check_section_name(name).map_err(Error::InvalidInput)?;
        let _order = self.hooks.selection_order.lock().await;
        let sections = {
            let mut layers = self.hooks.registrations();
            let sections = &mut layers.host.sections;
            if let Some((_, value)) = sections.iter_mut().rfind(|(key, _)| key == name) {
                *value = body;
            } else {
                sections.push((name.to_owned(), body));
            }
            layers.joined(|r| &r.sections)
        };
        self.hooks.stage_sections(sections);
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Replaces every host section from the next request; an invalid name rejects the
    /// whole set. Hosts pass sections again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_sections(&self, sections: Vec<(String, Option<String>)>) -> Result<(), Error> {
        self.reject_child()?;
        check_sections(&sections)?;
        let _order = self.hooks.selection_order.lock().await;
        let sections = {
            let mut layers = self.hooks.registrations();
            layers.host.sections = sections;
            layers.joined(|r| &r.sections)
        };
        self.hooks.stage_sections(sections);
        self.hooks.prompt.rebuild();
        Ok(())
    }

    /// Replaces host tool policies for hook invocations that start after this returns.
    /// Live children keep theirs.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_tool_policies(&self, policies: Vec<Arc<dyn ToolPolicy>>) -> Result<(), Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.tool_policies = policies;
        self.hooks
            .install_policies(&self.agent, layers.joined(|r| &r.tool_policies));
        Ok(())
    }

    /// Replaces host spawn hooks for spawns resolved after this returns.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub fn set_spawn_hooks(&self, hooks: Vec<Arc<dyn SpawnHook>>) -> Result<(), Error> {
        self.reject_child()?;
        let mut layers = self.hooks.registrations();
        layers.host.spawn_hooks = hooks;
        *self
            .hooks
            .spawn_hooks
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = layers.joined(|r| &r.spawn_hooks);
        Ok(())
    }

    /// Replaces host schemes. URLs resolve through the new set at once; the prompt's
    /// schemes section changes from the next request. An invalid or reserved name rejects
    /// the whole set and keeps the previous one.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_schemes(&self, schemes: Vec<Arc<dyn Scheme>>) -> Result<(), Error> {
        self.reject_child()?;
        let hooks = &self.hooks;
        let _order = hooks.selection_order.lock().await;
        let joined = schemes
            .iter()
            .chain(&hooks.registrations().layer.schemes)
            .cloned()
            .collect();
        hooks.stage_schemes(joined)?;
        hooks.registrations().host.schemes = schemes;
        hooks.prompt.rebuild();
        Ok(())
    }

    pub(super) fn reject_child(&self) -> Result<(), Error> {
        if self.hooks.child.is_some() {
            return Err(Error::InvalidInput(
                "This operation is only available on root sessions".into(),
            ));
        }
        Ok(())
    }

    /// Replaces host tool factories from the next request, still honoring the persona's
    /// allowlist; returns the new tool shape's diagnostics. Hosts pass tools again on
    /// reopen. Changes are cache-heavy on models without mid-conversation tool changes.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_tools(&self, tools: Vec<HostTool>) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let hooks = self.hooks.clone();
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            let tools = {
                let mut layers = hooks.registrations();
                layers.host.tools = tools;
                layers.joined(|r| &r.tools)
            };
            let diagnostics = hooks.stage_tools(tools);
            hooks.prompt.rebuild();
            Ok(diagnostics)
        })
        .await
    }

    /// Replaces the host's code-supplied definitions from the next request. File definitions
    /// and the layer's keep their precedence. The selected persona and mode keep the copy they
    /// were selected with. Returns merge, skill and command diagnostics, then the tool shape's.
    /// Hosts pass definitions again on reopen.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_definitions(
        &self,
        definitions: Definitions,
    ) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        let hooks = self.hooks.clone();
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            hooks.registrations().host.definitions = definitions;
            let (changed, mut diagnostics) = hooks.stage_definitions();
            let defs = hooks.definitions.get();
            diagnostics.extend(commands::registered_diagnostics(
                &hooks.commands(),
                &defs.modes,
                &defs.skills,
            ));
            if changed {
                diagnostics.extend(hooks.restage());
                hooks.prompt.rebuild();
            } else {
                diagnostics.extend(
                    hooks
                        .prompt
                        .shape
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .tool_diagnostics
                        .iter()
                        .cloned(),
                );
            }
            Ok(diagnostics)
        })
        .await
    }

    /// Replaces the layer over the host's registrations, and the layer's definitions, in one
    /// pass. An invalid section name or an invalid or reserved scheme name rejects the whole
    /// layer and changes nothing. Live children keep what they were spawned with. Returns the
    /// new tool shape's diagnostics, then definition diagnostics, then those of every
    /// registered command.
    #[ext(
        skip = "replaces the host's own registrations; extensions register through their manifest"
    )]
    pub async fn set_layer(&self, layer: Registrations) -> Result<Vec<Diagnostic>, Error> {
        self.reject_child()?;
        check_sections(&layer.sections)?;
        let (hooks, agent) = (self.hooks.clone(), self.agent.clone());
        detached(async move {
            let _order = hooks.selection_order.lock().await;
            // The only fallible stage goes first, so an error changes nothing.
            let schemes = hooks
                .registrations()
                .host
                .schemes
                .iter()
                .chain(&layer.schemes)
                .cloned()
                .collect();
            hooks.stage_schemes(schemes)?;
            hooks.registrations().layer = layer;
            let (_, definition_diagnostics) = hooks.stage_definitions();
            let (sections, tools, command_diagnostics) = {
                let layers = hooks.registrations();
                let command_diagnostics = hooks.apply_registrations(&agent, &layers);
                (
                    layers.joined(|r| &r.sections),
                    layers.joined(|r| &r.tools),
                    command_diagnostics,
                )
            };
            hooks.stage_sections(sections);
            let mut diagnostics = hooks.stage_tools(tools);
            hooks.prompt.rebuild();
            diagnostics.extend(definition_diagnostics);
            diagnostics.extend(command_diagnostics);
            Ok::<_, Error>(diagnostics)
        })
        .await
    }
}
