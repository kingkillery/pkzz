import assert from "node:assert/strict";
import test from "node:test";

import { sameGlobalAgentConfig } from "./DefaultConfigStep.tsx";

const base = {
  preferred_runtime: "buzz-agent",
  provider: "anthropic",
  model: "claude-sonnet-4-5",
  env_vars: { ANTHROPIC_API_KEY: "sk-1" },
};

test("sameGlobalAgentConfig: identical config is a no-op", () => {
  assert.equal(sameGlobalAgentConfig(base, { ...base }), true);
  assert.equal(
    sameGlobalAgentConfig(base, {
      ...base,
      env_vars: { ...base.env_vars },
    }),
    true,
  );
});

test("sameGlobalAgentConfig: any real change is detected", () => {
  assert.equal(sameGlobalAgentConfig(base, { ...base, model: "gpt-5" }), false);
  assert.equal(
    sameGlobalAgentConfig(base, {
      ...base,
      env_vars: { ANTHROPIC_API_KEY: "sk-2" },
    }),
    false,
  );
  assert.equal(sameGlobalAgentConfig(base, { ...base, env_vars: {} }), false);
});
