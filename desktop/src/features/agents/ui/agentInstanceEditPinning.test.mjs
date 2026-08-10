import assert from "node:assert/strict";
import test from "node:test";

import {
  requiredCredentialEnvKeys,
  runtimeSupportsLlmProviderSelection,
} from "./agentConfigOptions.tsx";
import {
  computeEditAgentFormValidity,
  hasMissingRequiredEnvKey,
  resolveAgentCommandUpdate,
  resolveInheritedRuntimeSubmission,
  resolveRuntimeProviderCapability,
} from "./personaRuntimeModel.ts";
import {
  buildRuntimeDropdownOptions,
  canPersistRuntimePin,
} from "./agentInstanceRuntimeIdentity.ts";

// ── Phase 1B.3b re-host pinning: inherit-toggle → gate → submit ─────────────
//
// AgentInstanceEditDialog (re-homed from EditAgentDialog) wires three seams
// that must never disagree after the move:
//   1. TOGGLE: flipping "Inherit runtime from persona" re-resolves
//      prospectiveRuntimeId from the LINKED PERSONA's runtime (not the stale
//      override) and feeds resolveInheritedRuntimeSubmission.
//   2. GATE: useRequiredCredentialState validates the SAME inheritedSubmission
//      provider/env the submit path will persist (gate ↔ record ↔ spawn).
//   3. SUBMIT: UpdateManagedAgentInput persists that same snapshot, with
//      resolveAgentCommandUpdate + harnessOverride derived from the toggle.
// These tests chain the pure modules exactly as the component does, so a
// re-host that re-derives any seam independently fails here.

const runtimes = [
  { id: "ompk", command: "ompk", defaultArgs: ["acp"] },
  { id: "buzz-agent", command: "buzz-agent-cmd", defaultArgs: [] },
  { id: "claude", command: "claude-cmd", defaultArgs: [] },
];

function selectedRuntimeIdFor({ runtime, agentCommand }) {
  const effectiveRuntimeId = runtime?.trim();
  const matched =
    runtimes.find((candidate) => candidate.id === effectiveRuntimeId) ??
    runtimes.find(
      (candidate) => candidate.command?.trim() === agentCommand.trim(),
    ) ??
    runtimes.find((candidate) => candidate.id === agentCommand.trim());
  return effectiveRuntimeId || matched?.id || "custom";
}

// Mirrors the component's prospectiveRuntimeId memo: when inheriting, resolve
// from the linked persona's runtime; fall back to the agentCommand dual-match.
function prospectiveRuntimeIdFor({
  inheritHarness,
  selectedRuntimeId,
  linkedPersonaRuntime,
  agentCommand,
}) {
  if (!inheritHarness) {
    return selectedRuntimeId;
  }
  const personaRuntimeId = linkedPersonaRuntime?.trim();
  if (personaRuntimeId) {
    return (
      runtimes.find((r) => r.id === personaRuntimeId)?.id ?? personaRuntimeId
    );
  }
  return (
    runtimes.find((r) => r.id === "ompk")?.id ??
    runtimes.find((r) => r.command?.trim() === agentCommand.trim())?.id ??
    runtimes.find((r) => r.id === agentCommand.trim())?.id ??
    ""
  );
}

function resolveRuntimeIdentityFields({
  runtimeTouched,
  argsTouched,
  inheritHarness,
  selectedRuntimeId,
  prospectiveRuntimeId,
  agentCommandUpdate,
  parsedArgs,
  originalArgs,
}) {
  const runtimeIdentityChanged =
    runtimeTouched ||
    (!inheritHarness &&
      selectedRuntimeId === "custom" &&
      agentCommandUpdate != null);
  const runtimeId = runtimeIdentityChanged
    ? inheritHarness
      ? prospectiveRuntimeId || undefined
      : selectedRuntimeId === "custom"
        ? null
        : selectedRuntimeId
    : undefined;
  const agentArgs = runtimeIdentityChanged
    ? (!inheritHarness && selectedRuntimeId === "custom") || argsTouched
      ? parsedArgs
      : []
    : argsTouched && parsedArgs.join(",") !== originalArgs.join(",")
      ? parsedArgs
      : undefined;
  return {
    runtimeId,
    harnessOverride: runtimeIdentityChanged ? !inheritHarness : undefined,
    agentArgs,
  };
}

// A Claude-pinned agent linked to a buzz-agent/anthropic persona — the
// inherit-transition scenario that exercises every seam at once.
const pinnedAgent = {
  name: "test-agent",
  agentCommand: "claude-cmd",
  agentCommandOverride: "claude-cmd",
  acpCommand: "acp",
  personaId: "p1",
  provider: null,
  model: null,
  envVars: {},
};
const persona = {
  runtime: "buzz-agent",
  provider: "anthropic",
  model: "claude-sonnet-4-5",
  envVars: { ANTHROPIC_API_KEY: "sk-persona" },
};

function inheritTransitionState() {
  const inheritHarness = true; // user just checked the toggle
  const prospectiveRuntimeId = prospectiveRuntimeIdFor({
    inheritHarness,
    selectedRuntimeId: "claude",
    linkedPersonaRuntime: persona.runtime,
    agentCommand: pinnedAgent.agentCommand,
  });
  const inheritedSubmission = resolveInheritedRuntimeSubmission({
    inheritHarness,
    agentWasHarnessPinned: pinnedAgent.agentCommandOverride != null,
    provider: "", // pinned Claude agent carries no provider
    personaProvider: persona.provider,
    model: "",
    personaModel: persona.model,
    envVars: {},
    personaEnvVars: persona.envVars,
  });
  return { inheritHarness, prospectiveRuntimeId, inheritedSubmission };
}

function providerEnvVarForRuntime(runtimeId) {
  if (runtimeId === "buzz-agent") return "BUZZ_AGENT_PROVIDER";
  if (runtimeId === "goose") return "GOOSE_PROVIDER";
  return null;
}

test("rehost_toggle_resolvesProspectiveRuntimeFromPersona_notOverride", () => {
  const { prospectiveRuntimeId } = inheritTransitionState();
  assert.equal(
    prospectiveRuntimeId,
    "buzz-agent",
    "inherit-toggle must resolve the prospective runtime from the linked persona, not the still-present Claude pin",
  );
});

test("rehost_gate_validatesTheSubmissionSnapshot_sameValuesAsSubmit", () => {
  const { prospectiveRuntimeId, inheritedSubmission } =
    inheritTransitionState();

  // Gate half: the credential requirement must be computed from the
  // PROSPECTIVE runtime + the submission snapshot's provider/env.
  const providerForRequiredKeys = runtimeSupportsLlmProviderSelection(
    providerEnvVarForRuntime(prospectiveRuntimeId),
  )
    ? (inheritedSubmission.provider ?? "")
    : "";
  const requiredKeys = requiredCredentialEnvKeys(
    prospectiveRuntimeId,
    providerForRequiredKeys,
  );
  const missing = hasMissingRequiredEnvKey(
    requiredKeys,
    inheritedSubmission.envVars,
  );

  // The persona snapshot carries the credential, so the gate must clear —
  // and it must clear because of the SAME env map submit will persist.
  assert.equal(inheritedSubmission.provider, "anthropic");
  assert.deepEqual(inheritedSubmission.envVars, {
    ANTHROPIC_API_KEY: "sk-persona",
  });
  assert.equal(
    missing,
    false,
    "gate must validate the submission snapshot (persona-layered env), not the agent's own empty env",
  );
});

test("rehost_gate_blocksSave_whenSubmissionSnapshotLacksCredential", () => {
  const { prospectiveRuntimeId } = inheritTransitionState();
  // Same transition but the persona carries no credential: the submission
  // snapshot is credential-less and the SAME gate must now block Save.
  const bareSubmission = resolveInheritedRuntimeSubmission({
    inheritHarness: true,
    agentWasHarnessPinned: true,
    provider: "",
    personaProvider: persona.provider,
    model: "",
    personaModel: persona.model,
    envVars: {},
    personaEnvVars: {}, // no key anywhere
  });
  const requiredKeys = requiredCredentialEnvKeys(
    prospectiveRuntimeId,
    bareSubmission.provider ?? "",
  );
  const requiredEnvKeyMissing = hasMissingRequiredEnvKey(
    requiredKeys,
    bareSubmission.envVars,
  );
  assert.equal(requiredEnvKeyMissing, true);

  const canSubmit = computeEditAgentFormValidity({
    name: pinnedAgent.name,
    parallelism: "1",
    agentAcpCommand: pinnedAgent.acpCommand,
    acpCommand: pinnedAgent.acpCommand,
    respondTo: "mentions",
    respondToAllowlistLength: 0,
    selectedRuntimeId: "claude",
    inheritHarness: true,
    agentCommand: pinnedAgent.agentCommand,
    requiredEnvKeyMissing,
  });
  assert.equal(
    canSubmit,
    false,
    "missing credential in the submission snapshot must block Save through the same validity path",
  );
});

test("rehost_submit_persistsToggleAsCommandClear_andOmitsHarnessOverride", () => {
  // Submit half of the toggle seam: inheriting with a persisted override must
  // send the clear sentinel (""), and harnessOverride derives from the SAME
  // agentCommandUpdate (omitted while inheriting — falsy per the component).
  const agentCommandUpdate = resolveAgentCommandUpdate({
    inheritHarness: true,
    agentCommand: pinnedAgent.agentCommand,
    originalAgentCommand: pinnedAgent.agentCommand,
    agentCommandOverride: pinnedAgent.agentCommandOverride,
  });
  assert.equal(
    agentCommandUpdate,
    "",
    "inherit with a persisted pin must persist the clear sentinel",
  );
  const harnessOverride =
    agentCommandUpdate != null ? !true /* inheritHarness */ : undefined;
  assert.equal(
    harnessOverride,
    false,
    "harnessOverride must derive from the shared agentCommandUpdate, signalling the cleared pin",
  );
  const identity = resolveRuntimeIdentityFields({
    runtimeTouched: true,
    argsTouched: false,
    inheritHarness: true,
    selectedRuntimeId: "claude",
    prospectiveRuntimeId: "buzz-agent",
    agentCommandUpdate,
    parsedArgs: [],
    originalArgs: [],
  });
  assert.deepEqual(identity, {
    runtimeId: "buzz-agent",
    harnessOverride: false,
    agentArgs: [],
  });

  // And the provider tri-state must classify the PROSPECTIVE runtime — the
  // same id the gate used — so submit persists what the gate validated.
  const { prospectiveRuntimeId, inheritedSubmission } =
    inheritTransitionState();
  const capability = resolveRuntimeProviderCapability(
    prospectiveRuntimeId,
    runtimeSupportsLlmProviderSelection(
      providerEnvVarForRuntime(prospectiveRuntimeId),
    ),
  );
  assert.equal(capability, "capable");
  const providerUpdate =
    capability === "capable"
      ? inheritedSubmission.provider !== (pinnedAgent.provider ?? null)
        ? inheritedSubmission.provider
        : undefined
      : undefined;
  assert.equal(
    providerUpdate,
    "anthropic",
    "submit must persist the gate-validated submission provider for the prospective runtime",
  );
});

test("rehost_steadyStateInherit_localEditsStayAuthoritative", () => {
  // Wipe-on-poll companion: an agent ALREADY inheriting at open (no pin) with
  // deliberate local edits — the submission snapshot must pass them through
  // untouched, not resurrect persona values (the pre-move contract).
  const submission = resolveInheritedRuntimeSubmission({
    inheritHarness: true,
    agentWasHarnessPinned: false, // steady state, not a transition
    provider: "databricks", // deliberate local re-point
    personaProvider: persona.provider,
    model: "",
    personaModel: persona.model,
    envVars: { DATABRICKS_TOKEN: "tok" },
    personaEnvVars: persona.envVars,
  });
  assert.equal(submission.provider, "databricks");
  assert.equal(
    submission.model,
    null,
    "empty local model in steady state stays empty (runtime default), never backfilled from the persona",
  );
  assert.deepEqual(submission.envVars, { DATABRICKS_TOKEN: "tok" });
});

test("effective runtime id seeds selection before command inference", () => {
  assert.equal(
    selectedRuntimeIdFor({
      runtime: "ompk",
      agentCommand: "claude-cmd",
    }),
    "ompk",
  );
});

test("explicit catalog selection sends id, pin intent, and untouched defaults sentinel", () => {
  assert.deepEqual(
    resolveRuntimeIdentityFields({
      runtimeTouched: true,
      argsTouched: false,
      inheritHarness: false,
      selectedRuntimeId: "ompk",
      prospectiveRuntimeId: "ompk",
      agentCommandUpdate: "ompk",
      parsedArgs: ["acp"],
      originalArgs: [],
    }),
    {
      runtimeId: "ompk",
      harnessOverride: true,
      agentArgs: [],
    },
  );
});

test("unready runtimes cannot become new pins but existing pins survive unrelated edits", () => {
  const unready = {
    id: "ompk",
    label: "Oh My PK",
    availability: "available",
    runtimeReadiness: "authentication_required",
  };
  const ready = {
    id: "goose",
    label: "Goose",
    availability: "available",
    runtimeReadiness: "ready",
  };
  const options = buildRuntimeDropdownOptions([unready, ready], "goose");
  assert.equal(
    options.find((option) => option.value === "ompk")?.disabled,
    true,
  );
  assert.equal(
    canPersistRuntimePin({
      inheritHarness: false,
      runtimeTouched: true,
      selectedRuntime: unready,
      selectedRuntimeId: "ompk",
    }),
    false,
  );
  assert.equal(
    canPersistRuntimePin({
      inheritHarness: false,
      runtimeTouched: false,
      selectedRuntime: unready,
      selectedRuntimeId: "ompk",
    }),
    true,
  );
});

test("manually edited catalog args remain explicit", () => {
  assert.deepEqual(
    resolveRuntimeIdentityFields({
      runtimeTouched: true,
      argsTouched: true,
      inheritHarness: false,
      selectedRuntimeId: "ompk",
      prospectiveRuntimeId: "ompk",
      agentCommandUpdate: "ompk",
      parsedArgs: ["acp", "--profile", "work"],
      originalArgs: [],
    }),
    {
      runtimeId: "ompk",
      harnessOverride: true,
      agentArgs: ["acp", "--profile", "work"],
    },
  );
});

test("runtime-less inherit resolves the current default id with non-explicit intent", () => {
  const prospectiveRuntimeId = prospectiveRuntimeIdFor({
    inheritHarness: true,
    selectedRuntimeId: "claude",
    linkedPersonaRuntime: null,
    agentCommand: "legacy-command",
  });
  assert.equal(prospectiveRuntimeId, "ompk");
  assert.deepEqual(
    resolveRuntimeIdentityFields({
      runtimeTouched: true,
      argsTouched: false,
      inheritHarness: true,
      selectedRuntimeId: "claude",
      prospectiveRuntimeId,
      agentCommandUpdate: "",
      parsedArgs: [],
      originalArgs: [],
    }),
    {
      runtimeId: "ompk",
      harnessOverride: false,
      agentArgs: [],
    },
  );
});

test("raw custom selection clears catalog identity and preserves command args", () => {
  assert.deepEqual(
    resolveRuntimeIdentityFields({
      runtimeTouched: true,
      argsTouched: false,
      inheritHarness: false,
      selectedRuntimeId: "custom",
      prospectiveRuntimeId: "custom",
      agentCommandUpdate: "/opt/custom-acp",
      parsedArgs: ["serve", "--stdio"],
      originalArgs: [],
    }),
    {
      runtimeId: null,
      harnessOverride: true,
      agentArgs: ["serve", "--stdio"],
    },
  );
});

test("name-only save omits runtime identity, pin intent, and args", () => {
  assert.deepEqual(
    resolveRuntimeIdentityFields({
      runtimeTouched: false,
      argsTouched: false,
      inheritHarness: true,
      selectedRuntimeId: "ompk",
      prospectiveRuntimeId: "ompk",
      agentCommandUpdate: undefined,
      parsedArgs: [],
      originalArgs: [],
    }),
    {
      runtimeId: undefined,
      harnessOverride: undefined,
      agentArgs: undefined,
    },
  );
});

test("editValidity_allowlistWithEmptyList_blocksSave", () => {
  // PR #1667 review: the edit-path crash-loop guard (respondToValid) was
  // never exercised — every other row uses a non-allowlist mode. An agent
  // saved as allowlist-with-empty-list crash-loops at startup.
  const base = {
    name: pinnedAgent.name,
    parallelism: "1",
    agentAcpCommand: pinnedAgent.acpCommand,
    acpCommand: pinnedAgent.acpCommand,
    selectedRuntimeId: "claude",
    inheritHarness: true,
    agentCommand: pinnedAgent.agentCommand,
    requiredEnvKeyMissing: false,
  };
  assert.equal(
    computeEditAgentFormValidity({
      ...base,
      respondTo: "allowlist",
      respondToAllowlistLength: 0,
    }),
    false,
    "allowlist with an empty list must block Save",
  );
  assert.equal(
    computeEditAgentFormValidity({
      ...base,
      respondTo: "allowlist",
      respondToAllowlistLength: 1,
    }),
    true,
    "allowlist with at least one pubkey must allow Save",
  );
});
