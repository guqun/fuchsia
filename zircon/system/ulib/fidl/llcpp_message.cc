// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/fidl/internal.h>
#include <lib/fidl/llcpp/message.h>
#include <lib/fidl/trace.h>
#include <zircon/assert.h>
#include <zircon/errors.h>

#include <cstring>
#include <string>

#ifdef __Fuchsia__
#include <lib/fidl/llcpp/client_base.h>
#include <lib/fidl/llcpp/internal/transport_channel.h>
#include <lib/fidl/llcpp/server.h>
#include <zircon/syscalls.h>
#else
#include <lib/fidl/llcpp/internal/transport_channel_host.h>
#endif  // __Fuchsia__

namespace fidl {

OutgoingMessage OutgoingMessage::FromEncodedCMessage(const fidl_outgoing_msg_t* c_msg) {
  return OutgoingMessage(c_msg, true);
}

OutgoingMessage OutgoingMessage::FromEncodedCValue(const fidl_outgoing_msg_t* c_msg) {
  return OutgoingMessage(c_msg, false);
}

OutgoingMessage::OutgoingMessage(const fidl_outgoing_msg_t* c_msg, bool is_transactional)
    : fidl::Status(fidl::Status::Ok()) {
  ZX_ASSERT(c_msg);
  transport_vtable_ = &internal::ChannelTransport::VTable;
  switch (c_msg->type) {
    case FIDL_OUTGOING_MSG_TYPE_IOVEC: {
      message_ = *c_msg;
      iovec_capacity_ = c_msg->iovec.num_iovecs;
      handle_capacity_ = c_msg->iovec.num_handles;
      break;
    }
    case FIDL_OUTGOING_MSG_TYPE_BYTE: {
      backing_buffer_ = reinterpret_cast<uint8_t*>(c_msg->byte.bytes);
      backing_buffer_capacity_ = c_msg->byte.num_bytes;
      converted_byte_message_iovec_ = {
          .buffer = backing_buffer_,
          .capacity = backing_buffer_capacity_,
          .reserved = 0,
      };
      message_ = {
          .type = FIDL_OUTGOING_MSG_TYPE_IOVEC,
          .iovec =
              {
                  .iovecs = &converted_byte_message_iovec_,
                  .num_iovecs = 1,
                  .handles = c_msg->byte.handles,
                  .handle_metadata = c_msg->byte.handle_metadata,
                  .num_handles = c_msg->byte.num_handles,
              },
      };
      iovec_capacity_ = 1;
      handle_capacity_ = c_msg->byte.num_handles;
      break;
    }
    default:
      ZX_PANIC("unhandled FIDL outgoing message type");
  }
  is_transactional_ = is_transactional;
}

OutgoingMessage::OutgoingMessage(const ::fidl::Status& failure)
    : fidl::Status(failure),
      message_({.type = FIDL_OUTGOING_MSG_TYPE_IOVEC,
                .iovec = {
                    .iovecs = nullptr,
                    .num_iovecs = 0,
                    .handles = nullptr,
                    .handle_metadata = nullptr,
                    .num_handles = 0,
                }}) {
  ZX_DEBUG_ASSERT(failure.status() != ZX_OK);
}

OutgoingMessage::OutgoingMessage(InternalIovecConstructorArgs args)
    : fidl::Status(fidl::Status::Ok()),
      transport_vtable_(args.transport_vtable),
      message_({
          .type = FIDL_OUTGOING_MSG_TYPE_IOVEC,
          .iovec = {.iovecs = args.iovecs,
                    .num_iovecs = 0,
                    .handles = args.handles,
                    .handle_metadata = args.handle_metadata,
                    .num_handles = 0},
      }),
      iovec_capacity_(args.iovec_capacity),
      handle_capacity_(args.handle_capacity),
      backing_buffer_capacity_(args.backing_buffer_capacity),
      backing_buffer_(args.backing_buffer),
      is_transactional_(args.is_transactional) {}

OutgoingMessage::OutgoingMessage(InternalByteBackedConstructorArgs args)
    : fidl::Status(fidl::Status::Ok()),
      transport_vtable_(args.transport_vtable),
      message_({
          .type = FIDL_OUTGOING_MSG_TYPE_IOVEC,
          .iovec =
              {
                  .iovecs = &converted_byte_message_iovec_,
                  .num_iovecs = 1,
                  .handles = args.handles,
                  .handle_metadata = args.handle_metadata,
                  .num_handles = args.num_handles,
              },
      }),
      iovec_capacity_(1),
      handle_capacity_(args.num_handles),
      backing_buffer_capacity_(args.num_bytes),
      backing_buffer_(args.bytes),
      converted_byte_message_iovec_(
          {.buffer = backing_buffer_, .capacity = backing_buffer_capacity_, .reserved = 0}),
      is_transactional_(args.is_transactional) {}

OutgoingMessage::~OutgoingMessage() {
  // We may not have a vtable when the |OutgoingMessage| represents an error.
  if (transport_vtable_) {
    transport_vtable_->encoding_configuration->close_many(handles(), handle_actual());
  }
}

fidl_outgoing_msg_t OutgoingMessage::ReleaseToEncodedCMessage() && {
  ZX_DEBUG_ASSERT(status() == ZX_OK);
  ZX_ASSERT(transport_type() == FIDL_TRANSPORT_TYPE_CHANNEL);
  fidl_outgoing_msg_t result = message_;
  ReleaseHandles();
  return result;
}

bool OutgoingMessage::BytesMatch(const OutgoingMessage& other) const {
  uint32_t iovec_index = 0, other_iovec_index = 0;
  uint32_t byte_index = 0, other_byte_index = 0;
  while (iovec_index < iovec_actual() && other_iovec_index < other.iovec_actual()) {
    zx_channel_iovec_t cur_iovec = iovecs()[iovec_index];
    zx_channel_iovec_t other_cur_iovec = other.iovecs()[other_iovec_index];
    const uint8_t* cur_bytes = reinterpret_cast<const uint8_t*>(cur_iovec.buffer);
    const uint8_t* other_cur_bytes = reinterpret_cast<const uint8_t*>(other_cur_iovec.buffer);

    uint32_t cmp_len =
        std::min(cur_iovec.capacity - byte_index, other_cur_iovec.capacity - other_byte_index);
    if (memcmp(&cur_bytes[byte_index], &other_cur_bytes[other_byte_index], cmp_len) != 0) {
      return false;
    }

    byte_index += cmp_len;
    if (byte_index == cur_iovec.capacity) {
      iovec_index++;
      byte_index = 0;
    }
    other_byte_index += cmp_len;
    if (other_byte_index == other_cur_iovec.capacity) {
      other_iovec_index++;
      other_byte_index = 0;
    }
  }
  return iovec_index == iovec_actual() && other_iovec_index == other.iovec_actual() &&
         byte_index == 0 && other_byte_index == 0;
}

void OutgoingMessage::EncodeImpl(fidl::internal::WireFormatVersion wire_format_version, void* data,
                                 size_t inline_size, fidl::internal::TopLevelEncodeFn encode_fn) {
  if (!ok()) {
    return;
  }
  if (wire_format_version != fidl::internal::WireFormatVersion::kV2) {
    SetStatus(fidl::Status::EncodeError(ZX_ERR_INVALID_ARGS, "only v2 wire format supported"));
    return;
  }

  fitx::result<fidl::Error, fidl::internal::WireEncoder::Result> result =
      fidl::internal::WireEncode(inline_size, encode_fn, transport_vtable_->encoding_configuration,
                                 data, iovecs(), iovec_capacity(), handles(),
                                 message_.iovec.handle_metadata, handle_capacity(),
                                 backing_buffer(), backing_buffer_capacity());
  if (!result.is_ok()) {
    SetStatus(result.error_value());
    return;
  }
  iovec_message().num_iovecs = static_cast<uint32_t>(result.value().iovec_actual);
  iovec_message().num_handles = static_cast<uint32_t>(result.value().handle_actual);

  if (is_transactional()) {
    ZX_ASSERT(iovec_actual() >= 1 && iovecs()[0].capacity >= sizeof(fidl_message_header_t));
    static_cast<fidl_message_header_t*>(const_cast<void*>(iovecs()[0].buffer))->at_rest_flags[0] |=
        FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2;
  }
}

void OutgoingMessage::Write(internal::AnyUnownedTransport transport, WriteOptions options) {
  if (!ok()) {
    return;
  }
  ZX_ASSERT(transport_type() == transport.type());
  ZX_ASSERT(is_transactional());
  zx_status_t status = transport.write(
      std::move(options), internal::WriteArgs{.data = iovecs(),
                                              .handles = handles(),
                                              .handle_metadata = message_.iovec.handle_metadata,
                                              .data_count = iovec_actual(),
                                              .handles_count = handle_actual()});
  ReleaseHandles();
  if (status != ZX_OK) {
    SetStatus(fidl::Status::TransportError(status));
  }
}

IncomingMessage OutgoingMessage::CallImpl(internal::AnyUnownedTransport transport,
                                          internal::MessageStorageViewBase& storage,
                                          CallOptions options) {
  if (status() != ZX_OK) {
    return IncomingMessage::Create(Status(*this));
  }
  ZX_ASSERT(transport_type() == transport.type());
  ZX_ASSERT(is_transactional());

  uint8_t* result_bytes;
  fidl_handle_t* result_handles;
  fidl_handle_metadata_t* result_handle_metadata;
  uint32_t actual_num_bytes = 0u;
  uint32_t actual_num_handles = 0u;
  internal::CallMethodArgs args = {
      .wr =
          internal::WriteArgs{
              .data = iovecs(),
              .handles = handles(),
              .handle_metadata = message_.iovec.handle_metadata,
              .data_count = iovec_actual(),
              .handles_count = handle_actual(),
          },
      .rd =
          internal::ReadArgs{
              .storage_view = &storage,
              .out_data = reinterpret_cast<void**>(&result_bytes),
              .out_handles = &result_handles,
              .out_handle_metadata = &result_handle_metadata,
              .out_data_actual_count = &actual_num_bytes,
              .out_handles_actual_count = &actual_num_handles,
          },
  };

  zx_status_t status = transport.call(std::move(options), args);
  ReleaseHandles();
  if (status != ZX_OK) {
    SetStatus(fidl::Status::TransportError(status));
    return IncomingMessage::Create(Status(*this));
  }

  return IncomingMessage(transport_vtable_, result_bytes, actual_num_bytes, result_handles,
                         result_handle_metadata, actual_num_handles);
}

OutgoingMessage::CopiedBytes::CopiedBytes(const OutgoingMessage& msg) {
  uint32_t byte_count = 0;
  for (uint32_t i = 0; i < msg.iovec_actual(); ++i) {
    byte_count += msg.iovecs()[i].capacity;
  }
  bytes_.reserve(byte_count);
  for (uint32_t i = 0; i < msg.iovec_actual(); ++i) {
    zx_channel_iovec_t iovec = msg.iovecs()[i];
    const uint8_t* buf_bytes = reinterpret_cast<const uint8_t*>(iovec.buffer);
    bytes_.insert(bytes_.end(), buf_bytes, buf_bytes + iovec.capacity);
  }
}

IncomingMessage::IncomingMessage(const internal::TransportVTable* transport_vtable, uint8_t* bytes,
                                 uint32_t byte_actual, zx_handle_t* handles,
                                 fidl_handle_metadata_t* handle_metadata, uint32_t handle_actual)
    : IncomingMessage(transport_vtable, bytes, byte_actual, handles, handle_metadata, handle_actual,
                      kSkipMessageHeaderValidation) {
  ValidateHeader();
  is_transactional_ = true;
}

IncomingMessage IncomingMessage::FromEncodedCMessage(const fidl_incoming_msg_t* c_msg) {
  return IncomingMessage(&internal::ChannelTransport::VTable,
                         reinterpret_cast<uint8_t*>(c_msg->bytes), c_msg->num_bytes, c_msg->handles,
                         c_msg->handle_metadata, c_msg->num_handles);
}

IncomingMessage::IncomingMessage(const internal::TransportVTable* transport_vtable, uint8_t* bytes,
                                 uint32_t byte_actual, zx_handle_t* handles,
                                 fidl_handle_metadata_t* handle_metadata, uint32_t handle_actual,
                                 SkipMessageHeaderValidationTag)
    : fidl::Status(fidl::Status::Ok()),
      transport_vtable_(transport_vtable),
      message_{
          .bytes = bytes,
          .handles = handles,
          .handle_metadata = handle_metadata,
          .num_bytes = byte_actual,
          .num_handles = handle_actual,
      } {}

IncomingMessage::IncomingMessage(const fidl::Status& failure) : fidl::Status(failure), message_{} {
  ZX_DEBUG_ASSERT(failure.status() != ZX_OK);
}

IncomingMessage::~IncomingMessage() { std::move(*this).CloseHandles(); }

fidl_incoming_msg_t IncomingMessage::ReleaseToEncodedCMessage() && {
  ZX_DEBUG_ASSERT(status() == ZX_OK);
  ZX_ASSERT(transport_vtable_->type == FIDL_TRANSPORT_TYPE_CHANNEL);
  fidl_incoming_msg_t result = message_;
  ReleaseHandles();
  return result;
}

void IncomingMessage::CloseHandles() && {
#ifdef __Fuchsia__
  if (handle_actual() > 0) {
    FidlHandleCloseMany(handles(), handle_actual());
  }
#else
  ZX_ASSERT(handle_actual() == 0);
#endif
  ReleaseHandles();
}

IncomingMessage IncomingMessage::SkipTransactionHeader() {
  ZX_ASSERT(is_transactional());
  fidl_handle_t* handles = message_.handles;
  fidl_handle_metadata_t* handle_metadata = message_.handle_metadata;
  uint32_t handle_actual = message_.num_handles;
  ReleaseHandles();
  return IncomingMessage(transport_vtable_, bytes() + sizeof(fidl_message_header_t),
                         byte_actual() - static_cast<uint32_t>(sizeof(fidl_message_header_t)),
                         handles, handle_metadata, handle_actual,
                         ::fidl::IncomingMessage::kSkipMessageHeaderValidation);
}

void IncomingMessage::Decode(size_t inline_size, bool contains_envelope,
                             internal::TopLevelDecodeFn decode_fn) {
  ZX_ASSERT(is_transactional_);
  // Old versions of the C bindings will send wire format V1 payloads that are compatible
  // with wire format V2 (they don't contain envelopes). Confirm that V1 payloads don't
  // contain envelopes and are compatible with V2.
  // TODO(fxbug.dev/99738) Remove this logic.
  if ((header()->at_rest_flags[0] & FIDL_MESSAGE_HEADER_AT_REST_FLAGS_0_USE_VERSION_V2) == 0 &&
      contains_envelope) {
    SetStatus(fidl::Status::DecodeError(
        ZX_ERR_INVALID_ARGS, "wire format v1 header received with unsupported envelope"));
    return;
  }
  Decode(inline_size, decode_fn, internal::WireFormatVersion::kV2, true);
}

void IncomingMessage::Decode(size_t inline_size, internal::TopLevelDecodeFn decode_fn,
                             internal::WireFormatVersion wire_format_version,
                             bool is_transactional) {
  ZX_DEBUG_ASSERT(status() == ZX_OK);
  if (wire_format_version != internal::WireFormatVersion::kV2) {
    SetStatus(fidl::Status::DecodeError(ZX_ERR_INVALID_ARGS, "only wire format v2 supported"));
    return;
  }

  fidl::Status decode_status = internal::WireDecode(
      inline_size, decode_fn, transport_vtable_->encoding_configuration, bytes(), byte_actual(),
      handles(), message_.handle_metadata, handle_actual());

  // Now the caller is responsible for the handles contained in `bytes()`.
  ReleaseHandles();
  if (!decode_status.ok()) {
    SetStatus(decode_status);
  }
}

void IncomingMessage::ValidateHeader() {
  if (byte_actual() < sizeof(fidl_message_header_t)) {
    return SetStatus(fidl::Status::UnexpectedMessage(ZX_ERR_INVALID_ARGS,
                                                     ::fidl::internal::kErrorInvalidHeader));
  }

  auto* hdr = header();
  zx_status_t status = fidl_validate_txn_header(hdr);
  if (status != ZX_OK) {
    return SetStatus(
        fidl::Status::UnexpectedMessage(status, ::fidl::internal::kErrorInvalidHeader));
  }

  // See https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0053_epitaphs?hl=en#wire_format
  if (unlikely(maybe_epitaph())) {
    if (hdr->txid != 0) {
      return SetStatus(fidl::Status::UnexpectedMessage(ZX_ERR_INVALID_ARGS,
                                                       ::fidl::internal::kErrorInvalidHeader));
    }
  }
}

OutgoingToIncomingMessage::OutgoingToIncomingMessage(OutgoingMessage& input)
    : incoming_message_(ConversionImpl(input, buf_bytes_, buf_handles_, buf_handle_metadata_)) {}

[[nodiscard]] std::string OutgoingToIncomingMessage::FormatDescription() const {
  return incoming_message_.FormatDescription();
}

IncomingMessage OutgoingToIncomingMessage::ConversionImpl(
    OutgoingMessage& input, OutgoingMessage::CopiedBytes& buf_bytes,
    std::unique_ptr<zx_handle_t[]>& buf_handles,
    // TODO(fxbug.dev/85734) Remove channel-specific logic.
    std::unique_ptr<fidl_channel_handle_metadata_t[]>& buf_handle_metadata) {
  zx_handle_t* handles = input.handles();
  fidl_channel_handle_metadata_t* handle_metadata =
      input.handle_metadata<fidl::internal::ChannelTransport>();
  uint32_t num_handles = input.handle_actual();
  input.ReleaseHandles();

  if (num_handles > ZX_CHANNEL_MAX_MSG_HANDLES) {
    FidlHandleCloseMany(handles, num_handles);
    return fidl::IncomingMessage::Create(fidl::Status::EncodeError(ZX_ERR_OUT_OF_RANGE));
  }

  // Note: it may be possible to remove these allocations.
  buf_handles = std::make_unique<zx_handle_t[]>(ZX_CHANNEL_MAX_MSG_HANDLES);
  buf_handle_metadata =
      std::make_unique<fidl_channel_handle_metadata_t[]>(ZX_CHANNEL_MAX_MSG_HANDLES);
  for (uint32_t i = 0; i < num_handles; i++) {
    const char* error;
    zx_status_t status = FidlEnsureActualHandleRights(&handles[i], handle_metadata[i].obj_type,
                                                      handle_metadata[i].rights, &error);
    if (status != ZX_OK) {
      FidlHandleCloseMany(handles, num_handles);
      FidlHandleCloseMany(buf_handles.get(), num_handles);
      return fidl::IncomingMessage::Create(fidl::Status::EncodeError(status));
    }
    buf_handles[i] = handles[i];
    buf_handle_metadata[i] = handle_metadata[i];
  }

  buf_bytes = input.CopyBytes();
  if (buf_bytes.size() > ZX_CHANNEL_MAX_MSG_BYTES) {
    FidlHandleCloseMany(handles, num_handles);
    FidlHandleCloseMany(buf_handles.get(), num_handles);
    return fidl::IncomingMessage::Create(fidl::Status::EncodeError(ZX_ERR_INVALID_ARGS));
  }

  if (input.is_transactional()) {
    return fidl::IncomingMessage::Create(buf_bytes.data(), buf_bytes.size(), buf_handles.get(),
                                         buf_handle_metadata.get(), num_handles);
  }
  return fidl::IncomingMessage::Create(buf_bytes.data(), buf_bytes.size(), buf_handles.get(),
                                       buf_handle_metadata.get(), num_handles,
                                       fidl::IncomingMessage::kSkipMessageHeaderValidation);
}

}  // namespace fidl
