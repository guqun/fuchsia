// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_REACHABILITY_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_REACHABILITY_H_

#include "src/media/audio/mixer_service/fidl/node.h"

namespace media_audio_mixer_service {

// Reports whether there exists a path from `src` to `dest`. The nodes may be ordinary nodes and/or
// meta nodes. For any given meta node M, there are implicit paths from M's child input nodes, to M
// itself, to M's child output nodes.
bool ExistsPath(const Node& src, const Node& dest);

}  // namespace media_audio_mixer_service

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_FIDL_REACHABILITY_H_
