//! `#[derive(WaveWire)]` — implements `wavedb_core::Wire` for structs and
//! enums. See `docs/wire_format.md` for the layout contract.
//!
//! - **Structs** flatten: field stack slots inline in declaration order,
//!   heap payloads appended in the same order. `STACK_SIZE` is the const sum
//!   of the field stack sizes.
//! - **Enums with only field-less variants** are 1 stack byte (the tag,
//!   the variant's ordinal index).
//! - **Enums with any payload variant** are `1 + 4` stack bytes (tag +
//!   `u32` payload length) and write the variant's fields as one
//!   self-contained `[stack][heap]` unit in the heap section.

use proc_macro2::TokenStream;
use quote::{format_ident, quote};
use syn::{Data, DataEnum, DataStruct, DeriveInput, Fields, Index};

pub fn expand(input: &DeriveInput) -> syn::Result<TokenStream> {
    match &input.data {
        Data::Struct(s) => Ok(expand_struct(input, s)),
        Data::Enum(e) => expand_enum(input, e),
        Data::Union(_) => Err(syn::Error::new_spanned(
            &input.ident,
            "#[derive(WaveWire)] does not support unions",
        )),
    }
}

/// Build the `Wire` impl for an already-parsed struct item — used by the
/// `#[wave_db]` attribute macro so annotated structs get their impl without
/// the consumer crate needing a direct `wavedb-macros` dependency for the
/// derive path.
pub fn expand_item_struct(item: &syn::ItemStruct) -> TokenStream {
    let input = DeriveInput::from(item.clone());
    match &input.data {
        Data::Struct(s) => expand_struct(&input, s),
        _ => unreachable!("ItemStruct always converts to Data::Struct"),
    }
}

/// Member accessors (`self.foo` / `self.0`) and types for a field list.
fn members(fields: &Fields) -> (Vec<TokenStream>, Vec<&syn::Type>) {
    match fields {
        Fields::Named(named) => named
            .named
            .iter()
            .map(|f| {
                let id = f.ident.as_ref().expect("named field");
                (quote! { #id }, &f.ty)
            })
            .unzip(),
        Fields::Unnamed(unnamed) => unnamed
            .unnamed
            .iter()
            .enumerate()
            .map(|(i, f)| {
                let idx = Index::from(i);
                (quote! { #idx }, &f.ty)
            })
            .unzip(),
        Fields::Unit => (Vec::new(), Vec::new()),
    }
}

fn stack_size_sum(types: &[&syn::Type]) -> TokenStream {
    quote! { 0 #( + <#types as ::wavedb_core::Wire>::STACK_SIZE )* }
}

fn fixed_all(types: &[&syn::Type]) -> TokenStream {
    quote! { true #( && <#types as ::wavedb_core::Wire>::FIXED )* }
}

fn add_wire_bounds(generics: &syn::Generics) -> syn::Generics {
    let mut g = generics.clone();
    for param in g.type_params_mut() {
        param.bounds.push(syn::parse_quote!(::wavedb_core::Wire));
    }
    g
}

fn expand_struct(input: &DeriveInput, data: &DataStruct) -> TokenStream {
    let name = &input.ident;
    let (accessors, types) = members(&data.fields);

    let stack_size = stack_size_sum(&types);
    let fixed = fixed_all(&types);

    let construct = match &data.fields {
        Fields::Named(named) => {
            let idents: Vec<_> =
                named.named.iter().map(|f| f.ident.clone()).collect();
            let tys: Vec<_> = named.named.iter().map(|f| &f.ty).collect();
            quote! {
                ::core::result::Result::Ok(Self {
                    #( #idents: <#tys as ::wavedb_core::Wire>::read(r)?, )*
                })
            }
        }
        Fields::Unnamed(_) => {
            quote! {
                ::core::result::Result::Ok(Self(
                    #( <#types as ::wavedb_core::Wire>::read(r)?, )*
                ))
            }
        }
        Fields::Unit => quote! { ::core::result::Result::Ok(Self) },
    };

    let generics = add_wire_bounds(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    quote! {
        #[automatically_derived]
        impl #impl_generics ::wavedb_core::Wire for #name #ty_generics #where_clause {
            const STACK_SIZE: usize = #stack_size;
            const FIXED: bool = #fixed;

            fn heap_size(&self) -> usize {
                0 #( + ::wavedb_core::Wire::heap_size(&self.#accessors) )*
            }

            fn write_stack(
                &self,
                w: &mut ::wavedb_core::WireWriter,
            ) -> ::wavedb_core::WireResult<()> {
                #( ::wavedb_core::Wire::write_stack(&self.#accessors, w)?; )*
                ::core::result::Result::Ok(())
            }

            fn read(
                r: &mut ::wavedb_core::WireReader<'_>,
            ) -> ::wavedb_core::WireResult<Self> {
                #construct
            }
        }
    }
}

/// Bind a variant's fields to generated idents for match arms.
fn variant_bindings(
    fields: &Fields,
) -> (TokenStream, Vec<syn::Ident>, Vec<&syn::Type>) {
    match fields {
        Fields::Named(named) => {
            let idents: Vec<syn::Ident> = named
                .named
                .iter()
                .map(|f| f.ident.clone().expect("named field"))
                .collect();
            let types = named.named.iter().map(|f| &f.ty).collect();
            (quote! { { #( #idents ),* } }, idents, types)
        }
        Fields::Unnamed(unnamed) => {
            let idents: Vec<syn::Ident> = (0..unnamed.unnamed.len())
                .map(|i| format_ident!("__f{i}"))
                .collect();
            let types = unnamed.unnamed.iter().map(|f| &f.ty).collect();
            (quote! { ( #( #idents ),* ) }, idents, types)
        }
        Fields::Unit => (quote! {}, Vec::new(), Vec::new()),
    }
}

/// Build the `Self::Variant { .. }` / `Self::Variant(..)` constructor from
/// values read in field order.
fn variant_construct(
    vname: &syn::Ident,
    fields: &Fields,
    types: &[&syn::Type],
) -> TokenStream {
    match fields {
        Fields::Named(named) => {
            let idents: Vec<_> =
                named.named.iter().map(|f| f.ident.clone()).collect();
            quote! {
                Self::#vname {
                    #( #idents: <#types as ::wavedb_core::Wire>::read(&mut __sub)?, )*
                }
            }
        }
        Fields::Unnamed(_) => quote! {
            Self::#vname(
                #( <#types as ::wavedb_core::Wire>::read(&mut __sub)?, )*
            )
        },
        Fields::Unit => quote! { Self::#vname },
    }
}

#[allow(clippy::too_many_lines)]
fn expand_enum(
    input: &DeriveInput,
    data: &DataEnum,
) -> syn::Result<TokenStream> {
    let name = &input.ident;
    let name_str = name.to_string();

    if data.variants.is_empty() {
        return Err(syn::Error::new_spanned(
            name,
            "#[derive(WaveWire)] requires at least one variant",
        ));
    }
    if data.variants.len() > 256 {
        return Err(syn::Error::new_spanned(
            name,
            "#[derive(WaveWire)] supports at most 256 variants (u8 tag)",
        ));
    }

    let generics = add_wire_bounds(&input.generics);
    let (impl_generics, ty_generics, where_clause) = generics.split_for_impl();

    let all_unit = data
        .variants
        .iter()
        .all(|v| matches!(v.fields, Fields::Unit));

    if all_unit {
        // 1 stack byte: the variant's ordinal index.
        let arms_write = data.variants.iter().enumerate().map(|(i, v)| {
            let vname = &v.ident;
            let tag = u8::try_from(i).expect("validated above");
            quote! { Self::#vname => #tag, }
        });
        let arms_read = data.variants.iter().enumerate().map(|(i, v)| {
            let vname = &v.ident;
            let tag = u8::try_from(i).expect("validated above");
            quote! { #tag => ::core::result::Result::Ok(Self::#vname), }
        });

        return Ok(quote! {
            #[automatically_derived]
            impl #impl_generics ::wavedb_core::Wire for #name #ty_generics #where_clause {
                const STACK_SIZE: usize = 1;
                const FIXED: bool = true;

                fn heap_size(&self) -> usize { 0 }

                fn write_stack(
                    &self,
                    w: &mut ::wavedb_core::WireWriter,
                ) -> ::wavedb_core::WireResult<()> {
                    let tag: u8 = match self { #( #arms_write )* };
                    ::wavedb_core::Wire::write_stack(&tag, w)
                }

                fn read(
                    r: &mut ::wavedb_core::WireReader<'_>,
                ) -> ::wavedb_core::WireResult<Self> {
                    let tag = <u8 as ::wavedb_core::Wire>::read(r)?;
                    match tag {
                        #( #arms_read )*
                        _ => ::core::result::Result::Err(
                            ::wavedb_core::WireError::InvalidTag {
                                type_name: #name_str,
                                tag,
                            },
                        ),
                    }
                }
            }
        });
    }

    // Payload form: [tag u8][payload len u32] in stack, variant fields as one
    // [stack][heap] unit in the heap section.
    let mut heap_arms = Vec::new();
    let mut write_arms = Vec::new();
    let mut read_arms = Vec::new();

    for (i, v) in data.variants.iter().enumerate() {
        let vname = &v.ident;
        let tag = u8::try_from(i).expect("validated above");
        let (pattern, idents, types) = variant_bindings(&v.fields);
        let vstack = stack_size_sum(&types);

        heap_arms.push(quote! {
            Self::#vname #pattern => {
                (#vstack) #( + ::wavedb_core::Wire::heap_size(#idents) )*
            }
        });

        write_arms.push(quote! {
            Self::#vname #pattern => {
                ::wavedb_core::Wire::write_stack(&#tag, w)?;
                let __payload =
                    (#vstack) #( + ::wavedb_core::Wire::heap_size(#idents) )*;
                w.put_len_slot(__payload)?;
                w.with_unit(#vstack, |w| {
                    #( ::wavedb_core::Wire::write_stack(#idents, w)?; )*
                    ::core::result::Result::Ok(())
                })?;
            }
        });

        let construct = variant_construct(vname, &v.fields, &types);
        read_arms.push(quote! {
            #tag => {
                let mut __sub =
                    ::wavedb_core::WireReader::for_unit(__region, #vstack)?;
                let __value = #construct;
                __sub.finish_unit()?;
                ::core::result::Result::Ok(__value)
            }
        });
    }

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics ::wavedb_core::Wire for #name #ty_generics #where_clause {
            const STACK_SIZE: usize = 1 + 4;
            const FIXED: bool = false;

            fn heap_size(&self) -> usize {
                match self { #( #heap_arms )* }
            }

            fn write_stack(
                &self,
                w: &mut ::wavedb_core::WireWriter,
            ) -> ::wavedb_core::WireResult<()> {
                match self { #( #write_arms )* }
                ::core::result::Result::Ok(())
            }

            fn read(
                r: &mut ::wavedb_core::WireReader<'_>,
            ) -> ::wavedb_core::WireResult<Self> {
                let tag = <u8 as ::wavedb_core::Wire>::read(r)?;
                let __region = r.take_len_slot_region()?;
                match tag {
                    #( #read_arms )*
                    _ => ::core::result::Result::Err(
                        ::wavedb_core::WireError::InvalidTag {
                            type_name: #name_str,
                            tag,
                        },
                    ),
                }
            }
        }
    })
}
