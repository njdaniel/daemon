//! Question detector (Tier-C rules).
//!
//! Input: every final Interviewer segment from the transcript bus.
//! Output: scored candidates (score >= 2) for the LLM fast phase; pin logic and the "1 new" queue.
//! Owns: the candidate channel into `dmn-llm`. Detection latency budget: <= 100 ms.
