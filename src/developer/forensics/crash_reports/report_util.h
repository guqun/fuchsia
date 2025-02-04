// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_REPORT_UTIL_H_
#define SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_REPORT_UTIL_H_

#include <fuchsia/feedback/cpp/fidl.h>
#include <lib/fpromise/result.h>

#include <optional>
#include <string>

#include "src/developer/forensics/crash_reports/annotation_map.h"
#include "src/developer/forensics/crash_reports/product.h"
#include "src/developer/forensics/crash_reports/report.h"
#include "src/developer/forensics/crash_reports/snapshot_manager.h"
#include "src/developer/forensics/utils/errors.h"

namespace forensics {
namespace crash_reports {

// Shorten |program_name| into a shortname by removing the "fuchsia-pkg://" prefix if present and
// replacing all '/' with ':'.
//
// For example `fuchsia-pkg://fuchsia.com/foo-bar#meta/foo_bar.cmx` becomes
// `fuchsia.com:foo-bar#meta:foo_bar.cmx`.
std::string Shorten(std::string program_name);

// Extract the component name without the ".cmx" suffix from |name|, if one is present.
//
// For example `fuchsia-pkg://fuchsia.com/foo-bar#meta/foo_bar.cmx` becomes
// `foo_bar`.
std::string Logname(std::string name);

// Builds the final report to add to the queue.
//
// * Most annotations are shared across all crash reports, e.g. the device uptime.
// * Some annotations are report-specific, e.g., Dart exception type.
// * Adds any annotations from |report|.
//
// * Some attachments are report-specific, e.g., Dart exception stack trace.
// * Adds any attachments from |report|.
std::optional<Report> MakeReport(fuchsia::feedback::CrashReport input_report, ReportId report_id,
                                 const SnapshotUuid& snapshot_uuid, const Snapshot& snapshot,
                                 const std::optional<timekeeper::time_utc>& current_time,
                                 const ErrorOr<std::string>& device_id,
                                 const AnnotationMap& default_annotations, const Product& product,
                                 bool is_hourly_report);

}  // namespace crash_reports
}  // namespace forensics

#endif  // SRC_DEVELOPER_FORENSICS_CRASH_REPORTS_REPORT_UTIL_H_
