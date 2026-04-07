pub fn create_magnet(info_hash_hex: &str, display_name: &str, trackers: &[&str]) -> String {
    let mut uri = format!(
        "magnet:?xt=urn:btih:{}&dn={}",
        info_hash_hex.to_uppercase(),
        urlencoding::encode(display_name)
    );
    for tracker in trackers {
        uri.push_str(&format!("&tr={}", urlencoding::encode(tracker)));
    }
    uri
}
