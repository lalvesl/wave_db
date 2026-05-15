//! `WaveDB` proc-macros — the `#[wave_db]` attribute macro.
//!
//! # What the macro does at compile time
//!
//! 1. Implements `WaveDbStruct` — pins `STRUCT_ID`, `STRUCT_VERSION`, `SHAPE`.
//! 2. Validates `struct_id` fits in u20, struct name ends with a version digit.
//! 3. Accepts shape flags: `NonUnique`, `NestedNonUnique`.
//! 4. Accepts anchor attributes: `primary_anchor`, `secondary_anchor`, `btree_threshold`.
//! 5. Accepts migration attributes (see below).
//!
//! # Migration attributes
//!
//! Each versioned struct declares its **immediate neighbours** in the chain:
//!
//! | Attribute | Direction | Kind | Description |
//! |-----------|-----------|------|-------------|
//! | `migrate_from = OldType` | backward | **type** | The older predecessor struct. |
//! | `migrate_from_with = fn` | backward | async fn | `async fn<Db>(&Db, OldType) -> Result<Self>` |
//! | `migrate_rollback = NewType` | forward | **type** | The newer successor struct (this struct receives its rollback). |
//! | `migrate_rollback_with = fn` | forward | async fn | `async fn<Db>(&Db, NewType) -> Result<Self>` |
//! | `first_try = fn` | — | async fn | `async fn<Db>(&Db) -> Result<Option<OldType>>` — called **before** the DB search; if `Some`, skip DB and run forward migration instead. |
//! | `fallback_not_found = fn` | — | async fn | `async fn<Db>(&Db) -> Result<Option<Self>>` — called when not found in the DB. |
//!
//! # Chain bounds
//!
//! - **First (oldest) version**: has no `migrate_from`. May have `migrate_rollback` once a vN+1 exists.
//! - **Middle versions**: have both `migrate_from` (older) and `migrate_rollback` (newer).
//! - **Last (current) version**: has `migrate_from` but no `migrate_rollback` yet.
//!
//! The full chain is traversable at compile time via `MigratesFrom::Source` (backward) and
//! `RollbackFrom::Future` (forward) — the registry can be reconstructed entirely from types.
//!
//! # Attribute dependencies
//!
//! - `migrate_from_with`, `first_try`     → require `migrate_from`
//! - `migrate_rollback_with`              → requires `migrate_rollback`
//! - `fallback_not_found`                 → standalone

#![warn(missing_docs)]

mod args;
mod crud;
mod migration;
mod type_id;
mod utils;

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemStruct, parse_macro_input};

use args::WaveDbArgs;
use crud::build_crud_impl;
use migration::build_migration_impl;
use type_id::build_type_id_impl;
use utils::{collect_derived_traits, parse_trailing_version};

/// The `#[wave_db(struct_id = N, ...)]` attribute macro.
///
/// See the crate-level documentation for the full attribute reference.
#[proc_macro_attribute]
pub fn wave_db(attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);

    let mut args = WaveDbArgs::new();
    let attr_parser = syn::meta::parser(|meta| args.parse(meta));
    parse_macro_input!(attr with attr_parser);
    let () = ();

    // ── Validate struct_id ───────────────────────────────────────────────────

    if args.struct_id == u32::MAX {
        return syn::Error::new_spanned(&input.ident, "missing `struct_id = N` in #[wave_db]")
            .to_compile_error()
            .into();
    }
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

    // ── Parse version from name ──────────────────────────────────────────────

    let name = &input.ident;
    let name_str = name.to_string();
    let version = match parse_trailing_version(&name_str, name.span()) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    // ── Validate migration attribute combinations ────────────────────────────

    let from_required = [
        ("migrate_from_with", args.migrate_from_with.is_some()),
        ("first_try", args.first_try.is_some()),
    ];
    for (attr_name, present) in from_required {
        if present && args.migrate_from.is_none() {
            return syn::Error::new_spanned(
                &input.ident,
                format!("`{attr_name}` requires `migrate_from = OldType` to also be set"),
            )
            .to_compile_error()
            .into();
        }
    }
    if args.migrate_rollback_with.is_some() && args.migrate_rollback.is_none() {
        return syn::Error::new_spanned(
            &input.ident,
            "`migrate_rollback_with` requires `migrate_rollback = NewType` to also be set",
        )
        .to_compile_error()
        .into();
    }

    let sid = args.struct_id;

    // ── Shape ────────────────────────────────────────────────────────────────

    let shape = if args.nested_non_unique {
        quote! { ::wavedb_core::Shape::NestedNonUnique }
    } else if args.non_unique {
        quote! { ::wavedb_core::Shape::NonUnique }
    } else {
        quote! { ::wavedb_core::Shape::Unique }
    };

    // ── Anchor attributes ────────────────────────────────────────────────────

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

    // ── Auto-derives: Debug, Clone, Serialize, Deserialize ──────────────────
    let existing_derives = collect_derived_traits(&input.attrs);
    let mut needed = Vec::new();
    if !existing_derives.contains("Debug") {
        needed.push(quote! { ::core::fmt::Debug });
    }
    if !existing_derives.contains("Clone") {
        needed.push(quote! { ::core::clone::Clone });
    }
    if !existing_derives.contains("Serialize") {
        needed.push(quote! { ::serde::Serialize });
    }
    if !existing_derives.contains("Deserialize") {
        needed.push(quote! { ::serde::Deserialize });
    }
    let auto_derives = if needed.is_empty() {
        quote! {}
    } else {
        quote! { #[derive( #(#needed),* )] }
    };

    // ── Auto-impl UniqueObject / NonUniqueObject ────────────────────────────
    let crud_impl = build_crud_impl(name, &args);

    // ── Migration code generation ─────────────────────────────────────────────
    let migration_impl = build_migration_impl(name, sid, version, &args);

    // ── TypeId: zero-sized search-handle marker ──────────────────────────────
    let (type_id_name, type_id_impl) = build_type_id_impl(name, &args);

    // ── Final expansion ──────────────────────────────────────────────────────

    let expanded = quote! {
        #auto_derives
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

            /// A typed, zero-cost search handle for this struct.
            ///
            /// Use `Self::TYPE_ID.get(&db).await?` to search the database
            /// without needing to import the concrete `*TypeId` type.
            pub const TYPE_ID: #type_id_name = #type_id_name;
        }

        #type_id_impl
        #crud_impl
        #migration_impl
    };

    expanded.into()
}
