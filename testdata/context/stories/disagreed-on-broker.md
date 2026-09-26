---
title: Disagreed with the tech lead on the message broker
tags: [conflict, disagreement, technical-decision]
---

Situation: At Contoso Logistics we were choosing a broker for the new shipment event pipeline.
The tech lead wanted the managed queue service the team already knew.

Task: I thought it was the wrong fit, because we needed replay and several consumers reading the
same stream, and I had to make that case without turning a design meeting into an argument.

Action: I asked for two days and built a spike with both options against our real event shapes,
wrote up the numbers, and took them to the lead one-on-one before any group discussion. He pushed
back on operational cost, which was fair, so we settled on the managed version of the log-based
broker. I also said I would commit to his call if the spike did not convince him.

Result: The pipeline shipped on time, and a year later replay let us recover from a consumer bug
that had been dropping events for six hours, with no data lost.
