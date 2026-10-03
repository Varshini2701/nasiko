package main

// llm.go — a minimal OpenAI-compatible tool-calling loop.
//
// This is the reference pattern for Go agents on the platform:
//   - The LLM base URL + API key come from the environment. When deployed through the
//     control plane, OPENAI_BASE_URL points at the platform's LLM gateway and
//     OPENAI_API_KEY is the agent-identity token minted at deploy — so calls are
//     metered and attributed. Locally, set them to any OpenAI-compatible endpoint.
//   - The inbound `traceparent` (from the A2A request) is forwarded on the LLM call so
//     the gateway can attribute tokens to the originating user. a2a-go's handler copies
//     HTTP headers into ExecutorContext.ServiceParams; we extract it in main.go and
//     thread it here. (When built with the loongsuite `otel go build` in the Dockerfile,
//     net/http is auto-instrumented and propagation needs no manual work — this manual
//     header is the no-instrumentation fallback and is harmless when both are present.)

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
)

// ── OpenAI wire types (chat.completions) ─────────────────────────────────────

type chatMessage struct {
	Role       string     `json:"role"`
	Content    string     `json:"content,omitempty"`
	ToolCalls  []toolCall `json:"tool_calls,omitempty"`
	ToolCallID string     `json:"tool_call_id,omitempty"`
	Name       string     `json:"name,omitempty"`
}

type toolCall struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"` // JSON-encoded
	} `json:"function"`
}

type toolDef struct {
	Type     string `json:"type"`
	Function struct {
		Name        string         `json:"name"`
		Description string         `json:"description"`
		Parameters  map[string]any `json:"parameters"`
	} `json:"function"`
}

type chatRequest struct {
	Model    string        `json:"model"`
	Messages []chatMessage `json:"messages"`
	Tools    []toolDef     `json:"tools,omitempty"`
	// DeepSeek's thinking mode rejects tool_choice; the platform agents disable it.
	ExtraBody map[string]any `json:"-"`
}

type chatResponse struct {
	Choices []struct {
		Message      chatMessage `json:"message"`
		FinishReason string      `json:"finish_reason"`
	} `json:"choices"`
}

// ── Client ───────────────────────────────────────────────────────────────────

type llmClient struct {
	baseURL string
	apiKey  string
	model   string
	http    *http.Client
}

func newLLMClient() *llmClient {
	base := os.Getenv("OPENAI_BASE_URL")
	if base == "" {
		base = "https://api.openai.com/v1"
	}
	model := os.Getenv("OPENAI_MODEL")
	if model == "" {
		model = os.Getenv("MODEL")
	}
	if model == "" {
		model = "gpt-4o-mini"
	}
	return &llmClient{
		baseURL: base,
		apiKey:  os.Getenv("OPENAI_API_KEY"),
		model:   model,
		http:    &http.Client{},
	}
}

// chat sends one round to the LLM. traceparent is the inbound W3C trace context
// header value (may be empty); forwarding it lets the gateway attribute the call.
func (c *llmClient) chat(ctx context.Context, messages []chatMessage, tools []toolDef, traceparent string) (*chatMessage, error) {
	reqBody := chatRequest{Model: c.model, Messages: messages, Tools: tools}
	payload, err := json.Marshal(reqBody)
	if err != nil {
		return nil, err
	}

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, c.baseURL+"/chat/completions", bytes.NewReader(payload))
	if err != nil {
		return nil, err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+c.apiKey)
	if traceparent != "" {
		req.Header.Set("traceparent", traceparent)
	}

	resp, err := c.http.Do(req)
	if err != nil {
		return nil, err
	}
	defer resp.Body.Close()

	body, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		return nil, fmt.Errorf("LLM API %d: %s", resp.StatusCode, string(body))
	}

	var out chatResponse
	if err := json.Unmarshal(body, &out); err != nil {
		return nil, fmt.Errorf("LLM response parse: %w", err)
	}
	if len(out.Choices) == 0 {
		return nil, fmt.Errorf("LLM returned no choices")
	}
	return &out.Choices[0].Message, nil
}

// runAgentLoop is the ReAct loop: the model decides which tool to call, we
// execute it, feed the result back, and repeat until the model answers with
// content instead of a tool call.
func (c *llmClient) runAgentLoop(ctx context.Context, query, traceparent string) (string, error) {
	messages := []chatMessage{
		{Role: "system", Content: systemPrompt},
		{Role: "user", Content: query},
	}

	for range maxToolRounds {
		msg, err := c.chat(ctx, messages, booksTools, traceparent)
		if err != nil {
			return "", err
		}

		// No tool calls → the model produced the final answer.
		if len(msg.ToolCalls) == 0 {
			if msg.Content == "" {
				return "", fmt.Errorf("model returned an empty reply")
			}
			return msg.Content, nil
		}

		// Record the assistant turn (with its tool calls), then run each tool.
		messages = append(messages, *msg)
		for _, tc := range msg.ToolCalls {
			result := dispatchTool(ctx, tc.Function.Name, tc.Function.Arguments)
			messages = append(messages, chatMessage{
				Role:       "tool",
				ToolCallID: tc.ID,
				Name:       tc.Function.Name,
				Content:    result,
			})
		}
	}
	return "", fmt.Errorf("exceeded %d tool rounds", maxToolRounds)
}
