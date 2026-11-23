use std::fmt;

/// The origin label the bridge puts on everything an extension registers
/// (`SessionToolInfo::origin`, `CommandInfo::origin`, `Skill::origin`, `ModeSpec::origin`,
/// `Blocked::origin`): `extension <id>`, or `extension <id> (scope <key>)` for an
/// extension enabled in an [`ExtensionScope`](crate::ExtensionScope).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ExtensionOrigin<'a> {
    pub id: &'a str,
    /// `ScopeOptions::key` of the scope that enabled it; `None` for the root manager.
    pub scope: Option<&'a str>,
}

impl<'a> ExtensionOrigin<'a> {
    /// `None` for a label the bridge did not write. Unambiguous because extension IDs and
    /// scope keys contain no whitespace.
    pub fn parse(origin: &'a str) -> Option<Self> {
        let rest = origin.strip_prefix("extension ")?;
        let (id, scope) = match rest.split_once(" (scope ") {
            None if !rest.contains(' ') => (rest, None),
            None => return None,
            Some((id, tail)) => (id, Some(tail.strip_suffix(')')?)),
        };
        let clean = |s: &str| !s.is_empty() && !s.contains(' ');
        (clean(id) && scope.is_none_or(clean)).then_some(Self { id, scope })
    }
}

impl fmt::Display for ExtensionOrigin<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.scope {
            Some(key) => write!(f, "extension {} (scope {key})", self.id),
            None => write!(f, "extension {}", self.id),
        }
    }
}
