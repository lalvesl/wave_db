use crate::args::WaveDbArgs;
use crate::utils::collect_derived_traits;
use quote::quote;
use syn::Attribute;

pub fn build_shape(args: &WaveDbArgs) -> proc_macro2::TokenStream {
    if args.nested_non_unique {
        quote! { ::wavedb_core::Shape::NestedNonUnique }
    } else if args.non_unique {
        quote! { ::wavedb_core::Shape::NonUnique }
    } else {
        quote! { ::wavedb_core::Shape::Unique }
    }
}

pub fn build_anchors_impl(args: &WaveDbArgs) -> proc_macro2::TokenStream {
    let threshold_const = args.btree_threshold.map(|t| {
        quote! {
            /// The array→B+tree conversion threshold for this struct's indexes.
            pub const BTREE_THRESHOLD: u32 = #t;
        }
    });

    let primary_accessor = args.primary_anchor.as_ref().map(|field| {
        let find_fn_name = syn::Ident::new(&format!("find_by_{field}"), field.span());
        quote! {
            /// Look up a record by its primary anchor field.
            pub fn primary_anchor_field() -> &'static str {
                stringify!(#field)
            }
            #[doc(hidden)]
            pub fn __primary_anchor_field_name() -> &'static str {
                stringify!(#find_fn_name)
            }
        }
    });

    let secondary_field_lists: Vec<&String> = args.secondary_anchors.iter().collect();
    let secondary_anchors_const = if secondary_field_lists.is_empty() {
        quote! {}
    } else {
        quote! {
            /// The secondary anchor field lists for this struct.
            pub const SECONDARY_ANCHOR_FIELDS: &'static [&'static str] = &[
                #(#secondary_field_lists),*
            ];
        }
    };

    quote! {
        #threshold_const
        #primary_accessor
        #secondary_anchors_const
    }
}

pub fn build_auto_derives(attrs: &[Attribute]) -> proc_macro2::TokenStream {
    let existing_derives = collect_derived_traits(attrs);
    let mut needed = Vec::new();
    if !existing_derives.contains("Debug") {
        needed.push(quote! { ::core::fmt::Debug });
    }
    if !existing_derives.contains("Clone") {
        needed.push(quote! { ::core::clone::Clone });
    }
    if !existing_derives.contains("Default") {
        needed.push(quote! { ::core::default::Default });
    }
    if !existing_derives.contains("Serialize") {
        needed.push(quote! { ::serde::Serialize });
    }
    if !existing_derives.contains("Deserialize") {
        needed.push(quote! { ::serde::Deserialize });
    }
    if needed.is_empty() {
        quote! {}
    } else {
        quote! { #[derive( #(#needed),* )] }
    }
}
