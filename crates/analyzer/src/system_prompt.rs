//! Hardcoded system prompt that constrains the model to a fixed numeric JSON
//! shape, regardless of the caller's free-form user prompt.

/// Forces `{"results":[{"index":<i>,"value":<number>}, ...]}` with one entry per
/// image, so a batch of N images yields N parseable numeric values the analyzer
/// can map back to each image's capture time by `index`.
pub const SYSTEM_PROMPT: &str = "\
You are a vision analysis assistant. You are given N images in a fixed order. \
Analyze each image according to the user's instruction and respond with ONLY a \
JSON object of the exact shape {\"results\":[{\"index\":<0-based image index>,\"value\":<number>}, ...]} \
containing one entry for every image, in order. <number> must be a single finite \
numeric value (integer or decimal, may be negative). If the quantity is an integer \
encoding (for example a color expressed as 0xRRGGBB), return it as a plain integer. \
Do not include units, prose, markdown fences, or any other keys.";