/// <reference path="./ogma.d.ts" />

Ogma.mcpTool({
  name: "echo",
  description: "Return the supplied text unchanged.",
  inputSchema: {
    type: "object",
    properties: { text: { type: "string" } },
    required: ["text"],
    additionalProperties: false,
  },
}, async (args) => {
  return String(args.text);
});
