import { requiredCredentialEnvKeys } from "@/features/agents/ui/agentConfigOptions";
import type {
  AcpRuntimeCatalogEntry,
  GlobalAgentConfig,
} from "@/shared/api/types";

export type AgentReadinessResult =
  | { ready: true; reason: "runtime"; runtimeLabel: string }
  | { ready: true; reason: "configured"; runtimeLabel: string }
  | { ready: false };

/**
 * Determine whether the user has a working agent path configured.
 *
 * Rust owns operational readiness. Runtimes whose catalog metadata projects
 * provider/model environment fields additionally need those normalized global
 * values and their provider credentials. No runtime ID is used as a
 * capability or readiness signal.
 */
function runtimeIsConfigured(
  runtime: AcpRuntimeCatalogEntry,
  globalConfig: GlobalAgentConfig,
): boolean {
  if (
    runtime.availability !== "available" ||
    runtime.runtimeReadiness !== "ready"
  ) {
    return false;
  }

  const needsProvider = runtime.providerEnvVar !== null;
  const needsModel = runtime.modelEnvVar !== null;
  const provider = globalConfig.provider?.trim() ?? "";
  const model = globalConfig.model?.trim() ?? "";
  if ((needsProvider && !provider) || (needsModel && !model)) {
    return false;
  }

  const required = needsProvider
    ? requiredCredentialEnvKeys(runtime.id, provider)
    : [];
  return required.every(
    (key) => (globalConfig.env_vars[key] ?? "").trim().length > 0,
  );
}

export function resolveAgentReadiness(
  runtimes: readonly AcpRuntimeCatalogEntry[],
  globalConfig: GlobalAgentConfig,
  scope: "any" | "preferred" = "any",
): AgentReadinessResult {
  const candidates =
    scope === "preferred"
      ? runtimes.filter(
          (runtime) => runtime.id === globalConfig.preferred_runtime,
        )
      : runtimes;

  for (const runtime of candidates) {
    if (!runtimeIsConfigured(runtime, globalConfig)) continue;
    const requiresNormalizedConfig =
      runtime.providerEnvVar !== null || runtime.modelEnvVar !== null;
    return {
      ready: true,
      reason: requiresNormalizedConfig ? "configured" : "runtime",
      runtimeLabel: runtime.label,
    };
  }

  return { ready: false };
}
