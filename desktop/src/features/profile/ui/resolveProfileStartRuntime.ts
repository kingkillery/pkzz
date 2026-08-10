import {
  availableRuntimesForStart,
  resolveStartRuntimeForDefinition,
} from "@/features/agents/lib/instanceInputForDefinition";
import type { AgentPersona, GlobalAgentConfig } from "@/shared/api/types";

type RuntimesQuery = Parameters<typeof availableRuntimesForStart>[0];
type RefetchGlobalConfig = () => Promise<GlobalAgentConfig>;

/** Resolve a profile start against fresh action-time config and runtime data. */
export async function resolveProfileStartRuntime(
  persona: AgentPersona,
  runtimesQuery: RuntimesQuery,
  refetchGlobalConfig: RefetchGlobalConfig,
) {
  const persistedConfig = await refetchGlobalConfig();
  const runtimes = await availableRuntimesForStart(runtimesQuery);
  return resolveStartRuntimeForDefinition(
    persona,
    runtimes,
    persistedConfig.preferred_runtime,
  );
}
