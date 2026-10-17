// Copyright 2024-2025, dependabot[bot], shadow3aaa
//
// This file is part of fas-rs.
//
// fas-rs is free software: you can redistribute it and/or modify it under
// the terms of the GNU General Public License as published by the Free
// Software Foundation, either version 3 of the License, or (at your option)
// any later version.
//
// fas-rs is distributed in the hope that it will be useful, but WITHOUT ANY
// WARRANTY; without even the implied warranty of MERCHANTABILITY or FITNESS
// FOR A PARTICULAR PURPOSE. See the GNU General Public License for more
// details.
//
// You should have received a copy of the GNU General Public License along
// with fas-rs. If not, see <https://www.gnu.org/licenses/>.

pub mod calculate;

use std::{
    collections::VecDeque,
    time::{Duration, Instant},
};

use libc::pid_t;
use likely_stable::unlikely;

use crate::{Extension, framework::config::TargetFps};

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum BufferWorkingState {
    Unusable,
    Usable,
}

#[derive(Debug)]
pub struct PackageInfo {
    pub pid: pid_t,
    pub pkg: String,
}

#[derive(Debug)]
pub struct FrameTimeState {
    pub current_fps_long: f64,
    pub avg_time_long: Duration,
    pub current_fps_short: f64,
    pub avg_time_short: Duration,
    pub frametimes: VecDeque<Duration>,
    pub additional_frametime: Duration,
}

impl FrameTimeState {
    fn new() -> Self {
        Self {
            current_fps_long: 0.0,
            avg_time_long: Duration::ZERO,
            current_fps_short: 0.0,
            avg_time_short: Duration::ZERO,
            frametimes: VecDeque::with_capacity(1440),
            additional_frametime: Duration::ZERO,
        }
    }
}

#[derive(Debug)]
pub struct TargetFpsState {
    pub target_fps: Option<u32>,
    target_fps_config: TargetFps,
}

impl TargetFpsState {
    const fn new(target_fps_config: TargetFps) -> Self {
        Self {
            target_fps: None,
            target_fps_config,
        }
    }
}

/// 帧率档位切换的防抖器。
///
/// 游戏切帧率（大厅 30fps → 游戏内 120fps）时，帧时间窗口需要若干
/// 控制周期才能反映新档位。本结构通过"连续确认"机制避免瞬时卡顿
/// 被误判为档位切换。
#[derive(Debug, Default)]
pub struct FpsSwitcher {
    current: Option<u32>,
    candidate: Option<u32>,
    confirm_count: u8,
}

impl FpsSwitcher {
    /// 连续多少个控制周期检测到同一新档位才真正切换。
    /// 控制周期约 100ms 时，3 次约 300ms 完成切换。
    const CONFIRM_THRESHOLD: u8 = 3;

    fn update(&mut self, detected: u32) -> u32 {
        let Some(current) = self.current else {
            self.current = Some(detected);
            return detected;
        };

        if detected == current {
            self.candidate = None;
            self.confirm_count = 0;
            return current;
        }

        if self.candidate == Some(detected) {
            self.confirm_count = self.confirm_count.saturating_add(1);
            if self.confirm_count >= Self::CONFIRM_THRESHOLD {
                self.current = Some(detected);
                self.candidate = None;
                self.confirm_count = 0;
            }
        } else {
            self.candidate = Some(detected);
            self.confirm_count = 1;
        }

        self.current.unwrap_or(detected)
    }
}

#[derive(Debug)]
pub struct BufferState {
    pub last_update: Instant,
    pub working_state: BufferWorkingState,
    calculate_timer: Instant,
    working_state_timer: Instant,
}

impl BufferState {
    fn new() -> Self {
        let now = Instant::now();
        Self {
            last_update: now,
            working_state: BufferWorkingState::Unusable,
            calculate_timer: now,
            working_state_timer: now,
        }
    }
}

#[derive(Debug)]
pub struct Buffer {
    pub package_info: PackageInfo,
    pub frametime_state: FrameTimeState,
    pub target_fps_state: TargetFpsState,
    pub state: BufferState,
    pub fps_switcher: FpsSwitcher,
}

impl Buffer {
    pub fn new(target_fps_config: TargetFps, pid: pid_t, pkg: String) -> Self {
        Self {
            package_info: PackageInfo { pid, pkg },
            frametime_state: FrameTimeState::new(),
            target_fps_state: TargetFpsState::new(target_fps_config),
            state: BufferState::new(),
            fps_switcher: FpsSwitcher::default(),
        }
    }

    pub fn push_frametime(&mut self, d: Duration, extension: &Extension) {
        self.frametime_state.additional_frametime = Duration::ZERO;
        self.state.last_update = Instant::now();

        while self.frametime_state.frametimes.len()
            >= self.target_fps_state.target_fps.unwrap_or(144) as usize * 5
        {
            self.frametime_state.frametimes.pop_back();
            self.try_usable();
        }

        self.frametime_state.frametimes.push_front(d);
        self.try_calculate(extension);
    }

    fn try_calculate(&mut self, extension: &Extension) {
        self.calculate_current_fps();
        if unlikely(self.state.calculate_timer.elapsed() >= Duration::from_millis(100)) {
            self.state.calculate_timer = Instant::now();
            self.calculate_target_fps(extension);
        }
    }

    pub fn try_usable(&mut self) {
        if self.state.working_state == BufferWorkingState::Unusable
            && self.state.working_state_timer.elapsed() >= Duration::from_secs(1)
        {
            self.state.working_state = BufferWorkingState::Usable;
        }
    }

    pub fn unusable(&mut self) {
        self.state.working_state = BufferWorkingState::Unusable;
        self.state.working_state_timer = Instant::now();
    }

    pub fn additional_frametime(&mut self, extension: &Extension) {
        self.frametime_state.additional_frametime = self.state.last_update.elapsed();
        self.try_calculate(extension);
    }
}
