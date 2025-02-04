// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_MEDIA_AUDIO_LIB_CLOCK_SYNTHETIC_CLOCK_H_
#define SRC_MEDIA_AUDIO_LIB_CLOCK_SYNTHETIC_CLOCK_H_

#include <lib/zircon-internal/thread_annotations.h>
#include <lib/zx/clock.h>

#include <memory>
#include <mutex>
#include <optional>
#include <string>

#include "src/media/audio/lib/clock/clock.h"

namespace media_audio {

class SyntheticClockRealm;

// A synthetic clock. Time advances on request only. See SyntheticClockRealm.
//
// All methods are safe to call from any thread.
class SyntheticClock : public Clock {
 public:
  std::string_view name() const override { return name_; }
  zx_koid_t koid() const override { return koid_; }
  uint32_t domain() const override { return domain_; }
  bool adjustable() const override { return adjustable_; }

  zx::time now() const override;
  ToClockMonoSnapshot to_clock_mono_snapshot() const override;
  void SetRate(int32_t rate_adjust_ppm) override;
  std::optional<zx::clock> DuplicateZxClockReadOnly() const override;

 private:
  friend class SyntheticClockRealm;

  static zx::time MonoToRef(const media::TimelineFunction& to_clock_mono, zx::time mono_time) {
    return zx::time(to_clock_mono.ApplyInverse(mono_time.get()));
  }

  static std::shared_ptr<SyntheticClock> Create(std::string_view name, uint32_t domain,
                                                bool adjustable,
                                                std::shared_ptr<const SyntheticClockRealm> realm,
                                                media::TimelineFunction to_clock_mono);

  SyntheticClock(std::string_view name, zx::clock clock, zx_koid_t koid, uint32_t domain,
                 bool adjustable, std::shared_ptr<const SyntheticClockRealm> realm,
                 media::TimelineFunction to_clock_mono)
      : name_(name),
        zx_clock_(std::move(clock)),
        koid_(koid),
        domain_(domain),
        adjustable_(adjustable),
        realm_(std::move(realm)),
        to_clock_mono_(to_clock_mono) {}

  const std::string name_;
  const zx::clock zx_clock_;
  const zx_koid_t koid_;
  const uint32_t domain_;
  const bool adjustable_;
  const std::shared_ptr<const SyntheticClockRealm> realm_;

  mutable std::mutex mutex_;
  media::TimelineFunction to_clock_mono_ TA_GUARDED(mutex_);
  int64_t generation_ TA_GUARDED(mutex_) = 0;
};

// Creates and controls a collection of synthetic clocks. Each realm has its own, isolated,
// synthetic monotonic clock, which advances on demand (see `AdvanceTo` and `AdvanceBy`). Within a
// realm, all clocks advance atomically relative to the realm's synthetic montonic clock.
//
// All methods are safe to call from any thread.
class SyntheticClockRealm : public std::enable_shared_from_this<SyntheticClockRealm> {
 public:
  // Create a new realm with `now() == zx::time(0)`.
  [[nodiscard]] static std::shared_ptr<SyntheticClockRealm> Create();

  // Creates a new clock. The clock starts starts with the given `to_clock_mono` transformation (by
  // default, the identity transform).
  [[nodiscard]] std::shared_ptr<SyntheticClock> CreateClock(
      std::string_view name, uint32_t domain, bool adjustable,
      media::TimelineFunction to_clock_mono = media::TimelineFunction(0, 0, 1, 1));

  // The current synthetic monotonic time.
  zx::time now() const;

  // Advance now to the given monotonic time.
  // Requires: `mono_now > now()`
  void AdvanceTo(zx::time mono_now);

  // Advance now by the given duration.
  // Requires: `mono_diff > 0`
  void AdvanceBy(zx::duration mono_diff);

 private:
  void AdvanceToImpl(zx::time mono_now) TA_REQ(mutex_);
  SyntheticClockRealm() = default;

  mutable std::mutex mutex_;
  zx::time mono_now_ TA_GUARDED(mutex_);
};

}  // namespace media_audio

#endif  // SRC_MEDIA_AUDIO_LIB_CLOCK_SYNTHETIC_CLOCK_H_
