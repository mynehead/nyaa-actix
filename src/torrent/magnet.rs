pub fn create_magnet(info_hash_hex: &str, display_name: &str, trackers: &[&str]) -> String {
    let mut uri =
        format!("magnet:?xt=urn:btih:{}&dn={}", info_hash_hex.to_uppercase(), urlencoding::encode(display_name));
    for tracker in trackers {
        uri.push_str(&format!("&tr={}", urlencoding::encode(tracker)));
    }
    uri
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_magnet_with_encoded_name_and_trackers() {
        let uri = create_magnet("abcdef", "Test & Co", &["udp://t1:6969/announce", "http://t2/announce"]);
        assert_eq!(
            uri,
            "magnet:?xt=urn:btih:ABCDEF&dn=Test%20%26%20Co\
             &tr=udp%3A%2F%2Ft1%3A6969%2Fannounce&tr=http%3A%2F%2Ft2%2Fannounce"
        );
    }

    #[test]
    fn builds_magnet_without_trackers() {
        assert_eq!(create_magnet("ab", "x", &[]), "magnet:?xt=urn:btih:AB&dn=x");
    }
}
