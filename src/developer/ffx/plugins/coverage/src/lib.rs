// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use {
    anyhow::{anyhow, Context, Result},
    ffx_core::ffx_plugin,
    ffx_coverage_args::CoverageCommand,
    glob::glob,
    std::{
        path::{Path, PathBuf},
        process::{Command, Stdio},
    },
    symbol_index::{global_symbol_index_path, SymbolIndex},
};

// The line found right above build ID in `llvm-profdata show --binary-ids` output.
const BINARY_ID_LINE: &str = "Binary IDs:";

#[ffx_plugin("coverage")]
pub async fn coverage(cmd: CoverageCommand) -> Result<()> {
    let clang_bin_dir = cmd.clang_dir.join("bin");
    let llvm_profdata_bin = clang_bin_dir.join("llvm-profdata");
    llvm_profdata_bin
        .exists()
        .then_some(())
        .ok_or(anyhow!("{:?} does not exist", llvm_profdata_bin))?;
    let llvm_cov_bin = clang_bin_dir.join("llvm-cov");
    llvm_cov_bin.exists().then_some(()).ok_or(anyhow!("{:?} does not exist", llvm_cov_bin))?;

    let profraws = glob_profraws(&cmd.test_output_dir)?;
    // TODO(https://fxbug.dev/99951): find a better place to put merged.profdata.
    let merged_profile = cmd.test_output_dir.join("merged.profdata");
    merge_profraws(&llvm_profdata_bin, &profraws, &merged_profile)
        .context("failed to merge profiles")?;

    let symbol_index_path = match cmd.symbol_index_json {
        Some(p) => p.to_string_lossy().to_string(),
        None => global_symbol_index_path()?,
    };
    let bin_files = find_binaries(
        &SymbolIndex::load(&symbol_index_path)?,
        &llvm_profdata_bin,
        &profraws,
        show_binary_id,
    )?;

    show_coverage(&llvm_cov_bin, &merged_profile, &bin_files, &cmd.src_files)
        .context("failed to show coverage")
}

/// Merges input `profraws` using llvm-profdata and writes output to `output_path`.
fn merge_profraws(
    llvm_profdata_bin: &Path,
    profraws: &[PathBuf],
    output_path: &Path,
) -> Result<()> {
    let merge_cmd = Command::new(llvm_profdata_bin)
        .args(["merge", "--sparse", "--output"])
        .arg(output_path)
        .args(profraws)
        .output()
        .context("failed to merge raw profiles")?;
    match merge_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(anyhow!(
            "failed to merge raw profiles: status code {}, stderr: {}",
            code,
            String::from_utf8_lossy(&merge_cmd.stderr)
        )),
        None => Err(anyhow!("profile merging terminated by signal unexpectedly")),
    }
}

/// Calls `llvm-profdata show --binary-ids` to fetch binary ID from input raw profile.
fn show_binary_id(llvm_profdata_bin: &Path, profraw: &Path) -> Result<String> {
    let cmd = Command::new(llvm_profdata_bin)
        .args(["show", "--binary-ids"])
        .arg(profraw)
        .output()
        .context(format!("failed to show binary ID from {:?}", profraw))?;
    let stdout = String::from_utf8_lossy(&cmd.stdout);
    let tokens: Vec<&str> = stdout.split(BINARY_ID_LINE).collect();
    match tokens[..] {
        [_, binary_id] => Ok(binary_id.trim().to_string()),
        _ => Err(anyhow!("unexpected llvm-profdata show output")),
    }
}

/// Find binary files from .build-id directories to pass. These are needed for `llvm-cov show`.
fn find_binaries<F: FnMut(&Path, &Path) -> Result<String>>(
    symbol_index: &SymbolIndex,
    llvm_profdata_bin: &Path,
    profraws: &[PathBuf],
    mut show_id: F, // stubbable in test
) -> Result<Vec<PathBuf>> {
    profraws
        .iter()
        .map(|profraw| {
            let binary_id = show_id(llvm_profdata_bin, profraw)?;
            find_debug_file(symbol_index, &binary_id)
                .context(anyhow!("failed to find binary file for {:?}", profraw,))
        })
        .collect()
}

/// Finds debug file in local .build-id directories from symbol index.
//
// TODO(https://fxbug.dev/100358): replace this with llvm-debuginfod-find when it's available.
fn find_debug_file(symbol_index: &SymbolIndex, binary_id: &str) -> Result<PathBuf> {
    if binary_id.len() > 2 {
        // For simplicity always return the first match. Note this is not always safe.
        symbol_index
            .build_id_dirs
            .iter()
            .find_map(|dir| {
                let p = PathBuf::from(&dir.path)
                    .join(binary_id[..2].to_string())
                    .join(format!("{}.debug", binary_id[2..].to_string()));
                p.exists().then_some(p)
            })
            .ok_or(anyhow!("no matching debug files found for binary ID {}", binary_id))
    } else {
        Err(anyhow!("binary ID must have more than 2 characters, got '{}'", binary_id))
    }
}

/// Calls `llvm-cov show` to display coverage from `merged_profile` for `bin_files`.
/// `src_files` can be used to filter coverage for selected source files.
fn show_coverage(
    llvm_cov_bin: &Path,
    merged_profile: &Path,
    bin_files: &[PathBuf],
    src_files: &[PathBuf],
) -> Result<()> {
    let bin_file_args = bin_files.iter().fold(Vec::new(), |mut acc, val| {
        if acc.len() > 0 {
            acc.push("-object");
        }
        acc.push(val.to_str().expect("failed to convert path to string"));
        acc
    });
    let show_cmd = Command::new(llvm_cov_bin)
        .args(["show", "-instr-profile"])
        .arg(merged_profile)
        .args(&bin_file_args)
        .args(src_files)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .output()
        .context("failed to show coverage")?;
    match show_cmd.status.code() {
        Some(0) => Ok(()),
        Some(code) => Err(anyhow!(
            "failed to show coverage: status code {}, stderr: {}",
            code,
            String::from_utf8_lossy(&show_cmd.stderr)
        )),
        None => Err(anyhow!("coverage display terminated by signal unexpectedly")),
    }
}

/// Finds all raw coverage profiles in `test_out_dir`.
fn glob_profraws(test_out_dir: &Path) -> Result<Vec<PathBuf>> {
    let pattern = test_out_dir.join("**").join("*.profraw");
    Ok(glob(pattern.to_str().unwrap())?.filter_map(Result::ok).collect::<Vec<PathBuf>>())
}

#[cfg(test)]
mod tests {
    use {
        super::*,
        assert_matches::assert_matches,
        std::{
            fs::{create_dir, set_permissions, File, Permissions},
            io::Write,
            os::unix::fs::PermissionsExt,
        },
        symbol_index::BuildIdDir,
        tempfile::TempDir,
    };

    #[test]
    fn test_glob_profraws() {
        let test_dir = TempDir::new().unwrap();
        create_dir(test_dir.path().join("nested")).unwrap();

        File::create(test_dir.path().join("foo.profraw")).unwrap();
        File::create(test_dir.path().join("nested").join("bar.profraw")).unwrap();
        File::create(test_dir.path().join("foo.not_profraw")).unwrap();
        File::create(test_dir.path().join("nested").join("baz.not_profraw")).unwrap();

        assert_eq!(
            glob_profraws(&test_dir.path().to_path_buf()).unwrap(),
            vec![
                PathBuf::from(test_dir.path().join("foo.profraw")),
                PathBuf::from(test_dir.path().join("nested").join("bar.profraw")),
            ],
        );
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_coverage() {
        ffx_config::init(&[], None, None).unwrap();

        let test_dir = TempDir::new().unwrap();
        let test_dir_path = test_dir.path().to_path_buf();
        let test_bin_dir = test_dir_path.join("bin");
        create_dir(&test_bin_dir).unwrap();

        // Create an empty symbol index for testing.
        let test_symbol_index_json = test_dir.path().join("symbol_index.json");
        File::create(&test_symbol_index_json).unwrap().write_all(b"{}").unwrap();

        // Missing both llvm-profdata and llvm-cov.
        assert!(coverage(CoverageCommand {
            test_output_dir: PathBuf::from(&test_dir_path),
            clang_dir: PathBuf::from(&test_dir_path),
            symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
            src_files: Vec::new(),
        })
        .await
        .is_err());

        // Create empty test binaries for the coverage function to call.
        File::create(test_bin_dir.join("llvm-profdata")).unwrap().write_all(b"#!/bin/sh").unwrap();
        set_permissions(test_bin_dir.join("llvm-profdata"), Permissions::from_mode(0o770)).unwrap();

        // Still missing llvm-cov.
        assert!(coverage(CoverageCommand {
            test_output_dir: PathBuf::from(&test_dir_path),
            clang_dir: PathBuf::from(&test_dir_path),
            symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
            src_files: Vec::new(),
        })
        .await
        .is_err());

        File::create(test_bin_dir.join("llvm-cov")).unwrap().write_all(b"#!/bin/sh").unwrap();
        set_permissions(test_bin_dir.join("llvm-cov"), Permissions::from_mode(0o770)).unwrap();

        assert_matches!(
            coverage(CoverageCommand {
                test_output_dir: PathBuf::from(&test_dir_path),
                clang_dir: PathBuf::from(&test_dir_path),
                symbol_index_json: Some(PathBuf::from(&test_symbol_index_json)),
                src_files: Vec::new(),
            })
            .await,
            Ok(())
        );
    }

    #[test]
    fn test_find_binaries_single_match() {
        let test_dir = TempDir::new().unwrap();
        create_dir(test_dir.path().join("fo")).unwrap();
        let debug_file = test_dir.path().join("fo").join("obar.debug");
        File::create(&debug_file).unwrap();

        assert_eq!(
            find_binaries(
                &SymbolIndex {
                    build_id_dirs: vec![BuildIdDir {
                        path: test_dir.path().to_str().unwrap().to_string(),
                        build_dir: None,
                    }],
                    includes: Vec::new(),
                    ids_txts: Vec::new(),
                    gcs_flat: Vec::new(),
                },
                &PathBuf::new(),   // llvm_profdata_bin, unused in test
                &[PathBuf::new()], // profraws, actual values don't matter
                |_: &Path, _: &Path| Ok("foobar".to_string()),
            )
            .unwrap(),
            vec![debug_file],
        )
    }

    #[test]
    fn test_find_binaries_multiple_matches() {
        let test_dir1 = TempDir::new().unwrap();
        create_dir(test_dir1.path().join("fo")).unwrap();
        let debug_file1 = test_dir1.path().join("fo").join("obar.debug");
        File::create(&debug_file1).unwrap();

        let test_dir2 = TempDir::new().unwrap();
        create_dir(test_dir2.path().join("ba")).unwrap();
        let debug_file2 = test_dir2.path().join("ba").join("rbaz.debug");
        File::create(&debug_file2).unwrap();

        let mut test_bin_ids = vec!["foobar", "barbaz"];
        assert_eq!(
            find_binaries(
                &SymbolIndex {
                    build_id_dirs: vec![
                        BuildIdDir {
                            path: test_dir1.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        },
                        BuildIdDir {
                            path: test_dir2.path().to_str().unwrap().to_string(),
                            build_dir: None,
                        },
                    ],
                    includes: Vec::new(),
                    ids_txts: Vec::new(),
                    gcs_flat: Vec::new(),
                },
                &PathBuf::new(), // llvm_profdata_bin, unused in test
                &[PathBuf::new(), PathBuf::new()], // profraws, actual values don't matter
                |_: &Path, _: &Path| Ok(test_bin_ids.remove(0).to_string()),
            )
            .unwrap(),
            vec![debug_file1, debug_file2],
        )
    }

    #[test]
    fn test_find_binaries_no_matches() {
        let test_dir = TempDir::new().unwrap();
        assert!(find_binaries(
            &SymbolIndex {
                build_id_dirs: vec![BuildIdDir {
                    path: test_dir.path().to_str().unwrap().to_string(),
                    build_dir: None,
                }],
                includes: Vec::new(),
                ids_txts: Vec::new(),
                gcs_flat: Vec::new(),
            },
            &PathBuf::new(),   // llvm_profdata_bin, unused in test
            &[PathBuf::new()], // profraws, actual values don't matter
            |_: &Path, _: &Path| Ok("foobar".to_string()),
        )
        .is_err())
    }

    #[test]
    fn test_find_binaries_show_id_err() {
        assert!(find_binaries(
            &SymbolIndex {
                build_id_dirs: Vec::new(),
                includes: Vec::new(),
                ids_txts: Vec::new(),
                gcs_flat: Vec::new(),
            },
            &PathBuf::new(),   // llvm_profdata_bin, unused in test
            &[PathBuf::new()], // profraws, actual values don't matter
            |_: &Path, _: &Path| Err(anyhow!("test err")),
        )
        .is_err())
    }

    #[test]
    fn test_find_binaries_id_too_short() {
        assert!(find_binaries(
            &SymbolIndex {
                build_id_dirs: Vec::new(),
                includes: Vec::new(),
                ids_txts: Vec::new(),
                gcs_flat: Vec::new(),
            },
            &PathBuf::new(),   // llvm_profdata_bin, unused in test
            &[PathBuf::new()], // profraws, actual values don't matter
            |_: &Path, _: &Path| Ok("a".to_string()),
        )
        .is_err())
    }
}
