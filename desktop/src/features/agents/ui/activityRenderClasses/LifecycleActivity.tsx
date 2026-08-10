import * as React from "react";
import { AlertCircle, CheckCircle2, ShieldCheck, XCircle } from "lucide-react";

import { decideManagedAgentPermission } from "@/shared/api/agentControl";
import { samePermissionControlBinding } from "../agentSessionTranscriptPermissions";
import { subscribeControlResults } from "@/features/agents/observerRelayStore";
import { Button } from "@/shared/ui/button";
import { formatTranscriptTimestampTitle } from "../agentSessionUtils";
import type { TranscriptItem } from "../agentSessionTypes";
import { ActivityRow, ActivityRowLabel } from "./ActivityRow";
import { ToolActivity } from "./ToolActivity";
import type { ActivityRenderClassItemProps } from "./types";

type PermissionItem = Extract<
  TranscriptItem,
  { type: "lifecycle"; renderClass: "permission" }
>;

export function isPermissionExpired(expiresAt: string, now = Date.now()) {
  const deadline = Date.parse(expiresAt);
  return !Number.isFinite(deadline) || deadline <= now;
}

function permissionOutcomeTone(outcome: string): "approve" | "deny" | "cancel" {
  if (outcome.startsWith("Approved")) return "approve";
  if (outcome === "Rejected" || outcome === "Invalid request") return "deny";
  return "cancel";
}

function PermissionDecisionActions({
  agentPubkey,
  item,
}: {
  agentPubkey: string;
  item: PermissionItem;
}) {
  const request = item.permission;
  const terminal = Boolean(item.permissionResolution || item.outcome);
  const [now, setNow] = React.useState(() => Date.now());
  const [submission, setSubmission] = React.useState<
    "idle" | "submitting" | "delivered" | "not_pending" | "invalid"
  >("idle");
  const [error, setError] = React.useState<string | null>(null);
  const submissionLock = React.useRef(false);

  React.useEffect(() => {
    if (!request || terminal) return;
    const remaining = Date.parse(request.expiresAt) - Date.now();
    if (!Number.isFinite(remaining) || remaining <= 0) {
      setNow(Date.now());
      return;
    }
    const timeout = window.setTimeout(
      () => setNow(Date.now()),
      Math.min(remaining + 10, 2_147_483_647),
    );
    return () => window.clearTimeout(timeout);
  }, [request, terminal]);

  React.useEffect(() => {
    if (!request || terminal) return;
    return subscribeControlResults(agentPubkey, (frame) => {
      if (
        frame.type !== "control_result" ||
        frame.command !== "permission_decision" ||
        samePermissionControlBinding(item.permission?.binding, frame) === false
      ) {
        return;
      }
      setError(null);
      submissionLock.current = true;
      setSubmission(frame.status);
    });
  }, [agentPubkey, item, request, terminal]);

  React.useEffect(() => {
    if (terminal) {
      submissionLock.current = true;
      setError(null);
    }
  }, [terminal]);

  if (!request || terminal) {
    return null;
  }

  const expired = isPermissionExpired(request.expiresAt, now);
  if (expired) {
    return (
      <div
        className="mt-1.5 text-muted-foreground"
        data-testid="transcript-permission-local-expired"
      >
        Request expired
      </div>
    );
  }

  if (submission === "delivered") {
    return (
      <div className="mt-1.5 text-muted-foreground">
        Decision sent; waiting for agent
      </div>
    );
  }
  if (submission === "not_pending") {
    return (
      <div className="mt-1.5 text-destructive" role="status">
        No longer pending
      </div>
    );
  }
  if (submission === "invalid") {
    return (
      <div className="mt-1.5 text-destructive" role="status">
        Decision was rejected as invalid
      </div>
    );
  }

  const submit = async (decision: "approve_once" | "reject") => {
    if (
      submissionLock.current ||
      isPermissionExpired(request.expiresAt) ||
      (decision === "approve_once" &&
        (!request.detailsComplete || !request.canApproveOnce))
    ) {
      return;
    }
    submissionLock.current = true;
    setSubmission("submitting");
    setError(null);
    try {
      await decideManagedAgentPermission(
        agentPubkey,
        request.binding,
        decision,
      );
    } catch (cause) {
      submissionLock.current = false;
      setSubmission("idle");
      setError(
        cause instanceof Error
          ? cause.message
          : "Failed to send the permission decision.",
      );
    }
  };

  return (
    <div className="mt-1.5 border-t border-amber-500/20 pt-1.5">
      <div className="flex flex-wrap gap-1.5">
        <Button
          data-testid="transcript-permission-approve-once"
          disabled={
            submission !== "idle" ||
            !request.detailsComplete ||
            !request.canApproveOnce
          }
          onClick={() => void submit("approve_once")}
          size="xs"
          type="button"
        >
          Approve once
        </Button>
        <Button
          data-testid="transcript-permission-reject"
          disabled={submission !== "idle"}
          onClick={() => void submit("reject")}
          size="xs"
          type="button"
          variant="destructive"
        >
          Reject
        </Button>
      </div>
      {!request.detailsComplete || !request.canApproveOnce ? (
        <div className="mt-1 text-muted-foreground">
          Approval is unavailable because the operation details are incomplete.
        </div>
      ) : null}
      {submission === "submitting" ? (
        <div className="mt-1 text-muted-foreground">Sending decision…</div>
      ) : null}
      {error ? (
        <div className="mt-1 text-destructive" role="alert">
          {error}
        </div>
      ) : null}
    </div>
  );
}

export function LifecycleActivity(props: ActivityRenderClassItemProps) {
  if (props.item.type === "tool") {
    return <ToolActivity {...props} />;
  }
  if (props.item.type !== "lifecycle") {
    return null;
  }

  const isError =
    props.item.renderClass === "error" ||
    props.item.title.toLowerCase().includes("error");
  const timestampTitle = formatTranscriptTimestampTitle(props.item.timestamp);

  if (props.item.renderClass === "permission") {
    const outcome = props.item.outcome;
    const tone = outcome ? permissionOutcomeTone(outcome) : null;
    return (
      <div
        className="rounded-md border border-amber-500/20 bg-amber-500/5 px-2 py-1.5 text-left text-xs text-amber-700 dark:text-amber-400"
        data-testid="transcript-permission-item"
        title={timestampTitle}
      >
        <div>
          <ShieldCheck className="mr-1.5 inline h-3.5 w-3.5 align-text-bottom" />
          <span className="font-medium">{props.item.title}</span>
        </div>
        {props.item.text ? (
          <pre
            className="mt-1 whitespace-pre-wrap break-words pl-5 font-mono text-2xs opacity-80"
            data-testid="transcript-permission-detail"
          >
            {props.item.text}
          </pre>
        ) : null}
        {outcome && tone ? (
          <>
            <div className="my-1 border-t border-amber-500/20" />
            <div
              className={
                tone === "approve"
                  ? "flex items-center gap-1 font-medium text-green-600 dark:text-green-400"
                  : tone === "deny"
                    ? "flex items-center gap-1 font-medium text-destructive"
                    : "flex items-center gap-1 font-medium text-muted-foreground"
              }
              data-testid="transcript-permission-outcome"
            >
              {tone === "approve" ? (
                <CheckCircle2 className="h-3.5 w-3.5 shrink-0" />
              ) : (
                <XCircle className="h-3.5 w-3.5 shrink-0 opacity-70" />
              )}
              {outcome}
            </div>
          </>
        ) : (
          <PermissionDecisionActions
            agentPubkey={props.agentPubkey}
            item={props.item}
          />
        )}
      </div>
    );
  }

  if (isError) {
    return (
      <div
        className="rounded-md border border-destructive/20 bg-destructive/5 px-2 py-1.5 text-left text-xs text-destructive"
        data-testid="transcript-lifecycle-item"
        title={timestampTitle}
      >
        <AlertCircle className="mr-1.5 inline h-3.5 w-3.5 align-text-bottom" />
        <span className="font-medium">{props.item.title}</span>
        {props.item.text ? (
          <span className="opacity-80"> · {props.item.text}</span>
        ) : null}
      </div>
    );
  }

  return (
    <ActivityRow testId="transcript-lifecycle-item" title={timestampTitle}>
      <ActivityRowLabel
        object={props.item.text || undefined}
        openToneScope="none"
        verb={props.item.title}
      />
    </ActivityRow>
  );
}
