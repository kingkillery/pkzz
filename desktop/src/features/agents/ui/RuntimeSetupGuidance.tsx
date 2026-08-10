import type { AcpRuntimeCatalogEntry } from "@/shared/api/types";
import { runtimeAvailabilityWarning } from "./runtimeAvailabilityWarning";

export function RuntimeSetupGuidance({
  runtime,
}: {
  runtime: AcpRuntimeCatalogEntry | undefined;
}) {
  const warning = runtime ? runtimeAvailabilityWarning(runtime) : null;
  return warning ? (
    <p className="text-xs text-warning">
      {warning} Visit Settings &gt; Agents to set it up.
    </p>
  ) : null;
}
