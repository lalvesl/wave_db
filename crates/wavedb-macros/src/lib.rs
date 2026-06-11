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
mod declare;
mod descriptor;
mod migration;
mod utils;
mod validation;
mod wire_derive;

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

/// Derive `wavedb_core::Wire` — WaveDB's own serialization (see
/// `docs/wire_format.md`).
///
/// Structs flatten field stack slots in declaration order and append heap
/// payloads in the same order; `STACK_SIZE` is a compile-time constant.
/// Field-less enums encode as a single tag byte; enums with payload variants
/// encode as `[tag u8][payload len u32]` in the stack section plus a
/// self-contained `[stack][heap]` unit in the heap section.
#[proc_macro_derive(WaveWire)]
pub fn derive_wave_wire(item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as syn::DeriveInput);
    wire_derive::expand(&input)
        .unwrap_or_else(syn::Error::into_compile_error)
        .into()
}

/// Build the static object registry every node compiles in at startup.
///
/// ```rust,ignore
/// declare_objects! {
///     pub mod app_objects {
///         orders:   [Order1, Order2],
///         profiles: [UserProfile1],
///     }
/// }
/// ```
///
/// Generates a module with one submodule per struct family (`STRUCT_ID`,
/// `VERSIONS`, per-family `DESCRIPTORS`), a global `DESCRIPTORS` slice, and
/// `find(header)` — a constant-comparison-chain lookup keyed on the `u32`
/// record header (`struct_id << 8 | version`). Duplicate headers and mixed
/// `struct_id`s within a family are compile errors.
#[proc_macro]
pub fn declare_objects(item: TokenStream) -> TokenStream {
    let decl = parse_macro_input!(item as declare::DeclareObjects);
    declare::expand(&decl).into()
}

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
    // Heapable (variable-length) field names — the stackable/heapable
    // descriptor emitted as `WaveDbStruct::HEAPABLE_FIELDS`.
    let mut heapable_field_names: Vec<String> = Vec::new();

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
                if utils::is_heapable_type(&f.ty) {
                    heapable_field_names.push(ident.to_string());
                }
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

    // ── Wire impl (own serialization — replaces serde/postcard) ─────────────
    // Generated directly (not via `#[derive(WaveWire)]`) so consumer crates
    // don't need a direct wavedb-macros dependency for the derive path.
    // `input` already carries the injected `id` / `metadata` fields here.
    let wire_impl = wire_derive::expand_item_struct(&input);

    // ── Object descriptor (static registry metadata) ─────────────────────────
    let descriptor_const = match &input.fields {
        syn::Fields::Named(named) => {
            descriptor::build_descriptor(name, named, &shape)
        }
        _ => unreachable!("named fields validated above"),
    };

    // ── Final expansion ──────────────────────────────────────────────────────
    // Typed wrappers (`FooId`, `FooAnchor`) are emitted first so they are in
    // scope for any code in the same module that references them.
    let expanded = quote! {
        #typed_wrappers
        #fields_accessor

        #auto_derives
        #input

        #wire_impl

        impl ::wavedb_core::WaveDbStruct for #name {
            const STRUCT_ID: u32 = #sid;
            const STRUCT_VERSION: u8 = #version;
            const SHAPE: ::wavedb_core::Shape = #shape;
            const HEAPABLE_FIELDS: &'static [&'static str] =
                &[ #(#heapable_field_names),* ];
        }

        impl #name {
            #anchors_impl
            #fields_const
            #field_consts
            #descriptor_const
        }

        #crud_impl
        #migration_impl
    };

    expanded.into()
}
