// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_runtime::utc_time;
use fuchsia_zircon::{self as zx, Task};
use tracing::warn;

use crate::logging::not_implemented;
use crate::syscalls::decls::SyscallDecl;
use crate::syscalls::*;
use crate::task::Waiter;
use crate::types::*;

pub fn sys_uname(current_task: &CurrentTask, name: UserRef<utsname_t>) -> Result<(), Errno> {
    fn init_array(fixed: &mut [u8; 65], init: &'static str) {
        let init_bytes = init.as_bytes();
        let len = init.len();
        fixed[..len].copy_from_slice(init_bytes)
    }

    let mut result = utsname_t {
        sysname: [0; 65],
        nodename: [0; 65],
        release: [0; 65],
        version: [0; 65],
        machine: [0; 65],
    };
    init_array(&mut result.sysname, "Linux");
    init_array(&mut result.nodename, "local");
    init_array(&mut result.release, "5.7.17-starnix");
    init_array(&mut result.version, "starnix");
    init_array(&mut result.machine, "x86_64");
    current_task.mm.write_object(name, &result)?;
    return Ok(());
}

pub fn sys_getrandom(
    current_task: &CurrentTask,
    buf_addr: UserAddress,
    size: usize,
    _flags: i32,
) -> Result<usize, Errno> {
    let mut buf = vec![0; size];
    zx::cprng_draw(&mut buf);
    current_task.mm.write_memory(buf_addr, &buf[0..size])?;
    Ok(size)
}

pub fn sys_reboot(
    current_task: &CurrentTask,
    magic: u32,
    magic2: u32,
    cmd: u32,
    _arg: UserAddress,
) -> Result<(), Errno> {
    if magic != LINUX_REBOOT_MAGIC1
        || (magic2 != LINUX_REBOOT_MAGIC2
            && magic2 != LINUX_REBOOT_MAGIC2A
            && magic2 != LINUX_REBOOT_MAGIC2B
            && magic2 != LINUX_REBOOT_MAGIC2C)
    {
        return error!(EINVAL);
    }
    if !current_task.read().creds.has_capability(CAP_SYS_BOOT) {
        return error!(EPERM);
    }
    // TODO(tbodt): only shut down the current Kernel rather than panicking the entire process
    panic!("starnix reboot({:#x})", cmd);
}

pub fn sys_clock_getres(
    current_task: &CurrentTask,
    which_clock: i32,
    tp_addr: UserRef<timespec>,
) -> Result<(), Errno> {
    if tp_addr.is_null() {
        return Ok(());
    }

    let tv = match which_clock as u32 {
        CLOCK_REALTIME
        | CLOCK_MONOTONIC
        | CLOCK_MONOTONIC_COARSE
        | CLOCK_MONOTONIC_RAW
        | CLOCK_BOOTTIME
        | CLOCK_THREAD_CPUTIME_ID
        | CLOCK_PROCESS_CPUTIME_ID => timespec { tv_sec: 0, tv_nsec: 1 },
        _ => {
            // Error if no dynamic clock can be found.
            let _ = get_dynamic_clock(current_task, which_clock)?;
            timespec { tv_sec: 0, tv_nsec: 1 }
        }
    };
    current_task.mm.write_object(tp_addr, &tv)?;
    Ok(())
}

pub fn sys_clock_gettime(
    current_task: &CurrentTask,
    which_clock: i32,
    tp_addr: UserRef<timespec>,
) -> Result<(), Errno> {
    let nanos = if which_clock < 0 {
        get_dynamic_clock(current_task, which_clock)?
    } else {
        match which_clock as u32 {
            CLOCK_REALTIME => utc_time().into_nanos(),
            CLOCK_MONOTONIC | CLOCK_MONOTONIC_COARSE | CLOCK_MONOTONIC_RAW | CLOCK_BOOTTIME => {
                zx::Time::get_monotonic().into_nanos()
            }
            CLOCK_THREAD_CPUTIME_ID => get_thread_cpu_time(current_task, current_task.id)?,
            CLOCK_PROCESS_CPUTIME_ID => get_process_cpu_time(current_task, current_task.id)?,
            _ => return error!(EINVAL),
        }
    };
    let tv = timespec { tv_sec: nanos / NANOS_PER_SECOND, tv_nsec: nanos % NANOS_PER_SECOND };
    current_task.mm.write_object(tp_addr, &tv)?;
    Ok(())
}

pub fn sys_gettimeofday(
    current_task: &CurrentTask,
    user_tv: UserRef<timeval>,
    user_tz: UserRef<timezone>,
) -> Result<(), Errno> {
    if !user_tv.is_null() {
        let tv = timeval_from_time(utc_time());
        current_task.mm.write_object(user_tv, &tv)?;
    }
    if !user_tz.is_null() {
        not_implemented!("gettimeofday does not implement tz argument");
    }
    return Ok(());
}

pub fn sys_nanosleep(
    current_task: &CurrentTask,
    user_request: UserRef<timespec>,
    user_remaining: UserRef<timespec>,
) -> Result<(), Errno> {
    let request = current_task.mm.read_object(user_request)?;
    let deadline = zx::Time::after(duration_from_timespec(request)?);
    match Waiter::new().wait_until(&current_task, deadline) {
        Err(err) if err == EINTR => {
            if !user_remaining.is_null() {
                let now = zx::Time::get_monotonic();
                let remaining = timespec_from_duration(std::cmp::max(
                    zx::Duration::from_nanos(0),
                    deadline - now,
                ));
                current_task.mm.write_object(user_remaining, &remaining)?;
            }
        }
        Err(err) if err == ETIMEDOUT => return Ok(()),
        non_eintr => non_eintr?,
    }
    Ok(())
}

pub fn sys_unknown(_ctx: &CurrentTask, syscall_number: u64) -> Result<SyscallResult, Errno> {
    warn!(target: "unknown_syscall", "UNKNOWN syscall({}): {}", syscall_number, SyscallDecl::from_number(syscall_number).name);
    // TODO: We should send SIGSYS once we have signals.
    error!(ENOSYS)
}

/// Returns the cpu time for the task with the given `pid`.
///
/// Returns EINVAL if no such task can be found.
fn get_thread_cpu_time(current_task: &CurrentTask, pid: pid_t) -> Result<i64, Errno> {
    let task = current_task.get_task(pid).ok_or(errno!(EINVAL))?;
    Ok(task.thread.get_runtime_info().map_err(|status| from_status_like_fdio!(status))?.cpu_time)
}

/// Returns the cpu time for the process associated with the given `pid`. `pid`
/// can be the `pid` for any task in the thread_group (so the caller can get the
/// process cpu time for any `task` by simply using `task.pid`).
///
/// Returns EINVAL if no such process can be found.
fn get_process_cpu_time(current_task: &CurrentTask, pid: pid_t) -> Result<i64, Errno> {
    let task = current_task.get_task(pid).ok_or(errno!(EINVAL))?;
    Ok(task
        .thread_group
        .process
        .get_runtime_info()
        .map_err(|status| from_status_like_fdio!(status))?
        .cpu_time)
}

/// Returns the type of cpu clock that `clock` encodes.
fn which_cpu_clock(clock: i32) -> i32 {
    const CPU_CLOCK_MASK: i32 = 3;
    clock & CPU_CLOCK_MASK
}

/// Returns whether or not `clock` encodes a valid clock type.
fn is_valid_cpu_clock(clock: i32) -> bool {
    const MAX_CPU_CLOCK: i32 = 3;
    if clock & 7 == 7 {
        return false;
    }
    if which_cpu_clock(clock) >= MAX_CPU_CLOCK {
        return false;
    }

    true
}

/// Returns the pid encoded in `clock`.
fn pid_of_clock_id(clock: i32) -> pid_t {
    // The pid is stored in the most significant 29 bits.
    !(clock >> 3) as pid_t
}

/// Returns true if the clock references a thread specific clock.
fn is_thread_clock(clock: i32) -> bool {
    const PER_THREAD_MASK: i32 = 4;
    clock & PER_THREAD_MASK != 0
}

/// Returns the cpu time for the clock specified in `which_clock`.
///
/// This is to support "dynamic clocks."
/// https://man7.org/linux/man-pages/man2/clock_gettime.2.html
///
/// `which_clock` is decoded as follows:
///   - Bit 0 and 1 are used to determine the type of clock.
///   - Bit 3 is used to determine whether the clock is for a thread or process.
///   - The remaining bits encode the pid of the thread/process.
fn get_dynamic_clock(current_task: &CurrentTask, which_clock: i32) -> Result<i64, Errno> {
    if !is_valid_cpu_clock(which_clock) {
        return error!(EINVAL);
    }

    let pid = pid_of_clock_id(which_clock);

    if is_thread_clock(which_clock) {
        get_thread_cpu_time(current_task, pid)
    } else {
        get_process_cpu_time(current_task, pid)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::task::*;
    use crate::testing::*;

    #[::fuchsia::test]
    fn test_nanosleep_without_remainder() {
        let (_kernel, current_task) = create_kernel_and_task();

        let task_clone = current_task.task_arc_clone();

        let thread = std::thread::spawn(move || {
            // Wait until the task is in nanosleep, and interrupt it.
            while !task_clone.interrupt(InterruptionType::Signal) {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });

        let duration = timespec_from_duration(zx::Duration::from_seconds(60));
        let address = map_memory(
            &current_task,
            UserAddress::default(),
            std::mem::size_of::<timespec>() as u64,
        );
        current_task.mm.write_object(address.into(), &duration).expect("write_object");

        // nanosleep will be interrupted by the current thread and should not fail because the
        // remainder pointer is null.
        assert_eq!(sys_nanosleep(&current_task, address.into(), UserRef::default()), Ok(()));

        thread.join().expect("join");
    }
}
