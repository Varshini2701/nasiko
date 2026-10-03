package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"iter"
	"log"
	"net"
	"net/http"
	"net/url"
	"strings"

	"github.com/a2aproject/a2a-go/v2/a2a"
	"github.com/a2aproject/a2a-go/v2/a2asrv"
	// [nasiko:imports]
)

type booksExecutor struct {
	llm *llmClient
}

var _ a2asrv.AgentExecutor = (*booksExecutor)(nil)

func (b *booksExecutor) Execute(ctx context.Context, execCtx *a2asrv.ExecutorContext) iter.Seq2[a2a.Event, error] {
	// The inbound W3C trace context, forwarded on the LLM call for attribution.
	traceparent := firstParam(execCtx, "traceparent")
	return func(yield func(a2a.Event, error) bool) {
		userText := extractText(execCtx.Message)
		// Run the LLM tool-calling loop: the model forms the search query, we
		// execute it against Open Library, then the model synthesizes the answer.
		result, err := b.llm.runAgentLoop(ctx, userText, traceparent)
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

func (*booksExecutor) Cancel(ctx context.Context, execCtx *a2asrv.ExecutorContext) iter.Seq2[a2a.Event, error] {
	return func(yield func(a2a.Event, error) bool) {}
}

func searchBooks(ctx context.Context, query string) (string, error) {
	apiURL := fmt.Sprintf("https://openlibrary.org/search.json?q=%s&limit=5&fields=title,author_name,first_publish_year,subject,isbn,number_of_pages_median,ratings_average", url.QueryEscape(query))

	resp, err := httpGet(ctx, apiURL)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()

	var data struct {
		NumFound int `json:"numFound"`
		Docs     []struct {
			Title            string   `json:"title"`
			AuthorName       []string `json:"author_name"`
			FirstPublishYear int      `json:"first_publish_year"`
			Subject          []string `json:"subject"`
			ISBN             []string `json:"isbn"`
			Pages            int      `json:"number_of_pages_median"`
			Rating           float64  `json:"ratings_average"`
		} `json:"docs"`
	}

	if err := json.NewDecoder(resp.Body).Decode(&data); err != nil {
		return "", err
	}

	if data.NumFound == 0 {
		return fmt.Sprintf("No books found for %q. Try a different search term.", query), nil
	}

	var sb strings.Builder
	sb.WriteString(fmt.Sprintf("Found %d books for %q (showing top %d):\n\n", data.NumFound, query, len(data.Docs)))

	for i, doc := range data.Docs {
		authors := "Unknown"
		if len(doc.AuthorName) > 0 {
			authors = strings.Join(doc.AuthorName, ", ")
		}
		sb.WriteString(fmt.Sprintf("%d. %s\n", i+1, doc.Title))
		sb.WriteString(fmt.Sprintf("   Author(s): %s\n", authors))
		if doc.FirstPublishYear > 0 {
			sb.WriteString(fmt.Sprintf("   First published: %d\n", doc.FirstPublishYear))
		}
		if doc.Pages > 0 {
			sb.WriteString(fmt.Sprintf("   Pages: %d\n", doc.Pages))
		}
		if doc.Rating > 0 {
			sb.WriteString(fmt.Sprintf("   Rating: %.1f/5\n", doc.Rating))
		}
		if len(doc.Subject) > 0 {
			subjects := doc.Subject
			if len(subjects) > 3 {
				subjects = subjects[:3]
			}
			sb.WriteString(fmt.Sprintf("   Subjects: %s\n", strings.Join(subjects, ", ")))
		}
		sb.WriteString("\n")
	}
	return sb.String(), nil
}

// httpGet issues a GET with the request context attached, so the
// loongsuite-instrumented transport (see Dockerfile) propagates the trace and
// the call shows up as a span.
func httpGet(ctx context.Context, url string) (*http.Response, error) {
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return nil, err
	}
	return http.DefaultClient.Do(req)
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

var port = flag.Int("port", 5002, "Port to listen on")

func main() {
	flag.Parse()
	addr := fmt.Sprintf("0.0.0.0:%d", *port)

	agentCard := &a2a.AgentCard{
		Name:        "Books Agent",
		Description: "An LLM-powered book assistant that searches Open Library for real book data — authors, publish dates, ratings, and subjects — and recommends or answers from the results (no API key required)",
		SupportedInterfaces: []*a2a.AgentInterface{
			a2a.NewAgentInterface(fmt.Sprintf("http://0.0.0.0:%d/a2a", *port), a2a.TransportProtocolJSONRPC),
		},
		DefaultInputModes:  []string{"text"},
		DefaultOutputModes: []string{"text"},
		Capabilities:       a2a.AgentCapabilities{Streaming: false},
		Skills: []a2a.AgentSkill{
			{
				ID:          "search_books",
				Name:        "Search Books",
				Description: "Search Open Library by title, author, or topic and get real book details: authors, first publish year, page count, rating, and subjects",
				Tags:        []string{"books", "search", "library", "reading"},
				Examples:    []string{"Search for books about machine learning", "Find books by Isaac Asimov", "Books about the history of Rome"},
			},
			{
				ID:          "recommend_books",
				Name:        "Recommend Books",
				Description: "Recommend books for a topic, genre, or mood, backed by real Open Library search results",
				Tags:        []string{"books", "recommendation", "reading"},
				Examples:    []string{"Recommend some sci-fi novels for beginners", "What are good highly-rated books on Stoicism?", "Suggest a short classic novel"},
			},
		},
	}

	handler := a2asrv.NewHandler(&booksExecutor{llm: newLLMClient()})

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
	log.Printf("Books Agent listening on %s", addr)
	log.Fatal(http.Serve(listener, mux))
}
