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

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControllerParams {
    /// 比例系数
    #[serde(default = "ControllerParams::default_kp")]
    pub kp: f64,

    /// 积分系数
    #[serde(default = "ControllerParams::default_ki")]
    pub ki: f64,

    /// 微分系数
    #[serde(default = "ControllerParams::default_kd")]
    pub kd: f64,

    /// 单次 control 最大幅度占 max_freq 的比例（0.0 ~ 1.0）
    #[serde(default = "ControllerParams::default_max_step_ratio")]
    pub max_step_ratio: f64,

    /// CPU 利用率低于此值时，正向 control 线性衰减（0.0 ~ 1.0）
    #[serde(default = "ControllerParams::default_util_decay_threshold")]
    pub util_decay_threshold: f64,
}

impl ControllerParams {
    const fn default_kp() -> f64 {
        0.0003
    }

    const fn default_ki() -> f64 {
        0.0
    }

    const fn default_kd() -> f64 {
        0.0
    }

    const fn default_max_step_ratio() -> f64 {
        0.15
    }

    const fn default_util_decay_threshold() -> f64 {
        0.3
    }
}

impl Default for ControllerParams {
    fn default() -> Self {
        Self {
            kp: Self::default_kp(),
            ki: Self::default_ki(),
            kd: Self::default_kd(),
            max_step_ratio: Self::default_max_step_ratio(),
            util_decay_threshold: Self::default_util_decay_threshold(),
        }
    }
}
