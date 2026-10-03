package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"iter"
	"log"
	"net"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2asrv"
	// [nasiko:imports]
)

type githubExecutor struct {
	llm *llmClient
}

var _ a2asrv.AgentExecutor = (*githubExecutor)(nil)

func (g *githubExecutor) Execute(ctx context.Context, execCtx *a2asrv.ExecutorContext) iter.Seq2[a2a.Event, error] {
	// The inbound W3C trace context, forwarded on the LLM call for attribution.
	traceparent := firstParam(execCtx, "traceparent")
	return func(yield func(a2a.Event, error) bool) {
		userText := extractText(execCtx.Message)
		// Run the LLM tool-calling loop: the model picks the GitHub tools, we
		// execute them, then the model synthesizes the answer.
		result, err := g.llm.runAgentLoop(ctx, userText, traceparent)
		if err != nil {
			yield(nil, err)
			return
		}
		yield(a2a.NewMessage(a2a.MessageRoleAgent, a2a.NewTextPart(result)), nil)
	}
}

// firstParam reads a single-valued service param (the a2a-go handler copies the
// inbound HTTP headers into ExecutorContext.ServiceParams, lowercased).
func firstParam(execCtx *a2asrv.ExecutorContext, name string) string {
	if execCtx == nil || execCtx.ServiceParams == nil {
		return ""
	}
	vals, ok := execCtx.ServiceParams.Get(name)
	if !ok || len(vals) == 0 {
		return ""
	}
	return vals[0]
}

func (*githubExecutor) Cancel(ctx context.Context, execCtx *a2asrv.ExecutorContext) iter.Seq2[a2a.Event, error] {
	return func(yield func(a2a.Event, error) bool) {}
}

func searchRepos(ctx context.Context, query string) (string, error) {
	apiURL := fmt.Sprintf("https://api.github.com/search/repositories?q=%s&sort=stars&order=desc&per_page=10", url.QueryEscape(query))
	var data struct {
		TotalCount int `json:"total_count"`
		Items      []struct {
			FullName    string `json:"full_name"`
			Description string `json:"description"`
			Stars       int    `json:"stargazers_count"`
			Language    string `json:"language"`
			HTMLURL     string `json:"html_url"`
		} `json:"items"`
	}
	if err := githubGet(ctx, apiURL, "", &data); err != nil {
		return "", err
	}
	if len(data.Items) == 0 {
		return fmt.Sprintf("No repositories found for %q.", query), nil
	}

	var sb strings.Builder
	sb.WriteString(fmt.Sprintf("Top repositories for %q (%d total):\n\n", query, data.TotalCount))
	for i, r := range data.Items {
		lang := r.Language
		if lang == "" {
			lang = "n/a"
		}
		sb.WriteString(fmt.Sprintf("%d. %s — ★ %d, %s\n   %s\n   %s\n",
			i+1, r.FullName, r.Stars, lang, firstLine(r.Description), r.HTMLURL))
	}
	return sb.String(), nil
}

func repoInfo(ctx context.Context, owner, repo string) (string, error) {
	apiURL := fmt.Sprintf("https://api.github.com/repos/%s/%s", owner, repo)
	var data struct {
		FullName    string `json:"full_name"`
		Description string `json:"description"`
		Stars       int    `json:"stargazers_count"`
		Forks       int    `json:"forks_count"`
		OpenIssues  int    `json:"open_issues_count"`
		Language    string `json:"language"`
		License     *struct {
			Name string `json:"name"`
		} `json:"license"`
		DefaultBranch string   `json:"default_branch"`
		Topics        []string `json:"topics"`
		HTMLURL       string   `json:"html_url"`
		UpdatedAt     string   `json:"updated_at"`
	}
	if err := githubGet(ctx, apiURL, "", &data); err != nil {
		return "", err
	}

	license := "none"
	if data.License != nil {
		license = data.License.Name
	}
	var sb strings.Builder
	sb.WriteString(fmt.Sprintf("%s\n%s\n\n", data.FullName, firstLine(data.Description)))
	sb.WriteString(fmt.Sprintf("Stars: %d | Forks: %d | Open issues: %d\n", data.Stars, data.Forks, data.OpenIssues))
	sb.WriteString(fmt.Sprintf("Language: %s | License: %s | Default branch: %s\n", data.Language, license, data.DefaultBranch))
	if len(data.Topics) > 0 {
		sb.WriteString(fmt.Sprintf("Topics: %s\n", strings.Join(data.Topics, ", ")))
	}
	sb.WriteString(fmt.Sprintf("Last updated: %s\n%s\n", data.UpdatedAt, data.HTMLURL))
	return sb.String(), nil
}

func listContents(ctx context.Context, owner, repo, path string) (string, error) {
	apiURL := fmt.Sprintf("https://api.github.com/repos/%s/%s/contents/%s", owner, repo, url.PathEscape(path))
	var entries []struct {
		Name string `json:"name"`
		Type string `json:"type"`
		Size int    `json:"size"`
	}
	if err := githubGet(ctx, apiURL, "", &entries); err != nil {
		// A file path returns an object, not an array — fall back to raw content.
		return getFile(ctx, owner, repo, path)
	}

	loc := owner + "/" + repo
	if path != "" {
		loc += "/" + path
	}
	var sb strings.Builder
	sb.WriteString(fmt.Sprintf("Contents of %s:\n\n", loc))
	for _, e := range entries {
		if e.Type == "dir" {
			sb.WriteString(fmt.Sprintf("  %s/\n", e.Name))
		} else {
			sb.WriteString(fmt.Sprintf("  %s (%d bytes)\n", e.Name, e.Size))
		}
	}
	return sb.String(), nil
}

func getFile(ctx context.Context, owner, repo, path string) (string, error) {
	apiURL := fmt.Sprintf("https://api.github.com/repos/%s/%s/contents/%s", owner, repo, url.PathEscape(path))
	raw, err := githubGetRaw(ctx, apiURL, "application/vnd.github.raw+json")
	if err != nil {
		return "", err
	}
	return truncate(fmt.Sprintf("%s/%s/%s:\n\n%s", owner, repo, path, raw), 8000), nil
}

func getReadme(ctx context.Context, owner, repo string) (string, error) {
	apiURL := fmt.Sprintf("https://api.github.com/repos/%s/%s/readme", owner, repo)
	raw, err := githubGetRaw(ctx, apiURL, "application/vnd.github.raw+json")
	if err != nil {
		return "", err
	}
	return truncate(fmt.Sprintf("README of %s/%s:\n\n%s", owner, repo, raw), 8000), nil
}

func githubGet(ctx context.Context, apiURL, accept string, out any) error {
	raw, err := githubGetRaw(ctx, apiURL, accept)
	if err != nil {
		return err
	}
	return json.Unmarshal([]byte(raw), out)
}

func githubGetRaw(ctx context.Context, apiURL, accept string) (string, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, apiURL, nil)
	if err != nil {
		return "", err
	}
	// GitHub rejects requests without a User-Agent.
	req.Header.Set("User-Agent", "nasiko-github-agent")
	req.Header.Set("X-GitHub-Api-Version", "2022-11-28")
	if accept == "" {
		accept = "application/vnd.github+json"
	}
	req.Header.Set("Accept", accept)

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", err
	}

	switch {
	case resp.StatusCode == http.StatusNotFound:
		return "", fmt.Errorf("not found: %s", apiURL)
	case resp.StatusCode == http.StatusForbidden || resp.StatusCode == http.StatusTooManyRequests:
		if resp.Header.Get("X-RateLimit-Remaining") == "0" {
			return "", fmt.Errorf("GitHub rate limit exceeded (unauthenticated: 60 req/hour); resets %s", rateLimitReset(resp))
		}
		return "", fmt.Errorf("GitHub returned %d: %s", resp.StatusCode, firstLine(string(body)))
	case resp.StatusCode >= 400:
		return "", fmt.Errorf("GitHub returned %d: %s", resp.StatusCode, firstLine(string(body)))
	}
	return string(body), nil
}

func rateLimitReset(resp *http.Response) string {
	epoch, err := strconv.ParseInt(resp.Header.Get("X-RateLimit-Reset"), 10, 64)
	if err != nil {
		return "soon"
	}
	return time.Unix(epoch, 0).UTC().Format("15:04 UTC")
}

func firstLine(s string) string {
	if i := strings.IndexByte(s, '\n'); i >= 0 {
		return s[:i]
	}
	return s
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "\n\n[truncated]"
}

func extractText(msg *a2a.Message) string {
	if msg == nil {
		return ""
	}
	var parts []string
	for _, p := range msg.Parts {
		if t := p.Text(); t != "" {
			parts = append(parts, t)
		}
	}
	return strings.Join(parts, " ")
}

var port = flag.Int("port", 5004, "Port to listen on")

func main() {
	flag.Parse()
	addr := fmt.Sprintf("0.0.0.0:%d", *port)

	agentCard := &a2a.AgentCard{
		Name:        "GitHub Agent",
		Description: "An LLM assistant that answers GitHub questions with live data: searches repositories, fetches repo overviews and READMEs, and browses file trees via GitHub's public API (no API key required, rate-limited to 60 req/hour)",
		SupportedInterfaces: []*a2a.AgentInterface{
			a2a.NewAgentInterface(fmt.Sprintf("http://0.0.0.0:%d/a2a", *port), a2a.TransportProtocolJSONRPC),
		},
		DefaultInputModes:  []string{"text"},
		DefaultOutputModes: []string{"text"},
		Capabilities:       a2a.AgentCapabilities{Streaming: false},
		Skills: []a2a.AgentSkill{
			{
				ID:          "search_repos",
				Name:        "Search Repositories",
				Description: "Search GitHub repositories by keyword, sorted by stars",
				Tags:        []string{"github", "search", "repositories"},
				Examples:    []string{"search rust async runtime", "find the most popular a2a protocol repos"},
			},
			{
				ID:          "repo_info",
				Name:        "Repository Overview",
				Description: "Get stars, forks, language, license, and topics for a repository",
				Tags:        []string{"github", "repository", "stats"},
				Examples:    []string{"tell me about golang/go", "how many stars does tokio-rs/tokio have?"},
			},
			{
				ID:          "browse",
				Name:        "Browse Contents",
				Description: "List files in a repository directory, read a file, or fetch the README",
				Tags:        []string{"github", "files", "readme", "navigate"},
				Examples:    []string{"what's in golang/go/src/net?", "summarize the README of a2aproject/a2a-go"},
			},
		},
	}

	handler := a2asrv.NewHandler(&githubExecutor{llm: newLLMClient()})

	mux := http.NewServeMux()
	mux.Handle("/a2a", a2asrv.NewJSONRPCHandler(handler))
	mux.Handle(a2asrv.WellKnownAgentCardPath, a2asrv.NewStaticAgentCardHandler(agentCard))
	mux.HandleFunc("/health", func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte("ok"))
	})

	listener, err := net.Listen("tcp", addr)
	if err != nil {
		log.Fatalf("Failed to listen: %v", err)
	}
	log.Printf("GitHub Agent listening on %s", addr)
	log.Fatal(http.Serve(listener, mux))
}
