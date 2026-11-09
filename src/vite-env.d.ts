/// <reference types="vite/client" />

export interface PrinterConfig {
  id: string;
  nickname: string;
  ip: string;
  accessCode: string;
  serial: string;
  deviceName?: string;
}

export interface AmsTray {
  id: number;
  tray_type: string;
  color: string;
  name: string;
  /** Default print temperature for this filament in °C */
  tray_temp: number;
}

export interface AmsUnit {
  id: number;
  humidity: number;
  trays: AmsTray[];
}

/** How the printer serves its camera; `unknown` until the first status report. */
export type Camera =
  | { kind: 'unknown' }
  | { kind: 'jpeg' }
  | { kind: 'rtsp'; url: string };

export interface Temperature {
  current: number;
  target: number;
}

export interface Nozzle {
  side: 'left' | 'right';
  current: number;
  target: number;
  active: boolean;
}

export interface ExternalSpool {
  /** Global slot ID, as in tray_now and load_filament. */
  slot: number;
  /** Set on dual-nozzle printers, which have a holder per side. */
  side: 'left' | 'right' | null;
  /** Whether load_filament supports this spool. */
  loadable: boolean;
  /** Feeding a nozzle. */
  loaded: boolean;
  tray: AmsTray;
}

export interface PrinterStatus {
  /** The active nozzle. */
  nozzle_temp: number;
  nozzle_target: number;
  /** Both nozzles of a dual-nozzle printer; empty on single-nozzle printers. */
  nozzles: Nozzle[];
  bed_temp: number;
  bed_target: number;
  /** Only for printers with a chamber sensor. */
  chamber: Temperature | null;
  progress: number;
  remaining_mins: number;
  layer_num: number;
  total_layer_num: number;
  stage: string;
  gcode_state: string;
  ams: AmsUnit[];
  external_spools: ExternalSpool[];
  chamber_light: boolean;
  spd_lvl: number;
  subtask_name: string;
  task_id: string;
  hms: string[];
  device_name: string;
  /** Global slot ID of the tray currently loaded in the nozzle. 255 = none, 254 = external. */
  tray_now: number;
  camera: Camera;
}
