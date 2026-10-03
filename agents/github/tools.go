package main

// tools.go — the agent's tools, exposed to the LLM as OpenAI function definitions.
//
// Each tool wraps one of the GitHub API helpers in main.go; `dispatchTool` routes
// the model's tool_call (name + JSON arguments) to it. The HTTP calls go through
// the package's shared client so the loongsuite-instrumented transport propagates
// trace context (and they show up as spans).

import (
	"context"
	"encoding/json"
	"fmt"
)

const maxToolRounds = 6

const systemPrompt = `You are a GitHub assistant. Answer questions about GitHub repositories using
your tools — never guess repository data; always fetch real data with the tools.

Tool guide:
- search_repos: find repositories by keyword (sorted by stars).
- get_repo: overview of one repository (stars, forks, language, license, topics).
- get_readme: fetch a repository's README.
- get_contents: list a directory inside a repository, or read a file's contents
  (leave path empty for the repo root).

Repositories are identified as owner/repo (e.g. "golang/go"). Chain tools when
needed — e.g. search first, then get_repo or get_contents on the top result.
Answer concisely with the actual data the tools return.`

// githubTools is the tool schema advertised to the model.
var githubTools = []toolDef{
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "search_repos",
			Description: "Search GitHub repositories by keyword, sorted by stars. Returns the top 10 matches.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"query": map[string]any{"type": "string", "description": "Search keywords, e.g. 'rust async runtime' or 'a2a protocol'"},
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
			Name:        "get_repo",
			Description: "Get an overview of a repository: description, stars, forks, open issues, language, license, topics.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"owner": map[string]any{"type": "string", "description": "Repository owner, e.g. 'golang'"},
					"repo":  map[string]any{"type": "string", "description": "Repository name, e.g. 'go'"},
				},
				"required": []string{"owner", "repo"},
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
			Name:        "get_readme",
			Description: "Fetch the README of a repository.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"owner": map[string]any{"type": "string", "description": "Repository owner"},
					"repo":  map[string]any{"type": "string", "description": "Repository name"},
				},
				"required": []string{"owner", "repo"},
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
			Name:        "get_contents",
			Description: "List the contents of a directory in a repository, or read a file's contents. Leave path empty for the repository root.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"owner": map[string]any{"type": "string", "description": "Repository owner"},
					"repo":  map[string]any{"type": "string", "description": "Repository name"},
					"path":  map[string]any{"type": "string", "description": "Path inside the repo, e.g. 'src/net'. Empty string means the repo root."},
				},
				"required": []string{"owner", "repo"},
			},
		},
	},
}

// dispatchTool runs one model-requested tool and returns its text result.
// Tool errors are returned as content (not Go errors) so the model can recover.
func dispatchTool(ctx context.Context, name, argsJSON string) string {
	// All four tools share the owner/repo-shaped argument set.
	var args struct {
		Query string `json:"query"`
		Owner string `json:"owner"`
		Repo  string `json:"repo"`
		Path  string `json:"path"`
	}
	if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
		return fmt.Sprintf("bad arguments: %v", err)
	}

	var (
		out string
		err error
	)
	switch name {
	case "search_repos":
		out, err = searchRepos(ctx, args.Query)
	case "get_repo":
		out, err = repoInfo(ctx, args.Owner, args.Repo)
	case "get_readme":
		out, err = getReadme(ctx, args.Owner, args.Repo)
	case "get_contents":
		out, err = listContents(ctx, args.Owner, args.Repo, args.Path)
	default:
		return fmt.Sprintf("unknown tool: %s", name)
	}
	if err != nil {
		return fmt.Sprintf("%s failed: %v", name, err)
	}
	return out
}
