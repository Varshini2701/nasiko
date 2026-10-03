/**
 * Resolves `Query(...)` statements against a real toolProvider, fires
 * `Mutation(...)`s, and runs `Action([...])` step sequences — all against
 * the REAL, already-existing `ui/common/core/data-sources.js` (see
 * poc.html), never a backend proxy. weave2.0's Python backend never sees
 * real data or performs a real write; it only ever knew a data source's
 * *name*.
 */

/**
 * Walks a dot-path ("a.b.c") into `obj`. Returns `obj` itself unchanged if
 * `path` is falsy. Never throws — a missing segment anywhere along the way
 * yields `undefined`, same as `defaults` would have shown.
 */
export function getPath(obj, path) {
  if (!path) return obj;
  return path.split('.').reduce((acc, key) => (acc == null ? undefined : acc[key]), obj);
}

/** Bounded retry budget for a permanently-failing query — without this, a
 * query whose source always errors (bad args, backend down) refires on
 * every single re-render forever, since a failure was never cached as
 * "settled" before this fix (confirmed live: 6,591 real requests in under a
 * minute for one broken query). Backoff delays are indexed by attempt
 * number; the last entry repeats for any attempt beyond its length. */
const MAX_QUERY_ATTEMPTS = 4;
const RETRY_BACKOFF_MS = [0, 500, 1500, 4000];

/**
 * For each query not already cached or in flight: marks it in flight, calls
 * `toolProvider(toolName, args)`, applies `getPath` if a `path` was given,
 * caches the resolved value, and calls `onSettled()` once settled (success
 * or failure) so the caller can re-render with the real value in place.
 *
 * A failing query is retried with backoff up to `MAX_QUERY_ATTEMPTS` times,
 * tracked per `statementId` in `failures`, then left permanently unresolved
 * (the caller's declared default keeps showing, no further requests fire).
 * `@Run`ning the same statement again resets its budget — see
 * `invalidateQueries` in poc.html, which must clear `failures` alongside
 * `cache` or a re-triggered fetch could be silently skipped as "given up".
 *
 * @param {Array<{statementId, toolName, args, path}>} queries
 * @param {Map<string, any>} cache statementId -> resolved value
 * @param {Set<string>} inFlight
 * @param {(toolName: string, args: any[]) => Promise<any>} toolProvider
 * @param {() => void} onSettled
 * @param {Map<string, number>} [failures] statementId -> attempt count
 */
export function resolvePendingQueries(queries, cache, inFlight, toolProvider, onSettled, failures = new Map()) {
  for (const q of queries) {
    if (cache.has(q.statementId) || inFlight.has(q.statementId)) continue;
    const attempt = failures.get(q.statementId) || 0;
    if (attempt >= MAX_QUERY_ATTEMPTS) continue;
    inFlight.add(q.statementId);
    const run = () => {
      toolProvider(q.toolName, q.args)
        .then((result) => {
          cache.set(q.statementId, getPath(result, q.path));
          failures.delete(q.statementId);
        })
        .catch((err) => {
          const next = attempt + 1;
          failures.set(q.statementId, next);
          const label = next >= MAX_QUERY_ATTEMPTS ? 'giving up' : `will retry (${next}/${MAX_QUERY_ATTEMPTS})`;
          console.warn(`query-manager: query "${q.toolName}" failed for ${q.statementId} — ${label}:`, err);
        })
        .finally(() => {
          inFlight.delete(q.statementId);
          onSettled();
        });
    };
    const delay = RETRY_BACKOFF_MS[Math.min(attempt, RETRY_BACKOFF_MS.length - 1)];
    if (delay > 0) setTimeout(run, delay);
    else run();
  }
}

/**
 * Fires one `Mutation(...)` for real. Guards against double-submit (the
 * same, only concurrency guard OpenUI itself has): if the mutation is
 * already `loading`, returns `false` immediately without starting a second
 * call. No confirmation step (deliberate — see the production plan §0.2;
 * revisit as a v2 decision, not built here).
 * @param {string} statementId
 * @param {{sourceName: string, args: any[]}} def
 * @param {Map<string, {status, data, error}>} mutationResults
 * @param {(sourceName: string, args: any[]) => Promise<any>} toolProvider
 * @param {() => void} onSettled called after every state transition (loading, then success/error)
 * @returns {Promise<boolean>} true if the mutation ran to completion (success OR error — i.e. wasn't
 *   rejected for being already in flight); false only on the double-submit guard.
 */
export async function fireMutation(statementId, def, mutationResults, toolProvider, onSettled) {
  const current = mutationResults.get(statementId);
  if (current && current.status === 'loading') return false;

  mutationResults.set(statementId, { status: 'loading', data: null, error: null });
  onSettled();
  try {
    const data = await toolProvider(def.sourceName, def.args);
    mutationResults.set(statementId, { status: 'success', data, error: null });
    onSettled();
    return true;
  } catch (err) {
    console.warn(`query-manager: mutation "${def.sourceName}" failed for ${statementId}:`, err);
    mutationResults.set(statementId, { status: 'error', data: null, error: String(err) });
    onSettled();
    return true; // ran to completion (with an error result), not rejected by the guard
  }
}

/**
 * Runs one `Action([...])`'s steps sequentially, awaiting each — halts the
 * remaining steps if a `@Run` of a Mutation fails (matches OpenUI's real
 * halt-on-mutation-failure rule). `@Run` of a Query is fire-and-forget
 * invalidation (cannot halt the chain). `@Set`/`@Reset` are synchronous
 * store writes. `@ToAssistant` sends a real message through the SAME path
 * a typed user message already takes (never a second, parallel path) and
 * is rate-limited by `deps.isAssistantBusy()`. `@OpenUrl` is closed by
 * default — it only navigates if `deps.openUrlAllowlist` is supplied AND
 * returns true for that URL; otherwise it logs a warning and does nothing.
 *
 * @param {{type:'action', steps: Array}} action materialized Action value
 * @param {object} evalCtx the current evaluation context (symbols/store/etc.)
 *   used to evaluate each step's raw AST at fire-time, not materialize-time
 * @param {object} deps
 * @param {(argsAst: object, evalCtx: object) => any} deps.evaluate
 * @param {Map<string, {sourceName, argsAst}>} deps.mutationDefs
 * @param {Map<string, {status, data, error}>} deps.mutationResults
 * @param {(sourceName: string, args: any[]) => Promise<any>} deps.toolProvider
 * @param {(statementIds: string[]) => void} deps.invalidateQueries
 * @param {ReturnType<typeof import('./store.js').createStore>} deps.store
 * @param {() => void} deps.onSettled
 * @param {(message: string) => Promise<void>} [deps.sendToAssistant]
 * @param {() => boolean} [deps.isAssistantBusy]
 * @param {(url: string) => boolean} [deps.openUrlAllowlist]
 */
export async function triggerAction(action, evalCtx, deps) {
  for (const step of action.steps) {
    if (step.kind === 'run') {
      if (step.refType === 'mutation') {
        const def = deps.mutationDefs.get(step.statementId);
        if (!def) return; // unknown mutation ref — halt, nothing to run
        const args = deps.evaluate(def.argsAst, evalCtx);
        const ok = await fireMutation(
          step.statementId,
          { sourceName: def.sourceName, args: Array.isArray(args) ? args : [] },
          deps.mutationResults, deps.toolProvider, deps.onSettled,
        );
        const result = deps.mutationResults.get(step.statementId);
        if (!ok || (result && result.status === 'error')) return; // halt on mutation failure
      } else {
        deps.invalidateQueries([step.statementId]); // fire-and-forget, cannot halt the chain
      }
    } else if (step.kind === 'set') {
      const value = deps.evaluate(step.valueAst, evalCtx);
      deps.store.set(step.target, value);
    } else if (step.kind === 'reset') {
      for (const target of step.targets) deps.store.set(target, null);
    } else if (step.kind === 'toAssistant') {
      if (deps.isAssistantBusy && deps.isAssistantBusy()) {
        console.warn('query-manager: @ToAssistant ignored — a turn is already in flight');
        continue;
      }
      const message = String(deps.evaluate(step.messageAst, evalCtx) ?? '');
      if (deps.sendToAssistant) await deps.sendToAssistant(message);
    } else if (step.kind === 'openUrl') {
      const url = String(deps.evaluate(step.urlAst, evalCtx) ?? '');
      if (deps.openUrlAllowlist && deps.openUrlAllowlist(url)) {
        window.open(url, '_blank');
      } else {
        console.warn(`query-manager: @OpenUrl("${url}") blocked — no real route allowlist configured yet`);
      }
    }
  }
}
