import assert from "node:assert/strict";
import test, { mock } from "node:test";

import { relayClient } from "./relayClient.ts";
import {
  buildPermissionDecisionPayload,
  decideManagedAgentPermission,
} from "./agentControl.ts";

const AGENT = "a".repeat(64);
const BINDING = {
  agentPubkey: AGENT,
  relayUrl: "wss://relay.example.test",
  sessionId: "session-1",
  requestId: "75a8098e-0cb2-4b47-bb95-ef9c33e17ae1",
};

test("buildPermissionDecisionPayload builds the exact one-shot approval payload", () => {
  assert.deepEqual(
    buildPermissionDecisionPayload(AGENT, BINDING, "approve_once"),
    {
      type: "permission_decision",
      agentPubkey: AGENT,
      relayUrl: "wss://relay.example.test",
      sessionId: "session-1",
      requestId: "75a8098e-0cb2-4b47-bb95-ef9c33e17ae1",
      decision: "approve_once",
    },
  );
});

test("buildPermissionDecisionPayload builds the exact reject payload", () => {
  const payload = buildPermissionDecisionPayload(AGENT, BINDING, "reject");
  assert.deepEqual(payload, {
    ...BINDING,
    type: "permission_decision",
    decision: "reject",
  });
  const serialized = JSON.stringify(payload);
  for (const forbidden of [
    "optionId",
    "allow_always",
    "command",
    "rawInput",
    "allowlist",
  ]) {
    assert.equal(
      serialized.includes(forbidden),
      false,
      `${forbidden} must not cross the renderer boundary`,
    );
  }
});

test("agent mismatch fails before any relay publish", async () => {
  let preconnectCalls = 0;
  mock.method(relayClient, "preconnect", async () => {
    preconnectCalls += 1;
  });
  await assert.rejects(
    decideManagedAgentPermission("b".repeat(64), BINDING, "approve_once"),
    /does not match/,
  );
  assert.equal(preconnectCalls, 0);
  mock.reset();
});

test("observer transport failures propagate to the presenter", async () => {
  mock.method(relayClient, "preconnect", async () => {
    throw new Error("relay unavailable");
  });
  await assert.rejects(
    decideManagedAgentPermission(AGENT, BINDING, "reject"),
    /relay unavailable/,
  );
  mock.reset();
});
