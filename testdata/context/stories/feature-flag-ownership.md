---
title: Taking over the orphaned feature-flag library
tags: [ownership, initiative]
---

Situation: A shared feature-flag library belonged to a team that had moved on. A bug made flags
evaluate inconsistently during deploys and caused a confusing incident for my team, but nobody
was really responsible for the library.

Task: Stop the incidents without waiting months for a ticket to reach the nominal owners.

Action: I spent a Friday tracing the bug to a cache that was not invalidated on config reload,
wrote the fix with a regression test, and sent it to the owners with a clear explanation. I then
proposed that my team take over the library, since we were its heaviest user, and asked the old
team to review our first few changes so it felt like a handoff rather than a takeover.

Result: The fix shipped the next week, my manager approved the transfer, and we have owned the
library since without a repeat of the incident.
