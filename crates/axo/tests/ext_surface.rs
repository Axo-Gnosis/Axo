use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, ImplItem, Item, ItemEnum, ItemImpl, ItemMod, ItemStruct, LitStr, Meta, Type,
    UseTree, Visibility,
};

const ROOTS: &str =
    "reads host-chosen roots; host.catalog and session.skills/contextFiles report the result";
const DERIVABLE: &str = "derivable from the value in TypeScript";
const FREE_LINE: u32 = line!() + 1;
const FREE: &[(&str, &str)] = &[
    ("valid_command_name", "host.validCommandName"),
    ("valid_section_name", "host.validSectionName"),
    ("valid_scheme_name", "host.validSchemeName"),
    ("valid_tool_name", "host.validToolName"),
    ("jobs_from_entries", "host.jobsFromEntries"),
    ("project", "host.project"),
    ("squash", "host.squash"),
    ("file_changes", "host.fileChanges"),
    ("git_root", "host.gitRoot"),
    ("project_root", "host.projectRoot"),
    ("home_agents_dir", "host.homeAgentsDir"),
    ("parse_command_args", "host.parseCommandArgs"),
    ("substitute_args", "host.substituteArgs"),
    ("load_agents", ROOTS),
    ("load_modes", ROOTS),
    ("load_keywords", ROOTS),
    ("load_skills", ROOTS),
    ("context_file_names", ROOTS),
    ("load_context_files", ROOTS),
    ("expand_skill_command", "served by session.renderSkill"),
    ("render_skill", "served by session.renderSkill"),
    (
        "find_on_path",
        "takes a host environment map; tools.runCommand resolves programs",
    ),
    ("run", "live process handles"),
    ("objects", "runtime metadata for hosts and generators"),
    (
        "lock_file",
        "a lock guard cannot cross; tools.writeFile holds it",
    ),
    ("file_details", DERIVABLE),
    ("file_records", DERIVABLE),
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

fn test_module(item: &ItemMod) -> bool {
    item.attrs.iter().any(|attr| {
        attr.path().is_ident("cfg") && attr.parse_args::<syn::Ident>().is_ok_and(|id| id == "test")
    })
}

impl<'ast> Visit<'ast> for File<'_> {
    fn visit_item_mod(&mut self, item: &'ast ItemMod) {
        if !test_module(item) {
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
            if matches!(attr.meta, Meta::List(_)) {
                attr.parse_nested_meta(|meta| {
                    let value: syn::Expr = meta.value()?.parse()?;
                    if meta.path.is_ident("block") {
                        let syn::Expr::Path(block) = value else {
                            return Err(meta.error("expected block name"));
                        };
                        let name = block.path.require_ident()?.to_string();
                        if !axo::ext::objects()
                            .iter()
                            .flat_map(|o| &o.blocks)
                            .any(|b| b.name == name)
                        {
                            self.surface.problems.push(format!(
                                "{}: {name} missing from ext::objects()",
                                self.at(block)
                            ));
                        }
                    }
                    Ok(())
                })
                .unwrap();
            }
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

fn scan(dir: &Path, files: &mut BTreeMap<PathBuf, syn::File>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            scan(&path, files);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            let file = syn::parse_file(&fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            files.insert(path, file);
        }
    }
}

#[derive(Default)]
struct FreeFunctions {
    // Re-exports are matched by the original Rust function name, even for `as` aliases.
    exports: BTreeSet<String>,
    functions: Vec<(String, String, bool)>,
}

impl FreeFunctions {
    fn export(&mut self, tree: &UseTree, path: &Path) {
        match tree {
            UseTree::Path(p) => self.export(&p.tree, path),
            UseTree::Group(g) => g.items.iter().for_each(|t| self.export(t, path)),
            UseTree::Name(n) => {
                self.exports.insert(n.ident.to_string());
            }
            UseTree::Rename(r) => {
                self.exports.insert(r.ident.to_string());
            }
            UseTree::Glob(g) => panic!(
                "{}:{}: public glob re-export unsupported; name the exposed items",
                path.display(),
                g.span().start().line
            ),
        }
    }

    fn collect(
        &mut self,
        files: &BTreeMap<PathBuf, syn::File>,
        path: &Path,
        dir: &Path,
        items: &[Item],
        public: bool,
    ) {
        for item in items {
            match item {
                Item::Fn(f) if matches!(f.vis, Visibility::Public(_)) => {
                    let name = &f.sig.ident;
                    let at = format!("{}:{}", path.display(), name.span().start().line);
                    self.functions.push((name.to_string(), at, public));
                }
                Item::Use(u) if public && matches!(u.vis, Visibility::Public(_)) => {
                    self.export(&u.tree, path);
                }
                Item::Mod(m) if !test_module(m) => {
                    let public = public && matches!(m.vis, Visibility::Public(_));
                    let child = dir.join(m.ident.to_string());
                    if let Some((_, items)) = &m.content {
                        self.collect(files, path, &child, items, public);
                    } else {
                        let flat = child.with_extension("rs");
                        let nested = child.join("mod.rs");
                        let source = if files.contains_key(&flat) {
                            flat
                        } else {
                            nested
                        };
                        let file = files.get(&source).unwrap_or_else(|| {
                            panic!(
                                "{}:{}: cannot find module {}",
                                path.display(),
                                m.ident.span().start().line,
                                m.ident
                            )
                        });
                        self.collect(files, &source, &child, &file.items, public);
                    }
                }
                _ => {}
            }
        }
    }
}

#[test]
fn public_surface_has_an_exposure_decision() {
    let mut surface = Surface::default();
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = BTreeMap::new();
    scan(&src, &mut files);
    for (path, file) in &files {
        File {
            path,
            surface: &mut surface,
        }
        .visit_file(file);
    }
    for (name, locations) in surface.unmarked {
        if !surface.public.contains(&name) {
            continue;
        }
        for location in locations {
            surface.problems.push(format!(
                "{location}: public impl of {name} missing #[expose]"
            ));
        }
    }
    let mut free = FreeFunctions::default();
    let root = src.join("lib.rs");
    free.collect(&files, &root, &src, &files[&root].items, true);
    let host: BTreeSet<_> = axo::ext::objects()
        .iter()
        .filter(|o| o.name == "host")
        .flat_map(|o| &o.blocks)
        .flat_map(|b| &b.entries)
        .filter(|m| m.skip.is_none())
        .map(|m| m.name)
        .collect();
    let mut seen = BTreeMap::new();
    for (name, at, public) in free.functions {
        if !public && !free.exports.contains(&name) {
            continue;
        }
        match FREE.iter().find(|(n, _)| *n == name) {
            None => surface
                .problems
                .push(format!("{at}: pub fn {name} missing FREE decision")),
            Some((_, decision)) => {
                if let Some(wire) = decision.strip_prefix("host.")
                    && !host.contains(wire)
                {
                    surface.problems.push(format!(
                        "{at}: FREE decision for {name} names missing callable {decision}"
                    ));
                }
            }
        }
        if let Some(first) = seen.insert(name.clone(), at.clone()) {
            surface.problems.push(format!(
                "{at}: pub fn {name} shares its FREE decision with {first}; rename one"
            ));
        }
    }
    for &(name, _) in FREE {
        if !seen.contains_key(name) {
            surface.problems.push(format!(
                "{}:{FREE_LINE}: stale FREE decision for {name}",
                file!()
            ));
        }
    }
    surface.problems.sort();
    assert!(
        surface.problems.is_empty(),
        "{}",
        surface.problems.join("\n")
    );
}

#[test]
fn tool_name_syntax_does_not_reserve_names() {
    for name in ["-", "_", "Aa09_-", &"a".repeat(64), "call", "yield"] {
        assert!(axo::valid_tool_name(name), "{name:?}");
    }
    for name in ["", &"a".repeat(65), "café", "a/b"] {
        assert!(!axo::valid_tool_name(name), "{name:?}");
    }
}
