use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_migrate_from_with(
    name: &Ident,
    args: &WaveDbArgs,
) -> Option<proc_macro2::TokenStream> {
    args.migrate_from_with
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
                    ) -> ::wavedb_core::Result<Self>
                    where
                        __WaveDbDb: ::core::marker::Send + ::core::marker::Sync,
                    {
                        #fn_path(db, source).await
                    }
                }
            }
        })
}

pub fn build_rollback_with(name: &Ident, args: &WaveDbArgs) -> Option<proc_macro2::TokenStream> {
    args.migrate_rollback_with
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
                    ) -> ::wavedb_core::Result<Self>
                    where
                        __WaveDbDb: ::core::marker::Send + ::core::marker::Sync,
                    {
                        #fn_path(db, future).await
                    }
                }
            }
        })
}

pub fn build_first_try(name: &Ident, args: &WaveDbArgs) -> Option<proc_macro2::TokenStream> {
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
                    ) -> ::wavedb_core::Result<::core::option::Option<#old_type>>
                    where
                        __WaveDbDb: ::core::marker::Send + ::core::marker::Sync,
                    {
                        #fn_path(db).await
                    }
                }
            }
        })
}

pub fn build_fallback(name: &Ident, args: &WaveDbArgs) -> Option<proc_macro2::TokenStream> {
    args.fallback_not_found.as_ref().map(|fn_path| {
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
                ) -> ::wavedb_core::Result<::core::option::Option<Self>>
                where
                    __WaveDbDb: ::core::marker::Send + ::core::marker::Sync,
                {
                    #fn_path(db).await
                }
            }
        }
    })
}
