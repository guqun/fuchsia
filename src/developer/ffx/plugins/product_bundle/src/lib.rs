// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A tool to:
//! - acquire and display product bundle information (metadata)
//! - acquire related data files, such as disk partition images (data)

use {
    anyhow::{Context, Result},
    ffx_core::ffx_plugin,
    ffx_product_bundle_args::{
        CreateCommand, GetCommand, ListCommand, ProductBundleCommand, SubCommand,
    },
    fidl_fuchsia_developer_ffx::RepositoryRegistryProxy,
    fidl_fuchsia_developer_ffx_ext::{RepositoryError, RepositorySpec},
    pbms::{get_product_data, is_pb_ready, product_bundle_urls, update_metadata},
    std::{
        convert::TryInto,
        io::{stdout, Write},
    },
};

mod create;

/// Provide functionality to list product-bundle metadata, fetch metadata, and
/// pull images and related data.
#[ffx_plugin(RepositoryRegistryProxy = "daemon::protocol")]
pub async fn product_bundle(
    cmd: ProductBundleCommand,
    repos: RepositoryRegistryProxy,
) -> Result<()> {
    product_bundle_plugin_impl(cmd, &mut stdout(), repos).await
}

/// Dispatch to a sub-command.
pub async fn product_bundle_plugin_impl<W>(
    command: ProductBundleCommand,
    writer: &mut W,
    repos: RepositoryRegistryProxy,
) -> Result<()>
where
    W: Write + Sync,
{
    match &command.sub {
        SubCommand::List(cmd) => pb_list(writer, &cmd).await?,
        SubCommand::Get(cmd) => pb_get(writer, &cmd, repos).await?,
        SubCommand::Create(cmd) => pb_create(&cmd).await?,
    }
    Ok(())
}

/// `ffx product-bundle list` sub-command.
async fn pb_list<W: Write + Sync>(writer: &mut W, cmd: &ListCommand) -> Result<()> {
    if !cmd.cached {
        update_metadata(/*verbose=*/ false, writer).await?;
    }
    let mut entries = product_bundle_urls().await.context("list pbms")?;
    entries.sort();
    writeln!(writer, "")?;
    for entry in entries {
        let ready = if is_pb_ready(&entry).await? { "*" } else { "" };
        writeln!(writer, "{}{}", entry, ready)?;
    }
    writeln!(
        writer,
        "\
        \n*No need to fetch with `ffx product-bundle get ...`. \
        The '*' is not part of the name.\
        \n"
    )?;
    Ok(())
}

/// `ffx product-bundle get` sub-command.
async fn pb_get<W: Write + Sync>(
    writer: &mut W,
    cmd: &GetCommand,
    repos: RepositoryRegistryProxy,
) -> Result<()> {
    let start = std::time::Instant::now();
    if !cmd.cached {
        update_metadata(cmd.verbose, writer).await?;
    }
    let product_url = pbms::select_product_bundle(&cmd.product_bundle_name).await?;
    get_product_data(&product_url, cmd.verbose, writer).await?;

    // Register a repository with the daemon if we downloaded any packaging artifacts.
    if let Some(product_name) = product_url.fragment() {
        if let Ok(repo_path) = pbms::get_packages_dir(&product_url).await {
            if repo_path.exists() {
                let repo_path = repo_path
                    .canonicalize()
                    .with_context(|| format!("canonicalizing {:?}", repo_path))?;

                let repo_spec = RepositorySpec::Pm { path: repo_path.try_into()? };
                repos
                    .add_repository(&product_name, &mut repo_spec.into())
                    .await
                    .context("communicating with ffx daemon")?
                    .map_err(RepositoryError::from)
                    .with_context(|| format!("registering repository {}", product_name))?;

                writeln!(writer, "Created repository named '{}'", product_name)?;
            }
        }
    }

    log::debug!("Total fx product-bundle get runtime {} seconds.", start.elapsed().as_secs_f32());
    Ok(())
}

/// `ffx product-bundle create` sub-command.
async fn pb_create(cmd: &CreateCommand) -> Result<()> {
    create::create_product_bundle(cmd).await
}

#[cfg(test)]
mod test {}
