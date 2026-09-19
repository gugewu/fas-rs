export type ConfigOptions = {
  keep_std: boolean;
  scene_game_list: boolean;
  language: "en" | "zh";
};

export type PowerSettings = {
  margin_fps: number;
  core_temp_thresh: number | "disabled";
};

export type ControllerParams = {
  kp: number;
  ki: number;
  kd: number;
  max_step_ratio: number;
  util_decay_threshold: number;
  demand_low: number;
  demand_high: number;
  demand_step_base: number;
  demand_step_scale: number;
  demand_up_max: number;
  mode_residency_ms: number;
  fps_ok_margin: number;
  fps_ok_recover_margin: number;
};

export type UpdatePowerModeFn = (
  mode: keyof PowerModes,
  setting: keyof PowerSettings,
  value: number | number[] | "disabled",
) => void;

export type PowerModes = {
  powersave: PowerSettings;
  balance: PowerSettings;
  performance: PowerSettings;
  fast: PowerSettings;
};

export type FpsValue = number | number[];

export type GameList = {
  [packageName: string]: FpsValue;
};
