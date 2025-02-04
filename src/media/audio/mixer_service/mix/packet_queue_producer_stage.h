// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_PACKET_QUEUE_PRODUCER_STAGE_H_
#define SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_PACKET_QUEUE_PRODUCER_STAGE_H_

#include <fidl/fuchsia.audio.mixer/cpp/wire.h>
#include <lib/fpromise/result.h>
#include <lib/zx/time.h>

#include <deque>
#include <optional>
#include <utility>

#include "src/media/audio/mixer_service/common/basic_types.h"
#include "src/media/audio/mixer_service/mix/packet_view.h"
#include "src/media/audio/mixer_service/mix/producer_stage.h"
#include "src/media/audio/mixer_service/mix/ptr_decls.h"

namespace media_audio_mixer_service {

class PacketQueueProducerStage : public ProducerStage {
 public:
  PacketQueueProducerStage(Format format, zx_koid_t reference_clock_koid)
      : ProducerStage("PacketQueueProducerStage", format, reference_clock_koid) {}

  // Registers a callback to invoke when a packet underflows.
  // The duration estimates how late the packet was relative to the system monotonic clock.
  void SetUnderflowReporter(fit::function<void(zx::duration)> underflow_reporter) {
    underflow_reporter_ = std::move(underflow_reporter);
  }

  // Clears the queue.
  void clear() { pending_packet_queue_.clear(); }

  // Returns whether the queue is empty or not.
  bool empty() const { return pending_packet_queue_.empty(); }

  // Pushes a new `packet` into the queue with an `on_destroy_callback` to be called once the packet
  // is fully consumed.
  void push(PacketView packet, fit::callback<void()> on_destroy_callback = {}) {
    pending_packet_queue_.emplace_back(packet, std::move(on_destroy_callback));
  }

 protected:
  // Implements `PipelineStage`.
  void AdvanceImpl(Fixed frame) final;
  std::optional<Packet> ReadImpl(MixJobContext& ctx, Fixed start_frame, int64_t frame_count) final;

 private:
  class PendingPacket : public PacketView {
   public:
    PendingPacket(PacketView view, fit::callback<void()> destructor)
        : PacketView(view), destructor_(std::move(destructor)) {}

    ~PendingPacket() {
      if (destructor_) {
        destructor_();
      }
    }

    PendingPacket(PendingPacket&& rhs) = default;
    PendingPacket& operator=(PendingPacket&& rhs) = default;

    PendingPacket(const PendingPacket& rhs) = delete;
    PendingPacket& operator=(const PendingPacket& rhs) = delete;

   private:
    friend class PacketQueueProducerStage;

    fit::callback<void()> destructor_;
    bool seen_in_read_ = false;
  };

  // Reports underflow of `underlow_frame_count`.
  void ReportUnderflow(Fixed underlow_frame_count);

  std::deque<PendingPacket> pending_packet_queue_;

  size_t underflow_count_;
  fit::function<void(zx::duration)> underflow_reporter_;
};

}  // namespace media_audio_mixer_service

#endif  // SRC_MEDIA_AUDIO_MIXER_SERVICE_MIX_PACKET_QUEUE_PRODUCER_STAGE_H_
