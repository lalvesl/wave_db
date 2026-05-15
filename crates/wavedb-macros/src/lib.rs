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

use proc_macro::TokenStream;
use quote::quote;
use syn::meta::ParseNestedMeta;
use syn::{ItemStruct, LitInt, LitStr, parse_macro_input};

/// Parsed arguments from the `#[wave_db(...)]` attribute.
struct WaveDbArgs {
    struct_id: u32,
    non_unique: bool,
    nested_non_unique: bool,
    primary_anchor: Option<syn::Ident>,
    secondary_anchors: Vec<String>,
    btree_threshold: Option<u32>,
    /// If set, skip the auto-generated `UniqueObject` / `NonUniqueObject` impl
    /// so the user can provide a custom one.
    no_auto_crud: bool,
    // ── Migration ────────────────────────────────────────────────────────────
    /// TYPE path of the older predecessor (e.g. `Message1`).
    migrate_from: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db, OldType) -> Result<Self>`
    migrate_from_with: Option<syn::Path>,
    /// TYPE path of the newer successor (e.g. `Message3`).
    migrate_rollback: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db, NewType) -> Result<Self>`
    migrate_rollback_with: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db) -> Result<Option<OldType>>` (before DB search)
    first_try: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db) -> Result<Option<Self>>` (after DB returns None)
    fallback_not_found: Option<syn::Path>,
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
            no_auto_crud: false,
            migrate_from: None,
            migrate_from_with: None,
            migrate_rollback: None,
            migrate_rollback_with: None,
            first_try: None,
            fallback_not_found: None,
        }
    }

    #[allow(clippy::needless_pass_by_value)]
    fn parse(&mut self, meta: ParseNestedMeta<'_>) -> syn::Result<()> {
        if meta.path.is_ident("struct_id") {
            let value = meta.value()?;
            let lit: LitInt = value.parse()?;
            self.struct_id = lit.base10_parse()?;
        } else if meta.path.is_ident("NonUnique") {
            self.non_unique = true;
        } else if meta.path.is_ident("NestedNonUnique") {
            self.nested_non_unique = true;
        } else if meta.path.is_ident("primary_anchor") {
            let value = meta.value()?;
            let ident: syn::Ident = value.parse()?;
            self.primary_anchor = Some(ident);
        } else if meta.path.is_ident("secondary_anchor") {
            let value = meta.value()?;
            let lit: LitStr = value.parse()?;
            let s = lit.value();
            if s.is_empty() {
                return Err(meta.error("secondary_anchor requires at least one field"));
            }
            self.secondary_anchors.push(s);
        } else if meta.path.is_ident("btree_threshold") {
            let value = meta.value()?;
            let lit: LitInt = value.parse()?;
            self.btree_threshold = Some(lit.base10_parse()?);
        } else if meta.path.is_ident("no_auto_crud") {
            self.no_auto_crud = true;
        // ── Migration attributes ──────────────────────────────────────────
        } else if meta.path.is_ident("migrate_from") {
            let value = meta.value()?;
            self.migrate_from = Some(value.parse()?);
        } else if meta.path.is_ident("migrate_from_with") {
            let value = meta.value()?;
            self.migrate_from_with = Some(value.parse()?);
        } else if meta.path.is_ident("migrate_rollback") {
            let value = meta.value()?;
            self.migrate_rollback = Some(value.parse()?);
        } else if meta.path.is_ident("migrate_rollback_with") {
            let value = meta.value()?;
            self.migrate_rollback_with = Some(value.parse()?);
        } else if meta.path.is_ident("first_try") {
            let value = meta.value()?;
            self.first_try = Some(value.parse()?);
        } else if meta.path.is_ident("fallback_not_found") {
            let value = meta.value()?;
            self.fallback_not_found = Some(value.parse()?);
        } else {
            return Err(meta.error("unrecognized wave_db attribute"));
        }
        Ok(())
    }
}

/// Collect the unqualified names of every trait the user has already derived.
///
/// Used to dedupe the auto-derives the macro adds (`Debug`, `Clone`,
/// `Serialize`, `Deserialize`) so a struct annotated with both `#[wave_db(…)]`
/// and a hand-rolled `#[derive(Debug)]` doesn't error on a duplicate impl.
fn collect_derived_traits(attrs: &[syn::Attribute]) -> std::collections::HashSet<String> {
    let mut out = std::collections::HashSet::new();
    for attr in attrs {
        if !attr.path().is_ident("derive") {
            continue;
        }
        if let Ok(list) = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
        ) {
            for path in list {
                if let Some(seg) = path.segments.last() {
                    out.insert(seg.ident.to_string());
                }
            }
        }
    }
    out
}

/// Parse the trailing integer from a struct name (e.g. `"Message42"` → `42`).
fn parse_trailing_version(name: &str, span: proc_macro2::Span) -> syn::Result<u8> {
    let digit_start = name
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);

    let digits = &name[digit_start..];
    if digits.is_empty() {
        return Err(syn::Error::new(
            span,
            format!("struct name `{name}` must end with a version number (e.g. `{name}1`)"),
        ));
    }

    digits.parse::<u8>().map_err(|_| {
        syn::Error::new(
            span,
            format!("trailing version `{digits}` in `{name}` does not fit in u8 (0..=255)"),
        )
    })
}

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
    // Compute which standard derives are missing and emit just those, so a
    // struct that already says `#[derive(Debug, Clone)]` doesn't trigger a
    // duplicate-impl error.
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
    // Generated unless `no_auto_crud` is set.  The struct must have an
    // `id: ::wavedb_core::Id` field for the NonUnique `delete` to work.
    let crud_impl = if args.no_auto_crud {
        quote! {}
    } else if args.nested_non_unique || args.non_unique {
        quote! {
            impl ::wavedb::object::NonUniqueObject for #name {
                fn query(
                    db: &::wavedb::Db,
                    expr: ::wavedb::query::Expr,
                ) -> impl ::core::future::Future<
                    Output = ::wavedb_core::Result<::std::vec::Vec<Self>>,
                > + ::core::marker::Send {
                    ::wavedb::object::do_query_non_unique::<Self>(db, expr)
                }

                fn update(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_write(db, &self).await }
                }

                fn delete(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_delete(db, self.id.raw()).await }
                }
            }
        }
    } else {
        // Unique shape (default).
        quote! {
            impl ::wavedb::object::UniqueObject for #name {
                fn search(
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<
                    Output = ::wavedb_core::Result<::core::option::Option<Self>>,
                > + ::core::marker::Send {
                    ::wavedb::object::do_search_unique::<Self>(db)
                }

                fn update(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_write(db, &self).await }
                }
            }
        }
    };

    // ── Migration code generation ─────────────────────────────────────────────

    let migration_impl = build_migration_impl(name, sid, version, &args);

    // ── Final expansion ──────────────────────────────────────────────────────

    // ── TypeId: zero-sized search-handle marker ──────────────────────────────
    //
    // `{Name}TypeId` is a unit struct that carries no data; its only purpose is
    // to give callers a typed handle for triggering a DB search for this struct.
    //
    //   Unique shape:             MyStruct1::TYPE_ID.get(&db).await? -> Option<MyStruct1>
    //   NonUnique / NestedNonUnique:  MyStruct1::TYPE_ID.get(&db).await? -> Vec<MyStruct1>
    let type_id_name = syn::Ident::new(&format!("{name}TypeId"), name.span());

    // Return type and body differ by shape.
    let type_id_impl = if args.nested_non_unique || args.non_unique {
        quote! {
            /// Zero-sized search-handle for [`#name`].
            ///
            /// Obtain via [`#name::TYPE_ID`] and call [`.get`](`#type_id_name::get`) to
            /// fetch all live records of this type for the current tenant.
            #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
            pub struct #type_id_name;

            impl #type_id_name {
                /// Fetch all live [`#name`] records for the current tenant.
                ///
                /// Equivalent to `#name::query(&db, Expr::all()).await`.
                pub async fn get(
                    self,
                    db: &::wavedb::Db,
                ) -> ::wavedb_core::Result<::std::vec::Vec<#name>> {
                    ::wavedb::object::do_query_non_unique::<#name>(
                        db,
                        ::wavedb::query::Expr::all(),
                    )
                    .await
                }
            }
        }
    } else {
        quote! {
            /// Zero-sized search-handle for [`#name`].
            ///
            /// Obtain via [`#name::TYPE_ID`] and call [`.get`](`#type_id_name::get`) to
            /// look up the single live record of this type for the current tenant.
            #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
            pub struct #type_id_name;

            impl #type_id_name {
                /// Look up the live [`#name`] record for the current tenant.
                ///
                /// Returns `None` when no record has been written yet.
                /// Equivalent to `#name::search(&db).await`.
                pub async fn get(
                    self,
                    db: &::wavedb::Db,
                ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                    ::wavedb::object::do_search_unique::<#name>(db).await
                }
            }
        }
    };

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

/// Build all migration-related `impl` blocks for a struct.
///
/// Generates:
/// - `MigratesFrom` trait impl if `migrate_from = OldType` is set.
/// - `RollbackFrom` trait impl if `migrate_rollback = NewType` is set.
/// - `register_migration(&mut MigrationRegistry)` — adds forward and/or
///   backward edges depending on which neighbours are declared.
/// - Typed async wrappers: `__wave_db_migrate_from`, `__wave_db_migrate_rollback`,
///   `__wave_db_first_try`, `__wave_db_fallback_not_found`.
fn build_migration_impl(
    name: &syn::Ident,
    sid: u32,
    version: u8,
    args: &WaveDbArgs,
) -> proc_macro2::TokenStream {
    // ── Trait impls ──────────────────────────────────────────────────────────
    let migrates_from_impl = args.migrate_from.as_ref().map(|old_type| {
        quote! {
            impl ::wavedb_core::MigratesFrom for #name {
                type Source = #old_type;
            }
        }
    });

    let rollback_from_impl = args.migrate_rollback.as_ref().map(|new_type| {
        quote! {
            impl ::wavedb_core::RollbackFrom for #name {
                type Future = #new_type;
            }
        }
    });

    // ── register_migration ───────────────────────────────────────────────────
    // Generated only when at least one neighbour is declared.
    let register_migration = if args.migrate_from.is_some() || args.migrate_rollback.is_some() {
        let forward_edge = args.migrate_from.as_ref().map(|old_type| {
            quote! {
                let from = ::wavedb_core::VersionRef::new(
                    <#old_type as ::wavedb_core::WaveDbStruct>::STRUCT_ID,
                    <#old_type as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                );
                registry.register_forward(from, self_ref);
            }
        });
        let rollback_edge = args.migrate_rollback.as_ref().map(|new_type| {
            quote! {
                let future = ::wavedb_core::VersionRef::new(
                    <#new_type as ::wavedb_core::WaveDbStruct>::STRUCT_ID,
                    <#new_type as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                );
                registry.register_rollback(future, self_ref);
            }
        });
        Some(quote! {
            impl #name {
                /// Register this struct's migration edges in the registry.
                ///
                /// - If `migrate_from = OldType` is declared, adds the **forward** edge
                ///   `OldType → Self` so the engine can upgrade old records to this version.
                /// - If `migrate_rollback = NewType` is declared, adds the **backward** edge
                ///   `NewType → Self` so the engine can roll back newer records to this version.
                ///
                /// Source / target `STRUCT_ID` / `STRUCT_VERSION` are pulled from the
                /// neighbour types' `WaveDbStruct` impls at compile time — no naming
                /// convention required.
                pub fn register_migration(registry: &mut ::wavedb_core::MigrationRegistry) {
                    let self_ref = ::wavedb_core::VersionRef::new(#sid, #version);
                    #forward_edge
                    #rollback_edge
                }
            }
        })
    } else {
        None
    };

    // ── migrate_from_with ────────────────────────────────────────────────────
    let migrate_from_with_impl = args
        .migrate_from_with
        .as_ref()
        .zip(args.migrate_from.as_ref())
        .map(|(fn_path, old_type)| {
            quote! {
                impl #name {
                    /// Upgrade an `OldType` record to `Self`.  Generic over `Db`
                    /// so it interops with any handle (`wavedb::Db`, mocks, etc.).
                    #[doc(hidden)]
                    pub async fn __wave_db_migrate_from<__WaveDbDb>(
                        db: &__WaveDbDb,
                        source: #old_type,
                    ) -> ::wavedb_core::Result<Self> {
                        #fn_path(db, source).await
                    }
                }
            }
        });

    // ── migrate_rollback_with ────────────────────────────────────────────────
    let rollback_with_impl = args
        .migrate_rollback_with
        .as_ref()
        .zip(args.migrate_rollback.as_ref())
        .map(|(fn_path, new_type)| {
            quote! {
                impl #name {
                    /// Receive a rollback from a future version and produce `Self`.
                    /// Generic over `Db` for transport flexibility.
                    #[doc(hidden)]
                    pub async fn __wave_db_migrate_rollback<__WaveDbDb>(
                        db: &__WaveDbDb,
                        future: #new_type,
                    ) -> ::wavedb_core::Result<Self> {
                        #fn_path(db, future).await
                    }
                }
            }
        });

    // ── first_try ────────────────────────────────────────────────────────────
    // Called BEFORE the DB search.  If Some(source), skip DB and run migration.
    let first_try_impl = args
        .first_try
        .as_ref()
        .zip(args.migrate_from.as_ref())
        .map(|(fn_path, old_type)| {
            quote! {
                impl #name {
                    /// Try to produce a source record before hitting the DB.
                    ///
                    /// If this returns `Some(source)`, the engine skips the normal DB
                    /// search and calls `__wave_db_migrate_from(db, source)` instead.
                    /// Return `None` to fall through to the normal DB path.
                    ///
                    /// Replaces the legacy Type-2 (compose) migration pattern: your
                    /// implementation looks up constituent source structs in the DB
                    /// and synthesises the `Source` value from them.
                    #[doc(hidden)]
                    pub async fn __wave_db_first_try<__WaveDbDb>(
                        db: &__WaveDbDb,
                    ) -> ::wavedb_core::Result<::core::option::Option<#old_type>> {
                        #fn_path(db).await
                    }
                }
            }
        });

    // ── fallback_not_found ───────────────────────────────────────────────────
    let fallback_impl = args.fallback_not_found.as_ref().map(|fn_path| {
        quote! {
            impl #name {
                /// Last-resort fallback when no record is found in the DB.
                ///
                /// Called after both the `first_try` hook (if any) and the normal
                /// DB search have returned `None`.  Return `Some(self_value)` to
                /// synthesise a default, or `None` to propagate the "not found".
                #[doc(hidden)]
                pub async fn __wave_db_fallback_not_found<__WaveDbDb>(
                    db: &__WaveDbDb,
                ) -> ::wavedb_core::Result<::core::option::Option<Self>> {
                    #fn_path(db).await
                }
            }
        }
    });

    // ── MigrationChain<Db> impl ──────────────────────────────────────────────
    // Generated on EVERY wave_db struct, regardless of migration attrs.  This
    // walks the chain (via `MigratesFrom`/`RollbackFrom`) to deserialize stored
    // bytes at any version into `Self`.  No manual `register_migration` needed —
    // the chain is the type system.
    let less_branch = match (&args.migrate_from, &args.migrate_from_with) {
        (Some(old_type), Some(_)) => quote! {
            let source: #old_type = <#old_type as ::wavedb_core::MigrationChain<__WaveDbDb>>::read_as_self(
                db, bytes, stored_version,
            ).await?;
            <Self>::__wave_db_migrate_from(db, source).await
        },
        _ => quote! {
            ::core::result::Result::Err(::wavedb_core::Error::Other(::std::format!(
                "no upgrade path: {} v{} cannot read stored v{}",
                ::core::stringify!(#name),
                <Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                stored_version,
            )))
        },
    };
    let greater_branch = match (&args.migrate_rollback, &args.migrate_rollback_with) {
        (Some(new_type), Some(_)) => quote! {
            let future: #new_type = <#new_type as ::wavedb_core::MigrationChain<__WaveDbDb>>::read_as_self(
                db, bytes, stored_version,
            ).await?;
            <Self>::__wave_db_migrate_rollback(db, future).await
        },
        _ => quote! {
            ::core::result::Result::Err(::wavedb_core::Error::Other(::std::format!(
                "no rollback path: {} v{} cannot read stored v{}",
                ::core::stringify!(#name),
                <Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                stored_version,
            )))
        },
    };
    let migration_chain_impl = quote! {
        impl<__WaveDbDb> ::wavedb_core::MigrationChain<__WaveDbDb> for #name
        where
            __WaveDbDb: ::core::marker::Send + ::core::marker::Sync,
        {
            fn read_as_self<'a>(
                db: &'a __WaveDbDb,
                bytes: &'a [u8],
                stored_version: u8,
            ) -> ::core::pin::Pin<
                ::std::boxed::Box<
                    dyn ::core::future::Future<Output = ::wavedb_core::Result<Self>>
                        + ::core::marker::Send
                        + 'a,
                >,
            >
            where
                __WaveDbDb: 'a,
            {
                ::std::boxed::Box::pin(async move {
                    match stored_version.cmp(&<Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION) {
                        ::core::cmp::Ordering::Equal => {
                            ::wavedb_core::migration::deserialize_for_migration::<Self>(bytes)
                        }
                        ::core::cmp::Ordering::Less => { #less_branch }
                        ::core::cmp::Ordering::Greater => { #greater_branch }
                    }
                })
            }
        }
    };

    quote! {
        #migrates_from_impl
        #rollback_from_impl
        #register_migration
        #migrate_from_with_impl
        #rollback_with_impl
        #first_try_impl
        #fallback_impl
        #migration_chain_impl
    }
}
