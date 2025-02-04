#!/usr/bin/env python3.8
# Copyright 2022 The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

import unittest

from eager_package_config import generate_omaha_client_config, generate_pkg_resolver_config


class TestEagerPackageConfig(unittest.TestCase):
    maxDiff = None

    def makeKeyConfig(self):
        return {
            "https://example.com":
                {
                    "latest": {
                        "id": 123,
                        "key": "foo",
                    },
                    "historical":
                        [
                            {
                                # Allow the public key ID to be a string.
                                # This is an easy mistake to make when
                                # writing a config, so we want to make sure
                                # we handle it correctly, i.e. cast string
                                # to int or fail.
                                "id": "246",
                                "key": "bar",
                            },
                            {
                                "id": 369,
                                "key": "baz",
                            },
                        ],
                },
        }

    def makeConfigs(self):
        return [
            {
                "url": "fuchsia-pkg://example.com/package_service_1",
                "default_channel": "stable",
                "flavor": "debug",
                "executable": True,
                "realms":
                    [
                        {
                            "app_id": "1a2b3c4d",
                            "channels": ["stable", "beta", "alpha"],
                        },
                        {
                            "app_id": "2b3c4d5e",
                            "channels": ["test"],
                        },
                    ],
                "service_url": "https://example.com",
            },
            {
                "url": "fuchsia-pkg://example.com/package_service_2",
                "realms": [{
                    "app_id": "5c6d7e8f",
                    "channels": ["stable"],
                }],
                "service_url": "https://example.com",
            },
            {
                "url": "fuchsia-pkg://example.com/package_noservice_1",
                "realms": [{
                    "app_id": "3c4d5e6f",
                    "channels": ["stable"],
                }],
            },
            {
                "url": "fuchsia-pkg://example.com/package_noservice_2",
                "realms": [{
                    "app_id": "4c5d6e7f",
                    "channels": ["stable"],
                }],
            },
        ]

    def test_generate_omaha_client_config(self):
        configs = self.makeConfigs()
        key_config = self.makeKeyConfig()
        self.assertEqual(
            generate_omaha_client_config(configs, key_config), {
                "eager_package_configs":
                    [
                        {
                            "server":
                                {
                                    'service_url': 'https://example.com',
                                    'public_keys':
                                        {
                                            'latest': {
                                                'id': 123,
                                                'key': 'foo',
                                            },
                                            'historical':
                                                [
                                                    {
                                                        'id': 246,
                                                        'key': 'bar',
                                                    }, {
                                                        'id': 369,
                                                        'key': 'baz',
                                                    }
                                                ],
                                        }
                                },
                            "packages":
                                [
                                    {
                                        "url":
                                            "fuchsia-pkg://example.com/package_service_1",
                                        "flavor":
                                            "debug",
                                        "channel_config":
                                            {
                                                "channels":
                                                    [
                                                        {
                                                            "name": "stable",
                                                            "repo": "stable",
                                                            "appid": "1a2b3c4d",
                                                        },
                                                        {
                                                            "name": "beta",
                                                            "repo": "beta",
                                                            "appid": "1a2b3c4d",
                                                        },
                                                        {
                                                            "name": "alpha",
                                                            "repo": "alpha",
                                                            "appid": "1a2b3c4d",
                                                        },
                                                        {
                                                            "name": "test",
                                                            "repo": "test",
                                                            "appid": "2b3c4d5e",
                                                        },
                                                    ],
                                                "default_channel": "stable",
                                            }
                                    },
                                    {
                                        "url":
                                            "fuchsia-pkg://example.com/package_service_2",
                                        "channel_config":
                                            {
                                                "channels":
                                                    [
                                                        {
                                                            "name": "stable",
                                                            "repo": "stable",
                                                            "appid": "5c6d7e8f",
                                                        },
                                                    ],
                                            }
                                    },
                                ]
                        }, {
                            "server":
                                None,
                            "packages":
                                [
                                    {
                                        "url":
                                            "fuchsia-pkg://example.com/package_noservice_1",
                                        "channel_config":
                                            {
                                                "channels":
                                                    [
                                                        {
                                                            "name": "stable",
                                                            "repo": "stable",
                                                            "appid": "3c4d5e6f",
                                                        }
                                                    ],
                                            }
                                    },
                                    {
                                        "url":
                                            "fuchsia-pkg://example.com/package_noservice_2",
                                        "channel_config":
                                            {
                                                "channels":
                                                    [
                                                        {
                                                            "name": "stable",
                                                            "repo": "stable",
                                                            "appid": "4c5d6e7f",
                                                        }
                                                    ],
                                            }
                                    },
                                ]
                        }
                    ]
            })

    def test_generate_omaha_client_config_wrong_default_channel(self):
        configs = [
            {
                "url":
                    "fuchsia-pkg://example.com/package_service_1",
                "default_channel":
                    "wrong",
                "realms":
                    [
                        {
                            "app_id": "1a2b3c4d",
                            "channels": ["stable", "beta", "alpha"]
                        }
                    ]
            }
        ]
        key_config = self.makeKeyConfig()
        with self.assertRaises(AssertionError):
            generate_omaha_client_config(configs, key_config)

    def test_generate_pkg_resolver_config(self):
        configs = self.makeConfigs()
        key_config = self.makeKeyConfig()
        self.assertEqual(
            generate_pkg_resolver_config(configs, key_config), {
                "packages":
                    [
                        {
                            "url":
                                "fuchsia-pkg://example.com/package_service_1",
                            "executable":
                                True,
                            'public_keys':
                                {
                                    'latest': {
                                        'id': 123,
                                        'key': 'foo',
                                    },
                                    'historical':
                                        [
                                            {
                                                'id': 246,
                                                'key': 'bar',
                                            }, {
                                                'id': 369,
                                                'key': 'baz',
                                            }
                                        ],
                                }
                        },
                        {
                            "url":
                                "fuchsia-pkg://example.com/package_service_2",
                            'public_keys':
                                {
                                    'latest': {
                                        'id': 123,
                                        'key': 'foo',
                                    },
                                    'historical':
                                        [
                                            {
                                                'id': 246,
                                                'key': 'bar',
                                            }, {
                                                'id': 369,
                                                'key': 'baz',
                                            }
                                        ],
                                }
                        },
                        {
                            "url":
                                "fuchsia-pkg://example.com/package_noservice_1",
                        },
                        {
                            "url":
                                "fuchsia-pkg://example.com/package_noservice_2",
                        },
                    ]
            })

    def test_reject_pinned_url(self):
        configs = [
            {
                "url":
                    "fuchsia-pkg://example.com/package?hash=deadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeefdeadbeef",
                "realms":
                    [
                        {
                            "app_id": "1a2b3c4d",
                            "channels": ["stable", "beta", "alpha"]
                        }
                    ]
            }
        ]
        key_config = self.makeKeyConfig()
        with self.assertRaises(ValueError):
            generate_omaha_client_config(configs, key_config)
