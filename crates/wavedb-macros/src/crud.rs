use crate::args::WaveDbArgs;
use quote::quote;
use syn::Ident;

pub fn build_crud_impl(name: &Ident, args: &WaveDbArgs) -> proc_macro2::TokenStream {
    if args.no_auto_crud {
        quote! {}
    } else if args.nested_non_unique || args.non_unique {
        quote! {
            impl ::wavedb::object::NonUniqueObject for #name {
                fn query(
                    db: &::wavedb::Db,
                    expr: ::wavedb::query::Expr,
                ) -> impl ::core::future::Future<
                    Output = ::wavedb_core::Result<::std::vec::Vec<Self>>,
                > + ::core::marker::Send {
                    ::wavedb::object::do_query_non_unique::<Self>(db, expr)
                }

                fn update(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_write(db, &self).await }
                }

                fn delete(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_delete(db, self.id.raw()).await }
                }
            }
        }
    } else {
        // Unique shape (default).
        quote! {
            impl ::wavedb::object::UniqueObject for #name {
                fn search(
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<
                    Output = ::wavedb_core::Result<::core::option::Option<Self>>,
                > + ::core::marker::Send {
                    ::wavedb::object::do_search_unique::<Self>(db)
                }

                fn update(
                    self,
                    db: &::wavedb::Db,
                ) -> impl ::core::future::Future<Output = ::wavedb_core::Result<()>>
                       + ::core::marker::Send {
                    async move { ::wavedb::object::do_write(db, &self).await }
                }
            }
        }
    }
}
