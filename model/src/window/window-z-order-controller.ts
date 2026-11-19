// SPDX-License-Identifier: GPL-3.0-or-later
import type { GenesisWindow, GenesisWindowId } from "./genesis-window.js";

export interface WindowZOrderController {
  bringToFront(windowId: GenesisWindowId): GenesisWindow;

  normalize(): readonly GenesisWindow[];

  getTopWindow(): GenesisWindow | undefined;
}
