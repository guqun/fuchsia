// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::base_package::BasePackage;
use crate::blobfs::construct_blobfs;

use anyhow::{Context, Result};
use assembly_config::ImageAssemblyConfig;
use assembly_fvm::{Filesystem, FilesystemAttributes, FvmBuilder, FvmType, NandFvmBuilder};
use assembly_images_config::{Fvm, FvmFilesystem, FvmOutput, SparseFvm};
use assembly_images_manifest::{Image, ImagesManifest};
use assembly_minfs::MinFSBuilder;
use assembly_tool::ToolProvider;
use log::info;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Constructs up-to four FVM files. Calling this function generates
/// a default FVM, a sparse FVM, a sparse blob-only FVM, and optionally a FVM
/// ready for fastboot flashing. This function returns the paths to each
/// generated FVM.
///
/// If the |fvm_config| includes information for an EMMC, then an EMMC-supported
/// sparse FVM will also be generated for fastboot flashing.
///
/// If the |fvm_config| includes information for a NAND, then an NAND-supported
/// sparse FVM will also be generated for fastboot flashing.
pub fn construct_fvm<'a>(
    outdir: impl AsRef<Path>,
    gendir: impl AsRef<Path>,
    tools: &impl ToolProvider,
    images_manifest: &mut ImagesManifest,
    assembly_config: &ImageAssemblyConfig,
    fvm_config: Fvm,
    base_package: &BasePackage,
) -> Result<()> {
    let mut builder = MultiFvmBuilder::new(
        outdir,
        gendir,
        assembly_config,
        images_manifest,
        fvm_config.slice_size,
        base_package,
    );
    for filesystem in fvm_config.filesystems {
        builder.filesystem(filesystem);
    }
    for output in fvm_config.outputs {
        builder.output(output);
    }
    builder.build(tools)
}

/// A builder that can produce multiple FVMs of various types in a single step. This is useful when
/// multiple fvms must be produced that share the same underlying filesystem, but we do not want
/// the cost of generating the filesystem multiple times.
pub struct MultiFvmBuilder<'a> {
    /// Map from the name of the filesystem to its entry.
    filesystems: HashMap<String, FilesystemEntry>,
    /// List of the FVMs to generate.
    outputs: Vec<FvmOutput>,
    /// The directory to write the outputs into.
    outdir: PathBuf,
    /// The directory to write the intermediate outputs into.
    gendir: PathBuf,
    /// The image assembly config.
    assembly_config: &'a ImageAssemblyConfig,
    /// The manifest of images to add new FVMs to.
    images_manifest: &'a mut ImagesManifest,
    /// The size of a slice for the FVM.
    slice_size: u64,
    /// The base package to add to blobfs.
    base_package: &'a BasePackage,
}

/// A single filesystem that can be added to the FVMs.
/// This is either the params to generate the filesystem, or the struct that contains how to use
/// the generated filesystem.
pub enum FilesystemEntry {
    Params(FvmFilesystem),
    Filesystem(Filesystem),
}

impl<'a> MultiFvmBuilder<'a> {
    /// Construct a new MultiFvmBuilder.
    /// These parameters are constant across all generated FVMs.
    pub fn new(
        outdir: impl AsRef<Path>,
        gendir: impl AsRef<Path>,
        assembly_config: &'a ImageAssemblyConfig,
        images_manifest: &'a mut ImagesManifest,
        slice_size: u64,
        base_package: &'a BasePackage,
    ) -> Self {
        Self {
            filesystems: HashMap::new(),
            outputs: Vec::new(),
            outdir: outdir.as_ref().to_path_buf(),
            gendir: gendir.as_ref().to_path_buf(),
            assembly_config,
            images_manifest,
            slice_size,
            base_package,
        }
    }

    /// Add a `filesystem` to the FVM.
    pub fn filesystem(&mut self, filesystem: FvmFilesystem) {
        let name = match &filesystem {
            FvmFilesystem::BlobFS(fs) => &fs.name,
            FvmFilesystem::MinFS(fs) => &fs.name,
            FvmFilesystem::EmptyMinFS(fs) => &fs.name,
            FvmFilesystem::EmptyAccount(fs) => &fs.name,
            FvmFilesystem::Reserved(fs) => &fs.name,
        };
        self.filesystems.insert(name.clone(), FilesystemEntry::Params(filesystem));
    }

    /// Add an `output` to be generated.
    pub fn output(&mut self, output: FvmOutput) {
        self.outputs.push(output);
    }

    /// Build all the FVM outputs.
    pub fn build(&mut self, tools: &impl ToolProvider) -> Result<()> {
        let outputs = self.outputs.clone();
        for output in outputs {
            self.build_output_and_add_to_manifest(tools, &output)?;
        }
        Ok(())
    }

    /// Build a single FVM output, and always add the result to the |images_manifest|.
    fn build_output_and_add_to_manifest(
        &mut self,
        tools: &impl ToolProvider,
        output: &FvmOutput,
    ) -> Result<()> {
        let add_to_manifest = true;
        self.build_output(tools, output, add_to_manifest)
    }

    /// Build a single FVM output, and let the caller choose whether to add it to the
    /// |images_manifest|.
    fn build_output(
        &mut self,
        tools: &impl ToolProvider,
        output: &FvmOutput,
        add_to_manifest: bool,
    ) -> Result<()> {
        match &output {
            FvmOutput::Standard(config) => {
                let fvm_tool = tools.get_tool("fvm")?;
                let path = self.outdir.join(format!("{}.blk", &config.name));
                let fvm_type = FvmType::Standard {
                    resize_image_file_to_fit: config.resize_image_file_to_fit,
                    truncate_to_length: config.truncate_to_length,
                };
                let mut builder =
                    FvmBuilder::new(fvm_tool, &path, self.slice_size, config.compress, fvm_type);
                for filesystem_name in &config.filesystems {
                    let fs = self
                        .get_filesystem(tools, &filesystem_name)
                        .context(format!("Including filesystem: {}", &filesystem_name))?;
                    builder.filesystem(fs);
                }
                builder.build()?;
                if add_to_manifest {
                    let image = match config.name.as_str() {
                        // Even though this is a standard FVM, people expect it to find it using
                        // the fvm.fastboot key in the ImagesManifest.
                        "fvm.fastboot" => Image::FVMFastboot(path),
                        _ => Image::FVM(path),
                    };
                    self.images_manifest.images.push(image);
                }
            }
            FvmOutput::Sparse(config) => {
                let fvm_tool = tools.get_tool("fvm")?;
                let path = self.outdir.join(format!("{}.blk", &config.name));
                let fvm_type = FvmType::Sparse { max_disk_size: config.max_disk_size };
                let compress = true;
                let mut builder =
                    FvmBuilder::new(fvm_tool, &path, self.slice_size, compress, fvm_type);

                let mut has_minfs = false;
                for filesystem_name in &config.filesystems {
                    let fs = self.get_filesystem(tools, &filesystem_name)?;
                    match fs {
                        Filesystem::MinFS { path: _, attributes: _ } => has_minfs = true,
                        _ => {}
                    }
                    builder.filesystem(fs);
                }

                builder.build()?;
                if add_to_manifest {
                    if has_minfs {
                        self.images_manifest.images.push(Image::FVMSparse(path));
                    } else {
                        self.images_manifest.images.push(Image::FVMSparseBlob(path));
                    }
                }
            }
            FvmOutput::Nand(config) => {
                // First, build the sparse FVM.
                let sparse_tmp_name = format!("{}.tmp", &config.name);
                let sparse_output = FvmOutput::Sparse(SparseFvm {
                    name: sparse_tmp_name.clone(),
                    filesystems: config.filesystems.clone(),
                    max_disk_size: config.max_disk_size,
                });
                let do_not_add_to_manifest = false;
                self.build_output(tools, &sparse_output, do_not_add_to_manifest)?;

                // Second, prepare it for NAND.
                let tool = tools.get_tool("fvm")?;
                let sparse_output = self.outdir.join(format!("{}.blk", &sparse_tmp_name));
                let output = self.outdir.join(format!("{}.blk", &config.name));
                let compression = if config.compress { Some("lz4".to_string()) } else { None };
                let builder = NandFvmBuilder {
                    tool,
                    output: output.clone(),
                    sparse_blob_fvm: sparse_output,
                    max_disk_size: config.max_disk_size,
                    compression,
                    page_size: config.page_size,
                    oob_size: config.oob_size,
                    pages_per_block: config.pages_per_block,
                    block_count: config.block_count,
                };
                builder.build()?;

                if add_to_manifest {
                    self.images_manifest.images.push(Image::FVMFastboot(output));
                }
            }
        }
        Ok(())
    }

    /// Return the info for the filesystem identified by the |name|.
    /// Reuses prebuilt info if possible.
    /// Builds the filesystem if necessary.
    fn get_filesystem(&mut self, tools: &impl ToolProvider, name: &String) -> Result<Filesystem> {
        let entry = match self.filesystems.get(name) {
            Some(e) => e,
            _ => anyhow::bail!("Filesystem is not specified: {}", name),
        };

        match entry {
            // Return the already assembled info.
            FilesystemEntry::Filesystem(ref filesystem) => Ok(filesystem.clone()),
            // Build the filesystem and assemble the info.
            FilesystemEntry::Params(params) => {
                info!("Creating FVM filesystem: {}", name);
                let (image, filesystem) = self
                    .build_filesystem(tools, params)
                    .context(format!("Building filesystem: {}", name))?;
                if let Some(image) = image {
                    self.images_manifest.images.push(image);
                }
                self.filesystems
                    .insert(name.clone(), FilesystemEntry::Filesystem(filesystem.clone()));
                Ok(filesystem)
            }
        }
    }

    /// Build a filesystem and return the info to use it, and optionally the image metadata to
    /// insert into the image manifest.
    fn build_filesystem(
        &self,
        tools: &impl ToolProvider,
        params: &FvmFilesystem,
    ) -> Result<(Option<Image>, Filesystem)> {
        let (image, filesystem) = match &params {
            FvmFilesystem::BlobFS(config) => {
                let (path, contents) = construct_blobfs(
                    tools.get_tool("blobfs")?,
                    &self.outdir,
                    &self.gendir,
                    &self.assembly_config,
                    &config,
                    &self.base_package,
                )
                .context("Constructing blobfs")?;
                (
                    Some(Image::BlobFS { path: path.clone(), contents }),
                    Filesystem::BlobFS {
                        path,
                        attributes: FilesystemAttributes {
                            name: config.name.clone(),
                            minimum_inodes: config.minimum_inodes,
                            minimum_data_bytes: config.minimum_data_bytes,
                            maximum_bytes: config.maximum_bytes,
                        },
                    },
                )
            }
            FvmFilesystem::MinFS(config) => {
                let path = self.outdir.join("data.blk");
                let builder = MinFSBuilder::new(tools.get_tool("minfs")?);
                builder.build(&path).context("Constructing minfs")?;
                (
                    None,
                    Filesystem::MinFS {
                        path,
                        attributes: FilesystemAttributes {
                            name: config.name.clone(),
                            minimum_inodes: config.minimum_inodes,
                            minimum_data_bytes: config.minimum_data_bytes,
                            maximum_bytes: config.maximum_bytes,
                        },
                    },
                )
            }
            FvmFilesystem::EmptyMinFS(_config) => (None, Filesystem::EmptyMinFS {}),
            FvmFilesystem::EmptyAccount(_config) => (None, Filesystem::EmptyAccount {}),
            FvmFilesystem::Reserved(config) => {
                (None, Filesystem::Reserved { slices: config.slices })
            }
        };
        Ok((image, filesystem))
    }
}

#[cfg(test)]
mod tests {
    use super::MultiFvmBuilder;

    use crate::base_package::BasePackage;
    use assembly_config::{ImageAssemblyConfig, KernelConfig};
    use assembly_images_config::{
        BlobFS, BlobFSLayout, EmptyAccount, EmptyMinFS, FvmFilesystem, FvmOutput, MinFS, NandFvm,
        Reserved, SparseFvm, StandardFvm,
    };
    use assembly_images_manifest::ImagesManifest;
    use assembly_tool::testing::FakeToolProvider;
    use assembly_tool::{ToolCommandLog, ToolProvider};
    use assembly_util::PathToStringExt;
    use serde_json::json;
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::io::Write;
    use tempfile::tempdir;

    #[test]
    fn construct_no_outputs() {
        let dir = tempdir().unwrap();

        let assembly_config = ImageAssemblyConfig {
            system: Vec::new(),
            base: Vec::new(),
            cache: Vec::new(),
            bootfs_packages: Vec::new(),
            kernel: KernelConfig {
                path: "path/to/kernel".into(),
                args: Vec::new(),
                clock_backstop: 0,
            },
            qemu_kernel: "path/to/qemu/kernel".into(),
            boot_args: Vec::new(),
            bootfs_files: Vec::new(),
        };
        let mut images_manifest = ImagesManifest::default();
        let base_package = BasePackage {
            merkle: [0u8; 32].into(),
            contents: BTreeMap::new(),
            path: "path/to/base_package".into(),
        };
        let slice_size = 0;
        let mut builder = MultiFvmBuilder::new(
            dir.path(),
            dir.path(),
            &assembly_config,
            &mut images_manifest,
            slice_size,
            &base_package,
        );
        let tools = FakeToolProvider::default();
        builder.build(&tools).unwrap();

        let expected_log: ToolCommandLog = serde_json::from_value(json!({
            "commands": []
        }))
        .unwrap();
        assert_eq!(&expected_log, tools.log());
    }

    #[test]
    fn construct_standard_no_fs() {
        let dir = tempdir().unwrap();

        let assembly_config = ImageAssemblyConfig {
            system: Vec::new(),
            base: Vec::new(),
            cache: Vec::new(),
            bootfs_packages: Vec::new(),
            kernel: KernelConfig {
                path: "path/to/kernel".into(),
                args: Vec::new(),
                clock_backstop: 0,
            },
            qemu_kernel: "path/to/qemu/kernel".into(),
            boot_args: Vec::new(),
            bootfs_files: Vec::new(),
        };
        let mut images_manifest = ImagesManifest::default();
        let base_package = BasePackage {
            merkle: [0u8; 32].into(),
            contents: BTreeMap::new(),
            path: "path/to/base_package".into(),
        };
        let slice_size = 0;
        let mut builder = MultiFvmBuilder::new(
            dir.path(),
            dir.path(),
            &assembly_config,
            &mut images_manifest,
            slice_size,
            &base_package,
        );
        builder.output(FvmOutput::Standard(StandardFvm {
            name: "fvm".into(),
            filesystems: Vec::new(),
            compress: false,
            resize_image_file_to_fit: false,
            truncate_to_length: None,
        }));
        let tools = FakeToolProvider::default();
        builder.build(&tools).unwrap();

        let output_path = dir.path().join("fvm.blk").path_to_string().unwrap();
        let expected_log: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
            {
                "tool": "./host_x64/fvm",
                "args": [
                    output_path,
                    "create",
                    "--slice",
                    "0",
                ]
            }
            ]
        }))
        .unwrap();
        assert_eq!(&expected_log, tools.log());
    }

    #[test]
    fn construct_multiple_no_fs() {
        let dir = tempdir().unwrap();

        let assembly_config = ImageAssemblyConfig {
            system: Vec::new(),
            base: Vec::new(),
            cache: Vec::new(),
            bootfs_packages: Vec::new(),
            kernel: KernelConfig {
                path: "path/to/kernel".into(),
                args: Vec::new(),
                clock_backstop: 0,
            },
            qemu_kernel: "path/to/qemu/kernel".into(),
            boot_args: Vec::new(),
            bootfs_files: Vec::new(),
        };
        let mut images_manifest = ImagesManifest::default();
        let base_package = BasePackage {
            merkle: [0u8; 32].into(),
            contents: BTreeMap::new(),
            path: "path/to/base_package".into(),
        };
        let slice_size = 0;
        let mut builder = MultiFvmBuilder::new(
            dir.path(),
            dir.path(),
            &assembly_config,
            &mut images_manifest,
            slice_size,
            &base_package,
        );
        builder.output(FvmOutput::Standard(StandardFvm {
            name: "fvm".into(),
            filesystems: Vec::new(),
            compress: false,
            resize_image_file_to_fit: false,
            truncate_to_length: None,
        }));
        builder.output(FvmOutput::Sparse(SparseFvm {
            name: "fvm.sparse".into(),
            filesystems: Vec::new(),
            max_disk_size: None,
        }));
        builder.output(FvmOutput::Nand(NandFvm {
            name: "fvm.nand".into(),
            filesystems: Vec::new(),
            max_disk_size: None,
            compress: false,
            block_count: 1,
            oob_size: 2,
            page_size: 3,
            pages_per_block: 4,
        }));
        let tools = FakeToolProvider::default();
        builder.build(&tools).unwrap();

        let standard_path = dir.path().join("fvm.blk").path_to_string().unwrap();
        let sparse_path = dir.path().join("fvm.sparse.blk").path_to_string().unwrap();
        let nand_tmp_path = dir.path().join("fvm.nand.tmp.blk").path_to_string().unwrap();
        let nand_path = dir.path().join("fvm.nand.blk").path_to_string().unwrap();
        let expected_log: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
            {
                "tool": "./host_x64/fvm",
                "args": [
                    standard_path,
                    "create",
                    "--slice",
                    "0",
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    sparse_path,
                    "sparse",
                    "--slice",
                    "0",
                    "--compress",
                    "lz4",
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    nand_tmp_path,
                    "sparse",
                    "--slice",
                    "0",
                    "--compress",
                    "lz4",
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    nand_path,
                    "ftl-raw-nand",
                    "--nand-page-size",
                    "3",
                    "--nand-oob-size",
                    "2",
                    "--nand-pages-per-block",
                    "4",
                    "--nand-block-count",
                    "1",
                    "--sparse",
                    nand_tmp_path,
                ]
            },
            ]
        }))
        .unwrap();
        assert_eq!(&expected_log, tools.log());
    }

    #[test]
    fn construct_standard_with_fs() {
        let dir = tempdir().unwrap();

        let assembly_config = ImageAssemblyConfig {
            system: Vec::new(),
            base: Vec::new(),
            cache: Vec::new(),
            bootfs_packages: Vec::new(),
            kernel: KernelConfig {
                path: "path/to/kernel".into(),
                args: Vec::new(),
                clock_backstop: 0,
            },
            qemu_kernel: "path/to/qemu/kernel".into(),
            boot_args: Vec::new(),
            bootfs_files: Vec::new(),
        };
        let mut images_manifest = ImagesManifest::default();

        let base_package_path = dir.path().join("base.far");
        let mut base_package_file = File::create(&base_package_path).unwrap();
        write!(base_package_file, "base package").unwrap();
        let base_package = BasePackage {
            merkle: [0u8; 32].into(),
            contents: BTreeMap::new(),
            path: base_package_path,
        };

        let slice_size = 0;
        let mut builder = MultiFvmBuilder::new(
            dir.path(),
            dir.path(),
            &assembly_config,
            &mut images_manifest,
            slice_size,
            &base_package,
        );
        builder.filesystem(FvmFilesystem::BlobFS(BlobFS {
            name: "blob".into(),
            compress: false,
            layout: BlobFSLayout::Compact,
            maximum_bytes: None,
            minimum_data_bytes: None,
            minimum_inodes: None,
        }));
        builder.filesystem(FvmFilesystem::MinFS(MinFS {
            name: "data".into(),
            maximum_bytes: None,
            minimum_data_bytes: None,
            minimum_inodes: None,
        }));
        builder.filesystem(FvmFilesystem::EmptyMinFS(EmptyMinFS { name: "empty-data".into() }));
        builder.filesystem(FvmFilesystem::EmptyAccount(EmptyAccount { name: "account".into() }));
        builder
            .filesystem(FvmFilesystem::Reserved(Reserved { name: "reserved".into(), slices: 10 }));
        builder.output(FvmOutput::Standard(StandardFvm {
            name: "fvm".into(),
            filesystems: vec![
                "blob".into(),
                "data".into(),
                "empty-data".into(),
                "account".into(),
                "reserved".into(),
            ],
            compress: false,
            resize_image_file_to_fit: false,
            truncate_to_length: None,
        }));
        let tools = FakeToolProvider::default();
        builder.build(&tools).unwrap();

        let blobfs_path = dir.path().join("blob.blk").path_to_string().unwrap();
        let blobs_json_path = dir.path().join("blobs.json").path_to_string().unwrap();
        let blob_manifest_path = dir.path().join("blob.manifest").path_to_string().unwrap();
        let minfs_path = dir.path().join("data.blk").path_to_string().unwrap();
        let standard_path = dir.path().join("fvm.blk").path_to_string().unwrap();
        let expected_log: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
            {
                "tool": "./host_x64/blobfs",
                "args": [
                    "--json-output",
                    blobs_json_path,
                    blobfs_path,
                    "create",
                    "--manifest",
                    blob_manifest_path,
                ]
            },
            {
                "tool": "./host_x64/minfs",
                "args": [
                    minfs_path,
                    "create",
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    standard_path,
                    "create",
                    "--slice",
                    "0",
                    "--blob",
                    blobfs_path,
                    "--data",
                    minfs_path,
                    "--with-empty-minfs",
                    "--with-empty-account-partition",
                    "--reserve-slices",
                    "10",
                ]
            }
            ]
        }))
        .unwrap();
        assert_eq!(&expected_log, tools.log());
    }

    #[test]
    fn construct_multiple_with_fs() {
        let dir = tempdir().unwrap();

        let assembly_config = ImageAssemblyConfig {
            system: Vec::new(),
            base: Vec::new(),
            cache: Vec::new(),
            bootfs_packages: Vec::new(),
            kernel: KernelConfig {
                path: "path/to/kernel".into(),
                args: Vec::new(),
                clock_backstop: 0,
            },
            qemu_kernel: "path/to/qemu/kernel".into(),
            boot_args: Vec::new(),
            bootfs_files: Vec::new(),
        };
        let mut images_manifest = ImagesManifest::default();

        let base_package_path = dir.path().join("base.far");
        let mut base_package_file = File::create(&base_package_path).unwrap();
        write!(base_package_file, "base package").unwrap();
        let base_package = BasePackage {
            merkle: [0u8; 32].into(),
            contents: BTreeMap::new(),
            path: base_package_path,
        };

        let slice_size = 0;
        let mut builder = MultiFvmBuilder::new(
            dir.path(),
            dir.path(),
            &assembly_config,
            &mut images_manifest,
            slice_size,
            &base_package,
        );
        builder.filesystem(FvmFilesystem::BlobFS(BlobFS {
            name: "blob".into(),
            compress: false,
            layout: BlobFSLayout::Compact,
            maximum_bytes: None,
            minimum_data_bytes: None,
            minimum_inodes: None,
        }));
        builder.filesystem(FvmFilesystem::MinFS(MinFS {
            name: "data".into(),
            maximum_bytes: None,
            minimum_data_bytes: None,
            minimum_inodes: None,
        }));
        builder.output(FvmOutput::Standard(StandardFvm {
            name: "fvm".into(),
            filesystems: vec!["blob".into(), "data".into()],
            compress: false,
            resize_image_file_to_fit: false,
            truncate_to_length: None,
        }));
        builder.output(FvmOutput::Sparse(SparseFvm {
            name: "fvm.sparse".into(),
            filesystems: vec!["blob".into(), "data".into()],
            max_disk_size: None,
        }));
        builder.output(FvmOutput::Nand(NandFvm {
            name: "fvm.nand".into(),
            filesystems: vec!["blob".into(), "data".into()],
            max_disk_size: None,
            compress: false,
            block_count: 1,
            oob_size: 2,
            page_size: 3,
            pages_per_block: 4,
        }));
        let tools = FakeToolProvider::default();
        builder.build(&tools).unwrap();

        let blobfs_path = dir.path().join("blob.blk").path_to_string().unwrap();
        let blobs_json_path = dir.path().join("blobs.json").path_to_string().unwrap();
        let blob_manifest_path = dir.path().join("blob.manifest").path_to_string().unwrap();
        let minfs_path = dir.path().join("data.blk").path_to_string().unwrap();
        let standard_path = dir.path().join("fvm.blk").path_to_string().unwrap();
        let sparse_path = dir.path().join("fvm.sparse.blk").path_to_string().unwrap();
        let nand_tmp_path = dir.path().join("fvm.nand.tmp.blk").path_to_string().unwrap();
        let nand_path = dir.path().join("fvm.nand.blk").path_to_string().unwrap();
        let expected_log: ToolCommandLog = serde_json::from_value(json!({
            "commands": [
            {
                "tool": "./host_x64/blobfs",
                "args": [
                    "--json-output",
                    blobs_json_path,
                    blobfs_path,
                    "create",
                    "--manifest",
                    blob_manifest_path,
                ]
            },
            {
                "tool": "./host_x64/minfs",
                "args": [
                    minfs_path,
                    "create",
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    standard_path,
                    "create",
                    "--slice",
                    "0",
                    "--blob",
                    blobfs_path,
                    "--data",
                    minfs_path,
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    sparse_path,
                    "sparse",
                    "--slice",
                    "0",
                    "--compress",
                    "lz4",
                    "--blob",
                    blobfs_path,
                    "--data",
                    minfs_path,
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    nand_tmp_path,
                    "sparse",
                    "--slice",
                    "0",
                    "--compress",
                    "lz4",
                    "--blob",
                    blobfs_path,
                    "--data",
                    minfs_path,
                ]
            },
            {
                "tool": "./host_x64/fvm",
                "args": [
                    nand_path,
                    "ftl-raw-nand",
                    "--nand-page-size",
                    "3",
                    "--nand-oob-size",
                    "2",
                    "--nand-pages-per-block",
                    "4",
                    "--nand-block-count",
                    "1",
                    "--sparse",
                    nand_tmp_path,
                ]
            },
            ]
        }))
        .unwrap();
        assert_eq!(&expected_log, tools.log());
    }
}
