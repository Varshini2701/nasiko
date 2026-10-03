/**
 * Hand-written canned DSL strings for Milestone A (no LLM involved yet).
 * Each exercises specific grammar/materialization behavior called out in the
 * approved plan's build order.
 */

/**
 * Exercises: forward references (root references `kpis`/`table` before they're
 * defined; `table` references `tokenopsRows` before it's defined), array-of-object
 * literal data for app-table, and all 4 POC components.
 */
export const BASIC = `root = AppStack([kpis, table], "md")
kpis = AppRow([kpiCost, kpiOps], "md")
kpiCost = AppStatCard("Total cost", "$12,450.00", "+8.2%", "up")
kpiOps = AppStatCard("Operations", "342", "+12", "neutral")
table = AppTable(tokenopsRows, 10, "pages", true)
tokenopsRows = [{model: "gpt-4o", cost: 120.5, tokens: 45000}, {model: "claude-3-5", cost: 88.2, tokens: 39000}, {model: "gpt-4o-mini", cost: 14.1, tokens: 61000}]
`;

/**
 * Appended after BASIC has fully streamed — re-uses the `kpiCost` identifier
 * with new args. Should REPLACE the existing stat card, not add a second one.
 */
export const REVISION_PATCH = `kpiCost = AppStatCard("Total cost", "$15,000.00", "+15.2%", "up")
`;

/**
 * Exercises: app-row's `wrap`/`justify` boolean+enum attributes, app-stat-card's
 * `loading` boolean, and a nested object literal with more than 2 keys.
 */
export const WITH_OPTIONS = `root = AppRow([loadingCard, statCard], "lg", "stretch", "between", null, true)
loadingCard = AppStatCard(null, null, null, null, true)
statCard = AppStatCard("Active agents", "12", "flat", "neutral")
`;

/**
 * Exercises a `Query(...)` statement referenced from a component argument —
 * offline/Node-testable: with an empty queryResults cache, `kpi`'s value
 * should be the default `0`; once the caller simulates
 * `cache.set('totalCostQ', 15000)` and re-materializes, it should be `15000`.
 * Uses a real registered source name (fetchUsageSummary) for realism, even
 * though this fixture never actually calls it.
 */
export const QUERY_DEMO = `root = AppStack([kpi], "md")
totalCostQ = Query("fetchUsageSummary", [], 0, "total_cost_usd")
kpi = AppStatCard("Total cost", totalCostQ, "up")
`;

export const FIXTURES = { BASIC, REVISION_PATCH, WITH_OPTIONS, QUERY_DEMO };
