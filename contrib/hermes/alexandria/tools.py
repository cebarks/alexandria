"""Native Hermes tool schemas, matching Alexandria's public MCP parameters."""


def tool_schemas() -> list[dict]:
    text = {"type": "string"}
    tags = {"type": "array", "items": text}
    integer = {"type": "integer"}
    specs = [
        ("store_memory", "Store durable facts proactively: preferences, corrections, conventions and resolved root causes.",
         {"content": text, "tags": tags}, ["content"]),
        ("search_memories", "Search long-term memory beyond automatic recall.",
         {"query": text, "limit": integer}, ["query"]),
        ("recall", "Explore memory clusters, then pass the returned scope_handle to narrow the search.",
         {"query": text, "scope_handle": text}, ["query"]),
        ("update_memory", "Correct an existing memory by ID; content updates preserve lineage.",
         {"id": text, "content": text, "tags": tags, "confidence": {"type": "number"}}, ["id"]),
        ("delete_memory", "Soft-delete a memory by ID when it is obsolete or the user asks to forget it.",
         {"id": text}, ["id"]),
        ("set_reminder", "Schedule a reminder. Supply exactly one of due_at, pattern or cron; delivery requires a later check. Confirm next fire times and inspect warnings. Cron uses 1=Sunday..7=Saturday and intersects restricted day-of-month/day-of-week.",
         {"message": text, "due_at": text, "cron": text,
          "pattern": {"type": "object", "properties": {
              "freq": {"type": "string", "enum": ["daily", "weekly", "monthly"]},
              "time": text, "weekdays": tags, "day_of_month": integer},
              "required": ["freq", "time"]},
          "target_project": text, "prov_project": text, "note": text}, ["message"]),
        ("check_reminders", "Consume due reminders and tell the user their messages. Delivery is best-effort-once; project matching is exact.",
         {"project": text}, []),
        ("list_reminders", "List reminders without consuming them; pending by default. Page using limit/offset.",
         {"status": {"type": "string", "enum": ["pending", "delivered", "cancelled", "all"]},
          "target_project": text, "limit": integer, "offset": integer}, []),
        ("cancel_reminder", "Cancel a reminder by ID when it is no longer needed.", {"id": text}, ["id"]),
    ]
    return [{"name": f"alexandria_{name}", "description": description,
             "parameters": {"type": "object", "properties": properties,
                            "required": required, "additionalProperties": False}}
            for name, description, properties, required in specs]
