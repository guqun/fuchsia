// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "decoder_fuzzer.h"

#include <fuzzer/FuzzedDataProvider.h>

static int global_display_ptr;

VADisplay vaGetDisplayMagma(magma_device_t device) { return &global_display_ptr; }

// A version of fbl::round_up that works on CheckedNumerics.
template <typename T>
static T RoundUp(T a, T b) {
  return (a + b - 1) / b * b;
}

void FakeCodecAdapterEvents::onCoreCodecFailCodec(const char *format, ...) {
  va_list args;
  va_start(args, format);
  printf("Got onCoreCodecFailCodec: ");
  vprintf(format, args);
  printf("\n");
  fflush(stdout);
  va_end(args);

  std::lock_guard<std::mutex> lock(lock_);
  fail_codec_count_++;
  cond_.notify_all();
}

void FakeCodecAdapterEvents::onCoreCodecFailStream(fuchsia::media::StreamError error) {
  printf("Got onCoreCodecFailStream %d\n", static_cast<int>(error));
  fflush(stdout);
  std::lock_guard<std::mutex> lock(lock_);
  fail_stream_count_++;
  cond_.notify_all();
}

void FakeCodecAdapterEvents::onCoreCodecResetStreamAfterCurrentFrame() {}

void FakeCodecAdapterEvents::onCoreCodecMidStreamOutputConstraintsChange(
    bool output_re_config_required) {
  owner_->onCoreCodecMidStreamOutputConstraintsChange(output_re_config_required);
}

void FakeCodecAdapterEvents::onCoreCodecOutputFormatChange() {}

void FakeCodecAdapterEvents::onCoreCodecInputPacketDone(CodecPacket *packet) {
  std::lock_guard lock(lock_);
  input_packets_done_.push_back(packet);
  cond_.notify_all();
}

void FakeCodecAdapterEvents::onCoreCodecOutputPacket(CodecPacket *packet,
                                                     bool error_detected_before,
                                                     bool error_detected_during) {
  auto output_format = codec_adapter_->CoreCodecGetOutputFormat(1u, 1u);
}

void FakeCodecAdapterEvents::onCoreCodecOutputEndOfStream(bool error_detected_before) {
  std::lock_guard lock(lock_);
  end_of_stream_count_++;
  cond_.notify_all();
}

void FakeCodecAdapterEvents::onCoreCodecLogEvent(
    media_metrics::StreamProcessorEvents2MetricDimensionEvent event_code) {}

void FakeCodecAdapterEvents::WaitForIdle(size_t input_packet_count, bool set_end_of_stream) {
  std::unique_lock<std::mutex> lock(lock_);
  cond_.wait_for(lock, std::chrono::milliseconds(50), [&]() FXL_NO_THREAD_SAFETY_ANALYSIS {
    if (set_end_of_stream) {
      if (end_of_stream_count_ > 0)
        return true;
    } else {
      if (input_packets_done_.size() == input_packet_count)
        return true;
    }
    return fail_codec_count_ > 0 || fail_stream_count_ > 0;
  });
}

VaapiFuzzerTestFixture::~VaapiFuzzerTestFixture() { decoder_.reset(); }

void VaapiFuzzerTestFixture::SetUp() {
  EXPECT_TRUE(VADisplayWrapper::InitializeSingletonForTesting());

  vaDefaultStubSetReturn();

  // Have to defer the construction of decoder_ until
  // VADisplayWrapper::InitializeSingletonForTesting is called
  decoder_ = std::make_unique<CodecAdapterVaApiDecoder>(lock_, &events_);
  events_.set_codec_adapter(decoder_.get());
}

void VaapiFuzzerTestFixture::CodecAndStreamInit(std::string mime_type) {
  fuchsia::media::FormatDetails format_details;
  format_details.set_format_details_version_ordinal(1);
  format_details.set_mime_type(mime_type);
  decoder_->CoreCodecInit(format_details);

  auto input_constraints = decoder_->CoreCodecGetBufferCollectionConstraints(
      CodecPort::kInputPort, fuchsia::media::StreamBufferConstraints(),
      fuchsia::media::StreamBufferPartialSettings());
  EXPECT_TRUE(input_constraints.buffer_memory_constraints.cpu_domain_supported);

  decoder_->CoreCodecStartStream();
  decoder_->CoreCodecQueueInputFormatDetails(format_details);
}

void VaapiFuzzerTestFixture::CodecStreamStop() {
  decoder_->CoreCodecStopStream();
  decoder_->CoreCodecEnsureBuffersNotConfigured(CodecPort::kOutputPort);
}

void VaapiFuzzerTestFixture::ParseDataIntoInputPackets(FuzzedDataProvider &provider) {
  constexpr uint32_t kMaxInputPackets = 32;
  uint32_t input_packets = 0;

  while ((input_packets < kMaxInputPackets) && (provider.remaining_bytes() > 0)) {
    std::string str = provider.ConsumeRandomLengthString(std::numeric_limits<uint32_t>::max());

    // CodecImpl validates that the size > 0.
    if (!str.empty()) {
      auto input_buffer = std::make_unique<CodecBufferForTest>(str.size(), 0, false);
      std::memcpy(input_buffer->base(), str.data(), str.size());

      auto input_packet = std::make_unique<CodecPacketForTest>(input_packets);
      input_packet->SetStartOffset(0);
      input_packet->SetValidLengthBytes(static_cast<uint32_t>(str.size()));
      input_packet->SetBuffer(input_buffer.get());
      decoder_->CoreCodecQueueInputPacket(input_packet.get());
      input_buffers_.push_back(std::move(input_buffer));
      input_packets_.push_back(std::move(input_packet));

      input_packets += 1;
    }
  }
}

void VaapiFuzzerTestFixture::ConfigureOutputBuffers(uint32_t output_packet_count,
                                                    size_t output_packet_size) {
  auto test_packets = Packets(output_packet_count);
  test_buffers_ = Buffers(std::vector<size_t>(output_packet_count, output_packet_size));

  test_packets_ = std::vector<std::unique_ptr<CodecPacket>>(output_packet_count);
  for (size_t i = 0; i < output_packet_count; i++) {
    auto &packet = test_packets.packets[i];
    test_packets_[i] = std::move(packet);
    decoder_->CoreCodecAddBuffer(CodecPort::kOutputPort, test_buffers_.buffers[i].get());
  }

  decoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
  for (size_t i = 0; i < output_packet_count; i++) {
    decoder_->CoreCodecRecycleOutputPacket(test_packets_[i].get());
  }

  decoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
}

void VaapiFuzzerTestFixture::RunFuzzer(std::string mime_type, const uint8_t *data, size_t size) {
  CodecAndStreamInit(mime_type);

  FuzzedDataProvider provider(data, size);
  // Test both with and without sending end of stream after all the data.
  // * Test with to help ensure that the decoder is attempting to decode all the data.
  // * Test without to double-check that tearing down without an end of stream doesn't cause issues.
  bool set_end_of_stream = provider.ConsumeBool();

  ParseDataIntoInputPackets(provider);
  if (set_end_of_stream) {
    decoder_->CoreCodecQueueInputEndOfStream();
  }
  events_.WaitForIdle(input_packets_.size(), set_end_of_stream);

  // Wait a tiny bit more to increase the chance of detecting teardown issues.
  std::this_thread::sleep_for(std::chrono::milliseconds(1));

  CodecStreamStop();
}

void VaapiFuzzerTestFixture::onCoreCodecMidStreamOutputConstraintsChange(
    bool output_re_config_required) {
  // Test a representative value.
  auto output_constraints = decoder_->CoreCodecGetBufferCollectionConstraints(
      CodecPort::kOutputPort, fuchsia::media::StreamBufferConstraints(),
      fuchsia::media::StreamBufferPartialSettings());
  EXPECT_TRUE(output_constraints.buffer_memory_constraints.cpu_domain_supported);

  // Fake out the client setting buffer constraints on sysmem
  fuchsia::sysmem::BufferCollectionInfo_2 buffer_collection;
  buffer_collection.settings.image_format_constraints =
      output_constraints.image_format_constraints.at(0);
  decoder_->CoreCodecSetBufferCollectionInfo(CodecPort::kOutputPort, buffer_collection);

  // Should be enough to handle a large fraction of bear.h264 output without recycling.
  constexpr uint32_t output_packet_count = 35;

  const auto &image_constraints = output_constraints.image_format_constraints[0];
  auto out_width = RoundUp(safemath::MakeCheckedNum(image_constraints.required_max_coded_width),
                           safemath::MakeCheckedNum(image_constraints.coded_width_divisor));
  auto out_width_stride =
      RoundUp(out_width, safemath::MakeCheckedNum(image_constraints.bytes_per_row_divisor));
  auto out_height = RoundUp(safemath::MakeCheckedNum(image_constraints.required_max_coded_height),
                            safemath::MakeCheckedNum(image_constraints.coded_height_divisor));

  auto main_plane_size = out_width_stride * out_height;
  auto uv_plane_size = main_plane_size / 2;
  auto pic_size_checked = main_plane_size + uv_plane_size;
  if (!pic_size_checked.IsValid()) {
    return;
  }
  size_t pic_size_bytes = pic_size_checked.ValueOrDie();

  // Place an arbitrary cap on the size to avoid OOMs when allocating output buffers and to reduce
  // the amount of test time spent allocating memory.
  constexpr uint32_t kMaxBufferSize = 1024 * 1024;
  if (pic_size_bytes > kMaxBufferSize) {
    return;
  }
  const size_t output_packet_size = pic_size_bytes;
  auto test_packets = Packets(output_packet_count);
  test_buffers_ = Buffers(std::vector<size_t>(output_packet_count, output_packet_size));

  test_packets_ = std::vector<std::unique_ptr<CodecPacket>>(output_packet_count);
  for (size_t i = 0; i < output_packet_count; i++) {
    auto &packet = test_packets.packets[i];
    test_packets_[i] = std::move(packet);
    decoder_->CoreCodecAddBuffer(CodecPort::kOutputPort, test_buffers_.buffers[i].get());
  }

  decoder_->CoreCodecConfigureBuffers(CodecPort::kOutputPort, test_packets_);
  for (size_t i = 0; i < output_packet_count; i++) {
    decoder_->CoreCodecRecycleOutputPacket(test_packets_[i].get());
  }

  decoder_->CoreCodecMidStreamOutputBufferReConfigFinish();
}
