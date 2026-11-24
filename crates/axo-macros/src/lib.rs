//! `#[expose(block = NAME, object = "session")]` binds an inherent impl to JSON.
//! Each public method must opt in with `#[ext]` or give a reason with `#[ext(skip = "...")]`.
//! The block describes both; callable entries decode positional arguments, call Rust, and
//! register their owned wire types. `target = T` constructs a caller-specific wrapper over `T`.
//! Bare `#[expose]` validates and strips markers on withheld-only impls without dispatch metadata.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as Tokens};
use quote::{format_ident, quote, quote_spanned};
use syn::ext::IdentExt;
use syn::spanned::Spanned;
use syn::visit::{self, Visit};
use syn::{
    Attribute, Error, Expr, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemImpl, Lit,
    LitStr, Meta, Pat, PathArguments, PathSegment, ReturnType, Type, TypeParamBound, Visibility,
    parse_macro_input, parse_quote_spanned,
};

const CANNOT_CROSS: &str = "this type cannot cross as JSON; withhold the method with #[ext(skip = \"reason\")] and add a wire method";

#[proc_macro_attribute]
pub fn expose(attr: TokenStream, item: TokenStream) -> TokenStream {
    let (mut block, mut object, mut target) = (None, None, None);
    let parser = syn::meta::parser(|meta| {
        if meta.path.is_ident("block") && block.is_none() {
            block = Some(meta.value()?.parse::<Ident>()?);
        } else if meta.path.is_ident("object") && object.is_none() {
            object = Some(meta.value()?.parse::<LitStr>()?);
        } else if meta.path.is_ident("target") && target.is_none() {
            target = Some(meta.value()?.parse::<Type>()?);
        } else {
            return Err(meta.error("expected `block`, `object` or `target`, each at most once"));
        }
        Ok(())
    });
    parse_macro_input!(attr with parser);
    let mut imp = parse_macro_input!(item as ItemImpl);
    if imp.trait_.is_some() {
        return Error::new_spanned(&imp.self_ty, "expose requires an inherent impl")
            .into_compile_error()
            .into();
    }
    let dispatch = match (block, object) {
        (Some(block), Some(object)) => {
            let self_ty = &imp.self_ty;
            let receiver = if target.is_some() {
                let Some(wrapper) = segment(self_ty).map(|s| &s.ident) else {
                    return Error::new_spanned(self_ty, "expected a named wire wrapper")
                        .into_compile_error()
                        .into();
                };
                quote!(let receiver = #wrapper::wire(target, _caller);)
            } else {
                quote!(let receiver = target;)
            };
            Some((
                block,
                object.value(),
                target.as_ref().unwrap_or(self_ty),
                receiver,
            ))
        }
        (None, None) if target.is_none() => None,
        _ => {
            return Error::new(
                Span::call_site(),
                "expected `block = NAME, object = \"name\"`",
            )
            .into_compile_error()
            .into();
        }
    };
    let mut defs = Tokens::new();
    let mut entries = Vec::new();
    let mut errors = Tokens::new();
    for item in &mut imp.items {
        let ImplItem::Fn(method) = item else { continue };
        let result = match take_marker(method) {
            Ok(Some(marker)) => match &dispatch {
                Some((_, object, target, receiver)) => {
                    entry(method, marker, object, target, receiver, &mut defs)
                }
                None => match marker {
                    Marker::Skip(_) => continue,
                    Marker::Call => Err(Error::new(
                        method.sig.ident.span(),
                        "withheld-only impl; add a wire method",
                    )),
                },
            },
            Ok(None) => continue,
            Err(e) => Err(e),
        };
        match result {
            Ok(entry) => entries.push(entry),
            Err(e) => errors.extend(e.into_compile_error()),
        }
    }
    if !errors.is_empty() {
        return quote!(#imp #errors).into();
    }
    let Some((block, object, target, _)) = dispatch else {
        return quote!(#imp).into();
    };
    quote! {
        #imp
        #defs
        #[doc(hidden)]
        #[allow(dead_code)]
        pub(crate) const #block: crate::ext::Block<#target> = crate::ext::Block {
            name: stringify!(#block), object: #object, entries: &[#(#entries),*],
        };
    }
    .into()
}

enum Marker {
    Call,
    Skip(LitStr),
}

fn take_marker(method: &mut ImplItemFn) -> syn::Result<Option<Marker>> {
    let mut markers = method.attrs.extract_if(.., |a| a.path().is_ident("ext"));
    let first = markers.next();
    let duplicates = markers.count();
    let ident = &method.sig.ident;
    if !matches!(method.vis, Visibility::Public(_)) {
        return if first.is_some() {
            Err(Error::new(ident.span(), "#[ext] requires a public method"))
        } else {
            Ok(None)
        };
    }
    if duplicates != 0 {
        return Err(Error::new(ident.span(), "duplicate #[ext] marker"));
    }
    let Some(attr) = first else {
        return Err(Error::new(
            ident.span(),
            format!("public method `{ident}` must be marked #[ext] or #[ext(skip = \"reason\")]"),
        ));
    };
    if matches!(attr.meta, Meta::Path(_)) {
        return Ok(Some(Marker::Call));
    }
    let mut reason = None;
    attr.parse_nested_meta(|meta| {
        if !meta.path.is_ident("skip") || reason.is_some() {
            return Err(meta.error("expected `skip = \"reason\"`"));
        }
        reason = Some(meta.value()?.parse()?);
        Ok(())
    })?;
    reason
        .map(|r| Some(Marker::Skip(r)))
        .ok_or_else(|| Error::new(attr.span(), "expected #[ext] or #[ext(skip = \"reason\")]"))
}

fn case(name: &str, mut upper: bool) -> String {
    let mut out = String::new();
    for c in name.trim_start_matches('_').chars() {
        if c == '_' {
            upper = true;
        } else if upper {
            out.extend(c.to_uppercase());
            upper = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn docs(attrs: &[Attribute]) -> String {
    let mut lines = Vec::new();
    for a in attrs {
        if let Meta::NameValue(m) = &a.meta
            && m.path.is_ident("doc")
            && let Expr::Lit(l) = &m.value
            && let Lit::Str(s) = &l.lit
        {
            let text = s.value();
            lines.extend(
                text.split('\n')
                    .map(|s| s.strip_prefix(' ').unwrap_or(s).to_owned()),
            );
        }
    }
    lines.join("\n")
}

fn segment(ty: &Type) -> Option<&PathSegment> {
    match ty {
        Type::Path(p) => p.path.segments.last(),
        Type::Paren(p) => segment(&p.elem),
        Type::Group(g) => segment(&g.elem),
        _ => None,
    }
}

fn first_type(seg: &PathSegment) -> Option<&Type> {
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    match args.args.first()? {
        GenericArgument::Type(t) => Some(t),
        _ => None,
    }
}

fn into_type(ty: &Type) -> Option<&Type> {
    let Type::ImplTrait(i) = ty else { return None };
    if i.bounds.len() != 1 {
        return None;
    }
    let TypeParamBound::Trait(t) = i.bounds.first()? else {
        return None;
    };
    let seg = t.path.segments.last()?;
    if seg.ident == "Into" {
        first_type(seg)
    } else {
        None
    }
}

fn validate(ty: &Type) -> syn::Result<()> {
    struct Check(Option<Error>);
    impl<'ast> Visit<'ast> for Check {
        fn visit_type(&mut self, ty: &'ast Type) {
            if matches!(ty, Type::TraitObject(_))
                || (matches!(ty, Type::ImplTrait(_)) && into_type(ty).is_none())
                || segment(ty).is_some_and(|s| s.ident == "Result")
            {
                self.0 = Some(Error::new_spanned(ty, CANNOT_CROSS));
            } else if self.0.is_none() {
                visit::visit_type(self, ty);
            }
        }
    }
    let mut check = Check(None);
    check.visit_type(ty);
    check.0.map_or(Ok(()), Err)
}

#[derive(Clone, Copy)]
enum Container {
    Option,
    Vec,
}

// Adapt from the inside out. Each intermediate is bound in the call future so even
// a slice of borrowed strings can borrow from the decoded arguments across an await.
enum Step {
    Borrow(Option<Ident>),
    Wrap(Tokens),
    Into(Tokens),
    Map(Container, Box<Step>),
}

impl Step {
    fn borrows(&self) -> bool {
        match self {
            Self::Borrow(_) => true,
            Self::Wrap(_) | Self::Into(_) => false,
            Self::Map(_, inner) => inner.borrows(),
        }
    }

    fn apply(&self, value: &Ident, borrowed: bool) -> Tokens {
        match self {
            Self::Borrow(Some(method)) => quote!(#value.#method()),
            Self::Borrow(None) if !borrowed => quote!(&#value),
            Self::Borrow(None) => quote!(#value),
            Self::Wrap(ptr) => quote!(#ptr::new(#value)),
            Self::Into(ty) => quote!(::std::convert::Into::<#ty>::into(#value)),
            Self::Map(Container::Option, inner) if matches!(**inner, Self::Borrow(None)) => {
                quote!(#value.as_ref())
            }
            Self::Map(Container::Option, inner) if matches!(**inner, Self::Borrow(Some(_))) => {
                quote!(#value.as_deref())
            }
            Self::Map(container, inner) => {
                let borrows = inner.borrows();
                let arg = format_ident!("item");
                let map = match &**inner {
                    Self::Borrow(None) => Tokens::new(),
                    Self::Wrap(ptr) => quote!(.map(#ptr::new)),
                    Self::Into(ty) => quote!(.map(::std::convert::Into::<#ty>::into)),
                    _ => {
                        let mapped = inner.apply(&arg, borrows);
                        quote!(.map(|#arg| #mapped))
                    }
                };
                match container {
                    Container::Option => {
                        let by_ref = borrows.then(|| quote!(.as_ref()));
                        quote!(#value #by_ref #map)
                    }
                    Container::Vec => {
                        let iter = if borrows {
                            quote!(iter)
                        } else {
                            quote!(into_iter)
                        };
                        quote!(#value.#iter() #map.collect::<::std::vec::Vec<_>>())
                    }
                }
            }
        }
    }
}

fn owned(ty: &Type, steps: &mut Vec<Step>) -> Type {
    let span = ty.span();
    if let Some(inner) = into_type(ty) {
        return owned(inner, steps);
    }
    match ty {
        Type::Paren(p) => return owned(&p.elem, steps),
        Type::Group(g) => return owned(&g.elem, steps),
        Type::Reference(r) => {
            let (ty, method) = match &*r.elem {
                Type::Slice(s) => {
                    let elem = owned_collection(&s.elem, Container::Vec, steps);
                    (
                        parse_quote_spanned!(span=> ::std::vec::Vec<#elem>),
                        Some(format_ident!("as_slice")),
                    )
                }
                t if segment(t).is_some_and(|s| s.ident == "str") => (
                    parse_quote_spanned!(span=> ::std::string::String),
                    Some(format_ident!("as_str")),
                ),
                t if segment(t).is_some_and(|s| s.ident == "Path") => (
                    parse_quote_spanned!(span=> ::std::path::PathBuf),
                    Some(format_ident!("as_path")),
                ),
                t => (owned(t, steps), None),
            };
            steps.push(Step::Borrow(method));
            return ty;
        }
        _ => {}
    }
    if let Some(seg) = segment(ty)
        && let Some(inner) = first_type(seg)
    {
        if seg.ident == "Option" {
            let inner = owned_collection(inner, Container::Option, steps);
            return parse_quote_spanned!(span=> ::std::option::Option<#inner>);
        }
        if seg.ident == "Vec" {
            let inner = owned_collection(inner, Container::Vec, steps);
            return parse_quote_spanned!(span=> ::std::vec::Vec<#inner>);
        }
        if seg.ident == "Box" || seg.ident == "Arc" {
            let inner = owned(inner, steps);
            let ptr = if seg.ident == "Box" {
                quote!(::std::boxed::Box)
            } else {
                quote!(::std::sync::Arc)
            };
            steps.push(Step::Wrap(ptr));
            return inner;
        }
    }
    ty.clone()
}

fn owned_collection(ty: &Type, c: Container, steps: &mut Vec<Step>) -> Type {
    let mut inner = Vec::new();
    let ty = owned(ty, &mut inner);
    steps.extend(inner.into_iter().map(|s| Step::Map(c, Box::new(s))));
    ty
}

// Borrowed and concrete returns serialize in place. Opaque `impl Into<T>` guarantees
// conversion, not Serialize; materialize only those values before encoding.
fn into_return(ty: &Type) -> Option<Step> {
    if let Some(inner) = into_type(ty) {
        return Some(Step::Into(quote!(#inner)));
    }
    let seg = segment(ty)?;
    let c = if seg.ident == "Option" {
        Container::Option
    } else if seg.ident == "Vec" {
        Container::Vec
    } else {
        return None;
    };
    Some(Step::Map(c, Box::new(into_return(first_type(seg)?)?)))
}

fn entry(
    method: &ImplItemFn,
    marker: Marker,
    object: &str,
    target: &Type,
    receiver: &Tokens,
    defs: &mut Tokens,
) -> syn::Result<Tokens> {
    let sig = &method.sig;
    let ident = &sig.ident;
    let rust_name = ident.unraw().to_string();
    let name = case(&rust_name, false);
    let doc = docs(&method.attrs);
    let callable = matches!(marker, Marker::Call);
    if callable {
        if !sig.generics.params.is_empty() {
            return Err(Error::new(
                ident.span(),
                "generic methods must use #[ext(skip = \"reason\")]",
            ));
        }
        if !sig
            .receiver()
            .is_some_and(|r| r.reference.is_some() && r.mutability.is_none())
        {
            return Err(Error::new(
                ident.span(),
                "a callable #[ext] method requires an &self receiver",
            ));
        }
    }
    let prefix = format!("{}{}", case(object, true), case(&rust_name, true));
    let mut fields = Vec::new();
    let mut params = Vec::new();
    let mut prepare = Tokens::new();
    let mut args = Vec::new();
    for input in &sig.inputs {
        let FnArg::Typed(input) = input else { continue };
        let ty = &input.ty;
        let field = match &*input.pat {
            Pat::Ident(p) => p.ident.clone(),
            _ if !callable => format_ident!("arg{}", params.len()),
            p => return Err(Error::new_spanned(p, "#[ext] parameters must have names")),
        };
        if callable {
            validate(ty)?;
        }
        let mut steps = Vec::new();
        let owned_ty = owned(ty, &mut steps);
        let optional = segment(&owned_ty).is_some_and(|s| s.ident == "Option");
        let wire = case(&field.unraw().to_string(), false);
        params.push(quote!(crate::ext::Param { name: #wire, optional: #optional }));
        if callable {
            let default = optional.then(|| quote!(#[serde(default)]));
            fields.push(quote_spanned!(ty.span()=> #default #field: #owned_ty));
            let arg = format_ident!("__ext_arg{}", args.len());
            prepare.extend(quote!(let #arg = decoded.#field;));
            for step in steps {
                let expr = step.apply(&arg, false);
                prepare.extend(quote!(let #arg = #expr;));
            }
            args.push(arg);
        }
    }
    let mut args_ty = None;
    let mut result_ty = None;
    let (call, skip) = match marker {
        Marker::Skip(reason) => (quote!(None), quote!(Some(#reason))),
        Marker::Call => {
            let decode = if fields.is_empty() {
                quote! {
                    if !args.is_empty() {
                        return Err(crate::ext::InvokeError::InvalidArguments("expected no arguments".into()));
                    }
                }
            } else {
                let id = format_ident!("{prefix}Args");
                defs.extend(quote! {
                    #[doc(hidden)]
                    #[allow(dead_code)]
                    #[derive(::serde::Deserialize, ::specta::Type)]
                    #[serde(rename_all = "camelCase")]
                    struct #id { #(#fields,)* }
                });
                let decode = quote! {
                    let decoded: #id = ::serde_json::from_value(::serde_json::Value::Array(args))
                        .map_err(|e| crate::ext::InvokeError::InvalidArguments(e.to_string()))?;
                };
                args_ty = Some(id);
                decode
            };
            let mut unwrap = Tokens::new();
            let mut convert = Tokens::new();
            let mut span = ident.span();
            if let ReturnType::Type(_, ty) = &sig.output {
                let success = if let Some(seg) = segment(ty).filter(|s| s.ident == "Result") {
                    let PathArguments::AngleBracketed(a) = &seg.arguments else {
                        return Err(Error::new_spanned(ty, CANNOT_CROSS));
                    };
                    for arg in &a.args {
                        if let GenericArgument::Type(t) = arg {
                            validate(t)?;
                        }
                    }
                    unwrap = quote!(.map_err(crate::ext::InvokeError::from)?);
                    first_type(seg).ok_or_else(|| Error::new_spanned(ty, CANNOT_CROSS))?
                } else {
                    validate(ty)?;
                    ty
                };
                span = success.span();
                if let Some(step) = into_return(success) {
                    let expr = step.apply(&format_ident!("out"), false);
                    convert = quote_spanned!(span=> let out = #expr;);
                }
                let owned = owned(success, &mut Vec::new());
                if !matches!(&owned, Type::Tuple(t) if t.elems.is_empty()) {
                    let id = format_ident!("{prefix}Result");
                    let field = quote_spanned!(span=> #owned);
                    defs.extend(quote! {
                        #[doc(hidden)]
                        #[allow(dead_code)]
                        #[derive(::specta::Type)]
                        #[specta(transparent)]
                        struct #id(#field);
                    });
                    result_ty = Some(id);
                }
            }
            let mut thunk = String::from("__ext_");
            for (i, ch) in object.char_indices() {
                if i != 0 && ch.is_ascii_uppercase() {
                    thunk.push('_');
                }
                thunk.push(ch.to_ascii_lowercase());
            }
            let call = format_ident!("{thunk}_{}", ident.unraw());
            let await_ = sig.asyncness.map(|_| quote!(.await));
            let encode = quote_spanned!(span=> ::serde_json::to_value(&out));
            defs.extend(quote! {
                #[doc(hidden)]
                #[allow(dead_code, clippy::let_unit_value, clippy::needless_borrows_for_generic_args)]
                fn #call<'a>(target: &'a #target, _caller: &'a crate::ext::Caller, args: ::std::vec::Vec<::serde_json::Value>) -> crate::ext::CallFut<'a> {
                    ::std::boxed::Box::pin(async move {
                        #decode
                        #prepare
                        #receiver
                        let out = receiver.#ident(#(#args),*) #await_ #unwrap;
                        #convert
                        #encode.map_err(|e| crate::ext::InvokeError::Encode(e.to_string()))
                    })
                }
            });
            (quote!(Some(#call)), quote!(None))
        }
    };
    let register = args_ty.iter().chain(result_ty.iter());
    let type_name = |ty: &Option<Ident>| match ty {
        Some(t) => quote!(Some(stringify!(#t))),
        None => quote!(None),
    };
    let args_name = type_name(&args_ty);
    let result_name = type_name(&result_ty);
    Ok(quote! {
        crate::ext::MethodEntry {
            call: #call,
            meta: crate::ext::Meta {
                name: #name, rust_name: #rust_name, doc: #doc, params: &[#(#params),*],
                args_type: #args_name, result_type: #result_name, skip: #skip,
                register: |types| types #(.register::<#register>())*,
            },
        }
    })
}
