//! Derive macros for the lua-rs embedding API.
//!
//! - `#[derive(LuaUserData)]` on a struct generates the `UserData` impl. Rust
//!   visibility is the scriptability boundary: **public** named fields are
//!   auto-exposed to Lua (`obj.field` reads/writes); private fields stay
//!   encapsulated unless force-exposed with `#[lua(field)]`. Tuple/newtype and
//!   unit structs (`struct Handle(App);`) expose no fields and become opaque
//!   userdata handles — ready for `#[lua(methods)]` and metamethods. Field
//!   attributes: `#[lua(skip)]`, `#[lua(readonly)]`, `#[lua(name = "...")]`,
//!   `#[lua(field)]` (force-expose a private field). `IntoLua` comes for free
//!   from the runtime's blanket `impl<T: UserData> IntoLua for T`.
//! - Struct attribute `#[lua_impl(Display, PartialEq, PartialOrd)]` wires the matching
//!   metamethods (`__tostring`, `__eq`, `__lt`/`__le`) from the type's Rust trait impls.
//! - Struct attribute `#[lua(methods)]` makes the generated `UserData` also register the
//!   methods declared by `#[lua_methods]` on an `impl` block.
//! - `#[lua_methods]` on an `impl` block exposes each `pub fn(&self/&mut self, ...)` to
//!   Lua as `obj:method(args)`.

use proc_macro::TokenStream;
use quote::quote;
use syn::{
    parse_macro_input, Data, DeriveInput, Fields, FnArg, ImplItem, ItemImpl, LitStr, ReturnType,
    Type,
};

// ---------------------------------------------------------------------------
// #[derive(LuaUserData)]
// ---------------------------------------------------------------------------

struct FieldCfg {
    ident: syn::Ident,
    ty: Type,
    lua_name: String,
    skip: bool,
    readonly: bool,
    force_field: bool,
}

fn parse_field_cfg(field: &syn::Field) -> syn::Result<FieldCfg> {
    let ident = field
        .ident
        .clone()
        .ok_or_else(|| syn::Error::new_spanned(field, "LuaUserData requires named fields"))?;
    let mut cfg = FieldCfg {
        lua_name: ident.to_string(),
        ident,
        ty: field.ty.clone(),
        skip: false,
        readonly: false,
        force_field: false,
    };
    for attr in &field.attrs {
        if !attr.path().is_ident("lua") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("skip") {
                cfg.skip = true;
                Ok(())
            } else if meta.path.is_ident("readonly") {
                cfg.readonly = true;
                Ok(())
            } else if meta.path.is_ident("field") {
                cfg.force_field = true;
                Ok(())
            } else if meta.path.is_ident("name") {
                let lit: LitStr = meta.value()?.parse()?;
                cfg.lua_name = lit.value();
                Ok(())
            } else {
                Err(meta.error(
                    "unknown #[lua(...)] attribute; expected skip, readonly, field, or name",
                ))
            }
        })?;
    }
    Ok(cfg)
}

/// Struct-level configuration from `#[lua(methods)]` and `#[lua_impl(...)]`.
struct StructCfg {
    register_methods: bool,
    impl_display: bool,
    impl_partial_eq: bool,
    impl_partial_ord: bool,
}

fn parse_struct_cfg(input: &DeriveInput) -> syn::Result<StructCfg> {
    let mut cfg = StructCfg {
        register_methods: false,
        impl_display: false,
        impl_partial_eq: false,
        impl_partial_ord: false,
    };
    for attr in &input.attrs {
        if attr.path().is_ident("lua") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("methods") {
                    cfg.register_methods = true;
                    Ok(())
                } else {
                    Err(meta.error("unknown #[lua(...)] attribute on struct; expected methods"))
                }
            })?;
        } else if attr.path().is_ident("lua_impl") {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("Display") {
                    cfg.impl_display = true;
                    Ok(())
                } else if meta.path.is_ident("PartialEq") {
                    cfg.impl_partial_eq = true;
                    Ok(())
                } else if meta.path.is_ident("PartialOrd") {
                    cfg.impl_partial_ord = true;
                    Ok(())
                } else {
                    Err(meta.error(
                        "unknown #[lua_impl(...)] trait; expected Display, PartialEq, or PartialOrd",
                    ))
                }
            })?;
        }
    }
    Ok(cfg)
}

/// Derive `UserData` for a struct: field access plus optional methods/metamethods.
#[proc_macro_derive(LuaUserData, attributes(lua, lua_impl))]
pub fn derive_lua_user_data(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    expand_derive(input).unwrap_or_else(|e| e.to_compile_error().into())
}

fn expand_derive(input: DeriveInput) -> syn::Result<TokenStream> {
    let name = &input.ident;

    if !input.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &input.generics,
            "LuaUserData does not yet support generic types",
        ));
    }

    let scfg = parse_struct_cfg(&input)?;

    let fields: Vec<&syn::Field> = match &input.data {
        Data::Struct(s) => match &s.fields {
            Fields::Named(named) => named.named.iter().collect(),
            // Tuple/newtype and unit structs have no named fields, so there is
            // nothing to auto-expose. The struct still becomes an opaque
            // `UserData` handle (type name, methods, metamethods) — the common
            // `struct Handle(App);` engine/resource-wrapper case. (issue #57)
            Fields::Unnamed(_) | Fields::Unit => Vec::new(),
        },
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "LuaUserData currently supports only structs",
            ))
        }
    };

    let mut field_regs = Vec::new();
    for field in fields {
        let cfg = parse_field_cfg(field)?;
        // Rust visibility is the scriptability boundary: public fields are
        // auto-exposed; private fields stay encapsulated unless the author
        // force-exposes one with `#[lua(field)]`. This lets a struct hold a
        // non-`Clone`/non-marshalable private field (e.g. `app: App`) and
        // still derive cleanly, since unexposed fields get no getter/setter
        // and so pick up no `Clone`/`IntoLua`/`FromLua` bound. (issue #56)
        let is_pub = matches!(field.vis, syn::Visibility::Public(_));
        if cfg.skip || (!is_pub && !cfg.force_field) {
            continue;
        }
        let ident = &cfg.ident;
        let ty = &cfg.ty;
        let lua_name = &cfg.lua_name;
        field_regs.push(quote! {
            __m.add_field_method_get(#lua_name, |_, __this| {
                ::core::result::Result::Ok(::core::clone::Clone::clone(&__this.#ident))
            });
        });
        if !cfg.readonly {
            field_regs.push(quote! {
                __m.add_field_method_set(#lua_name, |_, __this, __value: #ty| {
                    __this.#ident = __value;
                    ::core::result::Result::Ok(())
                });
            });
        }
    }

    let methods_call = if scfg.register_methods {
        quote! { <Self>::__lua_register_methods(__m); }
    } else {
        quote! {}
    };

    let mut meta_regs = Vec::new();
    if scfg.impl_display {
        meta_regs.push(quote! {
            __m.add_meta_method(::lua_rs_runtime::MetaMethod::ToString, |_, __this, ()| {
                ::core::result::Result::Ok(::std::string::ToString::to_string(__this))
            });
        });
    }
    if scfg.impl_partial_eq {
        meta_regs.push(quote! {
            __m.add_meta_method(
                ::lua_rs_runtime::MetaMethod::Eq,
                |_, __this, __other: ::lua_rs_runtime::Value| {
                    if let ::lua_rs_runtime::Value::UserData(__ud) = __other {
                        if let ::core::result::Result::Ok(__o) = __ud.borrow::<#name>() {
                            return ::core::result::Result::Ok(*__this == *__o);
                        }
                    }
                    ::core::result::Result::Ok(false)
                },
            );
        });
    }
    if scfg.impl_partial_ord {
        meta_regs.push(quote! {
            __m.add_meta_method(
                ::lua_rs_runtime::MetaMethod::Lt,
                |_, __this, __other: ::lua_rs_runtime::Value| {
                    if let ::lua_rs_runtime::Value::UserData(__ud) = __other {
                        if let ::core::result::Result::Ok(__o) = __ud.borrow::<#name>() {
                            return ::core::result::Result::Ok(*__this < *__o);
                        }
                    }
                    ::core::result::Result::Ok(false)
                },
            );
            __m.add_meta_method(
                ::lua_rs_runtime::MetaMethod::Le,
                |_, __this, __other: ::lua_rs_runtime::Value| {
                    if let ::lua_rs_runtime::Value::UserData(__ud) = __other {
                        if let ::core::result::Result::Ok(__o) = __ud.borrow::<#name>() {
                            return ::core::result::Result::Ok(*__this <= *__o);
                        }
                    }
                    ::core::result::Result::Ok(false)
                },
            );
        });
    }

    let add_meta_methods = if meta_regs.is_empty() {
        quote! {}
    } else {
        quote! {
            fn add_meta_methods<__M: ::lua_rs_runtime::UserDataMethods<Self>>(__m: &mut __M) {
                #(#meta_regs)*
            }
        }
    };

    let expanded = quote! {
        impl ::lua_rs_runtime::UserData for #name {
            fn add_methods<__M: ::lua_rs_runtime::UserDataMethods<Self>>(__m: &mut __M) {
                #(#field_regs)*
                #methods_call
            }
            #add_meta_methods
        }
    };

    Ok(expanded.into())
}

// ---------------------------------------------------------------------------
// #[lua_methods]
// ---------------------------------------------------------------------------

/// Expose an `impl` block's public methods to Lua as `obj:method(args)`.
#[proc_macro_attribute]
pub fn lua_methods(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemImpl);
    expand_methods(item).unwrap_or_else(|e| e.to_compile_error().into())
}

fn expand_methods(item: ItemImpl) -> syn::Result<TokenStream> {
    let self_ty = &item.self_ty;
    let mut regs = Vec::new();

    for impl_item in &item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };
        if !matches!(method.vis, syn::Visibility::Public(_)) {
            continue;
        }

        // Must have a self receiver to be callable as obj:method(...).
        let receiver = method.sig.inputs.first().and_then(|arg| match arg {
            FnArg::Receiver(r) => Some(r),
            _ => None,
        });
        let Some(receiver) = receiver else {
            continue;
        };
        let is_mut = receiver.mutability.is_some();

        let name = &method.sig.ident;
        let lua_name = name.to_string();

        // Collect the non-self arguments: names + types.
        let mut arg_names = Vec::new();
        let mut arg_types = Vec::new();
        for (i, arg) in method.sig.inputs.iter().enumerate().skip(1) {
            let FnArg::Typed(pat) = arg else {
                return Err(syn::Error::new_spanned(
                    arg,
                    "#[lua_methods] does not support a second receiver",
                ));
            };
            let ident = syn::Ident::new(&format!("__a{i}"), proc_macro2::Span::call_site());
            arg_names.push(ident);
            arg_types.push((*pat.ty).clone());
        }

        // Closure argument binding: () for none, `name: T` for one, `(..): (..)` for many.
        let arg_binding = match arg_names.len() {
            0 => quote! { () },
            1 => {
                let n = &arg_names[0];
                let t = &arg_types[0];
                quote! { #n: #t }
            }
            _ => {
                quote! { ( #(#arg_names),* ): ( #(#arg_types),* ) }
            }
        };

        // A method that returns a reference can't be marshaled as a Lua value;
        // instead it names a sub-object reachable from `self`. Register it as
        // an `add_function` that builds a delegate (a live sub-reference,
        // re-borrowed from the parent per call). `&mut T` returns become a
        // mutable delegate, `&T` returns a read-only one.
        if let ReturnType::Type(_, ty) = &method.sig.output {
            if let Type::Reference(r) = &**ty {
                let referent = &*r.elem;
                let ret_is_mut = r.mutability.is_some();
                if !ret_is_mut && is_mut {
                    return Err(syn::Error::new_spanned(
                        &method.sig,
                        "#[lua_methods]: a method returning `&T` must take `&self`; \
                         use `-> &mut T` to expose a mutable delegate",
                    ));
                }
                let func_binding = if arg_names.is_empty() {
                    quote! { __ud: ::lua_rs_runtime::AnyUserData }
                } else {
                    quote! {
                        ( __ud #(, #arg_names)* ):
                            ( ::lua_rs_runtime::AnyUserData #(, #arg_types)* )
                    }
                };
                let accessor = quote! { move |__this| <#self_ty>::#name(__this #(, #arg_names)*) };
                let reg = if ret_is_mut {
                    quote! {
                        __m.add_function(#lua_name, |__lua, #func_binding| {
                            __ud.delegate::<Self, #referent, _>(__lua, #accessor)
                        });
                    }
                } else {
                    quote! {
                        __m.add_function(#lua_name, |__lua, #func_binding| {
                            __ud.delegate_ref::<Self, #referent, _>(__lua, #accessor)
                        });
                    }
                };
                regs.push(reg);
                continue;
            }
        }

        let call = quote! { <#self_ty>::#name(__this #(, #arg_names)*) };
        let returns_unit = matches!(&method.sig.output, ReturnType::Default)
            || matches!(&method.sig.output, ReturnType::Type(_, ty) if is_unit_type(ty));
        let body = if returns_unit {
            quote! { { #call; ::core::result::Result::Ok(()) } }
        } else {
            quote! { ::core::result::Result::Ok(#call) }
        };

        if is_mut {
            regs.push(quote! {
                __m.add_method_mut(#lua_name, |_, __this, #arg_binding| #body);
            });
        } else {
            regs.push(quote! {
                __m.add_method(#lua_name, |_, __this, #arg_binding| #body);
            });
        }
    }

    let expanded = quote! {
        #item

        impl #self_ty {
            #[doc(hidden)]
            fn __lua_register_methods<__M: ::lua_rs_runtime::UserDataMethods<Self>>(__m: &mut __M) {
                #(#regs)*
            }
        }
    };

    Ok(expanded.into())
}

fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(t) if t.elems.is_empty())
}
