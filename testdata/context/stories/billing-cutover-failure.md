tags: failure, lessons-learned, migration

# The billing cutover that went wrong

Situation: At Northwind Payments I led the move of invoicing off an eight-year-old legacy system,
planned as one big weekend cutover.

Task: Switch every customer to the new invoicing service without disrupting month-end billing.

Action: The cutover ran, but about two percent of invoices came out wrong because of manual
adjustments and grandfathered plans I had underestimated. I went to finance the first morning
with what we knew, sent a daily written update with counts of affected, fixed and open invoices,
and worked through the cleanup with them for three weeks.

Result: Every invoice was corrected, and finance said the daily notes were why they trusted the
numbers again. Since then every migration I lead has a shadow phase: the new system computes
results nobody acts on, and we diff them against the old one until the diff is empty.
