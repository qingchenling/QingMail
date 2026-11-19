use super::*;
const CLIENT: &str = "12345678-1234-4234-8234-123456789abc";
fn login() -> Login {
    Login::begin(
        CLIENT,
        "test@outlook.com",
        Authentication::MicrosoftPersonal,
    )
    .unwrap()
}
fn callback(login: &Login, query: &str) -> String {
    let uri = url::Url::parse(&login.redirect).unwrap();
    format!(
        "GET {CALLBACK_PATH}?{query} HTTP/1.1\r\nHost: localhost:{}\r\n\r\n",
        uri.port().unwrap()
    )
}
#[test]
fn microsoft_profiles_validate_and_tokens_cannot_go_to_other_servers() {
    for kind in [
        Authentication::MicrosoftPersonal,
        Authentication::Microsoft365,
    ] {
        let mut account = settings("测试", "test@outlook.com", kind).unwrap();
        assert!(valid_servers(&account));
        account.smtp.host = "attacker.invalid".into();
        assert!(crate::accounts::validate(&account).is_err());
        account = settings("测试", "test@outlook.com", kind).unwrap();
        account.smtp.authentication = Authentication::Password;
        assert!(crate::accounts::validate(&account).is_err());
        account = settings("测试", "test@outlook.com", kind).unwrap();
        account.imap.security = ConnectionSecurity::StartTls;
        assert!(!valid_servers(&account));
    }
    assert!(settings("x", "test@outlook.com", Authentication::Password).is_err());
    assert!(!valid_client_id("not-a-client-secret"));
    assert!(valid_client_id(CLIENT));
}
#[test]
fn microsoft_pkce_uses_random_state_s256_and_correct_mail_scopes() {
    let a = login();
    let b = login();
    assert_ne!(*a.state, *b.state);
    assert_ne!(*a.verifier, *b.verifier);
    let url = url::Url::parse(&a.authorization_url).unwrap();
    assert_eq!(url.scheme(), "https");
    assert_eq!(url.host_str(), Some("login.microsoftonline.com"));
    assert_eq!(url.path(), "/consumers/oauth2/v2.0/authorize");
    let pairs: std::collections::HashMap<_, _> = url.query_pairs().collect();
    assert_eq!(pairs["code_challenge_method"], "S256");
    assert_eq!(
        pairs["code_challenge"],
        URL_SAFE_NO_PAD.encode(Sha256::digest(a.verifier.as_bytes()))
    );
    assert_eq!(pairs["scope"], SCOPES);
    assert!(!pairs.contains_key("client_secret"));
    assert_eq!(pairs["redirect_uri"], a.redirect);
    assert_eq!(
        a.listener.local_addr().unwrap().ip(),
        std::net::Ipv4Addr::LOCALHOST
    );
    let uri = url::Url::parse(&a.redirect).unwrap();
    assert_eq!(uri.host_str(), Some("localhost"));
    assert_eq!(uri.path(), CALLBACK_PATH);
}
#[test]
fn microsoft_callback_rejects_csrf_wrong_hosts_paths_duplicates_and_injection() {
    let login = login();
    let good = callback(
        &login,
        &format!("state={}&code=synthetic-code", *login.state),
    );
    assert_eq!(&*login.callback(&good).unwrap(), "synthetic-code");
    let port = login.listener.local_addr().unwrap().port();
    for bad in [
        good.replace(&*login.state, "wrong-state"),
        good.replace("Host: localhost", "Host: attacker.invalid"),
        good.replace(CALLBACK_PATH, "/wrong/path"),
        good.replace("GET ", "POST "),
        good.replace("code=synthetic-code", "code=one&code=two"),
        good.replace("code=synthetic-code", "code=line%0D%0Abreak"),
        good.replace("code=synthetic-code", "error=access_denied"),
        good.replace(" HTTP/1.1", " HTTP/1.1 extra"),
        good.replace("\r\n\r\n", &format!("\r\nHost: localhost:{port}\r\n\r\n")),
        callback(&login, "code=no-state"),
        callback(
            &login,
            &format!("state={}&state={}&code=code", *login.state, *login.state),
        ),
    ] {
        assert!(login.callback(&bad).is_err());
    }
    let denied = callback(
        &login,
        &format!("state={}&error=access_denied", *login.state),
    );
    assert_eq!(login.callback(&denied).err(), Some(Error::Cancelled));
}
#[test]
fn microsoft_cancel_stops_loopback_without_contacting_token_endpoint() {
    let login = login();
    assert_eq!(
        login.finish(&AtomicBool::new(true)).err(),
        Some(Error::Cancelled)
    );
}
#[test]
fn microsoft_tokens_refresh_rotation_preserves_missing_refresh_and_enforces_bounds() {
    let response = br#"{"access_token":"new-access","refresh_token":"rotated-refresh","token_type":"Bearer","expires_in":3600}"#;
    let tokens = parse_tokens(
        response,
        CLIENT,
        Authentication::MicrosoftPersonal,
        Some("old-refresh"),
    )
    .unwrap();
    assert_eq!(&*tokens.refresh_token, "rotated-refresh");
    assert!(!tokens.needs_refresh());
    let decoded = Tokens::decode(&tokens.encode().unwrap()).unwrap();
    assert_eq!(&*decoded.access_token, "new-access");
    let without = br#"{"access_token":"access","token_type":"Bearer","expires_in":3600}"#;
    assert_eq!(
        &*parse_tokens(
            without,
            CLIENT,
            Authentication::MicrosoftPersonal,
            Some("old-refresh")
        )
        .unwrap()
        .refresh_token,
        "old-refresh"
    );
    assert!(parse_tokens(without, CLIENT, Authentication::MicrosoftPersonal, None).is_err());
    for bad in [
        String::from_utf8(response.to_vec())
            .unwrap()
            .replace("Bearer", "Basic"),
        String::from_utf8(response.to_vec())
            .unwrap()
            .replace("3600", "0"),
        String::from_utf8(response.to_vec())
            .unwrap()
            .replace("new-access", "injected\\r\\n"),
        String::from_utf8(response.to_vec())
            .unwrap()
            .replace("new-access", &"x".repeat(65537)),
    ] {
        assert!(
            parse_tokens(
                bad.as_bytes(),
                CLIENT,
                Authentication::MicrosoftPersonal,
                None
            )
            .is_err()
        );
    }
    assert!(Tokens::decode(&vec![b'x'; MAX_SECRET_BYTES + 1]).is_err());
}
#[test]
fn microsoft_xoauth2_wire_is_exact_and_has_no_command_injection() {
    let encoded = xoauth2("test@outlook.com", "synthetic-token").unwrap();
    assert_eq!(
        STANDARD.decode(encoded.as_bytes()).unwrap(),
        b"user=test@outlook.com\x01auth=Bearer synthetic-token\x01\x01"
    );
    assert!(xoauth2("test\x01evil", "token").is_err());
    assert!(xoauth2("test@outlook.com", "token with space").is_err());
    assert!(xoauth2("test@outlook.com", "token\r\nLOGIN").is_err());
}
