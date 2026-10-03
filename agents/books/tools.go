package main

// tools.go — the agent's tools, exposed to the LLM as OpenAI function definitions.
//
// Each tool is a plain Go function; `dispatchTool` routes the model's tool_call
// (name + JSON arguments) to it. The Open Library HTTP calls go through the
// package's shared client so the loongsuite-instrumented transport propagates
// trace context (and they show up as spans).

import (
	"context"
	"encoding/json"
	"fmt"
)

const maxToolRounds = 6

const systemPrompt = `You are a book assistant. Use your tools — never invent titles, authors, or ratings.
For any book question (recommendations, "books by X", "books about Y"), call
search_books with a focused query to fetch real data from Open Library, then
recommend and answer from the actual results it returns. If a search comes back
empty or off-target, refine the query and try again before giving up.`

// booksTools is the tool schema advertised to the model.
var booksTools = []toolDef{
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "search_books",
			Description: "Search Open Library for books by title, author, subject, or free-text query. Returns top matches with authors, first publish year, page count, rating, and subjects.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"query": map[string]any{"type": "string", "description": "Search query, e.g. 'Isaac Asimov', 'history of Rome', or 'machine learning'"},
				},
				"required": []string{"query"},
			},
		},
	},
}

// dispatchTool runs one model-requested tool and returns its text result.
// Tool errors are returned as content (not Go errors) so the model can recover.
func dispatchTool(ctx context.Context, name, argsJSON string) string {
	switch name {
	case "search_books":
		var args struct {
			Query string `json:"query"`
		}
		if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
			return fmt.Sprintf("bad arguments: %v", err)
		}
		result, err := searchBooks(ctx, args.Query)
		if err != nil {
			return fmt.Sprintf("search_books failed: %v", err)
		}
		return result

	default:
		return fmt.Sprintf("unknown tool: %s", name)
	}
}
