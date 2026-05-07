//! `WaveDB` proc-macros — the `#[wave_db]` attribute macro.
//!
//! The macro does five jobs at compile time:
//! 1. Implements `WaveDbStruct` with `STRUCT_ID`, `STRUCT_VERSION`, and `SHAPE`.
//! 2. Validates `struct_id` fits in u20.
//! 3. Parses the trailing integer of the struct name as `struct_version` (u8).
//! 4. Accepts flags: `NonUnique`, `NestedNonUnique`.
//! 5. Accepts (optional) `primary_anchor` and `secondary_anchor` attributes.

use proc_macro::TokenStream;
use quote::quote;
use syn::meta::ParseNestedMeta;
use syn::{parse_macro_input, ItemStruct, LitInt, LitStr};

/// Parsed arguments from the `#[wave_db(...)]` attribute.
struct WaveDbArgs {
    struct_id: u32,
    non_unique: bool,
    nested_non_unique: bool,
    primary_anchor: Option<syn::Ident>,
    secondary_anchors: Vec<String>,
    btree_threshold: Option<u32>,
}

impl WaveDbArgs {
    const fn new() -> Self {
        Self {
            struct_id: u32::MAX,
            non_unique: false,
            nested_non_unique: false,
            primary_anchor: None,
            secondary_anchors: Vec::new(),
            btree_threshold: None,
        }
    }

    fn parse(&mut self, meta: ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("struct_id") {
            let value = meta.value()?;
            let lit: LitInt = value.parse()?;
            self.struct_id = lit.base10_parse()?;
            Ok(())
        } else if meta.path.is_ident("NonUnique") {
            self.non_unique = true;
            Ok(())
        } else if meta.path.is_ident("NestedNonUnique") {
            self.nested_non_unique = true;
            Ok(())
        } else if meta.path.is_ident("primary_anchor") {
            let value = meta.value()?;
            let ident: syn::Ident = value.parse()?;
            self.primary_anchor = Some(ident);
            Ok(())
        } else if meta.path.is_ident("secondary_anchor") {
            // Parse secondary_anchor = "field1,field2" as a string literal
            let value = meta.value()?;
            let lit: LitStr = value.parse()?;
            let s = lit.value();
            if s.is_empty() {
                return Err(meta.error("secondary_anchor requires at least one field"));
            }
            self.secondary_anchors.push(s);
            Ok(())
        } else if meta.path.is_ident("btree_threshold") {
            let value = meta.value()?;
            let lit: LitInt = value.parse()?;
            self.btree_threshold = Some(lit.base10_parse()?);
            Ok(())
        } else {
            Err(meta.error("unrecognized wave_db attribute"))
        }
    }
}

/// Parse the trailing integer from a struct name (e.g. "Message42" → 42).
fn parse_trailing_version(name: &str, span: proc_macro2::Span) -> syn::Result<u8> {
    let digit_start = name
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);

    let digits = &name[digit_start..];
    if digits.is_empty() {
        return Err(syn::Error::new(
            span,
            format!(
                "struct name `{name}` must end with a version number (e.g. `{name}1`)"
            ),
        ));
    }

    digits.parse::<u8>().map_err(|_| {
        syn::Error::new(
            span,
            format!(
                "trailing version `{digits}` in `{name}` does not fit in u8 (0..=255)"
            ),
        )
    })
}

/// The `#[wave_db(struct_id = N, ...)]` attribute macro.
///
/// Implements `WaveDbStruct` for the annotated struct and optionally generates
/// anchor accessor methods.
///
/// # Attributes
///
/// - `struct_id = N` — **required**. The permanent struct family ID (u20).
/// - `NonUnique` — marks the struct as having many records per tenant.
/// - `NestedNonUnique` — marks the struct as a child of a `NonUnique` parent.
/// - `primary_anchor = field` — hashes the given field into `SHARD_ID`.
/// - `secondary_anchor = "field1,field2"` — repeatable, registers alias anchors.
/// - `btree_threshold = K` — overrides the array→B+tree conversion threshold.
#[proc_macro_attribute]
pub fn wave_db(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);

    let mut args = WaveDbArgs::new();
    let attr_parser = syn::meta::parser(|meta| args.parse(meta));
    parse_macro_input!(attr with attr_parser);
    let () = ();

    // Validate struct_id was provided
    if args.struct_id == u32::MAX {
        return syn::Error::new_spanned(&input.ident, "missing `struct_id = N` in #[wave_db]")
            .to_compile_error()
            .into();
    }

    // Validate struct_id fits in u20
    if args.struct_id >= (1 << 20) {
        return syn::Error::new_spanned(
            &input.ident,
            format!(
                "struct_id {} does not fit in u20 (max {})",
                args.struct_id,
                (1u32 << 20) - 1
            ),
        )
        .to_compile_error()
        .into();
    }

    // Parse trailing version from struct name
    let name = &input.ident;
    let name_str = name.to_string();
    let version = match parse_trailing_version(&name_str, name.span()) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    let sid = args.struct_id;

    // Determine shape
    let shape = if args.nested_non_unique {
        quote! { ::wavedb_core::Shape::NestedNonUnique }
    } else if args.non_unique {
        quote! { ::wavedb_core::Shape::NonUnique }
    } else {
        quote! { ::wavedb_core::Shape::Unique }
    };

    // Generate btree_threshold constant if specified
    let threshold_const = args.btree_threshold.map(|t| {
        quote! {
            /// The array→B+tree conversion threshold for this struct's indexes.
            pub const BTREE_THRESHOLD: u32 = #t;
        }
    });

    // Generate primary_anchor accessor if specified
    let primary_accessor = args.primary_anchor.as_ref().map(|field| {
        let find_fn_name = syn::Ident::new(
            &format!("find_by_{field}"),
            field.span(),
        );
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

    // Build secondary anchor field list for runtime use
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

    let expanded = quote! {
        #input

        impl ::wavedb_core::WaveDbStruct for #name {
            const STRUCT_ID: u32 = #sid;
            const STRUCT_VERSION: u8 = #version;
            const SHAPE: ::wavedb_core::Shape = #shape;
        }

        impl #name {
            #threshold_const
            #primary_accessor
            #secondary_anchors_const
        }
    };

    expanded.into()
}

