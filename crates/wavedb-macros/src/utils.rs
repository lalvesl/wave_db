/// Collect the unqualified names of every trait the user has already derived.
///
/// Used to dedupe the auto-derives the macro adds (`Debug`, `Clone`,
/// `Serialize`, `Deserialize`) so a struct annotated with both `#[wave_db(…)]`
/// and a hand-rolled `#[derive(Debug)]` doesn't error on a duplicate impl.
pub fn collect_derived_traits(
    attrs: &[syn::Attribute],
) -> std::collections::HashSet<String> {
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

/// Whether a field type is **heapable** (variable-length): `String`,
/// `Vec<T>`, or an `Option` wrapping either.  Everything else — integers,
/// enums, IDs, fixed arrays — is **stackable** (fixed-width).
///
/// The classification is syntactic (last path segment), which matches how
/// the engine treats the bytes: a type alias that hides a `String` will be
/// misclassified, and that's accepted for v1 — the descriptor is a routing
/// hint, not a safety boundary.
pub fn is_heapable_type(ty: &syn::Type) -> bool {
    let syn::Type::Path(p) = ty else {
        return false;
    };
    let Some(seg) = p.path.segments.last() else {
        return false;
    };
    match seg.ident.to_string().as_str() {
        "String" | "Vec" => true,
        "Option" => match &seg.arguments {
            syn::PathArguments::AngleBracketed(args) => {
                args.args.iter().any(|a| {
                    matches!(
                        a,
                        syn::GenericArgument::Type(t) if is_heapable_type(t)
                    )
                })
            }
            _ => false,
        },
        _ => false,
    }
}

/// Parse the trailing integer from a struct name (e.g. `"Message42"` → `42`).
pub fn parse_trailing_version(
    name: &str,
    span: proc_macro2::Span,
) -> syn::Result<u8> {
    let digit_start = name
        .rfind(|c: char| !c.is_ascii_digit())
        .map_or(0, |i| i + 1);

    let digits = &name[digit_start..];
    if digits.is_empty() {
        return Err(syn::Error::new(
            span,
            format!(
                "struct name `{name}` must end with a version number (e.g. `{name}1`)"
            ),
        ));
    }

    digits.parse::<u8>().map_err(|_| {
        syn::Error::new(
            span,
            format!("trailing version `{digits}` in `{name}` does not fit in u8 (0..=255)"),
        )
    })
}
