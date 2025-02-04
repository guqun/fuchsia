// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/mock-function/mock-function.h>
#include <zircon/compiler.h>

#include <iterator>
#include <memory>

#include <zxtest/zxtest.h>

extern "C" {
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/mvm/mvm.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/mvm/time-event.h"
}

#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/platform/ieee80211.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/platform/memory.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/test/fake-ucode-test.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/test/mock-trans.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/test/sim-time-event.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/test/single-ap-test.h"
#include "src/connectivity/wlan/drivers/third_party/intel/iwlwifi/test/wlan-pkt-builder.h"

namespace wlan {
namespace testing {
namespace {

// Helper function to create a PHY context for the interface.
//
static void setup_phy_ctxt(struct iwl_mvm_vif* mvmvif) __TA_NO_THREAD_SAFETY_ANALYSIS {
  // Create a PHY context and assign it to mvmvif.
  wlan_channel_t chandef = {
      // any arbitrary values
      .primary = 6,
  };
  uint16_t phy_ctxt_id;

  struct iwl_mvm* mvm = mvmvif->mvm;
  mtx_unlock(&mvm->mutex);
  ASSERT_EQ(ZX_OK, iwl_mvm_add_chanctx(mvm, &chandef, &phy_ctxt_id));
  mvmvif->phy_ctxt = &(mvm->phy_ctxts[phy_ctxt_id]);
  mtx_lock(&mvm->mutex);
}

// An iwl_rx_cmd_buffer instance that cleans up its allocated resources.
class TestRxcb : public iwl_rx_cmd_buffer {
 public:
  explicit TestRxcb(struct device* dev, void* pkt_data, size_t pkt_len) {
    struct iwl_iobuf* io_buf = nullptr;
    ASSERT_OK(iwl_iobuf_allocate_contiguous(dev, pkt_len + sizeof(struct iwl_rx_packet), &io_buf));
    _iobuf = io_buf;
    _offset = 0;

    struct iwl_rx_packet* pkt = reinterpret_cast<struct iwl_rx_packet*>(iwl_iobuf_virtual(io_buf));
    // Most fields are not cared but initialized with known values.
    pkt->len_n_flags = cpu_to_le32(0);
    pkt->hdr.cmd = 0;
    pkt->hdr.group_id = 0;
    pkt->hdr.sequence = 0;
    memcpy(pkt->data, pkt_data, pkt_len);
  }

  ~TestRxcb() { iwl_iobuf_release(_iobuf); }
};

struct TestCtx {
  wlan_rx_info_t rx_info;
  size_t frame_len;
};

class MvmTest : public SingleApTest {
 public:
  MvmTest() __TA_NO_THREAD_SAFETY_ANALYSIS {
    mvm_ = iwl_trans_get_mvm(sim_trans_.iwl_trans());
    mvmvif_ = reinterpret_cast<struct iwl_mvm_vif*>(calloc(1, sizeof(struct iwl_mvm_vif)));
    mvmvif_->mvm = mvm_;
    mvmvif_->mac_role = WLAN_MAC_ROLE_CLIENT;
    mvmvif_->ifc.ops = reinterpret_cast<wlan_softmac_ifc_protocol_ops_t*>(
        calloc(1, sizeof(wlan_softmac_ifc_protocol_ops_t)));
    mvm_->mvmvif[0] = mvmvif_;
    mvm_->vif_count++;

    mtx_lock(&mvm_->mutex);
  }

  ~MvmTest() __TA_NO_THREAD_SAFETY_ANALYSIS {
    free(mvmvif_->ifc.ops);
    free(mvmvif_);
    mtx_unlock(&mvm_->mutex);
  }

 protected:
  // This function is kind of dirty. It hijacks the wlan_softmac_ifc_protocol_t.recv() so that we
  // can save the rx_info passed to MLME.  See TearDown() for cleanup logic related to this
  // function.
  void MockRecv(TestCtx* ctx) {
    // TODO(fxbug.dev/43218): replace rxq->napi with interface instance so that we can map to
    // mvmvif.
    mvmvif_->ifc.ctx = ctx;  // 'ctx' was used as 'wlan_softmac_ifc_protocol_t*', but we override it
                             // with 'TestCtx*'.
    mvmvif_->ifc.ops->recv = [](void* ctx, const wlan_rx_packet_t* packet) {
      TestCtx* test_ctx = reinterpret_cast<TestCtx*>(ctx);
      test_ctx->rx_info = packet->info;
      test_ctx->frame_len = packet->mac_frame_size;
    };
  }

  struct iwl_mvm* mvm_;
  struct iwl_mvm_vif* mvmvif_;
};

TEST_F(MvmTest, GetMvm) { EXPECT_NE(mvm_, nullptr); }

// In this test case, we expect the CCMP is removed.
//
// Before:
//
//   Frame Header
//   CCMP
//   Payload
//
// After:
//
//   Frame Header
//   Payload
//
TEST_F(MvmTest, testCreatePacket) {
  struct wlan_rx_info rx_info = {};

  const size_t kMacPayloadLen = 60;
  struct {
    struct iwl_rx_mpdu_res_start rx_res;
    struct ieee80211_frame_header frame;
    uint8_t ccmp[fuchsia_wlan_ieee80211_CCMP_HDR_LEN];
    uint8_t mac_payload[kMacPayloadLen];
    uint32_t rx_pkt_status;
  } __packed mpdu = {
      .rx_res =
          {
              .byte_count = kMacPayloadLen,
              .assist = 0,
          },
      .frame = {},
      .ccmp =
          {
              0x01,
              0x02,
              0x03,
              0x04,
              0x05,
              0x06,
              0x07,
              0x08,
          },
      .mac_payload =
          {
              0xff,
              0xfe,
              0xfd,
              0xfc,
          },
      .rx_pkt_status = 0x0,
  };
  TestRxcb mpdu_rxcb(sim_trans_.iwl_trans()->dev, &mpdu, sizeof(mpdu));

  size_t org_size =
      sizeof(struct ieee80211_frame_header) + fuchsia_wlan_ieee80211_CCMP_HDR_LEN + kMacPayloadLen;
  size_t new_size = iwl_mvm_create_packet(
      &mpdu.frame, org_size, fuchsia_wlan_ieee80211_CCMP_HDR_LEN, &rx_info, &mpdu_rxcb);
  EXPECT_EQ(new_size, org_size - fuchsia_wlan_ieee80211_CCMP_HDR_LEN);
  EXPECT_EQ(mpdu.ccmp[0], 0xff);  // moved from mpdu.mac_payload[0]
  EXPECT_EQ(mpdu.ccmp[3], 0xfc);  // moved from mpdu.mac_payload[3]
}

TEST_F(MvmTest, rxMpdu) {
  const int kExpChan = 40;

  // Simulate the previous PHY_INFO packet
  struct iwl_rx_phy_info phy_info = {
      .non_cfg_phy_cnt = IWL_RX_INFO_ENERGY_ANT_ABC_IDX + 1,
      .phy_flags = cpu_to_le16(0),
      .channel = cpu_to_le16(kExpChan),
      .non_cfg_phy =
          {
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wc99-designator"
              [IWL_RX_INFO_ENERGY_ANT_ABC_IDX] = 0x000a28,  // RSSI C:n/a B:-10, A:-40
#pragma GCC diagnostic pop
          },
      .rate_n_flags = cpu_to_le32(0x7),  // IWL_RATE_18M_PLCP
  };
  TestRxcb phy_info_rxcb(sim_trans_.iwl_trans()->dev, &phy_info, sizeof(phy_info));
  iwl_mvm_rx_rx_phy_cmd(mvm_, &phy_info_rxcb);

  // Now, it comes the MPDU packet.
  const size_t kMacPayloadLen = 60;
  struct {
    struct iwl_rx_mpdu_res_start rx_res;
    struct ieee80211_frame_header frame;
    uint8_t mac_payload[kMacPayloadLen];
    uint32_t rx_pkt_status;
  } __packed mpdu = {
      .rx_res =
          {
              .byte_count = kMacPayloadLen,
              .assist = 0,
          },
      .frame = {},
      .rx_pkt_status = 0x0,
  };
  TestRxcb mpdu_rxcb(sim_trans_.iwl_trans()->dev, &mpdu, sizeof(mpdu));

  TestCtx test_ctx = {};
  MockRecv(&test_ctx);
  iwl_mvm_rx_rx_mpdu(mvm_, nullptr /* napi */, &mpdu_rxcb);

  EXPECT_EQ(WLAN_RX_INFO_VALID_DATA_RATE,
            test_ctx.rx_info.valid_fields & WLAN_RX_INFO_VALID_DATA_RATE);
  EXPECT_EQ(TO_HALF_MBPS(18), test_ctx.rx_info.data_rate);
  EXPECT_EQ(kExpChan, test_ctx.rx_info.channel.primary);
  EXPECT_EQ(WLAN_RX_INFO_VALID_RSSI, test_ctx.rx_info.valid_fields & WLAN_RX_INFO_VALID_RSSI);
  EXPECT_EQ(static_cast<int8_t>(-10), test_ctx.rx_info.rssi_dbm);
}

// Basic test for Rx MQ (no padding by FW)
TEST_F(MvmTest, rxMqMpdu) {
  const int kExpChan = 11;

  // Simulate the previous PHY_INFO packet
  struct iwl_rx_phy_info phy_info = {
      .non_cfg_phy_cnt = IWL_RX_INFO_ENERGY_ANT_ABC_IDX + 1,
      .phy_flags = cpu_to_le16(0),
      .channel = cpu_to_le16(kExpChan),
      .non_cfg_phy =
          {
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wc99-designator"
              [IWL_RX_INFO_ENERGY_ANT_ABC_IDX] = 0x000a28,  // RSSI C:n/a B:-10, A:-40
#pragma GCC diagnostic pop
          },
      .rate_n_flags = cpu_to_le32(0x7),  // IWL_RATE_18M_PLCP
  };
  TestRxcb phy_info_rxcb(sim_trans_.iwl_trans()->dev, &phy_info, sizeof(phy_info));
  iwl_mvm_rx_rx_phy_cmd(mvm_, &phy_info_rxcb);

  // Now, it comes the MPDU packet.
  const size_t kMacPayloadLen = 60;
  struct {
    char mpdu_desc[IWL_RX_DESC_SIZE_V1];
    struct ieee80211_frame_header frame;
    uint8_t mac_payload[kMacPayloadLen];
  } __packed mpdu = {};
  struct iwl_rx_mpdu_desc* desc = (struct iwl_rx_mpdu_desc*)mpdu.mpdu_desc;
  desc->mpdu_len = kMacPayloadLen + sizeof(mpdu.frame);
  desc->v1.channel = kExpChan;
  desc->v1.energy_a = 0x7f;
  desc->v1.energy_b = 0x28;
  desc->status = 0x1007;
  desc->v1.rate_n_flags = 0x820a;
  mpdu.frame.frame_ctrl = 0x8;  // Data frame
  TestRxcb mpdu_rxcb(sim_trans_.iwl_trans()->dev, &mpdu, sizeof(mpdu));

  TestCtx test_ctx = {};
  MockRecv(&test_ctx);
  iwl_mvm_rx_mpdu_mq(mvm_, nullptr /* napi */, &mpdu_rxcb, 0);

  EXPECT_EQ(desc->mpdu_len, test_ctx.frame_len);
  EXPECT_EQ(WLAN_RX_INFO_VALID_DATA_RATE,
            test_ctx.rx_info.valid_fields & WLAN_RX_INFO_VALID_DATA_RATE);
  EXPECT_EQ(TO_HALF_MBPS(1), test_ctx.rx_info.data_rate);
  EXPECT_EQ(kExpChan, test_ctx.rx_info.channel.primary);
  EXPECT_EQ(WLAN_RX_INFO_VALID_RSSI, test_ctx.rx_info.valid_fields & WLAN_RX_INFO_VALID_RSSI);
  EXPECT_EQ(static_cast<int8_t>(-40), test_ctx.rx_info.rssi_dbm);
}

// Test checks to see frame header padding added by FW is indicated correctly to SME
TEST_F(MvmTest, rxMqMpdu_with_header_padding) {
  const int kExpChan = 11;

  // Simulate the previous PHY_INFO packet
  struct iwl_rx_phy_info phy_info = {
      .non_cfg_phy_cnt = IWL_RX_INFO_ENERGY_ANT_ABC_IDX + 1,
      .phy_flags = cpu_to_le16(0),
      .channel = cpu_to_le16(kExpChan),
      .non_cfg_phy =
          {
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Wc99-designator"
              [IWL_RX_INFO_ENERGY_ANT_ABC_IDX] = 0x000a28,  // RSSI C:n/a B:-10, A:-40
#pragma GCC diagnostic pop
          },
      .rate_n_flags = cpu_to_le32(0x7),  // IWL_RATE_18M_PLCP
  };
  TestRxcb phy_info_rxcb(sim_trans_.iwl_trans()->dev, &phy_info, sizeof(phy_info));
  iwl_mvm_rx_rx_phy_cmd(mvm_, &phy_info_rxcb);

  // Now, it comes the MPDU packet.
  const size_t kMacPayloadLen = 60;
  struct {
    char mpdu_desc[IWL_RX_DESC_SIZE_V1];
    // struct ieee80211_frame_header frame;
    uint8_t frame_header[28];
    uint8_t mac_payload[kMacPayloadLen];
  } __packed mpdu = {};
  struct iwl_rx_mpdu_desc* desc = (struct iwl_rx_mpdu_desc*)mpdu.mpdu_desc;
  struct ieee80211_frame_header* frame_header = (struct ieee80211_frame_header*)mpdu.frame_header;
  desc->mpdu_len = kMacPayloadLen + 28;
  desc->v1.channel = kExpChan;
  desc->v1.energy_a = 0x7f;
  desc->v1.energy_b = 0x28;
  desc->status = 0x1007;
  desc->v1.rate_n_flags = 0x820a;
  desc->mac_flags2 = IWL_RX_MPDU_MFLG2_PAD;
  frame_header->frame_ctrl = 0x288;  // QOS data frame
  TestRxcb mpdu_rxcb(sim_trans_.iwl_trans()->dev, &mpdu, sizeof(mpdu));

  TestCtx test_ctx = {};
  MockRecv(&test_ctx);
  iwl_mvm_rx_mpdu_mq(mvm_, nullptr /* napi */, &mpdu_rxcb, 0);

  // Expect FRAME_BODY_PADDING_4 is set in rx_flags
  EXPECT_EQ(WLAN_RX_INFO_FLAGS_FRAME_BODY_PADDING_4,
            test_ctx.rx_info.rx_flags & WLAN_RX_INFO_FLAGS_FRAME_BODY_PADDING_4);
  // Received frame length should be the same as actual receive length
  EXPECT_EQ(desc->mpdu_len, test_ctx.frame_len);
  EXPECT_EQ(TO_HALF_MBPS(1), test_ctx.rx_info.data_rate);
  EXPECT_EQ(kExpChan, test_ctx.rx_info.channel.primary);
  EXPECT_EQ(WLAN_RX_INFO_VALID_RSSI, test_ctx.rx_info.valid_fields & WLAN_RX_INFO_VALID_RSSI);
  EXPECT_EQ(static_cast<int8_t>(-40), test_ctx.rx_info.rssi_dbm);
}
// The antenna index will be toggled after each call.
// Check 'ucode_phy_sku' in test/single-ap-test.cc for the fake antenna setting.
TEST_F(MvmTest, toggleTxAntenna) {
  uint8_t ant = 1;  // the current antenna 1

  iwl_mvm_toggle_tx_ant(mvm_, &ant);
  // Since there is only antenna 1 and 0 available, the 'ant' should be updated to 0.
  EXPECT_EQ(0, ant);

  // Do again.
  iwl_mvm_toggle_tx_ant(mvm_, &ant);
  // The 'ant' should be toggled to 1.
  EXPECT_EQ(1, ant);
}

// Check 'ucode_phy_sku' in test/single-ap-test.cc for the fake antenna setting.
TEST_F(MvmTest, validRxAnt) { EXPECT_EQ(iwl_mvm_get_valid_rx_ant(mvm_), 6); }

TEST_F(MvmTest, scanLmacErrorChecking) {
  struct iwl_mvm_scan_params params = {
      .n_scan_plans = IWL_MAX_SCHED_SCAN_PLANS + 1,
  };

  EXPECT_EQ(ZX_ERR_INVALID_ARGS, iwl_mvm_scan_lmac(mvm_, &params));
}

// This test focuses on testing the scan_cmd filling for LMAC passive scan.
TEST_F(MvmTest, scanLmacPassiveCmdFilling) {
  ASSERT_NE(nullptr, mvm_->scan_cmd);  // scan cmd should have been allocated during init.

  struct iwl_mvm_scan_params params = {
      .type = IWL_SCAN_TYPE_WILD,
      .hb_type = IWL_SCAN_TYPE_NOT_SET,
      .n_channels = 4,
      .channels =
          {
              5,
              11,
              36,
              165,
          },
      .n_ssids = 0,
      .flags = 0,
      .pass_all = true,
      .n_match_sets = 0,
      .preq =
          {
              // arbitrary values for memory comparison below
              .mac_header =
                  {
                      .offset = cpu_to_le16(0x1234),
                      .len = cpu_to_le16(0x5678),
                  },
          },
      .n_scan_plans = 0,
  };

  EXPECT_EQ(ZX_OK, iwl_mvm_scan_lmac(mvm_, &params));

  struct iwl_scan_req_lmac* cmd = reinterpret_cast<struct iwl_scan_req_lmac*>(mvm_->scan_cmd);
  EXPECT_EQ(0x036d, le16_to_cpu(cmd->rx_chain_select));  // Refer iwl_mvm_scan_rx_chain() for the
                                                         // actual implementation.
  EXPECT_EQ(1, le32_to_cpu(cmd->iter_num));
  EXPECT_EQ(0, le32_to_cpu(cmd->delay));
  EXPECT_EQ(4, cmd->n_channels);
  EXPECT_EQ(PHY_BAND_24, le32_to_cpu(cmd->flags));
  EXPECT_EQ(1, cmd->schedule[0].iterations);
  struct iwl_scan_channel_cfg_lmac* channel_cfg =
      reinterpret_cast<struct iwl_scan_channel_cfg_lmac*>(cmd->data);
  EXPECT_EQ(5, le16_to_cpu(channel_cfg[0].channel_num));
  EXPECT_EQ(165, le16_to_cpu(channel_cfg[3].channel_num));
  // preq
  uint8_t* preq =
      &cmd->data[sizeof(struct iwl_scan_channel_cfg_lmac) * mvm_->fw->ucode_capa.n_scan_channels];
  EXPECT_EQ(0x34, preq[0]);
  EXPECT_EQ(0x12, preq[1]);
  EXPECT_EQ(0x78, preq[2]);
  EXPECT_EQ(0x56, preq[3]);
}

///////////////////////////////////////////////////////////////////////////////
//                                  Scan Test
//
// LMAC scan currently only supports passive scan.
class LmacScanTest : public MvmTest {
 public:
  LmacScanTest() {
    // Fake callback registered to capture scan completion responses.
    ops.scan_complete = [](void* ctx, zx_status_t status, uint64_t scan_id) {
      // TODO(fxbug.dev/88934): scan_id is always 0
      EXPECT_EQ(scan_id, 0);
      struct ScanResult* sr = (struct ScanResult*)ctx;
      sr->sme_notified = true;
      sr->success = (status == ZX_OK ? true : false);
    };

    mvmvif_sta.mvm = iwl_trans_get_mvm(sim_trans_.iwl_trans());
    mvmvif_sta.mac_role = WLAN_MAC_ROLE_CLIENT;
    mvmvif_sta.ifc.ops = &ops;
    mvmvif_sta.ifc.ctx = &scan_result;

    trans_ = sim_trans_.iwl_trans();
  }

  ~LmacScanTest() {}

  struct iwl_trans* trans_;
  wlan_softmac_ifc_protocol_ops_t ops;
  struct iwl_mvm_vif mvmvif_sta;
  uint8_t channels_to_scan_[4] = {7, 1, 40, 136};
  iwl_mvm_scan_req passive_scan_args_{
      .channels_list = channels_to_scan_, .channels_count = 4,
      // TODO(fxbug.dev/88943): Fill in other fields once support determined.
  };

  // Structure to capture scan results.
  struct ScanResult {
    bool sme_notified;
    bool success;
  } scan_result;
};

// UMAC scan currently supports both passive and active scan.
class UmacScanTest : public FakeUcodeTest {
 public:
  static constexpr size_t kChannelNum = 4;
  const uint8_t kChannelsToScan_[kChannelNum] = {7, 1, 40, 136};
  static constexpr size_t kFakeSsidLen = 6;
  const uint8_t kFakeSsid[kFakeSsidLen] = {'F', 'a', 'k', 'e', 'A', 'p'};
  static constexpr size_t kFakeIesLen = 6;
  const uint8_t kFakeIes[kFakeIesLen] = {'F', 'a', 'k', 'e', 'I', 'E'};
  static constexpr size_t kFakeMacHdrLen = 8;
  const uint8_t kFakeMacHdr[kFakeMacHdrLen] = {'F', 'a', 'k', 'e', 'H', 'e', 'a', 'd'};

  UmacScanTest() __TA_NO_THREAD_SAFETY_ANALYSIS
      : FakeUcodeTest(IWL_UCODE_TLV_CAPA_UMAC_SCAN / 32, BIT(IWL_UCODE_TLV_CAPA_UMAC_SCAN % 32),
                      IWL_UCODE_TLV_API_ADAPTIVE_DWELL / 32,
                      BIT(IWL_UCODE_TLV_API_ADAPTIVE_DWELL % 32)) {
    mvm_ = iwl_trans_get_mvm(sim_trans_.iwl_trans());
    mvmvif_ = reinterpret_cast<struct iwl_mvm_vif*>(calloc(1, sizeof(struct iwl_mvm_vif)));
    mvmvif_->mvm = mvm_;
    mvmvif_->mac_role = WLAN_MAC_ROLE_CLIENT;
    mvmvif_->ifc.ops = reinterpret_cast<wlan_softmac_ifc_protocol_ops_t*>(
        calloc(1, sizeof(wlan_softmac_ifc_protocol_ops_t)));
    mvm_->mvmvif[0] = mvmvif_;
    mvm_->vif_count++;

    // Set the channel filter to the exact channel that we want to conduct scan on to pass the
    // filtering.
    // TODO(fxbug.dev/95207): Remove the hardcode and read mcc_info from the firmware when
    // FakeUcodeTest supports multiple api_flag input.
    mvm_->mcc_info.num_ch = kChannelNum;
    memcpy(&(mvm_->mcc_info.channels[0]), &kChannelsToScan_[0], 4);
    for (uint16_t i = 0; i < kChannelNum; i++) {
      mvm_->mcc_info.ch_flags[i] = BIT(0) | BIT(3);  // NVM_CHANNEL_VALID and NVM_CHANNEL_ACTIVE
    }

    mtx_lock(&mvm_->mutex);

    // Fake callback registered to capture scan completion responses.
    ops_.scan_complete = [](void* ctx, zx_status_t status, uint64_t scan_id) {
      // TODO(fxbug.dev/88934): scan_id is always 0
      EXPECT_EQ(scan_id, 0);
      struct ScanResult* sr = (struct ScanResult*)ctx;
      sr->sme_notified = true;
      sr->success = (status == ZX_OK ? true : false);
    };

    mvmvif_sta_.mvm = iwl_trans_get_mvm(sim_trans_.iwl_trans());
    mvmvif_sta_.mac_role = WLAN_MAC_ROLE_CLIENT;
    mvmvif_sta_.ifc.ops = &ops_;
    mvmvif_sta_.ifc.ctx = &scan_result_;

    active_scan_args_.ssids =
        (struct iwl_mvm_ssid*)calloc(active_scan_args_.ssids_count, sizeof(struct iwl_mvm_ssid));
    active_scan_args_.ssids->ssid_len = 6;
    memcpy(active_scan_args_.ssids->ssid_data, kFakeSsid, 6);

    trans_ = sim_trans_.iwl_trans();
  }

  ~UmacScanTest() __TA_NO_THREAD_SAFETY_ANALYSIS {
    free(mvmvif_->ifc.ops);
    free(mvmvif_);
    free(active_scan_args_.ssids);
    mtx_unlock(&mvm_->mutex);
  }

  struct iwl_trans* trans_;
  wlan_softmac_ifc_protocol_ops_t ops_;
  struct iwl_mvm_vif mvmvif_sta_;

  iwl_mvm_scan_req passive_scan_args_{
      .channels_list = kChannelsToScan_, .channels_count = kChannelNum,
      // TODO(fxbug.dev/89693): iwlwifi ignores all other fields.
  };

  iwl_mvm_scan_req active_scan_args_{
      .channels_list = kChannelsToScan_,
      .channels_count = kChannelNum,
      .ssids_count = 1,
      .mac_header_buffer = kFakeMacHdr,
      .mac_header_size = 5,
      .ies_buffer = kFakeIes,
      .ies_size = 6,
      // TODO(fxbug.dev/89693): iwlwifi ignores all other fields.
  };

  // Structure to capture scan results.
  struct ScanResult {
    bool sme_notified;
    bool success;
  } scan_result_;

 protected:
  struct iwl_mvm* mvm_;
  struct iwl_mvm_vif* mvmvif_;
};

/* Tests for LMAC scan */
// Tests scenario for a successful scan completion.
TEST_F(LmacScanTest, RegPassiveLmacScanSuccess) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);
  ASSERT_EQ(false, scan_result.sme_notified);
  ASSERT_EQ(false, scan_result.success);

  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta, mvm_->scan_vif);

  struct iwl_periodic_scan_complete scan_notif {
    .status = IWL_SCAN_OFFLOAD_COMPLETED
  };
  TestRxcb rxb(sim_trans_.iwl_trans()->dev, &scan_notif, sizeof(scan_notif));

  // Call notify complete to simulate scan completion.
  iwl_mvm_rx_lmac_scan_complete_notif(mvm_, &rxb);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result.sme_notified);
  EXPECT_EQ(true, scan_result.success);
}

// Tests scenario where the scan request aborted / failed.
TEST_F(LmacScanTest, RegPassiveLmacScanAborted) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(false, scan_result.sme_notified);
  ASSERT_EQ(false, scan_result.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta, mvm_->scan_vif);

  // Set scan status to ABORTED so simulate a scan abort.
  struct iwl_periodic_scan_complete scan_notif {
    .status = IWL_SCAN_OFFLOAD_ABORTED
  };
  TestRxcb rxb(sim_trans_.iwl_trans()->dev, &scan_notif, sizeof(scan_notif));

  // Call notify complete to simulate scan abort.
  iwl_mvm_rx_lmac_scan_complete_notif(mvm_, &rxb);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result.sme_notified);
  EXPECT_EQ(false, scan_result.success);
}

/* Tests for UMAC scan */
// Tests scenario for a successful passive scan completion.
TEST_F(UmacScanTest, RegPassiveUmacScanSuccess) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);
  ASSERT_EQ(false, scan_result_.sme_notified);
  ASSERT_EQ(false, scan_result_.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta_, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta_, mvm_->scan_vif);

  struct iwl_umac_scan_complete scan_notif {
    .status = IWL_SCAN_OFFLOAD_COMPLETED
  };
  TestRxcb rxb(sim_trans_.iwl_trans()->dev, &scan_notif, sizeof(scan_notif));

  // Call notify complete to simulate scan completion.
  mtx_unlock(&mvm_->mutex);
  iwl_mvm_rx_umac_scan_complete_notif(mvm_, &rxb);
  mtx_lock(&mvm_->mutex);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result_.sme_notified);
  EXPECT_EQ(true, scan_result_.success);
}

// Tests scenario for a successful active scan completion.
TEST_F(UmacScanTest, RegActiveUmacScanSuccess) __TA_NO_THREAD_SAFETY_ANALYSIS {
  // Check some assumptions before running the test.
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);
  ASSERT_EQ(false, scan_result_.sme_notified);
  ASSERT_EQ(false, scan_result_.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta_, &active_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta_, mvm_->scan_vif);

  // Verify the scan cmd filling is correct.
  struct iwl_scan_req_umac* cmd = reinterpret_cast<struct iwl_scan_req_umac*>(mvm_->scan_cmd);
  // IWL_UCODE_TLV_API_ADAPTIVE_DWELL is set when constructing the UmacScanTest class, the
  // corresponding data type is v7.
  uint8_t* cmd_data = (uint8_t*)&cmd->v7.data;

  EXPECT_EQ(4, cmd->v7.channel.count);
  // Verify that it's an active scan.
  EXPECT_EQ(0, le16_to_cpu(cmd->general_flags) & IWL_UMAC_SCAN_GEN_FLAGS_PASSIVE);
  struct iwl_scan_channel_cfg_umac* channel_cfg =
      reinterpret_cast<struct iwl_scan_channel_cfg_umac*>(cmd_data);

  // Verify ssid_bitmap.
  EXPECT_EQ(BIT(0), le32_to_cpu(channel_cfg[0].flags));
  EXPECT_EQ(1, le32_to_cpu(channel_cfg[0].iter_count));
  EXPECT_EQ(0, le32_to_cpu(channel_cfg[0].iter_interval));

  // Verify the first and the last channel number.
  EXPECT_EQ(7, le16_to_cpu(channel_cfg[0].channel_num));
  EXPECT_EQ(136, le16_to_cpu(channel_cfg[3].channel_num));

  struct iwl_scan_req_umac_tail* sec_part_of_cmd_data =
      (struct iwl_scan_req_umac_tail*)(cmd_data + sizeof(struct iwl_scan_channel_cfg_umac) *
                                                      mvm_->fw->ucode_capa.n_scan_channels);

  struct iwl_scan_probe_req* preq = &sec_part_of_cmd_data->preq;
  uint8_t* frame_data = (uint8_t*)preq->buf;

  // Verify schedule data.
  EXPECT_EQ(1, sec_part_of_cmd_data->schedule[0].iter_count);
  EXPECT_EQ(0, sec_part_of_cmd_data->schedule[0].interval);

  // Verify MAC header.
  EXPECT_EQ(0, memcmp(kFakeMacHdr, frame_data + le16_to_cpu(preq->mac_header.offset),
                      le16_to_cpu(preq->mac_header.len) - 2));

  // Verify common IE.
  EXPECT_EQ(0, memcmp(kFakeIes, frame_data + le16_to_cpu(preq->common_data.offset),
                      le16_to_cpu(preq->common_data.len)));

  // Verify ssid.
  EXPECT_EQ(WLAN_EID_SSID, sec_part_of_cmd_data->direct_scan[0].id);
  EXPECT_EQ(6, sec_part_of_cmd_data->direct_scan[0].len);
  EXPECT_EQ(0, memcmp(kFakeSsid, sec_part_of_cmd_data->direct_scan[0].ssid, 6));

  struct iwl_umac_scan_complete scan_notif {
    .status = IWL_SCAN_OFFLOAD_COMPLETED
  };
  TestRxcb rxb(sim_trans_.iwl_trans()->dev, &scan_notif, sizeof(scan_notif));

  // Call notify complete to simulate scan completion.
  mtx_unlock(&mvm_->mutex);
  iwl_mvm_rx_umac_scan_complete_notif(mvm_, &rxb);
  mtx_lock(&mvm_->mutex);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result_.sme_notified);
  EXPECT_EQ(true, scan_result_.success);
}

// Tests scenario where the scan request aborted / failed.
TEST_F(UmacScanTest, RegPassiveUmacScanAborted) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(false, scan_result_.sme_notified);
  ASSERT_EQ(false, scan_result_.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta_, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta_, mvm_->scan_vif);

  // Set scan status to ABORTED so simulate a scan abort.
  struct iwl_umac_scan_complete scan_notif {
    .status = IWL_SCAN_OFFLOAD_ABORTED
  };
  TestRxcb rxb(sim_trans_.iwl_trans()->dev, &scan_notif, sizeof(scan_notif));

  // Call notify complete to simulate scan abort.
  mtx_unlock(&mvm_->mutex);
  iwl_mvm_rx_umac_scan_complete_notif(mvm_, &rxb);
  mtx_lock(&mvm_->mutex);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result_.sme_notified);
  EXPECT_EQ(false, scan_result_.success);
}

// Tests explicit abort of Passive Umac scan in progress.
TEST_F(UmacScanTest, RegPassiveUmacAbortScan) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(false, scan_result_.sme_notified);
  ASSERT_EQ(false, scan_result_.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta_, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta_, mvm_->scan_vif);

  // attempt to stop any ongoing scans.
  iwl_mvm_scan_stop(mvm_, IWL_MVM_SCAN_REGULAR, false);
  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result_.sme_notified);
  EXPECT_EQ(false, scan_result_.success);
}

/* Tests for both LMAC and UMAC scans */
// Tests condition where scan completion timeouts out due to no response from FW.
TEST_F(LmacScanTest, RegPassiveScanTimeout) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(false, scan_result.sme_notified);
  ASSERT_EQ(false, scan_result.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta, mvm_->scan_vif);

  // Do not call notify complete; instead invoke the timeout callback to simulate a timeout event.
  mtx_unlock(&mvm_->mutex);
  iwl_mvm_scan_timeout_wk(mvm_);
  mtx_lock(&mvm_->mutex);

  EXPECT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(true, scan_result.sme_notified);
  EXPECT_EQ(false, scan_result.success);
}

// Tests condition where timer is shutdown and there is no response from FW.
TEST_F(LmacScanTest, RegPassiveScanTimerShutdown) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(false, scan_result.sme_notified);
  ASSERT_EQ(false, scan_result.success);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta, mvm_->scan_vif);

  // Do not call notify complete, and do not invoke the timeout callback.  This simulates a timer
  // shutdown while it is pending.

  // Ensure the state is such that no FW response or timeout has happened.
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(false, scan_result.sme_notified);
  EXPECT_EQ(false, scan_result.success);
}

// Tests condition where iwl_mvm_mac_stop() is invoked while timer is pending.
TEST_F(LmacScanTest, RegPassiveScanTimerMvmStop) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(nullptr, mvm_->scan_vif);

  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(&mvmvif_sta, mvm_->scan_vif);

  mtx_unlock(&mvm_->mutex);
  iwl_mvm_mac_stop(mvm_);
  mtx_lock(&mvm_->mutex);
}

// Tests condition where multiple calls to the scan API returns appropriate error.
TEST_F(LmacScanTest, RegPassiveScanParallel) __TA_NO_THREAD_SAFETY_ANALYSIS {
  ASSERT_EQ(0, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  ASSERT_EQ(ZX_OK, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
  EXPECT_EQ(IWL_MVM_SCAN_REGULAR, mvm_->scan_status & IWL_MVM_SCAN_REGULAR);
  EXPECT_EQ(ZX_ERR_SHOULD_WAIT, iwl_mvm_reg_scan_start(&mvmvif_sta, &passive_scan_args_));
}

///////////////////////////////////////////////////////////////////////////////
//                             Time Event Test
//
class TimeEventTest : public MvmTest {
 public:
  TimeEventTest() {
    // In order to init the mvmvif_->time_event_data.id to TE_MAX.
    iwl_mvm_mac_ctxt_init(mvmvif_);
  }
};

TEST_F(TimeEventTest, NormalCase) {
  // wait_for_notif is true.
  ASSERT_EQ(0, list_length(&mvm_->time_event_list));
  ASSERT_EQ(ZX_OK, iwl_mvm_protect_session(mvm_, mvmvif_, 1, 2, 3, true));
  ASSERT_EQ(1, list_length(&mvm_->time_event_list));
  ASSERT_EQ(ZX_OK, iwl_mvm_stop_session_protection(mvmvif_));
  ASSERT_EQ(0, list_length(&mvm_->time_event_list));

  // wait_for_notif is false.
  ASSERT_EQ(0, list_length(&mvm_->time_event_list));
  ASSERT_EQ(ZX_OK, iwl_mvm_protect_session(mvm_, mvmvif_, 1, 2, 3, false));
  ASSERT_EQ(1, list_length(&mvm_->time_event_list));
  ASSERT_EQ(ZX_OK, iwl_mvm_stop_session_protection(mvmvif_));
  ASSERT_EQ(0, list_length(&mvm_->time_event_list));
}

TEST_F(TimeEventTest, Notification) {
  // Set wait_for_notif to false so that we don't wait for TIME_EVENT_NOTIFICATION.
  ASSERT_EQ(ZX_OK, iwl_mvm_protect_session(mvm_, mvmvif_, 1, 2, 3, false));

  // On the real device, 'te_data->uid' is populated by response of TIME_EVENT_CMD. However, the
  // iwl_mvm_time_event_send_add() uses iwl_wait_notification() to get the value instead of reading
  // from the cmd->resp_pkt (see the comment in iwl_mvm_time_event_send_add()).
  //
  // However, the current test/sim-mvm.cc is hard to implement the wait notification yet (which
  // requires multi-threading model). So, the hack is inserting the 'te_data->uid' in the test code.
  //
  // TODO(fxbug.dev/87974): remove this hack once the wait notification model is supported in the
  //                        testing code.
  //
  ASSERT_EQ(1, list_length(&mvm_->time_event_list));
  auto* te_data = list_peek_head_type(&mvm_->time_event_list, struct iwl_mvm_time_event_data, list);
  te_data->uid = kFakeUniqueId;

  // Generate a fake TIME_EVENT_NOTIFICATION from the firmware. Note that this notification is
  // differnt from the above code, which is the notification for TIME_EVENT_CMD.
  //
  // We expect the driver will remove the waiting notification from the `time_event_list`.
  //
  // TODO(fxbug.dev/51671): remove this hack once the test/sim-mvm.cc can support filing another
  //                        notification from one host command.
  //
  struct iwl_time_event_notif notif = {
      .unique_id = kFakeUniqueId,
      .action = TE_V2_NOTIF_HOST_EVENT_END,
  };
  TestRxcb time_event_rxcb(sim_trans_.iwl_trans()->dev, &notif, sizeof(notif));
  iwl_mvm_rx_time_event_notif(mvm_, &time_event_rxcb);
  ASSERT_EQ(0, list_length(&mvm_->time_event_list));
}

///////////////////////////////////////////////////////////////////////////////
//                               Binding Test
//
class BindingTest : public MvmTest {
 public:
  BindingTest() { setup_phy_ctxt(mvmvif_); }
};

TEST_F(BindingTest, CheckArgs) {
  // Failed because phy_ctxt is unexpected.
  mvmvif_->phy_ctxt = NULL;
  ASSERT_EQ(ZX_ERR_BAD_STATE, iwl_mvm_binding_add_vif(mvmvif_));

  ASSERT_EQ(ZX_ERR_INVALID_ARGS, iwl_mvm_binding_remove_vif(mvmvif_));
}

TEST_F(BindingTest, NormalCase) {
  ASSERT_EQ(ZX_OK, iwl_mvm_binding_add_vif(mvmvif_));
  ASSERT_EQ(ZX_OK, iwl_mvm_binding_remove_vif(mvmvif_));
}

///////////////////////////////////////////////////////////////////////////////
//                               Power Test
//
class PowerTest : public MvmTest {
 public:
  PowerTest() { setup_phy_ctxt(mvmvif_); }
};

// By default, only one interface is created and its ps_disabled is false. So:
//
//   - mvmvif->pm_enabled is true.
//   - mvmvif->ps_disabled is false.
//   - thus, mvm->ps_disabled is false as well.
//
TEST_F(PowerTest, DefaultCase) {
  ASSERT_EQ(ZX_OK, iwl_mvm_power_update_mac(mvm_));
  EXPECT_EQ(true, mvmvif_->pm_enabled);
  EXPECT_EQ(false, mvmvif_->ps_disabled);
  EXPECT_EQ(false, mvm_->ps_disabled);
}

// Disable the PS of interface. We shall see MVM PS is disabled as well.
//
//   - mvmvif->pm_enabled is true.
//   - mvmvif->ps_disabled is false.
//   - thus, mvm->ps_disabled is false as well.
//
TEST_F(PowerTest, PsDisabled) {
  mvmvif_->ps_disabled = true;
  ASSERT_EQ(ZX_OK, iwl_mvm_power_update_mac(mvm_));
  EXPECT_EQ(true, mvmvif_->pm_enabled);
  EXPECT_EQ(true, mvmvif_->ps_disabled);
  EXPECT_EQ(true, mvm_->ps_disabled);
}

// The input pm_enabled has no effect since it is determined by iwl_mvm_power_update_mac() according
// to the current interface configuraiton.
//
// The expected results are identical to the default case above.
//
TEST_F(PowerTest, PmHasNoEffect) {
  mvmvif_->pm_enabled = false;
  ASSERT_EQ(ZX_OK, iwl_mvm_power_update_mac(mvm_));
  EXPECT_EQ(true, mvmvif_->pm_enabled);
  EXPECT_EQ(false, mvmvif_->ps_disabled);
  EXPECT_EQ(false, mvm_->ps_disabled);

  mvmvif_->pm_enabled = true;
  ASSERT_EQ(ZX_OK, iwl_mvm_power_update_mac(mvm_));
  EXPECT_EQ(true, mvmvif_->pm_enabled);
  EXPECT_EQ(false, mvmvif_->ps_disabled);
  EXPECT_EQ(false, mvm_->ps_disabled);
}

///////////////////////////////////////////////////////////////////////////////
//                               Txq Test
//
class TxqTest : public MvmTest, public MockTrans {
 public:
  TxqTest()
      : sta_{
            .sta_id = 0,
            .mvmvif = mvmvif_,
            .addr = {0x02, 0x03, 0x04, 0x05, 0x06, 0x07},
        } {
    BIND_TEST(mvm_->trans);

    mvm_->fw_id_to_mac_id[0] = &sta_;
    for (size_t i = 0; i < std::size(sta_.txq); ++i) {
      sta_.txq[i] = reinterpret_cast<struct iwl_mvm_txq*>(calloc(1, sizeof(struct iwl_mvm_txq)));
      ASSERT_NE(nullptr, sta_.txq[i]);
    }
  }

  ~TxqTest() {
    for (size_t i = 0; i < std::size(sta_.txq); ++i) {
      free(sta_.txq[i]);
    }
  }

  // Expected fields.
  mock_function::MockFunction<zx_status_t,  // return value
                              size_t,       // packet size
                              uint16_t,     // cmd + group_id
                              int           // txq_id
                              >
      mock_tx_;

  static zx_status_t tx_wrapper(struct iwl_trans* trans, struct ieee80211_mac_packet* pkt,
                                const struct iwl_device_cmd* dev_cmd, int txq_id) {
    auto test = GET_TEST(TxqTest, trans);
    return test->mock_tx_.Call(pkt->header_size + pkt->headroom_used_size + pkt->body_size,
                               WIDE_ID(dev_cmd->hdr.group_id, dev_cmd->hdr.cmd), txq_id);
  }

 protected:
  struct iwl_mvm_sta sta_;
};

TEST_F(TxqTest, TestAllocManagement) {
  // Ensure the internal state is cleared.
  ASSERT_EQ(0, sta_.tid_data[IWL_MAX_TID_COUNT].txq_id);
  ASSERT_EQ(0, sta_.tfd_queue_msk);

  // Keep asking for queue for management packet (TID=MAX).
  // Expect txq_id IWL_MVM_DQA_MIN_MGMT_QUEUE is allocated.
  auto expected_mask = sta_.tfd_queue_msk;
  for (size_t i = 0; i < (IWL_MVM_DQA_MAX_MGMT_QUEUE - IWL_MVM_DQA_MIN_MGMT_QUEUE + 1); ++i) {
    int tid = IWL_MAX_TID_COUNT;
    ASSERT_EQ(ZX_OK, iwl_mvm_sta_alloc_queue(mvm_, &sta_, IEEE80211_AC_BE, tid));

    EXPECT_EQ(i + IWL_MVM_DQA_MIN_MGMT_QUEUE, sta_.tid_data[tid].txq_id);
    expected_mask |= BIT(i + IWL_MVM_DQA_MIN_MGMT_QUEUE);
    EXPECT_EQ(expected_mask, sta_.tfd_queue_msk);
  }

  // Request once more. Since there is no queue for management packet, expect data queue.
  ASSERT_EQ(ZX_OK, iwl_mvm_sta_alloc_queue(mvm_, &sta_, IEEE80211_AC_BE, IWL_MAX_TID_COUNT));
  EXPECT_EQ(IWL_MVM_DQA_MIN_DATA_QUEUE, sta_.tid_data[IWL_MAX_TID_COUNT].txq_id);
  expected_mask |= BIT(IWL_MVM_DQA_MIN_DATA_QUEUE);
  EXPECT_EQ(expected_mask, sta_.tfd_queue_msk);
}

TEST_F(TxqTest, TestAllocData) {
  // Ensure the internal state is cleared.
  ASSERT_EQ(0, sta_.tid_data[IWL_MAX_TID_COUNT].txq_id);
  ASSERT_EQ(0, sta_.tfd_queue_msk);

  // Keep asking for queue for data packet (TID!=MAX).
  // Expect txq_id IWL_MVM_DQA_MIN_DATA_QUEUE is allocated.
  auto expected_mask = sta_.tfd_queue_msk;
  for (size_t i = 0; i < (IWL_MVM_DQA_MAX_DATA_QUEUE - IWL_MVM_DQA_MIN_DATA_QUEUE + 1); ++i) {
    int tid = IWL_TID_NON_QOS;
    ASSERT_EQ(ZX_OK, iwl_mvm_sta_alloc_queue(mvm_, &sta_, IEEE80211_AC_BE, tid));

    EXPECT_EQ(i + IWL_MVM_DQA_MIN_DATA_QUEUE, sta_.tid_data[tid].txq_id);
    expected_mask |= BIT(i + IWL_MVM_DQA_MIN_DATA_QUEUE);
    EXPECT_EQ(expected_mask, sta_.tfd_queue_msk);
  }

  // Request once more. Since there is no queue for data packet, expect failure.
  // TODO(fxbug.dev/49530): this should be re-written once shared queue is supported.
  ASSERT_EQ(ZX_ERR_NO_RESOURCES, iwl_mvm_sta_alloc_queue(mvm_, &sta_, IEEE80211_AC_BE, 0));
}

TEST_F(TxqTest, DataTxCmd) {
  ieee80211_mac_packet pkt = {
      .body_size = 56,  // arbitrary value.
  };
  iwl_tx_cmd tx_cmd = {
      .tx_flags = TX_CMD_FLG_TSF,  // arbitary value to ensure the function would keep it.
  };
  iwl_mvm_set_tx_cmd(mvmvif_->mvm, &pkt, &tx_cmd, static_cast<uint8_t>(sta_.sta_id));

  // Currently the function doesn't consider the QoS so that those values are just fixed value.
  EXPECT_EQ(TX_CMD_FLG_TSF | TX_CMD_FLG_SEQ_CTL | TX_CMD_FLG_BT_DIS | TX_CMD_FLG_ACK,
            tx_cmd.tx_flags);

  EXPECT_EQ(IWL_MAX_TID_COUNT, tx_cmd.tid_tspec);
  EXPECT_EQ(cpu_to_le16(PM_FRAME_MGMT), tx_cmd.pm_frame_timeout);
  EXPECT_EQ(cpu_to_le16(static_cast<uint16_t>(pkt.body_size)), tx_cmd.len);
  EXPECT_EQ(cpu_to_le32(TX_CMD_LIFE_TIME_INFINITE), tx_cmd.life_time);
  EXPECT_EQ(0, tx_cmd.sta_id);
}

TEST_F(TxqTest, DataTxCmdRate) {
  iwl_tx_cmd tx_cmd = {};
  struct ieee80211_frame_header frame_hdr;
  // construct a data frame, and check the rate.
  frame_hdr.frame_ctrl |= IEEE80211_FRAME_TYPE_DATA;
  sta_.sta_state = IWL_STA_AUTHORIZED;

  iwl_mvm_set_tx_cmd_rate(mvmvif_->mvm, &tx_cmd, &frame_hdr);

  // Verify tx_cmd rate fields when frame type is data frame when station is authorized, the rate
  // should not be set.
  EXPECT_EQ(0, tx_cmd.initial_rate_index);
  EXPECT_GT(tx_cmd.tx_flags & cpu_to_le32(TX_CMD_FLG_STA_RATE), 0);
  EXPECT_EQ(0, tx_cmd.rate_n_flags);

  EXPECT_EQ(IWL_RTS_DFAULT_RETRY_LIMIT, tx_cmd.rts_retry_limit);
  EXPECT_EQ(IWL_DEFAULT_TX_RETRY, tx_cmd.data_retry_limit);
}

TEST_F(TxqTest, MgmtTxCmdRate) {
  iwl_tx_cmd tx_cmd = {};
  struct ieee80211_frame_header frame_hdr;

  // construct a non-data frame, and check the rate.
  frame_hdr.frame_ctrl |= IEEE80211_FRAME_TYPE_MGMT;

  iwl_mvm_set_tx_cmd_rate(mvmvif_->mvm, &tx_cmd, &frame_hdr);

  // Because the rate which is set to non-data frame in our code is a temporary value, so this line
  // might be changed in the future.
  EXPECT_EQ(iwl_mvm_mac80211_idx_to_hwrate(IWL_FIRST_OFDM_RATE) |
                (BIT(mvm_->mgmt_last_antenna_idx) << RATE_MCS_ANT_POS),
            tx_cmd.rate_n_flags);
}

TEST_F(TxqTest, TxPktInvalidInput) {
  WlanPktBuilder builder;
  std::shared_ptr<WlanPktBuilder::WlanPkt> wlan_pkt(builder.build());

  // Null STA
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, iwl_mvm_tx_skb(mvm_, wlan_pkt->mac_pkt(), nullptr));

  // invalid STA id.
  uint32_t sta_id = sta_.sta_id;
  sta_.sta_id = IWL_MVM_INVALID_STA;
  EXPECT_EQ(ZX_ERR_INVALID_ARGS, iwl_mvm_tx_skb(mvm_, wlan_pkt->mac_pkt(), &sta_));
  sta_.sta_id = sta_id;

  // the check in iwl_mvm_tx_pkt_queued() -- after iwl_trans_tx().
  {
    bindTx(tx_wrapper);
    mock_tx_.ExpectCall(ZX_OK, wlan_pkt->len(), WIDE_ID(0, TX_CMD), 0);

    uint32_t mac_id_n_color = sta_.mac_id_n_color;
    sta_.mac_id_n_color = NUM_MAC_INDEX_DRIVER;
    EXPECT_EQ(ZX_ERR_INVALID_ARGS, iwl_mvm_tx_skb(mvm_, wlan_pkt->mac_pkt(), &sta_));
    sta_.mac_id_n_color = mac_id_n_color;  // Restore the changed value.

    unbindTx();
  }
}

TEST_F(TxqTest, TxPkt) {
  WlanPktBuilder builder;
  std::shared_ptr<WlanPktBuilder::WlanPkt> wlan_pkt(builder.build());

  bindTx(tx_wrapper);
  mock_tx_.ExpectCall(ZX_OK, wlan_pkt->len(), WIDE_ID(0, TX_CMD), 0);
  EXPECT_EQ(ZX_OK, iwl_mvm_tx_skb(mvmvif_->mvm, wlan_pkt->mac_pkt(), &sta_));
  unbindTx();
}

// Check to see Tx params are set correctly based on frame control
TEST_F(TxqTest, TxPktProtected) {
  // Send a protected data frame and see that the crypt header is being added
  WlanPktBuilder builder;
  std::shared_ptr<WlanPktBuilder::WlanPkt> wlan_pkt(builder.build(0x4188));

  EXPECT_EQ(wlan_pkt->mac_pkt()->headroom_used_size, 0);
  // Setup a key conf to pretend that this is a secure connection
  auto key_conf = reinterpret_cast<ieee80211_key_conf*>(malloc(sizeof(ieee80211_key_conf) + 16));
  memset(key_conf, 0, sizeof(*key_conf) + 16);
  key_conf->cipher = 4;
  key_conf->key_type = 1;
  key_conf->keyidx = 0;
  key_conf->keylen = 16;
  key_conf->rx_seq = 0;
  wlan_pkt->mac_pkt()->info.control.hw_key = key_conf;

  bindTx(tx_wrapper);
  // Expect the packet length to be 8 bytes longer
  mock_tx_.ExpectCall(ZX_OK, wlan_pkt->len() + 8, WIDE_ID(0, TX_CMD), 0);
  EXPECT_EQ(ZX_OK, iwl_mvm_tx_skb(mvmvif_->mvm, wlan_pkt->mac_pkt(), &sta_));
  unbindTx();
  // Expect that the headroom size is set to 8
  EXPECT_EQ(wlan_pkt->mac_pkt()->headroom_used_size, 8);
  free(key_conf);
}

}  // namespace
}  // namespace testing
}  // namespace wlan
