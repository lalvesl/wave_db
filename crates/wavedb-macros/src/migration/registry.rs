use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_register_migration(
    name: &Ident,
    sid: u32,
    version: u8,
    args: &WaveDbArgs,
) -> Option<proc_macro2::TokenStream> {
    if args.migrate_from.is_none() && args.migrate_rollback.is_none() {
        return None;
    }

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
}
