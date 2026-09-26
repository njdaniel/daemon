---
title: Cutting the audit log down to ship for the conference
tags: prioritization, scope, tradeoffs, deadline
---

Situation: A customer-facing audit log was promised for a conference date, and three weeks out
the full design, with filtering, export and retention policies, clearly would not make it.

Task: Decide what to cut so we still shipped something the customers who asked for it could use.

Action: With the product manager I listed every capability against two questions: what did those
customers need on day one, and what could be added later without a data migration. Filtering and
export could come later; retention could not, because data stored without a retention policy
would later have to be deleted. When sales pushed back, I joined two customer calls to explain
the timeline myself.

Result: We shipped a plain log with retention built in. Both enterprise customers were
satisfied, and filtering followed six weeks later.
