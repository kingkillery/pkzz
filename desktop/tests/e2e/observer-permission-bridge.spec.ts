import { expect, test, type Page } from "@playwright/test";

import { installMockBridge, TEST_IDENTITIES } from "../helpers/bridge";

const AGENT_PUBKEY = TEST_IDENTITIES.tyler.pubkey;
const CHANNEL_ID = "94a444a4-c0a3-5966-ab05-530c6ddc2301";
const SESSION_ID = "permission-session-1";
const REQUEST_ID = "75a8098e-0cb2-4b47-bb95-ef9c33e17ae1";
const RELAY_URL = "wss://relay.example.test";
const MANAGED_AGENTS = [
  {
    pubkey: AGENT_PUBKEY,
    name: "Observer Agent",
    status: "running" as const,
    channelNames: ["agents"],
  },
];

type ObserverEvent = {
  seq: number;
  timestamp: string;
  kind: string;
  agentIndex: number | null;
  channelId: string | null;
  sessionId: string | null;
  turnId: string | null;
  payload: unknown;
};

async function openObserverFeed(page: Page) {
  await page.goto("/", { waitUntil: "domcontentloaded" });
  await page.waitForFunction(
    () => typeof window.__BUZZ_E2E_SEED_OBSERVER_EVENTS__ === "function",
  );
  await page.getByTestId("channel-agents").click();
  const messageRow = page
    .getByTestId("message-row")
    .filter({ has: page.getByText("Observer Agent", { exact: false }) });
  await expect(messageRow.first()).toBeVisible();
  await messageRow.first().getByRole("button").first().click();
  const activityButton = page.getByTestId(
    `user-profile-view-activity-${AGENT_PUBKEY}`,
  );
  await expect(activityButton).toBeVisible();
  await activityButton.click();
  const panel = page.getByTestId("agent-session-thread-panel");
  await expect(panel).toBeVisible();
  return panel;
}

async function installObserverControlCapture(page: Page) {
  await page.evaluate(() => {
    type CaptureWindow = Window & {
      __BUZZ_E2E_PERMISSION_CONTROLS__?: unknown[];
      __TAURI_INTERNALS__?: {
        invoke?: (
          command: string,
          args?: Record<string, unknown>,
          options?: unknown,
        ) => Promise<unknown>;
      };
    };
    const testWindow = window as CaptureWindow;
    const originalInvoke = testWindow.__TAURI_INTERNALS__?.invoke;
    if (!originalInvoke || !testWindow.__TAURI_INTERNALS__) {
      throw new Error("Tauri mock invoke bridge is unavailable");
    }
    testWindow.__BUZZ_E2E_PERMISSION_CONTROLS__ = [];
    testWindow.__TAURI_INTERNALS__.invoke = async (command, args, options) => {
      if (command === "build_observer_control_event") {
        testWindow.__BUZZ_E2E_PERMISSION_CONTROLS__?.push(args ?? null);
        return JSON.stringify({
          id: "c".repeat(64),
          pubkey: "d".repeat(64),
          created_at: Math.floor(Date.now() / 1_000),
          kind: 24200,
          tags: [
            ["p", String(args?.agentPubkey ?? "")],
            ["agent", String(args?.agentPubkey ?? "")],
            ["frame", "control"],
          ],
          content: "nip44-encrypted-control",
          sig: "e".repeat(128),
        });
      }
      return originalInvoke(command, args, options);
    };
  });
}

async function seed(page: Page, events: ObserverEvent[]) {
  await page.evaluate(
    ({ agentPubkey, observerEvents }) => {
      window.__BUZZ_E2E_SEED_OBSERVER_EVENTS__?.({
        agentPubkey,
        events: observerEvents,
      });
    },
    { agentPubkey: AGENT_PUBKEY, observerEvents: events },
  );
}

function semanticRequest(
  overrides: Record<string, unknown> = {},
): ObserverEvent {
  return {
    seq: 1,
    timestamp: new Date().toISOString(),
    kind: "permission_requested",
    agentIndex: 0,
    channelId: CHANNEL_ID,
    sessionId: SESSION_ID,
    turnId: "turn-1",
    payload: {
      requestId: REQUEST_ID,
      agentPubkey: AGENT_PUBKEY,
      relayUrl: RELAY_URL,
      sessionId: SESSION_ID,
      rpcId: 7,
      expiresAt: new Date(Date.now() + 120_000).toISOString(),
      toolCall: {
        toolCallId: "tool-1",
        title: "Write deployment manifest",
        kind: "edit",
        rawInput: { path: "deploy/app.yaml", content: "<script>no</script>" },
        locations: [{ path: "deploy/app.yaml" }],
      },
      detailsComplete: true,
      canApproveOnce: true,
      ...overrides,
    },
  };
}

function semanticResolution(outcome: string): ObserverEvent {
  return {
    seq: 3,
    timestamp: new Date(Date.now() + 1_000).toISOString(),
    kind: "permission_resolved",
    agentIndex: 0,
    channelId: CHANNEL_ID,
    sessionId: SESSION_ID,
    turnId: "turn-1",
    payload: {
      requestId: REQUEST_ID,
      agentPubkey: AGENT_PUBKEY,
      relayUrl: RELAY_URL,
      sessionId: SESSION_ID,
      rpcId: 7,
      outcome,
    },
  };
}

function controlResult(
  status: "delivered" | "not_pending" | "invalid",
  overrides: Record<string, unknown> = {},
): ObserverEvent {
  return {
    seq: 2,
    timestamp: new Date(Date.now() + 500).toISOString(),
    kind: "control_result",
    agentIndex: 0,
    channelId: CHANNEL_ID,
    sessionId: SESSION_ID,
    turnId: "turn-1",
    payload: {
      type: "control_result",
      command: "permission_decision",
      agentPubkey: AGENT_PUBKEY,
      relayUrl: RELAY_URL,
      sessionId: SESSION_ID,
      requestId: REQUEST_ID,
      status,
      ...overrides,
    },
  };
}

async function capturedControls(page: Page) {
  return page.evaluate(
    () =>
      (window as Window & { __BUZZ_E2E_PERMISSION_CONTROLS__?: unknown[] })
        .__BUZZ_E2E_PERMISSION_CONTROLS__ ?? [],
  );
}

test.describe("observer owner permission bridge", () => {
  test("approve once sends the exact binding and only terminal telemetry claims approval", async ({
    page,
  }) => {
    await installMockBridge(page, { managedAgents: MANAGED_AGENTS });
    const panel = await openObserverFeed(page);
    await installObserverControlCapture(page);
    await seed(page, [semanticRequest()]);

    await panel.getByRole("button", { name: "Approve once" }).click();
    await expect.poll(() => capturedControls(page)).toHaveLength(1);
    expect((await capturedControls(page))[0]).toEqual({
      agentPubkey: AGENT_PUBKEY,
      payload: {
        type: "permission_decision",
        agentPubkey: AGENT_PUBKEY,
        relayUrl: RELAY_URL,
        sessionId: SESSION_ID,
        requestId: REQUEST_ID,
        decision: "approve_once",
      },
    });
    await expect(panel.getByText("Approved once")).toHaveCount(0);

    await seed(page, [controlResult("delivered")]);
    await expect(
      panel.getByText("Decision sent; waiting for agent"),
    ).toBeVisible();
    await expect(panel.getByText("Approved once")).toHaveCount(0);

    await seed(page, [semanticResolution("approved")]);
    await expect(panel.getByText("Approved once")).toBeVisible();
    await expect(
      panel.getByRole("button", { name: "Approve once" }),
    ).toHaveCount(0);
    await expect(panel.getByRole("button", { name: "Reject" })).toHaveCount(0);
  });

  test("reject is one-shot and mismatched/not-pending results cannot approve", async ({
    page,
  }) => {
    await installMockBridge(page, { managedAgents: MANAGED_AGENTS });
    const panel = await openObserverFeed(page);
    await installObserverControlCapture(page);
    await seed(page, [semanticRequest()]);

    await panel
      .getByRole("button", { name: "Reject" })
      .evaluate((button: HTMLButtonElement) => {
        button.click();
        button.click();
      });
    await expect.poll(() => capturedControls(page)).toHaveLength(1);
    expect((await capturedControls(page))[0]).toMatchObject({
      payload: { decision: "reject", requestId: REQUEST_ID },
    });

    await seed(page, [controlResult("not_pending", { requestId: "wrong" })]);
    await expect(panel.getByText("No longer pending")).toHaveCount(0);
    await seed(page, [controlResult("not_pending")]);
    await expect(panel.getByText("No longer pending")).toBeVisible();
    await expect(panel.getByText("Approved once")).toHaveCount(0);

    await seed(page, [semanticResolution("rejected")]);
    await expect(panel.getByText("Rejected")).toBeVisible();
    await expect(panel.getByRole("button", { name: "Reject" })).toHaveCount(0);
  });

  test("expired and mismatched semantic requests never expose actions", async ({
    page,
  }) => {
    await installMockBridge(page, { managedAgents: MANAGED_AGENTS });
    const panel = await openObserverFeed(page);
    await seed(page, [
      semanticRequest({ expiresAt: "2000-01-01T00:00:00.000Z" }),
      {
        ...semanticRequest({ sessionId: "wrong-session", rpcId: 8 }),
        seq: 2,
      },
    ]);

    await expect(panel.getByText("Request expired")).toBeVisible();
    await expect(
      panel.getByRole("button", { name: "Approve once" }),
    ).toHaveCount(0);
    await expect(panel.getByRole("button", { name: "Reject" })).toHaveCount(0);
  });
});
