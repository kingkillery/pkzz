import assert from "node:assert/strict";
import test from "node:test";
import React from "react";
import { renderToStaticMarkup } from "react-dom/server";

import {
  LifecycleActivity,
  isPermissionExpired,
} from "./LifecycleActivity.tsx";

const AGENT = "a".repeat(64);
const BINDING = {
  agentPubkey: AGENT,
  relayUrl: "wss://relay.example.test",
  sessionId: "session-1",
  requestId: "75a8098e-0cb2-4b47-bb95-ef9c33e17ae1",
};

function permissionItem(overrides = {}) {
  return {
    id: "permission:session-1:1",
    type: "lifecycle",
    renderClass: "permission",
    title: "Run untrusted command",
    text: "Input:\n<script>alert('owned')</script> && rm -rf ./build",
    timestamp: "2026-06-30T10:00:00.000Z",
    sessionId: "session-1",
    channelId: "channel-1",
    turnId: "turn-1",
    permission: {
      binding: BINDING,
      rpcId: 1,
      expiresAt: "2099-06-30T10:02:00.000Z",
      canApproveOnce: true,
      detailsComplete: true,
      toolCall: {
        toolCallId: "tool-1",
        title: "Run untrusted command",
        rawInput: { command: "<script>alert('owned')</script>" },
      },
    },
    ...overrides,
  };
}

function render(item) {
  return renderToStaticMarkup(
    React.createElement(LifecycleActivity, {
      item,
      agentPubkey: AGENT,
      agentName: "Observer Agent",
      agentAvatarUrl: null,
    }),
  );
}

test("pending trusted request renders exactly Approve once and Reject controls", () => {
  const html = render(permissionItem());
  assert.match(html, />Approve once<\/button>/);
  assert.match(html, />Reject<\/button>/);
  assert.doesNotMatch(html, /Always allow|allow_always|optionId/);
});

test("terminal permission outcomes remove every decision button", () => {
  for (const outcome of ["Approved once", "Rejected", "Expired", "Cancelled"]) {
    const html = render(permissionItem({ outcome }));
    assert.match(html, new RegExp(outcome));
    assert.doesNotMatch(html, /<button/);
  }
});

test("incomplete details disable approval while retaining one-shot rejection", () => {
  const item = permissionItem();
  item.permission = {
    ...item.permission,
    detailsComplete: false,
    canApproveOnce: false,
  };
  const html = render(item);
  assert.match(
    html,
    /data-testid="transcript-permission-approve-once"[^>]*disabled=""/,
  );
  assert.match(html, /data-testid="transcript-permission-reject"/);
  assert.match(html, /operation details are incomplete/);
});

test("expired display deadline removes actions without manufacturing approval", () => {
  const item = permissionItem();
  item.permission = { ...item.permission, expiresAt: "2000-01-01T00:00:00Z" };
  const html = render(item);
  assert.match(html, /Request expired/);
  assert.doesNotMatch(html, /<button/);
  assert.doesNotMatch(html, /Approved/);
  assert.equal(isPermissionExpired(item.permission.expiresAt), true);
});

test("operation detail is escaped display data and never injected as HTML", () => {
  const html = render(permissionItem());
  assert.match(
    html,
    /&lt;script&gt;alert\(&#x27;owned&#x27;\)&lt;\/script&gt;/,
  );
  assert.doesNotMatch(html, /<script>/);
  assert.match(html, /rm -rf \.\/build/);
});
