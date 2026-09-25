//! The four kinds of source (SPEC § Context and LLM, Sources).

/// What a source is. Resume and job description are single documents; notes and stories are
/// folders with one file per note or story.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SourceKind {
    Resume,
    JobDescription,
    Notes,
    Stories,
}

impl SourceKind {
    pub const ALL: [SourceKind; 4] = [
        SourceKind::Resume,
        SourceKind::JobDescription,
        SourceKind::Notes,
        SourceKind::Stories,
    ];

    /// The text stored in `sources.kind`.
    pub fn as_str(self) -> &'static str {
        match self {
            SourceKind::Resume => "resume",
            SourceKind::JobDescription => "job_description",
            SourceKind::Notes => "notes",
            SourceKind::Stories => "stories",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|k| k.as_str() == s)
    }

    /// One document of this kind at a time: adding another replaces it.
    pub fn is_single_document(self) -> bool {
        matches!(self, SourceKind::Resume | SourceKind::JobDescription)
    }
}

impl std::fmt::Display for SourceKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_round_trip_and_only_resume_and_jd_are_single_documents() {
        for k in SourceKind::ALL {
            assert_eq!(SourceKind::parse(k.as_str()), Some(k));
        }
        assert_eq!(SourceKind::parse("diary"), None);
        assert!(SourceKind::Resume.is_single_document());
        assert!(SourceKind::JobDescription.is_single_document());
        assert!(!SourceKind::Notes.is_single_document());
        assert!(!SourceKind::Stories.is_single_document());
    }
}
