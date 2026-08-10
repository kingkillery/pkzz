import { sendAgentObserverControl } from "@/shared/api/observerRelay";
import type {
  CancelManagedAgentTurnResult,
  PermissionDecision,
  PermissionDecisionBinding,
} from "@/shared/api/types";

export type PermissionDecisionPayload = PermissionDecisionBinding & {
  type: "permission_decision";
  decision: PermissionDecision;
};

function requireNonEmptyBindingField(
  value: string,
  field: keyof PermissionDecisionBinding,
) {
  if (value.length === 0) {
    throw new Error(`Permission decision is missing ${field}.`);
  }
}

export function buildPermissionDecisionPayload(
  managedAgentPubkey: string,
  binding: PermissionDecisionBinding,
  decision: PermissionDecision,
): PermissionDecisionPayload {
  if (managedAgentPubkey !== binding.agentPubkey) {
    throw new Error(
      "Permission decision agent does not match the managed agent.",
    );
  }
  requireNonEmptyBindingField(binding.agentPubkey, "agentPubkey");
  requireNonEmptyBindingField(binding.relayUrl, "relayUrl");
  requireNonEmptyBindingField(binding.sessionId, "sessionId");
  requireNonEmptyBindingField(binding.requestId, "requestId");
  if (decision !== "approve_once" && decision !== "reject") {
    throw new Error("Unsupported permission decision.");
  }
  return {
    type: "permission_decision",
    agentPubkey: binding.agentPubkey,
    relayUrl: binding.relayUrl,
    sessionId: binding.sessionId,
    requestId: binding.requestId,
    decision,
  };
}

export async function decideManagedAgentPermission(
  managedAgentPubkey: string,
  binding: PermissionDecisionBinding,
  decision: PermissionDecision,
): Promise<void> {
  const payload = buildPermissionDecisionPayload(
    managedAgentPubkey,
    binding,
    decision,
  );
  await sendAgentObserverControl(managedAgentPubkey, payload);
}

export async function cancelManagedAgentTurn(
  pubkey: string,
  channelId: string,
): Promise<CancelManagedAgentTurnResult> {
  await sendAgentObserverControl(pubkey, {
    type: "cancel_turn",
    channelId,
  });
  return { status: "sent" };
}

/**
 * Send a live model-switch control frame to a running agent. The switch rides
 * the harness's cancel-switch-requeue path (busy turn) or invalidate-and-reapply
 * (idle); the outcome arrives asynchronously as a `control_result` observer
 * frame, not as the return value here. This is fire-and-forget on the send side.
 */
export async function switchManagedAgentModel(
  pubkey: string,
  channelId: string,
  modelId: string,
): Promise<void> {
  await sendAgentObserverControl(pubkey, {
    type: "switch_model",
    channelId,
    modelId,
  });
}
