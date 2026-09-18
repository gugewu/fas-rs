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

mod buffer;
mod clean;
mod policy;

use std::time::{Duration, Instant};

use frame_analyzer::Analyzer;
use likely_stable::{likely, unlikely};
#[cfg(debug_assertions)]
use log::debug;
use log::info;

pub use policy::ControllerParams;
use policy::controll::calculate_control;

use super::{FasData, thermal::Thermal, topapp::TopAppsWatcher};
use crate::{
    Controller,
    api::{trigger_load_fas, trigger_start_fas, trigger_stop_fas, trigger_unload_fas},
    framework::{
        Extension,
        config::Config,
        error::Result,
        node::{Mode, Node},
        pid_utils::get_process_name,
    },
};

use buffer::{Buffer, BufferWorkingState};
use clean::Cleaner;

const DELAY_TIME: Duration = Duration::from_secs(3);

#[derive(PartialEq)]
enum State {
    NotWorking,
    Waiting,
    Working,
}

struct FasState {
    mode: Mode,
    working_state: State,
    delay_timer: Instant,
    buffer: Option<Buffer>,
}

struct AnalyzerState {
    analyzer: Analyzer,
    restart_counter: u8,
    restart_timer: Instant,
}

pub(crate) struct ControllerState {
    controller: Controller,
    params: ControllerParams,
    target_fps_offset: f64,
    usage_sample_timer: Instant,
    is_janked: bool,   // 当前帧是否卡顿
    max_freq: isize,   // 全局最大频率，用于限制单次控制幅度
    // [MODIFIED] 以下三个字段用于消除多档位 fps 判定抖动
    /// 上一帧 fps_ok 的状态，用于双阈值滞回
    was_fps_ok: bool,
    /// 上一次使用的控制模式：true = 利用率闭环，false = PID
    last_control_mode: bool,
    /// 上一次控制模式切换的时间，用于最小驻留时间限制
    last_mode_switch: Instant,
}

pub struct Looper {
    analyzer_state: AnalyzerState,
    config: Config,
    node: Node,
    extension: Extension,
    therminal: Thermal,
    windows_watcher: TopAppsWatcher,
    cleaner: Cleaner,
    fas_state: FasState,
    controller_state: ControllerState,
    // [MODIFIED] 记录上一次的 target_fps，用于检测帧率档位切换
    last_target_fps: Option<u32>,
}

impl Looper {
    pub fn new(
        analyzer: Analyzer,
        config: Config,
        node: Node,
        extension: Extension,
        controller: Controller,
    ) -> Self {
        let params = config.controller_params.clone();

        Self {
            analyzer_state: AnalyzerState {
                analyzer,
                restart_counter: 0,
                restart_timer: Instant::now(),
            },
            config,
            node,
            extension,
            therminal: Thermal::new().unwrap(),
            windows_watcher: TopAppsWatcher::new(),
            cleaner: Cleaner::new(),
            fas_state: FasState {
                mode: Mode::Balance,
                buffer: None,
                working_state: State::NotWorking,
                delay_timer: Instant::now(),
            },
            controller_state: ControllerState {
                controller,
                params,
                target_fps_offset: 0.0,
                usage_sample_timer: Instant::now(),
                is_janked: false,
                max_freq: 2_918_400, // 会在 do_policy 里按设备实际最高频刷新
                // [MODIFIED] 初始化新增字段
                was_fps_ok: false,
                last_control_mode: false,
                // 初始设为 10 秒前，保证首次判定可以立即进入 demand 模式，
                // 避免启动后多跑 500ms PID。
                last_mode_switch: Instant::now() - Duration::from_secs(10),
            },
            // [MODIFIED]
            last_target_fps: None,
        }
    }

    pub fn enter_loop(&mut self) -> Result<()> {
        loop {
            self.switch_mode();
            let _ = self.update_analyzer();
            self.retain_topapp();

            if self.windows_watcher.visible_freeform_window() {
                self.disable_fas();
            }

            if let Some(data) = self.recv_message() {
                #[cfg(debug_assertions)]
                debug!("original frametime: {:?}", data.frametime);

                if let Some(state) = self.buffer_update(&data) {
                    match state {
                        BufferWorkingState::Usable => self.do_policy(),
                        BufferWorkingState::Unusable => self.disable_fas(),
                    }
                }
            } else if let Some(buffer) = self.fas_state.buffer.as_mut() {
                #[cfg(debug_assertions)]
                debug!("janked !");
                buffer.additional_frametime(&self.extension);

                match buffer.state.working_state {
                    BufferWorkingState::Unusable => {
                        self.restart_analyzer();
                        self.disable_fas();
                    }
                    BufferWorkingState::Usable => self.do_policy(),
                }
            }
        }
    }

    fn switch_mode(&mut self) {
        if let Ok(new_mode) = self.node.get_mode() {
            if likely(self.fas_state.mode != new_mode) {
                info!("Switch mode: {} -> {}", self.fas_state.mode, new_mode);
                self.fas_state.mode = new_mode;
                if self.fas_state.working_state == State::Working {
                    self.controller_state.controller.init_game(
                        self.fas_state.buffer.as_ref().unwrap().package_info.pid,
                        &self.extension,
                    );
                }
            }
        }
    }

    fn recv_message(&mut self) -> Option<FasData> {
        self.analyzer_state
            .analyzer
            .recv_timeout(Duration::from_millis(100))
            .map(|(pid, frametime)| FasData { pid, frametime })
    }

    fn update_analyzer(&mut self) -> Result<()> {
        for pid in self.windows_watcher.topapp_pids().iter().copied() {
            let pkg = get_process_name(pid)?;
            if self.config.need_fas(&pkg) {
                self.analyzer_state.analyzer.attach_app(pid)?;
            }
        }
        Ok(())
    }

    fn restart_analyzer(&mut self) {
        if self.analyzer_state.restart_counter == 1 {
            if self.analyzer_state.restart_timer.elapsed() >= Duration::from_secs(1) {
                self.analyzer_state.restart_timer = Instant::now();
                self.analyzer_state.restart_counter = 0;
                self.analyzer_state.analyzer.detach_apps();
                let _ = self.update_analyzer();
            }
        } else {
            self.analyzer_state.restart_counter += 1;
        }
    }

    fn do_policy(&mut self) {
        if unlikely(self.fas_state.working_state != State::Working) {
            #[cfg(debug_assertions)]
            debug!("Not running policy!");
            return;
        }

        // [MODIFIED] 帧率档位切换检测。
        //
        // 多档位（60/90/120）时，若游戏切换档位，旧档位的帧时间会
        // 污染新档位的 fps 判定，导致 fps_ok 在边界抖动，控制模式
        // 反复横跳。检测到档位变化时清空帧时间窗口，并跳过本次
        // 调频，等新档位的帧填满后再决策。
        let current_target = self
            .fas_state
            .buffer
            .as_ref()
            .and_then(|b| b.target_fps_state.target_fps);

        if self.last_target_fps.is_some() && self.last_target_fps != current_target {
            #[cfg(debug_assertions)]
            debug!(
                "target_fps changed: {:?} -> {:?}, clearing buffer",
                self.last_target_fps, current_target
            );

            if let Some(buffer) = self.fas_state.buffer.as_mut() {
                buffer.frametime_state.frametimes.clear();
                buffer.frametime_state.additional_frametime = Duration::ZERO;
            }
            self.last_target_fps = current_target;
            return;
        }
        self.last_target_fps = current_target;

        // 1. 刷新各 policy 的 CPU 利用率
        for cpu in self.controller_state.controller.cpu_infos_mut() {
            cpu.refresh_cpu_usage();
        }

        // 2. 取所有 policy 中的最大利用率
        let max_cpu_util = self
            .controller_state
            .controller
            .cpu_infos()
            .iter()
            .map(|cpu| cpu.cpu_usage() as f64)
            .fold(0.0f64, f64::max);

        #[cfg(debug_assertions)]
        debug!("max_cpu_util: {max_cpu_util:.4}");

        // 3. 动态刷新 max_freq（设备实际最高频）
        if let Some(mf) = self
            .controller_state
            .controller
            .cpu_infos()
            .iter()
            .flat_map(|info| info.freqs.iter().copied())
            .max()
        {
            self.controller_state.max_freq = mf as isize;
        }

        // 4. 调用 calculate_control，返回 (control, is_janked)
        let (control, is_janked) = if let Some(buffer) = &self.fas_state.buffer {
            let target_fps_offset = self
                .therminal
                .target_fps_offset(&mut self.config, self.fas_state.mode);

            calculate_control(
                buffer,
                &mut self.config,
                self.fas_state.mode,
                &mut self.controller_state,
                target_fps_offset,
                max_cpu_util,
            )
            .unwrap_or_default()
        } else {
            return;
        };

        self.controller_state.is_janked = is_janked;

        #[cfg(debug_assertions)]
        debug!("control: {control}khz");

        self.controller_state
            .controller
            .fas_update_freq(control, is_janked);
    }

    pub fn retain_topapp(&mut self) {
        if let Some(buffer) = self.fas_state.buffer.as_ref() {
            if !self
                .windows_watcher
                .topapp_pids()
                .contains(&buffer.package_info.pid)
            {
                let _ = self
                    .analyzer_state
                    .analyzer
                    .detach_app(buffer.package_info.pid);
                let pkg = buffer.package_info.pkg.clone();
                trigger_unload_fas(&self.extension, buffer.package_info.pid, pkg);
                self.fas_state.buffer = None;
            }
        }

        if self.fas_state.buffer.is_none() {
            self.disable_fas();
        } else {
            self.enable_fas();
        }
    }

    pub fn disable_fas(&mut self) {
        match self.fas_state.working_state {
            State::Working => {
                self.fas_state.working_state = State::NotWorking;
                self.cleaner.undo_cleanup();
                self.controller_state
                    .controller
                    .init_default(&self.extension);
                trigger_stop_fas(&self.extension);
            }
            State::Waiting => self.fas_state.working_state = State::NotWorking,
            State::NotWorking => (),
        }
    }

    pub fn enable_fas(&mut self) {
        match self.fas_state.working_state {
            State::NotWorking => {
                self.fas_state.working_state = State::Waiting;
                self.fas_state.delay_timer = Instant::now();
                trigger_start_fas(&self.extension);
            }
            State::Waiting => {
                if self.fas_state.delay_timer.elapsed() > DELAY_TIME {
                    self.fas_state.working_state = State::Working;
                    self.cleaner.cleanup();
                    self.controller_state.target_fps_offset = 0.0;
                    // [MODIFIED] 进入 Working 时重置新增状态，避免上一局
                    // 游戏的 fps_ok / 控制模式残留影响本次判定。
                    self.controller_state.was_fps_ok = false;
                    self.controller_state.last_control_mode = false;
                    self.controller_state.last_mode_switch =
                        Instant::now() - Duration::from_secs(10);
                    self.last_target_fps = None;

                    self.controller_state.controller.init_game(
                        self.fas_state.buffer.as_ref().unwrap().package_info.pid,
                        &self.extension,
                    );
                }
            }
            State::Working => (),
        }
    }

    pub fn buffer_update(&mut self, data: &FasData) -> Option<BufferWorkingState> {
        if unlikely(
            !self.windows_watcher.topapp_pids().contains(&data.pid) || data.frametime.is_zero(),
        ) {
            return None;
        }

        let pid = data.pid;
        let frametime = data.frametime;

        if let Some(buffer) = self.fas_state.buffer.as_mut() {
            buffer.push_frametime(frametime, &self.extension);
            Some(buffer.state.working_state)
        } else {
            let Ok(pkg) = get_process_name(data.pid) else {
                return None;
            };
            let target_fps = self.config.target_fps(&pkg)?;

            info!("New fas buffer on: [{pkg}]");

            trigger_load_fas(&self.extension, pid, pkg.clone());

            let mut buffer = Buffer::new(target_fps, pid, pkg);
            buffer.push_frametime(frametime, &self.extension);

            self.fas_state.buffer = Some(buffer);

            // [MODIFIED] 新 buffer 建立时清空档位记录，让下一次 do_policy
            // 把当前 target_fps 当作基线记录下来，而不是误判为档位切换。
            self.last_target_fps = None;

            Some(BufferWorkingState::Unusable)
        }
    }
}
