//! Context assembly: source ingestion, retrieval and session memory.
//!
//! Input: resume / job description / notes / stories (PDF, MD, TXT) and the extracted question.
//! Output: <= 1,500 tokens of retrieved chunks (BM25), the JD requirement bullets, and the last 8 summary lines.
//! Owns: the chunk store tables it writes through `dmn-store` and the session summariser task.
