import assert from "node:assert/strict";
import test from "node:test";

import { resolveAgentReadiness } from "./agentReadiness.ts";

function makeRuntime(overrides = {}) {
  return {
    id: "generic-cli",
    label: "Generic CLI",
    availability: "available",
    authStatus: { status: "unknown" },
    runtimeReadiness: "ready",
    canConnectAccount: false,
    avatarUrl: "",
    command: "generic-cli",
    binaryPath: "/usr/local/bin/generic-cli",
    defaultArgs: [],
    mcpCommand: null,
    modelEnvVar: null,
    providerEnvVar: null,
    thinkingEnvVar: null,
    maxTokensEnvVar: null,
    contextLimitEnvVar: null,
    maxRoundsEnvVar: null,
    installHint: "",
    installInstructionsUrl: "https://example.com",
    canAutoInstall: false,
    requiresExternalCli: false,
    underlyingCliPath: null,
    nodeRequired: false,
    loginHint: null,
    source: "preset",
    ...overrides,
  };
}

function configuredRuntime(overrides = {}) {
  return makeRuntime({
    id: "buzz-agent",
    label: "Pkzz Agent",
    providerEnvVar: "BUZZ_LLM_PROVIDER",
    modelEnvVar: "BUZZ_LLM_MODEL",
    ...overrides,
  });
}

function makeConfig(overrides = {}) {
  return {
    env_vars: {},
    provider: null,
    model: null,
    preferred_runtime: "generic-cli",
    ...overrides,
  };
}

test("auth unknown plus Rust-ready is ready", () => {
  const result = resolveAgentReadiness(
    [
      makeRuntime({
        id: "ompk",
        label: "Oh My PK",
        authStatus: { status: "unknown" },
        runtimeReadiness: "ready",
      }),
    ],
    makeConfig({ preferred_runtime: "ompk" }),
    "preferred",
  );
  assert.deepEqual(result, {
    ready: true,
    reason: "runtime",
    runtimeLabel: "Oh My PK",
  });
});

test("model-unavailable and unknown runtime readiness fail closed", () => {
  for (const runtimeReadiness of ["model_unavailable", "unknown"]) {
    const result = resolveAgentReadiness(
      [
        makeRuntime({
          authStatus: { status: "logged_in" },
          runtimeReadiness,
        }),
      ],
      makeConfig(),
      "preferred",
    );
    assert.deepEqual(result, { ready: false });
  }
});

test("authentication-required readiness fails even with stale logged-in auth", () => {
  const result = resolveAgentReadiness(
    [
      makeRuntime({
        authStatus: { status: "logged_in" },
        runtimeReadiness: "authentication_required",
      }),
    ],
    makeConfig(),
    "preferred",
  );
  assert.deepEqual(result, { ready: false });
});

test("unavailable runtimes never become ready", () => {
  const result = resolveAgentReadiness(
    [makeRuntime({ availability: "not_installed" })],
    makeConfig(),
    "preferred",
  );
  assert.deepEqual(result, { ready: false });
});

test("preferred scope evaluates only the saved runtime", () => {
  const result = resolveAgentReadiness(
    [
      makeRuntime({ id: "other", label: "Other" }),
      makeRuntime({
        id: "preferred",
        runtimeReadiness: "model_unavailable",
      }),
    ],
    makeConfig({ preferred_runtime: "preferred" }),
    "preferred",
  );
  assert.deepEqual(result, { ready: false });
});

test("any scope accepts any operationally ready runtime", () => {
  const result = resolveAgentReadiness(
    [
      makeRuntime({ id: "first", runtimeReadiness: "unknown" }),
      makeRuntime({ id: "second", label: "Second" }),
    ],
    makeConfig({ preferred_runtime: null }),
  );
  assert.deepEqual(result, {
    ready: true,
    reason: "runtime",
    runtimeLabel: "Second",
  });
});

test("catalog provider/model metadata requires normalized global config", () => {
  const runtime = configuredRuntime();
  assert.deepEqual(
    resolveAgentReadiness(
      [runtime],
      makeConfig({ preferred_runtime: runtime.id }),
      "preferred",
    ),
    { ready: false },
  );
  assert.deepEqual(
    resolveAgentReadiness(
      [runtime],
      makeConfig({
        model: "claude-3-5-sonnet-latest",
        preferred_runtime: runtime.id,
      }),
      "preferred",
    ),
    { ready: false },
  );
});

test("configured runtime requires provider credentials", () => {
  const runtime = configuredRuntime();
  const missingKey = resolveAgentReadiness(
    [runtime],
    makeConfig({
      model: "claude-3-5-sonnet-latest",
      preferred_runtime: runtime.id,
      provider: "anthropic",
    }),
    "preferred",
  );
  assert.deepEqual(missingKey, { ready: false });

  const ready = resolveAgentReadiness(
    [runtime],
    makeConfig({
      env_vars: { ANTHROPIC_API_KEY: "sk-ant-test" },
      model: "claude-3-5-sonnet-latest",
      preferred_runtime: runtime.id,
      provider: "anthropic",
    }),
    "preferred",
  );
  assert.deepEqual(ready, {
    ready: true,
    reason: "configured",
    runtimeLabel: "Pkzz Agent",
  });
});

test("provider capability is derived from catalog metadata, not runtime ID", () => {
  const runtime = configuredRuntime({
    id: "custom-provider-runtime",
    label: "Custom Provider Runtime",
  });
  const result = resolveAgentReadiness(
    [runtime],
    makeConfig({
      model: "custom-model",
      preferred_runtime: runtime.id,
      provider: "keyless-provider",
    }),
    "preferred",
  );
  assert.deepEqual(result, {
    ready: true,
    reason: "configured",
    runtimeLabel: "Custom Provider Runtime",
  });
});

test("empty catalog has no ready path", () => {
  assert.deepEqual(resolveAgentReadiness([], makeConfig()), { ready: false });
});
