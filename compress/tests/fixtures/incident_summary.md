# Incident 2026-09-16: agent-router OOMKilled

One replica of `agent-router` entered CrashLoopBackOff at 11:02 UTC after the container
exceeded its 512Mi memory limit. The remaining 119 replicas were unaffected and no user
requests failed, because the readiness probe removed the pod from the Service before the
kill.

## Cause

A single A2A flow accumulated tool results without bound. The flow guard
caps depth and fan-out but does not cap the aggregate size of stored tool output.

## Action

Raise the limit to 768Mi as a stopgap, and bound tool-result size at the point
where results are stored rather than where they are sent.
