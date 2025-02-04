// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// This module tests eager packages.
use {
    assert_matches::assert_matches,
    fidl_fuchsia_pkg::{CupData, GetInfoError, ResolveError},
    fuchsia_async as fasync,
    fuchsia_pkg_testing::{PackageBuilder, RepositoryBuilder, SystemImageBuilder},
    fuchsia_url::PinnedAbsolutePackageUrl,
    lib::{
        get_cup_response_with_name, make_pkg_with_extra_blobs, pkgfs_with_system_image_and_pkg,
        MountsBuilder, TestEnvBuilder, EMPTY_REPO_PATH,
    },
    omaha_client::{
        cup_ecdsa::{
            test_support::{
                make_default_json_public_keys_for_test, make_default_public_key_id_for_test,
                make_expected_signature_for_test, make_keys_for_test, make_public_keys_for_test,
                make_standard_intermediate_for_test,
            },
            Cupv2RequestHandler, PublicKeyId, StandardCupv2Handler,
        },
        protocol::request::Request,
    },
    std::sync::Arc,
};

fn make_cup_data(cup_response: &[u8]) -> CupData {
    let (priv_key, public_key) = make_keys_for_test();
    let public_key_id: PublicKeyId = make_default_public_key_id_for_test();
    let public_keys = make_public_keys_for_test(public_key_id, public_key);
    let cup_handler = StandardCupv2Handler::new(&public_keys);
    let request = Request::default();
    let mut intermediate = make_standard_intermediate_for_test(request);
    let request_metadata = cup_handler.decorate_request(&mut intermediate).unwrap();
    let request_body = intermediate.serialize_body().unwrap();
    let expected_signature: Vec<u8> =
        make_expected_signature_for_test(&priv_key, &request_metadata, &cup_response);
    fidl_fuchsia_pkg_ext::CupData::builder()
        .key_id(Some(public_key_id))
        .nonce(Some(request_metadata.nonce.into()))
        .request(Some(request_body))
        .response(Some(cup_response.to_vec()))
        .signature(Some(expected_signature))
        .build()
        .into()
}

#[fasync::run_singlethreaded(test)]
async fn test_empty_eager_config() {
    let package_name = "test-package";
    let pkg = PackageBuilder::new(package_name)
        .add_resource_at("test_file", "test-file-content".as_bytes())
        .build()
        .await
        .unwrap();

    let repo = Arc::new(
        RepositoryBuilder::from_template_dir(EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );

    let eager_config = serde_json::json!({
        "packages": [],
    });

    let env = TestEnvBuilder::new()
        .mounts(
            MountsBuilder::new()
                .custom_config_data("eager_package_config.json", eager_config.to_string())
                .enable_dynamic_config(lib::EnableDynamicConfig {
                    enable_dynamic_configuration: true,
                })
                .build(),
        )
        .build()
        .await;

    let served_repository = Arc::clone(&repo).server().start().unwrap();

    let repo_url = "fuchsia-pkg://test".parse().unwrap();
    let repo_config = served_repository.make_repo_config(repo_url);

    let () = env.proxies.repo_manager.add(repo_config.into()).await.unwrap().unwrap();

    let package = env
        .resolve_package(format!("fuchsia-pkg://test/{}", package_name).as_str())
        .await
        .expect("package to resolve without error");

    // Verify the served package directory contains the exact expected contents.
    pkg.verify_contents(&package).await.unwrap();

    env.stop().await;
}

#[fasync::run_singlethreaded(test)]
async fn test_eager_resolve_package() {
    let pkg_name = "test-package";
    let pkg = make_pkg_with_extra_blobs(&pkg_name, 0).await;
    let pkg_url = PinnedAbsolutePackageUrl::parse(&format!(
        "fuchsia-pkg://example.com/{}?hash={}",
        pkg_name,
        pkg.meta_far_merkle_root()
    ))
    .unwrap();

    let eager_config = serde_json::json!({
        "packages": [
            {
                "url": pkg_url.as_unpinned(),
                "public_keys": make_default_json_public_keys_for_test(),
            }
        ]
    });

    let system_image_package = SystemImageBuilder::new().build().await;
    let pkgfs = pkgfs_with_system_image_and_pkg(&system_image_package, Some(&pkg)).await;

    let cup_response = get_cup_response_with_name(&pkg_url);
    let cup_data: CupData = make_cup_data(&cup_response);

    let env = TestEnvBuilder::new()
        .pkgfs(pkgfs)
        .mounts(
            MountsBuilder::new()
                .eager_packages(vec![(pkg_url.clone(), cup_data.clone())])
                .custom_config_data("eager_package_config.json", eager_config.to_string())
                .enable_dynamic_config(lib::EnableDynamicConfig {
                    enable_dynamic_configuration: true,
                })
                .build(),
        )
        .build()
        .await;

    let package = env
        .resolve_package(format!("fuchsia-pkg://example.com/{}", pkg_name).as_str())
        .await
        .expect("package to resolve without error");

    // Verify the served package directory contains the exact expected contents.
    pkg.verify_contents(&package).await.unwrap();

    env.stop().await;
}

#[fasync::run_singlethreaded(test)]
async fn test_eager_get_hash() {
    let pkg_name = "test-package";
    let pkg = make_pkg_with_extra_blobs(&pkg_name, 0).await;
    let pkg_url = PinnedAbsolutePackageUrl::parse(&format!(
        "fuchsia-pkg://example.com/{}?hash={}",
        pkg_name,
        pkg.meta_far_merkle_root()
    ))
    .unwrap();

    let eager_config = serde_json::json!({
        "packages": [
            {
                "url": pkg_url.as_unpinned(),
                "public_keys": make_default_json_public_keys_for_test(),
            }
        ],
    });

    let system_image_package = SystemImageBuilder::new().build().await;
    let pkgfs = pkgfs_with_system_image_and_pkg(&system_image_package, Some(&pkg)).await;

    let cup_response = get_cup_response_with_name(&pkg_url);
    let cup_data: CupData = make_cup_data(&cup_response);

    let env = TestEnvBuilder::new()
        .pkgfs(pkgfs)
        .mounts(
            MountsBuilder::new()
                .eager_packages(vec![(pkg_url.clone(), cup_data.clone())])
                .custom_config_data("eager_package_config.json", eager_config.to_string())
                .enable_dynamic_config(lib::EnableDynamicConfig {
                    enable_dynamic_configuration: true,
                })
                .build(),
        )
        .build()
        .await;

    let package = env.get_hash("fuchsia-pkg://example.com/test-package").await;

    assert_eq!(package.unwrap(), pkg.meta_far_merkle_root().clone().into());

    env.stop().await;
}

#[fasync::run_singlethreaded(test)]
async fn test_cup_write() {
    let pkg_name = "test-package";
    let pkg = make_pkg_with_extra_blobs(&pkg_name, 0).await;
    let pkg_url = PinnedAbsolutePackageUrl::parse(&format!(
        "fuchsia-pkg://example.com/{}?hash={}",
        pkg_name,
        pkg.meta_far_merkle_root()
    ))
    .unwrap();

    let repo = Arc::new(
        RepositoryBuilder::from_template_dir(EMPTY_REPO_PATH)
            .add_package(&pkg)
            .build()
            .await
            .unwrap(),
    );
    let served_repository = Arc::clone(&repo).server().start().unwrap();

    let repo_config = served_repository.make_repo_config(pkg_url.repository().clone());

    let eager_config = serde_json::json!({
        "packages": [
            {
                "url": pkg_url.as_unpinned(),
                "public_keys": make_default_json_public_keys_for_test(),
            }
        ]
    });

    let env = TestEnvBuilder::new()
        .mounts(
            MountsBuilder::new()
                .static_repository(repo_config)
                .custom_config_data("eager_package_config.json", eager_config.to_string())
                .enable_dynamic_config(lib::EnableDynamicConfig {
                    enable_dynamic_configuration: true,
                })
                .build(),
        )
        .build()
        .await;

    // can't get info or resolve before write
    assert_matches!(
        env.cup_get_info(pkg_url.as_unpinned().to_string()).await,
        Err(GetInfoError::NotAvailable)
    );
    assert_matches!(
        env.resolve_package(&pkg_url.as_unpinned().to_string()).await,
        Err(ResolveError::PackageNotFound)
    );

    let cup_response = get_cup_response_with_name(&pkg_url);
    let cup_data: CupData = make_cup_data(&cup_response);
    env.cup_write(pkg_url.to_string(), cup_data).await.unwrap();

    // now get info and resolve works
    let (version, channel) = env.cup_get_info(pkg_url.as_unpinned().to_string()).await.unwrap();
    assert_eq!(version, "1.2.3.4");
    assert_eq!(channel, "stable");

    let package = env.resolve_package(&pkg_url.as_unpinned().to_string()).await.unwrap();
    // Verify the served package directory contains the exact expected contents.
    pkg.verify_contents(&package).await.unwrap();

    env.stop().await;
}

#[fasync::run_singlethreaded(test)]
async fn test_cup_get_info_persisted() {
    let pkg_name = "test-package";
    let pkg = make_pkg_with_extra_blobs(&pkg_name, 0).await;
    let pkg_url = PinnedAbsolutePackageUrl::parse(&format!(
        "fuchsia-pkg://example.com/{}?hash={}",
        pkg_name,
        pkg.meta_far_merkle_root()
    ))
    .unwrap();

    let eager_config = serde_json::json!({
        "packages": [
            {
                "url": pkg_url.as_unpinned() ,
                "public_keys": make_default_json_public_keys_for_test(),
            }
        ],
    });

    let system_image_package = SystemImageBuilder::new().build().await;
    let pkgfs = pkgfs_with_system_image_and_pkg(&system_image_package, Some(&pkg)).await;

    let cup_response = get_cup_response_with_name(&pkg_url);
    let cup_data: CupData = make_cup_data(&cup_response);

    let env = TestEnvBuilder::new()
        .pkgfs(pkgfs)
        .mounts(
            MountsBuilder::new()
                .eager_packages(vec![(pkg_url.clone(), cup_data.clone())])
                .custom_config_data("eager_package_config.json", eager_config.to_string())
                .build(),
        )
        .build()
        .await;

    let (version, channel) = env.cup_get_info(&pkg_url.as_unpinned().to_string()).await.unwrap();

    assert_eq!(version, "1.2.3.4");
    assert_eq!(channel, "stable");

    env.stop().await;
}
