import assert from "node:assert/strict";
import test from "node:test";

import { withAgentParentRecipient } from "./threading.ts";

const AGENT = "a".repeat(64);
const HUMAN = "b".repeat(64);
const SELF = "c".repeat(64);
const OTHER = "d".repeat(64);

const KNOWN_AGENTS = new Set([AGENT]);

test("agent parent author is appended to recipients", () => {
  assert.deepEqual(withAgentParentRecipient([], AGENT, SELF, KNOWN_AGENTS), [
    AGENT,
  ]);
});

test("existing mentions are preserved ahead of the agent parent", () => {
  assert.deepEqual(
    withAgentParentRecipient([OTHER], AGENT, SELF, KNOWN_AGENTS),
    [OTHER, AGENT],
  );
});

test("human parent author is never auto-added", () => {
  const mentions = [OTHER];
  assert.equal(
    withAgentParentRecipient(mentions, HUMAN, SELF, KNOWN_AGENTS),
    mentions,
  );
});

test("no parent author is a no-op", () => {
  const mentions = [OTHER];
  assert.equal(
    withAgentParentRecipient(mentions, null, SELF, KNOWN_AGENTS),
    mentions,
  );
  assert.equal(
    withAgentParentRecipient(mentions, undefined, SELF, KNOWN_AGENTS),
    mentions,
  );
});

test("replying to yourself never self-tags", () => {
  const mentions = [];
  assert.equal(
    withAgentParentRecipient(mentions, SELF, SELF, KNOWN_AGENTS),
    mentions,
  );
});

test("agent already mentioned is not duplicated", () => {
  const mentions = [AGENT];
  assert.equal(
    withAgentParentRecipient(mentions, AGENT, SELF, KNOWN_AGENTS),
    mentions,
  );
});

test("case and whitespace differences still dedupe and match", () => {
  const mentions = [AGENT.toUpperCase()];
  assert.equal(
    withAgentParentRecipient(mentions, ` ${AGENT} `, SELF, KNOWN_AGENTS),
    mentions,
  );
  // Registry stores normalized keys; a shouting parent pubkey still matches.
  assert.deepEqual(
    withAgentParentRecipient([], AGENT.toUpperCase(), SELF, KNOWN_AGENTS),
    [AGENT],
  );
});

test("unchanged inputs return the same array reference", () => {
  const mentions = [OTHER];
  assert.equal(
    withAgentParentRecipient(mentions, HUMAN, SELF, KNOWN_AGENTS),
    mentions,
  );
});

test("empty registry never tags anyone", () => {
  const mentions = [];
  assert.equal(
    withAgentParentRecipient(mentions, AGENT, SELF, new Set()),
    mentions,
  );
});
