use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_migration_chain(
    name: &Ident,
    args: &WaveDbArgs,
) -> proc_macro2::TokenStream {
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
    quote! {
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
    }
}
