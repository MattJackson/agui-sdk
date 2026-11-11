// The TypeScript side as producer: a one-request HTTP server that answers a
// RunAgentInput (checked against @ag-ui/core's RunAgentInputSchema) with a
// run encoded by the pinned @ag-ui/encoder, every event checked against
// EventSchemas first. The wire follows the request's Accept header (protobuf
// when offered), or is always SSE with `--sse`. With `--require-proto` it
// still serves the stream but fails unless protobuf was negotiated. Prints
// `LISTENING <port>` once bound; exits after the response, non-zero if the
// request or an event was invalid, or protobuf was required and not chosen.
import http from "node:http";
import { EventEncoder } from "@ag-ui/encoder";
import { EventSchemas, RunAgentInputSchema } from "@ag-ui/core/schemas";

const sseOnly = process.argv.includes("--sse");
const protoRequired = process.argv.includes("--require-proto");
const PROTO = "application/vnd.ag-ui.event+proto";
let failed = false;

function events(input) {
  const { threadId, runId } = input;
  return [
    { type: "RUN_STARTED", threadId, runId },
    { type: "STATE_SNAPSHOT", snapshot: { count: 1 } },
    { type: "TEXT_MESSAGE_START", messageId: "m-ts", role: "assistant" },
    { type: "TEXT_MESSAGE_CONTENT", messageId: "m-ts", delta: "Hello from " },
    { type: "TEXT_MESSAGE_CONTENT", messageId: "m-ts", delta: "TypeScript" },
    { type: "TEXT_MESSAGE_END", messageId: "m-ts" },
    { type: "TOOL_CALL_START", toolCallId: "tc-ts", toolCallName: "lookup", parentMessageId: "m-ts" },
    { type: "TOOL_CALL_ARGS", toolCallId: "tc-ts", delta: '{"q":"ts"}' },
    { type: "TOOL_CALL_END", toolCallId: "tc-ts" },
    { type: "TOOL_CALL_RESULT", messageId: "tr-ts", toolCallId: "tc-ts", content: "42", role: "tool" },
    { type: "STATE_DELTA", delta: [{ op: "replace", path: "/count", value: 2 }] },
    { type: "RUN_FINISHED", threadId, runId, result: { ok: true } },
  ];
}

const server = http.createServer((req, res) => {
  let body = "";
  req.setEncoding("utf8");
  req.on("data", (chunk) => (body += chunk));
  req.on("end", () => {
    let input;
    try {
      input = RunAgentInputSchema.parse(JSON.parse(body));
    } catch (error) {
      console.error(`TS producer: the Rust client's input is invalid: ${error}`);
      failed = true;
      res.writeHead(400, { "Content-Type": "application/json" });
      res.end(JSON.stringify({ code: "input.rejected", message: String(error) }));
      server.close();
      return;
    }
    const encoder = new EventEncoder({ accept: sseOnly ? "text/event-stream" : req.headers.accept });
    console.error(`TS producer: serving ${encoder.getContentType()}`);
    if (protoRequired && encoder.getContentType() !== PROTO) {
      console.error(
        `TS producer: protobuf was required but ${encoder.getContentType()} was negotiated (Accept: ${req.headers.accept})`,
      );
      failed = true;
    }
    res.writeHead(200, { "Content-Type": encoder.getContentType() });
    for (const event of events(input)) {
      const parsed = EventSchemas.safeParse(event);
      if (!parsed.success) {
        console.error(`TS producer: ${event.type} fails EventSchemas: ${parsed.error.message}`);
        failed = true;
      }
      res.write(encoder.encodeBinary(event));
    }
    res.end();
    server.close();
  });
});

server.on("close", () => process.exit(failed ? 1 : 0));
server.listen(0, "127.0.0.1", () => {
  console.log(`LISTENING ${server.address().port}`);
});
