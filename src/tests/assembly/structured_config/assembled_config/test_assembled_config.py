# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import argparse
import pathlib
import subprocess
import sys

from run_assembly import run_product_assembly


def main():
    parser = argparse.ArgumentParser(
        description=
        "Ensure that ffx assembly product adds structured config with the right ffx config."
    )
    parser.add_argument(
        "--ffx-bin",
        type=pathlib.Path,
        required=True,
        help="Path to ffx binary.")
    parser.add_argument(
        "--product-assembly-config",
        type=pathlib.Path,
        required=True,
        help="Path to product assembly configuration input.")
    parser.add_argument(
        "--input-bundles-dir",
        type=pathlib.Path,
        required=True,
        help="Path to input bundles directory.")
    parser.add_argument(
        "--legacy-bundle-dir",
        type=pathlib.Path,
        required=True,
        help="Path to the legacy input bundle directory.")
    parser.add_argument(
        "--outdir",
        type=pathlib.Path,
        required=True,
        help="Path to output directory.")
    parser.add_argument(
        "--stamp",
        type=pathlib.Path,
        required=True,
        help="Path to stampfile for telling ninja we're done.")
    args = parser.parse_args()

    output = run_product_assembly(
        ffx_bin=args.ffx_bin,
        product=args.product_assembly_config,
        input_bundles=args.input_bundles_dir,
        legacy_bundle=args.legacy_bundle_dir,
        outdir=args.outdir,
        extra_config=["assembly_example_enabled=true"])
    output.check_returncode()
    with open(args.stamp, 'w') as f:
        pass  # creates the file
