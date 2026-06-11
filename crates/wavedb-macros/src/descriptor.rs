//! `ObjectDescriptor` emission for `#[wave_db]` structs.
//!
//! Every annotated struct gets an inherent
//! `pub const DESCRIPTOR: &'static ObjectDescriptor` describing its full wire
//! layout — stack size, per-field offsets, heapable flags — all computed from
//! `<T as Wire>::STACK_SIZE` / `FIXED` constants so the values are exact even
//! for nested user types the macro cannot classify syntactically.

use proc_macro2::TokenStream;
use quote::quote;
use syn::Ident;

/// Map a field type to a `FieldKind` variant path (syntactic, last segment).
///
/// `stack_offset` / `stack_size` / `heapable` are computed from `Wire`
/// constants and stay exact regardless of this classification; `Other` only
/// means no specialised handling is implied. The `Id` / `Anchor` suffix match
/// covers the macro-generated `FooId` / `FooAnchor` wrapper types.
fn field_kind(ty: &syn::Type) -> TokenStream {
    let k = |v: &str| {
        let ident = Ident::new(v, proc_macro2::Span::call_site());
        quote! { ::wavedb_core::FieldKind::#ident }
    };

    let syn::Type::Path(p) = ty else {
        return k("Other");
    };
    let Some(seg) = p.path.segments.last() else {
        return k("Other");
    };
    let name = seg.ident.to_string();
    match name.as_str() {
        "u8" => k("U8"),
        "u16" => k("U16"),
        "u32" => k("U32"),
        "u64" => k("U64"),
        "u128" => k("U128"),
        "i8" => k("I8"),
        "i16" => k("I16"),
        "i32" => k("I32"),
        "i64" => k("I64"),
        "i128" => k("I128"),
        "f32" => k("F32"),
        "f64" => k("F64"),
        "bool" => k("Bool"),
        "char" => k("Char"),
        "Id" => k("Id"),
        "String" => k("Str"),
        "Option" => k("Option"),
        "Vec" => {
            let is_u8 = matches!(
                &seg.arguments,
                syn::PathArguments::AngleBracketed(args) if args.args.iter().any(|a| {
                    matches!(
                        a,
                        syn::GenericArgument::Type(syn::Type::Path(tp))
                            if tp.path.is_ident("u8")
                    )
                })
            );
            if is_u8 { k("Bytes") } else { k("List") }
        }
        _ if name.ends_with("Id") || name.ends_with("Anchor") => k("Id"),
        _ => k("Other"),
    }
}

/// Build the `pub const DESCRIPTOR` item for an annotated struct.
///
/// `fields` must be the **final** field list (after `id` / `metadata`
/// injection) so offsets describe the real wire layout.
pub fn build_descriptor(
    name: &Ident,
    fields: &syn::FieldsNamed,
    shape: &TokenStream,
) -> TokenStream {
    let mut prior_types: Vec<&syn::Type> = Vec::new();
    let mut entries = Vec::new();
    let mut heap_names = Vec::new();

    for f in &fields.named {
        let ident = f.ident.as_ref().expect("named field");
        let fname = ident.to_string();
        let ty = &f.ty;
        let kind = field_kind(ty);
        let offset = quote! {
            0usize #( + <#prior_types as ::wavedb_core::Wire>::STACK_SIZE )*
        };
        entries.push(quote! {
            ::wavedb_core::FieldDescriptor {
                name: #fname,
                stack_offset: #offset,
                stack_size: <#ty as ::wavedb_core::Wire>::STACK_SIZE,
                heapable: !<#ty as ::wavedb_core::Wire>::FIXED,
                kind: #kind,
            }
        });
        if crate::utils::is_heapable_type(ty) {
            heap_names.push(fname);
        }
        prior_types.push(ty);
    }

    quote! {
        /// Complete wire-layout descriptor for this object version.
        ///
        /// `'static` data consumed by the `declare_objects!` registry; lets
        /// any node locate fields, heap properties, and index keys without
        /// deserialising a record.
        pub const DESCRIPTOR: &'static ::wavedb_core::ObjectDescriptor =
            &::wavedb_core::ObjectDescriptor {
                header: <Self as ::wavedb_core::WaveDbStruct>::HEADER,
                struct_id: <Self as ::wavedb_core::WaveDbStruct>::STRUCT_ID,
                version: <Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                shape: #shape,
                type_name: ::core::stringify!(#name),
                stack_size: <Self as ::wavedb_core::Wire>::STACK_SIZE,
                fixed: <Self as ::wavedb_core::Wire>::FIXED,
                fields: &[ #(#entries),* ],
                heap_fields: &[ #(#heap_names),* ],
            };
    }
}
