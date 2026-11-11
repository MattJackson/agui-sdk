// The TypeScript client against agui-sdk's server: runs the pinned
// @ag-ui/client HttpAgent (SSE) against the Rust server at argv[2]. The
// HttpAgent pipeline applies the TS compatibility boundary, enforcement,
// chunk transformation and verifyEvents; every event is also checked against
// @ag-ui/core's EventSchemas. Exits non-zero on any rejection.
import { HttpAgent } from "@ag-ui/client";
import { EventSchemas } from "@ag-ui/core/schemas";

const url = process.argv[2];
if (!url) {
  console.error("usage: node consume.mjs <agent url>");
  process.exit(2);
}

const problems = [];
const types = [];
const agent = new HttpAgent({ url, threadId: "t-interop" });
agent.messages = [{ id: "u1", role: "user", content: "hello from TypeScript" }];

const subscriber = {
  onEvent({ event }) {
    types.push(event.type);
    const parsed = EventSchemas.safeParse(event);
    if (!parsed.success) {
      problems.push(`${event.type} fails EventSchemas: ${parsed.error.message}`);
    }
  },
  onRunErrorEvent({ event }) {
    problems.push(`RUN_ERROR from the server: ${JSON.stringify(event)}`);
  },
};

try {
  await agent.runAgent(
    {
      runId: "r-interop",
      tools: [
        {
          name: "confirm",
          description: "ask the user",
          parameters: { type: "object", properties: {} },
        },
      ],
    },
    subscriber,
  );
} catch (error) {
  problems.push(`the TS client rejected the stream: ${error?.stack ?? error}`);
}

const expect = (ok, what) => {
  if (!ok) problems.push(what);
};
expect(types[0] === "RUN_STARTED", `first event is ${types[0]}`);
expect(types.at(-1) === "RUN_FINISHED", `last event is ${types.at(-1)}`);
const assistant = agent.messages.find((m) => m.role === "assistant" && m.content === "Hello from Rust");
expect(assistant, "no assistant message 'Hello from Rust' in the TS history");
const calls = agent.messages.flatMap((m) => m.toolCalls ?? []);
expect(
  calls.some((c) => c.function?.name === "lookup" && c.function?.arguments === '{"q":"rust"}'),
  "the backend tool call is missing or its arguments differ",
);
expect(calls.some((c) => c.function?.name === "confirm"), "the frontend tool call is missing");
expect(
  agent.messages.some((m) => m.role === "tool" && m.content === "42"),
  "the backend tool result is missing",
);
expect(JSON.stringify(agent.state) === JSON.stringify({ count: 2 }), `state is ${JSON.stringify(agent.state)}`);

console.log(JSON.stringify({ events: types, messages: agent.messages.length, state: agent.state }));
if (problems.length > 0) {
  console.error(`TS consumer: ${problems.length} problem(s):\n  ${problems.join("\n  ")}`);
  process.exit(1);
}
console.error("TS consumer: the Rust server's stream was accepted");
