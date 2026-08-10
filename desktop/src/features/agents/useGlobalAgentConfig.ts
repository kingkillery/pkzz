/**
 * React hook: load the global agent configuration defaults.
 *
 * Backed by TanStack Query with a stable query key so the config is fetched
 * once per QueryClient lifetime and shared across all callers — dialogs always
 * receive the already-populated value on first render, eliminating the
 * per-mount IPC race that caused required-env-key rows to be missing on open.
 *
 * Callers that persist a runtime choice can distinguish the placeholder from a
 * completed read through `isReady`, or force an action-time read with
 * `refetchGlobalConfig`.
 */
import { useCallback } from "react";
import { useQuery } from "@tanstack/react-query";

import { getGlobalAgentConfig } from "@/shared/api/tauriGlobalAgentConfig";
import type { GlobalAgentConfig } from "@/shared/api/types";

const EMPTY_CONFIG: GlobalAgentConfig = {
  env_vars: {},
  provider: null,
  model: null,
  preferred_runtime: null,
};

export const globalAgentConfigQueryKey = ["globalAgentConfig"] as const;

export function useGlobalAgentConfig(): {
  globalConfig: GlobalAgentConfig;
  isLoading: boolean;
  isReady: boolean;
  refetchGlobalConfig: () => Promise<GlobalAgentConfig>;
} {
  const { data, isPending, isPlaceholderData, refetch } = useQuery({
    queryKey: globalAgentConfigQueryKey,
    queryFn: getGlobalAgentConfig,
    // Config is only mutated via setGlobalAgentConfig — treat as stable until
    // explicitly invalidated by AgentDefaultsSettingsCard after a save.
    staleTime: Number.POSITIVE_INFINITY,
    // Never show a stale empty flash while a background refetch runs.
    placeholderData: EMPTY_CONFIG,
  });

  const refetchGlobalConfig = useCallback(async () => {
    const result = await refetch({
      cancelRefetch: false,
      throwOnError: true,
    });
    if (!result.data || result.isPlaceholderData) {
      throw (
        result.error ??
        new Error("Global agent configuration has not finished loading.")
      );
    }
    return result.data;
  }, [refetch]);

  return {
    globalConfig: data ?? EMPTY_CONFIG,
    isLoading: isPending || isPlaceholderData,
    isReady: data !== undefined && !isPlaceholderData,
    refetchGlobalConfig,
  };
}
