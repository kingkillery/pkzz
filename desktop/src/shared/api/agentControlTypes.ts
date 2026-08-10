export type CancelManagedAgentTurnResult = {
  status: "sent" | "no_active_turn";
};

/**
 * Outcome of a live `switch_model` control frame, surfaced asynchronously via
 * the agent's `control_result` observer frame. Busy path: `sent` (cancel +
 * requeue on the new model) or `turn_ending` (oneshot already consumed this
 * turn). Idle path: `switched`, `unsupported_model`, or `no_active_turn`.
 */
export type SwitchManagedAgentModelStatus =
  | "sent"
  | "turn_ending"
  | "switched"
  | "unsupported_model"
  | "no_active_turn";

export type PermissionDecision = "approve_once" | "reject";

export type PermissionDecisionBinding = {
  agentPubkey: string;
  relayUrl: string;
  sessionId: string;
  requestId: string;
};

export type CancelTurnControlResultFrame = {
  type: "cancel_turn";
  status: CancelManagedAgentTurnResult["status"];
};

export type SwitchModelControlResultFrame = {
  type: "switch_model";
  status: SwitchManagedAgentModelStatus;
  modelId: string;
};

export type PermissionControlResultFrame = PermissionDecisionBinding & {
  type: "control_result";
  command: "permission_decision";
  status: "delivered" | "not_pending" | "invalid";
};

export type ControlResultFrame =
  | CancelTurnControlResultFrame
  | SwitchModelControlResultFrame
  | PermissionControlResultFrame;
