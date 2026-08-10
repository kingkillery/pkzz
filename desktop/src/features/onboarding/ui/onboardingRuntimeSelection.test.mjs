import assert from "node:assert/strict";
import test from "node:test";

import {
  getReadyOnboardingRuntimes,
  getVisibleOnboardingRuntimes,
  runtimeIsReadyForOnboarding,
  runtimeIsVisibleInOnboarding,
} from "./onboardingRuntimeSelection.ts";

function runtime(id, availability, runtimeReadiness, authStatus = "unknown") {
  return {
    id,
    availability,
    authStatus: { status: authStatus },
    runtimeReadiness,
  };
}

test("all featured harnesses are visible in onboarding", () => {
  assert.equal(runtimeIsVisibleInOnboarding("ompk"), true);
  assert.equal(runtimeIsVisibleInOnboarding("claude"), true);
  assert.equal(runtimeIsVisibleInOnboarding("codex"), true);
  assert.equal(runtimeIsVisibleInOnboarding("goose"), true);
  assert.equal(runtimeIsVisibleInOnboarding("buzz-agent"), true);
  assert.equal(runtimeIsVisibleInOnboarding("custom"), false);
});

test("visible onboarding runtimes use the product order", () => {
  const runtimes = [
    runtime("ompk", "available", "ready"),
    runtime("buzz-agent", "available", "ready"),
    runtime("codex", "available", "ready"),
    runtime("goose", "available", "ready"),
    runtime("claude", "available", "ready"),
  ];

  assert.deepEqual(
    getVisibleOnboardingRuntimes(runtimes).map(({ id }) => id),
    ["ompk", "claude", "codex", "goose", "buzz-agent"],
  );
});

test("readiness requires availability and Rust operational readiness", () => {
  assert.equal(
    runtimeIsReadyForOnboarding(
      runtime("ompk", "available", "ready", "unknown"),
    ),
    true,
  );
  assert.equal(
    runtimeIsReadyForOnboarding(
      runtime("ompk", "available", "model_unavailable", "unknown"),
    ),
    false,
  );
  assert.equal(
    runtimeIsReadyForOnboarding(
      runtime("claude", "available", "authentication_required", "logged_out"),
    ),
    false,
  );
  assert.equal(
    runtimeIsReadyForOnboarding(
      runtime("codex", "not_installed", "ready", "logged_in"),
    ),
    false,
  );
});

test("ready onboarding runtimes exclude unknown and non-ready harnesses", () => {
  const runtimes = [
    runtime("ompk", "available", "model_unavailable"),
    runtime("goose", "available", "ready"),
    runtime("codex", "available", "authentication_required", "logged_out"),
    runtime("buzz-agent", "available", "ready"),
    runtime("claude", "available", "ready", "unknown"),
    runtime("custom", "available", "ready"),
  ];

  assert.deepEqual(
    getReadyOnboardingRuntimes(runtimes).map(({ id }) => id),
    ["claude", "goose", "buzz-agent"],
  );
});
