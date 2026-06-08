use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

mod chain;
mod methods;
mod registry;
mod traits;

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
    let migrates_from_impl = traits::build_migrates_from(name, args);
    let rollback_from_impl = traits::build_rollback_from(name, args);
    let register_migration =
        registry::build_register_migration(name, sid, version, args);
    let migrate_from_with_impl = methods::build_migrate_from_with(name, args);
    let rollback_with_impl = methods::build_rollback_with(name, args);
    let first_try_impl = methods::build_first_try(name, args);
    let fallback_impl = methods::build_fallback(name, args);
    let migration_chain_impl = chain::build_migration_chain(name, args);

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
