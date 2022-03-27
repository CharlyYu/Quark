// Copyright (c) 2021 Quark Container Authors / 2018 The gVisor Authors.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

pub mod HostFileMap;
//pub mod TimerMgr;
pub mod syscall;
pub mod hostfdnotifier;
pub mod time;
pub mod host_pma_keeper;
pub mod random;
pub mod limits;
pub mod uringMgr;
pub mod host_uring;
pub mod kernel_io_thread;

use std::str;
use std::slice;
use std::fs;
use libc::*;
use std::marker::Send;
use serde_json;
use x86_64::structures::paging::PageTableFlags;
use tempfile::tempfile;
use std::os::unix::io::IntoRawFd;
use lazy_static::lazy_static;
use core::sync::atomic::AtomicU64;
use core::sync::atomic;

use super::runc::runtime::loader::*;
use super::runc::specutils::specutils::*;
use super::runc::container::mounts::*;
use super::qlib::*;
use super::qlib::task_mgr::*;
use super::qlib::common::{Error, Result};
use super::qlib::linux_def::*;
use super::qlib::pagetable::{PageTables};
use super::qlib::addr::{Addr};
use super::qlib::control_msg::*;
use super::qlib::qmsg::*;
use super::qlib::kernel::util::cstring::*;
use super::qlib::kernel::fs::host::dirent::Dirent64;
//use super::qlib::socket_buf::*;
use super::qlib::perf_tunning::*;
use super::qlib::kernel::guestfdnotifier::*;
use super::qlib::kernel::SignalProcess;
use super::namespace::MountNs;
use super::ucall::usocket::*;
use super::*;
use self::HostFileMap::fdinfo::*;
use self::syscall::*;
use self::random::*;
use self::limits::*;
use super::runc::runtime::signal_handle::*;
use super::kvm_vcpu::HostPageAllocator;
use super::kvm_vcpu::KVMVcpu;

const ARCH_SET_GS:u64 = 0x1001;
const ARCH_SET_FS:u64 = 0x1002;
const ARCH_GET_FS:u64 = 0x1003;
const ARCH_GET_GS:u64 = 0x1004;

lazy_static! {
    static ref UID: AtomicU64 = AtomicU64::new(1);
}

macro_rules! scan {
    ( $string:expr, $sep:expr, $( $x:ty ),+ ) => {{
        let mut iter = $string.split($sep);
        ($(iter.next().and_then(|word| word.parse::<$x>().ok()),)*)
    }}
}

pub fn NewUID() -> u64 {
    return UID.fetch_add(1, atomic::Ordering::SeqCst);
}

pub fn Init() {
    //self::fs::Init();
}

#[derive(Clone, Copy, Debug)]
pub struct WaitingMsgCall {
    pub taskId: TaskId,
    pub addr: u64,
    pub len: usize,
    pub retAddr: u64,
}

pub struct VMSpace {
    pub pageTables : PageTables,
    pub allocator: HostPageAllocator,
    pub hostAddrTop: u64,
    pub sharedLoasdOffset: u64,
    pub vdsoAddr: u64,
    pub vcpuCount: usize,
    pub vcpuMappingDelta: usize,

    pub rng: RandGen,
    pub args: Option<Args>,
    pub pivot: bool,
    pub waitingMsgCall: Option<WaitingMsgCall>,
    pub controlSock: i32,
    pub vcpus: Vec<Arc<KVMVcpu>>,
}

unsafe impl Sync for VMSpace {}
unsafe impl Send for VMSpace {}

impl VMSpace {
    ///////////start of file operation//////////////////////////////////////////////
    pub fn GetOsfd(hostfd: i32) -> Option<i32> {
        return IO_MGR.GetFdByHost(hostfd);
    }

    pub fn TlbShootdown(&self, vcpuMask: u64) -> u64 {
        let mut mask = 0;

        for i in 0..64 {
            if (1<<i) & vcpuMask != 0 {
                if self.vcpus[i].Signal(Signal::SIGCHLD) {
                    mask |= 1 << i;
                    SHARE_SPACE.scheduler.VcpuArr[i].InterruptTlbShootdown();
                }
            }
        }

        return mask;
    }

    pub fn GetFdInfo(hostfd: i32) -> Option<FdInfo> {
        return IO_MGR.GetByHost(hostfd);
    }

    pub fn ReadDir(dirfd: i32, data: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(dirfd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOReadDir(data)
    }

    pub fn Mount(&self, id: &str, rootfs: &str) -> Result<()> {
        let spec = &self.args.as_ref().unwrap().Spec;
        //let rootfs : &str = &spec.root.path;
        let cpath = format!("/{}", id);

        init_rootfs(spec, rootfs, &cpath, false)?;
        pivot_rootfs(&*rootfs)?;
        return Ok(())
    }

    pub fn PivotRoot(&self, rootfs: &str) {
        let mns = MountNs::New(rootfs.to_string());
        mns.PivotRoot();
    }

    pub fn WriteControlMsgResp(fd: i32, addr: u64, len: usize, close: bool) -> i64 {
        let buf = {
            let ptr = addr as * const u8;
            unsafe { slice::from_raw_parts(ptr, len) }
        };

        let resp : UCallResp = serde_json::from_slice(&buf[0..len]).expect("ControlMsgRet des fail");

        let usock = USocket {
            socket: fd,
        };

        match usock.SendResp(&resp) {
            Err(e) => error!("ControlMsgRet send resp fail with error {:?}", e),
            Ok(()) => (),
        }

        if close {
            usock.Drop();
        }

        return 0;
    }

    pub fn VCPUCount() -> usize {
        let mut cpuCount = num_cpus::get();

        if cpuCount < 2 {
            cpuCount = 2; // at least 2 vcpu (one for host io and the other for process vcpu)
        }

        if cpuCount > MAX_VCPU_COUNT {
            cpuCount = MAX_VCPU_COUNT;
        }

        return cpuCount
    }

    pub fn LoadProcessKernel(&mut self, processAddr: u64, buffLen: usize) -> i64 {
        let mut process = loader::Process::default();
        process.ID = self.args.as_ref().unwrap().ID.to_string();
        let spec = &mut self.args.as_mut().unwrap().Spec;

        let mut cwd = spec.process.cwd.to_string();
        if cwd.len() == 0 {
            cwd = "/".to_string();
        }
        process.Cwd = cwd;

        SetConole(spec.process.terminal);
        process.Terminal = spec.process.terminal;
        process.Args.append(&mut spec.process.args);
        process.Envs.append(&mut spec.process.env);

        //todo: credential fix.
        error!("LoadProcessKernel: need to study the user mapping handling...");
        process.UID = spec.process.user.uid;
        process.GID = spec.process.user.gid;
        process.AdditionalGids.append(&mut spec.process.user.additional_gids);
        process.limitSet = CreateLimitSet(&spec).expect("load limitSet fail").GetInternalCopy();
        process.Caps = Capabilities(false, &spec.process.capabilities);

        process.HostName = spec.hostname.to_string();

        process.NumCpu = self.vcpuCount as u32;
        process.ExecId = Some("".to_string());

        for i in 0..process.Stdiofds.len() {
            let osfd = unsafe {
                dup(i as i32) as i32
            };

            URING_MGR.lock().Addfd(osfd).unwrap();

            if  osfd < 0 {
                return osfd as i64
            }

            let hostfd = IO_MGR.AddFile(osfd);

            process.Stdiofds[i] = hostfd;
        }

        process.Root = "/".to_string();

        let rootfs = self.args.as_ref().unwrap().Rootfs.to_string();

        if self.pivot {
            self.PivotRoot(&rootfs);
        }

        //error!("LoadProcessKernel proces is {:?}", &process);

        let vec : Vec<u8> = serde_json::to_vec(&process).expect("LoadProcessKernel ser fail...");
        let buff = {
            let ptr = processAddr as *mut u8;
            unsafe { slice::from_raw_parts_mut(ptr, buffLen) }
        };

        assert!(vec.len() <= buff.len(), "LoadProcessKernel not enough space...");
        for i in 0..vec.len() {
            buff[i] = vec[i];
         }

        StartSignalHandle();

        //self.shareSpace.lock().AQHostInputCall(HostMsg::ExecProcess);

        return vec.len() as i64
    }

    pub fn TgKill(tgid: i32, tid: i32, signal: i32) -> i64 {
        let nr = SysCallID::sys_tgkill as usize;
        let ret = unsafe {
            syscall3(nr, tgid as usize, tid as usize, signal as usize) as i32
        };
        return ret as _;
    }

    pub fn CreateMemfd(len: i64) -> i64 {
        let uid = NewUID();
        let path = format!("/tmp/memfd_{}", uid);
        let cstr = CString::New(&path);

        let nr = SysCallID::sys_memfd_create as usize;
        let fd = unsafe {
            syscall2(nr, cstr.Ptr() as *const c_char as usize, 0) as i32
        };

        if fd < 0 {
            return Self::GetRet(fd as i64)
        }

        let ret = unsafe {
            ftruncate(fd, len)
        };

        if ret < 0 {
            unsafe {
                libc::close(fd);
            }
            return Self::GetRet(ret as i64)
        }

        let hostfd = IO_MGR.AddFile(fd);
        return hostfd as i64
    }

    pub fn Fallocate(fd: i32, mode: i32, offset: i64, len: i64) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            fallocate(fd, mode, offset, len)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn RenameAt(olddirfd: i32, oldpath: u64, newdirfd: i32, newpath: u64) -> i64 {
        let olddirfd = {
            if olddirfd > 0 {
                match Self::GetOsfd(olddirfd) {
                    Some(olddirfd) => olddirfd,
                    None => return -SysErr::EBADF as i64,
                }
            } else {
                olddirfd
            }
        };

        let newdirfd = {
            if newdirfd > 0 {
                match Self::GetOsfd(newdirfd) {
                    Some(newdirfd) => newdirfd,
                    None => return -SysErr::EBADF as i64,
                }
            } else {
                newdirfd
            }
        };

        let ret = unsafe {
            renameat(olddirfd, oldpath as *const c_char, newdirfd, newpath as *const c_char)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Ftruncate(fd: i32, len: i64) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            ftruncate64(fd, len)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn GetStr(string: u64) -> &'static str {
        let ptr = string as *const u8;
        let slice = unsafe { slice::from_raw_parts(ptr, 1024) };

        let len = {
            let mut res : usize = 0;
            for i in 0..1024 {
                if slice[i] == 0 {
                    res = i;
                    break
                }
            }

            res
        };

        return str::from_utf8(&slice[0..len]).unwrap();
    }

    pub fn GetStrWithLen(string: u64, len: u64) -> &'static str {
        let ptr = string as *const u8;
        let slice = unsafe { slice::from_raw_parts(ptr, len as usize) };

        return str::from_utf8(&slice[0..len as usize]).unwrap();
    }

    pub fn GetStrLen(string: u64) -> i64 {
        let ptr = string as *const u8;
        let slice = unsafe { slice::from_raw_parts(ptr, 1024) };

        let len = {
            let mut res : usize = 0;
            for i in 0..1024 {
                if slice[i] == 0 {
                    res = i;
                    break
                }
            }

            res
        };

        return (len+1) as i64
    }

    pub unsafe fn TryOpenHelper(dirfd: i32, name: u64) -> (i32, bool) {
        let flags = Flags::O_NOFOLLOW;
        let ret = libc::openat(dirfd, name as *const c_char, (flags | Flags::O_RDWR) as i32, 0);
        if ret > 0 {
            return (ret, true);
        }

        let err = Self::GetRet(ret as i64) as i32;
        if err == -SysErr::ENOENT {
            return (-SysErr::ENOENT, false)
        }

        let ret = libc::openat(dirfd, name as *const c_char, (flags | Flags::O_RDONLY) as i32, 0);
        if ret > 0 {
            return (ret, false);
        }

        let ret = libc::openat(dirfd, name as *const c_char, (flags | Flags::O_WRONLY) as i32, 0);
        if ret > 0 {
            return (ret, true);
        }

        let ret = libc::openat(dirfd, name as *const c_char, flags as i32 | Flags::O_PATH, 0);
        if ret > 0 {
            return (ret, false);
        }

        return (Self::GetRet(ret as i64) as i32, false)
    }

    pub fn TryOpenAt(dirfd: i32, name: u64, addr: u64) -> i64 {
        //info!("TryOpenAt: the filename is {}", Self::GetStr(name));
        let dirfd = if dirfd < 0 {
            dirfd
        } else {
            match Self::GetOsfd(dirfd) {
                Some(fd) => fd,
                None => return -SysErr::EBADF as i64,
            }
        };

        let tryOpenAt = unsafe {
            &mut *(addr as * mut TryOpenStruct)
        };

        let (fd, writeable) = unsafe {
            Self::TryOpenHelper(dirfd, name)
        };

        //error!("TryOpenAt dirfd {}, name {} ret {}", dirfd, Self::GetStr(name), fd);

        if fd < 0 {
            return fd as i64
        }

        let ret = unsafe {
            libc::fstat(fd, tryOpenAt.fstat as * const _ as u64 as *mut stat) as i64
        };

        if ret < 0 {
            unsafe {
                libc::close(fd);
            }
            return Self::GetRet(ret as i64)
        }

        tryOpenAt.writeable = writeable;
        let hostfd = IO_MGR.AddFile(fd);

        if tryOpenAt.fstat.IsRegularFile() {
            URING_MGR.lock().Addfd(hostfd).unwrap();
        }

        return hostfd as i64
    }

    pub fn CreateAt(dirfd: i32, fileName: u64, flags: i32, mode: i32, uid: u32, gid: u32, fstatAddr: u64) -> i32 {
        info!("CreateAt: the filename is {}, flag is {:x}, the mode is {:b}, owenr is {}:{}, dirfd is {}",
            Self::GetStr(fileName), flags, mode, uid, gid, dirfd);

        let dirfd = if dirfd < 0 {
            dirfd
        } else {
            match Self::GetOsfd(dirfd) {
                Some(fd) => fd,
                None => return -SysErr::EBADF as i32,
            }
        };

        unsafe {
            let osfd = libc::openat(dirfd, fileName as *const c_char, flags as c_int, mode as c_int);
            if osfd <= 0 {
                return Self::GetRet(osfd as i64) as i32
            }

            let ret = libc::fchown(osfd, uid, gid);
            if ret < 0 {
                libc::close(osfd);
                return Self::GetRet(ret as i64) as i32
            }

            let ret = libc::fstat(osfd, fstatAddr as *mut stat) as i64;

            if ret < 0 {
                libc::close(osfd);
                return Self::GetRet(ret as i64) as i32
            }

            let hostfd = IO_MGR.AddFile(osfd);

            URING_MGR.lock().Addfd(osfd).unwrap();

            return hostfd
        }
    }

    pub fn Close(fd: i32) -> i64 {
        let info = IO_MGR.RemoveFd(fd);

        URING_MGR.lock().Removefd(fd).unwrap();
        let res = if info.is_some() {
            0
        } else {
            -SysErr::EINVAL as i64
        };

        return res;
    }

    pub fn IORead(fd: i32, iovs: u64, iovcnt: i32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe{
            readv(fd as c_int, iovs as *const iovec, iovcnt) as i64
        };

        return Self::GetRet(ret as i64)
    }

    pub fn IOTTYRead(fd: i32, iovs: u64, iovcnt: i32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe{
            let opt : i32 = 1;
            // in some cases, tty read will blocked even after set unblock with fcntl
            // todo: this workaround, fix this
            ioctl(fd, FIONBIO, &opt);

            readv(fd as c_int, iovs as *const iovec, iovcnt) as i64
        };

        unsafe {
            let opt : i32 = 0;
            ioctl(fd, FIONBIO, &opt);
        }

        return Self::GetRet(ret as i64)
    }

    pub fn IOBufWrite(fd: i32, addr: u64, len: usize, offset: isize) -> i64 {
        PerfGoto(PerfType::BufWrite);
        defer!(PerfGofrom(PerfType::BufWrite));

        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOBufWrite(addr, len, offset);
    }

    pub fn IOWrite(fd: i32, iovs: u64, iovcnt: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOWrite(iovs, iovcnt)
    }

    pub fn UpdateWaitInfo(fd: i32, waitInfo: FdWaitInfo) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        fdInfo.UpdateWaitInfo(waitInfo);
        return 0;
    }

    pub fn IOAppend(fd: i32, iovs: u64, iovcnt: i32, fileLenAddr: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOAppend(iovs, iovcnt, fileLenAddr)
    }

    pub fn IOReadAt(fd: i32, iovs: u64, iovcnt: i32, offset: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOReadAt(iovs, iovcnt, offset)
    }

    pub fn IOWriteAt(fd: i32, iovs: u64, iovcnt: i32, offset: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOWriteAt(iovs, iovcnt, offset)
    }

    pub fn IOAccept(fd: i32, addr: u64, addrlen: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOAccept(addr, addrlen)
    }

    pub fn NewSocket(fd: i32) -> i64 {
        IO_MGR.AddSocket(fd);
        URING_MGR.lock().Addfd(fd).unwrap();
        return 0;
    }

    pub fn IOConnect(fd: i32, addr: u64, addrlen: u32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOConnect(addr, addrlen)
    }

    pub fn IORecvMsg(fd: i32, msghdr: u64, flags: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IORecvMsg(msghdr, flags)
    }

    pub fn IOSendMsg(fd: i32, msghdr: u64, flags: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOSendMsg(msghdr, flags)
    }

    pub fn Fcntl(fd: i32, cmd: i32, arg: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(info) => info,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOFcntl(cmd, arg)
    }

    pub fn IoCtl(fd: i32, cmd: u64, argp: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOIoCtl(cmd, argp)
    }

    pub fn SysSync() -> i64 {
        // as quark running inside container, assume sys_sync only works for the current fs namespace
        // todo: confirm this
        unsafe {
            libc::sync()
        };

        return 0;
    }

    pub fn SyncFs(fd: i32) -> i64 {
        let osfd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            libc::syncfs(osfd) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn SyncFileRange(fd: i32, offset: i64, nbytes: i64, flags: u32) -> i64 {
        let osfd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            libc::sync_file_range(osfd, offset, nbytes, flags) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn FSync(fd: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOFSync(false)
    }

    pub fn FDataSync(fd: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOFSync(true)
    }

    pub fn Seek(fd: i32, offset: i64, whence: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(fd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOSeek(offset, whence)
    }

    pub fn ReadLinkAt(dirfd: i32, path: u64, buf: u64, bufsize: u64) -> i64 {
        //info!("ReadLinkAt: the path is {}", Self::GetStr(path));

        let dirfd = {
            if dirfd == -100 {
                dirfd
            } else {
                match Self::GetOsfd(dirfd) {
                    Some(dirfd) => dirfd,
                    None => return -SysErr::EBADF as i64,
                }
            }
        };

        let res = unsafe{ readlinkat(dirfd, path as *const c_char, buf as *mut c_char, bufsize as usize) };
        return Self::GetRet(res as i64)
    }

    pub fn Fstat(fd: i32, buf: u64) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            libc::fstat(fd, buf as *mut stat) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn Getxattr(path: u64, name: u64, value: u64, size: u64) -> i64 {
        info!("Getxattr: the path is {}, name is {}", Self::GetStr(path), Self::GetStr(name));
        let ret = unsafe {
            getxattr(path as *const c_char, name as *const c_char, value as *mut c_void, size as usize) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn Lgetxattr(path: u64, name: u64, value: u64, size: u64) -> i64 {
        info!("Lgetxattr: the path is {}, name is {}", Self::GetStr(path), Self::GetStr(name));
        let ret = unsafe {
            lgetxattr(path as *const c_char, name as *const c_char, value as *mut c_void, size as usize) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn Fgetxattr(fd: i32, name: u64, value: u64, size: u64) -> i64 {
        let fd = Self::GetOsfd(fd).expect("fgetxattr");
        let ret = unsafe {
            fgetxattr(fd, name as *const c_char, value as *mut c_void, size as usize) as i64
        };

        return Self::GetRet(ret);
    }

    pub fn GetRet(ret: i64) -> i64 {
        if ret == -1 {
            //info!("get error, errno is {}", errno::errno().0);
            return -errno::errno().0 as i64
        }

        return ret
    }

    pub fn Fstatat(dirfd: i32, pathname: u64, buf: u64, flags: i32) -> i64 {
        let dirfd = {
            if dirfd > 0 {
                Self::GetOsfd(dirfd).expect("Fstatat")
            } else {
                dirfd
            }
        };

        return unsafe {
            Self::GetRet(libc::fstatat(dirfd, pathname as *const c_char, buf as *mut stat, flags) as i64)
        };
    }

    pub fn Fstatfs(fd: i32, buf: u64) -> i64 {
        let fd = Self::GetOsfd(fd).expect("Fstatfs");

        let ret = unsafe{
            fstatfs(fd, buf as *mut statfs)
        };

        return Self::GetRet(ret as i64);
    }

    pub fn Unlinkat(dirfd: i32, pathname: u64, flags: i32) -> i64 {
        info!("Unlinkat: the pathname is {}", Self::GetStr(pathname));
        let dirfd = {
            if dirfd > 0 {
                match Self::GetOsfd(dirfd) {
                    Some(dirfd) => dirfd,
                    None => return -SysErr::EBADF as i64,
                }
            } else {
                dirfd
            }
        };

        let ret = unsafe {
            unlinkat(dirfd, pathname as *const c_char, flags)
        };

        return Self::GetRet(ret as i64);
    }

    pub fn Mkdirat(dirfd: i32, pathname: u64, mode_ : u32, uid: u32, gid: u32) -> i64 {
        info!("Mkdirat: the pathname is {}", Self::GetStr(pathname));

        let dirfd = {
            if dirfd > 0 {
                match Self::GetOsfd(dirfd) {
                    Some(dirfd) => dirfd,
                    None => return -SysErr::EBADF as i64,
                }
            } else {
                dirfd
            }
        };

        let ret = unsafe {
            mkdirat(dirfd, pathname as *const c_char, mode_ as mode_t)
        };

        Self::ChDirOwnerat(dirfd, pathname, uid, gid);

        return Self::GetRet(ret as i64);
    }

    pub fn ChDirOwnerat(dirfd: i32, pathname: u64, uid: u32, gid: u32) {
        unsafe {
            let ret = libc::fchownat(dirfd, pathname as *const c_char, uid, gid, 0);
            if ret < 0 {
                panic!("fchownat fail with error {}", Self::GetRet(ret as i64))
            }
        }
    }

    pub fn MSync(addr: u64, len: usize, flags: i32) -> i64 {
        let ret = unsafe{
            msync(addr as *mut c_void, len, flags)
        };

        return Self::GetRet(ret as i64);
    }

    pub fn MAdvise(addr: u64, len: usize, advise: i32) -> i64 {
        let ret = unsafe{
            madvise(addr as *mut c_void, len, advise)
        };

        return Self::GetRet(ret as i64);
    }

    pub fn FAccessAt(dirfd: i32, pathname: u64, mode: i32, flags: i32) -> i64 {
        info!("FAccessAt: the pathName is {}", Self::GetStr(pathname));
        let dirfd = {
            if dirfd == -100 {
                dirfd
            } else {
                match Self::GetOsfd(dirfd) {
                    Some(dirfd) => dirfd,
                    None => return -SysErr::EBADF as i64,
                }
            }
        };

        let ret = unsafe{
            faccessat(dirfd, pathname as *const c_char, mode, flags)
        };

        return Self::GetRet(ret as i64);
    }

    ///////////end of file operation//////////////////////////////////////////////


    ///////////start of network operation//////////////////////////////////////////////////////////////////

    pub fn Socket(domain: i32, type_: i32, protocol: i32) -> i64 {
        let fd = unsafe{
            socket(domain, type_ | SocketFlags::SOCK_NONBLOCK | SocketFlags::SOCK_CLOEXEC, protocol)
        };

        if fd < 0 {
            return Self::GetRet(fd as i64);
        }

        let hostfd = IO_MGR.AddSocket(fd);
        URING_MGR.lock().Addfd(fd).unwrap();
        return Self::GetRet(hostfd as i64);
    }

    pub fn GetSockName(sockfd: i32, addr: u64, addrlen: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOGetSockName(addr, addrlen)
    }

    pub fn GetPeerName(sockfd: i32, addr: u64, addrlen: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOGetPeerName(addr, addrlen)
    }

    pub fn GetSockOpt(sockfd: i32, level: i32, optname: i32, optval: u64, optlen: u64) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOGetSockOpt(level, optname, optval, optlen)
    }

    pub fn SetSockOpt(sockfd: i32, level: i32, optname: i32, optval: u64, optlen: u32) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOSetSockOpt(level, optname, optval, optlen)
    }

    pub fn Bind(sockfd: i32, sockaddr: u64, addrlen: u32, umask: u32) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOBind(sockaddr, addrlen, umask)
    }

    pub fn Listen(sockfd: i32, backlog: i32, block: bool) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOListen(backlog, block)
    }


    /*pub fn RDMAListen(sockfd: i32, backlog: i32, block: bool, acceptQueue: AcceptQueue) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.RDMAListen(backlog, block, acceptQueue)
    }

    pub fn RDMANotify(sockfd: i32, typ: RDMANotifyType) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.RDMANotify(typ)
    }

    pub fn PostRDMAConnect(msg: &'static mut PostRDMAConnect) {
        let fdInfo = match Self::GetFdInfo(msg.fd) {
            Some(fdInfo) => fdInfo,
            None => {
                msg.Finish(-SysErr::EBADF as i64);
                return;
            }
        };

        fdInfo.PostRDMAConnect(msg);
    }*/

    pub fn Shutdown(sockfd: i32, how: i32) -> i64 {
        let fdInfo = match Self::GetFdInfo(sockfd) {
            Some(fdInfo) => fdInfo,
            None => return -SysErr::EBADF as i64,
        };

        return fdInfo.IOShutdown(how)
    }

    ///////////end of network operation//////////////////////////////////////////////////////////////////
    pub fn ReadControlMsg(fd: i32, addr: u64, len: usize) -> i64 {
        match super::ucall::ucall_server::ReadControlMsg(fd) {
            Err(_e) => {
                return -1
            }
            Ok(msg) => {
                let vec : Vec<u8> = serde_json::to_vec(&msg).expect("SendControlMsg ser fail...");
                let buff = {
                    let ptr = addr as *mut u8;
                    unsafe { slice::from_raw_parts_mut(ptr, len) }
                };

                if vec.len() > buff.len() {
                    panic!("ReadControlMsg not enough space..., required len is {}, buff len is {}", vec.len(), buff.len());
                }

                for i in 0..vec.len() {
                    buff[i] = vec[i];
                }

                return vec.len() as i64
            }
        }
    }

    pub fn SchedGetAffinity( pid: i32, cpuSetSize: u64, mask: u64) -> i64 {
        //todo: fix this
        //let pid = 0;

        let ret = unsafe{
            sched_getaffinity(pid as pid_t, cpuSetSize as size_t, mask as *mut cpu_set_t)
        };

        //todo: fix this.
        if ret == 0 {
            return 8;
        } else {
            Self::GetRet(ret as i64)
        }
    }

    pub fn GetTimeOfDay(tv: u64, tz: u64) -> i64 {
        //let res = unsafe{ gettimeofday(tv as *mut timeval, tz as *mut timezone) };
        //return Self::GetRet(res as i64)

        let nr = SysCallID::sys_gettimeofday as usize;
        unsafe {
            let res = syscall2(nr, tv as usize, tz as usize) as i64;
            //error!("finish GetTimeOfDay");
            return res
        }
    }

    pub fn GetRandom(&mut self, buf: u64, len: u64, _flags: u32) -> i64 {
        unsafe {
            let slice = slice::from_raw_parts_mut(buf as *mut u8, len as usize);
            self.rng.Fill(slice);
        }

        return len as i64;
    }

    pub fn GetRandomU8(&mut self) -> u8 {
        let mut data : [u8; 1]  = [0; 1];
        self.rng.Fill(&mut data);
        return data[0]
    }

    pub fn RandomVcpuMapping(&mut self) {
        let delta = self.GetRandomU8() as usize;
        self.vcpuMappingDelta = delta % Self::VCPUCount();
        error!("RandomVcpuMapping {}", self.vcpuMappingDelta);
    }

    pub fn ComputeVcpuCoreId(&self, threadId: usize) -> usize {
        // skip core #0 for uring
        let DedicateUring = QUARK_CONFIG.lock().DedicateUring;
        let id = (threadId + self.vcpuMappingDelta + DedicateUring) % Self::VCPUCount();

        return id;
    }

    pub fn Fchdir(fd: i32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            fchdir(fd)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Sysinfo(info: u64) -> i64 {
        unsafe {
            return Self::GetRet(sysinfo(info as *mut sysinfo) as i64);
        }
    }

    pub fn Fadvise(fd: i32, offset: u64, len: u64, advice: i32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            posix_fadvise(fd, offset as i64, len as i64, advice)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Mlock2(addr: u64, len: u64, flags: u32) -> i64 {
        let nr = SysCallID::sys_mlock2 as usize;
        let ret = unsafe {
            syscall3(nr, addr as usize, len as usize, flags as usize) as i64
        };

        return Self::GetRet(ret as i64)
    }

    pub fn MUnlock(addr: u64, len: u64) -> i64 {
        let ret = unsafe {
            munlock(addr as *const c_void, len as size_t)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Chown(pathname: u64, owner: u32, group: u32) -> i64 {
        info!("Chown: the pathname is {}", Self::GetStr(pathname));

        let ret = unsafe {
            chown(pathname as *const c_char, owner, group)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn FChown(fd: i32, owner: u32, group: u32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            fchown(fd, owner, group)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Chmod(pathname: u64, mode: u32) -> i64 {
        let ret = unsafe {
            chmod(pathname as *const c_char, mode as mode_t)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Fchmod(fd: i32, mode: u32) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            fchmod(fd, mode as mode_t)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn EventfdWrite(fd: i32) -> i64 {
        let val: u64 = 8;

        let ret = unsafe {
            write(fd, &val as * const _ as _, 8)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn WaitFD(fd: i32, mask: EventMask) -> i64 {
        let fdinfo = match Self::GetFdInfo(fd) {
            Some(fdinfo) => fdinfo,
            None => return -SysErr::EBADF as i64,
        };

        let ret = fdinfo.lock().WaitFd(mask);

        match ret {
            Ok(()) => return 0,
            Err(Error::SysError(syserror)) => return -syserror as i64,
            Err(e) => {
                panic!("WaitFD get error {:?}", e);
            }
        }
    }

    pub fn NonBlockingPoll(fd: i32, mask: EventMask) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let mut e = pollfd {
            fd: fd,
            events: mask as i16,
            revents: 0,
        };

        loop {
            let ret = unsafe {
                poll(&mut e, 1, 0)
            };

            let ret = Self::GetRet(ret as i64) as i32;
            // Interrupted by signal, try again.
            if ret == -SysErr::EINTR {
                continue;
            }

            // If an error occur we'll conservatively say the FD is ready for
            // whatever is being checked.
            if ret < 0 {
                return mask as i64;
            }

            // If no FDs were returned, it wasn't ready for anything.
            if ret == 0 {
                return 0;
            }

            return e.revents as i64;
        }
    }

    pub fn NewTmpfile(addr: u64) -> i64 {
        let file = match tempfile() {
            Err(e) => {
                error!("create tempfs file fail with error {:?}", e);
                return -SysErr::ENOENT as i64;
            }
            Ok(f) => f,
        };

        //take the ownership of the fd
        let fd = file.into_raw_fd();

        let ret = unsafe {
            fstat(fd, addr as * mut stat)
        };

        if ret < 0 {
            unsafe {
                close(fd);
            }

            return Self::GetRet(ret as i64);
        }

        let guestfd = IO_MGR.AddFile(fd);

        return guestfd as i64
    }

    pub fn NewFifo() -> i64 {
        let uid = NewUID();
        let path = format!("/tmp/fifo_{}", uid);
        let cstr = CString::New(&path);
        let ret = unsafe {
            mkfifo(cstr.Ptr() as *const c_char, 0o666)
        };

        error!("NewFifo apth is {}, id is {}", path, ret);

        if ret < 0 {
            return Self::GetRet(ret as i64);
        }

        return uid as i64;
    }

    pub fn NewTmpfsFile(typ: TmpfsFileType, addr: u64) -> i64 {
        match typ {
            TmpfsFileType::File => Self::NewTmpfile(addr),
            TmpfsFileType::Fifo => {
                // Self::NewFifo()
                panic!("NewTmpfsFile doesn't support fifo");
            },
        }
    }

    pub fn Statm(buf: u64) -> i64 {
        const STATM : &str = "/proc/self/statm";
        let contents = fs::read_to_string(STATM)
            .expect("Something went wrong reading the file");

        let output = scan!(&contents, char::is_whitespace, u64, u64);
        let mut statm = unsafe {
            &mut *(buf as * mut StatmInfo)
        };

        statm.vss = output.0.unwrap();
        statm.rss = output.1.unwrap();
        return 0;
    }

    pub fn HostEpollWaitProcess() -> i64 {
        let ret = FD_NOTIFIER.HostEpollWait();
        return ret;
    }

    pub fn HostID(axArg: u32, cxArg: u32) -> (u32, u32, u32, u32) {
        let ax: u32;
        let bx: u32;
        let cx: u32;
        let dx: u32;
        unsafe {
            llvm_asm!("
              CPUID
            "
            : "={eax}"(ax), "={ebx}"(bx), "={ecx}"(cx), "={edx}"(dx)
            : "{eax}"(axArg), "{ecx}"(cxArg)
            :
            : );
        }

        return (ax, bx, cx, dx)
    }

    pub fn SymLinkAt(oldpath: u64, newdirfd: i32, newpath: u64) -> i64 {
        let newdirfd = match Self::GetOsfd(newdirfd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            symlinkat(oldpath as *const c_char, newdirfd, newpath as *const c_char)
        };

        return Self::GetRet(ret as i64)
    }

    pub fn Futimens(fd: i32, times: u64) -> i64 {
        let fd = match Self::GetOsfd(fd) {
            Some(fd) => fd,
            None => return -SysErr::EBADF as i64,
        };

        let ret = unsafe {
            futimens(fd, times as *const timespec)
        };

        return Self::GetRet(ret as i64)
    }

    //map kernel table
    pub fn KernelMap(&mut self, start: Addr, end: Addr, physical: Addr, flags: PageTableFlags) -> Result<bool> {
        error!("KernelMap start is {:x}, end is {:x}", start.0, end.0);
        return self.pageTables.Map(start, end, physical, flags, &mut self.allocator, true);
    }

    pub fn KernelMapHugeTable(&mut self, start: Addr, end: Addr, physical: Addr, flags: PageTableFlags) -> Result<bool> {
        error!("KernelMap1G start is {:x}, end is {:x}", start.0, end.0);
        return self.pageTables.MapWith1G(start, end, physical, flags, &mut self.allocator, true);
    }

    pub fn PrintStr(phAddr: u64) {
        unsafe {
            info!("the Str: {} ", str::from_utf8_unchecked(slice::from_raw_parts(phAddr as *const u8, strlen(phAddr as *const i8)+1)));
        }
    }

    pub fn UnblockFd(fd: i32) {
        unsafe {
            let flags = fcntl(fd, Cmd::F_GETFL, 0);
            let ret = fcntl(fd, Cmd::F_SETFL, flags | Flags::O_NONBLOCK);
            assert!(ret==0, "UnblockFd fail");
        }
    }

    pub fn BlockFd(fd: i32) {
        unsafe {
            let flags = fcntl(fd, Cmd::F_GETFL, 0);
            let ret = fcntl(fd, Cmd::F_SETFL, flags & !Flags::O_NONBLOCK);
            assert!(ret==0, "UnblockFd fail");
        }
    }

    pub fn GetStdfds(addr: u64) -> i64 {
        let ptr = addr as * mut i32;
        let stdfds = unsafe { slice::from_raw_parts_mut(ptr, 3) };

        for i in 0..stdfds.len() {
            let osfd = unsafe {
                dup(i as i32) as i32
            };

            if  osfd < 0 {
                return  osfd as i64
            }

            Self::UnblockFd(osfd);

            let hostfd = IO_MGR.AddFile(osfd);
            stdfds[i] = hostfd;
        }

        return 0;
    }

    pub fn Signal(&self, signal: SignalArgs) {
        SignalProcess(&signal);
        //SHARE_SPACE.AQHostInputCall(&HostInputMsg::Signal(signal));
    }

    pub fn LibcFstat(osfd: i32) -> Result<LibcStat> {
        let mut stat = LibcStat::default();
        let ret = unsafe {
            fstat(osfd, &mut stat as * mut _ as u64 as * mut stat)
        };

        if ret < 0 {
            info!("can't fstat osfd {}", osfd);
            return Err(Error::SysError(errno::errno().0))
        }

        //Self::LibcStatx(osfd);

        return Ok(stat)
    }

    pub fn LibcStatx(osfd: i32) {
        let statx = Statx::default();
        let addr : i8 = 0;
        let ret = unsafe {
            libc::statx(osfd, &addr as *const c_char, libc::AT_EMPTY_PATH, libc::STATX_BASIC_STATS, &statx as * const _ as u64 as * mut statx)
        };

        error!("LibcStatx osfd is {} ret is {} error is {}", osfd, ret, errno::errno().0);
    }

    pub fn GetVcpuFreq(&self) -> i64 {
        let freq = self.vcpus[0].vcpu.get_tsc_khz().unwrap() * 1000;
        return freq as i64
    }

    pub fn Init() -> Self {
        return VMSpace {
            allocator: HostPageAllocator::New(),
            pageTables: PageTables::default(),
            hostAddrTop: 0,
            sharedLoasdOffset: 0x0000_5555_0000_0000,
            vdsoAddr: 0,
            vcpuCount: 0,
            vcpuMappingDelta: 0,
            rng: RandGen::Init(),
            args: None,
            pivot: false,
            waitingMsgCall: None,
            controlSock: -1,
            vcpus: Vec::new(),
        }
    }
}

impl PostRDMAConnect {
    pub fn Finish(&mut self, ret: i64) {
        self.ret = ret;
        SHARE_SPACE.scheduler.ScheduleQ(self.taskId, self.taskId.Queue())
    }

    pub fn ToRef(addr: u64) -> &'static mut Self {
        let msgRef = unsafe {
            &mut *(addr as * mut Self)
        };

        return msgRef
    }
}