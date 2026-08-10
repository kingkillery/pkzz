@echo off
setlocal DisableDelayedExpansion

if defined BUZZ_ACP_WINDOWS_FIXTURE_MARKER > "%BUZZ_ACP_WINDOWS_FIXTURE_MARKER%" echo entered
if not "%~1"=="acp" exit /b 11
if not "%~2"=="argument with spaces" exit /b 12
if not "%BUZZ_ACP_WINDOWS_FIXTURE_ENV%"=="layered environment value" exit /b 13

set /p "_request="
echo {"jsonrpc":"2.0","id":0,"result":{"protocolVersion":2,"agentCapabilities":{},"serverInfo":{"name":"ompk-shim","version":"1.0.0"}}}
set /p "_request="
echo {"jsonrpc":"2.0","id":1,"result":{"sessionId":"windows-batch-session","configOptions":[{"configId":"model","displayName":"Model","category":"model","type":"select","currentValue":"fixture/model","options":[{"value":"fixture/model","displayName":"Fixture Model"}]}]}}
set /p "_request="
