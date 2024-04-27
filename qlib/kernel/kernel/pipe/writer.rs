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

use alloc::sync::Arc;
use core::any::Any;
use core::ops::Deref;

use super::super::super::super::common::*;
use super::super::super::super::linux_def::*;
use super::super::super::super::mem::seq::*;
use super::super::super::fs::attr::*;
use super::super::super::fs::dentry::*;
use super::super::super::fs::dirent::*;
use super::super::super::fs::file::*;
use super::super::super::fs::host::hostinodeop::*;
use super::super::super::kernel::waiter::*;
use super::super::super::task::*;
use super::pipe::*;

#[derive(Clone)]
pub struct Writer(pub Arc<WriterInner>);
impl Deref for Writer {
    type Target = Arc<WriterInner>;

    fn deref(&self) -> &Arc<WriterInner> {
        &self.0
    }
}

pub struct WriterInner {
    pub pipe: Pipe,
}

impl Deref for WriterInner {
    type Target = Pipe;

    fn deref(&self) -> &Pipe {
        &self.pipe
    }
}

impl Drop for WriterInner {
    fn drop(&mut self) {
        self.pipe.WClose();

        // Wake up readers.
        self.pipe.Notify(EVENT_HUP);
    }
}

impl SpliceOperations for Writer {
    fn ReadFrom(&self, task: &Task, _file: &File, src: &File, opts: &SpliceOpts) -> Result<i64> {
        let n = self.pipe.ReadFrom(task, src, opts)?;
        if n > 0 {
            self.pipe.Notify(READABLE_EVENT)
        }

        return Ok(n as i64);
    }
}

impl FileOperations for Writer {
    fn as_any(&self) -> &Any {
        return self;
    }

    fn FopsType(&self) -> FileOpsType {
        return FileOpsType::Writer;
    }

    fn Seekable(&self) -> bool {
        return false;
    }

    fn Seek(
        &self,
        _task: &Task,
        _f: &File,
        _whence: i32,
        _current: i64,
        _offset: i64,
    ) -> Result<i64> {
        return Err(Error::SysError(SysErr::ESPIPE));
    }

    fn ReadDir(
        &self,
        _task: &Task,
        _f: &File,
        _offset: i64,
        _serializer: &mut DentrySerializer,
    ) -> Result<i64> {
        return Err(Error::SysError(SysErr::ENOTDIR));
    }

    fn ReadAt(
        &self,
        task: &Task,
        _f: &File,
        dsts: &mut [IoVec],
        _offset: i64,
        _blocking: bool,
    ) -> Result<i64> {
        //error!("pipe writer readat id {}, reader is {}", self.pipe.Uid(), self.pipe.Readers());
        let dsts = BlockSeq::NewFromSlice(dsts);
        let n = self.pipe.Read(task, dsts)?;
        if n > 0 {
            self.pipe.Notify(WRITEABLE_EVENT);
            return Ok(n as i64);
        }

        if dsts.NumBytes() == 0 {
            return Ok(0);
        }

        return Err(Error::SysError(SysErr::EAGAIN));
    }

    fn WriteAt(
        &self,
        task: &Task,
        _f: &File,
        srcs: &[IoVec],
        _offset: i64,
        _blocking: bool,
    ) -> Result<i64> {
        //error!("pipe writer WriteAt id {}, writers is {}", self.pipe.Uid(), self.pipe.Writers());
        let size = IoVec::NumBytes(srcs);
        let mut buf = DataBuff::New(size);
        let len = task.CopyDataInFromIovs(&mut buf.buf, srcs, true)?;
        let s = alloc::string::String::from_utf8(buf.buf.clone());
        let srcs = BlockSeq::New(&buf.buf[0..len]);

        let n = match self.pipe.Write(task, srcs) {
            Err(e) => {
                return Err(e);
            }
            Ok(n) => n,
        };

        if n > 0 {
            self.pipe.Notify(READABLE_EVENT);
            return Ok(n as i64);
        }

        if srcs.NumBytes() == 0 {
            return Ok(0);
        }

        return Err(Error::SysError(SysErr::EAGAIN));
    }

    fn Append(&self, task: &Task, f: &File, srcs: &[IoVec]) -> Result<(i64, i64)> {
        let n = self.WriteAt(task, f, srcs, 0, false)?;
        return Ok((n, 0));
    }

    fn Fsync(
        &self,
        _task: &Task,
        _f: &File,
        _start: i64,
        _end: i64,
        _syncType: SyncType,
    ) -> Result<()> {
        return Err(Error::SysError(SysErr::EINVAL));
    }

    fn Flush(&self, _task: &Task, _f: &File) -> Result<()> {
        return Ok(());
    }

    fn UnstableAttr(&self, task: &Task, f: &File) -> Result<UnstableAttr> {
        let inode = f.Dirent.Inode();
        return inode.UnstableAttr(task);
    }

    fn Ioctl(&self, task: &Task, _f: &File, _fd: i32, request: u64, val: u64) -> Result<()> {
        if request == IoCtlCmd::FIONREAD {
            let mut v = self.pipe.Queued();
            if v > core::i32::MAX as usize {
                // Silently truncate.
                v = core::i32::MAX as usize
            }

            //*task.GetTypeMut(val)? = v as i32;
            task.CopyOutObj(&v, val)?;
            return Ok(());
        }
        return Err(Error::SysError(SysErr::ENOTTY));
    }

    fn IterateDir(
        &self,
        _task: &Task,
        _d: &Dirent,
        _dirCtx: &mut DirCtx,
        _offset: i32,
    ) -> (i32, Result<i64>) {
        return (0, Err(Error::SysError(SysErr::ENOTDIR)));
    }

    fn Mappable(&self) -> Result<MMappable> {
        return Err(Error::SysError(SysErr::ENODEV));
    }
}

impl Waitable for Writer {
    fn Readiness(&self, _task: &Task, mask: EventMask) -> EventMask {
        return self.pipe.WReadiness() & mask;
    }

    fn EventRegister(&self, task: &Task, e: &WaitEntry, mask: EventMask) {
        return self.pipe.queue.EventRegister(task, e, mask);
    }

    fn EventUnregister(&self, task: &Task, e: &WaitEntry) {
        return self.pipe.queue.EventUnregister(task, e);
    }
}

impl SockOperations for Writer {}
