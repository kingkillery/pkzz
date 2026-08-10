import type {
  AgentActivityDescriptor,
  AgentActivityRenderClass,
  TranscriptItem,
  TranscriptPermissionBinding,
  TranscriptPermissionRequest,
  TranscriptPermissionResolution,
  TranscriptPermissionRpcId,
  TranscriptPermissionToolCall,
} from "./agentSessionTypes";
import { asRecord, asString } from "./agentSessionUtils";

export type TranscriptItemContext = {
  channelId: string | null;
  turnId: string | null;
  sessionId: string | null;
};

type PermissionRequestDescription = {
  title: string;
  text: string;
  optionNames: Map<string, string>;
  descriptor: AgentActivityDescriptor;
};

type SemanticPermissionRequestDescription = {
  itemId: string;
  title: string;
  text: string;
  request: TranscriptPermissionRequest | null;
};

type SemanticPermissionResolutionDescription = {
  itemId: string;
  correlationKey: string;
  resolution: TranscriptPermissionResolution;
};

export function stringifyPayload(value: unknown) {
  try {
    return JSON.stringify(value, null, 2) ?? String(value);
  } catch {
    return String(value);
  }
}

function permissionToolCall(
  value: unknown,
): TranscriptPermissionToolCall | null {
  const toolCall = asRecord(value);
  const toolCallId =
    asString(toolCall.toolCallId) ?? asString(toolCall.tool_call_id);
  const title = asString(toolCall.title);
  if (!toolCallId || !title) {
    return null;
  }
  const kind = asString(toolCall.kind);
  return {
    toolCallId,
    title,
    ...(kind ? { kind } : {}),
    ...("rawInput" in toolCall ? { rawInput: toolCall.rawInput } : {}),
    ...(Array.isArray(toolCall.content) ? { content: toolCall.content } : {}),
    ...(Array.isArray(toolCall.locations)
      ? { locations: toolCall.locations }
      : {}),
  };
}

function describePermissionToolCall(toolCall: TranscriptPermissionToolCall) {
  const detail: string[] = [];
  if (toolCall.title !== "Permission requested") {
    detail.push(toolCall.title);
  }
  detail.push(`Tool call: ${toolCall.toolCallId}`);
  if (toolCall.kind) {
    detail.push(`Operation: ${toolCall.kind}`);
  }
  if ("rawInput" in toolCall) {
    detail.push(`Input:\n${stringifyPayload(toolCall.rawInput)}`);
  }
  if (toolCall.content) {
    detail.push(`Content:\n${stringifyPayload(toolCall.content)}`);
  }
  if (toolCall.locations) {
    detail.push(`Locations:\n${stringifyPayload(toolCall.locations)}`);
  }
  detail.push("Approval applies to this call only.");
  return detail.join("\n");
}

export function describePermissionRequest(
  payload: Record<string, unknown>,
): PermissionRequestDescription {
  const params = asRecord(payload.params);
  const nestedToolCall = permissionToolCall(params.toolCall);
  const fallbackToolCall =
    permissionToolCall({
      toolCallId: params.toolCallId ?? params.tool_call_id,
      title: params.title ?? params.message ?? params.reason,
      kind: params.kind,
      rawInput: params.rawInput,
      content: params.content,
      locations: params.locations,
    }) ?? null;
  const toolCall = nestedToolCall ?? fallbackToolCall;
  const title = toolCall?.title ?? "Permission requested";

  // Adapter option IDs never enter display text or actionable permission data.
  // Retain only the raw optionId → kind relation to interpret the ACP response.
  const optionNames = new Map<string, string>();
  if (Array.isArray(params.options)) {
    for (const option of params.options) {
      const record = asRecord(option);
      const optionId = asString(record.optionId);
      const kind = asString(record.kind);
      if (optionId && kind) {
        optionNames.set(optionId, kind);
      }
    }
  }

  return {
    title,
    text: toolCall ? describePermissionToolCall(toolCall) : "",
    optionNames,
    descriptor: {
      renderClass: "permission",
      label: "Permission requested",
      preview: title,
      action: { verb: "Requested", object: title },
      tone: "admin",
      operation: "session/request_permission",
      object: title,
      source: "acp",
      groupKey: "permission:request",
    },
  };
}

export function describePermissionOutcome(
  outcome: string,
  optionId: string | null,
  optionNames: Map<string, string>,
): string {
  if (outcome === "cancelled") {
    return "Cancelled";
  }
  if (outcome === "selected" && optionId) {
    const kind = optionNames.get(optionId);
    if (kind === "allow_once") return "Approved once";
    if (kind?.startsWith("reject")) return "Rejected";
    if (kind?.startsWith("allow")) return "Approved";
  }
  return "Unavailable";
}

function permissionRpcId(value: unknown): TranscriptPermissionRpcId | null {
  if (typeof value === "string") return value;
  if (typeof value === "number" && Number.isFinite(value)) return value;
  return null;
}

export function permissionCorrelationKey(
  sessionId: string | null | undefined,
  rpcId: unknown,
): string | null {
  const typedRpcId = permissionRpcId(rpcId);
  if (!sessionId || typedRpcId === null) {
    return null;
  }
  return `${JSON.stringify(sessionId)}:${JSON.stringify(typedRpcId)}`;
}

export function permissionItemId(
  sessionId: string,
  rpcId: TranscriptPermissionRpcId,
) {
  return `permission:${JSON.stringify(sessionId)}:${JSON.stringify(rpcId)}`;
}

function permissionBinding(
  payload: Record<string, unknown>,
): TranscriptPermissionBinding | null {
  const agentPubkey = asString(payload.agentPubkey);
  const relayUrl = asString(payload.relayUrl);
  const sessionId = asString(payload.sessionId);
  const requestId = asString(payload.requestId);
  if (
    !agentPubkey ||
    !/^[0-9a-f]{64}$/.test(agentPubkey) ||
    !relayUrl ||
    !sessionId ||
    !requestId ||
    !/^[0-9a-f]{8}-[0-9a-f]{4}-[1-5][0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(
      requestId,
    )
  ) {
    return null;
  }
  return { agentPubkey, relayUrl, sessionId, requestId };
}

export function samePermissionControlBinding(
  left: TranscriptPermissionBinding | null | undefined,
  right:
    | Pick<
        TranscriptPermissionBinding,
        "agentPubkey" | "relayUrl" | "sessionId" | "requestId"
      >
    | null
    | undefined,
): boolean {
  if (left == null || right == null) {
    return false;
  }
  return (
    left.requestId === right.requestId &&
    left.agentPubkey === right.agentPubkey &&
    left.relayUrl === right.relayUrl &&
    left.sessionId === right.sessionId
  );
}

function semanticPermissionRequest(
  payload: Record<string, unknown>,
  eventSessionId: string | null,
): TranscriptPermissionRequest | null {
  const binding = permissionBinding(payload);
  const rpcId = permissionRpcId(payload.rpcId);
  const expiresAt = asString(payload.expiresAt);
  const toolCall = permissionToolCall(payload.toolCall);
  if (
    !binding ||
    binding.sessionId !== eventSessionId ||
    rpcId === null ||
    !expiresAt ||
    !Number.isFinite(Date.parse(expiresAt)) ||
    !toolCall ||
    typeof payload.detailsComplete !== "boolean" ||
    typeof payload.canApproveOnce !== "boolean"
  ) {
    return null;
  }
  return {
    binding,
    rpcId,
    expiresAt,
    detailsComplete: payload.detailsComplete,
    canApproveOnce: payload.canApproveOnce,
    toolCall,
  };
}

function isPermissionResolutionOutcome(
  value: string,
): value is TranscriptPermissionResolution["outcome"] {
  return [
    "approved",
    "rejected",
    "expired",
    "cancelled",
    "unavailable",
    "invalid_request",
  ].includes(value);
}

function semanticPermissionResolution(
  payload: Record<string, unknown>,
  eventSessionId: string | null,
  resolvedAt: string,
): TranscriptPermissionResolution | null {
  const binding = permissionBinding(payload);
  const rpcId = permissionRpcId(payload.rpcId);
  const outcome = asString(payload.outcome);
  if (
    !binding ||
    binding.sessionId !== eventSessionId ||
    rpcId === null ||
    !outcome ||
    !isPermissionResolutionOutcome(outcome)
  ) {
    return null;
  }
  return { binding, rpcId, outcome, resolvedAt };
}

function permissionResolutionLabel(
  outcome: TranscriptPermissionResolution["outcome"],
) {
  const labels: Record<TranscriptPermissionResolution["outcome"], string> = {
    approved: "Approved once",
    rejected: "Rejected",
    expired: "Expired",
    cancelled: "Cancelled",
    unavailable: "Unavailable",
    invalid_request: "Invalid request",
  };
  return labels[outcome];
}

export function describeSemanticPermissionRequest(
  payloadValue: unknown,
  eventSessionId: string | null,
): SemanticPermissionRequestDescription | null {
  const payload = asRecord(payloadValue);
  const rpcId = permissionRpcId(payload.rpcId);
  const sessionId = asString(payload.sessionId);
  const toolCall = permissionToolCall(payload.toolCall);
  if (
    rpcId === null ||
    !sessionId ||
    sessionId !== eventSessionId ||
    !toolCall
  ) {
    return null;
  }
  return {
    itemId: permissionItemId(sessionId, rpcId),
    title: toolCall.title,
    text: describePermissionToolCall(toolCall),
    request: semanticPermissionRequest(payload, eventSessionId),
  };
}

export function describeSemanticPermissionResolution(
  payloadValue: unknown,
  eventSessionId: string | null,
  resolvedAt: string,
): SemanticPermissionResolutionDescription | null {
  const resolution = semanticPermissionResolution(
    asRecord(payloadValue),
    eventSessionId,
    resolvedAt,
  );
  if (!resolution) {
    return null;
  }
  const correlationKey = permissionCorrelationKey(
    resolution.binding.sessionId,
    resolution.rpcId,
  );
  if (!correlationKey) {
    return null;
  }
  return {
    itemId: permissionItemId(resolution.binding.sessionId, resolution.rpcId),
    correlationKey,
    resolution,
  };
}

export function joinLifecycleText(existing: string, next: string) {
  if (!existing) return next;
  if (!next) return existing;
  return `${existing}\n${next}`;
}

export function nextLifecycleItem({
  acpSource,
  ctx,
  descriptor,
  existing,
  id,
  renderClass,
  text,
  timestamp,
  title,
}: {
  acpSource?: string;
  ctx: TranscriptItemContext;
  descriptor?: AgentActivityDescriptor;
  existing: TranscriptItem | undefined;
  id: string;
  renderClass: Extract<
    AgentActivityRenderClass,
    "error" | "permission" | "status"
  >;
  text: string;
  timestamp: string;
  title: string;
}): TranscriptItem {
  const common = {
    id,
    type: "lifecycle" as const,
    title,
    text:
      existing?.type === "lifecycle"
        ? joinLifecycleText(existing.text, text)
        : text,
    timestamp: existing?.type === "lifecycle" ? existing.timestamp : timestamp,
    descriptor:
      descriptor ??
      (existing?.type === "lifecycle" ? existing.descriptor : undefined),
    channelId: ctx.channelId,
    turnId:
      ctx.turnId ?? (existing?.type === "lifecycle" ? existing.turnId : null),
    sessionId:
      ctx.sessionId ??
      (existing?.type === "lifecycle" ? existing.sessionId : null),
    acpSource:
      acpSource ??
      (existing?.type === "lifecycle" ? existing.acpSource : undefined),
  };
  return renderClass === "permission"
    ? {
        ...common,
        renderClass,
        ...(existing?.type === "lifecycle" &&
        existing.renderClass === "permission"
          ? {
              outcome: existing.outcome,
              permission: existing.permission,
              permissionResolution: existing.permissionResolution,
            }
          : {}),
      }
    : { ...common, renderClass };
}

export function withPermissionRequest(
  existing: TranscriptItem | undefined,
  request: TranscriptPermissionRequest,
): TranscriptItem | null {
  if (existing?.type !== "lifecycle" || existing.renderClass !== "permission") {
    return null;
  }
  if (
    existing.permissionResolution &&
    samePermissionControlBinding(
      existing.permissionResolution.binding,
      request.binding,
    ) === false
  ) {
    return null;
  }
  if (
    existing.permission &&
    samePermissionControlBinding(
      existing.permission.binding,
      request.binding,
    ) === false
  ) {
    return null;
  }
  return {
    ...existing,
    title: request.toolCall.title,
    text: describePermissionToolCall(request.toolCall),
    permission: request,
  };
}

export function withPermissionResolution(
  existing: TranscriptItem | undefined,
  resolution: TranscriptPermissionResolution,
): TranscriptItem | null {
  if (existing?.type !== "lifecycle" || existing.renderClass !== "permission") {
    return null;
  }
  if (
    existing.permission &&
    samePermissionControlBinding(
      existing.permission.binding,
      resolution.binding,
    ) === false
  ) {
    return null;
  }
  if (
    existing.permissionResolution &&
    samePermissionControlBinding(
      existing.permissionResolution.binding,
      resolution.binding,
    ) === false
  ) {
    return null;
  }
  return {
    ...existing,
    outcome: permissionResolutionLabel(resolution.outcome),
    permissionResolution: resolution,
  };
}

export function clearPriorSessionPermissionActions(
  items: readonly TranscriptItem[],
  channelId: string | null,
  sessionId: string,
): TranscriptItem[] {
  const updates: TranscriptItem[] = [];
  for (const item of items) {
    if (
      item.type !== "lifecycle" ||
      item.renderClass !== "permission" ||
      !item.permission ||
      item.permissionResolution ||
      item.channelId !== channelId ||
      item.permission.binding.sessionId === sessionId
    ) {
      continue;
    }
    updates.push({ ...item, permission: undefined });
  }
  return updates;
}
