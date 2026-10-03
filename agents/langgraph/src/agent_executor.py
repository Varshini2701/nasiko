"""Deep Analyst executor — LangGraph agent on the 1.x A2A event-queue API."""

import logging

from a2a.helpers import (
    new_task_from_user_message,
    new_text_artifact_update_event,
    new_text_status_update_event,
)
from a2a.server.agent_execution import AgentExecutor, RequestContext
from a2a.server.events import EventQueue
from a2a.types import TaskState
from opentelemetry import context as otel_context, trace

from agent import DeepAnalystAgent
from telemetry import request_otel_context

logger = logging.getLogger(__name__)


class DeepAnalystAgentExecutor(AgentExecutor):
    def __init__(self):
        self.agent = DeepAnalystAgent()

    async def execute(self, context: RequestContext, event_queue: EventQueue) -> None:
        query = context.get_user_input()
        task = context.current_task or new_task_from_user_message(context.message)
        await event_queue.enqueue_event(task)

        # a2a-sdk runs execute() in a background asyncio task. The incoming W3C
        # traceparent is extracted by TraceparentMiddleware (in telemetry.py) into
        # request_otel_context, copied into this task at creation. Using it as the
        # explicit parent keeps each request on its own trace in Tempo.
        tracer = trace.get_tracer("deep-analyst")
        parent_ctx = request_otel_context.get() or otel_context.Context()
        with tracer.start_as_current_span("deep_analyst.request", context=parent_ctx) as span:
            span.set_attribute("session.id", task.context_id)

            try:
                async for item in self.agent.stream(query, task.context_id):
                    if not item["is_task_complete"] and not item["require_user_input"]:
                        await event_queue.enqueue_event(
                            new_text_status_update_event(
                                task_id=task.id,
                                context_id=task.context_id,
                                state=TaskState.TASK_STATE_WORKING,
                                text=item["content"],
                            )
                        )
                    elif item["require_user_input"]:
                        await event_queue.enqueue_event(
                            new_text_status_update_event(
                                task_id=task.id,
                                context_id=task.context_id,
                                state=TaskState.TASK_STATE_INPUT_REQUIRED,
                                text=item["content"],
                            )
                        )
                        break
                    else:
                        await event_queue.enqueue_event(
                            new_text_artifact_update_event(
                                task_id=task.id,
                                context_id=task.context_id,
                                name="analysis_result",
                                text=item["content"],
                            )
                        )
                        await event_queue.enqueue_event(
                            new_text_status_update_event(
                                task_id=task.id,
                                context_id=task.context_id,
                                state=TaskState.TASK_STATE_COMPLETED,
                                text=item["content"],
                            )
                        )
                        break
            except Exception as e:
                logger.error("Deep analyst error: %s", e)
                await event_queue.enqueue_event(
                    new_text_status_update_event(
                        task_id=task.id,
                        context_id=task.context_id,
                        state=TaskState.TASK_STATE_FAILED,
                        text=f"Error: {e}",
                    )
                )

    async def cancel(self, context: RequestContext, event_queue: EventQueue) -> None:
        pass
