use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, ImplItem, ItemEnum, ItemImpl, ItemMod, ItemStruct, LitStr, Meta, Type, Visibility,
};

// Remaining public surfaces to be classified in plan B.
const PENDING: &[&str] = &[
    "AgentDef",
    "AgentSpec",
    "Artifacts",
    "Axo",
    "AxoBuilder",
    "BashAccess",
    "BashTool",
    "ChildHandle",
    "Command",
    "CommandOutput",
    "Container",
    "ContextFile",
    "DevShellEnv",
    "DirScheme",
    "EditTool",
    "EffectiveConfig",
    "EnabledExtension",
    "Entry",
    "EntryBase",
    "EntryId",
    "Error",
    "FileLedger",
    "FindTool",
    "GrepTool",
    "Header",
    "HostTool",
    "InvokeError",
    "JobStatus",
    "JsonlStore",
    "KeywordSpec",
    "Limits",
    "ModeSpec",
    "NestedContext",
    "Process",
    "ProcessSupervisor",
    "ProjectSettings",
    "Projection",
    "ReadTool",
    "RunAs",
    "Schemes",
    "SessionEvent",
    "SessionTools",
    "Settings",
    "SkillScheme",
    "SnapshotStore",
    "StaticEnv",
    "TeamHandle",
    "TeamMember",
    "ToolLimits",
    "Truncation",
    "WriteTool",
];

#[derive(Default)]
struct Surface {
    public: BTreeSet<String>,
    unmarked: BTreeMap<String, Vec<String>>,
    problems: Vec<String>,
}

struct File<'a> {
    path: &'a Path,
    surface: &'a mut Surface,
}

impl File<'_> {
    fn at(&self, span: impl Spanned) -> String {
        format!("{}:{}", self.path.display(), span.span().start().line)
    }
}

impl<'ast> Visit<'ast> for File<'_> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if !item.attrs.iter().any(|attr| {
            attr.path().is_ident("cfg")
                && attr.parse_args::<syn::Ident>().is_ok_and(|id| id == "test")
        }) {
            visit::visit_item_mod(self, item);
        }
    }

    fn visit_item_struct(&mut self, item: &'ast ItemStruct) {
        if matches!(item.vis, Visibility::Public(_)) {
            self.surface.public.insert(item.ident.to_string());
        }
    }

    fn visit_item_enum(&mut self, item: &'ast ItemEnum) {
        if matches!(item.vis, Visibility::Public(_)) {
            self.surface.public.insert(item.ident.to_string());
        }
    }

    fn visit_attribute(&mut self, attr: &'ast Attribute) {
        if attr.path().is_ident("ext") && matches!(attr.meta, Meta::List(_)) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("skip") {
                    let reason: LitStr = meta.value()?.parse()?;
                    if reason.value().starts_with("pending") {
                        self.surface.problems.push(format!(
                            "{}: unresolved skip: {}",
                            self.at(reason.span()),
                            reason.value()
                        ));
                    }
                }
                Ok(())
            })
            .unwrap();
        }
    }

    fn visit_item_impl(&mut self, item: &'ast ItemImpl) {
        visit::visit_item_impl(self, item);
        let exposed = item.attrs.iter().find(|attr| {
            attr.path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "expose")
        });
        if let Some(attr) = exposed {
            attr.parse_nested_meta(|meta| {
                let value: syn::Expr = meta.value()?.parse()?;
                if meta.path.is_ident("block") {
                    let syn::Expr::Path(block) = value else {
                        return Err(meta.error("expected block name"));
                    };
                    let name = block.path.require_ident()?.to_string();
                    if !axo::ext::session_blocks().iter().any(|b| b.name == name) {
                        self.surface.problems.push(format!(
                            "{}: {name} missing from session_blocks()",
                            self.at(block)
                        ));
                    }
                }
                Ok(())
            })
            .unwrap();
            return;
        }
        if item.trait_.is_some() || !item.items.iter().any(|method| matches!(method, ImplItem::Fn(f) if matches!(f.vis, Visibility::Public(_)))) {
            return;
        }
        if let Type::Path(path) = &*item.self_ty
            && let Some(segment) = path.path.segments.last()
        {
            let at = self.at(segment);
            self.surface
                .unmarked
                .entry(segment.ident.to_string())
                .or_default()
                .push(at);
        }
    }
}

fn scan(dir: &Path, surface: &mut Surface) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, surface);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let file = syn::parse_file(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            File {
                path: &path,
                surface,
            }
            .visit_file(&file);
        }
    }
}

#[test]
fn public_surface_has_an_exposure_decision() {
    let mut surface = Surface::default();
    scan(
        &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
        &mut surface,
    );
    let mut stale: BTreeSet<_> = PENDING.iter().copied().collect();
    for (name, locations) in surface.unmarked {
        if name != "Session" && (!surface.public.contains(&name) || stale.remove(name.as_str())) {
            continue;
        }
        for location in locations {
            surface.problems.push(format!(
                "{location}: public impl of {name} missing #[expose]"
            ));
        }
    }
    for name in stale {
        surface
            .problems
            .push(format!("{}: stale PENDING name: {name}", file!()));
    }
    surface.problems.sort();
    assert!(
        surface.problems.is_empty(),
        "{}",
        surface.problems.join("\n")
    );
}
