// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Result},
    errors::ffx_bail,
    ffx_config::file,
    ffx_core::ffx_plugin,
    ffx_fastboot::common::{cmd::OemFile, from_manifest},
    ffx_flash_args::FlashCommand,
    fidl_fuchsia_developer_ffx::FastbootProxy,
    std::io::{stdout, Write},
};

const SSH_OEM_COMMAND: &str = "add-staged-bootloader-file ssh.authorized_keys";

#[ffx_plugin()]
pub async fn flash(fastboot_proxy: FastbootProxy, cmd: FlashCommand) -> Result<()> {
    flash_plugin_impl(fastboot_proxy, cmd, &mut stdout()).await
}

pub async fn flash_plugin_impl<W: Write>(
    fastboot_proxy: FastbootProxy,
    mut cmd: FlashCommand,
    writer: &mut W,
) -> Result<()> {
    match cmd.authorized_keys.as_ref() {
        Some(ssh) => {
            let ssh_file = match std::fs::canonicalize(ssh) {
                Ok(path) => path,
                Err(err) => {
                    ffx_bail!("Cannot find SSH key \"{}\": {}", ssh, err);
                }
            };
            if cmd.oem_stage.iter().find(|f| f.command() == SSH_OEM_COMMAND).is_some() {
                ffx_bail!("Both the SSH key and the SSH OEM Stage flags were set. Only use one.");
            }
            cmd.oem_stage.push(OemFile::new(
                SSH_OEM_COMMAND.to_string(),
                ssh_file
                    .into_os_string()
                    .into_string()
                    .map_err(|s| anyhow!("Cannot convert OsString \"{:?}\" to String", s))?,
            ));
        }
        None => {
            if cmd.oem_stage.iter().find(|f| f.command() == SSH_OEM_COMMAND).is_none() {
                let key: Option<String> = file("ssh.pub").await?;
                match key {
                    Some(k) => {
                        eprintln!("No `--authorized-keys` flag, using {}", k);
                        cmd.oem_stage.push(OemFile::new(SSH_OEM_COMMAND.to_string(), k));
                    }
                    None => ffx_bail!(
                        "Warning: flashing without a SSH key is not advised. \n\
                              Use the `--authorized-keys` to pass a ssh key."
                    ),
                }
            }
        }
    }

    from_manifest(writer, cmd, fastboot_proxy).await
}

////////////////////////////////////////////////////////////////////////////////
// tests

#[cfg(test)]
mod test {
    use super::*;
    use ffx_fastboot::test::setup;
    use std::default::Default;
    use std::path::PathBuf;
    use tempfile::NamedTempFile;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_nonexistent_file_throws_err() {
        assert!(flash(
            setup().1,
            FlashCommand {
                manifest: Some(PathBuf::from("ffx_test_does_not_exist")),
                ..Default::default()
            }
        )
        .await
        .is_err())
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_nonexistent_ssh_file_throws_err() {
        let tmp_file = NamedTempFile::new().expect("tmp access failed");
        let tmp_file_name = tmp_file.path().to_string_lossy().to_string();
        assert!(flash(
            setup().1,
            FlashCommand {
                manifest: Some(PathBuf::from(tmp_file_name)),
                authorized_keys: Some("ssh_does_not_exist".to_string()),
                ..Default::default()
            }
        )
        .await
        .is_err())
    }
}
