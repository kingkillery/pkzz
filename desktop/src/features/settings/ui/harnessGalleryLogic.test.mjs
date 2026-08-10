import assert from "node:assert/strict";
import { describe, it } from "node:test";

import {
  isEditableEntry,
  countAgentsReferencingHarness,
  deleteHarnessConfirmMessage,
  deleteConfirmState,
} from "./harnessGalleryLogic.ts";

// ── Minimal catalog entry factory ────────────────────────────────────────────

function entry(overrides = {}) {
  return {
    id: overrides.id ?? "test-id",
    label: overrides.label ?? "Test",
    source: overrides.source ?? "custom",
    availability: overrides.availability ?? "not_installed",
    avatarUrl: "",
    command: overrides.command ?? null,
    binaryPath: null,
    defaultArgs: [],
    mcpCommand: null,
    modelEnvVar: null,
    providerEnvVar: null,
    thinkingEnvVar: null,
    installHint: "",
    installInstructionsUrl: "",
    canAutoInstall: false,
    underlyingCliPath: null,
    nodeRequired: false,
    authStatus: { status: "not_applicable" },
    loginHint: null,
  };
}

// ── isEditableEntry ───────────────────────────────────────────────────────────

describe("isEditableEntry", () => {
  it("returns true for custom entries", () => {
    assert.ok(isEditableEntry(entry({ source: "custom" })));
  });

  it("returns false for preset entries", () => {
    assert.ok(!isEditableEntry(entry({ source: "preset" })));
  });

  it("returns false for builtin entries", () => {
    assert.ok(!isEditableEntry(entry({ source: "builtin" })));
  });
});

// ── countAgentsReferencingHarness ─────────────────────────────────────────────

describe("countAgentsReferencingHarness", () => {
  it("counts direct effective runtime identities", () => {
    const agents = [
      {
        runtime: "my-harness",
        personaId: null,
        agentCommandOverride: null,
      },
      { runtime: "other", personaId: null, agentCommandOverride: null },
      {
        runtime: "my-harness",
        personaId: "p1",
        agentCommandOverride: "my-command",
      },
    ];
    assert.equal(countAgentsReferencingHarness("my-harness", agents, []), 2);
  });

  it("fallback agents reference effective and unpinned persona-source identities", () => {
    const agents = [
      { runtime: "ompk", personaId: "p1", agentCommandOverride: null },
    ];
    const personas = [{ id: "p1", runtime: "custom-source" }];

    assert.equal(countAgentsReferencingHarness("ompk", agents, personas), 1);
    assert.equal(
      countAgentsReferencingHarness("custom-source", agents, personas),
      1,
    );
  });

  it("explicit overrides do not reference an unrelated persona source", () => {
    const agents = [
      {
        runtime: "ompk",
        personaId: "p1",
        agentCommandOverride: "ompk",
      },
    ];
    const personas = [{ id: "p1", runtime: "custom-source" }];

    assert.equal(countAgentsReferencingHarness("ompk", agents, personas), 1);
    assert.equal(
      countAgentsReferencingHarness("custom-source", agents, personas),
      0,
    );
  });

  it("runtime-less inherited agents reference their persona source", () => {
    const agents = [
      { runtime: null, personaId: "p1", agentCommandOverride: null },
    ];
    const personas = [{ id: "p1", runtime: "my-harness" }];
    assert.equal(
      countAgentsReferencingHarness("my-harness", agents, personas),
      1,
    );
  });

  it("runtime-less implicit OMPK counts its effective identity exactly once", () => {
    const agents = [
      { runtime: "ompk", personaId: "p1", agentCommandOverride: null },
    ];
    const personas = [{ id: "p1", runtime: null }];
    assert.equal(countAgentsReferencingHarness("ompk", agents, personas), 1);
  });

  it("agents with no selected or source runtime do not count", () => {
    const agents = [
      { runtime: null, personaId: null, agentCommandOverride: null },
    ];
    assert.equal(countAgentsReferencingHarness("my-harness", agents, []), 0);
  });
});

// ── deleteHarnessConfirmMessage ───────────────────────────────────────────────

describe("deleteHarnessConfirmMessage", () => {
  it("plain confirmation when nothing references the harness", () => {
    assert.equal(
      deleteHarnessConfirmMessage("My Harness", 0),
      "Delete My Harness?",
    );
  });

  it("singular copy for one referencing agent", () => {
    assert.equal(
      deleteHarnessConfirmMessage("My Harness", 1),
      "1 agent uses this harness and will stop launching. Delete My Harness?",
    );
  });

  it("plural copy for multiple referencing agents", () => {
    assert.equal(
      deleteHarnessConfirmMessage("My Harness", 3),
      "3 agents use this harness and will stop launching. Delete My Harness?",
    );
  });
});

// ── deleteConfirmState ────────────────────────────────────────────────────────

describe("deleteConfirmState", () => {
  const settled = (data) => ({ isPending: false, isError: false, data });
  const pending = { isPending: true, isError: false, data: undefined };
  const failed = { isPending: false, isError: true, data: undefined };

  it("disables confirm while agents query is still loading", () => {
    const state = deleteConfirmState("h1", "My Harness", pending, settled([]));
    assert.equal(state.canConfirm, false);
    assert.match(state.message, /Checking which agents/);
  });

  it("disables confirm while personas query is still loading", () => {
    const state = deleteConfirmState("h1", "My Harness", settled([]), pending);
    assert.equal(state.canConfirm, false);
    assert.match(state.message, /Checking which agents/);
  });

  it("query failure does not claim zero dependents", () => {
    const state = deleteConfirmState("h1", "My Harness", failed, settled([]));
    assert.equal(state.canConfirm, true);
    assert.match(state.message, /Couldn't check/);
    assert.doesNotMatch(state.message, /^Delete My Harness\?$/);
  });

  it("persona query failure also reports unknown blast radius", () => {
    const state = deleteConfirmState("h1", "My Harness", settled([]), failed);
    assert.equal(state.canConfirm, true);
    assert.match(state.message, /Couldn't check/);
  });

  it("settled queries produce the counted warning and enable confirm", () => {
    const agents = settled([
      { runtime: "h1", personaId: null, agentCommandOverride: null },
      { runtime: null, personaId: "p1", agentCommandOverride: null },
    ]);
    const personas = settled([{ id: "p1", runtime: "h1" }]);
    const state = deleteConfirmState("h1", "My Harness", agents, personas);
    assert.equal(state.canConfirm, true);
    assert.equal(
      state.message,
      "2 agents use this harness and will stop launching. Delete My Harness?",
    );
  });

  it("settled queries with zero dependents use the plain confirmation", () => {
    const state = deleteConfirmState(
      "h1",
      "My Harness",
      settled([]),
      settled([]),
    );
    assert.equal(state.canConfirm, true);
    assert.equal(state.message, "Delete My Harness?");
  });
});
