import assert from "node:assert/strict";
import test from "node:test";

import {
  _testDispatchControlResult,
  isControlResultFrame,
  resetAgentObserverStore,
  subscribeControlResults,
} from "./observerRelayStore.ts";

const AGENT = "a".repeat(64);
const OTHER_AGENT = "b".repeat(64);
const PERMISSION_RESULT = {
  type: "control_result",
  command: "permission_decision",
  agentPubkey: AGENT,
  relayUrl: "wss://relay.example.test",
  sessionId: "session-1",
  requestId: "75a8098e-0cb2-4b47-bb95-ef9c33e17ae1",
  status: "delivered",
};

test("permission control results require the complete discriminator and binding", () => {
  assert.equal(isControlResultFrame(PERMISSION_RESULT), true);
  for (const field of [
    "command",
    "agentPubkey",
    "relayUrl",
    "sessionId",
    "requestId",
    "status",
  ]) {
    const malformed = { ...PERMISSION_RESULT };
    delete malformed[field];
    assert.equal(isControlResultFrame(malformed), false, `missing ${field}`);
  }
  assert.equal(
    isControlResultFrame({ ...PERMISSION_RESULT, status: "approved" }),
    false,
  );
  assert.equal(
    isControlResultFrame({ ...PERMISSION_RESULT, command: "allow_always" }),
    false,
  );
});

test("existing cancel and switch result contracts remain validated", () => {
  assert.equal(
    isControlResultFrame({ type: "cancel_turn", status: "sent" }),
    true,
  );
  assert.equal(
    isControlResultFrame({
      type: "switch_model",
      status: "unsupported_model",
      modelId: "provider/model",
    }),
    true,
  );
  assert.equal(
    isControlResultFrame({ type: "switch_model", status: "sent" }),
    false,
  );
});

test("permission results dispatch only within the exact normalized agent scope", () => {
  resetAgentObserverStore();
  const received = [];
  const unsubscribe = subscribeControlResults(AGENT.toUpperCase(), (frame) =>
    received.push(frame),
  );

  _testDispatchControlResult(AGENT, PERMISSION_RESULT);
  _testDispatchControlResult(OTHER_AGENT, PERMISSION_RESULT);
  _testDispatchControlResult(AGENT, {
    ...PERMISSION_RESULT,
    agentPubkey: OTHER_AGENT,
  });
  _testDispatchControlResult(AGENT, {
    ...PERMISSION_RESULT,
    sessionId: "",
  });

  assert.deepEqual(received, [PERMISSION_RESULT]);
  unsubscribe();
  resetAgentObserverStore();
});
