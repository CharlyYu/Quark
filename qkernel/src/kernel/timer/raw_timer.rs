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
use spin::Mutex;
use core::ops::Deref;

use super::super::super::kernel::timer::*;
use super::timermgr::*;
use super::TIMER_STORE;

pub trait Notifier: Sync + Send {
    fn Timeout(&self) -> i64;
    fn Reset(&self);
}

#[derive(Eq, PartialEq, Clone, Copy, Debug)]
pub enum TimerState {
    Expired,
    Running,
    Stopped,
}

impl Default for TimerState {
    fn default() -> Self {
        return Self::Stopped;
    }
}

pub struct RawTimerInternal {
    pub Id: u64,
    pub Timer: Timer,
    pub State: TimerState,
    pub SeqNo: u64,
    pub TM: TimerMgr,
    pub userData: u64,
}

#[derive(Clone)]
pub struct RawTimer(Arc<Mutex<RawTimerInternal>>);

impl Drop for RawTimer {
    fn drop(&mut self) {
        if Arc::strong_count(&self.0) == 1 {
            self.Drop();
        }
    }
}

impl Deref for RawTimer {
    type Target = Arc<Mutex<RawTimerInternal>>;

    fn deref(&self) -> &Arc<Mutex<RawTimerInternal>> {
        &self.0
    }
}

impl RawTimer {
    pub fn New(id: u64, tm: &TimerMgr, timer: &Timer) -> Self {
        let internal = RawTimerInternal {
            Id: id,
            Timer: timer.clone(),
            State: TimerState::default(),
            SeqNo: 0,
            TM: tm.clone(),
            userData: 0,
        };

        return Self(Arc::new(Mutex::new(internal)))
    }

    // Stop prevents the Timer from firing.
    // It returns true if the call stops the timer, false if the timer has already
    // expired or been stopped.
    // Stop does not close the channel, to prevent a read from the channel succeeding
    // incorrectly.
    pub fn Stop(&self) -> bool {
        let needTrigger = {
            let mut tm = TIMER_STORE.lock();
            let mut t = self.lock();
            let state = t.State;
            t.State = TimerState::Stopped;
            if state == TimerState::Running {
                tm.RemoveTimer(t.Id);
            }

            state == TimerState::Running
        };

        // we need to call the TimerRemove out of lock to avoid deadlock
        if needTrigger {
            TIMER_STORE.Trigger(0);
        }

        return false;
    }

    pub fn Reset(&self, delta: i64) -> bool {
        assert!(delta >= 0, "Timer::Reset get negtive delta");
        if delta == 0 { // cancel the timer
            {
                let mut tm = TIMER_STORE.lock();
                let timerId;
                {
                    let mut t = self.lock();
                    if t.State != TimerState::Running {
                        return false; //one out of data fire.
                    }

                    t.SeqNo += 1;
                    timerId = t.Id;
                }

                tm.RemoveTimer(timerId);
            }

            TIMER_STORE.Trigger(0);
            return true;
        }

        {
            let mut tm = TIMER_STORE.lock();
            let mut t = self.lock();
            t.State = TimerState::Running;
            t.SeqNo += 1;
            let timerId = t.Id;
            let seqNo = t.SeqNo;

            tm.ResetTimer(timerId, seqNo, delta);
        }

        TIMER_STORE.Trigger(0);
        return false;
    }

    pub fn Fire(&self, SeqNo: u64) {
        let timer = {
            let mut t = self.lock();
            if SeqNo != t.SeqNo || t.State != TimerState::Running {
                return; //one out of data fire.
            }

            t.State = TimerState::Expired;
            t.Timer.clone()
        };

        let delta = timer.Timeout();
        if delta > 0 {
            self.Reset(delta);
        }
    }

    pub fn Drop(&mut self) {
        self.Stop();
        let tm = self.lock().TM.clone();
        tm.RemoveTimer(self);
    }
}

