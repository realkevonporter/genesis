// SPDX-License-Identifier: GPL-3.0-or-later
import type { ApplicationSessionId } from "../application-session-id.js";

export interface ApplicationLifecycleController {
  foregroundApplication(sessionId: ApplicationSessionId): void;

  backgroundApplication(sessionId: ApplicationSessionId): void;
}
