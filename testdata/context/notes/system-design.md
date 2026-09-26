# System design reminders

Fictional notes; a test fixture for `dmn sources`.

- Start with requirements: users, read and write rates, latency targets, consistency needs.
- Estimate before designing: requests per second, storage per year, bandwidth at peak.
- Draw the request path first, then the data model, then the failure modes.
- For every cache, say what happens on a miss, on a stampede and when the cache is down.
- For every queue, say how consumers stay idempotent and what the dead-letter path is.
- Name the trade-off out loud: consistency against latency, cost against durability.
- Close with how you would roll it out and what you would measure in the first week.
