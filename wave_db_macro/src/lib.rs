use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{
    DeriveInput, Expr, ExprLit, Ident, Lit, MetaNameValue, Token,
    parse::{Parse, ParseStream},
    parse_macro_input,
};

// ─── Attribute argument parsing ───────────────────────────────────────────────

/// Parsed arguments from `#[wave_db(...)]`.
#[derive(Default)]
struct WaveDbArgs {
    /// True when `NonUnique` is present.
    non_unique: bool,
    /// Optional override of `MAX_NON_UNIQUE_ELEMENTS`.
    btree_threshold: Option<u32>,
    /// The struct/table ID.
    struct_id: Option<u16>,
    /// The current schema version of this struct.
    struct_version: Option<u16>,
}

impl Parse for WaveDbArgs {
    fn parse(input: ParseStream) -> syn::Result<Self> {
        let mut args = WaveDbArgs::default();

        while !input.is_empty() {
            // Try to parse `key = value` first.
            if input.peek(Ident) && input.peek2(Token![=]) {
                let kv: MetaNameValue = input.parse()?;
                let key = kv
                    .path
                    .get_ident()
                    .map(|i| i.to_string())
                    .unwrap_or_default();
                match key.as_str() {
                    "btree_threshold" => {
                        if let Expr::Lit(ExprLit {
                            lit: Lit::Int(n), ..
                        }) = &kv.value
                        {
                            args.btree_threshold = Some(n.base10_parse()?);
                        } else {
                            return Err(syn::Error::new_spanned(&kv.value, "expected integer"));
                        }
                    }
                    "struct_id" => {
                        if let Expr::Lit(ExprLit {
                            lit: Lit::Int(n), ..
                        }) = &kv.value
                        {
                            args.struct_id = Some(n.base10_parse()?);
                        } else {
                            return Err(syn::Error::new_spanned(&kv.value, "expected integer"));
                        }
                    }
                    "struct_version" => {
                        if let Expr::Lit(ExprLit {
                            lit: Lit::Int(n), ..
                        }) = &kv.value
                        {
                            args.struct_version = Some(n.base10_parse()?);
                        } else {
                            return Err(syn::Error::new_spanned(&kv.value, "expected integer"));
                        }
                    }
                    other => {
                        return Err(syn::Error::new_spanned(
                            &kv.path,
                            format!("unknown attribute key `{other}`"),
                        ));
                    }
                }
            } else {
                // Bare identifier, e.g. `NonUnique`.
                let ident: Ident = input.parse()?;
                match ident.to_string().as_str() {
                    "NonUnique" => args.non_unique = true,
                    other => {
                        return Err(syn::Error::new_spanned(
                            ident,
                            format!("unknown attribute `{other}`"),
                        ));
                    }
                }
            }

            // Consume optional trailing comma.
            if input.peek(Token![,]) {
                let _: Token![,] = input.parse()?;
            }
        }

        Ok(args)
    }
}

// ─── Main proc-macro ──────────────────────────────────────────────────────────

/// `#[wave_db]` / `#[wave_db(NonUnique)]` / `#[wave_db(NonUnique, btree_threshold = 100)]`
///
/// Annotates a struct so it satisfies the `WaveDbObject` trait and generates
/// the necessary bookkeeping (struct-id constant, `WaveDbKind` marker).
#[proc_macro_attribute]
pub fn wave_db(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as WaveDbArgs);
    let input = parse_macro_input!(item as DeriveInput);

    match expand_wave_db(args, input) {
        Ok(ts) => ts.into(),
        Err(e) => e.into_compile_error().into(),
    }
}

fn expand_wave_db(args: WaveDbArgs, input: DeriveInput) -> syn::Result<TokenStream2> {
    let struct_name = &input.ident;
    let vis = &input.vis;
    let attrs = &input.attrs;
    let generics = &input.generics;

    // We only handle structs for now.
    let fields = match &input.data {
        syn::Data::Struct(s) => &s.fields,
        _ => {
            return Err(syn::Error::new_spanned(
                struct_name,
                "#[wave_db] can only be applied to structs",
            ));
        }
    };

    // Validate that `id: Id` and `metadata: Metadata` are present.
    let has_id = fields
        .iter()
        .any(|f| f.ident.as_ref().map(|i| i == "id").unwrap_or(false));
    let has_metadata = fields
        .iter()
        .any(|f| f.ident.as_ref().map(|i| i == "metadata").unwrap_or(false));

    if !has_id {
        return Err(syn::Error::new_spanned(
            struct_name,
            "#[wave_db] structs must contain a field `id: Id`",
        ));
    }
    if !has_metadata {
        return Err(syn::Error::new_spanned(
            struct_name,
            "#[wave_db] structs must contain a field `metadata: Metadata`",
        ));
    }

    let struct_id = match args.struct_id {
        Some(id) => id,
        None => {
            return Err(syn::Error::new_spanned(
                struct_name,
                "#[wave_db] requires a `struct_id = N` attribute",
            ));
        }
    };

    let struct_version = args.struct_version.unwrap_or(1);

    // Build marker type name: `<StructName>Kind`
    let kind_name = format_ident!("{}Kind", struct_name);

    // NonUnique marker + btree_threshold constant.
    let non_unique_impl = if args.non_unique {
        let threshold = args.btree_threshold.unwrap_or(50u32);
        quote! {
            impl ::wave_db::NonUniqueObject for #struct_name {
                const BTREE_THRESHOLD: u32 = #threshold;
            }
        }
    } else {
        quote! {
            impl ::wave_db::UniqueObject for #struct_name {}
        }
    };

    let expanded = quote! {
        // Re-emit the original struct unchanged.
        #(#attrs)*
        #vis struct #struct_name #generics #fields

        // Zero-sized marker type for kind dispatch.
        #[derive(Debug, Clone, Copy)]
        #vis struct #kind_name;

        // Core trait impl.
        impl ::wave_db::WaveDbObject for #struct_name {
            type Kind = #kind_name;
            const STRUCT_ID: u16 = #struct_id;
            const STRUCT_VERSION: u16 = #struct_version;

            fn id(&self) -> &::wave_db::Id {
                &self.id
            }

            fn metadata(&self) -> &::wave_db::Metadata {
                &self.metadata
            }

            fn id_mut(&mut self) -> &mut ::wave_db::Id {
                &mut self.id
            }

            fn metadata_mut(&mut self) -> &mut ::wave_db::Metadata {
                &mut self.metadata
            }
        }

        // Unique / NonUnique marker.
        #non_unique_impl
    };

    Ok(expanded)
}
