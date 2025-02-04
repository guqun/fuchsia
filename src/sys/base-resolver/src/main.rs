// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {fidl_fuchsia_component_decl as fdecl, fidl_fuchsia_component_resolution as fresolution};

mod base_resolver;
mod pkg_cache_resolver;

#[fuchsia::main]
async fn main() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let args = args.iter().map(|s| s.as_str()).collect::<Vec<_>>();
    match args.as_slice() {
        ["/pkg/bin/base_resolver", "base_resolver"] => base_resolver::main().await,
        ["/pkg/bin/base_resolver", "pkg_cache_resolver"] => pkg_cache_resolver::main().await,
        _ => {
            log::error!("invalid args {:?}", args);
            anyhow::bail!("invalid args {:?}", args)
        }
    }
}

#[derive(thiserror::Error, Debug)]
enum ResolverError {
    #[error("invalid component URL")]
    InvalidUrl(#[from] fuchsia_url::errors::ParseError),

    #[error("component URL with package hash not supported")]
    PackageHashNotSupported,

    #[error("the hostname refers to an unsupported repo")]
    UnsupportedRepo,

    #[error("component not found")]
    ComponentNotFound(#[source] mem_util::FileError),

    #[error("package not found")]
    PackageNotFound(#[source] io_util::node::OpenError),

    #[error("couldn't parse component manifest")]
    ParsingManifest(#[source] fidl::Error),

    #[error("couldn't find config values")]
    ConfigValuesNotFound(#[source] mem_util::FileError),

    #[error("config source missing or invalid")]
    InvalidConfigSource,

    #[error("unsupported config source: {:?}", _0)]
    UnsupportedConfigSource(fdecl::ConfigValueSource),

    #[error("failed to read the manifest")]
    ReadManifest(#[source] mem_util::DataError),

    #[error("failed to create FIDL endpoints")]
    CreateEndpoints(#[source] fidl::Error),

    #[error("serve package directory")]
    ServePackageDirectory(#[source] package_directory::Error),
}

impl From<&ResolverError> for fresolution::ResolverError {
    fn from(err: &ResolverError) -> fresolution::ResolverError {
        use ResolverError::*;
        match err {
            InvalidUrl(_) | PackageHashNotSupported => fresolution::ResolverError::InvalidArgs,
            UnsupportedRepo => fresolution::ResolverError::NotSupported,
            ComponentNotFound(_) => fresolution::ResolverError::ManifestNotFound,
            PackageNotFound(_) => fresolution::ResolverError::PackageNotFound,
            ConfigValuesNotFound(_) => fresolution::ResolverError::ConfigValuesNotFound,
            ParsingManifest(_) | UnsupportedConfigSource(_) | InvalidConfigSource => {
                fresolution::ResolverError::InvalidManifest
            }
            ReadManifest(_) | CreateEndpoints(_) | ServePackageDirectory(_) => {
                fresolution::ResolverError::Io
            }
        }
    }
}
