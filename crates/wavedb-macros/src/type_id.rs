use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_type_id_impl(
    name: &Ident,
    args: &WaveDbArgs,
) -> (syn::Ident, proc_macro2::TokenStream) {
    let type_id_name = syn::Ident::new(&format!("{}TypeId", name), name.span());

    // Return type and body differ by shape.
    let type_id_impl = if args.nested_non_unique || args.non_unique {
        quote! {
            /// Zero-sized search-handle for [`#name`].
            ///
            /// Obtain via [`#name::TYPE_ID`] and call [`.get`](`#type_id_name::get`) to
            /// fetch all live records of this type for the current tenant.
            #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
            pub struct #type_id_name;

            impl #type_id_name {
                /// Fetch all live [`#name`] records for the current tenant.
                ///
                /// Equivalent to `#name::query(&db, Expr::all()).await`.
                pub async fn get(
                    self,
                    db: &::wavedb::Db,
                ) -> ::wavedb_core::Result<::std::vec::Vec<#name>> {
                    ::wavedb::object::do_query_non_unique::<#name>(
                        db,
                        ::wavedb::query::Expr::all(),
                    )
                    .await
                }
            }
        }
    } else {
        quote! {
            /// Zero-sized search-handle for [`#name`].
            ///
            /// Obtain via [`#name::TYPE_ID`] and call [`.get`](`#type_id_name::get`) to
            /// look up the single live record of this type for the current tenant.
            #[derive(::core::fmt::Debug, ::core::clone::Clone, ::core::marker::Copy)]
            pub struct #type_id_name;

            impl #type_id_name {
                /// Look up the live [`#name`] record for the current tenant.
                ///
                /// Returns `None` when no record has been written yet.
                /// Equivalent to `#name::search(&db).await`.
                pub async fn get(
                    self,
                    db: &::wavedb::Db,
                ) -> ::wavedb_core::Result<::core::option::Option<#name>> {
                    ::wavedb::object::do_search_unique::<#name>(db).await
                }
            }
        }
    };

    (type_id_name, type_id_impl)
}
