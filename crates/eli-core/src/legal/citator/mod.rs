mod fetch;
mod quotes;
mod treatment;
pub use fetch::{fetch_citations, CitationRequest, CitationResponse, CitationVerdict, NetworkCase};
pub use quotes::{QuoteCheck, QuoteSummary, ShortCite};
pub use treatment::{TreatmentReport, TreatmentSignal};
