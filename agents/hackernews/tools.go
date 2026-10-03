package main

// tools.go — the agent's tools, exposed to the LLM as OpenAI function definitions.
//
// Each tool is a plain Go function; `dispatchTool` routes the model's tool_call
// (name + JSON arguments) to it. The Hacker News HTTP calls (Firebase API and
// Algolia search) go through the package's shared client so the
// loongsuite-instrumented transport propagates trace context (and they show up
// as spans).

import (
	"context"
	"encoding/json"
	"fmt"
)

const maxToolRounds = 6

const systemPrompt = `You are a Hacker News assistant. Use your tools — never invent stories.
For questions about what's trending or on the front page, call top_stories.
To find stories about a topic, call search_stories with a concise query.
For details about a specific story or item (score, comments, author), call get_item.
Answer concisely with the actual data the tools return.`

// hnTools is the tool schema advertised to the model.
var hnTools = []toolDef{
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "top_stories",
			Description: "Get the current top stories from the Hacker News front page.",
			Parameters: map[string]any{
				"type":       "object",
				"properties": map[string]any{},
			},
		},
	},
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "search_stories",
			Description: "Search Hacker News (via Algolia) for stories matching a query.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"query": map[string]any{"type": "string", "description": "Search keywords, e.g. 'Rust' or 'AI agents'"},
				},
				"required": []string{"query"},
			},
		},
	},
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "get_item",
			Description: "Get details (title, score, author, comments, URL) for a Hacker News item by its numeric ID.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"id": map[string]any{"type": "integer", "description": "Hacker News item ID"},
				},
				"required": []string{"id"},
			},
		},
	},
}

// dispatchTool runs one model-requested tool and returns its text result.
// Tool errors are returned as content (not Go errors) so the model can recover.
func dispatchTool(ctx context.Context, name, argsJSON string) string {
	switch name {
	case "top_stories":
		report, err := getTopStories(ctx)
		if err != nil {
			return fmt.Sprintf("top_stories failed: %v", err)
		}
		return report

	case "search_stories":
		var args struct {
			Query string `json:"query"`
		}
		if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
			return fmt.Sprintf("bad arguments: %v", err)
		}
		report, err := searchStories(ctx, args.Query)
		if err != nil {
			return fmt.Sprintf("search_stories failed for %q: %v", args.Query, err)
		}
		return report

	case "get_item":
		var args struct {
			ID int `json:"id"`
		}
		if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
			return fmt.Sprintf("bad arguments: %v", err)
		}
		report, err := getItemDetail(ctx, args.ID)
		if err != nil {
			return fmt.Sprintf("get_item failed for %d: %v", args.ID, err)
		}
		return report

	default:
		return fmt.Sprintf("unknown tool: %s", name)
	}
}
