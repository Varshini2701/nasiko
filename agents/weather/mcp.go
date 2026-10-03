package main

// mcp.go — the reference MCP-gateway client for platform agents.
//
// The whole configuration is two env vars, both injected at deploy time
// (docs: MCP_GATEWAY_DESIGN.md, MCP gateway agent auth):
//
//	MCP_GATEWAY_URL   — where the platform's aggregating gateway lives
//	MCP_GATEWAY_TOKEN — this agent's own credential, sent as a Bearer header
//
// There is NO per-request credential to plumb: the platform resolves the
// calling user server-side from the `traceparent` this agent forwards (the
// same header it already forwards on LLM calls — automatic under the
// loongsuite-instrumented build, manual fallback here), and authorizes the
// call only when this agent is a recorded participant of that flow. An agent
// that drops the header gets a clean 403, never misattribution.
//
// Discovery (`tools/list`) runs once at startup with agent-only identity —
// no flow exists yet, and none is needed for read-only metadata.

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net/http"
	"os"
)

type mcpClient struct {
	url   string
	token string
	http  *http.Client
	// Gateway tools discovered at startup, already in OpenAI function shape.
	tools []toolDef
	// Names of gateway tools, for dispatch routing.
	names map[string]bool
}

// newMCPClient returns a configured client, or nil when the platform didn't
// inject the gateway env (local runs, MCP disabled) — the agent then simply
// runs with its built-in tools only.
func newMCPClient() *mcpClient {
	url := os.Getenv("MCP_GATEWAY_URL")
	token := os.Getenv("MCP_GATEWAY_TOKEN")
	if url == "" || token == "" {
		return nil
	}
	return &mcpClient{url: url, token: token, http: &http.Client{}, names: map[string]bool{}}
}

// rpc posts one JSON-RPC request to the gateway. traceparent may be empty
// (startup discovery); when set it names the flow being served.
func (c *mcpClient) rpc(ctx context.Context, traceparent string, body map[string]any) (json.RawMessage, error) {
	payload, err := json.Marshal(body)
	if err != nil {
		return nil, err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.url, bytes.NewReader(payload))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.token)
	if traceparent != "" {
		req.Header.Set("traceparent", traceparent)
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()
	raw, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("gateway HTTP %d: %s", resp.StatusCode, string(raw))
	}
	var out struct {
		Result json.RawMessage `json:"result"`
		Error  *struct {
			Code    int    `json:"code"`
			Message string `json:"message"`
		} `json:"error"`
	}
	if err := json.Unmarshal(raw, &out); err != nil {
		return nil, fmt.Errorf("gateway response parse: %w", err)
	}
	if out.Error != nil {
		return nil, fmt.Errorf("gateway error %d: %s", out.Error.Code, out.Error.Message)
	}
	return out.Result, nil
}

// discoverTools fetches the agent's reachable tool manifest and converts it to
// the OpenAI function shape the LLM loop speaks. Best-effort: a failure only
// means the agent runs without gateway tools.
func (c *mcpClient) discoverTools(ctx context.Context) {
	result, err := c.rpc(ctx, "", map[string]any{
		"jsonrpc": "2.0", "id": 1, "method": "tools/list",
	})
	if err != nil {
		log.Printf("mcp: tools/list failed (continuing without gateway tools): %v", err)
		return
	}
	var manifest struct {
		Tools []struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			InputSchema map[string]any `json:"inputSchema"`
		} `json:"tools"`
	}
	if err := json.Unmarshal(result, &manifest); err != nil {
		log.Printf("mcp: tools/list parse failed: %v", err)
		return
	}
	for _, t := range manifest.Tools {
		params := t.InputSchema
		if params == nil {
			params = map[string]any{"type": "object", "properties": map[string]any{}}
		}
		var def toolDef
		def.Type = "function"
		def.Function.Name = t.Name
		def.Function.Description = t.Description
		def.Function.Parameters = params
		c.tools = append(c.tools, def)
		c.names[t.Name] = true
	}
	log.Printf("mcp: discovered %d gateway tools", len(c.tools))
}

// callTool executes one gateway tool on behalf of the flow named by
// traceparent. Errors come back as content (not Go errors) so the model can
// recover — same convention as dispatchTool.
func (c *mcpClient) callTool(ctx context.Context, traceparent, name, argsJSON string) string {
	var args map[string]any
	if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
		return fmt.Sprintf("bad arguments: %v", err)
	}
	result, err := c.rpc(ctx, traceparent, map[string]any{
		"jsonrpc": "2.0", "id": 1, "method": "tools/call",
		"params": map[string]any{"name": name, "arguments": args},
	})
	if err != nil {
		return fmt.Sprintf("tool %s failed: %v", name, err)
	}
	// Prefer the standard MCP text content; fall back to the raw result JSON
	// so structured-only results still reach the model.
	var content struct {
		Content []struct {
			Type string `json:"type"`
			Text string `json:"text"`
		} `json:"content"`
	}
	if err := json.Unmarshal(result, &content); err == nil {
		for _, c := range content.Content {
			if c.Type == "text" && c.Text != "" {
				return c.Text
			}
		}
	}
	return string(result)
}
