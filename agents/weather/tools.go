package main

// tools.go — the agent's tools, exposed to the LLM as OpenAI function definitions.
//
// Each tool is a plain Go function; `dispatchTool` routes the model's tool_call
// (name + JSON arguments) to it. The geocoding and weather HTTP calls go through
// the package's shared client so the loongsuite-instrumented transport propagates
// trace context (and they show up as spans).

import (
	"context"
	"encoding/json"
	"fmt"
)

const maxToolRounds = 6

const systemPrompt = `You are a weather assistant. Use your tools — never guess conditions.
For any weather question, first call geocode to resolve the location, then call
get_weather for the conditions. Answer concisely with the actual data the tools return.`

// weatherTools is the tool schema advertised to the model.
var weatherTools = []toolDef{
	{
		Type: "function",
		Function: struct {
			Name        string         `json:"name"`
			Description string         `json:"description"`
			Parameters  map[string]any `json:"parameters"`
		}{
			Name:        "geocode",
			Description: "Resolve a place name (city, region, landmark) to coordinates and a canonical name.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"location": map[string]any{"type": "string", "description": "Place name, e.g. 'Paris' or 'Tokyo, Japan'"},
				},
				"required": []string{"location"},
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
			Name:        "get_weather",
			Description: "Get current conditions and a 3-day forecast for coordinates from geocode.",
			Parameters: map[string]any{
				"type": "object",
				"properties": map[string]any{
					"latitude":  map[string]any{"type": "number", "description": "Latitude"},
					"longitude": map[string]any{"type": "number", "description": "Longitude"},
					"label":     map[string]any{"type": "string", "description": "Human-readable place name from geocode"},
				},
				"required": []string{"latitude", "longitude"},
			},
		},
	},
}

// dispatchTool runs one model-requested tool and returns its text result.
// Tool errors are returned as content (not Go errors) so the model can recover.
func dispatchTool(ctx context.Context, name, argsJSON string) string {
	switch name {
	case "geocode":
		var args struct {
			Location string `json:"location"`
		}
		if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
			return fmt.Sprintf("bad arguments: %v", err)
		}
		lat, lon, label, err := geocode(ctx, args.Location)
		if err != nil {
			return fmt.Sprintf("geocode failed for %q: %v", args.Location, err)
		}
		out, _ := json.Marshal(map[string]any{"latitude": lat, "longitude": lon, "label": label})
		return string(out)

	case "get_weather":
		var args struct {
			Latitude  float64 `json:"latitude"`
			Longitude float64 `json:"longitude"`
			Label     string  `json:"label"`
		}
		if err := json.Unmarshal([]byte(argsJSON), &args); err != nil {
			return fmt.Sprintf("bad arguments: %v", err)
		}
		label := args.Label
		if label == "" {
			label = fmt.Sprintf("%.3f,%.3f", args.Latitude, args.Longitude)
		}
		report, err := getWeather(ctx, args.Latitude, args.Longitude, label)
		if err != nil {
			return fmt.Sprintf("get_weather failed: %v", err)
		}
		return report

	default:
		return fmt.Sprintf("unknown tool: %s", name)
	}
}
