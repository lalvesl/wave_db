use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_migrates_from(name: &Ident, args: &WaveDbArgs) -> Option<proc_macro2::TokenStream> {
    args.migrate_from.as_ref().map(|old_type| {
        quote! {
            impl ::wavedb_core::MigratesFrom for #name {
                type Source = #old_type;
            }
        }
    })
}

pub fn build_rollback_from(name: &Ident, args: &WaveDbArgs) -> Option<proc_macro2::TokenStream> {
    args.migrate_rollback.as_ref().map(|new_type| {
        quote! {
            impl ::wavedb_core::RollbackFrom for #name {
                type Future = #new_type;
            }
        }
    })
}
