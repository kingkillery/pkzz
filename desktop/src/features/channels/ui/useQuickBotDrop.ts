import * as React from "react";

import {
  useAvailableAcpRuntimes,
  useCreateChannelManagedAgentMutation,
} from "@/features/agents/hooks";
import {
  getDefaultPersonaRuntime,
  resolvePersonaRuntime,
} from "@/features/agents/lib/resolvePersonaRuntime";
import { availableRuntimesForStart } from "@/features/agents/lib/instanceInputForDefinition";
import type { AgentPersona } from "@/shared/api/types";
import { getGlobalAgentConfig } from "@/shared/api/tauriGlobalAgentConfig";

type QuickBotDropState = {
  pending: boolean;
  error: string | null;
};

/**
 * Handles creating a new managed agent from a persona with a given instance name.
 */
export function useQuickBotDrop(channelId: string | null) {
  const createMutation = useCreateChannelManagedAgentMutation(channelId);
  const providersQuery = useAvailableAcpRuntimes();
  const [state, setState] = React.useState<QuickBotDropState>({
    pending: false,
    error: null,
  });

  const addBot = React.useCallback(
    async (persona: AgentPersona, instanceName: string) => {
      if (state.pending || !channelId) return;

      setState({ pending: true, error: null });

      try {
        const [providers, persistedConfig] = await Promise.all([
          availableRuntimesForStart(providersQuery),
          getGlobalAgentConfig(),
        ]);
        const defaultProvider = getDefaultPersonaRuntime(
          providers,
          persistedConfig.preferred_runtime,
        );
        const { runtime } = resolvePersonaRuntime(
          persona.runtime,
          providers,
          defaultProvider,
        );

        if (!runtime) {
          setState({
            pending: false,
            error: "No agent runtime available.",
          });
          return;
        }

        await createMutation.mutateAsync({
          runtime,
          name: instanceName,
          systemPrompt: persona.systemPrompt,
          avatarUrl: persona.avatarUrl ?? undefined,
          personaId: persona.id,
          harnessOverride: false,
          model: persona.model ?? undefined,
        });

        setState({ pending: false, error: null });
      } catch (err) {
        setState({
          pending: false,
          error: err instanceof Error ? err.message : "Failed to create agent.",
        });
      }
    },
    [channelId, createMutation, providersQuery, state.pending],
  );

  return { ...state, addBot };
}
