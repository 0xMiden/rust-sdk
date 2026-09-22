use super::*;

#[tokio::test]
async fn proof_of_work_matches_the_faucet_byte_order() {
    let nonce = solve_pow("0102ff", u64::MAX / 8).await.unwrap();
    // SHA-256(0102ff || 0000000000000014) starts with 0d84ea73c6cef561.
    assert_eq!(nonce, 20);
    assert_eq!(solve_pow("0x0102ff", u64::MAX / 8).await.unwrap(), nonce);
}

#[tokio::test]
async fn invalid_challenges_fail_and_difficult_work_times_out() {
    for (challenge, target) in [("00", 0), ("", 1), ("0", 1), ("zz", 1)] {
        assert!(matches!(solve_pow(challenge, target).await, Err(FundingError::Invalid(_))));
    }
    assert!(matches!(
        within(Duration::from_millis(10), "proof of work", solve_pow("01", 1)).await,
        Err(FundingError::Timeout("proof of work"))
    ));
}

#[test]
fn invalid_options_are_rejected() {
    for options in [
        FundingOptions {
            amount: Some(0),
            ..FundingOptions::default()
        },
        FundingOptions {
            timeout: Duration::ZERO,
            ..FundingOptions::default()
        },
        FundingOptions {
            poll_interval: Duration::ZERO,
            ..FundingOptions::default()
        },
    ] {
        assert!(options.validate().is_err());
    }
    // Browser timers wrap or throw for these values instead of waiting as requested.
    for duration in [Duration::from_nanos(1), Duration::MAX] {
        for options in [
            FundingOptions {
                timeout: duration,
                ..FundingOptions::default()
            },
            FundingOptions {
                poll_interval: duration,
                ..FundingOptions::default()
            },
        ] {
            assert!(options.validate().is_err());
        }
    }
}

fn mock_http(
    responses: alloc::vec::Vec<(&'static str, u16, String)>,
) -> (String, std::thread::JoinHandle<()>) {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}/api", listener.local_addr().unwrap());
    let task = std::thread::spawn(move || {
        for (path, status, body) in responses {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut reader = BufReader::new(&mut stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).unwrap(), 0);
                if line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            assert!(request.starts_with(&format!("GET /api/{path}")), "{request}");
            if path == "get_tokens?" {
                assert!(request.contains("asset_amount=9007199254740993"));
                assert!(request.contains("is_private_note=false"));
            }
            write!(stream, "HTTP/1.1 {status} Response\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
    });
    (endpoint, task)
}

fn faucet_id() -> AccountId {
    miden_protocol::testing::account_id::ACCOUNT_ID_FEE_FAUCET.try_into().unwrap()
}

#[tokio::test]
async fn request_preserves_large_integer_amounts_and_endpoint_paths() {
    let faucet = faucet_id();
    let id = crate::Word::default().to_string();
    let (url, task) = mock_http(vec![
        (
            "get_metadata",
            200,
            format!(r#"{{"id":"{faucet}","base_amount":9007199254740993}}"#),
        ),
        ("pow?", 200, r#"{"challenge":"0102","target":18446744073709551615}"#.into()),
        ("get_tokens?", 200, format!(r#"{{"note_id":"{id}","tx_id":"{id}"}}"#)),
    ]);
    let result = request_note(&url, faucet, faucet, NetworkId::Testnet, None).await.unwrap();
    task.join().unwrap();
    assert_eq!(result.amount, 9_007_199_254_740_993);
    assert_eq!(result.note_id.to_hex(), id);
}

#[tokio::test]
async fn mismatched_fee_asset_stops_before_requesting_tokens() {
    let other = miden_protocol::testing::account_id::ACCOUNT_ID_PUBLIC_FUNGIBLE_FAUCET;
    let other: AccountId = other.try_into().unwrap();
    let (url, task) = mock_http(vec![(
        "get_metadata",
        200,
        format!(r#"{{"id":"{other}","base_amount":1000000}}"#),
    )]);
    assert!(matches!(
        request_note(&url, other, faucet_id(), NetworkId::Testnet, None).await,
        Err(FundingError::Invalid(_))
    ));
    task.join().unwrap();
}

#[tokio::test]
async fn faucet_http_failure_is_reported_without_retrying() {
    let (url, task) = mock_http(vec![("get_metadata", 429, "rate limited".into())]);
    let error = request_note(&url, faucet_id(), faucet_id(), NetworkId::Testnet, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, FundingError::Http(error) if error.status() == Some(reqwest::StatusCode::TOO_MANY_REQUESTS))
    );
    task.join().unwrap();
}

#[tokio::test]
async fn mint_failure_is_not_retried() {
    let faucet = faucet_id();
    let (url, task) = mock_http(vec![
        (
            "get_metadata",
            200,
            format!(r#"{{"id":"{faucet}","base_amount":9007199254740993}}"#),
        ),
        ("pow?", 200, r#"{"challenge":"0102","target":18446744073709551615}"#.into()),
        ("get_tokens?", 503, "unavailable".into()),
    ]);
    let error = request_note(&url, faucet, faucet, NetworkId::Testnet, None).await.unwrap_err();
    assert!(
        matches!(error, FundingError::Http(error) if error.status() == Some(reqwest::StatusCode::SERVICE_UNAVAILABLE))
    );
    task.join().unwrap();
}

#[tokio::test]
async fn stage_timeout_cancels_a_stalled_operation() {
    let result =
        within::<()>(Duration::from_millis(10), "the funding note", core::future::pending()).await;
    assert!(matches!(result, Err(FundingError::Timeout("the funding note"))));
}

#[tokio::test]
async fn invalid_endpoint_fails_before_http() {
    for endpoint in [
        "ftp://localhost",
        "http://localhost/?key=value",
        "http://localhost/#fragment",
        "not a URL",
    ] {
        let result =
            request_note(endpoint, faucet_id(), faucet_id(), NetworkId::Testnet, None).await;
        assert!(matches!(result, Err(FundingError::Invalid(_))));
    }
}

#[tokio::test]
async fn faucet_address_must_match_the_network() {
    let faucet = Address::new(faucet_id()).encode(NetworkId::Devnet);
    let (url, task) = mock_http(vec![(
        "get_metadata",
        200,
        format!(r#"{{"id":"{faucet}","base_amount":1000000}}"#),
    )]);
    let error = request_note(&url, faucet_id(), faucet_id(), NetworkId::Testnet, None)
        .await
        .unwrap_err();
    assert!(
        matches!(error, FundingError::Invalid(message) if message.contains("different network"))
    );
    task.join().unwrap();
}
