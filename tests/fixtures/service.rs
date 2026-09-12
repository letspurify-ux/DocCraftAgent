/// Validates a support request and returns its result.
pub fn handle_request(name: &str) -> Result<String, &'static str> {
    if name.trim().is_empty() { return Err("name_required"); }
    Ok(format!("Hello, {name}"))
}
