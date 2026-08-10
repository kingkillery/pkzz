import assert from "node:assert/strict";
import test from "node:test";

import {
  samePermissionControlBinding,
  withPermissionRequest,
  withPermissionResolution,
} from "./agentSessionTranscriptPermissions.ts";

const BASE_BINDING = {
  agentPubkey: "a".repeat(64),
  relayUrl: "ws://127.0.0.1:3000",
  sessionId: "session-a",
  requestId: "11111111-1111-4111-8111-111111111111",
};

function permissionItem(overrides = {}) {
  return {
    id: "permission:session-a:1",
    type: "lifecycle",
    renderClass: "permission",
    title: "Permission requested",
    text: "detail",
    permission: {
      binding: { ...BASE_BINDING },
      rpcId: 1,
      expiresAt: new Date(Date.now() + 60_000).toISOString(),
      toolCall: { toolCallId: "tool-1", title: "Write file" },
      detailsComplete: true,
      canApproveOnce: true,
      canReject: true,
      canCancel: true,
    },
    ...overrides,
  };
}

test("samePermissionControlBinding requires the full control tuple", () => {
  assert.equal(samePermissionControlBinding(BASE_BINDING, BASE_BINDING), true);
  assert.equal(
    samePermissionControlBinding(BASE_BINDING, {
      ...BASE_BINDING,
      agentPubkey: "b".repeat(64),
    }),
    false,
  );
  assert.equal(
    samePermissionControlBinding(BASE_BINDING, {
      ...BASE_BINDING,
      relayUrl: "ws://127.0.0.1:3001",
    }),
    false,
  );
  assert.equal(
    samePermissionControlBinding(BASE_BINDING, {
      ...BASE_BINDING,
      sessionId: "session-b",
    }),
    false,
  );
  assert.equal(
    samePermissionControlBinding(BASE_BINDING, {
      ...BASE_BINDING,
      requestId: "22222222-2222-4222-8222-222222222222",
    }),
    false,
  );
});

test("withPermissionResolution rejects mismatched agentPubkey or relayUrl", () => {
  const existing = permissionItem();
  const resolution = {
    binding: {
      ...BASE_BINDING,
      agentPubkey: "b".repeat(64),
    },
    rpcId: 1,
    outcome: "approved",
    resolvedAt: new Date().toISOString(),
  };
  assert.equal(withPermissionResolution(existing, resolution), null);

  const relayMismatch = {
    ...resolution,
    binding: {
      ...BASE_BINDING,
      relayUrl: "ws://example.invalid/relay",
    },
  };
  assert.equal(withPermissionResolution(existing, relayMismatch), null);
});

test("withPermissionResolution accepts a matching full binding tuple", () => {
  const existing = permissionItem();
  const resolution = {
    binding: { ...BASE_BINDING },
    rpcId: 1,
    outcome: "approved",
    resolvedAt: new Date().toISOString(),
  };
  const merged = withPermissionResolution(existing, resolution);
  assert.ok(merged);
  assert.equal(merged.permissionResolution?.outcome, "approved");
  assert.equal(merged.outcome, "Approved once");
});

test("withPermissionRequest rejects mismatched pubkey/relay on an existing item", () => {
  const existing = permissionItem();
  const request = {
    ...existing.permission,
    binding: {
      ...BASE_BINDING,
      relayUrl: "ws://127.0.0.1:3999",
    },
  };
  assert.equal(withPermissionRequest(existing, request), null);

  const matched = withPermissionRequest(existing, existing.permission);
  assert.ok(matched);
  assert.equal(matched.permission?.binding.requestId, BASE_BINDING.requestId);
});
