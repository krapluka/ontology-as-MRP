"""Haystack `generate` pipeline (Hayhooks): an OpenAI-compatible chat stream.

Served on `POST /v1/chat/completions` with `model: "generate"`. The engine streams the
answer and enforces its own time bounds, retries and failure classes; this pipeline stays
transparent:

- the provider client never retries (`max_retries=0`), and its timeout is at or above the
  engine's total cap;
- a provider failure before any part of the answer arrives is answered as an HTTP error
  carrying the provider's status, `Retry-After` and error object (`provider_errors`);
- a failure after the stream began, or a provider stream that ends without a finish
  reason, closes the stream without a finish marker, so the engine never accepts a
  truncated answer;
- the final chunk carries the call's token usage.

The chat model is runtime configuration (`CHAT_MODEL`), never chosen by the engine. The
generated text is a proposal input only; grounding and governance stay in the engine.
"""

import asyncio
import os
import time
import uuid
from collections.abc import AsyncGenerator
from dataclasses import dataclass

from fastapi_openai_compat import ChatCompletion
from haystack.components.generators.chat import OpenAIChatGenerator
from haystack.dataclasses import ChatMessage, StreamingChunk
from haystack.tracing import tracer
from haystack.utils import Secret
from hayhooks import BasePipelineWrapper
from provider_errors import PROVIDER_TIMEOUT_SECS, to_http_error

_BASE = os.getenv("MODEL_BASE_URL", "https://openrouter.ai/api/v1")
_MODEL = os.getenv("CHAT_MODEL", "openai/gpt-oss-120b")
_PIPELINE = "generate"


@dataclass(frozen=True)
class _Finish:
    """The finish marker: an empty delta carrying the provider's finish reason."""

    finish_reason: str
    content: str = ""


class _Done:
    """Queue sentinel: the provider call has returned or raised."""


class PipelineWrapper(BasePipelineWrapper):
    def setup(self) -> None:
        self.generator = OpenAIChatGenerator(
            api_key=Secret.from_env_var("MODEL_API_KEY"),
            api_base_url=_BASE,
            model=_MODEL,
            timeout=PROVIDER_TIMEOUT_SECS,
            max_retries=0,
            generation_kwargs={"stream_options": {"include_usage": True}},
        )

    async def run_chat_completion_async(self, model: str, messages: list[dict], body: dict) -> AsyncGenerator:
        # The async client is created on the serving event loop (Haystack 3 lifecycle).
        await self.generator.warm_up_async()
        queue: asyncio.Queue = asyncio.Queue()

        async def on_chunk(chunk: StreamingChunk) -> None:
            await queue.put(chunk)

        task = asyncio.create_task(
            self.generator.run_async(
                messages=[ChatMessage.from_openai_dict_format(m) for m in messages],
                streaming_callback=on_chunk,
            )
        )
        task.add_done_callback(lambda _: queue.put_nowait(_Done()))

        # Wait for the provider's first event before answering, so a rate limit or an
        # unreachable provider becomes an HTTP error instead of an empty successful stream.
        first = await queue.get()
        if isinstance(first, _Done):
            if task.exception() is not None:
                raise to_http_error(task.exception())
            raise to_http_error(RuntimeError("the provider returned no stream"))
        return self._stream(first, queue, task)

    async def _stream(self, first: StreamingChunk, queue: asyncio.Queue, task: asyncio.Task) -> AsyncGenerator:
        try:
            finish_reason = None
            item = first
            while not isinstance(item, _Done):
                if item.content:
                    yield item.content
                if item.finish_reason:
                    finish_reason = item.finish_reason
                item = await queue.get()
            if task.exception() is not None:
                # Mid-stream failure: abort the response; no finish marker is sent.
                raise task.exception()
            if finish_reason is None:
                raise RuntimeError("the provider stream ended without a finish reason")
            yield _Finish(finish_reason=finish_reason)
            usage = task.result()["replies"][0].meta.get("usage") or {}
            _tag_span(usage)
            yield ChatCompletion(
                id=f"{_PIPELINE}-{uuid.uuid4()}",
                object="chat.completion.chunk",
                created=int(time.time()),
                model=_PIPELINE,
                choices=[],
                usage={
                    key: usage.get(key, 0) for key in ("prompt_tokens", "completion_tokens", "total_tokens")
                },
            )
        finally:
            # The engine dropped the call (its time bound, or its caller went away): stop
            # the provider stream instead of spending tokens on an answer nobody reads.
            if not task.done():
                task.cancel()


def _tag_span(usage: dict) -> None:
    """Record the model and token counts on the current span (no prompt or answer text)."""
    span = tracer.current_span()
    if span is None:
        return
    span.set_tag("gen_ai.request.model", _MODEL)
    span.set_tag("gen_ai.usage.input_tokens", usage.get("prompt_tokens", 0))
    span.set_tag("gen_ai.usage.output_tokens", usage.get("completion_tokens", 0))
