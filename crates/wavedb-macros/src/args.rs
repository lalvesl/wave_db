use syn::meta::ParseNestedMeta;
use syn::{LitInt, LitStr};

/// Parsed arguments from the `#[wave_db(...)]` attribute.
pub struct WaveDbArgs {
    pub struct_id: u32,
    pub non_unique: bool,
    pub nested_non_unique: bool,
    pub primary_anchor: Option<syn::Ident>,
    pub secondary_anchors: Vec<String>,
    pub btree_threshold: Option<u32>,
    /// If set, skip the auto-generated `UniqueObject` / `NonUniqueObject` impl
    /// so the user can provide a custom one.
    pub no_auto_crud: bool,
    // ── Migration ────────────────────────────────────────────────────────────
    /// TYPE path of the older predecessor (e.g. `Message1`).
    pub migrate_from: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db, OldType) -> Result<Self>`
    pub migrate_from_with: Option<syn::Path>,
    /// TYPE path of the newer successor (e.g. `Message3`).
    pub migrate_rollback: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db, NewType) -> Result<Self>`
    pub migrate_rollback_with: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db) -> Result<Option<OldType>>` (before DB search)
    pub first_try: Option<syn::Path>,
    /// Async fn: `async fn<Db>(&Db) -> Result<Option<Self>>` (after DB returns None)
    pub fallback_not_found: Option<syn::Path>,
}

impl WaveDbArgs {
    pub const fn new() -> Self {
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
    pub fn parse(&mut self, meta: ParseNestedMeta<'_>) -> syn::Result<()> {
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
