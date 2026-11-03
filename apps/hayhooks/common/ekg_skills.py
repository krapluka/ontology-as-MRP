"""The runtime's skill store.

Skills are authored in git, versioned and activated in the engine's durable registry, and
served by the engine's tool gateway: the runtime holds no skill content of its own. An agent
backs Haystack's `SkillToolset` with an `EngineSkillStore` built per run from the gateway URL
and the run's task token, next to the `MCPToolset` it reads the world model through:

    toolset = SkillToolset(EngineSkillStore(gateway_url, token))

The gateway serves only the skill versions the token pins, so a version switched while the
run is going never changes what the run loads. The token is per run: it is never serialized,
so `from_dict` needs it supplied again.
"""

import os
from typing import Any
from urllib.parse import quote

import httpx
from haystack.core.serialization import generate_qualified_class_name
from haystack.dataclasses.skill_info import SkillInfo

# How long one skill request may take. Skills are small and served from the engine's cache.
SKILLS_TIMEOUT_SECS = float(os.getenv("EKG_SKILLS_TIMEOUT_SECS", "10"))


class EngineSkillStore:
    """A Haystack `SkillStore` that reads a task's pinned skills from the engine's gateway."""

    def __init__(self, url: str, token: str) -> None:
        """
        :param url: The tool gateway's base URL (e.g. `http://ekg-backend:8091`).
        :param token: The run's task token; the gateway serves the skills it pins.
        :raises ValueError: Without a token: the engine serves skills to a task only.
        """
        if not token:
            raise ValueError("EngineSkillStore needs the run's task token: the engine serves skills to a task only")
        self.url = url.rstrip("/")
        self._token = token

    def _get(self, path: str) -> httpx.Response:
        try:
            return httpx.get(
                f"{self.url}{path}",
                headers={"Authorization": f"Bearer {self._token}"},
                timeout=SKILLS_TIMEOUT_SECS,
            )
        except httpx.HTTPError as err:
            raise RuntimeError(f"the engine's skill gateway is unreachable: {err}") from err

    @staticmethod
    def _failure(response: httpx.Response) -> tuple[str, str]:
        """The gateway's error kind (`unknown-skill`, `unknown-file`, ...) and message."""
        try:
            body = response.json()
        except ValueError:
            body = {}
        if not isinstance(body, dict):
            body = {}
        return str(body.get("error", "")), str(body.get("message") or response.text or response.reason_phrase)

    def _raise(self, response: httpx.Response, what: str) -> None:
        kind, message = self._failure(response)
        if response.status_code == 404 and kind == "unknown-file":
            raise FileNotFoundError(message)
        if response.status_code == 404:
            raise KeyError(message)
        raise RuntimeError(f"the engine refused {what} ({response.status_code}): {message}")

    def list_skills(self) -> dict[str, SkillInfo]:
        """The task's pinned skills, by name."""
        response = self._get("/skills")
        if response.status_code != 200:
            self._raise(response, "the skill list")
        return {
            skill["name"]: SkillInfo(name=skill["name"], description=skill["description"]) for skill in response.json()
        }

    def load_skill(self, name: str) -> tuple[str, list[str]]:
        """The pinned version's instructions and the sorted paths of its bundled files.

        :raises KeyError: If the task's token pins no skill `name`.
        """
        response = self._get(f"/skills/{quote(name, safe='')}")
        if response.status_code != 200:
            self._raise(response, f"skill {name!r}")
        skill = response.json()
        return skill["body"], list(skill["files"])

    def read_skill_file(self, name: str, path: str) -> str:
        """A file bundled with the pinned version, as text.

        :raises KeyError: If the task's token pins no skill `name`.
        :raises FileNotFoundError: If the pinned version bundles no file `path`.
        """
        response = self._get(f"/skills/{quote(name, safe='')}/files/{quote(path, safe='/')}")
        if response.status_code != 200:
            self._raise(response, f"file {path!r} of skill {name!r}")
        return response.text

    def to_dict(self) -> dict[str, Any]:
        """Serialize the store's URL only: the token is per run and never serialized."""
        return {"type": generate_qualified_class_name(type(self)), "init_parameters": {"url": self.url}}

    @classmethod
    def from_dict(cls, data: dict[str, Any]) -> "EngineSkillStore":
        """Rebuild a store; the run's token must be supplied again as `init_parameters["token"]`.

        :raises ValueError: Without a token.
        """
        params = data.get("init_parameters", {})
        token = params.get("token")
        if not token:
            raise ValueError(
                "EngineSkillStore.from_dict needs the run's task token in init_parameters['token']: "
                "a token is per run and is never serialized"
            )
        return cls(url=params["url"], token=token)
