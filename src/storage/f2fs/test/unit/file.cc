// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <unordered_set>

#include <gtest/gtest.h>

#include "src/lib/storage/block_client/cpp/fake_block_device.h"
#include "src/storage/f2fs/f2fs.h"
#include "unit_lib.h"

namespace f2fs {
namespace {

class FileTest : public F2fsFakeDevTestFixture {
 public:
  FileTest()
      : F2fsFakeDevTestFixture(TestOptions{
            .block_count = uint64_t{8} * 1024 * 1024 * 1024 / kDefaultSectorSize,
        }) {}
};

TEST_F(FileTest, BlkAddrLevel) {
  srand(testing::UnitTest::GetInstance()->random_seed());

  fbl::RefPtr<fs::Vnode> test_file;
  ASSERT_EQ(root_dir_->Create("test", S_IFREG, &test_file), ZX_OK);

  fbl::RefPtr<VnodeF2fs> test_file_vn = fbl::RefPtr<VnodeF2fs>::Downcast(std::move(test_file));
  File *test_file_ptr = static_cast<File *>(test_file_vn.get());

  char buf[kPageSize];
  unsigned int level = 0;

  for (size_t i = 0; i < kPageSize; ++i) {
    buf[i] = static_cast<char>(rand());
  }

  // fill kAddrsPerInode blocks
  for (int i = 0; i < kAddrsPerInode; ++i) {
    FileTester::AppendToFile(test_file_ptr, buf, kPageSize);
  }

  // check direct node #1 is not available yet
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, level);

  // fill one more block
  FileTester::AppendToFile(test_file_ptr, buf, kPageSize);

  // check direct node #1 is available
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, ++level);

  // fill direct node #1
  for (int i = 1; i < kAddrsPerBlock; ++i) {
    FileTester::AppendToFile(test_file_ptr, buf, kPageSize);
  }

  // check direct node #2 is not available yet
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, level);

  // fill one more block
  FileTester::AppendToFile(test_file_ptr, buf, kPageSize);

  // check direct node #2 is available
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, ++level);

  // fill direct node #2
  for (int i = 1; i < kAddrsPerBlock; ++i) {
    FileTester::AppendToFile(test_file_ptr, buf, kPageSize);
  }

  // check indirect node #1 is not available yet
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, level);

  // fill one more block
  FileTester::AppendToFile(test_file_ptr, buf, kPageSize);

  // check indirect node #1 is available
  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, ++level);

  ASSERT_EQ(test_file_vn->Close(), ZX_OK);
  test_file_vn = nullptr;
}

TEST_F(FileTest, NidAndBlkaddrAllocFree) {
  srand(testing::UnitTest::GetInstance()->random_seed());

  fbl::RefPtr<fs::Vnode> test_file;
  ASSERT_EQ(root_dir_->Create("test", S_IFREG, &test_file), ZX_OK);

  fbl::RefPtr<VnodeF2fs> test_file_vn = fbl::RefPtr<VnodeF2fs>::Downcast(std::move(test_file));
  File *test_file_ptr = static_cast<File *>(test_file_vn.get());

  char buf[kPageSize];

  for (size_t i = 0; i < kPageSize; ++i) {
    buf[i] = static_cast<char>(rand() % 128);
  }

  // Fill until direct nodes are full
  unsigned int level = 2;
  for (int i = 0; i < kAddrsPerInode + kAddrsPerBlock * 2; ++i) {
    FileTester::AppendToFile(test_file_ptr, buf, kPageSize);
  }

  test_file_ptr->SyncFile(0, test_file_ptr->GetSize(), false);

  MapTester::CheckNodeLevel(fs_.get(), test_file_ptr, level);

  // Build nid and blkaddr set
  std::unordered_set<nid_t> nid_set;
  std::unordered_set<block_t> blkaddr_set;

  nid_set.insert(test_file_ptr->Ino());
  {
    LockedPage ipage;
    ASSERT_EQ(fs_->GetNodeManager().GetNodePage(test_file_ptr->Ino(), &ipage), ZX_OK);
    Inode *inode = &(ipage->GetAddress<Node>()->i);

    for (int i = 0; i < kNidsPerInode; ++i) {
      if (inode->i_nid[i] != 0U)
        nid_set.insert(inode->i_nid[i]);
    }

    for (int i = 0; i < kAddrsPerInode; ++i) {
      ASSERT_NE(inode->i_addr[i], kNullAddr);
      blkaddr_set.insert(inode->i_addr[i]);
    }

    for (int i = 0; i < 2; ++i) {
      LockedPage direct_node_page;
      ASSERT_EQ(fs_->GetNodeManager().GetNodePage(inode->i_nid[i], &direct_node_page), ZX_OK);
      DirectNode *direct_node = &(direct_node_page->GetAddress<Node>()->dn);

      for (int j = 0; j < kAddrsPerBlock; j++) {
        ASSERT_NE(direct_node->addr[j], kNullAddr);
        blkaddr_set.insert(direct_node->addr[j]);
      }
    }
  }

  ASSERT_EQ(nid_set.size(), level + 1);
  ASSERT_EQ(blkaddr_set.size(), static_cast<uint32_t>(kAddrsPerInode + kAddrsPerBlock * 2));

  // After writing checkpoint, check if nids are removed from free nid list
  // Also, for allocated blkaddr, check if corresponding bit is set in valid bitmap of segment
  fs_->WriteCheckpoint(false, false);

  MapTester::CheckNidsInuse(fs_.get(), nid_set);
  MapTester::CheckBlkaddrsInuse(fs_.get(), blkaddr_set);

  // Remove file, writing checkpoint, then check if nids are added to free nid list
  // Also, for allocated blkaddr, check if corresponding bit is cleared in valid bitmap of segment
  ASSERT_EQ(test_file_vn->Close(), ZX_OK);
  test_file_vn = nullptr;

  root_dir_->Unlink("test", false);
  fs_->WriteCheckpoint(false, false);

  MapTester::CheckNidsFree(fs_.get(), nid_set);
  MapTester::CheckBlkaddrsFree(fs_.get(), blkaddr_set);
  test_file_vn = nullptr;
}

TEST_F(FileTest, FileReadExceedFileSize) {
  srand(testing::UnitTest::GetInstance()->random_seed());

  fbl::RefPtr<fs::Vnode> test_file;
  ASSERT_EQ(root_dir_->Create("test", S_IFREG, &test_file), ZX_OK);

  fbl::RefPtr<VnodeF2fs> test_file_vn = fbl::RefPtr<VnodeF2fs>::Downcast(std::move(test_file));
  File *test_file_ptr = static_cast<File *>(test_file_vn.get());

  uint32_t data_size = kPageSize * 7 / 4;
  uint32_t read_location = kPageSize * 5 / 4;

  char w_buf[data_size];
  char r_buf[read_location + kPageSize];

  for (size_t i = 0; i < data_size; ++i) {
    w_buf[i] = static_cast<char>(rand() % 128);
  }

  // Write data
  FileTester::AppendToFile(test_file_ptr, w_buf, data_size);
  ASSERT_EQ(test_file_ptr->GetSize(), data_size);

  size_t out;
  // Read first part of file
  ASSERT_EQ(test_file_ptr->Read(r_buf, read_location, 0, &out), ZX_OK);
  ASSERT_EQ(out, read_location);
  // Read excess file size, then check if actual read size does not exceed the end of file
  ASSERT_EQ(test_file_ptr->Read(&(r_buf[read_location]), kPageSize, read_location, &out), ZX_OK);
  ASSERT_EQ(out, data_size - read_location);

  ASSERT_EQ(memcmp(r_buf, w_buf, data_size), 0);

  ASSERT_EQ(test_file_vn->Close(), ZX_OK);
  test_file_vn = nullptr;
}

}  // namespace
}  // namespace f2fs
