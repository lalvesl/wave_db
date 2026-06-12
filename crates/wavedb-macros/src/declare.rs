//! The `declare_objects!` function-like macro — builds the static object
//! registry every node (quick, slow, client/WASM) compiles in at startup.
//!
//! ```rust,ignore
//! declare_objects! {
//!     pub mod app_objects {
//!         orders:   [Order1, Order2],
//!         profiles: [UserProfile1],
//!     }
//! }
//! ```
//!
//! Generates `pub mod app_objects` containing:
//!
//! - one submodule **per struct family** (`orders`, `profiles`) re-exporting
//!   its versions plus `STRUCT_ID`, `VERSIONS`, and the family's descriptor
//!   slice — with a compile-time check that all versions share one
//!   `struct_id`;
//! - `DESCRIPTORS` — every declared `&'static ObjectDescriptor`;
//! - `find(header: u32)` and friends — header-keyed lookup compiled as a
//!   constant-comparison chain (static dispatch, no `dyn`, no hashing);
//! - `validate(header, body)` / `preprocess(header, body)` — wire-level
//!   dispatch into each type's `WaveDbHooks` impl (typed decode, same
//!   compare-chain shape; hook-less types short-circuit without decoding);
//! - `REGISTRY: &'static ObjectRegistry` — the single handle node
//!   constructors take to become schema-aware;
//! - a compile-time duplicate-`(struct_id, version)` check across the whole
//!   declaration.

use proc_macro2::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::{Ident, Token, Visibility, braced, bracketed};

/// `family_name: [Type1, Type2, …]`
struct Family {
    name: Ident,
    types: Punctuated<syn::Path, Token![,]>,
}

impl Parse for Family {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        input.parse::<Token![:]>()?;
        let content;
        bracketed!(content in input);
        let types = content.parse_terminated(syn::Path::parse, Token![,])?;
        if types.is_empty() {
            return Err(syn::Error::new_spanned(
                &name,
                "family must declare at least one version",
            ));
        }
        Ok(Self { name, types })
    }
}

/// `pub mod name { family: [..], … }`
pub struct DeclareObjects {
    vis: Visibility,
    mod_name: Ident,
    families: Punctuated<Family, Token![,]>,
}

impl Parse for DeclareObjects {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let vis: Visibility = input.parse()?;
        input.parse::<Token![mod]>()?;
        let mod_name: Ident = input.parse()?;
        let content;
        braced!(content in input);
        let families = content.parse_terminated(Family::parse, Token![,])?;
        Ok(Self {
            vis,
            mod_name,
            families,
        })
    }
}

#[allow(clippy::too_many_lines)]
pub fn expand(decl: &DeclareObjects) -> TokenStream {
    let vis = &decl.vis;
    let mod_name = &decl.mod_name;

    let all_types: Vec<&syn::Path> =
        decl.families.iter().flat_map(|f| f.types.iter()).collect();
    let count = all_types.len();

    // ── Per-family submodules ────────────────────────────────────────────
    let family_mods = decl.families.iter().map(|family| {
        let fam_name = &family.name;
        let types: Vec<&syn::Path> = family.types.iter().collect();
        let first = types[0];
        let fam_doc =
            format!("Versions of the `{fam_name}` struct family.");
        quote! {
            #[doc = #fam_doc]
            #vis mod #fam_name {
                // Resolve the declared type paths in the invocation scope.
                use super::super::*;

                /// The family's permanent `struct_id`.
                pub const STRUCT_ID: u32 =
                    <#first as ::wavedb_core::WaveDbStruct>::STRUCT_ID;

                /// Declared schema versions, in declaration order.
                pub const VERSIONS: &[u8] = &[
                    #( <#types as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION ),*
                ];

                /// Descriptors of every declared version of this family.
                pub static DESCRIPTORS:
                    &[&'static ::wavedb_core::ObjectDescriptor] =
                    &[ #( #types::DESCRIPTOR ),* ];

                // Every version of a family must share one struct_id.
                const _: () = {
                    #(
                        assert!(
                            <#types as ::wavedb_core::WaveDbStruct>::STRUCT_ID
                                == STRUCT_ID,
                            "declare_objects!: family mixes struct_ids",
                        );
                    )*
                };
            }
        }
    });

    // ── Lookup fns: constant-comparison chain, no dyn ────────────────────
    let find_arms = all_types.iter().map(|ty| {
        quote! {
            if header == <#ty as ::wavedb_core::WaveDbStruct>::HEADER {
                return ::core::option::Option::Some(#ty::DESCRIPTOR);
            }
        }
    });

    // ── Hook dispatch: typed decode + WaveDbHooks, same compare chain ────
    let validate_arms = all_types.iter().map(|ty| {
        quote! {
            if header == <#ty as ::wavedb_core::WaveDbStruct>::HEADER {
                // Const condition — the decode below compiles out entirely
                // for types that declared no `validate` hook.
                if !<#ty as ::wavedb_core::WaveDbHooks>::HAS_VALIDATE {
                    return ::core::result::Result::Ok(());
                }
                let record: #ty = ::wavedb_core::wire::from_wire(body)?;
                return ::wavedb_core::WaveDbHooks::validate(&record)
                    .map_err(|source| ::wavedb_core::Error::Validation {
                        struct_id:
                            <#ty as ::wavedb_core::WaveDbStruct>::STRUCT_ID,
                        source,
                    });
            }
        }
    });
    let preprocess_arms = all_types.iter().map(|ty| {
        quote! {
            if header == <#ty as ::wavedb_core::WaveDbStruct>::HEADER {
                if !<#ty as ::wavedb_core::WaveDbHooks>::HAS_PREPROCESS {
                    return ::core::result::Result::Ok(
                        ::core::option::Option::None,
                    );
                }
                let mut record: #ty =
                    ::wavedb_core::wire::from_wire(body)?;
                ::wavedb_core::WaveDbHooks::preprocess(&mut record)
                    .map_err(|source| ::wavedb_core::Error::Validation {
                        struct_id:
                            <#ty as ::wavedb_core::WaveDbStruct>::STRUCT_ID,
                        source,
                    })?;
                return ::core::result::Result::Ok(
                    ::core::option::Option::Some(
                        ::wavedb_core::wire::to_wire(&record)?,
                    ),
                );
            }
        }
    });

    let mod_doc = format!(
        "Static object registry ({count} declared versions) — generated by `declare_objects!`."
    );
    let registry_idx: Vec<syn::Index> =
        (0..count).map(syn::Index::from).collect();

    quote! {
        #[doc = #mod_doc]
        #vis mod #mod_name {
            // Resolve the declared type paths in the invocation scope.
            use super::*;

            #( #family_mods )*

            /// Every declared object version's descriptor.
            pub static DESCRIPTORS:
                &[&'static ::wavedb_core::ObjectDescriptor] =
                &[ #( #all_types::DESCRIPTOR ),* ];

            // Compile-time duplicate-(struct_id, version) check.
            const _: () = {
                const HEADERS: [u32; #count] = [
                    #( <#all_types as ::wavedb_core::WaveDbStruct>::HEADER ),*
                ];
                let mut i = 0;
                while i < #count {
                    let mut j = i + 1;
                    while j < #count {
                        assert!(
                            HEADERS[i] != HEADERS[j],
                            "declare_objects!: duplicate (struct_id, version) header",
                        );
                        j += 1;
                    }
                    i += 1;
                }
                // Silence "unused" when count == 1.
                let _ = HEADERS;
            };

            /// Look up a descriptor by its `u32` record header
            /// (`struct_id << 8 | version`).
            ///
            /// Compiles to a chain of constant compares — monomorphised,
            /// no `dyn`, no hash table.
            #[must_use]
            pub fn find(
                header: u32,
            ) -> ::core::option::Option<&'static ::wavedb_core::ObjectDescriptor>
            {
                #( #find_arms )*
                ::core::option::Option::None
            }

            /// Whether this registry knows the given header.
            #[must_use]
            pub fn contains(header: u32) -> bool {
                find(header).is_some()
            }

            /// Compile-time stack-section size for a header, if declared.
            #[must_use]
            pub fn stack_size(header: u32) -> ::core::option::Option<usize> {
                find(header).map(|d| d.stack_size)
            }

            /// Heap-owning field names for a header, if declared.
            #[must_use]
            pub fn heap_fields(
                header: u32,
            ) -> ::core::option::Option<&'static [&'static str]> {
                find(header).map(|d| d.heap_fields)
            }

            /// Run the header's `validate` hook against a wire-encoded
            /// record body (no version-byte prefix, no record envelope).
            ///
            /// Decodes through the matching struct's typed `Wire` impl and
            /// calls its [`::wavedb_core::WaveDbHooks::validate`].  Types
            /// without a declared hook return `Ok(())` without decoding.
            ///
            /// # Errors
            ///
            /// - [`::wavedb_core::Error::UnknownHeader`] — header not declared.
            /// - [`::wavedb_core::Error::Validation`] — the hook rejected it.
            /// - [`::wavedb_core::Error::Wire`] — body bytes don't decode.
            pub fn validate(
                header: u32,
                body: &[u8],
            ) -> ::wavedb_core::Result<()> {
                #( #validate_arms )*
                ::core::result::Result::Err(
                    ::wavedb_core::Error::UnknownHeader(header),
                )
            }

            /// Run the header's `preprocess` hook against a wire-encoded
            /// record body, returning the re-encoded result.
            ///
            /// `Ok(None)` means the type declared no hook — the caller keeps
            /// the original bytes and skips the re-encode cost.
            ///
            /// # Errors
            ///
            /// Same cases as [`validate`].
            pub fn preprocess(
                header: u32,
                body: &[u8],
            ) -> ::wavedb_core::Result<
                ::core::option::Option<::std::vec::Vec<u8>>,
            > {
                #( #preprocess_arms )*
                ::core::result::Result::Err(
                    ::wavedb_core::Error::UnknownHeader(header),
                )
            }

            /// The registry handle to hand to node constructors
            /// (`QuickNode::with_registry`, …): descriptor lookup plus
            /// wire-level `validate` / `preprocess` dispatch as one
            /// `&'static` value.
            pub static REGISTRY: &::wavedb_core::ObjectRegistry =
                &::wavedb_core::ObjectRegistry {
                    descriptors: DESCRIPTORS,
                    find_fn: find,
                    validate_fn: validate,
                    preprocess_fn: preprocess,
                };

            /// Latest declared version of a struct family.
            #[must_use]
            pub fn latest_version(struct_id: u32) -> ::core::option::Option<u8> {
                let mut best: ::core::option::Option<u8> =
                    ::core::option::Option::None;
                #(
                    {
                        let d = DESCRIPTORS[#registry_idx];
                        if d.struct_id == struct_id
                            && best.is_none_or(|b| d.version > b)
                        {
                            best = ::core::option::Option::Some(d.version);
                        }
                    }
                )*
                best
            }
        }
    }
}
