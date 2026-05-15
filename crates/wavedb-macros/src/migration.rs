use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

/// Build all migration-related `impl` blocks for a struct.
///
/// Generates:
/// - `MigratesFrom` trait impl if `migrate_from = OldType` is set.
/// - `RollbackFrom` trait impl if `migrate_rollback = NewType` is set.
/// - `register_migration(&mut MigrationRegistry)` — adds forward and/or
///   backward edges depending on which neighbours are declared.
/// - Typed async wrappers: `__wave_db_migrate_from`, `__wave_db_migrate_rollback`,
///   `__wave_db_first_try`, `__wave_db_fallback_not_found`.
pub fn build_migration_impl(
    name: &Ident,
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
    let first_try_impl =
        args.first_try
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
    let less_branch = if let (Some(old_type), Some(_)) =
        (&args.migrate_from, &args.migrate_from_with)
    {
        quote! {
            let source: #old_type = <#old_type as ::wavedb_core::MigrationChain<__WaveDbDb>>::read_as_self(
                db, bytes, stored_version,
            ).await?;
            <Self>::__wave_db_migrate_from(db, source).await
        }
    } else {
        quote! {
            ::core::result::Result::Err(::wavedb_core::Error::Other(::std::format!(
                "no upgrade path: {} v{} cannot read stored v{}",
                ::core::stringify!(#name),
                <Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                stored_version,
            )))
        }
    };
    let greater_branch = if let (Some(new_type), Some(_)) =
        (&args.migrate_rollback, &args.migrate_rollback_with)
    {
        quote! {
            let future: #new_type = <#new_type as ::wavedb_core::MigrationChain<__WaveDbDb>>::read_as_self(
                db, bytes, stored_version,
            ).await?;
            <Self>::__wave_db_migrate_rollback(db, future).await
        }
    } else {
        quote! {
            ::core::result::Result::Err(::wavedb_core::Error::Other(::std::format!(
                "no rollback path: {} v{} cannot read stored v{}",
                ::core::stringify!(#name),
                <Self as ::wavedb_core::WaveDbStruct>::STRUCT_VERSION,
                stored_version,
            )))
        }
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
