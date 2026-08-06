import { RecoveryScreen } from "./RecoveryScreen";

export function RelaunchRequiredScreen() {
  return (
    <RecoveryScreen
      testId="relaunch-required"
      title="Restart Pkzz to finish recovery"
      body="Your identity was updated. Pkzz needs to restart so syncing and agents run under it."
    />
  );
}
