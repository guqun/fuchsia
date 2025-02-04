// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    fidl_fuchsia_io as fio, fidl_fuchsia_pkg::PackageResolverProxy,
    fuchsia_url::AbsolutePackageUrl, futures::prelude::*, thiserror::Error,
    update_package::UpdatePackage,
};

const CONCURRENT_PACKAGE_RESOLVES: usize = 5;

/// Error encountered while resolving a package.
#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("fidl error while resolving {1}")]
    Fidl(#[source] fidl::Error, AbsolutePackageUrl),

    #[error("error while resolving {1}")]
    Error(#[source] fidl_fuchsia_pkg_ext::ResolveError, AbsolutePackageUrl),

    #[error("while creating fidl proxy and stream")]
    CreateProxy(#[source] fidl::Error),
}

/// Resolves the update package given by `url` through the pkg_resolver.
pub(super) async fn resolve_update_package(
    pkg_resolver: &PackageResolverProxy,
    url: &AbsolutePackageUrl,
) -> Result<UpdatePackage, ResolveError> {
    let dir = resolve_package(pkg_resolver, &url).await?;
    Ok(UpdatePackage::new(dir))
}

/// Resolves each package URL through the package resolver with some concurrency, yielding results
/// of the resolved package directories. The output order is not guaranteed to match the input
/// order.
pub(super) fn resolve_packages<'a, I>(
    pkg_resolver: &'a PackageResolverProxy,
    urls: I,
) -> impl Stream<Item = Result<fio::DirectoryProxy, ResolveError>> + 'a
where
    I: 'a + Iterator<Item = &'a AbsolutePackageUrl>,
{
    stream::iter(urls)
        .map(move |url| resolve_package(pkg_resolver, url))
        .buffer_unordered(CONCURRENT_PACKAGE_RESOLVES)
}

async fn resolve_package(
    pkg_resolver: &PackageResolverProxy,
    url: &AbsolutePackageUrl,
) -> Result<fio::DirectoryProxy, ResolveError> {
    let (dir, dir_server_end) =
        fidl::endpoints::create_proxy().map_err(ResolveError::CreateProxy)?;
    let res = pkg_resolver.resolve(&url.to_string(), dir_server_end);
    let res = res.await.map_err(|e| ResolveError::Fidl(e, url.clone()))?;

    let () = res.map_err(|raw| ResolveError::Error(raw.into(), url.clone()))?;

    Ok(dir)
}
