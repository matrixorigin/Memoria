"""Prompt templates for memory Observer."""

OBSERVER_EXTRACTION_PROMPT = """\
Extract structured memories from this conversation turn.
Return a JSON array ONLY, no other text. Each item:
{"type": "profile|semantic|procedural|episodic",
 "content": "concise factual statement",
 "confidence": 0.0-1.0}

Types (choose the MOST SPECIFIC type — prefer profile over semantic when applicable):
- profile: user identity, preferences, environment, habits, tools, language, role.
  Examples: "prefers Go over Python", "uses vim", "works on mo-dev-agent project",
  "speaks Chinese", "is a backend developer", "uses conda for env management",
  "prefers concise code review feedback", "runs Linux"
- semantic: general knowledge or facts learned from the conversation that are NOT
  about the user themselves. Examples: "MatrixOne supports time-travel queries",
  "event sourcing stores all state changes as events"
- procedural: repeated action patterns the user follows across multiple turns.
  Examples: "always runs tests before commit", "reviews staged changes before pushing"
- episodic: what the user DID or ASKED ABOUT in this conversation — activities,
  tasks, topics explored, decisions made. Use when the user worked on something
  specific that they may want to recall later.
  Examples: "reviewed TiDB PR #12345 about memory leak fix",
  "debugged a session_id propagation bug in Memoria integration",
  "asked about the latest TiDB pull requests"

IMPORTANT: If a fact describes WHO the user is, WHAT they prefer, or HOW they work,
it is "profile" — not "semantic" or "procedural".
If the user asked about or worked on a specific topic/task, extract it as "episodic".

Confidence guide:
- 1.0: user explicitly stated
- 0.7: strongly implied by context
- 0.4: weakly inferred

Do NOT extract: greetings, pure meta-conversation ("what did we discuss").
If nothing worth remembering, return [].
"""
