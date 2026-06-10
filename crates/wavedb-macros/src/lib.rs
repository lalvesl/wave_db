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
mod codegen;
mod crud;
mod migration;
mod utils;
mod validation;

use proc_macro::TokenStream;
use quote::quote;
use syn::{ItemStruct, parse_macro_input};

use args::WaveDbArgs;
use codegen::{
    build_anchors_impl, build_auto_derives, build_field_consts,
    build_fields_accessor, build_shape, build_typed_wrappers,
};
use crud::build_crud_impl;
use migration::build_migration_impl;
use quote::format_ident;
use utils::parse_trailing_version;
use validation::{validate_migration_attributes, validate_struct_id};

/// The `#[wave_db(struct_id = N, ...)]` attribute macro.
///
/// See the crate-level documentation for the full attribute reference.
#[proc_macro_attribute]
pub fn wave_db(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut input = parse_macro_input!(item as ItemStruct);

    let mut args = WaveDbArgs::new();
    let attr_parser = syn::meta::parser(|meta| args.parse(meta));
    parse_macro_input!(attr with attr_parser);

    // Extract name early — needed for typed ID field injection below.
    let name = &input.ident;
    let name_str = name.to_string();

    // Captured **before** `id`/`metadata` are injected — used to emit
    // `XxxFields` typed field handles for the query DSL.
    let mut user_field_idents: Vec<syn::Ident> = Vec::new();

    if let syn::Fields::Named(ref mut fields) = input.fields {
        for f in &fields.named {
            if let Some(ident) = &f.ident {
                if ident == "id" || ident == "metadata" {
                    return syn::Error::new_spanned(
                        f,
                        format!("field `{ident}` is automatically added by #[wave_db] and cannot be added manually"),
                    )
                    .to_compile_error()
                    .into();
                }
                user_field_idents.push(ident.clone());
            }
        }

        let id_field: syn::Field =
            syn::parse_quote! { pub id: ::wavedb_core::Id };
        let metadata_field: syn::Field =
            syn::parse_quote! { pub metadata: ::wavedb_core::Metadata };

        fields.named.insert(0, id_field);
        fields.named.insert(1, metadata_field);
    } else {
        return syn::Error::new_spanned(
            &input.ident,
            "#[wave_db] requires a struct with named fields",
        )
        .to_compile_error()
        .into();
    }

    // ── Validate struct_id ───────────────────────────────────────────────────
    if let Err(e) = validate_struct_id(&input.ident, args.struct_id) {
        return e.to_compile_error().into();
    }

    // `name` and `name_str` are already set above.
    let version = match parse_trailing_version(&name_str, name.span()) {
        Ok(v) => v,
        Err(e) => return e.to_compile_error().into(),
    };

    // ── Validate migration attribute combinations ────────────────────────────
    if let Err(e) = validate_migration_attributes(&input.ident, &args) {
        return e.to_compile_error().into();
    }

    let sid = args.struct_id;
    let shape = build_shape(&args);
    let typed_wrappers = build_typed_wrappers(name, &args, &input.vis);
    let anchors_impl = build_anchors_impl(name, &args);
    let auto_derives = build_auto_derives(&input.attrs);
    let fields_accessor =
        build_fields_accessor(name, &input.vis, &user_field_idents);
    let field_consts = build_field_consts(&user_field_idents);
    let fields_name = format_ident!("{}Fields", name);
    let fields_const = quote! {
        /// Typed field handles for the query DSL — see [`::wavedb::query::Field`].
        ///
        /// `MyStruct::FIELDS.field_name().gt(value)` produces an [`::wavedb::query::Expr`]
        /// with a compile-time-checked field name (typos become compile errors).
        pub const FIELDS: #fields_name = #fields_name;
    };

    // ── Auto-impl UniqueObject / NonUniqueObject ────────────────────────────
    let crud_impl = build_crud_impl(name, &args);

    // ── Migration code generation ─────────────────────────────────────────────
    let migration_impl = build_migration_impl(name, sid, version, &args);

    // ── Final expansion ──────────────────────────────────────────────────────
    // Typed wrappers (`FooId`, `FooAnchor`) are emitted first so they are in
    // scope for any code in the same module that references them.
    let expanded = quote! {
        #typed_wrappers
        #fields_accessor

        #auto_derives
        #input

        impl ::wavedb_core::WaveDbStruct for #name {
            const STRUCT_ID: u32 = #sid;
            const STRUCT_VERSION: u8 = #version;
            const SHAPE: ::wavedb_core::Shape = #shape;
        }

        impl #name {
            #anchors_impl
            #fields_const
            #field_consts
        }

        #crud_impl
        #migration_impl
    };

    expanded.into()
}
