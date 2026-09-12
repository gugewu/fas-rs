// Copyright 2024-2025, shadow3aaa
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

pub mod controll;

#[derive(Debug, Copy, Clone)]
pub struct ControllerParams {
    pub kp: f64,

    /// 单次 control 调整的最大幅度（相对于 max_freq 的比例）
    /// 默认 0.15，即单次最多调整 15% 的最大频率
    pub max_step_ratio: f64,

    /// 利用率衰减阈值：CPU 利用率低于此值时，正向 control 线性衰减
    /// 默认 0.3，即利用率 30% 以下开始衰减
    pub util_decay_threshold: f64,
}

impl Default for ControllerParams {
    fn default() -> Self {
        Self {
            kp: 0.000_3,
            max_step_ratio: 0.15,
            util_decay_threshold: 0.3,
        }
    }
}
