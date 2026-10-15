import { invoke } from "@tauri-apps/api/core";
import type { UsageScript } from "@/types";

export interface PiCurrentState {
  enabledProviderIds: string[];
  defaultProviderId: string | null;
}

export type PiSessionDiscovery =
  | {
      status: "available";
    }
  | {
      status: "requires_project_context";
      configuredPath: string;
    }
  | {
      status: "unavailable";
      reason: string;
    };

export interface PiObservationStatus {
  enabled: boolean;
  canManage: boolean;
  updateAvailable: boolean;
  extensionPath: string;
  lastObservedAt: number | null;
}

export const piApi = {
  async getObservationStatus(): Promise<PiObservationStatus> {
    return await invoke("get_pi_observation_status");
  },

  async setObservationEnabled(enabled: boolean): Promise<PiObservationStatus> {
    return await invoke("set_pi_observation_enabled", { enabled });
  },

  async getCurrentState(): Promise<PiCurrentState> {
    return await invoke("get_pi_current_state");
  },

  async updateProviderUsageScript(
    id: string,
    usageScript: UsageScript,
  ): Promise<boolean> {
    return await invoke("update_pi_provider_usage_script", {
      id,
      usageScript,
    });
  },

  async getSessionDiscovery(): Promise<PiSessionDiscovery> {
    return await invoke("get_pi_session_discovery");
  },
};
