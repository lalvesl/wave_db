use crate::args::WaveDbArgs;
use crate::utils::collect_derived_traits;
use quote::{format_ident, quote};
use syn::{Attribute, Ident};

pub fn build_shape(args: &WaveDbArgs) -> proc_macro2::TokenStream {
    if args.nested_non_unique {
        quote! { ::wavedb_core::Shape::NestedNonUnique }
    } else if args.non_unique {
        quote! { ::wavedb_core::Shape::NonUnique }
    } else {
        quote! { ::wavedb_core::Shape::Unique }
    }
}

/// Generate the `FooId` and `FooAnchor` typed wrapper types for a struct.
///
/// `FooId` wraps a raw `Id` with a struct-specific phantom type so cross-references
/// between unrelated record families are caught at compile time.
/// `FooAnchor` enforces the anchor-key invariant (`CREATED_AT = 0`) via its
/// `From<Id>` implementation, which always calls `anchor_key()`.
pub fn build_typed_wrappers(
    name: &Ident,
    args: &WaveDbArgs,
    vis: &syn::Visibility,
) -> proc_macro2::TokenStream {
    let id_name = format_ident!("{}Id", name);
    let anchor_name = format_ident!("{}Anchor", name);

    // `get` for a typed ID: find the record whose raw `id` equals the stored ID.
    // For Unique records there is only one per tenant, so `search` is used instead.
    let get_by_id = if args.non_unique || args.nested_non_unique {
        quote! {
            /// Fetch the record whose `id` matches this typed ID.
            ///
            /// Queries all live records and filters client-side (the transport
            /// layer has no `Id`-field filter expression yet).  Returns `None`
            /// if the record has been deleted or never written.
            pub async fn get(
                self,
                db: &::wavedb::Db,
            ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                let records = <#name as ::wavedb::object::NonUniqueObject>::query(
                    db,
                    ::wavedb::query::Expr::all(),
                )
                .await?;
                ::core::result::Result::Ok(records.into_iter().find(|r| r.id == self.0))
            }
        }
    } else {
        quote! {
            /// Fetch the single live record for the current tenant.
            ///
            /// For `Unique` records the ID is implicit; `self` is not used in the
            /// lookup.
            pub async fn get(
                self,
                db: &::wavedb::Db,
            ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                <#name as ::wavedb::object::UniqueObject>::search(db).await
            }
        }
    };

    // `get` for a typed Anchor: find the record whose `anchor_key()` matches.
    // This is stable across record mutations — the anchor always resolves to the
    // current live version without a history walk.
    let get_by_anchor = if args.non_unique || args.nested_non_unique {
        quote! {
            /// Fetch the live record at this anchor address.
            ///
            /// Because the anchor key zeroes `CREATED_AT`, this resolves to the
            /// current live version even if the record has been mutated or replaced
            /// since the anchor was stored.  Returns `None` if the record has been
            /// tombstoned.
            pub async fn get(
                self,
                db: &::wavedb::Db,
            ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                let records = <#name as ::wavedb::object::NonUniqueObject>::query(
                    db,
                    ::wavedb::query::Expr::all(),
                )
                .await?;
                ::core::result::Result::Ok(
                    records.into_iter().find(|r| r.id.anchor_key() == self.0),
                )
            }
        }
    } else {
        quote! {
            /// Fetch the single live record for the current tenant.
            pub async fn get(
                self,
                db: &::wavedb::Db,
            ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                <#name as ::wavedb::object::UniqueObject>::search(db).await
            }
        }
    };

    quote! {
        /// Typed `Id` wrapper for [`#name`] records.
        ///
        /// Prevents mixing identifiers from unrelated record families at
        /// compile time.  Convert from a raw [`::wavedb_core::Id`] with `into()` or
        /// [`From`]; convert to an anchor key with [`.anchor()`](Self::anchor).
        #[derive(
            ::core::fmt::Debug,
            ::core::clone::Clone,
            ::core::marker::Copy,
            ::core::cmp::PartialEq,
            ::core::cmp::Eq,
            ::core::hash::Hash,
            ::core::default::Default,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(transparent)]
        #vis struct #id_name(pub ::wavedb_core::Id);

        impl #id_name {
            /// The zero sentinel — "not set yet".
            pub const ZERO: Self = Self(::wavedb_core::Id::ZERO);

            /// Convert to a [`#anchor_name`] by zeroing the `CREATED_AT` field.
            pub fn anchor(self) -> #anchor_name {
                #anchor_name(self.0.anchor_key())
            }

            /// Return the inner raw [`::wavedb_core::Id`].
            pub fn inner(self) -> ::wavedb_core::Id {
                self.0
            }

            /// Return the raw 128-bit representation.
            pub fn raw(self) -> u128 {
                self.0.raw()
            }

            #get_by_id
        }

        impl ::core::convert::From<::wavedb_core::Id> for #id_name {
            fn from(id: ::wavedb_core::Id) -> Self {
                Self(id)
            }
        }

        impl ::core::convert::From<#id_name> for ::wavedb_core::Id {
            fn from(t: #id_name) -> Self {
                t.0
            }
        }

        /// Typed anchor-key wrapper for [`#name`] records.
        ///
        /// The inner `Id` always has `CREATED_AT = 0` (enforced by the
        /// `From<Id>` and `From<#id_name>` impls, which call `anchor_key()`).
        /// Use this type for cross-reference fields so references survive any
        /// mutation of the target record without a history walk.
        #[derive(
            ::core::fmt::Debug,
            ::core::clone::Clone,
            ::core::marker::Copy,
            ::core::cmp::PartialEq,
            ::core::cmp::Eq,
            ::core::hash::Hash,
            ::core::default::Default,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[serde(transparent)]
        #vis struct #anchor_name(pub ::wavedb_core::Id);

        impl #anchor_name {
            /// The zero sentinel — "no anchor set".
            pub const ZERO: Self = Self(::wavedb_core::Id::ZERO);

            /// Return the inner raw [`::wavedb_core::Id`] (has `CREATED_AT = 0`).
            pub fn inner(self) -> ::wavedb_core::Id {
                self.0
            }

            /// Return the raw 128-bit representation.
            pub fn raw(self) -> u128 {
                self.0.raw()
            }

            #get_by_anchor
        }

        /// Converts a raw `Id` to an anchor key by calling `anchor_key()`.
        impl ::core::convert::From<::wavedb_core::Id> for #anchor_name {
            fn from(id: ::wavedb_core::Id) -> Self {
                Self(id.anchor_key())
            }
        }

        impl ::core::convert::From<#anchor_name> for ::wavedb_core::Id {
            fn from(t: #anchor_name) -> Self {
                t.0
            }
        }

        /// Converts a typed `Id` to an anchor key by calling `anchor_key()`.
        impl ::core::convert::From<#id_name> for #anchor_name {
            fn from(t: #id_name) -> Self {
                Self(t.0.anchor_key())
            }
        }
    }
}

pub fn build_anchors_impl(name: &Ident, args: &WaveDbArgs) -> proc_macro2::TokenStream {
    let id_name = format_ident!("{}Id", name);
    let anchor_name = format_ident!("{}Anchor", name);

    let typed_accessors = quote! {
        /// Return a typed [`#id_name`] wrapping this record's raw `id`.
        pub fn typed_id(&self) -> #id_name {
            #id_name(self.id)
        }

        /// Return a typed [`#anchor_name`] — the stable anchor key for this record.
        ///
        /// Equivalent to `#id_name(self.id).anchor()`.
        pub fn anchor(&self) -> #anchor_name {
            #anchor_name(self.id.anchor_key())
        }
    };

    let threshold_const = args.btree_threshold.map(|t| {
        quote! {
            /// The array→B+tree conversion threshold for this struct's indexes.
            pub const BTREE_THRESHOLD: u32 = #t;
        }
    });

    let primary_accessor = args.primary_anchor.as_ref().map(|field| {
        let find_fn_name = syn::Ident::new(&format!("find_by_{field}"), field.span());
        quote! {
            /// Look up a record by its primary anchor field.
            pub fn primary_anchor_field() -> &'static str {
                stringify!(#field)
            }
            #[doc(hidden)]
            pub fn __primary_anchor_field_name() -> &'static str {
                stringify!(#find_fn_name)
            }
        }
    });

    let secondary_field_lists: Vec<&String> = args.secondary_anchors.iter().collect();
    let secondary_anchors_const = if secondary_field_lists.is_empty() {
        quote! {}
    } else {
        quote! {
            /// The secondary anchor field lists for this struct.
            pub const SECONDARY_ANCHOR_FIELDS: &'static [&'static str] = &[
                #(#secondary_field_lists),*
            ];
        }
    };

    quote! {
        #typed_accessors
        #threshold_const
        #primary_accessor
        #secondary_anchors_const
    }
}

pub fn build_auto_derives(attrs: &[Attribute]) -> proc_macro2::TokenStream {
    let existing_derives = collect_derived_traits(attrs);
    let mut needed = Vec::new();
    if !existing_derives.contains("Debug") {
        needed.push(quote! { ::core::fmt::Debug });
    }
    if !existing_derives.contains("Clone") {
        needed.push(quote! { ::core::clone::Clone });
    }
    if !existing_derives.contains("Default") {
        needed.push(quote! { ::core::default::Default });
    }
    if !existing_derives.contains("Serialize") {
        needed.push(quote! { ::serde::Serialize });
    }
    if !existing_derives.contains("Deserialize") {
        needed.push(quote! { ::serde::Deserialize });
    }
    if needed.is_empty() {
        quote! {}
    } else {
        quote! { #[derive( #(#needed),* )] }
    }
}
