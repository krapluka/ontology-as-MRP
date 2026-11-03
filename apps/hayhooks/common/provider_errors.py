"""Provider-failure transparency shared by the pipelines.

The engine owns every retry, time bound and failure class. So the runtime never retries a
provider call itself; when the provider fails, it answers the engine with the provider's
own HTTP status, its `Retry-After` / `X-RateLimit-Reset`, and its error object in the
OpenAI shape (`{"error": {...}}`, carried as the FastAPI `detail`). The engine unwraps the
`detail` and classifies the failure the same way as on the direct route, including an
`insufficient_quota` 429 that must not be retried.

A provider that cannot be reached, or does not answer within the runtime's own limit, is
answered as 503.
"""

import os

import openai
from fastapi import HTTPException

# The runtime's own limit for one provider call. It must stay at or above the engine's
# total cap (120 s by default), so the engine, not the runtime, decides when a call has
# taken too long.
PROVIDER_TIMEOUT_SECS = float(os.getenv("PROVIDER_TIMEOUT_SECS", "150"))

# Rate-limit hints the engine honours, passed through unchanged.
_PASSED_HEADERS = ("retry-after", "x-ratelimit-reset")


def _error_object(err: openai.APIError) -> dict:
    """The provider's error object, or one built from the exception's message."""
    body = err.body
    if isinstance(body, dict):
        inner = body.get("error", body)
        if isinstance(inner, dict) and inner:
            return inner
    return {"message": err.message}


def to_http_error(err: BaseException) -> BaseException:
    """Map a provider failure to the HTTP error the engine should see.

    Anything that is not a provider failure is returned unchanged, so Hayhooks reports it
    as its usual 500.
    """
    if isinstance(err, (openai.APITimeoutError, openai.APIConnectionError)):
        return HTTPException(
            status_code=503,
            detail={"error": {"message": f"provider unavailable: {err.message}", "type": "unavailable"}},
        )
    if isinstance(err, openai.APIStatusError):
        headers = {name: err.response.headers[name] for name in _PASSED_HEADERS if name in err.response.headers}
        return HTTPException(status_code=err.status_code, detail={"error": _error_object(err)}, headers=headers)
    if isinstance(err, openai.APIError):
        # An error the provider streamed inside a 200 (some gateways report an upstream
        # rate limit this way). Its numeric `code`, when it is an HTTP status, keeps the class.
        error = _error_object(err)
        code = error.get("code")
        status = int(code) if str(code).isdigit() and 400 <= int(code) <= 599 else 503
        return HTTPException(status_code=status, detail={"error": error})
    return err
