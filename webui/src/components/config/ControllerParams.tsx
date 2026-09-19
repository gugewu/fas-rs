"use client";

import type { ControllerParams } from "@/types/config";
import {
  Card,
  CardContent,
  CardDescription,
  CardHeader,
  CardTitle,
} from "@/components/ui/card";
import { Slider } from "@/components/ui/slider";
import { useTranslation } from "react-i18next";

interface ControllerParamsProps {
  controllerParams: ControllerParams;
  updateControllerParam: <K extends keyof ControllerParams>(
    key: K,
    value: ControllerParams[K],
  ) => void;
}

interface Field {
  key: keyof ControllerParams;
  min: number;
  max: number;
  step: number;
  format: (v: number) => string;
}

const demandFields: Field[] = [
  { key: "demand_low", min: 0, max: 1, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "demand_high", min: 0, max: 1, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "demand_step_base", min: 0, max: 0.5, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "demand_step_scale", min: 0, max: 0.5, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "demand_up_max", min: 0, max: 0.2, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "mode_residency_ms", min: 0, max: 2000, step: 50, format: (v) => `${v} ms` },
  { key: "fps_ok_margin", min: 0, max: 10, step: 1, format: (v) => v.toFixed(0) },
  { key: "fps_ok_recover_margin", min: 0, max: 10, step: 1, format: (v) => v.toFixed(0) },
];

const pidFields: Field[] = [
  { key: "kp", min: 0, max: 0.001, step: 0.00001, format: (v) => v.toFixed(5) },
  { key: "ki", min: 0, max: 0.001, step: 0.00001, format: (v) => v.toFixed(5) },
  { key: "kd", min: 0, max: 0.001, step: 0.00001, format: (v) => v.toFixed(5) },
  { key: "max_step_ratio", min: 0, max: 1, step: 0.01, format: (v) => v.toFixed(2) },
  { key: "util_decay_threshold", min: 0, max: 1, step: 0.01, format: (v) => v.toFixed(2) },
];

export function ControllerParams({
  controllerParams,
  updateControllerParam,
}: ControllerParamsProps) {
  const { t } = useTranslation();

  const renderFields = (fields: Field[]) =>
    fields.map(({ key, min, max, step, format }) => (
      <div key={key} className="space-y-3">
        <div className="flex items-center justify-between">
          <span className="text-sm font-medium">{t(`common:${key}`)}</span>
          <span className="text-sm text-muted-foreground">
            {format(controllerParams[key] as number)}
          </span>
        </div>
        <Slider
          value={[controllerParams[key] as number]}
          min={min}
          max={max}
          step={step}
          onValueChange={(value) =>
            updateControllerParam(key, value[0] as never)
          }
        />
      </div>
    ));

  return (
    <div className="space-y-4">
      <Card>
        <CardHeader>
          <CardTitle>{t("common:demand_loop")}</CardTitle>
          <CardDescription>{t("common:demand_loop_desc")}</CardDescription>
        </CardHeader>
        <CardContent className="space-y-6">{renderFields(demandFields)}</CardContent>
      </Card>

      <Card>
        <CardHeader>
          <CardTitle>{t("common:pid_tuning")}</CardTitle>
          <CardDescription>{t("common:pid_tuning_desc")}</CardDescription>
        </CardHeader>
        <CardContent className="space-y-6">{renderFields(pidFields)}</CardContent>
      </Card>
    </div>
  );
}
