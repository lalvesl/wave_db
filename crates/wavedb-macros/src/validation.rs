use crate::args::WaveDbArgs;
use syn::Ident;

pub fn validate_struct_id(ident: &Ident, struct_id: u32) -> syn::Result<()> {
    if struct_id == u32::MAX {
        return Err(syn::Error::new_spanned(
            ident,
            "missing `struct_id = N` in #[wave_db]",
        ));
    }
    if struct_id >= (1 << 20) {
        return Err(syn::Error::new_spanned(
            ident,
            format!(
                "struct_id {} does not fit in u20 (max {})",
                struct_id,
                (1u32 << 20) - 1
            ),
        ));
    }
    Ok(())
}

pub fn validate_migration_attributes(
    ident: &Ident,
    args: &WaveDbArgs,
) -> syn::Result<()> {
    let from_required = [
        ("migrate_from_with", args.migrate_from_with.is_some()),
        ("first_try", args.first_try.is_some()),
    ];
    for (attr_name, present) in from_required {
        if present && args.migrate_from.is_none() {
            return Err(syn::Error::new_spanned(
                ident,
                format!(
                    "`{attr_name}` requires `migrate_from = OldType` to also be set"
                ),
            ));
        }
    }
    if args.migrate_rollback_with.is_some() && args.migrate_rollback.is_none() {
        return Err(syn::Error::new_spanned(
            ident,
            "`migrate_rollback_with` requires `migrate_rollback = NewType` to also be set",
        ));
    }
    Ok(())
}
