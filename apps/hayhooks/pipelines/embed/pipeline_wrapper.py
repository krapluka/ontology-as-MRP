"""Haystack `embed` pipeline (Hayhooks), provider-neutral via an OpenAI-compatible API.

request : { "texts": [str, ...] }
response: { "vectors": [[float, ...], ...], "dimension": int }

The vector dimension must match the engine's pinned embedding dimension; change the model
and that setting together. Like `generate`, the provider client never retries and a
provider failure reaches the engine with its own status (`provider_errors`).
"""

import os

from haystack.components.embedders import OpenAITextEmbedder
from haystack.utils import Secret
from hayhooks import BasePipelineWrapper
from provider_errors import PROVIDER_TIMEOUT_SECS, to_http_error

_BASE = os.getenv("EMBED_BASE_URL") or os.getenv("MODEL_BASE_URL", "https://openrouter.ai/api/v1")
_MODEL = os.getenv("EMBED_MODEL", "qwen/qwen3-embedding-8b")
_KEY_VAR = "EMBED_API_KEY" if os.getenv("EMBED_API_KEY") else "MODEL_API_KEY"


class PipelineWrapper(BasePipelineWrapper):
    def setup(self) -> None:
        self.embedder = OpenAITextEmbedder(
            api_key=Secret.from_env_var(_KEY_VAR),
            api_base_url=_BASE,
            model=_MODEL,
            timeout=PROVIDER_TIMEOUT_SECS,
            max_retries=0,
        )

    async def run_api_async(self, texts: list[str]) -> dict:
        """Embed a batch of texts into fixed-dimension vectors."""
        await self.embedder.warm_up_async()
        try:
            vectors = [(await self.embedder.run_async(text=text))["embedding"] for text in texts]
        except Exception as err:
            raise to_http_error(err) from err
        return {"vectors": vectors, "dimension": len(vectors[0]) if vectors else 0}
