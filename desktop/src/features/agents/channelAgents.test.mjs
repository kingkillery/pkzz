import assert from "node:assert/strict";
import test from "node:test";

const previousWindow = globalThis.window;
const invokes = [];
let createdCount = 0;

function rawAgent(runtime, overrides = {}) {
  createdCount += 1;
  return {
    pubkey: String(createdCount).padStart(64, "a"),
    name: "agent",
    persona_id: null,
    runtime,
    relay_url: "wss://relay.example",
    acp_command: "buzz-acp",
    agent_command: runtime ?? "raw-command",
    agent_args: [],
    mcp_command: "",
    status: "stopped",
    backend: { type: "local" },
    ...overrides,
  };
}

globalThis.window = {
  __TAURI_INTERNALS__: {
    invoke(command, args) {
      invokes.push({ command, args });
      if (command === "create_managed_agent") {
        return Promise.resolve({
          agent: rawAgent(args.input.runtimeId, {
            name: args.input.name,
            persona_id: args.input.personaId ?? null,
            agent_command: args.input.agentCommand,
          }),
          private_key_nsec: "nsec-test",
          profile_sync_error: null,
          spawn_error: null,
        });
      }
      if (command === "get_channel_members") {
        return Promise.resolve({ members: [] });
      }
      if (command === "list_managed_agents") {
        return Promise.resolve([]);
      }
      if (command === "add_channel_members") {
        return Promise.resolve({ added: args.pubkeys, errors: [] });
      }
      return Promise.reject(new Error(`Unexpected command: ${command}`));
    },
  },
};

const { ensureChannelAgentPresetInChannel, provisionChannelManagedAgent } =
  await import("./channelAgents.ts");

test.after(() => {
  globalThis.window = previousWindow;
});

const ompkRuntime = {
  id: "ompk",
  label: "Oh My PK",
  command: "ompk",
  defaultArgs: ["acp"],
  mcpCommand: "copied-capability-must-not-cross-create",
};

function managedAgent(runtime, overrides = {}) {
  return {
    pubkey: "b".repeat(64),
    name: "persona agent",
    personaId: "persona-1",
    runtime,
    agentCommand: runtime ?? "raw-command",
    systemPrompt: null,
    status: "running",
    updatedAt: "2026-08-06T00:00:00Z",
    backend: { type: "local" },
    ...overrides,
  };
}

test("catalog-backed provisioning forwards runtimeId and empty live-default args", async () => {
  invokes.length = 0;
  const result = await provisionChannelManagedAgent({
    runtime: ompkRuntime,
    name: "OMPK",
    forceNewInstance: true,
  });

  const create = invokes.find(
    (call) => call.command === "create_managed_agent",
  );
  assert.ok(create);
  assert.equal(create.args.input.runtimeId, "ompk");
  assert.deepEqual(create.args.input.agentArgs, []);
  assert.equal(create.args.input.harnessOverride, false);
  assert.equal("defaultArgs" in create.args.input, false);
  assert.equal(create.args.input.mcpCommand, undefined);
  assert.equal(result.runtimeId, "ompk");
});

test("preset creation also forwards catalog identity without copied metadata", async () => {
  invokes.length = 0;
  const result = await ensureChannelAgentPresetInChannel("channel-1", {
    runtime: ompkRuntime,
    ensureRunning: false,
  });

  const create = invokes.find(
    (call) => call.command === "create_managed_agent",
  );
  assert.ok(create);
  assert.equal(create.args.input.runtimeId, "ompk");
  assert.deepEqual(create.args.input.agentArgs, []);
  assert.equal(create.args.input.mcpCommand, undefined);
  assert.equal(result.runtimeId, "ompk");
});

test("explicit persona reuse selects only the requested effective runtime", async () => {
  invokes.length = 0;
  const goose = managedAgent("goose", { pubkey: "c".repeat(64) });
  const ompk = managedAgent("ompk", {
    pubkey: "d".repeat(64),
    status: "stopped",
  });

  const result = await provisionChannelManagedAgent(
    {
      runtime: ompkRuntime,
      name: "persona agent",
      personaId: "persona-1",
      harnessOverride: true,
    },
    {
      managedAgents: [goose, ompk],
      channelMemberPubkeys: new Set(),
    },
  );

  assert.equal(result.created, false);
  assert.equal(result.agent, ompk);
  assert.equal(result.runtimeId, "ompk");
  assert.deepEqual(invokes, []);
});

test("implicit persona reuse reports the retained agent's actual runtime", async () => {
  invokes.length = 0;
  const goose = managedAgent("goose");
  const result = await provisionChannelManagedAgent(
    {
      runtime: ompkRuntime,
      name: "persona agent",
      personaId: "persona-1",
      harnessOverride: false,
    },
    {
      managedAgents: [goose],
      channelMemberPubkeys: new Set(),
    },
  );

  assert.equal(result.agent, goose);
  assert.equal(result.runtimeId, "goose");
  assert.deepEqual(invokes, []);
});

test("implicit legacy/raw reuse reports nullable runtime identity", async () => {
  invokes.length = 0;
  const legacy = managedAgent(null);
  const result = await provisionChannelManagedAgent(
    {
      runtime: ompkRuntime,
      name: "persona agent",
      personaId: "persona-1",
    },
    {
      managedAgents: [legacy],
      channelMemberPubkeys: new Set(),
    },
  );

  assert.equal(result.agent, legacy);
  assert.equal(result.runtimeId, null);
  assert.deepEqual(invokes, []);
});
