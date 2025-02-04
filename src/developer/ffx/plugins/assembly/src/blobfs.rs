// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base_package::BasePackage;

use anyhow::{Context, Result};
use assembly_blobfs::BlobFSBuilder;
use assembly_config::ImageAssemblyConfig;
use assembly_images_config::BlobFS;
use assembly_images_manifest::BlobfsContents;
use assembly_tool::Tool;
use std::path::{Path, PathBuf};

pub fn construct_blobfs(
    blobfs_tool: Box<dyn Tool>,
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    image_config: &ImageAssemblyConfig,
    blobfs_config: &BlobFS,
    base_package: &BasePackage,
) -> Result<(PathBuf, BlobfsContents)> {
    let mut contents = BlobfsContents::default();
    let mut blobfs_builder = BlobFSBuilder::new(blobfs_tool, blobfs_config.layout.to_string());
    blobfs_builder.set_compressed(blobfs_config.compress);

    // Add the base and cache packages.
    for package_manifest_path in &image_config.base {
        blobfs_builder.add_package(&package_manifest_path)?;
        contents.packages.base.add_package(package_manifest_path)?;
    }
    for package_manifest_path in &image_config.cache {
        blobfs_builder.add_package(&package_manifest_path)?;
        contents.packages.cache.add_package(package_manifest_path)?;
    }

    // Add the base package and its contents.
    blobfs_builder.add_file(&base_package.path)?;
    for (_, source) in &base_package.contents {
        blobfs_builder.add_file(source)?;
    }

    // Build the blobfs and return its path.
    let blobfs_path = outdir.as_ref().join("blob.blk");
    blobfs_builder.build(gendir, &blobfs_path).context("Failed to build the blobfs")?;
    Ok((blobfs_path, contents))
}

#[cfg(test)]
mod tests {
    use super::construct_blobfs;
    use crate::base_package::BasePackage;
    use assembly_config::ImageAssemblyConfig;
    use assembly_images_config::{BlobFS, BlobFSLayout};
    use assembly_tool::testing::FakeToolProvider;
    use assembly_tool::ToolProvider;
    use fuchsia_hash::Hash;
    use std::collections::BTreeMap;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn construct() {
        let dir = tempdir().unwrap();
        let image_config = ImageAssemblyConfig::new_for_testing("kernel", 0);
        let blobfs_config = BlobFS {
            name: "blob".into(),
            layout: BlobFSLayout::Compact,
            compress: true,
            maximum_bytes: None,
            minimum_data_bytes: None,
            minimum_inodes: None,
        };

        // Create a fake base package.
        let base_path = dir.path().join("base.far");
        std::fs::write(&base_path, "fake base").unwrap();
        let base = BasePackage {
            merkle: Hash::from_str(
                "0000000000000000000000000000000000000000000000000000000000000000",
            )
            .unwrap(),
            contents: BTreeMap::default(),
            path: base_path,
        };

        // Create a fake blobfs tool.
        let tools = FakeToolProvider::default();
        let blobfs_tool = tools.get_tool("blobfs").unwrap();

        // Construct blobfs, and ensure no error is returned.
        construct_blobfs(blobfs_tool, dir.path(), dir.path(), &image_config, &blobfs_config, &base)
            .unwrap();
    }
}
