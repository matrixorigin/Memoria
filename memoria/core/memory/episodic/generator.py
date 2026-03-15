"""Episodic memory generator using LLM summarization."""

from __future__ import annotations

import json
import logging
import re
from typing import Any

from memoria.core.memory.types import EpisodicMetadata

logger = logging.getLogger(__name__)

EPISODIC_PROMPT = """You are analyzing a conversation session to create an episodic memory summary.

Extract the following information from the conversation:
1. **Topic**: The main subject or theme discussed (1-2 sentences)
2. **Action**: Key actions, decisions, or activities performed (2-3 sentences)
3. **Outcome**: Results, conclusions, or current state (1-2 sentences)

Be concise and factual. Focus on what was accomplished, not how the conversation flowed.
{focus_clause}
Conversation messages:
{messages}

Respond with a JSON object containing: topic, action, outcome
"""

LIGHTWEIGHT_PROMPT = """Summarize this conversation segment into 3-5 key points.

Focus on:
- What was discussed or decided
- Actions taken or planned
- Important facts or conclusions

Be extremely concise (each point max 10 words).

Conversation:
{messages}

Respond with a JSON object: {{"points": ["point 1", "point 2", ...]}}
"""


def _extract_json(text: str) -> str:
    """Extract JSON from text, handling markdown code blocks and surrounding noise."""
    text = text.strip()
    # Strip markdown code fences
    text = re.sub(r"^```(?:json)?\s*", "", text)
    text = re.sub(r"\s*```$", "", text)
    text = text.strip()
    # If still not starting with {, try to find first { ... } block
    if not text.startswith("{"):
        match = re.search(r"\{.*\}", text, re.DOTALL)
        if match:
            text = match.group(0)
    return text


def _truncate_messages(
    messages: list[dict[str, Any]],
    max_messages: int,
    max_tokens: int,
) -> tuple[list[dict[str, Any]], bool]:
    """Truncate messages to fit within limits, preserving head and tail for context.

    Returns (messages, truncated).
    """
    truncated = False

    if len(messages) > max_messages:
        # Keep first 10% and last 90% to preserve opening context + recent content
        head_count = max(1, max_messages // 10)
        tail_count = max_messages - head_count
        head = messages[:head_count]
        tail = messages[-tail_count:]
        messages = head + tail
        truncated = True
        logger.warning(
            "Truncated messages from %d to %d (kept head=%d, tail=%d)",
            len(messages) + (len(messages) - max_messages),
            max_messages,
            head_count,
            tail_count,
        )

    total_chars = sum(len(m.get("content", "")) for m in messages)
    if total_chars > max_tokens * 4:
        budget = max_tokens * 4
        kept: list[dict[str, Any]] = []
        char_count = 0
        for msg in reversed(messages):
            content = msg.get("content", "")
            if char_count + len(content) > budget:
                truncated = True
                break
            kept.insert(0, msg)
            char_count += len(content)
        original_count = len(messages)
        messages = kept
        logger.warning(
            "Token budget exceeded: truncated from %d to %d messages (budget=%d chars)",
            original_count,
            len(messages),
            budget,
        )

    return messages, truncated


def generate_episodic_memory(
    messages: list[dict[str, Any]],
    llm_client: Any,
    max_messages: int = 200,
    max_tokens: int = 16000,
    session_id: str | None = None,
    focus_topics: list[str] | None = None,
) -> tuple[EpisodicMetadata, bool]:
    """Generate full episodic memory (topic/action/outcome) from session messages.

    Returns:
        Tuple of (EpisodicMetadata, truncated_flag)
    """
    if not messages:
        raise ValueError("Cannot generate episodic memory from empty message list")

    messages, truncated = _truncate_messages(messages, max_messages, max_tokens)

    msg_text = "\n".join(
        f"{m.get('role', 'unknown')}: {m.get('content', '')[:500]}" for m in messages
    )

    focus_clause = (
        f"\nPay special attention to these topics: {', '.join(focus_topics)}.\n"
        if focus_topics
        else ""
    )
    prompt = EPISODIC_PROMPT.format(messages=msg_text, focus_clause=focus_clause)
    response = llm_client.chat(
        messages=[{"role": "user", "content": prompt}],
        temperature=0.3,
    )

    try:
        data = json.loads(_extract_json(response))
        metadata = EpisodicMetadata(
            topic=data["topic"],
            action=data["action"],
            outcome=data["outcome"],
            source_event_ids=[m.get("id", "") for m in messages if m.get("id")],
            session_id=session_id,
        )
        return metadata, truncated
    except (json.JSONDecodeError, KeyError) as e:
        logger.error(
            "Failed to parse LLM response for full mode: %s, response: %.200s",
            e,
            response,
        )
        raise ValueError(f"LLM returned invalid JSON: {e}")


def generate_lightweight_summary(
    messages: list[dict[str, Any]],
    llm_client: Any,
    max_messages: int = 50,
    max_tokens: int = 4000,
) -> tuple[list[str], bool]:
    """Generate lightweight incremental summary (3-5 bullet points).

    Returns:
        Tuple of (points_list, truncated_flag)
    """
    if not messages:
        raise ValueError("Cannot generate lightweight summary from empty message list")

    messages, truncated = _truncate_messages(messages, max_messages, max_tokens)

    msg_text = "\n".join(
        f"{m.get('role', 'unknown')}: {m.get('content', '')[:300]}" for m in messages
    )

    prompt = LIGHTWEIGHT_PROMPT.format(messages=msg_text)
    response = llm_client.chat(
        messages=[{"role": "user", "content": prompt}],
        temperature=0.3,
    )

    try:
        data = json.loads(_extract_json(response))
        points: list[str] = data.get("points", [])
        if not isinstance(points, list) or not points:
            raise ValueError("Expected non-empty 'points' list")
        return points, truncated
    except (json.JSONDecodeError, KeyError, ValueError) as e:
        logger.error(
            "Failed to parse LLM response for lightweight mode: %s, response: %.200s",
            e,
            response,
        )
        raise ValueError(f"LLM returned invalid JSON for lightweight mode: {e}")
