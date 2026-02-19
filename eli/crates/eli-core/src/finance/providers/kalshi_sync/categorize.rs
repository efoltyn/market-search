/// Returns true if a category string looks like sports/esports — these are noise for financial analysis.
fn is_sports_category(cat: &str) -> bool {
    let c = cat.to_lowercase();
    c.contains("sport") || c.contains("esport")
}
