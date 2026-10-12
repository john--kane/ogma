  const mcpTools = new Map();
  function @@REGISTRATION@@(definition, handler) {
    if (!definition || typeof definition !== "object" || typeof handler !== "function")
      throw new TypeError("MCP registration requires a tool definition and handler");
    const serialized = JSON.stringify(definition);
    native("mcpValidateTool", serialized);
    const metadata = JSON.parse(serialized);
    if (mcpTools.has(metadata.name) || mcpTools.size >= @@MAX_TOOLS@@)
      throw new TypeError("Duplicate MCP tool or tool count exceeded");
    mcpTools.set(metadata.name, { metadata, handler });
  }
  globalThis.__ogmaRunMcp = async (operation, invocation) => {
    if (operation === "mcp-list") {
      native("mcpResult", JSON.stringify(Array.from(mcpTools.values(), tool => tool.metadata)));
      return;
    }
    const tool = mcpTools.get(invocation.name);
    if (!tool) return native("mcpMissingTool", invocation.name);
    native("mcpValidateArguments", JSON.stringify(tool.metadata), JSON.stringify(invocation.arguments));
    const result = await tool.handler(invocation.arguments);
    if (typeof result !== "string") throw new TypeError("MCP tool handlers must return text");
    native("mcpResult", JSON.stringify(result));
  };
