// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/media/audio/mixer_service/mix/ring_buffer_producer_stage.h"

#include <lib/fzl/vmo-mapper.h>
#include <lib/zx/time.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/media/audio/mixer_service/common/basic_types.h"
#include "src/media/audio/mixer_service/mix/packet_view.h"
#include "src/media/audio/mixer_service/mix/testing/defaults.h"

namespace media_audio_mixer_service {
namespace {

using ::fuchsia_mediastreams::wire::AudioSampleFormat;

const Format kFormat = Format::CreateOrDie({AudioSampleFormat::kFloat, 2, 48000});
const int64_t kFrameCount = 480;

class RingBufferProducerStageTest : public ::testing::Test {
 public:
  RingBufferProducerStageTest() {
    fzl::VmoMapper vmo_mapper_;
    const auto status =
        vmo_mapper_.CreateAndMap(zx_system_get_page_size(), ZX_VM_PERM_READ | ZX_VM_PERM_WRITE);
    FX_CHECK(status == ZX_OK);
    ring_buffer_producer_stage_.emplace(kFormat, DefaultClockKoid(), std::move(vmo_mapper_),
                                        kFrameCount, [this]() { return safe_read_frame_; });
  }

  RingBufferProducerStage& ring_buffer_producer_stage() { return *ring_buffer_producer_stage_; }

  void SetSafeReadFrame(int64_t safe_read_frame) { safe_read_frame_ = safe_read_frame; }

 private:
  std::optional<RingBufferProducerStage> ring_buffer_producer_stage_;
  int64_t safe_read_frame_ = -1;
};

TEST_F(RingBufferProducerStageTest, ReadBeyondSafeReadFrame) {
  auto& ring_buffer = ring_buffer_producer_stage();

  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(0), 1);
  EXPECT_FALSE(packet.has_value());
}

TEST_F(RingBufferProducerStageTest, ReadFullyExpiredPacket) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 960.
  SetSafeReadFrame(959);

  // The first 480 frames should be now unavailable.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(0), 480);
  EXPECT_FALSE(packet.has_value());
}

TEST_F(RingBufferProducerStageTest, ReadNotYetAvailablePacket) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 480.
  SetSafeReadFrame(479);

  // The frames after 480 should not be available yet.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(480), 1);
  EXPECT_FALSE(packet.has_value());
}

TEST_F(RingBufferProducerStageTest, ReadFullyAvailableRegion) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 48.
  SetSafeReadFrame(47);

  // All 48 frames should be returned.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(0), 48);
  ASSERT_TRUE(packet);
  EXPECT_EQ(packet->start(), Fixed(0));
  EXPECT_EQ(packet->length(), 48);
}

TEST_F(RingBufferProducerStageTest, ReadPartiallyAvailableRegion) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 48.
  SetSafeReadFrame(47);

  // Only the first 48 frames of 96 requested frames should be returned.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(0), 96);
  ASSERT_TRUE(packet);
  EXPECT_EQ(packet->start(), Fixed(0));
  EXPECT_EQ(packet->length(), 48);
}

TEST_F(RingBufferProducerStageTest, ReadSkipsExpiredFrames) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 480 + 48 to wrap around the ring.
  SetSafeReadFrame(527);

  // The first 48 expired frames should be skipped.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(0), 96);
  ASSERT_TRUE(packet);
  EXPECT_EQ(packet->start(), Fixed(48));
  EXPECT_EQ(packet->length(), 48);
}

TEST_F(RingBufferProducerStageTest, ReadAfterTruncatePacketAtEndOfTheRing) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame 480 + 48 to wrap around the ring.
  SetSafeReadFrame(527);

  // The returned packet should be truncated beyond the end of the ring.
  {
    const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(432), 96);
    ASSERT_TRUE(packet);
    EXPECT_EQ(packet->start(), Fixed(432));
    EXPECT_EQ(packet->length(), 48);
  }
  // Now read that last 48 frames at the start of the ring.
  {
    const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(480), 48);
    ASSERT_TRUE(packet);
    EXPECT_EQ(packet->start(), Fixed(480));
    EXPECT_EQ(packet->length(), 48);
  }
}

TEST_F(RingBufferProducerStageTest, ReadNegativeFrames) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // Advance the safe read frame just before frame -480.
  SetSafeReadFrame(-481);

  // All 10 frames should be available and returned.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(-500), 10);
  ASSERT_TRUE(packet);
  EXPECT_EQ(packet->start(), Fixed(-500));
  EXPECT_EQ(packet->length(), 10);
}

TEST_F(RingBufferProducerStageTest, ReadNegativeThroughPositiveFrames) {
  auto& ring_buffer = ring_buffer_producer_stage();

  // First 5 frames should be available and returned.
  const auto packet = ring_buffer.Read(DefaultCtx(), Fixed(-5), 10);
  ASSERT_TRUE(packet);
  EXPECT_EQ(packet->start(), Fixed(-5));
  EXPECT_EQ(packet->length(), 5);
}

}  // namespace
}  // namespace media_audio_mixer_service
