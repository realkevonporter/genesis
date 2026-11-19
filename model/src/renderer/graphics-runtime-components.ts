// SPDX-License-Identifier: GPL-3.0-or-later
export interface GraphicsRendererLifecycle {
  initialize(): void;

  shutdown(): void;
}

export interface GraphicsRenderLoopLifecycle {
  start(): void;

  stop(): void;
}

export interface GraphicsSchedulerLifecycle {
  start(): void;

  stop(): void;
}
