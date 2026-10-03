import logging
import os

from dotenv import load_dotenv

load_dotenv()
logging.basicConfig(level=logging.INFO)

# Instrumentation must initialize before a2a-sdk (and anything it imports,
# e.g. Starlette) is imported below: OTel's Starlette instrumentor patches by
# rebinding `starlette.applications.Starlette` to an instrumented subclass, so
# any module that already did `from starlette.applications import Starlette`
# keeps its original, un-instrumented reference forever — no incoming
# traceparent gets extracted, and every request starts an orphan root trace
# instead of joining the platform's session trace.
from telemetry import TraceparentMiddleware, init_telemetry

init_telemetry(os.environ.get("OTEL_SERVICE_NAME", "deep-analyst"))

import click
import uvicorn
from a2a.server.request_handlers import DefaultRequestHandler
from a2a.server.routes import create_agent_card_routes, create_jsonrpc_routes
from a2a.server.tasks import InMemoryTaskStore
from a2a.types import AgentCapabilities, AgentCard, AgentInterface, AgentSkill
from starlette.applications import Starlette

from agent import DeepAnalystAgent
from agent_executor import DeepAnalystAgentExecutor

logger = logging.getLogger(__name__)


@click.command()
@click.option("--host", default="0.0.0.0")
@click.option("--port", default=int(os.environ.get("PORT", "8000")), type=int)
def main(host: str, port: int):
    """Starts the Deep Analyst Agent server (A2A 1.0)."""
    agent_url = os.getenv("HOST_OVERRIDE", f"http://{host}:{port}/")

    agent_card = AgentCard(
        name="Deep Analyst Agent",
        description="Stateful multi-step reasoning agent with tool access for deep analysis. Uses web search and exchange rate data.",
        supported_interfaces=[
            AgentInterface(
                protocol_binding="JSONRPC",
                url=agent_url,
            )
        ],
        version="1.0.0",
        default_input_modes=DeepAnalystAgent.SUPPORTED_CONTENT_TYPES,
        default_output_modes=DeepAnalystAgent.SUPPORTED_CONTENT_TYPES,
        capabilities=AgentCapabilities(streaming=True),
        skills=[
            AgentSkill(
                id="deep_analysis",
                name="Deep Analysis",
                description="Multi-step reasoning with web search and financial data tools for thorough analysis.",
                tags=["analysis", "reasoning", "research", "finance"],
                examples=[
                    "What's the current USD to EUR rate and how has it trended?",
                    "Analyze the impact of recent AI developments on the tech market",
                ],
            )
        ],
    )

    handler = DefaultRequestHandler(
        agent_executor=DeepAnalystAgentExecutor(),
        task_store=InMemoryTaskStore(),
        agent_card=agent_card,
    )

    routes = create_agent_card_routes(agent_card) + create_jsonrpc_routes(handler, rpc_url="/")
    app = Starlette(routes=routes)
    # Wrap with our ASGI middleware AFTER creating the Starlette app so it runs
    # first and populates request_otel_context for each incoming HTTP request.
    app = TraceparentMiddleware(app)

    logger.info("Deep Analyst Agent listening on %s:%s", host, port)
    uvicorn.run(app, host=host, port=port)


if __name__ == "__main__":
    main()
