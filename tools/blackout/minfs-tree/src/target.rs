// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{Context, Result},
    async_trait::async_trait,
    blackout_target::{
        static_tree::{DirectoryEntry, EntryDistribution},
        Test, TestServer,
    },
    fs_management::Minfs,
    rand::{rngs::StdRng, Rng, SeedableRng},
};

#[derive(Copy, Clone)]
struct MinfsTree;

#[async_trait]
impl Test for MinfsTree {
    async fn setup(&self, block_device: String, _seed: u64) -> Result<()> {
        log::info!("provided block device: {}", block_device);
        let dev = blackout_target::find_dev(&block_device).await?;
        log::info!("using equivalent block device: {}", dev);
        let mut minfs = Minfs::new(&dev)?;

        log::info!("formatting block device with minfs");
        minfs.format().context("failed to format minfs")?;

        Ok(())
    }

    async fn test(&self, block_device: String, seed: u64) -> Result<()> {
        log::info!("provided block device: {}", block_device);
        let dev = blackout_target::find_dev(&block_device).await?;
        log::info!("using equivalent block device: {}", dev);
        let mut minfs = Minfs::new(&dev)?;

        let root = format!("/test-fs-root-{}", seed);

        log::info!("mounting minfs into default namespace at {}", root);
        minfs.mount(&root).context("failed to mount minfs")?;

        log::info!("generating load");
        let mut rng = StdRng::seed_from_u64(seed);
        loop {
            log::info!("generating tree");
            let dist = EntryDistribution::new(6);
            let tree: DirectoryEntry = rng.sample(&dist);
            log::info!("generated tree: {:?}", tree);
            let tree_name = tree.get_name();
            let tree_path = format!("{}/{}", root, tree_name);
            log::info!("writing tree");
            tree.write_tree_at(&root).context("failed to write directory tree")?;
            // now try renaming the tree root
            let tree_path2 = format!("{}/{}-renamed", root, tree_name);
            log::info!("moving tree");
            std::fs::rename(&tree_path, &tree_path2).context("failed to move directory tree")?;
            // then try deleting the entire thing.
            log::info!("deleting tree");
            std::fs::remove_dir_all(&tree_path2).context("failed to delete directory tree")?;
        }
    }

    async fn verify(&self, block_device: String, _seed: u64) -> Result<()> {
        log::info!("provided block device: {}", block_device);
        let dev = blackout_target::find_dev(&block_device).await?;
        log::info!("using equivalent block device: {}", dev);
        let mut minfs = Minfs::new(&dev)?;

        log::info!("verifying disk with fsck");
        minfs.fsck().context("failed to run fsck")?;

        log::info!("verification successful");
        Ok(())
    }
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let server = TestServer::new(MinfsTree)?;
    server.serve().await;

    Ok(())
}
