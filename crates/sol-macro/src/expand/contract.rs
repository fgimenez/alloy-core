//! [`ItemContract`] expansion.

use super::{attr, ty, ExpCtxt};
use crate::utils::ExprArray;
use ast::{Item, ItemContract, ItemError, ItemEvent, ItemFunction, SolIdent};
use heck::ToSnakeCase;
use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote};
use syn::{ext::IdentExt, parse_quote, Attribute, Result};

/// Expands an [`ItemContract`]:
///
/// ```ignore,pseudo-code
/// pub mod #name {
///     pub enum #{name}Calls {
///         ...
///    }
///
///     pub enum #{name}Errors {
///         ...
///    }
/// }
/// ```
pub(super) fn expand(cx: &ExpCtxt<'_>, contract: &ItemContract) -> Result<TokenStream> {
    let ItemContract {
        attrs, name, body, ..
    } = contract;

    let mut functions = Vec::with_capacity(contract.body.len());
    let mut errors = Vec::with_capacity(contract.body.len());
    let mut events = Vec::with_capacity(contract.body.len());

    let mut item_tokens = TokenStream::new();
    let d_attrs: Vec<Attribute> = attr::derives(attrs).cloned().collect();
    for item in body {
        match item {
            Item::Function(function) => functions.push(function),
            Item::Error(error) => errors.push(error),
            Item::Event(event) => events.push(event),
            _ => {}
        }
        if !d_attrs.is_empty() {
            item_tokens.extend(quote!(#(#d_attrs)*));
        }
        item_tokens.extend(cx.expand_item(item)?);
    }

    let functions_enum = (functions.len() > 1).then(|| {
        let mut attrs = d_attrs.clone();
        let doc_str = format!("Container for all the `{name}` function calls.");
        attrs.push(parse_quote!(#[doc = #doc_str]));
        CallLikeExpander::from_functions(cx, name, functions).expand(&attrs)
    });

    let errors_enum = (errors.len() > 1).then(|| {
        let mut attrs = d_attrs.clone();
        let doc_str = format!("Container for all the `{name}` custom errors.");
        attrs.push(parse_quote!(#[doc = #doc_str]));
        CallLikeExpander::from_errors(cx, name, errors).expand(&attrs)
    });

    let events_enum = (events.len() > 1).then(|| {
        let mut attrs = d_attrs;
        let doc_str = format!("Container for all the `{name}` events.");
        attrs.push(parse_quote!(#[doc = #doc_str]));
        CallLikeExpander::from_events(cx, name, events).expand_event(&attrs)
    });

    let mod_attrs = attr::docs(attrs);
    let tokens = quote! {
        #(#mod_attrs)*
        #[allow(non_camel_case_types, non_snake_case, clippy::style)]
        pub mod #name {
            #item_tokens
            #functions_enum
            #errors_enum
            #events_enum
        }
    };
    Ok(tokens)
}

// note that item impls generated here do not need to be wrapped in an anonymous
// constant (`const _: () = { ... };`) because they are in one already

/// Expands a `SolInterface` enum:
///
/// ```ignore,pseudo-code
/// #name = #{contract_name}Calls | #{contract_name}Errors | #{contract_name}Events;
///
/// pub enum #name {
///    #(#variants(#types),)*
/// }
///
/// impl SolInterface for #name {
///     ...
/// }
///
/// impl #name {
///     #(
///         pub fn #is_variant,#as_variant,#as_variant_mut(...) -> ... { ... }
///     )*
/// }
/// ```
struct CallLikeExpander {
    name: Ident,
    variants: Vec<Ident>,
    min_data_len: usize,
    trait_: Ident,
    data: CallLikeExpanderData,
}

enum CallLikeExpanderData {
    Function {
        selectors: Vec<ExprArray<u8, 4>>,
        types: Vec<Ident>,
    },
    Error {
        selectors: Vec<ExprArray<u8, 4>>,
    },
    Event {
        selectors: Vec<ExprArray<u8, 32>>,
    },
}

impl CallLikeExpander {
    fn from_functions(
        cx: &ExpCtxt<'_>,
        contract_name: &SolIdent,
        functions: Vec<&ItemFunction>,
    ) -> Self {
        let variants: Vec<_> = functions
            .iter()
            .map(|f| cx.function_name_ident(f).0)
            .collect();

        let types: Vec<_> = variants.iter().map(|name| cx.raw_call_name(name)).collect();

        let mut selectors: Vec<_> = functions.iter().map(|f| cx.function_selector(f)).collect();
        selectors.sort_unstable_by_key(|a| a.array);

        Self {
            name: format_ident!("{contract_name}Calls"),
            variants,
            min_data_len: functions
                .iter()
                .map(|function| ty::params_base_data_size(cx, &function.arguments))
                .min()
                .unwrap(),
            trait_: Ident::new("SolCall", Span::call_site()),
            data: CallLikeExpanderData::Function { selectors, types },
        }
    }

    fn from_errors(cx: &ExpCtxt<'_>, contract_name: &SolIdent, errors: Vec<&ItemError>) -> Self {
        let mut selectors: Vec<_> = errors.iter().map(|e| cx.error_selector(e)).collect();
        selectors.sort_unstable_by_key(|a| a.array);

        Self {
            name: format_ident!("{contract_name}Errors"),
            variants: errors.iter().map(|error| error.name.0.clone()).collect(),
            min_data_len: errors
                .iter()
                .map(|error| ty::params_base_data_size(cx, &error.parameters))
                .min()
                .unwrap(),
            trait_: Ident::new("SolError", Span::call_site()),
            data: CallLikeExpanderData::Error { selectors },
        }
    }

    fn from_events(cx: &ExpCtxt<'_>, contract_name: &SolIdent, events: Vec<&ItemEvent>) -> Self {
        let mut selectors: Vec<_> = events.iter().map(|e| cx.event_selector(e)).collect();
        selectors.sort_unstable_by_key(|a| a.array);

        Self {
            name: format_ident!("{contract_name}Events"),
            variants: events.iter().map(|event| event.name.0.clone()).collect(),
            min_data_len: events
                .iter()
                .map(|event| ty::params_base_data_size(cx, &event.params()))
                .min()
                .unwrap(),
            trait_: Ident::new("SolEvent", Span::call_site()),
            data: CallLikeExpanderData::Event { selectors },
        }
    }

    /// Type name overrides. Currently only functions support this through
    /// overloading.
    fn types(&self) -> &[Ident] {
        match &self.data {
            CallLikeExpanderData::Function { types, .. } => types,
            _ => &self.variants,
        }
    }

    fn expand(self, attrs: &[Attribute]) -> TokenStream {
        let Self {
            name,
            variants,
            min_data_len,
            trait_,
            ..
        } = &self;
        let types = self.types();

        assert_eq!(variants.len(), types.len());
        let name_s = name.to_string();
        let count = variants.len();
        let def = self.generate_enum(attrs);
        quote! {
            #def

            #[automatically_derived]
            impl ::alloy_sol_types::SolInterface for #name {
                const NAME: &'static str = #name_s;
                const MIN_DATA_LENGTH: usize = #min_data_len;
                const COUNT: usize = #count;

                #[inline]
                fn selector(&self) -> [u8; 4] {
                    match self {#(
                        Self::#variants(_) => <#types as ::alloy_sol_types::#trait_>::SELECTOR,
                    )*}
                }

                #[inline]
                fn selector_at(i: usize) -> Option<[u8; 4]> {
                    Self::SELECTORS.get(i).copied()
                }

                #[inline]
                fn type_check(selector: [u8; 4]) -> ::alloy_sol_types::Result<()> {
                    match selector {
                        #(<#types as ::alloy_sol_types::#trait_>::SELECTOR)|* => Ok(()),
                        s => ::core::result::Result::Err(::alloy_sol_types::Error::unknown_selector(
                            Self::NAME,
                            s,
                        )),
                    }
                }

                #[inline]
                fn decode_raw(
                    selector: [u8; 4],
                    data: &[u8],
                    validate: bool
                )-> ::alloy_sol_types::Result<Self> {
                    match selector {
                        #(<#types as ::alloy_sol_types::#trait_>::SELECTOR => {
                            <#types as ::alloy_sol_types::#trait_>::decode_raw(data, validate)
                                .map(Self::#variants)
                        })*
                        s => ::core::result::Result::Err(::alloy_sol_types::Error::unknown_selector(
                            Self::NAME,
                            s,
                        )),
                    }
                }

                #[inline]
                fn encoded_size(&self) -> usize {
                    match self {#(
                        Self::#variants(inner) =>
                            <#types as ::alloy_sol_types::#trait_>::encoded_size(inner),
                    )*}
                }

                #[inline]
                fn encode_raw(&self, out: &mut ::alloy_sol_types::private::Vec<u8>) {
                    match self {#(
                        Self::#variants(inner) =>
                            <#types as ::alloy_sol_types::#trait_>::encode_raw(inner, out),
                    )*}
                }
            }
        }
    }

    fn expand_event(self, attrs: &[Attribute]) -> TokenStream {
        // TODO: SolInterface for events
        self.generate_enum(attrs)
    }

    fn generate_enum(&self, attrs: &[Attribute]) -> TokenStream {
        let Self {
            name,
            variants,
            data,
            ..
        } = self;
        let (selectors, selector_type) = match data {
            CallLikeExpanderData::Function { selectors, .. }
            | CallLikeExpanderData::Error { selectors } => {
                (quote!(#(#selectors,)*), quote!([u8; 4]))
            }
            CallLikeExpanderData::Event { selectors } => {
                (quote!(#(#selectors,)*), quote!([u8; 32]))
            }
        };

        let types = self.types();
        let conversions = variants
            .iter()
            .zip(types)
            .map(|(v, t)| generate_variant_conversions(name, v, t));
        let methods = variants.iter().zip(types).map(generate_variant_methods);

        quote! {
            #(#attrs)*
            pub enum #name {
                #(#variants(#types),)*
            }

            #(#conversions)*

            #[automatically_derived]
            impl #name {
                /// All the selectors of this enum.
                ///
                /// Note that the selectors might not be in the same order as the
                /// variants, as they are sorted instead of ordered by definition.
                pub const SELECTORS: &'static [#selector_type] = &[#selectors];

                #(#methods)*
            }
        }
    }
}

fn generate_variant_conversions(name: &Ident, variant: &Ident, ty: &Ident) -> TokenStream {
    quote! {
        #[automatically_derived]
        impl ::core::convert::From<#ty> for #name {
            #[inline]
            fn from(value: #ty) -> Self {
                Self::#variant(value)
            }
        }

        #[automatically_derived]
        impl ::core::convert::TryFrom<#name> for #ty {
            type Error = #name;

            #[inline]
            fn try_from(value: #name) -> ::core::result::Result<Self, #name> {
                match value {
                    #name::#variant(value) => ::core::result::Result::Ok(value),
                    _ => ::core::result::Result::Err(value),
                }
            }
        }
    }
}

fn generate_variant_methods((variant, ty): (&Ident, &Ident)) -> TokenStream {
    let name = variant.unraw();
    let name_snake = name.to_string().to_snake_case();

    let is_variant = format_ident!("is_{name_snake}");
    let is_variant_doc = format!("Returns `true` if `self` matches [`{name}`](Self::{name}).");

    let as_variant = format_ident!("as_{name_snake}");
    let as_variant_doc = format!(
        "Returns an immutable reference to the inner [`{ty}`] if `self` matches [`{name}`](Self::{name})."
    );

    let as_variant_mut = format_ident!("as_{name_snake}_mut");
    let as_variant_mut_doc = format!(
        "Returns a mutable reference to the inner [`{ty}`] if `self` matches [`{name}`](Self::{name})."
    );

    let try_into_variant = format_ident!("try_into_{name_snake}");
    let try_into_variant_doc =
        format!("Unwraps the inner [`{ty}`] if `self` matches [`{name}`](Self::{name}).");

    quote! {
        #[doc = #is_variant_doc]
        #[inline]
        pub const fn #is_variant(&self) -> bool {
            ::core::matches!(self, Self::#variant(_))
        }

        #[doc = #as_variant_doc]
        #[inline]
        pub const fn #as_variant(&self) -> ::core::option::Option<&#ty> {
            match self {
                Self::#variant(inner) => ::core::option::Option::Some(inner),
                _ => ::core::option::Option::None,
            }
        }

        #[doc = #as_variant_mut_doc]
        #[inline]
        pub fn #as_variant_mut(&mut self) -> ::core::option::Option<&mut #ty> {
            match self {
                Self::#variant(inner) => ::core::option::Option::Some(inner),
                _ => ::core::option::Option::None,
            }
        }

        #[doc = #try_into_variant_doc]
        #[inline]
        pub const fn #try_into_variant(self) -> ::core::result::Result<#ty, Self> {
            match self {
                Self::#variant(inner) => ::core::result::Result::Ok(inner),
                _ => ::core::result::Result::Err(self),
            }
        }
    }
}