use dual_rail_rails::khqr::{BakongVerifier, KhqrStatus, KhqrVerifier};
use khqr_api::BakongClient;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn md5s(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| (*name).to_owned()).collect()
}

fn paid(hash: &str) -> Value {
    json!({ "hash": hash, "amount": 10.5, "currency": "USD", "toAccountId": "shop@aclb" })
}

async fn respond(server: &MockServer, endpoint: &str, template: ResponseTemplate, times: u64) {
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(template)
        .expect(times)
        .mount(server)
        .await;
}

fn verifier(server: &MockServer) -> BakongVerifier {
    BakongVerifier::new(BakongClient::with_base_url(server.uri(), "token"))
}

#[tokio::test]
async fn batch_answers_are_read_item_by_item() {
    let server = MockServer::start().await;
    let body = json!({
        "responseCode": 0,
        "data": [
            { "md5": "a", "status": "SUCCESS", "data": paid("hash-a") },
            { "md5": "b", "status": "NOT_FOUND" },
            { "md5": "c", "status": "SUCCESS", "amount": 1.5 }
        ]
    });
    respond(
        &server,
        "/v1/check_transaction_by_md5_list",
        ResponseTemplate::new(200).set_body_json(body),
        1,
    )
    .await;

    let statuses = verifier(&server)
        .check(&md5s(&["a", "b", "c"]))
        .await
        .unwrap();

    match &statuses[0] {
        KhqrStatus::Paid(transfer) => {
            assert_eq!(transfer.hash, "hash-a");
            assert_eq!(transfer.amount_minor, Some(1050));
        }
        other => panic!("expected paid, got {other:?}"),
    }
    assert_eq!(statuses[1], KhqrStatus::Unpaid);
    assert!(
        matches!(statuses[2], KhqrStatus::Unreadable(_)),
        "one bad item stays local"
    );
}

#[tokio::test]
async fn a_refused_batch_endpoint_falls_back_to_single_lookups_for_good() {
    let server = MockServer::start().await;
    respond(
        &server,
        "/v1/check_transaction_by_md5_list",
        ResponseTemplate::new(403).set_body_string("<html>403 Forbidden</html>"),
        1,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/v1/check_transaction_by_md5"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "responseCode": 0, "data": paid("hash-x") })),
        )
        .expect(4)
        .mount(&server)
        .await;
    let verifier = verifier(&server);

    let first = verifier.check(&md5s(&["a", "b"])).await.unwrap();
    let second = verifier.check(&md5s(&["c", "d"])).await.unwrap();

    for status in first.iter().chain(&second) {
        assert!(matches!(status, KhqrStatus::Paid(_)), "{status:?}");
    }
}

#[tokio::test]
async fn an_unreachable_bakong_fails_the_whole_check() {
    let server = MockServer::start().await;
    respond(
        &server,
        "/v1/check_transaction_by_md5_list",
        ResponseTemplate::new(403),
        1,
    )
    .await;
    respond(
        &server,
        "/v1/check_transaction_by_md5",
        ResponseTemplate::new(403),
        1,
    )
    .await;

    assert!(
        verifier(&server)
            .check(&md5s(&["a", "b", "c"]))
            .await
            .is_err()
    );
}
