import type { AcpRuntime, AcpRuntimeCatalogEntry } from "@/shared/api/types";

const DEFAULT_PERSONA_RUNTIME_ORDER: readonly string[] = [
  "ompk",
  "buzz-agent",
  "goose",
];

/** Rank a runtime for implicit selection and picker ordering. */
export function getPersonaRuntimePreferenceRank(runtimeId: string) {
  const rank = DEFAULT_PERSONA_RUNTIME_ORDER.indexOf(runtimeId);
  return rank === -1 ? DEFAULT_PERSONA_RUNTIME_ORDER.length : rank;
}

/**
 * Select the best default runtime from a catalog using the same preference
 * order as the UI picker: OMPK first, then the bundled Pkzz Agent, then Goose,
 * then the first ready entry, then null when no implicit candidate is ready.
 *
 * An explicit available global preference always wins, regardless of
 * readiness, so a saved choice that needs setup remains visible and
 * actionable. Only implicit candidates are readiness-gated, and upstream OMP
 * is explicit-only.
 *
 * Generic so that passing AcpRuntime[] (the already-filtered start-path
 * list) returns AcpRuntime | null while passing AcpRuntimeCatalogEntry[]
 * (the full catalog) returns AcpRuntimeCatalogEntry | null. Both call sites
 * share one preference-order implementation.
 */
export function getDefaultPersonaRuntime<T extends AcpRuntimeCatalogEntry>(
  runtimes: readonly T[],
  preferredRuntimeId?: string | null,
): T | null {
  const available = runtimes.filter(
    (runtime) => runtime.availability === "available",
  );
  const configured = available.find(
    (runtime) => runtime.id === preferredRuntimeId,
  );
  if (configured) return configured;

  const readyImplicitCandidates = available.filter(
    (runtime) => runtime.id !== "omp" && runtime.runtimeReadiness === "ready",
  );
  for (const runtimeId of DEFAULT_PERSONA_RUNTIME_ORDER) {
    const runtime = readyImplicitCandidates.find(
      (candidate) => candidate.id === runtimeId,
    );
    if (runtime) return runtime;
  }

  return readyImplicitCandidates[0] ?? null;
}

/**
 * Why a runtime was selected. This provenance is also the launch-intent
 * boundary: only `forced_override` represents a deliberate per-instance pin;
 * persona inheritance, implicit defaults, and unavailable-runtime fallbacks
 * remain unpinned.
 */
export type PersonaRuntimeResolutionProvenance =
  | "implicit_default"
  | "persona"
  | "fallback"
  | "forced_override";

/**
 * Result of resolving a persona's preferred runtime against the set of
 * currently-available ACP runtimes.
 *
 * `runtime` is the runtime that should be used for deployment.
 * `warnings` contains user-visible messages when the resolved runtime
 * differs from what the persona requested (e.g. the configured runtime
 * was uninstalled) or when no runtime is available at all.
 * `isOverridden` is retained for callers that only need mismatch state;
 * `provenance` preserves the actual selection reason and pin intent.
 */
export type ResolvePersonaRuntimeResult = {
  runtime: AcpRuntime | null;
  warnings: string[];
  isOverridden: boolean;
  provenance: PersonaRuntimeResolutionProvenance;
};

/**
 * Resolve which ACP runtime to use when deploying an agent from a persona.
 *
 * Resolution order:
 * 1. If the persona has no `runtimeId` → use `defaultRuntime`, no warnings.
 * 2. If the persona's `runtimeId` matches an available runtime → use it,
 *    unless `forceOverride` is true and `defaultRuntime` is set, in which case
 *    `defaultRuntime` is used instead (with an info warning if they differ).
 * 3. If the persona's `runtimeId` is set but not found in `runtimes` →
 *    fall back to `defaultRuntime` and emit a warning.
 * 4. If there is no `defaultRuntime` either → return `null` with an error
 *    warning so the UI can block deployment.
 */
export function resolvePersonaRuntime(
  personaRuntimeId: string | undefined | null,
  runtimes: readonly AcpRuntime[],
  defaultRuntime: AcpRuntime | null,
  forceOverride?: boolean,
): ResolvePersonaRuntimeResult {
  // Case 1: Persona has no runtime preference — use the default.
  if (!personaRuntimeId) {
    return {
      runtime: defaultRuntime,
      warnings: defaultRuntime
        ? []
        : [
            "No agent runtimes are available. Install a runtime (e.g. Goose) to deploy agents.",
          ],
      isOverridden: false,
      provenance: "implicit_default",
    };
  }

  // Case 2: Persona's preferred runtime is available.
  const matched = runtimes.find((p) => p.id === personaRuntimeId);
  if (matched) {
    if (forceOverride && defaultRuntime && matched.id !== defaultRuntime.id) {
      return {
        runtime: defaultRuntime,
        warnings: [
          `Runtime override: using ${defaultRuntime.label} instead of ${matched.label}.`,
        ],
        isOverridden: true,
        provenance: "forced_override",
      };
    }
    return {
      runtime: forceOverride && defaultRuntime ? defaultRuntime : matched,
      warnings: [],
      isOverridden: false,
      provenance: "persona",
    };
  }

  // Case 3 & 4: Persona's runtime is not available — fall back.
  if (defaultRuntime) {
    return {
      runtime: defaultRuntime,
      warnings: [
        `This agent is configured for runtime "${personaRuntimeId}" but it is not available. Using ${defaultRuntime.label} instead.`,
      ],
      isOverridden: true,
      provenance: "fallback",
    };
  }

  return {
    runtime: null,
    warnings: [
      `This agent is configured for runtime "${personaRuntimeId}" but it is not available, and no other runtimes were found.`,
    ],
    isOverridden: false,
    provenance: "fallback",
  };
}

/**
 * Collect runtime-resolution warnings for a list of personas.
 *
 * Used by deploy dialogs to surface inline alerts when one or more
 * personas reference a runtime that isn't currently available.
 */
export function collectRuntimeWarnings(
  personas: readonly { runtime: string | null }[],
  runtimes: readonly AcpRuntime[],
  fallbackRuntime: AcpRuntime | null,
  forceOverride?: boolean,
): string[] {
  // When no fallback runtime exists, the caller's UI is responsible for
  // showing the global "no runtimes found" state. Per-persona warnings
  // would be redundant noise alongside that.
  if (!fallbackRuntime) return [];
  const warnings: string[] = [];
  for (const persona of personas) {
    const { warnings: w } = resolvePersonaRuntime(
      persona.runtime,
      runtimes,
      fallbackRuntime,
      forceOverride,
    );
    warnings.push(...w);
  }
  return warnings;
}
